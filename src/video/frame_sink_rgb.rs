//! `FrameSink` の受け口のうち、係数表を通らないもの。DirectShow の RGB24 の
//! 並べ替え（`push_bgr24`）、MJPEG の展開（`push_mjpeg`）、デコーダが RGB に
//! 直したフレーム（`push_decoded`）。
//!
//! **どれも色空間・色レンジ・映像調整が効かず、統計ではデコーダの経路
//! （`ConvertPath::Fallback`）として数える。** YUY2 / 4:2:0 の受け口と、積んで
//! UI スレッドを起こす本体（`push`）は `frame_sink.rs` に置いてある。状態は
//! `frame_sink.rs` の `FrameSink` が持ち、ここは `impl FrameSink` を足すだけ。
//!
//! 例外の確保（`push_decoded` はデコーダが確保した Vec を受け取り、`push_mjpeg`
//! はデコーダの内部で確保が起きる）は `frame_sink.rs` の冒頭を参照。

use log::warn;
use std::time::Instant;

use super::convert::{bgr24_stride, bgr24_to_rgb, mjpeg_to_rgb};
use super::frame_buffer::{frame_len_status, FrameLenStatus};
use super::frame_format::{ConvertPath, PixelFormat};
use super::frame_sink::FrameSink;

/// デコーダへ倒れたフレームの「色変換」欄に出す文字列。
/// 係数表を選べないので、そのことが分かる文言にしてある
const DECODER_MATRIX_NAME: &str = "（デコーダ任せ）";

/// RGB24 のまま届いたフレームの「色変換」欄に出す文字列（ログ用）
const RGB_MATRIX_NAME: &str = "（RGB のまま）";

impl FrameSink {
    /// DirectShow の RGB24（BGR の並び、行は 4 バイト境界）を RGB に並べ替えて積む。
    ///
    /// **係数表を通らないので、色空間・色レンジ・映像調整は効かない。**
    /// 変換先の Vec は YUY2 と同じく使い回す。データが足りなければ捨てる。
    pub(super) fn push_bgr24(
        &mut self,
        width: usize,
        height: usize,
        bottom_up: bool,
        src: &[u8],
        received_at: Instant,
    ) -> bool {
        let stride = bgr24_stride(width);
        if src.len() < stride * height {
            if self.short_frame_notice.take() {
                warn!(
                    "RGB24 のフレームが短いので破棄した（{}x{} に必要な {} バイトに対し {} バイト）。以降は記録しない",
                    width,
                    height,
                    stride * height,
                    src.len()
                );
            }
            return false;
        }
        let (frame, ()) = self.fill_frame(width, height, PixelFormat::Rgb24, |rgb| {
            bgr24_to_rgb(width, height, stride, bottom_up, src, rgb)
        });
        self.push(
            frame,
            received_at,
            ConvertPath::Fallback,
            "RGB24",
            RGB_MATRIX_NAME,
        )
    }

    /// MJPEG の 1 フレームを展開して積む。
    ///
    /// **この経路でも色空間・色レンジ・映像調整は効かない**（`push_decoded` と
    /// 同じ）。展開先の Vec は使い回すが、デコーダの内部では確保が起きる
    /// （`convert::mjpeg_to_rgb`）。壊れたフレームは捨て、初回だけ記録する。
    /// **捨てるときも展開先は手放さず、次のフレームの変換先として残す。**
    pub(super) fn push_mjpeg(
        &mut self,
        width: usize,
        height: usize,
        src: &[u8],
        received_at: Instant,
    ) -> bool {
        let (frame, decoded) = self.fill_frame(width, height, PixelFormat::Rgb24, |rgb| {
            mjpeg_to_rgb(width, height, src, rgb)
        });
        if let Err(reason) = decoded {
            // 誰にも渡していないので他に持ち主はいない。次のフレームで使い回す
            self.recyclable = Some(frame);
            if self.decode_error_notice.take() {
                warn!(
                    "MJPEG のフレームを展開できないので破棄した（{}x{}、{} バイト）: {}。以降は記録しない",
                    width,
                    height,
                    src.len(),
                    reason
                );
            }
            return false;
        }
        self.push(
            frame,
            received_at,
            ConvertPath::Fallback,
            "MJPEG",
            DECODER_MATRIX_NAME,
        )
    }

    /// デコーダが RGB に直したフレームを積む（汎用パス）。
    ///
    /// **この経路では色空間・色レンジ・映像調整が効かない。** 係数表は
    /// デコーダの内部にあり、外から差し替えられないため。
    /// `source_format` は元のフォーマットの表示名。積めたら `true`。
    ///
    /// **長さを `width * height * 3` に揃えてから積む**（#309）。nokhwa の
    /// YUYV → RGB は出力の長さを解像度ではなく入力の長さから決めるので、
    /// 幅が奇数のときや行に詰め物があるときは合わない Vec が来る。合わない
    /// まま積むと UI スレッドの `ColorImage::from_rgb` の assert で落ちる。
    /// 長ければ切り詰め（`truncate` なので確保は起きない）、短ければ捨てる。
    pub(super) fn push_decoded(
        &mut self,
        width: usize,
        height: usize,
        mut rgb: Vec<u8>,
        received_at: Instant,
        source_format: &'static str,
    ) -> bool {
        match frame_len_status(rgb.len(), width, height) {
            FrameLenStatus::Exact => {}
            FrameLenStatus::TooLong { expected } => {
                if self.decoded_long_notice.take() {
                    warn!(
                        "デコーダが返した {} のフレームが長いので切り詰めた（{}x{} に必要な {} バイトに対し {} バイト）。以降は記録しない",
                        source_format,
                        width,
                        height,
                        expected,
                        rgb.len()
                    );
                }
                rgb.truncate(expected);
            }
            FrameLenStatus::TooShort { expected } => {
                self.decoded_short_drops += 1;
                if self.decoded_short_notice.take() {
                    warn!(
                        "デコーダが返した {} のフレームが短いので破棄した（{}x{} に必要な {} バイトに対し {} バイト）。以降は数えてストリームを閉じるときに記録する",
                        source_format,
                        width,
                        height,
                        expected,
                        rgb.len()
                    );
                }
                return false;
            }
        }
        // デコーダが確保した Vec はそのまま使う。回収したフレームは `Arc` だけを
        // 使い回し、中にあった古い Vec はここ（ロックの外）で手放す。以前は回収
        // せずに積んでいたので、置き換えた 2 世代前のフレームの解放がフレーム
        // バッファのロックの中で起きていた
        let (frame, ()) = self.fill_frame(width, height, PixelFormat::Rgb24, |data| *data = rgb);
        self.push(
            frame,
            received_at,
            ConvertPath::Fallback,
            source_format,
            DECODER_MATRIX_NAME,
        )
    }
}

impl Drop for FrameSink {
    /// ストリームを閉じるとき（フレームを生むスレッドが受け口を手放すとき）に、
    /// 毎フレームは記録しなかった破棄の数をまとめて残す。
    fn drop(&mut self) {
        if self.decoded_short_drops > 0 {
            warn!(
                "デコーダの経路で長さが足りないフレームを {} 枚破棄した",
                self.decoded_short_drops
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repaint::RepaintWaker;
    use crate::video::color::SharedColorConversion;
    use crate::video::frame_buffer::VideoFrames;
    use std::sync::Arc;

    fn sink_for(frames: &VideoFrames) -> FrameSink {
        let color = Arc::new(SharedColorConversion::new());
        FrameSink::new(frames, color, RepaintWaker::default())
    }

    #[test]
    fn frame_sink_push_decoded_counts_as_fallback() {
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(sink.push_decoded(1, 1, vec![1, 2, 3], Instant::now(), "MJPEG"));

        let stats = frames.stats();
        assert_eq!(stats.fast_count, 0);
        assert_eq!(stats.fallback_count, 1);
        assert_eq!(stats.source_format, Some("MJPEG"));
        assert_eq!(frames.latest().expect("積んだ").data, vec![1, 2, 3]);
    }

    #[test]
    fn frame_sink_push_decoded_truncates_a_longer_frame() {
        // 幅 3 の YUYV を nokhwa が RGB にすると、入力 6 バイトから 6 画素ぶん
        // （18 バイト）が返る。3x1 に要るのは 9 バイトなので、先頭へ切り詰めて積む
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);
        let rgb: Vec<u8> = (0..18).collect();

        assert!(sink.push_decoded(3, 1, rgb, Instant::now(), "YUYV"));

        let frame = frames.latest().expect("積んだ");
        assert_eq!((frame.width, frame.height), (3, 1));
        assert_eq!(frame.data, (0..9).collect::<Vec<u8>>());
        assert_eq!(
            frame_len_status(frame.data.len(), frame.width, frame.height),
            FrameLenStatus::Exact
        );
    }

    #[test]
    fn frame_sink_push_decoded_drops_a_shorter_frame_and_counts_it() {
        // 2x2 には 12 バイト要る。足りなければ積まず、捨てた枚数を数える
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(!sink.push_decoded(2, 2, vec![0; 11], Instant::now(), "YUYV"));
        assert!(!sink.push_decoded(2, 2, vec![0; 3], Instant::now(), "YUYV"));

        assert!(frames.latest().is_none());
        assert_eq!(frames.stats().fallback_count, 0);
        assert_eq!(sink.decoded_short_drops, 2);
    }

    #[test]
    fn frame_sink_push_bgr24_reorders_into_rgb() {
        // 1x1 の赤（BGR の並びで 0, 0, 255、詰め物 1 バイト）
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(sink.push_bgr24(1, 1, true, &[0, 0, 255, 0], Instant::now()));

        let frame = frames.latest().expect("積んだフレームが読める");
        assert_eq!(frame.data, vec![255, 0, 0]);
        let stats = frames.stats();
        assert_eq!(stats.fallback_count, 1);
        assert_eq!(stats.source_format, Some("RGB24"));
    }

    #[test]
    fn frame_sink_push_bgr24_short_frame_is_dropped() {
        // 1x2 は詰め物込みで 8 バイト要る
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(!sink.push_bgr24(1, 2, true, &[0, 0, 255, 0], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_mjpeg_broken_frame_is_dropped() {
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(!sink.push_mjpeg(2, 2, &[0xFF, 0xD8], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_mjpeg_decodes_and_stores_the_frame() {
        let rgb = vec![200u8; 2 * 2 * 3];
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode(&rgb, 2, 2, image::ExtendedColorType::Rgb8)
            .expect("JPEG にできる");
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);

        assert!(sink.push_mjpeg(2, 2, &jpeg, Instant::now()));

        let frame = frames.latest().expect("積んだフレームが読める");
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frames.stats().source_format, Some("MJPEG"));
    }

    #[test]
    fn frame_sink_push_mjpeg_keeps_the_buffer_when_decoding_fails() {
        // 展開に失敗しても変換先は捨てず、次のフレームで使い回す
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);
        assert!(!sink.push_mjpeg(2, 1, &[0, 1, 2, 3], Instant::now()));
        assert!(frames.latest().is_none());
        assert!(sink.recyclable.is_some());
    }

    #[test]
    fn frame_sink_push_decoded_takes_the_recyclable_before_the_lock() {
        // デコーダの経路でも回収待ちをロックの前に取り出し、`Arc` を使い回す
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);
        assert!(sink.push_decoded(1, 1, vec![1, 2, 3], Instant::now(), "NV12"));
        let first = Arc::as_ptr(&frames.latest().expect("1 枚目"));
        assert!(sink.push_decoded(1, 1, vec![4, 5, 6], Instant::now(), "NV12"));
        assert!(sink.push_decoded(1, 1, vec![7, 8, 9], Instant::now(), "NV12"));

        let third = frames.latest().expect("3 枚目");
        assert_eq!(Arc::as_ptr(&third), first);
        assert_eq!(third.data, vec![7, 8, 9]);
    }
}
