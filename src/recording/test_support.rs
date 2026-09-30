//! 録画のテストの補助（`#[cfg(test)]` のときだけ組み込む）。
//!
//! `#[ignore]` のテストが使う、フェイクの映像と音声を流して `Session` で録画する
//! 部分と、書いた MP4 を読み戻す部分。

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::recorder::{RecordingEvent, RecordingRequest, RecordingTelemetry};
use super::session::Session;
use super::RecordingError;
use crate::audio::AudioTap;

/// 書いた MP4 を読み戻し、映像と音声それぞれの終わり（最後のサンプルの時刻 + 長さ、100ns）を返す。
fn stream_ends(path: &Path) -> (i64, i64) {
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::{
        MFCreateSourceReaderFromURL, MF_SOURCE_READERF_ENDOFSTREAM,
        MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    };
    let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path), None) }
        .expect("書いた MP4 を開ける");
    let end_of = |stream: i32| {
        let mut end = 0i64;
        loop {
            let (mut flags, mut time, mut sample) = (0u32, 0i64, None);
            unsafe {
                reader.ReadSample(
                    stream as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )
            }
            .expect("読める");
            if let Some(sample) = sample {
                end = end.max(time + unsafe { sample.GetSampleDuration() }.unwrap_or(0));
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return end;
            }
        }
    };
    (
        end_of(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0),
        end_of(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0),
    )
}

/// フェイクの映像（720p60）で 2 秒録画し、640x480 へ開き直して大きさの変化で止める。
/// `audio_device` が偽なら音声デバイスが無い状態（無音で埋める）。
/// 止まったファイルの映像と音声の終わり（100ns）を返す。
pub(super) fn record_until_size_changes(audio_device: bool) -> (i64, i64) {
    use crate::audio::{AudioControls, FakeAudioCapture, FakeAudioOptions, PassthroughRequest};
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::repaint::RepaintWaker;
    use crate::video::{FakeVideoCapture, FakeVideoOptions, SharedColorConversion, VideoFrames};
    use std::time::Duration;

    let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
    let _mf = MfPlatform::start().expect("MF を起こせる");
    let frames = VideoFrames::new();
    let audio_tap = AudioTap::new();
    let mut video = FakeVideoCapture::new(
        frames.clone(),
        Arc::new(SharedColorConversion::new()),
        RepaintWaker::new(),
        FakeVideoOptions {
            device_count: 1,
            disconnect_after: None,
            failures_before_success: 0,
        },
    );
    let start_video = |video: &mut FakeVideoCapture, size| {
        video
            .start_capture(Some("Fake Camera 1"), Some(size), Some("YUY2"), Some(60))
            .expect("フェイクの映像を開ける");
    };
    start_video(&mut video, (1280, 720));
    let mut audio = FakeAudioCapture::new(
        Arc::new(AudioControls::default()),
        audio_tap.clone(),
        FakeAudioOptions {
            input_count: 1,
            failures_before_success: 0,
            stream_error_after: None,
        },
    );
    if audio_device {
        audio
            .start_passthrough(&PassthroughRequest {
                input_device_name: Some("Fake Audio Input 1"),
                output_device_name: Some("Fake Audio Output 1"),
                sample_rate: None,
                channels: None,
                input_capabilities: None,
                output_capabilities: None,
                buffer_ms: 50,
            })
            .expect("フェイクの音声を開ける");
    }

    let dir = tempfile::tempdir().expect("一時ディレクトリを作れること");
    let (events, received) = std::sync::mpsc::channel();
    let request = RecordingRequest {
        folder: dir.path().to_path_buf(),
        file_stem: "size".to_string(),
        video_bitrate_kbps: 4000,
        hardware_encoder: false,
        nominal_fps: Some(60),
        audio_bitrate_kbps: Some(160),
    };
    let telemetry = Arc::new(RecordingTelemetry::default());
    let mut session = Session::begin(request, frames.tap(), audio_tap, telemetry, events)
        .expect("録画を始められる");
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        session.tick().expect("録画を続けられる");
        std::thread::sleep(Duration::from_millis(5));
    }
    video.stop_capture();
    start_video(&mut video, (640, 480));
    let deadline = Instant::now() + Duration::from_secs(3);
    let error = loop {
        assert!(Instant::now() < deadline, "大きさの変化で止まらない");
        if let Err(error) = session.tick() {
            break error;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let expected = error.clone();
    session.end_with_error(error);
    video.stop_capture();
    audio.stop_capture();

    let failed = received.try_iter().find_map(|event| match event {
        RecordingEvent::Failed { error, summary } => Some((error, summary?.path)),
        _ => None,
    });
    // 音声を仕上げても、知らせる理由は最初の失敗（大きさの変化）のまま
    let (error, path) = failed.expect("閉じたファイルがある");
    assert!(matches!(error, RecordingError::SizeChanged { .. }));
    assert_eq!(error, expected);
    stream_ends(&path)
}
