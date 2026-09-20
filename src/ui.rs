use crate::audio::{self, AudioCapabilities, ChoiceSource};
use crate::hotkey::{HotkeyAction, HotkeyError};
use crate::settings::{
    AppSettings, ColorRange, ColorSpace, ScreenshotDestination, ScreenshotFormat, DEFAULT_CHANNELS,
    DEFAULT_SAMPLE_RATE, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY,
};
use crate::status::{ConnectionStatus, ErrorSource, LinkStatus};
use crate::video::{DeviceCapabilities, VideoMode};
use eframe::egui;
use log::debug;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 設定ダイアログで行われた操作。
///
/// 各ボタンの意味は `README.md` の「設定」と
/// `docs/ARCHITECTURE.md` の「適用の境界を明確にする」に合わせている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsDialogAction {
    /// まだ何も押されていない（編集中）
    None,
    /// 適用: ドラフトを実行中の設定へ反映してファイルへ保存する。ダイアログは閉じない
    Apply,
    /// OK: 適用と同じことをしたうえで閉じる
    Ok,
    /// キャンセル: ドラフトを捨てて閉じる。タイトルバーの × も同じ扱い。
    /// 「適用」で既に反映したぶんは元に戻さない
    Cancel,
    /// テスト再生: 効果音を編集中の音量で鳴らすだけ。
    /// 設定は動かさないしダイアログも閉じない
    TestSound,
    /// 設定を書き出す: **実行中の設定**をファイルへ保存する。
    /// 編集中のドラフトではないので、ダイアログの中身は動かない
    ExportSettings,
    /// 設定を読み込む: ファイルを読んでドラフトへ入れる。
    /// 反映は「適用」「OK」で行うので、ここでは実行中の設定を触らない
    ImportSettings,
    /// 設定を初期化: ドラフトを既定値に戻す。こちらも反映は「適用」「OK」
    ResetDraft,
}

/// 「その他」タブに出す 1 行のメッセージ。
///
/// 書き出し・読み込み・初期化の結果をその場で伝える。失敗はトーストでも
/// 出すが、トーストは画面下部に出るためダイアログに隠れることがある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementMessage {
    pub text: String,
    /// 失敗を伝えるものか。表示色を分ける
    pub is_error: bool,
}

/// 設定ダイアログのタブ。
///
/// 「接続状態」を最後に置き、既定は「デバイス設定」のままにしてある。
/// ダイアログを開く主な目的は設定の変更で、状態の確認は調べたいときだけ
/// だからで、先頭に置くと毎回そこを通ることになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Device,
    Screenshot,
    /// 設定の書き出し・読み込み・初期化
    Other,
    /// 映像と音声が実際に何へ繋がっているか、直近の失敗は何か
    Status,
}

/// デバイス能力の取得状態。
///
/// 取得はデバイスを開いて対応表を引く重い処理なので、描画スレッドでは行わず
/// 使い捨てのスレッドへ投げる。ダイアログは進行状況をこの型で受け取って描き分ける。
///
/// 型引数はビデオ（`DeviceCapabilities`）とオーディオ（`AudioCapabilities`）で
/// 中身が違うため。取得と受け渡しの手順は同じなので、キャッシュは共有する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState<T> {
    /// 取得を要求済みで、結果を待っている
    Pending,
    /// 取得できた
    Ready(T),
    /// 取得に失敗した。文字列は画面に出す理由
    Failed(String),
}

/// ビデオデバイスの能力キャッシュ。
pub type VideoCapabilityCache = CapabilityCache<DeviceCapabilities>;
/// オーディオデバイスの能力キャッシュ。入力と出力で別に持つ。
pub type AudioCapabilityCache = CapabilityCache<AudioCapabilities>;

/// デバイス能力のキャッシュと、まだワーカーへ渡していない取得要求。
///
/// 触るのは UI スレッド（`CaptureCardViewer`）だけなのでロックを持たない。
/// 実際の取得は `CaptureCardViewer::dispatch_capability_requests` が別スレッドへ
/// 投げ、結果はチャネル経由で `apply_result` に入る。
pub struct CapabilityCache<T> {
    /// デバイス名 → 取得状態
    states: HashMap<String, CapabilityState<T>>,
    /// まだワーカーへ渡していないデバイス名
    requests: Vec<String>,
    /// デバイスを切り替えた直後で、能力が届いたら選択肢の既定値を
    /// 選び直す対象のデバイス名
    awaiting_defaults: Option<String>,
}

// `#[derive(Default)]` は `T: Default` を要求してしまう。キャッシュの中身は
// 空の HashMap なので、`T` に条件を付けずに実装する
impl<T> Default for CapabilityCache<T> {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            requests: Vec::new(),
            awaiting_defaults: None,
        }
    }
}

impl<T> CapabilityCache<T> {
    /// まだ一度も問い合わせていないデバイスなら、取得を要求して `Pending` にする。
    ///
    /// 既に `Pending` / `Ready` / `Failed` のいずれかなら何もしない。描画のたびに
    /// 呼ばれるため、ここで弾かないと同じデバイスを毎フレーム開きに行く。失敗した
    /// デバイスを問い合わせ直すのは `retry` の仕事。
    ///
    /// 要求を積んだときだけ `true` を返す。
    pub fn request(&mut self, device: &str) -> bool {
        if device.is_empty() || self.states.contains_key(device) {
            return false;
        }
        self.states
            .insert(device.to_string(), CapabilityState::Pending);
        self.requests.push(device.to_string());
        true
    }

    /// 取得済み・失敗済みを問わず問い合わせ直す。「再取得」ボタン用。
    ///
    /// 結果待ちの間に押されても投げ直さない。投げ直すと、先に飛ばした取得が
    /// あとから届いて新しい結果を上書きする。
    pub fn retry(&mut self, device: &str) -> bool {
        if device.is_empty() || self.is_pending(device) {
            return false;
        }
        self.states.remove(device);
        self.request(device)
    }

    /// 溜まっている取得要求を取り出す。呼び出し側がワーカーへ渡す。
    pub fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.requests)
    }

    /// ワーカーから届いた結果を反映する。
    pub fn apply_result(&mut self, device: String, result: Result<T, String>) {
        let state = match result {
            Ok(caps) => CapabilityState::Ready(caps),
            Err(reason) => CapabilityState::Failed(reason),
        };
        self.states.insert(device, state);
    }

    /// 取得状態。まだ要求もしていなければ `None`。
    pub fn state(&self, device: &str) -> Option<&CapabilityState<T>> {
        self.states.get(device)
    }

    /// 取得できた能力。結果待ち・失敗・未要求はいずれも `None` になる。
    pub fn ready(&self, device: &str) -> Option<&T> {
        match self.states.get(device) {
            Some(CapabilityState::Ready(caps)) => Some(caps),
            _ => None,
        }
    }

    /// 結果待ちか。**まだ要求していない場合は `false`。**
    ///
    /// 音声の接続はこれが `false` になるまで待つ（`poll_device_connection`）。
    /// 未要求を `true` にすると、要求を積む経路が無い状態で永久に待ってしまう。
    pub fn is_pending(&self, device: &str) -> bool {
        matches!(self.states.get(device), Some(CapabilityState::Pending))
    }

    /// デバイスが切り替わったことを記録する。能力が届いた時点でフォーマットの
    /// 既定値を選び直させるための目印。
    pub fn expect_defaults(&mut self, device: &str) {
        self.awaiting_defaults = Some(device.to_string());
    }

    /// `device` の能力が届いていて、切り替え直後の選び直しがまだなら `true`。
    ///
    /// 一度 `true` を返したら目印を消す。消さないと、ユーザーが選び直した
    /// フォーマットを毎フレーム先頭へ戻してしまう。
    pub fn should_apply_defaults(&mut self, device: &str) -> bool {
        if self.awaiting_defaults.as_deref() != Some(device) {
            return false;
        }
        if !matches!(self.states.get(device), Some(CapabilityState::Ready(_))) {
            return false;
        }
        self.awaiting_defaults = None;
        true
    }
}

/// ホットキー入力ダイアログの入力状態。
///
/// 以前は `static mut CAPTURING` / `static mut TEMP_HOTKEY` に持っていた。
/// `static_mut_refs` が Rust 2024 edition でエラーになるほか、
/// 参照のたびに `unsafe` が要るため構造体へ移した。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeyCaptureState {
    /// キー入力の待機中か
    capturing: bool,
    /// 待機中に確定したホットキー文字列。
    /// OK を押すまで呼び出し側へは渡さない
    temp: String,
    /// どのアクションのホットキーを編集しているか。
    ///
    /// **`reset` でも消さない。** 「クリア」で閉じたときに、呼び出し側が
    /// どのアクションを未設定にすればよいか分からなくなるため。
    /// 次に一覧の「設定...」が押されたときに入れ替わる。
    editing: Option<HotkeyAction>,
}

impl HotkeyCaptureState {
    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// 編集対象のアクションを決めて、入力状態を初期化する。
    /// 一覧の「設定...」から呼ぶ。
    pub fn begin_for(&mut self, action: HotkeyAction) {
        self.editing = Some(action);
        self.capturing = false;
        self.temp.clear();
    }

    /// 編集中のアクション。まだ一度も開いていなければ `None`。
    pub fn editing(&self) -> Option<HotkeyAction> {
        self.editing
    }

    /// 待機中に確定したホットキー文字列。まだ何も取れていなければ空。
    pub fn temp(&self) -> &str {
        &self.temp
    }

    /// キー入力の待機を始める。前回の取得結果は捨てる。
    pub fn start(&mut self) {
        self.capturing = true;
        self.temp.clear();
    }

    /// キー入力の待機をやめる。取得済みの文字列は残す。
    pub fn stop(&mut self) {
        self.capturing = false;
    }

    /// キーの組み合わせが確定したので待機を終える。
    pub fn finish(&mut self, hotkey: String) {
        self.temp = hotkey;
        self.capturing = false;
    }

    /// 確定した文字列を取り出して状態を空に戻す。
    /// 何も取れていなければ `None` を返し、呼び出し側の値を書き換えさせない。
    pub fn take_captured(&mut self) -> Option<String> {
        self.capturing = false;
        if self.temp.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.temp))
        }
    }

    /// 取得結果ごと状態を捨てる。キャンセルとクリアで使う。
    pub fn reset(&mut self) {
        self.capturing = false;
        self.temp.clear();
    }
}

/// 操作に対して、ダイアログの外側（`CaptureCardViewer`）が行うこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsDialogTransition {
    /// ドラフトを実行中の設定へ反映するか
    pub commit_draft: bool,
    /// 設定ファイルへ保存するか
    pub save_to_file: bool,
    /// ダイアログを閉じるか
    pub close: bool,
}

/// 設定ダイアログの状態。
///
/// ダイアログは共有の `AppSettings` を直接書き換えず、開いたときに複製した
/// ドラフトを編集する。ドラフトが実行中の設定へ移るのは「適用」と「OK」の
/// ときだけで、閉じるときは必ず捨てる。
///
/// こうしないと、未確定の編集が共有設定へ混ざり、ウィンドウ操作や音量変更を
/// きっかけにした保存に巻き込まれてファイルへ書き出されてしまう。
///
/// 選択中のタブ、デバイス能力のキャッシュ、ホットキー入力の状態は
/// ドラフトとは別に持つ。これらは設定の中身ではないので「適用」や「キャンセル」
/// では捨てず、ダイアログを開き直しても引き継ぐ（static だったときと同じ振る舞い）。
#[derive(Default)]
pub struct SettingsDialogState {
    draft: Option<AppSettings>,
    // 開いた時点の設定。ドラフトのどの項目が実際に編集されたかを判別するために持つ
    original: Option<AppSettings>,
    // 選択中のタブ
    selected_tab: SettingsTab,
    // デバイス名 → そのデバイスが扱えるフォーマット・解像度・FPS の取得状態。
    // 取得はデバイスを開く重い処理なので別スレッドへ投げ、一度取ったら保持する
    capabilities: VideoCapabilityCache,
    // 音声デバイスの対応設定。入力と出力で別に持つ。
    // **名前で引くので、入力と出力に同名のデバイスがあっても混ざらないよう分ける。**
    audio_input_capabilities: AudioCapabilityCache,
    audio_output_capabilities: AudioCapabilityCache,
    // ホットキー入力ダイアログの入力状態
    hotkey_capture: HotkeyCaptureState,
    // 「その他」タブに出す直近の結果。ドラフトについての説明なので、
    // ドラフトを作り直すとき・捨てるときに一緒に捨てる
    management_message: Option<ManagementMessage>,
    // 「設定を初期化」の確認待ちか。押し間違いで設定が消えないよう、
    // 1 段目のボタンではこれを立てるだけにして、2 段目で確定させる
    reset_confirm: bool,
}

impl SettingsDialogState {
    /// 編集中のドラフトを持っているか。
    pub fn has_draft(&self) -> bool {
        self.draft.is_some()
    }

    /// 現在の設定を複製して編集を始める。
    ///
    /// 呼ぶたびに作り直す。以前は `TEMP_SETTINGS` が None のときだけ退避し、
    /// × で閉じたときに消していなかったため、次に開いたときへ古い値が
    /// 持ち越されていた。
    pub fn begin_edit(&mut self, current: &AppSettings) {
        self.draft = Some(current.clone());
        self.original = Some(current.clone());
        self.forget_management_state();
    }

    /// 編集を終える。ドラフトは捨てる。
    pub fn end_edit(&mut self) {
        self.draft = None;
        self.original = None;
        self.forget_management_state();
    }

    /// 「その他」タブの表示状態を捨てる。
    ///
    /// メッセージも確認待ちもドラフトについてのものなので、ドラフトを
    /// 作り直すとき・捨てるときに残さない。残すと、開き直したダイアログに
    /// 「読み込みました」が出たままになる。
    fn forget_management_state(&mut self) {
        self.management_message = None;
        self.reset_confirm = false;
    }

    /// 「その他」タブに出すメッセージを差し替える。
    pub fn set_management_message(&mut self, text: String, is_error: bool) {
        self.management_message = Some(ManagementMessage { text, is_error });
    }

    /// 「その他」タブのメッセージを消す。
    ///
    /// 「適用」で反映したあとに呼ぶ。「適用」か「OK」で反映してください、と
    /// 促す文言が、反映したあとも残ると読み手を迷わせる。
    pub fn clear_management_message(&mut self) {
        self.management_message = None;
    }

    pub fn draft(&self) -> Option<&AppSettings> {
        self.draft.as_ref()
    }

    pub fn draft_mut(&mut self) -> Option<&mut AppSettings> {
        self.draft.as_mut()
    }

    /// ホットキー入力ダイアログの入力状態。
    ///
    /// ホットキー入力ダイアログは設定ダイアログから開くが、設定ダイアログを
    /// × で先に閉じても入力中の状態を失わないよう、`end_edit` では触らない。
    pub fn hotkey_capture_mut(&mut self) -> &mut HotkeyCaptureState {
        &mut self.hotkey_capture
    }

    /// ホットキー入力ダイアログの入力状態（読み取り）。
    pub fn hotkey_capture(&self) -> &HotkeyCaptureState {
        &self.hotkey_capture
    }

    /// デバイス能力の取得状態。
    ///
    /// 取得要求の取り出しと結果の反映は `CaptureCardViewer` が行うため、
    /// ダイアログを開いていない間（起動時の先読み）も触られる。
    pub fn capabilities_mut(&mut self) -> &mut VideoCapabilityCache {
        &mut self.capabilities
    }

    /// オーディオ入力デバイスの対応設定。
    ///
    /// ビデオ側と同じく、取得要求の取り出しと結果の反映は `CaptureCardViewer`
    /// が行う。音声の接続も開く直前にここを読むため、ダイアログを開いていない
    /// 間も触られる。
    pub fn audio_input_capabilities_mut(&mut self) -> &mut AudioCapabilityCache {
        &mut self.audio_input_capabilities
    }

    /// オーディオ出力デバイスの対応設定。
    pub fn audio_output_capabilities_mut(&mut self) -> &mut AudioCapabilityCache {
        &mut self.audio_output_capabilities
    }

    /// オーディオ入力デバイスの対応設定（読み取り）。
    pub fn audio_input_capabilities(&self) -> &AudioCapabilityCache {
        &self.audio_input_capabilities
    }

    /// オーディオ出力デバイスの対応設定（読み取り）。
    pub fn audio_output_capabilities(&self) -> &AudioCapabilityCache {
        &self.audio_output_capabilities
    }

    /// ドラフトを実行中の設定へ反映する。ドラフトを持っていなければ何もしない。
    pub fn commit_into(&self, target: &mut AppSettings) {
        if let (Some(draft), Some(original)) = (&self.draft, &self.original) {
            commit_draft(target, draft, original);
        }
    }

    /// 操作に対して、ダイアログの外側が行うことを決める。
    pub fn transition_for(action: SettingsDialogAction) -> SettingsDialogTransition {
        match action {
            SettingsDialogAction::None => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: false,
            },
            SettingsDialogAction::Apply => SettingsDialogTransition {
                commit_draft: true,
                save_to_file: true,
                close: false,
            },
            SettingsDialogAction::Ok => SettingsDialogTransition {
                commit_draft: true,
                save_to_file: true,
                close: true,
            },
            SettingsDialogAction::Cancel => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: true,
            },
            // 効果音を鳴らすだけなので、ドラフトもファイルもダイアログも動かさない
            SettingsDialogAction::TestSound => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: false,
            },
            // 書き出し・読み込み・初期化は、呼び出し側が別に処理する。
            //
            // **`save_to_file` を立てない。** ここでの保存は
            // `%AppData%` の設定ファイルへの書き出しを指しており、
            // ユーザーが選んだ場所への書き出しとは別物。読み込みと初期化も
            // ドラフトを差し替えるだけで、反映は「適用」「OK」に任せる
            SettingsDialogAction::ExportSettings
            | SettingsDialogAction::ImportSettings
            | SettingsDialogAction::ResetDraft => SettingsDialogTransition {
                commit_draft: false,
                save_to_file: false,
                close: false,
            },
        }
    }
}

/// 描画後の状態から、実際に行われた操作を決める。
///
/// `window_still_open` は `egui::Window::open()` に渡した値の描画後の状態。
/// タイトルバーの × で閉じられるとボタンを押さずに false になるため、
/// キャンセルと同じ扱いにする。これを拾わないと、ドラフトを捨てる処理が
/// 走らずに次へ持ち越される。
pub fn resolve_action(
    button: SettingsDialogAction,
    window_still_open: bool,
) -> SettingsDialogAction {
    if !window_still_open && button == SettingsDialogAction::None {
        SettingsDialogAction::Cancel
    } else {
        button
    }
}

/// デバイスを切り替えたときに選び直すビデオの既定値を決める。
///
/// 返すのは `(フォーマット名, (幅, 高さ), fps)`。能力が空なら `None` を返し、
/// 呼び出し側は設定を触らない。
///
/// フォーマットは能力一覧の先頭（組み合わせを 1 つ以上持つもの）を採る。
/// `video::get_device_capabilities` が YUY2 → MJPEG → RGB24 の順で積むため、
/// 実質 YUY2 が優先される。
///
/// 解像度と FPS は**切り替え前の値に最も近い組み合わせ**を選ぶ。デバイスを
/// 替えただけで 1080p60 が 640x480 まで落ちると使い物にならないため、
/// 直前の設定を手掛かりにする。前の値が無い（初回など）ときは、対応する中で
/// 最大の解像度・最高の FPS を選ぶ。
///
/// **フォーマットだけを入れ直してはいけない。** 解像度と FPS が前のデバイスの
/// 値のまま残ると、新しいデバイスが対応していない組み合わせが画面に出て、
/// `start_capture` が `Closest` で寄せた実際の設定と表示が食い違う。
pub fn select_default_video_mode(
    caps: &DeviceCapabilities,
    previous_resolution: Option<(u32, u32)>,
    previous_fps: Option<u32>,
) -> Option<(String, (u32, u32), u32)> {
    // 組み合わせを持たないフォーマットを選ぶと、解像度の選択肢が空になる
    let capability = caps
        .iter()
        .find(|capability| !capability.modes.is_empty())?;

    let &mode = capability
        .modes
        .iter()
        .min_by_key(|&&mode| video_mode_rank(mode, previous_resolution, previous_fps))?;

    Some((capability.name.clone(), mode.resolution(), mode.fps))
}

/// `select_default_video_mode` の並べ替えキー。小さいほど「望ましい」。
///
/// 1. 前の解像度との画素数の差（前の値が無ければ全て 0 で並ばない）
/// 2. 前の解像度との幅・高さの差の和（同上）
/// 3. 前の FPS との差（同上）
/// 4. 画素数の降順
/// 5. FPS の降順
///
/// 2 が要るのは、画素数だけでは縦横比の違う同面積の解像度が並んでしまうため。
/// 1280x720 と 960x960 はどちらも 921,600 画素なので、前の解像度に完全一致
/// する側が一覧の後ろにあると取りこぼす。
///
/// 4 と 5 は「前の値が無いときは最大の解像度・最高の FPS」という既定であり、
/// 同時に 1〜3 が並んだときの決着でもある。ここが無いと `HashMap` 由来の
/// 順序でフレームごとに違う値が選ばれうる。
fn video_mode_rank(
    mode: VideoMode,
    previous_resolution: Option<(u32, u32)>,
    previous_fps: Option<u32>,
) -> (
    u64,
    u64,
    u64,
    std::cmp::Reverse<u64>,
    std::cmp::Reverse<u32>,
) {
    let pixels = mode.pixel_count();

    let (pixel_distance, dimension_distance) = match previous_resolution {
        Some((prev_width, prev_height)) => (
            pixels.abs_diff(u64::from(prev_width) * u64::from(prev_height)),
            u64::from(mode.width.abs_diff(prev_width))
                + u64::from(mode.height.abs_diff(prev_height)),
        ),
        None => (0, 0),
    };
    let fps_distance = match previous_fps {
        Some(prev_fps) => u64::from(mode.fps.abs_diff(prev_fps)),
        None => 0,
    };

    (
        pixel_distance,
        dimension_distance,
        fps_distance,
        std::cmp::Reverse(pixels),
        std::cmp::Reverse(mode.fps),
    )
}

/// ドラフトのうち、設定ダイアログが編集する範囲だけを実行中の設定へ反映する。
/// `original` はダイアログを開いた時点の設定。
///
/// `ui` セクションを丸ごと上書きしないのは、ウィンドウのサイズ・位置、
/// 最前面表示、画面ドラッグ移動がダイアログの外で変わるため。丸ごと入れると、
/// ダイアログを開いている間に動かしたウィンドウの位置が、開いた時点の
/// スナップショットで巻き戻る。
///
/// ダイアログでも外でも変えられる `maintain_aspect_ratio` と `volume` は、
/// **ダイアログで実際に編集されたときだけ**反映する。開いた時点の値と同じなら
/// 外側の変更（映像上でのホイール操作、コンテキストメニュー）を残す。無条件に
/// 入れると、ダイアログを開いたままホイールで音量を変えて「適用」を押したときに
/// 音量が巻き戻る。
///
/// `audio` / `screenshot` にこの比較が要らないのは、ダイアログの外から
/// 書き換わらないため。ホットキー入力ダイアログもドラフトへ書く。
///
/// `video` の `auto_reconnect` だけは例外で、ダイアログに無く右クリックメニューで
/// 切り替える。丸ごと上書きすると、ダイアログを開いている間の切り替えが
/// 開いた時点のスナップショットで巻き戻るため、`ui` の 2 項目と同じ比較を使い、
/// **ドラフトで実際に変わったときだけ**反映する。通常の編集ではドラフトの値が
/// 動かないので、これまでどおり実行中の値が残る。動くのは設定の読み込みと
/// 初期化だけで、そのときはユーザーが選んだ内容を反映する側が正しい。
///
/// `ui` の `muted` もダイアログに無い（右クリックメニュー・ミドルクリック・
/// ホットキーで切り替える）。ここで触らないので、ダイアログを開いている間の
/// 切り替えはそのまま残る。
///
/// **ダイアログに `ui` セクションの項目を足すときは、ここにも足すこと。**
/// **逆に、ダイアログの外だけで変える項目を足すときは、ここで残すこと。**
pub fn commit_draft(target: &mut AppSettings, draft: &AppSettings, original: &AppSettings) {
    let auto_reconnect = target.video.auto_reconnect;
    target.video = draft.video.clone();
    // ドラフトが開いた時点のままなら、ダイアログの外（右クリックメニュー）で
    // 切り替えた値を残す。変わっているのは読み込みと初期化のときだけ
    if draft.video.auto_reconnect == original.video.auto_reconnect {
        target.video.auto_reconnect = auto_reconnect;
    }
    target.audio = draft.audio.clone();
    target.screenshot = draft.screenshot.clone();
    // ホットキーの割り当てもダイアログの中だけで変わる
    target.hotkeys = draft.hotkeys.clone();

    // ダイアログの「ユーザーインターフェース」グループが編集する 2 項目
    if draft.ui.maintain_aspect_ratio != original.ui.maintain_aspect_ratio {
        target.ui.maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;
    }
    if draft.ui.volume != original.ui.volume {
        target.ui.volume = draft.ui.volume;
    }
}

/// 読み込んだ設定からドラフトを作る。
///
/// `current` は差し替える前のドラフト。`imported` の `ui` セクションは
/// **ダイアログが編集する 2 項目（`volume` / `maintain_aspect_ratio`）だけ**を
/// 採り、残りは `current` の値を保つ。
///
/// ウィンドウの位置とサイズを持ち込まないのがいちばんの理由。別の画面構成の
/// PC で書き出したファイルを読むと、画面の外にウィンドウが飛ぶ。
///
/// 他の `ui` の項目（`always_on_top` / `enable_drag_move` / `show_stats_overlay` /
/// `muted`）を持ち込まないのは、**`commit_draft` がそれらを反映しないため。**
/// 右クリックメニューで切り替えるものなので、ドラフトへ入れても「適用」で
/// 実行中の値へ戻る。読めたように見えて反映されない項目を作るより、
/// 最初から触らないほうが分かりやすい。
///
/// `video.auto_reconnect` は同じく右クリックメニューで切り替えるが、
/// `commit_draft` が「ドラフトで変わったときだけ反映する」形になっているので
/// **読み込んだ値をそのまま採る。** 読み込みも初期化もドラフトの編集なので、
/// 反映される側が正しい。
///
/// **`commit_draft` が反映しない項目を増やすときは、ここでも `current` の値を
/// 保つこと。逆に反映する項目を増やすときは、ここでも `imported` から採ること。**
/// 2 つが食い違うと、読み込んだのに反映されない項目が生まれる。
pub fn draft_from_imported(imported: AppSettings, current: &AppSettings) -> AppSettings {
    let mut draft = imported;
    let volume = draft.ui.volume;
    let maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;

    draft.ui = current.ui.clone();
    draft.ui.volume = volume;
    draft.ui.maintain_aspect_ratio = maintain_aspect_ratio;
    draft
}

/// 初期化でドラフトを作る。
///
/// 既定値を読み込んだのと同じ扱いにしてある。ウィンドウの位置とサイズが
/// 保たれるのも、`ui` の他の項目が現状のまま残るのも読み込みと同じ。
///
/// 初期化でウィンドウが既定の大きさに戻らないのは意図した動作。位置と
/// サイズは設定ダイアログで触れる項目ではなく、初期化したい対象でもない。
pub fn draft_from_defaults(current: &AppSettings) -> AppSettings {
    draft_from_imported(AppSettings::default(), current)
}

/// 設定ダイアログへ渡すデバイスの一覧。
///
/// 3 本の借用を個別に渡していたが、引数が増えすぎたのでまとめた。
/// 入力デバイスと出力デバイスはどちらも `&[String]` で、順番を取り違えても
/// コンパイルが通ってしまうため、名前で区別できる形にする意味もある。
///
/// 中身は `CaptureCardViewer` がキャッシュしているもの。デバイスの列挙は
/// 重いので、ダイアログ側からは列挙しない。
pub struct DeviceLists<'a> {
    /// ビデオデバイス `(名前, 説明)`
    pub video: &'a [(String, String)],
    /// オーディオ入力デバイス名
    pub input: &'a [String],
    /// オーディオ出力デバイス名
    pub output: &'a [String],
}

/// 設定ダイアログを描画し、行われた操作を返す。
///
/// 編集対象は `dialog` が持つドラフトで、実行中の設定はここでは触らない。
/// ドラフトの反映・保存・クローズは呼び出し側が `transition_for` の結果に
/// 従って行う。
pub fn show_settings_dialog(
    ctx: &egui::Context,
    show_settings: &mut bool,
    dialog: &mut SettingsDialogState,
    show_hotkey_dialog: &mut bool,
    devices: &DeviceLists<'_>,
    connection: &ConnectionStatus,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyError>,
) -> SettingsDialogAction {
    // フィールドごとに分解して受ける。ドラフトを編集しながら
    // デバイス能力のキャッシュやホットキー入力の状態も書き換えるため、
    // dialog をまるごと借りると二重の可変借用になる
    let SettingsDialogState {
        draft,
        selected_tab,
        capabilities,
        audio_input_capabilities,
        audio_output_capabilities,
        hotkey_capture,
        management_message,
        reset_confirm,
        ..
    } = dialog;

    let mut audio_capabilities = AudioCapabilityCaches {
        input: audio_input_capabilities,
        output: audio_output_capabilities,
    };

    // ドラフトが用意できていなければ描画しない。呼び出し側が begin_edit を
    // 呼ぶまで待つ
    let Some(draft) = draft.as_mut() else {
        return SettingsDialogAction::None;
    };

    let mut button = SettingsDialogAction::None;

    egui::Window::new("設定")
        .open(show_settings)
        .default_size([650.0, 500.0])
        .resizable(true)
        .show(ctx, |ui| {
            // タブ選択
            ui.horizontal(|ui| {
                ui.selectable_value(selected_tab, SettingsTab::Device, "デバイス設定");
                ui.selectable_value(
                    selected_tab,
                    SettingsTab::Screenshot,
                    "スクリーンショット設定",
                );
                ui.selectable_value(selected_tab, SettingsTab::Other, "その他");
                ui.selectable_value(selected_tab, SettingsTab::Status, "接続状態");
            });

            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| match selected_tab {
                SettingsTab::Device => show_device_settings_tab(
                    ui,
                    draft,
                    capabilities,
                    &mut audio_capabilities,
                    devices,
                ),
                SettingsTab::Screenshot => {
                    if show_screenshot_settings_tab(
                        ui,
                        draft,
                        show_hotkey_dialog,
                        hotkey_capture,
                        hotkey_errors,
                    ) {
                        button = SettingsDialogAction::TestSound;
                    }
                }
                SettingsTab::Other => {
                    let requested = show_other_tab(ui, management_message.as_ref(), reset_confirm);
                    if requested != SettingsDialogAction::None {
                        button = requested;
                    }
                }
                SettingsTab::Status => show_status_tab(ui, connection),
            });

            ui.separator();

            // OK、キャンセル、適用ボタン
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    button = SettingsDialogAction::Ok;
                }

                if ui.button("キャンセル").clicked() {
                    button = SettingsDialogAction::Cancel;
                }

                if ui.button("適用").clicked() {
                    button = SettingsDialogAction::Apply;
                }
            });
        });

    resolve_action(button, *show_settings)
}

/// オーディオの入力・出力の能力キャッシュをまとめて渡すための束。
///
/// 2 本の `&mut` を個別に引数へ並べると入れ替えても型が合ってしまうため、
/// 名前で区別できる形にする（`DeviceLists` と同じ理由）。
pub struct AudioCapabilityCaches<'a> {
    pub input: &'a mut AudioCapabilityCache,
    pub output: &'a mut AudioCapabilityCache,
}

fn show_device_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    capabilities: &mut VideoCapabilityCache,
    audio_capabilities: &mut AudioCapabilityCaches<'_>,
    devices: &DeviceLists<'_>,
) {
    ui.heading("デバイス設定");
    ui.add_space(10.0);

    // ビデオ設定
    ui.group(|ui| {
        ui.strong("ビデオ設定");
        ui.add_space(5.0);

        // ビデオデバイス選択
        // 一覧は main.rs 側でキャッシュ済みのものを受け取る（毎フレームの列挙を避けるため）
        let current_device = settings.video.device_name.clone().unwrap_or_default();

        let mut device_changed = false;
        egui::ComboBox::from_label("ビデオデバイス")
            .selected_text(if current_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_device
            })
            .show_ui(ui, |ui| {
                for (name, description) in devices.video {
                    let display_text = if description.is_empty() {
                        name.clone()
                    } else {
                        format!("{} ({})", name, description)
                    };
                    if ui
                        .selectable_label(
                            settings.video.device_name.as_ref() == Some(name),
                            display_text,
                        )
                        .clicked()
                        && settings.video.device_name.as_ref() != Some(name)
                    {
                        settings.video.device_name = Some(name.clone());
                        device_changed = true;
                    }
                }
            });

        // 選択後のデバイス名。この下の能力参照はすべてこちらを使う。
        // 切り替えたフレームで切り替え前の名前を見ると、1 フレームだけ前の
        // デバイスの選択肢が出てしまう
        let selected_device = settings.video.device_name.clone().unwrap_or_default();

        if device_changed {
            // 能力が届いた時点でフォーマットを選び直させる
            capabilities.expect_defaults(&selected_device);
        }

        // 能力の取得を要求する。デバイスを開くのは別スレッドなので UI は止まらない。
        // 要求済み・取得済み・失敗済みのときは何も起きない
        capabilities.request(&selected_device);

        // 取得の進行状況。失敗を黙って捨てると、選択肢が既定値のまま出る理由が
        // ユーザーに分からない
        let mut retry_requested = false;
        match capabilities.state(&selected_device) {
            Some(CapabilityState::Pending) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("対応形式を取得中...");
                });
            }
            Some(CapabilityState::Failed(reason)) => {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!("⚠ 対応形式を取得できませんでした: {}", reason),
                    );
                    if ui.button("再取得").clicked() {
                        retry_requested = true;
                    }
                });
                ui.label("下の選択肢は既定値です。");
            }
            _ => {}
        }
        if retry_requested {
            capabilities.retry(&selected_device);
        }

        // 切り替えたデバイスの能力が届いたら、フォーマット・解像度・FPS を
        // まとめて選び直す。
        //
        // 能力の取得は別スレッドなので、切り替えた直後はまだ `Pending` で
        // ここを通らない。その間は前のデバイスの値が出たままになるが、
        // 入れ直しの手掛かりとして必要なので消さない。`expect_defaults` の
        // 目印が残るため、`Ready` になったフレームで入れ直される。
        // 取得に失敗したときは入れ直さない（選択肢が既定値のままなので、
        // そこへ寄せても実態に合わない）。
        if capabilities.should_apply_defaults(&selected_device) {
            if let Some((format, resolution, fps)) =
                capabilities.ready(&selected_device).and_then(|caps| {
                    select_default_video_mode(caps, settings.video.resolution, settings.video.fps)
                })
            {
                debug!(
                    "デバイスを {} に切り替えたので既定値を選び直した: {} {}x{} {}fps",
                    selected_device, format, resolution.0, resolution.1, fps
                );
                settings.video.format = Some(format);
                settings.video.resolution = Some(resolution);
                settings.video.fps = Some(fps);
            }
        }

        // フォーマット選択（フォーマットが起点）
        let mut format_changed = false;
        ui.horizontal(|ui| {
            ui.label("フォーマット:");
            let current_format = settings
                .video
                .format
                .clone()
                .unwrap_or_else(|| "YUY2".to_string());

            egui::ComboBox::from_id_source("format_combo")
                .selected_text(&current_format)
                .show_ui(ui, |ui| {
                    // キャッシュからフォーマット一覧を取得
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        for capability in caps {
                            if ui
                                .selectable_value(
                                    &mut settings.video.format,
                                    Some(capability.name.clone()),
                                    &capability.name,
                                )
                                .clicked()
                            {
                                format_changed = true;
                            }
                        }
                    } else {
                        // キャッシュがない場合はデフォルト
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("YUY2".to_string()),
                            "YUY2",
                        );
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("MJPEG".to_string()),
                            "MJPEG",
                        );
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("RGB24".to_string()),
                            "RGB24",
                        );
                    }
                });
        });

        // フォーマット変更時に解像度をリセット
        if format_changed {
            if let Some(caps) = capabilities.ready(&selected_device) {
                if let Some(current_format) = &settings.video.format {
                    // 現在のフォーマットに対応する最初の解像度を選択
                    for capability in caps {
                        if &capability.name == current_format {
                            if let Some(mode) = capability.modes.first() {
                                settings.video.resolution = Some(mode.resolution());
                                settings.video.fps = Some(mode.fps);
                            }
                            break;
                        }
                    }
                }
            }
        }

        // 解像度選択（フォーマットに応じて動的に変更）
        let mut resolution_changed = false;
        ui.horizontal(|ui| {
            ui.label("解像度:");
            let current_resolution = settings.video.resolution.unwrap_or((1280, 720));

            egui::ComboBox::from_id_source("resolution_combo")
                .selected_text(format!("{}x{}", current_resolution.0, current_resolution.1))
                .show_ui(ui, |ui| {
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        if let Some(current_format) = &settings.video.format {
                            // 現在のフォーマットに対応する解像度一覧
                            let mut unique_resolutions =
                                std::collections::HashSet::<(u32, u32)>::new();
                            for capability in caps {
                                if &capability.name == current_format {
                                    for mode in &capability.modes {
                                        unique_resolutions.insert(mode.resolution());
                                    }
                                }
                            }

                            // ソートして表示
                            let mut sorted_resolutions: Vec<_> =
                                unique_resolutions.into_iter().collect();
                            sorted_resolutions.sort_by(|a, b| {
                                let size_a = a.0 * a.1;
                                let size_b = b.0 * b.1;
                                size_b.cmp(&size_a)
                            });

                            for (w, h) in sorted_resolutions {
                                if ui
                                    .selectable_value(
                                        &mut settings.video.resolution,
                                        Some((w, h)),
                                        format!("{}x{}", w, h),
                                    )
                                    .clicked()
                                {
                                    resolution_changed = true;
                                }
                            }
                        }
                    } else {
                        // デフォルトの解像度
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((1920, 1080)),
                            "1920x1080",
                        );
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((1280, 720)),
                            "1280x720",
                        );
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((640, 480)),
                            "640x480",
                        );
                    }
                });
        });

        // 解像度変更時にFPSをリセット
        if resolution_changed {
            if let Some(caps) = capabilities.ready(&selected_device) {
                if let Some(current_format) = &settings.video.format {
                    if let Some((w, h)) = settings.video.resolution {
                        // 現在のフォーマットと解像度に対応する最初のFPSを選択
                        for capability in caps {
                            if &capability.name == current_format {
                                for mode in &capability.modes {
                                    if mode.resolution() == (w, h) {
                                        settings.video.fps = Some(mode.fps);
                                        break;
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // FPS選択（フォーマットと解像度に応じて動的に変更）
        ui.horizontal(|ui| {
            ui.label("フレームレート:");
            let current_fps = settings.video.fps.unwrap_or(30);

            egui::ComboBox::from_id_source("fps_combo")
                .selected_text(format!("{} fps", current_fps))
                .show_ui(ui, |ui| {
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        if let Some(current_format) = &settings.video.format {
                            if let Some((w, h)) = settings.video.resolution {
                                // 現在のフォーマットと解像度に対応するFPS一覧
                                let mut available_fps = Vec::new();
                                for capability in caps {
                                    if &capability.name == current_format {
                                        for mode in &capability.modes {
                                            if mode.resolution() == (w, h) {
                                                available_fps.push(mode.fps);
                                            }
                                        }
                                    }
                                }

                                // 重複を削除してソート
                                available_fps.sort();
                                available_fps.dedup();
                                available_fps.reverse(); // 大きい順

                                for fps in available_fps {
                                    ui.selectable_value(
                                        &mut settings.video.fps,
                                        Some(fps),
                                        format!("{} fps", fps),
                                    );
                                }
                            }
                        }
                    } else {
                        // デフォルトのFPS
                        ui.selectable_value(&mut settings.video.fps, Some(30), "30 fps");
                        ui.selectable_value(&mut settings.video.fps, Some(60), "60 fps");
                    }
                });
        });

        // 色空間の選択。デバイスは入力信号の色空間を通知してこないので、
        // 通常は解像度から推定する（自動）。推定が外れる機種のために固定できる
        ui.horizontal(|ui| {
            ui.label("色空間:");
            egui::ComboBox::from_id_source("color_space_combo")
                .selected_text(settings.video.color_space.label())
                .show_ui(ui, |ui| {
                    for space in ColorSpace::ALL {
                        ui.selectable_value(&mut settings.video.color_space, space, space.label());
                    }
                })
                .response
                .on_hover_text("色がずれて見える場合に切り替えます。通常は自動のままで構いません");
        });

        // 輝度レンジの選択。フルレンジで出すかどうかはデバイス側の設定次第で、
        // 信号からも解像度からも判別できないため手で選ばせる
        ui.horizontal(|ui| {
            ui.label("色レンジ:");
            egui::ComboBox::from_id_source("color_range_combo")
                .selected_text(settings.video.color_range.label())
                .show_ui(ui, |ui| {
                    for range in ColorRange::ALL {
                        ui.selectable_value(&mut settings.video.color_range, range, range.label());
                    }
                })
                .response
                .on_hover_text("黒が灰色に浮く、または黒潰れ・白飛びする場合に切り替えます");
        });
    });

    ui.add_space(15.0);

    // オーディオ設定
    ui.group(|ui| {
        ui.strong("オーディオ設定");
        ui.add_space(5.0);

        // オーディオ入力デバイス選択 - キャッシュリストを使用
        let current_input_device = settings.audio.input_device_name.clone().unwrap_or_default();

        let mut input_changed = false;
        egui::ComboBox::from_label("オーディオ入力デバイス")
            .selected_text(if current_input_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_input_device
            })
            .show_ui(ui, |ui| {
                for device_name in devices.input {
                    if ui
                        .selectable_value(
                            &mut settings.audio.input_device_name,
                            Some(device_name.clone()),
                            device_name,
                        )
                        .clicked()
                        && current_input_device != *device_name
                    {
                        input_changed = true;
                    }
                }
            });

        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings
            .audio
            .output_device_name
            .clone()
            .unwrap_or_default();

        let mut output_changed = false;
        egui::ComboBox::from_label("オーディオ出力デバイス")
            .selected_text(if current_output_device.is_empty() {
                "デフォルト"
            } else {
                &current_output_device
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut settings.audio.output_device_name, None, "デフォルト")
                    .clicked()
                    && !current_output_device.is_empty()
                {
                    output_changed = true;
                }
                for device_name in devices.output {
                    if ui
                        .selectable_value(
                            &mut settings.audio.output_device_name,
                            Some(device_name.clone()),
                            device_name,
                        )
                        .clicked()
                        && current_output_device != *device_name
                    {
                        output_changed = true;
                    }
                }
            });

        // 選択後のデバイス名から作るキャッシュのキー。ビデオ側と同じく、
        // 切り替えたフレームで切り替え前の名前を見ると 1 フレームだけ
        // 前のデバイスの選択肢が出てしまう
        let input_key = audio::cache_key(settings.audio.input_device_name.as_deref());
        let output_key = audio::cache_key(settings.audio.output_device_name.as_deref());

        if input_changed {
            audio_capabilities.input.expect_defaults(&input_key);
        }
        if output_changed {
            audio_capabilities.output.expect_defaults(&output_key);
        }

        // 対応設定の取得を要求する。列挙は別スレッドなので UI は止まらない
        audio_capabilities.input.request(&input_key);
        audio_capabilities.output.request(&output_key);

        show_audio_capability_progress(ui, audio_capabilities, &input_key, &output_key);

        // 入出力の両方が対応する値だけを選択肢にする。取得できていない側は
        // 制約にしない（片側だけ、どちらも無ければ固定の既定一覧）
        let rates = audio::selectable_sample_rates(
            audio_capabilities.input.ready(&input_key),
            audio_capabilities.output.ready(&output_key),
        );
        let channel_choices = audio::selectable_channels(
            audio_capabilities.input.ready(&input_key),
            audio_capabilities.output.ready(&output_key),
        );
        // 設定に希望値が入っていないときの手掛かり。入力デバイスの既定を採る
        // （入力が音の出どころなので、そちらへ揃えるほうが変換が減る）
        let input_defaults = audio_capabilities
            .input
            .ready(&input_key)
            .map(|caps| (caps.default_sample_rate(), caps.default_channels()));

        // デバイスを切り替えたあとに能力が届いたら、対応する値へ寄せ直す。
        // **`|` で書いて両方を必ず評価する。** `||` だと入力側が真のときに
        // 出力側の目印が消えず、次のフレームでもう一度寄せ直してしまう
        let repick = audio_capabilities.input.should_apply_defaults(&input_key)
            | audio_capabilities.output.should_apply_defaults(&output_key);
        if repick {
            let desired_rate = settings
                .audio
                .sample_rate
                .or(input_defaults.map(|(rate, _)| rate))
                .unwrap_or(DEFAULT_SAMPLE_RATE);
            if let Some(rate) = audio::nearest_sample_rate(&rates.values, desired_rate) {
                if settings.audio.sample_rate != Some(rate) {
                    debug!("オーディオデバイスの切り替えでサンプリングレートを {} Hz にした", rate);
                }
                settings.audio.sample_rate = Some(rate);
            }
            let desired_channels = settings
                .audio
                .channels
                .or(input_defaults.map(|(_, channels)| channels))
                .unwrap_or(DEFAULT_CHANNELS);
            if let Some(channels) = audio::nearest_channels(&channel_choices.values, desired_channels)
            {
                if settings.audio.channels != Some(channels) {
                    debug!("オーディオデバイスの切り替えでチャンネル数を {} ch にした", channels);
                }
                settings.audio.channels = Some(channels);
            }
        }

        // 選択肢が 1 つしか無い値は、デバイスを切り替えていなくてもそこへ寄せる。
        //
        // **`repick` の目印はデバイスを選び直したときにしか立たない。** 設定ファイルに
        // 古い値が残ったまま（Windows 側で既定デバイスの形式を変えた、設定ファイルを
        // 手で書き換えた）起動すると、選べる値が 1 つしか無いのに違う値が残る。
        // チャンネル数のコンボは 1 択のとき操作できないので、ユーザーが直す手段が無い
        if let [only] = rates.values[..] {
            if settings.audio.sample_rate != Some(only) {
                debug!("サンプリングレートの選択肢が 1 つなので {} Hz に寄せた", only);
                settings.audio.sample_rate = Some(only);
            }
        }
        if let [only] = channel_choices.values[..] {
            if settings.audio.channels != Some(only) {
                debug!("チャンネル数の選択肢が 1 つなので {} ch に寄せた", only);
                settings.audio.channels = Some(only);
            }
        }

        // サンプルレート
        ui.horizontal(|ui| {
            ui.label("サンプリングレート:");
            let current_rate = settings.audio.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
            egui::ComboBox::from_id_source("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate))
                .show_ui(ui, |ui| {
                    for rate in &rates.values {
                        ui.selectable_value(
                            &mut settings.audio.sample_rate,
                            Some(*rate),
                            format!("{} Hz", rate),
                        );
                    }
                });
        });
        show_choice_note(ui, rates.source, "サンプリングレート");
        // 設定ファイルを手で書き換えた場合など、選択肢に無い値が残ることがある。
        // 黙って別の値で開くと「選んだ値と違う」理由が分からない
        if let Some(note) = out_of_range_note(
            &rates.values,
            settings.audio.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE),
            " Hz",
        ) {
            ui.colored_label(egui::Color32::YELLOW, note);
        }

        // チャンネル数
        let single_channel_choice = channel_choices.values.len() == 1;
        ui.horizontal(|ui| {
            ui.label("チャンネル数:");
            let current_channels = settings.audio.channels.unwrap_or(DEFAULT_CHANNELS);
            // 選択肢が 1 つしか無いときは操作させない。開ける値が 1 つなのに
            // 選べると、選んだ値と実際の値が食い違う
            ui.add_enabled_ui(!single_channel_choice, |ui| {
                egui::ComboBox::from_id_source("channels_combo")
                    .selected_text(channel_label(current_channels))
                    .show_ui(ui, |ui| {
                        for channels in &channel_choices.values {
                            ui.selectable_value(
                                &mut settings.audio.channels,
                                Some(*channels),
                                channel_label(*channels),
                            );
                        }
                    });
            });
        });
        if single_channel_choice && channel_choices.source != ChoiceSource::Fallback {
            // WASAPI は共有モードのミックスフォーマットしか列挙しないため、
            // Windows では実質ここに落ちる
            ui.label("このデバイスの組み合わせでは 1 つしか選べません（Windows の共有モードではデバイスのミックスフォーマットに固定されます）");
        }
        show_choice_note(ui, channel_choices.source, "チャンネル数");
        let channel_values: Vec<u32> = channel_choices
            .values
            .iter()
            .map(|&channels| u32::from(channels))
            .collect();
        if let Some(note) = out_of_range_note(
            &channel_values,
            u32::from(settings.audio.channels.unwrap_or(DEFAULT_CHANNELS)),
            " ch",
        ) {
            ui.colored_label(egui::Color32::YELLOW, note);
        }

        ui.add_space(10.0);

        // オーディオパススルー制御
        ui.horizontal(|ui| {
            ui.label("音声パススルー:");
            if ui
                .checkbox(&mut settings.audio.passthrough_enabled, "有効")
                .changed()
            {
                // ここで書き換わるのはドラフト。実設定へ反映されるのは「適用」または「OK」のとき
                debug!(
                    "設定ダイアログで音声パススルーを {} に変更した",
                    settings.audio.passthrough_enabled
                );
            }
        });

        if !settings.audio.passthrough_enabled {
            ui.colored_label(
                egui::Color32::YELLOW,
                "⚠ 音声パススルーが無効です（音は出力されません）",
            );
        }
    });

    ui.add_space(15.0);

    // UI設定
    ui.group(|ui| {
        ui.strong("ユーザーインターフェース");
        ui.add_space(5.0);

        ui.checkbox(&mut settings.ui.maintain_aspect_ratio, "アスペクト比を維持");

        ui.horizontal(|ui| {
            ui.label("初期音量:");
            ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%"));
        });
    });
}

/// 「その他」タブを描画し、押されたボタンを返す。
///
/// 設定の書き出し・読み込み・初期化を置いてある。**ここでは何も実行しない。**
/// ファイルダイアログもファイル I/O も `CaptureCardViewer` が行う
/// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
///
/// タブに分けてあるのは、下部の「OK / キャンセル / 適用」の並びへ足すと
/// 「初期化」が「OK」の隣に来るため。押し間違いで設定が消える並びにしない。
/// 読み込みと初期化が「適用」を押すまで反映されないことの説明も、
/// ボタンの真下に書けるほうが伝わる。
fn show_other_tab(
    ui: &mut egui::Ui,
    message: Option<&ManagementMessage>,
    reset_confirm: &mut bool,
) -> SettingsDialogAction {
    ui.heading("その他");
    ui.add_space(10.0);

    let mut action = SettingsDialogAction::None;

    ui.group(|ui| {
        ui.strong("設定ファイル");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            if ui.button("設定を書き出す...").clicked() {
                action = SettingsDialogAction::ExportSettings;
            }
            if ui.button("設定を読み込む...").clicked() {
                action = SettingsDialogAction::ImportSettings;
            }
        });

        ui.add_space(5.0);
        ui.small("書き出すのは実行中の設定です。編集中の内容を含めたい場合は、先に「適用」を押してください。");
        ui.small("読み込んだ内容は編集中の設定に入ります。「適用」か「OK」を押すまで反映されません。");
        ui.small("ウィンドウの位置とサイズは読み込みません。別の画面構成で書き出したファイルを読んでも、ウィンドウは動きません。");
    });

    ui.add_space(15.0);

    ui.group(|ui| {
        ui.strong("初期化");
        ui.add_space(5.0);

        if *reset_confirm {
            ui.colored_label(
                egui::Color32::YELLOW,
                "⚠ 編集中の設定を初期値に戻します。よろしいですか？",
            );
            ui.horizontal(|ui| {
                if ui.button("初期化する").clicked() {
                    action = SettingsDialogAction::ResetDraft;
                    *reset_confirm = false;
                }
                if ui.button("やめる").clicked() {
                    *reset_confirm = false;
                }
            });
        } else if ui.button("設定を初期化...").clicked() {
            *reset_confirm = true;
        }

        ui.add_space(5.0);
        ui.small(
            "初期化も編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。",
        );
        ui.small("戻る範囲は読み込みと同じです。ウィンドウの位置とサイズ、右クリックメニューで切り替える項目は初期化しません。");
    });

    if let Some(message) = message {
        ui.add_space(15.0);
        ui.separator();
        let color = if message.is_error {
            egui::Color32::LIGHT_RED
        } else {
            egui::Color32::LIGHT_GREEN
        };
        ui.colored_label(color, &message.text);
    }

    action
}

/// チャンネル数の表示名。
fn channel_label(channels: u16) -> String {
    match channels {
        1 => "1（モノラル）".to_string(),
        2 => "2（ステレオ）".to_string(),
        other => format!("{} ch", other),
    }
}

/// 現在の設定値が選択肢に無いときに出す注意書き。選択肢にあれば `None`。
///
/// **実際に使われる値を併記する。** 黙って別の値で開くと、設定画面の表示と
/// 「接続状態」タブの値が食い違う理由がユーザーに分からない。寄せ先は
/// `audio::select_best_config` と同じ「最も近い値」で、同点なら小さいほう。
///
/// `values` が空のときは何も出さない。選択肢を作れていない状況なので、
/// どの値へ寄るかをここで断定できない。
pub fn out_of_range_note(values: &[u32], current: u32, unit: &str) -> Option<String> {
    if values.is_empty() || values.contains(&current) {
        return None;
    }
    let nearest = values.iter().copied().min_by_key(|v| v.abs_diff(current))?;
    Some(format!(
        "⚠ {current}{unit} はこの組み合わせでは使えません。最も近い {nearest}{unit} で開きます"
    ))
}

/// 選択肢の出どころに応じた説明を添える。共通部分から作れているときは何も出さない。
fn show_choice_note(ui: &mut egui::Ui, source: ChoiceSource, label: &str) {
    match source {
        // 入出力の両方が対応する値だけが並んでいる。説明は要らない
        ChoiceSource::Common => {}
        ChoiceSource::OneSided => {
            ui.label(format!(
                "{}の選択肢は、対応設定を取得できた側のデバイスだけから作っています",
                label
            ));
        }
        ChoiceSource::Disjoint => {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "⚠ 入力と出力で共通の{}がありません。それぞれ最も近い値で開き、変換して出力します（音質がわずかに落ちます）",
                    label
                ),
            );
        }
        ChoiceSource::Fallback => {
            ui.label(format!(
                "{}の選択肢は既定の一覧です（デバイスの対応設定を取得できていません）",
                label
            ));
        }
    }
}

/// オーディオデバイスの対応設定の取得状況を描く。
///
/// 取得中はスピナー、失敗したら理由と「再取得」ボタン。ビデオ側と同じ扱いで、
/// 黙って既定の一覧を出すと選択肢が実態と違う理由が分からない。
fn show_audio_capability_progress(
    ui: &mut egui::Ui,
    caches: &mut AudioCapabilityCaches<'_>,
    input_key: &str,
    output_key: &str,
) {
    let retry_input = show_audio_capability_state(ui, caches.input.state(input_key), "入力");
    let retry_output = show_audio_capability_state(ui, caches.output.state(output_key), "出力");

    if retry_input {
        caches.input.retry(input_key);
    }
    if retry_output {
        caches.output.retry(output_key);
    }
}

/// 片方向ぶんの取得状況を描く。「再取得」が押されたら `true`。
fn show_audio_capability_state(
    ui: &mut egui::Ui,
    state: Option<&CapabilityState<AudioCapabilities>>,
    label: &str,
) -> bool {
    let mut retry_requested = false;
    match state {
        Some(CapabilityState::Pending) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("{}デバイスの対応設定を取得中...", label));
            });
        }
        Some(CapabilityState::Failed(reason)) => {
            ui.horizontal(|ui| {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!("⚠ {}デバイスの対応設定を取得できません: {}", label, reason),
                );
                if ui.button("再取得").clicked() {
                    retry_requested = true;
                }
            });
        }
        _ => {}
    }
    retry_requested
}

/// 接続状態タブを描画する。
///
/// **ここでは何も編集しない。** 映像と音声が実際に何へ繋がっているかと、
/// 直近の失敗を読むためのタブで、値は呼び出し側が複製して渡す
/// （描画中に `video_capture` / `audio_capture` のロックを取らないため）。
fn show_status_tab(ui: &mut egui::Ui, connection: &ConnectionStatus) {
    ui.heading("接続状態");
    ui.add_space(10.0);

    show_link_status(ui, "映像", &connection.video);
    ui.add_space(15.0);
    show_link_status(ui, "音声", &connection.audio);

    ui.add_space(15.0);
    ui.label("この内容は表示だけで、「適用」や「OK」では変わりません。");
    ui.label("詳しい経過はログファイルに残っています（%AppData%\\capturecard_viewer\\logs）。");
}

/// 映像か音声、片方の接続状態を 1 つの枠に描く。
fn show_link_status(ui: &mut egui::Ui, title: &str, status: &LinkStatus) {
    ui.group(|ui| {
        ui.strong(title);
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("状態:");
            if status.connected {
                ui.colored_label(egui::Color32::LIGHT_GREEN, status.headline());
            } else {
                ui.colored_label(egui::Color32::YELLOW, status.headline());
            }
        });

        for line in &status.details {
            ui.label(line);
        }

        // 繋がっている間は再試行していないので、回数を出しても 0 が並ぶだけ
        if !status.connected && status.attempts > 0 {
            ui.label(format!("連続失敗: {} 回", status.attempts));
        }

        match &status.error {
            Some((message, time)) => {
                // 長いエラー文でダイアログの幅が広がらないよう折り返す
                ui.colored_label(egui::Color32::YELLOW, format!("⚠ {}", message));
                ui.label(format!("発生時刻: {}", time));
            }
            None => {
                ui.label("直近のエラー: なし");
            }
        }
    });
}

/// スクリーンショット設定タブを描画し、「テスト再生」が押されたかを返す。
///
/// 効果音の再生はダイアログの仕事ではないので、ここでは鳴らさずに
/// イベントとして上へ返す（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
fn show_screenshot_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    show_hotkey_dialog: &mut bool,
    capture: &mut HotkeyCaptureState,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyError>,
) -> bool {
    ui.heading("スクリーンショット設定");
    ui.add_space(10.0);

    let mut test_sound_requested = false;

    // 出力先
    ui.group(|ui| {
        ui.strong("出力先");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::File,
                "ファイルに保存",
            );
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::Clipboard,
                "クリップボードにコピー",
            );
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::Both,
                "両方",
            );
        });

        ui.add_space(5.0);
        ui.small(
            "クリップボードへは圧縮せずそのままの画をコピーします。
             保存場所と保存形式は、ファイルに保存するときだけ使われます。",
        );
    });

    ui.add_space(15.0);

    // 保存場所と保存形式はファイルへ出すときだけ効く。クリップボードだけを
    // 選んでいるときは触れないようにして、変えても何も起きない項目を操作させない
    // （JPEG 品質を PNG のときに無効にしているのと同じ考え方）
    let saves_file = settings.screenshot.destination.saves_file();

    // 保存フォルダー
    ui.add_enabled_ui(saves_file, |ui| {
        ui.group(|ui| {
            ui.strong("保存場所");
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.label("保存フォルダ:");
                let mut folder_str = settings
                    .screenshot
                    .save_folder
                    .to_string_lossy()
                    .to_string();
                ui.text_edit_singleline(&mut folder_str);
                settings.screenshot.save_folder = std::path::PathBuf::from(folder_str);

                if ui.button("参照...").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        settings.screenshot.save_folder = folder;
                    }
                }
            });
        });

        ui.add_space(15.0);

        // 保存形式
        ui.group(|ui| {
            ui.strong("保存形式");
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut settings.screenshot.format,
                    ScreenshotFormat::Jpeg,
                    "JPEG (.jpg)",
                );
                ui.radio_value(
                    &mut settings.screenshot.format,
                    ScreenshotFormat::Png,
                    "PNG (.png)",
                );
            });

            // 品質は JPEG のときだけ効く。PNG では触れないようにして、
            // 変えても何も起きない項目を操作させない
            let jpeg_selected = settings.screenshot.format == ScreenshotFormat::Jpeg;
            ui.horizontal(|ui| {
                ui.label("JPEG 品質:");
                ui.add_enabled(
                    jpeg_selected,
                    egui::Slider::new(
                        &mut settings.screenshot.jpeg_quality,
                        MIN_JPEG_QUALITY..=MAX_JPEG_QUALITY,
                    ),
                );
            });

            ui.add_space(5.0);
            ui.small(
                "JPEG はファイルが小さくなりますが、文字や細い線ににじみが出ます。
                 PNG は元の画をそのまま保存できるかわりに、ファイルが数倍の大きさになります。",
            );
        });
    });

    ui.add_space(15.0);

    // サウンド設定
    ui.group(|ui| {
        ui.strong("効果音");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("サウンドファイル:");
            let sound_file_str = settings
                .screenshot
                .sound_file
                .as_ref()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| "未選択".to_string());
            ui.label(&sound_file_str);

            if ui.button("ファイル選択...").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("音声ファイル", &["mp3", "wav", "ogg"])
                    .pick_file()
                {
                    settings.screenshot.sound_file = Some(file);
                }
            }
        });

        if settings.screenshot.sound_file.is_some() {
            ui.horizontal(|ui| {
                ui.label("音量:");
                ui.add(
                    egui::Slider::new(&mut settings.screenshot.sound_volume, 0.0..=200.0)
                        .suffix("%"),
                );
            });

            ui.horizontal(|ui| {
                if ui.button("テスト再生").clicked() {
                    test_sound_requested = true;
                }
                if ui.button("クリア").clicked() {
                    settings.screenshot.sound_file = None;
                }
            });
        }
    });

    ui.add_space(15.0);

    // ホットキー設定
    show_hotkey_assignments(ui, settings, show_hotkey_dialog, capture, hotkey_errors);

    test_sound_requested
}

/// アクションごとのホットキー割り当ての一覧を描く。
///
/// `hotkey_errors` は**実行中の設定**で登録できなかったもの。ドラフトの
/// 内容ではないので、割り当てを変えても「適用」を押すまで消えない。
fn show_hotkey_assignments(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    show_hotkey_dialog: &mut bool,
    capture: &mut HotkeyCaptureState,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyError>,
) {
    ui.group(|ui| {
        ui.strong("ホットキー");
        ui.add_space(5.0);
        ui.small(
            "他のアプリを操作している間も効きます。スクリーンショット以外の操作にも割り当てられます。",
        );
        ui.add_space(8.0);

        let duplicates = duplicate_hotkey_actions(&settings.hotkeys);
        // 一覧を描いている間は settings を読むだけにして、書き換えは
        // 描き終えてから行う（同じデータを読みながら書き換えないため）
        let mut clear_requested: Option<HotkeyAction> = None;

        egui::Grid::new("hotkey_assignments")
            .num_columns(4)
            .spacing([8.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for action in HotkeyAction::ALL {
                    ui.label(action.label());

                    match settings.hotkey(action) {
                        Some(hotkey) => {
                            let text = egui::RichText::new(hotkey).monospace();
                            if duplicates.contains(&action) {
                                ui.label(text.color(egui::Color32::YELLOW));
                            } else {
                                ui.label(text);
                            }
                        }
                        None => {
                            ui.weak("未設定");
                        }
                    }

                    if ui.button("設定...").clicked() {
                        // どのアクションを編集しているかを入力ダイアログへ渡す
                        capture.begin_for(action);
                        *show_hotkey_dialog = true;
                    }

                    let can_clear = settings.hotkey(action).is_some();
                    if ui
                        .add_enabled(can_clear, egui::Button::new("クリア"))
                        .clicked()
                    {
                        clear_requested = Some(action);
                    }

                    ui.end_row();
                }
            });

        if let Some(action) = clear_requested {
            debug!("{} のホットキーをクリアする", action.label());
            settings.set_hotkey(action, None);
        }

        if !duplicates.is_empty() {
            let names: Vec<&str> = duplicates.iter().map(|action| action.label()).collect();
            ui.add_space(5.0);
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "注意: 同じキーが複数のアクションに割り当てられています（{}）。適用しても、上にある側だけが有効になります。",
                    names.join("、")
                ),
            );
        }

        // 登録に失敗したものを、理由とともに出す。トーストは気付かせるための
        // もので流れて消えるため、どのアクションが失敗しているかはここで見る。
        // 見出しは status.rs の定型文をそのまま使い、通知と表現を揃える
        if !hotkey_errors.is_empty() {
            ui.add_space(5.0);
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("{}:", ErrorSource::Hotkey.headline()),
            );
            for (action, error) in hotkey_errors {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("{}（{}）— {}", action.label(), error.hotkey, error.message),
                );
            }
        }
    });
}

/// 同じキーが 2 つ以上のアクションに割り当てられているものを返す。
///
/// 比較は表記のゆれを吸収する。`"Ctrl+S"` と `"ctrl + s"`、`"Shift+Ctrl+S"` は
/// どれも同じ `HotKey` になるため、文字列のまま比べると重複を見逃す。
pub fn duplicate_hotkey_actions(
    hotkeys: &BTreeMap<HotkeyAction, String>,
) -> BTreeSet<HotkeyAction> {
    let mut seen: HashMap<String, Vec<HotkeyAction>> = HashMap::new();
    for (action, hotkey) in hotkeys {
        seen.entry(normalize_hotkey(hotkey))
            .or_default()
            .push(*action);
    }

    seen.into_values()
        .filter(|actions| actions.len() > 1)
        .flatten()
        .collect()
}

/// ホットキー文字列を、同じキーの組み合わせなら同じになる形へ正規化する。
///
/// 大文字小文字と空白を落とし、`+` で分けた要素を並べ替える。
/// `hotkey::parse_hotkey` が修飾キーの順序を問わないことに合わせてある。
fn normalize_hotkey(hotkey: &str) -> String {
    let mut parts: Vec<String> = hotkey
        .split('+')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    parts.sort();
    parts.join("+")
}

/// egui のキーを、ホットキー文字列で使う名前に変換する。
/// ホットキーとして扱わないキーは `None` を返す。
fn hotkey_key_name(key: egui::Key) -> Option<&'static str> {
    let name = match key {
        egui::Key::A => "A",
        egui::Key::B => "B",
        egui::Key::C => "C",
        egui::Key::D => "D",
        egui::Key::E => "E",
        egui::Key::F => "F",
        egui::Key::G => "G",
        egui::Key::H => "H",
        egui::Key::I => "I",
        egui::Key::J => "J",
        egui::Key::K => "K",
        egui::Key::L => "L",
        egui::Key::M => "M",
        egui::Key::N => "N",
        egui::Key::O => "O",
        egui::Key::P => "P",
        egui::Key::Q => "Q",
        egui::Key::R => "R",
        egui::Key::S => "S",
        egui::Key::T => "T",
        egui::Key::U => "U",
        egui::Key::V => "V",
        egui::Key::W => "W",
        egui::Key::X => "X",
        egui::Key::Y => "Y",
        egui::Key::Z => "Z",
        egui::Key::F1 => "F1",
        egui::Key::F2 => "F2",
        egui::Key::F3 => "F3",
        egui::Key::F4 => "F4",
        egui::Key::F5 => "F5",
        egui::Key::F6 => "F6",
        egui::Key::F7 => "F7",
        egui::Key::F8 => "F8",
        egui::Key::F9 => "F9",
        egui::Key::F10 => "F10",
        egui::Key::F11 => "F11",
        egui::Key::F12 => "F12",
        egui::Key::Num0 => "0",
        egui::Key::Num1 => "1",
        egui::Key::Num2 => "2",
        egui::Key::Num3 => "3",
        egui::Key::Num4 => "4",
        egui::Key::Num5 => "5",
        egui::Key::Num6 => "6",
        egui::Key::Num7 => "7",
        egui::Key::Num8 => "8",
        egui::Key::Num9 => "9",
        egui::Key::Space => "Space",
        egui::Key::Enter => "Enter",
        _ => return None,
    };
    Some(name)
}

/// 押されている修飾キーと通常キーから、`screenshot::parse_hotkey` が解釈できる
/// ホットキー文字列を組み立てる。
///
/// 通常キーが 1 つも押されていない（修飾キーだけの）場合は `None` を返す。
fn build_hotkey_string(modifiers: &egui::Modifiers, keys_down: &[egui::Key]) -> Option<String> {
    // 通常キーが 1 つも無いうちは確定させない。修飾キーだけの文字列を確定させると
    // screenshot::parse_hotkey が "No key code specified" で弾き、登録に失敗する。
    // 押されているキーのうち対応している最初の 1 つだけを使う（ホットキーに含められる
    // 通常キーは 1 つだけのため）。
    let key_name = keys_down.iter().copied().find_map(hotkey_key_name)?;

    let mut parts = Vec::new();

    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    parts.push(key_name);

    Some(parts.join("+"))
}

/// ホットキー入力ダイアログの結果。
///
/// 「クリア」を `Captured` と区別できるようにしてある。以前は
/// 「確定したか」の `bool` だけを返しており、クリアしても呼び出し側は
/// 何も受け取れず、設定のホットキーが `None` にならなかった。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyDialogOutcome {
    /// 何も確定していない。開いたまま、キャンセル、× で閉じた場合
    None,
    /// `captured_hotkey` の値で確定した
    Captured,
    /// クリアされた。ホットキーを未設定にする
    Cleared,
}

/// ホットキー入力ダイアログを描画する。
///
/// `action` は編集対象のアクション、`captured_hotkey` は呼び出し側が持つ
/// 確定済みのホットキー、`capture` は入力待機中の一時状態。
/// いずれも呼び出し側が保持する。
pub fn show_hotkey_capture_dialog(
    ctx: &egui::Context,
    show_dialog: &mut bool,
    action: HotkeyAction,
    captured_hotkey: &mut String,
    capture: &mut HotkeyCaptureState,
) -> HotkeyDialogOutcome {
    let mut close_dialog = false;
    let mut outcome = HotkeyDialogOutcome::None;

    egui::Window::new("ホットキー設定")
        .open(show_dialog)
        .fixed_size([350.0, 200.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(format!("ホットキー設定: {}", action.label()));
                ui.add_space(10.0);

                if !capture.is_capturing() {
                    ui.label(format!(
                        "『キャプチャ開始』を押して「{}」に割り当てるキーを入力してください",
                        action.label()
                    ));

                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        ui.label("現在のホットキー:");
                        let hotkey_text = if captured_hotkey.is_empty() {
                            "未設定"
                        } else {
                            captured_hotkey.as_str()
                        };
                        ui.monospace(hotkey_text);
                    });

                    ui.add_space(15.0);

                    if ui.button("キャプチャ開始").clicked() {
                        capture.start();
                    }
                } else {
                    ui.colored_label(egui::Color32::YELLOW, "キー入力待機中...");
                    ui.label("任意のキーコンビネーションを押してください");

                    // キーボード入力をキャプチャ
                    ctx.input(|i| {
                        // HashSet の反復順は不定なので、同じ組み合わせから常に同じ
                        // ホットキー文字列が得られるよう並べてから渡す
                        let mut keys_down: Vec<egui::Key> = i.keys_down.iter().copied().collect();
                        keys_down.sort();

                        if let Some(hotkey) = build_hotkey_string(&i.modifiers, &keys_down) {
                            capture.finish(hotkey);
                        }
                    });

                    if !capture.temp().is_empty() {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label("取得:");
                            ui.monospace(capture.temp());
                        });
                    }

                    ui.add_space(10.0);

                    if ui.button("停止").clicked() {
                        capture.stop();
                    }
                }

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        // 何も取れていなければ現在のホットキーをそのまま残す
                        if let Some(hotkey) = capture.take_captured() {
                            *captured_hotkey = hotkey;
                        }
                        // 取り直していなくても確定として返す。呼び出し側は
                        // 同じ値なら登録し直さないので、二重登録にはならない
                        if !captured_hotkey.is_empty() {
                            outcome = HotkeyDialogOutcome::Captured;
                        }
                        close_dialog = true;
                    }

                    if ui.button("キャンセル").clicked() {
                        capture.reset();
                        close_dialog = true;
                    }

                    if ui.button("クリア").clicked() {
                        captured_hotkey.clear();
                        capture.reset();
                        // クリアしたことを呼び出し側へ伝える。伝えないと
                        // 設定のホットキーが残ったままになり、そのキーが
                        // 効き続ける
                        outcome = HotkeyDialogOutcome::Cleared;
                        close_dialog = true;
                    }
                });
            });
        });

    if close_dialog {
        *show_dialog = false;
    } else if !*show_dialog {
        // × で閉じられた場合。`egui::Window::open` が `show_dialog` を
        // false にするだけでボタンは押されないため、設定ダイアログの ×
        // と同じくキャンセル扱いにして入力中の状態を捨てる。
        // 捨てないと、次に開いたときに前回の取得結果が残ったままになり、
        // 何も入力せず OK を押しただけでそのキーが確定してしまう
        capture.reset();
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AudioSettings, ScreenshotSettings, UiSettings, VideoSettings};
    use crate::video::FormatCapability;
    use std::path::PathBuf;

    /// 既定値と全項目が異なる設定。どの項目が反映され、どの項目が
    /// 据え置かれるかを区別できるようにするためのもの。
    fn sample_settings() -> AppSettings {
        AppSettings {
            video: VideoSettings {
                device_name: Some("Capture Device".to_string()),
                resolution: Some((1920, 1080)),
                format: Some("MJPEG".to_string()),
                fps: Some(30),
                // 既定値（true）と異なる値にして、反映の有無を見分けられるようにする
                auto_reconnect: false,
                // 色空間とレンジも既定値と異なる値にしておく
                color_space: ColorSpace::Bt709,
                color_range: ColorRange::Full,
            },
            audio: AudioSettings {
                input_device_name: Some("Line In".to_string()),
                output_device_name: Some("Speakers".to_string()),
                sample_rate: Some(44100),
                channels: Some(1),
                passthrough_enabled: false,
            },
            screenshot: ScreenshotSettings {
                destination: ScreenshotDestination::Both,
                save_folder: PathBuf::from("C:/shots"),
                format: ScreenshotFormat::Png,
                jpeg_quality: 60,
                sound_file: Some(PathBuf::from("sound/custom.mp3")),
                sound_volume: 50.0,
                legacy_hotkey: None,
            },
            ui: UiSettings {
                volume: 80.0,
                muted: false,
                maintain_aspect_ratio: false,
                last_window_size: Some((800.0, 600.0)),
                last_window_pos: Some((10.0, 20.0)),
                always_on_top: true,
                enable_drag_move: false,
                show_stats_overlay: true,
                borderless: false,
            },
            hotkeys: BTreeMap::from([
                (HotkeyAction::Screenshot, "Ctrl+S".to_string()),
                (HotkeyAction::ToggleFullscreen, "F11".to_string()),
            ]),
        }
    }

    // ---- ホットキーの重複判定 ----

    fn hotkeys(pairs: &[(HotkeyAction, &str)]) -> BTreeMap<HotkeyAction, String> {
        pairs
            .iter()
            .map(|(action, key)| (*action, (*key).to_string()))
            .collect()
    }

    #[test]
    fn duplicate_hotkey_actions_without_duplicates_is_empty() {
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::ToggleFullscreen, "F6"),
        ]);

        assert!(duplicate_hotkey_actions(&assigned).is_empty());
    }

    #[test]
    fn duplicate_hotkey_actions_reports_both_sides() {
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::ToggleFullscreen, "F6"),
            (HotkeyAction::VolumeUp, "F5"),
        ]);

        let duplicates = duplicate_hotkey_actions(&assigned);

        assert_eq!(
            duplicates.into_iter().collect::<Vec<_>>(),
            vec![HotkeyAction::Screenshot, HotkeyAction::VolumeUp]
        );
    }

    #[test]
    fn duplicate_hotkey_actions_ignores_case_and_spaces() {
        // 同じ HotKey になる書き方は重複として扱う。文字列のまま比べると
        // 見逃して、登録の段階で片方が黙って無効になる
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "Ctrl+S"),
            (HotkeyAction::VolumeUp, " ctrl + s "),
        ]);

        assert_eq!(duplicate_hotkey_actions(&assigned).len(), 2);
    }

    #[test]
    fn duplicate_hotkey_actions_ignores_modifier_order() {
        // parse_hotkey は修飾キーの順序を問わないので、判定も揃える
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "Ctrl+Shift+A"),
            (HotkeyAction::VolumeDown, "Shift+Ctrl+A"),
        ]);

        assert_eq!(duplicate_hotkey_actions(&assigned).len(), 2);
    }

    #[test]
    fn duplicate_hotkey_actions_empty_assignment_is_empty() {
        assert!(duplicate_hotkey_actions(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn normalize_hotkey_same_combination_gives_same_string() {
        assert_eq!(normalize_hotkey("Ctrl+S"), normalize_hotkey("ctrl+s"));
        assert_eq!(
            normalize_hotkey("Ctrl+Shift+A"),
            normalize_hotkey("shift+ctrl+a")
        );
        assert_ne!(normalize_hotkey("Ctrl+S"), normalize_hotkey("Ctrl+A"));
        assert_ne!(normalize_hotkey("Ctrl+S"), normalize_hotkey("Alt+S"));
    }

    // ---- ホットキー入力ダイアログの編集対象 ----

    #[test]
    fn hotkey_capture_begin_for_records_the_action() {
        let mut capture = HotkeyCaptureState::default();

        capture.begin_for(HotkeyAction::VolumeUp);

        assert_eq!(capture.editing(), Some(HotkeyAction::VolumeUp));
        assert!(!capture.is_capturing());
        assert!(capture.temp().is_empty());
    }

    #[test]
    fn hotkey_capture_begin_for_discards_the_previous_input() {
        // 別のアクションを編集し始めたときに、前のアクションで取得した
        // キーが残っていると、そのまま OK を押しただけで確定してしまう
        let mut capture = HotkeyCaptureState::default();
        capture.begin_for(HotkeyAction::Screenshot);
        capture.finish("Ctrl+S".to_string());

        capture.begin_for(HotkeyAction::VolumeDown);

        assert!(capture.temp().is_empty());
        assert_eq!(capture.editing(), Some(HotkeyAction::VolumeDown));
    }

    #[test]
    fn hotkey_capture_reset_keeps_the_editing_action() {
        // 「クリア」やキャンセルで閉じたあとも、呼び出し側がどのアクションを
        // 未設定にすればよいか分かる必要がある
        let mut capture = HotkeyCaptureState::default();
        capture.begin_for(HotkeyAction::ReconnectDevices);
        capture.finish("F9".to_string());

        capture.reset();

        assert_eq!(capture.editing(), Some(HotkeyAction::ReconnectDevices));
        assert!(capture.temp().is_empty());
    }

    #[test]
    fn transition_for_none_does_nothing() {
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::None);
        assert!(!transition.commit_draft);
        assert!(!transition.save_to_file);
        assert!(!transition.close);
    }

    #[test]
    fn transition_for_apply_commits_and_saves_without_closing() {
        // 適用 = 反映してファイルへ保存する。閉じないところだけが OK と違う
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        assert!(transition.commit_draft);
        assert!(transition.save_to_file);
        assert!(!transition.close);
    }

    #[test]
    fn transition_for_apply_and_ok_differ_only_in_closing() {
        // 「適用」と「OK」の違いは閉じるかどうかだけにする。
        // 保存の有無で分けると「適用したのに再起動で戻る」が起きる
        let apply = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        let ok = SettingsDialogState::transition_for(SettingsDialogAction::Ok);
        assert_eq!(apply.commit_draft, ok.commit_draft);
        assert_eq!(apply.save_to_file, ok.save_to_file);
        assert!(!apply.close);
        assert!(ok.close);
    }

    #[test]
    fn transition_for_ok_commits_saves_and_closes() {
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Ok);
        assert!(transition.commit_draft);
        assert!(transition.save_to_file);
        assert!(transition.close);
    }

    #[test]
    fn transition_for_cancel_discards_without_committing() {
        // キャンセル = ドラフトを捨てて閉じるだけ。適用済みの分は戻さない
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::Cancel);
        assert!(!transition.commit_draft);
        assert!(!transition.save_to_file);
        assert!(transition.close);
    }

    #[test]
    fn transition_for_test_sound_changes_nothing() {
        // テスト再生は効果音を鳴らすだけ。ドラフトの反映も保存もクローズもしない
        let transition = SettingsDialogState::transition_for(SettingsDialogAction::TestSound);
        assert!(!transition.commit_draft);
        assert!(!transition.save_to_file);
        assert!(!transition.close);
    }

    #[test]
    fn transition_for_settings_file_actions_change_nothing() {
        // 書き出し・読み込み・初期化はダイアログの外側が別に処理する。
        // ここで save_to_file を立てると %AppData% の設定ファイルまで
        // 書き換わり、読み込んだだけで取り消せなくなる
        for action in [
            SettingsDialogAction::ExportSettings,
            SettingsDialogAction::ImportSettings,
            SettingsDialogAction::ResetDraft,
        ] {
            let transition = SettingsDialogState::transition_for(action);
            assert!(
                !transition.commit_draft,
                "{:?} が反映を要求している",
                action
            );
            assert!(
                !transition.save_to_file,
                "{:?} が保存を要求している",
                action
            );
            assert!(!transition.close, "{:?} がクローズを要求している", action);
        }
    }

    #[test]
    fn draft_from_imported_takes_device_and_screenshot_sections() {
        let imported = sample_settings();
        let current = AppSettings::default();

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.video.device_name, imported.video.device_name);
        assert_eq!(draft.video.resolution, imported.video.resolution);
        assert_eq!(draft.video.auto_reconnect, imported.video.auto_reconnect);
        assert_eq!(draft.audio.sample_rate, imported.audio.sample_rate);
        assert_eq!(draft.screenshot.format, imported.screenshot.format);
        assert_eq!(draft.hotkeys, imported.hotkeys);
    }

    #[test]
    fn draft_from_imported_takes_auto_reconnect() {
        // auto_reconnect は右クリックメニューで切り替えるが、commit_draft が
        // 「ドラフトで変わったときだけ反映する」形なので読み込める
        let imported = sample_settings();
        let current = AppSettings::default();
        assert_ne!(imported.video.auto_reconnect, current.video.auto_reconnect);

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.video.auto_reconnect, imported.video.auto_reconnect);
    }

    #[test]
    fn draft_from_defaults_takes_the_default_auto_reconnect() {
        // 初期化も同じ。自動再接続を切っている状態から初期化すれば、
        // 既定値（オン）へ戻る
        let mut current = sample_settings();
        current.video.auto_reconnect = false;

        let draft = draft_from_defaults(&current);

        assert!(draft.video.auto_reconnect);
    }

    #[test]
    fn commit_draft_applies_auto_reconnect_changed_by_import() {
        // 読み込みで変わった auto_reconnect は実行中の設定へ届くこと。
        // commit_draft_keeps_auto_reconnect_changed_outside_dialog と対になる
        let mut target = AppSettings::default();
        let original = target.clone();
        let mut draft = target.clone();
        draft.video.auto_reconnect = !original.video.auto_reconnect;

        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.auto_reconnect, draft.video.auto_reconnect);
    }

    #[test]
    fn draft_from_imported_keeps_the_window_geometry() {
        // 別の画面構成で書き出したファイルを読んでも、ウィンドウが
        // 画面の外へ飛ばないこと
        let imported = sample_settings();
        let mut current = AppSettings::default();
        current.ui.last_window_size = Some((1280.0, 720.0));
        current.ui.last_window_pos = Some((100.0, 50.0));

        let draft = draft_from_imported(imported, &current);

        assert_eq!(draft.ui.last_window_size, Some((1280.0, 720.0)));
        assert_eq!(draft.ui.last_window_pos, Some((100.0, 50.0)));
    }

    #[test]
    fn draft_from_imported_takes_the_two_ui_items_the_dialog_edits() {
        // commit_draft が反映する 2 項目だけは読み込む。
        // 読み込んでも反映されない項目を作らないため
        let imported = sample_settings();
        let current = AppSettings::default();
        assert_ne!(imported.ui.volume, current.ui.volume);
        assert_ne!(
            imported.ui.maintain_aspect_ratio,
            current.ui.maintain_aspect_ratio
        );

        let draft = draft_from_imported(imported.clone(), &current);

        assert_eq!(draft.ui.volume, imported.ui.volume);
        assert_eq!(
            draft.ui.maintain_aspect_ratio,
            imported.ui.maintain_aspect_ratio
        );
    }

    #[test]
    fn draft_from_imported_keeps_ui_items_the_dialog_cannot_apply() {
        // always_on_top などは右クリックメニューで切り替えるもので、
        // commit_draft が反映しない。読み込んでも「適用」で消えるだけなので、
        // 最初からドラフトへ入れない
        let mut imported = sample_settings();
        imported.ui.borderless = true;
        let current = AppSettings::default();
        assert_ne!(imported.ui.always_on_top, current.ui.always_on_top);
        assert_ne!(imported.ui.borderless, current.ui.borderless);

        let draft = draft_from_imported(imported, &current);

        assert_eq!(draft.ui.always_on_top, current.ui.always_on_top);
        assert_eq!(draft.ui.enable_drag_move, current.ui.enable_drag_move);
        assert_eq!(draft.ui.show_stats_overlay, current.ui.show_stats_overlay);
        assert_eq!(draft.ui.muted, current.ui.muted);
        assert_eq!(draft.ui.borderless, current.ui.borderless);
    }

    #[test]
    fn draft_from_defaults_resets_the_settings_but_not_the_window_geometry() {
        let mut current = sample_settings();
        current.ui.last_window_size = Some((640.0, 480.0));
        current.ui.last_window_pos = Some((5.0, 6.0));
        let defaults = AppSettings::default();

        let draft = draft_from_defaults(&current);

        assert_eq!(draft.video.resolution, defaults.video.resolution);
        assert_eq!(draft.audio.sample_rate, defaults.audio.sample_rate);
        assert_eq!(draft.screenshot.format, defaults.screenshot.format);
        assert_eq!(draft.hotkeys, defaults.hotkeys);
        assert_eq!(draft.ui.volume, defaults.ui.volume);
        assert_eq!(
            draft.ui.maintain_aspect_ratio,
            defaults.ui.maintain_aspect_ratio
        );
        assert_eq!(draft.ui.last_window_size, Some((640.0, 480.0)));
        assert_eq!(draft.ui.last_window_pos, Some((5.0, 6.0)));
    }

    #[test]
    fn imported_draft_reaches_the_shared_settings_on_commit() {
        // 読み込み → 「適用」の一連。commit_draft が拾う範囲と
        // draft_from_imported が読む範囲が噛み合っていることを見る
        let mut target = AppSettings::default();
        let original = target.clone();
        let imported = sample_settings();

        let draft = draft_from_imported(imported.clone(), &original);
        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.device_name, imported.video.device_name);
        assert_eq!(target.screenshot.format, imported.screenshot.format);
        assert_eq!(target.hotkeys, imported.hotkeys);
        assert_eq!(target.ui.volume, imported.ui.volume);
        assert_eq!(
            target.ui.maintain_aspect_ratio,
            imported.ui.maintain_aspect_ratio
        );
        // ウィンドウの位置とサイズは動かない
        assert_eq!(target.ui.last_window_size, original.ui.last_window_size);
        assert_eq!(target.ui.last_window_pos, original.ui.last_window_pos);
        // 自動再接続は読み込んだ値が届く
        assert_eq!(target.video.auto_reconnect, imported.video.auto_reconnect);
        // commit_draft が反映しない項目は、ドラフトにも実行中の値が入っている。
        // 「読み込んだのに反映されない」項目が生まれていないこと
        assert_eq!(target.ui.always_on_top, original.ui.always_on_top);
        assert_eq!(draft.ui.always_on_top, original.ui.always_on_top);
    }

    #[test]
    fn reset_draft_reaches_the_shared_settings_on_commit() {
        // 初期化 →「適用」の一連。自動再接続を切っている状態から初期化すれば
        // 既定値（オン）へ戻り、ドラフトと実行中の設定が食い違わないこと
        let mut target = sample_settings();
        target.video.auto_reconnect = false;
        let original = target.clone();
        let defaults = AppSettings::default();

        let draft = draft_from_defaults(&original);
        commit_draft(&mut target, &draft, &original);

        assert_eq!(target.video.resolution, defaults.video.resolution);
        assert_eq!(target.hotkeys, defaults.hotkeys);
        assert_eq!(target.ui.volume, defaults.ui.volume);
        assert_eq!(target.video.auto_reconnect, defaults.video.auto_reconnect);
        assert_eq!(draft.video.auto_reconnect, defaults.video.auto_reconnect);
        assert_eq!(target.ui.last_window_size, original.ui.last_window_size);
        assert_eq!(target.ui.last_window_pos, original.ui.last_window_pos);
    }

    #[test]
    fn settings_dialog_state_end_edit_drops_the_management_message() {
        // 開き直したダイアログに「読み込みました」が残らないこと
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());
        state.set_management_message("読み込みました".to_string(), false);

        state.end_edit();

        assert!(state.management_message.is_none());
    }

    #[test]
    fn settings_dialog_state_begin_edit_drops_the_management_message() {
        let mut state = SettingsDialogState::default();
        state.set_management_message("失敗しました".to_string(), true);

        state.begin_edit(&AppSettings::default());

        assert!(state.management_message.is_none());
    }

    #[test]
    fn settings_dialog_state_management_message_keeps_the_error_flag() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());

        state.set_management_message("読み込めません".to_string(), true);

        let message = state
            .management_message
            .as_ref()
            .expect("メッセージがあること");
        assert_eq!(message.text, "読み込めません");
        assert!(message.is_error);
    }

    #[test]
    fn settings_dialog_state_clear_management_message_removes_it() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&AppSettings::default());
        state.set_management_message("読み込みました".to_string(), false);

        state.clear_management_message();

        assert!(state.management_message.is_none());
    }

    #[test]
    fn resolve_action_window_closed_without_button_returns_cancel() {
        // タイトルバーの × で閉じた場合。ボタンは押されていないが、
        // ドラフトを捨てるためにキャンセルとして扱う
        assert_eq!(
            resolve_action(SettingsDialogAction::None, false),
            SettingsDialogAction::Cancel
        );
    }

    #[test]
    fn resolve_action_window_open_without_button_returns_none() {
        assert_eq!(
            resolve_action(SettingsDialogAction::None, true),
            SettingsDialogAction::None
        );
    }

    #[test]
    fn resolve_action_keeps_pressed_button() {
        for action in [
            SettingsDialogAction::Ok,
            SettingsDialogAction::Cancel,
            SettingsDialogAction::Apply,
            SettingsDialogAction::TestSound,
        ] {
            assert_eq!(resolve_action(action, true), action);
            assert_eq!(resolve_action(action, false), action);
        }
    }

    #[test]
    fn commit_draft_replaces_device_and_screenshot_sections() {
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.video.format, Some("MJPEG".to_string()));
        assert_eq!(shared.video.resolution, Some((1920, 1080)));
        assert_eq!(shared.video.fps, Some(30));
        assert_eq!(shared.audio.sample_rate, Some(44100));
        assert_eq!(shared.audio.channels, Some(1));
        assert!(!shared.audio.passthrough_enabled);
        assert_eq!(shared.screenshot.sound_volume, 50.0);
        // 保存形式・品質・出力先も screenshot セクションごと差し替わる
        assert_eq!(shared.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(shared.screenshot.jpeg_quality, 60);
        assert_eq!(shared.screenshot.destination, ScreenshotDestination::Both);
    }

    #[test]
    fn commit_draft_replaces_hotkey_assignments() {
        // ホットキーの割り当てはダイアログの中だけで変わるので、
        // ドラフトの内容でまるごと差し替える
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(shared.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
    }

    #[test]
    fn commit_draft_clearing_every_hotkey_reaches_the_shared_settings() {
        // すべての割り当てを外した状態を、空のマップとして反映できること。
        // 「ドラフトに何も無い＝変更なし」と扱うと、解除が反映されない
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let mut draft = AppSettings::default();
        draft.hotkeys.clear();

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.hotkeys.is_empty());
    }

    #[test]
    fn commit_draft_applies_ui_items_the_dialog_edits() {
        // 「ユーザーインターフェース」グループの 2 項目は、ダイアログで
        // 編集されていれば反映する
        let mut shared = AppSettings::default();
        let original = AppSettings::default();
        let draft = sample_settings();

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 80.0);
        assert!(!shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn commit_draft_keeps_auto_reconnect_changed_outside_dialog() {
        // 自動再接続は右クリックメニューだけで切り替える。ダイアログを開いたまま
        // 切り替えて「適用」を押しても、開いた時点の値へ巻き戻ってはいけない
        let original = sample_settings(); // auto_reconnect = false
        let draft = original.clone(); // ダイアログでは触れない項目
        let mut shared = original.clone();

        shared.video.auto_reconnect = true;

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.video.auto_reconnect);
        // 同じ video セクションの他の項目はドラフトで差し替わる
        assert_eq!(shared.video.fps, Some(30));
    }

    #[test]
    fn commit_draft_keeps_window_state_changed_while_dialog_is_open() {
        // ダイアログを開いている間にウィンドウを動かす・最前面表示を切り替える
        // といった操作をしても、OK でその変更が巻き戻ってはいけない。
        // ドラフトは開いた時点のスナップショットなので、これらを丸ごと
        // 書き戻すと位置が飛ぶ
        let mut shared = sample_settings();
        let draft = shared.clone();
        let original = shared.clone();

        shared.ui.last_window_size = Some((1280.0, 720.0));
        shared.ui.last_window_pos = Some((100.0, 200.0));
        shared.ui.always_on_top = false;
        shared.ui.enable_drag_move = true;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.last_window_size, Some((1280.0, 720.0)));
        assert_eq!(shared.ui.last_window_pos, Some((100.0, 200.0)));
        assert!(!shared.ui.always_on_top);
        assert!(shared.ui.enable_drag_move);
    }

    #[test]
    fn commit_draft_keeps_mute_changed_outside_dialog() {
        // ミュートはダイアログに無い。ダイアログを開いたまま切り替えて
        // 「適用」を押しても、開いた時点の値へ巻き戻ってはいけない
        let original = sample_settings();
        let draft = original.clone();
        let mut shared = original.clone();

        shared.ui.muted = !original.ui.muted;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.muted, !original.ui.muted);
    }

    #[test]
    fn commit_draft_keeps_borderless_changed_outside_dialog() {
        // タイトルバーの表示もダイアログに無い。ダイアログを開いたまま
        // 右クリックメニューで隠して「適用」を押しても、装飾が戻ってはいけない
        let original = sample_settings();
        let draft = original.clone();
        let mut shared = original.clone();

        shared.ui.borderless = !original.ui.borderless;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.borderless, !original.ui.borderless);
    }

    #[test]
    fn commit_draft_keeps_drag_move_enabled_by_the_borderless_guard() {
        // 装飾を外すときのガードが有効にした「画面ドラッグ移動」も、
        // ダイアログの「適用」で切られてはいけない。切られると
        // タイトルバーもドラッグ移動も無い状態になり、ウィンドウを動かせなくなる
        let mut original = sample_settings();
        original.ui.enable_drag_move = false;
        let draft = original.clone();
        let mut shared = original.clone();

        // 右クリックメニューで「タイトルバーを隠す」を押した状態
        shared.ui.borderless = true;
        shared.ui.enable_drag_move = true;

        commit_draft(&mut shared, &draft, &original);

        assert!(shared.ui.borderless);
        assert!(shared.ui.enable_drag_move);
    }

    #[test]
    fn commit_draft_keeps_ui_items_changed_outside_dialog() {
        // ダイアログを開いたまま映像上でホイール操作をして音量を変え、
        // コンテキストメニューでアスペクト比を切り替えたあとに「適用」を
        // 押しても、それらが巻き戻ってはいけない
        let original = sample_settings();
        let draft = original.clone(); // ダイアログでは何も編集していない
        let mut shared = original.clone();

        shared.ui.volume = 150.0;
        shared.ui.maintain_aspect_ratio = true;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 150.0);
        assert!(shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn commit_draft_applies_ui_items_edited_in_dialog_over_outside_changes() {
        // ダイアログ側で編集していれば、外側の変更より優先する
        let original = sample_settings();
        let mut draft = original.clone();
        let mut shared = original.clone();

        draft.ui.volume = 120.0;
        draft.ui.maintain_aspect_ratio = true;
        shared.ui.volume = 150.0;

        commit_draft(&mut shared, &draft, &original);

        assert_eq!(shared.ui.volume, 120.0);
        assert!(shared.ui.maintain_aspect_ratio);
    }

    #[test]
    fn settings_dialog_state_commit_into_without_draft_does_nothing() {
        // ドラフトを持っていない状態で反映しても何も起きない
        let state = SettingsDialogState::default();
        let mut shared = sample_settings();
        let before = shared.clone();

        state.commit_into(&mut shared);

        assert_eq!(shared.video.fps, before.video.fps);
        assert_eq!(shared.ui.volume, before.ui.volume);
    }

    #[test]
    fn settings_dialog_state_begin_edit_snapshots_current_settings() {
        let settings = sample_settings();
        let mut state = SettingsDialogState::default();
        assert!(!state.has_draft());

        state.begin_edit(&settings);

        assert!(state.has_draft());
        assert_eq!(state.draft().expect("ドラフトがある").video.fps, Some(30));
    }

    #[test]
    fn settings_dialog_state_draft_edit_does_not_reach_source() {
        // ドラフトの編集が共有設定へ漏れないこと。
        // 漏れると、未確定の編集がウィンドウ操作などをきっかけに保存される
        let settings = sample_settings();
        let mut state = SettingsDialogState::default();
        state.begin_edit(&settings);

        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn settings_dialog_state_end_edit_drops_draft() {
        let mut state = SettingsDialogState::default();
        state.begin_edit(&sample_settings());

        state.end_edit();

        assert!(!state.has_draft());
        assert!(state.draft().is_none());
    }

    #[test]
    fn settings_dialog_state_reopen_after_close_by_window_button_uses_latest_settings() {
        // × で閉じたあと、別の手段で設定を変えてから開き直したとき、
        // 閉じる前のスナップショットが復活してはいけない
        let mut settings = sample_settings();
        let mut state = SettingsDialogState::default();

        state.begin_edit(&settings);
        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        // タイトルバーの × で閉じる
        let closed =
            SettingsDialogState::transition_for(resolve_action(SettingsDialogAction::None, false));
        assert!(closed.close);
        assert!(!closed.commit_draft);
        state.end_edit();

        // 別の手段で設定が変わる
        settings.video.fps = Some(60);

        state.begin_edit(&settings);

        assert_eq!(state.draft().expect("ドラフトがある").video.fps, Some(60));
    }

    #[test]
    fn apply_then_cancel_keeps_applied_values() {
        // 「適用」で反映した内容は、そのあと「キャンセル」しても戻さない
        let mut shared = sample_settings();
        let mut state = SettingsDialogState::default();
        state.begin_edit(&shared);
        state.draft_mut().expect("ドラフトがある").video.fps = Some(24);

        let applied = SettingsDialogState::transition_for(SettingsDialogAction::Apply);
        assert!(applied.commit_draft);
        assert!(applied.save_to_file);
        assert!(!applied.close);
        state.commit_into(&mut shared);

        let cancelled = SettingsDialogState::transition_for(SettingsDialogAction::Cancel);
        assert!(!cancelled.commit_draft);
        assert!(cancelled.close);
        state.end_edit();

        assert_eq!(shared.video.fps, Some(24));
        assert!(!state.has_draft());
    }

    fn modifiers(ctrl: bool, shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            // Windows では command は ctrl と同じ値にする決まりになっている
            command: ctrl,
        }
    }

    #[test]
    fn build_hotkey_string_no_input_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_only_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_two_modifiers_only_returns_none() {
        // 修飾キーが 2 つ押されただけで確定してしまう不具合の再現
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, true), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_only_returns_none() {
        assert_eq!(build_hotkey_string(&modifiers(true, true, true), &[]), None);
    }

    #[test]
    fn build_hotkey_string_unsupported_key_only_returns_none() {
        // 対応していないキーは通常キーとして数えない
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Tab]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_single_key_returns_key_only() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::F5]),
            Some("F5".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::A]),
            Some("A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_with_key_returns_combination() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[egui::Key::S]),
            Some("Ctrl+S".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_with_key_keeps_fixed_order() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, true), &[egui::Key::A]),
            Some("Ctrl+Shift+Alt+A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_digit_keys_are_supported() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::Num0]),
            Some("0".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Num9]),
            Some("Ctrl+Shift+9".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_ignores_unsupported_keys_when_key_is_present() {
        assert_eq!(
            build_hotkey_string(
                &modifiers(true, false, false),
                &[egui::Key::Tab, egui::Key::S]
            ),
            Some("Ctrl+S".to_string())
        );
    }

    #[test]
    fn hotkey_capture_state_default_is_idle_and_empty() {
        let capture = HotkeyCaptureState::default();
        assert!(!capture.is_capturing());
        assert_eq!(capture.temp(), "");
    }

    #[test]
    fn hotkey_capture_state_start_enters_capturing_and_clears_previous_result() {
        // 『キャプチャ開始』を押し直したときに、前回取ったキーが
        // 残っていると、何も押さずに OK しただけで古い値が確定してしまう
        let mut capture = HotkeyCaptureState::default();
        capture.finish("Ctrl+S".to_string());

        capture.start();

        assert!(capture.is_capturing());
        assert_eq!(capture.temp(), "");
    }

    #[test]
    fn hotkey_capture_state_finish_leaves_capturing_and_keeps_key() {
        let mut capture = HotkeyCaptureState::default();
        capture.start();

        capture.finish("Ctrl+Shift+A".to_string());

        assert!(!capture.is_capturing());
        assert_eq!(capture.temp(), "Ctrl+Shift+A");
    }

    #[test]
    fn hotkey_capture_state_stop_keeps_captured_key() {
        // 『停止』は待機をやめるだけで、取得済みのキーは捨てない
        let mut capture = HotkeyCaptureState::default();
        capture.finish("F5".to_string());
        capture.start();
        capture.finish("F6".to_string());

        capture.stop();

        assert!(!capture.is_capturing());
        assert_eq!(capture.temp(), "F6");
    }

    #[test]
    fn hotkey_capture_state_take_captured_returns_key_and_empties_state() {
        let mut capture = HotkeyCaptureState::default();
        capture.start();
        capture.finish("Ctrl+S".to_string());

        assert_eq!(capture.take_captured(), Some("Ctrl+S".to_string()));
        assert!(!capture.is_capturing());
        assert_eq!(capture.temp(), "");
        // 2 度目は何も返さない。返すと同じキーを再度確定させてしまう
        assert_eq!(capture.take_captured(), None);
    }

    #[test]
    fn hotkey_capture_state_take_captured_without_key_returns_none() {
        // 何も入力せずに OK を押した場合。呼び出し側の現在値を消さないよう None を返す
        let mut capture = HotkeyCaptureState::default();
        capture.start();

        assert_eq!(capture.take_captured(), None);
        assert!(!capture.is_capturing());
    }

    #[test]
    fn hotkey_capture_state_reset_discards_capturing_and_key() {
        // キャンセルとクリア。次に開いたときへ入力中の状態を持ち越さない
        let mut capture = HotkeyCaptureState::default();
        capture.start();
        capture.finish("Alt+F4".to_string());
        capture.start();

        capture.reset();

        assert!(!capture.is_capturing());
        assert_eq!(capture.temp(), "");
    }

    #[test]
    fn settings_dialog_state_end_edit_keeps_hotkey_capture_state() {
        // ホットキー入力ダイアログを開いたまま設定ダイアログを × で閉じても、
        // 入力中の状態を失わない（static だったときと同じ振る舞い）
        let mut state = SettingsDialogState::default();
        state.begin_edit(&sample_settings());
        state.hotkey_capture_mut().start();

        state.end_edit();

        assert!(!state.has_draft());
        assert!(state.hotkey_capture_mut().is_capturing());
    }

    #[test]
    fn settings_dialog_state_default_tab_is_device() {
        let mut state = SettingsDialogState::default();
        assert_eq!(state.selected_tab, SettingsTab::Device);

        // タブの選択はドラフトとは別なので、開いて閉じても引き継がれる
        state.begin_edit(&sample_settings());
        state.selected_tab = SettingsTab::Screenshot;
        state.end_edit();

        assert_eq!(state.selected_tab, SettingsTab::Screenshot);
    }

    /// 取得できたことにする能力。中身そのものは検証の対象ではないので最小限
    fn sample_capabilities() -> DeviceCapabilities {
        vec![
            FormatCapability::new(
                "MJPEG",
                vec![
                    VideoMode::new(1920, 1080, 30),
                    VideoMode::new(1280, 720, 60),
                ],
            ),
            FormatCapability::new("YUY2", vec![VideoMode::new(1280, 720, 60)]),
        ]
    }

    #[test]
    fn out_of_range_note_is_none_when_the_value_is_selectable() {
        assert_eq!(out_of_range_note(&[44100, 48000], 48000, " Hz"), None);
    }

    #[test]
    fn out_of_range_note_names_the_value_actually_used() {
        // 設定ファイルを手で書き換えた場合など、選択肢に無い値が残ることがある。
        // 何で開かれるかを併記しないと、接続状態タブとの食い違いが分からない
        let note = out_of_range_note(&[32000, 48000], 44100, " Hz").expect("注意書きが要る");

        assert!(note.contains("44100 Hz"), "{note}");
        assert!(note.contains("48000 Hz"), "{note}");
    }

    #[test]
    fn out_of_range_note_tie_picks_the_smaller_value() {
        // audio::nearest_sample_rate と同じ寄せ方でないと、実際に開く値と食い違う
        let note = out_of_range_note(&[32000, 48000], 40000, " Hz").expect("注意書きが要る");

        assert!(note.contains("32000 Hz"), "{note}");
    }

    #[test]
    fn out_of_range_note_empty_choices_returns_none() {
        // 選択肢を作れていない状況では、どの値へ寄るかを断定できない
        assert_eq!(out_of_range_note(&[], 44100, " Hz"), None);
    }

    #[test]
    fn channel_label_names_mono_and_stereo() {
        assert_eq!(channel_label(1), "1（モノラル）");
        assert_eq!(channel_label(2), "2（ステレオ）");
        assert_eq!(channel_label(6), "6 ch");
    }

    #[test]
    fn capability_cache_holds_audio_capabilities_too() {
        // 型引数を変えただけで同じキャッシュが使えること
        let mut cache = AudioCapabilityCache::default();

        assert!(cache.request(crate::audio::DEFAULT_DEVICE_KEY));
        assert!(cache.is_pending(crate::audio::DEFAULT_DEVICE_KEY));
        assert!(cache.ready(crate::audio::DEFAULT_DEVICE_KEY).is_none());

        cache.apply_result(
            crate::audio::DEFAULT_DEVICE_KEY.to_string(),
            Err("デバイスがありません".to_string()),
        );

        assert!(!cache.is_pending(crate::audio::DEFAULT_DEVICE_KEY));
    }

    #[test]
    fn capability_cache_is_pending_is_false_for_an_unrequested_device() {
        // 未要求を「待ち」と見なすと、音声の接続が永久に待ってしまう
        let cache = VideoCapabilityCache::default();

        assert!(!cache.is_pending("Capture Device"));
    }

    #[test]
    fn capability_cache_request_new_device_marks_pending_and_queues() {
        let mut cache = VideoCapabilityCache::default();

        assert!(cache.request("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
        assert_eq!(cache.take_requests(), vec!["Capture Device".to_string()]);
    }

    #[test]
    fn capability_cache_request_twice_queues_only_once() {
        // 描画のたびに呼ばれるので、二重に投げるとデバイスを何度も開きに行く
        let mut cache = VideoCapabilityCache::default();

        assert!(cache.request("Capture Device"));
        assert!(!cache.request("Capture Device"));
        assert_eq!(cache.take_requests().len(), 1);
    }

    #[test]
    fn capability_cache_request_empty_device_name_is_ignored() {
        // デバイス未選択のとき。空の名前で問い合わせても意味がない
        let mut cache = VideoCapabilityCache::default();

        assert!(!cache.request(""));
        assert_eq!(cache.state(""), None);
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_request_after_failure_does_not_queue_again() {
        // 失敗したデバイスを毎フレーム開きに行かない。投げ直すのは「再取得」だけ
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(!cache.request("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_take_requests_empties_the_queue() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("A");
        cache.request("B");

        assert_eq!(
            cache.take_requests(),
            vec!["A".to_string(), "B".to_string()]
        );
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_apply_result_ok_becomes_ready() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert_eq!(cache.ready("Capture Device"), Some(&sample_capabilities()));
    }

    #[test]
    fn capability_cache_apply_result_err_becomes_failed_with_reason() {
        // 理由は画面に出すので、握り潰さず保持する
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        cache.apply_result(
            "Capture Device".to_string(),
            Err("Device 'Capture Device' not found".to_string()),
        );

        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Failed(
                "Device 'Capture Device' not found".to_string()
            ))
        );
        assert_eq!(cache.ready("Capture Device"), None);
    }

    #[test]
    fn capability_cache_ready_is_none_while_pending() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");

        assert_eq!(cache.ready("Capture Device"), None);
    }

    #[test]
    fn capability_cache_retry_after_failure_queues_again() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(cache.retry("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
        assert_eq!(cache.take_requests(), vec!["Capture Device".to_string()]);
    }

    #[test]
    fn capability_cache_retry_while_pending_does_not_queue() {
        // 投げ直すと、先の取得があとから届いて新しい結果を上書きする
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        assert!(!cache.retry("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_retry_after_success_queues_again() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.retry("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_true_once_after_result_arrives() {
        // 目印を消さないと、ユーザーが選び直したフォーマットを毎フレーム戻してしまう
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.should_apply_defaults("Capture Device"));
        assert!(!cache.should_apply_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_false_while_pending() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.expect_defaults("Capture Device");

        assert!(!cache.should_apply_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_false_for_another_device() {
        // 取得を待っている間にもう一度切り替えた場合。先に届いた別デバイスの
        // 能力で選択を書き換えない
        let mut cache = VideoCapabilityCache::default();
        cache.request("A");
        cache.request("B");
        cache.take_requests();
        cache.expect_defaults("B");
        cache.apply_result("A".to_string(), Ok(sample_capabilities()));

        assert!(!cache.should_apply_defaults("A"));
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_false_when_failed() {
        // 失敗したときは選択を書き換えない。既定の選択肢のまま残す
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(!cache.should_apply_defaults("Capture Device"));
    }

    #[test]
    fn select_default_video_mode_without_previous_takes_largest_resolution_and_fps() {
        // 前の値が無いとき（初回など）は、対応する中で最大の解像度・最高の FPS
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "MJPEG",
            vec![
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1920, 1080, 24),
                VideoMode::new(1920, 1080, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, None, None),
            Some(("MJPEG".to_string(), (1920, 1080), 30))
        );
    }

    #[test]
    fn select_default_video_mode_keeps_previous_when_supported() {
        // 新しいデバイスが同じ組み合わせに対応していれば、そのまま据え置く
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_keeps_previous_over_same_pixel_count_resolution() {
        // 960x960 と 1280x720 はどちらも 921,600 画素で、画素数の差だけでは並ぶ。
        // 完全一致する 1280x720 が一覧の後ろにあっても取りこぼさないこと
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![VideoMode::new(960, 960, 60), VideoMode::new(1280, 720, 60)],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_picks_nearest_resolution() {
        // 1600x900（1,440,000 画素）に最も近いのは 1280x720（921,600 画素）。
        // 1920x1080 は 2,073,600 画素で差が大きい
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1600, 900)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_prefers_resolution_over_fps() {
        // 解像度が先。FPS を合わせるために解像度を落とさない
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(640, 480, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1920, 1080)), Some(60)),
            Some(("YUY2".to_string(), (1920, 1080), 30))
        );
    }

    #[test]
    fn select_default_video_mode_picks_nearest_fps_within_same_resolution() {
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1280, 720, 30),
                VideoMode::new(1280, 720, 24),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(25)),
            Some(("YUY2".to_string(), (1280, 720), 24))
        );
    }

    #[test]
    fn select_default_video_mode_equal_distance_takes_larger_resolution() {
        // 1,000,000 画素からの差がどちらも 200,000 で並ぶ。
        // 決着を付けないとフレームごとに違う値が選ばれうる
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(800, 1000, 30),
                VideoMode::new(1200, 1000, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1000, 1000)), Some(30)),
            Some(("YUY2".to_string(), (1200, 1000), 30))
        );
    }

    #[test]
    fn select_default_video_mode_without_previous_fps_takes_highest_for_that_resolution() {
        // 解像度だけ分かっているとき。FPS は差で並ばないので最高のものになる
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1280, 720, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1920, 1080, 60),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), None),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_without_previous_resolution_takes_nearest_fps() {
        // FPS だけ分かっているとき。解像度は差で並ばないので、まず FPS が合う
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 60),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, None, Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_uses_first_format_even_if_another_matches_better() {
        // フォーマットは能力一覧の先頭を採る（YUY2 優先）。
        // 解像度と FPS はそのフォーマットが対応する中から選ぶので、
        // 他のフォーマットにもっと近い組み合わせがあっても移らない
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![VideoMode::new(640, 480, 30)]),
            FormatCapability::new("MJPEG", vec![VideoMode::new(1920, 1080, 60)]),
        ];

        assert_eq!(
            select_default_video_mode(&caps, Some((1920, 1080)), Some(60)),
            Some(("YUY2".to_string(), (640, 480), 30))
        );
    }

    #[test]
    fn select_default_video_mode_skips_format_without_any_mode() {
        // 組み合わせを持たないフォーマットを選ぶと、解像度の選択肢が空になる
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![]),
            FormatCapability::new("MJPEG", vec![VideoMode::new(1280, 720, 60)]),
        ];

        assert_eq!(
            select_default_video_mode(&caps, None, None),
            Some(("MJPEG".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_returns_none_for_empty_capabilities() {
        // 呼び出し側は設定を触らない。空の値で上書きしない
        let caps: DeviceCapabilities = Vec::new();

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            None
        );
    }

    #[test]
    fn select_default_video_mode_returns_none_when_every_format_is_empty() {
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![]),
            FormatCapability::new("MJPEG", vec![]),
        ];

        assert_eq!(select_default_video_mode(&caps, None, None), None);
    }
}
