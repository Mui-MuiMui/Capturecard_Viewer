use super::{BackgroundHotkeyRunner, HotkeyAction};
use crate::keyboard_hook::{KeyChord, KeyboardHook, KeyboardHookError};
use crate::repaint::RepaintWaker;
use log::{debug, error, trace, warn};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// リスナースレッドがキー入力を待つ時間。
///
/// タイムアウトするたびに終了要求を確認するため、終了を要求してから
/// スレッドが実際に止まるまで最大でこの時間かかる。待つのはウィンドウを
/// 閉じたあとなので、画面上は見えない。キー入力があればタイムアウトを
/// 待たずに起きるので、押下の反応はこの長さに左右されない。
const LISTENER_WAIT_TIMEOUT: Duration = Duration::from_millis(200);

/// 同じアクションの連続実行を無視する時間。
/// キーリピートで何枚も撮れてしまうのを防ぐ。
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(200);

/// 押下をどこで実行するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressRouting {
    /// UI スレッドが `take_pressed` で取りに来るまで保留する
    Deferred,
    /// 最小化中なので、UI スレッドを介さずその場で実行する
    Background,
    /// デバウンス期間内なので捨てる
    Debounced,
    /// 「フォーカスがあるときだけ反応する」がオンで、フォーカスが無いので捨てる
    Unfocused,
    /// このアプリのテキスト欄に入力中なので捨てる（#206）
    Typing,
}

/// リスナースレッドと共有する状態。
///
/// **登録中の組み合わせと押下の記録を 1 つのロックにまとめてある。** 別々に持つと、
/// リスナーが組み合わせを照合してから押下を記録するまでの隙に解除処理が終わり、
/// 解除したはずのキーで 1 回だけ実行されることがある。
pub(super) struct ListenerState {
    /// 登録中のキーの組み合わせ → アクション。未登録は空のマップで表す。
    pub(super) registered: HashMap<KeyChord, HotkeyAction>,
    /// リスナーが検出した押下の回数。UI スレッドが毎フレーム取り出して空にする。
    ///
    /// **集合ではなく回数で持つ。** 最小化している間は `update()` が呼ばれず
    /// 押下が溜まるため、何回押されたかが分からないと復帰したときに
    /// 畳めない（`folded_repeats`）。`BTreeMap` にしてあるので、取り出す
    /// 順序はアクションの宣言順で安定する。
    pub(super) pressed: BTreeMap<HotkeyAction, u32>,
    /// アクションごとの、最後に押下として受け付けた時刻。デバウンスの基準。
    ///
    /// **UI スレッド側ではなくここで見る。** 最小化中は実行が UI スレッドを
    /// 通らないため、実行の時点で計ると押しっぱなしのキーリピートを
    /// 捨てられない。
    pub(super) last_press: HashMap<HotkeyAction, Instant>,
    /// ウィンドウが最小化されているか。UI スレッドが毎フレーム書き込む。
    ///
    /// 最小化中は `update()` が呼ばれないので、ここが真のまま止まる。
    /// それが狙いで、リスナーは真の間だけ `background` へ回す。
    pub(super) minimized: bool,
    /// ウィンドウにキーボードフォーカスがあるか。UI スレッドが毎フレーム書き込む。
    ///
    /// **フックの中で `GetForegroundWindow` を呼んで調べない。** フックの
    /// コールバックでは判定以外のことをしない決まり（`crate::keyboard_hook`）
    /// なので、`minimized` と同じく UI スレッドが知っている値を書いておく。
    pub(super) focused: bool,
    /// 「フォーカスがあるときだけ反応する」がオンか。設定の反映のたびに書く。
    pub(super) only_when_focused: bool,
    /// egui がキーボード入力を受けているか（`Context::wants_keyboard_input`）。
    /// UI スレッドが毎フレーム書き込む。
    ///
    /// キーを奪わなくなったので、設定ダイアログのテキスト欄へ打った文字も
    /// ここへ届く（#206）。`focused` と同じく、フックの中で egui に
    /// 問い合わせずに UI スレッドが知っている値を書いておく。
    pub(super) typing: bool,
    /// 動いていたリスナーが止まった理由。止まっていなければ `None`。
    ///
    /// リスナーはキー入力を待てなくなると終わる（フックも外れる）。ここに
    /// 書いておき、UI スレッドの `apply` が拾って失敗として画面に出す。
    /// 書かないと、効かなくなったのに登録済みのまま何も表示されない。
    pub(super) listener_failure: Option<KeyboardHookError>,
    /// 最小化中のアクションの実行先。
    pub(super) background: BackgroundHotkeyRunner,
    /// 押下を記録したあとに UI スレッドを起こす窓口。
    ///
    /// **押下は `update()` が `take_pressed` で取りに来るまで実行されない。**
    /// 映像が届いていない間の `update()` は 250ms 間隔まで落ちるため、
    /// 起こさないとホットキーの反応がそのぶん遅れる。
    ///
    /// 既定の `RepaintWaker` は何もしないので、渡さなくても動作は変わらない
    /// （反応が遅くなるだけ）。
    pub(super) waker: RepaintWaker,
}

impl Default for ListenerState {
    fn default() -> Self {
        Self {
            registered: HashMap::new(),
            pressed: BTreeMap::new(),
            last_press: HashMap::new(),
            minimized: false,
            // 最初の update() が書くまでの間は「フォーカスあり」に倒す。
            // 起動直後はたいてい自分が前面にいる
            focused: true,
            // 既定はオフ。#133 のとおり、他のアプリの操作中や最小化中も効かせる
            only_when_focused: false,
            typing: false,
            listener_failure: None,
            background: BackgroundHotkeyRunner::default(),
            waker: RepaintWaker::default(),
        }
    }
}

impl ListenerState {
    /// 受け付けた押下を記録し、どこで実行するかを返す。
    ///
    /// デバウンスの判定もここで行う。抑止した場合に `last_press` を
    /// 更新しないのは、押しっぱなしのキーリピートで抑止が延々と続き、
    /// いつまでも実行できない状態にしないため（`decide_trigger`）。
    ///
    /// ウィンドウの状態で捨てるとき（`rejected_by_window_state`）は、
    /// デバウンスの基準も更新せずに捨てる。
    fn record_press(&mut self, action: HotkeyAction, now: Instant) -> PressRouting {
        if let Some(rejected) = rejected_by_window_state(
            self.only_when_focused,
            self.focused,
            self.minimized,
            self.typing,
        ) {
            return rejected;
        }

        let since_last = self
            .last_press
            .get(&action)
            .map(|last| now.duration_since(*last));
        if decide_trigger(since_last, HOTKEY_DEBOUNCE) == TriggerDecision::Debounced {
            return PressRouting::Debounced;
        }
        self.last_press.insert(action, now);

        if self.minimized && action.runs_while_minimized() {
            return PressRouting::Background;
        }
        *self.pressed.entry(action).or_insert(0) += 1;
        PressRouting::Deferred
    }
}

/// ウィンドウの状態から、押下を捨てるかを決める。捨てるならその理由を返す。
///
/// - 「フォーカスがあるときだけ反応する」がオンで前面にいないときは捨てる。
///   最小化中は前面にいないものとして扱う
/// - このアプリのテキスト欄に入力中（`typing`）なら捨てる（#206）。打った文字が
///   ホットキーとしても実行されないようにするため
///
/// **`typing` は前面にいて最小化していないときだけ見る。** egui はウィンドウが
/// フォーカスを失ってもテキスト欄のフォーカスを手放さないので、入力欄を
/// 選んだまま他のアプリへ移ると `typing` が真のまま残る。そこで捨てると、
/// 他のアプリの操作中にホットキーが効かなくなる。
fn rejected_by_window_state(
    only_when_focused: bool,
    focused: bool,
    minimized: bool,
    typing: bool,
) -> Option<PressRouting> {
    let foreground = focused && !minimized;
    if only_when_focused && !foreground {
        return Some(PressRouting::Unfocused);
    }
    if typing && foreground {
        return Some(PressRouting::Typing);
    }
    None
}

/// 観測した押下を、どのアクションの押下として扱うか。
///
/// リスナースレッドはアプリ全体で 1 本だけ動いており、まだ何も登録していない
/// 間も全てのキー入力を観測している。判定に必要なものを引数で受け取る
/// 純粋関数にしてあるのは、実機のキー入力なしでテストするため。
///
/// - 登録していない組み合わせなら無視する。他のアプリへ打っている文字も
///   全てここを通る
/// - 修飾キーは完全一致で比べる（`F5` の割り当ては `Ctrl+F5` では反応しない）。
///   解放とキーリピートはフックの側で落としてある
fn accepted_action(
    registered: &HashMap<KeyChord, HotkeyAction>,
    chord: KeyChord,
) -> Option<HotkeyAction> {
    registered.get(&chord).copied()
}

/// 押下を受け取ったときの判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerDecision {
    /// 実行する。最終実行時刻を更新する
    Fire,
    /// デバウンス期間内なので抑止する。最終実行時刻は更新しない
    Debounced,
}

/// 前回実行からの経過時間から、実際に実行するかを決める。
/// `since_last_trigger` が `None` なら、そのアクションはまだ 1 度も実行していない。
///
/// 抑止した場合に最終実行時刻を更新しないのは、押しっぱなしのキーリピートで
/// 抑止が延々と続き、いつまでも実行できない状態にしないため。
fn decide_trigger(since_last_trigger: Option<Duration>, debounce: Duration) -> TriggerDecision {
    match since_last_trigger {
        None => TriggerDecision::Fire,
        Some(elapsed) if elapsed > debounce => TriggerDecision::Fire,
        Some(_) => TriggerDecision::Debounced,
    }
}

/// 観測した押下 1 回ぶんを処理する。リスナースレッドから呼ばれる。
///
/// **この間は次のキー入力のフックが待たされる**（他のアプリの入力も
/// 待たされる）。ロックは照合と記録のあいだだけ握り、UI スレッドを起こす
/// ことと最小化中の実行はロックを手放してから行う。
fn handle_key_down(state: &Mutex<ListenerState>, chord: KeyChord) {
    // **照合と押下の記録を同じロックの中で行う。**
    // ロックを手放してから記録すると、その隙に解除処理が
    // 「組み合わせを消す → 押下を落とす」を終えてしまい、
    // クリアしたはずのキーで 1 回だけ実行されることがある。
    // ログはロックを手放してから出す（trace ではファイルへの
    // 書き出しが入るため、その間ロックを握らない）
    //
    // 押下を記録したときに UI スレッドを起こすための複製と、
    // 最小化中にその場で実行するための複製。
    // **どちらもロックを手放してから呼ぶ。** 握ったまま呼ぶと、
    // 相手を待つ間この共有状態も止まる
    let mut wake = None;
    let mut background = None;
    let outcome = match state.lock() {
        Ok(mut state) => accepted_action(&state.registered, chord).map(|action| {
            let routing = state.record_press(action, Instant::now());
            match routing {
                PressRouting::Deferred => wake = Some(state.waker.clone()),
                PressRouting::Background => background = Some((state.background.clone(), action)),
                PressRouting::Debounced | PressRouting::Unfocused | PressRouting::Typing => {}
            }
            (action, routing)
        }),
        Err(_) => {
            // release ビルドは panic = "abort" なので毒されない
            warn!("ホットキーの共有状態のロックを取得できないので押下を捨てる");
            None
        }
    };

    if let Some(waker) = wake {
        waker.wake();
    }
    if let Some((runner, action)) = background {
        // 最小化中なので UI スレッドは動いていない。
        // 復帰を待たずにここから実行させる（#133）
        debug!("最小化中の {} をワーカーへ回す", action.label());
        runner.run(action);
    }

    // 割り当てていないキー（他のアプリへ打っている文字）は何も出さない。
    // 全てのキー入力がここを通るので、trace でも積もりすぎる
    match outcome {
        Some((action, PressRouting::Deferred)) => {
            trace!("{} の押下を記録した", action.label())
        }
        Some((action, PressRouting::Background)) => {
            trace!("{} を最小化中のまま実行した", action.label())
        }
        Some((action, PressRouting::Debounced)) => trace!(
            "デバウンスにより {} の押下を捨てた（{}ms 以内）",
            action.label(),
            HOTKEY_DEBOUNCE.as_millis()
        ),
        Some((action, PressRouting::Unfocused)) => {
            trace!("フォーカスが無いので {} の押下を捨てた", action.label())
        }
        Some((action, PressRouting::Typing)) => {
            trace!("テキスト入力中なので {} の押下を捨てた", action.label())
        }
        None => {}
    }
}

/// キー入力を観測するスレッドを 1 本起動する。
///
/// 低レベルキーボードフックはこのスレッドに登録し、このスレッドの
/// メッセージループの中で呼ばれる。**リスナーはアプリ全体で 1 本だけにする。**
/// 何本も作ると 1 回のキー入力が全てのフックを順に通り、他のアプリの入力を
/// そのぶん遅らせる。
///
/// フックを登録できたかどうかを待ってから返す。登録できなかったときは
/// スレッドはすぐに終わり、理由を返す。
pub(super) fn spawn_listener(
    state: Arc<Mutex<ListenerState>>,
    shutdown: Arc<AtomicBool>,
) -> (JoinHandle<()>, Result<(), KeyboardHookError>) {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        debug!("ホットキーのリスナースレッドを開始した");

        let hook = match KeyboardHook::install() {
            Ok(hook) => {
                // 受け手（spawn_listener）は結果を受け取るまで待っているので、
                // 送れないことはない
                let _ = ready_tx.send(Ok(()));
                hook
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                debug!("キーボードフックを登録できないのでリスナースレッドを終える");
                return;
            }
        };

        while !shutdown.load(Ordering::Acquire) {
            let pumped = hook.pump(LISTENER_WAIT_TIMEOUT, |chord| {
                handle_key_down(&state, chord)
            });
            if !pumped {
                // 待てないまま回り続けると CPU を使い切るので抜ける。
                // 以降ホットキーは効かなくなるが、他のアプリの入力は妨げない
                let source = std::io::Error::last_os_error().to_string();
                error!(
                    "ホットキーのリスナーがキー入力を待てないので終了する: {}",
                    source
                );
                // UI スレッドの apply が拾い、HookUnavailable として画面に出す
                match state.lock() {
                    Ok(mut state) => {
                        state.listener_failure = Some(KeyboardHookError::WaitFailed(source))
                    }
                    Err(_) => {
                        warn!("ホットキーの共有状態のロックを取得できないので停止を伝えられない")
                    }
                }
                break;
            }
        }

        // ここでフックを外す
        drop(hook);
        debug!("ホットキーのリスナースレッドを終了した");
    });

    // スレッドが結果を送る前に終わった場合（起動直後のパニック）だけ受け取れない
    let ready = ready_rx
        .recv()
        .unwrap_or(Err(KeyboardHookError::ListenerStopped));
    (handle, ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::parse::VK_F1;
    use crate::keyboard_hook::Modifiers;

    // テストで使う仮想キーコード。英字は ASCII の大文字と同じ値
    const VK_A: u32 = 0x41;
    const VK_F5: u32 = 0x74;

    // ---- リスナースレッドの押下の照合とデバウンス ----

    const F5: KeyChord = KeyChord {
        modifiers: Modifiers::empty(),
        vk: VK_F5,
    };
    const CTRL_F11: KeyChord = KeyChord {
        modifiers: Modifiers::CONTROL,
        vk: VK_F1 + 10,
    };

    fn registered_chords() -> HashMap<KeyChord, HotkeyAction> {
        HashMap::from([
            (F5, HotkeyAction::Screenshot),
            (CTRL_F11, HotkeyAction::ToggleFullscreen),
        ])
    }

    #[test]
    fn accepted_action_matching_chord_returns_that_action() {
        assert_eq!(
            accepted_action(&registered_chords(), F5),
            Some(HotkeyAction::Screenshot)
        );
        assert_eq!(
            accepted_action(&registered_chords(), CTRL_F11),
            Some(HotkeyAction::ToggleFullscreen)
        );
    }

    #[test]
    fn accepted_action_extra_modifier_returns_none() {
        // F5 の割り当ては Ctrl+F5 では反応しない（RegisterHotKey と同じ）。
        // 他のアプリの Ctrl+F5（再読み込みなど）で撮られると困る
        let ctrl_f5 = KeyChord {
            modifiers: Modifiers::CONTROL,
            vk: VK_F5,
        };
        assert_eq!(accepted_action(&registered_chords(), ctrl_f5), None);
    }

    #[test]
    fn accepted_action_missing_modifier_returns_none() {
        // Ctrl+F11 の割り当ては F11 単独では反応しない
        let f11 = KeyChord {
            modifiers: Modifiers::empty(),
            vk: VK_F1 + 10,
        };
        assert_eq!(accepted_action(&registered_chords(), f11), None);
    }

    #[test]
    fn accepted_action_unassigned_key_returns_none() {
        // 他のアプリへ打っている文字も全てリスナーを通る
        let a = KeyChord {
            modifiers: Modifiers::empty(),
            vk: VK_A,
        };
        assert_eq!(accepted_action(&registered_chords(), a), None);
    }

    #[test]
    fn accepted_action_without_registration_returns_none() {
        // リスナーは登録前から動いている。何も登録していない間は反応しない
        assert_eq!(accepted_action(&HashMap::new(), F5), None);
    }

    #[test]
    fn decide_trigger_first_time_fires() {
        // まだ 1 度も実行していないアクションは、経過時間を待たずに実行する
        assert_eq!(decide_trigger(None, HOTKEY_DEBOUNCE), TriggerDecision::Fire);
    }

    #[test]
    fn decide_trigger_after_debounce_fires() {
        assert_eq!(
            decide_trigger(Some(Duration::from_millis(201)), Duration::from_millis(200)),
            TriggerDecision::Fire
        );
    }

    #[test]
    fn decide_trigger_at_debounce_boundary_is_debounced() {
        // 経過がちょうど デバウンス時間 のときは抑止する（判定は「超えたら実行」）
        assert_eq!(
            decide_trigger(Some(Duration::from_millis(200)), Duration::from_millis(200)),
            TriggerDecision::Debounced
        );
    }

    #[test]
    fn decide_trigger_within_debounce_is_debounced() {
        // キーリピートで連続して届いた場合
        assert_eq!(
            decide_trigger(Some(Duration::ZERO), Duration::from_millis(200)),
            TriggerDecision::Debounced
        );
    }

    #[test]
    fn spawn_listener_stops_after_shutdown_request() {
        // 終了要求を待ちのタイムアウトで拾えること。拾えないと
        // join が返らず、アプリが終了できなくなる
        let state = Arc::new(Mutex::new(ListenerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        // フックを登録できない環境でも、スレッドが終わることは確かめられる
        let (handle, _ready) = spawn_listener(Arc::clone(&state), Arc::clone(&shutdown));

        shutdown.store(true, Ordering::Release);
        let started = Instant::now();
        handle.join().expect("リスナースレッドが正常に終わること");

        // 待ち時間はタイムアウト 1 回ぶんが上限。CI の遅さを見込んで
        // 4 倍を上限にしている
        assert!(
            started.elapsed() < LISTENER_WAIT_TIMEOUT * 4,
            "終了までに {:?} かかった",
            started.elapsed()
        );
        // 何も登録していないので押下は記録されない
        assert!(state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .is_empty());
    }

    // ---- 押下の記録（デバウンスと最小化中の振り分け） ----

    #[test]
    fn record_press_within_debounce_is_dropped() {
        // キーリピートで連続して届いた場合。1 回目だけ数える
        let mut state = ListenerState::default();
        let now = Instant::now();

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, now),
            PressRouting::Deferred
        );
        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, now + HOTKEY_DEBOUNCE),
            PressRouting::Debounced
        );

        assert_eq!(state.pressed.get(&HotkeyAction::Screenshot), Some(&1));
    }

    #[test]
    fn record_press_after_debounce_counts_again() {
        // 最小化中は取り出す側が居ないので、回数が積み上がる
        let mut state = ListenerState::default();
        let now = Instant::now();

        state.record_press(HotkeyAction::ToggleFullscreen, now);
        state.record_press(
            HotkeyAction::ToggleFullscreen,
            now + HOTKEY_DEBOUNCE + Duration::from_millis(1),
        );

        assert_eq!(state.pressed.get(&HotkeyAction::ToggleFullscreen), Some(&2));
    }

    #[test]
    fn record_press_debounce_is_per_action() {
        // スクリーンショットを撮った直後でも、別のアクションは抑止されない
        let mut state = ListenerState::default();
        let now = Instant::now();

        state.record_press(HotkeyAction::Screenshot, now);

        assert_eq!(
            state.record_press(HotkeyAction::VolumeUp, now),
            PressRouting::Deferred
        );
    }

    #[test]
    fn record_press_while_minimized_runs_ui_free_actions_in_background() {
        // 最小化中の音量・ミュート・再接続は復帰を待たずに実行する（#133）。
        // 保留にも残さない（残すと復帰したときに二重で効く）
        let mut state = ListenerState {
            minimized: true,
            ..Default::default()
        };

        assert_eq!(
            state.record_press(HotkeyAction::ToggleMute, Instant::now()),
            PressRouting::Background
        );
        assert!(state.pressed.is_empty(), "保留にも残っている");
    }

    #[test]
    fn record_press_while_minimized_defers_actions_that_need_the_window() {
        // 画面が要るものは最小化中に実行しても意味がないので溜める
        let mut state = ListenerState {
            minimized: true,
            ..Default::default()
        };

        assert_eq!(
            state.record_press(HotkeyAction::ToggleFullscreen, Instant::now()),
            PressRouting::Deferred
        );
        assert_eq!(state.pressed.get(&HotkeyAction::ToggleFullscreen), Some(&1));
    }

    #[test]
    fn record_press_when_not_minimized_defers_even_ui_free_actions() {
        // 最小化していなければ UI スレッドが実行する。ワーカーへ回すと
        // 右クリックメニューと経路が変わってしまう
        let mut state = ListenerState::default();

        assert_eq!(
            state.record_press(HotkeyAction::ToggleMute, Instant::now()),
            PressRouting::Deferred
        );
    }

    // ---- フォーカスがあるときだけ反応する ----

    #[test]
    fn listener_state_default_reacts_without_focus() {
        // 既定はオフ。他のアプリを操作している間も効く（#133 の挙動を保つ）
        let mut state = ListenerState {
            focused: false,
            ..Default::default()
        };

        assert!(!state.only_when_focused);
        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, Instant::now()),
            PressRouting::Deferred
        );
    }

    #[test]
    fn record_press_only_when_focused_drops_presses_without_focus() {
        // 他のアプリにフォーカスがある間は反応しない。保留にも残さない
        // （残すと、戻ってきたときに実行される）
        let mut state = ListenerState {
            only_when_focused: true,
            focused: false,
            ..Default::default()
        };

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, Instant::now()),
            PressRouting::Unfocused
        );
        assert!(state.pressed.is_empty(), "保留に残っている");
    }

    #[test]
    fn record_press_only_when_focused_accepts_presses_with_focus() {
        let mut state = ListenerState {
            only_when_focused: true,
            focused: true,
            ..Default::default()
        };

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, Instant::now()),
            PressRouting::Deferred
        );
    }

    #[test]
    fn record_press_only_when_focused_treats_minimized_as_unfocused() {
        // 最小化中は前面にいない。フォーカスの旗が古いまま真でも、
        // 音量などをワーカーへ回さない
        let mut state = ListenerState {
            only_when_focused: true,
            focused: true,
            minimized: true,
            ..Default::default()
        };

        assert_eq!(
            state.record_press(HotkeyAction::VolumeUp, Instant::now()),
            PressRouting::Unfocused
        );
    }

    #[test]
    fn record_press_unfocused_does_not_start_the_debounce() {
        // 捨てた押下でデバウンスの基準を更新すると、フォーカスを戻した
        // 直後の押下まで捨てられる
        let mut state = ListenerState {
            only_when_focused: true,
            focused: false,
            ..Default::default()
        };
        let now = Instant::now();
        state.record_press(HotkeyAction::Screenshot, now);

        state.focused = true;

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, now),
            PressRouting::Deferred
        );
    }

    // ---- テキスト入力中は反応しない（#206） ----

    #[test]
    fn rejected_by_window_state_accepts_when_nothing_applies() {
        assert_eq!(rejected_by_window_state(false, true, false, false), None);
        assert_eq!(rejected_by_window_state(true, true, false, false), None);
        // 既定（オフ）なら前面にいなくても受け付ける
        assert_eq!(rejected_by_window_state(false, false, false, false), None);
        assert_eq!(rejected_by_window_state(false, true, true, false), None);
    }

    #[test]
    fn rejected_by_window_state_drops_presses_while_typing() {
        // 設定ダイアログのテキスト欄へ打った文字を実行しない。
        // 「フォーカスがあるときだけ反応する」の設定に関係なく捨てる
        assert_eq!(
            rejected_by_window_state(false, true, false, true),
            Some(PressRouting::Typing)
        );
        assert_eq!(
            rejected_by_window_state(true, true, false, true),
            Some(PressRouting::Typing)
        );
    }

    #[test]
    fn rejected_by_window_state_ignores_typing_without_focus() {
        // egui はウィンドウがフォーカスを失ってもテキスト欄のフォーカスを
        // 手放さない。入力欄を選んだまま他のアプリへ移っても効かせる
        assert_eq!(rejected_by_window_state(false, false, false, true), None);
    }

    #[test]
    fn rejected_by_window_state_ignores_typing_while_minimized() {
        // 最小化中は入力欄へ打てない。旗が真のまま残っていても、
        // 最小化中の実行（#133）を止めない
        assert_eq!(rejected_by_window_state(false, true, true, true), None);
    }

    #[test]
    fn rejected_by_window_state_reports_unfocused_before_typing() {
        // 前面にいないときの理由は「フォーカスが無い」。入力中の旗は見ない
        assert_eq!(
            rejected_by_window_state(true, false, false, true),
            Some(PressRouting::Unfocused)
        );
        assert_eq!(
            rejected_by_window_state(true, true, true, true),
            Some(PressRouting::Unfocused)
        );
    }

    #[test]
    fn record_press_while_typing_is_not_kept_and_does_not_start_the_debounce() {
        // 保留に残すと入力欄から離れたときに実行される。デバウンスの
        // 基準を更新すると、離れた直後の押下まで捨てられる
        let mut state = ListenerState {
            typing: true,
            ..Default::default()
        };
        let now = Instant::now();

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, now),
            PressRouting::Typing
        );
        assert!(state.pressed.is_empty(), "保留に残っている");

        state.typing = false;

        assert_eq!(
            state.record_press(HotkeyAction::Screenshot, now),
            PressRouting::Deferred
        );
    }
}
