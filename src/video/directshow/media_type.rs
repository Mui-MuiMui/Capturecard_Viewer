//! DirectShow のメディアタイプ（`AM_MEDIA_TYPE`）の読み書きと、COM の初期化。
//!
//! メディアタイプの中身を読んで「何の形式で、幅と高さはいくつか」に直す
//! 判定は純粋関数（`sample_format_from_header` / `fps_from_interval`）に
//! 出してあり、COM を使わずに単体テストできる。

use std::mem::{size_of, ManuallyDrop};
use std::ptr;

use windows::core::GUID;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIATYPE_Video, AM_MEDIA_TYPE, MEDIASUBTYPE_MJPG,
    MEDIASUBTYPE_RGB24, MEDIASUBTYPE_YUY2, MEDIASUBTYPE_YUYV, VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE,
};

/// DirectShow の時間の単位（100ns）で 1 秒
const UNITS_PER_SECOND: i64 = 10_000_000;

/// 受け取れるサンプルの形式。
///
/// 名前は設定画面と同じ語彙（`VideoCapture` の対応形式と揃える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum SampleKind {
    Yuy2,
    Mjpeg,
    Rgb24,
}

impl SampleKind {
    /// 設定画面とログに出す名前
    pub(super) fn name(self) -> &'static str {
        match self {
            SampleKind::Yuy2 => "YUY2",
            SampleKind::Mjpeg => "MJPEG",
            SampleKind::Rgb24 => "RGB24",
        }
    }

    /// 設定に書かれた名前から引く。知らない名前なら `None`
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "YUY2" => Some(SampleKind::Yuy2),
            "MJPEG" => Some(SampleKind::Mjpeg),
            "RGB24" => Some(SampleKind::Rgb24),
            _ => None,
        }
    }

    /// メディアタイプのサブタイプから引く。受け取れない形式なら `None`
    fn from_subtype(subtype: &GUID) -> Option<Self> {
        // YUYV は YUY2 と同じ並び（Y0 U Y1 V）で、呼び名が違うだけ
        if *subtype == MEDIASUBTYPE_YUY2 || *subtype == MEDIASUBTYPE_YUYV {
            Some(SampleKind::Yuy2)
        } else if *subtype == MEDIASUBTYPE_MJPG {
            Some(SampleKind::Mjpeg)
        } else if *subtype == MEDIASUBTYPE_RGB24 {
            Some(SampleKind::Rgb24)
        } else {
            None
        }
    }
}

/// 1 本のストリームで流れてくるサンプルの形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SampleFormat {
    pub(super) kind: SampleKind,
    pub(super) width: u32,
    pub(super) height: u32,
    /// 行が下から上へ並んでいるか。RGB で `biHeight` が正のときだけ真
    pub(super) bottom_up: bool,
    /// 1 フレームの長さ（100ns 単位）。デバイスが書いていなければ 0
    pub(super) avg_time_per_frame: i64,
}

/// `BITMAPINFOHEADER` の幅・高さからサンプルの形を決める。
///
/// **高さの符号の意味は形式で違う。** RGB は正なら下から上（ボトムアップ）、
/// 負なら上から下。YUV（YUY2）と圧縮形式（MJPEG）は符号によらず上から下。
/// 幅か高さが 0 のものは受け取らない。
pub(super) fn sample_format_from_header(
    kind: SampleKind,
    bi_width: i32,
    bi_height: i32,
    avg_time_per_frame: i64,
) -> Option<SampleFormat> {
    if bi_width <= 0 || bi_height == 0 {
        return None;
    }
    Some(SampleFormat {
        kind,
        width: bi_width.unsigned_abs(),
        height: bi_height.unsigned_abs(),
        bottom_up: kind == SampleKind::Rgb24 && bi_height > 0,
        avg_time_per_frame,
    })
}

/// 1 フレームの長さ（100ns 単位）を fps に直す。四捨五入する（333333 → 30）。
/// 0 以下や、1fps を下回る長さは `None`
pub(super) fn fps_from_interval(interval: i64) -> Option<u32> {
    if interval <= 0 || interval > UNITS_PER_SECOND {
        return None;
    }
    u32::try_from((UNITS_PER_SECOND + interval / 2) / interval).ok()
}

/// fps を 1 フレームの長さ（100ns 単位）に直す。`fps_from_interval` の逆
pub(super) fn interval_from_fps(fps: u32) -> i64 {
    UNITS_PER_SECOND / i64::from(fps.max(1))
}

/// `SetFormat` に書く 1 フレームの長さを決める。
///
/// fps は四捨五入した整数なので、そのまま間隔へ戻すと 29.97 / 59.94fps の
/// デバイスでは `MinFrameInterval`（333667 / 166833）より短くなり、範囲を
/// 厳しく見るドライバに `SetFormat` を断られる。デバイスが示した範囲が
/// 読めれば、その中へ収める。
pub(super) fn interval_within_caps(fps: u32, min_interval: i64, max_interval: i64) -> i64 {
    let interval = interval_from_fps(fps);
    if min_interval > 0 && max_interval >= min_interval {
        interval.clamp(min_interval, max_interval)
    } else {
        interval
    }
}

/// メディアタイプを読んで、受け取れる形式ならその形を返す。
///
/// # Safety
/// `mt.pbFormat` は `mt.cbFormat` バイトを指していること（DirectShow から
/// 受け取ったものならそうなっている）。
pub(super) unsafe fn sample_format_of(mt: &AM_MEDIA_TYPE) -> Option<SampleFormat> {
    if mt.majortype != MEDIATYPE_Video || mt.pbFormat.is_null() {
        return None;
    }
    let kind = SampleKind::from_subtype(&mt.subtype)?;
    let len = mt.cbFormat as usize;
    if mt.formattype == FORMAT_VideoInfo && len >= size_of::<VIDEOINFOHEADER>() {
        // pbFormat の揃え（アラインメント）は保証されないので読み出しで写す
        let header = unsafe { ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER) };
        sample_format_from_header(
            kind,
            header.bmiHeader.biWidth,
            header.bmiHeader.biHeight,
            header.AvgTimePerFrame,
        )
    } else if mt.formattype == FORMAT_VideoInfo2 && len >= size_of::<VIDEOINFOHEADER2>() {
        let header = unsafe { ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER2) };
        sample_format_from_header(
            kind,
            header.bmiHeader.biWidth,
            header.bmiHeader.biHeight,
            header.AvgTimePerFrame,
        )
    } else {
        None
    }
}

/// メディアタイプの 1 フレームの長さを書き換える。fps を指定して開くときに使う。
///
/// # Safety
/// `sample_format_of` と同じ。
pub(super) unsafe fn set_avg_time_per_frame(mt: &mut AM_MEDIA_TYPE, interval: i64) {
    if mt.pbFormat.is_null() {
        return;
    }
    let len = mt.cbFormat as usize;
    if mt.formattype == FORMAT_VideoInfo && len >= size_of::<VIDEOINFOHEADER>() {
        let target = mt.pbFormat as *mut VIDEOINFOHEADER;
        let mut header = unsafe { ptr::read_unaligned(target) };
        header.AvgTimePerFrame = interval;
        unsafe { ptr::write_unaligned(target, header) };
    } else if mt.formattype == FORMAT_VideoInfo2 && len >= size_of::<VIDEOINFOHEADER2>() {
        let target = mt.pbFormat as *mut VIDEOINFOHEADER2;
        let mut header = unsafe { ptr::read_unaligned(target) };
        header.AvgTimePerFrame = interval;
        unsafe { ptr::write_unaligned(target, header) };
    }
}

/// DirectShow が `CoTaskMemAlloc` で確保して渡してきたメディアタイプを解放する
/// （DirectShow の基底クラスの `DeleteMediaType` に当たる）。
///
/// # Safety
/// `pmt` は null か、`CoTaskMemAlloc` で確保された `AM_MEDIA_TYPE` を指すこと。
pub(super) unsafe fn delete_media_type(pmt: *mut AM_MEDIA_TYPE) {
    if pmt.is_null() {
        return;
    }
    unsafe {
        free_media_type_contents(&mut *pmt);
        CoTaskMemFree(Some(pmt as *const _));
    }
}

/// メディアタイプの中身（形式のブロックと `pUnk`）だけを解放する
/// （`FreeMediaType` に当たる）。
///
/// # Safety
/// `mt.pbFormat` は null か `CoTaskMemAlloc` で確保されたものであること。
pub(super) unsafe fn free_media_type_contents(mt: &mut AM_MEDIA_TYPE) {
    if !mt.pbFormat.is_null() {
        unsafe { CoTaskMemFree(Some(mt.pbFormat as *const _)) };
    }
    mt.pbFormat = ptr::null_mut();
    mt.cbFormat = 0;
    // pUnk は ManuallyDrop なので、明示的に落とす
    let unk = std::mem::replace(&mut mt.pUnk, ManuallyDrop::new(None));
    drop(ManuallyDrop::into_inner(unk));
}

/// 接続したときのメディアタイプの控え。
///
/// `IPin::ConnectionMediaType` で返すために持つ。形式のブロックは自分の
/// `Vec` に写してあり、返すときに `CoTaskMemAlloc` で複製する。
#[derive(Debug, Clone)]
pub(super) struct OwnedMediaType {
    majortype: GUID,
    subtype: GUID,
    fixed_size_samples: bool,
    temporal_compression: bool,
    sample_size: u32,
    formattype: GUID,
    format: Vec<u8>,
}

impl OwnedMediaType {
    /// 受け取ったメディアタイプを写す。
    ///
    /// # Safety
    /// `sample_format_of` と同じ。
    pub(super) unsafe fn copy_from(mt: &AM_MEDIA_TYPE) -> Self {
        let format = if mt.pbFormat.is_null() || mt.cbFormat == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(mt.pbFormat, mt.cbFormat as usize) }.to_vec()
        };
        Self {
            majortype: mt.majortype,
            subtype: mt.subtype,
            fixed_size_samples: mt.bFixedSizeSamples.as_bool(),
            temporal_compression: mt.bTemporalCompression.as_bool(),
            sample_size: mt.lSampleSize,
            formattype: mt.formattype,
            format,
        }
    }

    /// 呼び出し側が用意した `AM_MEDIA_TYPE` へ書き出す。形式のブロックは
    /// `CoTaskMemAlloc` で確保し、解放は受け取った側（DirectShow の決まり）。
    ///
    /// 確保に失敗したら形式のブロックを空にして `false` を返す。
    pub(super) fn write_to(&self, out: &mut AM_MEDIA_TYPE) -> bool {
        out.majortype = self.majortype;
        out.subtype = self.subtype;
        out.bFixedSizeSamples = self.fixed_size_samples.into();
        out.bTemporalCompression = self.temporal_compression.into();
        out.lSampleSize = self.sample_size;
        out.formattype = self.formattype;
        out.pUnk = ManuallyDrop::new(None);
        out.cbFormat = 0;
        out.pbFormat = ptr::null_mut();
        if self.format.is_empty() {
            return true;
        }
        let block = unsafe { CoTaskMemAlloc(self.format.len()) } as *mut u8;
        if block.is_null() {
            return false;
        }
        unsafe { ptr::copy_nonoverlapping(self.format.as_ptr(), block, self.format.len()) };
        out.cbFormat = self.format.len() as u32;
        out.pbFormat = block;
        true
    }
}

/// このスレッドで COM を使えるようにしておく印。落とすと初期化を戻す。
///
/// **シングルスレッドアパートメント（STA）で初期化する。** 同じワーカー
/// スレッドの上で nokhwa（Media Foundation）と cpal（WASAPI）がどちらも STA で
/// 初期化しており、ここだけ MTA にすると、後から初期化する側が
/// `RPC_E_CHANGED_MODE` で失敗する（nokhwa はそれを起動の失敗として扱う）。
pub(super) struct ComApartment {
    /// `CoUninitialize` で戻す必要があるか。既に別のモデルで初期化されていて
    /// `RPC_E_CHANGED_MODE` が返ったときだけ偽
    initialized: bool,
    /// スレッドに紐づくので、ほかのスレッドへ持ち出させない
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl ComApartment {
    /// このスレッドの COM を初期化する。既に同じモデルで初期化済みでもよい
    /// （回数が数えられるだけで、`Drop` で 1 回戻す）。
    pub(super) fn enter() -> Result<Self, windows::core::Error> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            // 別のモデル（MTA）で初期化済み。COM は使えるので、戻さずに使う
            return Ok(Self {
                initialized: false,
                _not_send: std::marker::PhantomData,
            });
        }
        hr.ok()?;
        Ok(Self {
            initialized: true,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_format_from_header_rgb_with_positive_height_is_bottom_up() {
        let format =
            sample_format_from_header(SampleKind::Rgb24, 640, 480, 333_333).expect("受け取れる");
        assert_eq!((format.width, format.height), (640, 480));
        assert!(format.bottom_up);
    }

    #[test]
    fn sample_format_from_header_rgb_with_negative_height_is_top_down() {
        let format =
            sample_format_from_header(SampleKind::Rgb24, 640, -480, 333_333).expect("受け取れる");
        assert_eq!(format.height, 480);
        assert!(!format.bottom_up);
    }

    #[test]
    fn sample_format_from_header_yuy2_is_top_down_regardless_of_sign() {
        // YUV は高さの符号によらず上から下
        let positive = sample_format_from_header(SampleKind::Yuy2, 1920, 1080, 0).expect("正");
        let negative = sample_format_from_header(SampleKind::Yuy2, 1920, -1080, 0).expect("負");
        assert!(!positive.bottom_up);
        assert!(!negative.bottom_up);
        assert_eq!(negative.height, 1080);
    }

    #[test]
    fn sample_format_from_header_zero_or_negative_width_is_rejected() {
        assert_eq!(sample_format_from_header(SampleKind::Yuy2, 0, 480, 0), None);
        assert_eq!(
            sample_format_from_header(SampleKind::Yuy2, -640, 480, 0),
            None
        );
        assert_eq!(
            sample_format_from_header(SampleKind::Mjpeg, 640, 0, 0),
            None
        );
    }

    #[test]
    fn fps_from_interval_rounds_to_the_nearest_integer() {
        assert_eq!(fps_from_interval(333_333), Some(30));
        assert_eq!(fps_from_interval(166_666), Some(60));
        // 29.97fps は 30 へ丸める
        assert_eq!(fps_from_interval(333_667), Some(30));
        assert_eq!(fps_from_interval(10_000_000), Some(1));
    }

    #[test]
    fn fps_from_interval_invalid_values_are_none() {
        assert_eq!(fps_from_interval(0), None);
        assert_eq!(fps_from_interval(-1), None);
        // 1fps を下回る（1 フレームが 1 秒より長い）ものは扱わない
        assert_eq!(fps_from_interval(10_000_001), None);
    }

    #[test]
    fn interval_from_fps_is_the_inverse_of_fps_from_interval() {
        assert_eq!(interval_from_fps(30), 333_333);
        assert_eq!(interval_from_fps(60), 166_666);
        assert_eq!(fps_from_interval(interval_from_fps(60)), Some(60));
        // 0 は 1 として扱い、0 除算にしない
        assert_eq!(interval_from_fps(0), 10_000_000);
    }

    #[test]
    fn interval_within_caps_keeps_the_ntsc_minimum_interval() {
        // 29.97fps のデバイスで 30fps を選ぶと、333333 ではなく最短の 333667
        assert_eq!(interval_within_caps(30, 333_667, 333_667), 333_667);
        assert_eq!(interval_within_caps(60, 166_833, 333_667), 166_833);
    }

    #[test]
    fn interval_within_caps_inside_the_range_is_unchanged() {
        assert_eq!(interval_within_caps(30, 166_666, 666_666), 333_333);
    }

    #[test]
    fn interval_within_caps_clamps_to_the_longest_interval() {
        // 範囲より遅い fps は最長の間隔へ
        assert_eq!(interval_within_caps(15, 166_666, 333_333), 333_333);
    }

    #[test]
    fn interval_within_caps_ignores_an_unreadable_range() {
        // 範囲が読めない（0 や逆転）ときは fps から戻した値のまま
        assert_eq!(interval_within_caps(30, 0, 0), 333_333);
        assert_eq!(interval_within_caps(30, 333_667, 166_833), 333_333);
    }

    #[test]
    fn sample_kind_names_round_trip() {
        for kind in [SampleKind::Yuy2, SampleKind::Mjpeg, SampleKind::Rgb24] {
            assert_eq!(SampleKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(SampleKind::from_name("NV12"), None);
        assert_eq!(SampleKind::from_name(""), None);
    }
}
