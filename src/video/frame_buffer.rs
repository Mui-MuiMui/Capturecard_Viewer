//! フレームコールバックスレッドと UI スレッドの間でフレームを受け渡す箱と、
//! そこから読み出す観測値。
//!
//! **映像フレームだけはワーカースレッドのチャネルを通さない。** 理由は
//! `VideoFrames` の説明と `docs/design/device-worker.md` にある。

use log::warn;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// フレーム間隔から求めたばらつきの指標。単位はミリ秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntervalStats {
    /// 平均間隔から求めた実効 FPS。
    ///
    /// nokhwa の `camera_format().frame_rate()` は当てにならない値を返すため、
    /// 実際に届いたフレームの間隔から計算する
    pub fps: f32,
    /// 平均間隔
    pub average_ms: f32,
    /// 最小間隔
    pub min_ms: f32,
    /// 最大間隔
    pub max_ms: f32,
    /// 間隔の母標準偏差。コマ落ちや取り込みの詰まりでここが膨らむ
    pub stddev_ms: f32,
    /// 集計に使ったサンプル数
    pub samples: usize,
}

/// 映像パイプラインの観測値。OSD の表示に使う。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameStats {
    /// フレーム間隔の集計。2 枚目が届くまでは `None`
    pub intervals: Option<IntervalStats>,
    /// 直近 1 フレームの RGB 変換にかかった時間（ミリ秒）
    pub last_decode_ms: f32,
    /// 自前の YUY2 変換（高速パス）を通ったフレーム数
    pub fast_count: u64,
    /// デコーダ任せの汎用パスを通ったフレーム数
    pub fallback_count: u64,
    /// 直近フレームの画素数。フレームが無ければ `None`
    pub resolution: Option<(usize, usize)>,
    /// 直近フレームの入力フォーマット名。フレームが無ければ `None`
    pub source_format: Option<&'static str>,
    /// 最後にフレームが届いてからの経過時間（ミリ秒）
    pub since_last_frame_ms: Option<f32>,
}

/// フレーム間隔の列から実効 FPS とばらつきを求める。
///
/// 要素が無ければ `None` を返す。フレームがまだ 1 枚も来ていない状態と、
/// 間隔が取れている状態を呼び出し側で区別できるようにするため。
///
/// 平均間隔が 0 の場合は FPS を 0 にする。そのまま割ると無限大になり、
/// 表示にそれが出てしまう。
fn interval_stats(intervals: &VecDeque<f32>) -> Option<IntervalStats> {
    let samples = intervals.len();
    if samples == 0 {
        return None;
    }

    let count = samples as f32;
    let average_ms = intervals.iter().sum::<f32>() / count;
    let mut min_ms = f32::MAX;
    let mut max_ms = f32::MIN;
    for &interval in intervals {
        min_ms = min_ms.min(interval);
        max_ms = max_ms.max(interval);
    }
    let variance = intervals
        .iter()
        .map(|interval| {
            let diff = interval - average_ms;
            diff * diff
        })
        .sum::<f32>()
        / count;

    Some(IntervalStats {
        fps: if average_ms > 0.0 {
            1000.0 / average_ms
        } else {
            0.0
        },
        average_ms,
        min_ms,
        max_ms,
        stddev_ms: variance.sqrt(),
        samples,
    })
}

/// フレームコールバックスレッドと UI スレッドの間でフレームを受け渡す。
///
/// 画素データは `Arc` で共有するため、取り出しても複製は発生しない。
/// `generation` は push のたびに進み、取り出し側が新着の有無を判別するために使う。
pub(super) struct FrameBuffer {
    latest: Option<Arc<VideoFrame>>,
    generation: u64,
    last_frame_instant: Option<Instant>,
    frame_intervals: VecDeque<f32>, // ミリ秒
    last_decode_ms: f32,
    fast_count: u64,
    fallback_count: u64,
    // 直近フレームの入力フォーマット。デバイスが要求どおりに開けたとは
    // 限らないため、設定値ではなく実際に届いたフレームのものを持つ
    source_format: Option<&'static str>,
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            latest: None,
            generation: 0,
            last_frame_instant: None,
            frame_intervals: VecDeque::with_capacity(120),
            last_decode_ms: 0.0,
            fast_count: 0,
            fallback_count: 0,
            source_format: None,
        }
    }
    /// 新しいフレームを格納し、置き換えられた古いフレームを返す。
    ///
    /// 返した `Arc` の参照が呼び出し側だけになっていれば、中の `Vec` を
    /// 次の変換先として回収できる。回収しない場合はそのまま捨ててよい。
    pub(super) fn push_back(
        &mut self,
        frame: VideoFrame,
        received_at: Instant,
        decode_ms: f32,
        fast: bool,
        source_format: &'static str,
    ) -> Option<Arc<VideoFrame>> {
        let replaced = self.latest.replace(Arc::new(frame));
        self.generation += 1;
        self.last_decode_ms = decode_ms;
        self.source_format = Some(source_format);
        if fast {
            self.fast_count += 1;
        } else {
            self.fallback_count += 1;
        }
        // 間隔は RGB 変換が終わった時刻ではなく、フレームを受け取った時刻で測る。
        // 変換時間が揺れると、その差が間隔へそのまま乗ってばらつきが実態より
        // 大きく出る
        let now = received_at;
        if let Some(prev) = self.last_frame_instant.replace(now) {
            let dt = now.duration_since(prev).as_secs_f32() * 1000.0;
            if self.frame_intervals.len() == 120 {
                self.frame_intervals.pop_front();
            }
            self.frame_intervals.push_back(dt);
        }
        replaced
    }
    /// 直近のフレームとその世代番号を返す。新着かどうかは問わない。
    ///
    /// 返すのは `Arc` の複製なので、画素データはコピーされない。
    fn latest_frame(&self) -> Option<(Arc<VideoFrame>, u64)> {
        self.latest
            .as_ref()
            .map(|frame| (Arc::clone(frame), self.generation))
    }

    /// 最後にフレームが届いてからの経過時間。1 枚も届いていなければ `None`。
    fn since_last_frame(&self) -> Option<Duration> {
        self.last_frame_instant.map(|at| at.elapsed())
    }

    /// 保持しているフレームと統計を捨てる。キャプチャの停止時に呼ぶ。
    ///
    /// 世代番号は巻き戻さない。巻き戻すと、再接続後の最初のフレームが
    /// 取り出し側の記録している世代と一致して、新着と見なされなくなる。
    fn reset(&mut self) {
        self.latest = None;
        self.generation += 1;
        self.last_frame_instant = None;
        self.frame_intervals.clear();
        self.last_decode_ms = 0.0;
        self.fast_count = 0;
        self.fallback_count = 0;
        self.source_format = None;
    }

    /// OSD に出す観測値をまとめて返す。
    ///
    /// 間隔の集計は最大 120 要素の走査で済むため、表示中に毎フレーム
    /// 呼んでも問題にならない。表示していないときは呼ばない。
    fn stats(&self) -> FrameStats {
        FrameStats {
            intervals: interval_stats(&self.frame_intervals),
            last_decode_ms: self.last_decode_ms,
            fast_count: self.fast_count,
            fallback_count: self.fallback_count,
            resolution: self.latest.as_ref().map(|f| (f.width, f.height)),
            source_format: self.source_format,
            since_last_frame_ms: self
                .last_frame_instant
                .map(|at| at.elapsed().as_secs_f32() * 1000.0),
        }
    }
}

/// フレームバッファを共有するハンドル。
///
/// **映像フレームだけはワーカースレッドのチャネルを通さない。** デバイスの
/// 操作は `app::worker_loop` へ移してあるが、フレームをコマンドと同じ列に
/// 並べると、接続やデバイス列挙の後ろで待たされて遅延が増える。ここだけは
/// 従来どおり `Arc<Mutex<FrameBuffer>>` をフレームコールバックスレッドと
/// UI スレッドで直接共有する。
///
/// ロックの中で行うのは `Arc` の複製か最大 120 要素の集計だけで、
/// フレームコールバックの `push_back` をほとんど待たせない。
#[derive(Clone)]
pub struct VideoFrames {
    inner: Arc<Mutex<FrameBuffer>>,
}

impl Default for VideoFrames {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoFrames {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FrameBuffer::new())),
        }
    }

    /// 直近のフレームを新着かどうかに関わらず返す。
    ///
    /// スクリーンショットは「いま画面に出ている画」を保存するものなので、
    /// 新着でなくても最後に届いたフレームを返す必要がある。
    pub fn latest(&self) -> Option<Arc<VideoFrame>> {
        self.inner
            .lock()
            .ok()
            .and_then(|fb| fb.latest_frame().map(|(frame, _)| frame))
    }

    /// 世代番号が `last_generation` と異なるフレームがある場合だけ、
    /// フレームと世代番号を返す。
    ///
    /// 新着がなければ `None` を返すので、呼び出し側は前回の結果を使い回せる。
    pub fn newer_than(&self, last_generation: u64) -> Option<(Arc<VideoFrame>, u64)> {
        self.inner
            .lock()
            .ok()
            .and_then(|fb| match fb.latest_frame() {
                Some((frame, generation)) if generation != last_generation => {
                    Some((frame, generation))
                }
                _ => None,
            })
    }

    /// 映像パイプラインの観測値を返す。
    /// ロックを取れなかった場合は既定値（フレーム無し）を返す。
    pub fn stats(&self) -> FrameStats {
        self.inner
            .lock()
            .ok()
            .map(|fb| fb.stats())
            .unwrap_or_default()
    }

    /// 最後にフレームが届いてからの経過時間。
    ///
    /// ロックを取れなかった場合は「まだ 1 枚も届いていない」として返す。
    /// 途絶時間が取れない状態で切断と判断させないため。
    pub fn since_last_frame(&self) -> Option<Duration> {
        self.inner.lock().ok().and_then(|fb| fb.since_last_frame())
    }

    /// 保持しているフレームと統計を捨てる。キャプチャの停止時に呼ぶ。
    pub(super) fn reset(&self) {
        if let Ok(mut fb) = self.inner.lock() {
            fb.reset();
        } else {
            warn!("フレームバッファのロックを取得できないので統計を消せない");
        }
    }

    /// フレームコールバックへ渡す生のハンドル。
    pub(super) fn buffer(&self) -> Arc<Mutex<FrameBuffer>> {
        Arc::clone(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WIDTH: usize = 2;
    const TEST_HEIGHT: usize = 2;
    const TEST_FRAME_LEN: usize = TEST_WIDTH * TEST_HEIGHT * 3;

    /// 識別しやすいように全画素を marker で埋めたフレームを作る
    fn test_frame(marker: u8) -> VideoFrame {
        VideoFrame {
            width: TEST_WIDTH,
            height: TEST_HEIGHT,
            data: vec![marker; TEST_FRAME_LEN],
        }
    }

    /// 期待値との差が許容範囲に収まっているか調べる。
    /// f32 の演算誤差を吸収するためだけのもので、判定の緩和には使わない
    fn assert_close(actual: f32, expected: f32, label: &str) {
        assert!(
            (actual - expected).abs() < 0.001,
            "{}: 実際の値 {} が期待値 {} と離れている",
            label,
            actual,
            expected
        );
    }

    #[test]
    fn frame_buffer_latest_frame_after_push_returns_newest_frame() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");
        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");

        let (frame, generation) = buffer
            .latest_frame()
            .expect("push 済みなのでフレームが取れる");
        assert_eq!(frame.width, TEST_WIDTH);
        assert_eq!(frame.height, TEST_HEIGHT);
        assert_eq!(frame.data, vec![2u8; TEST_FRAME_LEN]);
        assert_eq!(generation, 2);
    }

    #[test]
    fn frame_buffer_latest_frame_without_push_returns_none() {
        let buffer = FrameBuffer::new();
        assert!(buffer.latest_frame().is_none());
    }

    #[test]
    fn frame_buffer_latest_frame_without_new_push_keeps_generation() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");

        let (_, first) = buffer.latest_frame().expect("1 枚目が取れる");
        let (_, second) = buffer.latest_frame().expect("取り出しても消えない");
        assert_eq!(first, second, "push が無ければ世代は進まない");

        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");
        let (_, third) = buffer.latest_frame().expect("2 枚目が取れる");
        assert_eq!(third, second + 1, "push すれば世代が 1 つ進む");
    }

    #[test]
    fn frame_buffer_latest_frame_twice_shares_same_allocation() {
        // 取り出しで画素データが複製されないこと（このタスクの本題）
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");

        let (first, _) = buffer.latest_frame().expect("1 回目");
        let (second, _) = buffer.latest_frame().expect("2 回目");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn frame_buffer_reset_drops_frame_and_advances_generation() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");
        let (_, before) = buffer.latest_frame().expect("push 済み");

        buffer.reset();
        assert!(buffer.latest_frame().is_none());

        // 再接続後の最初のフレームが「新着」と判別できること
        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");
        let (_, after) = buffer.latest_frame().expect("再接続後の 1 枚目");
        assert!(after > before);
    }

    #[test]
    fn frame_buffer_concurrent_push_returns_latest_frame_without_panic() {
        // コールバックスレッドが push し続ける裏で UI スレッドが取り出す状況を模す
        const PUSH_COUNT: usize = 500;
        let buffer = Arc::new(Mutex::new(FrameBuffer::new()));

        let writer = {
            let buffer = Arc::clone(&buffer);
            std::thread::spawn(move || {
                for i in 0..PUSH_COUNT {
                    buffer.lock().expect("書き込み側のロックに失敗").push_back(
                        test_frame(i as u8),
                        Instant::now(),
                        1.0,
                        true,
                        "YUY2",
                    );
                }
            })
        };

        let mut last_generation = 0;
        while !writer.is_finished() {
            if let Some((_, generation)) = buffer
                .lock()
                .expect("読み出し側のロックに失敗")
                .latest_frame()
            {
                assert!(generation >= last_generation, "世代は巻き戻らない");
                last_generation = generation;
            }
        }
        writer.join().expect("書き込みスレッドがパニックした");

        let (frame, generation) = buffer
            .lock()
            .expect("読み出し側のロックに失敗")
            .latest_frame()
            .expect("最後に push したフレームが残っている");
        assert_eq!(frame.data, vec![(PUSH_COUNT - 1) as u8; TEST_FRAME_LEN]);
        assert_eq!(generation, PUSH_COUNT as u64);
    }

    #[test]
    fn interval_stats_no_samples_returns_none() {
        // フレームがまだ 1 枚も来ていない状態。
        // 0 サンプルで平均を出すと NaN になるので、ここで弾く
        assert_eq!(interval_stats(&VecDeque::new()), None);
    }

    #[test]
    fn interval_stats_single_sample_has_no_spread() {
        // サンプルが 1 つだけでも標準偏差の計算で落ちないこと
        let stats = interval_stats(&VecDeque::from(vec![20.0])).expect("1 件でも集計できる");
        assert_eq!(stats.samples, 1);
        assert_close(stats.fps, 50.0, "fps");
        assert_close(stats.average_ms, 20.0, "average_ms");
        assert_close(stats.min_ms, 20.0, "min_ms");
        assert_close(stats.max_ms, 20.0, "max_ms");
        assert_close(stats.stddev_ms, 0.0, "stddev_ms");
    }

    #[test]
    fn interval_stats_constant_interval_reports_zero_stddev() {
        // 16ms 間隔がきれいに並んでいる状態。62.5fps でばらつきは 0
        let stats =
            interval_stats(&VecDeque::from(vec![16.0, 16.0, 16.0, 16.0])).expect("集計できる");
        assert_eq!(stats.samples, 4);
        assert_close(stats.fps, 62.5, "fps");
        assert_close(stats.average_ms, 16.0, "average_ms");
        assert_close(stats.stddev_ms, 0.0, "stddev_ms");
    }

    #[test]
    fn interval_stats_varying_interval_reports_spread() {
        // 10ms と 20ms が交互に来る状態。平均 15ms、母標準偏差 5ms
        let stats =
            interval_stats(&VecDeque::from(vec![10.0, 20.0, 10.0, 20.0])).expect("集計できる");
        assert_eq!(stats.samples, 4);
        assert_close(stats.average_ms, 15.0, "average_ms");
        assert_close(stats.min_ms, 10.0, "min_ms");
        assert_close(stats.max_ms, 20.0, "max_ms");
        assert_close(stats.stddev_ms, 5.0, "stddev_ms");
        assert_close(stats.fps, 66.6667, "fps");
    }

    #[test]
    fn interval_stats_zero_average_reports_zero_fps() {
        // 間隔の計測が全て 0 になった場合。1000 / 0 は無限大になるため、
        // そのまま表示へ流さず 0 にする
        let stats = interval_stats(&VecDeque::from(vec![0.0, 0.0])).expect("集計できる");
        assert_close(stats.fps, 0.0, "fps");
        assert_close(stats.average_ms, 0.0, "average_ms");
    }

    #[test]
    fn frame_buffer_stats_reports_latest_frame_and_paths() {
        // 高速パスと汎用パスの回数、解像度、フォーマット名が
        // 押し込んだとおりに読み出せること
        let mut buffer = FrameBuffer::new();
        // 受信時刻を 16ms 離して渡す。間隔は変換にかかった時間ではなく、
        // この差から計算されなければならない
        let now = Instant::now();
        buffer.push_back(
            test_frame(1),
            now - Duration::from_millis(32),
            2.5,
            true,
            "YUY2",
        );
        buffer.push_back(
            test_frame(2),
            now - Duration::from_millis(16),
            3.5,
            false,
            "MJPEG",
        );

        let stats = buffer.stats();
        let intervals = stats.intervals.expect("2 枚押し込めば間隔が 1 つ取れる");
        assert_eq!(intervals.samples, 1);
        assert_close(intervals.average_ms, 16.0, "average_ms");
        assert_close(intervals.fps, 62.5, "fps");
        assert_eq!(stats.fast_count, 1);
        assert_eq!(stats.fallback_count, 1);
        assert_close(stats.last_decode_ms, 3.5, "last_decode_ms");
        assert_eq!(stats.resolution, Some((TEST_WIDTH, TEST_HEIGHT)));
        assert_eq!(stats.source_format, Some("MJPEG"));
        assert!(stats.since_last_frame_ms.is_some());
    }

    #[test]
    fn frame_buffer_stats_after_reset_has_no_frame() {
        // キャプチャを止めたあとに前回の統計が残らないこと
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 2.5, true, "YUY2");
        buffer.push_back(test_frame(2), Instant::now(), 3.5, true, "YUY2");
        buffer.reset();

        let stats = buffer.stats();
        assert_eq!(stats.intervals, None);
        assert_eq!(stats.resolution, None);
        assert_eq!(stats.source_format, None);
        assert_eq!(stats.since_last_frame_ms, None);
        assert_eq!(stats.fast_count, 0);
        assert_eq!(stats.fallback_count, 0);
        assert_close(stats.last_decode_ms, 0.0, "last_decode_ms");
    }

    #[test]
    fn frame_buffer_push_back_returns_replaced_frame() {
        // 置き換えられたフレームを受け取れること。
        // コールバック側はこれを回収して変換先に使い回す
        let mut buffer = FrameBuffer::new();
        assert!(
            buffer
                .push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2")
                .is_none(),
            "1 枚目は置き換える対象が無い"
        );

        let replaced = buffer
            .push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2")
            .expect("2 枚目は 1 枚目を置き換える");
        assert_eq!(replaced.data, vec![1u8; TEST_FRAME_LEN]);
        assert!(
            Arc::try_unwrap(replaced).is_ok(),
            "取り出し側が保持していなければ Vec を回収できる"
        );
    }
}
