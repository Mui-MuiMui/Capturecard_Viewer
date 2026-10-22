//! フェイクの音声デバイスが名乗るデバイスの一覧と形（`fake.rs` の表）。
//!
//! 一覧の組み立てと名前での引き当てだけを持つ。開く・閉じるは `fake.rs` の
//! `FakeAudioCapture`。

use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange};

use super::{AudioDirection, AudioError};

const INPUT_NAME_PREFIX: &str = "Fake Audio Input";
const OUTPUT_NAME_PREFIX: &str = "Fake Audio Output";

/// 入力デバイスの形。どの入力も同じ
pub(super) const INPUT_SAMPLE_RATE: u32 = 48_000;
pub(super) const INPUT_CHANNELS: u16 = 2;

/// 出力デバイスの形。`(サンプリングレート, チャンネル数)`。
/// 1 番は入力と揃い、2 番は揃わない（変換の経路を通すため。ドリフト補正はどちらにも掛かる）
const OUTPUTS: [(u32, u16); 2] = [(48_000, 2), (44_100, 1)];

/// 1 番の入力の正弦波の周波数。n 番はこの n 倍
const BASE_FREQUENCY_HZ: f64 = 440.0;

/// デバイス 1 台の形。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct FakeDevice {
    pub(super) name: String,
    pub(super) sample_rate: u32,
    pub(super) channels: u16,
    /// 入力なら正弦波の周波数。出力では使わない
    pub(super) frequency_hz: f64,
}

impl FakeDevice {
    /// 対応設定。実機の WASAPI 共有モードと同じく、既定の形 1 つだけを返す
    pub(super) fn configs(&self) -> Vec<SupportedStreamConfigRange> {
        vec![SupportedStreamConfigRange::new(
            self.channels,
            self.sample_rate,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )]
    }

    pub(super) fn default_config(&self) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            self.channels,
            self.sample_rate,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )
    }
}

/// 入力デバイスの一覧。`input_count` 台を名乗る
pub(super) fn inputs(input_count: u32) -> Vec<FakeDevice> {
    (1..=input_count)
        .map(|index| FakeDevice {
            name: format!("{INPUT_NAME_PREFIX} {index}"),
            sample_rate: INPUT_SAMPLE_RATE,
            channels: INPUT_CHANNELS,
            frequency_hz: BASE_FREQUENCY_HZ * f64::from(index),
        })
        .collect()
}

pub(super) fn outputs() -> Vec<FakeDevice> {
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

/// 名前でデバイスを引く。`None` なら既定（1 番）
pub(super) fn find(
    input_count: u32,
    direction: AudioDirection,
    device_name: Option<&str>,
) -> Result<FakeDevice, AudioError> {
    let devices = match direction {
        AudioDirection::Input => inputs(input_count),
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
