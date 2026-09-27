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

/// 差し替えが途中で失敗したときの戻し方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// `.old` へ動かした元の exe を元の名前へ戻す
    RestoreOld,
    /// ダウンロードした `.new` を消す
    RemoveNew,
}

/// `failed` の手順で失敗したとき、何をどの順で戻すか。
///
/// 元の exe が元の名前から離れるのは `MoveCurrentToOld` が成功したあとだけ。
/// それより前の失敗では元の exe はそのままなので、`.new` を消すだけでよい。
/// `MoveNewToCurrent` の失敗では元の名前が空いているので、先に `.old` を戻す。
/// **戻すより先に `.new` を消さない。** 戻せなかったときに、手で置ける exe が
/// 1 つも無くなるため。
pub fn recovery_for(failed: SwapStep) -> &'static [Recovery] {
    match failed {
        SwapStep::RemoveStaleOld | SwapStep::MoveCurrentToOld => &[Recovery::RemoveNew],
        SwapStep::MoveNewToCurrent => &[Recovery::RestoreOld, Recovery::RemoveNew],
    }
}

/// 照合の済んだ `.new` を元の名前へ置く。元の exe は `.old` に退避する。
///
/// 途中で失敗したら `recovery_for` の手順で戻し、`ApplyError::Replace` を返す。
pub fn swap_in(paths: &ExePaths) -> Result<(), ApplyError> {
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
            recover(paths, step);
            return Err(ApplyError::Replace(e.to_string()));
        }
    }
    info!(
        "exe を差し替えた: {}（元の exe は {}）",
        paths.exe.display(),
        paths.old.display()
    );
    Ok(())
}

fn recover(paths: &ExePaths, failed: SwapStep) {
    for recovery in recovery_for(failed) {
        match recovery {
            Recovery::RestoreOld => match fs::rename(&paths.old, &paths.exe) {
                Ok(()) => info!("元の exe を戻した: {}", paths.exe.display()),
                Err(e) => warn!(
                    "元の exe を戻せない（{} に残っている）: {}",
                    paths.old.display(),
                    e
                ),
            },
            Recovery::RemoveNew => remove_if_exists(&paths.new),
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

    // ---- 差し替えの戻し方 ----

    #[test]
    fn recovery_for_failures_before_moving_the_exe_only_removes_new() {
        assert_eq!(
            recovery_for(SwapStep::RemoveStaleOld),
            &[Recovery::RemoveNew]
        );
        assert_eq!(
            recovery_for(SwapStep::MoveCurrentToOld),
            &[Recovery::RemoveNew]
        );
    }

    #[test]
    fn recovery_for_failure_after_moving_the_exe_restores_it_first() {
        // 元の名前が空いているので、.new を消す前に .old を戻す
        assert_eq!(
            recovery_for(SwapStep::MoveNewToCurrent),
            &[Recovery::RestoreOld, Recovery::RemoveNew]
        );
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
    fn swap_in_failing_to_move_exe_removes_new() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(dir.path());
        // 元の exe が無いので退避できない
        fs::write(&paths.new, "new").unwrap();

        assert!(matches!(swap_in(&paths), Err(ApplyError::Replace(_))));

        assert!(!paths.new.exists());
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
    fn ensure_writable_missing_dir_is_an_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");

        ensure_writable(dir.path()).expect("一時ディレクトリには書ける");
        assert!(ensure_writable(&dir.path().join("missing")).is_err());
        // 確かめたあとに何も残さない
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
