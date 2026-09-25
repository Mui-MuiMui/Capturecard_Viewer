use super::HotkeyAction;
use crate::keyboard_hook::{KeyChord, KeyboardHookError, Modifiers};
use eframe::egui;
use std::collections::HashMap;
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

/// egui が受け取ったキー入力を、フックが観測するのと同じ `KeyChord` に直す。
/// ホットキーに使えないキー（Tab や矢印キーなど）は `None` を返す。
///
/// 自アプリが前面のとき、ホットキーのキーを egui から取り除くために使う（#217）。
/// **キー名は egui の `Key::name()` を `parse_key_code` へ通して引く。**
/// 対応表を別に持つと、設定ファイルで受け付けるキーと取り除くキーが食い違う。
///
/// egui の修飾キーには Windows キーが無いので、`SUPER` は付かない。
/// `Win+F5` を押したときも egui には `F5` として届くが、Windows キーとの
/// 組み合わせはたいていシェルが先に使うので、区別しない。
pub(super) fn chord_from_egui(key: egui::Key, modifiers: egui::Modifiers) -> Option<KeyChord> {
    let vk = parse_key_code(key.name()).ok()?;
    let mut chord_modifiers = Modifiers::empty();
    if modifiers.ctrl {
        chord_modifiers |= Modifiers::CONTROL;
    }
    if modifiers.alt {
        chord_modifiers |= Modifiers::ALT;
    }
    if modifiers.shift {
        chord_modifiers |= Modifiers::SHIFT;
    }
    Some(KeyChord {
        modifiers: chord_modifiers,
        vk,
    })
}

/// 自アプリが前面のとき egui へ届いたキー入力から、ホットキーに割り当てたキーの
/// 押下を取り除く。取り除いた数を返す（#217）。
///
/// キーを奪わないフックにしたので、前面にいる間は割り当てたキーが egui にも
/// 届く。Escape を割り当てると右クリックメニューも同時に閉じる、単キーが
/// 設定ダイアログのボタン操作と重なる、といった衝突を避けるため、
/// **フックが反応するキーは egui には渡さない。**
///
/// - 取り除くのは押下（キーリピートを含む）だけ。解放は残す。押下の無い解放は
///   egui では何も起こさない
/// - テキスト欄に入力中（`typing`）なら何も取り除かない。その間はリスナーが
///   押下を捨てている（`listener::rejected_by_window_state`）ので、キーは
///   egui のものになる
/// - 照合は登録中の表（`registered`）で、フックと同じく修飾キーの完全一致。
///   ホットキー入力ダイアログを開いている間は表が空（`pause`）なので、押した
///   キーはそのままダイアログへ届く。デバウンスや「フォーカスがあるときだけ
///   反応する」は見ない。egui にキーが届くのは前面にいるときだけで、
///   デバウンスで捨てた押下もホットキーのキーであることに変わりはないため
pub(super) fn remove_hotkey_key_events(
    registered: &HashMap<KeyChord, HotkeyAction>,
    events: &mut Vec<egui::Event>,
    typing: bool,
) -> usize {
    if typing || registered.is_empty() {
        return 0;
    }
    let before = events.len();
    events.retain(|event| match event {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => chord_from_egui(*key, *modifiers).is_none_or(|chord| !registered.contains_key(&chord)),
        _ => true,
    });
    before - events.len()
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

    // ---- egui のキー入力 → KeyChord ----

    #[test]
    fn chord_from_egui_matches_the_parsed_hotkey() {
        // 設定ファイルの文字列を解析した結果と、同じキーを egui で押した結果が
        // 一致しないと、フックが反応したキーを egui から取り除けない
        let cases = [
            (egui::Key::F5, egui::Modifiers::NONE, "F5"),
            (egui::Key::Escape, egui::Modifiers::NONE, "Escape"),
            (egui::Key::Enter, egui::Modifiers::NONE, "Enter"),
            (egui::Key::Space, egui::Modifiers::NONE, "Space"),
            (egui::Key::A, egui::Modifiers::NONE, "A"),
            (egui::Key::Num0, egui::Modifiers::NONE, "0"),
            (egui::Key::F12, egui::Modifiers::CTRL, "Ctrl+F12"),
            (
                egui::Key::F9,
                egui::Modifiers::CTRL | egui::Modifiers::ALT,
                "Ctrl+Alt+F9",
            ),
            (egui::Key::S, egui::Modifiers::SHIFT, "Shift+S"),
        ];

        for (key, modifiers, hotkey) in cases {
            assert_eq!(
                chord_from_egui(key, modifiers),
                Some(parse_hotkey(hotkey).expect("解析できること")),
                "{hotkey}"
            );
        }
    }

    #[test]
    fn chord_from_egui_unsupported_key_returns_none() {
        // ホットキーに割り当てられないキーは、取り除く対象にもならない
        for key in [egui::Key::Tab, egui::Key::ArrowDown, egui::Key::Plus] {
            assert_eq!(chord_from_egui(key, egui::Modifiers::NONE), None, "{key:?}");
        }
    }

    // ---- egui へ届いたキー入力からホットキーのキーを取り除く（#217） ----

    fn key_event(
        key: egui::Key,
        modifiers: egui::Modifiers,
        pressed: bool,
        repeat: bool,
    ) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        }
    }

    /// F5 → スクリーンショット、Ctrl+F11 → フルスクリーン切替
    fn registered_chords() -> HashMap<KeyChord, HotkeyAction> {
        HashMap::from([
            (
                parse_hotkey("F5").expect("解析できること"),
                HotkeyAction::Screenshot,
            ),
            (
                parse_hotkey("Ctrl+F11").expect("解析できること"),
                HotkeyAction::ToggleFullscreen,
            ),
        ])
    }

    #[test]
    fn remove_hotkey_key_events_removes_presses_of_assigned_keys() {
        // F5 の押下とキーリピートは取り除き、解放と割り当てていないキーは残す
        let mut events = vec![
            key_event(egui::Key::F5, egui::Modifiers::NONE, true, false),
            key_event(egui::Key::F5, egui::Modifiers::NONE, true, true),
            key_event(egui::Key::F5, egui::Modifiers::NONE, false, false),
            key_event(egui::Key::Escape, egui::Modifiers::NONE, true, false),
            egui::Event::Text("a".to_string()),
        ];

        let removed = remove_hotkey_key_events(&registered_chords(), &mut events, false);

        assert_eq!(removed, 2);
        assert_eq!(
            events,
            vec![
                key_event(egui::Key::F5, egui::Modifiers::NONE, false, false),
                key_event(egui::Key::Escape, egui::Modifiers::NONE, true, false),
                egui::Event::Text("a".to_string()),
            ]
        );
    }

    #[test]
    fn remove_hotkey_key_events_compares_modifiers_exactly() {
        // フックと同じく修飾キーは完全一致。F5 の割り当てで Ctrl+F5 は取り除かず、
        // Ctrl+F11 の割り当てで F11 単独も取り除かない
        let mut events = vec![
            key_event(egui::Key::F5, egui::Modifiers::CTRL, true, false),
            key_event(egui::Key::F11, egui::Modifiers::NONE, true, false),
            key_event(egui::Key::F11, egui::Modifiers::CTRL, true, false),
        ];

        let removed = remove_hotkey_key_events(&registered_chords(), &mut events, false);

        assert_eq!(removed, 1);
        assert_eq!(
            events,
            vec![
                key_event(egui::Key::F5, egui::Modifiers::CTRL, true, false),
                key_event(egui::Key::F11, egui::Modifiers::NONE, true, false),
            ]
        );
    }

    #[test]
    fn remove_hotkey_key_events_keeps_everything_while_typing() {
        // 入力中はリスナーが押下を捨てるので、キーは egui（テキスト欄）へ渡す（#206）
        let mut events = vec![key_event(egui::Key::F5, egui::Modifiers::NONE, true, false)];

        let removed = remove_hotkey_key_events(&registered_chords(), &mut events, true);

        assert_eq!(removed, 0);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn remove_hotkey_key_events_keeps_everything_when_nothing_is_registered() {
        // ホットキー入力ダイアログを開いている間（pause）は表が空。
        // 押したキーがダイアログへ届かないと割り当てられない
        let mut events = vec![key_event(egui::Key::F5, egui::Modifiers::NONE, true, false)];

        let removed = remove_hotkey_key_events(&HashMap::new(), &mut events, false);

        assert_eq!(removed, 0);
        assert_eq!(events.len(), 1);
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
