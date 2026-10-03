//! ホットキーのリスナースレッドの本体と、押下を受け取る方式の切り替え（#207）。
//!
//! 押下の受け取り方は 2 つある。どちらも**同じリスナースレッド 1 本**の
//! メッセージループで受け取り、照合から先（`listener::handle_key_down`）は同じ。
//!
//! | 方式 | 受け取り方 | キー |
//! |---|---|---|
//! | `HotkeyMethod::Hook`（既定） | 低レベルキーボードフック（`crate::keyboard_hook`） | 奪わない |
//! | `HotkeyMethod::RegisterHotKey` | `RegisterHotKey` の `WM_HOTKEY`（`crate::system_hotkey`） | 奪う |
//!
//! フックも `RegisterHotKey` も、登録したスレッドに紐づく。そのため登録と解除は
//! UI スレッドからの要求（`SyncRequest`）としてリスナーへ渡し、リスナーが
//! 自分のスレッドで行う。

use super::listener::{handle_key_down, ListenerState, PressSource};
use crate::keyboard_hook::{self, KeyChord, KeyboardHook, KeyboardHookError, ListenerMessage};
use crate::system_hotkey::SystemHotkey;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// リスナースレッドがキー入力を待つ時間。
///
/// タイムアウトするたびに終了要求と UI スレッドからの要求を確認する。
/// 終了を要求してからスレッドが実際に止まるまで最大でこの時間かかるが、
/// `Listener::stop` は起こしてから待つので、普段はすぐに止まる。キー入力が
/// あればタイムアウトを待たずに起きるので、押下の反応はこの長さに左右されない。
const LISTENER_WAIT_TIMEOUT: Duration = Duration::from_millis(200);

/// UI スレッドがリスナーの返事を待つ上限。
///
/// リスナーは登録と解除をするだけなので、普段は数 ms で返る。これを超えるのは
/// リスナーが止まっているときで、そのときは使えないものとして扱う。
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// `RegisterHotKey` に渡す番号の範囲（アプリが使ってよいのは 0x0000〜0xBFFF）。
const FIRST_HOTKEY_ID: i32 = 1;
const LAST_HOTKEY_ID: i32 = 0xBFFF;

/// 押下を受け取る方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HotkeyMethod {
    /// 低レベルキーボードフックで観測する。キーを奪わない
    #[default]
    Hook,
    /// `RegisterHotKey` で登録する。キーを奪う。管理者として実行しているアプリが
    /// 前面にあっても効く（#207）
    RegisterHotKey,
}

impl HotkeyMethod {
    /// 設定（`[hotkey_settings] use_register_hotkey`）から方式を決める。
    pub fn from_setting(use_register_hotkey: bool) -> Self {
        if use_register_hotkey {
            Self::RegisterHotKey
        } else {
            Self::Hook
        }
    }

    /// ログに出す名前。
    pub(super) fn log_name(self) -> &'static str {
        match self {
            Self::Hook => "低レベルキーボードフック",
            Self::RegisterHotKey => "RegisterHotKey",
        }
    }
}

/// UI スレッドからリスナーへの要求。「この方式で、この組み合わせを」受け取れる
/// 状態にしてほしい、という最終形だけを渡し、差分はリスナーが取る（`plan_sync`）。
struct SyncRequest {
    method: HotkeyMethod,
    /// `RegisterHotKey` で登録しておく組み合わせ。フックの方式では空
    chords: Vec<KeyChord>,
    reply: mpsc::SyncSender<SyncOutcome>,
}

/// 要求を処理した結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SyncOutcome {
    /// フックの方式なのにフックを登録できなかった理由
    pub(super) hook_error: Option<KeyboardHookError>,
    /// `RegisterHotKey` で登録できなかった組み合わせと OS のエラー文。
    /// 他のアプリが同じ組み合わせを登録済みのときなど
    pub(super) failed: Vec<(KeyChord, String)>,
    /// この要求で新しく `RegisterHotKey` に登録できた組み合わせ。前から
    /// 登録してあったものは含まない（ログを登録できたときに 1 回だけ出すため）
    pub(super) registered: Vec<KeyChord>,
}

/// リスナーが行うことの一覧。`plan_sync` が決める。
#[derive(Debug, Default, PartialEq, Eq)]
struct SyncPlan {
    install_hook: bool,
    remove_hook: bool,
    unregister: Vec<KeyChord>,
    register: Vec<KeyChord>,
}

/// いまの状態（フックの有無と `RegisterHotKey` の登録）から、要求された形へ
/// 移すために何をするかを決める。
///
/// - フックの方式: フックが無ければ付ける。`RegisterHotKey` の登録は全て外す
///   （残すとキーを奪い続ける）
/// - `RegisterHotKey` の方式: フックがあれば外す（残すと 1 回の押下が両方から
///   届き 2 回動く）。登録は差分だけ行う。2 秒ごとの再適用のたびに外して
///   登録し直すと、その瞬間の押下を取りこぼす
///
/// 登録に失敗した組み合わせは `current` に入らないので、次の要求で試し直す。
fn plan_sync(
    method: HotkeyMethod,
    hook_installed: bool,
    current: &[KeyChord],
    desired: &[KeyChord],
) -> SyncPlan {
    match method {
        HotkeyMethod::Hook => SyncPlan {
            install_hook: !hook_installed,
            remove_hook: false,
            unregister: current.to_vec(),
            register: Vec::new(),
        },
        HotkeyMethod::RegisterHotKey => SyncPlan {
            install_hook: false,
            remove_hook: hook_installed,
            unregister: current
                .iter()
                .filter(|chord| !desired.contains(chord))
                .copied()
                .collect(),
            register: desired
                .iter()
                .filter(|chord| !current.contains(chord))
                .copied()
                .collect(),
        },
    }
}

/// リスナースレッドだけが持つ、OS へ登録したもの。
///
/// フックも `RegisterHotKey` も登録したスレッドでしか外せないので、
/// スレッドの外へは出さない。落とすと全て外れる。
#[derive(Default)]
struct Registrations {
    hook: Option<KeyboardHook>,
    system: Vec<(KeyChord, SystemHotkey)>,
    next_id: i32,
}

impl Registrations {
    fn sync(&mut self, method: HotkeyMethod, desired: &[KeyChord]) -> SyncOutcome {
        let current: Vec<KeyChord> = self.system.iter().map(|(chord, _)| *chord).collect();
        let plan = plan_sync(method, self.hook.is_some(), &current, desired);
        let mut outcome = SyncOutcome::default();

        if plan.remove_hook {
            self.hook = None;
            debug!("キーボードフックを外した");
        }
        // 落とすと UnregisterHotKey される
        self.system
            .retain(|(chord, _)| !plan.unregister.contains(chord));
        for chord in plan.register {
            let id = self.allocate_id();
            match SystemHotkey::register(id, chord) {
                Ok(hotkey) => {
                    self.system.push((chord, hotkey));
                    outcome.registered.push(chord);
                }
                Err(e) => outcome.failed.push((chord, e)),
            }
        }
        if plan.install_hook {
            match KeyboardHook::install() {
                Ok(hook) => {
                    self.hook = Some(hook);
                    debug!("キーボードフックを登録した");
                }
                Err(e) => outcome.hook_error = Some(e),
            }
        }
        outcome
    }

    /// まだ使っていない番号を返す。
    fn allocate_id(&mut self) -> i32 {
        loop {
            let id = if (FIRST_HOTKEY_ID..=LAST_HOTKEY_ID).contains(&self.next_id) {
                self.next_id
            } else {
                FIRST_HOTKEY_ID
            };
            self.next_id = id + 1;
            if !self.system.iter().any(|(_, hotkey)| hotkey.id() == id) {
                return id;
            }
        }
    }

    /// `WM_HOTKEY` の番号から組み合わせを引く。外したあとに届いたものは `None`。
    fn chord_for(&self, id: i32) -> Option<KeyChord> {
        self.system
            .iter()
            .find(|(_, hotkey)| hotkey.id() == id)
            .map(|(chord, _)| *chord)
    }

    fn handle_requests(&mut self, requests: &mpsc::Receiver<SyncRequest>) {
        while let Ok(request) = requests.try_recv() {
            let outcome = self.sync(request.method, &request.chords);
            // 待ちきれずに諦めた要求なら受け手がいない。次の要求で揃うので捨てる
            let _ = request.reply.send(outcome);
        }
    }
}

/// UI スレッドが持つ、リスナースレッドの窓口。
///
/// **リスナーはアプリ全体で 1 本だけにする。** 何本も作ると 1 回のキー入力が
/// 全てのフックを順に通り、他のアプリの入力をそのぶん遅らせる。
pub(super) struct Listener {
    /// リスナースレッドの番号。起こすメッセージの宛先。起動に失敗したら `None`
    thread_id: Option<u32>,
    requests: mpsc::Sender<SyncRequest>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Listener {
    /// リスナースレッドを 1 本起動する。
    ///
    /// 起動時はフックの方式で始める。フックを登録できたかを待ってから返す
    /// （数 ms）。**登録できなくてもスレッドは終わらない。** `RegisterHotKey`
    /// の方式へ切り替えれば使えるため。
    pub(super) fn spawn(state: Arc<Mutex<ListenerState>>) -> (Self, Result<(), KeyboardHookError>) {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (request_tx, request_rx) = mpsc::channel::<SyncRequest>();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            debug!("ホットキーのリスナースレッドを開始した");
            let thread_id = keyboard_hook::prepare_message_queue();

            let mut registrations = Registrations::default();
            let started = registrations.sync(HotkeyMethod::Hook, &[]);
            let ready = match started.hook_error {
                Some(e) => Err(e),
                None => Ok(()),
            };
            // 受け手（spawn）は結果を受け取るまで待っているので、送れないことはない
            let _ = ready_tx.send((thread_id, ready));

            run_listener(&state, &thread_shutdown, &request_rx, &mut registrations);

            // ここでフックと RegisterHotKey の登録を外す
            drop(registrations);
            debug!("ホットキーのリスナースレッドを終了した");
        });

        // スレッドが結果を送る前に終わった場合（起動直後のパニック）だけ受け取れない
        let (thread_id, ready) = match ready_rx.recv() {
            Ok((thread_id, ready)) => (Some(thread_id), ready),
            Err(_) => (None, Err(KeyboardHookError::ListenerStopped)),
        };
        let listener = Self {
            thread_id,
            requests: request_tx,
            shutdown,
            handle: Some(handle),
        };
        (listener, ready)
    }

    /// 方式と `RegisterHotKey` で登録しておく組み合わせを渡し、リスナーが
    /// 揃え終えるまで待つ。
    ///
    /// **UI スレッドが待つ。** リスナーは登録と解除をするだけなので数 ms で
    /// 返る。返らないとき（リスナーが止まっている）は理由を返す。
    pub(super) fn sync(
        &self,
        method: HotkeyMethod,
        chords: Vec<KeyChord>,
    ) -> Result<SyncOutcome, KeyboardHookError> {
        let Some(thread_id) = self.thread_id else {
            return Err(KeyboardHookError::ListenerStopped);
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let request = SyncRequest {
            method,
            chords,
            reply: reply_tx,
        };
        if self.requests.send(request).is_err() {
            return Err(KeyboardHookError::ListenerStopped);
        }
        keyboard_hook::wake_listener(thread_id);
        reply_rx
            .recv_timeout(SYNC_TIMEOUT)
            .map_err(|_| KeyboardHookError::ListenerStopped)
    }

    /// リスナースレッドを止めて join する。2 回目以降は何もしない。
    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Some(thread_id) = self.thread_id {
            keyboard_hook::wake_listener(thread_id);
        }
        // 起こせなくても終了要求は待ちのタイムアウトで拾うので、待ち時間は
        // 最大で LISTENER_WAIT_TIMEOUT。切り離すとプロセスが終わるまで
        // スレッドが残り、フックも外れない
        if handle.join().is_err() {
            // release ビルドは panic = "abort" なのでここには来ない
            warn!("ホットキーのリスナースレッドがパニックした");
        }
    }
}

/// リスナースレッドのメッセージループ。終了を要求されるか、待てなくなるまで回る。
fn run_listener(
    state: &Mutex<ListenerState>,
    shutdown: &AtomicBool,
    requests: &mpsc::Receiver<SyncRequest>,
    registrations: &mut Registrations,
) {
    while !shutdown.load(Ordering::Acquire) {
        // 要求はメッセージを待つ前にも処理する。起こすメッセージが届く前に
        // 積まれた要求も、ここで拾える
        registrations.handle_requests(requests);

        let registered = &*registrations;
        let pumped = keyboard_hook::pump_messages(LISTENER_WAIT_TIMEOUT, |message| match message {
            ListenerMessage::KeyDown(chord) => handle_key_down(state, chord, PressSource::Hook),
            // 照合から先はフックの方式と同じ経路を通す。フォーカスや入力中の
            // 判定、デバウンス、最小化中の扱いが方式で変わらないように。
            // 外したあとに届いた WM_HOTKEY は捨てる。
            //
            // 受け取ったことは info で残す（#207 の切り分け用）。この方式では
            // フックを外してあるので、ここでファイルへ書いても他のアプリの
            // 入力は待たされない
            ListenerMessage::Hotkey(id) => match registered.chord_for(id) {
                Some(chord) => {
                    info!("WM_HOTKEY を受け取った（id={}）", id);
                    handle_key_down(state, chord, PressSource::SystemHotkey);
                }
                None => info!("登録を外した番号の WM_HOTKEY を捨てた（id={}）", id),
            },
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard_hook::Modifiers;

    fn chord(vk: u32) -> KeyChord {
        KeyChord {
            modifiers: Modifiers::empty(),
            vk,
        }
    }

    // F5〜F8
    const F5: u32 = 0x74;
    const F6: u32 = 0x75;
    const F7: u32 = 0x76;

    #[test]
    fn method_from_setting_defaults_to_the_hook() {
        // 既定（偽）はキーを奪わないフックの方式
        assert_eq!(HotkeyMethod::default(), HotkeyMethod::Hook);
        assert_eq!(HotkeyMethod::from_setting(false), HotkeyMethod::Hook);
        assert_eq!(
            HotkeyMethod::from_setting(true),
            HotkeyMethod::RegisterHotKey
        );
    }

    #[test]
    fn plan_sync_hook_installs_the_hook_once() {
        assert_eq!(
            plan_sync(HotkeyMethod::Hook, false, &[], &[]),
            SyncPlan {
                install_hook: true,
                ..SyncPlan::default()
            }
        );
        // 付いていれば何もしない（2 秒ごとの再適用で付け直さない）
        assert_eq!(
            plan_sync(HotkeyMethod::Hook, true, &[], &[]),
            SyncPlan::default()
        );
    }

    #[test]
    fn plan_sync_switching_to_the_hook_releases_every_registered_key() {
        // RegisterHotKey の登録を残すと、フックの方式でもキーを奪い続ける
        let plan = plan_sync(HotkeyMethod::Hook, false, &[chord(F5), chord(F6)], &[]);

        assert!(plan.install_hook);
        assert_eq!(plan.unregister, vec![chord(F5), chord(F6)]);
        assert!(plan.register.is_empty());
    }

    #[test]
    fn plan_sync_switching_to_register_hotkey_removes_the_hook() {
        // フックを残すと、1 回の押下がフックと WM_HOTKEY の両方から届いて 2 回動く
        let plan = plan_sync(HotkeyMethod::RegisterHotKey, true, &[], &[chord(F5)]);

        assert!(plan.remove_hook);
        assert!(!plan.install_hook);
        assert_eq!(plan.register, vec![chord(F5)]);
    }

    #[test]
    fn plan_sync_register_hotkey_touches_only_the_difference() {
        // 2 秒ごとの再適用で全て外して登録し直すと、その瞬間の押下を取りこぼす
        let plan = plan_sync(
            HotkeyMethod::RegisterHotKey,
            false,
            &[chord(F5), chord(F6)],
            &[chord(F6), chord(F7)],
        );

        assert_eq!(
            plan,
            SyncPlan {
                install_hook: false,
                remove_hook: false,
                unregister: vec![chord(F5)],
                register: vec![chord(F7)],
            }
        );
    }

    #[test]
    fn plan_sync_register_hotkey_without_changes_does_nothing() {
        assert_eq!(
            plan_sync(
                HotkeyMethod::RegisterHotKey,
                false,
                &[chord(F5)],
                &[chord(F5)]
            ),
            SyncPlan::default()
        );
    }

    #[test]
    fn plan_sync_modifiers_make_a_different_chord() {
        // F5 と Ctrl+F5 は別の登録。片方へ変えたら外して登録し直す
        let ctrl_f5 = KeyChord {
            modifiers: Modifiers::CONTROL,
            vk: F5,
        };
        let plan = plan_sync(
            HotkeyMethod::RegisterHotKey,
            false,
            &[chord(F5)],
            &[ctrl_f5],
        );

        assert_eq!(plan.unregister, vec![chord(F5)]);
        assert_eq!(plan.register, vec![ctrl_f5]);
    }

    #[test]
    fn listener_stops_after_shutdown_request() {
        // 終了要求で止まること。止まらないと join が返らず、アプリが終了できなくなる
        let state = Arc::new(Mutex::new(ListenerState::default()));

        // フックを登録できない環境でも、スレッドが終わることは確かめられる
        let (mut listener, _ready) = Listener::spawn(Arc::clone(&state));

        // 実時間の長さでは判定しない（負荷の高い環境で落ちるため）。止め終えた
        // ことを別スレッドから知らせてもらい、返らなければ時間切れにする。
        // 上限は遅い CI でも収まる 10 秒
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            listener.stop();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("終了要求から 10 秒以内にリスナースレッドが終わること");
        // 何も登録していないので押下は記録されない
        assert!(state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .is_empty());
    }

    #[test]
    fn listener_answers_sync_requests() {
        // 方式の切り替えの要求に返事が来ること。返事が来ないと UI スレッドが
        // SYNC_TIMEOUT だけ待たされたうえ、使えないものとして扱われる
        let state = Arc::new(Mutex::new(ListenerState::default()));
        let (mut listener, _ready) = Listener::spawn(state);

        let outcome = listener
            .sync(HotkeyMethod::RegisterHotKey, Vec::new())
            .expect("リスナーが返事をすること");
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.hook_error, None);

        listener.stop();
    }
}
