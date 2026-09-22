use crate::video::VideoFrame;
use log::info;
use rodio::{Decoder, OutputStream, Sink};
use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// スクリーンショットまわりの処理が失敗した理由。
///
/// クリップボードへのコピーと効果音の読み込みを 1 つの enum にまとめてある。
/// どちらも `ErrorSource::Screenshot` として同じ経路で表示され、呼び出し側は
/// 出力先の種類で処理を分けないため（`app::screenshot` の
/// `summarize_screenshot_delivery`）。
///
/// **表示用の日本語はこの型の `Display` が持つ。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenshotError {
    /// 幅か高さが 0 のフレームを渡された
    EmptyFrame { width: usize, height: usize },
    /// 幅 × 高さ × 3 が `usize` に収まらない
    FrameTooLarge { width: usize, height: usize },
    /// 幅と高さから決まる長さに対して画素が足りない
    FrameTooShort {
        width: usize,
        height: usize,
        len: usize,
    },
    /// クリップボードを開けない（他のアプリが掴んでいる場合など）
    ClipboardOpenFailed(String),
    /// クリップボードを開けたが画像を書き込めない
    ClipboardWriteFailed(String),
    /// 効果音ファイルが見つかったのに読めない。既定の効果音へ倒してある
    SoundFileUnreadable { path: PathBuf, source: String },
}

impl fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScreenshotError::EmptyFrame { width, height } => write!(
                f,
                "大きさのない映像フレームはクリップボードへコピーできない: {width}x{height}"
            ),
            ScreenshotError::FrameTooLarge { width, height } => {
                write!(f, "画像として扱えない大きさのフレーム: {width}x{height}")
            }
            ScreenshotError::FrameTooShort { width, height, len } => write!(
                f,
                "映像フレームの画素が足りない: {width}x{height} に対して {len} バイト"
            ),
            ScreenshotError::ClipboardOpenFailed(source) => {
                write!(f, "クリップボードを開けない: {source}")
            }
            ScreenshotError::ClipboardWriteFailed(source) => {
                write!(f, "クリップボードへ画像を書き込めない: {source}")
            }
            ScreenshotError::SoundFileUnreadable { path, source } => write!(
                f,
                "効果音ファイル {} を読み込めないため既定の効果音を使う: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ScreenshotError {}

// 既定の効果音。実行ファイルに埋め込む。
// 既定値が "sound/SS.mp3" というカレントディレクトリ基準の相対パスだったため、
// ショートカット経由など作業ディレクトリが exe の場所と異なる起動では鳴らなかった。
const EMBEDDED_SOUND: &[u8] = include_bytes!("../sound/SS.mp3");

// 設定に保存された効果音のパスを、何から読むかへ解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSource {
    // 実行ファイルに埋め込んだ既定の効果音を使う
    Embedded,
    // 指定されたファイルを読む。相対パスは解決済み
    File(PathBuf),
}

// 設定の効果音パスを、実際に読むパスへ解決する。
//
// 相対パスをカレントディレクトリではなく exe の置き場所を基準に解決するのが要点。
// 既存ユーザーの設定ファイルには、かつての既定値 "sound/SS.mp3" が相対パスのまま
// 保存されているため、exe の隣から探せるようにする。
//
// 見つからない場合は無音ではなく埋め込みの既定音へ倒す。ログの出口が無い現状では、
// 無音にするとユーザーに原因を伝える手段が無く、故障と区別が付かないため。
// 効果音そのものを止めたい場合は、設定の sound_file を None にする（設定画面の
// 「クリア」）。その場合はこの関数が呼ばれない。
//
// exists を引数で受けるのはテストのため。実行時は |path| path.exists() を渡す。
pub fn resolve_sound_path(
    configured: &Path,
    exe_dir: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> SoundSource {
    // 空のパスを exe_dir.join() に通すと exe のディレクトリ自身になり、
    // ディレクトリを効果音として読もうとしてしまう
    if configured.as_os_str().is_empty() {
        return SoundSource::Embedded;
    }

    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        match exe_dir {
            Some(dir) => dir.join(configured),
            // exe の場所が分からない場合、基準にできるのはカレントディレクトリ
            // しか残らない。そこを見に行くと修正前の挙動に戻るため、
            // 相対パスの解決自体を諦めて埋め込み音へ倒す
            None => return SoundSource::Embedded,
        }
    };

    if exists(&candidate) {
        SoundSource::File(candidate)
    } else {
        SoundSource::Embedded
    }
}

// 実行ファイルが置かれているディレクトリ。取得できない場合は None。
//
// カレントディレクトリへフォールバックしない。そうすると、この修正で
// 取り除いたはずのカレントディレクトリ依存が黙って復活するため。
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// 映像フレームをクリップボードへ画像としてコピーする。
///
/// アプリの状態にも共有ロックにも触れないので、そのまま別スレッドで実行できる。
/// `app/screenshot.rs` の `save_frame` と対になる、撮影スレッドから呼ぶ関数。
///
/// **UI スレッドから呼ばない。** Windows のクリップボードは一度に 1 つの
/// プロセスしか開けず、他のアプリが掴んでいる間は待たされる（arboard は
/// 5ms 間隔で 5 回まで再試行する）。
///
/// **コピーしたスレッドが終わっても内容は残る。** Windows の
/// `SetClipboardData` は渡したメモリの所有権をシステムへ移すため、
/// `arboard::Clipboard` を落としてもクリップボードの中身は失われない
/// （遅延レンダリングを使っていないので、貼り付けのたびに元のスレッドへ
/// 問い合わせに行くこともない）。撮影ごとに spawn する短命のスレッドから
/// 呼んでよいのはこのため。
///
/// 画は圧縮せずそのまま渡す。保存形式と JPEG 品質はファイルへ出すときだけの
/// 設定で、クリップボードには効かない。
pub fn copy_frame_to_clipboard(frame: &VideoFrame) -> Result<(), ScreenshotError> {
    let bytes = rgb_to_rgba(&frame.data, frame.width, frame.height)?;

    // Clipboard はスレッドごとに作る。Windows では OpenClipboard が呼んだ
    // スレッドに紐づくため、他スレッドで作ったものを持ち回せない
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| ScreenshotError::ClipboardOpenFailed(e.to_string()))?;

    clipboard
        .set_image(arboard::ImageData {
            width: frame.width,
            height: frame.height,
            bytes: Cow::Owned(bytes),
        })
        .map_err(|e| ScreenshotError::ClipboardWriteFailed(e.to_string()))
}

/// RGB の画素列を、`arboard` が要求する RGBA へ広げる。
///
/// 不透明として扱うのでアルファは常に 255。キャプチャーした映像に透過は無く、
/// 0 を入れると貼り付け先によっては全面が透明になる。
fn rgb_to_rgba(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ScreenshotError> {
    if width == 0 || height == 0 {
        return Err(ScreenshotError::EmptyFrame { width, height });
    }

    // 1080p でも 1920*1080*3 で usize には十分収まるが、壊れた値が来たときに
    // 掛け算が一周して短い長さを要求してしまうのを避ける
    let needed = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(ScreenshotError::FrameTooLarge { width, height })?;

    if rgb.len() < needed {
        return Err(ScreenshotError::FrameTooShort {
            width,
            height,
            len: rgb.len(),
        });
    }

    let mut rgba = Vec::with_capacity(needed / 3 * 4);
    // needed は 3 の倍数なので端数は出ない
    let (pixels, _) = rgb[..needed].as_chunks::<3>();
    for pixel in pixels {
        rgba.extend_from_slice(pixel);
        rgba.push(u8::MAX);
    }
    Ok(rgba)
}

/// スクリーンショットの効果音を持つ。
///
/// **ホットキーの登録と押下の検出は持たない。** グローバルホットキーは
/// スクリーンショット以外のアクションにも割り当てられるため、`hotkey.rs` の
/// `HotkeyManager` が一手に扱う。
pub struct ScreenshotManager {
    sound_data: Option<Vec<u8>>,
}

impl ScreenshotManager {
    pub fn new() -> Self {
        Self { sound_data: None }
    }

    /// 効果音を捨て、以降スクリーンショットを無音にする。
    ///
    /// 設定画面で効果音を「クリア」したときに呼ぶ。`set_sound_file` は
    /// ファイルが見つからなければ埋め込みの既定音へ倒すため、「鳴らさない」は
    /// 設定を `None` にすることでしか表せない。その `None` をここで実行時へ
    /// 反映する。
    pub fn clear_sound(&mut self) {
        if self.sound_data.is_none() {
            return;
        }
        info!("効果音を破棄した。以降スクリーンショットは無音になる");
        self.sound_data = None;
    }

    // 効果音を読み込む。
    //
    // 相対パスは exe の置き場所を基準に解決し、見つからなければ埋め込みの
    // 既定音を使う。そのため呼び出し後は必ず鳴らせる状態になっている。
    // Err を返すのは、解決したファイルが存在したのに読めなかった場合だけ。
    // このときも既定音を入れてあるので、鳴らないという結果にはならない。
    pub fn set_sound_file(&mut self, sound_path: &Path) -> Result<(), ScreenshotError> {
        match resolve_sound_path(sound_path, exe_dir().as_deref(), |path| path.exists()) {
            SoundSource::Embedded => {
                self.sound_data = Some(EMBEDDED_SOUND.to_vec());
                Ok(())
            }
            SoundSource::File(path) => match std::fs::read(&path) {
                Ok(data) => {
                    self.sound_data = Some(data);
                    Ok(())
                }
                Err(e) => {
                    self.sound_data = Some(EMBEDDED_SOUND.to_vec());
                    Err(ScreenshotError::SoundFileUnreadable {
                        path,
                        source: e.to_string(),
                    })
                }
            },
        }
    }

    pub fn play_screenshot_sound(&self, volume: f32) {
        if let Some(sound_data) = &self.sound_data {
            let sound_data = sound_data.clone();
            let volume = (volume / 100.0).clamp(0.0, 2.0); // パーセンテージを0.0-2.0範囲に変換
            std::thread::spawn(move || {
                if let Ok((_stream, stream_handle)) = OutputStream::try_default() {
                    if let Ok(sink) = Sink::try_new(&stream_handle) {
                        sink.set_volume(volume);
                        let cursor = Cursor::new(sound_data);
                        if let Ok(decoder) = Decoder::new(cursor) {
                            sink.append(decoder);
                            sink.sleep_until_end();
                        }
                    }
                }
            });
        }
    }
}

impl Default for ScreenshotManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // exe が置かれている想定のディレクトリ。実在しなくてよい
    const EXE_DIR: &str = "C:/Program Files/capturecard_viewer";

    #[test]
    fn resolve_sound_path_absolute_existing_uses_that_file() {
        // 設定画面からユーザーがファイルを選んだ場合。rfd は絶対パスを返す
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("custom.mp3");
        assert!(configured.is_absolute(), "テストの前提: 絶対パスであること");

        let resolved = resolve_sound_path(&configured, Some(Path::new(EXE_DIR)), |_| true);

        assert_eq!(resolved, SoundSource::File(configured));
    }

    #[test]
    fn resolve_sound_path_absolute_missing_falls_back_to_embedded() {
        // ユーザーが選んだファイルを後から移動・削除した場合
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("deleted.mp3");

        let resolved = resolve_sound_path(&configured, Some(Path::new(EXE_DIR)), |_| false);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_existing_resolves_against_exe_dir() {
        // 既存ユーザーの設定に残っている "sound/SS.mp3" を、exe の隣から見つける。
        // カレントディレクトリ基準で解決していると exists が false になり、
        // SoundSource::Embedded へ落ちる
        let expected = PathBuf::from("C:/Program Files/capturecard_viewer/sound/SS.mp3");

        let resolved = resolve_sound_path(
            Path::new("sound/SS.mp3"),
            Some(Path::new(EXE_DIR)),
            |path| path == expected,
        );

        assert_eq!(resolved, SoundSource::File(expected));
    }

    #[test]
    fn resolve_sound_path_relative_missing_falls_back_to_embedded() {
        // exe の隣にも sound/ が無い配布形態。埋め込みの既定音で鳴らす
        let resolved =
            resolve_sound_path(Path::new("sound/SS.mp3"), Some(Path::new(EXE_DIR)), |_| {
                false
            });

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_empty_falls_back_to_embedded() {
        // 設定に空文字が入っていた場合。exe のディレクトリ自身を
        // 効果音ファイルとして読もうとしないこと
        let resolved = resolve_sound_path(Path::new(""), Some(Path::new(EXE_DIR)), |_| true);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_ignores_current_dir() {
        // 不具合そのものの再現。カレントディレクトリ配下にだけファイルがある
        // 状況を作り、そこを見に行かないことを確かめる
        let cwd_candidate = PathBuf::from("sound/SS.mp3");

        let resolved = resolve_sound_path(
            Path::new("sound/SS.mp3"),
            Some(Path::new(EXE_DIR)),
            |path| path == cwd_candidate,
        );

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_without_exe_dir_falls_back_to_embedded() {
        // current_exe() が失敗して exe の場所が分からない場合。ここで
        // カレントディレクトリを基準にすると修正前の挙動に戻るため、
        // ファイルが存在していても埋め込み音へ倒す
        let resolved = resolve_sound_path(Path::new("sound/SS.mp3"), None, |_| true);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_absolute_without_exe_dir_uses_that_file() {
        // 絶対パスの指定は基準ディレクトリを必要としないため、
        // exe の場所が分からなくてもそのまま使える
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("custom.mp3");

        let resolved = resolve_sound_path(&configured, None, |_| true);

        assert_eq!(resolved, SoundSource::File(configured));
    }

    #[test]
    fn embedded_sound_starts_with_mpeg_frame_sync() {
        // 埋め込む mp3 が空や別形式に差し替わると、例外も出ないまま無音になる。
        // MPEG のフレーム同期（11 ビットすべて 1）で最低限の形式を確かめる
        assert!(EMBEDDED_SOUND.len() > 2, "埋め込んだ効果音が短すぎる");
        assert_eq!(EMBEDDED_SOUND[0], 0xFF);
        assert_eq!(EMBEDDED_SOUND[1] & 0xE0, 0xE0);
    }

    #[test]
    fn rgb_to_rgba_inserts_opaque_alpha_after_every_pixel() {
        // 2x1 の赤と緑。並びを変えずにアルファだけが挟まること
        let rgb = vec![255, 0, 0, 0, 255, 0];

        let rgba = rgb_to_rgba(&rgb, 2, 1).expect("変換できること");

        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn rgb_to_rgba_ignores_trailing_bytes_beyond_the_frame() {
        // 幅と高さから決まる長さだけを使う。余りが付いていても無視する
        let rgb = vec![1, 2, 3, 9, 9, 9];

        let rgba = rgb_to_rgba(&rgb, 1, 1).expect("変換できること");

        assert_eq!(rgba, vec![1, 2, 3, 255]);
    }

    #[test]
    fn rgb_to_rgba_short_data_returns_error() {
        // 2x2 なら 12 バイト要る。足りない分を 0 で埋めて渡すと、
        // 画面に出ている画と違うものがクリップボードへ入る
        let rgb = vec![0; 11];

        let err = rgb_to_rgba(&rgb, 2, 2).expect_err("エラーになること");

        assert_eq!(
            err,
            ScreenshotError::FrameTooShort {
                width: 2,
                height: 2,
                len: 11,
            }
        );
    }

    #[test]
    fn rgb_to_rgba_zero_sized_frame_returns_error() {
        assert!(rgb_to_rgba(&[], 0, 10).is_err());
        assert!(rgb_to_rgba(&[], 10, 0).is_err());
    }

    #[test]
    fn rgb_to_rgba_overflowing_size_returns_error() {
        // 壊れた値が来ても掛け算が一周して短い長さを通さないこと
        let err = rgb_to_rgba(&[0; 8], usize::MAX, 2).expect_err("エラーになること");

        assert_eq!(
            err,
            ScreenshotError::FrameTooLarge {
                width: usize::MAX,
                height: 2,
            }
        );
    }

    #[test]
    fn screenshot_error_display_keeps_the_numbers_and_the_underlying_reason() {
        // 文言はそのままトーストに出る。大きさや下位のエラー文が落ちると
        // 何が起きたのか分からなくなる
        let too_short = ScreenshotError::FrameTooShort {
            width: 2,
            height: 2,
            len: 11,
        };
        assert_eq!(
            too_short.to_string(),
            "映像フレームの画素が足りない: 2x2 に対して 11 バイト"
        );

        let clipboard = ScreenshotError::ClipboardOpenFailed("access denied".to_string());
        assert_eq!(
            clipboard.to_string(),
            "クリップボードを開けない: access denied"
        );

        let sound = ScreenshotError::SoundFileUnreadable {
            path: PathBuf::from("C:/sounds/SS.mp3"),
            source: "permission denied".to_string(),
        };
        assert_eq!(
            sound.to_string(),
            "効果音ファイル C:/sounds/SS.mp3 を読み込めないため既定の効果音を使う: permission denied"
        );
    }

    #[test]
    fn screenshot_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            ScreenshotError::EmptyFrame {
                width: 0,
                height: 10,
            },
            ScreenshotError::FrameTooLarge {
                width: usize::MAX,
                height: 2,
            },
            ScreenshotError::FrameTooShort {
                width: 2,
                height: 2,
                len: 11,
            },
            ScreenshotError::ClipboardOpenFailed("busy".to_string()),
            ScreenshotError::ClipboardWriteFailed("busy".to_string()),
            ScreenshotError::SoundFileUnreadable {
                path: PathBuf::from("C:/sounds/SS.mp3"),
                source: "missing".to_string(),
            },
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }

    #[test]
    fn clear_sound_discards_loaded_sound() {
        // 設定の効果音を「クリア」したセッションで鳴り続けていた不具合の再現。
        // 空パスは埋め込みの既定音へ倒れるので、実ファイルは要らない
        let mut manager = ScreenshotManager::new();
        manager
            .set_sound_file(Path::new(""))
            .expect("埋め込みの既定音は必ず読める");
        assert!(manager.sound_data.is_some());

        manager.clear_sound();

        assert!(manager.sound_data.is_none());
    }
}
