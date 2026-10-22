//! 本番のバックエンド。映像は Media Foundation（`VideoCapture`）と DirectShow
//! （`DirectShowCapture`）を `SystemVideo` で束ね、音声は `AudioCapture` を
//! trait に包むだけ。どれも中身には手を入れない。
//!
//! **Media Foundation と DirectShow のどちらで開くかは、デバイス名と設定の
//! 「映像の開き方」（`video.backend`）で決まる**（`route_for`）。一覧は
//! Media Foundation を優先し、DirectShow にしか無いものだけを「(DirectShow)」を
//! 添えて足す（`merge_video_devices`）。Web カメラやキャプチャーボードの多くは
//! 両方に出るが、同じデバイスを 2 つ並べても選び間違えるだけなので 1 つにする。
//!
//! **自動では名前の付いたデバイスをまず DirectShow で開き、開けなければ同じ
//! 呼び出しの中で Media Foundation で開く**（`attempt_with_fallback`、#490）。両方に
//! 出るボードは DirectShow で開いたほうが表示までの遅れが短く（`docs/LATENCY.md`）、
//! Media Foundation では開けないボード（AVerMedia GC551、#387）もある。DirectShow の
//! 一覧に無いデバイスは今までどおり Media Foundation で開く。対応形式の問い合わせも
//! 同じ規則で引く。開き方を Media Foundation / DirectShow に固定した設定では倒さない。

use super::{
    AudioBackend, BackendShared, CaptureRequest, DeviceBackends, VideoBackend, VideoEnumeration,
};
use crate::audio::{
    self, ActiveAudio, AudioCapabilities, AudioCapture, AudioDirection, AudioError, AudioPinFeed,
    AudioPinPresence, PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::settings::VideoBackendSetting;
use crate::video::{
    ActiveVideo, DeviceCapabilities, DirectShowCapture, VideoCapture, VideoError, VideoLinkState,
};
use std::sync::Arc;

use super::system_devices::merge_video_devices;
use super::system_route::{
    attempt_with_fallback, capabilities_with_fallback, Operation, VideoRoute,
};

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
            audio_tap,
            repaint_waker,
        } = shared;
        // 映像の音声ピンと音声のバックエンドをつなぐ差し込み口（#388）。ワーカーの
        // 中で閉じた共有で、UI スレッドからは触らない（`ResampleTelemetry` と同じ扱い）
        let pin_feed = AudioPinFeed::new();
        // どちらも同じフレームバッファへ積む。同時に開くのは片方だけ
        let video = SystemVideo {
            media_foundation: VideoCapture::new(
                frames.clone(),
                color_conversion.clone(),
                repaint_waker.clone(),
            ),
            direct_show: DirectShowCapture::new(
                frames,
                color_conversion,
                repaint_waker,
                pin_feed.clone(),
            ),
            open: None,
        };
        (
            Box::new(video),
            Box::new(AudioCapture::new(audio_controls, audio_tap, pin_feed)),
        )
    }
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

    fn capabilities(
        &self,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
    ) -> Result<DeviceCapabilities, VideoError> {
        // 開くときと同じ規則で経路を決める（#249）。設定ダイアログの能力
        // キャッシュはデバイス名と開き方の組で引くので、DirectShow で開く
        // 設定なら選択肢も DirectShow 側の対応形式になる。自動では開くときと
        // 同じくまず DirectShow に問い合わせ、DirectShow の一覧に無いか取れなければ
        // Media Foundation に問い合わせる（#490）。DirectShow が空の一覧を返したときも
        // Media Foundation に問い合わせる（開くときは DirectShow のグラフが失敗して
        // Media Foundation で開くため、`capabilities_with_fallback`）。キャッシュの
        // (名前, 自動) には開くときと同じ経路の対応形式が入るので食い違わない
        let direct_show = &self.direct_show;
        // 音声ピンを繋ぐ指定は見ない。能力キャッシュの鍵は (名前, 開き方) で、
        // 入力の種類を含まないため。違いが出るのは DirectShow の一覧にあって
        // 開けないときだけ（音声ピンの指定があると開くときは Media Foundation へ
        // 倒さない、#425）で、そのときは開けないので選択肢の食い違いは問題にならない
        capabilities_with_fallback(device_name, backend, |route, name| match (route, name) {
            (VideoRoute::DirectShow, Some(name)) => direct_show.capabilities(name),
            (_, name) => VideoCapture::get_device_capabilities(name),
        })
    }

    fn start_capture(&mut self, request: &CaptureRequest<'_>) -> Result<(), VideoError> {
        self.stop_capture();
        let CaptureRequest {
            device_name,
            resolution,
            format,
            fps,
            backend,
            connect_audio_pin,
        } = *request;
        // 自動ではまず DirectShow で開き、開けなければ Media Foundation で開く（#490）。
        // `open` には実際に開けた経路を入れるので、`link_state` / `stop_capture` /
        // `active` もそちらを見る（「接続状態」タブの「開き方」も `active` から出る）。
        // 音声ピンを繋ぐ指定があれば、DirectShow の一覧にあって開けなかったときは
        // Media Foundation へ倒さない（#425、`route_for` / `attempt_with_fallback`）
        let media_foundation = &mut self.media_foundation;
        let direct_show = &mut self.direct_show;
        let (result, route) = attempt_with_fallback(
            device_name,
            backend,
            connect_audio_pin,
            Operation::Connect,
            |route, name| match (route, name) {
                (VideoRoute::DirectShow, Some(name)) => {
                    direct_show.start_capture(name, resolution, format, fps, connect_audio_pin)
                }
                (_, name) => media_foundation.start_capture(name, resolution, format, fps),
            },
        );
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

    fn audio_pin_presence(&mut self) -> Vec<(String, AudioPinPresence)> {
        self.direct_show.audio_pin_presence()
    }

    fn enumerate(&self) -> VideoEnumeration {
        let media_foundation = VideoCapture::try_list_devices();
        let direct_show = self.direct_show.try_list_friendly_names();
        enumeration_from(media_foundation, direct_show)
    }
}

/// 経路ごとの列挙結果から `VideoEnumeration` を組み立てる。
///
/// 設定に書ける名前は `list_devices` と同じく `merge_video_devices` で作る。
/// **どちらかの経路が失敗していたら作らない**（`VideoEnumeration::selectable`）。
fn enumeration_from(
    media_foundation: Result<Vec<(String, String)>, VideoError>,
    direct_show: Result<Vec<String>, VideoError>,
) -> VideoEnumeration {
    let selectable = match (&media_foundation, &direct_show) {
        (Ok(mf), Ok(ds)) => Some(
            merge_video_devices(mf.clone(), ds.clone())
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        ),
        _ => None,
    };
    let media_foundation =
        media_foundation.map(|devices| devices.into_iter().map(|(name, _)| name).collect());
    VideoEnumeration {
        sources: vec![
            ("Media Foundation", media_foundation),
            ("DirectShow", direct_show),
        ],
        selectable,
    }
}

impl AudioBackend for AudioCapture {
    fn list_input_devices(&self) -> Vec<String> {
        AudioCapture::list_input_devices(self)
    }

    fn list_output_devices(&self) -> Vec<String> {
        AudioCapture::list_output_devices(self)
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

    fn dropped_frame_count(&self) -> Option<u32> {
        AudioCapture::dropped_frame_count(self)
    }

    fn xrun_count(&self) -> Option<u32> {
        AudioCapture::xrun_count(self)
    }

    fn take_stream_error(&self) -> bool {
        AudioCapture::take_stream_error(self)
    }

    fn enumerate_devices(&self, direction: AudioDirection) -> Result<Vec<String>, AudioError> {
        AudioCapture::try_list_devices(self, direction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_from_merges_the_selectable_names_like_list_devices() {
        // 設定に書ける名前は `list_devices` と同じ表記（「(DirectShow)」付き）
        let enumeration = enumeration_from(
            Ok(vec![("USB Video".to_string(), "MF".to_string())]),
            Ok(vec![
                "USB Video".to_string(),
                "OBS Virtual Camera".to_string(),
            ]),
        );
        assert_eq!(
            enumeration.selectable,
            Some(vec![
                "USB Video".to_string(),
                "OBS Virtual Camera (DirectShow)".to_string()
            ])
        );
        // ログには経路ごとの生の名前を出す
        assert_eq!(enumeration.sources.len(), 2);
        assert_eq!(enumeration.sources[0].0, "Media Foundation");
        assert_eq!(enumeration.sources[1].0, "DirectShow");
        assert_eq!(
            enumeration.sources[1].1,
            Ok(vec![
                "USB Video".to_string(),
                "OBS Virtual Camera".to_string()
            ])
        );
    }

    #[test]
    fn enumeration_from_without_selectable_names_when_a_source_failed() {
        // 失敗した経路に目当てのデバイスが居たかもしれないので、判定には使わせない
        let failure = VideoError::DeviceQueryFailed("E_FAIL".to_string());
        let enumeration = enumeration_from(Ok(Vec::new()), Err(failure.clone()));
        assert_eq!(enumeration.selectable, None);
        assert_eq!(enumeration.sources[1].1, Err(failure));
        assert_eq!(enumeration.sources[0].1, Ok(Vec::new()));
    }
}
