//! wgpu の swapchain の present の仕方（present mode と溜めるフレーム数）。
//!
//! 既定は Mailbox と 1。撮り比べのための試験用の環境変数で Fifo / 2 に替えられる
//! （#476）。設定ファイルにも UI にも出さない。理由は `docs/design/video-pipeline.md` の
//! 「描画バックエンド」。

use eframe::egui_wgpu::wgpu;

/// present mode を選ぶ環境変数。`mailbox` か `fifo`（大文字小文字は問わない）
pub const PRESENT_MODE_ENV: &str = "CAPTURECARD_VIEWER_PRESENT_MODE";

/// 溜めるフレーム数を選ぶ環境変数。`1` か `2`
pub const FRAME_LATENCY_ENV: &str = "CAPTURECARD_VIEWER_FRAME_LATENCY";

/// 既定の present mode。**Mailbox。**
///
/// Mailbox は待たずに渡し、次の垂直同期で最新の 1 枚を出す（ティアリング無し）。
/// Fifo は垂直同期まで待たされるぶん、届いたフレームが画面に出るのが遅れうる。
/// Immediate（垂直同期を待たずに出す）はティアリングが起きるので使わない（#456、
/// ユーザー判断）。`PRESENT_MODE_ENV` でも Mailbox と Fifo しか受け付けない。
///
/// present mode は起動前の `WgpuConfiguration` で決める。eframe 0.36 は起動後に
/// 変えられなかった（`Frame::set_wgpu_surface_config` は `Frame` が持つ複製を書き換える
/// だけで、描画側へ届かなかった）。0.37 で描画側へ届くようになったが（eframe #8442。
/// `Frame` の複製は起動時の `WgpuConfiguration` の値から始まるので、起動時の指定は
/// そのまま効く）、このアプリは起動後に変えない。wgpu は対応していない present mode を
/// 指定すると落ちる（自動の Fifo への切り替えは `AutoVsync` / `AutoNoVsync` にしか無い）。
/// そのため **選んだ present mode を出せることが分かっているバックエンド（DX12）だけで
/// wgpu を使う。** wgpu-hal の DX12 は Mailbox と Fifo を常に対応一覧に載せる。
/// アダプターの選び方（`adapter_rank`）でも、ウィンドウへ選んだ present mode で出せない
/// ものは選ばない
pub const DEFAULT_PRESENT_MODE: wgpu::PresentMode = wgpu::PresentMode::Mailbox;

/// 既定の、先に描いたフレームを何枚まで溜めてよいか。**1（最小）。**
/// DX12 では `SetMaximumFrameLatency(1)` と、swapchain のバッファ 2 枚になる
pub const DEFAULT_FRAME_LATENCY: u32 = 1;

/// 試験用の環境変数の値の解釈。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvOverride<T> {
    /// 指定していない（空や空白だけも含む）。既定の値
    Unset,
    /// 指定どおり
    Explicit(T),
    /// 解釈できない値。WARN を出して既定の値にする
    Invalid(String),
}

impl<T: Copy> EnvOverride<T> {
    /// 使う値。指定が無いか解釈できないときは `default`
    pub fn value_or(&self, default: T) -> T {
        match self {
            Self::Explicit(value) => *value,
            Self::Unset | Self::Invalid(_) => default,
        }
    }
}

/// 前後の空白を除いた値を `pick` で解釈する。空なら `Unset`、解釈できなければ `Invalid`。
/// `CAPTURECARD_VIEWER_RENDERER` の解釈（`parse_renderer`）と同じ流儀
fn parse_override<T>(value: Option<&str>, pick: impl Fn(&str) -> Option<T>) -> EnvOverride<T> {
    let Some(value) = value else {
        return EnvOverride::Unset;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return EnvOverride::Unset;
    }
    match pick(trimmed) {
        Some(parsed) => EnvOverride::Explicit(parsed),
        None => EnvOverride::Invalid(value.to_string()),
    }
}

/// `CAPTURECARD_VIEWER_PRESENT_MODE` の値を解釈する。受け付けるのは `mailbox` と `fifo` だけ。
/// Immediate / AutoNoVsync（ティアリングが起きる）や AutoVsync は解釈できない値として扱う
pub fn parse_present_mode(value: Option<&str>) -> EnvOverride<wgpu::PresentMode> {
    parse_override(value, |trimmed| {
        if trimmed.eq_ignore_ascii_case("mailbox") {
            Some(wgpu::PresentMode::Mailbox)
        } else if trimmed.eq_ignore_ascii_case("fifo") {
            Some(wgpu::PresentMode::Fifo)
        } else {
            None
        }
    })
}

/// `CAPTURECARD_VIEWER_FRAME_LATENCY` の値を解釈する。受け付けるのは `1` と `2` だけ
pub fn parse_frame_latency(value: Option<&str>) -> EnvOverride<u32> {
    parse_override(value, |trimmed| match trimmed {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    })
}

/// swapchain の present の仕方。起動前に環境変数から決め、起動後は変えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentSettings {
    pub present_mode: wgpu::PresentMode,
    pub frame_latency: u32,
}

impl PresentSettings {
    /// 環境変数の値から決める。解釈できない値は WARN を出して既定の値にする。
    /// 既定から変えたときも、試験用の口を使っていると分かるよう WARN を出す。
    /// 起動時に 1 回だけ呼ぶ（`renderer::configure`）
    pub fn from_env() -> Self {
        let mode_value = std::env::var(PRESENT_MODE_ENV).ok();
        let mode = parse_present_mode(mode_value.as_deref());
        if let EnvOverride::Invalid(raw) = &mode {
            log::warn!(
                "{PRESENT_MODE_ENV} の値 {raw:?} は解釈できないので既定の {DEFAULT_PRESENT_MODE:?} にする（mailbox か fifo を指定する）"
            );
        }
        let latency_value = std::env::var(FRAME_LATENCY_ENV).ok();
        let latency = parse_frame_latency(latency_value.as_deref());
        if let EnvOverride::Invalid(raw) = &latency {
            log::warn!(
                "{FRAME_LATENCY_ENV} の値 {raw:?} は解釈できないので既定の {DEFAULT_FRAME_LATENCY} にする（1 か 2 を指定する）"
            );
        }
        let settings = Self {
            present_mode: mode.value_or(DEFAULT_PRESENT_MODE),
            frame_latency: latency.value_or(DEFAULT_FRAME_LATENCY),
        };
        if settings != Self::default() {
            log::warn!(
                "試験用の環境変数で present の仕方を既定から変えている: present mode {:?}、最大フレーム遅延 {}",
                settings.present_mode,
                settings.frame_latency
            );
        }
        settings
    }
}

impl Default for PresentSettings {
    fn default() -> Self {
        Self {
            present_mode: DEFAULT_PRESENT_MODE,
            frame_latency: DEFAULT_FRAME_LATENCY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_settings_default_is_mailbox_with_one_frame() {
        assert_eq!(
            PresentSettings::default(),
            PresentSettings {
                present_mode: wgpu::PresentMode::Mailbox,
                frame_latency: 1,
            }
        );
    }

    #[test]
    fn parse_present_mode_unset_or_blank_is_default_mailbox() {
        for value in [None, Some(""), Some("  ")] {
            let request = parse_present_mode(value);
            assert_eq!(request, EnvOverride::Unset, "{value:?}");
            assert_eq!(
                request.value_or(DEFAULT_PRESENT_MODE),
                wgpu::PresentMode::Mailbox
            );
        }
    }

    #[test]
    fn parse_present_mode_accepts_mailbox_and_fifo_case_insensitively() {
        for value in ["mailbox", "MAILBOX", " Mailbox "] {
            assert_eq!(
                parse_present_mode(Some(value)),
                EnvOverride::Explicit(wgpu::PresentMode::Mailbox),
                "{value:?}"
            );
        }
        for value in ["fifo", "FIFO", "Fifo\n"] {
            assert_eq!(
                parse_present_mode(Some(value)),
                EnvOverride::Explicit(wgpu::PresentMode::Fifo),
                "{value:?}"
            );
        }
    }

    #[test]
    fn parse_present_mode_rejects_tearing_and_unknown_modes() {
        // Immediate / AutoNoVsync はティアリングが起きるので受け付けない。
        // 打ち間違いと合わせて WARN を出して既定の Mailbox にする
        for value in [
            "immediate",
            "AutoNoVsync",
            "autovsync",
            "vsync",
            "fifo_relaxed",
            "0",
        ] {
            let request = parse_present_mode(Some(value));
            assert_eq!(request, EnvOverride::Invalid(value.to_string()));
            assert_eq!(
                request.value_or(DEFAULT_PRESENT_MODE),
                wgpu::PresentMode::Mailbox
            );
        }
    }

    #[test]
    fn parse_frame_latency_accepts_only_one_and_two() {
        assert_eq!(parse_frame_latency(None), EnvOverride::Unset);
        assert_eq!(parse_frame_latency(Some(" ")), EnvOverride::Unset);
        assert_eq!(parse_frame_latency(Some("1")), EnvOverride::Explicit(1));
        assert_eq!(parse_frame_latency(Some(" 2 ")), EnvOverride::Explicit(2));
        for value in ["0", "3", "-1", "01", "two", "1.0"] {
            let request = parse_frame_latency(Some(value));
            assert_eq!(
                request,
                EnvOverride::Invalid(value.to_string()),
                "{value:?}"
            );
            assert_eq!(request.value_or(DEFAULT_FRAME_LATENCY), 1);
        }
    }
}
