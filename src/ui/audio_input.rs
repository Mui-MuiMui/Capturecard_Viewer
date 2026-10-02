//! 「デバイス設定」タブの「オーディオ入力デバイス」（#394、#409）。
//!
//! 先頭に映像デバイスの音声（`[audio] input_source = "video_pin"`）を置き、区切りの
//! 下に WASAPI の入力デバイスを並べる。先頭の項目名は**映像デバイスの表示名そのもの**
//! で、「DirectShow」「音声ピン」の語は利用者に見せない（#409）。選べるかどうかと
//! 選べない理由、開いている映像の名前は `app` がワーカーの観測値（`ActiveVideo`）から
//! 作り、`SettingsDialogView` の借用で受け取る。**描画の中でデバイスに問い合わせない**
//! （`docs/design/directshow-audio.md` の (5)）。

use crate::audio::AudioCapabilities;
use crate::i18n::Text;
use crate::settings::{AppSettings, AudioInputSource};
use crate::video::directshow_friendly_name;
use eframe::egui;

use super::warning_label;

/// 映像デバイスの音声を選べるか。
#[derive(Debug, Clone, PartialEq)]
enum PinAvailability {
    /// 選べる。音声ピンを繋いでいればその形式 1 つの対応設定を持つ（サンプリング
    /// レートとチャンネル数の選択肢に使う）。繋いでいなければ `None` で、入力側の
    /// 制約にしない
    Selectable(Option<AudioCapabilities>),
    /// 選べない。理由はホバーと、選ばれているときのコンボボックスの下に出す
    Unavailable(String),
}

/// 先頭の項目（映像デバイスの音声）を選べるかと、項目名に使う映像デバイスの名前。
/// `SettingsDialogState` が持ち、`app` が描画の前に毎回差し替える。
#[derive(Debug, Clone, PartialEq)]
pub struct VideoPinChoice {
    availability: PinAvailability,
    /// 開いている映像デバイスの名前。開いていなければ `None` で、項目名は設定の
    /// `video.device_name` から作る
    active_device: Option<String>,
}

impl Default for VideoPinChoice {
    /// 映像の観測値がまだ届いていないときは「映像デバイスが開いていない」と同じ扱い
    fn default() -> Self {
        Self::unavailable(Text::AudioPinVideoNotOpen.get().to_string(), None)
    }
}

impl VideoPinChoice {
    /// 選べる。`capabilities` は繋いでいる音声ピンの形式 1 つの対応設定
    pub fn selectable(
        capabilities: Option<AudioCapabilities>,
        active_device: Option<String>,
    ) -> Self {
        Self {
            availability: PinAvailability::Selectable(capabilities),
            active_device,
        }
    }

    /// 選べない。`reason` はホバーとコンボボックスの下に出す
    pub fn unavailable(reason: String, active_device: Option<String>) -> Self {
        Self {
            availability: PinAvailability::Unavailable(reason),
            active_device,
        }
    }

    fn is_selectable(&self) -> bool {
        matches!(self.availability, PinAvailability::Selectable(_))
    }

    fn unavailable_reason(&self) -> Option<&str> {
        match &self.availability {
            PinAvailability::Unavailable(reason) => Some(reason),
            PinAvailability::Selectable(_) => None,
        }
    }

    /// 入力が音声ピンのときに、選択肢を作る入力側の対応設定。
    pub(super) fn capabilities(&self) -> Option<&AudioCapabilities> {
        match &self.availability {
            PinAvailability::Selectable(capabilities) => capabilities.as_ref(),
            PinAvailability::Unavailable(_) => None,
        }
    }

    /// 先頭の項目名。`configured_device` は設定（ドラフト）の映像デバイス名
    fn label(&self, configured_device: Option<&str>) -> String {
        video_pin_label(self.active_device.as_deref(), configured_device)
    }
}

/// 映像デバイスの音声の項目名を決める（#409）。
///
/// 開いている映像の名前、無ければ設定の映像デバイス名を、「(DirectShow)」の印を
/// 外して出す。どちらも無い（空の）ときは従来の文言（`Text::AudioInputVideoPin`）。
fn video_pin_label(active_device: Option<&str>, configured_device: Option<&str>) -> String {
    [active_device, configured_device]
        .into_iter()
        .flatten()
        .map(|name| directshow_friendly_name(name).unwrap_or(name).trim())
        .find(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| Text::AudioInputVideoPin.get().to_string())
}

/// 入力の種類とデバイス名を、閉じたコンボボックスに出す文言にする。
fn selected_text(settings: &AppSettings, pin: &VideoPinChoice) -> String {
    match (
        settings.audio.input_source,
        settings.audio.input_device_name.as_deref(),
    ) {
        (AudioInputSource::VideoPin, _) => pin.label(settings.video.device_name.as_deref()),
        (AudioInputSource::Device, Some(name)) if !name.is_empty() => name.to_string(),
        (AudioInputSource::Device, _) => Text::SelectDevice.get().to_string(),
    }
}

/// 「オーディオ入力デバイス」のコンボボックスを描く。入力（種類かデバイス名）が
/// 変わったら `true`。
///
/// - 先頭の項目（映像デバイスの音声。項目名は映像デバイスの名前）を選ぶと
///   `input_source = VideoPin`。入力デバイス名は書き換えない（WASAPI へ戻したときに
///   前の選択を戻すため、(4)）
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
    let text = selected_text(settings, pin);
    let pin_label = pin.label(settings.video.device_name.as_deref());

    // Id は表示文字列から作らない（docs/design/i18n.md）
    egui::ComboBox::new("audio_input_device_combo", Text::AudioInputDevice.get())
        .selected_text(text)
        .show_ui(ui, |ui| {
            let response = ui.add_enabled(
                pin.is_selectable(),
                egui::SelectableLabel::new(pin_selected, pin_label),
            );
            let response = match pin.unavailable_reason() {
                Some(reason) => response.on_disabled_hover_text(reason),
                None => response.on_hover_text(Text::AudioInputVideoPinHint.get()),
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
        if let Some(reason) = pin.unavailable_reason() {
            warning_label(ui, reason.to_string());
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
        let pin = VideoPinChoice::default();
        settings.audio.input_device_name = Some("ライン入力".to_string());
        assert_eq!(selected_text(&settings, &pin), "ライン入力");
        // 音声ピンの間は入力デバイス名が残っていても映像デバイスの名前を出す
        settings.audio.input_source = AudioInputSource::VideoPin;
        settings.video.device_name = Some("AVerMedia GC551 Video Capture (DirectShow)".to_string());
        assert_eq!(
            selected_text(&settings, &pin),
            "AVerMedia GC551 Video Capture"
        );
        settings.audio.input_source = AudioInputSource::Device;
        settings.audio.input_device_name = None;
        assert_eq!(selected_text(&settings, &pin), Text::SelectDevice.get());
    }

    #[test]
    fn video_pin_label_prefers_the_open_device_without_the_directshow_mark() {
        // 開いている映像の名前が先。「(DirectShow)」の印は外す
        assert_eq!(
            video_pin_label(Some("GC551 (DirectShow)"), Some("別のデバイス")),
            "GC551"
        );
        // 開いていなければ設定の映像デバイス名
        assert_eq!(video_pin_label(None, Some("USB Video")), "USB Video");
        assert_eq!(
            video_pin_label(None, Some("USB Video (DirectShow)")),
            "USB Video"
        );
        // 空の名前は飛ばす
        assert_eq!(video_pin_label(Some(""), Some("USB Video")), "USB Video");
        // どちらも無ければ従来の文言
        assert_eq!(video_pin_label(None, None), Text::AudioInputVideoPin.get());
        assert_eq!(
            video_pin_label(Some("  "), Some("")),
            Text::AudioInputVideoPin.get()
        );
    }

    #[test]
    fn video_pin_choice_gives_capabilities_only_when_selectable() {
        let caps = PinFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_type: PinSampleType::I16,
        }
        .capabilities();
        let choice = VideoPinChoice::selectable(Some(caps.clone()), Some("GC551".to_string()));
        assert!(choice.is_selectable());
        assert_eq!(choice.unavailable_reason(), None);
        assert_eq!(choice.capabilities(), Some(&caps));
        assert_eq!(caps.sample_rates(), vec![48_000]);
        assert_eq!(caps.channels(), vec![2]);

        assert_eq!(VideoPinChoice::selectable(None, None).capabilities(), None);
        let unavailable = VideoPinChoice::default();
        assert!(!unavailable.is_selectable());
        assert_eq!(
            unavailable.unavailable_reason(),
            Some(Text::AudioPinVideoNotOpen.get())
        );
        assert_eq!(unavailable.capabilities(), None);
    }
}
