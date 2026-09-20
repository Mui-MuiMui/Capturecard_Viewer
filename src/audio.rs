use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{debug, error, info, trace};
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
    volume: Arc<Mutex<f32>>,
    audio_passthrough_enabled: Arc<AtomicBool>,
    // 稼働中のストリームでエラーが起きたことを表す旗。
    //
    // cpal のエラーコールバックはデバイスが消えた（`DeviceNotAvailable`）
    // ときにも呼ばれるが、呼ばれるのは cpal のストリームスレッドなので
    // そこから再接続を始められない。旗を立てるだけにして、UI スレッドが
    // 毎フレーム回収する。
    //
    // **ストリームを開き直すたびに新しい `Arc` へ差し替える。** 使い回すと、
    // 閉じたストリームのエラーコールバックが後から旗を立て、開き直した直後の
    // 正常なストリームを切断と誤判定する
    stream_error: Arc<AtomicBool>,
}

impl AudioCapture {
    pub fn new() -> Self {
        let host = cpal::default_host();
        debug!("AudioCapture を作成した（ホスト: {:?}）", host.id());

        Self {
            host,
            input_stream: None,
            output_stream: None,
            volume: Arc::new(Mutex::new(1.0)),
            // 既定では音声パススルーを有効にする（音が出る状態で起動する）
            audio_passthrough_enabled: Arc::new(AtomicBool::new(true)),
            stream_error: Arc::new(AtomicBool::new(false)),
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
        desired_sample_rate: Option<u32>,
        desired_channels: Option<u16>,
    ) -> Result<(), String> {
        self.stop_capture();
        info!("音声パススルーを開始する");

        // デバイス取得の簡素化
        let input_device = if let Some(name) = input_device_name {
            debug!("入力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, true)?
        } else {
            debug!("既定の入力デバイスを使う");
            self.host
                .default_input_device()
                .ok_or_else(|| "No default input device".to_string())?
        };

        let output_device = if let Some(name) = output_device_name {
            debug!("出力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, false)?
        } else {
            debug!("既定の出力デバイスを使う");
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
        info!(
            "使用するデバイス - 入力: {}、出力: {}",
            input_device_name, output_device_name
        );

        // デバイスの既定設定。希望値が無いときの基準であり、
        // 対応設定を列挙できなかったときの退避先でもある
        let input_default = input_device
            .default_input_config()
            .map_err(|e| format!("Failed to get input config: {}", e))?;

        let output_default = output_device
            .default_output_config()
            .map_err(|e| format!("Failed to get output config: {}", e))?;

        // 設定画面で選んだサンプルレート・チャンネル数を、デバイスが対応する
        // 組み合わせの中で最も近いものへ寄せる。列挙できない、または選べる設定が
        // 無いデバイスでは既定設定のまま開く（従来の挙動）
        let input_config = input_device
            .supported_input_configs()
            .ok()
            .and_then(|configs| {
                select_best_config(
                    &configs.collect::<Vec<_>>(),
                    desired_sample_rate.unwrap_or_else(|| input_default.sample_rate().0),
                    desired_channels.unwrap_or_else(|| input_default.channels()),
                )
            })
            .unwrap_or(input_default);

        let output_config = output_device
            .supported_output_configs()
            .ok()
            .and_then(|configs| {
                select_best_config(
                    &configs.collect::<Vec<_>>(),
                    desired_sample_rate.unwrap_or_else(|| output_default.sample_rate().0),
                    desired_channels.unwrap_or_else(|| output_default.channels()),
                )
            })
            .unwrap_or(output_default);

        info!(
            "音声の設定 - 入力: {}Hz {}ch ({:?})、出力: {}Hz {}ch ({:?})",
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

        debug!("リングバッファを作成した（{} サンプル）", buffer_size * 2);

        // このストリーム専用のエラー旗。開き直すたびに作り直す
        let stream_error = Arc::new(AtomicBool::new(false));

        // 入力ストリーム。デバイスのサンプル型ごとに正規化の仕方が違うので明示的に分ける
        let input_stream_config = input_config.config();
        let input_stream = match input_config.sample_format() {
            SampleFormat::F32 => build_input_stream_with::<f32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                |sample| sample,
            ),
            SampleFormat::I16 => build_input_stream_with::<i16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                i16_to_f32,
            ),
            SampleFormat::U16 => build_input_stream_with::<u16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                u16_to_f32,
            ),
            SampleFormat::I32 => build_input_stream_with::<i32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
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
                stream_error.clone(),
                |sample| sample,
            ),
            SampleFormat::I16 => build_output_stream_with::<i16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                stream_error.clone(),
                f32_to_i16,
            ),
            SampleFormat::U16 => build_output_stream_with::<u16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                stream_error.clone(),
                f32_to_u16,
            ),
            SampleFormat::I32 => build_output_stream_with::<i32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                vol_arc,
                passthrough_arc,
                stream_error.clone(),
                f32_to_i32,
            ),
            other => return Err(unsupported_sample_format_error("出力", other)),
        }
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

        // ストリーム開始
        debug!("音声ストリームを開始する");
        input_stream
            .play()
            .map_err(|e| format!("Failed to start input stream: {}", e))?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        output_stream
            .play()
            .map_err(|e| format!("Failed to start output stream: {}", e))?;

        self.input_stream = Some(input_stream);
        self.output_stream = Some(output_stream);
        // 監視の対象を、いま開いたストリームの旗へ差し替える
        self.stream_error = stream_error;

        info!("音声パススルーを開始した");
        Ok(())
    }

    pub fn stop_capture(&mut self) {
        if let Some(s) = self.input_stream.take() {
            let _ = s.pause();
        }
        if let Some(s) = self.output_stream.take() {
            let _ = s.pause();
        }
        // 閉じたストリームのエラーコールバックが後から立てる旗を読まないよう、
        // 監視対象を新しいものへ差し替える
        self.stream_error = Arc::new(AtomicBool::new(false));
    }

    /// 稼働中のストリームでエラーが起きていたかを返し、旗を下ろす。
    ///
    /// デバイスが消えたときの `DeviceNotAvailable` もここに現れる。
    /// 読んだ側が再接続を要求する責任を持つため、読み取りと同時に下ろす。
    /// `&self` なのは、UI スレッドが `Mutex` の可変借用を取らずに
    /// 毎フレーム確認できるようにするため
    pub fn take_stream_error(&self) -> bool {
        self.stream_error.swap(false, Ordering::Relaxed)
    }

    pub fn set_volume(&mut self, volume_percent: f32) {
        let v = (volume_percent / 100.0).clamp(0.0, 2.0);
        if let Ok(mut vol) = self.volume.lock() {
            *vol = v;
        }
    }

    pub fn set_audio_passthrough_enabled(&mut self, enabled: bool) {
        // apply_settings から 2 秒ごとに呼ばれる。変化の有無を判別できないので trace に落とす
        trace!("音声パススルーの有効/無効を設定する: {}", enabled);
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

/// 対応しているサンプルフォーマットの優先度。小さいほど優先する。未対応なら `None`。
///
/// `build_input_stream_with` / `build_output_stream_with` で扱える型と一致させること。
/// ここに無いフォーマットを選ぶと、設定としては選べてもストリームを組み立てられない。
fn sample_format_priority(format: SampleFormat) -> Option<u8> {
    match format {
        // リングバッファと同じ表現なので変換が要らない
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::I32 => Some(2),
        SampleFormat::U16 => Some(3),
        _ => None,
    }
}

/// デバイスが対応する設定から、希望するサンプルレート・チャンネル数に最も近いものを選ぶ。
///
/// 選ぶ順は チャンネル数の差 → サンプルレートの差 → サンプルフォーマットの優先度。
/// チャンネル数を先に見るのは、モノラルとステレオの違いが聴感に直結するのに対し、
/// サンプルレートは必ず「対応している中で最も近い値」へ寄せられるため。
/// すべて同点なら列挙順の先頭を選ぶ（デバイスが優先する設定が先に来る）。
///
/// WASAPI はデバイスのミックスフォーマットのチャンネル数しか列挙しないため、
/// モノラルを希望してもステレオしか選べないことがある。UI の選択肢をデバイスの
/// 能力から生成する作業は別タスク。
///
/// 選べる設定が 1 つも無ければ `None`。呼び出し側はデバイスの既定設定へ落とす。
fn select_best_config(
    configs: &[SupportedStreamConfigRange],
    desired_sample_rate: u32,
    desired_channels: u16,
) -> Option<SupportedStreamConfig> {
    configs
        .iter()
        .filter_map(|range| {
            let priority = sample_format_priority(range.sample_format())?;
            let min_rate = range.min_sample_rate().0;
            let max_rate = range.max_sample_rate().0;
            // 壊れた列挙で clamp が panic するのを避ける
            if min_rate > max_rate {
                return None;
            }

            let rate = desired_sample_rate.clamp(min_rate, max_rate);
            let key = (
                range.channels().abs_diff(desired_channels),
                rate.abs_diff(desired_sample_rate),
                priority,
            );
            Some((key, range.try_with_sample_rate(SampleRate(rate))?))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, config)| config)
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
    stream_error: Arc<AtomicBool>,
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
        move |e| {
            error!("入力ストリームのエラー: {}", e);
            // 呼ばれるのは cpal のストリームスレッド。ここで開き直すと
            // ストリーム自身を drop することになるので、旗を立てるだけにする
            stream_error.store(true, Ordering::Relaxed);
        },
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
    stream_error: Arc<AtomicBool>,
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
        move |e| {
            error!("出力ストリームのエラー: {}", e);
            // 入力側と同じ理由で、旗を立てるだけにする
            stream_error.store(true, Ordering::Relaxed);
        },
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

    /// テスト用の対応設定。`supported_input_configs()` が返す形を模す。
    fn config_range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(min_rate),
            SampleRate(max_rate),
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    /// WASAPI のように離散的なレートを列挙するデバイスを模す
    fn discrete_range(
        channels: u16,
        rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        config_range(channels, rate, rate, format)
    }

    #[test]
    fn select_best_config_exact_match_is_chosen() {
        let configs = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 96000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(48000));
        assert_eq!(selected.channels(), 2);
    }

    #[test]
    fn select_best_config_unsupported_rate_falls_back_to_nearest() {
        // 44100 は列挙されていない。48000 (差 3900) が 32000 (差 12100) より近い
        let configs = [
            discrete_range(2, 32000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 44100, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(48000));
    }

    #[test]
    fn select_best_config_equidistant_rates_pick_the_first() {
        // 40000 は 32000 と 48000 の中間。列挙順の先頭を選ぶ
        let configs = [
            discrete_range(2, 32000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 40000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(32000));
    }

    #[test]
    fn select_best_config_prefers_matching_channels_over_matching_rate() {
        // WASAPI はミックスフォーマットのチャンネル数しか列挙しないが、
        // 複数出る環境ではチャンネル数を先に合わせる
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(1, 44100, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 1).expect("選べるはず");

        assert_eq!(selected.channels(), 1);
        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }

    #[test]
    fn select_best_config_unavailable_channels_falls_back_to_nearest() {
        // モノラルを希望してもステレオしか無ければステレオを選ぶ
        let configs = [discrete_range(2, 48000, SampleFormat::F32)];

        let selected = select_best_config(&configs, 48000, 1).expect("選べるはず");

        assert_eq!(selected.channels(), 2);
    }

    #[test]
    fn select_best_config_skips_unsupported_sample_formats() {
        // U8 と I64 は変換関数が無く、選んでもストリームを組み立てられない。
        // 希望レートに一致していても選ばない
        let configs = [
            discrete_range(2, 48000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::I64),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_format(), SampleFormat::I16);
        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }

    #[test]
    fn select_best_config_prefers_f32_when_rate_and_channels_tie() {
        // f32 はリングバッファと同じ表現なので変換が要らない
        let configs = [
            discrete_range(2, 48000, SampleFormat::I16),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn select_best_config_clamps_into_a_continuous_range() {
        // 連続した範囲を返すホストでは、範囲内へ丸める
        let configs = [config_range(2, 8000, 96000, SampleFormat::F32)];

        let inside = select_best_config(&configs, 44100, 2).expect("選べるはず");
        assert_eq!(inside.sample_rate(), SampleRate(44100));

        let above = select_best_config(&configs, 192000, 2).expect("選べるはず");
        assert_eq!(above.sample_rate(), SampleRate(96000));

        let below = select_best_config(&configs, 5512, 2).expect("選べるはず");
        assert_eq!(below.sample_rate(), SampleRate(8000));
    }

    #[test]
    fn select_best_config_empty_list_returns_none() {
        assert!(select_best_config(&[], 48000, 2).is_none());
    }

    #[test]
    fn select_best_config_all_unsupported_formats_returns_none() {
        let configs = [
            discrete_range(2, 48000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::F64),
        ];

        assert!(select_best_config(&configs, 48000, 2).is_none());
    }

    #[test]
    fn select_best_config_reversed_range_is_skipped() {
        // min > max の壊れた列挙で panic しないこと
        let configs = [
            config_range(2, 96000, 8000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }
}
