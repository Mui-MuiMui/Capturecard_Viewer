//! 本番のバックエンド（`system.rs`）が使う、映像デバイスの一覧の突き合わせ
//! （`merge_video_devices`）。#536 で `system_route.rs` から分けた。デバイスに触らず、
//! 単体テストできる。

use crate::video::directshow_display_name;

/// Media Foundation の一覧に、DirectShow にしか無いデバイスを足す。
///
/// 照合は表示名で行う。**同じ名前が両方にあれば Media Foundation のほうだけを
/// 残す**（印の無い名前で残る。自動の開き方ではまず DirectShow で開き、開けなければ
/// Media Foundation で開く。`route_for` / `attempt_with_fallback`、#490）。一覧の名前を
/// 開き方で変えないのは、設定に残る名前が変わると別のデバイス扱いになるため。
/// DirectShow 側で同じ名前が重なっていれば 1 つにする（同じ名前では
/// 選び分けられない）。DirectShow のデバイスの説明は空にする（設定画面は
/// 「名前 (説明)」と出すので、「(DirectShow)」が二重に見えるのを避ける）。
pub(super) fn merge_video_devices(
    media_foundation: Vec<(String, String)>,
    direct_show: Vec<String>,
) -> Vec<(String, String)> {
    let mut merged = media_foundation;
    let mut added: Vec<String> = Vec::new();
    for name in direct_show {
        let known = merged.iter().any(|(mf_name, _)| *mf_name == name);
        if known || added.contains(&name) {
            continue;
        }
        added.push(name);
    }
    merged.extend(
        added
            .iter()
            .map(|name| (directshow_display_name(name), String::new())),
    );
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[(String, String)]) -> Vec<&str> {
        list.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test]
    fn merge_video_devices_prefers_media_foundation_for_the_same_name() {
        let merged = merge_video_devices(
            vec![("USB Video".to_string(), "MF".to_string())],
            vec!["USB Video".to_string(), "OBS Virtual Camera".to_string()],
        );
        assert_eq!(
            names(&merged),
            vec!["USB Video", "OBS Virtual Camera (DirectShow)"]
        );
        // Media Foundation の説明は残し、DirectShow の説明は空
        assert_eq!(merged[0].1, "MF");
        assert_eq!(merged[1].1, "");
    }

    #[test]
    fn merge_video_devices_collapses_duplicate_directshow_names() {
        let merged = merge_video_devices(
            Vec::new(),
            vec!["Virtual Cam".to_string(), "Virtual Cam".to_string()],
        );
        assert_eq!(names(&merged), vec!["Virtual Cam (DirectShow)"]);
    }

    #[test]
    fn merge_video_devices_without_directshow_keeps_the_media_foundation_list() {
        let mf = vec![
            ("A".to_string(), String::new()),
            ("B".to_string(), String::new()),
        ];
        assert_eq!(merge_video_devices(mf.clone(), Vec::new()), mf);
    }

    #[test]
    fn merge_video_devices_both_empty_is_empty() {
        assert!(merge_video_devices(Vec::new(), Vec::new()).is_empty());
    }
}
