//! 描画のバックエンド（glow / wgpu）の選び方（#456 の (2)）。
//!
//! 既定は wgpu の DX12。フリップモデルの swapchain（`DXGI_SWAP_EFFECT_FLIP_DISCARD`）で
//! present し、present mode は Mailbox（最新のフレームだけを次の垂直同期で出す。
//! ティアリングは起きない）。環境変数 `CAPTURECARD_VIEWER_RENDERER=glow` で同じ exe の
//! まま glow（OpenGL）へ戻せる。遅れの撮り比べのためで、設定ファイルには入れない。
//! DX12 の GPU のアダプターが取れないときも glow へ倒す。
//!
//! **どちらで描くかは `run_native` の前に決める。** winit のイベントループは 1 プロセスに
//! 1 回しか作れないので、wgpu で起動に失敗してから glow で開き直すことはできない。
//! 起動の前に DX12 のアダプターを列挙し、デバイスまで作れるかを確かめる（`probe_dx12`）。
//!
//! 理由と成立条件は `docs/design/video-pipeline.md` の「描画バックエンド」。

use eframe::egui_wgpu::{self, wgpu, SurfaceConfig, WgpuConfiguration, WgpuSetupCreateNew};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// 描画のバックエンドを選ぶ環境変数。`glow` か `wgpu`（大文字小文字は問わない）。
pub const RENDERER_ENV: &str = "CAPTURECARD_VIEWER_RENDERER";

/// present mode。**Mailbox だけを使う。**
///
/// Mailbox は待たずに渡し、次の垂直同期で最新の 1 枚を出す（ティアリング無し）。
/// Fifo は垂直同期まで待たされるぶん、届いたフレームが画面に出るのが遅れうる。
/// Immediate（垂直同期を待たずに出す）はティアリングが起きるので使わない（#456、
/// ユーザー判断）。
///
/// eframe 0.36 は present mode を起動前の `WgpuConfiguration` で決め、起動後に
/// 変えられない（`Frame::set_wgpu_surface_config` は `Frame` が持つ複製を書き換える
/// だけで、描画側の `RenderState` へ届かない）。wgpu は対応していない present mode を
/// 指定すると落ちる（自動の Fifo への切り替えは `AutoVsync` / `AutoNoVsync` にしか無い）。
/// そのため **Mailbox を出せることが分かっているバックエンド（DX12）だけで wgpu を使う。**
/// wgpu-hal の DX12 は Mailbox と Fifo を常に対応一覧に載せる。アダプターの選び方
/// （`adapter_rank`）でも、ウィンドウへ Mailbox で出せないものは選ばない
const PRESENT_MODE: wgpu::PresentMode = wgpu::PresentMode::Mailbox;

/// 先に描いたフレームを何枚まで溜めてよいか。**1（最小）。**
/// DX12 では `SetMaximumFrameLatency(1)` と、swapchain のバッファ 2 枚になる
const MAX_FRAME_LATENCY: u32 = 1;

/// 描画のバックエンド。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererKind {
    Glow,
    Wgpu,
}

impl RendererKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Glow => "glow",
            Self::Wgpu => "wgpu",
        }
    }
}

/// 環境変数の値の解釈。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererRequest {
    /// 指定していない（空や空白だけも含む）。既定の wgpu
    Unset,
    /// 指定どおり
    Explicit(RendererKind),
    /// 解釈できない値。WARN を出して既定の wgpu にする
    Invalid(String),
}

impl RendererRequest {
    /// 望むバックエンド。指定が無いか解釈できないときは wgpu
    pub fn kind(&self) -> RendererKind {
        match self {
            Self::Explicit(kind) => *kind,
            Self::Unset | Self::Invalid(_) => RendererKind::Wgpu,
        }
    }

    /// CPU で描く DX12 のアダプター（WARP）を使ってよいか。**`wgpu` と明示したときだけ。**
    ///
    /// 既定では使わず glow へ倒す。WARP は画素を CPU で塗るので、GPU の無い VM で
    /// フェイク 720p60 を描くと CPU が 1 コアの 500% 超になり、取り込めるのは毎秒 6 枚ほど
    /// だった（同じ VM の glow は VMware の OpenGL ドライバーで 27%、毎秒 54 枚）。
    /// 明示したときは撮り比べや確認のために使わせる
    pub fn allows_software_adapter(&self) -> bool {
        matches!(self, Self::Explicit(RendererKind::Wgpu))
    }
}

/// `CAPTURECARD_VIEWER_RENDERER` の値を解釈する。前後の空白は無視し、大文字小文字は問わない。
pub fn parse_renderer(value: Option<&str>) -> RendererRequest {
    let Some(value) = value else {
        return RendererRequest::Unset;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        RendererRequest::Unset
    } else if trimmed.eq_ignore_ascii_case("glow") {
        RendererRequest::Explicit(RendererKind::Glow)
    } else if trimmed.eq_ignore_ascii_case("wgpu") {
        RendererRequest::Explicit(RendererKind::Wgpu)
    } else {
        RendererRequest::Invalid(value.to_string())
    }
}

/// アダプターの候補。種類と、ウィンドウへ `PRESENT_MODE` で出せるか。
#[derive(Clone, Copy, Debug)]
struct Candidate {
    device_type: wgpu::DeviceType,
    presentable: bool,
}

/// アダプターの優先順位。小さいほど先に選ぶ。`None` は選ばない。
///
/// GPU（外付け → 内蔵 → 仮想）を先にする。CPU で描く WARP（Microsoft Basic Render
/// Driver）は `allow_software` のときだけ最後の候補にする（`allows_software_adapter`）。
/// Mailbox で出せないアダプターは選ばない（`PRESENT_MODE`）
fn adapter_rank(candidate: Candidate, allow_software: bool) -> Option<u8> {
    if !candidate.presentable {
        return None;
    }
    match candidate.device_type {
        wgpu::DeviceType::DiscreteGpu => Some(0),
        wgpu::DeviceType::IntegratedGpu => Some(1),
        wgpu::DeviceType::VirtualGpu => Some(2),
        wgpu::DeviceType::Other => Some(3),
        wgpu::DeviceType::Cpu => allow_software.then_some(4),
    }
}

/// 候補から使うアダプターの位置を選ぶ。同じ順位なら先に列挙されたもの。
/// 選べるものが無ければ `None`
fn pick_adapter(candidates: &[Candidate], allow_software: bool) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, &candidate)| {
            adapter_rank(candidate, allow_software).map(|rank| (rank, index))
        })
        .min()
        .map(|(_, index)| index)
}

/// wgpu で描くときに選んだアダプター。統計 OSD とログに出す。
#[derive(Clone, Debug)]
pub struct WgpuAdapter {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
}

impl WgpuAdapter {
    fn from_info(info: &wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            backend: info.backend,
            device_type: info.device_type,
        }
    }
}

/// 起動時に決めた描画のバックエンド。`main.rs` が作り、ラベルをアプリへ渡す。
#[derive(Clone, Debug)]
pub struct RendererChoice {
    pub kind: RendererKind,
    /// wgpu のときに実際に選ばれたアダプター。描画側（eframe）がアダプターを選んだ
    /// 時点で入る。アプリを作るより前に選ばれるので、アプリを作るときには入っている
    adapter: Arc<OnceLock<WgpuAdapter>>,
}

impl RendererChoice {
    /// 統計 OSD に出す描画のバックエンドの説明（言語に依らない技術名）。
    /// 例: `wgpu Dx12 Mailbox / NVIDIA GeForce RTX 4070`、`glow (OpenGL)`
    pub fn label(&self) -> String {
        format_label(self.kind, self.adapter.get())
    }
}

fn format_label(kind: RendererKind, adapter: Option<&WgpuAdapter>) -> String {
    match (kind, adapter) {
        (RendererKind::Glow, _) => "glow (OpenGL)".to_string(),
        (RendererKind::Wgpu, Some(adapter)) => format!(
            "wgpu {:?} {PRESENT_MODE:?} / {}",
            adapter.backend, adapter.name
        ),
        (RendererKind::Wgpu, None) => format!("wgpu {PRESENT_MODE:?}"),
    }
}

/// 環境変数と DX12 の確認から描画のバックエンドを決め、`NativeOptions` に入れる。
/// 決めた内容はログに出す（INFO、glow へ倒したときは WARN）。
pub fn configure(options: &mut eframe::NativeOptions) -> RendererChoice {
    let value = std::env::var(RENDERER_ENV).ok();
    let request = parse_renderer(value.as_deref());
    if let RendererRequest::Invalid(raw) = &request {
        log::warn!(
            "{RENDERER_ENV} の値 {raw:?} は解釈できないので既定の wgpu にする（glow か wgpu を指定する）"
        );
    }

    let adapter = Arc::new(OnceLock::new());
    let allow_software = request.allows_software_adapter();
    let kind = match request.kind() {
        RendererKind::Glow => {
            log::info!("描画は glow（OpenGL）。{RENDERER_ENV} で指定された");
            RendererKind::Glow
        }
        RendererKind::Wgpu => match probe_dx12(allow_software) {
            Ok((info, elapsed_ms)) => {
                log::info!(
                    "描画は wgpu（DX12、present mode {PRESENT_MODE:?}、最大フレーム遅延 {MAX_FRAME_LATENCY}）。\
                     使えるアダプター: {} ({:?})。確認に {elapsed_ms}ms",
                    info.name,
                    info.device_type
                );
                options.wgpu_options = wgpu_configuration(Arc::clone(&adapter), allow_software);
                RendererKind::Wgpu
            }
            Err(reason) => {
                log::warn!("DX12 で描けないので glow（OpenGL）で描く: {reason}");
                RendererKind::Glow
            }
        },
    };
    options.renderer = match kind {
        RendererKind::Glow => eframe::Renderer::Glow,
        RendererKind::Wgpu => eframe::Renderer::Wgpu,
    };
    RendererChoice { kind, adapter }
}

/// DX12 のインスタンスの組み立て。起動前の確認と eframe の両方が同じものを使う。
///
/// - バックエンドは DX12 だけ（`PRESENT_MODE` の理由）。`WGPU_BACKEND` も読まない
/// - シェーダーのコンパイラは FXC（Windows に入っている `d3dcompiler_47.dll`）。
///   既定の Auto は PATH 上の `dxcompiler.dll` を拾いうるので、どこで起動しても
///   同じになるよう固定する
fn dx12_instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::DX12;
    descriptor.flags = wgpu::InstanceFlags::from_build_config().with_env();
    descriptor.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    descriptor
}

/// DX12 のアダプターを列挙し、選んだものでデバイスまで作れるかを確かめる。
/// 作ったものはすぐ捨てる（eframe が作り直す）。かかった時間も返す
fn probe_dx12(allow_software: bool) -> Result<(WgpuAdapter, u128), String> {
    let started = Instant::now();
    let instance = wgpu::Instance::new(dx12_instance_descriptor());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
    // ウィンドウがまだ無いので、ここでは present mode を確かめられない。
    // DX12 は Mailbox を常に出せる（`PRESENT_MODE`）
    let candidates: Vec<_> = adapters
        .iter()
        .map(|adapter| Candidate {
            device_type: adapter.get_info().device_type,
            presentable: true,
        })
        .collect();
    let Some(index) = pick_adapter(&candidates, allow_software) else {
        let names: Vec<_> = adapters
            .iter()
            .map(|adapter| {
                let info = adapter.get_info();
                format!("{} ({:?})", info.name, info.device_type)
            })
            .collect();
        return Err(format!(
            "GPU の DX12 のアダプターが無い（列挙できたもの: {names:?}。CPU で描く WARP は {RENDERER_ENV}=wgpu のときだけ使う）"
        ));
    };
    let adapter = &adapters[index];
    let info = adapter.get_info();
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("capturecard_viewer probe"),
        ..Default::default()
    }))
    .map_err(|e| format!("{} でデバイスを作れない: {e}", info.name))?;
    Ok((WgpuAdapter::from_info(&info), started.elapsed().as_millis()))
}

/// eframe へ渡す wgpu の設定。
fn wgpu_configuration(
    selected: Arc<OnceLock<WgpuAdapter>>,
    allow_software: bool,
) -> WgpuConfiguration {
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor = dx12_instance_descriptor();
    // アダプターは自分で選ぶ。起動前の確認（`probe_dx12`）と同じ順位で選び、
    // ウィンドウの surface へ Mailbox で出せるものに限る
    setup.native_adapter_selector = Some(Arc::new(move |adapters, surface| {
        let candidates: Vec<_> = adapters
            .iter()
            .map(|adapter| Candidate {
                device_type: adapter.get_info().device_type,
                presentable: surface.is_none_or(|surface| {
                    adapter.is_surface_supported(surface)
                        && surface
                            .get_capabilities(adapter)
                            .present_modes
                            .contains(&PRESENT_MODE)
                }),
            })
            .collect();
        let index = pick_adapter(&candidates, allow_software).ok_or_else(|| {
            format!("ウィンドウへ {PRESENT_MODE:?} で出せる DX12 のアダプターが無い")
        })?;
        let adapter = adapters[index].clone();
        let info = adapter.get_info();
        log::info!(
            "wgpu のアダプター: {} ({:?}, {:?}, ドライバー {} {})",
            info.name,
            info.backend,
            info.device_type,
            info.driver,
            info.driver_info
        );
        let _ = selected.set(WgpuAdapter::from_info(&info));
        Ok(adapter)
    }));

    WgpuConfiguration {
        surface: SurfaceConfig {
            present_mode: PRESENT_MODE,
            desired_maximum_frame_latency: Some(MAX_FRAME_LATENCY),
        },
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::DeviceType;

    fn candidate(device_type: DeviceType) -> Candidate {
        Candidate {
            device_type,
            presentable: true,
        }
    }

    #[test]
    fn parse_renderer_unset_or_blank_is_default_wgpu() {
        assert_eq!(parse_renderer(None), RendererRequest::Unset);
        assert_eq!(parse_renderer(Some("")), RendererRequest::Unset);
        assert_eq!(parse_renderer(Some("  ")), RendererRequest::Unset);
        assert_eq!(parse_renderer(None).kind(), RendererKind::Wgpu);
    }

    #[test]
    fn parse_renderer_accepts_names_case_insensitively() {
        for value in ["glow", "GLOW", " Glow "] {
            assert_eq!(
                parse_renderer(Some(value)),
                RendererRequest::Explicit(RendererKind::Glow),
                "{value:?}"
            );
        }
        for value in ["wgpu", "WGPU", "wgpu\n"] {
            assert_eq!(
                parse_renderer(Some(value)),
                RendererRequest::Explicit(RendererKind::Wgpu),
                "{value:?}"
            );
        }
    }

    #[test]
    fn parse_renderer_unknown_value_falls_back_to_wgpu() {
        // 打ち間違いや、受け付けない別名（opengl / dx12）は WARN を出して既定にする
        for value in ["opengl", "dx12", "gl", "wgpu-fifo", "1"] {
            let request = parse_renderer(Some(value));
            assert_eq!(request, RendererRequest::Invalid(value.to_string()));
            assert_eq!(request.kind(), RendererKind::Wgpu);
        }
    }

    #[test]
    fn software_adapter_is_allowed_only_when_wgpu_is_explicit() {
        assert!(parse_renderer(Some("wgpu")).allows_software_adapter());
        assert!(!parse_renderer(None).allows_software_adapter());
        assert!(!parse_renderer(Some("opengl")).allows_software_adapter());
        assert!(!parse_renderer(Some("glow")).allows_software_adapter());
    }

    #[test]
    fn pick_adapter_prefers_gpu_over_warp() {
        // 列挙の順で WARP が先に来ても GPU を選ぶ
        let candidates = [
            candidate(DeviceType::Cpu),
            candidate(DeviceType::IntegratedGpu),
            candidate(DeviceType::DiscreteGpu),
        ];
        assert_eq!(pick_adapter(&candidates, true), Some(2));
        assert_eq!(pick_adapter(&candidates, false), Some(2));
    }

    #[test]
    fn pick_adapter_uses_warp_only_when_allowed() {
        // GPU の無い VM では WARP（Microsoft Basic Render Driver）しか無い。
        // 既定では選ばず glow へ倒し、wgpu と明示したときだけ使う
        let candidates = [candidate(DeviceType::Cpu)];
        assert_eq!(pick_adapter(&candidates, false), None);
        assert_eq!(pick_adapter(&candidates, true), Some(0));
    }

    #[test]
    fn pick_adapter_keeps_enumeration_order_on_ties() {
        let candidates = [
            candidate(DeviceType::IntegratedGpu),
            candidate(DeviceType::IntegratedGpu),
        ];
        assert_eq!(pick_adapter(&candidates, false), Some(0));
    }

    #[test]
    fn pick_adapter_skips_adapters_without_the_present_mode() {
        let not_presentable = Candidate {
            device_type: DeviceType::DiscreteGpu,
            presentable: false,
        };
        let candidates = [not_presentable, candidate(DeviceType::IntegratedGpu)];
        assert_eq!(pick_adapter(&candidates, false), Some(1));
        assert_eq!(pick_adapter(&[not_presentable], true), None);
        assert_eq!(pick_adapter(&[], true), None);
    }

    #[test]
    fn label_names_backend_present_mode_and_adapter() {
        let adapter = WgpuAdapter {
            name: "Microsoft Basic Render Driver".to_string(),
            backend: wgpu::Backend::Dx12,
            device_type: DeviceType::Cpu,
        };
        assert_eq!(
            format_label(RendererKind::Wgpu, Some(&adapter)),
            "wgpu Dx12 Mailbox / Microsoft Basic Render Driver"
        );
        assert_eq!(format_label(RendererKind::Wgpu, None), "wgpu Mailbox");
        assert_eq!(format_label(RendererKind::Glow, None), "glow (OpenGL)");
    }
}
