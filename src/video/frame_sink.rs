//! フレームコールバックの本体。受け取った画素を RGB に直して `FrameBuffer` へ
//! 積み、UI スレッドを起こす。
//!
//! **実機（`capture.rs` の nokhwa のコールバック）とフェイク（`fake.rs` の
//! 生成スレッド）の両方がここを通る。** 以前は nokhwa のクロージャの中に
//! 直接書かれていたため、`nokhwa::Buffer` を作れないフェイクからは同じ変換を
//! 通せなかった。「幅・高さ・バイト列」を受ける形に出してあるのはそのため
//! （`docs/design/device-worker.md` の「フェイクデバイス（#142）の置き場所」）。
//!
//! **毎フレーム呼ばれるので、ロックはフレームバッファの 1 回だけ（録画中は録画の
//! リングの待たない `try_lock` が 1 回増える）、アロケーションは置き換えたフレームを
//! 回収できなかったときだけにする**
//! （`docs/design/video-pipeline.md`）。

use log::{info, trace, warn};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::color::{adjusted_color_matrix, color_matrix_for, ColorMatrix, SharedColorConversion};
use super::convert::{bgr24_stride, bgr24_to_rgb, mjpeg_to_rgb, yuy2_to_rgb_naive};
use super::frame_buffer::{frame_len_status, FrameBuffer, FrameLenStatus, VideoFrame, VideoFrames};
use super::tap::VideoTap;
use super::yuv420::{yuv420_frame_len, yuv420_to_rgb, Yuv420Layout};
use crate::repaint::RepaintWaker;

/// YUY2 の高速パスで積んだフレームに付けるフォーマット名。
/// 設定画面と同じ語彙にそろえてある
const YUY2_FORMAT_NAME: &str = "YUY2";

/// デコーダへ倒れたフレームの「色変換」欄に出す文字列。
/// 係数表を選べないので、そのことが分かる文言にしてある
const DECODER_MATRIX_NAME: &str = "（デコーダ任せ）";

/// RGB24 のまま届いたフレームの「色変換」欄に出す文字列（ログ用）
const RGB_MATRIX_NAME: &str = "（RGB のまま）";

/// 「最初の 1 回だけ」を判定するフラグ。
///
/// フレームコールバックは 1080p60 なら毎秒 60 回呼ばれるため、到着や
/// フォールバックをそのまま記録するとログが埋まる。初回だけ記録するための
/// 判定をここに閉じ込めて、単体テストできるようにしてある。
#[derive(Debug, Default)]
pub(super) struct FirstTimeOnly {
    fired: bool,
}

impl FirstTimeOnly {
    /// 最初に呼ばれたときだけ `true` を返す。2 回目以降は常に `false`。
    pub(super) fn take(&mut self) -> bool {
        let first = !self.fired;
        self.fired = true;
        first
    }
}

/// 1 本のストリームぶんのフレームの受け口。
///
/// ストリームを開くたびに作り、フレームを生むスレッド（nokhwa のコールバック
/// スレッドか、フェイクの生成スレッド）へ持たせる。**`FirstTimeOnly` の
/// 記録もストリームごと**なので、開き直せば「最初のフレームが届いた」が
/// また 1 回出る。
pub(super) struct FrameSink {
    buffer: Arc<Mutex<FrameBuffer>>,
    color_conversion: Arc<SharedColorConversion>,
    /// フレームを置いたことを UI スレッドへ知らせる窓口。
    /// これが無いと、UI 側は保険の間隔でしか新着を見に来ない
    repaint_waker: RepaintWaker,
    /// 録画へ映像を回す差し込み口。録画中だけ、画面へ置いたのと同じ `Arc` を積む
    tap: VideoTap,
    /// 直前に置き換えられたフレーム。UI スレッドが手放していれば
    /// 中の Vec を次の変換先として回収し、毎フレームの確保を避ける。
    /// 1 世代ぶん遅らせて回収するのは、置き換えた直後のフレームは
    /// UI スレッドがテクスチャ化のために掴んでいることが多いため。
    recyclable: Option<Arc<VideoFrame>>,
    // 毎フレーム流れる事象のうち、初回だけ記録したいもの。
    // 2 回目以降は trace! に落とすか、何も出さない
    first_frame: FirstTimeOnly,
    short_frame_notice: FirstTimeOnly,
    lock_error_notice: FirstTimeOnly,
    decode_error_notice: FirstTimeOnly,
    decoded_long_notice: FirstTimeOnly,
    decoded_short_notice: FirstTimeOnly,
    /// デコーダの経路で長さが足りずに捨てたフレームの数。
    /// 警告は初回だけなので、何枚捨てたかはストリームを閉じるときに出す（`Drop`）
    decoded_short_drops: u64,
}

impl FrameSink {
    pub(super) fn new(
        frames: &VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: RepaintWaker,
    ) -> Self {
        Self {
            buffer: frames.buffer(),
            color_conversion,
            repaint_waker,
            tap: frames.tap(),
            recyclable: None,
            first_frame: FirstTimeOnly::default(),
            short_frame_notice: FirstTimeOnly::default(),
            lock_error_notice: FirstTimeOnly::default(),
            decode_error_notice: FirstTimeOnly::default(),
            decoded_long_notice: FirstTimeOnly::default(),
            decoded_short_notice: FirstTimeOnly::default(),
            decoded_short_drops: 0,
        }
    }

    /// 次の変換先にする Vec。回収できたものがあれば使い回し、無ければ空
    /// （変換側がリサイズするので、その時だけ確保が起きる）。
    ///
    /// 回収できなかった回数は、録画中だけ `VideoTap` が数える。録画スレッドが
    /// リングから取った `Arc` を持ち続けると増える（`docs/design/recording.md`）。
    fn recycled_buffer(&mut self) -> Vec<u8> {
        match self.recyclable.take().map(Arc::try_unwrap) {
            Some(Ok(previous)) => previous.data,
            Some(Err(_)) => {
                self.tap.note_recycle_miss();
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// YUY2 のフレームを自前の変換（高速パス）で RGB に直して積む。
    ///
    /// `received_at` はフレームを受け取った時刻。変換時間の計測と、
    /// フレーム間隔の基準を兼ねる。
    ///
    /// **データが `width * height * 2` バイトに満たなければ捨てる。**
    /// 画面は止まるが、足りない分を 0 で埋めた画を出すよりよい。
    /// 積めたら `true`。
    pub(super) fn push_yuy2(
        &mut self,
        width: usize,
        height: usize,
        src: &[u8],
        received_at: Instant,
    ) -> bool {
        if src.len() < width * height * 2 {
            // フレームを捨てるので画面が止まる。以降は同じ行が
            // 毎フレーム出るため初回だけ残す
            if self.short_frame_notice.take() {
                warn!(
                    "YUY2 のフレームが短いので破棄した（{}x{} に必要な {} バイトに対し {} バイト）。以降は記録しない",
                    width,
                    height,
                    width * height * 2,
                    src.len()
                );
            }
            return false;
        }

        // 回収できた Vec があれば使い回し、無ければ新規に確保する
        let mut rgb = self.recycled_buffer();
        let matrix = self.current_matrix(width, height);
        yuy2_to_rgb_naive(width, height, src, &matrix, &mut rgb);

        self.push(
            VideoFrame {
                width,
                height,
                data: rgb,
            },
            received_at,
            true,
            YUY2_FORMAT_NAME,
            matrix.name,
        )
    }

    /// いまの設定で使う係数表。色空間・レンジ・映像調整を畳み込んだもの。
    ///
    /// 入力信号の色空間は通知されないため、設定が「自動」なら解像度から
    /// 推定する。レンジは常に設定の値を使う。読むのはアトミックだけ
    fn current_matrix(&self, width: usize, height: usize) -> ColorMatrix {
        let (space, range) = self.color_conversion.load();
        adjusted_color_matrix(
            color_matrix_for(width, height, space, range),
            self.color_conversion.load_adjustments(),
        )
    }

    /// NV12 / I420 / YV12（4:2:0 の YUV）のフレームを RGB に直して積む。
    ///
    /// **YUY2 と同じ係数表を通るので、色空間・色レンジ・映像調整が効く**
    /// （統計でも高速パスとして数える）。変換先の Vec は使い回す。
    /// データが `yuv420_frame_len` に満たなければ捨てる。積めたら `true`。
    pub(super) fn push_yuv420(
        &mut self,
        layout: Yuv420Layout,
        width: usize,
        height: usize,
        src: &[u8],
        received_at: Instant,
    ) -> bool {
        let required = yuv420_frame_len(width, height);
        if src.len() < required {
            if self.short_frame_notice.take() {
                warn!(
                    "{} のフレームが短いので破棄した（{}x{} に必要な {} バイトに対し {} バイト）。以降は記録しない",
                    layout.name(),
                    width,
                    height,
                    required,
                    src.len()
                );
            }
            return false;
        }
        let mut rgb = self.recycled_buffer();
        let matrix = self.current_matrix(width, height);
        yuv420_to_rgb(layout, width, height, src, &matrix, &mut rgb);
        self.push(
            VideoFrame {
                width,
                height,
                data: rgb,
            },
            received_at,
            true,
            layout.name(),
            matrix.name,
        )
    }

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
        let mut rgb = self.recycled_buffer();
        bgr24_to_rgb(width, height, stride, bottom_up, src, &mut rgb);
        self.push(
            VideoFrame {
                width,
                height,
                data: rgb,
            },
            received_at,
            false,
            "RGB24",
            RGB_MATRIX_NAME,
        )
    }

    /// MJPEG の 1 フレームを展開して積む。
    ///
    /// **この経路でも色空間・色レンジ・映像調整は効かない**（`push_decoded` と
    /// 同じ）。展開先の Vec は使い回すが、デコーダの内部では確保が起きる
    /// （`convert::mjpeg_to_rgb`）。壊れたフレームは捨て、初回だけ記録する。
    pub(super) fn push_mjpeg(
        &mut self,
        width: usize,
        height: usize,
        src: &[u8],
        received_at: Instant,
    ) -> bool {
        let mut rgb = self.recycled_buffer();
        if let Err(reason) = mjpeg_to_rgb(width, height, src, &mut rgb) {
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
            VideoFrame {
                width,
                height,
                data: rgb,
            },
            received_at,
            false,
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
        self.push(
            VideoFrame {
                width,
                height,
                data: rgb,
            },
            received_at,
            false,
            source_format,
            DECODER_MATRIX_NAME,
        )
    }

    /// フレームバッファへ置き、置けたら UI スレッドを起こす。
    fn push(
        &mut self,
        frame: VideoFrame,
        received_at: Instant,
        used_fast: bool,
        source_format: &'static str,
        matrix_name: &'static str,
    ) -> bool {
        let decode_ms = received_at.elapsed().as_secs_f32() * 1000.0;
        let (width, height) = (frame.width, frame.height);
        // `Arc` に包むのはロックの外で済ませる（包むときに小さな確保が起きる）
        let frame = Arc::new(frame);
        // 録画中だけ、同じフレームの `Arc` を複製しておく（参照の数が増えるだけで確保は無い）
        let tapped = self.tap.is_attached().then(|| Arc::clone(&frame));
        // フレームバッファへ置けたか。置けたときだけ UI スレッドを
        // 起こす。**起こすのはロックを手放してから。** 握ったまま
        // 呼ぶと、egui 側の待ちの間このバッファも止まる
        let pushed = match self.buffer.lock() {
            Ok(mut guard) => {
                self.recyclable =
                    guard.push_back(frame, received_at, decode_ms, used_fast, source_format);
                if self.first_frame.take() {
                    // 「接続した」と「映像が出ている」は別物なので、
                    // 最初の 1 枚が届いたことだけは info で残す
                    info!(
                        "最初のフレームが届いた（{}x{}、フォーマット: {}、変換 {:.2}ms、経路: {}、色変換: {}）",
                        width,
                        height,
                        source_format,
                        decode_ms,
                        if used_fast { "高速パス" } else { "デコーダ" },
                        matrix_name
                    );
                } else {
                    trace!(
                        "フレームが届いた（{}x{}、変換 {:.2}ms）",
                        width,
                        height,
                        decode_ms
                    );
                }
                true
            }
            Err(_) => {
                if self.lock_error_notice.take() {
                    warn!(
                        "フレームバッファのロックを取得できないのでフレームを捨てた。以降は記録しない"
                    );
                }
                false
            }
        };

        if pushed {
            // 届いたその場で UI スレッドを起こす。ここが映像の
            // 遅延を決めるので、重い処理を前に挟まないこと
            self.repaint_waker.wake();
            // 録画へ回すのは画面へ出す経路の後ろ。表示の遅延に足さない。
            // 積めなければ捨てて数えるだけで、待たない（`VideoTap::offer`）
            if let Some(frame) = tapped {
                self.tap.offer(frame, received_at);
            }
        }
        pushed
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

    #[test]
    fn first_time_only_first_take_returns_true() {
        let mut flag = FirstTimeOnly::default();
        assert!(flag.take());
    }

    #[test]
    fn first_time_only_subsequent_takes_return_false() {
        // 毎フレーム呼ばれる前提なので、2 回目以降は必ず false になること
        let mut flag = FirstTimeOnly::default();
        flag.take();
        assert!(!flag.take());
        assert!(!flag.take());
        assert!(!flag.take());
    }

    #[test]
    fn first_time_only_instances_are_independent() {
        // 「初回のフレーム」と「初回のフォールバック」を別々に数えるため、
        // 片方を消費してももう片方は初回のまま
        let mut first = FirstTimeOnly::default();
        let mut second = FirstTimeOnly::default();
        assert!(first.take());
        assert!(second.take());
    }

    #[test]
    fn frame_sink_push_yuy2_converts_and_stores_the_frame() {
        // 白（Y=235）の 2x1。BT.601 のリミテッドレンジで 254 になる
        // （`convert.rs` の既存テストと同じ値）
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

        assert!(sink.push_yuy2(2, 1, &[235, 128, 235, 128], Instant::now()));

        let frame = frames.latest().expect("積んだフレームが読める");
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.data, vec![254, 254, 254, 254, 254, 254]);
        let stats = frames.stats();
        assert_eq!(stats.fast_count, 1);
        assert_eq!(stats.source_format, Some("YUY2"));
    }

    #[test]
    fn frame_sink_push_yuy2_short_frame_is_dropped() {
        // 2x2 には 8 バイト要る。足りなければ積まない
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

        assert!(!sink.push_yuy2(2, 2, &[235, 128, 235, 128], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_yuv420_converts_through_the_color_table() {
        // 白（Y=235、Cb = Cr = 128）の 2x2。YUY2 と同じ 254 になり、高速パスとして数える
        for (layout, src, name) in [
            (Yuv420Layout::Nv12, [235, 235, 235, 235, 128, 128], "NV12"),
            (Yuv420Layout::I420, [235, 235, 235, 235, 128, 128], "I420"),
        ] {
            let frames = VideoFrames::new();
            let mut sink = FrameSink::new(
                &frames,
                Arc::new(SharedColorConversion::new()),
                RepaintWaker::default(),
            );

            assert!(sink.push_yuv420(layout, 2, 2, &src, Instant::now()));

            let frame = frames.latest().expect("積んだフレームが読める");
            assert_eq!((frame.width, frame.height), (2, 2));
            assert_eq!(frame.data, vec![254; 12]);
            let stats = frames.stats();
            assert_eq!(stats.fast_count, 1);
            assert_eq!(stats.source_format, Some(name));
        }
    }

    #[test]
    fn frame_sink_push_yuv420_short_frame_is_dropped() {
        // 3x3 には 17 バイト要る（色差は切り上げて 2x2 組）
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

        assert!(!sink.push_yuv420(Yuv420Layout::I420, 3, 3, &[235; 16], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_decoded_counts_as_fallback() {
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

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
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );
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
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

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
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

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
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

        assert!(!sink.push_bgr24(1, 2, true, &[0, 0, 255, 0], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_mjpeg_broken_frame_is_dropped() {
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

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
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );

        assert!(sink.push_mjpeg(2, 2, &jpeg, Instant::now()));

        let frame = frames.latest().expect("積んだフレームが読める");
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frames.stats().source_format, Some("MJPEG"));
    }

    #[test]
    fn frame_sink_push_while_recording_offers_the_same_frame_to_the_tap() {
        // 録画中は、画面へ置いたのと同じ Arc がリングに積まれる（画素を複製しない）
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut consumer = tap.attach(super::super::tap::VIDEO_TAP_CAPACITY);
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );
        let at = Instant::now();

        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], at));

        let (tapped, tapped_at) = consumer.pop().expect("リングに 1 枚積まれている");
        let shown = frames.latest().expect("画面側にも置かれている");
        assert!(Arc::ptr_eq(&tapped, &shown));
        assert_eq!(tapped_at, at);
    }

    #[test]
    fn frame_sink_push_without_recording_leaves_the_tap_empty() {
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));

        // 差し込まれていないので何も積まず、捨てた数にも入れない
        let mut consumer = tap.attach(1);
        assert!(consumer.pop().is_none());
        assert_eq!(tap.dropped(), 0);
    }

    #[test]
    fn frame_sink_recycle_miss_is_counted_while_recording() {
        // 録画スレッドがリングの Arc を持ったままだと、2 世代前の Vec を回収できない
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut consumer = tap.attach(super::super::tap::VIDEO_TAP_CAPACITY);
        let mut sink = FrameSink::new(
            &frames,
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
        );
        for _ in 0..3 {
            assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        }

        // 3 枚目の変換先を用意するときに 1 枚目を回収しようとして、リングが持っているので失敗する
        assert_eq!(tap.recycle_misses(), 1);
        // 取り出して手放せば、以降は回収できる
        while consumer.pop().is_some() {}
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        assert_eq!(tap.recycle_misses(), 1);
    }
}
