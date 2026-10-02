//! 本番のバックエンド。映像は Media Foundation（`VideoCapture`）と DirectShow
//! （`DirectShowCapture`）を `SystemVideo` で束ね、音声は `AudioCapture` を
//! trait に包むだけ。どれも中身には手を入れない。
//!
//! **Media Foundation と DirectShow のどちらで開くかは、デバイス名と設定の
//! 「映像の開き方」（`video.backend`）で決まる**（`route_for`）。自動なら
//! まず名前で決め、DirectShow のデバイスは名前に「(DirectShow)」が
//! 付いていて、設定にもその名前で残る。一覧は Media Foundation を優先し、
//! DirectShow にしか無いものだけを足す（`merge_video_devices`）。
//! Web カメラやキャプチャーボードの多くは両方に出るが、同じデバイスを
//! 2 つ並べても選び間違えるだけなので、実績のある Media Foundation を使う。
//! それを DirectShow で開きたいときは開き方を DirectShow にする（#237）。
//!
//! **自動のときだけ、Media Foundation で「見つかったが開けない」なら同じ
//! 呼び出しの中で DirectShow でも試す**（`attempt_with_fallback`、#387）。
//! 両方に出るのに Media Foundation では開けないボード（AVerMedia GC551）が
//! あり、自動のままでは永遠に再試行を繰り返すため。対応形式の問い合わせも
//! 同じ規則で倒す。開き方を Media Foundation に固定した設定では倒さない。

use super::{
    AudioBackend, BackendShared, CaptureRequest, DeviceBackends, VideoBackend, VideoEnumeration,
};
use crate::audio::{
    self, ActiveAudio, AudioCapabilities, AudioCapture, AudioDirection, AudioError, AudioPinFeed,
    AudioPinPresence, PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::settings::VideoBackendSetting;
use crate::video::{
    directshow_display_name, directshow_friendly_name, ActiveVideo, DeviceCapabilities,
    DirectShowCapture, VideoCapture, VideoError, VideoLinkState,
};
use log::{debug, info, warn};
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

/// どちらの経路でデバイスを扱うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoRoute {
    MediaFoundation,
    DirectShow,
}

/// デバイス名と設定の「映像の開き方」から、経路とその経路へ渡す名前を決める。
///
/// | 開き方 | 経路 | 渡す名前 |
/// |---|---|---|
/// | 自動 | 「(DirectShow)」付きなら DirectShow、それ以外は Media Foundation | そのまま |
/// | Media Foundation | Media Foundation | 「(DirectShow)」を外した名前 |
/// | DirectShow | DirectShow | そのまま（`DirectShowCapture` が印の有無を問わず探す） |
///
/// **デバイスが未指定なら開き方によらず Media Foundation**（先頭のデバイス）。
/// DirectShow の経路は名前が無いと開けない。
///
/// ここで決めるのは最初に試す経路だけ。自動で Media Foundation が「見つかったが
/// 開けない」ときは、`attempt_with_fallback` が DirectShow でも試す（#387）。
///
/// Media Foundation で「(DirectShow)」を外すのは、印は「DirectShow にしか
/// 無い」という一覧の上の目印で、デバイスの本来の名前ではないため。多くは
/// Media Foundation に居ないので「見つからない」になる（それが正しい結果）。
fn route_for(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
) -> (VideoRoute, Option<&str>) {
    let Some(name) = device_name else {
        return (VideoRoute::MediaFoundation, None);
    };
    let friendly = directshow_friendly_name(name);
    match backend {
        VideoBackendSetting::Auto => match friendly {
            Some(_) => (VideoRoute::DirectShow, Some(name)),
            None => (VideoRoute::MediaFoundation, Some(name)),
        },
        VideoBackendSetting::MediaFoundation => {
            (VideoRoute::MediaFoundation, Some(friendly.unwrap_or(name)))
        }
        VideoBackendSetting::DirectShow => (VideoRoute::DirectShow, Some(name)),
    }
}

/// 「自動」で Media Foundation が失敗したとき、DirectShow で試し直す名前（#387）。
///
/// 返すのは、開き方が自動で、Media Foundation の経路へ名前付きで渡していて、
/// 失敗が「見つかったが開けない」（`CameraOpenFailed` / `StreamOpenFailed`）の
/// ときだけ。名前はそのまま渡す（`DirectShowCapture` は印の有無を問わず探す）。
///
/// **見つからない・列挙に失敗した・1 台も無いときは試さない。** デバイスを
/// 抜いている間の再試行のたびに DirectShow の列挙を足すことになるうえ、
/// Media Foundation に居ないものは一覧の上で「(DirectShow)」付きになっていて、
/// 最初から DirectShow の経路へ行く。開き方を固定した設定（Media Foundation /
/// DirectShow）では、利用者が選んだ経路の結果をそのまま返す。
fn directshow_fallback<'a>(
    route: VideoRoute,
    name: Option<&'a str>,
    backend: VideoBackendSetting,
    error: &VideoError,
) -> Option<&'a str> {
    if backend != VideoBackendSetting::Auto || route != VideoRoute::MediaFoundation {
        return None;
    }
    match error {
        VideoError::CameraOpenFailed { .. } | VideoError::StreamOpenFailed { .. } => name,
        VideoError::DeviceQueryFailed(_)
        | VideoError::DeviceNotFound(_)
        | VideoError::NoDevices => None,
    }
}

/// 経路を決めて `attempt` を呼び、「自動」で Media Foundation が開けなければ
/// **同じ呼び出しの中で** DirectShow でも試す（#387）。返すのは結果と、その
/// 結果を出した経路。`what` はログに出す操作の名前（「接続」など、
/// 「〜に失敗した」と続く名詞）。
///
/// 再試行（`ConnectRetry`）から見ると、Media Foundation → DirectShow は
/// 試行 1 回のまま。DirectShow にも同じ名前が無ければ（`DeviceNotFound`）、
/// あっても開けなければ、**Media Foundation の失敗をそのまま返す。** 利用者が
/// 選んだのは自動で、DirectShow は代わりに試しただけなので、画面に出す理由は
/// 本来の経路のものにする（DirectShow の失敗は WARN でログに残す）。
fn attempt_with_fallback<T>(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    what: &str,
    mut attempt: impl FnMut(VideoRoute, Option<&str>) -> Result<T, VideoError>,
) -> (Result<T, VideoError>, VideoRoute) {
    let (route, name) = route_for(device_name, backend);
    let first = attempt(route, name);
    let Err(first_error) = &first else {
        return (first, route);
    };
    let Some(fallback_name) = directshow_fallback(route, name, backend, first_error) else {
        return (first, route);
    };
    match attempt(VideoRoute::DirectShow, Some(fallback_name)) {
        Ok(value) => {
            info!(
                "開き方が自動で、Media Foundation では{}に失敗したので DirectShow で試し、成功した: {}（Media Foundation の失敗: {}）",
                what, fallback_name, first_error
            );
            (Ok(value), VideoRoute::DirectShow)
        }
        Err(VideoError::DeviceNotFound(_)) => {
            debug!(
                "Media Foundation では{}に失敗し、DirectShow の一覧にも無い: {}",
                what, fallback_name
            );
            (first, route)
        }
        Err(e) => {
            warn!(
                "Media Foundation では{}に失敗し、DirectShow でも失敗した: {}: {}",
                what, fallback_name, e
            );
            (first, route)
        }
    }
}

/// Media Foundation の一覧に、DirectShow にしか無いデバイスを足す。
///
/// 照合は表示名で行う。**同じ名前が両方にあれば Media Foundation のほうだけを
/// 残す**（Media Foundation で開けなければ、自動の開き方なら DirectShow で
/// 開き直す。`attempt_with_fallback`）。DirectShow 側で同じ名前が重なって
/// いれば 1 つにする（同じ名前では
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

    fn capabilities(
        &self,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
    ) -> Result<DeviceCapabilities, VideoError> {
        // 開くときと同じ規則で経路を決める（#249）。設定ダイアログの能力
        // キャッシュはデバイス名と開き方の組で引くので、DirectShow で開く
        // 設定なら選択肢も DirectShow 側の対応形式になる。自動で Media
        // Foundation が開けないデバイスは、開くときと同じく DirectShow の
        // 対応形式を返す（#387）。キャッシュの (名前, 自動) にはこちらが入り、
        // 自動で開くときもやはり DirectShow へ倒れるので食い違わない
        let direct_show = &self.direct_show;
        let (result, _) = attempt_with_fallback(
            device_name,
            backend,
            "対応形式の取得",
            |route, name| match (route, name) {
                (VideoRoute::DirectShow, Some(name)) => direct_show.capabilities(name),
                (_, name) => VideoCapture::get_device_capabilities(name),
            },
        );
        result
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
        // 自動で Media Foundation が開けなければ DirectShow でも試す（#387）。
        // `open` には実際に開けた経路を入れるので、`link_state` / `stop_capture` /
        // `active` もそちらを見る（「接続状態」タブの「開き方」も `active` から出る）。
        // 音声ピンを繋ぐ指定は、倒したときの DirectShow にも渡す（GC551 はこの経路で開く）
        let media_foundation = &mut self.media_foundation;
        let direct_show = &mut self.direct_show;
        let (result, route) =
            attempt_with_fallback(device_name, backend, "接続", |route, name| {
                match (route, name) {
                    (VideoRoute::DirectShow, Some(name)) => {
                        direct_show.start_capture(name, resolution, format, fps, connect_audio_pin)
                    }
                    (_, name) => media_foundation.start_capture(name, resolution, format, fps),
                }
            });
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

    const DS_ONLY: &str = "OBS Virtual Camera (DirectShow)";
    const BOTH: &str = "USB Video";

    #[test]
    fn route_for_auto_uses_the_directshow_suffix() {
        let auto = VideoBackendSetting::Auto;
        assert_eq!(
            route_for(Some(DS_ONLY), auto),
            (VideoRoute::DirectShow, Some(DS_ONLY))
        );
        assert_eq!(
            route_for(Some(BOTH), auto),
            (VideoRoute::MediaFoundation, Some(BOTH))
        );
        // 未指定は今までどおり Media Foundation の先頭
        assert_eq!(route_for(None, auto), (VideoRoute::MediaFoundation, None));
    }

    #[test]
    fn route_for_direct_show_opens_even_a_media_foundation_name_with_directshow() {
        let ds = VideoBackendSetting::DirectShow;
        // 両方に出るデバイス。同じ表示名を DirectShow の一覧から探す
        assert_eq!(
            route_for(Some(BOTH), ds),
            (VideoRoute::DirectShow, Some(BOTH))
        );
        // もともと DirectShow のデバイスは自動と同じ
        assert_eq!(
            route_for(Some(DS_ONLY), ds),
            (VideoRoute::DirectShow, Some(DS_ONLY))
        );
    }

    #[test]
    fn route_for_media_foundation_strips_the_directshow_suffix() {
        let mf = VideoBackendSetting::MediaFoundation;
        // 印を外した本来の名前で Media Foundation の一覧を探す
        assert_eq!(
            route_for(Some(DS_ONLY), mf),
            (VideoRoute::MediaFoundation, Some("OBS Virtual Camera"))
        );
        assert_eq!(
            route_for(Some(BOTH), mf),
            (VideoRoute::MediaFoundation, Some(BOTH))
        );
    }

    #[test]
    fn route_for_without_a_device_is_media_foundation_for_every_setting() {
        // DirectShow の経路は名前が無いと開けないので、開き方によらず先頭へ
        for backend in VideoBackendSetting::ALL {
            assert_eq!(
                route_for(None, backend),
                (VideoRoute::MediaFoundation, None),
                "{backend:?}"
            );
        }
    }

    #[test]
    fn route_for_treats_a_bare_suffix_as_a_plain_name() {
        // 「(DirectShow)」だけの名前は印ではなく名前そのものとして扱う
        // （`directshow_friendly_name` が空の名前を返さない）
        let bare = " (DirectShow)";
        assert_eq!(
            route_for(Some(bare), VideoBackendSetting::Auto),
            (VideoRoute::MediaFoundation, Some(bare))
        );
        assert_eq!(
            route_for(Some(bare), VideoBackendSetting::MediaFoundation),
            (VideoRoute::MediaFoundation, Some(bare))
        );
    }

    // ---- 自動で Media Foundation が開けないときの DirectShow への倒し方（#387） ----

    use super::super::mock::MockVideoBackend;

    const GC551: &str = "AVerMedia GC551 Video Capture";

    /// Media Foundation では「見つかったが開けない」ときの失敗（実機の 0xC00D36B4）
    fn mf_open_failed() -> VideoError {
        VideoError::CameraOpenFailed {
            device: GC551.to_string(),
            source: "0xC00D36B4".to_string(),
        }
    }

    /// 経路ごとにモックを 1 つずつ置き、`SystemVideo::start_capture` と同じ形で
    /// `attempt_with_fallback` を通す
    fn start_on_mocks(
        media_foundation: &mut MockVideoBackend,
        direct_show: &mut MockVideoBackend,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
    ) -> (Result<(), VideoError>, VideoRoute) {
        attempt_with_fallback(device_name, backend, "接続", |route, name| {
            let request = CaptureRequest {
                device_name: name,
                resolution: None,
                format: None,
                fps: None,
                backend,
                connect_audio_pin: true,
            };
            match route {
                VideoRoute::DirectShow => direct_show.start_capture(&request),
                VideoRoute::MediaFoundation => media_foundation.start_capture(&request),
            }
        })
    }

    /// Media Foundation 側のモックを「見つかったが開けない」で 1 回失敗させる
    fn failing_media_foundation(error: VideoError) -> MockVideoBackend {
        let mock = MockVideoBackend::default();
        mock.with(|state| {
            state.failures_before_success = 1;
            state.failure = Some(error);
        });
        mock
    }

    #[test]
    fn auto_reopens_with_directshow_when_media_foundation_cannot_open() {
        let mut mf = failing_media_foundation(mf_open_failed());
        let mut ds = MockVideoBackend::default();
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        assert_eq!(result, Ok(()));
        // 実際に開けた経路を返す。`SystemVideo` はこれを `open` に入れる
        assert_eq!(route, VideoRoute::DirectShow);
        // 同じ呼び出しの中で 1 回ずつ。DirectShow には同じ名前をそのまま渡す
        assert_eq!(mf.with(|state| state.start_calls), 1);
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(
            ds.with(|state| state.last_device_name.clone()),
            Some(GC551.to_string())
        );
        assert!(ds.with(|state| state.capturing));
    }

    #[test]
    fn auto_returns_the_media_foundation_failure_when_directshow_lacks_the_device() {
        let mut mf = failing_media_foundation(mf_open_failed());
        // DirectShow の一覧にも無い（モックの既定の失敗は `DeviceNotFound`）
        let mut ds = MockVideoBackend::default();
        ds.with(|state| state.failures_before_success = 1);
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        assert_eq!(result, Err(mf_open_failed()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 1);
    }

    #[test]
    fn auto_returns_the_media_foundation_failure_when_directshow_also_fails_to_open() {
        let mut mf = failing_media_foundation(mf_open_failed());
        let mut ds = MockVideoBackend::default();
        ds.with(|state| {
            state.failures_before_success = 1;
            state.failure = Some(VideoError::StreamOpenFailed {
                device: GC551.to_string(),
                source: "VFW_E_NO_ACCEPTABLE_TYPES".to_string(),
            });
        });
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        // 画面に出すのは利用者が選んだ自動の本来の経路（Media Foundation）の理由
        assert_eq!(result, Err(mf_open_failed()));
        assert_eq!(route, VideoRoute::MediaFoundation);
    }

    #[test]
    fn media_foundation_setting_never_falls_back_to_directshow() {
        let mut mf = failing_media_foundation(mf_open_failed());
        let mut ds = MockVideoBackend::default();
        let (result, route) = start_on_mocks(
            &mut mf,
            &mut ds,
            Some(GC551),
            VideoBackendSetting::MediaFoundation,
        );
        assert_eq!(result, Err(mf_open_failed()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 0);
    }

    #[test]
    fn auto_does_not_try_directshow_when_media_foundation_cannot_find_the_device() {
        // 抜いている間の再試行。DirectShow の列挙を毎回足さない
        let not_found = VideoError::DeviceNotFound(GC551.to_string());
        let mut mf = failing_media_foundation(not_found.clone());
        let mut ds = MockVideoBackend::default();
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        assert_eq!(result, Err(not_found));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 0);
    }

    #[test]
    fn auto_does_not_try_directshow_when_media_foundation_opens() {
        let mut mf = MockVideoBackend::default();
        let mut ds = MockVideoBackend::default();
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 0);
    }

    #[test]
    fn directshow_fallback_only_for_auto_media_foundation_open_failures() {
        let auto = VideoBackendSetting::Auto;
        let mf = VideoRoute::MediaFoundation;
        let stream_failed = VideoError::StreamOpenFailed {
            device: GC551.to_string(),
            source: "E_FAIL".to_string(),
        };
        assert_eq!(
            directshow_fallback(mf, Some(GC551), auto, &mf_open_failed()),
            Some(GC551)
        );
        assert_eq!(
            directshow_fallback(mf, Some(GC551), auto, &stream_failed),
            Some(GC551)
        );
        // 最初から DirectShow の経路（「(DirectShow)」付き）なら、倒す先が無い
        assert_eq!(
            directshow_fallback(
                VideoRoute::DirectShow,
                Some(DS_ONLY),
                auto,
                &mf_open_failed()
            ),
            None
        );
        // 未指定（先頭のデバイス）は DirectShow では開けない
        assert_eq!(directshow_fallback(mf, None, auto, &mf_open_failed()), None);
        // 見つからない・列挙の失敗・1 台も無い、は倒さない
        for error in [
            VideoError::DeviceNotFound(GC551.to_string()),
            VideoError::DeviceQueryFailed("E_FAIL".to_string()),
            VideoError::NoDevices,
        ] {
            assert_eq!(
                directshow_fallback(mf, Some(GC551), auto, &error),
                None,
                "{error:?}"
            );
        }
        // 開き方を固定した設定では倒さない
        for backend in [
            VideoBackendSetting::MediaFoundation,
            VideoBackendSetting::DirectShow,
        ] {
            assert_eq!(
                directshow_fallback(mf, Some(GC551), backend, &mf_open_failed()),
                None,
                "{backend:?}"
            );
        }
    }

    #[test]
    fn capabilities_fall_back_to_directshow_like_opening() {
        // 対応形式の問い合わせも同じ規則で倒す（`SystemVideo::capabilities`）
        let mut asked = Vec::new();
        let (result, route) = attempt_with_fallback(
            Some(GC551),
            VideoBackendSetting::Auto,
            "対応形式の取得",
            |route, name| {
                asked.push((route, name.map(str::to_string)));
                match route {
                    VideoRoute::MediaFoundation => Err(mf_open_failed()),
                    VideoRoute::DirectShow => Ok(DeviceCapabilities::default()),
                }
            },
        );
        assert_eq!(result, Ok(DeviceCapabilities::default()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(
            asked,
            vec![
                (VideoRoute::MediaFoundation, Some(GC551.to_string())),
                (VideoRoute::DirectShow, Some(GC551.to_string())),
            ]
        );
    }
}
