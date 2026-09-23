//! 「スクリーンショット設定」タブ。
//!
//! 効果音の再生もファイルダイアログもここでは行わず、イベントとして
//! 上へ返す（`docs/design/settings-dialog.md`）。

use crate::settings::{
    AppSettings, ScreenshotDestination, ScreenshotFormat, DEFAULT_SOUND_FILE, MAX_JPEG_QUALITY,
    MIN_JPEG_QUALITY,
};
use eframe::egui;
use std::path::{Path, PathBuf};

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

        // 表示に要るものを先に取り出しておく。SoundChoice はドラフトを借りるので、
        // 持ったままではボタンでドラフトを書き換えられない
        let choice = SoundChoice::of(settings.screenshot.sound_file.as_deref());
        let label = choice.label();
        let full_path = match choice {
            SoundChoice::File(path) => Some(path.display().to_string()),
            SoundChoice::Silent | SoundChoice::Default => None,
        };
        let is_default = choice == SoundChoice::Default;

        ui.horizontal(|ui| {
            ui.label("サウンドファイル:");
            let shown = ui.label(label);
            // 欄にはファイル名しか出さないので、どこのファイルかは重ねて見せる
            if let Some(full_path) = full_path {
                shown.on_hover_text(full_path);
            }

            if ui.button("ファイル選択...").clicked() {
                events.push(SettingsEvent::PickSoundFile);
            }
            // 内蔵の既定音はファイルとして配布していないので、ファイル選択からは
            // 選び直せない。既定値を書き戻すこのボタンが唯一の戻し方になる
            if ui
                .add_enabled(!is_default, egui::Button::new("既定に戻す"))
                .on_hover_text("内蔵の効果音を使います")
                .clicked()
            {
                settings.screenshot.sound_file = Some(PathBuf::from(DEFAULT_SOUND_FILE));
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
                // None は「鳴らさない」の意味（ScreenshotManager::clear_sound）。
                // 以前は「クリア」という文言で、既定音に戻る操作と区別が付かなかった
                if ui.button("効果音を鳴らさない").clicked() {
                    settings.screenshot.sound_file = None;
                }
            });
        }
    });
}

/// 効果音の欄に出す、いま選ばれている効果音の種類。
///
/// 設定の値は `Option<PathBuf>` だけなので、内蔵の既定音は既定値のパス
/// （`DEFAULT_SOUND_FILE`）と一致するかで見分ける。既定値のファイルは
/// 配布しておらず、`screenshot::resolve_sound_path` が埋め込みの既定音へ
/// 倒して鳴らす（`docs/design/assets.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoundChoice<'a> {
    /// 効果音を鳴らさない（`None`）
    Silent,
    /// 内蔵の既定音
    Default,
    /// ユーザーが選んだファイル
    File(&'a Path),
}

impl<'a> SoundChoice<'a> {
    fn of(sound_file: Option<&'a Path>) -> Self {
        match sound_file {
            None => SoundChoice::Silent,
            // 空のパスも resolve_sound_path が内蔵音へ倒すので、同じ扱いにする
            Some(path) if path.as_os_str().is_empty() || path == Path::new(DEFAULT_SOUND_FILE) => {
                SoundChoice::Default
            }
            Some(path) => SoundChoice::File(path),
        }
    }

    fn label(&self) -> String {
        match self {
            SoundChoice::Silent => "なし（効果音を鳴らさない）".to_string(),
            SoundChoice::Default => "既定（内蔵）".to_string(),
            SoundChoice::File(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ScreenshotSettings;

    #[test]
    fn sound_choice_default_settings_is_default() {
        // 既定値と「既定に戻す」が書く値が食い違うと、戻したのに「既定（内蔵）」と出ない
        let settings = ScreenshotSettings::default();
        assert_eq!(
            SoundChoice::of(settings.sound_file.as_deref()),
            SoundChoice::Default
        );
        assert_eq!(SoundChoice::Default.label(), "既定（内蔵）");
    }

    #[test]
    fn sound_choice_none_is_silent() {
        assert_eq!(SoundChoice::of(None), SoundChoice::Silent);
        assert_eq!(SoundChoice::Silent.label(), "なし（効果音を鳴らさない）");
    }

    #[test]
    fn sound_choice_backslash_default_path_is_default() {
        // 設定ファイルを手で書き換えて区切りが \ になっていても既定として扱う
        let path = PathBuf::from("sound\\SS.mp3");
        assert_eq!(SoundChoice::of(Some(&path)), SoundChoice::Default);
    }

    #[test]
    fn sound_choice_empty_path_is_default() {
        let path = PathBuf::new();
        assert_eq!(SoundChoice::of(Some(&path)), SoundChoice::Default);
    }

    #[test]
    fn sound_choice_custom_file_shows_file_name() {
        let path = PathBuf::from("C:\\sounds\\shutter.wav");
        let choice = SoundChoice::of(Some(&path));
        assert_eq!(choice, SoundChoice::File(&path));
        assert_eq!(choice.label(), "shutter.wav");
    }

    #[test]
    fn sound_choice_same_name_in_other_folder_is_file() {
        // 名前が同じでも場所が違えばユーザーが選んだファイル
        let path = PathBuf::from("C:\\sounds\\SS.mp3");
        assert_eq!(SoundChoice::of(Some(&path)), SoundChoice::File(&path));
    }
}
