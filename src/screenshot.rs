use crate::i18n;
use crate::video::VideoFrame;
use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;

/// スクリーンショットまわりの処理が失敗した理由。
///
/// クリップボードへのコピーと効果音の読み込み・再生を 1 つの enum にまとめてある。
/// 型は共通だが、画面に出すときの発生源は分けてある。画像の出力（フレームと
/// クリップボード）は `ErrorSource::Screenshot`、効果音（`Sound*`）は
/// `ErrorSource::ScreenshotSound`（Issue #356）。画像を保存できているのに
/// 「スクリーンショットを出力できません」と出さないため。
///
/// **表示用の文言はこの型の `Display` が `crate::i18n` から引く。** 定型文
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
    /// 効果音ファイルは読めたが音声としてデコードできない（拡張子だけ mp3 など）。
    /// 既定音へは倒さず、撮影時は無音になる（`docs/design/assets.md`）
    SoundFileUndecodable { path: PathBuf, source: String },
    /// 効果音の出力先（既定の出力デバイス）を開けない。撮影しても無音になる
    SoundOutputUnavailable(String),
}

impl fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ScreenshotError::EmptyFrame { width, height } => {
                i18n::screenshot_empty_frame(*width, *height)
            }
            ScreenshotError::FrameTooLarge { width, height } => {
                i18n::screenshot_frame_too_large(width, height)
            }
            ScreenshotError::FrameTooShort { width, height, len } => {
                i18n::screenshot_frame_too_short(*width, *height, *len)
            }
            ScreenshotError::ClipboardOpenFailed(source) => {
                i18n::screenshot_clipboard_open_failed(source)
            }
            ScreenshotError::ClipboardWriteFailed(source) => {
                i18n::screenshot_clipboard_write_failed(source)
            }
            ScreenshotError::SoundFileUnreadable { path, source } => {
                i18n::sound_file_unreadable(path.display(), source)
            }
            ScreenshotError::SoundFileUndecodable { path, source } => {
                i18n::sound_file_undecodable(path.display(), source)
            }
            ScreenshotError::SoundOutputUnavailable(source) => {
                i18n::sound_output_unavailable(source)
            }
        };
        f.write_str(&text)
    }
}

impl std::error::Error for ScreenshotError {}

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

#[cfg(test)]
mod tests {
    use super::*;

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
            ScreenshotError::SoundFileUndecodable {
                path: PathBuf::from("C:/sounds/SS.mp3"),
                source: "unrecognized format".to_string(),
            },
            ScreenshotError::SoundOutputUnavailable("NoDevice".to_string()),
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
}
