//! ワーカーが行うデバイス操作のうち音声側。音声を開く（`try_connect_audio`）、
//! 入力が未指定のときに開かずに待つ、映像の復帰に合わせた開き直し、
//! 対応設定の問い合わせ。
//!
//! `super::worker_connect` と同じく `WorkerState` に生やす形で、デバイス
//! ワーカースレッドの上でだけ走る。映像側と列挙のログ、「Windows 側にも
//! 見えていない」の判定の適用は `super::worker_connect` に置く。

use super::monitor::{decide_audio_fallback, should_resync_audio_after_video, AudioFallbackAction};
use super::retry::backoff_delay;
use super::worker::{DeviceConfig, DeviceEvent};
use super::worker_connect::failure_message;
use super::worker_loop::WorkerState;
use crate::audio::{self, AudioDirection, AudioError};
use crate::i18n;
use log::{debug, info, warn};
use std::time::Instant;

/// 音声を開いてよいだけの入力デバイスが設定に書かれているか（#304）。
///
/// **未指定の入力を「Windows の既定の入力」の意味に取らない。** 既定の入力は
/// 環境依存で、ノート PC ならほぼ確実に内蔵マイクになり、パススルーがその音を
/// スピーカーへ流す（#134 / #165 と同じ症状）。未指定のまま届くのは設定の
/// 初期化・読み込みと、起動時に入力が 1 台も列挙できなかったとき。
/// 空文字も名前として扱わない（手で書き換えた設定ファイルで起こりうる）。
pub(super) fn audio_input_is_selected(input: Option<&str>) -> bool {
    input.is_some_and(|name| !name.is_empty())
}

/// 音声を開けなかった理由が、入力と出力のどちらで起きたか（#323）。
///
/// 接続に失敗したとき、取得済みの対応設定をこの向きの分だけ捨てるために使う。
/// 今ある失敗はどれも向きを持つ。**向きを持たない失敗を足すときは、呼び出し側で
/// 両方を捨てる扱いにすること。** どちらの一覧が古いのか分からないまま片方だけ
/// 残すと、挿し直した側の古い一覧で失敗し続ける。
pub(super) fn failed_direction(error: &AudioError) -> AudioDirection {
    match error {
        AudioError::DeviceEnumerationFailed { direction, .. }
        | AudioError::DeviceNotFound { direction, .. }
        | AudioError::NoDefaultDevice(direction)
        | AudioError::DefaultConfigFailed { direction, .. }
        | AudioError::SupportedConfigsFailed { direction, .. }
        | AudioError::UnsupportedSampleFormat { direction, .. }
        | AudioError::StreamBuildFailed { direction, .. }
        | AudioError::StreamPlayFailed { direction, .. } => *direction,
    }
}

impl WorkerState {
    /// 映像が途絶から復帰したときに、音声の再接続も要求する。
    pub(super) fn resync_audio_after_video_recovery(
        &mut self,
        config: &DeviceConfig,
        recovered: bool,
    ) {
        if !should_resync_audio_after_video(
            recovered,
            self.audio.active().is_some(),
            self.audio_retry.is_active(),
        ) {
            if recovered {
                debug!("音声は繋がっているので、映像の復帰にあわせた開き直しはしない");
            }
            return;
        }
        self.last_audio_target = None;
        self.audio_retry.request_now(config.audio.clone());
        info!("映像が戻ったので、音声デバイスの再接続も要求した");
    }

    /// 音声デバイスへの接続を 1 回だけ試す。
    ///
    /// **開けなくても、別のデバイスへは倒さない。** 失敗が続いたときの扱いは
    /// `monitor::decide_audio_fallback` を参照。
    pub(super) fn try_connect_audio(&mut self, config: &DeviceConfig, now: Instant) {
        let (input_device_name, output_device_name, sample_rate, channels, buffer_ms) =
            config.audio.clone();
        if !audio_input_is_selected(input_device_name.as_deref()) {
            self.hold_audio_without_input(config);
            return;
        }
        let attempt = self.audio_retry.attempts() + 1;
        info!(
            "音声デバイスへの接続を試す（{} 回目）- 入力: {:?}、出力: {:?}、バッファ: {} ms",
            attempt, input_device_name, output_device_name, buffer_ms
        );

        // デバイスの列挙は実測で 300ms 前後かかる。設定値との突き合わせに要るのは
        // 最初の 1 回だけなので、再試行のたびには出さない
        if attempt == 1 {
            debug!(
                "利用できる入力デバイス: {:?}",
                self.audio.list_input_devices()
            );
            debug!(
                "利用できる出力デバイス: {:?}",
                self.audio.list_output_devices()
            );
        }

        // 対応設定は `start_passthrough` が要る。**無ければここで取りに行く。**
        // 以前は UI スレッドで開いていたため、届くまで接続を見送る仕組みを
        // 持っていた。このスレッドは止まってよいので、素直に待てばよい
        let input_key = audio::cache_key(input_device_name.as_deref());
        let output_key = audio::cache_key(output_device_name.as_deref());
        self.ensure_audio_capabilities(AudioDirection::Input, &input_key);
        self.ensure_audio_capabilities(AudioDirection::Output, &output_key);

        let result = self.audio.start_passthrough(&audio::PassthroughRequest {
            input_device_name: input_device_name.as_deref(),
            output_device_name: output_device_name.as_deref(),
            sample_rate,
            channels,
            input_capabilities: self
                .audio_capabilities
                .get(&(AudioDirection::Input, input_key.clone())),
            output_capabilities: self
                .audio_capabilities
                .get(&(AudioDirection::Output, output_key.clone())),
            buffer_ms,
        });

        match result {
            Ok(()) => {
                info!("音声デバイスに接続した");
                self.audio_retry.record_success(now);
                self.audio_not_visible = None;
                self.last_audio_failure = None;
                // **新しいストリームが開けたので、保留していたエラーは要らない。**
                // エラーは古いストリームのもので、旗は開き直しで新しい `Arc` に
                // 替わっている。残すと、設定変更や既定デバイスの追従で正常に
                // 開いたストリームを、下限が明けた回に閉じて開き直してしまう（#310）。
                // 落とすのはここ（開けたとき）だけで、見送っている間は落とさない
                self.audio_stream_error_pending = false;
                // 形を緩めて繋がった場合も、設定に書かれている値を記録する。
                // ここで実際に開いた値を入れると、設定のレートやチャンネル数へ
                // 戻せるようになっても差分が立たず、緩めたままになる
                self.last_audio_target = Some(config.audio.clone());
                self.emit(DeviceEvent::AudioConnected);
            }
            Err(e) => {
                warn!("音声デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                // 失敗が続いていることを 1 度だけ記録する。倒す先が無いので、
                // ここで開く相手が変わることはない
                if decide_audio_fallback(attempt) == AudioFallbackAction::WarnAndRetry {
                    warn!(
                        "音声デバイスに {} 回続けて接続できない。既定のデバイスへは倒さず、戻るまで再試行を続ける",
                        attempt
                    );
                }
                self.audio_retry.record_failure(now);
                // 映像と同じく、開く前に古いストリームを閉じているので記録も消す（#311）
                self.last_audio_target = None;
                // 映像と同じく、UI へは日本語の 1 行に落として渡す
                let reason = e.to_string();
                self.emit(DeviceEvent::AudioFailed(failure_message(
                    self.audio_not_visible.as_ref(),
                    &reason,
                )));
                self.last_audio_failure = Some(reason);
                // **取得済みの対応設定を捨てて取り直す。** デバイスが挿し直された
                // 場合、古い一覧でしか開けない設定を選び続けて失敗が繰り返される。
                // 捨てるのは失敗した向きだけ（#323）。問い合わせの間はワーカーが
                // 止まるので、正常な側まで毎回取り直すと映像の監視も待たされる
                let stale = failed_direction(&e);
                let key = match stale {
                    AudioDirection::Input => input_key,
                    AudioDirection::Output => output_key,
                };
                self.audio_capabilities.remove(&(stale, key));
                debug!(
                    "音声デバイスへの再試行は {} ms 後",
                    backoff_delay(self.audio_retry.attempts()).as_millis()
                );
            }
        }
    }

    /// 入力デバイスが選ばれていないので、音声を開かずに待つ（#304）。
    ///
    /// 開いているパススルーは閉じる。設定が「入力なし」になったのに前の入力の
    /// 音を流し続けると、画面の表示（未選択）と実際の音が食い違う。
    /// 再試行はしない。繋ぐ相手が決まるのはユーザーが入力を選んだときで、
    /// そのときは設定が変わるので `apply_config` の差分判定で要求が立つ。
    fn hold_audio_without_input(&mut self, config: &DeviceConfig) {
        info!("入力デバイスが未設定なので音声を開かない（Windows の既定の入力へは倒さない）");
        self.audio_retry.cancel();
        if self.audio.active().is_some() {
            self.audio.stop_capture();
        }
        // 同じ設定が 2 秒ごとに届くたびに要求を立て直して通知を繰り返さないよう、
        // この設定は扱い済みとして記録する
        self.last_audio_target = Some(config.audio.clone());
        self.audio_not_visible = None;
        let reason = i18n::Text::AudioInputNotSelected.get().to_string();
        self.last_audio_failure = Some(reason.clone());
        self.emit(DeviceEvent::AudioFailed(reason));
    }

    /// 対応設定が手元に無ければ問い合わせる。
    pub(super) fn ensure_audio_capabilities(&mut self, direction: AudioDirection, key: &str) {
        if self
            .audio_capabilities
            .contains_key(&(direction, key.to_string()))
        {
            return;
        }
        self.query_audio_capabilities(direction, key);
    }

    /// 音声デバイスの対応設定を問い合わせる。
    ///
    /// 成功した分だけワーカー側にも控えておく。`start_passthrough` が
    /// 一覧を要るためで、渡さないとその場で列挙し直すことになる。
    pub(super) fn query_audio_capabilities(&mut self, direction: AudioDirection, key: &str) {
        let started = Instant::now();
        let result = self
            .audio
            .capabilities(direction, audio::device_name_from_key(key));
        match &result {
            Ok(caps) => {
                info!(
                    "{}デバイスの対応設定を取得した: {}（{} 件、{} ms）",
                    direction.label(),
                    key,
                    caps.configs().len(),
                    started.elapsed().as_millis()
                );
                self.audio_capabilities
                    .insert((direction, key.to_string()), caps.clone());
            }
            Err(e) => warn!(
                "{}デバイスの対応設定を取得できない: {}: {}",
                direction.label(),
                key,
                e
            ),
        }
        self.emit(DeviceEvent::AudioCapabilities(
            direction,
            key.to_string(),
            // 能力キャッシュは理由を画面に出すだけなので、日本語の 1 行へ落とす
            Box::new(result.map_err(|e| e.to_string())),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::worker_loop::testing::{apply_config, config_for, mock_state};
    use super::*;
    use std::time::Duration;

    #[test]
    fn worker_reopens_the_previous_audio_right_after_a_failed_switch() {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, _events) = mock_state(&video, &audio);
        let a = config_for(None, Some("入力 A"));
        apply_config(&mut state, a.clone(), true);
        let base = Instant::now();
        state.tick(base);
        assert_eq!(audio.with(|state| state.start_calls), 1);

        audio.with(|state| state.failures_before_success = 3);
        apply_config(&mut state, config_for(None, Some("入力 B")), false);
        for step in 2..5 {
            state.tick(base + Duration::from_secs(step));
        }
        assert_eq!(audio.with(|state| state.start_calls), 4);

        apply_config(&mut state, a, false);
        state.tick(base + Duration::from_millis(4_100));
        assert_eq!(audio.with(|state| state.start_calls), 5);
        assert!(audio.with(|state| state.running));
    }

    /// 1 回失敗させてから繋がるまでに、対応設定を問い合わせた向きの列。
    fn capability_queries_across_one_failure(failure: AudioDirection) -> Vec<AudioDirection> {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        audio.with(|state| {
            state.capabilities_ok = true;
            state.failures_before_success = 1;
            state.failure_direction = Some(failure);
        });
        let (mut state, _events) = mock_state(&video, &audio);
        apply_config(&mut state, config_for(None, Some("入力 A")), true);
        let base = Instant::now();
        state.tick(base);
        // バックオフより十分に後
        state.tick(base + Duration::from_secs(60));
        assert_eq!(audio.with(|state| state.start_calls), 2);
        assert!(audio.with(|state| state.running));
        audio.with(|state| state.capability_queries.clone())
    }

    #[test]
    fn worker_requeries_only_the_input_side_after_an_input_failure() {
        assert_eq!(
            capability_queries_across_one_failure(AudioDirection::Input),
            vec![
                AudioDirection::Input,
                AudioDirection::Output,
                AudioDirection::Input
            ]
        );
    }

    #[test]
    fn worker_requeries_only_the_output_side_after_an_output_failure() {
        assert_eq!(
            capability_queries_across_one_failure(AudioDirection::Output),
            vec![
                AudioDirection::Input,
                AudioDirection::Output,
                AudioDirection::Output
            ]
        );
    }

    #[test]
    fn failed_direction_follows_the_error() {
        let cases = [
            (
                AudioError::DeviceNotFound {
                    direction: AudioDirection::Input,
                    name: "キャプチャーボード".to_string(),
                },
                AudioDirection::Input,
            ),
            (
                AudioError::NoDefaultDevice(AudioDirection::Output),
                AudioDirection::Output,
            ),
            (
                AudioError::StreamBuildFailed {
                    direction: AudioDirection::Output,
                    source: "失敗".to_string(),
                },
                AudioDirection::Output,
            ),
            (
                AudioError::StreamPlayFailed {
                    direction: AudioDirection::Input,
                    source: "失敗".to_string(),
                },
                AudioDirection::Input,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(failed_direction(&error), expected, "{error:?}");
        }
    }

    #[test]
    fn audio_input_is_selected_only_with_a_name() {
        assert!(audio_input_is_selected(Some("キャプチャーボード")));
        assert!(!audio_input_is_selected(None));
        assert!(!audio_input_is_selected(Some("")));
    }
}
