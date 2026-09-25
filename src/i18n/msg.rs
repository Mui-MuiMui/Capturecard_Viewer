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

// 英語の文の途中へ `Text` の語（「Input」「Sample rate」）を入れるときに
// 小文字へ揃える。`Text` の側は単独で見出しやラベルに使うので、文頭の形で持つ
fn mid_sentence(word: &str) -> String {
    word.to_lowercase()
}

// ---- 映像（video::VideoError / ActiveVideo） ----

pub fn video_device_query_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイスを列挙できない: {source}"),
        Language::English => format!("Cannot list video devices: {source}"),
    }
}

pub fn video_device_not_found(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{name}' が見つからない"),
        Language::English => format!("Video device '{name}' not found"),
    }
}

pub fn video_camera_open_failed(device: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{device}' を開けない: {source}"),
        Language::English => format!("Cannot open video device '{device}': {source}"),
    }
}

pub fn video_stream_open_failed(device: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像デバイス '{device}' のストリームを開けない: {source}"),
        Language::English => {
            format!("Cannot open the stream of video device '{device}': {source}")
        }
    }
}

/// 解像度だけ取れて、フォーマットが取れなかったとき。
pub fn video_actual_format_unknown(width: u32, height: u32) -> String {
    match language() {
        Language::Japanese => format!("{width}x{height} （フォーマット不明）"),
        Language::English => format!("{width}x{height} (unknown format)"),
    }
}

/// フォーマットだけ取れて、解像度が取れなかったとき。
pub fn video_actual_resolution_unknown(format: impl Display) -> String {
    match language() {
        Language::Japanese => format!("（解像度不明） {format}"),
        Language::English => format!("(unknown resolution) {format}"),
    }
}

// ---- 音声（audio::AudioError） ----
//
// `direction` は `AudioDirection::label()`（`Text::Input` / `Text::Output`）の文言。

pub fn audio_device_enumeration_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスを列挙できない: {source}"),
        Language::English => format!("Cannot list {} devices: {source}", mid_sentence(direction)),
    }
}

pub fn audio_device_not_found(direction: &str, name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイス '{name}' が見つからない"),
        Language::English => format!("{direction} device '{name}' not found"),
    }
}

pub fn audio_no_default_device(direction: &str) -> String {
    match language() {
        Language::Japanese => format!("既定の{direction}デバイスがない"),
        Language::English => format!("No default {} device", mid_sentence(direction)),
    }
}

pub fn audio_default_config_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの既定の設定を取得できない: {source}"),
        Language::English => format!(
            "Cannot get the default configuration of the {} device: {source}",
            mid_sentence(direction)
        ),
    }
}

pub fn audio_supported_configs_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を列挙できない: {source}"),
        Language::English => format!(
            "Cannot list the supported configurations of the {} device: {source}",
            mid_sentence(direction)
        ),
    }
}

pub fn audio_unsupported_sample_format(direction: &str, format: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "{direction}デバイスのサンプルフォーマット {format} に対応していない（対応: f32 / i16 / u16 / i32）"
        ),
        Language::English => format!(
            "The {} device's sample format {format} is not supported (supported: f32 / i16 / u16 / i32)",
            mid_sentence(direction)
        ),
    }
}

pub fn audio_stream_build_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}ストリームを組み立てられない: {source}"),
        Language::English => format!(
            "Cannot build the {} stream: {source}",
            mid_sentence(direction)
        ),
    }
}

pub fn audio_stream_play_failed(direction: &str, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}ストリームを開始できない: {source}"),
        Language::English => format!(
            "Cannot start the {} stream: {source}",
            mid_sentence(direction)
        ),
    }
}

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

// ---- 接続状態（status.rs） ----

pub fn resample_ratio(ratio: f32) -> String {
    match language() {
        Language::Japanese => format!("リサンプル比: {ratio:.4}"),
        Language::English => format!("Resample ratio: {ratio:.4}"),
    }
}

/// `level` は整形済みの割合（「75%」）か `Text::WaterLevelUnknown`。
pub fn buffer_water_level(level: impl Display) -> String {
    match language() {
        Language::Japanese => format!("バッファ水位: {level}"),
        Language::English => format!("Buffer level: {level}"),
    }
}

pub fn underrun_count(count: u32) -> String {
    match language() {
        Language::Japanese => format!("アンダーラン: {count} 回"),
        Language::English => format!("Underruns: {count}"),
    }
}

// ---- 「デバイス設定」タブ（ui/device_tab.rs / ui/capability.rs） ----

pub fn video_capability_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("対応形式を取得できませんでした: {reason}"),
        Language::English => format!("Could not get the supported formats: {reason}"),
    }
}

/// 設定値が選択肢に無いとき。`unit` は値の直後に付ける単位（「 Hz」）。
pub fn out_of_range_note(current: u32, nearest: u32, unit: &str) -> String {
    match language() {
        Language::Japanese => format!(
            "{current}{unit} はこの組み合わせでは使えません。最も近い {nearest}{unit} で開きます"
        ),
        Language::English => format!(
            "{current}{unit} is not available for this combination. The nearest value, {nearest}{unit}, is used instead"
        ),
    }
}

/// `label` は `Text::SampleRate` / `Text::Channels` の文言。
pub fn choice_note_one_sided(label: &str) -> String {
    match language() {
        Language::Japanese => {
            format!("{label}の選択肢は、対応設定を取得できた側のデバイスだけから作っています")
        }
        Language::English => format!(
            "The {} choices come only from the device whose supported configurations could be read",
            mid_sentence(label)
        ),
    }
}

pub fn choice_note_disjoint(label: &str) -> String {
    match language() {
        Language::Japanese => format!(
            "入力と出力で共通の{label}がありません。それぞれ最も近い値で開き、変換して出力します（音質がわずかに落ちます）"
        ),
        Language::English => format!(
            "Input and output have no {} in common. Each opens with its nearest value and the audio is converted for output (slightly lower quality)",
            mid_sentence(label)
        ),
    }
}

pub fn choice_note_fallback(label: &str) -> String {
    match language() {
        Language::Japanese => {
            format!("{label}の選択肢は既定の一覧です（デバイスの対応設定を取得できていません）")
        }
        Language::English => format!(
            "The {} choices are a default list (the device's supported configurations could not be read)",
            mid_sentence(label)
        ),
    }
}

/// `direction` は `AudioDirection::label()` の文言。
pub fn audio_capability_pending(direction: &str) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を取得中..."),
        Language::English => format!(
            "Querying the supported configurations of the {} device...",
            mid_sentence(direction)
        ),
    }
}

pub fn audio_capability_failed(direction: &str, reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を取得できません: {reason}"),
        Language::English => format!(
            "Cannot get the supported configurations of the {} device: {reason}",
            mid_sentence(direction)
        ),
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

// ---- 「接続状態」タブ（ui/status_tab.rs） ----

pub fn consecutive_failures(count: u32) -> String {
    match language() {
        Language::Japanese => format!("連続失敗: {count} 回"),
        Language::English => format!("Consecutive failures: {count}"),
    }
}

pub fn error_time(time: impl Display) -> String {
    match language() {
        Language::Japanese => format!("発生時刻: {time}"),
        Language::English => format!("Occurred at: {time}"),
    }
}

// ---- 「接続状態」タブへ渡す詳細（app/error_report.rs） ----

pub fn link_device(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("デバイス: {name}"),
        Language::English => format!("Device: {name}"),
    }
}

pub fn link_video(summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像: {summary}"),
        Language::English => format!("Video: {summary}"),
    }
}

pub fn link_requested_fps(fps: u32) -> String {
    match language() {
        Language::Japanese => format!("要求フレームレート: {fps} fps"),
        Language::English => format!("Requested frame rate: {fps} fps"),
    }
}

pub fn link_audio_input(device: impl Display, summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("入力: {device}（{summary}）"),
        Language::English => format!("Input: {device} ({summary})"),
    }
}

pub fn link_audio_output(device: impl Display, summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("出力: {device}（{summary}）"),
        Language::English => format!("Output: {device} ({summary})"),
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
    use crate::i18n::{with_language, Text};

    #[test]
    fn msg_english_places_the_name_mid_sentence() {
        // 語順を言語ごとに組み立てていること。日本語では名前が文頭寄り、
        // 英語では文の途中に来る
        assert_eq!(
            with_language(Language::English, || video_camera_open_failed(
                "Cam", "busy"
            )),
            "Cannot open video device 'Cam': busy"
        );
        assert_eq!(
            video_camera_open_failed("Cam", "busy"),
            "映像デバイス 'Cam' を開けない: busy"
        );
    }

    #[test]
    fn msg_english_lowercases_direction_only_mid_sentence() {
        // 文頭ではそのまま、文の途中では小文字にする
        with_language(Language::English, || {
            let input = Text::Input.get();
            assert_eq!(
                audio_device_not_found(input, "Mic"),
                "Input device 'Mic' not found"
            );
            assert_eq!(audio_no_default_device(input), "No default input device");
            assert_eq!(
                choice_note_fallback(Text::SampleRate.get()),
                "The sample rate choices are a default list (the device's supported configurations could not be read)"
            );
        });
    }

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
