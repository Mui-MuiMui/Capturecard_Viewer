//! エンコーダ MFT の列挙と、候補から開く順番、入出力の形の組み立て（③ リプレイバッファ）。
//!
//! `EncoderMft`（`encoder.rs`）が作るときにだけ使う。**録画スレッドだけが触る。**

use std::ptr;

use log::warn;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFActivate,
    IMFAttributes, IMFMediaType, IMFTransform, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute,
    MFVideoFormat_NV12, MFT_ENUM_FLAG, MFT_REGISTER_TYPE_INFO, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use super::encoder::{EncoderError, EncoderMft};
use super::pts::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use super::writer::{
    allocated_string, audio_input_media_type, audio_output_media_type, input_media_type,
    output_media_type, pack, WriterParams,
};

/// 組み立ての関数へ渡す、MFT とストリームの番号。
pub(super) struct Transform<'a> {
    pub(super) transform: &'a IMFTransform,
    pub(super) input: u32,
    pub(super) output: u32,
}

/// H.264: キーフレームの間隔と B フレームの数を先に伝え、出力 → 入力の順に形を決める
/// （エンコーダは出力の形を先に求める）。
pub(super) fn configure_video(
    target: &Transform<'_>,
    params: &WriterParams,
) -> windows::core::Result<()> {
    // 受け付けないエンコーダもある。付かなくてもエンコードはできるので止めないが、リングは
    // キーフレームで捨てるので `warn` に残す。キーフレームは `force_keyframe` で補う（#313）
    match target.transform.cast::<ICodecAPI>() {
        Ok(codec) => {
            let gop = VARIANT::from(params.gop_size());
            if let Err(error) = unsafe { codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop) } {
                warn!(
                    "エンコーダがキーフレームの間隔（{} 枚）を受け付けない: {}",
                    params.gop_size(),
                    error
                );
            }
            let no_b_frames = VARIANT::from(0u32);
            let _ = unsafe { codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &no_b_frames) };
        }
        Err(error) => warn!(
            "エンコーダが ICodecAPI を持たないので、キーフレームの間隔を伝えられない: {}",
            error
        ),
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
pub(super) fn configure_audio(
    target: &Transform<'_>,
    bitrate_kbps: u32,
) -> windows::core::Result<()> {
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
pub(super) fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32; 1];
    let mut output = [0u32; 1];
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        Err(_) => (0, 0),
    }
}

/// 候補を先頭から開き、最初に組み立てられたものを返す。倒したことは `warn` で残す
/// （失敗ではない経過なので、録画スレッドが自分で残してよい）。
pub(super) fn open_first(
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
pub(super) fn enumerate(
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
