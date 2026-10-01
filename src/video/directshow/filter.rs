//! サンプルを受け取るための自前のレンダラーフィルター。
//!
//! `ISampleGrabber`（qedit.h）は現在の Windows SDK から消えているので使わず、
//! 入力ピンを 1 本だけ持つフィルターを `IBaseFilter` / `IPin` /
//! `IMemInputPin` の実装として書いている。上流（キャプチャーのフィルター、
//! 間に入る変換フィルター）が `IMemInputPin::Receive` でサンプルを渡してくる。
//!
//! **媒体を問わない作りにしてある。** ピンとフィルターの参照の数え方、
//! アロケーター、列挙、状態の切り替えはここ 1 か所に置き、媒体に固有な
//! 部分（受け取る形式の判定と、受け取ったサンプルの渡し先）だけを
//! `StreamState` の種類で分ける。映像の渡し先は `video_stream.rs`
//! （`FrameSink` へ渡す）。
//!
//! **`Receive` はグラフのストリーミングスレッドから呼ばれる。** ここでは
//! ロックもアロケーションもしない。渡し先（`StreamState`）を持つのは
//! ストリーミングスレッドだけなので、`Mutex` では包まず `StreamSlot`
//! （待たない旗）で守っている。**捨てるときも失敗を返さない**（フラッシュ中と
//! 停止中を除く）。`Receive` が失敗を返すと上流はストリームを止めてしまう。
//!
//! 接続・状態の切り替え・問い合わせ（`ReceiveConnection` / `Run` / `Stop` /
//! `QueryPinInfo` など）はデバイスワーカースレッドからグラフ経由で呼ばれる。
//! こちらは 1 回の接続につき数回しか通らないので、`Mutex` を使ってよい。
//!
//! フィルターとピンは互いを参照するが、**ピン → フィルターの参照は数えない**
//! （DirectShow の決まり。数えると循環して解放されない）。フィルターが
//! 消えるときにピン側の生ポインタを消す。グラフ → フィルターの参照
//! （`JoinFilterGraph`）も同じく数えない。

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{
    implement, ComObject, Error, Interface, OutRef, Ref, Result, GUID, HRESULT, PCWSTR, PWSTR,
};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    IBaseFilter, IBaseFilter_Impl, IEnumMediaTypes, IEnumMediaTypes_Impl, IEnumPins,
    IEnumPins_Impl, IFilterGraph, IMediaFilter_Impl, IMediaSample, IMemAllocator, IMemInputPin,
    IMemInputPin_Impl, IPin, IPin_Impl, State_Paused, State_Running, State_Stopped,
    ALLOCATOR_PROPERTIES, FILTER_INFO, FILTER_STATE, PINDIR_INPUT, PIN_DIRECTION, PIN_INFO,
    VFW_E_ALREADY_CONNECTED, VFW_E_NOT_CONNECTED, VFW_E_NOT_FOUND, VFW_E_NOT_STOPPED,
    VFW_E_NO_CLOCK, VFW_E_TYPE_NOT_ACCEPTED, VFW_E_WRONG_STATE,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::MediaFoundation::{CLSID_MemoryAllocator, AM_MEDIA_TYPE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemAlloc, IPersist_Impl, CLSCTX_INPROC_SERVER,
};

use super::audio_pin::{pin_format_of, AudioStream};
use super::media_type::{sample_format_of, OwnedMediaType, SampleFormat};
use super::video_stream::VideoStream;
use crate::audio::{AudioPinFeed, PinFormat};
use crate::video::frame_sink::FrameSink;

/// このフィルターのクラス ID。登録はしないので、`GetClassID` に答えるためだけの値
const RENDERER_CLSID: GUID = GUID::from_u128(0x6f3a8c21_4d2b_4e6a_9b1c_2f7d5e8a9c03);

/// 入力ピンの名前と ID
const PIN_NAME: &str = "In";

fn error(code: HRESULT) -> Error {
    // `Error::from(HRESULT)` はスレッドのエラー情報を取りに行くので使わない
    Error::from_hresult(code)
}

/// 文字列を `FILTER_INFO` / `PIN_INFO` の名前欄（128 文字、終端込み）へ写す
fn write_name(dst: &mut [u16; 128], name: &str) {
    dst.fill(0);
    for (slot, unit) in dst.iter_mut().take(127).zip(name.encode_utf16()) {
        *slot = unit;
    }
}

/// フィルターとピンが共有する状態。
#[derive(Default)]
struct Shared {
    /// `FILTER_STATE` の値
    state: AtomicI32,
    /// 上流が `BeginFlush` を呼んでから `EndFlush` を呼ぶまで
    flushing: AtomicBool,
}

impl Shared {
    fn state(&self) -> FILTER_STATE {
        FILTER_STATE(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: FILTER_STATE) {
        self.state.store(state.0, Ordering::Release);
    }
}

/// 入力ピンが受ける媒体。ピンを作るときに決まり、以後変わらない。
///
/// **`StreamState` とは別に持つ。** 受け取る形式の判定（`QueryAccept`）は
/// ストリーミングスレッドと同時に呼ばれうるので、`StreamSlot` を経由せずに
/// 読めるようにしておく。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Video,
    Audio,
}

/// 上流と接続したときの形式。媒体ごとに中身が違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectedFormat {
    Video(SampleFormat),
    Audio(PinFormat),
}

/// メディアタイプを読んで、ピンの媒体で受け取れる形式ならその形を返す。
///
/// # Safety
/// `sample_format_of` と同じ。
unsafe fn accepted_format(kind: MediaKind, mt: &AM_MEDIA_TYPE) -> Option<ConnectedFormat> {
    match kind {
        MediaKind::Video => unsafe { sample_format_of(mt) }.map(ConnectedFormat::Video),
        MediaKind::Audio => unsafe { pin_format_of(mt) }.map(ConnectedFormat::Audio),
    }
}

/// ストリーミングスレッドだけが触る状態。媒体ごとの渡し先。
enum StreamState {
    Video(VideoStream),
    Audio(AudioStream),
}

impl StreamState {
    /// 接続したときの形式を渡し先へ書く。媒体が食い違う組み合わせは来ない
    /// （`accepted_format` がピンの媒体で判定している）
    fn set_format(&mut self, format: ConnectedFormat) {
        match (self, format) {
            (StreamState::Video(stream), ConnectedFormat::Video(format)) => {
                stream.format = Some(format);
            }
            (StreamState::Audio(stream), ConnectedFormat::Audio(format)) => {
                stream.format = Some(format);
            }
            (StreamState::Video(_), ConnectedFormat::Audio(_))
            | (StreamState::Audio(_), ConnectedFormat::Video(_)) => {}
        }
    }

    /// サンプル 1 つを渡し先へ渡す。
    fn receive(&mut self, sample: &IMediaSample, received_at: Instant) {
        match self {
            StreamState::Video(stream) => stream.receive(sample, received_at),
            // 音声は受け取った時刻を使わない（録画の PTS は `AudioTap` が積んだ時刻で決まる）
            StreamState::Audio(stream) => stream.receive(sample),
        }
    }
}

/// `StreamState` を「待たない旗」で守る入れ物。
///
/// **`Receive` から `Mutex` を取らないためのもの。** 触るのは基本的に
/// ストリーミングスレッド 1 本だけで、例外は接続時に形式を書き込む
/// `ReceiveConnection`（ストリームが止まっている間しか呼ばれない）。
/// 万一同時に来たら、後から来た側は待たずに諦める（サンプルなら 1 枚捨てる）。
struct StreamSlot {
    busy: AtomicBool,
    state: UnsafeCell<StreamState>,
}

impl StreamSlot {
    fn new(state: StreamState) -> Self {
        Self {
            busy: AtomicBool::new(false),
            state: UnsafeCell::new(state),
        }
    }

    /// 空いていれば中身を触らせる。使用中なら `None`
    fn try_with<R>(&self, f: impl FnOnce(&mut StreamState) -> R) -> Option<R> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        // SAFETY: 旗を立てられたのは自分だけなので、ほかに参照は無い
        let result = f(unsafe { &mut *self.state.get() });
        self.busy.store(false, Ordering::Release);
        Some(result)
    }
}

/// 接続先と、接続したときのメディアタイプ。
struct Connection {
    peer: IPin,
    media_type: OwnedMediaType,
    format: ConnectedFormat,
}

/// 入力ピン。`IMemInputPin` も同じオブジェクトが持つ（上流は `IPin` から
/// `QueryInterface` で取りに来る）。
#[implement(IPin, IMemInputPin)]
struct InputPin {
    /// 持ち主のフィルター（`IBaseFilter` の生ポインタ）。**参照は数えない。**
    /// フィルターが消えるときに null へ戻る
    filter: AtomicPtr<c_void>,
    /// 受ける媒体。受け取る形式の判定に使う
    kind: MediaKind,
    shared: Arc<Shared>,
    connection: Mutex<Option<Connection>>,
    allocator: Mutex<Option<IMemAllocator>>,
    stream: StreamSlot,
}

impl InputPin {
    fn is_stopped(&self) -> bool {
        self.shared.state() == State_Stopped
    }

    fn with_connection<R>(&self, f: impl FnOnce(&Option<Connection>) -> R) -> R {
        match self.connection.lock() {
            Ok(guard) => f(&guard),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }

    /// `Receive` の本体。**ストリーミングスレッドから呼ばれる。ロックも
    /// アロケーションもしない**（`Error::from_hresult` は確保しない）。
    fn receive_sample(&self, sample: &IMediaSample) -> Result<()> {
        if self.shared.flushing.load(Ordering::Acquire) {
            // フラッシュ中は受け取らない（S_FALSE で上流に送るのを止めさせる）
            return Err(error(S_FALSE));
        }
        if self.is_stopped() {
            return Err(error(VFW_E_WRONG_STATE));
        }
        let received_at = Instant::now();
        // 使用中（接続し直しの最中）なら、このサンプルは捨てる。待たない
        self.stream
            .try_with(|state| state.receive(sample, received_at));
        Ok(())
    }

    fn set_connection(&self, value: Option<Connection>) -> Option<Connection> {
        match self.connection.lock() {
            Ok(mut guard) => std::mem::replace(&mut *guard, value),
            Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), value),
        }
    }
}

impl IPin_Impl for InputPin_Impl {
    fn Connect(&self, _receive_pin: Ref<IPin>, _pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        // 接続を始めるのは出力ピンの役目
        Err(error(E_UNEXPECTED))
    }

    fn ReceiveConnection(&self, connector: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        let peer = connector.ok()?.clone();
        if pmt.is_null() {
            return Err(error(E_POINTER));
        }
        if self.with_connection(Option::is_some) {
            return Err(error(VFW_E_ALREADY_CONNECTED));
        }
        if !self.is_stopped() {
            return Err(error(VFW_E_NOT_STOPPED));
        }
        let mt = unsafe { &*pmt };
        let Some(format) = (unsafe { accepted_format(self.kind, mt) }) else {
            return Err(error(VFW_E_TYPE_NOT_ACCEPTED));
        };
        // 止まっている間なので、ストリーミングスレッドとは競合しない
        if self
            .stream
            .try_with(|state| state.set_format(format))
            .is_none()
        {
            return Err(error(E_UNEXPECTED));
        }
        log::debug!("DirectShow の上流と接続した: {:?}", format);
        self.set_connection(Some(Connection {
            peer,
            media_type: unsafe { OwnedMediaType::copy_from(mt) },
            format,
        }));
        Ok(())
    }

    fn Disconnect(&self) -> Result<()> {
        if !self.is_stopped() {
            return Err(error(VFW_E_NOT_STOPPED));
        }
        match self.set_connection(None) {
            Some(_) => Ok(()),
            // 繋がっていなければ S_FALSE
            None => Err(error(S_FALSE)),
        }
    }

    fn ConnectedTo(&self) -> Result<IPin> {
        self.with_connection(|connection| {
            connection
                .as_ref()
                .map(|connection| connection.peer.clone())
                .ok_or_else(|| error(VFW_E_NOT_CONNECTED))
        })
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> Result<()> {
        if pmt.is_null() {
            return Err(error(E_POINTER));
        }
        let out = unsafe { &mut *pmt };
        self.with_connection(|connection| match connection {
            Some(connection) => {
                if connection.media_type.write_to(out) {
                    Ok(())
                } else {
                    Err(error(windows::Win32::Foundation::E_OUTOFMEMORY))
                }
            }
            None => {
                // 繋がっていないときは中身を空にして返す決まり
                *out = AM_MEDIA_TYPE::default();
                Err(error(VFW_E_NOT_CONNECTED))
            }
        })
    }

    fn QueryPinInfo(&self, pinfo: *mut PIN_INFO) -> Result<()> {
        if pinfo.is_null() {
            return Err(error(E_POINTER));
        }
        let raw = self.filter.load(Ordering::Acquire);
        // 返すフィルターは参照を 1 つ足して渡す（受け取った側が解放する）
        let filter = unsafe { IBaseFilter::from_raw_borrowed(&raw) }.cloned();
        let info = unsafe { &mut *pinfo };
        info.pFilter = ManuallyDrop::new(filter);
        info.dir = PINDIR_INPUT;
        write_name(&mut info.achName, PIN_NAME);
        Ok(())
    }

    fn QueryDirection(&self) -> Result<PIN_DIRECTION> {
        Ok(PINDIR_INPUT)
    }

    fn QueryId(&self) -> Result<PWSTR> {
        // 受け取った側が CoTaskMemFree する
        let units: Vec<u16> = PIN_NAME.encode_utf16().chain(Some(0)).collect();
        let block = unsafe { CoTaskMemAlloc(units.len() * 2) } as *mut u16;
        if block.is_null() {
            return Err(error(windows::Win32::Foundation::E_OUTOFMEMORY));
        }
        unsafe { ptr::copy_nonoverlapping(units.as_ptr(), block, units.len()) };
        Ok(PWSTR(block))
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        if pmt.is_null() {
            return E_POINTER;
        }
        if unsafe { accepted_format(self.kind, &*pmt) }.is_some() {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn EnumMediaTypes(&self) -> Result<IEnumMediaTypes> {
        // 自分から勧める形式は無い。上流の形式を受けるかどうかだけを答える
        Ok(EmptyMediaTypes.into())
    }

    fn QueryInternalConnections(&self, _pins: OutRef<IPin>, _count: *mut u32) -> Result<()> {
        Err(error(E_NOTIMPL))
    }

    fn EndOfStream(&self) -> Result<()> {
        Ok(())
    }

    fn BeginFlush(&self) -> Result<()> {
        self.shared.flushing.store(true, Ordering::Release);
        Ok(())
    }

    fn EndFlush(&self) -> Result<()> {
        self.shared.flushing.store(false, Ordering::Release);
        Ok(())
    }

    fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> Result<()> {
        Ok(())
    }
}

impl IMemInputPin_Impl for InputPin_Impl {
    fn GetAllocator(&self) -> Result<IMemAllocator> {
        // 上流が自前のアロケーターを持たない場合に備えて、標準のものを渡す
        // （DirectShow の基底クラスの CBaseInputPin と同じ振る舞い）
        let mut guard = self.allocator.lock().map_err(|_| error(E_UNEXPECTED))?;
        if let Some(allocator) = guard.as_ref() {
            return Ok(allocator.clone());
        }
        let allocator: IMemAllocator =
            unsafe { CoCreateInstance(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)? };
        *guard = Some(allocator.clone());
        Ok(allocator)
    }

    fn NotifyAllocator(
        &self,
        allocator: Ref<IMemAllocator>,
        _read_only: windows::core::BOOL,
    ) -> Result<()> {
        let allocator = allocator.ok()?.clone();
        let mut guard = self.allocator.lock().map_err(|_| error(E_UNEXPECTED))?;
        *guard = Some(allocator);
        Ok(())
    }

    fn GetAllocatorRequirements(&self) -> Result<ALLOCATOR_PROPERTIES> {
        Err(error(E_NOTIMPL))
    }

    fn Receive(&self, sample: Ref<IMediaSample>) -> Result<()> {
        self.receive_sample(sample.ok()?)
    }

    fn ReceiveMultiple(&self, samples: *const Option<IMediaSample>, count: i32) -> Result<i32> {
        if samples.is_null() || count < 0 {
            return Err(error(E_POINTER));
        }
        let samples = unsafe { std::slice::from_raw_parts(samples, count as usize) };
        let mut processed = 0;
        for sample in samples {
            let sample = sample.as_ref().ok_or_else(|| error(E_POINTER))?;
            self.receive_sample(sample)?;
            processed += 1;
        }
        Ok(processed)
    }

    fn ReceiveCanBlock(&self) -> Result<()> {
        // `Receive` は待たない（S_FALSE）
        Err(error(S_FALSE))
    }
}

/// レンダラーフィルター本体。
#[implement(IBaseFilter)]
struct RendererFilter {
    shared: Arc<Shared>,
    pin: ComObject<InputPin>,
    /// 参加しているグラフ（`IFilterGraph` の生ポインタ）。**参照は数えない**
    graph: AtomicPtr<c_void>,
    name: Mutex<[u16; 128]>,
    clock: Mutex<Option<IReferenceClock>>,
}

impl Drop for RendererFilter {
    fn drop(&mut self) {
        // ピンがフィルターより長生きしても、消えたフィルターを指さないように
        self.pin.filter.store(ptr::null_mut(), Ordering::Release);
    }
}

impl IPersist_Impl for RendererFilter_Impl {
    fn GetClassID(&self) -> Result<GUID> {
        Ok(RENDERER_CLSID)
    }
}

impl IMediaFilter_Impl for RendererFilter_Impl {
    fn Stop(&self) -> Result<()> {
        self.shared.set_state(State_Stopped);
        Ok(())
    }

    fn Pause(&self) -> Result<()> {
        self.shared.set_state(State_Paused);
        Ok(())
    }

    fn Run(&self, _start: i64) -> Result<()> {
        self.shared.set_state(State_Running);
        Ok(())
    }

    fn GetState(&self, _timeout_ms: u32) -> Result<FILTER_STATE> {
        Ok(self.shared.state())
    }

    fn SetSyncSource(&self, clock: Ref<IReferenceClock>) -> Result<()> {
        // 時計は持つだけで使わない。届いたサンプルは待たずにすぐ積む
        let clock = clock.as_ref().cloned();
        let mut guard = self.clock.lock().map_err(|_| error(E_UNEXPECTED))?;
        *guard = clock;
        Ok(())
    }

    fn GetSyncSource(&self) -> Result<IReferenceClock> {
        let guard = self.clock.lock().map_err(|_| error(E_UNEXPECTED))?;
        guard.clone().ok_or_else(|| error(VFW_E_NO_CLOCK))
    }
}

impl IBaseFilter_Impl for RendererFilter_Impl {
    fn EnumPins(&self) -> Result<IEnumPins> {
        Ok(PinEnumerator {
            pins: vec![self.pin.to_interface::<IPin>()],
            position: AtomicUsize::new(0),
        }
        .into())
    }

    fn FindPin(&self, id: &PCWSTR) -> Result<IPin> {
        if id.is_null() {
            return Err(error(E_POINTER));
        }
        let requested = unsafe { id.to_string() }.map_err(|_| error(VFW_E_NOT_FOUND))?;
        if requested == PIN_NAME {
            Ok(self.pin.to_interface::<IPin>())
        } else {
            Err(error(VFW_E_NOT_FOUND))
        }
    }

    fn QueryFilterInfo(&self, pinfo: *mut FILTER_INFO) -> Result<()> {
        if pinfo.is_null() {
            return Err(error(E_POINTER));
        }
        let raw = self.graph.load(Ordering::Acquire);
        let graph = unsafe { IFilterGraph::from_raw_borrowed(&raw) }.cloned();
        let info = unsafe { &mut *pinfo };
        info.achName = match self.name.lock() {
            Ok(guard) => *guard,
            Err(_) => [0; 128],
        };
        info.pGraph = ManuallyDrop::new(graph);
        Ok(())
    }

    fn JoinFilterGraph(&self, graph: Ref<IFilterGraph>, name: &PCWSTR) -> Result<()> {
        // グラフへの参照は数えない（DirectShow の決まり）
        let raw = graph
            .as_ref()
            .map_or(ptr::null_mut(), |graph| graph.as_raw());
        self.graph.store(raw, Ordering::Release);
        let text = if name.is_null() {
            String::new()
        } else {
            unsafe { name.to_string() }.unwrap_or_default()
        };
        if let Ok(mut guard) = self.name.lock() {
            write_name(&mut guard, &text);
        }
        Ok(())
    }

    fn QueryVendorInfo(&self) -> Result<PWSTR> {
        Err(error(E_NOTIMPL))
    }
}

/// `IEnumPins`。ピンは 1 本だけ。
#[implement(IEnumPins)]
struct PinEnumerator {
    pins: Vec<IPin>,
    position: AtomicUsize,
}

impl IEnumPins_Impl for PinEnumerator_Impl {
    fn Next(&self, count: u32, pins: *mut Option<IPin>, fetched: *mut u32) -> HRESULT {
        if pins.is_null() || (count > 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let mut written = 0u32;
        while written < count {
            let position = self.position.load(Ordering::Acquire);
            let Some(pin) = self.pins.get(position) else {
                break;
            };
            unsafe { pins.add(written as usize).write(Some(pin.clone())) };
            self.position.store(position + 1, Ordering::Release);
            written += 1;
        }
        if !fetched.is_null() {
            unsafe { fetched.write(written) };
        }
        if written == count {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> Result<()> {
        let position = self.position.load(Ordering::Acquire) + count as usize;
        if position > self.pins.len() {
            self.position.store(self.pins.len(), Ordering::Release);
            return Err(error(S_FALSE));
        }
        self.position.store(position, Ordering::Release);
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.position.store(0, Ordering::Release);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumPins> {
        Ok(PinEnumerator {
            pins: self.pins.clone(),
            position: AtomicUsize::new(self.position.load(Ordering::Acquire)),
        }
        .into())
    }
}

/// `IEnumMediaTypes`。勧める形式が無いので常に空。
#[implement(IEnumMediaTypes)]
struct EmptyMediaTypes;

impl IEnumMediaTypes_Impl for EmptyMediaTypes_Impl {
    fn Next(&self, count: u32, _types: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        if !fetched.is_null() {
            unsafe { fetched.write(0) };
        }
        if count == 0 {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> Result<()> {
        if count == 0 {
            Ok(())
        } else {
            Err(error(S_FALSE))
        }
    }

    fn Reset(&self) -> Result<()> {
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumMediaTypes> {
        Ok(EmptyMediaTypes.into())
    }
}

/// グラフへ入れるレンダラー。フィルターと、接続結果を読むためのピンの控え。
pub(super) struct Renderer {
    pub(super) filter: IBaseFilter,
    pin: ComObject<InputPin>,
}

impl Renderer {
    /// 映像のレンダラー。`sink` はストリーミングスレッドへ渡る
    pub(super) fn video(sink: FrameSink) -> Self {
        Self::new(MediaKind::Video, StreamState::Video(VideoStream::new(sink)))
    }

    /// 音声のレンダラー。受け取った PCM を `feed` へ、グラフの番号 `graph` を
    /// 添えて渡す（`AudioPinFeed::push`）
    pub(super) fn audio(feed: AudioPinFeed, graph: u64) -> Self {
        Self::new(
            MediaKind::Audio,
            StreamState::Audio(AudioStream::new(feed, graph)),
        )
    }

    fn new(kind: MediaKind, stream: StreamState) -> Self {
        let shared = Arc::new(Shared::default());
        let pin = ComObject::new(InputPin {
            filter: AtomicPtr::new(ptr::null_mut()),
            kind,
            shared: shared.clone(),
            connection: Mutex::new(None),
            allocator: Mutex::new(None),
            stream: StreamSlot::new(stream),
        });
        let filter_object = ComObject::new(RendererFilter {
            shared,
            pin: pin.clone(),
            graph: AtomicPtr::new(ptr::null_mut()),
            name: Mutex::new([0; 128]),
            clock: Mutex::new(None),
        });
        let filter: IBaseFilter = filter_object.to_interface();
        pin.filter.store(filter.as_raw(), Ordering::Release);
        Self { filter, pin }
    }

    /// 上流と接続できていれば、そのときの形式
    fn connected_format(&self) -> Option<ConnectedFormat> {
        self.pin
            .with_connection(|connection| connection.as_ref().map(|c| c.format))
    }

    /// 映像のレンダラーが上流と接続できていれば、そのときの形式
    pub(super) fn connected_video_format(&self) -> Option<SampleFormat> {
        match self.connected_format()? {
            ConnectedFormat::Video(format) => Some(format),
            ConnectedFormat::Audio(_) => None,
        }
    }

    /// 音声のレンダラーが上流と接続できていれば、そのときの形式
    pub(super) fn connected_audio_format(&self) -> Option<PinFormat> {
        match self.connected_format()? {
            ConnectedFormat::Audio(format) => Some(format),
            ConnectedFormat::Video(_) => None,
        }
    }

    /// 接続で決まったアロケーターの 1 バッファのバイト数（= 1 塊の上限）。
    /// アロケーターが無い・読めないときは `None`
    pub(super) fn allocator_buffer_bytes(&self) -> Option<u32> {
        let allocator = self.pin.allocator.lock().ok()?.clone()?;
        let props = unsafe { allocator.GetProperties() }.ok()?;
        u32::try_from(props.cbBuffer)
            .ok()
            .filter(|bytes| *bytes > 0)
    }
}
