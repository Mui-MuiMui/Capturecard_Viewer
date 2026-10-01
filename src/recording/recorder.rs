//! 録画スレッドの窓口（`Recorder`）と、UI スレッドとやり取りする型（コマンド・イベント・観測値）。
//! 録画スレッドの本体は `super::recorder_loop`。
//!
//! **窓口は UI スレッドの `CaptureCardViewer` が持つ。** 録画スレッドの寿命は
//! 「録画中、またはリプレイバッファが ON のあいだ」（`docs/design/threads.md`）。
//! 録画を始めるか、リプレイバッファを ON にしたときに 1 本起こし、どちらも無くなったら
//! 窓口が止めて join する。`JoinHandle` は捨てない（`on_exit` も同じ）。待たないと
//! `Finalize` の途中でプロセスが落ち、再生できない MP4 が残る。
//!
//! **スレッドを止めると決めるのは窓口（UI スレッド）だけ。** 録画スレッドが自分で抜けると、
//! 抜ける直前に送られたコマンドが宙に浮くため。
//!
//! 録画には 2 つの経路がある（`docs/design/recording.md` の「①②の経路と③の経路」）。
//!
//! - リプレイバッファが OFF: `super::session::Session`。Sink Writer がエンコードも行う（①②）
//! - リプレイバッファが ON: `super::replay::ReplayPipeline`。エンコーダ MFT を常に回して
//!   エンコード済みのリングに持ち、録画を始めたらリングからエンコードなしの Sink Writer へ書く
//!
//! どちらを使うかは録画スレッドが決める。窓口は録画の開始・停止とリプレイバッファの設定を
//! 送るだけで、経路を知らない。
//!
//! やり取りは mpsc。UI → 録画が `RecordingCommand`、録画 → UI が `RecordingEvent`。
//! **録画スレッドから直接 `error!` を出さない。** 失敗を画面に出せるのは UI スレッド
//! だけなので、受け取った UI スレッドがログと通知を出す（スクリーンショットの保存
//! スレッドと同じ。`docs/design/threads.md`）。録画スレッドが自分で残すのは、
//! 失敗ではない経過（使ったエンコーダ、ハードウェアからソフトウェアへ倒したこと）だけ。
//!
//! 書いた枚数などの観測値は `RecordingTelemetry`（Atomic）で共有し、UI が統計 OSD を
//! 描くときに読む。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use log::{debug, warn};

use super::audio::AudioStats;
use super::pts::{AUDIO_SAMPLE_RATE, UNITS_PER_SECOND};
use super::replay_config::ReplayConfig;
use super::{EncoderInfo, RecordingError};
use crate::audio::AudioTap;
use crate::video::VideoTap;

/// 観測値の「リプレイバッファを通していない」
const NO_REPLAY: u64 = u64::MAX;

/// 録画を始めるのに要るもの。UI スレッドが設定から組み立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingRequest {
    /// 保存先のフォルダ。無ければ作る
    pub folder: PathBuf,
    /// ファイル名（拡張子なし）。書式の検めは済んでいる（`resolve_file_stem`）
    pub file_stem: String,
    pub video_bitrate_kbps: u32,
    pub hardware_encoder: bool,
    /// 公称 fps。デバイスへ要求した fps（`ActiveVideo::requested_fps`）。
    /// 映像が無ければ `None`（60 として扱う）
    pub nominal_fps: Option<u32>,
    /// 音声（AAC）の平均ビットレート（kbps）。96 / 128 / 160 / 192 のどれかに寄せてある。
    /// `None` なら音声を録らない（映像だけの MP4）
    pub audio_bitrate_kbps: Option<u32>,
}

/// UI スレッド → 録画スレッド。
pub(super) enum RecordingCommand {
    Start(RecordingRequest),
    Stop,
    /// リプレイバッファの設定。`None` なら OFF
    Replay(Option<ReplayConfig>),
    /// 録画を閉じ、リプレイバッファを止めて抜ける
    Shutdown,
}

/// 録画を閉じたときの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSummary {
    pub path: PathBuf,
    /// 書いた映像の長さ（最後の PTS まで）
    pub duration: Duration,
    pub frames_written: u64,
    /// 録画に回せなかった枚数（リングが満杯）と、エンコーダの遅れで捨てた枚数の和
    pub frames_dropped: u64,
    /// 録画中に `FrameSink` が Vec を回収できなかった回数
    pub recycle_misses: u64,
    /// リプレイバッファからさかのぼった長さ。リプレイバッファを通していなければ `None`
    pub replay_lead: Option<Duration>,
}

/// 録画スレッド → UI スレッド。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingEvent {
    /// 保存先と空き容量を確かめ、録画を始めた。映像を待っている
    Started,
    /// 使っているエンコーダ
    EncoderSelected(EncoderInfo),
    /// 止めて `Finalize` まで終えた
    Stopped(RecordingSummary),
    /// 始められなかった、または途中で止まった。`summary` はファイルを閉じてあれば入る
    Failed {
        error: RecordingError,
        summary: Option<RecordingSummary>,
    },
    /// リプレイバッファを続けられない（エンコーダを用意できない など）。録画していないときだけ
    /// 送る。設定が変わるまで作り直さず、その間の録画はリプレイバッファを通さない経路で行う
    ReplayFailed(RecordingError),
}

/// 録画スレッドと UI スレッドで共有する観測値。**書くのは録画スレッドだけ**、UI は読むだけ。
#[derive(Debug)]
pub(super) struct RecordingTelemetry {
    pub(super) frames_written: AtomicU64,
    /// エンコーダの遅れで捨てた枚数
    pub(super) frames_skipped: AtomicU64,
    /// 差し込み口が録画に回せなかった枚数の、録画を始めたときの値。リプレイバッファは
    /// 差し込み口を録画をまたいで差したままにするので、録画中の値はここからの差で出す
    dropped_baseline: AtomicU64,
    /// 音声の起点を揃えるために足した無音（出力フレーム数、48kHz）。
    /// 音声が来ていない間に埋めた分と、止めるときに映像の終わりまで埋めた分も含む
    audio_silence_frames: AtomicU64,
    /// 音声の起点を揃えるために先頭から削った入力の長さ（100ns）
    audio_trimmed_units: AtomicU64,
    /// 音声のリングが溢れて捨てたコールバックの回数
    audio_overflows: AtomicU64,
    /// リプレイバッファからさかのぼった長さ（ms）。通していなければ `NO_REPLAY`
    replay_lead_ms: AtomicU64,
    /// リプレイバッファのリングが持っている映像の長さ（ms）
    replay_held_ms: AtomicU64,
    /// リプレイバッファのリングから古い GOP を捨てた回数
    replay_discarded_gops: AtomicU64,
    /// リプレイバッファのリングが持っているデータの大きさ（バイト、映像と音声の合計。#313）
    replay_ring_bytes: AtomicU64,
    /// リングが上限の大きさを超え、キーフレームが 1 つしか無いので空にした回数（#313）
    replay_ring_overflows: AtomicU64,
}

impl Default for RecordingTelemetry {
    fn default() -> Self {
        Self {
            frames_written: AtomicU64::new(0),
            frames_skipped: AtomicU64::new(0),
            dropped_baseline: AtomicU64::new(0),
            audio_silence_frames: AtomicU64::new(0),
            audio_trimmed_units: AtomicU64::new(0),
            audio_overflows: AtomicU64::new(0),
            replay_lead_ms: AtomicU64::new(NO_REPLAY),
            replay_held_ms: AtomicU64::new(0),
            replay_discarded_gops: AtomicU64::new(0),
            replay_ring_bytes: AtomicU64::new(0),
            replay_ring_overflows: AtomicU64::new(0),
        }
    }
}

impl RecordingTelemetry {
    /// 1 回の録画の値を 0 に戻す。`dropped_baseline` は差し込み口の捨てた枚数のいまの値。
    pub(super) fn begin_recording(&self, dropped_baseline: u64) {
        self.frames_written.store(0, Ordering::Relaxed);
        self.frames_skipped.store(0, Ordering::Relaxed);
        self.dropped_baseline
            .store(dropped_baseline, Ordering::Relaxed);
        self.publish_audio(&AudioStats::default());
        self.replay_lead_ms.store(NO_REPLAY, Ordering::Relaxed);
    }

    /// 音声の観測値を書き出す。
    pub(super) fn publish_audio(&self, stats: &AudioStats) {
        self.audio_silence_frames
            .store(stats.silence_frames, Ordering::Relaxed);
        self.audio_trimmed_units
            .store(stats.trimmed_units, Ordering::Relaxed);
        self.audio_overflows
            .store(stats.overflows, Ordering::Relaxed);
    }

    /// リプレイバッファからさかのぼった長さを書き出す（ファイルを作ったとき）。
    pub(super) fn set_replay_lead(&self, lead: Duration) {
        self.replay_lead_ms
            .store(lead.as_millis() as u64, Ordering::Relaxed);
    }

    /// リプレイバッファのリングの状態を書き出す。`held_units` は 100ns。
    pub(super) fn publish_ring(&self, held_units: i64, discarded_gops: u64) {
        let held_ms = u64::try_from(held_units).unwrap_or(0) / (UNITS_PER_SECOND as u64 / 1000);
        self.replay_held_ms.store(held_ms, Ordering::Relaxed);
        self.replay_discarded_gops
            .store(discarded_gops, Ordering::Relaxed);
    }

    /// リプレイバッファのリングの大きさ（バイト）と、上限を超えて空にした回数を書き出す。
    /// 画面には出さない（#313）。
    pub(super) fn publish_ring_size(&self, bytes: usize, overflows: u64) {
        self.replay_ring_bytes
            .store(bytes as u64, Ordering::Relaxed);
        self.replay_ring_overflows
            .store(overflows, Ordering::Relaxed);
    }
}

/// 統計 OSD に出す録画の音声の状態。音声を録らない設定なら作らない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordingAudioStats {
    /// 揃えるために足した無音の合計（ms）
    pub silence_ms: u64,
    /// 揃えるために先頭から削った入力の合計（ms）
    pub trimmed_ms: u64,
    /// リングが溢れて捨てたコールバックの回数
    pub overflows: u64,
}

/// リプレイバッファのリングの状態。ログに残す（画面には出さない。#182 の決定）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayRingStats {
    /// 持っている映像の長さ
    pub held: Duration,
    /// 古い GOP を捨てた回数
    pub discarded_gops: u64,
}

/// 録画中の UI 側の控え。
struct ActiveRecording {
    started_at: Instant,
    stop_requested: bool,
    encoder: Option<EncoderInfo>,
    /// 音声を録っているか（`RecordingRequest::audio_bitrate_kbps` が `Some`）
    audio: bool,
}

/// 動いている録画スレッド。
struct RecorderThread {
    commands: Sender<RecordingCommand>,
    events: Receiver<RecordingEvent>,
    handle: Option<JoinHandle<()>>,
    telemetry: Arc<RecordingTelemetry>,
}

impl RecorderThread {
    fn spawn(video_tap: VideoTap, audio_tap: AudioTap) -> Result<Self, RecordingError> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let telemetry = Arc::new(RecordingTelemetry::default());
        let handle = {
            let telemetry = Arc::clone(&telemetry);
            thread::Builder::new()
                .name("recorder".to_string())
                .spawn(move || {
                    super::recorder_loop::run(command_rx, event_tx, video_tap, audio_tap, telemetry)
                })
                .map_err(|e| RecordingError::Platform {
                    reason: e.to_string(),
                })?
        };
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            handle: Some(handle),
            telemetry,
        })
    }

    /// 止めるよう頼み、抜けるまで待って、届いたイベントを返す。**上限は置かない。**
    /// 置くと `Finalize` の途中でプロセスが落ち、再生できない MP4 が残る。
    fn shutdown(mut self) -> Vec<RecordingEvent> {
        // 既に抜けていれば送れないが、それで構わない
        let _ = self.commands.send(RecordingCommand::Shutdown);
        // 送り手は録画スレッドが持っていて、抜けるときに落ちる。落ちるまで受け取り続ける
        let events = self.events.iter().collect();
        self.join();
        events
    }

    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                // release は panic = "abort" なのでここには来ない
                warn!("録画スレッドがパニックした");
            }
        }
    }
}

impl Drop for RecorderThread {
    /// **スレッドを切り離さない。** 止めるよう頼んでから、抜けるまで待つ。
    fn drop(&mut self) {
        let _ = self.commands.send(RecordingCommand::Shutdown);
        self.join();
    }
}

/// 録画スレッドの窓口。UI スレッドが 1 つ持つ。
pub struct Recorder {
    video_tap: VideoTap,
    audio_tap: AudioTap,
    thread: Option<RecorderThread>,
    /// 最後に送ったリプレイバッファの設定。`None` なら OFF
    replay: Option<ReplayConfig>,
    recording: Option<ActiveRecording>,
}

impl Recorder {
    /// 窓口を作る。スレッドはまだ起こさない。
    pub fn new(video_tap: VideoTap, audio_tap: AudioTap) -> Self {
        Self {
            video_tap,
            audio_tap,
            thread: None,
            replay: None,
            recording: None,
        }
    }

    /// 録画を始める。スレッドが無ければ起こす。起こせなければ失敗。
    /// 録画中（`Finalize` を待っている間も含む）は何もしない。
    pub fn start(&mut self, request: RecordingRequest) -> Result<(), RecordingError> {
        if self.recording.is_some() {
            debug!("録画中の開始要求は無視する");
            return Ok(());
        }
        let audio = request.audio_bitrate_kbps.is_some();
        self.send(RecordingCommand::Start(request))?;
        self.recording = Some(ActiveRecording {
            started_at: Instant::now(),
            stop_requested: false,
            encoder: None,
            audio,
        });
        Ok(())
    }

    /// 停止を頼む。録画スレッドは残りを書いて `Finalize` し、`Stopped` を返す。
    pub fn request_stop(&mut self) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        if !recording.stop_requested {
            recording.stop_requested = true;
            if let Some(thread) = &self.thread {
                // 既に抜けていれば送れないが、それで構わない
                let _ = thread.commands.send(RecordingCommand::Stop);
            }
        }
    }

    /// リプレイバッファの設定を渡す。前に渡したものと同じなら何もしない。
    /// ON にしたらスレッドを起こし、その時点から溜め始める。OFF にして録画もしていなければ
    /// スレッドを止める。
    pub fn set_replay(&mut self, config: Option<ReplayConfig>) -> Result<(), RecordingError> {
        if config == self.replay {
            return Ok(());
        }
        if config.is_some() || self.thread.is_some() {
            self.send(RecordingCommand::Replay(config.clone()))?;
        }
        // 送れてから控える。送れなければ（スレッドを起こせない）次の `apply_settings` で送り直す
        self.replay = config;
        self.shutdown_if_idle();
        Ok(())
    }

    /// 録画中か（`Finalize` を待っている間も含む）。
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// 停止を頼んだあと（`Finalize` を待っている間）か。
    pub fn is_stopping(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|recording| recording.stop_requested)
    }

    /// 開始からの経過時間。録画していなければ 0。
    pub fn elapsed(&self) -> Duration {
        self.recording
            .as_ref()
            .map_or(Duration::ZERO, |recording| recording.started_at.elapsed())
    }

    /// 届いているイベントを 1 つ取り出す。待たない。録画もリプレイバッファも無くなって
    /// いれば、ここでスレッドを止める。
    pub fn try_recv(&mut self) -> Option<RecordingEvent> {
        let received = self.thread.as_ref()?.events.try_recv();
        match received {
            Ok(event) => {
                self.note(&event);
                Some(event)
            }
            Err(TryRecvError::Empty) => {
                self.shutdown_if_idle();
                None
            }
            Err(TryRecvError::Disconnected) => {
                // 自分からは抜けないので、パニック以外では来ない
                if let Some(mut thread) = self.thread.take() {
                    thread.join();
                }
                self.recording = None;
                None
            }
        }
    }

    /// 録画を止め、リプレイバッファも止めて、録画スレッドが `Finalize` を終えて抜けるまで待ち、
    /// 届いたイベントを返す。終了時（`on_exit`）に使う。
    pub fn shutdown(&mut self) -> Vec<RecordingEvent> {
        self.recording = None;
        self.replay = None;
        match self.thread.take() {
            Some(thread) => thread.shutdown(),
            None => Vec::new(),
        }
    }

    /// 使っているエンコーダ。決まるまでは `None`。
    pub fn encoder(&self) -> Option<&EncoderInfo> {
        self.recording.as_ref()?.encoder.as_ref()
    }

    /// 書いた枚数。
    pub fn frames_written(&self) -> u64 {
        self.telemetry()
            .map_or(0, |t| t.frames_written.load(Ordering::Relaxed))
    }

    /// 捨てた枚数（リングが満杯、エンコーダの遅れ）。
    pub fn frames_dropped(&self) -> u64 {
        let Some(telemetry) = self.telemetry() else {
            return 0;
        };
        let baseline = telemetry.dropped_baseline.load(Ordering::Relaxed);
        self.video_tap.dropped().saturating_sub(baseline)
            + telemetry.frames_skipped.load(Ordering::Relaxed)
    }

    /// 音声の状態（足した無音・削った入力・リングの溢れ）。音声を録らない設定なら `None`。
    pub fn audio_stats(&self) -> Option<RecordingAudioStats> {
        if !self.recording.as_ref()?.audio {
            return None;
        }
        let telemetry = self.telemetry()?;
        Some(RecordingAudioStats {
            silence_ms: audio_frames_to_ms(telemetry.audio_silence_frames.load(Ordering::Relaxed)),
            trimmed_ms: telemetry.audio_trimmed_units.load(Ordering::Relaxed)
                / (UNITS_PER_SECOND as u64 / 1000),
            overflows: telemetry.audio_overflows.load(Ordering::Relaxed),
        })
    }

    /// リプレイバッファからさかのぼった長さ。録画中でリプレイバッファを通していて、
    /// 先頭が決まったときだけ。
    pub fn replay_lead(&self) -> Option<Duration> {
        self.recording.as_ref()?;
        let lead = self.telemetry()?.replay_lead_ms.load(Ordering::Relaxed);
        (lead != NO_REPLAY).then(|| Duration::from_millis(lead))
    }

    /// リプレイバッファのリングの状態。リプレイバッファが OFF なら `None`。
    pub fn replay_ring(&self) -> Option<ReplayRingStats> {
        self.replay.as_ref()?;
        let telemetry = self.telemetry()?;
        Some(ReplayRingStats {
            held: Duration::from_millis(telemetry.replay_held_ms.load(Ordering::Relaxed)),
            discarded_gops: telemetry.replay_discarded_gops.load(Ordering::Relaxed),
        })
    }

    fn telemetry(&self) -> Option<&RecordingTelemetry> {
        self.thread.as_ref().map(|thread| thread.telemetry.as_ref())
    }

    /// コマンドを送る。スレッドが無ければ起こす。
    fn send(&mut self, command: RecordingCommand) -> Result<(), RecordingError> {
        if self.thread.is_none() {
            self.thread = Some(RecorderThread::spawn(
                self.video_tap.clone(),
                self.audio_tap.clone(),
            )?);
        }
        match &self.thread {
            Some(thread) => thread
                .commands
                .send(command)
                .map_err(|_| RecordingError::ThreadStopped),
            None => Ok(()),
        }
    }

    /// 届いたイベントを控えに反映する。
    fn note(&mut self, event: &RecordingEvent) {
        match event {
            RecordingEvent::EncoderSelected(info) => {
                if let Some(recording) = self.recording.as_mut() {
                    recording.encoder = Some(info.clone());
                }
            }
            RecordingEvent::Stopped(_) | RecordingEvent::Failed { .. } => self.recording = None,
            RecordingEvent::Started | RecordingEvent::ReplayFailed(_) => {}
        }
    }

    /// 録画もリプレイバッファも無ければスレッドを止める。止まるまで待つが、
    /// 何もしていないスレッドなのですぐ返る。
    fn shutdown_if_idle(&mut self) {
        if self.recording.is_some() || self.replay.is_some() {
            return;
        }
        if let Some(thread) = self.thread.take() {
            for event in thread.shutdown() {
                // 録画は無いので、届くのはリプレイバッファの失敗くらい。もう使わない
                debug!(
                    "止めた録画スレッドから届いていたイベントを捨てる: {:?}",
                    event
                );
            }
        }
    }
}

/// 出力フレーム数（48kHz）を ms に直す。
fn audio_frames_to_ms(frames: u64) -> u64 {
    frames.saturating_mul(1000) / u64::from(AUDIO_SAMPLE_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frames_to_ms_converts_48k_frames() {
        assert_eq!(audio_frames_to_ms(0), 0);
        assert_eq!(audio_frames_to_ms(48), 1);
        // 1ms に満たない端数は切り捨てる
        assert_eq!(audio_frames_to_ms(47), 0);
        assert_eq!(audio_frames_to_ms(48_000 * 3), 3_000);
    }

    #[test]
    fn recording_telemetry_publish_ring_converts_to_ms() {
        let telemetry = RecordingTelemetry::default();
        telemetry.publish_ring(25 * UNITS_PER_SECOND + 5_000, 7);
        assert_eq!(telemetry.replay_held_ms.load(Ordering::Relaxed), 25_000);
        assert_eq!(telemetry.replay_discarded_gops.load(Ordering::Relaxed), 7);
        // 負の長さは 0
        telemetry.publish_ring(-1, 0);
        assert_eq!(telemetry.replay_held_ms.load(Ordering::Relaxed), 0);
        telemetry.publish_ring_size(1_234, 2);
        assert_eq!(telemetry.replay_ring_bytes.load(Ordering::Relaxed), 1_234);
        assert_eq!(telemetry.replay_ring_overflows.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn recording_telemetry_begin_recording_clears_the_previous_recording() {
        let telemetry = RecordingTelemetry::default();
        telemetry.frames_written.store(10, Ordering::Relaxed);
        telemetry.set_replay_lead(Duration::from_secs(20));
        telemetry.begin_recording(5);
        assert_eq!(telemetry.frames_written.load(Ordering::Relaxed), 0);
        assert_eq!(telemetry.dropped_baseline.load(Ordering::Relaxed), 5);
        assert_eq!(telemetry.replay_lead_ms.load(Ordering::Relaxed), NO_REPLAY);
    }

    #[test]
    fn recorder_without_replay_or_recording_has_no_thread() {
        let mut recorder = Recorder::new(VideoTap::new(), AudioTap::new());
        assert!(recorder.set_replay(None).is_ok());
        assert!(recorder.thread.is_none());
        assert!(!recorder.is_recording());
        assert_eq!(recorder.replay_lead(), None);
        assert_eq!(recorder.replay_ring(), None);
        assert!(recorder.try_recv().is_none());
    }

    /// フェイクの映像（720p60 のカラーバーにフレーム番号を焼き込んだもの）と音声（正弦波）を流す。
    fn start_fakes(
        frames: &crate::video::VideoFrames,
        audio_tap: &AudioTap,
    ) -> (
        crate::video::FakeVideoCapture,
        crate::audio::FakeAudioCapture,
    ) {
        use crate::audio::{AudioControls, FakeAudioCapture, FakeAudioOptions, PassthroughRequest};
        use crate::repaint::RepaintWaker;
        use crate::video::{FakeVideoCapture, FakeVideoOptions, SharedColorConversion};
        let mut video = FakeVideoCapture::new(
            frames.clone(),
            Arc::new(SharedColorConversion::new()),
            RepaintWaker::new(),
            FakeVideoOptions {
                device_count: 1,
                disconnect_after: None,
                failures_before_success: 0,
            },
        );
        video
            .start_capture(
                Some("Fake Camera 1"),
                Some((1280, 720)),
                Some("YUY2"),
                Some(60),
            )
            .expect("フェイクの映像を開ける");
        let mut audio = FakeAudioCapture::new(
            Arc::new(AudioControls::default()),
            audio_tap.clone(),
            FakeAudioOptions {
                input_count: 1,
                failures_before_success: 0,
                stream_error_after: None,
            },
        );
        audio
            .start_passthrough(&PassthroughRequest {
                input: crate::audio::PassthroughInput::Device(Some("Fake Audio Input 1")),
                output_device_name: Some("Fake Audio Output 1"),
                sample_rate: None,
                channels: None,
                input_capabilities: None,
                output_capabilities: None,
                buffer_ms: 50,
            })
            .expect("フェイクの音声を開ける");
        (video, audio)
    }

    /// 届いたイベントを集めながら `duration` だけ待つ。
    fn poll_for(recorder: &mut Recorder, duration: Duration) -> Vec<RecordingEvent> {
        let until = Instant::now() + duration;
        let mut events = Vec::new();
        while Instant::now() < until {
            events.extend(std::iter::from_fn(|| recorder.try_recv()));
            thread::sleep(Duration::from_millis(50));
        }
        events
    }

    /// フェイクを流してリプレイバッファを `seconds` 秒で ON にし（`None` なら OFF のまま）、
    /// `wait` 待ってから `record` だけ録画して止める。保存した結果と、読み戻した長さ（100ns）を返す。
    fn record_with_replay(
        seconds: Option<u32>,
        wait: Duration,
        record: Duration,
    ) -> (RecordingSummary, i64) {
        use crate::com::{ComApartment, ComModel, MfPlatform};
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{
            MFCreateSourceReaderFromURL, MF_PD_DURATION, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
        };

        let frames = crate::video::VideoFrames::new();
        let audio_tap = AudioTap::new();
        let (mut video, mut audio) = start_fakes(&frames, &audio_tap);
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れること");
        let mut recorder = Recorder::new(frames.tap(), audio_tap);
        recorder
            .set_replay(seconds.map(|seconds| ReplayConfig {
                seconds,
                video_bitrate_kbps: 4000,
                hardware_encoder: false,
                audio_bitrate_kbps: Some(160),
                nominal_fps: Some(60),
            }))
            .expect("リプレイバッファを始められる");
        // OFF のままならスレッドは起きない（OFF のときの負荷は①②と同じ）
        assert_eq!(recorder.thread.is_some(), seconds.is_some());
        let events = poll_for(&mut recorder, wait);
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, RecordingEvent::ReplayFailed(_))),
            "{events:?}"
        );

        recorder
            .start(RecordingRequest {
                folder: dir.path().to_path_buf(),
                file_stem: "replay".to_string(),
                video_bitrate_kbps: 4000,
                hardware_encoder: false,
                nominal_fps: Some(60),
                audio_bitrate_kbps: Some(160),
            })
            .expect("録画を始められる");
        poll_for(&mut recorder, record);
        let lead = recorder.replay_lead();
        assert_eq!(lead.is_some(), seconds.is_some());
        recorder.request_stop();
        let events = poll_for(&mut recorder, Duration::from_secs(3));
        let summary = events
            .iter()
            .find_map(|event| match event {
                RecordingEvent::Stopped(summary) => Some(summary.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("保存できた: {events:?}"));
        // 統計 OSD の値は ms で丸めてある
        assert_eq!(
            summary.replay_lead.map(|lead| lead.as_millis()),
            lead.map(|lead| lead.as_millis())
        );
        // リプレイバッファが ON のままなら、止めてもスレッドは動き続ける。OFF なら止まる
        poll_for(&mut recorder, Duration::from_millis(200));
        assert_eq!(recorder.thread.is_some(), seconds.is_some());
        recorder.shutdown();
        video.stop_capture();
        audio.stop_capture();

        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let reader =
            unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(summary.path.as_path()), None) }
                .expect("書いた MP4 を開ける");
        let value = unsafe {
            reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        }
        .expect("長さを読める");
        let duration = unsafe { value.Anonymous.Anonymous.Anonymous.uhVal } as i64;
        assert!(unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
        }
        .is_ok());
        // 先頭の映像のサンプルは 0 から始まる（最初から再生できる）
        let (mut flags, mut time, mut sample) = (0u32, -1i64, None);
        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )
        }
        .expect("読める");
        assert!(sample.is_some());
        // リングからの書き出しは先頭のキーフレームを 0 にする。①②の経路は録画の開始から
        // 最初のフレームが届くまでの分（1 枚ぶん程度）だけ後ろから始まる
        let limit = if seconds.is_some() { 10_000 } else { 1_000_000 };
        assert!(time.abs() < limit, "先頭の時刻 {time}");
        (summary, duration)
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要。30 秒ほどかかる"]
    fn recorder_replay_buffer_prepends_the_seconds_before_the_start() {
        // 実行: cargo test -- --ignored recorder_replay_buffer_prepends_the_seconds_before_the_start
        //
        // 30 秒で ON にして 20 秒待ち、5 秒録画する。溜まっているのは 20 秒ぶんなので、
        // ファイルは 20 秒 + 5 秒 ≒ 25 秒（先頭はフェイクの最初のキーフレーム）
        let (summary, duration) =
            record_with_replay(Some(30), Duration::from_secs(20), Duration::from_secs(5));
        let lead = summary.replay_lead.expect("さかのぼった");
        assert!(lead > Duration::from_secs(18), "さかのぼり {lead:?}");
        assert!(
            (230_000_000..=270_000_000).contains(&duration),
            "長さ {duration}（{summary:?}）"
        );
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要。20 秒ほどかかる"]
    fn recorder_replay_buffer_keeps_only_the_configured_seconds() {
        // 実行: cargo test -- --ignored recorder_replay_buffer_keeps_only_the_configured_seconds
        //
        // 5 秒で ON にして 12 秒待ち、3 秒録画する。さかのぼるのは「いま − 5 秒」以降の
        // 最初のキーフレームからなので 3〜5 秒（キーフレームは 2 秒ごと）
        let (summary, duration) =
            record_with_replay(Some(5), Duration::from_secs(12), Duration::from_secs(3));
        let lead = summary.replay_lead.expect("さかのぼった");
        assert!(
            (Duration::from_secs(3)..=Duration::from_millis(5_200)).contains(&lead),
            "さかのぼり {lead:?}"
        );
        assert!(
            (55_000_000..=85_000_000).contains(&duration),
            "長さ {duration}（{summary:?}）"
        );
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要。10 秒ほどかかる"]
    fn recorder_without_replay_records_only_after_the_start() {
        // 実行: cargo test -- --ignored recorder_without_replay_records_only_after_the_start
        //
        // リプレイバッファが OFF なら①②の経路（Sink Writer がエンコードも行う）で録る。
        // 開始前の 5 秒は入らず、ファイルは録画した 3 秒ぶん
        let (summary, duration) =
            record_with_replay(None, Duration::from_secs(5), Duration::from_secs(3));
        assert_eq!(summary.replay_lead, None);
        assert!(
            (25_000_000..=40_000_000).contains(&duration),
            "長さ {duration}（{summary:?}）"
        );
    }
}
