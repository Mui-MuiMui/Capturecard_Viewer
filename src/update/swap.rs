//! 更新の適用のうち、ファイルの置き換えにまつわる部分。exe の隣の一時名
//! （`.new` / `.old`）、フォルダに書けるかの確認、差し替えと失敗したときの戻し方、
//! 前回の更新の残りの後片付け（`docs/design/update.md` の「適用」）。
//!
//! **元の exe を壊す経路を作らない。** 戻し方の順は純粋関数（`recovery_for`）で決める。

use super::apply::ApplyError;
use log::{info, warn};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// 実行中の exe と、その隣に置く一時名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExePaths {
    /// 実行中の exe
    pub exe: PathBuf,
    /// ダウンロード先（`<exe の名前>.new`）。照合が済んでから `exe` へ改名する
    pub new: PathBuf,
    /// 差し替えで退避した元の exe（`<exe の名前>.old`）。次の起動で消す
    pub old: PathBuf,
}

impl ExePaths {
    /// `exe` の隣の一時名を決める。ファイル名の無いパスなら `None`。
    ///
    /// 名前は実行中の exe の名前に `.new` / `.old` を足したもの。
    /// exe の名前を変えて使っていても、その名前のまま差し替わる。
    pub fn for_exe(exe: PathBuf) -> Option<Self> {
        let name = exe.file_name()?.to_os_string();
        let with_suffix = |suffix: &str| {
            let mut name = name.clone();
            name.push(suffix);
            exe.with_file_name(name)
        };
        Some(Self {
            new: with_suffix(".new"),
            old: with_suffix(".old"),
            exe,
        })
    }

    /// 実行中の exe について決める。
    pub fn current() -> Result<Self, ApplyError> {
        let exe = std::env::current_exe().map_err(|e| ApplyError::ExePath(e.to_string()))?;
        let display = exe.display().to_string();
        Self::for_exe(exe).ok_or(ApplyError::ExePath(display))
    }

    /// exe を置いてあるフォルダ。
    pub fn dir(&self) -> &Path {
        self.exe.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// `dir` に一時ファイルを作って消せるかを確かめる。
///
/// Program Files のように書けないフォルダでは、ダウンロードを始める前にここで
/// 分かる。作れても消せなければ、差し替え（改名）もできないとみなす。
pub fn ensure_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(
        ".capturecard_viewer-write-test-{}.tmp",
        std::process::id()
    ));
    File::create(&probe)?;
    fs::remove_file(&probe)
}

/// 差し替えの手順。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapStep {
    /// 前回の更新で残った `.old` を消す
    RemoveStaleOld,
    /// 実行中の exe を `.old` へ改名する（Windows は実行中でも改名できる）
    MoveCurrentToOld,
    /// `.new` を元の名前へ改名する
    MoveNewToCurrent,
}

/// 差し替えが途中で失敗したときの戻し方の 1 手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// `.old` へ動かした元の exe を元の名前へ戻す
    RestoreOld,
    /// 元の exe を戻せなかったので、照合済みの `.new` を元の名前へ置く
    PutNewInPlace,
    /// ダウンロードした `.new` を消す
    RemoveNew,
}

/// 戻し終えたとき、元の名前に何が残ったか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// 元の exe（一度も動かしていないか、`.old` から戻した）
    Old,
    /// 照合済みの新しい exe。元の exe は `.old` に残っている
    New,
    /// どちらも置けなかった。元の exe は `.old`、新しい exe は `.new` に残っている
    Nothing,
}

/// 戻し方の次の 1 手か、終わりか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryNext {
    Then(Recovery),
    Done(Kept),
}

/// `failed` の手順で失敗したとき、最初に何をするか。
///
/// 元の exe が元の名前から離れるのは `MoveCurrentToOld` が成功したあとだけ。
/// それより前の失敗では元の exe はそのままなので、`.new` を消すだけでよい。
/// `MoveNewToCurrent` の失敗では元の名前が空いているので、先に `.old` を戻す。
pub fn recovery_for(failed: SwapStep) -> Recovery {
    match failed {
        SwapStep::RemoveStaleOld | SwapStep::MoveCurrentToOld => Recovery::RemoveNew,
        SwapStep::MoveNewToCurrent => Recovery::RestoreOld,
    }
}

/// 差し替えを始めてよいか。元の名前に exe があるときだけ真。
///
/// 前回の差し替えが `ReplaceKeptNothing` で終わると、元の名前に exe が無く、
/// 元の exe は `.old` にある。そこから再試行して最初の手順（`RemoveStaleOld`）へ
/// 進むと元の exe を消してしまう（Issue #332）。
pub fn may_swap(exe_exists: bool) -> bool {
    exe_exists
}

/// `done` を試して `succeeded` だったとき、次に何をするか。
///
/// **`.new` を消すのは、元の名前に元の exe があるときだけ。** `.old` を戻せなければ
/// `.new` を元の名前へ置き直し（`roll_back` と同じ考え方）、それも駄目なら `.new` を
/// 残す。消すと元の名前に何も無いまま、手で置ける照合済みの exe が 1 つ減る。
/// `.new` を消せなかったときは、元の exe が元の名前にあるので害は無く、次の起動で消す。
pub fn next_recovery(done: Recovery, succeeded: bool) -> RecoveryNext {
    match (done, succeeded) {
        (Recovery::RestoreOld, true) => RecoveryNext::Then(Recovery::RemoveNew),
        (Recovery::RestoreOld, false) => RecoveryNext::Then(Recovery::PutNewInPlace),
        (Recovery::PutNewInPlace, true) => RecoveryNext::Done(Kept::New),
        (Recovery::PutNewInPlace, false) => RecoveryNext::Done(Kept::Nothing),
        (Recovery::RemoveNew, _) => RecoveryNext::Done(Kept::Old),
    }
}

/// 照合の済んだ `.new` を元の名前へ置く。元の exe は `.old` に退避する。
///
/// 途中で失敗したら `recovery_for` / `next_recovery` の手順で戻し、元の名前に何が
/// 残ったかで `ApplyError::Replace` / `ReplaceKeptNew` / `ReplaceKeptNothing` を返す。
///
/// 元の名前に exe が無ければ（`may_swap` が偽）、どの手順にも進まず
/// `ReplaceKeptNothing` を返す。そのとき `.old` は元の exe かもしれないので消さない。
pub fn swap_in(paths: &ExePaths) -> Result<(), ApplyError> {
    if !may_swap(paths.exe.exists()) {
        warn!(
            "元の名前に exe が無いので差し替えない（{} と {} には触らない）",
            paths.old.display(),
            paths.new.display()
        );
        return Err(ApplyError::ReplaceKeptNothing {
            source: crate::i18n::update_exe_missing(paths.exe.display()),
            old: paths.old.display().to_string(),
            new: paths.new.display().to_string(),
        });
    }
    let steps = [
        SwapStep::RemoveStaleOld,
        SwapStep::MoveCurrentToOld,
        SwapStep::MoveNewToCurrent,
    ];
    for step in steps {
        let result = match step {
            SwapStep::RemoveStaleOld => remove_file_if_exists(&paths.old),
            SwapStep::MoveCurrentToOld => fs::rename(&paths.exe, &paths.old),
            SwapStep::MoveNewToCurrent => fs::rename(&paths.new, &paths.exe),
        };
        if let Err(e) = result {
            warn!("exe の差し替えに失敗した（{:?}）: {}", step, e);
            let source = e.to_string();
            return Err(match recover(paths, step) {
                Kept::Old => ApplyError::Replace(source),
                Kept::New => ApplyError::ReplaceKeptNew {
                    source,
                    old: paths.old.display().to_string(),
                },
                Kept::Nothing => ApplyError::ReplaceKeptNothing {
                    source,
                    old: paths.old.display().to_string(),
                    new: paths.new.display().to_string(),
                },
            });
        }
    }
    info!(
        "exe を差し替えた: {}（元の exe は {}）",
        paths.exe.display(),
        paths.old.display()
    );
    Ok(())
}

fn recover(paths: &ExePaths, failed: SwapStep) -> Kept {
    recover_with(failed, |recovery| match recovery {
        Recovery::RestoreOld => match fs::rename(&paths.old, &paths.exe) {
            Ok(()) => {
                info!("元の exe を戻した: {}", paths.exe.display());
                true
            }
            Err(e) => {
                warn!(
                    "元の exe を戻せない（{} に残っている）: {}",
                    paths.old.display(),
                    e
                );
                false
            }
        },
        Recovery::PutNewInPlace => match fs::rename(&paths.new, &paths.exe) {
            Ok(()) => {
                warn!(
                    "新しい exe を元の名前へ置いた: {}（元の exe は {}）",
                    paths.exe.display(),
                    paths.old.display()
                );
                true
            }
            Err(e) => {
                warn!(
                    "新しい exe も元の名前へ置けない（{} に残っている）: {}",
                    paths.new.display(),
                    e
                );
                false
            }
        },
        Recovery::RemoveNew => {
            remove_if_exists(&paths.new);
            true
        }
    })
}

/// `run` で 1 手ずつ試し、`next_recovery` で次を決める。`run` は成功したかを返す。
fn recover_with(failed: SwapStep, mut run: impl FnMut(Recovery) -> bool) -> Kept {
    let mut recovery = recovery_for(failed);
    loop {
        let succeeded = run(recovery);
        match next_recovery(recovery, succeeded) {
            RecoveryNext::Then(next) => recovery = next,
            RecoveryNext::Done(kept) => return kept,
        }
    }
}

/// 差し替えたあとで新しい exe を起動できなかったとき、元の exe へ戻す。
///
/// 新しい exe を `.new` へ戻してから `.old` を元の名前へ戻し、最後に `.new` を消す。
/// 元の exe を戻せなければ、新しい exe を元の名前へ置き直す（どちらかの exe は
/// 必ず元の名前に残す）。
pub fn roll_back(paths: &ExePaths) -> io::Result<()> {
    fs::rename(&paths.exe, &paths.new)?;
    if let Err(e) = fs::rename(&paths.old, &paths.exe) {
        if let Err(back) = fs::rename(&paths.new, &paths.exe) {
            warn!(
                "新しい exe も元の名前へ戻せない（{} に残っている）: {}",
                paths.new.display(),
                back
            );
        }
        return Err(e);
    }
    remove_if_exists(&paths.new);
    Ok(())
}

/// 前回の更新で残ったものを消す。**起動時に呼ぶ。**
///
/// `.old` は差し替えで退避した前の版、`.new` はダウンロードの途中で終了したときの
/// 書きかけ。どちらも使わない。無ければ何もしない。`.old` は前の版のプロセスが
/// まだ終わりきっていないと消せないので、失敗は返して呼び出し側に任せる。
pub fn remove_leftovers(paths: &ExePaths) -> io::Result<()> {
    remove_if_exists(&paths.new);
    remove_file_if_exists(&paths.old)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// 消せなくてもログに残すだけにする（戻し方の途中で、それ以上できることが無い）。
pub(super) fn remove_if_exists(path: &Path) {
    if let Err(e) = remove_file_if_exists(path) {
        warn!("{} を消せない: {}", path.display(), e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_paths_for_exe_appends_suffixes_to_the_file_name() {
        let paths = ExePaths::for_exe(PathBuf::from(r"C:\tools\viewer.exe")).expect("名前がある");

        assert_eq!(paths.new, PathBuf::from(r"C:\tools\viewer.exe.new"));
        assert_eq!(paths.old, PathBuf::from(r"C:\tools\viewer.exe.old"));
        assert_eq!(paths.dir(), Path::new(r"C:\tools"));
        assert_eq!(ExePaths::for_exe(PathBuf::from(r"C:\")), None);
    }

    #[test]
    fn may_swap_only_when_the_exe_is_in_place() {
        assert!(may_swap(true));
        assert!(!may_swap(false));
    }

    // ---- 差し替えの戻し方 ----

    #[test]
    fn recovery_for_failures_before_moving_the_exe_only_removes_new() {
        assert_eq!(recovery_for(SwapStep::RemoveStaleOld), Recovery::RemoveNew);
        assert_eq!(
            recovery_for(SwapStep::MoveCurrentToOld),
            Recovery::RemoveNew
        );
    }

    #[test]
    fn recovery_for_failure_after_moving_the_exe_restores_it_first() {
        // 元の名前が空いているので、.new に触る前に .old を戻す
        assert_eq!(
            recovery_for(SwapStep::MoveNewToCurrent),
            Recovery::RestoreOld
        );
    }

    #[test]
    fn next_recovery_removes_new_only_after_the_old_exe_is_back() {
        assert_eq!(
            next_recovery(Recovery::RestoreOld, true),
            RecoveryNext::Then(Recovery::RemoveNew)
        );
        // 戻せなければ .new を消さず、元の名前へ置く
        assert_eq!(
            next_recovery(Recovery::RestoreOld, false),
            RecoveryNext::Then(Recovery::PutNewInPlace)
        );
        assert_eq!(
            next_recovery(Recovery::PutNewInPlace, true),
            RecoveryNext::Done(Kept::New)
        );
        assert_eq!(
            next_recovery(Recovery::PutNewInPlace, false),
            RecoveryNext::Done(Kept::Nothing)
        );
        // .new を消せなくても、元の名前には元の exe がある
        assert_eq!(
            next_recovery(Recovery::RemoveNew, true),
            RecoveryNext::Done(Kept::Old)
        );
        assert_eq!(
            next_recovery(Recovery::RemoveNew, false),
            RecoveryNext::Done(Kept::Old)
        );
    }

    /// `recover_with` を、`fails` に挙げた手だけ失敗させて回す。試した手の順と結果を返す。
    fn run_recovery(failed: SwapStep, fails: &[Recovery]) -> (Vec<Recovery>, Kept) {
        let mut ran = Vec::new();
        let kept = recover_with(failed, |recovery| {
            ran.push(recovery);
            !fails.contains(&recovery)
        });
        (ran, kept)
    }

    #[test]
    fn recover_restoring_the_old_exe_then_removes_new() {
        let (ran, kept) = run_recovery(SwapStep::MoveNewToCurrent, &[]);

        assert_eq!(ran, [Recovery::RestoreOld, Recovery::RemoveNew]);
        assert_eq!(kept, Kept::Old);
    }

    #[test]
    fn recover_keeps_new_when_the_old_exe_cannot_be_restored() {
        // .old を戻せなかったら、照合済みの .new を消さずに元の名前へ置く（Issue #305）
        let (ran, kept) = run_recovery(SwapStep::MoveNewToCurrent, &[Recovery::RestoreOld]);

        assert_eq!(ran, [Recovery::RestoreOld, Recovery::PutNewInPlace]);
        assert_eq!(kept, Kept::New);
    }

    #[test]
    fn recover_leaves_old_and_new_when_neither_can_be_put_back() {
        let (ran, kept) = run_recovery(
            SwapStep::MoveNewToCurrent,
            &[Recovery::RestoreOld, Recovery::PutNewInPlace],
        );

        assert_eq!(ran, [Recovery::RestoreOld, Recovery::PutNewInPlace]);
        assert_eq!(kept, Kept::Nothing);
    }

    #[test]
    fn recover_before_moving_the_exe_only_removes_new() {
        let (ran, kept) = run_recovery(SwapStep::MoveCurrentToOld, &[Recovery::RemoveNew]);

        assert_eq!(ran, [Recovery::RemoveNew]);
        assert_eq!(kept, Kept::Old);
    }

    // ---- ファイル操作（一時ディレクトリで実際に改名する） ----

    fn dummy_paths(dir: &Path) -> ExePaths {
        ExePaths::for_exe(dir.join("capturecard_viewer.exe")).expect("名前がある")
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("読めなければならない")
    }

    #[test]
    fn swap_in_moves_new_into_place_and_keeps_the_old_exe() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "old").unwrap();
        fs::write(&paths.new, "new").unwrap();
        // 前回の更新の残り
        fs::write(&paths.old, "older").unwrap();

        swap_in(&paths).expect("差し替えられる");

        assert_eq!(read(&paths.exe), "new");
        assert_eq!(read(&paths.old), "old");
        assert!(!paths.new.exists());
    }

    #[test]
    fn swap_in_failing_to_remove_stale_old_keeps_exe_and_removes_new() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "old").unwrap();
        fs::write(&paths.new, "new").unwrap();
        // 中身のあるフォルダはファイルとして消せない
        fs::create_dir(&paths.old).unwrap();
        fs::write(paths.old.join("x"), "x").unwrap();

        assert!(matches!(swap_in(&paths), Err(ApplyError::Replace(_))));

        assert_eq!(read(&paths.exe), "old");
        assert!(!paths.new.exists());
    }

    #[test]
    fn swap_in_without_the_exe_or_old_keeps_new() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        // 元の exe が無いので、手順に進まない。照合済みの .new は残す
        fs::write(&paths.new, "new").unwrap();

        assert!(matches!(
            swap_in(&paths),
            Err(ApplyError::ReplaceKeptNothing { .. })
        ));

        assert_eq!(read(&paths.new), "new");
        assert!(!paths.exe.exists());
        assert!(!paths.old.exists());
    }

    #[test]
    fn swap_in_failing_to_move_new_restores_the_old_exe() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "old").unwrap();
        // .new が無いので、元の exe を退避したあとの改名で失敗する

        assert!(matches!(swap_in(&paths), Err(ApplyError::Replace(_))));

        assert_eq!(read(&paths.exe), "old");
        assert!(!paths.old.exists());
    }

    #[test]
    fn swap_in_without_the_exe_keeps_old_and_new() {
        // 前回 ReplaceKeptNothing で終わった状態（元の名前に exe が無く、.old と .new だけ）
        // から再試行しても、元の exe である .old を消さない（Issue #332）
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.old, "old").unwrap();
        fs::write(&paths.new, "new").unwrap();

        assert!(matches!(
            swap_in(&paths),
            Err(ApplyError::ReplaceKeptNothing { .. })
        ));

        assert_eq!(read(&paths.old), "old");
        assert_eq!(read(&paths.new), "new");
        assert!(!paths.exe.exists());
    }

    #[test]
    fn swap_in_can_move_a_running_exe() {
        // Windows は実行中の exe を消せないが、改名はできる。差し替えはこれに頼る。
        // 実行中のものとして、ping.exe の複製を動かしておく（テストの実行ファイル
        // そのものは使わない）
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot がある");
        let ping = Path::new(&system_root).join("System32").join("PING.EXE");
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::copy(&ping, &paths.exe).expect("ping.exe を複製できる");
        fs::write(&paths.new, "new").unwrap();
        let mut child = std::process::Command::new(&paths.exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("複製した ping.exe を起動できる");
        // 動いている間は消せないことを先に確かめておく（前提の確認）
        assert!(fs::remove_file(&paths.exe).is_err());

        let result = swap_in(&paths);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(result, Ok(()));
        assert_eq!(read(&paths.exe), "new");
        assert!(paths.old.exists());
    }

    #[test]
    fn roll_back_puts_the_old_exe_back() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "new").unwrap();
        fs::write(&paths.old, "old").unwrap();

        roll_back(&paths).expect("戻せる");

        assert_eq!(read(&paths.exe), "old");
        assert!(!paths.old.exists());
        assert!(!paths.new.exists());
    }

    #[test]
    fn roll_back_without_old_keeps_the_new_exe_in_place() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "new").unwrap();

        assert!(roll_back(&paths).is_err());

        // 元の名前に exe が 1 つは残っている
        assert_eq!(read(&paths.exe), "new");
    }

    #[test]
    fn remove_leftovers_removes_old_and_new_and_ignores_missing() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        fs::write(&paths.exe, "exe").unwrap();
        fs::write(&paths.old, "old").unwrap();
        fs::write(&paths.new, "new").unwrap();

        remove_leftovers(&paths).expect("消せる");
        remove_leftovers(&paths).expect("無くても失敗にしない");

        assert!(paths.exe.exists());
        assert!(!paths.old.exists());
        assert!(!paths.new.exists());
    }

    #[test]
    fn remove_leftovers_when_started_from_old_keeps_itself() {
        // 案内に従わず .old のまま起動した場合、一時名は `.old.old` / `.old.new` になり、
        // 起動した .old 自身と隣の .new には触らない（Issue #332）
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let original = dummy_paths(dir.path());
        fs::write(&original.old, "old").unwrap();
        fs::write(&original.new, "new").unwrap();
        let paths = ExePaths::for_exe(original.old.clone()).expect("名前がある");

        remove_leftovers(&paths).expect("消すものが無くても失敗にしない");

        assert_eq!(read(&original.old), "old");
        assert_eq!(read(&original.new), "new");
    }

    #[test]
    fn ensure_writable_missing_dir_is_an_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");

        ensure_writable(dir.path()).expect("一時ディレクトリには書ける");
        assert!(ensure_writable(&dir.path().join("missing")).is_err());
        // 確かめたあとに何も残さない
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
