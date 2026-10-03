//! RGB → NV12 の画素変換。録画スレッドが Sink Writer へ渡す前に行う純粋関数。
//!
//! 係数は `video::color` の表（YCbCr → RGB）の逆向き。書き出す NV12 は常に標準の組
//! （HD は BT.709、SD は BT.601、どちらもリミテッドレンジ）にして、同じ印を
//! メディアタイプに付ける（`writer.rs`）。画面で色空間やレンジを直していても、
//! 直した結果の RGB から作るのでファイルは画面と同じ見た目になる
//! （`docs/design/recording.md` の「映像は変換後の RGB を `Arc` のまま渡す」）。

/// RGB → YCbCr の係数。65536 倍の固定小数点（`>> 16` で戻す）。
///
/// `Kr` / `Kb` は色空間ごとの輝度の重み、`Kg = 1 - Kr - Kb`。リミテッドレンジへ
/// 縮めるスケールは Y が 219/255、Cb / Cr が 224/255。
///
/// ```text
/// Y  = 16  + 219/255 * (Kr R + Kg G + Kb B)
/// Cb = 128 + 224/255 * (B - Y') / (2 (1 - Kb))
/// Cr = 128 + 224/255 * (R - Y') / (2 (1 - Kr))      Y' = Kr R + Kg G + Kb B
/// ```
///
/// Cb / Cr の行は和が 0 になるように丸めてある（灰色が 128 から動かない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RgbToYuv {
    y: [i32; 3],
    cb: [i32; 3],
    cr: [i32; 3],
}

/// BT.709 リミテッド（HD）。Kr = 0.2126、Kb = 0.0722。
const BT709: RgbToYuv = RgbToYuv {
    y: [11966, 40254, 4064],
    cb: [-6596, -22188, 28784],
    cr: [28784, -26145, -2639],
};

/// BT.601 リミテッド（SD）。Kr = 0.299、Kb = 0.114。
const BT601: RgbToYuv = RgbToYuv {
    y: [16829, 33039, 6416],
    cb: [-9714, -19070, 28784],
    cr: [28784, -24103, -4681],
};

/// 書き出す NV12 の色空間。メディアタイプの印（`writer.rs`）と係数表の両方をここから引く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nv12Matrix {
    Bt709,
    Bt601,
}

impl Nv12Matrix {
    /// 解像度から選ぶ。境界は表示の「自動」と同じ（`video::is_hd_resolution`）。
    pub(super) fn for_size(width: usize, height: usize) -> Self {
        if crate::video::is_hd_resolution(width, height) {
            Nv12Matrix::Bt709
        } else {
            Nv12Matrix::Bt601
        }
    }

    fn coefficients(self) -> &'static RgbToYuv {
        match self {
            Nv12Matrix::Bt709 => &BT709,
            Nv12Matrix::Bt601 => &BT601,
        }
    }
}

/// NV12 の大きさ。幅と高さは偶数に切り下げる（NV12 は 2x2 画素で 1 組の色差を持つ）。
/// 右端の列・下端の行が 1 画素ずつ落ちるだけで、縮小はしない。
pub(super) fn even_size(width: usize, height: usize) -> (usize, usize) {
    (width & !1, height & !1)
}

/// NV12 のバイト数（Y 面 + 交互に並んだ Cb / Cr 面）。
pub(super) fn nv12_len(width: usize, height: usize) -> usize {
    width * height + width * height / 2
}

/// RGB（1 画素 3 バイト、行の詰め物なし）の `src` を、偶数に切り下げた
/// `width` x `height` の NV12 にして `dst` へ書く。`dst` は必要な長さへ詰め直す
/// （使い回せば、2 回目以降は確保が起きない）。
///
/// `src_width` / `src_height` は元の大きさ。`width` / `height` はそれ以下の偶数で
/// あること（`even_size` の結果を渡す）。`src` が足りなければ何もせず `false`。
///
/// 色差は 2x2 画素の RGB の和から求める（平均を取ってから変換するのと同じ。
/// 変換が線形なので、画素ごとに変換してから平均するのとも一致する）。
pub(super) fn rgb_to_nv12(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    width: usize,
    height: usize,
    matrix: Nv12Matrix,
    dst: &mut Vec<u8>,
) -> bool {
    if width > src_width
        || height > src_height
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || src.len() < src_width * src_height * 3
    {
        return false;
    }
    dst.resize(nv12_len(width, height), 0);
    let coefficients = matrix.coefficients();
    let (y_plane, uv_plane) = dst.split_at_mut(width * height);
    let stride = src_width * 3;

    for pair in 0..height / 2 {
        let top = &src[(pair * 2) * stride..(pair * 2) * stride + width * 3];
        let bottom = &src[(pair * 2 + 1) * stride..(pair * 2 + 1) * stride + width * 3];
        let (y_top, y_bottom) =
            y_plane[(pair * 2) * width..(pair * 2 + 2) * width].split_at_mut(width);
        for (y, rgb) in y_top.iter_mut().zip(top.as_chunks::<3>().0) {
            *y = luma(coefficients, rgb);
        }
        for (y, rgb) in y_bottom.iter_mut().zip(bottom.as_chunks::<3>().0) {
            *y = luma(coefficients, rgb);
        }

        // 横に 2 画素ずつ（6 バイト）の組を上下の行で合わせ、2x2 の和から色差を 1 組作る
        let uv_row = &mut uv_plane[pair * width..(pair + 1) * width];
        let blocks = top.as_chunks::<6>().0.iter().zip(bottom.as_chunks::<6>().0);
        for (uv, (upper, lower)) in uv_row.as_chunks_mut::<2>().0.iter_mut().zip(blocks) {
            let sum: [i32; 3] = std::array::from_fn(|channel| {
                i32::from(upper[channel])
                    + i32::from(upper[3 + channel])
                    + i32::from(lower[channel])
                    + i32::from(lower[3 + channel])
            });
            uv[0] = chroma(&coefficients.cb, sum);
            uv[1] = chroma(&coefficients.cr, sum);
        }
    }
    true
}

/// 1 画素の Y。四捨五入してから 16 を足す。
fn luma(coefficients: &RgbToYuv, rgb: &[u8; 3]) -> u8 {
    let [r, g, b] = [i32::from(rgb[0]), i32::from(rgb[1]), i32::from(rgb[2])];
    let value =
        (coefficients.y[0] * r + coefficients.y[1] * g + coefficients.y[2] * b + 32_768) >> 16;
    (16 + value).clamp(0, 255) as u8
}

/// 2x2 画素の RGB の和（4 画素ぶん）から色差を 1 つ。4 で割るぶん 2 ビット余分に戻す。
fn chroma(row: &[i32; 3], sum: [i32; 3]) -> u8 {
    let value = (row[0] * sum[0] + row[1] * sum[1] + row[2] * sum[2] + (1 << 17)) >> 18;
    (128 + value).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 75% のカラーバー（白・黄・シアン・緑・マゼンタ・赤・青・黒）の RGB
    const BARS_RGB: [[u8; 3]; 8] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
        [0, 0, 0],
    ];

    /// 同じカラーバーの BT.709 リミテッドの Y / Cb / Cr（SMPTE RP 219 の 8 ビット値）
    const BARS_BT709: [[u8; 3]; 8] = [
        [180, 128, 128],
        [168, 44, 136],
        [145, 147, 44],
        [133, 63, 52],
        [63, 193, 204],
        [51, 109, 212],
        [28, 212, 120],
        [16, 128, 128],
    ];

    /// 同じカラーバーの BT.601 リミテッドの Y / Cb / Cr
    const BARS_BT601: [[u8; 3]; 8] = [
        [180, 128, 128],
        [161, 44, 142],
        [131, 156, 44],
        [112, 72, 58],
        [84, 184, 198],
        [65, 100, 212],
        [35, 212, 114],
        [16, 128, 128],
    ];

    /// 1 色で塗った `width` x `height` の RGB
    fn solid(rgb: [u8; 3], width: usize, height: usize) -> Vec<u8> {
        rgb.iter()
            .copied()
            .cycle()
            .take(width * height * 3)
            .collect()
    }

    fn assert_near(actual: u8, expected: u8, label: &str) {
        assert!(
            actual.abs_diff(expected) <= 1,
            "{label}: 実際 {actual} / 期待 {expected}"
        );
    }

    #[test]
    fn rgb_to_nv12_color_bars_match_bt709_values() {
        for (rgb, yuv) in BARS_RGB.iter().zip(BARS_BT709) {
            let mut nv12 = Vec::new();
            assert!(rgb_to_nv12(
                &solid(*rgb, 2, 2),
                2,
                2,
                2,
                2,
                Nv12Matrix::Bt709,
                &mut nv12
            ));
            assert_eq!(nv12.len(), 6);
            for y in &nv12[..4] {
                assert_near(*y, yuv[0], "Y");
            }
            assert_near(nv12[4], yuv[1], "Cb");
            assert_near(nv12[5], yuv[2], "Cr");
        }
    }

    #[test]
    fn rgb_to_nv12_color_bars_match_bt601_values() {
        for (rgb, yuv) in BARS_RGB.iter().zip(BARS_BT601) {
            let mut nv12 = Vec::new();
            assert!(rgb_to_nv12(
                &solid(*rgb, 2, 2),
                2,
                2,
                2,
                2,
                Nv12Matrix::Bt601,
                &mut nv12
            ));
            assert_near(nv12[0], yuv[0], "Y");
            assert_near(nv12[4], yuv[1], "Cb");
            assert_near(nv12[5], yuv[2], "Cr");
        }
    }

    #[test]
    fn rgb_to_nv12_round_trip_stays_close_to_the_source() {
        // NV12 を BT.709 リミテッドの式で RGB へ戻し、元の色との差を見る。
        // 往復で色差がわずかに鈍るのは許容する（docs/design/recording.md）
        for rgb in BARS_RGB {
            let mut nv12 = Vec::new();
            assert!(rgb_to_nv12(
                &solid(rgb, 2, 2),
                2,
                2,
                2,
                2,
                Nv12Matrix::Bt709,
                &mut nv12
            ));
            let y = (f32::from(nv12[0]) - 16.0) * 255.0 / 219.0;
            let cb = (f32::from(nv12[4]) - 128.0) * 255.0 / 224.0;
            let cr = (f32::from(nv12[5]) - 128.0) * 255.0 / 224.0;
            let back = [
                y + 1.5748 * cr,
                y - 0.187_324 * cb - 0.468_124 * cr,
                y + 1.8556 * cb,
            ];
            for (channel, (original, restored)) in rgb.iter().zip(back).enumerate() {
                assert!(
                    (f32::from(*original) - restored).abs() <= 3.0,
                    "{rgb:?} の {channel} 番目: {restored}"
                );
            }
        }
    }

    #[test]
    fn rgb_to_nv12_averages_chroma_over_2x2() {
        // 左の列が白、右の列が黒。色差は灰色（128）、Y は画素ごと
        let src = [
            255, 255, 255, 0, 0, 0, //
            255, 255, 255, 0, 0, 0,
        ];
        let mut nv12 = Vec::new();
        assert!(rgb_to_nv12(&src, 2, 2, 2, 2, Nv12Matrix::Bt709, &mut nv12));
        assert_eq!(nv12, vec![235, 16, 235, 16, 128, 128]);
    }

    #[test]
    fn rgb_to_nv12_odd_source_drops_the_last_column_and_row() {
        // 3x3 の入力を 2x2 で書く。右端の列と下端の行（赤）は使わない
        let red = [255, 0, 0];
        let white = [255, 255, 255];
        let mut src = Vec::new();
        for row in 0..3 {
            for column in 0..3 {
                src.extend_from_slice(if row == 2 || column == 2 {
                    &red
                } else {
                    &white
                });
            }
        }
        let (width, height) = even_size(3, 3);
        let mut nv12 = Vec::new();
        assert!(rgb_to_nv12(
            &src,
            3,
            3,
            width,
            height,
            Nv12Matrix::Bt601,
            &mut nv12
        ));
        assert_eq!((width, height), (2, 2));
        assert_eq!(nv12, vec![235, 235, 235, 235, 128, 128]);
    }

    #[test]
    fn rgb_to_nv12_rejects_short_source_and_odd_target() {
        let mut nv12 = Vec::new();
        assert!(!rgb_to_nv12(
            &[0; 11],
            2,
            2,
            2,
            2,
            Nv12Matrix::Bt709,
            &mut nv12
        ));
        assert!(!rgb_to_nv12(
            &[0; 27],
            3,
            3,
            3,
            3,
            Nv12Matrix::Bt709,
            &mut nv12
        ));
        assert!(!rgb_to_nv12(
            &[0; 12],
            2,
            2,
            4,
            2,
            Nv12Matrix::Bt709,
            &mut nv12
        ));
    }

    #[test]
    fn nv12_matrix_for_size_follows_the_hd_boundary() {
        assert_eq!(Nv12Matrix::for_size(1280, 720), Nv12Matrix::Bt709);
        assert_eq!(Nv12Matrix::for_size(1920, 1080), Nv12Matrix::Bt709);
        assert_eq!(Nv12Matrix::for_size(720, 480), Nv12Matrix::Bt601);
    }

    #[test]
    fn nv12_len_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(nv12_len(1920, 1080), 3_110_400);
        assert_eq!(nv12_len(2, 2), 6);
    }
}
