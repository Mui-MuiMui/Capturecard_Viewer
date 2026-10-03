//! 録画。映像を H.264、音声を AAC にして、Media Foundation の Sink Writer で MP4 へ書き出す。
//!
//! 設計は `docs/design/recording.md`。第 3 段（リプレイバッファ）まで。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `recorder.rs` | 録画スレッドの窓口 `Recorder`（UI スレッドが持ち、スレッドの寿命を決める）と、コマンド・イベント・観測値の型 |
//! | `recorder_loop.rs` | 録画スレッドの本体。コマンドの受け口と、リプレイバッファを通すかの経路の切り替え |
//! | `session.rs` | リプレイバッファを通さない録画（①②）。Sink Writer がエンコードも行う。失敗の扱いの共通部分 |
//! | `replay.rs` | リプレイバッファ（③）。エンコーダ MFT を回してエンコード済みのリングに持つ |
//! | `replay_config.rs` | リプレイバッファの設定（UI スレッドが組み立てて渡す）と、エンコーダの作り直しが要るかの判定 |
//! | `replay_recording.rs` | リプレイバッファを通す録画。リングからエンコードなしの Sink Writer へ書く |
//! | `replay_ring.rs` | エンコード済みのリングと、書き出す位置・捨てる境界・PTS の付け替え（純粋関数） |
//! | `replay_save.rs` | リプレイバッファの中身だけを保存する操作（#438）。録画か保存かの種類と、保存できない理由の判定（純粋関数） |
//! | `encoder.rs` | エンコーダ MFT（H.264 / AAC、同期型と非同期型） |
//! | `encoder_setup.rs` | エンコーダ MFT を作るときだけ使う補助（列挙、候補を先頭から開く、入出力の形の組み立て） |
//! | `passthrough.rs` | エンコードなしの Sink Writer |
//! | `bitstream.rs` | H.264 の IDR と SPS / PPS の読み取り、AAC の `MF_MT_USER_DATA`（純粋関数） |
//! | `writer.rs` | Sink Writer の組み立てと書き込み、使っているエンコーダの名前 |
//! | `sample_pool.rs` | Sink Writer とエンコーダ MFT へ渡す NV12 のサンプルの使い回し |
//! | `convert.rs` | RGB → NV12 の画素変換（純粋関数） |
//! | `audio.rs` | 音声トラック。`AudioTap` のリングから取り出し、48kHz 2ch の 16bit PCM へ寄せて PTS を付ける |
//! | `pts.rs` | 映像と音声の PTS（純粋関数） |
//! | `file_name.rs` | ファイル名の書式の検めと連番（純粋関数） |
//! | `storage.rs` | 保存先の空き容量 |
//! | `test_support.rs` | 録画のテストの補助（`#[cfg(test)]`。フェイクを流して録画し、書いた MP4 を読み戻す） |
//!
//! 依存の向きは `app → recording → video / audio`。`video` と `audio` は録画を知らず、
//! フレームコールバックは `video::VideoTap`、入力コールバックは `audio::AudioTap` のリングへ
//! 積むだけ。**録画スレッドはデバイスに触らない**ので、「デバイスに触る使い捨ての
//! スレッドを作らない」には当たらない。

mod audio;
mod bitstream;
mod convert;
mod encoder;
mod encoder_setup;
mod file_name;
mod passthrough;
mod pts;
mod recorder;
mod recorder_loop;
mod replay;
mod replay_config;
mod replay_recording;
mod replay_ring;
mod replay_save;
mod sample_pool;
mod session;
mod storage;
#[cfg(test)]
mod test_support;
mod writer;

pub use file_name::{render_file_name, resolve_file_stem, RECORDING_EXTENSION};
pub use recorder::{
    Recorder, RecordingAudioStats, RecordingEvent, RecordingRequest, RecordingSummary,
    ReplayRingStats,
};
pub use replay_config::ReplayConfig;
pub use replay_save::SaveReplayBlock;

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
    /// リプレイバッファを通す録画で、リングを書き出すと 500MB を切る。始めていない（#313）
    ReplayDiskShort { free_mb: u64, required_mb: u64 },
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
    /// 録画スレッドへコマンドを送れない（スレッドが既に終わっている）
    ThreadStopped,
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
            RecordingError::ReplayDiskShort {
                free_mb,
                required_mb,
            } => i18n::recording_replay_disk_short(*free_mb, *required_mb),
            RecordingError::WriteFailed { reason } => i18n::recording_write_failed(reason),
            RecordingError::EncoderUnavailable { reason } => {
                i18n::recording_encoder_unavailable(reason)
            }
            RecordingError::SizeChanged { from, to } => {
                i18n::recording_size_changed(from.0, from.1, to.0, to.1)
            }
            RecordingError::NoVideo => Text::RecordingNoVideo.get().to_string(),
            RecordingError::Platform { reason } => i18n::recording_platform_failed(reason),
            RecordingError::ThreadStopped => {
                i18n::recording_platform_failed(Text::RecordingThreadStopped.get())
            }
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

    // #315: 録画スレッドが止まっているときの文言が英語の画面で日本語にならない
    #[test]
    fn thread_stopped_display_follows_the_language() {
        let english = i18n::with_language(i18n::Language::English, || {
            RecordingError::ThreadStopped.to_string()
        });
        assert_eq!(
            english,
            "Cannot prepare for recording: The recording thread has stopped"
        );
        let japanese = i18n::with_language(i18n::Language::Japanese, || {
            RecordingError::ThreadStopped.to_string()
        });
        assert_eq!(japanese, "録画の準備ができない: 録画スレッドが止まっている");
    }
}
