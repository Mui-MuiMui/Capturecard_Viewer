//! パススルーの出力側の組み立て。出力デバイスを決め、変換器とクロック
//! ドリフト補正を付けて cpal の出力ストリームを作る。
//!
//! **入力の出どころを知らない。** 入力が WASAPI のデバイス（cpal の入力
//! ストリーム）でも DirectShow の音声ピン（`super::pin_feed`）でも、出力側が
//! 見るのはリングバッファの中身と入力の形（レートとチャンネル数）だけなので、
//! `AudioCapture` の 2 つの入り口がここを共有する（`docs/design/directshow-audio.md`
//! の (2)）。コールバックの本体は `stream_output.rs`。

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{debug, info};
use ringbuf::traits::Split;
use ringbuf::HeapRb;
use std::sync::{Arc, Mutex};

use super::capabilities::{device_name, AudioCapabilities};
use super::capture::{ring_buffer_samples, target_water_level};
use super::controls::AudioControls;
use super::convert::PassthroughConverter;
use super::resample::ResampleTelemetry;
use super::sample::{f32_to_i16, f32_to_i32, f32_to_u16};
use super::stream::{AudioConsumer, AudioProducer, StreamCounters};
use super::stream_config::resolve_ranges;
use super::stream_output::{build_output_stream_with, OutputSignals};
use super::{AudioDirection, AudioError};

/// 出力デバイスと、その既定設定・対応設定。
pub(super) struct OutputDevice {
    pub(super) device: Device,
    /// ログと「接続状態」タブに出す名前
    pub(super) name: String,
    /// 希望値が無いときの基準であり、対応設定を列挙できなかったときの退避先
    pub(super) default: SupportedStreamConfig,
    pub(super) ranges: Vec<SupportedStreamConfigRange>,
}

/// 出力デバイスを名前（`None` なら Windows の既定）で探し、既定設定と
/// 対応設定を読む。
///
/// 対応設定は**先にワーカーが取ってあればそれを使う**（`capabilities`）。
/// WASAPI の列挙は 300ms 前後かかるため、開くたびにここで走らせると、
/// ワーカーがその分だけ次のコマンドを処理できなくなる。
pub(super) fn open_output_device(
    host: &cpal::Host,
    name: Option<&str>,
    capabilities: Option<&AudioCapabilities>,
) -> Result<OutputDevice, AudioError> {
    let device = match name {
        Some(name) => {
            debug!("出力デバイスを名前で探す: {}", name);
            find_device_by_name(host, name, AudioDirection::Output)?
        }
        None => {
            debug!("既定の出力デバイスを使う");
            host.default_output_device()
                .ok_or(AudioError::NoDefaultDevice(AudioDirection::Output))?
        }
    };
    let default = device
        .default_output_config()
        .map_err(|e| AudioError::DefaultConfigFailed {
            direction: AudioDirection::Output,
            source: e.to_string(),
        })?;
    let ranges = resolve_ranges(capabilities, AudioDirection::Output, || {
        device.supported_output_configs().map(|it| it.collect())
    });
    let name = device_name(&device).unwrap_or_else(|| "Unknown Output".to_string());
    Ok(OutputDevice {
        device,
        name,
        default,
        ranges,
    })
}

/// 名前でデバイスを探す。向きを `bool` ではなく `AudioDirection` で受けるのは、
/// 見つからなかったときのエラーに入力・出力のどちらかを載せるため。
pub(super) fn find_device_by_name(
    host: &cpal::Host,
    name: &str,
    direction: AudioDirection,
) -> Result<Device, AudioError> {
    let iter = match direction {
        AudioDirection::Input => host.input_devices(),
        AudioDirection::Output => host.output_devices(),
    }
    .map_err(|e| AudioError::DeviceEnumerationFailed {
        direction,
        source: e.to_string(),
    })?;
    for d in iter {
        if device_name(&d).as_deref() == Some(name) {
            return Ok(d);
        }
    }
    Err(AudioError::DeviceNotFound {
        direction,
        name: name.to_string(),
    })
}

/// 入力と出力をつなぐリングバッファ一式。
pub(super) struct PassthroughRing {
    pub(super) producer: Arc<Mutex<AudioProducer>>,
    pub(super) consumer: Arc<Mutex<AudioConsumer>>,
    /// 出力が最初に待つ水位と、クロックドリフト補正が保つ水位。これが遅延になる
    pub(super) target_level: usize,
}

/// リングバッファを作る。
///
/// 長さは設定で選べる（`settings::AudioSettings::buffer_ms`）。小さいほど遅延が
/// 減るが、出力コールバックが間に合わずアンダーランが出やすくなる。容量は
/// 目標水位の 2 倍にして、入力が先行しても後れても同じだけ余裕を持たせる。
pub(super) fn make_ring(sample_rate: u32, channels: usize, buffer_ms: u32) -> PassthroughRing {
    let capacity = ring_buffer_samples(sample_rate, channels, buffer_ms) * 2;
    let target_level = target_water_level(capacity, channels);
    let (producer, consumer) = HeapRb::<f32>::new(capacity).split();
    debug!(
        "リングバッファを作成した（{} サンプル、{} ms 相当 × 2、目標水位 {} サンプル）",
        capacity, buffer_ms, target_level
    );
    PassthroughRing {
        producer: Arc::new(Mutex::new(producer)),
        consumer: Arc::new(Mutex::new(consumer)),
        target_level,
    }
}

/// 組み立てた出力ストリームと、デバイスワーカーが読み書きする補正の共有状態。
pub(super) struct PassthroughOutput {
    pub(super) stream: cpal::Stream,
    pub(super) telemetry: Arc<ResampleTelemetry>,
}

/// 出力ストリームを組み立てる。`input` は入力の `(レート, チャンネル数)`。
///
/// 入出力の形が違う場合の変換器は**ここで作る（ストリームの構築時）。**
/// 補間に使うバッファを先に確保しておかないと、出力コールバックの中で
/// アロケーションが起きる。出力はリングバッファが目標水位まで溜まって
/// から取り出し始める（入力と出力を同時に始めてよいのはこのため）。
///
/// クロックドリフト補正は**入出力の形が揃っていても行う**（Issue #308）。
/// 公称レートが同じでも、キャプチャーカードと出力デバイスは別の時計で動く。
pub(super) fn build_passthrough_output(
    output: &OutputDevice,
    config: &SupportedStreamConfig,
    input: (u32, u16),
    ring: &PassthroughRing,
    controls: Arc<AudioControls>,
    counters: &StreamCounters,
) -> Result<PassthroughOutput, AudioError> {
    let (input_rate, input_channels) = input;
    let converter = PassthroughConverter::new(
        input_rate,
        input_channels,
        config.sample_rate(),
        config.channels(),
    )
    .with_prebuffer(ring.target_level);
    if converter.is_identity() {
        debug!("入出力の形が同じなので変換しない（クロックドリフト補正だけ行う）");
    } else {
        info!(
            "入出力の形が違うので変換する - レート比: {:.4}、チャンネル: {} -> {}",
            f64::from(input_rate) / f64::from(config.sample_rate()),
            input_channels,
            config.channels()
        );
    }
    let telemetry = Arc::new(ResampleTelemetry::new(ring.target_level));
    let converter = converter.with_telemetry(Some(telemetry.clone()));
    let signals = OutputSignals {
        error: counters.error.clone(),
        underruns: counters.underruns.clone(),
    };
    let device = &output.device;
    let stream_config = config.config();
    let consumer = ring.consumer.clone();
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_output_stream_with::<f32>(
            device,
            &stream_config,
            consumer,
            controls,
            signals,
            converter,
            |sample| sample,
        ),
        SampleFormat::I16 => build_output_stream_with::<i16>(
            device,
            &stream_config,
            consumer,
            controls,
            signals,
            converter,
            f32_to_i16,
        ),
        SampleFormat::U16 => build_output_stream_with::<u16>(
            device,
            &stream_config,
            consumer,
            controls,
            signals,
            converter,
            f32_to_u16,
        ),
        SampleFormat::I32 => build_output_stream_with::<i32>(
            device,
            &stream_config,
            consumer,
            controls,
            signals,
            converter,
            f32_to_i32,
        ),
        other => {
            return Err(AudioError::UnsupportedSampleFormat {
                direction: AudioDirection::Output,
                format: other,
            })
        }
    }
    .map_err(|e| AudioError::StreamBuildFailed {
        direction: AudioDirection::Output,
        source: e.to_string(),
    })?;
    Ok(PassthroughOutput { stream, telemetry })
}
