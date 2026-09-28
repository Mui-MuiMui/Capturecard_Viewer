//! 録画スレッドの窓口（`Recorder`）と、録画スレッドの本体。
//!
//! **窓口は UI スレッドの `CaptureCardViewer` が持つ。** 開始で録画スレッドを 1 本起こし、
//! 停止で `Finalize` まで終えたらスレッドは自分で抜ける。`JoinHandle` は捨てず、
//! `Recorder` を落とすときに join する（`on_exit` も同じ）。待たないと `Finalize` の
//! 途中でプロセスが落ち、再生できない MP4 が残る。
//!
//! やり取りは mpsc。UI → 録画が `RecordingCommand`、録画 → UI が `RecordingEvent`。
//! **録画スレッドから直接 `error!` を出さない。** 失敗を画面に出せるのは UI スレッド
//! だけなので、受け取った UI スレッドがログと通知を出す（スクリーンショットの保存
//! スレッドと同じ。`docs/design/threads.md`）。録画スレッドが自分で残すのは、
//! 失敗ではない経過（使ったエンコーダ、ハードウェアからソフトウェアへ倒したこと）だけ。
//!
//! 書いた枚数などの観測値は `RecordingTelemetry`（Atomic）で共有し、UI が統計 OSD を
//! 描くときに読む。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use super::audio::{AudioChunk, AudioTrack};
use super::convert::{even_size, rgb_to_nv12, Nv12Matrix};
use super::file_name::unique_path;
use super::pts::{units_from, PtsClock, AUDIO_SAMPLE_RATE};
use super::storage::{free_bytes, is_low, megabytes, DISK_CHECK_INTERVAL};
use super::writer::{SinkWriter, WriterError, WriterParams, WriterStage};
use super::{EncoderInfo, RecordingError};
use crate::audio::AudioTap;
use crate::com::{ComApartment, ComModel, MfPlatform};
use crate::video::{VideoFrame, VideoTap, VideoTapConsumer, VIDEO_TAP_CAPACITY};

/// コマンドを待つ間隔。コマンドとリングの両方を見るため、数 ms で起きてリングを空にする。
///
/// リングの `Arc` が `FrameSink` の Vec の回収を妨げないよう、取り出しは速いほうがよい
/// （`FrameSink` は 2 世代前の Vec を回収するので、60fps なら 33ms の猶予がある）。
/// `thread::sleep` では待たない。
const POLL_INTERVAL: Duration = Duration::from_millis(4);

/// エンコーダが受け取ってまだエンコードしていない枚数がこれを超えたら、NV12 へ直す前に捨てる。
/// スロットリングを切ってあるので、放っておくとエンコーダの遅れの分だけメモリが溜まる。
const MAX_ENCODER_BACKLOG: u64 = 30;

/// 映像が無いときに使う公称 fps
const FALLBACK_FPS: u32 = 60;

/// 音声を Sink Writer へ渡す最小の長さ（出力フレーム数、約 21ms）。数 ms ごとの小さな
/// 塊で `WriteSample` を増やさないため。止めるときは残りをまとめて渡す
const MIN_AUDIO_CHUNK_FRAMES: usize = 1024;

/// Sink Writer を作る前（最初の映像のフレームが届く前）に溜めておく音声の上限
/// （出力フレーム数、5 秒）。超えたら古いものから捨てる。塊は PTS を持っているので、
/// 先頭を捨てても後ろの時刻はずれない
const MAX_PENDING_AUDIO_FRAMES: usize = AUDIO_SAMPLE_RATE as usize * 5;

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
enum RecordingCommand {
    Start(RecordingRequest),
    Stop,
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
}

/// 録画スレッド → UI スレッド。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingEvent {
    /// 保存先と空き容量を確かめ、リングを差し込んだ。映像を待っている
    Started,
    /// 最初のフレームで Sink Writer を作った。使っているエンコーダ
    EncoderSelected(EncoderInfo),
    /// 止めて `Finalize` まで終えた
    Stopped(RecordingSummary),
    /// 始められなかった、または途中で止まった。`summary` はファイルを閉じてあれば入る
    Failed {
        error: RecordingError,
        summary: Option<RecordingSummary>,
    },
}

/// 録画スレッドと UI スレッドで共有する観測値。
#[derive(Debug, Default)]
struct RecordingTelemetry {
    frames_written: AtomicU64,
    /// エンコーダの遅れで捨てた枚数
    frames_skipped: AtomicU64,
}

/// 録画スレッドの窓口。UI スレッドが持つ。
pub struct Recorder {
    commands: Sender<RecordingCommand>,
    events: Receiver<RecordingEvent>,
    thread: Option<JoinHandle<()>>,
    telemetry: Arc<RecordingTelemetry>,
    tap: VideoTap,
    started_at: Instant,
    stop_requested: bool,
    encoder: Option<EncoderInfo>,
}

impl Recorder {
    /// 録画スレッドを起こして録画を始める。スレッドを起こせなければ失敗。
    /// `audio_tap` は音声の差し込み口。`request.audio_bitrate_kbps` が `None` なら差し込まない。
    pub fn start(
        request: RecordingRequest,
        tap: VideoTap,
        audio_tap: AudioTap,
    ) -> Result<Self, RecordingError> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let telemetry = Arc::new(RecordingTelemetry::default());
        let thread = {
            let tap = tap.clone();
            let telemetry = Arc::clone(&telemetry);
            thread::Builder::new()
                .name("recorder".to_string())
                .spawn(move || run(command_rx, event_tx, tap, audio_tap, telemetry))
                .map_err(|e| RecordingError::Platform {
                    reason: e.to_string(),
                })?
        };
        // 受け手はいま起こしたスレッドなので、送れないことは無い
        let _ = command_tx.send(RecordingCommand::Start(request));
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
            telemetry,
            tap,
            started_at: Instant::now(),
            stop_requested: false,
            encoder: None,
        })
    }

    /// 停止を頼む。録画スレッドは残りを書いて `Finalize` し、`Stopped` を返して終わる。
    pub fn request_stop(&mut self) {
        if !self.stop_requested {
            self.stop_requested = true;
            // スレッドが既に終わっていれば送れないが、それで構わない
            let _ = self.commands.send(RecordingCommand::Stop);
        }
    }

    /// 停止を頼んだあと（`Finalize` を待っている間）か。
    pub fn is_stopping(&self) -> bool {
        self.stop_requested
    }

    /// 開始からの経過時間。
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// 届いているイベントを 1 つ取り出す。待たない。
    pub fn try_recv(&mut self) -> Option<RecordingEvent> {
        let event = self.events.try_recv().ok()?;
        if let RecordingEvent::EncoderSelected(info) = &event {
            self.encoder = Some(info.clone());
        }
        Some(event)
    }

    /// 停止を頼み、録画スレッドが `Finalize` を終えて抜けるまで待ち、届いたイベントを返す。
    /// 終了時（`on_exit`）に使う。**上限は置かない。** 置くと `Finalize` の途中で
    /// プロセスが落ち、再生できない MP4 が残る。
    pub fn stop_and_wait(mut self) -> Vec<RecordingEvent> {
        self.request_stop();
        // 送り手は録画スレッドが持っていて、抜けるときに落ちる。落ちるまで受け取り続ける
        let events: Vec<RecordingEvent> = self.events.iter().collect();
        // ここで `Drop` が join する
        events
    }

    /// 使っているエンコーダ。Sink Writer を作るまで（最初のフレームが届くまで）は `None`。
    pub fn encoder(&self) -> Option<&EncoderInfo> {
        self.encoder.as_ref()
    }

    /// 書いた枚数。
    pub fn frames_written(&self) -> u64 {
        self.telemetry.frames_written.load(Ordering::Relaxed)
    }

    /// 捨てた枚数（リングが満杯、エンコーダの遅れ）。
    pub fn frames_dropped(&self) -> u64 {
        self.tap.dropped() + self.telemetry.frames_skipped.load(Ordering::Relaxed)
    }
}

impl Drop for Recorder {
    /// **スレッドを切り離さない。** 停止を頼んでから、`Finalize` が終わるまで待つ。
    fn drop(&mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                // release は panic = "abort" なのでここには来ない
                warn!("録画スレッドがパニックした");
            }
        }
    }
}

/// 録画スレッドの入口。
fn run(
    commands: Receiver<RecordingCommand>,
    events: Sender<RecordingEvent>,
    tap: VideoTap,
    audio_tap: AudioTap,
    telemetry: Arc<RecordingTelemetry>,
) {
    let request = match commands.recv() {
        Ok(RecordingCommand::Start(request)) => request,
        // 窓口が先に落ちた
        Ok(RecordingCommand::Stop) | Err(_) => return,
    };
    // 順序は COM → MF。落とすのは逆順（ローカル変数は宣言の逆順に落ちる）
    let _com = match ComApartment::enter(ComModel::MultiThreaded) {
        Ok(com) => com,
        Err(e) => {
            fail(&events, platform_error(e));
            return;
        }
    };
    let _mf = match MfPlatform::start() {
        Ok(mf) => mf,
        Err(e) => {
            fail(&events, platform_error(e));
            return;
        }
    };
    Session::new(request, tap, audio_tap, telemetry, events).run(&commands);
}

fn platform_error(error: windows::core::Error) -> RecordingError {
    RecordingError::Platform {
        reason: error.to_string(),
    }
}

fn fail(events: &Sender<RecordingEvent>, error: RecordingError) {
    let _ = events.send(RecordingEvent::Failed {
        error,
        summary: None,
    });
}

/// 録画を閉じた結果。
enum Finished {
    /// 1 枚も書いていない（ファイルは作っていない）
    NoFile,
    /// `Finalize` まで済んだ
    Saved(RecordingSummary),
    /// `Finalize` に失敗した。ファイルは残してある
    FinalizeFailed(RecordingError, Option<RecordingSummary>),
}

/// 1 回の録画。録画スレッドの中だけにある。
struct Session {
    request: RecordingRequest,
    tap: VideoTap,
    audio_tap: AudioTap,
    /// 音声トラック。音声を録らない設定なら `None`
    audio: Option<AudioTrack>,
    /// Sink Writer へまだ渡していない音声（Sink Writer を作る前の分）
    audio_pending: VecDeque<AudioChunk>,
    audio_pending_frames: usize,
    telemetry: Arc<RecordingTelemetry>,
    events: Sender<RecordingEvent>,
    consumer: Option<VideoTapConsumer>,
    clock: PtsClock,
    writer: Option<SinkWriter>,
    path: Option<PathBuf>,
    /// NV12 の変換先。使い回す（録画スレッドは確保してよいが、毎フレーム確保し直す理由も無い）
    nv12: Vec<u8>,
    frames_skipped: u64,
    last_disk_check: Instant,
}

impl Session {
    fn new(
        request: RecordingRequest,
        tap: VideoTap,
        audio_tap: AudioTap,
        telemetry: Arc<RecordingTelemetry>,
        events: Sender<RecordingEvent>,
    ) -> Self {
        let fps = request.nominal_fps.unwrap_or(FALLBACK_FPS);
        let now = Instant::now();
        Self {
            request,
            tap,
            audio_tap,
            audio: None,
            audio_pending: VecDeque::new(),
            audio_pending_frames: 0,
            telemetry,
            events,
            consumer: None,
            clock: PtsClock::new(now, fps),
            writer: None,
            path: None,
            nv12: Vec::new(),
            frames_skipped: 0,
            last_disk_check: now,
        }
    }

    fn fps(&self) -> u32 {
        self.request.nominal_fps.unwrap_or(FALLBACK_FPS).max(1)
    }

    fn run(mut self, commands: &Receiver<RecordingCommand>) {
        if let Err(error) = self.prepare() {
            fail(&self.events, error);
            return;
        }
        // リングを差し込んだ時刻が PTS の基準 t0
        self.consumer = Some(self.tap.attach(VIDEO_TAP_CAPACITY));
        let t0 = Instant::now();
        self.clock = PtsClock::new(t0, self.fps());
        self.last_disk_check = t0;
        // 音声も同じ t0 を基準にする。差し込むのは t0 の直後なので、t0 より前に
        // 届いたサンプルはリングに入らない（コールバック 1 回ぶんの端数は削る）
        if self.request.audio_bitrate_kbps.is_some() {
            self.audio = Some(AudioTrack::attach(self.audio_tap.clone(), t0));
        }
        info!(
            "録画を始めた（保存先: {}、ファイル名: {}.mp4、{}kbps、ハードウェアエンコーダ: {}、音声: {}）",
            self.request.folder.display(),
            self.request.file_stem,
            self.request.video_bitrate_kbps,
            if self.request.hardware_encoder {
                "使う"
            } else {
                "使わない"
            },
            match self.request.audio_bitrate_kbps {
                Some(kbps) => format!("AAC {kbps}kbps"),
                None => "録らない".to_string(),
            }
        );
        let _ = self.events.send(RecordingEvent::Started);

        loop {
            match commands.recv_timeout(POLL_INTERVAL) {
                Ok(RecordingCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(RecordingCommand::Start(_)) => debug!("録画中の開始要求は無視する"),
                Err(RecvTimeoutError::Timeout) => {}
            }
            // 映像を先に書く。Sink Writer は最初の映像のフレームで作るので、
            // 音声はそれまで溜めておき、作られたあとで渡す
            if let Err(error) = self
                .drain()
                .and_then(|()| self.pump_audio(MIN_AUDIO_CHUNK_FRAMES))
            {
                self.end_with_error(error);
                return;
            }
            if self.last_disk_check.elapsed() >= DISK_CHECK_INTERVAL {
                self.last_disk_check = Instant::now();
                let free = free_bytes(&self.request.folder);
                if is_low(free) {
                    let free_mb = free.map(megabytes).unwrap_or(0);
                    self.end_with_error(RecordingError::DiskLow { free_mb });
                    return;
                }
            }
        }

        // 止める。リングを抜いてから、残っている分を書き切る
        self.tap.detach();
        if let Some(audio) = &self.audio {
            audio.detach();
        }
        if let Err(error) = self.drain().and_then(|()| self.finish_audio()) {
            self.end_with_error(error);
            return;
        }
        match self.finish() {
            Finished::Saved(summary) => {
                let _ = self.events.send(RecordingEvent::Stopped(summary));
            }
            // 1 枚も届かなかった。ファイルは作っていない
            Finished::NoFile => fail(&self.events, RecordingError::NoVideo),
            Finished::FinalizeFailed(error, summary) => {
                let _ = self.events.send(RecordingEvent::Failed { error, summary });
            }
        }
    }

    /// 保存先を作り、空き容量を確かめる。
    fn prepare(&self) -> Result<(), RecordingError> {
        check_folder(&self.request.folder)?;
        std::fs::create_dir_all(&self.request.folder).map_err(|e| RecordingError::Folder {
            path: self.request.folder.clone(),
            reason: e.to_string(),
        })?;
        let free = free_bytes(&self.request.folder);
        if is_low(free) {
            return Err(RecordingError::DiskLow {
                free_mb: free.map(megabytes).unwrap_or(0),
            });
        }
        Ok(())
    }

    /// リングに溜まっている分を書く。
    fn drain(&mut self) -> Result<(), RecordingError> {
        while let Some((frame, received_at)) = self.consumer.as_mut().and_then(|c| c.pop()) {
            self.process(frame, received_at)?;
        }
        Ok(())
    }

    /// 1 枚を処理する。**`Arc` は NV12 へ直したらすぐ手放し、手放してから書く**
    /// （`FrameSink` の Vec の回収を妨げないため）。
    fn process(
        &mut self,
        frame: Arc<VideoFrame>,
        received_at: Instant,
    ) -> Result<(), RecordingError> {
        let Some(pts) = self.clock.pts_for(received_at) else {
            // 録画を始める前に受け取ったフレーム
            return Ok(());
        };
        let (width, height) = even_size(frame.width, frame.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let size = (width as u32, height as u32);

        match &self.writer {
            None => self.open_writer(size)?,
            Some(writer) if writer.size() != size => {
                return Err(RecordingError::SizeChanged {
                    from: writer.size(),
                    to: size,
                });
            }
            Some(writer) if writer.backlog() > MAX_ENCODER_BACKLOG => {
                // エンコーダが追いつかない。表示は落とさず、録画だけがコマ落ちする
                self.frames_skipped += 1;
                self.telemetry
                    .frames_skipped
                    .store(self.frames_skipped, Ordering::Relaxed);
                return Ok(());
            }
            Some(_) => {}
        }

        let matrix = Nv12Matrix::for_size(width, height);
        let converted = rgb_to_nv12(
            &frame.data,
            frame.width,
            frame.height,
            width,
            height,
            matrix,
            &mut self.nv12,
        );
        drop(frame);
        if !converted {
            // 画素が足りないフレーム。`FrameSink` が作るフレームでは起きない
            return Ok(());
        }
        self.write_current(pts)
    }

    /// 変換済みの NV12 を書く。最初の 1 枚をハードウェアで書けなければ、
    /// ファイルを消してソフトウェアで作り直し、同じ 1 枚を書き直す。
    fn write_current(&mut self, pts: i64) -> Result<(), RecordingError> {
        let duration = self.clock.sample_duration();
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let result = writer.write_nv12(&self.nv12, pts, duration);
        let first_sample = writer.samples_written() == 0;
        let hardware = writer.hardware();
        match result {
            Ok(()) => {}
            Err(error) if first_sample && hardware => {
                warn!(
                    "ハードウェアのエンコーダで最初のフレームを書けないので、ソフトウェアで作り直す: {}",
                    error
                );
                let size = writer.size();
                self.discard_writer();
                self.create_writer(size, false)?;
                let writer =
                    self.writer
                        .as_mut()
                        .ok_or_else(|| RecordingError::EncoderUnavailable {
                            reason: error.to_string(),
                        })?;
                writer
                    .write_nv12(&self.nv12, pts, duration)
                    .map_err(|e| write_error(&e))?;
            }
            Err(error) => return Err(write_error(&error)),
        }
        self.telemetry
            .frames_written
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 最初のフレームの大きさで Sink Writer を作る。ハードウェアで作れなければ
    /// ソフトウェアで作り直す。
    fn open_writer(&mut self, size: (u32, u32)) -> Result<(), RecordingError> {
        if self.request.hardware_encoder {
            match self.create_writer(size, true) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    warn!(
                        "ハードウェアのエンコーダで Sink Writer を作れないので、ソフトウェアで作り直す: {}",
                        error
                    );
                }
            }
        }
        self.create_writer(size, false)
    }

    /// Sink Writer を作る。失敗したら作りかけのファイルを消す。
    fn create_writer(&mut self, size: (u32, u32), hardware: bool) -> Result<(), RecordingError> {
        let path = unique_path(&self.request.folder, &self.request.file_stem);
        let params = WriterParams {
            width: size.0,
            height: size.1,
            fps: self.fps(),
            bitrate_kbps: self.request.video_bitrate_kbps,
            hardware,
            audio_bitrate_kbps: self.request.audio_bitrate_kbps,
        };
        match SinkWriter::create(&path, params) {
            Ok(writer) => {
                let encoder = writer.encoder_info();
                info!(
                    "録画のファイルを作った: {}（{}x{}、{}fps、{}kbps、エンコーダ: {}、ハードウェア: {}）",
                    path.display(),
                    size.0,
                    size.1,
                    params.fps,
                    params.bitrate_kbps,
                    encoder.name.as_deref().unwrap_or("（名前を取得できない）"),
                    match encoder.hardware {
                        Some(true) => "はい",
                        Some(false) => "いいえ",
                        None => "不明",
                    }
                );
                let _ = self.events.send(RecordingEvent::EncoderSelected(encoder));
                self.writer = Some(writer);
                self.path = Some(path);
                Ok(())
            }
            Err(error) => {
                remove_partial_file(&path);
                Err(create_error(&self.request.folder, &error))
            }
        }
    }

    /// 書いたものを捨てる（ハードウェアからソフトウェアへ作り直すとき）。
    fn discard_writer(&mut self) {
        self.writer = None;
        if let Some(path) = self.path.take() {
            remove_partial_file(&path);
        }
    }

    /// 閉じる。
    fn finish(&mut self) -> Finished {
        let Some(writer) = self.writer.take() else {
            return Finished::NoFile;
        };
        let summary = self.summary();
        let finalized = writer.finalize();
        if let Some(audio) = &self.audio {
            audio.stats().log(self.clock.duration());
        }
        match (finalized, summary) {
            (Ok(()), Some(summary)) => Finished::Saved(summary),
            // Sink Writer はあるのにパスが無いことは無い
            (Ok(()), None) => Finished::NoFile,
            // ファイルは消さない（再生できないかもしれないことを伝える）
            (Err(error), summary) => Finished::FinalizeFailed(write_error(&error), summary),
        }
    }

    /// 音声トラックのリングから取り出して PCM にし、溜まった分を Sink Writer へ渡す。
    /// `min_frames` に満たない分は次の呼び出しへ回す。音声を録らない設定なら何もしない。
    fn pump_audio(&mut self, min_frames: usize) -> Result<(), RecordingError> {
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        audio.pump(Instant::now());
        let chunk = audio.take_chunk(min_frames);
        if let Some(chunk) = chunk {
            self.queue_audio(chunk);
        }
        self.write_pending_audio()
    }

    /// 止めるときの音声。残りを取り出し、映像より短ければ映像の終わりまで無音で埋めて
    /// すべて渡す（音声トラックの長さを映像と揃えるため）。
    fn finish_audio(&mut self) -> Result<(), RecordingError> {
        let video_end = units_from(self.clock.duration());
        let has_writer = self.writer.is_some();
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        audio.pump(Instant::now());
        if has_writer {
            audio.finish(video_end);
        }
        let chunk = audio.take_chunk(0);
        if let Some(chunk) = chunk {
            self.queue_audio(chunk);
        }
        self.write_pending_audio()
    }

    /// 渡す音声を列へ積む。Sink Writer を作る前は溜め、上限を超えたら古いものから捨てる。
    fn queue_audio(&mut self, chunk: AudioChunk) {
        self.audio_pending_frames += chunk.frames();
        self.audio_pending.push_back(chunk);
        while self.audio_pending_frames > MAX_PENDING_AUDIO_FRAMES {
            match self.audio_pending.pop_front() {
                Some(dropped) => self.audio_pending_frames -= dropped.frames(),
                None => break,
            }
        }
    }

    /// 溜めた音声を Sink Writer へ書く。Sink Writer がまだ無ければ溜めたままにする。
    fn write_pending_audio(&mut self) -> Result<(), RecordingError> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        while let Some(chunk) = self.audio_pending.pop_front() {
            self.audio_pending_frames -= chunk.frames();
            writer
                .write_pcm(&chunk.samples, chunk.pts, chunk.duration)
                .map_err(|e| write_error(&e))?;
        }
        Ok(())
    }

    /// 途中で止める。リングを抜き、書いていればファイルを閉じてから知らせる。
    fn end_with_error(mut self, error: RecordingError) {
        self.tap.detach();
        if let Some(audio) = &self.audio {
            audio.detach();
        }
        let summary = match self.finish() {
            Finished::NoFile => None,
            Finished::Saved(summary) => Some(summary),
            // 閉じるのにも失敗した。最初の理由を優先して伝える
            Finished::FinalizeFailed(_, summary) => summary,
        };
        let _ = self.events.send(RecordingEvent::Failed { error, summary });
    }

    fn summary(&self) -> Option<RecordingSummary> {
        let path = self.path.clone()?;
        Some(RecordingSummary {
            path,
            duration: self.clock.duration(),
            frames_written: self.telemetry.frames_written.load(Ordering::Relaxed),
            frames_dropped: self.tap.dropped() + self.frames_skipped,
            recycle_misses: self.tap.recycle_misses(),
        })
    }
}

/// 保存先に使えるパスか。**空や相対パスは拒む。** カレントディレクトリ基準で解決すると、
/// 起動元によって保存先が変わる（`Program Files` を指すこともある。`docs/design/assets.md`）。
/// 設定ダイアログの欄は空にも相対パスにもできるので、ここで弾く。
fn check_folder(folder: &Path) -> Result<(), RecordingError> {
    if folder.is_absolute() {
        Ok(())
    } else {
        Err(RecordingError::FolderNotAbsolute {
            path: folder.to_path_buf(),
        })
    }
}

/// Sink Writer を作れなかった理由を、利用者に出す種別へ直す。
fn create_error(folder: &Path, error: &WriterError) -> RecordingError {
    match error.stage {
        // ファイルを作れない（保存先に書けない、パスが長すぎる など）
        WriterStage::Create => RecordingError::Folder {
            path: folder.to_path_buf(),
            reason: error.error.to_string(),
        },
        _ => RecordingError::EncoderUnavailable {
            reason: error.error.to_string(),
        },
    }
}

fn write_error(error: &WriterError) -> RecordingError {
    RecordingError::WriteFailed {
        reason: error.error.to_string(),
    }
}

/// 作りかけのファイルを消す。消せなくても続きは無いので、理由はログに残すだけ。
fn remove_partial_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => debug!("作りかけの録画ファイルを消した: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(
            "作りかけの録画ファイルを消せない: {}: {}",
            path.display(),
            e
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_folder_accepts_an_absolute_path() {
        assert_eq!(check_folder(Path::new(r"C:\Users\tester\Videos")), Ok(()));
    }

    #[test]
    fn check_folder_rejects_empty_and_relative_paths() {
        // 空や相対パスはカレントディレクトリ基準になり、保存先が起動元で変わる
        for folder in ["", "videos", r".\videos", r"..\videos"] {
            assert_eq!(
                check_folder(Path::new(folder)),
                Err(RecordingError::FolderNotAbsolute {
                    path: PathBuf::from(folder)
                }),
                "{folder}"
            );
        }
    }
}
