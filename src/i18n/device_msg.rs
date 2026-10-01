//! 引数を取る文字列のうち、デバイス（映像・音声）の接続と状態で使うもの。
//! 映像・音声のエラー（`video::VideoError` / `audio::AudioError`）、「Windows 側にも
//! 見えていない」、接続状態の観測値、「デバイス設定」タブ、「接続状態」タブ。
//!
//! 書き方の決まりは `msg.rs` と同じ（1 関数が 1 件、文全体をここで組み立てる）。
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

// ---- 映像デバイスの音声ピン（audio::PinFailure、app::monitor_audio_pin、#388） ----

/// 音声ピンに繋げなかった理由（`PinFailure`）だけを 1 行で表す。
pub fn audio_pin_failure(failure: &crate::audio::PinFailure) -> String {
    use crate::audio::PinFailure;
    match (failure, language()) {
        (PinFailure::Connect(source), Language::Japanese) => {
            format!("音声ピンとつなげない: {source}")
        }
        (PinFailure::Connect(source), Language::English) => {
            format!("cannot connect the audio pin: {source}")
        }
        (PinFailure::Run(source), Language::Japanese) => {
            format!("音声ピンをつなぐと映像を動かせないので外した: {source}")
        }
        (PinFailure::Run(source), Language::English) => {
            format!("removed the audio pin because the video could not run with it: {source}")
        }
    }
}

/// 入力が音声ピンなのに、音声ピンに繋げなかったので音声を開かずに待つ理由。
pub fn audio_pin_connect_failed(failure: &crate::audio::PinFailure) -> String {
    let reason = audio_pin_failure(failure);
    match language() {
        Language::Japanese => format!("映像デバイスの音声ピンに繋げませんでした（{reason}）"),
        Language::English => {
            format!("Could not connect the audio pin of the video device ({reason})")
        }
    }
}

// ---- Windows 側にも見えていない（app::monitor::DeviceNotVisible） ----

pub fn device_not_visible_not_listed(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "'{name}' が Windows 側にも見えていない可能性があります（ほかのデバイスは見えています）。デバイスマネージャーで接続を確認してください"
        ),
        Language::English => format!(
            "Windows may not see '{name}' either (other devices are visible). Check the connection in Device Manager"
        ),
    }
}

/// 接続の失敗の理由に「Windows 側にも見えていない」を添える。
///
/// **案内を前に置く。** トーストは 60 文字で切り詰めるので、後ろに置くと
/// 失敗の理由（デバイス名を含んで長い）に押し出されて読めなくなる。
pub fn failure_with_device_not_visible(notice: impl Display, reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{notice}（{reason}）"),
        Language::English => format!("{notice} ({reason})"),
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

pub fn dropped_frame_count(count: u32) -> String {
    match language() {
        Language::Japanese => format!("満杯で捨てた: {count} フレーム"),
        Language::English => format!("Dropped (buffer full): {count} frames"),
    }
}

/// cpal が知らせた入力の取りこぼし（`Xrun`）の回数（Issue #377）。
/// 音声を開いていない（`None`）ときは「-」を出す。
pub fn xrun_count(count: Option<u32>) -> String {
    match (language(), count) {
        (Language::Japanese, Some(count)) => format!("入力の取りこぼし: {count} 回"),
        (Language::Japanese, None) => "入力の取りこぼし: -".to_string(),
        (Language::English, Some(count)) => format!("Input glitches (xrun): {count}"),
        (Language::English, None) => "Input glitches (xrun): -".to_string(),
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

/// 実際に開いた経路（Media Foundation / DirectShow）。`api` は `CaptureApi::label`
pub fn link_capture_api(api: impl Display) -> String {
    match language() {
        Language::Japanese => format!("開き方: {api}"),
        Language::English => format!("Opened with: {api}"),
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
    fn xrun_count_shows_the_number_or_a_dash() {
        assert_eq!(xrun_count(Some(3)), "入力の取りこぼし: 3 回");
        assert_eq!(xrun_count(None), "入力の取りこぼし: -");
        assert_eq!(
            with_language(Language::English, || xrun_count(Some(0))),
            "Input glitches (xrun): 0"
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
}
