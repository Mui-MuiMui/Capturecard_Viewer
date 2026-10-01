//! フェイクの音声デバイス（`fake.rs` の `FakeAudioCapture`）が立てる
//! 入出力のスレッドの本体。
//!
//! cpal のコールバックスレッドの代わりに、`TICK` ごとに起きて経過時間ぶんの
//! フレームを処理する。入力は正弦波を吐いて `stream::process_input` へ、出力は
//! `stream_output::process_output` で取り出して捨てる。どちらも本物と同じ関数を通る。

use log::{debug, warn};
use std::f64::consts::TAU;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::controls::AudioControls;
use super::convert::PassthroughConverter;
use super::pin_feed::{AudioPinFeed, PinFormat};
use super::sample::f32_to_i16;
use super::stream::{process_input, AudioConsumer, AudioProducer};
use super::stream_output::process_output;
use super::tap::AudioTap;
use super::{AudioDirection, AudioError};

/// 正弦波の振幅。フルスケールだと音量 200% で頭打ちになるので控えめにする
const SINE_AMPLITUDE: f64 = 0.25;

/// 入出力のスレッドが起きる間隔。WASAPI の既定の周期と同じくらいにしてある
const TICK: Duration = Duration::from_millis(10);
/// 1 回に処理する最大の長さ。スレッドが長く止まったあとに一度に
/// 取り返そうとしないための上限で、超えた分は捨てる
const MAX_CHUNK: Duration = Duration::from_millis(200);

/// 入出力のスレッドを起こす。起こせなければ、その向きのストリームを
/// 開始できなかったことにする
pub(super) fn spawn_named(
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
fn run_paced(stop: &Receiver<()>, sample_rate: u32, mut on_frames: impl FnMut(usize)) {
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
pub(super) struct SineInput {
    pub(super) producer: Arc<Mutex<AudioProducer>>,
    pub(super) tap: AudioTap,
    pub(super) dropped_frames: Arc<AtomicU32>,
    pub(super) sample_rate: u32,
    pub(super) channels: u16,
    pub(super) frequency_hz: f64,
}

impl SineInput {
    pub(super) fn run(self, stop: Receiver<()>) {
        let channels = self.channels as usize;
        // 1 回に処理する最大の長さぶんを先に確保し、毎回はスライスで使う
        let capacity =
            (f64::from(self.sample_rate) * MAX_CHUNK.as_secs_f64()) as usize * channels.max(1);
        let mut buffer = vec![0.0f32; capacity];
        let mut phase = 0.0;
        run_paced(&stop, self.sample_rate, |frames| {
            let len = (frames * channels).min(buffer.len());
            let chunk = &mut buffer[..len];
            fill_sine(
                chunk,
                channels,
                self.sample_rate,
                self.frequency_hz,
                &mut phase,
            );
            process_input(
                chunk,
                channels,
                &self.producer,
                &self.tap,
                &self.dropped_frames,
                |sample| sample,
            );
        });
        debug!("フェイクの音声入力のスレッドを終えた");
    }
}

/// フェイクの映像デバイスの音声ピン（#394）。正弦波を DirectShow の音声ピンと同じ
/// 形（16bit 整数の PCM のバイト列）にして `AudioPinFeed` へ流すスレッド。
///
/// 実機ではキャプチャーフィルターのストリーミングスレッドが `Receive` から
/// `AudioPinFeed::push` を呼ぶ。その代わりに `TICK`（10ms）ごとに起きて同じ関数を呼ぶ。
/// 持ち主は `video::FakeVideoCapture`（映像を開いている間だけ動く）。
pub struct FakePinSource {
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

impl FakePinSource {
    /// 流し始める。`graph` は映像が配ったグラフの番号で、`format` の
    /// サンプルの型は 16bit 整数だけを受け付ける（フェイクの形は GC551 と同じ）
    pub fn spawn(
        feed: AudioPinFeed,
        graph: u64,
        format: PinFormat,
        frequency_hz: f64,
    ) -> Result<Self, AudioError> {
        let (stop, stop_rx) = mpsc::channel();
        let handle = spawn_named("fake-audio-pin", AudioDirection::Input, move || {
            run_pin_source(&stop_rx, &feed, graph, format, frequency_hz)
        })?;
        Ok(Self { stop, handle })
    }

    /// 止めて、スレッドが終わるまで待つ
    pub fn stop(self) {
        drop(self.stop);
        if self.handle.join().is_err() {
            warn!("フェイクの音声ピンのスレッドが異常終了していた");
        }
    }
}

fn run_pin_source(
    stop: &Receiver<()>,
    feed: &AudioPinFeed,
    graph: u64,
    format: PinFormat,
    frequency_hz: f64,
) {
    let channels = usize::from(format.channels);
    let capacity =
        (f64::from(format.sample_rate) * MAX_CHUNK.as_secs_f64()) as usize * channels.max(1);
    let mut samples = vec![0.0f32; capacity];
    let mut bytes = vec![0u8; capacity * 2];
    let mut phase = 0.0;
    run_paced(stop, format.sample_rate, |frames| {
        let len = (frames * channels).min(samples.len());
        let chunk = &mut samples[..len];
        fill_sine(
            chunk,
            channels,
            format.sample_rate,
            frequency_hz,
            &mut phase,
        );
        for (sample, out) in chunk.iter().zip(bytes.as_chunks_mut::<2>().0) {
            *out = f32_to_i16(*sample).to_le_bytes();
        }
        feed.push(graph, format, &bytes[..len * 2], false);
    });
    debug!("フェイクの音声ピンのスレッドを終えた");
}

/// 書き込みを捨てる出力。cpal の出力コールバックの代わり。
pub(super) struct DiscardOutput {
    pub(super) consumer: Arc<Mutex<AudioConsumer>>,
    pub(super) controls: Arc<AudioControls>,
    pub(super) converter: PassthroughConverter,
    pub(super) underruns: Arc<AtomicU32>,
    pub(super) sample_rate: u32,
    pub(super) channels: u16,
}

impl DiscardOutput {
    pub(super) fn run(mut self, stop: Receiver<()>) {
        let channels = self.channels as usize;
        let capacity =
            (f64::from(self.sample_rate) * MAX_CHUNK.as_secs_f64()) as usize * channels.max(1);
        let mut buffer = vec![0.0f32; capacity];
        run_paced(&stop, self.sample_rate, |frames| {
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
}
