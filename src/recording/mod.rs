//! 録画。映像を H.264、音声を AAC にして、Media Foundation の Sink Writer で MP4 へ書き出す。
//!
//! 設計は `docs/design/recording.md`。いまは第 2 段（映像と音声）まで。
//! リプレイバッファ（③）はまだ無い。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `recorder.rs` | 録画スレッドの窓口 `Recorder`（UI スレッドが持つ）と、録画スレッドの本体 |
//! | `writer.rs` | Sink Writer の組み立てと書き込み、使っているエンコーダの名前 |
//! | `convert.rs` | RGB → NV12 の画素変換（純粋関数） |
//! | `audio.rs` | 音声トラック。`AudioTap` のリングから取り出し、48kHz 2ch の 16bit PCM へ寄せて PTS を付ける |
//! | `pts.rs` | 映像と音声の PTS（純粋関数） |
//! | `file_name.rs` | ファイル名の書式の検めと連番（純粋関数） |
//! | `storage.rs` | 保存先の空き容量 |
//!
//! 依存の向きは `app → recording → video / audio`。`video` と `audio` は録画を知らず、
//! フレームコールバックは `video::VideoTap`、入力コールバックは `audio::AudioTap` のリングへ
//! 積むだけ。**録画スレッドはデバイスに触らない**ので、「デバイスに触る使い捨ての
//! スレッドを作らない」には当たらない。

mod audio;
// ③ リプレイバッファの部品。録画スレッドから使うのは経路を切り替える段から。
// それまでは誰も呼ばないので、dead_code の警告を段の間だけ許す
#[allow(dead_code)]
mod bitstream;
mod convert;
#[allow(dead_code)]
mod encoder;
mod file_name;
mod pts;
mod recorder;
#[allow(dead_code)]
mod replay_ring;
mod storage;
mod writer;

pub use file_name::{render_file_name, resolve_file_stem, RECORDING_EXTENSION};
pub use recorder::{
    Recorder, RecordingAudioStats, RecordingEvent, RecordingRequest, RecordingSummary,
};

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::i18n::{self, Text};

/// 使っているエンコーダ。統計 OSD とログに出す（「壊れても原因が分かる」ため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// MFT の名前（`MFT_FRIENDLY_NAME_Attribute`）。持っていない MFT もある
    pub name: Option<String>,
    /// ハードウェアの MFT か。MFT が属性を返さなければ分からない（`None`）
    pub hardware: Option<bool>,
}

/// 録画が始められなかった、または途中で止まった理由。
///
/// **文字列ではなく種別で返す**（`docs/design/error-reporting.md`）。文言はこの型の
/// `Display` が `crate::i18n` から引く。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingError {
    /// 保存先を作れない・書けない
    Folder { path: PathBuf, reason: String },
    /// 保存先が空か相対パス。カレントディレクトリ基準にはしない
    FolderNotAbsolute { path: PathBuf },
    /// 保存先の空き容量が足りない（開始時、または録画中に 500MB を切った）
    DiskLow { free_mb: u64 },
    /// 書き込みに失敗した。ファイルは再生できないかもしれない
    WriteFailed { reason: String },
    /// H.264 / AAC のエンコーダを用意できない（音声を録らない設定なら H.264 だけ）
    EncoderUnavailable { reason: String },
    /// 録画中に映像の大きさが変わった。そこまでのファイルは閉じてある
    SizeChanged { from: (u32, u32), to: (u32, u32) },
    /// 映像が 1 枚も届かないまま止めた。ファイルは作っていない
    NoVideo,
    /// 録画スレッドを起こせない、COM や Media Foundation を初期化できない
    Platform { reason: String },
}

impl fmt::Display for RecordingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            RecordingError::Folder { path, reason } => {
                i18n::recording_folder_failed(path.display(), reason)
            }
            RecordingError::FolderNotAbsolute { path } => {
                i18n::recording_folder_not_absolute(path.display())
            }
            RecordingError::DiskLow { free_mb } => i18n::recording_disk_low(*free_mb),
            RecordingError::WriteFailed { reason } => i18n::recording_write_failed(reason),
            RecordingError::EncoderUnavailable { reason } => {
                i18n::recording_encoder_unavailable(reason)
            }
            RecordingError::SizeChanged { from, to } => {
                i18n::recording_size_changed(from.0, from.1, to.0, to.1)
            }
            RecordingError::NoVideo => Text::RecordingNoVideo.get().to_string(),
            RecordingError::Platform { reason } => i18n::recording_platform_failed(reason),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for RecordingError {}

/// 経過時間を `00:12:34` の形にする。右クリックメニューと録画中の印に使う。
/// 言語によらない表記なので `crate::i18n` を通さない。
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_pads_hours_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::ZERO), "00:00:00");
        assert_eq!(format_elapsed(Duration::from_millis(59_999)), "00:00:59");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "00:12:34");
        assert_eq!(format_elapsed(Duration::from_secs(36_000 + 61)), "10:01:01");
    }

    #[test]
    fn format_elapsed_beyond_99_hours_keeps_all_digits() {
        assert_eq!(format_elapsed(Duration::from_secs(360_000)), "100:00:00");
    }

    #[test]
    fn recording_error_display_is_localized() {
        let japanese = RecordingError::SizeChanged {
            from: (1920, 1080),
            to: (1280, 720),
        }
        .to_string();
        assert!(japanese.contains("1920x1080"), "{japanese}");
        assert!(japanese.contains("1280x720"), "{japanese}");

        let english = i18n::with_language(i18n::Language::English, || {
            RecordingError::NoVideo.to_string()
        });
        assert!(english.is_ascii(), "{english}");
    }
}
