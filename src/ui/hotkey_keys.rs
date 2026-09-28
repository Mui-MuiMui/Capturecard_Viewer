//! ホットキー入力ダイアログが使う、egui のキー入力 → ホットキー文字列の変換。
//!
//! 受け付けるキーの対応表（`hotkey_key_name`）、割り当てさせない組み合わせ
//! （`is_clipboard_command_chord`）、文字列の組み立て（`build_hotkey_string`）。
//! どれも純粋関数で、確定の判定と描画は `hotkey_capture` にある。
//! `hotkey_capture.rs` が 800 行を超えたので分けた（#266）。

use eframe::egui;

/// egui のキーを、ホットキー文字列で使う名前に変換する。
/// ホットキーとして扱わないキーは `None` を返す。
///
/// 受け付けるキーは `hotkey::parse` が設定ファイルで受け付けるものと揃える
/// （F1〜F12、A〜Z、0〜9、Space、Enter、Escape、Tab、Backspace、Insert、Delete、
/// Home、End、PageUp、PageDown、矢印 4 つ）。Escape が抜けていたため、
/// 入力ダイアログで Escape を押しても何も確定せず、一覧の表示が変わらなかった（#251）。
/// 名前は egui の `Key::name()` と同じ（矢印は `Up` / `Down` / `Left` / `Right`）。
pub(super) fn hotkey_key_name(key: egui::Key) -> Option<&'static str> {
    let name = match key {
        egui::Key::A => "A",
        egui::Key::B => "B",
        egui::Key::C => "C",
        egui::Key::D => "D",
        egui::Key::E => "E",
        egui::Key::F => "F",
        egui::Key::G => "G",
        egui::Key::H => "H",
        egui::Key::I => "I",
        egui::Key::J => "J",
        egui::Key::K => "K",
        egui::Key::L => "L",
        egui::Key::M => "M",
        egui::Key::N => "N",
        egui::Key::O => "O",
        egui::Key::P => "P",
        egui::Key::Q => "Q",
        egui::Key::R => "R",
        egui::Key::S => "S",
        egui::Key::T => "T",
        egui::Key::U => "U",
        egui::Key::V => "V",
        egui::Key::W => "W",
        egui::Key::X => "X",
        egui::Key::Y => "Y",
        egui::Key::Z => "Z",
        egui::Key::F1 => "F1",
        egui::Key::F2 => "F2",
        egui::Key::F3 => "F3",
        egui::Key::F4 => "F4",
        egui::Key::F5 => "F5",
        egui::Key::F6 => "F6",
        egui::Key::F7 => "F7",
        egui::Key::F8 => "F8",
        egui::Key::F9 => "F9",
        egui::Key::F10 => "F10",
        egui::Key::F11 => "F11",
        egui::Key::F12 => "F12",
        egui::Key::Num0 => "0",
        egui::Key::Num1 => "1",
        egui::Key::Num2 => "2",
        egui::Key::Num3 => "3",
        egui::Key::Num4 => "4",
        egui::Key::Num5 => "5",
        egui::Key::Num6 => "6",
        egui::Key::Num7 => "7",
        egui::Key::Num8 => "8",
        egui::Key::Num9 => "9",
        egui::Key::Space => "Space",
        egui::Key::Enter => "Enter",
        egui::Key::Escape => "Escape",
        egui::Key::Tab => "Tab",
        egui::Key::Backspace => "Backspace",
        egui::Key::Insert => "Insert",
        egui::Key::Delete => "Delete",
        egui::Key::Home => "Home",
        egui::Key::End => "End",
        egui::Key::PageUp => "PageUp",
        egui::Key::PageDown => "PageDown",
        egui::Key::ArrowUp => "Up",
        egui::Key::ArrowDown => "Down",
        egui::Key::ArrowLeft => "Left",
        egui::Key::ArrowRight => "Right",
        _ => return None,
    };
    Some(name)
}

/// egui-winit がクリップボードの操作へ置き換える組み合わせか。
///
/// egui-winit 0.26 は Windows で Ctrl+Insert をコピー、Shift+Delete を切り取り、
/// Shift+Insert を貼り付けのイベント（`Event::Copy` / `Cut` / `Paste`）に
/// 置き換え、`Event::Key` を作らない（他の修飾キーが一緒に押されていても同じ）。
/// 前面でホットキーのキーを egui から取り除く判定（`hotkey::parse` の
/// `chord_from_egui_event`）はこれらを Ctrl+C / Ctrl+X / Ctrl+V として読むので、
/// Insert / Delete を使うこの組み合わせを割り当てると、取り除く対象を読み違える。
/// **入力ダイアログでは割り当てられないようにする**（#266）。
///
/// 普段はこの組み合わせの押下は `keys_down` に入らないので確定しないが、
/// Insert を押した直後の同じフレームで Shift を押すと、`keys_down` に Insert、
/// 修飾キーに Shift が載った状態で判定に来る。
pub(super) fn is_clipboard_command_chord(modifiers: &egui::Modifiers, key: egui::Key) -> bool {
    match key {
        egui::Key::Insert => modifiers.ctrl || modifiers.shift,
        egui::Key::Delete => modifiers.shift,
        _ => false,
    }
}

/// 押されている修飾キーと通常キーから、`screenshot::parse_hotkey` が解釈できる
/// ホットキー文字列を組み立てる。
///
/// 通常キーが 1 つも押されていない（修飾キーだけの）場合は `None` を返す。
pub(super) fn build_hotkey_string(
    modifiers: &egui::Modifiers,
    keys_down: &[egui::Key],
) -> Option<String> {
    // 通常キーが 1 つも無いうちは確定させない。修飾キーだけの文字列を確定させると
    // screenshot::parse_hotkey が "No key code specified" で弾き、登録に失敗する。
    // 押されているキーのうち対応している最初の 1 つだけを使う（ホットキーに含められる
    // 通常キーは 1 つだけのため）。
    let key_name = keys_down.iter().copied().find_map(hotkey_key_name)?;

    let mut parts = Vec::new();

    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    parts.push(key_name);

    Some(parts.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- egui のキー → ホットキー文字列の名前（#251） ----

    #[test]
    fn hotkey_key_name_accepts_escape() {
        assert_eq!(hotkey_key_name(egui::Key::Escape), Some("Escape"));
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::Escape]),
            Some("Escape".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[egui::Key::Escape]),
            Some("Ctrl+Escape".to_string())
        );
    }

    #[test]
    fn hotkey_key_name_is_never_empty_and_matches_egui_name() {
        // 一覧に出る名前が空にならないこと。名前は egui の `Key::name()` と同じで、
        // `hotkey::parse` はこの名前を設定ファイル上の名前として解釈する
        for &key in egui::Key::ALL {
            if let Some(name) = hotkey_key_name(key) {
                assert!(!name.is_empty(), "{key:?}");
                assert_eq!(name, key.name(), "{key:?}");
            }
        }
    }

    #[test]
    fn hotkey_key_name_covers_every_key_the_config_file_accepts() {
        let accepted = [
            egui::Key::Space,
            egui::Key::Enter,
            egui::Key::Escape,
            egui::Key::A,
            egui::Key::Z,
            egui::Key::Num0,
            egui::Key::Num9,
            egui::Key::F1,
            egui::Key::F12,
        ];
        for key in accepted.into_iter().chain(ADDED_KEYS.map(|(key, _)| key)) {
            assert!(hotkey_key_name(key).is_some(), "{key:?}");
        }
        // 設定ファイルでも受け付けないキーは入力ダイアログでも確定させない
        for key in [
            egui::Key::Plus,
            egui::Key::Minus,
            egui::Key::Comma,
            egui::Key::F13,
            egui::Key::F20,
        ] {
            assert_eq!(hotkey_key_name(key), None, "{key:?}");
        }
    }

    /// #266 で足したキーと、設定ファイル上の名前（egui の `Key::name()`）。
    /// **この名前は設定ファイルに書かれるので変えられない。**
    const ADDED_KEYS: [(egui::Key, &str); 12] = [
        (egui::Key::Tab, "Tab"),
        (egui::Key::Backspace, "Backspace"),
        (egui::Key::Insert, "Insert"),
        (egui::Key::Delete, "Delete"),
        (egui::Key::Home, "Home"),
        (egui::Key::End, "End"),
        (egui::Key::PageUp, "PageUp"),
        (egui::Key::PageDown, "PageDown"),
        (egui::Key::ArrowUp, "Up"),
        (egui::Key::ArrowDown, "Down"),
        (egui::Key::ArrowLeft, "Left"),
        (egui::Key::ArrowRight, "Right"),
    ];

    #[test]
    fn hotkey_key_name_added_keys_use_the_fixed_config_names() {
        // 一覧に出る名前・設定ファイルに書く名前が、決めた文字列のまま変わらないこと。
        // egui を上げて `Key::name()` が変わったときもここで気付ける
        for (key, name) in ADDED_KEYS {
            assert_eq!(hotkey_key_name(key), Some(name), "{key:?}");
            assert_eq!(key.name(), name, "{key:?}");
            assert_eq!(
                build_hotkey_string(&modifiers(true, false, true), &[key]),
                Some(format!("Ctrl+Alt+{name}")),
                "{key:?}"
            );
        }
    }

    fn modifiers(ctrl: bool, shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            // Windows では command は ctrl と同じ値にする決まりになっている
            command: ctrl,
        }
    }

    #[test]
    fn build_hotkey_string_no_input_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_only_returns_none() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_two_modifiers_only_returns_none() {
        // 修飾キーが 2 つ押されただけで確定してしまう不具合の再現
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, true), &[]),
            None
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, true, true), &[]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_only_returns_none() {
        assert_eq!(build_hotkey_string(&modifiers(true, true, true), &[]), None);
    }

    #[test]
    fn build_hotkey_string_unsupported_key_only_returns_none() {
        // 対応していないキーは通常キーとして数えない
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Plus]),
            None
        );
    }

    #[test]
    fn build_hotkey_string_single_key_returns_key_only() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::F5]),
            Some("F5".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::A]),
            Some("A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_one_modifier_with_key_returns_combination() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, false, false), &[egui::Key::S]),
            Some("Ctrl+S".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_three_modifiers_with_key_keeps_fixed_order() {
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, true), &[egui::Key::A]),
            Some("Ctrl+Shift+Alt+A".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_digit_keys_are_supported() {
        assert_eq!(
            build_hotkey_string(&modifiers(false, false, false), &[egui::Key::Num0]),
            Some("0".to_string())
        );
        assert_eq!(
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Num9]),
            Some("Ctrl+Shift+9".to_string())
        );
    }

    #[test]
    fn build_hotkey_string_ignores_unsupported_keys_when_key_is_present() {
        assert_eq!(
            build_hotkey_string(
                &modifiers(true, false, false),
                &[egui::Key::Plus, egui::Key::S]
            ),
            Some("Ctrl+S".to_string())
        );
    }
}
