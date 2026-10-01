//! 「デバイス設定」タブの「オーディオ入力デバイス」（#394）。
//!
//! 先頭に「映像デバイスの音声 (DirectShow)」（`[audio] input_source = "video_pin"`）を
//! 置き、区切りの下に WASAPI の入力デバイスを並べる。選べるかどうかと選べない理由は
//! `app` がワーカーの観測値（`ActiveVideo::audio_pin`）から作り、`SettingsDialogView`
//! の借用で受け取る。**描画の中でデバイスに問い合わせない**
//! （`docs/design/directshow-audio.md` の (5)）。

use crate::audio::AudioCapabilities;
use crate::i18n::Text;
use crate::settings::{AppSettings, AudioInputSource};
use eframe::egui;

use super::warning_label;

/// 「映像デバイスの音声 (DirectShow)」を選べるか。`SettingsDialogState` が持ち、
/// `app` が描画の前に毎回差し替える。
#[derive(Debug, Clone, PartialEq)]
pub enum VideoPinChoice {
    /// 選べる。音声ピンを繋いでいればその形式 1 つの対応設定を持つ（サンプリング
    /// レートとチャンネル数の選択肢に使う）。繋いでいなければ `None` で、入力側の
    /// 制約にしない
    Selectable(Option<AudioCapabilities>),
    /// 選べない。理由はホバーと、選ばれているときのコンボボックスの下に出す
    Unavailable(String),
}

impl Default for VideoPinChoice {
    /// 映像の観測値がまだ届いていないときは「映像デバイスが開いていない」と同じ扱い
    fn default() -> Self {
        Self::Unavailable(Text::AudioPinVideoNotOpen.get().to_string())
    }
}

impl VideoPinChoice {
    fn is_selectable(&self) -> bool {
        matches!(self, Self::Selectable(_))
    }

    /// 入力が音声ピンのときに、選択肢を作る入力側の対応設定。
    pub(super) fn capabilities(&self) -> Option<&AudioCapabilities> {
        match self {
            Self::Selectable(capabilities) => capabilities.as_ref(),
            Self::Unavailable(_) => None,
        }
    }
}

/// 入力の種類とデバイス名を、閉じたコンボボックスに出す文言にする。
fn selected_text(settings: &AppSettings) -> &str {
    match (
        settings.audio.input_source,
        settings.audio.input_device_name.as_deref(),
    ) {
        (AudioInputSource::VideoPin, _) => Text::AudioInputVideoPin.get(),
        (AudioInputSource::Device, Some(name)) if !name.is_empty() => name,
        (AudioInputSource::Device, _) => Text::SelectDevice.get(),
    }
}

/// 「オーディオ入力デバイス」のコンボボックスを描く。入力（種類かデバイス名）が
/// 変わったら `true`。
///
/// - 「映像デバイスの音声」を選ぶと `input_source = VideoPin`。入力デバイス名は
///   書き換えない（WASAPI へ戻したときに前の選択を戻すため、(4)）
/// - WASAPI のデバイスを選ぶと `input_source = Device` と入力デバイス名を書く
/// - 選べないときは灰色にしてホバーで理由を出す。**すでに選ばれている設定は、選べない
///   状態でも選ばれたまま表示し**、コンボボックスの下に理由を出す（選び直しを強いない。
///   映像デバイスを抜いているだけのことがある）
pub(super) fn show_audio_input_combo(
    ui: &mut egui::Ui,
    settings: &mut AppSettings,
    inputs: &[String],
    pin: &VideoPinChoice,
) -> bool {
    let before = (
        settings.audio.input_source,
        settings.audio.input_device_name.clone(),
    );
    let pin_selected = before.0 == AudioInputSource::VideoPin;
    let text = selected_text(settings).to_string();

    // Id は表示文字列から作らない（docs/design/i18n.md）
    egui::ComboBox::new("audio_input_device_combo", Text::AudioInputDevice.get())
        .selected_text(text)
        .show_ui(ui, |ui| {
            let response = ui.add_enabled(
                pin.is_selectable(),
                egui::SelectableLabel::new(pin_selected, Text::AudioInputVideoPin.get()),
            );
            let response = match pin {
                VideoPinChoice::Unavailable(reason) => response.on_disabled_hover_text(reason),
                VideoPinChoice::Selectable(_) => response,
            };
            if response.clicked() {
                settings.audio.input_source = AudioInputSource::VideoPin;
            }
            ui.separator();
            for name in inputs {
                let selected =
                    !pin_selected && settings.audio.input_device_name.as_deref() == Some(name);
                if ui.selectable_label(selected, name).clicked() {
                    settings.audio.input_source = AudioInputSource::Device;
                    settings.audio.input_device_name = Some(name.clone());
                }
            }
        });

    if settings.audio.input_source == AudioInputSource::VideoPin {
        if let VideoPinChoice::Unavailable(reason) = pin {
            warning_label(ui, reason.clone());
        }
    }
    (
        settings.audio.input_source,
        settings.audio.input_device_name.as_deref(),
    ) != (before.0, before.1.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{PinFormat, PinSampleType};

    #[test]
    fn selected_text_follows_the_input_source() {
        let mut settings = AppSettings::default();
        settings.audio.input_device_name = Some("ライン入力".to_string());
        assert_eq!(selected_text(&settings), "ライン入力");
        // 音声ピンの間は入力デバイス名が残っていても項目の名前を出す
        settings.audio.input_source = AudioInputSource::VideoPin;
        assert_eq!(selected_text(&settings), Text::AudioInputVideoPin.get());
        settings.audio.input_source = AudioInputSource::Device;
        settings.audio.input_device_name = None;
        assert_eq!(selected_text(&settings), Text::SelectDevice.get());
    }

    #[test]
    fn video_pin_choice_gives_capabilities_only_when_selectable() {
        let caps = PinFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: PinSampleType::I16,
        }
        .capabilities();
        let choice = VideoPinChoice::Selectable(Some(caps.clone()));
        assert!(choice.is_selectable());
        assert_eq!(choice.capabilities(), Some(&caps));
        assert_eq!(caps.sample_rates(), vec![48_000]);
        assert_eq!(caps.channels(), vec![2]);

        assert_eq!(VideoPinChoice::Selectable(None).capabilities(), None);
        let unavailable = VideoPinChoice::default();
        assert!(!unavailable.is_selectable());
        assert_eq!(unavailable.capabilities(), None);
    }
}
