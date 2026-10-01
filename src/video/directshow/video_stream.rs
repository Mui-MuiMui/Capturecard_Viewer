//! 映像ピンのレンダラーが受け取ったサンプルを `FrameSink` へ渡す部分。
//!
//! `filter.rs` のレンダラーは媒体を問わない作りで、媒体に固有な部分だけを
//! ここ（映像）に分けてある。**呼ばれるのはキャプチャーフィルターの
//! ストリーミングスレッド**（`IMemInputPin::Receive`）なので、ここでもロックも
//! アロケーションもしない。変換と `FrameBuffer` への積み込みは `FrameSink` が
//! やる（ロックはフレームバッファの 1 回だけで、nokhwa のフレームコールバックと
//! 同じ扱い。`docs/design/video-pipeline.md`）。

use std::time::Instant;

use windows::Win32::Media::DirectShow::IMediaSample;

use super::media_type::{delete_media_type, sample_format_of, SampleFormat, SampleKind};
use crate::video::frame_sink::FrameSink;
use crate::video::yuv420::Yuv420Layout;

/// 映像ピンのストリーミングスレッドだけが触る状態。
pub(super) struct VideoStream {
    sink: FrameSink,
    /// いま流れてくるサンプルの形。接続時に決まり、流れの途中で変わることがある
    pub(super) format: Option<SampleFormat>,
}

impl VideoStream {
    pub(super) fn new(sink: FrameSink) -> Self {
        Self { sink, format: None }
    }

    /// サンプル 1 つを `FrameSink` へ渡す。
    pub(super) fn receive(&mut self, sample: &IMediaSample, received_at: Instant) {
        // 流れの途中で形式が変わると、そのサンプルに新しいメディアタイプが
        // 付いてくる。付いていなければ null（S_FALSE）で、確保は起きない
        if let Ok(pmt) = unsafe { sample.GetMediaType() } {
            if !pmt.is_null() {
                let changed = unsafe { sample_format_of(&*pmt) };
                unsafe { delete_media_type(pmt) };
                if changed != self.format {
                    log::info!("DirectShow のサンプルの形式が変わった: {:?}", changed);
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
