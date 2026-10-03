//! 更新の適用。新しい版の exe をダウンロードし、`SHA256SUMS.txt` と照合して、
//! 実行中の exe と差し替える（`docs/design/update.md` の「適用」）。
//!
//! ここにあるのは、別スレッドで走らせる本体（`run_apply`）と、その部品の
//! 純粋関数・ファイル操作だけ。スレッドを起こして進捗を画面へ渡すのは
//! `app::update`、新しい exe の起動は終了時の `on_exit`（`relaunch_updated_exe`）。
//!
//! **元の exe を壊す経路を作らない。** ダウンロードは exe と同じフォルダの
//! `<exe の名前>.new` へ落とし、照合が済むまで元の exe には触らない。
//! 差し替え（`swap_in`）が途中で失敗したら、動かした元の exe を戻して `.new` を消す。
//! 元の exe を戻せなければ `.new` は消さず、元の名前へ置く（置けなければ残す）。
//!
//! 差し替えとその戻し方、exe の隣の一時名（`ExePaths`）は `swap.rs`、
//! `SHA256SUMS.txt` の読み方と照合は `checksum.rs`、資産の選び方（`ApplyPlan`）は `assets.rs`、
//! 資産の読み取り（HTTP / ファイルを開き、小分けに読みながらキャンセルを見る）は `download.rs`。

pub use super::swap::{remove_leftovers, roll_back, ExePaths};

use super::assets::ApplyPlan;
use super::checksum::{checksum_matches, find_checksum, to_hex};
use super::download::{fetch_text, file_error, open_source, read_in_chunks, ReadLimits};
use super::swap::{ensure_writable, remove_if_exists, swap_in};
use super::UpdateCheck;
use crate::i18n::{self, Text};
use log::{info, warn};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// exe の大きさの上限。配布物は数十 MB なので、桁違いに大きいものは
/// 取り違えとみなしてディスクを埋める前に止める。
const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;

/// `SHA256SUMS.txt` の大きさの上限。数行のテキストなので十分に大きい。
pub(super) const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

/// exe の本文を受け取り終えるまでの上限（全体）。遅い回線でも数十 MB が落ちきる長さにする。
/// 受け取りが止まったときの打ち切りとキャンセルは、これとは別に読み取りを待つ間にも
/// 見る（`download::ReadLimits`）。
const EXE_BODY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// `SHA256SUMS.txt` の本文を受け取り終えるまでの上限。数行（上限 64 KiB）なので、
/// 遅い回線でも数秒で届く。exe と同じ 10 分にすると、受け取りが止まったときに
/// キャンセルしてもスレッドが 10 分残り、その間は次の更新を始められない（Issue #319）。
const CHECKSUMS_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// 大きさが分からないときに進捗を知らせる間隔。
const PROGRESS_STEP_BYTES: u64 = 256 * 1024;

/// 更新に失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// Release に自動更新用の資産（exe と `SHA256SUMS.txt`）が無い
    NoAssets,
    /// 資産の URL がこのリポジトリの Release のものではない
    UnexpectedAssetUrl(String),
    /// 実行中の exe の場所が分からない
    ExePath(String),
    /// exe のフォルダに書き込めない。持っているのはログ用の詳細
    NotWritable(String),
    /// 接続できない、または受け取りが途中で止まった
    Network(String),
    /// 上限の時間内に受け取れなかった
    Timeout,
    /// HTTP の失敗
    HttpStatus(u16),
    /// 上限（`MAX_EXE_BYTES` / `MAX_CHECKSUMS_BYTES`）より大きい
    TooLarge,
    /// `SHA256SUMS.txt` に exe の行が無い
    ChecksumMissing,
    /// ダウンロードした exe の SHA-256 が `SHA256SUMS.txt` と合わない
    ChecksumMismatch,
    /// ダウンロード先（`.new`）を書けない、または資産のファイルを読めない
    File(String),
    /// exe を置き換えられない。元の exe は戻してある
    Replace(String),
    /// exe を置き換えられず、元の exe も戻せなかったので、照合済みの新しい exe を
    /// 元の名前へ置いた。`old` は元の exe の場所
    ReplaceKeptNew { source: String, old: String },
    /// exe を置き換えられず、元の exe も新しい exe も元の名前へ置けなかった。
    /// `old` は元の exe、`new` は照合済みの新しい exe の場所
    ReplaceKeptNothing {
        source: String,
        old: String,
        new: String,
    },
    /// キャンセルされた。画面には出さない
    Cancelled,
    /// 更新のスレッドが結果を送らずに終わった。ダウンロードの失敗とは限らないので分けてある
    ThreadEnded,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ApplyError::NoAssets => Text::UpdateNoAssets.get().to_string(),
            ApplyError::UnexpectedAssetUrl(_) => Text::UpdateUnexpectedAssetUrl.get().to_string(),
            ApplyError::ExePath(source) => i18n::update_exe_path_failed(source),
            ApplyError::NotWritable(_) => Text::UpdateNotWritable.get().to_string(),
            ApplyError::Network(source) => i18n::update_download_failed(source),
            ApplyError::Timeout => Text::UpdateDownloadTimedOut.get().to_string(),
            ApplyError::HttpStatus(code) => i18n::update_download_http_status(*code),
            ApplyError::TooLarge => Text::UpdateDownloadTooLarge.get().to_string(),
            ApplyError::ChecksumMissing => Text::UpdateChecksumMissing.get().to_string(),
            ApplyError::ChecksumMismatch => Text::UpdateChecksumMismatch.get().to_string(),
            ApplyError::File(source) => i18n::update_file_failed(source),
            ApplyError::Replace(source) => i18n::update_replace_failed(source),
            ApplyError::ReplaceKeptNew { source, old } => {
                i18n::update_replace_kept_new(old, source)
            }
            ApplyError::ReplaceKeptNothing { source, old, new } => {
                i18n::update_replace_kept_nothing(old, new, source)
            }
            ApplyError::Cancelled => Text::UpdateCancelled.get().to_string(),
            ApplyError::ThreadEnded => Text::UpdateThreadEnded.get().to_string(),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for ApplyError {}

/// 別スレッドから画面へ返す進み具合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyProgress {
    /// 書き込めるかの確認と `SHA256SUMS.txt` の取得
    Preparing,
    /// exe を受け取っている。`total` は大きさが分からなければ `None`
    Downloading { downloaded: u64, total: Option<u64> },
    /// 照合が済み、置き換えている
    Installing,
}

impl ApplyProgress {
    /// 受け取った割合（0〜100）。大きさが分からない段階なら `None`。
    pub fn percent(self) -> Option<u8> {
        match self {
            ApplyProgress::Downloading {
                downloaded,
                total: Some(total),
            } if total > 0 => Some((downloaded.min(total) * 100 / total) as u8),
            _ => None,
        }
    }
}

/// `check` の版へ更新する。**ブロックする**ので UI スレッドから呼ばない。
///
/// 1. `paths` のフォルダに書けるかを確かめる（書けなければ何も落とさない）
/// 2. 資産を選ぶ（`ApplyPlan::from_check`。`allow_any_source` はそちらを参照）
/// 3. `SHA256SUMS.txt` を落とし、exe の行の hash を取る
/// 4. exe を `.new` へ落としながら SHA-256 を計算し、照合する
/// 5. 実行中の exe を `.old` へ、`.new` を元の名前へ改名する（`swap_in`）
///
/// 書けるかを資産より先に見るのは、書けないフォルダ（Program Files など）は
/// どの版でも自動更新できず、exe を移せば直る、という伝えるべきことだから。
///
/// 失敗・キャンセルのどちらでも `.new` は消し、元の exe は元の名前のまま残す。
/// 例外は差し替えで元の exe を戻せなかったときで、`.new` を元の名前へ置くか、
/// 置けなければ `.old` と `.new` を残す（`ReplaceKeptNew` / `ReplaceKeptNothing`）。
/// 成功したら、新しい exe は `paths.exe` にあり、起動は呼び出し側が行う。
/// キャンセルは読み取りの合間に見る。差し替えの直前に `ApplyControl::begin_swap` で
/// キャンセルと取り合い、先にキャンセルされていれば差し替えない。
pub fn run_apply(
    check: &UpdateCheck,
    allow_any_source: bool,
    paths: &ExePaths,
    cancel: &ApplyControl,
    progress: &mut dyn FnMut(ApplyProgress),
) -> Result<(), ApplyError> {
    progress(ApplyProgress::Preparing);
    ensure_writable(paths.dir())
        .map_err(|e| ApplyError::NotWritable(format!("{}: {}", paths.dir().display(), e)))?;
    let plan = &ApplyPlan::from_check(check, allow_any_source)?;

    let sums = fetch_text(
        &plan.checksums,
        MAX_CHECKSUMS_BYTES,
        ReadLimits::with_body(CHECKSUMS_BODY_TIMEOUT),
        cancel,
    )?;
    let expected = find_checksum(&sums, &plan.exe_name)
        .ok_or(ApplyError::ChecksumMissing)?
        .to_string();

    let result = download_and_verify(plan, paths, &expected, cancel, progress);
    if result.is_err() {
        remove_if_exists(&paths.new);
        return result;
    }

    progress(ApplyProgress::Installing);
    if !cancel.begin_swap() {
        remove_if_exists(&paths.new);
        return Err(ApplyError::Cancelled);
    }
    swap_in(paths)
}

fn download_and_verify(
    plan: &ApplyPlan,
    paths: &ExePaths,
    expected: &str,
    cancel: &ApplyControl,
    progress: &mut dyn FnMut(ApplyProgress),
) -> Result<(), ApplyError> {
    let (mut reader, total) =
        open_source(&plan.exe, ReadLimits::with_body(EXE_BODY_TIMEOUT), cancel)?;
    if total.is_some_and(|total| total > MAX_EXE_BYTES) {
        return Err(ApplyError::TooLarge);
    }
    info!(
        "更新の exe を落とす: {:?} → {}（{:?} バイト）",
        plan.exe,
        paths.new.display(),
        total
    );

    let mut file = File::create(&paths.new).map_err(|e| file_error(&paths.new, e))?;
    let mut hasher = Sha256::new();
    let mut reported: Option<ApplyProgress> = None;
    let downloaded = read_in_chunks(
        &mut reader,
        &plan.exe,
        MAX_EXE_BYTES,
        cancel,
        |chunk, downloaded| {
            hasher.update(chunk);
            file.write_all(chunk)
                .map_err(|e| file_error(&paths.new, e))?;

            let now = ApplyProgress::Downloading { downloaded, total };
            if should_report(reported, now) {
                progress(now);
                reported = Some(now);
            }
            Ok(())
        },
    )?;
    // 改名する前にディスクへ書き切る。途中で電源が落ちても、
    // 照合を通った中身が元の名前に来るようにする
    file.sync_all().map_err(|e| file_error(&paths.new, e))?;
    drop(file);

    let actual = hasher.finalize();
    if !checksum_matches(expected, actual.as_slice()) {
        warn!(
            "更新の exe の SHA-256 が合わない（期待: {}、実際: {}）",
            expected,
            to_hex(actual.as_slice())
        );
        return Err(ApplyError::ChecksumMismatch);
    }
    info!("更新の exe を照合した（{} バイト）", downloaded);
    Ok(())
}

/// 進捗を画面へ知らせるか。割合が 1 つ進んだとき（大きさが分からなければ
/// `PROGRESS_STEP_BYTES` ごと）だけにして、チャネルを細かい通知で埋めない。
fn should_report(last: Option<ApplyProgress>, now: ApplyProgress) -> bool {
    let Some(last) = last else {
        return true;
    };
    match (last.percent(), now.percent()) {
        (Some(last), Some(now)) => now > last,
        _ => match (last, now) {
            (
                ApplyProgress::Downloading { downloaded: a, .. },
                ApplyProgress::Downloading { downloaded: b, .. },
            ) => b / PROGRESS_STEP_BYTES > a / PROGRESS_STEP_BYTES,
            _ => true,
        },
    }
}

/// 更新のスレッドと UI スレッドで共有する、キャンセルと差し替えの取り合い。
///
/// キャンセルと差し替えの開始は、どちらか先に来た方だけが通る（`compare_exchange`）。
/// **差し替えを始めたら、もうキャンセルできない。** キャンセルが通らなかった側
/// （`on_exit`）は差し替えが終わるのを待つ。実行中の exe を `.old` へ動かしてから
/// `.new` を元の名前へ置くまでの間にプロセスが終わると、元の名前に exe が
/// 1 つも無くなるため。ダウンロードの最中はキャンセルが通るので、待たない。
///
/// `Mutex` にしないのは、差し替え（ファイルの改名）の間ロックを握ることになるため
/// （`GUARDRAIL.md`）。状態を `Arc` に入れているのは、ureq の接続（`'static` を求める）
/// へキャンセルを見るだけの控え（`CancelWatch`）を渡すため。
#[derive(Debug, Default)]
pub struct ApplyControl {
    state: Arc<AtomicU8>,
}

/// キャンセルされたかを見るだけの控え。読み取りを待つ間の ureq の接続が持つ
/// （`download::WatchedTransport`）。
#[derive(Debug, Clone)]
pub(super) struct CancelWatch(Arc<AtomicU8>);

impl CancelWatch {
    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == CONTROL_CANCELLED
    }
}

const CONTROL_RUNNING: u8 = 0;
const CONTROL_CANCELLED: u8 = 1;
const CONTROL_SWAPPING: u8 = 2;

impl ApplyControl {
    /// キャンセルする。差し替えを始める前なら `true`（スレッドは止まり `.new` を消す）。
    /// もう差し替えを始めていれば `false` で、呼び出し側は終わるのを待つ。
    pub fn cancel(&self) -> bool {
        match self.state.compare_exchange(
            CONTROL_RUNNING,
            CONTROL_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(current) => current == CONTROL_CANCELLED,
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CONTROL_CANCELLED
    }

    pub(super) fn watch(&self) -> CancelWatch {
        CancelWatch(Arc::clone(&self.state))
    }

    /// 差し替えを始める。先にキャンセルされていれば `false`。
    fn begin_swap(&self) -> bool {
        self.state
            .compare_exchange(
                CONTROL_RUNNING,
                CONTROL_SWAPPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::assets::{legacy_exe_asset_name, CHECKSUMS_ASSET_NAME, EXE_ASSET_NAME};
    use crate::update::download::slow_http_server;
    use crate::update::ReleaseAsset;
    use semver::Version;
    use std::fs;
    use std::path::Path;

    fn check_with_assets(tag: &str, assets: &[(&str, &str)]) -> UpdateCheck {
        UpdateCheck {
            current: Version::new(1, 0, 0),
            latest: Version::new(1, 2, 0),
            tag: tag.to_string(),
            release_url: String::new(),
            assets: assets
                .iter()
                .map(|(name, url)| ReleaseAsset {
                    name: name.to_string(),
                    download_url: url.to_string(),
                })
                .collect(),
        }
    }

    // "hello" の SHA-256
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    // ---- 進捗 ----

    #[test]
    fn apply_progress_percent_is_clamped_and_needs_a_size() {
        let downloading = |downloaded, total| ApplyProgress::Downloading { downloaded, total };

        assert_eq!(downloading(0, Some(200)).percent(), Some(0));
        assert_eq!(downloading(199, Some(200)).percent(), Some(99));
        assert_eq!(downloading(200, Some(200)).percent(), Some(100));
        assert_eq!(downloading(300, Some(200)).percent(), Some(100));
        assert_eq!(downloading(10, Some(0)).percent(), None);
        assert_eq!(downloading(10, None).percent(), None);
        assert_eq!(ApplyProgress::Installing.percent(), None);
    }

    #[test]
    fn should_report_only_when_percent_or_step_advances() {
        let downloading = |downloaded, total| ApplyProgress::Downloading { downloaded, total };

        assert!(should_report(None, downloading(0, Some(1000))));
        assert!(!should_report(
            Some(downloading(10, Some(1000))),
            downloading(19, Some(1000))
        ));
        assert!(should_report(
            Some(downloading(19, Some(1000))),
            downloading(20, Some(1000))
        ));
        // 大きさが分からなければ 256 KiB ごと
        assert!(!should_report(
            Some(downloading(1, None)),
            downloading(256 * 1024 - 1, None)
        ));
        assert!(should_report(
            Some(downloading(1, None)),
            downloading(256 * 1024, None)
        ));
        assert!(should_report(
            Some(ApplyProgress::Preparing),
            downloading(0, None)
        ));
    }

    // ---- 一連の流れ（ローカルのファイルを資産にする） ----

    fn dummy_paths(dir: &Path) -> ExePaths {
        ExePaths::for_exe(dir.join("capturecard_viewer.exe")).expect("名前がある")
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("読めなければならない")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: ExePaths,
        check: UpdateCheck,
    }

    /// 新しい exe（中身は "hello"）と SHA256SUMS.txt を資産として置き、
    /// 差し替え先にダミーの exe（中身は "old"）を置く。
    fn fixture(exe_name: &str, sums: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let release = dir.path().join("release");
        let install = dir.path().join("install");
        fs::create_dir(&release).unwrap();
        fs::create_dir(&install).unwrap();
        fs::write(release.join(exe_name), "hello").unwrap();
        fs::write(release.join(CHECKSUMS_ASSET_NAME), sums).unwrap();
        let paths = dummy_paths(&install);
        fs::write(&paths.exe, "old").unwrap();

        let url = |name: &str| format!("file:///{}", release.join(name).display());
        let check = check_with_assets(
            "v9.9.9",
            &[
                (exe_name, &url(exe_name)),
                (CHECKSUMS_ASSET_NAME, &url(CHECKSUMS_ASSET_NAME)),
            ],
        );
        Fixture {
            _dir: dir,
            paths,
            check,
        }
    }

    fn run(fixture: &Fixture, cancel: bool) -> (Result<(), ApplyError>, Vec<ApplyProgress>) {
        let flag = ApplyControl::default();
        if cancel {
            flag.cancel();
        }
        let mut seen = Vec::new();
        let result = run_apply(&fixture.check, true, &fixture.paths, &flag, &mut |p| {
            seen.push(p)
        });
        (result, seen)
    }

    #[test]
    fn run_apply_downloads_verifies_and_swaps() {
        // 生成側は小文字だが、大文字の hash でも通す
        let fixture = fixture(
            EXE_ASSET_NAME,
            &format!("{}  capturecard_viewer.exe\n", HELLO_SHA256.to_uppercase()),
        );

        let (result, seen) = run(&fixture, false);

        assert_eq!(result, Ok(()));
        assert_eq!(read(&fixture.paths.exe), "hello");
        assert_eq!(read(&fixture.paths.old), "old");
        assert!(!fixture.paths.new.exists());
        assert_eq!(seen.first(), Some(&ApplyProgress::Preparing));
        assert!(seen.contains(&ApplyProgress::Downloading {
            downloaded: 5,
            total: Some(5)
        }));
        assert_eq!(seen.last(), Some(&ApplyProgress::Installing));
    }

    #[test]
    fn run_apply_with_the_legacy_exe_name_looks_up_its_own_line() {
        // 旧名の Release では、SHA256SUMS.txt の行も旧名で引く
        let legacy = legacy_exe_asset_name("v9.9.9");
        let fixture = fixture(
            &legacy,
            &format!(
                "{}  capturecard_viewer.exe\n{HELLO_SHA256}  {legacy}\n",
                "0".repeat(64)
            ),
        );

        let (result, _) = run(&fixture, false);

        assert_eq!(result, Ok(()));
        assert_eq!(read(&fixture.paths.exe), "hello");
        assert!(!fixture.paths.new.exists());
    }

    #[test]
    fn run_apply_checksum_mismatch_keeps_the_exe_and_removes_new() {
        let fixture = fixture(
            EXE_ASSET_NAME,
            &format!("{}  capturecard_viewer.exe\n", "0".repeat(64)),
        );

        let (result, _) = run(&fixture, false);

        assert_eq!(result, Err(ApplyError::ChecksumMismatch));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
        assert!(!fixture.paths.old.exists());
    }

    #[test]
    fn run_apply_missing_checksum_line_downloads_nothing() {
        let fixture = fixture(
            EXE_ASSET_NAME,
            &format!("{HELLO_SHA256}  something-else.exe\n"),
        );

        let (result, _) = run(&fixture, false);

        assert_eq!(result, Err(ApplyError::ChecksumMissing));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
    }

    #[test]
    fn run_apply_cancelled_keeps_the_exe_and_removes_new() {
        let fixture = fixture(
            EXE_ASSET_NAME,
            &format!("{HELLO_SHA256}  capturecard_viewer.exe\n"),
        );

        let (result, _) = run(&fixture, true);

        assert_eq!(result, Err(ApplyError::Cancelled));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
        assert!(!fixture.paths.old.exists());
    }

    #[test]
    fn run_apply_unwritable_dir_downloads_nothing() {
        let fixture = fixture(
            EXE_ASSET_NAME,
            &format!("{HELLO_SHA256}  capturecard_viewer.exe\n"),
        );
        // フォルダが無い = 書けない
        let paths = dummy_paths(&fixture.paths.dir().join("missing"));
        let flag = ApplyControl::default();

        let result = run_apply(&fixture.check, true, &paths, &flag, &mut |_| {});

        assert!(matches!(result, Err(ApplyError::NotWritable(_))));
        assert_eq!(read(&fixture.paths.exe), "old");
    }

    #[test]
    fn run_apply_checks_the_folder_before_the_assets() {
        // 資産の無い版でも、書けないフォルダならそちらを先に伝える
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(&dir.path().join("missing"));
        let no_assets = check_with_assets("v1.1.0", &[]);
        let flag = ApplyControl::default();

        let unwritable = run_apply(&no_assets, false, &paths, &flag, &mut |_| {});
        let writable = run_apply(
            &no_assets,
            false,
            &dummy_paths(dir.path()),
            &flag,
            &mut |_| {},
        );

        assert!(matches!(unwritable, Err(ApplyError::NotWritable(_))));
        assert_eq!(writable, Err(ApplyError::NoAssets));
    }

    // ---- 受け取り中のキャンセル（ローカルの HTTP サーバー） ----

    /// exe を `exe_url`、`SHA256SUMS.txt` を `sums_url` から取る `run_apply` を 200ms 後に
    /// キャンセルし、結果と `.new` が残っているかを返す。キャンセルから 10 秒で
    /// 戻らなければ失敗とする（戻ればスレッドが終わり、`is_applying` が偽になる）。
    fn cancel_run_apply_after_200ms(
        exe_url: &str,
        sums_url: &str,
    ) -> (Result<(), ApplyError>, bool) {
        use std::sync::{mpsc, Arc};

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        let check = check_with_assets(
            "v9.9.9",
            &[(EXE_ASSET_NAME, exe_url), (CHECKSUMS_ASSET_NAME, sums_url)],
        );
        let control = Arc::new(ApplyControl::default());
        let (tx, rx) = mpsc::channel();
        let worker_control = Arc::clone(&control);
        std::thread::spawn(move || {
            let result = run_apply(&check, true, &paths, &worker_control, &mut |_| {});
            let _ = tx.send((result, paths.new.exists()));
            drop(dir);
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(control.cancel());
        rx.recv_timeout(Duration::from_secs(10))
            .expect("キャンセルしてから 10 秒以内に戻らなければならない")
    }

    #[test]
    fn run_apply_cancel_while_checksums_trickle_in_returns_promptly() {
        // 本文が少しずつしか届かなくても、読み取りの合間にキャンセルへ気づく
        let url = slow_http_server(Some(Duration::from_millis(20)));

        let (result, _) =
            cancel_run_apply_after_200ms("http://127.0.0.1:9/capturecard_viewer.exe", &url);

        assert_eq!(result, Err(ApplyError::Cancelled));
    }

    #[test]
    fn run_apply_cancel_while_the_exe_is_stalled_returns_promptly() {
        // exe の受け取りが完全に止まっても（1 バイトも届かない）、本文の上限（10 分）を
        // 待たずにキャンセルへ気づき、`.new` を消す（Issue #357）
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let sums = dir.path().join(CHECKSUMS_ASSET_NAME);
        fs::write(&sums, format!("{HELLO_SHA256}  capturecard_viewer.exe\n")).unwrap();
        let exe_url = slow_http_server(None);

        let (result, new_left) =
            cancel_run_apply_after_200ms(&exe_url, &format!("file:///{}", sums.display()));

        assert_eq!(result, Err(ApplyError::Cancelled));
        assert!(!new_left);
    }

    // ---- キャンセルと差し替えの取り合い ----

    #[test]
    fn apply_control_cancel_before_swap_wins() {
        let control = ApplyControl::default();

        assert!(control.cancel());
        // 2 回目も「止まる側」として扱う
        assert!(control.cancel());
        assert!(!control.begin_swap());
    }

    #[test]
    fn apply_control_cancel_after_swap_started_is_refused() {
        // 差し替えを始めたら止めない。呼び出し側は終わるのを待つ
        let control = ApplyControl::default();

        assert!(control.begin_swap());
        assert!(!control.cancel());
        assert!(!control.is_cancelled());
    }

    #[test]
    fn apply_error_display_follows_the_language() {
        assert_eq!(
            ApplyError::NotWritable("C:/x".to_string()).to_string(),
            "このフォルダには書き込めないため自動更新できません。リリースページから手動で更新してください"
        );
        assert_eq!(
            i18n::with_language(i18n::Language::English, || ApplyError::HttpStatus(404)
                .to_string()),
            "The download returned HTTP 404"
        );
    }

    // #315: スレッドが結果を返さずに終わったことを「ダウンロードできない」に分類せず、
    // 言語も混ぜない
    #[test]
    fn thread_ended_display_is_its_own_localized_message() {
        let japanese = i18n::with_language(i18n::Language::Japanese, || {
            ApplyError::ThreadEnded.to_string()
        });
        assert_eq!(japanese, "更新のスレッドが結果を返さずに終わった");
        let english = i18n::with_language(i18n::Language::English, || {
            ApplyError::ThreadEnded.to_string()
        });
        assert_eq!(english, "The update thread ended without a result");
    }
}
