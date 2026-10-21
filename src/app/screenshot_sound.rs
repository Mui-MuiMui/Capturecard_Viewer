//! スクリーンショットの効果音ファイルの読み込みと再生。
//!
//! 適用（`apply_settings`）と設定画面の「テスト再生」の両方の読み込みを、
//! 要求ごとに起こすスレッドで行う。再生も 1 回ごとのスレッドで行う。
//! UI スレッドは結果をチャネルで受け取り、反映と失敗の報告だけをする。
//! スクリーンショットの保存スレッドと同じ流儀（`docs/design/threads.md`）。
//! ただし保存スレッドと違い、読み込みスレッドは終了時に待たない。
//!
//! 大きなファイルや遅いドライブでは、読み込みとデコードの確認に時間がかかる。
//! UI スレッドで行うと、適用やテスト再生の瞬間に描画が止まる（Issue #214）。

use super::CaptureCardViewer;
use crate::screenshot::ScreenshotError;
use crate::screenshot_sound;
use crate::status::ErrorSource;
use chrono::Local;
use log::{debug, warn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 読めない効果音ファイルを自動で読み直すまでの最初の間隔（#507）。
/// 2 秒ごとの適用より十分に長くし、読み込みスレッドを詰めて起こさない
const SOUND_RETRY_INITIAL: Duration = Duration::from_secs(30);
/// 自動で読み直す間隔の上限。失敗するたびに倍にしていき、ここで止める
const SOUND_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// 読めない効果音ファイルの自動の読み直しの予定。
///
/// `delay` は直前に使った間隔で、次に失敗したらこれの倍にする。`next_at` は
/// 次に読み直す時刻。読み直しを始めたら `None` にし、結果を待つ。
#[derive(Debug, Clone, Copy)]
pub(super) struct SoundLoadRetry {
    delay: Duration,
    next_at: Option<Instant>,
}

/// 効果音のスレッドから UI スレッドへ返すもの。
pub(super) enum SoundMessage {
    /// 読み込みの結果（適用・テスト再生）
    Loaded(SoundLoadResult),
    /// 再生スレッドが出力先を開けたか。開けなかったときは理由
    Output(Result<(), ScreenshotError>),
}

/// 読み込みスレッドから UI スレッドへ返す結果。
///
/// **失敗しても鳴らせるデータは必ず入っている。** `load_sound_data` は
/// 見つからない・読めないファイルを内蔵音へ倒し、デコードできないファイルは
/// 中身をそのまま返すため。`error` は報告するためだけのもの。
pub(super) struct SoundLoadResult {
    purpose: SoundLoadPurpose,
    data: Arc<[u8]>,
    error: Option<ScreenshotError>,
}

/// 何のための読み込みか。受け取った側の扱いがまったく違う。
enum SoundLoadPurpose {
    /// 撮影時に鳴らす音として反映する。`id` は `ScreenshotManager::begin_load` の番号
    Apply { id: u64 },
    /// その場で 1 回鳴らす。適用済みの音には触れない。
    /// `volume` はボタンを押した時点のドラフトの音量
    TestPlay { id: u64, volume: f32 },
}

impl CaptureCardViewer {
    /// 撮影時に鳴らす効果音として `path` の読み込みを始める。
    ///
    /// 結果が届くまでは直前の音（無ければ内蔵音）で鳴る。続けて呼ぶと、
    /// 先の読み込みの結果は届いても捨てられる。
    pub(super) fn request_sound_apply(&mut self, path: &Path) {
        let Some(id) = self
            .screenshot_manager
            .lock()
            .ok()
            .map(|mut ss| ss.begin_load())
        else {
            warn!("効果音の適用で screenshot_manager のロックを取得できない");
            return;
        };
        self.spawn_sound_load(path.to_path_buf(), SoundLoadPurpose::Apply { id });
    }

    /// 設定画面の「テスト再生」の読み込みを始める。読み込めたら `volume` で鳴らす。
    pub(super) fn request_test_sound(&mut self, path: &Path, volume: f32) {
        let Some(id) = self
            .screenshot_manager
            .lock()
            .ok()
            .map(|mut ss| ss.begin_test_play())
        else {
            warn!("テスト再生で screenshot_manager のロックを取得できない");
            return;
        };
        self.spawn_sound_load(
            path.to_path_buf(),
            SoundLoadPurpose::TestPlay { id, volume },
        );
    }

    fn spawn_sound_load(&mut self, path: PathBuf, purpose: SoundLoadPurpose) {
        let result_tx = self.sound_tx.clone();
        // 映像が止まっている間は update() の間隔が広がっているので、届いたら起こす。
        // 起こさないとテスト再生の音が次の再描画まで鳴らない
        let waker = self.repaint_waker.clone();
        // **`JoinHandle` は持たず、終了時に待たない。** 読むだけで副作用が無く、
        // ネットワークドライブ上のファイルは読み終わりが読めないため（#507。
        // スクリーンショットの保存スレッドとは違う。docs/design/threads.md）
        std::thread::spawn(move || {
            let (data, error) = screenshot_sound::load_sound_data(&path);
            // ログは受け取った UI スレッド側で出す（保存スレッドと同じ）
            if result_tx
                .send(SoundMessage::Loaded(SoundLoadResult {
                    purpose,
                    data,
                    error,
                }))
                .is_err()
            {
                // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                debug!("効果音の読み込み結果の送り先が既に無いので捨てる");
                return;
            }
            waker.wake();
        });
    }

    /// 設定ダイアログの「適用」「OK」で呼ぶ。最後に適用した効果音ファイルを
    /// 読み込めていなければ、続く `apply_settings` で読み直させる。
    ///
    /// 2 秒ごとの適用では読み直さない（#507）。ファイルを直した利用者がすぐに
    /// 読み直させる契機。設定を変えていなくても押せば読み直す。自動の読み直しの
    /// 間隔は最初（30 秒）からやり直す。
    pub(super) fn retry_failed_sound_load(&mut self) {
        if should_retry_sound_load(self.sound_load_failure.as_deref()) {
            self.last_sound_file = None;
            self.sound_load_retry = None;
        }
    }

    /// 読めない効果音ファイルの自動の読み直しの時刻が来ていれば、続く適用で
    /// 読み直させる。`apply_settings` が効果音の差分判定の前に呼ぶ。
    ///
    /// 間隔は 30 秒から失敗するたびに倍にし、5 分で止める（`next_sound_retry`）。
    /// 起動直後にネットワークドライブがまだ繋がっていなかった場合などに、
    /// 利用者が何もしなくても戻るようにするため。
    pub(super) fn start_due_sound_retry(&mut self, now: Instant) {
        let Some(retry) = self.sound_load_retry.as_mut() else {
            return;
        };
        if is_sound_retry_due(retry.next_at, now) {
            debug!(
                "読めなかった効果音ファイルを読み直す（間隔 {:?}）",
                retry.delay
            );
            // 結果が届くまでは次を予定しない
            retry.next_at = None;
            self.last_sound_file = None;
        }
    }

    /// 効果音を別スレッドで 1 回鳴らす。撮影とテスト再生の両方がここを通る。
    ///
    /// 出力先を開けたかは `SoundMessage::Output` で UI スレッドへ返る。
    /// **`screenshot_manager` のロックを握ったまま呼ばない。** 渡すのは `Arc` の複製だけ。
    pub(super) fn play_sound(&self, data: Arc<[u8]>, volume: f32) {
        let tx = self.sound_tx.clone();
        let waker = self.repaint_waker.clone();
        screenshot_sound::play_sound_data(data, volume, move |outcome| {
            let failed = outcome.is_err();
            // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
            if tx.send(SoundMessage::Output(outcome)).is_ok() && failed {
                // 映像が止まっている間でも、通知がすぐ出るよう起こす
                waker.wake();
            }
        });
    }

    /// 別スレッドから届いた効果音の読み込みと再生の結果を取り込む。`update()` の先頭で呼ぶ。
    pub(super) fn drain_sound_results(&mut self) {
        while let Ok(message) = self.sound_rx.try_recv() {
            match message {
                SoundMessage::Loaded(result) => self.apply_sound_load_result(result),
                SoundMessage::Output(outcome) => self.apply_sound_output(outcome),
            }
        }
    }

    /// 再生スレッドが出力先を開けたかを取り込む。
    ///
    /// **同じ理由が続く間は 1 度だけ報告する。** 撮影のたびに鳴らそうとするので、
    /// 出力デバイスが無いまま撮り続けると同じ失敗が撮影の回数だけ届く。
    /// 一定時間ごとの再通知（`ErrorCenter` の間隔）も入れていない。画像は保存できて
    /// いて、撮るたびに思い出させる必要が無いため。理由は「接続状態」タブに残る。
    ///
    /// 出力先を開けるようになったら記録を落とし（タブからも消える）、次に開けなく
    /// なったときはまた知らせる。
    fn apply_sound_output(&mut self, outcome: Result<(), ScreenshotError>) {
        let reason = outcome.err().map(|e| e.to_string());
        let report = should_report_sound_failure(self.sound_output_failure.as_deref(), &reason);
        match &reason {
            Some(reason) if report => {
                warn!("効果音を鳴らせない: {}", reason);
                self.report_error(ErrorSource::ScreenshotSound, reason.clone());
            }
            Some(reason) => debug!("効果音を鳴らせない（報告済み）: {}", reason),
            None if self.sound_output_failure.is_some() => {
                self.errors.clear(ErrorSource::ScreenshotSound);
            }
            None => {}
        }
        self.sound_output_failure = reason;
    }

    fn apply_sound_load_result(&mut self, result: SoundLoadResult) {
        let SoundLoadResult {
            purpose,
            data,
            error,
        } = result;

        match purpose {
            SoundLoadPurpose::Apply { id } => {
                // report_error は self 全体を借りるので、ロックはここで離す
                let accepted = match self.screenshot_manager.lock() {
                    Ok(mut ss) => ss.finish_load(id, data),
                    Err(_) => {
                        warn!("効果音の反映で screenshot_manager のロックを取得できない");
                        false
                    }
                };
                if !accepted {
                    // 後から出した要求があるか、「鳴らさない」へ切り替えられた。
                    // 失敗も報告しない。報告すべきなのは最後の要求の結果だけ
                    debug!("古い効果音の読み込み結果が届いたので捨てる");
                    return;
                }
                let Some(e) = error else {
                    // 読み込めたので、前に読めなかった記録を取り下げる。ただし
                    // 出力先を開けない失敗が続いている間は、記録をその理由へ戻す
                    // （読めなかった記録が上書きしていることがあるため）。
                    // 報告済みの理由なので、トーストは出さない
                    self.sound_load_failure = None;
                    self.sound_load_retry = None;
                    match self.sound_output_failure.clone() {
                        None => self.errors.clear(ErrorSource::ScreenshotSound),
                        Some(reason) => self.record_sound_failure_quietly(reason),
                    }
                    return;
                };
                // **読めなくても適用済みのまま（`last_sound_file`）にする。** 2 秒ごとの
                // 適用では読み直さず、読み直すのは設定を変えたとき、設定ダイアログの
                // 「適用」「OK」（`retry_failed_sound_load`）、読めないファイルなら間隔を
                // 延ばしながらの自動の読み直し（`start_due_sound_retry`）だけ（#507）。
                // 読めないファイルの撮影時は内蔵音、デコードできないファイルは無音
                // （docs/design/assets.md）
                let now = Instant::now();
                self.sound_load_retry =
                    next_sound_retry(&e, self.sound_load_retry.map(|retry| retry.delay)).map(
                        |delay| SoundLoadRetry {
                            delay,
                            next_at: Some(now + delay),
                        },
                    );

                // 同じ理由が続く間は 1 度だけ報告する。自動の読み直しや「適用」の押し直しで
                // 同じ理由で読めなかったときは、トーストを出さず記録だけをその理由へ戻す
                let reason = e.to_string();
                let report = should_report_sound_failure(
                    self.sound_load_failure.as_deref(),
                    &Some(reason.clone()),
                );
                self.sound_load_failure = Some(reason.clone());
                if report {
                    warn!("効果音の適用: {}", reason);
                    self.report_error(ErrorSource::ScreenshotSound, reason);
                } else {
                    debug!("効果音の適用（報告済み）: {}", reason);
                    self.record_sound_failure_quietly(reason);
                }
            }
            SoundLoadPurpose::TestPlay { id, volume } => {
                let accepted = match self.screenshot_manager.lock() {
                    Ok(mut ss) => ss.finish_test_play(id),
                    Err(_) => {
                        warn!("テスト再生で screenshot_manager のロックを取得できない");
                        false
                    }
                };
                if !accepted {
                    debug!("後からテスト再生が押されたので、古い読み込み結果は鳴らさない");
                    return;
                }
                // 読めなければ内蔵音で鳴らし、理由をトーストへ出す
                if let Some(e) = error {
                    warn!("テスト再生で効果音を読み込めない: {}", e);
                    self.report_error(ErrorSource::ScreenshotSound, e.to_string());
                }
                self.play_sound(data, volume);
            }
        }
    }

    /// 報告済みの理由を、トーストを出さずに「接続状態」タブの記録へ戻す。
    ///
    /// 記録は発生源ごとに 1 件なので、別の失敗（テスト再生で読めなかった別の
    /// ファイルなど）が上書きしていることがある。既にその理由なら何もしない。
    fn record_sound_failure_quietly(&mut self, reason: String) {
        let latest = self
            .errors
            .latest(ErrorSource::ScreenshotSound)
            .map(|recorded| recorded.message.as_str());
        if latest != Some(reason.as_str()) {
            self.errors.record(
                ErrorSource::ScreenshotSound,
                reason,
                Instant::now(),
                Local::now(),
            );
        }
    }
}

/// 効果音の失敗（再生の出力先・適用の読み込み）を報告するかを決める。
///
/// `previous` は前回の失敗の理由（成功していれば `None`）、`current` は今回の
/// 結果。失敗したうえで、前回と違う理由のときだけ報告する。
fn should_report_sound_failure(previous: Option<&str>, current: &Option<String>) -> bool {
    match current {
        Some(reason) => previous != Some(reason.as_str()),
        None => false,
    }
}

/// 設定ダイアログの「適用」「OK」で効果音ファイルを読み直すかを決める。
///
/// `load_failure` は最後の適用で読み込めなかった理由（読み込めていれば `None`）。
/// 読み込めなかったときだけ読み直す。読めているファイルを押すたびに読み直さない。
fn should_retry_sound_load(load_failure: Option<&str>) -> bool {
    load_failure.is_some()
}

/// 適用の読み込みに失敗したあと、自動で読み直すまでの間隔を決める。
///
/// `previous` は直前の自動の読み直しで使った間隔（初めての失敗なら `None`）。
/// 最初は 30 秒、失敗するたびに倍にして 5 分で止める。**デコードできない
/// ファイルは自動では読み直さない（`None`）。** 中身を差し替えない限り結果が
/// 変わらず、大きなファイルを数分ごとに読み直すだけになるため。こちらは
/// 設定の変更か「適用」「OK」で読み直す。
fn next_sound_retry(error: &ScreenshotError, previous: Option<Duration>) -> Option<Duration> {
    if matches!(error, ScreenshotError::SoundFileUndecodable { .. }) {
        return None;
    }
    Some(match previous {
        None => SOUND_RETRY_INITIAL,
        Some(delay) => (delay * 2).min(SOUND_RETRY_MAX),
    })
}

/// 自動の読み直しの時刻が来たかを決める。`next_at` が `None`（読み直しの
/// 結果を待っている）なら来ていない。
fn is_sound_retry_due(next_at: Option<Instant>, now: Instant) -> bool {
    next_at.is_some_and(|at| now >= at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_sound_failure_first_failure_is_reported() {
        assert!(should_report_sound_failure(None, &Some("NoDevice".into())));
    }

    #[test]
    fn should_report_sound_failure_same_failure_again_is_not_reported() {
        // 出力デバイスが無いまま撮り続けた場合。撮影のたびにトーストを出さない
        assert!(!should_report_sound_failure(
            Some("NoDevice"),
            &Some("NoDevice".into())
        ));
    }

    #[test]
    fn should_report_sound_failure_different_failure_is_reported() {
        assert!(should_report_sound_failure(
            Some("NoDevice"),
            &Some("Busy".into())
        ));
    }

    #[test]
    fn should_report_sound_failure_success_is_never_reported() {
        // 開けたときは報告しない。記録は落とすので、次に開けなくなったらまた知らせる
        assert!(!should_report_sound_failure(Some("NoDevice"), &None));
        assert!(!should_report_sound_failure(None, &None));
    }

    #[test]
    fn should_report_sound_failure_same_load_failure_on_retry_is_not_reported() {
        // 読めないファイルのまま「適用」を押し直した場合（#507）。
        // 押すたびに同じトーストを出さない
        let reason = ScreenshotError::SoundFileUnreadable {
            path: PathBuf::from("Z:/sounds/SS.mp3"),
            source: "network path not found".to_string(),
        }
        .to_string();
        assert!(!should_report_sound_failure(
            Some(reason.as_str()),
            &Some(reason.clone())
        ));
    }

    #[test]
    fn should_report_sound_failure_load_failure_with_other_reason_is_reported() {
        // 読めなかったファイルを壊れた（デコードできない）ものへ差し替えた場合
        let path = PathBuf::from("Z:/sounds/SS.mp3");
        let unreadable = ScreenshotError::SoundFileUnreadable {
            path: path.clone(),
            source: "network path not found".to_string(),
        }
        .to_string();
        let undecodable = ScreenshotError::SoundFileUndecodable {
            path,
            source: "unrecognized format".to_string(),
        }
        .to_string();
        assert!(should_report_sound_failure(
            Some(unreadable.as_str()),
            &Some(undecodable)
        ));
    }

    #[test]
    fn should_retry_sound_load_after_failure_returns_true() {
        // 直したファイルを「適用」で読み直せること
        assert!(should_retry_sound_load(Some("読めない")));
    }

    #[test]
    fn should_retry_sound_load_after_success_returns_false() {
        // 読めているファイルは「適用」のたびに読み直さない
        assert!(!should_retry_sound_load(None));
    }

    fn unreadable() -> ScreenshotError {
        ScreenshotError::SoundFileUnreadable {
            path: PathBuf::from("Z:/sounds/SS.mp3"),
            source: "network path not found".to_string(),
        }
    }

    #[test]
    fn next_sound_retry_first_failure_waits_30_seconds() {
        assert_eq!(
            next_sound_retry(&unreadable(), None),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn next_sound_retry_doubles_after_each_failure() {
        assert_eq!(
            next_sound_retry(&unreadable(), Some(Duration::from_secs(30))),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            next_sound_retry(&unreadable(), Some(Duration::from_secs(120))),
            Some(Duration::from_secs(240))
        );
    }

    #[test]
    fn next_sound_retry_stops_growing_at_5_minutes() {
        // 240 秒の倍は 480 秒だが、上限の 5 分で止まる
        assert_eq!(
            next_sound_retry(&unreadable(), Some(Duration::from_secs(240))),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            next_sound_retry(&unreadable(), Some(Duration::from_secs(300))),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn next_sound_retry_first_interval_is_longer_than_the_apply_period() {
        // 2 秒ごとの apply_settings で読み込みスレッドを詰めて起こさないこと
        let first = next_sound_retry(&unreadable(), None).expect("読み直すこと");
        assert!(first > Duration::from_secs(2));
    }

    #[test]
    fn next_sound_retry_undecodable_file_is_not_retried_automatically() {
        let error = ScreenshotError::SoundFileUndecodable {
            path: PathBuf::from("C:/sounds/SS.mp3"),
            source: "unrecognized format".to_string(),
        };
        assert_eq!(next_sound_retry(&error, None), None);
    }

    #[test]
    fn is_sound_retry_due_before_the_time_returns_false() {
        let now = Instant::now();
        assert!(!is_sound_retry_due(Some(now + Duration::from_secs(1)), now));
    }

    #[test]
    fn is_sound_retry_due_at_or_after_the_time_returns_true() {
        let now = Instant::now();
        assert!(is_sound_retry_due(Some(now), now));
        assert!(is_sound_retry_due(Some(now), now + Duration::from_secs(1)));
    }

    #[test]
    fn is_sound_retry_due_while_waiting_for_the_result_returns_false() {
        // 読み直しを始めて結果を待っている間は、2 秒ごとの適用で重ねて起こさない
        assert!(!is_sound_retry_due(None, Instant::now()));
    }
}
