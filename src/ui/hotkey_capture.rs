//! ホットキー入力ダイアログ。
//!
//! 開いた瞬間からキー入力を受け付け、確定できる組み合わせなら
//! `HotkeyDialogEvent::Captured` を返す。登録そのものは行わない
//! （`docs/design/hotkeys.md`）。

use crate::hotkey::HotkeyAction;
use crate::i18n::{self, Text};
use eframe::egui;
use std::collections::BTreeMap;

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

/// egui のキーを、ホットキー文字列で使う名前に変換する。
/// ホットキーとして扱わないキーは `None` を返す。
fn hotkey_key_name(key: egui::Key) -> Option<&'static str> {
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
        _ => return None,
    };
    Some(name)
}

/// 押されている修飾キーと通常キーから、`screenshot::parse_hotkey` が解釈できる
/// ホットキー文字列を組み立てる。
///
/// 通常キーが 1 つも押されていない（修飾キーだけの）場合は `None` を返す。
fn build_hotkey_string(modifiers: &egui::Modifiers, keys_down: &[egui::Key]) -> Option<String> {
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
}

/// 押されている修飾キー・通常キーから、確定候補のキー入力をどう扱うか判定する。
///
/// - 通常キーが押されていなければ `Waiting`。修飾キーだけが押されているなら
///   `ModifiersOnly`（`Waiting` と区別するのは、ダイアログ側で「修飾キーだけでは
///   登録できません」と理由を出し分けるため）
/// - 候補が組み立てられても、`action` 以外のアクションに同じキーが
///   割り当て済みなら `Duplicate`。**`action` 自身への再割当て（変更なし、
///   または同じキーの入力し直し）は許す**
/// - それ以外は `Accepted`。ただし押下を観測する仕組み（キーボードフック）が
///   使えているかはここでは分からない。呼び出し側が
///   `HotkeyManager::try_register` で確かめること
fn judge_hotkey_capture(
    modifiers: &egui::Modifiers,
    keys_down: &[egui::Key],
    action: HotkeyAction,
    existing: &BTreeMap<HotkeyAction, String>,
) -> HotkeyCaptureJudgement {
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
        judge_hotkey_capture(&i.modifiers, &keys_down, action, existing)
    });

    // このフレームの判定から出る拒否の理由。
    //
    // **表示にはこちらを優先して使う。** 覚えてもらうのは呼び出し側なので、
    // `Rejected` を返しただけでは `rejection` に入るのは次のフレーム。
    // キーを押したまま次の再描画が来ないと、理由が一度も出ないことがある
    let judged_rejection = match &judgement {
        HotkeyCaptureJudgement::ModifiersOnly => Some(Text::HotkeyModifiersOnly.get().to_string()),
        HotkeyCaptureJudgement::Duplicate { other, .. } => {
            Some(i18n::hotkey_duplicate_assignment(other.label()))
        }
        // 待機中でもここでは理由を消さない。呼び出し側（app/mod.rs）が
        // `HotkeyManager::try_register` の失敗理由をこのフレームより後で
        // `set_rejection` することがあり、ここで無条件に消すと次のフレームの
        // 冒頭（この判定）で即座に消えて一度も表示されない。
        // 理由を消すのは `begin_for`（編集対象の切り替え）と `reset`
        // （キャンセル・× で閉じる）の役目
        HotkeyCaptureJudgement::Waiting | HotkeyCaptureJudgement::Accepted(_) => None,
    };
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

    egui::Window::new(Text::HotkeySettings.get())
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
            build_hotkey_string(&modifiers(true, true, false), &[egui::Key::Tab]),
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
                &[egui::Key::Tab, egui::Key::S]
            ),
            Some("Ctrl+S".to_string())
        );
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
        // Tab は hotkey_key_name の対象外。修飾キーの単独入力とは区別しなくてよい
        // （build_hotkey_string が None を返す点は同じで、実害も無い）
        assert_eq!(
            judge_hotkey_capture(
                &no_modifiers(),
                &[egui::Key::Tab],
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
