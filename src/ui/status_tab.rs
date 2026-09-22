//! 「接続状態」タブ。
//!
//! **ここでは何も編集しない。** 映像と音声が実際に何へ繋がっているかと、
//! 直近の失敗を読むためのタブ（`docs/design/error-reporting.md`）。

use crate::status::{ConnectionStatus, LinkStatus};
use eframe::egui;

use super::{status_badge, warning_label, NoticeKind};

/// 接続状態を、記号を付けた見出しと表示の種別へ変換する。
///
/// **色だけで区別しない。** 記号（`●` / `⚠` / `×`）と `LinkStatus::headline` の
/// 文言だけで、接続中・再接続中・未接続を見分けられるようにしてある。
fn link_status_badge(status: &LinkStatus) -> (String, NoticeKind) {
    let kind = match (status.connected, status.reconnecting) {
        (true, _) => NoticeKind::Success,
        // 追いかけている最中で、復帰する見込みがある
        (false, true) => NoticeKind::Warning,
        // 映像も音声も出ていない。注意より強く出す
        (false, false) => NoticeKind::Error,
    };
    (format!("{} {}", kind.symbol(), status.headline()), kind)
}

/// 接続状態タブを描画する。
///
/// **ここでは何も編集しない。** 映像と音声が実際に何へ繋がっているかと、
/// 直近の失敗を読むためのタブで、値は呼び出し側が複製して渡す
/// （描画中にデバイスへ問い合わせないため）。
pub(super) fn show_status_tab(ui: &mut egui::Ui, connection: &ConnectionStatus) {
    ui.heading("接続状態");
    ui.add_space(10.0);

    show_link_status(ui, "映像", &connection.video);
    ui.add_space(15.0);
    show_link_status(ui, "音声", &connection.audio);

    ui.add_space(15.0);
    ui.label("この内容は表示だけで、「適用」や「OK」では変わりません。");
    ui.label("詳しい経過はログファイルに残っています（%AppData%\\capturecard_viewer\\logs）。");
}

/// 映像か音声、片方の接続状態を 1 つの枠に描く。
fn show_link_status(ui: &mut egui::Ui, title: &str, status: &LinkStatus) {
    ui.group(|ui| {
        ui.strong(title);
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("状態:");
            let (text, kind) = link_status_badge(status);
            status_badge(ui, &text, kind);
        });

        for line in &status.details {
            ui.label(line);
        }

        // 繋がっている間は再試行していないので、回数を出しても 0 が並ぶだけ
        if !status.connected && status.attempts > 0 {
            ui.label(format!("連続失敗: {} 回", status.attempts));
        }

        match &status.error {
            Some((message, time)) => {
                // 長いエラー文でダイアログの幅が広がらないよう折り返す。
                // 繋がったあとも記録として残り続けるため、今まさに失敗している
                // わけではない。失敗ではなく注意として出す
                warning_label(ui, message);
                ui.label(format!("発生時刻: {}", time));
            }
            None => {
                ui.label("直近のエラー: なし");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::status::LinkStatus;

    // ---- 接続状態のバッジ ----

    fn link_status(connected: bool, reconnecting: bool) -> LinkStatus {
        LinkStatus {
            connected,
            reconnecting,
            ..LinkStatus::default()
        }
    }

    #[test]
    fn link_status_badge_connected_is_success_with_a_filled_circle() {
        let (text, kind) = link_status_badge(&link_status(true, false));

        assert_eq!(kind, NoticeKind::Success);
        assert_eq!(text, "● 接続中");
    }

    #[test]
    fn link_status_badge_reconnecting_is_a_warning() {
        // 追いかけている最中は復帰する見込みがあるので、失敗まで強めない
        let (text, kind) = link_status_badge(&link_status(false, true));

        assert_eq!(kind, NoticeKind::Warning);
        assert_eq!(text, "⚠ 未接続（再接続を試しています）");
    }

    #[test]
    fn link_status_badge_disconnected_is_an_error() {
        let (text, kind) = link_status_badge(&link_status(false, false));

        assert_eq!(kind, NoticeKind::Error);
        assert_eq!(text, "× 未接続");
    }

    #[test]
    fn link_status_badge_connected_ignores_the_reconnecting_flag() {
        // 繋がったあとにフラグが落ちるまでの間があるため、接続中を優先する
        let (text, kind) = link_status_badge(&link_status(true, true));

        assert_eq!(kind, NoticeKind::Success);
        assert_eq!(text, "● 接続中");
    }
}
