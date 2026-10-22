//! 設定ファイルの値を寛容に読む serde の補助。
//!
//! 手で書き換えた設定ファイルの 1 項目が読めないだけで、設定全体を失わせない。
//! `deserialize_with` に渡す各項目の関数（`video.rs` などの、その設定と同じ
//! ファイルに置く）がここを呼ぶ。知らない文字列と範囲外の数値は、その項目だけ
//! 既定値か範囲の端にして WARN を残す。

use log::warn;
use serde::{Deserialize, Deserializer};

/// 文字列で書く選択肢（enum）を読む。
///
/// `parse` が解釈できない文字列は `T::default()` にする。
/// `what` は項目名、`fallback` は既定値の表示名で、どちらもログにだけ使う。
pub(super) fn choice<'de, D, T>(
    deserializer: D,
    what: &str,
    fallback: &str,
    parse: fn(&str) -> Option<T>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default,
{
    let raw = String::deserialize(deserializer)?;
    Ok(parse(&raw).unwrap_or_else(|| {
        warn!(
            "設定の{} \"{}\" を解釈できないので「{}」として扱う",
            what, raw, fallback
        );
        T::default()
    }))
}

/// 整数の項目を読む。
///
/// 範囲の確かめは呼び出し側（`clamp_integer` など）。TOML の整数は i64 なので
/// 広いほうで返す。項目の型（u8 / u32 / i32）のまま読むと、範囲外の値で
/// パースがファイル単位で失敗するため。`what` と `default` は使っていない
pub(super) fn integer<'de, D>(deserializer: D, _what: &str, _default: i64) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    i64::deserialize(deserializer)
}

/// 数値の項目を読む。整数も受け付ける。
///
/// NaN・無限大と範囲の扱いは呼び出し側（`clamp_finite`）。
pub(super) fn number<'de, D>(deserializer: D, _what: &str, _default: f64) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    f64::deserialize(deserializer)
}

/// 整数を `min..=max` へ丸める。丸めたら WARN を残す。`unit` はログの単位（" ms" など）。
pub(super) fn clamp_integer(what: &str, unit: &str, raw: i64, min: i64, max: i64) -> i64 {
    let clamped = raw.clamp(min, max);
    if clamped != raw {
        warn!(
            "設定の{} {}{} は範囲外なので {}{} として扱う",
            what, raw, unit, clamped, unit
        );
    }
    clamped
}

/// 数値を f32 にして `min..=max` へ丸める。NaN・無限大（f32 に収まらない値を含む）は
/// `default` にする。どちらも WARN を残す。
///
/// NaN と無限大は clamp では落ちない（NaN.clamp(..) は NaN を返す）ため、先に倒す。
pub(super) fn clamp_finite(what: &str, raw: f64, default: f32, min: f32, max: f32) -> f32 {
    let raw = raw as f32;
    if !raw.is_finite() {
        warn!(
            "設定の{} {} は数値として扱えないので {}% として扱う",
            what, raw, default
        );
        return default;
    }
    let clamped = raw.clamp(min, max);
    if clamped != raw {
        warn!(
            "設定の{} {} は範囲外なので {} として扱う",
            what, raw, clamped
        );
    }
    clamped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_integer_keeps_values_in_range_and_clamps_the_rest() {
        assert_eq!(clamp_integer("回数", "", 7, 0, 10), 7);
        assert_eq!(clamp_integer("回数", "", 0, 0, 10), 0);
        assert_eq!(clamp_integer("回数", "", 10, 0, 10), 10);
        assert_eq!(clamp_integer("回数", "", 11, 0, 10), 10);
        assert_eq!(clamp_integer("回数", "", i64::MIN, 0, 10), 0);
    }

    #[test]
    fn clamp_finite_falls_back_on_non_finite_values() {
        assert_eq!(clamp_finite("量", 50.0, 100.0, 0.0, 200.0), 50.0);
        assert_eq!(clamp_finite("量", 300.0, 100.0, 0.0, 200.0), 200.0);
        assert_eq!(clamp_finite("量", f64::NAN, 100.0, 0.0, 200.0), 100.0);
        assert_eq!(clamp_finite("量", f64::INFINITY, 100.0, 0.0, 200.0), 100.0);
        // f32 に収まらない値は無限大になるので既定値
        assert_eq!(clamp_finite("量", 1e300, 100.0, 0.0, 200.0), 100.0);
    }
}
