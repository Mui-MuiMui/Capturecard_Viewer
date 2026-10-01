//! 録画の設定（[recording]）。ビットレート・リプレイバッファの長さの範囲と
//! serde の補助、保存先の既定値。

use log::warn;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// 録画の設定。設定ファイルでは [recording] になる（`docs/design/recording.md` の
// 「設定 `[recording]`」）。
//
// **項目は、その項目が効く段で足す。** 効かない項目を先に出さない
// （`docs/ARCHITECTURE.md` の「設定は実際に効かせる」）。
// 録画中に変えた設定は次の録画から効く。リプレイバッファの ON / OFF とさかのぼる長さは
// すぐ効く（ON にしたらその時点から溜め始める）。ただし、リプレイバッファを通さない録画の
// 最中に ON にしたときは、差し込み口が空くその録画の終わりから溜め始める。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    // 保存先のフォルダ。無ければ録画の開始時に作る
    pub folder: PathBuf,
    // ファイル名の書式（chrono の strftime）。拡張子（.mp4）は付けない。
    // 使う前に `recording::resolve_file_stem` が検め、使えなければ既定へ倒す
    pub file_name_format: String,
    // 映像の平均ビットレート（kbps）
    #[serde(deserialize_with = "deserialize_recording_bitrate")]
    pub video_bitrate_kbps: u32,
    // ハードウェアのエンコーダ（Intel / NVIDIA / AMD の MFT）を選ばせるか。
    // 選べなければソフトウェアのエンコーダへ倒れる
    pub hardware_encoder: bool,
    // 音声（AAC）も録るか。録るのは入力の音そのもので、音量・ミュート・
    // パススルーの無効は効かない（`docs/design/recording.md`）
    pub audio_enabled: bool,
    // 音声の平均ビットレート（kbps）。Microsoft の AAC エンコーダが受け付ける
    // RECORDING_AUDIO_BITRATES_KBPS の 4 つだけ。それ以外は近いものへ寄せる
    #[serde(deserialize_with = "deserialize_recording_audio_bitrate")]
    pub audio_bitrate_kbps: u32,
    // リプレイバッファ（さかのぼり録画、#182）。ON のあいだはエンコーダを常に回し、
    // 直近 replay_seconds 秒ぶんのエンコード済みの映像と音声をメモリに持つ。
    // 録画を始めると、その分を先頭に含める
    pub replay_enabled: bool,
    // さかのぼる長さ（秒）。MIN_REPLAY_SECONDS〜MAX_REPLAY_SECONDS（上限 5 分は #182 の決定）
    #[serde(deserialize_with = "deserialize_replay_seconds")]
    pub replay_seconds: u32,
    // 映像と音声のずれの補正（ms、#404）。録画の音声のサンプルを受け取った時刻に足す。
    // 正なら音声を遅らせ（先頭に無音が入る）、負なら早める（先頭の音声をそのぶん削る）。
    // MIN_RECORDING_AUDIO_OFFSET_MS〜MAX_RECORDING_AUDIO_OFFSET_MS。リプレイバッファにも効く
    #[serde(deserialize_with = "deserialize_audio_offset_ms")]
    pub audio_offset_ms: i32,
}

// 録画のファイル名の既定の書式
pub const DEFAULT_RECORDING_FILE_NAME_FORMAT: &str = "Recording_%Y-%m-%d_%H-%M-%S";

// 録画の映像のビットレート（kbps）の下限・上限と既定値
pub const MIN_RECORDING_BITRATE_KBPS: u32 = 1_000;

pub const MAX_RECORDING_BITRATE_KBPS: u32 = 50_000;

pub const DEFAULT_RECORDING_BITRATE_KBPS: u32 = 8_000;

// 録画の音声（AAC）のビットレート（kbps）の選択肢と既定値。
// Microsoft の AAC エンコーダが受け付けるのはこの 4 つだけ
// （`MF_MT_AUDIO_AVG_BYTES_PER_SECOND` = 12000 / 16000 / 20000 / 24000）
pub const RECORDING_AUDIO_BITRATES_KBPS: [u32; 4] = [96, 128, 160, 192];

pub const DEFAULT_RECORDING_AUDIO_BITRATE_KBPS: u32 = 160;

// リプレイバッファのさかのぼる長さ（秒）の下限・上限と既定値。
// 上限の 5 分は #182 の決定（8Mbps + 160kbps で約 300MB のメモリを使う）
pub const MIN_REPLAY_SECONDS: u32 = 5;

pub const MAX_REPLAY_SECONDS: u32 = 300;

pub const DEFAULT_REPLAY_SECONDS: u32 = 30;

// 録画の映像と音声のずれの補正（ms）の範囲（#404）。#398 の実測（音声が 50〜60ms 遅れる）に
// 対して両方向に余裕を持たせた
pub const MIN_RECORDING_AUDIO_OFFSET_MS: i32 = -200;

pub const MAX_RECORDING_AUDIO_OFFSET_MS: i32 = 200;

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            folder: default_recording_folder(),
            file_name_format: DEFAULT_RECORDING_FILE_NAME_FORMAT.to_string(),
            video_bitrate_kbps: DEFAULT_RECORDING_BITRATE_KBPS,
            hardware_encoder: true,
            audio_enabled: true,
            audio_bitrate_kbps: DEFAULT_RECORDING_AUDIO_BITRATE_KBPS,
            replay_enabled: false,
            replay_seconds: DEFAULT_REPLAY_SECONDS,
            audio_offset_ms: 0,
        }
    }
}

impl RecordingSettings {
    // エンコーダへ渡すビットレート。設定ダイアログからは範囲外を作れないが、
    // 読み込み後に値を差し替える経路もあるので、渡す前に丸めておく
    pub fn clamped_bitrate_kbps(&self) -> u32 {
        self.video_bitrate_kbps
            .clamp(MIN_RECORDING_BITRATE_KBPS, MAX_RECORDING_BITRATE_KBPS)
    }

    // 録画スレッドへ渡す音声のビットレート。音声を録らないなら None。
    // 4 つの選択肢のどれかへ寄せてから渡す（理由は clamped_bitrate_kbps と同じ）
    pub fn audio_bitrate_for_recording(&self) -> Option<u32> {
        self.audio_enabled
            .then(|| nearest_audio_bitrate_kbps(i64::from(self.audio_bitrate_kbps)))
    }

    // 録画スレッドへ渡すさかのぼる長さ。リプレイバッファが OFF なら None。
    // 範囲に丸めてから渡す（理由は clamped_bitrate_kbps と同じ）
    pub fn replay_seconds_for_recording(&self) -> Option<u32> {
        self.replay_enabled.then(|| {
            self.replay_seconds
                .clamp(MIN_REPLAY_SECONDS, MAX_REPLAY_SECONDS)
        })
    }

    // 録画スレッドへ渡す映像と音声のずれの補正（ms）。範囲に丸めてから渡す
    // （理由は clamped_bitrate_kbps と同じ）
    pub fn clamped_audio_offset_ms(&self) -> i32 {
        self.audio_offset_ms
            .clamp(MIN_RECORDING_AUDIO_OFFSET_MS, MAX_RECORDING_AUDIO_OFFSET_MS)
    }
}

// 範囲外のさかのぼる長さが書かれていても、設定全体を失わせない。
// 考え方は deserialize_recording_bitrate と同じ。
fn deserialize_replay_seconds<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_REPLAY_SECONDS), i64::from(MAX_REPLAY_SECONDS));
    if clamped != raw {
        warn!(
            "設定のリプレイバッファのさかのぼる長さ {} 秒は範囲外なので {} 秒として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

// 範囲外の映像と音声のずれの補正が書かれていても、設定全体を失わせない。
// 考え方は deserialize_recording_bitrate と同じ。
fn deserialize_audio_offset_ms<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(
        i64::from(MIN_RECORDING_AUDIO_OFFSET_MS),
        i64::from(MAX_RECORDING_AUDIO_OFFSET_MS),
    );
    if clamped != raw {
        warn!(
            "設定の録画の映像と音声のずれの補正 {} ms は範囲外なので {} ms として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので i32 に収まる
    Ok(clamped as i32)
}

// RECORDING_AUDIO_BITRATES_KBPS のうち `kbps` に最も近いもの。
// ちょうど中間（例: 112）なら高いほうへ寄せる（音質を落とさない側）
pub fn nearest_audio_bitrate_kbps(kbps: i64) -> u32 {
    RECORDING_AUDIO_BITRATES_KBPS
        .iter()
        .copied()
        .min_by_key(|&candidate| {
            let distance = (i64::from(candidate) - kbps).unsigned_abs();
            // 距離が同じなら高いほうを先にする
            (distance, std::cmp::Reverse(candidate))
        })
        .unwrap_or(DEFAULT_RECORDING_AUDIO_BITRATE_KBPS)
}

// 選択肢に無い音声のビットレートが書かれていても、設定全体を失わせない。
// 近い選択肢へ寄せる。考え方は deserialize_recording_bitrate と同じ。
fn deserialize_recording_audio_bitrate<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let nearest = nearest_audio_bitrate_kbps(raw);
    if i64::from(nearest) != raw {
        warn!(
            "設定の録画の音声のビットレート {} kbps は選べないので {} kbps として扱う",
            raw, nearest
        );
    }
    Ok(nearest)
}

// 範囲外のビットレートが書かれていても、設定全体を失わせない。
// 考え方は deserialize_jpeg_quality と同じで、TOML の整数である i64 で受けてから丸める。
fn deserialize_recording_bitrate<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(
        i64::from(MIN_RECORDING_BITRATE_KBPS),
        i64::from(MAX_RECORDING_BITRATE_KBPS),
    );
    if clamped != raw {
        warn!(
            "設定の録画のビットレート {} kbps は範囲外なので {} kbps として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

// 録画の保存先の既定値。
//
// ビデオフォルダ → デスクトップ → %USERPROFILE% → 実行ファイルの置き場所 → 一時フォルダ。
// **カレントディレクトリは使わない**（理由は default_screenshot_folder と同じ。
// `docs/design/assets.md`）。先頭の候補だけがスクリーンショットと違う。
fn default_recording_folder() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));

    recording_folder_from(
        dirs::video_dir(),
        dirs::desktop_dir(),
        dirs::home_dir(),
        exe_dir,
        std::env::temp_dir(),
    )
}

// 録画の保存先の候補から実際に使うものを選ぶ。選ぶ部分だけを切り出してテストする。
fn recording_folder_from(
    videos: Option<PathBuf>,
    desktop: Option<PathBuf>,
    home: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    last_resort: PathBuf,
) -> PathBuf {
    if let Some(videos) = videos {
        return videos;
    }
    let fallback = desktop.or(home).or(exe_dir).unwrap_or(last_resort);
    warn!(
        "ビデオフォルダの場所が分からないので、録画の保存先を {} にする",
        fallback.display()
    );
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::testing::{DESKTOP, EXE_DIR, FULL_CONFIG, HOME, TEMP};
    use crate::settings::AppSettings;

    const VIDEOS: &str = r"C:\Users\tester\Videos";

    #[test]
    fn recording_folder_from_videos_available_uses_videos() {
        let folder = recording_folder_from(
            Some(PathBuf::from(VIDEOS)),
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(folder, PathBuf::from(VIDEOS));
    }

    #[test]
    fn recording_folder_from_without_videos_falls_back_in_the_screenshot_order() {
        // ビデオフォルダが無ければ、スクリーンショットと同じ順（デスクトップ → ホーム → exe）
        let desktop = recording_folder_from(
            None,
            Some(PathBuf::from(DESKTOP)),
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );
        let home = recording_folder_from(
            None,
            None,
            Some(PathBuf::from(HOME)),
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );
        let exe = recording_folder_from(
            None,
            None,
            None,
            Some(PathBuf::from(EXE_DIR)),
            PathBuf::from(TEMP),
        );

        assert_eq!(desktop, PathBuf::from(DESKTOP));
        assert_eq!(home, PathBuf::from(HOME));
        assert_eq!(exe, PathBuf::from(EXE_DIR));
    }

    #[test]
    fn recording_folder_from_nothing_available_uses_the_last_resort_not_current_dir() {
        let folder = recording_folder_from(None, None, None, None, PathBuf::from(TEMP));

        assert_eq!(folder, PathBuf::from(TEMP));
        assert!(folder.is_absolute());
    }

    #[test]
    fn app_settings_without_recording_section_uses_recording_defaults() {
        // 録画が入る前の版が書いた設定ファイル。録画は既定値で、他の項目は保たれる
        assert!(!FULL_CONFIG.contains("[recording]"));

        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("[recording] が無くても読めなければならない");

        assert_eq!(
            settings.recording.file_name_format,
            DEFAULT_RECORDING_FILE_NAME_FORMAT
        );
        assert_eq!(settings.recording.video_bitrate_kbps, 8_000);
        assert!(settings.recording.hardware_encoder);
        assert!(settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 160);
        assert_eq!(
            settings.recording.folder,
            RecordingSettings::default().folder
        );
        assert_eq!(settings.screenshot.jpeg_quality, 60);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_partial_recording_section_keeps_the_other_defaults() {
        let config = format!("{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = 12000\n");

        let settings: AppSettings =
            toml::from_str(&config).expect("[recording] の一部だけでも読めなければならない");

        assert_eq!(settings.recording.video_bitrate_kbps, 12_000);
        assert_eq!(
            settings.recording.file_name_format,
            DEFAULT_RECORDING_FILE_NAME_FORMAT
        );
        assert!(settings.recording.hardware_encoder);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_recording_section_round_trips() {
        let original = AppSettings {
            recording: RecordingSettings {
                folder: PathBuf::from(r"D:\captures"),
                file_name_format: "clip_%Y%m%d_%H%M%S".to_string(),
                video_bitrate_kbps: 25_000,
                hardware_encoder: false,
                audio_enabled: false,
                audio_bitrate_kbps: 128,
                replay_enabled: true,
                replay_seconds: 120,
                audio_offset_ms: -80,
            },
            ..AppSettings::default()
        };

        let text = toml::to_string(&original).expect("書き出せる");
        assert!(text.contains("[recording]"), "{text}");
        let restored: AppSettings = toml::from_str(&text).expect("読み戻せる");

        assert_eq!(restored.recording, original.recording);
    }

    #[test]
    fn app_settings_out_of_range_recording_bitrate_is_clamped_without_losing_settings() {
        for (written, expected) in [
            (0, 1_000),
            (-5, 1_000),
            (999_999, 50_000),
            (1_000, 1_000),
            (50_000, 50_000),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("範囲外のビットレートでも読めなければならない");

            assert_eq!(settings.recording.video_bitrate_kbps, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn app_settings_recording_section_from_the_video_only_version_enables_audio() {
        // 第 1 段（映像のみ）の版が書いた [recording] には音声の項目が無い。
        // 構造体の既定値（音声を録る、160kbps）になり、書いてある項目は保たれる
        let config = format!(
            "{FULL_CONFIG}\n[recording]\nvideo_bitrate_kbps = 12000\nhardware_encoder = false\n"
        );

        let settings: AppSettings = toml::from_str(&config).expect("読めなければならない");

        assert!(settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 160);
        assert_eq!(settings.recording.video_bitrate_kbps, 12_000);
        assert!(!settings.recording.hardware_encoder);
    }

    #[test]
    fn app_settings_unlisted_recording_audio_bitrate_snaps_without_losing_settings() {
        for (written, expected) in [
            (0, 96),
            (-5, 96),
            (100, 96),
            (112, 128),
            (150, 160),
            (999_999, 192),
            (128, 128),
            (192, 192),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\naudio_bitrate_kbps = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("選べないビットレートでも読めなければならない");

            assert_eq!(settings.recording.audio_bitrate_kbps, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn app_settings_recording_section_from_the_audio_version_keeps_replay_off() {
        // 第 2 段（音声）の版が書いた [recording] にはリプレイバッファの項目が無い。
        // 既定（OFF、30 秒）で読み、他の値は失わない
        let config =
            format!("{FULL_CONFIG}\n[recording]\naudio_enabled = false\naudio_bitrate_kbps = 96\n");

        let settings: AppSettings = toml::from_str(&config).expect("読めなければならない");

        assert!(!settings.recording.replay_enabled);
        assert_eq!(settings.recording.replay_seconds, 30);
        assert!(!settings.recording.audio_enabled);
        assert_eq!(settings.recording.audio_bitrate_kbps, 96);
    }

    #[test]
    fn app_settings_out_of_range_replay_seconds_is_clamped_without_losing_settings() {
        for (written, expected) in [
            (0, 5),
            (-10, 5),
            (4, 5),
            (5, 5),
            (300, 300),
            (301, 300),
            (86_400, 300),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\nreplay_seconds = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("範囲外の長さでも読めなければならない");

            assert_eq!(settings.recording.replay_seconds, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn app_settings_audio_offset_round_trips_with_the_full_config() {
        // 補正が入る前の版の設定ファイルには項目が無い。0（補正しない）で読む
        let settings: AppSettings = toml::from_str(FULL_CONFIG).expect("読めなければならない");
        assert_eq!(settings.recording.audio_offset_ms, 0);

        // 負の値（音声を早める）も書き出して読み戻せる
        let config = format!("{FULL_CONFIG}\n[recording]\naudio_offset_ms = -80\n");
        let settings: AppSettings = toml::from_str(&config).expect("読めなければならない");
        assert_eq!(settings.recording.audio_offset_ms, -80);
        let text = toml::to_string(&settings).expect("書き出せる");
        let restored: AppSettings = toml::from_str(&text).expect("読み戻せる");
        assert_eq!(restored.recording, settings.recording);
    }

    #[test]
    fn app_settings_out_of_range_audio_offset_is_clamped_without_losing_settings() {
        for (written, expected) in [
            (-201, -200),
            (-200, -200),
            (0, 0),
            (200, 200),
            (201, 200),
            (i64::MAX, 200),
            (i64::MIN, -200),
        ] {
            let config = format!("{FULL_CONFIG}\n[recording]\naudio_offset_ms = {written}\n");

            let settings: AppSettings =
                toml::from_str(&config).expect("範囲外の補正でも読めなければならない");

            assert_eq!(settings.recording.audio_offset_ms, expected, "{written}");
            assert_eq!(settings.ui.volume, 80.0);
        }
    }

    #[test]
    fn recording_settings_clamped_audio_offset_stays_in_range() {
        // 読み込み後に差し替えられた範囲外の値も、渡す前に丸める
        let mut recording = RecordingSettings {
            audio_offset_ms: 1_000,
            ..RecordingSettings::default()
        };
        assert_eq!(recording.clamped_audio_offset_ms(), 200);
        recording.audio_offset_ms = -1_000;
        assert_eq!(recording.clamped_audio_offset_ms(), -200);
        recording.audio_offset_ms = 55;
        assert_eq!(recording.clamped_audio_offset_ms(), 55);
    }

    #[test]
    fn recording_settings_replay_seconds_for_recording_follows_the_switch() {
        let mut recording = RecordingSettings {
            replay_seconds: 60,
            ..RecordingSettings::default()
        };
        assert_eq!(recording.replay_seconds_for_recording(), None);
        recording.replay_enabled = true;
        assert_eq!(recording.replay_seconds_for_recording(), Some(60));
        // 読み込み後に差し替えられた範囲外の値も、渡す前に丸める
        recording.replay_seconds = 1_000;
        assert_eq!(recording.replay_seconds_for_recording(), Some(300));
        recording.replay_seconds = 1;
        assert_eq!(recording.replay_seconds_for_recording(), Some(5));
    }

    #[test]
    fn nearest_audio_bitrate_prefers_the_higher_one_at_the_midpoint() {
        assert_eq!(nearest_audio_bitrate_kbps(112), 128);
        assert_eq!(nearest_audio_bitrate_kbps(144), 160);
        assert_eq!(nearest_audio_bitrate_kbps(176), 192);
        assert_eq!(nearest_audio_bitrate_kbps(143), 128);
    }

    #[test]
    fn recording_settings_audio_bitrate_for_recording_follows_the_switch() {
        let mut recording = RecordingSettings {
            audio_bitrate_kbps: 130,
            ..RecordingSettings::default()
        };
        // 読み込んだあとに値を差し替えられても、渡す前に選択肢へ寄せる
        assert_eq!(recording.audio_bitrate_for_recording(), Some(128));
        recording.audio_enabled = false;
        assert_eq!(recording.audio_bitrate_for_recording(), None);
    }

    #[test]
    fn recording_settings_clamped_bitrate_stays_in_range() {
        let mut recording = RecordingSettings {
            video_bitrate_kbps: 10,
            ..RecordingSettings::default()
        };
        assert_eq!(recording.clamped_bitrate_kbps(), MIN_RECORDING_BITRATE_KBPS);
        recording.video_bitrate_kbps = 60_000;
        assert_eq!(recording.clamped_bitrate_kbps(), MAX_RECORDING_BITRATE_KBPS);
        recording.video_bitrate_kbps = 8_000;
        assert_eq!(recording.clamped_bitrate_kbps(), 8_000);
    }
}
