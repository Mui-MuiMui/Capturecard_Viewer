use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SupportedStreamConfigRange};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::HeapRb;

pub struct AudioCapture {
    host: cpal::Host,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    is_active: bool,
    volume: Arc<Mutex<f32>>,
    // 簡素化されたリングバッファ（シングルバッファ構成）
    buffer_capacity: usize,

    audio_passthrough_enabled: Arc<AtomicBool>,
    // 音声データ用のコンシューマハンドル - 型の複雑さは設計上必要
    #[allow(clippy::type_complexity)]
    raw_audio_consumer: Option<Arc<Mutex<ringbuf::Consumer<f32, Arc<ringbuf::HeapRb<f32>>>>>>,
    #[allow(clippy::type_complexity)]
    processed_audio_consumer: Option<Arc<Mutex<ringbuf::Consumer<f32, Arc<ringbuf::HeapRb<f32>>>>>>,
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

        // 入力ストリーム - F32のみサポート（簡素化）
        let input_stream = if input_config.sample_format() == SampleFormat::F32 {
            let producer_clone = producer.clone();
            input_device.build_input_stream(
                &input_config.config(),
                move |data: &[f32], _| {
                    if let Ok(mut prod) = producer_clone.try_lock() {
                        for &sample in data {
                            let _ = prod.push(sample);
                        }
                    }
                },
                |e| eprintln!("Input stream error: {}", e),
                None,
            )
        } else {
            // I16をF32に変換
            let producer_clone = producer.clone();
            input_device.build_input_stream(
                &input_config.config(),
                move |data: &[i16], _| {
                    if let Ok(mut prod) = producer_clone.try_lock() {
                        for &sample in data {
                            let f32_sample = sample as f32 / i16::MAX as f32;
                            let _ = prod.push(f32_sample);
                        }
                    }
                },
                |e| eprintln!("Input stream error: {}", e),
                None,
            )
        }
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

        // 出力ストリーム - F32のみサポート（簡素化）
        let vol_arc = self.volume.clone();
        let passthrough_arc = self.audio_passthrough_enabled.clone();
        let output_stream = if output_config.sample_format() == SampleFormat::F32 {
            let consumer_clone = consumer.clone();
            output_device.build_output_stream(
                &output_config.config(),
                move |data: &mut [f32], _| {
                    let volume = vol_arc.lock().map(|v| *v).unwrap_or(1.0);
                    let passthrough_enabled = passthrough_arc.load(Ordering::Relaxed);
                    if let Ok(mut cons) = consumer_clone.try_lock() {
                        render_output_samples(
                            data,
                            volume,
                            passthrough_enabled,
                            || cons.pop(),
                            |value| value,
                        );
                    } else {
                        data.fill(0.0);
                    }
                },
                |e| eprintln!("Output stream error: {}", e),
                None,
            )
        } else {
            // I16への変換
            let consumer_clone = consumer.clone();
            output_device.build_output_stream(
                &output_config.config(),
                move |data: &mut [i16], _| {
                    let volume = vol_arc.lock().map(|v| *v).unwrap_or(1.0);
                    let passthrough_enabled = passthrough_arc.load(Ordering::Relaxed);
                    if let Ok(mut cons) = consumer_clone.try_lock() {
                        render_output_samples(
                            data,
                            volume,
                            passthrough_enabled,
                            || cons.pop(),
                            |value| (value * i16::MAX as f32) as i16,
                        );
                    } else {
                        data.fill(0);
                    }
                },
                |e| eprintln!("Output stream error: {}", e),
                None,
            )
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

        render_output_samples(
            &mut data,
            1.0,
            true,
            || source.pop(),
            |value| (value * i16::MAX as f32) as i16,
        );

        assert_eq!(data, [32767, -32767]);
    }

    #[test]
    fn render_output_samples_i16_passthrough_disabled_writes_zero() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(
            &mut data,
            1.0,
            false,
            || source.pop(),
            |value| (value * i16::MAX as f32) as i16,
        );

        assert_eq!(data, [0, 0]);
    }

    #[test]
    fn render_output_samples_empty_output_buffer_does_not_consume_source() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data: [f32; 0] = [];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 0);
    }
}
