//! `AudioCapture`。パススルーの開始と停止、観測値の取り出し。
//!
//! **`cpal::Stream` はスレッドをまたげない（`!Send`）ので、この型を持つのは
//! デバイスワーカースレッドだけ**（`docs/design/device-worker.md`）。設定の
//! 選択は `stream_config`、入力ストリームの組み立ては `stream`、出力側
//! （出力デバイス・リングバッファ・出力ストリーム）の組み立ては
//! `passthrough_output` に分けてある。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SupportedStreamConfig;
use log::{debug, info};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use super::capabilities::{device_name, AudioCapabilities};
use super::controls::AudioControls;
use super::passthrough_output::{
    build_passthrough_output, find_device_by_name, make_ring, open_output_device, PassthroughOutput,
};
use super::resample::{ResampleStatus, ResampleTelemetry};
use super::stream::{build_input_stream, StreamCounters};
use super::stream_config::{choose_passthrough_configs, resolve_ranges};
use super::tap::AudioTap;
use super::{ActiveAudio, AudioDirection, AudioError};

/// パススルーを開くときの要求。
///
/// 引数で渡していたが、対応設定のキャッシュとバッファ長を加えて 7 つに
/// なったので構造体へまとめた。`input_*` と `output_*` はどちらも同じ型で、
/// 順番を取り違えてもコンパイルが通ってしまうため、名前で区別できる形に
/// する意味もある。
pub struct PassthroughRequest<'a> {
    pub input_device_name: Option<&'a str>,
    pub output_device_name: Option<&'a str>,
    /// 設定画面で選んだサンプリングレート。`None` ならデバイスの既定に従う
    pub sample_rate: Option<u32>,
    /// 設定画面で選んだチャンネル数。`None` ならデバイスの既定に従う
    pub channels: Option<u16>,
    /// デバイスワーカーが先に取っておいた入力デバイスの対応設定。
    /// `None` のときだけ、この場で列挙する（そのぶん開くのが 300ms 遅れる）
    pub input_capabilities: Option<&'a AudioCapabilities>,
    /// 同上、出力デバイスの対応設定
    pub output_capabilities: Option<&'a AudioCapabilities>,
    /// 設定画面で選んだリングバッファの長さ（ミリ秒）。
    /// `settings::MIN_BUFFER_MS`〜`MAX_BUFFER_MS` の範囲
    pub buffer_ms: u32,
}

/// リングバッファに確保するサンプル数を決める。
///
/// 返すのは「目標水位ぶん」のサンプル数で、実際のリングバッファはこの 2 倍を
/// 確保する。入力が先行しても後れても同じだけ余裕を持たせるためで、
/// 目標水位（`target_water_level`）はその半分をフレームの境界へ揃えた値になる。
///
/// フェイクの音声（`super::fake`）も同じ長さで確保する。
///
/// **下限を 1 サンプルで止める。** `buffer_ms` は設定側で 20ms 以上に
/// 丸めてあるので通常は効かないが、0 を返すと `HeapRb::new(0)` になり
/// 入力も出力も 1 サンプルも運べなくなる。
pub(super) fn ring_buffer_samples(sample_rate: u32, channels: usize, buffer_ms: u32) -> usize {
    let samples = (sample_rate as usize)
        .saturating_mul(channels)
        .saturating_mul(buffer_ms as usize)
        / 1000;
    samples.max(1)
}

/// 目標水位（サンプル数）を決める。リングバッファの容量（`capacity`）の半分を、
/// フレーム（`channels` サンプル）の境界へ切り捨てた値。
///
/// 出力は最初にリングバッファがここまで溜まるまで取り出さずに待ち
/// （`PassthroughConverter::with_prebuffer`）、クロックドリフト補正もこの水位を
/// 保つように動く（`ResampleTelemetry::new`）。**つまりこれが音声の遅延になる。**
/// 容量は `ring_buffer_samples` の 2 倍なので、設定のバッファ長ぶんに当たる。
///
/// - 容量の半分に置くのは、入力が先行しても後れても同じだけ余裕を持たせるため。
///   20ms のような短い設定でも半分は空いているので、入力の塊（10ms 前後）が
///   溢れずに入る
/// - フレームの境界へ揃えるのは、リングバッファがフレーム単位でしか増減しない
///   ため（`docs/design/audio.md`）。44.1kHz 2ch の 25ms のように半分が奇数に
///   なる長さでも、届かない水位を目標にしない
/// - 少なくとも 1 フレーム、多くても容量まで
pub(super) fn target_water_level(capacity: usize, channels: usize) -> usize {
    let channels = channels.max(1);
    (capacity / 2 / channels * channels)
        .max(channels)
        .min(capacity)
}

pub struct AudioCapture {
    host: cpal::Host,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    /// いま開いているストリームの内容。閉じているときは `None`
    active: Option<ActiveAudio>,
    /// 出力コールバックと共有する音量・パススルー・ミュート。
    /// ストリームを開き直しても差し替えない
    controls: Arc<AudioControls>,
    /// 録画へ回す差し込み口。`controls` と同じく開き直しても差し替えない。
    /// 入力の形と開き直しの番号は、ストリームを開くたびに書く
    tap: AudioTap,
    /// クロックドリフト補正の共有状態。まだ音声を開いていなければ `None`。
    /// 入出力の形が揃っていても作る（Issue #308）。デバイスワーカーが `tick` の中で
    /// 数秒ごとに読み書きする（`app::worker_timers`）
    resample_telemetry: Option<Arc<ResampleTelemetry>>,
    /// いま開いているストリームの旗と数え手。**ストリームを開き直すたびと
    /// 閉じるたびに新しいものへ差し替える**（`StreamCounters` の説明）。
    /// 読むのはデバイスワーカースレッド（`app::worker_loop`）だけ
    counters: StreamCounters,
}

impl AudioCapture {
    /// 音量などの共有パラメータを受け取って作る。
    ///
    /// **`cpal::Stream` はスレッドをまたげない（`!Send`）ので、実際に使う
    /// スレッドで作ること。** いまはデバイスワーカースレッドが唯一の持ち主で、
    /// `AudioControls` と録画の差し込み口（`AudioTap`）だけを UI スレッドと共有する。
    pub fn new(controls: Arc<AudioControls>, tap: AudioTap) -> Self {
        let host = cpal::default_host();
        debug!("AudioCapture を作成した（ホスト: {:?}）", host.id());

        Self {
            host,
            input_stream: None,
            output_stream: None,
            active: None,
            controls,
            tap,
            resample_telemetry: None,
            counters: StreamCounters::default(),
        }
    }

    pub fn list_input_devices(&self) -> Vec<String> {
        self.try_list_devices(AudioDirection::Input)
            .unwrap_or_default()
    }

    pub fn list_output_devices(&self) -> Vec<String> {
        self.try_list_devices(AudioDirection::Output)
            .unwrap_or_default()
    }

    /// デバイス名の一覧。`list_*_devices` と違い、列挙に失敗した理由を返す。
    ///
    /// **ワーカーが列挙の結果をログへ残すため**（`app::worker_connect`）。
    /// 0 台と「列挙そのものが失敗した」を区別したい。
    pub fn try_list_devices(&self, direction: AudioDirection) -> Result<Vec<String>, AudioError> {
        let devices = match direction {
            AudioDirection::Input => self.host.input_devices(),
            AudioDirection::Output => self.host.output_devices(),
        };
        match devices {
            Ok(devices) => Ok(devices.filter_map(|d| device_name(&d)).collect()),
            Err(e) => Err(AudioError::DeviceEnumerationFailed {
                direction,
                source: e.to_string(),
            }),
        }
    }

    /// Windows 側の既定の出力デバイス名。取得できなければ `None`。
    ///
    /// 「既定のデバイス」設定が Windows 側の切り替えに追従しているかを
    /// 確認するために呼ぶ（3〜5 秒おき）。ストリームは開かないので
    /// `list_output_devices` より軽いが、COM を伴うため毎フレームは避ける。
    pub fn default_output_device_name(&self) -> Option<String> {
        device_name(&self.host.default_output_device()?)
    }

    pub fn start_passthrough(
        &mut self,
        request: &PassthroughRequest<'_>,
    ) -> Result<(), AudioError> {
        let PassthroughRequest {
            input_device_name,
            output_device_name,
            sample_rate: desired_sample_rate,
            channels: desired_channels,
            input_capabilities,
            output_capabilities,
            buffer_ms,
        } = *request;

        self.stop_capture();
        info!("音声パススルーを開始する");

        let input_device = if let Some(name) = input_device_name {
            debug!("入力デバイスを名前で探す: {}", name);
            find_device_by_name(&self.host, name, AudioDirection::Input)?
        } else {
            debug!("既定の入力デバイスを使う");
            self.host
                .default_input_device()
                .ok_or(AudioError::NoDefaultDevice(AudioDirection::Input))?
        };
        let output = open_output_device(&self.host, output_device_name, output_capabilities)?;

        let input_device_name =
            device_name(&input_device).unwrap_or_else(|| "Unknown Input".to_string());
        info!(
            "使用するデバイス - 入力: {}、出力: {}",
            input_device_name, output.name
        );

        // デバイスの既定設定。希望値が無いときの基準であり、
        // 対応設定を列挙できなかったときの退避先でもある
        let input_default =
            input_device
                .default_input_config()
                .map_err(|e| AudioError::DefaultConfigFailed {
                    direction: AudioDirection::Input,
                    source: e.to_string(),
                })?;
        // 対応設定の一覧。**先にワーカーが取ってあればそれを使う**（出力と同じ理由）
        let input_ranges = resolve_ranges(input_capabilities, AudioDirection::Input, || {
            input_device
                .supported_input_configs()
                .map(|it| it.collect())
        });

        let (input_config, output_config) = choose_passthrough_configs(
            &input_ranges,
            &output.ranges,
            input_default,
            output.default,
            desired_sample_rate,
            desired_channels,
        );
        log_configs(&input_config, &output_config);

        let ring = make_ring(
            input_config.sample_rate(),
            usize::from(input_config.channels()),
            buffer_ms,
        );
        // このストリーム専用の旗と数え手。開き直すたびに作り直す
        let counters = StreamCounters::default();

        // 録画へ入力の形と開き直しを知らせる。**入力のコールバックが動き出す前に書く。**
        // 録画スレッドはここを境に、前のストリームのサンプルと分けて扱う
        self.tap
            .begin_stream(input_config.sample_rate(), input_config.channels());

        let input_stream = build_input_stream(
            &input_device,
            &input_config,
            ring.producer.clone(),
            self.tap.clone(),
            &counters,
        )?;
        let output_stream = build_passthrough_output(
            &output,
            &output_config,
            (input_config.sample_rate(), input_config.channels()),
            &ring,
            Arc::clone(&self.controls),
            &counters,
        )?;

        // ストリーム開始。**入力が溜まるのを sleep で待たない。** 出力コールバックが
        // 目標水位まで無音を書いて待つので、ここでは続けて開始するだけでよい
        debug!("音声ストリームを開始する");
        input_stream
            .play()
            .map_err(|e| AudioError::StreamPlayFailed {
                direction: AudioDirection::Input,
                source: e.to_string(),
            })?;
        let active = ActiveAudio {
            input_device: input_device_name,
            output_device: output.name.clone(),
            input_sample_rate: input_config.sample_rate(),
            input_channels: input_config.channels(),
            output_sample_rate: output_config.sample_rate(),
            output_channels: output_config.channels(),
        };
        self.commit(Some(input_stream), output_stream, counters, active)
    }

    /// 出力ストリームを動かし、開いたものを控える。入力の種類によらず共通の後半。
    /// 出力を動かせなければ、入力のストリーム（あれば）もここで落とす。
    fn commit(
        &mut self,
        input_stream: Option<cpal::Stream>,
        output: PassthroughOutput,
        counters: StreamCounters,
        active: ActiveAudio,
    ) -> Result<(), AudioError> {
        output
            .stream
            .play()
            .map_err(|e| AudioError::StreamPlayFailed {
                direction: AudioDirection::Output,
                source: e.to_string(),
            })?;
        self.input_stream = input_stream;
        self.output_stream = Some(output.stream);
        // デバイスワーカーが `tick` の中で読み書きする対象を差し替える
        self.resample_telemetry = Some(output.telemetry);
        // 監視の対象と数え手も、いま開いたストリームのものへ差し替える
        self.counters = counters;
        // 接続状態の表示用に、実際に開いた内容を控える
        self.active = Some(active);
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

    /// クロックドリフト補正の共有状態。まだ音声を開いていなければ `None`。
    /// デバイスワーカーが `tick` の中で水位を読み、補正係数を書く
    pub fn resample_telemetry(&self) -> Option<&Arc<ResampleTelemetry>> {
        self.resample_telemetry.as_ref()
    }

    /// 「接続状態」タブへ出すための、リサンプル補正の現在値。
    pub fn resample_status(&self) -> Option<ResampleStatus> {
        self.resample_telemetry
            .as_ref()
            .map(|telemetry| ResampleStatus {
                ratio: telemetry.correction(),
                water_level: telemetry.water_level(),
                target_level: telemetry.target_level(),
            })
    }

    /// 開いているときだけ数え手の値を返す。閉じている間の 0 を「開いていて
    /// 一度も起きていない」と読み違えさせないため
    fn count_while_open(&self, counter: &AtomicU32) -> Option<u32> {
        self.active
            .as_ref()
            .map(|_| counter.load(Ordering::Relaxed))
    }

    /// 統計 OSD と「接続状態」タブへ出す、アンダーランの累計回数。
    /// 音声を開いていなければ `None`
    pub fn underrun_count(&self) -> Option<u32> {
        self.count_while_open(&self.counters.underruns)
    }

    /// 「接続状態」タブへ出す、入力がリングバッファの満杯で捨てたフレーム数の累計。
    /// `underrun_count` と同じく、開いていなければ `None`
    pub fn dropped_frame_count(&self) -> Option<u32> {
        self.count_while_open(&self.counters.dropped_frames)
    }

    /// 「接続状態」タブへ出す、入力の取りこぼしの累計。
    /// `underrun_count` と同じく、開いていなければ `None`
    pub fn xrun_count(&self) -> Option<u32> {
        self.count_while_open(&self.counters.xruns)
    }

    pub fn stop_capture(&mut self) {
        self.active = None;
        self.resample_telemetry = None;
        if let Some(s) = self.input_stream.take() {
            let _ = s.pause();
        }
        if let Some(s) = self.output_stream.take() {
            let _ = s.pause();
        }
        // 閉じたストリームのエラーコールバックが後から立てる旗を読まないよう、
        // 監視対象を新しいものへ差し替える。数え手も同じ理由で差し替える
        self.counters = StreamCounters::default();
    }

    /// 稼働中のストリームでエラーが起きていたかを返し、旗を下ろす。
    ///
    /// デバイスが消えたときの `DeviceNotAvailable` もここに現れる。
    /// 読んだ側が再接続を要求する責任を持つため、読み取りと同時に下ろす。
    /// `&self` なのは、デバイスワーカースレッドが `Mutex` の可変借用を取らずに
    /// 毎ループ確認できるようにするため
    pub fn take_stream_error(&self) -> bool {
        self.counters.error.swap(false, Ordering::Relaxed)
    }
}

/// 実際に開く入出力の設定をログへ残す。
fn log_configs(input: &SupportedStreamConfig, output: &SupportedStreamConfig) {
    info!(
        "音声の設定 - 入力: {}Hz {}ch ({:?})、出力: {}Hz {}ch ({:?})",
        input.sample_rate(),
        input.channels(),
        input.sample_format(),
        output.sample_rate(),
        output.channels(),
        output.sample_format()
    );
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_samples_matches_the_requested_length() {
        // 48kHz ステレオの 50ms は 4800 サンプル（= 48000 * 2 * 0.05）。
        // 設定項目にする前のハードコードと同じ計算
        assert_eq!(ring_buffer_samples(48_000, 2, 50), 4800);
    }

    #[test]
    fn ring_buffer_samples_scales_with_the_buffer_length() {
        // バッファ長を倍にしたらサンプル数も倍になる。遅延が長さに比例すること
        let short = ring_buffer_samples(48_000, 2, 20);
        let long = ring_buffer_samples(48_000, 2, 200);

        assert_eq!(short, 1920);
        assert_eq!(long, short * 10);
    }

    #[test]
    fn ring_buffer_samples_never_returns_zero() {
        // 設定側で 20ms 以上に丸めてあるので通常は起きないが、0 を返すと
        // HeapRb::new(0) になり 1 サンプルも運べないストリームができる
        assert_eq!(ring_buffer_samples(48_000, 2, 0), 1);
        assert_eq!(ring_buffer_samples(0, 2, 50), 1);
    }

    #[test]
    fn target_water_level_is_half_the_capacity() {
        // 48kHz ステレオの 50ms。容量 9600 の半分 = 4800（設定のバッファ長ぶん）
        let capacity = ring_buffer_samples(48_000, 2, 50) * 2;
        assert_eq!(target_water_level(capacity, 2), 4800);
        // 200ms なら 4 倍
        let capacity = ring_buffer_samples(48_000, 2, 200) * 2;
        assert_eq!(target_water_level(capacity, 2), 19_200);
    }

    #[test]
    fn target_water_level_for_the_shortest_setting_leaves_half_the_ring_free() {
        // 20ms（下限）。容量 40ms の半分の 20ms を目標にし、残りの 20ms で
        // 入力の塊（10ms 前後）を受け止める
        let capacity = ring_buffer_samples(48_000, 2, 20) * 2;
        assert_eq!(capacity, 3840);
        assert_eq!(target_water_level(capacity, 2), 1920);
    }

    #[test]
    fn target_water_level_is_aligned_to_whole_frames() {
        // 44.1kHz ステレオの 25ms は 2205 サンプルで、フレームの途中になる。
        // リングバッファはフレーム単位でしか増減しないので 2204 へ切り捨てる
        let capacity = ring_buffer_samples(44_100, 2, 25) * 2;
        assert_eq!(capacity, 4410);
        assert_eq!(target_water_level(capacity, 2), 2204);
        // 6ch でも同じ
        assert_eq!(target_water_level(100, 6), 48);
    }

    #[test]
    fn target_water_level_stays_between_one_frame_and_the_capacity() {
        // 容量がフレーム 1 つぶんしか無くても、目標は 1 フレーム（0 にすると待たない）
        assert_eq!(target_water_level(2, 2), 2);
        // 容量がフレームに満たない（設定側で起きないが）ときは容量を超えない
        assert_eq!(target_water_level(1, 2), 1);
        // 0ch は 1ch として扱い、ゼロ除算しない
        assert_eq!(target_water_level(10, 0), 5);
    }
}
