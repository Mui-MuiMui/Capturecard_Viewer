//! 「デバイス設定」タブ。
//!
//! ビデオとオーディオのデバイス・フォーマット・映像調整を並べる。
//! 選択肢は `capability` のキャッシュから作り、描画中にデバイスへ
//! 問い合わせない（`docs/design/error-reporting.md`）。

use crate::audio::{self, AudioDirection, ChoiceSource};
use crate::settings::{
    AppSettings, ColorRange, ColorSpace, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MAX_BUFFER_MS,
    MAX_VIDEO_ADJUSTMENT, MIN_BUFFER_MS, MIN_VIDEO_ADJUSTMENT,
};
use eframe::egui;
use log::debug;

use super::capability::{
    channel_label, out_of_range_note, show_audio_capability_progress, show_choice_note,
    CapabilityState, VideoCapabilityCache,
};
use super::video_mode::select_default_video_mode;
use super::{warning_label, AudioCapabilityCaches, CapabilityEvent, DeviceLists, SettingsEvent};

/// 映像調整のスライダー 1 本。明るさ・コントラスト・彩度で見た目を揃える。
///
/// 3 本とも範囲と既定値が同じなので、目盛りの刻みや中央の位置が
/// 揃っていないと「0 が無調整」であることが読み取りにくくなる。
fn video_adjustment_slider(ui: &mut egui::Ui, value: &mut i32, label: &str, hint: &str) {
    ui.add(
        egui::Slider::new(value, MIN_VIDEO_ADJUSTMENT..=MAX_VIDEO_ADJUSTMENT)
            .text(label)
            .clamp_to_range(true),
    )
    .on_hover_text(hint);
}

pub(super) fn show_device_settings_tab(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    capabilities: &VideoCapabilityCache,
    audio_capabilities: &AudioCapabilityCaches<'_>,
    devices: &DeviceLists<'_>,
    events: &mut Vec<SettingsEvent>,
) {
    ui.heading("デバイス設定");
    ui.add_space(10.0);

    // ビデオ設定
    ui.group(|ui| {
        ui.strong("ビデオ設定");
        ui.add_space(5.0);

        // ビデオデバイス選択
        // 一覧は app 側でキャッシュ済みのものを受け取る（毎フレームの列挙を避けるため）
        let current_device = settings.video.device_name.clone().unwrap_or_default();

        let mut device_changed = false;
        egui::ComboBox::from_label("ビデオデバイス")
            .selected_text(if current_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_device
            })
            .show_ui(ui, |ui| {
                for (name, description) in devices.video {
                    let display_text = if description.is_empty() {
                        name.clone()
                    } else {
                        format!("{} ({})", name, description)
                    };
                    if ui
                        .selectable_label(
                            settings.video.device_name.as_ref() == Some(name),
                            display_text,
                        )
                        .clicked()
                        && settings.video.device_name.as_ref() != Some(name)
                    {
                        settings.video.device_name = Some(name.clone());
                        device_changed = true;
                    }
                }
            });

        // 選択後のデバイス名。この下の能力参照はすべてこちらを使う。
        // 切り替えたフレームで切り替え前の名前を見ると、1 フレームだけ前の
        // デバイスの選択肢が出てしまう
        let selected_device = settings.video.device_name.clone().unwrap_or_default();

        if device_changed {
            // 能力が届いた時点でフォーマットを選び直させる
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ExpectVideoDefaults(selected_device.clone()),
            ));
        }

        // 能力の取得を要求する。デバイスを開くのはワーカーなので UI は止まらない。
        // 要求済み・取得済み・失敗済みのときは何も起きない
        events.push(SettingsEvent::Capability(CapabilityEvent::RequestVideo(
            selected_device.clone(),
        )));

        // 取得の進行状況。失敗を黙って捨てると、選択肢が既定値のまま出る理由が
        // ユーザーに分からない
        let mut retry_requested = false;
        match capabilities.state(&selected_device) {
            Some(CapabilityState::Pending) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("対応形式を取得中...");
                });
            }
            Some(CapabilityState::Failed(reason)) => {
                // 理由はデバイス由来の長い文字列になることがある。ボタンと横に並べると
                // 折り返せずダイアログからはみ出すので、行を分ける
                warning_label(ui, format!("対応形式を取得できませんでした: {}", reason));
                if ui.button("再取得").clicked() {
                    retry_requested = true;
                }
                ui.label("下の選択肢は既定値です。");
            }
            _ => {}
        }
        if retry_requested {
            events.push(SettingsEvent::Capability(CapabilityEvent::RetryVideo(
                selected_device.clone(),
            )));
        }

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
        if capabilities.awaits_defaults(&selected_device) {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ClearVideoDefaults(selected_device.clone()),
            ));
            if let Some((format, resolution, fps)) =
                capabilities.ready(&selected_device).and_then(|caps| {
                    select_default_video_mode(caps, settings.video.resolution, settings.video.fps)
                })
            {
                debug!(
                    "デバイスを {} に切り替えたので既定値を選び直した: {} {}x{} {}fps",
                    selected_device, format, resolution.0, resolution.1, fps
                );
                settings.video.format = Some(format);
                settings.video.resolution = Some(resolution);
                settings.video.fps = Some(fps);
            }
        }

        // フォーマット選択（フォーマットが起点）
        let mut format_changed = false;
        ui.horizontal(|ui| {
            ui.label("フォーマット:");
            let current_format = settings
                .video
                .format
                .clone()
                .unwrap_or_else(|| "YUY2".to_string());

            egui::ComboBox::from_id_source("format_combo")
                .selected_text(&current_format)
                .show_ui(ui, |ui| {
                    // キャッシュからフォーマット一覧を取得
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        for capability in caps {
                            if ui
                                .selectable_value(
                                    &mut settings.video.format,
                                    Some(capability.name.clone()),
                                    &capability.name,
                                )
                                .clicked()
                            {
                                format_changed = true;
                            }
                        }
                    } else {
                        // キャッシュがない場合はデフォルト
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("YUY2".to_string()),
                            "YUY2",
                        );
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("MJPEG".to_string()),
                            "MJPEG",
                        );
                        ui.selectable_value(
                            &mut settings.video.format,
                            Some("RGB24".to_string()),
                            "RGB24",
                        );
                    }
                });
        });

        // フォーマット変更時に解像度をリセット
        if format_changed {
            if let Some(caps) = capabilities.ready(&selected_device) {
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
        let mut resolution_changed = false;
        ui.horizontal(|ui| {
            ui.label("解像度:");
            let current_resolution = settings.video.resolution.unwrap_or((1280, 720));

            egui::ComboBox::from_id_source("resolution_combo")
                .selected_text(format!("{}x{}", current_resolution.0, current_resolution.1))
                .show_ui(ui, |ui| {
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        if let Some(current_format) = &settings.video.format {
                            // 現在のフォーマットに対応する解像度一覧
                            let mut unique_resolutions =
                                std::collections::HashSet::<(u32, u32)>::new();
                            for capability in caps {
                                if &capability.name == current_format {
                                    for mode in &capability.modes {
                                        unique_resolutions.insert(mode.resolution());
                                    }
                                }
                            }

                            // ソートして表示
                            let mut sorted_resolutions: Vec<_> =
                                unique_resolutions.into_iter().collect();
                            sorted_resolutions.sort_by(|a, b| {
                                let size_a = a.0 * a.1;
                                let size_b = b.0 * b.1;
                                size_b.cmp(&size_a)
                            });

                            for (w, h) in sorted_resolutions {
                                if ui
                                    .selectable_value(
                                        &mut settings.video.resolution,
                                        Some((w, h)),
                                        format!("{}x{}", w, h),
                                    )
                                    .clicked()
                                {
                                    resolution_changed = true;
                                }
                            }
                        }
                    } else {
                        // デフォルトの解像度
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((1920, 1080)),
                            "1920x1080",
                        );
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((1280, 720)),
                            "1280x720",
                        );
                        ui.selectable_value(
                            &mut settings.video.resolution,
                            Some((640, 480)),
                            "640x480",
                        );
                    }
                });
        });

        // 解像度変更時にFPSをリセット
        if resolution_changed {
            if let Some(caps) = capabilities.ready(&selected_device) {
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
            ui.label("フレームレート:");
            let current_fps = settings.video.fps.unwrap_or(30);

            egui::ComboBox::from_id_source("fps_combo")
                .selected_text(format!("{} fps", current_fps))
                .show_ui(ui, |ui| {
                    if let Some(caps) = capabilities.ready(&selected_device) {
                        if let Some(current_format) = &settings.video.format {
                            if let Some((w, h)) = settings.video.resolution {
                                // 現在のフォーマットと解像度に対応するFPS一覧
                                let mut available_fps = Vec::new();
                                for capability in caps {
                                    if &capability.name == current_format {
                                        for mode in &capability.modes {
                                            if mode.resolution() == (w, h) {
                                                available_fps.push(mode.fps);
                                            }
                                        }
                                    }
                                }

                                // 重複を削除してソート
                                available_fps.sort();
                                available_fps.dedup();
                                available_fps.reverse(); // 大きい順

                                for fps in available_fps {
                                    ui.selectable_value(
                                        &mut settings.video.fps,
                                        Some(fps),
                                        format!("{} fps", fps),
                                    );
                                }
                            }
                        }
                    } else {
                        // デフォルトのFPS
                        ui.selectable_value(&mut settings.video.fps, Some(30), "30 fps");
                        ui.selectable_value(&mut settings.video.fps, Some(60), "60 fps");
                    }
                });
        });

        // 色空間の選択。デバイスは入力信号の色空間を通知してこないので、
        // 通常は解像度から推定する（自動）。推定が外れる機種のために固定できる
        ui.horizontal(|ui| {
            ui.label("色空間:");
            egui::ComboBox::from_id_source("color_space_combo")
                .selected_text(settings.video.color_space.label())
                .show_ui(ui, |ui| {
                    for space in ColorSpace::ALL {
                        ui.selectable_value(&mut settings.video.color_space, space, space.label());
                    }
                })
                .response
                .on_hover_text("色がずれて見える場合に切り替えます。通常は自動のままで構いません");
        });

        // 輝度レンジの選択。フルレンジで出すかどうかはデバイス側の設定次第で、
        // 信号からも解像度からも判別できないため手で選ばせる
        ui.horizontal(|ui| {
            ui.label("色レンジ:");
            egui::ComboBox::from_id_source("color_range_combo")
                .selected_text(settings.video.color_range.label())
                .show_ui(ui, |ui| {
                    for range in ColorRange::ALL {
                        ui.selectable_value(&mut settings.video.color_range, range, range.label());
                    }
                })
                .response
                .on_hover_text("黒が灰色に浮く、または黒潰れ・白飛びする場合に切り替えます");
        });

        ui.add_space(5.0);

        // 映像調整。色空間・レンジを合わせても残る機種ごとのクセを手で埋める。
        // 3 つとも YUY2 → RGB の係数表へ畳み込まれるので、変換は重くならない
        ui.horizontal(|ui| {
            ui.strong("映像調整");
            if ui
                .button("リセット")
                .on_hover_text("明るさ・コントラスト・彩度を無調整（0）へ戻します")
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
            "明るさ",
            "映像全体を明るく（＋）または暗く（－）します",
        );
        video_adjustment_slider(
            ui,
            &mut settings.video.contrast,
            "コントラスト",
            "明暗の差を強く（＋）または弱く（－）します。-100 で中間グレー一色になります",
        );
        video_adjustment_slider(
            ui,
            &mut settings.video.saturation,
            "彩度",
            "色の濃さを強く（＋）または弱く（－）します。-100 で白黒になります",
        );
    });

    ui.add_space(15.0);

    // オーディオ設定
    ui.group(|ui| {
        ui.strong("オーディオ設定");
        ui.add_space(5.0);

        // オーディオ入力デバイス選択 - キャッシュリストを使用
        let current_input_device = settings.audio.input_device_name.clone().unwrap_or_default();

        let mut input_changed = false;
        egui::ComboBox::from_label("オーディオ入力デバイス")
            .selected_text(if current_input_device.is_empty() {
                "デバイスを選択..."
            } else {
                &current_input_device
            })
            .show_ui(ui, |ui| {
                for device_name in devices.input {
                    if ui
                        .selectable_value(
                            &mut settings.audio.input_device_name,
                            Some(device_name.clone()),
                            device_name,
                        )
                        .clicked()
                        && current_input_device != *device_name
                    {
                        input_changed = true;
                    }
                }
            });

        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings
            .audio
            .output_device_name
            .clone()
            .unwrap_or_default();

        let mut output_changed = false;
        egui::ComboBox::from_label("オーディオ出力デバイス")
            .selected_text(if current_output_device.is_empty() {
                "デフォルト"
            } else {
                &current_output_device
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut settings.audio.output_device_name, None, "デフォルト")
                    .clicked()
                    && !current_output_device.is_empty()
                {
                    output_changed = true;
                }
                for device_name in devices.output {
                    if ui
                        .selectable_value(
                            &mut settings.audio.output_device_name,
                            Some(device_name.clone()),
                            device_name,
                        )
                        .clicked()
                        && current_output_device != *device_name
                    {
                        output_changed = true;
                    }
                }
            });

        // 選択後のデバイス名から作るキャッシュのキー。ビデオ側と同じく、
        // 切り替えたフレームで切り替え前の名前を見ると 1 フレームだけ
        // 前のデバイスの選択肢が出てしまう
        let input_key = audio::cache_key(settings.audio.input_device_name.as_deref());
        let output_key = audio::cache_key(settings.audio.output_device_name.as_deref());

        if input_changed {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ExpectAudioDefaults(AudioDirection::Input, input_key.clone()),
            ));
        }
        if output_changed {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ExpectAudioDefaults(AudioDirection::Output, output_key.clone()),
            ));
        }

        // 対応設定の取得を要求する。列挙はワーカーなので UI は止まらない
        events.push(SettingsEvent::Capability(CapabilityEvent::RequestAudio(
            AudioDirection::Input,
            input_key.clone(),
        )));
        events.push(SettingsEvent::Capability(CapabilityEvent::RequestAudio(
            AudioDirection::Output,
            output_key.clone(),
        )));

        show_audio_capability_progress(ui, audio_capabilities, &input_key, &output_key, events);

        // 入出力の両方が対応する値だけを選択肢にする。取得できていない側は
        // 制約にしない（片側だけ、どちらも無ければ固定の既定一覧）
        let rates = audio::selectable_sample_rates(
            audio_capabilities.input.ready(&input_key),
            audio_capabilities.output.ready(&output_key),
        );
        let channel_choices = audio::selectable_channels(
            audio_capabilities.input.ready(&input_key),
            audio_capabilities.output.ready(&output_key),
        );
        // 設定に希望値が入っていないときの手掛かり。入力デバイスの既定を採る
        // （入力が音の出どころなので、そちらへ揃えるほうが変換が減る）
        let input_defaults = audio_capabilities
            .input
            .ready(&input_key)
            .map(|caps| (caps.default_sample_rate(), caps.default_channels()));

        // デバイスを切り替えたあとに能力が届いたら、対応する値へ寄せ直す。
        // **両方を必ず調べて、立っている目印は両方とも落とす要求を返す。**
        // 片方で早期に打ち切ると、残った目印のせいで次のフレームでも
        // もう一度寄せ直してしまう
        let input_awaits = audio_capabilities.input.awaits_defaults(&input_key);
        let output_awaits = audio_capabilities.output.awaits_defaults(&output_key);
        if input_awaits {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ClearAudioDefaults(AudioDirection::Input, input_key.clone()),
            ));
        }
        if output_awaits {
            events.push(SettingsEvent::Capability(
                CapabilityEvent::ClearAudioDefaults(AudioDirection::Output, output_key.clone()),
            ));
        }
        let repick = input_awaits || output_awaits;
        if repick {
            let desired_rate = settings
                .audio
                .sample_rate
                .or(input_defaults.map(|(rate, _)| rate))
                .unwrap_or(DEFAULT_SAMPLE_RATE);
            if let Some(rate) = audio::nearest_sample_rate(&rates.values, desired_rate) {
                if settings.audio.sample_rate != Some(rate) {
                    debug!("オーディオデバイスの切り替えでサンプリングレートを {} Hz にした", rate);
                }
                settings.audio.sample_rate = Some(rate);
            }
            let desired_channels = settings
                .audio
                .channels
                .or(input_defaults.map(|(_, channels)| channels))
                .unwrap_or(DEFAULT_CHANNELS);
            if let Some(channels) = audio::nearest_channels(&channel_choices.values, desired_channels)
            {
                if settings.audio.channels != Some(channels) {
                    debug!("オーディオデバイスの切り替えでチャンネル数を {} ch にした", channels);
                }
                settings.audio.channels = Some(channels);
            }
        }

        // 選択肢が 1 つしか無い値は、デバイスを切り替えていなくてもそこへ寄せる。
        //
        // **`repick` の目印はデバイスを選び直したときにしか立たない。** 設定ファイルに
        // 古い値が残ったまま（Windows 側で既定デバイスの形式を変えた、設定ファイルを
        // 手で書き換えた）起動すると、選べる値が 1 つしか無いのに違う値が残る。
        // チャンネル数のコンボは 1 択のとき操作できないので、ユーザーが直す手段が無い
        if let [only] = rates.values[..] {
            if settings.audio.sample_rate != Some(only) {
                debug!("サンプリングレートの選択肢が 1 つなので {} Hz に寄せた", only);
                settings.audio.sample_rate = Some(only);
            }
        }
        if let [only] = channel_choices.values[..] {
            if settings.audio.channels != Some(only) {
                debug!("チャンネル数の選択肢が 1 つなので {} ch に寄せた", only);
                settings.audio.channels = Some(only);
            }
        }

        // サンプルレート
        ui.horizontal(|ui| {
            ui.label("サンプリングレート:");
            let current_rate = settings.audio.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
            egui::ComboBox::from_id_source("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate))
                .show_ui(ui, |ui| {
                    for rate in &rates.values {
                        ui.selectable_value(
                            &mut settings.audio.sample_rate,
                            Some(*rate),
                            format!("{} Hz", rate),
                        );
                    }
                });
        });
        show_choice_note(ui, rates.source, "サンプリングレート");
        // 設定ファイルを手で書き換えた場合など、選択肢に無い値が残ることがある。
        // 黙って別の値で開くと「選んだ値と違う」理由が分からない
        if let Some(note) = out_of_range_note(
            &rates.values,
            settings.audio.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE),
            " Hz",
        ) {
            warning_label(ui, note);
        }

        // チャンネル数
        let single_channel_choice = channel_choices.values.len() == 1;
        ui.horizontal(|ui| {
            ui.label("チャンネル数:");
            let current_channels = settings.audio.channels.unwrap_or(DEFAULT_CHANNELS);
            // 選択肢が 1 つしか無いときは操作させない。開ける値が 1 つなのに
            // 選べると、選んだ値と実際の値が食い違う
            ui.add_enabled_ui(!single_channel_choice, |ui| {
                egui::ComboBox::from_id_source("channels_combo")
                    .selected_text(channel_label(current_channels))
                    .show_ui(ui, |ui| {
                        for channels in &channel_choices.values {
                            ui.selectable_value(
                                &mut settings.audio.channels,
                                Some(*channels),
                                channel_label(*channels),
                            );
                        }
                    });
            });
        });
        if single_channel_choice && channel_choices.source != ChoiceSource::Fallback {
            // WASAPI は共有モードのミックスフォーマットしか列挙しないため、
            // Windows では実質ここに落ちる
            ui.label("このデバイスの組み合わせでは 1 つしか選べません（Windows の共有モードではデバイスのミックスフォーマットに固定されます）");
        }
        show_choice_note(ui, channel_choices.source, "チャンネル数");
        let channel_values: Vec<u32> = channel_choices
            .values
            .iter()
            .map(|&channels| u32::from(channels))
            .collect();
        if let Some(note) = out_of_range_note(
            &channel_values,
            u32::from(settings.audio.channels.unwrap_or(DEFAULT_CHANNELS)),
            " ch",
        ) {
            warning_label(ui, note);
        }

        ui.add_space(10.0);

        // 音声バッファ（遅延）
        //
        // ここで書き換わるのはドラフト。デバイスを開き直すのは「適用」または
        // 「OK」のときで、スライダーを動かしている間は何も起きない。
        // サンプリングレートやチャンネル数と同じ経路に乗せてある
        ui.horizontal(|ui| {
            ui.label("音声バッファ:");
            ui.add(
                egui::Slider::new(
                    &mut settings.audio.buffer_ms,
                    MIN_BUFFER_MS..=MAX_BUFFER_MS,
                )
                .suffix(" ms"),
            );
        });
        ui.label("小さいほど低遅延だがノイズが出やすい（既定: 50 ms）");

        ui.add_space(10.0);

        // オーディオパススルー制御
        ui.horizontal(|ui| {
            ui.label("音声パススルー:");
            if ui
                .checkbox(&mut settings.audio.passthrough_enabled, "有効")
                .changed()
            {
                // ここで書き換わるのはドラフト。実設定へ反映されるのは「適用」または「OK」のとき
                debug!(
                    "設定ダイアログで音声パススルーを {} に変更した",
                    settings.audio.passthrough_enabled
                );
            }
        });

        if !settings.audio.passthrough_enabled {
            warning_label(ui, "音声パススルーが無効です（音は出力されません）");
        }
    });

    ui.add_space(15.0);

    // UI設定
    ui.group(|ui| {
        ui.strong("ユーザーインターフェース");
        ui.add_space(5.0);

        ui.checkbox(&mut settings.ui.maintain_aspect_ratio, "アスペクト比を維持");

        ui.horizontal(|ui| {
            ui.label("初期音量:");
            ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%"));
        });
    });
}
