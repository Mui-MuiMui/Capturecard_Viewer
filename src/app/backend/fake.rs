//! フェイクのバックエンド。実機なしで映像と音声を流す。
//!
//! 環境変数 `CAPTURECARD_VIEWER_FAKE_DEVICES=<台数>` を指定して起動したときだけ
//! 使われる。指定が無ければ本番（`super::system`）のまま、挙動は何も変わらない。
//! **release ビルドにも入っているが、既定では無効。** ログのレベル
//! （`CAPTURECARD_VIEWER_LOG`）と同じく、設定ファイルには持たせていない。
//!
//! 中身は `crate::video::FakeVideoCapture` / `crate::audio::FakeAudioCapture` で、
//! ここはそれを trait に包むのと、環境変数の解釈だけを持つ。置き場所の理由は
//! `docs/design/device-worker.md` の「フェイクデバイス（#142）」。

use super::{AudioBackend, BackendShared, DeviceBackends, VideoBackend};
use crate::audio::{
    ActiveAudio, AudioCapabilities, AudioDirection, AudioError, FakeAudioCapture, FakeAudioOptions,
    PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::video::{
    ActiveVideo, DeviceCapabilities, FakeVideoCapture, FakeVideoOptions, VideoError, VideoLinkState,
};
use log::warn;
use std::sync::Arc;
use std::time::Duration;

/// フェイクを有効にする環境変数。値は名乗る映像デバイス（と音声の入力
/// デバイス）の台数
pub(super) const FAKE_DEVICES_ENV: &str = "CAPTURECARD_VIEWER_FAKE_DEVICES";

/// フェイクに起こさせる出来事を指定する環境変数。
/// `disconnect:<秒>` / `fail:<回数>` / `audio-error:<秒>` をカンマで区切って並べる
pub(super) const FAKE_SCENARIO_ENV: &str = "CAPTURECARD_VIEWER_FAKE_SCENARIO";

/// 名乗れる台数の上限。設定画面の一覧が埋まらない程度にとどめる
const MAX_DEVICES: u32 = 8;

/// フェイクに起こさせる出来事。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FakeScenario {
    /// 映像を開いてからこの時間が経つとフレームを止める
    pub(super) disconnect_after: Option<Duration>,
    /// 映像と音声のそれぞれで、最初にこの回数だけ開くのに失敗する
    pub(super) failures_before_success: u32,
    /// 音声を開いてからこの時間が経つとストリームのエラーを立てる
    pub(super) audio_error_after: Option<Duration>,
}

/// フェイクを組み立てる役。`DeviceWorker::spawn` が `SystemBackends` の
/// 代わりにワーカースレッドへ送る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FakeBackends {
    pub(super) device_count: u32,
    pub(super) scenario: FakeScenario,
}

impl FakeBackends {
    /// 環境変数の値からフェイクの設定を作る。フェイクを使わないなら `None`。
    ///
    /// 台数が無い・空・0・解釈できないときは使わない。**打ち間違えで
    /// フェイクになってしまうより、実機のまま起動するほうが害が小さい**ため。
    pub(super) fn from_env_values(devices: Option<&str>, scenario: Option<&str>) -> Option<Self> {
        let device_count = parse_device_count(devices)?;
        Some(Self {
            device_count,
            scenario: parse_scenario(scenario),
        })
    }
}

impl DeviceBackends for FakeBackends {
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
        let video = FakeVideoCapture::new(
            frames,
            color_conversion,
            repaint_waker,
            FakeVideoOptions {
                device_count: self.device_count,
                disconnect_after: self.scenario.disconnect_after,
                failures_before_success: self.scenario.failures_before_success,
            },
        );
        let audio = FakeAudioCapture::new(
            audio_controls,
            FakeAudioOptions {
                input_count: self.device_count,
                failures_before_success: self.scenario.failures_before_success,
                stream_error_after: self.scenario.audio_error_after,
            },
        );
        (Box::new(video), Box::new(audio))
    }
}

/// 台数の指定を読む。使わないなら `None`。上限を超えたら上限へ丸める
fn parse_device_count(value: Option<&str>) -> Option<u32> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    match value.parse::<u32>() {
        Ok(0) => None,
        Ok(count) if count > MAX_DEVICES => {
            warn!(
                "{} の {} 台は多すぎるので {} 台にする",
                FAKE_DEVICES_ENV, count, MAX_DEVICES
            );
            Some(MAX_DEVICES)
        }
        Ok(count) => Some(count),
        Err(_) => {
            warn!(
                "{} の値 '{}' を台数として読めないのでフェイクを使わない",
                FAKE_DEVICES_ENV, value
            );
            None
        }
    }
}

/// シナリオの指定を読む。読めない項目は記録して読み飛ばす
fn parse_scenario(value: Option<&str>) -> FakeScenario {
    let mut scenario = FakeScenario::default();
    let Some(value) = value else {
        return scenario;
    };

    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let parsed = item.split_once(':').and_then(|(key, arg)| {
            let arg = arg.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                // 0 秒は受け付けない。1 枚も届かないと切断とみなされない
                // （`docs/design/reconnect.md`）ので、再現にならない
                "disconnect" => arg
                    .parse::<u64>()
                    .ok()
                    .filter(|secs| *secs > 0)
                    .map(|secs| {
                        scenario.disconnect_after = Some(Duration::from_secs(secs));
                    }),
                // 0 秒も受け付けない。開いた直後に毎回エラーになり、
                // 再接続の間隔の下限でしか音が出なくなる
                "audio-error" => arg
                    .parse::<u64>()
                    .ok()
                    .filter(|secs| *secs > 0)
                    .map(|secs| {
                        scenario.audio_error_after = Some(Duration::from_secs(secs));
                    }),
                "fail" => arg.parse::<u32>().ok().map(|count| {
                    scenario.failures_before_success = count;
                }),
                _ => None,
            }
        });
        if parsed.is_none() {
            warn!(
                "{} の '{}' を読めないので無視する（書式は disconnect:<秒> / fail:<回数> / audio-error:<秒>）",
                FAKE_SCENARIO_ENV, item
            );
        }
    }
    scenario
}

impl VideoBackend for FakeVideoCapture {
    fn list_devices(&self) -> Vec<(String, String)> {
        FakeVideoCapture::list_devices(self)
    }

    fn capabilities(&self, device_name: Option<&str>) -> Result<DeviceCapabilities, VideoError> {
        FakeVideoCapture::capabilities(self, device_name)
    }

    fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        FakeVideoCapture::start_capture(self, device_name, resolution, format, fps)
    }

    fn stop_capture(&mut self) {
        FakeVideoCapture::stop_capture(self);
    }

    fn link_state(&self) -> VideoLinkState {
        FakeVideoCapture::link_state(self)
    }

    fn active(&self) -> Option<ActiveVideo> {
        FakeVideoCapture::active(self)
    }
}

impl AudioBackend for FakeAudioCapture {
    fn list_input_devices(&self) -> Vec<String> {
        FakeAudioCapture::list_input_devices(self)
    }

    fn list_output_devices(&self) -> Vec<String> {
        FakeAudioCapture::list_output_devices(self)
    }

    fn default_input_device_name(&self) -> Option<String> {
        FakeAudioCapture::default_input_device_name(self)
    }

    fn default_output_device_name(&self) -> Option<String> {
        FakeAudioCapture::default_output_device_name(self)
    }

    fn capabilities(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<AudioCapabilities, AudioError> {
        FakeAudioCapture::capabilities(self, direction, device_name)
    }

    fn start_passthrough(&mut self, request: &PassthroughRequest<'_>) -> Result<(), AudioError> {
        FakeAudioCapture::start_passthrough(self, request)
    }

    fn stop_capture(&mut self) {
        FakeAudioCapture::stop_capture(self);
    }

    fn active(&self) -> Option<ActiveAudio> {
        FakeAudioCapture::active(self)
    }

    fn resample_status(&self) -> Option<ResampleStatus> {
        FakeAudioCapture::resample_status(self)
    }

    fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>> {
        FakeAudioCapture::resample_telemetry(self).cloned()
    }

    fn underrun_count(&self) -> Option<u32> {
        FakeAudioCapture::underrun_count(self)
    }

    fn take_stream_error(&self) -> bool {
        FakeAudioCapture::take_stream_error(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_values_without_count_is_disabled() {
        // 環境変数が無ければ今までと同じく実機で動く
        assert_eq!(FakeBackends::from_env_values(None, None), None);
        assert_eq!(FakeBackends::from_env_values(Some(""), None), None);
        assert_eq!(FakeBackends::from_env_values(Some("  "), None), None);
        assert_eq!(FakeBackends::from_env_values(Some("0"), None), None);
        // シナリオだけ指定しても有効にならない
        assert_eq!(FakeBackends::from_env_values(None, Some("fail:3")), None);
    }

    #[test]
    fn from_env_values_unreadable_count_is_disabled() {
        assert_eq!(FakeBackends::from_env_values(Some("two"), None), None);
        assert_eq!(FakeBackends::from_env_values(Some("-1"), None), None);
    }

    #[test]
    fn from_env_values_count_enables_fakes() {
        assert_eq!(
            FakeBackends::from_env_values(Some(" 2 "), None),
            Some(FakeBackends {
                device_count: 2,
                scenario: FakeScenario::default(),
            })
        );
    }

    #[test]
    fn from_env_values_large_count_is_clamped() {
        assert_eq!(
            FakeBackends::from_env_values(Some("100"), None).map(|fake| fake.device_count),
            Some(MAX_DEVICES)
        );
    }

    #[test]
    fn parse_scenario_reads_disconnect_and_fail() {
        assert_eq!(
            parse_scenario(Some("disconnect:10")),
            FakeScenario {
                disconnect_after: Some(Duration::from_secs(10)),
                failures_before_success: 0,
                audio_error_after: None,
            }
        );
        assert_eq!(
            parse_scenario(Some("fail:3")),
            FakeScenario {
                disconnect_after: None,
                failures_before_success: 3,
                audio_error_after: None,
            }
        );
        // カンマで並べられる。大文字小文字と前後の空白は問わない
        assert_eq!(
            parse_scenario(Some(" FAIL:2 , disconnect: 5 ")),
            FakeScenario {
                disconnect_after: Some(Duration::from_secs(5)),
                failures_before_success: 2,
                audio_error_after: None,
            }
        );
    }

    #[test]
    fn parse_scenario_reads_audio_error() {
        assert_eq!(
            parse_scenario(Some("Audio-Error: 3 ,fail:1")),
            FakeScenario {
                disconnect_after: None,
                failures_before_success: 1,
                audio_error_after: Some(Duration::from_secs(3)),
            }
        );
    }

    #[test]
    fn parse_scenario_skips_unreadable_items() {
        // 読めない項目だけを捨て、読める項目は残す
        assert_eq!(
            parse_scenario(Some("disconnect:0,explode:1,fail:x,fail:1,disconnect")),
            FakeScenario {
                disconnect_after: None,
                failures_before_success: 1,
                audio_error_after: None,
            }
        );
        assert_eq!(
            parse_scenario(Some("audio-error:0,audio-error:x,audio-error")),
            FakeScenario::default()
        );
        assert_eq!(parse_scenario(None), FakeScenario::default());
        assert_eq!(parse_scenario(Some("")), FakeScenario::default());
    }

    #[test]
    fn worker_with_fakes_retries_then_streams_frames() {
        // ワーカー本体（`worker_loop::run`）にフェイクを載せ、UI スレッドが
        // 送るのと同じ設定で開かせる。1 回目は失敗させ（fail:1）、
        // 再試行で繋がって実際にフレームが届くところまでを通す
        use crate::app::worker::{DeviceCommand, DeviceEvent, DeviceSnapshot};
        use crate::app::worker_loop::testing::config_for;
        use crate::audio::AudioControls;
        use crate::repaint::RepaintWaker;
        use crate::video::{SharedColorConversion, VideoFrames};
        use std::sync::mpsc::channel;
        use std::sync::RwLock;
        use std::time::Instant;

        let backends = FakeBackends::from_env_values(Some("2"), Some("fail:1")).expect("有効");
        let frames = VideoFrames::new();
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = channel();
        let snapshot = Arc::new(RwLock::new(DeviceSnapshot::default()));
        let handle = {
            let frames = frames.clone();
            let snapshot = Arc::clone(&snapshot);
            std::thread::spawn(move || {
                crate::app::worker_loop::run(
                    command_rx,
                    event_tx,
                    snapshot,
                    BackendShared {
                        frames,
                        color_conversion: Arc::new(SharedColorConversion::new()),
                        audio_controls: Arc::new(AudioControls::default()),
                        repaint_waker: RepaintWaker::new(),
                    },
                    Box::new(backends),
                );
            })
        };

        command_tx
            .send(DeviceCommand::ApplyConfig {
                config: Box::new(config_for(
                    Some("Fake Camera 2"),
                    Some("Fake Audio Input 1"),
                )),
                initial: true,
            })
            .expect("送れる");

        // バックオフの最初の待ちは 200ms。CI の遅いランナーでも収まるよう長めに待つ
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            events.extend(event_rx.try_iter());
            let connected = |wanted: fn(&DeviceEvent) -> bool| events.iter().any(wanted);
            if connected(|event| matches!(event, DeviceEvent::VideoConnected))
                && connected(|event| matches!(event, DeviceEvent::AudioConnected))
                && frames.latest().is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoFailed(_))),
            "1 回目の映像は失敗する: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::AudioFailed(_))),
            "1 回目の音声は失敗する: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoConnected)),
            "映像が繋がらない: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::AudioConnected)),
            "音声が繋がらない: {events:?}"
        );
        let frame = frames.latest().expect("フレームが届いている");
        // 解像度は未指定なので 1280x720。2 番は青のベタ塗りで、右下は焼き込みの外
        assert_eq!((frame.width, frame.height), (1280, 720));
        let last = frame.data.len() - 3;
        let [r, g, b] = [frame.data[last], frame.data[last + 1], frame.data[last + 2]];
        assert!(r <= 2 && g <= 2 && b.abs_diff(191) <= 2, "{r} {g} {b}");

        // 観測値にもフェイクの名前で載る（「接続状態」タブに出るもの）
        let active_video = snapshot
            .read()
            .expect("読める")
            .active_video
            .clone()
            .map(|active| active.device_name);
        assert_eq!(active_video.as_deref(), Some("Fake Camera 2"));

        // 終了で生成スレッドまで止まる（join できる）こと
        command_tx.send(DeviceCommand::Shutdown).expect("送れる");
        handle.join().expect("ワーカーが終わる");
    }
}
