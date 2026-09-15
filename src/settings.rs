use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    pub sound_file: Option<PathBuf>,
    pub sound_volume: f32,
    pub hotkey: Option<String>,
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
            resolution: Some((1280, 720)), // 720pで安定性を優先
            format: Some("YUY2".to_string()), // YUY2フォーマット
            fps: Some(60), // 60fps目標
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

impl AppSettings {
    pub fn load() -> Self {
        confy::load("capturecard_viewer", None).unwrap_or_default()
    }
    
    pub fn save(&self) {
        if let Err(e) = confy::store("capturecard_viewer", None, self) {
            eprintln!("Failed to save settings: {}", e);
        }
    }
    
    pub fn get_screenshot_path(&self, timestamp: &str) -> PathBuf {
        let mut path = self.screenshot.save_folder.clone();
        path.push(format!("{}.jpg", timestamp));
        
        // ファイル名の競合を処理
        let mut counter = 1;
        while path.exists() {
            let stem = format!("{}({})", timestamp, counter);
            path.set_file_name(format!("{}.jpg", stem));
            counter += 1;
        }

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!config.contains("fps ="), "テスト用の設定から fps が消えていない");

        let settings: AppSettings =
            toml::from_str(&config).expect("fps が欠けていても読めなければならない");

        assert_eq!(settings.video.fps, Some(60)); // 欠けた項目だけ既定値
        assert_eq!(settings.video.device_name, Some("Capture Device".to_string()));
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
        assert!(!config.contains("[ui]"), "テスト用の設定から [ui] が消えていない");

        let settings: AppSettings =
            toml::from_str(&config).expect("[ui] セクションが欠けていても読めなければならない");

        assert_eq!(settings.ui.volume, 100.0); // UiSettings ごと既定値
        assert!(settings.ui.maintain_aspect_ratio);
        assert_eq!(settings.ui.last_window_size, None);
        assert_eq!(settings.video.device_name, Some("Capture Device".to_string()));
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

        assert_eq!(restored.video.device_name, Some("Capture Device".to_string()));
        assert_eq!(restored.video.resolution, Some((1920, 1080)));
        assert_eq!(restored.video.format, Some("MJPEG".to_string()));
        assert_eq!(restored.video.fps, Some(30));
        assert_eq!(restored.audio.input_device_name, Some("Line In".to_string()));
        assert_eq!(restored.audio.output_device_name, Some("Speakers".to_string()));
        assert_eq!(restored.audio.sample_rate, Some(44100));
        assert_eq!(restored.audio.channels, Some(1));
        assert!(!restored.audio.passthrough_enabled);
        assert_eq!(restored.screenshot.save_folder, PathBuf::from(r"C:\shots"));
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
}