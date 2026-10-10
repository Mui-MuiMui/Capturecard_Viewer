//! フレームコールバックの本体。受け取った画素を RGB に直して `FrameBuffer` へ
//! 積み、UI スレッドを起こす。
//!
//! **実機（`capture.rs` の nokhwa のコールバック）とフェイク（`fake.rs` の
//! 生成スレッド）の両方がここを通る。** 以前は nokhwa のクロージャの中に
//! 直接書かれていたため、`nokhwa::Buffer` を作れないフェイクからは同じ変換を
//! 通せなかった。「幅・高さ・バイト列」を受ける形に出してあるのはそのため
//! （`docs/design/device-worker.md` の「フェイクデバイス（#142）の置き場所」）。
//!
//! **毎フレーム呼ばれるので、ロックはフレームバッファの 1 回と、積んだあとに UI
//! スレッドを起こす `RepaintWaker::wake` の中の 2 つ（egui の `Context` の `RwLock` と
//! eframe の `EventLoopProxy` の `Mutex`、#459）だけ（録画中は録画のリングの待たない
//! `try_lock` が 1 回増える）、アロケーションは置き換えたフレームを
//! 回収できなかったときだけにする**（`docs/design/video-pipeline.md`）。回収できた
//! フレームは画素の Vec だけでなく `Arc` ごと使い回す（`fill_recycled`）。例外は
//! デコーダに任せる経路で、`push_decoded` はデコーダが確保した Vec を受け取り、
//! `push_mjpeg` もデコーダの内部で確保が起きる。
//!
//! 係数表を通らない受け口（`push_bgr24` / `push_mjpeg` / `push_decoded`）は
//! `frame_sink_rgb.rs` に分けてある。状態はここの `FrameSink` が持ち、あちらは
//! `impl FrameSink` を足すだけ。
//!
//! **フレームバッファのロックの中では解放も起こさない。** 置き換えたフレームは
//! ロックの中で `recyclable` へ移すだけにし、手放すのは次のフレームでロックの外。

use log::{info, trace, warn};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::color::{adjusted_color_matrix, color_matrix_for, ColorMatrix, SharedColorConversion};
use super::convert::yuy2_to_rgb_naive;
use super::frame_buffer::{fill_recycled, FrameBuffer, VideoFrame, VideoFrames};
use super::frame_format::{ConvertPath, PixelFormat};
use super::tap::VideoTap;
use super::yuv420::{yuv420_frame_len, yuv420_to_rgb, Yuv420Layout};
use crate::repaint::RepaintWaker;

/// YUY2 の高速パスで積んだフレームに付けるフォーマット名。
/// 設定画面と同じ語彙にそろえてある
const YUY2_FORMAT_NAME: &str = "YUY2";

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
    /// `Arc` ごと次の変換先として回収し、毎フレームの確保を避ける。
    /// 1 世代ぶん遅らせて回収するのは、置き換えた直後のフレームは
    /// UI スレッドがテクスチャ化のために掴んでいることが多いため。
    pub(super) recyclable: Option<Arc<VideoFrame>>,
    // 毎フレーム流れる事象のうち、初回だけ記録したいもの。
    // 2 回目以降は trace! に落とすか、何も出さない
    first_frame: FirstTimeOnly,
    pub(super) short_frame_notice: FirstTimeOnly,
    lock_error_notice: FirstTimeOnly,
    pub(super) decode_error_notice: FirstTimeOnly,
    pub(super) decoded_long_notice: FirstTimeOnly,
    pub(super) decoded_short_notice: FirstTimeOnly,
    /// デコーダの経路で長さが足りずに捨てたフレームの数。
    /// 警告は初回だけなので、何枚捨てたかはストリームを閉じるときに出す
    /// （`frame_sink_rgb.rs` の `Drop`）
    pub(super) decoded_short_drops: u64,
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

    /// 回収待ちのフレームを取り出し、使い回せればその中身へ `fill` で書く
    /// （`fill_recycled`）。幅と高さもここで付ける。**フレームバッファのロックの
    /// 前に呼ぶ**ので、置き換えたフレームの解放が起きてもロックの外になる。
    ///
    /// 回収できなかった回数は、録画中だけ `VideoTap` が数える。録画スレッドが
    /// リングから取った `Arc` を持ち続けると増える（`docs/design/recording.md`）。
    pub(super) fn fill_frame<R>(
        &mut self,
        width: usize,
        height: usize,
        format: PixelFormat,
        fill: impl FnOnce(&mut Vec<u8>) -> R,
    ) -> (Arc<VideoFrame>, R) {
        let (frame, result, missed) = fill_recycled(self.recyclable.take(), |target| {
            target.width = width;
            target.height = height;
            target.format = format;
            fill(&mut target.data)
        });
        if missed {
            self.tap.note_recycle_miss();
        }
        (frame, result)
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

        // 回収できたフレームがあれば使い回し、無ければ新規に確保する
        let matrix = self.current_matrix(width, height);
        if self.color_conversion.yuy2_on_gpu() && width.is_multiple_of(2) {
            // GPU で変換する（#456）。YUY2 のまま写して積み、係数表はフレームに持たせる。
            // 写す先は回収した Vec で、RGB のころより短いので容量が足りて確保は起きない。
            // 奇数幅は 2 画素 1 組の境目が行をまたぐので、従来どおり CPU で変換する
            let len = width * height * 2;
            let (frame, ()) = self.fill_frame(width, height, PixelFormat::Yuy2(matrix), |data| {
                data.clear();
                data.extend_from_slice(&src[..len]);
            });
            return self.push(
                frame,
                received_at,
                ConvertPath::Gpu,
                YUY2_FORMAT_NAME,
                matrix.name,
            );
        }
        let (frame, ()) = self.fill_frame(width, height, PixelFormat::Rgb24, |rgb| {
            yuy2_to_rgb_naive(width, height, src, &matrix, rgb)
        });

        self.push(
            frame,
            received_at,
            ConvertPath::Fast,
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
        let matrix = self.current_matrix(width, height);
        let (frame, ()) = self.fill_frame(width, height, PixelFormat::Rgb24, |rgb| {
            yuv420_to_rgb(layout, width, height, src, &matrix, rgb)
        });
        self.push(
            frame,
            received_at,
            ConvertPath::Fast,
            layout.name(),
            matrix.name,
        )
    }

    /// フレームバッファへ置き、置けたら UI スレッドを起こす。
    ///
    /// `frame` は `fill_frame` で作ったもの。回収待ち（`recyclable`）はそこで
    /// 取り出し済みなので、ロックの中の代入で古いフレームが解放されることはない。
    pub(super) fn push(
        &mut self,
        frame: Arc<VideoFrame>,
        received_at: Instant,
        path: ConvertPath,
        source_format: &'static str,
        matrix_name: &'static str,
    ) -> bool {
        let decode_ms = received_at.elapsed().as_secs_f32() * 1000.0;
        let (width, height) = (frame.width, frame.height);
        // 録画中だけ、同じフレームの `Arc` を複製しておく（参照の数が増えるだけで確保は無い）
        let tapped = self.tap.is_attached().then(|| Arc::clone(&frame));
        // フレームバッファへ置けたか。置けたときだけ UI スレッドを
        // 起こす。**起こすのはロックを手放してから。** 握ったまま
        // 呼ぶと、egui 側の待ちの間このバッファも止まる
        let pushed = match self.buffer.lock() {
            Ok(mut guard) => {
                self.recyclable =
                    guard.push_back(frame, received_at, decode_ms, path, source_format);
                if self.first_frame.take() {
                    // 「接続した」と「映像が出ている」は別物なので、
                    // 最初の 1 枚が届いたことだけは info で残す
                    info!(
                        "最初のフレームが届いた（{}x{}、フォーマット: {}、変換 {:.2}ms、経路: {}、色変換: {}）",
                        width,
                        height,
                        source_format,
                        decode_ms,
                        match path {
                            ConvertPath::Fast => "高速パス",
                            ConvertPath::Gpu => "GPU（YUY2 のまま積む）",
                            ConvertPath::Fallback => "デコーダ",
                        },
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
                // 置けなかったフレームは次の変換先として残す（毎フレーム確保し直さない）
                self.recyclable = Some(frame);
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

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

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
        let mut sink = sink_for(&frames);

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
        let mut sink = sink_for(&frames);

        assert!(!sink.push_yuy2(2, 2, &[235, 128, 235, 128], Instant::now()));
        assert!(frames.latest().is_none());
    }

    /// 何も要求していない状態の `egui::Context`。作り方の理由は `repaint.rs` のテストの
    /// `settled_context` にある（生成直後と 1 回目の終わりに再描画を要求するので空回しする）
    fn settled_context() -> eframe::egui::Context {
        let ctx = eframe::egui::Context::default();
        for _ in 0..3 {
            ctx.run_ui(eframe::egui::RawInput::default(), |_| {})
                .drop_without_applying_deltas();
        }
        assert!(!ctx.has_requested_repaint(), "前提が崩れている");
        ctx
    }

    #[test]
    fn frame_sink_push_wakes_the_ui_thread() {
        // 映像の取り込みは到着で起こすことだけが駆動する（#459）。積んだのに
        // 起こさなければ、次の update() まで最大 250ms 待たされる
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(&frames, Arc::new(SharedColorConversion::new()), waker);

        assert!(sink.push_yuy2(2, 1, &[235, 128, 235, 128], Instant::now()));
        assert!(ctx.has_requested_repaint(), "積んだのに起こしていない");
    }

    #[test]
    fn frame_sink_dropped_frame_does_not_wake_the_ui_thread() {
        // 積めなかったフレームで起こすと、新着なしの update() が増えるだけになる
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);
        let frames = VideoFrames::new();
        let mut sink = FrameSink::new(&frames, Arc::new(SharedColorConversion::new()), waker);

        assert!(!sink.push_yuy2(2, 2, &[235, 128, 235, 128], Instant::now()));
        assert!(!ctx.has_requested_repaint(), "積んでいないのに起こしている");
    }

    #[test]
    fn frame_sink_push_yuv420_converts_through_the_color_table() {
        // 白（Y=235、Cb = Cr = 128）の 2x2。YUY2 と同じ 254 になり、高速パスとして数える
        for (layout, src, name) in [
            (Yuv420Layout::Nv12, [235, 235, 235, 235, 128, 128], "NV12"),
            (Yuv420Layout::I420, [235, 235, 235, 235, 128, 128], "I420"),
        ] {
            let frames = VideoFrames::new();
            let mut sink = sink_for(&frames);

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
        let mut sink = sink_for(&frames);

        assert!(!sink.push_yuv420(Yuv420Layout::I420, 3, 3, &[235; 16], Instant::now()));
        assert!(frames.latest().is_none());
    }

    #[test]
    fn frame_sink_push_while_recording_offers_the_same_frame_to_the_tap() {
        // 録画中は、画面へ置いたのと同じ Arc がリングに積まれる（画素を複製しない）
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut consumer = tap.attach(super::super::tap::VIDEO_TAP_CAPACITY);
        let mut sink = sink_for(&frames);
        let at = Instant::now();

        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], at));

        let (tapped, tapped_at) = consumer.try_pop().expect("リングに 1 枚積まれている");
        let shown = frames.latest().expect("画面側にも置かれている");
        assert!(Arc::ptr_eq(&tapped, &shown));
        assert_eq!(tapped_at, at);
    }

    #[test]
    fn frame_sink_push_without_recording_leaves_the_tap_empty() {
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut sink = sink_for(&frames);
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));

        // 差し込まれていないので何も積まず、捨てた数にも入れない
        let mut consumer = tap.attach(1);
        assert!(consumer.try_pop().is_none());
        assert_eq!(tap.dropped(), 0);
    }

    #[test]
    fn frame_sink_recycle_miss_is_counted_while_recording() {
        // 録画スレッドがリングの Arc を持ったままだと、2 世代前の Vec を回収できない
        let frames = VideoFrames::new();
        let tap = frames.tap();
        let mut consumer = tap.attach(super::super::tap::VIDEO_TAP_CAPACITY);
        let mut sink = sink_for(&frames);
        for _ in 0..3 {
            assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        }

        // 3 枚目の変換先を用意するときに 1 枚目を回収しようとして、リングが持っているので失敗する
        assert_eq!(tap.recycle_misses(), 1);
        // 取り出して手放せば、以降は回収できる
        while consumer.try_pop().is_some() {}
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        assert_eq!(tap.recycle_misses(), 1);
    }

    fn sink_for(frames: &VideoFrames) -> FrameSink {
        let color = Arc::new(SharedColorConversion::new());
        FrameSink::new(frames, color, RepaintWaker::default())
    }

    #[test]
    fn frame_sink_reuses_the_frame_replaced_two_pushes_ago() {
        // 1 世代遅らせて回収するので、3 枚目は 1 枚目と同じ `Arc` に書かれる
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        let first = Arc::as_ptr(&frames.latest().expect("1 枚目"));
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        assert!(sink.push_yuy2(2, 1, &[235, 128, 235, 128], Instant::now()));

        let third = frames.latest().expect("3 枚目");
        assert_eq!(Arc::as_ptr(&third), first);
        assert_eq!(third.data, vec![254; 6]);
    }

    #[test]
    fn frame_sink_does_not_overwrite_a_frame_still_held_elsewhere() {
        // UI スレッドが握り続けているフレームは、回収の順番が来ても書き換えない
        let frames = VideoFrames::new();
        let mut sink = sink_for(&frames);
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        let held = frames.latest().expect("1 枚目");
        assert!(sink.push_yuy2(2, 1, &[16, 128, 16, 128], Instant::now()));
        assert!(sink.push_yuy2(2, 1, &[235, 128, 235, 128], Instant::now()));

        assert_eq!(held.data, vec![0; 6]);
        let third = frames.latest().expect("3 枚目");
        assert!(!Arc::ptr_eq(&third, &held));
        assert_eq!(third.data, vec![254; 6]);
    }

    #[test]
    fn frame_sink_push_yuy2_on_gpu_stores_the_raw_frame_with_its_matrix() {
        // GPU で変換するとき（#456）は YUY2 のまま積み、係数表をフレームに持たせる
        let frames = VideoFrames::new();
        let color = Arc::new(SharedColorConversion::new());
        color.set_yuy2_on_gpu(true);
        let mut sink = FrameSink::new(&frames, color, RepaintWaker::default());
        // 末尾の余り（詰め物）は写さない
        assert!(sink.push_yuy2(2, 1, &[235, 128, 235, 128, 9, 9], Instant::now()));

        let frame = frames.latest().expect("積んだ");
        assert_eq!(frame.data, vec![235, 128, 235, 128]);
        assert_eq!(frame.format, PixelFormat::Yuy2(super::super::color::BT601));
        let stats = frames.stats();
        assert!(stats.on_gpu);
        assert_eq!(stats.fast_count, 1);
    }

    #[test]
    fn frame_sink_push_yuy2_on_gpu_converts_an_odd_width_on_the_cpu() {
        // 2 画素 1 組が行をまたぐ奇数幅は GPU へ回さない
        let frames = VideoFrames::new();
        let color = Arc::new(SharedColorConversion::new());
        color.set_yuy2_on_gpu(true);
        let mut sink = FrameSink::new(&frames, color, RepaintWaker::default());
        assert!(sink.push_yuy2(3, 1, &[235, 128, 235, 128, 235, 128], Instant::now()));

        let frame = frames.latest().expect("積んだ");
        assert_eq!(frame.format, PixelFormat::Rgb24);
        assert!(!frames.stats().on_gpu);
    }
}
