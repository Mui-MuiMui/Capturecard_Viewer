//! 「接続状態」タブ。
//!
//! **ここでは何も編集しない。** 映像と音声が実際に何へ繋がっているかと、
//! 直近の失敗を読むためのタブ（`docs/design/error-reporting.md`）。

use crate::i18n::{self, Text};
use crate::status::{self, ConnectionStatus, LinkStatus};
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
    ui.heading(Text::TabStatus.get());
    ui.add_space(10.0);

    // 実機に繋がらない原因がここで分かるよう、先頭に出す（#252）
    if let Some(notice) = status::fake_devices_notice(connection.fake_devices) {
        warning_label(ui, notice);
        ui.add_space(10.0);
    }

    show_link_status(ui, Text::LinkVideo.get(), &connection.video);
    ui.add_space(15.0);
    show_link_status(ui, Text::LinkAudio.get(), &connection.audio);

    // 効果音は繋ぎっぱなしのデバイスではないので、失敗が残っているときだけ枠を出す（#356）
    if let Some(error) = &connection.screenshot_sound_error {
        show_recorded_errors(ui, Text::LinkScreenshotSound.get(), &[error]);
    }
    // 設定ファイルも同じ。起動時に読めなかった知らせ（#505）はトーストが
    // 接続の失敗に上書きされうるので、ここで読み返せるようにする
    let settings: Vec<_> = [&connection.settings_load_notice, &connection.settings_error]
        .into_iter()
        .flatten()
        .collect();
    if !settings.is_empty() {
        show_recorded_errors(ui, Text::SettingsFile.get(), &settings);
    }

    ui.add_space(15.0);
    ui.label(Text::StatusReadOnlyHint.get());
    ui.label(Text::StatusLogHint.get());
}

/// 接続中・未接続の見出しを持たないもの（効果音・設定ファイル）の直近の失敗を
/// 1 つの枠に描く。`errors` の各要素は `(整形済みの文言, 発生時刻)` で、上から並べる。
fn show_recorded_errors(ui: &mut egui::Ui, title: &str, errors: &[&(String, String)]) {
    ui.add_space(15.0);
    ui.group(|ui| {
        ui.strong(title);
        for (message, time) in errors {
            ui.add_space(5.0);
            warning_label(ui, message);
            ui.label(i18n::error_time(time));
        }
    });
}

/// 映像か音声、片方の接続状態を 1 つの枠に描く。
fn show_link_status(ui: &mut egui::Ui, title: &str, status: &LinkStatus) {
    ui.group(|ui| {
        ui.strong(title);
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label(Text::StateLabel.get());
            let (text, kind) = link_status_badge(status);
            status_badge(ui, &text, kind);
        });

        for line in &status.details {
            ui.label(line);
        }

        // 繋がっている間は再試行していないので、回数を出しても 0 が並ぶだけ
        if !status.connected && status.attempts > 0 {
            ui.label(i18n::consecutive_failures(status.attempts));
        }

        match &status.error {
            Some((message, time)) => {
                // 長いエラー文でダイアログの幅が広がらないよう折り返す。
                // 繋がったあとも記録として残り続けるため、今まさに失敗している
                // わけではない。失敗ではなく注意として出す
                warning_label(ui, message);
                ui.label(i18n::error_time(time));
            }
            None => {
                ui.label(Text::NoRecentError.get());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::settings::AppSettings;
    use crate::status::LinkStatus;
    use crate::ui::testing::{dialog_harness, DialogFixture};
    use crate::ui::{SettingsTab, VideoPinChoice};
    use egui_kittest::kittest::Queryable;

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

    // ---- 設定ファイルの枠（#505） ----

    fn status_tab_harness(
        connection: ConnectionStatus,
    ) -> egui_kittest::Harness<'static, DialogFixture> {
        let fixture = DialogFixture::new(
            &AppSettings::default(),
            SettingsTab::Status,
            VideoPinChoice::default(),
            &[],
        )
        .with_connection(connection);
        dialog_harness(fixture)
    }

    #[test]
    fn status_tab_shows_the_settings_file_error() {
        // 起動時に読めなかった知らせはトーストが上書きされうるので、ここで読み返せること
        let message = "設定ファイルを読み書きできません: 既定値に初期化した";
        let connection = ConnectionStatus {
            settings_load_notice: Some((message.to_string(), "12:34:56".to_string())),
            ..ConnectionStatus::default()
        };

        let harness = status_tab_harness(connection);

        harness.get_by_label(Text::SettingsFile.get());
        // 注意書きの記号が前に付く
        harness.get_by_label(&format!("{} {message}", NoticeKind::Warning.symbol()));
    }

    #[test]
    fn status_tab_keeps_the_load_notice_beside_a_later_settings_error() {
        // 読み込みの知らせ（#505）は、あとの保存の失敗と並べて出し、上書きさせない
        let notice = "設定ファイルを読み書きできません: 既定値に初期化した";
        let error = "設定ファイルを読み書きできません: C:\\config へ書き出せない";
        let connection = ConnectionStatus {
            settings_load_notice: Some((notice.to_string(), "12:34:56".to_string())),
            settings_error: Some((error.to_string(), "12:35:00".to_string())),
            ..ConnectionStatus::default()
        };

        let harness = status_tab_harness(connection);

        let symbol = NoticeKind::Warning.symbol();
        harness.get_by_label(&format!("{symbol} {notice}"));
        harness.get_by_label(&format!("{symbol} {error}"));
    }

    #[test]
    fn status_tab_hides_the_settings_file_box_without_an_error() {
        let harness = status_tab_harness(ConnectionStatus::default());

        assert!(harness.query_by_label(Text::SettingsFile.get()).is_none());
    }
}
