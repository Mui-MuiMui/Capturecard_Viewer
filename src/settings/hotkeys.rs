//! ホットキーの割り当て（[hotkeys]）と、割り当て以外の設定（[hotkey_settings]）。
//! 既定の割り当てと、旧版の screenshot.hotkey からの移行
//! （`docs/design/hotkeys.md` の「旧形式からの移行」）。

use super::AppSettings;
use crate::hotkey::HotkeyAction;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// 既定のホットキー割り当て。
//
// **スクリーンショット以外は既定で未割り当てにしてある。** ホットキーは
// 既定で他のアプリを操作している間も反応するため、こちらから勝手に
// F11 や Ctrl+↑ のような一般的なキーを割り当てると、他のアプリでそのキーを
// 押すたびにこちらも動いてしまう。
pub(super) fn default_hotkeys() -> BTreeMap<HotkeyAction, String> {
    BTreeMap::from([(HotkeyAction::Screenshot, "F5".to_string())])
}

// 設定ファイルの [hotkeys] と、旧版の screenshot.hotkey から、実際に使う
// 割り当てを決める。
//
// - [hotkeys] がある（新しい版が書いた）: そのまま使う。旧版の項目は無視する
// - [hotkeys] が無い（旧版が書いた）: 既定値を土台に、screenshot.hotkey が
//   あればスクリーンショットへ移す
//
// 旧版の設定ファイルで screenshot.hotkey が欠けている場合は既定の F5 になる。
// 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった（項目ごと
// 消えるため、欠けた項目と区別できない）ので、そこは従来どおりの挙動に揃えてある。
pub(super) fn migrate_hotkeys(
    table: Option<BTreeMap<String, String>>,
    legacy_hotkey: Option<String>,
) -> BTreeMap<HotkeyAction, String> {
    let Some(table) = table else {
        let mut hotkeys = default_hotkeys();
        if let Some(hotkey) = legacy_hotkey {
            info!(
                "旧版の設定にあるスクリーンショットのホットキー {} を hotkeys へ移す",
                hotkey
            );
            hotkeys.insert(HotkeyAction::Screenshot, hotkey);
        }
        return hotkeys;
    };

    let mut hotkeys = BTreeMap::new();
    for (key, hotkey) in table {
        match HotkeyAction::from_key(&key) {
            Some(action) => {
                hotkeys.insert(action, hotkey);
            }
            // 新しい版が増やしたアクションを古い版で読んだ場合など。
            // ここでエラーにすると設定ファイル全体が読めなくなる
            None => warn!(
                "設定の hotkeys にある知らないアクション \"{}\" を無視する",
                key
            ),
        }
    }
    hotkeys
}

// ホットキーの割り当て以外の設定（「ホットキー」タブの下の段）。
//
// プリセットには入れない（`Preset` のコメント）。既定値は全て偽なので
// `Default` は導出で足りる。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    // このアプリにキーボードフォーカスがあるときだけ反応するか。
    //
    // 既定はオフ。他のアプリを操作している間も、最小化している間も
    // 反応する（#133）。オンにすると、他のアプリで同じキーを使っていても
    // こちらは動かない（#202）。どちらの場合もキーは奪わず、他のアプリにも届く
    pub only_when_focused: bool,
}

impl AppSettings {
    // アクションに割り当てられたホットキー。未割り当てなら None。
    pub fn hotkey(&self, action: HotkeyAction) -> Option<&str> {
        self.hotkeys.get(&action).map(String::as_str)
    }

    // アクションのホットキーを差し替える。`None` は割り当ての解除。
    //
    // 解除をキーの削除で表すのは、空文字と「未割り当て」を混ぜないため。
    // 空文字を入れるとパースに失敗して、毎回ログへ理由が出ることになる。
    pub fn set_hotkey(&mut self, action: HotkeyAction, hotkey: Option<String>) {
        match hotkey {
            Some(hotkey) => {
                self.hotkeys.insert(action, hotkey);
            }
            None => {
                self.hotkeys.remove(&action);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::testing::{without_key, FULL_CONFIG, LEGACY_CONFIG};

    // ---- ホットキーの移行 ----

    #[test]
    fn legacy_config_moves_screenshot_hotkey_into_hotkeys() {
        // アクション別にする前の版が書いた設定ファイル。設定していた
        // ホットキーがスクリーンショットへ移り、失われないこと
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        // 他のアクションは未割り当てのまま
        assert_eq!(settings.hotkeys.len(), 1);
        // 無関係な項目も保持される
        assert_eq!(settings.ui.volume, 80.0);
        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn legacy_config_without_hotkey_keeps_the_default_f5() {
        // 旧版では「ホットキーを外した状態」を設定ファイルに残せなかった
        // （項目ごと消えるため、欠けた項目と区別できない）。移行後も
        // 従来と同じく既定の F5 になること
        let config = without_key(LEGACY_CONFIG, "hotkey");
        assert!(
            !config.contains("hotkey ="),
            "テスト用の設定に hotkey が残っている"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("hotkey が無い旧版の設定も読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
    }

    #[test]
    fn hotkeys_section_wins_over_the_legacy_key() {
        // 手で書き換えて両方が書かれている場合。新しい形式を正とする
        let config = format!("{}\n[hotkeys]\nscreenshot = \"F8\"\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("両方あっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F8"));
    }

    #[test]
    fn empty_hotkeys_section_means_no_assignment() {
        // すべての割り当てを外した状態。セクションはあるが中身が無い。
        // 既定の F5 を入れ直してはならない
        let config = format!("{}\n[hotkeys]\n", LEGACY_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("空の [hotkeys] でも読めなければならない");

        assert!(settings.hotkeys.is_empty());
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
    }

    #[test]
    fn empty_hotkeys_survives_a_save_and_load_roundtrip() {
        // 「割り当て無し」を書き出して読み直しても、既定の F5 に戻らないこと。
        // [hotkeys] セクションごと書き出されないと、旧版の設定ファイルと
        // 区別が付かなくなる
        let mut original = AppSettings::default();
        original.hotkeys.clear();

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert!(
            serialized.contains("[hotkeys]"),
            "空でも [hotkeys] セクションが書き出されること: {}",
            serialized
        );
        assert!(restored.hotkeys.is_empty());
    }

    #[test]
    fn hotkey_settings_missing_section_reacts_without_focus() {
        // [hotkey_settings] が無い（この項目より前の版が書いた）設定ファイル。
        // 従来どおり、他のアプリを操作している間も反応する側に倒す
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定を読めなければならない");

        assert!(!settings.hotkey_settings.only_when_focused);
    }

    #[test]
    fn hotkey_settings_survive_a_save_and_load_roundtrip() {
        let mut original = AppSettings::default();
        original.hotkey_settings.only_when_focused = true;

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert!(restored.hotkey_settings.only_when_focused);
        // [hotkeys] は値が文字列である前提で読んでいる。真偽値を混ぜると
        // 旧版では [hotkeys] ごと読めなくなるので、別のセクションに書く
        assert!(
            serialized.contains("[hotkey_settings]"),
            "別のセクションに書き出されること: {}",
            serialized
        );
    }

    #[test]
    fn saved_config_does_not_keep_the_legacy_hotkey_key() {
        // 移行したあとは旧版の項目を書き戻さない。残すと 2 つの置き場所が
        // 食い違ったときにどちらが正か決まらなくなる
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        let serialized = toml::to_string(&settings).expect("設定を書き出せなければならない");

        assert!(
            !serialized.contains("hotkey = "),
            "screenshot.hotkey が書き戻されている: {}",
            serialized
        );
        assert!(serialized.contains("screenshot = \"Ctrl+S\""));
    }

    #[test]
    fn migrated_settings_drop_the_legacy_field() {
        // 読み込んだ時点で旧版の項目は空になる。残っていると、そこを見て
        // 動く処理をうっかり足せてしまう
        let settings: AppSettings =
            toml::from_str(LEGACY_CONFIG).expect("旧版の設定ファイルが読めなければならない");

        assert_eq!(settings.screenshot.legacy_hotkey, None);
    }

    #[test]
    fn unknown_hotkey_action_is_ignored_without_losing_settings() {
        // 新しい版が増やしたアクションを古い版で読んだ場合。知らない名前で
        // ファイル全体のパースを失敗させない
        let config = format!("{}mute = \"Ctrl+M\"\n", FULL_CONFIG);

        let settings: AppSettings =
            toml::from_str(&config).expect("知らないアクションがあっても読めなければならない");

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.hotkey(HotkeyAction::ToggleFullscreen), Some("F11"));
        assert_eq!(settings.hotkeys.len(), 2);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn hotkeys_are_readable_for_every_action() {
        // アクションを足したときに、設定ファイル側のキー名が読めなくなって
        // いないことを全アクションで確かめる
        let lines: String = HotkeyAction::ALL
            .iter()
            .map(|action| format!("{} = \"F5\"\n", action.as_str()))
            .collect();
        let config = format!("[hotkeys]\n{}", lines);

        let settings: AppSettings =
            toml::from_str(&config).expect("全アクションぶんの割り当てが読めなければならない");

        assert_eq!(settings.hotkeys.len(), HotkeyAction::ALL.len());
        for action in HotkeyAction::ALL {
            assert_eq!(settings.hotkey(action), Some("F5"), "{:?}", action);
        }
    }

    #[test]
    fn set_hotkey_none_removes_the_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, None);

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), None);
        assert!(settings.hotkeys.is_empty());
    }

    #[test]
    fn set_hotkey_replaces_the_existing_assignment() {
        let mut settings = AppSettings::default();

        settings.set_hotkey(HotkeyAction::Screenshot, Some("Ctrl+S".to_string()));
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Shift+1".to_string()));

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(
            settings.hotkey(HotkeyAction::VolumeUp),
            Some("Ctrl+Shift+1")
        );
    }

    #[test]
    fn default_hotkeys_assign_only_the_screenshot() {
        // 他のアクションを既定で割り当てない。ホットキーは既定で
        // 他のアプリを操作している間も反応するため、こちらから押さえない
        let settings = AppSettings::default();

        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("F5"));
        assert_eq!(settings.hotkeys.len(), 1);
    }
}
