//! 設定の入口。
//!
//! `AppSettings` と、読み込みで必ず通る `RawAppSettings` → `From`（旧形式からの
//! 移行とプリセットの整え）、`SettingsError` をここに置く。各セクションの型と
//! serde の補助は子モジュールに分けてある（映像 `video`、音声 `audio`、
//! スクリーンショット `screenshot`、録画 `recording`、UI と更新 `ui`、
//! ホットキー `hotkeys`、プリセット `preset`、読み書き `store`）。
//!
//! 外から使う経路（`crate::settings::...`）はここの `pub use` に集める。
//! 外からテストでしか使わない項目は再輸出しない（誰も使わない `pub use` が
//! `unused_imports` の警告になる）。

use crate::hotkey::HotkeyAction;
use crate::i18n;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

mod audio;
mod hotkeys;
mod preset;
mod recording;
mod screenshot;
mod store;
#[cfg(test)]
mod testing;
mod ui;
mod video;

pub use audio::{
    AudioSettings, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MAX_BUFFER_MS, MIN_BUFFER_MS,
};
// 外からはテスト（`app::worker_loop` のテスト用の組み立て）からしか使わない。
// テストを含まないビルドで再輸出すると誰も使わない `pub use` になり
// `unused_imports` の警告になるので、テストのときだけ出す
#[cfg(test)]
pub use audio::DEFAULT_BUFFER_MS;
pub use hotkeys::HotkeySettings;
use hotkeys::{default_hotkeys, migrate_hotkeys};
use preset::sanitize_presets;
pub use preset::{resolved_active_preset, validate_preset_name, Preset};
pub use recording::{
    RecordingSettings, DEFAULT_RECORDING_FILE_NAME_FORMAT, MAX_RECORDING_BITRATE_KBPS,
    MAX_REPLAY_SECONDS, MIN_RECORDING_BITRATE_KBPS, MIN_REPLAY_SECONDS,
    RECORDING_AUDIO_BITRATES_KBPS,
};
pub use screenshot::{
    ScreenshotDestination, ScreenshotEncoding, ScreenshotFormat, ScreenshotSettings,
    DEFAULT_SOUND_FILE, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY,
};
pub use store::{export_file_name, export_to, import_from, AutoSavePolicy};
pub use ui::{LanguageSetting, UiSettings, UpdateSettings, MAX_VOLUME, MIN_VOLUME};
pub use video::{
    ColorRange, ColorSpace, VideoBackendSetting, VideoSettings, MAX_VIDEO_ADJUSTMENT,
    MIN_VIDEO_ADJUSTMENT,
};

/// 設定ファイルの保存・書き出し・読み込みが失敗した理由。
///
/// 対象は `%AppData%` の設定ファイルの保存（`AppSettings::save`）と、
/// 「その他」タブの「設定を書き出す」「設定を読み込む」。起動時の読み込み
/// （`AppSettings::load`）は失敗しても既定値で起動し、結果を `LoadOutcome` で
/// 返すのでここを通らない。
///
/// **表示用の文言はこの型の `Display` が `crate::i18n` から引く。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事
/// （`docs/design/error-reporting.md`）。文言に「設定ファイル」を付けないのは、
/// 定型文が既に「設定ファイルを読み書きできません」で始まるため。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// 読み込もうとした場所にファイルが無い
    FileNotFound(PathBuf),
    /// 読み込もうとした場所はあるが、ファイルではない（ディレクトリなど）
    NotAFile(PathBuf),
    /// TOML として書き出せない（書き込み権限が無い、ディスクが一杯など）
    ExportFailed { path: PathBuf, source: String },
    /// ファイルは読めたが TOML として解釈できない
    ImportFailed { path: PathBuf, source: String },
    /// 設定ファイルの置き場所が分からず、保存できない
    LocationUnavailable(String),
    /// 設定ファイルを保存できない（書き込み権限が無い、ディスクが一杯、
    /// 他のプロセスが開いているなど）
    SaveFailed { path: PathBuf, source: String },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            SettingsError::FileNotFound(path) => i18n::settings_file_not_found(path.display()),
            SettingsError::NotAFile(path) => i18n::settings_not_a_file(path.display()),
            SettingsError::ExportFailed { path, source } => {
                i18n::file_write_failed(path.display(), source)
            }
            SettingsError::ImportFailed { path, source } => {
                i18n::settings_import_failed(path.display(), source)
            }
            SettingsError::LocationUnavailable(source) => {
                i18n::settings_location_unavailable(source)
            }
            SettingsError::SaveFailed { path, source } => {
                i18n::file_write_failed(path.display(), source)
            }
        };
        f.write_str(&text)
    }
}

impl std::error::Error for SettingsError {}

// 設定ファイルの置き場所（crate::config_path が %AppData% の下に組み立てる）に使う名前。
// ここがずれると既存の設定ファイルを見失うため、1 箇所にまとめてある。
// ログの出力先も同じデータディレクトリを基準に決めるので、logging から参照する。
pub(crate) const APP_NAME: &str = "capturecard_viewer";

// 各構造体の #[serde(default)] は、項目を追加したあとも古い設定ファイルを
// 読めるようにするためのもの。これが無いと、
//   - Option 以外の項目が欠けた場合はパースが失敗し、全項目が初期化される
//   - Option の項目が欠けた場合は None になり、Default の値が使われない
// という形で既存ユーザーの設定が失われる。新しい項目を足すときも外さないこと。

// 読み込みは RawAppSettings を経由する。旧版の screenshot.hotkey を
// hotkeys へ移す処理（migrate_hotkeys）を、どの経路で読んでも必ず通すため。
// #[serde(from)] を外すと、テストの toml::from_str だけ移行を通らない、
// といった食い違いが生まれる。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawAppSettings")]
pub struct AppSettings {
    // 選択中のプリセット名。どれも選んでいなければ `None`。
    //
    // **TOML のテーブルより前に置くこと。** 素の値はテーブルの前にしか
    // 書けないため、`video` などの後ろに宣言すると保存できない形になる。
    pub active_preset: Option<String>,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub screenshot: ScreenshotSettings,
    // 録画。設定ファイルでは [recording] になる。プリセットには入れない
    // （保存先やビットレートはデバイスと一体の設定ではない。`docs/design/presets.md`）
    pub recording: RecordingSettings,
    pub ui: UiSettings,
    // アクション → ホットキー文字列。割り当てが無いアクションは入っていない。
    //
    // 設定ファイルでは独立した [hotkeys] セクションになる。**セクションごと
    // 存在しない場合と、空のセクションがある場合は意味が違う。** 前者は
    // 旧版が書いた設定ファイル（既定の F5 を入れる）、後者はすべての
    // 割り当てを外した状態（何も入れない）。
    pub hotkeys: BTreeMap<HotkeyAction, String>,
    // ホットキーの割り当て以外の設定。設定ファイルでは [hotkey_settings] になる。
    //
    // **[hotkeys] に混ぜないこと。** あちらは値が全て文字列である前提で
    // 読んでおり（`RawAppSettings::hotkeys`）、真偽値が混ざると旧版では
    // [hotkeys] ごと読めなくなる。
    pub hotkey_settings: HotkeySettings,
    // 名前付きのプリセット。設定ファイルでは [[presets]] の並びになる。
    //
    // 中身は `video` と `audio` だけ。線引きの理由は `Preset` のコメントを見ること。
    pub presets: Vec<Preset>,
    // 更新の確認。設定ファイルでは [update] になる。プリセットには入れない
    pub update: UpdateSettings,
}

// 設定ファイルから読んだままの形。
//
// AppSettings との違いは hotkeys が Option であることだけ。`None` は
// 「[hotkeys] セクションが無い」を表し、空のマップ（セクションはあるが
// 中身が空）と区別する。この区別が無いと、旧版の設定ファイルを読んだときに
// 既定値の F5 とユーザーが外した状態を見分けられない。
//
// キーを String で受けるのは、知らないアクション名が書かれていても
// ファイル全体のパースを失敗させないため。読めない名前は捨ててログに残す。
//
// **AppSettings に項目を足すときは、ここと From の実装にも足すこと。**
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawAppSettings {
    active_preset: Option<String>,
    video: VideoSettings,
    audio: AudioSettings,
    screenshot: ScreenshotSettings,
    recording: RecordingSettings,
    ui: UiSettings,
    hotkeys: Option<BTreeMap<String, String>>,
    hotkey_settings: HotkeySettings,
    presets: Vec<Preset>,
    update: UpdateSettings,
}

impl From<RawAppSettings> for AppSettings {
    fn from(raw: RawAppSettings) -> Self {
        let RawAppSettings {
            active_preset,
            video,
            audio,
            mut screenshot,
            recording,
            ui,
            hotkeys,
            hotkey_settings,
            presets,
            update,
        } = raw;

        // 旧版の項目はここで読み切って捨てる。保存では書き出さない
        let legacy_hotkey = screenshot.legacy_hotkey.take();
        let hotkeys = migrate_hotkeys(hotkeys, legacy_hotkey);

        // 設定ファイルは手で書き換えられる。名前が無い、あるいは重複した
        // プリセットをそのまま持ち込ませない
        let presets = sanitize_presets(presets);

        let mut settings = Self {
            // 名前の前後の空白は落としてある（sanitize_presets）ので、
            // 選択側も同じ形に揃える。揃えないと空白の有無だけで引けなくなる
            active_preset: active_preset.map(|name| name.trim().to_string()),
            video,
            audio,
            screenshot,
            recording,
            ui,
            hotkeys,
            hotkey_settings,
            presets,
            update,
        };
        // 設定ファイルを手で書き換えて、選択中のプリセットと実際の値を
        // 食い違わせることができる。読んだ時点で辻褄を合わせておく
        settings.refresh_active_preset();
        settings
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            active_preset: None,
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            screenshot: ScreenshotSettings::default(),
            recording: RecordingSettings::default(),
            ui: UiSettings::default(),
            hotkeys: default_hotkeys(),
            hotkey_settings: HotkeySettings::default(),
            presets: Vec::new(),
            update: UpdateSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{without_key, FULL_CONFIG};
    use super::*;

    #[test]
    fn app_settings_missing_one_key_keeps_other_values() {
        // 項目を 1 つ足してリリースした直後に起きる状況。
        // 欠けたキーだけが既定値になり、他の値は保持されなければならない。
        let config = without_key(FULL_CONFIG, "fps");
        assert!(
            !config.contains("fps ="),
            "テスト用の設定から fps が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("fps が欠けていても読めなければならない");

        assert_eq!(settings.video.fps, Some(60)); // 欠けた項目だけ既定値
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.video.format, Some("MJPEG".to_string()));
        assert_eq!(settings.audio.sample_rate, Some(44100));
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_bool_key_keeps_other_values() {
        // bool のように「値が無い＝false」と誤解されやすい型でも、
        // 欠けたときは Default の値（true）に戻ることを確かめる。
        let config = without_key(FULL_CONFIG, "enable_drag_move");
        assert!(
            !config.contains("enable_drag_move ="),
            "テスト用の設定から enable_drag_move が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("enable_drag_move が欠けていても読めなければならない");

        assert!(settings.ui.enable_drag_move); // 既定値は true
        assert!(settings.ui.always_on_top);
        assert!(!settings.ui.maintain_aspect_ratio);
        assert_eq!(settings.ui.last_window_size, Some((800.0, 600.0)));
    }

    #[test]
    fn app_settings_missing_section_keeps_other_sections() {
        // 設定の構造体をまるごと 1 つ足した状況。
        // セクションごと存在しなくても、他のセクションは読めなければならない。
        let config = FULL_CONFIG
            .split("[ui]")
            .next()
            .expect("FULL_CONFIG に [ui] セクションがある")
            .to_string();
        assert!(
            !config.contains("[ui]"),
            "テスト用の設定から [ui] が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("[ui] セクションが欠けていても読めなければならない");

        assert_eq!(settings.ui.volume, 100.0); // UiSettings ごと既定値
        assert!(settings.ui.maintain_aspect_ratio);
        assert_eq!(settings.ui.last_window_size, None);
        assert_eq!(
            settings.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(settings.audio.channels, Some(1));
    }

    #[test]
    fn app_settings_empty_config_uses_all_defaults() {
        // 設定ファイルが空でも既定値で起動できること。
        let settings: AppSettings = toml::from_str("").expect("空の設定でも読めなければならない");

        assert_eq!(settings.video.device_name, None);
        assert_eq!(settings.video.resolution, Some((1280, 720)));
        assert_eq!(settings.video.format, Some("YUY2".to_string()));
        assert_eq!(settings.video.fps, Some(60));
        assert_eq!(settings.audio.sample_rate, Some(48000));
        assert_eq!(settings.audio.channels, Some(2));
        assert!(settings.audio.passthrough_enabled);
        assert_eq!(settings.screenshot.sound_volume, 100.0);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 90);
        // 既存ユーザーの設定にも保存されている値。screenshot_sound::resolve_sound_path が
        // exe の置き場所を基準に解決する前提になっている
        assert_eq!(
            settings.screenshot.sound_file,
            Some(PathBuf::from("sound/SS.mp3"))
        );
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
        assert_eq!(settings.ui.volume, 100.0);
        assert!(settings.ui.maintain_aspect_ratio);
        assert!(!settings.ui.always_on_top);
        assert!(settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_unknown_key_is_ignored() {
        // 新しい版で増えた項目が残った設定ファイルを、古い版で読む場合。
        // 知らないキーで失敗せず、既知の項目が保持されなければならない。
        // [hotkeys] は値が文字列でなければ読めないため、末尾ではなく
        // [ui] の中へ入れる
        let config = FULL_CONFIG.replace(
            "show_stats_overlay = true",
            "show_stats_overlay = true\nfuture_option = true",
        );
        assert!(config.contains("future_option = true"));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らないキーがあっても読めなければならない");

        assert_eq!(settings.ui.volume, 80.0);
        assert!(!settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_roundtrip_preserves_all_values() {
        // 書き出して読み直したときに全項目が保たれること。
        let original: AppSettings =
            toml::from_str(FULL_CONFIG).expect("FULL_CONFIG が読めなければならない");
        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert_eq!(
            restored.video.device_name,
            Some("Capture Device".to_string())
        );
        assert_eq!(restored.video.resolution, Some((1920, 1080)));
        assert_eq!(restored.video.format, Some("MJPEG".to_string()));
        assert_eq!(restored.video.fps, Some(30));
        assert_eq!(
            restored.audio.input_device_name,
            Some("Line In".to_string())
        );
        assert_eq!(
            restored.audio.output_device_name,
            Some("Speakers".to_string())
        );
        assert_eq!(restored.audio.sample_rate, Some(44100));
        assert_eq!(restored.audio.channels, Some(1));
        assert!(!restored.audio.passthrough_enabled);
        assert_eq!(restored.screenshot.destination, ScreenshotDestination::Both);
        assert_eq!(restored.screenshot.save_folder, PathBuf::from(r"C:\shots"));
        assert_eq!(restored.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(restored.screenshot.jpeg_quality, 60);
        assert_eq!(
            restored.screenshot.sound_file,
            Some(PathBuf::from("sound/custom.mp3"))
        );
        assert_eq!(restored.screenshot.sound_volume, 50.0);
        assert_eq!(restored.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(restored.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
        assert_eq!(restored.ui.volume, 80.0);
        assert!(!restored.ui.maintain_aspect_ratio);
        assert_eq!(restored.ui.last_window_size, Some((800.0, 600.0)));
        assert_eq!(restored.ui.last_window_pos, Some((10.0, 20.0)));
        assert!(restored.ui.always_on_top);
        assert!(!restored.ui.enable_drag_move);
    }

    #[test]
    fn settings_error_display_keeps_the_path_and_the_underlying_reason() {
        // 文言はそのままトーストに出る。場所と下位のエラー文が落ちると
        // どのファイルで何が起きたのか分からなくなる
        let missing = SettingsError::FileNotFound(PathBuf::from("C:/tmp/settings.toml"));
        assert_eq!(missing.to_string(), "C:/tmp/settings.toml が見つからない");

        let not_a_file = SettingsError::NotAFile(PathBuf::from("C:/tmp/settings"));
        assert_eq!(not_a_file.to_string(), "C:/tmp/settings はファイルではない");

        let export = SettingsError::ExportFailed {
            path: PathBuf::from("C:/tmp/settings.toml"),
            source: "permission denied".to_string(),
        };
        assert_eq!(
            export.to_string(),
            "C:/tmp/settings.toml へ書き出せない: permission denied"
        );

        let import = SettingsError::ImportFailed {
            path: PathBuf::from("C:/tmp/settings.toml"),
            source: "expected a table".to_string(),
        };
        assert_eq!(
            import.to_string(),
            "C:/tmp/settings.toml を読み込めない: expected a table"
        );
    }

    #[test]
    fn settings_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            SettingsError::FileNotFound(PathBuf::from("C:/tmp/settings.toml")),
            SettingsError::NotAFile(PathBuf::from("C:/tmp/settings")),
            SettingsError::ExportFailed {
                path: PathBuf::from("C:/tmp/settings.toml"),
                source: "denied".to_string(),
            },
            SettingsError::ImportFailed {
                path: PathBuf::from("C:/tmp/settings.toml"),
                source: "broken".to_string(),
            },
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
}
