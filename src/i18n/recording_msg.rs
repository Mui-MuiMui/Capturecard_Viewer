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

/// 保存先が空か相対パスのとき。空のときに「保存先  に」と読めない文にならないよう、
/// パスはかぎかっこで囲む。
pub fn recording_folder_not_absolute(path: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "保存先「{path}」はドライブ名（C:\\ など）かネットワークのパスから始まるフォルダではない。設定の「録画」タブで選び直してください"
        ),
        Language::English => format!(
            "The folder \"{path}\" does not start with a drive (such as C:\\) or a network path. Choose it again in the Recording tab of the settings"
        ),
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

/// リプレイバッファを通す録画で、リングを書き出すだけの空きが無いので始めなかったとき。
pub fn recording_replay_disk_short(free_mb: u64, required_mb: u64) -> String {
    match language() {
        Language::Japanese => format!(
            "保存先の空き容量が足りないので録画を始めなかった（残り {free_mb} MB、リプレイバッファの書き出しに {required_mb} MB 要る）"
        ),
        Language::English => format!(
            "Did not start recording because the disk does not have enough space ({free_mb} MB left, {required_mb} MB needed to write out the replay buffer)"
        ),
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
        Language::Japanese => format!("H.264 / AAC のエンコーダを用意できない: {reason}"),
        Language::English => format!("Cannot set up the H.264 / AAC encoder: {reason}"),
    }
}

// ---- エンコーダ MFT の失敗（recording::encoder::EncoderError）。上の文の理由に入る ----

pub fn recording_encoder_configure_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("エンコーダを設定できない: {reason}"),
        Language::English => format!("Cannot configure the encoder: {reason}"),
    }
}

pub fn recording_encoder_encode_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!("エンコードに失敗した: {reason}"),
        Language::English => format!("Encoding failed: {reason}"),
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

/// リプレイバッファを続けられない。`reason` は `RecordingError` の文言。
/// 設定を変えるまで作り直さず、その間の録画はさかのぼらずに行う。
pub fn recording_replay_failed(reason: impl Display) -> String {
    match language() {
        Language::Japanese => format!(
            "リプレイバッファ（さかのぼり録画）を使えない。録画はさかのぼらずに行う: {reason}"
        ),
        Language::English => format!(
            "The replay buffer is unavailable. Recordings will not include earlier footage: {reason}"
        ),
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

/// 設定ダイアログの「録画」タブに出す、いまの書式で作られるファイル名の例
pub fn recording_file_name_preview(file_name: impl Display) -> String {
    match language() {
        Language::Japanese => format!("例: {file_name}"),
        Language::English => format!("Example: {file_name}"),
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

/// 統計 OSD の録画の 1 行目に添える、リプレイバッファからさかのぼった長さ（秒）。
/// リプレイバッファを通した録画でだけ出す（ON であることだけでは出さない。#182 の決定）。
pub fn stats_recording_replay(seconds: u64) -> String {
    match language() {
        Language::Japanese => format!(" / さかのぼり {seconds} 秒"),
        Language::English => format!(" / replay {seconds} s"),
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

/// 統計 OSD の録画の音声の行。起点を揃えるために足した無音と削った入力の合計（ms）、
/// リングが溢れた回数。削った入力が 0 なら出さない（通常は録画の開始時に数 ms 削るだけ）。
pub fn stats_recording_audio(silence_ms: u64, trimmed_ms: u64, overflows: u64) -> String {
    match language() {
        Language::Japanese => {
            let trimmed = if trimmed_ms > 0 {
                format!(" / 削除 -{trimmed_ms} ms")
            } else {
                String::new()
            };
            format!("音声: 無音 +{silence_ms} ms{trimmed} / 溢れ {overflows} 回")
        }
        Language::English => {
            let trimmed = if trimmed_ms > 0 {
                format!(" / trimmed -{trimmed_ms} ms")
            } else {
                String::new()
            };
            format!("Audio: silence +{silence_ms} ms{trimmed} / overflows {overflows}")
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

    #[test]
    fn stats_recording_audio_shows_trim_only_when_trimmed() {
        let text = with_language(Language::Japanese, || stats_recording_audio(0, 0, 0));
        assert_eq!(text, "音声: 無音 +0 ms / 溢れ 0 回");
        let text = with_language(Language::Japanese, || stats_recording_audio(120, 8, 2));
        assert_eq!(text, "音声: 無音 +120 ms / 削除 -8 ms / 溢れ 2 回");
        let english = with_language(Language::English, || stats_recording_audio(120, 8, 2));
        assert_eq!(
            english,
            "Audio: silence +120 ms / trimmed -8 ms / overflows 2"
        );
    }

    #[test]
    fn stats_recording_replay_is_a_suffix_for_the_recording_line() {
        let text = with_language(Language::Japanese, || stats_recording_replay(28));
        assert_eq!(text, " / さかのぼり 28 秒");
        let english = with_language(Language::English, || stats_recording_replay(0));
        assert_eq!(english, " / replay 0 s");
    }

    #[test]
    fn recording_replay_failed_contains_the_reason_in_both_languages() {
        let text = with_language(Language::Japanese, || recording_replay_failed("理由"));
        assert!(text.ends_with("理由"), "{text}");
        let english = with_language(Language::English, || recording_replay_failed("reason"));
        assert!(
            english.is_ascii() && english.ends_with("reason"),
            "{english}"
        );
    }
}
