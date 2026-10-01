//! 実機なしで動くフェイクの音声デバイス。
//!
//! 環境変数 `CAPTURECARD_VIEWER_FAKE_DEVICES` を指定して起動したときだけ、
//! `AudioCapture` の代わりに使われる（選ぶのは `app::worker::DeviceWorker::spawn`、
//! trait に包むのは `app::backend::fake`）。指定が無ければ作られない。
//!
//! 入力は正弦波を吐き、出力は書き込みを捨てる。**間の経路は本物と同じ**で、
//! リングバッファ・開く設定の選択（`choose_passthrough_configs`）・入出力の
//! 形の変換（`PassthroughConverter`）・クロックドリフト補正の水位
//! （`ResampleTelemetry`）・音量とミュート（`AudioControls`）・アンダーランの
//! 数え方は、cpal のコールバックと同じ関数（`stream::process_input` /
//! `stream_output::process_output`）を通る。cpal のコールバックスレッドの代わりに、
//! 10ms ごとに起きるスレッドを入力と出力に 1 本ずつ立てる。
//!
//! | デバイス | 形 |
//! |---|---|
//! | Fake Audio Input 1, 2, … | 48kHz 2ch。1 番が 440Hz、2 番が 880Hz、… の正弦波 |
//! | Fake Audio Output 1 | 48kHz 2ch（入力と揃うので変換しない。ドリフト補正は動く） |
//! | Fake Audio Output 2 | 44.1kHz 1ch（入力と揃わないので変換し、ドリフト補正も動く） |

use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{info, warn};
use ringbuf::traits::Split;
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capabilities::AudioCapabilities;
use super::capture::{ring_buffer_samples, target_water_level, PassthroughRequest};
use super::controls::AudioControls;
use super::convert::PassthroughConverter;
use super::fake_stream::{spawn_named, DiscardOutput, SineInput};
use super::resample::{ResampleStatus, ResampleTelemetry};
use super::stream_config::{choose_passthrough_configs, resolve_ranges};
use super::tap::AudioTap;
use super::{ActiveAudio, AudioDirection, AudioError};

const INPUT_NAME_PREFIX: &str = "Fake Audio Input";
const OUTPUT_NAME_PREFIX: &str = "Fake Audio Output";

/// 入力デバイスの形。どの入力も同じ
const INPUT_SAMPLE_RATE: u32 = 48_000;
const INPUT_CHANNELS: u16 = 2;

/// 出力デバイスの形。`(サンプリングレート, チャンネル数)`。
/// 1 番は入力と揃い、2 番は揃わない（変換の経路を通すため。ドリフト補正はどちらにも掛かる）
const OUTPUTS: [(u32, u16); 2] = [(48_000, 2), (44_100, 1)];

/// 1 番の入力の正弦波の周波数。n 番はこの n 倍
const BASE_FREQUENCY_HZ: f64 = 440.0;

/// フェイクの音声デバイスの振る舞い。環境変数から組み立てる
/// （`app::backend::fake`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeAudioOptions {
    /// 名乗る入力デバイスの台数。1 以上
    pub input_count: u32,
    /// 最初にこの回数だけ開くのに失敗する（接続失敗と再試行の再現）
    pub failures_before_success: u32,
    /// 開いてからこの時間が経つと、ストリームのエラーを 1 回立てる
    /// （`take_stream_error` が真を返す）。開き直すと数え直す
    pub stream_error_after: Option<Duration>,
}

/// デバイス 1 台の形。
#[derive(Debug, Clone, PartialEq)]
struct FakeDevice {
    name: String,
    sample_rate: u32,
    channels: u16,
    /// 入力なら正弦波の周波数。出力では使わない
    frequency_hz: f64,
}

impl FakeDevice {
    /// 対応設定。実機の WASAPI 共有モードと同じく、既定の形 1 つだけを返す
    fn configs(&self) -> Vec<SupportedStreamConfigRange> {
        vec![SupportedStreamConfigRange::new(
            self.channels,
            self.sample_rate,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )]
    }

    fn default_config(&self) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            self.channels,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )
    }
}

/// 開いているストリーム。入出力のスレッドと、それぞれを止める送り口。
struct FakeAudioStream {
    stop_input: Sender<()>,
    stop_output: Sender<()>,
    input: JoinHandle<()>,
    output: JoinHandle<()>,
}

/// フェイクの音声デバイス。`AudioCapture` と同じ窓口を持つ。
///
/// **デバイスワーカースレッド（`app::worker_loop`）だけが触る。** エラーの旗と
/// アンダーランの数え手を開き直すたびに新しい `Arc` へ差し替えるのも、
/// `AudioCapture` と同じ。
pub struct FakeAudioCapture {
    controls: Arc<AudioControls>,
    /// 録画へ回す差し込み口。入力のスレッドが `process_input` 越しに積む（本物と同じ経路）
    tap: AudioTap,
    options: FakeAudioOptions,
    /// シナリオ（`failures_before_success`）で、あと何回失敗させるか
    remaining_failures: u32,
    stream: Option<FakeAudioStream>,
    active: Option<ActiveAudio>,
    resample_telemetry: Option<Arc<ResampleTelemetry>>,
    /// ストリームのエラーの旗。窓口と差し替えの作法を `AudioCapture` と揃える。
    /// シナリオ（`stream_error_after`）の期限は `take_stream_error` が見る
    stream_error: Arc<AtomicBool>,
    /// 今のストリームを開いた時刻。シナリオの期限の起点
    opened_at: Option<Instant>,
    /// 今のストリームでシナリオのエラーをもう立てたか。開き直すと下ろす
    scenario_error_raised: AtomicBool,
    /// 「今」を返す時計。本番は `Instant::now`。テストが差し替えて、
    /// シナリオの期限を実時間を待たずに跨ぐ（`with_clock`）
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    underruns: Arc<AtomicU32>,
    /// 入力がリングバッファの満杯で捨てたフレーム数。`AudioCapture` と同じ扱い
    dropped_frames: Arc<AtomicU32>,
}

impl FakeAudioCapture {
    pub fn new(controls: Arc<AudioControls>, tap: AudioTap, options: FakeAudioOptions) -> Self {
        Self {
            controls,
            tap,
            remaining_failures: options.failures_before_success,
            options,
            stream: None,
            active: None,
            resample_telemetry: None,
            stream_error: Arc::new(AtomicBool::new(false)),
            opened_at: None,
            scenario_error_raised: AtomicBool::new(false),
            clock: Arc::new(Instant::now),
            underruns: Arc::new(AtomicU32::new(0)),
            dropped_frames: Arc::new(AtomicU32::new(0)),
        }
    }

    /// シナリオの期限の判定に使う時計を差し替える。テスト専用
    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub fn list_input_devices(&self) -> Vec<String> {
        self.inputs()
            .into_iter()
            .map(|device| device.name)
            .collect()
    }

    pub fn list_output_devices(&self) -> Vec<String> {
        outputs().into_iter().map(|device| device.name).collect()
    }

    /// 既定の出力は 1 番
    pub fn default_output_device_name(&self) -> Option<String> {
        outputs().into_iter().next().map(|device| device.name)
    }

    pub fn capabilities(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<AudioCapabilities, AudioError> {
        let device = self.find(direction, device_name)?;
        Ok(AudioCapabilities::new(
            device.configs(),
            device.sample_rate,
            device.channels,
        ))
    }

    pub fn start_passthrough(
        &mut self,
        request: &PassthroughRequest<'_>,
    ) -> Result<(), AudioError> {
        self.stop_capture();

        let input = self.find(AudioDirection::Input, request.input_device_name)?;
        let output = self.find(AudioDirection::Output, request.output_device_name)?;

        if self.remaining_failures > 0 {
            self.remaining_failures -= 1;
            info!(
                "フェイクの音声デバイスを開くのに失敗させた（シナリオ fail、残り {} 回）",
                self.remaining_failures
            );
            return Err(AudioError::StreamBuildFailed {
                direction: AudioDirection::Input,
                source: "フェイクのシナリオ（fail）で失敗させた".to_string(),
            });
        }

        // 開く設定の選び方は実機と同じ。取得済みの対応設定が渡されれば使い、
        // 無ければこの場で「列挙」する
        let input_ranges =
            resolve_ranges(request.input_capabilities, AudioDirection::Input, || {
                Ok::<_, AudioError>(input.configs())
            });
        let output_ranges =
            resolve_ranges(request.output_capabilities, AudioDirection::Output, || {
                Ok::<_, AudioError>(output.configs())
            });
        let (input_config, output_config) = choose_passthrough_configs(
            &input_ranges,
            &output_ranges,
            input.default_config(),
            output.default_config(),
            request.sample_rate,
            request.channels,
        );
        let input_rate = input_config.sample_rate();
        let input_channels = input_config.channels();
        let output_rate = output_config.sample_rate();
        let output_channels = output_config.channels();

        let buffer_size =
            ring_buffer_samples(input_rate, input_channels as usize, request.buffer_ms);
        let capacity = buffer_size * 2;
        // 出力が最初に待つ水位と、クロックドリフト補正が保つ水位（本物と同じ）
        let target_level = target_water_level(capacity, input_channels as usize);
        let (producer, consumer) = HeapRb::<f32>::new(capacity).split();
        let producer = Arc::new(Mutex::new(producer));
        let consumer = Arc::new(Mutex::new(consumer));

        // 開き直すたびに作り直す（`AudioCapture` と同じ理由）
        let stream_error = Arc::new(AtomicBool::new(false));
        let underruns = Arc::new(AtomicU32::new(0));
        let dropped_frames = Arc::new(AtomicU32::new(0));

        // 出力は目標水位まで溜まってから取り出し始める。本物と同じく、入力と
        // 出力のスレッドは同時に起こしてよい
        let converter =
            PassthroughConverter::new(input_rate, input_channels, output_rate, output_channels)
                .with_prebuffer(target_level);
        // 入出力の形が揃っていても補正する（本物と同じ、Issue #308）
        let resample_telemetry = Some(Arc::new(ResampleTelemetry::new(target_level)));
        let converter = converter.with_telemetry(resample_telemetry.clone());

        // 録画へ入力の形と開き直しを知らせる。入力のスレッドを起こす前に書く（本物と同じ）
        self.tap.begin_stream(input_rate, input_channels);

        let (stop_input, input_rx) = mpsc::channel();
        let (stop_output, output_rx) = mpsc::channel();
        let sine = SineInput {
            producer,
            tap: self.tap.clone(),
            dropped_frames: Arc::clone(&dropped_frames),
            sample_rate: input_rate,
            channels: input_channels,
            frequency_hz: input.frequency_hz,
        };
        let sink = DiscardOutput {
            consumer,
            controls: Arc::clone(&self.controls),
            converter,
            underruns: Arc::clone(&underruns),
            sample_rate: output_rate,
            channels: output_channels,
        };
        let input_handle = spawn_named("fake-audio-in", AudioDirection::Input, move || {
            sine.run(input_rx)
        })?;
        let output_handle = match spawn_named("fake-audio-out", AudioDirection::Output, move || {
            sink.run(output_rx)
        }) {
            Ok(handle) => handle,
            Err(e) => {
                // 入力だけ動いたまま残さない
                drop(stop_input);
                let _ = input_handle.join();
                return Err(e);
            }
        };

        info!(
            "フェイクの音声デバイスを開いた - 入力: {} {}Hz {}ch（{}Hz の正弦波）、出力: {} {}Hz {}ch",
            input.name,
            input_rate,
            input_channels,
            input.frequency_hz,
            output.name,
            output_rate,
            output_channels
        );
        self.stream = Some(FakeAudioStream {
            stop_input,
            stop_output,
            input: input_handle,
            output: output_handle,
        });
        self.stream_error = stream_error;
        self.opened_at = Some((self.clock)());
        self.scenario_error_raised = AtomicBool::new(false);
        self.resample_telemetry = resample_telemetry;
        self.underruns = underruns;
        self.dropped_frames = dropped_frames;
        self.active = Some(ActiveAudio {
            input_device: input.name,
            output_device: output.name,
            input_sample_rate: input_rate,
            input_channels,
            output_sample_rate: output_rate,
            output_channels,
        });
        Ok(())
    }

    pub fn active(&self) -> Option<ActiveAudio> {
        self.active.clone()
    }

    pub fn resample_telemetry(&self) -> Option<&Arc<ResampleTelemetry>> {
        self.resample_telemetry.as_ref()
    }

    pub fn resample_status(&self) -> Option<ResampleStatus> {
        self.resample_telemetry
            .as_ref()
            .map(|telemetry| ResampleStatus {
                ratio: telemetry.correction(),
                water_level: telemetry.water_level(),
                target_level: telemetry.target_level(),
            })
    }

    pub fn underrun_count(&self) -> Option<u32> {
        self.active
            .as_ref()
            .map(|_| self.underruns.load(Ordering::Relaxed))
    }

    pub fn dropped_frame_count(&self) -> Option<u32> {
        self.active
            .as_ref()
            .map(|_| self.dropped_frames.load(Ordering::Relaxed))
    }

    pub fn stop_capture(&mut self) {
        self.active = None;
        self.resample_telemetry = None;
        if let Some(stream) = self.stream.take() {
            drop(stream.stop_input);
            drop(stream.stop_output);
            // 両方を先に join する。`||` で繋ぐと、入力が異常終了していたときに
            // 出力の `JoinHandle` が join されずに捨てられる
            let input_panicked = stream.input.join().is_err();
            let output_panicked = stream.output.join().is_err();
            if input_panicked || output_panicked {
                warn!("フェイクの音声のスレッドが異常終了していた");
            }
            info!("フェイクの音声デバイスを閉じた");
        }
        self.stream_error = Arc::new(AtomicBool::new(false));
        self.opened_at = None;
        self.underruns = Arc::new(AtomicU32::new(0));
        self.dropped_frames = Arc::new(AtomicU32::new(0));
    }

    pub fn take_stream_error(&self) -> bool {
        // シナリオ（audio-error）の期限を過ぎていたら、開いている間に 1 回だけ
        // 旗を立てる。本物の cpal のエラーコールバックが立てるのと同じ旗
        if let (Some(after), Some(opened_at)) = (self.options.stream_error_after, self.opened_at) {
            if (self.clock)().saturating_duration_since(opened_at) >= after
                && !self.scenario_error_raised.swap(true, Ordering::Relaxed)
            {
                info!("フェイクの音声ストリームのエラーを立てた（シナリオ audio-error）");
                self.stream_error.store(true, Ordering::Relaxed);
            }
        }
        self.stream_error.swap(false, Ordering::Relaxed)
    }

    fn inputs(&self) -> Vec<FakeDevice> {
        (1..=self.options.input_count)
            .map(|index| FakeDevice {
                name: format!("{INPUT_NAME_PREFIX} {index}"),
                sample_rate: INPUT_SAMPLE_RATE,
                channels: INPUT_CHANNELS,
                frequency_hz: BASE_FREQUENCY_HZ * f64::from(index),
            })
            .collect()
    }

    /// 名前でデバイスを引く。`None` なら既定（1 番）
    fn find(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<FakeDevice, AudioError> {
        let devices = match direction {
            AudioDirection::Input => self.inputs(),
            AudioDirection::Output => outputs(),
        };
        match device_name {
            None => devices
                .into_iter()
                .next()
                .ok_or(AudioError::NoDefaultDevice(direction)),
            Some(name) => devices
                .into_iter()
                .find(|device| device.name == name)
                .ok_or_else(|| AudioError::DeviceNotFound {
                    direction,
                    name: name.to_string(),
                }),
        }
    }
}

impl Drop for FakeAudioCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

fn outputs() -> Vec<FakeDevice> {
    OUTPUTS
        .iter()
        .enumerate()
        .map(|(slot, &(sample_rate, channels))| FakeDevice {
            name: format!("{OUTPUT_NAME_PREFIX} {}", slot + 1),
            sample_rate,
            channels,
            frequency_hz: 0.0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

    fn capture(input_count: u32, failures_before_success: u32) -> FakeAudioCapture {
        FakeAudioCapture::new(
            Arc::new(AudioControls::default()),
            AudioTap::new(),
            FakeAudioOptions {
                input_count,
                failures_before_success,
                stream_error_after: None,
            },
        )
    }

    fn request<'a>(input: Option<&'a str>, output: Option<&'a str>) -> PassthroughRequest<'a> {
        PassthroughRequest {
            input_device_name: input,
            output_device_name: output,
            sample_rate: None,
            channels: None,
            input_capabilities: None,
            output_capabilities: None,
            buffer_ms: 50,
        }
    }

    #[test]
    fn fake_audio_lists_inputs_by_count_and_two_outputs() {
        let capture = capture(2, 0);
        assert_eq!(
            capture.list_input_devices(),
            vec!["Fake Audio Input 1", "Fake Audio Input 2"]
        );
        assert_eq!(
            capture.list_output_devices(),
            vec!["Fake Audio Output 1", "Fake Audio Output 2"]
        );
        assert_eq!(
            capture.default_output_device_name().as_deref(),
            Some("Fake Audio Output 1")
        );
    }

    #[test]
    fn fake_audio_capabilities_report_the_device_shape() {
        let capture = capture(1, 0);
        let output = capture
            .capabilities(AudioDirection::Output, Some("Fake Audio Output 2"))
            .expect("ある");
        assert_eq!(output.sample_rates(), vec![44_100]);
        assert_eq!(output.channels(), vec![1]);
        assert_eq!(output.default_sample_rate(), 44_100);

        assert_eq!(
            capture.capabilities(AudioDirection::Input, Some("Fake Audio Input 9")),
            Err(AudioError::DeviceNotFound {
                direction: AudioDirection::Input,
                name: "Fake Audio Input 9".to_string(),
            })
        );
    }

    #[test]
    fn fake_audio_same_shape_opens_without_conversion() {
        let mut capture = capture(1, 0);
        capture
            .start_passthrough(&request(None, Some("Fake Audio Output 1")))
            .expect("開ける");

        let active = capture.active().expect("開いている");
        assert_eq!(active.input_device, "Fake Audio Input 1");
        assert_eq!(active.output_device, "Fake Audio Output 1");
        assert_eq!(active.input_summary(), "48000Hz 2ch");
        assert_eq!(active.output_summary(), "48000Hz 2ch");
        // 揃っていてもクロックドリフト補正の対象になる（実機と同じ、Issue #308）
        let status = capture.resample_status().expect("揃っていても補正の対象");
        assert_eq!(status.target_level, 4800);
        assert_eq!(status.ratio, 1.0);
        assert!(capture.underrun_count().is_some());
        assert!(capture.dropped_frame_count().is_some());

        capture.stop_capture();
        assert!(capture.active().is_none());
        assert!(capture.resample_status().is_none());
        assert!(capture.underrun_count().is_none());
        assert!(capture.dropped_frame_count().is_none());
    }

    #[test]
    fn fake_audio_different_shape_runs_the_resample_path() {
        let mut capture = capture(1, 0);
        capture
            .start_passthrough(&request(
                Some("Fake Audio Input 1"),
                Some("Fake Audio Output 2"),
            ))
            .expect("開ける");

        let active = capture.active().expect("開いている");
        assert_eq!(active.output_summary(), "44100Hz 1ch");
        let status = capture.resample_status().expect("変換が要るので補正の対象");
        // 48kHz 2ch の 50ms は 4800 サンプル（`ring_buffer_samples` と同じ）
        assert_eq!(status.target_level, 4800);
        assert_eq!(status.ratio, 1.0);
        capture.stop_capture();
        assert!(capture.resample_status().is_none());
    }

    #[test]
    fn fake_audio_output_consumes_what_the_input_produces() {
        // 入出力のスレッドが本物の経路でリングバッファを回していること。
        // 変換の経路なら出力が水位を書き込むので、それで確かめる
        let mut capture = capture(1, 0);
        capture
            .start_passthrough(&request(None, Some("Fake Audio Output 2")))
            .expect("開ける");
        let telemetry = Arc::clone(capture.resample_telemetry().expect("変換の経路"));
        let target = telemetry.target_level();

        // 開いた直後は目標水位のまま。出力が 1 回でも回れば実際の水位に変わる
        let deadline = Instant::now() + Duration::from_secs(5);
        while telemetry.water_level() == target && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(telemetry.water_level(), target, "出力が水位を書いていない");
        // 溢れていないこと（容量は目標の 2 倍）
        assert!(telemetry.water_level() <= target * 2);
    }

    #[test]
    fn fake_audio_output_starts_after_the_ring_reaches_the_target_level() {
        // Issue #308。出力は、リングバッファが目標水位（バッファ長ぶん）まで溜まってから
        // 取り出し始める。修正前は入力の開始から 50ms 待って出力を始めていたので、
        // 200ms にしても水位は 50ms 前後（目標の 25%）で落ち着いていた
        let mut capture = capture(1, 0);
        let mut request = request(None, Some("Fake Audio Output 2"));
        request.buffer_ms = 200;
        capture.start_passthrough(&request).expect("開ける");
        let telemetry = Arc::clone(capture.resample_telemetry().expect("変換の経路"));
        let target = telemetry.target_level();

        // 目標まで溜まるのに 200ms かかる。落ち着くまでもう少し待つ
        std::thread::sleep(Duration::from_millis(800));
        let level = telemetry.water_level();
        capture.stop_capture();

        assert!(
            level >= target / 2,
            "水位が目標の半分に届いていない（水位 {level} / 目標 {target}）"
        );
        assert!(level <= target * 2);
    }

    #[test]
    fn fake_audio_input_reaches_the_recording_tap() {
        // 録画の差し込み口にも、本物と同じ `process_input` の経路で正弦波が積まれること
        let tap = AudioTap::new();
        let mut capture = FakeAudioCapture::new(
            Arc::new(AudioControls::default()),
            tap.clone(),
            FakeAudioOptions {
                input_count: 1,
                failures_before_success: 0,
                stream_error_after: None,
            },
        );
        let mut attachment = tap.attach(tap.one_second_capacity());
        capture
            .start_passthrough(&request(None, None))
            .expect("開ける");
        assert_eq!(tap.format(), Some((INPUT_SAMPLE_RATE, INPUT_CHANNELS)));
        assert_eq!(tap.snapshot().generation, 1);

        let deadline = Instant::now() + Duration::from_secs(5);
        while tap.snapshot().samples_total == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        capture.stop_capture();
        tap.detach();

        let snapshot = tap.snapshot();
        assert!(
            snapshot.samples_total > 0,
            "入力が録画のリングへ積んでいない"
        );
        let mut read = vec![0.0f32; snapshot.samples_total as usize];
        let count = attachment.consumer.pop_slice(&mut read);
        assert_eq!(count as u64, snapshot.samples_total);
        // 正弦波が入力の形のまま入っている
        assert!(read[..count].iter().any(|&sample| sample != 0.0));
    }

    #[test]
    fn fake_audio_fail_scenario_fails_then_succeeds() {
        let mut capture = capture(1, 1);
        assert!(matches!(
            capture.start_passthrough(&request(None, None)),
            Err(AudioError::StreamBuildFailed { .. })
        ));
        assert!(capture.active().is_none());
        capture
            .start_passthrough(&request(None, None))
            .expect("2 回目は開ける");
        assert!(capture.active().is_some());
    }

    #[test]
    fn fake_audio_unknown_output_is_not_found() {
        let mut capture = capture(1, 0);
        assert_eq!(
            capture.start_passthrough(&request(None, Some("スピーカー"))),
            Err(AudioError::DeviceNotFound {
                direction: AudioDirection::Output,
                name: "スピーカー".to_string(),
            })
        );
    }
}
