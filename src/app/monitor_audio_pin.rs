//! 音声の入力が映像デバイスの音声ピン（`[audio] input_source = "video_pin"`、#388）の
//! ときの**判定**。音声を開いてよいか、待つならその理由、映像の開き直しに合わせて
//! 音声を開き直すか。
//!
//! `super::monitor` と同じく純粋関数だけを置く（時計もデバイスも触らない）。
//! `monitor.rs` が 800 行に近いので分けてある。呼ぶのはデバイスワーカーの
//! `worker_audio_connect`（開くか待つか）と `worker_audio_timers`（開き直すか）。
//! 理由は `docs/design/directshow-audio.md` の (3)。

use crate::audio::{AudioInputRoute, AudioPinState, PinFailure};
use crate::i18n::{self, Text};
use crate::video::capture::CaptureApi;
use crate::video::ActiveVideo;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{PinConnection, PinFormat, PinSampleType};

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
}
