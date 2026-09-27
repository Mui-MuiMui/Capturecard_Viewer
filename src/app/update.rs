//! 更新の確認と適用を別スレッドで行い、結果を取り込む。通知ダイアログの操作もここ。
//!
//! 問い合わせそのものは `crate::update::check_latest_release`、ダウンロード・照合・
//! 差し替えは `crate::update::apply::run_apply`。どちらもネットワークを待つので
//! UI スレッドでは呼ばず、スレッドを起こして結果をチャネルで `update()` へ返す。
//! 効果音の読み込み（`app::screenshot_sound`）と同じ流儀
//! （`docs/design/threads.md`、`docs/design/update.md`）。
//!
//! **確認に失敗しても起動は止めない。** ログと「その他」タブの表示に出すだけ。
//! トーストは「更新を確認」を押したときだけ出す（`CheckOrigin::notifies_failure`）。
//! 更新（適用）の失敗は人が押した操作の結果なので、必ずトーストにも出す。
//!
//! **終了時にどのスレッドも待たない。** 確認はネットワークだけを触る。適用は
//! 書きかけの `.new` を残しうるが、元の exe には照合が済むまで触らず、残った
//! `.new` は次の起動で消す（`clean_up_update_leftovers`）。差し替えが済んでいれば、
//! 新しい exe の起動は `on_exit` の最後に行う（`relaunch_updated_exe`）。

use super::screenshot::drop_finished_threads;
use super::CaptureCardViewer;
use crate::status::ErrorSource;
use crate::ui::{self, UpdateDialogEvent, UpdateDialogView};
use crate::update::apply::{self, ApplyError, ApplyProgress, ExePaths};
use crate::update::{
    self, CheckOutcome, CheckOverrides, UpdateCheck, UpdateError, UpdateStatus, UpdateView,
};
use eframe::egui;
use log::{debug, error, info, warn};
use semver::Version;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// 起動時に前の版の `.old` を消すときの再試行の間隔と回数。
///
/// 更新で起動した直後は、前の版のプロセスがまだ終わりきっておらず `.old` を
/// 消せない（実行中の exe は消せない）。数秒待てば消せるので、しばらく繰り返す。
const LEFTOVER_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const LEFTOVER_RETRIES: u32 = 20;

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

/// 適用のスレッドから UI スレッドへ返すもの。
enum ApplyMessage {
    /// 進み具合。ダイアログに出す
    Progress(ApplyProgress),
    /// 終わった。成功なら差し替えが済んでいる
    Finished(Result<(), ApplyError>),
}

/// 走っている更新（適用）1 つ。
struct ApplyJob {
    rx: Receiver<ApplyMessage>,
    // 立てると、スレッドが読み取りの合間か差し替えの直前で止まり `.new` を消す
    cancel: Arc<AtomicBool>,
    // 更新している版。ダイアログの見出しと失敗の表示に使う
    check: UpdateCheck,
    paths: ExePaths,
    progress: ApplyProgress,
}

/// 通知ダイアログに何を出しているか。
enum UpdateDialogState {
    /// 新しい版を知らせている
    Available(UpdateCheck),
    /// 更新している。版と進み具合は `UpdateState::apply` が持つ
    Applying,
    /// 差し替えが済み、終了して新しい版を起動するところ
    Restarting,
    /// 更新できなかった
    Failed { check: UpdateCheck, reason: String },
}

/// 更新の確認と適用にまつわる状態。`CaptureCardViewer::update_check` に 1 つだけ置く。
///
/// 設定ダイアログの `SettingsDialogState` に入れないのは、起動時の確認と
/// 通知ダイアログが設定ダイアログを開いていなくても動くため。
pub(super) struct UpdateState {
    tx: Sender<UpdateCheckResult>,
    rx: Receiver<UpdateCheckResult>,
    // 確認・適用・後片付けのスレッド。ハンドルは持っておくが、**終了時に join しない。**
    // 確認の最中に閉じても待たずに終わってよい（効果音・スクリーンショットの
    // 保存スレッドとは違う。docs/design/update.md）
    threads: Vec<JoinHandle<()>>,
    // 「その他」タブに出す確認の状態
    status: UpdateStatus,
    // 通知ダイアログに出しているもの。`None` なら出さない
    dialog: Option<UpdateDialogState>,
    // 走っている更新。同時に 1 つまで
    apply: Option<ApplyJob>,
    // 差し替えが済んだ exe。`on_exit` の最後にこれを起動する
    restart: Option<ExePaths>,
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
            apply: None,
            restart: None,
            overrides,
            current,
        }
    }

    /// 「その他」タブの「更新」の欄へ渡すもの。
    pub(super) fn view(&self) -> UpdateView<'_> {
        UpdateView {
            current: &self.current,
            status: &self.status,
            applying: self.apply.is_some() || self.restart.is_some(),
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
                // 起動時の通知ダイアログが開いたまま「更新を確認」を押した場合は閉じる。
                // 結果は「更新」の欄に出るので、古い確認の内容を操作させない。
                // 更新の最中と失敗の表示は閉じない
                if origin == CheckOrigin::Manual
                    && matches!(
                        self.update_check.dialog,
                        Some(UpdateDialogState::Available(_))
                    )
                {
                    self.update_check.dialog = None;
                }
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
                if origin == CheckOrigin::Startup
                    && self.update_check.dialog.is_none()
                    && self.should_notify_update(&check)
                {
                    self.update_check.dialog = Some(UpdateDialogState::Available(check.clone()));
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

    /// 通知ダイアログを描き、押されたものを処理する。出すものが無ければ何もしない。
    pub(super) fn draw_update_dialog(&mut self, ctx: &egui::Context) {
        let state = &self.update_check;
        let view = match &state.dialog {
            None => return,
            Some(UpdateDialogState::Available(check)) => UpdateDialogView::Available(check),
            Some(UpdateDialogState::Applying) => match &state.apply {
                Some(job) => UpdateDialogView::Applying {
                    check: &job.check,
                    progress: job.progress,
                },
                None => return,
            },
            Some(UpdateDialogState::Restarting) => UpdateDialogView::Restarting,
            Some(UpdateDialogState::Failed { check, reason }) => {
                UpdateDialogView::Failed { check, reason }
            }
        };
        let events = ui::show_update_dialog(ctx, view);
        for event in events {
            self.handle_update_dialog_event(ctx, event);
        }
    }

    fn handle_update_dialog_event(&mut self, ctx: &egui::Context, event: UpdateDialogEvent) {
        match event {
            UpdateDialogEvent::StartUpdate => {
                if let Some(UpdateDialogState::Available(check)) = self.update_check.dialog.take() {
                    self.start_update_apply(check);
                }
            }
            UpdateDialogEvent::OpenReleasePage => {
                let url = match self.update_check.dialog.take() {
                    Some(UpdateDialogState::Available(check))
                    | Some(UpdateDialogState::Failed { check, .. }) => check.release_url,
                    other => {
                        self.update_check.dialog = other;
                        return;
                    }
                };
                // 自動で更新できないときの逃げ道。ブラウザの起動は eframe に任せる
                info!("リリースページを開く: {}", url);
                ctx.open_url(egui::OpenUrl::new_tab(&url));
            }
            UpdateDialogEvent::Later => {
                debug!("更新の通知を閉じた（次の起動でまた知らせる）");
                self.update_check.dialog = None;
            }
            UpdateDialogEvent::Close => {
                self.update_check.dialog = None;
            }
            UpdateDialogEvent::SkipThisVersion => {
                let Some(UpdateDialogState::Available(check)) = self.update_check.dialog.take()
                else {
                    return;
                };
                info!("v{} は通知しない", check.latest);
                if let Ok(mut settings) = self.settings.lock() {
                    settings.update.skipped_version = Some(check.latest.to_string());
                } else {
                    warn!("「このバージョンは通知しない」で settings のロックを取得できない");
                    return;
                }
                self.mark_settings_dirty();
            }
            UpdateDialogEvent::CancelUpdate => self.cancel_update_apply(),
        }
    }

    /// 「その他」タブの「更新する」。見つかっている版で更新を始める。
    pub(super) fn start_update_from_settings(&mut self) {
        if let UpdateStatus::Available(check) = &self.update_check.status {
            let check = check.clone();
            self.start_update_apply(check);
        }
    }

    /// 更新（ダウンロード・照合・差し替え）を別スレッドで始める。
    ///
    /// exe の場所が分からないときだけはスレッドを起こす前に失敗を出す。
    /// フォルダに書けるか、資産があるか（1.1.0 以前の Release には無い）は
    /// スレッドの最初で確かめる（ファイルを作って消すので UI スレッドでは行わない）。
    fn start_update_apply(&mut self, check: UpdateCheck) {
        if self.update_check.apply.is_some() || self.update_check.restart.is_some() {
            debug!("更新の最中なので、重ねて始めない");
            return;
        }
        // テスト用の問い合わせ先を使っているときだけ、ローカルの資産を受け付ける
        let allow_any_source = self.update_check.overrides.source.is_some();
        let paths = match ExePaths::current() {
            Ok(paths) => paths,
            Err(e) => {
                self.fail_update_apply(check, e);
                return;
            }
        };

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let thread_paths = paths.clone();
        let thread_check = check.clone();
        // 映像が止まっている間は update() の間隔が広がっているので、届いたら起こす
        let waker = self.repaint_waker.clone();
        let spawned = std::thread::Builder::new()
            .name("update-apply".to_string())
            .spawn(move || {
                let mut report = |progress| {
                    if tx.send(ApplyMessage::Progress(progress)).is_ok() {
                        waker.wake();
                    }
                };
                let result = apply::run_apply(
                    &thread_check,
                    allow_any_source,
                    &thread_paths,
                    &thread_cancel,
                    &mut report,
                );
                // 受信側が無いのはキャンセルしたときか、アプリが終了したとき。捨ててよい
                if tx.send(ApplyMessage::Finished(result)).is_ok() {
                    waker.wake();
                }
            });

        match spawned {
            Ok(handle) => {
                info!(
                    "v{} への更新を始める（{}）",
                    check.latest,
                    paths.exe.display()
                );
                drop_finished_threads(&mut self.update_check.threads);
                self.update_check.threads.push(handle);
                self.update_check.apply = Some(ApplyJob {
                    rx,
                    cancel,
                    check,
                    paths,
                    progress: ApplyProgress::Preparing,
                });
                self.update_check.dialog = Some(UpdateDialogState::Applying);
            }
            Err(e) => {
                warn!("更新のスレッドを起こせない: {}", e);
                self.fail_update_apply(check, ApplyError::Network(e.to_string()));
            }
        }
    }

    /// 別スレッドから届いた更新の進み具合と結果を取り込む。`update()` の先頭で呼ぶ。
    ///
    /// 差し替えが済んだら、ウィンドウを閉じて通常の終了（`on_exit`）へ進む。
    /// 新しい exe は `on_exit` の最後で起動する。
    pub(super) fn drain_update_apply_results(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.update_check.apply else {
            return;
        };
        let finished = loop {
            match job.rx.try_recv() {
                Ok(ApplyMessage::Progress(progress)) => job.progress = progress,
                Ok(ApplyMessage::Finished(result)) => break Some(result),
                Err(TryRecvError::Empty) => break None,
                // 結果を送らずにスレッドが終わった。失敗として扱う
                Err(TryRecvError::Disconnected) => {
                    break Some(Err(ApplyError::Network(
                        "the update thread ended without a result".to_string(),
                    )))
                }
            }
        };
        let Some(result) = finished else {
            return;
        };
        let Some(job) = self.update_check.apply.take() else {
            return;
        };

        match result {
            Ok(()) => {
                info!(
                    "v{} へ差し替えた。終了して新しい版を起動する",
                    job.check.latest
                );
                self.errors.clear(ErrorSource::Update);
                self.update_check.restart = Some(job.paths);
                self.update_check.dialog = Some(UpdateDialogState::Restarting);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(ApplyError::Cancelled) => {
                info!("更新をキャンセルした（元の exe はそのまま）");
                self.update_check.dialog = None;
            }
            Err(e) => self.fail_update_apply(job.check, e),
        }
    }

    /// 更新の失敗をダイアログ・トースト・ログに出す。元の exe はそのまま残っている。
    fn fail_update_apply(&mut self, check: UpdateCheck, error: ApplyError) {
        // `NotWritable` などは画面の文言に詳細を入れないので、ログには中身ごと残す
        warn!("v{} へ更新できない: {:?}", check.latest, error);
        let reason = error.to_string();
        self.report_error(ErrorSource::Update, reason.clone());
        self.update_check.dialog = Some(UpdateDialogState::Failed { check, reason });
    }

    /// 更新をやめる。スレッドは読み取りの合間に気づいて `.new` を消す。
    ///
    /// 結果は待たずにダイアログを閉じる。受信側を捨てるので、止まるまでに
    /// 届いた結果は使わない（`docs/design/update.md`）。
    fn cancel_update_apply(&mut self) {
        if let Some(job) = self.update_check.apply.take() {
            info!("v{} への更新をキャンセルする", job.check.latest);
            job.cancel.store(true, Ordering::Release);
        }
        self.update_check.dialog = None;
    }

    /// 前回の更新の残り（`.old` と書きかけの `.new`）を消す。起動直後に 1 回だけ呼ぶ。
    ///
    /// 更新の直後は前の版のプロセスがまだ終わっておらず `.old` を消せないので、
    /// 別スレッドでしばらく繰り返す。消せなければ WARN を残すだけで、次の起動でまた試す。
    pub(super) fn clean_up_update_leftovers(&mut self) {
        let paths = match ExePaths::current() {
            Ok(paths) => paths,
            Err(e) => {
                warn!("前回の更新の残りを確かめられない: {}", e);
                return;
            }
        };
        if !paths.old.exists() && !paths.new.exists() {
            return;
        }

        let spawned = std::thread::Builder::new()
            .name("update-cleanup".to_string())
            .spawn(move || {
                for attempt in 1..=LEFTOVER_RETRIES {
                    match apply::remove_leftovers(&paths) {
                        Ok(()) => {
                            info!("前回の更新の残りを消した: {}", paths.old.display());
                            return;
                        }
                        Err(e) if attempt == LEFTOVER_RETRIES => {
                            warn!("前回の更新の残り {} を消せない: {}", paths.old.display(), e);
                        }
                        Err(e) => {
                            debug!("前回の更新の残りをまだ消せない（{} 回目）: {}", attempt, e);
                            std::thread::sleep(LEFTOVER_RETRY_INTERVAL);
                        }
                    }
                }
            });
        match spawned {
            Ok(handle) => self.update_check.threads.push(handle),
            Err(e) => warn!("前回の更新の残りを消すスレッドを起こせない: {}", e),
        }
    }

    /// 終了時の更新の後始末。`on_exit` の**最後**に呼ぶ。
    ///
    /// ダウンロードの最中なら止めさせる（待たない。残った `.new` は次の起動で消す）。
    /// 差し替えが済んでいれば新しい exe を起動する。設定の保存とスレッドの join を
    /// 終えてから起動するので、新しい版は保存し終えた設定を読み、デバイスも
    /// 手放されたあとに開く。起動できなければ元の exe へ戻す。
    pub(super) fn relaunch_updated_exe(&mut self) {
        if let Some(job) = &self.update_check.apply {
            job.cancel.store(true, Ordering::Release);
        }
        let Some(paths) = self.update_check.restart.take() else {
            return;
        };
        // 起動したときの引数はそのまま渡す
        match std::process::Command::new(&paths.exe)
            .args(std::env::args_os().skip(1))
            .spawn()
        {
            Ok(child) => info!(
                "新しい版を起動した（pid {}）: {}",
                child.id(),
                paths.exe.display()
            ),
            Err(e) => {
                error!("新しい版を起動できない: {}: {}", paths.exe.display(), e);
                match apply::roll_back(&paths) {
                    Ok(()) => warn!("元の exe へ戻した: {}", paths.exe.display()),
                    Err(e) => error!("元の exe へ戻せない: {}", e),
                }
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
