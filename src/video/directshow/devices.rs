//! DirectShow の映像入力デバイスの列挙と、対応形式の問い合わせ。
//!
//! 列挙は `ICreateDevEnum` の `CLSID_VideoInputDeviceCategory`、対応形式は
//! キャプチャーピンの `IAMStreamConfig::GetStreamCaps`。
//!
//! 対応形式の一覧から「どれで開くか」を決める判定と設定画面向けの並べ替えは
//! `stream_select.rs`（#414）。

use std::mem::size_of;
use std::ptr;

use windows::core::Interface;
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2, ICreateDevEnum, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory, MEDIATYPE_Video, AM_MEDIA_TYPE,
    PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CoCreateInstance, IBindCtx, IEnumMoniker, IMoniker, CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};

use super::media_type::{delete_media_type, fps_from_interval, sample_format_of, SampleFormat};
use super::stream_select::{fps_list, fps_range};

/// 列挙で見つかった 1 台。
pub(super) struct DeviceEntry {
    /// デバイスの表示名（`FriendlyName`）。「(DirectShow)」は付いていない
    pub(super) friendly_name: String,
    /// フィルターを作るための名札
    pub(super) moniker: IMoniker,
}

/// 映像入力デバイスを列挙する。1 台も無ければ空の一覧を返す。
///
/// 表示名を読めないものは飛ばす（選びようがないため）。
pub(super) fn enumerate() -> windows::core::Result<Vec<DeviceEntry>> {
    let dev_enum: ICreateDevEnum =
        unsafe { CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)? };
    let mut monikers: Option<IEnumMoniker> = None;
    // カテゴリにデバイスが 1 つも無いときは S_FALSE で、列挙子も返らない
    unsafe {
        dev_enum.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut monikers, 0)?;
    }
    let Some(monikers) = monikers else {
        return Ok(Vec::new());
    };

    let mut devices = Vec::new();
    loop {
        let mut slot = [None];
        let mut fetched = 0u32;
        let hr = unsafe { monikers.Next(&mut slot, Some(&mut fetched)) };
        if hr.is_err() || fetched == 0 {
            break;
        }
        let Some(moniker) = slot[0].take() else {
            break;
        };
        match friendly_name(&moniker) {
            Some(friendly_name) => devices.push(DeviceEntry {
                friendly_name,
                moniker,
            }),
            None => log::debug!("DirectShow のデバイスの表示名を読めないので飛ばす"),
        }
    }
    Ok(devices)
}

/// 名札から表示名（`FriendlyName`）を読む。
fn friendly_name(moniker: &IMoniker) -> Option<String> {
    let bag: IPropertyBag =
        unsafe { moniker.BindToStorage(None::<&IBindCtx>, None::<&IMoniker>) }.ok()?;
    let mut value = VARIANT::default();
    let read = unsafe { bag.Read(windows::core::w!("FriendlyName"), &mut value, None) };
    let name = if read.is_ok() && unsafe { value.Anonymous.Anonymous.vt } == VT_BSTR {
        let bstr = unsafe { &value.Anonymous.Anonymous.Anonymous.bstrVal };
        Some(bstr.to_string())
    } else {
        None
    };
    // BSTR はここで解放する。失敗しても漏れるだけなので無視する
    let _ = unsafe { VariantClear(&mut value) };
    name.filter(|name| !name.is_empty())
}

/// 名札からフィルターを作る。**ここでデバイスを掴む。**
pub(super) fn bind_filter(entry: &DeviceEntry) -> windows::core::Result<IBaseFilter> {
    unsafe {
        entry
            .moniker
            .BindToObject::<_, _, IBaseFilter>(None::<&IBindCtx>, None::<&IMoniker>)
    }
}

/// フィルターのキャプチャーピンから `IAMStreamConfig` を取る。
///
/// キャプチャーのカテゴリで見つからなければ、カテゴリを問わず映像のピンを
/// 探す。仮想カメラにはピンのカテゴリを名乗らないものがある。
pub(super) fn stream_config(
    builder: &ICaptureGraphBuilder2,
    source: &IBaseFilter,
) -> Option<IAMStreamConfig> {
    for category in [Some(&PIN_CATEGORY_CAPTURE as *const _), None] {
        let mut raw = ptr::null_mut();
        let found = unsafe {
            builder.FindInterface(
                category,
                Some(&MEDIATYPE_Video),
                source,
                &IAMStreamConfig::IID,
                &mut raw,
            )
        };
        if found.is_ok() && !raw.is_null() {
            return Some(unsafe { IAMStreamConfig::from_raw(raw) });
        }
    }
    None
}

/// `GetStreamCaps` の 1 件を、受け取れる形式に直したもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamCandidate {
    /// `GetStreamCaps` に渡す番号
    pub(super) index: i32,
    pub(super) format: SampleFormat,
    /// 設定画面に出す fps。大きい順
    pub(super) fps: Vec<u32>,
    /// `MinFrameInterval` 〜 `MaxFrameInterval` を fps に直した `(最小, 最大)`。
    /// 範囲を持たない（min = max か読めない）なら `None`（#389）
    pub(super) fps_range: Option<(u32, u32)>,
}

/// `IAMStreamConfig` の対応形式を読む。受け取れない形式（UYVY など）は飛ばす。
pub(super) fn read_candidates(config: &IAMStreamConfig) -> Vec<StreamCandidate> {
    let (mut count, mut size) = (0i32, 0i32);
    if unsafe { config.GetNumberOfCapabilities(&mut count, &mut size) }.is_err() {
        return Vec::new();
    }
    // 映像のピンなら VIDEO_STREAM_CONFIG_CAPS の大きさが返る。違うなら
    // 書き込み先が足りないので読まない
    if size as usize > size_of::<VIDEO_STREAM_CONFIG_CAPS>() {
        log::warn!(
            "DirectShow の対応形式の構造体が想定より大きい（{} バイト）ので読まない",
            size
        );
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for index in 0..count {
        let mut pmt: *mut AM_MEDIA_TYPE = ptr::null_mut();
        let mut caps = VIDEO_STREAM_CONFIG_CAPS::default();
        let read = unsafe {
            config.GetStreamCaps(
                index,
                &mut pmt,
                &mut caps as *mut VIDEO_STREAM_CONFIG_CAPS as *mut u8,
            )
        };
        if read.is_err() || pmt.is_null() {
            continue;
        }
        let format = unsafe { sample_format_of(&*pmt) };
        unsafe { delete_media_type(pmt) };
        let Some(format) = format else {
            continue;
        };
        let fps = fps_list(
            format.avg_time_per_frame,
            caps.MinFrameInterval,
            caps.MaxFrameInterval,
        );
        if fps.is_empty() {
            continue;
        }
        let fps_range = fps_range(caps.MinFrameInterval, caps.MaxFrameInterval);
        candidates.push(StreamCandidate {
            index,
            format,
            fps,
            fps_range,
        });
    }
    candidates
}

/// `IAMStreamConfig::GetFormat` が返す、ピンのいまの形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CurrentFormat {
    pub(super) resolution: (u32, u32),
    /// `AvgTimePerFrame` を fps に直したもの。読めなければ `None`（#410）
    pub(super) fps: Option<u32>,
}

/// `IAMStreamConfig::GetFormat` が返す、ピンのいまの形式。読めなければ `None`。
///
/// **`SetFormat` する前に呼ぶ。** 入力信号の解像度と fps をここへ映すドライバーがある
/// （AVerMedia GC551 は入力が 1920x1080 なら、前に 1280x720 で開いたあとでも
/// 1920x1080 を返す。#391）。受け取れない形式（UYVY など）のときは `None`。
pub(super) fn current_format(config: &IAMStreamConfig) -> Option<CurrentFormat> {
    let pmt = unsafe { config.GetFormat() }.ok()?;
    if pmt.is_null() {
        return None;
    }
    let format = unsafe { sample_format_of(&*pmt) };
    unsafe { delete_media_type(pmt) };
    format.map(|format| CurrentFormat {
        resolution: (format.width, format.height),
        fps: fps_from_interval(format.avg_time_per_frame),
    })
}
