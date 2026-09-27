//! 起動時に新しい版を知らせるダイアログ。
//!
//! **状態を持たず、何も実行しない。** 押されたボタンを `UpdateDialogEvent` で
//! 返し、リリースページを開くのも設定を書き換えるのも `app::update` が行う
//! （`docs/design/update.md`）。

use crate::i18n::{self, Text};
use crate::update::UpdateCheck;
use eframe::egui;

/// 通知ダイアログで押されたもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDialogEvent {
    /// 「更新する」。この段階ではリリースページを開いて閉じる
    OpenReleasePage,
    /// 「後で」とタイトルバーの ×。次の起動でまた知らせる
    Later,
    /// 「この版は通知しない」。この版を設定の `skipped_version` へ入れて閉じる
    SkipThisVersion,
}

/// リリースノートの要約を出す欄の高さの上限。長い本文でボタンが押し出されないよう、
/// ここを超える分はスクロールさせる。
const NOTES_MAX_HEIGHT: f32 = 160.0;

/// 新しい版を知らせるダイアログを描き、押されたものを返す。
///
/// 映像の上に出すだけで、映像も音声も止めない。
pub fn show_update_dialog(ctx: &egui::Context, check: &UpdateCheck) -> Vec<UpdateDialogEvent> {
    let mut events = Vec::new();
    // × を拾うためのローカル。閉じるのは呼び出し側（`UpdateDialogEvent::Later`）
    let mut window_open = true;

    // Id は固定にする。タイトルから作ると言語の切り替えで別のウィンドウになる
    // （docs/design/i18n.md）
    egui::Window::new(Text::UpdateDialogTitle.get())
        .id(egui::Id::new("update_dialog"))
        .open(&mut window_open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(i18n::update_available_heading(
                    &check.latest,
                    &check.current,
                ))
                .strong(),
            );

            if !check.notes_summary.is_empty() {
                ui.add_space(8.0);
                ui.label(Text::UpdateReleaseNotes.get());
                egui::ScrollArea::vertical()
                    .max_height(NOTES_MAX_HEIGHT)
                    .show(ui, |ui| {
                        ui.label(&check.notes_summary);
                    });
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(Text::UpdateNow.get())
                    .on_hover_text(Text::UpdateNowHint.get())
                    .clicked()
                {
                    events.push(UpdateDialogEvent::OpenReleasePage);
                }
                if ui.button(Text::UpdateLater.get()).clicked() {
                    events.push(UpdateDialogEvent::Later);
                }
                if ui.button(Text::UpdateSkipVersion.get()).clicked() {
                    events.push(UpdateDialogEvent::SkipThisVersion);
                }
            });
            ui.small(Text::UpdateNowHint.get());
        });

    if !window_open && events.is_empty() {
        events.push(UpdateDialogEvent::Later);
    }
    events
}
