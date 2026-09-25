//! 対応設定の一覧から、実際に開く設定を選ぶ。
//!
//! どの設定を扱えるか（`sample_format_priority`）は `stream` が組み立てられる
//! サンプル型と一致させる必要があるため、選ぶ側のここに置いてある。

use cpal::{SampleFormat, SampleRate, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{debug, info, warn};

use super::capabilities::{
    intersect_sorted, nearest_channels, nearest_sample_rate, supported_channels,
    supported_sample_rates, AudioCapabilities,
};
use super::AudioDirection;

/// 対応しているサンプルフォーマットの優先度。小さいほど優先する。未対応なら `None`。
///
/// `build_input_stream_with` / `build_output_stream_with` で扱える型と一致させること。
/// ここに無いフォーマットを選ぶと、設定としては選べてもストリームを組み立てられない。
pub(super) fn sample_format_priority(format: SampleFormat) -> Option<u8> {
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
pub(super) fn select_best_config(
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

/// 入力と出力の両方が対応する設定を選ぶ。
///
/// 揃えられればリングバッファのサンプルをそのまま流せる。WASAPI は共有モードで
/// ミックスフォーマットしか通さないことが多く、入力 48kHz・出力 44.1kHz のように
/// 揃えられない組み合わせは珍しくない。その場合は `None` を返し、呼び出し側が
/// それぞれの最寄りを選ぶ。
///
/// 共通の候補を出してから `select_best_config` へ同じ値を渡し、**両方が本当に
/// その値で開けるかを最後に確かめる。** レートとチャンネル数を別々に共通化して
/// いるため、「片方はそのレート、もう片方はそのチャンネル数」しか持たない
/// 組み合わせが候補に残りうる。
pub(super) fn select_aligned_configs(
    input: &[SupportedStreamConfigRange],
    output: &[SupportedStreamConfigRange],
    desired_sample_rate: u32,
    desired_channels: u16,
) -> Option<(SupportedStreamConfig, SupportedStreamConfig)> {
    let rates = intersect_sorted(
        &supported_sample_rates(input),
        &supported_sample_rates(output),
    );
    let channels = intersect_sorted(&supported_channels(input), &supported_channels(output));

    let rate = nearest_sample_rate(&rates, desired_sample_rate)?;
    let channels = nearest_channels(&channels, desired_channels)?;

    let input_config = select_best_config(input, rate, channels)?;
    let output_config = select_best_config(output, rate, channels)?;

    if input_config.sample_rate() != output_config.sample_rate()
        || input_config.channels() != output_config.channels()
    {
        return None;
    }

    info!(
        "入出力を同じ設定に揃えた: {}Hz {}ch",
        input_config.sample_rate().0,
        input_config.channels()
    );
    Some((input_config, output_config))
}

/// パススルーで実際に開く入出力の設定を決める。
///
/// 設定画面で選んだサンプルレート・チャンネル数（`None` なら入力デバイスの
/// 既定）を、デバイスが対応する組み合わせの中で最も近いものへ寄せる。
/// 列挙できない、または選べる設定が無いデバイスでは既定設定のまま開く
/// （従来の挙動）。
///
/// **まず入出力で同じ設定に揃えられないかを見る。** 揃っていれば
/// リングバッファのサンプルをそのまま流せる。揃わない組み合わせでは
/// それぞれの最寄りを選び、変換（線形補間とミックス）で吸収する。
///
/// 実機（`capture.rs`）とフェイク（`fake.rs`）の両方が使う。
pub(super) fn choose_passthrough_configs(
    input_ranges: &[SupportedStreamConfigRange],
    output_ranges: &[SupportedStreamConfigRange],
    input_default: SupportedStreamConfig,
    output_default: SupportedStreamConfig,
    desired_sample_rate: Option<u32>,
    desired_channels: Option<u16>,
) -> (SupportedStreamConfig, SupportedStreamConfig) {
    let aligned = select_aligned_configs(
        input_ranges,
        output_ranges,
        desired_sample_rate.unwrap_or_else(|| input_default.sample_rate().0),
        desired_channels.unwrap_or_else(|| input_default.channels()),
    );
    if let Some(pair) = aligned {
        return pair;
    }

    let input_config = select_best_config(
        input_ranges,
        desired_sample_rate.unwrap_or_else(|| input_default.sample_rate().0),
        desired_channels.unwrap_or_else(|| input_default.channels()),
    )
    .unwrap_or(input_default);
    let output_config = select_best_config(
        output_ranges,
        desired_sample_rate.unwrap_or_else(|| output_default.sample_rate().0),
        desired_channels.unwrap_or_else(|| output_default.channels()),
    )
    .unwrap_or(output_default);
    if input_config.sample_rate() != output_config.sample_rate()
        || input_config.channels() != output_config.channels()
    {
        warn!(
            "入出力で共通の設定が無いため別々の設定で開く（線形補間とミックスで変換する）- 入力: {}Hz {}ch、出力: {}Hz {}ch",
            input_config.sample_rate().0,
            input_config.channels(),
            output_config.sample_rate().0,
            output_config.channels()
        );
    }
    (input_config, output_config)
}

/// 対応設定の一覧を用意する。
///
/// ワーカーが先に取ったものがあればそれを使い、無いときだけその場で列挙する。
/// 列挙に失敗したら空を返す。空なら `select_best_config` が `None` を返し、
/// 呼び出し側がデバイスの既定設定へ落ちる（従来の挙動）。
pub(super) fn resolve_ranges<E: std::fmt::Display>(
    cached: Option<&AudioCapabilities>,
    direction: AudioDirection,
    enumerate: impl FnOnce() -> Result<Vec<SupportedStreamConfigRange>, E>,
) -> Vec<SupportedStreamConfigRange> {
    if let Some(caps) = cached {
        debug!(
            "{}デバイスの対応設定は取得済みのものを使う（{} 件）",
            direction.label(),
            caps.configs().len()
        );
        return caps.configs().to_vec();
    }

    let started = std::time::Instant::now();
    match enumerate() {
        Ok(ranges) => {
            debug!(
                "{}デバイスの対応設定をこの場で列挙した（{} 件、{} ms）",
                direction.label(),
                ranges.len(),
                started.elapsed().as_millis()
            );
            ranges
        }
        Err(e) => {
            warn!(
                "{}デバイスの対応設定を列挙できないので既定の設定で開く: {}",
                direction.label(),
                e
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{config_range, discrete_range};

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
    fn select_aligned_configs_matches_both_sides() {
        let input = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];
        let output = [discrete_range(2, 48000, SampleFormat::I16)];

        let (in_config, out_config) =
            select_aligned_configs(&input, &output, 44100, 2).expect("揃えられるはず");

        // 44100 は出力が対応しないので、共通の 48000 へ寄る
        assert_eq!(in_config.sample_rate(), SampleRate(48000));
        assert_eq!(out_config.sample_rate(), SampleRate(48000));
        assert_eq!(in_config.channels(), 2);
        assert_eq!(out_config.channels(), 2);
    }

    #[test]
    fn select_aligned_configs_without_a_common_rate_returns_none() {
        let input = [discrete_range(2, 48000, SampleFormat::F32)];
        let output = [discrete_range(2, 44100, SampleFormat::F32)];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_without_a_common_channel_count_returns_none() {
        let input = [discrete_range(1, 48000, SampleFormat::F32)];
        let output = [discrete_range(2, 48000, SampleFormat::F32)];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_rejects_a_cross_matched_combination() {
        // レートは 48000 が、チャンネル数は 2 が共通しているが、
        // 「48000Hz かつ 2ch」を両方が開けるわけではない
        let input = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(1, 48000, SampleFormat::F32),
        ];
        let output = [
            discrete_range(1, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_empty_lists_return_none() {
        assert!(select_aligned_configs(&[], &[], 48000, 2).is_none());
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

    /// テスト用の既定設定。`default_input_config()` が返す形を模す
    fn default_config(channels: u16, rate: u32) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            channels,
            SampleRate(rate),
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )
    }

    #[test]
    fn choose_passthrough_configs_without_a_common_setting_picks_each_nearest() {
        // 入力 48kHz 2ch、出力 44.1kHz 1ch しか開けない組み合わせ。
        // 揃えられないので、それぞれが開ける設定で開く
        let input = [discrete_range(2, 48000, SampleFormat::F32)];
        let output = [discrete_range(1, 44100, SampleFormat::F32)];

        let (in_config, out_config) = choose_passthrough_configs(
            &input,
            &output,
            default_config(2, 48000),
            default_config(1, 44100),
            None,
            None,
        );

        assert_eq!(in_config.sample_rate(), SampleRate(48000));
        assert_eq!(in_config.channels(), 2);
        assert_eq!(out_config.sample_rate(), SampleRate(44100));
        assert_eq!(out_config.channels(), 1);
    }

    #[test]
    fn choose_passthrough_configs_without_ranges_falls_back_to_the_defaults() {
        // 対応設定を列挙できなかったデバイスは既定設定のまま開く
        let (in_config, out_config) = choose_passthrough_configs(
            &[],
            &[],
            default_config(2, 48000),
            default_config(2, 44100),
            Some(96000),
            Some(1),
        );

        assert_eq!(in_config.sample_rate(), SampleRate(48000));
        assert_eq!(in_config.channels(), 2);
        assert_eq!(out_config.sample_rate(), SampleRate(44100));
        assert_eq!(out_config.channels(), 2);
    }
}
