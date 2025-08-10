use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_passthrough_enabled() -> bool {
    true // デフォルトで音声パススルーは有効
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSettings {
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub screenshot: ScreenshotSettings,
    pub ui: UiSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSettings {
    pub device_name: Option<String>,
    pub resolution: Option<(u32, u32)>,
    pub format: Option<String>,
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSettings {
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>, 
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    #[serde(default = "default_passthrough_enabled")]
    pub passthrough_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotSettings {
    pub save_folder: PathBuf,
    pub sound_file: Option<PathBuf>,
    pub sound_volume: f32,
    pub hotkey: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSettings {
    pub volume: f32,
    pub maintain_aspect_ratio: bool,
    pub last_window_size: Option<(f32, f32)>,
    pub last_window_pos: Option<(f32, f32)>,
    pub always_on_top: bool,
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