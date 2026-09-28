//! ホットキー入力ダイアログ。
//!
//! 開いた瞬間からキー入力を受け付け、確定できる組み合わせなら
//! `HotkeyDialogEvent::Captured` を返す。登録そのものは行わない
//! （`docs/design/hotkeys.md`）。

use crate::hotkey::HotkeyAction;
use crate::i18n::{self, Text};
use eframe::egui;
use std::collections::BTreeMap;

use super::hotkey_keys::{
    build_hotkey_string, hotkey_key_name, is_clipboard_command_chord, is_clipboard_command_event,
};
use super::hotkeys_tab::normalize_hotkey;
use super::{notice_label, status_badge, NoticeKind, SETTINGS_WINDOW_SCREEN_MARGIN};

/// ホットキー入力ダイアログの入力状態。
///
/// 開いた瞬間からキー入力を受け付けるため、以前あった「キャプチャ開始」待ちの
/// 状態（`capturing` / `temp`）は持たない。持つのは編集対象と、直前の入力が
/// 拒否された理由だけ。
///
/// 以前は `static mut CAPTURING` / `static mut TEMP_HOTKEY` に持っていた。
/// `static_mut_refs` が Rust 2024 edition でエラーになるほか、
/// 参照のたびに `unsafe` が要るため構造体へ移した。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeyCaptureState {
    /// どのアクションのホットキーを編集しているか。
    ///
    /// **`reset` でも消さない。** ダイアログを開いたまま一覧の別の行の
    /// 「設定...」を押したときに参照されるほか、閉じたあとも直前に何を
    /// 編集していたかが分かるようにしてある。
    editing: Option<HotkeyAction>,
    /// 直前に確定を試みたキー入力が拒否された理由。
    ///
    /// 修飾キーのみ・他のアクションとの重複・（呼び出し側からの）登録失敗の
    /// いずれかで、確定しないまま次のフレームでも表示し続けるために持つ。
    /// 新しい判定が出るたびに置き換わり、待機状態に戻ったら消える。
    rejection: Option<String>,
}

impl HotkeyCaptureState {
    /// 編集対象のアクションを決めて、入力状態を初期化する。
    /// 一覧の「設定...」から呼ぶ。
    pub fn begin_for(&mut self, action: HotkeyAction) {
        self.editing = Some(action);
        self.clear_rejection();
    }

    /// 編集中のアクション。まだ一度も開いていなければ `None`。
    pub fn editing(&self) -> Option<HotkeyAction> {
        self.editing
    }

    /// 直前に拒否された理由。無ければ `None`。
    pub fn rejection(&self) -> Option<&str> {
        self.rejection.as_deref()
    }

    /// 拒否の理由を差し替える。
    pub fn set_rejection(&mut self, reason: String) {
        self.rejection = Some(reason);
    }

    /// 拒否の理由を消す。判定が「待機中」に戻ったときに使う。
    pub fn clear_rejection(&mut self) {
        self.rejection = None;
    }

    /// 拒否の理由を捨てる。キャンセル・× で閉じたときに使う。
    ///
    /// `editing` は残す。ダイアログを開き直しても直前の編集対象が分かるように
    /// するためで、実害も無い（次に一覧の「設定...」が押されたときに入れ替わる）。
    pub fn reset(&mut self) {
        self.clear_rejection();
    }
}

/// ホットキー入力ダイアログがこれより小さくなることはない。
///
/// 設定ダイアログの `SETTINGS_WINDOW_MIN_SIZE` と同じ考え方。中身が
/// 見出し・バッジ・説明文・キャンセルボタンだけと小さいので、下限も
/// それに合わせて小さくしてある
const HOTKEY_CAPTURE_DIALOG_MIN_SIZE: egui::Vec2 = egui::vec2(260.0, 140.0);

/// ホットキー入力ダイアログで、確定候補のキー入力をどう扱うかの判定。
///
/// 実機のキー入力を経由せずテストできるよう、`egui::Context` から取り出した
/// 値だけを引数に取る純粋関数（`judge_hotkey_capture`）の戻り値にしてある。
#[derive(Debug, Clone, PartialEq, Eq)]
enum HotkeyCaptureJudgement {
    /// まだ判定できる入力がない（通常キーが押されていない）
    Waiting,
    /// 修飾キーだけが押されている。通常キーが無いと登録できない
    ModifiersOnly,
    /// 受け付けられる
    Accepted(String),
    /// 他のアクションに割り当て済みのキーなので拒否する
    Duplicate { hotkey: String, other: HotkeyAction },
    /// egui-winit がクリップボードの操作へ置き換える組み合わせなので拒否する
    /// （`is_clipboard_command_chord`）
    ClipboardCommand,
}

/// 押されている修飾キー・通常キーから、確定候補のキー入力をどう扱うか判定する。
///
/// - 通常キーが押されていなければ `Waiting`。修飾キーだけが押されているなら
///   `ModifiersOnly`（`Waiting` と区別するのは、ダイアログ側で「修飾キーだけでは
///   登録できません」と理由を出し分けるため）
/// - 候補が組み立てられても、`action` 以外のアクションに同じキーが
///   割り当て済みなら `Duplicate`。**`action` 自身への再割当て（変更なし、
///   または同じキーの入力し直し）は許す**
/// - Ctrl+Insert / Shift+Insert / Shift+Delete（と、それに修飾キーを足したもの）は
///   重複の有無より先に `ClipboardCommand` で弾く（`is_clipboard_command_chord`）
/// - それ以外は `Accepted`。ただし押下を観測する仕組み（キーボードフック）が
///   使えているかはここでは分からない。呼び出し側が
///   `HotkeyManager::try_register` で確かめること
fn judge_hotkey_capture(
    modifiers: &egui::Modifiers,
    keys_down: &[egui::Key],
    action: HotkeyAction,
    existing: &BTreeMap<HotkeyAction, String>,
) -> HotkeyCaptureJudgement {
    // build_hotkey_string と同じく、対応している最初の 1 つを通常キーとして見る
    let key = keys_down
        .iter()
        .copied()
        .find(|key| hotkey_key_name(*key).is_some());
    if key.is_some_and(|key| is_clipboard_command_chord(modifiers, key)) {
        return HotkeyCaptureJudgement::ClipboardCommand;
    }
    match build_hotkey_string(modifiers, keys_down) {
        Some(candidate) => {
            let normalized = normalize_hotkey(&candidate);
            for (other_action, other_hotkey) in existing {
                if *other_action == action {
                    continue;
                }
                if normalize_hotkey(other_hotkey) == normalized {
                    return HotkeyCaptureJudgement::Duplicate {
                        hotkey: candidate,
                        other: *other_action,
                    };
                }
            }
            HotkeyCaptureJudgement::Accepted(candidate)
        }
        None if keys_down.is_empty() && modifiers.any() => HotkeyCaptureJudgement::ModifiersOnly,
        None => HotkeyCaptureJudgement::Waiting,
    }
}

/// このフレームの判定から出す拒否の理由。`previous` は覚えている直前の理由。
///
/// - 待機中と確定ではここでは理由を消さない（`None`）。呼び出し側（app/mod.rs）が
///   `HotkeyManager::try_register` の失敗理由をこのフレームより後で
///   `set_rejection` することがあり、ここで無条件に消すと次のフレームの
///   冒頭（この判定）で即座に消えて一度も表示されない。理由を消すのは
///   `begin_for`（編集対象の切り替え）と `reset`（キャンセル・× で閉じる）の役目
/// - **修飾キーだけの判定は、直前の理由がクリップボードの組み合わせなら上書き
///   しない**（#266）。クリップボードのイベントは押した瞬間の 1 フレームにしか
///   届かないので、Ctrl を押したままの次のフレームで「修飾キーだけ」に
///   置き換わり、理由がすぐ消えてしまう
fn rejection_for(judgement: &HotkeyCaptureJudgement, previous: Option<&str>) -> Option<String> {
    match judgement {
        HotkeyCaptureJudgement::ModifiersOnly
            if previous == Some(Text::HotkeyClipboardCommand.get()) =>
        {
            None
        }
        HotkeyCaptureJudgement::ModifiersOnly => Some(Text::HotkeyModifiersOnly.get().to_string()),
        HotkeyCaptureJudgement::Duplicate { other, .. } => {
            Some(i18n::hotkey_duplicate_assignment(other.label()))
        }
        HotkeyCaptureJudgement::ClipboardCommand => {
            Some(Text::HotkeyClipboardCommand.get().to_string())
        }
        HotkeyCaptureJudgement::Waiting | HotkeyCaptureJudgement::Accepted(_) => None,
    }
}

/// ホットキー入力ダイアログの 1 フレームで起きたこと。
///
/// 開いた瞬間から受付状態で、修飾キー以外のキーが押されて `judge_hotkey_capture`
/// が `Accepted` を返した時点で自動的に確定する（「キャプチャ開始」「OK」は無い）。
///
/// 受け取った側は**まず `Close` を反映してから `Captured` を処理すること。**
/// `Captured` は `HotkeyManager::try_register` で登録できるか確かめ、
/// 失敗したら開き直して理由を `HotkeyCaptureState::set_rejection` で伝える。
/// 順序が逆だと、開き直したはずのダイアログを `Close` が閉じてしまう。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyDialogEvent {
    /// このホットキー文字列で確定した
    Captured(String),
    /// 入力を受け付けられなかった理由。表示のために覚えておく
    Rejected(String),
    /// キャンセル・× で閉じた。入力中の状態を捨てる
    Cancelled,
    /// ダイアログを閉じる
    Close,
}

/// ホットキー入力ダイアログを描画し、このフレームで起きたことを返す。
///
/// `action` は編集対象のアクション、`existing` は現在の全アクションの割り当て
/// （重複判定に使う。`action` 自身の分が入っていても、判定側で除外する）、
/// `rejection` は前のフレームまでに覚えている拒否の理由。
///
/// 状態は何も書き換えない。開閉も拒否理由の記憶も `HotkeyDialogEvent` で返す
/// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
pub fn show_hotkey_capture_dialog(
    ctx: &egui::Context,
    action: HotkeyAction,
    existing: &BTreeMap<HotkeyAction, String>,
    rejection: Option<&str>,
) -> Vec<HotkeyDialogEvent> {
    let mut events: Vec<HotkeyDialogEvent> = Vec::new();
    let mut close_dialog = false;

    // タイトルバーの × を拾うためのローカル。設定ダイアログと同じ理由で、
    // 呼び出し側の開閉フラグはここからは触らない
    let mut window_open = true;

    // 判定はウィンドウを描く前に行う。**表示に使うのはこのフレームの判定結果。**
    // ui クロージャの中で ctx.input を呼んでから notice を描くと、表示が
    // 1 フレーム遅れて前回の判定のままになる
    let judgement = ctx.input(|i| {
        // HashSet の反復順は不定なので、同じ組み合わせから常に同じ
        // ホットキー文字列が得られるよう並べてから渡す
        let mut keys_down: Vec<egui::Key> = i.keys_down.iter().copied().collect();
        keys_down.sort();
        // Ctrl+C や Ctrl+Insert などは keys_down に入らず、クリップボードの
        // イベントだけが届く（`is_clipboard_command_event`）
        if i.events.iter().any(is_clipboard_command_event) {
            return HotkeyCaptureJudgement::ClipboardCommand;
        }
        judge_hotkey_capture(&i.modifiers, &keys_down, action, existing)
    });

    // このフレームの判定から出る拒否の理由。
    //
    // **表示にはこちらを優先して使う。** 覚えてもらうのは呼び出し側なので、
    // `Rejected` を返しただけでは `rejection` に入るのは次のフレーム。
    // キーを押したまま次の再描画が来ないと、理由が一度も出ないことがある
    let judged_rejection = rejection_for(&judgement, rejection);
    if let Some(reason) = &judged_rejection {
        events.push(HotkeyDialogEvent::Rejected(reason.clone()));
    }
    if let HotkeyCaptureJudgement::Accepted(candidate) = &judgement {
        events.push(HotkeyDialogEvent::Captured(candidate.clone()));
        close_dialog = true;
    }

    let shown_rejection = judged_rejection.as_deref().or(rejection);

    // 設定ダイアログと同じく、固定サイズだと極端に小さい画面で下端の
    // 「キャンセル」が画面外へ出て押せなくなる（`fixed_size` は外側の
    // 大きさを固定するだけで、`constrain_to` は位置しか動かさない）。
    // `default_size` + 画面由来の `max_size` に変え、中身はスクロールできる
    // ようにしておく
    let screen_rect = ctx.screen_rect();
    let max_size = (screen_rect.size() - egui::Vec2::splat(SETTINGS_WINDOW_SCREEN_MARGIN))
        .max(HOTKEY_CAPTURE_DIALOG_MIN_SIZE);

    // Id は固定にする。タイトルから作ると、言語を切り替えたときに
    // 位置や大きさが引き継がれない（docs/design/i18n.md）
    egui::Window::new(Text::HotkeySettings.get())
        .id(egui::Id::new("hotkey_capture_dialog"))
        .open(&mut window_open)
        .default_size([360.0, 180.0])
        .collapsible(false)
        .constrain_to(screen_rect)
        .min_size(HOTKEY_CAPTURE_DIALOG_MIN_SIZE)
        .max_size(max_size)
        .show(ctx, |ui| {
            // 「キャンセル」を先に確保する。理由は設定ダイアログの
            // ボタン列と同じ（コード上の順序に関わらず、呼んだ時点で
            // 親 Ui の下端から高さを確保するので、あとに続く ScrollArea が
            // このぶんを押し出して隠すことがない）
            egui::TopBottomPanel::bottom("hotkey_capture_dialog_buttons").show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.vertical_centered(|ui| {
                    if ui.button(Text::ButtonCancel.get()).clicked() {
                        events.push(HotkeyDialogEvent::Cancelled);
                        close_dialog = true;
                    }
                });
                ui.add_space(4.0);
            });

            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading(i18n::hotkey_capture_heading(action.label()));
                        ui.add_space(10.0);

                        // 受け付けている最中であることを示すバッジ。失敗ではないので
                        // 注意ではなく、進行中を表す種別で出す
                        status_badge(
                            ui,
                            &format!(
                                "{} {}",
                                NoticeKind::Success.symbol(),
                                Text::HotkeyCaptureWaiting.get()
                            ),
                            NoticeKind::Success,
                        );
                        ui.label(i18n::hotkey_capture_prompt(action.label()));

                        if let Some(reason) = shown_rejection {
                            ui.add_space(10.0);
                            notice_label(ui, NoticeKind::Error, reason.to_string());
                        }
                    });
                });
        });

    if close_dialog {
        events.push(HotkeyDialogEvent::Close);
    } else if !window_open {
        // × で閉じられた場合。`egui::Window::open` が渡した bool を
        // false にするだけでボタンは押されないため、設定ダイアログの ×
        // と同じくキャンセル扱いにして入力中の状態を捨てる
        events.push(HotkeyDialogEvent::Cancelled);
        events.push(HotkeyDialogEvent::Close);
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;

    use std::collections::BTreeMap;

    // ---- ホットキー入力ダイアログの編集対象 ----

    #[test]
    fn hotkey_capture_begin_for_records_the_action() {
        let mut capture = HotkeyCaptureState::default();

        capture.begin_for(HotkeyAction::VolumeUp);

        assert_eq!(capture.editing(), Some(HotkeyAction::VolumeUp));
        assert_eq!(capture.rejection(), None);
    }

    #[test]
    fn hotkey_capture_begin_for_discards_the_previous_rejection() {
        // 別のアクションを編集し始めたときに、前のアクションで拒否された
        // 理由が残っていると、新しいアクションの入力がまだ何も起きていないのに
        // エラーが表示されたままになる
        let mut capture = HotkeyCaptureState::default();
        capture.begin_for(HotkeyAction::Screenshot);
        capture.set_rejection("同じキーが「フルスクリーン切替」に割り当てられています".to_string());

        capture.begin_for(HotkeyAction::VolumeDown);

        assert_eq!(capture.rejection(), None);
        assert_eq!(capture.editing(), Some(HotkeyAction::VolumeDown));
    }

    #[test]
    fn hotkey_capture_reset_keeps_the_editing_action_but_clears_rejection() {
        // キャンセルや × で閉じたあとも、呼び出し側がどのアクションを
        // 編集していたか分かる必要がある。拒否の理由は残さない
        let mut capture = HotkeyCaptureState::default();
        capture.begin_for(HotkeyAction::ReconnectDevices);
        capture.set_rejection("修飾キーだけでは登録できません".to_string());

        capture.reset();

        assert_eq!(capture.editing(), Some(HotkeyAction::ReconnectDevices));
        assert_eq!(capture.rejection(), None);
    }

    #[test]
    fn judge_hotkey_capture_every_assignable_key_is_accepted_alone_and_with_alt() {
        // 対応表にあるキー（#266 で足した Tab / 矢印 / Insert なども含む）は、
        // 単独でも Alt との組み合わせでも確定する。Ctrl / Shift との組み合わせは
        // Insert / Delete で弾かれるものがあるので、ここでは Alt だけを足す
        // （弾く組み合わせは次のテスト）
        for &key in egui::Key::ALL {
            let Some(name) = hotkey_key_name(key) else {
                continue;
            };
            for (mods, expected) in [
                (no_modifiers(), name.to_string()),
                (modifiers(false, false, true), format!("Alt+{name}")),
            ] {
                let judged =
                    judge_hotkey_capture(&mods, &[key], HotkeyAction::Screenshot, &BTreeMap::new());
                assert_eq!(
                    judged,
                    HotkeyCaptureJudgement::Accepted(expected),
                    "{key:?}"
                );
            }
        }
    }

    #[test]
    fn rejection_for_modifiers_only_keeps_the_clipboard_reason() {
        // Ctrl+C を押した次のフレーム（Ctrl だけが押されたまま）で、
        // クリップボードの理由が「修飾キーだけ」に置き換わらない
        let clipboard = Text::HotkeyClipboardCommand.get();
        assert_eq!(
            rejection_for(&HotkeyCaptureJudgement::ClipboardCommand, None).as_deref(),
            Some(clipboard)
        );
        assert_eq!(
            rejection_for(&HotkeyCaptureJudgement::ModifiersOnly, Some(clipboard)),
            None
        );
    }

    #[test]
    fn rejection_for_modifiers_only_replaces_other_reasons() {
        let modifiers_only = Some(Text::HotkeyModifiersOnly.get());
        assert_eq!(
            rejection_for(&HotkeyCaptureJudgement::ModifiersOnly, None).as_deref(),
            modifiers_only
        );
        assert_eq!(
            rejection_for(
                &HotkeyCaptureJudgement::ModifiersOnly,
                Some("同じキーが「フルスクリーン切替」に割り当てられています")
            )
            .as_deref(),
            modifiers_only
        );
        assert_eq!(
            rejection_for(&HotkeyCaptureJudgement::Waiting, Some("理由")),
            None
        );
    }

    #[test]
    fn judge_hotkey_capture_clipboard_command_chords_are_rejected() {
        // egui-winit が Copy / Paste / Cut に置き換える組み合わせ。修飾キーを
        // 足しても置き換えは同じなので、足したものも弾く
        let rejected = [
            (modifiers(true, false, false), egui::Key::Insert),
            (modifiers(false, true, false), egui::Key::Insert),
            (modifiers(false, true, false), egui::Key::Delete),
            (modifiers(true, true, false), egui::Key::Insert),
            (modifiers(true, true, true), egui::Key::Delete),
        ];
        for (mods, key) in rejected {
            assert_eq!(
                judge_hotkey_capture(&mods, &[key], HotkeyAction::Screenshot, &BTreeMap::new()),
                HotkeyCaptureJudgement::ClipboardCommand,
                "{mods:?} {key:?}"
            );
        }
    }

    #[test]
    fn judge_hotkey_capture_other_insert_and_delete_chords_are_accepted() {
        // 置き換えられない組み合わせまで弾かない
        let accepted = [
            (no_modifiers(), egui::Key::Insert, "Insert"),
            (no_modifiers(), egui::Key::Delete, "Delete"),
            (
                modifiers(true, false, false),
                egui::Key::Delete,
                "Ctrl+Delete",
            ),
            (
                modifiers(false, false, true),
                egui::Key::Insert,
                "Alt+Insert",
            ),
            (
                modifiers(false, false, true),
                egui::Key::Delete,
                "Alt+Delete",
            ),
        ];
        for (mods, key, expected) in accepted {
            assert_eq!(
                judge_hotkey_capture(&mods, &[key], HotkeyAction::Screenshot, &BTreeMap::new()),
                HotkeyCaptureJudgement::Accepted(expected.to_string()),
                "{expected}"
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

    // ---- ホットキー入力ダイアログの確定判定 ----

    fn no_modifiers() -> egui::Modifiers {
        modifiers(false, false, false)
    }

    #[test]
    fn judge_hotkey_capture_no_keys_is_waiting() {
        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[],
                HotkeyAction::Screenshot,
                &BTreeMap::new()
            ),
            HotkeyCaptureJudgement::Waiting
        );
    }

    #[test]
    fn judge_hotkey_capture_modifiers_only_is_rejected_as_modifiers_only() {
        assert_eq!(
            judge_hotkey_capture(
                &modifiers(true, false, false),
                &[],
                HotkeyAction::Screenshot,
                &BTreeMap::new()
            ),
            HotkeyCaptureJudgement::ModifiersOnly
        );
        assert_eq!(
            judge_hotkey_capture(
                &modifiers(true, true, true),
                &[],
                HotkeyAction::Screenshot,
                &BTreeMap::new()
            ),
            HotkeyCaptureJudgement::ModifiersOnly
        );
    }

    #[test]
    fn judge_hotkey_capture_unsupported_key_only_is_waiting() {
        // Plus は hotkey_key_name の対象外。修飾キーの単独入力とは区別しなくてよい
        // （build_hotkey_string が None を返す点は同じで、実害も無い）
        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[egui::Key::Plus],
                HotkeyAction::Screenshot,
                &BTreeMap::new()
            ),
            HotkeyCaptureJudgement::Waiting
        );
    }

    #[test]
    fn judge_hotkey_capture_new_key_without_conflict_is_accepted() {
        let existing = BTreeMap::from([(HotkeyAction::VolumeUp, "F8".to_string())]);

        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[egui::Key::F5],
                HotkeyAction::Screenshot,
                &existing
            ),
            HotkeyCaptureJudgement::Accepted("F5".to_string())
        );
    }

    #[test]
    fn judge_hotkey_capture_key_assigned_to_another_action_is_rejected() {
        let existing = BTreeMap::from([(HotkeyAction::ToggleFullscreen, "F5".to_string())]);

        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[egui::Key::F5],
                HotkeyAction::Screenshot,
                &existing
            ),
            HotkeyCaptureJudgement::Duplicate {
                hotkey: "F5".to_string(),
                other: HotkeyAction::ToggleFullscreen,
            }
        );
    }

    #[test]
    fn judge_hotkey_capture_key_assigned_to_the_same_action_is_accepted() {
        // 同じアクションへの再割当て（変更なし、または同じキーの入力し直し）は
        // 重複として扱わない
        let existing = BTreeMap::from([(HotkeyAction::Screenshot, "F5".to_string())]);

        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[egui::Key::F5],
                HotkeyAction::Screenshot,
                &existing
            ),
            HotkeyCaptureJudgement::Accepted("F5".to_string())
        );
    }

    #[test]
    fn judge_hotkey_capture_duplicate_check_ignores_case_and_modifier_order() {
        // 表記のゆれがあっても同じキーとみなす（一覧の警告と同じ正規化）
        let existing =
            BTreeMap::from([(HotkeyAction::ToggleFullscreen, "shift+ctrl+a".to_string())]);

        assert_eq!(
            judge_hotkey_capture(
                &modifiers(true, true, false),
                &[egui::Key::A],
                HotkeyAction::Screenshot,
                &existing
            ),
            HotkeyCaptureJudgement::Duplicate {
                hotkey: "Ctrl+Shift+A".to_string(),
                other: HotkeyAction::ToggleFullscreen,
            }
        );
    }

    #[test]
    fn hotkey_capture_state_default_has_no_editing_action_or_rejection() {
        let capture = HotkeyCaptureState::default();
        assert_eq!(capture.editing(), None);
        assert_eq!(capture.rejection(), None);
    }

    #[test]
    fn hotkey_capture_state_set_rejection_replaces_the_previous_reason() {
        let mut capture = HotkeyCaptureState::default();
        capture.set_rejection("修飾キーだけでは登録できません".to_string());

        capture.set_rejection("同じキーが「スクリーンショット」に割り当てられています".to_string());

        assert_eq!(
            capture.rejection(),
            Some("同じキーが「スクリーンショット」に割り当てられています")
        );
    }

    #[test]
    fn hotkey_capture_state_clear_rejection_removes_the_reason() {
        let mut capture = HotkeyCaptureState::default();
        capture.set_rejection("修飾キーだけでは登録できません".to_string());

        capture.clear_rejection();

        assert_eq!(capture.rejection(), None);
    }
}
