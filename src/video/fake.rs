//! 実機なしで動くフェイクの映像デバイス。
//!
//! 環境変数 `CAPTURECARD_VIEWER_FAKE_DEVICES` を指定して起動したときだけ、
//! `VideoCapture` の代わりに使われる（選ぶのは `app::worker::DeviceWorker::spawn`、
//! trait に包むのは `app::backend::fake`）。指定が無ければ作られない。
//!
//! 「Fake Camera 1」「Fake Camera 2」…を名乗り、YUY2 のテストパターンを指定の
//! fps で吐く。**変換から先は実機と同じ `FrameSink` を通る**ので、色空間・
//! レンジ・映像調整・統計 OSD・スクリーンショットは実機と同じように効く。
//! 再現できないのは Media Foundation そのものの挙動（列挙の遅さ、フォーマットの
//! 癖、`Camera::new` の所要時間）だけ。
//!
//! | デバイス | パターン |
//! |---|---|
//! | 奇数番（1, 3, …） | 75% のカラーバー 8 本（白・黄・シアン・緑・マゼンタ・赤・青・黒） |
//! | 偶数番（2, 4, …） | ベタ塗り。2 番が青、4 番が赤、6 番が緑、8 番が黄 |
//!
//! どちらにも左上へフレーム番号を焼き込む。パターンの描き方は
//! `super::test_pattern`、ここはデバイスとしての振る舞いと生成スレッドだけを持つ。

use log::{debug, info, warn};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::capabilities::{DeviceCapabilities, FormatCapability, VideoMode};
use super::capture::{ActiveVideo, VideoLinkState};
use super::color::SharedColorConversion;
use super::frame_buffer::VideoFrames;
use super::frame_sink::FrameSink;
use super::test_pattern::{burn_frame_number, description_for, pattern_for, render_pattern};
use super::VideoError;
use crate::repaint::RepaintWaker;

/// フェイクの映像デバイスが名乗る名前の前半。後ろに 1 から始まる番号が付く
const DEVICE_NAME_PREFIX: &str = "Fake Camera";

/// 開ける映像モード。解像度の大きい順、同じ解像度なら fps の大きい順
/// （実機の `get_device_capabilities` と同じ並び）。
const MODES: [VideoMode; 6] = [
    VideoMode::new(1920, 1080, 60),
    VideoMode::new(1920, 1080, 30),
    VideoMode::new(1280, 720, 60),
    VideoMode::new(1280, 720, 30),
    VideoMode::new(640, 480, 60),
    VideoMode::new(640, 480, 30),
];

/// 解像度が未指定のときに開くモード。実機の `start_capture` と同じ 1280x720 60fps
const DEFAULT_MODE: VideoMode = VideoMode::new(1280, 720, 60);

/// 受け付ける fps の範囲。実機の `start_capture` と同じ
const MIN_FPS: u32 = 15;
const MAX_FPS: u32 = 120;

/// フェイクの映像デバイスの振る舞い。環境変数から組み立てる
/// （`app::backend::fake`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FakeVideoOptions {
    /// 名乗るデバイスの台数。1 以上
    pub device_count: u32,
    /// 開いてからこの時間が経つとフレームを止める（切断の再現）。
    /// **開き直すたびに数え直す**ので、再接続のたびに同じだけ流れてまた止まる
    pub disconnect_after: Option<Duration>,
    /// 最初にこの回数だけ開くのに失敗する（接続失敗と再試行の再現）
    pub failures_before_success: u32,
}

/// 開いているストリーム。生成スレッドと、それを止めるための送り口。
struct FakeVideoStream {
    /// 落とすと生成スレッドが止まる（受け側が切断を見る）
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

/// フェイクの映像デバイス。`VideoCapture` と同じ窓口を持つ。
///
/// **デバイスワーカースレッド（`app::worker_loop`）だけが触る。** 生成スレッドは
/// `start_capture` で起こし、`stop_capture` で止めて join する。
pub struct FakeVideoCapture {
    frames: VideoFrames,
    color_conversion: Arc<SharedColorConversion>,
    repaint_waker: RepaintWaker,
    options: FakeVideoOptions,
    /// シナリオ（`failures_before_success`）で、あと何回失敗させるか
    remaining_failures: u32,
    stream: Option<FakeVideoStream>,
    active: Option<ActiveVideo>,
}

impl FakeVideoCapture {
    /// 引数の意味は `VideoCapture::new` と同じ。
    pub fn new(
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: RepaintWaker,
        options: FakeVideoOptions,
    ) -> Self {
        Self {
            frames,
            color_conversion,
            repaint_waker,
            remaining_failures: options.failures_before_success,
            options,
            stream: None,
            active: None,
        }
    }

    /// 名乗るデバイスの一覧。`(名前, 説明)`
    pub fn list_devices(&self) -> Vec<(String, String)> {
        (1..=self.options.device_count)
            .map(|index| (device_name(index), description_for(index)))
            .collect()
    }

    /// デバイスの対応形式。どのデバイスも YUY2 の `MODES` を返す
    pub fn capabilities(
        &self,
        device_name: Option<&str>,
    ) -> Result<DeviceCapabilities, VideoError> {
        self.find_device(device_name)?;
        Ok(vec![FormatCapability::new("YUY2", MODES.to_vec())])
    }

    pub fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        self.stop_capture();

        let index = self.find_device(device_name)?;
        let name = self::device_name(index);

        if self.remaining_failures > 0 {
            self.remaining_failures -= 1;
            info!(
                "フェイクの映像デバイスを開くのに失敗させた（シナリオ fail、残り {} 回）",
                self.remaining_failures
            );
            return Err(VideoError::CameraOpenFailed {
                device: name,
                source: "フェイクのシナリオ（fail）で失敗させた".to_string(),
            });
        }

        if let Some(other) = format.filter(|f| !f.is_empty() && *f != "YUY2") {
            // 実機と同じく、YUY2 以外を選ばれても YUY2 で開く
            warn!(
                "フェイクの映像デバイスは YUY2 しか出さないので {} ではなく YUY2 で開く",
                other
            );
        }
        let mode = choose_mode(resolution, fps);
        let (width, height) = (mode.width as usize, mode.height as usize);
        let pattern = pattern_for(index);
        let base = render_pattern(pattern, width, height);

        let (stop_tx, stop_rx) = mpsc::channel();
        let sink = FrameSink::new(
            &self.frames,
            self.color_conversion.clone(),
            self.repaint_waker.clone(),
        );
        let generator = Generator {
            sink,
            width,
            height,
            interval: Duration::from_secs_f64(1.0 / f64::from(mode.fps)),
            base,
            disconnect_after: self.options.disconnect_after,
        };
        let handle = std::thread::Builder::new()
            .name("fake-video".to_string())
            .spawn(move || generator.run(stop_rx))
            .map_err(|e| VideoError::StreamOpenFailed {
                device: name.clone(),
                source: e.to_string(),
            })?;

        info!(
            "フェイクの映像デバイスを開いた（{}、{}x{} {}fps、パターン: {:?}）",
            name, mode.width, mode.height, mode.fps, pattern
        );
        self.stream = Some(FakeVideoStream {
            stop: stop_tx,
            handle,
        });
        self.active = Some(ActiveVideo {
            device_name: name,
            resolution: Some(mode.resolution()),
            // 実機は nokhwa の列挙名（`YUYV`）が入るので揃える
            format: Some("YUYV".to_string()),
            requested_fps: mode.fps,
        });
        Ok(())
    }

    pub fn stop_capture(&mut self) {
        self.active = None;
        if let Some(stream) = self.stream.take() {
            drop(stream.stop);
            if stream.handle.join().is_err() {
                warn!("フェイクの映像の生成スレッドが異常終了していた");
            }
            info!("フェイクの映像デバイスを閉じた");
        }
        // 生成スレッドを止めてから消す。先に消すと、止まる前の 1 枚が残る
        self.frames.reset();
    }

    pub fn link_state(&self) -> VideoLinkState {
        VideoLinkState {
            capturing: self.stream.is_some(),
            since_last_frame: self.frames.since_last_frame(),
        }
    }

    pub fn active(&self) -> Option<ActiveVideo> {
        self.active.clone()
    }

    /// 名前からデバイスの番号（1 から）を引く。`None` なら先頭
    fn find_device(&self, device_name: Option<&str>) -> Result<u32, VideoError> {
        match device_name {
            None if self.options.device_count > 0 => Ok(1),
            None => Err(VideoError::NoDevices),
            Some(name) => (1..=self.options.device_count)
                .find(|index| self::device_name(*index) == name)
                .ok_or_else(|| VideoError::DeviceNotFound(name.to_string())),
        }
    }
}

impl Drop for FakeVideoCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

fn device_name(index: u32) -> String {
    format!("{DEVICE_NAME_PREFIX} {index}")
}

/// 要求された解像度と fps から開くモードを決める。
///
/// 解像度は `MODES` にあればそれ、無ければ画素数が最も近いもの（実機の
/// `RequestedFormatType::Closest` に倣う）。fps は実機と同じく 15〜120 へ丸める。
fn choose_mode(resolution: Option<(u32, u32)>, fps: Option<u32>) -> VideoMode {
    let Some((width, height)) = resolution else {
        return DEFAULT_MODE;
    };
    let requested = u64::from(width) * u64::from(height);
    let closest = MODES
        .iter()
        .min_by_key(|mode| mode.pixel_count().abs_diff(requested))
        .copied()
        .unwrap_or(DEFAULT_MODE);
    VideoMode::new(
        closest.width,
        closest.height,
        fps.unwrap_or(DEFAULT_MODE.fps).clamp(MIN_FPS, MAX_FPS),
    )
}

/// 生成スレッドが持つもの一式。
struct Generator {
    sink: FrameSink,
    width: usize,
    height: usize,
    interval: Duration,
    /// 焼き込みの無い下地。毎フレームこれを写してから番号を焼く
    base: Vec<u8>,
    disconnect_after: Option<Duration>,
}

impl Generator {
    /// `stop` の送り手が落とされるまでフレームを吐き続ける。
    ///
    /// 待ちは `recv_timeout` で行い、止める指示が来たらその場で抜ける。
    /// 処理が間に合わなかったときは遅れを溜めず、次の予定を今に寄せる。
    fn run(mut self, stop: Receiver<()>) {
        let started = Instant::now();
        let mut next = started;
        let mut frame = self.base.clone();
        let mut number: u64 = 0;

        loop {
            let now = Instant::now();
            let stopped = if next > now {
                !matches!(
                    stop.recv_timeout(next - now),
                    Err(RecvTimeoutError::Timeout)
                )
            } else {
                !matches!(stop.try_recv(), Err(TryRecvError::Empty))
            };
            if stopped {
                break;
            }

            if self
                .disconnect_after
                .is_some_and(|after| started.elapsed() >= after)
            {
                info!(
                    "フェイクの映像を止めた（シナリオ disconnect、{} 枚目まで）",
                    number
                );
                // 止める指示（送り手が落ちる）まで何もしない
                let _ = stop.recv();
                break;
            }

            let received_at = Instant::now();
            frame.copy_from_slice(&self.base);
            burn_frame_number(&mut frame, self.width, self.height, number);
            self.sink
                .push_yuy2(self.width, self.height, &frame, received_at);
            number += 1;

            next += self.interval;
            let now = Instant::now();
            if next < now {
                next = now;
            }
        }
        debug!("フェイクの映像の生成スレッドを終えた（{} 枚）", number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_mode_keeps_a_listed_resolution_and_clamps_fps() {
        assert_eq!(
            choose_mode(Some((640, 480)), Some(30)),
            VideoMode::new(640, 480, 30)
        );
        // 実機と同じく 15〜120 へ丸める
        assert_eq!(
            choose_mode(Some((1920, 1080)), Some(240)),
            VideoMode::new(1920, 1080, 120)
        );
        assert_eq!(
            choose_mode(Some((1920, 1080)), Some(1)),
            VideoMode::new(1920, 1080, 15)
        );
    }

    #[test]
    fn choose_mode_unlisted_resolution_picks_the_closest() {
        // 1600x1200 は 1280x720 より 1920x1080 に画素数が近い
        assert_eq!(
            choose_mode(Some((1600, 1200)), None),
            VideoMode::new(1920, 1080, 60)
        );
        // 1600x900 は 1920x1080 より 1280x720 に画素数が近い
        assert_eq!(
            choose_mode(Some((1600, 900)), Some(30)),
            VideoMode::new(1280, 720, 30)
        );
        assert_eq!(choose_mode(None, Some(30)), DEFAULT_MODE);
    }

    fn capture(options: FakeVideoOptions) -> (FakeVideoCapture, VideoFrames) {
        let frames = VideoFrames::new();
        let capture = FakeVideoCapture::new(
            frames.clone(),
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
            options,
        );
        (capture, frames)
    }

    const TWO_DEVICES: FakeVideoOptions = FakeVideoOptions {
        device_count: 2,
        disconnect_after: None,
        failures_before_success: 0,
    };

    /// フレームが届くまで待つ。CI の遅いランナーでも収まるよう長めに取る
    fn wait_for_frame(frames: &VideoFrames) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if frames.latest().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn fake_video_lists_numbered_devices() {
        let (capture, _) = capture(TWO_DEVICES);
        let names: Vec<String> = capture
            .list_devices()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["Fake Camera 1", "Fake Camera 2"]);
    }

    #[test]
    fn fake_video_unknown_device_is_not_found() {
        let (mut capture, _) = capture(TWO_DEVICES);
        assert_eq!(
            capture.capabilities(Some("Fake Camera 3")),
            Err(VideoError::DeviceNotFound("Fake Camera 3".to_string()))
        );
        assert_eq!(
            capture.start_capture(Some("Fake Camera 3"), None, None, None),
            Err(VideoError::DeviceNotFound("Fake Camera 3".to_string()))
        );
    }

    #[test]
    fn fake_video_start_delivers_frames_and_stop_clears_them() {
        let (mut capture, frames) = capture(TWO_DEVICES);
        capture
            .start_capture(
                Some("Fake Camera 2"),
                Some((640, 480)),
                Some("YUY2"),
                Some(60),
            )
            .expect("フェイクは開ける");

        assert!(wait_for_frame(&frames), "フレームが届かない");
        let frame = frames.latest().expect("届いている");
        assert_eq!((frame.width, frame.height), (640, 480));
        // 焼き込みの外（右下）は 2 番のベタ塗り（青）
        // 焼き込みの外（右下）は 2 番のベタ塗り（青）。色の検証そのものは
        // `test_pattern` のテストが持つので、ここは青であることだけを見る
        let last = frame.data.len() - 3;
        let (r, g, b) = (frame.data[last], frame.data[last + 1], frame.data[last + 2]);
        assert!(r <= 2 && g <= 2 && b.abs_diff(191) <= 2, "{r} {g} {b}");
        assert!(capture.link_state().capturing);
        let active = capture.active().expect("開いている");
        assert_eq!(active.device_name, "Fake Camera 2");
        assert_eq!(active.resolution, Some((640, 480)));

        capture.stop_capture();
        assert!(frames.latest().is_none());
        assert!(!capture.link_state().capturing);
        assert!(capture.active().is_none());
    }

    #[test]
    fn fake_video_fail_scenario_fails_then_succeeds() {
        let (mut capture, _) = capture(FakeVideoOptions {
            failures_before_success: 2,
            ..TWO_DEVICES
        });

        for _ in 0..2 {
            assert!(matches!(
                capture.start_capture(None, Some((640, 480)), None, Some(30)),
                Err(VideoError::CameraOpenFailed { .. })
            ));
            assert!(!capture.link_state().capturing);
        }
        capture
            .start_capture(None, Some((640, 480)), None, Some(30))
            .expect("3 回目は開ける");
        assert_eq!(
            capture.active().map(|active| active.device_name),
            Some("Fake Camera 1".to_string())
        );
    }

    #[test]
    fn fake_video_disconnect_scenario_stops_frames_but_stays_open() {
        let (mut capture, frames) = capture(FakeVideoOptions {
            disconnect_after: Some(Duration::from_millis(100)),
            ..TWO_DEVICES
        });
        capture
            .start_capture(None, Some((640, 480)), None, Some(60))
            .expect("開ける");
        assert!(wait_for_frame(&frames), "止まる前に 1 枚は届く");

        // 止まってから十分待つ。途絶時間が伸び続けていれば止まっている
        std::thread::sleep(Duration::from_millis(600));
        let state = capture.link_state();
        assert!(
            state.capturing,
            "ストリームは開いたまま（信号だけが止まる）"
        );
        assert!(
            state
                .since_last_frame
                .is_some_and(|since| since >= Duration::from_millis(300)),
            "フレームが止まっていない: {:?}",
            state.since_last_frame
        );
    }
}
