//! RGB で届いた（または RGB に展開した）フレームへ、輝度レンジの伸長と
//! 映像調整（明るさ・コントラスト・彩度）を掛ける表（#472）。
//!
//! YUY2 / 4:2:0 の経路は係数表（`super::color::adjusted_color_matrix`）へ
//! 畳み込んで掛けるが、MJPEG の展開後・RGB24・デコーダ任せのフレームは
//! 既に RGB になっていて係数表を通らない。ここでは同じ式を RGB の上で表す。
//!
//! ```text
//! Y(v)  = y * (v - y_offset) + offset        … 係数表の輝度の項（1024 倍）
//! L     = 0.299 Y(R) + 0.587 Y(G) + 0.114 Y(B) … 彩度の中心（輝度）
//! out_c = (L + saturation * (Y(c) - L)) >> 10
//! ```
//!
//! `y` / `y_offset` / `offset` は、YUY2 の経路が同じ設定で使う係数表
//! （`adjusted_color_matrix`）からそのまま取る。リミテッドの伸長（1192 / 1024）、
//! コントラストと明るさの畳み込み、`>> 10` の切り捨てまで同じなので、**灰色
//! （色差のない画素）は YUY2 の経路と 1 も違わない。** 倍率の意味（-100 で 0 倍、
//! 0 で 1 倍、100 で 2 倍、明るさは RGB へ直に足す）もそちらと同じ。
//!
//! **色空間（BT.601 / BT.709）は RGB では意味を持たない**（Y'CbCr から RGB へ
//! 直す係数の選び方なので、既に RGB のフレームには掛けようがない）。輝度の項の
//! 係数は BT.601 と BT.709 で同じなので、どちらの表から取っても変わらない。
//! 彩度の中心の輝度は、JPEG（JFIF）の Y と同じ BT.601 の重みで取る。
//!
//! 式はチャンネルごとの表（256 要素）だけで書ける。`Y(c)` から輝度の寄与を
//! 引いた分を `gain` に、輝度の寄与を `luma` に入れておけば、1 画素は表引き 6 回と
//! 足し算だけになる。彩度が無調整なら輝度の項は消えるので、`direct`（u8 の表）を
//! 引くだけにする。**表の置き場所（約 4KB）は受け口を作るときに 1 度だけ確保し、
//! 設定が変わったときはその中を書き直す。** フレームコールバックの中で確保しない
//! ため。`FrameSink` に直に持たせないのは、それを抱える DirectShow の列挙型
//! （`directshow/filter.rs` の `StreamState::Video`）が大きくなりすぎるため。

use crate::settings::{ColorRange, ColorSpace};

use super::color::{adjusted_color_matrix, color_matrix_for, VideoAdjustments};

/// 彩度の中心にする輝度の重み（BT.601、JFIF の Y と同じ）
const LUMA_WEIGHTS: [f32; 3] = [0.299, 0.587, 0.114];

/// 掛け方が決まった表。何も効かせない設定（フルレンジ・無調整）では使わない
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tables {
    /// 彩度が無調整のときに引く表（出力をそのまま持つ）
    direct: [u8; 256],
    /// 彩度を掛けるか。偽なら `direct` だけを引く
    mix: bool,
    /// `Y(v)` から、その値が灰色だったときの輝度の寄与（`luma` の 3 つの和）を
    /// 引いたもの（1024 倍）。灰色なら `luma` と足して `Y(v)` ちょうどに戻る
    gain: [i32; 256],
    /// `(1 - saturation) * 重み * (Y(v) - offset)`（1024 倍）。R・G・B の順
    luma: [[i32; 256]; 3],
}

impl Tables {
    /// 中身が空の表。`fill_tables` で書いてから使う
    fn empty() -> Self {
        Self {
            direct: [0; 256],
            mix: false,
            gain: [0; 256],
            luma: [[0; 256]; 3],
        }
    }

    /// 1 画素（R・G・B）に掛ける。`>> 10` は YUY2 の経路と同じ切り捨て
    fn apply_pixel(&self, px: &mut [u8; 3]) {
        if !self.mix {
            for c in px.iter_mut() {
                *c = self.direct[*c as usize];
            }
            return;
        }
        let l = self.luma[0][px[0] as usize]
            + self.luma[1][px[1] as usize]
            + self.luma[2][px[2] as usize];
        for c in px.iter_mut() {
            *c = ((self.gain[*c as usize] + l) >> 10).clamp(0, 255) as u8;
        }
    }
}

/// 何も効かせない設定（フルレンジ・無調整）か。そのときは画素に触らない
fn is_identity(range: ColorRange, adjustments: VideoAdjustments) -> bool {
    range == ColorRange::Full && adjustments.is_neutral()
}

/// レンジと映像調整から表を書く。確保はしない
fn fill_tables(tables: &mut Tables, range: ColorRange, adjustments: VideoAdjustments) {
    // 輝度の項だけを使うので、色空間と解像度はどれでもよい（BT.601 と BT.709 で同じ）
    let matrix = adjusted_color_matrix(
        color_matrix_for(0, 0, ColorSpace::Bt601, range),
        adjustments,
    );
    let keep = 1.0 - (1.0 + adjustments.saturation() as f32 / 100.0);

    tables.mix = adjustments.saturation() != 0;
    for v in 0..256 {
        // 係数表の輝度の項（YUY2 の `cy * Y - bias` と同じ値）
        let scaled = matrix.y * (v as i32 - matrix.y_offset);
        let luma_term = scaled + matrix.offset;
        tables.direct[v] = (luma_term >> 10).clamp(0, 255) as u8;
        let mut gray_luma = 0;
        for (channel, weight) in LUMA_WEIGHTS.iter().enumerate() {
            let part = (keep * weight * scaled as f32).round() as i32;
            tables.luma[channel][v] = part;
            gray_luma += part;
        }
        tables.gain[v] = luma_term - gray_luma;
    }
}

/// RGB の経路で掛ける表と、それを作ったときの設定。
///
/// `FrameSink` が 1 つ持ち、フレームごとに `refresh` で設定と突き合わせる。
/// 比較は列挙 1 つと整数 3 つなので、毎フレームでも安い。
#[derive(Debug)]
pub(super) struct RgbAdjust {
    /// 表を書いたときの設定。まだ書いていなければ `None`
    built_for: Option<(ColorRange, VideoAdjustments)>,
    /// 表を掛けるか。何も効かせない設定なら偽で、画素に触らない
    active: bool,
    /// 表の置き場所。ここで 1 度だけ確保し、以後は中を書き直すだけ
    tables: Box<Tables>,
}

impl Default for RgbAdjust {
    fn default() -> Self {
        Self {
            built_for: None,
            active: false,
            tables: Box::new(Tables::empty()),
        }
    }
}

impl RgbAdjust {
    /// 設定が前回と違えば表を書き直す。同じなら何もしない
    pub(super) fn refresh(&mut self, range: ColorRange, adjustments: VideoAdjustments) {
        let key = (range, adjustments);
        if self.built_for == Some(key) {
            return;
        }
        self.active = !is_identity(range, adjustments);
        if self.active {
            fill_tables(&mut self.tables, range, adjustments);
        }
        self.built_for = Some(key);
    }

    /// RGB24 の画素列（R・G・B の順）に掛ける。何も効かせない設定なら触らない。
    /// 末尾に 3 バイトに満たない余りがあれば、そこには触らない
    pub(super) fn apply(&self, rgb: &mut [u8]) {
        if !self.active {
            return;
        }
        for px in rgb.as_chunks_mut::<3>().0 {
            self.tables.apply_pixel(px);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::convert::yuy2_to_rgb_naive;
    use super::*;

    fn adjusted(range: ColorRange, adjustments: VideoAdjustments, rgb: [u8; 3]) -> [u8; 3] {
        let mut adjust = RgbAdjust::default();
        adjust.refresh(range, adjustments);
        let mut px = rgb;
        adjust.apply(&mut px);
        px
    }

    fn gray(range: ColorRange, adjustments: VideoAdjustments, v: u8) -> u8 {
        adjusted(range, adjustments, [v, v, v])[0]
    }

    const NEUTRAL: VideoAdjustments = VideoAdjustments::NEUTRAL;

    #[test]
    fn full_range_without_adjustments_builds_no_table() {
        assert!(is_identity(ColorRange::Full, NEUTRAL));
        assert!(!is_identity(ColorRange::Limited, NEUTRAL));
        assert!(!is_identity(
            ColorRange::Full,
            VideoAdjustments::new(0, 0, 1)
        ));
        assert_eq!(
            adjusted(ColorRange::Full, NEUTRAL, [1, 128, 254]),
            [1, 128, 254]
        );
    }

    #[test]
    fn limited_range_stretches_16_235_to_0_255() {
        // YUY2 の経路と同じく 1192 / 1024 倍して切り捨てる。235 は 254.9 で 254
        // （YUY2 の白と同じ値）、236 から上は 255 に張り付く
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 0), 0);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 16), 0);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 17), 1);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 128), 130);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 235), 254);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 236), 255);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 255), 255);
    }

    #[test]
    fn brightness_adds_to_every_channel_at_both_ends() {
        let up = VideoAdjustments::new(100, 0, 0);
        assert_eq!(gray(ColorRange::Full, up, 0), 100);
        assert_eq!(gray(ColorRange::Full, up, 154), 254);
        assert_eq!(gray(ColorRange::Full, up, 155), 255);
        let down = VideoAdjustments::new(-100, 0, 0);
        assert_eq!(gray(ColorRange::Full, down, 100), 0);
        assert_eq!(gray(ColorRange::Full, down, 255), 155);
        // リミテッドでは伸ばしてから足す（16 → 0 → 100）
        assert_eq!(gray(ColorRange::Limited, up, 16), 100);
    }

    #[test]
    fn contrast_scales_around_128() {
        let flat = VideoAdjustments::new(0, -100, 0);
        for v in [0, 16, 128, 235, 255] {
            assert_eq!(gray(ColorRange::Full, flat, v), 128);
        }
        let steep = VideoAdjustments::new(0, 100, 0);
        assert_eq!(gray(ColorRange::Full, steep, 0), 0);
        assert_eq!(gray(ColorRange::Full, steep, 64), 0);
        assert_eq!(gray(ColorRange::Full, steep, 128), 128);
        assert_eq!(gray(ColorRange::Full, steep, 191), 254);
        assert_eq!(gray(ColorRange::Full, steep, 192), 255);
    }

    #[test]
    fn saturation_minus_100_turns_colors_into_their_luma() {
        // 赤の輝度は 0.299 * 255 = 76.2
        let mono = VideoAdjustments::new(0, 0, -100);
        assert_eq!(adjusted(ColorRange::Full, mono, [255, 0, 0]), [76, 76, 76]);
        assert_eq!(
            adjusted(ColorRange::Full, mono, [255, 255, 255]),
            [255, 255, 255]
        );
    }

    #[test]
    fn saturation_plus_100_pushes_colors_away_from_luma() {
        // 赤: 2 * 255 - 76.2 = 433.8 → 255、緑と青: 0 - 76.2 → 0
        let vivid = VideoAdjustments::new(0, 0, 100);
        assert_eq!(adjusted(ColorRange::Full, vivid, [255, 0, 0]), [255, 0, 0]);
        // (100, 50, 50): 輝度 64.95。R = 200 - 64.95 = 135.05、G = B = 100 - 64.95 = 35.05
        assert_eq!(
            adjusted(ColorRange::Full, vivid, [100, 50, 50]),
            [135, 35, 35]
        );
    }

    #[test]
    fn saturation_leaves_grays_unchanged() {
        // 灰色は輝度と同じなので、彩度をどちらの端へ振っても動かない
        for saturation in [-100, -50, 50, 100] {
            let adjustments = VideoAdjustments::new(0, 0, saturation);
            for v in [0u8, 1, 16, 128, 235, 254, 255] {
                assert_eq!(
                    gray(ColorRange::Full, adjustments, v),
                    v,
                    "{saturation} {v}"
                );
            }
        }
    }

    #[test]
    fn limited_range_with_all_three_adjustments_at_the_boundaries() {
        // リミテッド・明るさ +20・コントラスト +50・彩度 -100。
        // 輝度の係数は round(1192 * 1.5) = 1788、定数は 1024 * (128 * -0.5 + 20) = -45056
        let mono = VideoAdjustments::new(20, 50, -100);
        assert_eq!(gray(ColorRange::Limited, mono, 0), 0);
        assert_eq!(gray(ColorRange::Limited, mono, 16), 0);
        // (1788 * 112 - 45056) / 1024 = 151.6
        assert_eq!(gray(ColorRange::Limited, mono, 128), 151);
        assert_eq!(gray(ColorRange::Limited, mono, 235), 255);
        assert_eq!(gray(ColorRange::Limited, mono, 255), 255);
        // リミテッドの赤（235, 16, 16）は白黒になり、輝度 0.299 * 382.4 - 44 = 70.3
        assert_eq!(
            adjusted(ColorRange::Limited, mono, [235, 16, 16]),
            [70, 70, 70]
        );

        // リミテッド・明るさ -20・コントラスト -50・彩度 +100。
        // 輝度の係数は 596、定数は 1024 * (128 * 0.5 - 20) = 45056
        let vivid = VideoAdjustments::new(-20, -50, 100);
        // 16 を下回る入力も YUY2 の経路と同じく伸ばしたまま扱う（(596 * -16 + 45056) / 1024 = 34.7）
        assert_eq!(gray(ColorRange::Limited, vivid, 0), 34);
        assert_eq!(gray(ColorRange::Limited, vivid, 16), 44);
        assert_eq!(gray(ColorRange::Limited, vivid, 235), 171);
        assert_eq!(gray(ColorRange::Limited, vivid, 255), 183);
        // 赤は輝度から遠ざかる。R は 260.8 で 255 に張り付き、G と B は 5.9 → 5
        assert_eq!(
            adjusted(ColorRange::Limited, vivid, [235, 16, 16]),
            [255, 5, 5]
        );
    }

    #[test]
    fn grays_match_the_yuy2_path_exactly() {
        // 同じ設定で、YUY2 の経路（係数表 + `yuy2_to_rgb_naive`）と RGB の経路の灰色を
        // 突き合わせる。灰色（Cb = Cr = 128）は色差の項が消えるので、RGB の経路が
        // 係数表から取った輝度の項と同じ値になり、1 も違わないはず。
        //
        // 有彩色は突き合わせない。YUY2 の経路は Y'CbCr の色差に彩度を掛け、色空間
        // （BT.601 / BT.709）で輝度の重みも色差の係数も変わる。RGB の経路は届いた RGB
        // から BT.601 の重みで輝度を取り直すので、同じ色を同じ値にする前提が無い
        // （差は色と色空間しだいで、許容差を決める根拠が無い）
        let cases = [
            (0, 0, 0),
            (100, 0, 0),
            (-100, 0, 0),
            (0, 100, 0),
            (0, -100, 0),
            (20, 50, -100),
            (-20, -50, 100),
            (50, -30, 40),
            (-100, 100, -100),
        ];
        for (brightness, contrast, saturation) in cases {
            let adjustments = VideoAdjustments::new(brightness, contrast, saturation);
            for range in [ColorRange::Limited, ColorRange::Full] {
                for space in [ColorSpace::Bt601, ColorSpace::Bt709] {
                    let matrix =
                        adjusted_color_matrix(color_matrix_for(2, 1, space, range), adjustments);
                    for y in [0u8, 1, 15, 16, 17, 64, 128, 200, 234, 235, 236, 254, 255] {
                        let mut yuy2 = Vec::new();
                        yuy2_to_rgb_naive(2, 1, &[y, 128, y, 128], &matrix, &mut yuy2);
                        let rgb = adjusted(range, adjustments, [y, y, y]);
                        assert_eq!(
                            &yuy2[..3],
                            &rgb,
                            "Y={y} {range:?} {space:?} 調整 {brightness} {contrast} {saturation}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn refresh_rebuilds_only_when_the_settings_change() {
        let mut adjust = RgbAdjust::default();
        adjust.refresh(ColorRange::Limited, NEUTRAL);
        let mut px = [16, 16, 16];
        adjust.apply(&mut px);
        assert_eq!(px, [0, 0, 0]);

        // フルへ戻せば画素に触らない
        adjust.refresh(ColorRange::Full, NEUTRAL);
        let mut px = [16, 16, 16];
        adjust.apply(&mut px);
        assert_eq!(px, [16, 16, 16]);
        assert_eq!(adjust.built_for, Some((ColorRange::Full, NEUTRAL)));
    }

    #[test]
    fn apply_leaves_a_trailing_partial_pixel_alone() {
        let mut adjust = RgbAdjust::default();
        adjust.refresh(ColorRange::Limited, NEUTRAL);
        let mut data = [235, 235, 235, 16];
        adjust.apply(&mut data);
        assert_eq!(data, [254, 254, 254, 16]);
    }
}
