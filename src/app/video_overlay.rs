//! 映像の上に常設で重ねる表示（統計オーバーレイ・フェイクデバイスの帯・録画中の印）の置き場所（#284）。
//!
//! 何を描くかはそれぞれの呼び出し元（`view.rs` / `recording.rs`）が持ち、
//! ここは「どの層へ描くか」だけを持つ。

use eframe::egui;

/// `add_contents` が描くものを `area` の中の `align` の位置（左上・上端中央・右上など）へ寄せて、
/// 映像の上・設定ダイアログの下へ描く。返すのは実際に描いた範囲。
///
/// **`egui::Area` にしない。** Area は自分の層を持ち、`Order::Foreground` では
/// 設定ダイアログ（`egui::Window` は `Order::Middle`）の上に重なって文字が読めなくなる。
/// `Order::Middle` にしても、egui は「前のフレームで見えていなかった Area」を同じ層の
/// 一番上へ移すので、ダイアログを開いたまま情報表示をオンにしたり録画を始めたりすると
/// やはりダイアログの上に来る。そこで映像を描いた `CentralPanel` と同じ背景の層
/// （`LayerId::background()`）へ、映像のあとから描く。背景の層はどの Window よりも
/// 下に描かれ、同じ層の中では後に描いたものが上になるので、映像の上・ダイアログの下に収まる。
/// **呼ぶのは映像の `CentralPanel` を描いたあと。** 先に描くと映像の下に隠れる。
///
/// 映像のドラッグ（ウィンドウの移動）や右クリックを吸わないよう、文字を選択できなくする
/// （選択できる文字はドラッグを受ける）。枠や文字が確保する矩形は hover だけを受ける。
///
/// トースト（`crate::overlay` の `TransientOverlay`）はここへ載せず `Order::Foreground` のままにする。
/// 設定ダイアログの中の操作（プリセットの切り替えなど）の結果も知らせるので、
/// ダイアログの下に隠れると役に立たない。出るのも数秒だけ。
///
/// `add_contents` は 2 回呼ぶ。1 回目は描かずに大きさを測り、2 回目で寄せた位置へ描く。
/// Area は前のフレームの大きさを覚えて寄せるが、ここでは状態を持たないため。
/// 中身は数行の文字なので、2 回並べても負担にならない。
pub(super) fn show_video_overlay(
    ctx: &egui::Context,
    id: egui::Id,
    area: egui::Rect,
    align: egui::Align2,
    add_contents: impl Fn(&mut egui::Ui),
) -> egui::Rect {
    let mut measure = overlay_ui(ctx, id.with("measure"), area);
    measure.set_invisible();
    add_contents(&mut measure);
    let size = measure.min_rect().size();

    let placed = align.align_size_within_rect(size, area);
    // 幅は測ったときに折り返した幅に合わせる（少し余らせて、同じ位置で折り返させる）
    let max_rect = egui::Rect::from_min_max(
        placed.min,
        egui::pos2(placed.max.x + 1.0, area.max.y.max(placed.max.y)),
    );
    let mut ui = overlay_ui(ctx, id, max_rect);
    add_contents(&mut ui);
    ui.min_rect()
}

/// 背景の層に描く `Ui` を作る。左上から上から下へ並べる。
fn overlay_ui(ctx: &egui::Context, id: egui::Id, max_rect: egui::Rect) -> egui::Ui {
    let mut ui = egui::Ui::new(
        ctx.clone(),
        id,
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(max_rect),
    );
    // egui 0.36 の `Ui::new` は切り抜きを `max_rect` にするので、0.26 と同じく画面全体に戻す。
    // 枠の影などが寄せた範囲の外へ少しはみ出しても切れないように
    ui.set_clip_rect(ctx.content_rect());
    ui.style_mut().interaction.selectable_labels = false;
    ui
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY_TEXT: &str = "overlay";
    const DIALOG_TEXT: &str = "dialog";

    fn screen_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        }
    }

    /// 映像の代わりに塗りつぶした `CentralPanel`、重ねる表示、設定ダイアログ役の
    /// `egui::Window` を、実際の `update()` と同じ順で描く。
    fn draw_frame(ctx: &egui::Context, overlay: bool, dialog: bool) -> egui::FullOutput {
        let mut output = ctx.run_ui(screen_input(), |ui| {
            let ctx = &ui.ctx().clone();
            egui::CentralPanel::default().show(ui, |ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, egui::Color32::BLUE);
            });
            if overlay {
                show_video_overlay(
                    ctx,
                    egui::Id::new("test_overlay"),
                    ctx.content_rect(),
                    egui::Align2::LEFT_TOP,
                    |ui| {
                        ui.label(OVERLAY_TEXT);
                    },
                );
            }
            if dialog {
                egui::Window::new("dialog")
                    .id(egui::Id::new("test_dialog"))
                    .fixed_pos(egui::pos2(0.0, 0.0))
                    .show(ctx, |ui| ui.label(DIALOG_TEXT));
            }
        });
        // 描かないのでテクスチャの差分は捨てる。残したまま落とすと debug_assert で止まる
        output.textures_delta.clear();
        output
    }

    /// 描かれた順（奥から手前）で、`text` を含む文字の図形が何番目かを返す。
    fn text_index(output: &egui::FullOutput, text: &str) -> Option<usize> {
        output
            .shapes
            .iter()
            .position(|clipped| match &clipped.shape {
                egui::Shape::Text(shape) => shape.galley.job.text == text,
                _ => false,
            })
    }

    fn background_fill_index(output: &egui::FullOutput) -> Option<usize> {
        output
            .shapes
            .iter()
            .position(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) => rect.fill == egui::Color32::BLUE,
                _ => false,
            })
    }

    fn assert_between_video_and_dialog(output: &egui::FullOutput) {
        let video = background_fill_index(output).expect("映像の代わりの塗りが無い");
        let overlay = text_index(output, OVERLAY_TEXT).expect("重ねる表示が無い");
        let dialog = text_index(output, DIALOG_TEXT).expect("ダイアログの文字が無い");
        assert!(video < overlay, "映像 {video} の下に描かれた {overlay}");
        assert!(
            overlay < dialog,
            "ダイアログ {dialog} の上に描かれた {overlay}"
        );
    }

    #[test]
    fn overlay_is_drawn_above_video_and_below_dialog() {
        let ctx = egui::Context::default();
        // egui の Window は大きさを測るため、最初のフレームでは描かれない
        draw_frame(&ctx, true, true);
        let output = draw_frame(&ctx, true, true);
        assert_between_video_and_dialog(&output);
    }

    #[test]
    fn overlay_that_appears_while_dialog_is_open_stays_below_dialog() {
        // #284: ダイアログを開いたあとで情報表示をオンにする・録画を始めると、
        // Area だと「前のフレームで見えていなかった」として一番上へ移っていた
        let ctx = egui::Context::default();
        draw_frame(&ctx, false, true);
        draw_frame(&ctx, false, true);
        let output = draw_frame(&ctx, true, true);
        assert_between_video_and_dialog(&output);
        let output = draw_frame(&ctx, true, true);
        assert_between_video_and_dialog(&output);
    }

    /// 1 フレームだけ回し、`align` で寄せた短い文字の範囲を返す。
    fn placed_rect(area: egui::Rect, align: egui::Align2) -> egui::Rect {
        let ctx = egui::Context::default();
        let mut placed = egui::Rect::NOTHING;
        ctx.run_ui(screen_input(), |ui| {
            placed =
                show_video_overlay(ui.ctx(), egui::Id::new("test_overlay"), area, align, |ui| {
                    ui.label(OVERLAY_TEXT);
                });
        })
        .drop_without_applying_deltas();
        placed
    }

    #[test]
    fn overlay_is_placed_by_align_and_sized_to_contents() {
        // 帯（上端中央）と録画中の印（右上）が、横幅いっぱいに広がらず中身の大きさで寄る
        let area = egui::Rect::from_min_max(egui::pos2(8.0, 20.0), egui::pos2(792.0, 592.0));

        let left = placed_rect(area, egui::Align2::LEFT_TOP);
        assert_eq!(left.min, area.min);
        assert!(left.width() < 200.0, "{left:?}");

        let center = placed_rect(area, egui::Align2::CENTER_TOP);
        assert!(
            (center.center().x - area.center().x).abs() < 1.0,
            "{center:?}"
        );
        assert_eq!(center.top(), area.top());
        assert!((center.width() - left.width()).abs() < 1.0, "{center:?}");

        let right = placed_rect(area, egui::Align2::RIGHT_TOP);
        assert!((right.right() - area.right()).abs() < 1.0, "{right:?}");
        assert_eq!(right.top(), area.top());
    }
}
