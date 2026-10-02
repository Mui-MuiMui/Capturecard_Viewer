//! DirectShow の映像デバイス。Media Foundation（nokhwa）に出ないデバイス
//! （DirectShow のフィルターとしてしか登録されない仮想カメラや古い
//! キャプチャーボード）を扱うためのもの（#143）。
//!
//! `DirectShowCapture` は `VideoCapture` と同じ窓口（列挙・能力・開く・閉じる・
//! 観測）を持ち、**デバイスワーカースレッドだけが触る。** Media Foundation と
//! 束ねて 1 つの `VideoBackend` にするのは `app::backend::system` の役目で、
//! ここは DirectShow のことだけを知っている。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `devices.rs` | 列挙（`ICreateDevEnum`）、対応形式（`IAMStreamConfig::GetStreamCaps`）、いまの解像度（`GetFormat`）、開く解像度と形式の選び方 |
//! | `graph.rs` | フィルターグラフの組み立て・開始・停止・破棄 |
//! | `filter.rs` | サンプルを受け取る自前のレンダラーフィルター（`IBaseFilter` / `IPin` / `IMemInputPin`）。媒体を問わない |
//! | `audio_pin.rs` | 音声ピン（#388）。有無の記録、塊の長さの提案と接続、`Run` が通らないときに外してやり直す、音声のレンダラーが受け取った PCM を `AudioPinFeed` へ渡す |
//! | `video_stream.rs` | 映像のレンダラーが受け取ったサンプルを `FrameSink` へ渡す |
//! | `media_type.rs` | `AM_MEDIA_TYPE` の読み書きと解放（COM の初期化は `crate::com`） |
//! | `timestamp_probe.rs` | テストを含むビルドだけ。サンプルの到着時刻とタイムスタンプの計測（#406） |
//!
//! **デバイス名には「(DirectShow)」を添える**（`display_name`）。設定に
//! 保存されるのもこの名前で、Media Foundation の経路とどちらで開くかは
//! まずこの印で決まる（自動で Media Foundation が開けないときに DirectShow で
//! 試し直すのは `app::backend::system`、#387）。言語によって変えない（設定に
//! 残る識別子なので、画面の言語を切り替えると別のデバイスになってしまう）。

mod audio_pin;
mod devices;
mod filter;
mod graph;
mod media_type;
#[cfg(test)]
mod timestamp_probe;
mod video_stream;

use log::{debug, info, warn};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

use super::capabilities::DeviceCapabilities;
use super::capture::{ActiveVideo, CaptureApi, VideoLinkState};
use super::color::SharedColorConversion;
use super::frame_buffer::VideoFrames;
use super::frame_sink::FrameSink;
use super::{elapsed_ms, VideoError};
use crate::audio::{AudioPinFeed, AudioPinState, PinConnection};
use crate::com::{ComApartment, ComModel};
use crate::repaint::RepaintWaker;
use audio_pin::{AudioPinRequest, PinOutcome};
use devices::DeviceEntry;
use graph::{CaptureGraph, FormatRequest, GraphError};

/// DirectShow のデバイス名に添える印。**設定に保存される識別子の一部なので、
/// 翻訳しない。**
const DISPLAY_SUFFIX: &str = " (DirectShow)";

/// 表示名（「(DirectShow)」付き）を作る
pub fn display_name(friendly_name: &str) -> String {
    format!("{friendly_name}{DISPLAY_SUFFIX}")
}

/// 表示名から DirectShow の表示名（`FriendlyName`）を取り出す。
/// 「(DirectShow)」が付いていなければ `None`（Media Foundation のデバイス）
pub fn friendly_name(display_name: &str) -> Option<&str> {
    display_name
        .strip_suffix(DISPLAY_SUFFIX)
        .filter(|name| !name.is_empty())
}

/// DirectShow の映像デバイス。
pub struct DirectShowCapture {
    graph: Option<CaptureGraph>,
    active: Option<ActiveVideo>,
    frames: VideoFrames,
    color_conversion: Arc<SharedColorConversion>,
    repaint_waker: RepaintWaker,
    /// 音声ピンの差し込み口（#388）。音声のバックエンドと同じものを指す。グラフを
    /// 組むたびに番号を配り、繋いだ音声ピンを書く
    pin_feed: AudioPinFeed,
    /// いまのグラフで、音声ピンから届いた塊の長さをログへ出したか。`link_state` が
    /// `&self` で書くので `Cell`（触るのはワーカーだけ）
    pin_chunk_logged: Cell<bool>,
    /// **最後に落とす。** グラフや名札（COM のオブジェクト）を手放してから
    /// COM の初期化を戻す。フィールドは宣言順に落ちるので、末尾に置いてある
    _com: Option<ComApartment>,
}

impl DirectShowCapture {
    /// 引数の意味は `VideoCapture::new` と同じ。`pin_feed` は音声ピンの差し込み口で、
    /// 音声のバックエンド（`AudioCapture`）へ渡したものの複製。**デバイスワーカー
    /// スレッドの中で作る**（ここでそのスレッドの COM を初期化する）。
    pub fn new(
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: RepaintWaker,
        pin_feed: AudioPinFeed,
    ) -> Self {
        let com = match ComApartment::enter(ComModel::SingleThreaded) {
            Ok(com) => Some(com),
            Err(e) => {
                // 以降の列挙や接続がそれぞれの場所で失敗として出る
                warn!("DirectShow のために COM を初期化できない: {}", e);
                None
            }
        };
        Self {
            graph: None,
            active: None,
            frames,
            color_conversion,
            repaint_waker,
            pin_feed,
            pin_chunk_logged: Cell::new(false),
            _com: com,
        }
    }

    /// デバイスの表示名（`FriendlyName`、「(DirectShow)」なし）の一覧。
    /// 失敗しても空の一覧を返す。
    pub fn list_friendly_names(&self) -> Vec<String> {
        match self.try_list_friendly_names() {
            Ok(names) => names,
            Err(e) => {
                warn!("DirectShow: {}", e);
                Vec::new()
            }
        }
    }

    /// デバイスの表示名の一覧。`list_friendly_names` と違い、列挙に失敗した
    /// 理由を返す（ワーカーが列挙の結果をログへ残すため）。
    pub fn try_list_friendly_names(&self) -> Result<Vec<String>, VideoError> {
        let start = Instant::now();
        match devices::enumerate() {
            Ok(devices) => {
                let names: Vec<String> = devices
                    .into_iter()
                    .map(|device| device.friendly_name)
                    .collect();
                debug!(
                    "DirectShow の映像デバイスの一覧を取得した（{} 件、{:.1}ms）: {:?}",
                    names.len(),
                    elapsed_ms(start),
                    names
                );
                Ok(names)
            }
            Err(e) => {
                debug!(
                    "DirectShow の映像デバイスの列挙に失敗した（{:.1}ms）",
                    elapsed_ms(start)
                );
                Err(VideoError::DeviceQueryFailed(e.to_string()))
            }
        }
    }

    /// 表示名（「(DirectShow)」付き）からデバイスを探す。
    fn find(display: &str) -> Result<DeviceEntry, VideoError> {
        let wanted = friendly_name(display).unwrap_or(display);
        devices::enumerate()
            .map_err(|e| VideoError::DeviceQueryFailed(e.to_string()))?
            .into_iter()
            .find(|device| device.friendly_name == wanted)
            .ok_or_else(|| VideoError::DeviceNotFound(display.to_string()))
    }

    /// デバイスの対応形式。`display` は表示名（「(DirectShow)」付き）。
    ///
    /// 受け取れる形式（YUY2 / NV12 / I420 / YV12 / MJPEG / RGB24）が 1 つも無ければ空の一覧を返す。
    pub fn capabilities(&self, display: &str) -> Result<DeviceCapabilities, VideoError> {
        let start = Instant::now();
        let entry = Self::find(display)?;
        let candidates = graph::query_candidates(&entry).map_err(|e| match e {
            GraphError::Open(e) | GraphError::Stream(e) => VideoError::CameraOpenFailed {
                device: display.to_string(),
                source: e.to_string(),
            },
        })?;
        let Some(queried) = candidates else {
            warn!(
                "DirectShow のデバイス {} は IAMStreamConfig を持たないので、対応形式を出せない",
                display
            );
            return Ok(Vec::new());
        };
        let capabilities = devices::capabilities_from_candidates(
            &queried.candidates,
            queried.current,
            queried.current_fps,
        );
        debug!(
            "DirectShow のデバイス能力の内訳（{}、{:.1}ms、いまの解像度: {:?}、いまの fps: {:?}）: {}",
            display,
            elapsed_ms(start),
            queried.current,
            queried.current_fps,
            capabilities
                .iter()
                .map(|capability| {
                    // いまの解像度で選べる fps も出す（#410）
                    let fps: Vec<u32> = capability
                        .modes
                        .iter()
                        .filter(|mode| Some((mode.width, mode.height)) == queried.current)
                        .map(|mode| mode.fps)
                        .collect();
                    format!(
                        "{}: {} 件（いまの解像度の fps: {:?}）",
                        capability.name,
                        capability.modes.len(),
                        fps
                    )
                })
                .collect::<Vec<_>>()
                .join("、")
        );
        Ok(capabilities)
    }

    /// ストリームを開く。既に開いていれば閉じてから開き直す。
    /// `display` は表示名（「(DirectShow)」付き）。
    ///
    /// `connect_audio_pin` が真なら、同じグラフの音声ピンも繋ぐ（#388。
    /// `[audio] input_source = "video_pin"` のときだけ真）。偽でも音声ピンの有無は
    /// 調べて `ActiveVideo::audio_pin` に残す。
    pub fn start_capture(
        &mut self,
        display: &str,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
        connect_audio_pin: bool,
    ) -> Result<(), VideoError> {
        self.stop_capture();

        let entry = Self::find(display)?;
        let sink = FrameSink::new(
            &self.frames,
            self.color_conversion.clone(),
            self.repaint_waker.clone(),
        );
        let start = Instant::now();
        // グラフごとに番号を配る。音声の差し込み先はこの番号と合うときだけ積まれる
        let pin_graph = self.pin_feed.begin_graph();
        self.pin_chunk_logged.set(false);
        let graph = CaptureGraph::start(
            &entry,
            FormatRequest {
                resolution,
                format,
                fps,
            },
            sink,
            AudioPinRequest {
                connect: connect_audio_pin,
                feed: &self.pin_feed,
                graph: pin_graph,
            },
        )
        .map_err(|e| match e {
            GraphError::Open(e) => VideoError::CameraOpenFailed {
                device: display.to_string(),
                source: e.to_string(),
            },
            GraphError::Stream(e) => VideoError::StreamOpenFailed {
                device: display.to_string(),
                source: e.to_string(),
            },
        })?;

        let audio_pin = match graph.audio_pin().clone() {
            PinOutcome::Missing => AudioPinState::Missing,
            PinOutcome::Available => AudioPinState::Available,
            PinOutcome::Failed(failure) => AudioPinState::Failed(failure),
            PinOutcome::Connected {
                format,
                chunk_bytes,
            } => {
                let connection = PinConnection {
                    graph: pin_graph,
                    device: display.to_string(),
                    format,
                    chunk_bytes,
                };
                // 音声のバックエンドはここから形式と番号を読んで差し込む
                self.pin_feed.set_connected(connection.clone());
                AudioPinState::Connected(connection)
            }
        };
        let active = ActiveVideo {
            device_name: display.to_string(),
            api: CaptureApi::DirectShow,
            resolution: Some((graph.format.width, graph.format.height)),
            format: Some(graph.format_name().to_string()),
            requested_fps: graph.requested_fps,
            audio_pin,
        };
        info!(
            "DirectShow の映像ストリームを開いた（デバイス: {}、実際の設定: {}、{}fps、{:.1}ms）",
            display,
            active.summary(),
            graph.requested_fps,
            elapsed_ms(start)
        );
        self.graph = Some(graph);
        self.active = Some(active);
        Ok(())
    }

    /// ストリームを閉じる。開いていなければフレームを消すだけ。
    pub fn stop_capture(&mut self) {
        self.active = None;
        // 落とすとグラフが止まり（上流のストリーミングスレッドが止まるまで
        // 待つ）、フィルターが外れる。音声ピンのストリーミングスレッドも止まる
        self.graph = None;
        // 音声のバックエンドから見て、繋いだ音声ピンは無くなった
        self.pin_feed.clear();
        // 止めてから消す。先に消すと、止まる前の 1 枚が残る
        self.frames.reset();
    }

    pub fn link_state(&self) -> VideoLinkState {
        self.log_first_pin_chunk();
        VideoLinkState {
            capturing: self.graph.is_some(),
            since_last_frame: self.frames.since_last_frame(),
            // グラフのイベントをここで読む。ワーカーの監視の周期で呼ばれるので、
            // そのまま EC_DEVICE_LOST を拾う周期になる
            device_lost: self
                .graph
                .as_ref()
                .is_some_and(|graph| graph.poll_device_lost()),
        }
    }

    /// 繋いだ音声ピンから実際に届いた塊の長さを、グラフごとに 1 度だけログへ残す。
    ///
    /// アロケーターの大きさ（`音声ピンを繋いだ` のログ）は 1 塊の上限で、フィルターが
    /// それより短く区切って渡してくることがある。`Receive` の中ではログを出せない
    /// （確保が起きる）ので、ワーカーの監視の周期で読む `link_state` から出す。
    fn log_first_pin_chunk(&self) {
        if self.pin_chunk_logged.get() {
            return;
        }
        let Some(AudioPinState::Connected(connection)) =
            self.active.as_ref().map(|active| &active.audio_pin)
        else {
            return;
        };
        let Some(bytes) = self.pin_feed.observed_chunk_bytes() else {
            return;
        };
        self.pin_chunk_logged.set(true);
        info!(
            "音声ピンから塊が届き始めた（ここまでで最も長い塊: {} バイト = {:?} ms、{}）",
            bytes,
            connection.format.chunk_ms(bytes),
            connection.format.summary()
        );
    }

    pub fn active(&self) -> Option<ActiveVideo> {
        self.active.clone()
    }
}

impl Drop for DirectShowCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_appends_the_suffix() {
        assert_eq!(
            display_name("OBS Virtual Camera"),
            "OBS Virtual Camera (DirectShow)"
        );
    }

    #[test]
    fn friendly_name_strips_the_suffix() {
        assert_eq!(
            friendly_name("OBS Virtual Camera (DirectShow)"),
            Some("OBS Virtual Camera")
        );
    }

    #[test]
    fn friendly_name_without_suffix_is_none() {
        // Media Foundation のデバイス名はそのまま
        assert_eq!(friendly_name("Game Capture HD60"), None);
        // 印だけで名前が空のものは DirectShow のデバイスとみなさない
        assert_eq!(friendly_name(" (DirectShow)"), None);
        assert_eq!(friendly_name(""), None);
    }

    #[test]
    fn friendly_name_round_trips_display_name() {
        let name = "USB Video (DirectShow) Device";
        assert_eq!(friendly_name(&display_name(name)), Some(name));
    }

    fn capture() -> DirectShowCapture {
        DirectShowCapture::new(
            VideoFrames::new(),
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::default(),
            AudioPinFeed::new(),
        )
    }

    // 実行: cargo test directshow -- --ignored --nocapture --test-threads=1
    // 並列に走らせると、同じ仮想カメラを複数のスレッドが同時に開き、
    // 対応形式が空で返ることがある（アプリではワーカー 1 本だけが開くので起きない）
    #[test]
    #[ignore = "DirectShow の映像入力デバイス（OBS の仮想カメラなど）が必要"]
    fn list_friendly_names_finds_directshow_devices() {
        let capture = capture();
        assert!(capture._com.is_some(), "COM を初期化できない");
        let names = capture.list_friendly_names();
        println!("DirectShow の映像デバイス: {names:?}");
        assert!(
            !names.is_empty(),
            "DirectShow のデバイスが 1 つも見つからない"
        );
    }

    #[test]
    #[ignore = "DirectShow の映像入力デバイス（OBS の仮想カメラなど）が必要"]
    fn capabilities_lists_formats_of_every_directshow_device() {
        let capture = capture();
        for name in capture.list_friendly_names() {
            let display = display_name(&name);
            let caps = capture.capabilities(&display);
            println!("{display}: {caps:?}");
            assert!(caps.is_ok(), "{display} の能力を取得できない: {caps:?}");
        }
    }

    #[test]
    #[ignore = "DirectShow の映像入力デバイス（OBS の仮想カメラなど）が必要。OBS なら仮想カメラを開始しておく"]
    fn start_capture_receives_frames_from_the_first_directshow_device() {
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .next()
            .expect("DirectShow のデバイスがある");
        let display = display_name(&name);
        capture
            .start_capture(&display, None, None, None, false)
            .expect("開ける");
        println!("開いた: {:?}", capture.active());

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while capture.frames.latest().is_none() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let frame = capture.frames.latest().expect("5 秒以内にフレームが届く");
        println!("フレーム: {}x{}", frame.width, frame.height);
        assert!(capture.link_state().capturing);

        capture.stop_capture();
        assert!(!capture.link_state().capturing);
        assert!(capture.frames.latest().is_none());
    }

    #[test]
    #[ignore = "DirectShow のキャプチャーボード（AVerMedia GC551 など）に入力信号を入れておく"]
    fn start_capture_without_resolution_opens_the_current_resolution() {
        // 実行: cargo test start_capture_without_resolution -- --ignored --nocapture
        // 解像度が未指定（初回）なら、デバイスのいまの解像度で開く（#391）。GC551 は
        // 入力信号の解像度を返し、それ以外で開くと警告画面になる
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .next()
            .expect("DirectShow のデバイスがある");
        let display = display_name(&name);
        let entry = DirectShowCapture::find(&display).expect("見つかる");
        let current = graph::query_candidates(&entry)
            .expect("対応形式を読める")
            .and_then(|queried| queried.current)
            .expect("いまの解像度を読める");
        println!("{display} のいまの解像度: {current:?}");

        capture
            .start_capture(&display, None, None, None, false)
            .expect("開ける");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while capture.frames.latest().is_none() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let frame = capture.frames.latest().expect("5 秒以内にフレームが届く");
        // 警告画面も同じ大きさのフレームとして届くので、映っているかは目で確かめる
        assert_eq!((frame.width as u32, frame.height as u32), current);
        capture.stop_capture();
    }

    #[test]
    #[ignore = "DirectShow のキャプチャーボード（AVerMedia GC551）に 1920x1080 60Hz の入力信号を入れておく"]
    fn capabilities_list_only_the_input_fps_for_ranged_formats() {
        // 実行: cargo test capabilities_list_only_the_input_fps -- --ignored --nocapture
        // #410: fps が範囲の形式では、GetFormat のいまの fps（入力信号の 60）だけを出す
        let capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .next()
            .expect("DirectShow のデバイスがある");
        let display = display_name(&name);
        let entry = DirectShowCapture::find(&display).expect("見つかる");
        let queried = graph::query_candidates(&entry)
            .expect("対応形式を読める")
            .expect("IAMStreamConfig がある");
        println!(
            "いまの解像度: {:?}、いまの fps: {:?}",
            queried.current, queried.current_fps
        );
        let capabilities = capture.capabilities(&display).expect("能力を読める");
        for capability in &capabilities {
            let fps: Vec<u32> = capability
                .modes
                .iter()
                .filter(|mode| (mode.width, mode.height) == (1920, 1080))
                .map(|mode| mode.fps)
                .collect();
            println!("{}: 1920x1080 の fps の候補 {:?}", capability.name, fps);
            if !fps.is_empty() {
                assert_eq!(fps, vec![60]);
            }
        }
    }

    #[test]
    #[ignore = "DirectShow のキャプチャーボード（AVerMedia GC551）に 1920x1080 60Hz の入力信号を入れておく"]
    fn start_capture_requested_fps_inside_the_range_reports_the_delivered_rate() {
        // 実行: cargo test start_capture_requested_fps_inside_the_range -- --ignored --nocapture
        // #389。範囲（15〜60fps）の中の 30fps を要求したら 30fps で開くこと。実際に
        // 届く間隔はボードしだい（入力信号の fps で出すものがある）なので、数値を出すだけ
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains("GC551"))
            .expect("GC551 がある");
        let display = display_name(&name);
        capture
            .start_capture(&display, Some((1920, 1080)), Some("YUY2"), Some(30), false)
            .expect("開ける");
        println!("開いた: {:?}", capture.active());
        std::thread::sleep(std::time::Duration::from_secs(10));
        let intervals = capture.frames.stats().intervals.expect("フレームが届く");
        println!("届いた間隔: {intervals:?}");
        capture.stop_capture();
    }

    /// `seconds` 秒のあいだ 10ms ごとに最後のフレームからの経過を見て、映像が
    /// 100ms 以上途切れたところを `(経過秒, 途切れた ms)` で返す。
    fn video_stalls(capture: &DirectShowCapture, seconds: u64) -> Vec<(u64, u128)> {
        let started = Instant::now();
        let mut stalls: Vec<(u64, u128)> = Vec::new();
        let mut longest_in_stall = 0;
        while started.elapsed() < std::time::Duration::from_secs(seconds) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let since = capture
                .frames
                .since_last_frame()
                .map_or(0, |d| d.as_millis());
            if since >= 100 {
                longest_in_stall = longest_in_stall.max(since);
            } else if longest_in_stall > 0 {
                stalls.push((started.elapsed().as_secs(), longest_in_stall));
                longest_in_stall = 0;
            }
        }
        stalls
    }

    #[test]
    #[ignore = "音声ピン付きの DirectShow のキャプチャーボード（AVerMedia GC551）に 1920x1080 の入力信号を入れておく。2 分かかる"]
    fn start_capture_with_audio_pin_does_not_add_video_stalls() {
        // 実行: cargo test start_capture_with_audio_pin_does_not_add_video_stalls -- --ignored --nocapture
        // #388。音声ピンを繋いでも映像の途切れが増えないこと。繋がずに 60 秒、繋いで 60 秒
        // 開き、映像が 100ms 以上途切れたところを数える（音声のバックエンドは差し込まない）。
        // GC551 は音声ピンを繋がなくても数分に 1 度ほど 200〜400ms 止まる（2026-10-01 の実測）
        // ので、1 回の差は許す
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains("GC551"))
            .expect("GC551 がある");
        let display = display_name(&name);
        let mut results = Vec::new();
        for connect in [false, true] {
            capture
                .start_capture(&display, Some((1920, 1080)), None, None, connect)
                .expect("開ける");
            println!("音声ピンを繋ぐ {connect}: {:?}", capture.active());
            let stalls = video_stalls(&capture, 60);
            println!("音声ピンを繋ぐ {connect}: 映像の途切れ {stalls:?}");
            results.push(stalls.len());
            capture.stop_capture();
        }
        assert!(
            results[1] <= results[0] + 1,
            "音声ピンを繋ぐと映像の途切れが増える: {results:?}"
        );
    }

    #[test]
    #[ignore = "音声ピン付きの DirectShow のキャプチャーボード（AVerMedia GC551）に 1920x1080 60Hz の入力信号を入れておく"]
    fn sample_timestamps_track_the_arrival_time() {
        // 実行: cargo test sample_timestamps_track_the_arrival_time -- --ignored --nocapture
        // #406。映像と音声ピンのサンプルに `IMediaSample::GetTime` のタイムスタンプが
        // 付くか、付くなら到着時刻と比べてどれだけ揺れが小さいかを測る。基準時計を
        // 外したグラフ（アプリと同じ）と付けたグラフで、それぞれ
        // `CAPTURECARD_VIEWER_PIN_TEST_SECONDS` 秒（既定 10 秒）。数値を出すだけで、
        // 結果は `docs/design/recording.md` の「DirectShow のサンプルのタイムスタンプ（#406）」
        let seconds: u64 = std::env::var("CAPTURECARD_VIEWER_PIN_TEST_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let _ = timestamp_probe::BASE.set(Instant::now());
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains("GC551"))
            .expect("GC551 がある");
        let display = display_name(&name);
        for default_clock in [false, true] {
            timestamp_probe::set_default_clock(default_clock);
            capture
                .start_capture(&display, Some((1920, 1080)), None, None, true)
                .expect("開ける");
            println!("基準時計を付ける {default_clock}: {:?}", capture.active());
            // 開いた直後の詰まりを避ける
            std::thread::sleep(std::time::Duration::from_secs(2));
            timestamp_probe::VIDEO.reset();
            timestamp_probe::AUDIO.reset();
            timestamp_probe::set_recording(true);
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            // 記録を止め、グラフを止めてから読む。`record` は番号を取ってから値を
            // 書くので、流れている間に読むと最後の行が書きかけになりうる。記録を
            // 先に止めるのは、止める途中に届くサンプルを数えないため
            timestamp_probe::set_recording(false);
            capture.stop_capture();
            let video = timestamp_probe::VIDEO.take();
            let audio = timestamp_probe::AUDIO.take();
            let video_lag = timestamp_probe::report("映像", &video);
            let audio_lag = timestamp_probe::report("音声", &audio);
            if let (Some(v), Some(a)) = (video_lag, audio_lag) {
                println!(
                    "到着 − タイムスタンプの平均: 映像 {v:.3}ms 音声 {a:.3}ms（差 {:.3}ms）",
                    a - v
                );
            }
        }
        timestamp_probe::set_default_clock(false);
    }

    #[test]
    #[ignore = "OBS の仮想カメラが必要。開始したあと 30 秒以内に OBS 側で仮想カメラを止める（または OBS を終了する）"]
    fn link_state_reports_device_lost_when_the_source_goes_away() {
        // 実行: cargo test link_state_reports_device_lost -- --ignored --nocapture
        let mut capture = capture();
        let name = capture
            .list_friendly_names()
            .into_iter()
            .find(|name| name.contains("OBS"))
            .expect("OBS の仮想カメラがある");
        let display = display_name(&name);
        capture
            .start_capture(&display, None, None, None, false)
            .expect("開ける");
        println!("開いた。30 秒以内に OBS 側で仮想カメラを止める");

        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let mut state = capture.link_state();
        while !state.device_lost && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            state = capture.link_state();
        }
        println!("観測値: {state:?}");
        assert!(state.device_lost, "30 秒以内に喪失の知らせが届く");
        // 一度立ったら閉じるまで下りない
        assert!(capture.link_state().device_lost);
        capture.stop_capture();
        assert!(!capture.link_state().device_lost);
    }
}
