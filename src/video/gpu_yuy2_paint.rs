//! YUY2 → RGB の GPU 変換（#456）のうち、egui の描画へ差し込むコールバックの部分。
//! 窓口（`super::gpu_yuy2::GpuYuy2`）が預けたフレームを、描画の中で変換器へ渡す。
//!
//! 変換器は描画バックエンド（#492）に合わせて 2 つある。glow は `egui_glow::CallbackFn` の
//! 中で GL の変換器（`super::gpu_yuy2_gl`）を呼び、egui のテクスチャへ直接描く。wgpu は
//! `egui_wgpu::CallbackTrait::prepare` の中で wgpu の変換器（`super::gpu_yuy2_wgpu`）を呼び、
//! 作ったコマンドを egui が自分の描画より先に送る。どちらも UI スレッドで呼ばれる。

use eframe::egui;
use eframe::egui_glow;
use eframe::egui_wgpu::{self, wgpu};
use log::warn;
use std::sync::{Arc, Mutex};

use super::color::{ColorMatrix, SharedColorConversion};
use super::frame_buffer::VideoFrame;
use super::frame_format::PixelFormat;
use super::gpu_yuy2_gl::{GlConverter, GpuFailure};
use super::gpu_yuy2_wgpu::WgpuConverter;
use crate::repaint::RepaintWaker;

/// 変換器。起動時の描画バックエンドで決まり、途中で入れ替えない
pub(super) enum Converter {
    Gl(GlConverter),
    // wgpu の資源の持ち手は GL のものより大きいので箱に入れる（clippy::large_enum_variant）
    Wgpu(Box<WgpuConverter>),
}

/// 描画のコールバックと UI スレッドで共有するもの。
///
/// **触るのは UI スレッドだけ**（`update()` と、その後の描画のコールバック）。`Mutex` に
/// しているのは、egui のコールバック（`egui_glow::CallbackFn` / `egui_wgpu::CallbackTrait`）が
/// `Send + Sync` を求めるため。待つ相手はいない。
pub(super) struct Shared {
    pub(super) converter: Converter,
    /// 次の描画で変換するフレーム。新しいものが来たら古いものは捨てる（変換しない）
    pub(super) pending: Option<Arc<VideoFrame>>,
    /// 描画の途中で失敗した理由。以後このセッションでは GPU を使わない
    pub(super) failure: Option<GpuFailure>,
    pub(super) color_conversion: Arc<SharedColorConversion>,
    /// 失敗したとき、CPU で描き直す `update()` を起こす
    pub(super) repaint_waker: RepaintWaker,
}

impl Shared {
    /// 預かっているフレームを取り出す。失敗したあとと、YUY2 でないものは取り出さない
    fn take_pending(&mut self) -> Option<(Arc<VideoFrame>, ColorMatrix)> {
        if self.failure.is_some() {
            return None;
        }
        let frame = self.pending.take()?;
        let PixelFormat::Yuy2(matrix) = frame.format else {
            return None;
        };
        Some((frame, matrix))
    }

    /// 描画中の変換の失敗。以後このセッションでは GPU を使わない
    fn fail(&mut self, reason: GpuFailure) {
        warn!(
            "YUY2 を GPU で変換できなかったので、このセッションでは以後 CPU で変換する: {}",
            reason
        );
        // フレームコールバックはすぐ CPU へ戻す。描けなかったこのフレームは、
        // 次の update() が CPU で描き直す（`GpuYuy2::take_switched_to_cpu`）
        self.color_conversion.set_yuy2_on_gpu(false);
        self.failure = Some(reason);
        self.repaint_waker.wake();
    }

    /// glow: 預かっているフレームを `target`（egui のテクスチャ）へ変換して書く。描画中に呼ぶ
    fn convert_pending_gl(&mut self, painter: &egui_glow::Painter, target: egui::TextureId) {
        if self.failure.is_some() || !matches!(self.converter, Converter::Gl(_)) {
            return;
        }
        let Some(texture) = painter.texture(target) else {
            // 映像のテクスチャがまだレンダラーに無い。預けたまま次の描画でやり直す
            return;
        };
        let Some((frame, matrix)) = self.take_pending() else {
            return;
        };
        let Converter::Gl(converter) = &mut self.converter else {
            return;
        };
        // SAFETY: egui の描画中（UI スレッド、GL のコンテキストが current）に呼ばれる
        let result = unsafe {
            converter.convert(
                painter.gl(),
                &frame,
                &matrix,
                texture,
                painter.intermediate_fbo(),
                painter.max_texture_side(),
            )
        };
        if let Err(reason) = result {
            self.fail(reason);
        }
    }

    /// wgpu: 預かっているフレームを `destination`（egui の映像のテクスチャ）へ変換して
    /// 書くコマンドを返す。egui の描画の前段（`CallbackTrait::prepare`）で呼ぶ
    fn convert_pending_wgpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        destination: &wgpu::Texture,
    ) -> Vec<wgpu::CommandBuffer> {
        if !matches!(self.converter, Converter::Wgpu(_)) {
            return Vec::new();
        }
        let Some((frame, matrix)) = self.take_pending() else {
            return Vec::new();
        };
        let Converter::Wgpu(converter) = &mut self.converter else {
            return Vec::new();
        };
        match converter.convert(device, queue, &frame, &matrix, destination) {
            Ok(command) => command.into_iter().collect(),
            Err(reason) => {
                self.fail(reason);
                Vec::new()
            }
        }
    }
}

/// glow の描画へ差し込む変換のコールバック。`target` は映像のテクスチャ
pub(super) fn gl_paint_callback(
    shared: Arc<Mutex<Shared>>,
    target: egui::TextureId,
    rect: egui::Rect,
) -> egui::PaintCallback {
    let callback = egui_glow::CallbackFn::new(move |_info, painter| {
        if let Ok(mut shared) = shared.lock() {
            shared.convert_pending_gl(painter, target);
        }
    });
    egui::PaintCallback {
        rect,
        callback: Arc::new(callback),
    }
}

/// wgpu の描画へ差し込む変換のコールバック。`destination` は映像のテクスチャの実体
pub(super) fn wgpu_paint_callback(
    shared: Arc<Mutex<Shared>>,
    destination: wgpu::Texture,
    rect: egui::Rect,
) -> egui::PaintCallback {
    egui_wgpu::Callback::new_paint_callback(
        rect,
        WgpuConvertCallback {
            shared,
            destination,
        },
    )
}

/// 変換は `prepare` で済ませ、`paint` では描かない（egui がそのあと映像のテクスチャを
/// 普通に描く）
struct WgpuConvertCallback {
    shared: Arc<Mutex<Shared>>,
    /// 書き込み先（egui の映像のテクスチャ）。`GpuYuy2::paint_callback` が引いたもの
    destination: wgpu::Texture,
}

impl egui_wgpu::CallbackTrait for WgpuConvertCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        match self.shared.lock() {
            Ok(mut shared) => shared.convert_pending_wgpu(device, queue, &self.destination),
            Err(_) => Vec::new(),
        }
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        _render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &egui_wgpu::CallbackResources,
    ) {
    }
}
