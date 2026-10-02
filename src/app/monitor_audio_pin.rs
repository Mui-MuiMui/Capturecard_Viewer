//! 音声の入力が映像デバイスの音声ピン（`[audio] input_source = "video_pin"`、#388）の
//! ときの**判定**。音声を開いてよいか、待つならその理由、映像の開き直しに合わせて
//! 音声を開き直すか。
//!
//! `super::monitor` と同じく純粋関数だけを置く（時計もデバイスも触らない）。
//! `monitor.rs` が 800 行に近いので分けてある。呼ぶのはデバイスワーカーの
//! `worker_audio_connect`（開くか待つか）と `worker_audio_timers`（開き直すか）。
//! 理由は `docs/design/directshow-audio.md` の (3)。

use crate::audio::{AudioInputRoute, AudioPinPresence, AudioPinState, PinFailure, PinFormat};
use crate::i18n::{self, Text};
use crate::video::capture::CaptureApi;
use crate::video::{directshow_friendly_name, ActiveVideo};

/// 音声ピンを使えないので、音声を開かずに待っている理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PinWait {
    /// 映像デバイスが開いていない
    VideoNotOpen,
    /// Media Foundation で開いた映像には音声ピンが無い
    MediaFoundation,
    /// DirectShow で開いたが、この映像デバイスには音声ピンが無い
    NoPin,
    /// 音声ピンはあるが、映像のグラフに繋いでいない。映像の開き直しを待っている
    NotConnected,
    /// 音声ピンに繋げなかった
    ConnectFailed(PinFailure),
}

impl PinWait {
    /// 「接続状態」タブとトーストに出す理由。
    pub(super) fn message(&self) -> String {
        match self {
            PinWait::VideoNotOpen => Text::AudioPinVideoNotOpen.get().to_string(),
            PinWait::MediaFoundation => Text::AudioPinMediaFoundation.get().to_string(),
            PinWait::NoPin => Text::AudioPinMissing.get().to_string(),
            PinWait::NotConnected => Text::AudioPinNotConnected.get().to_string(),
            PinWait::ConnectFailed(failure) => i18n::audio_pin_connect_failed(failure),
        }
    }
}

/// 音声ピンから音声を開けるか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PinReadiness {
    /// 開ける。`graph` は映像のグラフの番号で、`PassthroughInput::VideoPin` へ渡す
    Ready { graph: u64 },
    /// 開かずに待つ
    Wait(PinWait),
}

/// 映像の観測値（`VideoBackend::active`）から、音声ピンで音声を開けるかを決める。
///
/// Media Foundation で開いた映像とフェイク・モックの映像はどちらも
/// `NotApplicable` なので、経路（`CaptureApi`）で理由を分ける。
pub(super) fn decide_pin_readiness(video: Option<&ActiveVideo>) -> PinReadiness {
    let Some(video) = video else {
        return PinReadiness::Wait(PinWait::VideoNotOpen);
    };
    match &video.audio_pin {
        AudioPinState::Connected(connection) => PinReadiness::Ready {
            graph: connection.graph,
        },
        AudioPinState::NotApplicable => match video.api {
            CaptureApi::MediaFoundation => PinReadiness::Wait(PinWait::MediaFoundation),
            CaptureApi::DirectShow | CaptureApi::Fake => PinReadiness::Wait(PinWait::NoPin),
        },
        AudioPinState::Missing => PinReadiness::Wait(PinWait::NoPin),
        AudioPinState::Available => PinReadiness::Wait(PinWait::NotConnected),
        AudioPinState::Failed(failure) => {
            PinReadiness::Wait(PinWait::ConnectFailed(failure.clone()))
        }
    }
}

/// 入力が音声ピンのとき、音声を開き直す要求を立てるか。ワーカーの `tick` が毎回呼ぶ。
///
/// - `audio_route`: 開いている音声の入力の経路。開いていなければ `None`
/// - `waiting`: 開かずに待っているならその理由（前回の判定）
/// - `retrying`: 音声の接続を追いかけている最中か。最中なら、次の試行が
///   今の映像の状態で開くか待つかを決めるので、ここでは立てない
///
/// **映像を開き直すと番号が進むので、音声も毎回開き直す**（形式が同じでも
/// 分岐させない。`docs/design/directshow-audio.md` の「開き直しの順序」）。
/// 待っている間は、理由が変わったとき（映像が開いた・経路が変わった）だけ立てる。
/// 同じ理由で立て続けると、そのたびに同じ理由を通知することになる。
pub(super) fn should_resync_pin_audio(
    readiness: &PinReadiness,
    audio_route: Option<AudioInputRoute>,
    waiting: Option<&PinWait>,
    retrying: bool,
) -> bool {
    if retrying {
        return false;
    }
    match audio_route {
        Some(AudioInputRoute::VideoPin { graph }) => *readiness != PinReadiness::Ready { graph },
        // 設定は音声ピンなのに WASAPI の入力で開いている。食い違いを直す
        Some(AudioInputRoute::Device) => true,
        None => waiting.is_some_and(|reason| *readiness != PinReadiness::Wait(reason.clone())),
    }
}

/// 設定ダイアログで映像デバイスの音声（項目名は映像デバイスの名前）を選べるか
/// （#394、#409）。
///
/// - `video`: いま開いている映像の観測値
/// - `presence`: 列挙の時点で調べた、対象の映像デバイス（開いていればその相手、
///   開いていなければ設定の映像デバイス）の音声ピンの有無（`presence_of`）。
///   一覧に載っていなければ `None`
///
/// **選べないのは、音声ピンが無いと分かっているときだけ。** 開いた結果で無い
/// （DirectShow で開いて無かった）か、列挙の時点で無いと分かったとき。開いていない・
/// Media Foundation で開いた・有無が分からないときは選べる側に倒し、選んで適用したら
/// 「開かずに待つ + 理由」の経路（`decide_pin_readiness`）に乗る（#409）。
///
/// `Ok` には繋いでいる音声ピンの形式を入れる。サンプリングレートとチャンネル数の
/// 選択肢をこの形式 1 つで作るためで、繋いでいない間は `None`（入力側の制約なし）
/// （`docs/design/directshow-audio.md` の (5)、(6)）。
pub(super) fn pin_choice(
    video: Option<&ActiveVideo>,
    presence: Option<AudioPinPresence>,
) -> Result<Option<PinFormat>, PinWait> {
    match decide_pin_readiness(video) {
        PinReadiness::Ready { .. } => Ok(video.and_then(|video| match &video.audio_pin {
            AudioPinState::Connected(connection) => Some(connection.format),
            _ => None,
        })),
        PinReadiness::Wait(PinWait::NotConnected | PinWait::ConnectFailed(_)) => Ok(None),
        // 開いた結果で無いと分かっている
        PinReadiness::Wait(PinWait::NoPin) => Err(PinWait::NoPin),
        // 開いていない・Media Foundation で開いた。列挙の時点の有無で決める
        PinReadiness::Wait(_) => match presence {
            Some(AudioPinPresence::Absent) => Err(PinWait::NoPin),
            Some(AudioPinPresence::Present | AudioPinPresence::Unknown) | None => Ok(None),
        },
    }
}

/// 列挙の時点の有無の一覧から、映像デバイス `device` の分を引く（#409）。
///
/// 一覧の名前と `device` は「(DirectShow)」の印の有無が揃っていない（一覧は
/// DirectShow の表示名、設定と開いた結果は印付きのことがある）ので、両方から
/// 印を外して突き合わせる。載っていなければ `None`。
pub(super) fn presence_of(
    list: &[(String, AudioPinPresence)],
    device: &str,
) -> Option<AudioPinPresence> {
    let bare = |name: &str| directshow_friendly_name(name).unwrap_or(name).to_string();
    let wanted = bare(device);
    list.iter()
        .find(|(name, _)| bare(name) == wanted)
        .map(|(_, presence)| *presence)
}

/// 入力が未設定のまま起動したとき、最初の映像の試行の結果から入力を
/// 「映像デバイスの音声」に決めるか（#394）。
///
/// 映像が開けていて音声ピンがある（初回は繋がずに開くので、ふつうは「あるが
/// 繋いでいない」）ときだけ真。Media Foundation で開いた・ピンが無い・映像が
/// 開けなかったときは偽で、今までどおり WASAPI の列挙の先頭に決める
/// （`docs/design/directshow-audio.md` の (4) の「初回の既定」）。
pub(super) fn default_input_uses_pin(video: Option<&ActiveVideo>) -> bool {
    video.is_some_and(|video| {
        matches!(
            video.audio_pin,
            AudioPinState::Available | AudioPinState::Connected(_)
        )
    })
}

/// 音声ピンを待つ理由を、通知せずに待ってよいか（#394）。
///
/// 「音声ピンを繋ぐために映像を開き直すのを待っている」（`NotConnected`）で、
/// まだ映像の開き直しが済んでいない（`video_reopen_pending`。設定の映像の接続対象と
/// いま開いている相手が違う）なら、すぐに繋がるので理由を出さない。入力を
/// 「映像デバイスの音声」へ切り替えた直後と、初回の既定で決まった直後は、映像の
/// 開き直しに「成功から 1 秒」の下限が掛かり、その間に音声の試行が先に来る。
/// 映像が開き直ると状態が変わり、`tick` の監視が音声の要求を立て直す。
pub(super) fn waits_silently(reason: &PinWait, video_reopen_pending: bool) -> bool {
    *reason == PinWait::NotConnected && video_reopen_pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{PinConnection, PinSampleType};

    fn video(api: CaptureApi, audio_pin: AudioPinState) -> ActiveVideo {
        ActiveVideo {
            device_name: "AVerMedia GC551 Video Capture".to_string(),
            api,
            resolution: Some((1920, 1080)),
            format: Some("YUY2".to_string()),
            requested_fps: 60,
            audio_pin,
        }
    }

    fn connected(graph: u64) -> AudioPinState {
        AudioPinState::Connected(PinConnection {
            graph,
            device: "AVerMedia GC551 Video Capture".to_string(),
            format: PinFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_type: PinSampleType::I16,
            },
            chunk_bytes: Some(1920),
        })
    }

    #[test]
    fn decide_pin_readiness_is_ready_with_the_graph_of_a_connected_pin() {
        let active = video(CaptureApi::DirectShow, connected(7));
        assert_eq!(
            decide_pin_readiness(Some(&active)),
            PinReadiness::Ready { graph: 7 }
        );
    }

    #[test]
    fn decide_pin_readiness_waits_with_a_reason_for_each_video_state() {
        let cases = [
            (None, PinWait::VideoNotOpen),
            (
                Some(video(
                    CaptureApi::MediaFoundation,
                    AudioPinState::NotApplicable,
                )),
                PinWait::MediaFoundation,
            ),
            (
                Some(video(CaptureApi::DirectShow, AudioPinState::Missing)),
                PinWait::NoPin,
            ),
            (
                Some(video(CaptureApi::Fake, AudioPinState::NotApplicable)),
                PinWait::NoPin,
            ),
            (
                Some(video(CaptureApi::DirectShow, AudioPinState::Available)),
                PinWait::NotConnected,
            ),
            (
                Some(video(
                    CaptureApi::DirectShow,
                    AudioPinState::Failed(PinFailure::Connect("E_FAIL".to_string())),
                )),
                PinWait::ConnectFailed(PinFailure::Connect("E_FAIL".to_string())),
            ),
        ];
        for (active, expected) in cases {
            assert_eq!(
                decide_pin_readiness(active.as_ref()),
                PinReadiness::Wait(expected.clone()),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn should_resync_pin_audio_when_the_video_graph_changed() {
        // 映像を開き直して番号が進んだ。古いグラフの差し込み先のままでは鳴らない
        let ready = PinReadiness::Ready { graph: 2 };
        let route = Some(AudioInputRoute::VideoPin { graph: 1 });
        assert!(should_resync_pin_audio(&ready, route, None, false));
        // 同じ番号なら何もしない
        let route = Some(AudioInputRoute::VideoPin { graph: 2 });
        assert!(!should_resync_pin_audio(&ready, route, None, false));
    }

    #[test]
    fn should_resync_pin_audio_when_the_video_closed_under_an_open_pin() {
        // 映像が閉じたら、音声も閉じて理由を出すために開き直しへ回す
        let wait = PinReadiness::Wait(PinWait::VideoNotOpen);
        let route = Some(AudioInputRoute::VideoPin { graph: 3 });
        assert!(should_resync_pin_audio(&wait, route, None, false));
    }

    #[test]
    fn should_resync_pin_audio_while_waiting_only_when_the_reason_changes() {
        let waiting = PinWait::VideoNotOpen;
        // 映像が開いて音声ピンが繋がった
        assert!(should_resync_pin_audio(
            &PinReadiness::Ready { graph: 1 },
            None,
            Some(&waiting),
            false
        ));
        // 映像が Media Foundation で開いた（理由が変わる）
        assert!(should_resync_pin_audio(
            &PinReadiness::Wait(PinWait::MediaFoundation),
            None,
            Some(&waiting),
            false
        ));
        // 同じ理由のまま。2 秒ごとに同じ通知を出さない
        assert!(!should_resync_pin_audio(
            &PinReadiness::Wait(PinWait::VideoNotOpen),
            None,
            Some(&waiting),
            false
        ));
    }

    #[test]
    fn should_resync_pin_audio_not_while_retrying_or_idle() {
        let ready = PinReadiness::Ready { graph: 5 };
        // 追いかけている最中は、次の試行が今の状態で決める
        assert!(!should_resync_pin_audio(
            &ready,
            Some(AudioInputRoute::VideoPin { graph: 1 }),
            None,
            true
        ));
        // 開いておらず待ってもいない（エラーで閉じたまま等）なら、ここでは立てない
        assert!(!should_resync_pin_audio(&ready, None, None, false));
    }

    #[test]
    fn should_resync_pin_audio_when_opened_from_a_wasapi_device() {
        // 設定は音声ピンなのに WASAPI の入力で開いている食い違い
        assert!(should_resync_pin_audio(
            &PinReadiness::Ready { graph: 1 },
            Some(AudioInputRoute::Device),
            None,
            false
        ));
    }

    #[test]
    fn pin_wait_message_is_not_empty_for_every_reason() {
        for reason in [
            PinWait::VideoNotOpen,
            PinWait::MediaFoundation,
            PinWait::NoPin,
            PinWait::NotConnected,
            PinWait::ConnectFailed(PinFailure::Run("E_FAIL".to_string())),
        ] {
            assert!(!reason.message().is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn pin_choice_is_selectable_while_the_video_has_an_audio_pin() {
        let format = PinFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: PinSampleType::I16,
        };
        let active = video(CaptureApi::DirectShow, connected(1));
        assert_eq!(pin_choice(Some(&active), None), Ok(Some(format)));
        // 繋いでいない・繋げなかったときも選べる。形式は分からないので制約にしない
        for pin in [
            AudioPinState::Available,
            AudioPinState::Failed(PinFailure::Connect("E_FAIL".to_string())),
        ] {
            let active = video(CaptureApi::DirectShow, pin);
            assert_eq!(pin_choice(Some(&active), None), Ok(None));
            // 開いた結果が先。列挙の時点の有無が食い違っても見ない
            let absent = Some(AudioPinPresence::Absent);
            assert_eq!(pin_choice(Some(&active), absent), Ok(None));
        }
    }

    #[test]
    fn pin_choice_is_not_selectable_only_when_known_to_have_no_pin() {
        // DirectShow で開いて音声ピンが無かった。列挙の時点の有無は見ない
        let missing = video(CaptureApi::DirectShow, AudioPinState::Missing);
        for presence in [
            None,
            Some(AudioPinPresence::Present),
            Some(AudioPinPresence::Unknown),
        ] {
            assert_eq!(pin_choice(Some(&missing), presence), Err(PinWait::NoPin));
        }
        // 開いていない・Media Foundation で開いたときは、列挙の時点で無いと分かった
        // ときだけ選べない
        let media_foundation = video(CaptureApi::MediaFoundation, AudioPinState::NotApplicable);
        for active in [None, Some(&media_foundation)] {
            assert_eq!(
                pin_choice(active, Some(AudioPinPresence::Absent)),
                Err(PinWait::NoPin)
            );
        }
    }

    #[test]
    fn pin_choice_is_selectable_before_the_video_opens_unless_known_absent() {
        // 開いていなくても、ある・不明・一覧に無いなら選べる。適用したら開かずに待つ
        // 経路（decide_pin_readiness）に乗る
        let media_foundation = video(CaptureApi::MediaFoundation, AudioPinState::NotApplicable);
        for active in [None, Some(&media_foundation)] {
            for presence in [
                None,
                Some(AudioPinPresence::Present),
                Some(AudioPinPresence::Unknown),
            ] {
                assert_eq!(pin_choice(active, presence), Ok(None), "{presence:?}");
            }
        }
    }

    #[test]
    fn presence_of_matches_names_with_or_without_the_directshow_mark() {
        let list = vec![
            (
                "AVerMedia GC551 Video Capture".to_string(),
                AudioPinPresence::Present,
            ),
            ("OBS Virtual Camera".to_string(), AudioPinPresence::Absent),
        ];
        assert_eq!(
            presence_of(&list, "AVerMedia GC551 Video Capture (DirectShow)"),
            Some(AudioPinPresence::Present)
        );
        assert_eq!(
            presence_of(&list, "AVerMedia GC551 Video Capture"),
            Some(AudioPinPresence::Present)
        );
        assert_eq!(
            presence_of(&list, "OBS Virtual Camera (DirectShow)"),
            Some(AudioPinPresence::Absent)
        );
        assert_eq!(presence_of(&list, "USB Video"), None);
        assert_eq!(presence_of(&[], "USB Video"), None);
        // 一覧の側が印付きでも突き合わせる
        let marked = vec![(
            "USB Video (DirectShow)".to_string(),
            AudioPinPresence::Absent,
        )];
        assert_eq!(
            presence_of(&marked, "USB Video"),
            Some(AudioPinPresence::Absent)
        );
    }

    #[test]
    fn default_input_uses_pin_only_when_the_opened_video_has_one() {
        assert!(default_input_uses_pin(Some(&video(
            CaptureApi::DirectShow,
            AudioPinState::Available
        ))));
        assert!(default_input_uses_pin(Some(&video(
            CaptureApi::DirectShow,
            connected(1)
        ))));
        assert!(!default_input_uses_pin(None));
        for (api, pin) in [
            (CaptureApi::MediaFoundation, AudioPinState::NotApplicable),
            (CaptureApi::DirectShow, AudioPinState::Missing),
            (
                CaptureApi::DirectShow,
                AudioPinState::Failed(PinFailure::Run("E_FAIL".to_string())),
            ),
        ] {
            assert!(!default_input_uses_pin(Some(&video(api, pin))));
        }
    }

    #[test]
    fn waits_silently_only_for_a_pending_reopen() {
        assert!(waits_silently(&PinWait::NotConnected, true));
        // 開き直しが済んだのにまだ繋がっていないなら、理由を出す
        assert!(!waits_silently(&PinWait::NotConnected, false));
        // ほかの理由は開き直しを待っても直らない
        assert!(!waits_silently(&PinWait::NoPin, true));
        assert!(!waits_silently(&PinWait::VideoNotOpen, true));
    }
}
