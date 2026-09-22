//! 「ホットキー」タブ。
//!
//! アクションごとの割り当ての一覧と、重複の判定を置いてある。
//! 入力ダイアログ本体は `hotkey_capture`（`docs/design/hotkeys.md`）。

use crate::hotkey::{HotkeyAction, HotkeyError};
use crate::settings::AppSettings;
use crate::status::ErrorSource;
use eframe::egui;
use log::debug;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{notice_frame, warning_label, NoticeKind, SettingsEvent};

/// 「ホットキー」タブを描く。
///
/// 以前はスクリーンショット設定タブの一部だったが、フルスクリーン切替や
/// 音量操作などスクリーンショット以外のアクションも並ぶため、タブ名と
/// 内容が合っていなかった。一覧の描画自体は `show_hotkey_assignments` を
/// そのまま使う。
pub(super) fn show_hotkey_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyError>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading("ホットキー設定");
    ui.add_space(10.0);

    show_hotkey_assignments(ui, settings, hotkey_errors, events);
}

/// アクションごとのホットキー割り当ての一覧を描く。
///
/// `hotkey_errors` は**実行中の設定**で登録できなかったもの。ドラフトの
/// 内容ではないので、割り当てを変えても「適用」を押すまで消えない。
fn show_hotkey_assignments(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    hotkey_errors: &BTreeMap<HotkeyAction, HotkeyError>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.group(|ui| {
        ui.strong("ホットキー");
        ui.add_space(5.0);
        ui.small(
            "他のアプリを操作している間も効きます。スクリーンショット以外の操作にも割り当てられます。",
        );
        ui.add_space(8.0);

        let duplicates = duplicate_hotkey_actions(&settings.hotkeys);
        // 一覧を描いている間は settings を読むだけにして、書き換えは
        // 描き終えてから行う（同じデータを読みながら書き換えないため）
        let mut clear_requested: Option<HotkeyAction> = None;

        egui::Grid::new("hotkey_assignments")
            .num_columns(4)
            .spacing([8.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for action in HotkeyAction::ALL {
                    ui.label(action.label());

                    match settings.hotkey(action) {
                        Some(hotkey) => {
                            if duplicates.contains(&action) {
                                // 表のセルなので枠は付けない。記号と太字で示し、
                                // 何が起きるかは表の下の注意書きで説明する
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} {}",
                                        NoticeKind::Warning.symbol(),
                                        hotkey
                                    ))
                                    .monospace()
                                    .strong(),
                                );
                            } else {
                                ui.label(egui::RichText::new(hotkey).monospace());
                            }
                        }
                        None => {
                            ui.weak("未設定");
                        }
                    }

                    if ui.button("設定...").clicked() {
                        // どのアクションを編集するかは呼び出し側が
                        // `HotkeyCaptureState::begin_for` で記録する。
                        // **ダイアログもここでは開かない。** 開くのは
                        // このイベントを受けた `app`
                        events.push(SettingsEvent::OpenHotkeyCapture(action));
                    }

                    let can_clear = settings.hotkey(action).is_some();
                    if ui
                        .add_enabled(can_clear, egui::Button::new("クリア"))
                        .clicked()
                    {
                        clear_requested = Some(action);
                    }

                    ui.end_row();
                }
            });

        if let Some(action) = clear_requested {
            debug!("{} のホットキーをクリアする", action.label());
            settings.set_hotkey(action, None);
        }

        if !duplicates.is_empty() {
            let names: Vec<&str> = duplicates.iter().map(|action| action.label()).collect();
            ui.add_space(5.0);
            warning_label(
                ui,
                format!(
                    "同じキーが複数のアクションに割り当てられています（{}）。適用しても、上にある側だけが有効になります。",
                    names.join("、")
                ),
            );
        }

        // 登録に失敗したものを、理由とともに出す。トーストは気付かせるための
        // もので流れて消えるため、どのアクションが失敗しているかはここで見る。
        // 見出しは status.rs の定型文をそのまま使い、通知と表現を揃える
        if !hotkey_errors.is_empty() {
            ui.add_space(5.0);
            // 失敗が複数あっても枠は 1 つにまとめる。1 行ごとに枠を重ねると、
            // 縁ばかりが並んで読みづらくなる
            notice_frame(ui, NoticeKind::Error).show(ui, |ui| {
                ui.label(format!(
                    "{} {}:",
                    NoticeKind::Error.symbol(),
                    ErrorSource::Hotkey.headline()
                ));
                for (action, error) in hotkey_errors {
                    ui.label(format!(
                        "{}（{}）— {}",
                        action.label(),
                        error.hotkey,
                        error.message
                    ));
                }
            });
        }
    });
}

/// 同じキーが 2 つ以上のアクションに割り当てられているものを返す。
///
/// 比較は表記のゆれを吸収する。`"Ctrl+S"` と `"ctrl + s"`、`"Shift+Ctrl+S"` は
/// どれも同じ `HotKey` になるため、文字列のまま比べると重複を見逃す。
pub fn duplicate_hotkey_actions(
    hotkeys: &BTreeMap<HotkeyAction, String>,
) -> BTreeSet<HotkeyAction> {
    let mut seen: HashMap<String, Vec<HotkeyAction>> = HashMap::new();
    for (action, hotkey) in hotkeys {
        seen.entry(normalize_hotkey(hotkey))
            .or_default()
            .push(*action);
    }

    seen.into_values()
        .filter(|actions| actions.len() > 1)
        .flatten()
        .collect()
}

/// ホットキー文字列を、同じキーの組み合わせなら同じになる形へ正規化する。
///
/// 大文字小文字と空白を落とし、`+` で分けた要素を並べ替える。
/// `hotkey::parse_hotkey` が修飾キーの順序を問わないことに合わせてある。
pub(super) fn normalize_hotkey(hotkey: &str) -> String {
    let mut parts: Vec<String> = hotkey
        .split('+')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    parts.sort();
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;

    use std::collections::BTreeMap;

    // ---- ホットキーの重複判定 ----

    fn hotkeys(pairs: &[(HotkeyAction, &str)]) -> BTreeMap<HotkeyAction, String> {
        pairs
            .iter()
            .map(|(action, key)| (*action, (*key).to_string()))
            .collect()
    }

    #[test]
    fn duplicate_hotkey_actions_without_duplicates_is_empty() {
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::ToggleFullscreen, "F6"),
        ]);

        assert!(duplicate_hotkey_actions(&assigned).is_empty());
    }

    #[test]
    fn duplicate_hotkey_actions_reports_both_sides() {
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "F5"),
            (HotkeyAction::ToggleFullscreen, "F6"),
            (HotkeyAction::VolumeUp, "F5"),
        ]);

        let duplicates = duplicate_hotkey_actions(&assigned);

        assert_eq!(
            duplicates.into_iter().collect::<Vec<_>>(),
            vec![HotkeyAction::Screenshot, HotkeyAction::VolumeUp]
        );
    }

    #[test]
    fn duplicate_hotkey_actions_ignores_case_and_spaces() {
        // 同じ HotKey になる書き方は重複として扱う。文字列のまま比べると
        // 見逃して、登録の段階で片方が黙って無効になる
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "Ctrl+S"),
            (HotkeyAction::VolumeUp, " ctrl + s "),
        ]);

        assert_eq!(duplicate_hotkey_actions(&assigned).len(), 2);
    }

    #[test]
    fn duplicate_hotkey_actions_ignores_modifier_order() {
        // parse_hotkey は修飾キーの順序を問わないので、判定も揃える
        let assigned = hotkeys(&[
            (HotkeyAction::Screenshot, "Ctrl+Shift+A"),
            (HotkeyAction::VolumeDown, "Shift+Ctrl+A"),
        ]);

        assert_eq!(duplicate_hotkey_actions(&assigned).len(), 2);
    }

    #[test]
    fn duplicate_hotkey_actions_empty_assignment_is_empty() {
        assert!(duplicate_hotkey_actions(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn normalize_hotkey_same_combination_gives_same_string() {
        assert_eq!(normalize_hotkey("Ctrl+S"), normalize_hotkey("ctrl+s"));
        assert_eq!(
            normalize_hotkey("Ctrl+Shift+A"),
            normalize_hotkey("shift+ctrl+a")
        );
        assert_ne!(normalize_hotkey("Ctrl+S"), normalize_hotkey("Ctrl+A"));
        assert_ne!(normalize_hotkey("Ctrl+S"), normalize_hotkey("Alt+S"));
    }
}
