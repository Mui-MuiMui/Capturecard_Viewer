//! デバイスワーカーのコマンドの受け口。
//!
//! UI スレッドから届いた `DeviceCommand` を振り分け（`handle`）、設定の
//! 受け取り（開き直しが要るものだけ要求を立てる）、最小化中のホットキーの
//! 代役（音量とミュート）、即時の再接続を行う。デバイスを開く・閉じる・
//! 列挙する処理は `super::worker_connect` / `super::worker_audio_connect`。
//!
//! `super::worker_loop` の `WorkerState` へ `impl` を足す形で、状態はここに持たない。

use super::audio_control::volume_change_result;
use super::monitor::VideoLinkAction;
use super::worker::{DeviceCommand, DeviceConfig, DeviceEvent};
use super::worker_loop::WorkerState;
use log::{info, trace};

impl WorkerState {
    pub(super) fn handle(&mut self, command: DeviceCommand) {
        match command {
            DeviceCommand::ApplyConfig { config, initial } => self.apply_config(*config, initial),
            DeviceCommand::ReconnectNow => self.reconnect_now(),
            DeviceCommand::RefreshDeviceLists => self.refresh_device_lists(),
            DeviceCommand::QueryVideoCapabilities(device, backend) => {
                self.query_video_capabilities(device, backend)
            }
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
            // 列挙そのものは最初の接続を試したあとの `tick` で行う。ここで
            // 列挙すると、その分だけ最初の接続が遅れる
            self.startup_enumeration_pending = true;
        }

        // 繋ぐ相手が変わったら「見えていない」の判定は前の相手のもの。
        // **直前に受け取った設定と比べる。** 繋がっていない間は
        // `last_video_target` が `None` のままなので、そちらと比べると
        // 2 秒ごとの `apply_settings` のたびに消えてしまう
        let previous = self.config.as_ref();
        if previous.map(|config| &config.video) != Some(&config.video) {
            self.video_not_visible = None;
        }
        if previous.map(|config| &config.audio) != Some(&config.audio) {
            self.audio_not_visible = None;
        }

        // 未指定でも要求を立てる。ストリームを閉じるのは `try_connect_video`（#334）
        let need_video_restart = Some(&config.video) != self.last_video_target.as_ref();
        if need_video_restart || initial {
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
}
