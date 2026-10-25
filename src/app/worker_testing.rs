//! デバイスワーカーのテストの組み立て（`#[cfg(test)]`）。モックを載せた `WorkerState` を
//! スレッドを起こさずに作る、設定を流し込む、届いたイベントを取り出す、ログを捕まえる。
//! `worker_loop` / `worker_timers` / `worker_connect` などのテストから使う。

use super::backend::mock::{MockAudioBackend, MockVideoBackend};
use super::backend::{AudioBackend, VideoBackend};
use super::worker::{DeviceCommand, DeviceConfig, DeviceEvent, DeviceSnapshot};
use super::worker_loop::WorkerState;
use crate::audio::AudioControls;
use crate::repaint::RepaintWaker;
use crate::settings::DEFAULT_BUFFER_MS;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::sync::RwLock;

/// モックのバックエンドを載せた `WorkerState` を、スレッドを起こさずに作る。
///
/// **実時間を進めずに `tick` を回すためのもの。** 本物のループ
/// （`run`）は `Instant::now()` で駆動するが、こちらはテストが渡した
/// 時刻をそのまま使えるので、バックオフの待ち時間だけ「進めた」ことに
/// できる。
pub(in crate::app) fn mock_state(
    video: &MockVideoBackend,
    audio: &MockAudioBackend,
) -> (WorkerState, std::sync::mpsc::Receiver<DeviceEvent>) {
    state_with(Box::new(video.clone()), Box::new(audio.clone()))
}

/// 任意のバックエンド（フェイクなど）を載せた `WorkerState` を作る。
/// 使い方は `mock_state` と同じ
pub(in crate::app) fn state_with(
    video: Box<dyn VideoBackend>,
    audio: Box<dyn AudioBackend>,
) -> (WorkerState, std::sync::mpsc::Receiver<DeviceEvent>) {
    let (event_tx, event_rx) = channel();
    let state = WorkerState::new(
        video,
        audio,
        Arc::new(AudioControls::default()),
        event_tx,
        Arc::new(RwLock::new(DeviceSnapshot::default())),
        RepaintWaker::new(),
    );
    (state, event_rx)
}

/// 届いているイベントを全て取り出す。
pub(in crate::app) fn drain(events: &std::sync::mpsc::Receiver<DeviceEvent>) -> Vec<DeviceEvent> {
    events.try_iter().collect()
}

/// 設定を適用する。`WorkerState::handle` はこのファイルの私有なので、
/// 他のファイルのテストからはここを通す。
pub(in crate::app) fn apply_config(state: &mut WorkerState, config: DeviceConfig, initial: bool) {
    state.handle(DeviceCommand::ApplyConfig {
        config: Box::new(config),
        initial,
    });
}

/// デバイス名だけを指定した `DeviceConfig` を作る。
pub(in crate::app) fn config_for(
    video_device: Option<&str>,
    input_device: Option<&str>,
) -> DeviceConfig {
    DeviceConfig {
        video: (
            video_device.map(str::to_string),
            None,
            None,
            None,
            crate::settings::VideoBackendSetting::Auto,
            false,
        ),
        audio: (
            input_device.map(str::to_string),
            None,
            None,
            None,
            DEFAULT_BUFFER_MS,
            crate::settings::AudioInputSource::Device,
        ),
        auto_reconnect: false,
    }
}

/// テストのスレッドで出たログを、レベルと本文の組で取り出すロガー（#506）。
///
/// ロガーはテストのバイナリ全体で 1 つしか登録できないので、記録する先を
/// スレッドごとに分ける。`capture_logs` の中のスレッドだけが記録し、
/// 並行して走る他のテストのログは混ざらない。
struct CaptureLogger;

thread_local! {
    static CAPTURED: std::cell::RefCell<Option<Vec<(log::Level, String)>>> =
        const { std::cell::RefCell::new(None) };
}

impl log::Log for CaptureLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        CAPTURED.with(|captured| {
            if let Some(lines) = captured.borrow_mut().as_mut() {
                lines.push((record.level(), record.args().to_string()));
            }
        });
    }

    fn flush(&self) {}
}

static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;
static INSTALL_CAPTURE_LOGGER: std::sync::Once = std::sync::Once::new();

/// `f` を走らせ、その間にこのスレッドで出たログを全レベル分返す。
/// ワーカーの `tick` はテストのスレッドの上で回るので、そのまま拾える。
pub(in crate::app) fn capture_logs(f: impl FnOnce()) -> Vec<(log::Level, String)> {
    INSTALL_CAPTURE_LOGGER.call_once(|| {
        if log::set_logger(&CAPTURE_LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
    CAPTURED.with(|captured| *captured.borrow_mut() = Some(Vec::new()));
    f();
    CAPTURED.with(|captured| captured.borrow_mut().take().unwrap_or_default())
}
