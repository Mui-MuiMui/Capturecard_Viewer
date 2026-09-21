//! 稼働中のデバイスが生きているかの監視。
//!
//! 起動時の接続は `super::device` の担当で、ここは「一度繋がったあとに
//! 消えた」場合と、Windows 側の既定デバイスが切り替わった場合を扱う。
//! 判断がついたら `super::retry::ConnectRetry` へ要求を積むところまでで、
//! 実際に開き直すのは次のフレームの `poll_device_connection`。

use super::device::{audio_target, video_target};
use super::CaptureCardViewer;
use crate::audio;
use crate::video;
use log::{debug, info, warn};
use std::time::{Duration, Instant};

/// フレームが途絶えてから「映像が切れた」と判断するまでの時間。
///
/// 60fps なら 1 枚あたり 16ms、30fps でも 33ms なので、3 秒は 100 枚近い
/// 欠落にあたる。一時的なコマ落ちで表示が消えない程度に長く、ユーザーが
/// 「固まった」と気付くより先に反応する程度に短い値として置いている。
const VIDEO_SIGNAL_TIMEOUT: Duration = Duration::from_secs(3);

/// ストリームのエラーを理由に音声を開き直すときの、最短の間隔。
///
/// 開いた直後に必ず落ちるデバイスでは、エラー → 開き直し → エラーの繰り返しに
/// なる。音声を開く処理は実測で 300ms 前後かかり、その間 UI スレッドが止まる
/// ため、下限を置いて毎フレーム開き直さないようにする。
const AUDIO_ERROR_RECONNECT_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// 「既定のデバイス」を追いかけるために、Windows 側の既定デバイス名を
/// 確認する間隔。
///
/// `default_input_device()` / `default_output_device()` は COM 呼び出しを
/// 伴うため、毎フレームは避ける（#135）。5 秒キャッシュの `cached_*_devices`
/// と違って設定ダイアログの開閉に関係なく動かす必要があるので、別のタイマーを持つ。
const DEFAULT_AUDIO_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(4);

/// フレームの途絶を見たあと、そのフレームで何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VideoLinkAction {
    /// 何もしない。フレームが流れている、まだ 1 枚も届いていない、
    /// またはストリームを開けていない
    Keep,
    /// 表示中のテクスチャを捨てて「映像信号がありません」に戻す
    ClearTexture,
    /// テクスチャを捨てたうえで、ストリームを閉じて開き直す
    ClearTextureAndReconnect,
}

/// 映像が途絶えたかを判定する。
///
/// 判定をここへ切り出してあるのは、実機でしか作れない状況（USB を抜く、
/// 入力信号を落とす）をテストで代替するため。時計もデバイスも触らない。
///
/// - **ストリームを開けていない場合は何もしない。** 接続は `ConnectRetry` の
///   担当で、ここが二重に面倒を見ると起動時の接続と競合する
/// - **1 枚も届いていない場合も何もしない。** 開けた直後は 1 枚目まで実測で
///   0.8 秒かかるうえ、入力信号が無いデバイスは開けても永久にフレームを
///   出さない。ここで切断と見なすと、開き直しを延々と繰り返すことになる
/// - 期限ちょうどは切断とみなす側に倒す。1 フレーム待って得るものが無いため
fn decide_video_link(
    state: video::VideoLinkState,
    auto_reconnect: bool,
    timeout: Duration,
) -> VideoLinkAction {
    if !state.capturing {
        return VideoLinkAction::Keep;
    }
    let Some(elapsed) = state.since_last_frame else {
        return VideoLinkAction::Keep;
    };
    if elapsed < timeout {
        return VideoLinkAction::Keep;
    }
    if auto_reconnect {
        VideoLinkAction::ClearTextureAndReconnect
    } else {
        VideoLinkAction::ClearTexture
    }
}

/// ストリームのエラーを理由に、いま音声を開き直してよいかを判定する。
///
/// `since_last_reconnect` は前回この理由で開き直してからの経過時間で、
/// `None` は「まだ一度も開き直していない」を表す。
fn should_reconnect_after_stream_error(since_last_reconnect: Option<Duration>) -> bool {
    match since_last_reconnect {
        None => true,
        Some(elapsed) => elapsed >= AUDIO_ERROR_RECONNECT_MIN_INTERVAL,
    }
}

/// 保留中の音声ストリームのエラーに対して、そのフレームで何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioErrorAction {
    /// 何もしない。保留しているエラーが無い
    Idle,
    /// 保留したまま待つ。自動再接続が無効か、開き直しの下限に達していない
    Wait,
    /// ストリームを閉じて開き直す
    Reconnect,
}

/// 保留中の音声エラーの扱いを決める。
///
/// **見送るときも保留を落とさない（`Wait` で持ち越す）。** エラーの通知は
/// `take_stream_error` が読んだ時点で消えるため、ここで捨てると誰も
/// 開き直さないまま音が戻らなくなる。自動再接続を有効にし直したとき、
/// または下限に達したときのフレームで `Reconnect` に変わる。
fn decide_audio_reconnect(
    error_pending: bool,
    auto_reconnect: bool,
    since_last_reconnect: Option<Duration>,
) -> AudioErrorAction {
    if !error_pending {
        return AudioErrorAction::Idle;
    }
    if !auto_reconnect {
        return AudioErrorAction::Wait;
    }
    if !should_reconnect_after_stream_error(since_last_reconnect) {
        return AudioErrorAction::Wait;
    }
    AudioErrorAction::Reconnect
}

/// 「既定のデバイス」設定を追いかけている 1 方向（入力または出力）について、
/// Windows 側の既定が実際に切り替わったかを判定する。
///
/// `configured` は設定に書かれたデバイス名で、`None` が「既定のデバイス」を
/// 表す。`opened` はいま実際に開いている名前（`audio::ActiveAudio` の
/// 該当フィールド）、`current_default` は今回問い合わせた Windows 側の
/// 既定デバイス名。
///
/// **明示的にデバイスを選んでいる場合（`configured` が `Some`）は常に
/// `false`。** 既定の切り替えを追う話であって、設定のデバイスを Windows の
/// 既定へ倒す話ではない（「音声は繋がらなくても別のデバイスへ倒さない」を参照）。
/// `current_default` が取れなかった場合（`None`）も判定を保留する。
fn default_audio_device_changed(
    configured: Option<&str>,
    opened: &str,
    current_default: Option<&str>,
) -> bool {
    if configured.is_some() {
        return false;
    }
    match current_default {
        Some(name) => name != opened,
        None => false,
    }
}

/// Windows 側の既定デバイス名を確認してよい時刻が来ているかを判定する。
/// `elapsed` は前回確認してからの経過時間で、`None` は「一度も確認していない」を表す。
fn should_poll_default_audio_device(elapsed: Option<Duration>) -> bool {
    match elapsed {
        None => true,
        Some(elapsed) => elapsed >= DEFAULT_AUDIO_DEVICE_POLL_INTERVAL,
    }
}

impl CaptureCardViewer {
    /// 稼働中のデバイスが生きているかを見る。`update()` から毎フレーム呼ぶ。
    ///
    /// 起動時の接続は `poll_device_connection` の担当で、ここは「一度繋がった
    /// あとに消えた」場合だけを扱う。判断がついたら `ConnectRetry` へ要求を
    /// 積むところまでで、実際に開き直すのは次のフレームの
    /// `poll_device_connection`。開く処理を 2 か所に持たないため。
    ///
    /// ロックは settings → video → audio の順に 1 つずつ取り、重ねない。
    pub(super) fn monitor_device_health(&mut self) {
        // 設定からは真偽値を 1 つ読むだけで手放す。ここで設定を丸ごと複製すると
        // デバイス名の String が毎フレーム複製される
        let auto_reconnect = match self.settings.lock() {
            Ok(settings) => settings.video.auto_reconnect,
            Err(_) => {
                warn!("デバイスの監視で settings のロックを取得できない");
                return;
            }
        };

        self.monitor_video_link(auto_reconnect);
        self.monitor_audio_stream(auto_reconnect);
    }

    /// フレームの途絶を見て、表示を落とし、必要なら映像を開き直す。
    fn monitor_video_link(&mut self, auto_reconnect: bool) {
        let state = match self.video_capture.lock() {
            Ok(video) => video.link_state(),
            Err(_) => {
                warn!("デバイスの監視で video_capture のロックを取得できない");
                return;
            }
        };
        // 描画側が参照する値をここで更新する。ロックは既に手放している
        self.video_capturing = state.capturing;

        let action = decide_video_link(state, auto_reconnect, VIDEO_SIGNAL_TIMEOUT);
        if action == VideoLinkAction::Keep {
            // 途絶が解消した（開き直した、ストリームを閉じた）。記録も戻して、
            // 次の途絶をもう一度検出できるようにする
            self.last_video_link_action = action;
            return;
        }
        // 途絶は毎フレーム同じ判定に当たる。同じ扱いが続く間は 1 度だけ動く。
        // **「動いたかどうか」ではなく「何をしたか」で見る。** 自動再接続を
        // 切ったまま途絶（ClearTexture）したあとに有効化すると判定が
        // ClearTextureAndReconnect へ変わるので、そこで開き直せる
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
        // テクスチャを捨てて「映像信号がありません」の表示へ戻す
        self.video_texture = None;

        if action != VideoLinkAction::ClearTextureAndReconnect {
            debug!("自動再接続が無効なので映像は開き直さない");
            return;
        }

        // 開き直しの対象は設定から取り直す
        let target = match self.settings.lock() {
            Ok(settings) => video_target(&settings),
            Err(_) => {
                warn!("映像の再接続で settings のロックを取得できない");
                return;
            }
        };

        // ストリームを閉じてから要求する。閉じておくと表示が
        // 「デバイスが接続されていません」へ変わり、信号だけが無い状態と区別できる
        match self.video_capture.lock() {
            Ok(mut video) => video.stop_capture(),
            Err(_) => {
                warn!("映像の再接続で video_capture のロックを取得できない");
                return;
            }
        }
        self.video_capturing = false;

        // 既存のバックオフへ乗せる。**ここでデバイスを列挙しない。**
        // 対象が戻っているかは `start_capture` の中の列挙（実測 1〜3ms）が
        // 確かめる。再試行の間隔は最大 5 秒で頭打ちなので、
        // MediaFoundation への問い合わせもその頻度を超えない
        self.last_video_device = None;
        // 次に繋がったときは、同じ USB 機器の音声も戻っているとみなして
        // 音声の再接続も要求する。起動時の接続と区別するためにここで立てる
        self.video_reconnect_after_loss = true;
        self.video_retry.request_now(target);
        info!("映像デバイスの再接続を要求した");
    }

    /// 音声ストリームのエラーを拾って、必要なら開き直す。
    ///
    /// cpal のエラーコールバックはストリームのスレッドから呼ばれるため、
    /// そこでは旗を立てるだけにしてある（`audio::AudioCapture::take_stream_error`）。
    fn monitor_audio_stream(&mut self, auto_reconnect: bool) {
        let errored = match self.audio_capture.lock() {
            Ok(audio) => audio.take_stream_error(),
            Err(_) => {
                warn!("デバイスの監視で audio_capture のロックを取得できない");
                return;
            }
        };
        if errored {
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
            // 保留しているエラーが無い
            AudioErrorAction::Idle => return,
            // 保留したまま待つ。自動再接続を有効にし直したとき、または
            // 下限に達したときのフレームでここを抜ける。
            // 毎フレーム通るのでログは出さない
            AudioErrorAction::Wait => return,
            AudioErrorAction::Reconnect => {}
        }

        let target = match self.settings.lock() {
            Ok(settings) => audio_target(&settings),
            Err(_) => {
                warn!("音声の再接続で settings のロックを取得できない");
                return;
            }
        };

        match self.audio_capture.lock() {
            Ok(mut audio) => audio.stop_capture(),
            Err(_) => {
                warn!("音声の再接続で audio_capture のロックを取得できない");
                return;
            }
        }

        self.audio_stream_error_pending = false;
        self.last_audio_error_reconnect = Some(Instant::now());
        self.last_audio_device = None;
        self.audio_retry.request_now(target);
        info!("音声デバイスの再接続を要求した");
    }

    /// 「既定のデバイス」設定（入力・出力のどちらか、または両方）が、
    /// Windows 側の既定切り替えに追従しているかを確認する。`update()` から
    /// 毎フレーム呼ぶが、内部でタイマーを見て `DEFAULT_AUDIO_DEVICE_POLL_INTERVAL`
    /// おきにしか動かない（#135）。
    ///
    /// cpal は WASAPI の `IMMNotificationClient` を公開しておらず、既定
    /// デバイスの切り替えを通知では受け取れない。`default_input_device()` /
    /// `default_output_device()` を都度問い合わせて名前を突き合わせるしかなく、
    /// この呼び出しは COM を伴うため毎フレームは避ける。
    ///
    /// **既存の 5 秒キャッシュ（`update_cached_device_lists`）には相乗りしない。**
    /// あちらは設定ダイアログを描画している間しか呼ばれないため、閉じている間は
    /// 既定の切り替えに気付けなくなる。
    ///
    /// 判定そのものは純粋関数 `default_audio_device_changed` に切り出してある。
    pub(super) fn poll_default_audio_device(&mut self) {
        let elapsed = self.last_default_audio_check.map(|last| last.elapsed());
        if !should_poll_default_audio_device(elapsed) {
            return;
        }
        self.last_default_audio_check = Some(Instant::now());

        // 既に音声の再接続を追いかけている最中なら何もしない。ストリームの
        // エラーコールバックや映像復帰による再接続と要求が重なるのを防ぐための
        // もので、`ConnectRetry` に「同じ対象なら要求を積み直さない」仕組みが
        // 既にあるが、ここでは対象を確定させる前に丸ごと見送る
        if self.audio_retry.is_active() {
            return;
        }

        let settings = match self.settings.lock() {
            Ok(settings) => settings.clone(),
            Err(_) => {
                warn!("既定音声デバイスの監視で settings のロックを取得できない");
                return;
            }
        };
        let track_input = settings.audio.input_device_name.is_none();
        let track_output = settings.audio.output_device_name.is_none();
        if !track_input && !track_output {
            // 入出力とも明示的にデバイスを選んでいるので、追いかける対象が無い
            return;
        }

        let Ok(audio) = self.audio_capture.lock() else {
            warn!("既定音声デバイスの監視で audio_capture のロックを取得できない");
            return;
        };
        // まだ何も開けていない（起動直後・再接続中）なら、開いた時点の名前が
        // 無いので比べようがない。poll_device_connection の担当
        let Some(active) = audio.active() else {
            return;
        };
        let current_input = if track_input {
            audio.default_input_device_name()
        } else {
            None
        };
        let current_output = if track_output {
            audio.default_output_device_name()
        } else {
            None
        };
        drop(audio);

        let input_switched = track_input
            && default_audio_device_changed(
                settings.audio.input_device_name.as_deref(),
                &active.input_device,
                current_input.as_deref(),
            );
        let output_switched = track_output
            && default_audio_device_changed(
                settings.audio.output_device_name.as_deref(),
                &active.output_device,
                current_output.as_deref(),
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
        // サンプリングレートやチャンネル数のまま開こうとしうる。
        // 接続に失敗したときの取り直し（`try_connect_audio`）と同じ扱い
        if input_switched {
            self.settings_dialog
                .audio_input_capabilities_mut()
                .retry(&audio::cache_key(None));
        }
        if output_switched {
            self.settings_dialog
                .audio_output_capabilities_mut()
                .retry(&audio::cache_key(None));
        }
        self.dispatch_capability_requests();

        match self.audio_capture.lock() {
            Ok(mut audio) => audio.stop_capture(),
            Err(_) => {
                warn!("既定音声デバイスの再接続で audio_capture のロックを取得できない");
                return;
            }
        }
        self.last_audio_device = None;
        self.last_audio_output = None;
        self.audio_retry.request_now(audio_target(&settings));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 映像リンクの観測値を組み立てる補助。
    fn link_state(capturing: bool, since_last_frame: Option<Duration>) -> video::VideoLinkState {
        video::VideoLinkState {
            capturing,
            since_last_frame,
        }
    }

    #[test]
    fn decide_video_link_not_capturing_keeps_current_state() {
        // ストリームを開けていない間の面倒は ConnectRetry が見る。
        // ここで手を出すと起動時の接続と二重になる
        let state = link_state(false, Some(Duration::from_secs(60)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_no_frame_yet_keeps_current_state() {
        // 開いた直後は 1 枚目まで実測で 0.8 秒かかる。入力信号が無いデバイスは
        // 開けても永久にフレームを出さないので、切断と見なすと開き直しが止まらない
        let state = link_state(true, None);
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_just_before_timeout_keeps_current_state() {
        let state = link_state(true, Some(Duration::from_millis(2999)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_exactly_at_timeout_disconnects() {
        // 境界は切断とみなす側に倒す
        let state = link_state(true, Some(Duration::from_secs(3)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn decide_video_link_after_timeout_without_auto_reconnect_only_clears() {
        // 自動再接続を切っていても、止まった画を残し続けない
        let state = link_state(true, Some(Duration::from_secs(10)));
        assert_eq!(
            decide_video_link(state, false, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTexture
        );
    }

    #[test]
    fn decide_video_link_zero_timeout_disconnects_on_any_gap() {
        // 閾値を 0 にした場合、経過が 0 でも切断側へ倒れる（境界の確認）
        let state = link_state(true, Some(Duration::ZERO));
        assert_eq!(
            decide_video_link(state, true, Duration::ZERO),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn decide_video_link_action_changes_when_auto_reconnect_is_turned_on() {
        // 自動再接続を切ったまま途絶したあとに有効化した場合。
        // 呼び出し側は「何をしたか」と比べて動くので、判定が変われば
        // 開き直しへ進める（真偽値のラッチだと遮られてしまう）
        let state = link_state(true, Some(Duration::from_secs(10)));
        let before = decide_video_link(state, false, VIDEO_SIGNAL_TIMEOUT);
        let after = decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT);

        assert_eq!(before, VideoLinkAction::ClearTexture);
        assert_eq!(after, VideoLinkAction::ClearTextureAndReconnect);
        assert_ne!(before, after);
    }

    #[test]
    fn decide_audio_reconnect_without_pending_error_is_idle() {
        assert_eq!(
            decide_audio_reconnect(false, true, None),
            AudioErrorAction::Idle
        );
        // 保留が無ければ、間隔の下限に達していても何もしない
        assert_eq!(
            decide_audio_reconnect(false, true, Some(Duration::from_secs(600))),
            AudioErrorAction::Idle
        );
    }

    #[test]
    fn decide_audio_reconnect_pending_error_reconnects() {
        assert_eq!(
            decide_audio_reconnect(true, true, None),
            AudioErrorAction::Reconnect
        );
    }

    #[test]
    fn decide_audio_reconnect_without_auto_reconnect_waits() {
        // 見送るだけで、保留は呼び出し側に残る。捨てると音が戻らなくなる
        assert_eq!(
            decide_audio_reconnect(true, false, None),
            AudioErrorAction::Wait
        );
    }

    #[test]
    fn decide_audio_reconnect_within_minimum_interval_waits() {
        assert_eq!(
            decide_audio_reconnect(true, true, Some(Duration::from_millis(4999))),
            AudioErrorAction::Wait
        );
    }

    #[test]
    fn decide_audio_reconnect_after_minimum_interval_reconnects() {
        // 下限に達したフレームで、保留していたエラーが処理される
        assert_eq!(
            decide_audio_reconnect(true, true, Some(Duration::from_secs(5))),
            AudioErrorAction::Reconnect
        );
    }

    #[test]
    fn default_audio_device_changed_explicit_device_returns_false() {
        // 明示的にデバイスを選んでいる向きは、既定が変わっても関係ない
        assert!(!default_audio_device_changed(
            Some("USB マイク"),
            "USB マイク",
            Some("別のマイク")
        ));
    }

    #[test]
    fn default_audio_device_changed_same_name_returns_false() {
        // 開いている名前と Windows 側の既定が一致していれば追従済み
        assert!(!default_audio_device_changed(
            None,
            "スピーカー (Realtek)",
            Some("スピーカー (Realtek)")
        ));
    }

    #[test]
    fn default_audio_device_changed_different_name_returns_true() {
        // Windows 側で既定が切り替わり、開いている名前と食い違っている
        assert!(default_audio_device_changed(
            None,
            "スピーカー (Realtek)",
            Some("ヘッドセット (USB)")
        ));
    }

    #[test]
    fn default_audio_device_changed_current_default_unknown_returns_false() {
        // 既定デバイスの問い合わせ自体に失敗した場合は判定を保留する
        assert!(!default_audio_device_changed(
            None,
            "スピーカー (Realtek)",
            None
        ));
    }

    #[test]
    fn should_poll_default_audio_device_never_checked_returns_true() {
        // 一度も確認していない状態では必ず確認する
        assert!(should_poll_default_audio_device(None));
    }

    #[test]
    fn should_poll_default_audio_device_just_before_interval_returns_false() {
        assert!(!should_poll_default_audio_device(Some(
            Duration::from_millis(3999)
        )));
    }

    #[test]
    fn should_poll_default_audio_device_at_interval_returns_true() {
        // 境界。ちょうど 4000ms で確認する
        assert!(should_poll_default_audio_device(Some(
            Duration::from_millis(4000)
        )));
    }

    #[test]
    fn should_poll_default_audio_device_long_after_interval_returns_true() {
        assert!(should_poll_default_audio_device(Some(Duration::from_secs(
            3600
        ))));
    }

    #[test]
    fn should_reconnect_after_stream_error_first_time_returns_true() {
        // 一度も開き直していないなら待たせない
        assert!(should_reconnect_after_stream_error(None));
    }

    #[test]
    fn should_reconnect_after_stream_error_just_reconnected_returns_false() {
        assert!(!should_reconnect_after_stream_error(Some(
            Duration::from_millis(10)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_just_before_interval_returns_false() {
        assert!(!should_reconnect_after_stream_error(Some(
            Duration::from_millis(4999)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_at_interval_returns_true() {
        assert!(should_reconnect_after_stream_error(Some(
            Duration::from_secs(5)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_long_after_returns_true() {
        assert!(should_reconnect_after_stream_error(Some(
            Duration::from_secs(600)
        )));
    }
}
