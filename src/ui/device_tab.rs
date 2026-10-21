//! 「デバイス設定」タブ。
//!
//! ビデオとオーディオのデバイス・フォーマット・映像調整を並べる。
//! 選択肢は `capability` のキャッシュから作り、描画中にデバイスへ
//! 問い合わせない（`docs/design/error-reporting.md`）。

use crate::audio::{self, AudioDirection, ChoiceSource};
use crate::i18n::Text;
use crate::settings::{
    AppSettings, AudioInputSource, ColorRange, ColorSpace, VideoBackendSetting,
    VideoConvertSetting, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MAX_BUFFER_MS,
    MAX_VIDEO_ADJUSTMENT, MIN_BUFFER_MS, MIN_VIDEO_ADJUSTMENT,
};
use eframe::egui;
use log::debug;

use super::audio_input::{show_audio_input_combo, VideoPinChoice};
use super::capability::{
    channel_label, out_of_range_note, should_reselect_video_defaults,
    show_audio_capability_progress, show_choice_note, show_video_capability_progress,
    VideoCapabilityCache, VideoCapabilityKey,
};
use super::video_mode::{fps_choices, resolution_choices, select_default_video_mode};
use super::{warning_label, AudioCapabilityCaches, CapabilityEvent, DeviceLists, SettingsEvent};

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
fn choice_combo<T: PartialEq, S: Into<egui::WidgetText>>(
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
        ui.label(label);
        let combo = egui::ComboBox::from_id_salt(id_salt).selected_text(label_of(*value));
        let choices = all.into_iter().map(|choice| (choice, label_of(choice)));
        choice_combo(ui, combo, value, choices)
            .0
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
                ui.label(Text::FormatLabel.get());
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
                choice_combo(ui, combo, &mut settings.video.format, choices).1
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
                ui.label(Text::ResolutionLabel.get());
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
                choice_combo(ui, combo, &mut settings.video.resolution, choices).1
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
            ui.label(Text::FrameRateLabel.get());
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
            choice_combo(ui, combo, &mut settings.video.fps, choices);
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
        ui.strong(Text::AudioSettings.get());
        ui.add_space(5.0);

        // オーディオ入力デバイス選択。先頭が「映像デバイスの音声」、その下に
        // キャッシュした WASAPI のデバイス（#394）
        let input_changed = show_audio_input_combo(ui, settings, devices.input, video_pin);
        // 入力が音声ピンなら、入力の対応設定は音声ピンの形式 1 つだけ。ワーカーへは
        // 問い合わせない（`docs/design/directshow-audio.md` の (6)）
        let input_is_pin = settings.audio.input_source == AudioInputSource::VideoPin;

        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings
            .audio
            .output_device_name
            .clone()
            .unwrap_or_default();

        let output_combo =
            egui::ComboBox::new("audio_output_device_combo", Text::AudioOutputDevice.get())
                .selected_text(if current_output_device.is_empty() {
                    Text::DefaultDevice.get()
                } else {
                    &current_output_device
                });
        let output_choices = std::iter::once((None, Text::DefaultDevice.get().to_string())).chain(
            devices
                .output
                .iter()
                .map(|device_name| (Some(device_name.clone()), device_name.clone())),
        );
        choice_combo(
            ui,
            output_combo,
            &mut settings.audio.output_device_name,
            output_choices,
        );
        // 既定（`None`）は空の名前と同じに数える
        let output_changed = settings
            .audio
            .output_device_name
            .as_deref()
            .unwrap_or_default()
            != current_output_device;

        // 選択後のデバイス名から作るキャッシュのキー。ビデオ側と同じく、
        // 切り替えたフレームで切り替え前の名前を見ると 1 フレームだけ
        // 前のデバイスの選択肢が出てしまう
        let input_key = audio::cache_key(settings.audio.input_device_name.as_deref());
        let output_key = audio::cache_key(settings.audio.output_device_name.as_deref());

        if input_changed && !input_is_pin {
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
        if !input_is_pin {
            events.push(SettingsEvent::Capability(CapabilityEvent::RequestAudio(
                AudioDirection::Input,
                input_key.clone(),
            )));
        }
        events.push(SettingsEvent::Capability(CapabilityEvent::RequestAudio(
            AudioDirection::Output,
            output_key.clone(),
        )));

        let progress_input_key = (!input_is_pin).then_some(input_key.as_str());
        show_audio_capability_progress(
            ui,
            audio_capabilities,
            progress_input_key,
            &output_key,
            events,
        );

        // 入力側の対応設定。音声ピンなら繋いだ形式 1 つ（映像が開いていない・
        // 繋いでいない間は `None` で、入力側の制約にしない）
        let input_capabilities = if input_is_pin {
            video_pin.capabilities()
        } else {
            audio_capabilities.input.ready(&input_key)
        };
        // 入出力の両方が対応する値だけを選択肢にする。取得できていない側は
        // 制約にしない（片側だけ、どちらも無ければ固定の既定一覧）
        let rates = audio::selectable_sample_rates(
            input_capabilities,
            audio_capabilities.output.ready(&output_key),
        );
        let channel_choices = audio::selectable_channels(
            input_capabilities,
            audio_capabilities.output.ready(&output_key),
        );
        // 設定に希望値が入っていないときの手掛かり。入力デバイスの既定を採る
        // （入力が音の出どころなので、そちらへ揃えるほうが変換が減る）
        let input_defaults =
            input_capabilities.map(|caps| (caps.default_sample_rate(), caps.default_channels()));

        // デバイスを切り替えたあとに能力が届いたら、対応する値へ寄せ直す。
        // **両方を必ず調べて、立っている目印は両方とも落とす要求を返す。**
        // 片方で早期に打ち切ると、残った目印のせいで次のフレームでも
        // もう一度寄せ直してしまう。音声ピンへ切り替えたときは届くのを待たない
        // （形式は手元にある）ので、切り替えたフレームで寄せ直す
        let input_awaits = !input_is_pin && audio_capabilities.input.awaits_defaults(&input_key);
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
        let repick = input_awaits || output_awaits || (input_changed && input_is_pin);
        if repick {
            let desired_rate = settings
                .audio
                .sample_rate
                .or(input_defaults.map(|(rate, _)| rate))
                .unwrap_or(DEFAULT_SAMPLE_RATE);
            if let Some(rate) = audio::nearest_sample_rate(&rates.values, desired_rate) {
                if settings.audio.sample_rate != Some(rate) {
                    debug!(
                        "オーディオデバイスの切り替えでサンプリングレートを {} Hz にした",
                        rate
                    );
                }
                settings.audio.sample_rate = Some(rate);
            }
            let desired_channels = settings
                .audio
                .channels
                .or(input_defaults.map(|(_, channels)| channels))
                .unwrap_or(DEFAULT_CHANNELS);
            if let Some(channels) =
                audio::nearest_channels(&channel_choices.values, desired_channels)
            {
                if settings.audio.channels != Some(channels) {
                    debug!(
                        "オーディオデバイスの切り替えでチャンネル数を {} ch にした",
                        channels
                    );
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
                debug!(
                    "サンプリングレートの選択肢が 1 つなので {} Hz に寄せた",
                    only
                );
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
            ui.label(Text::SampleRateLabel.get());
            let current_rate = settings.audio.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
            let combo = egui::ComboBox::from_id_salt("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate));
            let choices = rates
                .values
                .iter()
                .map(|&rate| (Some(rate), format!("{} Hz", rate)));
            choice_combo(ui, combo, &mut settings.audio.sample_rate, choices);
        });
        show_choice_note(ui, rates.source, Text::SampleRate.get());
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
            ui.label(Text::ChannelsLabel.get());
            let current_channels = settings.audio.channels.unwrap_or(DEFAULT_CHANNELS);
            // 選択肢が 1 つしか無いときは操作させない。開ける値が 1 つなのに
            // 選べると、選んだ値と実際の値が食い違う
            ui.add_enabled_ui(!single_channel_choice, |ui| {
                let combo = egui::ComboBox::from_id_salt("channels_combo")
                    .selected_text(channel_label(current_channels));
                let choices = channel_choices
                    .values
                    .iter()
                    .map(|&channels| (Some(channels), channel_label(channels)));
                choice_combo(ui, combo, &mut settings.audio.channels, choices);
            });
        });
        if single_channel_choice && channel_choices.source != ChoiceSource::Fallback {
            // WASAPI は共有モードのミックスフォーマットしか列挙しないため、
            // Windows では実質ここに落ちる
            ui.label(Text::ChannelsFixedByDevice.get());
        }
        show_choice_note(ui, channel_choices.source, Text::Channels.get());
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
            ui.label(Text::AudioBufferLabel.get());
            ui.add(
                egui::Slider::new(&mut settings.audio.buffer_ms, MIN_BUFFER_MS..=MAX_BUFFER_MS)
                    .suffix(" ms"),
            );
        });
        ui.label(Text::AudioBufferHint.get());

        ui.add_space(10.0);

        // オーディオパススルー制御
        ui.horizontal(|ui| {
            ui.label(Text::PassthroughLabel.get());
            if ui
                .checkbox(&mut settings.audio.passthrough_enabled, Text::Enabled.get())
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
            warning_label(ui, Text::PassthroughDisabledWarning.get());
        }
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
            ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%"));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
