//! 引数を取る文字列。
//!
//! 1 関数が 1 件。**関数名がキーで、引数の型と数もコンパイラが確かめる。**
//! 言語ごとに語順が変わるため、呼び出し側で `Text` の断片と値を `format!` で
//! つながず、文全体をここで組み立てる。
//!
//! 値をそのまま差し込むだけの引数（デバイス名、下位のエラー文、パス）は
//! `impl Display` で受ける。呼び出し側で `to_string()` させないため。
//!
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。

use std::fmt::Display;

use super::{language, Language};

// ---- 映像（video::VideoError / ActiveVideo） ----

pub fn video_device_query_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイスを列挙できない: {source}"),
    }
}

pub fn video_device_not_found(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{name}' が見つからない"),
    }
}

pub fn video_camera_open_failed(device: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{device}' を開けない: {source}"),
    }
}

pub fn video_stream_open_failed(device: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{device}' のストリームを開けない: {source}"),
    }
}

/// 解像度だけ取れて、フォーマットが取れなかったとき。
pub fn video_actual_format_unknown(width: u32, height: u32) -> String {
    match language() {
        Language::Japanese => format!("{width}x{height} （フォーマット不明）"),
    }
}

/// フォーマットだけ取れて、解像度が取れなかったとき。
pub fn video_actual_resolution_unknown(format: impl Display) -> String {
    match language() {
        Language::Japanese => format!("（解像度不明） {format}"),
    }
}

// ---- 音声（audio::AudioError） ----
//
// `direction` は `AudioDirection::label()`（`Text::Input` / `Text::Output`）の文言。

pub fn audio_device_enumeration_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスを列挙できない: {source}"),
    }
}

pub fn audio_device_not_found(direction: &str, name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイス '{name}' が見つからない"),
    }
}

pub fn audio_no_default_device(direction: &str) -> String {
    match language() {
        Language::Japanese => format!("既定の{direction}デバイスがない"),
    }
}

pub fn audio_default_config_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの既定の設定を取得できない: {source}"),
    }
}

pub fn audio_supported_configs_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を列挙できない: {source}"),
    }
}

pub fn audio_unsupported_sample_format(direction: &str, format: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "{direction}デバイスのサンプルフォーマット {format} に対応していない（対応: f32 / i16 / u16 / i32）"
        ),
    }
}

pub fn audio_stream_build_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}ストリームを組み立てられない: {source}"),
    }
}

pub fn audio_stream_play_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}ストリームを開始できない: {source}"),
    }
}

// ---- スクリーンショットと効果音（screenshot::ScreenshotError） ----

pub fn screenshot_empty_frame(width: usize, height: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("大きさのない映像フレームはクリップボードへコピーできない: {width}x{height}")
        }
    }
}

pub fn screenshot_frame_too_large(width: impl Display, height: impl Display) -> String {
    match language() {
        Language::Japanese => format!("画像として扱えない大きさのフレーム: {width}x{height}"),
    }
}

pub fn screenshot_frame_too_short(width: usize, height: usize, len: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("映像フレームの画素が足りない: {width}x{height} に対して {len} バイト")
        }
    }
}

pub fn screenshot_clipboard_open_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードを開けない: {source}"),
    }
}

pub fn screenshot_clipboard_write_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードへ画像を書き込めない: {source}"),
    }
}

pub fn sound_file_unreadable(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("効果音ファイル {path} を読み込めないため既定の効果音を使う: {source}")
        }
    }
}

pub fn sound_file_undecodable(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "効果音ファイル {path} を音声として読めないため、撮影時は効果音が鳴らない: {source}"
        ),
    }
}

// ---- ホットキー（hotkey::HotkeyError / keyboard_hook::KeyboardHookError） ----

pub fn hotkey_unsupported_key(key: impl Display) -> String {
    match language() {
        Language::Japanese => format!("未対応のキー: {key}"),
    }
}

/// 同じキーが別のアクションに割り当て済み。`other` はアクション名。
pub fn hotkey_duplicate_assignment(other: impl Display) -> String {
    match language() {
        Language::Japanese => format!("同じキーが「{other}」に割り当てられています"),
    }
}

pub fn hotkey_hook_unavailable(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ホットキーの仕組みを初期化できません: {source}"),
    }
}

pub fn keyboard_hook_install_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("キーボードフックを登録できません: {source}"),
    }
}

pub fn keyboard_hook_wait_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "キー入力を待てなくなったのでホットキーを止めました。アプリを再起動してください: {source}"
        ),
    }
}

// ---- 設定ファイル（settings::SettingsError） ----

pub fn settings_file_not_found(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} が見つからない"),
    }
}

pub fn settings_not_a_file(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} はファイルではない"),
    }
}

/// ファイルへ書き出せない。設定の書き出しとスクリーンショットの保存で共有する。
pub fn file_write_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ書き出せない: {source}"),
    }
}

pub fn settings_import_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を読み込めない: {source}"),
    }
}

// ---- 接続状態（status.rs） ----

pub fn resample_ratio(ratio: f32) -> String {
    match language() {
        Language::Japanese => format!("リサンプル比: {ratio:.4}"),
    }
}

/// `level` は整形済みの割合（「75%」）か `Text::WaterLevelUnknown`。
pub fn buffer_water_level(level: impl Display) -> String {
    match language() {
        Language::Japanese => format!("バッファ水位: {level}"),
    }
}

pub fn underrun_count(count: u32) -> String {
    match language() {
        Language::Japanese => format!("アンダーラン: {count} 回"),
    }
}
