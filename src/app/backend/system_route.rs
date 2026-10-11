//! 本番のバックエンド（`system.rs`）が使う、映像の経路の判定（#414 で `system.rs` から分けた）。
//!
//! どちらの経路で開くか（`route_for`）、自動で DirectShow が開けないときに
//! Media Foundation で開き直すか（`media_foundation_fallback` / `attempt_with_fallback`、
//! #425、#490）、一覧の突き合わせ（`merge_video_devices`）。どれもデバイスに触らず、
//! 単体テストできる。

use crate::settings::VideoBackendSetting;
use crate::video::{directshow_display_name, directshow_friendly_name, VideoError};
use log::{debug, info, warn};

/// どちらの経路でデバイスを扱うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoRoute {
    MediaFoundation,
    DirectShow,
}

/// デバイス名と設定の「映像の開き方」から、最初に試す経路とその経路へ渡す名前を決める。
///
/// | 開き方 | 経路 | 渡す名前 |
/// |---|---|---|
/// | 自動 | DirectShow | そのまま（`DirectShowCapture` が印の有無を問わず探す） |
/// | Media Foundation | Media Foundation | 「(DirectShow)」を外した名前 |
/// | DirectShow | DirectShow | そのまま |
///
/// **デバイスが未指定なら開き方によらず Media Foundation**（先頭のデバイス）。
/// DirectShow の経路は名前が無いと開けない。
///
/// **自動では、名前の付いたデバイスをまず DirectShow で開く**（#490）。同じボード
/// （Live Gamer EXTREME 3）を DirectShow で開くと、表示までの遅れが Media Foundation より
/// 約 6ms 短かった（`docs/LATENCY.md`）。「(DirectShow)」の付かない名前は Media Foundation の
/// 一覧に居るもの（`merge_video_devices`）で、DirectShow の一覧にも居るか（両方に居るか）は
/// `DirectShowCapture` が開くときの列挙で分かる。居なければ `DeviceNotFound` が返り、
/// `attempt_with_fallback` が今までどおり Media Foundation で開く。DirectShow で
/// 「見つかったが開けない」ときも Media Foundation で開き直す（`media_foundation_fallback`）。
/// 「(DirectShow)」付きの名前は DirectShow にしか居ないので、今までどおり DirectShow だけ。
///
/// 開く前に一覧を取り直して両方に居るかを確かめないのは、起動直後の最初の接続が
/// 列挙より先に走り（列挙で最初の接続を遅らせない）、再接続の前にデバイスを列挙しない
/// 決まり（`docs/design/reconnect.md`）があるため。DirectShow で探すのは
/// `DirectShowCapture::start_capture` の中の列挙で、Media Foundation の `start_capture` の
/// 中の列挙と同じ扱い。
///
/// Media Foundation で「(DirectShow)」を外すのは、印は「DirectShow にしか
/// 無い」という一覧の上の目印で、デバイスの本来の名前ではないため。多くは
/// Media Foundation に居ないので「見つからない」になる（それが正しい結果）。
pub(super) fn route_for(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
) -> (VideoRoute, Option<&str>) {
    let Some(name) = device_name else {
        return (VideoRoute::MediaFoundation, None);
    };
    match backend {
        VideoBackendSetting::Auto | VideoBackendSetting::DirectShow => {
            (VideoRoute::DirectShow, Some(name))
        }
        VideoBackendSetting::MediaFoundation => (
            VideoRoute::MediaFoundation,
            Some(directshow_friendly_name(name).unwrap_or(name)),
        ),
    }
}

/// 「自動」で DirectShow が失敗したとき、Media Foundation で開き直す名前（#425、#490）。
///
/// 返すのは、開き方が自動で、名前が「(DirectShow)」付きでない（付いていれば
/// Media Foundation の一覧に居ない）ときだけ。名前はそのまま渡す。
///
/// | DirectShow の失敗 | 音声ピンの指定なし | 音声ピンの指定あり |
/// |---|---|---|
/// | `DeviceNotFound`（DirectShow の一覧に居ない） | 開き直す | 開き直す |
/// | 見つかったが開けない・列挙に失敗した | 開き直す | 開き直さない |
///
/// DirectShow の一覧に居ないのは、Media Foundation にだけ居るデバイス（か、抜いている間）。
/// 今までどおり Media Foundation で開く。見つかったが開けないときは、両方に居る
/// デバイスを今までの経路で開き直す。列挙に失敗したときは両方に居るかが分からないので、
/// やはり今までの経路で開く。
///
/// **音声ピンを繋ぐ指定（`[audio] input_source = "video_pin"`）があって DirectShow の
/// 一覧にあるなら、開けなくても開き直さない**（#425）。Media Foundation には音声ピンが無く、
/// 開き直すと映像は出ても音声は「Media Foundation で開いた」の理由で待ち続ける。
/// DirectShow の失敗を出したほうが利用者に原因が分かる。
fn media_foundation_fallback<'a>(
    name: Option<&'a str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
    error: &VideoError,
) -> Option<&'a str> {
    let name = name?;
    if backend != VideoBackendSetting::Auto || directshow_friendly_name(name).is_some() {
        return None;
    }
    let not_listed = matches!(error, VideoError::DeviceNotFound(_));
    (not_listed || !audio_pin).then_some(name)
}

/// 経路を決めて `attempt` を呼び、「自動」で DirectShow が開けなければ
/// **同じ呼び出しの中で** Media Foundation でも試す（#425、#490）。返すのは結果と、
/// その結果を出した経路。`what` はログに出す操作の名前（「接続」など、
/// 「〜に失敗した」「〜に成功した」と続く名詞）。
///
/// 再試行（`ConnectRetry`）から見ると、DirectShow → Media Foundation は
/// 試行 1 回のまま。Media Foundation で開き直したら、**その結果をそのまま返す。**
/// 両方に居るデバイスにとって Media Foundation は今までの経路なので、両方で開けなければ
/// 画面に出す理由は今までと同じ Media Foundation のものにする（DirectShow の失敗は
/// ログに残す）。Media Foundation にだけ居るデバイスでは、それがもともとの結果。
///
/// `audio_pin` は音声ピンを繋ぐ指定（`CaptureRequest::connect_audio_pin`）。立っていれば
/// DirectShow の一覧にあって開けなかったときに開き直さない（`media_foundation_fallback`）。
/// 開き方を固定した設定（Media Foundation / DirectShow）では倒さず、利用者が選んだ
/// 経路の結果をそのまま返す。
pub(super) fn attempt_with_fallback<T>(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
    what: &str,
    mut attempt: impl FnMut(VideoRoute, Option<&str>) -> Result<T, VideoError>,
) -> (Result<T, VideoError>, VideoRoute) {
    let (route, name) = route_for(device_name, backend);
    let first = attempt(route, name);
    let first_error = match &first {
        Ok(_) => {
            if backend == VideoBackendSetting::Auto && route == VideoRoute::DirectShow {
                info!(
                    "開き方が自動なので DirectShow で試し、{}に成功した: {}",
                    what,
                    name.unwrap_or_default()
                );
            }
            return (first, route);
        }
        Err(e) => e,
    };
    let Some(mf_name) = media_foundation_fallback(name, backend, audio_pin, first_error) else {
        return (first, route);
    };
    let not_listed = matches!(first_error, VideoError::DeviceNotFound(_));
    if not_listed && audio_pin {
        info!(
            "映像デバイスの音声のために DirectShow で{}しようとしたが、DirectShow の一覧に無いので Media Foundation で試す: {}",
            what, mf_name
        );
    } else if not_listed {
        // Media Foundation にだけ居るデバイスか、抜いている間。再試行のたびに出るので debug
        debug!(
            "DirectShow の一覧に無いので Media Foundation で{}を試す: {}",
            what, mf_name
        );
    } else {
        info!(
            "開き方が自動で、DirectShow では{}に失敗したので Media Foundation で試す: {}（DirectShow の失敗: {}）",
            what, mf_name, first_error
        );
    }
    let second = attempt(VideoRoute::MediaFoundation, Some(mf_name));
    if !not_listed {
        match &second {
            Ok(_) => info!(
                "DirectShow では{}に失敗したので Media Foundation で試し、成功した: {}",
                what, mf_name
            ),
            Err(e) => warn!(
                "DirectShow でも Media Foundation でも{}に失敗した: {}: {}",
                what, mf_name, e
            ),
        }
    }
    (second, VideoRoute::MediaFoundation)
}

/// Media Foundation の一覧に、DirectShow にしか無いデバイスを足す。
///
/// 照合は表示名で行う。**同じ名前が両方にあれば Media Foundation のほうだけを
/// 残す**（印の無い名前で残る。自動の開き方ではまず DirectShow で開き、開けなければ
/// Media Foundation で開く。`route_for` / `attempt_with_fallback`、#490）。一覧の名前を
/// 開き方で変えないのは、設定に残る名前が変わると別のデバイス扱いになるため。
/// DirectShow 側で同じ名前が重なっていれば 1 つにする（同じ名前では
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
    fn route_for_auto_opens_named_devices_with_directshow_first() {
        // 両方に居るデバイスも、まず DirectShow（#490）。DirectShow にしか居ないものは今までどおり
        let auto = VideoBackendSetting::Auto;
        assert_eq!(
            route_for(Some(BOTH), auto),
            (VideoRoute::DirectShow, Some(BOTH))
        );
        assert_eq!(
            route_for(Some(DS_ONLY), auto),
            (VideoRoute::DirectShow, Some(DS_ONLY))
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
        let auto = VideoBackendSetting::Auto;
        assert_eq!(
            route_for(Some(bare), auto),
            (VideoRoute::DirectShow, Some(bare))
        );
        assert_eq!(
            route_for(Some(bare), VideoBackendSetting::MediaFoundation),
            (VideoRoute::MediaFoundation, Some(bare))
        );
        // 印の無い名前なので、DirectShow で開けなければ Media Foundation で開き直す
        let not_found = VideoError::DeviceNotFound(bare.to_string());
        assert_eq!(
            media_foundation_fallback(Some(bare), auto, false, &not_found),
            Some(bare)
        );
    }

    // ---- 自動で DirectShow を先に試し、開けなければ Media Foundation で開く（#490） ----

    use super::super::mock::MockVideoBackend;

    const GC551: &str = "AVerMedia GC551 Video Capture";

    /// Media Foundation では「見つかったが開けない」ときの失敗（GC551 の実機の 0xC00D36B4）
    fn mf_open_failed() -> VideoError {
        VideoError::CameraOpenFailed {
            device: GC551.to_string(),
            source: "0xC00D36B4".to_string(),
        }
    }

    /// DirectShow では「見つかったが開けない」ときの失敗
    fn ds_open_failed() -> VideoError {
        VideoError::StreamOpenFailed {
            device: BOTH.to_string(),
            source: "VFW_E_NO_ACCEPTABLE_TYPES".to_string(),
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

    /// 1 回だけ `error` で失敗するモック
    fn failing_once(error: VideoError) -> MockVideoBackend {
        let mock = MockVideoBackend::default();
        mock.with(|state| {
            state.failures_before_success = 1;
            state.failure = Some(error);
        });
        mock
    }

    #[test]
    fn auto_opens_a_device_on_both_lists_with_directshow_without_trying_media_foundation() {
        let mut mf = MockVideoBackend::default();
        let mut ds = MockVideoBackend::default();
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(BOTH), VideoBackendSetting::Auto);
        assert_eq!(result, Ok(()));
        // 実際に開けた経路を返す。`SystemVideo` はこれを `open` に入れる
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(
            ds.with(|state| state.last_device_name.clone()),
            Some(BOTH.to_string())
        );
        assert!(ds.with(|state| state.capturing));
    }

    #[test]
    fn auto_opens_media_foundation_when_directshow_lacks_the_device() {
        // Media Foundation にだけ居るデバイス。今までどおり Media Foundation で開く
        let mut mf = MockVideoBackend::default();
        let mut ds = failing_once(VideoError::DeviceNotFound(BOTH.to_string()));
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(BOTH), VideoBackendSetting::Auto);
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        // 同じ呼び出しの中で 1 回ずつ。Media Foundation には同じ名前をそのまま渡す
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(mf.with(|state| state.start_calls), 1);
        assert_eq!(
            mf.with(|state| state.last_device_name.clone()),
            Some(BOTH.to_string())
        );
        assert!(mf.with(|state| state.capturing));
    }

    #[test]
    fn auto_reopens_with_media_foundation_when_directshow_cannot_open() {
        // 両方に居るが DirectShow では開けない。今までの経路（Media Foundation）で開き直す
        for error in [
            ds_open_failed(),
            VideoError::CameraOpenFailed {
                device: BOTH.to_string(),
                source: "E_FAIL".to_string(),
            },
            // 列挙に失敗したら両方に居るかが分からないので、今までの経路で開く
            VideoError::DeviceQueryFailed("E_FAIL".to_string()),
        ] {
            let mut mf = MockVideoBackend::default();
            let mut ds = failing_once(error.clone());
            let (result, route) =
                start_on_mocks(&mut mf, &mut ds, Some(BOTH), VideoBackendSetting::Auto);
            assert_eq!(result, Ok(()), "{error:?}");
            assert_eq!(route, VideoRoute::MediaFoundation, "{error:?}");
            assert_eq!(mf.with(|state| state.start_calls), 1, "{error:?}");
        }
    }

    #[test]
    fn auto_returns_the_media_foundation_failure_when_both_routes_fail() {
        // 両方で開けなければ、画面に出す理由は今までと同じ Media Foundation のもの
        let mut mf = failing_once(mf_open_failed());
        let mut ds = failing_once(ds_open_failed());
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(BOTH), VideoBackendSetting::Auto);
        assert_eq!(result, Err(mf_open_failed()));
        assert_eq!(route, VideoRoute::MediaFoundation);
    }

    #[test]
    fn auto_returns_not_found_while_the_device_is_unplugged() {
        // 抜いている間の再試行。どちらにも居ないので Media Foundation の「見つからない」を返す
        let not_found = VideoError::DeviceNotFound(BOTH.to_string());
        let mut mf = failing_once(not_found.clone());
        let mut ds = failing_once(not_found.clone());
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(BOTH), VideoBackendSetting::Auto);
        assert_eq!(result, Err(not_found));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(mf.with(|state| state.start_calls), 1);
    }

    #[test]
    fn auto_does_not_try_media_foundation_for_a_directshow_only_device() {
        // 「(DirectShow)」付きは Media Foundation の一覧に居ないので、開き直す先が無い
        let not_found = VideoError::DeviceNotFound(DS_ONLY.to_string());
        let mut mf = MockVideoBackend::default();
        let mut ds = failing_once(not_found.clone());
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(DS_ONLY), VideoBackendSetting::Auto);
        assert_eq!(result, Err(not_found));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
    }

    #[test]
    fn auto_opens_gc551_with_directshow_without_failing_on_media_foundation_first() {
        // Media Foundation では開けないボード（#387）。今は最初から DirectShow で開くので、
        // Media Foundation の失敗を待たない
        let mut mf = failing_once(mf_open_failed());
        let mut ds = MockVideoBackend::default();
        let (result, route) =
            start_on_mocks(&mut mf, &mut ds, Some(GC551), VideoBackendSetting::Auto);
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
    }

    #[test]
    fn media_foundation_setting_never_tries_directshow() {
        // 開き方を Media Foundation に固定した設定は今までどおり（逃げ道）
        let mut mf = failing_once(mf_open_failed());
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

        let mut mf = MockVideoBackend::default();
        let (result, route) = start_on_mocks(
            &mut mf,
            &mut ds,
            Some(BOTH),
            VideoBackendSetting::MediaFoundation,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 0);
    }

    #[test]
    fn direct_show_setting_never_falls_back_to_media_foundation() {
        let mut mf = MockVideoBackend::default();
        let mut ds = failing_once(ds_open_failed());
        let (result, route) = start_on_mocks(
            &mut mf,
            &mut ds,
            Some(BOTH),
            VideoBackendSetting::DirectShow,
        );
        assert_eq!(result, Err(ds_open_failed()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
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
        // DirectShow の一覧にあって開けなかったときは Media Foundation へは倒さず、
        // DirectShow の失敗を返す
        let mut mf = MockVideoBackend::default();
        let mut ds = failing_once(ds_open_failed());
        let (result, route) = start_on_mocks_with_pin(
            &mut mf,
            &mut ds,
            Some(GC551),
            VideoBackendSetting::Auto,
            true,
        );
        assert_eq!(result, Err(ds_open_failed()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(mf.with(|state| state.start_calls), 0);
    }

    #[test]
    fn auto_with_an_audio_pin_opens_media_foundation_when_directshow_lacks_the_device() {
        // DirectShow の一覧に無い（Media Foundation にだけ居る機種）。映像まで開けなく
        // ならないよう、今までどおり Media Foundation で開く。音声は理由を出して待つ
        let mut mf = MockVideoBackend::default();
        let mut ds = failing_once(VideoError::DeviceNotFound(GC551.to_string()));
        let (result, route) = start_on_mocks_with_pin(
            &mut mf,
            &mut ds,
            Some(GC551),
            VideoBackendSetting::Auto,
            true,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(ds.with(|state| state.start_calls), 1);
        assert_eq!(mf.with(|state| state.start_calls), 1);
        assert_eq!(
            mf.with(|state| state.last_device_name.clone()),
            Some(GC551.to_string())
        );
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
    fn media_foundation_fallback_only_for_auto_names_without_the_directshow_suffix() {
        let auto = VideoBackendSetting::Auto;
        let not_found = VideoError::DeviceNotFound(BOTH.to_string());
        let query_failed = VideoError::DeviceQueryFailed("E_FAIL".to_string());
        // 音声ピンの指定なし: 理由を問わず開き直す
        for error in [not_found.clone(), ds_open_failed(), query_failed.clone()] {
            assert_eq!(
                media_foundation_fallback(Some(BOTH), auto, false, &error),
                Some(BOTH),
                "{error:?}"
            );
        }
        // 音声ピンの指定あり: DirectShow の一覧に無いときだけ
        assert_eq!(
            media_foundation_fallback(Some(BOTH), auto, true, &not_found),
            Some(BOTH)
        );
        for error in [ds_open_failed(), query_failed] {
            assert_eq!(
                media_foundation_fallback(Some(BOTH), auto, true, &error),
                None,
                "{error:?}"
            );
        }
        // 「(DirectShow)」付きの名前は Media Foundation の一覧に居ない
        for audio_pin in [false, true] {
            assert_eq!(
                media_foundation_fallback(Some(DS_ONLY), auto, audio_pin, &not_found),
                None
            );
        }
        // 未指定（先頭のデバイス）はもともと Media Foundation の経路
        assert_eq!(
            media_foundation_fallback(None, auto, false, &not_found),
            None
        );
        // 開き方を固定した設定では倒さない
        for backend in [
            VideoBackendSetting::MediaFoundation,
            VideoBackendSetting::DirectShow,
        ] {
            assert_eq!(
                media_foundation_fallback(Some(BOTH), backend, false, &ds_open_failed()),
                None,
                "{backend:?}"
            );
        }
    }

    #[test]
    fn capabilities_follow_the_same_order_as_opening() {
        // 対応形式の問い合わせも同じ規則（`SystemVideo::capabilities`）。両方に居れば
        // DirectShow 側の対応形式を返し、能力キャッシュの (名前, 自動) にはそれが入る
        let mut asked = Vec::new();
        let (result, route) = attempt_with_fallback(
            Some(BOTH),
            VideoBackendSetting::Auto,
            false,
            "対応形式の取得",
            |route, name| {
                asked.push((route, name.map(str::to_string)));
                Ok(DeviceCapabilities::default())
            },
        );
        assert_eq!(result, Ok(DeviceCapabilities::default()));
        assert_eq!(route, VideoRoute::DirectShow);
        assert_eq!(
            asked,
            vec![(VideoRoute::DirectShow, Some(BOTH.to_string()))]
        );

        // DirectShow の一覧に無ければ Media Foundation 側の対応形式
        let mut asked = Vec::new();
        let (result, route) = attempt_with_fallback(
            Some(BOTH),
            VideoBackendSetting::Auto,
            false,
            "対応形式の取得",
            |route, name| {
                asked.push((route, name.map(str::to_string)));
                match route {
                    VideoRoute::DirectShow => Err(VideoError::DeviceNotFound(BOTH.to_string())),
                    VideoRoute::MediaFoundation => Ok(DeviceCapabilities::default()),
                }
            },
        );
        assert_eq!(result, Ok(DeviceCapabilities::default()));
        assert_eq!(route, VideoRoute::MediaFoundation);
        assert_eq!(
            asked,
            vec![
                (VideoRoute::DirectShow, Some(BOTH.to_string())),
                (VideoRoute::MediaFoundation, Some(BOTH.to_string())),
            ]
        );
    }
}
