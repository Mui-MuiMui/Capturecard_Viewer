use crate::repaint::RepaintWaker;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use log::{debug, error, info, trace, warn};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// リスナースレッドがホットキーのイベントを待つ時間。
///
/// タイムアウトするたびに終了要求を確認するため、終了を要求してから
/// スレッドが実際に止まるまで最大でこの時間かかる。待つのはウィンドウを
/// 閉じたあとなので、画面上は見えない。
const LISTENER_RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// 同じアクションの連続実行を無視する時間。
/// キーリピートで何枚も撮れてしまうのを防ぐ。
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(200);

/// グローバルホットキーで実行できるアクション。
///
/// **順序が設定ファイルのキーの並び順と、設定画面の一覧の並び順になる。**
/// `BTreeMap` のキーとして使うため `Ord` を導出しており、その順序は
/// ここでの宣言順で決まる。並べ替えると既存の設定ファイルの見た目が変わる。
///
/// 追加するときは `ALL` と `as_str` / `from_key` / `label` の 4 か所を足す。
/// `as_str` は設定ファイルに書かれる文字列なので、一度出した名前は変えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HotkeyAction {
    /// スクリーンショットを撮る
    Screenshot,
    /// フルスクリーン表示を切り替える
    ToggleFullscreen,
    /// 最前面表示を切り替える
    ToggleAlwaysOnTop,
    /// デバイスを開き直す
    ReconnectDevices,
    /// 音量を上げる
    VolumeUp,
    /// 音量を下げる
    VolumeDown,
    /// ミュートを切り替える
    ToggleMute,
}

impl HotkeyAction {
    /// 設定画面と一覧の表示順。宣言順（`Ord`）と同じにしておく。
    pub const ALL: [HotkeyAction; 7] = [
        HotkeyAction::Screenshot,
        HotkeyAction::ToggleFullscreen,
        HotkeyAction::ToggleAlwaysOnTop,
        HotkeyAction::ReconnectDevices,
        HotkeyAction::VolumeUp,
        HotkeyAction::VolumeDown,
        HotkeyAction::ToggleMute,
    ];

    /// 設定ファイルに書かれるキー名。**変えると既存の設定を見失う。**
    pub fn as_str(self) -> &'static str {
        match self {
            HotkeyAction::Screenshot => "screenshot",
            HotkeyAction::ToggleFullscreen => "toggle_fullscreen",
            HotkeyAction::ToggleAlwaysOnTop => "toggle_always_on_top",
            HotkeyAction::ReconnectDevices => "reconnect_devices",
            HotkeyAction::VolumeUp => "volume_up",
            HotkeyAction::VolumeDown => "volume_down",
            HotkeyAction::ToggleMute => "toggle_mute",
        }
    }

    /// 設定ファイルのキー名からアクションを引く。知らない名前は `None`。
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == key)
    }

    /// 設定画面に出す日本語名。
    pub fn label(self) -> &'static str {
        match self {
            HotkeyAction::Screenshot => "スクリーンショット",
            HotkeyAction::ToggleFullscreen => "フルスクリーン切替",
            HotkeyAction::ToggleAlwaysOnTop => "最前面表示の切替",
            HotkeyAction::ReconnectDevices => "デバイス再接続",
            HotkeyAction::VolumeUp => "音量を上げる",
            HotkeyAction::VolumeDown => "音量を下げる",
            HotkeyAction::ToggleMute => "ミュート切替",
        }
    }

    /// 最小化している間も、その場で実行してよいか。
    ///
    /// **分かれ目は「画面が要るか」。** 音量・ミュート・再接続は出力
    /// コールバックやデバイスワーカーに届けば効くので、最小化中でも
    /// 意味がある。フルスクリーンや最前面表示、スクリーンショットは
    /// 見えていないウィンドウに対して行っても意味がないうえ、UI スレッド
    /// でしか触れない状態を書き換えるため、復帰するまで保留する（#133）。
    ///
    /// 真を返すものの実行経路は `app::hotkeys::background_hotkey_runner`。
    pub fn runs_while_minimized(self) -> bool {
        match self {
            HotkeyAction::ReconnectDevices
            | HotkeyAction::VolumeUp
            | HotkeyAction::VolumeDown
            | HotkeyAction::ToggleMute => true,
            HotkeyAction::Screenshot
            | HotkeyAction::ToggleFullscreen
            | HotkeyAction::ToggleAlwaysOnTop => false,
        }
    }
}

/// 保留していた押下を、復帰したときに何回実行するかへ畳む。
///
/// 最小化している間 `update()` は呼ばれないので、押下は復帰するまで溜まる。
/// 溜まった数をそのまま実行すると、フルスクリーンを 2 回押して戻したはずが
/// 復帰後にフルスクリーンになる、といった食い違いが出る。
///
/// **`match` に `_` を置かないこと。** アクションを増やしたときに、
/// 溜まった押下をどう畳むかをここで必ず決めさせるため。
fn folded_repeats(action: HotkeyAction, presses: u32) -> u32 {
    match action {
        // トグルは偶数回なら元の状態へ戻る。押した回数ぶん切り替えても
        // 結果は同じなので、奇数回のときだけ 1 回実行する
        HotkeyAction::ToggleFullscreen
        | HotkeyAction::ToggleAlwaysOnTop
        | HotkeyAction::ToggleMute => presses % 2,
        // 復帰してから撮るので、何回押されていても同じ 1 枚にしかならない
        HotkeyAction::Screenshot => presses.min(1),
        // 開き直しは何回要求しても結果が同じ
        HotkeyAction::ReconnectDevices => presses.min(1),
        // 増減は押した回数ぶん効かせる。畳むと「10 段上げたのに 1 段」になる
        HotkeyAction::VolumeUp | HotkeyAction::VolumeDown => presses,
    }
}

// 設定では BTreeMap<HotkeyAction, String> のキーとして使う。TOML のキーは
// 文字列でなければならないため、derive ではなく文字列として書き出す。
// derive の単位バリアントはシリアライザによってキーとして受け付けられない
// ことがあり、そこに寄りかかると TOML 側の都合で保存できなくなる。
//
// 読むほうは `Deserialize` を実装していない。知らないアクション名が書かれて
// いても設定ファイル全体を失わないよう、`settings::migrate_hotkeys` が
// 文字列のまま受けて `from_key` で振り分ける。
impl Serialize for HotkeyAction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// アクションに割り当てたキーを登録できなかった理由。
///
/// `hotkey` を一緒に持つのは、同じアクションでもキーが変われば別の失敗として
/// 扱うため。設定画面へそのまま出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyError {
    /// 登録しようとしたホットキー文字列
    pub hotkey: String,
    /// 画面に出す理由
    pub message: String,
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
    fn run(&self, action: HotkeyAction) {
        if let Some(run) = &self.run {
            run(action);
        }
    }
}

/// 押下をどこで実行するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressRouting {
    /// UI スレッドが `take_pressed` で取りに来るまで保留する
    Deferred,
    /// 最小化中なので、UI スレッドを介さずその場で実行する
    Background,
    /// デバウンス期間内なので捨てる
    Debounced,
}

/// リスナースレッドと共有する状態。
///
/// **登録中の ID と押下の記録を 1 つのロックにまとめてある。** 別々に持つと、
/// リスナーが ID を照合してから押下を記録するまでの隙に解除処理が終わり、
/// 解除したはずのキーで 1 回だけ実行されることがある。
#[derive(Default)]
struct ListenerState {
    /// 登録中のホットキー ID → アクション。
    ///
    /// 未登録を空のマップで表す。global-hotkey の ID は修飾キーとキー名から
    /// 作るハッシュなので 0 も正規の値になりうる。番兵の数値で未登録を表すと、
    /// たまたまその値になったホットキーだけが効かなくなる。
    registered: HashMap<u32, HotkeyAction>,
    /// リスナーが検出した押下の回数。UI スレッドが毎フレーム取り出して空にする。
    ///
    /// **集合ではなく回数で持つ。** 最小化している間は `update()` が呼ばれず
    /// 押下が溜まるため、何回押されたかが分からないと復帰したときに
    /// 畳めない（`folded_repeats`）。`BTreeMap` にしてあるので、取り出す
    /// 順序はアクションの宣言順で安定する。
    pressed: BTreeMap<HotkeyAction, u32>,
    /// アクションごとの、最後に押下として受け付けた時刻。デバウンスの基準。
    ///
    /// **UI スレッド側ではなくここで見る。** 最小化中は実行が UI スレッドを
    /// 通らないため、実行の時点で計ると押しっぱなしのキーリピートを
    /// 捨てられない。
    last_press: HashMap<HotkeyAction, Instant>,
    /// ウィンドウが最小化されているか。UI スレッドが毎フレーム書き込む。
    ///
    /// 最小化中は `update()` が呼ばれないので、ここが真のまま止まる。
    /// それが狙いで、リスナーは真の間だけ `background` へ回す。
    minimized: bool,
    /// 最小化中のアクションの実行先。
    background: BackgroundHotkeyRunner,
    /// 押下を記録したあとに UI スレッドを起こす窓口。
    ///
    /// **押下は `update()` が `take_pressed` で取りに来るまで実行されない。**
    /// 映像が届いていない間の `update()` は 250ms 間隔まで落ちるため、
    /// 起こさないとホットキーの反応がそのぶん遅れる。
    ///
    /// 既定の `RepaintWaker` は何もしないので、渡さなくても動作は変わらない
    /// （反応が遅くなるだけ）。
    waker: RepaintWaker,
}

impl ListenerState {
    /// 受け付けた押下を記録し、どこで実行するかを返す。
    ///
    /// デバウンスの判定もここで行う。抑止した場合に `last_press` を
    /// 更新しないのは、押しっぱなしのキーリピートで抑止が延々と続き、
    /// いつまでも実行できない状態にしないため（`decide_trigger`）。
    fn record_press(&mut self, action: HotkeyAction, now: Instant) -> PressRouting {
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

/// グローバルホットキーの登録と押下の検出。
///
/// **UI スレッドだけが触るので `Mutex` で包まない。** リスナースレッドと
/// 共有するのは内部の `Arc<Mutex<ListenerState>>` だけで、そこには
/// 登録中の ID と押下の記録しか入っていない。
pub struct HotkeyManager {
    manager: Option<GlobalHotKeyManager>,
    /// 登録に成功しているアクション → (ホットキー文字列, 登録した HotKey)
    registered: BTreeMap<HotkeyAction, (String, HotKey)>,
    /// 登録できなかったアクション → 理由
    errors: BTreeMap<HotkeyAction, HotkeyError>,
    state: Arc<Mutex<ListenerState>>,
    /// リスナースレッドへの終了要求
    listener_shutdown: Arc<AtomicBool>,
    /// リスナースレッドのハンドル。`Drop` で join するために持つ
    listener: Option<JoinHandle<()>>,
    /// ホットキー入力ダイアログのために一時解除しているか。
    ///
    /// 一時停止中は `apply` を呼んでも何もしない。2 秒ごとの再適用
    /// （`apply_settings`）が動き続けていても、一時停止中に登録し直されて
    /// しまわないようにするため。
    paused: bool,
}

// ホットキー文字列の解析。`HotkeyManager` の状態に依存しないためフリー関数に
// してある（ユニットテストから直接呼べるようにするため）。

/// `"F5"` や `"Ctrl+Shift+A"` のような文字列を `HotKey` に変換する。
///
/// 修飾キーだけの指定（`"Ctrl+Shift"` など）と、通常キーを 2 つ以上含む指定
/// （`"Ctrl+A+B"` など）は登録できないため、エラーにする。
fn parse_hotkey(hotkey_str: &str) -> Result<HotKey, String> {
    let parts: Vec<&str> = hotkey_str.split('+').collect();
    let mut modifiers = Modifiers::empty();
    let mut key_code = None;

    for part in parts {
        let part = part.trim().to_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "alt" => modifiers |= Modifiers::ALT,
            "shift" => modifiers |= Modifiers::SHIFT,
            "win" | "windows" | "super" => modifiers |= Modifiers::SUPER,
            key => {
                // HotKey が持てる通常キーは 1 つだけ。黙って上書きすると
                // "Ctrl+A+B" が "Ctrl+B" として登録され、設定した覚えのない
                // キーが効いてしまうため、2 つ目を見つけた時点で弾く
                if key_code.is_some() {
                    return Err("通常キーを 2 つ以上は指定できません".to_string());
                }
                key_code = Some(parse_key_code(key)?);
            }
        }
    }

    let code = key_code.ok_or_else(|| "通常キーが指定されていません".to_string())?;
    Ok(HotKey::new(Some(modifiers), code))
}

/// 単一のキー名を `Code` に変換する。大文字小文字と前後の空白は無視する。
fn parse_key_code(key: &str) -> Result<Code, String> {
    let normalized = key.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "f1" => Ok(Code::F1),
        "f2" => Ok(Code::F2),
        "f3" => Ok(Code::F3),
        "f4" => Ok(Code::F4),
        "f5" => Ok(Code::F5),
        "f6" => Ok(Code::F6),
        "f7" => Ok(Code::F7),
        "f8" => Ok(Code::F8),
        "f9" => Ok(Code::F9),
        "f10" => Ok(Code::F10),
        "f11" => Ok(Code::F11),
        "f12" => Ok(Code::F12),
        "a" => Ok(Code::KeyA),
        "b" => Ok(Code::KeyB),
        "c" => Ok(Code::KeyC),
        "d" => Ok(Code::KeyD),
        "e" => Ok(Code::KeyE),
        "f" => Ok(Code::KeyF),
        "g" => Ok(Code::KeyG),
        "h" => Ok(Code::KeyH),
        "i" => Ok(Code::KeyI),
        "j" => Ok(Code::KeyJ),
        "k" => Ok(Code::KeyK),
        "l" => Ok(Code::KeyL),
        "m" => Ok(Code::KeyM),
        "n" => Ok(Code::KeyN),
        "o" => Ok(Code::KeyO),
        "p" => Ok(Code::KeyP),
        "q" => Ok(Code::KeyQ),
        "r" => Ok(Code::KeyR),
        "s" => Ok(Code::KeyS),
        "t" => Ok(Code::KeyT),
        "u" => Ok(Code::KeyU),
        "v" => Ok(Code::KeyV),
        "w" => Ok(Code::KeyW),
        "x" => Ok(Code::KeyX),
        "y" => Ok(Code::KeyY),
        "z" => Ok(Code::KeyZ),
        "0" => Ok(Code::Digit0),
        "1" => Ok(Code::Digit1),
        "2" => Ok(Code::Digit2),
        "3" => Ok(Code::Digit3),
        "4" => Ok(Code::Digit4),
        "5" => Ok(Code::Digit5),
        "6" => Ok(Code::Digit6),
        "7" => Ok(Code::Digit7),
        "8" => Ok(Code::Digit8),
        "9" => Ok(Code::Digit9),
        "space" => Ok(Code::Space),
        "enter" => Ok(Code::Enter),
        "escape" => Ok(Code::Escape),
        _ => Err(format!("未対応のキー: {}", key)),
    }
}

/// 受け取ったイベントを、どのアクションの押下として扱うか。
///
/// リスナースレッドはアプリ全体で 1 本だけ動いており、まだ何も登録していない
/// 間もイベントチャネルを待っている。判定に必要なものを引数で受け取る
/// 純粋関数にしてあるのは、実機のキー入力なしでテストするため。
///
/// - 登録していない ID なら無視する。ホットキーを切り替えた直後は、解除した
///   古いキーのイベントがチャネルに残っていることがある
/// - 解放（`Released`）は無視する。押下だけを 1 回として数える
fn accepted_action(
    registered: &HashMap<u32, HotkeyAction>,
    event_id: u32,
    state: HotKeyState,
) -> Option<HotkeyAction> {
    if state != HotKeyState::Pressed {
        return None;
    }
    registered.get(&event_id).copied()
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

/// ホットキーのイベントを待つスレッドを 1 本起動する。
///
/// `GlobalHotKeyEvent::receiver()` が返すのはプロセスに 1 つしかないチャネルなので、
/// リスナーもアプリ全体で 1 本だけにする。`recv_timeout` でブロックして待ち、
/// タイムアウトしたときにだけ終了要求を確認する。
fn spawn_listener(state: Arc<Mutex<ListenerState>>, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        debug!("ホットキーのリスナースレッドを開始した");
        let channel = GlobalHotKeyEvent::receiver();

        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }

            match channel.recv_timeout(LISTENER_RECV_TIMEOUT) {
                Ok(event) => {
                    // 押下・解放のたびに流れるので trace に落とす
                    trace!(
                        "ホットキーのイベントを受信した: ID={}、State={:?}",
                        event.id(),
                        event.state()
                    );

                    // **ID の照合と押下の記録を同じロックの中で行う。**
                    // ロックを手放してから記録すると、その隙に解除処理が
                    // 「ID を消す → 押下を落とす」を終えてしまい、
                    // クリアしたはずのキーで 1 回だけ実行されることがある。
                    // ログはロックを手放してから出す（trace ではファイルへの
                    // 書き出しが入るため、その間ロックを握らない）
                    // 押下を記録したときに UI スレッドを起こすための複製と、
                    // 最小化中にその場で実行するための複製。
                    // **どちらもロックを手放してから呼ぶ。** 握ったまま呼ぶと、
                    // 相手を待つ間この共有状態も止まる
                    let mut wake = None;
                    let mut background = None;
                    let outcome = match state.lock() {
                        Ok(mut state) => {
                            let action =
                                accepted_action(&state.registered, event.id(), event.state());
                            let routing = action.map(|action| {
                                let routing = state.record_press(action, Instant::now());
                                match routing {
                                    PressRouting::Deferred => wake = Some(state.waker.clone()),
                                    PressRouting::Background => {
                                        background = Some((state.background.clone(), action))
                                    }
                                    PressRouting::Debounced => {}
                                }
                                (action, routing)
                            });
                            Some(routing)
                        }
                        Err(_) => {
                            // release ビルドは panic = "abort" なので毒されない
                            warn!("ホットキーの共有状態のロックを取得できないのでイベントを捨てる");
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

                    match outcome {
                        Some(Some((action, PressRouting::Deferred))) => {
                            trace!("{} の押下を記録した", action.label())
                        }
                        Some(Some((action, PressRouting::Background))) => {
                            trace!("{} を最小化中のまま実行した", action.label())
                        }
                        Some(Some((action, PressRouting::Debounced))) => trace!(
                            "デバウンスにより {} の押下を捨てた（{}ms 以内）",
                            action.label(),
                            HOTKEY_DEBOUNCE.as_millis()
                        ),
                        Some(None) => trace!("対象外のイベントなので無視する"),
                        None => {}
                    }
                }
                Err(e) if e.is_disconnected() => {
                    // 送信側は global-hotkey の static なので通常は起きない。
                    // 切断された状態で recv_timeout を呼ぶと待たずに返り続けるため、
                    // 全力で回り続けないようここで抜ける
                    warn!("ホットキーのイベントチャネルが切断されたのでリスナーを終了する");
                    break;
                }
                Err(_) => {
                    // タイムアウト。ループの先頭で終了要求を確認する
                }
            }
        }

        debug!("ホットキーのリスナースレッドを終了した");
    })
}

impl HotkeyManager {
    /// ホットキーのリスナースレッドを起動して `HotkeyManager` を作る。
    ///
    /// この時点ではまだ何も登録していないので、リスナーは受け取ったイベントを
    /// すべて捨てる。登録は `apply` が行う。
    /// スレッドを止めるのは `Drop` だけなので、**アプリ全体で 1 つだけ作ること。**
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(ListenerState::default()));
        let listener_shutdown = Arc::new(AtomicBool::new(false));

        // リスナーはここで 1 本だけ起動し、登録のたびには作り直さない。
        // イベントチャネルはプロセスに 1 つしかないので、複数のスレッドで
        // 待つとどちらがイベントを取るか決まらない
        let listener = spawn_listener(Arc::clone(&state), Arc::clone(&listener_shutdown));

        Self {
            manager: None,
            registered: BTreeMap::new(),
            errors: BTreeMap::new(),
            state,
            listener_shutdown,
            listener: Some(listener),
            paused: false,
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

    /// ウィンドウが最小化されているかを伝える。**毎フレーム呼ぶ。**
    ///
    /// 最小化すると `update()` が呼ばれなくなるので、最後に書き込んだ値が
    /// そのまま残る。リスナーはその値を見て、画面の要らないアクションだけを
    /// `BackgroundHotkeyRunner` へ回す（#133）。
    pub fn set_minimized(&mut self, minimized: bool) {
        match self.state.lock() {
            Ok(mut state) => state.minimized = minimized,
            // 最小化中のアクションが復帰まで保留されるだけで、検出は続く
            Err(_) => warn!("ホットキーの共有状態のロックを取得できないので最小化を伝えられない"),
        }
    }

    /// 設定のホットキー割り当てを実際の登録へ反映する。
    ///
    /// **差分だけを処理する。** 2 秒ごとの再適用で呼ばれるため、無条件に
    /// 登録し直すとその瞬間のキー入力を取りこぼす。
    ///
    /// 登録できなかったアクションは `registered` に入らないので、次に呼ばれた
    /// ときに再試行する。他のアプリがキーを離せば、操作しなくても効くようになる。
    /// ログは理由が変わったときだけ出す（同じ失敗が 2 秒ごとに積もらないように）。
    pub fn apply(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        // 一時停止中は何もしない。ホットキー入力ダイアログを開いている間に
        // 2 秒ごとの再適用が割り込むと、解除したはずのキーが登録し直されてしまう
        if self.paused {
            return;
        }

        // 解除するのは、割り当てが消えたアクションとキーが変わったアクション
        let stale: Vec<HotkeyAction> = self
            .registered
            .iter()
            .filter(|(action, (hotkey, _))| desired.get(action) != Some(hotkey))
            .map(|(action, _)| *action)
            .collect();
        for action in stale {
            self.unregister(action);
        }

        // 失敗の記録も、対象のキーが変わったら捨てる。残すと設定画面に
        // 解消済みの理由が出続ける
        self.errors
            .retain(|action, error| desired.get(action) == Some(&error.hotkey));

        for (action, hotkey) in desired {
            if self.registered.contains_key(action) {
                continue;
            }
            self.register(*action, hotkey);
        }
    }

    /// 保留している押下を取り出す。
    ///
    /// 毎フレーム UI スレッドから呼ばれる。返す順序はアクションの宣言順で、
    /// 同じフレームに複数届いても並び順は変わらない。
    ///
    /// **デバウンスはリスナー側で済んでいる**（`ListenerState::record_press`）。
    /// ここで行うのは、最小化している間に溜まった押下を何回ぶん実行するかの
    /// 判断だけで、判断そのものは `folded_repeats` が持つ。最小化していない
    /// 間はアクションごとに高々 1 回しか溜まらないので、畳んでも結果は変わらない。
    pub fn take_pressed(&mut self) -> Vec<HotkeyAction> {
        let pressed: BTreeMap<HotkeyAction, u32> = match self.state.lock() {
            Ok(mut state) => std::mem::take(&mut state.pressed),
            Err(_) => {
                // ここが失敗するのはロックが毒されたときだけで、毎フレーム呼ばれる。
                // release ビルドは panic = "abort" なので毒されること自体が起きない
                warn!("ホットキーの押下確認で共有状態のロックを取得できない");
                return Vec::new();
            }
        };

        let mut fired = Vec::new();
        for (action, presses) in pressed {
            let repeats = folded_repeats(action, presses);
            if repeats < presses {
                debug!(
                    "{} の押下 {} 回を {} 回へ畳んだ",
                    action.label(),
                    presses,
                    repeats
                );
            }
            for _ in 0..repeats {
                debug!("{} をホットキーから実行する", action.label());
                fired.push(action);
            }
        }
        fired
    }

    /// 登録できなかったアクションと、その理由。設定画面に出す。
    ///
    /// 直っていない間は毎回の `apply` で試し直しているので、ここに残っている
    /// のは「いまも登録できていないもの」だけ。
    pub fn errors(&self) -> &BTreeMap<HotkeyAction, HotkeyError> {
        &self.errors
    }

    /// 登録中のホットキーをすべて一時解除する。ホットキー入力ダイアログを開くときに使う。
    ///
    /// **押しても効かなくなるのはグローバルホットキーだけ。** リスナースレッドは
    /// 止めない（止めると再開が重くなるうえ、アプリ全体で 1 本という前提が崩れる）。
    ///
    /// 既に一時停止中なら何もしない。二重に呼んでも安全にしておくことで、
    /// 呼び出し側でダイアログの開閉検出が多少ずれても壊れない。
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;

        let actions: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in actions {
            self.unregister(action);
        }
        info!("ホットキー入力ダイアログのためグローバルホットキーを一時解除した");
    }

    /// 一時停止を終え、`desired` の内容で登録し直す。
    ///
    /// 一時停止していなければ何もしない（`pause` を呼んでいないのに解除中の
    /// キーが無いのに登録し直そうとする、という状況を防ぐ）。
    pub fn resume(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        if !self.paused {
            return;
        }
        self.paused = false;
        info!("グローバルホットキーの一時解除を終える");
        self.apply(desired);
    }

    /// 候補のホットキーを実際に登録できるか試す。
    ///
    /// ホットキー入力ダイアログでキーが確定したときに使う。**`pause` で
    /// 自分自身の登録をすべて解除したあとに呼ぶことを想定している。** そうして
    /// おけば、ここでの試し登録が自分の他のアクションと衝突することはなく、
    /// 純粋に「他のアプリと競合していないか」だけを確かめられる。
    ///
    /// 登録に成功したら直ちに解除する。ここでの登録は `registered` へ記録しない。
    /// 実際に使い続けるための登録は、この呼び出しのあとに行う `resume` が行う。
    pub fn try_register(&mut self, hotkey_str: &str) -> Result<(), String> {
        let hotkey = parse_hotkey(hotkey_str)?;

        if self.manager.is_none() {
            debug!("ホットキーマネージャーを作成する");
            match GlobalHotKeyManager::new() {
                Ok(manager) => self.manager = Some(manager),
                Err(e) => {
                    return Err(format!("ホットキーの仕組みを初期化できません: {}", e));
                }
            }
        }

        let Some(manager) = &self.manager else {
            // 直前に作っているので通常は来ない
            return Err("ホットキーの仕組みを初期化できません".to_string());
        };

        match manager.register(hotkey) {
            Ok(()) => match manager.unregister(hotkey) {
                Ok(()) => Ok(()),
                // 解除に失敗すると、OS 側にはまだこの試し登録が残ったままになる。
                // ここを成功扱いにすると、呼び出し側が候補を受理してダイアログを
                // 閉じ、resume でも同じキーを登録しようとして「二重登録」で
                // 失敗する。**解除できるまで候補を受理させない。**
                Err(e) => Err(format!(
                    "試し登録したホットキーを解除できません。もう一度お試しください: {e}"
                )),
            },
            Err(e) => Err(format!(
                "登録できません（他のアプリと競合している可能性があります）: {e}"
            )),
        }
    }

    /// 1 つのアクションにホットキーを登録する。失敗は `errors` に記録する。
    fn register(&mut self, action: HotkeyAction, hotkey_str: &str) {
        let hotkey = match parse_hotkey(hotkey_str) {
            Ok(hotkey) => hotkey,
            Err(e) => {
                self.record_error(action, hotkey_str, e);
                return;
            }
        };

        // 同じキーを 2 つのアクションへ割り当てると、どちらのイベントなのか
        // ID から区別できない。設定画面でも警告するが、設定ファイルを手で
        // 書き換えられる前提でここでも弾く。先に登録したほう（宣言順で先の
        // アクション）を残す
        if let Some(other) = self.action_for_id(hotkey.id()) {
            self.record_error(
                action,
                hotkey_str,
                format!("同じキーが「{}」に割り当てられています", other.label()),
            );
            return;
        }

        if self.manager.is_none() {
            debug!("ホットキーマネージャーを作成する");
            match GlobalHotKeyManager::new() {
                Ok(manager) => self.manager = Some(manager),
                Err(e) => {
                    self.record_error(
                        action,
                        hotkey_str,
                        format!("ホットキーの仕組みを初期化できません: {}", e),
                    );
                    return;
                }
            }
        }

        let Some(manager) = &self.manager else {
            // 直前に作っているので通常は来ない
            return;
        };

        // F11/F12 は他のアプリと取り合いになりやすい。登録自体は成功しても
        // 効かないことがあるので、不具合報告から切り分けられるよう残す
        let lowered = hotkey_str.to_lowercase();
        if lowered == "f11" || lowered == "f12" {
            info!(
                "{} をグローバルホットキーとして登録する。他のアプリが使っていないか確認すること",
                hotkey_str
            );
        }

        if let Err(e) = manager.register(hotkey) {
            self.record_error(
                action,
                hotkey_str,
                format!("登録できません（他のアプリと競合している可能性があります）: {e}"),
            );
            return;
        }

        // リスナーが照合に使う ID を足す
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.insert(hotkey.id(), action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        self.registered
            .insert(action, (hotkey_str.to_string(), hotkey));
        self.errors.remove(&action);
        info!(
            "{} に {} を割り当てた（ID: {}）",
            action.label(),
            hotkey_str,
            hotkey.id()
        );
    }

    /// 1 つのアクションの登録を解除する。登録していなければ何もしない。
    fn unregister(&mut self, action: HotkeyAction) {
        let Some((hotkey_str, hotkey)) = self.registered.remove(&action) else {
            return;
        };

        // 先に共有している ID を消す。解除が終わるまでの間に届いたイベントを
        // 押下として扱わないため。**同じロックの中で保留中の押下も落とす。**
        // 別々のロックで行うと、クリアした直後のフレームで 1 回だけ実行される
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.remove(&hotkey.id());
                state.pressed.remove(&action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        info!("{} の {} の割り当てを解除する", action.label(), hotkey_str);

        let Some(manager) = &self.manager else {
            return;
        };
        if let Err(e) = manager.unregister(hotkey) {
            // 解除できなくても新しいホットキーの登録は続けられるので、
            // 失敗しても止めない。原因が追えるようログには残す
            warn!("古いホットキーを登録解除できない: {}", e);
        }
    }

    /// その ID を既に使っているアクション。
    fn action_for_id(&self, id: u32) -> Option<HotkeyAction> {
        self.registered
            .iter()
            .find(|(_, (_, hotkey))| hotkey.id() == id)
            .map(|(action, _)| *action)
    }

    /// 失敗を記録する。同じ理由が続く間はログに出さない。
    ///
    /// 登録に失敗したアクションは 2 秒ごとの再適用で試し直すため、毎回
    /// ログへ書くと同じ行が延々と積もる。
    fn record_error(&mut self, action: HotkeyAction, hotkey: &str, message: String) {
        let error = HotkeyError {
            hotkey: hotkey.to_string(),
            message,
        };
        if self.errors.get(&action) != Some(&error) {
            error!(
                "{} に {} を割り当てられない: {}",
                action.label(),
                error.hotkey,
                error.message
            );
        }
        self.errors.insert(action, error);
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

        // 終了要求は recv_timeout のタイムアウトで拾うため、待ち時間は
        // 最大で LISTENER_RECV_TIMEOUT。ウィンドウを閉じたあとの待ちなので
        // 画面上は見えない。切り離すとプロセスが終わるまでスレッドが残る
        if handle.join().is_err() {
            // release ビルドは panic = "abort" なのでここには来ない
            warn!("ホットキーのリスナースレッドがパニックした");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- アクションの名前 ----

    #[test]
    fn hotkey_action_all_contains_every_variant_once() {
        // ALL から漏れると、設定画面に出ないアクションができる
        let mut seen: Vec<&str> = HotkeyAction::ALL.iter().map(|a| a.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), HotkeyAction::ALL.len());
    }

    #[test]
    fn hotkey_action_from_key_roundtrips() {
        for action in HotkeyAction::ALL {
            assert_eq!(HotkeyAction::from_key(action.as_str()), Some(action));
        }
    }

    #[test]
    fn hotkey_action_from_key_unknown_returns_none() {
        // 設定ファイルに知らないアクションが書かれている場合
        assert_eq!(HotkeyAction::from_key("mute"), None);
        assert_eq!(HotkeyAction::from_key(""), None);
        assert_eq!(HotkeyAction::from_key("Screenshot"), None);
    }

    #[test]
    fn hotkey_action_order_is_declaration_order() {
        // BTreeMap の並び順＝設定ファイルのキーの並び順。変わると見た目が変わる
        let mut sorted = HotkeyAction::ALL;
        sorted.sort();
        assert_eq!(sorted, HotkeyAction::ALL);
    }

    // ---- ホットキー文字列の解析 ----

    // HotKey の `mods` / `key` は pub(crate) で外から読めないため、期待する組み合わせで
    // 作った HotKey と比較する。id は修飾キーとキーコードから決まるので等価判定で足りる。
    fn assert_hotkey(actual: &HotKey, expected_mods: Modifiers, expected_code: Code) {
        assert_eq!(*actual, HotKey::new(Some(expected_mods), expected_code));
    }

    #[test]
    fn parse_hotkey_modifier_only_returns_error() {
        // 修飾キーだけでは登録できないため、パース時点で弾く
        assert!(parse_hotkey("Ctrl").is_err());
        assert!(parse_hotkey("Ctrl+Shift").is_err());
        assert!(parse_hotkey("Ctrl+Shift+Alt").is_err());
    }

    #[test]
    fn parse_hotkey_empty_returns_error() {
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn parse_hotkey_unknown_key_returns_error() {
        assert!(parse_hotkey("Ctrl+Nonexistent").is_err());
        assert!(parse_hotkey("F13").is_err());
    }

    #[test]
    fn parse_hotkey_multiple_key_codes_returns_error() {
        // 黙って最後のキーで上書きせず、エラーにする
        assert!(parse_hotkey("Ctrl+A+B").is_err());
        assert!(parse_hotkey("A+B").is_err());
        assert!(parse_hotkey("F5+F6").is_err());
    }

    #[test]
    fn parse_hotkey_single_key_has_no_modifiers() {
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        assert_hotkey(&hotkey, Modifiers::empty(), Code::F5);
    }

    #[test]
    fn parse_hotkey_with_one_modifier_sets_that_modifier() {
        let hotkey = parse_hotkey("Ctrl+S").expect("Ctrl+S は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL, Code::KeyS);
    }

    #[test]
    fn parse_hotkey_with_two_modifiers_sets_both() {
        let hotkey = parse_hotkey("Ctrl+Shift+A").expect("Ctrl+Shift+A は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyA);
    }

    #[test]
    fn parse_hotkey_accepts_modifier_aliases() {
        let control = parse_hotkey("Control+A").expect("Control は Ctrl の別名");
        assert_hotkey(&control, Modifiers::CONTROL, Code::KeyA);

        let win = parse_hotkey("Win+A").expect("Win は Super の別名");
        assert_hotkey(&win, Modifiers::SUPER, Code::KeyA);

        let windows = parse_hotkey("Windows+A").expect("Windows は Super の別名");
        assert_hotkey(&windows, Modifiers::SUPER, Code::KeyA);

        let superkey = parse_hotkey("Super+A").expect("Super はそのまま使える");
        assert_hotkey(&superkey, Modifiers::SUPER, Code::KeyA);
    }

    #[test]
    fn parse_hotkey_is_case_insensitive() {
        let upper = parse_hotkey("CTRL+SHIFT+A").expect("大文字でも解析できる");
        assert_hotkey(&upper, Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyA);

        let lower = parse_hotkey("ctrl+shift+a").expect("小文字でも解析できる");
        assert_hotkey(&lower, Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyA);
    }

    #[test]
    fn parse_hotkey_ignores_spaces_around_parts() {
        let hotkey = parse_hotkey(" Ctrl + S ").expect("前後の空白は無視する");
        assert_hotkey(&hotkey, Modifiers::CONTROL, Code::KeyS);
    }

    #[test]
    fn parse_key_code_letters_are_mapped() {
        assert_eq!(parse_key_code("a"), Ok(Code::KeyA));
        assert_eq!(parse_key_code("m"), Ok(Code::KeyM));
        assert_eq!(parse_key_code("z"), Ok(Code::KeyZ));
    }

    #[test]
    fn parse_key_code_function_keys_are_mapped() {
        assert_eq!(parse_key_code("f1"), Ok(Code::F1));
        assert_eq!(parse_key_code("f9"), Ok(Code::F9));
        assert_eq!(parse_key_code("f10"), Ok(Code::F10));
        assert_eq!(parse_key_code("f12"), Ok(Code::F12));
    }

    #[test]
    fn parse_key_code_digits_are_mapped() {
        assert_eq!(parse_key_code("0"), Ok(Code::Digit0));
        assert_eq!(parse_key_code("5"), Ok(Code::Digit5));
        assert_eq!(parse_key_code("9"), Ok(Code::Digit9));
    }

    #[test]
    fn parse_hotkey_digit_with_modifiers_is_accepted() {
        let hotkey = parse_hotkey("Ctrl+Shift+9").expect("Ctrl+Shift+9 は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, Code::Digit9);
    }

    #[test]
    fn parse_key_code_named_keys_are_mapped() {
        assert_eq!(parse_key_code("space"), Ok(Code::Space));
        assert_eq!(parse_key_code("enter"), Ok(Code::Enter));
        assert_eq!(parse_key_code("escape"), Ok(Code::Escape));
    }

    #[test]
    fn parse_key_code_uppercase_is_accepted() {
        // parse_hotkey は小文字化してから渡すが、直接呼ばれても同じ結果になること
        assert_eq!(parse_key_code("A"), Ok(Code::KeyA));
        assert_eq!(parse_key_code("F5"), Ok(Code::F5));
        assert_eq!(parse_key_code("Space"), Ok(Code::Space));
    }

    #[test]
    fn parse_key_code_unknown_key_returns_error() {
        assert!(parse_key_code("f13").is_err());
        assert!(parse_key_code("").is_err());
        assert!(parse_key_code("ctrl").is_err());
    }

    // ---- リスナースレッドのイベント照合とデバウンス ----

    // ID は modifiers とキー名のハッシュなので、テストでは適当な値で足りる
    const SCREENSHOT_ID: u32 = 1234;
    const FULLSCREEN_ID: u32 = 5678;

    fn registered_ids() -> HashMap<u32, HotkeyAction> {
        HashMap::from([
            (SCREENSHOT_ID, HotkeyAction::Screenshot),
            (FULLSCREEN_ID, HotkeyAction::ToggleFullscreen),
        ])
    }

    #[test]
    fn accepted_action_matching_id_returns_that_action() {
        assert_eq!(
            accepted_action(&registered_ids(), SCREENSHOT_ID, HotKeyState::Pressed),
            Some(HotkeyAction::Screenshot)
        );
        assert_eq!(
            accepted_action(&registered_ids(), FULLSCREEN_ID, HotKeyState::Pressed),
            Some(HotkeyAction::ToggleFullscreen)
        );
    }

    #[test]
    fn accepted_action_released_returns_none() {
        // 押下と解放で 2 回流れる。解放で撮ると 1 回の操作で 2 枚になる
        assert_eq!(
            accepted_action(&registered_ids(), SCREENSHOT_ID, HotKeyState::Released),
            None
        );
    }

    #[test]
    fn accepted_action_other_id_returns_none() {
        // ホットキーを切り替えた直後、解除済みのキーのイベントが残っていることがある
        assert_eq!(
            accepted_action(&registered_ids(), SCREENSHOT_ID + 1, HotKeyState::Pressed),
            None
        );
    }

    #[test]
    fn accepted_action_without_registration_returns_none() {
        // リスナーは登録前から動いている。何も登録していない間は反応しない
        let empty = HashMap::new();
        assert_eq!(
            accepted_action(&empty, SCREENSHOT_ID, HotKeyState::Pressed),
            None
        );
        assert_eq!(accepted_action(&empty, 0, HotKeyState::Pressed), None);
    }

    #[test]
    fn accepted_action_id_zero_is_a_valid_registration() {
        // ID はハッシュなので 0 も正規の値。未登録を 0 で表していると
        // そのホットキーだけが効かなくなる
        let registered = HashMap::from([(0, HotkeyAction::VolumeUp)]);
        assert_eq!(
            accepted_action(&registered, 0, HotKeyState::Pressed),
            Some(HotkeyAction::VolumeUp)
        );
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
        // 終了要求を recv_timeout のタイムアウトで拾えること。拾えないと
        // join が返らず、アプリが終了できなくなる
        let state = Arc::new(Mutex::new(ListenerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_listener(Arc::clone(&state), Arc::clone(&shutdown));

        shutdown.store(true, Ordering::Release);
        let started = Instant::now();
        handle.join().expect("リスナースレッドが正常に終わること");

        // 待ち時間はタイムアウト 1 回ぶんが上限。CI の遅さを見込んで
        // 4 倍を上限にしている
        assert!(
            started.elapsed() < LISTENER_RECV_TIMEOUT * 4,
            "終了までに {:?} かかった",
            started.elapsed()
        );
        // イベントを受け取っていないので押下は記録されない
        assert!(state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .is_empty());
    }

    // ---- 登録の差分処理 ----
    //
    // GlobalHotKeyManager は実際に Windows へ登録するため、CI では
    // 成功しないことがある。ここで確かめるのは「解除と押下の扱い」だけにし、
    // 登録そのものの成否には依存しないテストにしてある。

    fn assignments(pairs: &[(HotkeyAction, &str)]) -> BTreeMap<HotkeyAction, String> {
        pairs
            .iter()
            .map(|(action, key)| (*action, (*key).to_string()))
            .collect()
    }

    #[test]
    fn apply_empty_assignment_registers_nothing() {
        let mut manager = HotkeyManager::new();

        manager.apply(&BTreeMap::new());

        assert!(manager.registered.is_empty());
        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_unparsable_hotkey_records_an_error() {
        // 設定ファイルを手で書き換えた場合。登録へ進まずに理由を残す
        let mut manager = HotkeyManager::new();

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(!manager.registered.contains_key(&HotkeyAction::Screenshot));
        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("理由が残ること");
        assert_eq!(error.hotkey, "Ctrl+Shift");
    }

    #[test]
    fn apply_clearing_an_assignment_drops_the_error() {
        // 直せないキーを入れたあとにクリアしたら、設定画面から理由も消える
        let mut manager = HotkeyManager::new();
        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));
        assert!(manager.errors.contains_key(&HotkeyAction::Screenshot));

        manager.apply(&BTreeMap::new());

        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_changing_the_key_drops_the_old_error() {
        // 別のキーへ変えた時点で、前のキーの理由は意味を失う
        let mut manager = HotkeyManager::new();
        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Alt+Shift")]));

        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("新しいキーの理由が残ること");
        assert_eq!(error.hotkey, "Alt+Shift");
    }

    #[test]
    fn unregister_drops_pending_press() {
        // 解除の直前に届いた押下を残すと、クリアした直後に 1 回だけ実行される。
        // 登録の成否に依存しないよう、共有状態を直接組み立てて確かめる
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state
                .registered
                .insert(hotkey.id(), HotkeyAction::Screenshot);
            state.pressed.insert(HotkeyAction::Screenshot, 1);
        }

        manager.unregister(HotkeyAction::Screenshot);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert!(state.pressed.is_empty(), "保留中の押下が残っている");
        assert!(state.registered.is_empty(), "登録中の ID が残っている");
    }

    #[test]
    fn unregister_keeps_other_actions_pressed() {
        // 1 つのアクションを解除しても、他のアクションの押下は捨てない
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::Screenshot, 1);
            state.pressed.insert(HotkeyAction::VolumeUp, 1);
        }

        manager.unregister(HotkeyAction::Screenshot);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert_eq!(
            state.pressed.keys().copied().collect::<Vec<_>>(),
            vec![HotkeyAction::VolumeUp]
        );
    }

    #[test]
    fn take_pressed_returns_actions_in_declaration_order() {
        let mut manager = HotkeyManager::new();
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::VolumeDown, 1);
            state.pressed.insert(HotkeyAction::Screenshot, 1);
            state.pressed.insert(HotkeyAction::ReconnectDevices, 1);
        }

        let fired = manager.take_pressed();

        assert_eq!(
            fired,
            vec![
                HotkeyAction::Screenshot,
                HotkeyAction::ReconnectDevices,
                HotkeyAction::VolumeDown,
            ]
        );
    }

    #[test]
    fn take_pressed_consumes_the_press() {
        let mut manager = HotkeyManager::new();
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .pressed
            .insert(HotkeyAction::Screenshot, 1);

        assert_eq!(manager.take_pressed(), vec![HotkeyAction::Screenshot]);
        assert!(manager.take_pressed().is_empty());
    }

    #[test]
    fn take_pressed_folds_repeated_presses() {
        // 最小化している間に溜まった押下。トグルは偶数回なら実行しない
        let mut manager = HotkeyManager::new();
        {
            let mut state = manager.state.lock().expect("ロックが毒されていないこと");
            state.pressed.insert(HotkeyAction::ToggleFullscreen, 2);
            state.pressed.insert(HotkeyAction::ToggleAlwaysOnTop, 3);
            state.pressed.insert(HotkeyAction::Screenshot, 4);
        }

        assert_eq!(
            manager.take_pressed(),
            vec![HotkeyAction::Screenshot, HotkeyAction::ToggleAlwaysOnTop,]
        );
    }

    // ---- 一時停止と再開 ----

    #[test]
    fn pause_clears_currently_registered_actions() {
        // 実際の登録成否に依存せず、bookkeeping だけを直接組み立てて確かめる
        let mut manager = HotkeyManager::new();
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        manager
            .registered
            .insert(HotkeyAction::Screenshot, ("F5".to_string(), hotkey));
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .insert(hotkey.id(), HotkeyAction::Screenshot);

        manager.pause();

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
        assert!(manager.paused);
    }

    #[test]
    fn pause_twice_is_a_no_op() {
        let mut manager = HotkeyManager::new();
        manager.pause();
        manager.pause();

        assert!(manager.paused);
    }

    #[test]
    fn apply_while_paused_does_nothing() {
        // 一時停止中に 2 秒ごとの再適用が割り込んでも、登録し直されないこと
        let mut manager = HotkeyManager::new();
        manager.pause();

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(manager.registered.is_empty());
        assert!(
            manager.errors.is_empty(),
            "一時停止中は apply が何もしないこと"
        );
    }

    #[test]
    fn resume_without_pause_does_nothing() {
        // pause を呼んでいない状態で resume しても apply は走らない
        let mut manager = HotkeyManager::new();

        manager.resume(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(manager.errors.is_empty());
    }

    #[test]
    fn resume_after_pause_applies_the_given_assignments() {
        // 解析に失敗するキーを使い、実際の OS 登録に依存せず「apply が走ったこと」
        // だけを確かめる（登録そのものの成否は他のテストと同じ理由で見ない）
        let mut manager = HotkeyManager::new();
        manager.pause();

        manager.resume(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+Shift")]));

        assert!(!manager.paused);
        let error = manager
            .errors
            .get(&HotkeyAction::Screenshot)
            .expect("再開後は apply が走り、失敗が記録されること");
        assert_eq!(error.hotkey, "Ctrl+Shift");
    }

    // ---- 試し登録 ----

    #[test]
    fn try_register_unparsable_hotkey_returns_error_without_creating_manager() {
        let mut manager = HotkeyManager::new();

        let result = manager.try_register("Ctrl+Shift");

        assert!(result.is_err());
        // GlobalHotKeyManager は実際に Windows へ触るため、解析の時点で
        // 弾ける入力では作らないことを確かめる
        assert!(manager.manager.is_none());
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

    // ---- 溜まった押下の畳み方 ----

    #[test]
    fn folded_repeats_toggles_cancel_out_in_pairs() {
        // 最小化中に 2 回押して戻したなら、復帰しても切り替えない
        assert_eq!(folded_repeats(HotkeyAction::ToggleFullscreen, 2), 0);
        assert_eq!(folded_repeats(HotkeyAction::ToggleFullscreen, 3), 1);
        assert_eq!(folded_repeats(HotkeyAction::ToggleAlwaysOnTop, 4), 0);
        assert_eq!(folded_repeats(HotkeyAction::ToggleMute, 1), 1);
    }

    #[test]
    fn folded_repeats_screenshot_is_taken_once() {
        // 復帰してから撮るので、何回押されていても同じ 1 枚にしかならない
        assert_eq!(folded_repeats(HotkeyAction::Screenshot, 5), 1);
        assert_eq!(folded_repeats(HotkeyAction::Screenshot, 0), 0);
    }

    #[test]
    fn folded_repeats_volume_keeps_every_press() {
        // 増減は押した回数ぶん効かせる（最小化中はワーカーが実行するので、
        // ここへ来るのは通常のフレームで溜まった分だけ）
        assert_eq!(folded_repeats(HotkeyAction::VolumeUp, 3), 3);
        assert_eq!(folded_repeats(HotkeyAction::VolumeDown, 1), 1);
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
}
