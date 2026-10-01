//! サンプルのタイムスタンプの計測（#406）。**テストを含むビルドだけにある。**
//!
//! レンダラーの `Receive` に届いたサンプルの到着時刻（`Receive` の入口の
//! `Instant`）と `IMediaSample::GetTime` の値を、映像と音声それぞれの
//! 固定長の表へ書く。`Receive` から呼ばれるので、表は Atomic だけで作り、
//! ロックもアロケーションもしない。書くのは `BASE` を置いてからだけで、
//! 計測しないテストでは何もしない。
//!
//! 使うのは `#[ignore]` のテスト `sample_timestamps_track_the_arrival_time`
//! （`mod.rs`）だけ。結果と結論は `docs/design/recording.md` の
//! 「DirectShow のサンプルのタイムスタンプ（#406）」。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// 表の長さ。60fps の映像で 270 秒、10ms の塊の音声で 160 秒ぶん
const CAPACITY: usize = 16384;

/// 1 つのピンに届いたサンプルの表。
pub(super) struct Probe {
    len: AtomicUsize,
    arrival_ns: [AtomicI64; CAPACITY],
    start: [AtomicI64; CAPACITY],
    end: [AtomicI64; CAPACITY],
    hresult: [AtomicI64; CAPACITY],
}

/// 1 つのサンプルの記録。
#[derive(Debug, Clone, Copy)]
pub(super) struct Row {
    /// `BASE` からの到着時刻（ns）
    pub(super) arrival_ns: i64,
    /// `GetTime` の開始と終わり（100ns。ストリーム時間）
    pub(super) start: i64,
    pub(super) end: i64,
    /// `GetTime` の結果。0 なら成功
    pub(super) hresult: i32,
}

// 配列の初期化子にだけ使う。共有の値として読むことはない
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicI64 = AtomicI64::new(0);

pub(super) static VIDEO: Probe = Probe::new();
pub(super) static AUDIO: Probe = Probe::new();
/// 到着時刻の基準。置くまでは記録しない
pub(super) static BASE: OnceLock<Instant> = OnceLock::new();
/// 真なら、グラフに基準時計を付ける（`SetSyncSource(NULL)` の代わりに `SetDefaultSyncSource`）
static DEFAULT_CLOCK: AtomicBool = AtomicBool::new(false);

pub(super) fn use_default_clock() -> bool {
    DEFAULT_CLOCK.load(Ordering::Relaxed)
}

pub(super) fn set_default_clock(value: bool) {
    DEFAULT_CLOCK.store(value, Ordering::Relaxed);
}

impl Probe {
    const fn new() -> Self {
        Self {
            len: AtomicUsize::new(0),
            arrival_ns: [ZERO; CAPACITY],
            start: [ZERO; CAPACITY],
            end: [ZERO; CAPACITY],
            hresult: [ZERO; CAPACITY],
        }
    }

    /// `Receive` から呼ぶ。表が埋まったら捨てる。
    pub(super) fn record(&self, at: Instant, hresult: i32, start: i64, end: i64) {
        let Some(base) = BASE.get() else {
            return;
        };
        let i = self.len.fetch_add(1, Ordering::Relaxed);
        if i >= CAPACITY {
            return;
        }
        let arrival = at.saturating_duration_since(*base).as_nanos() as i64;
        self.arrival_ns[i].store(arrival, Ordering::Relaxed);
        self.start[i].store(start, Ordering::Relaxed);
        self.end[i].store(end, Ordering::Relaxed);
        self.hresult[i].store(i64::from(hresult), Ordering::Relaxed);
    }

    pub(super) fn reset(&self) {
        self.len.store(0, Ordering::Relaxed);
    }

    pub(super) fn take(&self) -> Vec<Row> {
        let n = self.len.load(Ordering::Relaxed).min(CAPACITY);
        (0..n)
            .map(|i| Row {
                arrival_ns: self.arrival_ns[i].load(Ordering::Relaxed),
                start: self.start[i].load(Ordering::Relaxed),
                end: self.end[i].load(Ordering::Relaxed),
                hresult: self.hresult[i].load(Ordering::Relaxed) as i32,
            })
            .collect()
    }
}

/// 最小二乗の直線 y = a + b x からの残差。
fn residuals(x: &[f64], y: &[f64]) -> Vec<f64> {
    let n = x.len() as f64;
    let mx = x.iter().sum::<f64>() / n;
    let my = y.iter().sum::<f64>() / n;
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let sxx: f64 = x.iter().map(|a| (a - mx) * (a - mx)).sum();
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    x.iter()
        .zip(y)
        .map(|(a, c)| c - (my + b * (a - mx)))
        .collect()
}

/// (平均, 標準偏差, 最小, 最大)
fn stats(v: &[f64]) -> (f64, f64, f64, f64) {
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|a| (a - mean) * (a - mean)).sum::<f64>() / n).sqrt();
    let min = v.iter().copied().fold(f64::MAX, f64::min);
    let max = v.iter().copied().fold(f64::MIN, f64::max);
    (mean, sd, min, max)
}

/// 計測の結果を出す。タイムスタンプが付いていれば、到着 − タイムスタンプの平均（ms）を返す。
///
/// 揺れは「直線からの残差」で見る。到着だけなら番号に対する直線（取りこぼしが
/// あると段差になる）、タイムスタンプと比べるならタイムスタンプに対する直線
/// （2 つの時計の速さの違いは傾きに入る）。
pub(super) fn report(label: &str, rows: &[Row]) -> Option<f64> {
    println!("== {label}: サンプル {} 個", rows.len());
    if rows.len() < 3 {
        return None;
    }
    let failed: Vec<&Row> = rows.iter().filter(|r| r.hresult != 0).collect();
    println!(
        "GetTime の失敗 {} 個（最初の HRESULT {:?}）",
        failed.len(),
        failed
            .first()
            .map(|r| format!("0x{:08X}", r.hresult as u32))
    );
    let arrival: Vec<f64> = rows.iter().map(|r| r.arrival_ns as f64 / 1e6).collect();
    let intervals: Vec<f64> = arrival.windows(2).map(|w| w[1] - w[0]).collect();
    let (mean_interval, sd, min, max) = stats(&intervals);
    println!("到着の間隔 ms: 平均 {mean_interval:.3} 標準偏差 {sd:.3} 最小 {min:.3} 最大 {max:.3}");
    let gaps: Vec<String> = intervals
        .iter()
        .enumerate()
        .filter(|(_, d)| **d > mean_interval * 1.8)
        .map(|(i, d)| format!("{:.1}s:{:.0}ms", arrival[i] / 1000.0, d))
        .collect();
    println!("間隔が平均の 1.8 倍を超えた所: {gaps:?}");
    let index: Vec<f64> = (0..arrival.len()).map(|i| i as f64).collect();
    let (_, sd, min, max) = stats(&residuals(&index, &arrival));
    println!("到着の番号に対する直線からの残差 ms: 標準偏差 {sd:.3} 最小 {min:.3} 最大 {max:.3}");

    let stamped: Vec<&Row> = rows.iter().filter(|r| r.hresult == 0).collect();
    if stamped.len() < 3 {
        return None;
    }
    let time: Vec<f64> = stamped.iter().map(|r| r.start as f64 / 1e4).collect();
    let arrival: Vec<f64> = stamped.iter().map(|r| r.arrival_ns as f64 / 1e6).collect();
    println!(
        "最初のタイムスタンプ ms: {:?}（長さ {:.3}ms）",
        &time[..time.len().min(3)],
        (stamped[0].end - stamped[0].start) as f64 / 1e4
    );
    let time_intervals: Vec<f64> = time.windows(2).map(|w| w[1] - w[0]).collect();
    let (mean, sd, min, max) = stats(&time_intervals);
    println!(
        "タイムスタンプの間隔 ms: 平均 {mean:.3} 標準偏差 {sd:.3} 最小 {min:.3} 最大 {max:.3}"
    );
    let lag = residuals(&time, &arrival);
    let (_, sd, min, max) = stats(&lag);
    println!(
        "到着のタイムスタンプに対する直線からの残差 ms: 標準偏差 {sd:.3} 最小 {min:.3} 最大 {max:.3}"
    );
    let last = time.len() - 1;
    let slope = (arrival[last] - arrival[0]) / (time[last] - time[0]);
    println!(
        "到着 ÷ タイムスタンプの傾き（両端）: {:+.0}ppm",
        (slope - 1.0) * 1e6
    );
    let offsets: Vec<f64> = arrival.iter().zip(&time).map(|(a, t)| a - t).collect();
    Some(offsets.iter().sum::<f64>() / offsets.len() as f64)
}
