//! 「デバイス設定」タブの「オーディオ入力デバイス」（#394、#409、#425）。
//!
//! 先頭に映像デバイスの音声（`[audio] input_source = "video_pin"`）を置き、区切りの
//! 下に WASAPI の入力デバイスを並べる。先頭の項目名は**映像デバイスの表示名そのもの**
//! で、「DirectShow」「音声ピン」の語は利用者に見せない（#409）。項目名と選べるかの
//! 基準は**ドラフトの映像デバイス**で、「適用」の前でも選んだ映像デバイスの音声が出る
//! （#425）。選べるかどうかと選べない理由は `app` がドラフトとワーカーの観測値から作り、
//! `SettingsDialogView` の借用で受け取る。**描画の中でデバイスに問い合わせない**
//! （`docs/design/directshow-audio.md` の (5)）。映像を選び直しても音声の選択は変えない。

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
    /// `app` が判定に使った映像デバイスの名前（ドラフトの映像デバイス、無ければ
    /// 開いている映像。#425）。項目名はドラフトの `video.device_name` を先に使い、
    /// これは予備
    device: Option<String>,
}

impl Default for VideoPinChoice {
    /// 映像の観測値がまだ届いていないときは「映像デバイスが開いていない」と同じ扱い
    fn default() -> Self {
        Self::unavailable(Text::AudioPinVideoNotOpen.get().to_string(), None)
    }
}

impl VideoPinChoice {
    /// 選べる。`capabilities` は繋いでいる音声ピンの形式 1 つの対応設定
    pub fn selectable(capabilities: Option<AudioCapabilities>, device: Option<String>) -> Self {
        Self {
            availability: PinAvailability::Selectable(capabilities),
            device,
        }
    }

    /// 選べない。`reason` はホバーとコンボボックスの下に出す
    pub fn unavailable(reason: String, device: Option<String>) -> Self {
        Self {
            availability: PinAvailability::Unavailable(reason),
            device,
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
        video_pin_label(configured_device, self.device.as_deref())
    }
}

/// 映像デバイスの音声の項目名を決める（#409、#425）。
///
/// 設定（ドラフト）の映像デバイス名、無ければ `app` が渡した名前（開いている映像）を、
/// 「(DirectShow)」の印を外して出す。ドラフトを先にするのは、映像デバイスを選び
/// 直したフレームから項目名を合わせるため（`app` の判定は次のフレームで追いつく）。
/// どちらも無い（空の）ときは従来の文言（`Text::AudioInputVideoPin`）。
fn video_pin_label(configured_device: Option<&str>, device: Option<&str>) -> String {
    [configured_device, device]
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
                egui::Button::selectable(pin_selected, pin_label),
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
    use crate::audio::{AudioDirection, PinFormat, PinSampleType};
    use crate::ui::testing::{dialog_harness, DialogFixture};
    use crate::ui::{CapabilityEvent, SettingsEvent, SettingsTab};
    use eframe::egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};

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
    fn video_pin_label_prefers_the_draft_device_without_the_directshow_mark() {
        // ドラフトの映像デバイス名が先（#425）。「(DirectShow)」の印は外す
        assert_eq!(
            video_pin_label(Some("GC551 (DirectShow)"), Some("開いている別のデバイス")),
            "GC551"
        );
        // ドラフトに無ければ app が渡した名前（開いている映像）
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

    // ---- ウィジェットのテスト（egui_kittest、#419） ----
    //
    // 設定ダイアログを「デバイス設定」タブで描き、コンボボックスを開いて項目を押す。
    // 組み立ては `ui::testing`。描画はデバイスに問い合わせないので実機は要らない

    /// 「デバイス設定」タブを開いた設定ダイアログ。入力デバイスは 2 つ
    fn device_tab_harness(
        draft: &AppSettings,
        pin: VideoPinChoice,
    ) -> egui_kittest::Harness<'static, DialogFixture> {
        dialog_harness(DialogFixture::new(
            draft,
            SettingsTab::Device,
            pin,
            &["ライン入力", "マイク"],
        ))
    }

    /// 「オーディオ入力デバイス」のコンボボックスを開き、一覧の項目名を上から返す
    fn open_input_combo(harness: &mut egui_kittest::Harness<'_, DialogFixture>) -> Vec<String> {
        harness.get_by_label(Text::AudioInputDevice.get()).click();
        harness.run();
        // 一覧の項目は同じ親の下に並ぶ。先頭の項目の親から兄弟を順に読む
        let first = harness.get_by_label("ライン入力");
        let list = first.accesskit_node().parent().expect("一覧の親があるはず");
        list.children().filter_map(|node| node.label()).collect()
    }

    #[test]
    fn audio_input_combo_lists_the_video_device_first_and_selects_it() {
        // WASAPI の「ライン入力」を選んでいる状態から、映像デバイスの音声へ切り替える
        let mut draft = AppSettings::default();
        draft.audio.input_device_name = Some("ライン入力".to_string());
        let pin = VideoPinChoice::selectable(None, Some("GC551 (DirectShow)".to_string()));
        let mut harness = device_tab_harness(&draft, pin);

        // 先頭は映像デバイスの名前（「(DirectShow)」の印は外す）、その下に WASAPI
        assert_eq!(
            open_input_combo(&mut harness),
            ["GC551", "ライン入力", "マイク"]
        );

        harness.state_mut().events.clear();
        harness.get_by_label("GC551").click();
        harness.run();

        let fixture = harness.state();
        assert_eq!(
            fixture.draft().audio.input_source,
            AudioInputSource::VideoPin
        );
        // WASAPI へ戻したときのために入力デバイス名は残す
        assert_eq!(
            fixture.draft().audio.input_device_name.as_deref(),
            Some("ライン入力")
        );
        // 既定値の選び直しは手元の形式で済ませるので、届くのを待つ目印を立てない
        assert!(
            !fixture.events.iter().any(|event| matches!(
                event,
                SettingsEvent::Capability(CapabilityEvent::ExpectAudioDefaults(
                    AudioDirection::Input,
                    _
                ))
            )),
            "{:?}",
            fixture.events
        );

        // 切り替えたあとのフレームでは、入力側の対応設定をワーカーへ問い合わせない
        // （クリックは押下と離しで別のフレームになるので、押下のフレームの分は見ない）
        harness.state_mut().events.clear();
        harness.step();
        let events = &harness.state().events;
        assert!(!events.is_empty(), "描画が 1 フレーム走っているはず");
        assert!(
            !events.iter().any(|event| matches!(
                event,
                SettingsEvent::Capability(CapabilityEvent::RequestAudio(AudioDirection::Input, _))
            )),
            "{:?}",
            events
        );
    }

    /// `app` の `draft_pin_choice` の代わり。音声ピンがあるのは GC551 だけで、
    /// 開いている映像（USB Video）には無い。項目名にはドラフトの映像デバイスを渡す
    fn pin_for_draft(draft: &AppSettings) -> VideoPinChoice {
        let device = draft.video.device_name.clone();
        if device.as_deref() == Some("GC551 (DirectShow)") {
            VideoPinChoice::selectable(None, device)
        } else {
            VideoPinChoice::unavailable(Text::AudioPinMissing.get().to_string(), device)
        }
    }

    #[test]
    fn audio_input_combo_lists_the_draft_video_device_before_applying() {
        // 音声ピンの無い USB Video が開いている状態で、ドラフトの映像デバイスを
        // GC551 にすると、「適用」の前でも先頭が GC551 になり選べる（#425）
        let mut draft = AppSettings::default();
        draft.video.device_name = Some("USB Video".to_string());
        draft.audio.input_device_name = Some("ライン入力".to_string());
        let fixture = DialogFixture::new(
            &draft,
            SettingsTab::Device,
            pin_for_draft(&draft),
            &["ライン入力", "マイク"],
        )
        .with_video_devices(&["USB Video", "GC551 (DirectShow)"])
        .with_pin_for_draft(pin_for_draft);
        let mut harness = dialog_harness(fixture);

        // はじめは開いている USB Video。音声ピンが無いので選べない
        assert_eq!(
            open_input_combo(&mut harness),
            ["USB Video", "ライン入力", "マイク"]
        );
        assert!(harness
            .get_by_label("USB Video")
            .accesskit_node()
            .is_disabled());
        harness.key_press(egui::Key::Escape);
        harness.run();

        // ドラフトの映像デバイスを GC551 に選び直す
        harness.get_by_label(Text::VideoDevice.get()).click();
        harness.run();
        harness.get_by_label("GC551 (DirectShow)").click();
        harness.run();
        let unchanged = |harness: &egui_kittest::Harness<'_, DialogFixture>| {
            let audio = &harness.state().draft().audio;
            assert_eq!(audio.input_source, AudioInputSource::Device);
            assert_eq!(audio.input_device_name.as_deref(), Some("ライン入力"));
        };
        assert_eq!(
            harness.state().draft().video.device_name.as_deref(),
            Some("GC551 (DirectShow)")
        );
        // 映像を選び直しただけでは音声の選択は変わらない
        unchanged(&harness);

        // 一覧の先頭が GC551 になり、選べる。開いて見ただけでも選択は変わらない
        assert_eq!(
            open_input_combo(&mut harness),
            ["GC551", "ライン入力", "マイク"]
        );
        assert!(!harness.get_by_label("GC551").accesskit_node().is_disabled());
        unchanged(&harness);

        // 選んだときに初めて変わる
        harness.get_by_label("GC551").click();
        harness.run();
        let audio = &harness.state().draft().audio;
        assert_eq!(audio.input_source, AudioInputSource::VideoPin);
        assert_eq!(audio.input_device_name.as_deref(), Some("ライン入力"));
    }

    #[test]
    fn audio_input_combo_selecting_a_wasapi_device_requests_its_defaults() {
        // 映像デバイスの音声から WASAPI の「マイク」へ切り替える
        let mut draft = AppSettings::default();
        draft.audio.input_source = AudioInputSource::VideoPin;
        let pin = VideoPinChoice::selectable(None, Some("GC551".to_string()));
        let mut harness = device_tab_harness(&draft, pin);

        open_input_combo(&mut harness);
        harness.state_mut().events.clear();
        harness.get_by_label("マイク").click();
        harness.run();

        let fixture = harness.state();
        assert_eq!(fixture.draft().audio.input_source, AudioInputSource::Device);
        assert_eq!(
            fixture.draft().audio.input_device_name.as_deref(),
            Some("マイク")
        );
        // 切り替えたフレームで既定値の選び直しを頼み、対応設定を問い合わせる
        let expect = SettingsEvent::Capability(CapabilityEvent::ExpectAudioDefaults(
            AudioDirection::Input,
            "マイク".to_string(),
        ));
        let request = SettingsEvent::Capability(CapabilityEvent::RequestAudio(
            AudioDirection::Input,
            "マイク".to_string(),
        ));
        assert!(fixture.events.contains(&expect), "{:?}", fixture.events);
        assert!(fixture.events.contains(&request), "{:?}", fixture.events);
    }

    #[test]
    fn audio_input_combo_unavailable_pin_is_disabled_and_explains_why() {
        // 映像デバイスの音声が「無い」と分かっている。項目名は設定の映像デバイス名
        let mut draft = AppSettings::default();
        draft.video.device_name = Some("USB Video (DirectShow)".to_string());
        draft.audio.input_device_name = Some("ライン入力".to_string());
        let reason = "この映像デバイスには音声がありません";
        let pin = VideoPinChoice::unavailable(reason.to_string(), None);
        let mut harness = device_tab_harness(&draft, pin.clone());

        assert_eq!(
            open_input_combo(&mut harness),
            ["USB Video", "ライン入力", "マイク"]
        );
        assert!(harness
            .get_by_label("USB Video")
            .accesskit_node()
            .is_disabled());

        // 押しても選ばれない
        harness.get_by_label("USB Video").click();
        harness.run();
        assert_eq!(
            harness.state().draft().audio.input_source,
            AudioInputSource::Device
        );
        // 選ばれていないので、コンボボックスの下に理由は出ない
        let shows_reason = |harness: &egui_kittest::Harness<'_, DialogFixture>| -> bool {
            harness
                .query_by(|node| {
                    node.role() == Role::Label
                        && node.value().is_some_and(|value| value.contains(reason))
                })
                .is_some()
        };
        assert!(!shows_reason(&harness));

        // 選ばれたまま「無い」になった（映像デバイスを差し替えたなど）ときは、
        // 選ばれたまま表示し、下に理由を出す
        draft.audio.input_source = AudioInputSource::VideoPin;
        let harness = device_tab_harness(&draft, pin);
        assert_eq!(
            harness
                .get_by_label(Text::AudioInputDevice.get())
                .accesskit_node()
                .value()
                .as_deref(),
            Some("USB Video")
        );
        assert!(shows_reason(&harness));
    }
}
