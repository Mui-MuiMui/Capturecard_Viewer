//! 「スクリーンショット設定」タブ。
//!
//! 効果音の再生もファイルダイアログもここでは行わず、イベントとして
//! 上へ返す（`docs/design/settings-dialog.md`）。

use crate::settings::{
    AppSettings, ScreenshotDestination, ScreenshotFormat, MAX_JPEG_QUALITY, MIN_JPEG_QUALITY,
};
use eframe::egui;

use super::SettingsEvent;

/// スクリーンショット設定タブを描画する。
///
/// 効果音の再生もファイルダイアログもここでは行わず、イベントとして
/// 上へ返す（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
/// **`rfd` のファイルダイアログは UI スレッドを止めるモーダル**なので、
/// 描画の途中で開くと止まった位置のフレームが表示されたままになる。
/// 設定の書き出し・読み込みと同じく、フレームを描き終えてから開く。
pub(super) fn show_screenshot_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading("スクリーンショット設定");
    ui.add_space(10.0);

    // 出力先
    ui.group(|ui| {
        ui.strong("出力先");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::File,
                "ファイルに保存",
            );
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::Clipboard,
                "クリップボードにコピー",
            );
            ui.radio_value(
                &mut settings.screenshot.destination,
                ScreenshotDestination::Both,
                "両方",
            );
        });

        ui.add_space(5.0);
        ui.small(
            "クリップボードへは圧縮せずそのままの画をコピーします。
             保存場所と保存形式は、ファイルに保存するときだけ使われます。",
        );
    });

    ui.add_space(15.0);

    // 保存場所と保存形式はファイルへ出すときだけ効く。クリップボードだけを
    // 選んでいるときは触れないようにして、変えても何も起きない項目を操作させない
    // （JPEG 品質を PNG のときに無効にしているのと同じ考え方）
    let saves_file = settings.screenshot.destination.saves_file();

    // 保存フォルダー
    ui.add_enabled_ui(saves_file, |ui| {
        ui.group(|ui| {
            ui.strong("保存場所");
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.label("保存フォルダ:");
                let mut folder_str = settings
                    .screenshot
                    .save_folder
                    .to_string_lossy()
                    .to_string();
                ui.text_edit_singleline(&mut folder_str);
                settings.screenshot.save_folder = std::path::PathBuf::from(folder_str);

                if ui.button("参照...").clicked() {
                    events.push(SettingsEvent::PickScreenshotFolder);
                }
            });
        });

        ui.add_space(15.0);

        // 保存形式
        ui.group(|ui| {
            ui.strong("保存形式");
            ui.add_space(5.0);

            ui.horizontal(|ui| {
                ui.radio_value(
                    &mut settings.screenshot.format,
                    ScreenshotFormat::Jpeg,
                    "JPEG (.jpg)",
                );
                ui.radio_value(
                    &mut settings.screenshot.format,
                    ScreenshotFormat::Png,
                    "PNG (.png)",
                );
            });

            // 品質は JPEG のときだけ効く。PNG では触れないようにして、
            // 変えても何も起きない項目を操作させない
            let jpeg_selected = settings.screenshot.format == ScreenshotFormat::Jpeg;
            ui.horizontal(|ui| {
                ui.label("JPEG 品質:");
                ui.add_enabled(
                    jpeg_selected,
                    egui::Slider::new(
                        &mut settings.screenshot.jpeg_quality,
                        MIN_JPEG_QUALITY..=MAX_JPEG_QUALITY,
                    ),
                );
            });

            ui.add_space(5.0);
            ui.small(
                "JPEG はファイルが小さくなりますが、文字や細い線ににじみが出ます。
                 PNG は元の画をそのまま保存できるかわりに、ファイルが数倍の大きさになります。",
            );
        });
    });

    ui.add_space(15.0);

    // サウンド設定
    ui.group(|ui| {
        ui.strong("効果音");
        ui.add_space(5.0);

        ui.horizontal(|ui| {
            ui.label("サウンドファイル:");
            let sound_file_str = settings
                .screenshot
                .sound_file
                .as_ref()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| "未選択".to_string());
            ui.label(&sound_file_str);

            if ui.button("ファイル選択...").clicked() {
                events.push(SettingsEvent::PickSoundFile);
            }
        });

        if settings.screenshot.sound_file.is_some() {
            ui.horizontal(|ui| {
                ui.label("音量:");
                ui.add(
                    egui::Slider::new(&mut settings.screenshot.sound_volume, 0.0..=200.0)
                        .suffix("%"),
                );
            });

            ui.horizontal(|ui| {
                if ui.button("テスト再生").clicked() {
                    events.push(SettingsEvent::TestSound);
                }
                if ui.button("クリア").clicked() {
                    settings.screenshot.sound_file = None;
                }
            });
        }
    });
}
