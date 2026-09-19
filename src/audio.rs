use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SupportedStreamConfigRange};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::HeapRb;

/// リングバッファの内部表現は f32 に統一する。デバイス側のサンプル型は
/// 入力で f32 へ正規化し、出力で書き戻す。
type AudioProducer = ringbuf::Producer<f32, Arc<HeapRb<f32>>>;
type AudioConsumer = ringbuf::Consumer<f32, Arc<HeapRb<f32>>>;

pub struct AudioCapture {
    host: cpal::Host,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    is_active: bool,
    volume: Arc<Mutex<f32>>,
    // 簡素化されたリングバッファ（シングルバッファ構成）
    buffer_capacity: usize,

    audio_passthrough_enabled: Arc<AtomicBool>,
    // 音声データ用のコンシューマハンドル
    raw_audio_consumer: Option<Arc<Mutex<AudioConsumer>>>,
    processed_audio_consumer: Option<Arc<Mutex<AudioConsumer>>>,
}

impl AudioCapture {
    pub fn new() -> Self {
        println!("Debug: Creating AudioCapture with WASAPI host");
        let host = cpal::default_host();
        println!("Debug: Host created: {:?}", host.id());

        Self {
            host,
            input_stream: None,
            output_stream: None,
            is_active: false,
            volume: Arc::new(Mutex::new(1.0)),
            buffer_capacity: 0,
            // 既定では音声パススルーを有効にする（音が出る状態で起動する）
            audio_passthrough_enabled: Arc::new(AtomicBool::new(true)),
            raw_audio_consumer: None,
            processed_audio_consumer: None,
        }
    }

    pub fn list_input_devices(&self) -> Vec<String> {
        match self.host.input_devices() {
            Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn list_output_devices(&self) -> Vec<String> {
        match self.host.output_devices() {
            Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn start_passthrough_with_settings(
        &mut self,
        input_device_name: Option<&str>,
        output_device_name: Option<&str>,
        _desired_sample_rate: Option<u32>,
        _desired_channels: Option<u16>,
    ) -> Result<(), String> {
        self.stop_capture();
        println!("Debug: Starting simplified audio passthrough");

        // デバイス取得の簡素化
        let input_device = if let Some(name) = input_device_name {
            println!("Debug: Looking for input device: {}", name);
            self.find_device_by_name(name, true)?
        } else {
            println!("Debug: Using default input device");
            self.host
                .default_input_device()
                .ok_or_else(|| "No default input device".to_string())?
        };

        let output_device = if let Some(name) = output_device_name {
            println!("Debug: Looking for output device: {}", name);
            self.find_device_by_name(name, false)?
        } else {
            println!("Debug: Using default output device");
            self.host
                .default_output_device()
                .ok_or_else(|| "No default output device".to_string())?
        };

        // デバイス名をログ出力
        let input_device_name = input_device
            .name()
            .unwrap_or_else(|_| "Unknown Input".to_string());
        let output_device_name = output_device
            .name()
            .unwrap_or_else(|_| "Unknown Output".to_string());
        println!(
            "Debug: Selected devices - Input: '{}', Output: '{}'",
            input_device_name, output_device_name
        );

        // 設定の簡素化
        let input_config = input_device
            .default_input_config()
            .map_err(|e| format!("Failed to get input config: {}", e))?;

        let output_config = output_device
            .default_output_config()
            .map_err(|e| format!("Failed to get output config: {}", e))?;

        println!(
            "Debug: Audio config - Input: {}Hz {}ch ({:?}), Output: {}Hz {}ch ({:?})",
            input_config.sample_rate().0,
            input_config.channels(),
            input_config.sample_format(),
            output_config.sample_rate().0,
            output_config.channels(),
            output_config.sample_format()
        );

        // メモリリーク修正: リングバッファサイズを制限
        let sample_rate = input_config.sample_rate().0;
        let channels = input_config.channels() as usize;
        let buffer_size = (sample_rate as usize * channels * 50) / 1000; // 50msバッファに削減

        let ring = HeapRb::<f32>::new(buffer_size * 2); // サイズを削減
        let (producer, consumer) = ring.split();

        let producer = Arc::new(Mutex::new(producer));
        let consumer = Arc::new(Mutex::new(consumer));

        println!(
            "Debug: Created ring buffer with {} samples",
            buffer_size * 2
        );

        // 入力ストリーム。デバイスのサンプル型ごとに正規化の仕方が違うので明示的に分ける
        let input_stream_config = input_config.config();
        let input_stream = match input_config.sample_format() {
            SampleFormat::F32 => build_input_stream_with::<f32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                |sample| sample,
            ),
            SampleFormat::I16 => build_input_stream_with::<i16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                i16_to_f32,
            ),
            SampleFormat::U16 => build_input_stream_with::<u16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                u16_to_f32,
            ),
            SampleFormat::I32 => build_input_stream_with::<i32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                i32_to_f32,
            ),
            other => return Err(unsupported_sample_format_error("入力", other)),
        }
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

        // 出力ストリーム
        let vol_arc = self.volume.clone();
        let passthrough_arc = self.audio_passthrough_enabled.clone();
        let output_stream_config = output_config.config();
        let output_stream = match output_config.sample_format() {
            SampleFormat::F32 => build_output_stream_with::<f32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                |sample| sample,
            ),
            SampleFormat::I16 => build_output_stream_with::<i16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                f32_to_i16,
            ),
            SampleFormat::U16 => build_output_stream_with::<u16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                f32_to_u16,
            ),
            SampleFormat::I32 => build_output_stream_with::<i32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                f32_to_i32,
            ),
            other => return Err(unsupported_sample_format_error("出力", other)),
        }
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

        // ストリーム開始
        println!("Debug: Starting audio streams...");
        input_stream
            .play()
            .map_err(|e| format!("Failed to start input stream: {}", e))?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        output_stream
            .play()
            .map_err(|e| format!("Failed to start output stream: {}", e))?;

        self.input_stream = Some(input_stream);
        self.output_stream = Some(output_stream);
        self.is_active = true;

        // 簡素化のため、raw/processedバッファは使用しない
        self.raw_audio_consumer = Some(consumer.clone());
        self.processed_audio_consumer = Some(consumer);

        println!("Debug: Audio passthrough started successfully");
        Ok(())
    }

    #[allow(dead_code)]
    fn select_best_config(
        configs: &mut [SupportedStreamConfigRange],
        desired_sample_rate: Option<u32>,
        _desired_channels: Option<u16>,
    ) -> Option<cpal::SupportedStreamConfig> {
        if configs.is_empty() {
            return None;
        }

        // デフォルト設定を使用 (簡素化)
        let config = *configs.first()?;
        let sample_rate = desired_sample_rate.unwrap_or(48000);

        Some(config.with_sample_rate(cpal::SampleRate(sample_rate)))
    }

    pub fn stop_capture(&mut self) {
        if let Some(s) = self.input_stream.take() {
            let _ = s.pause();
        }
        if let Some(s) = self.output_stream.take() {
            let _ = s.pause();
        }
        self.is_active = false;
        self.buffer_capacity = 0;
    }

    pub fn set_volume(&mut self, volume_percent: f32) {
        let v = (volume_percent / 100.0).clamp(0.0, 2.0);
        if let Ok(mut vol) = self.volume.lock() {
            *vol = v;
        }
    }

    pub fn set_audio_passthrough_enabled(&mut self, enabled: bool) {
        println!("Setting audio passthrough enabled: {}", enabled);
        // 出力コールバック（リアルタイムスレッド）から読むため、ロックを取らない
        self.audio_passthrough_enabled
            .store(enabled, Ordering::Relaxed);
    }

    fn find_device_by_name(&self, name: &str, input: bool) -> Result<Device, String> {
        let iter = if input {
            self.host.input_devices()
        } else {
            self.host.output_devices()
        }
        .map_err(|e| format!("enumerate devices: {e}"))?;
        for d in iter {
            if let Ok(n) = d.name() {
                if n == name {
                    return Ok(d);
                }
            }
        }
        Err(format!("Device '{name}' not found"))
    }
}

/// 未対応のサンプルフォーマットに当たったときのエラー文言を組み立てる。
///
/// ストリーム構築エラーをそのまま上げると「なぜ開けなかったのか」が分からないので、
/// 何が来て何に対応しているのかを明示する。`direction` は「入力」か「出力」。
fn unsupported_sample_format_error(direction: &str, format: SampleFormat) -> String {
    format!(
        "{direction}デバイスのサンプルフォーマット {format} に対応していません（対応: f32 / i16 / u16 / i32）"
    )
}

/// 入力ストリームを組み立てる。
///
/// `to_f32` でデバイスのサンプル型をリングバッファの表現（f32）へ正規化する。
fn build_input_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    producer: Arc<Mutex<AudioProducer>>,
    to_f32: impl Fn(T) -> f32 + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if let Ok(mut prod) = producer.try_lock() {
                for &sample in data {
                    let _ = prod.push(to_f32(sample));
                }
            }
        },
        |e| eprintln!("Input stream error: {}", e),
        None,
    )
}

/// 出力ストリームを組み立てる。
///
/// `to_sample` はリングバッファの f32 をデバイスのサンプル型へ戻す。
fn build_output_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    consumer: Arc<Mutex<AudioConsumer>>,
    volume: Arc<Mutex<f32>>,
    passthrough_enabled: Arc<AtomicBool>,
    to_sample: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let volume = volume.lock().map(|v| *v).unwrap_or(1.0);
            let passthrough = passthrough_enabled.load(Ordering::Relaxed);
            if let Ok(mut cons) = consumer.try_lock() {
                render_output_samples(data, volume, passthrough, || cons.pop(), &to_sample);
            } else {
                // 無音を表す値は型ごとに違う（u16 は 0 ではなく 32768）ので変換関数に通す
                data.fill(to_sample(0.0));
            }
        },
        |e| eprintln!("Output stream error: {}", e),
        None,
    )
}

/// 整数サンプルの振幅の基準。f32 の -1.0 が型の最小値、+1.0 が最大値 + 1 に対応する。
/// 2 のべき乗なので f32 の除算・乗算で誤差が出ない。
const I16_SCALE: f32 = 32_768.0;
const I32_SCALE: f32 = 2_147_483_648.0;
/// u16 の原点。無音は 0 ではなく 32768。
const U16_ORIGIN: f32 = 32_768.0;

/// i16 のサンプルを f32（-1.0..1.0）へ正規化する。
fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / I16_SCALE
}

/// f32 のサンプルを i16 へ変換する。
///
/// 音量 200% では 1.0 を超える値が来る。Rust の float → int キャストは飽和するので、
/// 折り返して最大音量が最小音量に化けることはない。
fn f32_to_i16(sample: f32) -> i16 {
    (sample * I16_SCALE) as i16
}

/// u16 のサンプルを f32（-1.0..1.0）へ正規化する。
///
/// u16 は 32768 が原点なので、そのまま符号付きとして読むと最大振幅の直流になる。
fn u16_to_f32(sample: u16) -> f32 {
    (sample as f32 - U16_ORIGIN) / U16_ORIGIN
}

/// f32 のサンプルを u16 へ変換する。
fn f32_to_u16(sample: f32) -> u16 {
    (sample * U16_ORIGIN + U16_ORIGIN) as u16
}

/// i32 のサンプルを f32（-1.0..1.0）へ正規化する。
fn i32_to_f32(sample: i32) -> f32 {
    sample as f32 / I32_SCALE
}

/// f32 のサンプルを i32 へ変換する。
fn f32_to_i32(sample: f32) -> i32 {
    (sample * I32_SCALE) as i32
}

/// 出力コールバック 1 回分のサンプルを書き込む。
///
/// `passthrough_enabled` が false のときは無音を書き込む。ストリームは止めない。
///
/// `next_sample` はリングバッファから 1 サンプル取り出す。取り出せなければ `None`。
/// `to_sample` は音量を掛けた f32 を出力ストリームのサンプル型へ変換する。
fn render_output_samples<T>(
    data: &mut [T],
    volume: f32,
    passthrough_enabled: bool,
    mut next_sample: impl FnMut() -> Option<f32>,
    to_sample: impl Fn(f32) -> T,
) {
    // パススルーが無効でもリングバッファは同じ数だけ消費する。
    // 消費を止めるとバッファが溢れ、再度有効にしたときに古い音から再生されてしまう。
    for slot in data.iter_mut() {
        let sample = next_sample().unwrap_or(0.0);
        let value = if passthrough_enabled {
            sample * volume
        } else {
            0.0
        };
        *slot = to_sample(value);
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のサンプル供給源。取り出した回数も数える。
    struct SampleSource {
        samples: std::collections::VecDeque<f32>,
        pop_count: usize,
    }

    impl SampleSource {
        fn new(samples: &[f32]) -> Self {
            Self {
                samples: samples.iter().copied().collect(),
                pop_count: 0,
            }
        }

        fn pop(&mut self) -> Option<f32> {
            self.pop_count += 1;
            self.samples.pop_front()
        }
    }

    #[test]
    fn render_output_samples_passthrough_enabled_applies_volume() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 0.5, true, || source.pop(), |value| value);

        assert_eq!(data, [0.5, 0.25, -0.125]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_writes_silence() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_still_consumes_source() {
        // 消費を止めるとリングバッファが溢れ、再有効化した瞬間に古い音が出るため、
        // 無効時もバッファからは同じ数だけ取り出す
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25, 0.75]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 3);
        assert_eq!(source.samples.len(), 1);
    }

    #[test]
    fn render_output_samples_source_underrun_fills_remainder_with_silence() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_empty_source_writes_silence() {
        let mut source = SampleSource::new(&[]);
        let mut data = [9.0f32; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_converts_to_i16() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i16);

        assert_eq!(data, [32767, -32768]);
    }

    #[test]
    fn render_output_samples_i16_passthrough_disabled_writes_zero() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_i16);

        assert_eq!(data, [0, 0]);
    }

    #[test]
    fn render_output_samples_converts_to_u16() {
        // u16 の無音は 0 ではなく 32768
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9u16; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_u16);

        assert_eq!(data, [65535, 0, 32768]);
    }

    #[test]
    fn render_output_samples_u16_passthrough_disabled_writes_midpoint() {
        // 無効時に 0 を書くと u16 では最大振幅の直流になるため、原点を書く
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9u16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_u16);

        assert_eq!(data, [32768, 32768]);
    }

    #[test]
    fn render_output_samples_converts_to_i32() {
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9i32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i32);

        assert_eq!(data, [2147483647, -2147483648, 0]);
    }

    #[test]
    fn render_output_samples_empty_output_buffer_does_not_consume_source() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data: [f32; 0] = [];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 0);
    }

    #[test]
    fn unsupported_sample_format_error_names_the_format() {
        // 「何が未対応だったか」が分からないと原因にたどり着けない
        let message = unsupported_sample_format_error("入力", SampleFormat::U32);

        assert!(message.contains("入力"), "{message}");
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn i16_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(-32768), -1.0);
        assert_eq!(i16_to_f32(32767), 0.999_969_5);
        assert_eq!(i16_to_f32(16384), 0.5);
    }

    #[test]
    fn f32_to_i16_boundaries_saturate() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(-1.0), -32768);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(0.5), 16384);
    }

    #[test]
    fn f32_to_i16_out_of_range_clamps_instead_of_wrapping() {
        // 音量 200% で 1.0 のサンプルが 2.0 になることがある。
        // 折り返すと最大音量が最小音量に化けるため、飽和させる
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32768);
    }

    #[test]
    fn u16_to_f32_midpoint_is_silence() {
        // u16 は 32768 が原点。ここを 0.0 にできないと無音が直流になる
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(u16_to_f32(0), -1.0);
        assert_eq!(u16_to_f32(65535), 0.999_969_5);
        assert_eq!(u16_to_f32(49152), 0.5);
    }

    #[test]
    fn f32_to_u16_boundaries_map_to_full_range() {
        assert_eq!(f32_to_u16(0.0), 32768);
        assert_eq!(f32_to_u16(-1.0), 0);
        assert_eq!(f32_to_u16(1.0), 65535);
        assert_eq!(f32_to_u16(0.5), 49152);
    }

    #[test]
    fn f32_to_u16_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_u16(2.0), 65535);
        assert_eq!(f32_to_u16(-2.0), 0);
    }

    #[test]
    fn i32_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i32_to_f32(0), 0.0);
        assert_eq!(i32_to_f32(-2147483648), -1.0);
        assert_eq!(i32_to_f32(1073741824), 0.5);
    }

    #[test]
    fn f32_to_i32_boundaries_saturate() {
        assert_eq!(f32_to_i32(0.0), 0);
        assert_eq!(f32_to_i32(-1.0), -2147483648);
        assert_eq!(f32_to_i32(1.0), 2147483647);
        assert_eq!(f32_to_i32(0.5), 1073741824);
    }

    #[test]
    fn f32_to_i32_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_i32(2.0), 2147483647);
        assert_eq!(f32_to_i32(-2.0), -2147483648);
    }

    #[test]
    fn u16_silence_read_as_i16_becomes_full_scale_dc() {
        // 修正前は F32 以外をすべて i16 として扱っていた。
        // u16 の無音 (32768) を i16 として読むと最大振幅の直流になり、
        // ストリームが構築できた場合でも正しい音にならない
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(i16_to_f32(32768u16 as i16), -1.0);
    }

    #[test]
    fn f32_round_trip_preserves_sample_within_quantization_error() {
        // リングバッファの表現は f32。入力側で正規化した値が出力側で元の量子化値へ戻る
        for raw in [-32768i16, -1, 0, 1, 32767] {
            assert_eq!(f32_to_i16(i16_to_f32(raw)), raw);
        }
        for raw in [0u16, 1, 32768, 65535] {
            assert_eq!(f32_to_u16(u16_to_f32(raw)), raw);
        }
    }
}
