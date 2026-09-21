//! デバイス一覧のキャッシュと、デバイス能力・対応設定の取得。
//!
//! 取得はどれもデバイスを開く重い処理なので使い捨てのスレッドへ投げ、
//! UI スレッドは `update()` で結果を受け取るだけにする。キャッシュ
//! （`ui::CapabilityCache`）を触るのは UI スレッドだけなのでロックは要らない。

use super::CaptureCardViewer;
use crate::audio::{self, AudioCapabilities, AudioDirection};
use crate::video::{self, VideoCapture};
use log::{debug, info, warn};
use std::time::{Duration, Instant};

/// デバイスリストのキャッシュを更新する間隔
const DEVICE_LIST_CACHE_INTERVAL: Duration = Duration::from_secs(5);

/// デバイス能力の取得結果。`(問い合わせたデバイス名, 結果)`。
/// 取得スレッドから UI スレッドへ、この形でチャネル越しに返す
pub(super) type CapabilityResult = (String, Result<video::DeviceCapabilities, String>);

/// オーディオデバイスの対応設定の取得結果。
/// `(入力か出力か, 問い合わせたキャッシュのキー, 結果)`。
///
/// **向きを添えるのは、入力と出力でキャッシュを分けているため。** 同じ名前の
/// デバイスが入力にも出力にもあると、キーだけではどちらへ入れるか決まらない
pub(super) type AudioCapabilityResult = (AudioDirection, String, Result<AudioCapabilities, String>);

impl CaptureCardViewer {
    /// デバイスリストのキャッシュを更新すべきかを判定する。
    /// `elapsed` は前回更新からの経過時間で、`None` は「一度も取得していない」を表す。
    fn should_refresh_device_list(elapsed: Option<Duration>) -> bool {
        match elapsed {
            None => true,
            Some(elapsed) => elapsed >= DEVICE_LIST_CACHE_INTERVAL,
        }
    }

    fn update_cached_device_lists(&mut self) {
        // パフォーマンス影響を避けるため一定間隔でのみデバイスリストを更新
        let elapsed = self.last_device_list_update.map(|last| last.elapsed());
        if !Self::should_refresh_device_list(elapsed) {
            return;
        }

        // ビデオデバイスの列挙は MediaFoundation への問い合わせで重いため、
        // オーディオデバイスと同じ間隔でキャッシュする
        self.cached_video_devices = VideoCapture::list_devices();

        if let Ok(audio) = self.audio_capture.lock() {
            self.cached_input_devices = audio.list_input_devices();
            self.cached_output_devices = audio.list_output_devices();
        }

        // ロック取得に失敗した場合も時刻は更新する。
        // 更新しないと次のフレームでビデオデバイスの列挙が再び走ってしまう
        self.last_device_list_update = Some(Instant::now());
    }

    /// 別スレッドから届いたデバイス能力の取得結果を設定ダイアログへ反映する。
    /// キャッシュを触るのは UI スレッドだけなのでロックは要らない。
    pub(super) fn drain_capability_results(&mut self) {
        while let Ok((device, result)) = self.capability_rx.try_recv() {
            self.settings_dialog
                .capabilities_mut()
                .apply_result(device, result);
        }
        while let Ok((direction, key, result)) = self.audio_capability_rx.try_recv() {
            match direction {
                AudioDirection::Input => self
                    .settings_dialog
                    .audio_input_capabilities_mut()
                    .apply_result(key, result),
                AudioDirection::Output => self
                    .settings_dialog
                    .audio_output_capabilities_mut()
                    .apply_result(key, result),
            }
        }
    }

    /// 設定に書かれているオーディオデバイスの対応設定を要求する。
    ///
    /// 既に取得済み・取得中なら何も起きない（`CapabilityCache::request`）。
    /// 実際にスレッドへ渡すのは `dispatch_capability_requests`。
    pub(super) fn request_audio_capabilities(&mut self) {
        let Some((input_key, output_key)) = self.audio_capability_keys() else {
            return;
        };
        self.settings_dialog
            .audio_input_capabilities_mut()
            .request(&input_key);
        self.settings_dialog
            .audio_output_capabilities_mut()
            .request(&output_key);
    }

    /// 設定に書かれている入出力デバイスの、能力キャッシュのキー。
    /// settings のロックを取れなければ `None`。
    fn audio_capability_keys(&self) -> Option<(String, String)> {
        let settings = match self.settings.lock() {
            Ok(settings) => settings,
            Err(_) => {
                warn!("音声の対応設定の要求で settings のロックを取得できない");
                return None;
            }
        };
        Some((
            audio::cache_key(settings.audio.input_device_name.as_deref()),
            audio::cache_key(settings.audio.output_device_name.as_deref()),
        ))
    }

    /// 溜まったデバイス能力の取得要求を、使い捨てのスレッドへ渡す。
    ///
    /// `get_device_capabilities` は `Camera::new` でデバイスを開いたうえで
    /// 3 フォーマット分の対応表を引くため数百 ms 以上かかる。以前は設定ダイアログの
    /// 描画中に直接呼んでいたため、デバイスを切り替えるたびにアプリ全体が固まっていた。
    pub(super) fn dispatch_capability_requests(&mut self) {
        for device in self.settings_dialog.capabilities_mut().take_requests() {
            let tx = self.capability_tx.clone();
            let name = device.clone();
            let spawned = std::thread::Builder::new()
                .name("capability-query".to_string())
                .spawn(move || {
                    let started = Instant::now();
                    let result = VideoCapture::get_device_capabilities(Some(&name));
                    match &result {
                        Ok(caps) => info!(
                            "デバイス能力を取得した: {}（{} フォーマット, {} ms）",
                            name,
                            caps.len(),
                            started.elapsed().as_millis()
                        ),
                        Err(e) => warn!("デバイス能力を取得できない: {}: {}", name, e),
                    }
                    if tx.send((name, result)).is_err() {
                        // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                        debug!("デバイス能力の送り先が既に無いので結果を捨てる");
                    }
                });

            if let Err(e) = spawned {
                warn!("デバイス能力を取得するスレッドを起動できない: {}", e);
                // 投げられなかった要求を Pending のまま残すと、再取得もできずに
                // 「取得中...」が出続ける
                self.settings_dialog.capabilities_mut().apply_result(
                    device,
                    Err(format!("取得用のスレッドを起動できませんでした: {}", e)),
                );
            }
        }

        self.dispatch_audio_capability_requests(AudioDirection::Input);
        self.dispatch_audio_capability_requests(AudioDirection::Output);
    }

    /// 溜まったオーディオデバイスの取得要求を、使い捨てのスレッドへ渡す。
    ///
    /// ビデオ側と同じ仕組み。`supported_*_configs()` は WASAPI で
    /// 13 レート × 5 形式の `IsFormatSupported`（実測約 300ms）になるため、
    /// UI スレッドでは呼ばない。
    fn dispatch_audio_capability_requests(&mut self, direction: AudioDirection) {
        let cache = match direction {
            AudioDirection::Input => self.settings_dialog.audio_input_capabilities_mut(),
            AudioDirection::Output => self.settings_dialog.audio_output_capabilities_mut(),
        };

        for key in cache.take_requests() {
            let tx = self.audio_capability_tx.clone();
            let thread_key = key.clone();
            let spawned = std::thread::Builder::new()
                .name("audio-capability-query".to_string())
                .spawn(move || {
                    let started = Instant::now();
                    let result = audio::query_capabilities(
                        direction,
                        audio::device_name_from_key(&thread_key),
                    );
                    match &result {
                        Ok(caps) => info!(
                            "{}デバイスの対応設定を取得した: {}（{} 件、{} ms）",
                            direction.label(),
                            thread_key,
                            caps.configs().len(),
                            started.elapsed().as_millis()
                        ),
                        Err(e) => warn!(
                            "{}デバイスの対応設定を取得できない: {}: {}",
                            direction.label(),
                            thread_key,
                            e
                        ),
                    }
                    if tx.send((direction, thread_key, result)).is_err() {
                        debug!("音声の対応設定の送り先が既に無いので結果を捨てる");
                    }
                });

            if let Err(e) = spawned {
                warn!("音声の対応設定を取得するスレッドを起動できない: {}", e);
                // Pending のまま残すと、音声の接続がここで待ち続けてしまう
                let cache = match direction {
                    AudioDirection::Input => self.settings_dialog.audio_input_capabilities_mut(),
                    AudioDirection::Output => self.settings_dialog.audio_output_capabilities_mut(),
                };
                cache.apply_result(
                    key,
                    Err(format!("取得用のスレッドを起動できませんでした: {}", e)),
                );
            }
        }
    }

    pub(super) fn get_cached_video_devices(&mut self) -> &Vec<(String, String)> {
        self.update_cached_device_lists();
        &self.cached_video_devices
    }

    pub(super) fn get_cached_input_devices(&mut self) -> &Vec<String> {
        self.update_cached_device_lists();
        &self.cached_input_devices
    }

    pub(super) fn get_cached_output_devices(&mut self) -> &Vec<String> {
        self.update_cached_device_lists();
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
