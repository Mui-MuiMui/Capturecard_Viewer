use crate::settings::{AppSettings, ScreenshotFormat, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY};
use crate::video::{DeviceCapabilities, VideoMode};
use eframe::egui;
use log::debug;
use std::collections::HashMap;

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
}

/// 設定ダイアログのタブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Device,
    Screenshot,
}

/// デバイス能力の取得状態。
///
/// 取得は `Camera::new` でデバイスを開いたうえで 3 フォーマット分の対応表を
/// 引く重い処理なので、描画スレッドでは行わず使い捨てのスレッドへ投げる。
/// ダイアログは進行状況をこの型で受け取って描き分ける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState {
    /// 取得を要求済みで、結果を待っている
    Pending,
    /// 取得できた
    Ready(DeviceCapabilities),
    /// 取得に失敗した。文字列は画面に出す理由
    Failed(String),
}

/// デバイス能力のキャッシュと、まだワーカーへ渡していない取得要求。
///
/// 触るのは UI スレッド（`CaptureCardViewer`）だけなのでロックを持たない。
/// 実際の取得は `CaptureCardViewer::dispatch_capability_requests` が別スレッドへ
/// 投げ、結果はチャネル経由で `apply_result` に入る。
#[derive(Default)]
pub struct CapabilityCache {
    /// デバイス名 → 取得状態
    states: HashMap<String, CapabilityState>,
    /// まだワーカーへ渡していないデバイス名
    requests: Vec<String>,
    /// デバイスを切り替えた直後で、能力が届いたらフォーマットの既定値を
    /// 選び直す対象のデバイス名
    awaiting_defaults: Option<String>,
}

impl CapabilityCache {
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
        if device.is_empty() || self.states.get(device) == Some(&CapabilityState::Pending) {
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
    pub fn apply_result(&mut self, device: String, result: Result<DeviceCapabilities, String>) {
        let state = match result {
            Ok(caps) => CapabilityState::Ready(caps),
            Err(reason) => CapabilityState::Failed(reason),
        };
        self.states.insert(device, state);
    }

    /// 取得状態。まだ要求もしていなければ `None`。
    pub fn state(&self, device: &str) -> Option<&CapabilityState> {
        self.states.get(device)
    }

    /// 取得できた能力。結果待ち・失敗・未要求はいずれも `None` になる。
    pub fn ready(&self, device: &str) -> Option<&DeviceCapabilities> {
        match self.states.get(device) {
            Some(CapabilityState::Ready(caps)) => Some(caps),
            _ => None,
        }
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
}

impl HotkeyCaptureState {
    pub fn is_capturing(&self) -> bool {
        self.capturing
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
    capabilities: CapabilityCache,
    // ホットキー入力ダイアログの入力状態
    hotkey_capture: HotkeyCaptureState,
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
    }

    /// 編集を終える。ドラフトは捨てる。
    pub fn end_edit(&mut self) {
        self.draft = None;
        self.original = None;
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

    /// デバイス能力の取得状態。
    ///
    /// 取得要求の取り出しと結果の反映は `CaptureCardViewer` が行うため、
    /// ダイアログを開いていない間（起動時の先読み）も触られる。
    pub fn capabilities_mut(&mut self) -> &mut CapabilityCache {
        &mut self.capabilities
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
/// `video` / `audio` / `screenshot` にこの比較が要らないのは、ダイアログの外から
/// 書き換わらないため。ホットキー入力ダイアログもドラフトへ書く。
///
/// **ダイアログに `ui` セクションの項目を足すときは、ここにも足すこと。**
pub fn commit_draft(target: &mut AppSettings, draft: &AppSettings, original: &AppSettings) {
    target.video = draft.video.clone();
    target.audio = draft.audio.clone();
    target.screenshot = draft.screenshot.clone();

    // ダイアログの「ユーザーインターフェース」グループが編集する 2 項目
    if draft.ui.maintain_aspect_ratio != original.ui.maintain_aspect_ratio {
        target.ui.maintain_aspect_ratio = draft.ui.maintain_aspect_ratio;
    }
    if draft.ui.volume != original.ui.volume {
        target.ui.volume = draft.ui.volume;
    }
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
    video_devices: &[(String, String)],
    input_devices: &[String],
    output_devices: &[String],
) -> SettingsDialogAction {
    // フィールドごとに分解して受ける。ドラフトを編集しながら
    // デバイス能力のキャッシュも書き換えるため、dialog をまるごと借りると
    // 二重の可変借用になる
    let SettingsDialogState {
        draft,
        selected_tab,
        capabilities,
        ..
    } = dialog;

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
            });

            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| match selected_tab {
                SettingsTab::Device => show_device_settings_tab(
                    ui,
                    draft,
                    capabilities,
                    video_devices,
                    input_devices,
                    output_devices,
                ),
                SettingsTab::Screenshot => {
                    if show_screenshot_settings_tab(ui, draft, show_hotkey_dialog) {
                        button = SettingsDialogAction::TestSound;
                    }
                }
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

fn show_device_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    capabilities: &mut CapabilityCache,
    video_devices: &[(String, String)],
    input_devices: &[String],
    output_devices: &[String],
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
                for (name, description) in video_devices {
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
    });

    ui.add_space(15.0);

    // オーディオ設定
    ui.group(|ui| {
        ui.strong("オーディオ設定");
        ui.add_space(5.0);

        // オーディオ入力デバイス選択 - キャッシュリストを使用
        let current_input_device = settings.audio.input_device_name.clone().unwrap_or_default();

        egui::ComboBox::from_label("オーディオ入力デバイス")
            .selected_text(if current_input_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_input_device
            })
            .show_ui(ui, |ui| {
                for device_name in input_devices {
                    ui.selectable_value(
                        &mut settings.audio.input_device_name,
                        Some(device_name.clone()),
                        device_name,
                    );
                }
            });

        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings
            .audio
            .output_device_name
            .clone()
            .unwrap_or_default();

        egui::ComboBox::from_label("オーディオ出力デバイス")
            .selected_text(if current_output_device.is_empty() {
                "デフォルト"
            } else {
                &current_output_device
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.audio.output_device_name, None, "デフォルト");
                for device_name in output_devices {
                    ui.selectable_value(
                        &mut settings.audio.output_device_name,
                        Some(device_name.clone()),
                        device_name,
                    );
                }
            });

        // サンプルレート
        ui.horizontal(|ui| {
            ui.label("サンプリングレート:");
            let sample_rates = vec![8000, 16000, 22050, 32000, 44100, 48000, 96000];
            let current_rate = settings.audio.sample_rate.unwrap_or(44100);
            egui::ComboBox::from_id_source("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate))
                .show_ui(ui, |ui| {
                    for rate in sample_rates {
                        ui.selectable_value(
                            &mut settings.audio.sample_rate,
                            Some(rate),
                            format!("{} Hz", rate),
                        );
                    }
                });
        });

        // チャンネル数
        ui.horizontal(|ui| {
            ui.label("チャンネル数:");
            let current_channels = settings.audio.channels.unwrap_or(2);
            egui::ComboBox::from_id_source("channels_combo")
                .selected_text(format!("{}", current_channels))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.audio.channels, Some(1), "1 (Mono)");
                    ui.selectable_value(&mut settings.audio.channels, Some(2), "2 (Stereo)");
                });
        });

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

/// スクリーンショット設定タブを描画し、「テスト再生」が押されたかを返す。
///
/// 効果音の再生はダイアログの仕事ではないので、ここでは鳴らさずに
/// イベントとして上へ返す（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
fn show_screenshot_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    show_hotkey_dialog: &mut bool,
) -> bool {
    ui.heading("スクリーンショット設定");
    ui.add_space(10.0);

    let mut test_sound_requested = false;

    // 保存フォルダー
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
    ui.group(|ui| {
        ui.strong("ホットキー設定");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("スクリーンショットホットキー:");
            let hotkey_str = settings
                .screenshot
                .hotkey
                .clone()
                .unwrap_or_else(|| "未設定".to_string());

            ui.label(&hotkey_str);

            if ui.button("ホットキー設定...").clicked() {
                *show_hotkey_dialog = true;
            }
        });

        if settings.screenshot.hotkey.is_some() {
            ui.horizontal(|ui| {
                if ui.button("ホットキー解除").clicked() {
                    settings.screenshot.hotkey = None;
                }
            });
        }

        ui.add_space(5.0);
        ui.small("『ホットキー設定...』を押して希望のキーコンビネーションを入力してください。");
    });

    test_sound_requested
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

/// ホットキー入力ダイアログを描画する。
///
/// `captured_hotkey` は呼び出し側が持つ確定済みのホットキー、
/// `capture` は入力待機中の一時状態。両方とも呼び出し側が保持する。
/// 戻り値は、このフレームでホットキーが確定したかどうか。
pub fn show_hotkey_capture_dialog(
    ctx: &egui::Context,
    show_dialog: &mut bool,
    captured_hotkey: &mut String,
    capture: &mut HotkeyCaptureState,
) -> bool {
    let mut close_dialog = false;

    egui::Window::new("ホットキー設定")
        .open(show_dialog)
        .fixed_size([350.0, 200.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("ホットキー設定");
                ui.add_space(10.0);

                if !capture.is_capturing() {
                    ui.label(
                        "『キャプチャ開始』を押してスクリーンショット用のキーを入力してください",
                    );

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
                        close_dialog = true;
                    }

                    if ui.button("キャンセル").clicked() {
                        capture.reset();
                        close_dialog = true;
                    }

                    if ui.button("クリア").clicked() {
                        captured_hotkey.clear();
                        capture.reset();
                        close_dialog = true;
                    }
                });
            });
        });

    let hotkey_captured = !captured_hotkey.is_empty() && close_dialog;

    if close_dialog {
        *show_dialog = false;
    }

    hotkey_captured
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
            },
            audio: AudioSettings {
                input_device_name: Some("Line In".to_string()),
                output_device_name: Some("Speakers".to_string()),
                sample_rate: Some(44100),
                channels: Some(1),
                passthrough_enabled: false,
            },
            screenshot: ScreenshotSettings {
                save_folder: PathBuf::from("C:/shots"),
                format: ScreenshotFormat::Png,
                jpeg_quality: 60,
                sound_file: Some(PathBuf::from("sound/custom.mp3")),
                sound_volume: 50.0,
                hotkey: Some("Ctrl+S".to_string()),
            },
            ui: UiSettings {
                volume: 80.0,
                maintain_aspect_ratio: false,
                last_window_size: Some((800.0, 600.0)),
                last_window_pos: Some((10.0, 20.0)),
                always_on_top: true,
                enable_drag_move: false,
                show_stats_overlay: true,
            },
        }
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
        assert_eq!(shared.screenshot.hotkey, Some("Ctrl+S".to_string()));
        assert_eq!(shared.screenshot.sound_volume, 50.0);
        // 保存形式と品質も screenshot セクションごと差し替わる
        assert_eq!(shared.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(shared.screenshot.jpeg_quality, 60);
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
    fn capability_cache_request_new_device_marks_pending_and_queues() {
        let mut cache = CapabilityCache::default();

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
        let mut cache = CapabilityCache::default();

        assert!(cache.request("Capture Device"));
        assert!(!cache.request("Capture Device"));
        assert_eq!(cache.take_requests().len(), 1);
    }

    #[test]
    fn capability_cache_request_empty_device_name_is_ignored() {
        // デバイス未選択のとき。空の名前で問い合わせても意味がない
        let mut cache = CapabilityCache::default();

        assert!(!cache.request(""));
        assert_eq!(cache.state(""), None);
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_request_after_failure_does_not_queue_again() {
        // 失敗したデバイスを毎フレーム開きに行かない。投げ直すのは「再取得」だけ
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(!cache.request("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_take_requests_empties_the_queue() {
        let mut cache = CapabilityCache::default();
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
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert_eq!(cache.ready("Capture Device"), Some(&sample_capabilities()));
    }

    #[test]
    fn capability_cache_apply_result_err_becomes_failed_with_reason() {
        // 理由は画面に出すので、握り潰さず保持する
        let mut cache = CapabilityCache::default();
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
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");

        assert_eq!(cache.ready("Capture Device"), None);
    }

    #[test]
    fn capability_cache_retry_after_failure_queues_again() {
        let mut cache = CapabilityCache::default();
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
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        assert!(!cache.retry("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_retry_after_success_queues_again() {
        let mut cache = CapabilityCache::default();
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
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.should_apply_defaults("Capture Device"));
        assert!(!cache.should_apply_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_false_while_pending() {
        let mut cache = CapabilityCache::default();
        cache.request("Capture Device");
        cache.expect_defaults("Capture Device");

        assert!(!cache.should_apply_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_should_apply_defaults_is_false_for_another_device() {
        // 取得を待っている間にもう一度切り替えた場合。先に届いた別デバイスの
        // 能力で選択を書き換えない
        let mut cache = CapabilityCache::default();
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
        let mut cache = CapabilityCache::default();
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
