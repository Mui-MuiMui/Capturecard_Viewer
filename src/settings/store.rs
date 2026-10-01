//! 設定ファイルの読み書き。起動時の読み込み（`AppSettings::load`）と保存、
//! 読めなかったファイルの退避、自動保存を許すかの判断（`AutoSavePolicy`）、
//! 設定の書き出しと読み込み（`docs/design/settings.md`）。
//!
//! 置き場所の決定は `crate::config_path` が持ち、ここは呼ぶだけ。

use super::{AppSettings, SettingsError, APP_NAME};
use chrono::Datelike;
use log::{error, warn};
use std::path::{Path, PathBuf};

// 設定ファイルをどう読めたか。起動時に既定値を書き戻してよいかの判断に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    // 読み込めた。初回起動で confy が既定値のファイルを作った場合も含む
    Loaded,
    // 読み込めなかったので既定値で起動した。読めなかったファイルは退避済みか、
    // そもそも存在しなかった。どちらもディスクに壊れたファイルは残っていない
    FellBackToDefaults,
    // 読み込めず、退避もできなかった。読めなかったファイルがそのまま残っている
    BrokenFileLeftBehind,
}

impl LoadOutcome {
    // 起動時に既定値を設定ファイルへ書き戻してよいか。
    //
    // 退避できなかった場合だけ false になる。読めなかったファイルがディスクに
    // 残っているため、ここで書き戻すとユーザーが設定を取り戻す最後の手段が消える。
    // 書き戻さなければ壊れたファイルは手元に残り、次回以降も退避を試みられる。
    pub fn may_write_defaults_on_startup(self) -> bool {
        !matches!(self, LoadOutcome::BrokenFileLeftBehind)
    }
}

// 設定の自動保存（デバウンス保存と終了時保存）を許してよいかを持つ。
//
// 読めなかった設定ファイルを退避できなかった場合、ディスクには壊れたファイルが
// そのまま残っている。起動時の書き戻しだけを止めても、ウィンドウを動かせば
// 2 秒後のデバウンス保存が、何もしなくても終了時の保存が、同じファイルを
// 既定値で上書きしてしまう。そのため壊れたファイルが残っている間は
// 自動保存そのものを止める。
//
// 止めている間はウィンドウの位置・サイズや音量も永続化されない。設定を
// 取り戻す手段を残すほうを優先する、という判断。
//
// 設定ダイアログの「適用」「OK」による保存はユーザーの明示的な操作なので
// 止めない。それが成功した時点で壊れたファイルはユーザーの意思で置き換わって
// いるため、以降の自動保存も解禁する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSavePolicy {
    allowed: bool,
}

impl AutoSavePolicy {
    // 設定の読み込み結果から初期状態を決める。
    pub fn from_load_outcome(outcome: LoadOutcome) -> Self {
        Self {
            allowed: !matches!(outcome, LoadOutcome::BrokenFileLeftBehind),
        }
    }

    // 自動保存してよいか。
    pub fn is_allowed(self) -> bool {
        self.allowed
    }

    // 明示的な保存操作の結果を反映する。`saved` は実際に書き出せたか。
    //
    // 失敗した場合に解禁しないのは、壊れたファイルがまだ残っているため。
    // 解禁してしまうと、次のウィンドウ操作で自動保存が走って上書きしうる。
    pub fn note_explicit_save(&mut self, saved: bool) {
        if saved {
            self.allowed = true;
        }
    }
}

// 退避の結果から読み込み結果を決める。
//
// load() 自体は設定ファイルの置き場所（既定は %AppData%）を直接読み書きするためテストできない。
// 判断の部分だけをこの関数に切り出して、退避が失敗した場合を含めて検証する。
fn outcome_from_backup(backup: std::io::Result<Option<PathBuf>>) -> LoadOutcome {
    match backup {
        Ok(_) => LoadOutcome::FellBackToDefaults,
        Err(_) => LoadOutcome::BrokenFileLeftBehind,
    }
}

// 読み込めなかった設定ファイルを退避する。
// 退避できた場合は退避先のパスを返す。元のファイルが無い場合は None を返す。
//
// コピーではなく rename にしているのは、退避したあとに既定値が書き戻されて
// 元のファイルが上書きされ、内容が失われるのを避けるため。
fn backup_broken_config(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let backup_path = next_backup_path(path);
    std::fs::rename(path, &backup_path)?;
    Ok(Some(backup_path))
}

// 退避先のパスを決める。<元のファイル名>.bak を基本とし、
// 既に存在する場合は .bak.1、.bak.2 と連番を足して過去の退避を上書きしない。
fn next_backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut candidate = path.with_file_name(format!("{}.bak", file_name));
    let mut counter = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{}.bak.{}", file_name, counter));
        counter += 1;
    }

    candidate
}

// 書き出す設定ファイルの既定のファイル名。
//
// 日付を入れるのは、同じフォルダへ何度も書き出したときに前回のものを
// 黙って上書きしないため。同じ日に 2 度書き出した場合は、保存ダイアログが
// 上書きの確認を出す。
//
// 時刻を入れないのは、不具合報告に添える用途で名前が長くなりすぎるため。
// 日が変わらないうちの 2 度目は、ユーザーが名前を変えればよい。
pub fn export_file_name(date: &impl Datelike) -> String {
    format!(
        "{}-settings-{:04}{:02}{:02}.toml",
        APP_NAME,
        date.year(),
        date.month(),
        date.day()
    )
}

// 設定を、指定した場所へ TOML として書き出す。
//
// **confy の `store_path` をそのまま使う。** 書式を `%AppData%` の設定ファイルと
// 揃えたいためで、ここだけ別の toml 実装で書くと、confy が書式を変えたときに
// 書き出したファイルを読み戻せない組み合わせが生まれる。
//
// 書き方は `save()` と同じく、書き出し先と同じフォルダの一時ファイルへ書いて
// rename で置き換える（Issue #361）。選んだファイルへ直接書くと、書き込み中に
// 止まったとき壊れたファイルが残る。
pub fn export_to(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
    replace_atomically(path, settings).map_err(|source| SettingsError::ExportFailed {
        path: path.to_path_buf(),
        source,
    })
}

// 書き出した設定ファイルを読む。
//
// 読めた場合は `AppSettings` へのパースを通っているので、知らない値は
// 既定へ倒れ、旧版のホットキーも移行済みになっている（`RawAppSettings`）。
//
// **`confy::load_path` はファイルが無いと既定値で新しく作る。** 読み込みの
// つもりで呼んだ結果、選んだ場所に既定値のファイルが増えるのは意図と違うので、
// 先に存在を確かめてから渡す。
//
// 確かめ方に `Path::is_file()` を使わないのは、**実在するのにメタデータを
// 取れない場合も `false` を返す**ため。権限の無いファイルを選んだときに
// 「見つからない」と出すと、置き場所を疑って直しようがなくなる。
pub fn import_from(path: &Path) -> Result<AppSettings, SettingsError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        // ディレクトリやデバイスファイル。confy へ渡しても読めない
        Ok(_) => return Err(SettingsError::NotAFile(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SettingsError::FileNotFound(path.to_path_buf()))
        }
        Err(e) => {
            return Err(SettingsError::ImportFailed {
                path: path.to_path_buf(),
                source: e.to_string(),
            })
        }
    }

    confy::load_path(path).map_err(|e| SettingsError::ImportFailed {
        path: path.to_path_buf(),
        source: e.to_string(),
    })
}

// 設定ファイルが空（0 バイトか空白だけ）か。
//
// 全ての設定構造体に `#[serde(default)]` が付いているため、空の文字列は
// TOML として読めてしまい、全項目が既定値の設定になる。保存の途中で
// 強制終了や電源断が起きて中身が消えたファイルもこの形になるので、
// 読めたことにせず「壊れている」として退避する（Issue #317）。
// ユーザーが意図して空のファイルを置く理由は無い。
fn is_blank_config(contents: &[u8]) -> bool {
    contents.iter().all(|b| b.is_ascii_whitespace())
}

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

// 一時ファイルを排他的に（`create_new`）作り、そのパスを返す。既にある
// ファイルは開かないので、ここから返るのは自分が作ったものだけ。
fn create_temp_file(path: &Path) -> std::io::Result<PathBuf> {
    let mut last_error = None;
    for attempt in 0..TEMP_ATTEMPTS {
        let temp_path = temp_path_for(path, temp_token(attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(_) => return Ok(temp_path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
}

// 設定を一時ファイルへ書き、ディスクへ書き切ってから本来のファイルと置き換える。
//
// confy の `store_path` を本来のファイルへ直接使うと、`truncate` で開いてから
// 書き込むため、途中で止まると 0 バイトか書きかけのファイルが残る（Issue #317）。
// 置き換えを rename にすれば、ディスクに残るのは古い内容か新しい内容の
// どちらかになる。Windows の `std::fs::rename` は置き換え先があっても
// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` で差し替える。
//
// 一時ファイルへの書き込みには confy の `store_path` をそのまま使う。書式を
// 読み込み（confy）に揃えるため。このクレートが
// 直接使う toml と confy が内部で使う toml は版が違う。
fn write_atomically(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
    replace_atomically(path, settings).map_err(|source| SettingsError::SaveFailed {
        path: path.to_path_buf(),
        source,
    })
}

// `write_atomically` と `export_to` の本体。失敗の理由だけを返し、どの
// `SettingsError` にするかは呼び出し側が決める。
fn replace_atomically(path: &Path, settings: &AppSettings) -> Result<(), String> {
    // 名前を排他的に確保してから書く。confy は `truncate` で開き直して書くが、
    // 開くのは自分で作ったファイルだけ
    let temp_path = create_temp_file(path).map_err(|e| e.to_string())?;

    let written = confy::store_path(&temp_path, settings)
        .map_err(|e| e.to_string())
        // rename の前にディスクへ書き切る。書き切る前に置き換えると、
        // 電源断のあとに中身の無いファイルへ置き換わっていることがある
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&temp_path)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())
        })
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

impl AppSettings {
    // 設定と、その読み込み結果を返す。
    //
    // 結果を返しているのは、起動時に既定値を書き戻してよいかを呼び出し側が
    // 判断できるようにするため。退避に失敗したまま書き戻すと、読めなかった
    // ファイルを既定値で上書きしてしまい、証跡ごと消える。
    pub fn load() -> (Self, LoadOutcome) {
        // 置き場所は環境変数 CAPTURECARD_VIEWER_CONFIG_DIR で差し替えられる
        // （crate::config_path）。指定が無ければ confy の既定
        let path = match crate::config_path::config_file_path() {
            Ok(path) => path,
            // 設定ファイルの置き場所が分からず、退避を試みることすらできない。
            // 読めなかったファイルが残っている可能性があるため、
            // 書き戻さない側に倒す。
            Err(e) => {
                error!(
                    "設定ファイルの置き場所が分からないため既定値で起動する: {}",
                    e
                );
                return (Self::default(), LoadOutcome::BrokenFileLeftBehind);
            }
        };
        Self::load_from(&path)
    }

    // 指定したパスの設定ファイルを読む。`load` の本体。
    //
    // パスを引数にしているのは、一時フォルダのファイルでテストするため。
    fn load_from(path: &Path) -> (Self, LoadOutcome) {
        // 読めないファイルはここで判定せず、confy の失敗として扱う
        let blank = std::fs::read(path)
            .map(|contents| is_blank_config(&contents))
            .unwrap_or(false);
        let loaded = if blank {
            Err("設定ファイルが空（0 バイトか空白だけ）".to_string())
        } else {
            confy::load_path(path).map_err(|e| e.to_string())
        };

        match loaded {
            Ok(settings) => (settings, LoadOutcome::Loaded),
            Err(e) => {
                error!("設定ファイルを読み込めないため既定値で起動する: {}", e);

                // 読み込みに失敗した設定ファイルは、既定値で起動する前に退避する。
                // 黙って上書きすると、ユーザーが自分の設定を取り戻す手段が無くなる。
                //
                // 失敗したという事実は LoadOutcome として呼び出し側へ渡し、
                // 理由はログに残す。ここは起動直後で UI がまだ無いため、
                // ユーザーへ伝える手段がログしかない。
                let backup = backup_broken_config(path);
                match &backup {
                    Ok(Some(backup_path)) => warn!(
                        "読み込めなかった設定ファイルを {} へ退避した",
                        backup_path.display()
                    ),
                    // 元のファイルが無い。退避するものが無いだけなので何も言わない
                    Ok(None) => {}
                    Err(e) => error!(
                        "読み込めなかった設定ファイル {} を退避できない: {}",
                        path.display(),
                        e
                    ),
                }
                (Self::default(), outcome_from_backup(backup))
            }
        }
    }

    // 設定ファイルへ保存する。
    //
    // 結果を捨てないのは、デバウンスして書き出す側が失敗を検知して
    // 再試行できるようにするため。失敗を握り潰すと、書けなかった変更が
    // 保存済みとして扱われて消える。
    //
    // **ここではログを出さない。** 失敗が続く間は同じ理由が繰り返し届くため、
    // 何を・いつログとトーストに出すかは呼び出し側（`app::settings_store`）が
    // 失敗の続き具合を見て決める。
    pub fn save(&self) -> Result<(), SettingsError> {
        let path =
            crate::config_path::config_file_path().map_err(SettingsError::LocationUnavailable)?;
        write_atomically(&path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::testing::{FULL_CONFIG, LEGACY_CONFIG};
    use crate::settings::{ColorSpace, ScreenshotFormat, MAX_JPEG_QUALITY};
    use chrono::NaiveDate;
    use std::fs;
    use tempfile::tempdir;

    // `path` の隣に、この書式（`<名前>.<16 桁の 16 進数>.tmp`）の一時ファイルが残っているか
    fn has_own_temp_file(path: &Path) -> bool {
        let name = format!("{}.", path.file_name().unwrap().to_string_lossy());
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                let entry = entry.file_name().to_string_lossy().into_owned();
                entry
                    .strip_prefix(&name)
                    .and_then(|rest| rest.strip_suffix(".tmp"))
                    .is_some_and(|token| {
                        token.len() == 16 && token.chars().all(|c| c.is_ascii_hexdigit())
                    })
            })
    }

    #[test]
    fn backup_broken_config_moves_file_and_returns_path() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "[video] 壊れている").expect("テスト用の設定を書けること");

        let backup = backup_broken_config(&path)
            .expect("退避に成功すること")
            .expect("退避先のパスが返ること");

        assert_eq!(backup, dir.path().join("default-config.toml.bak"));
        assert!(!path.exists(), "退避後に元のファイルが残っている");
        assert_eq!(
            fs::read_to_string(&backup).expect("退避先を読めること"),
            "[video] 壊れている"
        );
    }

    #[test]
    fn backup_broken_config_existing_backup_gets_numbered_suffix() {
        // 続けて壊れた場合に、前回退避した内容を上書きしないこと。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        fs::write(&path, "1 回目").expect("テスト用の設定を書けること");
        let first = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "2 回目").expect("テスト用の設定を書けること");
        let second = backup_broken_config(&path).unwrap().unwrap();
        fs::write(&path, "3 回目").expect("テスト用の設定を書けること");
        let third = backup_broken_config(&path).unwrap().unwrap();

        assert_eq!(first, dir.path().join("default-config.toml.bak"));
        assert_eq!(second, dir.path().join("default-config.toml.bak.1"));
        assert_eq!(third, dir.path().join("default-config.toml.bak.2"));
        assert_eq!(fs::read_to_string(&first).unwrap(), "1 回目");
        assert_eq!(fs::read_to_string(&second).unwrap(), "2 回目");
        assert_eq!(fs::read_to_string(&third).unwrap(), "3 回目");
    }

    #[test]
    fn outcome_from_backup_backup_failed_forbids_writing_defaults() {
        // 退避に失敗した場合。読めなかったファイルがディスクに残っているため、
        // 起動時に既定値を書き戻してはならない。書き戻すとユーザーが設定を
        // 取り戻す最後の手段が消える。
        let failed = Err(std::io::Error::other("退避に失敗した"));

        let outcome = outcome_from_backup(failed);

        assert_eq!(outcome, LoadOutcome::BrokenFileLeftBehind);
        assert!(!outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_backup_succeeded_allows_writing_defaults() {
        // 退避できた場合。元のファイルは .bak として残っているため、
        // 既定値を書き戻してよい。
        let backed_up = Ok(Some(PathBuf::from("default-config.toml.bak")));

        let outcome = outcome_from_backup(backed_up);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn outcome_from_backup_nothing_to_back_up_allows_writing_defaults() {
        // 退避するファイルがそもそも無かった場合。
        // 潰す相手がいないので、既定値を書き戻してよい。
        let nothing = Ok(None);

        let outcome = outcome_from_backup(nothing);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(outcome.may_write_defaults_on_startup());
    }

    #[test]
    fn auto_save_policy_broken_file_left_behind_blocks_autosave() {
        // 退避できなかった場合。ウィンドウを動かすか終了するだけで
        // 壊れたファイルが既定値で潰れるのを防ぐため、自動保存を止める
        let policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_loaded_allows_autosave() {
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded).is_allowed());
    }

    #[test]
    fn auto_save_policy_fell_back_to_defaults_allows_autosave() {
        // 退避できていれば元の内容は .bak に残っている。守る相手がいないので
        // ウィンドウ位置や音量を通常どおり保存してよい
        assert!(AutoSavePolicy::from_load_outcome(LoadOutcome::FellBackToDefaults).is_allowed());
    }

    #[test]
    fn auto_save_policy_successful_explicit_save_unblocks_autosave() {
        // 設定画面の「適用」「OK」で保存できた時点で、壊れたファイルは
        // ユーザーの意思で置き換わっている。以降は自動保存を止めない
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(true);

        assert!(policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_keeps_autosave_blocked() {
        // 保存に失敗した場合は壊れたファイルがまだ残っているため、
        // 止めたままにする
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::BrokenFileLeftBehind);

        policy.note_explicit_save(false);

        assert!(!policy.is_allowed());
    }

    #[test]
    fn auto_save_policy_failed_explicit_save_does_not_block_allowed_policy() {
        // もともと許可されている状態は、保存の失敗で止まらない。
        // 一時的な書き込み失敗で以降の保存が全部止まると、
        // 復旧したあとも設定が残らなくなる
        let mut policy = AutoSavePolicy::from_load_outcome(LoadOutcome::Loaded);

        policy.note_explicit_save(false);

        assert!(policy.is_allowed());
    }

    #[test]
    fn load_outcome_loaded_allows_writing_defaults() {
        // 正常に読めた場合。通常どおり保存してよい。
        assert!(LoadOutcome::Loaded.may_write_defaults_on_startup());
    }

    #[test]
    fn backup_broken_config_missing_file_returns_none() {
        // 初回起動のように設定ファイルがまだ無い場合。退避するものが無いだけで、
        // エラーとして扱わない。
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");

        let backup = backup_broken_config(&path).expect("失敗しないこと");

        assert!(backup.is_none());
    }

    #[test]
    fn is_blank_config_empty_and_whitespace_are_blank() {
        assert!(is_blank_config(b""));
        assert!(is_blank_config(b"  \r\n\t\n"));
    }

    #[test]
    fn is_blank_config_any_content_is_not_blank() {
        // 1 文字でも中身があれば TOML の解釈に任せる（コメントだけでも読める）
        assert!(!is_blank_config(b"# comment"));
        assert!(!is_blank_config(b"[video]"));
    }

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
    fn export_to_keeps_existing_tmp_file_on_success() {
        // Issue #369。書き出し先の隣にもともとある同じ名前の `.tmp` を
        // 上書きも削除もしない
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        let stale = dir.path().join("exported.toml.tmp");
        fs::write(&stale, "user data").expect("既存の .tmp を作れること");

        export_to(&path, &AppSettings::default()).expect("書き出せること");

        assert_eq!(fs::read_to_string(&stale).unwrap(), "user data");
        assert!(path.is_file());
        // 自分の一時ファイルは rename で消えている
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn export_to_keeps_existing_tmp_file_on_failure() {
        // 置き換え先がフォルダで rename が失敗しても、既存の `.tmp` は残り、
        // 自分の一時ファイルだけが消える
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        fs::create_dir(&path).expect("置き換えられないフォルダを作れること");
        let stale = dir.path().join("exported.toml.tmp");
        fs::write(&stale, "user data").expect("既存の .tmp を作れること");

        assert!(export_to(&path, &AppSettings::default()).is_err());

        assert_eq!(fs::read_to_string(&stale).unwrap(), "user data");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn load_from_empty_file_backs_it_up_and_falls_back_to_defaults() {
        // Issue #317。保存の途中で止まって 0 バイトになったファイルは、
        // `#[serde(default)]` のせいで「読めた」ことになり、退避されずに
        // 全設定が既定値へ戻っていた。壊れたファイルとして退避すること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "").expect("空のファイルを作れること");

        let (settings, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(dir.path().join("default-config.toml.bak").exists());
        assert!(!path.exists(), "空のファイルが元の場所に残っている");
        assert_eq!(
            settings.video.device_name,
            AppSettings::default().video.device_name
        );
    }

    #[test]
    fn load_from_whitespace_only_file_is_treated_as_broken() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "\r\n  \r\n").expect("書けること");

        let (_, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(dir.path().join("default-config.toml.bak").exists());
    }

    #[test]
    fn load_from_valid_file_is_loaded_without_backup() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, FULL_CONFIG).expect("書けること");
        let expected: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");

        let (settings, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(settings.video.device_name, expected.video.device_name);
        assert!(!dir.path().join("default-config.toml.bak").exists());
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

    #[test]
    fn export_to_replaces_an_existing_file_and_leaves_no_temp_file() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        fs::write(&path, "[video]\nfps = 15\n").expect("古い内容を書けること");
        let settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");

        export_to(&path, &settings).expect("書き出せること");

        assert!(!has_own_temp_file(&path), "一時ファイルが残っている");
        let imported = import_from(&path).expect("読み戻せること");
        assert_eq!(imported.video.fps, settings.video.fps);
    }

    #[test]
    fn export_to_failure_returns_export_error_and_removes_the_temp_file() {
        // 書き出し先がディレクトリで置き換えられない場合。保存ではなく
        // 書き出しの失敗として返り、一時ファイルも残らないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        fs::create_dir(&path).expect("ディレクトリを作れること");

        let err = export_to(&path, &AppSettings::default()).expect_err("失敗すること");

        assert!(
            matches!(&err, SettingsError::ExportFailed { path: p, .. } if p == &path),
            "書き出しの失敗として返ること: {err:?}"
        );
        assert!(path.is_dir(), "元の場所が変わっている");
        assert!(!has_own_temp_file(&path), "一時ファイルが残っている");
    }

    #[test]
    fn export_file_name_uses_the_given_date() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260921.toml"
        );
    }

    #[test]
    fn export_file_name_pads_single_digit_month_and_day() {
        // 境界。0 埋めを忘れると 2026-1-5 が 202615 になり、並べ替えが崩れる
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).expect("日付として正しいこと");

        assert_eq!(
            export_file_name(&date),
            "capturecard_viewer-settings-20260105.toml"
        );
    }

    #[test]
    fn export_to_then_import_from_restores_the_values() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("exported.toml");
        let mut settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");
        settings.set_hotkey(HotkeyAction::VolumeUp, Some("Ctrl+Up".to_string()));

        export_to(&path, &settings).expect("書き出せること");
        let imported = import_from(&path).expect("読み戻せること");

        assert_eq!(imported.video.device_name, settings.video.device_name);
        assert_eq!(imported.video.resolution, settings.video.resolution);
        assert_eq!(imported.audio.sample_rate, settings.audio.sample_rate);
        assert_eq!(imported.screenshot.format, settings.screenshot.format);
        assert_eq!(
            imported.screenshot.jpeg_quality,
            settings.screenshot.jpeg_quality
        );
        assert_eq!(imported.ui.volume, settings.ui.volume);
        assert_eq!(imported.hotkeys, settings.hotkeys);
    }

    #[test]
    fn import_from_missing_file_returns_error_without_creating_it() {
        // confy の load_path はファイルが無いと既定値で作ってしまう。
        // 読み込みのつもりで選んだ場所にファイルが増えないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("missing.toml");

        let err = import_from(&path).expect_err("エラーになること");

        assert_eq!(err, SettingsError::FileNotFound(path.clone()));
        assert!(!path.exists());
    }

    #[test]
    fn import_from_broken_toml_returns_error() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("broken.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書けること");

        let err = import_from(&path).expect_err("エラーになること");

        // ファイルはあるので「見つからない」ではなく解釈の失敗として返ること。
        // 区別が付かないと、ユーザーは置き場所を疑って直しようがなくなる
        assert!(
            matches!(err, SettingsError::ImportFailed { .. }),
            "解釈の失敗として返ること: {err:?}"
        );
    }

    #[test]
    fn import_from_a_directory_is_not_reported_as_missing() {
        // 実在するのに「見つからない」と出すと、置き場所を疑って直しようがない。
        // is_file() だけで判定していたころはここが FileNotFound になっていた
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("not-a-file");
        fs::create_dir(&path).expect("ディレクトリを作れること");

        let err = import_from(&path).expect_err("エラーになること");

        assert_eq!(err, SettingsError::NotAFile(path));
    }

    #[test]
    fn import_from_unknown_values_falls_back_to_defaults() {
        // 手で書き換えたファイルを読み込んだ場合。解釈できない値だけが
        // 既定へ倒れ、他の項目は読めていること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("odd.toml");
        fs::write(
            &path,
            r#"
[video]
fps = 30
color_space = "bt2020"

[screenshot]
format = "webp"
jpeg_quality = 500
"#,
        )
        .expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.video.fps, Some(30));
        assert_eq!(imported.video.color_space, ColorSpace::Auto);
        assert_eq!(imported.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(imported.screenshot.jpeg_quality, MAX_JPEG_QUALITY);
    }

    #[test]
    fn import_from_legacy_file_migrates_the_hotkey() {
        // 旧版が書き出したファイルを読み込んだ場合も、通常の起動と同じく
        // screenshot.hotkey が hotkeys へ移ること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("legacy.toml");
        fs::write(&path, LEGACY_CONFIG).expect("書けること");

        let imported = import_from(&path).expect("読めること");

        assert_eq!(imported.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
    }
}
