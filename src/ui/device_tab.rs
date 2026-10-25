//! 「デバイス設定」タブ。
//!
//! ビデオとオーディオのデバイス・フォーマット・映像調整を並べる。
//! 選択肢は `capability` のキャッシュから作り、描画中にデバイスへ
//! 問い合わせない（`docs/design/error-reporting.md`）。オーディオ設定の枠の
//! 中身は `device_tab_audio.rs`。

use crate::i18n::Text;
use crate::settings::{
    AppSettings, ColorRange, ColorSpace, VideoBackendSetting, VideoConvertSetting,
    MAX_VIDEO_ADJUSTMENT, MIN_VIDEO_ADJUSTMENT,
};
use eframe::egui;
use log::debug;

use super::audio_input::VideoPinChoice;
use super::capability::{should_reselect_video_defaults, VideoCapabilityCache, VideoCapabilityKey};
use super::capability_view::show_video_capability_progress;
use super::device_tab_audio::show_audio_settings;
use super::video_mode::{fps_choices, resolution_choices, select_default_video_mode};
use super::{
    name_slider, warning_label, AudioCapabilityCaches, CapabilityEvent, DeviceLists, SettingsEvent,
};

/// 映像調整のスライダー 1 本。明るさ・コントラスト・彩度で見た目を揃える。
///
/// 3 本とも範囲と既定値が同じなので、目盛りの刻みや中央の位置が
/// 揃っていないと「0 が無調整」であることが読み取りにくくなる。
fn video_adjustment_slider(ui: &mut egui::Ui, value: &mut i32, label: &str, hint: &str) {
    ui.add(
        egui::Slider::new(value, MIN_VIDEO_ADJUSTMENT..=MAX_VIDEO_ADJUSTMENT)
            .text(label)
            .clamping(egui::SliderClamping::Always),
    )
    .on_hover_text(hint);
}

/// コンボの一覧に `(値, 表示)` の選択肢を並べ、押された項目の値を `value` へ入れる。
///
/// Id・見出し・閉じているときの表示は呼び出し側が作った `combo` が持つ。Id は固定の
/// 値を渡し、表示文字列から作らない（`docs/design/i18n.md`）。書き換えるのはドラフトの
/// `value` だけで、起きたことは呼び出し側が `SettingsEvent` で返す。
///
/// 返すのはコンボの `Response` と、項目が押されたか。**押した項目がいまの値と同じでも
/// 真になる。** フォーマット・解像度は押し直すとその下の選択を入れ直すため
///
/// 見出しを横に別の `Label` で置くときは、呼び出し側が返した `Response` に
/// `labelled_by` で見出しを結ぶ。読み上げとウィジェットのテスト（egui_kittest は名前の
/// 無い入力のウィジェットで落ちる）がコンボを名前で引けるようにするため
pub(super) fn choice_combo<T: PartialEq, S: Into<egui::WidgetText>>(
    ui: &mut egui::Ui,
    combo: egui::ComboBox,
    value: &mut T,
    choices: impl IntoIterator<Item = (T, S)>,
) -> (egui::Response, bool) {
    let mut clicked = false;
    let response = combo
        .show_ui(ui, |ui| {
            for (choice, text) in choices {
                clicked |= ui.selectable_value(value, choice, text).clicked();
            }
        })
        .response;
    (response, clicked)
}

/// 見出し・コンボ・説明の 1 行。選択肢が設定の列挙（`ALL` と `label`）で決まるものに使う
fn enum_combo_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    id_salt: &str,
    value: &mut T,
    all: impl IntoIterator<Item = T>,
    label_of: fn(T) -> &'static str,
    hint: &str,
) {
    ui.horizontal(|ui| {
        let label = ui.label(label);
        let combo = egui::ComboBox::from_id_salt(id_salt).selected_text(label_of(*value));
        let choices = all.into_iter().map(|choice| (choice, label_of(choice)));
        // 名前は見出しから取る。説明は読み上げの補足（accessible description）になる
        choice_combo(ui, combo, value, choices)
            .0
            .labelled_by(label.id)
            .on_hover_text(hint);
    });
}

pub(super) fn show_device_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    capabilities: &VideoCapabilityCache,
    audio_capabilities: &AudioCapabilityCaches<'_>,
    video_pin: &VideoPinChoice,
    devices: &DeviceLists<'_>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading(Text::TabDevice.get());
    ui.add_space(10.0);

    // ビデオ設定
    ui.group(|ui| {
        ui.strong(Text::VideoSettings.get());
        ui.add_space(5.0);

        // ビデオデバイス選択
        // 一覧は app 側でキャッシュ済みのものを受け取る（毎フレームの列挙を避けるため）
        let current_device = settings.video.device_name.clone().unwrap_or_default();
        // 切り替え前のキー。デバイスか開き方が変わったら既定値を選び直す
        let previous_key =
            VideoCapabilityKey::new(settings.video.device_name.as_deref(), settings.video.backend);

        // Id は表示文字列から作らない。言語を切り替えると Id が変わり、
        // 開いていた一覧の状態が引き継がれないため（docs/design/i18n.md）
        let video_combo = egui::ComboBox::new("video_device_combo", Text::VideoDevice.get())
            .selected_text(if current_device.is_empty() {
                Text::SelectDevice.get()
            } else {
                &current_device
            });
        let video_choices = devices.video.iter().map(|(name, description)| {
            let display_text = if description.is_empty() {
                name.clone()
            } else {
                format!("{} ({})", name, description)
            };
            (Some(name.clone()), display_text)
        });
        choice_combo(
            ui,
            video_combo,
            &mut settings.video.device_name,
            video_choices,
        );

        // 映像の開き方（#237）。両方に出るデバイスを DirectShow で開くための
        // 切り替え。変えたら他の映像設定と同じく「適用」で開き直す
        // （`DeviceConfig` の差分判定に載っている）
        enum_combo_row(
            ui,
            Text::VideoBackendLabel.get(),
            "video_backend_combo",
            &mut settings.video.backend,
            VideoBackendSetting::ALL,
            VideoBackendSetting::label,
            Text::VideoBackendHint.get(),
        );
        if settings.video.backend != VideoBackendSetting::Auto {
            warning_label(ui, Text::VideoBackendNotice.get());
        }

        // YUY2 → RGB の変換の場所（#456）。デバイスは開き直さず、「適用」で次のフレームから効く
        enum_combo_row(
            ui,
            Text::VideoConvertLabel.get(),
            "video_convert_combo",
            &mut settings.video.convert,
            VideoConvertSetting::ALL,
            VideoConvertSetting::label,
            Text::VideoConvertHint.get(),
        );

        // 選択後のデバイス名と開き方。この下の能力参照はすべてこちらを使う。
        // 切り替えたフレームで切り替え前のキーを見ると、1 フレームだけ前の
        // デバイス（開き方）の選択肢が出てしまう。開き方もキーに含めるのは、
        // 両方に出るデバイスを DirectShow で開くときに DirectShow 側の対応形式を
        // 選択肢に出すため（#249）
        let selected_key =
            VideoCapabilityKey::new(settings.video.device_name.as_deref(), settings.video.backend);

        if should_reselect_video_defaults(&previous_key, &selected_key) {
            // 能力が届いた時点でフォーマットを選び直させる
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ExpectVideoDefaults(selected_key.clone()),
            ));
        }

        // 能力の取得を要求する。デバイスを開くのはワーカーなので UI は止まらない。
        // 要求済み・取得済み・失敗済みのときは何も起きない
        events.push(SettingsEvent::Capability(CapabilityEvent::RequestVideo(
            selected_key.clone(),
        )));

        // 取得の進行状況。失敗や既定の一覧を黙って出すと、選択肢が実態と違う理由が
        // ユーザーに分からない
        show_video_capability_progress(ui, capabilities, &selected_key, events);

        // 切り替えたデバイスの能力が届いたら、フォーマット・解像度・FPS を
        // まとめて選び直す。
        //
        // 能力の取得はワーカー側なので、切り替えた直後はまだ `Pending` で
        // ここを通らない。その間は前のデバイスの値が出たままになるが、
        // 入れ直しの手掛かりとして必要なので消さない。`expect_defaults` の
        // 目印が残るため、`Ready` になったフレームで入れ直される。
        // 取得に失敗したときは入れ直さない（選択肢が既定値のままなので、
        // そこへ寄せても実態に合わない）。
        //
        // 目印を落とすのは `app`。**入れ直せたかどうかに関わらず落とす。**
        // 残すと、ユーザーが選び直したフォーマットを毎フレーム先頭へ戻す
        if capabilities.awaits_defaults(&selected_key) {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ClearVideoDefaults(selected_key.clone()),
            ));
            if let Some((format, resolution, fps)) =
                capabilities.ready(&selected_key).and_then(|caps| {
                    select_default_video_mode(caps, settings.video.resolution, settings.video.fps)
                })
            {
                debug!(
                    "デバイスか開き方を {}（{:?}）に切り替えたので既定値を選び直した: {} {}x{} {}fps",
                    selected_key.device, selected_key.backend, format, resolution.0, resolution.1, fps
                );
                settings.video.format = Some(format);
                settings.video.resolution = Some(resolution);
                settings.video.fps = Some(fps);
            }
        }

        // フォーマット選択（フォーマットが起点）
        // 既定の一覧から選んで押しても、下の入れ直しはキャッシュが無ければ何もしない
        let video_caps = capabilities.ready(&selected_key);
        let format_changed = ui
            .horizontal(|ui| {
                let label = ui.label(Text::FormatLabel.get());
                let current_format = settings
                    .video
                    .format
                    .clone()
                    .unwrap_or_else(|| "YUY2".to_string());
                let combo =
                    egui::ComboBox::from_id_salt("format_combo").selected_text(&current_format);
                // キャッシュがない場合はデフォルト
                let names: Vec<String> = match video_caps {
                    Some(caps) => caps
                        .iter()
                        .map(|capability| capability.name.clone())
                        .collect(),
                    None => ["YUY2", "MJPEG", "RGB24"].map(String::from).into(),
                };
                let choices = names.into_iter().map(|name| (Some(name.clone()), name));
                let (response, clicked) =
                    choice_combo(ui, combo, &mut settings.video.format, choices);
                response.labelled_by(label.id);
                clicked
            })
            .inner;

        // フォーマット変更時に解像度をリセット
        if format_changed {
            if let Some(caps) = capabilities.ready(&selected_key) {
                if let Some(current_format) = &settings.video.format {
                    // 現在のフォーマットに対応する最初の解像度を選択
                    for capability in caps {
                        if &capability.name == current_format {
                            if let Some(mode) = capability.modes.first() {
                                settings.video.resolution = Some(mode.resolution());
                                settings.video.fps = Some(mode.fps);
                            }
                            break;
                        }
                    }
                }
            }
        }

        // 解像度選択（フォーマットに応じて動的に変更）
        let resolution_changed = ui
            .horizontal(|ui| {
                let label = ui.label(Text::ResolutionLabel.get());
                let current_resolution = settings.video.resolution.unwrap_or((1280, 720));
                let combo = egui::ComboBox::from_id_salt("resolution_combo")
                    .selected_text(format!("{}x{}", current_resolution.0, current_resolution.1));
                let resolutions = match video_caps {
                    Some(caps) => settings
                        .video
                        .format
                        .as_deref()
                        .map(|format| resolution_choices(caps, format))
                        .unwrap_or_default(),
                    // デフォルトの解像度
                    None => vec![(1920, 1080), (1280, 720), (640, 480)],
                };
                let choices = resolutions
                    .into_iter()
                    .map(|(w, h)| (Some((w, h)), format!("{}x{}", w, h)));
                let (response, clicked) =
                    choice_combo(ui, combo, &mut settings.video.resolution, choices);
                response.labelled_by(label.id);
                clicked
            })
            .inner;

        // 解像度変更時にFPSをリセット
        if resolution_changed {
            if let Some(caps) = capabilities.ready(&selected_key) {
                if let Some(current_format) = &settings.video.format {
                    if let Some((w, h)) = settings.video.resolution {
                        // 現在のフォーマットと解像度に対応する最初のFPSを選択
                        for capability in caps {
                            if &capability.name == current_format {
                                for mode in &capability.modes {
                                    if mode.resolution() == (w, h) {
                                        settings.video.fps = Some(mode.fps);
                                        break;
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // FPS選択（フォーマットと解像度に応じて動的に変更）
        ui.horizontal(|ui| {
            let label = ui.label(Text::FrameRateLabel.get());
            let current_fps = settings.video.fps.unwrap_or(30);

            let combo = egui::ComboBox::from_id_salt("fps_combo")
                .selected_text(format!("{} fps", current_fps));
            let fps_list = match video_caps {
                // 現在のフォーマットと解像度に対応するFPS一覧
                Some(caps) => match (settings.video.format.as_deref(), settings.video.resolution) {
                    (Some(format), Some(resolution)) => fps_choices(caps, format, resolution),
                    _ => Vec::new(),
                },
                // デフォルトのFPS
                None => vec![30, 60],
            };
            let choices = fps_list
                .into_iter()
                .map(|fps| (Some(fps), format!("{} fps", fps)));
            choice_combo(ui, combo, &mut settings.video.fps, choices)
                .0
                .labelled_by(label.id);
        });

        // 色空間の選択。デバイスは入力信号の色空間を通知してこないので、
        // 通常は解像度から推定する（自動）。推定が外れる機種のために固定できる
        enum_combo_row(
            ui,
            Text::ColorSpaceLabel.get(),
            "color_space_combo",
            &mut settings.video.color_space,
            ColorSpace::ALL,
            ColorSpace::label,
            Text::ColorSpaceHint.get(),
        );

        // 輝度レンジの選択。フルレンジで出すかどうかはデバイス側の設定次第で、
        // 信号からも解像度からも判別できないため手で選ばせる
        enum_combo_row(
            ui,
            Text::ColorRangeLabel.get(),
            "color_range_combo",
            &mut settings.video.color_range,
            ColorRange::ALL,
            ColorRange::label,
            Text::ColorRangeHint.get(),
        );

        ui.add_space(5.0);

        // 映像調整。色空間・レンジを合わせても残る機種ごとのクセを手で埋める。
        // 3 つとも YUY2 → RGB の係数表へ畳み込まれるので、変換は重くならない
        ui.horizontal(|ui| {
            ui.strong(Text::VideoAdjustments.get());
            if ui
                .button(Text::ButtonReset.get())
                .on_hover_text(Text::VideoAdjustmentsResetHint.get())
                .clicked()
            {
                settings.video.brightness = 0;
                settings.video.contrast = 0;
                settings.video.saturation = 0;
            }
        });

        video_adjustment_slider(
            ui,
            &mut settings.video.brightness,
            Text::Brightness.get(),
            Text::BrightnessHint.get(),
        );
        video_adjustment_slider(
            ui,
            &mut settings.video.contrast,
            Text::Contrast.get(),
            Text::ContrastHint.get(),
        );
        video_adjustment_slider(
            ui,
            &mut settings.video.saturation,
            Text::Saturation.get(),
            Text::SaturationHint.get(),
        );
    });

    ui.add_space(15.0);

    // オーディオ設定
    ui.group(|ui| {
        show_audio_settings(ui, settings, audio_capabilities, video_pin, devices, events);
    });

    ui.add_space(15.0);

    // UI設定
    ui.group(|ui| {
        ui.strong(Text::UserInterface.get());
        ui.add_space(5.0);

        ui.checkbox(
            &mut settings.ui.maintain_aspect_ratio,
            Text::MaintainAspectRatio.get(),
        );

        ui.horizontal(|ui| {
            ui.label(Text::InitialVolumeLabel.get());
            name_slider(
                ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%")),
                Text::InitialVolumeLabel.get(),
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::audio::{self, AudioDirection};
    use crate::ui::testing::{dialog_harness, DialogFixture};
    use crate::ui::SettingsTab;
    use crate::video::capabilities::FormatCapability;
    use crate::video::{DeviceCapabilities, VideoMode};
    use eframe::egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    /// 閉じているときの表示が `value` のコンボを開く。見出しを持たないコンボは表示で探す
    fn open_combo(harness: &mut Harness<'_, DialogFixture>, value: &str) {
        let combos: Vec<_> = harness
            .get_all_by_role(Role::ComboBox)
            .filter(|node| node.value().as_deref() == Some(value))
            .collect();
        assert_eq!(combos.len(), 1, "表示が {value} のコンボは 1 つのはず");
        combos[0].click();
        harness.run();
    }

    /// 「デバイス設定」タブを開き、「Capture Device」の能力に `caps` が届いた設定ダイアログ
    fn device_tab(
        draft: &AppSettings,
        caps: DeviceCapabilities,
    ) -> Harness<'static, DialogFixture> {
        let mut fixture = DialogFixture::new(
            draft,
            SettingsTab::Device,
            VideoPinChoice::selectable(None, None),
            &[],
        )
        .with_video_devices(&["Capture Device"])
        .with_output_devices(&["スピーカー"]);
        let key = VideoCapabilityKey::new(Some("Capture Device"), VideoBackendSetting::Auto);
        fixture.state.capabilities_mut().apply_result(key, Ok(caps));
        let mut harness = dialog_harness(fixture);
        harness.run();
        harness
    }

    #[test]
    fn choosing_format_and_resolution_reselects_the_settings_below() {
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![VideoMode::new(1920, 1080, 60)]),
            FormatCapability::new(
                "MJPEG",
                vec![
                    VideoMode::new(1280, 720, 30),
                    VideoMode::new(3840, 2160, 24),
                    VideoMode::new(3840, 2160, 30),
                ],
            ),
        ];
        let mut draft = AppSettings::default();
        draft.video.device_name = Some("Capture Device".to_string());
        draft.video.format = Some("YUY2".to_string());
        draft.video.resolution = Some((1920, 1080));
        draft.video.fps = Some(60);
        let mut harness = device_tab(&draft, caps);

        // フォーマットを選ぶと、そのフォーマットの先頭の組み合わせへ入れ直す
        open_combo(&mut harness, "YUY2");
        harness.get_by_label("MJPEG").click();
        harness.run();
        let video = &harness.state().draft().video;
        assert_eq!(video.format.as_deref(), Some("MJPEG"));
        assert_eq!((video.resolution, video.fps), (Some((1280, 720)), Some(30)));

        // 解像度を選ぶと、その解像度の先頭の FPS へ入れ直す
        open_combo(&mut harness, "1280x720");
        harness.get_by_label("3840x2160").click();
        harness.run();
        let video = &harness.state().draft().video;
        assert_eq!(
            (video.resolution, video.fps),
            (Some((3840, 2160)), Some(24))
        );

        open_combo(&mut harness, "24 fps");
        harness.get_by_label("30 fps").click();
        harness.run();
        assert_eq!(harness.state().draft().video.fps, Some(30));
    }

    #[test]
    fn choosing_color_range_and_output_device_updates_the_draft() {
        let mut harness = device_tab(&AppSettings::default(), Vec::new());

        open_combo(&mut harness, ColorRange::Limited.label());
        harness.get_by_label(ColorRange::Full.label()).click();
        harness.run();
        assert_eq!(harness.state().draft().video.color_range, ColorRange::Full);

        // 出力デバイスを替えたら、届いた対応設定へ寄せ直すよう要求する
        // 出力デバイスのコンボは 900x800 の画面の下に外れるので、見える位置まで送ってから押す
        harness
            .get_by_label(Text::AudioOutputDevice.get())
            .scroll_to_me();
        harness.run();
        harness.get_by_label(Text::AudioOutputDevice.get()).click();
        harness.run();
        harness.get_by_label("スピーカー").click();
        harness.run();
        assert_eq!(
            harness.state().draft().audio.output_device_name.as_deref(),
            Some("スピーカー")
        );
        let speaker = audio::cache_key(Some("スピーカー"));
        assert!(harness.state().events.iter().any(|event| matches!(
            event,
            SettingsEvent::Capability(CapabilityEvent::ExpectAudioDefaults(
                AudioDirection::Output,
                key,
            )) if *key == speaker
        )));
    }
}
