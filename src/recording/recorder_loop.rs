//! 録画スレッドの本体。コマンドの受け口と、録画の経路の切り替え。
//!
//! 窓口（`super::recorder::Recorder`）が起こし、`Shutdown` を送るか窓口が落ちたら抜ける。
//! **自分からは抜けない**（窓口が止めると決める。理由は `super::recorder`）。
//!
//! - リプレイバッファが OFF の録画は `super::session::Session`（①②の経路）
//! - リプレイバッファが ON なら `super::replay::ReplayPipeline` を回し続け、録画もそこから行う
//! - ON / OFF を切り替えたときに録画中なら、差し込み口が 1 つしか無いので待たせる。
//!   リプレイバッファを通さない録画の間に ON にされたら、その録画が終わってから溜め始める
//!   （`ReplayState::Pending`）。リプレイバッファを通す録画の間にエンコーダが変わる設定や
//!   OFF にされたら、その録画が終わってから反映する（`Worker::deferred`）
//! - リプレイバッファの中身だけを保存する（#438）ときも `ReplayPipeline` の録画の口を使う。
//!   保存中の設定の変更も録画中と同じく保存が終わってから反映する
//!
//! 録画もリプレイバッファも動いていない間（リプレイバッファを用意できずに止まっているとき）は、
//! コマンドが来るまで待つだけで起きない。

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info};

use super::recorder::{RecordingCommand, RecordingEvent, RecordingRequest, RecordingTelemetry};
use super::replay::ReplayPipeline;
use super::replay_config::ReplayConfig;
use super::replay_save::{RecordingKind, SaveReplayBlock};
use super::session::{fail, Session};
use super::RecordingError;
use crate::audio::AudioTap;
use crate::com::{ComApartment, ComModel, MfPlatform};
use crate::video::VideoTap;

/// コマンドを待つ間隔。コマンドとリングの両方を見るため、数 ms で起きてリングを空にする。
///
/// リングの `Arc` が `FrameSink` の Vec の回収を妨げないよう、取り出しは速いほうがよい
/// （`FrameSink` は 2 世代前の Vec を回収するので、60fps なら 33ms の猶予がある）。
/// `thread::sleep` では待たない。録画もリプレイバッファも無い間（リプレイバッファを
/// 用意できずに止まっているとき）は、コマンドが来るまで待つだけで起きない。
const POLL_INTERVAL: Duration = Duration::from_millis(4);

/// リプレイバッファの状態。録画スレッドの中だけにある。
enum ReplayState {
    Off,
    /// ON にされたが、リプレイバッファを通さない録画が差し込み口を使っている。
    /// その録画が終わったら始める
    Pending(ReplayConfig),
    Running(Box<ReplayPipeline>),
    /// 続けられなかった。設定が変わるまで作り直さない
    Failed(ReplayConfig),
}

/// 録画スレッドの状態。
struct Worker {
    events: Sender<RecordingEvent>,
    video_tap: VideoTap,
    audio_tap: AudioTap,
    telemetry: Arc<RecordingTelemetry>,
    /// リプレイバッファを通さない録画
    session: Option<Session>,
    replay: ReplayState,
    /// リプレイバッファを通す録画の間に変えられた設定。録画が終わってから反映する
    /// （エンコーダを作り直すとリングもファイルも続けられないため）。`Some(None)` は OFF
    deferred: Option<Option<ReplayConfig>>,
    /// COM か MF を初期化できなかった理由。あれば録画もリプレイバッファも始めない
    platform_error: Option<RecordingError>,
}

/// 録画スレッドの入口。
pub(super) fn run(
    commands: Receiver<RecordingCommand>,
    events: Sender<RecordingEvent>,
    video_tap: VideoTap,
    audio_tap: AudioTap,
    telemetry: Arc<RecordingTelemetry>,
) {
    // 順序は COM → MF。落とすのは逆順（ローカル変数は宣言の逆順に落ちる）
    let com = ComApartment::enter(ComModel::MultiThreaded);
    let mf = com.as_ref().ok().map(|_| MfPlatform::start());
    let platform_error = match (&com, &mf) {
        (Err(e), _) | (Ok(_), Some(Err(e))) => Some(RecordingError::Platform {
            reason: e.to_string(),
        }),
        _ => None,
    };
    let mut worker = Worker {
        events,
        video_tap,
        audio_tap,
        telemetry,
        session: None,
        replay: ReplayState::Off,
        deferred: None,
        platform_error,
    };
    loop {
        let busy = worker.session.is_some() || matches!(worker.replay, ReplayState::Running(_));
        let command = if busy {
            match commands.recv_timeout(POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(RecordingCommand::Shutdown),
            }
        } else {
            // 何もしていない。コマンドが来るまで起きない
            Some(commands.recv().unwrap_or(RecordingCommand::Shutdown))
        };
        match command {
            Some(RecordingCommand::Shutdown) => {
                worker.shutdown();
                break;
            }
            Some(command) => worker.handle(command),
            None => {}
        }
        worker.tick();
    }
    drop(mf);
    drop(com);
}

impl Worker {
    fn handle(&mut self, command: RecordingCommand) {
        match command {
            RecordingCommand::Start(request) => self.start(request),
            RecordingCommand::Stop => self.stop(),
            RecordingCommand::Replay(config) => self.set_replay(config),
            RecordingCommand::SaveReplay(request) => self.save_replay(request),
            // `run` が受け取る
            RecordingCommand::Shutdown => {}
        }
    }

    /// リプレイバッファの中身だけを保存する（#438）。窓口が判定してから送ってくるが、
    /// ここでも同じ条件を見て、保存できなければ理由を返す（窓口の「保存中」を落とすため、
    /// 何もしないときも必ず返事を返す）。
    fn save_replay(&mut self, request: RecordingRequest) {
        if let Some(error) = &self.platform_error {
            let _ = self.events.send(RecordingEvent::ReplaySaveFailed {
                error: error.clone(),
                summary: None,
            });
            return;
        }
        let refusal = match &self.replay {
            // リプレイバッファを通さない録画の最中（リプレイバッファは `Pending`）
            _ if self.session.is_some() => Some(SaveReplayBlock::Recording),
            ReplayState::Running(pipeline) => match pipeline.recording_kind() {
                None => None,
                Some(RecordingKind::Recording) => Some(SaveReplayBlock::Recording),
                Some(RecordingKind::SaveReplay) => Some(SaveReplayBlock::Saving),
            },
            // OFF、またはリプレイバッファを用意できずに止まっている
            ReplayState::Off | ReplayState::Pending(_) | ReplayState::Failed(_) => {
                Some(SaveReplayBlock::ReplayOff)
            }
        };
        match (refusal, &mut self.replay) {
            (None, ReplayState::Running(pipeline)) => {
                pipeline.start_recording(request, RecordingKind::SaveReplay);
            }
            (refusal, _) => {
                let block = refusal.unwrap_or(SaveReplayBlock::ReplayOff);
                debug!("リプレイを保存しない: {:?}", block);
                let _ = self.events.send(RecordingEvent::ReplaySaveRefused(block));
            }
        }
    }

    fn start(&mut self, request: RecordingRequest) {
        if let Some(error) = &self.platform_error {
            fail(&self.events, error.clone());
            return;
        }
        let recording = self.session.is_some()
            || matches!(&self.replay, ReplayState::Running(pipeline) if pipeline.is_recording());
        if recording {
            debug!("録画中の開始要求は無視する");
            return;
        }
        match &mut self.replay {
            ReplayState::Running(pipeline) => {
                pipeline.start_recording(request, RecordingKind::Recording)
            }
            // OFF、またはリプレイバッファを用意できなかった。リプレイバッファを通さずに録る
            _ => {
                self.session = Session::begin(
                    request,
                    self.video_tap.clone(),
                    self.audio_tap.clone(),
                    Arc::clone(&self.telemetry),
                    self.events.clone(),
                );
            }
        }
    }

    fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop();
            self.after_session();
        } else if let ReplayState::Running(pipeline) = &mut self.replay {
            pipeline.stop_recording();
        }
    }

    fn set_replay(&mut self, config: Option<ReplayConfig>) {
        if let Some(error) = &self.platform_error {
            if config.is_some() {
                let _ = self
                    .events
                    .send(RecordingEvent::ReplayFailed(error.clone()));
            }
            return;
        }
        let state = std::mem::replace(&mut self.replay, ReplayState::Off);
        self.replay = match (state, config) {
            (ReplayState::Running(mut pipeline), config) if pipeline.is_recording() => {
                // 録画中はエンコーダを作り直さない。さかのぼる長さだけ先に反映し、
                // エンコーダが変わる設定と OFF は録画が終わってから反映する
                match &config {
                    Some(new) if pipeline.config().same_encoders(new) => {
                        pipeline.set_retain_seconds(new.seconds);
                        self.deferred = None;
                    }
                    Some(new) => {
                        pipeline.set_retain_seconds(new.seconds);
                        self.deferred = Some(config);
                    }
                    None => {
                        pipeline.set_retain_seconds(0);
                        self.deferred = Some(None);
                    }
                }
                ReplayState::Running(pipeline)
            }
            (ReplayState::Running(mut pipeline), Some(new))
                if pipeline.config().same_encoders(&new) =>
            {
                pipeline.set_retain_seconds(new.seconds);
                ReplayState::Running(pipeline)
            }
            // 作り直す。**前のものを落としてから**次を差し込む（`ReplayPipeline` の `Drop`）
            (ReplayState::Running(pipeline), config) => {
                drop(pipeline);
                self.replay_state_for(config)
            }
            (ReplayState::Failed(failed), Some(new)) if failed == new => {
                ReplayState::Failed(failed)
            }
            (_, config) => self.replay_state_for(config),
        };
    }

    /// 設定からリプレイバッファの状態を作る。リプレイバッファを通さない録画の間は、
    /// 差し込み口が空くまで待つ。
    fn replay_state_for(&self, config: Option<ReplayConfig>) -> ReplayState {
        match config {
            None => ReplayState::Off,
            Some(config) if self.session.is_some() => ReplayState::Pending(config),
            Some(config) => ReplayState::Running(Box::new(ReplayPipeline::start(
                config,
                self.video_tap.clone(),
                self.audio_tap.clone(),
                Arc::clone(&self.telemetry),
                self.events.clone(),
            ))),
        }
    }

    fn tick(&mut self) {
        if let Some(session) = self.session.as_mut() {
            if let Err(error) = session.tick() {
                if let Some(session) = self.session.take() {
                    session.end_with_error(error);
                }
                self.after_session();
            }
        }
        let ReplayState::Running(pipeline) = &mut self.replay else {
            return;
        };
        let was_recording = pipeline.is_recording();
        match pipeline.tick() {
            Ok(()) => {
                if was_recording && !pipeline.is_recording() {
                    self.after_replay_recording();
                }
            }
            Err(error) => {
                let state = std::mem::replace(&mut self.replay, ReplayState::Off);
                if let ReplayState::Running(pipeline) = state {
                    let config = pipeline.config().clone();
                    // 録画中ならその録画の失敗として知らせる。していなければリプレイバッファの失敗
                    if !pipeline.fail(error.clone()) {
                        let _ = self.events.send(RecordingEvent::ReplayFailed(error));
                    }
                    self.replay = ReplayState::Failed(config);
                    if let Some(deferred) = self.deferred.take() {
                        self.set_replay(deferred);
                    }
                }
            }
        }
    }

    /// リプレイバッファを通さない録画が終わった。待たせていたリプレイバッファを始める。
    fn after_session(&mut self) {
        if let ReplayState::Pending(config) = &self.replay {
            let config = config.clone();
            self.replay = self.replay_state_for(Some(config));
        }
    }

    /// リプレイバッファを通す録画が終わった。その間に変えられた設定を反映する。
    fn after_replay_recording(&mut self) {
        if let Some(config) = self.deferred.take() {
            self.set_replay(config);
        }
    }

    /// 録画を閉じ、リプレイバッファを止める。終了時。
    fn shutdown(&mut self) {
        info!("録画スレッドを止める");
        if let Some(session) = self.session.take() {
            session.stop();
        }
        if let ReplayState::Running(pipeline) = &mut self.replay {
            pipeline.finish_recording_now();
        }
        self.replay = ReplayState::Off;
    }
}
