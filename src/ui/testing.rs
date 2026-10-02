//! 設定ダイアログのウィジェットのテスト（egui_kittest、#419）で使う組み立て。
//!
//! `show_settings_dialog` を本番と同じ引数の形で呼び、1 フレームごとに返った
//! `SettingsEvent` を溜める。**描画関数の引数の形はテストのために変えない。**
//! 組み立てに要るもの（`SettingsDialogState`、デバイスの一覧、接続状態、更新の
//! 状態）はすべてここが持ち、テストは開くタブと設定・一覧を渡すだけにする。
//!
//! 描画はデバイスに問い合わせないので（`docs/design/settings-dialog.md`）、
//! 実機もフェイクも要らない。
//!
//! テストは描画の対象と同じファイルの `mod tests` に置く。ここはその土台だけ。

use crate::hotkey::{HotkeyAction, HotkeyAssignmentError};
use crate::settings::AppSettings;
use crate::status::ConnectionStatus;
use crate::update::{current_version, UpdateStatus, UpdateView};
use eframe::egui;
use egui_kittest::Harness;
use semver::Version;
use std::collections::BTreeMap;

use super::VideoPinChoice;
use super::{show_settings_dialog, DeviceLists, SettingsDialogState, SettingsEvent, SettingsTab};

/// 設定ダイアログの描画に渡すものを一式持つ。`Harness` の状態として使う。
pub(super) struct DialogFixture {
    pub(super) state: SettingsDialogState,
    video_devices: Vec<(String, String)>,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    connection: ConnectionStatus,
    pub(super) hotkey_errors: BTreeMap<HotkeyAction, HotkeyAssignmentError>,
    version: Version,
    update_status: UpdateStatus,
    /// 描画が返した `SettingsEvent`。**フレームをまたいで溜める。** クリックが
    /// 効くのは押したフレームなので、最後の 1 フレームだけを見ると取りこぼす
    pub(super) events: Vec<SettingsEvent>,
}

impl DialogFixture {
    /// `draft` を編集中のドラフトにして、`tab` を開いた状態の組み立て。
    /// 入力デバイスの一覧は `inputs`、映像デバイスの音声を選べるかは `pin`。
    pub(super) fn new(
        draft: &AppSettings,
        tab: SettingsTab,
        pin: VideoPinChoice,
        inputs: &[&str],
    ) -> Self {
        let mut state = SettingsDialogState::default();
        state.begin_edit(draft);
        state.select_tab(tab);
        state.set_video_pin(pin);
        Self {
            state,
            video_devices: Vec::new(),
            input_devices: inputs.iter().map(|name| name.to_string()).collect(),
            output_devices: Vec::new(),
            connection: ConnectionStatus::default(),
            hotkey_errors: BTreeMap::new(),
            version: current_version(),
            update_status: UpdateStatus::default(),
            events: Vec::new(),
        }
    }

    /// 編集中のドラフト。描画が書き換えた結果を見るのに使う
    pub(super) fn draft(&self) -> &AppSettings {
        self.state.draft().expect("ドラフトを持っているはず")
    }

    /// 1 フレーム分の描画。`app::settings_dialog` が毎フレーム行うのと同じ呼び方
    fn draw(&mut self, ui: &mut egui::Ui) {
        let Self {
            state,
            video_devices,
            input_devices,
            output_devices,
            connection,
            hotkey_errors,
            version,
            update_status,
            events,
        } = self;
        let Some((draft, view)) = state.split_for_draw() else {
            return;
        };
        let devices = DeviceLists {
            video: video_devices,
            input: input_devices,
            output: output_devices,
        };
        let update = UpdateView {
            current: version,
            status: update_status,
            applying: false,
        };
        events.extend(show_settings_dialog(
            ui.ctx(),
            draft,
            &view,
            &devices,
            connection,
            hotkey_errors,
            &update,
        ));
    }
}

/// 設定ダイアログを描く `Harness`。ダイアログが収まる大きさで作る
pub(super) fn dialog_harness(fixture: DialogFixture) -> Harness<'static, DialogFixture> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 800.0))
        .build_ui_state(|ui, fixture: &mut DialogFixture| fixture.draw(ui), fixture)
}
