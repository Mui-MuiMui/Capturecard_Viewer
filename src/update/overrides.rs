//! 更新の確認を試すための環境変数。
//!
//! 実際に新しい Release が無くても通知の流れを確かめられるよう、比較に使う
//! 「いまの版」と、問い合わせ先の Release API を差し替える。フェイクデバイス
//! （`CAPTURECARD_VIEWER_FAKE_DEVICES`）と同じく、起動するときだけ指定する
//! 開発者向けのもので、設定ファイルには保存しない（`docs/design/update.md`）。
//!
//! 環境変数の解釈は純粋関数（`CheckOverrides::from_env_values`）にしてあり、
//! 環境変数を読むのは `from_env` の 1 か所だけ。

use log::warn;
use semver::Version;
use std::path::PathBuf;

use super::{parse_version_tag, LATEST_RELEASE_API_URL};

/// 比較に使う「いまの版」を差し替える。`1.0.0` / `v1.0.0`。
pub const CURRENT_VERSION_ENV: &str = "CAPTURECARD_VIEWER_UPDATE_CURRENT_VERSION";

/// Release API の URL を差し替える。`http://` / `https://` の URL か、
/// Release の JSON を置いたファイルの `file://` の URL。
pub const API_URL_ENV: &str = "CAPTURECARD_VIEWER_UPDATE_API_URL";

/// 最新の Release の JSON をどこから取るか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseSource {
    /// HTTP で取る。既定は GitHub の `releases/latest`
    Http(String),
    /// ファイルをそのまま読む（`file://` で指定したとき）
    File(PathBuf),
}

impl Default for ReleaseSource {
    fn default() -> Self {
        ReleaseSource::Http(LATEST_RELEASE_API_URL.to_string())
    }
}

/// 環境変数で差し替えた内容。どちらも無ければ通常の確認と同じ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOverrides {
    /// 比較に使う「いまの版」。`None` なら実行中の版
    pub current_version: Option<Version>,
    /// 問い合わせ先。`None` なら GitHub の `releases/latest`
    pub source: Option<ReleaseSource>,
}

impl CheckOverrides {
    /// 環境変数を読む。**起動時に 1 回だけ呼ぶ。** 差し替えがあれば WARN で残す。
    pub fn from_env() -> Self {
        let current = std::env::var(CURRENT_VERSION_ENV).ok();
        let api_url = std::env::var(API_URL_ENV).ok();
        let overrides = Self::from_env_values(current.as_deref(), api_url.as_deref());
        if overrides.is_active() {
            // 通知が出る・出ない理由がログから分かるよう、目立つ段で残す
            warn!(
                "更新の確認のテスト用のオーバーライドが有効（いまの版: {:?}、問い合わせ先: {:?}）",
                overrides.current_version.as_ref().map(ToString::to_string),
                overrides.source
            );
        }
        overrides
    }

    /// 環境変数の値を解釈する。読めない値は WARN を残して使わない
    /// （通常の確認に倒す）。空や空白だけの値は指定していないのと同じ。
    pub fn from_env_values(current_version: Option<&str>, api_url: Option<&str>) -> Self {
        Self {
            current_version: current_version.and_then(parse_current_version),
            source: api_url.and_then(parse_release_source),
        }
    }

    /// どちらかを差し替えているか。
    pub fn is_active(&self) -> bool {
        self.current_version.is_some() || self.source.is_some()
    }

    /// 比較に使う「いまの版」。
    pub fn current_version_or(&self, actual: Version) -> Version {
        self.current_version.clone().unwrap_or(actual)
    }

    /// 問い合わせ先。
    pub fn source_or_default(&self) -> ReleaseSource {
        self.source.clone().unwrap_or_default()
    }
}

fn parse_current_version(value: &str) -> Option<Version> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let version = parse_version_tag(value);
    if version.is_none() {
        warn!(
            "{} の値 '{}' を版として読めないので使わない",
            CURRENT_VERSION_ENV, value
        );
    }
    version
}

fn parse_release_source(value: &str) -> Option<ReleaseSource> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = strip_prefix_ignore_case(value, "file://") {
        return match file_url_to_path(rest) {
            Some(path) => Some(ReleaseSource::File(path)),
            None => {
                warn!(
                    "{} の値 '{}' にファイルのパスが無いので使わない",
                    API_URL_ENV, value
                );
                None
            }
        };
    }
    let after_scheme = strip_prefix_ignore_case(value, "https://")
        .or_else(|| strip_prefix_ignore_case(value, "http://"));
    if let Some(rest) = after_scheme {
        if has_host(rest) {
            return Some(ReleaseSource::Http(value.to_string()));
        }
        warn!(
            "{} の値 '{}' にホスト名が無いので使わない",
            API_URL_ENV, value
        );
        return None;
    }
    warn!(
        "{} の値 '{}' は http:// / https:// / file:// のどれでもないので使わない",
        API_URL_ENV, value
    );
    None
}

/// `http://` / `https://` の後ろにホスト名があるか。
///
/// `http://:8000/x` や `http:///x` のようにホスト名が空のものを、問い合わせる前に
/// 弾くためのもの。URL として正しいかまでは見ない（おかしければ問い合わせの
/// 失敗として出る）。
fn has_host(after_scheme: &str) -> bool {
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // `user:pass@host:port` の `host` を取り出す
    let host_and_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if host_and_port.starts_with('[') {
        // IPv6 の `[::1]:8000`
        host_and_port.split(']').next().unwrap_or_default()
    } else {
        host_and_port.split(':').next().unwrap_or_default()
    };
    !host.trim_start_matches('[').is_empty()
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

/// `file://` の後ろをパスにする。
///
/// `file:///C:/work/release.json`（標準の形）でも `file://C:/work/release.json`
/// （スラッシュが 2 本）でも同じパスになるよう、ドライブ文字の前の `/` を落とす。
/// `%20` のような URL の符号化は解かない。空白を含むパスはそのまま書けばよい。
fn file_url_to_path(rest: &str) -> Option<PathBuf> {
    let bytes = rest.as_bytes();
    let path = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("テスト用の版は読めなければならない")
    }

    #[test]
    fn from_env_values_nothing_set_is_inactive() {
        let overrides = CheckOverrides::from_env_values(None, None);

        assert_eq!(overrides, CheckOverrides::default());
        assert!(!overrides.is_active());
        assert_eq!(overrides.current_version_or(v("1.1.0")), v("1.1.0"));
        assert_eq!(
            overrides.source_or_default(),
            ReleaseSource::Http(
                "https://api.github.com/repos/Mui-MuiMui/Capturecard_Viewer/releases/latest"
                    .to_string()
            )
        );
    }

    #[test]
    fn from_env_values_empty_values_are_ignored() {
        let overrides = CheckOverrides::from_env_values(Some(""), Some("   "));

        assert!(!overrides.is_active());
    }

    #[test]
    fn from_env_values_reads_current_version_with_or_without_prefix() {
        let plain = CheckOverrides::from_env_values(Some("1.0.0"), None);
        let prefixed = CheckOverrides::from_env_values(Some(" v1.0.0 "), None);

        assert_eq!(plain.current_version, Some(v("1.0.0")));
        assert_eq!(prefixed.current_version, Some(v("1.0.0")));
        assert!(plain.is_active());
        assert_eq!(plain.current_version_or(v("1.1.0")), v("1.0.0"));
    }

    #[test]
    fn from_env_values_unreadable_current_version_is_ignored() {
        let overrides = CheckOverrides::from_env_values(Some("old"), None);

        assert_eq!(overrides.current_version, None);
        assert!(!overrides.is_active());
    }

    #[test]
    fn from_env_values_reads_http_and_https_urls() {
        let https = CheckOverrides::from_env_values(
            None,
            Some("https://api.github.com/repos/someone/test/releases/latest"),
        );
        let http = CheckOverrides::from_env_values(None, Some("HTTP://127.0.0.1:8000/latest.json"));

        assert_eq!(
            https.source,
            Some(ReleaseSource::Http(
                "https://api.github.com/repos/someone/test/releases/latest".to_string()
            ))
        );
        assert_eq!(
            http.source,
            Some(ReleaseSource::Http(
                "HTTP://127.0.0.1:8000/latest.json".to_string()
            ))
        );
    }

    #[test]
    fn from_env_values_reads_file_urls() {
        let three_slashes =
            CheckOverrides::from_env_values(None, Some("file:///C:/work/release.json"));
        let two_slashes =
            CheckOverrides::from_env_values(None, Some("file://C:/work/release.json"));
        let relative = CheckOverrides::from_env_values(None, Some("file://release.json"));

        let expected = Some(ReleaseSource::File(PathBuf::from("C:/work/release.json")));
        assert_eq!(three_slashes.source, expected);
        assert_eq!(two_slashes.source, expected);
        assert_eq!(
            relative.source,
            Some(ReleaseSource::File(PathBuf::from("release.json")))
        );
    }

    #[test]
    fn from_env_values_file_url_without_path_is_ignored() {
        assert_eq!(
            CheckOverrides::from_env_values(None, Some("file://")).source,
            None
        );
    }

    #[test]
    fn from_env_values_unknown_scheme_is_ignored() {
        assert_eq!(
            CheckOverrides::from_env_values(None, Some("ftp://example.invalid/x.json")).source,
            None
        );
        assert_eq!(
            CheckOverrides::from_env_values(None, Some("C:/work/release.json")).source,
            None
        );
    }

    #[test]
    fn from_env_values_http_url_without_host_is_ignored() {
        for value in [
            "http://",
            "http:///latest.json",
            "http://:8000/latest.json",
            "https://@/x",
        ] {
            assert_eq!(
                CheckOverrides::from_env_values(None, Some(value)).source,
                None,
                "{value} は弾かれなければならない"
            );
        }
    }

    #[test]
    fn from_env_values_http_url_with_port_user_or_ipv6_is_kept() {
        for value in [
            "http://localhost:8000/latest.json",
            "http://user@127.0.0.1/latest.json",
            "http://[::1]:8000/latest.json",
            "https://example.invalid?x=1",
        ] {
            assert_eq!(
                CheckOverrides::from_env_values(None, Some(value)).source,
                Some(ReleaseSource::Http(value.to_string())),
                "{value} は使われなければならない"
            );
        }
    }

    #[test]
    fn from_env_values_reads_both_at_once() {
        let overrides = CheckOverrides::from_env_values(Some("1.0.0"), Some("file:///C:/r.json"));

        assert_eq!(overrides.current_version, Some(v("1.0.0")));
        assert_eq!(
            overrides.source,
            Some(ReleaseSource::File(PathBuf::from("C:/r.json")))
        );
    }
}
