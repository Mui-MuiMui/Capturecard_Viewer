//! 音声デバイスの対応設定の取得と、設定画面に出す選択肢の組み立て。
//!
//! 実際に開く設定を選ぶのは `stream_config`。ここは「デバイスが何を
//! 扱えるか」と「ユーザーに何を見せるか」だけを持つ。

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SupportedStreamConfigRange};

use super::stream_config::sample_format_priority;
use super::{AudioDirection, AudioError};

/// 音声デバイスが対応する設定。
///
/// `supported_input_configs()` / `supported_output_configs()` は WASAPI で
/// 13 レート × 5 形式の `IsFormatSupported`（実測 300ms 前後）になるため、
/// UI スレッドでは呼ばない。デバイスワーカーが一度取ってこの型で持ち回す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioCapabilities {
    /// デバイスが列挙した対応設定。`select_best_config` へそのまま渡せる
    configs: Vec<SupportedStreamConfigRange>,
    /// デバイスの既定設定（WASAPI のミックスフォーマット）
    default_sample_rate: u32,
    default_channels: u16,
}

impl AudioCapabilities {
    /// 対応設定を直接組み立てる。実機は `query_capabilities` が作るので、
    /// これを使うのはデバイスを列挙しないフェイク（`super::fake`）だけ
    pub(super) fn new(
        configs: Vec<SupportedStreamConfigRange>,
        default_sample_rate: u32,
        default_channels: u16,
    ) -> Self {
        Self {
            configs,
            default_sample_rate,
            default_channels,
        }
    }

    /// デバイスが列挙した対応設定。
    pub fn configs(&self) -> &[SupportedStreamConfigRange] {
        &self.configs
    }

    /// デバイスの既定のサンプリングレート。
    pub fn default_sample_rate(&self) -> u32 {
        self.default_sample_rate
    }

    /// デバイスの既定のチャンネル数。
    pub fn default_channels(&self) -> u16 {
        self.default_channels
    }

    /// 設定画面に出せるサンプリングレート。昇順・重複なし。
    pub fn sample_rates(&self) -> Vec<u32> {
        supported_sample_rates(&self.configs)
    }

    /// 設定画面に出せるチャンネル数。昇順・重複なし。
    pub fn channels(&self) -> Vec<u16> {
        supported_channels(&self.configs)
    }
}

/// 指定したデバイスの対応設定を取り、`AudioCapabilities` にまとめる。
///
/// **UI スレッドから直接呼ばないこと。** 列挙は WASAPI への問い合わせを
/// 繰り返すため実測 300ms 前後かかる。呼ぶのはデバイスワーカースレッド
/// （`app::worker_connect`）で、結果はチャネルで UI スレッドへ返る。
///
/// `AudioCapture` のホストは使わず、この関数の中で新しく作る。`AudioCapture`
/// を持たないスレッドからも呼べるようにしてあり、実際 `query_capabilities`
/// だけを別スレッドへ切り出すことになっても手を入れずに済む。
pub fn query_capabilities(
    direction: AudioDirection,
    device_name: Option<&str>,
) -> Result<AudioCapabilities, AudioError> {
    let host = cpal::default_host();

    let device = match device_name {
        Some(name) => find_device_in_host(&host, name, direction)?,
        None => match direction {
            AudioDirection::Input => host.default_input_device(),
            AudioDirection::Output => host.default_output_device(),
        }
        .ok_or(AudioError::NoDefaultDevice(direction))?,
    };

    let default_config = match direction {
        AudioDirection::Input => device.default_input_config(),
        AudioDirection::Output => device.default_output_config(),
    }
    .map_err(|e| AudioError::DefaultConfigFailed {
        direction,
        source: e.to_string(),
    })?;

    let configs = match direction {
        AudioDirection::Input => device.supported_input_configs().map(|it| it.collect()),
        AudioDirection::Output => device.supported_output_configs().map(|it| it.collect()),
    }
    .map_err(|e| AudioError::SupportedConfigsFailed {
        direction,
        source: e.to_string(),
    })?;

    Ok(AudioCapabilities {
        configs,
        default_sample_rate: default_config.sample_rate().0,
        default_channels: default_config.channels(),
    })
}

/// ホストの一覧から名前でデバイスを探す。`AudioCapture::find_device_by_name` と
/// 同じことを、`AudioCapture` を持たない場所（`query_capabilities`）から
/// 行うためのもの。
fn find_device_in_host(
    host: &cpal::Host,
    name: &str,
    direction: AudioDirection,
) -> Result<Device, AudioError> {
    let devices = match direction {
        AudioDirection::Input => host.input_devices(),
        AudioDirection::Output => host.output_devices(),
    }
    .map_err(|e| AudioError::DeviceEnumerationFailed {
        direction,
        source: e.to_string(),
    })?;

    for device in devices {
        if device.name().ok().as_deref() == Some(name) {
            return Ok(device);
        }
    }
    Err(AudioError::DeviceNotFound {
        direction,
        name: name.to_string(),
    })
}

/// 設定画面に出すサンプリングレートの当たり値。
///
/// 連続した範囲（min < max）を返すホストでは「対応している値」を列挙できない
/// ため、この一覧のうち範囲に収まるものを候補にする。WASAPI のように離散値を
/// 列挙するホストでは、列挙された値がそのまま候補になるのでここは効かない。
const BASELINE_SAMPLE_RATES: [u32; 7] = [8000, 16000, 22050, 32000, 44100, 48000, 96000];

/// 能力を取得できなかったときに出す既定の選択肢。
///
/// 従来の固定一覧と同じ。デバイスが対応しない値も選べるが、`select_best_config`
/// が最も近い値へ寄せるので開けなくなることはない。
pub const FALLBACK_SAMPLE_RATES: [u32; 7] = BASELINE_SAMPLE_RATES;
/// 同上、チャンネル数の既定の選択肢。
pub const FALLBACK_CHANNELS: [u16; 2] = [1, 2];

/// 対応設定の一覧から、選択肢に出せるサンプリングレートを作る。昇順・重複なし。
///
/// 扱えないサンプル形式（`sample_format_priority` が `None` を返すもの）しか
/// 持たない設定は、選んでもストリームを組み立てられないので数に入れない。
pub(super) fn supported_sample_rates(configs: &[SupportedStreamConfigRange]) -> Vec<u32> {
    let mut rates = Vec::new();
    for range in configs {
        if sample_format_priority(range.sample_format()).is_none() {
            continue;
        }
        let min = range.min_sample_rate().0;
        let max = range.max_sample_rate().0;
        // 壊れた列挙は無視する（select_best_config と同じ扱い）
        if min > max {
            continue;
        }
        if min == max {
            rates.push(min);
        } else {
            rates.extend(
                BASELINE_SAMPLE_RATES
                    .iter()
                    .copied()
                    .filter(|&rate| (min..=max).contains(&rate)),
            );
        }
    }
    rates.sort_unstable();
    rates.dedup();
    rates
}

/// 対応設定の一覧から、選択肢に出せるチャンネル数を作る。昇順・重複なし。
pub(super) fn supported_channels(configs: &[SupportedStreamConfigRange]) -> Vec<u16> {
    let mut channels: Vec<u16> = configs
        .iter()
        .filter(|range| sample_format_priority(range.sample_format()).is_some())
        .filter(|range| range.min_sample_rate() <= range.max_sample_rate())
        .map(|range| range.channels())
        .collect();
    channels.sort_unstable();
    channels.dedup();
    channels
}

/// 選択肢の出どころ。UI が添える説明を出し分けるために持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceSource {
    /// 入出力の両方が対応する値だけを出している
    Common,
    /// 共通の値が無いので両方の和を出している。開くときに入出力で別の値になる
    Disjoint,
    /// 片方のデバイスの能力しか取れていないので、そちらだけで作った
    OneSided,
    /// どちらの能力も取れていないので固定の既定一覧
    Fallback,
}

/// 設定画面に出す選択肢と、その出どころ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChoices<T> {
    pub values: Vec<T>,
    pub source: ChoiceSource,
}

/// 入出力の能力から選択肢を組み立てる共通処理。
///
/// - 両方取れていて共通部分があれば共通部分（`Common`）
/// - 両方取れていて共通部分が無ければ和（`Disjoint`）
/// - 片方だけ取れていればその一覧（`OneSided`）
/// - どちらも取れていなければ `fallback`（`Fallback`）
///
/// 共通部分が空のときに片側だけを出さないのは、どちらのデバイスに合わせたいかを
/// ユーザーが決められるようにするため。
fn build_choices<T: Copy + Ord>(
    input: Option<Vec<T>>,
    output: Option<Vec<T>>,
    fallback: &[T],
) -> AudioChoices<T> {
    match (input, output) {
        (Some(input), Some(output)) => {
            let common = intersect_sorted(&input, &output);
            if !common.is_empty() {
                return AudioChoices {
                    values: common,
                    source: ChoiceSource::Common,
                };
            }
            let mut union = input;
            union.extend(output);
            union.sort_unstable();
            union.dedup();
            AudioChoices {
                values: union,
                source: ChoiceSource::Disjoint,
            }
        }
        (Some(values), None) | (None, Some(values)) => AudioChoices {
            values,
            source: ChoiceSource::OneSided,
        },
        (None, None) => AudioChoices {
            values: fallback.to_vec(),
            source: ChoiceSource::Fallback,
        },
    }
}

/// 片方だけ能力が取れているときに、空の一覧を「取れていない」と同じに扱う。
///
/// 対応設定を列挙できても、扱えるサンプル形式が 1 つも無ければ候補は空になる。
/// 空のまま `build_choices` へ渡すと共通部分も和も空になり、選択肢が消える。
fn non_empty<T>(values: Vec<T>) -> Option<Vec<T>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// 設定画面に出すサンプリングレートの選択肢。
pub fn selectable_sample_rates(
    input: Option<&AudioCapabilities>,
    output: Option<&AudioCapabilities>,
) -> AudioChoices<u32> {
    build_choices(
        input.and_then(|caps| non_empty(caps.sample_rates())),
        output.and_then(|caps| non_empty(caps.sample_rates())),
        &FALLBACK_SAMPLE_RATES,
    )
}

/// 設定画面に出すチャンネル数の選択肢。
pub fn selectable_channels(
    input: Option<&AudioCapabilities>,
    output: Option<&AudioCapabilities>,
) -> AudioChoices<u16> {
    build_choices(
        input.and_then(|caps| non_empty(caps.channels())),
        output.and_then(|caps| non_empty(caps.channels())),
        &FALLBACK_CHANNELS,
    )
}

/// 昇順の 2 つの一覧の共通部分。
pub(super) fn intersect_sorted<T: Copy + Ord>(a: &[T], b: &[T]) -> Vec<T> {
    a.iter()
        .copied()
        .filter(|value| b.contains(value))
        .collect()
}

/// 候補の中から希望値に最も近いものを返す。候補が空なら `None`。
///
/// 同じ差の候補が 2 つあるときは先に来たほう（一覧は昇順なので小さいほう）を選ぶ。
/// `select_best_config` が同点で列挙順の先頭を採るのと揃えてある。
pub fn nearest_sample_rate(values: &[u32], desired: u32) -> Option<u32> {
    values.iter().copied().min_by_key(|v| v.abs_diff(desired))
}

/// チャンネル数版の `nearest_sample_rate`。
pub fn nearest_channels(values: &[u16], desired: u16) -> Option<u16> {
    values.iter().copied().min_by_key(|v| v.abs_diff(desired))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{config_range, discrete_range};
    use cpal::SampleFormat;

    /// テスト用の `AudioCapabilities`。既定値は WASAPI のミックスフォーマットを模す。
    fn capabilities(configs: &[SupportedStreamConfigRange]) -> AudioCapabilities {
        AudioCapabilities {
            configs: configs.to_vec(),
            default_sample_rate: 48000,
            default_channels: 2,
        }
    }

    #[test]
    fn supported_sample_rates_discrete_ranges_are_listed_sorted_and_deduped() {
        // WASAPI は min == max の離散値を並べる。同じレートが形式違いで複数出る
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
            discrete_range(2, 48000, SampleFormat::I16),
        ];

        assert_eq!(supported_sample_rates(&configs), vec![44100, 48000]);
    }

    #[test]
    fn supported_sample_rates_continuous_range_uses_baseline_values() {
        // 連続した範囲では「対応している値」を列挙できないので、当たり値のうち
        // 範囲に収まるものを候補にする
        let configs = [config_range(2, 16000, 48000, SampleFormat::F32)];

        assert_eq!(
            supported_sample_rates(&configs),
            vec![16000, 22050, 32000, 44100, 48000]
        );
    }

    #[test]
    fn supported_sample_rates_skips_unsupported_formats() {
        // 変換関数が無い形式は選んでもストリームを組み立てられない
        let configs = [
            discrete_range(2, 96000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert_eq!(supported_sample_rates(&configs), vec![48000]);
    }

    #[test]
    fn supported_sample_rates_skips_reversed_range() {
        // min > max の壊れた列挙。select_best_config と同じく無視する
        let configs = [config_range(2, 96000, 8000, SampleFormat::F32)];

        assert!(supported_sample_rates(&configs).is_empty());
    }

    #[test]
    fn supported_channels_are_sorted_and_deduped() {
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(1, 48000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        assert_eq!(supported_channels(&configs), vec![1, 2]);
    }

    #[test]
    fn supported_channels_skips_unsupported_formats() {
        let configs = [
            discrete_range(6, 48000, SampleFormat::F64),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert_eq!(supported_channels(&configs), vec![2]);
    }

    #[test]
    fn intersect_sorted_keeps_only_common_values() {
        assert_eq!(intersect_sorted(&[1, 2, 3], &[2, 3, 4]), vec![2, 3]);
        assert!(intersect_sorted(&[1, 2], &[3, 4]).is_empty());
        assert!(intersect_sorted::<u32>(&[], &[1]).is_empty());
    }

    #[test]
    fn nearest_sample_rate_picks_the_closest_value() {
        assert_eq!(nearest_sample_rate(&[32000, 48000], 44100), Some(48000));
        assert_eq!(nearest_sample_rate(&[44100, 48000], 44100), Some(44100));
    }

    #[test]
    fn nearest_sample_rate_tie_picks_the_smaller_value() {
        // 一覧は昇順なので、同じ差なら先に来た小さいほうが残る。
        // select_best_config が同点で列挙順の先頭を採るのと揃えてある
        assert_eq!(nearest_sample_rate(&[32000, 48000], 40000), Some(32000));
    }

    #[test]
    fn nearest_sample_rate_empty_list_returns_none() {
        assert_eq!(nearest_sample_rate(&[], 48000), None);
    }

    #[test]
    fn nearest_channels_picks_the_closest_value() {
        assert_eq!(nearest_channels(&[2], 1), Some(2));
        assert_eq!(nearest_channels(&[1, 2], 1), Some(1));
        assert_eq!(nearest_channels(&[], 2), None);
    }

    #[test]
    fn selectable_sample_rates_uses_the_common_values() {
        let input = capabilities(&[
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ]);
        let output = capabilities(&[
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 96000, SampleFormat::F32),
        ]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![48000]);
        assert_eq!(choices.source, ChoiceSource::Common);
    }

    #[test]
    fn selectable_sample_rates_without_common_values_falls_back_to_the_union() {
        // 入力 48kHz・出力 44.1kHz は WASAPI では珍しくない。
        // 片側だけを出すと、どちらに合わせたいかをユーザーが選べなくなる
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);
        let output = capabilities(&[discrete_range(2, 44100, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![44100, 48000]);
        assert_eq!(choices.source, ChoiceSource::Disjoint);
    }

    #[test]
    fn selectable_sample_rates_with_one_side_missing_uses_that_side() {
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), None);

        assert_eq!(choices.values, vec![48000]);
        assert_eq!(choices.source, ChoiceSource::OneSided);
    }

    #[test]
    fn selectable_sample_rates_with_no_capabilities_uses_the_fallback_list() {
        let choices = selectable_sample_rates(None, None);

        assert_eq!(choices.values, FALLBACK_SAMPLE_RATES.to_vec());
        assert_eq!(choices.source, ChoiceSource::Fallback);
    }

    #[test]
    fn selectable_sample_rates_treats_an_empty_capability_as_missing() {
        // 列挙はできたが扱える形式が 1 つも無いデバイス。空のまま共通部分を
        // 取ると選択肢が消えるので、取得できていないのと同じに扱う
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::U8)]);
        let output = capabilities(&[discrete_range(2, 44100, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![44100]);
        assert_eq!(choices.source, ChoiceSource::OneSided);
    }

    #[test]
    fn selectable_channels_uses_the_common_values() {
        let input = capabilities(&[
            discrete_range(1, 48000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ]);
        let output = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);

        let choices = selectable_channels(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![2]);
        assert_eq!(choices.source, ChoiceSource::Common);
    }

    #[test]
    fn selectable_channels_with_no_capabilities_uses_the_fallback_list() {
        let choices = selectable_channels(None, None);

        assert_eq!(choices.values, FALLBACK_CHANNELS.to_vec());
        assert_eq!(choices.source, ChoiceSource::Fallback);
    }
}
