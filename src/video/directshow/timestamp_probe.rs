//! サンプルのタイムスタンプの計測（#406）。**テストを含むビルドだけにある。**
//!
//! レンダラーの `Receive` に届いたサンプルの到着時刻（`Receive` の入口の
//! `Instant`）と `IMediaSample::GetTime` の値を、映像と音声それぞれの
//! 固定長の表へ書く。`Receive` から呼ばれるので、表は Atomic だけで作り、
//! ロックもアロケーションもしない。書くのは `BASE` を置き、`set_recording(true)` の間だけで、
//! 計測しないテストでは何もしない。
//!
//! 使うのは `#[ignore]` のテスト `sample_timestamps_track_the_arrival_time`
//! （`mod.rs`）と、このファイルの `directshow_sample_time_to_receive_lag`（#476）。
//! 結果と結論は `docs/design/recording.md` の「DirectShow のサンプルの
//! タイムスタンプ（#406）」と `docs/design/video-pipeline.md` の「取り込み側の
//! 遅れの計測（#476）」。
//!
//! #476 のために、`Receive` の時点のグラフのストリーム時刻（基準時計の
//! `GetTime` − `Run` に渡された原点）も書く。サンプルの開始時刻との差が
//! 「ドライバーの打刻 → コールバック」の遅れになる。基準時計はレンダラーの
//! `SetSyncSource` が、原点は `Run` がここへ渡す。遅れの集計（`report_lag`）は
//! Media Foundation の経路の計測（`video::capture` のテスト）も使う。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Media::IReferenceClock;

/// 表の長さ。60fps の映像で 270 秒、10ms の塊の音声で 160 秒ぶん
const CAPACITY: usize = 16384;

/// 1 つのピンに届いたサンプルの表。
pub(super) struct Probe {
    len: AtomicUsize,
    arrival_ns: [AtomicI64; CAPACITY],
    start: [AtomicI64; CAPACITY],
    end: [AtomicI64; CAPACITY],
    hresult: [AtomicI64; CAPACITY],
    stream_now: [AtomicI64; CAPACITY],
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
    /// `Receive` の時点のグラフのストリーム時刻（100ns）。基準時計が無ければ `None`
    pub(super) stream_now: Option<i64>,
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
/// 偽の間は記録しない。測る区間の頭で立て、終わりで下ろす
static RECORDING: AtomicBool = AtomicBool::new(false);
/// グラフの基準時計（`IReferenceClock` の生ポインタ。参照を 1 つ持つ）。無ければ null
static CLOCK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// `Run` に渡されたストリーム時刻の原点（基準時計の時刻、100ns）。まだなら `NO_TIME`
static RUN_START: AtomicI64 = AtomicI64::new(NO_TIME);
/// 時刻が無いことを表す値
const NO_TIME: i64 = i64::MIN;

/// 記録を始めるか止める。止めたあとグラフを止めれば（`Stop` はストリーミングスレッドが
/// 抜けるまで戻らない）、書きかけの行は残らない
pub(super) fn set_recording(value: bool) {
    RECORDING.store(value, Ordering::Release);
}

pub(super) fn use_default_clock() -> bool {
    DEFAULT_CLOCK.load(Ordering::Relaxed)
}

pub(super) fn set_default_clock(value: bool) {
    DEFAULT_CLOCK.store(value, Ordering::Relaxed);
}

/// レンダラーの `SetSyncSource` から呼ぶ。前の時計の参照を手放して差し替える。
/// `SetSyncSource` はグラフが止まっている間にしか呼ばれないので、
/// `stream_time_now` が使っている最中の時計を手放すことはない
pub(super) fn set_clock(clock: Option<&IReferenceClock>) {
    let raw = clock.map_or(std::ptr::null_mut(), |clock| clock.clone().into_raw());
    let old = CLOCK.swap(raw, Ordering::AcqRel);
    if !old.is_null() {
        // SAFETY: `into_raw` で参照を 1 つ持たせたポインタ。ここで手放す
        drop(unsafe { IReferenceClock::from_raw(old) });
    }
}

/// レンダラーの `Run` から呼ぶ。ストリーム時刻の原点
pub(super) fn set_run_start(start: i64) {
    RUN_START.store(start, Ordering::Release);
}

/// いまのグラフのストリーム時刻（100ns）。記録中でない・基準時計が無い・
/// まだ `Run` されていないなら `None`。`Receive` から呼ぶ（ロックもアロケーションもしない）
pub(super) fn stream_time_now() -> Option<i64> {
    if !RECORDING.load(Ordering::Acquire) {
        return None;
    }
    let raw = CLOCK.load(Ordering::Acquire);
    let start = RUN_START.load(Ordering::Acquire);
    if raw.is_null() || start == NO_TIME {
        return None;
    }
    // SAFETY: `set_clock` が参照を持たせたポインタ。差し替えはグラフが止まっている間だけ
    let clock = unsafe { IReferenceClock::from_raw_borrowed(&raw) }?;
    let now = unsafe { clock.GetTime() }.ok()?;
    Some(now - start)
}

/// 計測の対象のデバイス名（の一部）。環境変数 `CAPTURECARD_VIEWER_PIN_TEST_DEVICE`、既定は GC551。
/// Media Foundation の経路の計測（`video::capture` のテスト）も使う
pub(crate) fn test_device() -> String {
    std::env::var("CAPTURECARD_VIEWER_PIN_TEST_DEVICE")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "GC551".to_string())
}

impl Probe {
    const fn new() -> Self {
        Self {
            len: AtomicUsize::new(0),
            arrival_ns: [ZERO; CAPACITY],
            start: [ZERO; CAPACITY],
            end: [ZERO; CAPACITY],
            hresult: [ZERO; CAPACITY],
            stream_now: [ZERO; CAPACITY],
        }
    }

    /// `Receive` から呼ぶ。表が埋まったら捨てる。
    pub(super) fn record(
        &self,
        at: Instant,
        hresult: i32,
        start: i64,
        end: i64,
        stream_now: Option<i64>,
    ) {
        if !RECORDING.load(Ordering::Acquire) {
            return;
        }
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
        self.stream_now[i].store(stream_now.unwrap_or(NO_TIME), Ordering::Relaxed);
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
                stream_now: Some(self.stream_now[i].load(Ordering::Relaxed))
                    .filter(|t| *t != NO_TIME),
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

/// 「打刻 → コールバック」の遅れの集計（ms）。DirectShow と Media Foundation の
/// 経路で同じ集計を使う（#476）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct LagStats {
    pub(crate) count: usize,
    pub(crate) mean: f64,
    pub(crate) sd: f64,
    pub(crate) min: f64,
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) max: f64,
}

/// 遅れ（ms）の列を集計して出す。3 個に満たなければ `None`
pub(crate) fn report_lag(label: &str, lags_ms: &[f64]) -> Option<LagStats> {
    if lags_ms.len() < 3 {
        println!(
            "== {label}: 遅れを測れたサンプルが {} 個しか無い",
            lags_ms.len()
        );
        return None;
    }
    let (mean, sd, min, max) = stats(lags_ms);
    let mut sorted = lags_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    let lag = LagStats {
        count: lags_ms.len(),
        mean,
        sd,
        min,
        p50: at(0.5),
        p95: at(0.95),
        max,
    };
    println!(
        "== {label}: 打刻 → コールバックの遅れ ms（{} 個）: 平均 {:.3} 標準偏差 {:.3} 最小 {:.3} 中央値 {:.3} 95% {:.3} 最大 {:.3}（平均 − 最小 {:.3}、最大 − 最小 {:.3}）",
        lag.count,
        lag.mean,
        lag.sd,
        lag.min,
        lag.p50,
        lag.p95,
        lag.max,
        lag.mean - lag.min,
        lag.max - lag.min
    );
    Some(lag)
}

/// 複数回の計測の集計を表の 1 行ずつで出す（Issue に写す形）
pub(crate) fn print_lag_summary(label: &str, runs: &[Option<LagStats>]) {
    println!("| {label} | 個数 | 平均 | 標準偏差 | 最小 | 中央値 | 95% | 最大 |");
    for (i, run) in runs.iter().enumerate() {
        match run {
            Some(l) => println!(
                "| {} 回目 | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
                i + 1,
                l.count,
                l.mean,
                l.sd,
                l.min,
                l.p50,
                l.p95,
                l.max
            ),
            None => println!("| {} 回目 | 測れない | | | | | | |", i + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{display_name, DirectShowCapture};
    use super::*;
    use crate::audio::AudioPinFeed;
    use crate::repaint::RepaintWaker;
    use crate::video::{SharedColorConversion, VideoFrames};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    #[ignore = "DirectShow のキャプチャーボード（CAPTURECARD_VIEWER_PIN_TEST_DEVICE、既定 GC551）に 1920x1080 60Hz の入力信号を入れておく"]
    fn directshow_sample_time_to_receive_lag() {
        // 実行: cargo test directshow_sample_time_to_receive_lag -- --ignored --nocapture
        // #476。基準時計を付けたグラフで 1920x1080 60fps YUY2 を開き、2 秒待ってから
        // 10 秒のあいだ、サンプルの開始時刻（ドライバーの打刻、ストリーム時刻）と
        // `Receive` の時点のストリーム時刻の差を測る。2 回開き直して再現性を見る。
        // 音声ピンは繋がない。アプリは基準時計を外して動かすので、その点だけ条件が違う。
        // Media Foundation の経路は `video::capture` の
        // `media_foundation_capture_timestamp_to_callback_lag`
        let device = test_device();
        let _ = BASE.set(Instant::now());
        let mut capture = DirectShowCapture::new(
            VideoFrames::new(),
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
            AudioPinFeed::new(),
        );
        let name = capture
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains(&device))
            .unwrap_or_else(|| panic!("DirectShow のデバイス {device} がある"));
        let display = display_name(&name);
        set_default_clock(true);
        let mut runs = Vec::new();
        for run in 1..=2 {
            capture
                .start_capture(&display, Some((1920, 1080)), Some("YUY2"), Some(60), false)
                .expect("開ける");
            println!("{run} 回目: {:?}", capture.active());
            std::thread::sleep(Duration::from_secs(2));
            VIDEO.reset();
            set_recording(true);
            std::thread::sleep(Duration::from_secs(10));
            set_recording(false);
            // アロケーターは接続で決まる。止める前に読む
            match capture
                .graph
                .as_ref()
                .and_then(|graph| graph.video_allocator())
            {
                Some((bytes, count)) => {
                    println!("映像ピンのアロケーター: cBuffers {count}、cbBuffer {bytes} バイト")
                }
                None => println!("映像ピンのアロケーターを読めない"),
            }
            // 止めてから読む（`sample_timestamps_track_the_arrival_time` と同じ理由）
            capture.stop_capture();
            let rows = VIDEO.take();
            let label = format!("DirectShow {run} 回目");
            report(&label, &rows);
            let lags: Vec<f64> = rows
                .iter()
                .filter(|row| row.hresult == 0)
                .filter_map(|row| row.stream_now.map(|now| (now - row.start) as f64 / 1e4))
                .collect();
            runs.push(report_lag(&label, &lags));
        }
        set_default_clock(false);
        set_clock(None);
        print_lag_summary("DirectShow", &runs);
        assert!(
            runs.iter().all(Option::is_some),
            "サンプルの時刻か基準時計の時刻が取れない"
        );
    }
}
