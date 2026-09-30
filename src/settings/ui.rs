//! ウィンドウ・音量・言語の設定（[ui]）と、更新の確認の設定（[update]）。
//! 音量の範囲と言語の選択肢、それぞれの serde の補助。

use crate::i18n::{self, Text};
use log::warn;
use serde::{Deserialize, Serialize};

// 画面に出す言語の設定。設定ファイルには language = "auto" / "ja" / "en" と書かれる。
//
// 実際に使う言語（`i18n::Language`）とは別の型にしてある。こちらは
// 「自動」を持ち、OS の言語と合わせて初めて 1 つに決まるため
// （`resolve`、`docs/design/i18n.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageSetting {
    // 既存ユーザーの設定ファイルには language が無い。自動にしておけば、
    // 日本語の Windows ではこれまでどおり日本語で出る
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "en")]
    English,
}

impl LanguageSetting {
    // 設定ダイアログのコンボボックスに出す表示名
    pub fn label(self) -> &'static str {
        match self {
            LanguageSetting::Auto => Text::LanguageAuto.get(),
            LanguageSetting::Japanese => Text::LanguageJapanese.get(),
            LanguageSetting::English => Text::LanguageEnglish.get(),
        }
    }

    pub const ALL: [LanguageSetting; 3] = [
        LanguageSetting::Auto,
        LanguageSetting::Japanese,
        LanguageSetting::English,
    ];

    // 実際に使う言語を決める。`os_language` は起動時に 1 回だけ OS から
    // 推定したもの（`platform::os_ui_language`）
    pub fn resolve(self, os_language: i18n::Language) -> i18n::Language {
        match self {
            LanguageSetting::Auto => os_language,
            LanguageSetting::Japanese => i18n::Language::Japanese,
            LanguageSetting::English => i18n::Language::English,
        }
    }
}

// 設定ファイルの language に知らない値が書かれていても、設定全体を
// 失わせない。色空間と同じ考え方で、自動として扱う
fn deserialize_language<'de, D>(deserializer: D) -> Result<LanguageSetting, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(language_setting_from_str(&raw).unwrap_or_else(|| {
        warn!("設定の言語 \"{}\" を解釈できないので自動として扱う", raw);
        LanguageSetting::default()
    }))
}

fn language_setting_from_str(raw: &str) -> Option<LanguageSetting> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(LanguageSetting::Auto),
        "ja" | "japanese" => Some(LanguageSetting::Japanese),
        "en" | "english" => Some(LanguageSetting::English),
        _ => None,
    }
}

// 音量の下限と上限。100% が等倍で、そこから先は増幅になる。
// UI（スライダー・ホイール）と OSD の表示もこの範囲を前提にしている
pub const MIN_VOLUME: f32 = 0.0;

pub const MAX_VOLUME: f32 = 200.0;

// 音量の既定値。等倍
pub const DEFAULT_VOLUME: f32 = 100.0;

// 範囲外の音量が書かれていても、そのまま受け取らない。
// UI からは 0〜200% しか作れないが、設定ファイルは手で書き換えられる。
// 1000% が書かれていると OSD に「音量: 1000%」と出てしまう
// （音声側は AudioCapture::set_volume が別途 0〜2.0 に丸めている）。
//
// jpeg_quality と違い、範囲外でもパース自体は成功するので設定が失われる
// わけではない。表示と実際の音量を食い違わせないために丸めている。
//
// NaN と無限大は clamp では落ちない（NaN.clamp(..) は NaN を返す）ため、
// 先に既定値へ倒す。
fn deserialize_volume<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = f32::deserialize(deserializer)?;
    if !raw.is_finite() {
        warn!(
            "設定の音量 {} は数値として扱えないので {}% として扱う",
            raw, DEFAULT_VOLUME
        );
        return Ok(DEFAULT_VOLUME);
    }

    let clamped = raw.clamp(MIN_VOLUME, MAX_VOLUME);
    if clamped != raw {
        warn!("設定の音量 {} は範囲外なので {} として扱う", raw, clamped);
    }
    Ok(clamped)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    #[serde(deserialize_with = "deserialize_volume")]
    pub volume: f32,
    // ミュート中か。**音量とは別に持つ。** 音量 0% で代用すると、解除したときに
    // 戻すべき値が残らない。設定ダイアログには出さず、右クリックメニュー・
    // ミドルクリック・ホットキーだけで切り替える
    pub muted: bool,
    pub maintain_aspect_ratio: bool,
    pub last_window_size: Option<(f32, f32)>,
    pub last_window_pos: Option<(f32, f32)>,
    pub always_on_top: bool,
    pub enable_drag_move: bool,
    // 映像の上に FPS などの統計を重ねて出すか
    pub show_stats_overlay: bool,
    // タイトルバーと枠を消すか。デュアルモニタでサブウィンドウとして置くときに
    // 装飾が邪魔になるため。設定ダイアログには出さず、右クリックメニューだけで
    // 切り替える。**有効にすると × が無くなる** ので、右クリックメニューの
    // 「終了」と Alt+F4 が閉じる手段になる
    pub borderless: bool,
    // 画面に出す言語。設定ダイアログの「その他」タブで選び、「適用」で
    // 再起動せずに切り替わる。プリセットには入れない（`docs/design/presets.md`）
    #[serde(deserialize_with = "deserialize_language")]
    pub language: LanguageSetting,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            volume: DEFAULT_VOLUME,
            // 既定は音が出る状態にする
            muted: false,
            maintain_aspect_ratio: true,
            last_window_size: None,
            last_window_pos: None,
            always_on_top: false,
            enable_drag_move: true,
            // 常時出しているものではないので、既定は非表示にする
            show_stats_overlay: false,
            // 既定はタイトルバーありにする。装飾なしは閉じ方・動かし方が
            // 通常のウィンドウと変わるので、知らずにその状態で起動させない
            borderless: false,
            language: LanguageSetting::Auto,
        }
    }
}

// 更新の確認（「その他」タブの「更新」の欄）。`docs/design/update.md`。
//
// プリセットには入れない（`Preset` のコメント）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    // 起動時に GitHub へ新しい版を問い合わせるか。切っていても、
    // 「その他」タブの「更新を確認」からは確認できる
    pub check_on_startup: bool,
    // 起動時の確認で新しい版が見つかったとき、ダイアログで知らせるか。
    // 切っていると「その他」タブの「更新」の欄に出るだけ
    pub notify_on_startup: bool,
    // 「この版は通知しない」を選んだ版（`1.2.0` の形。`v` は付けない）。
    // この版のあいだは起動時のダイアログを出さない。もっと新しい版が出たら出す
    pub skipped_version: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            notify_on_startup: true,
            skipped_version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::testing::{without_key, FULL_CONFIG};
    use crate::settings::AppSettings;

    #[test]
    fn app_settings_missing_muted_defaults_to_unmuted() {
        // ミュートの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、音が出る状態で起動すること
        let config = without_key(FULL_CONFIG, "muted");
        assert!(
            !config.contains("muted ="),
            "テスト用の設定から muted が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("muted が欠けていても読めなければならない");

        assert!(!settings.ui.muted); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
    }

    #[test]
    fn app_settings_muted_is_read_and_kept_apart_from_volume() {
        // ミュートは音量とは別の項目。読み込みで音量へ潰れてはいけない
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.muted);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_show_stats_overlay_defaults_to_hidden() {
        // 情報表示の項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、既定の非表示になること。
        let config = without_key(FULL_CONFIG, "show_stats_overlay");
        assert!(
            !config.contains("show_stats_overlay ="),
            "テスト用の設定から show_stats_overlay が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("show_stats_overlay が欠けていても読めなければならない");

        assert!(!settings.ui.show_stats_overlay); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(!settings.ui.enable_drag_move);
    }

    #[test]
    fn app_settings_missing_borderless_defaults_to_decorated() {
        // 装飾なしの項目を足した版へ上げた直後、既存ユーザーの設定ファイルには
        // このキーが無い。欠けていても他の項目が保持され、タイトルバーありで起動すること。
        // **既定が true になると、更新しただけで × が消えたウィンドウが出てくる。**
        let config = without_key(FULL_CONFIG, "borderless");
        assert!(
            !config.contains("borderless ="),
            "テスト用の設定から borderless が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("borderless が欠けていても読めなければならない");

        assert!(!settings.ui.borderless); // 既定値は false
        assert_eq!(settings.ui.volume, 80.0);
        assert!(settings.ui.always_on_top);
        assert!(settings.ui.show_stats_overlay);
    }

    #[test]
    fn app_settings_borderless_is_read_from_the_file() {
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("設定ファイルを読めなければならない");

        assert!(settings.ui.borderless);
    }

    #[test]
    fn app_settings_volume_above_maximum_is_clamped() {
        // 手で書き換えた設定ファイル。UI からは作れない値でも読めてしまうので、
        // 表示と実際の音量が食い違わないよう上限で止める
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 1000.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 200.0);
    }

    #[test]
    fn app_settings_volume_below_minimum_is_clamped() {
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = -50.0\n").expect("範囲外でも読めなければならない");

        assert_eq!(settings.ui.volume, 0.0);
    }

    #[test]
    fn app_settings_volume_not_a_number_falls_back_to_default() {
        // TOML は nan / inf をそのまま書ける。clamp では落とせないので
        // 既定値へ倒していること
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = nan\n").expect("nan でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = inf\n").expect("inf でも読めなければならない");
        assert_eq!(settings.ui.volume, 100.0);
    }

    #[test]
    fn app_settings_volume_at_bounds_is_kept() {
        // 境界。丸めが 1 段ずれて端の値が使えなくなっていないこと
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 200.0\n").expect("上限が読めなければならない");
        assert_eq!(settings.ui.volume, 200.0);

        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 0.0\n").expect("下限が読めなければならない");
        assert_eq!(settings.ui.volume, 0.0);
    }

    #[test]
    fn language_setting_serializes_as_short_codes() {
        // 設定ファイルに書き出される綴り。ここが変わると、既に配布した版が
        // 書いた設定ファイルを読めなくなる
        for (setting, expected) in [
            (LanguageSetting::Auto, r#"language = "auto""#),
            (LanguageSetting::Japanese, r#"language = "ja""#),
            (LanguageSetting::English, r#"language = "en""#),
        ] {
            let mut settings = AppSettings::default();
            settings.ui.language = setting;
            let serialized = toml::to_string(&settings).expect("設定を書き出せること");
            assert!(serialized.contains(expected), "{}", serialized);

            let restored: AppSettings = toml::from_str(&serialized).expect("読み戻せること");
            assert_eq!(restored.ui.language, setting);
        }
    }

    #[test]
    fn language_setting_missing_defaults_to_auto() {
        // 言語の項目ができる前の設定ファイル
        let settings: AppSettings = toml::from_str("[ui]\nvolume = 40.0\n").expect("読めること");
        assert_eq!(settings.ui.language, LanguageSetting::Auto);
        assert_eq!(settings.ui.volume, 40.0);
    }

    #[test]
    fn language_setting_unknown_value_falls_back_to_auto_and_keeps_other_items() {
        let settings: AppSettings =
            toml::from_str("[ui]\nvolume = 40.0\nlanguage = \"fr\"\n").expect("読めること");
        assert_eq!(settings.ui.language, LanguageSetting::Auto);
        assert_eq!(settings.ui.volume, 40.0);
    }

    #[test]
    fn language_setting_from_str_accepts_known_spellings() {
        assert_eq!(
            language_setting_from_str("auto"),
            Some(LanguageSetting::Auto)
        );
        assert_eq!(
            language_setting_from_str(" JA "),
            Some(LanguageSetting::Japanese)
        );
        assert_eq!(
            language_setting_from_str("japanese"),
            Some(LanguageSetting::Japanese)
        );
        assert_eq!(
            language_setting_from_str("en"),
            Some(LanguageSetting::English)
        );
        assert_eq!(
            language_setting_from_str("English"),
            Some(LanguageSetting::English)
        );
        assert_eq!(language_setting_from_str(""), None);
        assert_eq!(language_setting_from_str("fr"), None);
    }

    #[test]
    fn language_setting_resolve_uses_os_language_only_for_auto() {
        use crate::i18n::Language;
        for os in [Language::Japanese, Language::English] {
            assert_eq!(LanguageSetting::Auto.resolve(os), os);
            assert_eq!(LanguageSetting::Japanese.resolve(os), Language::Japanese);
            assert_eq!(LanguageSetting::English.resolve(os), Language::English);
        }
    }

    #[test]
    fn update_settings_missing_section_uses_defaults() {
        // [update] が無い（更新の確認を足す前の版が書いた）設定ファイル。
        // 確認も通知もする側に倒し、他の項目は残る
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("[update] が無くても読めなければならない");

        assert!(settings.update.check_on_startup);
        assert!(settings.update.notify_on_startup);
        assert_eq!(settings.update.skipped_version, None);
        assert_eq!(settings.video.fps, Some(30));
    }

    #[test]
    fn update_settings_missing_one_key_keeps_the_others() {
        // 構造体レベルの #[serde(default)] なので、欠けた bool は false ではなく
        // 構造体の既定値（true）になる
        let settings: AppSettings =
            toml::from_str("[update]\nnotify_on_startup = false\nskipped_version = \"1.2.0\"\n")
                .expect("[update] の一部が欠けていても読めなければならない");

        assert!(settings.update.check_on_startup);
        assert!(!settings.update.notify_on_startup);
        assert_eq!(settings.update.skipped_version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn update_settings_survive_a_save_and_load_roundtrip() {
        let mut original = AppSettings::default();
        original.update.check_on_startup = false;
        original.update.skipped_version = Some("1.2.0".to_string());

        let serialized = toml::to_string(&original).expect("設定を書き出せなければならない");
        let restored: AppSettings =
            toml::from_str(&serialized).expect("書き出した設定を読み直せなければならない");

        assert_eq!(restored.update, original.update);
        assert!(
            serialized.contains("[update]"),
            "[update] セクションに書き出されること: {}",
            serialized
        );
    }
}
