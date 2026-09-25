//! フレームコールバックの本体。受け取った画素を RGB に直して `FrameBuffer` へ
//! 積み、UI スレッドを起こす。
//!
//! **実機（`capture.rs` の nokhwa のコールバック）とフェイク（`fake.rs` の
//! 生成スレッド）の両方がここを通る。** 以前は nokhwa のクロージャの中に
//! 直接書かれていたため、`nokhwa::Buffer` を作れないフェイクからは同じ変換を
//! 通せなかった。「幅・高さ・バイト列」を受ける形に出してあるのはそのため
//! （`docs/design/device-worker.md` の「フェイクデバイス（#142）の置き場所」）。
//!
//! **毎フレーム呼ばれるので、ロックはフレームバッファの 1 回だけ、
//! アロケーションは置き換えたフレームを回収できなかったときだけにする**
//! （`docs/design/video-pipeline.md`）。

use log::{info, trace, warn};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::color::{adjusted_color_matrix, color_matrix_for, SharedColorConversion};
use super::convert::yuy2_to_rgb_naive;
use super::frame_buffer::{FrameBuffer, VideoFrame, VideoFrames};
use crate::repaint::RepaintWaker;

/// YUY2 の高速パスで積んだフレームに付けるフォーマット名。
/// 設定画面と同じ語彙にそろえてある
const YUY2_FORMAT_NAME: &str = "YUY2";

/// デコーダへ倒れたフレームの「色変換」欄に出す文字列。
/// 係数表を選べないので、そのことが分かる文言にしてある
const DECODER_MATRIX_NAME: &str = "（デコーダ任せ）";

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
            recyclable: None,
            first_frame: FirstTimeOnly::default(),
            short_frame_notice: FirstTimeOnly::default(),
            lock_error_notice: FirstTimeOnly::default(),
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
        let mut rgb = self
            .recyclable
            .take()
            .and_then(|previous| Arc::try_unwrap(previous).ok())
            .map(|previous| previous.data)
            .unwrap_or_default();
        // 入力信号の色空間は通知されないため、設定が「自動」なら
        // 解像度から推定する。レンジは常に設定の値を使う
        let (space, range) = self.color_conversion.load();
        let matrix = adjusted_color_matrix(
            color_matrix_for(width, height, space, range),
            self.color_conversion.load_adjustments(),
        );
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

    /// デコーダが RGB に直したフレームを積む（汎用パス）。
    ///
    /// **この経路では色空間・色レンジ・映像調整が効かない。** 係数表は
    /// デコーダの内部にあり、外から差し替えられないため。
    /// `source_format` は元のフォーマットの表示名。積めたら `true`。
    pub(super) fn push_decoded(
        &mut self,
        width: usize,
        height: usize,
        rgb: Vec<u8>,
        received_at: Instant,
        source_format: &'static str,
    ) -> bool {
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
        }
        pushed
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
}
