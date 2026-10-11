//! YUY2 → RGB を GPU（シェーダー）で行う窓口（#456）。使うかどうかの判断、UI スレッドから
//! 変換するフレームを預かる口、egui の描画へ差し込むコールバック、CPU へ戻す判断。
//!
//! 変換器は描画バックエンド（`crate::renderer`、#492）に合わせて 2 つある。glow で描くときは
//! GL の資源と GLSL（`super::gpu_yuy2_gl`、egui の `egui_glow::CallbackFn`）、wgpu で描くときは
//! wgpu の資源と WGSL（`super::gpu_yuy2_wgpu`、`egui_wgpu::CallbackTrait`）。どちらを使うかは
//! 起動時に渡された描画のコンテキスト（`GpuContext`）で決まり、この窓口の使い方は変わらない。
//! 性能の見張りは `super::gpu_watch`。
//! 設計は `docs/design/video-pipeline.md` の「YUY2 → RGB を GPU で変換する（#456）」。
//!
//! - 起動時（`init`）にシェーダーを用意し、自己診断で CPU の変換と全画素が一致したら
//!   GPU を使える状態にする。使うかどうかは設定（`[video] convert`）と見張りで決め、
//!   使うあいだだけ `SharedColorConversion::set_yuy2_on_gpu` を立てる。立つとフレーム
//!   コールバック（`FrameSink::push_yuy2`）は YUY2 を変換せずに積む
//! - UI スレッドは YUY2 のフレームを `queue` で預け、描画のときに `paint_callback` が
//!   返すコールバックを映像の前に置く。コールバックが映像のテクスチャへ RGB で書き、
//!   egui がそのテクスチャを普通に描く
//! - **自動で CPU へ戻すのは GPU → CPU の一方向で、1 セッションに 1 回だけ。** GL の失敗は
//!   以後ずっと、見張りによる切り替えは利用者が設定を変えて適用するまで GPU へ戻さない
//! - 環境変数 `CAPTURECARD_VIEWER_GPU_CONVERT=0` で GPU を準備しない（開発者向けの A/B。設定より優先）

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use eframe::glow::{self, HasContext as _};
use log::{info, warn};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::color::SharedColorConversion;
use super::frame_buffer::{FrameStats, VideoFrame};
use super::gpu_watch::{is_software_renderer, watched_interval, Observation, SlowPaintWatch};
use super::gpu_yuy2_gl::{GlConverter, GpuFailure};
use super::gpu_yuy2_paint::{gl_paint_callback, wgpu_paint_callback, Converter, Shared};
use super::gpu_yuy2_wgpu::WgpuConverter;
use crate::repaint::RepaintWaker;
use crate::settings::VideoConvertSetting;

/// 起動時に eframe から受け取る描画のコンテキスト（#492）。どちらの変換器を使うかを決める
#[derive(Clone, Copy)]
pub enum GpuContext<'a> {
    /// どちらも無い（テストで作ったアプリなど）
    None,
    /// glow（OpenGL）で描いている
    Glow(&'a Arc<glow::Context>),
    /// wgpu（DX12）で描いている
    Wgpu(&'a egui_wgpu::RenderState),
}

/// GPU での変換を切る環境変数。`0`（または `false` / `off`）で CPU の経路に戻す
pub const GPU_CONVERT_ENV: &str = "CAPTURECARD_VIEWER_GPU_CONVERT";

/// 環境変数の値から、GPU での変換を試すかを決める。未指定・空・それ以外の値は試す
pub fn gpu_convert_requested(value: Option<&str>) -> bool {
    let value = value.map(|v| v.trim().to_ascii_lowercase());
    !matches!(value.as_deref(), Some("0" | "false" | "off"))
}

/// YUY2 をどこで RGB にしているか。ログと「接続状態」タブ・統計 OSD に出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yuy2Conversion {
    /// GPU（シェーダー）
    Gpu,
    /// CPU（フレームコールバック）。理由付き
    Cpu(CpuReason),
}

/// CPU で変換している理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuReason {
    /// 環境変数で切ってある
    DisabledByEnv,
    /// 設定（`[video] convert = "cpu"`）で選んである
    DisabledBySetting,
    /// 描画のコンテキスト（GL / wgpu）が無い（テストで作ったアプリなど）
    NoGpuContext,
    /// ソフトウェア描画（設定が「自動」のとき）。`GL_RENDERER` か wgpu のアダプターの名前
    SoftwareRenderer(String),
    /// 描画が遅い状態が続いたので戻した（設定が「自動」のとき）
    TooSlow,
    /// GPU で変換できない（コンパイル・自己診断・描画の失敗）。理由の文
    Unavailable(GpuFailure),
}

impl CpuReason {
    /// 自動で CPU へ戻したものか（利用者が選んだのではない）。統計 OSD に印を出す
    pub fn is_fallback(&self) -> bool {
        matches!(
            self,
            CpuReason::SoftwareRenderer(_) | CpuReason::TooSlow | CpuReason::Unavailable(_)
        )
    }
}

/// 起動時に用意できた GPU の様子。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GpuInfo {
    /// GL なら `GL_RENDERER`、wgpu ならアダプターの名前
    renderer: String,
    /// ソフトウェア描画か（`gpu_watch::is_software_renderer`。wgpu は CPU のアダプターも）
    software: bool,
}

/// いま YUY2 をどこで変換するかを決める。**優先順は 環境変数 → 設定の CPU → 準備の失敗 →
/// 描画の失敗 →（自動のときだけ）ソフトウェア描画 → 遅さ。**
fn decide(
    prepared: &Result<GpuInfo, CpuReason>,
    failure: Option<&GpuFailure>,
    preference: VideoConvertSetting,
    too_slow: bool,
) -> Yuy2Conversion {
    let reason = match (prepared, preference, failure) {
        (Err(CpuReason::DisabledByEnv), _, _) => CpuReason::DisabledByEnv,
        (_, VideoConvertSetting::Cpu, _) => CpuReason::DisabledBySetting,
        (Err(reason), _, _) => reason.clone(),
        (Ok(_), _, Some(failure)) => CpuReason::Unavailable(failure.clone()),
        (Ok(info), VideoConvertSetting::Auto, None) if info.software => {
            CpuReason::SoftwareRenderer(info.renderer.clone())
        }
        (Ok(_), VideoConvertSetting::Auto, None) if too_slow => CpuReason::TooSlow,
        (Ok(_), _, None) => return Yuy2Conversion::Gpu,
    };
    Yuy2Conversion::Cpu(reason)
}

/// YUY2 を GPU で変換する窓口。`CaptureCardViewer` が 1 つ持つ。
pub struct GpuYuy2 {
    /// 起動時の準備の結果
    prepared: Result<GpuInfo, CpuReason>,
    /// 設定（`[video] convert`）
    preference: VideoConvertSetting,
    /// 見張りが遅いと判断した。設定を変えて適用するまで戻さない
    too_slow: bool,
    watch: SlowPaintWatch,
    /// いまの判断。変わったときだけログへ出す
    current: Yuy2Conversion,
    /// GPU から CPU へ切り替わったのに、まだ CPU で描き直していない
    switched_to_cpu: bool,
    shared: Option<Arc<Mutex<Shared>>>,
    /// wgpu で描くとき、egui の映像のテクスチャ（`wgpu::Texture`）を引くためのレンダラー。
    /// 読むのは `update()` の中（`paint_callback`）だけで、eframe が描画で書き込みのロックを
    /// 取るのはその後なので待たない
    wgpu_renderer: Option<Arc<egui::mutex::RwLock<egui_wgpu::Renderer>>>,
    color_conversion: Arc<SharedColorConversion>,
}

impl Default for GpuYuy2 {
    /// 描画のコンテキストを渡されていない状態（CPU で変換する）
    fn default() -> Self {
        Self::not_prepared(
            CpuReason::NoGpuContext,
            Arc::new(SharedColorConversion::new()),
        )
    }
}

impl GpuYuy2 {
    fn not_prepared(reason: CpuReason, color_conversion: Arc<SharedColorConversion>) -> Self {
        Self {
            current: Yuy2Conversion::Cpu(reason.clone()),
            prepared: Err(reason),
            preference: VideoConvertSetting::default(),
            too_slow: false,
            watch: SlowPaintWatch::default(),
            switched_to_cpu: false,
            shared: None,
            wgpu_renderer: None,
            color_conversion,
        }
    }

    /// 起動時に 1 回だけ呼ぶ。`context` は eframe から受け取った描画のコンテキスト、
    /// `env_value` は `GPU_CONVERT_ENV` の値、`preference` は設定。
    ///
    /// シェーダーを用意して自己診断を通し、設定と GPU の様子から使うかを決める。どこかで
    /// 失敗したら WARN を出して CPU のまま（`color_conversion` の旗は立てない）
    pub fn init(
        context: GpuContext<'_>,
        color_conversion: &Arc<SharedColorConversion>,
        repaint_waker: &RepaintWaker,
        preference: VideoConvertSetting,
        env_value: Option<&str>,
    ) -> Self {
        let color = Arc::clone(color_conversion);
        let mut gpu = if !gpu_convert_requested(env_value) {
            info!(
                "{} が指定されているので、YUY2 → RGB の変換は CPU で行う",
                GPU_CONVERT_ENV
            );
            Self::not_prepared(CpuReason::DisabledByEnv, color)
        } else {
            match context {
                GpuContext::Glow(gl) => Self::prepare_gl(gl, color, repaint_waker),
                GpuContext::Wgpu(render_state) => {
                    Self::prepare_wgpu(render_state, color, repaint_waker)
                }
                GpuContext::None => {
                    warn!("描画のコンテキストが無いので、YUY2 → RGB の変換は CPU で行う");
                    Self::not_prepared(CpuReason::NoGpuContext, color)
                }
            }
        };
        gpu.preference = preference;
        gpu.refresh();
        gpu
    }

    /// 準備が済んだ変換器を持たせる
    fn prepared(
        converter: Converter,
        info: GpuInfo,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: &RepaintWaker,
    ) -> Self {
        let mut gpu = Self::not_prepared(CpuReason::NoGpuContext, Arc::clone(&color_conversion));
        gpu.prepared = Ok(info);
        gpu.shared = Some(Arc::new(Mutex::new(Shared {
            converter,
            pending: None,
            failure: None,
            color_conversion,
            repaint_waker: repaint_waker.clone(),
        })));
        gpu
    }

    /// wgpu: シェーダーを用意して自己診断を通す（#492）
    fn prepare_wgpu(
        render_state: &egui_wgpu::RenderState,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: &RepaintWaker,
    ) -> Self {
        let adapter = render_state.adapter.get_info();
        let device = &render_state.device;
        let prepared = WgpuConverter::new(device).and_then(|mut converter| {
            converter
                .self_test(device, &render_state.queue)
                .map(|()| converter)
        });
        match prepared {
            Ok(converter) => {
                info!(
                    "YUY2 → RGB を GPU（シェーダー）で変換できる（自己診断で CPU の変換と全画素が一致、wgpu {:?}、{}）",
                    adapter.backend, adapter.name
                );
                // WARP（CPU で描く DX12）は名前でも種類でも見分ける
                let software = adapter.device_type == wgpu::DeviceType::Cpu
                    || is_software_renderer(&adapter.name);
                let info = GpuInfo {
                    renderer: adapter.name,
                    software,
                };
                let mut gpu = Self::prepared(
                    Converter::Wgpu(Box::new(converter)),
                    info,
                    color_conversion,
                    repaint_waker,
                );
                gpu.wgpu_renderer = Some(Arc::clone(&render_state.renderer));
                gpu
            }
            Err(reason) => {
                warn!(
                    "YUY2 を GPU で変換できないので CPU で変換する（wgpu {:?}、{}）: {}",
                    adapter.backend, adapter.name, reason
                );
                Self::not_prepared(CpuReason::Unavailable(reason), color_conversion)
            }
        }
    }

    /// glow: シェーダーを用意して自己診断を通す
    fn prepare_gl(
        gl: &glow::Context,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: &RepaintWaker,
    ) -> Self {
        // SAFETY: 起動時（eframe の CreationContext を受け取ったところ、UI スレッド）で、
        // GL のコンテキストは current
        let (version, renderer) = unsafe {
            (
                gl.get_parameter_string(glow::VERSION),
                gl.get_parameter_string(glow::RENDERER),
            )
        };
        // SAFETY: 同上
        let prepared = unsafe {
            GlConverter::new(gl).and_then(|mut converter| match converter.self_test(gl, None) {
                Ok(()) => Ok(converter),
                Err(reason) => {
                    converter.destroy(gl);
                    Err(reason)
                }
            })
        };
        match prepared {
            Ok(converter) => {
                info!(
                    "YUY2 → RGB を GPU（シェーダー）で変換できる（自己診断で CPU の変換と全画素が一致、OpenGL {}、{}）",
                    version, renderer
                );
                let software = is_software_renderer(&renderer);
                let info = GpuInfo { renderer, software };
                Self::prepared(
                    Converter::Gl(converter),
                    info,
                    color_conversion,
                    repaint_waker,
                )
            }
            Err(reason) => {
                warn!(
                    "YUY2 を GPU で変換できないので CPU で変換する（OpenGL {}、{}）: {}",
                    version, renderer, reason
                );
                Self::not_prepared(CpuReason::Unavailable(reason), color_conversion)
            }
        }
    }

    /// 設定（`[video] convert`）を反映する。`apply_settings` が呼ぶ（同じ値なら何もしない）。
    /// 値が変わったら、見張りによる切り替えを取り消して数え直す（GL の失敗は取り消さない）
    pub fn set_preference(&mut self, preference: VideoConvertSetting) {
        if preference == self.preference {
            return;
        }
        self.preference = preference;
        self.too_slow = false;
        self.watch.reset();
        self.refresh();
    }

    /// 1 回の `update()` ごとに呼ぶ。描画のコールバックの失敗を取り込み、設定が自動で GPU を
    /// 使っているあいだは描画の遅さを見張る。`frame_time` は直前のフレームの描画時間
    /// （eframe の `cpu_usage`）、`stats` は見張るときだけ呼ぶ
    pub fn tick(
        &mut self,
        now: Instant,
        frame_time: Option<Duration>,
        stats: impl FnOnce() -> FrameStats,
    ) {
        if self.current == Yuy2Conversion::Gpu && self.preference == VideoConvertSetting::Auto {
            let stats = stats();
            // 見張るのは、実際に GPU で変換しているフレーム（偶数幅の YUY2）が流れている間だけ。
            // MJPEG / NV12 / RGB24 や奇数幅の YUY2 は CPU の経路で積まれるので、その間の
            // 描画が遅くても GPU の判断材料にしない（`None` を渡すと見張りは数え直す）
            let frame_interval = if stats.on_gpu {
                watched_interval(
                    stats.intervals.map(|i| i.average_ms),
                    stats.since_last_frame_ms,
                )
            } else {
                None
            };
            let observation = Observation {
                frame_time,
                frame_interval,
            };
            if self.watch.observe(now, observation) {
                self.too_slow = true;
            }
        } else {
            self.watch.reset();
        }
        self.refresh();
    }

    /// いまの判断を決め直し、フレームコールバックの旗を合わせる。変わったらログへ出す
    fn refresh(&mut self) {
        let failure = self
            .shared
            .as_ref()
            .and_then(|shared| shared.lock().ok()?.failure.clone());
        let next = decide(
            &self.prepared,
            failure.as_ref(),
            self.preference,
            self.too_slow,
        );
        if next != self.current {
            match &next {
                Yuy2Conversion::Gpu => info!("YUY2 → RGB の変換を GPU（シェーダー）で行う"),
                Yuy2Conversion::Cpu(reason) if reason.is_fallback() => warn!(
                    "YUY2 → RGB の変換を CPU で行う（GPU へは、設定を変えて適用するか起動し直すまで戻さない）: {:?}",
                    reason
                ),
                Yuy2Conversion::Cpu(reason) => {
                    info!("YUY2 → RGB の変換を CPU で行う: {:?}", reason)
                }
            }
            if self.current == Yuy2Conversion::Gpu {
                self.switched_to_cpu = true;
                // 預けたまま描かれていないフレームを捨てる。残すと、CPU で描き直した
                // テクスチャを次の描画のコールバックが古いフレームで上書きしうる
                if let Some(mut shared) = self.shared.as_ref().and_then(|s| s.lock().ok()) {
                    shared.pending = None;
                }
            }
            self.current = next;
        }
        self.color_conversion
            .set_yuy2_on_gpu(self.current == Yuy2Conversion::Gpu);
    }

    /// いま YUY2 をどこで RGB にしているか
    pub fn conversion(&self) -> Yuy2Conversion {
        self.current.clone()
    }

    /// GPU から CPU へ切り替わったあと、まだ描き直していなければ `true` を返して落とす。
    /// 呼び出し側は手元の最新フレームを CPU で描き直す（切り替えの瞬間に黒や前の画を残さない）
    pub fn take_switched_to_cpu(&mut self) -> bool {
        std::mem::take(&mut self.switched_to_cpu)
    }

    /// YUY2 のフレームを次の描画で変換するよう預ける。預けられたら `true`。GPU を使って
    /// いなければ `false` で、呼び出し側は CPU で変換して描く（切り替えの前後に届いたもの）
    pub fn queue(&self, frame: Arc<VideoFrame>) -> bool {
        // 奇数幅は 2 画素 1 組が行をまたぐのでシェーダーでは描けない。`FrameSink` が CPU へ
        // 回しているので来ないはずだが、ここでも弾く（守りを 1 か所にしない）
        if self.current != Yuy2Conversion::Gpu || !frame.width.is_multiple_of(2) {
            return false;
        }
        let Some(mut shared) = self.shared.as_ref().and_then(|s| s.lock().ok()) else {
            return false;
        };
        if shared.failure.is_some() {
            return false;
        }
        shared.pending = Some(frame);
        true
    }

    /// 預かっているフレームがあれば、それを `target`（映像のテクスチャ）へ書く
    /// コールバックを返す。**映像を描くより前に置くこと**（egui は置いた順に描く）。
    /// `rect` は映像を描く矩形（egui は大きさの無いコールバックを呼ばない）
    pub fn paint_callback(
        &self,
        target: egui::TextureId,
        rect: egui::Rect,
    ) -> Option<egui::PaintCallback> {
        let shared = self.shared.as_ref()?;
        let guard = shared.lock().ok()?;
        // 預かっていなければ置かない（同じフレームを描き直すたびに変換しない）
        let pending = guard.pending.as_ref()?;
        if let Converter::Wgpu(_) = guard.converter {
            // 書き込み先（egui の映像のテクスチャ）をここで引く。コールバックの中からは
            // レンダラーに触れない（eframe が書き込みのロックを持っている）。大きさが変わった
            // 直後は、作り直したテクスチャがレンダラーへ渡るのがこの update() の後なので、
            // 前の大きさのものしか無い。そのときは預けたまま次の update() でやり直す
            let size = (pending.width, pending.height);
            let destination = self
                .wgpu_renderer
                .as_ref()
                .and_then(|renderer| renderer.read().texture(&target)?.texture.clone())
                .filter(|t| (t.width() as usize, t.height() as usize) == size);
            let Some(destination) = destination else {
                guard.repaint_waker.wake();
                return None;
            };
            drop(guard);
            return Some(wgpu_paint_callback(Arc::clone(shared), destination, rect));
        }
        drop(guard);
        Some(gl_paint_callback(Arc::clone(shared), target, rect))
    }

    /// GL の資源を消す。アプリの終了時（`on_exit`）に呼ぶ。wgpu の資源は捨てれば消える
    pub fn destroy(&mut self, gl: &glow::Context) {
        if let Some(shared) = self.shared.take() {
            if let Ok(shared) = shared.lock() {
                if let Converter::Gl(converter) = &shared.converter {
                    // SAFETY: on_exit は UI スレッドで、GL のコンテキストが current のまま呼ばれる
                    unsafe { converter.destroy(gl) };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(software: bool) -> Result<GpuInfo, CpuReason> {
        Ok(GpuInfo {
            renderer: "Renderer".to_string(),
            software,
        })
    }

    #[test]
    fn gpu_convert_requested_is_on_unless_turned_off() {
        assert!(gpu_convert_requested(None));
        assert!(gpu_convert_requested(Some("")));
        assert!(gpu_convert_requested(Some("1")));
        assert!(gpu_convert_requested(Some("gpu")));
        assert!(!gpu_convert_requested(Some("0")));
        assert!(!gpu_convert_requested(Some(" 0 ")));
        assert!(!gpu_convert_requested(Some("False")));
        assert!(!gpu_convert_requested(Some("OFF")));
    }

    #[test]
    fn decide_uses_the_gpu_when_ready_and_allowed() {
        for preference in [VideoConvertSetting::Auto, VideoConvertSetting::Gpu] {
            assert_eq!(
                decide(&ready(false), None, preference, false),
                Yuy2Conversion::Gpu
            );
        }
    }

    #[test]
    fn decide_env_wins_over_the_setting() {
        // 環境変数（開発者向けの A/B）は設定より優先する
        assert_eq!(
            decide(
                &Err(CpuReason::DisabledByEnv),
                None,
                VideoConvertSetting::Cpu,
                false
            ),
            Yuy2Conversion::Cpu(CpuReason::DisabledByEnv)
        );
        assert_eq!(
            decide(&ready(false), None, VideoConvertSetting::Cpu, false),
            Yuy2Conversion::Cpu(CpuReason::DisabledBySetting)
        );
    }

    #[test]
    fn decide_failures_fall_back_even_when_gpu_is_chosen() {
        // 「GPU」を選んでいても、GPU で変換できなければ CPU へ戻す（強制はしない）
        assert_eq!(
            decide(
                &Err(CpuReason::Unavailable(GpuFailure::Compile("log".into()))),
                None,
                VideoConvertSetting::Gpu,
                false
            ),
            Yuy2Conversion::Cpu(CpuReason::Unavailable(GpuFailure::Compile("log".into())))
        );
        assert_eq!(
            decide(
                &ready(false),
                Some(&GpuFailure::GlError(0x502)),
                VideoConvertSetting::Gpu,
                false
            ),
            Yuy2Conversion::Cpu(CpuReason::Unavailable(GpuFailure::GlError(0x502)))
        );
    }

    #[test]
    fn decide_performance_fallbacks_apply_only_to_auto() {
        assert_eq!(
            decide(&ready(true), None, VideoConvertSetting::Auto, false),
            Yuy2Conversion::Cpu(CpuReason::SoftwareRenderer("Renderer".into()))
        );
        assert_eq!(
            decide(&ready(false), None, VideoConvertSetting::Auto, true),
            Yuy2Conversion::Cpu(CpuReason::TooSlow)
        );
        // 「GPU」を選んでいれば、ソフトウェア描画でも遅くても GPU のまま
        assert_eq!(
            decide(&ready(true), None, VideoConvertSetting::Gpu, true),
            Yuy2Conversion::Gpu
        );
    }

    fn test_gpu() -> GpuYuy2 {
        // GL を作れないテストでは、準備が済んだことにして判断の動きだけを見る
        let mut gpu = GpuYuy2 {
            prepared: ready(false),
            ..GpuYuy2::default()
        };
        gpu.refresh();
        gpu
    }

    #[test]
    fn switching_to_cpu_is_reported_once_and_clears_the_flag() {
        let mut gpu = test_gpu();
        assert_eq!(gpu.conversion(), Yuy2Conversion::Gpu);
        assert!(gpu.color_conversion.yuy2_on_gpu());
        assert!(!gpu.take_switched_to_cpu());

        gpu.set_preference(VideoConvertSetting::Cpu);
        assert!(!gpu.color_conversion.yuy2_on_gpu());
        assert!(gpu.take_switched_to_cpu(), "切り替えたら 1 回だけ描き直す");
        assert!(!gpu.take_switched_to_cpu());
    }

    #[test]
    fn too_slow_stays_until_the_setting_changes() {
        // 見張りが戻したら、性能が戻っても GPU へは戻さない。設定を変えて適用したら数え直す
        let mut gpu = test_gpu();
        gpu.too_slow = true;
        gpu.tick(Instant::now(), None, FrameStats::default);
        assert_eq!(gpu.conversion(), Yuy2Conversion::Cpu(CpuReason::TooSlow));
        gpu.tick(Instant::now(), Some(Duration::ZERO), FrameStats::default);
        assert_eq!(gpu.conversion(), Yuy2Conversion::Cpu(CpuReason::TooSlow));
        // 同じ値の適用では変わらない
        gpu.set_preference(VideoConvertSetting::Auto);
        assert_eq!(gpu.conversion(), Yuy2Conversion::Cpu(CpuReason::TooSlow));

        gpu.set_preference(VideoConvertSetting::Gpu);
        assert_eq!(gpu.conversion(), Yuy2Conversion::Gpu);
        gpu.set_preference(VideoConvertSetting::Auto);
        assert_eq!(gpu.conversion(), Yuy2Conversion::Gpu);
    }

    #[test]
    fn slow_paint_counts_only_while_frames_are_converted_on_the_gpu() {
        // MJPEG などで CPU の経路を通っている間は、描画が遅くても GPU を止めない。
        // 同じ描画時間でも、GPU で変換しているフレームなら猶予と 5 秒のあとに止める
        use crate::video::frame_buffer::IntervalStats;
        let stats = |on_gpu: bool| FrameStats {
            intervals: Some(IntervalStats {
                fps: 60.0,
                average_ms: 16.7,
                min_ms: 16.0,
                max_ms: 17.5,
                stddev_ms: 0.3,
                samples: 120,
            }),
            since_last_frame_ms: Some(5.0),
            on_gpu,
            ..FrameStats::default()
        };
        let slow = Some(Duration::from_millis(100));
        let start = Instant::now();
        let mut gpu = test_gpu();
        for tenth in 0..120 {
            let now = start + Duration::from_millis(tenth * 100);
            gpu.tick(now, slow, || stats(false));
        }
        assert_eq!(gpu.conversion(), Yuy2Conversion::Gpu);

        let mut gpu = test_gpu();
        for tenth in 0..120 {
            let now = start + Duration::from_millis(tenth * 100);
            gpu.tick(now, slow, || stats(true));
        }
        assert_eq!(gpu.conversion(), Yuy2Conversion::Cpu(CpuReason::TooSlow));
    }

    #[test]
    fn init_without_gl_stays_on_the_cpu() {
        let color = Arc::new(SharedColorConversion::new());
        let gpu = GpuYuy2::init(
            GpuContext::None,
            &color,
            &RepaintWaker::default(),
            VideoConvertSetting::Auto,
            None,
        );
        assert_eq!(
            gpu.conversion(),
            Yuy2Conversion::Cpu(CpuReason::NoGpuContext)
        );
        assert!(!color.yuy2_on_gpu(), "GPU を使えないなら旗を立てない");
    }

    #[test]
    fn init_turned_off_by_env_stays_on_the_cpu() {
        let color = Arc::new(SharedColorConversion::new());
        let gpu = GpuYuy2::init(
            GpuContext::None,
            &color,
            &RepaintWaker::default(),
            VideoConvertSetting::Gpu,
            Some("0"),
        );
        assert_eq!(
            gpu.conversion(),
            Yuy2Conversion::Cpu(CpuReason::DisabledByEnv)
        );
        assert!(!color.yuy2_on_gpu());
    }

    #[test]
    fn queue_without_gpu_refuses_the_frame() {
        // 呼び出し側は CPU で変換して描く
        let frame = Arc::new(VideoFrame {
            width: 2,
            height: 1,
            data: vec![16, 128, 16, 128],
            format: super::super::frame_format::PixelFormat::Yuy2(super::super::color::BT601),
        });
        let gpu = GpuYuy2::default();
        assert!(!gpu.queue(frame));
        assert!(gpu
            .paint_callback(
                egui::TextureId::default(),
                egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0))
            )
            .is_none());
    }
}
