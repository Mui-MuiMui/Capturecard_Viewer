//! Media Foundation のソースリーダーに `MF_LOW_LATENCY` を付けるかの切り替え（#456）。
//!
//! 既定は付ける。環境変数 `CAPTURECARD_VIEWER_MF_LOW_LATENCY` に `0` などを
//! 指定すると付けない。**効果を撮り比べるための開発者向けの切り替えで、
//! 設定ファイルの項目にはしない**（`docs/LATENCY.md`）。
//!
//! 属性を付けるのは nokhwa-bindings-windows の中（`vendor/` に置いた版）で、
//! ここは環境変数を読んでその旗を立てるだけ。解釈は純粋関数
//! （`parse_low_latency`）にしてある。理由は `docs/design/video-pipeline.md` の
//! 「Media Foundation のソースリーダーの低遅延モード（#456）」。

use log::{info, warn};

/// `MF_LOW_LATENCY` を付けるかを切り替える環境変数。
const MF_LOW_LATENCY_ENV: &str = "CAPTURECARD_VIEWER_MF_LOW_LATENCY";

/// 環境変数の値の解釈。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LowLatencyOverride {
    /// 未指定か空。既定（付ける）
    Unset,
    /// `1` / `true` / `on` / `yes`
    Enabled,
    /// `0` / `false` / `off` / `no`
    Disabled,
    /// どれでもない値。既定（付ける）に倒す
    Unrecognized(String),
}

impl LowLatencyOverride {
    /// `MF_LOW_LATENCY` を付けるか。外すのは明示的に OFF を指定したときだけ
    fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// 環境変数の値を解釈する。前後の空白と大文字小文字は区別しない。
fn parse_low_latency(value: Option<&str>) -> LowLatencyOverride {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return LowLatencyOverride::Unset;
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => LowLatencyOverride::Enabled,
        "0" | "false" | "off" | "no" => LowLatencyOverride::Disabled,
        _ => LowLatencyOverride::Unrecognized(value.to_string()),
    }
}

/// 環境変数を読み、次に開く Media Foundation のデバイスから効くよう旗を立てる。
///
/// 実機のバックエンドを選んだときにワーカースレッドから 1 回だけ呼ぶ
/// （`app::backend::backends_from_env`）。デバイスを開く前に呼ぶこと。
pub fn apply_mf_low_latency_from_env() {
    let value = std::env::var(MF_LOW_LATENCY_ENV).ok();
    let parsed = parse_low_latency(value.as_deref());
    match &parsed {
        LowLatencyOverride::Unset | LowLatencyOverride::Enabled => {
            info!("Media Foundation のソースリーダーに MF_LOW_LATENCY を付ける");
        }
        // 撮り比べるときに、どちらで動いたかがログから分かるよう目立つ段で残す
        LowLatencyOverride::Disabled => warn!(
            "{} で OFF が指定されているので、Media Foundation のソースリーダーに MF_LOW_LATENCY を付けない",
            MF_LOW_LATENCY_ENV
        ),
        LowLatencyOverride::Unrecognized(v) => warn!(
            "{} の値 '{}' は解釈できないので既定どおり MF_LOW_LATENCY を付ける（OFF にするなら 0）",
            MF_LOW_LATENCY_ENV, v
        ),
    }
    nokhwa_bindings_windows::wmf::set_low_latency(parsed.enabled());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_blank_is_default_and_enabled() {
        for value in [None, Some(""), Some("   ")] {
            let parsed = parse_low_latency(value);
            assert_eq!(parsed, LowLatencyOverride::Unset, "{value:?}");
            assert!(parsed.enabled(), "{value:?}");
        }
    }

    #[test]
    fn off_values_disable() {
        for value in ["0", "false", "off", "no", "FALSE", "Off", " 0 "] {
            let parsed = parse_low_latency(Some(value));
            assert_eq!(parsed, LowLatencyOverride::Disabled, "{value:?}");
            assert!(!parsed.enabled(), "{value:?}");
        }
    }

    #[test]
    fn on_values_enable() {
        for value in ["1", "true", "on", "yes", "TRUE", " On "] {
            let parsed = parse_low_latency(Some(value));
            assert_eq!(parsed, LowLatencyOverride::Enabled, "{value:?}");
            assert!(parsed.enabled(), "{value:?}");
        }
    }

    // 打ち間違いで OFF になってしまわないよう、分からない値は既定（付ける）に倒す
    #[test]
    fn unrecognized_value_falls_back_to_enabled() {
        for value in ["2", "disable", "00", "-1"] {
            let parsed = parse_low_latency(Some(value));
            assert_eq!(
                parsed,
                LowLatencyOverride::Unrecognized(value.to_string()),
                "{value:?}"
            );
            assert!(parsed.enabled(), "{value:?}");
        }
    }

    #[test]
    fn unrecognized_value_is_trimmed() {
        assert_eq!(
            parse_low_latency(Some("  maybe ")),
            LowLatencyOverride::Unrecognized("maybe".to_string())
        );
    }
}
