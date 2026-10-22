//! 映像ピンのレンダラーが受け取ったサンプルを `FrameSink` へ渡す部分。
//!
//! `filter.rs` のレンダラーは媒体を問わない作りで、媒体に固有な部分だけを
//! ここ（映像）に分けてある。**呼ばれるのはキャプチャーフィルターの
//! ストリーミングスレッド**（`IMemInputPin::Receive`）なので、ここでもロックも
//! アロケーションもしない。変換と `FrameBuffer` への積み込みは `FrameSink` が
//! やる（ロックはフレームバッファの 1 回だけで、nokhwa のフレームコールバックと
//! 同じ扱い。`docs/design/video-pipeline.md`）。
//!
//! **ログも出さない**（ロガーのロックと文字列の確保が起きる）。流れの途中で
//! 形式が変わったことは `FormatChanges` の Atomic へ書くだけにして、ログは
//! ワーカーの監視の周期で `CaptureGraph::poll` が出す（音声ピンの塊の長さと
//! 同じ扱い。`docs/design/directshow-audio.md` の「`Receive` の中でしないこと」）。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Media::DirectShow::IMediaSample;

use super::media_type::{
    delete_media_type, fps_from_interval, sample_format_of, SampleFormat, SampleKind,
};
use crate::video::frame_sink::FrameSink;
use crate::video::yuv420::Yuv420Layout;

/// 流れの途中で形式が変わったことを、`Receive` からワーカーへ渡す控え。
///
/// `Receive` は回数と新しい形式を Atomic へ書くだけ（`record`）。ワーカーが
/// 回数を見て、前に見たときから増えていればログへ出す。形式は 1 つの
/// `AtomicU64` に詰める（`pack_format`）。回数と形式の間で読み違えても、
/// ずれるのはログの 1 行だけ。
#[derive(Clone, Default)]
pub(super) struct FormatChanges {
    inner: Arc<FormatChangesInner>,
}

#[derive(Default)]
struct FormatChangesInner {
    count: AtomicU32,
    latest: AtomicU64,
}

impl FormatChanges {
    /// 形式が変わった。`Receive` から呼ぶ。ロックも確保もしない
    fn record(&self, format: Option<SampleFormat>) {
        self.inner
            .latest
            .store(pack_format(format), Ordering::Relaxed);
        self.inner.count.fetch_add(1, Ordering::Release);
    }

    /// これまでに変わった回数と、最後に変わった先の形式。ワーカーが読む
    pub(super) fn latest(&self) -> (u32, Option<ObservedFormat>) {
        let count = self.inner.count.load(Ordering::Acquire);
        let latest = self.inner.latest.load(Ordering::Relaxed);
        (count, unpack_format(latest))
    }
}

/// `FormatChanges` から読み戻した形式。ログに出すものだけ
/// （1 フレームの長さは fps に丸めてある）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObservedFormat {
    pub(super) kind: SampleKind,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bottom_up: bool,
    pub(super) fps: Option<u32>,
}

/// 幅・高さに使うビット数。100 万画素を超える辺は来ない
const DIMENSION_BITS: u32 = 20;
const DIMENSION_MASK: u64 = (1 << DIMENSION_BITS) - 1;
const FPS_SHIFT: u32 = DIMENSION_BITS * 2;
const FPS_MASK: u64 = 0x7FFF;
const BOTTOM_UP_BIT: u64 = 1 << 55;
const KIND_SHIFT: u32 = 56;
const PRESENT_BIT: u64 = 1 << 63;

/// 形式を 1 つの `u64` に詰める。`None`（受け取れない形式）は 0。
/// 収まらない幅・高さ・fps は詰められる最大値で頭打ちにする（ログに使うだけ）
fn pack_format(format: Option<SampleFormat>) -> u64 {
    let Some(format) = format else {
        return 0;
    };
    let kind = SampleKind::ALL
        .iter()
        .position(|kind| *kind == format.kind)
        .unwrap_or(0) as u64;
    let fps = fps_from_interval(format.avg_time_per_frame).map_or(0, u64::from);
    let mut packed = PRESENT_BIT
        | (kind << KIND_SHIFT)
        | (fps.min(FPS_MASK) << FPS_SHIFT)
        | (u64::from(format.height).min(DIMENSION_MASK) << DIMENSION_BITS)
        | u64::from(format.width).min(DIMENSION_MASK);
    if format.bottom_up {
        packed |= BOTTOM_UP_BIT;
    }
    packed
}

/// `pack_format` の逆。fps が分からなければ `None`
fn unpack_format(packed: u64) -> Option<ObservedFormat> {
    if packed & PRESENT_BIT == 0 {
        return None;
    }
    let kind = *SampleKind::ALL.get(((packed >> KIND_SHIFT) & 0x7F) as usize)?;
    let fps = ((packed >> FPS_SHIFT) & FPS_MASK) as u32;
    Some(ObservedFormat {
        kind,
        width: (packed & DIMENSION_MASK) as u32,
        height: ((packed >> DIMENSION_BITS) & DIMENSION_MASK) as u32,
        bottom_up: packed & BOTTOM_UP_BIT != 0,
        fps: (fps > 0).then_some(fps),
    })
}

/// 映像ピンのストリーミングスレッドだけが触る状態。
pub(super) struct VideoStream {
    sink: FrameSink,
    /// いま流れてくるサンプルの形。接続時に決まり、流れの途中で変わることがある
    pub(super) format: Option<SampleFormat>,
    /// 流れの途中で形式が変わったことの控え。ワーカーが読んでログへ出す
    changes: FormatChanges,
}

impl VideoStream {
    pub(super) fn new(sink: FrameSink, changes: FormatChanges) -> Self {
        Self {
            sink,
            format: None,
            changes,
        }
    }

    /// サンプル 1 つを `FrameSink` へ渡す。**ログも出さない。**
    pub(super) fn receive(&mut self, sample: &IMediaSample, received_at: Instant) {
        // 流れの途中で形式が変わると、そのサンプルに新しいメディアタイプが
        // 付いてくる。付いていなければ null（S_FALSE）で、確保は起きない。
        // 付いていたときの確保は上流（`CoTaskMemAlloc`）がしたもので、形式が
        // 変わったサンプルにしか起きない
        if let Ok(pmt) = unsafe { sample.GetMediaType() } {
            if !pmt.is_null() {
                let changed = unsafe { sample_format_of(&*pmt) };
                unsafe { delete_media_type(pmt) };
                if changed != self.format {
                    self.changes.record(changed);
                }
                self.format = changed;
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
        let (width, height) = (format.width as usize, format.height as usize);
        match format.kind {
            SampleKind::Yuy2 => {
                self.sink.push_yuy2(width, height, src, received_at);
            }
            SampleKind::Nv12 => {
                self.sink
                    .push_yuv420(Yuv420Layout::Nv12, width, height, src, received_at);
            }
            SampleKind::I420 => {
                self.sink
                    .push_yuv420(Yuv420Layout::I420, width, height, src, received_at);
            }
            SampleKind::Yv12 => {
                self.sink
                    .push_yuv420(Yuv420Layout::Yv12, width, height, src, received_at);
            }
            SampleKind::Rgb24 => {
                self.sink
                    .push_bgr24(width, height, format.bottom_up, src, received_at);
            }
            SampleKind::Mjpeg => {
                self.sink.push_mjpeg(width, height, src, received_at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(kind: SampleKind, width: u32, height: u32, interval: i64) -> SampleFormat {
        SampleFormat {
            kind,
            width,
            height,
            bottom_up: false,
            avg_time_per_frame: interval,
        }
    }

    #[test]
    fn pack_format_round_trips_every_kind() {
        for kind in SampleKind::ALL {
            let packed = pack_format(Some(format(kind, 1920, 1080, 166_667)));
            assert_eq!(
                unpack_format(packed),
                Some(ObservedFormat {
                    kind,
                    width: 1920,
                    height: 1080,
                    bottom_up: false,
                    fps: Some(60),
                })
            );
        }
    }

    #[test]
    fn pack_format_keeps_bottom_up_and_unknown_fps() {
        let rgb = SampleFormat {
            bottom_up: true,
            ..format(SampleKind::Rgb24, 640, 480, 0)
        };
        let observed = unpack_format(pack_format(Some(rgb))).expect("詰めた形式が戻る");
        assert!(observed.bottom_up);
        assert_eq!(observed.fps, None);
        assert_eq!((observed.width, observed.height), (640, 480));
    }

    #[test]
    fn pack_format_of_an_unsupported_format_is_none() {
        assert_eq!(pack_format(None), 0);
        assert_eq!(unpack_format(0), None);
    }

    #[test]
    fn pack_format_caps_oversized_dimensions() {
        // 壊れた値でも隣のビットへはみ出さない
        let packed = pack_format(Some(format(SampleKind::Yuy2, u32::MAX, 2, 333_333)));
        let observed = unpack_format(packed).expect("詰めた形式が戻る");
        assert_eq!(observed.width, (1 << DIMENSION_BITS) - 1);
        assert_eq!(observed.height, 2);
        assert_eq!(observed.fps, Some(30));
        assert_eq!(observed.kind, SampleKind::Yuy2);
    }

    #[test]
    fn format_changes_counts_and_keeps_the_latest() {
        let changes = FormatChanges::default();
        assert_eq!(changes.latest(), (0, None));
        changes.record(Some(format(SampleKind::Nv12, 1280, 720, 333_333)));
        changes.record(Some(format(SampleKind::Mjpeg, 1920, 1080, 166_667)));
        let (count, latest) = changes.latest();
        assert_eq!(count, 2);
        assert_eq!(latest.map(|f| f.kind), Some(SampleKind::Mjpeg));
        // 受け取れない形式へ変わったことも数える
        changes.record(None);
        assert_eq!(changes.latest(), (3, None));
    }
}
