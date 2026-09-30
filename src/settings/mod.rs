use crate::hotkey::HotkeyAction;
use crate::i18n;
use chrono::Datelike;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

mod audio;
mod hotkeys;
mod preset;
mod recording;
mod screenshot;
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
pub use ui::{LanguageSetting, UiSettings, UpdateSettings, MAX_VOLUME, MIN_VOLUME};
pub use video::{
    ColorRange, ColorSpace, VideoBackendSetting, VideoSettings, MAX_VIDEO_ADJUSTMENT,
    MIN_VIDEO_ADJUSTMENT,
};

/// 設定ファイルの書き出し・読み込みが失敗した理由。
///
/// 対象は「設定を書き出す」「設定を読み込む」の 2 つだけ。`%AppData%` 側の
/// 読み書き（`AppSettings::load` / `save`）は成否を `bool` で扱い、理由は
/// ログにしか出していないのでここを通らない。
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
        };
        f.write_str(&text)
    }
}

impl std::error::Error for SettingsError {}

// confy が設定ファイルの置き場所を決めるのに使う名前。
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

// 設定ファイルをどう読めたか。起動時に既定値を書き戻してよいかの判断に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    // 読み込めた。初回起動で confy が既定値のファイルを作った場合も含む
    Loaded,
    // 読み込めなかったので既定値で起動した。読めなかったファイルは退避済みか、
    // そもそも存在しなかった。どちらもディスクに壊れたファイルは残っていない
    FellBackToDefaults,
    // 読み込めず、退避もできなかった。読めなかったファイルがそのまま残っている
    BrokenFileLeftBehind,
}

impl LoadOutcome {
    // 起動時に既定値を設定ファイルへ書き戻してよいか。
    //
    // 退避できなかった場合だけ false になる。読めなかったファイルがディスクに
    // 残っているため、ここで書き戻すとユーザーが設定を取り戻す最後の手段が消える。
    // 書き戻さなければ壊れたファイルは手元に残り、次回以降も退避を試みられる。
    pub fn may_write_defaults_on_startup(self) -> bool {
        !matches!(self, LoadOutcome::BrokenFileLeftBehind)
    }
}

// 設定の自動保存（デバウンス保存と終了時保存）を許してよいかを持つ。
//
// 読めなかった設定ファイルを退避できなかった場合、ディスクには壊れたファイルが
// そのまま残っている。起動時の書き戻しだけを止めても、ウィンドウを動かせば
// 2 秒後のデバウンス保存が、何もしなくても終了時の保存が、同じファイルを
// 既定値で上書きしてしまう。そのため壊れたファイルが残っている間は
// 自動保存そのものを止める。
//
// 止めている間はウィンドウの位置・サイズや音量も永続化されない。設定を
// 取り戻す手段を残すほうを優先する、という判断。
//
// 設定ダイアログの「適用」「OK」による保存はユーザーの明示的な操作なので
// 止めない。それが成功した時点で壊れたファイルはユーザーの意思で置き換わって
// いるため、以降の自動保存も解禁する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSavePolicy {
    allowed: bool,
}

impl AutoSavePolicy {
    // 設定の読み込み結果から初期状態を決める。
    pub fn from_load_outcome(outcome: LoadOutcome) -> Self {
        Self {
            allowed: !matches!(outcome, LoadOutcome::BrokenFileLeftBehind),
        }
    }

    // 自動保存してよいか。
    pub fn is_allowed(self) -> bool {
        self.allowed
    }

    // 明示的な保存操作の結果を反映する。`saved` は実際に書き出せたか。
    //
    // 失敗した場合に解禁しないのは、壊れたファイルがまだ残っているため。
    // 解禁してしまうと、次のウィンドウ操作で自動保存が走って上書きしうる。
    pub fn note_explicit_save(&mut self, saved: bool) {
        if saved {
            self.allowed = true;
        }
    }
}

// 退避の結果から読み込み結果を決める。
//
// load() 自体は設定ファイルの置き場所（既定は %AppData%）を直接読み書きするためテストできない。
// 判断の部分だけをこの関数に切り出して、退避が失敗した場合を含めて検証する。
fn outcome_from_backup(backup: std::io::Result<Option<PathBuf>>) -> LoadOutcome {
    match backup {
        Ok(_) => LoadOutcome::FellBackToDefaults,
        Err(_) => LoadOutcome::BrokenFileLeftBehind,
    }
}

// 読み込めなかった設定ファイルを退避する。
// 退避できた場合は退避先のパスを返す。元のファイルが無い場合は None を返す。
//
// コピーではなく rename にしているのは、退避したあとに既定値が書き戻されて
// 元のファイルが上書きされ、内容が失われるのを避けるため。
fn backup_broken_config(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let backup_path = next_backup_path(path);
    std::fs::rename(path, &backup_path)?;
    Ok(Some(backup_path))
}

// 退避先のパスを決める。<元のファイル名>.bak を基本とし、
// 既に存在する場合は .bak.1、.bak.2 と連番を足して過去の退避を上書きしない。
fn next_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut candidate = path.with_file_name(format!("{}.bak", file_name));
    let mut counter = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{}.bak.{}", file_name, counter));
        counter += 1;
    }

    candidate
}

impl AppSettings {
    // 設定と、その読み込み結果を返す。
    //
    // 結果を返しているのは、起動時に既定値を書き戻してよいかを呼び出し側が
    // 判断できるようにするため。退避に失敗したまま書き戻すと、読めなかった
    // ファイルを既定値で上書きしてしまい、証跡ごと消える。
    pub fn load() -> (Self, LoadOutcome) {
        // 置き場所は環境変数 CAPTURECARD_VIEWER_CONFIG_DIR で差し替えられる
        // （crate::config_path）。指定が無ければ confy の既定
        let path = match crate::config_path::config_file_path() {
            Ok(path) => path,
            // 設定ファイルの置き場所が分からず、退避を試みることすらできない。
            // 読めなかったファイルが残っている可能性があるため、
            // 書き戻さない側に倒す。
            Err(e) => {
                error!(
                    "設定ファイルの置き場所が分からないため既定値で起動する: {}",
                    e
                );
                return (Self::default(), LoadOutcome::BrokenFileLeftBehind);
            }
        };
        match confy::load_path(&path) {
            Ok(settings) => (settings, LoadOutcome::Loaded),
            Err(e) => {
                error!("設定ファイルを読み込めないため既定値で起動する: {}", e);

                // 読み込みに失敗した設定ファイルは、既定値で起動する前に退避する。
                // 黙って上書きすると、ユーザーが自分の設定を取り戻す手段が無くなる。
                //
                // 失敗したという事実は LoadOutcome として呼び出し側へ渡し、
                // 理由はログに残す。ここは起動直後で UI がまだ無いため、
                // ユーザーへ伝える手段がログしかない。
                let backup = backup_broken_config(&path);
                match &backup {
                    Ok(Some(backup_path)) => warn!(
                        "読み込めなかった設定ファイルを {} へ退避した",
                        backup_path.display()
                    ),
                    // 元のファイルが無い。退避するものが無いだけなので何も言わない
                    Ok(None) => {}
                    Err(e) => error!(
                        "読み込めなかった設定ファイル {} を退避できない: {}",
                        path.display(),
                        e
                    ),
                }
                (Self::default(), outcome_from_backup(backup))
            }
        }
    }

    // 保存できたかを返す。
    //
    // 結果を捨てないのは、デバウンスして書き出す側が失敗を検知して
    // 再試行できるようにするため。失敗を握り潰すと、書けなかった変更が
    // 保存済みとして扱われて消える。
    pub fn save(&self) -> bool {
        let path = match crate::config_path::config_file_path() {
            Ok(path) => path,
            Err(e) => {
                error!("設定の保存に失敗した: {}", e);
                return false;
            }
        };
        match confy::store_path(&path, self) {
            Ok(()) => true,
            Err(e) => {
                error!("設定の保存に失敗した: {}", e);
                false
            }
        }
    }
}

// 書き出す設定ファイルの既定のファイル名。
//
// 日付を入れるのは、同じフォルダへ何度も書き出したときに前回のものを
// 黙って上書きしないため。同じ日に 2 度書き出した場合は、保存ダイアログが
// 上書きの確認を出す。
//
// 時刻を入れないのは、不具合報告に添える用途で名前が長くなりすぎるため。
// 日が変わらないうちの 2 度目は、ユーザーが名前を変えればよい。
pub fn export_file_name(date: &impl Datelike) -> String {
    format!(
        "{}-settings-{:04}{:02}{:02}.toml",
        APP_NAME,
        date.year(),
        date.month(),
        date.day()
    )
}

// 設定を、指定した場所へ TOML として書き出す。
//
// **confy の `store_path` をそのまま使う。** 書式を `%AppData%` の設定ファイルと
// 揃えたいためで、ここだけ別の toml 実装で書くと、confy が書式を変えたときに
// 書き出したファイルを読み戻せない組み合わせが生まれる。
pub fn export_to(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
    confy::store_path(path, settings).map_err(|e| SettingsError::ExportFailed {
        path: path.to_path_buf(),
        source: e.to_string(),
    })
}

// 書き出した設定ファイルを読む。
//
// 読めた場合は `AppSettings` へのパースを通っているので、知らない値は
// 既定へ倒れ、旧版のホットキーも移行済みになっている（`RawAppSettings`）。
//
// **`confy::load_path` はファイルが無いと既定値で新しく作る。** 読み込みの
// つもりで呼んだ結果、選んだ場所に既定値のファイルが増えるのは意図と違うので、
// 先に存在を確かめてから渡す。
//
// 確かめ方に `Path::is_file()` を使わないのは、**実在するのにメタデータを
// 取れない場合も `false` を返す**ため。権限の無いファイルを選んだときに
// 「見つからない」と出すと、置き場所を疑って直しようがなくなる。
pub fn import_from(path: &Path) -> Result<AppSettings, SettingsError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        // ディレクトリやデバイスファイル。confy へ渡しても読めない
        Ok(_) => return Err(SettingsError::NotAFile(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SettingsError::FileNotFound(path.to_path_buf()))
        }
        Err(e) => {
            return Err(SettingsError::ImportFailed {
                path: path.to_path_buf(),
                source: e.to_string(),
            })
        }
    }

    confy::load_path(path).map_err(|e| SettingsError::ImportFailed {
        path: path.to_path_buf(),
        source: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::testing::{without_key, FULL_CONFIG, LEGACY_CONFIG};
    use super::*;
    use chrono::NaiveDate;
    use std::fs;
    use tempfile::tempdir;

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
        // 既存ユーザーの設定にも保存されている値。screenshot::resolve_sound_path が
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
    fn backup_broken_config_moves_file_and_returns_path() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "[video] 壊れている").expect("テスト用の設定を書けること");

        let backup = backup_broken_config(&path)
            .expect("退避に成功すること")
            .expect("退避先のパスが返ること");

        assert_eq!(backup, dir.path().join("default-config.toml.bak"));
        assert!(!path.exists(), "退避後に元のファイルが残っている");
        assert_eq!(
            fs::read_to_string(&backup).expect("退避先を読めること"),
            "[video] 壊れている"
        );
    }

    #[test]
    fn backup_broken_config_existing_backup_gets_numbered_suffix() {
        // 続けて壊れた場合に、前回退避した内容を上書きしないこと。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        fs::write(&path, "1 回目").expect("テスト用の設定を書けること");
        let first = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "2 回目").expect("テスト用の設定を書けること");
        let second = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "3 回目").expect("テスト用の設定を書けること");
        let third = backup_broken_config(&path).unwrap().unwrap();

        assert_eq!(first, dir.path().join("default-config.toml.bak"));
        assert_eq!(second, dir.path().join("default-config.toml.bak.1"));
        assert_eq!(third, dir.path().join("default-config.toml.bak.2"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "1 回目");
        assert_eq!(fs::read_to_string(&second).unwrap(), "2 回目");
        assert_eq!(fs::read_to_string(&third).unwrap(), "3 回目");
    }

    #[test]
    fn outcome_from_backup_backup_failed_forbids_writing_defaults() {
        // 退避に失敗した場合。読めなかったファイルがディスクに残っているため、
        // 起動時に既定値を書き戻してはならない。書き戻すとユーザーが設定を
        // 取り戻す最後の手段が消える。
        let failed = Err(std::io::Error::other("退避に失敗した"));

        let outcome = outcome_from_backup(failed);

        assert_eq!(outcome, LoadOutcome::BrokenFileLeftBehind);
        assert!(!outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_backup_succeeded_allows_writing_defaults() {
        // 退避できた場合。元のファイルは .bak として残っているため、
        // 既定値を書き戻してよい。
        let backed_up = Ok(Some(PathBuf::from("default-config.toml.bak")));

        let outcome = outcome_from_backup(backed_up);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_nothing_to_back_up_allows_writing_defaults() {
        // 退避するファイルがそもそも無かった場合。
        // 潰す相手がいないので、既定値を書き戻してよい。
        let nothing = Ok(None);

        let outcome = outcome_from_backup(nothing);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn auto_save_policy_broken_file_left_behind_blocks_autosave() {
        // 退避できなかった場合。ウィンドウを動かすか終了するだけで
        // 壊れたファイルが既定値で潰れるのを防ぐため、自動保存を止める
        let policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_loaded_allows_autosave() {
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded).is_allowed());
    }

    #[test]
    fn auto_save_policy_fell_back_to_defaults_allows_autosave() {
        // 退避できていれば元の内容は .bak に残っている。守る相手がいないので
        // ウィンドウ位置や音量を通常どおり保存してよい
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::FellBackToDefaults).is_allowed());
    }

    #[test]
    fn auto_save_policy_successful_explicit_save_unblocks_autosave() {
        // 設定画面の「適用」「OK」で保存できた時点で、壊れたファイルは
        // ユーザーの意思で置き換わっている。以降は自動保存を止めない
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(true);

        assert!(policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_keeps_autosave_blocked() {
        // 保存に失敗した場合は壊れたファイルがまだ残っているため、
        // 止めたままにする
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(false);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_does_not_block_allowed_policy() {
        // もともと許可されている状態は、保存の失敗で止まらない。
        // 一時的な書き込み失敗で以降の保存が全部止まると、
        // 復旧したあとも設定が残らなくなる
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded);

        policy.note_explicit_save(false);

        assert!(policy.is_allowed());
    }

    #[test]
    fn load_outcome_loaded_allows_writing_defaults() {
        // 正常に読めた場合。通常どおり保存してよい。
        assert!(LoadOutcome::Loaded.may_write_defaults_on_startup());
    }

    #[test]
    fn backup_broken_config_missing_file_returns_none() {
        // 初回起動のように設定ファイルがまだ無い場合。退避するものが無いだけで、
        // エラーとして扱わない。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        let backup = backup_broken_config(&path).expect("失敗しないこと");

        assert!(backup.is_none());
    }

    #[test]
    fn export_file_name_uses_the_given_date() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260921.toml"
        );
    }

    #[test]
    fn export_file_name_pads_single_digit_month_and_day() {
        // 境界。0 埋めを忘れると 2026-1-5 が 202615 になり、並べ替えが崩れる
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260105.toml"
        );
    }

    #[test]
    fn export_to_then_import_from_restores_the_values() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        let mut settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Up".to_string()));

        export_to(&path, &settings).expect("書き出せること");
        let imported = import_from(&path).expect("読み戻せること");

        assert_eq!(imported.video.device_name, settings.video.device_name);
        assert_eq!(imported.video.resolution, settings.video.resolution);
        assert_eq!(imported.audio.sample_rate, settings.audio.sample_rate);
        assert_eq!(imported.screenshot.format, settings.screenshot.format);
        assert_eq!(
            imported.screenshot.jpeg_quality,
            settings.screenshot.jpeg_quality
        );
        assert_eq!(imported.ui.volume, settings.ui.volume);
        assert_eq!(imported.hotkeys, settings.hotkeys);
    }

    #[test]
    fn import_from_missing_file_returns_error_without_creating_it() {
        // confy の load_path はファイルが無いと既定値で作ってしまう。
        // 読み込みのつもりで選んだ場所にファイルが増えないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("missing.toml");

        let err = import_from(&path).expect_err("エラーになること");

        assert_eq!(err, SettingsError::FileNotFound(path.clone()));
        assert!(!path.exists());
    }

    #[test]
    fn import_from_broken_toml_returns_error() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("broken.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書けること");

        let err = import_from(&path).expect_err("エラーになること");

        // ファイルはあるので「見つからない」ではなく解釈の失敗として返ること。
        // 区別が付かないと、ユーザーは置き場所を疑って直しようがなくなる
        assert!(
            matches!(err, SettingsError::ImportFailed { .. }),
            "解釈の失敗として返ること: {err:?}"
        );
    }

    #[test]
    fn import_from_a_directory_is_not_reported_as_missing() {
        // 実在するのに「見つからない」と出すと、置き場所を疑って直しようがない。
        // is_file() だけで判定していたころはここが FileNotFound になっていた
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("not-a-file");
        fs::create_dir(&path).expect("ディレクトリを作れること");

        let err = import_from(&path).expect_err("エラーになること");

        assert_eq!(err, SettingsError::NotAFile(path));
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

    #[test]
    fn import_from_unknown_values_falls_back_to_defaults() {
        // 手で書き換えたファイルを読み込んだ場合。解釈できない値だけが
        // 既定へ倒れ、他の項目は読めていること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("odd.toml");
        fs::write(
            &path,
            r#"
[video]
fps = 30
color_space = "bt2020"

[screenshot]
format = "webp"
jpeg_quality = 500
"#,
        )
        .expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.video.fps, Some(30));
        assert_eq!(imported.video.color_space, ColorSpace::Auto);
        assert_eq!(imported.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(imported.screenshot.jpeg_quality, MAX_JPEG_QUALITY);
    }

    #[test]
    fn import_from_legacy_file_migrates_the_hotkey() {
        // 旧版が書き出したファイルを読み込んだ場合も、通常の起動と同じく
        // screenshot.hotkey が hotkeys へ移ること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("legacy.toml");
        fs::write(&path, LEGACY_CONFIG).expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
    }
}
