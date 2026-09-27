//! 新しい版を知らせ、更新の進み具合を出すダイアログ。
//!
//! **状態を持たず、何も実行しない。** 出す内容は `UpdateDialogView` で受け取り、
//! 押されたボタンを `UpdateDialogEvent` で返す。更新を始めるのも、リリースページを
//! 開くのも、設定を書き換えるのも `app::update` が行う
//! （`docs/design/update.md`）。

use crate::i18n::{self, Text};
use crate::update::apply::ApplyProgress;
use crate::update::UpdateCheck;
use eframe::egui;

/// ダイアログに出すもの。
#[derive(Debug, Clone, Copy)]
pub enum UpdateDialogView<'a> {
    /// 新しい版がある。「更新する」「後で」「この版は通知しない」
    Available(&'a UpdateCheck),
    /// 更新している。「キャンセル」だけ
    Applying {
        check: &'a UpdateCheck,
        progress: ApplyProgress,
    },
    /// 差し替えが済み、終了して新しい版を起動するところ。ボタンは無い
    Restarting,
    /// 更新できなかった。「リリースページを開く」「閉じる」
    Failed {
        check: &'a UpdateCheck,
        reason: &'a str,
    },
}

/// ダイアログで押されたもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDialogEvent {
    /// 「更新する」。ダウンロードと差し替えを始める
    StartUpdate,
    /// 「リリースノートを見る」と、失敗したときの「リリースページを開く」。
    /// どちらもその版のリリースページをブラウザで開く（手で更新するときの逃げ道でもある）。
    /// 「リリースノートを見る」ではダイアログを閉じない
    OpenReleasePage,
    /// 「後で」とタイトルバーの ×。次の起動でまた知らせる
    Later,
    /// 「この版は通知しない」。この版を設定の `skipped_version` へ入れて閉じる
    SkipThisVersion,
    /// 更新中の「キャンセル」と ×
    CancelUpdate,
    /// 失敗を出したあとの「閉じる」と ×
    Close,
}

/// ダイアログの幅。中身の長さで横に伸び縮みさせない。英語の下の段のボタン 3 つが
/// 1 行に並ぶくらいにしてあり、収まらなければ折り返す。
const DIALOG_WIDTH: f32 = 440.0;

/// 進捗の棒の幅。
const PROGRESS_WIDTH: f32 = 320.0;

/// 1 MiB。大きさが分からないときの量の表示に使う。
const MIB: f64 = 1024.0 * 1024.0;

/// ダイアログを描き、押されたものを返す。
///
/// 映像の上に出すだけで、映像も音声も止めない。
pub fn show_update_dialog(
    ctx: &egui::Context,
    view: UpdateDialogView<'_>,
) -> Vec<UpdateDialogEvent> {
    let mut events = Vec::new();
    // × を拾うためのローカル。閉じるのは呼び出し側
    let mut window_open = true;

    let title = match view {
        UpdateDialogView::Available(_) => Text::UpdateDialogTitle,
        UpdateDialogView::Applying { .. } | UpdateDialogView::Restarting => {
            Text::UpdateApplyingTitle
        }
        UpdateDialogView::Failed { .. } => Text::UpdateFailedTitle,
    };
    // 置き換えを始めたら × を出さない。もう止められない
    let closable = !matches!(
        view,
        UpdateDialogView::Restarting
            | UpdateDialogView::Applying {
                progress: ApplyProgress::Installing,
                ..
            }
    );

    // Id は固定にする。タイトルから作ると言語の切り替えや段階の移り変わりで
    // 別のウィンドウになる（docs/design/i18n.md）
    let mut window = egui::Window::new(title.get())
        .id(egui::Id::new("update_dialog"))
        .collapsible(false)
        .resizable(false)
        .min_width(DIALOG_WIDTH)
        .max_width(DIALOG_WIDTH)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO);
    if closable {
        window = window.open(&mut window_open);
    }
    window.show(ctx, |ui| match view {
        UpdateDialogView::Available(check) => show_available(ui, check, &mut events),
        UpdateDialogView::Applying { check, progress } => {
            show_applying(ui, check, progress, &mut events)
        }
        UpdateDialogView::Restarting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(Text::UpdateRestarting.get());
            });
        }
        UpdateDialogView::Failed { check, reason } => show_failed(ui, check, reason, &mut events),
    });

    if !window_open && events.is_empty() {
        events.push(match view {
            UpdateDialogView::Available(_) => UpdateDialogEvent::Later,
            UpdateDialogView::Applying { .. } => UpdateDialogEvent::CancelUpdate,
            UpdateDialogView::Restarting | UpdateDialogView::Failed { .. } => {
                UpdateDialogEvent::Close
            }
        });
    }
    events
}

fn heading(ui: &mut egui::Ui, check: &UpdateCheck) {
    ui.label(
        egui::RichText::new(i18n::update_available_heading(
            &check.latest,
            &check.current,
        ))
        .strong(),
    );
}

/// 新しい版を知らせる。見出しの 1 行とボタンだけにする。
///
/// リリースノートは本文を載せず、「リリースノートを見る」でリリースページを開く。
/// 本文は長さがまちまちで、ダイアログの中では読みにくいため。
///
/// ボタンは 2 段にする。上の段は「リリースノートを見る」だけで、ダイアログを閉じない
/// （読んでから「更新する」を押せるように）。下の段にそれ以外の 3 つを並べる。
fn show_available(ui: &mut egui::Ui, check: &UpdateCheck, events: &mut Vec<UpdateDialogEvent>) {
    heading(ui, check);

    ui.add_space(8.0);
    if ui.button(Text::UpdateViewReleaseNotes.get()).clicked() {
        events.push(UpdateDialogEvent::OpenReleasePage);
    }

    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(Text::UpdateNow.get())
            .on_hover_text(Text::UpdateNowHint.get())
            .clicked()
        {
            events.push(UpdateDialogEvent::StartUpdate);
        }
        if ui.button(Text::UpdateLater.get()).clicked() {
            events.push(UpdateDialogEvent::Later);
        }
        if ui.button(Text::UpdateSkipVersion.get()).clicked() {
            events.push(UpdateDialogEvent::SkipThisVersion);
        }
    });
}

fn show_applying(
    ui: &mut egui::Ui,
    check: &UpdateCheck,
    progress: ApplyProgress,
    events: &mut Vec<UpdateDialogEvent>,
) {
    heading(ui, check);
    ui.add_space(8.0);

    match progress {
        ApplyProgress::Preparing => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(Text::UpdatePreparing.get());
            });
        }
        ApplyProgress::Downloading { downloaded, .. } => {
            let text = match progress.percent() {
                Some(percent) => i18n::update_downloading_percent(percent),
                None => i18n::update_downloading_amount(downloaded as f64 / MIB),
            };
            match progress.percent() {
                Some(percent) => {
                    ui.add(
                        egui::ProgressBar::new(f32::from(percent) / 100.0)
                            .desired_width(PROGRESS_WIDTH)
                            .text(text),
                    );
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(text);
                    });
                }
            }
        }
        ApplyProgress::Installing => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(Text::UpdateInstalling.get());
            });
        }
    }

    ui.add_space(8.0);
    // 置き換えを始めたらもう止めない（止めると元の exe を戻す手間が増えるだけ）
    let cancellable = !matches!(progress, ApplyProgress::Installing);
    if ui
        .add_enabled(cancellable, egui::Button::new(Text::ButtonCancel.get()))
        .clicked()
    {
        events.push(UpdateDialogEvent::CancelUpdate);
    }
}

fn show_failed(
    ui: &mut egui::Ui,
    check: &UpdateCheck,
    reason: &str,
    events: &mut Vec<UpdateDialogEvent>,
) {
    heading(ui, check);
    ui.add_space(8.0);
    super::notice_label(ui, super::NoticeKind::Error, reason);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button(Text::UpdateOpenReleasePage.get()).clicked() {
            events.push(UpdateDialogEvent::OpenReleasePage);
        }
        if ui.button(Text::UpdateClose.get()).clicked() {
            events.push(UpdateDialogEvent::Close);
        }
    });
}
