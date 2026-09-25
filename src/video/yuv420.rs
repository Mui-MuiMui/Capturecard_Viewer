//! 4:2:0 の YUV（NV12 / I420 / YV12）→ RGB24 の画素変換。
//!
//! 1 画素の式と係数表（`super::color` の `ColorMatrix`）は YUY2
//! （`super::convert::yuy2_to_rgb_naive`）と同じで、違うのは色差の置き方だけ。
//! `convert.rs` が 800 行を超えたので分けた。**フレームコールバックから毎フレーム
//! 呼ばれるので、ロックもアロケーションも行わない。**

use super::color::ColorMatrix;

/// 4:2:0 の YUV（NV12 / I420 / YV12）の面の並び。
///
/// どれも Y 面（1 画素 1 バイト）のあとに、縦横とも半分の解像度の色差が続く。
/// 違うのは色差の置き方だけで、1 画素の式と係数表は YUY2 と同じものを使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Yuv420Layout {
    /// Y 面のあとに U と V が 1 バイトずつ交互に並ぶ 1 枚の面
    Nv12,
    /// Y 面のあとに U 面、V 面が順に並ぶ（IYUV も同じ並び）
    I420,
    /// I420 の U 面と V 面が逆（Y 面、V 面、U 面の順）
    Yv12,
}

impl Yuv420Layout {
    /// 設定画面とログに出す名前。DirectShow の `SampleKind::name` と揃える
    pub(super) fn name(self) -> &'static str {
        match self {
            Yuv420Layout::Nv12 => "NV12",
            Yuv420Layout::I420 => "I420",
            Yuv420Layout::Yv12 => "YV12",
        }
    }
}

/// 4:2:0 の 1 フレームに要るバイト数。NV12 / I420 / YV12 で同じ。
///
/// **幅か高さが奇数なら、色差は切り上げた大きさを持つ**（右端の列・下端の行は
/// 1 画素ぶんで色差を 1 組持つ）。libyuv と同じ扱い。各面の行に詰め物
/// （ストライドの余り）は無いものとする。
pub(super) fn yuv420_frame_len(width: usize, height: usize) -> usize {
    width * height + 2 * width.div_ceil(2) * height.div_ceil(2)
}

/// NV12 / I420 / YV12 → RGB24 の変換。
///
/// 係数は `matrix` で受け取り、1 画素の式は `yuy2_to_rgb_naive` と同じ
/// （同じ Y・Cb・Cr なら同じ RGB になる）。`out` は使い回す前提で、
/// `width * height * 3` バイトへリサイズして全域を書き切る。
///
/// 入力が `yuv420_frame_len` に満たなければ全域を 0 で埋める（呼び出し側の
/// `FrameSink` は足りないフレームを先に捨てるので、通常は来ない）。
pub(super) fn yuv420_to_rgb(
    layout: Yuv420Layout,
    width: usize,
    height: usize,
    src: &[u8],
    matrix: &ColorMatrix,
    out: &mut Vec<u8>,
) {
    let row_bytes = width * 3;
    out.resize(row_bytes * height, 0);
    // 幅か高さが 0 なら書く画素が無い（色差の面も空なので、先で切り出さない）
    if out.is_empty() {
        return;
    }
    if src.len() < yuv420_frame_len(width, height) {
        out.fill(0);
        return;
    }

    let ColorMatrix {
        y_offset,
        y: cy,
        r_v,
        g_u,
        g_v,
        b_u,
        offset,
        ..
    } = *matrix;
    // `yuy2_to_rgb_naive` と同じく、Y の原点と調整の定数項を 1 つに畳む
    let bias = cy * y_offset - offset;

    let chroma_width = width.div_ceil(2);
    let chroma_plane = chroma_width * height.div_ceil(2);
    let (y_plane, chroma) = src.split_at(width * height);
    // 色差の 1 行の長さと、行の中で U / V が何バイトおきに並ぶか
    let (u_plane, v_plane, chroma_stride, step) = match layout {
        Yuv420Layout::Nv12 => (chroma, &chroma[1..], chroma_width * 2, 2),
        Yuv420Layout::I420 => (chroma, &chroma[chroma_plane..], chroma_width, 1),
        Yuv420Layout::Yv12 => (&chroma[chroma_plane..], chroma, chroma_width, 1),
    };

    for (row, out_row) in out.chunks_exact_mut(row_bytes).enumerate() {
        let y_row = &y_plane[row * width..(row + 1) * width];
        let chroma_start = (row / 2) * chroma_stride;
        let u_row = &u_plane[chroma_start..];
        let v_row = &v_plane[chroma_start..];
        // 横に並ぶ 2 画素が 1 組の色差を共有する。幅が奇数なら最後の組は 1 画素
        for (column, (y_pair, out_pair)) in y_row.chunks(2).zip(out_row.chunks_mut(6)).enumerate() {
            let d = u_row[column * step] as i32 - 128;
            let e = v_row[column * step] as i32 - 128;
            let red = r_v * e;
            let green = g_u * d + g_v * e;
            let blue = b_u * d;
            let (pixels, _) = out_pair.as_chunks_mut::<3>();
            for (&y, pixel) in y_pair.iter().zip(pixels) {
                let l = cy * y as i32 - bias;
                // 係数は 1024 倍の固定小数点なので >> 10 で戻す
                pixel[0] = ((l + red) >> 10).clamp(0, 255) as u8;
                pixel[1] = ((l - green) >> 10).clamp(0, 255) as u8;
                pixel[2] = ((l + blue) >> 10).clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::color::{
        adjusted_color_matrix, VideoAdjustments, BT601, BT601_FULL, BT709, BT709_FULL,
    };
    use crate::video::convert::yuy2_to_rgb_naive;

    /// YUY2 の変換結果を新しい Vec で受け取る。カラーバーの突き合わせに使う
    fn convert_yuy2(width: usize, height: usize, src: &[u8], matrix: &ColorMatrix) -> Vec<u8> {
        let mut out = Vec::new();
        yuy2_to_rgb_naive(width, height, src, matrix, &mut out);
        out
    }

    /// NV12 / I420 の変換結果を新しい Vec で受け取るテスト用ヘルパー
    fn convert_420(
        layout: Yuv420Layout,
        width: usize,
        height: usize,
        src: &[u8],
        matrix: &ColorMatrix,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        yuv420_to_rgb(layout, width, height, src, matrix, &mut out);
        out
    }

    /// Y 面と、2x2 画素ごとの (U, V) の並びから NV12 と I420 のフレームを作る
    fn planes_420(y: &[u8], chroma: &[(u8, u8)]) -> (Vec<u8>, Vec<u8>) {
        let mut nv12 = y.to_vec();
        let mut i420 = y.to_vec();
        for &(u, v) in chroma {
            nv12.extend_from_slice(&[u, v]);
            i420.push(u);
        }
        i420.extend(chroma.iter().map(|&(_, v)| v));
        (nv12, i420)
    }

    #[test]
    fn yuv420_frame_len_rounds_chroma_up_for_odd_sizes() {
        assert_eq!(yuv420_frame_len(2, 2), 6);
        // 3x3 は Y が 9 バイト、色差が 2x2 組 × 2 バイト
        assert_eq!(yuv420_frame_len(3, 3), 17);
        assert_eq!(yuv420_frame_len(1920, 1080), 3_110_400);
        assert_eq!(yuv420_frame_len(0, 0), 0);
    }

    #[test]
    fn yuv420_to_rgb_solid_white_converts_every_pixel() {
        // 単色（Y=235、Cb = Cr = 128）。BT.601 のリミテッドで 254 になる
        let (nv12, i420) = planes_420(&[235; 4], &[(128, 128)]);
        for (layout, src) in [(Yuv420Layout::Nv12, &nv12), (Yuv420Layout::I420, &i420)] {
            assert_eq!(
                convert_420(layout, 2, 2, src, &BT601),
                vec![254; 12],
                "{layout:?}"
            );
        }
    }

    #[test]
    fn yuv420_to_rgb_yv12_reads_v_plane_before_u_plane() {
        // YV12 は Y 面のあとに V 面、U 面の順。U=90, V=240 を V・U の順に置き、
        // YUY2 の既知パターン（Y0=81, Y1=145）と同じ値になることを見る
        let src = [81, 145, 81, 145, 240, 90];
        let row = [254, 0, 0, 255, 73, 73];
        let expected: Vec<u8> = row.iter().chain(row.iter()).copied().collect();
        assert_eq!(
            convert_420(Yuv420Layout::Yv12, 2, 2, &src, &BT601),
            expected
        );
        assert_ne!(
            convert_420(Yuv420Layout::I420, 2, 2, &src, &BT601),
            expected,
            "同じバイト列を I420 として読むと U と V が入れ替わる"
        );
    }

    #[test]
    fn yuv420_to_rgb_known_pattern_matches_yuy2_expectation() {
        // YUY2 の既知パターンと同じ Y0=81, U=90, Y1=145, V=240 を 2 行に並べる。
        // U と V を取り違えると結果が変わるので、面の並びの確認も兼ねる
        let (nv12, i420) = planes_420(&[81, 145, 81, 145], &[(90, 240)]);
        let row = [254, 0, 0, 255, 73, 73];
        let expected: Vec<u8> = row.iter().chain(row.iter()).copied().collect();
        assert_eq!(
            convert_420(Yuv420Layout::Nv12, 2, 2, &nv12, &BT601),
            expected
        );
        assert_eq!(
            convert_420(Yuv420Layout::I420, 2, 2, &i420, &BT601),
            expected
        );
    }

    #[test]
    fn yuv420_to_rgb_color_bars_match_the_yuy2_path() {
        // 75% のカラーバー（BT.601 リミテッドの Y, Cb, Cr）。1 本を 2 画素幅にして
        // 16x2 に並べ、同じ値の YUY2 と画素ごとに一致することを見る。
        // 色空間・レンジ・映像調整を畳んだ表でも同じになること
        const BARS: [(u8, u8, u8); 8] = [
            (180, 128, 128),
            (162, 44, 142),
            (131, 156, 44),
            (112, 72, 58),
            (84, 184, 198),
            (65, 100, 212),
            (35, 212, 114),
            (16, 128, 128),
        ];
        let y_row: Vec<u8> = BARS.iter().flat_map(|&(y, _, _)| [y, y]).collect();
        let y_plane: Vec<u8> = y_row.iter().chain(y_row.iter()).copied().collect();
        let chroma: Vec<(u8, u8)> = BARS.iter().map(|&(_, u, v)| (u, v)).collect();
        let (nv12, i420) = planes_420(&y_plane, &chroma);
        let yuy2_row: Vec<u8> = BARS.iter().flat_map(|&(y, u, v)| [y, u, y, v]).collect();
        let yuy2: Vec<u8> = yuy2_row.iter().chain(yuy2_row.iter()).copied().collect();

        let adjusted = adjusted_color_matrix(&BT709, VideoAdjustments::new(20, -30, 40));
        for matrix in [&BT601, &BT709, &BT601_FULL, &BT709_FULL, &adjusted] {
            let expected = convert_yuy2(16, 2, &yuy2, matrix);
            assert_eq!(
                convert_420(Yuv420Layout::Nv12, 16, 2, &nv12, matrix),
                expected,
                "NV12 {}",
                matrix.name
            );
            assert_eq!(
                convert_420(Yuv420Layout::I420, 16, 2, &i420, matrix),
                expected,
                "I420 {}",
                matrix.name
            );
        }
        // 1 本目（白）と 6 本目（赤）の 1 画素は手計算の値とも合うこと。
        // 白: (1192 * 164) >> 10 = 190
        // 赤: R = (1192 * 49 + 1634 * 84) >> 10 = 191、G と B は負か 0 付近で 0
        let out = convert_420(Yuv420Layout::Nv12, 16, 2, &nv12, &BT601);
        assert_eq!(&out[0..3], &[190, 190, 190]);
        assert_eq!(&out[30..33], &[191, 0, 0]);
    }

    #[test]
    fn yuv420_to_rgb_odd_size_converts_the_last_column_and_row() {
        // 3x3。右端の列と下端の行は 1 画素で色差を 1 組持つ。
        // 右下の 1 画素だけに色を付け、そこだけが赤になることを見る
        let mut y = [235u8; 9];
        y[8] = 81;
        let (nv12, i420) = planes_420(&y, &[(128, 128), (128, 128), (128, 128), (90, 240)]);
        let mut expected = vec![254u8; 27];
        expected[24..27].copy_from_slice(&[254, 0, 0]);
        assert_eq!(
            convert_420(Yuv420Layout::Nv12, 3, 3, &nv12, &BT601),
            expected
        );
        assert_eq!(
            convert_420(Yuv420Layout::I420, 3, 3, &i420, &BT601),
            expected
        );
    }

    #[test]
    fn yuv420_to_rgb_short_source_is_all_black() {
        // 2x2 に 6 バイト要るところ 5 バイト。使い回した Vec の前の画素を残さない
        let mut out = vec![0xFF; 12];
        yuv420_to_rgb(Yuv420Layout::Nv12, 2, 2, &[235; 5], &BT601, &mut out);
        assert_eq!(out, vec![0; 12]);
    }

    #[test]
    fn yuv420_to_rgb_zero_size_returns_empty() {
        let mut out = vec![1, 2, 3];
        yuv420_to_rgb(Yuv420Layout::Nv12, 0, 2, &[], &BT601, &mut out);
        assert!(out.is_empty());
        yuv420_to_rgb(Yuv420Layout::I420, 2, 0, &[], &BT601, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn yuv420_to_rgb_reused_buffer_keeps_allocation() {
        // 同じ解像度で呼び直したときに再確保が起きないこと
        let (nv12, _) = planes_420(&[81, 145, 81, 145], &[(90, 240)]);
        let mut out = Vec::new();
        yuv420_to_rgb(Yuv420Layout::Nv12, 2, 2, &nv12, &BT601, &mut out);
        let first_ptr = out.as_ptr();
        yuv420_to_rgb(Yuv420Layout::Nv12, 2, 2, &nv12, &BT601, &mut out);
        assert_eq!(out.as_ptr(), first_ptr, "確保済みの領域を使い回す");
    }
}
