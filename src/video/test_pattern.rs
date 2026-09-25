//! フェイクの映像デバイス（`super::fake`）が吐くテストパターンの描画。
//!
//! どれも純粋関数で、デバイスにもスレッドにも触らない。下地（カラーバー /
//! ベタ塗り）はストリームを開くときに 1 回だけ描き、フレーム番号の焼き込み
//! だけを毎フレーム行う。
//!
//! パターンの色は、解像度が HD なら BT.709、それ未満なら BT.601 の
//! リミテッドレンジで符号化する。実機のキャプチャーボードと同じ扱いで、
//! 色空間が「自動」ならそのまま正しい色に戻る。

use super::color::is_hd_resolution;

/// カラーバーの色（フルレンジ RGB）。75% の SMPTE 風に 8 本並べる
const COLOR_BARS: [[u8; 3]; 8] = [
    [191, 191, 191],
    [191, 191, 0],
    [0, 191, 191],
    [0, 191, 0],
    [191, 0, 191],
    [191, 0, 0],
    [0, 0, 191],
    [0, 0, 0],
];

/// ベタ塗りの色（フルレンジ RGB）。偶数番のデバイスへ順に割り当てる
const SOLID_COLORS: [(&str, [u8; 3]); 4] = [
    ("青", [0, 0, 191]),
    ("赤", [191, 0, 0]),
    ("緑", [0, 191, 0]),
    ("黄", [191, 191, 0]),
];

/// 焼き込むフレーム番号の数字。3x5 のビットマップで、各行の下位 3 ビットが
/// 左から右の画素を表す
const DIGIT_GLYPHS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// 焼き込みの文字と背景の Y。リミテッドレンジの白と黒
const TEXT_Y: u8 = 235;
const TEXT_BACKGROUND_Y: u8 = 16;

/// デバイス 1 台が吐くテストパターン。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pattern {
    ColorBars,
    Solid([u8; 3]),
}

/// 番号（1 から）からパターンを決める。奇数がカラーバー、偶数がベタ塗り
pub(super) fn pattern_for(index: u32) -> Pattern {
    if index % 2 == 1 {
        Pattern::ColorBars
    } else {
        let (_, rgb) = SOLID_COLORS[solid_color_slot(index)];
        Pattern::Solid(rgb)
    }
}

/// 偶数番のデバイスがベタ塗りの色のどれを使うか
fn solid_color_slot(index: u32) -> usize {
    ((index / 2).saturating_sub(1) as usize) % SOLID_COLORS.len()
}

/// 一覧に出す説明
pub(super) fn description_for(index: u32) -> String {
    match pattern_for(index) {
        Pattern::ColorBars => "フェイクの映像デバイス（カラーバー）".to_string(),
        Pattern::Solid(_) => format!(
            "フェイクの映像デバイス（ベタ塗り: {}）",
            SOLID_COLORS[solid_color_slot(index)].0
        ),
    }
}

/// フルレンジの RGB を、リミテッドレンジの YCbCr へ直す。
///
/// パターンを作るときに 1 色あたり 1 回だけ呼ぶ。毎フレームは通らない。
fn rgb_to_ycbcr(rgb: [u8; 3], bt709: bool) -> [u8; 3] {
    let (kr, kb) = if bt709 {
        (0.2126, 0.0722)
    } else {
        (0.299, 0.114)
    };
    let [r, g, b] = rgb.map(|v| f64::from(v) / 255.0);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let pb = (b - y) / (2.0 * (1.0 - kb));
    let pr = (r - y) / (2.0 * (1.0 - kr));
    let quantize = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    [
        quantize(16.0 + 219.0 * y),
        quantize(128.0 + 224.0 * pb),
        quantize(128.0 + 224.0 * pr),
    ]
}

/// パターンを YUY2 で描く。焼き込みの無い下地で、ストリームを開くときに 1 回だけ作る。
///
/// 幅は偶数を前提にする（`super::fake` の `MODES` はどれも偶数）。奇数なら最後の 1 画素は描かない。
pub(super) fn render_pattern(pattern: Pattern, width: usize, height: usize) -> Vec<u8> {
    let bt709 = is_hd_resolution(width, height);
    let mut out = vec![0u8; width * height * 2];
    let bars = COLOR_BARS.map(|rgb| rgb_to_ycbcr(rgb, bt709));
    let solid = match pattern {
        Pattern::Solid(rgb) => Some(rgb_to_ycbcr(rgb, bt709)),
        Pattern::ColorBars => None,
    };

    if width == 0 {
        // `chunks_exact_mut(0)` は panic する。描くものも無い
        return out;
    }
    for row in out.chunks_exact_mut(width * 2) {
        let (pairs, _) = row.as_chunks_mut::<4>();
        for (pair, chunk) in pairs.iter_mut().enumerate() {
            // 1 組（2 画素）で U / V を共有するので、左の画素で色を決める
            let [y, u, v] = solid.unwrap_or_else(|| bars[pair * 2 * bars.len() / width]);
            *chunk = [y, u, y, v];
        }
    }
    out
}

/// 左上へフレーム番号を焼き込む。**毎フレーム呼ぶのでアロケーションしない。**
///
/// 背景を黒、文字を白で描き、その範囲の色差は無彩色（128）にする。
/// 文字の大きさは高さに合わせて変え、枠に収まらない部分は描かない。
pub(super) fn burn_frame_number(buffer: &mut [u8], width: usize, height: usize, number: u64) {
    // 桁を上から並べる。u64 は最大 20 桁
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = number;
    loop {
        digits[count] = (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    digits[..count].reverse();

    let scale = (height / 90).max(1);
    let left = scale * 2;
    let top = scale * 2;
    // 余白 1 マス + 1 桁あたり 4 マス（字 3 + 間隔 1）、縦は余白込みで 7 マス
    let box_width = (count * 4 + 1) * scale;
    let box_height = 7 * scale;

    for y in top..(top + box_height).min(height) {
        let cell_row = (y - top) / scale;
        for x in left..(left + box_width).min(width) {
            let cell_col = (x - left) / scale;
            let lit = (1..=5).contains(&cell_row) && cell_col >= 1 && {
                let glyph_col = (cell_col - 1) % 4;
                let digit = (cell_col - 1) / 4;
                glyph_col < 3
                    && digit < count
                    && DIGIT_GLYPHS[digits[digit] as usize][cell_row - 1] & (0b100 >> glyph_col)
                        != 0
            };
            let index = (y * width + x) * 2;
            buffer[index] = if lit { TEXT_Y } else { TEXT_BACKGROUND_Y };
            // 偶数画素なら U、奇数画素なら V。どちらも無彩色にする
            buffer[index + 1] = 128;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repaint::RepaintWaker;
    use crate::video::color::SharedColorConversion;
    use crate::video::frame_buffer::VideoFrames;
    use crate::video::frame_sink::FrameSink;
    use std::sync::Arc;
    use std::time::Instant;

    /// 期待値との差が許容範囲に収まっているか調べる。
    ///
    /// **許容幅はパターン自体の量子化の分。** フルレンジの RGB をリミテッド
    /// レンジの 8 ビット YCbCr へ丸めた時点で最大 1〜2 段ずれるので、戻した
    /// RGB も同じだけずれうる。変換の誤りはこれより大きく外れる
    fn assert_rgb_close(actual: &[u8], expected: [u8; 3], label: &str) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                a.abs_diff(e) <= 2,
                "{label}: 実際の値 {actual:?} が期待値 {expected:?} から離れている"
            );
        }
    }

    /// パターンを実機と同じ受け口（`FrameSink`）へ流し、RGB で読み戻す
    fn convert_through_sink(pattern: Pattern, width: usize, height: usize) -> Vec<u8> {
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );
        let yuy2 = render_pattern(pattern, width, height);
        assert!(sink.push_yuy2(width, height, &yuy2, Instant::now()));
        let frame = frames.latest().expect("積んだフレームが読める");
        assert_eq!((frame.width, frame.height), (width, height));
        frame.data.clone()
    }

    /// 画素 (x, y) の RGB
    fn pixel(rgb: &[u8], width: usize, x: usize, y: usize) -> &[u8] {
        let index = (y * width + x) * 3;
        &rgb[index..index + 3]
    }

    #[test]
    fn color_bars_sd_come_back_as_the_intended_colors() {
        // 640x480 は SD なので BT.601 で符号化し、色空間「自動」も BT.601 で戻す
        let (width, height) = (640, 480);
        let rgb = convert_through_sink(Pattern::ColorBars, width, height);

        // 各バーの真ん中の画素を見る。バーは 80 画素ずつ
        let expected = [
            [191, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
            [0, 0, 0],
        ];
        for (bar, color) in expected.into_iter().enumerate() {
            let x = bar * 80 + 40;
            assert_rgb_close(
                pixel(&rgb, width, x, height - 1),
                color,
                &format!("SD のバー {bar}"),
            );
        }
    }

    #[test]
    fn color_bars_hd_come_back_as_the_intended_colors() {
        // 1280x720 は HD なので BT.709 で符号化する。色空間「自動」が
        // BT.709 を選ぶので、同じ色に戻る
        let (width, height) = (1280, 720);
        let rgb = convert_through_sink(Pattern::ColorBars, width, height);

        let expected = [
            [191, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
            [0, 0, 0],
        ];
        for (bar, color) in expected.into_iter().enumerate() {
            let x = bar * 160 + 80;
            assert_rgb_close(
                pixel(&rgb, width, x, height / 2),
                color,
                &format!("HD のバー {bar}"),
            );
        }
    }

    #[test]
    fn solid_pattern_fills_every_pixel_with_the_color() {
        let (width, height) = (640, 480);
        let rgb = convert_through_sink(Pattern::Solid([0, 0, 191]), width, height);

        let (pixels, rest) = rgb.as_chunks::<3>();
        assert!(rest.is_empty());
        assert_eq!(pixels.len(), width * height);
        // 全画素が同じ色になる（焼き込みを通していないので例外は無い）
        let first = pixels[0];
        assert_rgb_close(&first, [0, 0, 191], "先頭の画素");
        assert!(pixels.iter().all(|pixel| *pixel == first));
    }

    #[test]
    fn rgb_to_ycbcr_matches_the_reference_values() {
        // 75% の青は BT.601 で Y=35 Cb=212 Cr=114、BT.709 で Y=28 Cb=212 Cr=120、
        // 75% の白はどちらも Y=180（リミテッドレンジのカラーバーの規格値）
        assert_eq!(rgb_to_ycbcr([0, 0, 191], false), [35, 212, 114]);
        assert_eq!(rgb_to_ycbcr([0, 0, 191], true), [28, 212, 120]);
        assert_eq!(rgb_to_ycbcr([191, 191, 191], true), [180, 128, 128]);
        // 黒と白は色空間によらない
        assert_eq!(rgb_to_ycbcr([0, 0, 0], true), [16, 128, 128]);
        assert_eq!(rgb_to_ycbcr([255, 255, 255], false), [235, 128, 128]);
    }

    #[test]
    fn burn_frame_number_only_touches_the_top_left_box() {
        let (width, height) = (640, 480);
        let base = render_pattern(Pattern::Solid([0, 0, 191]), width, height);
        let mut frame = base.clone();
        burn_frame_number(&mut frame, width, height, 8);

        // 480 / 90 = 5 倍。箱は x 10〜34、y 10〜44（1 桁 = 余白込み 5 マス × 7 マス）
        let changed: Vec<usize> = (0..width * height)
            .filter(|&i| frame[i * 2..i * 2 + 2] != base[i * 2..i * 2 + 2])
            .collect();
        assert!(!changed.is_empty(), "焼き込みで何も変わっていない");
        for i in changed {
            let (x, y) = (i % width, i / width);
            assert!(
                (10..35).contains(&x) && (10..45).contains(&y),
                "箱の外 ({x}, {y}) が書き換わった"
            );
        }

        // 「8」の左上の角（マス (1, 1)）は点灯、字の間の余白（マス (0, 0)）は背景
        assert_eq!(frame[(15 * width + 15) * 2], TEXT_Y);
        assert_eq!(frame[(10 * width + 10) * 2], TEXT_BACKGROUND_Y);
    }

    #[test]
    fn burn_frame_number_differs_between_numbers() {
        let (width, height) = (640, 480);
        let base = render_pattern(Pattern::ColorBars, width, height);
        let mut first = base.clone();
        let mut second = base.clone();
        burn_frame_number(&mut first, width, height, 0);
        burn_frame_number(&mut second, width, height, 1);

        assert_ne!(first, second);
    }

    #[test]
    fn burn_frame_number_larger_than_the_frame_does_not_panic() {
        // 幅 4 の画に 20 桁は収まらない。はみ出した分は描かない
        let mut frame = vec![0u8; 4 * 2 * 2];
        burn_frame_number(&mut frame, 4, 2, u64::MAX);
    }

    #[test]
    fn pattern_for_alternates_bars_and_solid_colors() {
        assert_eq!(pattern_for(1), Pattern::ColorBars);
        assert_eq!(pattern_for(2), Pattern::Solid([0, 0, 191]));
        assert_eq!(pattern_for(3), Pattern::ColorBars);
        assert_eq!(pattern_for(4), Pattern::Solid([191, 0, 0]));
        // 色が尽きたら最初へ戻る
        assert_eq!(pattern_for(10), Pattern::Solid([0, 0, 191]));
    }
}
