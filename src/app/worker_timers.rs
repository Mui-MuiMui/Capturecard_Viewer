//! タイマーで駆動する監視。`tick` の入口、接続の再試行、フレームの途絶。
//! 音声まわり（ストリームのエラー、Windows の既定デバイスの切り替え、
//! クロックドリフト補正）は `super::worker_audio_timers` に置く。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の `tick` から
//! 呼ばれ、そのスレッドの上でだけ走る。`WorkerState` に生やす形にしてあるのは
//! `super::worker_connect` と同じ理由で、状態を 1 つに保ったまま役割ごとに
//! ファイルを分けるため。
//!
//! **判定そのもの（途絶したか、開き直してよいか）は `super::monitor` の
//! 純粋関数が持つ。** ここはその結果を受けてデバイスを触る側と、
//! 「いつ見に行くか」の間隔だけを持つ。

use super::monitor::{decide_video_link, VideoLinkAction, VIDEO_SIGNAL_TIMEOUT};
use super::worker::DeviceEvent;
use super::worker_loop::WorkerState;
use log::{debug, info};
use std::time::Instant;

impl WorkerState {
    /// 期限が来ている接続を試し、稼働中のデバイスが生きているかを見る。
    pub(super) fn tick(&mut self, now: Instant) {
        self.poll_connection(now);
        // 接続を試した直後に置く。起動時は最初の接続を待たせずに列挙し、
        // 失敗が節目に届いた回もその場で判定できる
        self.log_device_enumeration();
        self.monitor_video_link();
        // 映像の途絶を見たあとに置く。映像を閉じた回に、音声ピンの音声も同じ回で閉じる
        self.monitor_audio_pin();
        self.monitor_audio_stream(now);
        self.poll_default_audio_device(now);
        self.adjust_resample_correction(now);
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
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::worker_loop::testing::{apply_config, config_for, drain, mock_state};
    use super::*;
    use std::time::Duration;

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

    /// 映像の失敗の理由を、届いた順に取り出す。
    fn video_failures(events: &[DeviceEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::VideoFailed(reason) => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn worker_closes_the_video_stream_after_the_device_is_cleared() {
        // 設定の初期化・読み込みで映像デバイスが未指定になったら、開いている
        // ストリームを閉じる（#334）。閉じないと古い映像が映り続けたまま、
        // 設定の表示だけが「未選択」になる
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, events) = mock_state(&video, &audio);
        let selected = config_for(Some("キャプチャーボード"), None);
        apply_config(&mut state, selected.clone(), false);
        let base = Instant::now();
        state.tick(base);
        assert!(video.with(|state| state.capturing));
        drain(&events);

        // 「設定を初期化」→「適用」。映像デバイスが未指定になって届く
        let cleared = config_for(None, None);
        apply_config(&mut state, cleared.clone(), false);
        state.tick(base + Duration::from_secs(2));

        assert_eq!(video.with(|state| state.start_calls), 1, "開き直さないこと");
        assert!(
            !video.with(|state| state.capturing),
            "ストリームを閉じること"
        );
        assert!(!state.video_retry.is_active(), "再試行も続けないこと");
        let after_clear = drain(&events);
        assert!(
            after_clear
                .iter()
                .any(|event| matches!(event, DeviceEvent::VideoSignalLost)),
            "最後のフレームを画面から落とすこと"
        );
        let reasons = video_failures(&after_clear);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            reasons[0].contains("映像デバイスが選ばれていません"),
            "{reasons:?}"
        );

        // 2 秒ごとの `apply_settings` で同じ設定が届いても、通知を繰り返さない
        for step in 2..6 {
            apply_config(&mut state, cleared.clone(), false);
            state.tick(base + Duration::from_secs(2) * step);
        }
        assert_eq!(video.with(|state| state.stop_calls), 1);
        assert!(video_failures(&drain(&events)).is_empty());

        // 映像デバイスを選び直せば、また開く
        apply_config(&mut state, selected, false);
        state.tick(base + Duration::from_secs(20));
        assert_eq!(video.with(|state| state.start_calls), 2);
        assert!(video.with(|state| state.capturing));
    }
}
