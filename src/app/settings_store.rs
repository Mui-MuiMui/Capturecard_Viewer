//! 実行中の設定をディスクへ書き出す経路。
//!
//! ウィンドウの移動・リサイズ、音量スクロール、コンテキストメニューの操作は
//! その場で書かず `mark_settings_dirty` で保留し、操作が落ち着いたところで
//! `flush_settings_if_due` がまとめて書き出す。**設定を書き換える処理を
//! 足すときは `AppSettings::save()` を直接呼ばないこと。**
//!
//! 保存に失敗し続けている間は、再試行の間隔を伸ばし、同じ理由のログと
//! トーストを間引く（`SaveFailureStreak`、Issue #317）。

use super::CaptureCardViewer;
use crate::settings::{LoadOutcome, SettingsError};
use crate::status::ErrorSource;
use eframe::egui;
use log::{debug, error, info, trace, warn};
use std::time::{Duration, Instant};

/// 設定をディスクへ書き出すまでに待つ時間。
/// ウィンドウのドラッグ中や音量スクロール中は設定が毎フレーム変わるため、
/// 最後の変更からこの時間が空くまで書き出しをまとめる
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// 保存の失敗が続いたときの再試行の間隔の上限。
///
/// ディスク満杯・権限なし・他のプロセスのロックは、ユーザーが手を打つまで
/// 直らないことが多い。2 秒ごとに試し続けても書けないうえ、そのたびに
/// ディスクへ触る。1 分に 1 回まで落とせば、直ったあとも 1 分以内に書ける
const SETTINGS_SAVE_MAX_RETRY: Duration = Duration::from_secs(60);

/// 保存の失敗が何回続いているかと、直前の理由。
///
/// フィールドは `CaptureCardViewer::settings_save_failures`（`app/mod.rs`）。
/// 判定だけをここへ切り出して、時計やファイルに触らずテストできるようにしてある。
#[derive(Debug, Default)]
pub(super) struct SaveFailureStreak {
    /// 続けて失敗した回数。0 は「失敗していない」
    count: u32,
    /// 直前の失敗の理由（`SettingsError` の `Display`）
    last_reason: Option<String>,
}

/// 保存の失敗をどう知らせるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureNotice {
    /// 初めての失敗か、理由が変わった。`error!` を出してトーストで知らせる
    Report,
    /// 同じ理由の失敗が続いている。ログを間引き、トーストも出さない
    Quiet,
}

/// 何が保存を始めたか。失敗をどこまで知らせるかが変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveTrigger {
    /// 設定ダイアログの「適用」「OK」。失敗は毎回知らせる
    Explicit,
    /// デバウンス保存・終了時の保存・起動時の書き戻し。同じ理由は間引く
    Automatic,
}

impl FailureNotice {
    /// 保存を始めたものに合わせて、知らせ方を決め直す。
    ///
    /// ユーザーが押した保存は、同じ理由の失敗が続いていても知らせる。
    /// 押した結果が何も出ないと、保存できたのか分からないため。
    /// 失敗の回数（再試行の間隔）はどちらでも同じように数える。
    fn for_trigger(self, trigger: SaveTrigger) -> Self {
        match trigger {
            SaveTrigger::Explicit => FailureNotice::Report,
            SaveTrigger::Automatic => self,
        }
    }
}

impl SaveFailureStreak {
    /// 失敗を数え、知らせるべきかを返す。
    fn record_failure(&mut self, reason: &str) -> FailureNotice {
        self.count = self.count.saturating_add(1);
        let same_reason = self.last_reason.as_deref() == Some(reason);
        if !same_reason {
            self.last_reason = Some(reason.to_string());
        }
        if same_reason {
            FailureNotice::Quiet
        } else {
            FailureNotice::Report
        }
    }

    /// 成功を記録する。直前まで失敗していたなら、その回数を返す。
    fn record_success(&mut self) -> Option<u32> {
        let failed = self.count;
        *self = Self::default();
        (failed > 0).then_some(failed)
    }

    /// 次に書き出しを試すまで、最後の変更（か失敗）から待つ時間。
    fn retry_delay(&self) -> Duration {
        save_retry_delay(self.count)
    }
}

/// 続けて失敗した回数から、次に書き出しを試すまでの待ち時間を決める。
///
/// 失敗していなければデバウンスの 2 秒。失敗するたびに倍にして
/// 2 → 4 → 8 → 16 → 32 → 60 秒で頭打ちにする。
fn save_retry_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return SETTINGS_SAVE_DEBOUNCE;
    }
    // 1 回目の失敗の後も 2 秒。シフトが桁あふれしないよう指数を抑える
    let exponent = (consecutive_failures - 1).min(16);
    SETTINGS_SAVE_DEBOUNCE
        .saturating_mul(1u32 << exponent)
        .min(SETTINGS_SAVE_MAX_RETRY)
}

/// 起動時に設定ファイルを読めなかったことを知らせる文言。読めたなら None。
///
/// その時点の画面の言語で作るので、言語を決めたあとに呼ぶ（`report_startup_settings`）。
fn startup_load_message(outcome: &LoadOutcome) -> Option<String> {
    outcome.failure().map(|failure| failure.to_string())
}

impl CaptureCardViewer {
    /// 起動時の設定の結果を取り込み、失敗をトーストと「接続状態」タブで知らせる
    /// （Issue #505）。`new()` で預けた書き戻しの結果（`pending_startup_save`）を
    /// 取り込み、そのあとで設定ファイルを読めなかったことを知らせる。読めて
    /// 書き戻せたなら何も出さない。
    ///
    /// 呼ぶのは `main.rs` が作って画面の言語を決めた直後の 1 回だけ。**文言はここで
    /// 初めて作る。** `new()` の中で作ると、英語の設定でも既定の日本語で出る。
    ///
    /// 書き戻しの失敗と読み込みの知らせは同じ発生源（`ErrorSource::Settings`）で、
    /// `ErrorCenter` は直近の 1 件しか持たないので、後から出す読み込みの知らせが残る。
    /// 読み込みの知らせは `settings_load_notice` にも残し、あとの保存の成功
    /// （`errors.clear`）や次の失敗では消さない。トーストは後勝ちなので、直後に接続の失敗が届くと上書きされる
    /// （`docs/design/error-reporting.md`）。優先度は付けず、「接続状態」タブの
    /// 「設定ファイル」の枠に残ることで読み返せるようにしてある。退避できずに
    /// 自動保存を止めた知らせは、「適用」「OK」で保存できたときに取り下げる
    /// （`app::settings_dialog`）
    pub fn report_startup_settings(&mut self, outcome: &LoadOutcome) {
        if let Some(result) = self.pending_startup_save.take() {
            self.note_settings_save_result(result, SaveTrigger::Automatic);
        }
        if let Some(message) = startup_load_message(outcome) {
            self.report_error(ErrorSource::Settings, message);
            // 保存の成功（`errors.clear`）や次の失敗で消えないよう、別に残す
            self.settings_load_notice = self.errors.latest(ErrorSource::Settings).cloned();
        }
    }

    /// 設定に未保存の変更があることを記録する。
    /// 実際の書き出しは `flush_settings_if_due` がまとめて行う。
    pub(super) fn mark_settings_dirty(&mut self) {
        // 自動保存を止めている間は保留として積まない。積むと
        // flush_settings_if_due が書き出す時刻へ再描画を予約し続け、
        // 書き出さないまま 2 秒ごとに起こされることになる
        if !self.autosave.is_allowed() {
            trace!("自動保存を止めているので設定の変更を保留しない");
            return;
        }
        self.settings_dirty_since = Some(Instant::now());
    }

    /// ユーザーの操作（設定ダイアログの「適用」「OK」）で、いま設定を
    /// ディスクへ書き出す。書き出せたかを返す。
    ///
    /// 失敗は同じ理由が続いていても毎回ログとトーストで知らせる。
    /// 押した結果が何も出ないと、保存できたのか分からないため。
    pub(super) fn save_settings_now(&mut self) -> bool {
        self.save_settings(SaveTrigger::Explicit)
    }

    /// 自動保存（デバウンス保存と終了時の保存）で、いま設定をディスクへ
    /// 書き出す。書き出せたかを返す。同じ理由の失敗は初回だけ知らせる。
    pub(super) fn save_settings_automatically(&mut self) -> bool {
        self.save_settings(SaveTrigger::Automatic)
    }

    fn save_settings(&mut self, trigger: SaveTrigger) -> bool {
        // ロックが取れなかった場合は保留のままにして、次の機会に書き出す。
        // 複製してからロックを放すのは、ファイル I/O の間に UI やワーカーの
        // 読み出しを待たせないため
        let snapshot = match self.settings.lock() {
            Ok(settings) => settings.clone(),
            Err(_) => {
                warn!("設定の保存で settings のロックを取得できない");
                return false;
            }
        };
        let result = snapshot.save();
        self.note_settings_save_result(result, trigger)
    }

    /// 保存の結果を取り込む。書き出せたかを返す。
    ///
    /// 起動時の書き戻し（`app/mod.rs` の `CaptureCardViewer::new`）も
    /// `SaveTrigger::Automatic` でここを通す。そちらは settings のロックを
    /// 握ったまま保存するため、`save_settings_automatically` を呼べない。
    pub(super) fn note_settings_save_result(
        &mut self,
        result: Result<(), SettingsError>,
        trigger: SaveTrigger,
    ) -> bool {
        match result {
            Ok(()) => {
                self.settings_dirty_since = None;
                if let Some(failed) = self.settings_save_failures.record_success() {
                    info!("設定を保存できた（{} 回続けて失敗したあと）", failed);
                    // 保存の失敗として出したトーストと「接続状態」タブの記録を
                    // 取り下げる。書き出し・読み込みの失敗も同じ発生源だが、
                    // どれも直近の 1 件しか持たないため区別しない
                    self.errors.clear(ErrorSource::Settings);
                }
                true
            }
            Err(e) => {
                let reason = e.to_string();
                let notice = self.settings_save_failures.record_failure(&reason);
                match notice.for_trigger(trigger) {
                    FailureNotice::Report => {
                        error!("設定の保存に失敗した: {}", reason);
                        if trigger == SaveTrigger::Explicit {
                            // ErrorCenter は同じ文言を 60 秒間引くので、記録を
                            // 消してから記録し直し、押すたびにトーストを出す
                            self.errors.clear(ErrorSource::Settings);
                        }
                        self.report_error(ErrorSource::Settings, reason);
                    }
                    FailureNotice::Quiet => debug!(
                        "設定の保存に続けて失敗した（{} 回目、次は {} 秒後）: {}",
                        self.settings_save_failures.count,
                        self.settings_save_failures.retry_delay().as_secs(),
                        reason
                    ),
                }
                // 書き出せなかった変更を保存済みとして捨てず、保留のまま残す。
                // 時刻を入れ直しているのは、次の再試行までの待ち時間
                // （retry_delay）をここから数えるため
                self.settings_dirty_since = Some(Instant::now());
                false
            }
        }
    }

    /// 保留中の設定変更を書き出すべきかを判定する。
    /// `since_last_change` は最後の変更（か失敗）からの経過時間で、
    /// `None` は「保留中の変更が無い」を表す。`delay` は待つ時間。
    fn should_flush_settings(since_last_change: Option<Duration>, delay: Duration) -> bool {
        match since_last_change {
            None => false,
            Some(elapsed) => elapsed >= delay,
        }
    }

    /// 保留中の設定変更を、最後の変更から一定時間が空いていれば書き出す。
    ///
    /// 書き出しはディスク I/O だが、デバウンスにより数秒に 1 回までしか走らない
    /// ため UI スレッドで行っている。ウィンドウのドラッグや音量の連続操作のように
    /// 毎フレーム値が変わる間は、変更が止まるまで 1 度も書き出さない。
    /// 保存に失敗し続けている間は、待つ時間を最大 1 分まで伸ばす。
    pub(super) fn flush_settings_if_due(&mut self, ctx: &egui::Context) {
        // 読めなかった設定ファイルが残っている間は書き出さない。
        // mark_settings_dirty 側でも積まないようにしてあるが、
        // 保留を直接立てる経路が増えても止まるようにここでも見る
        if !self.autosave.is_allowed() {
            return;
        }

        let delay = self.settings_save_failures.retry_delay();
        let elapsed = self.settings_dirty_since.map(|since| since.elapsed());
        if !Self::should_flush_settings(elapsed, delay) {
            // 書き出す時刻に再描画を予約する。映像が来ていないときは再描画が
            // 止まりうるため、これが無いと update() が呼ばれず書き出しが遅れる
            if let Some(elapsed) = elapsed {
                ctx.request_repaint_after(delay.saturating_sub(elapsed));
            }
            return;
        }

        self.save_settings_automatically();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{with_language, Language};
    use crate::settings::LoadFailure;
    use std::path::PathBuf;

    #[test]
    fn should_flush_settings_no_pending_change_returns_false() {
        // 保留中の変更が無ければ書き出さない
        assert!(!CaptureCardViewer::should_flush_settings(
            None,
            SETTINGS_SAVE_DEBOUNCE
        ));
    }

    #[test]
    fn should_flush_settings_just_changed_returns_false() {
        // 変更した直後は書き出さない（ドラッグ中の毎フレーム書き込みを防ぐ肝）
        assert!(!CaptureCardViewer::should_flush_settings(
            Some(Duration::from_secs(0)),
            SETTINGS_SAVE_DEBOUNCE
        ));
    }

    #[test]
    fn should_flush_settings_just_before_interval_returns_false() {
        // 境界の手前。1999ms では書き出さない
        assert!(!CaptureCardViewer::should_flush_settings(
            Some(Duration::from_millis(1999)),
            SETTINGS_SAVE_DEBOUNCE
        ));
    }

    #[test]
    fn should_flush_settings_at_interval_returns_true() {
        // 境界。ちょうど 2000ms で書き出す
        assert!(CaptureCardViewer::should_flush_settings(
            Some(Duration::from_millis(2000)),
            SETTINGS_SAVE_DEBOUNCE
        ));
    }

    #[test]
    fn should_flush_settings_long_after_interval_returns_true() {
        assert!(CaptureCardViewer::should_flush_settings(
            Some(Duration::from_secs(3600)),
            SETTINGS_SAVE_DEBOUNCE
        ));
    }

    #[test]
    fn should_flush_settings_backed_off_waits_for_the_longer_delay() {
        // 失敗が続いて待ち時間が伸びている間は、2 秒経っても書き出さない
        let delay = save_retry_delay(3);
        assert!(!CaptureCardViewer::should_flush_settings(
            Some(Duration::from_secs(2)),
            delay
        ));
        assert!(CaptureCardViewer::should_flush_settings(Some(delay), delay));
    }

    #[test]
    fn save_retry_delay_without_failures_is_the_debounce() {
        assert_eq!(save_retry_delay(0), SETTINGS_SAVE_DEBOUNCE);
    }

    #[test]
    fn save_retry_delay_doubles_after_each_failure() {
        // 1 回目の失敗の後はデバウンスと同じ 2 秒、以降は倍々
        assert_eq!(save_retry_delay(1), Duration::from_secs(2));
        assert_eq!(save_retry_delay(2), Duration::from_secs(4));
        assert_eq!(save_retry_delay(3), Duration::from_secs(8));
        assert_eq!(save_retry_delay(4), Duration::from_secs(16));
        assert_eq!(save_retry_delay(5), Duration::from_secs(32));
    }

    #[test]
    fn save_retry_delay_caps_at_one_minute() {
        // 境界。64 秒になるところで 60 秒に抑える
        assert_eq!(save_retry_delay(6), SETTINGS_SAVE_MAX_RETRY);
        assert_eq!(save_retry_delay(7), SETTINGS_SAVE_MAX_RETRY);
    }

    #[test]
    fn save_retry_delay_huge_count_does_not_overflow() {
        // 何日も失敗し続けても桁あふれで短い間隔に戻らない
        assert_eq!(save_retry_delay(u32::MAX), SETTINGS_SAVE_MAX_RETRY);
    }

    #[test]
    fn save_failure_streak_first_failure_is_reported() {
        let mut streak = SaveFailureStreak::default();

        assert_eq!(
            streak.record_failure("ディスクが一杯"),
            FailureNotice::Report
        );
    }

    #[test]
    fn save_failure_streak_same_reason_is_quiet() {
        // 同じ理由が続く間はログを間引き、トーストも出さない
        let mut streak = SaveFailureStreak::default();
        streak.record_failure("ディスクが一杯");

        assert_eq!(
            streak.record_failure("ディスクが一杯"),
            FailureNotice::Quiet
        );
        assert_eq!(
            streak.record_failure("ディスクが一杯"),
            FailureNotice::Quiet
        );
        assert_eq!(streak.count, 3);
    }

    #[test]
    fn save_failure_streak_changed_reason_is_reported_again() {
        // 理由が変わったら知らせ直す。違う手の打ち方が要るため
        let mut streak = SaveFailureStreak::default();
        streak.record_failure("ディスクが一杯");

        assert_eq!(
            streak.record_failure("アクセスが拒否された"),
            FailureNotice::Report
        );
        // 失敗の回数は理由が変わっても続けて数える（待ち時間を戻さない）
        assert_eq!(streak.count, 2);
    }

    #[test]
    fn save_failure_streak_success_resets_and_returns_the_count() {
        let mut streak = SaveFailureStreak::default();
        streak.record_failure("ディスクが一杯");
        streak.record_failure("ディスクが一杯");

        assert_eq!(streak.record_success(), Some(2));
        assert_eq!(streak.retry_delay(), SETTINGS_SAVE_DEBOUNCE);
        // 直ったあとに同じ理由で失敗したら、また知らせる
        assert_eq!(
            streak.record_failure("ディスクが一杯"),
            FailureNotice::Report
        );
    }

    #[test]
    fn failure_notice_explicit_save_is_always_reported() {
        // 「適用」「OK」で保存した場合は、同じ理由が続いていても知らせる
        assert_eq!(
            FailureNotice::Quiet.for_trigger(SaveTrigger::Explicit),
            FailureNotice::Report
        );
        assert_eq!(
            FailureNotice::Report.for_trigger(SaveTrigger::Explicit),
            FailureNotice::Report
        );
    }

    #[test]
    fn failure_notice_automatic_save_keeps_the_streak_decision() {
        assert_eq!(
            FailureNotice::Quiet.for_trigger(SaveTrigger::Automatic),
            FailureNotice::Quiet
        );
        assert_eq!(
            FailureNotice::Report.for_trigger(SaveTrigger::Automatic),
            FailureNotice::Report
        );
    }

    #[test]
    fn save_failure_streak_success_without_failures_returns_none() {
        // 普段の成功では何もしない（errors.clear も呼ばない）
        let mut streak = SaveFailureStreak::default();

        assert_eq!(streak.record_success(), None);
    }

    // ---- 起動時の知らせの言語（#505） ----
    //
    // 言語は `with_language` でこのスレッドだけ切り替える。`set_language` は
    // プロセス全体を書き換え、同じプロセスの他のテストを巻き込む（#256）

    #[test]
    fn startup_load_message_uses_the_language_at_report_time() {
        // 読み込みの結果は文言を持たず、知らせる時点の言語で作る。英語の設定なら英語
        let outcome = LoadOutcome::FellBackToDefaults {
            reason: LoadFailure::Blank,
            backup: Some(PathBuf::from(r"C:\config\default-config.toml.bak")),
        };

        let english = with_language(Language::English, || startup_load_message(&outcome))
            .expect("知らせること");
        let japanese = with_language(Language::Japanese, || startup_load_message(&outcome))
            .expect("知らせること");

        assert!(english.contains("Reset to defaults"), "{english}");
        assert!(english.contains("the file is empty"), "{english}");
        assert!(!japanese.contains("Reset to defaults"), "{japanese}");
        assert!(japanese.contains("既定値"), "{japanese}");
    }

    #[test]
    fn startup_load_message_gist_fits_in_the_toast() {
        // トーストは定型文と合わせて 60 文字で切れる。退避先のパスが長くても、
        // 初期化した / 自動保存を止めた、は切れずに読めること
        let backup = PathBuf::from(
            r"C:\Users\someone\AppData\Roaming\capturecard_viewer\config\default-config.toml.bak",
        );
        let fell_back = LoadOutcome::FellBackToDefaults {
            reason: LoadFailure::Unreadable("TOML parse error at line 1, column 5".into()),
            backup: Some(backup.clone()),
        };
        let kept = LoadOutcome::BrokenFileLeftBehind {
            reason: LoadFailure::Unreadable("TOML parse error at line 1, column 5".into()),
            path: Some(backup),
        };
        let toast = |outcome: &LoadOutcome| {
            let message = startup_load_message(outcome).expect("知らせること");
            crate::status::truncate(
                &crate::status::format_message(ErrorSource::Settings, &message),
                crate::status::TOAST_MESSAGE_LIMIT,
            )
        };

        let cases = [
            (Language::Japanese, &fell_back, "既定値に初期化した"),
            (Language::Japanese, &kept, "自動保存を止めた"),
            (Language::English, &fell_back, "Reset to defaults"),
            (Language::English, &kept, "Auto-save stopped"),
        ];
        for (language, outcome, gist) in cases {
            let text = with_language(language, || toast(outcome));
            assert!(text.contains(gist), "{text}");
        }
    }

    #[test]
    fn startup_load_message_is_none_when_loaded() {
        assert_eq!(startup_load_message(&LoadOutcome::Loaded), None);
    }

    #[test]
    fn pending_startup_save_error_uses_the_language_at_report_time() {
        // `new()` が預ける書き戻しの失敗も型のまま持ち、取り込む時点の言語で文言にする
        let error = SettingsError::SaveFailed {
            path: PathBuf::from(r"C:\config\default-config.toml"),
            source: "access denied".to_string(),
        };

        let english = with_language(Language::English, || error.to_string());
        let japanese = with_language(Language::Japanese, || error.to_string());

        assert!(english.starts_with("Cannot write to"), "{english}");
        assert!(japanese.contains("書き出せない"), "{japanese}");
    }
}
