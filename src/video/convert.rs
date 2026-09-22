//! YUY2 → RGB24 の画素変換。
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
}
