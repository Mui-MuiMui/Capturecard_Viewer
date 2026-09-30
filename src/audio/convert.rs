//! サンプル型の変換と、入出力の形が違う場合の変換。
//!
//! リングバッファの内部表現は f32 に統一してあるので、デバイス側のサンプル型は
//! 入力で f32 へ正規化し、出力で書き戻す（`i16_to_f32` などの一連の関数）。
//! レートやチャンネル数の違いは `PassthroughConverter` が吸収する。

use std::collections::VecDeque;
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
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
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
    /// `prev` と `next` の間の位置。0.0 以上 1.0 未満。入力が尽きて
    /// フレームを進められなかった間だけ 1.0 以上のまま残る
    position: f64,
    /// `prev` / `next` のうち読み込み済みのフレーム数（0〜2）。
    /// 入力が尽きても読んだフレームは捨てず、続きから読む
    loaded: u8,
    /// 組み立て済みの出力フレーム。長さは `out_channels`。
    /// identity 経路では、リングバッファから読んだ入力フレームをそのまま置く
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
            loaded: 0,
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
    /// `read_frame` はリングバッファから入力フレーム（`dst.len()` =
    /// 入力のチャンネル数ぶんのサンプル）を 1 つ取り出す。**フレームが丸ごと
    /// 揃っていれば `dst` を埋めて `true`、揃っていなければ何も読まずに
    /// `false` を返すこと。** 途中まで読んで捨てると、以降のサンプルが
    /// 1 つずれて左右が入れ替わったまま戻らない。
    pub fn next_sample(&mut self, read_frame: &mut impl FnMut(&mut [f32]) -> bool) -> Option<f32> {
        // 出力フレームの先頭でだけ入力を読む。途中で読むとチャンネルがずれる
        if self.channel == 0 {
            self.starved = if self.identity {
                // 揃っている組み合わせでは入力フレームがそのまま出力フレームになる
                !read_frame(&mut self.frame)
            } else {
                // 尽きても読み込み済みのフレームと位置は残し、次の呼び出しで続きから読む
                !self.fill_frame(read_frame)
            };
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

    /// 溜めてある入力から、出力フレームを出せるだけ組み立てて `output` へ足す。
    /// 録画スレッドが使う（`crate::recording`）。
    ///
    /// **出力フレームの途中で入力を切らさない。** 次のフレームに要る入力が
    /// 揃っているときだけ組み立て、足りない分は `input` に残して次の呼び出しへ回す。
    /// 録画は数 ms ごとに溜まった分を渡すので、出力フレームの途中で尽きて
    /// 無音を挟むと雑音になる。
    pub fn convert_buffered(&mut self, input: &mut VecDeque<f32>, output: &mut Vec<f32>) {
        // レート比が 0 以下だと入力を読まずに出力し続けてしまう。形は録画側が
        // 0 を弾いてから渡すので来ないが、無限に回らないよう止める
        if !self.identity && self.step <= 0.0 {
            return;
        }
        while input.len() >= self.input_needed_for_next_frame() {
            let mut read = |dst: &mut [f32]| read_frame_from(input, dst);
            for _ in 0..self.out_channels {
                match self.next_sample(&mut read) {
                    Some(sample) => output.push(sample),
                    // 足りることは確かめてあるので来ない
                    None => return,
                }
            }
        }
    }

    /// 次の出力フレームを組み立てるのに読む入力サンプル数。出力フレームの境界で呼ぶ。
    fn input_needed_for_next_frame(&self) -> usize {
        if self.identity {
            return self.in_channels;
        }
        // 読み込み前なら補間の両端のうち足りない分。読み込み後は、位置が `next` を
        // 追い越した分だけ進める（`fill_frame` の while と同じ数）
        let frames = if self.loaded < 2 {
            usize::from(2 - self.loaded)
        } else {
            self.position.floor() as usize
        };
        frames * self.in_channels
    }

    /// 次の出力フレームを組み立てる。入力が足りなければ `false`。
    ///
    /// **足りなくても、読み込み済みのフレームと位置は捨てない。** `read_frame` は
    /// 揃っていないフレームを読まないので、次の呼び出しで同じ所から続けられる。
    fn fill_frame(&mut self, read_frame: &mut impl FnMut(&mut [f32]) -> bool) -> bool {
        if self.loaded < 1 {
            if !read_frame(&mut self.prev) {
                return false;
            }
            self.loaded = 1;
        }
        if self.loaded < 2 {
            if !read_frame(&mut self.next) {
                return false;
            }
            self.loaded = 2;
            self.position = 0.0;
        }

        // 位置が `next` を追い越しているあいだ、入力フレームを進める。
        // ダウンサンプル（step > 1）では 1 回の出力で複数フレーム進む。
        // 次のフレームは要らなくなる `prev` の側へ読み、読めてから入れ替える。
        // 先に入れ替えると、読めなかったときに `prev` を失う
        while self.position >= 1.0 {
            if !read_frame(&mut self.prev) {
                return false;
            }
            std::mem::swap(&mut self.prev, &mut self.next);
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

/// `VecDeque` に溜めた入力からフレームを 1 つ取り出す。`next_sample` へ渡す
/// `read_frame` の約束どおり、揃っていなければ何も読まずに `false` を返す。
fn read_frame_from(input: &mut VecDeque<f32>, dst: &mut [f32]) -> bool {
    let len = dst.len();
    if input.len() < len {
        return false;
    }
    for (slot, sample) in dst.iter_mut().zip(input.drain(..len)) {
        *slot = sample;
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
        let mut source = input.iter().copied().collect::<VecDeque<_>>();
        drain_from(converter, &mut source)
    }

    /// `drain_converter` の、入力を呼び出しをまたいで持ち越す版。リングバッファと
    /// 同じく、読まれなかった（フレームが揃っていなかった）サンプルは `source` に残る。
    fn drain_from(converter: &mut PassthroughConverter, source: &mut VecDeque<f32>) -> Vec<f32> {
        let mut read = |dst: &mut [f32]| read_frame_from(source, dst);
        let mut out = Vec::new();
        let frame_len = converter.out_channels;
        loop {
            let before = out.len();
            for _ in 0..frame_len {
                if let Some(sample) = converter.next_sample(&mut read) {
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

    /// 入力を `chunk` サンプルずつ（フレームの途中で切れる長さでもよい）
    /// 渡しながら出せるだけ出す。リングバッファにフレームの途中までしか
    /// 届いていない瞬間を、出力コールバックが何度も読みに来る状況を真似る。
    fn drain_in_chunks(
        converter: &mut PassthroughConverter,
        input: &[f32],
        chunk: usize,
    ) -> Vec<f32> {
        let mut source = VecDeque::new();
        let mut out = Vec::new();
        for piece in input.chunks(chunk) {
            source.extend(piece.iter().copied());
            out.extend(drain_from(converter, &mut source));
        }
        out
    }

    #[test]
    fn passthrough_converter_same_format_passes_samples_through() {
        // 揃えて開けた場合は補間も再配置も挟まない
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 2);

        assert!(converter.is_identity());
        // 末尾の半端な 1 サンプルはフレームが揃うまで読まない
        assert_eq!(
            drain_converter(&mut converter, &[0.25, -0.5, 1.0]),
            vec![0.25, -0.5]
        );
    }

    #[test]
    fn passthrough_converter_identity_leaves_a_partial_frame_in_the_source() {
        // リングバッファにフレームの途中までしか届いていないとき、
        // 届いた分だけ読むと次のフレームから左右が入れ替わったまま戻らない
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 2);
        let mut source: VecDeque<f32> = [0.25].into_iter().collect();

        assert!(drain_from(&mut converter, &mut source).is_empty());
        assert_eq!(source.len(), 1);

        source.extend([-0.25, 0.5, -0.5]);
        assert_eq!(
            drain_from(&mut converter, &mut source),
            vec![0.25, -0.25, 0.5, -0.5]
        );
    }

    #[test]
    fn passthrough_converter_partial_frame_does_not_shift_channels() {
        // Issue #307 の再現。48kHz 2ch -> 44.1kHz 2ch で、最初の入力フレームの
        // 左だけが届いた状態で読みに来る。修正前は左の 1.0 を読んで捨て、
        // 以降を [-1.0, 0.5] / [-0.5, 0.25] と 1 つずれたフレームとして組んでいた
        let mut converter = PassthroughConverter::new(48000, 2, 44100, 2);
        let mut source: VecDeque<f32> = [1.0].into_iter().collect();

        assert!(drain_from(&mut converter, &mut source).is_empty());
        assert_eq!(source.len(), 1);

        source.extend([-1.0, 0.5, -0.5, 0.25, -0.25]);
        let out = drain_from(&mut converter, &mut source);

        assert_eq!(&out[..2], &[1.0, -1.0]);
        // どのフレームも左右が逆符号の組のまま（入れ替わっていない）
        assert_eq!(out.len(), 4);
        assert!(out[2] > 0.0 && out[2] == -out[3], "{out:?}");
    }

    #[test]
    fn passthrough_converter_input_split_mid_frame_matches_one_pass() {
        // フレームの途中で切れた長さ（7 サンプル = 3.5 フレーム）ずつ届いても、
        // まとめて渡したときと同じ出力になること。尽きたときに読み込み済みの
        // フレームや補間の位置を捨てると、ここが食い違う
        let input: Vec<f32> = (0..882)
            .map(|i| {
                let frame = (i / 2) as f32 / 441.0;
                if i % 2 == 0 {
                    frame
                } else {
                    -frame
                }
            })
            .collect();

        for (in_rate, out_rate) in [(48000, 44100), (44100, 48000), (48000, 24000)] {
            let mut whole = PassthroughConverter::new(in_rate, 2, out_rate, 2);
            let expected = drain_converter(&mut whole, &input);

            let mut chunked = PassthroughConverter::new(in_rate, 2, out_rate, 2);
            let actual = drain_in_chunks(&mut chunked, &input, 7);

            assert_eq!(actual, expected, "{in_rate} -> {out_rate}");
            // 左右が逆符号の組のまま
            for frame in actual.chunks(2) {
                assert_eq!(frame[0], -frame[1], "{in_rate} -> {out_rate}");
            }
        }
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

        // 続きの 2 フレームを渡すと、先頭のチャンネルから組み立てる。
        // 読み込み済みの 1 フレーム目は捨てずに補間の左端として使う
        let out = drain_converter(&mut converter, &[0.5, -0.5, 0.25, -0.25]);

        assert_eq!(out, vec![1.0, -1.0, 0.0, 0.0, 0.5, -0.5, 0.0, 0.0]);
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

    #[test]
    fn convert_buffered_identity_keeps_an_incomplete_frame_for_later() {
        let mut converter = PassthroughConverter::new(48_000, 2, 48_000, 2);
        let mut input: VecDeque<f32> = [0.1, 0.2, 0.3, 0.4, 0.5].into_iter().collect();
        let mut output = Vec::new();

        converter.convert_buffered(&mut input, &mut output);

        // 2ch なので 2 フレームぶんだけ出し、半端な 1 サンプルは次へ回す
        assert_eq!(output, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn convert_buffered_in_small_chunks_matches_one_pass() {
        // 録画は数 ms ごとに溜まった分を渡す。分けて渡しても補間が途切れず、
        // まとめて渡したときと同じ出力になること
        let samples: Vec<f32> = (0..441).map(|i| (i as f32 / 441.0) - 0.5).collect();

        let mut whole = PassthroughConverter::new(44_100, 1, 48_000, 2);
        let mut all_input: VecDeque<f32> = samples.iter().copied().collect();
        let mut expected = Vec::new();
        whole.convert_buffered(&mut all_input, &mut expected);

        let mut chunked = PassthroughConverter::new(44_100, 1, 48_000, 2);
        let mut pending = VecDeque::new();
        let mut actual = Vec::new();
        for chunk in samples.chunks(7) {
            pending.extend(chunk.iter().copied());
            chunked.convert_buffered(&mut pending, &mut actual);
        }

        assert_eq!(actual, expected);
        // 441 入力フレーム（10ms）から、48kHz でおよそ 480 フレーム（2ch）が出る
        assert!((950..=962).contains(&actual.len()), "{}", actual.len());
        assert_eq!(actual.len() % 2, 0);
    }

    #[test]
    fn convert_buffered_spreads_mono_to_both_output_channels() {
        let mut converter = PassthroughConverter::new(48_000, 1, 48_000, 2);
        let mut input: VecDeque<f32> = [0.5, -0.5, 0.25].into_iter().collect();
        let mut output = Vec::new();

        converter.convert_buffered(&mut input, &mut output);

        // モノラルは同じ値を左右へ配る。最後の 1 フレームは補間の右端として読み込み済みで、
        // 次の入力が来たら出る（変換の遅れは 1 入力フレーム）
        assert_eq!(output, vec![0.5, 0.5, -0.5, -0.5]);
        assert!(input.is_empty());
    }
}
