//! 設定ファイルの値を寛容に読む serde の補助。
//!
//! 手で書き換えた設定ファイルの 1 項目が読めないだけで、設定全体を失わせない。
//! `deserialize_with` に渡す各項目の関数（`video.rs` などの、その設定と同じ
//! ファイルに置く）がここを呼ぶ。値はまず `toml::Value` で受けて型を確かめるので、
//! 知らない文字列・範囲外の数値に加えて、**型の違う値**（`color_space = 5`、
//! `jpeg_quality = 90.0` など）もその項目だけ既定値にして WARN を残す（#531）。
//!
//! どの項目に効かせているか、構造体レベルの `#[serde(default)]` との関係は
//! `docs/design/settings.md` の「1 項目が読めなくても設定全体を失わせない」。

use log::warn;
use serde::{Deserialize, Deserializer};

/// 文字列で書く選択肢（enum）を読む。
///
/// 文字列でない値と、`parse` が解釈できない文字列は `T::default()` にする。
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
    let value = toml::Value::deserialize(deserializer)?;
    let Some(raw) = value.as_str() else {
        warn!(
            "設定の{} {}（{}）は文字列ではないので「{}」として扱う",
            what,
            value,
            value.type_str(),
            fallback
        );
        return Ok(T::default());
    };
    Ok(parse(raw).unwrap_or_else(|| {
        warn!(
            "設定の{} \"{}\" を解釈できないので「{}」として扱う",
            what, raw, fallback
        );
        T::default()
    }))
}

/// 整数の項目を読む。整数でなければ `default` にする（小数も整数ではない）。
///
/// 範囲の確かめは呼び出し側（`clamp_integer` など）。TOML の整数は i64 なので
/// 広いほうで返す。項目の型（u8 / u32 / i32）のまま読むと、範囲外の値で
/// パースがファイル単位で失敗するため。
pub(super) fn integer<'de, D>(deserializer: D, what: &str, default: i64) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    match value.as_integer() {
        Some(raw) => Ok(raw),
        None => {
            warn!(
                "設定の{} {}（{}）は整数ではないので既定の {} として扱う",
                what,
                value,
                value.type_str(),
                default
            );
            Ok(default)
        }
    }
}

/// 数値の項目を読む。整数も受け付け、数値でなければ `default` にする。
///
/// NaN・無限大と範囲の扱いは呼び出し側（`clamp_finite`）。
pub(super) fn number<'de, D>(deserializer: D, what: &str, default: f64) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    match &value {
        toml::Value::Float(raw) => Ok(*raw),
        // i64 → f64 は大きな値で丸まるが、範囲の確かめで端へ寄せるので差し支えない
        toml::Value::Integer(raw) => Ok(*raw as f64),
        _ => {
            warn!(
                "設定の{} {}（{}）は数値ではないので既定の {} として扱う",
                what,
                value,
                value.type_str(),
                default
            );
            Ok(default)
        }
    }
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
    use super::super::store::LoadOutcome;
    use super::super::testing::FULL_CONFIG;
    use super::super::AppSettings;
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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

    // ---- 型の違う値（#531） ----

    #[derive(Debug, Default, PartialEq, Eq)]
    enum Pick {
        #[default]
        First,
        Second,
    }

    fn pick_from_str(raw: &str) -> Option<Pick> {
        match raw {
            "first" => Some(Pick::First),
            "second" => Some(Pick::Second),
            _ => None,
        }
    }

    #[derive(Debug, Deserialize)]
    struct Item {
        #[serde(deserialize_with = "read_pick")]
        pick: Pick,
        #[serde(deserialize_with = "read_count")]
        count: i64,
        #[serde(deserialize_with = "read_level")]
        level: f64,
    }

    fn read_pick<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Pick, D::Error> {
        choice(deserializer, "選択", "first", pick_from_str)
    }

    fn read_count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let raw = integer(deserializer, "回数", 5)?;
        Ok(clamp_integer("回数", "", raw, 0, 10))
    }

    fn read_level<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        number(deserializer, "量", 1.5)
    }

    fn item(pick: &str, count: &str, level: &str) -> Item {
        toml::from_str(&format!(
            "pick = {pick}\ncount = {count}\nlevel = {level}\n"
        ))
        .expect("1 項目が読めなくても全体は読めること")
    }

    #[test]
    fn values_of_the_right_type_are_read_as_written() {
        let read = item("\"second\"", "7", "2.5");
        assert_eq!(read.pick, Pick::Second);
        assert_eq!(read.count, 7);
        assert_eq!(read.level, 2.5);
        // 数値の項目は整数も受け付ける
        assert_eq!(item("\"first\"", "1", "3").level, 3.0);
    }

    #[test]
    fn unknown_strings_and_out_of_range_integers_fall_back_per_item() {
        let read = item("\"third\"", "11", "-1.0");
        assert_eq!(read.pick, Pick::First);
        assert_eq!(read.count, 10);
        assert_eq!(item("\"first\"", "-1", "0.0").count, 0);
    }

    #[test]
    fn values_of_another_type_fall_back_to_the_default_per_item() {
        for (pick, count, level) in [
            ("5", "7.0", "\"loud\""),
            ("true", "\"7\"", "true"),
            ("[\"second\"]", "[7]", "[1.0]"),
            ("{ name = \"second\" }", "{ n = 7 }", "{ v = 1.0 }"),
            ("1979-05-27", "1979-05-27", "1979-05-27"),
        ] {
            let read = item(pick, count, level);
            assert_eq!(read.pick, Pick::First, "{pick}");
            assert_eq!(read.count, 5, "{count}");
            assert_eq!(read.level, 1.5, "{level}");
        }
    }

    // ---- 読み込み全体（`AppSettings::load_from`）と退避の関係 ----

    /// `FULL_CONFIG` の `from` を `to` に置き換えたファイルを読む。3 つ目は退避したか
    fn load_with(from: &str, to: &str) -> (AppSettings, LoadOutcome, bool) {
        assert!(FULL_CONFIG.contains(from), "{from} が FULL_CONFIG に無い");
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, FULL_CONFIG.replace(from, to)).expect("書けること");
        let (settings, outcome) = AppSettings::load_from(&path);
        let backed_up = dir.path().join("default-config.toml.bak").exists();
        (settings, outcome, backed_up)
    }

    #[test]
    fn a_value_of_another_type_no_longer_backs_up_the_whole_file() {
        // 以前は型が違うだけでパース全体が失敗し、ファイルを退避して既定値で起動していた
        let written: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");
        let defaults = AppSettings::default();
        for (from, to) in [
            ("color_space = \"bt601\"", "color_space = 5"),
            ("jpeg_quality = 60", "jpeg_quality = 90.0"),
            ("volume = 80.0", "volume = \"loud\""),
            ("buffer_ms = 120", "buffer_ms = \"120\""),
            ("brightness = 10", "brightness = [10]"),
        ] {
            let (settings, outcome, backed_up) = load_with(from, to);
            assert_eq!(outcome, LoadOutcome::Loaded, "{to}");
            assert!(!backed_up, "{to}");
            // 型の違う項目以外は書いたとおりに読める
            assert_eq!(
                settings.video.device_name, written.video.device_name,
                "{to}"
            );
            assert_eq!(settings.hotkeys, written.hotkeys, "{to}");
        }

        // 型の違う項目は既定値になる
        let (settings, _, _) = load_with("color_space = \"bt601\"", "color_space = 5");
        assert_eq!(settings.video.color_space, defaults.video.color_space);
        let (settings, _, _) = load_with("jpeg_quality = 60", "jpeg_quality = 90.0");
        assert_eq!(
            settings.screenshot.jpeg_quality,
            defaults.screenshot.jpeg_quality
        );
        let (settings, _, _) = load_with("volume = 80.0", "volume = \"loud\"");
        assert_eq!(settings.ui.volume, defaults.ui.volume);
        let (settings, _, _) = load_with("buffer_ms = 120", "buffer_ms = \"120\"");
        assert_eq!(settings.audio.buffer_ms, defaults.audio.buffer_ms);
    }

    #[test]
    fn a_value_of_another_type_in_other_items_still_backs_up_the_file() {
        // 補助を通していない項目（真偽値・文字列・パス・解像度など）は今までどおり
        // パース全体が失敗し、ファイルを退避して既定値で起動する
        for (from, to) in [
            ("always_on_top = true", "always_on_top = \"yes\""),
            ("device_name = \"Capture Device\"", "device_name = 5"),
            ("resolution = [1920, 1080]", "resolution = \"1080p\""),
        ] {
            let (settings, outcome, backed_up) = load_with(from, to);
            assert!(
                matches!(outcome, LoadOutcome::FellBackToDefaults { .. }),
                "{to}: {outcome:?}"
            );
            assert!(backed_up, "{to}");
            assert_eq!(
                settings.video.device_name,
                AppSettings::default().video.device_name
            );
        }
    }
}
