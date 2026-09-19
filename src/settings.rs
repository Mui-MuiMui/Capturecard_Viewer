use log::{error, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// confy が設定ファイルの置き場所を決めるのに使う名前。
// ここがずれると既存の設定ファイルを見失うため、1 箇所にまとめてある。
// ログの出力先も同じデータディレクトリを基準に決めるので、logging から参照する。
pub(crate) const APP_NAME: &str = "capturecard_viewer";

// 各構造体の #[serde(default)] は、項目を追加したあとも古い設定ファイルを
// 読めるようにするためのもの。これが無いと、
//   - Option 以外の項目が欠けた場合はパースが失敗し、全項目が初期化される
//   - Option の項目が欠けた場合は None になり、Default の値が使われない
// という形で既存ユーザーの設定が失われる。新しい項目を足すときも外さないこと。

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppSettings {
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub screenshot: ScreenshotSettings,
    pub ui: UiSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub device_name: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub format: Option<String>,
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub passthrough_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSettings {
    pub save_folder: PathBuf,
    // 保存形式と JPEG の品質を別々の項目にしてある。品質を持つ enum を
    // 1 項目として持たせると TOML では [screenshot.format] のテーブルになり、
    // 同じセクションの後続のキー（sound_file など）がテーブルの内側へ
    // 取り込まれてしまう。また項目を分けておくと、PNG に切り替えても
    // 品質の値が残り、JPEG へ戻したときに選び直さずに済む。
    //
    // エンコードへ渡すときは encoding() で ScreenshotEncoding にまとめ、
    // 「PNG なのに品質が付いている」組み合わせを作れないようにする
    #[serde(deserialize_with = "deserialize_screenshot_format")]
    pub format: ScreenshotFormat,
    #[serde(deserialize_with = "deserialize_jpeg_quality")]
    pub jpeg_quality: u8,
    pub sound_file: Option<PathBuf>,
    pub sound_volume: f32,
    pub hotkey: Option<String>,
}

// スクリーンショットの保存形式。設定ファイルには format = "jpeg" / "png" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    // 既存ユーザーの設定ファイルには format が無い。既定を JPEG にしてあるので
    // 従来どおり JPEG で保存され、拡張子も .jpg のまま変わらない
    #[default]
    Jpeg,
    Png,
}

impl ScreenshotFormat {
    // 保存するファイルの拡張子。先頭のドットは含まない
    pub fn extension(self) -> &'static str {
        match self {
            ScreenshotFormat::Jpeg => "jpg",
            ScreenshotFormat::Png => "png",
        }
    }
}

impl ScreenshotSettings {
    // 設定からエンコードの指定を組み立てる。
    //
    // 品質は設定ファイルを手で書き換えられる前提で、ここで範囲に収める。
    // image 0.24 の JpegEncoder も内部で 1〜100 に丸めるが、そこに寄りかかると
    // クレートの版が変わったときに振る舞いが変わる。渡す前に確定させておく
    pub fn encoding(&self) -> ScreenshotEncoding {
        match self.format {
            ScreenshotFormat::Jpeg => ScreenshotEncoding::Jpeg {
                quality: self.jpeg_quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY),
            },
            ScreenshotFormat::Png => ScreenshotEncoding::Png,
        }
    }
}

// 実際にエンコードするときの形式とパラメータ。
//
// 設定の保存形式（ScreenshotFormat）と分けてあるのは、保存関数へ
// 「PNG なのに品質が付いている」ような組み合わせを渡せなくするため。
// 設定ファイルには書かれないので、TOML の都合に縛られない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotEncoding {
    Jpeg { quality: u8 },
    Png,
}

// JPEG 品質の下限と上限。image クレートの JpegEncoder が受け付ける範囲に合わせてある
pub const MIN_JPEG_QUALITY: u8 = 1;
pub const MAX_JPEG_QUALITY: u8 = 100;

// 設定ファイルの format に知らない値が書かれていても、設定全体を失わせない。
// ここでエラーを返すと TOML のパースがファイル単位で失敗し、保存形式と
// 無関係な項目まで既定値へ戻ってしまう。
//
// 値が文字列ですらない場合（format = 3 など）はここでも落ちる。手で書き換えた
// ときに起きやすいのは綴りの誤りなので、拾うのはそこまでにしてある。
fn deserialize_screenshot_format<'de, D>(deserializer: D) -> Result<ScreenshotFormat, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(screenshot_format_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の保存形式 \"{}\" を解釈できないので JPEG として扱う",
            raw
        );
        ScreenshotFormat::default()
    }))
}

// 範囲外の品質が書かれていても、設定全体を失わせない。u8 のまま読むと
// jpeg_quality = 256 のような値でパースがファイル単位で失敗し、品質と
// 無関係な項目まで既定値へ戻ってしまう。TOML の整数は i64 なので、
// 広いほうで受けてから 1〜100 に丸める。
//
// 値が整数ですらない場合（jpeg_quality = 90.5 など）はここでも落ちる。
// format と同じく、手で書き換えたときに起きやすいところだけを拾う。
fn deserialize_jpeg_quality<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_JPEG_QUALITY), i64::from(MAX_JPEG_QUALITY));
    if clamped != raw {
        warn!(
            "設定の JPEG 品質 {} は範囲外なので {} として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u8 に収まる
    Ok(clamped as u8)
}

// 設定ファイルに書かれた文字列から保存形式を決める。
// 解釈できない場合は None を返す。
fn screenshot_format_from_str(raw: &str) -> Option<ScreenshotFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some(ScreenshotFormat::Jpeg),
        "png" => Some(ScreenshotFormat::Png),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub volume: f32,
    pub maintain_aspect_ratio: bool,
    pub last_window_size: Option<(f32, f32)>,
    pub last_window_pos: Option<(f32, f32)>,
    pub always_on_top: bool,
    pub enable_drag_move: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            device_name: None,
            resolution: Some((1280, 720)),    // 720pで安定性を優先
            format: Some("YUY2".to_string()), // YUY2フォーマット
            fps: Some(60),                    // 60fps目標
        }
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device_name: None,
            output_device_name: None,
            sample_rate: Some(48000),
            channels: Some(2),
            passthrough_enabled: true,
        }
    }
}

impl Default for ScreenshotSettings {
    fn default() -> Self {
        Self {
            save_folder: dirs::desktop_dir().unwrap_or_else(|| PathBuf::from(".")),
            format: ScreenshotFormat::Jpeg,
            // image クレートの save() は JpegEncoder::new を通るため、
            // これまでの保存は品質 75 固定だった。ゲーム画面のように
            // 文字や細い線が多い画には 75 では圧縮の跡が見えるので、
            // 既定をひとつ上の 90 にしてある。ファイルは 75 のおよそ 2 倍に
            // なるが、それでも PNG よりはずっと小さい。
            // 品質を気にしない用途は既定のまま、跡を残したくない用途は
            // PNG を選ぶ、という切り分けにする
            jpeg_quality: 90,
            // 相対パスのまま既定値にしてある。既存ユーザーの設定ファイルにも
            // この値が保存されているため、変えると移行の前提が崩れる。
            // 解決は screenshot::resolve_sound_path が exe の置き場所を基準に行い、
            // 見つからなければ埋め込みの既定音へ倒す。
            // None は「効果音を鳴らさない」の意味なので、既定値には使えない
            sound_file: Some(PathBuf::from("sound/SS.mp3")),
            sound_volume: 100.0,
            hotkey: Some("F5".to_string()),
        }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            volume: 100.0,
            maintain_aspect_ratio: true,
            last_window_size: None,
            last_window_pos: None,
            always_on_top: false,
            enable_drag_move: true,
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

// 退避の結果から読み込み結果を決める。
//
// load() 自体は confy が %AppData% を直接読み書きするためテストできない。
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
        match confy::load(APP_NAME, None) {
            Ok(settings) => (settings, LoadOutcome::Loaded),
            Err(e) => {
                error!("設定ファイルを読み込めないため既定値で起動する: {}", e);

                // 読み込みに失敗した設定ファイルは、既定値で起動する前に退避する。
                // 黙って上書きすると、ユーザーが自分の設定を取り戻す手段が無くなる。
                //
                // 失敗したという事実は LoadOutcome として呼び出し側へ渡し、
                // 理由はログに残す。ここは起動直後で UI がまだ無いため、
                // ユーザーへ伝える手段がログしかない。
                let outcome = match confy::get_configuration_file_path(APP_NAME, None) {
                    Ok(path) => {
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
                        outcome_from_backup(backup)
                    }
                    // 設定ファイルの置き場所が分からず、退避を試みることすらできない。
                    // 読めなかったファイルが残っている可能性があるため、
                    // 書き戻さない側に倒す。
                    Err(e) => {
                        error!("設定ファイルの置き場所が分からず退避できない: {}", e);
                        LoadOutcome::BrokenFileLeftBehind
                    }
                };
                (Self::default(), outcome)
            }
        }
    }

    // 保存できたかを返す。
    //
    // 結果を捨てないのは、デバウンスして書き出す側が失敗を検知して
    // 再試行できるようにするため。失敗を握り潰すと、書けなかった変更が
    // 保存済みとして扱われて消える。
    pub fn save(&self) -> bool {
        match confy::store(APP_NAME, None, self) {
            Ok(()) => true,
            Err(e) => {
                error!("設定の保存に失敗した: {}", e);
                false
            }
        }
    }

    pub fn get_screenshot_path(&self, timestamp: &str) -> PathBuf {
        let extension = self.screenshot.format.extension();
        let mut path = self.screenshot.save_folder.clone();
        path.push(format!("{}.{}", timestamp, extension));

        // ファイル名の競合を処理
        let mut counter = 1;
        while path.exists() {
            let stem = format!("{}({})", timestamp, counter);
            path.set_file_name(format!("{}.{}", stem, extension));
            counter += 1;
        }

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // 全項目を明示した設定ファイル。値はすべて既定値と異なるものにしてある。
    // 各テストはここから一部を削り、「古い版が書いた設定ファイル」を再現する。
    const FULL_CONFIG: &str = r#"
[video]
device_name = "Capture Device"
resolution = [1920, 1080]
format = "MJPEG"
fps = 30

[audio]
input_device_name = "Line In"
output_device_name = "Speakers"
sample_rate = 44100
channels = 1
passthrough_enabled = false

[screenshot]
save_folder = 'C:\shots'
format = "png"
jpeg_quality = 60
sound_file = 'sound/custom.mp3'
sound_volume = 50.0
hotkey = "Ctrl+S"

[ui]
volume = 80.0
maintain_aspect_ratio = false
last_window_size = [800.0, 600.0]
last_window_pos = [10.0, 20.0]
always_on_top = true
enable_drag_move = false
"#;

    // 指定したキーの行を取り除く。項目を 1 つ追加した直後の、
    // そのキーだけが存在しない設定ファイルを作るために使う。
    fn without_key(config: &str, key: &str) -> String {
        let prefix = format!("{} =", key);
        config
            .lines()
            .filter(|line| !line.trim_start().starts_with(&prefix))
            .collect::<Vec<_>>()
            .join("\n")
    }

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
        assert_eq!(settings.screenshot.hotkey, Some("Ctrl+S".to_string()));
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
        assert_eq!(settings.screenshot.hotkey, Some("F5".to_string()));
        assert_eq!(settings.ui.volume, 100.0);
        assert!(settings.ui.maintain_aspect_ratio);
        assert!(!settings.ui.always_on_top);
        assert!(settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_unknown_key_is_ignored() {
        // 新しい版で増えた項目が残った設定ファイルを、古い版で読む場合。
        // 知らないキーで失敗せず、既知の項目が保持されなければならない。
        let config = format!("{}future_option = true\n", FULL_CONFIG);

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
        assert_eq!(restored.screenshot.save_folder, PathBuf::from(r"C:\shots"));
        assert_eq!(restored.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(restored.screenshot.jpeg_quality, 60);
        assert_eq!(
            restored.screenshot.sound_file,
            Some(PathBuf::from("sound/custom.mp3"))
        );
        assert_eq!(restored.screenshot.sound_volume, 50.0);
        assert_eq!(restored.screenshot.hotkey, Some("Ctrl+S".to_string()));
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
    fn load_outcome_loaded_allows_writing_defaults() {
        // 正常に読めた場合。通常どおり保存してよい。
        assert!(LoadOutcome::Loaded.may_write_defaults_on_startup());
    }

    // get_screenshot_path は save_folder しか見ないため、
    // 一時ディレクトリを指した設定を組み立てれば %AppData% にもデスクトップにも触れない。
    fn settings_saving_into(dir: &Path) -> AppSettings {
        let mut settings = AppSettings::default();
        settings.screenshot.save_folder = dir.to_path_buf();
        settings
    }

    #[test]
    fn get_screenshot_path_no_conflict_uses_timestamp_as_is() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_one_conflict_appends_1() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_two_conflicts_appends_2() {
        // 連番付きのファイルも競合の判定に含めること。
        // 「(1) を作ったら (1) を上書きした」を防ぐための確認
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(2).jpg"));
    }

    #[test]
    fn get_screenshot_path_three_conflicts_appends_3() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(3).jpg"));
    }

    #[test]
    fn get_screenshot_path_gap_in_numbering_fills_the_gap() {
        // (1) だけ消された状態。連番は「空いている最小の番号」であり、
        // 既存の最大値 + 1 ではない
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_timestamp_with_dots_keeps_jpg_extension() {
        // タイムスタンプ自体にドットが含まれる場合。連番を付けるときに
        // ドット以降を拡張子と見なして削ってしまうと ".jpg" を失う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026.09.19_12.00.00.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026.09.19_12.00.00");

        assert_eq!(path, dir.path().join("2026.09.19_12.00.00(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_only_numbered_file_exists_uses_timestamp_as_is() {
        // 連番だけがあって本体が無い場合は、連番を付けずに本体の名前を使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000(1).jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_png_format_uses_png_extension() {
        // 保存形式を PNG にしたら拡張子も追従すること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn get_screenshot_path_png_conflict_keeps_png_extension() {
        // 連番を付けるときに拡張子を .jpg へ戻してしまわないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.png"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).png"));
    }

    #[test]
    fn get_screenshot_path_png_ignores_jpg_with_same_name() {
        // 形式が違えばファイル名は衝突しない。同名の .jpg があっても
        // .png 側は連番を付けずに撮影時刻そのままを使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn screenshot_format_extension_matches_format() {
        assert_eq!(ScreenshotFormat::Jpeg.extension(), "jpg");
        assert_eq!(ScreenshotFormat::Png.extension(), "png");
    }

    #[test]
    fn encoding_jpeg_passes_quality_through() {
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;
        settings.screenshot.jpeg_quality = 55;

        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 55 }
        );
    }

    #[test]
    fn encoding_jpeg_clamps_quality_into_range() {
        // 設定ファイルを手で書き換えられた場合。エンコーダへ渡す前に丸める
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;

        settings.screenshot.jpeg_quality = 0;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 1 }
        );

        settings.screenshot.jpeg_quality = 255;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 100 }
        );
    }

    #[test]
    fn encoding_png_ignores_jpeg_quality() {
        // PNG は可逆なので品質の値を持ち込まない
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Png;
        settings.screenshot.jpeg_quality = 10;

        assert_eq!(settings.screenshot.encoding(), ScreenshotEncoding::Png);
    }

    #[test]
    fn app_settings_missing_format_key_defaults_to_jpeg() {
        // format を足す前の版が書いた設定ファイル。これまでと同じ JPEG で
        // 保存され、他の項目も保持されなければならない。
        // without_key を使わないのは [video] にも format があるため
        let config = FULL_CONFIG.replace(
            "format = \"png\"
",
            "",
        );
        assert!(
            !config.contains("format = \"png\""),
            "テスト用の設定から screenshot の format が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("format が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.screenshot.hotkey, Some("Ctrl+S".to_string()));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_jpeg_quality_key_uses_default_quality() {
        let config = without_key(FULL_CONFIG, "jpeg_quality");
        assert!(
            !config.contains("jpeg_quality ="),
            "テスト用の設定から jpeg_quality が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("jpeg_quality が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 90);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
    }

    #[test]
    fn app_settings_unknown_format_value_falls_back_to_jpeg_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。保存形式だけが既定へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"format = "png""#, r#"format = "webp""#);
        assert!(config.contains(r#"format = "webp""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない保存形式でも読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.screenshot.hotkey, Some("Ctrl+S".to_string()));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_out_of_range_jpeg_quality_is_clamped_without_losing_settings() {
        // 手で書き換えて u8 に収まらない値を入れた場合。品質だけが範囲に
        // 収まり、無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = 256");
        assert!(config.contains("jpeg_quality = 256"));

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 100);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(settings.screenshot.hotkey, Some("Ctrl+S".to_string()));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_negative_jpeg_quality_is_clamped_to_minimum() {
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = -5");

        let settings: AppSettings =
            toml::from_str(&config).expect("負の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 1);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn screenshot_format_from_str_accepts_known_spellings() {
        assert_eq!(
            screenshot_format_from_str("jpeg"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str("JPG"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str(" png "),
            Some(ScreenshotFormat::Png)
        );
        assert_eq!(screenshot_format_from_str(""), None);
        assert_eq!(screenshot_format_from_str("bmp"), None);
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
}
