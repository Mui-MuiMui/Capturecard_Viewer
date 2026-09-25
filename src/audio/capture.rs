//! `AudioCapture`。パススルーの開始と停止、観測値の取り出し。
//!
//! **`cpal::Stream` はスレッドをまたげない（`!Send`）ので、この型を持つのは
//! デバイスワーカースレッドだけ**（`docs/design/device-worker.md`）。設定の
//! 選択は `stream_config`、ストリームの組み立ては `stream` に分けてある。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};
use log::{debug, info};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::capabilities::AudioCapabilities;
use super::controls::AudioControls;
use super::convert::{
    f32_to_i16, f32_to_i32, f32_to_u16, i16_to_f32, i32_to_f32, u16_to_f32, PassthroughConverter,
};
use super::resample::{ResampleStatus, ResampleTelemetry};
use super::stream::{build_input_stream_with, build_output_stream_with, OutputSignals};
use super::stream_config::{choose_passthrough_configs, resolve_ranges};
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
/// クロックドリフト補正の目標水位（`ResampleTelemetry::new`）もこの値になる。
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

pub struct AudioCapture {
    host: cpal::Host,
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    /// いま開いているストリームの内容。閉じているときは `None`
    active: Option<ActiveAudio>,
    /// 出力コールバックと共有する音量・パススルー・ミュート。
    /// ストリームを開き直しても差し替えない
    controls: Arc<AudioControls>,
    /// クロックドリフト補正の共有状態。変換が要らない（identity）、または
    /// まだ音声を開いていなければ `None`。デバイスワーカーが `tick` の中で
    /// 数秒ごとに読み書きする（`app::worker_loop`）
    resample_telemetry: Option<Arc<ResampleTelemetry>>,
    // 稼働中のストリームでエラーが起きたことを表す旗。
    //
    // cpal のエラーコールバックはデバイスが消えた（`DeviceNotAvailable`）
    // ときにも呼ばれるが、呼ばれるのは cpal のストリームスレッドなので
    // そこから再接続を始められない。旗を立てるだけにして、デバイスワーカー
    // スレッドが毎ループ回収する。
    //
    // **ストリームを開き直すたびに新しい `Arc` へ差し替える。** 使い回すと、
    // 閉じたストリームのエラーコールバックが後から旗を立て、開き直した直後の
    // 正常なストリームを切断と誤判定する
    //
    // 読むのはデバイスワーカースレッド（`app::worker_loop`）だけ
    stream_error: Arc<AtomicBool>,
    /// 出力コールバックがリングバッファから取り出せなかった回数（コールバック
    /// 1 回につき最大 1 回）。**バッファ長（`buffer_ms`）を詰めすぎていないかを
    /// 耳ではなく数で判断するために置いてある。**
    ///
    /// `stream_error` と同じく、ストリームを開き直すたびに新しい `Arc` へ
    /// 差し替えて 0 から数え直す。使い回すと、閉じたストリームが最後に数えた分が
    /// 開き直した直後の値として残ってしまう
    underruns: Arc<AtomicU32>,
}

impl AudioCapture {
    /// 音量などの共有パラメータを受け取って作る。
    ///
    /// **`cpal::Stream` はスレッドをまたげない（`!Send`）ので、実際に使う
    /// スレッドで作ること。** いまはデバイスワーカースレッドが唯一の持ち主で、
    /// `AudioControls` だけを UI スレッドと共有する。
    pub fn new(controls: Arc<AudioControls>) -> Self {
        let host = cpal::default_host();
        debug!("AudioCapture を作成した（ホスト: {:?}）", host.id());

        Self {
            host,
            input_stream: None,
            output_stream: None,
            active: None,
            controls,
            resample_telemetry: None,
            stream_error: Arc::new(AtomicBool::new(false)),
            underruns: Arc::new(AtomicU32::new(0)),
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

        // デバイス取得の簡素化
        let input_device = if let Some(name) = input_device_name {
            debug!("入力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, AudioDirection::Input)?
        } else {
            debug!("既定の入力デバイスを使う");
            self.host
                .default_input_device()
                .ok_or(AudioError::NoDefaultDevice(AudioDirection::Input))?
        };

        let output_device = if let Some(name) = output_device_name {
            debug!("出力デバイスを名前で探す: {}", name);
            self.find_device_by_name(name, AudioDirection::Output)?
        } else {
            debug!("既定の出力デバイスを使う");
            self.host
                .default_output_device()
                .ok_or(AudioError::NoDefaultDevice(AudioDirection::Output))?
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
        let input_default =
            input_device
                .default_input_config()
                .map_err(|e| AudioError::DefaultConfigFailed {
                    direction: AudioDirection::Input,
                    source: e.to_string(),
                })?;

        let output_default =
            output_device
                .default_output_config()
                .map_err(|e| AudioError::DefaultConfigFailed {
                    direction: AudioDirection::Output,
                    source: e.to_string(),
                })?;

        // 対応設定の一覧。**先にワーカーが取ってあればそれを使う。**
        // WASAPI の列挙は 300ms 前後かかるため、開くたびにここで走らせると、
        // ワーカーがその分だけ次のコマンドを処理できなくなる
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

        let (input_config, output_config) = choose_passthrough_configs(
            &input_ranges,
            &output_ranges,
            input_default,
            output_default,
            desired_sample_rate,
            desired_channels,
        );

        info!(
            "音声の設定 - 入力: {}Hz {}ch ({:?})、出力: {}Hz {}ch ({:?})",
            input_config.sample_rate().0,
            input_config.channels(),
            input_config.sample_format(),
            output_config.sample_rate().0,
            output_config.channels(),
            output_config.sample_format()
        );

        // リングバッファの長さは設定で選べる（`settings::AudioSettings::buffer_ms`）。
        // 小さいほど遅延が減るが、出力コールバックが間に合わずアンダーランが
        // 出やすくなる。容量は目標水位の 2 倍にして、入力が先行しても後れても
        // 同じだけ余裕を持たせる
        let sample_rate = input_config.sample_rate().0;
        let channels = input_config.channels() as usize;
        let buffer_size = ring_buffer_samples(sample_rate, channels, buffer_ms);

        let ring = HeapRb::<f32>::new(buffer_size * 2);
        let (producer, consumer) = ring.split();

        let producer = Arc::new(Mutex::new(producer));
        let consumer = Arc::new(Mutex::new(consumer));

        debug!(
            "リングバッファを作成した（{} サンプル、{} ms 相当 × 2）",
            buffer_size * 2,
            buffer_ms
        );

        // このストリーム専用のエラー旗。開き直すたびに作り直す
        let stream_error = Arc::new(AtomicBool::new(false));
        // アンダーランの数え手も同じく作り直す（開き直したら 0 から）
        let underruns = Arc::new(AtomicU32::new(0));

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
            other => {
                return Err(AudioError::UnsupportedSampleFormat {
                    direction: AudioDirection::Input,
                    format: other,
                })
            }
        }
        .map_err(|e| AudioError::StreamBuildFailed {
            direction: AudioDirection::Input,
            source: e.to_string(),
        })?;

        // 出力ストリーム
        let output_signals = OutputSignals {
            error: stream_error.clone(),
            underruns: underruns.clone(),
        };
        let controls = Arc::clone(&self.controls);
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
        // クロックドリフト補正は変換が要る組み合わせだけが対象。目標水位は
        // リングバッファのちょうど半分（`buffer_size` ぶん）に置く
        let resample_telemetry = if make_converter().is_identity() {
            debug!("入出力の形が同じなので、サンプルはそのまま流す");
            None
        } else {
            info!(
                "入出力の形が違うので変換する - レート比: {:.4}、チャンネル: {} -> {}",
                f64::from(input_config.sample_rate().0) / f64::from(output_config.sample_rate().0),
                input_config.channels(),
                output_config.channels()
            );
            Some(Arc::new(ResampleTelemetry::new(buffer_size)))
        };

        let output_stream = match output_config.sample_format() {
            SampleFormat::F32 => build_output_stream_with::<f32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                output_signals.clone(),
                make_converter().with_telemetry(resample_telemetry.clone()),
                |sample| sample,
            ),
            SampleFormat::I16 => build_output_stream_with::<i16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                output_signals.clone(),
                make_converter().with_telemetry(resample_telemetry.clone()),
                f32_to_i16,
            ),
            SampleFormat::U16 => build_output_stream_with::<u16>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                output_signals.clone(),
                make_converter().with_telemetry(resample_telemetry.clone()),
                f32_to_u16,
            ),
            SampleFormat::I32 => build_output_stream_with::<i32>(
                &output_device,
                &output_stream_config,
                consumer.clone(),
                controls,
                output_signals.clone(),
                make_converter().with_telemetry(resample_telemetry.clone()),
                f32_to_i32,
            ),
            other => {
                return Err(AudioError::UnsupportedSampleFormat {
                    direction: AudioDirection::Output,
                    format: other,
                })
            }
        }
        .map_err(|e| AudioError::StreamBuildFailed {
            direction: AudioDirection::Output,
            source: e.to_string(),
        })?;

        // ストリーム開始
        debug!("音声ストリームを開始する");
        input_stream
            .play()
            .map_err(|e| AudioError::StreamPlayFailed {
                direction: AudioDirection::Input,
                source: e.to_string(),
            })?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        output_stream
            .play()
            .map_err(|e| AudioError::StreamPlayFailed {
                direction: AudioDirection::Output,
                source: e.to_string(),
            })?;

        self.input_stream = Some(input_stream);
        self.output_stream = Some(output_stream);
        // 監視の対象を、いま開いたストリームの旗へ差し替える
        self.stream_error = stream_error;
        // デバイスワーカーが `tick` の中で読み書きする対象も差し替える
        self.resample_telemetry = resample_telemetry;
        // 数え手も、いま開いたストリームのものへ差し替える
        self.underruns = underruns;
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

    /// クロックドリフト補正の共有状態。変換が要らない（identity）、または
    /// まだ音声を開いていなければ `None`。デバイスワーカーが `tick` の中で
    /// 水位を読み、補正係数を書く
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

    /// 統計 OSD と「接続状態」タブへ出す、アンダーランの累計回数。
    ///
    /// 音声を開いていなければ `None`。閉じている間の 0 を「開いていて一度も
    /// 落ちていない」と読み違えさせないため、開いているときだけ数を返す。
    pub fn underrun_count(&self) -> Option<u32> {
        self.active
            .as_ref()
            .map(|_| self.underruns.load(Ordering::Relaxed))
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
        // 監視対象を新しいものへ差し替える
        self.stream_error = Arc::new(AtomicBool::new(false));
        // 同じ理由で、数え手も新しいものへ差し替える
        self.underruns = Arc::new(AtomicU32::new(0));
    }

    /// 稼働中のストリームでエラーが起きていたかを返し、旗を下ろす。
    ///
    /// デバイスが消えたときの `DeviceNotAvailable` もここに現れる。
    /// 読んだ側が再接続を要求する責任を持つため、読み取りと同時に下ろす。
    /// `&self` なのは、デバイスワーカースレッドが `Mutex` の可変借用を取らずに
    /// 毎ループ確認できるようにするため
    pub fn take_stream_error(&self) -> bool {
        self.stream_error.swap(false, Ordering::Relaxed)
    }

    /// 名前でデバイスを探す。向きを `bool` ではなく `AudioDirection` で受けるのは、
    /// 見つからなかったときのエラーに入力・出力のどちらかを載せるため。
    fn find_device_by_name(
        &self,
        name: &str,
        direction: AudioDirection,
    ) -> Result<Device, AudioError> {
        let iter = match direction {
            AudioDirection::Input => self.host.input_devices(),
            AudioDirection::Output => self.host.output_devices(),
        }
        .map_err(|e| AudioError::DeviceEnumerationFailed {
            direction,
            source: e.to_string(),
        })?;
        for d in iter {
            if let Ok(n) = d.name() {
                if n == name {
                    return Ok(d);
                }
            }
        }
        Err(AudioError::DeviceNotFound {
            direction,
            name: name.to_string(),
        })
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
}
