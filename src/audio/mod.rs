//! 音声のキャプチャと再生。
//!
//! cpal による「入力 → リングバッファ → 出力」のパススルーを、役割ごとに
//! 分けてある。**この `mod.rs` が持つのは、どのファイルからも使う型
//! （向き・エラー・実際に開いた内容）と、外向きの `pub use` だけ。**
//!
//! | ファイル | 役割 |
//! |---|---|
//! | `capabilities.rs` | デバイスの対応設定の取得と、設定画面に出す選択肢の組み立て |
//! | `stream_config.rs` | 対応設定の中から、実際に開く設定を選ぶ |
//! | `capture.rs` | `AudioCapture`。パススルーの開始と停止、観測値の取り出し |
//! | `stream.rs` | cpal の入力ストリームの組み立てと入力のコールバック、リングバッファの型、ストリームのエラーの扱い |
//! | `stream_output.rs` | cpal の出力ストリームの組み立てと出力のコールバック、アンダーランの数え方 |
//! | `passthrough_output.rs` | パススルーの出力側の組み立て（出力デバイス、リングバッファ、変換器と補正を付けた出力ストリーム）。入力の種類によらず共有する |
//! | `convert.rs` | 入出力の形が違う場合の変換（線形補間とミックス） |
//! | `sample.rs` | サンプル型の変換（f32 ⇄ i16 / u16 / i32） |
//! | `resample.rs` | クロックドリフト補正の共有状態と、補正係数の決め方 |
//! | `controls.rs` | 音量・パススルー・ミュートの共有状態 |
//! | `fake.rs` | 実機なしで動くフェイクの音声デバイス（正弦波の入力と、書き込みを捨てる出力）。環境変数で有効にしたときだけ使う |
//! | `fake_stream.rs` | フェイクの入出力のスレッドの本体（正弦波を吐く入力と、書き込みを捨てる出力） |
//! | `pin_feed.rs` | DirectShow の映像デバイスの音声ピンと `AudioCapture` をつなぐ差し込み口（`AudioPinFeed`）。音声ピンの `Receive` が受け取った PCM を `process_input_iter` へ渡す。音声ピンの状態（`AudioPinState`）と形式もここ |
//! | `tap.rs` | 録画へ音声を回す差し込み口（`AudioTap`）。録画中だけ、入力コールバックが f32 へ直した値を入力の形のまま録画のリングへも積む。PTS を決めるための累計・時刻・入力の形・開き直しの番号も持つ |

mod capabilities;
mod capture;
mod controls;
mod convert;
mod fake;
mod fake_stream;
mod passthrough_output;
mod pin_feed;
mod resample;
mod sample;
mod stream;
mod stream_config;
mod stream_output;
mod tap;

// `audio` の外から使うものだけを並べる。**使われていない再輸出は
// `unused_imports` で落ちる**（このクレートは bin だけで lib を持たないため、
// 外へ公開するという意味を持たない）。`AudioChoices` のように、返り値として
// 受け取るだけで名前を書かない型はここに載せない
pub use capabilities::{
    nearest_channels, nearest_sample_rate, query_capabilities, selectable_channels,
    selectable_sample_rates, AudioCapabilities, ChoiceSource,
};
pub use capture::{AudioCapture, PassthroughInput, PassthroughRequest};
pub use controls::AudioControls;
pub use fake::{FakeAudioCapture, FakeAudioOptions};
pub use pin_feed::{
    AudioPinFeed, AudioPinState, PinConnection, PinFailure, PinFormat, PinSampleType,
};
pub(crate) use resample::decide_resample_correction;
pub use resample::{ResampleStatus, ResampleTelemetry};
// 録画スレッド（`crate::recording`）が録画用に 1 つ持つ変換器と、16bit PCM への変換
pub(crate) use convert::PassthroughConverter;
pub(crate) use sample::f32_to_i16;
pub use tap::{AudioTap, AudioTapConsumer, AudioTapSnapshot};

use cpal::SampleFormat;
use std::fmt;

use crate::i18n::{self, Text};

/// 実際に開いた音声ストリームの内容。
///
/// 設定ダイアログの「接続状態」タブに出すために持つ。**設定に書かれた値では
/// なく、`select_best_config` が確定させた値を入れる。** 設定画面の選択肢は
/// 入出力の両方が対応する値に絞ってあるが、能力を取得できなかったデバイスでは
/// 既定の一覧を出すため、選んだ値と実際の値は食い違いうる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAudio {
    /// 実際に開いた入力デバイス名
    pub input_device: String,
    /// 実際に開いた出力デバイス名
    pub output_device: String,
    /// 入力のサンプリングレート（Hz）とチャンネル数
    pub input_sample_rate: u32,
    pub input_channels: u16,
    /// 出力のサンプリングレート（Hz）とチャンネル数
    pub output_sample_rate: u32,
    pub output_channels: u16,
    /// 入力の経路。WASAPI のデバイスか、映像デバイスの音声ピンか
    pub input_route: AudioInputRoute,
    /// 音声ピンの塊の長さに合わせてリングバッファを広げたときの内訳。
    /// 設定のまま開いたときは `None`
    pub widened_buffer: Option<WidenedBuffer>,
}

/// 音声の入力の経路。「接続状態」タブの入力の欄と、ワーカーの開き直しの判定に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioInputRoute {
    /// WASAPI の入力デバイス（cpal の入力ストリーム）
    #[default]
    Device,
    /// 映像デバイスの音声ピン。`graph` は差し込んだグラフの番号
    /// （`AudioPinFeed::begin_graph`）で、映像を開き直すと映像側の番号と食い違う
    VideoPin { graph: u64 },
}

/// 音声ピンの塊が長くてリングバッファを広げたときの内訳（すべて ms）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidenedBuffer {
    /// 設定のバッファ長
    pub configured_ms: u32,
    /// 実際に開いた長さ
    pub actual_ms: u32,
    /// 音声ピンの 1 塊の長さ
    pub chunk_ms: u32,
}

impl ActiveAudio {
    /// 入力側を 1 行で表す。
    pub fn input_summary(&self) -> String {
        format!("{}Hz {}ch", self.input_sample_rate, self.input_channels)
    }

    /// 出力側を 1 行で表す。
    pub fn output_summary(&self) -> String {
        format!("{}Hz {}ch", self.output_sample_rate, self.output_channels)
    }
}

/// 音声デバイスの向き。能力の取得とログの文言で使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioDirection {
    Input,
    Output,
}

impl AudioDirection {
    /// ログと画面に出す呼び名。
    pub fn label(self) -> &'static str {
        match self {
            AudioDirection::Input => Text::Input.get(),
            AudioDirection::Output => Text::Output.get(),
        }
    }
}

/// 音声デバイスの操作が失敗した理由。
///
/// **文字列ではなく種別で返す。** 呼び出し側（`app::worker_connect`）が
/// 「デバイスが見つからない」と「ストリームを組み立てられない」を区別できる
/// ようにするため。下位のエラーは cpal の型が段ごとに違う（`DevicesError` /
/// `DefaultStreamConfigError` / `BuildStreamError` / `PlayStreamError`）ので、
/// 文字列に落として持たせる。
///
/// どのバリアントも入力・出力のどちらで起きたかを持つ。音声は 2 本の
/// ストリームを開くため、向きが分からないと設定のどちらを直せばよいか
/// 伝えられない。
///
/// **表示用の文言はこの型の `Display` が `crate::i18n` から引く。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// デバイスの列挙に失敗した
    DeviceEnumerationFailed {
        direction: AudioDirection,
        source: String,
    },
    /// 設定に書かれた名前のデバイスが列挙結果に無い
    DeviceNotFound {
        direction: AudioDirection,
        name: String,
    },
    /// 名前が未指定なのに、Windows の既定デバイスが無い
    NoDefaultDevice(AudioDirection),
    /// デバイスの既定設定（WASAPI のミックスフォーマット）を取得できない
    DefaultConfigFailed {
        direction: AudioDirection,
        source: String,
    },
    /// デバイスの対応設定を列挙できない
    SupportedConfigsFailed {
        direction: AudioDirection,
        source: String,
    },
    /// 選ばれた設定のサンプルフォーマットを扱えない
    UnsupportedSampleFormat {
        direction: AudioDirection,
        format: SampleFormat,
    },
    /// ストリームを組み立てられない
    StreamBuildFailed {
        direction: AudioDirection,
        source: String,
    },
    /// 組み立てたストリームを開始できない
    StreamPlayFailed {
        direction: AudioDirection,
        source: String,
    },
    /// 入力が映像デバイスの音声ピンなのに、指定のグラフの音声ピンが繋がっていない。
    /// ワーカーは繋がっているのを確かめてから開くので、ふつうは起きない
    VideoPinUnavailable,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            AudioError::DeviceEnumerationFailed { direction, source } => {
                i18n::audio_device_enumeration_failed(direction.label(), source)
            }
            AudioError::DeviceNotFound { direction, name } => {
                i18n::audio_device_not_found(direction.label(), name)
            }
            AudioError::NoDefaultDevice(direction) => {
                i18n::audio_no_default_device(direction.label())
            }
            AudioError::DefaultConfigFailed { direction, source } => {
                i18n::audio_default_config_failed(direction.label(), source)
            }
            AudioError::SupportedConfigsFailed { direction, source } => {
                i18n::audio_supported_configs_failed(direction.label(), source)
            }
            AudioError::UnsupportedSampleFormat { direction, format } => {
                i18n::audio_unsupported_sample_format(direction.label(), format)
            }
            AudioError::StreamBuildFailed { direction, source } => {
                i18n::audio_stream_build_failed(direction.label(), source)
            }
            AudioError::StreamPlayFailed { direction, source } => {
                i18n::audio_stream_play_failed(direction.label(), source)
            }
            AudioError::VideoPinUnavailable => Text::AudioPinUnavailable.get().to_string(),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for AudioError {}

/// 出力デバイスが「デフォルト」（設定上は `None`）のときに、能力キャッシュの
/// キーとして使う名前。
///
/// キャッシュはデバイス名の文字列で引くため、「既定のデバイス」を表す口が要る。
/// 山括弧で囲んだ日本語は Windows のデバイスのフレンドリ名には現れないので、
/// 実在のデバイス名と衝突しない。
pub const DEFAULT_DEVICE_KEY: &str = "<既定のデバイス>";

/// 設定に書かれたデバイス名を、能力キャッシュのキーへ直す。
///
/// 未選択（`None` や空文字）は「既定のデバイス」を指すキーにする。キャッシュは
/// 空のキーを無視するため、そのまま渡すと出力が「デフォルト」のときに
/// 対応設定を取りに行かない。
pub fn cache_key(device_name: Option<&str>) -> String {
    match device_name {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => DEFAULT_DEVICE_KEY.to_string(),
    }
}

/// 能力キャッシュのキーを、`cpal` へ渡すデバイス名へ戻す。
///
/// `DEFAULT_DEVICE_KEY` と空文字は「既定のデバイス」を表す `None` になる。
pub fn device_name_from_key(key: &str) -> Option<&str> {
    if key.is_empty() || key == DEFAULT_DEVICE_KEY {
        None
    } else {
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::SupportedStreamConfigRange;

    #[test]
    fn audio_error_unsupported_sample_format_names_the_format() {
        // 「何が未対応だったか」が分からないと原因にたどり着けない
        let message = AudioError::UnsupportedSampleFormat {
            direction: AudioDirection::Input,
            format: SampleFormat::U32,
        }
        .to_string();

        assert!(message.contains("入力"), "{message}");
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn audio_error_display_names_the_direction_and_the_device() {
        // 音声は入力と出力の 2 本を開くので、向きが落ちると設定のどちらを
        // 直せばよいか伝わらない
        let not_found = AudioError::DeviceNotFound {
            direction: AudioDirection::Output,
            name: "スピーカー (Realtek)".to_string(),
        };
        assert_eq!(
            not_found.to_string(),
            "出力デバイス 'スピーカー (Realtek)' が見つからない"
        );

        assert_eq!(
            AudioError::NoDefaultDevice(AudioDirection::Input).to_string(),
            "既定の入力デバイスがない"
        );

        let build_failed = AudioError::StreamBuildFailed {
            direction: AudioDirection::Input,
            source: "device unavailable".to_string(),
        };
        assert_eq!(
            build_failed.to_string(),
            "入力ストリームを組み立てられない: device unavailable"
        );
    }

    #[test]
    fn audio_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            AudioError::DeviceEnumerationFailed {
                direction: AudioDirection::Input,
                source: "backend failure".to_string(),
            },
            AudioError::DeviceNotFound {
                direction: AudioDirection::Input,
                name: "Mic".to_string(),
            },
            AudioError::NoDefaultDevice(AudioDirection::Output),
            AudioError::DefaultConfigFailed {
                direction: AudioDirection::Output,
                source: "no config".to_string(),
            },
            AudioError::SupportedConfigsFailed {
                direction: AudioDirection::Input,
                source: "no config".to_string(),
            },
            AudioError::UnsupportedSampleFormat {
                direction: AudioDirection::Output,
                format: SampleFormat::U32,
            },
            AudioError::StreamBuildFailed {
                direction: AudioDirection::Input,
                source: "busy".to_string(),
            },
            AudioError::StreamPlayFailed {
                direction: AudioDirection::Output,
                source: "busy".to_string(),
            },
            AudioError::VideoPinUnavailable,
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }
    #[test]
    fn cache_key_round_trips_through_device_name() {
        assert_eq!(cache_key(Some("Line In")), "Line In");
        assert_eq!(device_name_from_key("Line In"), Some("Line In"));

        // 未選択と空文字はどちらも既定のデバイスを指す
        assert_eq!(cache_key(None), DEFAULT_DEVICE_KEY);
        assert_eq!(cache_key(Some("")), DEFAULT_DEVICE_KEY);
        assert_eq!(device_name_from_key(DEFAULT_DEVICE_KEY), None);
        assert_eq!(device_name_from_key(""), None);
    }

    // ここの `pub(super)` な関数は、子モジュールのテストからも使う共通の
    // 土台（`use crate::audio::tests::discrete_range;`）。対応設定を組み立てる
    // 補助は `capabilities` と `stream_config` の両方のテストが要るので、
    // 同じものを両方へ写さずここへ置いてある（リングバッファの `ring` も同じ理由）

    /// テスト用の対応設定。`supported_input_configs()` が返す形を模す。
    pub(super) fn config_range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            min_rate,
            max_rate,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    /// WASAPI のように離散的なレートを列挙するデバイスを模す
    pub(super) fn discrete_range(
        channels: u16,
        rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        config_range(channels, rate, rate, format)
    }

    /// `process_input` / `process_output` に渡すリングバッファ一式。入力側
    /// （`stream`）と出力側（`stream_output`）の両方のテストが使う
    pub(super) fn ring(
        capacity: usize,
    ) -> (
        std::sync::Mutex<stream::AudioProducer>,
        std::sync::Mutex<stream::AudioConsumer>,
    ) {
        use ringbuf::traits::Split;
        let (producer, consumer) = ringbuf::HeapRb::<f32>::new(capacity).split();
        (
            std::sync::Mutex::new(producer),
            std::sync::Mutex::new(consumer),
        )
    }
}
