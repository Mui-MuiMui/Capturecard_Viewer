//! 「その他」タブ。
//!
//! プリセットと、設定の書き出し・読み込み・初期化を置いてある。
//! **ここでは何も実行しない。** ファイルダイアログもファイル I/O も
//! `CaptureCardViewer` が行う（`docs/design/settings-dialog.md`）。

use crate::i18n::{self, Text};
use crate::settings::AppSettings;
use eframe::egui;

use super::preset::{active_preset_label, PresetRowAction};
use super::state::SettingsDialogView;
use super::{notice_label, warning_label, NoticeKind, SettingsEvent};

/// 「その他」タブを描画する。
///
/// 設定の書き出し・読み込み・初期化を置いてある。**ここでは何も実行しない。**
/// ファイルダイアログもファイル I/O も `CaptureCardViewer` が行う
/// （`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
///
/// `draft` を読み取りで受けるのは、このタブが設定を書き換えないため。
/// プリセットの追加・上書き・削除も、名前入力欄も、起きたことを
/// `SettingsEvent` で返して `app` が反映する。
///
/// タブに分けてあるのは、下部の「OK / キャンセル / 適用」の並びへ足すと
/// 「初期化」が「OK」の隣に来るため。押し間違いで設定が消える並びにしない。
/// 読み込みと初期化が「適用」を押すまで反映されないことの説明も、
/// ボタンの真下に書けるほうが伝わる。
pub(super) fn show_other_tab(
    ui: &mut egui::Ui,
    draft: &AppSettings,
    view: &SettingsDialogView<'_>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading(Text::TabOther.get());
    ui.add_space(10.0);

    show_preset_group(ui, draft, view.new_preset_name, events);

    ui.add_space(15.0);

    ui.group(|ui| {
        ui.strong(Text::SettingsFile.get());
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            if ui.button(Text::ExportSettings.get()).clicked() {
                events.push(SettingsEvent::ExportSettings);
            }
            if ui.button(Text::ImportSettings.get()).clicked() {
                events.push(SettingsEvent::ImportSettings);
            }
        });

        ui.add_space(5.0);
        ui.small(Text::ExportHint.get());
        ui.small(Text::ImportHint.get());
        ui.small(Text::ImportWindowHint.get());
    });

    ui.add_space(15.0);

    ui.group(|ui| {
        ui.strong(Text::ResetGroup.get());
        ui.add_space(5.0);

        if view.reset_confirm {
            warning_label(ui, Text::ResetConfirm.get());
            ui.horizontal(|ui| {
                if ui.button(Text::ResetConfirmYes.get()).clicked() {
                    events.push(SettingsEvent::ResetDraft);
                    events.push(SettingsEvent::SetResetConfirm(false));
                }
                if ui.button(Text::ResetConfirmNo.get()).clicked() {
                    events.push(SettingsEvent::SetResetConfirm(false));
                }
            });
        } else if ui.button(Text::ResetButton.get()).clicked() {
            events.push(SettingsEvent::SetResetConfirm(true));
        }

        ui.add_space(5.0);
        ui.small(Text::ResetHint.get());
        ui.small(Text::ResetScopeHint.get());
    });

    if let Some(message) = view.management_message {
        ui.add_space(15.0);
        ui.separator();
        let kind = if message.is_error {
            NoticeKind::Error
        } else {
            NoticeKind::Success
        };
        notice_label(ui, kind, &message.text);
    }
}

/// 「その他」タブのプリセット節を描く。
///
/// **ここでの操作はすべてドラフトに対して行う。** 一覧の編集（新規保存・
/// 上書き・削除）も、選択中のプリセットの切替も、実行中の設定へ移るのは
/// 「適用」か「OK」のとき。読み込み・初期化と同じ扱いにしてあるので、
/// 「キャンセル」で丸ごと取り消せる。
///
/// 節をタブの先頭に置いてあるのは、書き出し・読み込み・初期化よりも
/// 使う頻度が高いため。
fn show_preset_group(
    ui: &mut egui::Ui,
    draft: &AppSettings,
    new_preset_name: &str,
    events: &mut Vec<SettingsEvent>,
) {
    ui.group(|ui| {
        ui.strong(Text::Preset.get());
        ui.add_space(5.0);

        ui.label(i18n::preset_current(active_preset_label(draft)));
        ui.add_space(5.0);

        let mut row_action: Option<PresetRowAction> = None;

        if draft.presets.is_empty() {
            ui.small(Text::PresetEmpty.get());
        } else {
            egui::Grid::new("preset_list")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for (index, preset) in draft.presets.iter().enumerate() {
                        ui.label(&preset.name);
                        ui.horizontal(|ui| {
                            if ui
                                .button(Text::PresetLoad.get())
                                .on_hover_text(Text::PresetLoadHint.get())
                                .clicked()
                            {
                                row_action = Some(PresetRowAction::Load(index));
                            }
                            if ui
                                .button(Text::PresetOverwrite.get())
                                .on_hover_text(Text::PresetOverwriteHint.get())
                                .clicked()
                            {
                                row_action = Some(PresetRowAction::Overwrite(index));
                            }
                            if ui.button(Text::PresetDelete.get()).clicked() {
                                row_action = Some(PresetRowAction::Delete(index));
                            }
                        });
                        ui.end_row();
                    }
                });
        }

        if let Some(row_action) = row_action {
            events.push(SettingsEvent::PresetRow(row_action));
        }

        ui.add_space(10.0);
        ui.label(Text::PresetSaveNewLabel.get());
        ui.horizontal(|ui| {
            // `TextEdit` は `&mut String` を要求するので、呼び出し側が持つ
            // 入力欄を複製して渡し、変わったらイベントで返す。**このフレームの
            // 表示にはこの複製を使う**ので、打った文字はその場で出る
            let mut name = new_preset_name.to_string();
            // Enter でも保存できるようにする。名前を打った直後に
            // マウスへ持ち替えさせない
            let response = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .desired_width(200.0)
                    .hint_text(Text::PresetNameHint.get()),
            );
            if response.changed() {
                events.push(SettingsEvent::SetNewPresetName(name));
            }
            let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            // **入力欄の内容は載せない。** 直前の `SetNewPresetName` を
            // 先に処理した呼び出し側が、自分の持つ値を使う
            if ui.button(Text::PresetSave.get()).clicked() || entered {
                events.push(SettingsEvent::SaveNewPreset);
            }
        });

        ui.add_space(5.0);
        ui.small(Text::PresetScopeHint.get());
        ui.small(Text::PresetAutoReconnectHint.get());
        ui.small(Text::PresetDraftHint.get());
        ui.small(Text::PresetMenuHint.get());
    });
}
