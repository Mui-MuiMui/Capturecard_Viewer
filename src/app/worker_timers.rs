//! タイマーで駆動する監視。接続の再試行、フレームの途絶、音声ストリームの
//! エラー、Windows の既定デバイスの切り替え、クロックドリフト補正。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の `tick` から
//! 呼ばれ、そのスレッドの上でだけ走る。`WorkerState` に生やす形にしてあるのは
//! `super::worker_connect` と同じ理由で、状態を 1 つに保ったまま役割ごとに
//! ファイルを分けるため。
//!
//! **判定そのもの（途絶したか、開き直してよいか）は `super::monitor` の
//! 純粋関数が持つ。** ここはその結果を受けてデバイスを触る側と、
//! 「いつ見に行くか」の間隔だけを持つ。

use super::monitor::{
    decide_audio_reconnect, decide_video_link, default_audio_device_changed,
    should_poll_default_audio_device, AudioErrorAction, VideoLinkAction, VIDEO_SIGNAL_TIMEOUT,
};
use super::worker::DeviceEvent;
use super::worker_loop::WorkerState;
use crate::audio::{self, AudioDirection};
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
    /// 期限が来ている接続を試し、稼働中のデバイスが生きているかを見る。
    pub(super) fn tick(&mut self, now: Instant) {
        self.poll_connection(now);
        self.monitor_video_link();
        self.monitor_audio_stream();
        self.poll_default_audio_device();
        self.adjust_resample_correction(now);
    }

    /// 音声のクロックドリフト補正。水位を見て、レート比の補正係数を
    /// `RESAMPLE_CORRECTION_INTERVAL` ごとに動かす。
    ///
    /// **揃っている組み合わせ（identity）では何もしない。** `resample_telemetry`
    /// は変換が要る場合しか作らないので、まだ音声を開いていない場合も含めて
    /// ここで早期に諦める。
    fn adjust_resample_correction(&mut self, now: Instant) {
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

        let water_level = telemetry.water_level();
        let target_level = telemetry.target_level();
        // ここへ来る時点で identity ではないと分かっているので false 固定。
        // 純粋関数側の identity 判定は主にテストのための引数
        let ratio = audio::decide_resample_correction(false, water_level, target_level);
        telemetry.set_correction(ratio);
        if (ratio - 1.0).abs() > f32::EPSILON {
            debug!(
                "音声のリサンプル比を補正した: {:.5}（水位 {} / 目標 {}）",
                ratio, water_level, target_level
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
                "音声リングバッファの水位が目標から大きく外れている（水位 {}、目標 {}）。補正の上限（±0.1%）で追いつかない可能性がある",
                water_level, target_level
            );
        }
    }

    /// 期限が来ているデバイスの接続を 1 回だけ試す。
    fn poll_connection(&mut self, now: Instant) {
        let video_due = self.video_retry.is_due(now);
        let audio_due = self.audio_retry.is_due(now);
        if !video_due && !audio_due {
            return;
        }
        let Some(config) = self.config.clone() else {
            return;
        };

        if video_due {
            self.try_connect_video(&config, now);
        }
        if audio_due {
            self.try_connect_audio(&config, now);
        }
    }

    /// フレームの途絶を見て、表示を落とし、必要なら映像を開き直す。
    fn monitor_video_link(&mut self) {
        let auto_reconnect = self
            .config
            .as_ref()
            .map(|config| config.auto_reconnect)
            .unwrap_or(true);
        let state = self.video.link_state();
        let action = decide_video_link(state, auto_reconnect, VIDEO_SIGNAL_TIMEOUT);

        if action == VideoLinkAction::Keep {
            // 途絶が解消した（開き直した、ストリームを閉じた、フレームが戻った）。
            // **記録は必ず戻す。** 戻さないと、開き直したあとの途絶が
            // 「同じ処置が続いている」と見なされて検出されなくなる。
            //
            // ログを出すのはフレームが実際に戻ったときだけ。ストリームを
            // 閉じた直後も判定は `Keep` になるので、そこで「届き始めた」と
            // 書くと嘘になる
            if self.last_video_link_action != VideoLinkAction::Keep
                && state.capturing
                && state.since_last_frame.is_some()
            {
                info!("映像フレームが再び届き始めたので表示を再開する");
            }
            self.last_video_link_action = action;
            return;
        }
        // 途絶は毎回同じ判定に当たる。同じ扱いが続く間は 1 度だけ動く
        if action == self.last_video_link_action {
            return;
        }
        self.last_video_link_action = action;

        if state.device_lost {
            // 知らせの中身（イベントの種類）はバックエンドが出している
            info!("映像デバイスが喪失を知らせてきたので、表示を落として切断として扱う");
        } else {
            let elapsed_ms = state
                .since_last_frame
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default();
            info!(
                "映像フレームが {} ms 途絶えたので、表示を落として切断として扱う",
                elapsed_ms
            );
        }
        // 最後のフレームが残り続けると、止まっているのか映っているのか判らない。
        // テクスチャを捨てて「映像信号がありません」の表示へ戻すのは UI の仕事
        self.emit(DeviceEvent::VideoSignalLost);

        if action != VideoLinkAction::ClearTextureAndReconnect {
            debug!("自動再接続が無効なので映像は開き直さない");
            return;
        }

        let Some(config) = self.config.clone() else {
            return;
        };

        // ストリームを閉じてから要求する。閉じておくと表示が
        // 「デバイスが接続されていません」へ変わり、信号だけが無い状態と区別できる
        self.video.stop_capture();

        // 既存のバックオフへ乗せる。**ここでデバイスを列挙しない。**
        // 対象が戻っているかは `start_capture` の中の列挙（実測 1〜3ms）が
        // 確かめる。再試行の間隔は最大 5 秒で頭打ちなので、
        // MediaFoundation への問い合わせもその頻度を超えない
        self.last_video_target = None;
        // 次に繋がったときは、同じ USB 機器の音声も戻っているとみなして
        // 音声の再接続も要求する。起動時の接続と区別するためにここで立てる
        self.video_reconnect_after_loss = true;
        self.video_retry.request_now(config.video);
        info!("映像デバイスの再接続を要求した");
    }

    /// 音声ストリームのエラーを拾って、必要なら開き直す。
    fn monitor_audio_stream(&mut self) {
        let auto_reconnect = self
            .config
            .as_ref()
            .map(|config| config.auto_reconnect)
            .unwrap_or(true);

        if self.audio.take_stream_error() {
            // エラーの内容自体は audio::stream が error! で残している
            warn!("音声ストリームのエラーを検出したので切断として扱う");
            // **旗は読んだ時点で下りている。** ここへ移しておかないと、
            // 自動再接続が無効な間や下限に達していない間のエラーが消え、
            // 誰も開き直さないまま音が戻らなくなる
            self.audio_stream_error_pending = true;
        }

        let since_last = self
            .last_audio_error_reconnect
            .map(|reconnected_at| reconnected_at.elapsed());
        match decide_audio_reconnect(self.audio_stream_error_pending, auto_reconnect, since_last) {
            // 保留しているエラーが無い / 保留したまま待つ。
            // 毎回通るのでログは出さない
            AudioErrorAction::Idle | AudioErrorAction::Wait => return,
            AudioErrorAction::Reconnect => {}
        }

        let Some(config) = self.config.clone() else {
            return;
        };

        self.audio.stop_capture();
        self.audio_stream_error_pending = false;
        self.last_audio_error_reconnect = Some(Instant::now());
        self.last_audio_target = None;
        self.audio_retry.request_now(config.audio);
        info!("音声デバイスの再接続を要求した");
    }

    /// 「既定のデバイス」設定が、Windows 側の既定切り替えに追従しているかを
    /// 確認する。内部でタイマーを見て `DEFAULT_AUDIO_DEVICE_POLL_INTERVAL`
    /// おきにしか動かない（#135）。
    ///
    /// cpal は WASAPI の `IMMNotificationClient` を公開しておらず、既定
    /// デバイスの切り替えを通知では受け取れない。`default_input_device()` /
    /// `default_output_device()` を都度問い合わせて名前を突き合わせるしかない。
    fn poll_default_audio_device(&mut self) {
        let elapsed = self.last_default_audio_check.map(|last| last.elapsed());
        if !should_poll_default_audio_device(elapsed) {
            return;
        }
        self.last_default_audio_check = Some(Instant::now());

        // 既に音声の再接続を追いかけている最中なら何もしない。ストリームの
        // エラーや映像復帰による再接続と要求が重なるのを防ぐ
        if self.audio_retry.is_active() {
            return;
        }

        let Some(config) = self.config.clone() else {
            return;
        };
        let (configured_input, configured_output, ..) = config.audio.clone();
        let track_input = configured_input.is_none();
        let track_output = configured_output.is_none();
        if !track_input && !track_output {
            // 入出力とも明示的にデバイスを選んでいるので、追いかける対象が無い
            return;
        }

        // まだ何も開けていない（起動直後・再接続中）なら、開いた時点の名前が
        // 無いので比べようがない
        let Some(active) = self.audio.active() else {
            return;
        };

        let input_switched = track_input
            && default_audio_device_changed(
                configured_input.as_deref(),
                &active.input_device,
                self.audio.default_input_device_name().as_deref(),
            );
        let output_switched = track_output
            && default_audio_device_changed(
                configured_output.as_deref(),
                &active.output_device,
                self.audio.default_output_device_name().as_deref(),
            );
        if !input_switched && !output_switched {
            return;
        }

        info!(
            "Windows 側の既定音声デバイスが切り替わったので再接続する（入力: {}, 出力: {}）",
            input_switched, output_switched
        );

        // 「既定のデバイス」のキャッシュキーは切り替わっても同じ文字列
        // （`DEFAULT_DEVICE_KEY`）のままなので、古い物理デバイスの対応設定が
        // 残ってしまう。取り直さないと、新しい既定デバイスが対応しない
        // サンプリングレートやチャンネル数のまま開こうとしうる
        let default_key = audio::cache_key(None);
        if input_switched {
            self.audio_capabilities
                .remove(&(AudioDirection::Input, default_key.clone()));
        }
        if output_switched {
            self.audio_capabilities
                .remove(&(AudioDirection::Output, default_key));
        }

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
    fn worker_retries_video_until_the_backend_succeeds() {
        // 2 回失敗してから繋がる。バックオフ（200ms → 400ms）を跨ぐように
        // 時刻を進め、その都度 1 回だけ試していることを見る
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| state.failures_before_success = 2);
        let (mut state, events) = mock_state(&video, &audio);

        apply_config(
            &mut state,
            config_for(Some("モックカメラ"), Some("モック入力")),
            false,
        );

        let base = Instant::now();
        state.tick(base);
        // まだ 200ms 経っていないので、ここでは試さない
        state.tick(base + Duration::from_millis(100));
        assert_eq!(
            video.with(|state| state.start_calls),
            1,
            "バックオフの途中で試し直さないこと"
        );

        state.tick(base + Duration::from_millis(250));
        state.tick(base + Duration::from_millis(700));

        assert_eq!(
            video.with(|state| state.start_calls),
            3,
            "失敗した回数のぶんだけ試して繋がること"
        );
        assert!(
            !state.video_retry.is_active(),
            "繋がったら追いかけるのをやめること"
        );

        let events = drain(&events);
        let failures = events
            .iter()
            .filter(|event| matches!(event, DeviceEvent::VideoFailed(_)))
            .count();
        assert_eq!(failures, 2, "失敗のたびに理由を返すこと");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoConnected)),
            "最後は接続を知らせること"
        );
    }

    #[test]
    fn worker_video_signal_loss_stops_the_stream_and_requests_a_reconnect() {
        // 開けているのにフレームだけが止まった場合。表示を落として開き直す
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);

        let mut config = config_for(Some("モックカメラ"), Some("モック入力"));
        config.auto_reconnect = true;
        apply_config(&mut state, config, false);

        let base = Instant::now();
        state.tick(base);
        assert!(video.with(|state| state.capturing), "まず繋がること");
        drain(&events);

        // フレームが途絶えたことにする。`VIDEO_SIGNAL_TIMEOUT` 未満では動かない
        video.with(|state| {
            state.since_last_frame = Some(VIDEO_SIGNAL_TIMEOUT - Duration::from_millis(1));
        });
        state.tick(base + Duration::from_millis(100));
        assert!(
            drain(&events).is_empty(),
            "閾値に届くまでは切断として扱わないこと"
        );
        assert_eq!(video.with(|state| state.stop_calls), 0);

        video.with(|state| {
            state.since_last_frame = Some(VIDEO_SIGNAL_TIMEOUT + Duration::from_millis(1));
        });
        state.tick(base + Duration::from_millis(200));

        assert!(
            drain(&events)
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoSignalLost)),
            "表示中のテクスチャを捨てるよう UI へ知らせること"
        );
        assert_eq!(
            video.with(|state| state.stop_calls),
            1,
            "「信号だけ無い」と区別できるようストリームを閉じること"
        );
        assert!(state.video_retry.is_active(), "開き直しを要求すること");
        assert!(
            state.video_reconnect_after_loss,
            "次に繋がったとき音声も開き直す目印を立てること"
        );
    }

    #[test]
    fn worker_video_device_lost_requests_a_reconnect_without_waiting_for_timeout() {
        // DirectShow のグラフが EC_DEVICE_LOST を知らせてきた場合。フレームは
        // 直前まで届いているが、途絶の閾値を待たずに次の tick で開き直しを積む
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);

        let mut config = config_for(Some("モックカメラ"), Some("モック入力"));
        config.auto_reconnect = true;
        apply_config(&mut state, config, false);

        let base = Instant::now();
        state.tick(base);
        assert!(video.with(|state| state.capturing), "まず繋がること");
        drain(&events);

        video.with(|state| {
            state.since_last_frame = Some(Duration::from_millis(16));
            state.device_lost = true;
        });
        state.tick(base + Duration::from_millis(100));

        assert!(
            drain(&events)
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoSignalLost)),
            "表示中のテクスチャを捨てるよう UI へ知らせること"
        );
        assert_eq!(
            video.with(|state| state.stop_calls),
            1,
            "ストリームを閉じること"
        );
        assert_eq!(
            video.with(|state| state.start_calls),
            1,
            "監視の中では開かないこと（開くのは次の tick の poll_connection）"
        );
        assert!(state.video_retry.is_active(), "開き直しを要求すること");

        // 接続に成功してから 1 秒は開き直さない（#232）。要求は積んだまま待つ
        state.tick(base + Duration::from_millis(200));
        assert_eq!(
            video.with(|state| state.start_calls),
            1,
            "成功から 1 秒の下限までは開き直さないこと"
        );

        state.tick(base + Duration::from_secs(1));
        assert_eq!(
            video.with(|state| state.start_calls),
            2,
            "下限を過ぎた tick で開き直すこと"
        );
    }

    #[test]
    fn worker_video_signal_loss_without_auto_reconnect_keeps_the_stream() {
        // 自動再接続を切っているときは、表示を落とすだけで開き直さない
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);

        // `config_for` の既定が auto_reconnect: false
        apply_config(
            &mut state,
            config_for(Some("モックカメラ"), Some("モック入力")),
            false,
        );

        let base = Instant::now();
        state.tick(base);
        drain(&events);

        video.with(|state| {
            state.since_last_frame = Some(VIDEO_SIGNAL_TIMEOUT + Duration::from_millis(1));
        });
        state.tick(base + Duration::from_millis(100));

        assert!(
            drain(&events)
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoSignalLost)),
            "表示は落とすこと"
        );
        assert_eq!(
            video.with(|state| state.stop_calls),
            0,
            "開き直さないのだからストリームは閉じないこと"
        );
        assert!(!state.video_retry.is_active(), "再試行を要求しないこと");
    }

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
}
