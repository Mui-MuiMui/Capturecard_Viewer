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
        let Some(telemetry) = self.audio.resample_telemetry().cloned() else {
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

        let elapsed_ms = state
            .since_last_frame
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        info!(
            "映像フレームが {} ms 途絶えたので、表示を落として切断として扱う",
            elapsed_ms
        );
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
            // エラーの内容自体は audio.rs が error! で残している
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
