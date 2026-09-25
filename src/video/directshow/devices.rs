//! DirectShow の映像入力デバイスの列挙と、対応形式の問い合わせ。
//!
//! 列挙は `ICreateDevEnum` の `CLSID_VideoInputDeviceCategory`、対応形式は
//! キャプチャーピンの `IAMStreamConfig::GetStreamCaps`。
//!
//! 対応形式の一覧から「どれで開くか」を決める判定（`choose_candidate`）と、
//! 設定画面向けの形への並べ替え（`capabilities_from_candidates`）は純粋関数に
//! してあり、デバイスなしで単体テストできる。

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

use super::media_type::{
    delete_media_type, fps_from_interval, sample_format_of, SampleFormat, SampleKind,
};
use crate::video::capabilities::{DeviceCapabilities, FormatCapability, VideoMode};

/// 解像度が未指定のときに開く形。Media Foundation の経路（`VideoCapture`）と同じ
pub(super) const DEFAULT_RESOLUTION: (u32, u32) = (1280, 720);
pub(super) const DEFAULT_FPS: u32 = 60;

/// 受け付ける fps の範囲。Media Foundation の経路と同じ
pub(super) const MIN_FPS: u32 = 15;
pub(super) const MAX_FPS: u32 = 120;

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
    /// 開ける fps。大きい順
    pub(super) fps: Vec<u32>,
}

/// `IAMStreamConfig` の対応形式を読む。受け取れない形式（NV12 など）は飛ばす。
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
        candidates.push(StreamCandidate { index, format, fps });
    }
    candidates
}

/// 1 件の対応形式で開ける fps を並べる。
///
/// メディアタイプの既定値と、`VIDEO_STREAM_CONFIG_CAPS` の最短・最長の間隔を
/// 候補にする。どれも読めなければ空（その形式は選択肢に出さない）。
pub(super) fn fps_list(avg: i64, min_interval: i64, max_interval: i64) -> Vec<u32> {
    let mut fps: Vec<u32> = [avg, min_interval, max_interval]
        .into_iter()
        .filter_map(fps_from_interval)
        .collect();
    fps.sort_unstable_by(|a, b| b.cmp(a));
    fps.dedup();
    fps
}

/// 対応形式を、設定画面の「対応形式」に出す形へ並べ替える。
///
/// 形式ごとにまとめ、解像度の大きい順、同じ解像度なら fps の大きい順にする
/// （Media Foundation の経路の `get_device_capabilities` と同じ並び）。
/// 形式の並びは YUY2・MJPEG・RGB24。
pub(super) fn capabilities_from_candidates(candidates: &[StreamCandidate]) -> DeviceCapabilities {
    let mut result = Vec::new();
    for kind in [SampleKind::Yuy2, SampleKind::Mjpeg, SampleKind::Rgb24] {
        let mut modes: Vec<VideoMode> = candidates
            .iter()
            .filter(|candidate| candidate.format.kind == kind)
            .flat_map(|candidate| {
                candidate.fps.iter().map(|fps| {
                    VideoMode::new(candidate.format.width, candidate.format.height, *fps)
                })
            })
            .collect();
        modes.sort_by(|a, b| {
            b.pixel_count()
                .cmp(&a.pixel_count())
                .then(b.width.cmp(&a.width))
                .then(b.fps.cmp(&a.fps))
        });
        modes.dedup();
        if !modes.is_empty() {
            result.push(FormatCapability::new(kind.name(), modes));
        }
    }
    result
}

/// どの対応形式で開くかを決める。`(候補の添字, 開く fps)`。候補が無ければ `None`。
///
/// 優先順は次のとおり。
///
/// 1. 形式が指定されていて、その形式の候補があれば、その形式だけから選ぶ
/// 2. 解像度が近いもの（画素数の差が小さいもの。一致が最優先）
/// 3. 形式が未指定なら YUY2・MJPEG・RGB24 の順（YUY2 だけが色空間と映像調整の
///    効く高速パスを通るため）
/// 4. 開ける fps が要求に近いもの
///
/// 解像度が未指定なら 1280x720 60fps を要求したものとして扱う（Media
/// Foundation の経路と同じ）。fps は 15〜120 へ丸める。
pub(super) fn choose_candidate(
    candidates: &[StreamCandidate],
    resolution: Option<(u32, u32)>,
    format: Option<&str>,
    fps: Option<u32>,
) -> Option<(usize, u32)> {
    let (width, height) = resolution.unwrap_or(DEFAULT_RESOLUTION);
    let requested_pixels = u64::from(width) * u64::from(height);
    let requested_fps = if resolution.is_some() {
        fps.unwrap_or(DEFAULT_FPS)
    } else {
        DEFAULT_FPS
    }
    .clamp(MIN_FPS, MAX_FPS);
    let requested_kind = format.and_then(SampleKind::from_name);
    let has_requested_kind = requested_kind.is_some_and(|kind| {
        candidates
            .iter()
            .any(|candidate| candidate.format.kind == kind)
    });

    let closest_fps = |candidate: &StreamCandidate| {
        candidate
            .fps
            .iter()
            .copied()
            .min_by_key(|fps| (fps.abs_diff(requested_fps), u32::MAX - fps))
    };

    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            !has_requested_kind || Some(candidate.format.kind) == requested_kind
        })
        .filter_map(|(index, candidate)| {
            let fps = closest_fps(candidate)?;
            let pixels = u64::from(candidate.format.width) * u64::from(candidate.format.height);
            let exact = candidate.format.width == width && candidate.format.height == height;
            let key = (
                !exact,
                pixels.abs_diff(requested_pixels),
                candidate.format.kind,
                fps.abs_diff(requested_fps),
            );
            Some((key, index, fps))
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, index, fps)| (index, fps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        index: i32,
        kind: SampleKind,
        width: u32,
        height: u32,
        fps: &[u32],
    ) -> StreamCandidate {
        StreamCandidate {
            index,
            format: SampleFormat {
                kind,
                width,
                height,
                bottom_up: false,
                avg_time_per_frame: 0,
            },
            fps: fps.to_vec(),
        }
    }

    fn sample_candidates() -> Vec<StreamCandidate> {
        vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[30]),
            candidate(1, SampleKind::Yuy2, 1280, 720, &[60, 30]),
            candidate(2, SampleKind::Mjpeg, 1920, 1080, &[60, 30]),
            candidate(3, SampleKind::Rgb24, 640, 480, &[30]),
        ]
    }

    #[test]
    fn choose_candidate_exact_match_wins() {
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("YUY2"), Some(60)),
            Some((1, 60))
        );
    }

    #[test]
    fn choose_candidate_requested_format_takes_priority_over_resolution() {
        // MJPEG を指定したら、解像度が合わなくても MJPEG から選ぶ
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("MJPEG"), Some(60)),
            Some((2, 60))
        );
    }

    #[test]
    fn choose_candidate_unavailable_format_falls_back_to_any_format() {
        // デバイスに無い形式を指定されたら、形式を問わず解像度で選ぶ
        let candidates = vec![candidate(0, SampleKind::Yuy2, 1280, 720, &[30])];
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("MJPEG"), Some(30)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_without_format_prefers_yuy2_at_the_same_resolution() {
        // 同じ 1920x1080 に YUY2 と MJPEG があれば YUY2
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(30)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_without_resolution_requests_720p60() {
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, None, None, None),
            Some((1, 60))
        );
    }

    #[test]
    fn choose_candidate_picks_the_closest_resolution() {
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[30]),
            candidate(1, SampleKind::Yuy2, 640, 480, &[30]),
        ];
        // 800x600 は 640x480 のほうが画素数が近い
        assert_eq!(
            choose_candidate(&candidates, Some((800, 600)), Some("YUY2"), Some(30)),
            Some((1, 30))
        );
    }

    #[test]
    fn choose_candidate_fps_is_clamped_and_matched_to_the_closest() {
        let candidates = vec![candidate(0, SampleKind::Yuy2, 1280, 720, &[60, 30])];
        // 240 は 120 へ丸められ、開ける中で最も近い 60 になる
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), None, Some(240)),
            Some((0, 60))
        );
        // 5 は 15 へ丸められ、最も近い 30 になる
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), None, Some(5)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_empty_list_is_none() {
        assert_eq!(
            choose_candidate(&[], Some((1280, 720)), None, Some(60)),
            None
        );
    }

    #[test]
    fn fps_list_collects_distinct_rates_in_descending_order() {
        // 既定 30fps、最短 60fps、最長 15fps
        assert_eq!(fps_list(333_333, 166_666, 666_666), vec![60, 30, 15]);
        // 同じ値は 1 つにする
        assert_eq!(fps_list(333_333, 333_333, 333_333), vec![30]);
        // 読めない値は捨てる
        assert_eq!(fps_list(0, 0, 0), Vec::<u32>::new());
    }

    #[test]
    fn capabilities_from_candidates_groups_by_format_in_fixed_order() {
        let caps = capabilities_from_candidates(&sample_candidates());
        let names: Vec<&str> = caps.iter().map(|cap| cap.name.as_str()).collect();
        assert_eq!(names, vec!["YUY2", "MJPEG", "RGB24"]);
        assert_eq!(
            caps[0].modes,
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1280, 720, 30),
            ]
        );
    }

    #[test]
    fn capabilities_from_candidates_removes_duplicates() {
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 640, 480, &[30]),
            candidate(1, SampleKind::Yuy2, 640, 480, &[30]),
        ];
        let caps = capabilities_from_candidates(&candidates);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].modes, vec![VideoMode::new(640, 480, 30)]);
    }

    #[test]
    fn capabilities_from_candidates_empty_input_is_empty() {
        assert!(capabilities_from_candidates(&[]).is_empty());
    }
}
