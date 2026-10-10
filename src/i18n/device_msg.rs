//! 引数を取る文字列のうち、デバイス（映像・音声）の接続と状態で使うもの。
//! 映像・音声のエラー（`video::VideoError` / `audio::AudioError`）、「Windows 側にも
//! 見えていない」、接続状態の観測値、「デバイス設定」タブ、「接続状態」タブ。
//!
//! 書き方の決まりは `msg.rs` と同じ（1 関数が 1 件、文全体をここで組み立てる）。
//! 並びは使う場所ごとにまとめてある。足すときは近い塊の末尾へ置く。

use std::fmt::Display;

use super::{language, Language};
use crate::video::{CpuReason, Yuy2Conversion, GPU_CONVERT_ENV};

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
            format!("つなげない: {source}")
        }
        (PinFailure::Connect(source), Language::English) => {
            format!("cannot connect: {source}")
        }
        (PinFailure::Run(source), Language::Japanese) => {
            format!("つなぐと映像を動かせないので外した: {source}")
        }
        (PinFailure::Run(source), Language::English) => {
            format!("disconnected because the video could not run with it: {source}")
        }
    }
}

/// 入力が音声ピンなのに、音声ピンに繋げなかったので音声を開かずに待つ理由。
pub fn audio_pin_connect_failed(failure: &crate::audio::PinFailure) -> String {
    let reason = audio_pin_failure(failure);
    match language() {
        Language::Japanese => format!("映像デバイスの音声をつなげませんでした（{reason}）"),
        Language::English => {
            format!("Could not connect the video device's audio ({reason})")
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

/// 統計 OSD の音声の行に、入力が映像デバイスの音声ピンであることを「映像デバイスの音声」と添える
/// （`status::format_osd_audio_line`、#394）
pub fn via_audio_pin(line: &str) -> String {
    match language() {
        Language::Japanese => format!("{line}（映像デバイスの音声）"),
        Language::English => format!("{line} (video device audio)"),
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

/// YUY2 をどこで RGB にしているか（#456）。「接続状態」タブの映像の欄に出す。
/// CPU のときは理由も出す
pub fn link_yuy2_conversion(conversion: &Yuy2Conversion) -> String {
    match (conversion, language()) {
        (Yuy2Conversion::Gpu, Language::Japanese) => "YUY2 の変換: GPU（シェーダー）".to_string(),
        (Yuy2Conversion::Gpu, Language::English) => "YUY2 conversion: GPU (shader)".to_string(),
        (Yuy2Conversion::Cpu(reason), Language::Japanese) => {
            format!("YUY2 の変換: CPU（{}）", cpu_reason(reason))
        }
        (Yuy2Conversion::Cpu(reason), Language::English) => {
            format!("YUY2 conversion: CPU ({})", cpu_reason(reason))
        }
    }
}

/// GPU から CPU へ自動で戻したときに、統計 OSD の「デコード」の行の下へ出す 1 行（#456）
pub fn stats_gpu_fallback(reason: &CpuReason) -> String {
    match language() {
        Language::Japanese => format!("GPU の変換を止めて CPU へ戻した: {}", cpu_reason(reason)),
        Language::English => format!("Fell back from GPU to CPU: {}", cpu_reason(reason)),
    }
}

/// CPU で変換している理由
fn cpu_reason(reason: &CpuReason) -> String {
    let ja = language() == Language::Japanese;
    match reason {
        CpuReason::DisabledByEnv if ja => format!("{GPU_CONVERT_ENV}=0 で切ってある"),
        CpuReason::DisabledByEnv => format!("turned off by {GPU_CONVERT_ENV}=0"),
        CpuReason::DisabledBySetting if ja => "設定で CPU を選んでいる".to_string(),
        CpuReason::DisabledBySetting => "CPU is selected in the settings".to_string(),
        CpuReason::NoGl if ja => "OpenGL のコンテキストが無い".to_string(),
        CpuReason::NoGl => "no OpenGL context".to_string(),
        CpuReason::SoftwareRenderer(renderer) if ja => {
            format!("ソフトウェア描画（{renderer}）")
        }
        CpuReason::SoftwareRenderer(renderer) => format!("software rendering ({renderer})"),
        CpuReason::TooSlow if ja => "描画が映像に追いつかない".to_string(),
        CpuReason::TooSlow => "drawing cannot keep up with the video".to_string(),
        CpuReason::Unavailable(detail) if ja => format!("GPU で変換できない: {detail}"),
        CpuReason::Unavailable(detail) => format!("cannot convert on the GPU: {detail}"),
    }
}

/// 設定の形式で開けず YUY2 で開いたことを伝える行（#81）。Media Foundation の経路だけ
pub fn link_video_format_fallback(requested: &str) -> String {
    match language() {
        Language::Japanese => format!("形式: {requested} では開けないので YUY2 で開いた"),
        Language::English => format!("Format: could not open as {requested}, opened as YUY2"),
    }
}

pub fn link_audio_input(device: impl Display, summary: impl Display) -> String {
    match language() {
        Language::Japanese => format!("入力: {device}（{summary}）"),
        Language::English => format!("Input: {device} ({summary})"),
    }
}

/// 入力が映像デバイスの音声ピンのときの入力の行（#388）。`device` は映像デバイスの名前で、
/// 「(DirectShow)」の印は外して出す（設定ダイアログの項目名と揃える、#409）
pub fn link_audio_input_video_pin(device: &str, summary: impl Display) -> String {
    let device = crate::video::directshow_friendly_name(device).unwrap_or(device);
    match language() {
        Language::Japanese => format!("入力: 映像デバイスの音声（{device}、{summary}）"),
        Language::English => {
            format!("Input: video device audio ({device}, {summary})")
        }
    }
}

/// 音声ピンの塊が長くてリングバッファを広げたときのバッファの行（#388）
pub fn link_audio_buffer_widened(configured_ms: u32, actual_ms: u32, chunk_ms: u32) -> String {
    match language() {
        Language::Japanese => format!(
            "バッファ: 設定 {configured_ms} ms → 実際 {actual_ms} ms（映像デバイスの音声が {chunk_ms} ms ごとに届くため）"
        ),
        Language::English => format!(
            "Buffer: {configured_ms} ms set → {actual_ms} ms used (the video device delivers audio in {chunk_ms} ms chunks)"
        ),
    }
}

/// 映像の欄に出す音声ピンの行（#388）。対象外（Media Foundation など）なら `None`
pub fn link_audio_pin(state: &crate::audio::AudioPinState) -> Option<String> {
    use crate::audio::AudioPinState;
    let japanese = language() == Language::Japanese;
    let text = match state {
        AudioPinState::NotApplicable => return None,
        AudioPinState::Missing if japanese => "映像デバイスの音声: なし".to_string(),
        AudioPinState::Missing => "Video device audio: none".to_string(),
        AudioPinState::Available if japanese => {
            "映像デバイスの音声: あり（使っていない）".to_string()
        }
        AudioPinState::Available => "Video device audio: present (not in use)".to_string(),
        AudioPinState::Connected(connection) => {
            let summary = connection.format.summary();
            let chunk = connection
                .chunk_bytes
                .and_then(|bytes| connection.format.chunk_ms(bytes));
            match (chunk, japanese) {
                (Some(ms), true) => {
                    format!("映像デバイスの音声: 使っている {summary}、{ms} ms ごと")
                }
                (Some(ms), false) => {
                    format!("Video device audio: in use {summary}, {ms} ms chunks")
                }
                (None, true) => format!("映像デバイスの音声: 使っている {summary}"),
                (None, false) => format!("Video device audio: in use {summary}"),
            }
        }
        AudioPinState::Failed(failure) => {
            let reason = audio_pin_failure(failure);
            if japanese {
                format!("映像デバイスの音声: つなげなかった（{reason}）")
            } else {
                format!("Video device audio: not connected ({reason})")
            }
        }
    };
    Some(text)
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
    fn link_audio_pin_shows_each_state_and_hides_not_applicable() {
        use crate::audio::{AudioPinState, PinConnection, PinFailure, PinFormat, PinSampleType};
        let connection = PinConnection {
            graph: 1,
            device: "AVerMedia GC551 Video Capture (DirectShow)".to_string(),
            format: PinFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_type: PinSampleType::I16,
            },
            chunk_bytes: Some(1920),
        };
        assert_eq!(link_audio_pin(&AudioPinState::NotApplicable), None);
        assert_eq!(
            link_audio_pin(&AudioPinState::Connected(connection.clone())).as_deref(),
            Some("映像デバイスの音声: 使っている 48000Hz 2ch 16bit、10 ms ごと")
        );
        assert_eq!(
            link_audio_pin(&AudioPinState::Available).as_deref(),
            Some("映像デバイスの音声: あり（使っていない）")
        );
        assert_eq!(
            link_audio_pin(&AudioPinState::Missing).as_deref(),
            Some("映像デバイスの音声: なし")
        );
        let failed = link_audio_pin(&AudioPinState::Failed(PinFailure::Run("E_FAIL".into())))
            .expect("繋げなかった旨を出す");
        assert!(failed.contains("E_FAIL"), "{failed}");
        let without_chunk = PinConnection {
            chunk_bytes: None,
            ..connection
        };
        assert_eq!(
            with_language(Language::English, || link_audio_pin(
                &AudioPinState::Connected(without_chunk)
            ))
            .as_deref(),
            Some("Video device audio: in use 48000Hz 2ch 16bit")
        );
    }

    #[test]
    fn link_audio_input_video_pin_hides_the_directshow_mark() {
        assert_eq!(
            link_audio_input_video_pin("AVerMedia GC551 Video Capture (DirectShow)", "48000Hz 2ch"),
            "入力: 映像デバイスの音声（AVerMedia GC551 Video Capture、48000Hz 2ch）"
        );
    }

    #[test]
    fn link_video_format_fallback_names_the_requested_format() {
        assert_eq!(
            link_video_format_fallback("MJPEG"),
            "形式: MJPEG では開けないので YUY2 で開いた"
        );
        assert_eq!(
            with_language(Language::English, || link_video_format_fallback("NV12")),
            "Format: could not open as NV12, opened as YUY2"
        );
    }

    #[test]
    fn link_audio_buffer_widened_shows_the_reason() {
        assert_eq!(
            link_audio_buffer_widened(50, 1000, 500),
            "バッファ: 設定 50 ms → 実際 1000 ms（映像デバイスの音声が 500 ms ごとに届くため）"
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
