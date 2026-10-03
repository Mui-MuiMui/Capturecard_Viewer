//! 映像デバイスの音声ピン（#388）。グラフに音声ピンがあるかの記録、繋ぐ指定が
//! あるときの塊の長さの提案と接続、音声のレンダラーが受け取った PCM を
//! `AudioPinFeed` へ渡す部分。
//!
//! キャプチャーフィルターが映像ピンと並べて持つ音声ピンを、**映像と同じグラフの
//! 中で**自前のレンダラー（`filter.rs`。映像とは別のインスタンス）へ繋ぐ。
//! 音声ピンだけを別のグラフで開く経路は無い（`docs/design/directshow-audio.md`）。
//!
//! **どこで失敗しても映像は止めない。** 繋げなければ音声のレンダラーを外し、
//! 理由を `PinFailure` に残して映像だけで動かす。繋いだせいで `Run` が通らない
//! ときも、音声のレンダラーを外して 1 度だけやり直す（`run_with_fallback`）。
//!
//! 接続と問い合わせはデバイスワーカースレッドから、`AudioStream::receive` は
//! キャプチャーフィルターが音声ピンのために持つストリーミングスレッドから
//! 呼ばれる。後者ではロックを待たず、確保もしない（`AudioPinFeed::push`）。

use std::mem::{size_of, ManuallyDrop};
use std::ptr;

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::DirectShow::{
    IAMBufferNegotiation, IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2, IGraphBuilder,
    IMediaControl, IMediaSample, IPin, ALLOCATOR_PROPERTIES, AUDIO_STREAM_CONFIG_CAPS,
    PINDIR_OUTPUT, PIN_INFO,
};
use windows::Win32::Media::MediaFoundation::{
    FORMAT_WaveFormatEx, MEDIATYPE_Audio, AM_MEDIA_TYPE, PIN_CATEGORY_CAPTURE,
};

use super::devices::{bind_filter, DeviceEntry};
use super::filter::Renderer;
use super::media_type::delete_media_type;
use crate::audio::{AudioPinFeed, AudioPinPresence, PinFailure, PinFormat, PinSampleType};

/// 音声ピンに提案する 1 塊の長さ（ms）。このアプリの前提（パススルーの
/// リングと録画の PTS）は 10ms 前後の塊で成り立っている（設計の「塊の長さ」）
const SUGGESTED_CHUNK_MS: u32 = 10;

/// `WAVEFORMATEX` の形式の番号
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `WAVEFORMATEX` の大きさ（`cbSize` まで、詰め物なし）
const WAVEFORMATEX_LEN: usize = 18;
/// `WAVEFORMATEXTENSIBLE` の大きさ
const WAVEFORMATEXTENSIBLE_LEN: usize = 40;

/// `KSDATAFORMAT_SUBTYPE_PCM` などの GUID のうち、先頭の 4 バイト（形式の番号）を
/// 除いた残り（`XXXXXXXX-0000-0010-8000-00AA00389B71`）
const KSDATAFORMAT_SUBTYPE_SUFFIX: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// `WAVEFORMATEX`（と `WAVEFORMATEXTENSIBLE`）のバイト列を読んで、受け取れる
/// 形式ならその形を返す。
///
/// 受け取るのは 16bit の整数 PCM と 32bit の浮動小数点、それらを
/// `WAVE_FORMAT_EXTENSIBLE` で包んだもので、チャンネル数は 1〜8
/// （`docs/design/directshow-audio.md` の (1)）。バイト列の並び（アラインメント）は
/// 前提にしない。
pub(super) fn pin_format_from_wave(bytes: &[u8]) -> Option<PinFormat> {
    if bytes.len() < WAVEFORMATEX_LEN {
        return None;
    }
    let mut tag = u16_at(bytes, 0)?;
    let channels = u16_at(bytes, 2)?;
    let sample_rate = u32_at(bytes, 4)?;
    let block_align = u16_at(bytes, 12)?;
    let bits = u16_at(bytes, 14)?;
    if tag == WAVE_FORMAT_EXTENSIBLE {
        if bytes.len() < WAVEFORMATEXTENSIBLE_LEN {
            return None;
        }
        let valid_bits = u16_at(bytes, 18)?;
        // 入れ物より少ない有効ビット（24bit を 32bit に入れたものなど）は読まない
        if valid_bits != 0 && valid_bits != bits {
            return None;
        }
        let sub_format = u32_at(bytes, 24)?;
        if bytes.get(28..40)? != KSDATAFORMAT_SUBTYPE_SUFFIX {
            return None;
        }
        tag = u16::try_from(sub_format).ok()?;
    }
    let sample_type = match (tag, bits) {
        (WAVE_FORMAT_PCM, 16) => PinSampleType::I16,
        (WAVE_FORMAT_IEEE_FLOAT, 32) => PinSampleType::F32,
        _ => return None,
    };
    if !(1..=8).contains(&channels) || sample_rate == 0 {
        return None;
    }
    let format = PinFormat {
        sample_rate,
        channels,
        sample_type,
    };
    // 1 フレームの長さが食い違うものは、読み出しの区切りがずれるので受け取らない
    if u32::from(block_align) != u32::from(channels) * format.bytes_per_sample() {
        return None;
    }
    Some(format)
}

/// メディアタイプを読んで、音声ピンから受け取れる形式ならその形を返す。
///
/// # Safety
/// `mt.pbFormat` は `mt.cbFormat` バイトを指していること（DirectShow から
/// 受け取ったものならそうなっている）。
pub(super) unsafe fn pin_format_of(mt: &AM_MEDIA_TYPE) -> Option<PinFormat> {
    if mt.majortype != MEDIATYPE_Audio
        || mt.formattype != FORMAT_WaveFormatEx
        || mt.pbFormat.is_null()
    {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(mt.pbFormat, mt.cbFormat as usize) };
    pin_format_from_wave(bytes)
}

/// 10ms の塊のバイト数。1 フレームの境界へ切り捨てる。
pub(super) fn suggested_chunk_bytes(format: PinFormat) -> u32 {
    let frame = u32::from(format.channels) * format.bytes_per_sample();
    let bytes = u64::from(format.bytes_per_second()) * u64::from(SUGGESTED_CHUNK_MS) / 1000;
    let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
    (bytes / frame.max(1) * frame).max(frame)
}

/// 音声のレンダラーのストリーミングスレッドだけが触る状態。
pub(super) struct AudioStream {
    feed: AudioPinFeed,
    /// このレンダラーを入れたグラフの番号。差し込み先の番号と合うときだけ積まれる
    graph: u64,
    /// いま流れてくるサンプルの形。接続時に決まり、流れの途中で変わることがある
    pub(super) format: Option<PinFormat>,
}

impl AudioStream {
    pub(super) fn new(feed: AudioPinFeed, graph: u64) -> Self {
        Self {
            feed,
            graph,
            format: None,
        }
    }

    /// サンプル 1 つを `AudioPinFeed` へ渡す。**ログも出さない**（確保が起きる）。
    pub(super) fn receive(&mut self, sample: &IMediaSample) {
        // 流れの途中で形式が変わると、そのサンプルに新しいメディアタイプが付いてくる。
        // 差し込み先は差し込んだときの形式と比べて、食い違えば積まない
        if let Ok(pmt) = unsafe { sample.GetMediaType() } {
            if !pmt.is_null() {
                self.format = unsafe { pin_format_of(&*pmt) };
                unsafe { delete_media_type(pmt) };
            }
        }
        let Some(format) = self.format else {
            return;
        };
        let Ok(data) = (unsafe { sample.GetPointer() }) else {
            return;
        };
        let len = unsafe { sample.GetActualDataLength() };
        if data.is_null() || len <= 0 {
            return;
        }
        let src = unsafe { std::slice::from_raw_parts(data, len as usize) };
        let discontinuity = unsafe { sample.IsDiscontinuity() } == S_OK;
        self.feed.push(self.graph, format, src, discontinuity);
    }
}

/// 音声ピンをどうするかの指定。`CaptureGraph::start` へ渡す。
pub(super) struct AudioPinRequest<'a> {
    /// 繋ぐか（`[audio] input_source = "video_pin"` のときだけ真）
    pub(super) connect: bool,
    pub(super) feed: &'a AudioPinFeed,
    /// このグラフの番号（`AudioPinFeed::begin_graph`）
    pub(super) graph: u64,
}

/// グラフを組んだときの音声ピンの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PinOutcome {
    /// 音声ピンが無い
    Missing,
    /// あるが繋いでいない
    Available,
    /// 繋いだ。`chunk_bytes` はアロケーターが決めた 1 塊のバイト数
    Connected {
        format: PinFormat,
        chunk_bytes: Option<u32>,
    },
    /// 繋げなかった（音声のレンダラーは外してある）
    Failed(PinFailure),
}

/// グラフに入れた音声のレンダラーと、その結果。
pub(super) struct AttachedAudio {
    pub(super) outcome: PinOutcome,
    /// 繋いでいれば音声のレンダラー。グラフを捨てるときに外す
    pub(super) renderer: Option<Renderer>,
    /// 繋いでいればキャプチャーフィルターの音声ピン。外すときに接続を切る
    pin: Option<IPin>,
}

/// 音声ピンを探す。キャプチャーのカテゴリで見つからなければ、カテゴリを問わず探す
/// （映像のピンと同じく、カテゴリを名乗らないフィルターがある）。
fn find_audio_pin(builder: &ICaptureGraphBuilder2, source: &IBaseFilter) -> Option<IPin> {
    for category in [Some(&PIN_CATEGORY_CAPTURE as *const GUID), None] {
        let found = unsafe {
            builder.FindPin(
                source,
                PINDIR_OUTPUT,
                category,
                Some(&MEDIATYPE_Audio),
                true,
                0,
            )
        };
        if let Ok(pin) = found {
            return Some(pin);
        }
    }
    None
}

/// 列挙の時点で、映像デバイスに音声ピンがあるかを調べる（#409）。
///
/// 名札からフィルターを作り、出力ピンが勧めるメディアタイプに音声があるかを
/// 見るだけで、グラフには入れず繋がない。`find_audio_pin` の「カテゴリを問わず
/// 探す」と同じ範囲を、組み立て役なしで見る（見つからないのか失敗したのかを
/// 分けるため）。途中で失敗したら「不明」。
pub(super) fn probe_presence(entry: &DeviceEntry) -> AudioPinPresence {
    let Ok(filter) = bind_filter(entry) else {
        return AudioPinPresence::Unknown;
    };
    match filter_has_audio_output(&filter) {
        Ok(true) => AudioPinPresence::Present,
        Ok(false) => AudioPinPresence::Absent,
        Err(_) => AudioPinPresence::Unknown,
    }
}

/// フィルターの出力ピンのどれかが、音声のメディアタイプを勧めるか。
fn filter_has_audio_output(filter: &IBaseFilter) -> windows::core::Result<bool> {
    let pins = unsafe { filter.EnumPins()? };
    loop {
        let mut slot = [None];
        let mut fetched = 0u32;
        if unsafe { pins.Next(&mut slot, Some(&mut fetched)) } != S_OK || fetched == 0 {
            return Ok(false);
        }
        let Some(pin) = slot[0].take() else {
            return Ok(false);
        };
        if unsafe { pin.QueryDirection()? } == PINDIR_OUTPUT && pin_offers_audio(&pin)? {
            return Ok(true);
        }
    }
}

/// ピンが勧めるメディアタイプに音声があるか。受け取ったメディアタイプは都度解放する。
fn pin_offers_audio(pin: &IPin) -> windows::core::Result<bool> {
    let types = unsafe { pin.EnumMediaTypes()? };
    loop {
        let mut slot = [ptr::null_mut::<AM_MEDIA_TYPE>()];
        let mut fetched = 0u32;
        if unsafe { types.Next(&mut slot, Some(&mut fetched)) } != S_OK || fetched == 0 {
            return Ok(false);
        }
        let mt = slot[0];
        if mt.is_null() {
            return Ok(false);
        }
        let audio = unsafe { (*mt).majortype } == MEDIATYPE_Audio;
        unsafe { delete_media_type(mt) };
        if audio {
            return Ok(true);
        }
    }
}

/// 音声ピンの対応形式をログへ出す。第 1 段では選ばない（`SetFormat` は第 3 段）。
fn log_stream_caps(config: &IAMStreamConfig) {
    let (mut count, mut size) = (0i32, 0i32);
    if unsafe { config.GetNumberOfCapabilities(&mut count, &mut size) }.is_err() {
        log::debug!("DirectShow の音声ピンの対応形式の数を読めない");
        return;
    }
    // 書き込み先は返ってきた大きさで用意する。GC551 の音声ピンは映像の
    // `VIDEO_STREAM_CONFIG_CAPS` と同じ 128 バイトを返す（実機で確かめた）。
    // 読むのは先頭の `AUDIO_STREAM_CONFIG_CAPS` の分だけで、足りなければ読まない
    let Ok(size) = usize::try_from(size) else {
        return;
    };
    if size < size_of::<AUDIO_STREAM_CONFIG_CAPS>() || size > 4096 {
        log::debug!(
            "DirectShow の音声ピンの対応形式の構造体の大きさ（{} バイト）が想定と違うので読まない",
            size
        );
        return;
    }
    let mut buffer = vec![0u8; size];
    for index in 0..count {
        let mut pmt: *mut AM_MEDIA_TYPE = ptr::null_mut();
        let read = unsafe { config.GetStreamCaps(index, &mut pmt, buffer.as_mut_ptr()) };
        if read.is_err() || pmt.is_null() {
            continue;
        }
        // 書き込み先の揃え（アラインメント）は保証しないので読み出しで写す
        let caps =
            unsafe { ptr::read_unaligned(buffer.as_ptr() as *const AUDIO_STREAM_CONFIG_CAPS) };
        let format = unsafe { pin_format_of(&*pmt) };
        unsafe { delete_media_type(pmt) };
        log::debug!(
            "DirectShow の音声ピンの対応形式 {}: {:?}（{}〜{}ch、{}〜{}bit、{}〜{}Hz）",
            index,
            format.map(PinFormat::summary),
            caps.MinimumChannels,
            caps.MaximumChannels,
            caps.MinimumBitsPerSample,
            caps.MaximumBitsPerSample,
            caps.MinimumSampleFrequency,
            caps.MaximumSampleFrequency
        );
    }
}

/// 音声ピンの今の形式を読み、1 塊を 10ms にするよう提案する。
///
/// **繋ぐ前に呼ぶ**（アロケーターは接続のときに決まる）。提案が通るかは
/// フィルター次第で、通らなくても繋ぐ（塊が長ければ音声側がリングを広げる）。
fn suggest_chunk_length(pin: &IPin) {
    let format = pin.cast::<IAMStreamConfig>().ok().and_then(|config| {
        log_stream_caps(&config);
        let pmt = unsafe { config.GetFormat() }.ok()?;
        if pmt.is_null() {
            return None;
        }
        let format = unsafe { pin_format_of(&*pmt) };
        unsafe { delete_media_type(pmt) };
        format
    });
    let Some(format) = format else {
        log::debug!("DirectShow の音声ピンの今の形式を読めないので、塊の長さを提案しない");
        return;
    };
    let bytes = suggested_chunk_bytes(format);
    let negotiation = match pin.cast::<IAMBufferNegotiation>() {
        Ok(negotiation) => negotiation,
        Err(e) => {
            log::debug!(
                "DirectShow の音声ピンが IAMBufferNegotiation を持たないので、塊の長さを提案しない: {}",
                e
            );
            return;
        }
    };
    // -1 は「どれでもよい」
    let props = ALLOCATOR_PROPERTIES {
        cBuffers: -1,
        cbBuffer: i32::try_from(bytes).unwrap_or(i32::MAX),
        cbAlign: -1,
        cbPrefix: -1,
    };
    match unsafe { negotiation.SuggestAllocatorProperties(&props) } {
        Ok(()) => log::debug!(
            "DirectShow の音声ピンへ塊の長さ {} ms（{} バイト、{}）を提案した",
            SUGGESTED_CHUNK_MS,
            bytes,
            format.summary()
        ),
        Err(e) => log::debug!("DirectShow の音声ピンが塊の長さの提案を断った: {}", e),
    }
}

/// 音声ピンの有無を記録し、繋ぐ指定があれば音声のレンダラーへ繋ぐ。
///
/// 映像を繋いだあと、`Run` の前に呼ぶ。**失敗しても映像は止めない**（音声の
/// レンダラーを外して `Failed` を返す）。
pub(super) fn attach(
    graph: &IGraphBuilder,
    builder: &ICaptureGraphBuilder2,
    source: &IBaseFilter,
    request: AudioPinRequest<'_>,
) -> AttachedAudio {
    let unattached = |outcome| AttachedAudio {
        outcome,
        renderer: None,
        pin: None,
    };
    let Some(pin) = find_audio_pin(builder, source) else {
        log::debug!("DirectShow の映像デバイスに音声ピンが無い");
        return unattached(PinOutcome::Missing);
    };
    if !request.connect {
        log::debug!(
            "DirectShow の映像デバイスに音声ピンがあるが、入力の種類の指定が無いので繋がない"
        );
        return unattached(PinOutcome::Available);
    }
    suggest_chunk_length(&pin);

    let renderer = Renderer::audio(request.feed.clone(), request.graph);
    if let Err(e) = connect_renderer(graph, builder, source, &renderer) {
        log::warn!("DirectShow の音声ピンに繋げないので、映像だけで開く: {}", e);
        // 途中まで繋がった（変換フィルターだけ入った）ことがあるので、鎖ごと外す
        remove_chain(graph, &pin, &renderer.filter);
        return unattached(PinOutcome::Failed(PinFailure::Connect(e.to_string())));
    }
    let Some(format) = renderer.connected_audio_format() else {
        log::warn!("DirectShow の音声ピンと繋がった形式を読めないので、映像だけで開く");
        remove_chain(graph, &pin, &renderer.filter);
        return unattached(PinOutcome::Failed(PinFailure::Connect(
            windows::core::Error::from_hresult(
                windows::Win32::Media::DirectShow::VFW_E_NOT_CONNECTED,
            )
            .to_string(),
        )));
    };
    let buffers = renderer.allocator_buffers().or_else(|| {
        pin.cast::<IAMBufferNegotiation>()
            .ok()
            .and_then(|negotiation| unsafe { negotiation.GetAllocatorProperties() }.ok())
            .and_then(|props| {
                let bytes = u32::try_from(props.cbBuffer)
                    .ok()
                    .filter(|bytes| *bytes > 0)?;
                Some((bytes, props.cBuffers))
            })
    });
    let chunk_bytes = buffers.map(|(bytes, _)| bytes);
    log::info!(
        "音声ピンを繋いだ（形式: {}、塊: {}、バッファの数: {:?}、提案: {} ms）",
        format.summary(),
        match chunk_bytes.and_then(|bytes| format.chunk_ms(bytes).map(|ms| (bytes, ms))) {
            Some((bytes, ms)) => format!("{bytes} バイト = {ms} ms"),
            None => "不明（最初のサンプルの長さで見る）".to_string(),
        },
        buffers.map(|(_, count)| count),
        SUGGESTED_CHUNK_MS
    );
    AttachedAudio {
        outcome: PinOutcome::Connected {
            format,
            chunk_bytes,
        },
        renderer: Some(renderer),
        pin: Some(pin),
    }
}

/// 音声のレンダラーをグラフへ入れ、音声ピンと繋ぐ。直接繋がらなければ
/// DirectShow が間に変換フィルター（ACM Wrapper など）を挟もうとする。
fn connect_renderer(
    graph: &IGraphBuilder,
    builder: &ICaptureGraphBuilder2,
    source: &IBaseFilter,
    renderer: &Renderer,
) -> windows::core::Result<()> {
    unsafe {
        graph.AddFilter(
            &renderer.filter,
            windows::core::w!("Capturecard_Viewer Audio Renderer"),
        )?
    };
    let with_category = unsafe {
        builder.RenderStream(
            Some(&PIN_CATEGORY_CAPTURE),
            &MEDIATYPE_Audio,
            source,
            None::<&IBaseFilter>,
            &renderer.filter,
        )
    };
    if let Err(e) = with_category {
        log::debug!(
            "音声ピンをキャプチャーのカテゴリで繋げなかったので、カテゴリを問わず繋ぐ: {}",
            e
        );
        unsafe {
            builder.RenderStream(
                None,
                &MEDIATYPE_Audio,
                source,
                None::<&IBaseFilter>,
                &renderer.filter,
            )?
        };
    }
    Ok(())
}

/// グラフを動かす。**音声を繋いだせいで `Run` が通らないときは、音声の
/// レンダラーを外して 1 度だけやり直す**（設計の「`Run` が音声を繋いだせいで
/// 失敗したときの扱い」）。やり直しても失敗したら、今までどおり接続の失敗として返す。
pub(super) fn run_with_fallback(
    graph: &IGraphBuilder,
    control: &IMediaControl,
    audio: &mut AttachedAudio,
) -> windows::core::Result<()> {
    // Run は状態の遷移が済む前に S_FALSE で戻ることがある。失敗だけを見る
    let Err(first) = (unsafe { control.Run() }) else {
        return Ok(());
    };
    let Some(renderer) = audio.renderer.take() else {
        return Err(first);
    };
    log::warn!(
        "音声ピンを繋いだグラフを動かせないので、音声のレンダラーを外して映像だけでやり直す: {}",
        first
    );
    let _ = unsafe { control.Stop() };
    if let Some(pin) = audio.pin.take() {
        remove_chain(graph, &pin, &renderer.filter);
    } else {
        let _ = unsafe { graph.RemoveFilter(&renderer.filter) };
    }
    audio.outcome = PinOutcome::Failed(PinFailure::Run(first.to_string()));
    unsafe { control.Run() }
}

/// 繋いでいれば、音声ピンから音声のレンダラーまでを外す。グラフを捨てるとき
/// （`CaptureGraph` の `Drop` と組み立ての失敗）に、映像のフィルターより先に呼ぶ。
pub(super) fn detach(graph: &IGraphBuilder, audio: &mut AttachedAudio) {
    let Some(renderer) = audio.renderer.take() else {
        return;
    };
    match audio.pin.take() {
        Some(pin) => remove_chain(graph, &pin, &renderer.filter),
        None => {
            let _ = unsafe { graph.RemoveFilter(&renderer.filter) };
        }
    }
}

/// 音声ピンの接続を切り、間に挟まった変換フィルター（あれば）と音声の
/// レンダラーをグラフから外す。
///
/// **レンダラーを外すだけでは足りない。** `RenderStream` が ACM Wrapper などの
/// 変換フィルターを挟んでいると、キャプチャーフィルターの音声ピンはその変換
/// フィルターと繋がったまま残り、`Run` のやり直しでも音声ピンが動いてしまう。
/// 変換フィルターが 2 段以上でも、音声ピンを切れば残りへは何も流れない。
fn remove_chain(graph: &IGraphBuilder, pin: &IPin, renderer: &IBaseFilter) {
    if let Ok(peer) = unsafe { pin.ConnectedTo() } {
        let downstream = peer_filter(&peer);
        let _ = unsafe { graph.Disconnect(&peer) };
        let _ = unsafe { graph.Disconnect(pin) };
        // 繋がっていた相手がレンダラーでなければ変換フィルター。外すとその先の
        // 接続（レンダラーとの間）も切れる
        if let Some(filter) = downstream.filter(|filter| filter.as_raw() != renderer.as_raw()) {
            let _ = unsafe { graph.RemoveFilter(&filter) };
        }
    }
    let _ = unsafe { graph.RemoveFilter(renderer) };
}

/// ピンの持ち主のフィルター。
fn peer_filter(pin: &IPin) -> Option<IBaseFilter> {
    let mut info = PIN_INFO::default();
    unsafe { pin.QueryPinInfo(&mut info) }.ok()?;
    // 受け取ったフィルターは参照が 1 つ足されているので、ここで引き取って落とす
    ManuallyDrop::into_inner(info.pFilter)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WAVEFORMATEX` のバイト列を組み立てる
    fn wave(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block = channels * bits / 8;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        bytes.extend_from_slice(&block.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    /// `WAVEFORMATEXTENSIBLE` のバイト列を組み立てる
    fn extensible(sub_format: u32, channels: u16, bits: u16, valid_bits: u16) -> Vec<u8> {
        let mut bytes = wave(WAVE_FORMAT_EXTENSIBLE, channels, 48_000, bits);
        bytes[16..18].copy_from_slice(&22u16.to_le_bytes());
        bytes.extend_from_slice(&valid_bits.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&sub_format.to_le_bytes());
        bytes.extend_from_slice(&KSDATAFORMAT_SUBTYPE_SUFFIX);
        bytes
    }

    #[test]
    fn pin_format_from_wave_reads_the_gc551_format() {
        // GC551 の音声ピンの形（ffmpeg の実測: pcm_s16le 48000 Hz stereo）
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 2, 48_000, 16)),
            Some(PinFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_type: PinSampleType::I16,
            })
        );
    }

    #[test]
    fn pin_format_from_wave_reads_float() {
        let format = pin_format_from_wave(&wave(WAVE_FORMAT_IEEE_FLOAT, 1, 44_100, 32));
        assert_eq!(format.map(|f| f.sample_type), Some(PinSampleType::F32));
    }

    #[test]
    fn pin_format_from_wave_unwraps_extensible() {
        let pcm = pin_format_from_wave(&extensible(1, 2, 16, 16)).expect("16bit PCM");
        assert_eq!(pcm.sample_type, PinSampleType::I16);
        let float = pin_format_from_wave(&extensible(3, 6, 32, 0)).expect("32bit float");
        assert_eq!((float.sample_type, float.channels), (PinSampleType::F32, 6));
    }

    #[test]
    fn pin_format_from_wave_rejects_unsupported_depths() {
        // 24bit / 32bit 整数の PCM は第 3 段
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 2, 48_000, 24)),
            None
        );
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 2, 48_000, 32)),
            None
        );
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 2, 48_000, 8)),
            None
        );
        // 32bit の入れ物に 24bit を入れたもの
        assert_eq!(pin_format_from_wave(&extensible(1, 2, 32, 24)), None);
    }

    #[test]
    fn pin_format_from_wave_rejects_broken_headers() {
        assert_eq!(pin_format_from_wave(&[0; 10]), None);
        // チャンネル数は 1〜8
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 0, 48_000, 16)),
            None
        );
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 9, 48_000, 16)),
            None
        );
        assert_eq!(pin_format_from_wave(&wave(WAVE_FORMAT_PCM, 2, 0, 16)), None);
        // 1 フレームの長さが食い違う
        let mut bytes = wave(WAVE_FORMAT_PCM, 2, 48_000, 16);
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(pin_format_from_wave(&bytes), None);
        // 拡張形式なのに後ろが足りない・知らない GUID
        assert_eq!(
            pin_format_from_wave(&wave(WAVE_FORMAT_EXTENSIBLE, 2, 48_000, 16)),
            None
        );
        let mut unknown = extensible(1, 2, 16, 16);
        unknown[39] = 0;
        assert_eq!(pin_format_from_wave(&unknown), None);
        // MP3 など
        assert_eq!(pin_format_from_wave(&wave(0x0055, 2, 48_000, 16)), None);
    }

    #[test]
    fn suggested_chunk_bytes_is_ten_milliseconds_of_whole_frames() {
        let gc551 = PinFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: PinSampleType::I16,
        };
        assert_eq!(suggested_chunk_bytes(gc551), 1920);
        // 44.1kHz 2ch 16bit の 10ms は 1764 バイト（441 フレーム）
        let cd = PinFormat {
            sample_rate: 44_100,
            ..gc551
        };
        assert_eq!(suggested_chunk_bytes(cd), 1764);
        // 端数はフレームの境界へ切り捨てる（6ch float の 1 フレームは 24 バイト）
        let surround = PinFormat {
            sample_rate: 44_100,
            channels: 6,
            sample_type: PinSampleType::F32,
        };
        assert_eq!(suggested_chunk_bytes(surround) % 24, 0);
    }
}
