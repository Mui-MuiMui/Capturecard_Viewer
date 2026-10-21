//! フレームの到着からテクスチャへ取り込むまでの遅れ（表示までの遅れ、#455）の集計。
//!
//! **UI スレッドだけが持つ。** 測るのは UI スレッドがフレームをテクスチャへ取り込んだ
//! 直後で、到着時刻（`FrameSink` がフレームを受け取った時刻）は `VideoFrames::newer_than`
//! が世代番号と同じロックの中で返す。フレームコールバックの側には何も足していない。
//!
//! 測れるのは「コールバックが呼ばれた → テクスチャを更新した」まで。ボードが取り込んで
//! からコールバックが呼ばれるまでと、テクスチャを更新してから GPU が画面へ出すまでは
//! 含まない（`docs/design/video-pipeline.md` の「表示までの遅れの計測」）。

use crate::i18n::{self, Text};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 統計 OSD と「接続状態」タブに出す集計の窓。直近この長さに取り込んだフレームを見る
const DISPLAY_LATENCY_WINDOW: Duration = Duration::from_secs(1);

/// ログへ出す間隔。音声の観測値のログ（`app::worker_audio_timers`）と揃える
const DISPLAY_LATENCY_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 既定のログレベル（INFO）で出す要約の間隔（#501）。30 秒ごとのログは `debug` なので、
/// 利用者の手元のログには残らない。長時間動かしたときに表示の側の値が時間とともに
/// 変わったかを、既定のログから追えるようにする。1 時間に 12 行
const DISPLAY_SUMMARY_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// 遅れの集計。単位はミリ秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySummary {
    pub average_ms: f32,
    pub max_ms: f32,
    /// 集計に使ったフレームの数
    pub samples: usize,
}

/// ログへ出す 30 秒ぶんの集計。遅れに加えて、再描画の間隔の判断（`crate::repaint`、#459）を
/// 確かめるための数を持つ。最小化を挟んだ窓ではどちらも大きく出る（最小化中も 1 秒ごとの
/// `update()` が数えられ、取り込むたびに世代が飛ぶ。`docs/design/video-pipeline.md`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyLog {
    pub latency: LatencySummary,
    /// 取り込む前に次のフレームで上書きされた枚数（世代番号が飛んだ数）
    pub skipped_frames: u64,
    /// 新着が無いまま回った `update()` の回数
    pub idle_passes: u64,
}

/// INFO で出す 5 分ぶんの要約（#501）。30 秒の `LatencyLog` と同じ数に、取り込んだ速さと
/// 描画の CPU 時間を足したもの。
///
/// 描画の CPU 時間は eframe の `IntegrationInfo::cpu_usage`（`update()`・テッセレーション・
/// 描画のコールバックを含み、swap / present の待ちを含まない）。**取り込みの速さが落ちて
/// 上書きが増えたのに CPU 時間が変わらなければ、UI スレッドは CPU の外（present の待ち
/// など）で止まっている**と読める
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplaySummaryLog {
    pub latency: LatencySummary,
    /// 窓の長さ。取り込んだ速さ（`frames_per_second`）の分母
    pub elapsed: Duration,
    pub skipped_frames: u64,
    pub idle_passes: u64,
    /// 描画の CPU 時間。`update()` が 1 回も来ていなければ `None`
    pub frame_time: Option<LatencySummary>,
}

impl DisplaySummaryLog {
    /// テクスチャへ取り込んだ速さ（枚/秒）。窓の長さが 0 なら 0
    pub fn frames_per_second(&self) -> f32 {
        let seconds = self.elapsed.as_secs_f32();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.latency.samples as f32 / seconds
    }
}

/// 統計 OSD と「接続状態」タブに出す行。取り込んだフレームが無ければ数値を出さない
pub fn format_display_latency(summary: Option<LatencySummary>) -> String {
    match summary {
        Some(summary) => i18n::stats_display_latency(summary.average_ms, summary.max_ms),
        None => Text::StatsDisplayLatencyPending.get().to_string(),
    }
}

/// 平均と最大を足し込む。窓の中身を持たないログの側で使う
#[derive(Debug, Clone, Copy, Default)]
struct Accumulator {
    samples: usize,
    sum_ms: f64,
    max_ms: f32,
}

impl Accumulator {
    fn add(&mut self, ms: f32) {
        self.samples += 1;
        self.sum_ms += f64::from(ms);
        self.max_ms = self.max_ms.max(ms);
    }

    fn summary(&self) -> Option<LatencySummary> {
        (self.samples > 0).then(|| LatencySummary {
            average_ms: (self.sum_ms / self.samples as f64) as f32,
            max_ms: self.max_ms,
            samples: self.samples,
        })
    }
}

/// 表示までの遅れの集計。`CaptureCardViewer` が 1 つ持つ。
#[derive(Debug, Default)]
pub struct DisplayLatency {
    // 直近 `DISPLAY_LATENCY_WINDOW` に取り込んだフレームの (取り込んだ時刻, 遅れ)。
    // 取り込むたびに窓の外を捨てるので、長さは fps 程度に収まる
    recent: VecDeque<(Instant, f32)>,
    // ログの窓の始まりと、そこからの集計
    log_started: Option<Instant>,
    log: Accumulator,
    log_skipped_frames: u64,
    log_idle_passes: u64,
    // INFO で出す 5 分の要約（#501）の窓の始まりと、そこからの集計
    summary_started: Option<Instant>,
    summary: Accumulator,
    summary_skipped_frames: u64,
    summary_idle_passes: u64,
    summary_frame_time: Accumulator,
}

impl DisplayLatency {
    /// 新着が無いまま `update()` が回ったことを数える。ログにだけ出す
    pub fn note_idle_pass(&mut self) {
        self.log_idle_passes += 1;
        self.summary_idle_passes += 1;
    }

    /// 直前のフレームの描画の CPU 時間（eframe の `cpu_usage`）を数える。5 分の要約にだけ出す。
    /// 映像を 1 枚も取り込んでいない間（窓が始まっていない間）は数えない
    pub fn note_frame_time(&mut self, frame_time: Duration) {
        if self.summary_started.is_some() {
            self.summary_frame_time
                .add(frame_time.as_secs_f32() * 1000.0);
        }
    }

    /// 5 分の要約の窓が `DISPLAY_SUMMARY_LOG_INTERVAL` に達していれば、その集計を返して
    /// 次の窓を始める。`record` の直後に呼ぶ（窓の始まりは `record` が置く）
    pub fn take_summary(&mut self, now: Instant) -> Option<DisplaySummaryLog> {
        let started = self.summary_started?;
        let elapsed = now.saturating_duration_since(started);
        if elapsed < DISPLAY_SUMMARY_LOG_INTERVAL {
            return None;
        }
        let latency = self.summary.summary()?;
        let log = DisplaySummaryLog {
            latency,
            elapsed,
            skipped_frames: std::mem::take(&mut self.summary_skipped_frames),
            idle_passes: std::mem::take(&mut self.summary_idle_passes),
            frame_time: std::mem::take(&mut self.summary_frame_time).summary(),
        };
        self.summary = Accumulator::default();
        self.summary_started = Some(now);
        Some(log)
    }

    /// テクスチャへ取り込んだ時刻 `at` と、その遅れ `latency` を記録する。
    /// `skipped_frames` は前に取り込んだフレームからこのフレームまでに上書きされた枚数。
    ///
    /// ログの窓が `DISPLAY_LATENCY_LOG_INTERVAL` に達していれば、その窓の集計を返して
    /// 次の窓を始める。呼び出し側はそれをログへ出す。
    pub fn record(
        &mut self,
        at: Instant,
        latency: Duration,
        skipped_frames: u64,
    ) -> Option<LatencyLog> {
        let ms = latency.as_secs_f32() * 1000.0;
        // 前の記録からログの窓の長さ以上空いた（映像が長く止まっていた）なら、止まる前の
        // 集計は捨てて窓を張り直す。残すと再接続直後の 1 枚で止まる前の集計が出て、
        // 再接続の前後の遅れが 1 行に混ざる
        let stalled = self.recent.back().is_some_and(|&(last, _)| {
            at.saturating_duration_since(last) >= DISPLAY_LATENCY_LOG_INTERVAL
        });
        if stalled {
            self.log = Accumulator::default();
            self.log_started = None;
            self.log_skipped_frames = 0;
            self.log_idle_passes = 0;
            // 5 分の要約も同じ理由で張り直す。止まっていた間を窓に含めると、取り込んだ
            // 速さが止まっていた時間のぶん低く出る
            self.summary = Accumulator::default();
            self.summary_started = None;
            self.summary_skipped_frames = 0;
            self.summary_idle_passes = 0;
            self.summary_frame_time = Accumulator::default();
        }
        while let Some(&(oldest, _)) = self.recent.front() {
            if at.saturating_duration_since(oldest) > DISPLAY_LATENCY_WINDOW {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        self.recent.push_back((at, ms));

        self.summary_started.get_or_insert(at);
        self.summary.add(ms);
        self.summary_skipped_frames += skipped_frames;

        let started = *self.log_started.get_or_insert(at);
        self.log.add(ms);
        self.log_skipped_frames += skipped_frames;
        if at.saturating_duration_since(started) < DISPLAY_LATENCY_LOG_INTERVAL {
            return None;
        }
        // 直前に足したので必ずある
        let latency = self.log.summary()?;
        let log = LatencyLog {
            latency,
            skipped_frames: std::mem::take(&mut self.log_skipped_frames),
            idle_passes: std::mem::take(&mut self.log_idle_passes),
        };
        self.log = Accumulator::default();
        self.log_started = Some(at);
        Some(log)
    }

    /// `now` から見て直近 `DISPLAY_LATENCY_WINDOW` の集計。取り込んだフレームが無ければ `None`。
    ///
    /// 映像が止まると窓が空になり `None` へ戻る。止まる前の値を出し続けない。
    pub fn recent(&self, now: Instant) -> Option<LatencySummary> {
        let mut window = Accumulator::default();
        for &(at, ms) in &self.recent {
            if now.saturating_duration_since(at) <= DISPLAY_LATENCY_WINDOW {
                window.add(ms);
            }
        }
        window.summary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn recent_without_frames_is_none() {
        let latency = DisplayLatency::default();
        assert_eq!(latency.recent(Instant::now()), None);
    }

    #[test]
    fn recent_reports_average_and_max_within_the_window() {
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(2), 0);
        latency.record(base + ms(16), ms(4), 0);
        latency.record(base + ms(33), ms(12), 0);

        let summary = latency.recent(base + ms(40)).expect("3 枚取り込んだ");
        assert_eq!(summary.samples, 3);
        assert!((summary.average_ms - 6.0).abs() < 1e-3, "{summary:?}");
        assert!((summary.max_ms - 12.0).abs() < 1e-3, "{summary:?}");
    }

    #[test]
    fn recent_drops_frames_older_than_the_window() {
        // 1 秒より前の大きな遅れが、いまの最大に残らない
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(50), 0);
        latency.record(base + ms(1_500), ms(3), 0);

        let summary = latency.recent(base + ms(1_500)).expect("1 枚は窓の中");
        assert_eq!(summary.samples, 1);
        assert!((summary.max_ms - 3.0).abs() < 1e-3, "{summary:?}");
    }

    #[test]
    fn recent_becomes_none_after_the_video_stops() {
        // 取り込みが止まったら、止まる前の値を出し続けない
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(5), 0);

        assert!(latency.recent(base + ms(1_000)).is_some(), "窓の端は含む");
        assert_eq!(latency.recent(base + ms(1_001)), None);
    }

    #[test]
    fn recent_keeps_only_about_one_window_of_samples() {
        // 60fps で 10 秒取り込んでも、持つのは直近 1 秒ぶんだけ
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        for i in 0..600u64 {
            latency.record(base + Duration::from_micros(i * 16_667), ms(1), 0);
        }
        assert!(latency.recent.len() <= 62, "{}", latency.recent.len());
    }

    #[test]
    fn format_display_latency_shows_numbers_only_with_frames() {
        let pending = format_display_latency(None);
        assert_eq!(pending, Text::StatsDisplayLatencyPending.get());

        let line = format_display_latency(Some(LatencySummary {
            average_ms: 3.24,
            max_ms: 8.0,
            samples: 60,
        }));
        assert_eq!(line, i18n::stats_display_latency(3.24, 8.0));
        assert!(line.contains("3.2"), "{line}");
        assert!(line.contains("8.0"), "{line}");
    }

    #[test]
    fn record_restarts_the_log_window_after_a_long_stall() {
        // 止まる前の 10 秒ぶんを、再接続直後の 1 枚で出さない
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(50), 0);
        latency.record(base + ms(10_000), ms(50), 0);

        let resumed = base + ms(60_000);
        assert_eq!(latency.record(resumed, ms(2), 0), None, "窓を張り直す");
        assert_eq!(latency.record(resumed + ms(15_000), ms(3), 0), None);
        let summary = latency
            .record(resumed + ms(30_000), ms(4), 0)
            .expect("張り直してから 30 秒")
            .latency;
        assert_eq!(summary.samples, 3);
        assert!((summary.max_ms - 4.0).abs() < 1e-3, "{summary:?}");
    }

    #[test]
    fn record_returns_the_log_summary_once_per_interval() {
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        assert_eq!(latency.record(base, ms(2), 0), None, "窓の始まり");
        assert_eq!(latency.record(base + ms(29_999), ms(10), 0), None);

        let summary = latency
            .record(base + ms(30_000), ms(6), 0)
            .expect("30 秒に達したら出す")
            .latency;
        assert_eq!(summary.samples, 3);
        assert!((summary.average_ms - 6.0).abs() < 1e-3, "{summary:?}");
        assert!((summary.max_ms - 10.0).abs() < 1e-3, "{summary:?}");

        // 次の窓は出したところから数え直す。前の窓の最大を持ち越さない
        assert_eq!(latency.record(base + ms(59_999), ms(1), 0), None);
        let next = latency
            .record(base + ms(60_000), ms(1), 0)
            .expect("次の 30 秒")
            .latency;
        assert_eq!(next.samples, 2);
        assert!((next.max_ms - 1.0).abs() < 1e-3, "{next:?}");
    }

    #[test]
    fn record_reports_skipped_frames_and_idle_passes_per_window() {
        // 再描画の間隔の判断（#459）を確かめる数。窓ごとに数え直す
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(2), 0);
        latency.note_idle_pass();
        latency.record(base + ms(10_000), ms(2), 2);
        latency.note_idle_pass();
        let log = latency
            .record(base + ms(30_000), ms(2), 1)
            .expect("30 秒に達したら出す");
        assert_eq!(log.skipped_frames, 3);
        assert_eq!(log.idle_passes, 2);

        latency.record(base + ms(45_000), ms(2), 0);
        let next = latency
            .record(base + ms(60_000), ms(2), 0)
            .expect("次の 30 秒");
        assert_eq!(next.skipped_frames, 0, "前の窓を持ち越している");
        assert_eq!(next.idle_passes, 0, "前の窓を持ち越している");
    }

    #[test]
    fn record_drops_the_counts_before_a_long_stall() {
        // 止まっている間に回った update() を、再接続後の窓に混ぜない
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(2), 5);
        latency.note_idle_pass();

        let resumed = base + ms(60_000);
        latency.record(resumed, ms(2), 0);
        latency.record(resumed + ms(15_000), ms(2), 0);
        let log = latency
            .record(resumed + ms(30_000), ms(2), 0)
            .expect("張り直してから 30 秒");
        assert_eq!(log.skipped_frames, 0);
        assert_eq!(log.idle_passes, 0);
    }

    #[test]
    fn take_summary_before_any_frame_is_none() {
        let mut latency = DisplayLatency::default();
        latency.note_idle_pass();
        latency.note_frame_time(ms(5));
        assert_eq!(latency.take_summary(Instant::now()), None);
    }

    #[test]
    fn take_summary_returns_once_per_five_minutes_with_rate_and_frame_time() {
        // 長時間の劣化（#501）を INFO で追うための要約。窓は 5 分
        // 20 秒ごとに 1 枚（30 秒以上空けると止まったとみなされて窓を張り直す）。
        // 0 秒から 280 秒までの 15 枚は遅れ 2ms、上書き 1 枚ずつ
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        for i in 0..15u64 {
            latency.record(base + ms(i * 20_000), ms(2), 1);
        }
        latency.note_frame_time(ms(4));
        latency.note_idle_pass();
        latency.note_frame_time(ms(8));
        assert_eq!(
            latency.take_summary(base + ms(299_999)),
            None,
            "5 分に届かない"
        );

        latency.record(base + ms(300_000), ms(18), 0);
        let summary = latency
            .take_summary(base + ms(300_000))
            .expect("5 分に達したら出す");
        assert_eq!(summary.latency.samples, 16);
        assert!(
            (summary.latency.average_ms - 3.0).abs() < 1e-3,
            "{summary:?}"
        );
        assert!((summary.latency.max_ms - 18.0).abs() < 1e-3, "{summary:?}");
        // 15 枚目まで 1 枚ずつ
        assert_eq!(summary.skipped_frames, 15);
        assert_eq!(summary.idle_passes, 1);
        assert_eq!(summary.elapsed, ms(300_000));
        let rate = summary.frames_per_second();
        assert!((rate - 16.0 / 300.0).abs() < 1e-6, "{rate}");
        let frame_time = summary.frame_time.expect("update() は 2 回来た");
        assert_eq!(frame_time.samples, 2);
        assert!((frame_time.average_ms - 6.0).abs() < 1e-3, "{frame_time:?}");
        assert!((frame_time.max_ms - 8.0).abs() < 1e-3, "{frame_time:?}");

        // 次の窓は出したところから数え直す。前の窓の数を持ち越さない
        latency.record(base + ms(310_000), ms(1), 0);
        let next = latency.take_summary(base + ms(600_000)).expect("次の 5 分");
        assert_eq!(next.latency.samples, 1);
        assert_eq!(next.skipped_frames, 0);
        assert_eq!(next.idle_passes, 0);
        assert_eq!(next.frame_time, None);
    }

    #[test]
    fn take_summary_restarts_after_a_long_stall() {
        // 映像が止まっていた間を窓に含めない。含めると取り込んだ速さが低く出る
        let base = Instant::now();
        let mut latency = DisplayLatency::default();
        latency.record(base, ms(50), 7);
        latency.note_frame_time(ms(40));

        // 再開してからは 20 秒ごとに 1 枚（0 秒から 300 秒までの 16 枚）
        let resumed = base + ms(200_000);
        for i in 0..15u64 {
            latency.record(resumed + ms(i * 20_000), ms(2), 0);
        }
        assert_eq!(latency.take_summary(resumed + ms(299_999)), None);
        latency.record(resumed + ms(300_000), ms(2), 0);
        let summary = latency
            .take_summary(resumed + ms(300_000))
            .expect("張り直してから 5 分");
        assert_eq!(summary.latency.samples, 16);
        assert!((summary.latency.max_ms - 2.0).abs() < 1e-3, "{summary:?}");
        assert_eq!(summary.skipped_frames, 0);
        assert_eq!(summary.frame_time, None);
    }

    #[test]
    fn frames_per_second_of_an_empty_window_is_zero() {
        let summary = DisplaySummaryLog {
            latency: LatencySummary {
                average_ms: 1.0,
                max_ms: 1.0,
                samples: 1,
            },
            elapsed: Duration::ZERO,
            skipped_frames: 0,
            idle_passes: 0,
            frame_time: None,
        };
        assert_eq!(summary.frames_per_second(), 0.0);
    }
}
