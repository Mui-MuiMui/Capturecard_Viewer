//! 「その他」タブ。
//!
//! プリセット、画面の言語、更新の確認、設定の書き出し・読み込み・初期化を置いてある。
//! **ここでは何も実行しない。** ファイルダイアログもファイル I/O も
//! `CaptureCardViewer` が行う（`docs/design/settings-dialog.md`）。

use crate::i18n::{self, Text};
use crate::settings::{AppSettings, LanguageSetting, UpdateSettings};
use crate::update::{UpdateStatus, UpdateView};
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
    update: &UpdateView<'_>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading(Text::TabOther.get());
    ui.add_space(10.0);

    show_preset_group(ui, draft, view.new_preset_name, events);

    ui.add_space(15.0);

    show_language_group(ui, draft, events);

    ui.add_space(15.0);

    show_update_group(ui, &draft.update, update, events);

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

/// 「その他」タブの言語の節を描く。
///
/// 選んだ言語はドラフトへ入るだけで、画面が切り替わるのは「適用」「OK」の
/// とき。**他の設定と同じ扱いにしてある。** 選んだ瞬間に切り替えると、
/// 「キャンセル」で閉じたときに戻すべき言語が分からなくなる。
fn show_language_group(ui: &mut egui::Ui, draft: &AppSettings, events: &mut Vec<SettingsEvent>) {
    ui.group(|ui| {
        ui.strong(Text::LanguageGroup.get());
        ui.add_space(5.0);

        let current = draft.ui.language;
        // Id は表示文字列から作らない。言語を切り替えると変わってしまうため
        egui::ComboBox::from_id_salt("language_combo")
            .selected_text(current.label())
            .show_ui(ui, |ui| {
                for language in LanguageSetting::ALL {
                    if ui
                        .selectable_label(current == language, language.label())
                        .clicked()
                        && current != language
                    {
                        events.push(SettingsEvent::SetLanguage(language));
                    }
                }
            });

        ui.add_space(5.0);
        ui.small(Text::LanguageHint.get());
    });
}

/// 「その他」タブの更新の節を描く。
///
/// 現在の版と確認の結果（`update`）は実行中のアプリの状態で、ドラフトではない。
/// 現在の版は、テスト用の環境変数で差し替えていればその版を出す
/// （通知ダイアログの「いまは vA.B.C」と揃える）。
/// 「更新を確認」は `SettingsEvent::CheckForUpdates` を返し、問い合わせは
/// `app::update` が別スレッドで行う。「更新する」は `SettingsEvent::StartUpdate` を
/// 返し、ダウンロードと差し替えも `app::update` が別スレッドで行う。
/// 2 つのチェックと「解除」はドラフトの `update` を差し替えるイベントを返すだけで、
/// 反映は「適用」「OK」のとき。
///
/// 「リリースページを開く」は egui のリンク。開くのは eframe（ブラウザの起動）で、
/// アプリの状態は動かさない。
fn show_update_group(
    ui: &mut egui::Ui,
    draft: &UpdateSettings,
    update: &UpdateView<'_>,
    events: &mut Vec<SettingsEvent>,
) {
    let update_status = update.status;
    ui.group(|ui| {
        ui.strong(Text::UpdateGroup.get());
        ui.add_space(5.0);

        ui.label(i18n::update_current_version(update.current));

        ui.horizontal(|ui| {
            // 問い合わせ中は押せなくする。同時に 2 本走らせない
            let checking = matches!(update_status, UpdateStatus::Checking);
            if ui
                .add_enabled(!checking, egui::Button::new(Text::UpdateCheckNow.get()))
                .clicked()
            {
                events.push(SettingsEvent::CheckForUpdates);
            }
            show_update_status(ui, update_status);
        });

        // 新しい版が見つかっていれば、ここからも更新を始められる。
        // 起動時のダイアログを「後で」で閉じたときや、通知を切っているときの入口
        if matches!(update_status, UpdateStatus::Available(_))
            && ui
                .add_enabled(!update.applying, egui::Button::new(Text::UpdateNow.get()))
                .on_hover_text(Text::UpdateNowHint.get())
                .clicked()
        {
            events.push(SettingsEvent::StartUpdate);
        }

        ui.hyperlink_to(
            Text::UpdateOpenReleasePage.get(),
            update_status.release_url(),
        );

        ui.add_space(5.0);

        let mut check_on_startup = draft.check_on_startup;
        if ui
            .checkbox(&mut check_on_startup, Text::UpdateCheckOnStartup.get())
            .changed()
        {
            events.push(SettingsEvent::SetUpdateSettings(UpdateSettings {
                check_on_startup,
                ..draft.clone()
            }));
        }
        let mut notify_on_startup = draft.notify_on_startup;
        if ui
            .checkbox(&mut notify_on_startup, Text::UpdateNotifyOnStartup.get())
            .changed()
        {
            events.push(SettingsEvent::SetUpdateSettings(UpdateSettings {
                notify_on_startup,
                ..draft.clone()
            }));
        }

        if let Some(skipped) = &draft.skipped_version {
            ui.horizontal(|ui| {
                ui.label(i18n::update_skipped_version(skipped));
                if ui.button(Text::UpdateClearSkipped.get()).clicked() {
                    events.push(SettingsEvent::SetUpdateSettings(UpdateSettings {
                        skipped_version: None,
                        ..draft.clone()
                    }));
                }
            });
        }

        ui.add_space(5.0);
        ui.small(Text::UpdateHint.get());
        ui.small(Text::UpdateDraftHint.get());
    });
}

/// 確認の結果を 1 行で出す。
fn show_update_status(ui: &mut egui::Ui, update_status: &UpdateStatus) {
    match update_status {
        UpdateStatus::NotChecked => {
            ui.label(Text::UpdateNotChecked.get());
        }
        UpdateStatus::Checking => {
            ui.spinner();
            ui.label(Text::UpdateChecking.get());
        }
        UpdateStatus::UpToDate => {
            notice_label(ui, NoticeKind::Success, Text::UpdateUpToDate.get());
        }
        UpdateStatus::Available(check) => {
            notice_label(
                ui,
                NoticeKind::Warning,
                i18n::update_status_available(&check.latest),
            );
        }
        UpdateStatus::Failed(reason) => {
            notice_label(ui, NoticeKind::Error, i18n::update_status_failed(reason));
        }
    }
}
