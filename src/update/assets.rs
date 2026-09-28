//! 更新の適用のうち資産の選び方。見つかった版の Release の資産から、
//! どの exe と `SHA256SUMS.txt` をどこから落とすかを決める（`ApplyPlan`）
//! （`docs/design/update.md` の「適用」）。
//!
//! ダウンロードと照合、差し替えは `apply.rs`。

use super::apply::ApplyError;
use super::overrides::{file_url_to_path, strip_prefix_ignore_case};
use super::{ReleaseAsset, UpdateCheck};
use std::path::PathBuf;

/// 照合に使う資産の名前（`docs/RELEASE.md` の「配布物」）。
pub(super) const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// 資産の URL として受け付ける頭。テスト用の問い合わせ先を使っていないときは、
/// `<頭><タグ>/<資産名>` と完全に一致するものしか落とさない。
const DOWNLOAD_URL_PREFIX: &str =
    "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/";

/// Release に添付する exe の名前（1.2.1 から。`docs/RELEASE.md` の「配布物」）。
/// 版を含めないので、ダウンロードした名前と、更新で維持される名前が一致する。
pub(super) const EXE_ASSET_NAME: &str = "capturecard_viewer.exe";

/// 1.2.0 の Release に添付していた exe の名前。`tag` は Release のタグそのまま（`v1.2.0`）。
/// `EXE_ASSET_NAME` が無い Release からも更新できるよう、こちらも探す。
pub(super) fn legacy_exe_asset_name(tag: &str) -> String {
    format!("capturecard_viewer-{tag}-windows-x64.exe")
}

/// 資産をどこから取るか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AssetSource {
    /// HTTP で取る
    Http(String),
    /// ファイルをそのまま読む。テスト用の問い合わせ先の JSON に `file://` で
    /// 書いたときだけ（`CAPTURECARD_VIEWER_UPDATE_API_URL`）
    File(PathBuf),
}

/// 更新に使う 2 つの資産。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApplyPlan {
    /// exe の資産名。`SHA256SUMS.txt` の行をこの名前で引く
    pub(super) exe_name: String,
    /// exe の取り先
    pub(super) exe: AssetSource,
    /// `SHA256SUMS.txt` の取り先
    pub(super) checksums: AssetSource,
}

impl ApplyPlan {
    /// 見つかった版の資産から、何をどこから落とすかを決める。
    ///
    /// `allow_any_source` はテスト用の問い合わせ先を使っているとき。偽なら
    /// 資産の URL がこのリポジトリの Release のものでなければ落とさない。
    ///
    /// exe は `EXE_ASSET_NAME` を探し、無ければ旧名（`legacy_exe_asset_name`）を探す。
    /// `SHA256SUMS.txt` の行は選んだほうの名前で引く（`exe_name`）。
    ///
    /// **資産が無い（1.1.0 以前の Release）なら `ApplyError::NoAssets`。**
    /// 自動では更新できないので、リリースページから手で更新してもらう。
    pub(super) fn from_check(
        check: &UpdateCheck,
        allow_any_source: bool,
    ) -> Result<Self, ApplyError> {
        let find = |name: &str| check.assets.iter().find(|asset| asset.name == name);
        let exe = find(EXE_ASSET_NAME).or_else(|| find(&legacy_exe_asset_name(&check.tag)));
        let (Some(exe), Some(checksums)) = (exe, find(CHECKSUMS_ASSET_NAME)) else {
            return Err(ApplyError::NoAssets);
        };
        Ok(Self {
            exe: asset_source(exe, &check.tag, allow_any_source)?,
            checksums: asset_source(checksums, &check.tag, allow_any_source)?,
            exe_name: exe.name.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

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

    const EXE_URL: &str =
        "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/capturecard_viewer.exe";
    const SUMS_URL: &str =
        "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/SHA256SUMS.txt";
    const LEGACY_EXE_URL: &str = "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/capturecard_viewer-v1.2.0-windows-x64.exe";
    const LEGACY_SUMS_URL: &str =
        "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/SHA256SUMS.txt";

    #[test]
    fn exe_asset_names_follow_the_release_naming() {
        assert_eq!(EXE_ASSET_NAME, "capturecard_viewer.exe");
        assert_eq!(
            legacy_exe_asset_name("v1.2.0"),
            "capturecard_viewer-v1.2.0-windows-x64.exe"
        );
    }

    #[test]
    fn apply_plan_from_check_picks_exe_and_checksums() {
        let check = check_with_assets(
            "v1.2.1",
            &[
                ("capturecard_viewer.exe", EXE_URL),
                ("SHA256SUMS.txt", SUMS_URL),
            ],
        );

        let plan = ApplyPlan::from_check(&check, false).expect("資産は揃っている");

        assert_eq!(plan.exe_name, "capturecard_viewer.exe");
        assert_eq!(plan.exe, AssetSource::Http(EXE_URL.to_string()));
        assert_eq!(plan.checksums, AssetSource::Http(SUMS_URL.to_string()));
    }

    #[test]
    fn apply_plan_from_check_falls_back_to_the_legacy_exe_name() {
        // 1.2.0 の Release は版付きの名前
        let check = check_with_assets(
            "v1.2.0",
            &[
                ("capturecard_viewer-v1.2.0-windows-x64.exe", LEGACY_EXE_URL),
                ("SHA256SUMS.txt", LEGACY_SUMS_URL),
            ],
        );

        let plan = ApplyPlan::from_check(&check, false).expect("旧名の資産でも揃っている");

        assert_eq!(plan.exe_name, "capturecard_viewer-v1.2.0-windows-x64.exe");
        assert_eq!(plan.exe, AssetSource::Http(LEGACY_EXE_URL.to_string()));
        assert_eq!(
            plan.checksums,
            AssetSource::Http(LEGACY_SUMS_URL.to_string())
        );
    }

    #[test]
    fn apply_plan_from_check_prefers_the_plain_exe_name() {
        let legacy_url = "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/capturecard_viewer-v1.2.1-windows-x64.exe";
        let check = check_with_assets(
            "v1.2.1",
            &[
                ("capturecard_viewer-v1.2.1-windows-x64.exe", legacy_url),
                ("capturecard_viewer.exe", EXE_URL),
                ("SHA256SUMS.txt", SUMS_URL),
            ],
        );

        let plan = ApplyPlan::from_check(&check, false).expect("資産は揃っている");

        assert_eq!(plan.exe_name, "capturecard_viewer.exe");
        assert_eq!(plan.exe, AssetSource::Http(EXE_URL.to_string()));
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
        let exe_only = check_with_assets("v1.2.1", &[("capturecard_viewer.exe", EXE_URL)]);
        let legacy_exe_only = check_with_assets(
            "v1.2.0",
            &[("capturecard_viewer-v1.2.0-windows-x64.exe", LEGACY_EXE_URL)],
        );
        let sums_only = check_with_assets("v1.2.1", &[("SHA256SUMS.txt", SUMS_URL)]);
        // 旧名はタグと版が合うものだけを探す
        let other_tag = check_with_assets(
            "v1.2.1",
            &[
                ("capturecard_viewer-v1.2.0-windows-x64.exe", LEGACY_EXE_URL),
                ("SHA256SUMS.txt", SUMS_URL),
            ],
        );

        for check in [zip_only, exe_only, legacy_exe_only, sums_only, other_tag] {
            assert_eq!(
                ApplyPlan::from_check(&check, false),
                Err(ApplyError::NoAssets)
            );
        }
    }

    #[test]
    fn apply_plan_from_check_rejects_foreign_urls() {
        for exe_url in [
            "https://example.invalid/capturecard_viewer.exe",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/../../../../attacker/x/releases/download/v1.2.1/capturecard_viewer.exe",
            "https://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.0/capturecard_viewer.exe",
            "http://github.com/Mui-MuiMui/Capturecard_Viewer/releases/download/v1.2.1/capturecard_viewer.exe",
            "file:///C:/work/capturecard_viewer.exe",
        ] {
            let check = check_with_assets(
                "v1.2.1",
                &[
                    ("capturecard_viewer.exe", exe_url),
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
                ("capturecard_viewer.exe", "file:///C:/work/new.exe"),
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
                ("capturecard_viewer.exe", "ftp://x.invalid/a.exe"),
                ("SHA256SUMS.txt", "file://"),
            ],
        );

        assert!(matches!(
            ApplyPlan::from_check(&check, true),
            Err(ApplyError::UnexpectedAssetUrl(_))
        ));
    }
}
