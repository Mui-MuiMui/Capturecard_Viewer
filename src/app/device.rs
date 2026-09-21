//! デバイスへの接続と、設定の実行時への適用。
//!
//! 接続は `super::retry::ConnectRetry` の期限が来たフレームで 1 回だけ試す。
//! `apply_settings` は設定の差分を見て「開き直しが要る」ことを要求するだけで、
//! ここで開かない（ダイアログやメニューから呼ばれるため、その場で UI が
//! 止まらないようにする）。

use super::monitor::VideoLinkAction;
use super::retry::backoff_delay;
use super::CaptureCardViewer;
use crate::audio::{self, PassthroughRequest};
use crate::settings::AppSettings;
use crate::status::ErrorSource;
use crate::video::VideoAdjustments;
use log::{debug, info, trace, warn};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// オーディオデバイスの対応設定を待つ上限。
///
/// 取得は別スレッドで走り、実測 300ms 前後で終わる。終わるまで音声を開かない
/// のは、開く処理の中の列挙を UI スレッドで走らせないため。**ただし待ち続け
/// ない。** cpal の列挙が返ってこない環境で音が一切出なくなるより、
/// その場で列挙してでも繋ぐほうがよい
const AUDIO_CAPABILITY_WAIT_LIMIT: Duration = Duration::from_secs(3);

/// 音声で、この回数だけ連続して失敗したら「繋がっていない」ことをログに残す。
///
/// **ここで別のデバイスへ倒したりはしない。** 以前は同じ 3 回目に既定の
/// デバイスへフォールバックしていた（`decide_audio_fallback` を参照）。
/// 残したのはログだけで、回数を合わせてあるのは、以前のログと同じ位置に
/// 「ここで方針が分かれていた」という目印を置くため。
const AUDIO_RETRY_WARN_AFTER: u32 = 3;

/// 音声の接続に失敗したあと、その場で何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioFallbackAction {
    /// 何もしない。`ConnectRetry` のバックオフで次の回を待つ
    Retry,
    /// 繋がらないまま再試行を続けることを 1 度だけ記録し、あとは `Retry` と同じ
    WarnAndRetry,
}

/// 音声の接続に失敗したときに、その回で何をするかを決める。
///
/// **どの回でも接続先は変えない。** 以前は 3 回失敗した時点で入出力とも
/// Windows の既定デバイスで開き直していたが、これが USB を抜いた直後の
/// 再接続でも効いていた。設定のデバイスが消えている間は必ず 3 回失敗する
/// （1.5 秒）一方、USB の再列挙には 10 秒以上かかるため、**フォールバックが
/// 必ず勝って PC のマイクを入力として掴み、接続成功として確定してしまう**
/// （#134）。以後は設定のデバイスを試さないので音は戻らず、おまけにマイクの
/// 音がスピーカーへ流れ続ける。
///
/// 起動時も同じ扱いにしてある。設定のデバイスが見つからないときに黙って
/// 別のデバイスを開くのは、音が出ないことより分かりにくい誤動作のため。
/// 繋がらないことは「接続状態」タブと通知に出るので、気付く手段はある。
///
/// 残っているのは「繋がらないまま再試行を続けている」ことをログへ 1 度だけ
/// 残す判断だけ。毎回出すとログが埋まり、一度も出さないと調査で気付けない。
fn decide_audio_fallback(attempt: u32) -> AudioFallbackAction {
    if attempt == AUDIO_RETRY_WARN_AFTER {
        return AudioFallbackAction::WarnAndRetry;
    }
    AudioFallbackAction::Retry
}

/// 映像が復帰したときに、音声にも再接続を要求するかを判定する。
///
/// 映像と音声は同じ USB 機器なので、映像が戻ったなら音声のデバイスも戻って
/// いる。音声側のバックオフ（最大 5 秒）を待たせる理由が無いため、そこで
/// 待ち時間を飛ばす。**保険であって主経路ではない。** 音声の切断は cpal の
/// エラーコールバックが拾い、`monitor_audio_stream` が再接続を要求する。
///
/// **音声が開けていて再試行も走っていないなら何もしない。** 映像だけが
/// 消える構成（音声は別のマイク）で、無事だったストリームを開き直すと
/// その都度 UI スレッドが 300ms 止まる。
fn should_resync_audio_after_video(
    video_recovered: bool,
    audio_connected: bool,
    audio_retry_active: bool,
) -> bool {
    if !video_recovered {
        return false;
    }
    !audio_connected || audio_retry_active
}

/// 映像の接続対象。これが変わったらバックオフを捨てて即座に開き直す。
/// `(デバイス名, 解像度, フォーマット, fps)`
pub(super) type VideoTarget = (
    Option<String>,
    Option<(u32, u32)>,
    Option<String>,
    Option<u32>,
);

/// 音声の接続対象。`(入力デバイス名, 出力デバイス名, サンプリングレート, チャンネル数)`
pub(super) type AudioTarget = (Option<String>, Option<String>, Option<u32>, Option<u16>);

/// 設定から映像の接続対象を取り出す。
pub(super) fn video_target(settings: &AppSettings) -> VideoTarget {
    (
        settings.video.device_name.clone(),
        settings.video.resolution,
        settings.video.format.clone(),
        settings.video.fps,
    )
}

/// 設定から音声の接続対象を取り出す。
pub(super) fn audio_target(settings: &AppSettings) -> AudioTarget {
    (
        settings.audio.input_device_name.clone(),
        settings.audio.output_device_name.clone(),
        settings.audio.sample_rate,
        settings.audio.channels,
    )
}

impl CaptureCardViewer {
    /// 設定値を適用し直す必要があるかを判定する。
    /// `last` は最後に適用できた値で、`None` は「まだ適用できていない」を表す。
    /// `initial` が真なら値が変わっていなくても適用する。
    fn needs_reapply<T: PartialEq>(initial: bool, current: &T, last: &Option<T>) -> bool {
        initial || last.as_ref() != Some(current)
    }

    /// 期限が来ているデバイスの接続を 1 回だけ試す。`update()` から毎フレーム呼ぶ。
    ///
    /// **ここで `thread::sleep` を使わない。** UI スレッドを止めると、接続に
    /// 失敗し続ける間ウィンドウが固まる。待つ代わりに次に試してよい時刻を
    /// `ConnectRetry` に覚えておき、そのフレームが来るまで何もしない。
    ///
    /// 繋がっている間は `bool` を 2 つ見るだけで戻るので、設定の複製もしない。
    pub(super) fn poll_device_connection(&mut self) {
        let now = Instant::now();
        let video_due = self.video_retry.is_due(now);
        let audio_due = self.audio_retry.is_due(now);
        if !video_due && !audio_due {
            return;
        }

        // 設定はここで 1 度だけ複製する。デバイスを開いている間 settings の
        // ロックを握らないための措置で、apply_settings と同じ考え方
        let snapshot = match self.settings.lock() {
            Ok(settings) => settings.clone(),
            Err(_) => {
                warn!("デバイスの接続で settings のロックを取得できない");
                return;
            }
        };

        if video_due {
            self.try_connect_video(&snapshot, now);
        }
        if audio_due && self.audio_capabilities_ready(&snapshot, now) {
            self.try_connect_audio(&snapshot, now);
        }
    }

    /// 音声を開いてよいかを返す。対応設定の取得が終わっていなければ `false`。
    ///
    /// 開く処理（`start_passthrough`）は対応設定の一覧を要る。キャッシュが
    /// 無ければその場で列挙することになり、**UI スレッドが 300ms 止まる。**
    /// 取得は起動時とデバイス変更時に別スレッドへ投げてあるので、それが
    /// 届くまでこのフレームは見送る。
    ///
    /// **失敗（`Failed`）は待たない。** 取得できないデバイスを待ち続けると
    /// 音が一切出なくなる。`AUDIO_CAPABILITY_WAIT_LIMIT` を超えた場合も同じ。
    ///
    /// 見送っても `ConnectRetry` は失敗として数えない。バックオフが進むと、
    /// 取得が終わったあとの接続まで遅れてしまう。
    fn audio_capabilities_ready(&mut self, settings: &AppSettings, now: Instant) -> bool {
        let input_key = audio::cache_key(settings.audio.input_device_name.as_deref());
        let output_key = audio::cache_key(settings.audio.output_device_name.as_deref());

        // 設定画面を通らずにデバイス名が変わった場合（設定ファイルの外部編集）
        // でも取りに行けるよう、ここでも要求を積む
        self.settings_dialog
            .audio_input_capabilities_mut()
            .request(&input_key);
        self.settings_dialog
            .audio_output_capabilities_mut()
            .request(&output_key);
        self.dispatch_capability_requests();

        let pending = self
            .settings_dialog
            .audio_input_capabilities()
            .is_pending(&input_key)
            || self
                .settings_dialog
                .audio_output_capabilities()
                .is_pending(&output_key);

        if !pending {
            self.audio_capability_wait_since = None;
            return true;
        }

        let waiting_since = *self.audio_capability_wait_since.get_or_insert(now);
        if now.duration_since(waiting_since) < AUDIO_CAPABILITY_WAIT_LIMIT {
            trace!("音声の対応設定を待っているので、この回の接続は見送る");
            return false;
        }

        warn!(
            "音声の対応設定が {} 秒経っても届かないので、列挙しながら接続する",
            AUDIO_CAPABILITY_WAIT_LIMIT.as_secs()
        );
        self.audio_capability_wait_since = None;
        true
    }

    /// 映像デバイスへの接続を 1 回だけ試す。
    fn try_connect_video(&mut self, settings: &AppSettings, now: Instant) {
        let Some(device_name) = settings.video.device_name.clone() else {
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

        // Arc を複製してから開く。self を借りたまま開くと、結果を書き戻すときに
        // 借用が衝突する
        let video_capture = Arc::clone(&self.video_capture);
        let result = match video_capture.lock() {
            Ok(mut video) => video.start_capture(
                Some(&device_name),
                settings.video.resolution,
                settings.video.format.as_deref(),
                settings.video.fps,
            ),
            Err(_) => Err("video_capture のロックを取得できない".to_string()),
        };

        match result {
            Ok(()) => {
                info!("映像デバイスに接続した");
                self.video_retry.record_success();
                // 繋がったので直前の失敗は消す。プレースホルダーと
                // 「接続状態」タブに古い理由が残らないようにする
                self.errors.clear(ErrorSource::Video);
                self.last_video_device = settings.video.device_name.clone();
                self.last_video_res = settings.video.resolution;
                self.last_video_format = settings.video.format.clone();
                self.last_video_fps = settings.video.fps;
                // 途絶から復帰したのであれば、音声も同時に戻っているはず。
                // **旗はここで落とす。** 残すと、以降の接続のたびに音声を
                // 開き直してしまう
                let recovered = std::mem::take(&mut self.video_reconnect_after_loss);
                self.resync_audio_after_video_recovery(settings, recovered);
            }
            Err(e) => {
                warn!("映像デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                self.video_retry.record_failure(now);
                self.report_error(ErrorSource::Video, e);
                debug!(
                    "映像デバイスへの再試行は {} ms 後",
                    backoff_delay(self.video_retry.attempts()).as_millis()
                );
            }
        }
    }

    /// 映像が途絶から復帰したときに、音声の再接続も要求する。
    ///
    /// 音声の切断は cpal のエラーコールバックが拾うのが主経路で、これはその
    /// 保険。映像が戻った時点でバックオフの残り（最大 5 秒）を飛ばす。
    ///
    /// **ここでもデバイスを開かない。** 要求を立てるだけにして、実際に開くのは
    /// 次のフレームの `poll_device_connection`。
    ///
    /// ロックは video を手放したあとに audio を取る（settings → video → audio）。
    fn resync_audio_after_video_recovery(&mut self, settings: &AppSettings, recovered: bool) {
        if !recovered {
            return;
        }
        let audio_connected = match self.audio_capture.lock() {
            Ok(audio) => audio.active().is_some(),
            Err(_) => {
                warn!("映像の復帰にあわせた音声の確認で audio_capture のロックを取得できない");
                return;
            }
        };
        if !should_resync_audio_after_video(
            recovered,
            audio_connected,
            self.audio_retry.is_active(),
        ) {
            debug!("音声は繋がっているので、映像の復帰にあわせた開き直しはしない");
            return;
        }
        self.last_audio_device = None;
        self.audio_retry.request_now(audio_target(settings));
        info!("映像が戻ったので、音声デバイスの再接続も要求した");
    }

    /// 音声デバイスへの接続を 1 回だけ試す。
    ///
    /// **開けなくても、別のデバイスへは倒さない。** 失敗が続いたときの扱いは
    /// `decide_audio_fallback` を参照。
    fn try_connect_audio(&mut self, settings: &AppSettings, now: Instant) {
        let attempt = self.audio_retry.attempts() + 1;
        info!(
            "音声デバイスへの接続を試す（{} 回目）- 入力: {:?}、出力: {:?}",
            attempt, settings.audio.input_device_name, settings.audio.output_device_name
        );

        // 対応設定は別スレッドで取ったものを渡す。ここで列挙すると UI が止まる。
        // **ロックを取る前に複製する。** キャッシュは `settings_dialog` の中に
        // あり、`audio_capture` のロックを握ったまま `self` を借りられない
        let input_key = audio::cache_key(settings.audio.input_device_name.as_deref());
        let output_key = audio::cache_key(settings.audio.output_device_name.as_deref());
        let input_capabilities = self
            .settings_dialog
            .audio_input_capabilities()
            .ready(&input_key)
            .cloned();
        let output_capabilities = self
            .settings_dialog
            .audio_output_capabilities()
            .ready(&output_key)
            .cloned();

        let audio_capture = Arc::clone(&self.audio_capture);
        let Ok(mut audio) = audio_capture.lock() else {
            let reason = "audio_capture のロックを取得できない".to_string();
            warn!(
                "音声デバイスへの接続に失敗した（{} 回目）: {}",
                attempt, reason
            );
            self.audio_retry.record_failure(now);
            self.report_error(ErrorSource::Audio, reason);
            return;
        };

        // デバイスの列挙は実測で 300ms 前後かかる。設定値との突き合わせに要るのは
        // 最初の 1 回だけなので、再試行のたびには出さない
        if attempt == 1 {
            debug!("利用できる入力デバイス: {:?}", audio.list_input_devices());
            debug!("利用できる出力デバイス: {:?}", audio.list_output_devices());
        }

        // **音量とパススルーの反映は、ストリームを開く前に必ず済ませる。**
        // 開いたあとに反映すると、最初のバッファだけ AudioCapture の既定値
        // （100%・パススルー有効）で鳴ってしまう。音量 0% を保存して
        // 再起動したときに、起動直後だけ音が出るのがこの窓。
        // apply_settings でも同じ値を入れているが、そちらは「接続の要求を
        // 立てる」だけで実際に開くのはこの関数なので、開く直前でも入れておく
        audio.set_volume(settings.ui.volume);
        audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);

        let result = audio.start_passthrough(&PassthroughRequest {
            input_device_name: settings.audio.input_device_name.as_deref(),
            output_device_name: settings.audio.output_device_name.as_deref(),
            sample_rate: settings.audio.sample_rate,
            channels: settings.audio.channels,
            input_capabilities: input_capabilities.as_ref(),
            output_capabilities: output_capabilities.as_ref(),
        });

        let error = match result {
            Ok(()) => {
                info!("音声デバイスに接続した");
                None
            }
            Err(e) => {
                warn!("音声デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                Some(e)
            }
        };

        // 失敗が続いていることを 1 度だけ記録する。倒す先が無いので、
        // ここで開く相手が変わることはない
        if error.is_some() && decide_audio_fallback(attempt) == AudioFallbackAction::WarnAndRetry {
            warn!(
                "音声デバイスに {} 回続けて接続できない。既定のデバイスへは倒さず、戻るまで再試行を続ける",
                attempt
            );
        }

        drop(audio);

        match error {
            None => {
                self.audio_retry.record_success();
                // 繋がったので直前の失敗は消す
                self.errors.clear(ErrorSource::Audio);
                // 形を緩めて繋がった場合も、設定に書かれている値を記録する。
                // ここで実際に開いた値（None）を入れると、設定のレートや
                // チャンネル数へ戻せるようになっても need_audio_restart が
                // 立たず、緩めたままになる
                self.last_audio_device = settings.audio.input_device_name.clone();
                self.last_audio_output = settings.audio.output_device_name.clone();
                self.last_audio_rate = settings.audio.sample_rate;
                self.last_audio_channels = settings.audio.channels;
            }
            Some(reason) => {
                self.audio_retry.record_failure(now);
                self.report_error(ErrorSource::Audio, reason);
                // **取得済みの対応設定を捨てて取り直す。** デバイスが挿し直された
                // 場合、古い一覧でしか開けない設定を選び続けて失敗が繰り返される。
                // 取り直しは別スレッドなので、次の再試行までには届く
                self.settings_dialog
                    .audio_input_capabilities_mut()
                    .retry(&input_key);
                self.settings_dialog
                    .audio_output_capabilities_mut()
                    .retry(&output_key);
                debug!(
                    "音声デバイスへの再試行は {} ms 後",
                    backoff_delay(self.audio_retry.attempts()).as_millis()
                );
            }
        }
    }

    pub(super) fn apply_settings(&mut self, initial: bool) {
        // 設定はここで 1 度だけ複製し、以降はこの複製だけを見る。
        // デバイスの開き直しはリトライの sleep を含めて秒単位かかるため、
        // その間 settings のロックを握っていると他の経路が止まる。
        // 複製しておけば video / audio / screenshot のロックをネストせずに済み、
        // 複数のロックを重ねて取る箇所がこの関数から無くなる
        let snapshot = match self.settings.lock() {
            Ok(settings) => Some(settings.clone()),
            Err(_) => {
                warn!("設定の適用で settings のロックを取得できない");
                None
            }
        };

        if let Some(settings) = snapshot {
            // Video
            //
            // ここではデバイスを開かない。要求を立てるだけにして、実際に開くのは
            // update() から呼ばれる poll_device_connection に任せる。
            // この関数は設定ダイアログや右クリックメニューからも呼ばれるため、
            // ここで開くと失敗したときにその場で UI が止まる
            let need_video_restart = settings.video.device_name != self.last_video_device
                || settings.video.resolution != self.last_video_res
                || settings.video.format != self.last_video_format
                || settings.video.fps != self.last_video_fps;

            if settings.video.device_name.is_some() && (need_video_restart || initial) {
                self.video_retry.request(video_target(&settings));
            }

            // 色空間とレンジはデバイスの開き直しを伴わない。共有の Atomic へ
            // 書くだけで次のフレームから効くので、ここで反映する。
            // 2 秒ごとに video_capture のロックを取らないよう差分で判定する
            let color_conversion = (settings.video.color_space, settings.video.color_range);
            if Self::needs_reapply(initial, &color_conversion, &self.last_color_conversion) {
                if let Ok(video) = self.video_capture.lock() {
                    video.set_color_conversion(color_conversion.0, color_conversion.1);
                    self.last_color_conversion = Some(color_conversion);
                } else {
                    // 次の適用タイミングで入れ直す
                    warn!("色変換の設定で video_capture のロックを取得できない");
                    self.last_color_conversion = None;
                }
            }

            // 明るさ・コントラスト・彩度も係数表へ畳み込まれるだけなので、
            // 色空間・レンジと同じく開き直しを伴わない
            let adjustments = VideoAdjustments::new(
                settings.video.brightness,
                settings.video.contrast,
                settings.video.saturation,
            );
            if Self::needs_reapply(initial, &adjustments, &self.last_video_adjustments) {
                if let Ok(video) = self.video_capture.lock() {
                    video.set_video_adjustments(adjustments);
                    self.last_video_adjustments = Some(adjustments);
                } else {
                    // 次の適用タイミングで入れ直す
                    warn!("映像調整で video_capture のロックを取得できない");
                    self.last_video_adjustments = None;
                }
            }

            // Audio
            //
            // 映像と同じく、ここでは要求を立てるだけ。パススルーの有効・無効と
            // 音量は開き直しを伴わないので、その場で反映する
            let previous_volume = self.volume;
            if let Ok(mut audio) = self.audio_capture.lock() {
                // パススルーと音量は、下の audio_retry.request より前に反映する。
                // ストリームを開いたあとに反映すると、無効のまま（あるいは
                // 音量 0% で）起動したときに最初のバッファだけ出力されてしまう。
                // 実際に開く try_connect_audio でも開く直前に入れ直している
                audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);

                // 音量を適用
                self.volume = settings.ui.volume;
                audio.set_volume(self.volume);

                // ミュートも同じ扱い。ストリームの開き直しは伴わない
                self.muted = settings.ui.muted;
                audio.set_muted(self.muted);
            } else {
                warn!("パススルー・音量・ミュートの反映で audio_capture のロックを取得できない");
            }

            // 設定ダイアログの「適用」「OK」で音量が変わったときも OSD を出す。
            // ホイールや右クリックメニューでの変更は設定側も同時に更新しているため、
            // 2 秒ごとの再適用ではここに入らず、OSD が出っぱなしにはならない。
            // 起動時は変更ではないので出さない
            if !initial && (self.volume - previous_volume).abs() > 0.01 {
                self.show_volume_overlay();
            }

            // 出力デバイスも比較する。入れないと、設定画面で出力先だけを
            // 変えたときに要求が立たず、古い出力先のまま鳴り続ける
            let need_audio_restart = settings.audio.input_device_name != self.last_audio_device
                || settings.audio.output_device_name != self.last_audio_output
                || settings.audio.sample_rate != self.last_audio_rate
                || settings.audio.channels != self.last_audio_channels
                || initial; // 起動時は必ず接続試行

            if need_audio_restart {
                self.audio_retry.request(audio_target(&settings));
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

            // スクリーンショットの効果音
            //
            // **`None`（クリア）も差分として扱う。** 以前は `if let Some(..)` で
            // 包んでいたため、設定画面で「クリア」してもそのセッション中は
            // 効果音が鳴り続けていた
            if let Ok(mut ss) = self.screenshot_manager.lock() {
                // 無条件に呼ぶと 2 秒ごとに効果音ファイル全体を読み直すことになる
                if Self::needs_reapply(
                    initial,
                    &settings.screenshot.sound_file,
                    &self.last_sound_file,
                ) {
                    match &settings.screenshot.sound_file {
                        Some(sf) => match ss.set_sound_file(sf) {
                            Ok(()) => self.last_sound_file = Some(Some(sf.clone())),
                            // 見つからない場合は埋め込みの既定音へ倒して Ok になる。
                            // ここへ来るのはファイルがあるのに読めなかった場合なので、
                            // last を空にして次の適用タイミングで読み直す
                            Err(_) => self.last_sound_file = None,
                        },
                        None => {
                            // 未選択は「鳴らさない」の意味。set_sound_file は
                            // 見つからないファイルを既定音へ倒すので、無音は
                            // ここでしか表せない
                            ss.clear_sound();
                            self.last_sound_file = Some(None);
                        }
                    }
                }
            } else {
                warn!(
                    "スクリーンショットの効果音の反映で screenshot_manager のロックを取得できない"
                );
            }
        }

        if !initial {
            self.last_settings_applied = Instant::now();
        }
    }

    /// デバイスを強制的に開き直す。右クリックメニューの「デバイス再接続」と同じ。
    pub(super) fn reconnect_devices(&mut self) {
        info!("デバイスの再接続を要求された");
        // 強制的にデバイス再接続（last_*をクリアして強制再接続）
        self.last_video_device = None;
        self.last_audio_device = None;
        // 途絶の記録も落とす。開き直したあとの途絶を、改めて
        // 検出してログに残せるようにする
        self.last_video_link_action = VideoLinkAction::Keep;
        // 保留していた音声のエラーも、ここで開き直すので落とす
        self.audio_stream_error_pending = false;
        // ユーザーが明示的にやり直しを求めているので、
        // バックオフの待ち時間を飛ばして次のフレームで試す
        if let Ok(settings) = self.settings.lock() {
            self.video_retry.request_now(video_target(&settings));
            self.audio_retry.request_now(audio_target(&settings));
        } else {
            warn!("デバイスの再接続で settings のロックを取得できない");
        }
        self.apply_settings(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn decide_audio_fallback_never_switches_devices() {
        // #134 の本体。設定のデバイスが消えている間は必ず 3 回失敗するが、
        // ここで既定のデバイス（＝PC のマイク）へ倒すと、それを接続成功として
        // 確定してしまい、設定のデバイスが戻っても繋ぎ直さない。
        // **どの回数でも接続先を変える選択肢が無いこと**を、網羅で確かめる
        for attempt in [1, 2, 3, 4, 5, 100, u32::MAX] {
            assert!(
                matches!(
                    decide_audio_fallback(attempt),
                    AudioFallbackAction::Retry | AudioFallbackAction::WarnAndRetry
                ),
                "attempt = {}",
                attempt
            );
        }
    }

    #[test]
    fn decide_audio_fallback_at_threshold_warns_once() {
        // 3 回目だけ記録する。毎回出すとログが埋まり、一度も出さないと
        // 「音が出ない」の調査でこの状態に気付けない
        assert_eq!(decide_audio_fallback(3), AudioFallbackAction::WarnAndRetry);
    }

    #[test]
    fn decide_audio_fallback_before_and_after_threshold_retries() {
        for attempt in [1, 2, 4, 5, 100] {
            assert_eq!(
                decide_audio_fallback(attempt),
                AudioFallbackAction::Retry,
                "attempt = {}",
                attempt
            );
        }
    }

    #[test]
    fn should_resync_audio_after_video_without_recovery_returns_false() {
        // 起動時の接続では立てない。毎回音声を開き直すと UI が 300ms 止まる
        assert!(!should_resync_audio_after_video(false, false, true));
        assert!(!should_resync_audio_after_video(false, false, false));
    }

    #[test]
    fn should_resync_audio_after_video_when_audio_is_down_returns_true() {
        // 音声が開けていない、または再試行中なら、映像の復帰にあわせて試す
        assert!(should_resync_audio_after_video(true, false, false));
        assert!(should_resync_audio_after_video(true, false, true));
        assert!(should_resync_audio_after_video(true, true, true));
    }

    #[test]
    fn should_resync_audio_after_video_when_audio_is_healthy_returns_false() {
        // 音声が別のデバイス（マイクなど）で無事なら触らない
        assert!(!should_resync_audio_after_video(true, true, false));
    }

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

    #[test]
    fn needs_reapply_path_same_value_returns_false() {
        // PathBuf でも同じ判定になること
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &PathBuf::from("sound/SS.mp3"),
            &Some(PathBuf::from("sound/SS.mp3"))
        ));
    }
}
