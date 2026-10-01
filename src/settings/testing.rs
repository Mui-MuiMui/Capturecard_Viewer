//! 設定のテストが複数のファイルから使う共有の材料。
//!
//! 設定ファイルの例（FULL_CONFIG / LEGACY_CONFIG）と、そこから 1 行を削る
//! without_key、保存先の候補の例（DESKTOP など）を置く。各ファイルの
//! `mod tests` はここから取る。`mod tests` は 1 ファイルに 1 つだけという
//! 決まりがあるので、共有するものは別のモジュールへ出してある。

// 全項目を明示した設定ファイル。値はすべて既定値と異なるものにしてある。
// 各テストはここから一部を削り、「古い版が書いた設定ファイル」を再現する。
pub(super) const FULL_CONFIG: &str = r#"
[video]
device_name = "Capture Device"
resolution = [1920, 1080]
format = "MJPEG"
fps = 30
backend = "direct_show"
auto_reconnect = false
color_space = "bt601"
color_range = "full"
brightness = 10
contrast = -20
saturation = 30

[audio]
input_source = "video_pin"
input_device_name = "Line In"
output_device_name = "Speakers"
sample_rate = 44100
channels = 1
passthrough_enabled = false
buffer_ms = 120

[screenshot]
destination = "both"
save_folder = 'C:\shots'
format = "png"
jpeg_quality = 60
sound_file = 'sound/custom.mp3'
sound_volume = 50.0

[ui]
volume = 80.0
muted = true
maintain_aspect_ratio = false
last_window_size = [800.0, 600.0]
last_window_pos = [10.0, 20.0]
always_on_top = true
enable_drag_move = false
show_stats_overlay = true
borderless = true

[hotkeys]
screenshot = "Ctrl+S"
toggle_fullscreen = "F11"
"#;

// ホットキーをアクション別にする前の版が書いた設定ファイル。
// [hotkeys] が無く、screenshot セクションに hotkey がある。
pub(super) const LEGACY_CONFIG: &str = r#"
[video]
device_name = "Capture Device"
fps = 30

[audio]
sample_rate = 44100

[screenshot]
save_folder = 'C:\shots'
sound_volume = 50.0
hotkey = "Ctrl+S"

[ui]
volume = 80.0
"#;

// 指定したキーの行を取り除く。項目を 1 つ追加した直後の、
// そのキーだけが存在しない設定ファイルを作るために使う。
pub(super) fn without_key(config: &str, key: &str) -> String {
    let prefix = format!("{} =", key);
    config
        .lines()
        .filter(|line| !line.trim_start().starts_with(&prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

// 保存先の候補。実在しないパスでよい。screenshot_folder_from は
// 候補の存在を確かめず、取れた順に選ぶだけ
pub(super) const DESKTOP: &str = r"C:\Users\tester\Desktop";
pub(super) const HOME: &str = r"C:\Users\tester";
pub(super) const EXE_DIR: &str = r"C:\Program Files\capturecard_viewer";
pub(super) const TEMP: &str = r"C:\Users\tester\AppData\Local\Temp";

// `path` の隣に、この書式（`<名前>.<16 桁の 16 進数>.tmp`）の一時ファイルが残っているか
pub(super) fn has_own_temp_file(path: &std::path::Path) -> bool {
    let name = format!("{}.", path.file_name().unwrap().to_string_lossy());
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            let entry = entry.file_name().to_string_lossy().into_owned();
            entry
                .strip_prefix(&name)
                .and_then(|rest| rest.strip_suffix(".tmp"))
                .is_some_and(|token| {
                    token.len() == 16 && token.chars().all(|c| c.is_ascii_hexdigit())
                })
        })
}
