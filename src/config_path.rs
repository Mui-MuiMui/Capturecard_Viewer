//! 設定ファイルとログの置き場所。
//!
//! 既定は confy が決める `%AppData%\capturecard_viewer\config\default-config.toml` と、
//! その 2 つ上のデータディレクトリの下の `logs`。環境変数
//! `CAPTURECARD_VIEWER_CONFIG_DIR` にフォルダを指定すると、設定ファイルとログを
//! そのフォルダへ置く（Issue #290）。
//!
//! 開発者向けのもの。複数のエージェントがそれぞれアプリを起動すると、全員が
//! 同じ `%AppData%` の設定ファイルを読み書きして取り合うため、起動ごとに
//! 別のフォルダを指せるようにしてある。フェイクデバイス
//! （`CAPTURECARD_VIEWER_FAKE_DEVICES`）と同じく起動するときだけ指定し、
//! 設定ファイルには保存しない。
//!
//! 環境変数を読むのは起動後の最初の問い合わせの 1 回だけで、結果を覚えておく。
//! 設定とログで置き場所が食い違わないようにするため。解釈は純粋関数
//! （`parse_config_dir` / `resolve`）にしてある。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 設定ファイルとログを置くフォルダを差し替える環境変数。絶対パスだけを受け付ける。
pub const CONFIG_DIR_ENV: &str = "CAPTURECARD_VIEWER_CONFIG_DIR";

/// 差し替えたフォルダに置く設定ファイルの名前。confy の既定と揃えてある
const CONFIG_FILE_NAME: &str = "default-config.toml";

/// 実際に使う置き場所。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLocation {
    /// 設定ファイルのパス
    pub config_file: PathBuf,
    /// ログのフォルダを作る基準のフォルダ
    pub data_dir: PathBuf,
    /// 環境変数で差し替えているか
    pub overridden: bool,
    /// 環境変数を使わなかった理由。ロガーを登録する前に決まるので、ここに
    /// 持っておいてロガーの登録後に WARN で書き出す
    pub notice: Option<String>,
}

/// 環境変数の値の解釈。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirOverride {
    /// 指定していない（空や空白だけも含む）
    Unset,
    /// 絶対パス。このフォルダを使う
    Absolute(PathBuf),
    /// 相対パス。使わない
    Relative(String),
}

static LOCATION: OnceLock<Result<ConfigLocation, String>> = OnceLock::new();

/// 設定ファイルとログの置き場所。**最初の呼び出しで環境変数を読んで決め、以後は同じ値を返す。**
pub fn location() -> Result<&'static ConfigLocation, String> {
    LOCATION
        .get_or_init(|| {
            let value = std::env::var(CONFIG_DIR_ENV).ok();
            resolve(value.as_deref(), default_config_file)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// 設定ファイルのパス。
pub fn config_file_path() -> Result<PathBuf, String> {
    location().map(|location| location.config_file.clone())
}

/// confy が決める既定の設定ファイルのパス。
fn default_config_file() -> Result<PathBuf, String> {
    confy::get_configuration_file_path(crate::settings::APP_NAME, None)
        .map_err(|e| format!("設定ファイルのパスを取得できない: {}", e))
}

/// 環境変数の値と既定の設定ファイルのパスから、使う置き場所を決める。
///
/// 相対パスは使わない。基準をカレントディレクトリにすると起動の仕方で置き場所が
/// 変わり、exe の置き場所にすると worktree ごとに別のフォルダを指したい用途に
/// 合わないため（`docs/design/assets.md`）。フォルダを作れない場合も使わない。
/// どちらも既定の置き場所へ倒し、理由を `notice` に残す。
fn resolve(
    value: Option<&str>,
    default_config_file: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<ConfigLocation, String> {
    let notice = match parse_config_dir(value) {
        DirOverride::Unset => None,
        DirOverride::Absolute(dir) => match prepare_dir(&dir) {
            Ok(()) => {
                return Ok(ConfigLocation {
                    config_file: dir.join(CONFIG_FILE_NAME),
                    data_dir: dir,
                    overridden: true,
                    notice: None,
                })
            }
            Err(e) => Some(format!(
                "{} のフォルダ {} を使えないので既定の置き場所を使う: {}",
                CONFIG_DIR_ENV,
                dir.display(),
                e
            )),
        },
        DirOverride::Relative(value) => Some(format!(
            "{} の値 '{}' は相対パスなので使わない（既定の置き場所を使う）",
            CONFIG_DIR_ENV, value
        )),
    };

    let config_file = default_config_file()?;
    // 設定ファイルは <データディレクトリ>\config\default-config.toml に置かれる。
    // 2 つ上がデータディレクトリ
    let data_dir = config_file
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "設定ファイルのパス {} から親ディレクトリを取れない",
                config_file.display()
            )
        })?;
    Ok(ConfigLocation {
        config_file,
        data_dir,
        overridden: false,
        notice,
    })
}

/// 環境変数の値を解釈する。空や空白だけの値は指定していないのと同じ。
fn parse_config_dir(value: Option<&str>) -> DirOverride {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return DirOverride::Unset;
    };
    let path = PathBuf::from(value);
    if path.is_absolute() {
        DirOverride::Absolute(path)
    } else {
        DirOverride::Relative(value.to_string())
    }
}

/// フォルダが無ければ作る。ファイルがあるなどでフォルダとして使えなければ `Err`。
fn prepare_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if dir.is_dir() {
        Ok(())
    } else {
        Err("フォルダではない".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_file() -> Result<PathBuf, String> {
        Ok(PathBuf::from(
            "C:\\data\\capturecard_viewer\\config\\default-config.toml",
        ))
    }

    #[test]
    fn parse_config_dir_none_is_unset() {
        assert_eq!(parse_config_dir(None), DirOverride::Unset);
    }

    #[test]
    fn parse_config_dir_empty_or_blank_is_unset() {
        assert_eq!(parse_config_dir(Some("")), DirOverride::Unset);
        assert_eq!(parse_config_dir(Some("   ")), DirOverride::Unset);
    }

    #[test]
    fn parse_config_dir_absolute_path_is_used() {
        let dir = std::env::temp_dir().join("cv-config");
        let value = dir.to_string_lossy().into_owned();
        assert_eq!(parse_config_dir(Some(&value)), DirOverride::Absolute(dir));
    }

    #[test]
    fn parse_config_dir_relative_path_is_rejected() {
        assert_eq!(
            parse_config_dir(Some(".agent-config")),
            DirOverride::Relative(".agent-config".to_string())
        );
        assert_eq!(
            parse_config_dir(Some(" sub\\dir ")),
            DirOverride::Relative("sub\\dir".to_string())
        );
    }

    #[test]
    fn resolve_unset_uses_default_location() {
        let location = resolve(None, default_file).unwrap();
        assert_eq!(location.config_file, default_file().unwrap());
        assert_eq!(
            location.data_dir,
            PathBuf::from("C:\\data\\capturecard_viewer")
        );
        assert!(!location.overridden);
        assert_eq!(location.notice, None);
    }

    #[test]
    fn resolve_absolute_path_creates_the_directory_and_uses_it() {
        let root = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let dir = root.path().join("nested").join("config");
        let value = dir.to_string_lossy().into_owned();

        let location = resolve(Some(&value), || panic!("既定の置き場所を引かない")).unwrap();

        assert!(dir.is_dir());
        assert_eq!(location.config_file, dir.join("default-config.toml"));
        assert_eq!(location.data_dir, dir);
        assert!(location.overridden);
        assert_eq!(location.notice, None);
    }

    #[test]
    fn resolve_relative_path_falls_back_with_notice() {
        let location = resolve(Some(".agent-config"), default_file).unwrap();
        assert_eq!(location.config_file, default_file().unwrap());
        assert!(!location.overridden);
        assert!(location.notice.unwrap().contains(".agent-config"));
    }

    #[test]
    fn resolve_unusable_directory_falls_back_with_notice() {
        let root = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let value = file.to_string_lossy().into_owned();

        let location = resolve(Some(&value), default_file).unwrap();

        assert_eq!(location.config_file, default_file().unwrap());
        assert!(!location.overridden);
        assert!(location.notice.is_some());
    }

    #[test]
    fn resolve_default_error_is_returned() {
        let result = resolve(None, || Err("取れない".to_string()));
        assert_eq!(result, Err("取れない".to_string()));
    }
}
