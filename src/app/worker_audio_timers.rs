//! タイマーで駆動する監視のうち音声まわり。音声ストリームのエラー、
//! Windows の既定デバイスの切り替え、クロックドリフト補正。
//!
//! `super::worker_timers` の `tick` から呼ばれ、デバイスワーカースレッドの
//! 上でだけ走る。`WorkerState` に生やす形にしてあるのは `super::worker_timers`
//! と同じ理由で、状態を 1 つに保ったまま役割ごとにファイルを分けるため。
//!
//! **判定そのもの（開き直してよいか、既定デバイスが変わったか）は
//! `super::monitor` の純粋関数が持つ。**

use super::monitor::{
    decide_audio_reconnect, default_audio_device_changed, should_poll_default_audio_device,
    AudioErrorAction,
};
use super::worker::DeviceEvent;
use super::worker_loop::WorkerState;
use crate::audio::{self, AudioDirection};
use crate::i18n;
use log::{debug, info, warn};
use std::time::{Duration, Instant};

/// 音声のクロックドリフト補正（レート比の微調整）を行う間隔。
///
/// `tick` 自体は 100〜500ms ごとに回るが、補正はもっと粗くてよい。
/// クロックのずれは秒単位でしか積もらないので、毎 tick 動かしても
/// 得るものが無く、ログだけ増える。
const RESAMPLE_CORRECTION_INTERVAL: Duration = Duration::from_secs(3);

/// 水位が目標から大きく外れ続けているときの `warn` を間引く間隔。
///
/// 補正が追いつかない状態は一過性のこともあるため、連打せず数十秒に 1 回に留める。
const RESAMPLE_WARN_INTERVAL: Duration = Duration::from_secs(30);

/// この相対誤差（目標水位に対する比率）を超えたら「大きく外れている」とみなす。
///
/// 補正の上限は ±0.1% なので、通常のクロックドリフト（数十〜数百 ppm）は
/// 吸収できる。それでもここまで外れるのは、デバイス側の極端なドリフトや
/// バッファ長そのものが実情に合っていない可能性がある
const RESAMPLE_WARN_RELATIVE_ERROR: f64 = 0.5;

impl WorkerState {
    /// 音声のクロックドリフト補正。水位を見て、レート比の補正係数を
    /// `RESAMPLE_CORRECTION_INTERVAL` ごとに動かす。
    ///
    /// **見るのは前回からの観測の窓の平均**（`take_window`、Issue #308）。
    /// 瞬間の水位は入出力の塊の位相で目標の ±20% ほど揺れるので、1 点で
    /// 決めると補正の向きが読んだ瞬間で変わる。
    ///
    /// 入出力の形が揃っている組み合わせも対象（Issue #308）。まだ音声を
    /// 開いていなければ `resample_telemetry` が無いので、ここで早期に諦める。
    pub(super) fn adjust_resample_correction(&mut self, now: Instant) {
        let Some(telemetry) = self.audio.resample_telemetry() else {
            return;
        };

        let due = self
            .last_resample_correction
            .map(|last| now.duration_since(last) >= RESAMPLE_CORRECTION_INTERVAL)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_resample_correction = Some(now);

        let window = telemetry.take_window();
        let target_level = telemetry.target_level();
        let ratio = audio::decide_resample_correction(window, target_level);
        telemetry.set_correction(ratio);
        // 観測が無い（出力コールバックが回っていない、最初の水位を溜めている）
        // 間は補正も警告もしない
        let Some(water_level) = window.mean() else {
            return;
        };
        if window.underran() && water_level > target_level {
            // 速める補正をアンダーランで見送った（`decide_resample_correction`）。
            // バッファ長が短すぎるかを判断する材料になる
            debug!(
                "アンダーランが起きたので音声のリサンプル比を速める補正を見送った（水位の平均 {} / 目標 {}）",
                water_level, target_level
            );
        }
        if (ratio - 1.0).abs() > f32::EPSILON {
            debug!(
                "音声のリサンプル比を補正した: {:.5}（水位の平均 {} / 目標 {}、{} 回の観測）",
                ratio,
                water_level,
                target_level,
                window.count()
            );
        }

        if target_level == 0 {
            return;
        }
        let relative_error = (water_level as f64 - target_level as f64).abs() / target_level as f64;
        if relative_error < RESAMPLE_WARN_RELATIVE_ERROR {
            return;
        }
        let should_warn = self
            .last_resample_warn
            .map(|last| now.duration_since(last) >= RESAMPLE_WARN_INTERVAL)
            .unwrap_or(true);
        if should_warn {
            self.last_resample_warn = Some(now);
            warn!(
                "音声リングバッファの水位が目標から大きく外れている（水位の平均 {}、目標 {}）。補正の上限（±0.1%）で追いつかない可能性がある",
                water_level, target_level
            );
        }
    }

    /// 音声ストリームのエラーを拾って、必要なら開き直す。
    ///
    /// 間隔は `tick` の `now` で数える。`Instant::now()` を読むと、ワーカーの
    /// テストから開き直しの下限（5 秒）を跨げない（#310）
    pub(super) fn monitor_audio_stream(&mut self, now: Instant) {
        let auto_reconnect = self
            .config
            .as_ref()
            .map(|config| config.auto_reconnect)
            .unwrap_or(true);

        let new_error = self.audio.take_stream_error();
        if new_error {
            // エラーの内容自体は audio::stream が error! で残している
            warn!("音声ストリームのエラーを検出したので切断として扱う");
            // **旗は読んだ時点で下りている。** ここへ移しておかないと、
            // 自動再接続が無効な間や下限に達していない間のエラーが消え、
            // 誰も開き直さないまま音が戻らなくなる
            self.audio_stream_error_pending = true;
        }

        let since_last = self
            .last_audio_error_reconnect
            .map(|reconnected_at| now.saturating_duration_since(reconnected_at));
        match decide_audio_reconnect(
            self.audio_stream_error_pending,
            new_error,
            auto_reconnect,
            since_last,
        ) {
            // 保留しているエラーが無い / 保留したまま待つ。
            // 毎回通るのでログは出さない
            AudioErrorAction::Idle | AudioErrorAction::Wait => return,
            AudioErrorAction::CloseAndReport => {
                // 止まったストリームを閉じ、「接続状態」タブが接続中と出し続けない
                // ようにする。開き直さないので保留は持ち越す（自動再接続を有効に
                // し直したときに開き直す）
                info!("自動再接続が無効なので音声は開き直さない");
                self.audio.stop_capture();
                let reason = i18n::Text::AudioStreamStoppedWithoutReconnect
                    .get()
                    .to_string();
                self.last_audio_failure = Some(reason.clone());
                self.emit(DeviceEvent::AudioFailed(reason));
                return;
            }
            AudioErrorAction::Reconnect => {}
        }

        let Some(config) = self.config.clone() else {
            return;
        };

        self.audio.stop_capture();
        self.audio_stream_error_pending = false;
        self.last_audio_error_reconnect = Some(now);
        self.last_audio_target = None;
        self.audio_retry.request_now(config.audio);
        info!("音声デバイスの再接続を要求した");
    }

    /// 出力の「既定のデバイス」設定が、Windows 側の既定切り替えに追従しているかを
    /// 確認する。内部でタイマーを見て `DEFAULT_AUDIO_DEVICE_POLL_INTERVAL`
    /// おきにしか動かない（#135）。
    ///
    /// cpal は WASAPI の `IMMNotificationClient` を公開しておらず、既定
    /// デバイスの切り替えを通知では受け取れない。
    /// `default_output_device()` を都度問い合わせて名前を突き合わせるしかない。
    pub(super) fn poll_default_audio_device(&mut self, now: Instant) {
        let elapsed = self
            .last_default_audio_check
            .map(|last| now.saturating_duration_since(last));
        if !should_poll_default_audio_device(elapsed) {
            return;
        }
        self.last_default_audio_check = Some(now);

        // 既に音声の再接続を追いかけている最中なら何もしない。ストリームの
        // エラーや映像復帰による再接続と要求が重なるのを防ぐ
        if self.audio_retry.is_active() {
            return;
        }

        let Some(config) = self.config.clone() else {
            return;
        };
        // 追いかけるのは出力だけ。入力が未指定なら音声を開かない
        // （`worker_audio_connect::audio_input_is_selected`、#304）ので、入力を
        // 「既定のデバイス」のまま開いている状態は無い
        let (_, configured_output, ..) = config.audio.clone();
        if configured_output.is_some() {
            // 出力を明示的に選んでいるので、追いかける対象が無い
            return;
        }

        // まだ何も開けていない（起動直後・再接続中）なら、開いた時点の名前が
        // 無いので比べようがない
        let Some(active) = self.audio.active() else {
            return;
        };

        if !default_audio_device_changed(
            configured_output.as_deref(),
            &active.output_device,
            self.audio.default_output_device_name().as_deref(),
        ) {
            return;
        }

        info!("Windows 側の既定の出力デバイスが切り替わったので再接続する");

        // 「既定のデバイス」のキャッシュキーは切り替わっても同じ文字列
        // （`DEFAULT_DEVICE_KEY`）のままなので、古い物理デバイスの対応設定が
        // 残ってしまう。取り直さないと、新しい既定デバイスが対応しない
        // サンプリングレートやチャンネル数のまま開こうとしうる
        self.audio_capabilities
            .remove(&(AudioDirection::Output, audio::cache_key(None)));

        self.audio.stop_capture();
        self.last_audio_target = None;
        self.audio_retry.request_now(config.audio);
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::worker_loop::testing::{
        apply_config, config_for, drain, mock_state, state_with,
    };
    use super::*;

    // どのテストもモックのバックエンド（`super::super::backend::mock`）を載せ、
    // スレッドを起こさずに `tick` を直接呼ぶ。渡す時刻は自分で進めるので、
    // バックオフもフレームの途絶も実時間を待たずに跨げる。

    #[test]
    fn worker_audio_stream_error_reopens_the_passthrough() {
        // cpal のエラーコールバックが旗を立てた場合。ワーカーが回収して開き直す
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);

        // 映像は未設定にして、音声だけを動かす
        let mut config = config_for(None, Some("モック入力"));
        config.auto_reconnect = true;
        apply_config(&mut state, config, false);

        let base = Instant::now();
        state.tick(base);
        assert_eq!(audio.with(|state| state.start_calls), 1, "まず繋がること");
        drain(&events);

        audio.with(|state| state.stream_error = true);
        state.tick(base + Duration::from_millis(100));
        assert_eq!(
            audio.with(|state| state.stop_calls),
            1,
            "エラーを拾ったらストリームを閉じること"
        );

        // 映像と同じく、接続に成功してから 1 秒は開き直さない（#232）
        state.tick(base + Duration::from_millis(200));
        assert_eq!(
            audio.with(|state| state.start_calls),
            1,
            "成功から 1 秒の下限までは開き直さないこと"
        );

        state.tick(base + Duration::from_secs(1));
        assert_eq!(
            audio.with(|state| state.start_calls),
            2,
            "閉じたあと開き直すこと"
        );
    }

    #[test]
    fn worker_picks_up_fake_audio_error_scenario_and_reopens() {
        // フェイクの音声（シナリオ audio-error）が立てたエラーを、本物の cpal の
        // エラーと同じ経路で拾って開き直す。フェイクの時計を `tick` へ渡す時刻と
        // 揃え、実時間を待たずに期限を跨ぐ
        use crate::audio::{AudioControls, FakeAudioCapture, FakeAudioOptions};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let base = Instant::now();
        let elapsed_ms = Arc::new(AtomicU64::new(0));
        let clock = {
            let elapsed_ms = Arc::clone(&elapsed_ms);
            move || base + Duration::from_millis(elapsed_ms.load(Ordering::Relaxed))
        };
        let at = |ms: u64| {
            elapsed_ms.store(ms, Ordering::Relaxed);
            base + Duration::from_millis(ms)
        };

        let after = Duration::from_secs(5);
        let audio = FakeAudioCapture::new(
            Arc::new(AudioControls::default()),
            audio::AudioTap::new(),
            FakeAudioOptions {
                input_count: 1,
                failures_before_success: 0,
                stream_error_after: Some(after),
            },
        )
        .with_clock(Arc::new(clock));
        let video = MockVideoBackend::default();
        let (mut state, events) = state_with(Box::new(video), Box::new(audio));

        let mut config = config_for(None, Some("Fake Audio Input 1"));
        config.auto_reconnect = true;
        apply_config(&mut state, config, false);

        state.tick(at(0));
        assert!(
            drain(&events)
                .iter()
                .any(|event| matches!(event, DeviceEvent::AudioConnected)),
            "まず繋がること"
        );
        state.tick(at(4_900));
        assert!(
            state.last_audio_error_reconnect.is_none(),
            "期限前は開き直さないこと"
        );

        state.tick(at(5_000));
        assert!(
            state.last_audio_error_reconnect.is_some(),
            "エラーを拾って再接続を積むこと"
        );

        state.tick(at(5_300));
        assert!(
            drain(&events)
                .iter()
                .any(|event| matches!(event, DeviceEvent::AudioConnected)),
            "開き直して繋がること"
        );
        // 開き直した時刻から数え直す。元の期限の倍ではまだ立たない
        at(10_000);
        assert!(!state.audio.take_stream_error(), "開き直したら数え直すこと");
        at(10_400);
        assert!(state.audio.take_stream_error(), "新しい期限で立つこと");
    }

    #[test]
    fn worker_audio_stream_error_without_auto_reconnect_closes_and_reports() {
        // 自動再接続を切っていても、エラーを拾ったらストリームを閉じて UI へ
        // 知らせる（#310）。開き直しはしないが、保留は持ち越す
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);
        // `config_for` の既定が auto_reconnect: false
        apply_config(&mut state, config_for(None, Some("モック入力")), false);

        let base = Instant::now();
        state.tick(base);
        assert!(state.audio.active().is_some(), "まず繋がること");
        drain(&events);

        audio.with(|state| state.stream_error = true);
        state.tick(base + Duration::from_millis(100));
        assert_eq!(audio.with(|state| state.stop_calls), 1, "閉じること");
        assert!(
            state.audio.active().is_none(),
            "接続中として出し続けないこと"
        );
        assert_eq!(
            audio_failures(&drain(&events)).len(),
            1,
            "1 度だけ知らせること"
        );
        assert!(!state.audio_retry.is_active(), "再試行はしないこと");

        state.tick(base + Duration::from_secs(10));
        assert!(
            audio_failures(&drain(&events)).is_empty(),
            "繰り返さないこと"
        );
        assert_eq!(audio.with(|state| state.start_calls), 1, "開き直さないこと");

        // 自動再接続を有効にし直すと、持ち越した保留で開き直す
        let mut config = config_for(None, Some("モック入力"));
        config.auto_reconnect = true;
        apply_config(&mut state, config, false);
        state.tick(base + Duration::from_secs(11));
        state.tick(base + Duration::from_secs(12));
        assert_eq!(audio.with(|state| state.start_calls), 2, "開き直すこと");
    }

    #[test]
    fn worker_does_not_reopen_a_stream_opened_after_the_error() {
        // 下限の内側で保留したエラーのあと、設定変更で新しいストリームが
        // 正常に開いたら、下限が明けてもそれを閉じない（#310）
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, _events) = mock_state(&video, &audio);
        let mut config = config_for(None, Some("モック入力"));
        config.auto_reconnect = true;
        apply_config(&mut state, config.clone(), false);

        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        state.tick(at(0));
        // 1 回目のエラーで開き直す（下限はここから数える）
        audio.with(|state| state.stream_error = true);
        state.tick(at(100));
        state.tick(at(1_100));
        assert_eq!(audio.with(|state| state.start_calls), 2);

        // 2 回目は下限（5 秒）の内側なので保留される
        audio.with(|state| state.stream_error = true);
        state.tick(at(2_200));
        assert!(state.audio_stream_error_pending, "保留されること");

        // 設定変更で新しいストリームが開く
        config.audio.4 += 10;
        apply_config(&mut state, config, false);
        state.tick(at(2_300));
        assert_eq!(
            audio.with(|state| state.start_calls),
            3,
            "設定変更で開くこと"
        );
        let stops = audio.with(|state| state.stop_calls);

        state.tick(at(5_200));
        state.tick(at(6_300));
        assert_eq!(
            audio.with(|state| state.stop_calls),
            stops,
            "正常に開いたストリームを閉じないこと"
        );
        assert_eq!(audio.with(|state| state.start_calls), 3);
    }

    /// 音声の失敗の理由を、届いた順に取り出す。
    fn audio_failures(events: &[DeviceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::AudioFailed(reason) => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn worker_does_not_open_the_default_input_after_the_input_is_cleared() {
        // 設定の初期化・読み込みで入力が未指定になっても、Windows の既定の
        // 入力（ノート PC なら内蔵マイク）を開かない（#304）。起動直後でない
        // `ApplyConfig` では `resolve_default_devices` が通らないので、`None` の
        // まま接続の試行まで届く
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        audio.with(|state| state.input_devices = vec!["モック入力".to_string()]);
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), Some("モック入力")),
            true,
        );
        let base = Instant::now();
        state.tick(base);
        assert_eq!(audio.with(|state| state.start_calls), 1);
        assert!(audio.with(|state| state.running));
        drain(&events);

        // 「設定を初期化」→「適用」。入力だけが未指定になって届く
        let cleared = config_for(Some("キャプチャーボード"), None);
        apply_config(&mut state, cleared.clone(), false);
        state.tick(base + Duration::from_secs(2));

        assert_eq!(
            audio.with(|state| state.start_calls),
            1,
            "未指定の入力で開き直さないこと"
        );
        assert!(
            !audio.with(|state| state.running),
            "設定に合わせて、前の入力のパススルーは閉じること"
        );
        assert!(!state.audio_retry.is_active(), "再試行も続けないこと");
        let reasons = audio_failures(&drain(&events));
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            reasons[0].contains("オーディオ入力デバイスが選ばれていません"),
            "{reasons:?}"
        );

        // 2 秒ごとの `apply_settings` で同じ設定が届いても、通知を繰り返さない
        for step in 2..6 {
            apply_config(&mut state, cleared.clone(), false);
            state.tick(base + Duration::from_secs(2) * step);
        }
        assert_eq!(audio.with(|state| state.start_calls), 1);
        assert!(audio_failures(&drain(&events)).is_empty());

        // 入力を選び直せば、また開く
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), Some("モック入力")),
            false,
        );
        state.tick(base + Duration::from_secs(20));
        assert_eq!(audio.with(|state| state.start_calls), 2);
        assert!(audio.with(|state| state.running));
    }

    #[test]
    fn worker_does_not_open_the_default_input_when_none_is_listed_at_startup() {
        // 起動時に入力が 1 台も列挙できないと `resolve_default_devices` が埋められず、
        // 未指定のまま届く。このときも既定の入力へは倒さない
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), None),
            true,
        );
        state.tick(Instant::now());

        assert_eq!(audio.with(|state| state.start_calls), 0);
        assert_eq!(audio_failures(&drain(&events)).len(), 1);
    }
}
