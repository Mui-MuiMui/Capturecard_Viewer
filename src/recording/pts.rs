//! 映像の PTS（表示時刻）の付け方。MF の時間の単位は 100ns。
//!
//! 基準は録画を始めた時刻 `t0`（録画スレッドがリングを差し込んだ時点）で、
//! **PTS = フレームを受け取った時刻 − `t0`**。受け取った時刻は `FrameSink` が
//! 持っている値で、変換時間の揺れを含まない（`docs/design/recording.md` の「PTS」）。

use std::time::{Duration, Instant};

/// MF の時間の単位（100ns）で 1 秒
pub(super) const UNITS_PER_SECOND: i64 = 10_000_000;

/// 映像の PTS を付ける。1 回の録画につき 1 つ。
#[derive(Debug, Clone)]
pub(super) struct PtsClock {
    t0: Instant,
    /// 直前に付けた PTS。単調増加を保つために持つ
    last: Option<i64>,
    /// 1 枚ぶんの長さ（公称 fps から）
    sample_duration: i64,
}

impl PtsClock {
    /// `nominal_fps` は 0 なら 1 として扱う（割り算を避けるだけで、実際には来ない）。
    pub(super) fn new(t0: Instant, nominal_fps: u32) -> Self {
        Self {
            t0,
            last: None,
            sample_duration: UNITS_PER_SECOND / i64::from(nominal_fps.max(1)),
        }
    }

    /// `received_at` に受け取ったフレームの PTS。`t0` より前なら `None`（捨てる）。
    ///
    /// 直前以下の値になったら直前 + 1 にする。MP4 のサンプルは時刻が増えていく
    /// 決まりで、同じ時刻に 2 枚届いた（時計の分解能より短い間隔）ときに備える。
    pub(super) fn pts_for(&mut self, received_at: Instant) -> Option<i64> {
        let elapsed = received_at.checked_duration_since(self.t0)?;
        let mut pts = units_from(elapsed);
        if let Some(last) = self.last {
            if pts <= last {
                pts = last + 1;
            }
        }
        self.last = Some(pts);
        Some(pts)
    }

    /// 1 枚ぶんの長さ。MP4 のサンプルの長さは次のサンプルの時刻との差で決まるので、
    /// 実際の間隔が揺れても表示の時間はずれない。
    pub(super) fn sample_duration(&self) -> i64 {
        self.sample_duration
    }

    /// 書いた映像の長さ。最後の PTS に 1 枚ぶんを足したもの。1 枚も無ければ 0。
    pub(super) fn duration(&self) -> Duration {
        match self.last {
            Some(last) => duration_from(last + self.sample_duration),
            None => Duration::ZERO,
        }
    }
}

fn units_from(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

fn duration_from(units: i64) -> Duration {
    Duration::from_nanos(u64::try_from(units).unwrap_or(0) * 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_clock_uses_100ns_units_from_t0() {
        let t0 = Instant::now();
        let mut clock = PtsClock::new(t0, 60);
        assert_eq!(clock.pts_for(t0), Some(0));
        assert_eq!(clock.pts_for(t0 + Duration::from_millis(16)), Some(160_000));
        assert_eq!(clock.pts_for(t0 + Duration::from_secs(2)), Some(20_000_000));
    }

    #[test]
    fn pts_clock_frames_before_t0_are_dropped() {
        let t0 = Instant::now() + Duration::from_secs(1);
        let mut clock = PtsClock::new(t0, 60);
        assert_eq!(clock.pts_for(t0 - Duration::from_millis(1)), None);
        // 捨てたフレームは単調増加の基準にもならない
        assert_eq!(clock.pts_for(t0), Some(0));
    }

    #[test]
    fn pts_clock_keeps_increasing_for_equal_or_earlier_times() {
        let t0 = Instant::now();
        let mut clock = PtsClock::new(t0, 30);
        let at = t0 + Duration::from_millis(10);
        assert_eq!(clock.pts_for(at), Some(100_000));
        assert_eq!(clock.pts_for(at), Some(100_001));
        assert_eq!(clock.pts_for(t0 + Duration::from_millis(5)), Some(100_002));
        assert_eq!(clock.pts_for(t0 + Duration::from_millis(20)), Some(200_000));
    }

    #[test]
    fn pts_clock_sample_duration_follows_the_nominal_fps() {
        let t0 = Instant::now();
        assert_eq!(PtsClock::new(t0, 60).sample_duration(), 166_666);
        assert_eq!(PtsClock::new(t0, 30).sample_duration(), 333_333);
        // 0 は 1 として扱う
        assert_eq!(PtsClock::new(t0, 0).sample_duration(), 10_000_000);
    }

    #[test]
    fn pts_clock_duration_adds_one_sample_to_the_last_pts() {
        let t0 = Instant::now();
        let mut clock = PtsClock::new(t0, 50);
        assert_eq!(clock.duration(), Duration::ZERO);
        clock.pts_for(t0 + Duration::from_secs(1));
        assert_eq!(clock.duration(), Duration::from_millis(1020));
    }
}
