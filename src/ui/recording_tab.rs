//! 「録画」タブ（`docs/design/recording.md` の「設定 `[recording]`」）。
//!
//! 保存先・ファイル名の書式・映像のビットレート・ハードウェアエンコーダの 4 つ。
//! 音声（②）とリプレイバッファ（③）の項目は、効く段になってから足す。
//! フォルダの選択はここでは開かず、`SettingsEvent::PickRecordingFolder` で上へ返す
//! （`docs/design/settings-dialog.md`）。

use chrono::Local;
use eframe::egui;

use super::{warning_label, SettingsEvent};
use crate::i18n::{self, Text};
use crate::recording::{render_file_name, RECORDING_EXTENSION};
use crate::settings::{AppSettings, MAX_RECORDING_BITRATE_KBPS, MIN_RECORDING_BITRATE_KBPS};

/// 「録画」タブを描画する。書き換えるのはドラフトだけ。
pub(super) fn show_recording_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading(Text::TabRecording.get());
    ui.add_space(5.0);
    ui.small(Text::RecordingTabHint.get());
    ui.add_space(10.0);

    // 保存先
    ui.group(|ui| {
        ui.strong(Text::SaveLocation.get());
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.label(Text::SaveFolderLabel.get());
            let mut folder = settings.recording.folder.to_string_lossy().to_string();
            if ui.text_edit_singleline(&mut folder).changed() {
                settings.recording.folder = std::path::PathBuf::from(folder);
            }
            if ui.button(Text::ButtonBrowse.get()).clicked() {
                events.push(SettingsEvent::PickRecordingFolder);
            }
        });
    });

    ui.add_space(15.0);

    // ファイル名の書式。録画を始めるときと同じ判定で注意書きを出す
    ui.group(|ui| {
        ui.strong(Text::RecordingFileNameGroup.get());
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.label(Text::RecordingFileNameFormatLabel.get());
            ui.text_edit_singleline(&mut settings.recording.file_name_format);
        });
        match render_file_name(&settings.recording.file_name_format, &Local::now()) {
            Ok(name) => {
                ui.label(i18n::recording_file_name_preview(format!(
                    "{name}.{RECORDING_EXTENSION}"
                )));
            }
            Err(reason) => warning_label(ui, reason.to_string()),
        }
        ui.add_space(5.0);
        ui.small(Text::RecordingFileNameHint.get());
    });

    ui.add_space(15.0);

    // 映像
    ui.group(|ui| {
        ui.strong(Text::LinkVideo.get());
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.label(Text::RecordingBitrateLabel.get());
            ui.add(
                egui::Slider::new(
                    &mut settings.recording.video_bitrate_kbps,
                    MIN_RECORDING_BITRATE_KBPS..=MAX_RECORDING_BITRATE_KBPS,
                )
                .logarithmic(true)
                .suffix(" kbps"),
            );
        });
        ui.checkbox(
            &mut settings.recording.hardware_encoder,
            Text::RecordingHardwareEncoder.get(),
        );
        ui.add_space(5.0);
        ui.small(Text::RecordingVideoHint.get());
    });
}
