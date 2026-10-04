//! 引数を取る文字列。
//!
//! 1 関数が 1 件。**関数名がキーで、引数の型と数もコンパイラが確かめる。**
//! 言語ごとに語順が変わるため、呼び出し側で `Text` の断片と値を `format!` で
//! つながず、文全体をここで組み立てる。
//!
//! 値をそのまま差し込むだけの引数（デバイス名、下位のエラー文、パス）は
//! `impl Display` で受ける。呼び出し側で `to_string()` させないため。
//!
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。デバイス（映像・
//! 音声）の接続と状態で使うものは `device_msg.rs`、更新は `update_msg.rs`、録画は
//! `recording_msg.rs` に分けてある。

use std::fmt::Display;

use super::{language, Language};

// ---- スクリーンショットと効果音（screenshot::ScreenshotError） ----

pub fn screenshot_empty_frame(width: usize, height: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("大きさのない映像フレームはクリップボードへコピーできない: {width}x{height}")
        }
        Language::English => {
            format!("Cannot copy a video frame with no size to the clipboard: {width}x{height}")
        }
    }
}

pub fn screenshot_frame_too_large(width: impl Display, height: impl Display) -> String {
    match language() {
        Language::Japanese => format!("画像として扱えない大きさのフレーム: {width}x{height}"),
        Language::English => format!("Frame too large to handle as an image: {width}x{height}"),
    }
}

pub fn screenshot_frame_too_short(width: usize, height: usize, len: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("映像フレームの画素が足りない: {width}x{height} に対して {len} バイト")
        }
        Language::English => {
            format!("Video frame is missing pixels: {len} bytes for {width}x{height}")
        }
    }
}

pub fn screenshot_clipboard_open_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードを開けない: {source}"),
        Language::English => format!("Cannot open the clipboard: {source}"),
    }
}

pub fn screenshot_clipboard_write_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードへ画像を書き込めない: {source}"),
        Language::English => format!("Cannot write the image to the clipboard: {source}"),
    }
}

pub fn sound_file_unreadable(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("効果音ファイル {path} を読み込めないため既定の効果音を使う: {source}")
        }
        Language::English => {
            format!("Cannot read sound file {path}, so the default sound is used instead: {source}")
        }
    }
}

pub fn sound_file_undecodable(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "効果音ファイル {path} を音声として読めないため、撮影時は効果音が鳴らない: {source}"
        ),
        Language::English => format!(
            "Cannot decode sound file {path} as audio, so no sound plays when taking screenshots: {source}"
        ),
    }
}

pub fn sound_output_unavailable(source: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("音声の出力先を開けないため、効果音が鳴らない: {source}")
        }
        Language::English => {
            format!("Cannot open the audio output, so the screenshot sound does not play: {source}")
        }
    }
}

// ---- ホットキー（hotkey::HotkeyError / keyboard_hook::KeyboardHookError） ----

pub fn hotkey_unsupported_key(key: impl Display) -> String {
    match language() {
        Language::Japanese => format!("未対応のキー: {key}"),
        Language::English => format!("Unsupported key: {key}"),
    }
}

/// 同じキーが別のアクションに割り当て済み。`other` はアクション名。
pub fn hotkey_duplicate_assignment(other: impl Display) -> String {
    match language() {
        Language::Japanese => format!("同じキーが「{other}」に割り当てられています"),
        Language::English => format!("The same key is already assigned to \"{other}\""),
    }
}

pub fn hotkey_hook_unavailable(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ホットキーの仕組みを初期化できません: {source}"),
        Language::English => format!("Cannot initialize hotkeys: {source}"),
    }
}

pub fn keyboard_hook_install_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("キーボードフックを登録できません: {source}"),
        Language::English => format!("Cannot install the keyboard hook: {source}"),
    }
}

pub fn keyboard_hook_wait_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "キー入力を待てなくなったのでホットキーを止めました。アプリを再起動してください: {source}"
        ),
        Language::English => format!(
            "Hotkeys were stopped because key input can no longer be received. Please restart the app: {source}"
        ),
    }
}

// ---- 設定ファイル（settings::SettingsError） ----

pub fn settings_file_not_found(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} が見つからない"),
        Language::English => format!("{path} not found"),
    }
}

pub fn settings_not_a_file(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} はファイルではない"),
        Language::English => format!("{path} is not a file"),
    }
}

/// ファイルへ書き出せない。設定の書き出しとスクリーンショットの保存で共有する。
pub fn file_write_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ書き出せない: {source}"),
        Language::English => format!("Cannot write to {path}: {source}"),
    }
}

pub fn settings_import_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を読み込めない: {source}"),
        Language::English => format!("Cannot read {path}: {source}"),
    }
}

/// 保存先（`%AppData%` の設定ファイル）の置き場所が分からない。
pub fn settings_location_unavailable(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("保存先が分からない: {source}"),
        Language::English => format!("Cannot determine where to save: {source}"),
    }
}

// ---- 「ホットキー」タブと入力ダイアログ（ui/hotkeys_tab.rs / ui/hotkey_capture.rs） ----

/// 同じキーが割り当てられたアクション名を並べた警告。
pub fn hotkey_duplicates_warning(actions: &[&str]) -> String {
    match language() {
        Language::Japanese => format!(
            "同じキーが複数のアクションに割り当てられています（{}）。適用しても、上にある側だけが有効になります。",
            actions.join("、")
        ),
        Language::English => format!(
            "The same key is assigned to more than one action ({}). After applying, only the one listed higher takes effect.",
            actions.join(", ")
        ),
    }
}

/// 登録できなかったホットキーの一覧の 1 行。
pub fn hotkey_assignment_error_row(
    action: impl Display,
    hotkey: impl Display,
    reason: impl Display,
) -> String {
    match language() {
        Language::Japanese => format!("{action}（{hotkey}）— {reason}"),
        Language::English => format!("{action} ({hotkey}) — {reason}"),
    }
}

pub fn hotkey_capture_heading(action: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ホットキー設定: {action}"),
        Language::English => format!("Hotkey: {action}"),
    }
}

pub fn hotkey_capture_prompt(action: impl Display) -> String {
    match language() {
        Language::Japanese => format!("「{action}」に割り当てるキーの組み合わせを押してください"),
        Language::English => format!("Press the key combination to assign to \"{action}\""),
    }
}

// ---- 「その他」タブとプリセット（ui/other_tab.rs / ui/preset.rs） ----

pub fn preset_current(label: impl Display) -> String {
    match language() {
        Language::Japanese => format!("現在: {label}"),
        Language::English => format!("Current: {label}"),
    }
}

/// プリセットを読み込んだあとに値を変えたとき。
pub fn preset_modified(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{name}（変更あり）"),
        Language::English => format!("{name} (modified)"),
    }
}

pub fn preset_loaded(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を読み込みました。「適用」または「OK」で反映します")
        }
        Language::English => format!("Loaded preset \"{name}\". Press Apply or OK to use it"),
    }
}

pub fn preset_overwritten(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を上書きしました。「適用」または「OK」で反映します")
        }
        Language::English => {
            format!("Overwrote preset \"{name}\". Press Apply or OK to use it")
        }
    }
}

pub fn preset_deleted(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "プリセット「{name}」を削除しました。取り消すには「キャンセル」を押してください"
        ),
        Language::English => format!("Deleted preset \"{name}\". Press Cancel to undo"),
    }
}

pub fn preset_added(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を追加しました。「適用」または「OK」で反映します")
        }
        Language::English => format!("Added preset \"{name}\". Press Apply or OK to use it"),
    }
}

// ---- 統計 OSD（app/view.rs） ----

pub fn stats_fps(fps: f32, average_ms: f32, samples: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("FPS {fps:.1} (平均間隔 {average_ms:.1}ms / {samples} 件)")
        }
        Language::English => {
            format!("FPS {fps:.1} (avg interval {average_ms:.1}ms / {samples} samples)")
        }
    }
}

pub fn stats_jitter(stddev_ms: f32, min_ms: f32, max_ms: f32) -> String {
    match language() {
        Language::Japanese => {
            format!("ばらつき ±{stddev_ms:.2}ms (最小 {min_ms:.1} / 最大 {max_ms:.1})")
        }
        Language::English => {
            format!("Jitter ±{stddev_ms:.2}ms (min {min_ms:.1} / max {max_ms:.1})")
        }
    }
}

pub fn stats_decode(decode_ms: f32, fast_count: u64, fallback_count: u64) -> String {
    match language() {
        Language::Japanese => {
            format!("デコード {decode_ms:.2}ms (高速 {fast_count} / 汎用 {fallback_count})")
        }
        Language::English => {
            format!("Decode {decode_ms:.2}ms (fast {fast_count} / generic {fallback_count})")
        }
    }
}

pub fn stats_since_last_frame(elapsed_ms: f32) -> String {
    match language() {
        Language::Japanese => format!("最終フレーム {elapsed_ms:.0}ms 前"),
        Language::English => format!("Last frame {elapsed_ms:.0}ms ago"),
    }
}

/// 表示までの遅れ（#455）。統計 OSD と「接続状態」タブの映像の欄で同じ文言を使う。
/// 測っているのはフレームの到着からテクスチャを更新するまで（画面に出るまでではない）
pub fn stats_display_latency(average_ms: f32, max_ms: f32) -> String {
    match language() {
        Language::Japanese => format!(
            "表示までの遅れ: 平均 {average_ms:.1}ms / 最大 {max_ms:.1}ms (到着→テクスチャ更新、直近 1 秒)"
        ),
        Language::English => format!(
            "Display latency: avg {average_ms:.1}ms / max {max_ms:.1}ms (arrival→texture update, last 1 s)"
        ),
    }
}

/// 描画のバックエンド（#456 の (2)）。`label` は `renderer::RendererChoice::label` の技術名
/// （`wgpu Dx12 Mailbox / <アダプター名>` や `glow (OpenGL)`）で、訳さない
pub fn stats_renderer(label: impl Display) -> String {
    match language() {
        Language::Japanese => format!("描画 {label}"),
        Language::English => format!("Renderer {label}"),
    }
}

// ---- 音量とミュート（app/audio_control.rs / app/menu/items.rs） ----

/// 音量の表示。右クリックメニューと OSD で同じ文言を使う。
pub fn volume_percent(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("音量: {volume}%"),
        Language::English => format!("Volume: {volume}%"),
    }
}

pub fn volume_percent_muted(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("音量: {volume}%（ミュート中）"),
        Language::English => format!("Volume: {volume}% (muted)"),
    }
}

pub fn unmuted(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("ミュート解除（音量: {volume}%）"),
        Language::English => format!("Unmuted (volume: {volume}%)"),
    }
}

// ---- スクリーンショットの保存（app/screenshot.rs） ----

pub fn screenshot_copied_and_saved(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードへコピーし、{path} へ保存した"),
        Language::English => format!("Copied to the clipboard and saved to {path}"),
    }
}

pub fn screenshot_saved(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ保存した"),
        Language::English => format!("Saved to {path}"),
    }
}

/// 片方の出力先だけ失敗したとき。`done` は成功したほうの説明。
pub fn screenshot_partially_failed(reason: impl Display, done: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{reason}（{done}）"),
        Language::English => format!("{reason} ({done})"),
    }
}

pub fn screenshot_save_empty_frame(width: usize, height: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("大きさのない映像フレームは保存できない: {width}x{height}")
        }
        Language::English => format!("Cannot save a video frame with no size: {width}x{height}"),
    }
}

pub fn screenshot_create_dir_failed(dir: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("保存先のディレクトリ {dir} を作成できない: {source}"),
        Language::English => format!("Cannot create the save folder {dir}: {source}"),
    }
}

pub fn screenshot_image_build_failed(width: u32, height: u32, len: usize) -> String {
    match language() {
        Language::Japanese => format!(
            "映像フレームから画像を組み立てられない: {width}x{height} に対して {len} バイト"
        ),
        Language::English => {
            format!("Cannot build an image from the video frame: {len} bytes for {width}x{height}")
        }
    }
}

pub fn file_create_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を作成できない: {source}"),
        Language::English => format!("Cannot create {path}: {source}"),
    }
}

pub fn file_flush_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を書き切れない: {source}"),
        Language::English => format!("Cannot finish writing {path}: {source}"),
    }
}

// ---- 設定ダイアログの操作の結果とホットキーの失敗（app/settings_dialog.rs / app/hotkeys.rs） ----

/// 右クリックメニューからプリセットを切り替えたときの OSD。
pub fn preset_switched(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("プリセット: {name}"),
        Language::English => format!("Preset: {name}"),
    }
}

pub fn settings_exported(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ書き出しました"),
        Language::English => format!("Exported to {path}"),
    }
}

pub fn settings_imported(path: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("{path} を読み込みました。「適用」または「OK」で反映します")
        }
        Language::English => format!("Imported {path}. Press Apply or OK to use it"),
    }
}

/// トーストに出す、登録できなかったホットキーの 1 件。
pub fn hotkey_error_summary_item(
    action: impl Display,
    hotkey: impl Display,
    reason: impl Display,
) -> String {
    match language() {
        Language::Japanese => format!("{action}（{hotkey}）: {reason}"),
        Language::English => format!("{action} ({hotkey}): {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::with_language;

    #[test]
    fn msg_english_joins_actions_with_commas() {
        assert_eq!(
            with_language(Language::English, || hotkey_duplicates_warning(&[
                "Screenshot",
                "Toggle mute"
            ])),
            "The same key is assigned to more than one action (Screenshot, Toggle mute). After applying, only the one listed higher takes effect."
        );
    }
}
