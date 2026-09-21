use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{debug, error, info, trace, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ringbuf::HeapRb;

/// リングバッファの内部表現は f32 に統一する。デバイス側のサンプル型は
/// 入力で f32 へ正規化し、出力で書き戻す。
type AudioProducer = ringbuf::Producer<f32, Arc<HeapRb<f32>>>;
type AudioConsumer = ringbuf::Consumer<f32, Arc<HeapRb<f32>>>;

/// 音量の既定値（100%）。設定を読めなかった場合もここへ倒す。
const DEFAULT_VOLUME: f32 = 1.0;

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
    /// ログと画面に出す日本語の呼び名。
    pub fn label(self) -> &'static str {
        match self {
            AudioDirection::Input => "入力",
            AudioDirection::Output => "出力",
        }
    }
}

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

/// 音声デバイスが対応する設定。
///
/// `supported_input_configs()` / `supported_output_configs()` は WASAPI で
/// 13 レート × 5 形式の `IsFormatSupported`（実測 300ms 前後）になるため、
/// UI スレッドでは呼ばない。別スレッドで一度取ってこの型で持ち回す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioCapabilities {
    /// デバイスが列挙した対応設定。`select_best_config` へそのまま渡せる
    configs: Vec<SupportedStreamConfigRange>,
    /// デバイスの既定設定（WASAPI のミックスフォーマット）
    default_sample_rate: u32,
    default_channels: u16,
}

impl AudioCapabilities {
    /// デバイスが列挙した対応設定。
    pub fn configs(&self) -> &[SupportedStreamConfigRange] {
        &self.configs
    }

    /// デバイスの既定のサンプリングレート。
    pub fn default_sample_rate(&self) -> u32 {
        self.default_sample_rate
    }

    /// デバイスの既定のチャンネル数。
    pub fn default_channels(&self) -> u16 {
        self.default_channels
    }

    /// 設定画面に出せるサンプリングレート。昇順・重複なし。
    pub fn sample_rates(&self) -> Vec<u32> {
        supported_sample_rates(&self.configs)
    }

    /// 設定画面に出せるチャンネル数。昇順・重複なし。
    pub fn channels(&self) -> Vec<u16> {
        supported_channels(&self.configs)
    }
}

/// 指定したデバイスの対応設定を取り、`AudioCapabilities` にまとめる。
///
/// **UI スレッドから直接呼ばないこと。** 列挙は WASAPI への問い合わせを
/// 繰り返すため実測 300ms 前後かかる。`CaptureCardViewer` が使い捨ての
/// スレッドへ投げ、結果をチャネルで受け取る。
///
/// `AudioCapture` のホストは使わず、この関数の中で新しく作る。`audio_capture`
/// のロックを別スレッドから握ると、その間 UI スレッドの再接続が止まるため。
pub fn query_capabilities(
    direction: AudioDirection,
    device_name: Option<&str>,
) -> Result<AudioCapabilities, String> {
    let host = cpal::default_host();

    let device = match device_name {
        Some(name) => find_device_in_host(&host, name, direction)?,
        None => match direction {
            AudioDirection::Input => host
                .default_input_device()
                .ok_or_else(|| "既定の入力デバイスがありません".to_string())?,
            AudioDirection::Output => host
                .default_output_device()
                .ok_or_else(|| "既定の出力デバイスがありません".to_string())?,
        },
    };

    let default_config = match direction {
        AudioDirection::Input => device.default_input_config(),
        AudioDirection::Output => device.default_output_config(),
    }
    .map_err(|e| format!("既定の設定を取得できません: {e}"))?;

    let configs = match direction {
        AudioDirection::Input => device.supported_input_configs().map(|it| it.collect()),
        AudioDirection::Output => device.supported_output_configs().map(|it| it.collect()),
    }
    .map_err(|e| format!("対応設定を列挙できません: {e}"))?;

    Ok(AudioCapabilities {
        configs,
        default_sample_rate: default_config.sample_rate().0,
        default_channels: default_config.channels(),
    })
}

/// ホストの一覧から名前でデバイスを探す。`AudioCapture::find_device_by_name` と
/// 同じことを、`AudioCapture` を持たない別スレッドから行うためのもの。
fn find_device_in_host(
    host: &cpal::Host,
    name: &str,
    direction: AudioDirection,
) -> Result<Device, String> {
    let devices = match direction {
        AudioDirection::Input => host.input_devices(),
        AudioDirection::Output => host.output_devices(),
    }
    .map_err(|e| format!("デバイスを列挙できません: {e}"))?;

    for device in devices {
        if device.name().ok().as_deref() == Some(name) {
            return Ok(device);
        }
    }
    Err(format!("デバイス '{name}' が見つかりません"))
}

/// 設定画面に出すサンプリングレートの当たり値。
///
/// 連続した範囲（min < max）を返すホストでは「対応している値」を列挙できない
/// ため、この一覧のうち範囲に収まるものを候補にする。WASAPI のように離散値を
/// 列挙するホストでは、列挙された値がそのまま候補になるのでここは効かない。
const BASELINE_SAMPLE_RATES: [u32; 7] = [8000, 16000, 22050, 32000, 44100, 48000, 96000];

/// 能力を取得できなかったときに出す既定の選択肢。
///
/// 従来の固定一覧と同じ。デバイスが対応しない値も選べるが、`select_best_config`
/// が最も近い値へ寄せるので開けなくなることはない。
pub const FALLBACK_SAMPLE_RATES: [u32; 7] = BASELINE_SAMPLE_RATES;
/// 同上、チャンネル数の既定の選択肢。
pub const FALLBACK_CHANNELS: [u16; 2] = [1, 2];

/// 対応設定の一覧から、選択肢に出せるサンプリングレートを作る。昇順・重複なし。
///
/// 扱えないサンプル形式（`sample_format_priority` が `None` を返すもの）しか
/// 持たない設定は、選んでもストリームを組み立てられないので数に入れない。
fn supported_sample_rates(configs: &[SupportedStreamConfigRange]) -> Vec<u32> {
    let mut rates = Vec::new();
    for range in configs {
        if sample_format_priority(range.sample_format()).is_none() {
            continue;
        }
        let min = range.min_sample_rate().0;
        let max = range.max_sample_rate().0;
        // 壊れた列挙は無視する（select_best_config と同じ扱い）
        if min > max {
            continue;
        }
        if min == max {
            rates.push(min);
        } else {
            rates.extend(
                BASELINE_SAMPLE_RATES
                    .iter()
                    .copied()
                    .filter(|&rate| (min..=max).contains(&rate)),
            );
        }
    }
    rates.sort_unstable();
    rates.dedup();
    rates
}

/// 対応設定の一覧から、選択肢に出せるチャンネル数を作る。昇順・重複なし。
fn supported_channels(configs: &[SupportedStreamConfigRange]) -> Vec<u16> {
    let mut channels: Vec<u16> = configs
        .iter()
        .filter(|range| sample_format_priority(range.sample_format()).is_some())
        .filter(|range| range.min_sample_rate() <= range.max_sample_rate())
        .map(|range| range.channels())
        .collect();
    channels.sort_unstable();
    channels.dedup();
    channels
}

/// 選択肢の出どころ。UI が添える説明を出し分けるために持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceSource {
    /// 入出力の両方が対応する値だけを出している
    Common,
    /// 共通の値が無いので両方の和を出している。開くときに入出力で別の値になる
    Disjoint,
    /// 片方のデバイスの能力しか取れていないので、そちらだけで作った
    OneSided,
    /// どちらの能力も取れていないので固定の既定一覧
    Fallback,
}

/// 設定画面に出す選択肢と、その出どころ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChoices<T> {
    pub values: Vec<T>,
    pub source: ChoiceSource,
}

/// 入出力の能力から選択肢を組み立てる共通処理。
///
/// - 両方取れていて共通部分があれば共通部分（`Common`）
/// - 両方取れていて共通部分が無ければ和（`Disjoint`）
/// - 片方だけ取れていればその一覧（`OneSided`）
/// - どちらも取れていなければ `fallback`（`Fallback`）
///
/// 共通部分が空のときに片側だけを出さないのは、どちらのデバイスに合わせたいかを
/// ユーザーが決められるようにするため。
fn build_choices<T: Copy + Ord>(
    input: Option<Vec<T>>,
    output: Option<Vec<T>>,
    fallback: &[T],
) -> AudioChoices<T> {
    match (input, output) {
        (Some(input), Some(output)) => {
            let common = intersect_sorted(&input, &output);
            if !common.is_empty() {
                return AudioChoices {
                    values: common,
                    source: ChoiceSource::Common,
                };
            }
            let mut union = input;
            union.extend(output);
            union.sort_unstable();
            union.dedup();
            AudioChoices {
                values: union,
                source: ChoiceSource::Disjoint,
            }
        }
        (Some(values), None) | (None, Some(values)) => AudioChoices {
            values,
            source: ChoiceSource::OneSided,
        },
        (None, None) => AudioChoices {
            values: fallback.to_vec(),
            source: ChoiceSource::Fallback,
        },
    }
}

/// 片方だけ能力が取れているときに、空の一覧を「取れていない」と同じに扱う。
///
/// 対応設定を列挙できても、扱えるサンプル形式が 1 つも無ければ候補は空になる。
/// 空のまま `build_choices` へ渡すと共通部分も和も空になり、選択肢が消える。
fn non_empty<T>(values: Vec<T>) -> Option<Vec<T>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// 設定画面に出すサンプリングレートの選択肢。
pub fn selectable_sample_rates(
    input: Option<&AudioCapabilities>,
    output: Option<&AudioCapabilities>,
) -> AudioChoices<u32> {
    build_choices(
        input.and_then(|caps| non_empty(caps.sample_rates())),
        output.and_then(|caps| non_empty(caps.sample_rates())),
        &FALLBACK_SAMPLE_RATES,
    )
}

/// 設定画面に出すチャンネル数の選択肢。
pub fn selectable_channels(
    input: Option<&AudioCapabilities>,
    output: Option<&AudioCapabilities>,
) -> AudioChoices<u16> {
    build_choices(
        input.and_then(|caps| non_empty(caps.channels())),
        output.and_then(|caps| non_empty(caps.channels())),
        &FALLBACK_CHANNELS,
    )
}

/// 昇順の 2 つの一覧の共通部分。
fn intersect_sorted<T: Copy + Ord>(a: &[T], b: &[T]) -> Vec<T> {
    a.iter()
        .copied()
        .filter(|value| b.contains(value))
        .collect()
}

/// 候補の中から希望値に最も近いものを返す。候補が空なら `None`。
///
/// 同じ差の候補が 2 つあるときは先に来たほう（一覧は昇順なので小さいほう）を選ぶ。
/// `select_best_config` が同点で列挙順の先頭を採るのと揃えてある。
pub fn nearest_sample_rate(values: &[u32], desired: u32) -> Option<u32> {
    values.iter().copied().min_by_key(|v| v.abs_diff(desired))
}

/// チャンネル数版の `nearest_sample_rate`。
pub fn nearest_channels(values: &[u16], desired: u16) -> Option<u16> {
    values.iter().copied().min_by_key(|v| v.abs_diff(desired))
}

/// パススルーを開くときの要求。
///
/// 引数で渡していたが、対応設定のキャッシュを加えて 6 つになったので構造体へ
/// まとめた。`input_*` と `output_*` はどちらも同じ型で、順番を取り違えても
/// コンパイルが通ってしまうため、名前で区別できる形にする意味もある。
pub struct PassthroughRequest<'a> {
    pub input_device_name: Option<&'a str>,
    pub output_device_name: Option<&'a str>,
    /// 設定画面で選んだサンプリングレート。`None` ならデバイスの既定に従う
    pub sample_rate: Option<u32>,
    /// 設定画面で選んだチャンネル数。`None` ならデバイスの既定に従う
    pub channels: Option<u16>,
    /// 別スレッドで先に取っておいた入力デバイスの対応設定。
    /// `None` のときだけ、この場（UI スレッド）で列挙する
    pub input_capabilities: Option<&'a AudioCapabilities>,
    /// 同上、出力デバイスの対応設定
    pub output_capabilities: Option<&'a AudioCapabilities>,
}

pub struct AudioCapture {
    host: cpal::Host,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    /// いま開いているストリームの内容。閉じているときは `None`
    active: Option<ActiveAudio>,
    /// 出力に掛ける倍率。`0.0`〜`2.0`。
    ///
    /// **出力コールバック（リアルタイムスレッド）が 1 回ごとに読むので、
    /// ロックを使わない。** f32 の値を直接持てる Atomic 型が無いため、
    /// `to_bits` / `from_bits` でビット表現のまま出し入れする。
    /// `Mutex` だと、UI スレッドが音量を書き換えている最中に
    /// コールバックが待たされ、バッファを埋め損ねて音が途切れうる。
    volume: Arc<AtomicU32>,
    audio_passthrough_enabled: Arc<AtomicBool>,
    /// ミュート中か。
    ///
    /// **音量とは独立に持つ。** 音量 0% で代用すると、ミュートを解除したときに
    /// 戻すべき値が残らない。出力コールバックから読むので、パススルーの旗と
    /// 同じく `AtomicBool` にしてロックを避ける。
    muted: Arc<AtomicBool>,
    // 稼働中のストリームでエラーが起きたことを表す旗。
    //
    // cpal のエラーコールバックはデバイスが消えた（`DeviceNotAvailable`）
    // ときにも呼ばれるが、呼ばれるのは cpal のストリームスレッドなので
    // そこから再接続を始められない。旗を立てるだけにして、UI スレッドが
    // 毎フレーム回収する。
    //
    // **ストリームを開き直すたびに新しい `Arc` へ差し替える。** 使い回すと、
    // 閉じたストリームのエラーコールバックが後から旗を立て、開き直した直後の
    // 正常なストリームを切断と誤判定する
    stream_error: Arc<AtomicBool>,
}

/// 共有している音量へ書き込む。
fn store_volume(cell: &AtomicU32, volume: f32) {
    cell.store(volume.to_bits(), Ordering::Relaxed);
}

/// 共有している音量を読み出す。
fn load_volume(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

/// パーセント指定の音量を、出力サンプルに掛ける倍率へ直す。
///
/// 設定ファイルは手で編集できるため、範囲外の値や `nan` も入りうる。
/// NaN をそのまま掛けると出力が全て NaN になり、デバイスによっては
/// 耳障りな雑音になるので、有限でない値は既定値へ倒す。
fn normalize_volume(volume_percent: f32) -> f32 {
    if !volume_percent.is_finite() {
        return DEFAULT_VOLUME;
    }
    (volume_percent / 100.0).clamp(0.0, 2.0)
}

impl AudioCapture {
    pub fn new() -> Self {
        let host = cpal::default_host();
        debug!("AudioCapture を作成した（ホスト: {:?}）", host.id());

        Self {
            host,
            input_stream: None,
            output_stream: None,
            active: None,
            volume: Arc::new(AtomicU32::new(DEFAULT_VOLUME.to_bits())),
            // 既定では音声パススルーを有効にする（音が出る状態で起動する）
            audio_passthrough_enabled: Arc::new(AtomicBool::new(true)),
            // 既定はミュート解除。設定から読んだ値は apply_settings が入れ直す
            muted: Arc::new(AtomicBool::new(false)),
            stream_error: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn list_input_devices(&self) -> Vec<String> {
        match self.host.input_devices() {
            Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn list_output_devices(&self) -> Vec<String> {
        match self.host.output_devices() {
            Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Windows 側の既定の入力デバイス名。取得できなければ `None`。
    ///
    /// 「既定のデバイス」設定が Windows 側の切り替えに追従しているかを
    /// 確認するために呼ぶ（3〜5 秒おき）。ストリームは開かないので
    /// `list_input_devices` より軽いが、COM を伴うため毎フレームは避ける。
    pub fn default_input_device_name(&self) -> Option<String> {
        self.host.default_input_device()?.name().ok()
    }

    /// Windows 側の既定の出力デバイス名。取得できなければ `None`。
    /// 意図は `default_input_device_name` と同じ。
    pub fn default_output_device_name(&self) -> Option<String> {
        self.host.default_output_device()?.name().ok()
    }

    pub fn start_passthrough(&mut self, request: &PassthroughRequest<'_>) -> Result<(), String> {
        let PassthroughRequest {
            input_device_name,
            output_device_name,
            sample_rate: desired_sample_rate,
            channels: desired_channels,
            input_capabilities,
            output_capabilities,
        } = *request;

        self.stop_capture();
        info!("音声パススルーを開始する");

        // デバイス取得の簡素化
        let input_device = if let Some(name) = input_device_name {
            debug!("入力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, true)?
        } else {
            debug!("既定の入力デバイスを使う");
            self.host
                .default_input_device()
                .ok_or_else(|| "No default input device".to_string())?
        };

        let output_device = if let Some(name) = output_device_name {
            debug!("出力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, false)?
        } else {
            debug!("既定の出力デバイスを使う");
            self.host
                .default_output_device()
                .ok_or_else(|| "No default output device".to_string())?
        };

        // デバイス名をログ出力
        let input_device_name = input_device
            .name()
            .unwrap_or_else(|_| "Unknown Input".to_string());
        let output_device_name = output_device
            .name()
            .unwrap_or_else(|_| "Unknown Output".to_string());
        info!(
            "使用するデバイス - 入力: {}、出力: {}",
            input_device_name, output_device_name
        );

        // デバイスの既定設定。希望値が無いときの基準であり、
        // 対応設定を列挙できなかったときの退避先でもある
        let input_default = input_device
            .default_input_config()
            .map_err(|e| format!("Failed to get input config: {}", e))?;

        let output_default = output_device
            .default_output_config()
            .map_err(|e| format!("Failed to get output config: {}", e))?;

        // 対応設定の一覧。**先に別スレッドで取ってあればそれを使う。**
        // WASAPI の列挙は 300ms 前後かかるため、ここ（UI スレッド）で毎回
        // 走らせるとデバイスの切り替えのたびにウィンドウが固まる
        let input_ranges = resolve_ranges(input_capabilities, AudioDirection::Input, || {
            input_device
                .supported_input_configs()
                .map(|it| it.collect())
        });
        let output_ranges = resolve_ranges(output_capabilities, AudioDirection::Output, || {
            output_device
                .supported_output_configs()
                .map(|it| it.collect())
        });

        // 設定画面で選んだサンプルレート・チャンネル数を、デバイスが対応する
        // 組み合わせの中で最も近いものへ寄せる。列挙できない、または選べる設定が
        // 無いデバイスでは既定設定のまま開く（従来の挙動）
        //
        // **まず入出力で同じ設定に揃えられないかを見る。** 揃っていれば
        // リングバッファのサンプルをそのまま流せる。揃わない組み合わせでは
        // 再生速度とピッチがずれる
        let aligned = select_aligned_configs(
            &input_ranges,
            &output_ranges,
            desired_sample_rate.unwrap_or_else(|| input_default.sample_rate().0),
            desired_channels.unwrap_or_else(|| input_default.channels()),
        );

        let (input_config, output_config) = match aligned {
            Some(pair) => pair,
            None => {
                let input_config = select_best_config(
                    &input_ranges,
                    desired_sample_rate.unwrap_or_else(|| input_default.sample_rate().0),
                    desired_channels.unwrap_or_else(|| input_default.channels()),
                )
                .unwrap_or(input_default);
                let output_config = select_best_config(
                    &output_ranges,
                    desired_sample_rate.unwrap_or_else(|| output_default.sample_rate().0),
                    desired_channels.unwrap_or_else(|| output_default.channels()),
                )
                .unwrap_or(output_default);
                if input_config.sample_rate() != output_config.sample_rate()
                    || input_config.channels() != output_config.channels()
                {
                    warn!(
                        "入出力で共通の設定が無いため別々の設定で開く（線形補間とミックスで変換する）- 入力: {}Hz {}ch、出力: {}Hz {}ch",
                        input_config.sample_rate().0,
                        input_config.channels(),
                        output_config.sample_rate().0,
                        output_config.channels()
                    );
                }
                (input_config, output_config)
            }
        };

        info!(
            "音声の設定 - 入力: {}Hz {}ch ({:?})、出力: {}Hz {}ch ({:?})",
            input_config.sample_rate().0,
            input_config.channels(),
            input_config.sample_format(),
            output_config.sample_rate().0,
            output_config.channels(),
            output_config.sample_format()
        );

        // メモリリーク修正: リングバッファサイズを制限
        let sample_rate = input_config.sample_rate().0;
        let channels = input_config.channels() as usize;
        let buffer_size = (sample_rate as usize * channels * 50) / 1000; // 50msバッファに削減

        let ring = HeapRb::<f32>::new(buffer_size * 2); // サイズを削減
        let (producer, consumer) = ring.split();

        let producer = Arc::new(Mutex::new(producer));
        let consumer = Arc::new(Mutex::new(consumer));

        debug!("リングバッファを作成した（{} サンプル）", buffer_size * 2);

        // このストリーム専用のエラー旗。開き直すたびに作り直す
        let stream_error = Arc::new(AtomicBool::new(false));

        // 入力ストリーム。デバイスのサンプル型ごとに正規化の仕方が違うので明示的に分ける
        let input_stream_config = input_config.config();
        let input_stream = match input_config.sample_format() {
            SampleFormat::F32 => build_input_stream_with::<f32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                |sample| sample,
            ),
            SampleFormat::I16 => build_input_stream_with::<i16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                i16_to_f32,
            ),
            SampleFormat::U16 => build_input_stream_with::<u16>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                u16_to_f32,
            ),
            SampleFormat::I32 => build_input_stream_with::<i32>(
                &input_device,
                &input_stream_config,
                producer.clone(),
                stream_error.clone(),
                i32_to_f32,
            ),
            other => return Err(unsupported_sample_format_error("入力", other)),
        }
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

        // 出力ストリーム
        let controls = OutputControls {
            volume: self.volume.clone(),
            passthrough_enabled: self.audio_passthrough_enabled.clone(),
            muted: self.muted.clone(),
        };
        let output_stream_config = output_config.config();

        // 入出力の形が違う場合の変換器。**ここで作る（ストリームの構築時）。**
        // 補間に使うバッファを先に確保しておかないと、出力コールバックの中で
        // アロケーションが起きる
        let make_converter = || {
            PassthroughConverter::new(
                input_config.sample_rate().0,
                input_config.channels(),
                output_config.sample_rate().0,
                output_config.channels(),
            )
        };
        if make_converter().is_identity() {
            debug!("入出力の形が同じなので、サンプルはそのまま流す");
        } else {
            info!(
                "入出力の形が違うので変換する - レート比: {:.4}、チャンネル: {} -> {}",
                f64::from(input_config.sample_rate().0) / f64::from(output_config.sample_rate().0),
                input_config.channels(),
                output_config.channels()
            );
        }

        let output_stream = match output_config.sample_format() {
            SampleFormat::F32 => build_output_stream_with::<f32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                stream_error.clone(),
                make_converter(),
                |sample| sample,
            ),
            SampleFormat::I16 => build_output_stream_with::<i16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                stream_error.clone(),
                make_converter(),
                f32_to_i16,
            ),
            SampleFormat::U16 => build_output_stream_with::<u16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                stream_error.clone(),
                make_converter(),
                f32_to_u16,
            ),
            SampleFormat::I32 => build_output_stream_with::<i32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                stream_error.clone(),
                make_converter(),
                f32_to_i32,
            ),
            other => return Err(unsupported_sample_format_error("出力", other)),
        }
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

        // ストリーム開始
        debug!("音声ストリームを開始する");
        input_stream
            .play()
            .map_err(|e| format!("Failed to start input stream: {}", e))?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        output_stream
            .play()
            .map_err(|e| format!("Failed to start output stream: {}", e))?;

        self.input_stream = Some(input_stream);
        self.output_stream = Some(output_stream);
        // 監視の対象を、いま開いたストリームの旗へ差し替える
        self.stream_error = stream_error;
        // 接続状態の表示用に、実際に開いた内容を控える
        self.active = Some(ActiveAudio {
            input_device: input_device_name,
            output_device: output_device_name,
            input_sample_rate: input_config.sample_rate().0,
            input_channels: input_config.channels(),
            output_sample_rate: output_config.sample_rate().0,
            output_channels: output_config.channels(),
        });

        info!("音声パススルーを開始した");
        Ok(())
    }

    /// いま開いているストリームの内容。開いていなければ `None`。
    ///
    /// 設定ダイアログを開いている間だけ呼ばれる。小さな構造体の複製だけで、
    /// デバイスの列挙もストリームへの問い合わせも行わない。
    pub fn active(&self) -> Option<ActiveAudio> {
        self.active.clone()
    }

    pub fn stop_capture(&mut self) {
        self.active = None;
        if let Some(s) = self.input_stream.take() {
            let _ = s.pause();
        }
        if let Some(s) = self.output_stream.take() {
            let _ = s.pause();
        }
        // 閉じたストリームのエラーコールバックが後から立てる旗を読まないよう、
        // 監視対象を新しいものへ差し替える
        self.stream_error = Arc::new(AtomicBool::new(false));
    }

    /// 稼働中のストリームでエラーが起きていたかを返し、旗を下ろす。
    ///
    /// デバイスが消えたときの `DeviceNotAvailable` もここに現れる。
    /// 読んだ側が再接続を要求する責任を持つため、読み取りと同時に下ろす。
    /// `&self` なのは、UI スレッドが `Mutex` の可変借用を取らずに
    /// 毎フレーム確認できるようにするため
    pub fn take_stream_error(&self) -> bool {
        self.stream_error.swap(false, Ordering::Relaxed)
    }

    pub fn set_volume(&mut self, volume_percent: f32) {
        // apply_settings から 2 秒ごとに呼ばれるため trace に落とす
        trace!("音量を設定する: {}%", volume_percent);
        // 出力コールバック（リアルタイムスレッド）から読むため、ロックを取らない
        store_volume(&self.volume, normalize_volume(volume_percent));
    }

    pub fn set_audio_passthrough_enabled(&mut self, enabled: bool) {
        // apply_settings から 2 秒ごとに呼ばれる。変化の有無を判別できないので trace に落とす
        trace!("音声パススルーの有効/無効を設定する: {}", enabled);
        // 出力コールバック（リアルタイムスレッド）から読むため、ロックを取らない
        self.audio_passthrough_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// ミュートの入切を設定する。
    ///
    /// **音量には触らない。** ミュート中も `volume` は元の値のまま残り、
    /// 解除するとその音量で鳴り始める。
    pub fn set_muted(&mut self, muted: bool) {
        // apply_settings から 2 秒ごとに呼ばれるため trace に落とす
        trace!("ミュートを設定する: {}", muted);
        // 出力コールバック（リアルタイムスレッド）から読むため、ロックを取らない
        self.muted.store(muted, Ordering::Relaxed);
    }

    fn find_device_by_name(&self, name: &str, input: bool) -> Result<Device, String> {
        let iter = if input {
            self.host.input_devices()
        } else {
            self.host.output_devices()
        }
        .map_err(|e| format!("enumerate devices: {e}"))?;
        for d in iter {
            if let Ok(n) = d.name() {
                if n == name {
                    return Ok(d);
                }
            }
        }
        Err(format!("Device '{name}' not found"))
    }
}

/// 対応しているサンプルフォーマットの優先度。小さいほど優先する。未対応なら `None`。
///
/// `build_input_stream_with` / `build_output_stream_with` で扱える型と一致させること。
/// ここに無いフォーマットを選ぶと、設定としては選べてもストリームを組み立てられない。
fn sample_format_priority(format: SampleFormat) -> Option<u8> {
    match format {
        // リングバッファと同じ表現なので変換が要らない
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::I32 => Some(2),
        SampleFormat::U16 => Some(3),
        _ => None,
    }
}

/// デバイスが対応する設定から、希望するサンプルレート・チャンネル数に最も近いものを選ぶ。
///
/// 選ぶ順は チャンネル数の差 → サンプルレートの差 → サンプルフォーマットの優先度。
/// チャンネル数を先に見るのは、モノラルとステレオの違いが聴感に直結するのに対し、
/// サンプルレートは必ず「対応している中で最も近い値」へ寄せられるため。
/// すべて同点なら列挙順の先頭を選ぶ（デバイスが優先する設定が先に来る）。
///
/// WASAPI はデバイスのミックスフォーマットのチャンネル数しか列挙しないため、
/// モノラルを希望してもステレオしか選べないことがある。UI の選択肢をデバイスの
/// 能力から生成する作業は別タスク。
///
/// 選べる設定が 1 つも無ければ `None`。呼び出し側はデバイスの既定設定へ落とす。
fn select_best_config(
    configs: &[SupportedStreamConfigRange],
    desired_sample_rate: u32,
    desired_channels: u16,
) -> Option<SupportedStreamConfig> {
    configs
        .iter()
        .filter_map(|range| {
            let priority = sample_format_priority(range.sample_format())?;
            let min_rate = range.min_sample_rate().0;
            let max_rate = range.max_sample_rate().0;
            // 壊れた列挙で clamp が panic するのを避ける
            if min_rate > max_rate {
                return None;
            }

            let rate = desired_sample_rate.clamp(min_rate, max_rate);
            let key = (
                range.channels().abs_diff(desired_channels),
                rate.abs_diff(desired_sample_rate),
                priority,
            );
            Some((key, range.try_with_sample_rate(SampleRate(rate))?))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, config)| config)
}

/// 入力と出力の両方が対応する設定を選ぶ。
///
/// 揃えられればリングバッファのサンプルをそのまま流せる。WASAPI は共有モードで
/// ミックスフォーマットしか通さないことが多く、入力 48kHz・出力 44.1kHz のように
/// 揃えられない組み合わせは珍しくない。その場合は `None` を返し、呼び出し側が
/// それぞれの最寄りを選ぶ。
///
/// 共通の候補を出してから `select_best_config` へ同じ値を渡し、**両方が本当に
/// その値で開けるかを最後に確かめる。** レートとチャンネル数を別々に共通化して
/// いるため、「片方はそのレート、もう片方はそのチャンネル数」しか持たない
/// 組み合わせが候補に残りうる。
fn select_aligned_configs(
    input: &[SupportedStreamConfigRange],
    output: &[SupportedStreamConfigRange],
    desired_sample_rate: u32,
    desired_channels: u16,
) -> Option<(SupportedStreamConfig, SupportedStreamConfig)> {
    let rates = intersect_sorted(
        &supported_sample_rates(input),
        &supported_sample_rates(output),
    );
    let channels = intersect_sorted(&supported_channels(input), &supported_channels(output));

    let rate = nearest_sample_rate(&rates, desired_sample_rate)?;
    let channels = nearest_channels(&channels, desired_channels)?;

    let input_config = select_best_config(input, rate, channels)?;
    let output_config = select_best_config(output, rate, channels)?;

    if input_config.sample_rate() != output_config.sample_rate()
        || input_config.channels() != output_config.channels()
    {
        return None;
    }

    info!(
        "入出力を同じ設定に揃えた: {}Hz {}ch",
        input_config.sample_rate().0,
        input_config.channels()
    );
    Some((input_config, output_config))
}

/// 対応設定の一覧を用意する。
///
/// 別スレッドで取ったキャッシュがあればそれを使い、無いときだけその場で列挙する。
/// 列挙に失敗したら空を返す。空なら `select_best_config` が `None` を返し、
/// 呼び出し側がデバイスの既定設定へ落ちる（従来の挙動）。
fn resolve_ranges<E: std::fmt::Display>(
    cached: Option<&AudioCapabilities>,
    direction: AudioDirection,
    enumerate: impl FnOnce() -> Result<Vec<SupportedStreamConfigRange>, E>,
) -> Vec<SupportedStreamConfigRange> {
    if let Some(caps) = cached {
        debug!(
            "{}デバイスの対応設定は取得済みのものを使う（{} 件）",
            direction.label(),
            caps.configs().len()
        );
        return caps.configs().to_vec();
    }

    let started = std::time::Instant::now();
    match enumerate() {
        Ok(ranges) => {
            debug!(
                "{}デバイスの対応設定をこの場で列挙した（{} 件、{} ms）",
                direction.label(),
                ranges.len(),
                started.elapsed().as_millis()
            );
            ranges
        }
        Err(e) => {
            warn!(
                "{}デバイスの対応設定を列挙できないので既定の設定で開く: {}",
                direction.label(),
                e
            );
            Vec::new()
        }
    }
}

/// 未対応のサンプルフォーマットに当たったときのエラー文言を組み立てる。
///
/// ストリーム構築エラーをそのまま上げると「なぜ開けなかったのか」が分からないので、
/// 何が来て何に対応しているのかを明示する。`direction` は「入力」か「出力」。
fn unsupported_sample_format_error(direction: &str, format: SampleFormat) -> String {
    format!(
        "{direction}デバイスのサンプルフォーマット {format} に対応していません（対応: f32 / i16 / u16 / i32）"
    )
}

/// 入力ストリームを組み立てる。
///
/// `to_f32` でデバイスのサンプル型をリングバッファの表現（f32）へ正規化する。
fn build_input_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    producer: Arc<Mutex<AudioProducer>>,
    stream_error: Arc<AtomicBool>,
    to_f32: impl Fn(T) -> f32 + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if let Ok(mut prod) = producer.try_lock() {
                for &sample in data {
                    let _ = prod.push(to_f32(sample));
                }
            }
        },
        move |e| {
            error!("入力ストリームのエラー: {}", e);
            // 呼ばれるのは cpal のストリームスレッド。ここで開き直すと
            // ストリーム自身を drop することになるので、旗を立てるだけにする
            stream_error.store(true, Ordering::Relaxed);
        },
        None,
    )
}

/// 出力コールバックが 1 回ごとに読む共有の値。
///
/// 個別の引数で渡していたが数が増えたのでまとめた。どれも `Arc` の複製を
/// コールバックへ移すだけなので、束ねても寿命の扱いは変わらない。
struct OutputControls {
    volume: Arc<AtomicU32>,
    passthrough_enabled: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
}

/// 出力ストリームを組み立てる。
///
/// `to_sample` はリングバッファの f32 をデバイスのサンプル型へ戻す。
/// `converter` は入出力でレートやチャンネル数が違う場合の変換を持つ。
fn build_output_stream_with<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    consumer: Arc<Mutex<AudioConsumer>>,
    controls: OutputControls,
    stream_error: Arc<AtomicBool>,
    mut converter: PassthroughConverter,
    to_sample: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let volume = load_volume(&controls.volume);
            let audible = output_is_audible(
                controls.passthrough_enabled.load(Ordering::Relaxed),
                controls.muted.load(Ordering::Relaxed),
            );
            if let Ok(mut cons) = consumer.try_lock() {
                let mut pop = || cons.pop();
                render_output_samples(
                    data,
                    volume,
                    audible,
                    || converter.next_sample(&mut pop),
                    &to_sample,
                );
            } else {
                // 無音を表す値は型ごとに違う（u16 は 0 ではなく 32768）ので変換関数に通す
                data.fill(to_sample(0.0));
            }
        },
        move |e| {
            error!("出力ストリームのエラー: {}", e);
            // 入力側と同じ理由で、旗を立てるだけにする
            stream_error.store(true, Ordering::Relaxed);
        },
        None,
    )
}

/// 整数サンプルの振幅の基準。f32 の -1.0 が型の最小値、+1.0 が最大値 + 1 に対応する。
/// 2 のべき乗なので f32 の除算・乗算で誤差が出ない。
const I16_SCALE: f32 = 32_768.0;
const I32_SCALE: f32 = 2_147_483_648.0;
/// u16 の原点。無音は 0 ではなく 32768。
const U16_ORIGIN: f32 = 32_768.0;

/// i16 のサンプルを f32（-1.0..1.0）へ正規化する。
fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / I16_SCALE
}

/// f32 のサンプルを i16 へ変換する。
///
/// 音量 200% では 1.0 を超える値が来る。Rust の float → int キャストは飽和するので、
/// 折り返して最大音量が最小音量に化けることはない。
fn f32_to_i16(sample: f32) -> i16 {
    (sample * I16_SCALE) as i16
}

/// u16 のサンプルを f32（-1.0..1.0）へ正規化する。
///
/// u16 は 32768 が原点なので、そのまま符号付きとして読むと最大振幅の直流になる。
fn u16_to_f32(sample: u16) -> f32 {
    (sample as f32 - U16_ORIGIN) / U16_ORIGIN
}

/// f32 のサンプルを u16 へ変換する。
fn f32_to_u16(sample: f32) -> u16 {
    (sample * U16_ORIGIN + U16_ORIGIN) as u16
}

/// i32 のサンプルを f32（-1.0..1.0）へ正規化する。
fn i32_to_f32(sample: i32) -> f32 {
    sample as f32 / I32_SCALE
}

/// f32 のサンプルを i32 へ変換する。
fn f32_to_i32(sample: f32) -> i32 {
    (sample * I32_SCALE) as i32
}

/// 入力ストリームのサンプルを、出力ストリームの形へ変換しながら 1 つずつ渡す。
///
/// リングバッファには入力デバイスの形（入力のレート、入力のチャンネル数で
/// インターリーブ）のまま積まれている。入出力でレートやチャンネル数が違うと、
/// そのまま出しては再生速度とピッチがずれ、チャンネルの割り当ても崩れる。
///
/// - サンプリングレートの違いは**線形補間**で吸収する。外部クレートを足さずに
///   済み、遅延も 1 入力フレームで足りる。折り返し雑音は完全には消えないが、
///   素通しの遅延を優先する用途（`docs/ARCHITECTURE.md` の「設計の前提」）に
///   合わせてこちらを採る
/// - チャンネル数の違いはアップ／ダウンミックスで吸収する。モノラル → 多ch は
///   同じ値を全チャンネルへ、多ch → モノラルは平均。それ以外は先頭から
///   対応させ、余った出力チャンネルは無音にする
///
/// **リアルタイムスレッド（出力コールバック）で動くので、アロケーションも
/// ロックも行わない。** バッファはストリームを組み立てるときに確保する。
///
/// 長時間再生で効いてくるクロックドリフト（入出力のハードウェアクロックが
/// 厳密には一致しないこと）は扱わない。バッファ水位に応じた微調整が要る。
pub struct PassthroughConverter {
    /// 入出力が同じ形なので変換が要らない。リングバッファの値をそのまま出す
    identity: bool,
    in_channels: usize,
    out_channels: usize,
    /// 出力 1 フレームぶんで進める入力フレーム数（入力レート / 出力レート）。
    /// 丸め誤差が位相のずれとして溜まるので f64 で持つ
    step: f64,
    /// 補間の左端と右端になる入力フレーム。長さは `in_channels`
    prev: Vec<f32>,
    next: Vec<f32>,
    /// `prev` と `next` の間の位置。0.0 以上 1.0 未満
    position: f64,
    /// `prev` / `next` を読み込み済みか
    primed: bool,
    /// 組み立て済みの出力フレーム。長さは `out_channels`
    frame: Vec<f32>,
    /// `frame` の中で次に返すチャンネル
    channel: usize,
    /// 入力が尽きたまま出力フレームの途中にいる。
    ///
    /// **フレーム境界まで `None` を返し続ける。** 尽きた直後に読み直すと、
    /// 出力チャンネルの途中から新しいフレームの 0 番を書くことになり、
    /// 左右が入れ替わる
    starved: bool,
}

impl PassthroughConverter {
    pub fn new(
        input_sample_rate: u32,
        input_channels: u16,
        output_sample_rate: u32,
        output_channels: u16,
    ) -> Self {
        // 0 が来ると剰余とゼロ除算で落ちる。cpal は 0 を返さないが、
        // ここで倒れると出力コールバックの中で panic する
        let in_channels = (input_channels as usize).max(1);
        let out_channels = (output_channels as usize).max(1);
        let step = if output_sample_rate == 0 {
            1.0
        } else {
            f64::from(input_sample_rate) / f64::from(output_sample_rate)
        };

        // 揃っている場合は補間も再配置も要らない。**この経路を残すのは、
        // 揃えて開けた場合（`select_aligned_configs`）に従来どおり
        // リングバッファの値をそのまま出すため。**
        let identity = in_channels == out_channels && input_sample_rate == output_sample_rate;

        Self {
            identity,
            in_channels,
            out_channels,
            step,
            prev: vec![0.0; in_channels],
            next: vec![0.0; in_channels],
            position: 0.0,
            primed: false,
            frame: vec![0.0; out_channels],
            channel: 0,
            starved: false,
        }
    }

    /// 変換が要らない組み合わせか。ログとテストのための問い合わせ。
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// 出力サンプルを 1 つ取り出す。入力が足りなければ `None`。
    ///
    /// `pop` はリングバッファから入力サンプルを 1 つ取り出す。
    pub fn next_sample(&mut self, pop: &mut impl FnMut() -> Option<f32>) -> Option<f32> {
        if self.identity {
            return pop();
        }

        // 出力フレームの先頭でだけ入力を読む。途中で読むとチャンネルがずれる
        if self.channel == 0 {
            self.starved = !self.fill_frame(pop);
            if self.starved {
                // 途中まで読んだフレームは捨て、次のフレーム境界で組み立て直す
                self.primed = false;
                self.position = 0.0;
            }
        }

        let sample = if self.starved {
            None
        } else {
            Some(self.frame[self.channel])
        };

        self.channel += 1;
        if self.channel >= self.out_channels {
            self.channel = 0;
        }
        sample
    }

    /// 次の出力フレームを組み立てる。入力が足りなければ `false`。
    fn fill_frame(&mut self, pop: &mut impl FnMut() -> Option<f32>) -> bool {
        if !self.primed {
            if !read_frame(&mut self.prev, pop) || !read_frame(&mut self.next, pop) {
                return false;
            }
            self.primed = true;
            self.position = 0.0;
        }

        // 位置が `next` を追い越しているあいだ、入力フレームを進める。
        // ダウンサンプル（step > 1）では 1 回の出力で複数フレーム進む
        while self.position >= 1.0 {
            std::mem::swap(&mut self.prev, &mut self.next);
            if !read_frame(&mut self.next, pop) {
                return false;
            }
            self.position -= 1.0;
        }

        mix_frame(
            &self.prev,
            &self.next,
            self.position as f32,
            self.in_channels,
            &mut self.frame,
        );
        self.position += self.step;
        true
    }
}

/// 入力フレームを 1 つ読み込む。途中で尽きたら `false`。
///
/// 尽きた場合に読んだぶんは捨てる。呼び出し側が組み立て直すので、
/// 中途半端なフレームを持ち越さない。
fn read_frame(dst: &mut [f32], pop: &mut impl FnMut() -> Option<f32>) -> bool {
    for slot in dst.iter_mut() {
        match pop() {
            Some(sample) => *slot = sample,
            None => return false,
        }
    }
    true
}

/// 2 つの入力フレームを線形補間し、出力チャンネル数へミックスする。
///
/// `position` は 0.0 で `prev`、1.0 で `next` に一致する位置。
///
/// - 入力がモノラルなら、同じ値を全ての出力チャンネルへ配る
/// - 出力がモノラルなら、入力の全チャンネルの平均を書く
/// - それ以外は先頭から対応させ、余った出力チャンネルは無音にする
///   （5.1ch の出力へ 2ch を流すときにサラウンドへ前方の音が漏れないように）
fn mix_frame(prev: &[f32], next: &[f32], position: f32, in_channels: usize, out: &mut [f32]) {
    let at = |channel: usize| -> f32 {
        let a = prev[channel];
        a + (next[channel] - a) * position
    };

    if in_channels == 1 {
        let sample = at(0);
        out.fill(sample);
        return;
    }

    if out.len() == 1 {
        let sum: f32 = (0..in_channels).map(at).sum();
        out[0] = sum / in_channels as f32;
        return;
    }

    for (channel, slot) in out.iter_mut().enumerate() {
        *slot = if channel < in_channels {
            at(channel)
        } else {
            0.0
        };
    }
}

/// 出力に音を書き込んでよいか。
///
/// パススルーの無効とミュートは理由も操作経路も別だが、出力コールバックから見れば
/// どちらも「無音を書く」に落ちる。**片方だけを見る書き方にしないため**に、
/// 判定をここへ 1 つにまとめてある。
fn output_is_audible(passthrough_enabled: bool, muted: bool) -> bool {
    passthrough_enabled && !muted
}

/// 出力コールバック 1 回分のサンプルを書き込む。
///
/// `audible` が false のときは無音を書き込む。ストリームは止めない。
///
/// `next_sample` はリングバッファから 1 サンプル取り出す。取り出せなければ `None`。
/// `to_sample` は音量を掛けた f32 を出力ストリームのサンプル型へ変換する。
fn render_output_samples<T>(
    data: &mut [T],
    volume: f32,
    audible: bool,
    mut next_sample: impl FnMut() -> Option<f32>,
    to_sample: impl Fn(f32) -> T,
) {
    // 無音を書くときもリングバッファは同じ数だけ消費する。
    // 消費を止めるとバッファが溢れ、再度鳴らしたときに古い音から再生されてしまう。
    for slot in data.iter_mut() {
        let sample = next_sample().unwrap_or(0.0);
        let value = if audible { sample * volume } else { 0.0 };
        *slot = to_sample(value);
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用のサンプル供給源。取り出した回数も数える。
    struct SampleSource {
        samples: std::collections::VecDeque<f32>,
        pop_count: usize,
    }

    impl SampleSource {
        fn new(samples: &[f32]) -> Self {
            Self {
                samples: samples.iter().copied().collect(),
                pop_count: 0,
            }
        }

        fn pop(&mut self) -> Option<f32> {
            self.pop_count += 1;
            self.samples.pop_front()
        }
    }

    #[test]
    fn normalize_volume_percent_maps_to_multiplier() {
        assert_eq!(normalize_volume(0.0), 0.0);
        assert_eq!(normalize_volume(100.0), 1.0);
        assert_eq!(normalize_volume(200.0), 2.0);
    }

    #[test]
    fn normalize_volume_out_of_range_is_clamped() {
        // 設定ファイルを手で編集すれば UI の上限を超えた値も入る
        assert_eq!(normalize_volume(-50.0), 0.0);
        assert_eq!(normalize_volume(1000.0), 2.0);
    }

    #[test]
    fn normalize_volume_non_finite_falls_back_to_default() {
        // NaN を掛けると出力が全て NaN になるので既定値へ倒す
        assert_eq!(normalize_volume(f32::NAN), DEFAULT_VOLUME);
        assert_eq!(normalize_volume(f32::INFINITY), DEFAULT_VOLUME);
    }

    #[test]
    fn store_volume_and_load_volume_round_trip() {
        // 出力コールバックは f32 をビット表現のまま受け取る。
        // 0.0 が別の値に化けると、音量 0% でも音が出てしまう
        let cell = AtomicU32::new(DEFAULT_VOLUME.to_bits());
        store_volume(&cell, 0.0);
        assert_eq!(load_volume(&cell), 0.0);
        store_volume(&cell, 1.75);
        assert_eq!(load_volume(&cell), 1.75);
    }

    #[test]
    fn output_is_audible_only_when_passthrough_on_and_not_muted() {
        assert!(output_is_audible(true, false));
        // ミュート中はパススルーが有効でも鳴らさない
        assert!(!output_is_audible(true, true));
        // パススルーが無効なら、ミュートを解除しても鳴らさない
        assert!(!output_is_audible(false, false));
        assert!(!output_is_audible(false, true));
    }

    #[test]
    fn render_output_samples_passthrough_enabled_applies_volume() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 0.5, true, || source.pop(), |value| value);

        assert_eq!(data, [0.5, 0.25, -0.125]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_writes_silence() {
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_passthrough_disabled_still_consumes_source() {
        // 消費を止めるとリングバッファが溢れ、再有効化した瞬間に古い音が出るため、
        // 無効時もバッファからは同じ数だけ取り出す
        let mut source = SampleSource::new(&[1.0, 0.5, -0.25, 0.75]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, false, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 3);
        assert_eq!(source.samples.len(), 1);
    }

    #[test]
    fn render_output_samples_source_underrun_fills_remainder_with_silence() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data = [9.0f32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_empty_source_writes_silence() {
        let mut source = SampleSource::new(&[]);
        let mut data = [9.0f32; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(data, [0.0, 0.0]);
    }

    #[test]
    fn render_output_samples_converts_to_i16() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i16);

        assert_eq!(data, [32767, -32768]);
    }

    #[test]
    fn render_output_samples_i16_passthrough_disabled_writes_zero() {
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9i16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_i16);

        assert_eq!(data, [0, 0]);
    }

    #[test]
    fn render_output_samples_converts_to_u16() {
        // u16 の無音は 0 ではなく 32768
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9u16; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_u16);

        assert_eq!(data, [65535, 0, 32768]);
    }

    #[test]
    fn render_output_samples_u16_passthrough_disabled_writes_midpoint() {
        // 無効時に 0 を書くと u16 では最大振幅の直流になるため、原点を書く
        let mut source = SampleSource::new(&[1.0, -1.0]);
        let mut data = [9u16; 2];

        render_output_samples(&mut data, 1.0, false, || source.pop(), f32_to_u16);

        assert_eq!(data, [32768, 32768]);
    }

    #[test]
    fn render_output_samples_converts_to_i32() {
        let mut source = SampleSource::new(&[1.0, -1.0, 0.0]);
        let mut data = [9i32; 3];

        render_output_samples(&mut data, 1.0, true, || source.pop(), f32_to_i32);

        assert_eq!(data, [2147483647, -2147483648, 0]);
    }

    #[test]
    fn render_output_samples_empty_output_buffer_does_not_consume_source() {
        let mut source = SampleSource::new(&[1.0]);
        let mut data: [f32; 0] = [];

        render_output_samples(&mut data, 1.0, true, || source.pop(), |value| value);

        assert_eq!(source.pop_count, 0);
    }

    /// 変換器へ入力サンプルを流し、出せるだけ出力サンプルを取り出す。
    ///
    /// **出力フレーム単位で呼ぶ。** 実際の出力コールバックはバッファ長ぶん
    /// 呼び続け、入力が尽きた区間は無音で埋める。1 サンプルでも `None` が
    /// 出たら止める作りにすると、フレーム境界まで `None` を返す仕様
    /// （`starved`）を確かめられない。
    fn drain_converter(converter: &mut PassthroughConverter, input: &[f32]) -> Vec<f32> {
        let mut source = input
            .iter()
            .copied()
            .collect::<std::collections::VecDeque<_>>();
        let mut pop = || source.pop_front();
        let mut out = Vec::new();
        let frame_len = converter.out_channels;
        loop {
            let before = out.len();
            for _ in 0..frame_len {
                if let Some(sample) = converter.next_sample(&mut pop) {
                    out.push(sample);
                }
            }
            // 1 フレームまるごと出せなければ入力が尽きている
            if out.len() == before || out.len() > 10_000 {
                break;
            }
        }
        out
    }

    #[test]
    fn passthrough_converter_same_format_passes_samples_through() {
        // 揃えて開けた場合は補間も再配置も挟まない
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 2);

        assert!(converter.is_identity());
        assert_eq!(
            drain_converter(&mut converter, &[0.25, -0.5, 1.0]),
            vec![0.25, -0.5, 1.0]
        );
    }

    #[test]
    fn passthrough_converter_is_not_identity_when_only_the_rate_differs() {
        assert!(!PassthroughConverter::new(48000, 2, 44100, 2).is_identity());
    }

    #[test]
    fn passthrough_converter_is_not_identity_when_only_the_channels_differ() {
        assert!(!PassthroughConverter::new(48000, 1, 48000, 2).is_identity());
    }

    #[test]
    fn passthrough_converter_downsampling_keeps_every_other_frame() {
        // 48kHz -> 24kHz。1 出力フレームにつき入力を 2 フレーム進む
        let mut converter = PassthroughConverter::new(48000, 1, 24000, 1);

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);

        assert_eq!(out, vec![0.0, 2.0, 4.0]);
    }

    #[test]
    fn passthrough_converter_upsampling_interpolates_midpoints() {
        // 24kHz -> 48kHz。中間の位置は線形補間で埋める
        let mut converter = PassthroughConverter::new(24000, 1, 48000, 1);

        let out = drain_converter(&mut converter, &[0.0, 1.0, 2.0]);

        assert_eq!(out, vec![0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn passthrough_converter_mono_input_is_copied_to_every_output_channel() {
        let mut converter = PassthroughConverter::new(48000, 1, 48000, 2);

        let out = drain_converter(&mut converter, &[0.25, 0.5, 0.75]);

        // 最後の 1 フレームは補間の右端として読まれるだけで、出力には出ない
        assert_eq!(out, vec![0.25, 0.25, 0.5, 0.5]);
    }

    #[test]
    fn passthrough_converter_multi_channel_input_is_averaged_into_mono() {
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 1);

        // フレームは [1.0, 0.0] / [0.0, 0.0] / [0.5, 0.5]
        let out = drain_converter(&mut converter, &[1.0, 0.0, 0.0, 0.0, 0.5, 0.5]);

        assert_eq!(out, vec![0.5, 0.0]);
    }

    #[test]
    fn passthrough_converter_extra_output_channels_are_silent() {
        // 2ch を 4ch の出力へ流す。前方 2 本だけに配り、残りは無音にする
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 4);

        let out = drain_converter(&mut converter, &[1.0, -1.0, 0.5, -0.5]);

        assert_eq!(out, vec![1.0, -1.0, 0.0, 0.0]);
    }

    #[test]
    fn passthrough_converter_empty_input_returns_none() {
        let mut converter = PassthroughConverter::new(48000, 2, 44100, 2);

        assert!(drain_converter(&mut converter, &[]).is_empty());
    }

    #[test]
    fn passthrough_converter_underrun_does_not_swap_channels() {
        // 入力が尽きたあと、出力フレームの途中から新しいフレームの
        // 0 番を書き始めると左右が入れ替わる。尽きたらフレーム境界まで
        // 何も出さないこと
        let mut converter = PassthroughConverter::new(48000, 2, 48000, 4);

        // 1 フレームだけでは補間の右端が無く、何も出せない
        assert!(drain_converter(&mut converter, &[1.0, -1.0]).is_empty());

        // 改めて 2 フレーム渡すと、先頭のチャンネルから組み立て直す
        let out = drain_converter(&mut converter, &[0.5, -0.5, 0.25, -0.25]);

        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    #[test]
    fn passthrough_converter_output_length_follows_the_rate_ratio() {
        // 再生速度がずれないこと。48kHz の入力 4800 フレームは
        // 44.1kHz の出力でおよそ 4410 フレームになる
        let mut converter = PassthroughConverter::new(48000, 1, 44100, 1);
        let input: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();

        let out = drain_converter(&mut converter, &input);

        // 補間の右端を先読みするぶん数フレーム少なくなる
        assert!(
            (4405..=4410).contains(&out.len()),
            "出力フレーム数が想定から外れている: {}",
            out.len()
        );
    }

    #[test]
    fn passthrough_converter_zero_channels_do_not_panic() {
        // cpal は 0 を返さないが、出力コールバックの中で落ちると復旧できない
        let mut converter = PassthroughConverter::new(48000, 0, 48000, 0);

        assert!(drain_converter(&mut converter, &[1.0, 2.0, 3.0]).len() <= 3);
    }

    #[test]
    fn passthrough_converter_zero_output_rate_does_not_divide_by_zero() {
        let mut converter = PassthroughConverter::new(48000, 1, 0, 1);

        // step が 1.0 に倒れるので、入力をそのまま並べたものになる
        assert_eq!(
            drain_converter(&mut converter, &[1.0, 2.0, 3.0]),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn unsupported_sample_format_error_names_the_format() {
        // 「何が未対応だったか」が分からないと原因にたどり着けない
        let message = unsupported_sample_format_error("入力", SampleFormat::U32);

        assert!(message.contains("入力"), "{message}");
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn i16_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(-32768), -1.0);
        assert_eq!(i16_to_f32(32767), 0.999_969_5);
        assert_eq!(i16_to_f32(16384), 0.5);
    }

    #[test]
    fn f32_to_i16_boundaries_saturate() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(-1.0), -32768);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(0.5), 16384);
    }

    #[test]
    fn f32_to_i16_out_of_range_clamps_instead_of_wrapping() {
        // 音量 200% で 1.0 のサンプルが 2.0 になることがある。
        // 折り返すと最大音量が最小音量に化けるため、飽和させる
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32768);
    }

    #[test]
    fn u16_to_f32_midpoint_is_silence() {
        // u16 は 32768 が原点。ここを 0.0 にできないと無音が直流になる
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(u16_to_f32(0), -1.0);
        assert_eq!(u16_to_f32(65535), 0.999_969_5);
        assert_eq!(u16_to_f32(49152), 0.5);
    }

    #[test]
    fn f32_to_u16_boundaries_map_to_full_range() {
        assert_eq!(f32_to_u16(0.0), 32768);
        assert_eq!(f32_to_u16(-1.0), 0);
        assert_eq!(f32_to_u16(1.0), 65535);
        assert_eq!(f32_to_u16(0.5), 49152);
    }

    #[test]
    fn f32_to_u16_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_u16(2.0), 65535);
        assert_eq!(f32_to_u16(-2.0), 0);
    }

    #[test]
    fn i32_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i32_to_f32(0), 0.0);
        assert_eq!(i32_to_f32(-2147483648), -1.0);
        assert_eq!(i32_to_f32(1073741824), 0.5);
    }

    #[test]
    fn f32_to_i32_boundaries_saturate() {
        assert_eq!(f32_to_i32(0.0), 0);
        assert_eq!(f32_to_i32(-1.0), -2147483648);
        assert_eq!(f32_to_i32(1.0), 2147483647);
        assert_eq!(f32_to_i32(0.5), 1073741824);
    }

    #[test]
    fn f32_to_i32_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_i32(2.0), 2147483647);
        assert_eq!(f32_to_i32(-2.0), -2147483648);
    }

    #[test]
    fn u16_silence_read_as_i16_becomes_full_scale_dc() {
        // 修正前は F32 以外をすべて i16 として扱っていた。
        // u16 の無音 (32768) を i16 として読むと最大振幅の直流になり、
        // ストリームが構築できた場合でも正しい音にならない
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(i16_to_f32(32768u16 as i16), -1.0);
    }

    #[test]
    fn f32_round_trip_preserves_sample_within_quantization_error() {
        // リングバッファの表現は f32。入力側で正規化した値が出力側で元の量子化値へ戻る
        for raw in [-32768i16, -1, 0, 1, 32767] {
            assert_eq!(f32_to_i16(i16_to_f32(raw)), raw);
        }
        for raw in [0u16, 1, 32768, 65535] {
            assert_eq!(f32_to_u16(u16_to_f32(raw)), raw);
        }
    }

    /// テスト用の対応設定。`supported_input_configs()` が返す形を模す。
    fn config_range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(min_rate),
            SampleRate(max_rate),
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    /// WASAPI のように離散的なレートを列挙するデバイスを模す
    fn discrete_range(
        channels: u16,
        rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        config_range(channels, rate, rate, format)
    }

    #[test]
    fn select_best_config_exact_match_is_chosen() {
        let configs = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 96000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(48000));
        assert_eq!(selected.channels(), 2);
    }

    #[test]
    fn select_best_config_unsupported_rate_falls_back_to_nearest() {
        // 44100 は列挙されていない。48000 (差 3900) が 32000 (差 12100) より近い
        let configs = [
            discrete_range(2, 32000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 44100, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(48000));
    }

    #[test]
    fn select_best_config_equidistant_rates_pick_the_first() {
        // 40000 は 32000 と 48000 の中間。列挙順の先頭を選ぶ
        let configs = [
            discrete_range(2, 32000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 40000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(32000));
    }

    #[test]
    fn select_best_config_prefers_matching_channels_over_matching_rate() {
        // WASAPI はミックスフォーマットのチャンネル数しか列挙しないが、
        // 複数出る環境ではチャンネル数を先に合わせる
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(1, 44100, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 1).expect("選べるはず");

        assert_eq!(selected.channels(), 1);
        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }

    #[test]
    fn select_best_config_unavailable_channels_falls_back_to_nearest() {
        // モノラルを希望してもステレオしか無ければステレオを選ぶ
        let configs = [discrete_range(2, 48000, SampleFormat::F32)];

        let selected = select_best_config(&configs, 48000, 1).expect("選べるはず");

        assert_eq!(selected.channels(), 2);
    }

    #[test]
    fn select_best_config_skips_unsupported_sample_formats() {
        // U8 と I64 は変換関数が無く、選んでもストリームを組み立てられない。
        // 希望レートに一致していても選ばない
        let configs = [
            discrete_range(2, 48000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::I64),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_format(), SampleFormat::I16);
        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }

    #[test]
    fn select_best_config_prefers_f32_when_rate_and_channels_tie() {
        // f32 はリングバッファと同じ表現なので変換が要らない
        let configs = [
            discrete_range(2, 48000, SampleFormat::I16),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn select_best_config_clamps_into_a_continuous_range() {
        // 連続した範囲を返すホストでは、範囲内へ丸める
        let configs = [config_range(2, 8000, 96000, SampleFormat::F32)];

        let inside = select_best_config(&configs, 44100, 2).expect("選べるはず");
        assert_eq!(inside.sample_rate(), SampleRate(44100));

        let above = select_best_config(&configs, 192000, 2).expect("選べるはず");
        assert_eq!(above.sample_rate(), SampleRate(96000));

        let below = select_best_config(&configs, 5512, 2).expect("選べるはず");
        assert_eq!(below.sample_rate(), SampleRate(8000));
    }

    #[test]
    fn select_best_config_empty_list_returns_none() {
        assert!(select_best_config(&[], 48000, 2).is_none());
    }

    #[test]
    fn select_best_config_all_unsupported_formats_returns_none() {
        let configs = [
            discrete_range(2, 48000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::F64),
        ];

        assert!(select_best_config(&configs, 48000, 2).is_none());
    }

    /// テスト用の `AudioCapabilities`。既定値は WASAPI のミックスフォーマットを模す。
    fn capabilities(configs: &[SupportedStreamConfigRange]) -> AudioCapabilities {
        AudioCapabilities {
            configs: configs.to_vec(),
            default_sample_rate: 48000,
            default_channels: 2,
        }
    }

    #[test]
    fn supported_sample_rates_discrete_ranges_are_listed_sorted_and_deduped() {
        // WASAPI は min == max の離散値を並べる。同じレートが形式違いで複数出る
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
            discrete_range(2, 48000, SampleFormat::I16),
        ];

        assert_eq!(supported_sample_rates(&configs), vec![44100, 48000]);
    }

    #[test]
    fn supported_sample_rates_continuous_range_uses_baseline_values() {
        // 連続した範囲では「対応している値」を列挙できないので、当たり値のうち
        // 範囲に収まるものを候補にする
        let configs = [config_range(2, 16000, 48000, SampleFormat::F32)];

        assert_eq!(
            supported_sample_rates(&configs),
            vec![16000, 22050, 32000, 44100, 48000]
        );
    }

    #[test]
    fn supported_sample_rates_skips_unsupported_formats() {
        // 変換関数が無い形式は選んでもストリームを組み立てられない
        let configs = [
            discrete_range(2, 96000, SampleFormat::U8),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert_eq!(supported_sample_rates(&configs), vec![48000]);
    }

    #[test]
    fn supported_sample_rates_skips_reversed_range() {
        // min > max の壊れた列挙。select_best_config と同じく無視する
        let configs = [config_range(2, 96000, 8000, SampleFormat::F32)];

        assert!(supported_sample_rates(&configs).is_empty());
    }

    #[test]
    fn supported_channels_are_sorted_and_deduped() {
        let configs = [
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(1, 48000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        assert_eq!(supported_channels(&configs), vec![1, 2]);
    }

    #[test]
    fn supported_channels_skips_unsupported_formats() {
        let configs = [
            discrete_range(6, 48000, SampleFormat::F64),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert_eq!(supported_channels(&configs), vec![2]);
    }

    #[test]
    fn intersect_sorted_keeps_only_common_values() {
        assert_eq!(intersect_sorted(&[1, 2, 3], &[2, 3, 4]), vec![2, 3]);
        assert!(intersect_sorted(&[1, 2], &[3, 4]).is_empty());
        assert!(intersect_sorted::<u32>(&[], &[1]).is_empty());
    }

    #[test]
    fn nearest_sample_rate_picks_the_closest_value() {
        assert_eq!(nearest_sample_rate(&[32000, 48000], 44100), Some(48000));
        assert_eq!(nearest_sample_rate(&[44100, 48000], 44100), Some(44100));
    }

    #[test]
    fn nearest_sample_rate_tie_picks_the_smaller_value() {
        // 一覧は昇順なので、同じ差なら先に来た小さいほうが残る。
        // select_best_config が同点で列挙順の先頭を採るのと揃えてある
        assert_eq!(nearest_sample_rate(&[32000, 48000], 40000), Some(32000));
    }

    #[test]
    fn nearest_sample_rate_empty_list_returns_none() {
        assert_eq!(nearest_sample_rate(&[], 48000), None);
    }

    #[test]
    fn nearest_channels_picks_the_closest_value() {
        assert_eq!(nearest_channels(&[2], 1), Some(2));
        assert_eq!(nearest_channels(&[1, 2], 1), Some(1));
        assert_eq!(nearest_channels(&[], 2), None);
    }

    #[test]
    fn selectable_sample_rates_uses_the_common_values() {
        let input = capabilities(&[
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ]);
        let output = capabilities(&[
            discrete_range(2, 48000, SampleFormat::F32),
            discrete_range(2, 96000, SampleFormat::F32),
        ]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![48000]);
        assert_eq!(choices.source, ChoiceSource::Common);
    }

    #[test]
    fn selectable_sample_rates_without_common_values_falls_back_to_the_union() {
        // 入力 48kHz・出力 44.1kHz は WASAPI では珍しくない。
        // 片側だけを出すと、どちらに合わせたいかをユーザーが選べなくなる
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);
        let output = capabilities(&[discrete_range(2, 44100, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![44100, 48000]);
        assert_eq!(choices.source, ChoiceSource::Disjoint);
    }

    #[test]
    fn selectable_sample_rates_with_one_side_missing_uses_that_side() {
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), None);

        assert_eq!(choices.values, vec![48000]);
        assert_eq!(choices.source, ChoiceSource::OneSided);
    }

    #[test]
    fn selectable_sample_rates_with_no_capabilities_uses_the_fallback_list() {
        let choices = selectable_sample_rates(None, None);

        assert_eq!(choices.values, FALLBACK_SAMPLE_RATES.to_vec());
        assert_eq!(choices.source, ChoiceSource::Fallback);
    }

    #[test]
    fn selectable_sample_rates_treats_an_empty_capability_as_missing() {
        // 列挙はできたが扱える形式が 1 つも無いデバイス。空のまま共通部分を
        // 取ると選択肢が消えるので、取得できていないのと同じに扱う
        let input = capabilities(&[discrete_range(2, 48000, SampleFormat::U8)]);
        let output = capabilities(&[discrete_range(2, 44100, SampleFormat::F32)]);

        let choices = selectable_sample_rates(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![44100]);
        assert_eq!(choices.source, ChoiceSource::OneSided);
    }

    #[test]
    fn selectable_channels_uses_the_common_values() {
        let input = capabilities(&[
            discrete_range(1, 48000, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ]);
        let output = capabilities(&[discrete_range(2, 48000, SampleFormat::F32)]);

        let choices = selectable_channels(Some(&input), Some(&output));

        assert_eq!(choices.values, vec![2]);
        assert_eq!(choices.source, ChoiceSource::Common);
    }

    #[test]
    fn selectable_channels_with_no_capabilities_uses_the_fallback_list() {
        let choices = selectable_channels(None, None);

        assert_eq!(choices.values, FALLBACK_CHANNELS.to_vec());
        assert_eq!(choices.source, ChoiceSource::Fallback);
    }

    #[test]
    fn select_aligned_configs_matches_both_sides() {
        let input = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];
        let output = [discrete_range(2, 48000, SampleFormat::I16)];

        let (in_config, out_config) =
            select_aligned_configs(&input, &output, 44100, 2).expect("揃えられるはず");

        // 44100 は出力が対応しないので、共通の 48000 へ寄る
        assert_eq!(in_config.sample_rate(), SampleRate(48000));
        assert_eq!(out_config.sample_rate(), SampleRate(48000));
        assert_eq!(in_config.channels(), 2);
        assert_eq!(out_config.channels(), 2);
    }

    #[test]
    fn select_aligned_configs_without_a_common_rate_returns_none() {
        let input = [discrete_range(2, 48000, SampleFormat::F32)];
        let output = [discrete_range(2, 44100, SampleFormat::F32)];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_without_a_common_channel_count_returns_none() {
        let input = [discrete_range(1, 48000, SampleFormat::F32)];
        let output = [discrete_range(2, 48000, SampleFormat::F32)];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_rejects_a_cross_matched_combination() {
        // レートは 48000 が、チャンネル数は 2 が共通しているが、
        // 「48000Hz かつ 2ch」を両方が開けるわけではない
        let input = [
            discrete_range(2, 44100, SampleFormat::F32),
            discrete_range(1, 48000, SampleFormat::F32),
        ];
        let output = [
            discrete_range(1, 44100, SampleFormat::F32),
            discrete_range(2, 48000, SampleFormat::F32),
        ];

        assert!(select_aligned_configs(&input, &output, 48000, 2).is_none());
    }

    #[test]
    fn select_aligned_configs_empty_lists_return_none() {
        assert!(select_aligned_configs(&[], &[], 48000, 2).is_none());
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

    #[test]
    fn select_best_config_reversed_range_is_skipped() {
        // min > max の壊れた列挙で panic しないこと
        let configs = [
            config_range(2, 96000, 8000, SampleFormat::F32),
            discrete_range(2, 44100, SampleFormat::I16),
        ];

        let selected = select_best_config(&configs, 48000, 2).expect("選べるはず");

        assert_eq!(selected.sample_rate(), SampleRate(44100));
    }
}
