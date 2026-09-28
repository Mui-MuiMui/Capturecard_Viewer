//! Media Foundation の Sink Writer（`IMFSinkWriter`）で NV12 を H.264、16bit PCM を
//! AAC にして MP4 へ書く。
//!
//! **録画スレッドだけが触る。** COM（MTA）と MF の初期化は録画スレッドの入口で済ませて
//! ある（`crate::com`）。組み立ての手順と決めた値の理由は `docs/design/recording.md` の
//! 「Sink Writer の組み立て」。
//!
//! - 入力は NV12 に揃える。ハードウェアの MFT も Microsoft のソフトウェアの H.264
//!   エンコーダも必ず受け取る形式で、Sink Writer に変換を挟ませない
//! - 音声の入力は 16bit PCM の 48kHz 2ch に揃える。Microsoft の AAC エンコーダが
//!   受け取る形に、録画スレッド（`super::audio`）が寄せてから渡す
//! - スロットリングは切る（`MF_SINK_WRITER_DISABLE_THROTTLING`）。エンコーダの遅れは
//!   `backlog` を見て録画スレッドが自分で間引く
//! - D3D のデバイスマネージャは渡さない。サンプルはシステムメモリに置く

use std::path::Path;
use std::ptr;

use windows::core::{Interface, GUID, HSTRING, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_High, CODECAPI_AVEncMPVGOPSize, IMFActivate, IMFAttributes, IMFMediaType,
    IMFSample, IMFSinkWriter, IMFTransform, MFCreateAttributes, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Video,
    MFNominalRange_16_235, MFTEnumEx, MFTGetInfo, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_TRANSFORM_CLSID_Attribute, MFTranscodeContainerType_MPEG4,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoPrimaries_SMPTE170M, MFVideoTransFunc_709, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING,
    MF_SINK_WRITER_STATISTICS, MF_TRANSCODE_CONTAINERTYPE,
};
use windows::Win32::Media::MediaFoundation::{
    MFAudioFormat_AAC, MFAudioFormat_PCM, MFMediaType_Audio, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND,
};
use windows::Win32::System::Com::{CoTaskMemFree, IPersist};

use super::convert::{nv12_len, Nv12Matrix};
use super::pts::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::EncoderInfo;

/// Sink Writer を組み立てるときに決める値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WriterParams {
    /// NV12 の幅と高さ（偶数）
    pub(super) width: u32,
    pub(super) height: u32,
    /// 公称 fps。エンコーダのレート制御の目安とキーフレームの間隔に使う
    pub(super) fps: u32,
    pub(super) bitrate_kbps: u32,
    /// ハードウェアの MFT を選ばせるか
    pub(super) hardware: bool,
    /// AAC の平均ビットレート（kbps）。`None` なら音声トラックを作らない。
    /// Microsoft の AAC エンコーダが受け付ける 96 / 128 / 160 / 192 のどれか（呼び出し側で寄せてある）
    pub(super) audio_bitrate_kbps: Option<u32>,
}

impl WriterParams {
    /// キーフレームの間隔（枚）。2 秒ごと。③のリプレイバッファで古いものを捨てる
    /// 粒度がこの間隔になるので、①から揃えておく（`docs/design/recording.md`）
    pub(super) fn gop_size(&self) -> u32 {
        self.fps.max(1) * 2
    }

    /// 書き出す NV12 の色空間。表示の「自動」と同じ境界で選ぶ
    pub(super) fn matrix(&self) -> Nv12Matrix {
        Nv12Matrix::for_size(self.width as usize, self.height as usize)
    }
}

/// どの段で失敗したか。録画スレッドが利用者に出す理由を選ぶのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriterStage {
    /// `MFCreateSinkWriterFromURL`。ファイルを作れない（保存先に書けない）
    Create,
    /// `AddStream` / `SetInputMediaType` / `BeginWriting`。エンコーダを用意できない
    Configure,
    /// `WriteSample`
    Write,
    /// `Finalize`
    Finalize,
}

/// Sink Writer の失敗。
#[derive(Debug, Clone)]
pub(super) struct WriterError {
    pub(super) stage: WriterStage,
    pub(super) error: windows::core::Error,
}

impl WriterError {
    fn at(stage: WriterStage) -> impl FnOnce(windows::core::Error) -> Self {
        move |error| Self { stage, error }
    }
}

impl std::fmt::Display for WriterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.error)
    }
}

/// 1 本の MP4 を書く Sink Writer。
pub(super) struct SinkWriter {
    writer: IMFSinkWriter,
    stream: u32,
    /// 音声のストリーム。音声トラックを作らなければ `None`
    audio_stream: Option<u32>,
    params: WriterParams,
    samples_written: u64,
}

impl SinkWriter {
    /// `path` に MP4 を作り、書き始められる状態にする。
    ///
    /// 失敗したら作りかけのファイルが残ることがある。消すのは呼び出し側。
    pub(super) fn create(path: &Path, params: WriterParams) -> Result<Self, WriterError> {
        let attributes = sink_writer_attributes(params.hardware)?;
        let url = HSTRING::from(path);
        let writer = unsafe { MFCreateSinkWriterFromURL(&url, None, &attributes) }
            .map_err(WriterError::at(WriterStage::Create))?;

        let configure = WriterError::at(WriterStage::Configure);
        let output = output_media_type(&params).map_err(configure)?;
        let stream = unsafe { writer.AddStream(&output) }
            .map_err(WriterError::at(WriterStage::Configure))?;
        let input = input_media_type(&params).map_err(WriterError::at(WriterStage::Configure))?;
        let encoding =
            encoding_parameters(&params).map_err(WriterError::at(WriterStage::Configure))?;
        unsafe { writer.SetInputMediaType(stream, &input, &encoding) }
            .map_err(WriterError::at(WriterStage::Configure))?;
        // 音声（②）。出力は AAC、入力は 16bit PCM の 48kHz 2ch
        let audio_stream = match params.audio_bitrate_kbps {
            Some(bitrate_kbps) => {
                let output = audio_output_media_type(bitrate_kbps)
                    .map_err(WriterError::at(WriterStage::Configure))?;
                let audio_stream = unsafe { writer.AddStream(&output) }
                    .map_err(WriterError::at(WriterStage::Configure))?;
                let input =
                    audio_input_media_type().map_err(WriterError::at(WriterStage::Configure))?;
                unsafe { writer.SetInputMediaType(audio_stream, &input, None) }
                    .map_err(WriterError::at(WriterStage::Configure))?;
                Some(audio_stream)
            }
            None => None,
        };
        // ここでエンコーダが決まる
        unsafe { writer.BeginWriting() }.map_err(WriterError::at(WriterStage::Configure))?;

        Ok(Self {
            writer,
            stream,
            audio_stream,
            params,
            samples_written: 0,
        })
    }

    /// 組み立てたときの大きさ。届いたフレームと違えば、そのファイルは閉じる。
    pub(super) fn size(&self) -> (u32, u32) {
        (self.params.width, self.params.height)
    }

    /// ハードウェアの MFT を選ばせて組み立てたか。
    pub(super) fn hardware(&self) -> bool {
        self.params.hardware
    }

    /// 書いた枚数。
    pub(super) fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// エンコーダが受け取ったがまだエンコードしていない枚数。取れなければ 0。
    ///
    /// スロットリングを切ってあるので、エンコーダが遅れるとここが増えていく。
    /// 録画スレッドは一定数を超えたら NV12 へ直す前に捨てる。
    pub(super) fn backlog(&self) -> u64 {
        let mut stats = MF_SINK_WRITER_STATISTICS {
            cb: std::mem::size_of::<MF_SINK_WRITER_STATISTICS>() as u32,
            ..Default::default()
        };
        match unsafe { self.writer.GetStatistics(self.stream, &mut stats) } {
            Ok(()) => stats
                .qwNumSamplesReceived
                .saturating_sub(stats.qwNumSamplesEncoded),
            Err(_) => 0,
        }
    }

    /// NV12 の 1 枚を書く。`pts` と `duration` は 100ns 単位。
    pub(super) fn write_nv12(
        &mut self,
        data: &[u8],
        pts: i64,
        duration: i64,
    ) -> Result<(), WriterError> {
        let expected = nv12_len(self.params.width as usize, self.params.height as usize);
        debug_assert_eq!(data.len(), expected);
        let sample =
            memory_sample(data, pts, duration).map_err(WriterError::at(WriterStage::Write))?;
        unsafe { self.writer.WriteSample(self.stream, &sample) }
            .map_err(WriterError::at(WriterStage::Write))?;
        self.samples_written += 1;
        Ok(())
    }

    /// 16bit PCM（48kHz 2ch インターリーブ）を書く。`pts` と `duration` は 100ns 単位。
    /// 音声トラックを作っていなければ何もしない。
    pub(super) fn write_pcm(
        &mut self,
        samples: &[i16],
        pts: i64,
        duration: i64,
    ) -> Result<(), WriterError> {
        let Some(stream) = self.audio_stream else {
            return Ok(());
        };
        if samples.is_empty() {
            return Ok(());
        }
        // SAFETY: i16 の並びをそのままバイト列として読む（リトルエンディアンの PCM と同じ並び）
        let bytes = unsafe {
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples),
            )
        };
        let sample =
            memory_sample(bytes, pts, duration).map_err(WriterError::at(WriterStage::Write))?;
        unsafe { self.writer.WriteSample(stream, &sample) }
            .map_err(WriterError::at(WriterStage::Write))?;
        Ok(())
    }

    /// 残りをエンコードして `moov` を書き、ファイルを閉じる。
    ///
    /// **標準の MP4 はこれを通るまで再生できない。** 失敗してもファイルは消さない
    /// （途中まででも取り出せる手段が残っているかもしれないため）。
    pub(super) fn finalize(self) -> Result<(), WriterError> {
        unsafe { self.writer.Finalize() }.map_err(WriterError::at(WriterStage::Finalize))
    }

    /// 実際に使っているエンコーダの名前と、ハードウェアかどうか。
    ///
    /// Sink Writer からエンコーダの MFT を取り（`GetServiceForStream` に `GUID_NULL`）、
    /// その属性の `MFT_FRIENDLY_NAME_Attribute` と `MFT_ENUM_HARDWARE_URL_Attribute`
    /// （あればハードウェア）を読む。
    ///
    /// 名前を持たない MFT もある（Microsoft のソフトウェアのエンコーダは名前も CLSID も
    /// 属性に持たない。実機で確かめた）。その場合は CLSID（属性の
    /// `MFT_TRANSFORM_CLSID_Attribute`、無ければ `IPersist::GetClassID`）から登録名
    /// （`MFTGetInfo`）を引き、それも無ければ、同じ種類（ハードウェア / ソフトウェア）で
    /// NV12 → H.264 の登録が 1 つだけならその名前を使う。どれも取れなければ `None`。
    /// 属性自体が無ければハードウェアかどうかも `None`。
    pub(super) fn encoder_info(&self) -> EncoderInfo {
        let Some(transform) = self.encoder_transform() else {
            return EncoderInfo {
                name: None,
                hardware: None,
            };
        };
        // SAFETY: 取り出した MFT に対して属性を問い合わせるだけ
        let attributes = unsafe { transform.GetAttributes() }.ok();
        let hardware = attributes.as_ref().map(|attributes| {
            unsafe { attributes.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute) }.is_ok()
        });
        let friendly = attributes
            .as_ref()
            .and_then(|attributes| allocated_string(attributes, &MFT_FRIENDLY_NAME_Attribute));
        let name = friendly
            .or_else(|| {
                let clsid = attributes
                    .as_ref()
                    .and_then(|attributes| {
                        unsafe { attributes.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok()
                    })
                    .or_else(|| {
                        let persist = transform.cast::<IPersist>().ok()?;
                        unsafe { persist.GetClassID() }.ok()
                    })?;
                registered_name(clsid)
            })
            .or_else(|| sole_registered_encoder_name(hardware?));
        EncoderInfo { name, hardware }
    }

    fn encoder_transform(&self) -> Option<IMFTransform> {
        let mut raw: *mut core::ffi::c_void = ptr::null_mut();
        unsafe {
            self.writer.GetServiceForStream(
                self.stream,
                &GUID::zeroed(),
                &IMFTransform::IID,
                &mut raw,
            )
        }
        .ok()?;
        if raw.is_null() {
            return None;
        }
        // SAFETY: IID を指定して受け取った参照 1 つぶんを引き取る
        Some(unsafe { IMFTransform::from_raw(raw) })
    }
}

/// NV12 を受けて H.264 を出すエンコーダのうち、`hardware` の種類の登録が 1 つだけなら
/// その名前。0 個か 2 個以上なら（どれが使われたか分からないので）`None`。
fn sole_registered_encoder_name(hardware: bool) -> Option<String> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0)
    } else {
        MFT_ENUM_FLAG_SYNCMFT
    };
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    }
    .ok()?;
    if activates.is_null() {
        return None;
    }
    // SAFETY: MFTEnumEx が `count` 個の要素を CoTaskMemAlloc で返している。
    // 要素は 1 つずつ引き取って落とし（参照を戻し）、配列そのものは最後に解放する
    let names: Vec<Option<String>> = (0..count as usize)
        .map(|index| {
            let activate = unsafe { (*activates.add(index)).take() }?;
            let attributes: IMFAttributes = activate.cast().ok()?;
            allocated_string(&attributes, &MFT_FRIENDLY_NAME_Attribute)
        })
        .collect();
    unsafe { CoTaskMemFree(Some(activates as *const core::ffi::c_void)) };
    match names.as_slice() {
        [Some(name)] => Some(name.clone()),
        _ => None,
    }
}

/// MFT の CLSID から、登録されている名前を引く。登録されていなければ `None`。
fn registered_name(clsid: GUID) -> Option<String> {
    let mut name = PWSTR::null();
    unsafe { MFTGetInfo(clsid, Some(&mut name), None, None, None, None, None) }.ok()?;
    if name.is_null() {
        return None;
    }
    // SAFETY: MFTGetInfo が NUL 終端の文字列を CoTaskMemAlloc で返している
    let text = unsafe { name.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(name.0 as *const core::ffi::c_void)) };
    text.filter(|text| !text.is_empty())
}

/// 属性の文字列を取り出す。無ければ `None`。
fn allocated_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    unsafe { attributes.GetAllocatedString(key, &mut value, &mut length) }.ok()?;
    if value.is_null() {
        return None;
    }
    // SAFETY: GetAllocatedString が NUL 終端の文字列を CoTaskMemAlloc で返している
    let text = unsafe { value.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(value.0 as *const core::ffi::c_void)) };
    text.filter(|text| !text.is_empty())
}

fn new_attributes(capacity: u32) -> windows::core::Result<IMFAttributes> {
    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, capacity) }?;
    attributes.ok_or_else(windows::core::Error::empty)
}

/// Sink Writer 自体の属性。入れ物は拡張子に頼らず MP4 と指定する。
fn sink_writer_attributes(hardware: bool) -> Result<IMFAttributes, WriterError> {
    (|| -> windows::core::Result<IMFAttributes> {
        let attributes = new_attributes(3)?;
        unsafe {
            attributes.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                u32::from(hardware),
            )?;
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }
        Ok(attributes)
    })()
    .map_err(WriterError::at(WriterStage::Create))
}

/// 2 つの 32 ビット値を 1 つの属性に詰める（`MFSetAttributeSize` / `MFSetAttributeRatio` と同じ形）。
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// 大きさ・fps・画素の縦横比・プログレッシブを、出力と入力の両方に付ける。
fn set_common_video_attributes(
    media_type: &IMFMediaType,
    params: &WriterParams,
) -> windows::core::Result<()> {
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(params.width, params.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(params.fps.max(1), 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(())
}

/// 出力（H.264 High、平均ビットレートの指定だけ）。
fn output_media_type(params: &WriterParams) -> windows::core::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }?;
    set_common_video_attributes(&media_type, params)?;
    unsafe {
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, params.bitrate_kbps.saturating_mul(1000))?;
        media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    }
    set_color_attributes(&media_type, params)?;
    Ok(media_type)
}

/// 色空間とレンジの印（HD は BT.709、SD は BT.601、どちらもリミテッドレンジ）。
/// 入力と出力の両方に付ける。出力にも付けるのは、エンコーダが H.264 の VUI へ
/// 書く値をそこから取るため（付けないとプレーヤーが色空間を推し量ることになる）。
fn set_color_attributes(
    media_type: &IMFMediaType,
    params: &WriterParams,
) -> windows::core::Result<()> {
    let (matrix, primaries) = match params.matrix() {
        Nv12Matrix::Bt709 => (MFVideoTransferMatrix_BT709.0, MFVideoPrimaries_BT709.0),
        Nv12Matrix::Bt601 => (MFVideoTransferMatrix_BT601.0, MFVideoPrimaries_SMPTE170M.0),
    };
    unsafe {
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, matrix as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, primaries as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

/// 入力（NV12、行の詰め物なし）。
fn input_media_type(params: &WriterParams) -> windows::core::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }?;
    set_common_video_attributes(&media_type, params)?;
    set_color_attributes(&media_type, params)?;
    unsafe {
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, params.width)?;
    }
    Ok(media_type)
}

/// システムメモリに置いたサンプルを 1 つ作る。映像（NV12）と音声（PCM）で共通。
fn memory_sample(data: &[u8], pts: i64, duration: i64) -> windows::core::Result<IMFSample> {
    let length = data.len() as u32;
    let buffer = unsafe { MFCreateMemoryBuffer(length) }?;
    let mut target: *mut u8 = ptr::null_mut();
    unsafe { buffer.Lock(&mut target, None, None) }?;
    // SAFETY: Lock が長さ `length` 以上の書き込める領域を返している
    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), target, data.len()) };
    unsafe { buffer.Unlock() }?;
    unsafe { buffer.SetCurrentLength(length) }?;
    let sample = unsafe { MFCreateSample() }?;
    unsafe { sample.AddBuffer(&buffer) }?;
    unsafe { sample.SetSampleTime(pts) }?;
    unsafe { sample.SetSampleDuration(duration) }?;
    Ok(sample)
}

/// 16bit PCM の 1 秒あたりのバイト数（48kHz 2ch なら 192000）
const PCM_BYTES_PER_SECOND: u32 = AUDIO_SAMPLE_RATE * AUDIO_CHANNELS as u32 * 2;

/// 音声の出力（AAC 48kHz 2ch）。ビットレートは `AVG_BYTES_PER_SECOND`（kbps × 1000 ÷ 8）で渡す。
fn audio_output_media_type(bitrate_kbps: u32) -> windows::core::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }?;
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(AUDIO_CHANNELS))?;
        media_type.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            bitrate_kbps.saturating_mul(1000) / 8,
        )?;
    }
    Ok(media_type)
}

/// 音声の入力（16bit PCM 48kHz 2ch）。Microsoft の AAC エンコーダが受け取るのは
/// 16bit PCM の 44.1kHz / 48kHz、1 / 2 / 6ch だけなので、録画スレッドで寄せてから渡す。
fn audio_input_media_type() -> windows::core::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }?;
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(AUDIO_CHANNELS))?;
        media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, u32::from(AUDIO_CHANNELS) * 2)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, PCM_BYTES_PER_SECOND)?;
    }
    Ok(media_type)
}

/// エンコーダへ渡す値。2 秒ごとのキーフレームだけ。レート制御の方式は触らない。
fn encoding_parameters(params: &WriterParams) -> windows::core::Result<IMFAttributes> {
    let attributes = new_attributes(1)?;
    unsafe { attributes.SetUINT32(&CODECAPI_AVEncMPVGOPSize, params.gop_size()) }?;
    Ok(attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::recording::convert::rgb_to_nv12;
    use tempfile::tempdir;
    use windows::Win32::Media::MediaFoundation::{
        MFCreateSourceReaderFromURL, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
        MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    };

    fn params(width: u32, height: u32, fps: u32) -> WriterParams {
        WriterParams {
            width,
            height,
            fps,
            bitrate_kbps: 8000,
            hardware: false,
            audio_bitrate_kbps: None,
        }
    }

    #[test]
    fn writer_params_gop_is_two_seconds() {
        assert_eq!(params(1920, 1080, 60).gop_size(), 120);
        assert_eq!(params(1920, 1080, 30).gop_size(), 60);
        assert_eq!(params(1920, 1080, 0).gop_size(), 2);
    }

    #[test]
    fn writer_params_matrix_follows_the_size() {
        assert_eq!(params(1920, 1080, 60).matrix(), Nv12Matrix::Bt709);
        assert_eq!(params(640, 480, 60).matrix(), Nv12Matrix::Bt601);
    }

    #[test]
    fn pack_puts_the_first_value_in_the_high_bits() {
        assert_eq!(pack(1920, 1080), (1920u64 << 32) | 1080);
        assert_eq!(pack(1, 1), 0x0000_0001_0000_0001);
    }

    #[test]
    #[ignore = "Media Foundation の H.264 エンコーダが必要（CI のランナーにあるかは未確認）"]
    fn sink_writer_writes_a_playable_mp4() {
        // 実行: cargo test -- --ignored sink_writer_writes_a_playable_mp4
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("test.mp4");
        let params = params(320, 240, 30);
        let mut writer = SinkWriter::create(&path, params).expect("Sink Writer を作れる");

        let rgb = vec![128u8; 320 * 240 * 3];
        let mut nv12 = Vec::new();
        assert!(rgb_to_nv12(
            &rgb,
            320,
            240,
            320,
            240,
            params.matrix(),
            &mut nv12
        ));
        for index in 0..30 {
            writer
                .write_nv12(&nv12, index * 333_333, 333_333)
                .expect("書ける");
        }
        writer.finalize().expect("閉じられる");

        let size = std::fs::metadata(&path).expect("ファイルがある").len();
        assert!(size > 0);

        // Media Foundation の MP4 ソースで読み戻せること（= 再生できる）と、
        // 色空間とレンジの印が H.264 に入っていること（SD なので BT.601 / SMPTE 170M）。
        // 出力のメディアタイプに印を付けないと、エンコーダは VUI に書かない（実機で確かめた）
        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_path()), None) }
            .expect("書いた MP4 を開ける");
        let native =
            unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0) }
                .expect("映像のストリームがある");
        let read = |key: &GUID| unsafe { native.GetUINT32(key) }.ok();
        assert_eq!(
            unsafe { native.GetUINT64(&MF_MT_FRAME_SIZE) }.ok(),
            Some(pack(320, 240))
        );
        assert_eq!(
            read(&MF_MT_YUV_MATRIX),
            Some(MFVideoTransferMatrix_BT601.0 as u32)
        );
        assert_eq!(
            read(&MF_MT_VIDEO_PRIMARIES),
            Some(MFVideoPrimaries_SMPTE170M.0 as u32)
        );
        assert_eq!(
            read(&MF_MT_VIDEO_NOMINAL_RANGE),
            Some(MFNominalRange_16_235.0 as u32)
        );
        assert_eq!(
            read(&MF_MT_TRANSFER_FUNCTION),
            Some(MFVideoTransFunc_709.0 as u32)
        );
    }

    #[test]
    #[ignore = "Media Foundation の H.264 / AAC エンコーダが必要（CI のランナーにあるかは未確認）"]
    fn sink_writer_writes_an_aac_track_alongside_the_video() {
        // 実行: cargo test -- --ignored sink_writer_writes_an_aac_track_alongside_the_video
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("test_audio.mp4");
        let params = WriterParams {
            audio_bitrate_kbps: Some(160),
            ..params(320, 240, 30)
        };
        let mut writer = SinkWriter::create(&path, params).expect("Sink Writer を作れる");

        let mut nv12 = Vec::new();
        assert!(rgb_to_nv12(
            &vec![64u8; 320 * 240 * 3],
            320,
            240,
            320,
            240,
            params.matrix(),
            &mut nv12
        ));
        // 1 秒ぶん。映像は 30 枚、音声は 440Hz の正弦波を 100ms ずつ 10 回
        let frames_per_chunk = 4_800usize;
        for index in 0..30 {
            writer
                .write_nv12(&nv12, index * 333_333, 333_333)
                .expect("映像を書ける");
        }
        for chunk in 0..10u64 {
            let pcm: Vec<i16> = (0..frames_per_chunk)
                .flat_map(|frame| {
                    let t = (chunk as usize * frames_per_chunk + frame) as f64 / 48_000.0;
                    let value = (8_000.0 * (std::f64::consts::TAU * 440.0 * t).sin()) as i16;
                    [value, value]
                })
                .collect();
            writer
                .write_pcm(&pcm, chunk as i64 * 1_000_000, 1_000_000)
                .expect("音声を書ける");
        }
        writer.finalize().expect("閉じられる");

        // MF の MP4 ソースで読み戻し、AAC 48kHz 2ch の音声ストリームがあること
        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_path()), None) }
            .expect("書いた MP4 を開ける");
        let native =
            unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0) }
                .expect("音声のストリームがある");
        assert_eq!(
            unsafe { native.GetGUID(&MF_MT_SUBTYPE) }.ok(),
            Some(MFAudioFormat_AAC)
        );
        let read = |key: &GUID| unsafe { native.GetUINT32(key) }.ok();
        assert_eq!(read(&MF_MT_AUDIO_SAMPLES_PER_SECOND), Some(48_000));
        assert_eq!(read(&MF_MT_AUDIO_NUM_CHANNELS), Some(2));
        // 映像のストリームも残っている
        assert!(unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
        }
        .is_ok());
    }
}
