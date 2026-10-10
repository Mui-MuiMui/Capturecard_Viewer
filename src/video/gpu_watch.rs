//! GPU で YUY2 を変換しているときの性能の見張り（#456）と、ソフトウェア描画の見分け。
//! どちらも判定だけの純粋な部分で、切り替えるのは `super::gpu_yuy2`。
//!
//! **見張りが CPU へ戻すのは、設定が「自動」のときだけ、1 セッションに 1 回だけ。** 戻したら
//! 「性能が戻った」で GPU へ戻す判定は持たない（映像の経路がころころ変わらないように）。
//! GPU へ戻るのは、利用者が設定を変えて適用したときかアプリを起動し直したとき。
//! 閾値と猶予の理由は `docs/design/video-pipeline.md` の「性能の見張り」。

use std::time::{Duration, Instant};

/// 1 フレームの描画（`update()` と描画のコールバックを含む、swap の待ちは含まない）が、
/// 映像の到着間隔の何倍を超えたら「遅い」とみなすか。2 倍を超えると、描画が追いつかずに
/// 映像のフレームを取りこぼし続ける
pub(super) const SLOW_FRAME_RATIO: u32 = 2;

/// 「遅い」がこの時間続いたら CPU へ戻す。途中で 1 フレームでも間に合えば数え直す
pub(super) const SLOW_FOR: Duration = Duration::from_secs(5);

/// 映像が流れ始めてから（または途切れから戻ってから）見張りを始めるまでの猶予。
/// 起動直後・設定の変更直後・デバイスの開き直し直後・最小化からの戻り直後は、
/// テクスチャの作り直しやシェーダーの初回の準備で描画が一時的に重くなる
pub(super) const GRACE: Duration = Duration::from_secs(3);

/// 前の観測からこれより空いたら、映像が途切れていた（最小化・止まっていた）とみなして
/// 猶予から数え直す。映像が流れている間の `update()` はフレームごとに来る
pub(super) const MAX_OBSERVATION_GAP: Duration = Duration::from_millis(500);

/// 1 回の `update()` での観測。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Observation {
    /// 直前のフレームの描画にかかった時間（eframe の `IntegrationInfo::cpu_usage`）
    pub frame_time: Option<Duration>,
    /// 映像の到着間隔（平均）。映像が流れていなければ `None`
    pub frame_interval: Option<Duration>,
}

/// 映像の到着間隔（統計の平均）と最後の到着からの経過から、見張りに使う間隔を決める。
/// 最後の到着から `MAX_OBSERVATION_GAP` を超えていれば、流れていないとして `None`
pub(super) fn watched_interval(
    average_ms: Option<f32>,
    since_last_frame_ms: Option<f32>,
) -> Option<Duration> {
    let average = average_ms.filter(|ms| ms.is_finite() && *ms > 0.0)?;
    let since = since_last_frame_ms?;
    (since <= MAX_OBSERVATION_GAP.as_secs_f32() * 1000.0)
        .then(|| Duration::from_micros((average * 1000.0).round() as u64))
}

/// 描画が遅い状態が続いているかの見張り。
#[derive(Debug, Default)]
pub(super) struct SlowPaintWatch {
    /// 前に観測した時刻。空きが大きければ猶予から数え直す
    last_seen: Option<Instant>,
    /// 映像が流れ始めた時刻（猶予の起点）
    flowing_since: Option<Instant>,
    /// 「遅い」が始まった時刻
    slow_since: Option<Instant>,
}

impl SlowPaintWatch {
    /// 数え直す。設定の変更を適用したときに呼ぶ
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// 観測を 1 つ足す。CPU へ戻すべきなら `true`
    pub(super) fn observe(&mut self, now: Instant, observation: Observation) -> bool {
        let gap = self
            .last_seen
            .replace(now)
            .map(|at| now.saturating_duration_since(at));
        let (Some(frame_time), Some(interval)) =
            (observation.frame_time, observation.frame_interval)
        else {
            // 映像が流れていない。流れ始めたら猶予から数える
            self.flowing_since = None;
            self.slow_since = None;
            return false;
        };
        if gap.is_some_and(|gap| gap > MAX_OBSERVATION_GAP) {
            self.flowing_since = None;
            self.slow_since = None;
        }
        let flowing_since = *self.flowing_since.get_or_insert(now);
        if now.saturating_duration_since(flowing_since) < GRACE {
            return false;
        }
        if frame_time <= interval * SLOW_FRAME_RATIO {
            self.slow_since = None;
            return false;
        }
        let slow_since = *self.slow_since.get_or_insert(now);
        now.saturating_duration_since(slow_since) >= SLOW_FOR
    }
}

/// `GL_RENDERER` の文字列がソフトウェア描画のものか。設定が「自動」なら最初から CPU にする。
/// GPU の無い環境の OpenGL（Windows の GDI Generic、Mesa の llvmpipe / softpipe など）は、
/// シェーダーとテクスチャの転送も CPU で行うので、CPU で変換するより速くならない
pub(super) fn is_software_renderer(renderer: &str) -> bool {
    const SOFTWARE: [&str; 6] = [
        "gdi generic",
        "llvmpipe",
        "softpipe",
        "microsoft basic render driver",
        "swiftshader",
        "software rasterizer",
    ];
    let renderer = renderer.to_ascii_lowercase();
    SOFTWARE.iter().any(|name| renderer.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(16);

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn flowing(frame_time: Duration) -> Observation {
        Observation {
            frame_time: Some(frame_time),
            frame_interval: Some(INTERVAL),
        }
    }

    /// `start` から `until` まで 16ms ごとに同じ観測を足し、最初に `true` を返した時刻
    fn run(
        watch: &mut SlowPaintWatch,
        start: Instant,
        until: Duration,
        observation: Observation,
    ) -> Option<Duration> {
        let mut at = Duration::ZERO;
        while at <= until {
            if watch.observe(start + at, observation) {
                return Some(at);
            }
            at += INTERVAL;
        }
        None
    }

    #[test]
    fn slow_frames_fall_back_only_after_the_grace_and_five_seconds() {
        // 猶予 3 秒のあとから数え始め、5 秒続いた時点で戻す（3 + 5 = 8 秒）
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        let fired = run(&mut watch, start, Duration::from_secs(10), flowing(ms(40)))
            .expect("遅いまま続けば戻す");
        assert!(fired >= GRACE + SLOW_FOR, "{fired:?}");
        assert!(fired < GRACE + SLOW_FOR + INTERVAL * 2, "{fired:?}");
    }

    #[test]
    fn slow_frames_within_the_grace_are_ignored() {
        // 流れ始めの 3 秒はどれだけ遅くても数えない
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        assert_eq!(
            run(&mut watch, start, GRACE - INTERVAL, flowing(ms(500))),
            None
        );
        assert!(watch.slow_since.is_none());
    }

    #[test]
    fn a_single_fast_frame_restarts_the_count() {
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        // 猶予のあと 4 秒遅い
        assert_eq!(
            run(&mut watch, start, GRACE + ms(4000), flowing(ms(40))),
            None
        );
        // 1 フレームだけ間に合う
        assert!(!watch.observe(start + GRACE + ms(4016), flowing(ms(10))));
        // そこから 4.9 秒遅くても、まだ戻さない
        let resumed = start + GRACE + ms(4032);
        let mut at = Duration::ZERO;
        while at < ms(4900) {
            assert!(!watch.observe(resumed + at, flowing(ms(40))), "{at:?}");
            at += INTERVAL;
        }
    }

    #[test]
    fn exactly_twice_the_interval_is_not_slow() {
        // 境界。2 倍ちょうどは間に合っている側
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        assert_eq!(
            run(
                &mut watch,
                start,
                Duration::from_secs(10),
                flowing(INTERVAL * 2)
            ),
            None
        );
    }

    #[test]
    fn a_gap_in_observations_restarts_the_grace() {
        // 最小化などで update() が空いたら、戻ってきてから猶予を置き直す
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        assert_eq!(
            run(&mut watch, start, GRACE + ms(4000), flowing(ms(40))),
            None
        );
        let back = start + GRACE + ms(4000) + ms(1000);
        // 戻ってから 3 秒は数えない。直前まで 4 秒遅かったことも持ち越さない
        assert_eq!(
            run(
                &mut watch,
                back,
                GRACE + SLOW_FOR - ms(100),
                flowing(ms(40))
            ),
            None
        );
    }

    #[test]
    fn no_video_resets_the_watch() {
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        assert_eq!(
            run(&mut watch, start, GRACE + ms(4000), flowing(ms(40))),
            None
        );
        let stopped = Observation {
            frame_time: Some(ms(40)),
            frame_interval: None,
        };
        assert!(!watch.observe(start + GRACE + ms(4016), stopped));
        assert!(watch.flowing_since.is_none() && watch.slow_since.is_none());
    }

    #[test]
    fn reset_restarts_from_the_grace() {
        let mut watch = SlowPaintWatch::default();
        let start = Instant::now();
        assert_eq!(
            run(&mut watch, start, GRACE + ms(4900), flowing(ms(40))),
            None
        );
        watch.reset();
        assert!(!watch.observe(start + GRACE + ms(5000), flowing(ms(40))));
        assert!(watch.slow_since.is_none());
    }

    #[test]
    fn watched_interval_needs_recent_frames() {
        assert_eq!(watched_interval(Some(16.0), Some(5.0)), Some(ms(16)));
        assert_eq!(watched_interval(None, Some(5.0)), None);
        assert_eq!(watched_interval(Some(16.0), None), None);
        // 最後の到着から 500ms を超えたら流れていない
        assert_eq!(watched_interval(Some(16.0), Some(501.0)), None);
        assert_eq!(watched_interval(Some(0.0), Some(5.0)), None);
    }

    #[test]
    fn software_renderers_are_recognised() {
        assert!(is_software_renderer("GDI Generic"));
        assert!(is_software_renderer("llvmpipe (LLVM 17.0.6, 256 bits)"));
        assert!(is_software_renderer("Microsoft Basic Render Driver"));
        assert!(is_software_renderer("Google SwiftShader"));
        // VMware の SVGA3D は名前に LLVM を含むが、ホストの GPU で描く
        assert!(!is_software_renderer("SVGA3D; build: RELEASE;  LLVM;"));
        assert!(!is_software_renderer("NVIDIA GeForce RTX 4070/PCIe/SSE2"));
        assert!(!is_software_renderer("AMD Radeon(TM) Graphics"));
    }
}
