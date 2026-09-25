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
//! | `devices.rs` | 列挙（`ICreateDevEnum`）、対応形式（`IAMStreamConfig::GetStreamCaps`）、開く形式の選び方 |
//! | `graph.rs` | フィルターグラフの組み立て・開始・停止・破棄 |
//! | `filter.rs` | サンプルを受け取る自前のレンダラーフィルター（`IBaseFilter` / `IPin` / `IMemInputPin`） |
//! | `media_type.rs` | `AM_MEDIA_TYPE` の読み書き、COM の初期化 |
//!
//! **デバイス名には「(DirectShow)」を添える**（`display_name`）。設定に
//! 保存されるのもこの名前で、Media Foundation の経路とどちらで開くかは
//! この印で決まる。言語によって変えない（設定に残る識別子なので、画面の
//! 言語を切り替えると別のデバイスになってしまう）。

mod devices;
mod filter;
mod graph;
mod media_type;

use log::{debug, info, warn};
use std::sync::Arc;
use std::time::Instant;

use super::capabilities::DeviceCapabilities;
use super::capture::{ActiveVideo, VideoLinkState};
use super::color::SharedColorConversion;
use super::frame_buffer::VideoFrames;
use super::frame_sink::FrameSink;
use super::{elapsed_ms, VideoError};
use crate::repaint::RepaintWaker;
use devices::DeviceEntry;
use graph::{CaptureGraph, FormatRequest, GraphError};
use media_type::ComApartment;

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
    /// **最後に落とす。** グラフや名札（COM のオブジェクト）を手放してから
    /// COM の初期化を戻す。フィールドは宣言順に落ちるので、末尾に置いてある
    _com: Option<ComApartment>,
}

impl DirectShowCapture {
    /// 引数の意味は `VideoCapture::new` と同じ。**デバイスワーカースレッドの
    /// 中で作る**（ここでそのスレッドの COM を初期化する）。
    pub fn new(
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: RepaintWaker,
    ) -> Self {
        let com = match ComApartment::enter() {
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
            _com: com,
        }
    }

    /// デバイスの表示名（`FriendlyName`、「(DirectShow)」なし）の一覧。
    /// 失敗しても空の一覧を返す。
    pub fn list_friendly_names(&self) -> Vec<String> {
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
                names
            }
            Err(e) => {
                warn!(
                    "DirectShow の映像デバイスの列挙に失敗した（{:.1}ms）: {}",
                    elapsed_ms(start),
                    e
                );
                Vec::new()
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
    /// 受け取れる形式（YUY2 / MJPEG / RGB24）が 1 つも無ければ空の一覧を返す。
    pub fn capabilities(&self, display: &str) -> Result<DeviceCapabilities, VideoError> {
        let start = Instant::now();
        let entry = Self::find(display)?;
        let candidates = graph::query_candidates(&entry).map_err(|e| match e {
            GraphError::Open(e) | GraphError::Stream(e) => VideoError::CameraOpenFailed {
                device: display.to_string(),
                source: e.to_string(),
            },
        })?;
        let Some(candidates) = candidates else {
            warn!(
                "DirectShow のデバイス {} は IAMStreamConfig を持たないので、対応形式を出せない",
                display
            );
            return Ok(Vec::new());
        };
        let capabilities = devices::capabilities_from_candidates(&candidates);
        debug!(
            "DirectShow のデバイス能力の内訳（{}、{:.1}ms）: {}",
            display,
            elapsed_ms(start),
            capabilities
                .iter()
                .map(|capability| format!("{}: {} 件", capability.name, capability.modes.len()))
                .collect::<Vec<_>>()
                .join("、")
        );
        Ok(capabilities)
    }

    /// ストリームを開く。既に開いていれば閉じてから開き直す。
    /// `display` は表示名（「(DirectShow)」付き）。
    pub fn start_capture(
        &mut self,
        display: &str,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        self.stop_capture();

        let entry = Self::find(display)?;
        let sink = FrameSink::new(
            &self.frames,
            self.color_conversion.clone(),
            self.repaint_waker.clone(),
        );
        let start = Instant::now();
        let graph = CaptureGraph::start(
            &entry,
            FormatRequest {
                resolution,
                format,
                fps,
            },
            sink,
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

        let active = ActiveVideo {
            device_name: display.to_string(),
            resolution: Some((graph.format.width, graph.format.height)),
            format: Some(graph.format_name().to_string()),
            requested_fps: graph.requested_fps,
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
        // 待つ）、フィルターが外れる
        self.graph = None;
        // 止めてから消す。先に消すと、止まる前の 1 枚が残る
        self.frames.reset();
    }

    pub fn link_state(&self) -> VideoLinkState {
        VideoLinkState {
            capturing: self.graph.is_some(),
            since_last_frame: self.frames.since_last_frame(),
        }
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
            .start_capture(&display, None, None, None)
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
}
