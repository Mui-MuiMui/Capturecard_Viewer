//! デバイスワーカースレッドの本体。
//!
//! デバイスを開く・閉じる・列挙する・能力を問い合わせる処理は、すべてこの
//! スレッドの上で起きる。UI スレッドは `super::worker::DeviceCommand` を
//! 送るだけなので、数百 ms かかる `start_capture` / `start_passthrough` で
//! ウィンドウが固まらない。
//!
//! **再試行と監視のタイマーもここで回す。** `update()` から駆動していた頃は、
//! 最小化している間 eframe が再描画要求を捨てるために自動再接続も切断監視も
//! 止まっていた（#133）。このスレッドはウィンドウの状態に関係なく動く。
//!
//! 判定そのもの（途絶したか、開き直してよいか）は `super::monitor` の
//! 純粋関数に切り出してある。ここはデバイスを触る側だけを持つ。

use super::audio_control::volume_change_result;
use super::monitor::{
    decide_audio_reconnect, decide_video_link, default_audio_device_changed,
    should_poll_default_audio_device, AudioErrorAction, VideoLinkAction, VIDEO_SIGNAL_TIMEOUT,
};
use super::retry::ConnectRetry;
use super::worker::{
    AudioTarget, DeviceCommand, DeviceConfig, DeviceEvent, DeviceSnapshot, RetryStatus,
    SharedSnapshot, VideoTarget,
};
use crate::audio::{self, AudioCapabilities, AudioCapture, AudioControls, AudioDirection};
use crate::repaint::RepaintWaker;
use crate::video::{SharedColorConversion, VideoCapture, VideoFrames};
use log::{debug, info, trace, warn};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 接続を追いかけている間、期限を見に戻る間隔。
///
/// バックオフの最小値が 200ms なので、それより細かく起きる意味はない。
/// 100ms なら 1 回目の再試行は 200〜300ms の間に入る。
const RETRY_TICK: Duration = Duration::from_millis(100);

/// 何も追いかけていないときに、期限を見に戻る間隔。
///
/// ここで見るのはフレームの途絶（3 秒）と音声ストリームのエラー旗、
/// 既定デバイスの切り替え（4 秒）だけなので、500ms で足りる。
/// **短くしても得るものが無く、待機中の消費電力だけが増える。**
const IDLE_TICK: Duration = Duration::from_millis(500);

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

/// 次にコマンドを待つ時間を決める。
///
/// 接続を追いかけている間だけ細かく起きる。判定を関数にしてあるのは、
/// スレッドを起こさずにテストできるようにするため。
fn next_tick_delay(retrying: bool) -> Duration {
    if retrying {
        RETRY_TICK
    } else {
        IDLE_TICK
    }
}

/// ワーカースレッドの入口。`DeviceCommand::Shutdown` か、コマンドの送り手が
/// 居なくなるまで回り続ける。
pub(super) fn run(
    commands: Receiver<DeviceCommand>,
    events: Sender<DeviceEvent>,
    snapshot: SharedSnapshot,
    frames: VideoFrames,
    color_conversion: Arc<SharedColorConversion>,
    audio_controls: Arc<AudioControls>,
    repaint_waker: RepaintWaker,
) {
    debug!("デバイスワーカーを開始する");
    let mut state = WorkerState::new(
        events,
        snapshot,
        frames,
        color_conversion,
        audio_controls,
        repaint_waker,
    );

    loop {
        // **コマンドを受ける前に期限を片付ける。** 起動直後は
        // ApplyConfig → （接続）→ QueryVideoCapabilities の順で処理したい。
        // 逆にすると、数百 ms かかる能力取得の後ろで最初の接続が待たされる
        state.tick(Instant::now());
        state.publish_snapshot();

        let timeout = next_tick_delay(state.is_retrying());
        match commands.recv_timeout(timeout) {
            Ok(DeviceCommand::Shutdown) => {
                debug!("デバイスワーカーの停止を受け取った");
                break;
            }
            Ok(command) => state.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // 送り手が落ちた。`on_exit` を通らずにアプリが終わる経路
                debug!("デバイスワーカーへのコマンド送信元が無くなったので終わる");
                break;
            }
        }
    }

    state.shutdown();
    debug!("デバイスワーカーを終了した");
}

/// ワーカースレッドだけが触る状態。
///
/// **`pub(super)` にしてあるのは、デバイスを開く処理を
/// `super::worker_connect` へ分けているため。** `app` の外からは見えない。
pub(super) struct WorkerState {
    pub(super) video: VideoCapture,
    pub(super) audio: AudioCapture,
    /// 音量・ミュート・パススルーの共有 Atomic。
    ///
    /// 普段は UI スレッドが書き、出力コールバックが読むだけで、ワーカーは
    /// `AudioCapture` へ渡すためだけに触っていた。**最小化中のホットキーを
    /// 代わりに実行するために、ここでも複製を持つ**（#133）
    audio_controls: Arc<AudioControls>,
    pub(super) events: Sender<DeviceEvent>,
    pub(super) snapshot: SharedSnapshot,
    /// イベントを積んだときに UI スレッドを起こす窓口。
    /// 間隔を広げているときだけ効く（`repaint::should_wake_on_event`）
    pub(super) repaint_waker: RepaintWaker,

    /// 最後に受け取った設定。まだ一度も来ていなければ `None`
    pub(super) config: Option<DeviceConfig>,

    pub(super) video_retry: ConnectRetry<VideoTarget>,
    pub(super) audio_retry: ConnectRetry<AudioTarget>,

    /// 最後に接続できた対象。**開き直しが要るかの差分判定はワーカーが持つ。**
    /// UI 側に置くと、接続の成否を知っているのはワーカーなのに記録は UI、
    /// という分かれ方になる
    pub(super) last_video_target: Option<VideoTarget>,
    pub(super) last_audio_target: Option<AudioTarget>,

    /// フレームの途絶に対して最後に行った処置。
    ///
    /// 同じ判定に毎回当たるため、同じ処置を繰り返さないための番人。
    /// **「一度扱った」という真偽値ではなく「何をしたか」で持つ。**
    /// 自動再接続を切ったまま途絶したあとに有効化すると判定が変わるので、
    /// そこで開き直せる
    pub(super) last_video_link_action: VideoLinkAction,
    /// 途絶を検出して映像を開き直している最中か。
    /// 次に映像が繋がったときだけ音声の再接続も要求するための目印
    pub(super) video_reconnect_after_loss: bool,

    /// 未処理の音声ストリームのエラーがあるか。
    /// `take_stream_error` は読んだ時点で旗を下ろすため、見送ったエラーを
    /// ここへ移しておかないと、そのまま音が戻らなくなる
    pub(super) audio_stream_error_pending: bool,
    /// ストリームのエラーを理由に音声を開き直した時刻
    pub(super) last_audio_error_reconnect: Option<Instant>,
    /// Windows 側の既定デバイス名を最後に確認した時刻
    pub(super) last_default_audio_check: Option<Instant>,

    /// 音声デバイスの対応設定。開くときに要るので、ワーカー側でも持つ。
    /// 設定ダイアログ用のキャッシュ（`ui::CapabilityCache`）とは別物で、
    /// こちらは問い合わせた結果をそのまま溜めるだけ
    pub(super) audio_capabilities: HashMap<(AudioDirection, String), AudioCapabilities>,

    /// 音声のクロックドリフト補正を最後に行った時刻。
    /// `RESAMPLE_CORRECTION_INTERVAL` おきにしか動かさないための記録
    pub(super) last_resample_correction: Option<Instant>,
    /// 水位が目標から大きく外れている旨の `warn` を最後に出した時刻。
    /// 連打を防ぐための記録
    pub(super) last_resample_warn: Option<Instant>,
}

impl WorkerState {
    fn new(
        events: Sender<DeviceEvent>,
        snapshot: SharedSnapshot,
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        audio_controls: Arc<AudioControls>,
        repaint_waker: RepaintWaker,
    ) -> Self {
        Self {
            video: VideoCapture::new(frames, color_conversion, repaint_waker.clone()),
            audio: AudioCapture::new(Arc::clone(&audio_controls)),
            audio_controls,
            events,
            snapshot,
            repaint_waker,
            config: None,
            video_retry: ConnectRetry::default(),
            audio_retry: ConnectRetry::default(),
            last_video_target: None,
            last_audio_target: None,
            last_video_link_action: VideoLinkAction::Keep,
            video_reconnect_after_loss: false,
            audio_stream_error_pending: false,
            last_audio_error_reconnect: None,
            last_default_audio_check: None,
            audio_capabilities: HashMap::new(),
            last_resample_correction: None,
            last_resample_warn: None,
        }
    }

    /// 接続を追いかけている最中か。コマンドを待つ間隔の決定に使う。
    fn is_retrying(&self) -> bool {
        self.video_retry.is_active() || self.audio_retry.is_active()
    }

    /// UI スレッドへイベントを 1 つ返す。
    ///
    /// 受信側が無いのはアプリが終わったときだけなので、送れなくても捨ててよい。
    ///
    /// **送る前に観測値を書き出す。** イベントとスナップショットは別の経路で
    /// 届くため、先にイベントを送ると「接続に失敗した」を受け取った UI が
    /// 失敗前のスナップショット（再試行していない）を読みうる。
    pub(super) fn emit(&self, event: DeviceEvent) {
        self.publish_snapshot();
        if self.events.send(event).is_err() {
            trace!("デバイスイベントの送り先が既に無いので捨てる");
            return;
        }
        // 再描画の間隔を広げている間は、通知しないと最大 500ms 表示が遅れる。
        // 16ms で回っている間は `RepaintWaker` 側が無効になっているので、
        // ここを呼んでも何も起きない
        self.repaint_waker.wake();
    }

    /// 観測値を UI スレッドから読める形へ写す。
    pub(super) fn publish_snapshot(&self) {
        let next = DeviceSnapshot {
            video_capturing: self.video.link_state().capturing,
            active_video: self.video.active(),
            active_audio: self.audio.active(),
            video_retry: RetryStatus {
                active: self.video_retry.is_active(),
                attempts: self.video_retry.attempts(),
            },
            audio_retry: RetryStatus {
                active: self.audio_retry.is_active(),
                attempts: self.audio_retry.attempts(),
            },
            audio_resample: self.audio.resample_status(),
            audio_underruns: self.audio.underrun_count(),
        };
        match self.snapshot.write() {
            Ok(mut slot) => *slot = next,
            Err(_) => warn!("デバイスの観測値を書き込めない"),
        }
    }

    fn handle(&mut self, command: DeviceCommand) {
        match command {
            DeviceCommand::ApplyConfig { config, initial } => self.apply_config(*config, initial),
            DeviceCommand::ReconnectNow => self.reconnect_now(),
            DeviceCommand::RefreshDeviceLists => self.refresh_device_lists(),
            DeviceCommand::QueryVideoCapabilities(device) => self.query_video_capabilities(device),
            DeviceCommand::QueryAudioCapabilities(direction, key) => {
                self.query_audio_capabilities(direction, &key);
            }
            DeviceCommand::AdjustVolume(delta) => self.adjust_volume(delta),
            DeviceCommand::ToggleMute => self.toggle_mute(),
            // 呼び出し側（`run`）がループを抜けるので、ここへは来ない
            DeviceCommand::Shutdown => {}
        }
    }

    /// デバイスに関係する設定を受け取り、開き直しが要るものだけ要求を立てる。
    ///
    /// **ここではデバイスを開かない。** 実際に開くのは次の `tick`。要求を
    /// 立てるところと開くところを分けてあるのは、`ConnectRetry` のバックオフに
    /// 一本化するため（2 か所から開くと、同じデバイスを二重に開こうとする）。
    fn apply_config(&mut self, mut config: DeviceConfig, initial: bool) {
        trace!("デバイス設定を受け取った（起動直後: {}）", initial);

        if initial {
            self.resolve_default_devices(&mut config);
        }

        let need_video_restart = Some(&config.video) != self.last_video_target.as_ref();
        if config.video.0.is_some() && (need_video_restart || initial) {
            self.video_retry.request(config.video.clone());
        }

        let need_audio_restart = Some(&config.audio) != self.last_audio_target.as_ref() || initial;
        if need_audio_restart {
            self.audio_retry.request(config.audio.clone());
        }

        self.config = Some(config);
    }

    /// 最小化中のホットキーで音量を変える。**UI スレッドの代役。**
    ///
    /// 基準にするのは `AudioControls` に入っている値で、UI スレッドが持つ
    /// `CaptureCardViewer::volume` とは最大 0.5% ずれうる（UI 側は変化が
    /// その幅を超えたときだけ Atomic へ書く）。ずれは復帰したときの
    /// `adjust_volume` で UI 側の値へ揃うので、聞こえ方の差にはならない。
    ///
    /// 上下限とミュートの扱いは UI と同じ `volume_change_result` に任せる。
    /// ここで独自に計算すると、経路によって上限や解除の有無が変わる
    fn adjust_volume(&self, delta: f32) {
        let (volume, muted) = volume_change_result(self.audio_controls.volume_percent(), delta);
        self.audio_controls.set_volume(volume);
        self.audio_controls.set_muted(muted);
        info!("最小化中のホットキーで音量を {}% にした", volume as i32);
        self.emit(DeviceEvent::VolumeAdjusted(delta));
    }

    /// 最小化中のホットキーでミュートを切り替える。**UI スレッドの代役。**
    fn toggle_mute(&self) {
        let muted = !self.audio_controls.muted();
        self.audio_controls.set_muted(muted);
        info!(
            "最小化中のホットキーでミュートを{}にした",
            if muted { "オン" } else { "オフ" }
        );
        self.emit(DeviceEvent::MuteToggled);
    }

    /// バックオフを飛ばして映像・音声とも開き直す。
    fn reconnect_now(&mut self) {
        info!("デバイスの再接続を要求された");
        // 開き直したあとの途絶を、改めて検出してログに残せるようにする
        self.last_video_link_action = VideoLinkAction::Keep;
        // 保留していた音声のエラーも、ここで開き直すので落とす
        self.audio_stream_error_pending = false;
        self.last_video_target = None;
        self.last_audio_target = None;
        let Some(config) = self.config.clone() else {
            // まだ設定を受け取っていない。次の ApplyConfig が要求を立てる
            return;
        };
        self.video_retry.request_now(config.video);
        self.audio_retry.request_now(config.audio);
    }

    /// 期限が来ている接続を試し、稼働中のデバイスが生きているかを見る。
    fn tick(&mut self, now: Instant) {
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

    /// ストリームを閉じる。スレッドを抜ける直前に呼ぶ。
    fn shutdown(&mut self) {
        self.video.stop_capture();
        self.audio.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioControls;
    use crate::settings::DEFAULT_BUFFER_MS;
    use crate::video::VideoFrames;
    use std::sync::mpsc::channel;
    use std::sync::RwLock;

    #[test]
    fn next_tick_delay_while_retrying_is_short() {
        // バックオフの最小値 200ms より細かく起きないと、1 回目の再試行が
        // 500ms 近くまでずれる
        assert_eq!(next_tick_delay(true), RETRY_TICK);
        assert!(next_tick_delay(true) < Duration::from_millis(200));
    }

    #[test]
    fn next_tick_delay_while_idle_is_long() {
        // 何も追いかけていないときに細かく起きても得るものが無い
        assert_eq!(next_tick_delay(false), IDLE_TICK);
        assert!(next_tick_delay(false) > next_tick_delay(true));
    }

    /// テスト用のワーカー一式。
    struct Harness {
        commands: std::sync::mpsc::Sender<DeviceCommand>,
        events: std::sync::mpsc::Receiver<DeviceEvent>,
        snapshot: SharedSnapshot,
        /// ワーカーと共有している音量・ミュート。最小化中のホットキーの確認に使う
        audio_controls: Arc<AudioControls>,
        handle: std::thread::JoinHandle<()>,
    }

    impl Harness {
        /// 停止を伝えて、スレッドが終わるまで待つ。
        fn shutdown(self) {
            self.commands
                .send(DeviceCommand::Shutdown)
                .expect("停止を送れる");
            self.handle.join().expect("スレッドが終わる");
        }
    }

    /// デバイスを 1 つも掴まずにワーカーを起動する。
    ///
    /// `VideoCapture` も `AudioCapture` も、作るだけならデバイスを開かない
    /// （`AudioCapture::new` は cpal のホストを取るだけ）。開くのは
    /// `ApplyConfig` を送ってからなので、存在しないデバイス名を渡せば
    /// CI でも失敗経路をなぞれる。
    fn spawn_worker() -> Harness {
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = channel();
        let snapshot: SharedSnapshot = Arc::new(RwLock::new(DeviceSnapshot::default()));
        let thread_snapshot = Arc::clone(&snapshot);
        let audio_controls = Arc::new(AudioControls::default());
        let thread_controls = Arc::clone(&audio_controls);
        let handle = std::thread::spawn(move || {
            run(
                command_rx,
                event_tx,
                thread_snapshot,
                VideoFrames::new(),
                Arc::new(SharedColorConversion::new()),
                thread_controls,
                RepaintWaker::new(),
            );
        });
        Harness {
            commands: command_tx,
            events: event_rx,
            snapshot,
            audio_controls,
            handle,
        }
    }

    /// デバイス名だけを指定した `DeviceConfig` を作る。
    fn config_for(video_device: Option<&str>, input_device: Option<&str>) -> DeviceConfig {
        DeviceConfig {
            video: (video_device.map(str::to_string), None, None, None),
            audio: (
                input_device.map(str::to_string),
                None,
                None,
                None,
                DEFAULT_BUFFER_MS,
            ),
            auto_reconnect: false,
        }
    }

    #[test]
    fn worker_shutdown_command_stops_the_thread() {
        // `on_exit` が待てること。止まらないとアプリが終わらない
        spawn_worker().shutdown();
    }

    /// イベントが 1 つ届くまで待つ。CI の遅さを見込んで長めに待つ
    fn wait_for_event(worker: &Harness) -> DeviceEvent {
        worker
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("イベントが届くこと")
    }

    #[test]
    fn worker_adjust_volume_changes_the_shared_value_and_reports_the_delta() {
        // 最小化中のホットキーの経路。デバイスを開いていなくても効くこと
        let worker = spawn_worker();
        worker.audio_controls.set_volume(100.0);

        worker
            .commands
            .send(DeviceCommand::AdjustVolume(10.0))
            .expect("コマンドを送れる");

        match wait_for_event(&worker) {
            DeviceEvent::VolumeAdjusted(delta) => assert_eq!(delta, 10.0),
            other => panic!("音量の変更が返らない: {:?}", other),
        }
        // 出力コールバックが読む値が既に変わっていること（聞こえ方が先に変わる）
        assert!(
            (worker.audio_controls.volume_percent() - 110.0).abs() < 0.01,
            "音量が変わっていない: {}",
            worker.audio_controls.volume_percent()
        );
        worker.shutdown();
    }

    #[test]
    fn worker_adjust_volume_releases_mute() {
        // UI 側の `adjust_volume` と同じ扱い。解除しないと
        // 「上げたのに鳴らない」状態になる
        let worker = spawn_worker();
        worker.audio_controls.set_muted(true);

        worker
            .commands
            .send(DeviceCommand::AdjustVolume(-10.0))
            .expect("コマンドを送れる");
        wait_for_event(&worker);

        assert!(!worker.audio_controls.muted());
        worker.shutdown();
    }

    #[test]
    fn worker_toggle_mute_flips_the_shared_value() {
        let worker = spawn_worker();
        assert!(!worker.audio_controls.muted());

        worker
            .commands
            .send(DeviceCommand::ToggleMute)
            .expect("コマンドを送れる");

        match wait_for_event(&worker) {
            DeviceEvent::MuteToggled => {}
            other => panic!("ミュートの切替が返らない: {:?}", other),
        }
        assert!(worker.audio_controls.muted());
        worker.shutdown();
    }

    #[test]
    fn worker_dropping_the_sender_stops_the_thread() {
        // `on_exit` を通らずに終わる経路。送り手が居なくなったら抜ける
        let worker = spawn_worker();
        drop(worker.commands);
        worker.handle.join().expect("スレッドが終わる");
    }

    #[test]
    fn worker_reports_failure_for_a_missing_video_device() {
        // 存在しないデバイス名なら、実機が無い CI でも必ず失敗経路を通る。
        // 失敗しても諦めずに再試行を続けること（`active` が立ったまま）を見る
        let worker = spawn_worker();
        worker
            .commands
            .send(DeviceCommand::ApplyConfig {
                config: Box::new(config_for(
                    Some("存在しないキャプチャーデバイス"),
                    Some("存在しない入力デバイス"),
                )),
                initial: true,
            })
            .expect("送信できる");

        let reason = loop {
            match worker.events.recv_timeout(Duration::from_secs(30)) {
                Ok(DeviceEvent::VideoFailed(reason)) => break reason,
                // 能力取得やデバイス一覧が先に届くことがある
                Ok(_) => continue,
                Err(e) => panic!("映像の失敗がイベントで返ること: {e}"),
            }
        };
        assert!(!reason.is_empty(), "失敗理由を必ず添える");

        // **イベントを受け取った時点の観測値が入っている。** ワーカーは
        // イベントを送る前にスナップショットを書き出す
        let observed = worker.snapshot.read().expect("観測値を読める").clone();
        assert!(!observed.video_capturing, "開けていないこと");
        assert!(observed.video_retry.active, "繋がるまで追いかけ続けること");
        assert_eq!(observed.video_retry.attempts, 1, "1 回目の失敗を数えること");
        assert!(observed.active_video.is_none(), "開いた内容が残らないこと");

        worker.shutdown();
    }

    #[test]
    fn worker_reconnect_without_config_does_not_panic() {
        // 設定を一度も受け取っていない状態で「デバイス再接続」を押した場合。
        // 起動直後の数フレームで届きうる
        let worker = spawn_worker();
        worker
            .commands
            .send(DeviceCommand::ReconnectNow)
            .expect("送信できる");
        worker.shutdown();
    }
}
