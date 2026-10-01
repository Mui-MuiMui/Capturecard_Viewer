//! 音声の設定（[audio]）。サンプリングレートとチャンネル数の既定値、
//! リングバッファの長さの範囲と、その serde の補助。

use log::warn;
use serde::{Deserialize, Serialize};

// PartialEq は VideoSettings と同じくプリセットとの一致判定で使う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    // 入力の種類。WASAPI のデバイス（既定）か、DirectShow で開いた映像デバイスの
    // 音声ピンか（#388）。"video_pin" の間は input_device_name を使わないが消さない
    // （WASAPI のデバイスへ戻したときに前の選択を戻すため）。
    // 知らない値が書かれていても設定全体を失わせない（deserialize_input_source）
    #[serde(deserialize_with = "deserialize_input_source")]
    pub input_source: AudioInputSource,
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
    // 以下 2 項目は「希望値」。実際に開く値はデバイスの能力に合わせて
    // `audio::select_best_config` が寄せるため、ここと食い違うことがある
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub passthrough_enabled: bool,
    // 入力から出力へ受け渡すリングバッファの長さ（ミリ秒）。
    //
    // 小さいほど遅延が減るが、出力コールバックが間に合わずプチプチという
    // 音（アンダーラン）が出やすくなる。最適な値は環境ごとに違うので
    // 設定ダイアログのスライダーで選ばせる。
    //
    // **同じ [audio] の sample_rate / channels と違い `Option` にしない。**
    // あちらは「希望値」で、デバイスの能力に合わせて寄せられる余地があるが、
    // バッファ長はこちらで好きに決められるので寄せ先が無い。
    //
    // 範囲外の値が書かれていても設定全体を失わせない（deserialize_buffer_ms）
    #[serde(deserialize_with = "deserialize_buffer_ms")]
    pub buffer_ms: u32,
}

// オーディオのサンプリングレートとチャンネル数の既定値。
// 設定に値が入っていないときの表示にも使うので、設定画面側と揃うよう定数にしてある
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

pub const DEFAULT_CHANNELS: u16 = 2;

// 音声のリングバッファの長さ（ミリ秒）の下限・上限と既定値。
//
// 下限を 20ms にしてあるのは、WASAPI 共有モードの出力コールバックが
// 10ms 前後の周期で呼ばれるため。それを下回る長さにすると、1 回の
// コールバックで使い切ってしまい常に音が途切れる。
//
// 上限の 200ms は、遅延として体感できる上限の目安。これ以上を選べても
// 「映像より音が遅れている」状態を積むだけで、低遅延という目的から外れる。
//
// 既定の 50ms は設定項目になる前にハードコードされていた長さ。
// 更新しても既存ユーザーの音の出かたが変わらないようにしてある
pub const MIN_BUFFER_MS: u32 = 20;

pub const MAX_BUFFER_MS: u32 = 200;

pub const DEFAULT_BUFFER_MS: u32 = 50;

// 範囲外の音声バッファ長が書かれていても、設定全体を失わせない。
// 考え方は deserialize_jpeg_quality と同じで、TOML の整数である i64 で
// 受けてから 20〜200ms へ丸める。u32 のまま読むと、手で書き換えられた
// 負の値でパースがファイル単位で失敗し、無関係な項目まで既定値へ戻る。
fn deserialize_buffer_ms<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i64::deserialize(deserializer)?;
    let clamped = raw.clamp(i64::from(MIN_BUFFER_MS), i64::from(MAX_BUFFER_MS));
    if clamped != raw {
        warn!(
            "設定の音声バッファ長 {} ms は範囲外なので {} ms として扱う",
            raw, clamped
        );
    }
    // clamp 済みなので u32 に収まる
    Ok(clamped as u32)
}

// 音声の入力の種類。設定ファイルには input_source = "device" / "video_pin" と
// 書かれる。**一度出した名前は変えない**（設定に残る識別子。`HotkeyAction::as_str` と
// 同じ理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AudioInputSource {
    // WASAPI の入力デバイス（input_device_name）。キーの無い既存の設定ファイルもこれ
    #[default]
    #[serde(rename = "device")]
    Device,
    // DirectShow で開いた映像デバイスの音声ピン。映像を DirectShow で開いたとき
    // だけ鳴る（docs/design/directshow-audio.md）
    #[serde(rename = "video_pin")]
    VideoPin,
}

// 設定ファイルの input_source に知らない値が書かれていても、設定全体を
// 失わせない。映像の開き方（deserialize_video_backend）と同じ考え方で、
// 今までどおり WASAPI の入力として扱う
fn deserialize_input_source<'de, D>(deserializer: D) -> Result<AudioInputSource, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(input_source_from_str(&raw).unwrap_or_else(|| {
        warn!(
            "設定の音声の入力の種類 \"{}\" を解釈できないので device として扱う",
            raw
        );
        AudioInputSource::default()
    }))
}

// 設定ファイルに書かれた文字列から入力の種類を決める。解釈できない場合は None。
// 手書きされることを見込んで、大文字小文字と前後の空白は問わない
fn input_source_from_str(raw: &str) -> Option<AudioInputSource> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "device" => Some(AudioInputSource::Device),
        "video_pin" => Some(AudioInputSource::VideoPin),
        _ => None,
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_source: AudioInputSource::Device,
            input_device_name: None,
            output_device_name: None,
            sample_rate: Some(DEFAULT_SAMPLE_RATE),
            channels: Some(DEFAULT_CHANNELS),
            passthrough_enabled: true,
            // 既定は 50ms。この値が設定項目になる前にハードコードされていた
            // 長さと同じで、更新しても音の出かたが変わらない
            buffer_ms: DEFAULT_BUFFER_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::testing::{without_key, FULL_CONFIG};
    use crate::settings::AppSettings;

    #[test]
    fn app_settings_missing_audio_buffer_key_uses_the_previous_hardcoded_length() {
        // 音声バッファを設定項目にする前の版が書いた設定ファイル。
        // 既定は当時ハードコードされていた 50ms で、音の出かたが変わらない
        let config = without_key(FULL_CONFIG, "buffer_ms");
        assert!(
            !config.contains("buffer_ms ="),
            "テスト用の設定から buffer_ms が消えていない"
        );

        let settings: AppSettings =
            toml::from_str(&config).expect("buffer_ms が欠けていても読めなければならない");

        assert_eq!(settings.audio.buffer_ms, DEFAULT_BUFFER_MS);
        assert_eq!(settings.audio.sample_rate, Some(44100));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_out_of_range_audio_buffer_is_clamped_without_losing_settings() {
        // 手で書き換えて桁を間違えた場合。バッファ長だけが範囲に収まり、
        // 無関係な項目は保持されなければならない
        let config = FULL_CONFIG.replace("buffer_ms = 120", "buffer_ms = 5000");

        let settings: AppSettings =
            toml::from_str(&config).expect("範囲外のバッファ長でも読めなければならない");

        assert_eq!(settings.audio.buffer_ms, MAX_BUFFER_MS);
        assert_eq!(settings.audio.channels, Some(1));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_negative_audio_buffer_is_clamped_to_minimum() {
        // u32 のまま読むと負の値でファイル単位のパースが落ちる。
        // i64 で受けてから丸めているので、他の項目まで失わない
        let config = FULL_CONFIG.replace("buffer_ms = 120", "buffer_ms = -1");

        let settings: AppSettings =
            toml::from_str(&config).expect("負のバッファ長でも読めなければならない");

        assert_eq!(settings.audio.buffer_ms, MIN_BUFFER_MS);
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn app_settings_input_source_survives_a_save_and_load_roundtrip() {
        // FULL_CONFIG は既定値（device）と違う video_pin を書いてある
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("テスト用の設定を読めること");
        assert_eq!(settings.audio.input_source, AudioInputSource::VideoPin);
        // video_pin の間も、WASAPI のデバイスの選択は残っている
        assert_eq!(settings.audio.input_device_name.as_deref(), Some("Line In"));

        let written = toml::to_string(&settings).expect("設定を書き出せること");
        assert!(
            written.contains("input_source = \"video_pin\""),
            "設定ファイル上の名前で書かれていない: {written}"
        );
        let restored: AppSettings = toml::from_str(&written).expect("書き出した設定を読めること");
        assert_eq!(restored.audio.input_source, AudioInputSource::VideoPin);
    }

    #[test]
    fn app_settings_missing_input_source_is_device() {
        // input_source を足す前の版が書いた設定ファイル。今までどおり WASAPI の入力で開く
        let config = without_key(FULL_CONFIG, "input_source");
        let settings: AppSettings = toml::from_str(&config).expect("欠けていても読める");
        assert_eq!(settings.audio.input_source, AudioInputSource::Device);
        assert_eq!(settings.audio.buffer_ms, 120);
    }

    #[test]
    fn app_settings_unknown_input_source_is_device_without_losing_settings() {
        let config = FULL_CONFIG.replace(
            "input_source = \"video_pin\"",
            "input_source = \"hdmi_magic\"",
        );
        let settings: AppSettings = toml::from_str(&config).expect("知らない値でも読める");
        assert_eq!(settings.audio.input_source, AudioInputSource::Device);
        assert_eq!(settings.audio.channels, Some(1));
        assert_eq!(settings.ui.volume, 80.0);
    }

    #[test]
    fn input_source_from_str_accepts_the_written_names_loosely() {
        assert_eq!(
            input_source_from_str("device"),
            Some(AudioInputSource::Device)
        );
        assert_eq!(
            input_source_from_str(" Video_Pin "),
            Some(AudioInputSource::VideoPin)
        );
        assert_eq!(input_source_from_str(""), None);
        assert_eq!(input_source_from_str("wasapi"), None);
    }

    #[test]
    fn app_settings_audio_buffer_survives_a_save_and_load_roundtrip() {
        // 既定値と違う値が TOML を往復しても保たれること。
        // [audio] へ書き出されなければ、次の起動で 50ms へ戻ってしまう
        let settings: AppSettings =
            toml::from_str(FULL_CONFIG).expect("テスト用の設定を読めること");
        assert_eq!(settings.audio.buffer_ms, 120);

        let written = toml::to_string(&settings).expect("設定を書き出せること");
        let restored: AppSettings = toml::from_str(&written).expect("書き出した設定を読めること");

        assert_eq!(restored.audio.buffer_ms, 120);
    }
}
