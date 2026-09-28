//! エンコーダ MFT（`IMFTransform`）を自分で回す（③ リプレイバッファ）。
//!
//! ①②の Sink Writer はエンコードとファイルへのまとめを一緒に行い、エンコード済みの
//! サンプルを取り出す口が無い。リプレイバッファはエンコード済みのサンプルをメモリに
//! 持つので、エンコーダ MFT を直接使う（`docs/design/recording.md` の
//! 「リプレイバッファへの伸ばし方（#182）」）。**録画スレッドだけが触る。**
//!
//! - **同期型と非同期型の両方を扱う。** Microsoft のソフトウェアのエンコーダ（H.264 / AAC）は
//!   同期型で、`ProcessInput` と `ProcessOutput` を交互に呼べばよい。ハードウェアの
//!   H.264 エンコーダ（Intel / NVIDIA / AMD）は非同期型で、`MF_TRANSFORM_ASYNC_UNLOCK` を
//!   立ててから、`IMFMediaEventGenerator` の `METransformNeedInput` / `METransformHaveOutput`
//!   に従って入出力する。**イベントは待たずに取る**（`MF_EVENT_FLAG_NO_WAIT`）。録画スレッドは
//!   数 ms ごとに起きるので、そのたびに溜まった分を捌く
//! - 非同期型が入力を求めていないときに届いたフレームは、エンコーダへ渡さずに捨てる
//!   （`accepts_input`）。エンコーダが追いつかない分だけ録画がコマ落ちし、表示は落とさない。
//!   ①の Sink Writer の「遅れが 30 枚を超えたら捨てる」と同じ考え方
//! - H.264 はキーフレームを 2 秒ごと（`CODECAPI_AVEncMPVGOPSize` = fps × 2）にし、
//!   B フレームを使わせない（`CODECAPI_AVEncMPVDefaultBPictureCount` = 0）。B フレームが
//!   あると出力が表示順と違う順で出てきて、リングを時刻で切る前提が崩れる
//! - D3D のデバイスマネージャは渡さない。サンプルはシステムメモリに置く（①と同じ）

use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Arc;

use log::{info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFActivate,
    IMFAttributes, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    MEError, METransformHaveOutput, METransformNeedInput, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateAlignedMemoryBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Audio,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFT_CATEGORY_AUDIO_ENCODER, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_SUBTYPE, MF_MT_USER_DATA, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use super::bitstream::{aac_user_data, is_idr, parameter_sets};
use super::pts::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::writer::{
    allocated_string, audio_input_media_type, audio_output_media_type, input_media_type,
    output_media_type, pack, WriterParams,
};
use super::EncoderInfo;

/// 出力のバッファの大きさをエンコーダが教えてくれないときの予備（音声、バイト）
const FALLBACK_AUDIO_OUTPUT_BYTES: u32 = 64 * 1024;

/// エンコード済みのサンプル 1 つ。時刻は 100ns 単位で、リプレイバッファの基準
/// （リングを差し込んだ時刻）から。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EncodedSample {
    pub(super) pts: i64,
    pub(super) duration: i64,
    /// そこから復号を始められるか（H.264 の IDR）。音声は常に真
    pub(super) keyframe: bool,
    /// 中身。リングと Sink Writer の両方へ渡すので `Arc` で持つ
    pub(super) data: Arc<[u8]>,
}

/// 映像か音声か。キーフレームの見分け方と、出力のバッファの見積もりが違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Video,
    Audio,
}

/// エンコーダ MFT の失敗。
#[derive(Debug, Clone)]
pub(super) enum EncoderError {
    /// 使えるエンコーダが登録されていない
    NotFound,
    /// メディアタイプの設定や開始の通知に失敗した
    Configure(windows::core::Error),
    /// `ProcessInput` / `ProcessOutput` に失敗した
    Encode(windows::core::Error),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncoderError::NotFound => f.write_str("エンコーダが登録されていない"),
            EncoderError::Configure(error) => write!(f, "組み立て: {error}"),
            EncoderError::Encode(error) => write!(f, "エンコード: {error}"),
        }
    }
}

/// 1 つのエンコーダ MFT。
pub(super) struct EncoderMft {
    activate: IMFActivate,
    transform: IMFTransform,
    /// 非同期型のときだけある
    events: Option<IMFMediaEventGenerator>,
    input_stream: u32,
    output_stream: u32,
    /// 非同期型: 受け取った `METransformNeedInput` のうち、まだ入力していない数
    pending_input: u32,
    /// 非同期型: 受け取った `METransformHaveOutput` のうち、まだ取り出していない数
    pending_output: u32,
    /// 出力のサンプルをエンコーダが用意するか（しないなら呼び出し側がバッファを渡す）
    provides_samples: bool,
    output_bytes: u32,
    output_alignment: u32,
    kind: MediaKind,
    info: EncoderInfo,
    /// 出力のメディアタイプに SPS / PPS が無かったときに、最初のキーフレームから取り出したもの
    sequence_header: Option<Vec<u8>>,
    /// 取り出した出力の数
    produced: u64,
    /// 取り出したがまだ渡していない出力
    ready: Vec<EncodedSample>,
}

impl EncoderMft {
    /// H.264 のエンコーダを作る。`params.hardware` が真ならハードウェアの MFT から試し、
    /// どれも組み立てられなければソフトウェアの MFT へ倒す（①の Sink Writer と同じ）。
    pub(super) fn video(params: &WriterParams) -> Result<Self, EncoderError> {
        let mut candidates = Vec::new();
        if params.hardware {
            let flags = MFT_ENUM_FLAG(
                MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
            );
            candidates.extend(
                enumerate(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    (MFMediaType_Video, MFVideoFormat_NV12),
                    (MFMediaType_Video, MFVideoFormat_H264),
                )
                .into_iter()
                .map(|activate| (activate, true)),
            );
        }
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        candidates.extend(
            enumerate(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                (MFMediaType_Video, MFVideoFormat_NV12),
                (MFMediaType_Video, MFVideoFormat_H264),
            )
            .into_iter()
            .map(|activate| (activate, false)),
        );
        open_first(candidates, |activate, hardware| {
            Self::open(activate, hardware, MediaKind::Video, |transform| {
                configure_video(transform, params)
            })
        })
    }

    /// AAC のエンコーダを作る（Microsoft の AAC エンコーダ。同期型）。
    pub(super) fn audio(bitrate_kbps: u32) -> Result<Self, EncoderError> {
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        let candidates = enumerate(
            MFT_CATEGORY_AUDIO_ENCODER,
            flags,
            (MFMediaType_Audio, MFAudioFormat_PCM),
            (MFMediaType_Audio, MFAudioFormat_AAC),
        )
        .into_iter()
        .map(|activate| (activate, false))
        .collect();
        open_first(candidates, |activate, hardware| {
            Self::open(activate, hardware, MediaKind::Audio, |transform| {
                configure_audio(transform, bitrate_kbps)
            })
        })
    }

    fn open(
        activate: IMFActivate,
        hardware: bool,
        kind: MediaKind,
        configure: impl FnOnce(&Transform<'_>) -> windows::core::Result<()>,
    ) -> Result<Self, EncoderError> {
        let name = activate
            .cast::<IMFAttributes>()
            .ok()
            .and_then(|attributes| allocated_string(&attributes, &MFT_FRIENDLY_NAME_Attribute));
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(EncoderError::Configure)?;
        // 以降で失敗したら、作ったものを片付ける（`Drop` が行う）
        let mut encoder = Self {
            activate,
            transform,
            events: None,
            input_stream: 0,
            output_stream: 0,
            pending_input: 0,
            pending_output: 0,
            provides_samples: false,
            output_bytes: 0,
            output_alignment: 0,
            kind,
            info: EncoderInfo {
                name,
                hardware: Some(hardware),
            },
            sequence_header: None,
            produced: 0,
            ready: Vec::new(),
        };
        encoder
            .prepare(configure)
            .map_err(EncoderError::Configure)?;
        Ok(encoder)
    }

    fn prepare(
        &mut self,
        configure: impl FnOnce(&Transform<'_>) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        // 非同期型は、使う側が非同期として扱うと宣言しないと動かない
        let asynchronous = unsafe { self.transform.GetAttributes() }
            .ok()
            .filter(|attributes| unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) } == Ok(1));
        if let Some(attributes) = &asynchronous {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }?;
        }
        let (input_stream, output_stream) = stream_ids(&self.transform);
        self.input_stream = input_stream;
        self.output_stream = output_stream;
        configure(&Transform {
            transform: &self.transform,
            input: input_stream,
            output: output_stream,
        })?;
        self.refresh_output_info()?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        if asynchronous.is_some() {
            self.events = Some(self.transform.cast()?);
        }
        Ok(())
    }

    fn refresh_output_info(&mut self) -> windows::core::Result<()> {
        let info = unsafe { self.transform.GetOutputStreamInfo(self.output_stream) }?;
        self.provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        self.output_bytes = info.cbSize;
        self.output_alignment = info.cbAlignment;
        Ok(())
    }

    /// 名前とハードウェアかどうか。名前は列挙したときの登録名。
    pub(super) fn info(&self) -> EncoderInfo {
        self.info.clone()
    }

    /// ハードウェアの MFT か。
    pub(super) fn is_hardware(&self) -> bool {
        self.info.hardware == Some(true)
    }

    /// 出力を 1 つでも取り出したか。ハードウェアで最初の 1 枚から失敗したときに
    /// ソフトウェアへ倒すかの判断に使う。
    pub(super) fn has_produced(&self) -> bool {
        self.produced > 0
    }

    /// いま入力を受け取れるか。同期型は常に受け取る。非同期型は `METransformNeedInput` が
    /// 残っているときだけ。
    pub(super) fn accepts_input(&mut self) -> Result<bool, EncoderError> {
        self.poll_events()?;
        Ok(self.events.is_none() || self.pending_input > 0)
    }

    /// サンプルを 1 つ渡し、出せるだけ出力を取り出す。取り出した出力は `take_output` で受け取る。
    pub(super) fn encode(&mut self, sample: &IMFSample) -> Result<(), EncoderError> {
        self.poll_events()?;
        if self.events.is_some() {
            if self.pending_input == 0 {
                // 呼び出し側は `accepts_input` を見てから渡す。ここへは来ない
                return Ok(());
            }
            self.pending_input -= 1;
        }
        match unsafe { self.transform.ProcessInput(self.input_stream, sample, 0) } {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_NOTACCEPTING && self.events.is_none() => {
                // 同期型が出力を先に取り出すよう求めている。取り出してから渡し直す
                self.pull()?;
                unsafe { self.transform.ProcessInput(self.input_stream, sample, 0) }
                    .map_err(EncoderError::Encode)?;
            }
            Err(error) => return Err(EncoderError::Encode(error)),
        }
        self.pull()
    }

    /// 出せるだけ出力を取り出す。非同期型は、届いている `METransformHaveOutput` の数だけ。
    pub(super) fn pull(&mut self) -> Result<(), EncoderError> {
        self.poll_events()?;
        if self.events.is_some() {
            while self.pending_output > 0 {
                self.pending_output -= 1;
                self.process_output()?;
            }
        } else {
            while self.process_output()? {}
        }
        Ok(())
    }

    /// 取り出した出力を渡す。
    pub(super) fn take_output(&mut self) -> Vec<EncodedSample> {
        std::mem::take(&mut self.ready)
    }

    /// エンコードなしの Sink Writer へ渡すメディアタイプ。エンコーダの出力のメディアタイプの
    /// 写しに、H.264 なら `MF_MT_MPEG_SEQUENCE_HEADER`（SPS / PPS）、AAC なら `MF_MT_USER_DATA`
    /// が入っていることを確かめ、無ければ補う。補えなければ失敗（SPS / PPS は最初の
    /// キーフレームが出るまで分からないことがある）。
    pub(super) fn stream_type(&self) -> windows::core::Result<IMFMediaType> {
        let current = unsafe { self.transform.GetOutputCurrentType(self.output_stream) }?;
        let copy = unsafe { MFCreateMediaType() }?;
        unsafe { current.CopyAllItems(&copy) }?;
        match self.kind {
            MediaKind::Video => {
                if unsafe { copy.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.unwrap_or(0) == 0 {
                    let header = self
                        .sequence_header
                        .as_ref()
                        .ok_or_else(windows::core::Error::empty)?;
                    unsafe { copy.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, header) }?;
                }
            }
            MediaKind::Audio => {
                if unsafe { copy.GetBlobSize(&MF_MT_USER_DATA) }.unwrap_or(0) == 0 {
                    let data = aac_user_data(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS)
                        .ok_or_else(windows::core::Error::empty)?;
                    unsafe { copy.SetBlob(&MF_MT_USER_DATA, &data) }?;
                }
            }
        }
        Ok(copy)
    }

    /// 非同期型のイベントを、待たずに取れるだけ取る。
    fn poll_events(&mut self) -> Result<(), EncoderError> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(EncoderError::Encode(error)),
            };
            let kind = unsafe { event.GetType() }.map_err(EncoderError::Encode)?;
            match kind as i32 {
                kind if kind == METransformNeedInput.0 => self.pending_input += 1,
                kind if kind == METransformHaveOutput.0 => self.pending_output += 1,
                kind if kind == MEError.0 => {
                    let status = unsafe { event.GetStatus() }.map_err(EncoderError::Encode)?;
                    return Err(EncoderError::Encode(status.into()));
                }
                // 流し切り（`METransformDrainComplete`）などは使わない
                _ => {}
            }
        }
    }

    /// `ProcessOutput` を 1 回呼ぶ。出力が無ければ（入力が足りない）偽。
    fn process_output(&mut self) -> Result<bool, EncoderError> {
        let sample = if self.provides_samples {
            None
        } else {
            Some(self.output_sample().map_err(EncoderError::Encode)?)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_stream,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        // SAFETY: 渡したもの、または MFT が入れたものを引き取って落とす（参照を戻す）
        let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });
        match result {
            Ok(()) => {
                if let Some(sample) = sample {
                    let encoded = self.read_output(&sample).map_err(EncoderError::Encode)?;
                    self.produced += 1;
                    self.ready.push(encoded);
                }
                Ok(true)
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // 出力の形が変わった（ハードウェアの H.264 エンコーダが最初に求めることがある）。
                // エンコーダが示す形を選び直して続ける
                let media_type =
                    unsafe { self.transform.GetOutputAvailableType(self.output_stream, 0) }
                        .map_err(EncoderError::Encode)?;
                unsafe {
                    self.transform
                        .SetOutputType(self.output_stream, &media_type, 0)
                }
                .map_err(EncoderError::Encode)?;
                self.refresh_output_info().map_err(EncoderError::Encode)?;
                info!("エンコーダの出力の形が変わったので選び直した");
                Ok(true)
            }
            Err(error) => Err(EncoderError::Encode(error)),
        }
    }

    /// 出力を受け取るサンプル。エンコーダが大きさを教えてくれなければ見積もる。
    fn output_sample(&self) -> windows::core::Result<IMFSample> {
        let bytes = match (self.output_bytes, self.kind) {
            (0, MediaKind::Video) => self.video_frame_bytes().unwrap_or(4 * 1024 * 1024),
            (0, MediaKind::Audio) => FALLBACK_AUDIO_OUTPUT_BYTES,
            (bytes, _) => bytes,
        };
        let alignment = self.output_alignment.saturating_sub(1);
        let buffer = unsafe { MFCreateAlignedMemoryBuffer(bytes, alignment) }?;
        let sample = unsafe { MFCreateSample() }?;
        unsafe { sample.AddBuffer(&buffer) }?;
        Ok(sample)
    }

    /// 映像の 1 枚の NV12 の大きさ。エンコードした 1 枚はこれを超えない見込みで使う。
    fn video_frame_bytes(&self) -> Option<u32> {
        let media_type = unsafe { self.transform.GetOutputCurrentType(self.output_stream) }.ok()?;
        let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
        let (width, height) = ((size >> 32) as u32, size as u32);
        width
            .checked_mul(height)?
            .checked_mul(3)
            .map(|bytes| bytes / 2)
    }

    fn read_output(&mut self, sample: &IMFSample) -> windows::core::Result<EncodedSample> {
        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let duration = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
        let clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
            .ok()
            .map(|value| value != 0);
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
        let mut source: *mut u8 = ptr::null_mut();
        let mut length = 0u32;
        unsafe { buffer.Lock(&mut source, None, Some(&mut length)) }?;
        // SAFETY: Lock が長さ `length` の読める領域を返している
        let data: Arc<[u8]> =
            Arc::from(unsafe { std::slice::from_raw_parts(source, length as usize) });
        unsafe { buffer.Unlock() }?;
        let keyframe = match self.kind {
            // 中身で確かめる。Annex B として読めなければ印に頼る
            MediaKind::Video => is_idr(&data).or(clean_point).unwrap_or(false),
            MediaKind::Audio => true,
        };
        if keyframe && self.kind == MediaKind::Video && self.sequence_header.is_none() {
            self.sequence_header = parameter_sets(&data);
        }
        Ok(EncodedSample {
            pts,
            duration,
            keyframe,
            data,
        })
    }
}

impl Drop for EncoderMft {
    /// 流しかけのものを捨て、ストリームの終わりを伝えて閉じる。非同期型は `IMFShutdown` で
    /// 止めないと、内部の作業スレッドが残る。
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
            let _ = unsafe { shutdown.Shutdown() };
        }
        let _ = unsafe { self.activate.ShutdownObject() };
    }
}

/// 組み立ての関数へ渡す、MFT とストリームの番号。
struct Transform<'a> {
    transform: &'a IMFTransform,
    input: u32,
    output: u32,
}

/// H.264: キーフレームの間隔と B フレームの数を先に伝え、出力 → 入力の順に形を決める
/// （エンコーダは出力の形を先に求める）。
fn configure_video(target: &Transform<'_>, params: &WriterParams) -> windows::core::Result<()> {
    if let Ok(codec) = target.transform.cast::<ICodecAPI>() {
        // 受け付けないエンコーダもある。付かなくても録画はできるので、失敗は捨てる
        let gop = VARIANT::from(params.gop_size());
        let _ = unsafe { codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop) };
        let no_b_frames = VARIANT::from(0u32);
        let _ = unsafe { codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &no_b_frames) };
    }
    let output = output_media_type(params)?;
    unsafe { target.transform.SetOutputType(target.output, &output, 0) }?;
    let input = input_media_type(params)?;
    if unsafe { target.transform.SetInputType(target.input, &input, 0) }.is_ok() {
        return Ok(());
    }
    // 自前の形を受け付けないエンコーダには、エンコーダが示す NV12 の形に大きさと fps を足して渡す
    let offered = offered_type(target.transform, target.input, &MFVideoFormat_NV12)?;
    unsafe {
        offered.SetUINT64(&MF_MT_FRAME_SIZE, pack(params.width, params.height))?;
        offered.SetUINT64(&MF_MT_FRAME_RATE, pack(params.fps.max(1), 1))?;
        target.transform.SetInputType(target.input, &offered, 0)
    }
}

/// AAC: 入力（16bit PCM 48kHz 2ch）と出力（AAC）の形を決める。決める順を求めるエンコーダが
/// あるので、入力 → 出力で駄目なら出力 → 入力で試す。
fn configure_audio(target: &Transform<'_>, bitrate_kbps: u32) -> windows::core::Result<()> {
    let input = audio_input_media_type()?;
    let output = audio_output_media_type(bitrate_kbps)?;
    let input_first = unsafe {
        target
            .transform
            .SetInputType(target.input, &input, 0)
            .and_then(|()| target.transform.SetOutputType(target.output, &output, 0))
    };
    if input_first.is_ok() {
        return Ok(());
    }
    unsafe {
        target.transform.SetOutputType(target.output, &output, 0)?;
        target.transform.SetInputType(target.input, &input, 0)?;
    }
    // 出力の形の要点が変わっていないことを確かめる（AAC 48kHz 2ch）
    let current = unsafe { target.transform.GetOutputCurrentType(target.output) }?;
    let rate = unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }?;
    let channels = unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }?;
    if rate != AUDIO_SAMPLE_RATE || channels != u32::from(AUDIO_CHANNELS) {
        return Err(windows::core::Error::empty());
    }
    Ok(())
}

/// エンコーダが入力に示す形のうち、`subtype` のもの。
fn offered_type(
    transform: &IMFTransform,
    stream: u32,
    subtype: &GUID,
) -> windows::core::Result<IMFMediaType> {
    let mut index = 0;
    loop {
        let media_type = unsafe { transform.GetInputAvailableType(stream, index) }?;
        if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.as_ref() == Ok(subtype) {
            return Ok(media_type);
        }
        index += 1;
    }
}

/// 入力と出力のストリームの番号。固定のもの（`E_NOTIMPL`）は 0。
fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32; 1];
    let mut output = [0u32; 1];
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}

/// 候補を先頭から開き、最初に組み立てられたものを返す。倒したことは `warn` で残す
/// （失敗ではない経過なので、録画スレッドが自分で残してよい）。
fn open_first(
    candidates: Vec<(IMFActivate, bool)>,
    mut open: impl FnMut(IMFActivate, bool) -> Result<EncoderMft, EncoderError>,
) -> Result<EncoderMft, EncoderError> {
    let mut last_error = EncoderError::NotFound;
    for (activate, hardware) in candidates {
        let name = activate
            .cast::<IMFAttributes>()
            .ok()
            .and_then(|attributes| allocated_string(&attributes, &MFT_FRIENDLY_NAME_Attribute));
        match open(activate, hardware) {
            Ok(encoder) => return Ok(encoder),
            Err(error) => {
                warn!(
                    "エンコーダ {}（ハードウェア: {}）を組み立てられないので次を試す: {}",
                    name.as_deref().unwrap_or("（名前不明）"),
                    if hardware { "はい" } else { "いいえ" },
                    error
                );
                last_error = error;
            }
        }
    }
    Err(last_error)
}

/// 条件に合うエンコーダ MFT を並べる。
fn enumerate(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    (input_major, input_subtype): (GUID, GUID),
    (output_major, output_subtype): (GUID, GUID),
) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: input_major,
        guidSubtype: input_subtype,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: output_major,
        guidSubtype: output_subtype,
    };
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    let enumerated = unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    };
    if enumerated.is_err() || activates.is_null() {
        return Vec::new();
    }
    // SAFETY: MFTEnumEx が `count` 個の要素を CoTaskMemAlloc で返している。
    // 要素は 1 つずつ引き取り、配列そのものは最後に解放する
    let found = (0..count as usize)
        .filter_map(|index| unsafe { (*activates.add(index)).take() })
        .collect();
    unsafe { CoTaskMemFree(Some(activates as *const core::ffi::c_void)) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::{ComApartment, ComModel, MfPlatform};
    use crate::recording::convert::rgb_to_nv12;
    use crate::recording::writer::memory_sample;

    fn params(hardware: bool) -> WriterParams {
        WriterParams {
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 2000,
            hardware,
            audio_bitrate_kbps: None,
        }
    }

    /// 3 秒ぶん（90 枚）をエンコードし、出力を集める。
    fn encode_three_seconds(encoder: &mut EncoderMft) -> Vec<EncodedSample> {
        let mut nv12 = Vec::new();
        let mut outputs = Vec::new();
        for index in 0..90u8 {
            let rgb = vec![index.wrapping_mul(3); 320 * 240 * 3];
            assert!(rgb_to_nv12(
                &rgb,
                320,
                240,
                320,
                240,
                params(false).matrix(),
                &mut nv12
            ));
            let pts = i64::from(index) * 333_333;
            let sample = memory_sample(&nv12, pts, 333_333).expect("サンプルを作れる");
            // 非同期型は入力を求められるまで待つ（イベントは数 ms で届く）
            let mut waited = 0;
            while !encoder.accepts_input().expect("イベントを読める") && waited < 1000 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                waited += 1;
            }
            encoder.encode(&sample).expect("エンコードできる");
            outputs.extend(encoder.take_output());
        }
        // 非同期型は出力が遅れて届く
        for _ in 0..200 {
            encoder.pull().expect("取り出せる");
            outputs.extend(encoder.take_output());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        outputs
    }

    fn check_h264_outputs(encoder: &EncoderMft, outputs: &[EncodedSample]) {
        assert!(outputs.len() >= 60, "出力が {} 枚しか無い", outputs.len());
        assert!(outputs[0].keyframe, "最初の出力はキーフレーム");
        // 2 秒ごと（60 枚ごと）にキーフレームがある
        let keyframes: Vec<i64> = outputs
            .iter()
            .filter(|sample| sample.keyframe)
            .map(|sample| sample.pts)
            .collect();
        assert!(keyframes.len() >= 2, "キーフレーム: {keyframes:?}");
        assert!(
            (keyframes[1] - keyframes[0] - 20_000_000).abs() < 1_000_000,
            "キーフレームの間隔: {keyframes:?}"
        );
        // 時刻は増えていく（B フレームが無い）
        assert!(outputs.windows(2).all(|pair| pair[0].pts < pair[1].pts));
        let stream_type = encoder.stream_type().expect("Sink Writer へ渡す形を作れる");
        assert!(unsafe { stream_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.unwrap_or(0) > 0);
    }

    #[test]
    #[ignore = "Media Foundation の H.264 エンコーダが必要（CI のランナーにあるかは未確認）"]
    fn software_h264_encoder_emits_keyframes_every_two_seconds() {
        // 実行: cargo test -- --ignored software_h264_encoder_emits_keyframes_every_two_seconds
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let mut encoder = EncoderMft::video(&params(false)).expect("エンコーダを作れる");
        assert!(!encoder.is_hardware());
        let outputs = encode_three_seconds(&mut encoder);
        check_h264_outputs(&encoder, &outputs);
    }

    #[test]
    #[ignore = "ハードウェアの H.264 エンコーダ（GPU）が必要"]
    fn hardware_h264_encoder_emits_keyframes_every_two_seconds() {
        // 実行: cargo test -- --ignored hardware_h264_encoder_emits_keyframes_every_two_seconds
        // 非同期型（ハードウェアの MFT）の入出力を通すためのテスト。ハードウェアの MFT が
        // 無い環境ではソフトウェアへ倒れるので、そこで落とす（倒れたまま通すと何も確かめていない）
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let mut encoder = EncoderMft::video(&params(true)).expect("エンコーダを作れる");
        assert!(
            encoder.is_hardware(),
            "ハードウェアの H.264 エンコーダが無いか、組み立てられずにソフトウェアへ倒れた: {:?}",
            encoder.info()
        );
        let outputs = encode_three_seconds(&mut encoder);
        check_h264_outputs(&encoder, &outputs);
    }

    #[test]
    #[ignore = "Media Foundation の AAC エンコーダが必要（CI のランナーにあるかは未確認）"]
    fn aac_encoder_emits_frames_with_user_data() {
        // 実行: cargo test -- --ignored aac_encoder_emits_frames_with_user_data
        let _com = ComApartment::enter(ComModel::MultiThreaded).expect("COM を初期化できる");
        let _mf = MfPlatform::start().expect("MF を起こせる");
        let mut encoder = EncoderMft::audio(160).expect("エンコーダを作れる");
        let mut outputs = Vec::new();
        // 1 秒ぶんを 100ms ずつ
        for chunk in 0..10i64 {
            let pcm = vec![0u8; 4_800 * 4];
            let sample = memory_sample(&pcm, chunk * 1_000_000, 1_000_000).expect("作れる");
            encoder.encode(&sample).expect("エンコードできる");
            outputs.extend(encoder.take_output());
        }
        // AAC は 1024 フレーム（約 21.3ms）ずつ。1 秒で 40 個以上
        assert!(outputs.len() >= 40, "出力が {} 個しか無い", outputs.len());
        assert!(outputs.iter().all(|sample| sample.keyframe));
        assert!(outputs.windows(2).all(|pair| pair[0].pts < pair[1].pts));
        let stream_type = encoder.stream_type().expect("Sink Writer へ渡す形を作れる");
        assert!(unsafe { stream_type.GetBlobSize(&MF_MT_USER_DATA) }.unwrap_or(0) >= 14);
    }
}
