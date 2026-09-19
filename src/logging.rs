//! ログの初期化とファイルへの書き出し。
//!
//! `#![windows_subsystem = "windows"]` のためコンソールが存在せず、
//! 標準出力へ書いても誰にも届かない。`log` クレートのファサード経由で
//! ファイルへ残し、不具合報告時にユーザーが添付できる状態にする。
//!
//! ローテーションは「1 回の起動 = 1 ファイル」とし、起動時に古い世代を
//! 削除する。日付でローテートするより、どの起動で何が起きたかを
//! 切り分けやすい。

use chrono::{DateTime, Local};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// ログレベルを上書きする環境変数。
///
/// 設定ファイルではなく環境変数にしてあるのは、`AppSettings` に項目を足さずに
/// 済ませるため。ログを見たいのは不具合の調査時に限られ、そのときだけ
/// `set CAPTURECARD_VIEWER_LOG=debug` して起動すればよい。
pub const LOG_LEVEL_ENV: &str = "CAPTURECARD_VIEWER_LOG";

/// 環境変数の指定が無い、または解釈できないときのログレベル
const DEFAULT_LEVEL: LevelFilter = LevelFilter::Info;

/// 設定ファイルと同じデータディレクトリの下に作る、ログ用ディレクトリの名前
const LOG_DIR_NAME: &str = "logs";

/// ログファイル名の前半。この前後の形に合うものだけを世代管理の対象にする
const FILE_PREFIX: &str = "capturecard_viewer-";
/// ログファイル名の後半
const FILE_SUFFIX: &str = ".log";

/// ファイル名に入れる日時の書式。辞書順が時刻順と一致する形にしてあるので、
/// 名前を並べ替えるだけで古い順が決まる
const FILE_TIMESTAMP_FORMAT: &str = "%Y%m%d-%H%M%S";

/// ログ 1 行の先頭に入れる時刻の書式
const LINE_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// 残すログファイルの数。今回の起動で作るものを含むので、直近 10 回分の起動が残る
const MAX_LOG_FILES: usize = 10;

/// 同じ秒に起動が重なったときに、名前の連番を進めて試す回数
const MAX_NAME_COLLISIONS: u32 = 9;

/// ログ基盤を初期化し、書き出し先のパスを返す。
///
/// 失敗しても panic しない。ログが無くてもアプリ自体は動くため、
/// 呼び出し側は `Err` を受けてもそのまま起動を続けてよい。
pub fn init() -> Result<PathBuf, String> {
    let level = level_from_env(std::env::var(LOG_LEVEL_ENV).ok().as_deref());

    let dir = log_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|e| format!("ログディレクトリ {} を作成できない: {}", dir.display(), e))?;

    // これから 1 つ作るので、既存は 1 つ少なく残す。
    // 削除できなかったものはロガーの登録後に警告として書き出す
    let undeleted = remove_obsolete_logs(&dir, MAX_LOG_FILES.saturating_sub(1));

    let (path, file) = create_log_file(&dir, Local::now())?;

    let logger = FileLogger {
        level,
        file: Mutex::new(file),
    };
    log::set_boxed_logger(Box::new(logger)).map_err(|e| format!("ロガーを登録できない: {}", e))?;
    log::set_max_level(level);

    log::info!(
        "capturecard_viewer {} を起動した（ログレベル: {}、出力先: {}）",
        env!("CARGO_PKG_VERSION"),
        level,
        path.display()
    );
    for name in undeleted {
        log::warn!("古いログファイル {} を削除できなかった", name);
    }

    Ok(path)
}

/// ログの出力先ディレクトリを決める。
///
/// 設定ファイルと同じデータディレクトリの下に置く。場所を confy から引いて
/// いるのは、設定の置き場所と食い違わせないため。ユーザーに「設定ファイルの
/// 隣」と案内できる状態を保つ。
fn log_dir() -> Result<PathBuf, String> {
    let config_path = confy::get_configuration_file_path(crate::settings::APP_NAME, None)
        .map_err(|e| format!("設定ファイルのパスを取得できない: {}", e))?;

    // 設定ファイルは <データディレクトリ>\config\default-config.toml に置かれる。
    // 2 つ上がデータディレクトリなので、その下に logs を作る
    let data_dir = config_path.parent().and_then(Path::parent).ok_or_else(|| {
        format!(
            "設定ファイルのパス {} から親ディレクトリを取れない",
            config_path.display()
        )
    })?;

    Ok(data_dir.join(LOG_DIR_NAME))
}

/// 環境変数の値をログレベルに変換する。
///
/// 解釈できない値は既定へ倒す。指定を打ち間違えてログが一切出なくなるほうが困るため。
fn level_from_env(value: Option<&str>) -> LevelFilter {
    let Some(value) = value else {
        return DEFAULT_LEVEL;
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => DEFAULT_LEVEL,
    }
}

/// この起動で使うログファイルの名前を決める。
///
/// `collision` は同じ秒に起動が重なったときの連番。0 のときは付けない。
/// 区切りに `_` を使っているのは、`.log` の `.`（0x2E）より大きい文字なら
/// 連番付きの名前が元の名前より後ろに並び、辞書順と時刻順の一致が崩れないため。
fn log_file_name(now: DateTime<Local>, collision: u32) -> String {
    let suffix = if collision == 0 {
        String::new()
    } else {
        format!("_{}", collision)
    };

    format!(
        "{}{}{}{}",
        FILE_PREFIX,
        now.format(FILE_TIMESTAMP_FORMAT),
        suffix,
        FILE_SUFFIX
    )
}

/// この起動用のログファイルを作り、パスと一緒に返す。
///
/// `create_new` で開くので、既にあるファイルは掴まない。同じ秒に 2 つ起動しても
/// 互いのログが 1 つのファイルに混ざらず、「1 回の起動 = 1 ファイル」を保てる。
/// 衝突したら連番を進めて作り直す。
fn create_log_file(dir: &Path, now: DateTime<Local>) -> Result<(PathBuf, File), String> {
    for collision in 0..=MAX_NAME_COLLISIONS {
        let path = dir.join(log_file_name(now, collision));
        match OpenOptions::new().create_new(true).append(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            // 同じ秒に起動した別のプロセスが先に作っている。次の連番を試す
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "ログファイル {} を作成できない: {}",
                    path.display(),
                    e
                ))
            }
        }
    }

    Err(format!(
        "ログファイルを作成できない。{} に同じ時刻の名前が {} 個ある",
        dir.display(),
        MAX_NAME_COLLISIONS + 1
    ))
}

/// 削除する対象のファイル名を、古い順に選ぶ。
///
/// `keep` は残す数。ファイル名の日時部分は辞書順が時刻順と一致するため、
/// 名前を昇順に並べれば先頭が最も古い。決まった形に合う名前だけを対象にして、
/// ユーザーが同じディレクトリに置いた別のファイルを消さない。
fn obsolete_log_files(file_names: &[String], keep: usize) -> Vec<String> {
    let mut logs: Vec<&String> = file_names
        .iter()
        .filter(|name| name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX))
        .collect();
    logs.sort();

    let remove_count = logs.len().saturating_sub(keep);
    logs.into_iter().take(remove_count).cloned().collect()
}

/// 古い世代のログファイルを削除し、削除できなかったものの名前を返す。
///
/// ロガーの登録前に呼ばれるので、ここでは失敗を記録できない。
/// 呼び出し側が登録後に書き出す。
fn remove_obsolete_logs(dir: &Path, keep: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        // ディレクトリを読めないなら削除対象も分からない。
        // この後のファイル作成で失敗して理由が返るので、ここでは何もしない
        return Vec::new();
    };

    let file_names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    obsolete_log_files(&file_names, keep)
        .into_iter()
        .filter(|name| fs::remove_file(dir.join(name)).is_err())
        .collect()
}

/// ログ 1 行を組み立てる。末尾の改行まで含む
fn format_line(timestamp: &str, level: Level, target: &str, message: &str) -> String {
    format!("{} [{:<5}] {} - {}\n", timestamp, level, target, message)
}

/// ファイルへ書き出すロガー。
///
/// 1 行ごとにフラッシュする。`[profile.release]` の `panic = "abort"` により
/// 異常終了時はバッファの中身が捨てられるため、落ちる直前のログこそ残したい
/// この用途ではバッファリングが裏目に出る。
struct FileLogger {
    level: LevelFilter,
    file: Mutex<File>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = format_line(
            &Local::now().format(LINE_TIMESTAMP_FORMAT).to_string(),
            record.level(),
            record.target(),
            &record.args().to_string(),
        );

        // 書き込みの失敗は伝える先が無い。ここが唯一の出力先で、コンソールも無い。
        // ログを出そうとしてアプリが落ちるほうが害が大きいので、失敗は無視する。
        // ロックが毒されていても以降のログを捨てないよう、中身を取り出して使う
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }

    fn flush(&self) {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn level_from_env_none_returns_default() {
        assert_eq!(level_from_env(None), LevelFilter::Info);
    }

    #[test]
    fn level_from_env_known_values_return_matching_level() {
        assert_eq!(level_from_env(Some("error")), LevelFilter::Error);
        assert_eq!(level_from_env(Some("warn")), LevelFilter::Warn);
        assert_eq!(level_from_env(Some("info")), LevelFilter::Info);
        assert_eq!(level_from_env(Some("debug")), LevelFilter::Debug);
        assert_eq!(level_from_env(Some("trace")), LevelFilter::Trace);
    }

    #[test]
    fn level_from_env_ignores_case_and_surrounding_spaces() {
        assert_eq!(level_from_env(Some("  TRACE ")), LevelFilter::Trace);
        assert_eq!(level_from_env(Some("Warn")), LevelFilter::Warn);
    }

    #[test]
    fn level_from_env_unknown_value_returns_default() {
        // 打ち間違いでログが止まると調査できなくなるため、既定へ倒す
        assert_eq!(level_from_env(Some("verbose")), LevelFilter::Info);
        assert_eq!(level_from_env(Some("")), LevelFilter::Info);
        assert_eq!(level_from_env(Some("off")), LevelFilter::Info);
    }

    #[test]
    fn log_file_name_uses_sortable_timestamp() {
        let now = Local.with_ymd_and_hms(2026, 9, 19, 1, 2, 3).unwrap();
        assert_eq!(
            log_file_name(now, 0),
            "capturecard_viewer-20260919-010203.log"
        );
    }

    #[test]
    fn log_file_name_with_collision_sorts_after_the_original() {
        let now = Local.with_ymd_and_hms(2026, 9, 19, 1, 2, 3).unwrap();
        let first = log_file_name(now, 0);
        let second = log_file_name(now, 1);

        assert_eq!(second, "capturecard_viewer-20260919-010203_1.log");
        // 世代の削除は名前の昇順に頼っているので、後から作ったほうが後ろに来ること
        assert!(first < second, "連番付きの名前が元の名前より後ろに並ぶ");
    }

    #[test]
    fn create_log_file_same_second_creates_separate_files() {
        // 同じ秒に 2 つ起動しても、互いのログが 1 つのファイルに混ざらないこと
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let now = Local.with_ymd_and_hms(2026, 9, 19, 1, 2, 3).unwrap();

        let (first_path, _first) = create_log_file(dir.path(), now).expect("1 つ目を作れる");
        let (second_path, _second) = create_log_file(dir.path(), now).expect("2 つ目を作れる");

        assert_eq!(
            first_path,
            dir.path().join("capturecard_viewer-20260919-010203.log")
        );
        assert_eq!(
            second_path,
            dir.path().join("capturecard_viewer-20260919-010203_1.log")
        );
        assert!(first_path.exists());
        assert!(second_path.exists());
    }

    #[test]
    fn create_log_file_existing_file_is_not_truncated() {
        // 既にあるファイルを掴まないこと。掴むと先に起動したほうのログが混ざる
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let now = Local.with_ymd_and_hms(2026, 9, 19, 1, 2, 3).unwrap();
        let existing = dir.path().join("capturecard_viewer-20260919-010203.log");
        fs::write(&existing, b"before").expect("テスト用のファイルを作れる");

        let (path, _file) = create_log_file(dir.path(), now).expect("別名で作れる");

        assert_ne!(path, existing);
        assert_eq!(fs::read(&existing).expect("読める"), b"before");
    }

    #[test]
    fn create_log_file_all_names_taken_returns_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let now = Local.with_ymd_and_hms(2026, 9, 19, 1, 2, 3).unwrap();
        for collision in 0..=MAX_NAME_COLLISIONS {
            fs::write(dir.path().join(log_file_name(now, collision)), b"x")
                .expect("テスト用のファイルを作れる");
        }

        assert!(create_log_file(dir.path(), now).is_err());
    }

    #[test]
    fn obsolete_log_files_keeps_all_when_below_limit() {
        let names = vec![
            "capturecard_viewer-20260919-010203.log".to_string(),
            "capturecard_viewer-20260919-010204.log".to_string(),
        ];
        assert!(obsolete_log_files(&names, 3).is_empty());
        // ちょうど上限のときも消さない
        assert!(obsolete_log_files(&names, 2).is_empty());
    }

    #[test]
    fn obsolete_log_files_removes_oldest_first() {
        let names = vec![
            "capturecard_viewer-20260919-010205.log".to_string(),
            "capturecard_viewer-20260918-235959.log".to_string(),
            "capturecard_viewer-20260919-010203.log".to_string(),
            "capturecard_viewer-20261001-000000.log".to_string(),
        ];
        assert_eq!(
            obsolete_log_files(&names, 2),
            vec![
                "capturecard_viewer-20260918-235959.log".to_string(),
                "capturecard_viewer-20260919-010203.log".to_string(),
            ]
        );
    }

    #[test]
    fn obsolete_log_files_keep_zero_removes_every_log() {
        let names = vec!["capturecard_viewer-20260919-010203.log".to_string()];
        assert_eq!(obsolete_log_files(&names, 0), names);
    }

    #[test]
    fn obsolete_log_files_ignores_unrelated_files() {
        // ログ以外のファイルを巻き込んで消さないこと
        let names = vec![
            "default-config.toml".to_string(),
            "capturecard_viewer.log".to_string(),
            "capturecard_viewer-20260919-010203.log.bak".to_string(),
            "capturecard_viewer-20260919-010203.log".to_string(),
        ];
        assert!(obsolete_log_files(&names, 1).is_empty());
        assert_eq!(
            obsolete_log_files(&names, 0),
            vec!["capturecard_viewer-20260919-010203.log".to_string()]
        );
    }

    #[test]
    fn format_line_contains_timestamp_level_target_and_message() {
        assert_eq!(
            format_line(
                "2026-09-19 01:02:03.456",
                Level::Warn,
                "audio",
                "音が出ない"
            ),
            "2026-09-19 01:02:03.456 [WARN ] audio - 音が出ない\n"
        );
    }

    #[test]
    fn remove_obsolete_logs_deletes_only_excess_logs() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let keep_me = dir.path().join("capturecard_viewer-20260919-010204.log");
        let delete_me = dir.path().join("capturecard_viewer-20260919-010203.log");
        let unrelated = dir.path().join("memo.txt");
        for path in [&keep_me, &delete_me, &unrelated] {
            fs::write(path, b"x").expect("テスト用のファイルを作れる");
        }

        assert!(remove_obsolete_logs(dir.path(), 1).is_empty());

        assert!(keep_me.exists(), "新しいログは残る");
        assert!(!delete_me.exists(), "古いログは消える");
        assert!(unrelated.exists(), "ログ以外は消さない");
    }

    #[test]
    fn remove_obsolete_logs_on_missing_directory_returns_empty() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let missing = dir.path().join("not-exist");
        assert!(remove_obsolete_logs(&missing, 0).is_empty());
    }
}
