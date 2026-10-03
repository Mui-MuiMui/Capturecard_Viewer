//! 映像が出ていないときのプレースホルダー。文言の決め方と、映像エリアの
//! 中央への配置。描画の本体は `super::view`。

use super::view::VIDEO_AREA_SENSE;
use crate::i18n::Text;
use eframe::egui;

/// 映像が出ていないときに画面へ出す文言を決める。
///
/// 「デバイスは開けているが信号が来ていない」と「デバイスそのものが消えた」は
/// ユーザーの取るべき行動が違う（入力機器の電源を見るのか、ケーブルを挿し直すのか）
/// ため、同じ文言にしない。
pub(super) fn video_placeholder_message(capturing: bool, reconnecting: bool) -> &'static str {
    let text = match (capturing, reconnecting) {
        (true, _) => Text::PlaceholderNoSignal,
        (false, true) => Text::PlaceholderReconnecting,
        (false, false) => Text::PlaceholderDisconnected,
    };
    text.get()
}

/// 映像が出ていないときに画面へ出す文言を、理由の 1 行を添えて組み立てる。
///
/// `detail` は直近の失敗（`ErrorCenter` に記録されたもの）。**ストリームを
/// 開けている場合は添えない。** 映像信号が来ていないのはデバイスの手前の
/// 問題で、そこに古い接続エラーを出すと原因を取り違えさせる。
///
/// 理由の切り詰めは呼び出し側（`error_detail`）が済ませてある。ここで
/// 長さを見ないのは、切り詰めの基準を 1 か所に集めておくため。
pub(super) fn video_placeholder_text(
    capturing: bool,
    reconnecting: bool,
    detail: Option<&str>,
) -> String {
    let head = video_placeholder_message(capturing, reconnecting);
    match detail {
        Some(detail) if !capturing => format!("{}\n{}", head, detail),
        _ => head.to_string(),
    }
}

/// 映像が無いときの領域を確保し、その中央にプレースホルダーの文言を描く。
/// 返すのは領域全体の応答で、ドラッグや右クリックはこちらで受ける。
///
/// **文言は確保した矩形の中へ `put` で置く（#274）。** 以前は領域全体を
/// `allocate_response` で確保したあとに `centered_and_justified` で文言を
/// 足していた。確保で残りの場所が 0 になるため、文言は映像エリアの下端の外に
/// 置かれて切り取られ、一度も画面に出ていなかった。
///
/// 文言は選択できないようにする。既定（`selectable_labels`）のままだと
/// 文字列の選択がドラッグと右クリックを取り、ウィンドウを動かせなくなる。
pub(super) fn show_video_placeholder(
    ui: &mut egui::Ui,
    available_size: egui::Vec2,
    placeholder: &str,
) -> egui::Response {
    let response = ui.allocate_response(available_size, VIDEO_AREA_SENSE);
    ui.put(
        response.rect,
        egui::Label::new(placeholder).selectable(false),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_placeholder_message_capturing_says_no_signal() {
        // デバイスは開けている。ユーザーが見るべきは入力機器側
        assert_eq!(
            video_placeholder_message(true, false),
            "映像信号がありません"
        );
        // 開けている間は再接続の有無で文言を変えない
        assert_eq!(
            video_placeholder_message(true, true),
            "映像信号がありません"
        );
    }

    #[test]
    fn video_placeholder_message_not_capturing_says_device_is_gone() {
        assert_eq!(
            video_placeholder_message(false, false),
            "デバイスが接続されていません"
        );
        assert_eq!(
            video_placeholder_message(false, true),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn video_placeholder_text_without_detail_is_the_message_alone() {
        assert_eq!(
            video_placeholder_text(false, true, None),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn video_placeholder_text_adds_the_reason_on_a_second_line() {
        assert_eq!(
            video_placeholder_text(false, true, Some("映像デバイスに接続できません: not found")),
            "デバイスが接続されていません（再接続を試しています）\n映像デバイスに接続できません: not found"
        );
    }

    #[test]
    fn video_placeholder_text_while_capturing_drops_the_reason() {
        // ストリームは開けている＝接続の失敗ではない。古い接続エラーを
        // 出すと、入力機器ではなく USB を疑わせてしまう
        assert_eq!(
            video_placeholder_text(true, false, Some("映像デバイスに接続できません: not found")),
            "映像信号がありません"
        );
    }

    /// 描画せずに 1 フレーム回し、プレースホルダーの文字の矩形と切り取り範囲を返す。
    fn placeholder_text_rects() -> Vec<(egui::Rect, egui::Rect)> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let available_size = ui.available_size();
                show_video_placeholder(ui, available_size, "placeholder");
            });
        });
        let rects = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.rect.translate(text.pos.to_vec2()),
                    clipped.clip_rect,
                )),
                _ => None,
            })
            .collect();
        // 描かないのでテクスチャの差分も捨てる。そのまま落とすと debug_assert で止まる
        output.drop_without_applying_deltas();
        rects
    }

    #[test]
    fn show_video_placeholder_draws_text_inside_visible_area() {
        // #274: 領域を確保したあとに文言を足していたため、文字が下端の外
        // （y = 600 付近）に置かれて切り取られていた
        let rects = placeholder_text_rects();
        assert_eq!(rects.len(), 1);
        let (text, clip) = rects[0];
        assert!(clip.contains_rect(text), "text {text:?} clip {clip:?}");
        // 上下とも中央に置く
        assert!((text.center().y - 300.0).abs() < 20.0, "text {text:?}");
        assert!((text.center().x - 400.0).abs() < 20.0, "text {text:?}");
    }
}
