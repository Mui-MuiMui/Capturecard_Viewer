//! 「録画」タブ（`docs/design/recording.md` の「設定 `[recording]`」）。
//!
//! 保存先・ファイル名の書式・映像のビットレート・ハードウェアエンコーダ・音声（録るか、ビットレート、映像とのずれの補正）・
//! リプレイバッファ（ON / OFF、さかのぼる長さ）。
//! フォルダの選択はここでは開かず、`SettingsEvent::PickRecordingFolder` で上へ返す
//! （`docs/design/settings-dialog.md`）。

use chrono::Local;
use eframe::egui;

use super::{notice_label, warning_label, NoticeKind, SettingsEvent};
use crate::i18n::{self, Text};
use crate::recording::{render_file_name, RECORDING_EXTENSION};
use crate::settings::{
    AppSettings, MAX_RECORDING_AUDIO_OFFSET_MS, MAX_RECORDING_BITRATE_KBPS, MAX_REPLAY_SECONDS,
    MIN_RECORDING_AUDIO_OFFSET_MS, MIN_RECORDING_BITRATE_KBPS, MIN_REPLAY_SECONDS,
    RECORDING_AUDIO_BITRATES_KBPS,
};

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

    ui.add_space(15.0);

    // 音声
    ui.group(|ui| {
        ui.strong(Text::LinkAudio.get());
        ui.add_space(5.0);
        ui.checkbox(
            &mut settings.recording.audio_enabled,
            Text::RecordingAudioEnabled.get(),
        );
        ui.add_enabled_ui(settings.recording.audio_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(Text::RecordingBitrateLabel.get());
                // Id は表示文字列から作らない（`docs/design/i18n.md`）
                egui::ComboBox::from_id_salt("recording_audio_bitrate")
                    .selected_text(format!("{} kbps", settings.recording.audio_bitrate_kbps))
                    .show_ui(ui, |ui| {
                        for kbps in RECORDING_AUDIO_BITRATES_KBPS {
                            ui.selectable_value(
                                &mut settings.recording.audio_bitrate_kbps,
                                kbps,
                                format!("{kbps} kbps"),
                            );
                        }
                    });
            });
            // 映像と音声のずれの補正（#404）。正なら音声を遅らせ、負なら早める
            ui.horizontal(|ui| {
                ui.label(Text::RecordingAudioOffsetLabel.get());
                ui.add(
                    egui::Slider::new(
                        &mut settings.recording.audio_offset_ms,
                        MIN_RECORDING_AUDIO_OFFSET_MS..=MAX_RECORDING_AUDIO_OFFSET_MS,
                    )
                    .suffix(" ms"),
                );
            });
            ui.small(Text::RecordingAudioOffsetHint.get());
        });
        ui.add_space(5.0);
        ui.small(Text::RecordingAudioHint.get());
    });

    ui.add_space(15.0);

    // リプレイバッファ（さかのぼり録画、#182）。長さの横には「長くするほどメモリを使う」を添える
    ui.group(|ui| {
        ui.strong(Text::RecordingReplayGroup.get());
        ui.add_space(5.0);
        ui.checkbox(
            &mut settings.recording.replay_enabled,
            Text::RecordingReplayEnabled.get(),
        );
        ui.add_enabled_ui(settings.recording.replay_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(Text::RecordingReplaySecondsLabel.get());
                ui.add(
                    egui::Slider::new(
                        &mut settings.recording.replay_seconds,
                        MIN_REPLAY_SECONDS..=MAX_REPLAY_SECONDS,
                    )
                    .suffix(" s"),
                );
            });
            notice_label(
                ui,
                NoticeKind::Warning,
                Text::RecordingReplayMemoryNotice.get(),
            );
        });
        ui.add_space(5.0);
        ui.small(Text::RecordingReplayHint.get());
    });
}
