//! 更新の確認。GitHub の Release から最新の版を取り、いまの版と比べる。
//!
//! ここにあるのは問い合わせ（`check_latest_release`）と、その結果を判断する
//! 純粋関数だけ。スレッドを起こして結果を画面へ渡すのは `app::update`、
//! 通知ダイアログの描画は `ui::update_dialog`（`docs/design/update.md`）。
//!
//! 新しい版の exe のダウンロード・照合・差し替えは `apply.rs`（部品は `swap.rs` と
//! `checksum.rs`）。
//! 試すための環境変数（比較に使う版と問い合わせ先の差し替え）は `overrides.rs`。

pub mod apply;
mod checksum;
mod overrides;
mod swap;

pub use self::overrides::CheckOverrides;

use self::overrides::ReleaseSource;
use crate::i18n::{self, Text};
use crate::settings::UpdateSettings;
use log::debug;
use semver::Version;
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// 最新の Release を返す API。draft と pre-release は GitHub 側で除かれる。
const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/Mui-MuiMui/Capturecard_Viewer/releases/latest";

/// 画面から開く最新のリリースページ。確認がまだ済んでいないときや、
/// API の返した URL がこのリポジトリのものでないときに使う。
pub const LATEST_RELEASE_PAGE_URL: &str =
    "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/latest";

/// リリースページとして開いてよい URL の頭。API の応答をそのまま
/// ブラウザへ渡さないための確認に使う。
const RELEASE_PAGE_PREFIX: &str = "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/";

/// 問い合わせ全体（名前解決・接続・応答の読み取り）の上限。
///
/// 起動を止めないよう別スレッドで行う。終了時には待たないので、ここを長くしても
/// 閉じるのは遅れない。長くすると「確認中」が続く時間が延びる。
/// 失敗の文言（`Text::UpdateTimedOut`）にも秒数を書いてあるので、変えたら合わせる。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub の API は `User-Agent` の無い要求を拒否する。
const USER_AGENT: &str = concat!("capturecard_viewer/", env!("CARGO_PKG_VERSION"));

/// 実行中の版。`Cargo.toml` の `version` から取る（`docs/BUILD.md` の「バージョン番号」）。
pub fn current_version() -> Version {
    // Cargo は semver として読める版しか受け付けないので、ここで失敗することは無い。
    // 万一読めなくても起動は止めず、どの版よりも古い扱いにする
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// Release に添付された資産 1 つ。
///
/// 更新の適用が `capturecard_viewer-<tag>-windows-x64.exe` と `SHA256SUMS.txt` を
/// 名前で引く（`apply::ApplyPlan::from_check`、`docs/RELEASE.md` の「配布物」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
}

/// 新しい版が見つかったときの内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// 実行中の版
    pub current: Version,
    /// 見つかった新しい版
    pub latest: Version,
    /// Release のタグそのまま（`v1.2.0`）。資産名に入っている
    pub tag: String,
    /// 開くリリースページ
    pub release_url: String,
    /// 添付された資産
    pub assets: Vec<ReleaseAsset>,
}

/// 問い合わせの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// 新しい版は無い（同じ版、古い版、pre-release のタグを含む）
    UpToDate,
    /// 新しい版がある
    Available(UpdateCheck),
}

/// 「その他」タブの「更新」の欄へ渡すもの。
pub struct UpdateView<'a> {
    /// 比較に使う「いまの版」。テスト用の環境変数で差し替えていればその版
    pub current: &'a Version,
    /// 確認の状態
    pub status: &'a UpdateStatus,
    /// 更新（ダウンロードと差し替え）の最中か。最中は「更新する」を押せなくする
    pub applying: bool,
}

/// 「その他」タブに出す確認の状態。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 起動時の確認を切っていて、まだ一度も確認していない
    #[default]
    NotChecked,
    /// 問い合わせ中
    Checking,
    /// 最新
    UpToDate,
    /// 新しい版がある
    Available(UpdateCheck),
    /// 確認できなかった。理由は起きた時点の言語で文字列にしてある
    Failed(String),
}

impl UpdateStatus {
    /// 「リリースページを開く」で開く URL。新しい版が見つかっていればその版の
    /// ページ、それ以外は最新のリリースページ。
    pub fn release_url(&self) -> &str {
        match self {
            UpdateStatus::Available(check) => &check.release_url,
            _ => LATEST_RELEASE_PAGE_URL,
        }
    }
}

/// 確認に失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// 接続できない（名前解決、TLS、切断など）。下位のエラー文を持つ
    Network(String),
    /// `REQUEST_TIMEOUT` 以内に応答が揃わなかった
    Timeout,
    /// 公開された Release が 1 つも無い（404）
    NoRelease,
    /// 認証なしの問い合わせ回数の上限に達した（403 / 429）
    RateLimited,
    /// それ以外の HTTP の失敗
    HttpStatus(u16),
    /// 応答を JSON として読めない
    InvalidResponse(String),
    /// タグを版として読めない
    InvalidTag(String),
    /// テスト用の環境変数で指定した Release の JSON のファイルを読めない
    LocalFile(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            UpdateError::Network(source) => i18n::update_network_failed(source),
            UpdateError::Timeout => Text::UpdateTimedOut.get().to_string(),
            UpdateError::NoRelease => Text::UpdateNoRelease.get().to_string(),
            UpdateError::RateLimited => Text::UpdateRateLimited.get().to_string(),
            UpdateError::HttpStatus(code) => i18n::update_http_status(*code),
            UpdateError::InvalidResponse(source) => i18n::update_invalid_response(source),
            UpdateError::InvalidTag(tag) => i18n::update_invalid_tag(tag),
            UpdateError::LocalFile(source) => i18n::update_local_file_failed(source),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for UpdateError {}

/// API の応答のうち使う項目だけ。知らない項目は読み飛ばす。
#[derive(Debug, Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Debug, Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

/// GitHub に最新の Release を問い合わせ、いまの版と比べる。
///
/// `overrides` はテスト用の環境変数で差し替えた版と問い合わせ先。通常は空。
///
/// **ブロックする。** 最大で `REQUEST_TIMEOUT` かかるので、UI スレッドから
/// 呼ばないこと（`app::update` が別スレッドで呼ぶ）。
pub fn check_latest_release(overrides: &CheckOverrides) -> Result<CheckOutcome, UpdateError> {
    let json = match overrides.source_or_default() {
        ReleaseSource::Http(url) => fetch_release_json(&url)?,
        ReleaseSource::File(path) => std::fs::read_to_string(&path)
            .map_err(|e| UpdateError::LocalFile(format!("{}: {}", path.display(), e)))?,
    };
    let release = parse_release_json(&json)?;
    evaluate_release(&overrides.current_version_or(current_version()), release)
}

/// Release の JSON を HTTP で取る。`url` は通常 `LATEST_RELEASE_API_URL`。
fn fetch_release_json(url: &str) -> Result<String, UpdateError> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .tls_config(tls_config())
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    debug!("最新の Release を問い合わせる: {}", url);
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(update_error_from)?;
    response
        .body_mut()
        .read_to_string()
        .map_err(update_error_from)
}

/// HTTP の TLS の設定。問い合わせと資産のダウンロード（`apply`）で共有する。
///
/// TLS は Windows の schannel。証明書は OS の証明書ストアで確かめる
/// （`PlatformVerifier` は native-tls では OS の既定のルートを使う指定）。
fn tls_config() -> ureq::tls::TlsConfig {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

    TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build()
}

fn update_error_from(error: ureq::Error) -> UpdateError {
    match error {
        ureq::Error::StatusCode(404) => UpdateError::NoRelease,
        ureq::Error::StatusCode(403 | 429) => UpdateError::RateLimited,
        ureq::Error::StatusCode(code) => UpdateError::HttpStatus(code),
        ureq::Error::Timeout(_) => UpdateError::Timeout,
        other => UpdateError::Network(other.to_string()),
    }
}

fn parse_release_json(json: &str) -> Result<ReleaseJson, UpdateError> {
    serde_json::from_str(json).map_err(|e| UpdateError::InvalidResponse(e.to_string()))
}

/// Release の内容といまの版から、更新があるかを決める。
fn evaluate_release(current: &Version, release: ReleaseJson) -> Result<CheckOutcome, UpdateError> {
    let latest = parse_version_tag(&release.tag_name)
        .ok_or_else(|| UpdateError::InvalidTag(release.tag_name.clone()))?;

    if release.draft || release.prerelease || !is_newer_stable(current, &latest) {
        return Ok(CheckOutcome::UpToDate);
    }

    Ok(CheckOutcome::Available(UpdateCheck {
        current: current.clone(),
        latest,
        tag: release.tag_name.trim().to_string(),
        release_url: release_page_url(&release.html_url).to_string(),
        assets: release
            .assets
            .into_iter()
            .map(|asset| ReleaseAsset {
                name: asset.name,
                download_url: asset.browser_download_url,
            })
            .collect(),
    }))
}

/// タグ（`v1.2.0`）を版として読む。先頭の `v` / `V` は無くてもよい。
///
/// 読めないものは `None`。前後の空白は落とす。
pub fn parse_version_tag(tag: &str) -> Option<Version> {
    let trimmed = tag.trim();
    let without_prefix = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    Version::parse(without_prefix).ok()
}

/// `latest` が `current` より新しい正式版か。
///
/// **pre-release（`1.2.0-rc.1` のようなタグ）は対象にしない。** `/latest` は
/// GitHub で pre-release に印を付けた Release を元から除くが、印を付け忘れた
/// ものまで勧めないよう、タグの形でも弾く。ダウングレードもしない。
pub fn is_newer_stable(current: &Version, latest: &Version) -> bool {
    latest.pre.is_empty() && latest > current
}

/// API が返したリリースページの URL を、開いてよい形のときだけ使う。
///
/// ブラウザへ渡す文字列なので、このリポジトリの Release 以外は開かない。
/// **頭が一致するだけでは許さない。** `.../releases/../../../他人/リポジトリ/...` は
/// ブラウザが `..` を畳んで別のリポジトリを開くため、`releases/` の後ろは
/// `tag/<タグ>` の形（タグは英数字と `.` `-` `_` だけで、`.` / `..` ではない）に限る。
fn release_page_url(html_url: &str) -> &str {
    let is_release_tag_page = html_url
        .strip_prefix(RELEASE_PAGE_PREFIX)
        .and_then(|rest| rest.strip_prefix("tag/"))
        .is_some_and(|tag| {
            !tag.is_empty()
                && tag != "."
                && tag != ".."
                && tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        });
    if is_release_tag_page {
        html_url
    } else {
        LATEST_RELEASE_PAGE_URL
    }
}

/// 設定の「この版は通知しない」が `latest` を指しているか。
///
/// 設定ファイルは手で書き換えられるので、`v` の有無や前後の空白の違いは
/// 同じ版として扱う。読めない値は、どの版も指していないものとする。
pub fn is_skipped(settings: &UpdateSettings, latest: &Version) -> bool {
    settings
        .skipped_version
        .as_deref()
        .and_then(parse_version_tag)
        .is_some_and(|skipped| &skipped == latest)
}

/// 起動時の確認で見つけた新しい版を、ダイアログで知らせるか。
///
/// 知らせないときも「その他」タブの「更新」の欄には出る。
pub fn should_notify_on_startup(settings: &UpdateSettings, latest: &Version) -> bool {
    settings.notify_on_startup && !is_skipped(settings, latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect("テスト用の版は読めなければならない")
    }

    fn release(tag: &str) -> ReleaseJson {
        ReleaseJson {
            tag_name: tag.to_string(),
            html_url: format!(
                "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/{tag}"
            ),
            draft: false,
            prerelease: false,
            assets: vec![AssetJson {
                name: format!("capturecard_viewer-{tag}-windows-x64.exe"),
                browser_download_url: "https://example.invalid/a.exe".to_string(),
            }],
        }
    }

    // ---- 版の比較 ----

    #[test]
    fn parse_version_tag_accepts_prefix_and_plain() {
        assert_eq!(parse_version_tag("v1.2.3"), Some(v("1.2.3")));
        assert_eq!(parse_version_tag("V1.2.3"), Some(v("1.2.3")));
        assert_eq!(parse_version_tag("1.2.3"), Some(v("1.2.3")));
        assert_eq!(parse_version_tag("  v1.2.3\n"), Some(v("1.2.3")));
    }

    #[test]
    fn parse_version_tag_rejects_malformed_strings() {
        assert_eq!(parse_version_tag(""), None);
        assert_eq!(parse_version_tag("v"), None);
        assert_eq!(parse_version_tag("latest"), None);
        assert_eq!(parse_version_tag("v1.2"), None);
        assert_eq!(parse_version_tag("vv1.2.3"), None);
        assert_eq!(parse_version_tag("v1.2.3.4"), None);
        assert_eq!(parse_version_tag("release-1.2.3"), None);
    }

    #[test]
    fn is_newer_stable_newer_version_returns_true() {
        assert!(is_newer_stable(&v("1.1.0"), &v("1.2.0")));
        assert!(is_newer_stable(&v("1.1.0"), &v("1.1.1")));
        assert!(is_newer_stable(&v("1.9.0"), &v("2.0.0")));
        // 桁で比べる（文字列の比較なら 1.10.0 < 1.9.0 になる）
        assert!(is_newer_stable(&v("1.9.0"), &v("1.10.0")));
    }

    #[test]
    fn is_newer_stable_same_version_returns_false() {
        assert!(!is_newer_stable(&v("1.1.0"), &v("1.1.0")));
    }

    #[test]
    fn is_newer_stable_older_version_returns_false() {
        // ダウングレードはしない
        assert!(!is_newer_stable(&v("1.2.0"), &v("1.1.9")));
    }

    #[test]
    fn is_newer_stable_prerelease_tag_returns_false() {
        assert!(!is_newer_stable(&v("1.1.0"), &v("1.2.0-rc.1")));
        assert!(!is_newer_stable(&v("1.1.0"), &v("2.0.0-beta")));
    }

    #[test]
    fn evaluate_release_newer_tag_is_available() {
        let outcome = evaluate_release(&v("1.1.0"), release("v1.2.0")).expect("読めるタグ");

        let CheckOutcome::Available(check) = outcome else {
            panic!("新しい版として扱われていない: {outcome:?}");
        };
        assert_eq!(check.current, v("1.1.0"));
        assert_eq!(check.latest, v("1.2.0"));
        assert_eq!(check.tag, "v1.2.0");
        assert_eq!(
            check.release_url,
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.2.0"
        );
        assert_eq!(check.assets.len(), 1);
        assert_eq!(
            check.assets[0].name,
            "capturecard_viewer-v1.2.0-windows-x64.exe"
        );
    }

    #[test]
    fn evaluate_release_same_or_older_tag_is_up_to_date() {
        assert_eq!(
            evaluate_release(&v("1.1.0"), release("v1.1.0")),
            Ok(CheckOutcome::UpToDate)
        );
        assert_eq!(
            evaluate_release(&v("1.1.0"), release("v1.0.7")),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn evaluate_release_flagged_draft_or_prerelease_is_up_to_date() {
        let mut prerelease = release("v1.2.0");
        prerelease.prerelease = true;
        assert_eq!(
            evaluate_release(&v("1.1.0"), prerelease),
            Ok(CheckOutcome::UpToDate)
        );

        let mut draft = release("v1.2.0");
        draft.draft = true;
        assert_eq!(
            evaluate_release(&v("1.1.0"), draft),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn evaluate_release_prerelease_tag_is_up_to_date() {
        assert_eq!(
            evaluate_release(&v("1.1.0"), release("v1.2.0-rc.1")),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn evaluate_release_malformed_tag_is_an_error() {
        assert_eq!(
            evaluate_release(&v("1.1.0"), release("nightly")),
            Err(UpdateError::InvalidTag("nightly".to_string()))
        );
    }

    #[test]
    fn evaluate_release_foreign_url_falls_back_to_latest_page() {
        // API の応答をそのままブラウザへ渡さない
        let mut foreign = release("v1.2.0");
        foreign.html_url = "https://example.invalid/phishing".to_string();

        let Ok(CheckOutcome::Available(check)) = evaluate_release(&v("1.1.0"), foreign) else {
            panic!("新しい版として扱われていない");
        };
        assert_eq!(check.release_url, LATEST_RELEASE_PAGE_URL);
    }

    #[test]
    fn release_page_url_accepts_only_this_repository_tag_pages() {
        let ok = "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.2.0-rc_1";
        assert_eq!(release_page_url(ok), ok);

        for rejected in [
            // `..` をブラウザが畳むと別のリポジトリになる
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/../../../attacker/evil/releases/tag/v1.2.0",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/..",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.2.0/../../x",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/%2e%2e",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/a.exe",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer_evil/releases/tag/v1.2.0",
            "",
        ] {
            assert_eq!(
                release_page_url(rejected),
                LATEST_RELEASE_PAGE_URL,
                "{rejected} は弾かれなければならない"
            );
        }
    }

    #[test]
    fn parse_release_json_reads_the_api_shape() {
        // GitHub の応答の抜粋。使わない項目が混ざっていても読めること
        let json = r#"{
            "url": "https://api.github.com/repos/x/y/releases/1",
            "tag_name": "v1.2.0",
            "html_url": "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.2.0",
            "draft": false,
            "prerelease": false,
            "body": null,
            "assets": [
                {"name": "SHA256SUMS.txt", "size": 200,
                 "browser_download_url": "https://github.com/x/y/releases/download/v1.2.0/SHA256SUMS.txt"}
            ]
        }"#;

        let parsed = parse_release_json(json).expect("API の形は読めなければならない");

        assert_eq!(parsed.tag_name, "v1.2.0");
        assert_eq!(parsed.assets.len(), 1);
        assert_eq!(parsed.assets[0].name, "SHA256SUMS.txt");
    }

    #[test]
    fn parse_release_json_broken_json_is_an_error() {
        assert!(matches!(
            parse_release_json("<html>rate limited</html>"),
            Err(UpdateError::InvalidResponse(_))
        ));
        // tag_name が無いものも読めない扱い
        assert!(matches!(
            parse_release_json("{}"),
            Err(UpdateError::InvalidResponse(_))
        ));
    }

    // ---- 通知の判定 ----

    fn update_settings(notify: bool, skipped: Option<&str>) -> UpdateSettings {
        UpdateSettings {
            check_on_startup: true,
            notify_on_startup: notify,
            skipped_version: skipped.map(str::to_string),
        }
    }

    #[test]
    fn should_notify_on_startup_default_settings_returns_true() {
        assert!(should_notify_on_startup(
            &UpdateSettings::default(),
            &v("1.2.0")
        ));
    }

    #[test]
    fn should_notify_on_startup_notify_off_returns_false() {
        assert!(!should_notify_on_startup(
            &update_settings(false, None),
            &v("1.2.0")
        ));
    }

    #[test]
    fn should_notify_on_startup_skipped_same_version_returns_false() {
        assert!(!should_notify_on_startup(
            &update_settings(true, Some("1.2.0")),
            &v("1.2.0")
        ));
        // 手で書いた `v` 付きや空白入りも同じ版として扱う
        assert!(!should_notify_on_startup(
            &update_settings(true, Some(" v1.2.0 ")),
            &v("1.2.0")
        ));
    }

    #[test]
    fn should_notify_on_startup_skipped_other_version_returns_true() {
        // 飛ばしたのより新しい版が出たら、また知らせる
        assert!(should_notify_on_startup(
            &update_settings(true, Some("1.2.0")),
            &v("1.3.0")
        ));
    }

    #[test]
    fn should_notify_on_startup_unreadable_skipped_version_returns_true() {
        assert!(should_notify_on_startup(
            &update_settings(true, Some("よく分からない値")),
            &v("1.2.0")
        ));
    }

    #[test]
    fn update_status_release_url_prefers_found_release() {
        assert_eq!(
            UpdateStatus::UpToDate.release_url(),
            LATEST_RELEASE_PAGE_URL
        );
        let Ok(CheckOutcome::Available(check)) = evaluate_release(&v("1.1.0"), release("v1.2.0"))
        else {
            panic!("新しい版として扱われていない");
        };
        assert_eq!(
            UpdateStatus::Available(check).release_url(),
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.2.0"
        );
    }

    #[test]
    fn update_error_display_follows_the_language() {
        assert_eq!(
            UpdateError::HttpStatus(500).to_string(),
            "GitHub が HTTP 500 を返した"
        );
        assert_eq!(
            i18n::with_language(i18n::Language::English, || UpdateError::InvalidTag(
                "nightly".to_string()
            )
            .to_string()),
            "Cannot read the latest release tag 'nightly' as a version"
        );
    }

    // ---- テスト用の環境変数で差し替えた確認 ----

    #[test]
    fn check_latest_release_reads_a_local_file_with_an_older_current_version() {
        // CAPTURECARD_VIEWER_UPDATE_API_URL=file://... と
        // CAPTURECARD_VIEWER_UPDATE_CURRENT_VERSION=1.0.0 を指定したときの流れ
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなければならない");
        let path = dir.path().join("latest.json");
        std::fs::write(
            &path,
            r#"{"tag_name": "v1.1.0",
                "html_url": "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/tag/v1.1.0",
                "body": "概要。\n### 追加\n- a", "assets": []}"#,
        )
        .expect("テスト用の JSON を書けなければならない");
        let overrides = CheckOverrides {
            current_version: Some(v("1.0.0")),
            source: Some(ReleaseSource::File(path)),
        };

        let Ok(CheckOutcome::Available(check)) = check_latest_release(&overrides) else {
            panic!("差し替えた版より新しい版として扱われていない");
        };
        assert_eq!(check.current, v("1.0.0"));
        assert_eq!(check.latest, v("1.1.0"));
    }

    #[test]
    fn check_latest_release_missing_local_file_is_an_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れなければならない");
        let overrides = CheckOverrides {
            current_version: None,
            source: Some(ReleaseSource::File(dir.path().join("missing.json"))),
        };

        assert!(matches!(
            check_latest_release(&overrides),
            Err(UpdateError::LocalFile(_))
        ));
    }

    // ---- ネットワーク ----

    #[test]
    #[ignore = "ネットワーク（api.github.com への HTTPS）が必要"]
    fn check_latest_release_reaches_github() {
        // 実行: cargo test check_latest_release_reaches_github -- --ignored
        // 公開済みの Release があれば、少なくとも読めて比べられること
        let outcome = check_latest_release(&CheckOverrides::default())
            .expect("GitHub に問い合わせられなければならない");
        println!("{outcome:?}");
    }
}
