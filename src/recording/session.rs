//! リプレイバッファを通さない 1 回の録画（①②の経路）。**録画スレッドの中だけにある。**
//!
//! 録画を始めたらリングを差し込み、最初の映像のフレームで Sink Writer（エンコードも行う）を
//! 作って NV12 と 16bit PCM を渡す。リプレイバッファが OFF のときはこの経路だけを使う。
//! ③で経路を切り替えられるようにしたが、中身は①②で実機確認したまま変えていない
//! （`docs/design/recording.md` の「①②の経路と③の経路」）。
//!
//! 録画スレッドの入口（`super::recorder`）が数 ms ごとに `tick` を呼び、止めるときに
//! `stop` を呼ぶ。失敗の扱い（保存先・空き容量・書き込み・大きさの変化）は
//! リプレイバッファの経路（`super::replay`）と共有する関数をここに置く。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use log::{debug, info, warn};
use ringbuf::traits::Consumer;

use super::audio::{AudioChunk, AudioTrack};
use super::convert::{even_size, rgb_to_nv12, Nv12Matrix};
use super::file_name::unique_path;
use super::pts::{units_from, PtsClock, AUDIO_SAMPLE_RATE};
use super::recorder::{RecordingEvent, RecordingRequest, RecordingSummary, RecordingTelemetry};
use super::storage::{free_bytes, is_low, megabytes, DISK_CHECK_INTERVAL};
use super::writer::{SinkWriter, WriterError, WriterParams, WriterStage};
use super::RecordingError;
use crate::audio::AudioTap;
use crate::video::{VideoFrame, VideoTap, VideoTapConsumer, VIDEO_TAP_CAPACITY};

/// エンコーダが受け取ってまだエンコードしていない枚数がこれを超えたら、NV12 へ直す前に捨てる。
/// スロットリングを切ってあるので、放っておくとエンコーダの遅れの分だけメモリが溜まる。
const MAX_ENCODER_BACKLOG: u64 = 30;

/// 映像が無いときに使う公称 fps
pub(super) const FALLBACK_FPS: u32 = 60;

/// 音声を Sink Writer（またはエンコーダ）へ渡す最小の長さ（出力フレーム数、約 21ms）。
/// 数 ms ごとの小さな塊で `WriteSample` を増やさないため。止めるときは残りをまとめて渡す
pub(super) const MIN_AUDIO_CHUNK_FRAMES: usize = 1024;

/// Sink Writer を作る前（最初の映像のフレームが届く前）に溜めておく音声の上限
/// （出力フレーム数、5 秒）。超えたら古いものから捨てる。塊は PTS を持っているので、
/// 先頭を捨てても後ろの時刻はずれない
const MAX_PENDING_AUDIO_FRAMES: usize = AUDIO_SAMPLE_RATE as usize * 5;

/// 録画を閉じた結果。
pub(super) enum Finished {
    /// 1 枚も書いていない（ファイルは作っていない）
    NoFile,
    /// `Finalize` まで済んだ
    Saved(RecordingSummary),
    /// `Finalize` に失敗した。ファイルは残してある
    FinalizeFailed(RecordingError, Option<RecordingSummary>),
}

impl Finished {
    /// 閉じた結果を UI スレッドへ知らせる。
    pub(super) fn report(self, events: &Sender<RecordingEvent>) {
        match self {
            Finished::Saved(summary) => {
                let _ = events.send(RecordingEvent::Stopped(summary));
            }
            // 1 枚も届かなかった。ファイルは作っていない
            Finished::NoFile => fail(events, RecordingError::NoVideo),
            Finished::FinalizeFailed(error, summary) => {
                let _ = events.send(RecordingEvent::Failed { error, summary });
            }
        }
    }

    /// 途中で止まったときに、閉じたファイルの内容だけを取り出す。閉じるのにも失敗したら
    /// 最初の理由を優先して伝えるので、ここでは捨てる。
    pub(super) fn into_summary(self) -> Option<RecordingSummary> {
        match self {
            Finished::NoFile => None,
            Finished::Saved(summary) => Some(summary),
            Finished::FinalizeFailed(_, summary) => summary,
        }
    }
}

/// リプレイバッファを通さない 1 回の録画。
pub(super) struct Session {
    request: RecordingRequest,
    tap: VideoTap,
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
    /// 保存先を確かめ、リングを差し込んで録画を始める。始められなければ `Failed` を送って `None`。
    pub(super) fn begin(
        request: RecordingRequest,
        tap: VideoTap,
        audio_tap: AudioTap,
        telemetry: Arc<RecordingTelemetry>,
        events: Sender<RecordingEvent>,
    ) -> Option<Self> {
        if let Err(error) = prepare_folder(&request.folder) {
            fail(&events, error);
            return None;
        }
        let fps = request.nominal_fps.unwrap_or(FALLBACK_FPS).max(1);
        // リングを差し込んだ時刻が PTS の基準 t0
        let consumer = tap.attach(VIDEO_TAP_CAPACITY);
        let t0 = Instant::now();
        telemetry.begin_recording(0);
        // 音声も同じ t0 を基準にする。差し込むのは t0 の直後なので、t0 より前に
        // 届いたサンプルはリングに入らない（コールバック 1 回ぶんの端数は削る）
        let audio = request
            .audio_bitrate_kbps
            .map(|_| AudioTrack::attach(audio_tap, t0));
        info!(
            "録画を始めた（保存先: {}、ファイル名: {}.mp4、{}kbps、ハードウェアエンコーダ: {}、音声: {}）",
            request.folder.display(),
            request.file_stem,
            request.video_bitrate_kbps,
            if request.hardware_encoder {
                "使う"
            } else {
                "使わない"
            },
            match request.audio_bitrate_kbps {
                Some(kbps) => format!("AAC {kbps}kbps"),
                None => "録らない".to_string(),
            }
        );
        let _ = events.send(RecordingEvent::Started);
        Some(Self {
            request,
            tap,
            audio,
            audio_pending: VecDeque::new(),
            audio_pending_frames: 0,
            telemetry,
            events,
            consumer: Some(consumer),
            clock: PtsClock::new(t0, fps),
            writer: None,
            path: None,
            nv12: Vec::new(),
            frames_skipped: 0,
            last_disk_check: t0,
        })
    }

    fn fps(&self) -> u32 {
        self.request.nominal_fps.unwrap_or(FALLBACK_FPS).max(1)
    }

    /// リングに溜まった分を書き、空き容量を見る。録画スレッドが数 ms ごとに呼ぶ。
    /// 失敗したら呼び出し側が `end_with_error` で止める。
    pub(super) fn tick(&mut self) -> Result<(), RecordingError> {
        // 映像を先に書く。Sink Writer は最初の映像のフレームで作るので、
        // 音声はそれまで溜めておき、作られたあとで渡す
        self.drain()?;
        self.pump_audio(MIN_AUDIO_CHUNK_FRAMES)?;
        if self.last_disk_check.elapsed() >= DISK_CHECK_INTERVAL {
            self.last_disk_check = Instant::now();
            check_disk(&self.request.folder)?;
        }
        Ok(())
    }

    /// 止める。リングを抜いてから、残っている分を書き切って閉じ、結果を知らせる。
    pub(super) fn stop(mut self) {
        self.detach();
        if let Err(error) = self.drain().and_then(|()| self.finish_audio()) {
            self.end_with_error(error);
            return;
        }
        self.finish().report(&self.events);
    }

    /// 途中で止める。リングを抜き、書いていればファイルを閉じてから知らせる。
    /// まだ書ける失敗なら、閉じる前に止めたときと同じく音声を映像の終わりまで揃える。
    /// 揃えるのに失敗しても、知らせるのは最初の失敗の理由。
    pub(super) fn end_with_error(mut self, error: RecordingError) {
        self.detach();
        if finishes_audio_after(&error) {
            if let Err(audio_error) = self.finish_audio() {
                warn!(
                    "途中で止めた録画の音声を映像の終わりまで揃えられない（止めた理由: {}）: {}",
                    error, audio_error
                );
            }
        }
        let summary = self.finish().into_summary();
        let _ = self.events.send(RecordingEvent::Failed { error, summary });
    }

    fn detach(&self) {
        self.tap.detach();
        if let Some(audio) = &self.audio {
            audio.detach();
        }
    }

    /// リングに溜まっている分を書く。
    fn drain(&mut self) -> Result<(), RecordingError> {
        while let Some((frame, received_at)) = self.consumer.as_mut().and_then(|c| c.try_pop()) {
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
        let (width, height) = even_size(frame.width, frame.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let size = (width as u32, height as u32);
        let writer_size = self.writer.as_ref().map(SinkWriter::size);
        let Some(pts) = stamp_frame(&mut self.clock, writer_size, size, received_at)? else {
            // 録画を始める前に受け取ったフレーム
            return Ok(());
        };

        match &self.writer {
            None => self.open_writer(size)?,
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
        self.telemetry.publish_audio(&audio.stats());
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
        self.telemetry.publish_audio(&audio.stats());
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

    fn summary(&self) -> Option<RecordingSummary> {
        let path = self.path.clone()?;
        Some(RecordingSummary {
            path,
            duration: self.clock.duration(),
            frames_written: self.telemetry.frames_written.load(Ordering::Relaxed),
            frames_dropped: self.tap.dropped() + self.frames_skipped,
            recycle_misses: self.tap.recycle_misses(),
            replay_lead: None,
        })
    }
}

/// フレームに PTS を付ける。`t0` より前なら `Ok(None)`。書いている大きさと違えば
/// `SizeChanged` を返し、**`PtsClock` を進めない。** 書かずに捨てるフレームの時刻が
/// 映像の長さ（`PtsClock::duration`）に入ると、止めるときに音声がその分だけ長く揃い、
/// `RecordingSummary::duration` にも入るため（#340）。
fn stamp_frame(
    clock: &mut PtsClock,
    writer_size: Option<(u32, u32)>,
    size: (u32, u32),
    received_at: Instant,
) -> Result<Option<i64>, RecordingError> {
    if !clock.accepts(received_at) {
        return Ok(None);
    }
    if let Some(from) = writer_size {
        if from != size {
            return Err(RecordingError::SizeChanged { from, to: size });
        }
    }
    Ok(clock.pts_for(received_at))
}

/// 途中で止まったときに、閉じる前に音声を仕上げるか（残りを渡し、映像の終わりまで無音で埋める）。
/// Sink Writer がまだ書ける失敗（大きさの変化・空き容量が境界を切った）だけ仕上げる。
/// 書き込みの失敗では書けないので仕上げない。Sink Writer を作れなかった失敗では
/// 書く先が無い。種類を足したときに決め忘れないよう、`_` でまとめない。
fn finishes_audio_after(error: &RecordingError) -> bool {
    match error {
        RecordingError::SizeChanged { .. } | RecordingError::DiskLow { .. } => true,
        RecordingError::WriteFailed { .. }
        | RecordingError::Folder { .. }
        | RecordingError::FolderNotAbsolute { .. }
        | RecordingError::ReplayDiskShort { .. }
        | RecordingError::EncoderUnavailable { .. }
        | RecordingError::NoVideo
        | RecordingError::Platform { .. }
        | RecordingError::ThreadStopped => false,
    }
}

/// 始められなかったことを知らせる。
pub(super) fn fail(events: &Sender<RecordingEvent>, error: RecordingError) {
    let _ = events.send(RecordingEvent::Failed {
        error,
        summary: None,
    });
}

/// 保存先を作り、空き容量を確かめる。
pub(super) fn prepare_folder(folder: &Path) -> Result<(), RecordingError> {
    check_folder(folder)?;
    std::fs::create_dir_all(folder).map_err(|e| RecordingError::Folder {
        path: folder.to_path_buf(),
        reason: e.to_string(),
    })?;
    check_disk(folder)
}

/// 空き容量が境界を切っていれば失敗。
pub(super) fn check_disk(folder: &Path) -> Result<(), RecordingError> {
    let free = free_bytes(folder);
    if is_low(free) {
        return Err(RecordingError::DiskLow {
            free_mb: free.map(megabytes).unwrap_or(0),
        });
    }
    Ok(())
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
pub(super) fn create_error(folder: &Path, error: &WriterError) -> RecordingError {
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

pub(super) fn write_error(error: &WriterError) -> RecordingError {
    RecordingError::WriteFailed {
        reason: error.error.to_string(),
    }
}

/// 作りかけのファイルを消す。消せなくても続きは無いので、理由はログに残すだけ。
pub(super) fn remove_partial_file(path: &Path) {
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
    use crate::recording::test_support::record_until_size_changes;

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

    #[test]
    fn finished_into_summary_keeps_only_a_closed_file() {
        assert_eq!(Finished::NoFile.into_summary(), None);
        let summary = RecordingSummary {
            path: PathBuf::from(r"C:\v\a.mp4"),
            duration: std::time::Duration::from_secs(1),
            frames_written: 60,
            frames_dropped: 0,
            recycle_misses: 0,
            replay_lead: None,
        };
        assert_eq!(
            Finished::Saved(summary.clone()).into_summary(),
            Some(summary.clone())
        );
        assert_eq!(
            Finished::FinalizeFailed(RecordingError::NoVideo, Some(summary.clone())).into_summary(),
            Some(summary)
        );
    }

    #[test]
    fn dropped_frame_of_new_size_does_not_advance_clock() {
        let t0 = Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        let mut clock = PtsClock::new(t0, 60);
        let size = Some((1280, 720));
        assert_eq!(
            stamp_frame(&mut clock, None, (1280, 720), at(0)).unwrap(),
            Some(0)
        );
        assert!(stamp_frame(&mut clock, size, (1280, 720), at(1000))
            .unwrap()
            .is_some());
        let before = clock.duration();
        // 大きさの変わったフレームは書かずに捨てる。映像の長さに入れない
        let error = stamp_frame(&mut clock, size, (640, 480), at(1500)).unwrap_err();
        assert_eq!(
            error,
            RecordingError::SizeChanged {
                from: (1280, 720),
                to: (640, 480)
            }
        );
        assert_eq!(clock.duration(), before);
        // t0 より前のフレームは大きさが違っても捨てるだけ
        let mut later = PtsClock::new(at(10), 60);
        assert_eq!(stamp_frame(&mut later, size, (640, 480), t0).unwrap(), None);
        assert_eq!(later.duration(), std::time::Duration::ZERO);
    }

    #[test]
    fn finishes_audio_only_while_the_writer_can_still_write() {
        // 大きさの変化と空き容量は、Sink Writer がまだ書けるので音声を映像の終わりまで揃える
        assert!(finishes_audio_after(&RecordingError::SizeChanged {
            from: (1280, 720),
            to: (640, 480)
        }));
        assert!(finishes_audio_after(&RecordingError::DiskLow {
            free_mb: 499
        }));
        // 書き込みに失敗したら書けない。Sink Writer を作れなかった失敗には書く先が無い
        let reason = || "理由".to_string();
        for error in [
            RecordingError::WriteFailed { reason: reason() },
            RecordingError::EncoderUnavailable { reason: reason() },
            RecordingError::Folder {
                path: PathBuf::from(r"C:\v"),
                reason: reason(),
            },
            RecordingError::FolderNotAbsolute {
                path: PathBuf::from("v"),
            },
            RecordingError::NoVideo,
            RecordingError::Platform { reason: reason() },
            RecordingError::ThreadStopped,
        ] {
            assert!(!finishes_audio_after(&error), "{error:?}");
        }
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要。5 秒ほどかかる"]
    fn session_stopped_by_size_change_keeps_audio_as_long_as_video() {
        // 実行: cargo test -- --ignored session_stopped_by_size_change --nocapture
        //
        // 大きさの変化で止まったファイルも、止めたときと同じく音声を映像の終わりまで揃える。
        // 音声デバイスが無いと無音は「いま − 200ms」までしか書かないので、揃えなければ
        // 200ms ほど短くなる。フェイクの音声があるときも、渡していない残りを捨てない
        for audio_device in [false, true] {
            let (video_end, audio_end) = record_until_size_changes(audio_device);
            let diff_ms = (audio_end - video_end) / 10_000;
            println!(
                "音声デバイス {audio_device}: 映像 {video_end}、音声 {audio_end}（{diff_ms}ms）"
            );
            // 下は AAC の 1 フレーム（1024 / 48000 ≒ 21ms）ぶんの端数を許す。上は大きさの
            // 変わったフレームが届くまで（開き直しの間）を許す
            assert!(
                (-30..=200).contains(&diff_ms),
                "音声デバイス {audio_device}: 映像 {video_end}、音声 {audio_end}（{diff_ms}ms）"
            );
        }
    }
}
