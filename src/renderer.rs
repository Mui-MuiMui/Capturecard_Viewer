//! 描画のバックエンド（glow / wgpu）の選び方（#492。最初の形は #456 の (2) で PR #465）。
//!
//! 既定は wgpu の DX12。フリップモデルの swapchain（`DXGI_SWAP_EFFECT_FLIP_DISCARD`）で
//! present し、present mode は Mailbox（最新のフレームだけを次の垂直同期で出す。
//! ティアリングは起きない）。環境変数 `CAPTURECARD_VIEWER_RENDERER=glow` で同じ exe の
//! まま glow（OpenGL）へ戻せる。遅れの撮り比べのためで、設定ファイルには入れない。
//! DX12 の GPU のアダプターが取れないときも glow へ倒す。
//! present mode（Mailbox / Fifo）と溜めるフレーム数（1 / 2）も、撮り比べのための
//! 試験用の環境変数 `CAPTURECARD_VIEWER_PRESENT_MODE` / `CAPTURECARD_VIEWER_FRAME_LATENCY`
//! で替えられる（#476）。指定しなければ Mailbox と 1。
//!
//! **どちらで描くかは `run_native` の前に決める。** winit のイベントループは 1 プロセスに
//! 1 回しか作れないので、wgpu で起動に失敗してから glow で開き直すことはできない。
//! 起動の前に DX12 のアダプターを列挙し、デバイスまで作れるかを確かめる（`probe_dx12`）。
//!
//! 理由と成立条件は `docs/design/video-pipeline.md` の「描画バックエンド」。

use eframe::egui_wgpu::{self, wgpu, SurfaceConfig, WgpuConfiguration, WgpuSetupCreateNew};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::video::is_software_renderer;

/// 描画のバックエンドを選ぶ環境変数。`glow` か `wgpu`（大文字小文字は問わない）。
pub const RENDERER_ENV: &str = "CAPTURECARD_VIEWER_RENDERER";

mod present;

use present::PresentSettings;

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

/// アダプターを見分ける値。起動前の確認でデバイスまで作れたアダプターを、eframe が
/// 選ぶときにもう一度見つけるのに使う。
///
/// **DXGI の LUID（`DXGI_ADAPTER_DESC1::AdapterLuid`）で見分ける。** PCI のベンダー ID と
/// デバイス ID だけだと、同じ型の GPU の 2 枚挿しで 2 枚が同じ値になり、確認でデバイスを
/// 作れなかった 1 枚目を eframe が選びうる（#512）。wgpu 30 の `AdapterInfo` に LUID は無く、
/// `device_pci_bus_id` も DX12 では (ベンダー, デバイス) の一致する最初の表示アダプターの
/// 値を引くので 2 枚を見分けられない。そのため wgpu-hal の DX12 のアダプターから DXGI の
/// アダプターを借りて読む。LUID はアダプターごとに一意で、再起動かドライバーの入れ直しまで
/// 変わらない。起動前の確認と eframe は同じプロセスで数秒のうちに列挙するので食い違わない。
/// 読めないとき（DX12 以外）は `None` で、ベンダー ID とデバイス ID だけで見分ける
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdapterId {
    vendor: u32,
    device: u32,
    luid: Option<u64>,
}

fn adapter_id(adapter: &wgpu::Adapter, info: &wgpu::AdapterInfo) -> AdapterId {
    AdapterId {
        vendor: info.vendor,
        device: info.device,
        luid: dx12_adapter_luid(adapter),
    }
}

/// DX12 のアダプターの LUID。読めなければ `None`。
///
/// インスタンスは DX12 だけ（`dx12_instance_descriptor`）なので、読めないのは wgpu の
/// `as_hal` の形が変わった（eframe / wgpu を上げた）ときくらいしか無い。そのときは黙って
/// (ベンダー, デバイス) の見分け方へ戻るので、気づけるよう WARN を 1 回だけ出す
fn dx12_adapter_luid(adapter: &wgpu::Adapter) -> Option<u64> {
    let luid = read_dx12_luid(adapter);
    if luid.is_none() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            log::warn!(
                "DX12 のアダプターの LUID を読めないので、ベンダー ID とデバイス ID だけで見分ける\
                 （同じ型の GPU の 2 枚挿しでは見分けられない。wgpu の as_hal の形が変わっていないか確かめる）"
            );
        });
    }
    luid
}

fn read_dx12_luid(adapter: &wgpu::Adapter) -> Option<u64> {
    // SAFETY: wgpu-hal のアダプターを借りて記述を読むだけで、状態を変えない。
    // 借りたものはこの関数の中で手放す
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }?;
    // SAFETY: DXGI のアダプターは wgpu-hal が持っていて有効。`GetDesc1` は読むだけ
    let desc = unsafe { hal.raw_adapter().GetDesc1() }.ok()?;
    let luid = desc.AdapterLuid;
    Some(luid_value(luid.HighPart, luid.LowPart))
}

/// LUID の上位・下位を 1 つの値にする（`HighPart` は符号付きで来る）
fn luid_value(high: i32, low: u32) -> u64 {
    (u64::from(high.cast_unsigned()) << 32) | u64::from(low)
}

/// アダプターの候補。種類、名前がソフトウェア描画のものか、ウィンドウへ選んだ present mode で出せるか。
#[derive(Clone, Copy, Debug)]
struct Candidate {
    id: AdapterId,
    device_type: wgpu::DeviceType,
    /// 名前がソフトウェア描画（`video::is_software_renderer`。GPU 変換の判断と同じ見分け方）
    software_name: bool,
    presentable: bool,
}

/// アダプターの優先順位。小さいほど先に選ぶ。`None` は選ばない。
///
/// GPU（外付け → 内蔵 → 仮想）を先にする。CPU で描く WARP（Microsoft Basic Render
/// Driver）は `allow_software` のときだけ最後の候補にする（`allows_software_adapter`）。
/// WARP は種類（`DeviceType::Cpu`）でも名前でも見分ける。ドライバーによっては種類が
/// `Other` で返るため。選んだ present mode で出せないアダプターは選ばない（`present::DEFAULT_PRESENT_MODE`）
fn adapter_rank(candidate: Candidate, allow_software: bool) -> Option<u8> {
    if !candidate.presentable {
        return None;
    }
    if candidate.software_name {
        return allow_software.then_some(4);
    }
    match candidate.device_type {
        wgpu::DeviceType::DiscreteGpu => Some(0),
        wgpu::DeviceType::IntegratedGpu => Some(1),
        wgpu::DeviceType::VirtualGpu => Some(2),
        wgpu::DeviceType::Other => Some(3),
        wgpu::DeviceType::Cpu => allow_software.then_some(4),
    }
}

/// 選べる候補の位置を、試す順（優先順位、同じ順位なら列挙の順）に並べる
fn ranked_adapters(candidates: &[Candidate], allow_software: bool) -> Vec<usize> {
    let mut ranked: Vec<(u8, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, &candidate)| {
            adapter_rank(candidate, allow_software).map(|rank| (rank, index))
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, index)| index).collect()
}

/// 候補から使うアダプターの位置を選ぶ。`preferred`（起動前の確認でデバイスまで
/// 作れたもの）が選べる候補にあればそれを、無ければ最も順位の高いものを選ぶ。
/// 選べるものが無ければ `None`
fn pick_adapter(
    candidates: &[Candidate],
    allow_software: bool,
    preferred: Option<AdapterId>,
) -> Option<usize> {
    let ranked = ranked_adapters(candidates, allow_software);
    preferred
        .and_then(|id| {
            ranked
                .iter()
                .copied()
                .find(|&index| candidates[index].id == id)
        })
        .or_else(|| ranked.first().copied())
}

/// wgpu で描くときに選んだアダプター。統計 OSD とログに出す。
#[derive(Clone, Debug)]
pub struct WgpuAdapter {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    id: AdapterId,
}

impl WgpuAdapter {
    fn new(adapter: &wgpu::Adapter, info: &wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            backend: info.backend,
            device_type: info.device_type,
            id: adapter_id(adapter, info),
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
    /// wgpu で描くときの present の仕方
    present: PresentSettings,
}

impl RendererChoice {
    /// 統計 OSD に出す描画のバックエンドの説明（言語に依らない技術名）。
    /// 例: `wgpu Dx12 Mailbox latency 1 / NVIDIA GeForce RTX 4070`、`glow (OpenGL)`
    pub fn label(&self) -> String {
        format_label(self.kind, self.present, self.adapter.get())
    }
}

fn format_label(
    kind: RendererKind,
    present: PresentSettings,
    adapter: Option<&WgpuAdapter>,
) -> String {
    let PresentSettings {
        present_mode,
        frame_latency,
    } = present;
    match (kind, adapter) {
        (RendererKind::Glow, _) => "glow (OpenGL)".to_string(),
        (RendererKind::Wgpu, Some(adapter)) => format!(
            "wgpu {:?} {present_mode:?} latency {frame_latency} / {}",
            adapter.backend, adapter.name
        ),
        (RendererKind::Wgpu, None) => format!("wgpu {present_mode:?} latency {frame_latency}"),
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
    let present = match request.kind() {
        RendererKind::Glow => PresentSettings::default(),
        RendererKind::Wgpu => PresentSettings::from_env(),
    };
    let kind = match request.kind() {
        RendererKind::Glow => {
            log::info!("描画は glow（OpenGL）。{RENDERER_ENV} で指定された");
            RendererKind::Glow
        }
        RendererKind::Wgpu => match probe_dx12(allow_software) {
            Ok((info, elapsed_ms)) => {
                log::info!(
                    "描画は wgpu（DX12、present mode {:?}、最大フレーム遅延 {}）。\
                     使えるアダプター: {} ({:?})。確認に {elapsed_ms}ms",
                    present.present_mode,
                    present.frame_latency,
                    info.name,
                    info.device_type
                );
                options.wgpu_options =
                    wgpu_configuration(Arc::clone(&adapter), allow_software, info.id, present);
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
    RendererChoice {
        kind,
        adapter,
        present,
    }
}

/// DX12 のインスタンスの組み立て。起動前の確認と eframe の両方が同じものを使う。
///
/// - バックエンドは DX12 だけ（`present::DEFAULT_PRESENT_MODE` の理由）。`WGPU_BACKEND` も読まない
/// - シェーダーのコンパイラは FXC（Windows に入っている `d3dcompiler_47.dll`）。
///   既定の Auto は PATH 上の `dxcompiler.dll` を拾いうるので、どこで起動しても
///   同じになるよう固定する
pub(crate) fn dx12_instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::DX12;
    descriptor.flags = wgpu::InstanceFlags::from_build_config().with_env();
    descriptor.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    descriptor
}

/// DX12 のアダプターを列挙し、優先順位の順にデバイスを作ってみて、最初に作れた
/// アダプターを返す。作ったものはすぐ捨てる（eframe が作り直す）。かかった時間も返す。
/// 最も順位の高いアダプターでデバイスを作れなくても、次の候補で作れれば wgpu で描く
fn probe_dx12(allow_software: bool) -> Result<(WgpuAdapter, u128), String> {
    let started = Instant::now();
    let instance = wgpu::Instance::new(dx12_instance_descriptor());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
    // ウィンドウがまだ無いので、ここでは present mode を確かめられない。
    // DX12 は Mailbox と Fifo を常に出せる（`present::DEFAULT_PRESENT_MODE`）
    let candidates: Vec<_> = adapters
        .iter()
        .map(|adapter| {
            let info = adapter.get_info();
            Candidate {
                id: adapter_id(adapter, &info),
                device_type: info.device_type,
                presentable: true,
                software_name: is_software_renderer(&info.name),
            }
        })
        .collect();
    let ranked = ranked_adapters(&candidates, allow_software);
    if ranked.is_empty() {
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
    }
    let mut failures = Vec::new();
    for index in ranked {
        let adapter = &adapters[index];
        let info = adapter.get_info();
        match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("capturecard_viewer probe"),
            ..Default::default()
        })) {
            Ok(_) => {
                if !failures.is_empty() {
                    log::warn!(
                        "DX12 の順位の高いアダプターでデバイスを作れなかったので {} を使う: {}",
                        info.name,
                        failures.join(" / ")
                    );
                }
                return Ok((
                    WgpuAdapter::new(adapter, &info),
                    started.elapsed().as_millis(),
                ));
            }
            Err(e) => failures.push(format!("{} でデバイスを作れない: {e}", info.name)),
        }
    }
    Err(failures.join(" / "))
}

/// eframe へ渡す wgpu の設定。
///
/// `preferred` は起動前の確認（`probe_dx12`）でデバイスまで作れたアダプター。
/// eframe が選ぶときもそれを優先し、確認と違うアダプターで初期化しないようにする
fn wgpu_configuration(
    selected: Arc<OnceLock<WgpuAdapter>>,
    allow_software: bool,
    preferred: AdapterId,
    present: PresentSettings,
) -> WgpuConfiguration {
    let present_mode = present.present_mode;
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor = dx12_instance_descriptor();
    // アダプターは自分で選ぶ。起動前の確認と同じ順位で、確認で作れたものを優先し、
    // ウィンドウの surface へ選んだ present mode で出せるものに限る
    setup.native_adapter_selector = Some(Arc::new(move |adapters, surface| {
        let candidates: Vec<_> = adapters
            .iter()
            .map(|adapter| {
                let info = adapter.get_info();
                Candidate {
                    id: adapter_id(adapter, &info),
                    device_type: info.device_type,
                    software_name: is_software_renderer(&info.name),
                    presentable: surface.is_none_or(|surface| {
                        adapter.is_surface_supported(surface)
                            && surface
                                .get_capabilities(adapter)
                                .present_modes
                                .contains(&present_mode)
                    }),
                }
            })
            .collect();
        let index =
            pick_adapter(&candidates, allow_software, Some(preferred)).ok_or_else(|| {
                format!("ウィンドウへ {present_mode:?} で出せる DX12 のアダプターが無い")
            })?;
        let adapter = adapters[index].clone();
        let info = adapter.get_info();
        let chosen = WgpuAdapter::new(&adapter, &info);
        log::info!(
            "wgpu のアダプター: {} ({:?}, {:?}, ドライバー {} {}, LUID {})。起動前の確認と{}",
            info.name,
            info.backend,
            info.device_type,
            info.driver,
            info.driver_info,
            chosen
                .id
                .luid
                .map_or_else(|| "不明".to_string(), |luid| format!("{luid:#x}")),
            if chosen.id == preferred {
                "同じ"
            } else {
                "違う（確認で作れたものをウィンドウへ出せない）"
            }
        );
        let _ = selected.set(chosen);
        Ok(adapter)
    }));

    WgpuConfiguration {
        surface: SurfaceConfig {
            present_mode,
            desired_maximum_frame_latency: Some(present.frame_latency),
        },
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::DeviceType;

    /// LUID の読めないアダプターの見分け方（ベンダー ID とデバイス ID だけ）
    fn pci(vendor: u32, device: u32) -> AdapterId {
        AdapterId {
            vendor,
            device,
            luid: None,
        }
    }

    fn candidate(device_type: DeviceType) -> Candidate {
        candidate_with_id(device_type, pci(0, 0))
    }

    fn candidate_with_id(device_type: DeviceType, id: AdapterId) -> Candidate {
        Candidate {
            id,
            device_type,
            presentable: true,
            software_name: false,
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
        assert_eq!(pick_adapter(&candidates, true, None), Some(2));
        assert_eq!(pick_adapter(&candidates, false, None), Some(2));
    }

    #[test]
    fn pick_adapter_uses_warp_only_when_allowed() {
        // GPU の無い VM では WARP（Microsoft Basic Render Driver）しか無い。
        // 既定では選ばず glow へ倒し、wgpu と明示したときだけ使う
        let candidates = [candidate(DeviceType::Cpu)];
        assert_eq!(pick_adapter(&candidates, false, None), None);
        assert_eq!(pick_adapter(&candidates, true, None), Some(0));
    }

    #[test]
    fn pick_adapter_excludes_warp_by_name_even_if_the_type_is_other() {
        // 種類が Other で返っても、名前が WARP なら GPU として扱わない。順位も最後
        let warp_named = Candidate {
            software_name: true,
            ..candidate(DeviceType::Other)
        };
        assert_eq!(pick_adapter(&[warp_named], false, None), None);
        assert_eq!(pick_adapter(&[warp_named], true, None), Some(0));
        let candidates = [warp_named, candidate(DeviceType::VirtualGpu)];
        assert_eq!(ranked_adapters(&candidates, true), vec![1, 0]);
    }

    #[test]
    fn pick_adapter_keeps_enumeration_order_on_ties() {
        let candidates = [
            candidate(DeviceType::IntegratedGpu),
            candidate(DeviceType::IntegratedGpu),
        ];
        assert_eq!(pick_adapter(&candidates, false, None), Some(0));
    }

    #[test]
    fn pick_adapter_skips_adapters_without_the_present_mode() {
        let not_presentable = Candidate {
            presentable: false,
            ..candidate(DeviceType::DiscreteGpu)
        };
        let candidates = [not_presentable, candidate(DeviceType::IntegratedGpu)];
        assert_eq!(pick_adapter(&candidates, false, None), Some(1));
        assert_eq!(pick_adapter(&[not_presentable], true, None), None);
        assert_eq!(pick_adapter(&[], true, None), None);
    }

    #[test]
    fn ranked_adapters_lists_every_usable_adapter_in_trial_order() {
        // 起動前の確認は、この順にデバイスを作ってみる。順位の高いもので作れなくても次を試す
        let candidates = [
            candidate(DeviceType::Cpu),
            candidate(DeviceType::IntegratedGpu),
            candidate(DeviceType::DiscreteGpu),
            candidate(DeviceType::IntegratedGpu),
        ];
        assert_eq!(ranked_adapters(&candidates, false), vec![2, 1, 3]);
        assert_eq!(ranked_adapters(&candidates, true), vec![2, 1, 3, 0]);
    }

    #[test]
    fn pick_adapter_prefers_the_adapter_the_probe_could_open() {
        // 起動前の確認で外付けの GPU のデバイスを作れず、内蔵の GPU で作れたときは、
        // eframe が選ぶときも内蔵の GPU にする
        let discrete = candidate_with_id(DeviceType::DiscreteGpu, pci(0x10de, 1));
        let integrated = candidate_with_id(DeviceType::IntegratedGpu, pci(0x8086, 2));
        let candidates = [discrete, integrated];
        assert_eq!(
            pick_adapter(&candidates, false, Some(pci(0x8086, 2))),
            Some(1)
        );
        // 確認で使ったものが見つからない（ウィンドウへ出せない）ときは順位で選ぶ
        let unpresentable_integrated = Candidate {
            presentable: false,
            ..integrated
        };
        assert_eq!(
            pick_adapter(
                &[discrete, unpresentable_integrated],
                false,
                Some(pci(0x8086, 2))
            ),
            Some(0)
        );
        // 選ばない種類（WARP）を確認の結果として渡されても選ばない
        let warp = candidate_with_id(DeviceType::Cpu, pci(0x1414, 0x8c));
        assert_eq!(pick_adapter(&[warp], false, Some(pci(0x1414, 0x8c))), None);
    }

    #[test]
    fn pick_adapter_tells_twin_gpus_apart_by_luid() {
        // 同じ型の GPU の 2 枚挿し。ベンダー ID とデバイス ID は同じで LUID だけが違う。
        // 起動前の確認で 1 枚目のデバイスを作れず 2 枚目で作れたら、eframe も 2 枚目を選ぶ
        let twin = |luid| AdapterId {
            luid: Some(luid),
            ..pci(0x10de, 0x2786)
        };
        let candidates = [
            candidate_with_id(DeviceType::DiscreteGpu, twin(0x1_0000_0a00)),
            candidate_with_id(DeviceType::DiscreteGpu, twin(0x1_0000_0b00)),
        ];
        assert_eq!(
            pick_adapter(&candidates, false, Some(twin(0x1_0000_0b00))),
            Some(1)
        );
        assert_eq!(
            pick_adapter(&candidates, false, Some(twin(0x1_0000_0a00))),
            Some(0)
        );
    }

    #[test]
    fn luid_value_joins_the_high_and_low_parts() {
        assert_eq!(luid_value(0, 0x0000_d4f2), 0x0000_d4f2);
        assert_eq!(luid_value(1, 0xffff_ffff), 0x1_ffff_ffff);
        // HighPart は符号付き。負の値も符号拡張せず上位 32 ビットにそのまま入れる
        assert_eq!(luid_value(-1, 0), 0xffff_ffff_0000_0000);
    }

    #[test]
    fn label_names_backend_present_mode_and_adapter() {
        let adapter = WgpuAdapter {
            name: "Microsoft Basic Render Driver".to_string(),
            backend: wgpu::Backend::Dx12,
            device_type: DeviceType::Cpu,
            id: pci(0x1414, 0x8c),
        };
        let default = PresentSettings::default();
        assert_eq!(
            format_label(RendererKind::Wgpu, default, Some(&adapter)),
            "wgpu Dx12 Mailbox latency 1 / Microsoft Basic Render Driver"
        );
        assert_eq!(
            format_label(RendererKind::Wgpu, default, None),
            "wgpu Mailbox latency 1"
        );
        assert_eq!(
            format_label(RendererKind::Glow, default, None),
            "glow (OpenGL)"
        );
        // 試験用の環境変数で替えたときは、実際に使う値を出す
        let fifo = PresentSettings {
            present_mode: wgpu::PresentMode::Fifo,
            frame_latency: 2,
        };
        assert_eq!(
            format_label(RendererKind::Wgpu, fifo, Some(&adapter)),
            "wgpu Dx12 Fifo latency 2 / Microsoft Basic Render Driver"
        );
    }
}
