//! 引数を取る文字列のうち、録画（`src/recording/`、右クリックメニューの録画の項目、
//! 録画中の印、統計 OSD の録画の行、設定ダイアログの「録画」タブ）で使うもの。
//!
//! 書き方の決まりは `msg.rs` と同じ（1 関数が 1 件、文全体をここで組み立てる）。
//! 録画の段階（②③）で増えていくので、`update_msg.rs` にならって分けてある。

use std::fmt::Display;

use super::{language, Language};

// ---- 録画の失敗（recording::RecordingError） ----

pub fn recording_folder_failed(path: impl Display, reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("保存先 {path} に書き込めない: {reason}"),
        Language::English => format!("Cannot write to the folder {path}: {reason}"),
    }
}

pub fn recording_disk_low(free_mb: u64) -> String {
    match language() {
        Language::Japanese => {
            format!("保存先の空き容量が少ないので録画を止めた（残り {free_mb} MB）")
        }
        Language::English => {
            format!("Stopped recording because the disk is almost full ({free_mb} MB left)")
        }
    }
}

pub fn recording_write_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => {
            format!(
                "書き込みに失敗したので録画を止めた。ファイルは再生できないかもしれない: {reason}"
            )
        }
        Language::English => {
            format!("Stopped recording because writing failed. The file may not play: {reason}")
        }
    }
}

pub fn recording_encoder_unavailable(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("H.264 のエンコーダを用意できない: {reason}"),
        Language::English => format!("Cannot set up an H.264 encoder: {reason}"),
    }
}

pub fn recording_size_changed(
    from_width: u32,
    from_height: u32,
    to_width: u32,
    to_height: u32,
) -> String {
    match language() {
        Language::Japanese => format!(
            "映像の大きさが {from_width}x{from_height} から {to_width}x{to_height} に変わったので録画を止めた"
        ),
        Language::English => format!(
            "Stopped recording because the video size changed from {from_width}x{from_height} to {to_width}x{to_height}"
        ),
    }
}

pub fn recording_platform_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("録画の準備ができない: {reason}"),
        Language::English => format!("Cannot prepare for recording: {reason}"),
    }
}

// ---- ファイル名の書式（recording::file_name） ----

pub fn recording_file_name_forbidden_character(character: &char) -> String {
    let shown = if character.is_control() {
        format!("U+{:04X}", u32::from(*character))
    } else {
        character.to_string()
    };
    match language() {
        Language::Japanese => format!("ファイル名に使えない文字 {shown} が入ります"),
        Language::English => format!("The file name would contain {shown}, which is not allowed"),
    }
}

pub fn recording_file_name_reserved(name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("{name} は Windows が予約している名前なので使えません"),
        Language::English => format!("{name} is a name reserved by Windows"),
    }
}

// ---- 録画の結果と表示（app/recording.rs / app/menu/items.rs） ----

/// 録画を保存したときのトースト。`file_name` は拡張子まで含めたファイル名。
pub fn recording_saved(file_name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("録画を保存した: {file_name}"),
        Language::English => format!("Saved the recording: {file_name}"),
    }
}

/// 右クリックメニューの「録画を停止（00:12:34）」
pub fn menu_stop_recording(elapsed: impl Display) -> String {
    match language() {
        Language::Japanese => format!("録画を停止（{elapsed}）"),
        Language::English => format!("Stop recording ({elapsed})"),
    }
}

/// 統計 OSD の録画の 1 行目
pub fn stats_recording(elapsed: impl Display, written: u64, dropped: u64) -> String {
    match language() {
        Language::Japanese => format!("録画 {elapsed} / 書いた {written} / 捨てた {dropped}"),
        Language::English => format!("REC {elapsed} / written {written} / dropped {dropped}"),
    }
}

/// 統計 OSD の録画の 2 行目。エンコーダの名前とハードウェアかどうか。
/// `name` が `None` なら名前を取れなかった。`hardware` が `None` なら分からない。
pub fn stats_recording_encoder(name: Option<&str>, hardware: Option<bool>) -> String {
    match language() {
        Language::Japanese => {
            let kind = match hardware {
                Some(true) => "ハードウェア",
                Some(false) => "ソフトウェア",
                None => "種類不明",
            };
            format!("エンコーダ {}（{kind}）", name.unwrap_or("名前不明"))
        }
        Language::English => {
            let kind = match hardware {
                Some(true) => "hardware",
                Some(false) => "software",
                None => "unknown type",
            };
            format!("Encoder {} ({kind})", name.unwrap_or("unnamed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::with_language;

    #[test]
    fn recording_size_changed_names_both_sizes_in_every_language() {
        for language in [Language::Japanese, Language::English] {
            let text = with_language(language, || recording_size_changed(1920, 1080, 1280, 720));
            assert!(text.contains("1920x1080"), "{text}");
            assert!(text.contains("1280x720"), "{text}");
        }
    }

    #[test]
    fn recording_file_name_forbidden_character_shows_control_characters_as_code_points() {
        let text = recording_file_name_forbidden_character(&'\t');
        assert!(text.contains("U+0009"), "{text}");
        assert!(recording_file_name_forbidden_character(&':').contains(':'));
    }

    #[test]
    fn stats_recording_encoder_without_details_says_so() {
        let text = stats_recording_encoder(None, None);
        assert!(text.contains("名前不明"), "{text}");
        assert!(text.contains("種類不明"), "{text}");
        let english = with_language(Language::English, || {
            stats_recording_encoder(Some("NVIDIA H.264 Encoder MFT"), Some(true))
        });
        assert_eq!(english, "Encoder NVIDIA H.264 Encoder MFT (hardware)");
    }
}
