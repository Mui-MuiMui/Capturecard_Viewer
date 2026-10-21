//! デバイスを開く・閉じる・列挙する・能力を問い合わせる処理。
//!
//! どれもデバイスワーカースレッド（`super::worker_loop`）の上でだけ走る。
//! `WorkerState` に生やす形にしてあるのは、`CaptureCardViewer` を
//! `app` の子モジュールで分担しているのと同じ理由で、状態を 1 つに保ったまま
//! 役割ごとにファイルを分けるため。音声を開く側（`try_connect_audio` など）と
//! 音声の対応設定の問い合わせは `super::worker_audio_connect` に置く。
//!
//! **ここに時計やタイマーを置かない。** 「いつ試すか」は
//! `super::retry::ConnectRetry`、「途絶したか」は `super::monitor` が決める。

use super::backend::CaptureRequest;
use super::monitor::{decide_device_not_visible, should_log_enumeration, DeviceNotVisible};
use super::retry::{
    backoff_delay, connect_attempt_log_level, connect_failure_log_level, enumeration_log_level,
    enumeration_text, is_retry_log_milestone,
};
use super::worker::{DeviceConfig, DeviceEvent, VideoTarget};
use super::worker_loop::WorkerState;
use crate::audio::AudioDirection;
use crate::i18n;
use crate::settings::{AudioInputSource, VideoBackendSetting};
use log::{debug, info, log, warn};
use std::fmt::Display;
use std::time::Instant;

/// 接続の失敗を UI へ渡す 1 行。「Windows 側にも見えていない」と判定済みなら
/// 案内を添える。
pub(super) fn failure_message(not_visible: Option<&DeviceNotVisible>, reason: &str) -> String {
    match not_visible {
        Some(notice) => i18n::failure_with_device_not_visible(notice, reason),
        None => reason.to_string(),
    }
}

/// 設定で選ばれている映像デバイスの名前。未指定（`None`、空文字も含む）なら
/// `None` で、そのときは開いているストリームを閉じて待つ（#334、音声の #304 と揃える）。
fn selected_video_device(name: Option<&str>) -> Option<&str> {
    name.filter(|name| !name.is_empty())
}

impl WorkerState {
    /// 未設定の映像デバイス名を、列挙結果の先頭で埋める。**起動直後の 1 回だけ。**
    ///
    /// **入力はここで決めない。** 最初の映像の試行のあとに、映像に音声ピンが
    /// あるかを見て決める（`worker_audio_connect` の `settle_default_input`、#394）。
    /// 入力を未設定のままにしない理由（既定の入力は内蔵マイクになりうる）はそちら。
    /// 出力は `None`（Windows の既定デバイス）のままにする。
    ///
    /// 決めた名前は `DefaultDevicesResolved` で UI スレッドへ返し、設定へ
    /// 書き戻してもらう。**返す前にこの場の `config` も書き換える。**
    /// 往復を待つと、最初の接続がその分だけ遅れる。
    ///
    /// **映像の名前を埋めたら、解像度は未指定にする**（#391）。設定の解像度は
    /// 既定の 1280x720 で、このデバイスに合わせて選んだ値ではない。入力信号と
    /// 違う解像度で開くと警告画面しか出さないボード（AVerMedia GC551）では、
    /// それで開くと「繋がっているのに映らない」になる。未指定なら DirectShow は
    /// デバイスのいまの解像度で開き、Media Foundation はこれまでどおり 1280x720 を
    /// 要求する。開いた解像度は接続後に `VideoResolutionResolved` で返す。
    pub(super) fn resolve_default_devices(&mut self, config: &mut DeviceConfig) {
        // 出力は埋めない。`None` のまま Windows の既定デバイスへ任せる
        if config.audio.1.is_none() {
            debug!("出力デバイスは既定（自動選択）にする");
        }
        if config.video.0.is_some() {
            return;
        }
        let Some((name, _)) = self.video.list_devices().into_iter().next() else {
            return;
        };
        info!(
            "映像デバイスの既定を {} にし、解像度はデバイスに合わせる",
            name
        );
        config.video.0 = Some(name.clone());
        config.video.1 = None;
        self.emit(DeviceEvent::DefaultDevicesResolved {
            video: Some(name),
            input: None,
            input_source: None,
        });
    }

    /// 映像デバイスへの接続を 1 回だけ試す。試したあとで、初回の入力の既定を
    /// 決める（開けたか・音声ピンがあるかで決まる。`settle_default_input`、#394）。
    pub(super) fn try_connect_video(&mut self, config: &DeviceConfig, now: Instant) {
        self.connect_video_once(config, now);
        self.settle_default_input();
    }

    fn connect_video_once(&mut self, config: &DeviceConfig, now: Instant) {
        let (_, resolution, format, fps, backend, connect_audio_pin) = config.video.clone();
        let Some(device_name) = selected_video_device(config.video.0.as_deref()) else {
            self.hold_video_without_device(config);
            return;
        };

        let attempt = self.video_retry.attempts() + 1;
        // 繋がらない間は毎回出さない（#506、`retry::is_retry_log_milestone`）
        log!(
            connect_attempt_log_level(attempt),
            "映像デバイスへの接続を試す（{} 回目）: {}（開き方: {:?}）",
            attempt,
            device_name,
            backend
        );
        let started = Instant::now();

        let result = self.video.start_capture(&CaptureRequest {
            device_name: Some(device_name),
            resolution,
            format: format.as_deref(),
            fps,
            backend,
            connect_audio_pin,
        });

        match result {
            Ok(()) => {
                // 何回目で繋がったかと、この試行にかかった時間は必ず残す。
                // 「試す」の行は DEBUG に落ちていることがあるため（#506）
                info!(
                    "映像デバイスに接続した（{} 回目、{:.1} ms）",
                    attempt,
                    started.elapsed().as_secs_f32() * 1000.0
                );
                self.video_retry.record_success(now);
                self.video_not_visible = None;
                self.last_video_failure = None;
                self.last_video_target = Some(config.video.clone());
                self.emit(DeviceEvent::VideoConnected);
                self.report_resolved_resolution(&config.video);
                // 途絶から復帰したのであれば、音声も同時に戻っているはず。
                // **旗はここで落とす。** 残すと、以降の接続のたびに音声を
                // 開き直してしまう
                let recovered = std::mem::take(&mut self.video_reconnect_after_loss);
                self.resync_audio_after_video_recovery(config, recovered);
            }
            Err(e) => {
                // UI へは日本語の 1 行に落として渡す。`DeviceEvent` に種別を
                // 載せても、いまの再試行は理由で戦略を変えないため
                let reason = e.to_string();
                // 同じ理由が続く間は節目の回だけ WARN にする（#506）。比べる相手は
                // 直近の失敗の理由で、繋がったら消えるので、次の失敗の 1 回目は必ず出る
                let reason_changed = self.last_video_failure.as_deref() != Some(reason.as_str());
                log!(
                    connect_failure_log_level(attempt, reason_changed),
                    "映像デバイスへの接続に失敗した（{} 回目）: {}",
                    attempt,
                    reason
                );
                self.video_retry.record_failure(now);
                // **開けなかったら、開いている相手は無い。** `start_capture` は開く前に
                // 古いストリームを閉じている。前の設定を残すと、元へ戻したときに差分が
                // 立たず、この失敗で伸びたバックオフを待ってから開くことになる（#311）
                self.last_video_target = None;
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

    /// 解像度が未指定の要求で開けたら、実際に開いた解像度を UI へ返す（#391）。
    ///
    /// **返す前に、この場の設定と「開いている相手」も開いた解像度にする。**
    /// UI が書き戻した設定（解像度あり）が届いたときに、差分ありとみなして
    /// 開き直さないため。書き戻す前の設定（解像度なし）が遅れて届いた場合は
    /// `worker_commands` の `carry_resolved_resolution` が引き継ぐ。
    fn report_resolved_resolution(&mut self, target: &VideoTarget) {
        if target.1.is_some() {
            return;
        }
        let Some(resolution) = self.video.active().and_then(|active| active.resolution) else {
            return;
        };
        info!(
            "解像度が未指定だったので、開いた解像度 {}x{} を設定へ書き戻してもらう",
            resolution.0, resolution.1
        );
        let mut resolved = target.clone();
        resolved.1 = Some(resolution);
        if let Some(config) = self
            .config
            .as_mut()
            .filter(|config| config.video == *target)
        {
            config.video = resolved.clone();
        }
        self.last_video_target = Some(resolved);
        self.emit(DeviceEvent::VideoResolutionResolved {
            target: target.clone(),
            resolution,
        });
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

    /// 列挙の結果を 1 経路ぶんだけログへ出し、出したものを覚えておく。
    fn log_listing<E: Display>(
        &mut self,
        milestone: bool,
        trigger: &str,
        source: &'static str,
        result: &Result<Vec<String>, E>,
    ) {
        let (text, listed) = enumeration_text(result);
        let changed = self.last_enumeration_listing.get(source) != Some(&text);
        let level = enumeration_log_level(listed, milestone, changed);
        let what = if listed {
            "デバイスの列挙"
        } else {
            "デバイスを列挙できない"
        };
        log!(level, "{}（{}）: {}: {}", what, trigger, source, text);
        self.last_enumeration_listing.insert(source, text);
    }

    /// 列挙の結果をログへ出し、設定のデバイスが Windows 側にも見えていないかを
    /// 判定する（#236）。`tick` のたびに呼ばれ、出す回でなければ何もしない。
    ///
    /// 出すのは起動時の 1 回と、接続の失敗が節目（5 回目・10 回目・以後 10 回
    /// ごと、`monitor::is_enumeration_milestone`）に届いた回。Media Foundation /
    /// DirectShow / 音声入力 / 音声出力を 1 行ずつ、台数と名前を並べる。
    /// INFO で出すのは起動時と、再試行のログの節目（`retry::is_retry_log_milestone`）と、
    /// 前回出したものから変わった経路だけで、残りは DEBUG に落とす（#506）。
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
        // 列挙と判定はこれまでどおり 10 回ごとに行い、INFO で出すのは再試行の
        // ログと同じ節目に絞る（#506）。起動時は前回の記録が無いので必ず出る
        let milestone = (video_due && is_retry_log_milestone(video_failures))
            || (audio_due && is_retry_log_milestone(audio_failures));
        self.startup_enumeration_pending = false;
        self.enumeration_logged_failures = (video_failures, audio_failures);

        let video = self.video.enumerate();
        for (source, result) in &video.sources {
            self.log_listing(milestone, &trigger, source, result);
        }
        let input = self.audio.enumerate_devices(AudioDirection::Input);
        self.log_listing(milestone, &trigger, "音声入力", &input);
        let output = self.audio.enumerate_devices(AudioDirection::Output);
        self.log_listing(milestone, &trigger, "音声出力", &output);

        let video_notice = decide_device_not_visible(
            video_failures,
            config.video.0.as_deref(),
            video.selectable.as_deref(),
        );
        // 入力を先に見る。キャプチャーボードの音声は入力側に出る。
        // **入出力のどちらかの列挙に失敗したら、音声は判定しない。** 映像で
        // 失敗した経路があれば判定しないのと同じで、開けない理由が失敗した側に
        // あったかもしれない
        // 入力が映像デバイスの音声ピンなら、入力は判定しない。比べる WASAPI の名前が無い（#388）
        let configured_input = match config.audio.5 {
            AudioInputSource::Device => config.audio.0.as_deref(),
            AudioInputSource::VideoPin => None,
        };
        let audio_notice = match (input.as_deref(), output.as_deref()) {
            (Ok(input), Ok(output)) => decide_device_not_visible(
                audio_failures,
                configured_input,
                Some(input),
            )
            .or_else(|| {
                decide_device_not_visible(audio_failures, config.audio.1.as_deref(), Some(output))
            }),
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
        // 音声ピンの有無は一覧と同じ間隔で取り直す。分かったものは実装が覚えて
        // いるので、2 回目からはフィルターを作らない（#409）
        self.video_audio_pins = self.video.audio_pin_presence();
        self.publish_snapshot();
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
}

#[cfg(test)]
mod tests {
    use super::super::backend::mock::{MockAudioBackend, MockVideoBackend};
    use super::super::worker_loop::testing::{
        apply_config, capture_logs, config_for, drain, mock_state,
    };
    use super::*;
    use crate::video::VideoError;
    use log::Level;
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
        // 出した結果を経路ごとに覚え、次の回はそれと比べて DEBUG へ落とすか決める（#506）
        let logged = state.last_enumeration_listing.get("音声入力");
        assert_eq!(logged.map(String::as_str), Some("0 台 []"));
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

    // 切り替えに失敗したあとで元へ戻す（#311）。失敗した回は開く前に古い
    // ストリームを閉じているので、元の設定へ戻したら次の tick ですぐ開く。
    // B の失敗で伸びたバックオフは、B を選び直したときだけ効く

    #[test]
    fn worker_reopens_the_previous_video_right_after_a_failed_switch() {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        let (mut state, _events) = mock_state(&video, &audio);
        let a = config_for(Some("A"), None);
        let b = config_for(Some("B"), None);
        apply_config(&mut state, a.clone(), true);
        let base = Instant::now();
        state.tick(base);
        assert_eq!(video.with(|state| state.start_calls), 1);

        // B は 3 回続けて開けない。3 回目のあとは 800ms 待つ
        video.with(|state| state.failures_before_success = 3);
        apply_config(&mut state, b.clone(), false);
        for step in 2..5 {
            state.tick(base + Duration::from_secs(step));
        }
        assert_eq!(video.with(|state| state.start_calls), 4);
        assert!(
            !video.with(|state| state.capturing),
            "B の失敗で A も閉じている"
        );

        // A へ戻すと、B のバックオフ（+4.8 秒まで）を待たずに次の tick で開く
        apply_config(&mut state, a.clone(), false);
        let back = base + Duration::from_millis(4_100);
        state.tick(back);
        assert_eq!(video.with(|state| state.start_calls), 5);
        assert_eq!(
            video.with(|state| state.last_device_name.clone()),
            Some("A".to_string())
        );
        assert!(video.with(|state| state.capturing));

        // 2 秒ごとの再適用では開き直さない
        apply_config(&mut state, a, false);
        state.tick(back + Duration::from_secs(2));
        assert_eq!(video.with(|state| state.start_calls), 5);

        // B を選び直すと即座に試し、失敗したら同じ B の再適用ではバックオフを守る
        video.with(|state| state.failures_before_success = u32::MAX);
        let again = back + Duration::from_secs(4);
        apply_config(&mut state, b.clone(), false);
        state.tick(again);
        assert_eq!(video.with(|state| state.start_calls), 6);
        apply_config(&mut state, b, false);
        state.tick(again + Duration::from_millis(100));
        assert_eq!(video.with(|state| state.start_calls), 6, "200ms 待つ");
        state.tick(again + Duration::from_millis(200));
        assert_eq!(video.with(|state| state.start_calls), 7);
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

    // 初回に映像デバイスを埋めたときの解像度（#391）。設定の 1280x720 は既定値で、
    // 入力と違う解像度では警告画面しか出さないボードがあるので、未指定で開いて
    // 開いた解像度を UI へ返す

    /// 設定ファイルが無い初回の設定（デバイス未選択、解像度は既定の 1280x720）
    fn first_run_config() -> DeviceConfig {
        let mut config = config_for(None, Some("モック入力"));
        config.video.1 = Some((1280, 720));
        config.video.2 = Some("YUY2".to_string());
        config.video.3 = Some(60);
        config
    }

    fn resolved_resolutions(events: &[DeviceEvent]) -> Vec<(VideoTarget, (u32, u32))> {
        events
            .iter()
            .filter_map(|event| match event {
                DeviceEvent::VideoResolutionResolved { target, resolution } => {
                    Some((target.clone(), *resolution))
                }
                _ => None,
            })
            .collect()
    }

    fn first_run_state() -> (
        WorkerState,
        std::sync::mpsc::Receiver<DeviceEvent>,
        MockVideoBackend,
    ) {
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state.devices = vec![("キャプチャーボード".to_string(), String::new())];
            state.opened_resolution = Some((1920, 1080));
        });
        audio.with(|state| state.input_devices = vec!["モック入力".to_string()]);
        let (state, events) = mock_state(&video, &audio);
        (state, events, video)
    }

    #[test]
    fn first_run_opens_the_resolved_video_device_without_a_resolution() {
        let (mut state, events, _video) = first_run_state();
        apply_config(&mut state, first_run_config(), true);

        let config = state.config.as_ref().expect("設定を覚えていること");
        assert_eq!(config.video.0.as_deref(), Some("キャプチャーボード"));
        assert_eq!(config.video.1, None, "解像度はデバイスに任せること");
        // 形式と fps は触らない
        assert_eq!(config.video.2.as_deref(), Some("YUY2"));
        assert_eq!(config.video.3, Some(60));

        // 返すイベントは開いたときの接続対象（解像度なし）を運ぶ。UI はこれと
        // 設定を突き合わせてから書き戻す
        let opened = config.video.clone();
        state.tick(Instant::now());
        let events = drain(&events);
        assert_eq!(resolved_resolutions(&events), vec![(opened, (1920, 1080))]);
        let config = state.config.as_ref().expect("設定を覚えていること");
        assert_eq!(config.video.1, Some((1920, 1080)));
        assert_eq!(state.last_video_target.as_ref(), Some(&config.video));
    }

    #[test]
    fn first_run_does_not_reopen_for_the_written_back_or_stale_settings() {
        let (mut state, events, video) = first_run_state();
        apply_config(&mut state, first_run_config(), true);
        let base = Instant::now();
        state.tick(base);
        assert_eq!(video.with(|state| state.start_calls), 1);
        drain(&events);

        // UI が書き戻す前の設定（解像度なし）が遅れて届いても開き直さない
        let mut stale = first_run_config();
        stale.video.0 = Some("キャプチャーボード".to_string());
        stale.video.1 = None;
        apply_config(&mut state, stale.clone(), false);
        // 書き戻したあとの設定（開いた解像度）でも開き直さない
        let mut written = stale;
        written.video.1 = Some((1920, 1080));
        apply_config(&mut state, written, false);
        state.tick(base + Duration::from_secs(6));
        assert_eq!(video.with(|state| state.start_calls), 1);
        assert!(resolved_resolutions(&drain(&events)).is_empty());
    }

    #[test]
    fn explicit_resolution_is_not_reported_back() {
        // 利用者が選んだ解像度で開いたときは書き戻さない
        let (mut state, events, _video) = first_run_state();
        let mut config = first_run_config();
        config.video.0 = Some("キャプチャーボード".to_string());
        apply_config(&mut state, config, true);
        assert_eq!(
            state.config.as_ref().map(|config| config.video.1),
            Some(Some((1280, 720)))
        );
        state.tick(Instant::now());
        assert!(resolved_resolutions(&drain(&events)).is_empty());
    }

    /// `needle` を含むログ行のレベルを、出た順に並べる。
    fn levels_of(logs: &[(Level, String)], needle: &str) -> Vec<Level> {
        logs.iter()
            .filter(|(_, line)| line.contains(needle))
            .map(|(level, _)| *level)
            .collect()
    }

    #[test]
    fn worker_logs_repeated_failures_at_debug_and_changes_loudly() {
        // 繋がらないまま tick を回し、同じ理由の失敗は節目だけ WARN、理由が
        // 変わった回は節目でなくても WARN、列挙は結果が変わった経路だけ節目の
        // 外でも INFO になることを、ログのレベルで確かめる（#506）
        let video = MockVideoBackend::default();
        let audio = MockAudioBackend::default();
        video.with(|state| {
            state.failures_before_success = u32::MAX;
            state.devices = vec![("Web カメラ".to_string(), String::new())];
        });
        let (mut state, _events) = mock_state(&video, &audio);
        apply_config(
            &mut state,
            config_for(Some("キャプチャーボード"), None),
            true,
        );

        let base = Instant::now();
        let logs = capture_logs(|| {
            for step in 0..120 {
                let attempt = step + 1;
                if attempt == 7 {
                    // 「見つからない」から「開けない」へ理由が変わる
                    video.with(|state| {
                        state.failure = Some(VideoError::CameraOpenFailed {
                            device: "キャプチャーボード".to_string(),
                            source: "使用中".to_string(),
                        })
                    });
                }
                if attempt == 105 {
                    // 101〜109 回目は節目ではないが、110 回目の列挙で結果が変わっている
                    video.with(|state| {
                        state
                            .devices
                            .push(("キャプチャーボード".to_string(), String::new()))
                    });
                }
                state.tick(base + Duration::from_secs(6) * step);
            }
        });
        assert_eq!(video.with(|state| state.start_calls), 120);

        let failed = |n: u32| levels_of(&logs, &format!("接続に失敗した（{n} 回目）"));
        assert_eq!(failed(1), vec![Level::Warn]);
        assert_eq!(failed(4), vec![Level::Debug], "同じ理由が続く間は DEBUG");
        assert_eq!(failed(5), vec![Level::Warn], "節目は WARN");
        assert_eq!(failed(6), vec![Level::Debug]);
        assert_eq!(failed(7), vec![Level::Warn], "理由が変わった回は WARN");
        assert_eq!(failed(8), vec![Level::Debug]);
        assert_eq!(failed(110), vec![Level::Debug]);
        let tried = |n: u32| levels_of(&logs, &format!("接続を試す（{n} 回目）"));
        assert_eq!(tried(4), vec![Level::Debug]);
        assert_eq!(tried(10), vec![Level::Info]);

        // 列挙は 10 回ごとに行い、100 回目より後は変わった経路だけ INFO。
        // 経路はモックの映像（`list_devices`）と音声入力で見分ける
        let listing = |n: u32, source: &str| {
            levels_of(&logs, &format!("映像 {n} 回、音声 0 回）: {source}:"))
        };
        assert_eq!(listing(100, "list_devices"), vec![Level::Info], "節目");
        assert_eq!(
            listing(110, "list_devices"),
            vec![Level::Info],
            "結果が変わった"
        );
        assert_eq!(listing(110, "音声入力"), vec![Level::Debug], "変わらない");
        assert_eq!(listing(120, "list_devices"), vec![Level::Debug]);
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
