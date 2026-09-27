//! デバイスの接続・切断まわりの**判定**。
//!
//! 「途絶したか」「開き直してよいか」「既定デバイスが切り替わったか」
//! 「列挙をログへ出す回か」「設定のデバイスが Windows 側にも見えていないか」を
//! 決める純粋関数だけを置く。実際にデバイスを開く・閉じるのは
//! `super::worker_loop`（デバイスワーカースレッド）で、そこから呼ばれる。
//!
//! 判定をここへ切り出してあるのは、実機でしか作れない状況（USB を抜く、
//! 入力信号を落とす、Windows の既定デバイスを切り替える）をテストで
//! 代替するため。時計もデバイスも触らない。

use crate::i18n::{self, Text};
use crate::video;
use std::fmt;
use std::time::Duration;

/// フレームが途絶えてから「映像が切れた」と判断するまでの時間。
///
/// 60fps なら 1 枚あたり 16ms、30fps でも 33ms なので、3 秒は 100 枚近い
/// 欠落にあたる。一時的なコマ落ちで表示が消えない程度に長く、ユーザーが
/// 「固まった」と気付くより先に反応する程度に短い値として置いている。
pub(super) const VIDEO_SIGNAL_TIMEOUT: Duration = Duration::from_secs(3);

/// ストリームのエラーを理由に音声を開き直すときの、最短の間隔。
///
/// 開いた直後に必ず落ちるデバイスでは、エラー → 開き直し → エラーの繰り返しに
/// なる。音声を開く処理は実測で 300ms 前後かかるため、下限を置いて
/// ワーカーが休みなく開き直すのを防ぐ。
const AUDIO_ERROR_RECONNECT_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// 「既定のデバイス」を追いかけるために、Windows 側の既定デバイス名を
/// 確認する間隔。
///
/// `default_input_device()` / `default_output_device()` は COM 呼び出しを
/// 伴うため、毎フレームは避ける（#135）。5 秒キャッシュの `cached_*_devices`
/// と違って設定ダイアログの開閉に関係なく動かす必要があるので、別のタイマーを持つ。
pub(super) const DEFAULT_AUDIO_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(4);

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
/// - **ストリームを開けていない場合は何もしない。** 接続は `ConnectRetry` の
///   担当で、ここが二重に面倒を見ると起動時の接続と競合する
/// - **1 枚も届いていない場合も何もしない。** 開けた直後は 1 枚目まで実測で
///   0.8 秒かかるうえ、入力信号が無いデバイスは開けても永久にフレームを
///   出さない。ここで切断と見なすと、開き直しを延々と繰り返すことになる
/// - 期限ちょうどは切断とみなす側に倒す。1 フレーム待って得るものが無いため
/// - **デバイス側が喪失を知らせてきた（`device_lost`）なら、途絶時間を待たずに
///   切断とみなす。** DirectShow のグラフの `EC_DEVICE_LOST` などで、抜いた
///   瞬間に分かる。1 枚も届いていなくても切断とみなすのは、これが時間からの
///   推測ではなくデバイスそのものが消えたという知らせだから。入力信号が無い
///   だけのデバイスはこの知らせを出さないので、開き直しが止まらなくなる心配は
///   上の「1 枚も届いていない」の場合と違って無い
pub(super) fn decide_video_link(
    state: video::VideoLinkState,
    auto_reconnect: bool,
    timeout: Duration,
) -> VideoLinkAction {
    if !state.capturing {
        return VideoLinkAction::Keep;
    }
    let lost = state.device_lost
        || state
            .since_last_frame
            .is_some_and(|elapsed| elapsed >= timeout);
    if !lost {
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
pub(super) fn should_reconnect_after_stream_error(since_last_reconnect: Option<Duration>) -> bool {
    match since_last_reconnect {
        None => true,
        Some(elapsed) => elapsed >= AUDIO_ERROR_RECONNECT_MIN_INTERVAL,
    }
}

/// 保留中の音声ストリームのエラーに対して、そのフレームで何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AudioErrorAction {
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
pub(super) fn decide_audio_reconnect(
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
pub(super) fn default_audio_device_changed(
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
pub(super) fn should_poll_default_audio_device(elapsed: Option<Duration>) -> bool {
    match elapsed {
        None => true,
        Some(elapsed) => elapsed >= DEFAULT_AUDIO_DEVICE_POLL_INTERVAL,
    }
}

/// 音声で、この回数だけ連続して失敗したら「繋がっていない」ことをログに残す。
///
/// **ここで別のデバイスへ倒したりはしない。** 以前は同じ 3 回目に既定の
/// デバイスへフォールバックしていた（`decide_audio_fallback` を参照）。
/// 残したのはログだけで、回数を合わせてあるのは、以前のログと同じ位置に
/// 「ここで方針が分かれていた」という目印を置くため。
const AUDIO_RETRY_WARN_AFTER: u32 = 3;

/// 音声の接続に失敗したあと、その場で何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AudioFallbackAction {
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
pub(super) fn decide_audio_fallback(attempt: u32) -> AudioFallbackAction {
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
/// その都度 300ms かかり、音が一瞬途切れる。
pub(super) fn should_resync_audio_after_video(
    video_recovered: bool,
    audio_connected: bool,
    audio_retry_active: bool,
) -> bool {
    if !video_recovered {
        return false;
    }
    !audio_connected || audio_retry_active
}

/// 接続の失敗がこの回数続いたら、列挙の結果を見て「Windows 側にも見えていない」
/// を知らせる（#236）。
///
/// 列挙をログへ出す回（`is_enumeration_milestone`）の最初と揃えてある。
/// 判定に使う一覧はその回の列挙で取るため。バックオフが 200ms から伸びるので、
/// 抜いてから 5 回目の失敗までは 3 秒ほど（途絶の検出を含めて 6 秒ほど）。
/// 挿し直しの USB の再列挙（10 秒以上）より先に知らせる必要は無いが、
/// 抜けたままなら 15 秒以内には出したい。
pub(super) const DEVICE_NOT_VISIBLE_AFTER: u32 = 5;

/// 接続の失敗が続いたときに、列挙の結果をログへ出す回か。
///
/// 5 回目・10 回目・以後 10 回ごと。**毎回は出さない。** 再試行は最大 5 秒
/// 間隔で無限に続くので、毎回だとログが列挙で埋まる。一方で 1 度きりだと、
/// 長く抜けていたあとに挿し直したとき OS 側に戻ったかがログから読めない。
/// 10 回ごとなら 5 秒間隔で 50 秒に 1 度になる。
///
/// 0（失敗していない）は対象外。起動時の列挙は回数と関係なく 1 度だけ出す
/// （`WorkerState::log_device_enumeration`）。
pub(super) fn is_enumeration_milestone(failures: u32) -> bool {
    failures == DEVICE_NOT_VISIBLE_AFTER || (failures >= 10 && failures.is_multiple_of(10))
}

/// 前回列挙を出したときの失敗回数 `logged` から見て、いま列挙を出すか。
///
/// ワーカーの `tick` は 100ms ごとに回るが、失敗回数は接続を試したときしか
/// 動かない。**同じ回数のまま何度も出さない**ために、出した回数を覚えておき
/// それと違うときだけ出す。接続に成功して回数が 0 へ戻ったあと、また 5 回
/// 失敗したら出す（`logged` は 5 のままでも、その間に 0 を経ているので
/// 呼び出し側が記録を 0 へ戻す）。
pub(super) fn should_log_enumeration(failures: u32, logged: u32) -> bool {
    failures != logged && is_enumeration_milestone(failures)
}

/// 設定に書かれたデバイスが、Windows 側の列挙にも出てこない状態。
///
/// 接続の失敗の理由（`VideoError::DeviceNotFound` など）は「アプリが開けない」
/// ことしか言わない。こちらは「OS がそもそも見ていない」ことを伝え、
/// 確かめる場所（デバイスマネージャー）を案内する。**文言はこの型の
/// `Display` が `crate::i18n` から引く。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeviceNotVisible {
    /// 列挙に 1 台も出てこない。ケーブルか、OS 側の認識そのものを疑う
    NoDevices,
    /// ほかのデバイスは出ているが、設定の名前が無い
    NotListed(String),
}

impl fmt::Display for DeviceNotVisible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            DeviceNotVisible::NoDevices => Text::DeviceNotVisibleNoDevices.get().to_string(),
            DeviceNotVisible::NotListed(name) => i18n::device_not_visible_not_listed(name),
        };
        f.write_str(&text)
    }
}

/// 設定のデバイスが Windows 側にも見えていないかを判定する。
///
/// - `failures` — 連続して接続に失敗した回数。`DEVICE_NOT_VISIBLE_AFTER` に
///   届くまでは知らせない。抜いた直後や挿し直しの途中は、見えていなくて当然
/// - `configured` — 設定に書かれた名前。`None`（Windows の既定デバイス）は
///   比べる相手が無いので知らせない
/// - `listed` — 列挙に出てきた名前（設定と同じ表記）。**`None` は列挙そのものに
///   失敗した場合で、知らせない。** 見えていないのか列挙が壊れたのか区別できない
///   ので、理由はログ（WARN）にだけ残す
pub(super) fn decide_device_not_visible(
    failures: u32,
    configured: Option<&str>,
    listed: Option<&[String]>,
) -> Option<DeviceNotVisible> {
    if failures < DEVICE_NOT_VISIBLE_AFTER {
        return None;
    }
    let name = configured?;
    let listed = listed?;
    if listed.iter().any(|listed_name| listed_name == name) {
        return None;
    }
    if listed.is_empty() {
        Some(DeviceNotVisible::NoDevices)
    } else {
        Some(DeviceNotVisible::NotListed(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 列挙のログと「Windows 側にも見えていない」の判定（#236） ----

    #[test]
    fn is_enumeration_milestone_at_5_10_and_every_10_after() {
        let milestones: Vec<u32> = (0..=45).filter(|n| is_enumeration_milestone(*n)).collect();
        assert_eq!(milestones, vec![5, 10, 20, 30, 40]);
    }

    #[test]
    fn should_log_enumeration_only_once_per_milestone() {
        // tick は同じ回数のまま何度も回るので、出した回数とは比べる
        assert!(should_log_enumeration(5, 0));
        assert!(!should_log_enumeration(5, 5), "同じ回数で出し直さない");
        assert!(!should_log_enumeration(6, 5), "節目でない回は出さない");
        assert!(should_log_enumeration(10, 5));
    }

    #[test]
    fn should_log_enumeration_without_failures_is_false() {
        // 繋がっている間（0 回）は出さない。起動時の 1 回は別の経路
        assert!(!should_log_enumeration(0, 5));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn decide_device_not_visible_before_threshold_is_none() {
        let listed = names(&[]);
        assert_eq!(
            decide_device_not_visible(4, Some("Capture"), Some(&listed)),
            None,
            "抜いた直後は見えていなくて当然なので知らせない"
        );
    }

    #[test]
    fn decide_device_not_visible_with_no_devices_at_threshold() {
        let listed = names(&[]);
        assert_eq!(
            decide_device_not_visible(5, Some("Capture"), Some(&listed)),
            Some(DeviceNotVisible::NoDevices)
        );
    }

    #[test]
    fn decide_device_not_visible_with_other_devices_names_the_missing_one() {
        let listed = names(&["Web カメラ"]);
        assert_eq!(
            decide_device_not_visible(12, Some("Capture"), Some(&listed)),
            Some(DeviceNotVisible::NotListed("Capture".to_string()))
        );
    }

    #[test]
    fn decide_device_not_visible_when_the_device_is_listed_is_none() {
        // OS には見えているのに開けない。デバイスマネージャーを案内しても的外れ
        let listed = names(&["Web カメラ", "Capture"]);
        assert_eq!(
            decide_device_not_visible(20, Some("Capture"), Some(&listed)),
            None
        );
    }

    #[test]
    fn decide_device_not_visible_without_configured_name_is_none() {
        // Windows の既定デバイスは比べる相手が無い
        let listed = names(&[]);
        assert_eq!(decide_device_not_visible(20, None, Some(&listed)), None);
    }

    #[test]
    fn decide_device_not_visible_when_enumeration_failed_is_none() {
        // 見えていないのか列挙が壊れたのか区別できない
        assert_eq!(decide_device_not_visible(20, Some("Capture"), None), None);
    }

    #[test]
    fn device_not_visible_display_points_to_the_device_manager() {
        let no_devices = DeviceNotVisible::NoDevices.to_string();
        assert!(no_devices.contains("Windows 側にも"), "{no_devices}");
        assert!(no_devices.contains("デバイスマネージャー"), "{no_devices}");

        let not_listed = DeviceNotVisible::NotListed("Capture".to_string()).to_string();
        assert!(not_listed.contains("'Capture'"), "{not_listed}");
        assert!(not_listed.contains("デバイスマネージャー"), "{not_listed}");

        let english = i18n::with_language(i18n::Language::English, || {
            DeviceNotVisible::NoDevices.to_string()
        });
        assert!(english.contains("Device Manager"), "{english}");
    }

    /// 映像リンクの観測値を組み立てる補助。
    fn link_state(capturing: bool, since_last_frame: Option<Duration>) -> video::VideoLinkState {
        video::VideoLinkState {
            capturing,
            since_last_frame,
            device_lost: false,
        }
    }

    /// デバイス側が喪失を知らせてきた観測値を組み立てる補助。
    fn lost_link_state(
        capturing: bool,
        since_last_frame: Option<Duration>,
    ) -> video::VideoLinkState {
        video::VideoLinkState {
            device_lost: true,
            ..link_state(capturing, since_last_frame)
        }
    }

    #[test]
    fn decide_video_link_device_lost_reconnects_without_waiting_for_timeout() {
        // DirectShow の EC_DEVICE_LOST。フレームが直前まで届いていても、
        // 3 秒待たずにその場で切断として扱う
        let state = lost_link_state(true, Some(Duration::ZERO));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn decide_video_link_device_lost_before_first_frame_reconnects() {
        // 1 枚目が届く前に抜かれた場合。途絶の検出はここでは働かないので、
        // 知らせを受けたら切断とみなす
        let state = lost_link_state(true, None);
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn decide_video_link_device_lost_without_auto_reconnect_only_clears_texture() {
        let state = lost_link_state(true, Some(Duration::ZERO));
        assert_eq!(
            decide_video_link(state, false, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTexture
        );
    }

    #[test]
    fn decide_video_link_device_lost_while_not_capturing_keeps_current_state() {
        // 閉じたあとの面倒は ConnectRetry が見る。閉じたグラフの知らせで
        // 二重に動かない
        let state = lost_link_state(false, None);
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

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
