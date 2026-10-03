//! 入力が未設定のまま起動したときの、入力の既定の決め方（#394）。
//!
//! 入力は最初の映像の試行のあとに決める。映像に音声ピンがあれば「映像デバイスの
//! 音声」、無ければ今までどおり WASAPI の列挙の先頭（`docs/design/directshow-audio.md`
//! の (4) の「初回の既定」）。決めるまでと、決めた値を UI が書き戻すまでの間は、
//! 音声を理由なしで待たせる。
//!
//! `super::worker_audio_connect` と同じく `WorkerState` に生やす形で、デバイス
//! ワーカースレッドの上でだけ走る。判定（音声ピンがあるか）は
//! `super::monitor_audio_pin::default_input_uses_pin`。

use super::monitor_audio_pin::default_input_uses_pin;
use super::worker::{AudioTarget, DeviceConfig, DeviceEvent};
use super::worker_audio_connect::audio_input_is_selected;
use super::worker_loop::WorkerState;
use crate::settings::AudioInputSource;
use log::{debug, info};

/// 入力が決まっていない設定か。WASAPI の入力で、デバイスが選ばれていない。
/// 「映像デバイスの音声」は入力デバイス名を使わないので、決まっている扱い。
pub(super) fn input_is_undecided(audio: &AudioTarget) -> bool {
    audio.5 == AudioInputSource::Device && !audio_input_is_selected(audio.0.as_deref())
}

/// 入力が未設定のまま起動したときの、入力の既定の決まり方（#394）。
///
/// 入力は最初の映像の試行のあとに決める（映像に音声ピンがあれば「映像デバイスの
/// 音声」、無ければ WASAPI の列挙の先頭）。決まるまでと、決めた値を UI が設定へ
/// 書き戻して送り直すまでの間は、音声は理由を出さずに待つ。「入力が選ばれていない」を
/// 出すと、初回起動のたびに一瞬トーストが出る（`docs/design/directshow-audio.md` の
/// (4) の「初回の既定」）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) enum DefaultInput {
    /// 決める必要が無い。設定に入力があるか、決めた値の書き戻しが届いた
    #[default]
    Settled,
    /// 最初の映像の試行を待っている
    AwaitingVideo,
    /// 決めて UI へ返した。書き戻した設定が届くまで、入力の決まっていない設定が
    /// 届いても理由を出さずに待つ。WASAPI の入力に決めたときはその名前を持ち、
    /// 届いた設定へ写して開いたままにする（往復を待たずに開くため）。
    /// 「映像デバイスの音声」に決めたときは `None` で、書き戻しを待って開く
    /// （ワーカーは UI から来た映像の接続対象を書き換えない）
    AwaitingWriteBack(Option<String>),
}

impl WorkerState {
    /// 起動直後の設定を受け取ったときと、以後の設定を受け取ったときに、初回の
    /// 入力の既定の状態を進める（#394）。`apply_config` が差分の判定より前に呼ぶ。
    ///
    /// - 起動直後で入力が決まっていなければ、最初の映像の試行を待つ
    /// - 入力の決まった設定が届いたら（書き戻し、または利用者が選んだ）終わる
    /// - WASAPI の入力に決めたあと、書き戻す前の設定（入力なし）が届いたら、決めた
    ///   名前を写す。写さないと差分ありとみなして音声を閉じ、理由まで出してしまう
    pub(super) fn track_default_input(&mut self, config: &mut DeviceConfig, initial: bool) {
        let undecided = input_is_undecided(&config.audio);
        if initial {
            self.default_input = if undecided {
                info!("入力デバイスが未設定なので、最初の映像の試行のあとに決める");
                DefaultInput::AwaitingVideo
            } else {
                DefaultInput::Settled
            };
            return;
        }
        if !undecided {
            self.default_input = DefaultInput::Settled;
            return;
        }
        if let DefaultInput::AwaitingWriteBack(Some(name)) = &self.default_input {
            config.audio.0 = Some(name.clone());
        }
    }

    /// 最初の映像の試行のあとに、入力の既定を決める（#394）。
    ///
    /// 映像に音声ピンがあれば「映像デバイスの音声」、無ければ WASAPI の列挙の先頭。
    /// 決めた値は `DefaultDevicesResolved` で UI へ返して書き戻してもらう。
    /// WASAPI に決めたときは往復を待たずにこの場の設定へ写して開く（今までの
    /// `resolve_default_devices` と同じ）。「映像デバイスの音声」に決めたときは
    /// 映像の接続対象（音声ピンを繋ぐか）も変わるが、**ワーカーは UI から来た
    /// 映像の接続対象を書き換えない。** 書き戻した設定が届いてから映像を開き直し、
    /// 音声はそのあとで開く（初回だけ映像がもう 1 度開き直る）。
    pub(super) fn settle_default_input(&mut self) {
        if self.default_input != DefaultInput::AwaitingVideo {
            return;
        }
        let Some(mut audio) = self.config.as_ref().map(|config| config.audio.clone()) else {
            return;
        };
        if default_input_uses_pin(self.video.active().as_ref()) {
            info!("映像デバイスに音声ピンがあるので、入力の既定を映像デバイスの音声にする");
            self.default_input = DefaultInput::AwaitingWriteBack(None);
            self.emit(DeviceEvent::DefaultDevicesResolved {
                video: None,
                input: None,
                input_source: Some(AudioInputSource::VideoPin),
            });
            return;
        }
        let list = self.audio.list_input_devices();
        debug!("利用できる入力デバイス: {:?}", list);
        let first = list.into_iter().next();
        match &first {
            Some(name) => info!("入力デバイスの既定を {} にした", name),
            // 1 台も無ければ今までどおり「入力が選ばれていない」で待つ
            None => info!("入力デバイスが 1 台も見つからないので、入力は未設定のまま"),
        }
        audio.0 = first.clone();
        if let Some(config) = self.config.as_mut() {
            config.audio = audio.clone();
        }
        self.default_input = match &first {
            Some(name) => DefaultInput::AwaitingWriteBack(Some(name.clone())),
            None => DefaultInput::Settled,
        };
        self.last_audio_target = None;
        self.audio_retry.request_now(audio);
        if first.is_some() {
            self.emit(DeviceEvent::DefaultDevicesResolved {
                video: None,
                input: first,
                input_source: Some(AudioInputSource::Device),
            });
        }
    }

    /// 初回の入力の既定を決めている間、音声を開かずに待つ（#394）。
    ///
    /// `hold_audio_without_input` と違い、理由は出さない。決まったら
    /// `settle_default_input` か、書き戻した設定の差分が要求を立て直す。
    pub(super) fn hold_audio_for_default_input(&mut self, config: &DeviceConfig) {
        debug!("入力の既定を決めているので、音声はまだ開かない");
        self.audio_retry.cancel();
        if self.audio.active().is_some() {
            self.audio.stop_capture();
        }
        self.last_audio_target = Some(config.audio.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::monitor_audio_pin::PinWait;
    use super::super::worker_loop::testing::{apply_config, config_for, drain, mock_state};
    use super::*;
    use crate::i18n;
    use std::time::{Duration, Instant};

    /// 入力の既定として返したもの `(入力, 入力の種類)` を、届いた順に取り出す。
    fn resolved_inputs(events: &[DeviceEvent]) -> Vec<(Option<String>, Option<AudioInputSource>)> {
        events
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::DefaultDevicesResolved {
                    input,
                    input_source,
                    ..
                } if input_source.is_some() => Some((input.clone(), *input_source)),
                _ => None,
            })
            .collect()
    }

    fn has_audio_failure(events: &[DeviceEvent]) -> bool {
        events
            .iter()
            .any(|event| matches!(event, DeviceEvent::AudioFailed(_)))
    }

    /// 設定ファイルが無い初回（映像も入力も未設定）のワーカー。映像は 1 台
    fn first_run(
        has_audio_pin: bool,
    ) -> (
        WorkerState,
        std::sync::mpsc::Receiver<DeviceEvent>,
        MockVideoBackend,
        MockAudioBackend,
    ) {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state.devices = vec![("キャプチャーボード".to_string(), String::new())];
            state.has_audio_pin = has_audio_pin;
        });
        audio.with(|state| state.input_devices = vec!["ライン入力".to_string()]);
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(&mut state, config_for(None, None), true);
        (state, events, video, audio)
    }

    #[test]
    fn first_run_picks_the_video_pin_and_opens_it_after_the_write_back() {
        let (mut state, events, video, audio) = first_run(true);
        let base = Instant::now();
        state.tick(base);
        let mut seen = drain(&events);
        assert_eq!(
            resolved_inputs(&seen),
            vec![(None, Some(AudioInputSource::VideoPin))]
        );
        // ワーカーは映像の接続対象を書き換えない。書き戻しが届くまで音声は開かずに待つ
        assert_eq!(
            video.with(|state| state.last_connect_audio_pin),
            Some(false)
        );
        assert_eq!(audio.with(|state| state.start_calls), 0);

        // UI が書き戻した設定（音声ピンを繋ぐ）を送り直す
        let mut written = state.config.clone().expect("設定を覚えていること");
        written.video.5 = true;
        written.audio.5 = AudioInputSource::VideoPin;
        apply_config(&mut state, written, false);
        // 映像は成功から 1 秒の下限が明けるまで開き直さない。その間に音声が先に
        // 試されても、開き直しを待っているだけなので理由は出さない
        state.tick(base + Duration::from_millis(500));
        assert_eq!(state.audio_pin_wait, Some(PinWait::NotConnected));
        state.tick(base + Duration::from_secs(2));
        assert_eq!(video.with(|state| state.last_connect_audio_pin), Some(true));
        state.tick(base + Duration::from_millis(2_100));
        assert_eq!(audio.with(|state| state.pin_graph), Some(2));
        assert!(audio.with(|state| state.running));
        seen.extend(drain(&events));
        assert!(!has_audio_failure(&seen), "{seen:?}");
    }

    #[test]
    fn first_run_picks_the_first_wasapi_input_without_a_pin() {
        let (mut state, events, _video, audio) = first_run(false);
        state.tick(Instant::now());
        let seen = drain(&events);
        assert_eq!(
            resolved_inputs(&seen),
            vec![(
                Some("ライン入力".to_string()),
                Some(AudioInputSource::Device)
            )]
        );
        // 往復を待たずに開く
        assert_eq!(audio.with(|state| state.start_calls), 1);
        assert!(!has_audio_failure(&seen), "{seen:?}");

        // 書き戻す前の設定（入力なし）が遅れて届いても、決めた名前を引き継いで閉じない
        let mut stale = state.config.clone().expect("設定を覚えていること");
        stale.audio.0 = None;
        apply_config(&mut state, stale, false);
        state.tick(Instant::now() + Duration::from_secs(2));
        assert_eq!(audio.with(|state| state.start_calls), 1);
        assert!(audio.with(|state| state.running));
        assert!(!has_audio_failure(&drain(&events)));
    }

    #[test]
    fn first_run_without_any_video_falls_back_to_wasapi() {
        // キャプチャーボードを挿していない初回は、今までどおり WASAPI の先頭
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        audio.with(|state| state.input_devices = vec!["ライン入力".to_string()]);
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(&mut state, config_for(None, None), true);
        state.tick(Instant::now());
        assert_eq!(
            resolved_inputs(&drain(&events)),
            vec![(
                Some("ライン入力".to_string()),
                Some(AudioInputSource::Device)
            )]
        );
        assert_eq!(
            state.default_input,
            DefaultInput::AwaitingWriteBack(Some("ライン入力".to_string()))
        );
    }

    #[test]
    fn reset_settings_after_startup_still_reports_the_missing_input() {
        // 設定の初期化で入力が未設定に戻ったときは、今までどおり理由を出して待つ（#304）
        let (mut state, events, _video, _audio) = first_run(false);
        let base = Instant::now();
        state.tick(base);
        let mut written = state.config.clone().expect("設定を覚えていること");
        apply_config(&mut state, written.clone(), false);
        assert_eq!(state.default_input, DefaultInput::Settled);
        drain(&events);

        written.audio.0 = None;
        apply_config(&mut state, written, false);
        state.tick(base + Duration::from_secs(2));
        let expected = i18n::Text::AudioInputNotSelected.get().to_string();
        assert!(drain(&events)
            .iter()
            .any(|event| matches!(event, DeviceEvent::AudioFailed(reason) if *reason == expected)));
    }
}
