//! 更新の確認を別スレッドで行い、結果を取り込む。起動時の通知ダイアログの操作もここ。
//!
//! 問い合わせそのものは `crate::update::check_latest_release`。ネットワークを
//! 待つので UI スレッドでは呼ばず、確認ごとにスレッドを起こして結果をチャネルで
//! `update()` へ返す。効果音の読み込み（`app::screenshot_sound`）と同じ流儀
//! （`docs/design/threads.md`、`docs/design/update.md`）。
//!
//! **失敗しても起動は止めない。** ログと「その他」タブの表示に出すだけ。トーストは
//! 「更新を確認」を押したときだけ出す（`CheckOrigin::notifies_failure`）。
//!
//! **終了時に確認のスレッドを待たない。** ネットワークだけを触り、ファイルも設定も
//! 書かないので、途中で打ち切られてもプロセスの終了で消えるだけで何も壊れない。

use super::screenshot::drop_finished_threads;
use super::CaptureCardViewer;
use crate::status::ErrorSource;
use crate::ui::{self, UpdateDialogEvent};
use crate::update::{
    self, CheckOutcome, CheckOverrides, UpdateCheck, UpdateError, UpdateStatus, UpdateView,
};
use eframe::egui;
use log::{debug, info, warn};
use semver::Version;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

/// 誰が確認を始めたか。見つかったときにダイアログを出すかが変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckOrigin {
    /// 起動時の自動の確認。設定によってはダイアログで知らせる
    Startup,
    /// 「その他」タブの「更新を確認」。結果は欄に出すだけで、ダイアログは出さない
    Manual,
}

impl CheckOrigin {
    /// 失敗をトースト（`report_error`）でも知らせるか。
    ///
    /// **起動時の自動の確認は知らせない。** ネットワークの無い環境で起動のたびに
    /// トーストが出るのは邪魔なだけで、理由は WARN のログと「更新」の欄に残る。
    /// 「更新を確認」は人が押した操作なので、結果が失敗でも知らせる。
    fn notifies_failure(self) -> bool {
        match self {
            CheckOrigin::Startup => false,
            CheckOrigin::Manual => true,
        }
    }
}

/// 確認のスレッドから UI スレッドへ返す結果。
pub(super) struct UpdateCheckResult {
    origin: CheckOrigin,
    result: Result<CheckOutcome, UpdateError>,
}

/// 更新の確認にまつわる状態。`CaptureCardViewer::update_check` に 1 つだけ置く。
///
/// 設定ダイアログの `SettingsDialogState` に入れないのは、起動時の確認と
/// 通知ダイアログが設定ダイアログを開いていなくても動くため。
pub(super) struct UpdateState {
    tx: Sender<UpdateCheckResult>,
    rx: Receiver<UpdateCheckResult>,
    // 確認のスレッド。ハンドルは持っておくが、**終了時に join しない。**
    // ネットワークだけを触る副作用の無いスレッドなので、確認の最中に閉じても
    // 待たずに終わってよい（効果音・スクリーンショットの保存スレッドとは違う。
    // docs/design/update.md）
    threads: Vec<JoinHandle<()>>,
    // 「その他」タブに出す確認の状態
    status: UpdateStatus,
    // 起動時に知らせる新しい版。`Some` の間は通知ダイアログを出す
    dialog: Option<UpdateCheck>,
    // テスト用の環境変数で差し替えた版と問い合わせ先。起動時に 1 回だけ読む
    overrides: CheckOverrides,
    // 比較に使う「いまの版」。差し替えていなければ実行中の版
    current: Version,
}

impl UpdateState {
    /// 起動時に 1 回だけ作る。テスト用の環境変数もここで読む
    /// （指定されていれば WARN で残る）。
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let overrides = CheckOverrides::from_env();
        let current = overrides.current_version_or(update::current_version());
        Self {
            tx,
            rx,
            threads: Vec::new(),
            status: UpdateStatus::default(),
            dialog: None,
            overrides,
            current,
        }
    }

    /// 「その他」タブの「更新」の欄へ渡すもの。
    pub(super) fn view(&self) -> UpdateView<'_> {
        UpdateView {
            current: &self.current,
            status: &self.status,
        }
    }
}

impl CaptureCardViewer {
    /// 起動時の確認。設定で切ってあれば何もしない。起動直後の 1 回だけ呼ぶ。
    pub(super) fn check_for_updates_on_startup(&mut self) {
        let enabled = match self.settings.lock() {
            Ok(settings) => settings.update.check_on_startup,
            Err(_) => {
                warn!("起動時の更新の確認で settings のロックを取得できない");
                return;
            }
        };
        if enabled {
            self.start_update_check(CheckOrigin::Startup);
        } else {
            debug!("起動時の更新の確認は設定で切ってある");
        }
    }

    /// 更新の確認を別スレッドで始める。確認中なら何もしない。
    pub(super) fn start_update_check(&mut self, origin: CheckOrigin) {
        if matches!(self.update_check.status, UpdateStatus::Checking) {
            debug!("更新の確認中なので、重ねて始めない");
            return;
        }

        let tx = self.update_check.tx.clone();
        let overrides = self.update_check.overrides.clone();
        // 映像が止まっている間は update() の間隔が広がっているので、届いたら起こす
        let waker = self.repaint_waker.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".to_string())
            .spawn(move || {
                let result = update::check_latest_release(&overrides);
                // ログは受け取った UI スレッド側で出す（効果音の読み込みと同じ）
                if tx.send(UpdateCheckResult { origin, result }).is_err() {
                    // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                    debug!("更新の確認結果の送り先が既に無いので捨てる");
                    return;
                }
                waker.wake();
            });

        match spawned {
            Ok(handle) => {
                info!("更新の確認を始める（{:?}）", origin);
                self.update_check.status = UpdateStatus::Checking;
                drop_finished_threads(&mut self.update_check.threads);
                self.update_check.threads.push(handle);
            }
            Err(e) => {
                warn!("更新の確認のスレッドを起こせない: {}", e);
                self.record_update_failure(origin, UpdateError::Network(e.to_string()));
            }
        }
    }

    /// 別スレッドから届いた確認の結果を取り込む。`update()` の先頭で呼ぶ。
    pub(super) fn drain_update_results(&mut self) {
        while let Ok(result) = self.update_check.rx.try_recv() {
            self.apply_update_result(result);
        }
    }

    fn apply_update_result(&mut self, message: UpdateCheckResult) {
        let UpdateCheckResult { origin, result } = message;
        match result {
            Ok(CheckOutcome::UpToDate) => {
                info!(
                    "更新の確認: 新しい版は無い（いまは v{}）",
                    self.update_check.current
                );
                self.errors.clear(ErrorSource::Update);
                self.update_check.status = UpdateStatus::UpToDate;
            }
            Ok(CheckOutcome::Available(check)) => {
                let assets: Vec<&str> = check.assets.iter().map(|a| a.name.as_str()).collect();
                info!(
                    "更新の確認: 新しい版 v{} がある（いまは v{}）。資産: [{}]",
                    check.latest,
                    check.current,
                    assets.join(", ")
                );
                self.errors.clear(ErrorSource::Update);
                if origin == CheckOrigin::Startup && self.should_notify_update(&check) {
                    self.update_check.dialog = Some(check.clone());
                }
                self.update_check.status = UpdateStatus::Available(check);
            }
            Err(e) => {
                warn!("更新を確認できない（{:?}）: {}", origin, e);
                self.record_update_failure(origin, e);
            }
        }
    }

    /// 確認の失敗を「更新」の欄へ出す。「更新を確認」からの失敗だけトーストにも出す。
    fn record_update_failure(&mut self, origin: CheckOrigin, error: UpdateError) {
        let reason = error.to_string();
        self.update_check.status = UpdateStatus::Failed(reason.clone());
        if origin.notifies_failure() {
            self.report_error(ErrorSource::Update, reason);
        }
    }

    /// 起動時の確認で見つけた版を、ダイアログで知らせるか。
    fn should_notify_update(&self, check: &UpdateCheck) -> bool {
        match self.settings.lock() {
            Ok(settings) => update::should_notify_on_startup(&settings.update, &check.latest),
            Err(_) => {
                warn!("更新の通知の判定で settings のロックを取得できない");
                false
            }
        }
    }

    /// 起動時の通知ダイアログを描き、押されたものを処理する。出すものが無ければ何もしない。
    pub(super) fn draw_update_dialog(&mut self, ctx: &egui::Context) {
        let Some(check) = &self.update_check.dialog else {
            return;
        };
        let events = ui::show_update_dialog(ctx, check);
        for event in events {
            self.handle_update_dialog_event(ctx, event);
        }
    }

    fn handle_update_dialog_event(&mut self, ctx: &egui::Context, event: UpdateDialogEvent) {
        // どのボタンでもダイアログは閉じる
        let Some(check) = self.update_check.dialog.take() else {
            return;
        };
        match event {
            UpdateDialogEvent::OpenReleasePage => {
                // この段階の「更新する」はリリースページを開くところまで。
                // ブラウザの起動は eframe に任せる
                info!("リリースページを開く: {}", check.release_url);
                ctx.open_url(egui::OpenUrl::new_tab(&check.release_url));
            }
            UpdateDialogEvent::Later => {
                debug!("更新の通知を閉じた（次の起動でまた知らせる）");
            }
            UpdateDialogEvent::SkipThisVersion => {
                info!("v{} は通知しない", check.latest);
                if let Ok(mut settings) = self.settings.lock() {
                    settings.update.skipped_version = Some(check.latest.to_string());
                } else {
                    warn!("「この版は通知しない」で settings のロックを取得できない");
                    return;
                }
                self.mark_settings_dirty();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_origin_startup_failure_is_not_toasted() {
        // 起動時の自動の確認はトーストを出さない。ログと「更新」の欄だけ
        assert!(!CheckOrigin::Startup.notifies_failure());
    }

    #[test]
    fn check_origin_manual_failure_is_toasted() {
        // 「更新を確認」は人が押した操作なので、失敗も知らせる
        assert!(CheckOrigin::Manual.notifies_failure());
    }
}
