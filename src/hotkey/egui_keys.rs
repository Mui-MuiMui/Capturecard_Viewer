//! egui のキー入力とホットキーの対応。egui のキー入力 → `KeyChord` の変換と、
//! egui へ渡す前のキー入力からホットキーのキーを取り除く判定（#217、#418）。
//!
//! ホットキー文字列の解析（`parse.rs`）から分けたもの（#443）。キー名は
//! `parse::parse_key_code` へ通して引くので、対応表はここに持たない。

use super::parse::parse_key_code;
use super::HotkeyAction;
use crate::keyboard_hook::{KeyChord, Modifiers};
use eframe::egui;
use std::collections::HashMap;

/// egui が受け取ったキー入力を、フックが観測するのと同じ `KeyChord` に直す。
/// ホットキーに使えないキー（記号キーや F13 以降など）は `None` を返す。
///
/// 自アプリが前面のとき、ホットキーのキーを egui から取り除くために使う（#217）。
/// **キー名は egui の `Key::name()` を `parse_key_code` へ通して引く。**
/// 対応表を別に持つと、設定ファイルで受け付けるキーと取り除くキーが食い違う。
///
/// egui の修飾キーには Windows キーが無いので、`SUPER` は付かない。
/// `Win+F5` を押したときも egui には `F5` として届くが、Windows キーとの
/// 組み合わせはたいていシェルが先に使うので、区別しない。
///
/// **数字キーは `None` にする（取り除かない）。** egui-winit はメイン列の `0` と
/// テンキーの `0` をどちらも `Key::Num0` にするので区別できない。フックの側では
/// テンキーは `VK_NUMPAD0` で、`0` の割り当てには反応しない。取り除くと、
/// ホットキーも発火せず egui にも届かない押下ができる。数字は egui では
/// テキスト入力にしか使わず、その間は取り除かないので、残しても衝突しない。
pub(super) fn chord_from_egui(key: egui::Key, modifiers: egui::Modifiers) -> Option<KeyChord> {
    if is_ambiguous_digit(key) {
        return None;
    }
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
    events.retain(|event| {
        chord_from_egui_event(event).is_none_or(|chord| !registered.contains_key(&chord))
    });
    before - events.len()
}

/// egui のイベント 1 つを、押されたキーの組み合わせとして読む。キーの押下でない
/// もの（解放、マウス、文字入力など）は `None` を返す。
///
/// **Ctrl+C / Ctrl+X / Ctrl+V は `Event::Key` ではなく `Event::Copy` / `Cut` /
/// `Paste` で届く。** egui-winit 0.26 が押下の時点でコマンドへ置き換え、
/// `Event::Key` を作らないため。これらを割り当てたときも取り除けるように、
/// それぞれ Ctrl+C / Ctrl+X / Ctrl+V として読む。egui-winit は Ctrl+Insert /
/// Shift+Delete / Shift+Insert も同じイベントにするので区別できない。
/// Insert と Delete は単独なら割り当てられるが（#266）、この組み合わせは
/// 入力ダイアログで割り当てられないようにしてある（`ui::hotkey_keys` の
/// `is_clipboard_command_chord`）。読み違えて困るのは「Ctrl+C を割り当てている
/// ときに Ctrl+Insert でのコピーも効かなくなる」と、設定ファイルへ直接
/// `Ctrl+Insert` などを書いたときに前面で egui のコピーが取り除かれないことだけ。
/// 入力中は取り除かないので、テキスト欄でのコピーと貼り付けには影響しない。
pub(super) fn chord_from_egui_event(event: &egui::Event) -> Option<KeyChord> {
    let (key, modifiers) = match event {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => (*key, *modifiers),
        egui::Event::Copy => (egui::Key::C, egui::Modifiers::CTRL),
        egui::Event::Cut => (egui::Key::X, egui::Modifiers::CTRL),
        egui::Event::Paste(_) => (egui::Key::V, egui::Modifiers::CTRL),
        _ => return None,
    };
    chord_from_egui(key, modifiers)
}

/// egui ではメイン列とテンキーを区別できない数字キーか（`chord_from_egui`）。
fn is_ambiguous_digit(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::Num0
            | egui::Key::Num1
            | egui::Key::Num2
            | egui::Key::Num3
            | egui::Key::Num4
            | egui::Key::Num5
            | egui::Key::Num6
            | egui::Key::Num7
            | egui::Key::Num8
            | egui::Key::Num9
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::parse::parse_hotkey;

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
    fn chord_from_egui_keys_have_non_empty_parsable_names() {
        // 一覧に出る名前が空になるキーが無いこと（#251）
        for &key in egui::Key::ALL {
            if chord_from_egui(key, egui::Modifiers::NONE).is_some() {
                assert!(!key.name().is_empty(), "{key:?}");
                assert!(parse_key_code(key.name()).is_ok(), "{key:?}");
            }
        }
        assert!(chord_from_egui(egui::Key::Escape, egui::Modifiers::NONE).is_some());
    }

    #[test]
    fn chord_from_egui_digit_returns_none() {
        // egui ではメイン列の 0 とテンキーの 0 が同じ Key::Num0 になり、区別できない。
        // フックはテンキーの 0 では `0` の割り当てに反応しないので、取り除くと
        // ホットキーも発火せず egui にも届かない押下ができてしまう
        for key in [egui::Key::Num0, egui::Key::Num5, egui::Key::Num9] {
            assert_eq!(chord_from_egui(key, egui::Modifiers::NONE), None, "{key:?}");
            assert_eq!(chord_from_egui(key, egui::Modifiers::CTRL), None, "{key:?}");
        }
    }

    #[test]
    fn chord_from_egui_unsupported_key_returns_none() {
        // ホットキーに割り当てられないキーは、取り除く対象にもならない
        for key in [
            egui::Key::Plus,
            egui::Key::Minus,
            egui::Key::F13,
            egui::Key::F20,
        ] {
            assert_eq!(chord_from_egui(key, egui::Modifiers::NONE), None, "{key:?}");
        }
    }

    // ---- #266 で足したキー ----

    /// #266 で足したキーと、その仮想キーコード。名前は egui の `Key::name()`
    const ADDED_KEYS: [(egui::Key, u32); 12] = [
        (egui::Key::Tab, 0x09),
        (egui::Key::Backspace, 0x08),
        (egui::Key::Insert, 0x2D),
        (egui::Key::Delete, 0x2E),
        (egui::Key::Home, 0x24),
        (egui::Key::End, 0x23),
        (egui::Key::PageUp, 0x21),
        (egui::Key::PageDown, 0x22),
        (egui::Key::ArrowUp, 0x26),
        (egui::Key::ArrowDown, 0x28),
        (egui::Key::ArrowLeft, 0x25),
        (egui::Key::ArrowRight, 0x27),
    ];

    #[test]
    fn parse_key_code_added_keys_map_egui_names_to_virtual_keys() {
        // 設定ファイル上の名前（egui の名前）を解析した結果が、フックが観測する
        // 仮想キーコードと一致すること。食い違うと割り当てても反応しない
        for (key, vk) in ADDED_KEYS {
            assert!(!key.name().is_empty(), "{key:?}");
            assert_eq!(parse_key_code(key.name()), Ok(vk), "{key:?}");
            assert_eq!(
                parse_key_code(&key.name().to_ascii_uppercase()),
                Ok(vk),
                "{key:?}"
            );
        }
    }

    #[test]
    fn parse_key_code_arrow_keys_use_egui_names() {
        // 矢印は egui の名前（Up / Down / Left / Right）に揃える。別名は受け付けない
        assert_eq!(parse_key_code("Up"), Ok(0x26));
        assert!(parse_key_code("ArrowUp").is_err());
        assert!(parse_key_code("PgUp").is_err());
    }

    #[test]
    fn chord_from_egui_added_keys_match_the_parsed_hotkey() {
        // 前面で押したときに egui から取り除けるよう、フックと同じ組になること
        for (key, vk) in ADDED_KEYS {
            assert_eq!(
                chord_from_egui(key, egui::Modifiers::NONE),
                Some(KeyChord {
                    modifiers: Modifiers::empty(),
                    vk
                }),
                "{key:?}"
            );
            let hotkey = format!("Ctrl+Shift+{}", key.name());
            assert_eq!(
                chord_from_egui(key, egui::Modifiers::CTRL | egui::Modifiers::SHIFT),
                Some(parse_hotkey(&hotkey).expect("解析できること")),
                "{hotkey}"
            );
        }
    }

    #[test]
    fn parse_key_code_numpad_names_are_not_accepted() {
        // テンキーは egui で区別できないので受け付けない（入力ダイアログと揃える）
        for name in ["Numpad0", "Numpad9", "NumpadAdd", "Add"] {
            assert!(parse_key_code(name).is_err(), "{name}");
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
    fn remove_hotkey_key_events_removes_clipboard_commands_of_assigned_keys() {
        // Ctrl+C / Ctrl+X / Ctrl+V は Event::Key ではなくコマンドのイベントで届く
        let registered = HashMap::from([
            (
                parse_hotkey("Ctrl+C").expect("解析できること"),
                HotkeyAction::Screenshot,
            ),
            (
                parse_hotkey("Ctrl+V").expect("解析できること"),
                HotkeyAction::VolumeUp,
            ),
        ]);
        let mut events = vec![
            egui::Event::Copy,
            egui::Event::Cut,
            egui::Event::Paste("text".to_string()),
        ];

        let removed = remove_hotkey_key_events(&registered, &mut events, false);

        assert_eq!(removed, 2);
        assert_eq!(events, vec![egui::Event::Cut]);
    }

    #[test]
    fn chord_from_egui_event_reads_only_presses_and_clipboard_commands() {
        assert_eq!(
            chord_from_egui_event(&key_event(
                egui::Key::F5,
                egui::Modifiers::NONE,
                true,
                false
            )),
            Some(parse_hotkey("F5").expect("解析できること"))
        );
        assert_eq!(
            chord_from_egui_event(&egui::Event::Cut),
            Some(parse_hotkey("Ctrl+X").expect("解析できること"))
        );
        assert_eq!(
            chord_from_egui_event(&key_event(
                egui::Key::F5,
                egui::Modifiers::NONE,
                false,
                false
            )),
            None
        );
        assert_eq!(
            chord_from_egui_event(&egui::Event::Text("a".to_string())),
            None
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

    /// ボタン 2 つの画面で 1 つ目にフォーカスを置き、`key` の押下を 1 回送ったあとに
    /// フォーカスのある Id と、1 つ目のボタンの Id を返す。
    ///
    /// `registered` が `Some` なら、`App::raw_input_hook` と同じく egui へ渡す前の
    /// `RawInput` から取り除いてから送る（#418）
    fn focus_after_key_press(
        key: egui::Key,
        registered: Option<&HashMap<KeyChord, HotkeyAction>>,
    ) -> (Option<egui::Id>, egui::Id) {
        let ctx = egui::Context::default();
        let mut first = None;
        let mut run = |events: Vec<egui::Event>, focus_first: bool| {
            let mut raw_input = egui::RawInput {
                events,
                ..Default::default()
            };
            if let Some(registered) = registered {
                remove_hotkey_key_events(registered, &mut raw_input.events, false);
            }
            ctx.run_ui(raw_input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let a = ui.button("a");
                    let _ = ui.button("b");
                    if focus_first {
                        a.request_focus();
                    }
                    first = Some(a.id);
                });
            })
            .drop_without_applying_deltas();
        };
        run(Vec::new(), true);
        run(Vec::new(), false);
        run(
            vec![key_event(key, egui::Modifiers::NONE, true, false)],
            false,
        );
        run(Vec::new(), false);
        (
            ctx.memory(|memory| memory.focused()),
            first.expect("描画されていること"),
        )
    }

    #[test]
    fn removing_from_raw_input_keeps_egui_focus_on_assigned_tab_and_escape() {
        // update() の中で input_mut から取り除いても、egui はフレームの始まりで
        // Tab ならフォーカスを次へ動かし、Escape なら外す。egui へ渡す前の RawInput から
        // 取り除けば、どちらも動かない（#418）
        let registered = HashMap::from([
            (
                parse_hotkey("Tab").expect("解析できること"),
                HotkeyAction::ToggleMute,
            ),
            (
                parse_hotkey("Escape").expect("解析できること"),
                HotkeyAction::Screenshot,
            ),
        ]);

        for key in [egui::Key::Tab, egui::Key::Escape] {
            let (focused, first) = focus_after_key_press(key, Some(&registered));
            assert_eq!(focused, Some(first), "{key:?}");
        }
    }

    #[test]
    fn unassigned_tab_and_escape_still_move_egui_focus() {
        // 上のテストの対照。割り当てていなければ（または取り除かなければ）
        // egui のフォーカスは動く。動かない環境だと上のテストが何も確かめていないことになる
        let (focused, first) = focus_after_key_press(egui::Key::Tab, Some(&registered_chords()));
        assert!(focused.is_some_and(|id| id != first));

        let (focused, _) = focus_after_key_press(egui::Key::Escape, None);
        assert_eq!(focused, None);
    }
}
