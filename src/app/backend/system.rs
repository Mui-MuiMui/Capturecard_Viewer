//! 本番のバックエンド。`VideoCapture` / `AudioCapture` を trait に包むだけで、
//! 中身には手を入れない。

use super::{AudioBackend, BackendShared, DeviceBackends, VideoBackend};
use crate::audio::{
    self, ActiveAudio, AudioCapabilities, AudioCapture, AudioDirection, AudioError,
    PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::video::{ActiveVideo, DeviceCapabilities, VideoCapture, VideoError, VideoLinkState};
use std::sync::Arc;

/// 本番のバックエンド。映像は Media Foundation（nokhwa）、音声は WASAPI（cpal）。
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
        (
            Box::new(VideoCapture::new(frames, color_conversion, repaint_waker)),
            Box::new(AudioCapture::new(audio_controls)),
        )
    }
}

impl VideoBackend for VideoCapture {
    fn list_devices(&self) -> Vec<(String, String)> {
        VideoCapture::list_devices()
    }

    fn capabilities(&self, device_name: Option<&str>) -> Result<DeviceCapabilities, VideoError> {
        VideoCapture::get_device_capabilities(device_name)
    }

    fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        VideoCapture::start_capture(self, device_name, resolution, format, fps)
    }

    fn stop_capture(&mut self) {
        VideoCapture::stop_capture(self);
    }

    fn link_state(&self) -> VideoLinkState {
        VideoCapture::link_state(self)
    }

    fn active(&self) -> Option<ActiveVideo> {
        VideoCapture::active(self)
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
