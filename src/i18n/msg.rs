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

// ---- 「デバイス設定」タブ（ui/device_tab.rs / ui/capability.rs） ----

pub fn video_capability_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("対応形式を取得できませんでした: {reason}"),
    }
}

/// 設定値が選択肢に無いとき。`unit` は値の直後に付ける単位（「 Hz」）。
pub fn out_of_range_note(current: u32, nearest: u32, unit: &str) -> String {
    match language() {
        Language::Japanese => format!(
            "{current}{unit} はこの組み合わせでは使えません。最も近い {nearest}{unit} で開きます"
        ),
    }
}

/// `label` は `Text::SampleRate` / `Text::Channels` の文言。
pub fn choice_note_one_sided(label: &str) -> String {
    match language() {
        Language::Japanese => {
            format!("{label}の選択肢は、対応設定を取得できた側のデバイスだけから作っています")
        }
    }
}

pub fn choice_note_disjoint(label: &str) -> String {
    match language() {
        Language::Japanese => format!(
            "入力と出力で共通の{label}がありません。それぞれ最も近い値で開き、変換して出力します（音質がわずかに落ちます）"
        ),
    }
}

pub fn choice_note_fallback(label: &str) -> String {
    match language() {
        Language::Japanese => {
            format!("{label}の選択肢は既定の一覧です（デバイスの対応設定を取得できていません）")
        }
    }
}

/// `direction` は `AudioDirection::label()` の文言。
pub fn audio_capability_pending(direction: &str) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を取得中..."),
    }
}

pub fn audio_capability_failed(direction: &str, reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{direction}デバイスの対応設定を取得できません: {reason}"),
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
    }
}

pub fn hotkey_capture_heading(action: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ホットキー設定: {action}"),
    }
}

pub fn hotkey_capture_prompt(action: impl Display) -> String {
    match language() {
        Language::Japanese => format!("「{action}」に割り当てるキーの組み合わせを押してください"),
    }
}

// ---- 「その他」タブとプリセット（ui/other_tab.rs / ui/preset.rs） ----

pub fn preset_current(label: impl Display) -> String {
    match language() {
        Language::Japanese => format!("現在: {label}"),
    }
}

/// プリセットを読み込んだあとに値を変えたとき。
pub fn preset_modified(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{name}（変更あり）"),
    }
}

pub fn preset_loaded(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を読み込みました。「適用」または「OK」で反映します")
        }
    }
}

pub fn preset_overwritten(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を上書きしました。「適用」または「OK」で反映します")
        }
    }
}

pub fn preset_deleted(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "プリセット「{name}」を削除しました。取り消すには「キャンセル」を押してください"
        ),
    }
}

pub fn preset_added(name: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("プリセット「{name}」を追加しました。「適用」または「OK」で反映します")
        }
    }
}

// ---- 「接続状態」タブ（ui/status_tab.rs） ----

pub fn consecutive_failures(count: u32) -> String {
    match language() {
        Language::Japanese => format!("連続失敗: {count} 回"),
    }
}

pub fn error_time(time: impl Display) -> String {
    match language() {
        Language::Japanese => format!("発生時刻: {time}"),
    }
}

// ---- 「接続状態」タブへ渡す詳細（app/error_report.rs） ----

pub fn link_device(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("デバイス: {name}"),
    }
}

pub fn link_video(summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("映像: {summary}"),
    }
}

pub fn link_requested_fps(fps: u32) -> String {
    match language() {
        Language::Japanese => format!("要求フレームレート: {fps} fps"),
    }
}

pub fn link_audio_input(device: impl Display, summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("入力: {device}（{summary}）"),
    }
}

pub fn link_audio_output(device: impl Display, summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("出力: {device}（{summary}）"),
    }
}

// ---- 統計 OSD（app/view.rs） ----

pub fn stats_fps(fps: f32, average_ms: f32, samples: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("FPS {fps:.1} (平均間隔 {average_ms:.1}ms / {samples} 件)")
        }
    }
}

pub fn stats_jitter(stddev_ms: f32, min_ms: f32, max_ms: f32) -> String {
    match language() {
        Language::Japanese => {
            format!("ばらつき ±{stddev_ms:.2}ms (最小 {min_ms:.1} / 最大 {max_ms:.1})")
        }
    }
}

pub fn stats_decode(decode_ms: f32, fast_count: u64, fallback_count: u64) -> String {
    match language() {
        Language::Japanese => {
            format!("デコード {decode_ms:.2}ms (高速 {fast_count} / 汎用 {fallback_count})")
        }
    }
}

pub fn stats_since_last_frame(elapsed_ms: f32) -> String {
    match language() {
        Language::Japanese => format!("最終フレーム {elapsed_ms:.0}ms 前"),
    }
}

// ---- 音量とミュート（app/audio_control.rs / app/menu/items.rs） ----

/// 音量の表示。右クリックメニューと OSD で同じ文言を使う。
pub fn volume_percent(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("音量: {volume}%"),
    }
}

pub fn volume_percent_muted(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("音量: {volume}%（ミュート中）"),
    }
}

pub fn unmuted(volume: i32) -> String {
    match language() {
        Language::Japanese => format!("ミュート解除（音量: {volume}%）"),
    }
}

// ---- スクリーンショットの保存（app/screenshot.rs） ----

pub fn screenshot_copied_and_saved(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("クリップボードへコピーし、{path} へ保存した"),
    }
}

pub fn screenshot_saved(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ保存した"),
    }
}

/// 片方の出力先だけ失敗したとき。`done` は成功したほうの説明。
pub fn screenshot_partially_failed(reason: impl Display, done: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{reason}（{done}）"),
    }
}

pub fn screenshot_save_empty_frame(width: usize, height: usize) -> String {
    match language() {
        Language::Japanese => {
            format!("大きさのない映像フレームは保存できない: {width}x{height}")
        }
    }
}

pub fn screenshot_create_dir_failed(dir: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("保存先のディレクトリ {dir} を作成できない: {source}"),
    }
}

pub fn screenshot_image_build_failed(width: u32, height: u32, len: usize) -> String {
    match language() {
        Language::Japanese => format!(
            "映像フレームから画像を組み立てられない: {width}x{height} に対して {len} バイト"
        ),
    }
}

pub fn file_create_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を作成できない: {source}"),
    }
}

pub fn file_flush_failed(path: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} を書き切れない: {source}"),
    }
}

// ---- 設定ダイアログの操作の結果とホットキーの失敗（app/settings_dialog.rs / app/hotkeys.rs） ----

/// 右クリックメニューからプリセットを切り替えたときの OSD。
pub fn preset_switched(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("プリセット: {name}"),
    }
}

pub fn settings_exported(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{path} へ書き出しました"),
    }
}

pub fn settings_imported(path: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("{path} を読み込みました。「適用」または「OK」で反映します")
        }
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
    }
}
