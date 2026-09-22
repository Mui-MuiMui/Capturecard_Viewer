//! デバイスを開く・閉じる・列挙する・能力を問い合わせる処理。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の上でだけ走る。
//! `WorkerState` に生やす形にしてあるのは、`CaptureCardViewer` を
//! `app` の子モジュールで分担しているのと同じ理由で、状態を 1 つに保ったまま
//! 役割ごとにファイルを分けるため。
//!
//! **ここに時計やタイマーを置かない。** 「いつ試すか」は
//! `super::retry::ConnectRetry`、「途絶したか」は `super::monitor` が決める。

use super::monitor::{decide_audio_fallback, should_resync_audio_after_video, AudioFallbackAction};
use super::retry::backoff_delay;
use super::worker::{DeviceConfig, DeviceEvent};
use super::worker_loop::WorkerState;
use crate::audio::{self, AudioDirection};
use crate::video::VideoCapture;
use log::{debug, info, warn};
use std::time::Instant;

impl WorkerState {
    /// 未設定のデバイス名を、列挙結果の先頭で埋める。**起動直後の 1 回だけ。**
    ///
    /// **入力デバイスは出力と違い、未設定のままにしない。** 出力の既定は
    /// 「スピーカー」でまず無害だが、入力の既定は環境依存（ノート PC ならほぼ
    /// 確実に内蔵マイク）で、パススルーがそのままマイクの音をスピーカーへ
    /// 流してしまう。#134（PR #147）で切断時に同じことが起きる不具合を
    /// 直したばかりで、初回起動で同じ誤動作を起こすわけにいかない。
    ///
    /// 出力は `None`（Windows の既定デバイス）のままにする。
    ///
    /// 決めた名前は `DefaultDevicesResolved` で UI スレッドへ返し、設定へ
    /// 書き戻してもらう。**返す前にこの場の `config` も書き換える。**
    /// 往復を待つと、最初の接続がその分だけ遅れる。
    pub(super) fn resolve_default_devices(&mut self, config: &mut DeviceConfig) {
        let mut resolved_video = None;
        let mut resolved_input = None;

        if config.video.0.is_none() {
            if let Some((name, _)) = VideoCapture::list_devices().into_iter().next() {
                config.video.0 = Some(name.clone());
                resolved_video = Some(name);
            }
        }

        if config.audio.0.is_none() {
            let list = self.audio.list_input_devices();
            debug!("利用できる入力デバイス: {:?}", list);
            if let Some(name) = list.into_iter().next() {
                info!("入力デバイスの既定を {} にした", name);
                config.audio.0 = Some(name.clone());
                resolved_input = Some(name);
            }
        }

        // 出力は埋めない。`None` のまま Windows の既定デバイスへ任せる
        if config.audio.1.is_none() {
            debug!("出力デバイスは既定（自動選択）にする");
        }

        if resolved_video.is_some() || resolved_input.is_some() {
            self.emit(DeviceEvent::DefaultDevicesResolved {
                video: resolved_video,
                input: resolved_input,
            });
        }
    }

    /// 映像デバイスへの接続を 1 回だけ試す。
    pub(super) fn try_connect_video(&mut self, config: &DeviceConfig, now: Instant) {
        let (device_name, resolution, format, fps) = config.video.clone();
        let Some(device_name) = device_name else {
            // 繋ぐ相手が無い。要求を取り下げて、デバイスが選ばれるまで待つ
            debug!("映像デバイスが未設定なので接続の要求を取り下げる");
            self.video_retry.cancel();
            return;
        };

        let attempt = self.video_retry.attempts() + 1;
        info!(
            "映像デバイスへの接続を試す（{} 回目）: {}",
            attempt, device_name
        );

        let result =
            self.video
                .start_capture(Some(&device_name), resolution, format.as_deref(), fps);

        match result {
            Ok(()) => {
                info!("映像デバイスに接続した");
                self.video_retry.record_success();
                self.last_video_target = Some(config.video.clone());
                self.emit(DeviceEvent::VideoConnected);
                // 途絶から復帰したのであれば、音声も同時に戻っているはず。
                // **旗はここで落とす。** 残すと、以降の接続のたびに音声を
                // 開き直してしまう
                let recovered = std::mem::take(&mut self.video_reconnect_after_loss);
                self.resync_audio_after_video_recovery(config, recovered);
            }
            Err(e) => {
                warn!("映像デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                self.video_retry.record_failure(now);
                // UI へは日本語の 1 行に落として渡す。`DeviceEvent` に種別を
                // 載せても、いまの再試行は理由で戦略を変えないため
                self.emit(DeviceEvent::VideoFailed(e.to_string()));
                debug!(
                    "映像デバイスへの再試行は {} ms 後",
                    backoff_delay(self.video_retry.attempts()).as_millis()
                );
            }
        }
    }

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
                self.audio_retry.record_success();
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
                self.emit(DeviceEvent::AudioFailed(e));
                // **取得済みの対応設定を捨てて取り直す。** デバイスが挿し直された
                // 場合、古い一覧でしか開けない設定を選び続けて失敗が繰り返される
                self.audio_capabilities
                    .remove(&(AudioDirection::Input, input_key));
                self.audio_capabilities
                    .remove(&(AudioDirection::Output, output_key));
                debug!(
                    "音声デバイスへの再試行は {} ms 後",
                    backoff_delay(self.audio_retry.attempts()).as_millis()
                );
            }
        }
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

    /// デバイス一覧を取り直して UI スレッドへ返す。
    pub(super) fn refresh_device_lists(&mut self) {
        let video = VideoCapture::list_devices();
        let input = self.audio.list_input_devices();
        let output = self.audio.list_output_devices();
        self.emit(DeviceEvent::DeviceLists {
            video,
            input,
            output,
        });
    }

    /// 映像デバイスの対応形式を問い合わせる。
    pub(super) fn query_video_capabilities(&mut self, device: String) {
        let started = Instant::now();
        // 設定ダイアログの能力キャッシュは理由を画面に出すだけなので、
        // ここで日本語の 1 行へ落として渡す
        let result = VideoCapture::get_device_capabilities(Some(&device));
        match &result {
            Ok(caps) => info!(
                "デバイス能力を取得した: {}（{} フォーマット, {} ms）",
                device,
                caps.len(),
                started.elapsed().as_millis()
            ),
            Err(e) => warn!("デバイス能力を取得できない: {}: {}", device, e),
        }
        self.emit(DeviceEvent::VideoCapabilities(
            device,
            Box::new(result.map_err(|e| e.to_string())),
        ));
    }

    /// 音声デバイスの対応設定を問い合わせる。
    ///
    /// 成功した分だけワーカー側にも控えておく。`start_passthrough` が
    /// 一覧を要るためで、渡さないとその場で列挙し直すことになる。
    pub(super) fn query_audio_capabilities(&mut self, direction: AudioDirection, key: &str) {
        let started = Instant::now();
        let result = audio::query_capabilities(direction, audio::device_name_from_key(key));
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
            Box::new(result),
        ));
    }
}
