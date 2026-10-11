//! フレームの画素の並び（`PixelFormat`）と、RGB を要る側（録画・スクリーンショット）が
//! YUY2 のまま積まれたフレームから RGB を取り出す口（#456）。
//!
//! **YUY2 を GPU で変換するとき（`super::gpu_yuy2`）だけ、フレームコールバックは
//! YUY2 を変換せずに積む。** 画面はシェーダーが RGB にするが、録画とスクリーンショットは
//! RGB を要るので、それぞれのスレッド（録画スレッド・保存スレッド）でここを通して
//! CPU で変換する。**UI スレッドとフレームコールバックからは呼ばない。**
//! 変換に使う係数表はフレームが持っている（積んだときの色空間・レンジ・映像調整）ので、
//! 画面・録画・スクリーンショットの色は CPU で変換していたころと同じになる。

use std::sync::Arc;

use super::color::ColorMatrix;
use super::convert::yuy2_to_rgb_naive;
use super::frame_buffer::{FrameLenStatus, VideoFrame};

/// フレームの画素の並び。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// RGB24（1 画素 3 バイト、上の行から）。YUY2 以外の経路と、CPU で変換した YUY2
    Rgb24,
    /// YUY2 のまま（1 画素 2 バイト、`Y0 U Y1 V` の並び）。GPU で変換するときだけ積む。
    /// 幅は偶数に限る（`FrameSink::push_yuy2` が奇数幅を CPU へ回す）。
    /// 持っている係数表は、積んだときの色空間・レンジ・映像調整を畳み込んだもの
    Yuy2(ColorMatrix),
}

impl PixelFormat {
    /// 1 画素のバイト数
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb24 => 3,
            PixelFormat::Yuy2(_) => 2,
        }
    }
}

/// フレームがどの経路で積まれたか。統計 OSD の数え方と「デコード」の行の出し方に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertPath {
    /// 自前の変換（係数表が効く）を CPU で通した
    Fast,
    /// YUY2 を変換せずに積んだ。GPU で自前の変換と同じ式・同じ係数表を通す
    Gpu,
    /// デコーダ任せか RGB のまま（係数表を通らず、色空間が効かない）
    Fallback,
}

/// `len` バイトの画素データが `width` x `height` で 1 画素 `bytes_per_pixel` バイトの
/// フレームに合うかを判定する。`幅 × 高さ × バイト数` が溢れるときは足りない扱い。
pub fn pixel_len_status(
    len: usize,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
) -> FrameLenStatus {
    let Some(expected) = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(bytes_per_pixel))
    else {
        return FrameLenStatus::TooShort {
            expected: usize::MAX,
        };
    };
    match len.cmp(&expected) {
        std::cmp::Ordering::Equal => FrameLenStatus::Exact,
        std::cmp::Ordering::Greater => FrameLenStatus::TooLong { expected },
        std::cmp::Ordering::Less => FrameLenStatus::TooShort { expected },
    }
}

impl VideoFrame {
    /// 画素データの長さが形式どおり（`幅 × 高さ × 1 画素のバイト数`）か。UI スレッドは
    /// これが真のフレームだけを描く（`egui::ColorImage::from_rgb` は長さが違うと assert で落ちる、#309）
    pub fn has_exact_len(&self) -> bool {
        pixel_len_status(
            self.data.len(),
            self.width,
            self.height,
            self.format.bytes_per_pixel(),
        ) == FrameLenStatus::Exact
    }

    /// RGB24 の画素を返す。RGB ならそのまま、YUY2 なら `scratch` へ変換してそれを返す。
    ///
    /// **録画スレッドが呼ぶ。** `scratch` は呼び出し側が使い回す（容量が足りていれば
    /// 確保は起きない）。返す長さは `幅 × 高さ × 3`
    pub fn rgb<'a>(&'a self, scratch: &'a mut Vec<u8>) -> &'a [u8] {
        match self.format {
            PixelFormat::Rgb24 => &self.data,
            PixelFormat::Yuy2(matrix) => {
                yuy2_to_rgb_naive(self.width, self.height, &self.data, &matrix, scratch);
                scratch
            }
        }
    }

    /// RGB24 のフレームにして返す。RGB ならそのまま（`Arc` の複製だけ）、YUY2 なら
    /// CPU で変換した新しいフレームを作る。
    ///
    /// **スクリーンショットの保存スレッドが呼ぶ**（1 枚ごとの確保は気にしない）
    pub fn into_rgb(self: Arc<Self>) -> Arc<VideoFrame> {
        if self.format == PixelFormat::Rgb24 {
            return self;
        }
        let mut data = Vec::new();
        self.rgb(&mut data);
        Arc::new(VideoFrame {
            width: self.width,
            height: self.height,
            data,
            format: PixelFormat::Rgb24,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::color::BT601;
    use super::*;

    fn yuy2_frame(width: usize, height: usize, data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            width,
            height,
            data,
            format: PixelFormat::Yuy2(BT601),
        }
    }

    #[test]
    fn pixel_len_status_counts_bytes_per_pixel() {
        assert_eq!(pixel_len_status(8, 2, 2, 2), FrameLenStatus::Exact);
        assert_eq!(
            pixel_len_status(9, 2, 2, 2),
            FrameLenStatus::TooLong { expected: 8 }
        );
        assert_eq!(
            pixel_len_status(11, 2, 2, 3),
            FrameLenStatus::TooShort { expected: 12 }
        );
        assert_eq!(
            pixel_len_status(0, usize::MAX, 2, 2),
            FrameLenStatus::TooShort {
                expected: usize::MAX
            }
        );
    }

    #[test]
    fn has_exact_len_follows_the_format() {
        // 2x1 の YUY2 は 4 バイト。RGB として数えると 6 バイト要る
        assert!(yuy2_frame(2, 1, vec![16, 128, 16, 128]).has_exact_len());
        assert!(!yuy2_frame(2, 1, vec![16; 6]).has_exact_len());
        let rgb = VideoFrame {
            width: 2,
            height: 1,
            data: vec![0; 6],
            format: PixelFormat::Rgb24,
        };
        assert!(rgb.has_exact_len());
    }

    #[test]
    fn rgb_of_an_rgb_frame_borrows_the_data() {
        let frame = VideoFrame {
            width: 1,
            height: 1,
            data: vec![1, 2, 3],
            format: PixelFormat::Rgb24,
        };
        let mut scratch = Vec::new();
        assert_eq!(frame.rgb(&mut scratch), &[1, 2, 3]);
        assert!(scratch.is_empty(), "RGB なら変換しない");
    }

    #[test]
    fn rgb_of_a_yuy2_frame_converts_with_the_frame_matrix() {
        // 白（Y=235）の 2x1。BT.601 のリミテッドで 254（`convert.rs` のテストと同じ値）
        let frame = yuy2_frame(2, 1, vec![235, 128, 235, 128]);
        let mut scratch = Vec::new();
        assert_eq!(frame.rgb(&mut scratch), &[254; 6]);
    }

    #[test]
    fn into_rgb_keeps_an_rgb_frame_and_converts_a_yuy2_frame() {
        let rgb = Arc::new(VideoFrame {
            width: 1,
            height: 1,
            data: vec![1, 2, 3],
            format: PixelFormat::Rgb24,
        });
        assert!(Arc::ptr_eq(&Arc::clone(&rgb).into_rgb(), &rgb));

        let converted = Arc::new(yuy2_frame(2, 1, vec![16, 128, 16, 128])).into_rgb();
        assert_eq!(converted.format, PixelFormat::Rgb24);
        assert_eq!((converted.width, converted.height), (2, 1));
        assert_eq!(converted.data, vec![0; 6]);
        assert!(converted.has_exact_len());
    }
}
