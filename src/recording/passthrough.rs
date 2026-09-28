//! エンコードなしの Sink Writer（③ リプレイバッファ）。
//!
//! 入力と出力のメディアタイプを同じ H.264 / AAC にすると、Sink Writer はエンコーダを挟まず、
//! 受け取ったサンプルを MP4 へまとめるだけになる。リプレイバッファのリングに持っている
//! エンコード済みのサンプルと、そのあとのライブのサンプルをここへ流す
//! （`docs/design/recording.md` の「リプレイバッファへの伸ばし方（#182）」）。
//!
//! - メディアタイプはエンコーダ MFT の出力のメディアタイプの写し（`EncoderMft::stream_type`）。
//!   H.264 の `MF_MT_MPEG_SEQUENCE_HEADER`（SPS / PPS）と AAC の `MF_MT_USER_DATA` が入っている。
//!   これが無いと MP4 の `avcC` / `esds` を書けない
//! - キーフレームには `MFSampleExtension_CleanPoint` を付ける。MP4 の同期サンプルの表
//!   （`stss`）はこれから作られ、プレーヤーのシークの起点になる
//! - スロットリングは切る（①と同じ）。**録画スレッドだけが触る**

use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSinkWriter, MFCreateSinkWriterFromURL, MFSampleExtension_CleanPoint,
    MFTranscodeContainerType_MPEG4, MF_SINK_WRITER_DISABLE_THROTTLING, MF_TRANSCODE_CONTAINERTYPE,
};

use super::encoder::EncodedSample;
use super::writer::{memory_sample, new_attributes, WriterError, WriterStage};

/// エンコードなしで MP4 へまとめる Sink Writer。
pub(super) struct PassthroughWriter {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
}

impl PassthroughWriter {
    /// `path` に MP4 を作り、書き始められる状態にする。`audio` が `None` なら映像だけ。
    ///
    /// 失敗したら作りかけのファイルが残ることがある。消すのは呼び出し側。
    pub(super) fn create(
        path: &Path,
        video: &IMFMediaType,
        audio: Option<&IMFMediaType>,
    ) -> Result<Self, WriterError> {
        let attributes = (|| -> windows::core::Result<_> {
            let attributes = new_attributes(2)?;
            unsafe {
                attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
                attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            }
            Ok(attributes)
        })()
        .map_err(WriterError::at(WriterStage::Create))?;
        let url = HSTRING::from(path);
        let writer = unsafe { MFCreateSinkWriterFromURL(&url, None, &attributes) }
            .map_err(WriterError::at(WriterStage::Create))?;
        let add = |media_type: &IMFMediaType| -> windows::core::Result<u32> {
            let stream = unsafe { writer.AddStream(media_type) }?;
            // 入力 = 出力。エンコーダは挟まらない
            unsafe { writer.SetInputMediaType(stream, media_type, None) }?;
            Ok(stream)
        };
        let video_stream = add(video).map_err(WriterError::at(WriterStage::Configure))?;
        let audio_stream = match audio {
            Some(audio) => Some(add(audio).map_err(WriterError::at(WriterStage::Configure))?),
            None => None,
        };
        unsafe { writer.BeginWriting() }.map_err(WriterError::at(WriterStage::Configure))?;
        Ok(Self {
            writer,
            video: video_stream,
            audio: audio_stream,
        })
    }

    /// 映像のサンプルを `pts`（付け替えた時刻、100ns）で書く。
    pub(super) fn write_video(
        &mut self,
        sample: &EncodedSample,
        pts: i64,
    ) -> Result<(), WriterError> {
        self.write(self.video, sample, pts)
    }

    /// 音声のサンプルを `pts` で書く。音声トラックを作っていなければ何もしない。
    pub(super) fn write_audio(
        &mut self,
        sample: &EncodedSample,
        pts: i64,
    ) -> Result<(), WriterError> {
        match self.audio {
            Some(stream) => self.write(stream, sample, pts),
            None => Ok(()),
        }
    }

    fn write(&mut self, stream: u32, sample: &EncodedSample, pts: i64) -> Result<(), WriterError> {
        let media = memory_sample(&sample.data, pts, sample.duration)
            .map_err(WriterError::at(WriterStage::Write))?;
        if sample.keyframe {
            unsafe { media.SetUINT32(&MFSampleExtension_CleanPoint, 1) }
                .map_err(WriterError::at(WriterStage::Write))?;
        }
        unsafe { self.writer.WriteSample(stream, &media) }
            .map_err(WriterError::at(WriterStage::Write))
    }

    /// `moov` を書いてファイルを閉じる。失敗してもファイルは消さない（①と同じ）。
    pub(super) fn finalize(self) -> Result<(), WriterError> {
        unsafe { self.writer.Finalize() }.map_err(WriterError::at(WriterStage::Finalize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::recording::convert::rgb_to_nv12;
    use crate::recording::encoder::EncoderMft;
    use crate::recording::pts::UNITS_PER_SECOND;
    use crate::recording::replay_ring::{EncodedRing, Track};
    use crate::recording::writer::WriterParams;
    use tempfile::tempdir;
    use windows::Win32::Media::MediaFoundation::{
        IMFSourceReader, MFCreateSourceReaderFromURL, MF_PD_DURATION,
        MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        MF_SOURCE_READER_MEDIASOURCE,
    };
    use windows::Win32::System::Variant::VT_UI8;

    /// 読み戻した MP4 の長さ（100ns）。
    fn duration_of(reader: &IMFSourceReader) -> i64 {
        let value = unsafe {
            reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        }
        .expect("長さを読める");
        assert_eq!(unsafe { value.Anonymous.Anonymous.vt }, VT_UI8);
        unsafe { value.Anonymous.Anonymous.Anonymous.uhVal as i64 }
    }

    /// 映像のストリームのサンプルを全部読み、時刻の並びを返す。
    fn video_times(reader: &IMFSourceReader) -> Vec<i64> {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut times = Vec::new();
        loop {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample = None;
            unsafe {
                reader.ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )
            }
            .expect("読める");
            if sample.is_none() {
                break;
            }
            times.push(time);
        }
        times
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要（CI のランナーにあるかは未確認）"]
    fn replay_ring_written_without_reencoding_plays_from_zero() {
        // 実行: cargo test -- --ignored replay_ring_written_without_reencoding_plays_from_zero
        //
        // エンコーダ MFT で 5 秒ぶん（30fps）と音声を溜め、「いま − 3 秒」以降の最初の
        // キーフレームからエンコードなしの Sink Writer へ書き、Source Reader で読み戻す
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let params = WriterParams {
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 2000,
            hardware: false,
            audio_bitrate_kbps: Some(160),
        };
        let mut video = EncoderMft::video(&params).expect("映像のエンコーダを作れる");
        let mut audio = EncoderMft::audio(160).expect("音声のエンコーダを作れる");
        let mut ring = EncodedRing::default();
        let mut nv12 = Vec::new();
        for index in 0..150i64 {
            let rgb = vec![(index * 7 % 256) as u8; 320 * 240 * 3];
            assert!(rgb_to_nv12(
                &rgb,
                320,
                240,
                320,
                240,
                params.matrix(),
                &mut nv12
            ));
            let sample = memory_sample(&nv12, index * 333_333, 333_333).expect("作れる");
            video.encode(&sample).expect("エンコードできる");
            video
                .take_output()
                .into_iter()
                .for_each(|s| ring.push_video(s));
        }
        for chunk in 0..50i64 {
            let pcm = vec![0u8; 4_800 * 4];
            let sample = memory_sample(&pcm, chunk * 1_000_000, 1_000_000).expect("作れる");
            audio.encode(&sample).expect("エンコードできる");
            audio
                .take_output()
                .into_iter()
                .for_each(|s| ring.push_audio(s));
        }

        // いま = 5 秒。3.1 秒さかのぼると 1.9 秒。その後の最初のキーフレーム（60 枚目、
        // 約 2 秒）から書く
        let offset = ring
            .start_point(19 * UNITS_PER_SECOND / 10)
            .expect("キーフレームがある");
        assert_eq!(offset, 60 * 333_333);

        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("replay.mp4");
        let video_type = video.stream_type().expect("映像の形");
        let audio_type = audio.stream_type().expect("音声の形");
        let mut writer =
            PassthroughWriter::create(&path, &video_type, Some(&audio_type)).expect("作れる");
        let mut written = 0;
        for (track, sample) in ring.samples_from(offset) {
            match track {
                Track::Video => {
                    writer
                        .write_video(sample, sample.pts - offset)
                        .expect("書ける");
                    written += 1;
                }
                Track::Audio => writer
                    .write_audio(sample, sample.pts - offset)
                    .expect("書ける"),
            }
        }
        writer.finalize().expect("閉じられる");

        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_path()), None) }
            .expect("書いた MP4 を開ける");
        // 約 3 秒（2 秒〜5 秒）
        let duration = duration_of(&reader);
        assert!((duration - 30_000_000).abs() < 1_000_000, "長さ {duration}");
        assert!(unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
        }
        .is_ok());
        // 書いた枚数だけ読め、先頭は 0 から始まる（最初から再生できる）
        let times = video_times(&reader);
        assert_eq!(times.len(), written);
        assert!(times[0].abs() < 10_000, "先頭の時刻 {}", times[0]);
    }
}
