//! サンプル型の変換と、入出力の形が違う場合の変換。
//!
//! リングバッファの内部表現は f32 に統一してあるので、デバイス側のサンプル型は
//! 入力で f32 へ正規化し、出力で書き戻す（`i16_to_f32` などの一連の関数）。
//! レートやチャンネル数の違いは `PassthroughConverter` が吸収する。

use std::sync::Arc;

use super::resample::ResampleTelemetry;

/// 整数サンプルの振幅の基準。f32 の -1.0 が型の最小値、+1.0 が最大値 + 1 に対応する。
/// 2 のべき乗なので f32 の除算・乗算で誤差が出ない。
const I16_SCALE: f32 = 32_768.0;
const I32_SCALE: f32 = 2_147_483_648.0;
/// u16 の原点。無音は 0 ではなく 32768。
const U16_ORIGIN: f32 = 32_768.0;

/// i16 のサンプルを f32（-1.0..1.0）へ正規化する。
pub(super) fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / I16_SCALE
}

/// f32 のサンプルを i16 へ変換する。
///
/// 音量 200% では 1.0 を超える値が来る。Rust の float → int キャストは飽和するので、
/// 折り返して最大音量が最小音量に化けることはない。
pub(super) fn f32_to_i16(sample: f32) -> i16 {
    (sample * I16_SCALE) as i16
}

/// u16 のサンプルを f32（-1.0..1.0）へ正規化する。
///
/// u16 は 32768 が原点なので、そのまま符号付きとして読むと最大振幅の直流になる。
pub(super) fn u16_to_f32(sample: u16) -> f32 {
    (sample as f32 - U16_ORIGIN) / U16_ORIGIN
}

/// f32 のサンプルを u16 へ変換する。
pub(super) fn f32_to_u16(sample: f32) -> u16 {
    (sample * U16_ORIGIN + U16_ORIGIN) as u16
}

/// i32 のサンプルを f32（-1.0..1.0）へ正規化する。
pub(super) fn i32_to_f32(sample: i32) -> f32 {
    sample as f32 / I32_SCALE
}

/// f32 のサンプルを i32 へ変換する。
pub(super) fn f32_to_i32(sample: f32) -> i32 {
    (sample * I32_SCALE) as i32
}

/// 入力ストリームのサンプルを、出力ストリームの形へ変換しながら 1 つずつ渡す。
///
/// リングバッファには入力デバイスの形（入力のレート、入力のチャンネル数で
/// インターリーブ）のまま積まれている。入出力でレートやチャンネル数が違うと、
/// そのまま出しては再生速度とピッチがずれ、チャンネルの割り当ても崩れる。
///
/// - サンプリングレートの違いは**線形補間**で吸収する。外部クレートを足さずに
///   済み、遅延も 1 入力フレームで足りる。折り返し雑音は完全には消えないが、
///   素通しの遅延を優先する用途（`docs/ARCHITECTURE.md` の「設計の前提」）に
///   合わせてこちらを採る
/// - チャンネル数の違いはアップ／ダウンミックスで吸収する。モノラル → 多ch は
///   同じ値を全チャンネルへ、多ch → モノラルは平均。それ以外は先頭から
///   対応させ、余った出力チャンネルは無音にする
///
/// **リアルタイムスレッド（出力コールバック）で動くので、アロケーションも
/// ロックも行わない。** バッファはストリームを組み立てるときに確保する。
///
/// 長時間再生で効いてくるクロックドリフト（入出力のハードウェアクロックが
/// 厳密には一致しないこと）は、リングバッファの水位に応じてレート比を
/// ±0.1% の範囲でわずかに動かして吸収する（`ResampleTelemetry`、
/// `decide_resample_correction`）。水位の観測とレート比の書き換えは別スレッド
/// （デバイスワーカー、`app::worker_loop`）が数秒ごとに行うので、ここは
/// 読み出すだけ。
pub struct PassthroughConverter {
    /// 入出力が同じ形なので変換が要らない。リングバッファの値をそのまま出す
    identity: bool,
    in_channels: usize,
    out_channels: usize,
    /// 出力 1 フレームぶんで進める入力フレーム数（入力レート / 出力レート）。
    /// 丸め誤差が位相のずれとして溜まるので f64 で持つ
    step: f64,
    /// 補間の左端と右端になる入力フレーム。長さは `in_channels`
    prev: Vec<f32>,
    next: Vec<f32>,
    /// `prev` と `next` の間の位置。0.0 以上 1.0 未満
    position: f64,
    /// `prev` / `next` を読み込み済みか
    primed: bool,
    /// 組み立て済みの出力フレーム。長さは `out_channels`
    frame: Vec<f32>,
    /// `frame` の中で次に返すチャンネル
    channel: usize,
    /// 入力が尽きたまま出力フレームの途中にいる。
    ///
    /// **フレーム境界まで `None` を返し続ける。** 尽きた直後に読み直すと、
    /// 出力チャンネルの途中から新しいフレームの 0 番を書くことになり、
    /// 左右が入れ替わる
    starved: bool,
    /// クロックドリフト補正の共有状態。`None` なら補正しない（無補正の
    /// `1.0` を使い続ける）
    telemetry: Option<Arc<ResampleTelemetry>>,
}

impl PassthroughConverter {
    pub fn new(
        input_sample_rate: u32,
        input_channels: u16,
        output_sample_rate: u32,
        output_channels: u16,
    ) -> Self {
        // 0 が来ると剰余とゼロ除算で落ちる。cpal は 0 を返さないが、
        // ここで倒れると出力コールバックの中で panic する
        let in_channels = (input_channels as usize).max(1);
        let out_channels = (output_channels as usize).max(1);
        let step = if output_sample_rate == 0 {
            1.0
        } else {
            f64::from(input_sample_rate) / f64::from(output_sample_rate)
        };

        // 揃っている場合は補間も再配置も要らない。**この経路を残すのは、
        // 揃えて開けた場合（`select_aligned_configs`）に従来どおり
        // リングバッファの値をそのまま出すため。**
        let identity = in_channels == out_channels && input_sample_rate == output_sample_rate;

        Self {
            identity,
            in_channels,
            out_channels,
            step,
            prev: vec![0.0; in_channels],
            next: vec![0.0; in_channels],
            position: 0.0,
            primed: false,
            frame: vec![0.0; out_channels],
            channel: 0,
            starved: false,
            telemetry: None,
        }
    }

    /// クロックドリフト補正の共有状態を紐づける。`None` なら補正しない
    /// （入出力の形が揃っている場合はこちらのまま使う）。
    pub fn with_telemetry(mut self, telemetry: Option<Arc<ResampleTelemetry>>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// 変換が要らない組み合わせか。ログとテストのための問い合わせ。
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// 出力コールバックが呼ぶ。リングバッファの水位を書く。
    /// 補正の対象外（`telemetry` が無い）ストリームでは何もしない。
    pub fn record_water_level(&self, level: usize) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.record_water_level(level);
        }
    }

    /// 出力サンプルを 1 つ取り出す。入力が足りなければ `None`。
    ///
    /// `pop` はリングバッファから入力サンプルを 1 つ取り出す。
    pub fn next_sample(&mut self, pop: &mut impl FnMut() -> Option<f32>) -> Option<f32> {
        if self.identity {
            return pop();
        }

        // 出力フレームの先頭でだけ入力を読む。途中で読むとチャンネルがずれる
        if self.channel == 0 {
            self.starved = !self.fill_frame(pop);
            if self.starved {
                // 途中まで読んだフレームは捨て、次のフレーム境界で組み立て直す
                self.primed = false;
                self.position = 0.0;
            }
        }

        let sample = if self.starved {
            None
        } else {
            Some(self.frame[self.channel])
        };

        self.channel += 1;
        if self.channel >= self.out_channels {
            self.channel = 0;
        }
        sample
    }

    /// 次の出力フレームを組み立てる。入力が足りなければ `false`。
    fn fill_frame(&mut self, pop: &mut impl FnMut() -> Option<f32>) -> bool {
        if !self.primed {
            if !read_frame(&mut self.prev, pop) || !read_frame(&mut self.next, pop) {
                return false;
            }
            self.primed = true;
            self.position = 0.0;
        }

        // 位置が `next` を追い越しているあいだ、入力フレームを進める。
        // ダウンサンプル（step > 1）では 1 回の出力で複数フレーム進む
        while self.position >= 1.0 {
            std::mem::swap(&mut self.prev, &mut self.next);
            if !read_frame(&mut self.next, pop) {
                return false;
            }
            self.position -= 1.0;
        }

        mix_frame(
            &self.prev,
            &self.next,
            self.position as f32,
            self.in_channels,
            &mut self.frame,
        );
        // クロックドリフト補正: デバイスワーカーが水位に応じて書き換えた
        // 係数を毎フレーム読む。ロックを取らない Atomic の読み出しだけなので
        // リアルタイムスレッドでも安全
        let correction = self
            .telemetry
            .as_ref()
            .map(|telemetry| telemetry.correction())
            .unwrap_or(1.0);
        self.position += self.step * f64::from(correction);
        true
    }
}

/// 入力フレームを 1 つ読み込む。途中で尽きたら `false`。
///
/// 尽きた場合に読んだぶんは捨てる。呼び出し側が組み立て直すので、
/// 中途半端なフレームを持ち越さない。
fn read_frame(dst: &mut [f32], pop: &mut impl FnMut() -> Option<f32>) -> bool {
    for slot in dst.iter_mut() {
        match pop() {
            Some(sample) => *slot = sample,
            None => return false,
        }
    }
    true
}

/// 2 つの入力フレームを線形補間し、出力チャンネル数へミックスする。
///
/// `position` は 0.0 で `prev`、1.0 で `next` に一致する位置。
///
/// - 入力がモノラルなら、同じ値を全ての出力チャンネルへ配る
/// - 出力がモノラルなら、入力の全チャンネルの平均を書く
/// - それ以外は先頭から対応させ、余った出力チャンネルは無音にする
///   （5.1ch の出力へ 2ch を流すときにサラウンドへ前方の音が漏れないように）
fn mix_frame(prev: &[f32], next: &[f32], position: f32, in_channels: usize, out: &mut [f32]) {
    let at = |channel: usize| -> f32 {
        let a = prev[channel];
        a + (next[channel] - a) * position
    };

    if in_channels == 1 {
        let sample = at(0);
        out.fill(sample);
        return;
    }

    if out.len() == 1 {
        let sum: f32 = (0..in_channels).map(at).sum();
        out[0] = sum / in_channels as f32;
        return;
    }

    for (channel, slot) in out.iter_mut().enumerate() {
        *slot = if channel < in_channels {
            at(channel)
        } else {
            0.0
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 変換器へ入力サンプルを流し、出せるだけ出力サンプルを取り出す。
    ///
    /// **出力フレーム単位で呼ぶ。** 実際の出力コールバックはバッファ長ぶん
    /// 呼び続け、入力が尽きた区間は無音で埋める。1 サンプルでも `None` が
    /// 出たら止める作りにすると、フレーム境界まで `None` を返す仕様
    /// （`starved`）を確かめられない。
    fn drain_converter(converter: &mut PassthroughConverter, input: &[f32]) -> Vec<f32> {
        let mut source = input
            .iter()
            .copied()
            .collect::<std::collections::VecDeque<_>>();
        let mut pop = || source.pop_front();
        let mut out = Vec::new();
        let frame_len = converter.out_channels;
        loop {
            let before = out.len();
            for _ in 0..frame_len {
                if let Some(sample) = converter.next_sample(&mut pop) {
                    out.push(sample);
                }
            }
            // 1 フレームまるごと出せなければ入力が尽きている
            if out.len() == before || out.len() > 10_000 {
                break;
            }
        }
        out
    }

    #[test]
    fn passthrough_converter_same_format_passes_samples_through() {
        // 揃えて開けた場合は補間も再配置も挟まない
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 2);

        assert!(converter.is_identity());
        assert_eq!(
            drain_converter(&mut converter, &[0.25, -0.5, 1.0]),
            vec![0.25, -0.5, 1.0]
        );
    }

    #[test]
    fn passthrough_converter_is_not_identity_when_only_the_rate_differs() {
        assert!(!PassthroughConverter::new(48000, 2, 44100, 2).is_identity());
    }

    #[test]
    fn passthrough_converter_is_not_identity_when_only_the_channels_differ() {
        assert!(!PassthroughConverter::new(48000, 1, 48000, 2).is_identity());
    }

    #[test]
    fn passthrough_converter_downsampling_keeps_every_other_frame() {
        // 48kHz -> 24kHz。1 出力フレームにつき入力を 2 フレーム進む
        let mut converter = PassthroughConverter::new(48000, 1, 24000, 1);

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);

        assert_eq!(out, vec![0.0, 2.0, 4.0]);
    }

    #[test]
    fn passthrough_converter_upsampling_interpolates_midpoints() {
        // 24kHz -> 48kHz。中間の位置は線形補間で埋める
        let mut converter = PassthroughConverter::new(24000, 1, 48000, 1);

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0]);

        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn passthrough_converter_mono_input_is_copied_to_every_output_channel() {
        let mut converter = PassthroughConverter::new(48000, 1, 48000, 2);

        let out = drain_converter(&mut converter, &[0.25, 0.5, 0.75]);

        // 最後の 1 フレームは補間の右端として読まれるだけで、出力には出ない
        assert_eq!(out, vec![0.25, 0.25, 0.5, 0.5]);
    }

    #[test]
    fn passthrough_converter_multi_channel_input_is_averaged_into_mono() {
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 1);

        // フレームは [1.0, 0.0] / [0.0, 0.0] / [0.5, 0.5]
        let out = drain_converter(&mut converter, &[1.0, 0.0, 0.0, 0.0, 0.5, 0.5]);

        assert_eq!(out, vec![0.5, 0.0]);
    }

    #[test]
    fn passthrough_converter_extra_output_channels_are_silent() {
        // 2ch を 4ch の出力へ流す。前方 2 本だけに配り、残りは無音にする
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 4);

        let out = drain_converter(&mut converter, &[1.0, -1.0, 0.5, -0.5]);

        assert_eq!(out, vec![1.0, -1.0, 0.0, 0.0]);
    }

    #[test]
    fn passthrough_converter_empty_input_returns_none() {
        let mut converter = PassthroughConverter::new(48000, 2, 44100, 2);

        assert!(drain_converter(&mut converter, &[]).is_empty());
    }

    #[test]
    fn passthrough_converter_underrun_does_not_swap_channels() {
        // 入力が尽きたあと、出力フレームの途中から新しいフレームの
        // 0 番を書き始めると左右が入れ替わる。尽きたらフレーム境界まで
        // 何も出さないこと
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 4);

        // 1 フレームだけでは補間の右端が無く、何も出せない
        assert!(drain_converter(&mut converter, &[1.0, -1.0]).is_empty());

        // 改めて 2 フレーム渡すと、先頭のチャンネルから組み立て直す
        let out = drain_converter(&mut converter, &[0.5, -0.5, 0.25, -0.25]);

        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    #[test]
    fn passthrough_converter_output_length_follows_the_rate_ratio() {
        // 再生速度がずれないこと。48kHz の入力 4800 フレームは
        // 44.1kHz の出力でおよそ 4410 フレームになる
        let mut converter = PassthroughConverter::new(48000, 1, 44100, 1);
        let input: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();

        let out = drain_converter(&mut converter, &input);

        // 補間の右端を先読みするぶん数フレーム少なくなる
        assert!(
            (4405..=4410).contains(&out.len()),
            "出力フレーム数が想定から外れている: {}",
            out.len()
        );
    }

    #[test]
    fn passthrough_converter_zero_channels_do_not_panic() {
        // cpal は 0 を返さないが、出力コールバックの中で落ちると復旧できない
        let mut converter = PassthroughConverter::new(48000, 0, 48000, 0);

        assert!(drain_converter(&mut converter, &[1.0, 2.0, 3.0]).len() <= 3);
    }

    #[test]
    fn passthrough_converter_zero_output_rate_does_not_divide_by_zero() {
        let mut converter = PassthroughConverter::new(48000, 1, 0, 1);

        // step が 1.0 に倒れるので、入力をそのまま並べたものになる
        assert_eq!(
            drain_converter(&mut converter, &[1.0, 2.0, 3.0]),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn passthrough_converter_with_telemetry_applies_correction_to_the_step() {
        // 24kHz -> 48kHz（無補正の step は 0.5）。補正係数を 1.5 にすると
        // 実効の step は 0.75 になり、入力を余分に消費して早く進む
        let telemetry = Arc::new(ResampleTelemetry::new(10));
        telemetry.set_correction(1.5);
        let mut converter =
            PassthroughConverter::new(24000, 1, 48000, 1).with_telemetry(Some(telemetry));

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0, 3.0]);

        assert_eq!(out, vec![0.0, 0.75, 1.5, 2.25]);
    }

    #[test]
    fn passthrough_converter_without_telemetry_is_uncorrected() {
        // telemetry を紐づけなければ従来どおり無補正（1.0）で進む
        let mut converter = PassthroughConverter::new(24000, 1, 48000, 1);

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0]);

        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5]);
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
