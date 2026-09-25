//! 本番のバックエンド。映像は Media Foundation（`VideoCapture`）と DirectShow
//! （`DirectShowCapture`）を `SystemVideo` で束ね、音声は `AudioCapture` を
//! trait に包むだけ。どれも中身には手を入れない。
//!
//! **Media Foundation と DirectShow のどちらで開くかは、デバイス名だけで
//! 決まる**（`route_for`）。DirectShow のデバイスは名前に「(DirectShow)」が
//! 付いていて、設定にもその名前で残る。一覧は Media Foundation を優先し、
//! DirectShow にしか無いものだけを足す（`merge_video_devices`）。
//! Web カメラやキャプチャーボードの多くは両方に出るが、同じデバイスを
//! 2 つ並べても選び間違えるだけなので、実績のある Media Foundation を使う。

use super::{AudioBackend, BackendShared, DeviceBackends, VideoBackend};
use crate::audio::{
    self, ActiveAudio, AudioCapabilities, AudioCapture, AudioDirection, AudioError,
    PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::video::{
    directshow_display_name, directshow_friendly_name, ActiveVideo, DeviceCapabilities,
    DirectShowCapture, VideoCapture, VideoError, VideoLinkState,
};
use std::sync::Arc;

/// 本番のバックエンド。映像は Media Foundation（nokhwa）と DirectShow、音声は
/// WASAPI（cpal）。
pub(in crate::app) struct SystemBackends;

impl DeviceBackends for SystemBackends {
    fn create(
        self: Box<Self>,
        shared: BackendShared,
    ) -> (Box<dyn VideoBackend>, Box<dyn AudioBackend>) {
        let BackendShared {
            frames,
            color_conversion,
            audio_controls,
            repaint_waker,
        } = shared;
        // どちらも同じフレームバッファへ積む。同時に開くのは片方だけ
        let video = SystemVideo {
            media_foundation: VideoCapture::new(
                frames.clone(),
                color_conversion.clone(),
                repaint_waker.clone(),
            ),
            direct_show: DirectShowCapture::new(frames, color_conversion, repaint_waker),
            open: None,
        };
        (Box::new(video), Box::new(AudioCapture::new(audio_controls)))
    }
}

/// どちらの経路でデバイスを扱うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoRoute {
    MediaFoundation,
    DirectShow,
}

/// デバイス名から経路を決める。「(DirectShow)」が付いていれば DirectShow、
/// それ以外（未指定を含む）は Media Foundation。
fn route_for(device_name: Option<&str>) -> VideoRoute {
    match device_name.and_then(directshow_friendly_name) {
        Some(_) => VideoRoute::DirectShow,
        None => VideoRoute::MediaFoundation,
    }
}

/// Media Foundation の一覧に、DirectShow にしか無いデバイスを足す。
///
/// 照合は表示名で行う。**同じ名前が両方にあれば Media Foundation のほうだけを
/// 残す。** DirectShow 側で同じ名前が重なっていれば 1 つにする（同じ名前では
/// 選び分けられない）。DirectShow のデバイスの説明は空にする（設定画面は
/// 「名前 (説明)」と出すので、「(DirectShow)」が二重に見えるのを避ける）。
fn merge_video_devices(
    media_foundation: Vec<(String, String)>,
    direct_show: Vec<String>,
) -> Vec<(String, String)> {
    let mut merged = media_foundation;
    let mut added: Vec<String> = Vec::new();
    for name in direct_show {
        let known = merged.iter().any(|(mf_name, _)| *mf_name == name);
        if known || added.contains(&name) {
            continue;
        }
        added.push(name);
    }
    merged.extend(
        added
            .iter()
            .map(|name| (directshow_display_name(name), String::new())),
    );
    merged
}

/// Media Foundation と DirectShow を 1 つの映像バックエンドに束ねたもの。
///
/// **同時に開くのは片方だけ。** 開き直すときは、いま開いている側を閉じて
/// から相手の側を開く。
struct SystemVideo {
    media_foundation: VideoCapture,
    direct_show: DirectShowCapture,
    /// いま開いている経路。閉じていれば `None`
    open: Option<VideoRoute>,
}

impl VideoBackend for SystemVideo {
    fn list_devices(&self) -> Vec<(String, String)> {
        merge_video_devices(
            VideoCapture::list_devices(),
            self.direct_show.list_friendly_names(),
        )
    }

    fn capabilities(&self, device_name: Option<&str>) -> Result<DeviceCapabilities, VideoError> {
        match (route_for(device_name), device_name) {
            (VideoRoute::DirectShow, Some(name)) => self.direct_show.capabilities(name),
            _ => VideoCapture::get_device_capabilities(device_name),
        }
    }

    fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        self.stop_capture();
        let route = route_for(device_name);
        let result = match (route, device_name) {
            (VideoRoute::DirectShow, Some(name)) => self
                .direct_show
                .start_capture(name, resolution, format, fps),
            _ => self
                .media_foundation
                .start_capture(device_name, resolution, format, fps),
        };
        if result.is_ok() {
            self.open = Some(route);
        }
        result
    }

    fn stop_capture(&mut self) {
        // 閉じている間も、今までどおり Media Foundation 側の stop を呼んで
        // フレームを消す（呼び出し側から見た振る舞いを変えない）
        match self.open.take() {
            Some(VideoRoute::DirectShow) => self.direct_show.stop_capture(),
            Some(VideoRoute::MediaFoundation) | None => self.media_foundation.stop_capture(),
        }
    }

    fn link_state(&self) -> VideoLinkState {
        match self.open {
            Some(VideoRoute::DirectShow) => self.direct_show.link_state(),
            _ => self.media_foundation.link_state(),
        }
    }

    fn active(&self) -> Option<ActiveVideo> {
        match self.open {
            Some(VideoRoute::DirectShow) => self.direct_show.active(),
            _ => self.media_foundation.active(),
        }
    }
}

impl AudioBackend for AudioCapture {
    fn list_input_devices(&self) -> Vec<String> {
        AudioCapture::list_input_devices(self)
    }

    fn list_output_devices(&self) -> Vec<String> {
        AudioCapture::list_output_devices(self)
    }

    fn default_input_device_name(&self) -> Option<String> {
        AudioCapture::default_input_device_name(self)
    }

    fn default_output_device_name(&self) -> Option<String> {
        AudioCapture::default_output_device_name(self)
    }

    fn capabilities(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<AudioCapabilities, AudioError> {
        // **`AudioCapture` のホストは使わない自由関数を呼ぶ。** 元から
        // そういう作りで、`AudioCapture` を持たないスレッドからも使える
        audio::query_capabilities(direction, device_name)
    }

    fn start_passthrough(&mut self, request: &PassthroughRequest<'_>) -> Result<(), AudioError> {
        AudioCapture::start_passthrough(self, request)
    }

    fn stop_capture(&mut self) {
        AudioCapture::stop_capture(self);
    }

    fn active(&self) -> Option<ActiveAudio> {
        AudioCapture::active(self)
    }

    fn resample_status(&self) -> Option<ResampleStatus> {
        AudioCapture::resample_status(self)
    }

    fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>> {
        AudioCapture::resample_telemetry(self).cloned()
    }

    fn underrun_count(&self) -> Option<u32> {
        AudioCapture::underrun_count(self)
    }

    fn take_stream_error(&self) -> bool {
        AudioCapture::take_stream_error(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[(String, String)]) -> Vec<&str> {
        list.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test]
    fn merge_video_devices_prefers_media_foundation_for_the_same_name() {
        let merged = merge_video_devices(
            vec![("USB Video".to_string(), "MF".to_string())],
            vec!["USB Video".to_string(), "OBS Virtual Camera".to_string()],
        );
        assert_eq!(
            names(&merged),
            vec!["USB Video", "OBS Virtual Camera (DirectShow)"]
        );
        // Media Foundation の説明は残し、DirectShow の説明は空
        assert_eq!(merged[0].1, "MF");
        assert_eq!(merged[1].1, "");
    }

    #[test]
    fn merge_video_devices_collapses_duplicate_directshow_names() {
        let merged = merge_video_devices(
            Vec::new(),
            vec!["Virtual Cam".to_string(), "Virtual Cam".to_string()],
        );
        assert_eq!(names(&merged), vec!["Virtual Cam (DirectShow)"]);
    }

    #[test]
    fn merge_video_devices_without_directshow_keeps_the_media_foundation_list() {
        let mf = vec![
            ("A".to_string(), String::new()),
            ("B".to_string(), String::new()),
        ];
        assert_eq!(merge_video_devices(mf.clone(), Vec::new()), mf);
    }

    #[test]
    fn merge_video_devices_both_empty_is_empty() {
        assert!(merge_video_devices(Vec::new(), Vec::new()).is_empty());
    }

    #[test]
    fn route_for_uses_the_directshow_suffix() {
        assert_eq!(
            route_for(Some("OBS Virtual Camera (DirectShow)")),
            VideoRoute::DirectShow
        );
        assert_eq!(route_for(Some("USB Video")), VideoRoute::MediaFoundation);
        // 未指定は今までどおり Media Foundation の先頭
        assert_eq!(route_for(None), VideoRoute::MediaFoundation);
    }
}
