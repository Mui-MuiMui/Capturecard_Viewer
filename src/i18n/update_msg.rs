//! 引数を取る文字列のうち、更新の確認と適用（`src/update/`、通知ダイアログ、
//! 「その他」タブの「更新」の欄）で使うもの。
//!
//! 書き方の決まりは `msg.rs` と同じ（1 関数が 1 件、文全体をここで組み立てる）。
//! `msg.rs` が 800 行を超えたので、塊ごとこちらへ分けた。

use std::fmt::Display;

use super::{language, Language};

// ---- 更新の確認（update::UpdateError / ui/update_dialog.rs / ui/other_tab.rs） ----

pub fn update_network_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("GitHub に接続できない: {source}"),
        Language::English => format!("Cannot connect to GitHub: {source}"),
    }
}

pub fn update_http_status(code: u16) -> String {
    match language() {
        Language::Japanese => format!("GitHub が HTTP {code} を返した"),
        Language::English => format!("GitHub returned HTTP {code}"),
    }
}

pub fn update_invalid_response(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("GitHub の応答を読めない: {source}"),
        Language::English => format!("Cannot read the response from GitHub: {source}"),
    }
}

pub fn update_invalid_tag(tag: impl Display) -> String {
    match language() {
        Language::Japanese => format!("最新のリリースのタグ '{tag}' をバージョンとして読めない"),
        Language::English => format!("Cannot read the latest release tag '{tag}' as a version"),
    }
}

pub fn update_local_file_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("テスト用の Release の JSON を読めない: {source}"),
        Language::English => format!("Cannot read the test release JSON: {source}"),
    }
}

/// 通知ダイアログの見出し。版は `v` を付けずに渡す。
pub fn update_available_heading(latest: impl Display, current: impl Display) -> String {
    match language() {
        Language::Japanese => format!("新しいバージョン v{latest} があります（いまは v{current}）"),
        Language::English => format!("Version v{latest} is available (you have v{current})"),
    }
}

pub fn update_current_version(version: impl Display) -> String {
    match language() {
        Language::Japanese => format!("現在のバージョン: v{version}"),
        Language::English => format!("Current version: v{version}"),
    }
}

pub fn update_status_available(latest: impl Display) -> String {
    match language() {
        Language::Japanese => format!("新しいバージョン v{latest} があります"),
        Language::English => format!("Version v{latest} is available"),
    }
}

pub fn update_status_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("確認できない: {reason}"),
        Language::English => format!("Could not check: {reason}"),
    }
}

pub fn update_skipped_version(version: impl Display) -> String {
    match language() {
        Language::Japanese => format!("通知しないバージョン: v{version}"),
        Language::English => format!("Not notifying about: v{version}"),
    }
}

// ---- 更新の適用（update::apply::ApplyError / ui/update_dialog.rs） ----

pub fn update_exe_path_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("実行中の exe の場所が分からない: {source}"),
        Language::English => format!("Cannot find the running exe: {source}"),
    }
}

pub fn update_download_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ダウンロードできない: {source}"),
        Language::English => format!("Cannot download the update: {source}"),
    }
}

pub fn update_download_http_status(code: u16) -> String {
    match language() {
        Language::Japanese => format!("ダウンロードで HTTP {code} が返った"),
        Language::English => format!("The download returned HTTP {code}"),
    }
}

pub fn update_file_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => format!("ダウンロードしたファイルを読み書きできない: {source}"),
        Language::English => format!("Cannot read or write the downloaded file: {source}"),
    }
}

pub fn update_replace_failed(source: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!("exe を置き換えられない（元の exe はそのまま）: {source}")
        }
        Language::English => {
            format!("Cannot replace the exe (the current exe is kept): {source}")
        }
    }
}

/// 置き換えに失敗し、元の exe も戻せなかったので新しい exe を置いたとき。
/// `old` は元の exe の場所。
pub fn update_replace_kept_new(old: impl Display, source: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "exe を置き換えられず、元の exe も戻せなかったため、新しい exe を置いた。次の起動から新しいバージョンになる（元の exe は {old}）: {source}"
        ),
        Language::English => format!(
            "Cannot replace the exe or restore the current one, so the new exe was put in place. The new version starts next time (the previous exe is at {old}): {source}"
        ),
    }
}

/// 差し替えようとしたら、元の名前に exe が無かったとき（前回の更新で `.old` と
/// `.new` だけが残った状態からの再試行）。`update_replace_kept_nothing` の理由に入る。
pub fn update_exe_missing(exe: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{exe} が無い"),
        Language::English => format!("{exe} does not exist"),
    }
}

/// 置き換えに失敗し、元の exe も新しい exe も元の名前へ置けなかったとき。
/// `old` は元の exe、`new` は照合済みの新しい exe の場所。
pub fn update_replace_kept_nothing(
    old: impl Display,
    new: impl Display,
    source: impl Display,
) -> String {
    match language() {
        Language::Japanese => format!(
            "exe を置き換えられず、元に戻すこともできなかった。{old} の名前から「.old」を外せば元のバージョンで起動できる（照合済みの新しい exe は {new}）: {source}"
        ),
        Language::English => format!(
            "Cannot replace the exe or put it back. Remove \".old\" from the name of {old} to start the previous version (the verified new exe is at {new}): {source}"
        ),
    }
}

/// ダウンロード中の割合。
pub fn update_downloading_percent(percent: u8) -> String {
    match language() {
        Language::Japanese => format!("ダウンロード中... {percent}%"),
        Language::English => format!("Downloading... {percent}%"),
    }
}

/// 大きさが分からないときのダウンロード中の量。`mib` は MiB 単位。
pub fn update_downloading_amount(mib: f64) -> String {
    match language() {
        Language::Japanese => format!("ダウンロード中... {mib:.1} MB"),
        Language::English => format!("Downloading... {mib:.1} MB"),
    }
}
