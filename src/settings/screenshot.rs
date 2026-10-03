//! スクリーンショットの設定（[screenshot]）。出力先・保存形式・JPEG 品質の
//! 選択肢と serde の補助、保存先の既定値、保存するファイルのパスの決め方。

use super::AppSettings;
use log::warn;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSettings {
    // 撮った画をどこへ出すか。ファイル・クリップボード・両方の 3 択。
    // 保存形式と JPEG 品質はファイルへ出すときだけ効く（クリップボードへは
    // 圧縮せずそのまま渡す）
    #[serde(deserialize_with = "deserialize_screenshot_destination")]
    pub destination: ScreenshotDestination,
    pub save_folder: PathBuf,
    // 保存形式と JPEG の品質を別々の項目にしてある。品質を持つ enum を
    // 1 項目として持たせると TOML では [screenshot.format] のテーブルになり、
    // 同じセクションの後続のキー（sound_file など）がテーブルの内側へ
    // 取り込まれてしまう。また項目を分けておくと、PNG に切り替えても
    // 品質の値が残り、JPEG へ戻したときに選び直さずに済む。
    //
    // エンコードへ渡すときは encoding() で ScreenshotEncoding にまとめ、
    // 「PNG なのに品質が付いている」組み合わせを作れないようにする
    #[serde(deserialize_with = "deserialize_screenshot_format")]
    pub format: ScreenshotFormat,
    #[serde(deserialize_with = "deserialize_jpeg_quality")]
    pub jpeg_quality: u8,
    pub sound_file: Option<PathBuf>,
    pub sound_volume: f32,
    // 旧版のホットキー設定。**読むだけで、保存では書き出さない。**
    //
    // ホットキーはアクションごとに持つようになったため、置き場所は
    // `AppSettings::hotkeys` へ移った。ここに残しているのは、既存の
    // 設定ファイルにある値を起動時に移すためだけ。`AppSettings` を作る
    // 時点で `None` に戻るので、この項目を見て動く処理を足さないこと。
    #[serde(rename = "hotkey", skip_serializing)]
    pub legacy_hotkey: Option<String>,
}

// スクリーンショットの出力先。
// 設定ファイルには destination = "file" / "clipboard" / "both" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotDestination {
    // 既存ユーザーの設定ファイルには destination が無い。既定をファイルに
    // してあるので、これまでどおりフォルダへ保存されるだけで挙動は変わらない
    #[default]
    File,
    Clipboard,
    Both,
}

impl ScreenshotDestination {
    // ファイルへ書き出すか。false のときは保存先のファイル名も作らない
    pub fn saves_file(self) -> bool {
        matches!(
            self,
            ScreenshotDestination::File | ScreenshotDestination::Both
        )
    }

    // クリップボードへコピーするか
    pub fn copies_to_clipboard(self) -> bool {
        matches!(
            self,
            ScreenshotDestination::Clipboard | ScreenshotDestination::Both
        )
    }
}

// スクリーンショットの保存形式。設定ファイルには format = "jpeg" / "png" と書かれる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    // 既存ユーザーの設定ファイルには format が無い。既定を JPEG にしてあるので
    // 従来どおり JPEG で保存され、拡張子も .jpg のまま変わらない
    #[default]
    Jpeg,
    Png,
}

impl ScreenshotFormat {
    // 保存するファイルの拡張子。先頭のドットは含まない
    pub fn extension(self) -> &'static str {
        match self {
            ScreenshotFormat::Jpeg => "jpg",
            ScreenshotFormat::Png => "png",
        }
    }
}

impl ScreenshotSettings {
    // 設定からエンコードの指定を組み立てる。
    //
    // 品質は設定ファイルを手で書き換えられる前提で、ここで範囲に収める。
    // image 0.24 の JpegEncoder も内部で 1〜100 に丸めるが、そこに寄りかかると
    // クレートの版が変わったときに振る舞いが変わる。渡す前に確定させておく
    pub fn encoding(&self) -> ScreenshotEncoding {
        match self.format {
            ScreenshotFormat::Jpeg => ScreenshotEncoding::Jpeg {
                quality: self.jpeg_quality.clamp(MIN_JPEG_QUALITY, MAX_JPEG_QUALITY),
            },
            ScreenshotFormat::Png => ScreenshotEncoding::Png,
        }
    }
}

// 実際にエンコードするときの形式とパラメータ。
//
// 設定の保存形式（ScreenshotFormat）と分けてあるのは、保存関数へ
// 「PNG なのに品質が付いている」ような組み合わせを渡せなくするため。
// 設定ファイルには書かれないので、TOML の都合に縛られない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotEncoding {
    Jpeg { quality: u8 },
    Png,
}

// JPEG 品質の下限と上限。image クレートの JpegEncoder が受け付ける範囲に合わせてある
pub const MIN_JPEG_QUALITY: u8 = 1;

pub const MAX_JPEG_QUALITY: u8 = 100;

// 効果音の既定値。実行ファイルに埋め込んだ既定音（内蔵の SS.mp3）を指す。
//
// ファイルとしては配布していないので、exe の隣を探しても見つからず、
// screenshot_sound::resolve_sound_path が埋め込みの既定音へ倒すことで鳴る。
// 設定画面の「既定に戻す」もこの値を書き、「既定（内蔵）」の表示もこの値との
// 一致で判定する（docs/design/assets.md）
pub const DEFAULT_SOUND_FILE: &str = "sound/SS.mp3";

// 設定ファイルの format に知らない値が書かれていても、設定全体を失わせない。
// ここでエラーを返すと TOML のパースがファイル単位で失敗し、保存形式と
// 無関係な項目まで既定値へ戻ってしまう。
//
// 値が文字列ですらない場合（format = 3 など）はここでも落ちる。手で書き換えた
// ときに起きやすいのは綴りの誤りなので、拾うのはそこまでにしてある。
fn deserialize_screenshot_format<'de, D>(deserializer: D) -> Result<ScreenshotFormat, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(screenshot_format_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の保存形式 \"{}\" を解釈できないので JPEG として扱う",
            raw
        );
        ScreenshotFormat::default()
    }))
}

// 設定ファイルの destination に知らない値が書かれていても、設定全体を失わせない。
// 理由は deserialize_screenshot_format と同じ。
fn deserialize_screenshot_destination<'de, D>(
    deserializer: D,
) -> Result<ScreenshotDestination, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(screenshot_destination_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の出力先 \"{}\" を解釈できないのでファイルへの保存として扱う",
            raw
        );
        ScreenshotDestination::default()
    }))
}

// 範囲外の品質が書かれていても、設定全体を失わせない。u8 のまま読むと
// jpeg_quality = 256 のような値でパースがファイル単位で失敗し、品質と
// 無関係な項目まで既定値へ戻ってしまう。TOML の整数は i64 なので、
// 広いほうで受けてから 1〜100 に丸める。
//
// 値が整数ですらない場合（jpeg_quality = 90.5 など）はここでも落ちる。
// format と同じく、手で書き換えたときに起きやすいところだけを拾う。
fn deserialize_jpeg_quality<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_JPEG_QUALITY), i64::from(MAX_JPEG_QUALITY));
    if clamped != raw {
        warn!(
            "設定の JPEG 品質 {} は範囲外なので {} として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u8 に収まる
    Ok(clamped as u8)
}

// 設定ファイルに書かれた文字列から出力先を決める。
// 解釈できない場合は None を返す。
fn screenshot_destination_from_str(raw: &str) -> Option<ScreenshotDestination> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "file" => Some(ScreenshotDestination::File),
        "clipboard" => Some(ScreenshotDestination::Clipboard),
        "both" => Some(ScreenshotDestination::Both),
        _ => None,
    }
}

// 設定ファイルに書かれた文字列から保存形式を決める。
// 解釈できない場合は None を返す。
fn screenshot_format_from_str(raw: &str) -> Option<ScreenshotFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => Some(ScreenshotFormat::Jpeg),
        "png" => Some(ScreenshotFormat::Png),
        _ => None,
    }
}

// スクリーンショットの保存先の既定値。
//
// デスクトップ → %USERPROFILE% → 実行ファイルの置き場所 → 一時フォルダ の順に倒す。
// **カレントディレクトリは使わない。** どこから起動したかで保存先が変わるうえ、
// ショートカットやタスクスケジューラから起動すると C:\Windows\System32 のような
// 書き込めない場所を指しうる。保存に失敗する理由が設定画面から見て分からない。
fn default_screenshot_folder() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    screenshot_folder_from(
        dirs::desktop_dir(),
        dirs::home_dir(),
        exe_dir,
        std::env::temp_dir(),
    )
}

// 保存先の候補から実際に使うものを選ぶ。
//
// 候補の取得は環境に依存するため、選ぶ部分だけを切り出してテストする。
// last_resort は常に値がある候補（一時フォルダ）を想定している。
fn screenshot_folder_from(
    desktop: Option<PathBuf>,
    home: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    last_resort: PathBuf,
) -> PathBuf {
    if let Some(desktop) = desktop {
        return desktop;
    }

    if let Some(home) = home {
        warn!(
            "デスクトップの場所が分からないので、スクリーンショットの保存先を {} にする",
            home.display()
        );
        return home;
    }

    if let Some(exe_dir) = exe_dir {
        warn!(
            "ユーザーフォルダの場所も分からないので、スクリーンショットの保存先を実行ファイルの場所 {} にする",
            exe_dir.display()
        );
        return exe_dir;
    }

    warn!(
        "実行ファイルの場所も分からないので、スクリーンショットの保存先を一時フォルダ {} にする",
        last_resort.display()
    );
    last_resort
}

impl Default for ScreenshotSettings {
    fn default() -> Self {
        Self {
            // 既定はファイルへの保存だけ。これまでの挙動をそのまま既定にする
            destination: ScreenshotDestination::File,
            save_folder: default_screenshot_folder(),
            format: ScreenshotFormat::Jpeg,
            // image クレートの save() は JpegEncoder::new を通るため、
            // これまでの保存は品質 75 固定だった。ゲーム画面のように
            // 文字や細い線が多い画には 75 では圧縮の跡が見えるので、
            // 既定をひとつ上の 90 にしてある。ファイルは 75 のおよそ 2 倍に
            // なるが、それでも PNG よりはずっと小さい。
            // 品質を気にしない用途は既定のまま、跡を残したくない用途は
            // PNG を選ぶ、という切り分けにする
            jpeg_quality: 90,
            // 相対パスのまま既定値にしてある。既存ユーザーの設定ファイルにも
            // この値が保存されているため、変えると移行の前提が崩れる。
            // 解決は screenshot_sound::resolve_sound_path が exe の置き場所を基準に行い、
            // 見つからなければ埋め込みの既定音へ倒す。
            // None は「効果音を鳴らさない」の意味なので、既定値には使えない
            sound_file: Some(PathBuf::from(DEFAULT_SOUND_FILE)),
            sound_volume: 100.0,
            // 既定は「旧版の項目が無い」。既定のホットキーは
            // default_hotkeys() が持つ
            legacy_hotkey: None,
        }
    }
}

impl AppSettings {
    pub fn get_screenshot_path(&self, timestamp: &str) -> PathBuf {
        let extension = self.screenshot.format.extension();
        let mut path = self.screenshot.save_folder.clone();
        path.push(format!("{}.{}", timestamp, extension));

        // ファイル名の競合を処理
        let mut counter = 1;
        while path.exists() {
            let stem = format!("{}({})", timestamp, counter);
            path.set_file_name(format!("{}.{}", stem, extension));
            counter += 1;
        }

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::HotkeyAction;
    use crate::settings::testing::{without_key, DESKTOP, EXE_DIR, FULL_CONFIG, HOME, TEMP};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn screenshot_folder_from_desktop_available_uses_desktop() {
        let folder = screenshot_folder_from(
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(DESKTOP));
    }

    #[test]
    fn screenshot_folder_from_no_desktop_falls_back_to_home() {
        // デスクトップをリダイレクトしている環境などで desktop_dir() が None になる場合
        let folder = screenshot_folder_from(
            None,
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(HOME));
    }

    #[test]
    fn screenshot_folder_from_no_user_folders_falls_back_to_exe_dir() {
        let folder = screenshot_folder_from(
            None,
            None,
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(EXE_DIR));
    }

    #[test]
    fn screenshot_folder_from_nothing_available_falls_back_to_last_resort() {
        let folder = screenshot_folder_from(None, None, None, PathBuf::from(TEMP));

        assert_eq!(folder, PathBuf::from(TEMP));
    }

    #[test]
    fn screenshot_folder_from_never_returns_current_dir() {
        // 修正前の挙動の再現防止。どの候補も取れなくてもカレントディレクトリを
        // 指さないこと。相対パスだと起動元によって保存先が変わる
        let folder = screenshot_folder_from(None, None, None, PathBuf::from(TEMP));

        assert_ne!(folder, PathBuf::from("."));
        assert!(folder.is_absolute(), "保存先の既定値は絶対パスであること");
    }

    // get_screenshot_path は save_folder しか見ないため、
    // 一時ディレクトリを指した設定を組み立てれば %AppData% にもデスクトップにも触れない。
    fn settings_saving_into(dir: &Path) -> AppSettings {
        let mut settings = AppSettings::default();
        settings.screenshot.save_folder = dir.to_path_buf();
        settings
    }

    #[test]
    fn get_screenshot_path_no_conflict_uses_timestamp_as_is() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_one_conflict_appends_1() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_two_conflicts_appends_2() {
        // 連番付きのファイルも競合の判定に含めること。
        // 「(1) を作ったら (1) を上書きした」を防ぐための確認
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(2).jpg"));
    }

    #[test]
    fn get_screenshot_path_three_conflicts_appends_3() {
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(1).jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(3).jpg"));
    }

    #[test]
    fn get_screenshot_path_gap_in_numbering_fills_the_gap() {
        // (1) だけ消された状態。連番は「空いている最小の番号」であり、
        // 既存の最大値 + 1 ではない
        let dir = tempdir().expect("一時ディレクトリを作れること");
        for name in [
            "2026-09-19_12-00-00-000.jpg",
            "2026-09-19_12-00-00-000(2).jpg",
        ] {
            fs::write(dir.path().join(name), b"").expect("先客のファイルを置けること");
        }
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_timestamp_with_dots_keeps_jpg_extension() {
        // タイムスタンプ自体にドットが含まれる場合。連番を付けるときに
        // ドット以降を拡張子と見なして削ってしまうと ".jpg" を失う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026.09.19_12.00.00.jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026.09.19_12.00.00");

        assert_eq!(path, dir.path().join("2026.09.19_12.00.00(1).jpg"));
    }

    #[test]
    fn get_screenshot_path_only_numbered_file_exists_uses_timestamp_as_is() {
        // 連番だけがあって本体が無い場合は、連番を付けずに本体の名前を使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000(1).jpg"), b"")
            .expect("先客のファイルを置けること");
        let settings = settings_saving_into(dir.path());

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.jpg"));
    }

    #[test]
    fn get_screenshot_path_png_format_uses_png_extension() {
        // 保存形式を PNG にしたら拡張子も追従すること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn get_screenshot_path_png_conflict_keeps_png_extension() {
        // 連番を付けるときに拡張子を .jpg へ戻してしまわないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.png"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000(1).png"));
    }

    #[test]
    fn get_screenshot_path_png_ignores_jpg_with_same_name() {
        // 形式が違えばファイル名は衝突しない。同名の .jpg があっても
        // .png 側は連番を付けずに撮影時刻そのままを使う
        let dir = tempdir().expect("一時ディレクトリを作れること");
        fs::write(dir.path().join("2026-09-19_12-00-00-000.jpg"), b"")
            .expect("先客のファイルを置けること");
        let mut settings = settings_saving_into(dir.path());
        settings.screenshot.format = ScreenshotFormat::Png;

        let path = settings.get_screenshot_path("2026-09-19_12-00-00-000");

        assert_eq!(path, dir.path().join("2026-09-19_12-00-00-000.png"));
    }

    #[test]
    fn screenshot_format_extension_matches_format() {
        assert_eq!(ScreenshotFormat::Jpeg.extension(), "jpg");
        assert_eq!(ScreenshotFormat::Png.extension(), "png");
    }

    #[test]
    fn encoding_jpeg_passes_quality_through() {
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;
        settings.screenshot.jpeg_quality = 55;

        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 55 }
        );
    }

    #[test]
    fn encoding_jpeg_clamps_quality_into_range() {
        // 設定ファイルを手で書き換えられた場合。エンコーダへ渡す前に丸める
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Jpeg;

        settings.screenshot.jpeg_quality = 0;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 1 }
        );

        settings.screenshot.jpeg_quality = 255;
        assert_eq!(
            settings.screenshot.encoding(),
            ScreenshotEncoding::Jpeg { quality: 100 }
        );
    }

    #[test]
    fn encoding_png_ignores_jpeg_quality() {
        // PNG は可逆なので品質の値を持ち込まない
        let mut settings = AppSettings::default();
        settings.screenshot.format = ScreenshotFormat::Png;
        settings.screenshot.jpeg_quality = 10;

        assert_eq!(settings.screenshot.encoding(), ScreenshotEncoding::Png);
    }

    #[test]
    fn app_settings_missing_format_key_defaults_to_jpeg() {
        // format を足す前の版が書いた設定ファイル。これまでと同じ JPEG で
        // 保存され、他の項目も保持されなければならない。
        // without_key を使わないのは [video] にも format があるため
        let config = FULL_CONFIG.replace(
            "format = \"png\"
",
            "",
        );
        assert!(
            !config.contains("format = \"png\""),
            "テスト用の設定から screenshot の format が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("format が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_missing_jpeg_quality_key_uses_default_quality() {
        let config = without_key(FULL_CONFIG, "jpeg_quality");
        assert!(
            !config.contains("jpeg_quality ="),
            "テスト用の設定から jpeg_quality が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("jpeg_quality が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 90);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
    }

    #[test]
    fn app_settings_missing_destination_key_defaults_to_file() {
        // 出力先を足す前の版が書いた設定ファイル。これまでどおりファイルへ
        // 保存され、他の項目も保持されなければならない
        let config = without_key(FULL_CONFIG, "destination");
        assert!(
            !config.contains("destination ="),
            "テスト用の設定から destination が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("destination が欠けていても読めなければならない");

        assert_eq!(settings.screenshot.destination, ScreenshotDestination::File);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_unknown_destination_value_falls_back_to_file_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。出力先だけが既定へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"destination = "both""#, r#"destination = "printer""#);
        assert!(config.contains(r#"destination = "printer""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない出力先でも読めなければならない");

        assert_eq!(settings.screenshot.destination, ScreenshotDestination::File);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn screenshot_destination_from_str_accepts_known_spellings() {
        assert_eq!(
            screenshot_destination_from_str("file"),
            Some(ScreenshotDestination::File)
        );
        assert_eq!(
            screenshot_destination_from_str("CLIPBOARD"),
            Some(ScreenshotDestination::Clipboard)
        );
        assert_eq!(
            screenshot_destination_from_str(" both "),
            Some(ScreenshotDestination::Both)
        );
        assert_eq!(screenshot_destination_from_str(""), None);
        assert_eq!(screenshot_destination_from_str("files"), None);
    }

    #[test]
    fn screenshot_destination_flags_match_each_variant() {
        // 出力先ごとに「何をするか」の判定。両方のときは 2 つとも true になる
        assert!(ScreenshotDestination::File.saves_file());
        assert!(!ScreenshotDestination::File.copies_to_clipboard());

        assert!(!ScreenshotDestination::Clipboard.saves_file());
        assert!(ScreenshotDestination::Clipboard.copies_to_clipboard());

        assert!(ScreenshotDestination::Both.saves_file());
        assert!(ScreenshotDestination::Both.copies_to_clipboard());
    }

    #[test]
    fn app_settings_unknown_format_value_falls_back_to_jpeg_without_losing_settings() {
        // 手で書き換えて綴りを誤った場合。保存形式だけが既定へ倒れ、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace(r#"format = "png""#, r#"format = "webp""#);
        assert!(config.contains(r#"format = "webp""#));

        let settings: AppSettings =
            toml::from_str(&config).expect("知らない保存形式でも読めなければならない");

        assert_eq!(settings.screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_out_of_range_jpeg_quality_is_clamped_without_losing_settings() {
        // 手で書き換えて u8 に収まらない値を入れた場合。品質だけが範囲に
        // 収まり、無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = 256");
        assert!(config.contains("jpeg_quality = 256"));

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 100);
        assert_eq!(settings.screenshot.format, ScreenshotFormat::Png);
        assert_eq!(settings.hotkey(HotkeyAction::Screenshot), Some("Ctrl+S"));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_negative_jpeg_quality_is_clamped_to_minimum() {
        let config = FULL_CONFIG.replace("jpeg_quality = 60", "jpeg_quality = -5");

        let settings: AppSettings =
            toml::from_str(&config).expect("負の品質でも読めなければならない");

        assert_eq!(settings.screenshot.jpeg_quality, 1);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn screenshot_format_from_str_accepts_known_spellings() {
        assert_eq!(
            screenshot_format_from_str("jpeg"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str("JPG"),
            Some(ScreenshotFormat::Jpeg)
        );
        assert_eq!(
            screenshot_format_from_str(" png "),
            Some(ScreenshotFormat::Png)
        );
        assert_eq!(screenshot_format_from_str(""), None);
        assert_eq!(screenshot_format_from_str("bmp"), None);
    }
}
