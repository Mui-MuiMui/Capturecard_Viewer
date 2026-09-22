//! 「その他」タブ。
//!
//! プリセットと、設定の書き出し・読み込み・初期化を置いてある。
//! **ここでは何も実行しない。** ファイルダイアログもファイル I/O も
//! `CaptureCardViewer` が行う（`docs/design/settings-dialog.md`）。

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
    ui.heading("その他");
    ui.add_space(10.0);

    show_preset_group(ui, draft, view.new_preset_name, events);

    ui.add_space(15.0);

    ui.group(|ui| {
        ui.strong("設定ファイル");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            if ui.button("設定を書き出す...").clicked() {
                events.push(SettingsEvent::ExportSettings);
            }
            if ui.button("設定を読み込む...").clicked() {
                events.push(SettingsEvent::ImportSettings);
            }
        });

        ui.add_space(5.0);
        ui.small("書き出すのは実行中の設定です。編集中の内容を含めたい場合は、先に「適用」を押してください。");
        ui.small("読み込んだ内容は編集中の設定に入ります。「適用」か「OK」を押すまで反映されません。");
        ui.small("ウィンドウの位置とサイズは読み込みません。別の画面構成で書き出したファイルを読んでも、ウィンドウは動きません。");
    });

    ui.add_space(15.0);

    ui.group(|ui| {
        ui.strong("初期化");
        ui.add_space(5.0);

        if view.reset_confirm {
            warning_label(ui, "編集中の設定を初期値に戻します。よろしいですか？");
            ui.horizontal(|ui| {
                if ui.button("初期化する").clicked() {
                    events.push(SettingsEvent::ResetDraft);
                    events.push(SettingsEvent::SetResetConfirm(false));
                }
                if ui.button("やめる").clicked() {
                    events.push(SettingsEvent::SetResetConfirm(false));
                }
            });
        } else if ui.button("設定を初期化...").clicked() {
            events.push(SettingsEvent::SetResetConfirm(true));
        }

        ui.add_space(5.0);
        ui.small(
            "初期化も編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。",
        );
        ui.small("戻る範囲は読み込みと同じです。ウィンドウの位置とサイズ、右クリックメニューで切り替える項目は初期化しません。");
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
        ui.strong("プリセット");
        ui.add_space(5.0);

        ui.label(format!("現在: {}", active_preset_label(draft)));
        ui.add_space(5.0);

        let mut row_action: Option<PresetRowAction> = None;

        if draft.presets.is_empty() {
            ui.small("プリセットはまだありません。下の入力欄から作れます。");
        } else {
            egui::Grid::new("preset_list")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for (index, preset) in draft.presets.iter().enumerate() {
                        ui.label(&preset.name);
                        ui.horizontal(|ui| {
                            if ui
                                .button("読み込む")
                                .on_hover_text(
                                    "このプリセットのビデオ・オーディオ設定を編集中の設定へ入れます",
                                )
                                .clicked()
                            {
                                row_action = Some(PresetRowAction::Load(index));
                            }
                            if ui
                                .button("上書き保存")
                                .on_hover_text(
                                    "編集中のビデオ・オーディオ設定でこのプリセットを置き換えます",
                                )
                                .clicked()
                            {
                                row_action = Some(PresetRowAction::Overwrite(index));
                            }
                            if ui.button("削除").clicked() {
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
        ui.label("現在の設定を新しいプリセットとして保存:");
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
                    .hint_text("例: 低遅延優先"),
            );
            if response.changed() {
                events.push(SettingsEvent::SetNewPresetName(name));
            }
            let entered =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            // **入力欄の内容は載せない。** 直前の `SetNewPresetName` を
            // 先に処理した呼び出し側が、自分の持つ値を使う
            if ui.button("保存").clicked() || entered {
                events.push(SettingsEvent::SaveNewPreset);
            }
        });

        ui.add_space(5.0);
        ui.small("プリセットに入るのは「デバイス設定」タブのビデオとオーディオだけです。スクリーンショット・ホットキー・ウィンドウの設定は含みません。");
        ui.small("デバイスの自動再接続もプリセットには含みません。右クリックメニューで切り替えた状態がそのまま残ります。");
        ui.small("追加・上書き・削除・読み込みは編集中の設定に対して行います。「適用」か「OK」を押すまで反映されません。");
        ui.small("切り替えは右クリックメニューの「プリセット」からも行えます。");
    });
}
