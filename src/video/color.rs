//! YCbCr → RGB の係数表と、その選び方・調整の畳み込み。
//!
//! 画素を実際に変換するのは `super::convert`。ここが持つのは「どの係数で
//! 変換するか」を決める側で、UI の設定（色空間・レンジ・明るさ・コントラスト・
//! 彩度）をフレームコールバックへ渡す箱（`SharedColorConversion`）も含む。

use log::info;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use crate::settings::{ColorRange, ColorSpace, MAX_VIDEO_ADJUSTMENT, MIN_VIDEO_ADJUSTMENT};

/// YCbCr -> RGB 変換の係数。
///
/// 入力の信号を フルレンジ RGB（0〜255）へ展開する行列を、1024 倍の
/// 固定小数点（`>> 10` で戻す）で保持する。
///
/// 各係数の導出は以下。Kr / Kb は色空間ごとの輝度の重み、Kg = 1 - Kr - Kb。
/// `sy` / `sc` は入力レンジをフルレンジへ伸ばすスケール。
///
/// ```text
/// リミテッドレンジ (Y 16〜235、Cb/Cr 16〜240): sy = 255/219、sc = 255/224
/// フルレンジ       (Y 0〜255、Cb/Cr 0〜255)  : sy = 1、      sc = 1
///
/// y   = sy
/// r_v = sc * 2 * (1 - Kr)
/// g_u = sc * 2 * Kb * (1 - Kb) / Kg
/// g_v = sc * 2 * Kr * (1 - Kr) / Kg
/// b_u = sc * 2 * (1 - Kb)
/// ```
///
/// `g_u` と `g_v` は減算に使うため、符号を除いた大きさを持つ。
/// Cb/Cr から引くオフセットはどちらのレンジでも 128 なので、表には持たせていない。
///
/// 明るさ・コントラスト・彩度の調整も、この表へ畳み込んで表現する
/// （`adjusted_color_matrix`）。変換式そのものは変わらないため、
/// 調整を入れても 1 画素あたりの演算は増えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ColorMatrix {
    /// ログに出す名前。どの表で変換したかを後から追えるようにする
    pub(super) name: &'static str,
    /// Y から引くオフセット。リミテッドは 16、フルは 0
    pub(super) y_offset: i32,
    /// Y - y_offset に掛ける係数
    pub(super) y: i32,
    /// R への Cr - 128 の寄与
    pub(super) r_v: i32,
    /// G から引く Cb - 128 の寄与
    pub(super) g_u: i32,
    /// G から引く Cr - 128 の寄与
    pub(super) g_v: i32,
    /// B への Cb - 128 の寄与
    pub(super) b_u: i32,
    /// R/G/B すべてに加える定数（係数と同じく 1024 倍の固定小数点）。
    ///
    /// 明るさとコントラストの調整がここに集約される。無調整では 0 で、
    /// そのとき変換結果は調整を入れる前と完全に一致する
    pub(super) offset: i32,
}

/// BT.601 リミテッドレンジ（SD 向け。Kr = 0.299、Kb = 0.114）。
///
/// 1.164 / 1.596 / 0.392 / 0.813 / 2.017 に相当する。
/// `g_v` だけは上式の丸め（832）ではなく 833 を使っている。古くから出回っている
/// 整数版の定数をそのまま引き継いだもので、1/1024 の差しかないため変えていない。
pub(super) static BT601: ColorMatrix = ColorMatrix {
    name: "BT.601 リミテッド",
    y_offset: 16,
    y: 1192,
    r_v: 1634,
    g_u: 401,
    g_v: 833,
    b_u: 2066,
    offset: 0,
};

/// BT.709 リミテッドレンジ（HD 向け。Kr = 0.2126、Kb = 0.0722）。
///
/// 上式に代入すると 1.16438 / 1.79274 / 0.21325 / 0.53291 / 2.11240 となり、
/// 1024 倍して四捨五入すると 1192 / 1836 / 218 / 546 / 2163 になる。
pub(super) static BT709: ColorMatrix = ColorMatrix {
    name: "BT.709 リミテッド",
    y_offset: 16,
    y: 1192,
    r_v: 1836,
    g_u: 218,
    g_v: 546,
    b_u: 2163,
    offset: 0,
};

/// BT.601 フルレンジ。
///
/// スケールを 1 にして Kr = 0.299、Kb = 0.114 を代入すると
/// 1.0 / 1.402 / 0.34414 / 0.71414 / 1.772 となり、1024 倍して四捨五入すると
/// 1024 / 1436 / 352 / 731 / 1815 になる。
pub(super) static BT601_FULL: ColorMatrix = ColorMatrix {
    name: "BT.601 フル",
    y_offset: 0,
    y: 1024,
    r_v: 1436,
    g_u: 352,
    g_v: 731,
    b_u: 1815,
    offset: 0,
};

/// BT.709 フルレンジ。
///
/// 同じくスケールを 1 にして Kr = 0.2126、Kb = 0.0722 を代入すると
/// 1.0 / 1.5748 / 0.18732 / 0.46812 / 1.8556 となり、1024 倍して四捨五入すると
/// 1024 / 1613 / 192 / 479 / 1900 になる。
pub(super) static BT709_FULL: ColorMatrix = ColorMatrix {
    name: "BT.709 フル",
    y_offset: 0,
    y: 1024,
    r_v: 1613,
    g_u: 192,
    g_v: 479,
    b_u: 1900,
    offset: 0,
};

/// HD とみなす境界。これ以上なら BT.709 を使う。
///
/// HD の放送規格（ITU-R BT.709）は 1280x720 以上を対象としており、
/// それ未満の SD 解像度は BT.601 で符号化される。キャプチャーボードは
/// 入力信号の色空間を通知してこないため、解像度から推定するしかない。
const HD_MIN_WIDTH: usize = 1280;
const HD_MIN_HEIGHT: usize = 720;

/// 設定とフレームの解像度から係数の表を選ぶ。
///
/// `space` が `Auto` のときだけ解像度から推定する。幅と高さのどちらかが
/// HD の境界に達していれば BT.709 とみなす。1440x1080 のようにアスペクト比が
/// 1:1 でない HD 形式があるため、片方だけを見ると取りこぼす。
///
/// レンジは推定できない。フルレンジで出すかどうかはデバイス側の設定次第で、
/// 信号からも解像度からも判別できないため、設定の値をそのまま使う。
pub(super) fn color_matrix_for(
    width: usize,
    height: usize,
    space: ColorSpace,
    range: ColorRange,
) -> &'static ColorMatrix {
    let is_bt709 = match space {
        ColorSpace::Auto => width >= HD_MIN_WIDTH || height >= HD_MIN_HEIGHT,
        ColorSpace::Bt601 => false,
        ColorSpace::Bt709 => true,
    };

    match (is_bt709, range) {
        (false, ColorRange::Limited) => &BT601,
        (false, ColorRange::Full) => &BT601_FULL,
        (true, ColorRange::Limited) => &BT709,
        (true, ColorRange::Full) => &BT709_FULL,
    }
}

/// 映像の明るさ・コントラスト・彩度の調整値。
///
/// いずれも -100〜100 で 0 が無調整。3 つで範囲を揃えてあるのは、
/// スライダーの中央が常に無調整になり、「リセット」が 3 つとも 0 を
/// 書くだけで済むため。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoAdjustments {
    brightness: i32,
    contrast: i32,
    saturation: i32,
}

impl VideoAdjustments {
    /// 無調整。係数表がそのまま使われる
    pub const NEUTRAL: Self = Self {
        brightness: 0,
        contrast: 0,
        saturation: 0,
    };

    /// 設定の値から作る。範囲外の値は丸める。
    ///
    /// 設定の読み込み側でも丸めているが、ここでも丸めておく。係数が
    /// 際限なく大きくなると `i32` の乗算が溢れうるため、変換に使う値は
    /// 入口を問わず範囲内であることをここで保証する
    pub fn new(brightness: i32, contrast: i32, saturation: i32) -> Self {
        Self {
            brightness: brightness.clamp(MIN_VIDEO_ADJUSTMENT, MAX_VIDEO_ADJUSTMENT),
            contrast: contrast.clamp(MIN_VIDEO_ADJUSTMENT, MAX_VIDEO_ADJUSTMENT),
            saturation: saturation.clamp(MIN_VIDEO_ADJUSTMENT, MAX_VIDEO_ADJUSTMENT),
        }
    }

    /// 3 つとも 0 か。ログに「調整あり」と出すかの判定に使う
    fn is_neutral(self) -> bool {
        self == Self::NEUTRAL
    }
}

/// RGB でのコントラストと彩度の中心。
///
/// コントラストは「中間グレーを動かさずに振幅を伸び縮みさせる」操作なので、
/// 0〜255 の中央である 128 を基準にする。
const ADJUSTMENT_PIVOT: f32 = 128.0;

/// 係数を 1024 倍で持つときの倍率。`>> 10` で戻すのと対になる
const FIXED_POINT_SCALE: f32 = 1024.0;

/// 係数表へ明るさ・コントラスト・彩度を畳み込む。
///
/// Y'CbCr → RGB はアフィン変換なので、出力側での
///
/// ```text
/// out = contrast * (in - 128) + 128 + brightness
/// ```
///
/// という調整は、係数と定数項の付け替えだけで表現できる。`in` は
/// 輝度の項（`y * (Y - y_offset)`）と色差の項の和なので、
///
/// - コントラスト … 輝度と色差の両方の係数に掛かる
/// - 彩度 … 色差の係数にだけ掛かる
/// - 明るさとコントラストの分の定数 … `offset` に集約される
///
/// となる。**変換関数の形は変わらないので、調整の有無で 1 画素あたりの
/// 演算数は変わらない。** この関数自体はフレームごとに呼ばれるが、係数の
/// 掛け直しは 6 回で、画素数（1080p なら 200 万）に比べれば無視できる。
/// 前回の結果を覚えて使い回す形にしていないのは、覚える対象が解像度・
/// 色空間・レンジ・調整値の 4 つになり、毎フレームの比較のほうが
/// 掛け算より安いとは言えないため。
///
/// 倍率は -100 で 0 倍、0 で 1 倍、100 で 2 倍。彩度 -100 は色差を
/// 完全に落として白黒に、コントラスト -100 は中間グレー一色になる。
/// 明るさは RGB へ直に足す値で、-100〜100 をそのまま使う。
pub(super) fn adjusted_color_matrix(
    base: &ColorMatrix,
    adjustments: VideoAdjustments,
) -> ColorMatrix {
    if adjustments.is_neutral() {
        // 無調整なら丸め誤差の入る余地も残さず、表をそのまま返す
        return *base;
    }

    let contrast = 1.0 + adjustments.contrast as f32 / 100.0;
    let saturation = 1.0 + adjustments.saturation as f32 / 100.0;
    // 色差にはコントラストと彩度の両方が掛かる
    let chroma = contrast * saturation;
    let brightness = adjustments.brightness as f32;

    let scale = |coefficient: i32, gain: f32| (coefficient as f32 * gain).round() as i32;

    ColorMatrix {
        name: base.name,
        y_offset: base.y_offset,
        y: scale(base.y, contrast),
        r_v: scale(base.r_v, chroma),
        g_u: scale(base.g_u, chroma),
        g_v: scale(base.g_v, chroma),
        b_u: scale(base.b_u, chroma),
        offset: (FIXED_POINT_SCALE * (ADJUSTMENT_PIVOT * (1.0 - contrast) + brightness)).round()
            as i32,
    }
}

/// 色空間とレンジの設定を、UI スレッドとフレームコールバックスレッドで共有する箱。
///
/// フレームコールバックは 1080p60 なら毎秒 60 回呼ばれる。ここで `Mutex` を
/// 取ると、設定を読むためだけに毎フレームのロックが増える。値は 2 つの
/// 列挙だけなので、`AtomicU8` に詰めて読み書きする。
///
/// 色空間とレンジを別々の Atomic にしてあるため、片方だけ書き換えた瞬間に
/// コールバックが読むと新旧が混ざりうる。混ざっても有効な組み合わせに
/// しかならず、次のフレームで揃うので、まとめて更新する仕組みは持たせていない。
/// 明るさ・コントラスト・彩度も同じ扱いで、1 フレームだけ途中の値が
/// 見えることがあるが、範囲内の値であることに変わりはない。
#[derive(Debug)]
pub struct SharedColorConversion {
    space: AtomicU8,
    range: AtomicU8,
    /// 映像調整。いずれも -100〜100 で、値の意味は `VideoAdjustments` と同じ
    brightness: AtomicI32,
    contrast: AtomicI32,
    saturation: AtomicI32,
}

/// `ColorSpace` を `AtomicU8` へ詰めるときの値。
/// 数値そのものは設定ファイルにもログにも出ないので、順序に意味はない。
const SPACE_AUTO: u8 = 0;
const SPACE_BT601: u8 = 1;
const SPACE_BT709: u8 = 2;

/// `ColorRange` を `AtomicU8` へ詰めるときの値。
const RANGE_LIMITED: u8 = 0;
const RANGE_FULL: u8 = 1;

impl Default for SharedColorConversion {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedColorConversion {
    pub fn new() -> Self {
        Self {
            space: AtomicU8::new(SPACE_AUTO),
            range: AtomicU8::new(RANGE_LIMITED),
            brightness: AtomicI32::new(0),
            contrast: AtomicI32::new(0),
            saturation: AtomicI32::new(0),
        }
    }

    /// 色変換に使う色空間とレンジを差し替える。
    ///
    /// キャプチャ中でも次のフレームから効く。デバイスを開き直さないのは、
    /// 開き直しがリトライの待ちを含めて秒単位かかり、色を見比べながら
    /// 設定を選ぶ操作に耐えないため。**デバイスを触らないので、UI スレッドから
    /// 直接呼んでよい。**
    pub fn set_color_conversion(&self, space: ColorSpace, range: ColorRange) {
        self.store(space, range);
        info!(
            "色変換の設定を反映した（色空間: {:?}、レンジ: {:?}）",
            space, range
        );
    }

    /// 明るさ・コントラスト・彩度を差し替える。
    ///
    /// 色空間やレンジと同じく、キャプチャ中でも次のフレームから効く。
    /// 調整は係数表へ畳み込まれるので、変換そのものは重くならない。
    pub fn set_video_adjustments(&self, adjustments: VideoAdjustments) {
        self.store_adjustments(adjustments);
        info!(
            "映像調整を反映した（明るさ: {}、コントラスト: {}、彩度: {}）",
            adjustments.brightness, adjustments.contrast, adjustments.saturation
        );
    }

    fn store(&self, space: ColorSpace, range: ColorRange) {
        let space = match space {
            ColorSpace::Auto => SPACE_AUTO,
            ColorSpace::Bt601 => SPACE_BT601,
            ColorSpace::Bt709 => SPACE_BT709,
        };
        let range = match range {
            ColorRange::Limited => RANGE_LIMITED,
            ColorRange::Full => RANGE_FULL,
        };
        self.space.store(space, Ordering::Relaxed);
        self.range.store(range, Ordering::Relaxed);
    }

    pub(super) fn load(&self) -> (ColorSpace, ColorRange) {
        // store 側が詰めた値しか入らないので、既定へ倒す分岐は保険
        let space = match self.space.load(Ordering::Relaxed) {
            SPACE_BT601 => ColorSpace::Bt601,
            SPACE_BT709 => ColorSpace::Bt709,
            _ => ColorSpace::Auto,
        };
        let range = match self.range.load(Ordering::Relaxed) {
            RANGE_FULL => ColorRange::Full,
            _ => ColorRange::Limited,
        };
        (space, range)
    }

    fn store_adjustments(&self, adjustments: VideoAdjustments) {
        self.brightness
            .store(adjustments.brightness, Ordering::Relaxed);
        self.contrast.store(adjustments.contrast, Ordering::Relaxed);
        self.saturation
            .store(adjustments.saturation, Ordering::Relaxed);
    }

    pub(super) fn load_adjustments(&self) -> VideoAdjustments {
        // store 側が範囲内の値しか入れないので、new の丸めは保険
        VideoAdjustments::new(
            self.brightness.load(Ordering::Relaxed),
            self.contrast.load(Ordering::Relaxed),
            self.saturation.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_matrix_for_sd_resolution_returns_bt601() {
        // 640x480 (VGA)、720x480 (NTSC)、720x576 (PAL) はいずれも SD
        assert_eq!(
            color_matrix_for(640, 480, ColorSpace::Auto, ColorRange::Limited),
            &BT601
        );
        assert_eq!(
            color_matrix_for(720, 480, ColorSpace::Auto, ColorRange::Limited),
            &BT601
        );
        assert_eq!(
            color_matrix_for(720, 576, ColorSpace::Auto, ColorRange::Limited),
            &BT601
        );
    }

    #[test]
    fn color_matrix_for_hd_resolution_returns_bt709() {
        assert_eq!(
            color_matrix_for(1280, 720, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
        assert_eq!(
            color_matrix_for(1920, 1080, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
        assert_eq!(
            color_matrix_for(3840, 2160, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
    }

    #[test]
    fn color_matrix_for_just_below_hd_threshold_returns_bt601() {
        // 幅・高さの両方が境界に届かない場合だけ BT.601
        assert_eq!(
            color_matrix_for(1279, 719, ColorSpace::Auto, ColorRange::Limited),
            &BT601
        );
    }

    #[test]
    fn color_matrix_for_either_dimension_at_threshold_returns_bt709() {
        // 1440x1080 のようにアスペクト比が 1:1 でない HD 形式を取りこぼさないため、
        // 幅と高さのどちらかが境界に達していれば BT.709 とみなす
        assert_eq!(
            color_matrix_for(1280, 719, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
        assert_eq!(
            color_matrix_for(1279, 720, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
        assert_eq!(
            color_matrix_for(1440, 1080, ColorSpace::Auto, ColorRange::Limited),
            &BT709
        );
    }

    #[test]
    fn color_matrix_for_zero_size_returns_bt601() {
        // 解像度が取れない異常系。どちらかに倒すしかないので SD 側へ倒す
        assert_eq!(
            color_matrix_for(0, 0, ColorSpace::Auto, ColorRange::Limited),
            &BT601
        );
    }

    #[test]
    fn color_matrix_for_explicit_space_ignores_resolution() {
        // SD で BT.709、HD で BT.601 を出すデバイスのために手で固定できる。
        // 固定したら解像度からの推定は働かない
        assert_eq!(
            color_matrix_for(640, 480, ColorSpace::Bt709, ColorRange::Limited),
            &BT709
        );
        assert_eq!(
            color_matrix_for(1920, 1080, ColorSpace::Bt601, ColorRange::Limited),
            &BT601
        );
    }

    #[test]
    fn color_matrix_for_full_range_selects_full_range_table() {
        // レンジは解像度からも色空間の指定からも独立して効く
        assert_eq!(
            color_matrix_for(640, 480, ColorSpace::Auto, ColorRange::Full),
            &BT601_FULL
        );
        assert_eq!(
            color_matrix_for(1920, 1080, ColorSpace::Auto, ColorRange::Full),
            &BT709_FULL
        );
        assert_eq!(
            color_matrix_for(640, 480, ColorSpace::Bt709, ColorRange::Full),
            &BT709_FULL
        );
    }

    #[test]
    fn shared_color_conversion_round_trips_every_combination() {
        // フレームコールバックが読む側。詰め直しで色空間とレンジが
        // 入れ替わらないことを全組み合わせで確かめる
        let shared = SharedColorConversion::new();
        assert_eq!(shared.load(), (ColorSpace::Auto, ColorRange::Limited));

        for space in ColorSpace::ALL {
            for range in ColorRange::ALL {
                shared.store(space, range);
                assert_eq!(shared.load(), (space, range));
            }
        }
    }

    // ---- 映像調整（明るさ / コントラスト / 彩度）----
    //
    // 調整値から係数表への変換と、その表を通した変換結果を分けて確かめる。
    // 期待する係数は手計算した値をベタ書きする

    #[test]
    fn adjusted_color_matrix_neutral_returns_the_base_table() {
        // 3 つとも 0 なら、どの表も 1 ビットも変わらないこと。
        // 既存ユーザーの見え方を変えないための前提
        for base in [&BT601, &BT709, &BT601_FULL, &BT709_FULL] {
            assert_eq!(
                adjusted_color_matrix(base, VideoAdjustments::NEUTRAL),
                *base
            );
        }
    }

    #[test]
    fn adjusted_color_matrix_brightness_only_moves_the_offset() {
        // 明るさは RGB へ直に足す値なので、係数は動かず offset だけが変わる。
        // offset は 1024 倍の固定小数点なので 50 * 1024 = 51200
        let adjusted = adjusted_color_matrix(&BT601, VideoAdjustments::new(50, 0, 0));
        assert_eq!(adjusted.y, BT601.y);
        assert_eq!(adjusted.r_v, BT601.r_v);
        assert_eq!(adjusted.g_u, BT601.g_u);
        assert_eq!(adjusted.g_v, BT601.g_v);
        assert_eq!(adjusted.b_u, BT601.b_u);
        assert_eq!(adjusted.y_offset, BT601.y_offset);
        assert_eq!(adjusted.offset, 51200);
    }

    #[test]
    fn adjusted_color_matrix_contrast_scales_every_coefficient() {
        // コントラスト +100 は 2 倍。輝度も色差も 2 倍になり、
        // 中間グレー（128）を動かさないための定数 128 * (1 - 2) * 1024 が入る
        let adjusted = adjusted_color_matrix(&BT601, VideoAdjustments::new(0, 100, 0));
        assert_eq!(adjusted.y, 2384);
        assert_eq!(adjusted.r_v, 3268);
        assert_eq!(adjusted.g_u, 802);
        assert_eq!(adjusted.g_v, 1666);
        assert_eq!(adjusted.b_u, 4132);
        assert_eq!(adjusted.offset, -131072);
    }

    #[test]
    fn adjusted_color_matrix_saturation_scales_only_chroma() {
        // 彩度 +100 は色差だけを 2 倍にする。輝度の係数と offset は動かない
        let adjusted = adjusted_color_matrix(&BT601, VideoAdjustments::new(0, 0, 100));
        assert_eq!(adjusted.y, BT601.y);
        assert_eq!(adjusted.r_v, 3268);
        assert_eq!(adjusted.g_u, 802);
        assert_eq!(adjusted.g_v, 1666);
        assert_eq!(adjusted.b_u, 4132);
        assert_eq!(adjusted.offset, 0);
    }

    #[test]
    fn adjusted_color_matrix_minimum_saturation_zeroes_chroma() {
        // 彩度 -100 は色差を完全に落とす。白黒になる
        let adjusted = adjusted_color_matrix(&BT709, VideoAdjustments::new(0, 0, -100));
        assert_eq!(adjusted.y, BT709.y);
        assert_eq!(adjusted.r_v, 0);
        assert_eq!(adjusted.g_u, 0);
        assert_eq!(adjusted.g_v, 0);
        assert_eq!(adjusted.b_u, 0);
        assert_eq!(adjusted.offset, 0);
    }

    #[test]
    fn adjusted_color_matrix_minimum_contrast_zeroes_luma_and_chroma() {
        // コントラスト -100 は振幅を 0 にする。offset だけが残り、
        // 中間グレー（128 * 1024 = 131072）一色になる
        let adjusted = adjusted_color_matrix(&BT601, VideoAdjustments::new(0, -100, 0));
        assert_eq!(adjusted.y, 0);
        assert_eq!(adjusted.r_v, 0);
        assert_eq!(adjusted.b_u, 0);
        assert_eq!(adjusted.offset, 131072);
    }

    #[test]
    fn video_adjustments_new_clamps_out_of_range_values() {
        // 設定ファイル側でも丸めているが、変換へ渡る値はここでも保証する
        let adjustments = VideoAdjustments::new(1000, -1000, i32::MAX);
        assert_eq!(adjustments.brightness, MAX_VIDEO_ADJUSTMENT);
        assert_eq!(adjustments.contrast, MIN_VIDEO_ADJUSTMENT);
        assert_eq!(adjustments.saturation, MAX_VIDEO_ADJUSTMENT);
    }

    #[test]
    fn video_adjustments_default_is_neutral() {
        assert_eq!(VideoAdjustments::default(), VideoAdjustments::NEUTRAL);
        assert!(VideoAdjustments::NEUTRAL.is_neutral());
        assert!(!VideoAdjustments::new(1, 0, 0).is_neutral());
    }

    #[test]
    fn shared_color_conversion_round_trips_adjustments() {
        // フレームコールバックが読む側。3 つの値が入れ替わらないこと
        let shared = SharedColorConversion::new();
        assert_eq!(shared.load_adjustments(), VideoAdjustments::NEUTRAL);

        let adjustments = VideoAdjustments::new(10, -20, 30);
        shared.store_adjustments(adjustments);
        assert_eq!(shared.load_adjustments(), adjustments);
        // 色空間とレンジは巻き添えにならない
        assert_eq!(shared.load(), (ColorSpace::Auto, ColorRange::Limited));
    }
}
