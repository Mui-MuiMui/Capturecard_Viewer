//! 映像と音声の PTS（表示時刻）の付け方。MF の時間の単位は 100ns。
//!
//! 基準は録画を始めた時刻 `t0`（録画スレッドがリングを差し込んだ時点）。
//!
//! - 映像: **PTS = フレームを受け取った時刻 − `t0`**。受け取った時刻は `FrameSink` が
//!   持っている値で、変換時間の揺れを含まない
//! - 音声: **PTS = 書いた出力フレーム数 ÷ 48000**。途切れたときだけ、受け取った時刻から
//!   無音を足すか先頭を削って揃える（下の「音声（②）」）
//!
//! どちらも判定と計算だけの純粋関数で、状態を持つのは `PtsClock`（映像）と
//! `super::audio::AudioTrack`（音声）。`docs/design/recording.md` の「PTS」。

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

    /// `received_at` が `t0` 以降か（PTS を付けられるか）。時計は進めない。
    pub(super) fn accepts(&self, received_at: Instant) -> bool {
        received_at >= self.t0
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

// ---- 音声（②） ----
//
// **音声の PTS = 書いた出力フレーム数 ÷ 48000。** 起点は t0（PTS 0）に固定し、
// サンプルの並びが途切れたとき（録画の開始、開き直し、リングの溢れ、音声が
// 来なくなって無音で埋めたあと）だけ、次のサンプルを受け取った時刻と
// 「次に書く位置」を比べて、無音を足すか先頭を削って揃える。PTS だけを飛ばすと、
// AAC のフレーム列は連続したまま時刻だけが食い違うため（`docs/design/recording.md`）。
// 途切れずに続く間は付け直さないので、入力デバイスの時計と PC の時計の差
// （ドリフト）はそのまま溜まる。②では直さず、停止時に `DriftSpan` の値を残す。

/// 録画の音声のレート。Microsoft の AAC エンコーダが受け取る 16bit PCM の
/// 44.1kHz / 48kHz のうち 48kHz に決め打ちする
pub(super) const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// 録画の音声のチャンネル数
pub(super) const AUDIO_CHANNELS: u16 = 2;

/// 付け直すときに、これより小さいずれは直さない（100ns 単位で 15ms）。
///
/// 「最後に積んだ時刻」と「累計」は同時に読めないので、入力コールバックの周期
/// （WASAPI の共有モードで 10ms 前後）ぶんの誤差がある。その範囲で無音を足したり
/// 削ったりすると、揃えるどころか途切れを増やすだけになる。
pub(super) const ALIGN_TOLERANCE: i64 = 150_000;

/// 最後に積んでからこれだけ経っても次が来なければ、音声が来ていないとみなして
/// 無音で埋める（100ns 単位で 200ms）。埋めるのもこの長さだけ手前まで。
/// まだ届いていないだけのサンプルと重ねないため。
pub(super) const AUDIO_STALE: i64 = 2_000_000;

/// 48kHz の出力フレーム数を 100ns 単位の時間にする。
pub(super) fn audio_units(frames: u64) -> i64 {
    let units = u128::from(frames) * UNITS_PER_SECOND as u128 / u128::from(AUDIO_SAMPLE_RATE);
    i64::try_from(units).unwrap_or(i64::MAX)
}

/// 100ns 単位の長さに、`rate` で何フレーム入るか（切り捨て）。0 以下なら 0。
pub(super) fn frames_in(units: i64, rate: u32) -> u64 {
    if units <= 0 {
        return 0;
    }
    let frames = units as u128 * u128::from(rate) / UNITS_PER_SECOND as u128;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

/// 入力コールバックが書いた「最後に積んだ時刻」と「累計のサンプル数」。
/// リングの中のサンプルをいつ受け取ったかを逆算するのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TapTiming {
    /// 最後に積んだ時刻（t0 からの 100ns。t0 より前なら負）
    pub(super) last_push: i64,
    /// 累計のサンプル数（チャンネルをまたいだ個数）
    pub(super) samples_total: u64,
    pub(super) sample_rate: u32,
    pub(super) channels: u16,
}

impl TapTiming {
    /// 累計で `index` 番目のサンプルを受け取った時刻（t0 からの 100ns）。
    /// 最後に積んだ時刻 −（累計 − `index`）÷ チャンネル数 ÷ レート。
    pub(super) fn time_of(&self, index: u64) -> i64 {
        let behind = self.samples_total.saturating_sub(index);
        let per_second = u128::from(self.sample_rate.max(1)) * u128::from(self.channels.max(1));
        let units = u128::from(behind) * UNITS_PER_SECOND as u128 / per_second;
        self.last_push
            .saturating_sub(i64::try_from(units).unwrap_or(i64::MAX))
    }
}

/// 途切れたあと、次のサンプルをどう揃えるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Alignment {
    /// ずれは許す範囲。そのまま続ける
    Keep,
    /// 次のサンプルが「次に書く位置」より後に届いた。出力（48kHz）でこのフレーム数の無音を先に書く
    InsertSilence { frames: u64 },
    /// 次のサンプルが「次に書く位置」より前に届いた（もう無音で埋めた区間に重なる）。
    /// 入力の先頭からこのフレーム数を捨てる
    Trim { input_frames: u64 },
}

/// 揃え方を決める。`expected` は次に書く位置（書いた出力フレーム数から、t0 からの
/// 100ns）、`actual` は次のサンプルを受け取った時刻（同じ基準）、`input_rate` は入力のレート。
pub(super) fn align(expected: i64, actual: i64, input_rate: u32) -> Alignment {
    let diff = actual.saturating_sub(expected);
    if diff.abs() < ALIGN_TOLERANCE {
        Alignment::Keep
    } else if diff > 0 {
        Alignment::InsertSilence {
            frames: frames_in(diff, AUDIO_SAMPLE_RATE),
        }
    } else {
        Alignment::Trim {
            input_frames: frames_in(-diff, input_rate),
        }
    }
}

/// 音声が来ていないとみなして無音で埋めるなら、埋める先（t0 からの 100ns）。
///
/// 入力の形が分からない（まだ 1 度も開いていない）、まだ 1 度も積んでいない、または
/// 最後に積んでから `AUDIO_STALE` 以上経っていれば `now − AUDIO_STALE` まで埋める。
/// 音声トラックの長さを映像と揃えるため（音声デバイスが無い・開けていない間）。
/// 来ているなら `None`。
pub(super) fn silence_until(now: i64, last_push: Option<i64>, format_known: bool) -> Option<i64> {
    let stale = match (format_known, last_push) {
        (true, Some(last)) => now.saturating_sub(last) >= AUDIO_STALE,
        _ => true,
    };
    stale.then(|| now.saturating_sub(AUDIO_STALE))
}

/// ドリフトの測定。途切れずに続いた区間の、PC の時計での経過と、入力のサンプル数 ÷ レート。
///
/// 映像の PTS は PC の時計で測った到着時刻、音声の PTS はサンプル数から作るので、
/// 2 つの差がそのまま録画の中の音と映像のずれになる。区間は付け直すたびに始め直す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DriftSpan {
    start: TapTiming,
    end: TapTiming,
}

impl DriftSpan {
    pub(super) fn new(timing: TapTiming) -> Self {
        Self {
            start: timing,
            end: timing,
        }
    }

    /// 区間の終わりを進める。
    pub(super) fn update(&mut self, timing: TapTiming) {
        self.end = timing;
    }

    /// `(PC の時計での経過, サンプル数 ÷ レート)`。どちらも 100ns 単位。
    /// 後者が前者より短ければ入力デバイスの時計が遅く、サンプル数で数える音声の PTS が実際の時刻より
    /// 小さくなっていくので、録画の音声は映像より先行していく（#398 の実測で確かめた向き）。
    pub(super) fn measure(&self) -> (i64, i64) {
        let by_clock = self.end.last_push.saturating_sub(self.start.last_push);
        let samples = self
            .end
            .samples_total
            .saturating_sub(self.start.samples_total);
        let per_second =
            u128::from(self.start.sample_rate.max(1)) * u128::from(self.start.channels.max(1));
        let by_samples = u128::from(samples) * UNITS_PER_SECOND as u128 / per_second;
        (by_clock, i64::try_from(by_samples).unwrap_or(i64::MAX))
    }
}

/// `Instant` を t0 からの 100ns にする。t0 より前なら負。
pub(super) fn units_since(t0: Instant, at: Instant) -> i64 {
    match at.checked_duration_since(t0) {
        Some(after) => units_from(after),
        None => -units_from(t0.duration_since(at)),
    }
}

pub(super) fn units_from(duration: Duration) -> i64 {
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

    #[test]
    fn audio_units_converts_48k_frames_to_100ns() {
        assert_eq!(audio_units(0), 0);
        assert_eq!(audio_units(48_000), UNITS_PER_SECOND);
        assert_eq!(audio_units(480), 100_000);
        // 1 フレームは 208.33.. なので切り捨てる。長さは差で取るので誤差は溜まらない
        assert_eq!(audio_units(1), 208);
        assert_eq!(audio_units(48_000 * 3600), 3600 * UNITS_PER_SECOND);
    }

    #[test]
    fn frames_in_counts_whole_frames_and_ignores_negative_lengths() {
        assert_eq!(frames_in(UNITS_PER_SECOND, 48_000), 48_000);
        assert_eq!(frames_in(100_000, 44_100), 441);
        assert_eq!(frames_in(0, 48_000), 0);
        assert_eq!(frames_in(-1, 48_000), 0);
    }

    fn timing(last_push_ms: i64, samples_total: u64, rate: u32, channels: u16) -> TapTiming {
        TapTiming {
            last_push: last_push_ms * 10_000,
            samples_total,
            sample_rate: rate,
            channels,
        }
    }

    #[test]
    fn tap_timing_counts_back_from_the_last_push() {
        // 1000ms に累計 96000 サンプル（48kHz 2ch の 1 秒）まで積んだ
        let timing = timing(1000, 96_000, 48_000, 2);
        assert_eq!(timing.time_of(96_000), 10_000_000);
        // 半分（0.5 秒ぶん）前のサンプルは 500ms に届いた
        assert_eq!(timing.time_of(48_000), 5_000_000);
        // 先頭は 0ms
        assert_eq!(timing.time_of(0), 0);
        // 累計より先の番号は最後に積んだ時刻として扱う（まだ届いていない）
        assert_eq!(timing.time_of(100_000), 10_000_000);
    }

    #[test]
    fn tap_timing_before_t0_is_negative() {
        // 録画を始めた直後に積まれたコールバックには、t0 より前に届いた分が混じる
        let timing = timing(5, 960, 48_000, 1);
        assert_eq!(timing.time_of(0), 50_000 - 200_000);
    }

    #[test]
    fn align_keeps_small_differences() {
        assert_eq!(align(1_000_000, 1_000_000, 48_000), Alignment::Keep);
        assert_eq!(
            align(1_000_000, 1_000_000 + ALIGN_TOLERANCE - 1, 48_000),
            Alignment::Keep
        );
        assert_eq!(
            align(1_000_000, 1_000_000 - ALIGN_TOLERANCE + 1, 48_000),
            Alignment::Keep
        );
    }

    #[test]
    fn align_inserts_silence_when_the_sample_arrived_later() {
        // 次に書く位置が 1 秒、次のサンプルは 1.5 秒に届いた。0.5 秒の無音を足す
        assert_eq!(
            align(10_000_000, 15_000_000, 44_100),
            Alignment::InsertSilence { frames: 24_000 }
        );
    }

    #[test]
    fn align_trims_the_head_when_the_sample_arrived_earlier() {
        // 無音で 1 秒まで埋めたあとに、0.8 秒に届いたサンプルが来た。入力の先頭 0.2 秒を捨てる
        assert_eq!(
            align(10_000_000, 8_000_000, 44_100),
            Alignment::Trim {
                input_frames: 8_820
            }
        );
    }

    #[test]
    fn silence_until_fills_only_when_audio_is_absent_or_stale() {
        let now = 50_000_000;
        // 入力の形が分からない（まだ開いていない）
        assert_eq!(silence_until(now, None, false), Some(now - AUDIO_STALE));
        // 開いたがまだ 1 度も積んでいない
        assert_eq!(silence_until(now, None, true), Some(now - AUDIO_STALE));
        // 最後に積んでから 200ms 以上経った
        assert_eq!(
            silence_until(now, Some(now - AUDIO_STALE), true),
            Some(now - AUDIO_STALE)
        );
        // 来ている
        assert_eq!(silence_until(now, Some(now - 100_000), true), None);
    }

    #[test]
    fn drift_span_compares_the_pc_clock_with_the_sample_count() {
        // 10 秒（PC の時計）の間に、48kHz 2ch で 9.999 秒ぶんしか届かなかった（100ppm 遅い）
        let mut span = DriftSpan::new(timing(1_000, 0, 48_000, 2));
        span.update(timing(11_000, 959_904, 48_000, 2));
        let (by_clock, by_samples) = span.measure();
        assert_eq!(by_clock, 100_000_000);
        assert_eq!(by_samples, 99_990_000);
    }

    #[test]
    fn units_since_is_signed_around_t0() {
        let t0 = Instant::now() + Duration::from_secs(1);
        assert_eq!(units_since(t0, t0 + Duration::from_millis(3)), 30_000);
        assert_eq!(units_since(t0, t0 - Duration::from_millis(3)), -30_000);
    }
}
