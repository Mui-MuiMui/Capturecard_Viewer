//! 録画へ映像を回す差し込み口（`VideoTap`）。
//!
//! フレームコールバック（`FrameSink`）が画面へ置いたのと同じ `Arc<VideoFrame>` を、
//! 録画中だけ SPSC のリングへ積む。**コールバックがするのは `Arc` の複製を
//! 1 つ積むことだけで、待つロックもアロケーションも足さない**
//! （`docs/design/recording.md` の「コールバックから渡す経路」、`docs/design/video-pipeline.md`）。
//!
//! リングは録画を始めるときに録画スレッドが確保して差し込み（`attach`）、
//! 止めるときに抜く（`detach`）。録画していない間はリングが無く、
//! コールバックは旗を 1 つ読んで何もしない。
//!
//! `video` は録画を知らない。ここにあるのは「差し込まれたリングへ積む」ことだけで、
//! 読み手（録画スレッド）は `crate::recording` にある。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::frame_buffer::VideoFrame;

/// リングに積む 1 件。フレームと、それを受け取った時刻（録画の PTS の元）。
pub type TappedFrame = (Arc<VideoFrame>, Instant);

type TapProducer = HeapProd<TappedFrame>;

/// リングの読み手。録画スレッドが持つ。
pub type VideoTapConsumer = HeapCons<TappedFrame>;

/// リングの容量（枚）。
///
/// **小さくしてあるのは、リングの中の `Arc` が `FrameSink` の Vec の回収を妨げるため。**
/// 録画スレッドが手放さないと、表示側で 1 枚ごとに 1080p なら 6MB の確保が起きる。
/// 録画スレッドは取り出したらすぐ NV12 へ直して `Arc` を手放す。
pub const VIDEO_TAP_CAPACITY: usize = 3;

struct TapShared {
    /// リングが差し込まれているか。コールバックはまずこれだけを読む
    attached: AtomicBool,
    /// 書き手。**コールバックは `try_lock` でしか取らない**（取れなければその 1 枚は捨てる）。
    /// 待ってよいのは差し込みと抜き取りをする録画スレッドの側だけ
    slot: Mutex<Option<TapProducer>>,
    /// 録画に回せなかった枚数（リングが満杯、または差し込み・抜き取りと重なった）
    dropped: AtomicU64,
    /// 差し込まれている間に、`FrameSink` が置き換えたフレームの Vec を回収できなかった回数。
    /// 録画スレッドが `Arc` を持ったままだと増える。実機で問題になったら
    /// 生データを渡す方式へ切り替える判断材料にする（`docs/design/recording.md`）
    recycle_misses: AtomicU64,
}

/// 録画へ映像を回す差し込み口。`VideoFrames` の隣に 1 つだけあり、複製は同じものを指す。
#[derive(Clone)]
pub struct VideoTap {
    shared: Arc<TapShared>,
}

impl Default for VideoTap {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoTap {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(TapShared {
                attached: AtomicBool::new(false),
                slot: Mutex::new(None),
                dropped: AtomicU64::new(0),
                recycle_misses: AtomicU64::new(0),
            }),
        }
    }

    /// リングを確保して差し込み、読み手を返す。録画スレッドが録画の開始時に呼ぶ。
    ///
    /// 数えている値（捨てた枚数・回収の失敗）はここで 0 に戻す。同時に録画するのは
    /// 1 本だけなので、差し込んでから抜くまでの値がそのまま 1 回の録画の値になる。
    pub fn attach(&self, capacity: usize) -> VideoTapConsumer {
        let (producer, consumer) = HeapRb::<TappedFrame>::new(capacity.max(1)).split();
        self.shared.dropped.store(0, Ordering::Relaxed);
        self.shared.recycle_misses.store(0, Ordering::Relaxed);
        match self.shared.slot.lock() {
            Ok(mut slot) => *slot = Some(producer),
            // release は panic = "abort" なので毒されない。差し込めなければ
            // 何も届かないだけで、録画スレッドは「映像なし」として終わる
            Err(poisoned) => *poisoned.into_inner() = Some(producer),
        }
        self.shared.attached.store(true, Ordering::Release);
        consumer
    }

    /// リングを抜く。録画スレッドが録画の終わりに呼ぶ。読み手に残っている分は読める。
    pub fn detach(&self) {
        self.shared.attached.store(false, Ordering::Release);
        match self.shared.slot.lock() {
            Ok(mut slot) => *slot = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }

    /// 差し込んでから録画に回せなかった枚数。
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 差し込んでから `FrameSink` が Vec を回収できなかった回数。
    pub fn recycle_misses(&self) -> u64 {
        self.shared.recycle_misses.load(Ordering::Relaxed)
    }

    /// リングが差し込まれているか。コールバックが毎フレーム読む。
    pub(super) fn is_attached(&self) -> bool {
        self.shared.attached.load(Ordering::Acquire)
    }

    /// フレームを 1 枚積む。コールバックから呼ぶ。
    ///
    /// **待たない。** 書き手を取れない（差し込み・抜き取りの最中）かリングが満杯なら、
    /// 積まずに捨てて数える。捨てた `Arc` は画面側がまだ持っているので、
    /// ここで解放は起きない。
    pub(super) fn offer(&self, frame: Arc<VideoFrame>, received_at: Instant) {
        if !self.is_attached() {
            return;
        }
        let pushed = match self.shared.slot.try_lock() {
            Ok(mut slot) => match slot.as_mut() {
                Some(producer) => producer.try_push((frame, received_at)).is_ok(),
                // 旗を読んだあとに抜かれた
                None => false,
            },
            Err(_) => false,
        };
        if !pushed {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `FrameSink` が Vec を回収できなかったことを数える。差し込まれている間だけ。
    pub(super) fn note_recycle_miss(&self) {
        if self.is_attached() {
            self.shared.recycle_misses.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

    fn frame(marker: u8) -> Arc<VideoFrame> {
        Arc::new(VideoFrame {
            width: 2,
            height: 2,
            data: vec![marker; 12],
        })
    }

    #[test]
    fn video_tap_offer_without_attach_does_nothing() {
        let tap = VideoTap::new();
        tap.offer(frame(1), Instant::now());
        // 録画していない間は捨てた数にも入れない
        assert_eq!(tap.dropped(), 0);
    }

    #[test]
    fn video_tap_offer_after_attach_reaches_the_consumer() {
        let tap = VideoTap::new();
        let mut consumer = tap.attach(VIDEO_TAP_CAPACITY);
        let at = Instant::now();
        tap.offer(frame(7), at);

        let (received, received_at) = consumer.try_pop().expect("積んだ 1 枚が読める");
        assert_eq!(received.data, vec![7u8; 12]);
        assert_eq!(received_at, at);
        assert_eq!(tap.dropped(), 0);
    }

    #[test]
    fn video_tap_full_ring_drops_and_counts() {
        let tap = VideoTap::new();
        let mut consumer = tap.attach(3);
        for marker in 0..5 {
            tap.offer(frame(marker), Instant::now());
        }

        // 容量 3 を超えた 2 枚は捨てて数える。待たない
        assert_eq!(tap.dropped(), 2);
        let kept: Vec<u8> = std::iter::from_fn(|| consumer.try_pop())
            .map(|(f, _)| f.data[0])
            .collect();
        assert_eq!(kept, vec![0, 1, 2]);
    }

    #[test]
    fn video_tap_detach_stops_accepting_but_keeps_queued_frames() {
        let tap = VideoTap::new();
        let mut consumer = tap.attach(3);
        tap.offer(frame(1), Instant::now());
        tap.detach();
        tap.offer(frame(2), Instant::now());

        assert!(!tap.is_attached());
        // 抜く前に積んだ分は、録画スレッドが最後に書き切るために読める
        assert_eq!(consumer.try_pop().map(|(f, _)| f.data[0]), Some(1));
        assert!(consumer.try_pop().is_none());
    }

    #[test]
    fn video_tap_attach_resets_counters() {
        let tap = VideoTap::new();
        let _first = tap.attach(1);
        tap.offer(frame(1), Instant::now());
        tap.offer(frame(2), Instant::now());
        tap.note_recycle_miss();
        assert_eq!(tap.dropped(), 1);
        assert_eq!(tap.recycle_misses(), 1);

        tap.detach();
        let _second = tap.attach(1);
        assert_eq!(tap.dropped(), 0);
        assert_eq!(tap.recycle_misses(), 0);
    }

    #[test]
    fn video_tap_recycle_miss_is_counted_only_while_attached() {
        let tap = VideoTap::new();
        tap.note_recycle_miss();
        assert_eq!(tap.recycle_misses(), 0);
    }
}
