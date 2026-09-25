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
//! `process_output`）を通る。cpal のコールバックスレッドの代わりに、
//! 10ms ごとに起きるスレッドを入力と出力に 1 本ずつ立てる。
//!
//! | デバイス | 形 |
//! |---|---|
//! | Fake Audio Input 1, 2, … | 48kHz 2ch。1 番が 440Hz、2 番が 880Hz、… の正弦波 |
//! | Fake Audio Output 1 | 48kHz 2ch（入力と揃うので変換しない） |
//! | Fake Audio Output 2 | 44.1kHz 1ch（入力と揃わないので変換し、ドリフト補正も動く） |

use cpal::{
    SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use log::{debug, info, warn};
use ringbuf::HeapRb;
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capabilities::AudioCapabilities;
use super::capture::{ring_buffer_samples, PassthroughRequest};
use super::controls::AudioControls;
use super::convert::PassthroughConverter;
use super::resample::{ResampleStatus, ResampleTelemetry};
use super::stream::{process_input, process_output, AudioConsumer, AudioProducer};
use super::stream_config::{choose_passthrough_configs, resolve_ranges};
use super::{ActiveAudio, AudioDirection, AudioError};

const INPUT_NAME_PREFIX: &str = "Fake Audio Input";
const OUTPUT_NAME_PREFIX: &str = "Fake Audio Output";

/// 入力デバイスの形。どの入力も同じ
const INPUT_SAMPLE_RATE: u32 = 48_000;
const INPUT_CHANNELS: u16 = 2;

/// 出力デバイスの形。`(サンプリングレート, チャンネル数)`。
/// 1 番は入力と揃い、2 番は揃わない（変換とドリフト補正の経路を通すため）
const OUTPUTS: [(u32, u16); 2] = [(48_000, 2), (44_100, 1)];

/// 1 番の入力の正弦波の周波数。n 番はこの n 倍
const BASE_FREQUENCY_HZ: f64 = 440.0;
/// 正弦波の振幅。フルスケールだと音量 200% で頭打ちになるので控えめにする
const SINE_AMPLITUDE: f64 = 0.25;

/// 入出力のスレッドが起きる間隔。WASAPI の既定の周期と同じくらいにしてある
const TICK: Duration = Duration::from_millis(10);
/// 1 回に処理する最大の長さ。スレッドが長く止まったあとに一度に
/// 取り返そうとしないための上限で、超えた分は捨てる
const MAX_CHUNK: Duration = Duration::from_millis(200);
/// 出力を入力より遅れて始める時間。実機（`AudioCapture::start_passthrough`）が
/// 入力を開始してから出力を開始するまでの待ちと同じ
const OUTPUT_START_DELAY: Duration = Duration::from_millis(50);

/// フェイクの音声デバイスの振る舞い。環境変数から組み立てる
/// （`app::backend::fake`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeAudioOptions {
    /// 名乗る入力デバイスの台数。1 以上
    pub input_count: u32,
    /// 最初にこの回数だけ開くのに失敗する（接続失敗と再試行の再現）
    pub failures_before_success: u32,
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
            SampleRate(self.sample_rate),
            SampleRate(self.sample_rate),
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )]
    }

    fn default_config(&self) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            self.channels,
            SampleRate(self.sample_rate),
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
    options: FakeAudioOptions,
    /// シナリオ（`failures_before_success`）で、あと何回失敗させるか
    remaining_failures: u32,
    stream: Option<FakeAudioStream>,
    active: Option<ActiveAudio>,
    resample_telemetry: Option<Arc<ResampleTelemetry>>,
    /// フェイクのストリームはエラーを起こさないので立つことはないが、
    /// 窓口と差し替えの作法を `AudioCapture` と揃えるために持つ
    stream_error: Arc<AtomicBool>,
    underruns: Arc<AtomicU32>,
}

impl FakeAudioCapture {
    pub fn new(controls: Arc<AudioControls>, options: FakeAudioOptions) -> Self {
        Self {
            controls,
            remaining_failures: options.failures_before_success,
            options,
            stream: None,
            active: None,
            resample_telemetry: None,
            stream_error: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
        }
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

    /// 既定の入力は 1 番
    pub fn default_input_device_name(&self) -> Option<String> {
        self.inputs().into_iter().next().map(|device| device.name)
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
        let input_rate = input_config.sample_rate().0;
        let input_channels = input_config.channels();
        let output_rate = output_config.sample_rate().0;
        let output_channels = output_config.channels();

        let buffer_size =
            ring_buffer_samples(input_rate, input_channels as usize, request.buffer_ms);
        let (producer, consumer) = HeapRb::<f32>::new(buffer_size * 2).split();
        let producer = Arc::new(Mutex::new(producer));
        let consumer = Arc::new(Mutex::new(consumer));

        // 開き直すたびに作り直す（`AudioCapture` と同じ理由）
        let stream_error = Arc::new(AtomicBool::new(false));
        let underruns = Arc::new(AtomicU32::new(0));

        let converter =
            PassthroughConverter::new(input_rate, input_channels, output_rate, output_channels);
        let resample_telemetry = if converter.is_identity() {
            None
        } else {
            Some(Arc::new(ResampleTelemetry::new(buffer_size)))
        };
        let converter = converter.with_telemetry(resample_telemetry.clone());

        let (stop_input, input_rx) = mpsc::channel();
        let (stop_output, output_rx) = mpsc::channel();
        let sine = SineInput {
            producer,
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
        self.resample_telemetry = resample_telemetry;
        self.underruns = underruns;
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
        self.underruns = Arc::new(AtomicU32::new(0));
    }

    pub fn take_stream_error(&self) -> bool {
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

/// 入出力のスレッドを起こす。起こせなければ、その向きのストリームを
/// 開始できなかったことにする
fn spawn_named(
    name: &str,
    direction: AudioDirection,
    body: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle<()>, AudioError> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .map_err(|e| AudioError::StreamPlayFailed {
            direction,
            source: e.to_string(),
        })
}

/// 経過時間に合わせて、決まったレートでフレームを処理し続ける。
///
/// `TICK` ごとに起き、開始からの経過時間ぶんに足りない数のフレームを
/// `on_frames` へ渡す。起きる間隔が揺れても、平均のレートはずれない。
/// `stop` の送り手が落とされたら抜ける。
fn run_paced(
    stop: &Receiver<()>,
    sample_rate: u32,
    start_delay: Duration,
    mut on_frames: impl FnMut(usize),
) {
    if !start_delay.is_zero()
        && !matches!(
            stop.recv_timeout(start_delay),
            Err(RecvTimeoutError::Timeout)
        )
    {
        return;
    }
    let max_frames = (f64::from(sample_rate) * MAX_CHUNK.as_secs_f64()) as u64;
    let started = Instant::now();
    let mut done: u64 = 0;
    loop {
        if !matches!(stop.recv_timeout(TICK), Err(RecvTimeoutError::Timeout)) {
            return;
        }
        let due = (started.elapsed().as_secs_f64() * f64::from(sample_rate)) as u64;
        let frames = due.saturating_sub(done).min(max_frames);
        // 上限で切った分は取り返さない。溜めると次の周期も上限に張り付く
        done = due;
        if frames > 0 {
            on_frames(frames as usize);
        }
    }
}

/// サンプル列を正弦波で埋める。全チャンネルに同じ値を書く。
///
/// `phase` は 0〜1 の位相で、呼び出しをまたいで引き継ぐ（波形を途切れさせない）。
fn fill_sine(
    buffer: &mut [f32],
    channels: usize,
    sample_rate: u32,
    frequency_hz: f64,
    phase: &mut f64,
) {
    let step = frequency_hz / f64::from(sample_rate.max(1));
    for frame in buffer.chunks_mut(channels.max(1)) {
        let value = (SINE_AMPLITUDE * (TAU * *phase).sin()) as f32;
        frame.fill(value);
        *phase = (*phase + step).fract();
    }
}

/// 正弦波を吐く入力。cpal の入力コールバックの代わり。
struct SineInput {
    producer: Arc<Mutex<AudioProducer>>,
    sample_rate: u32,
    channels: u16,
    frequency_hz: f64,
}

impl SineInput {
    fn run(self, stop: Receiver<()>) {
        let channels = self.channels as usize;
        // 1 回に処理する最大の長さぶんを先に確保し、毎回はスライスで使う
        let capacity =
            (f64::from(self.sample_rate) * MAX_CHUNK.as_secs_f64()) as usize * channels.max(1);
        let mut buffer = vec![0.0f32; capacity];
        let mut phase = 0.0;
        run_paced(&stop, self.sample_rate, Duration::ZERO, |frames| {
            let len = (frames * channels).min(buffer.len());
            let chunk = &mut buffer[..len];
            fill_sine(
                chunk,
                channels,
                self.sample_rate,
                self.frequency_hz,
                &mut phase,
            );
            process_input(chunk, &self.producer, |sample| sample);
        });
        debug!("フェイクの音声入力のスレッドを終えた");
    }
}

/// 書き込みを捨てる出力。cpal の出力コールバックの代わり。
struct DiscardOutput {
    consumer: Arc<Mutex<AudioConsumer>>,
    controls: Arc<AudioControls>,
    converter: PassthroughConverter,
    underruns: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
}

impl DiscardOutput {
    fn run(mut self, stop: Receiver<()>) {
        let channels = self.channels as usize;
        let capacity =
            (f64::from(self.sample_rate) * MAX_CHUNK.as_secs_f64()) as usize * channels.max(1);
        let mut buffer = vec![0.0f32; capacity];
        run_paced(&stop, self.sample_rate, OUTPUT_START_DELAY, |frames| {
            let len = (frames * channels).min(buffer.len());
            process_output(
                &mut buffer[..len],
                &self.consumer,
                &self.controls,
                &mut self.converter,
                &self.underruns,
                |sample| sample,
            );
        });
        debug!("フェイクの音声出力のスレッドを終えた");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(input_count: u32, failures_before_success: u32) -> FakeAudioCapture {
        FakeAudioCapture::new(
            Arc::new(AudioControls::default()),
            FakeAudioOptions {
                input_count,
                failures_before_success,
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
    fn fill_sine_starts_at_zero_and_peaks_at_a_quarter_period() {
        // 48kHz で 12kHz なら 1 周期 4 サンプル。0 → 振幅 → 0 → -振幅
        let mut buffer = [9.0f32; 8];
        let mut phase = 0.0;
        fill_sine(&mut buffer, 2, 48_000, 12_000.0, &mut phase);

        let expected = [0.0, 0.0, 0.25, 0.25, 0.0, 0.0, -0.25, -0.25];
        for (actual, expected) in buffer.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6, "{buffer:?}");
        }
        // 4 サンプル進めば 1 周して位相は 0 へ戻る
        assert!(phase.abs() < 1e-9 || (1.0 - phase).abs() < 1e-9, "{phase}");
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
            capture.default_input_device_name().as_deref(),
            Some("Fake Audio Input 1")
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
        // 揃っているので補正の対象にならない（実機と同じ）
        assert!(capture.resample_telemetry().is_none());
        assert!(capture.resample_status().is_none());
        assert!(capture.underrun_count().is_some());

        capture.stop_capture();
        assert!(capture.active().is_none());
        assert!(capture.underrun_count().is_none());
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
