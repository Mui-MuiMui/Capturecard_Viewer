//! 映像のキャプチャと変換。
//!
//! 2,451 行あった `src/video.rs` を役割ごとに分けたもの。**分割は移動だけで、
//! 挙動は変えていない。** 外から見える経路は下の `pub use` で分割前と同じに
//! してある（`crate::video::VideoCapture` など）。
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `capture.rs` | nokhwa の開閉、フレームコールバック、途絶の観測 |
//! | `capabilities.rs` | `VideoMode` / `FormatCapability` と、デバイス能力の問い合わせ |
//! | `color.rs` | 係数表とその選択、映像調整の畳み込み、設定の共有 |
//! | `convert.rs` | YUY2 → RGB24 の画素変換 |
//! | `frame_buffer.rs` | `FrameBuffer` と世代番号、観測値（`FrameStats`） |
//!
//! ここに置いてあるのは、どのファイルからも使う `VideoError` と
//! ログの書式を揃えるための `elapsed_ms` だけ。

// `FormatCapability`（`capabilities`）と `IntervalStats`（`frame_buffer`）は
// 呼び出し側のテストからしか参照されない。再輸出すると、テストを含まない
// ビルドで誰も使わない `pub use` が残って `unused_imports` の警告になるので、
// この 2 つのモジュールだけ `pub(crate)` にして子モジュールの経路
// （`crate::video::capabilities::FormatCapability`）で参照してもらう。
// `src/ui/` と同じ考え方
pub(crate) mod capabilities;
mod capture;
mod color;
mod convert;
pub(crate) mod frame_buffer;

pub use capabilities::{DeviceCapabilities, VideoMode};
pub use capture::{ActiveVideo, VideoCapture, VideoLinkState};
pub use color::{SharedColorConversion, VideoAdjustments};
pub use frame_buffer::{FrameStats, VideoFrame, VideoFrames};

use std::fmt;
use std::time::Instant;

/// 映像デバイスの操作が失敗した理由。
///
/// **文字列ではなく種別で返す。** 呼び出し側（`app::worker_connect`）が
/// 「デバイスが見つからない」と「ストリームを開けない」を区別できるようにする
/// ため。下位のエラーは `nokhwa` の型をそのまま持ち回すと公開 API に
/// nokhwa が漏れるので、文字列に落として持たせる。
///
/// **表示用の日本語はこの型の `Display` が持つ。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoError {
    /// デバイスの列挙に失敗した
    DeviceQueryFailed(String),
    /// 設定に書かれた名前のデバイスが列挙結果に無い
    DeviceNotFound(String),
    /// デバイスが 1 台も見つからない（名前が未指定のとき）
    NoDevices,
    /// デバイスは見つかったが開けなかった
    CameraOpenFailed { device: String, source: String },
    /// デバイスは開けたがストリームを開始できなかった
    StreamOpenFailed { device: String, source: String },
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoError::DeviceQueryFailed(source) => {
                write!(f, "映像デバイスを列挙できない: {source}")
            }
            VideoError::DeviceNotFound(name) => {
                write!(f, "映像デバイス '{name}' が見つからない")
            }
            VideoError::NoDevices => write!(f, "映像デバイスが 1 台も見つからない"),
            VideoError::CameraOpenFailed { device, source } => {
                write!(f, "映像デバイス '{device}' を開けない: {source}")
            }
            VideoError::StreamOpenFailed { device, source } => {
                write!(
                    f,
                    "映像デバイス '{device}' のストリームを開けない: {source}"
                )
            }
        }
    }
}

impl std::error::Error for VideoError {}

/// 経過時間をミリ秒で返す。ログの書式を揃えるための補助。
fn elapsed_ms(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_error_display_keeps_the_device_name_and_the_underlying_reason() {
        // 文言はそのままトーストと「接続状態」タブに出る。デバイス名と
        // 下位のエラー文が落ちると、どの機器の何が起きたのか分からなくなる
        let not_found = VideoError::DeviceNotFound("Game Capture HD60".to_string());
        assert_eq!(
            not_found.to_string(),
            "映像デバイス 'Game Capture HD60' が見つからない"
        );

        let open_failed = VideoError::CameraOpenFailed {
            device: "Game Capture HD60".to_string(),
            source: "device in use".to_string(),
        };
        assert_eq!(
            open_failed.to_string(),
            "映像デバイス 'Game Capture HD60' を開けない: device in use"
        );

        let stream_failed = VideoError::StreamOpenFailed {
            device: "Game Capture HD60".to_string(),
            source: "MF_E_INVALIDMEDIATYPE".to_string(),
        };
        assert_eq!(
            stream_failed.to_string(),
            "映像デバイス 'Game Capture HD60' のストリームを開けない: MF_E_INVALIDMEDIATYPE"
        );
    }

    #[test]
    fn video_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ。
        // ASCII だけの文言が残っていないことで確かめる
        let all = [
            VideoError::DeviceQueryFailed("backend failure".to_string()),
            VideoError::DeviceNotFound("Capture".to_string()),
            VideoError::NoDevices,
            VideoError::CameraOpenFailed {
                device: "Capture".to_string(),
                source: "busy".to_string(),
            },
            VideoError::StreamOpenFailed {
                device: "Capture".to_string(),
                source: "busy".to_string(),
            },
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
}
