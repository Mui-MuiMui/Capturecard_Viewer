//! 更新の適用。新しい版の exe をダウンロードし、`SHA256SUMS.txt` と照合して、
//! 実行中の exe と差し替える（`docs/design/update.md` の「適用」）。
//!
//! ここにあるのは、別スレッドで走らせる本体（`run_apply`）と、その部品の
//! 純粋関数・ファイル操作だけ。スレッドを起こして進捗を画面へ渡すのは
//! `app::update`、新しい exe の起動は終了時の `on_exit`（`relaunch_updated_exe`）。
//!
//! **元の exe を壊す経路を作らない。** ダウンロードは exe と同じフォルダの
//! `<exe の名前>.new` へ落とし、照合が済むまで元の exe には触らない。
//! 差し替え（`swap_in`）が途中で失敗したら、動かした元の exe を戻して `.new` を消す。
//!
//! 差し替えとその戻し方、exe の隣の一時名（`ExePaths`）は `swap.rs`、
//! `SHA256SUMS.txt` の読み方と照合は `checksum.rs`。

pub use super::swap::{remove_leftovers, roll_back, ExePaths};

use super::checksum::{checksum_matches, find_checksum, to_hex};
use super::overrides::{file_url_to_path, strip_prefix_ignore_case};
use super::swap::{ensure_writable, remove_if_exists, swap_in};
use super::{tls_config, ReleaseAsset, UpdateCheck, USER_AGENT};
use crate::i18n::{self, Text};
use log::{debug, info, warn};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// 照合に使う資産の名前（`docs/RELEASE.md` の「配布物」）。
pub const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// 資産の URL として受け付ける頭。テスト用の問い合わせ先を使っていないときは、
/// `<頭><タグ>/<資産名>` と完全に一致するものしか落とさない。
const DOWNLOAD_URL_PREFIX: &str =
    "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/";

/// exe の大きさの上限。配布物は数十 MB なので、桁違いに大きいものは
/// 取り違えとみなしてディスクを埋める前に止める。
const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;

/// `SHA256SUMS.txt` の大きさの上限。数行のテキストなので十分に大きい。
const MAX_CHECKSUMS_BYTES: u64 = 64 * 1024;

/// 接続（TLS のハンドシェイクを含む）と、応答のヘッダーが揃うまでの上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 本文を受け取り終えるまでの上限。遅い回線でも数十 MB が落ちきる長さにする。
/// キャンセルは読み取りの合間に見るので、ここより早く止められる。
const BODY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 1 回に読む大きさ。キャンセルと進捗はこの単位で見る。
const CHUNK_BYTES: usize = 64 * 1024;

/// 大きさが分からないときに進捗を知らせる間隔。
const PROGRESS_STEP_BYTES: u64 = 256 * 1024;

/// Release に添付する exe の名前。`tag` は Release のタグそのまま（`v1.2.0`）。
pub fn exe_asset_name(tag: &str) -> String {
    format!("capturecard_viewer-{tag}-windows-x64.exe")
}

/// 資産をどこから取るか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetSource {
    /// HTTP で取る
    Http(String),
    /// ファイルをそのまま読む。テスト用の問い合わせ先の JSON に `file://` で
    /// 書いたときだけ（`CAPTURECARD_VIEWER_UPDATE_API_URL`）
    File(PathBuf),
}

/// 更新に使う 2 つの資産。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPlan {
    /// exe の資産名。`SHA256SUMS.txt` の行をこの名前で引く
    pub exe_name: String,
    /// exe の取り先
    pub exe: AssetSource,
    /// `SHA256SUMS.txt` の取り先
    pub checksums: AssetSource,
}

impl ApplyPlan {
    /// 見つかった版の資産から、何をどこから落とすかを決める。
    ///
    /// `allow_any_source` はテスト用の問い合わせ先を使っているとき。偽なら
    /// 資産の URL がこのリポジトリの Release のものでなければ落とさない。
    ///
    /// **資産が無い（1.1.0 以前の Release）なら `ApplyError::NoAssets`。**
    /// 自動では更新できないので、リリースページから手で更新してもらう。
    pub fn from_check(check: &UpdateCheck, allow_any_source: bool) -> Result<Self, ApplyError> {
        let exe_name = exe_asset_name(&check.tag);
        let find = |name: &str| check.assets.iter().find(|asset| asset.name == name);
        let (Some(exe), Some(checksums)) = (find(&exe_name), find(CHECKSUMS_ASSET_NAME)) else {
            return Err(ApplyError::NoAssets);
        };
        Ok(Self {
            exe: asset_source(exe, &check.tag, allow_any_source)?,
            checksums: asset_source(checksums, &check.tag, allow_any_source)?,
            exe_name,
        })
    }
}

/// 資産の URL を、落としてよい取り先にする。
///
/// 通常は `https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/<タグ>/<資産名>`
/// と完全に一致するものだけ。頭の一致で許すと `..` を含む URL で別の場所を指せる。
/// テスト用の問い合わせ先を使っているときは `http://` / `https://` / `file://` を
/// そのまま受け付ける（ローカルのファイルやテスト用のリポジトリを指すため）。
fn asset_source(
    asset: &ReleaseAsset,
    tag: &str,
    allow_any_source: bool,
) -> Result<AssetSource, ApplyError> {
    let url = asset.download_url.trim();
    if allow_any_source {
        if let Some(rest) = strip_prefix_ignore_case(url, "file://") {
            if let Some(path) = file_url_to_path(rest) {
                return Ok(AssetSource::File(path));
            }
        } else if strip_prefix_ignore_case(url, "https://").is_some()
            || strip_prefix_ignore_case(url, "http://").is_some()
        {
            return Ok(AssetSource::Http(url.to_string()));
        }
        return Err(ApplyError::UnexpectedAssetUrl(url.to_string()));
    }

    let expected = format!("{DOWNLOAD_URL_PREFIX}{tag}/{}", asset.name);
    if url == expected {
        Ok(AssetSource::Http(expected))
    } else {
        Err(ApplyError::UnexpectedAssetUrl(url.to_string()))
    }
}

/// 更新に失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// Release に自動更新用の資産（exe と `SHA256SUMS.txt`）が無い
    NoAssets,
    /// 資産の URL がこのリポジトリの Release のものではない
    UnexpectedAssetUrl(String),
    /// 実行中の exe の場所が分からない
    ExePath(String),
    /// exe のフォルダに書き込めない。持っているのはログ用の詳細
    NotWritable(String),
    /// 接続できない、または受け取りが途中で止まった
    Network(String),
    /// 上限の時間内に受け取れなかった
    Timeout,
    /// HTTP の失敗
    HttpStatus(u16),
    /// 上限（`MAX_EXE_BYTES` / `MAX_CHECKSUMS_BYTES`）より大きい
    TooLarge,
    /// `SHA256SUMS.txt` に exe の行が無い
    ChecksumMissing,
    /// ダウンロードした exe の SHA-256 が `SHA256SUMS.txt` と合わない
    ChecksumMismatch,
    /// ダウンロード先（`.new`）を書けない、または資産のファイルを読めない
    File(String),
    /// exe を置き換えられない。元の exe は戻してある
    Replace(String),
    /// キャンセルされた。画面には出さない
    Cancelled,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ApplyError::NoAssets => Text::UpdateNoAssets.get().to_string(),
            ApplyError::UnexpectedAssetUrl(_) => Text::UpdateUnexpectedAssetUrl.get().to_string(),
            ApplyError::ExePath(source) => i18n::update_exe_path_failed(source),
            ApplyError::NotWritable(_) => Text::UpdateNotWritable.get().to_string(),
            ApplyError::Network(source) => i18n::update_download_failed(source),
            ApplyError::Timeout => Text::UpdateDownloadTimedOut.get().to_string(),
            ApplyError::HttpStatus(code) => i18n::update_download_http_status(*code),
            ApplyError::TooLarge => Text::UpdateDownloadTooLarge.get().to_string(),
            ApplyError::ChecksumMissing => Text::UpdateChecksumMissing.get().to_string(),
            ApplyError::ChecksumMismatch => Text::UpdateChecksumMismatch.get().to_string(),
            ApplyError::File(source) => i18n::update_file_failed(source),
            ApplyError::Replace(source) => i18n::update_replace_failed(source),
            ApplyError::Cancelled => Text::UpdateCancelled.get().to_string(),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for ApplyError {}

/// 別スレッドから画面へ返す進み具合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyProgress {
    /// 書き込めるかの確認と `SHA256SUMS.txt` の取得
    Preparing,
    /// exe を受け取っている。`total` は大きさが分からなければ `None`
    Downloading { downloaded: u64, total: Option<u64> },
    /// 照合が済み、置き換えている
    Installing,
}

impl ApplyProgress {
    /// 受け取った割合（0〜100）。大きさが分からない段階なら `None`。
    pub fn percent(self) -> Option<u8> {
        match self {
            ApplyProgress::Downloading {
                downloaded,
                total: Some(total),
            } if total > 0 => Some((downloaded.min(total) * 100 / total) as u8),
            _ => None,
        }
    }
}

/// `check` の版へ更新する。**ブロックする**ので UI スレッドから呼ばない。
///
/// 1. `paths` のフォルダに書けるかを確かめる（書けなければ何も落とさない）
/// 2. 資産を選ぶ（`ApplyPlan::from_check`。`allow_any_source` はそちらを参照）
/// 3. `SHA256SUMS.txt` を落とし、exe の行の hash を取る
/// 4. exe を `.new` へ落としながら SHA-256 を計算し、照合する
/// 5. 実行中の exe を `.old` へ、`.new` を元の名前へ改名する（`swap_in`）
///
/// 書けるかを資産より先に見るのは、書けないフォルダ（Program Files など）は
/// どの版でも自動更新できず、exe を移せば直る、という伝えるべきことだから。
///
/// 失敗・キャンセルのどちらでも `.new` は消し、元の exe は元の名前のまま残す。
/// 成功したら、新しい exe は `paths.exe` にあり、起動は呼び出し側が行う。
/// キャンセルは読み取りの合間に見る。差し替えの直前に `ApplyControl::begin_swap` で
/// キャンセルと取り合い、先にキャンセルされていれば差し替えない。
pub fn run_apply(
    check: &UpdateCheck,
    allow_any_source: bool,
    paths: &ExePaths,
    cancel: &ApplyControl,
    progress: &mut dyn FnMut(ApplyProgress),
) -> Result<(), ApplyError> {
    progress(ApplyProgress::Preparing);
    ensure_writable(paths.dir())
        .map_err(|e| ApplyError::NotWritable(format!("{}: {}", paths.dir().display(), e)))?;
    let plan = &ApplyPlan::from_check(check, allow_any_source)?;

    let sums = fetch_text(&plan.checksums, MAX_CHECKSUMS_BYTES)?;
    let expected = find_checksum(&sums, &plan.exe_name)
        .ok_or(ApplyError::ChecksumMissing)?
        .to_string();
    check_cancelled(cancel)?;

    let result = download_and_verify(plan, paths, &expected, cancel, progress);
    if result.is_err() {
        remove_if_exists(&paths.new);
        return result;
    }

    progress(ApplyProgress::Installing);
    if !cancel.begin_swap() {
        remove_if_exists(&paths.new);
        return Err(ApplyError::Cancelled);
    }
    swap_in(paths)
}

fn download_and_verify(
    plan: &ApplyPlan,
    paths: &ExePaths,
    expected: &str,
    cancel: &ApplyControl,
    progress: &mut dyn FnMut(ApplyProgress),
) -> Result<(), ApplyError> {
    let (mut reader, total) = open_source(&plan.exe)?;
    if total.is_some_and(|total| total > MAX_EXE_BYTES) {
        return Err(ApplyError::TooLarge);
    }
    info!(
        "更新の exe を落とす: {:?} → {}（{:?} バイト）",
        plan.exe,
        paths.new.display(),
        total
    );

    let mut file = File::create(&paths.new).map_err(|e| file_error(&paths.new, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut downloaded: u64 = 0;
    let mut reported: Option<ApplyProgress> = None;
    loop {
        check_cancelled(cancel)?;
        let read = reader.read(&mut buffer).map_err(read_error(&plan.exe))?;
        if read == 0 {
            break;
        }
        downloaded += read as u64;
        if downloaded > MAX_EXE_BYTES {
            return Err(ApplyError::TooLarge);
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|e| file_error(&paths.new, e))?;

        let now = ApplyProgress::Downloading { downloaded, total };
        if should_report(reported, now) {
            progress(now);
            reported = Some(now);
        }
    }
    // 改名する前にディスクへ書き切る。途中で電源が落ちても、
    // 照合を通った中身が元の名前に来るようにする
    file.sync_all().map_err(|e| file_error(&paths.new, e))?;
    drop(file);

    let actual = hasher.finalize();
    if !checksum_matches(expected, actual.as_slice()) {
        warn!(
            "更新の exe の SHA-256 が合わない（期待: {}、実際: {}）",
            expected,
            to_hex(actual.as_slice())
        );
        return Err(ApplyError::ChecksumMismatch);
    }
    info!("更新の exe を照合した（{} バイト）", downloaded);
    Ok(())
}

/// 進捗を画面へ知らせるか。割合が 1 つ進んだとき（大きさが分からなければ
/// `PROGRESS_STEP_BYTES` ごと）だけにして、チャネルを細かい通知で埋めない。
fn should_report(last: Option<ApplyProgress>, now: ApplyProgress) -> bool {
    let Some(last) = last else {
        return true;
    };
    match (last.percent(), now.percent()) {
        (Some(last), Some(now)) => now > last,
        _ => match (last, now) {
            (
                ApplyProgress::Downloading { downloaded: a, .. },
                ApplyProgress::Downloading { downloaded: b, .. },
            ) => b / PROGRESS_STEP_BYTES > a / PROGRESS_STEP_BYTES,
            _ => true,
        },
    }
}

fn check_cancelled(control: &ApplyControl) -> Result<(), ApplyError> {
    if control.is_cancelled() {
        Err(ApplyError::Cancelled)
    } else {
        Ok(())
    }
}

/// 更新のスレッドと UI スレッドで共有する、キャンセルと差し替えの取り合い。
///
/// キャンセルと差し替えの開始は、どちらか先に来た方だけが通る（`compare_exchange`）。
/// **差し替えを始めたら、もうキャンセルできない。** キャンセルが通らなかった側
/// （`on_exit`）は差し替えが終わるのを待つ。実行中の exe を `.old` へ動かしてから
/// `.new` を元の名前へ置くまでの間にプロセスが終わると、元の名前に exe が
/// 1 つも無くなるため。ダウンロードの最中はキャンセルが通るので、待たない。
///
/// `Mutex` にしないのは、差し替え（ファイルの改名）の間ロックを握ることになるため
/// （`GUARDRAIL.md`）。
#[derive(Debug, Default)]
pub struct ApplyControl {
    state: AtomicU8,
}

const CONTROL_RUNNING: u8 = 0;
const CONTROL_CANCELLED: u8 = 1;
const CONTROL_SWAPPING: u8 = 2;

impl ApplyControl {
    /// キャンセルする。差し替えを始める前なら `true`（スレッドは止まり `.new` を消す）。
    /// もう差し替えを始めていれば `false` で、呼び出し側は終わるのを待つ。
    pub fn cancel(&self) -> bool {
        match self.state.compare_exchange(
            CONTROL_RUNNING,
            CONTROL_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(current) => current == CONTROL_CANCELLED,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CONTROL_CANCELLED
    }

    /// 差し替えを始める。先にキャンセルされていれば `false`。
    fn begin_swap(&self) -> bool {
        self.state
            .compare_exchange(
                CONTROL_RUNNING,
                CONTROL_SWAPPING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// 資産を読み始める。大きさが分かればそれも返す。
fn open_source(source: &AssetSource) -> Result<(Box<dyn Read>, Option<u64>), ApplyError> {
    match source {
        AssetSource::File(path) => {
            let file = File::open(path).map_err(|e| file_error(path, e))?;
            let len = file.metadata().ok().map(|m| m.len());
            Ok((Box::new(file), len))
        }
        AssetSource::Http(url) => {
            let config = ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_recv_response(Some(CONNECT_TIMEOUT))
                .timeout_recv_body(Some(BODY_TIMEOUT))
                .tls_config(tls_config())
                .user_agent(USER_AGENT)
                .build();
            let agent = ureq::Agent::new_with_config(config);
            debug!("更新の資産を取る: {}", url);
            // GitHub の資産の URL は別のホストへのリダイレクトを返す。ureq が辿る
            let response = agent.get(url).call().map_err(download_error_from)?;
            let body = response.into_body();
            let len = body.content_length();
            Ok((Box::new(body.into_reader()), len))
        }
    }
}

/// 小さなテキストの資産（`SHA256SUMS.txt`）を読む。
fn fetch_text(source: &AssetSource, limit: u64) -> Result<String, ApplyError> {
    let (reader, total) = open_source(source)?;
    if total.is_some_and(|total| total > limit) {
        return Err(ApplyError::TooLarge);
    }
    let mut bytes = Vec::new();
    // 上限より 1 バイトだけ多く読み、読めてしまったら大きすぎる
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error(source))?;
    if bytes.len() as u64 > limit {
        return Err(ApplyError::TooLarge);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn download_error_from(error: ureq::Error) -> ApplyError {
    match error {
        ureq::Error::StatusCode(code) => ApplyError::HttpStatus(code),
        ureq::Error::Timeout(_) => ApplyError::Timeout,
        other => ApplyError::Network(other.to_string()),
    }
}

fn read_error(source: &AssetSource) -> impl Fn(io::Error) -> ApplyError + '_ {
    move |error| match source {
        AssetSource::File(path) => file_error(path, error),
        AssetSource::Http(_) if error.kind() == io::ErrorKind::TimedOut => ApplyError::Timeout,
        AssetSource::Http(_) => ApplyError::Network(error.to_string()),
    }
}

fn file_error(path: &Path, error: io::Error) -> ApplyError {
    ApplyError::File(format!("{}: {}", path.display(), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use std::fs;

    fn check_with_assets(tag: &str, assets: &[(&str, &str)]) -> UpdateCheck {
        UpdateCheck {
            current: Version::new(1, 0, 0),
            latest: Version::new(1, 2, 0),
            tag: tag.to_string(),
            release_url: String::new(),
            assets: assets
                .iter()
                .map(|(name, url)| ReleaseAsset {
                    name: name.to_string(),
                    download_url: url.to_string(),
                })
                .collect(),
        }
    }

    const EXE_URL: &str = "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/capturecard_viewer-v1.2.0-windows-x64.exe";
    const SUMS_URL: &str =
        "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/SHA256SUMS.txt";

    // "hello" の SHA-256
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    // ---- 資産の選び方 ----

    #[test]
    fn exe_asset_name_follows_the_release_naming() {
        assert_eq!(
            exe_asset_name("v1.2.0"),
            "capturecard_viewer-v1.2.0-windows-x64.exe"
        );
    }

    #[test]
    fn apply_plan_from_check_picks_exe_and_checksums() {
        let check = check_with_assets(
            "v1.2.0",
            &[
                (
                    "capturecard_viewer-v1.2.0-windows-x64.zip",
                    "https://x.invalid/zip",
                ),
                ("capturecard_viewer-v1.2.0-windows-x64.exe", EXE_URL),
                ("SHA256SUMS.txt", SUMS_URL),
            ],
        );

        let plan = ApplyPlan::from_check(&check, false).expect("資産は揃っている");

        assert_eq!(plan.exe_name, "capturecard_viewer-v1.2.0-windows-x64.exe");
        assert_eq!(plan.exe, AssetSource::Http(EXE_URL.to_string()));
        assert_eq!(plan.checksums, AssetSource::Http(SUMS_URL.to_string()));
    }

    #[test]
    fn apply_plan_from_check_without_assets_is_no_assets() {
        // 1.1.0 以前の Release は zip だけ
        let zip_only = check_with_assets(
            "v1.1.0",
            &[(
                "capturecard_viewer-v1.1.0-windows-x64.zip",
                "https://x.invalid/zip",
            )],
        );
        let exe_only = check_with_assets(
            "v1.2.0",
            &[("capturecard_viewer-v1.2.0-windows-x64.exe", EXE_URL)],
        );
        let sums_only = check_with_assets("v1.2.0", &[("SHA256SUMS.txt", SUMS_URL)]);

        for check in [zip_only, exe_only, sums_only] {
            assert_eq!(
                ApplyPlan::from_check(&check, false),
                Err(ApplyError::NoAssets)
            );
        }
    }

    #[test]
    fn apply_plan_from_check_rejects_foreign_urls() {
        for exe_url in [
            "https://example.invalid/capturecard_viewer-v1.2.0-windows-x64.exe",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/../../../../attacker/x/releases/download/v1.2.0/capturecard_viewer-v1.2.0-windows-x64.exe",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.1.0/capturecard_viewer-v1.2.0-windows-x64.exe",
            "http://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/capturecard_viewer-v1.2.0-windows-x64.exe",
            "file:///C:/work/capturecard_viewer-v1.2.0-windows-x64.exe",
        ] {
            let check = check_with_assets(
                "v1.2.0",
                &[
                    ("capturecard_viewer-v1.2.0-windows-x64.exe", exe_url),
                    ("SHA256SUMS.txt", SUMS_URL),
                ],
            );
            assert!(
                matches!(
                    ApplyPlan::from_check(&check, false),
                    Err(ApplyError::UnexpectedAssetUrl(_))
                ),
                "{exe_url} は弾かれなければならない"
            );
        }
    }

    #[test]
    fn apply_plan_from_check_with_test_source_accepts_local_files() {
        let check = check_with_assets(
            "v9.9.9",
            &[
                (
                    "capturecard_viewer-v9.9.9-windows-x64.exe",
                    "file:///C:/work/new.exe",
                ),
                ("SHA256SUMS.txt", "http://127.0.0.1:8000/SHA256SUMS.txt"),
            ],
        );

        let plan = ApplyPlan::from_check(&check, true).expect("テスト用の取り先は受け付ける");

        assert_eq!(
            plan.exe,
            AssetSource::File(PathBuf::from("C:/work/new.exe"))
        );
        assert_eq!(
            plan.checksums,
            AssetSource::Http("http://127.0.0.1:8000/SHA256SUMS.txt".to_string())
        );
    }

    #[test]
    fn apply_plan_from_check_with_test_source_rejects_unknown_schemes() {
        let check = check_with_assets(
            "v9.9.9",
            &[
                (
                    "capturecard_viewer-v9.9.9-windows-x64.exe",
                    "ftp://x.invalid/a.exe",
                ),
                ("SHA256SUMS.txt", "file://"),
            ],
        );

        assert!(matches!(
            ApplyPlan::from_check(&check, true),
            Err(ApplyError::UnexpectedAssetUrl(_))
        ));
    }

    // ---- 進捗 ----

    #[test]
    fn apply_progress_percent_is_clamped_and_needs_a_size() {
        let downloading = |downloaded, total| ApplyProgress::Downloading { downloaded, total };

        assert_eq!(downloading(0, Some(200)).percent(), Some(0));
        assert_eq!(downloading(199, Some(200)).percent(), Some(99));
        assert_eq!(downloading(200, Some(200)).percent(), Some(100));
        assert_eq!(downloading(300, Some(200)).percent(), Some(100));
        assert_eq!(downloading(10, Some(0)).percent(), None);
        assert_eq!(downloading(10, None).percent(), None);
        assert_eq!(ApplyProgress::Installing.percent(), None);
    }

    #[test]
    fn should_report_only_when_percent_or_step_advances() {
        let downloading = |downloaded, total| ApplyProgress::Downloading { downloaded, total };

        assert!(should_report(None, downloading(0, Some(1000))));
        assert!(!should_report(
            Some(downloading(10, Some(1000))),
            downloading(19, Some(1000))
        ));
        assert!(should_report(
            Some(downloading(19, Some(1000))),
            downloading(20, Some(1000))
        ));
        // 大きさが分からなければ 256 KiB ごと
        assert!(!should_report(
            Some(downloading(1, None)),
            downloading(256 * 1024 - 1, None)
        ));
        assert!(should_report(
            Some(downloading(1, None)),
            downloading(256 * 1024, None)
        ));
        assert!(should_report(
            Some(ApplyProgress::Preparing),
            downloading(0, None)
        ));
    }

    // ---- 一連の流れ（ローカルのファイルを資産にする） ----

    fn dummy_paths(dir: &Path) -> ExePaths {
        ExePaths::for_exe(dir.join("capturecard_viewer.exe")).expect("名前がある")
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("読めなければならない")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: ExePaths,
        check: UpdateCheck,
    }

    /// 新しい exe（中身は "hello"）と SHA256SUMS.txt を資産として置き、
    /// 差し替え先にダミーの exe（中身は "old"）を置く。
    fn fixture(sums: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let release = dir.path().join("release");
        let install = dir.path().join("install");
        fs::create_dir(&release).unwrap();
        fs::create_dir(&install).unwrap();
        let exe_name = exe_asset_name("v9.9.9");
        fs::write(release.join(&exe_name), "hello").unwrap();
        fs::write(release.join(CHECKSUMS_ASSET_NAME), sums).unwrap();
        let paths = dummy_paths(&install);
        fs::write(&paths.exe, "old").unwrap();

        let url = |name: &str| format!("file:///{}", release.join(name).display());
        let check = check_with_assets(
            "v9.9.9",
            &[
                (&exe_name, &url(&exe_name)),
                (CHECKSUMS_ASSET_NAME, &url(CHECKSUMS_ASSET_NAME)),
            ],
        );
        Fixture {
            _dir: dir,
            paths,
            check,
        }
    }

    fn run(fixture: &Fixture, cancel: bool) -> (Result<(), ApplyError>, Vec<ApplyProgress>) {
        let flag = ApplyControl::default();
        if cancel {
            flag.cancel();
        }
        let mut seen = Vec::new();
        let result = run_apply(&fixture.check, true, &fixture.paths, &flag, &mut |p| {
            seen.push(p)
        });
        (result, seen)
    }

    #[test]
    fn run_apply_downloads_verifies_and_swaps() {
        // 生成側は小文字だが、大文字の hash でも通す
        let fixture = fixture(&format!(
            "{}  capturecard_viewer-v9.9.9-windows-x64.exe\n",
            HELLO_SHA256.to_uppercase()
        ));

        let (result, seen) = run(&fixture, false);

        assert_eq!(result, Ok(()));
        assert_eq!(read(&fixture.paths.exe), "hello");
        assert_eq!(read(&fixture.paths.old), "old");
        assert!(!fixture.paths.new.exists());
        assert_eq!(seen.first(), Some(&ApplyProgress::Preparing));
        assert!(seen.contains(&ApplyProgress::Downloading {
            downloaded: 5,
            total: Some(5)
        }));
        assert_eq!(seen.last(), Some(&ApplyProgress::Installing));
    }

    #[test]
    fn run_apply_checksum_mismatch_keeps_the_exe_and_removes_new() {
        let fixture = fixture(&format!(
            "{}  capturecard_viewer-v9.9.9-windows-x64.exe\n",
            "0".repeat(64)
        ));

        let (result, _) = run(&fixture, false);

        assert_eq!(result, Err(ApplyError::ChecksumMismatch));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
        assert!(!fixture.paths.old.exists());
    }

    #[test]
    fn run_apply_missing_checksum_line_downloads_nothing() {
        let fixture = fixture(&format!("{HELLO_SHA256}  something-else.exe\n"));

        let (result, _) = run(&fixture, false);

        assert_eq!(result, Err(ApplyError::ChecksumMissing));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
    }

    #[test]
    fn run_apply_cancelled_keeps_the_exe_and_removes_new() {
        let fixture = fixture(&format!(
            "{HELLO_SHA256}  capturecard_viewer-v9.9.9-windows-x64.exe\n"
        ));

        let (result, _) = run(&fixture, true);

        assert_eq!(result, Err(ApplyError::Cancelled));
        assert_eq!(read(&fixture.paths.exe), "old");
        assert!(!fixture.paths.new.exists());
        assert!(!fixture.paths.old.exists());
    }

    #[test]
    fn run_apply_unwritable_dir_downloads_nothing() {
        let fixture = fixture(&format!(
            "{HELLO_SHA256}  capturecard_viewer-v9.9.9-windows-x64.exe\n"
        ));
        // フォルダが無い = 書けない
        let paths = dummy_paths(&fixture.paths.dir().join("missing"));
        let flag = ApplyControl::default();

        let result = run_apply(&fixture.check, true, &paths, &flag, &mut |_| {});

        assert!(matches!(result, Err(ApplyError::NotWritable(_))));
        assert_eq!(read(&fixture.paths.exe), "old");
    }

    #[test]
    fn run_apply_checks_the_folder_before_the_assets() {
        // 資産の無い版でも、書けないフォルダならそちらを先に伝える
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let paths = dummy_paths(&dir.path().join("missing"));
        let no_assets = check_with_assets("v1.1.0", &[]);
        let flag = ApplyControl::default();

        let unwritable = run_apply(&no_assets, false, &paths, &flag, &mut |_| {});
        let writable = run_apply(
            &no_assets,
            false,
            &dummy_paths(dir.path()),
            &flag,
            &mut |_| {},
        );

        assert!(matches!(unwritable, Err(ApplyError::NotWritable(_))));
        assert_eq!(writable, Err(ApplyError::NoAssets));
    }

    // ---- キャンセルと差し替えの取り合い ----

    #[test]
    fn apply_control_cancel_before_swap_wins() {
        let control = ApplyControl::default();

        assert!(control.cancel());
        // 2 回目も「止まる側」として扱う
        assert!(control.cancel());
        assert!(!control.begin_swap());
    }

    #[test]
    fn apply_control_cancel_after_swap_started_is_refused() {
        // 差し替えを始めたら止めない。呼び出し側は終わるのを待つ
        let control = ApplyControl::default();

        assert!(control.begin_swap());
        assert!(!control.cancel());
        assert!(!control.is_cancelled());
    }

    #[test]
    fn apply_error_display_follows_the_language() {
        assert_eq!(
            ApplyError::NotWritable("C:/x".to_string()).to_string(),
            "このフォルダには書き込めないため自動更新できません。リリースページから手動で更新してください"
        );
        assert_eq!(
            i18n::with_language(i18n::Language::English, || ApplyError::HttpStatus(404)
                .to_string()),
            "The download returned HTTP 404"
        );
    }
}
