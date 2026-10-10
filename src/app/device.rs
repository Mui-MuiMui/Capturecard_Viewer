//! 設定の実行時への適用と、デバイスワーカーとのやりとり（UI 側）。
//!
//! **ここではデバイスを開かない。** 開く・閉じる・列挙するのは
//! `super::worker_loop` のワーカースレッドで、この層は設定を
//! `DeviceConfig` へ写して送り、返ってきた `DeviceEvent` を画面の状態へ
//! 反映するだけ。`apply_settings` は設定ダイアログや右クリックメニューからも
//! 呼ばれるため、ここが数百 ms 止まると操作がそのまま固まる。
//!
//! 開き直しが要るかの差分判定はワーカー側が持つ。UI は毎回そのまま送る。

use super::worker::{DeviceCommand, DeviceConfig, DeviceEvent, VideoTarget};
use super::CaptureCardViewer;
use crate::audio::AudioDirection;
use crate::settings::{AppSettings, AudioInputSource, VideoSettings};
use crate::status::ErrorSource;
use crate::ui;
use crate::video::VideoAdjustments;
use log::warn;
use std::time::Instant;

/// 開いた解像度（`VideoResolutionResolved`）を設定へ書き戻してよいか（#391）。
///
/// **設定の解像度がまだ未指定で、名前・形式・fps・開き方が開いたときの相手と
/// 同じときだけ。** 届くまでの間に利用者が解像度を選んだ、あるいは別の
/// デバイスへ切り替えたなら、そちらを残す（別のデバイスの解像度を書かない）。
fn should_store_resolved_resolution(video: &VideoSettings, target: &VideoTarget) -> bool {
    video.resolution.is_none()
        && video.device_name == target.0
        && video.format == target.2
        && video.fps == target.3
        && video.backend == target.4
}

/// ワーカーが決めた既定のデバイス（`DefaultDevicesResolved`）を設定へ書く。書いたかを返す。
///
/// 映像の名前を埋めたら解像度は未指定にする。ワーカーは解像度を未指定にして開く
/// （#391）ので、揃えないと次に送る設定の解像度（既定の 1280x720）で開き直される。
/// 開いた解像度は `VideoResolutionResolved` で届く。
///
/// **入力は、設定の入力がまだ決まっていないときだけ書く**（#394）。入力は最初の
/// 映像の試行のあとに届くので、それまでに利用者が選んでいたらそちらを残す。
/// 「映像デバイスの音声」に決まったときは `input_source` だけを書き、
/// 入力デバイス名は未設定のまま（WASAPI へ戻したときに選び直す）。
pub(super) fn apply_resolved_devices(
    settings: &mut AppSettings,
    video: Option<String>,
    input: Option<String>,
    input_source: Option<AudioInputSource>,
) -> bool {
    let mut changed = false;
    if let Some(name) = video {
        settings.video.device_name = Some(name);
        settings.video.resolution = None;
        changed = true;
    }
    let input_undecided = settings.audio.input_source == AudioInputSource::Device
        && settings
            .audio
            .input_device_name
            .as_deref()
            .is_none_or(str::is_empty);
    if input_undecided {
        if let Some(name) = input {
            settings.audio.input_device_name = Some(name);
            changed = true;
        }
        if input_source == Some(AudioInputSource::VideoPin) {
            settings.audio.input_source = AudioInputSource::VideoPin;
            changed = true;
        }
    }
    changed
}

impl CaptureCardViewer {
    /// 設定値を適用し直す必要があるかを判定する。
    /// `last` は最後に適用できた値で、`None` は「まだ適用できていない」を表す。
    /// `initial` が真なら値が変わっていなくても適用する。
    fn needs_reapply<T: PartialEq>(initial: bool, current: &T, last: &Option<T>) -> bool {
        initial || last.as_ref() != Some(current)
    }

    /// ワーカーから届いたイベントを取り込む。`update()` の先頭で呼ぶ。
    ///
    /// **ここが唯一の取り込み口。** 接続の成否も、デバイス能力も、デバイス
    /// 一覧も同じチャネルで届く。最小化している間は `update()` が呼ばれない
    /// ため溜まるが、ワーカー側の再接続は止まらない（#133）。
    pub(super) fn drain_device_events(&mut self) {
        while let Some(event) = self.device.try_recv() {
            self.handle_device_event(event);
        }
    }

    fn handle_device_event(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::VideoConnected => {
                // 繋がったので直前の失敗は消す。プレースホルダーと
                // 「接続状態」タブに古い理由が残らないようにする
                self.errors.clear(ErrorSource::Video);
            }
            DeviceEvent::VideoFailed(reason) => self.report_error(ErrorSource::Video, reason),
            DeviceEvent::AudioConnected => self.errors.clear(ErrorSource::Audio),
            DeviceEvent::AudioFailed(reason) => self.report_error(ErrorSource::Audio, reason),
            DeviceEvent::VideoSignalLost => {
                // 最後のフレームが残り続けると、止まっているのか映っているのか
                // 判らない。テクスチャを捨てて「映像信号がありません」へ戻す。
                // 開き直すかどうかはワーカーが判断済み
                self.video_texture = None;
            }
            DeviceEvent::VideoCapabilities(device, backend, result) => {
                self.settings_dialog
                    .capabilities_mut()
                    .apply_result(ui::VideoCapabilityKey::new(Some(&device), backend), *result);
            }
            DeviceEvent::AudioCapabilities(direction, key, result) => match direction {
                AudioDirection::Input => self
                    .settings_dialog
                    .audio_input_capabilities_mut()
                    .apply_result(key, *result),
                AudioDirection::Output => self
                    .settings_dialog
                    .audio_output_capabilities_mut()
                    .apply_result(key, *result),
            },
            DeviceEvent::DeviceLists {
                video,
                input,
                output,
            } => {
                self.cached_video_devices = video;
                self.cached_input_devices = input;
                self.cached_output_devices = output;
            }
            DeviceEvent::VolumeAdjusted(delta) => {
                // 最小化中にワーカーが代わりに実行した分を、UI 側にも同じ
                // 経路で効かせる。音は既に変わっているので、ここで行うのは
                // 設定への反映と OSD。**復帰したフレームで初めて届く**
                self.adjust_volume(delta);
            }
            DeviceEvent::MuteToggled => self.toggle_mute(),
            DeviceEvent::DefaultDevicesResolved {
                video,
                input,
                input_source,
            } => {
                self.store_resolved_devices(video, input, input_source);
            }
            DeviceEvent::VideoResolutionResolved { target, resolution } => {
                self.store_resolved_resolution(&target, resolution);
            }
        }
    }

    /// 解像度が未指定のまま開いた映像の、実際の解像度を設定へ書き戻す（#391）。
    /// 書くかどうかは `should_store_resolved_resolution`。
    fn store_resolved_resolution(&mut self, target: &VideoTarget, resolution: (u32, u32)) {
        let changed = match self.settings.lock() {
            Ok(mut settings) if should_store_resolved_resolution(&settings.video, target) => {
                settings.video.resolution = Some(resolution);
                true
            }
            Ok(_) => false,
            Err(_) => {
                warn!("開いた解像度の書き戻しで settings のロックを取得できない");
                return;
            }
        };
        if changed {
            self.mark_settings_dirty();
        }
    }

    /// ワーカーが列挙結果の先頭で埋めたデバイス名を、設定へ書き戻す。
    ///
    /// **ここで `save()` を直接呼ばない。** 壊れた設定ファイルが残っている
    /// 場合に書き戻しを止める判断は `mark_settings_dirty` が持っている
    /// （`AutoSavePolicy`）。以前は `CaptureCardViewer::default` が
    /// `may_write_defaults_on_startup()` で同じ判断をしていた。
    ///
    /// 入力を「映像デバイスの音声」に決めたときは、その場で設定をワーカーへ送り直す
    /// （#394）。ワーカーは書き戻した設定（音声ピンを繋ぐ映像の接続対象）が届いてから
    /// 映像を開き直し、音声を開くので、2 秒ごとの再適用を待つとその分だけ音が遅れる。
    fn store_resolved_devices(
        &mut self,
        video: Option<String>,
        input: Option<String>,
        input_source: Option<AudioInputSource>,
    ) {
        let changed = match self.settings.lock() {
            Ok(mut settings) => apply_resolved_devices(&mut settings, video, input, input_source),
            Err(_) => {
                warn!("既定デバイスの書き戻しで settings のロックを取得できない");
                return;
            }
        };
        if changed {
            self.mark_settings_dirty();
            if input_source == Some(AudioInputSource::VideoPin) {
                self.apply_settings(false);
            }
        }
    }

    pub(super) fn apply_settings(&mut self, initial: bool) {
        // 設定はここで 1 度だけ複製し、以降はこの複製だけを見る。
        // 複製しておけばワーカーへ渡す値を組み立てる間もロックを握らずに済む
        let snapshot = match self.settings.lock() {
            Ok(settings) => Some(settings.clone()),
            Err(_) => {
                warn!("設定の適用で settings のロックを取得できない");
                None
            }
        };

        if let Some(settings) = snapshot {
            // 映像・音声のデバイス設定はワーカーへ丸ごと渡す。
            // 開き直しが要るかの判定も、実際に開く処理もあちらが行う
            self.device.send(DeviceCommand::ApplyConfig {
                config: Box::new(DeviceConfig::from_settings(&settings)),
                initial,
            });

            // リプレイバッファの ON / OFF とさかのぼる長さは、次の録画からではなくすぐ効かせる。
            // 変わったときだけ録画スレッドへ伝わる（`sync_replay_buffer`）
            self.sync_replay_buffer(&settings.recording);

            // 色空間とレンジはデバイスの開き直しを伴わない。共有の Atomic へ
            // 書くだけで次のフレームから効くので、UI スレッドから直接入れる。
            // 2 秒ごとに同じ値をログへ出さないよう差分で判定する
            let color_conversion = (settings.video.color_space, settings.video.color_range);
            if Self::needs_reapply(initial, &color_conversion, &self.last_color_conversion) {
                self.color_conversion
                    .set_color_conversion(color_conversion.0, color_conversion.1);
                self.last_color_conversion = Some(color_conversion);
            }

            // 明るさ・コントラスト・彩度も係数表へ畳み込まれるだけなので、
            // 色空間・レンジと同じく開き直しを伴わない
            let adjustments = VideoAdjustments::new(
                settings.video.brightness,
                settings.video.contrast,
                settings.video.saturation,
            );
            if Self::needs_reapply(initial, &adjustments, &self.last_video_adjustments) {
                self.color_conversion.set_video_adjustments(adjustments);
                self.last_video_adjustments = Some(adjustments);
            }

            // YUY2 → RGB の変換の場所（#456）。旗を書き換えるだけで、次のフレームから効く。
            // 値が変わったときだけ、見張りによる切り替えを取り消して数え直す
            self.gpu_yuy2.set_preference(settings.video.convert);

            // パススルーの有効・無効と音量・ミュートは、出力コールバックが読む
            // Atomic を書き換えるだけで効く。デバイスを開く処理を挟まないので、
            // ワーカーを経由させずに UI スレッドから直接入れる
            let previous_volume = self.volume;
            self.audio_controls
                .set_passthrough_enabled(settings.audio.passthrough_enabled);
            self.volume = settings.ui.volume;
            self.audio_controls.set_volume(self.volume);
            self.muted = settings.ui.muted;
            self.audio_controls.set_muted(self.muted);

            // 設定ダイアログの「適用」「OK」で音量が変わったときも OSD を出す。
            // ホイールや右クリックメニューでの変更は設定側も同時に更新しているため、
            // 2 秒ごとの再適用ではここに入らず、OSD が出っぱなしにはならない。
            // 起動時は変更ではないので出さない
            if !initial && (self.volume - previous_volume).abs() > 0.01 {
                self.show_volume_overlay();
            }

            // UI設定
            self.maintain_aspect_ratio = settings.ui.maintain_aspect_ratio;
            self.always_on_top = settings.ui.always_on_top;
            self.show_stats_overlay = settings.ui.show_stats_overlay;
            // 装飾の有無は値を取り込むだけで、ここでは ViewportCommand を送らない。
            // 実際の切替は右クリックメニュー（set_borderless）と起動時の
            // ViewportBuilder が行う。2 秒ごとにコマンドを送ると、フルスクリーン中に
            // 装飾を付け直そうとして表示がちらつく
            self.borderless = settings.ui.borderless;

            // ホットキーの割り当て。
            //
            // 差分は `HotkeyManager::apply` が取る。無条件に登録し直すと、
            // 2 秒ごとに unregister → register が走ってその瞬間のキー入力を
            // 取りこぼす
            self.apply_hotkey_assignments(&settings.hotkeys);
            // フォーカスがあるときだけ反応するか。値を書くだけなので毎回渡す
            self.hotkey_manager
                .set_only_when_focused(settings.hotkey_settings.only_when_focused);

            // スクリーンショットの効果音
            //
            // **`None`（効果音を鳴らさない）も差分として扱う。** 以前は `if let Some(..)` で
            // 包んでいたため、設定画面で効果音を外してもそのセッション中は
            // 効果音が鳴り続けていた
            //
            // 無条件に読むと 2 秒ごとに効果音ファイル全体を読み直すことになる
            if Self::needs_reapply(
                initial,
                &settings.screenshot.sound_file,
                &self.last_sound_file,
            ) {
                match &settings.screenshot.sound_file {
                    Some(sf) => {
                        // 読み込みは別スレッドで行い、結果は drain_sound_results が
                        // 受け取る（app::screenshot_sound）。**要求した時点で適用済みに
                        // する。** 結果を待ってからにすると、読み終わるまで 2 秒ごとに
                        // 同じファイルの読み込みを積み増してしまう。
                        // 読めなかった場合は受け取った側が last を空にして読み直させる
                        self.request_sound_apply(sf);
                        self.last_sound_file = Some(Some(sf.clone()));
                    }
                    None => {
                        // 未選択は「鳴らさない」の意味。読み込みは見つからない
                        // ファイルを既定音へ倒すので、無音はここでしか表せない。
                        // 読み込み中の要求もここで取り消される
                        if let Ok(mut ss) = self.screenshot_manager.lock() {
                            ss.clear_sound();
                            self.last_sound_file = Some(None);
                        } else {
                            warn!(
                                "スクリーンショットの効果音の反映で screenshot_manager のロックを取得できない"
                            );
                        }
                    }
                }
            }
        }

        if !initial {
            self.last_settings_applied = Instant::now();
        }
    }

    /// デバイスを強制的に開き直す。右クリックメニューの「デバイス再接続」と同じ。
    ///
    /// バックオフの待ち時間を飛ばすのはワーカー側の `ReconnectNow`。
    /// ユーザーが明示的にやり直しを求めているので、最大 5 秒待たせない
    /// （ログもワーカー側が残す）。
    pub(super) fn reconnect_devices(&mut self) {
        self.device.send(DeviceCommand::ReconnectNow);
        self.apply_settings(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn needs_reapply_not_applied_yet_returns_true() {
        // まだ一度も適用できていない場合は適用する
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &"F5".to_string(),
            &None
        ));
    }

    #[test]
    fn needs_reapply_same_value_returns_false() {
        // 値が変わっていなければ再適用しない（2 秒ごとの再登録を防ぐ肝）
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &"F5".to_string(),
            &Some("F5".to_string())
        ));
    }

    #[test]
    fn needs_reapply_changed_value_returns_true() {
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &"F7".to_string(),
            &Some("F5".to_string())
        ));
    }

    #[test]
    fn needs_reapply_cleared_value_returns_true() {
        // 設定画面で「クリア」した場合。設定は None になるが、実行中は
        // 古いホットキーが登録されたまま。ここを差分として拾えないと、
        // そのセッションの間ずっと解除されない
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &Some(Some("F5".to_string()))
        ));
    }

    #[test]
    fn needs_reapply_already_cleared_returns_false() {
        // 解除済みの状態。2 秒ごとに解除し直さない
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &Some(None)
        ));
    }

    #[test]
    fn needs_reapply_cleared_but_not_applied_yet_returns_true() {
        // 未適用（外側の None）と解除済み（Some(None)）を区別する。
        // 区別できないと、起動直後の 1 回が飛ぶ
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &None
        ));
    }

    #[test]
    fn needs_reapply_initial_same_value_returns_true() {
        // 起動直後は値が同じでも適用する
        assert!(CaptureCardViewer::needs_reapply(
            true,
            &"F5".to_string(),
            &Some("F5".to_string())
        ));
    }

    /// 初回に解像度を外したあとの設定と、それで開いたときの接続対象
    fn first_run_video() -> (VideoSettings, VideoTarget) {
        let video = VideoSettings {
            device_name: Some("ボード".to_string()),
            resolution: None,
            ..VideoSettings::default()
        };
        let target = DeviceConfig::from_settings(&crate::settings::AppSettings {
            video: video.clone(),
            ..Default::default()
        })
        .video;
        (video, target)
    }

    #[test]
    fn should_store_resolved_resolution_when_the_target_matches() {
        let (video, target) = first_run_video();
        assert!(should_store_resolved_resolution(&video, &target));
    }

    #[test]
    fn should_not_store_resolved_resolution_once_chosen() {
        // 届くまでの間に利用者が解像度を選んだら、そちらを残す
        let (mut video, target) = first_run_video();
        video.resolution = Some((1280, 720));
        assert!(!should_store_resolved_resolution(&video, &target));
    }

    #[test]
    fn should_not_store_resolved_resolution_for_another_target() {
        use crate::settings::VideoBackendSetting;
        let (video, target) = first_run_video();
        let changes: [fn(&mut VideoSettings); 4] = [
            |video| video.device_name = Some("別のボード".to_string()),
            |video| video.format = Some("MJPEG".to_string()),
            |video| video.fps = Some(30),
            |video| video.backend = VideoBackendSetting::DirectShow,
        ];
        for (index, change) in changes.into_iter().enumerate() {
            let mut changed = video.clone();
            change(&mut changed);
            assert!(
                !should_store_resolved_resolution(&changed, &target),
                "変更 {index} のあとでも書き戻している"
            );
        }
    }

    #[test]
    fn needs_reapply_path_same_value_returns_false() {
        // PathBuf でも同じ判定になること
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &PathBuf::from("sound/SS.mp3"),
            &Some(PathBuf::from("sound/SS.mp3"))
        ));
    }

    // 初回の既定の書き戻し（#394）

    /// 設定ファイルが無い初回の設定。映像も入力も未設定
    fn first_run_settings() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.video.device_name = None;
        settings.video.resolution = Some((1280, 720));
        settings.audio.input_device_name = None;
        settings
    }

    #[test]
    fn apply_resolved_devices_writes_the_video_pin_without_an_input_name() {
        let mut settings = first_run_settings();
        assert!(apply_resolved_devices(
            &mut settings,
            None,
            None,
            Some(AudioInputSource::VideoPin)
        ));
        assert_eq!(settings.audio.input_source, AudioInputSource::VideoPin);
        assert_eq!(settings.audio.input_device_name, None);
        // 映像は届いていないので触らない
        assert_eq!(settings.video.resolution, Some((1280, 720)));
    }

    #[test]
    fn apply_resolved_devices_writes_the_video_name_and_the_wasapi_input() {
        let mut settings = first_run_settings();
        assert!(apply_resolved_devices(
            &mut settings,
            Some("キャプチャーボード".to_string()),
            Some("ライン入力".to_string()),
            Some(AudioInputSource::Device)
        ));
        assert_eq!(
            settings.video.device_name.as_deref(),
            Some("キャプチャーボード")
        );
        assert_eq!(settings.video.resolution, None);
        assert_eq!(
            settings.audio.input_device_name.as_deref(),
            Some("ライン入力")
        );
        assert_eq!(settings.audio.input_source, AudioInputSource::Device);
    }

    #[test]
    fn apply_resolved_devices_keeps_an_input_chosen_in_the_meantime() {
        // 入力は最初の映像の試行のあとに届く。それまでに選ばれていたら残す
        let mut settings = first_run_settings();
        settings.audio.input_device_name = Some("選んだ入力".to_string());
        assert!(!apply_resolved_devices(
            &mut settings,
            None,
            None,
            Some(AudioInputSource::VideoPin)
        ));
        assert_eq!(settings.audio.input_source, AudioInputSource::Device);

        let mut settings = first_run_settings();
        settings.audio.input_source = AudioInputSource::VideoPin;
        assert!(!apply_resolved_devices(
            &mut settings,
            None,
            Some("ライン入力".to_string()),
            Some(AudioInputSource::Device)
        ));
        assert_eq!(settings.audio.input_device_name, None);
        assert_eq!(settings.audio.input_source, AudioInputSource::VideoPin);
    }
}
