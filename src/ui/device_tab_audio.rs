//! 「デバイス設定」タブのうちオーディオ設定の枠（入出力デバイス、サンプリングレート、
//! チャンネル数、音声バッファ、パススルー）。
//!
//! 枠の外側（見出しと `ui.group`）と、ビデオ設定・UI 設定の枠は `device_tab.rs`。
//! 選択肢は `capability` のキャッシュから作り、描画中にデバイスへ問い合わせない
//! （`docs/design/error-reporting.md`）。

use crate::audio::{self, AudioDirection, ChoiceSource};
use crate::i18n::Text;
use crate::settings::{
    AppSettings, AudioInputSource, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MAX_BUFFER_MS,
    MIN_BUFFER_MS,
};
use eframe::egui;
use log::debug;

use super::audio_input::{show_audio_input_combo, VideoPinChoice};
use super::capability_view::{
    channel_label, out_of_range_note, show_audio_capability_progress, show_choice_note,
};
use super::device_tab::choice_combo;
use super::{warning_label, AudioCapabilityCaches, CapabilityEvent, DeviceLists, SettingsEvent};

/// オーディオ設定の枠の中身を描く。`ui.group` の中から呼ぶ
pub(super) fn show_audio_settings(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    audio_capabilities: &AudioCapabilityCaches<'_>,
    video_pin: &VideoPinChoice,
    devices: &DeviceLists<'_>,
    events: &mut Vec<SettingsEvent>,
) {
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
        if let Some(channels) = audio::nearest_channels(&channel_choices.values, desired_channels) {
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
}
