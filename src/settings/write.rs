//! 設定ファイルの書き込み。同じフォルダの一時ファイルへ書いて rename で置き換える
//! （`replace_atomically`）と、その一時ファイルの名前（`docs/design/settings.md`）。

use super::store::serialize_settings;
use super::{AppSettings, SettingsError};
use std::path::{Path, PathBuf};

// 保存で使う一時ファイルのパス。同じフォルダの `<元のファイル名>.<乱数>.tmp`。
//
// 同じフォルダに置くのは、rename が同じボリュームの中でだけ置き換えとして
// 働くため。別のフォルダ（%TEMP% など）に置くとボリュームをまたぎうる。
// 名前に乱数を入れるのは、もともと同じ名前の `.tmp` があっても上書きしたり
// 消したりしないため（Issue #369）。書き出しは任意のフォルダへ書く。
fn temp_path_for(path: &Path, token: u64) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    path.with_file_name(format!("{}.{:016x}.tmp", file_name, token))
}

// 一時ファイルの名前に入れる乱数。クレートを増やさないため、`RandomState`
// （種をプロセスごとに乱数で取る）のハッシュへ時刻・プロセス ID・試行の番号を混ぜる。
fn temp_token(attempt: u32) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.write_u32(attempt);
    hasher.finish()
}

// 名前が衝突したときに別の名前で試す回数
const TEMP_ATTEMPTS: u32 = 8;

// 一時ファイルを排他的に（`create_new`）作り、そのパスと開いたファイルを返す。
// 既にあるファイルは開かないので、ここから返るのは自分が作ったものだけ。
fn create_temp_file(path: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let mut last_error = None;
    for attempt in 0..TEMP_ATTEMPTS {
        let temp_path = temp_path_for(path, temp_token(attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
}

// 設定を一時ファイルへ書き、ディスクへ書き切ってから本来のファイルと置き換える。
//
// 本来のファイルを `truncate` で開いて直接書くと、途中で止まったとき 0 バイトか
// 書きかけのファイルが残る（Issue #317。confy 0.6 の `store_path` がそうだった）。
// 置き換えを rename にすれば、ディスクに残るのは古い内容か新しい内容の
// どちらかになる。Windows の `std::fs::rename` は置き換え先があっても
// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` で差し替える。
pub(super) fn write_atomically(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
    replace_atomically(path, settings).map_err(|source| SettingsError::SaveFailed {
        path: path.to_path_buf(),
        source,
    })
}

// `write_atomically` と `export_to` の本体。失敗の理由だけを返し、どの
// `SettingsError` にするかは呼び出し側が決める。
pub(super) fn replace_atomically(path: &Path, settings: &AppSettings) -> Result<(), String> {
    use std::io::Write;

    // 書けない設定なら一時ファイルを作る前に止める
    let contents = serialize_settings(settings)?;

    // 名前を排他的に確保し、そのとき開いたファイルへ書く。開くのは自分で
    // 作ったファイルだけ
    let (temp_path, mut file) = create_temp_file(path).map_err(|e| e.to_string())?;

    let synced = file
        .write_all(contents.as_bytes())
        // rename の前にディスクへ書き切る。書き切る前に置き換えると、
        // 電源断のあとに中身の無いファイルへ置き換わっていることがある
        .and_then(|()| file.sync_all());
    // 置き換えや削除の前に閉じておく
    drop(file);
    let written = synced
        .map_err(|e| e.to_string())
        .and_then(|()| std::fs::rename(&temp_path, path).map_err(|e| e.to_string()));

    if let Err(source) = written {
        // 置き換えられなかった一時ファイルは残さない。消すのは自分で作った
        // `temp_path` だけ。元のファイルは手付かずのまま。消せなくても残るのは
        // 自分の一時ファイル 1 つだけなので、失敗は捨てる
        let _ = std::fs::remove_file(&temp_path);
        return Err(source);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::store::LoadOutcome;
    use crate::settings::testing::FULL_CONFIG;
    use std::fs;
    use tempfile::tempdir;

    use crate::settings::testing::has_own_temp_file;

    #[test]
    fn temp_path_for_puts_token_before_tmp_in_the_same_folder() {
        // rename を同じボリュームの中で済ませるため、同じフォルダに置く。
        // 乱数は 16 桁の 16 進数で入る
        let path = Path::new(r"C:\config\default-config.toml");

        assert_eq!(
            temp_path_for(path, 0xab),
            PathBuf::from(r"C:\config\default-config.toml.00000000000000ab.tmp")
        );
    }

    #[test]
    fn temp_token_differs_between_attempts() {
        // 衝突したときに試す名前が毎回変わること
        assert_ne!(temp_token(0), temp_token(1));
    }

    #[test]
    fn write_atomically_replaces_the_file_and_leaves_no_temp_file() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "[video]\nfps = 15\n").expect("古い内容を書けること");
        let settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");

        write_atomically(&path, &settings).expect("保存できること");

        assert!(!has_own_temp_file(&path), "一時ファイルが残っている");
        let (reloaded, outcome) = AppSettings::load_from(&path);
        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(reloaded.video.device_name, settings.video.device_name);
        assert_eq!(reloaded.video.fps, settings.video.fps);
        assert_eq!(reloaded.hotkeys, settings.hotkeys);
    }

    #[test]
    fn write_atomically_creates_a_missing_file() {
        // 初回起動のようにファイルがまだ無い場合も書けること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        write_atomically(&path, &AppSettings::default()).expect("保存できること");

        assert!(path.exists());
        assert!(!has_own_temp_file(&path));
    }

    #[test]
    fn write_atomically_failure_keeps_the_original_and_removes_the_temp_file() {
        // 置き換え先がディレクトリで rename できない場合。元の場所は手付かずで、
        // 一時ファイルも残らないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::create_dir(&path).expect("ディレクトリを作れること");

        let err = write_atomically(&path, &AppSettings::default()).expect_err("失敗すること");

        assert!(
            matches!(&err, SettingsError::SaveFailed { path: p, .. } if p == &path),
            "保存の失敗として返ること: {err:?}"
        );
        assert!(path.is_dir(), "元の場所が変わっている");
        assert!(!has_own_temp_file(&path), "一時ファイルが残っている");
    }

    #[test]
    fn write_atomically_read_only_target_keeps_the_original() {
        // 置き換え先が読み取り専用の場合。Windows の rename は読み取り専用の
        // ファイルを置き換えられないので失敗し、元の内容が残ること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "[video]\nfps = 15\n").expect("古い内容を書けること");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions.clone()).unwrap();

        let result = write_atomically(&path, &AppSettings::default());

        // 後片付けのために読み取り専用を外しておく
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
        if cfg!(windows) {
            assert!(result.is_err(), "読み取り専用のファイルを置き換えた");
            assert_eq!(fs::read_to_string(&path).unwrap(), "[video]\nfps = 15\n");
        }
        assert!(!has_own_temp_file(&path), "一時ファイルが残っている");
    }
}
