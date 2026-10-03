//! 録画のテストの補助（`#[cfg(test)]` のときだけ組み込む）。
//!
//! `#[ignore]` のテストが使う、フェイクの映像と音声を流して `Session` や窓口（`Recorder`）で
//! 録画する部分と、書いた MP4 を読み戻す部分。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::recorder::{
    Recorder, RecordingEvent, RecordingRequest, RecordingSummary, RecordingTelemetry,
};
use super::replay_config::ReplayConfig;
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
                input: crate::audio::PassthroughInput::Device(Some("Fake Audio Input 1")),
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
        audio_offset_ms: 0,
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

/// 実機の音声ピン付きの映像デバイスで録画した結果（`record_from_video_pin`）。
pub(super) struct VideoPinRun {
    /// 書いた MP4
    pub(super) path: std::path::PathBuf,
    /// 映像と音声それぞれの終わり（100ns）
    pub(super) video_end: i64,
    pub(super) audio_end: i64,
    /// 音声ピンを繋がずに開いたときと、繋いで開いたあとの実効 fps
    pub(super) fps_without_pin: Option<f32>,
    pub(super) fps_with_pin: Option<f32>,
    /// 10 秒ごとに読んだ `(アンダーラン, 捨てたフレーム, 入力の取りこぼし)`
    pub(super) counters: Vec<(Option<u32>, Option<u32>, Option<u32>)>,
}

/// 名前に `hint` を含む DirectShow の映像デバイスを、音声ピンの入力（#388）で開いて
/// `seconds` 秒録画する。**実機が要る。** 出力デバイスへは鳴らさない（ミュート）。
///
/// アプリと同じく、デバイスは STA のスレッド（デバイスワーカーの代わり）で、
/// 録画は MTA のスレッド（このスレッド）で扱う。録画の保存先は `folder`。
pub(super) fn record_from_video_pin(
    hint: &str,
    resolution: (u32, u32),
    seconds: u64,
    folder: &Path,
    audio_offset_ms: i32,
) -> VideoPinRun {
    use crate::audio::{
        AudioCapture, AudioControls, AudioPinFeed, AudioPinState, PassthroughInput,
        PassthroughRequest,
    };
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::repaint::RepaintWaker;
    use crate::video::{
        directshow_display_name, DirectShowCapture, SharedColorConversion, VideoFrames,
    };
    use std::sync::mpsc;

    let frames = VideoFrames::new();
    let audio_tap = AudioTap::new();
    let (ready_tx, ready_rx) = mpsc::channel::<u32>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let device_frames = frames.clone();
    let device_tap = audio_tap.clone();
    let hint = hint.to_string();
    let device = std::thread::spawn(move || {
        let frames = device_frames;
        let feed = AudioPinFeed::new();
        let mut video = DirectShowCapture::new(
            frames.clone(),
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::new(),
            feed.clone(),
        );
        let name = video
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains(&hint))
            .expect("名前に hint を含む DirectShow のデバイスがある");
        let display = directshow_display_name(&name);
        let fps_now = |frames: &VideoFrames| frames.stats().intervals.map(|i| i.fps);

        // 音声ピンを繋がずに開いたときの fps（比べる基準）
        video
            .start_capture(&display, Some(resolution), None, None, false)
            .expect("音声ピンなしで開ける");
        println!("音声ピンなし: {:?}", video.active().map(|a| a.audio_pin));
        std::thread::sleep(Duration::from_secs(6));
        let fps_without_pin = fps_now(&frames);
        video.stop_capture();

        video
            .start_capture(&display, Some(resolution), None, None, true)
            .expect("音声ピン付きで開ける");
        let active = video.active().expect("開いている");
        println!("音声ピンあり: {:?}", active.audio_pin);
        let AudioPinState::Connected(connection) = active.audio_pin else {
            panic!("音声ピンに繋がっていない");
        };
        let controls = Arc::new(AudioControls::default());
        controls.set_muted(true);
        let probe_tap = device_tap.clone();
        let mut audio = AudioCapture::new(controls, device_tap, feed.clone());
        audio
            .start_passthrough(&PassthroughRequest {
                input: PassthroughInput::VideoPin {
                    graph: connection.graph,
                },
                output_device_name: None,
                sample_rate: None,
                channels: None,
                input_capabilities: None,
                output_capabilities: None,
                buffer_ms: 50,
            })
            .expect("音声ピンから開ける");
        println!("音声: {:?}", audio.active());
        ready_tx.send(active.requested_fps).expect("知らせる");

        let mut counters = Vec::new();
        let mut fps_with_pin;
        let started = Instant::now();
        let mut last_push = None;
        let mut video_stalls = Vec::new();
        loop {
            // 10 秒のあいだ 10ms ごとに録画の差し込み口の「最後に積んだ時刻」を見て、
            // 音声ピンからの塊が 40ms 以上途切れたところを拾う（途切れた位置を数で残す）
            let mut stop = false;
            let mut gaps = Vec::new();
            for _ in 0..1000 {
                if stop_rx.recv_timeout(Duration::from_millis(10)).is_ok() {
                    stop = true;
                    break;
                }
                let now = probe_tap.snapshot().last_push;
                if let (Some(previous), Some(now)) = (last_push, now) {
                    let gap: Duration = now.saturating_sub(previous);
                    if gap >= Duration::from_millis(40) {
                        gaps.push((started.elapsed().as_secs_f32(), gap.as_millis()));
                    }
                }
                last_push = now.or(last_push);
                // 映像も同じときに止まっていたか（キャプチャーフィルターごと止まったのか、
                // 音声ピンだけなのかを見分ける）
                if let Some(since) = frames.since_last_frame() {
                    if since >= Duration::from_millis(40) {
                        video_stalls.push((started.elapsed().as_secs_f32(), since.as_millis()));
                    }
                }
            }
            if !gaps.is_empty() {
                println!("音声ピンの塊の途切れ（経過秒, 途切れた ms）: {gaps:?}");
            }
            if !video_stalls.is_empty() {
                println!("映像の途切れ（経過秒, 最後のフレームからの ms）: {video_stalls:?}");
                video_stalls.clear();
            }
            let sample = (
                audio.underrun_count(),
                audio.dropped_frame_count(),
                audio.xrun_count(),
            );
            fps_with_pin = fps_now(&frames);
            println!(
                "観測値: fps {:?}、(アンダーラン, 捨てたフレーム, 取りこぼし) = {:?}、塊 {:?} バイト、水位 {:?}",
                fps_with_pin,
                sample,
                feed.observed_chunk_bytes(),
                audio.resample_status()
            );
            counters.push(sample);
            if stop {
                break;
            }
        }
        audio.stop_capture();
        video.stop_capture();
        (fps_without_pin, fps_with_pin, counters)
    });

    let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
    let _mf = MfPlatform::start().expect("MF を起こせる");
    let nominal_fps = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("デバイスを開けた");
    let (events, received) = std::sync::mpsc::channel();
    let request = RecordingRequest {
        folder: folder.to_path_buf(),
        file_stem: "video_pin".to_string(),
        video_bitrate_kbps: 8000,
        hardware_encoder: true,
        nominal_fps: Some(nominal_fps),
        audio_bitrate_kbps: Some(160),
        audio_offset_ms,
    };
    let telemetry = Arc::new(RecordingTelemetry::default());
    let mut session = Session::begin(request, frames.tap(), audio_tap, telemetry, events)
        .expect("録画を始められる");
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        session.tick().expect("録画を続けられる");
        std::thread::sleep(Duration::from_millis(5));
    }
    session.stop();
    stop_tx.send(()).expect("デバイスのスレッドへ知らせる");
    let (fps_without_pin, fps_with_pin, counters) =
        device.join().expect("デバイスのスレッドが終わる");
    let path = received
        .try_iter()
        .find_map(|event| match event {
            RecordingEvent::Stopped(summary) => Some(summary.path),
            _ => None,
        })
        .expect("録画を保存できた");
    let (video_end, audio_end) = stream_ends(&path);
    VideoPinRun {
        path,
        video_end,
        audio_end,
        fps_without_pin,
        fps_with_pin,
        counters,
    }
}

// ---- 窓口（`Recorder`）にフェイクを流すテストの補助 ----

/// フェイクの映像（720p60 のカラーバーにフレーム番号を焼き込んだもの）と音声（正弦波）を流す。
pub(super) fn start_fakes(
    frames: &crate::video::VideoFrames,
    audio_tap: &AudioTap,
) -> (
    crate::video::FakeVideoCapture,
    crate::audio::FakeAudioCapture,
) {
    use crate::audio::{AudioControls, FakeAudioCapture, FakeAudioOptions, PassthroughRequest};
    use crate::repaint::RepaintWaker;
    use crate::video::{FakeVideoCapture, FakeVideoOptions, SharedColorConversion};
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
    video
        .start_capture(
            Some("Fake Camera 1"),
            Some((1280, 720)),
            Some("YUY2"),
            Some(60),
        )
        .expect("フェイクの映像を開ける");
    let mut audio = FakeAudioCapture::new(
        Arc::new(AudioControls::default()),
        audio_tap.clone(),
        FakeAudioOptions {
            input_count: 1,
            failures_before_success: 0,
            stream_error_after: None,
        },
    );
    audio
        .start_passthrough(&PassthroughRequest {
            input: crate::audio::PassthroughInput::Device(Some("Fake Audio Input 1")),
            output_device_name: Some("Fake Audio Output 1"),
            sample_rate: None,
            channels: None,
            input_capabilities: None,
            output_capabilities: None,
            buffer_ms: 50,
        })
        .expect("フェイクの音声を開ける");
    (video, audio)
}

/// 届いたイベントを集めながら `duration` だけ待つ。
pub(super) fn poll_for(recorder: &mut Recorder, duration: Duration) -> Vec<RecordingEvent> {
    let until = Instant::now() + duration;
    let mut events = Vec::new();
    while Instant::now() < until {
        events.extend(std::iter::from_fn(|| recorder.try_recv()));
        std::thread::sleep(Duration::from_millis(50));
    }
    events
}

/// テストで使うリプレイバッファの設定（ソフトウェアのエンコーダ、4000kbps、音声 160kbps）。
pub(super) fn replay_config(seconds: u32) -> ReplayConfig {
    ReplayConfig {
        seconds,
        video_bitrate_kbps: 4000,
        hardware_encoder: false,
        audio_bitrate_kbps: Some(160),
        nominal_fps: Some(60),
        audio_offset_ms: 0,
    }
}

/// テストで使う録画の要求。設定は `replay_config` と揃える。
pub(super) fn recording_request(folder: &Path, file_stem: &str) -> RecordingRequest {
    RecordingRequest {
        folder: folder.to_path_buf(),
        file_stem: file_stem.to_string(),
        video_bitrate_kbps: 4000,
        hardware_encoder: false,
        nominal_fps: Some(60),
        audio_bitrate_kbps: Some(160),
        audio_offset_ms: 0,
    }
}

/// 書いた MP4 を読み戻した結果。
pub(super) struct Mp4Info {
    /// 長さ（`MF_PD_DURATION`、100ns）
    pub(super) duration: i64,
    /// 先頭の映像のサンプルの時刻（100ns）
    pub(super) first_video_time: i64,
    /// 音声トラックがあるか
    pub(super) has_audio: bool,
}

/// 書いた MP4 を Source Reader で読み戻す。COM（MTA）と MF はここで起こす。
pub(super) fn read_mp4(path: &Path) -> Mp4Info {
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::{
        MFCreateSourceReaderFromURL, MF_PD_DURATION, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
        MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    };

    let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
    let _mf = MfPlatform::start().expect("MF を起こせる");
    let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path), None) }
        .expect("書いた MP4 を開ける");
    let value = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
    }
    .expect("長さを読める");
    let duration = unsafe { value.Anonymous.Anonymous.Anonymous.uhVal } as i64;
    let has_audio =
        unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0) }
            .is_ok();
    let (mut flags, mut time, mut sample) = (0u32, -1i64, None);
    unsafe {
        reader.ReadSample(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            0,
            None,
            Some(&mut flags),
            Some(&mut time),
            Some(&mut sample),
        )
    }
    .expect("読める");
    assert!(sample.is_some(), "映像のサンプルがある");
    Mp4Info {
        duration,
        first_video_time: time,
        has_audio,
    }
}

/// フェイクを流してリプレイバッファを `seconds` 秒で ON にし（`None` なら OFF のまま）、
/// `wait` 待ってから `record` だけ録画して止める。保存した結果と、読み戻した長さ（100ns）を返す。
pub(super) fn record_with_replay(
    seconds: Option<u32>,
    wait: Duration,
    record: Duration,
) -> (RecordingSummary, i64) {
    let frames = crate::video::VideoFrames::new();
    let audio_tap = AudioTap::new();
    let (mut video, mut audio) = start_fakes(&frames, &audio_tap);
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れること");
    let mut recorder = Recorder::new(frames.tap(), audio_tap);
    recorder
        .set_replay(seconds.map(replay_config))
        .expect("リプレイバッファを始められる");
    // OFF のままならスレッドは起きない（OFF のときの負荷は①②と同じ）
    assert_eq!(recorder.has_thread(), seconds.is_some());
    let events = poll_for(&mut recorder, wait);
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, RecordingEvent::ReplayFailed(_))),
        "{events:?}"
    );

    recorder
        .start(recording_request(dir.path(), "replay"))
        .expect("録画を始められる");
    poll_for(&mut recorder, record);
    let lead = recorder.replay_lead();
    assert_eq!(lead.is_some(), seconds.is_some());
    recorder.request_stop();
    let events = poll_for(&mut recorder, Duration::from_secs(3));
    let summary = events
        .iter()
        .find_map(|event| match event {
            RecordingEvent::Stopped(summary) => Some(summary.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("保存できた: {events:?}"));
    // 統計 OSD の値は ms で丸めてある
    assert_eq!(
        summary.replay_lead.map(|lead| lead.as_millis()),
        lead.map(|lead| lead.as_millis())
    );
    // リプレイバッファが ON のままなら、止めてもスレッドは動き続ける。OFF なら止まる
    poll_for(&mut recorder, Duration::from_millis(200));
    assert_eq!(recorder.has_thread(), seconds.is_some());
    recorder.shutdown();
    video.stop_capture();
    audio.stop_capture();

    let mp4 = read_mp4(&summary.path);
    assert!(mp4.has_audio);
    // リングからの書き出しは先頭のキーフレームを 0 にする。①②の経路は録画の開始から
    // 最初のフレームが届くまでの分（1 枚ぶん程度）だけ後ろから始まる
    let limit = if seconds.is_some() { 10_000 } else { 1_000_000 };
    assert!(
        mp4.first_video_time.abs() < limit,
        "先頭の時刻 {}",
        mp4.first_video_time
    );
    (summary, mp4.duration)
}
