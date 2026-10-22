//! デバイス能力の取得状況と、そこから作る選択肢まわりの表示。
//!
//! 取得状態の保持（`CapabilityCache`）は `capability.rs` の側。ここにあるのは
//! 「デバイス設定」タブが呼ぶ描画（取得中・失敗・既定の一覧の知らせと「再取得」）と、
//! 選択肢の注意書き・表示名の組み立て。

use crate::audio::{AudioCapabilities, AudioDirection, ChoiceSource};
use crate::i18n::{self, Text};
use crate::video::capabilities::is_assumed;
use eframe::egui;

use super::capability::{CapabilityState, VideoCapabilityCache, VideoCapabilityKey};
use super::{warning_label, AudioCapabilityCaches, CapabilityEvent, SettingsEvent};

/// チャンネル数の表示名。
pub(super) fn channel_label(channels: u16) -> String {
    match channels {
        1 => Text::ChannelMono.get().to_string(),
        2 => Text::ChannelStereo.get().to_string(),
        other => format!("{} ch", other),
    }
}

/// 現在の設定値が選択肢に無いときに出す注意書き。選択肢にあれば `None`。
///
/// **実際に使われる値を併記する。** 黙って別の値で開くと、設定画面の表示と
/// 「接続状態」タブの値が食い違う理由がユーザーに分からない。寄せ先は
/// `audio::select_best_config` と同じ「最も近い値」で、同点なら小さいほう。
///
/// `values` が空のときは何も出さない。選択肢を作れていない状況なので、
/// どの値へ寄るかをここで断定できない。
///
/// 先頭の記号は `warning_label` が付けるので、ここでは文言だけを返す。
pub fn out_of_range_note(values: &[u32], current: u32, unit: &str) -> Option<String> {
    if values.is_empty() || values.contains(&current) {
        return None;
    }
    let nearest = values.iter().copied().min_by_key(|v| v.abs_diff(current))?;
    Some(i18n::out_of_range_note(current, nearest, unit))
}

/// 選択肢の出どころに応じた説明を添える。共通部分から作れているときは何も出さない。
pub(super) fn show_choice_note(ui: &mut egui::Ui, source: ChoiceSource, label: &str) {
    match source {
        // 入出力の両方が対応する値だけが並んでいる。説明は要らない
        ChoiceSource::Common => {}
        ChoiceSource::OneSided => {
            ui.label(i18n::choice_note_one_sided(label));
        }
        ChoiceSource::Disjoint => {
            warning_label(ui, i18n::choice_note_disjoint(label));
        }
        ChoiceSource::Fallback => {
            ui.label(i18n::choice_note_fallback(label));
        }
    }
}

/// ビデオデバイスの能力の取得状況を描く。
///
/// 取得中はスピナー、失敗したら理由と「再取得」ボタン。取れたが既定の一覧
/// （どの形式の一覧もデバイスから取れなかった、`FormatCapability::assumed`、#446）
/// のときも、その旨と「再取得」ボタンを出す。黙って出すと、選択肢にデバイスが
/// 対応しない組み合わせが混ざる理由がユーザーに分からない。
pub(super) fn show_video_capability_progress(
    ui: &mut egui::Ui,
    capabilities: &VideoCapabilityCache,
    key: &VideoCapabilityKey,
    events: &mut Vec<SettingsEvent>,
) {
    let mut retry_requested = false;
    match capabilities.state(key) {
        Some(CapabilityState::Pending) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(Text::VideoCapabilityPending.get());
            });
        }
        Some(CapabilityState::Failed(reason)) => {
            // 理由はデバイス由来の長い文字列になることがある。ボタンと横に並べると
            // 折り返せずダイアログからはみ出すので、行を分ける
            warning_label(ui, i18n::video_capability_failed(reason));
            retry_requested = ui.button(Text::ButtonRetry.get()).clicked();
            ui.label(Text::VideoCapabilityFallback.get());
        }
        Some(CapabilityState::Ready(caps)) if is_assumed(caps) => {
            warning_label(ui, Text::VideoCapabilityAssumed.get());
            retry_requested = ui.button(Text::ButtonRetry.get()).clicked();
        }
        _ => {}
    }
    if retry_requested {
        events.push(SettingsEvent::Capability(CapabilityEvent::RetryVideo(
            key.clone(),
        )));
    }
}

/// オーディオデバイスの対応設定の取得状況を描く。
///
/// 取得中はスピナー、失敗したら理由と「再取得」ボタン。ビデオ側と同じ扱いで、
/// 黙って既定の一覧を出すと選択肢が実態と違う理由が分からない。
///
/// `input_key` が `None` なら入力側は描かない。入力が映像デバイスの音声ピンの
/// ときは、入力の対応設定をワーカーへ問い合わせないため（#394）。
pub(super) fn show_audio_capability_progress(
    ui: &mut egui::Ui,
    caches: &AudioCapabilityCaches<'_>,
    input_key: Option<&str>,
    output_key: &str,
    events: &mut Vec<SettingsEvent>,
) {
    let retry_input = input_key.filter(|key| {
        show_audio_capability_state(ui, caches.input.state(*key), AudioDirection::Input.label())
    });
    let retry_output = show_audio_capability_state(
        ui,
        caches.output.state(output_key),
        AudioDirection::Output.label(),
    );

    if let Some(input_key) = retry_input {
        events.push(SettingsEvent::Capability(CapabilityEvent::RetryAudio(
            AudioDirection::Input,
            input_key.to_string(),
        )));
    }
    if retry_output {
        events.push(SettingsEvent::Capability(CapabilityEvent::RetryAudio(
            AudioDirection::Output,
            output_key.to_string(),
        )));
    }
}

/// 片方向ぶんの取得状況を描く。「再取得」が押されたら `true`。
fn show_audio_capability_state(
    ui: &mut egui::Ui,
    state: Option<&CapabilityState<AudioCapabilities>>,
    label: &str,
) -> bool {
    let mut retry_requested = false;
    match state {
        Some(CapabilityState::Pending) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(i18n::audio_capability_pending(label));
            });
        }
        Some(CapabilityState::Failed(reason)) => {
            // ビデオ側と同じく、理由が長くなっても折り返せるよう行を分ける
            warning_label(ui, i18n::audio_capability_failed(label, reason));
            if ui.button(Text::ButtonRetry.get()).clicked() {
                retry_requested = true;
            }
        }
        _ => {}
    }
    retry_requested
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::settings::VideoBackendSetting;
    use crate::video::capabilities::FormatCapability;
    use crate::video::{DeviceCapabilities, VideoMode};
    use egui_kittest::kittest::Queryable;

    /// 取得できたことにする能力。中身そのものは検証の対象ではないので最小限
    fn sample_capabilities() -> DeviceCapabilities {
        vec![
            FormatCapability::new(
                "MJPEG",
                vec![
                    VideoMode::new(1920, 1080, 30),
                    VideoMode::new(1280, 720, 60),
                ],
            ),
            FormatCapability::new("YUY2", vec![VideoMode::new(1280, 720, 60)]),
        ]
    }

    /// 自動で開く設定のキー
    fn key(device: &str) -> VideoCapabilityKey {
        VideoCapabilityKey::new(Some(device), VideoBackendSetting::Auto)
    }

    #[test]
    fn out_of_range_note_is_none_when_the_value_is_selectable() {
        assert_eq!(out_of_range_note(&[44100, 48000], 48000, " Hz"), None);
    }

    #[test]
    fn out_of_range_note_names_the_value_actually_used() {
        // 設定ファイルを手で書き換えた場合など、選択肢に無い値が残ることがある。
        // 何で開かれるかを併記しないと、接続状態タブとの食い違いが分からない
        let note = out_of_range_note(&[32000, 48000], 44100, " Hz").expect("注意書きが要る");

        assert!(note.contains("44100 Hz"), "{note}");
        assert!(note.contains("48000 Hz"), "{note}");
    }

    #[test]
    fn out_of_range_note_tie_picks_the_smaller_value() {
        // audio::nearest_sample_rate と同じ寄せ方でないと、実際に開く値と食い違う
        let note = out_of_range_note(&[32000, 48000], 40000, " Hz").expect("注意書きが要る");

        assert!(note.contains("32000 Hz"), "{note}");
    }

    #[test]
    fn out_of_range_note_empty_choices_returns_none() {
        // 選択肢を作れていない状況では、どの値へ寄るかを断定できない
        assert_eq!(out_of_range_note(&[], 44100, " Hz"), None);
    }

    #[test]
    fn channel_label_names_mono_and_stereo() {
        assert_eq!(channel_label(1), "1（モノラル）");
        assert_eq!(channel_label(2), "2（ステレオ）");
        assert_eq!(channel_label(6), "6 ch");
    }
    /// 「デバイス設定」タブを開き、「Capture Device」の能力に `caps` が届いた設定ダイアログ
    fn device_tab_with_capabilities(
        caps: DeviceCapabilities,
    ) -> egui_kittest::Harness<'static, crate::ui::testing::DialogFixture> {
        use crate::settings::AppSettings;
        use crate::ui::testing::{dialog_harness, DialogFixture};
        use crate::ui::{SettingsTab, VideoPinChoice};

        let mut draft = AppSettings::default();
        draft.video.device_name = Some("Capture Device".to_string());
        let mut fixture = DialogFixture::new(
            &draft,
            SettingsTab::Device,
            VideoPinChoice::selectable(None, None),
            &[],
        )
        .with_video_devices(&["Capture Device"]);
        fixture
            .state
            .capabilities_mut()
            .apply_result(key("Capture Device"), Ok(caps));
        let mut harness = dialog_harness(fixture);
        harness.run();
        harness
    }

    #[test]
    fn device_tab_notes_an_assumed_list_and_offers_retry() {
        // どの形式の一覧も取れず、既定の一覧が届いた（#446）
        let assumed = vec![FormatCapability {
            assumed: true,
            ..FormatCapability::new("YUY2", vec![VideoMode::new(1280, 720, 60)])
        }];
        let mut harness = device_tab_with_capabilities(assumed);

        assert!(harness
            .query_by_label_contains(Text::VideoCapabilityAssumed.get())
            .is_some());

        harness.get_by_label(Text::ButtonRetry.get()).click();
        harness.run();
        assert!(harness.state().events.iter().any(|event| matches!(
            event,
            SettingsEvent::Capability(CapabilityEvent::RetryVideo(retried))
                if *retried == key("Capture Device")
        )));
    }

    #[test]
    fn device_tab_does_not_note_a_list_from_the_device() {
        let harness = device_tab_with_capabilities(sample_capabilities());

        assert!(harness
            .query_by_label_contains(Text::VideoCapabilityAssumed.get())
            .is_none());
        assert!(harness.query_by_label(Text::ButtonRetry.get()).is_none());
    }
}
