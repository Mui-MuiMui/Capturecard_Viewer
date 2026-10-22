//! 本番のバックエンド（`system.rs`）が使う、映像の経路の判定（#414 で `system.rs` から分けた）。
//!
//! どちらの経路で開くか（`route_for`）、自動で DirectShow が開けないときに
//! Media Foundation で開き直すか（`media_foundation_fallback` / `attempt_with_fallback`、
//! #425、#490）。一覧の突き合わせは `system_devices.rs`。どれもデバイスに触らず、
//! 単体テストできる。

use crate::settings::VideoBackendSetting;
use crate::video::{directshow_friendly_name, DeviceCapabilities, VideoError};
use log::{debug, info, log, warn, Level};

/// 経路をまたいで試す操作。ログの文言と、成功を残すレベルを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    /// ストリームを開く
    Connect,
    /// 対応形式の問い合わせ
    Capabilities,
}

impl Operation {
    /// ログに出す名前。「〜に失敗した」「〜に成功した」と続く名詞
    fn label(self) -> &'static str {
        match self {
            Operation::Connect => "接続",
            Operation::Capabilities => "対応形式の取得",
        }
    }

    /// 成功のログのレベル。開くのは設定の変更か切断のときだけなので INFO。
    /// 能力の問い合わせは設定ダイアログを開くたびに走るので DEBUG
    fn success_level(self) -> Level {
        match self {
            Operation::Connect => Level::Info,
            Operation::Capabilities => Level::Debug,
        }
    }
}

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
/// **自動では、名前の付いたデバイスをまず DirectShow で開く**（#490。表示までの遅れが
/// 短い）。両方の一覧に居るかは `DirectShowCapture` が開くときの列挙で分かり、居なければ
/// `DeviceNotFound` が返って `attempt_with_fallback` が Media Foundation で開く。開く前に
/// 一覧を取り直さない理由と条件の表は `docs/design/device-worker.md` の「自動で
/// DirectShow を先に試す理由（#490）」。
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
/// 一覧に居ないのは Media Foundation にだけ居るデバイス（か、抜いている間）。列挙に
/// 失敗したら両方に居るかが分からないので、今までの経路で開く。**音声ピンを繋ぐ指定が
/// あれば、DirectShow の一覧にあって開けなくても開き直さない**（#425。Media Foundation には
/// 音声ピンが無く、DirectShow の失敗を出したほうが原因が分かる）。
fn media_foundation_fallback<'a>(
    name: Option<&'a str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
    error: &VideoError,
) -> Option<&'a str> {
    let name = media_foundation_candidate(name, backend)?;
    let not_listed = matches!(error, VideoError::DeviceNotFound(_));
    (not_listed || !audio_pin).then_some(name)
}

/// 自動で DirectShow から Media Foundation へ倒してよい名前。開き方が自動で、名前が
/// 「(DirectShow)」付きでない（付いていれば Media Foundation の一覧に居ない）ときだけ。
fn media_foundation_candidate(name: Option<&str>, backend: VideoBackendSetting) -> Option<&str> {
    let name = name?;
    (backend == VideoBackendSetting::Auto && directshow_friendly_name(name).is_none())
        .then_some(name)
}

/// 対応形式の問い合わせ。`attempt_with_fallback` と同じ順で引き、さらに自動で
/// DirectShow が**空の一覧**を返したら Media Foundation でも引く（#490）。
///
/// `DirectShowCapture::capabilities` は `IAMStreamConfig` が無いか受け取れる形式が 0 件だと
/// `Ok(空)` を返す。開くときはグラフの組み立てが失敗して Media Foundation で開くので、
/// 選択肢も Media Foundation 側にしないと「選択肢は空なのに Media Foundation で映る」と
/// 食い違う。Media Foundation でも引けなければ、開くときと同じく Media Foundation の
/// 結果を返す。「(DirectShow)」付きの名前と開き方を固定した設定では空のまま返す。
pub(super) fn capabilities_with_fallback(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    mut query: impl FnMut(VideoRoute, Option<&str>) -> Result<DeviceCapabilities, VideoError>,
) -> Result<DeviceCapabilities, VideoError> {
    let (result, route) = attempt_with_fallback(
        device_name,
        backend,
        false,
        Operation::Capabilities,
        &mut query,
    );
    let empty = route == VideoRoute::DirectShow && result.as_ref().is_ok_and(Vec::is_empty);
    let Some(mf_name) = media_foundation_candidate(device_name, backend).filter(|_| empty) else {
        return result;
    };
    // 直前の attempt_with_fallback が「DirectShow で…対応形式の取得に成功した」と出して
    // いるので、成功は一覧が空だったことと、どちらの結果を使うかを続けて残す
    debug!(
        "DirectShow での対応形式の取得は成功したが一覧が空だったので、Media Foundation で引き直す: {}",
        mf_name
    );
    let second = query(VideoRoute::MediaFoundation, Some(mf_name));
    match &second {
        Ok(capabilities) => debug!(
            "Media Foundation で引き直した対応形式を使う（{} 件）: {}",
            capabilities.len(),
            mf_name
        ),
        Err(e) => debug!(
            "Media Foundation でも対応形式を引けなかったので、その失敗を返す: {}: {}",
            mf_name, e
        ),
    }
    second
}

/// 経路を決めて `attempt` を呼び、「自動」で DirectShow が開けなければ
/// **同じ呼び出しの中で** Media Foundation でも試す（#425、#490）。返すのは結果と、
/// その結果を出した経路。`operation` はログの文言とレベルを決める。
///
/// 再試行（`ConnectRetry`）から見ると、DirectShow → Media Foundation は
/// 試行 1 回のまま。Media Foundation で開き直したら、**その結果をそのまま返す。**
/// 両方に居るデバイスにとって Media Foundation は今までの経路なので、両方で開けなければ
/// 画面に出す理由は今までと同じ Media Foundation のものにする（DirectShow の失敗は
/// ログに残す）。Media Foundation にだけ居るデバイスでは、それがもともとの結果。
///
/// `audio_pin`（`CaptureRequest::connect_audio_pin`）の扱いは `media_foundation_fallback`。
/// 開き方を固定した設定では倒さず、利用者が選んだ経路の結果をそのまま返す。
pub(super) fn attempt_with_fallback<T>(
    device_name: Option<&str>,
    backend: VideoBackendSetting,
    audio_pin: bool,
    operation: Operation,
    mut attempt: impl FnMut(VideoRoute, Option<&str>) -> Result<T, VideoError>,
) -> (Result<T, VideoError>, VideoRoute) {
    let what = operation.label();
    let (route, name) = route_for(device_name, backend);
    let first = attempt(route, name);
    let first_error = match &first {
        Ok(_) => {
            if let (VideoBackendSetting::Auto, VideoRoute::DirectShow, Some(name)) =
                (backend, route, name)
            {
                let reason = if directshow_friendly_name(name).is_some() {
                    "名前に「(DirectShow)」が付いている（DirectShow にしか居ない）ので DirectShow で"
                } else {
                    "開き方が自動なので DirectShow で試し、"
                };
                log!(
                    operation.success_level(),
                    "{}{}に成功した: {}",
                    reason,
                    what,
                    name
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::backend::{CaptureRequest, VideoBackend};

    const DS_ONLY: &str = "OBS Virtual Camera (DirectShow)";
    const BOTH: &str = "USB Video";

    #[test]
    fn route_for_follows_the_backend_setting() {
        use VideoBackendSetting::{Auto, DirectShow, MediaFoundation};
        use VideoRoute as R;
        let cases = [
            // 自動: 両方に居るデバイスも、まず DirectShow（#490）。DirectShow にしか居ないものは今までどおり
            (BOTH, Auto, (R::DirectShow, BOTH)),
            (DS_ONLY, Auto, (R::DirectShow, DS_ONLY)),
            // DirectShow 固定: 同じ表示名を DirectShow の一覧から探す
            (BOTH, DirectShow, (R::DirectShow, BOTH)),
            (DS_ONLY, DirectShow, (R::DirectShow, DS_ONLY)),
            // Media Foundation 固定: 印を外した本来の名前で Media Foundation の一覧を探す
            (BOTH, MediaFoundation, (R::MediaFoundation, BOTH)),
            (
                DS_ONLY,
                MediaFoundation,
                (R::MediaFoundation, "OBS Virtual Camera"),
            ),
        ];
        for (name, backend, (route, passed)) in cases {
            assert_eq!(
                route_for(Some(name), backend),
                (route, Some(passed)),
                "{name} {backend:?}"
            );
        }
        // 未指定は開き方によらず Media Foundation の先頭（DirectShow は名前が無いと開けない）
        for backend in VideoBackendSetting::ALL {
            assert_eq!(
                route_for(None, backend),
                (R::MediaFoundation, None),
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
        attempt_with_fallback(
            device_name,
            backend,
            audio_pin,
            Operation::Connect,
            |route, name| {
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
            },
        )
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

    /// 経路の名前を形式名にした 1 件だけの対応形式（どちらの経路から来たかを見分ける）
    fn caps_of(route: VideoRoute) -> DeviceCapabilities {
        vec![crate::video::capabilities::FormatCapability {
            name: format!("{route:?}"),
            modes: Vec::new(),
            current_resolution: None,
            assumed: false,
        }]
    }

    /// `SystemVideo::capabilities` と同じ形で引き、問い合わせた経路を記録する
    fn query_caps(
        device_name: &str,
        backend: VideoBackendSetting,
        mut answer: impl FnMut(VideoRoute) -> Result<DeviceCapabilities, VideoError>,
    ) -> (Result<DeviceCapabilities, VideoError>, Vec<VideoRoute>) {
        let mut asked = Vec::new();
        let result = capabilities_with_fallback(Some(device_name), backend, |route, name| {
            assert_eq!(name, Some(device_name));
            asked.push(route);
            answer(route)
        });
        (result, asked)
    }

    #[test]
    fn capabilities_follow_the_same_order_as_opening() {
        // 両方に居れば DirectShow 側の対応形式。能力キャッシュの (名前, 自動) にはそれが入る
        let (result, asked) =
            query_caps(BOTH, VideoBackendSetting::Auto, |route| Ok(caps_of(route)));
        assert_eq!(result, Ok(caps_of(VideoRoute::DirectShow)));
        assert_eq!(asked, vec![VideoRoute::DirectShow]);

        // DirectShow の一覧に無ければ Media Foundation 側の対応形式
        let (result, asked) = query_caps(BOTH, VideoBackendSetting::Auto, |route| match route {
            VideoRoute::DirectShow => Err(VideoError::DeviceNotFound(BOTH.to_string())),
            VideoRoute::MediaFoundation => Ok(caps_of(route)),
        });
        assert_eq!(result, Ok(caps_of(VideoRoute::MediaFoundation)));
        assert_eq!(
            asked,
            vec![VideoRoute::DirectShow, VideoRoute::MediaFoundation]
        );
    }

    #[test]
    fn capabilities_fall_back_to_media_foundation_when_directshow_lists_nothing() {
        // `IAMStreamConfig` が無い・受け取れる形式が 0 件だと DirectShow は `Ok(空)`。
        // 開くときは Media Foundation で開くので、選択肢も Media Foundation 側にする
        let empty_on_directshow = |route| match route {
            VideoRoute::DirectShow => Ok(Vec::new()),
            VideoRoute::MediaFoundation => Ok(caps_of(route)),
        };
        let (result, asked) = query_caps(BOTH, VideoBackendSetting::Auto, empty_on_directshow);
        assert_eq!(result, Ok(caps_of(VideoRoute::MediaFoundation)));
        assert_eq!(
            asked,
            vec![VideoRoute::DirectShow, VideoRoute::MediaFoundation]
        );

        // Media Foundation でも引けなければ、開くときと同じく Media Foundation の結果
        let (result, _) = query_caps(BOTH, VideoBackendSetting::Auto, |route| match route {
            VideoRoute::DirectShow => Ok(Vec::new()),
            VideoRoute::MediaFoundation => Err(mf_open_failed()),
        });
        assert_eq!(result, Err(mf_open_failed()));

        // 「(DirectShow)」付きの名前と DirectShow 固定は、空のまま返して Media Foundation を引かない
        for (name, backend) in [
            (DS_ONLY, VideoBackendSetting::Auto),
            (BOTH, VideoBackendSetting::DirectShow),
        ] {
            let (result, asked) = query_caps(name, backend, empty_on_directshow);
            assert_eq!(result, Ok(Vec::new()), "{name} {backend:?}");
            assert_eq!(asked, vec![VideoRoute::DirectShow], "{name} {backend:?}");
        }
    }
}
