//! DirectShow のデバイスの表示名。Media Foundation の一覧と並べたときに見分けるため、
//! 名前に「(DirectShow)」を添える・外す。純粋関数。

/// DirectShow のデバイス名に添える印。**設定に保存される識別子の一部なので、
/// 翻訳しない。**
const DISPLAY_SUFFIX: &str = " (DirectShow)";

/// 表示名（「(DirectShow)」付き）を作る
pub fn display_name(friendly_name: &str) -> String {
    format!("{friendly_name}{DISPLAY_SUFFIX}")
}

/// 表示名から DirectShow の表示名（`FriendlyName`）を取り出す。
/// 「(DirectShow)」が付いていなければ `None`（Media Foundation のデバイス）
pub fn friendly_name(display_name: &str) -> Option<&str> {
    display_name
        .strip_suffix(DISPLAY_SUFFIX)
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_appends_the_suffix() {
        assert_eq!(
            display_name("OBS Virtual Camera"),
            "OBS Virtual Camera (DirectShow)"
        );
    }

    #[test]
    fn friendly_name_strips_the_suffix() {
        assert_eq!(
            friendly_name("OBS Virtual Camera (DirectShow)"),
            Some("OBS Virtual Camera")
        );
    }

    #[test]
    fn friendly_name_without_suffix_is_none() {
        // Media Foundation のデバイス名はそのまま
        assert_eq!(friendly_name("Game Capture HD60"), None);
        // 印だけで名前が空のものは DirectShow のデバイスとみなさない
        assert_eq!(friendly_name(" (DirectShow)"), None);
        assert_eq!(friendly_name(""), None);
    }

    #[test]
    fn friendly_name_round_trips_display_name() {
        let name = "USB Video (DirectShow) Device";
        assert_eq!(friendly_name(&display_name(name)), Some(name));
    }
}
