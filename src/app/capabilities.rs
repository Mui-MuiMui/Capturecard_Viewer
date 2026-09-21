//! デバイス一覧のキャッシュと、デバイス能力・対応設定の取得要求（UI 側）。
//!
//! 取得はどれもデバイスを開く重い処理なので、実際に問い合わせるのは
//! デバイスワーカースレッド（`super::worker_loop`）。ここは要求を積んで
//! コマンドとして流すところまでで、結果は `super::device` の
//! `drain_device_events` が受け取ってキャッシュへ入れる。
//!
//! キャッシュ（`ui::CapabilityCache`）を触るのは UI スレッドだけなので
//! ロックは要らない。

use super::worker::DeviceCommand;
use super::CaptureCardViewer;
use crate::audio::AudioDirection;
use std::time::{Duration, Instant};

/// デバイスリストのキャッシュを更新する間隔
const DEVICE_LIST_CACHE_INTERVAL: Duration = Duration::from_secs(5);

impl CaptureCardViewer {
    /// デバイスリストのキャッシュを更新すべきかを判定する。
    /// `elapsed` は前回更新からの経過時間で、`None` は「一度も取得していない」を表す。
    fn should_refresh_device_list(elapsed: Option<Duration>) -> bool {
        match elapsed {
            None => true,
            Some(elapsed) => elapsed >= DEVICE_LIST_CACHE_INTERVAL,
        }
    }

    /// デバイス一覧の取り直しを要求する。
    ///
    /// **その場では更新されない。** 映像は MediaFoundation、音声は WASAPI の
    /// 列挙（実測で音声側が 300ms 前後）なので、ワーカーへ投げて結果を待つ。
    /// 設定ダイアログを開いた直後の数フレームだけ、前回の一覧が出る。
    fn request_device_lists(&mut self) {
        let elapsed = self.last_device_list_update.map(|last| last.elapsed());
        if !Self::should_refresh_device_list(elapsed) {
            return;
        }
        // 結果を待たずに時刻を進める。進めないと、届くまでの毎フレームで
        // 要求を積み直してワーカーが列挙し続ける
        self.last_device_list_update = Some(Instant::now());
        self.device.send(DeviceCommand::RefreshDeviceLists);
    }

    /// 溜まったデバイス能力の取得要求を、ワーカーへコマンドとして流す。
    ///
    /// `get_device_capabilities` は `Camera::new` でデバイスを開いたうえで
    /// 3 フォーマット分の対応表を引くため数百 ms 以上かかる。以前は設定ダイアログの
    /// 描画中に直接呼んでいたため、デバイスを切り替えるたびにアプリ全体が固まっていた。
    pub(super) fn dispatch_capability_requests(&mut self) {
        for device in self.settings_dialog.capabilities_mut().take_requests() {
            self.device
                .send(DeviceCommand::QueryVideoCapabilities(device));
        }
        for key in self
            .settings_dialog
            .audio_input_capabilities_mut()
            .take_requests()
        {
            self.device.send(DeviceCommand::QueryAudioCapabilities(
                AudioDirection::Input,
                key,
            ));
        }
        for key in self
            .settings_dialog
            .audio_output_capabilities_mut()
            .take_requests()
        {
            self.device.send(DeviceCommand::QueryAudioCapabilities(
                AudioDirection::Output,
                key,
            ));
        }
    }

    pub(super) fn get_cached_video_devices(&mut self) -> &Vec<(String, String)> {
        self.request_device_lists();
        &self.cached_video_devices
    }

    pub(super) fn get_cached_input_devices(&mut self) -> &Vec<String> {
        self.request_device_lists();
        &self.cached_input_devices
    }

    pub(super) fn get_cached_output_devices(&mut self) -> &Vec<String> {
        self.request_device_lists();
        &self.cached_output_devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_refresh_device_list_never_updated_returns_true() {
        // 一度も列挙していない状態では必ず取得する
        assert!(CaptureCardViewer::should_refresh_device_list(None));
    }

    #[test]
    fn should_refresh_device_list_just_updated_returns_false() {
        assert!(!CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_secs(0)
        )));
    }

    #[test]
    fn should_refresh_device_list_just_before_interval_returns_false() {
        // 境界の手前。4999ms では更新しない
        assert!(!CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_millis(4999)
        )));
    }

    #[test]
    fn should_refresh_device_list_at_interval_returns_true() {
        // 境界。ちょうど 5000ms で更新する
        assert!(CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_millis(5000)
        )));
    }

    #[test]
    fn should_refresh_device_list_long_after_interval_returns_true() {
        assert!(CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_secs(3600)
        )));
    }
}
