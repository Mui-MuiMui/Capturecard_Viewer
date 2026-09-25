//! YUY2 / NV12 / I420 → RGB24 と、DirectShow の RGB24（BGR の並び）/ MJPEG → RGB24 の画素変換。
//!
//! 使う係数は `super::color` が決めた `ColorMatrix` を受け取るだけで、
//! ここは変換のループだけを持つ。**フレームコールバックから毎フレーム
//! 呼ばれるので、ロックもアロケーションも行わない。**

use super::color::ColorMatrix;

/// YUY2 -> RGB24 の高速変換 (最適化版)。
///
/// 変換に使う係数は `matrix` で受け取る。解像度から選ぶ場合は
/// `color_matrix_for` を通す。
///
/// 変換結果は `out` へ書き込む。`out` は呼び出し側が使い回す前提で、
/// 毎フレームの確保・ゼロクリア・解放を避けるために `&mut Vec<u8>` で受け取る。
///
/// `width * height * 3` バイトへリサイズしたうえで全域を書き切る。
/// 変換できなかった領域（幅が奇数で余る 1 画素、入力が足りない画素）は
/// 使い回した Vec に残る前フレームの画素が見えないよう 0 で埋める。
pub(super) fn yuy2_to_rgb_naive(
    width: usize,
    height: usize,
    src: &[u8],
    matrix: &ColorMatrix,
    out: &mut Vec<u8>,
) {
    // 既に確保済みの容量はそのまま使う。0 埋めが走るのは伸ばした分だけ
    out.resize(width * height * 3, 0);

    // 安全確保: 偶数幅前提 (YUYV ペア)
    // 4 バイト / 6 バイトに満たない端数は変換しない
    let converted_len = {
        // ループの外に出して、毎画素の間接参照を避ける
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

        // Y に掛ける前の減算（Y - y_offset）と、明るさ・コントラストの定数項を
        // 1 つにまとめる。cy * (Y - y_offset) + offset は cy * Y - bias に等しい。
        // ループの外で畳んでおけば、調整の有無で 1 画素あたりの演算が増えない
        let bias = cy * y_offset - offset;

        let (src_chunks, _) = src.as_chunks::<4>();
        let (out_chunks, _) = out.as_chunks_mut::<6>();
        let pair_count = src_chunks.len().min(out_chunks.len());

        for (src_chunk, out_chunk) in src_chunks.iter().zip(out_chunks.iter_mut()) {
            let y0 = src_chunk[0] as i32;
            let u = src_chunk[1] as i32;
            let y1 = src_chunk[2] as i32;
            let v = src_chunk[3] as i32;

            // 輝度の項。bias に「入力レンジの原点へ寄せる分」と
            // 「明るさ・コントラストの定数項」が畳み込まれている
            let l0 = cy * y0 - bias;
            let l1 = cy * y1 - bias;
            let d = u - 128;
            let e = v - 128;

            // 係数は 1024 倍の固定小数点なので >> 10 で戻す
            let r0 = (l0 + r_v * e) >> 10;
            let g0 = (l0 - g_u * d - g_v * e) >> 10;
            let b0 = (l0 + b_u * d) >> 10;
            let r1 = (l1 + r_v * e) >> 10;
            let g1 = (l1 - g_u * d - g_v * e) >> 10;
            let b1 = (l1 + b_u * d) >> 10;

            out_chunk[0] = r0.clamp(0, 255) as u8;
            out_chunk[1] = g0.clamp(0, 255) as u8;
            out_chunk[2] = b0.clamp(0, 255) as u8;
            out_chunk[3] = r1.clamp(0, 255) as u8;
            out_chunk[4] = g1.clamp(0, 255) as u8;
            out_chunk[5] = b1.clamp(0, 255) as u8;
        }

        pair_count * 6
    };

    // 変換しなかった領域は 0 で埋める。使い回した Vec では
    // 前フレームの画素が残っているため、埋めないと画面に出てしまう
    out[converted_len..].fill(0);
}

/// 4:2:0 の YUV（NV12 / I420）の面の並び。
///
/// どちらも Y 面（1 画素 1 バイト）のあとに、縦横とも半分の解像度の色差が続く。
/// 違うのは色差の置き方だけで、1 画素の式と係数表は YUY2 と同じものを使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Yuv420Layout {
    /// Y 面のあとに U と V が 1 バイトずつ交互に並ぶ 1 枚の面
    Nv12,
    /// Y 面のあとに U 面、V 面が順に並ぶ（IYUV も同じ並び）
    I420,
}

impl Yuv420Layout {
    /// 設定画面とログに出す名前。DirectShow の `SampleKind::name` と揃える
    pub(super) fn name(self) -> &'static str {
        match self {
            Yuv420Layout::Nv12 => "NV12",
            Yuv420Layout::I420 => "I420",
        }
    }
}

/// 4:2:0 の 1 フレームに要るバイト数。NV12 と I420 で同じ。
///
/// **幅か高さが奇数なら、色差は切り上げた大きさを持つ**（右端の列・下端の行は
/// 1 画素ぶんで色差を 1 組持つ）。libyuv と同じ扱い。各面の行に詰め物
/// （ストライドの余り）は無いものとする。
pub(super) fn yuv420_frame_len(width: usize, height: usize) -> usize {
    width * height + 2 * width.div_ceil(2) * height.div_ceil(2)
}

/// NV12 / I420 → RGB24 の変換。
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

/// DirectShow の RGB24（`MEDIASUBTYPE_RGB24`）を、上から下へ並んだ RGB へ並べ替える。
///
/// DirectShow の RGB24 は Windows のビットマップと同じ並びで、**1 画素が
/// B・G・R の順、各行は 4 バイト境界まで詰め物が入り、`bottom_up` なら
/// 最終行から先に並ぶ。** 係数表は通らない（色空間・映像調整は効かない）。
///
/// `stride` は 1 行のバイト数（詰め物を含む）。`out` は `yuy2_to_rgb_naive` と
/// 同じく使い回す前提で、`width * height * 3` バイトへリサイズして全域を書き切る。
/// 入力が足りない行は 0 で埋める。
pub(super) fn bgr24_to_rgb(
    width: usize,
    height: usize,
    stride: usize,
    bottom_up: bool,
    src: &[u8],
    out: &mut Vec<u8>,
) {
    let row_bytes = width * 3;
    out.resize(row_bytes * height, 0);
    if row_bytes == 0 {
        return;
    }
    for (row, out_row) in out.chunks_exact_mut(row_bytes).enumerate() {
        let src_row_index = if bottom_up { height - 1 - row } else { row };
        let start = src_row_index * stride;
        let Some(src_row) = src.get(start..start + row_bytes) else {
            out_row.fill(0);
            continue;
        };
        let (dst_pixels, _) = out_row.as_chunks_mut::<3>();
        let (src_pixels, _) = src_row.as_chunks::<3>();
        for (dst, bgr) in dst_pixels.iter_mut().zip(src_pixels) {
            dst[0] = bgr[2];
            dst[1] = bgr[1];
            dst[2] = bgr[0];
        }
    }
}

/// MJPEG の 1 フレーム（JPEG 1 枚）を RGB24 へ展開する。
///
/// **`image` クレートの JPEG デコーダ（jpeg-decoder）を使い、nokhwa の
/// デコーダ（mozjpeg）は使わない。** mozjpeg は壊れたデータを panic で知らせて
/// 内部で `catch_unwind` するが、release ビルドは `panic = "abort"` なので
/// そのままプロセスが落ちる（`docs/design/logging.md`）。キャプチャーの
/// MJPEG は途中で欠けたフレームが混ざりうるので、エラーを値で返すほうを選ぶ。
///
/// `out` は `width * height * 3` バイトへリサイズして書き込む。デコーダの中では
/// 作業用の確保が起きる（避けられない。MJPEG が「重い経路」である理由）。
/// 大きさがメディアタイプと食い違う・カラーでない・壊れている場合は
/// エラーの理由を返し、`out` の中身は不定。
pub(super) fn mjpeg_to_rgb(
    width: usize,
    height: usize,
    src: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    use image::codecs::jpeg::JpegDecoder;
    use image::{ColorType, ImageDecoder};

    let decoder = JpegDecoder::new(std::io::Cursor::new(src)).map_err(|e| e.to_string())?;
    let (decoded_width, decoded_height) = decoder.dimensions();
    if (decoded_width as usize, decoded_height as usize) != (width, height) {
        return Err(format!(
            "{}x{} のはずが {}x{}",
            width, height, decoded_width, decoded_height
        ));
    }
    if decoder.color_type() != ColorType::Rgb8 {
        return Err(format!("{:?} は扱わない", decoder.color_type()));
    }
    out.resize(width * height * 3, 0);
    decoder.read_image(out).map_err(|e| e.to_string())
}

/// RGB24 の 1 行のバイト数。Windows のビットマップと同じく 4 バイト境界へ揃える
pub(super) fn bgr24_stride(width: usize) -> usize {
    (width * 3 + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::color::{
        adjusted_color_matrix, VideoAdjustments, BT601, BT601_FULL, BT709, BT709_FULL,
    };
    use std::time::Instant;

    /// 変換結果を新しい Vec で受け取るテスト用ヘルパー。
    /// 出力先の使い回しそのものを見るテストは `yuy2_to_rgb_naive` を直接呼ぶ
    fn convert_yuy2(width: usize, height: usize, src: &[u8], matrix: &ColorMatrix) -> Vec<u8> {
        let mut out = Vec::new();
        yuy2_to_rgb_naive(width, height, src, matrix, &mut out);
        out
    }

    // 期待値は係数表から手計算した結果をベタ書きする。
    // 実装と同じ式で計算すると、実装が誤っていてもテストが通ってしまうため。
    //
    // 計算式（`>> 10` は負の値では負の無限大方向へ丸められる）:
    //   c = Y - 16, d = Cb - 128, e = Cr - 128
    //   R = (y*c + r_v*e) >> 10
    //   G = (y*c - g_u*d - g_v*e) >> 10
    //   B = (y*c + b_u*d) >> 10

    #[test]
    fn yuy2_to_rgb_naive_bt601_known_pattern_converts_two_pixels() {
        // Y0=81, U=90, Y1=145, V=240 (赤寄りの YUYV ペア)
        let src = [81u8, 90, 145, 240];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![254, 0, 0, 255, 73, 73]);
    }

    #[test]
    fn yuy2_to_rgb_naive_output_length_is_width_times_height_times_three() {
        let src = [235u8, 128, 235, 128, 235, 128, 235, 128];
        let out = convert_yuy2(2, 2, &src, &BT601);
        assert_eq!(out.len(), 2 * 2 * 3);
        assert_eq!(
            out,
            vec![254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_odd_width_leaves_last_pixel_black() {
        // 幅が奇数だと出力が 6 バイト単位で割り切れず、最後の 1 画素は変換されず 0 のまま残る
        let src = [235u8, 128, 235, 128, 0, 0];
        let out = convert_yuy2(3, 1, &src, &BT601);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_short_source_leaves_remaining_pixels_black() {
        // 入力が 1 ペア分しかない場合、残りの画素は 0 のまま (パニックしない)
        let src = [235u8, 128, 235, 128];
        let out = convert_yuy2(4, 1, &src, &BT601);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt601_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えるため飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![255, 125, 255, 255, 125, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt601_min_input_saturates_at_0() {
        // Y=0, U=0, V=0 では R と B が負になるため 0 に飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![0, 135, 0, 0, 135, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_zero_size_returns_empty() {
        let out = convert_yuy2(0, 0, &[], &BT601);
        assert!(out.is_empty());
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_clears_unconverted_area() {
        // 使い回した Vec に前フレームの画素が残っていても、
        // 変換されない領域は 0 になること
        let mut out = vec![0xFFu8; 12];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(4, 1, &src, &BT601, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_shrinks_to_new_size() {
        // 解像度が小さくなっても出力長が追従し、前の内容が残らないこと
        let mut out = vec![0xFFu8; 24];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(2, 1, &src, &BT601, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_keeps_allocation() {
        // 同じ解像度で呼び直したときに再確保が起きないこと（このタスクの本題）
        let src = [81u8, 90, 145, 240, 81, 90, 145, 240];
        let mut out = Vec::new();
        yuy2_to_rgb_naive(2, 2, &src, &BT601, &mut out);
        let first_ptr = out.as_ptr();
        let first_capacity = out.capacity();

        yuy2_to_rgb_naive(2, 2, &src, &BT601, &mut out);
        assert_eq!(out.as_ptr(), first_ptr, "確保済みの領域を使い回す");
        assert_eq!(out.capacity(), first_capacity);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_known_pattern_converts_two_pixels() {
        // BT.601 のテストと同じ入力。Y0=81, U=90, Y1=145, V=240
        let src = [81u8, 90, 145, 240];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709),
            vec![255, 24, 0, 255, 98, 69]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601),
            vec![254, 0, 0, 255, 73, 73],
            "同じ入力でも係数が違えば結果が変わる"
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_neutral_gray_matches_bt601() {
        // Cb = Cr = 128 の無彩色では色差の項が 0 になるため、
        // Y の係数が同じ 1192 である両者の結果は一致する
        let src = [235u8, 128, 235, 128];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709),
            vec![254, 254, 254, 254, 254, 254]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601),
            convert_yuy2(2, 1, &src, &BT709)
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えるため飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src, &BT709);
        assert_eq!(out, vec![255, 183, 255, 255, 183, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_min_input_saturates_at_0() {
        // Y=0, U=0, V=0 では R と B が負になるため 0 に飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src, &BT709);
        assert_eq!(out, vec![0, 76, 0, 0, 76, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_maps_y_endpoints_to_black_and_white() {
        // フルレンジでは Y=0 が黒、Y=255 が白にそのまま対応する。
        // Cb = Cr = 128 なので色差の寄与は 0
        let src = [0u8, 128, 255, 128];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601_FULL),
            vec![0, 0, 0, 255, 255, 255]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709_FULL),
            vec![0, 0, 0, 255, 255, 255]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_keeps_mid_gray_at_128() {
        // Y=128、Cb=Cr=128 は中間グレー。スケールが 1 倍なので値が変わらない
        let src = [128u8, 128, 128, 128];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601_FULL),
            vec![128, 128, 128, 128, 128, 128]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709_FULL),
            vec![128, 128, 128, 128, 128, 128]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_does_not_crush_limited_range_endpoints() {
        // この設定を足した理由そのもの。フルレンジの信号にリミテッドの係数を
        // 当てると、Y=16 が 0 へ潰れ Y=235 が 254 まで持ち上がる。
        // フルレンジの表ならどちらも入力の値のまま残る
        let src = [16u8, 128, 235, 128];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601_FULL),
            vec![16, 16, 16, 235, 235, 235]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601),
            vec![0, 0, 0, 254, 254, 254],
            "リミテッドの表では両端が黒と白へ張り付く"
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_known_pattern_converts_two_pixels() {
        // リミテッドのテストと同じ入力。Y0=81, U=90, Y1=145, V=240
        let src = [81u8, 90, 145, 240];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601_FULL),
            vec![238, 14, 13, 255, 78, 77]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709_FULL),
            vec![255, 35, 10, 255, 99, 74]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_min_input_saturates_at_0() {
        // Y=0, U=0, V=0。Cb/Cr のオフセットはフルレンジでも 128 なので
        // 色差は負に振れ、R と B が 0 へ飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src, &BT601_FULL);
        assert_eq!(out, vec![0, 135, 0, 0, 135, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_full_range_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えて飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src, &BT601_FULL);
        assert_eq!(out, vec![255, 120, 255, 255, 120, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_brightness_shifts_every_channel() {
        // フルレンジで Y=100（Cb = Cr = 128）。明るさ +25 で 125 になる
        let src = [100u8, 128, 100, 128];
        let matrix = adjusted_color_matrix(&BT601_FULL, VideoAdjustments::new(25, 0, 0));
        assert_eq!(
            convert_yuy2(2, 1, &src, &matrix),
            vec![125, 125, 125, 125, 125, 125]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_contrast_keeps_mid_gray_and_stretches_the_rest() {
        // フルレンジで Y=128 と Y=192。コントラスト +100（2 倍）では
        // 中間グレーの 128 は動かず、192 は (192-128)*2+128 = 256 で飽和する
        let src = [128u8, 128, 192, 128];
        let matrix = adjusted_color_matrix(&BT601_FULL, VideoAdjustments::new(0, 100, 0));
        assert_eq!(
            convert_yuy2(2, 1, &src, &matrix),
            vec![128, 128, 128, 255, 255, 255]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_minimum_saturation_produces_gray_pixels() {
        // 色の付いた入力でも、彩度 -100 なら R = G = B になる。
        // 値は輝度の項だけ: (1192 * (81 - 16)) >> 10 = 75、
        //                   (1192 * (145 - 16)) >> 10 = 150
        let src = [81u8, 90, 145, 240];
        let matrix = adjusted_color_matrix(&BT601, VideoAdjustments::new(0, 0, -100));
        assert_eq!(
            convert_yuy2(2, 1, &src, &matrix),
            vec![75, 75, 75, 150, 150, 150]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_minimum_contrast_produces_flat_mid_gray() {
        // コントラスト -100 では入力によらず一様な中間グレーになる
        let src = [81u8, 90, 145, 240];
        let matrix = adjusted_color_matrix(&BT601, VideoAdjustments::new(0, -100, 0));
        assert_eq!(
            convert_yuy2(2, 1, &src, &matrix),
            vec![128, 128, 128, 128, 128, 128]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_neutral_adjustments_match_the_base_table() {
        // 無調整では調整を入れる前と同じ結果になること。
        // BT.601 の既知パターンのテストと同じ期待値
        let src = [81u8, 90, 145, 240];
        let matrix = adjusted_color_matrix(&BT601, VideoAdjustments::NEUTRAL);
        assert_eq!(
            convert_yuy2(2, 1, &src, &matrix),
            vec![254, 0, 0, 255, 73, 73]
        );
    }

    #[test]
    #[ignore = "計測用"]
    fn yuy2_to_rgb_naive_1080p_conversion_time() {
        // 実行: cargo test --release -- --ignored --nocapture
        // 毎フレームの新規確保と、確保済み Vec の使い回しを比べる
        //
        // 計測値の出力に println! を使う。アプリ本体では
        // #![windows_subsystem = "windows"] のため標準出力はどこにも届かないが、
        // テストバイナリの標準出力は cargo がパイプで受け取るため
        // --nocapture を付ければ表示される（実測で確認済み）
        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1080;
        const FRAMES: usize = 120;

        // 1080p 相当のダミー YUYV。定数畳み込みを避けるため画素ごとに値を変える
        let src: Vec<u8> = (0..WIDTH * HEIGHT * 2).map(|i| (i % 251) as u8).collect();

        let allocating_start = Instant::now();
        for _ in 0..FRAMES {
            let mut out = Vec::new();
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
            std::hint::black_box(&out);
        }
        let allocating_ms = allocating_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        let mut out = Vec::new();
        yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
        let reusing_start = Instant::now();
        for _ in 0..FRAMES {
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
            std::hint::black_box(&out);
        }
        let reusing_ms = reusing_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        // 映像調整を入れた表でも同じ時間で変換できることを確かめる。
        // 調整は係数と定数項へ畳み込まれ、変換式の形は変わらないため
        let adjusted = adjusted_color_matrix(&BT709, VideoAdjustments::new(20, -30, 40));
        yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &adjusted, &mut out);
        let adjusted_start = Instant::now();
        for _ in 0..FRAMES {
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &adjusted, &mut out);
            std::hint::black_box(&out);
        }
        let adjusted_ms = adjusted_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        println!(
            "1080p YUY2->RGB {} frames: allocate={:.3} ms/frame, reuse={:.3} ms/frame, adjusted={:.3} ms/frame",
            FRAMES, allocating_ms, reusing_ms, adjusted_ms
        );
    }

    #[test]
    fn bgr24_to_rgb_bottom_up_reverses_rows_and_swaps_channels() {
        // 1x2。幅 1 なので 1 行 3 バイト + 詰め物 1 バイト = 4 バイト。
        // 下の行（青）が先に並んでいる
        let src = [255, 0, 0, 0, 0, 0, 255, 0];
        let mut out = Vec::new();
        bgr24_to_rgb(1, 2, 4, true, &src, &mut out);
        // 上の行が赤、下の行が青
        assert_eq!(out, vec![255, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn bgr24_to_rgb_top_down_keeps_row_order() {
        let src = [255, 0, 0, 0, 0, 0, 255, 0];
        let mut out = Vec::new();
        bgr24_to_rgb(1, 2, 4, false, &src, &mut out);
        assert_eq!(out, vec![0, 0, 255, 255, 0, 0]);
    }

    #[test]
    fn bgr24_to_rgb_short_input_fills_missing_rows_with_zero() {
        // 2 行ぶん要るのに 1 行しか無い。使い回した Vec の前の画素を残さない
        let src = [10, 20, 30, 0];
        let mut out = vec![9; 6];
        bgr24_to_rgb(1, 2, 4, false, &src, &mut out);
        assert_eq!(out, vec![30, 20, 10, 0, 0, 0]);
    }

    #[test]
    fn bgr24_to_rgb_zero_width_produces_empty_output() {
        let mut out = vec![1, 2, 3];
        bgr24_to_rgb(0, 2, 0, true, &[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn bgr24_stride_aligns_to_four_bytes() {
        assert_eq!(bgr24_stride(1), 4);
        assert_eq!(bgr24_stride(2), 8);
        assert_eq!(bgr24_stride(4), 12);
        assert_eq!(bgr24_stride(640), 1920);
        assert_eq!(bgr24_stride(0), 0);
    }

    #[test]
    fn mjpeg_to_rgb_decodes_a_small_jpeg() {
        // 2x2 の単色（灰色）を image で JPEG にしてから展開する
        let rgb = vec![128u8; 2 * 2 * 3];
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode(&rgb, 2, 2, image::ColorType::Rgb8)
            .expect("JPEG にできる");

        let mut out = Vec::new();
        mjpeg_to_rgb(2, 2, &jpeg, &mut out).expect("展開できる");
        assert_eq!(out.len(), 12);
        // 非可逆なので 128 ちょうどとは限らない
        assert!(out.iter().all(|v| v.abs_diff(128) <= 2), "{out:?}");
    }

    #[test]
    fn mjpeg_to_rgb_size_mismatch_is_an_error() {
        let rgb = vec![0u8; 2 * 2 * 3];
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&rgb, 2, 2, image::ColorType::Rgb8)
            .expect("JPEG にできる");
        let mut out = Vec::new();
        assert!(mjpeg_to_rgb(4, 4, &jpeg, &mut out).is_err());
    }

    #[test]
    fn mjpeg_to_rgb_broken_data_is_an_error_not_a_panic() {
        // release は panic = "abort" なので、壊れたデータは値で返ること
        let mut out = Vec::new();
        assert!(mjpeg_to_rgb(2, 2, &[0xFF, 0xD8, 0x00, 0x01], &mut out).is_err());
        assert!(mjpeg_to_rgb(2, 2, &[], &mut out).is_err());
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
