use super::listener::{spawn_listener, ListenerState};
use super::{HotkeyAction, HotkeyAssignmentError};
use crate::keyboard_hook::{KeyChord, KeyboardHookError};
use crate::repaint::RepaintWaker;
use log::{error, info, warn};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 止まったリスナーを作り直す回数の上限（#508）。
///
/// これを超えて失敗が続くなら作り直しをやめ、`hook_error` を残したまま
/// 「接続状態」タブとトーストに出し続ける。作り直しても直らない原因で、
/// スレッドとフックを作り直し続けないため。
pub(super) const LISTENER_RESTART_LIMIT: u32 = 3;

/// 失敗してから作り直すまでの間隔。
///
/// 再適用（2 秒ごと）のたびに作り直すと、直らない原因のときに上限を数秒で
/// 使い切る。失敗をいったん画面に出してから直すので、利用者は一度止まったことを知れる。
pub(super) const LISTENER_RESTART_INTERVAL: Duration = Duration::from_secs(10);

/// 作り直したリスナーがこれだけ動き続けたら、次に止まったときは回数を数え直す。
///
/// 長く使っている間にまれに止まるだけなら、そのたびに作り直してよい。
/// 数え直さないと、何時間も使ううちに上限へ届いて二度と戻らなくなる。
pub(super) const LISTENER_STABLE_PERIOD: Duration = Duration::from_secs(60);

/// 止まったリスナーをどうするか（`decide_listener_restart`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListenerRestart {
    /// いま作り直す
    Restart,
    /// 失敗してから間もないので、次の再適用まで待つ
    Wait,
    /// 作り直しても直らないので、もう作り直さない
    GiveUp,
}

/// 止まったリスナーを作り直すかを決める。
///
/// `restarts` はこれまでに作り直した回数、`since_failure` は最後に失敗してからの時間。
/// `Unsupported`（Windows 以外）は作り直してもフックが無いので作り直さない。
pub(super) fn decide_listener_restart(
    error: &KeyboardHookError,
    restarts: u32,
    since_failure: Duration,
) -> ListenerRestart {
    if matches!(error, KeyboardHookError::Unsupported) || restarts >= LISTENER_RESTART_LIMIT {
        return ListenerRestart::GiveUp;
    }
    if since_failure < LISTENER_RESTART_INTERVAL {
        return ListenerRestart::Wait;
    }
    ListenerRestart::Restart
}

/// 動いていたリスナーが止まったときの、作り直した回数の数え直し。
///
/// `ran_for` はそのリスナーが動き始めてから止まるまでの時間。
/// `LISTENER_STABLE_PERIOD` 以上動いていたなら、作り直しで直る種類の失敗と
/// みなして 0 から数え直す。
pub(super) fn restarts_after_failure(restarts: u32, ran_for: Duration) -> u32 {
    if ran_for >= LISTENER_STABLE_PERIOD {
        0
    } else {
        restarts
    }
}

/// リスナーの作り直しの記録。UI スレッドだけが触る。
#[derive(Debug, Default)]
pub(super) struct ListenerRecovery {
    /// 作り直した回数
    pub(super) restarts: u32,
    /// 最後に失敗した時刻（起動時のフックの登録の失敗、止まったのを拾った時刻、作り直しの失敗）
    pub(super) failed_at: Option<Instant>,
    /// いまのリスナーがフックを付けて動き始めた時刻
    pub(super) started_at: Option<Instant>,
    /// 作り直しをやめたことをログに出したか。同じ行を 2 秒ごとに積まないため
    pub(super) gave_up_logged: bool,
}

/// 最小化中のアクションを、UI スレッドを介さずに実行するための窓口。
///
/// 中身の組み立ては `app` 側（`app::hotkeys::background_hotkey_runner`）が
/// 持つ。ここでは「押されたアクションを渡す先」としてだけ扱い、
/// `DeviceCommand` のような `app` の型をこのモジュールへ持ち込まない。
///
/// **リスナースレッドから呼ばれる。** 渡す処理はデバイスワーカーへ
/// コマンドを送るだけにして、その場でブロックしないこと。
///
/// 既定は「何もしない」。渡さなければ、最小化中のアクションも復帰まで
/// 保留される（#133 を直す前と同じ振る舞い）。
#[derive(Clone, Default)]
pub struct BackgroundHotkeyRunner {
    run: Option<Arc<dyn Fn(HotkeyAction) + Send + Sync>>,
}

impl BackgroundHotkeyRunner {
    pub fn new(run: impl Fn(HotkeyAction) + Send + Sync + 'static) -> Self {
        Self {
            run: Some(Arc::new(run)),
        }
    }

    /// アクションを実行させる。窓口が渡されていなければ何もしない。
    pub(super) fn run(&self, action: HotkeyAction) {
        if let Some(run) = &self.run {
            run(action);
        }
    }
}

/// ホットキーの登録と押下の検出。
///
/// **押下は低レベルキーボードフックで観測し、キーを奪わない**
/// （`crate::keyboard_hook`、#202）。ここでの「登録」は OS へ登録することでは
/// なく、リスナーが照合に使う表へ載せることを指す。
///
/// **UI スレッドだけが触るので `Mutex` で包まない。** リスナースレッドと
/// 共有するのは内部の `Arc<Mutex<ListenerState>>` だけで、そこには
/// 登録中の組み合わせと押下の記録しか入っていない。
pub struct HotkeyManager {
    /// フックを使えないときの理由。使えていれば `None`。
    ///
    /// リスナーの起動時と、止まったリスナーを拾ったとき・作り直したときに決まる。
    /// 使えないときは、割り当てのたびにこの理由で失敗として記録し、
    /// 設定画面とトーストに出す。
    pub(super) hook_error: Option<KeyboardHookError>,
    /// 止まったリスナーの作り直しの記録（#508）
    pub(super) recovery: ListenerRecovery,
    /// 登録に成功しているアクション → (ホットキー文字列, キーの組み合わせ)
    pub(super) registered: BTreeMap<HotkeyAction, (String, KeyChord)>,
    /// 登録できなかったアクション → 理由
    pub(super) errors: BTreeMap<HotkeyAction, HotkeyAssignmentError>,
    pub(super) state: Arc<Mutex<ListenerState>>,
    /// リスナースレッドへの終了要求
    pub(super) listener_shutdown: Arc<AtomicBool>,
    /// リスナースレッドのハンドル。`Drop` で join するために持つ
    pub(super) listener: Option<JoinHandle<()>>,
    /// ホットキー入力ダイアログのために一時解除しているか。
    ///
    /// 一時停止中は `apply` を呼んでも何もしない。2 秒ごとの再適用
    /// （`apply_settings`）が動き続けていても、一時停止中に登録し直されて
    /// しまわないようにするため。
    pub(super) paused: bool,
}

impl HotkeyManager {
    /// ホットキーのリスナースレッドを起動して `HotkeyManager` を作る。
    ///
    /// この時点ではまだ何も登録していないので、リスナーは観測した押下を
    /// すべて捨てる。登録は `apply` が行う。
    /// スレッドを止めるのは `Drop` だけなので、**アプリ全体で 1 つだけ作ること。**
    ///
    /// キーボードフックを登録できたかを待ってから返す（数 ms）。
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(ListenerState::default()));
        let listener_shutdown = Arc::new(AtomicBool::new(false));

        // リスナーはここで 1 本だけ起動し、登録のたびには作り直さない。
        // フックはリスナースレッドに紐づくので、作り直すとフックも
        // 付け直しになり、その間のキー入力を取りこぼす
        let (listener, ready) = spawn_listener(Arc::clone(&state), Arc::clone(&listener_shutdown));
        let now = Instant::now();
        let mut recovery = ListenerRecovery::default();
        let hook_error = match ready {
            Ok(()) => {
                recovery.started_at = Some(now);
                None
            }
            Err(e) => {
                error!("ホットキーのキーボードフックを登録できない: {}", e);
                recovery.failed_at = Some(now);
                Some(e)
            }
        };

        Self {
            hook_error,
            recovery,
            registered: BTreeMap::new(),
            errors: BTreeMap::new(),
            state,
            listener_shutdown,
            listener: Some(listener),
            paused: false,
        }
    }

    /// 止まったリスナーを作り直す（#508）。止まっていなければ何もしない。
    ///
    /// `apply` が登録の前に呼ぶ。作り直すかは `decide_listener_restart` が決める。
    /// 作り直せたら `hook_error` を消すので、続く登録で割り当てが元に戻り、
    /// 失敗の表示も取り下げられる。
    ///
    /// **前のスレッドが終わってから作る。** 終わる前に作ると、一時的にでも
    /// リスナーとフックが 2 本になる（`docs/design/threads.md`）。まだ終わって
    /// いなければ次の呼び出しで見直す。join は終わったスレッドを回収するだけ
    /// なので待たない。共有状態（`ListenerState`）は作り直さず、同じものを
    /// 新しいリスナーへ渡す。
    pub(super) fn recover_listener(&mut self, now: Instant) {
        let Some(error) = &self.hook_error else {
            return;
        };
        if self
            .listener
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        let since_failure = self
            .recovery
            .failed_at
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        match decide_listener_restart(error, self.recovery.restarts, since_failure) {
            ListenerRestart::Wait => return,
            ListenerRestart::GiveUp => {
                if !self.recovery.gave_up_logged {
                    self.recovery.gave_up_logged = true;
                    warn!(
                        "ホットキーのリスナーをこれ以上作り直さない（{} 回作り直した）: {}",
                        self.recovery.restarts, error
                    );
                }
                return;
            }
            ListenerRestart::Restart => {}
        }

        if let Some(handle) = self.listener.take() {
            if handle.join().is_err() {
                // release ビルドは panic = "abort" なのでここには来ない
                warn!("ホットキーのリスナースレッドがパニックした");
            }
        }

        self.recovery.restarts += 1;
        info!(
            "止まったホットキーのリスナーを作り直す（{}/{} 回目）",
            self.recovery.restarts, LISTENER_RESTART_LIMIT
        );
        let (listener, ready) =
            spawn_listener(Arc::clone(&self.state), Arc::clone(&self.listener_shutdown));
        self.listener = Some(listener);
        match ready {
            Ok(()) => {
                info!("ホットキーのリスナーを作り直した");
                self.hook_error = None;
                self.recovery.started_at = Some(now);
            }
            Err(e) => {
                error!(
                    "作り直したホットキーのリスナーでキーボードフックを登録できない: {}",
                    e
                );
                self.hook_error = Some(e);
                self.recovery.failed_at = Some(now);
            }
        }
    }

    /// 押下を検出したときに UI スレッドを起こすための窓口を渡す。
    ///
    /// リスナースレッドとは `ListenerState` を通して共有するので、
    /// スレッドを起動したあとでも差し替えられる。
    pub fn set_repaint_waker(&mut self, waker: RepaintWaker) {
        match self.state.lock() {
            Ok(mut state) => state.waker = waker,
            // 起こせないだけで押下の検出は続く。反応が最大 250ms 遅れる
            Err(_) => warn!("ホットキーの共有状態のロックを取得できないので再描画の窓口を渡せない"),
        }
    }

    /// 最小化中のアクションを UI スレッドを介さずに実行する窓口を渡す。
    ///
    /// 渡さなければ、最小化中のアクションも他と同じように復帰まで保留される。
    pub fn set_background_runner(&mut self, runner: BackgroundHotkeyRunner) {
        match self.state.lock() {
            Ok(mut state) => state.background = runner,
            Err(_) => warn!("ホットキーの共有状態のロックを取得できないので実行の窓口を渡せない"),
        }
    }

    /// ウィンドウが最小化されているか、キーボードフォーカスがあるか、
    /// テキスト欄に入力中かを伝える。**毎フレーム呼ぶ。**
    ///
    /// 最小化すると `update()` が呼ばれなくなるので、最後に書き込んだ値が
    /// そのまま残る。リスナーはその値を見て、画面の要らないアクションだけを
    /// `BackgroundHotkeyRunner` へ回す（#133）。フォーカスは「フォーカスが
    /// あるときだけ反応する」がオンのときの判定に使う（#202）。入力中の間は
    /// 押下を捨てる（#206）。
    pub fn set_window_state(&mut self, minimized: bool, focused: bool, typing: bool) {
        match self.state.lock() {
            Ok(mut state) => {
                // 入力を始めたフレームに届いた押下を捨てる。この旗はフレームの
                // 末尾で書くので、テキスト欄にフォーカスが移ってからここまでの
                // 間の押下は、まだ偽のままの旗で保留に積まれている。そのキーは
                // 次のフレームで egui がテキスト欄へ入れるので、実行すると
                // 打った文字がホットキーとしても効いてしまう（#206）。
                // 保留はフレームの先頭（`take_pressed`）で空にしているので、
                // ここに残っているのはこのフレームの間の押下だけ
                let started_typing = !state.typing && typing;
                state.minimized = minimized;
                state.focused = focused;
                state.typing = typing;
                if started_typing {
                    state.pressed.clear();
                }
            }
            // 最小化中のアクションが復帰まで保留されるだけで、検出は続く
            Err(_) => {
                warn!(
                    "ホットキーの共有状態のロックを取得できないのでウィンドウの状態を伝えられない"
                )
            }
        }
    }

    /// 「フォーカスがあるときだけ反応する」を切り替える。
    ///
    /// 設定の反映（`apply_settings`）のたびに呼ばれる。値を書くだけなので
    /// 何度呼んでもよい。
    pub fn set_only_when_focused(&mut self, only_when_focused: bool) {
        match self.state.lock() {
            Ok(mut state) => state.only_when_focused = only_when_focused,
            Err(_) => warn!(
                "ホットキーの共有状態のロックを取得できないのでフォーカスの扱いを切り替えられない"
            ),
        }
    }
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        // 先に全てのホットキーを解除してからリスナーを止める
        let registered: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in registered {
            self.unregister(action);
        }

        self.listener_shutdown.store(true, Ordering::Release);
        let Some(handle) = self.listener.take() else {
            return;
        };

        // 終了要求は待ちのタイムアウトで拾うため、待ち時間は
        // 最大で LISTENER_WAIT_TIMEOUT。ウィンドウを閉じたあとの待ちなので
        // 画面上は見えない。切り離すとプロセスが終わるまでスレッドが残り、
        // フックも外れない
        if handle.join().is_err() {
            // release ビルドは panic = "abort" なのでここには来ない
            warn!("ホットキーのリスナースレッドがパニックした");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_window_state_and_only_when_focused_reach_the_listener() {
        let mut manager = HotkeyManager::new();

        manager.set_window_state(true, false, true);
        manager.set_only_when_focused(true);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert!(state.minimized);
        assert!(!state.focused);
        assert!(state.typing);
        assert!(state.only_when_focused);
    }

    #[test]
    fn set_window_state_drops_pending_presses_when_typing_starts() {
        // 旗を書く前（入力を始めたフレームの間）に保留へ積まれた押下は、
        // 次のフレームでテキスト欄へ入る文字なので実行しない
        let mut manager = HotkeyManager::new();
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .insert(HotkeyAction::VolumeUp, 1);

        manager.set_window_state(false, true, true);

        assert!(manager.take_pressed().is_empty());
    }

    #[test]
    fn set_window_state_keeps_pending_presses_unless_typing_starts() {
        // 入力中のままの間や入力を終えたときは、保留を捨てない。
        // 入力欄を選んだまま他のアプリで押したものも含まれる
        for (before, after) in [(false, false), (true, true), (true, false)] {
            let mut manager = HotkeyManager::new();
            manager.set_window_state(false, true, before);
            manager
                .state
                .lock()
                .expect("ロックが毒されていないこと")
                .pressed
                .insert(HotkeyAction::VolumeUp, 1);

            manager.set_window_state(false, true, after);

            assert_eq!(
                manager.take_pressed(),
                vec![HotkeyAction::VolumeUp],
                "{before} → {after} で保留が消えた"
            );
        }
    }

    // ---- 最小化中の実行の窓口 ----

    #[test]
    fn background_runner_passes_the_action_through() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let runner = BackgroundHotkeyRunner::new(move |action| {
            recorded
                .lock()
                .expect("ロックが毒されていないこと")
                .push(action);
        });

        runner.run(HotkeyAction::VolumeUp);

        assert_eq!(
            *seen.lock().expect("ロックが毒されていないこと"),
            vec![HotkeyAction::VolumeUp]
        );
    }

    #[test]
    fn background_runner_default_does_nothing() {
        // 窓口を渡し忘れても落ちない。最小化中のアクションが効かなくなるだけ
        BackgroundHotkeyRunner::default().run(HotkeyAction::ToggleMute);
    }

    // ---- 止まったリスナーの作り直し（#508） ----

    fn wait_failed() -> KeyboardHookError {
        KeyboardHookError::WaitFailed("failed".to_string())
    }

    #[test]
    fn decide_listener_restart_waits_until_the_interval_passes() {
        // 失敗した直後は作り直さない。2 秒ごとの再適用で上限を使い切らないため
        let just_before = LISTENER_RESTART_INTERVAL - Duration::from_millis(1);
        assert_eq!(
            decide_listener_restart(&wait_failed(), 0, Duration::ZERO),
            ListenerRestart::Wait
        );
        assert_eq!(
            decide_listener_restart(&wait_failed(), 0, just_before),
            ListenerRestart::Wait
        );
        assert_eq!(
            decide_listener_restart(&wait_failed(), 0, LISTENER_RESTART_INTERVAL),
            ListenerRestart::Restart
        );
    }

    #[test]
    fn decide_listener_restart_restarts_every_recoverable_failure() {
        // 起動時の登録の失敗も、動いていたリスナーが止まったのも作り直す
        for error in [
            KeyboardHookError::InstallFailed("failed".to_string()),
            KeyboardHookError::ListenerStopped,
            wait_failed(),
        ] {
            assert_eq!(
                decide_listener_restart(&error, 0, LISTENER_RESTART_INTERVAL),
                ListenerRestart::Restart,
                "{error:?}"
            );
        }
    }

    #[test]
    fn decide_listener_restart_gives_up_at_the_limit() {
        // 上限の手前までは作り直し、上限に届いたら間隔に関係なくやめる
        assert_eq!(
            decide_listener_restart(
                &wait_failed(),
                LISTENER_RESTART_LIMIT - 1,
                LISTENER_RESTART_INTERVAL
            ),
            ListenerRestart::Restart
        );
        assert_eq!(
            decide_listener_restart(&wait_failed(), LISTENER_RESTART_LIMIT, Duration::MAX),
            ListenerRestart::GiveUp
        );
        assert_eq!(
            decide_listener_restart(&wait_failed(), LISTENER_RESTART_LIMIT, Duration::ZERO),
            ListenerRestart::GiveUp
        );
    }

    #[test]
    fn decide_listener_restart_never_restarts_without_hook_support() {
        // Windows 以外。作り直してもフックは無い
        assert_eq!(
            decide_listener_restart(&KeyboardHookError::Unsupported, 0, Duration::MAX),
            ListenerRestart::GiveUp
        );
    }

    #[test]
    fn restarts_after_failure_resets_only_after_a_stable_run() {
        // 長く動いてから止まったなら数え直す。すぐ止まったなら数え続ける
        let just_before = LISTENER_STABLE_PERIOD - Duration::from_millis(1);
        assert_eq!(restarts_after_failure(2, just_before), 2);
        assert_eq!(restarts_after_failure(2, LISTENER_STABLE_PERIOD), 0);
        assert_eq!(restarts_after_failure(0, Duration::ZERO), 0);
    }
}
