use crate::hotkey::HotkeyAction;
use crate::i18n::{self, Text};
use chrono::Datelike;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

mod audio;
mod screenshot;
#[cfg(test)]
mod testing;
mod video;

pub use screenshot::{
    ScreenshotDestination, ScreenshotEncoding, ScreenshotFormat, ScreenshotSettings,
    DEFAULT_SOUND_FILE, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY,
};

pub use audio::{
    AudioSettings, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MAX_BUFFER_MS, MIN_BUFFER_MS,
};
// 外からはテスト（`app::worker_loop` のテスト用の組み立て）からしか使わない。
// テストを含まないビルドで再輸出すると誰も使わない `pub use` になり
// `unused_imports` の警告になるので、テストのときだけ出す
#[cfg(test)]
pub use audio::DEFAULT_BUFFER_MS;
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

// 設定ファイルから読んだプリセットの一覧を、扱える形に整える。
//
// 名前の前後の空白を落とし、名前が空のものと、既出の名前と重なるものを捨てる。
// **ここで弾かないと、設定ダイアログの `validate_preset_name` を通さずに
// 不正な状態を作れる。** 空の名前はメニューに空の項目として並び、重複した
// 名前は `preset()` も `remove_preset` も先頭しか見ないため、2 つ目以降を
// 選ぶことも消すこともできなくなる。
//
// 捨てるだけでエラーにはしない。他の項目と同じで、読めないところだけを
// 落として残りは活かす。
fn sanitize_presets(presets: Vec<Preset>) -> Vec<Preset> {
    let mut kept: Vec<Preset> = Vec::with_capacity(presets.len());
    for preset in presets {
        let name = preset.name.trim();
        if name.is_empty() {
            warn!("設定のプリセットに名前が無いので無視する");
            continue;
        }
        if kept.iter().any(|existing| existing.name == name) {
            warn!(
                "設定に同じ名前のプリセット \"{}\" が複数あるので後のほうを無視する",
                name
            );
            continue;
        }
        kept.push(Preset {
            name: name.to_string(),
            ..preset
        });
    }
    kept
}

// 既定のホットキー割り当て。
//
// **スクリーンショット以外は既定で未割り当てにしてある。** ホットキーは
// 既定で他のアプリを操作している間も反応するため、こちらから勝手に
// F11 や Ctrl+↑ のような一般的なキーを割り当てると、他のアプリでそのキーを
// 押すたびにこちらも動いてしまう。
fn default_hotkeys() -> BTreeMap<HotkeyAction, String> {
    BTreeMap::from([(HotkeyAction::Screenshot, "F5".to_string())])
}

// 設定ファイルの [hotkeys] と、旧版の screenshot.hotkey から、実際に使う
// 割り当てを決める。
//
// - [hotkeys] がある（新しい版が書いた）: そのまま使う。旧版の項目は無視する
// - [hotkeys] が無い（旧版が書いた）: 既定値を土台に、screenshot.hotkey が
//   あればスクリーンショットへ移す
//
// 旧版の設定ファイルで screenshot.hotkey が欠けている場合は既定の F5 になる。
// 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった（項目ごと
// 消えるため、欠けた項目と区別できない）ので、そこは従来どおりの挙動に揃えてある。
fn migrate_hotkeys(
    table: Option<BTreeMap<String, String>>,
    legacy_hotkey: Option<String>,
) -> BTreeMap<HotkeyAction, String> {
    let Some(table) = table else {
        let mut hotkeys = default_hotkeys();
        if let Some(hotkey) = legacy_hotkey {
            info!(
                "旧版の設定にあるスクリーンショットのホットキー {} を hotkeys へ移す",
                hotkey
            );
            hotkeys.insert(HotkeyAction::Screenshot, hotkey);
        }
        return hotkeys;
    };

    let mut hotkeys = BTreeMap::new();
    for (key, hotkey) in table {
        match HotkeyAction::from_key(&key) {
            Some(action) => {
                hotkeys.insert(action, hotkey);
            }
            // 新しい版が増やしたアクションを古い版で読んだ場合など。
            // ここでエラーにすると設定ファイル全体が読めなくなる
            None => warn!(
                "設定の hotkeys にある知らないアクション \"{}\" を無視する",
                key
            ),
        }
    }
    hotkeys
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

// 名前付きプリセット。
//
// **中身は `video` と `audio` だけ。** `screenshot` / `hotkeys` / `ui` は
// 入れていない。プリセットの用途は「複数のキャプチャボードの使い分け」と
// 「低遅延優先 / 画質優先の切替」で、どちらも映像と音声の取り込み方の話。
// 切り替えたらスクリーンショットの保存先やホットキーまで変わるほうが
// 驚きが大きく、「プリセットを切り替えたらホットキーが効かなくなった」
// という迷い方をさせる。
//
// `video.auto_reconnect` だけは `video` の中にありながら対象外。理由は
// `apply_to` のコメントを見ること。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    // **TOML のテーブルより前に置くこと。** 理由は AppSettings の
    // active_preset と同じ
    pub name: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

impl Preset {
    // いまの設定からプリセットを作る。
    pub fn from_settings(name: String, settings: &AppSettings) -> Self {
        let mut video = settings.video.clone();
        // 対象外の項目は既定値で固定する。現在値を書き込むと、設定ファイルを
        // 読んだ人に「プリセットで自動再接続が切り替わる」と読めてしまう
        video.auto_reconnect = VideoSettings::default().auto_reconnect;
        Self {
            name,
            video,
            audio: settings.audio.clone(),
        }
    }

    // プリセットの内容を設定へ写す。**`video` と `audio` 以外は触らない。**
    //
    // `video.auto_reconnect` も写さない。右クリックメニューだけで切り替える
    // 項目で、設定ダイアログにもプリセットの管理 UI にも出てこない。
    // 含めると次の 2 つが起きる。
    //   - プリセットを読み込んだだけで自動再接続が勝手に切り替わる
    //   - 自動再接続を切り替えただけでプリセットが「（変更あり）」になる
    // どちらもプリセットの目的と関係がない。
    pub fn apply_to(&self, settings: &mut AppSettings) {
        let auto_reconnect = settings.video.auto_reconnect;
        settings.video = self.video.clone();
        settings.video.auto_reconnect = auto_reconnect;
        settings.audio = self.audio.clone();
    }
}

// プリセット名として受け付けられない理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetNameError {
    // 空、または空白だけ
    Empty,
    // 同じ名前のプリセットが既にある
    Duplicate,
}

impl PresetNameError {
    // 設定ダイアログに出す文言。
    pub fn message(self) -> &'static str {
        match self {
            PresetNameError::Empty => Text::PresetNameEmpty.get(),
            PresetNameError::Duplicate => Text::PresetNameDuplicate.get(),
        }
    }
}

// プリセット名として使えるかを調べる。使えるなら前後の空白を落とした名前を返す。
//
// **名前の重複を許さない。** 切替は名前で行うので、同じ名前が 2 つあると
// どちらが選ばれるかがリストの並び順に依存する。
//
// `replacing` には上書き対象の名前を渡す。同じ名前のまま上書き保存するときに、
// 自分自身との重複で弾かないため。新規保存では `None` を渡す。
pub fn validate_preset_name(
    name: &str,
    presets: &[Preset],
    replacing: Option<&str>,
) -> Result<String, PresetNameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(PresetNameError::Empty);
    }

    let taken = presets
        .iter()
        .any(|preset| preset.name == trimmed && Some(preset.name.as_str()) != replacing);
    if taken {
        return Err(PresetNameError::Duplicate);
    }

    Ok(trimmed.to_string())
}

// 設定の `video` / `audio` がプリセットと一致するか。
//
// 一致の判定と適用（`Preset::apply_to`）は同じ項目を見ていなければならない。
// 適用しない `auto_reconnect` をここで比べると、読み込んだ直後から
// 「（変更あり）」になる。
pub fn matches_preset(preset: &Preset, settings: &AppSettings) -> bool {
    let mut video = settings.video.clone();
    video.auto_reconnect = preset.video.auto_reconnect;
    video == preset.video && settings.audio == preset.audio
}

// いま実際に効いているプリセットの名前。
//
// `active_preset` に名前が入っていても、そのプリセットが消えていたり、
// 読み込んだあとで手作業で値を変えていれば `None` を返す。**この関数が
// 「（変更あり）」表示の判定そのもの。**
pub fn resolved_active_preset(settings: &AppSettings) -> Option<&str> {
    let name = settings.active_preset.as_deref()?;
    let preset = settings.presets.iter().find(|preset| preset.name == name)?;
    if matches_preset(preset, settings) {
        Some(preset.name.as_str())
    } else {
        None
    }
}

// 画面に出す言語の設定。設定ファイルには language = "auto" / "ja" / "en" と書かれる。
//
// 実際に使う言語（`i18n::Language`）とは別の型にしてある。こちらは
// 「自動」を持ち、OS の言語と合わせて初めて 1 つに決まるため
// （`resolve`、`docs/design/i18n.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageSetting {
    // 既存ユーザーの設定ファイルには language が無い。自動にしておけば、
    // 日本語の Windows ではこれまでどおり日本語で出る
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "en")]
    English,
}

impl LanguageSetting {
    // 設定ダイアログのコンボボックスに出す表示名
    pub fn label(self) -> &'static str {
        match self {
            LanguageSetting::Auto => Text::LanguageAuto.get(),
            LanguageSetting::Japanese => Text::LanguageJapanese.get(),
            LanguageSetting::English => Text::LanguageEnglish.get(),
        }
    }

    pub const ALL: [LanguageSetting; 3] = [
        LanguageSetting::Auto,
        LanguageSetting::Japanese,
        LanguageSetting::English,
    ];

    // 実際に使う言語を決める。`os_language` は起動時に 1 回だけ OS から
    // 推定したもの（`platform::os_ui_language`）
    pub fn resolve(self, os_language: i18n::Language) -> i18n::Language {
        match self {
            LanguageSetting::Auto => os_language,
            LanguageSetting::Japanese => i18n::Language::Japanese,
            LanguageSetting::English => i18n::Language::English,
        }
    }
}

// 設定ファイルの language に知らない値が書かれていても、設定全体を
// 失わせない。色空間と同じ考え方で、自動として扱う
fn deserialize_language<'de, D>(deserializer: D) -> Result<LanguageSetting, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(language_setting_from_str(&raw).unwrap_or_else(|| {
        warn!("設定の言語 \"{}\" を解釈できないので自動として扱う", raw);
        LanguageSetting::default()
    }))
}

fn language_setting_from_str(raw: &str) -> Option<LanguageSetting> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(LanguageSetting::Auto),
        "ja" | "japanese" => Some(LanguageSetting::Japanese),
        "en" | "english" => Some(LanguageSetting::English),
        _ => None,
    }
}

// 録画の設定。設定ファイルでは [recording] になる（`docs/design/recording.md` の
// 「設定 `[recording]`」）。
//
// **項目は、その項目が効く段で足す。** 効かない項目を先に出さない
// （`docs/ARCHITECTURE.md` の「設定は実際に効かせる」）。
// 録画中に変えた設定は次の録画から効く。リプレイバッファの ON / OFF とさかのぼる長さは
// すぐ効く（ON にしたらその時点から溜め始める）。ただし、リプレイバッファを通さない録画の
// 最中に ON にしたときは、差し込み口が空くその録画の終わりから溜め始める。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    // 保存先のフォルダ。無ければ録画の開始時に作る
    pub folder: PathBuf,
    // ファイル名の書式（chrono の strftime）。拡張子（.mp4）は付けない。
    // 使う前に `recording::resolve_file_stem` が検め、使えなければ既定へ倒す
    pub file_name_format: String,
    // 映像の平均ビットレート（kbps）
    #[serde(deserialize_with = "deserialize_recording_bitrate")]
    pub video_bitrate_kbps: u32,
    // ハードウェアのエンコーダ（Intel / NVIDIA / AMD の MFT）を選ばせるか。
    // 選べなければソフトウェアのエンコーダへ倒れる
    pub hardware_encoder: bool,
    // 音声（AAC）も録るか。録るのは入力の音そのもので、音量・ミュート・
    // パススルーの無効は効かない（`docs/design/recording.md`）
    pub audio_enabled: bool,
    // 音声の平均ビットレート（kbps）。Microsoft の AAC エンコーダが受け付ける
    // RECORDING_AUDIO_BITRATES_KBPS の 4 つだけ。それ以外は近いものへ寄せる
    #[serde(deserialize_with = "deserialize_recording_audio_bitrate")]
    pub audio_bitrate_kbps: u32,
    // リプレイバッファ（さかのぼり録画、#182）。ON のあいだはエンコーダを常に回し、
    // 直近 replay_seconds 秒ぶんのエンコード済みの映像と音声をメモリに持つ。
    // 録画を始めると、その分を先頭に含める
    pub replay_enabled: bool,
    // さかのぼる長さ（秒）。MIN_REPLAY_SECONDS〜MAX_REPLAY_SECONDS（上限 5 分は #182 の決定）
    #[serde(deserialize_with = "deserialize_replay_seconds")]
    pub replay_seconds: u32,
}

// 録画のファイル名の既定の書式
pub const DEFAULT_RECORDING_FILE_NAME_FORMAT: &str = "Recording_%Y-%m-%d_%H-%M-%S";

// 録画の映像のビットレート（kbps）の下限・上限と既定値
pub const MIN_RECORDING_BITRATE_KBPS: u32 = 1_000;
pub const MAX_RECORDING_BITRATE_KBPS: u32 = 50_000;
pub const DEFAULT_RECORDING_BITRATE_KBPS: u32 = 8_000;

// 録画の音声（AAC）のビットレート（kbps）の選択肢と既定値。
// Microsoft の AAC エンコーダが受け付けるのはこの 4 つだけ
// （`MF_MT_AUDIO_AVG_BYTES_PER_SECOND` = 12000 / 16000 / 20000 / 24000）
pub const RECORDING_AUDIO_BITRATES_KBPS: [u32; 4] = [96, 128, 160, 192];
pub const DEFAULT_RECORDING_AUDIO_BITRATE_KBPS: u32 = 160;

// リプレイバッファのさかのぼる長さ（秒）の下限・上限と既定値。
// 上限の 5 分は #182 の決定（8Mbps + 160kbps で約 300MB のメモリを使う）
pub const MIN_REPLAY_SECONDS: u32 = 5;
pub const MAX_REPLAY_SECONDS: u32 = 300;
pub const DEFAULT_REPLAY_SECONDS: u32 = 30;

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            folder: default_recording_folder(),
            file_name_format: DEFAULT_RECORDING_FILE_NAME_FORMAT.to_string(),
            video_bitrate_kbps: DEFAULT_RECORDING_BITRATE_KBPS,
            hardware_encoder: true,
            audio_enabled: true,
            audio_bitrate_kbps: DEFAULT_RECORDING_AUDIO_BITRATE_KBPS,
            replay_enabled: false,
            replay_seconds: DEFAULT_REPLAY_SECONDS,
        }
    }
}

impl RecordingSettings {
    // エンコーダへ渡すビットレート。設定ダイアログからは範囲外を作れないが、
    // 読み込み後に値を差し替える経路もあるので、渡す前に丸めておく
    pub fn clamped_bitrate_kbps(&self) -> u32 {
        self.video_bitrate_kbps
            .clamp(MIN_RECORDING_BITRATE_KBPS, MAX_RECORDING_BITRATE_KBPS)
    }

    // 録画スレッドへ渡す音声のビットレート。音声を録らないなら None。
    // 4 つの選択肢のどれかへ寄せてから渡す（理由は clamped_bitrate_kbps と同じ）
    pub fn audio_bitrate_for_recording(&self) -> Option<u32> {
        self.audio_enabled
            .then(|| nearest_audio_bitrate_kbps(i64::from(self.audio_bitrate_kbps)))
    }

    // 録画スレッドへ渡すさかのぼる長さ。リプレイバッファが OFF なら None。
    // 範囲に丸めてから渡す（理由は clamped_bitrate_kbps と同じ）
    pub fn replay_seconds_for_recording(&self) -> Option<u32> {
        self.replay_enabled.then(|| {
            self.replay_seconds
                .clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS)
        })
    }
}

// 範囲外のさかのぼる長さが書かれていても、設定全体を失わせない。
// 考え方は deserialize_recording_bitrate と同じ。
fn deserialize_replay_seconds<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_REPLAY_SECONDS), i64::from(MAX_REPLAY_SECONDS));
    if clamped != raw {
        warn!(
            "設定のリプレイバッファのさかのぼる長さ {} 秒は範囲外なので {} 秒として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

// RECORDING_AUDIO_BITRATES_KBPS のうち `kbps` に最も近いもの。
// ちょうど中間（例: 112）なら高いほうへ寄せる（音質を落とさない側）
pub fn nearest_audio_bitrate_kbps(kbps: i64) -> u32 {
    RECORDING_AUDIO_BITRATES_KBPS
        .iter()
        .copied()
        .min_by_key(|&candidate| {
            let distance = (i64::from(candidate) - kbps).unsigned_abs();
            // 距離が同じなら高いほうを先にする
            (distance, std::cmp::Reverse(candidate))
        })
        .unwrap_or(DEFAULT_RECORDING_AUDIO_BITRATE_KBPS)
}

// 選択肢に無い音声のビットレートが書かれていても、設定全体を失わせない。
// 近い選択肢へ寄せる。考え方は deserialize_recording_bitrate と同じ。
fn deserialize_recording_audio_bitrate<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let nearest = nearest_audio_bitrate_kbps(raw);
    if i64::from(nearest) != raw {
        warn!(
            "設定の録画の音声のビットレート {} kbps は選べないので {} kbps として扱う",
            raw, nearest
        );
    }
    Ok(nearest)
}

// 範囲外のビットレートが書かれていても、設定全体を失わせない。
// 考え方は deserialize_jpeg_quality と同じで、TOML の整数である i64 で受けてから丸める。
fn deserialize_recording_bitrate<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(
        i64::from(MIN_RECORDING_BITRATE_KBPS),
        i64::from(MAX_RECORDING_BITRATE_KBPS),
    );
    if clamped != raw {
        warn!(
            "設定の録画のビットレート {} kbps は範囲外なので {} kbps として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

// 録画の保存先の既定値。
//
// ビデオフォルダ → デスクトップ → %USERPROFILE% → 実行ファイルの置き場所 → 一時フォルダ。
// **カレントディレクトリは使わない**（理由は default_screenshot_folder と同じ。
// `docs/design/assets.md`）。先頭の候補だけがスクリーンショットと違う。
fn default_recording_folder() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    recording_folder_from(
        dirs::video_dir(),
        dirs::desktop_dir(),
        dirs::home_dir(),
        exe_dir,
        std::env::temp_dir(),
    )
}

// 録画の保存先の候補から実際に使うものを選ぶ。選ぶ部分だけを切り出してテストする。
fn recording_folder_from(
    videos: Option<PathBuf>,
    desktop: Option<PathBuf>,
    home: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    last_resort: PathBuf,
) -> PathBuf {
    if let Some(videos) = videos {
        return videos;
    }
    let fallback = desktop.or(home).or(exe_dir).unwrap_or(last_resort);
    warn!(
        "ビデオフォルダの場所が分からないので、録画の保存先を {} にする",
        fallback.display()
    );
    fallback
}

// 音量の下限と上限。100% が等倍で、そこから先は増幅になる。
// UI（スライダー・ホイール）と OSD の表示もこの範囲を前提にしている
pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 200.0;

// 音量の既定値。等倍
pub const DEFAULT_VOLUME: f32 = 100.0;

// 範囲外の音量が書かれていても、そのまま受け取らない。
// UI からは 0〜200% しか作れないが、設定ファイルは手で書き換えられる。
// 1000% が書かれていると OSD に「音量: 1000%」と出てしまう
// （音声側は AudioCapture::set_volume が別途 0〜2.0 に丸めている）。
//
// jpeg_quality と違い、範囲外でもパース自体は成功するので設定が失われる
// わけではない。表示と実際の音量を食い違わせないために丸めている。
//
// NaN と無限大は clamp では落ちない（NaN.clamp(..) は NaN を返す）ため、
// 先に既定値へ倒す。
fn deserialize_volume<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = f32::deserialize(deserializer)?;
    if !raw.is_finite() {
        warn!(
            "設定の音量 {} は数値として扱えないので {}% として扱う",
            raw, DEFAULT_VOLUME
        );
        return Ok(DEFAULT_VOLUME);
    }

    let clamped = raw.clamp(MIN_VOLUME, MAX_VOLUME);
    if clamped != raw {
        warn!("設定の音量 {} は範囲外なので {} として扱う", raw, clamped);
    }
    Ok(clamped)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    #[serde(deserialize_with = "deserialize_volume")]
    pub volume: f32,
    // ミュート中か。**音量とは別に持つ。** 音量 0% で代用すると、解除したときに
    // 戻すべき値が残らない。設定ダイアログには出さず、右クリックメニュー・
    // ミドルクリック・ホットキーだけで切り替える
    pub muted: bool,
    pub maintain_aspect_ratio: bool,
    pub last_window_size: Option<(f32, f32)>,
    pub last_window_pos: Option<(f32, f32)>,
    pub always_on_top: bool,
    pub enable_drag_move: bool,
    // 映像の上に FPS などの統計を重ねて出すか
    pub show_stats_overlay: bool,
    // タイトルバーと枠を消すか。デュアルモニタでサブウィンドウとして置くときに
    // 装飾が邪魔になるため。設定ダイアログには出さず、右クリックメニューだけで
    // 切り替える。**有効にすると × が無くなる** ので、右クリックメニューの
    // 「終了」と Alt+F4 が閉じる手段になる
    pub borderless: bool,
    // 画面に出す言語。設定ダイアログの「その他」タブで選び、「適用」で
    // 再起動せずに切り替わる。プリセットには入れない（`docs/design/presets.md`）
    #[serde(deserialize_with = "deserialize_language")]
    pub language: LanguageSetting,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            volume: DEFAULT_VOLUME,
            // 既定は音が出る状態にする
            muted: false,
            maintain_aspect_ratio: true,
            last_window_size: None,
            last_window_pos: None,
            always_on_top: false,
            enable_drag_move: true,
            // 常時出しているものではないので、既定は非表示にする
            show_stats_overlay: false,
            // 既定はタイトルバーありにする。装飾なしは閉じ方・動かし方が
            // 通常のウィンドウと変わるので、知らずにその状態で起動させない
            borderless: false,
            language: LanguageSetting::Auto,
        }
    }
}

// ホットキーの割り当て以外の設定（「ホットキー」タブの下の段）。
//
// プリセットには入れない（`Preset` のコメント）。既定値は全て偽なので
// `Default` は導出で足りる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    // このアプリにキーボードフォーカスがあるときだけ反応するか。
    //
    // 既定はオフ。他のアプリを操作している間も、最小化している間も
    // 反応する（#133）。オンにすると、他のアプリで同じキーを使っていても
    // こちらは動かない（#202）。どちらの場合もキーは奪わず、他のアプリにも届く
    pub only_when_focused: bool,
}

// 更新の確認（「その他」タブの「更新」の欄）。`docs/design/update.md`。
//
// プリセットには入れない（`Preset` のコメント）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    // 起動時に GitHub へ新しい版を問い合わせるか。切っていても、
    // 「その他」タブの「更新を確認」からは確認できる
    pub check_on_startup: bool,
    // 起動時の確認で新しい版が見つかったとき、ダイアログで知らせるか。
    // 切っていると「その他」タブの「更新」の欄に出るだけ
    pub notify_on_startup: bool,
    // 「この版は通知しない」を選んだ版（`1.2.0` の形。`v` は付けない）。
    // この版のあいだは起動時のダイアログを出さない。もっと新しい版が出たら出す
    pub skipped_version: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            notify_on_startup: true,
            skipped_version: None,
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

    // アクションに割り当てられたホットキー。未割り当てなら None。
    pub fn hotkey(&self, action: HotkeyAction) -> Option<&str> {
        self.hotkeys.get(&action).map(String::as_str)
    }

    // アクションのホットキーを差し替える。`None` は割り当ての解除。
    //
    // 解除をキーの削除で表すのは、空文字と「未割り当て」を混ぜないため。
    // 空文字を入れるとパースに失敗して、毎回ログへ理由が出ることになる。
    pub fn set_hotkey(&mut self, action: HotkeyAction, hotkey: Option<String>) {
        match hotkey {
            Some(hotkey) => {
                self.hotkeys.insert(action, hotkey);
            }
            None => {
                self.hotkeys.remove(&action);
            }
        }
    }

    // 名前でプリセットを引く。
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.name == name)
    }

    // プリセットを適用する。**`video` と `audio` だけが変わる。**
    // 見つからなければ何もせず `false` を返す。
    pub fn apply_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self.preset(name).cloned() else {
            return false;
        };
        preset.apply_to(self);
        self.active_preset = Some(preset.name);
        true
    }

    // プリセットを追加、または同じ名前のものを上書きする。
    //
    // 上書きしたものが選択中なら、そのまま選択中のままにする。中身は
    // いまの設定から作ったものなので、一致したままになる。
    pub fn upsert_preset(&mut self, preset: Preset) {
        match self
            .presets
            .iter_mut()
            .find(|existing| existing.name == preset.name)
        {
            Some(existing) => *existing = preset,
            None => self.presets.push(preset),
        }
    }

    // プリセットを削除する。消せたら `true`。
    //
    // 選択中のものを消したときは選択も外す。名前だけが残っても、引ける
    // プリセットが無いので「なし」と同じ意味になる。
    pub fn remove_preset(&mut self, name: &str) -> bool {
        let Some(index) = self.presets.iter().position(|preset| preset.name == name) else {
            return false;
        };
        self.presets.remove(index);
        if self.active_preset.as_deref() == Some(name) {
            self.active_preset = None;
        }
        true
    }

    // 選択中のプリセットが実際の値と食い違っていれば選択を外す。
    //
    // **`video` / `audio` を書き換えたあとに呼ぶこと。** 呼ばないと、
    // プリセットを読み込んだあと設定ダイアログで解像度を変えても
    // 「プリセット: 低遅延優先」のままになる。
    pub fn refresh_active_preset(&mut self) {
        if resolved_active_preset(self).is_none() {
            self.active_preset = None;
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
    use super::testing::{without_key, DESKTOP, EXE_DIR, FULL_CONFIG, HOME, LEGACY_CONFIG, TEMP};
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
    fn app_settings_missing_muted_defaults_to_unmuted() {
        // ミュートの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、音が出る状態で起動すること
        let config = without_key(FULL_CONFIG, "muted");
        assert!(
            !config.contains("muted ="),
            "テスト用の設定から muted が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("muted が欠けていても読めなければならない");

        assert!(!settings.ui.muted); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
    }

    #[test]
    fn app_settings_muted_is_read_and_kept_apart_from_volume() {
        // ミュートは音量とは別の項目。読み込みで音量へ潰れてはいけない
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.muted);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_show_stats_overlay_defaults_to_hidden() {
        // 情報表示の項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、既定の非表示になること。
        let config = without_key(FULL_CONFIG, "show_stats_overlay");
        assert!(
            !config.contains("show_stats_overlay ="),
            "テスト用の設定から show_stats_overlay が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("show_stats_overlay が欠けていても読めなければならない");

        assert!(!settings.ui.show_stats_overlay); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(!settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_missing_borderless_defaults_to_decorated() {
        // 装飾なしの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、タイトルバーありで起動すること。
        // **既定が true になると、更新しただけで × が消えたウィンドウが出てくる。**
        let config = without_key(FULL_CONFIG, "borderless");
        assert!(
            !config.contains("borderless ="),
            "テスト用の設定から borderless が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("borderless が欠けていても読めなければならない");

        assert!(!settings.ui.borderless); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(settings.ui.show_stats_overlay);
    }

    #[test]
    fn app_settings_borderless_is_read_from_the_file() {
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.borderless);
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
    fn app_settings_volume_above_maximum_is_clamped() {
        // 手で書き換えた設定ファイル。UI からは作れない値でも読めてしまうので、
        // 表示と実際の音量が食い違わないよう上限で止める
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 1000.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 200.0);
    }

    #[test]
    fn app_settings_volume_below_minimum_is_clamped() {
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = -50.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 0.0);
    }

    #[test]
    fn app_settings_volume_not_a_number_falls_back_to_default() {
        // TOML は nan / inf をそのまま書ける。clamp では落とせないので
        // 既定値へ倒していること
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = nan\n").expect("nan でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = inf\n").expect("inf でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);
    }

    #[test]
    fn app_settings_volume_at_bounds_is_kept() {
        // 境界。丸めが 1 段ずれて端の値が使えなくなっていないこと
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 200.0\n").expect("上限が読めなければならない");
        assert_eq!(settings.ui.volume, 200.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 0.0\n").expect("下限が読めなければならない");
        assert_eq!(settings.ui.volume, 0.0);
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

    const VIDEOS: &str = r"C:\Users\tester\Videos";

    #[test]
    fn recording_folder_from_videos_available_uses_videos() {
        let folder = recording_folder_from(
            Some(PathBuf::from(VIDEOS)),
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(VIDEOS));
    }

    #[test]
    fn recording_folder_from_without_videos_falls_back_in_the_screenshot_order() {
        // ビデオフォルダが無ければ、スクリーンショットと同じ順（デスクトップ → ホーム → exe）
        let desktop = recording_folder_from(
            None,
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );
        let home = recording_folder_from(
            None,
            None,
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );
        let exe = recording_folder_from(
            None,
            None,
            None,
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(desktop, PathBuf::from(DESKTOP));
        assert_eq!(home, PathBuf::from(HOME));
        assert_eq!(exe, PathBuf::from(EXE_DIR));
    }

    #[test]
    fn recording_folder_from_nothing_available_uses_the_last_resort_not_current_dir() {
        let folder = recording_folder_from(None, None, None, None, PathBuf::from(TEMP));

        assert_eq!(folder, PathBuf::from(TEMP));
        assert!(folder.is_absolute());
    }

    #[test]
    fn app_settings_without_recording_section_uses_recording_defaults() {
        // 録画が入る前の版が書いた設定ファイル。録画は既定値で、他の項目は保たれる
        assert!(!FULL_CONFIG.contains("[recording]"));

        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("[recording] が無くても読めなければならない");

        assert_eq!(
            settings.recording.file_name_format,
            DEFAULT_RECORDING_FILE_NAME_FORMAT
        );
        assert_eq!(settings.recording.video_bitrate_kbps, 8_000);
        assert!(settings.recording.hardware_encoder);
        assert!(settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 160);
        assert_eq!(
            settings.recording.folder,
            RecordingSettings::default().folder
        );
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_partial_recording_section_keeps_the_other_defaults() {
        let config = format!("{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = 12000\n");

        let settings: AppSettings =
            toml::from_str(&config).expect("[recording] の一部だけでも読めなければならない");

        assert_eq!(settings.recording.video_bitrate_kbps, 12_000);
        assert_eq!(
            settings.recording.file_name_format,
            DEFAULT_RECORDING_FILE_NAME_FORMAT
        );
        assert!(settings.recording.hardware_encoder);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_recording_section_round_trips() {
        let original = AppSettings {
            recording: RecordingSettings {
                folder: PathBuf::from(r"D:\captures"),
                file_name_format: "clip_%Y%m%d_%H%M%S".to_string(),
                video_bitrate_kbps: 25_000,
                hardware_encoder: false,
                audio_enabled: false,
                audio_bitrate_kbps: 128,
                replay_enabled: true,
                replay_seconds: 120,
            },
            ..AppSettings::default()
        };

        let text = toml::to_string(&original).expect("書き出せる");
        assert!(text.contains("[recording]"), "{text}");
        let restored: AppSettings = toml::from_str(&text).expect("読み戻せる");

        assert_eq!(restored.recording, original.recording);
    }

    #[test]
    fn app_settings_out_of_range_recording_bitrate_is_clamped_without_losing_settings() {
        for (written, expected) in [
            (0, 1_000),
            (-5, 1_000),
            (999_999, 50_000),
            (1_000, 1_000),
            (50_000, 50_000),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("範囲外のビットレートでも読めなければならない");

            assert_eq!(settings.recording.video_bitrate_kbps, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn app_settings_recording_section_from_the_video_only_version_enables_audio() {
        // 第 1 段（映像のみ）の版が書いた [recording] には音声の項目が無い。
        // 構造体の既定値（音声を録る、160kbps）になり、書いてある項目は保たれる
        let config = format!(
            "{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = 12000\nhardware_encoder = false\n"
        );

        let settings: AppSettings = toml::from_str(&config).expect("読めなければならない");

        assert!(settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 160);
        assert_eq!(settings.recording.video_bitrate_kbps, 12_000);
        assert!(!settings.recording.hardware_encoder);
    }

    #[test]
    fn app_settings_unlisted_recording_audio_bitrate_snaps_without_losing_settings() {
        for (written, expected) in [
            (0, 96),
            (-5, 96),
            (100, 96),
            (112, 128),
            (150, 160),
            (999_999, 192),
            (128, 128),
            (192, 192),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\naudio_bitrate_kbps = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("選べないビットレートでも読めなければならない");

            assert_eq!(settings.recording.audio_bitrate_kbps, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn app_settings_recording_section_from_the_audio_version_keeps_replay_off() {
        // 第 2 段（音声）の版が書いた [recording] にはリプレイバッファの項目が無い。
        // 既定（OFF、30 秒）で読み、他の値は失わない
        let config =
            format!("{FULL_CONFIG}\n[recording]\naudio_enabled = false\naudio_bitrate_kbps = 96\n");

        let settings: AppSettings = toml::from_str(&config).expect("読めなければならない");

        assert!(!settings.recording.replay_enabled);
        assert_eq!(settings.recording.replay_seconds, 30);
        assert!(!settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 96);
    }

    #[test]
    fn app_settings_out_of_range_replay_seconds_is_clamped_without_losing_settings() {
        for (written, expected) in [
            (0, 5),
            (-10, 5),
            (4, 5),
            (5, 5),
            (300, 300),
            (301, 300),
            (86_400, 300),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\nreplay_seconds = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("範囲外の長さでも読めなければならない");

            assert_eq!(settings.recording.replay_seconds, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn recording_settings_replay_seconds_for_recording_follows_the_switch() {
        let mut recording = RecordingSettings {
            replay_seconds: 60,
            ..RecordingSettings::default()
        };
        assert_eq!(recording.replay_seconds_for_recording(), None);
        recording.replay_enabled = true;
        assert_eq!(recording.replay_seconds_for_recording(), Some(60));
        // 読み込み後に差し替えられた範囲外の値も、渡す前に丸める
        recording.replay_seconds = 1_000;
        assert_eq!(recording.replay_seconds_for_recording(), Some(300));
        recording.replay_seconds = 1;
        assert_eq!(recording.replay_seconds_for_recording(), Some(5));
    }

    #[test]
    fn nearest_audio_bitrate_prefers_the_higher_one_at_the_midpoint() {
        assert_eq!(nearest_audio_bitrate_kbps(112), 128);
        assert_eq!(nearest_audio_bitrate_kbps(144), 160);
        assert_eq!(nearest_audio_bitrate_kbps(176), 192);
        assert_eq!(nearest_audio_bitrate_kbps(143), 128);
    }

    #[test]
    fn recording_settings_audio_bitrate_for_recording_follows_the_switch() {
        let mut recording = RecordingSettings {
            audio_bitrate_kbps: 130,
            ..RecordingSettings::default()
        };
        // 読み込んだあとに値を差し替えられても、渡す前に選択肢へ寄せる
        assert_eq!(recording.audio_bitrate_for_recording(), Some(128));
        recording.audio_enabled = false;
        assert_eq!(recording.audio_bitrate_for_recording(), None);
    }

    #[test]
    fn recording_settings_clamped_bitrate_stays_in_range() {
        let mut recording = RecordingSettings {
            video_bitrate_kbps: 10,
            ..RecordingSettings::default()
        };
        assert_eq!(recording.clamped_bitrate_kbps(), MIN_RECORDING_BITRATE_KBPS);
        recording.video_bitrate_kbps = 60_000;
        assert_eq!(recording.clamped_bitrate_kbps(), MAX_RECORDING_BITRATE_KBPS);
        recording.video_bitrate_kbps = 8_000;
        assert_eq!(recording.clamped_bitrate_kbps(), 8_000);
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
    fn language_setting_serializes_as_short_codes() {
        // 設定ファイルに書き出される綴り。ここが変わると、既に配布した版が
        // 書いた設定ファイルを読めなくなる
        for (setting, expected) in [
            (LanguageSetting::Auto, r#"language = "auto""#),
            (LanguageSetting::Japanese, r#"language = "ja""#),
            (LanguageSetting::English, r#"language = "en""#),
        ] {
            let mut settings = AppSettings::default();
            settings.ui.language = setting;
            let serialized = toml::to_string(&settings).expect("設定を書き出せること");
            assert!(serialized.contains(expected), "{}", serialized);

            let restored: AppSettings = toml::from_str(&serialized).expect("読み戻せること");
            assert_eq!(restored.ui.language, setting);
        }
    }

    #[test]
    fn video_backend_is_part_of_the_preset() {
        // 開き方はデバイスと一体なので、プリセットで適用も比較もする
        let mut settings = AppSettings::default();
        settings.video.backend = VideoBackendSetting::DirectShow;
        let preset = Preset::from_settings("DS".to_string(), &settings);
        assert_eq!(preset.video.backend, VideoBackendSetting::DirectShow);

        let mut target = AppSettings::default();
        assert!(!matches_preset(&preset, &target));
        preset.apply_to(&mut target);
        assert_eq!(target.video.backend, VideoBackendSetting::DirectShow);
        assert!(matches_preset(&preset, &target));
    }

    #[test]
    fn language_setting_missing_defaults_to_auto() {
        // 言語の項目ができる前の設定ファイル
        let settings: AppSettings = toml::from_str("[ui]\nvolume = 40.0\n").expect("読めること");
        assert_eq!(settings.ui.language, LanguageSetting::Auto);
        assert_eq!(settings.ui.volume, 40.0);
    }

    #[test]
    fn language_setting_unknown_value_falls_back_to_auto_and_keeps_other_items() {
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 40.0\nlanguage = \"fr\"\n").expect("読めること");
        assert_eq!(settings.ui.language, LanguageSetting::Auto);
        assert_eq!(settings.ui.volume, 40.0);
    }

    #[test]
    fn language_setting_from_str_accepts_known_spellings() {
        assert_eq!(
            language_setting_from_str("auto"),
            Some(LanguageSetting::Auto)
        );
        assert_eq!(
            language_setting_from_str(" JA "),
            Some(LanguageSetting::Japanese)
        );
        assert_eq!(
            language_setting_from_str("japanese"),
            Some(LanguageSetting::Japanese)
        );
        assert_eq!(
            language_setting_from_str("en"),
            Some(LanguageSetting::English)
        );
        assert_eq!(
            language_setting_from_str("English"),
            Some(LanguageSetting::English)
        );
        assert_eq!(language_setting_from_str(""), None);
        assert_eq!(language_setting_from_str("fr"), None);
    }

    #[test]
    fn language_setting_resolve_uses_os_language_only_for_auto() {
        use crate::i18n::Language;
        for os in [Language::Japanese, Language::English] {
            assert_eq!(LanguageSetting::Auto.resolve(os), os);
            assert_eq!(LanguageSetting::Japanese.resolve(os), Language::Japanese);
            assert_eq!(LanguageSetting::English.resolve(os), Language::English);
        }
    }

    // ---- ホットキーの移行 ----

    #[test]
    fn legacy_config_moves_screenshot_hotkey_into_hotkeys() {
        // アクション別にする前の版が書いた設定ファイル。設定していた
        // ホットキーがスクリーンショットへ移り、失われないこと
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        // 他のアクションは未割り当てのまま
        assert_eq!(settings.hotkeys.len(), 1);
        // 無関係な項目も保持される
        assert_eq!(settings.ui.volume, 80.0);
        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn legacy_config_without_hotkey_keeps_the_default_f5() {
        // 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった
        // （項目ごと消えるため、欠けた項目と区別できない）。移行後も
        // 従来と同じく既定の F5 になること
        let config = without_key(LEGACY_CONFIG, "hotkey");
        assert!(
            !config.contains("hotkey ="),
            "テスト用の設定に hotkey が残っている"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("hotkey が無い旧版の設定も読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
    }

    #[test]
    fn hotkeys_section_wins_over_the_legacy_key() {
        // 手で書き換えて両方が書かれている場合。新しい形式を正とする
        let config = format!("{}\n[hotkeys]\nscreenshot = \"F8\"\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("両方あっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F8"));
    }

    #[test]
    fn empty_hotkeys_section_means_no_assignment() {
        // すべての割り当てを外した状態。セクションはあるが中身が無い。
        // 既定の F5 を入れ直してはならない
        let config = format!("{}\n[hotkeys]\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("空の [hotkeys] でも読めなければならない");

        assert!(settings.hotkeys.is_empty());
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
    }

    #[test]
    fn empty_hotkeys_survives_a_save_and_load_roundtrip() {
        // 「割り当て無し」を書き出して読み直しても、既定の F5 に戻らないこと。
        // [hotkeys] セクションごと書き出されないと、旧版の設定ファイルと
        // 区別が付かなくなる
        let mut original = AppSettings::default();
        original.hotkeys.clear();

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert!(
            serialized.contains("[hotkeys]"),
            "空でも [hotkeys] セクションが書き出されること: {}",
            serialized
        );
        assert!(restored.hotkeys.is_empty());
    }

    #[test]
    fn update_settings_missing_section_uses_defaults() {
        // [update] が無い（更新の確認を足す前の版が書いた）設定ファイル。
        // 確認も通知もする側に倒し、他の項目は残る
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("[update] が無くても読めなければならない");

        assert!(settings.update.check_on_startup);
        assert!(settings.update.notify_on_startup);
        assert_eq!(settings.update.skipped_version, None);
        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn update_settings_missing_one_key_keeps_the_others() {
        // 構造体レベルの #[serde(default)] なので、欠けた bool は false ではなく
        // 構造体の既定値（true）になる
        let settings: AppSettings =
            toml::from_str("[update]\nnotify_on_startup = false\nskipped_version = \"1.2.0\"\n")
                .expect("[update] の一部が欠けていても読めなければならない");

        assert!(settings.update.check_on_startup);
        assert!(!settings.update.notify_on_startup);
        assert_eq!(settings.update.skipped_version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn update_settings_survive_a_save_and_load_roundtrip() {
        let mut original = AppSettings::default();
        original.update.check_on_startup = false;
        original.update.skipped_version = Some("1.2.0".to_string());

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert_eq!(restored.update, original.update);
        assert!(
            serialized.contains("[update]"),
            "[update] セクションに書き出されること: {}",
            serialized
        );
    }

    #[test]
    fn hotkey_settings_missing_section_reacts_without_focus() {
        // [hotkey_settings] が無い（この項目より前の版が書いた）設定ファイル。
        // 従来どおり、他のアプリを操作している間も反応する側に倒す
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定を読めなければならない");

        assert!(!settings.hotkey_settings.only_when_focused);
    }

    #[test]
    fn hotkey_settings_survive_a_save_and_load_roundtrip() {
        let mut original = AppSettings::default();
        original.hotkey_settings.only_when_focused = true;

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert!(restored.hotkey_settings.only_when_focused);
        // [hotkeys] は値が文字列である前提で読んでいる。真偽値を混ぜると
        // 旧版では [hotkeys] ごと読めなくなるので、別のセクションに書く
        assert!(
            serialized.contains("[hotkey_settings]"),
            "別のセクションに書き出されること: {}",
            serialized
        );
    }

    #[test]
    fn saved_config_does_not_keep_the_legacy_hotkey_key() {
        // 移行したあとは旧版の項目を書き戻さない。残すと 2 つの置き場所が
        // 食い違ったときにどちらが正か決まらなくなる
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        let serialized = toml::to_string(&settings).expect("設定を書き出せなければならない");

        assert!(
            !serialized.contains("hotkey = "),
            "screenshot.hotkey が書き戻されている: {}",
            serialized
        );
        assert!(serialized.contains("screenshot = \"Ctrl+S\""));
    }

    #[test]
    fn migrated_settings_drop_the_legacy_field() {
        // 読み込んだ時点で旧版の項目は空になる。残っていると、そこを見て
        // 動く処理をうっかり足せてしまう
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.screenshot.legacy_hotkey, None);
    }

    #[test]
    fn unknown_hotkey_action_is_ignored_without_losing_settings() {
        // 新しい版が増やしたアクションを古い版で読んだ場合。知らない名前で
        // ファイル全体のパースを失敗させない
        let config = format!("{}mute = \"Ctrl+M\"\n", FULL_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("知らないアクションがあっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
        assert_eq!(settings.hotkeys.len(), 2);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn hotkeys_are_readable_for_every_action() {
        // アクションを足したときに、設定ファイル側のキー名が読めなくなって
        // いないことを全アクションで確かめる
        let lines: String = HotkeyAction::ALL
            .iter()
            .map(|action| format!("{} = \"F5\"\n", action.as_str()))
            .collect();
        let config = format!("[hotkeys]\n{}", lines);

        let settings: AppSettings =
            toml::from_str(&config).expect("全アクションぶんの割り当てが読めなければならない");

        assert_eq!(settings.hotkeys.len(), HotkeyAction::ALL.len());
        for action in HotkeyAction::ALL {
            assert_eq!(settings.hotkey(action), Some("F5"), "{:?}", action);
        }
    }

    #[test]
    fn set_hotkey_none_removes_the_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, None);

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
        assert!(settings.hotkeys.is_empty());
    }

    #[test]
    fn set_hotkey_replaces_the_existing_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, Some("Ctrl+S".to_string()));
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Shift+1".to_string()));

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(
            settings.hotkey(HotkeyAction::VolumeUp),
            Some("Ctrl+Shift+1")
        );
    }

    #[test]
    fn default_hotkeys_assign_only_the_screenshot() {
        // 他のアクションを既定で割り当てない。ホットキーは既定で
        // 他のアプリを操作している間も反応するため、こちらから押さえない
        let settings = AppSettings::default();

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
        assert_eq!(settings.hotkeys.len(), 1);
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

    // プリセットのテストで使う設定ファイル。
    //
    // 選択中のプリセットと [video] / [audio] の中身が一致している状態。
    // 省略した項目はどちらも既定値になるので、実際に一致する。
    const PRESET_CONFIG: &str = r#"
active_preset = "低遅延優先"

[video]
device_name = "Capture Device"
resolution = [1280, 720]
fps = 60

[audio]
input_device_name = "Line In"

[[presets]]
name = "低遅延優先"

[presets.video]
device_name = "Capture Device"
resolution = [1280, 720]
fps = 60

[presets.audio]
input_device_name = "Line In"

[[presets]]
name = "画質優先"

[presets.video]
device_name = "Capture Device"
resolution = [1920, 1080]
fps = 30

[presets.audio]
input_device_name = "Line In"
"#;

    // 解像度と fps だけが違うプリセットを作る。
    fn preset_named(name: &str, width: u32, height: u32, fps: u32) -> Preset {
        let mut settings = AppSettings::default();
        settings.video.resolution = Some((width, height));
        settings.video.fps = Some(fps);
        Preset::from_settings(name.to_string(), &settings)
    }

    #[test]
    fn validate_preset_name_empty_returns_error() {
        assert_eq!(
            validate_preset_name("", &[], None),
            Err(PresetNameError::Empty)
        );
    }

    #[test]
    fn validate_preset_name_whitespace_only_returns_error() {
        // 空白だけの名前はメニューに出しても読めない
        assert_eq!(
            validate_preset_name("   ", &[], None),
            Err(PresetNameError::Empty)
        );
    }

    #[test]
    fn validate_preset_name_trims_surrounding_whitespace() {
        assert_eq!(
            validate_preset_name("  低遅延優先  ", &[], None),
            Ok("低遅延優先".to_string())
        );
    }

    #[test]
    fn validate_preset_name_duplicate_returns_error() {
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name("低遅延優先", &presets, None),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn validate_preset_name_trimmed_duplicate_returns_error() {
        // 前後の空白を落としたあとで比べる。落とさないと
        // 「低遅延優先 」と「低遅延優先」が並んで見分けられなくなる
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name(" 低遅延優先 ", &presets, None),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn validate_preset_name_duplicate_is_allowed_when_replacing_itself() {
        // 同じ名前のまま上書き保存する場合
        let presets = vec![preset_named("低遅延優先", 1280, 720, 60)];

        assert_eq!(
            validate_preset_name("低遅延優先", &presets, Some("低遅延優先")),
            Ok("低遅延優先".to_string())
        );
    }

    #[test]
    fn validate_preset_name_duplicate_of_another_preset_is_rejected_when_replacing() {
        // 上書き対象以外との衝突は弾く
        let presets = vec![
            preset_named("低遅延優先", 1280, 720, 60),
            preset_named("画質優先", 1920, 1080, 30),
        ];

        assert_eq!(
            validate_preset_name("画質優先", &presets, Some("低遅延優先")),
            Err(PresetNameError::Duplicate)
        );
    }

    #[test]
    fn preset_from_settings_normalizes_auto_reconnect() {
        // 自動再接続はプリセットの対象外。作った時点の値を拾わず、
        // 既定値で固定されること
        let mut settings = AppSettings::default();
        settings.video.auto_reconnect = false;

        let preset = Preset::from_settings("低遅延優先".to_string(), &settings);

        assert!(preset.video.auto_reconnect);
    }

    #[test]
    fn apply_preset_changes_only_video_and_audio() {
        let mut settings = AppSettings::default();
        settings.screenshot.jpeg_quality = 55;
        settings.ui.volume = 33.0;
        settings.set_hotkey(HotkeyAction::Screenshot, Some("Ctrl+S".to_string()));
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(settings.apply_preset("画質優先"));

        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.video.fps, Some(30));
        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
        // プリセットが持たないセクションは 1 つも動かない
        assert_eq!(settings.screenshot.jpeg_quality, 55);
        assert_eq!(settings.ui.volume, 33.0);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
    }

    #[test]
    fn apply_preset_keeps_auto_reconnect() {
        // 右クリックメニューで切った自動再接続が、プリセットの
        // 切替で勝手に戻らないこと
        let mut settings = AppSettings::default();
        settings.video.auto_reconnect = false;
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(settings.apply_preset("画質優先"));

        assert!(!settings.video.auto_reconnect);
    }

    #[test]
    fn apply_preset_unknown_name_changes_nothing() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(!settings.apply_preset("無い名前"));

        assert_eq!(settings.video.resolution, Some((1280, 720)));
        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn matches_preset_after_apply_returns_true() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);

        assert!(matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_different_resolution_returns_false() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.video.resolution = Some((1280, 720));

        assert!(!matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_different_audio_returns_false() {
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.audio.passthrough_enabled = !settings.audio.passthrough_enabled;

        assert!(!matches_preset(&preset, &settings));
    }

    #[test]
    fn matches_preset_ignores_auto_reconnect() {
        // 適用しない項目を比べないこと。比べると、自動再接続を
        // 切り替えただけで「（変更あり）」になる
        let mut settings = AppSettings::default();
        let preset = preset_named("画質優先", 1920, 1080, 30);
        preset.apply_to(&mut settings);
        settings.video.auto_reconnect = !preset.video.auto_reconnect;

        assert!(matches_preset(&preset, &settings));
    }

    #[test]
    fn resolved_active_preset_returns_the_name_when_values_match() {
        let settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");

        assert_eq!(resolved_active_preset(&settings), Some("低遅延優先"));
        assert_eq!(settings.presets.len(), 2);
    }

    #[test]
    fn resolved_active_preset_returns_none_after_editing_video() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.video.fps = Some(30);

        assert_eq!(resolved_active_preset(&settings), None);
        // 判定だけでは active_preset を書き換えない
        assert_eq!(settings.active_preset.as_deref(), Some("低遅延優先"));
    }

    #[test]
    fn resolved_active_preset_returns_none_when_the_preset_was_removed() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.presets.clear();

        assert_eq!(resolved_active_preset(&settings), None);
    }

    #[test]
    fn refresh_active_preset_clears_a_stale_selection() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.video.fps = Some(24);
        settings.refresh_active_preset();

        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn refresh_active_preset_keeps_a_matching_selection() {
        let mut settings: AppSettings =
            toml::from_str(PRESET_CONFIG).expect("プリセット付きの設定を読めること");
        settings.refresh_active_preset();

        assert_eq!(settings.active_preset.as_deref(), Some("低遅延優先"));
    }

    #[test]
    fn load_clears_an_active_preset_that_does_not_match() {
        // 設定ファイルを手で書き換えて食い違わせた場合。
        // 読んだ時点で選択が外れていること
        let config = PRESET_CONFIG.replacen("fps = 60", "fps = 24", 1);
        let settings: AppSettings = toml::from_str(&config).expect("読めること");

        assert_eq!(settings.active_preset, None);
        assert_eq!(settings.presets.len(), 2);
    }

    #[test]
    fn upsert_preset_replaces_the_same_name() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings.upsert_preset(preset_named("画質優先", 1280, 720, 60));

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].video.resolution, Some((1280, 720)));
    }

    #[test]
    fn upsert_preset_appends_a_new_name() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings.upsert_preset(preset_named("低遅延優先", 1280, 720, 60));

        assert_eq!(settings.presets.len(), 2);
        assert_eq!(settings.presets[1].name, "低遅延優先");
    }

    #[test]
    fn remove_preset_clears_the_active_selection() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        assert!(settings.apply_preset("画質優先"));

        assert!(settings.remove_preset("画質優先"));

        assert!(settings.presets.is_empty());
        assert_eq!(settings.active_preset, None);
    }

    #[test]
    fn remove_preset_keeps_the_selection_of_another_preset() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings
            .presets
            .push(preset_named("低遅延優先", 1280, 720, 60));
        assert!(settings.apply_preset("画質優先"));

        assert!(settings.remove_preset("低遅延優先"));

        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
    }

    #[test]
    fn remove_preset_unknown_name_returns_false() {
        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));

        assert!(!settings.remove_preset("無い名前"));
        assert_eq!(settings.presets.len(), 1);
    }

    #[test]
    fn load_drops_a_preset_without_a_name() {
        // 手で書き換えた設定ファイル。名前が無いプリセットをそのまま持つと、
        // 右クリックメニューと一覧に空の項目が並ぶ
        let config = r#"
[[presets]]
name = "   "

[presets.video]
fps = 30

[[presets]]
name = "画質優先"

[presets.video]
fps = 30
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].name, "画質優先");
    }

    #[test]
    fn load_drops_a_duplicated_preset_name_keeping_the_first() {
        // 重複した名前を残すと、preset() も remove_preset() も先頭しか
        // 見ないため 2 つ目以降を選ぶことも消すこともできない
        let config = r#"
[[presets]]
name = "画質優先"

[presets.video]
fps = 30

[[presets]]
name = " 画質優先 "

[presets.video]
fps = 24
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets.len(), 1);
        assert_eq!(settings.presets[0].video.fps, Some(30));
    }

    #[test]
    fn load_trims_preset_names_and_the_active_selection() {
        // 名前と選択の両方から空白を落とす。片方だけだと空白の有無で引けなくなる
        let config = r#"
active_preset = "  画質優先  "

[video]
resolution = [1920, 1080]
fps = 30

[[presets]]
name = "  画質優先  "

[presets.video]
resolution = [1920, 1080]
fps = 30
"#;

        let settings: AppSettings = toml::from_str(config).expect("読めること");

        assert_eq!(settings.presets[0].name, "画質優先");
        assert_eq!(settings.active_preset.as_deref(), Some("画質優先"));
        assert_eq!(resolved_active_preset(&settings), Some("画質優先"));
    }

    #[test]
    fn app_settings_without_presets_section_has_no_presets() {
        // プリセットを足した版へ上げた直後。既存ユーザーの設定には
        // [[presets]] も active_preset も無いが、他の項目は保たれること
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.presets.is_empty());
        assert_eq!(settings.active_preset, None);
        assert_eq!(settings.video.resolution, Some((1920, 1080)));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn presets_survive_a_save_and_load_roundtrip() {
        // 保存で書き出せる形になっていることを確かめる。
        // **TOML はテーブルのあとに素の値を書けない。** active_preset の
        // 宣言位置を後ろへ移すとここで落ちる
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("settings.toml");

        let mut settings = AppSettings::default();
        settings
            .presets
            .push(preset_named("画質優先", 1920, 1080, 30));
        settings
            .presets
            .push(preset_named("低遅延優先", 1280, 720, 60));
        assert!(settings.apply_preset("画質優先"));

        export_to(&path, &settings).expect("書き出せること");
        let loaded = import_from(&path).expect("読めること");

        assert_eq!(loaded.presets, settings.presets);
        assert_eq!(loaded.active_preset.as_deref(), Some("画質優先"));
        assert_eq!(loaded.video.resolution, Some((1920, 1080)));
    }
}
