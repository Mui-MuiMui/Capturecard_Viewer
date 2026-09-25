//! デバイス能力のキャッシュと、そこから作る選択肢まわりの表示。
//!
//! 取得はデバイスを開く重い処理なので、描画スレッドでは行わずデバイス
//! ワーカーへ投げる。ここにあるのは進行状況の保持と、取得できた値を
//! 選択肢として出すときの注意書き。

use crate::audio::{AudioCapabilities, AudioDirection, ChoiceSource};
use crate::i18n::{self, Text};
use crate::video::DeviceCapabilities;
use eframe::egui;
use std::collections::HashMap;

use super::{warning_label, AudioCapabilityCaches, CapabilityEvent, SettingsEvent};

/// デバイス能力の取得状態。
///
/// 取得はデバイスを開いて対応表を引く重い処理なので、描画スレッドでは行わず
/// デバイスワーカーへ投げる。ダイアログは進行状況をこの型で受け取って描き分ける。
///
/// 型引数はビデオ（`DeviceCapabilities`）とオーディオ（`AudioCapabilities`）で
/// 中身が違うため。取得と受け渡しの手順は同じなので、キャッシュは共有する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityState<T> {
    /// 取得を要求済みで、結果を待っている
    Pending,
    /// 取得できた
    Ready(T),
    /// 取得に失敗した。文字列は画面に出す理由
    Failed(String),
}

/// ビデオデバイスの能力キャッシュ。
pub type VideoCapabilityCache = CapabilityCache<DeviceCapabilities>;
/// オーディオデバイスの能力キャッシュ。入力と出力で別に持つ。
pub type AudioCapabilityCache = CapabilityCache<AudioCapabilities>;

/// デバイス能力のキャッシュと、まだワーカーへ渡していない取得要求。
///
/// 触るのは UI スレッド（`CaptureCardViewer`）だけなのでロックを持たない。
/// 実際の取得は `CaptureCardViewer::dispatch_capability_requests` が
/// デバイスワーカーへコマンドとして流し、結果はイベント経由で `apply_result`
/// に入る。**これは設定ダイアログの選択肢のためのキャッシュで、音声を開く
/// ときに使う一覧はワーカーが別に持っている。**
pub struct CapabilityCache<T> {
    /// デバイス名 → 取得状態
    states: HashMap<String, CapabilityState<T>>,
    /// まだワーカーへ渡していないデバイス名
    requests: Vec<String>,
    /// デバイスを切り替えた直後で、能力が届いたら選択肢の既定値を
    /// 選び直す対象のデバイス名
    awaiting_defaults: Option<String>,
}

// `#[derive(Default)]` は `T: Default` を要求してしまう。キャッシュの中身は
// 空の HashMap なので、`T` に条件を付けずに実装する
impl<T> Default for CapabilityCache<T> {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            requests: Vec::new(),
            awaiting_defaults: None,
        }
    }
}

impl<T> CapabilityCache<T> {
    /// まだ一度も問い合わせていないデバイスなら、取得を要求して `Pending` にする。
    ///
    /// 既に `Pending` / `Ready` / `Failed` のいずれかなら何もしない。描画のたびに
    /// 呼ばれるため、ここで弾かないと同じデバイスを毎フレーム開きに行く。失敗した
    /// デバイスを問い合わせ直すのは `retry` の仕事。
    ///
    /// 要求を積んだときだけ `true` を返す。
    pub fn request(&mut self, device: &str) -> bool {
        if device.is_empty() || self.states.contains_key(device) {
            return false;
        }
        self.states
            .insert(device.to_string(), CapabilityState::Pending);
        self.requests.push(device.to_string());
        true
    }

    /// 取得済み・失敗済みを問わず問い合わせ直す。「再取得」ボタン用。
    ///
    /// 結果待ちの間に押されても投げ直さない。投げ直すと、先に飛ばした取得が
    /// あとから届いて新しい結果を上書きする。
    pub fn retry(&mut self, device: &str) -> bool {
        if device.is_empty() || self.is_pending(device) {
            return false;
        }
        self.states.remove(device);
        self.request(device)
    }

    /// 溜まっている取得要求を取り出す。呼び出し側がワーカーへ渡す。
    pub fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.requests)
    }

    /// ワーカーから届いた結果を反映する。
    pub fn apply_result(&mut self, device: String, result: Result<T, String>) {
        let state = match result {
            Ok(caps) => CapabilityState::Ready(caps),
            Err(reason) => CapabilityState::Failed(reason),
        };
        self.states.insert(device, state);
    }

    /// 取得状態。まだ要求もしていなければ `None`。
    pub fn state(&self, device: &str) -> Option<&CapabilityState<T>> {
        self.states.get(device)
    }

    /// 取得できた能力。結果待ち・失敗・未要求はいずれも `None` になる。
    pub fn ready(&self, device: &str) -> Option<&T> {
        match self.states.get(device) {
            Some(CapabilityState::Ready(caps)) => Some(caps),
            _ => None,
        }
    }

    /// 結果待ちか。**まだ要求していない場合は `false`。**
    ///
    /// 未要求を `true` にすると、要求を積む経路が無い状態で永久に待ってしまう。
    pub fn is_pending(&self, device: &str) -> bool {
        matches!(self.states.get(device), Some(CapabilityState::Pending))
    }

    /// デバイスが切り替わったことを記録する。能力が届いた時点でフォーマットの
    /// 既定値を選び直させるための目印。
    pub fn expect_defaults(&mut self, device: &str) {
        self.awaiting_defaults = Some(device.to_string());
    }

    /// `device` の能力が届いていて、切り替え直後の選び直しがまだなら `true`。
    ///
    /// **目印は消さない。** 描画中に読むため `&self` で済ませ、消すのは
    /// `CapabilityEvent::ClearVideoDefaults` を受けた `app` の仕事にしてある。
    /// 消さずに放っておくと、ユーザーが選び直したフォーマットを毎フレーム
    /// 先頭へ戻してしまうので、読んだ側は必ず消す要求を返すこと。
    pub fn awaits_defaults(&self, device: &str) -> bool {
        if self.awaiting_defaults.as_deref() != Some(device) {
            return false;
        }
        matches!(self.states.get(device), Some(CapabilityState::Ready(_)))
    }

    /// 既定値の選び直しの目印を落とす。`device` が目印の相手でなければ何もしない。
    ///
    /// 相手を確かめるのは、読んだときと消すときの間にユーザーがもう一度
    /// デバイスを切り替えた場合に、新しい目印まで巻き添えで消さないため。
    pub fn clear_awaiting_defaults(&mut self, device: &str) {
        if self.awaiting_defaults.as_deref() == Some(device) {
            self.awaiting_defaults = None;
        }
    }
}

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

/// オーディオデバイスの対応設定の取得状況を描く。
///
/// 取得中はスピナー、失敗したら理由と「再取得」ボタン。ビデオ側と同じ扱いで、
/// 黙って既定の一覧を出すと選択肢が実態と違う理由が分からない。
pub(super) fn show_audio_capability_progress(
    ui: &mut egui::Ui,
    caches: &AudioCapabilityCaches<'_>,
    input_key: &str,
    output_key: &str,
    events: &mut Vec<SettingsEvent>,
) {
    let retry_input = show_audio_capability_state(
        ui,
        caches.input.state(input_key),
        AudioDirection::Input.label(),
    );
    let retry_output = show_audio_capability_state(
        ui,
        caches.output.state(output_key),
        AudioDirection::Output.label(),
    );

    if retry_input {
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

    use crate::video::capabilities::FormatCapability;
    use crate::video::{DeviceCapabilities, VideoMode};

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

    #[test]
    fn capability_cache_holds_audio_capabilities_too() {
        // 型引数を変えただけで同じキャッシュが使えること
        let mut cache = AudioCapabilityCache::default();

        assert!(cache.request(crate::audio::DEFAULT_DEVICE_KEY));
        assert!(cache.is_pending(crate::audio::DEFAULT_DEVICE_KEY));
        assert!(cache.ready(crate::audio::DEFAULT_DEVICE_KEY).is_none());

        cache.apply_result(
            crate::audio::DEFAULT_DEVICE_KEY.to_string(),
            Err("デバイスがありません".to_string()),
        );

        assert!(!cache.is_pending(crate::audio::DEFAULT_DEVICE_KEY));
    }

    #[test]
    fn capability_cache_is_pending_is_false_for_an_unrequested_device() {
        // 未要求を「待ち」と見なすと、音声の接続が永久に待ってしまう
        let cache = VideoCapabilityCache::default();

        assert!(!cache.is_pending("Capture Device"));
    }

    #[test]
    fn capability_cache_request_new_device_marks_pending_and_queues() {
        let mut cache = VideoCapabilityCache::default();

        assert!(cache.request("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
        assert_eq!(cache.take_requests(), vec!["Capture Device".to_string()]);
    }

    #[test]
    fn capability_cache_request_twice_queues_only_once() {
        // 描画のたびに呼ばれるので、二重に投げるとデバイスを何度も開きに行く
        let mut cache = VideoCapabilityCache::default();

        assert!(cache.request("Capture Device"));
        assert!(!cache.request("Capture Device"));
        assert_eq!(cache.take_requests().len(), 1);
    }

    #[test]
    fn capability_cache_request_empty_device_name_is_ignored() {
        // デバイス未選択のとき。空の名前で問い合わせても意味がない
        let mut cache = VideoCapabilityCache::default();

        assert!(!cache.request(""));
        assert_eq!(cache.state(""), None);
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_request_after_failure_does_not_queue_again() {
        // 失敗したデバイスを毎フレーム開きに行かない。投げ直すのは「再取得」だけ
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(!cache.request("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_take_requests_empties_the_queue() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("A");
        cache.request("B");

        assert_eq!(
            cache.take_requests(),
            vec!["A".to_string(), "B".to_string()]
        );
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_apply_result_ok_becomes_ready() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert_eq!(cache.ready("Capture Device"), Some(&sample_capabilities()));
    }

    #[test]
    fn capability_cache_apply_result_err_becomes_failed_with_reason() {
        // 理由は画面に出すので、握り潰さず保持する
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        cache.apply_result(
            "Capture Device".to_string(),
            Err("Device 'Capture Device' not found".to_string()),
        );

        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Failed(
                "Device 'Capture Device' not found".to_string()
            ))
        );
        assert_eq!(cache.ready("Capture Device"), None);
    }

    #[test]
    fn capability_cache_ready_is_none_while_pending() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");

        assert_eq!(cache.ready("Capture Device"), None);
    }

    #[test]
    fn capability_cache_retry_after_failure_queues_again() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(cache.retry("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
        assert_eq!(cache.take_requests(), vec!["Capture Device".to_string()]);
    }

    #[test]
    fn capability_cache_retry_while_pending_does_not_queue() {
        // 投げ直すと、先の取得があとから届いて新しい結果を上書きする
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();

        assert!(!cache.retry("Capture Device"));
        assert!(cache.take_requests().is_empty());
    }

    #[test]
    fn capability_cache_retry_after_success_queues_again() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.retry("Capture Device"));
        assert_eq!(
            cache.state("Capture Device"),
            Some(&CapabilityState::Pending)
        );
    }

    #[test]
    fn capability_cache_awaits_defaults_is_true_after_result_arrives() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.awaits_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_awaits_defaults_stays_true_until_cleared() {
        // 読むだけでは消えない。描画は何度でも読めるが、消す要求を返さないと
        // ユーザーが選び直したフォーマットを毎フレーム戻してしまう
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Ok(sample_capabilities()));

        assert!(cache.awaits_defaults("Capture Device"));
        assert!(cache.awaits_defaults("Capture Device"));

        cache.clear_awaiting_defaults("Capture Device");

        assert!(!cache.awaits_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_clear_awaiting_defaults_ignores_another_device() {
        // 読んでから消すまでの間にもう一度切り替えた場合。古いデバイスに
        // 対する消去で、新しい目印まで落とさない
        let mut cache = VideoCapabilityCache::default();
        cache.request("A");
        cache.request("B");
        cache.take_requests();
        cache.apply_result("B".to_string(), Ok(sample_capabilities()));
        cache.expect_defaults("B");

        cache.clear_awaiting_defaults("A");

        assert!(cache.awaits_defaults("B"));
    }

    #[test]
    fn capability_cache_awaits_defaults_is_false_while_pending() {
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.expect_defaults("Capture Device");

        assert!(!cache.awaits_defaults("Capture Device"));
    }

    #[test]
    fn capability_cache_awaits_defaults_is_false_for_another_device() {
        // 取得を待っている間にもう一度切り替えた場合。先に届いた別デバイスの
        // 能力で選択を書き換えない
        let mut cache = VideoCapabilityCache::default();
        cache.request("A");
        cache.request("B");
        cache.take_requests();
        cache.expect_defaults("B");
        cache.apply_result("A".to_string(), Ok(sample_capabilities()));

        assert!(!cache.awaits_defaults("A"));
    }

    #[test]
    fn capability_cache_awaits_defaults_is_false_when_failed() {
        // 失敗したときは選択を書き換えない。既定の選択肢のまま残す
        let mut cache = VideoCapabilityCache::default();
        cache.request("Capture Device");
        cache.take_requests();
        cache.expect_defaults("Capture Device");
        cache.apply_result("Capture Device".to_string(), Err("開けません".to_string()));

        assert!(!cache.awaits_defaults("Capture Device"));
    }
}
