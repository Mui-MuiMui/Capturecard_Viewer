//! 設定ファイルの読み書き。起動時の読み込み（`AppSettings::load`）と保存、
//! 読めなかったファイルの退避、自動保存を許すかの判断（`AutoSavePolicy`）、
//! 設定ファイルの TOML の読み方と書き方（`docs/design/settings.md`）。
//! 一時ファイルを使う書き込みは `write`、書き出し / 読み込みは `transfer` が持つ。
//!
//! 置き場所の決定は `crate::config_path` が持ち、ここは呼ぶだけ。

use super::write::write_atomically;
use super::{AppSettings, SettingsError};
use log::{error, warn};
use std::path::{Path, PathBuf};

// 設定ファイルをどう読めたか。起動時に既定値を書き戻してよいかの判断に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    // 読み込めた。初回起動などでファイルがまだ無く、既定値を返した場合も含む
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

// 設定ファイルの中身（TOML）を `AppSettings` へ読む。
//
// 1.2.x までは confy 0.6（内部は toml 0.8）で読み書きしていた。toml 1.x へ
// 変えても、旧版が書いたファイルはそのまま読め、書き出す内容もバイト単位で
// 同じだった（`docs/DEPENDENCIES.md` の第 3 段）。
//
// 失敗の理由は 1 行にまとめる。toml のエラーの `Display` は該当行の抜き出しを
// 含む複数行で、設定の読み込みの失敗はトーストにも出るため。
pub(super) fn parse_settings(contents: &str) -> Result<AppSettings, String> {
    toml::from_str(contents).map_err(|e| {
        let full = e.to_string();
        // 1 行目は「TOML parse error at line L, column C」
        let position = full.lines().next().unwrap_or_default();
        format!("{}: {}", position, e.message().trim())
    })
}

// 設定を、設定ファイルに書く TOML にする。保存と書き出しで共通。
pub(super) fn serialize_settings(settings: &AppSettings) -> Result<String, String> {
    toml::to_string_pretty(settings).map_err(|e| e.to_string())
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

impl AppSettings {
    // 設定と、その読み込み結果を返す。
    //
    // 結果を返しているのは、起動時に既定値を書き戻してよいかを呼び出し側が
    // 判断できるようにするため。退避に失敗したまま書き戻すと、読めなかった
    // ファイルを既定値で上書きしてしまい、証跡ごと消える。
    pub fn load() -> (Self, LoadOutcome) {
        // 置き場所は環境変数 CAPTURECARD_VIEWER_CONFIG_DIR で差し替えられる
        // （crate::config_path）。指定が無ければ %AppData% の下
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
    pub(super) fn load_from(path: &Path) -> (Self, LoadOutcome) {
        match read_config_file(path) {
            Ok(Some(settings)) => (settings, LoadOutcome::Loaded),
            // 初回起動などでまだ無い。既定値で起動し、起動時の保存が作る
            // （`save` が親のフォルダも作る）。confy 0.6 はここで既定値の
            // ファイルを書いていた
            Ok(None) => (Self::default(), LoadOutcome::Loaded),
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
        self.save_to(&path)
    }

    // 指定したパスへ保存する。`save` の本体。パスを引数にしているのは
    // `load_from` と同じくテストのため。
    fn save_to(&self, path: &Path) -> Result<(), SettingsError> {
        // 初回起動では `%AppData%\capturecard_viewer\config` がまだ無い
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SettingsError::SaveFailed {
                path: path.to_path_buf(),
                source: e.to_string(),
            })?;
        }
        write_atomically(path, self)
    }
}

// 設定ファイルを読む。ファイルが無ければ `Ok(None)`。
//
// 空のファイルと読めないファイルは `Err`（理由はログに出す文言）。呼び出し側が
// 退避して既定値で起動する。
fn read_config_file(path: &Path) -> Result<Option<AppSettings>, String> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if is_blank_config(&contents) {
        return Err("設定ファイルが空（0 バイトか空白だけ）".to_string());
    }
    let contents = String::from_utf8(contents).map_err(|e| e.to_string())?;
    parse_settings(&contents).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::testing::has_own_temp_file;
    use crate::settings::testing::{FULL_CONFIG, LEGACY_CONFIG};
    use std::fs;
    use tempfile::tempdir;

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
    pub(super) fn load_from_empty_file_backs_it_up_and_falls_back_to_defaults() {
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
    pub(super) fn load_from_whitespace_only_file_is_treated_as_broken() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "\r\n  \r\n").expect("書けること");

        let (_, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(dir.path().join("default-config.toml.bak").exists());
    }

    #[test]
    pub(super) fn load_from_valid_file_is_loaded_without_backup() {
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
    pub(super) fn load_from_legacy_file_migrates_the_hotkey() {
        // 旧版が書いた設定ファイルを、起動時の読み込みでもそのまま読めること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, LEGACY_CONFIG).expect("書けること");

        let (settings, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
    }

    #[test]
    pub(super) fn load_from_missing_file_returns_defaults_without_creating_it() {
        // 初回起動。既定値で読めたことにし、ファイルは起動時の保存が作る
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("config").join("default-config.toml");

        let (settings, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::Loaded);
        assert!(outcome.may_write_defaults_on_startup());
        assert_eq!(settings.video.fps, AppSettings::default().video.fps);
        assert!(!path.exists());
    }

    #[test]
    pub(super) fn load_from_broken_toml_backs_it_up() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("default-config.toml");
        fs::write(&path, "これは TOML ではない [[[").expect("書けること");

        let (_, outcome) = AppSettings::load_from(&path);

        assert_eq!(outcome, LoadOutcome::FellBackToDefaults);
        assert!(dir.path().join("default-config.toml.bak").exists());
    }

    #[test]
    fn parse_settings_error_is_a_single_line_with_the_position() {
        // トーストに出るので 1 行にまとめる。位置は残す
        let err = parse_settings("これは TOML ではない [[[").expect_err("エラーになること");

        assert!(!err.contains('\n'), "複数行になっている: {err}");
        assert!(err.contains("line 1"), "位置が無い: {err}");
    }

    #[test]
    fn serialize_settings_round_trips() {
        let settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めること");

        let written = serialize_settings(&settings).expect("書けること");
        let reloaded = parse_settings(&written).expect("読み戻せること");

        assert_eq!(serialize_settings(&reloaded).unwrap(), written);
    }

    #[test]
    fn parse_settings_ignores_the_removed_register_hotkey_key() {
        // 「キーを奪う方式」（#207）を外す前の版が書いた設定ファイル。
        // 外した項目が残っていても読め、同じセクションの他の項目は失われない。
        // 次に保存したときには書き戻さない
        let config = format!(
            "{FULL_CONFIG}\n[hotkey_settings]\nonly_when_focused = true\nuse_register_hotkey = true\n"
        );

        let settings = parse_settings(&config).expect("外した項目が残っていても読めること");

        assert!(settings.hotkey_settings.only_when_focused);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        let written = serialize_settings(&settings).expect("書けること");
        assert!(
            !written.contains("use_register_hotkey"),
            "外した項目が書き戻されている: {written}"
        );
    }

    #[test]
    fn save_to_creates_the_missing_folder() {
        // 初回起動では設定ファイルのフォルダがまだ無い
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir
            .path()
            .join("capturecard_viewer")
            .join("config")
            .join("default-config.toml");

        AppSettings::default()
            .save_to(&path)
            .expect("保存できること");

        assert!(path.is_file());
        assert!(!has_own_temp_file(&path));
    }
}
