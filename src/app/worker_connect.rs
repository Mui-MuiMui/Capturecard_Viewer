//! デバイスを開く・閉じる・列挙する・能力を問い合わせる処理。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の上でだけ走る。
//! `WorkerState` に生やす形にしてあるのは、`CaptureCardViewer` を
//! `app` の子モジュールで分担しているのと同じ理由で、状態を 1 つに保ったまま
//! 役割ごとにファイルを分けるため。
//!
//! **ここに時計やタイマーを置かない。** 「いつ試すか」は
//! `super::retry::ConnectRetry`、「途絶したか」は `super::monitor` が決める。

use super::monitor::{
    decide_audio_fallback, decide_device_not_visible, should_log_enumeration,
    should_resync_audio_after_video, AudioFallbackAction, DeviceNotVisible,
};
use super::retry::backoff_delay;
use super::worker::{DeviceConfig, DeviceEvent};
use super::worker_loop::WorkerState;
use crate::audio::{self, AudioDirection};
use crate::i18n;
use crate::settings::VideoBackendSetting;
use log::{debug, info, warn};
use std::fmt::Display;
use std::time::Instant;

/// 接続の失敗を UI へ渡す 1 行。「Windows 側にも見えていない」と判定済みなら
/// 案内を添える。
fn failure_message(not_visible: Option<&DeviceNotVisible>, reason: &str) -> String {
    match not_visible {
        Some(notice) => i18n::failure_with_device_not_visible(notice, reason),
        None => reason.to_string(),
    }
}

/// 音声を開いてよいだけの入力デバイスが設定に書かれているか（#304）。
///
/// **未指定の入力を「Windows の既定の入力」の意味に取らない。** 既定の入力は
/// 環境依存で、ノート PC ならほぼ確実に内蔵マイクになり、パススルーがその音を
/// スピーカーへ流す（#134 / #165 と同じ症状）。未指定のまま届くのは設定の
/// 初期化・読み込みと、起動時に入力が 1 台も列挙できなかったとき。
/// 空文字も名前として扱わない（手で書き換えた設定ファイルで起こりうる）。
fn audio_input_is_selected(input: Option<&str>) -> bool {
    input.is_some_and(|name| !name.is_empty())
}

/// 設定で選ばれている映像デバイスの名前。未指定（`None`、空文字も含む）なら
/// `None` で、そのときは開いているストリームを閉じて待つ（#334、音声の #304 と揃える）。
fn selected_video_device(name: Option<&str>) -> Option<&str> {
    name.filter(|name| !name.is_empty())
}

/// 列挙の結果を 1 経路ぶんだけログへ出す。**台数と名前を必ず並べる。**
/// 0 台なのか、名前が食い違っているのかをログだけで見分けるため。
fn log_listing<E: Display>(trigger: &str, source: &str, result: &Result<Vec<String>, E>) {
    match result {
        Ok(names) => info!(
            "デバイスの列挙（{}）: {}: {} 台 {:?}",
            trigger,
            source,
            names.len(),
            names
        ),
        Err(e) => warn!("デバイスを列挙できない（{}）: {}: {}", trigger, source, e),
    }
}

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
            if let Some((name, _)) = self.video.list_devices().into_iter().next() {
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
        let (_, resolution, format, fps, backend) = config.video.clone();
        let Some(device_name) = selected_video_device(config.video.0.as_deref()) else {
            self.hold_video_without_device(config);
            return;
        };

        let attempt = self.video_retry.attempts() + 1;
        info!(
            "映像デバイスへの接続を試す（{} 回目）: {}（開き方: {:?}）",
            attempt, device_name, backend
        );

        let result = self.video.start_capture(
            Some(device_name),
            resolution,
            format.as_deref(),
            fps,
            backend,
        );

        match result {
            Ok(()) => {
                info!("映像デバイスに接続した");
                self.video_retry.record_success(now);
                self.video_not_visible = None;
                self.last_video_failure = None;
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
                let reason = e.to_string();
                self.emit(DeviceEvent::VideoFailed(failure_message(
                    self.video_not_visible.as_ref(),
                    &reason,
                )));
                self.last_video_failure = Some(reason);
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
                // 映像と同じく、UI へは日本語の 1 行に落として渡す
                let reason = e.to_string();
                self.emit(DeviceEvent::AudioFailed(failure_message(
                    self.audio_not_visible.as_ref(),
                    &reason,
                )));
                self.last_audio_failure = Some(reason);
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

    /// 映像デバイスが選ばれていないので、開いているストリームを閉じて待つ（#334）。
    ///
    /// 閉じないと古い映像が映り続けたまま、設定の表示だけが「未選択」になる。
    /// 再試行はしない。扱いは音声の `hold_audio_without_input` と同じ。
    fn hold_video_without_device(&mut self, config: &DeviceConfig) {
        info!("映像デバイスが未設定なので映像を開かない");
        self.video_retry.cancel();
        if self.video.active().is_some() {
            self.video.stop_capture();
            // 最後のフレームを画面から落とし、プレースホルダーへ戻してもらう
            self.emit(DeviceEvent::VideoSignalLost);
        }
        // 同じ設定が 2 秒ごとに届いても通知を繰り返さないよう、扱い済みとして記録する
        self.last_video_target = Some(config.video.clone());
        self.video_not_visible = None;
        let reason = i18n::Text::VideoDeviceNotSelected.get().to_string();
        self.last_video_failure = Some(reason.clone());
        self.emit(DeviceEvent::VideoFailed(reason));
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

    /// 列挙の結果をログへ出し、設定のデバイスが Windows 側にも見えていないかを
    /// 判定する（#236）。`tick` のたびに呼ばれ、出す回でなければ何もしない。
    ///
    /// 出すのは起動時の 1 回と、接続の失敗が節目（5 回目・10 回目・以後 10 回
    /// ごと、`monitor::is_enumeration_milestone`）に届いた回。Media Foundation /
    /// DirectShow / 音声入力 / 音声出力を 1 行ずつ、台数と名前を並べる。
    ///
    /// **接続の試行とは切り離してある。** 再接続の前に列挙しない決まり
    /// （`docs/design/reconnect.md`）はそのままで、これは試行のあとに、ログと
    /// 判定のためだけに間引いて行う。音声の列挙は実測 300ms 前後かかるので、
    /// 毎回やると再試行の間隔を食う。
    pub(super) fn log_device_enumeration(&mut self) {
        let video_failures = self.video_retry.attempts();
        let audio_failures = self.audio_retry.attempts();
        // 繋がって 0 へ戻ったら記録も戻す。また失敗が続いたら同じ節目で出す
        if video_failures == 0 {
            self.enumeration_logged_failures.0 = 0;
        }
        if audio_failures == 0 {
            self.enumeration_logged_failures.1 = 0;
        }
        let (video_logged, audio_logged) = self.enumeration_logged_failures;
        let video_due = should_log_enumeration(video_failures, video_logged);
        let audio_due = should_log_enumeration(audio_failures, audio_logged);
        if !self.startup_enumeration_pending && !video_due && !audio_due {
            return;
        }
        let Some(config) = self.config.clone() else {
            return;
        };
        let trigger = if self.startup_enumeration_pending {
            "起動時".to_string()
        } else {
            format!(
                "接続の失敗が続いている。映像 {} 回、音声 {} 回",
                video_failures, audio_failures
            )
        };
        self.startup_enumeration_pending = false;
        self.enumeration_logged_failures = (video_failures, audio_failures);

        let video = self.video.enumerate();
        for (source, result) in &video.sources {
            log_listing(&trigger, source, result);
        }
        let input = self.audio.enumerate_devices(AudioDirection::Input);
        log_listing(&trigger, "音声入力", &input);
        let output = self.audio.enumerate_devices(AudioDirection::Output);
        log_listing(&trigger, "音声出力", &output);

        let video_notice = decide_device_not_visible(
            video_failures,
            config.video.0.as_deref(),
            video.selectable.as_deref(),
        );
        // 入力を先に見る。キャプチャーボードの音声は入力側に出る。
        // **入出力のどちらかの列挙に失敗したら、音声は判定しない。** 映像で
        // 失敗した経路があれば判定しないのと同じで、開けない理由が失敗した側に
        // あったかもしれない
        let audio_notice = match (input.as_deref(), output.as_deref()) {
            (Ok(input), Ok(output)) => {
                decide_device_not_visible(audio_failures, config.audio.0.as_deref(), Some(input))
                    .or_else(|| {
                        decide_device_not_visible(
                            audio_failures,
                            config.audio.1.as_deref(),
                            Some(output),
                        )
                    })
            }
            _ => None,
        };
        self.set_video_not_visible(video_notice);
        self.set_audio_not_visible(audio_notice);
    }

    /// 映像の「Windows 側にも見えていない」を更新する。
    ///
    /// 新しく付いたら、直近の失敗の理由に添えて**その場で出し直す。**
    /// 次の失敗は最大 5 秒後で、それを待つと案内が遅れる。
    fn set_video_not_visible(&mut self, notice: Option<DeviceNotVisible>) {
        if notice == self.video_not_visible {
            return;
        }
        match &notice {
            Some(notice) => warn!(
                "設定の映像デバイスが Windows 側にも見えていない: {:?}",
                notice
            ),
            None => debug!("設定の映像デバイスが列挙に戻った"),
        }
        self.video_not_visible = notice;
        if let (Some(notice), Some(reason)) = (&self.video_not_visible, &self.last_video_failure) {
            let message = failure_message(Some(notice), reason);
            self.emit(DeviceEvent::VideoFailed(message));
        }
    }

    /// 音声の「Windows 側にも見えていない」を更新する。映像と同じ扱い。
    fn set_audio_not_visible(&mut self, notice: Option<DeviceNotVisible>) {
        if notice == self.audio_not_visible {
            return;
        }
        match &notice {
            Some(notice) => warn!(
                "設定の音声デバイスが Windows 側にも見えていない: {:?}",
                notice
            ),
            None => debug!("設定の音声デバイスが列挙に戻った"),
        }
        self.audio_not_visible = notice;
        if let (Some(notice), Some(reason)) = (&self.audio_not_visible, &self.last_audio_failure) {
            let message = failure_message(Some(notice), reason);
            self.emit(DeviceEvent::AudioFailed(message));
        }
    }

    /// デバイス一覧を取り直して UI スレッドへ返す。
    pub(super) fn refresh_device_lists(&mut self) {
        let video = self.video.list_devices();
        let input = self.audio.list_input_devices();
        let output = self.audio.list_output_devices();
        self.emit(DeviceEvent::DeviceLists {
            video,
            input,
            output,
        });
    }

    /// 映像デバイスの対応形式を問い合わせる。
    ///
    /// 経路は開くときと同じ規則で決まる（`backend::system` の `route_for`）。
    /// DirectShow で開く設定なら、選択肢も DirectShow 側の対応形式になる（#249）
    pub(super) fn query_video_capabilities(
        &mut self,
        device: String,
        backend: VideoBackendSetting,
    ) {
        let started = Instant::now();
        // 設定ダイアログの能力キャッシュは理由を画面に出すだけなので、
        // ここで日本語の 1 行へ落として渡す
        let result = self.video.capabilities(Some(&device), backend);
        match &result {
            Ok(caps) => info!(
                "デバイス能力を取得した: {}（開き方 {:?}、{} フォーマット, {} ms）",
                device,
                backend,
                caps.len(),
                started.elapsed().as_millis()
            ),
            Err(e) => warn!(
                "デバイス能力を取得できない: {}（開き方 {:?}）: {}",
                device, backend, e
            ),
        }
        self.emit(DeviceEvent::VideoCapabilities(
            device,
            backend,
            Box::new(result.map_err(|e| e.to_string())),
        ));
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
    use super::super::worker_loop::testing::{apply_config, config_for, drain, mock_state};
    use super::*;
    use std::time::Duration;

    // 「Windows 側にも見えていない」の案内（#236）。モックの列挙結果で
    // 設定の名前が無い状態を作り、`tick` を回して失敗の理由に案内が付くかを見る。
    // 時刻はバックオフの上限（5 秒）より大きく進めて、毎回 1 回ずつ試させる

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

    /// 映像がずっと失敗するモックで、`ticks` 回だけ接続を試させる。
    fn run_failing_video(
        listed: Vec<(String, String)>,
        ticks: u32,
    ) -> (Vec<DeviceEvent>, MockVideoBackend) {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state.failures_before_success = u32::MAX;
            state.devices = listed;
        });
        audio.with(|state| state.input_devices = vec!["モック入力".to_string()]);
        let (mut state, events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), Some("モック入力")),
            true,
        );

        let base = Instant::now();
        for step in 0..ticks {
            state.tick(base + Duration::from_secs(6) * step);
        }
        (drain(&events), video)
    }

    #[test]
    fn worker_notices_a_video_device_missing_from_the_enumeration_after_5_failures() {
        let (events, video) = run_failing_video(vec![("Web カメラ".to_string(), String::new())], 5);
        assert_eq!(video.with(|state| state.start_calls), 5);

        let reasons = video_failures(&events);
        // 4 回目までは素の理由だけ。抜いた直後は見えていなくて当然
        assert!(
            reasons[..4]
                .iter()
                .all(|reason| !reason.contains("Windows 側にも")),
            "{reasons:?}"
        );
        // 5 回目の列挙で判定し、次の失敗を待たずに案内付きで出し直す
        let last = reasons.last().expect("失敗が届いていること");
        assert!(
            last.contains("'キャプチャーボード' が Windows 側にも"),
            "{last}"
        );
        assert!(last.contains("ほかのデバイスは見えています"), "{last}");
        // 元の理由も残す
        assert!(last.contains("見つからない"), "{last}");
    }

    #[test]
    fn worker_notices_no_devices_at_all_with_a_different_message() {
        let (events, _video) = run_failing_video(Vec::new(), 5);
        let reasons = video_failures(&events);
        let last = reasons.last().expect("失敗が届いていること");
        assert!(last.contains("1 台も見えていません"), "{last}");
    }

    #[test]
    fn worker_keeps_the_notice_on_later_failures() {
        // 一度付いた案内は、以後の失敗にも添え続ける（繋がるまで）
        let (events, _video) = run_failing_video(Vec::new(), 7);
        let reasons = video_failures(&events);
        assert!(
            reasons
                .last()
                .is_some_and(|reason| reason.contains("Windows 側にも")),
            "{reasons:?}"
        );
    }

    #[test]
    fn worker_does_not_notice_a_device_that_the_enumeration_lists() {
        // OS には見えているのに開けない。デバイスマネージャーを案内しても的外れ
        let (events, _video) =
            run_failing_video(vec![("キャプチャーボード".to_string(), String::new())], 10);
        assert!(
            video_failures(&events)
                .iter()
                .all(|reason| !reason.contains("Windows 側にも")),
            "列挙に居るなら案内しないこと"
        );
    }

    #[test]
    fn worker_logs_the_enumeration_once_per_milestone() {
        // tick は 100ms ごとに回るが、同じ失敗回数のまま列挙し直さない
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| state.failures_before_success = u32::MAX);
        let (mut state, _events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), None),
            true,
        );

        let base = Instant::now();
        state.tick(base);
        assert!(
            !state.startup_enumeration_pending,
            "起動時の列挙は最初の tick で済ませること"
        );
        for step in 1..5 {
            state.tick(base + Duration::from_secs(6) * step);
        }
        assert_eq!(state.enumeration_logged_failures.0, 5);
        // 同じ 5 回のまま tick だけ進んでも、記録は変わらない
        state.tick(base + Duration::from_secs(24) + Duration::from_millis(100));
        assert_eq!(state.enumeration_logged_failures.0, 5);
    }

    #[test]
    fn worker_reopens_the_video_when_only_the_backend_changes() {
        // 同じデバイスでも開き方を変えたら開き直す（#237）。変えなければ
        // 2 秒ごとの `apply_settings` で開き直さない
        use crate::settings::VideoBackendSetting;

        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, _events) = mock_state(&video, &audio);
        let config = config_for(Some("キャプチャーボード"), None);
        apply_config(&mut state, config.clone(), true);
        let base = Instant::now();
        state.tick(base);
        assert_eq!(video.with(|state| state.start_calls), 1);
        assert_eq!(
            video.with(|state| state.last_backend),
            Some(VideoBackendSetting::Auto)
        );

        // 同じ設定をもう一度受け取っても開き直さない
        apply_config(&mut state, config.clone(), false);
        state.tick(base + Duration::from_millis(100));
        assert_eq!(video.with(|state| state.start_calls), 1);

        let mut direct_show = config;
        direct_show.video.4 = VideoBackendSetting::DirectShow;
        apply_config(&mut state, direct_show, false);
        // 接続に成功してからの下限（1 秒）が過ぎてから開き直す
        state.tick(base + Duration::from_secs(2));
        assert_eq!(video.with(|state| state.start_calls), 2);
        assert_eq!(
            video.with(|state| state.last_backend),
            Some(VideoBackendSetting::DirectShow)
        );
    }

    #[test]
    fn audio_input_is_selected_only_with_a_name() {
        assert!(audio_input_is_selected(Some("キャプチャーボード")));
        assert!(!audio_input_is_selected(None));
        assert!(!audio_input_is_selected(Some("")));
    }

    #[test]
    fn selected_video_device_only_with_a_name() {
        assert_eq!(selected_video_device(Some("カメラ")), Some("カメラ"));
        assert_eq!(selected_video_device(None), None);
        assert_eq!(selected_video_device(Some("")), None);
    }

    #[test]
    fn query_video_capabilities_asks_the_route_for_the_given_backend() {
        // 開き方を DirectShow にした設定なら、選択肢も DirectShow 側の対応形式に
        // する（#249）。開き方はイベントにも添えて返し、UI のキャッシュのキーにする
        use crate::video::capabilities::FormatCapability;
        use crate::video::VideoMode;

        let media_foundation = vec![FormatCapability::new(
            "YUY2",
            vec![VideoMode::new(1920, 1080, 60)],
        )];
        let direct_show = vec![FormatCapability::new(
            "MJPEG",
            vec![VideoMode::new(1280, 720, 30)],
        )];
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state
                .capabilities
                .insert(VideoBackendSetting::MediaFoundation, media_foundation);
            state
                .capabilities
                .insert(VideoBackendSetting::DirectShow, direct_show.clone());
        });
        let (mut state, events) = mock_state(&video, &audio);

        state.query_video_capabilities(
            "キャプチャーボード".to_string(),
            VideoBackendSetting::DirectShow,
        );

        assert_eq!(
            video.with(|state| state.last_capabilities_backend),
            Some(VideoBackendSetting::DirectShow)
        );
        let replies: Vec<_> = drain(&events)
            .into_iter()
            .filter_map(|event| match event {
                DeviceEvent::VideoCapabilities(device, backend, result) => {
                    Some((device, backend, *result))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            vec![(
                "キャプチャーボード".to_string(),
                VideoBackendSetting::DirectShow,
                Ok(direct_show)
            )]
        );
    }

    #[test]
    fn failure_message_puts_the_notice_first() {
        // トーストは 60 文字で切るので、案内を前に置く
        let message = failure_message(Some(&DeviceNotVisible::NoDevices), "理由");
        assert!(message.starts_with("Windows 側にも"), "{message}");
        assert!(message.ends_with("（理由）"), "{message}");
        assert_eq!(failure_message(None, "理由"), "理由");
    }
}
