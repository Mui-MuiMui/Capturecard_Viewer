//! リプレイバッファ（③）。**録画スレッドの中だけにある。**
//!
//! ON のあいだは差し込み口（`VideoTap` / `AudioTap`）を差したままにし、届いたフレームと
//! 音声をエンコーダ MFT（`super::encoder`）で H.264 / AAC にして、エンコード済みのリング
//! （`super::replay_ring`）に持つ。録画を始めたら、リングの「いま − N 秒」以降の最初の
//! キーフレームからエンコードなしの Sink Writer へ書き出す（`super::replay_recording`）。
//! 設計は `docs/design/recording.md` の「リプレイバッファへの伸ばし方（#182）」。
//!
//! - NV12 と PCM への変換、PTS の付け方は①②と同じもの（`convert` / `pts` / `audio`）を使う。
//!   PTS の基準はリングを差し込んだ時刻で、録画を始めた時刻ではない
//! - **録画にまつわる失敗（保存先・空き容量・書き込み・大きさの変化）では止まらない。**
//!   その録画だけを閉じて知らせ、リングは回り続ける。止まるのはエンコーダを用意できない・
//!   エンコードに失敗したときだけで、呼び出し側（`super::recorder`）へ返す
//! - 映像の大きさが変わったら、エンコーダを作り直してリングを空にする（違う大きさの
//!   サンプルは 1 つのファイルに入れられない）。録画中でファイルを作っていれば、
//!   ①と同じくそのファイルを閉じて止める

use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use log::{info, warn};

use super::audio::{AudioChunk, AudioTrack};
use super::convert::{even_size, rgb_to_nv12, Nv12Matrix};
use super::encoder::{EncodedSample, EncoderError, EncoderMft};
use super::pts::{units_since, PtsClock, AUDIO_CHANNELS, UNITS_PER_SECOND};
use super::recorder::{RecordingEvent, RecordingRequest, RecordingTelemetry};
use super::replay_recording::{Baseline, Counters, ReplayRecording};
use super::replay_ring::{keep_from, replay_cut, EncodedRing, Track};
use super::session::{check_disk, fail, prepare_folder, FALLBACK_FPS, MIN_AUDIO_CHUNK_FRAMES};
use super::writer::{memory_sample, WriterParams};
use super::RecordingError;
use crate::audio::AudioTap;
use crate::video::{VideoTap, VideoTapConsumer, VIDEO_TAP_CAPACITY};

/// キーフレームの間隔（100ns）。エンコーダには fps × 2 枚で伝えているので 2 秒
const GOP_UNITS: i64 = 2 * UNITS_PER_SECOND;

/// AAC のエンコーダへ 1 回に渡す長さの上限（出力フレーム数）。音声が来ていない間に
/// 溜まった無音などをまとめて渡さず、小分けにする
const MAX_AUDIO_INPUT_FRAMES: usize = 4096;

/// リプレイバッファの設定。UI スレッドが `[recording]` と映像の公称 fps から組み立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayConfig {
    /// さかのぼる長さ（秒）。5〜300 に丸めてある
    pub seconds: u32,
    pub video_bitrate_kbps: u32,
    pub hardware_encoder: bool,
    /// 音声（AAC）のビットレート。`None` なら音声を持たない
    pub audio_bitrate_kbps: Option<u32>,
    /// 公称 fps。映像が無ければ `None`（60 として扱う）
    pub nominal_fps: Option<u32>,
}

impl ReplayConfig {
    fn fps(&self) -> u32 {
        self.nominal_fps.unwrap_or(FALLBACK_FPS).max(1)
    }

    /// エンコーダの作り直しが要らない違いか（さかのぼる長さだけが違う）。
    pub(super) fn same_encoders(&self, other: &ReplayConfig) -> bool {
        self.video_bitrate_kbps == other.video_bitrate_kbps
            && self.hardware_encoder == other.hardware_encoder
            && self.audio_bitrate_kbps == other.audio_bitrate_kbps
            && self.fps() == other.fps()
    }
}

/// リプレイバッファ。ON のあいだ録画スレッドが持つ。
pub(super) struct ReplayPipeline {
    config: ReplayConfig,
    /// リングに持つ長さ（秒）。録画中に OFF にされたら 0（最後の GOP だけ持つ）
    retain_seconds: u32,
    tap: VideoTap,
    consumer: VideoTapConsumer,
    audio: Option<AudioTrack>,
    audio_encoder: Option<EncoderMft>,
    t0: Instant,
    clock: PtsClock,
    video_encoder: Option<EncoderMft>,
    size: Option<(u32, u32)>,
    /// ハードウェアのエンコーダで最初の 1 枚から失敗したので、以降はソフトウェアを使う
    hardware_failed: bool,
    nv12: Vec<u8>,
    ring: EncodedRing,
    recording: Option<ReplayRecording>,
    telemetry: Arc<RecordingTelemetry>,
    events: Sender<RecordingEvent>,
}

impl ReplayPipeline {
    /// 差し込み口を差してリプレイバッファを始める。エンコーダは最初のフレームが届いてから作る
    /// （大きさがフレームで決まるため）。
    pub(super) fn start(
        config: ReplayConfig,
        tap: VideoTap,
        audio_tap: AudioTap,
        telemetry: Arc<RecordingTelemetry>,
        events: Sender<RecordingEvent>,
    ) -> Self {
        let consumer = tap.attach(VIDEO_TAP_CAPACITY);
        let t0 = Instant::now();
        let audio = config
            .audio_bitrate_kbps
            .map(|_| AudioTrack::attach(audio_tap, t0));
        info!(
            "リプレイバッファを始めた（{} 秒、{}kbps、ハードウェアエンコーダ: {}、音声: {}、公称 {}fps）",
            config.seconds,
            config.video_bitrate_kbps,
            if config.hardware_encoder {
                "使う"
            } else {
                "使わない"
            },
            match config.audio_bitrate_kbps {
                Some(kbps) => format!("AAC {kbps}kbps"),
                None => "持たない".to_string(),
            },
            config.fps()
        );
        Self {
            retain_seconds: config.seconds,
            clock: PtsClock::new(t0, config.fps()),
            config,
            tap,
            consumer,
            audio,
            audio_encoder: None,
            t0,
            video_encoder: None,
            size: None,
            hardware_failed: false,
            nv12: Vec::new(),
            ring: EncodedRing::default(),
            recording: None,
            telemetry,
            events,
        }
    }

    pub(super) fn config(&self) -> &ReplayConfig {
        &self.config
    }

    /// さかのぼる長さを変える。エンコーダはそのまま。0 にすると最後の GOP だけを持つ
    /// （録画中に OFF にされたとき）。
    pub(super) fn set_retain_seconds(&mut self, seconds: u32) {
        self.retain_seconds = seconds;
        self.config.seconds = seconds;
    }

    pub(super) fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// 録画を始める。保存先を確かめ、リングに「いま − N 秒」以降のキーフレームがあれば
    /// そこから書き出す。無ければ次のキーフレームを待つ。
    pub(super) fn start_recording(&mut self, request: RecordingRequest) {
        if let Err(error) = prepare_folder(&request.folder) {
            fail(&self.events, error);
            return;
        }
        let now = Instant::now();
        let requested_at = units_since(self.t0, now);
        let baseline = Baseline {
            dropped: self.tap.dropped(),
            recycle_misses: self.tap.recycle_misses(),
            audio: self.audio.as_ref().map(AudioTrack::stats),
        };
        info!(
            "録画を始めた（リプレイバッファから {} 秒さかのぼる。保存先: {}、ファイル名: {}.mp4、リングの映像 {:.1} 秒・{} KB、捨てた GOP {} 回）",
            self.retain_seconds,
            request.folder.display(),
            request.file_stem,
            self.ring.held_units() as f64 / UNITS_PER_SECOND as f64,
            self.ring.bytes() / 1024,
            self.ring.discarded_gops()
        );
        self.recording = Some(ReplayRecording::new(
            request,
            requested_at,
            self.audio.is_some(),
            baseline,
            Arc::clone(&self.telemetry),
            now,
        ));
        let _ = self.events.send(RecordingEvent::Started);
        if let Some(encoder) = &self.video_encoder {
            let _ = self
                .events
                .send(RecordingEvent::EncoderSelected(encoder.info()));
        }
        let cut = replay_cut(requested_at, self.retain_seconds);
        if let Some(offset) = self.ring.start_point(cut) {
            self.open_recording(offset);
        }
    }

    /// 録画を止めるよう頼む。止めた時刻より前のサンプルが出てきてから閉じる（`tick` が行う）。
    pub(super) fn stop_recording(&mut self) {
        let now = Instant::now();
        let now_units = units_since(self.t0, now);
        if let Some(recording) = self.recording.as_mut() {
            recording.request_stop(now_units, now);
        }
    }

    /// 録画をすぐ閉じる（終了時）。止めた時刻より後のサンプルを待たない。
    pub(super) fn finish_recording_now(&mut self) {
        self.stop_recording();
        if let Some(recording) = self.recording.take() {
            self.close(recording);
        }
    }

    /// 溜まったフレームと音声をエンコードしてリングへ積み、録画中ならファイルへ書く。
    /// 録画スレッドが数 ms ごとに呼ぶ。エンコーダの失敗だけを返す（リプレイバッファは続けられない）。
    pub(super) fn tick(&mut self) -> Result<(), RecordingError> {
        let now = Instant::now();
        self.drain_video()?;
        self.pump_audio(now)?;

        let now_units = units_since(self.t0, now);
        self.ring
            .trim(keep_from(now_units, self.retain_seconds, GOP_UNITS));
        self.telemetry
            .publish_ring(self.ring.held_units(), self.ring.discarded_gops());

        let Some(recording) = self.recording.as_mut() else {
            return Ok(());
        };
        if recording.is_done(now) {
            if let Some(recording) = self.recording.take() {
                self.close(recording);
            }
        } else if recording.due_disk_check(now) {
            if let Err(error) = check_disk(&recording.request().folder) {
                self.end_recording_with_error(error);
            }
        }
        Ok(())
    }

    /// リプレイバッファが続けられなくなった。録画中ならその録画も閉じて知らせる。
    /// 録画を閉じたら真（呼び出し側はリプレイバッファの失敗を別に知らせなくてよい）。
    pub(super) fn fail(mut self, error: RecordingError) -> bool {
        if self.recording.is_none() {
            return false;
        }
        self.end_recording_with_error(error);
        true
    }

    // ---- 映像 ----

    fn drain_video(&mut self) -> Result<(), RecordingError> {
        while let Some((frame, received_at)) = self.consumer.pop() {
            let Some(pts) = self.clock.pts_for(received_at) else {
                continue;
            };
            let (width, height) = even_size(frame.width, frame.height);
            if width == 0 || height == 0 {
                continue;
            }
            let size = (width as u32, height as u32);
            if self.size != Some(size) {
                self.change_size(size);
            }
            if self.video_encoder.is_none() {
                self.open_video_encoder(size)?;
            }
            let accepts = match self.video_encoder.as_mut() {
                Some(encoder) => encoder.accepts_input().map_err(encoder_error)?,
                None => false,
            };
            if !accepts {
                // エンコーダが追いつかない。表示は落とさず、録画（リング）だけがコマ落ちする
                if let Some(recording) = self.recording.as_mut() {
                    recording.note_skipped();
                }
                continue;
            }
            let converted = rgb_to_nv12(
                &frame.data,
                frame.width,
                frame.height,
                width,
                height,
                Nv12Matrix::for_size(width, height),
                &mut self.nv12,
            );
            // **`Arc` は NV12 へ直したらすぐ手放す**（`FrameSink` の Vec の回収を妨げないため）
            drop(frame);
            if converted {
                self.encode_video(pts)?;
            }
        }
        // 非同期型は入力と関係なく出力が届く
        if let Some(encoder) = self.video_encoder.as_mut() {
            encoder.pull().map_err(encoder_error)?;
        }
        self.take_video_output();
        Ok(())
    }

    fn encode_video(&mut self, pts: i64) -> Result<(), RecordingError> {
        let duration = self.clock.sample_duration();
        let sample = memory_sample(&self.nv12, pts, duration).map_err(|error| {
            RecordingError::EncoderUnavailable {
                reason: error.to_string(),
            }
        })?;
        let Some(encoder) = self.video_encoder.as_mut() else {
            return Ok(());
        };
        match encoder.encode(&sample) {
            Ok(()) => {}
            Err(error) if encoder.is_hardware() && !encoder.has_produced() => {
                // ①と同じく、ハードウェアで最初の 1 枚から失敗したらソフトウェアで作り直す
                warn!(
                    "ハードウェアのエンコーダで最初のフレームをエンコードできないので、ソフトウェアで作り直す: {}",
                    error
                );
                self.hardware_failed = true;
                self.video_encoder = None;
                if let Some(size) = self.size {
                    self.open_video_encoder(size)?;
                }
                if let Some(encoder) = self.video_encoder.as_mut() {
                    encoder.encode(&sample).map_err(encoder_error)?;
                }
            }
            Err(error) => return Err(encoder_error(error)),
        }
        self.take_video_output();
        Ok(())
    }

    fn open_video_encoder(&mut self, size: (u32, u32)) -> Result<(), RecordingError> {
        let params = WriterParams {
            width: size.0,
            height: size.1,
            fps: self.config.fps(),
            bitrate_kbps: self.config.video_bitrate_kbps,
            hardware: self.config.hardware_encoder && !self.hardware_failed,
            audio_bitrate_kbps: None,
        };
        let encoder = EncoderMft::video(&params).map_err(encoder_error)?;
        let info = encoder.info();
        info!(
            "リプレイバッファのエンコーダを作った（{}x{}、{}fps、{}kbps、エンコーダ: {}、ハードウェア: {}）",
            size.0,
            size.1,
            params.fps,
            params.bitrate_kbps,
            info.name.as_deref().unwrap_or("（名前を取得できない）"),
            if info.hardware == Some(true) {
                "はい"
            } else {
                "いいえ"
            }
        );
        if self.recording.is_some() {
            let _ = self.events.send(RecordingEvent::EncoderSelected(info));
        }
        self.video_encoder = Some(encoder);
        Ok(())
    }

    /// 映像の大きさが変わった。エンコーダを作り直し、リングを空にする。
    fn change_size(&mut self, size: (u32, u32)) {
        if let Some(from) = self.size {
            info!(
                "リプレイバッファの映像の大きさが変わったので、エンコーダを作り直してリングを空にする（{}x{} → {}x{}）",
                from.0, from.1, size.0, size.1
            );
            if self
                .recording
                .as_ref()
                .is_some_and(ReplayRecording::has_file)
            {
                self.end_recording_with_error(RecordingError::SizeChanged { from, to: size });
            }
        }
        self.video_encoder = None;
        self.ring.clear();
        self.size = Some(size);
    }

    fn take_video_output(&mut self) {
        let outputs = match self.video_encoder.as_mut() {
            Some(encoder) => encoder.take_output(),
            None => return,
        };
        for sample in outputs {
            self.on_video(sample);
        }
    }

    /// エンコードした映像を 1 つ受け取る。リングへ積み、録画中ならファイルへ書く。
    fn on_video(&mut self, sample: EncodedSample) {
        self.ring.push_video(sample.clone());
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        if recording.has_cut() {
            if let Err(error) = recording.write(Track::Video, &sample) {
                self.end_recording_with_error(error);
            }
        } else if sample.keyframe && !recording.stop_requested() {
            // 「いま − N 秒」以降のキーフレームがリングに無かったので、ライブの
            // キーフレームを待っていた。ここから書く（リングにはもう積んである）
            self.open_recording(sample.pts);
        }
    }

    /// 先頭を `offset` に決めてファイルを作り、リングの中身を書く。
    fn open_recording(&mut self, offset: i64) {
        let result = self.ensure_audio_encoder().and_then(|()| {
            let video_type = self
                .video_encoder
                .as_ref()
                .ok_or(RecordingError::NoVideo)?
                .stream_type()
                .map_err(|error| RecordingError::EncoderUnavailable {
                    reason: error.to_string(),
                })?;
            let audio_type = match &self.audio_encoder {
                Some(encoder) => Some(encoder.stream_type().map_err(|error| {
                    RecordingError::EncoderUnavailable {
                        reason: error.to_string(),
                    }
                })?),
                None => None,
            };
            match self.recording.as_mut() {
                Some(recording) => {
                    recording.open(offset, &self.ring, &video_type, audio_type.as_ref())
                }
                None => Ok(()),
            }
        });
        if let Err(error) = result {
            self.end_recording_with_error(error);
        }
    }

    // ---- 音声 ----

    fn pump_audio(&mut self, now: Instant) -> Result<(), RecordingError> {
        let Some(track) = self.audio.as_mut() else {
            return Ok(());
        };
        track.pump(now);
        let stats = track.stats();
        let chunk = track.take_chunk(MIN_AUDIO_CHUNK_FRAMES);
        if let Some(base) = self.recording.as_ref().and_then(|r| r.baseline().audio) {
            self.telemetry.publish_audio(&stats.since(&base));
        }
        if let Some(chunk) = chunk {
            self.encode_audio(chunk)?;
        }
        Ok(())
    }

    fn ensure_audio_encoder(&mut self) -> Result<(), RecordingError> {
        let Some(bitrate_kbps) = self.config.audio_bitrate_kbps else {
            return Ok(());
        };
        if self.audio_encoder.is_none() {
            let encoder = EncoderMft::audio(bitrate_kbps).map_err(encoder_error)?;
            info!(
                "リプレイバッファの音声のエンコーダを作った（AAC {}kbps、エンコーダ: {}）",
                bitrate_kbps,
                encoder
                    .info()
                    .name
                    .as_deref()
                    .unwrap_or("（名前を取得できない）")
            );
            self.audio_encoder = Some(encoder);
        }
        Ok(())
    }

    /// PCM の塊を小分けにして AAC にし、リングへ積む（録画中ならファイルへも書く）。
    fn encode_audio(&mut self, chunk: AudioChunk) -> Result<(), RecordingError> {
        self.ensure_audio_encoder()?;
        let channels = usize::from(AUDIO_CHANNELS);
        let frames = chunk.frames();
        let mut start = 0;
        while start < frames {
            let end = (start + MAX_AUDIO_INPUT_FRAMES).min(frames);
            // 塊の中の位置から時刻を割り振る。長さは差で取り、切り捨ての誤差を溜めない
            let pts = chunk.pts + chunk.duration * start as i64 / frames as i64;
            let next = chunk.pts + chunk.duration * end as i64 / frames as i64;
            let samples = &chunk.samples[start * channels..end * channels];
            // SAFETY: i16 の並びをそのままバイト列として読む（リトルエンディアンの PCM と同じ並び）
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    samples.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(samples),
                )
            };
            let sample = memory_sample(bytes, pts, next - pts).map_err(|error| {
                RecordingError::EncoderUnavailable {
                    reason: error.to_string(),
                }
            })?;
            let outputs = match self.audio_encoder.as_mut() {
                Some(encoder) => {
                    encoder.encode(&sample).map_err(encoder_error)?;
                    encoder.take_output()
                }
                None => Vec::new(),
            };
            for output in outputs {
                self.on_audio(output);
            }
            start = end;
        }
        Ok(())
    }

    fn on_audio(&mut self, sample: EncodedSample) {
        self.ring.push_audio(sample.clone());
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        if let Err(error) = recording.write(Track::Audio, &sample) {
            self.end_recording_with_error(error);
        }
    }

    // ---- 録画を閉じる ----

    fn counters(&self) -> Counters {
        Counters {
            dropped: self.tap.dropped(),
            recycle_misses: self.tap.recycle_misses(),
        }
    }

    /// 閉じて結果を知らせる。
    fn close(&mut self, recording: ReplayRecording) {
        self.log_audio(&recording);
        recording.finish(self.counters()).report(&self.events);
    }

    /// 途中で止める。ファイルを作っていれば閉じてから知らせる。リングは回り続ける。
    fn end_recording_with_error(&mut self, error: RecordingError) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        self.log_audio(&recording);
        let summary = recording.finish(self.counters()).into_summary();
        let _ = self.events.send(RecordingEvent::Failed { error, summary });
    }

    /// 閉じた録画の音声をログへ残す（②と同じ 1 行。値は録画を始めてからの差）。
    fn log_audio(&self, recording: &ReplayRecording) {
        if !recording.has_file() {
            return;
        }
        if let (Some(track), Some(base)) = (&self.audio, recording.baseline().audio) {
            track.stats().since(&base).log(recording.duration());
        }
    }
}

impl Drop for ReplayPipeline {
    /// 差し込み口を抜く。**次のリプレイバッファや録画が差し込む前に落とすこと**
    /// （差し込み口は 1 つしか無く、あとから抜くと次の差し込みを抜いてしまう）。
    fn drop(&mut self) {
        self.tap.detach();
        if let Some(audio) = &self.audio {
            audio.detach();
        }
        info!(
            "リプレイバッファを止めた（リングの映像 {:.1} 秒・{} KB、捨てた GOP {} 回）",
            self.ring.held_units() as f64 / UNITS_PER_SECOND as f64,
            self.ring.bytes() / 1024,
            self.ring.discarded_gops()
        );
        self.telemetry.publish_ring(0, 0);
    }
}

fn encoder_error(error: EncoderError) -> RecordingError {
    RecordingError::EncoderUnavailable {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ReplayConfig {
        ReplayConfig {
            seconds: 30,
            video_bitrate_kbps: 8000,
            hardware_encoder: true,
            audio_bitrate_kbps: Some(160),
            nominal_fps: Some(60),
        }
    }

    #[test]
    fn replay_config_same_encoders_ignores_only_the_seconds() {
        let base = config();
        assert!(base.same_encoders(&ReplayConfig {
            seconds: 300,
            ..config()
        }));
        // 映像が無い（None）ときは 60fps として扱う
        assert!(base.same_encoders(&ReplayConfig {
            nominal_fps: None,
            ..config()
        }));
        for changed in [
            ReplayConfig {
                video_bitrate_kbps: 12_000,
                ..config()
            },
            ReplayConfig {
                hardware_encoder: false,
                ..config()
            },
            ReplayConfig {
                audio_bitrate_kbps: None,
                ..config()
            },
            ReplayConfig {
                nominal_fps: Some(30),
                ..config()
            },
        ] {
            assert!(!base.same_encoders(&changed), "{changed:?}");
        }
    }
}
