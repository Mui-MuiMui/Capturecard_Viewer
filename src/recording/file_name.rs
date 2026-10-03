//! 録画のファイル名。書式（chrono の strftime）の検めと、同じ名前が既にあるときの連番。
//!
//! **書式は使う前に必ず検める。** chrono は解釈できない指定子を含む書式を文字列に
//! するとパニックし、release は `panic = "abort"` なのでそのまま落ちる
//! （`docs/design/recording.md` の「設定 `[recording]`」）。どれも純粋関数で、
//! 録画の開始（`app::recording`）と設定ダイアログの注意書き（`ui::recording_tab`）が
//! 同じ判定を使う。

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::i18n::{self, Text};
use crate::settings::DEFAULT_RECORDING_FILE_NAME_FORMAT;

/// 録画のファイルに付ける拡張子。保存形式は MP4 固定。
pub const RECORDING_EXTENSION: &str = "mp4";

/// Windows のファイル名に使えない文字。制御文字も別に弾く。
const FORBIDDEN_CHARACTERS: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Windows の予約デバイス名。拡張子を付けても（`NUL.mp4`）予約名のまま。
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// ファイル名の書式が使えない理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNameFormatError {
    /// 書式が空、または結果が空
    Empty,
    /// chrono が解釈できない指定子を含む（`%Q` など）
    InvalidSpecifier,
    /// 結果に Windows のファイル名に使えない文字が入る
    ForbiddenCharacter(char),
    /// 結果の末尾が空白か `.`（Windows が黙って落とす）
    TrailingDotOrSpace,
    /// 結果が Windows の予約デバイス名になる
    ReservedName(String),
}

impl fmt::Display for FileNameFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            FileNameFormatError::Empty => Text::RecordingFileNameEmpty.get().to_string(),
            FileNameFormatError::InvalidSpecifier => {
                Text::RecordingFileNameInvalidSpecifier.get().to_string()
            }
            FileNameFormatError::ForbiddenCharacter(c) => {
                i18n::recording_file_name_forbidden_character(c)
            }
            FileNameFormatError::TrailingDotOrSpace => {
                Text::RecordingFileNameTrailingDot.get().to_string()
            }
            FileNameFormatError::ReservedName(name) => i18n::recording_file_name_reserved(name),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for FileNameFormatError {}

/// `format` を `at` の時刻で文字列にする（拡張子は付けない）。使えない書式なら理由を返す。
pub fn render_file_name(format: &str, at: &DateTime<Local>) -> Result<String, FileNameFormatError> {
    if format.trim().is_empty() {
        return Err(FileNameFormatError::Empty);
    }
    let items: Vec<Item<'_>> = StrftimeItems::new(format).collect();
    if items.iter().any(|item| matches!(item, Item::Error)) {
        return Err(FileNameFormatError::InvalidSpecifier);
    }
    let name = at.format_with_items(items.into_iter()).to_string();
    check_file_name(&name)?;
    Ok(name)
}

/// 録画に使うファイル名（拡張子なし）を決める。書式が使えなければ既定の書式へ倒し、
/// 理由も返す（呼び出し側がログに残す）。
pub fn resolve_file_stem(
    format: &str,
    at: &DateTime<Local>,
) -> (String, Option<FileNameFormatError>) {
    match render_file_name(format, at) {
        Ok(name) => (name, None),
        Err(reason) => {
            // 既定の書式は必ず通る（テストで確かめている）
            let fallback = render_file_name(DEFAULT_RECORDING_FILE_NAME_FORMAT, at)
                .unwrap_or_else(|_| "Recording".to_string());
            (fallback, Some(reason))
        }
    }
}

/// `folder` の中で使っていない `stem.mp4` のパス。あれば `_2`、`_3` … を付ける。
///
/// ファイルがあるかを見るだけなので、録画スレッドが Sink Writer を作る直前に呼ぶ。
pub fn unique_path(folder: &Path, stem: &str) -> PathBuf {
    let first = folder.join(format!("{stem}.{RECORDING_EXTENSION}"));
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| folder.join(format!("{stem}_{n}.{RECORDING_EXTENSION}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

/// 文字列にした結果が Windows のファイル名として使えるか。
fn check_file_name(name: &str) -> Result<(), FileNameFormatError> {
    if name.is_empty() {
        return Err(FileNameFormatError::Empty);
    }
    if let Some(c) = name
        .chars()
        .find(|c| FORBIDDEN_CHARACTERS.contains(c) || c.is_control())
    {
        return Err(FileNameFormatError::ForbiddenCharacter(c));
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(FileNameFormatError::TrailingDotOrSpace);
    }
    // 予約名は拡張子の手前（最初の `.` まで）で比べる。`NUL.foo` も `.mp4` を
    // 付ければ `NUL.foo.mp4` で、やはり予約名として扱われる
    let base = name.split('.').next().unwrap_or(name).trim_end();
    if RESERVED_NAMES
        .iter()
        .any(|reserved| base.eq_ignore_ascii_case(reserved))
    {
        return Err(FileNameFormatError::ReservedName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use tempfile::tempdir;

    fn sample_time() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 9, 29, 12, 34, 56)
            .single()
            .expect("曖昧でない時刻")
    }

    #[test]
    fn render_file_name_default_format_is_valid() {
        assert_eq!(
            render_file_name(DEFAULT_RECORDING_FILE_NAME_FORMAT, &sample_time()),
            Ok("Recording_2026-09-29_12-34-56".to_string())
        );
    }

    #[test]
    fn render_file_name_unknown_specifier_is_rejected() {
        // chrono はこのまま文字列にするとパニックする
        assert_eq!(
            render_file_name("rec_%Q", &sample_time()),
            Err(FileNameFormatError::InvalidSpecifier)
        );
        assert_eq!(
            render_file_name("rec_%", &sample_time()),
            Err(FileNameFormatError::InvalidSpecifier)
        );
    }

    #[test]
    fn render_file_name_forbidden_characters_are_rejected() {
        // %H:%M のようにコロンが入る書式はよく書かれる
        assert_eq!(
            render_file_name("%H:%M", &sample_time()),
            Err(FileNameFormatError::ForbiddenCharacter(':'))
        );
        assert_eq!(
            render_file_name("a/b", &sample_time()),
            Err(FileNameFormatError::ForbiddenCharacter('/'))
        );
        assert_eq!(
            render_file_name("rec?", &sample_time()),
            Err(FileNameFormatError::ForbiddenCharacter('?'))
        );
        assert_eq!(
            render_file_name("tab\there", &sample_time()),
            Err(FileNameFormatError::ForbiddenCharacter('\t'))
        );
    }

    #[test]
    fn render_file_name_trailing_dot_or_space_is_rejected() {
        assert_eq!(
            render_file_name("rec.", &sample_time()),
            Err(FileNameFormatError::TrailingDotOrSpace)
        );
        assert_eq!(
            render_file_name("rec ", &sample_time()),
            Err(FileNameFormatError::TrailingDotOrSpace)
        );
    }

    #[test]
    fn render_file_name_reserved_names_are_rejected_regardless_of_case() {
        for format in ["NUL", "con", "Com1", "lpt9", "AUX.backup", "prn .x"] {
            assert!(
                matches!(
                    render_file_name(format, &sample_time()),
                    Err(FileNameFormatError::ReservedName(_))
                ),
                "{format} は予約名"
            );
        }
        // 予約名を含むだけの名前は使える
        assert!(render_file_name("CONSOLE", &sample_time()).is_ok());
        assert!(render_file_name("NUL_1", &sample_time()).is_ok());
        assert!(render_file_name("COM10", &sample_time()).is_ok());
    }

    #[test]
    fn render_file_name_empty_format_is_rejected() {
        assert_eq!(
            render_file_name("", &sample_time()),
            Err(FileNameFormatError::Empty)
        );
        assert_eq!(
            render_file_name("   ", &sample_time()),
            Err(FileNameFormatError::Empty)
        );
    }

    #[test]
    fn resolve_file_stem_falls_back_to_the_default_format() {
        let (stem, reason) = resolve_file_stem("%H:%M", &sample_time());
        assert_eq!(stem, "Recording_2026-09-29_12-34-56");
        assert_eq!(reason, Some(FileNameFormatError::ForbiddenCharacter(':')));

        let (stem, reason) = resolve_file_stem("clip_%Y%m%d", &sample_time());
        assert_eq!(stem, "clip_20260929");
        assert_eq!(reason, None);
    }

    #[test]
    fn unique_path_without_conflict_uses_the_stem_as_is() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        assert_eq!(unique_path(dir.path(), "rec"), dir.path().join("rec.mp4"));
    }

    #[test]
    fn unique_path_with_conflicts_appends_numbers_from_2() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("rec.mp4"), b"").expect("先客を置ける");
        assert_eq!(unique_path(dir.path(), "rec"), dir.path().join("rec_2.mp4"));

        fs::write(dir.path().join("rec_2.mp4"), b"").expect("先客を置ける");
        assert_eq!(unique_path(dir.path(), "rec"), dir.path().join("rec_3.mp4"));
    }

    #[test]
    fn file_name_format_error_display_is_localized() {
        assert!(FileNameFormatError::ForbiddenCharacter(':')
            .to_string()
            .contains(':'));
        let english = crate::i18n::with_language(crate::i18n::Language::English, || {
            FileNameFormatError::InvalidSpecifier.to_string()
        });
        assert!(english.is_ascii(), "{english}");
    }
}
