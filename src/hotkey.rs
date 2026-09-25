use crate::keyboard_hook::{KeyChord, KeyboardHook, KeyboardHookError, Modifiers};
use crate::repaint::RepaintWaker;
use log::{debug, error, info, trace, warn};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
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

/// ホットキーで実行できるアクション。
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

/// ホットキーを解釈できなかった、または登録できなかった理由。
///
/// 解釈（`parse_hotkey`）と登録（`HotkeyManager::register` / `try_register`）を
/// 1 つの enum にまとめてある。どちらも `ErrorSource::Hotkey` として同じ経路で
/// 表示され、呼び出し側は「どの段で失敗したか」で処理を分けないため。
///
/// **表示用の日本語はこの型の `Display` が持つ。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事
/// （`docs/design/error-reporting.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// `"Ctrl+A+B"` のように通常キーを 2 つ以上含む
    MultipleKeys,
    /// `"Ctrl+Shift"` のように修飾キーだけで通常キーが無い
    MissingKey,
    /// 対応表に無いキー名。`key` は指定されたままの文字列
    UnsupportedKey(String),
    /// 同じキーが既に別のアクションへ割り当てられている
    DuplicateAssignment { other: HotkeyAction },
    /// 押下を観測するキーボードフックを使えない
    HookUnavailable(KeyboardHookError),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::MultipleKeys => write!(f, "通常キーを 2 つ以上は指定できません"),
            HotkeyError::MissingKey => write!(f, "通常キーが指定されていません"),
            HotkeyError::UnsupportedKey(key) => write!(f, "未対応のキー: {key}"),
            HotkeyError::DuplicateAssignment { other } => {
                write!(f, "同じキーが「{}」に割り当てられています", other.label())
            }
            HotkeyError::HookUnavailable(source) => {
                write!(f, "ホットキーの仕組みを初期化できません: {source}")
            }
        }
    }
}

impl std::error::Error for HotkeyError {}

/// アクションに割り当てたキーを登録できなかった理由。
///
/// `hotkey` を一緒に持つのは、同じアクションでもキーが変われば別の失敗として
/// 扱うため。設定画面へそのまま出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyAssignmentError {
    /// 登録しようとしたホットキー文字列
    pub hotkey: String,
    /// 画面に出す理由
    pub reason: HotkeyError,
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
struct ListenerState {
    /// 登録中のキーの組み合わせ → アクション。未登録は空のマップで表す。
    registered: HashMap<KeyChord, HotkeyAction>,
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
    /// ウィンドウにキーボードフォーカスがあるか。UI スレッドが毎フレーム書き込む。
    ///
    /// **フックの中で `GetForegroundWindow` を呼んで調べない。** フックの
    /// コールバックでは判定以外のことをしない決まり（`crate::keyboard_hook`）
    /// なので、`minimized` と同じく UI スレッドが知っている値を書いておく。
    focused: bool,
    /// 「フォーカスがあるときだけ反応する」がオンか。設定の反映のたびに書く。
    only_when_focused: bool,
    /// egui がキーボード入力を受けているか（`Context::wants_keyboard_input`）。
    /// UI スレッドが毎フレーム書き込む。
    ///
    /// キーを奪わなくなったので、設定ダイアログのテキスト欄へ打った文字も
    /// ここへ届く（#206）。`focused` と同じく、フックの中で egui に
    /// 問い合わせずに UI スレッドが知っている値を書いておく。
    typing: bool,
    /// 動いていたリスナーが止まった理由。止まっていなければ `None`。
    ///
    /// リスナーはキー入力を待てなくなると終わる（フックも外れる）。ここに
    /// 書いておき、UI スレッドの `apply` が拾って失敗として画面に出す。
    /// 書かないと、効かなくなったのに登録済みのまま何も表示されない。
    listener_failure: Option<KeyboardHookError>,
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
    /// リスナーの起動時に 1 度だけ決まる。使えないときは、割り当てのたびに
    /// この理由で失敗として記録し、設定画面とトーストに出す。
    hook_error: Option<KeyboardHookError>,
    /// 登録に成功しているアクション → (ホットキー文字列, キーの組み合わせ)
    registered: BTreeMap<HotkeyAction, (String, KeyChord)>,
    /// 登録できなかったアクション → 理由
    errors: BTreeMap<HotkeyAction, HotkeyAssignmentError>,
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

/// `"F5"` や `"Ctrl+Shift+A"` のような文字列を `KeyChord` に変換する。
///
/// 修飾キーだけの指定（`"Ctrl+Shift"` など）と、通常キーを 2 つ以上含む指定
/// （`"Ctrl+A+B"` など）は登録できないため、エラーにする。
fn parse_hotkey(hotkey_str: &str) -> Result<KeyChord, HotkeyError> {
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
                // 組み合わせが持てる通常キーは 1 つだけ。黙って上書きすると
                // "Ctrl+A+B" が "Ctrl+B" として登録され、設定した覚えのない
                // キーが効いてしまうため、2 つ目を見つけた時点で弾く
                if key_code.is_some() {
                    return Err(HotkeyError::MultipleKeys);
                }
                key_code = Some(parse_key_code(key)?);
            }
        }
    }

    let vk = key_code.ok_or(HotkeyError::MissingKey)?;
    Ok(KeyChord { modifiers, vk })
}

// Win32 の仮想キーコード。英字と数字は ASCII の大文字・数字と同じ値。
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_F1: u32 = 0x70;

/// 単一のキー名を仮想キーコードに変換する。大文字小文字と前後の空白は無視する。
///
/// 受け付けるキー名は global-hotkey を使っていたころと同じ
/// （F1〜F12、A〜Z、0〜9、Space、Enter、Escape）。
fn parse_key_code(key: &str) -> Result<u32, HotkeyError> {
    let normalized = key.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "f1" => Ok(VK_F1),
        "f2" => Ok(VK_F1 + 1),
        "f3" => Ok(VK_F1 + 2),
        "f4" => Ok(VK_F1 + 3),
        "f5" => Ok(VK_F1 + 4),
        "f6" => Ok(VK_F1 + 5),
        "f7" => Ok(VK_F1 + 6),
        "f8" => Ok(VK_F1 + 7),
        "f9" => Ok(VK_F1 + 8),
        "f10" => Ok(VK_F1 + 9),
        "f11" => Ok(VK_F1 + 10),
        "f12" => Ok(VK_F1 + 11),
        "space" => Ok(VK_SPACE),
        "enter" => Ok(VK_RETURN),
        "escape" => Ok(VK_ESCAPE),
        // 英字 1 文字と数字 1 文字。仮想キーコードは大文字と数字の ASCII と同じ
        single if single.len() == 1 && is_letter_or_digit(single.as_bytes()[0]) => {
            Ok(u32::from(single.as_bytes()[0].to_ascii_uppercase()))
        }
        _ => Err(HotkeyError::UnsupportedKey(key.to_string())),
    }
}

/// 1 文字のキー名として受け付ける文字か（小文字化したあとの英字と数字）。
fn is_letter_or_digit(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
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
fn spawn_listener(
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
        let hook_error = match ready {
            Ok(()) => None,
            Err(e) => {
                error!("ホットキーのキーボードフックを登録できない: {}", e);
                Some(e)
            }
        };

        Self {
            hook_error,
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
                state.minimized = minimized;
                state.focused = focused;
                state.typing = typing;
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

    /// 設定のホットキー割り当てを実際の登録へ反映する。
    ///
    /// **差分だけを処理する。** 2 秒ごとの再適用で呼ばれるため、無条件に
    /// 登録し直すとその瞬間のキー入力を取りこぼす。
    ///
    /// 登録できなかったアクションは `registered` に入らないので、次に呼ばれた
    /// ときに再試行する。ログは理由が変わったときだけ出す（同じ失敗が
    /// 2 秒ごとに積もらないように）。
    pub fn apply(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        // 一時停止中は何もしない。ホットキー入力ダイアログを開いている間に
        // 2 秒ごとの再適用が割り込むと、解除したはずのキーが登録し直されてしまう
        if self.paused {
            return;
        }

        // リスナーが途中で止まっていたら、フックを使えないのと同じ扱いにする。
        // 登録済みのものを外しておけば、下の登録で理由付きの失敗として記録され、
        // トーストと設定画面に出る
        self.take_listener_failure();

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
    pub fn errors(&self) -> &BTreeMap<HotkeyAction, HotkeyAssignmentError> {
        &self.errors
    }

    /// 登録中のホットキーをすべて一時解除する。ホットキー入力ダイアログを開くときに使う。
    ///
    /// 解除しないと、割り当て済みのキーを押して付け直そうとしたときに、
    /// そのアクションまで実行されてしまう。**リスナースレッドとフックは
    /// 止めない**（止めると再開が重くなるうえ、アプリ全体で 1 本という前提が
    /// 崩れる）。照合に使う表を空にするだけ。
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
        info!("ホットキー入力ダイアログのためホットキーを一時解除した");
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
        info!("ホットキーの一時解除を終える");
        self.apply(desired);
    }

    /// 候補のホットキーを登録できるか確かめる。
    ///
    /// ホットキー入力ダイアログでキーが確定したときに使う。見るのは
    /// 「解釈できるか」と「キーボードフックを使えているか」の 2 つだけで、
    /// ここでは何も登録しない。実際に使い続けるための登録は、この呼び出しの
    /// あとに行う `resume` が行う。
    ///
    /// **他のアプリとの競合は起きない。** キーを奪わずに観測するだけなので、
    /// 他のアプリが同じキーを使っていても両方が反応する（#202）。
    pub fn try_register(&self, hotkey_str: &str) -> Result<(), HotkeyError> {
        parse_hotkey(hotkey_str)?;
        match &self.hook_error {
            Some(e) => Err(HotkeyError::HookUnavailable(e.clone())),
            None => Ok(()),
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

        // 同じキーを 2 つのアクションへ割り当てると、どちらの押下なのか
        // 区別できない。設定画面でも警告するが、設定ファイルを手で
        // 書き換えられる前提でここでも弾く。先に登録したほう（宣言順で先の
        // アクション）を残す
        if let Some(other) = self.action_for_chord(hotkey) {
            self.record_error(
                action,
                hotkey_str,
                HotkeyError::DuplicateAssignment { other },
            );
            return;
        }

        // フックが無ければ押下を観測できない。表に載せても効かないので、
        // 理由を残して設定画面とトーストに出す
        if let Some(e) = &self.hook_error {
            let reason = HotkeyError::HookUnavailable(e.clone());
            self.record_error(action, hotkey_str, reason);
            return;
        }

        // リスナーが照合に使う組み合わせを足す
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.insert(hotkey, action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        self.registered
            .insert(action, (hotkey_str.to_string(), hotkey));
        self.errors.remove(&action);
        info!("{} に {} を割り当てた", action.label(), hotkey_str);
    }

    /// 1 つのアクションの登録を解除する。登録していなければ何もしない。
    fn unregister(&mut self, action: HotkeyAction) {
        let Some((hotkey_str, hotkey)) = self.registered.remove(&action) else {
            return;
        };

        // 照合に使う組み合わせを消す。**同じロックの中で保留中の押下も落とす。**
        // 別々のロックで行うと、クリアした直後のフレームで 1 回だけ実行される
        match self.state.lock() {
            Ok(mut state) => {
                state.registered.remove(&hotkey);
                state.pressed.remove(&action);
            }
            Err(_) => warn!("ホットキーの共有状態のロックを取得できない"),
        }

        info!("{} の {} の割り当てを解除した", action.label(), hotkey_str);
    }

    /// リスナーが途中で止まっていたら、理由を `hook_error` へ移し、
    /// 登録済みのものを外す。止まっていなければ何もしない。
    ///
    /// 外したものは続く登録で `HookUnavailable` として記録し直される。
    /// 一度移したら `hook_error` が埋まるので、以降は毎回の登録がそこで失敗する。
    fn take_listener_failure(&mut self) {
        if self.hook_error.is_some() {
            return;
        }
        let failure = match self.state.lock() {
            Ok(mut state) => state.listener_failure.take(),
            Err(_) => {
                warn!(
                    "ホットキーの共有状態のロックを取得できないのでリスナーの停止を確かめられない"
                );
                None
            }
        };
        let Some(failure) = failure else {
            return;
        };

        self.hook_error = Some(failure);
        let actions: Vec<HotkeyAction> = self.registered.keys().copied().collect();
        for action in actions {
            self.unregister(action);
        }
    }

    /// その組み合わせを既に使っているアクション。
    fn action_for_chord(&self, chord: KeyChord) -> Option<HotkeyAction> {
        self.registered
            .iter()
            .find(|(_, (_, hotkey))| *hotkey == chord)
            .map(|(action, _)| *action)
    }

    /// 失敗を記録する。同じ理由が続く間はログに出さない。
    ///
    /// 登録に失敗したアクションは 2 秒ごとの再適用で試し直すため、毎回
    /// ログへ書くと同じ行が延々と積もる。
    fn record_error(&mut self, action: HotkeyAction, hotkey: &str, reason: HotkeyError) {
        let error = HotkeyAssignmentError {
            hotkey: hotkey.to_string(),
            reason,
        };
        if self.errors.get(&action) != Some(&error) {
            error!(
                "{} に {} を割り当てられない: {}",
                action.label(),
                error.hotkey,
                error.reason
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

    // テストで使う仮想キーコード。英字と数字は ASCII の大文字・数字と同じ値
    const VK_A: u32 = 0x41;
    const VK_M: u32 = 0x4D;
    const VK_S: u32 = 0x53;
    const VK_Z: u32 = 0x5A;
    const VK_0: u32 = 0x30;
    const VK_5: u32 = 0x35;
    const VK_9: u32 = 0x39;
    const VK_F5: u32 = 0x74;
    const VK_F9: u32 = 0x78;
    const VK_F10: u32 = 0x79;
    const VK_F12: u32 = 0x7B;

    fn assert_hotkey(actual: &KeyChord, expected_mods: Modifiers, expected_vk: u32) {
        assert_eq!(
            *actual,
            KeyChord {
                modifiers: expected_mods,
                vk: expected_vk,
            }
        );
    }

    #[test]
    fn parse_hotkey_modifier_only_returns_error() {
        // 修飾キーだけでは登録できないため、パース時点で弾く
        assert_eq!(parse_hotkey("Ctrl"), Err(HotkeyError::MissingKey));
        assert_eq!(parse_hotkey("Ctrl+Shift"), Err(HotkeyError::MissingKey));
        assert_eq!(parse_hotkey("Ctrl+Shift+Alt"), Err(HotkeyError::MissingKey));
    }

    #[test]
    fn parse_hotkey_empty_returns_error() {
        // 空文字列は「未対応のキー」ではなく「通常キーが無い」として扱う。
        // "" が split で 1 要素の空文字列になり、修飾キーにも当たらない
        assert_eq!(
            parse_hotkey(""),
            Err(HotkeyError::UnsupportedKey(String::new()))
        );
    }

    #[test]
    fn parse_hotkey_unknown_key_returns_error() {
        // 種別が分かれていれば、設定画面に「未対応のキー: f13」と出せる
        assert_eq!(
            parse_hotkey("Ctrl+Nonexistent"),
            Err(HotkeyError::UnsupportedKey("nonexistent".to_string()))
        );
        assert_eq!(
            parse_hotkey("F13"),
            Err(HotkeyError::UnsupportedKey("f13".to_string()))
        );
    }

    #[test]
    fn parse_hotkey_multiple_key_codes_returns_error() {
        // 黙って最後のキーで上書きせず、エラーにする
        assert_eq!(parse_hotkey("Ctrl+A+B"), Err(HotkeyError::MultipleKeys));
        assert_eq!(parse_hotkey("A+B"), Err(HotkeyError::MultipleKeys));
        assert_eq!(parse_hotkey("F5+F6"), Err(HotkeyError::MultipleKeys));
    }

    #[test]
    fn parse_hotkey_single_key_has_no_modifiers() {
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        assert_hotkey(&hotkey, Modifiers::empty(), VK_F5);
    }

    #[test]
    fn parse_hotkey_with_one_modifier_sets_that_modifier() {
        let hotkey = parse_hotkey("Ctrl+S").expect("Ctrl+S は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL, VK_S);
    }

    #[test]
    fn parse_hotkey_with_two_modifiers_sets_both() {
        let hotkey = parse_hotkey("Ctrl+Shift+A").expect("Ctrl+Shift+A は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);
    }

    #[test]
    fn parse_hotkey_accepts_modifier_aliases() {
        let control = parse_hotkey("Control+A").expect("Control は Ctrl の別名");
        assert_hotkey(&control, Modifiers::CONTROL, VK_A);

        let win = parse_hotkey("Win+A").expect("Win は Super の別名");
        assert_hotkey(&win, Modifiers::SUPER, VK_A);

        let windows = parse_hotkey("Windows+A").expect("Windows は Super の別名");
        assert_hotkey(&windows, Modifiers::SUPER, VK_A);

        let superkey = parse_hotkey("Super+A").expect("Super はそのまま使える");
        assert_hotkey(&superkey, Modifiers::SUPER, VK_A);
    }

    #[test]
    fn parse_hotkey_is_case_insensitive() {
        let upper = parse_hotkey("CTRL+SHIFT+A").expect("大文字でも解析できる");
        assert_hotkey(&upper, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);

        let lower = parse_hotkey("ctrl+shift+a").expect("小文字でも解析できる");
        assert_hotkey(&lower, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);
    }

    #[test]
    fn parse_hotkey_ignores_spaces_around_parts() {
        let hotkey = parse_hotkey(" Ctrl + S ").expect("前後の空白は無視する");
        assert_hotkey(&hotkey, Modifiers::CONTROL, VK_S);
    }

    #[test]
    fn parse_key_code_letters_are_mapped() {
        assert_eq!(parse_key_code("a"), Ok(VK_A));
        assert_eq!(parse_key_code("m"), Ok(VK_M));
        assert_eq!(parse_key_code("z"), Ok(VK_Z));
    }

    #[test]
    fn parse_key_code_function_keys_are_mapped() {
        assert_eq!(parse_key_code("f1"), Ok(VK_F1));
        assert_eq!(parse_key_code("f9"), Ok(VK_F9));
        assert_eq!(parse_key_code("f10"), Ok(VK_F10));
        assert_eq!(parse_key_code("f12"), Ok(VK_F12));
    }

    #[test]
    fn parse_key_code_digits_are_mapped() {
        assert_eq!(parse_key_code("0"), Ok(VK_0));
        assert_eq!(parse_key_code("5"), Ok(VK_5));
        assert_eq!(parse_key_code("9"), Ok(VK_9));
    }

    #[test]
    fn parse_hotkey_digit_with_modifiers_is_accepted() {
        let hotkey = parse_hotkey("Ctrl+Shift+9").expect("Ctrl+Shift+9 は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, VK_9);
    }

    #[test]
    fn parse_key_code_named_keys_are_mapped() {
        assert_eq!(parse_key_code("space"), Ok(VK_SPACE));
        assert_eq!(parse_key_code("enter"), Ok(VK_RETURN));
        assert_eq!(parse_key_code("escape"), Ok(VK_ESCAPE));
    }

    #[test]
    fn parse_key_code_uppercase_is_accepted() {
        // parse_hotkey は小文字化してから渡すが、直接呼ばれても同じ結果になること
        assert_eq!(parse_key_code("A"), Ok(VK_A));
        assert_eq!(parse_key_code("F5"), Ok(VK_F5));
        assert_eq!(parse_key_code("Space"), Ok(VK_SPACE));
    }

    #[test]
    fn parse_key_code_unknown_key_returns_error() {
        assert!(parse_key_code("f13").is_err());
        assert!(parse_key_code("").is_err());
        assert!(parse_key_code("ctrl").is_err());
        // 1 文字でも英字と数字以外は受け付けない（global-hotkey のころと同じ）
        assert!(parse_key_code("-").is_err());
        assert!(parse_key_code("あ").is_err());
        // F キーは表にある 12 個だけ。数値として読んで範囲を広げない
        assert!(parse_key_code("f0").is_err());
        assert!(parse_key_code("f01").is_err());
    }

    // ---- エラーの文言 ----

    #[test]
    fn hotkey_error_display_keeps_the_key_and_the_underlying_reason() {
        // 文言はそのままトーストと設定画面の一覧に出る。キー名や下位の
        // エラー文が落ちると、何を直せばよいのか分からなくなる
        assert_eq!(
            HotkeyError::UnsupportedKey("f13".to_string()).to_string(),
            "未対応のキー: f13"
        );
        assert_eq!(
            HotkeyError::DuplicateAssignment {
                other: HotkeyAction::ToggleFullscreen,
            }
            .to_string(),
            "同じキーが「フルスクリーン切替」に割り当てられています"
        );
        assert_eq!(
            HotkeyError::HookUnavailable(KeyboardHookError::InstallFailed(
                "access denied".to_string()
            ))
            .to_string(),
            "ホットキーの仕組みを初期化できません: キーボードフックを登録できません: access denied"
        );
    }

    #[test]
    fn hotkey_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            HotkeyError::MultipleKeys,
            HotkeyError::MissingKey,
            HotkeyError::UnsupportedKey("f13".to_string()),
            HotkeyError::DuplicateAssignment {
                other: HotkeyAction::Screenshot,
            },
            HotkeyError::HookUnavailable(KeyboardHookError::Unsupported),
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }

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

    // ---- 登録の差分処理 ----
    //
    // HotkeyManager::new は実際にキーボードフックを登録するため、CI では
    // 成功しないことがある。ここで確かめるのは「解除と押下の扱い」だけにし、
    // フックの登録の成否には依存しないテストにしてある。

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
            state.registered.insert(hotkey, HotkeyAction::Screenshot);
            state.pressed.insert(HotkeyAction::Screenshot, 1);
        }

        manager.unregister(HotkeyAction::Screenshot);

        let state = manager.state.lock().expect("ロックが毒されていないこと");
        assert!(state.pressed.is_empty(), "保留中の押下が残っている");
        assert!(
            state.registered.is_empty(),
            "登録中の組み合わせが残っている"
        );
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
            .insert(hotkey, HotkeyAction::Screenshot);

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
    fn try_register_unparsable_hotkey_returns_the_parse_error() {
        // 解釈できない理由をそのまま返す。フックの有無より先に見る
        let manager = HotkeyManager::new();

        assert_eq!(
            manager.try_register("Ctrl+Shift"),
            Err(HotkeyError::MissingKey)
        );
    }

    #[test]
    fn try_register_does_not_register_anything() {
        // 確かめるだけで、照合の表には載せない。載せるのは resume の apply
        let manager = HotkeyManager::new();

        let _ = manager.try_register("F5");

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
    }

    #[test]
    fn try_register_without_hook_reports_why() {
        // フックを使えない環境では、どのキーを選んでも効かない。
        // ダイアログを閉じさせず、理由を出させる
        let mut manager = HotkeyManager::new();
        manager.hook_error = Some(KeyboardHookError::Unsupported);

        assert_eq!(
            manager.try_register("F5"),
            Err(HotkeyError::HookUnavailable(KeyboardHookError::Unsupported))
        );
    }

    #[test]
    fn apply_without_hook_records_the_reason() {
        // 表に載せても押下を観測できないので、失敗として設定画面へ出す
        let mut manager = HotkeyManager::new();
        manager.hook_error = Some(KeyboardHookError::Unsupported);

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "F5")]));

        assert!(!manager.registered.contains_key(&HotkeyAction::Screenshot));
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::Screenshot)
                .map(|error| &error.reason),
            Some(&HotkeyError::HookUnavailable(
                KeyboardHookError::Unsupported
            ))
        );
    }

    #[test]
    fn apply_after_listener_failure_reports_every_assignment() {
        // 動いていたリスナーが止まったら、登録済みのものも含めて失敗として
        // 記録し直す。記録しないと、効かないのに何も表示されない
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;
        let desired = assignments(&[(HotkeyAction::Screenshot, "F5")]);
        manager.apply(&desired);
        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));

        let failure = KeyboardHookError::WaitFailed("failed".to_string());
        manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .listener_failure = Some(failure.clone());
        manager.apply(&desired);

        assert!(manager.registered.is_empty());
        assert!(manager
            .state
            .lock()
            .expect("ロックが毒されていないこと")
            .registered
            .is_empty());
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::Screenshot)
                .map(|error| &error.reason),
            Some(&HotkeyError::HookUnavailable(failure))
        );
    }

    #[test]
    fn apply_with_hook_registers_the_chord_for_the_listener() {
        // フックが使えていれば、リスナーが照合する表に組み合わせが載る
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;

        manager.apply(&assignments(&[(HotkeyAction::Screenshot, "Ctrl+S")]));

        let chord = KeyChord {
            modifiers: Modifiers::CONTROL,
            vk: VK_S,
        };
        assert_eq!(
            manager
                .state
                .lock()
                .expect("ロックが毒されていないこと")
                .registered
                .get(&chord),
            Some(&HotkeyAction::Screenshot)
        );
        assert!(manager.errors.is_empty());
    }

    #[test]
    fn apply_duplicate_chord_keeps_the_first_action() {
        // 同じ組み合わせを 2 つのアクションへ割り当てたら、宣言順で先のほうを残す
        let mut manager = HotkeyManager::new();
        manager.hook_error = None;

        manager.apply(&assignments(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::VolumeUp, "f5"),
        ]));

        assert!(manager.registered.contains_key(&HotkeyAction::Screenshot));
        assert_eq!(
            manager
                .errors
                .get(&HotkeyAction::VolumeUp)
                .map(|error| &error.reason),
            Some(&HotkeyError::DuplicateAssignment {
                other: HotkeyAction::Screenshot
            })
        );
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
