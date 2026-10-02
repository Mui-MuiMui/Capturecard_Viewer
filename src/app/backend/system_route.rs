//! 本番のバックエンド（`system.rs`）が使う、映像の経路の判定（#414 で `system.rs` から分けた）。
//!
//! どちらの経路で開くか（`route_for`）、自動で Media Foundation が開けないときに
//! DirectShow でも試すか（`directshow_fallback` / `attempt_with_fallback`、#387）、
//! 一覧の突き合わせ（`merge_video_devices`）。どれもデバイスに触らず、単体テストできる。

use crate::settings::VideoBackendSetting;
use crate::video::{directshow_display_name, directshow_friendly_name, VideoError};
use log::{debug, info, warn};

/// どちらの経路でデバイスを扱うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoRoute {
    MediaFoundation,
    DirectShow,
}

/// デバイス名と設定の「映像の開き方」から、経路とその経路へ渡す名前を決める。
///
/// | 開き方 | 経路 | 渡す名前 |
/// |---|---|---|
/// | 自動（音声ピンを繋ぐ指定あり） | DirectShow | そのまま |
/// | 自動 | 「(DirectShow)」付きなら DirectShow、それ以外は Media Foundation | そのまま |
/// | Media Foundation | Media Foundation | 「(DirectShow)」を外した名前 |
/// | DirectShow | DirectShow | そのまま（`DirectShowCapture` が印の有無を問わず探す） |
///
/// **デバイスが未指定なら開き方によらず Media Foundation**（先頭のデバイス）。
/// DirectShow の経路は名前が無いと開けない。
///
/// `audio_pin` は音声ピンを繋ぐ指定（`CaptureRequest::connect_audio_pin`、
/// `[audio] input_source = "video_pin"` のときだけ真）。自動でこれが立っていれば
/// 最初から DirectShow で開く（#425）。利用者がこの映像デバイスの音声を使うと
/// 決めていて、音声ピンは DirectShow にしか無いため。DirectShow で開けなくても
/// Media Foundation へは倒さない（`directshow_fallback` は Media Foundation の
/// 経路の失敗だけを見る）。開き方を Media Foundation に固定した設定では従わない。
///
/// ここで決めるのは最初に試す経路だけ。自動で Media Foundation が「見つかったが
/// 開けない」ときは、`attempt_with_fallback` が DirectShow でも試す（#387）。
///
/// Media Foundation で「(DirectShow)」を外すのは、印は「DirectShow にしか
/// 無い」という一覧の上の目印で、デバイスの本来の名前ではないため。多くは
/// Media Foundation に居ないので「見つからない」になる（それが正しい結果）。
pub(super) fn route_for(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
) -> (VideoRoute, Option<&str>) {
    let Some(name) = device_name else {
        return (VideoRoute::MediaFoundation, None);
    };
    let friendly = directshow_friendly_name(name);
    match backend {
        VideoBackendSetting::Auto if audio_pin => (VideoRoute::DirectShow, Some(name)),
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
pub(super) fn directshow_fallback<'a>(
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
///
/// `audio_pin` は `route_for` へそのまま渡す。立っていれば自動でも最初から
/// DirectShow なので、ここでの倒し込みは起きない（#425）。
pub(super) fn attempt_with_fallback<T>(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
    what: &str,
    mut attempt: impl FnMut(VideoRoute, Option<&str>) -> Result<T, VideoError>,
) -> (Result<T, VideoError>, VideoRoute) {
    let (route, name) = route_for(device_name, backend, audio_pin);
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
pub(super) fn merge_video_devices(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::backend::{CaptureRequest, VideoBackend};
    use crate::video::DeviceCapabilities;

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

    const DS_ONLY: &str = "OBS Virtual Camera (DirectShow)";
    const BOTH: &str = "USB Video";

    #[test]
    fn route_for_auto_uses_the_directshow_suffix() {
        let auto = VideoBackendSetting::Auto;
        assert_eq!(
            route_for(Some(DS_ONLY), auto, false),
            (VideoRoute::DirectShow, Some(DS_ONLY))
        );
        assert_eq!(
            route_for(Some(BOTH), auto, false),
            (VideoRoute::MediaFoundation, Some(BOTH))
        );
        // 未指定は今までどおり Media Foundation の先頭
        assert_eq!(
            route_for(None, auto, false),
            (VideoRoute::MediaFoundation, None)
        );
    }

    #[test]
    fn route_for_direct_show_opens_even_a_media_foundation_name_with_directshow() {
        let ds = VideoBackendSetting::DirectShow;
        // 両方に出るデバイス。同じ表示名を DirectShow の一覧から探す
        assert_eq!(
            route_for(Some(BOTH), ds, false),
            (VideoRoute::DirectShow, Some(BOTH))
        );
        // もともと DirectShow のデバイスは自動と同じ
        assert_eq!(
            route_for(Some(DS_ONLY), ds, false),
            (VideoRoute::DirectShow, Some(DS_ONLY))
        );
    }

    #[test]
    fn route_for_media_foundation_strips_the_directshow_suffix() {
        let mf = VideoBackendSetting::MediaFoundation;
        // 印を外した本来の名前で Media Foundation の一覧を探す
        assert_eq!(
            route_for(Some(DS_ONLY), mf, false),
            (VideoRoute::MediaFoundation, Some("OBS Virtual Camera"))
        );
        assert_eq!(
            route_for(Some(BOTH), mf, false),
            (VideoRoute::MediaFoundation, Some(BOTH))
        );
    }

    #[test]
    fn route_for_without_a_device_is_media_foundation_for_every_setting() {
        // DirectShow の経路は名前が無いと開けないので、開き方によらず先頭へ
        for backend in VideoBackendSetting::ALL {
            assert_eq!(
                route_for(None, backend, false),
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
            route_for(Some(bare), VideoBackendSetting::Auto, false),
            (VideoRoute::MediaFoundation, Some(bare))
        );
        assert_eq!(
            route_for(Some(bare), VideoBackendSetting::MediaFoundation, false),
            (VideoRoute::MediaFoundation, Some(bare))
        );
    }

    #[test]
    fn route_for_auto_with_an_audio_pin_prefers_directshow() {
        // 「映像デバイスの音声」を選んだ自動は、両方に出るデバイスも DirectShow（#425）
        let auto = VideoBackendSetting::Auto;
        assert_eq!(
            route_for(Some(BOTH), auto, true),
            (VideoRoute::DirectShow, Some(BOTH))
        );
        assert_eq!(
            route_for(Some(DS_ONLY), auto, true),
            (VideoRoute::DirectShow, Some(DS_ONLY))
        );
        // 未指定は DirectShow では開けないので今までどおり
        assert_eq!(
            route_for(None, auto, true),
            (VideoRoute::MediaFoundation, None)
        );
        // 開き方を固定した設定には従う。Media Foundation 固定なら倒さず理由を出す
        assert_eq!(
            route_for(Some(DS_ONLY), VideoBackendSetting::MediaFoundation, true),
            (VideoRoute::MediaFoundation, Some("OBS Virtual Camera"))
        );
        assert_eq!(
            route_for(Some(BOTH), VideoBackendSetting::DirectShow, true),
            (VideoRoute::DirectShow, Some(BOTH))
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
    /// `attempt_with_fallback` を通す。音声ピンは繋がない
    fn start_on_mocks(
        media_foundation: &mut MockVideoBackend,
        direct_show: &mut MockVideoBackend,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
    ) -> (Result<(), VideoError>, VideoRoute) {
        start_on_mocks_with_pin(media_foundation, direct_show, device_name, backend, false)
    }

    /// `start_on_mocks` に音声ピンを繋ぐ指定（`connect_audio_pin`）を足したもの
    fn start_on_mocks_with_pin(
        media_foundation: &mut MockVideoBackend,
        direct_show: &mut MockVideoBackend,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
        audio_pin: bool,
    ) -> (Result<(), VideoError>, VideoRoute) {
        attempt_with_fallback(device_name, backend, audio_pin, "接続", |route, name| {
            let request = CaptureRequest {
                device_name: name,
                resolution: None,
                format: None,
                fps: None,
                backend,
                connect_audio_pin: audio_pin,
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
    fn auto_with_an_audio_pin_opens_directshow_without_trying_media_foundation() {
        // 「映像デバイスの音声」を選んで適用した自動（#425）。Media Foundation には
        // 音声ピンが無いので試さない
        let mut mf = MockVideoBackend::default();
        let mut ds = MockVideoBackend::default();
        let (result, route) = start_on_mocks_with_pin(
            &mut mf,
            &mut ds,
            Some(GC551),
            VideoBackendSetting::Auto,
            true,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(ds.with(|state| state.last_connect_audio_pin), Some(true));
    }

    #[test]
    fn auto_with_an_audio_pin_does_not_fall_back_to_media_foundation() {
        // DirectShow で開けなくても Media Foundation へは倒さず、DirectShow の失敗を返す
        let open_failed = VideoError::StreamOpenFailed {
            device: GC551.to_string(),
            source: "VFW_E_NO_ACCEPTABLE_TYPES".to_string(),
        };
        for error in [open_failed, VideoError::DeviceNotFound(GC551.to_string())] {
            let mut mf = MockVideoBackend::default();
            let mut ds = MockVideoBackend::default();
            ds.with(|state| {
                state.failures_before_success = 1;
                state.failure = Some(error.clone());
            });
            let (result, route) = start_on_mocks_with_pin(
                &mut mf,
                &mut ds,
                Some(GC551),
                VideoBackendSetting::Auto,
                true,
            );
            assert_eq!(result, Err(error.clone()));
            assert_eq!(route, VideoRoute::DirectShow);
            assert_eq!(mf.with(|state| state.start_calls), 0, "{error:?}");
        }
    }

    #[test]
    fn media_foundation_setting_with_an_audio_pin_stays_on_media_foundation() {
        // 開き方を Media Foundation に固定していれば、音声ピンの指定があっても従う。
        // 音声は「開かずに待つ」の経路で理由を出す（monitor_audio_pin）
        let mut mf = MockVideoBackend::default();
        let mut ds = MockVideoBackend::default();
        let (result, route) = start_on_mocks_with_pin(
            &mut mf,
            &mut ds,
            Some(GC551),
            VideoBackendSetting::MediaFoundation,
            true,
        );
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
            false,
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
