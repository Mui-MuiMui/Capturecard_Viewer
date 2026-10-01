//! 設定の書き出しと読み込み（`export_to` / `import_from`）と、書き出すファイルの
//! 既定の名前（`docs/design/settings.md`）。

use super::store::parse_settings;
use super::write::replace_atomically;
use super::{AppSettings, SettingsError, APP_NAME};
use chrono::Datelike;
use std::path::Path;

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
// **書式は `%AppData%` の設定ファイルと同じ（`serialize_settings`）。** 別の
// 書き方にすると、書き出したファイルを読み戻せない組み合わせが生まれる。
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
// 先にメタデータで「無い」「ファイルではない」を分けておく。読んだときの
// 失敗と区別が付かないと、ユーザーは置き場所を疑って直しようがなくなる。
//
// 確かめ方に `Path::is_file()` を使わないのは、**実在するのにメタデータを
// 取れない場合も `false` を返す**ため。権限の無いファイルを選んだときに
// 「見つからない」と出すと、置き場所を疑って直しようがなくなる。
pub fn import_from(path: &Path) -> Result<AppSettings, SettingsError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        // ディレクトリやデバイスファイル。読もうとしても読めない
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

    std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|contents| parse_settings(&contents))
        .map_err(|source| SettingsError::ImportFailed {
            path: path.to_path_buf(),
            source,
        })
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

    use crate::settings::testing::has_own_temp_file;

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
        // 1.2.x まで使っていた confy の load_path はファイルが無いと既定値で
        // 作っていた。読み込みのつもりで選んだ場所にファイルが増えないこと
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
