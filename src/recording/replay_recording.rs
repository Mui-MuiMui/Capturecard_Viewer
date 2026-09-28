//! リプレイバッファを通す 1 回の録画（③）。**録画スレッドの中だけにある。**
//!
//! リプレイバッファ（`super::replay::ReplayPipeline`）が持つリングの「いま − N 秒」以降の
//! 最初のキーフレームから、エンコードなしの Sink Writer（`super::passthrough`）へ書き出し、
//! 以降のライブのサンプルも同じだけ時刻をずらして書く。エンコードはリプレイバッファの
//! エンコーダ MFT が済ませてあるので、ここはまとめるだけ。
//!
//! - 先頭のキーフレームの時刻を 0 にする。音声は同じ時刻より前を捨てて先頭を揃える
//! - リングに「いま − N 秒」以降のキーフレームが無ければ（映像がまだ来ていない、
//!   途絶えていた）、次のキーフレームがエンコーダから出てくるのを待って、そこから書く
//! - 止めるときは、止めた時刻より前のサンプルがエンコーダから出てくるのを最大
//!   `STOP_GRACE` だけ待ってから閉じる。エンコーダは数枚遅れて出力するので、
//!   すぐ閉じると最後の数枚が入らない

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::info;
use windows::Win32::Media::MediaFoundation::IMFMediaType;

use super::audio::AudioStats;
use super::encoder::EncodedSample;
use super::file_name::unique_path;
use super::passthrough::PassthroughWriter;
use super::pts::UNITS_PER_SECOND;
use super::recorder::{RecordingRequest, RecordingSummary, RecordingTelemetry};
use super::replay_ring::{Cut, EncodedRing, Placement, Track};
use super::session::{create_error, remove_partial_file, write_error, Finished};
use super::storage::DISK_CHECK_INTERVAL;
use super::RecordingError;

/// 止めてから、止めた時刻より前のサンプルが出てくるのを待つ上限。
const STOP_GRACE: Duration = Duration::from_secs(1);

/// 録画を始めたときの観測値。録画中の値はここからの差で出す（リプレイバッファの
/// リングと差し込み口は録画をまたいで使い続けるため）。
#[derive(Debug, Clone, Copy)]
pub(super) struct Baseline {
    /// 差し込み口が録画に回せなかった枚数
    pub(super) dropped: u64,
    /// `FrameSink` が Vec を回収できなかった回数
    pub(super) recycle_misses: u64,
    /// 音声トラックの観測値。音声を録らない設定なら `None`
    pub(super) audio: Option<AudioStats>,
}

/// 閉じるときに要る、リプレイバッファ側の観測値。
#[derive(Debug, Clone, Copy)]
pub(super) struct Counters {
    pub(super) dropped: u64,
    pub(super) recycle_misses: u64,
}

/// リプレイバッファを通す 1 回の録画。
pub(super) struct ReplayRecording {
    request: RecordingRequest,
    /// 録画を始めた時刻（リプレイバッファの基準からの 100ns）
    requested_at: i64,
    cut: Option<Cut>,
    writer: Option<PassthroughWriter>,
    path: Option<PathBuf>,
    /// 書いた映像の終わり（付け替えた時刻、100ns）
    video_end: i64,
    video_passed_stop: bool,
    audio_passed_stop: bool,
    has_audio: bool,
    stop_deadline: Option<Instant>,
    last_disk_check: Instant,
    baseline: Baseline,
    frames_skipped: u64,
    telemetry: Arc<RecordingTelemetry>,
}

impl ReplayRecording {
    pub(super) fn new(
        request: RecordingRequest,
        requested_at: i64,
        has_audio: bool,
        baseline: Baseline,
        telemetry: Arc<RecordingTelemetry>,
        now: Instant,
    ) -> Self {
        telemetry.begin_recording(baseline.dropped);
        Self {
            request,
            requested_at,
            cut: None,
            writer: None,
            path: None,
            video_end: 0,
            video_passed_stop: false,
            audio_passed_stop: false,
            has_audio,
            stop_deadline: None,
            last_disk_check: now,
            baseline,
            frames_skipped: 0,
            telemetry,
        }
    }

    pub(super) fn request(&self) -> &RecordingRequest {
        &self.request
    }

    pub(super) fn baseline(&self) -> &Baseline {
        &self.baseline
    }

    /// 先頭のキーフレームを決めて書き始めたか。
    pub(super) fn has_cut(&self) -> bool {
        self.cut.is_some()
    }

    /// Sink Writer を作ったか（大きさが変わったときに止めるかの判断に使う）。
    pub(super) fn has_file(&self) -> bool {
        self.writer.is_some()
    }

    /// エンコーダが受け取れずに捨てた枚数を数える。
    pub(super) fn note_skipped(&mut self) {
        self.frames_skipped += 1;
        self.telemetry
            .frames_skipped
            .store(self.frames_skipped, Ordering::Relaxed);
    }

    /// 先頭を `offset` のキーフレームに決め、ファイルを作って、リングの `offset` 以降を書く。
    pub(super) fn open(
        &mut self,
        offset: i64,
        ring: &EncodedRing,
        video_type: &IMFMediaType,
        audio_type: Option<&IMFMediaType>,
    ) -> Result<(), RecordingError> {
        let path = unique_path(&self.request.folder, &self.request.file_stem);
        let audio_type = if self.has_audio { audio_type } else { None };
        let writer = match PassthroughWriter::create(&path, video_type, audio_type) {
            Ok(writer) => writer,
            Err(error) => {
                remove_partial_file(&path);
                return Err(create_error(&self.request.folder, &error));
            }
        };
        let lead = lead_units(self.requested_at, offset);
        info!(
            "録画のファイルを作った（リプレイバッファから）: {}（さかのぼり {:.1} 秒、リングの映像 {:.1} 秒・{} KB、音声: {}）",
            path.display(),
            lead as f64 / UNITS_PER_SECOND as f64,
            ring.held_units() as f64 / UNITS_PER_SECOND as f64,
            ring.bytes() / 1024,
            if audio_type.is_some() { "あり" } else { "なし" }
        );
        self.telemetry
            .set_replay_lead(Duration::from_nanos(lead as u64 * 100));
        self.writer = Some(writer);
        self.path = Some(path);
        self.cut = Some(Cut::new(offset));
        for (track, sample) in ring.samples_from(offset) {
            self.write(track, sample)?;
        }
        Ok(())
    }

    /// 止めるよう頼まれたか。頼まれたあとは先頭を決めない（ファイルを作らない）。
    pub(super) fn stop_requested(&self) -> bool {
        self.stop_deadline.is_some()
    }

    /// サンプルを 1 つ書く。先頭より前と止めた時刻以降は書かない。
    pub(super) fn write(
        &mut self,
        track: Track,
        sample: &EncodedSample,
    ) -> Result<(), RecordingError> {
        let Some(cut) = self.cut else {
            return Ok(());
        };
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        match cut.place(sample.pts) {
            Placement::Before => Ok(()),
            Placement::Inside(pts) => match track {
                Track::Video => {
                    writer
                        .write_video(sample, pts)
                        .map_err(|e| write_error(&e))?;
                    self.video_end = self.video_end.max(pts + sample.duration);
                    self.telemetry
                        .frames_written
                        .fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Track::Audio => writer.write_audio(sample, pts).map_err(|e| write_error(&e)),
            },
            Placement::After => {
                match track {
                    Track::Video => self.video_passed_stop = true,
                    Track::Audio => self.audio_passed_stop = true,
                }
                Ok(())
            }
        }
    }

    /// 止める。`now_units` はリプレイバッファの基準からの 100ns。先頭がまだ決まって
    /// いなければ（キーフレームを待っている）、すぐ閉じる（ファイルは作っていない）。
    pub(super) fn request_stop(&mut self, now_units: i64, now: Instant) {
        if self.stop_deadline.is_some() {
            return;
        }
        match self.cut.as_mut() {
            Some(cut) => {
                cut.stop_at = Some(now_units);
                self.stop_deadline = Some(now + STOP_GRACE);
            }
            None => self.stop_deadline = Some(now),
        }
    }

    /// 止めて閉じてよいか。止めた時刻以降のサンプルが映像と音声の両方で出てきたか、待つ上限を過ぎた。
    pub(super) fn is_done(&self, now: Instant) -> bool {
        let Some(deadline) = self.stop_deadline else {
            return false;
        };
        let passed = self.video_passed_stop && (self.audio_passed_stop || !self.has_audio);
        passed || now >= deadline
    }

    /// 空き容量を見る時期か。見るなら時刻を進める。
    pub(super) fn due_disk_check(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_disk_check) >= DISK_CHECK_INTERVAL {
            self.last_disk_check = now;
            true
        } else {
            false
        }
    }

    /// 閉じる。
    pub(super) fn finish(mut self, counters: Counters) -> Finished {
        let Some(writer) = self.writer.take() else {
            return Finished::NoFile;
        };
        let summary = self.summary(counters);
        match (writer.finalize(), summary) {
            (Ok(()), Some(summary)) => Finished::Saved(summary),
            (Ok(()), None) => Finished::NoFile,
            (Err(error), summary) => Finished::FinalizeFailed(write_error(&error), summary),
        }
    }

    /// 書いた映像の長さ。
    pub(super) fn duration(&self) -> Duration {
        Duration::from_nanos(u64::try_from(self.video_end).unwrap_or(0) * 100)
    }

    fn summary(&self, counters: Counters) -> Option<RecordingSummary> {
        let path = self.path.clone()?;
        let lead = self.cut.map(|cut| {
            Duration::from_nanos(lead_units(self.requested_at, cut.offset) as u64 * 100)
        });
        Some(RecordingSummary {
            path,
            duration: self.duration(),
            frames_written: self.telemetry.frames_written.load(Ordering::Relaxed),
            frames_dropped: counters.dropped.saturating_sub(self.baseline.dropped)
                + self.frames_skipped,
            recycle_misses: counters
                .recycle_misses
                .saturating_sub(self.baseline.recycle_misses),
            replay_lead: lead,
        })
    }
}

/// さかのぼった長さ（100ns）。先頭のキーフレームが録画を始めた時刻より後なら 0。
pub(super) fn lead_units(requested_at: i64, offset: i64) -> i64 {
    requested_at.saturating_sub(offset).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_units_is_the_distance_back_to_the_first_keyframe() {
        assert_eq!(
            lead_units(40 * UNITS_PER_SECOND, 12 * UNITS_PER_SECOND),
            28 * UNITS_PER_SECOND
        );
        // 次のキーフレームを待ってから書き始めたときは、さかのぼっていない
        assert_eq!(lead_units(40 * UNITS_PER_SECOND, 41 * UNITS_PER_SECOND), 0);
    }
}
