//! 録画へ音声を回す差し込み口（`AudioTap`）。
//!
//! 入力コールバック（`stream::process_input`。cpal とフェイクの両方が通る）が
//! f32 へ直したサンプルを、録画中だけ SPSC のリングへも積む。**積むのは入力の形
//! （入力のレート・チャンネル数でインターリーブ）のまま**で、出力側の変換
//! （`PassthroughConverter`）・クロックドリフト補正・音量・ミュート・パススルーの
//! 無効はどれも効かない。それらは手元で聞くための操作で、出力コールバックにある
//! （`docs/design/recording.md` の「音声は入力コールバックで f32 を複製する」）。
//!
//! 考え方は映像の `VideoTap` と同じ。
//!
//! - リングは録画を始めるときに録画スレッドが確保して差し込み（`attach`）、
//!   止めるときに抜く（`detach`）。録画していない間はリングが無く、
//!   コールバックは旗を 1 つ読んで何もしない
//! - **コールバックは待たない。** 書き手を `try_lock` で取り、取れないか空きが
//!   足りなければ、そのコールバックの分をまるごと捨てて数える。ロックを待つことも
//!   アロケーションもしない（`docs/design/audio.md`）
//!
//! 音声の PTS を決めるため、コールバックは Atomic に「累計のサンプル数」と
//! 「最後に積んだ時刻」を書く。ワーカーはストリームを開くたびに入力のレート・
//! チャンネル数と「開き直しの番号」を書く。録画スレッドはこれらから、リングの
//! 中のサンプルがいつ届いたかを逆算する（`crate::recording` の `pts`）。
//!
//! **数える単位はサンプル（チャンネルをまたいだ f32 の個数）で、フレームではない。**
//! 開き直しでチャンネル数が変わっても、リングの中の位置を同じ物差しで表すため。
//!
//! `audio` は録画を知らない。ここにあるのは「差し込まれたリングへ積む」ことと
//! 観測値だけで、読み手（録画スレッド）は `crate::recording` にある。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ringbuf::traits::{Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

type TapProducer = HeapProd<f32>;

/// リングの読み手。録画スレッドが持つ。
pub type AudioTapConsumer = HeapCons<f32>;

/// 入力の形が分からない（まだ 1 度も開いていない）ときに見積もるリングの形。
/// 録画の出力（48kHz 2ch）と同じにしておく
const FALLBACK_SAMPLES_PER_SECOND: usize = 48_000 * 2;

struct TapShared {
    /// 時刻の基準。`last_push_ns` はここからの経過時間
    base: Instant,
    /// リングが差し込まれているか。コールバックはまずこれだけを読む
    attached: AtomicBool,
    /// 書き手。**コールバックは `try_lock` でしか取らない。** 待ってよいのは
    /// 差し込みと抜き取りをする録画スレッドの側だけ
    slot: Mutex<Option<TapProducer>>,
    /// リングへ積んだサンプルの累計。**戻さない**（録画をまたいで増え続ける）。
    /// 書くのはリングへ積んだコールバックだけ
    samples_total: AtomicU64,
    /// 最後に積んだ時刻（`base` からのナノ秒 + 1）。0 はまだ 1 度も積んでいない
    last_push_ns: AtomicU64,
    /// 差し込んでから、リングが満杯（または差し込み・抜き取りと重なった）で
    /// 捨てたコールバックの回数
    overflows: AtomicU64,
    /// サンプルの並びが途切れた回数（開き直し、リングの溢れ）。録画スレッドは
    /// これが進んだのを見て、PTS を時刻で付け直す
    breaks: AtomicU64,
    /// 最後に途切れた位置（その時点の `samples_total`）。ここより前のサンプルは
    /// 途切れる前の並び・形のもの
    break_at: AtomicU64,
    /// 開き直しの番号。ワーカーがストリームを開くたびに進める。0 はまだ開いていない
    generation: AtomicU64,
    /// 入力のレートとチャンネル数。`generation` と一緒にワーカーが書く
    sample_rate: AtomicU32,
    channels: AtomicU32,
}

/// 録画へ音声を回す差し込み口。UI スレッドが 1 つ作り、`BackendShared` に載せて
/// `AudioCapture` / `FakeAudioCapture` へ渡す。複製は同じものを指す。
///
/// **開き直しても引き継ぐ**（`AudioControls` と同じ）。入力の形と開き直しの番号は
/// ストリームを開くたびに `begin_stream` で書き換える。
#[derive(Clone)]
pub struct AudioTap {
    shared: Arc<TapShared>,
}

/// 差し込んだときの位置。録画スレッドが持つ読み手の起点になる。
pub struct AudioTapAttachment {
    pub consumer: AudioTapConsumer,
    /// リングの先頭のサンプルが、累計で何番目か
    pub start_index: u64,
    /// 差し込んだ時点の途切れの回数。これより後の途切れだけを見る
    pub breaks: u64,
}

/// 録画スレッドが読む観測値の一式。
///
/// 読む順序に意味がある（`AudioTap::snapshot`）。`samples_total` を読んでから
/// `last_push` を読むので、`last_push` は `samples_total` と同じか、1 回ぶん
/// 新しいコールバックの時刻になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTapSnapshot {
    pub breaks: u64,
    pub break_at: u64,
    pub samples_total: u64,
    /// 最後に積んだ時刻（`base` からの経過）。まだ積んでいなければ `None`
    pub last_push: Option<Duration>,
    pub generation: u64,
    /// 入力のレートとチャンネル数。まだ 1 度も開いていなければ `None`
    pub format: Option<(u32, u16)>,
}

impl Default for AudioTap {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioTap {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(TapShared {
                base: Instant::now(),
                attached: AtomicBool::new(false),
                slot: Mutex::new(None),
                samples_total: AtomicU64::new(0),
                last_push_ns: AtomicU64::new(0),
                overflows: AtomicU64::new(0),
                breaks: AtomicU64::new(0),
                break_at: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                sample_rate: AtomicU32::new(0),
                channels: AtomicU32::new(0),
            }),
        }
    }

    /// 時刻の基準。`AudioTapSnapshot::last_push` はここからの経過。
    pub fn base(&self) -> Instant {
        self.shared.base
    }

    /// 入力の形で 1 秒ぶんのリングの容量（サンプル数）。形が分からなければ
    /// 48kHz 2ch として見積もる。**録画の出力（48kHz 2ch）の 1 秒より小さくはしない。**
    /// 小さいレートで開いていたあとで大きいレートへ開き直すと、1 秒に届かなくなるため。
    pub fn one_second_capacity(&self) -> usize {
        let per_second = match self.format() {
            Some((rate, channels)) => (rate as usize).saturating_mul(channels as usize),
            None => FALLBACK_SAMPLES_PER_SECOND,
        };
        per_second.max(FALLBACK_SAMPLES_PER_SECOND)
    }

    /// リングを確保して差し込み、読み手を返す。録画スレッドが録画の開始時に呼ぶ。
    ///
    /// 溢れた回数はここで 0 に戻す。累計のサンプル数は戻さず、リングの先頭が
    /// 累計の何番目かを返す（書き手のロックを握ったまま読むので、コールバックが
    /// 割り込んでずれることはない）。
    pub fn attach(&self, capacity: usize) -> AudioTapAttachment {
        let (producer, consumer) = HeapRb::<f32>::new(capacity.max(1)).split();
        self.shared.overflows.store(0, Ordering::Relaxed);
        let mut slot = match self.shared.slot.lock() {
            Ok(slot) => slot,
            // release は panic = "abort" なので毒されない
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(producer);
        let start_index = self.shared.samples_total.load(Ordering::Acquire);
        let breaks = self.shared.breaks.load(Ordering::Acquire);
        drop(slot);
        self.shared.attached.store(true, Ordering::Release);
        AudioTapAttachment {
            consumer,
            start_index,
            breaks,
        }
    }

    /// リングを抜く。録画スレッドが録画の終わりに呼ぶ。読み手に残っている分は読める。
    pub fn detach(&self) {
        self.shared.attached.store(false, Ordering::Release);
        match self.shared.slot.lock() {
            Ok(mut slot) => *slot = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }

    /// 差し込んでから溢れて捨てたコールバックの回数。
    pub fn overflows(&self) -> u64 {
        self.shared.overflows.load(Ordering::Relaxed)
    }

    /// 入力のレートとチャンネル数。まだ 1 度も開いていなければ `None`。
    pub fn format(&self) -> Option<(u32, u16)> {
        let rate = self.shared.sample_rate.load(Ordering::Acquire);
        let channels = self.shared.channels.load(Ordering::Acquire);
        if rate == 0 || channels == 0 {
            None
        } else {
            Some((rate, channels.min(u32::from(u16::MAX)) as u16))
        }
    }

    /// 観測値をまとめて読む。**読む順序は変えない。**
    ///
    /// 途切れの回数を最初に読み、次に累計、最後に時刻を読む。途切れを見逃した
    /// まま新しいサンプルを数えることが無く、時刻は累計と同じか新しいものになる。
    pub fn snapshot(&self) -> AudioTapSnapshot {
        let breaks = self.shared.breaks.load(Ordering::Acquire);
        let break_at = self.shared.break_at.load(Ordering::Acquire);
        let generation = self.shared.generation.load(Ordering::Acquire);
        let format = self.format();
        let samples_total = self.shared.samples_total.load(Ordering::Acquire);
        let last_push = match self.shared.last_push_ns.load(Ordering::Acquire) {
            0 => None,
            ns => Some(Duration::from_nanos(ns - 1)),
        };
        AudioTapSnapshot {
            breaks,
            break_at,
            samples_total,
            last_push,
            generation,
            format,
        }
    }

    /// ストリームを開くことを知らせる。**ワーカーが、入力のコールバックが動き出す前に呼ぶ。**
    ///
    /// 入力の形を書き、開き直しの番号を進め、ここを途切れの位置として記録する。
    /// 前のストリームは閉じてあるので、この時点の累計が「前の形のサンプル」の終わり。
    pub fn begin_stream(&self, sample_rate: u32, channels: u16) {
        let shared = &self.shared;
        shared.sample_rate.store(sample_rate, Ordering::Release);
        shared
            .channels
            .store(u32::from(channels.max(1)), Ordering::Release);
        shared.generation.fetch_add(1, Ordering::AcqRel);
        shared.break_at.store(
            shared.samples_total.load(Ordering::Acquire),
            Ordering::Release,
        );
        shared.breaks.fetch_add(1, Ordering::AcqRel);
    }

    /// 入力コールバック 1 回ぶんの書き手を取る。`len` はこれから積むサンプル数。
    ///
    /// 差し込まれていなければ `None`（何もしない）。差し込まれているのに書き手を
    /// 取れないか空きが `len` に足りなければ、**そのコールバックの分をまるごと捨てて**
    /// 溢れとして数え、`None` を返す。一部だけ積むと、どこが欠けたか読み手に分からない。
    ///
    /// **待たない・確保しない。** 入力コールバックから呼ぶ。
    pub(super) fn writer(&self, len: usize) -> Option<AudioTapWriter<'_>> {
        if len == 0 || !self.shared.attached.load(Ordering::Acquire) {
            return None;
        }
        let guard = match self.shared.slot.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                self.note_overflow();
                return None;
            }
        };
        match guard.as_ref() {
            Some(producer) if producer.vacant_len() >= len => Some(AudioTapWriter {
                tap: self,
                guard,
                pushed: 0,
            }),
            // 旗を読んだあとに抜かれた。録画は終わるところなので数えない
            None => None,
            Some(_) => {
                drop(guard);
                self.note_overflow();
                None
            }
        }
    }

    /// 入力コールバック 1 回ぶんを積む。`audio` の外（録画スレッドのテスト）から
    /// 入力コールバックの代わりに使う。
    #[cfg(test)]
    pub fn push_for_test(&self, samples: &[f32]) {
        if let Some(mut writer) = self.writer(samples.len()) {
            for &sample in samples {
                writer.push(sample);
            }
        }
    }

    /// 溢れを数え、ここを途切れの位置として記録する。
    fn note_overflow(&self) {
        let shared = &self.shared;
        shared.overflows.fetch_add(1, Ordering::Relaxed);
        shared.break_at.store(
            shared.samples_total.load(Ordering::Acquire),
            Ordering::Release,
        );
        shared.breaks.fetch_add(1, Ordering::AcqRel);
    }
}

/// 入力コールバック 1 回ぶんの書き手。落とすときに累計と時刻を書く。
pub(super) struct AudioTapWriter<'a> {
    tap: &'a AudioTap,
    guard: MutexGuard<'a, Option<TapProducer>>,
    pushed: u64,
}

impl AudioTapWriter<'_> {
    /// 1 サンプル積む。空きは `writer` で確かめてあるので溢れない。
    pub(super) fn push(&mut self, sample: f32) {
        if let Some(producer) = self.guard.as_mut() {
            if producer.try_push(sample).is_ok() {
                self.pushed += 1;
            }
        }
    }
}

impl Drop for AudioTapWriter<'_> {
    /// 積んだ分を累計に足し、時刻を書く。**時刻を先に、累計を後に書く。**
    /// 読み手は累計を先に読むので、読んだ累計に対して時刻が古くなることはない。
    fn drop(&mut self) {
        if self.pushed == 0 {
            return;
        }
        let shared = &self.tap.shared;
        let elapsed = u64::try_from(shared.base.elapsed().as_nanos()).unwrap_or(u64::MAX - 1);
        shared
            .last_push_ns
            .store(elapsed.saturating_add(1), Ordering::Release);
        shared
            .samples_total
            .fetch_add(self.pushed, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

    fn push_all(tap: &AudioTap, samples: &[f32]) {
        if let Some(mut writer) = tap.writer(samples.len()) {
            for &sample in samples {
                writer.push(sample);
            }
        }
    }

    #[test]
    fn audio_tap_writer_without_attach_is_none() {
        let tap = AudioTap::new();
        assert!(tap.writer(4).is_none());
        // 録画していない間は溢れにも数えず、累計も進めない
        assert_eq!(tap.overflows(), 0);
        assert_eq!(tap.snapshot().samples_total, 0);
    }

    #[test]
    fn audio_tap_pushes_reach_the_consumer_and_advance_the_counters() {
        let tap = AudioTap::new();
        let mut attachment = tap.attach(8);
        assert_eq!(attachment.start_index, 0);
        push_all(&tap, &[0.5, -0.5, 0.25, -0.25]);

        let mut read = [0.0f32; 8];
        let count = attachment.consumer.pop_slice(&mut read);
        assert_eq!(&read[..count], &[0.5, -0.5, 0.25, -0.25]);
        let snapshot = tap.snapshot();
        assert_eq!(snapshot.samples_total, 4);
        assert!(snapshot.last_push.is_some());
        assert_eq!(tap.overflows(), 0);
    }

    #[test]
    fn audio_tap_short_of_space_drops_the_whole_callback_and_marks_a_break() {
        let tap = AudioTap::new();
        let mut attachment = tap.attach(4);
        push_all(&tap, &[1.0, 1.0, 1.0]);
        // 空きは 1 しか無いので、2 サンプルのコールバックはまるごと捨てる
        push_all(&tap, &[2.0, 2.0]);

        assert_eq!(tap.overflows(), 1);
        let snapshot = tap.snapshot();
        assert_eq!(snapshot.samples_total, 3);
        assert_eq!(snapshot.breaks, attachment.breaks + 1);
        // 途切れの位置は、捨てた時点の累計（その前の 3 サンプルは続いている）
        assert_eq!(snapshot.break_at, 3);
        let mut read = [0.0f32; 4];
        assert_eq!(attachment.consumer.pop_slice(&mut read), 3);
    }

    #[test]
    fn audio_tap_begin_stream_sets_the_format_and_marks_a_break() {
        let tap = AudioTap::new();
        assert_eq!(tap.format(), None);
        assert_eq!(tap.snapshot().generation, 0);

        let _attachment = tap.attach(16);
        push_all(&tap, &[0.0; 6]);
        tap.begin_stream(44_100, 1);

        let snapshot = tap.snapshot();
        assert_eq!(snapshot.format, Some((44_100, 1)));
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.break_at, 6);
        assert_eq!(snapshot.breaks, 1);
    }

    #[test]
    fn audio_tap_attach_keeps_the_running_total_and_resets_overflows() {
        let tap = AudioTap::new();
        let first = tap.attach(2);
        push_all(&tap, &[0.0, 0.0]);
        push_all(&tap, &[0.0]);
        assert_eq!(tap.overflows(), 1);
        tap.detach();
        drop(first);

        // 累計は録画をまたいで増え続け、新しいリングの先頭はその続きの番号になる
        let second = tap.attach(4);
        assert_eq!(second.start_index, 2);
        assert_eq!(second.breaks, 1);
        assert_eq!(tap.overflows(), 0);
    }

    #[test]
    fn audio_tap_detach_stops_accepting_but_keeps_queued_samples() {
        let tap = AudioTap::new();
        let mut attachment = tap.attach(4);
        push_all(&tap, &[0.1]);
        tap.detach();
        push_all(&tap, &[0.2]);

        let mut read = [0.0f32; 4];
        assert_eq!(attachment.consumer.pop_slice(&mut read), 1);
        assert_eq!(read[0], 0.1);
        assert_eq!(tap.snapshot().samples_total, 1);
    }

    #[test]
    fn audio_tap_one_second_capacity_follows_the_input_but_not_below_48k_stereo() {
        let tap = AudioTap::new();
        assert_eq!(tap.one_second_capacity(), 96_000);
        tap.begin_stream(96_000, 8);
        assert_eq!(tap.one_second_capacity(), 768_000);
        tap.begin_stream(16_000, 1);
        assert_eq!(tap.one_second_capacity(), 96_000);
    }
}
