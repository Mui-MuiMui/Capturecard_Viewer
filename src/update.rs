//! 更新の確認。GitHub の Release から最新の版を取り、いまの版と比べる。
//!
//! ここにあるのは問い合わせ（`check_latest_release`）と、その結果を判断する
//! 純粋関数だけ。スレッドを起こして結果を画面へ渡すのは `app::update`、
//! 通知ダイアログの描画は `ui::update_dialog`（`docs/design/update.md`）。
//!
//! **この段階では何もダウンロードしない。** 「更新する」はリリースページを
//! 開くところまで。資産の取得・照合・差し替えは次の段階で足す。

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
/// 起動を止めないよう別スレッドで行うが、終了時にはこのスレッドを join する。
/// 長くすると、起動直後に閉じたときの待ちがそのぶん延びる。
/// 失敗の文言（`Text::UpdateTimedOut`）にも秒数を書いてあるので、変えたら合わせる。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub の API は `User-Agent` の無い要求を拒否する。
const USER_AGENT: &str = concat!("capturecard_viewer/", env!("CARGO_PKG_VERSION"));

/// Release の本文を要約するときに、`### ` の見出しが無い（または見出しより
/// 前が空の）場合に使う行数。
const NOTES_FALLBACK_LINES: usize = 5;

/// 実行中の版。`Cargo.toml` の `version` から取る（`docs/BUILD.md` の「バージョン番号」）。
pub fn current_version() -> Version {
    // Cargo は semver として読める版しか受け付けないので、ここで失敗することは無い。
    // 万一読めなくても起動は止めず、どの版よりも古い扱いにする
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// Release に添付された資産 1 つ。
///
/// この段階では使わず、ログに出すだけ。次の段階（ダウンロードと差し替え）で
/// `capturecard_viewer-<tag>-windows-x64.exe` と `SHA256SUMS.txt` を
/// ここから引く（`docs/RELEASE.md` の「配布物」）。
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
    /// リリースノートの要約（`summarize_notes`）。空のこともある
    pub notes_summary: String,
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
    // 本文の無い Release では null が来る
    #[serde(default)]
    body: Option<String>,
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
/// **ブロックする。** 最大で `REQUEST_TIMEOUT` かかるので、UI スレッドから
/// 呼ばないこと（`app::update` が別スレッドで呼ぶ）。
pub fn check_latest_release() -> Result<CheckOutcome, UpdateError> {
    let json = fetch_latest_release_json()?;
    let release = parse_release_json(&json)?;
    evaluate_release(&current_version(), release)
}

/// 最新の Release の JSON を取る。
fn fetch_latest_release_json() -> Result<String, UpdateError> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

    // TLS は Windows の schannel。証明書は OS の証明書ストアで確かめる
    // （`PlatformVerifier` は native-tls では OS の既定のルートを使う指定）
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .tls_config(tls)
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    debug!("最新の Release を問い合わせる: {}", LATEST_RELEASE_API_URL);
    let mut response = agent
        .get(LATEST_RELEASE_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(update_error_from)?;
    response
        .body_mut()
        .read_to_string()
        .map_err(update_error_from)
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
        notes_summary: summarize_notes(release.body.as_deref().unwrap_or("")),
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
fn release_page_url(html_url: &str) -> &str {
    if html_url.starts_with(RELEASE_PAGE_PREFIX) {
        html_url
    } else {
        LATEST_RELEASE_PAGE_URL
    }
}

/// リリースノートの本文から、通知ダイアログに出す要約を作る。
///
/// 最初の `### ` の見出しより前を採る。このリポジトリの Release の本文は
/// 「概要の段落 → `### 追加` などの見出しごとの一覧」の順（`docs/RELEASE.md`）。
/// 見出しが無い、または見出しより前が空なら、先頭の `NOTES_FALLBACK_LINES` 行。
///
/// HTML のタグだけの行（折りたたみの `<details>` など）と空行は落とす。
pub fn summarize_notes(body: &str) -> String {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && !is_html_tag_line(line))
        .collect();

    let before_heading: Vec<&str> = lines
        .iter()
        .take_while(|line| !line.trim_start().starts_with("### "))
        .copied()
        .collect();

    // 見出しが無ければ、見出しより前 = 全体になる
    let has_heading = before_heading.len() < lines.len();
    let picked = if has_heading && !before_heading.is_empty() {
        before_heading
    } else {
        lines.into_iter().take(NOTES_FALLBACK_LINES).collect()
    };
    picked.join("\n")
}

/// `<details>` や `<summary>…</summary>` のように、行がタグだけでできているか。
fn is_html_tag_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('<') && trimmed.ends_with('>')
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
            body: Some("概要の段落。\n\n### 追加\n\n- 何か".to_string()),
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
        assert_eq!(check.notes_summary, "概要の段落。");
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
        assert_eq!(parsed.body, None);
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

    // ---- 要約の切り出し ----

    #[test]
    fn summarize_notes_takes_text_before_first_heading() {
        let body = "1 行目。\n2 行目。\n\n### 追加\n\n- 項目\n### 修正\n- 項目";
        assert_eq!(summarize_notes(body), "1 行目。\n2 行目。");
    }

    #[test]
    fn summarize_notes_drops_html_tag_lines() {
        // 実際の Release（v1.1.0）の形。概要のあとに折りたたみが始まる
        let body = "概要。\r\n\r\n<details>\r\n<summary>変更の一覧</summary>\r\n\r\n### 追加\r\n\r\n- 項目";
        assert_eq!(summarize_notes(body), "概要。");
    }

    #[test]
    fn summarize_notes_without_heading_takes_first_five_lines() {
        let body = "1\n2\n3\n4\n5\n6\n7";
        assert_eq!(summarize_notes(body), "1\n2\n3\n4\n5");
    }

    #[test]
    fn summarize_notes_heading_first_takes_first_five_lines() {
        // 見出しより前が空なら、見出しを含めた先頭 5 行
        let body = "### 追加\n\n- a\n- b\n- c\n- d\n- e";
        assert_eq!(summarize_notes(body), "### 追加\n- a\n- b\n- c\n- d");
    }

    #[test]
    fn summarize_notes_empty_body_returns_empty() {
        assert_eq!(summarize_notes(""), "");
        assert_eq!(summarize_notes("\n\n  \n"), "");
    }

    #[test]
    fn summarize_notes_does_not_treat_deeper_heading_as_boundary() {
        // `#### ` は `### ` で始まらないので境界にしない
        let body = "概要。\n#### 小見出し\n本文\n### 追加\n- a";
        assert_eq!(summarize_notes(body), "概要。\n#### 小見出し\n本文");
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

    // ---- ネットワーク ----

    #[test]
    #[ignore = "ネットワーク（api.github.com への HTTPS）が必要"]
    fn check_latest_release_reaches_github() {
        // 実行: cargo test check_latest_release_reaches_github -- --ignored
        // 公開済みの Release があれば、少なくとも読めて比べられること
        let outcome = check_latest_release().expect("GitHub に問い合わせられなければならない");
        println!("{outcome:?}");
    }
}
