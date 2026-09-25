use super::HotkeyAction;
use crate::keyboard_hook::{KeyChord, KeyboardHookError, Modifiers};
use std::fmt;

/// ホットキーを解釈できなかった、または登録できなかった理由。
///
/// 解釈（`parse_hotkey`）と登録（`HotkeyManager::register` / `try_register`）を
/// 1 つの enum にまとめてある。どちらも `ErrorSource::Hotkey` として同じ経路で
/// 表示され、呼び出し側は「どの段で失敗したか」で処理を分けないため。
///
/// **表示用の日本語はこの型の `Display` が持つ。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事
/// （`docs/design/error-reporting.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// `"Ctrl+A+B"` のように通常キーを 2 つ以上含む
    MultipleKeys,
    /// `"Ctrl+Shift"` のように修飾キーだけで通常キーが無い
    MissingKey,
    /// 対応表に無いキー名。`key` は指定されたままの文字列
    UnsupportedKey(String),
    /// 同じキーが既に別のアクションへ割り当てられている
    DuplicateAssignment { other: HotkeyAction },
    /// 押下を観測するキーボードフックを使えない
    HookUnavailable(KeyboardHookError),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::MultipleKeys => write!(f, "通常キーを 2 つ以上は指定できません"),
            HotkeyError::MissingKey => write!(f, "通常キーが指定されていません"),
            HotkeyError::UnsupportedKey(key) => write!(f, "未対応のキー: {key}"),
            HotkeyError::DuplicateAssignment { other } => {
                write!(f, "同じキーが「{}」に割り当てられています", other.label())
            }
            HotkeyError::HookUnavailable(source) => {
                write!(f, "ホットキーの仕組みを初期化できません: {source}")
            }
        }
    }
}

impl std::error::Error for HotkeyError {}

// ホットキー文字列の解析。`HotkeyManager` の状態に依存しないためフリー関数に
// してある（ユニットテストから直接呼べるようにするため）。

/// `"F5"` や `"Ctrl+Shift+A"` のような文字列を `KeyChord` に変換する。
///
/// 修飾キーだけの指定（`"Ctrl+Shift"` など）と、通常キーを 2 つ以上含む指定
/// （`"Ctrl+A+B"` など）は登録できないため、エラーにする。
pub(super) fn parse_hotkey(hotkey_str: &str) -> Result<KeyChord, HotkeyError> {
    let parts: Vec<&str> = hotkey_str.split('+').collect();
    let mut modifiers = Modifiers::empty();
    let mut key_code = None;

    for part in parts {
        let part = part.trim().to_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "alt" => modifiers |= Modifiers::ALT,
            "shift" => modifiers |= Modifiers::SHIFT,
            "win" | "windows" | "super" => modifiers |= Modifiers::SUPER,
            key => {
                // 組み合わせが持てる通常キーは 1 つだけ。黙って上書きすると
                // "Ctrl+A+B" が "Ctrl+B" として登録され、設定した覚えのない
                // キーが効いてしまうため、2 つ目を見つけた時点で弾く
                if key_code.is_some() {
                    return Err(HotkeyError::MultipleKeys);
                }
                key_code = Some(parse_key_code(key)?);
            }
        }
    }

    let vk = key_code.ok_or(HotkeyError::MissingKey)?;
    Ok(KeyChord { modifiers, vk })
}

// Win32 の仮想キーコード。英字と数字は ASCII の大文字・数字と同じ値。
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
pub(super) const VK_F1: u32 = 0x70;

/// 単一のキー名を仮想キーコードに変換する。大文字小文字と前後の空白は無視する。
///
/// 受け付けるキー名は global-hotkey を使っていたころと同じ
/// （F1〜F12、A〜Z、0〜9、Space、Enter、Escape）。
fn parse_key_code(key: &str) -> Result<u32, HotkeyError> {
    let normalized = key.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "f1" => Ok(VK_F1),
        "f2" => Ok(VK_F1 + 1),
        "f3" => Ok(VK_F1 + 2),
        "f4" => Ok(VK_F1 + 3),
        "f5" => Ok(VK_F1 + 4),
        "f6" => Ok(VK_F1 + 5),
        "f7" => Ok(VK_F1 + 6),
        "f8" => Ok(VK_F1 + 7),
        "f9" => Ok(VK_F1 + 8),
        "f10" => Ok(VK_F1 + 9),
        "f11" => Ok(VK_F1 + 10),
        "f12" => Ok(VK_F1 + 11),
        "space" => Ok(VK_SPACE),
        "enter" => Ok(VK_RETURN),
        "escape" => Ok(VK_ESCAPE),
        // 英字 1 文字と数字 1 文字。仮想キーコードは大文字と数字の ASCII と同じ
        single if single.len() == 1 && is_letter_or_digit(single.as_bytes()[0]) => {
            Ok(u32::from(single.as_bytes()[0].to_ascii_uppercase()))
        }
        _ => Err(HotkeyError::UnsupportedKey(key.to_string())),
    }
}

/// 1 文字のキー名として受け付ける文字か（小文字化したあとの英字と数字）。
fn is_letter_or_digit(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ホットキー文字列の解析 ----

    // テストで使う仮想キーコード。英字と数字は ASCII の大文字・数字と同じ値
    const VK_A: u32 = 0x41;
    const VK_M: u32 = 0x4D;
    const VK_S: u32 = 0x53;
    const VK_Z: u32 = 0x5A;
    const VK_0: u32 = 0x30;
    const VK_5: u32 = 0x35;
    const VK_9: u32 = 0x39;
    const VK_F5: u32 = 0x74;
    const VK_F9: u32 = 0x78;
    const VK_F10: u32 = 0x79;
    const VK_F12: u32 = 0x7B;

    fn assert_hotkey(actual: &KeyChord, expected_mods: Modifiers, expected_vk: u32) {
        assert_eq!(
            *actual,
            KeyChord {
                modifiers: expected_mods,
                vk: expected_vk,
            }
        );
    }

    #[test]
    fn parse_hotkey_modifier_only_returns_error() {
        // 修飾キーだけでは登録できないため、パース時点で弾く
        assert_eq!(parse_hotkey("Ctrl"), Err(HotkeyError::MissingKey));
        assert_eq!(parse_hotkey("Ctrl+Shift"), Err(HotkeyError::MissingKey));
        assert_eq!(parse_hotkey("Ctrl+Shift+Alt"), Err(HotkeyError::MissingKey));
    }

    #[test]
    fn parse_hotkey_empty_returns_error() {
        // 空文字列は「未対応のキー」ではなく「通常キーが無い」として扱う。
        // "" が split で 1 要素の空文字列になり、修飾キーにも当たらない
        assert_eq!(
            parse_hotkey(""),
            Err(HotkeyError::UnsupportedKey(String::new()))
        );
    }

    #[test]
    fn parse_hotkey_unknown_key_returns_error() {
        // 種別が分かれていれば、設定画面に「未対応のキー: f13」と出せる
        assert_eq!(
            parse_hotkey("Ctrl+Nonexistent"),
            Err(HotkeyError::UnsupportedKey("nonexistent".to_string()))
        );
        assert_eq!(
            parse_hotkey("F13"),
            Err(HotkeyError::UnsupportedKey("f13".to_string()))
        );
    }

    #[test]
    fn parse_hotkey_multiple_key_codes_returns_error() {
        // 黙って最後のキーで上書きせず、エラーにする
        assert_eq!(parse_hotkey("Ctrl+A+B"), Err(HotkeyError::MultipleKeys));
        assert_eq!(parse_hotkey("A+B"), Err(HotkeyError::MultipleKeys));
        assert_eq!(parse_hotkey("F5+F6"), Err(HotkeyError::MultipleKeys));
    }

    #[test]
    fn parse_hotkey_single_key_has_no_modifiers() {
        let hotkey = parse_hotkey("F5").expect("F5 は解析できる");
        assert_hotkey(&hotkey, Modifiers::empty(), VK_F5);
    }

    #[test]
    fn parse_hotkey_with_one_modifier_sets_that_modifier() {
        let hotkey = parse_hotkey("Ctrl+S").expect("Ctrl+S は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL, VK_S);
    }

    #[test]
    fn parse_hotkey_with_two_modifiers_sets_both() {
        let hotkey = parse_hotkey("Ctrl+Shift+A").expect("Ctrl+Shift+A は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);
    }

    #[test]
    fn parse_hotkey_accepts_modifier_aliases() {
        let control = parse_hotkey("Control+A").expect("Control は Ctrl の別名");
        assert_hotkey(&control, Modifiers::CONTROL, VK_A);

        let win = parse_hotkey("Win+A").expect("Win は Super の別名");
        assert_hotkey(&win, Modifiers::SUPER, VK_A);

        let windows = parse_hotkey("Windows+A").expect("Windows は Super の別名");
        assert_hotkey(&windows, Modifiers::SUPER, VK_A);

        let superkey = parse_hotkey("Super+A").expect("Super はそのまま使える");
        assert_hotkey(&superkey, Modifiers::SUPER, VK_A);
    }

    #[test]
    fn parse_hotkey_is_case_insensitive() {
        let upper = parse_hotkey("CTRL+SHIFT+A").expect("大文字でも解析できる");
        assert_hotkey(&upper, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);

        let lower = parse_hotkey("ctrl+shift+a").expect("小文字でも解析できる");
        assert_hotkey(&lower, Modifiers::CONTROL | Modifiers::SHIFT, VK_A);
    }

    #[test]
    fn parse_hotkey_ignores_spaces_around_parts() {
        let hotkey = parse_hotkey(" Ctrl + S ").expect("前後の空白は無視する");
        assert_hotkey(&hotkey, Modifiers::CONTROL, VK_S);
    }

    #[test]
    fn parse_key_code_letters_are_mapped() {
        assert_eq!(parse_key_code("a"), Ok(VK_A));
        assert_eq!(parse_key_code("m"), Ok(VK_M));
        assert_eq!(parse_key_code("z"), Ok(VK_Z));
    }

    #[test]
    fn parse_key_code_function_keys_are_mapped() {
        assert_eq!(parse_key_code("f1"), Ok(VK_F1));
        assert_eq!(parse_key_code("f9"), Ok(VK_F9));
        assert_eq!(parse_key_code("f10"), Ok(VK_F10));
        assert_eq!(parse_key_code("f12"), Ok(VK_F12));
    }

    #[test]
    fn parse_key_code_digits_are_mapped() {
        assert_eq!(parse_key_code("0"), Ok(VK_0));
        assert_eq!(parse_key_code("5"), Ok(VK_5));
        assert_eq!(parse_key_code("9"), Ok(VK_9));
    }

    #[test]
    fn parse_hotkey_digit_with_modifiers_is_accepted() {
        let hotkey = parse_hotkey("Ctrl+Shift+9").expect("Ctrl+Shift+9 は解析できる");
        assert_hotkey(&hotkey, Modifiers::CONTROL | Modifiers::SHIFT, VK_9);
    }

    #[test]
    fn parse_key_code_named_keys_are_mapped() {
        assert_eq!(parse_key_code("space"), Ok(VK_SPACE));
        assert_eq!(parse_key_code("enter"), Ok(VK_RETURN));
        assert_eq!(parse_key_code("escape"), Ok(VK_ESCAPE));
    }

    #[test]
    fn parse_key_code_uppercase_is_accepted() {
        // parse_hotkey は小文字化してから渡すが、直接呼ばれても同じ結果になること
        assert_eq!(parse_key_code("A"), Ok(VK_A));
        assert_eq!(parse_key_code("F5"), Ok(VK_F5));
        assert_eq!(parse_key_code("Space"), Ok(VK_SPACE));
    }

    #[test]
    fn parse_key_code_unknown_key_returns_error() {
        assert!(parse_key_code("f13").is_err());
        assert!(parse_key_code("").is_err());
        assert!(parse_key_code("ctrl").is_err());
        // 1 文字でも英字と数字以外は受け付けない（global-hotkey のころと同じ）
        assert!(parse_key_code("-").is_err());
        assert!(parse_key_code("あ").is_err());
        // F キーは表にある 12 個だけ。数値として読んで範囲を広げない
        assert!(parse_key_code("f0").is_err());
        assert!(parse_key_code("f01").is_err());
    }

    // ---- エラーの文言 ----

    #[test]
    fn hotkey_error_display_keeps_the_key_and_the_underlying_reason() {
        // 文言はそのままトーストと設定画面の一覧に出る。キー名や下位の
        // エラー文が落ちると、何を直せばよいのか分からなくなる
        assert_eq!(
            HotkeyError::UnsupportedKey("f13".to_string()).to_string(),
            "未対応のキー: f13"
        );
        assert_eq!(
            HotkeyError::DuplicateAssignment {
                other: HotkeyAction::ToggleFullscreen,
            }
            .to_string(),
            "同じキーが「フルスクリーン切替」に割り当てられています"
        );
        assert_eq!(
            HotkeyError::HookUnavailable(KeyboardHookError::InstallFailed(
                "access denied".to_string()
            ))
            .to_string(),
            "ホットキーの仕組みを初期化できません: キーボードフックを登録できません: access denied"
        );
    }

    #[test]
    fn hotkey_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            HotkeyError::MultipleKeys,
            HotkeyError::MissingKey,
            HotkeyError::UnsupportedKey("f13".to_string()),
            HotkeyError::DuplicateAssignment {
                other: HotkeyAction::Screenshot,
            },
            HotkeyError::HookUnavailable(KeyboardHookError::Unsupported),
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
}
