//! RGB で届いた（または RGB に展開した）フレームへ、輝度レンジの伸長と
//! 映像調整（明るさ・コントラスト・彩度）を掛ける表（#472）。
//!
//! YUY2 / 4:2:0 の経路は係数表（`super::color::adjusted_color_matrix`）へ
//! 畳み込んで掛けるが、MJPEG の展開後・RGB24・デコーダ任せのフレームは
//! 既に RGB になっていて係数表を通らない。ここでは同じ式を RGB の上で表す。
//!
//! ```text
//! s(v)  = リミテッドなら (v - 16) * 255 / 219、フルなら v   … レンジの伸長
//! L     = 0.299 s(R) + 0.587 s(G) + 0.114 s(B)            … 彩度の中心（輝度）
//! out_c = contrast * (L + saturation * (s(c) - L) - 128) + 128 + brightness
//! ```
//!
//! 倍率の意味（-100 で 0 倍、0 で 1 倍、100 で 2 倍、明るさは RGB へ直に足す）は
//! 係数表の側と揃えてある。**色空間（BT.601 / BT.709）は RGB では意味を持たない**
//! （Y'CbCr から RGB へ直す係数の選び方なので、既に RGB のフレームには掛けようが
//! ない）。彩度の中心の輝度は、JPEG（JFIF）の Y と同じ BT.601 の重みで取る。
//!
//! 式はチャンネルごとの表（256 要素）だけで書ける。`s(c)` に掛かる分と定数を
//! `gain` に、輝度に掛かる分を `luma_*` に入れておけば、1 画素は表引き 6 回と
//! 足し算だけになる。彩度が無調整なら輝度の項は消えるので、`direct`（u8 の表）を
//! 引くだけにする。**表の置き場所（約 4KB）は受け口を作るときに 1 度だけ確保し、
//! 設定が変わったときはその中を書き直す。** フレームコールバックの中で確保しない
//! ため。`FrameSink` に直に持たせないのは、それを抱える DirectShow の列挙型
//! （`directshow/filter.rs` の `StreamState::Video`）が大きくなりすぎるため。

use crate::settings::ColorRange;

use super::color::VideoAdjustments;

/// 表を 1024 倍の固定小数点で持つ。係数表（`ColorMatrix`）と同じ倍率
const FIXED_POINT_SCALE: f32 = 1024.0;

/// コントラストと彩度の中心。係数表の側（`ADJUSTMENT_PIVOT`）と同じ 128
const PIVOT: f32 = 128.0;

/// 彩度の中心にする輝度の重み（BT.601、JFIF の Y と同じ）
const LUMA_WEIGHTS: [f32; 3] = [0.299, 0.587, 0.114];

/// 掛け方が決まった表。何も効かせない設定（フルレンジ・無調整）では使わない
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tables {
    /// 彩度が無調整のときに引く表（出力をそのまま持つ）
    direct: [u8; 256],
    /// 彩度を掛けるか。偽なら `direct` だけを引く
    mix: bool,
    /// `contrast * saturation * s(v)` と定数項（1024 倍）
    gain: [i32; 256],
    /// `contrast * (1 - saturation) * 重み * s(v)`（1024 倍）。R・G・B の順
    luma: [[i32; 256]; 3],
}

impl Tables {
    /// 中身が空の表。`fill` で書いてから使う
    fn empty() -> Self {
        Self {
            direct: [0; 256],
            mix: false,
            gain: [0; 256],
            luma: [[0; 256]; 3],
        }
    }

    /// 1 画素（R・G・B）に掛ける
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
            *c = ((self.gain[*c as usize] + l + 512) >> 10).clamp(0, 255) as u8;
        }
    }
}

/// 何も効かせない設定（フルレンジ・無調整）か。そのときは画素に触らない
fn is_identity(range: ColorRange, adjustments: VideoAdjustments) -> bool {
    range == ColorRange::Full && adjustments.is_neutral()
}

/// レンジと映像調整から表を書く。確保はしない
fn fill_tables(tables: &mut Tables, range: ColorRange, adjustments: VideoAdjustments) {
    let contrast = 1.0 + adjustments.contrast() as f32 / 100.0;
    let saturation = 1.0 + adjustments.saturation() as f32 / 100.0;
    let offset = PIVOT * (1.0 - contrast) + adjustments.brightness() as f32;
    let stretch = |v: usize| match range {
        ColorRange::Limited => (v as f32 - 16.0) * 255.0 / 219.0,
        ColorRange::Full => v as f32,
    };

    tables.mix = adjustments.saturation() != 0;
    for v in 0..256 {
        let s = stretch(v);
        tables.direct[v] = (contrast * s + offset).round().clamp(0.0, 255.0) as u8;
        tables.gain[v] = (FIXED_POINT_SCALE * (contrast * saturation * s + offset)).round() as i32;
        for (channel, weight) in LUMA_WEIGHTS.iter().enumerate() {
            tables.luma[channel][v] =
                (FIXED_POINT_SCALE * contrast * (1.0 - saturation) * weight * s).round() as i32;
        }
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
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 0), 0);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 16), 0);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 17), 1);
        // (128 - 16) * 255 / 219 = 130.4
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 128), 130);
        assert_eq!(gray(ColorRange::Limited, NEUTRAL, 235), 255);
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
        assert_eq!(data, [255, 255, 255, 16]);
    }
}
