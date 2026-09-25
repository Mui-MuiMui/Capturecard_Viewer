//! 設定ダイアログとホットキー設定ダイアログの描画。
//!
//! **描画は状態を持たず、書き換えるのもドラフトだけ。** 起きたことは
//! `SettingsEvent` / `HotkeyDialogEvent` の列で返し、実際に状態を動かすのは
//! `app` 側（`docs/design/settings-dialog.md`）。
//!
//! ここに置くのは全体の入口 `show_settings_dialog` と、タブをまたいで使う
//! イベント型・注意書きのヘルパーだけ。タブごとの中身と、設定の組み替えだけを
//! 行う関数は子モジュールへ分けてある。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `capability.rs` | デバイス能力のキャッシュと、そこから作る選択肢まわりの表示 |
//! | `state.rs` | `SettingsDialogState`（ドラフトの保持と操作の受け止め） |
//! | `draft.rs` | ドラフトの反映・読み込み・初期化 |
//! | `preset.rs` | プリセットの保存・読み込み・削除（描画を含まない） |
//! | `video_mode.rs` | デバイス切り替え時に選び直すビデオの既定値 |
//! | `device_tab.rs` | 「デバイス設定」タブ |
//! | `screenshot_tab.rs` | 「スクリーンショット設定」タブ |
//! | `hotkeys_tab.rs` | 「ホットキー」タブ |
//! | `hotkey_capture.rs` | ホットキー入力ダイアログ |
//! | `other_tab.rs` | 「その他」タブ |
//! | `status_tab.rs` | 「接続状態」タブ |

mod capability;
mod device_tab;
mod draft;
mod hotkey_capture;
mod hotkeys_tab;
mod other_tab;
mod preset;
mod screenshot_tab;
mod state;
mod status_tab;
mod video_mode;

// `ui` の外（`src/app/*.rs`）から使う経路は分割前と同じ `crate::ui::...` に
// する。子モジュールはすべて私有なので、呼び出し側は分割を知らない。
//
// **`ui` の中だけで使う `pub` 項目はここに並べない。** `mod ui;` 自体が私有
// なので、誰も使わない再輸出は `unused_imports` の警告になる。
// `commit_draft` や `select_default_video_mode` のような項目は、使う側が
// 子モジュールの経路（`self::draft::commit_draft`）で参照する
pub use self::capability::AudioCapabilityCache;
pub use self::draft::{draft_from_defaults, draft_from_imported};
pub use self::hotkey_capture::{show_hotkey_capture_dialog, HotkeyDialogEvent};
pub use self::preset::PresetRowAction;
pub use self::state::{resolve_action, SettingsDialogState, SettingsDialogView};

use self::device_tab::show_device_settings_tab;
use self::hotkeys_tab::show_hotkey_settings_tab;
use self::other_tab::show_other_tab;
use self::screenshot_tab::show_screenshot_settings_tab;
use self::status_tab::show_status_tab;

use crate::audio::AudioDirection;
use crate::hotkey::{HotkeyAction, HotkeyAssignmentError};
use crate::i18n::Text;
use crate::settings::AppSettings;
use crate::status::ConnectionStatus;
use eframe::egui;
use std::collections::BTreeMap;

/// 設定ダイアログの開閉と反映を決める操作。
///
/// 各ボタンの意味は `README.md` の「設定」と
/// `docs/ARCHITECTURE.md` の「適用の境界を明確にする」に合わせている。
///
/// **ここにあるのはダイアログの一生に関わる 3 つだけ。** テスト再生や
/// 書き出しのようにダイアログの状態を動かさない操作は `SettingsEvent` の
/// 別の変種で表す。混ぜると `transition_for` が「何もしない」変種ばかりに
/// なり、追加した操作が誤って反映や保存を引き起こす余地が残る。
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
}

/// 設定ダイアログの 1 フレームで起きたこと。
///
/// 描画関数は状態を書き換えず、起きたことをこの列で返す。実際に状態を
/// 動かすのは `app::settings_dialog::handle_settings_events`
/// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
///
/// **1 フレームで複数起きうるので `Vec` で返す。** 例えば名前を打ちながら
/// 「保存」を押した場合は `SetNewPresetName` と `SaveNewPreset` が並ぶ。
/// **並び順が意味を持つ**ので、受け取った側は順番どおりに処理すること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    /// 適用 / OK / キャンセル（タイトルバーの × を含む）。
    /// **必ず列の最後に来る。** 他の操作を反映してから閉じるため
    Dialog(SettingsDialogAction),
    /// タブを切り替えた
    SelectTab(SettingsTab),
    /// テスト再生: **ドラフトの**効果音を編集中の音量で鳴らすだけ。
    /// 設定は動かさないしダイアログも閉じない。
    ///
    /// 載せるのはドラフトの `screenshot.sound_file`。適用済みの効果音を
    /// 鳴らすと、ファイルを選び直した直後に押しても「適用」するまで古い音が
    /// 鳴る（Issue #204）。`None`（鳴らさない）のときはボタンを出さないので、
    /// このイベントも返らない
    TestSound(std::path::PathBuf),
    /// 設定を書き出す: **実行中の設定**をファイルへ保存する。
    /// 編集中のドラフトではないので、ダイアログの中身は動かない
    ExportSettings,
    /// 設定を読み込む: ファイルを読んでドラフトへ入れる。
    /// 反映は「適用」「OK」で行うので、ここでは実行中の設定を触らない
    ImportSettings,
    /// 設定を初期化: ドラフトを既定値に戻す。こちらも反映は「適用」「OK」
    ResetDraft,
    /// 「設定を初期化...」の確認待ちにする（`true`）／やめる（`false`）
    SetResetConfirm(bool),
    /// 新しいプリセットの名前入力欄の内容が変わった
    SetNewPresetName(String),
    /// 入力欄の名前で、ドラフトをプリセットとして保存する
    SaveNewPreset,
    /// プリセット一覧の行のボタンが押された
    PresetRow(PresetRowAction),
    /// ホットキー入力ダイアログをこのアクションで開く
    OpenHotkeyCapture(HotkeyAction),
    /// スクリーンショットの保存フォルダーをファイルダイアログで選ぶ
    PickScreenshotFolder,
    /// 効果音のファイルをファイルダイアログで選ぶ
    PickSoundFile,
    /// デバイス能力のキャッシュに対する要求
    Capability(CapabilityEvent),
}

/// デバイス能力のキャッシュに対する要求。
///
/// キャッシュは `SettingsDialogState` が持つが、書き換えるのは `app` だけ。
/// 描画側は「取得したい」「目印を落としてよい」を返すに留める。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityEvent {
    /// まだ問い合わせていなければ取得を要求する（`CapabilityCache::request`）
    RequestVideo(String),
    /// 取得済み・失敗済みでも問い合わせ直す（「再取得」）
    RetryVideo(String),
    /// デバイスを切り替えた。能力が届いたら既定値を選び直す目印を立てる
    ExpectVideoDefaults(String),
    /// 既定値の選び直しを済ませたので目印を落とす
    ClearVideoDefaults(String),
    /// オーディオ側の `RequestVideo` 相当。文字列は `audio::cache_key`
    RequestAudio(AudioDirection, String),
    /// オーディオ側の `RetryVideo` 相当
    RetryAudio(AudioDirection, String),
    /// オーディオ側の `ExpectVideoDefaults` 相当
    ExpectAudioDefaults(AudioDirection, String),
    /// オーディオ側の `ClearVideoDefaults` 相当
    ClearAudioDefaults(AudioDirection, String),
}

/// 「その他」タブに出す 1 行のメッセージ。
///
/// 書き出し・読み込み・初期化の結果をその場で伝える。失敗はトーストでも
/// 出すが、トーストは画面下部に出るためダイアログに隠れることがある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementMessage {
    pub text: String,
    /// 失敗を伝えるものか。表示の種別を分ける
    pub is_error: bool,
}

/// 注意・失敗・成功を伝える表示の種別。
///
/// **固定色（`egui::Color32::YELLOW` など）を直接書かないためにある。** 彩度の高い
/// 色を文字そのものへ使うと、テーマの背景との差が強すぎて読みづらくなる。ライトと
/// ダークのどちらかでしか成立しない色にもなりやすい。ここを通せば、文字色は通常の
/// ままで、薄い背景と記号によって種別が分かる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// 続行はできるが想定と違う
    Warning,
    /// その操作が成立していない
    Error,
    /// 意図した状態になっている
    Success,
}

impl NoticeKind {
    /// 背景と枠線の元にする色。
    ///
    /// 注意と失敗は `egui::Visuals` 由来の色をそのまま使うため、ライトでもダークでも
    /// 地の色との関係が保たれる。成功に当たる色だけは `Visuals` に無いので、ここで
    /// テーマごとの明度を持つ。**固定色を書いてよいのはこの 1 か所だけ。**
    fn accent(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            NoticeKind::Warning => visuals.warn_fg_color,
            NoticeKind::Error => visuals.error_fg_color,
            // ダークの地に沈まない明るめの緑と、ライトの地で浮かない濃い緑
            NoticeKind::Success if visuals.dark_mode => egui::Color32::from_rgb(0x5c, 0xb8, 0x5c),
            NoticeKind::Success => egui::Color32::from_rgb(0x2e, 0x7d, 0x32),
        }
    }

    /// 文言の先頭に付ける記号。
    ///
    /// **色を見分けられなくても種別が分かるようにする。** 背景の濃さは控えめなので、
    /// 記号が無いと注意と失敗の区別が色だけに頼ることになる。
    fn symbol(self) -> &'static str {
        match self {
            NoticeKind::Warning => "⚠",
            NoticeKind::Error => "×",
            NoticeKind::Success => "●",
        }
    }
}

/// 背景に敷く濃さ。文字は通常色のままなので、地と区別が付く程度に留める。
const NOTICE_FILL_FACTOR: f32 = 0.18;

/// 枠線の濃さ。背景だけでは輪郭が沈むため、縁は少し強めに出す。
const NOTICE_STROKE_FACTOR: f32 = 0.55;

/// 注意書きと状態表示を入れる枠。
fn notice_frame(ui: &egui::Ui, kind: NoticeKind) -> egui::Frame {
    let accent = kind.accent(ui.visuals());
    egui::Frame::none()
        .fill(accent.gamma_multiply(NOTICE_FILL_FACTOR))
        .stroke(egui::Stroke::new(
            1.0_f32,
            accent.gamma_multiply(NOTICE_STROKE_FACTOR),
        ))
        .rounding(egui::Rounding::same(4.0))
        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
}

/// 種別を指定して注意書きを描く。失敗なら `NoticeKind::Error` を渡す。
///
/// 記号は種別から付くので、**呼び出し側の文言に「⚠」などを書かない。**
fn notice_label(ui: &mut egui::Ui, kind: NoticeKind, text: impl Into<String>) {
    let text = text.into();
    notice_frame(ui, kind).show(ui, |ui| {
        ui.label(format!("{} {}", kind.symbol(), text));
    });
}

/// 続行できるが想定と違うことを伝える。設定ダイアログの注意書きはほぼこれ。
fn warning_label(ui: &mut egui::Ui, text: impl Into<String>) {
    notice_label(ui, NoticeKind::Warning, text);
}

/// 状態を短いバッジで描く。文言は記号を含んだ状態で渡す。
///
/// 注意書きと違って読み飛ばされては困るので、文字を太字にする。色は付けない。
fn status_badge(ui: &mut egui::Ui, text: &str, kind: NoticeKind) {
    notice_frame(ui, kind).show(ui, |ui| {
        ui.label(egui::RichText::new(text).strong());
    });
}

/// 設定ダイアログのタブ。
///
/// 並びはデバイス設定 / スクリーンショット設定 / ホットキー / その他 / 接続状態。
/// 「接続状態」を最後に置き、既定は「デバイス設定」のままにしてある。
/// ダイアログを開く主な目的は設定の変更で、状態の確認は調べたいときだけ
/// だからで、先頭に置くと毎回そこを通ることになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Device,
    Screenshot,
    /// ホットキーの一覧と割り当て。以前はスクリーンショット設定タブの中にあったが、
    /// フルスクリーン切替や音量操作などスクリーンショット以外のアクションも
    /// 増えたため、タブ名と内容を合わせて独立させた
    Hotkeys,
    /// 設定の書き出し・読み込み・初期化
    Other,
    /// 映像と音声が実際に何へ繋がっているか、直近の失敗は何か
    Status,
}

/// 設定ダイアログの上下左右に残す余白。
///
/// 画面ぴったりに合わせると、収まっているのかはみ出しているのか分かりにくい。
/// 右クリックメニューの `CONTEXT_MENU_SCREEN_MARGIN` と同じ理由
const SETTINGS_WINDOW_SCREEN_MARGIN: f32 = 40.0;

/// 設定ダイアログがこれより小さくなることはない。
///
/// タブの見出しと下部のボタン列が読めなくなるほど小さい画面でも、
/// 最低限操作できる大きさを確保する（内容はスクロールに任せる）
const SETTINGS_WINDOW_MIN_SIZE: egui::Vec2 = egui::vec2(320.0, 240.0);

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

/// 設定ダイアログを描画し、このフレームで起きたことを返す。
///
/// 書き換えてよいのは `draft` だけ。タブの選択・能力キャッシュ・ダイアログの
/// 開閉といった `app` 側の状態はここでは動かさず、`SettingsEvent` で返す
/// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
///
/// ドラフトの反映・保存・クローズは、呼び出し側が
/// `SettingsEvent::Dialog` を `transition_for` にかけて行う。
pub fn show_settings_dialog(
    ctx: &egui::Context,
    draft: &mut AppSettings,
    view: &SettingsDialogView<'_>,
    devices: &DeviceLists<'_>,
    connection: &ConnectionStatus,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyAssignmentError>,
) -> Vec<SettingsEvent> {
    let mut events: Vec<SettingsEvent> = Vec::new();
    let mut button = SettingsDialogAction::None;

    // タブは `selectable_value` に `&mut` が要るので複製を渡し、変わったら
    // イベントで返す。**このフレームの描画にはこの複製を使う。** 呼び出し側の
    // 値を見ると、切り替えた直後の 1 フレームだけ前のタブが描かれる
    let mut selected_tab = view.selected_tab;

    // タイトルバーの × を拾うためのローカル。`egui::Window::open` は
    // `&mut bool` を要求するが、呼び出し側の開閉フラグを直接渡すと
    // ここからダイアログを閉じられてしまう。閉じるのは
    // `SettingsEvent::Dialog` を受け取った `app` の仕事
    let mut window_open = true;

    // 画面より大きい・画面外にずれた位置で開いていると、下部のボタン列が
    // 押せなくなる（Issue #137）。ウィンドウそのものを画面内へ収め、
    // タブの中身だけをスクロールさせることで、OK / キャンセル / 適用は
    // どんな高さでも必ず見える位置に残す
    let screen_rect = ctx.screen_rect();
    let max_size = (screen_rect.size() - egui::Vec2::splat(SETTINGS_WINDOW_SCREEN_MARGIN))
        .max(SETTINGS_WINDOW_MIN_SIZE);
    // max_size は SETTINGS_WINDOW_MIN_SIZE との component-wise max で
    // 作っているため、常に SETTINGS_WINDOW_MIN_SIZE 以上になる。
    // min_size がこれを超えることはない
    let min_size = SETTINGS_WINDOW_MIN_SIZE;

    egui::Window::new(Text::SettingsTitle.get())
        .open(&mut window_open)
        .default_size([650.0, 500.0])
        .resizable(true)
        .constrain_to(screen_rect)
        .min_size(min_size)
        .max_size(max_size)
        .show(ctx, |ui| {
            // タブ選択
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut selected_tab,
                    SettingsTab::Device,
                    Text::TabDevice.get(),
                );
                ui.selectable_value(
                    &mut selected_tab,
                    SettingsTab::Screenshot,
                    Text::TabScreenshot.get(),
                );
                ui.selectable_value(&mut selected_tab, SettingsTab::Hotkeys, Text::Hotkeys.get());
                ui.selectable_value(&mut selected_tab, SettingsTab::Other, Text::TabOther.get());
                ui.selectable_value(
                    &mut selected_tab,
                    SettingsTab::Status,
                    Text::TabStatus.get(),
                );
            });

            ui.separator();

            // OK / キャンセル / 適用のボタン列を先に描く。egui は Ui の残り
            // 領域を上から順に消費するだけなので、ScrollArea を先に描くと
            // 「まだ描いていないボタン列の分」を差し引けず、ScrollArea が
            // 残り全部を使い切ってボタン列がウィンドウの外へ押し出される。
            // `TopBottomPanel::bottom` は呼んだ時点で自分の高さぶんを
            // 親 Ui の下端から確保し、以降の ScrollArea が使える高さを
            // 先に縮めてくれるので、コード上の見た目の順序とは逆に
            // 「ボタン列 → タブの中身」の順で描く
            egui::TopBottomPanel::bottom("settings_dialog_buttons").show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(Text::ButtonOk.get()).clicked() {
                        button = SettingsDialogAction::Ok;
                    }

                    if ui.button(Text::ButtonCancel.get()).clicked() {
                        button = SettingsDialogAction::Cancel;
                    }

                    if ui.button(Text::ButtonApply.get()).clicked() {
                        button = SettingsDialogAction::Apply;
                    }
                });
                ui.add_space(4.0);
            });

            // ここでタブの中身を高さいっぱいに広げてしまうと、上のボタン列の
            // 予約が効いていても、タブによってはスクロール領域自体が必要
            // 以上に大きく残る。`auto_shrink` で中身が少ないタブでは
            // 領域自体を縮める
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| match selected_tab {
                    SettingsTab::Device => show_device_settings_tab(
                        ui,
                        draft,
                        view.video_capabilities,
                        &view.audio_capabilities,
                        devices,
                        &mut events,
                    ),
                    SettingsTab::Screenshot => show_screenshot_settings_tab(ui, draft, &mut events),
                    SettingsTab::Hotkeys => {
                        show_hotkey_settings_tab(ui, draft, hotkey_errors, &mut events)
                    }
                    SettingsTab::Other => show_other_tab(ui, draft, view, &mut events),
                    SettingsTab::Status => show_status_tab(ui, connection),
                });
        });

    if selected_tab != view.selected_tab {
        events.push(SettingsEvent::SelectTab(selected_tab));
    }

    // **必ず最後に積む。** 「適用」を押したフレームに起きた他の操作
    // （プリセットの読み込みなど）を先に処理しないと、反映が 1 フレーム遅れる
    let action = resolve_action(button, window_open);
    if action != SettingsDialogAction::None {
        events.push(SettingsEvent::Dialog(action));
    }

    events
}

/// オーディオの入力・出力の能力キャッシュをまとめて渡すための束。
///
/// 2 本の借用を個別に引数へ並べると入れ替えても型が合ってしまうため、
/// 名前で区別できる形にする（`DeviceLists` と同じ理由）。
pub struct AudioCapabilityCaches<'a> {
    pub input: &'a AudioCapabilityCache,
    pub output: &'a AudioCapabilityCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::{
        AppSettings, AudioSettings, ColorRange, ColorSpace, HotkeySettings, Preset,
        ScreenshotDestination, ScreenshotFormat, ScreenshotSettings, UiSettings, VideoSettings,
    };

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    // ここの `pub(super)` な関数は、子モジュールのテストからも使う共通の
    // 土台（`use crate::ui::tests::sample_settings;`）。同じものを各
    // ファイルへ写すと、設定に項目が増えたときの直し漏れが出る

    /// 既定値と全項目が異なる設定。どの項目が反映され、どの項目が
    /// 据え置かれるかを区別できるようにするためのもの。
    pub(super) fn sample_settings() -> AppSettings {
        AppSettings {
            // プリセットは持たない状態。プリセットを見るテストは
            // それぞれの中で足す
            active_preset: None,
            presets: Vec::new(),
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
                // 映像調整も既定値（0）と異なる値にしておく
                brightness: 10,
                contrast: -20,
                saturation: 30,
            },
            audio: AudioSettings {
                input_device_name: Some("Line In".to_string()),
                output_device_name: Some("Speakers".to_string()),
                sample_rate: Some(44100),
                channels: Some(1),
                passthrough_enabled: false,
                // 既定値（50ms）と異なる値にして、反映の有無を見分けられるようにする
                buffer_ms: 120,
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
            // 既定値（false）と異なる値にして、反映の有無を見分けられるようにする
            hotkey_settings: HotkeySettings {
                only_when_focused: true,
            },
        }
    }

    // ---- プリセット ----

    /// 解像度と fps だけが違うプリセットを作る。
    pub(super) fn preset_named(name: &str, width: u32, height: u32, fps: u32) -> Preset {
        let mut settings = AppSettings::default();
        settings.video.resolution = Some((width, height));
        settings.video.fps = Some(fps);
        Preset::from_settings(name.to_string(), &settings)
    }

    /// 既定値にプリセットの一覧だけを足した設定。
    pub(super) fn defaults_with_presets(presets: Vec<Preset>) -> AppSettings {
        AppSettings {
            presets,
            ..AppSettings::default()
        }
    }

    #[test]
    fn notice_kind_symbols_are_all_different() {
        // 色を見分けられなくても種別が分かるようにするための記号なので、
        // 重複すると意味が無くなる
        let symbols = [
            NoticeKind::Warning.symbol(),
            NoticeKind::Error.symbol(),
            NoticeKind::Success.symbol(),
        ];

        assert_eq!(BTreeSet::from(symbols).len(), symbols.len());
    }
}
