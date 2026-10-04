//! デバイスワーカーがデバイスに触るときの入口。
//!
//! **trait の境界は「ワーカーがデバイスへ触る場所」に置いてある。**
//! 具体的には開く・閉じる・列挙する・能力を問い合わせる・観測値を読む、の
//! 5 つだけで、`super::worker_loop` / `super::worker_connect` /
//! `super::worker_audio_connect` / `super::worker_timers` /
//! `super::worker_audio_timers` が呼ぶ操作がそのまま並ぶ。本番の実装は
//! `system` にあり、映像は `SystemVideo`（`crate::video::VideoCapture` と
//! `crate::video::DirectShowCapture` を束ねる）、音声は
//! `crate::audio::AudioCapture` をそのまま trait に包んでいる。
//!
//! **フレームコールバックと cpal のコールバックの経路には挟まない。**
//! 映像フレームは `VideoFrames`、音量とミュートは `AudioControls` の共有
//! ハンドル越しに流れ続ける。あのコールバックはロックもアロケーションも
//! しない決まりなので、動的ディスパッチを足す場所ではない
//! （`docs/design/video-pipeline.md` / `docs/design/audio.md`）。
//!
//! 実装は 3 つある。
//!
//! | 実装 | 置き場所 | 使われるとき |
//! |---|---|---|
//! | 本番（`SystemBackends`） | `system` | 通常の起動 |
//! | フェイク（`fake::FakeBackends`） | `fake`。中身は `crate::video::FakeVideoCapture` / `crate::audio::FakeAudioCapture` | 環境変数 `CAPTURECARD_VIEWER_FAKE_DEVICES` を指定して起動したとき |
//! | モック | このファイルの `mock`（`#[cfg(test)]`） | ワーカーの単体テスト |
//!
//! 本番とフェイクのどちらを使うかは `backends_from_env` が決め、呼ぶのは
//! `DeviceWorker::spawn` の 1 か所だけ。

use crate::audio::{
    ActiveAudio, AudioCapabilities, AudioControls, AudioDirection, AudioError, AudioPinPresence,
    AudioTap, PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::repaint::RepaintWaker;
use crate::settings::VideoBackendSetting;
use crate::video::{
    ActiveVideo, DeviceCapabilities, SharedColorConversion, VideoError, VideoFrames, VideoLinkState,
};
use log::warn;
use std::sync::Arc;

mod fake;
mod system;
mod system_route;

use fake::{FakeBackends, FAKE_DEVICES_ENV, FAKE_SCENARIO_ENV};
pub(super) use system::SystemBackends;

/// 映像デバイスの開閉・列挙・観測。
///
/// **開いた結果を別のハンドル型では返さない。** ストリームを持つのは実装
/// 自身で、`stop_capture` / `link_state` / `active` がその持ち物に対する窓口に
/// なる。分割後は開いた分が `video/capture.rs` 1 ファイルに収まっていて
/// 切り出せるが、あえてしていない。フェイク（#142）の作りやすさは変わらず、
/// ワーカーの観測値の読み出しがすべて `Option<ハンドル>` 越しになるほうが
/// 重いため。理由と見直す条件は `docs/design/device-worker.md` の
/// 「開いたストリームは実装自身が持つ」。
pub(super) trait VideoBackend {
    /// 映像デバイスの一覧。`(名前, 説明)`。失敗しても空の一覧を返す
    fn list_devices(&self) -> Vec<(String, String)>;

    /// デバイスが対応する形式の一覧。`None` なら先頭のデバイス。
    ///
    /// `backend` は `start_capture` と同じく設定の「映像の開き方」。開くときと
    /// 同じ経路へ問い合わせる（#249）。経路が 1 つしかない実装は見ない
    fn capabilities(
        &self,
        device_name: Option<&str>,
        backend: VideoBackendSetting,
    ) -> Result<DeviceCapabilities, VideoError>;

    /// ストリームを開く。既に開いていれば閉じてから開き直す。
    fn start_capture(&mut self, request: &CaptureRequest<'_>) -> Result<(), VideoError>;

    /// ストリームを閉じる。開いていなければ何もしない
    fn stop_capture(&mut self);

    /// 開けているかと、最後のフレームからの経過時間。切断の判定に使う
    fn link_state(&self) -> VideoLinkState;

    /// 実際に開いたストリームの内容。開いていなければ `None`
    fn active(&self) -> Option<ActiveVideo>;

    /// 列挙の時点で調べた、各デバイスの音声ピンの有無（#409）。`(名前, 有無)` で、
    /// 名前は「(DirectShow)」の印の有無を問わない（`monitor_audio_pin::presence_of`
    /// が印を外して突き合わせる）。設定ダイアログで映像デバイスの音声を選べるかを
    /// 開く前に決めるためだけに使う。既定は空（モック。載っていない名前は「不明」）
    fn audio_pin_presence(&mut self) -> Vec<(String, AudioPinPresence)> {
        Vec::new()
    }

    /// 経路ごとの列挙結果。**ログと「Windows 側にも見えていない」の判定専用**
    /// （`super::worker_connect::log_device_enumeration`）。
    ///
    /// `list_devices` と違い、列挙に失敗した経路の理由を捨てない。既定は
    /// `list_devices` を 1 つの経路として返すだけで、失敗を区別できない
    /// フェイクとモックはこれで足りる。本番（`system`）は Media Foundation と
    /// DirectShow を分けて返す
    fn enumerate(&self) -> VideoEnumeration {
        let names = self
            .list_devices()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        VideoEnumeration::single("list_devices", names)
    }
}

/// 映像を開くときの要求。
///
/// 引数で渡していたが、「音声ピンを繋ぐか」（#388）を足して `self` を入れて
/// 7 つになるので、`audio::PassthroughRequest` と同じく構造体へまとめた。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CaptureRequest<'a> {
    /// 開くデバイスの名前。`None` なら先頭のデバイス
    pub(super) device_name: Option<&'a str>,
    pub(super) resolution: Option<(u32, u32)>,
    pub(super) format: Option<&'a str>,
    pub(super) fps: Option<u32>,
    /// 設定の「映像の開き方」（`video.backend`）。経路が 1 つしかない実装
    /// （フェイク・モック）は見ない
    pub(super) backend: VideoBackendSetting,
    /// DirectShow で開くとき、同じグラフの音声ピンも繋ぐか
    /// （`[audio] input_source = "video_pin"` のときだけ真）。Media Foundation と
    /// フェイクは見ない。自動で DirectShow へ倒すとき（#387）も同じ指定を渡す
    pub(super) connect_audio_pin: bool,
}

/// 映像デバイスの列挙結果。経路（Media Foundation / DirectShow）ごとに分けて持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VideoEnumeration {
    /// `(経路名, 列挙結果)`。経路名はログにだけ出す
    pub(super) sources: Vec<(&'static str, Result<Vec<String>, VideoError>)>,
    /// 設定に書ける名前（`list_devices` と同じ表記）の一覧。
    /// **どれか 1 つの経路でも列挙に失敗したら `None`。** 失敗した経路に
    /// 目当てのデバイスが居たかもしれないので、「見えていない」とは言えない
    pub(super) selectable: Option<Vec<String>>,
}

impl VideoEnumeration {
    /// 経路が 1 つで、失敗しない実装（フェイク・モック）の列挙結果。
    pub(super) fn single(source: &'static str, names: Vec<String>) -> Self {
        Self {
            sources: vec![(source, Ok(names.clone()))],
            selectable: Some(names),
        }
    }
}

/// 音声デバイスの開閉・列挙・観測。
pub(super) trait AudioBackend {
    fn list_input_devices(&self) -> Vec<String>;
    fn list_output_devices(&self) -> Vec<String>;

    /// Windows 側の既定の出力デバイス名。切り替えの追従に使う（`super::worker_timers`）
    fn default_output_device_name(&self) -> Option<String>;

    /// デバイスが対応するサンプリングレートとチャンネル数
    fn capabilities(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<AudioCapabilities, AudioError>;

    /// 入力 → リングバッファ → 出力のパススルーを開く
    fn start_passthrough(&mut self, request: &PassthroughRequest<'_>) -> Result<(), AudioError>;

    /// ストリームを閉じる
    fn stop_capture(&mut self);

    /// 実際に開いたストリームの内容。開いていなければ `None`
    fn active(&self) -> Option<ActiveAudio>;

    /// クロックドリフト補正の現在値。「接続状態」タブへ出す
    fn resample_status(&self) -> Option<ResampleStatus>;

    /// クロックドリフト補正の共有状態。音声を開いていなければ `None`
    /// （入出力の形が揃っていても開いていれば作る）。
    ///
    /// **借用ではなく複製を返す。** trait オブジェクト越しでも扱いを揃える
    /// ためで、呼び出し側（`super::worker_timers`）はどのみち複製していた
    fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>>;

    /// 出力のアンダーラン累計。開いていなければ `None`
    fn underrun_count(&self) -> Option<u32>;

    /// 入力がリングバッファの満杯で捨てたフレーム数の累計。開いていなければ `None`
    fn dropped_frame_count(&self) -> Option<u32>;

    /// cpal が知らせた入力の取りこぼし（`Xrun`）の累計。開いていなければ `None`
    fn xrun_count(&self) -> Option<u32>;

    /// ストリームのエラー旗を読んで落とす。**読んだ時点で下りる**ので、
    /// 見送る場合は呼び出し側が保持する（`super::worker_timers`）
    fn take_stream_error(&self) -> bool;

    /// 向きごとの列挙結果。**ログと「Windows 側にも見えていない」の判定専用。**
    ///
    /// `list_*_devices` と違い、列挙に失敗した理由を捨てない。既定は
    /// `list_*_devices` をそのまま成功として返すだけで、失敗を区別できない
    /// フェイクとモックはこれで足りる
    fn enumerate_devices(&self, direction: AudioDirection) -> Result<Vec<String>, AudioError> {
        Ok(match direction {
            AudioDirection::Input => self.list_input_devices(),
            AudioDirection::Output => self.list_output_devices(),
        })
    }
}

/// バックエンドが UI スレッドと共有するハンドル一式。
///
/// どれも UI スレッドが先に作って複製を持ち続けるもので、`DeviceWorker::spawn`
/// から渡ってくる。**バックエンドを作れるのはワーカースレッドの中だけ**
/// （`cpal::Stream` は `!Send`）なので、材料だけを送ってあちら側で組み立てる。
pub(super) struct BackendShared {
    /// フレームコールバックが書き、UI スレッドが読む映像フレーム
    pub(super) frames: VideoFrames,
    /// UI スレッドが書き、フレームコールバックが読む色変換と映像調整
    pub(super) color_conversion: Arc<SharedColorConversion>,
    /// UI スレッドが書き、出力コールバックが読む音量・ミュート・パススルー
    pub(super) audio_controls: Arc<AudioControls>,
    /// 入力コールバックが書き、録画スレッドが読む録画の差し込み口。
    /// `audio_controls` と同じく開き直しても引き継ぐ
    pub(super) audio_tap: AudioTap,
    /// フレームが届いたことを UI スレッドへ知らせる窓口。
    /// **キャプチャを開くより前に渡す必要がある**（`VideoCapture::new` の説明）
    pub(super) repaint_waker: RepaintWaker,
}

/// 映像と音声のバックエンドを、ワーカースレッドの中で組み立てる役。
///
/// `DeviceWorker::spawn` が実装を 1 つ選んでスレッドへ送り、テストはモックを
/// 送る。**`Send` が要るのはこの型だけ**で、作られたあとのバックエンドは
/// ワーカースレッドから出ない。
pub(super) trait DeviceBackends: Send {
    fn create(
        self: Box<Self>,
        shared: BackendShared,
    ) -> (Box<dyn VideoBackend>, Box<dyn AudioBackend>);
}

/// 環境変数を見て、使うバックエンドを選ぶ。
///
/// **呼ぶのは `DeviceWorker::spawn` の 1 か所だけ。** `CAPTURECARD_VIEWER_FAKE_DEVICES`
/// が無ければ本番（`SystemBackends`）で、今までと何も変わらない。
///
/// 2 つ目の値はフェイクを選んだか。UI 側が画面へ知らせるのに使う（#252）
pub(super) fn backends_from_env() -> (Box<dyn DeviceBackends>, bool) {
    let devices = std::env::var(FAKE_DEVICES_ENV).ok();
    let scenario = std::env::var(FAKE_SCENARIO_ENV).ok();
    match FakeBackends::from_env_values(devices.as_deref(), scenario.as_deref()) {
        Some(fake) => {
            // 実機が映らない理由がログから分かるよう、目立つ段で残す
            warn!(
                "{} が指定されているので、実機ではなくフェイクデバイスで動く（{} 台、シナリオ: {:?}）",
                FAKE_DEVICES_ENV, fake.device_count, fake.scenario
            );
            (Box::new(fake), true)
        }
        None => (Box::new(SystemBackends), false),
    }
}

/// テスト用のモック。**実機なしでワーカーの再試行と切断検出を回すためだけのもの。**
///
/// 映像や音声の中身は作らない（カラーバーや正弦波を吐くのは `fake`）。
/// ここにあるのは「指定回数失敗してから成功する」「列挙結果を差し替える」
/// 「フレームが止まったことにする」「音声ストリームのエラーを起こす」の
/// 4 つだけ。
///
/// 状態は `Arc<Mutex<..>>` で外に出してある。バックエンドはワーカーへ
/// 渡してしまうと手元に残らないので、テスト側は共有した中身を覗く。
#[cfg(test)]
pub(super) mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// 映像モックの中身。テストが直接読み書きする。
    #[derive(Debug, Default)]
    pub(in crate::app) struct MockVideoState {
        /// 成功させるまでに失敗させる回数。0 なら最初から成功する
        pub(in crate::app) failures_before_success: u32,
        /// 失敗させるときに返すエラー。`None` なら `DeviceNotFound`（見つからない）
        pub(in crate::app) failure: Option<VideoError>,
        /// `start_capture` を呼ばれた回数
        pub(in crate::app) start_calls: u32,
        /// `stop_capture` を呼ばれた回数
        pub(in crate::app) stop_calls: u32,
        /// `list_devices` が返す一覧
        pub(in crate::app) devices: Vec<(String, String)>,
        /// ストリームを開けている状態か。`start_capture` の成否で動く
        pub(in crate::app) capturing: bool,
        /// `link_state` が返す途絶時間。`None` は「まだ 1 枚も届いていない」。
        /// **ここへ `VIDEO_SIGNAL_TIMEOUT` より長い値を入れると切断になる**
        pub(in crate::app) since_last_frame: Option<Duration>,
        /// `link_state` が返すデバイス喪失の知らせ（DirectShow の `EC_DEVICE_LOST`
        /// の代わり）。立てると途絶時間に関係なく切断になる。開き直す・閉じると下りる
        pub(in crate::app) device_lost: bool,
        /// 最後に開こうとしたデバイス名
        pub(in crate::app) last_device_name: Option<String>,
        /// 最後に開こうとしたときの「映像の開き方」
        pub(in crate::app) last_backend: Option<VideoBackendSetting>,
        /// `capabilities` が開き方ごとに返す対応形式。無い開き方は空の一覧
        pub(in crate::app) capabilities: HashMap<VideoBackendSetting, DeviceCapabilities>,
        /// 最後に対応形式を問い合わせたときの「映像の開き方」
        pub(in crate::app) last_capabilities_backend: Option<VideoBackendSetting>,
        /// `active` が返す開いた解像度。`None` なら解像度を返さない（実機の観測値が無い形）
        pub(in crate::app) opened_resolution: Option<(u32, u32)>,
        /// 最後に開こうとしたときの「音声ピンを繋ぐか」
        pub(in crate::app) last_connect_audio_pin: Option<bool>,
        /// 立てておくと、DirectShow のデバイスのように音声ピンを持つ。開くたびに
        /// `audio_pin` を「繋いだ（番号 = `start_calls`）」か「あるが繋いでいない」へ書き換える
        pub(in crate::app) has_audio_pin: bool,
        /// `active` が返す音声ピンの状態。`has_audio_pin` が偽ならテストが書いた値のまま
        pub(in crate::app) audio_pin: crate::audio::AudioPinState,
    }

    /// 映像バックエンドのモック。複製しても同じ中身を指す。
    #[derive(Debug, Clone, Default)]
    pub(in crate::app) struct MockVideoBackend {
        state: Arc<Mutex<MockVideoState>>,
    }

    impl MockVideoBackend {
        /// 中身を書き換える / 読む。ロックが壊れていたらテストごと落とす
        pub(in crate::app) fn with<R>(&self, f: impl FnOnce(&mut MockVideoState) -> R) -> R {
            f(&mut self.state.lock().expect("モックの状態を触れる"))
        }
    }

    impl VideoBackend for MockVideoBackend {
        fn list_devices(&self) -> Vec<(String, String)> {
            self.with(|state| state.devices.clone())
        }

        fn capabilities(
            &self,
            _device_name: Option<&str>,
            backend: VideoBackendSetting,
        ) -> Result<DeviceCapabilities, VideoError> {
            self.with(|state| {
                state.last_capabilities_backend = Some(backend);
                Ok(state
                    .capabilities
                    .get(&backend)
                    .cloned()
                    .unwrap_or_default())
            })
        }

        fn start_capture(&mut self, request: &CaptureRequest<'_>) -> Result<(), VideoError> {
            let device_name = request.device_name;
            self.with(|state| {
                state.start_calls += 1;
                state.last_device_name = device_name.map(str::to_string);
                state.last_backend = Some(request.backend);
                state.last_connect_audio_pin = Some(request.connect_audio_pin);
                if state.has_audio_pin {
                    state.audio_pin = if request.connect_audio_pin {
                        crate::audio::AudioPinState::Connected(mock_pin_connection(u64::from(
                            state.start_calls,
                        )))
                    } else {
                        crate::audio::AudioPinState::Available
                    };
                }
                if state.failures_before_success > 0 {
                    state.failures_before_success -= 1;
                    state.capturing = false;
                    return Err(state.failure.clone().unwrap_or_else(|| {
                        VideoError::DeviceNotFound(device_name.unwrap_or("（未指定）").to_string())
                    }));
                }
                state.capturing = true;
                // 開き直したら途絶の記録も喪失の知らせも消える（実装と同じ）
                state.since_last_frame = None;
                state.device_lost = false;
                Ok(())
            })
        }

        fn stop_capture(&mut self) {
            self.with(|state| {
                state.stop_calls += 1;
                state.capturing = false;
                state.since_last_frame = None;
                state.device_lost = false;
            });
        }

        fn link_state(&self) -> VideoLinkState {
            self.with(|state| VideoLinkState {
                capturing: state.capturing,
                since_last_frame: state.since_last_frame,
                device_lost: state.device_lost,
            })
        }

        fn active(&self) -> Option<ActiveVideo> {
            self.with(|state| {
                state.capturing.then(|| ActiveVideo {
                    device_name: state.last_device_name.clone().unwrap_or_default(),
                    api: crate::video::capture::CaptureApi::Fake,
                    resolution: state.opened_resolution,
                    format: None,
                    format_fallback: None,
                    requested_fps: 0,
                    audio_pin: state.audio_pin.clone(),
                })
            })
        }
    }

    /// 音声ピンを持つモックが「繋いだ」ときの中身。番号以外は GC551 の形
    pub(in crate::app) fn mock_pin_connection(graph: u64) -> crate::audio::PinConnection {
        crate::audio::PinConnection {
            graph,
            device: "モックの音声ピン付きカメラ".to_string(),
            format: crate::audio::PinFormat {
                sample_rate: 48_000,
                channels: 2,
                sample_type: crate::audio::PinSampleType::I16,
            },
            chunk_bytes: Some(1920),
        }
    }

    /// 音声モックの中身。
    #[derive(Debug, Default)]
    pub(in crate::app) struct MockAudioState {
        pub(in crate::app) failures_before_success: u32,
        pub(in crate::app) start_calls: u32,
        pub(in crate::app) stop_calls: u32,
        pub(in crate::app) input_devices: Vec<String>,
        pub(in crate::app) output_devices: Vec<String>,
        /// パススルーを開けている状態か
        pub(in crate::app) running: bool,
        /// 立てておくと `take_stream_error` が 1 回だけ真を返す
        pub(in crate::app) stream_error: bool,
        /// 立てておくと `capabilities` が空の一覧で成功する（既定は失敗）
        pub(in crate::app) capabilities_ok: bool,
        /// `capabilities` が呼ばれた向きを順に記録する
        pub(in crate::app) capability_queries: Vec<AudioDirection>,
        /// `start_passthrough` を失敗させるときの向き。`None` なら入力
        pub(in crate::app) failure_direction: Option<AudioDirection>,
        /// 最後に開いたときの音声ピンの番号。WASAPI の入力で開いたら `None`
        pub(in crate::app) pin_graph: Option<u64>,
    }

    /// 音声バックエンドのモック。
    #[derive(Debug, Clone, Default)]
    pub(in crate::app) struct MockAudioBackend {
        state: Arc<Mutex<MockAudioState>>,
    }

    impl MockAudioBackend {
        pub(in crate::app) fn with<R>(&self, f: impl FnOnce(&mut MockAudioState) -> R) -> R {
            f(&mut self.state.lock().expect("モックの状態を触れる"))
        }
    }

    impl AudioBackend for MockAudioBackend {
        fn list_input_devices(&self) -> Vec<String> {
            self.with(|state| state.input_devices.clone())
        }

        fn list_output_devices(&self) -> Vec<String> {
            self.with(|state| state.output_devices.clone())
        }

        fn default_output_device_name(&self) -> Option<String> {
            None
        }

        fn capabilities(
            &self,
            direction: AudioDirection,
            _device_name: Option<&str>,
        ) -> Result<AudioCapabilities, AudioError> {
            self.with(|state| {
                state.capability_queries.push(direction);
                if state.capabilities_ok {
                    Ok(AudioCapabilities::new(Vec::new(), 48_000, 2))
                } else {
                    Err(AudioError::NoDefaultDevice(direction))
                }
            })
        }

        fn start_passthrough(
            &mut self,
            request: &PassthroughRequest<'_>,
        ) -> Result<(), AudioError> {
            self.with(|state| {
                state.start_calls += 1;
                state.pin_graph = match request.input {
                    crate::audio::PassthroughInput::VideoPin { graph } => Some(graph),
                    crate::audio::PassthroughInput::Device(_) => None,
                };
                if state.failures_before_success > 0 {
                    state.failures_before_success -= 1;
                    state.running = false;
                    return Err(AudioError::NoDefaultDevice(
                        state.failure_direction.unwrap_or(AudioDirection::Input),
                    ));
                }
                state.running = true;
                Ok(())
            })
        }

        fn stop_capture(&mut self) {
            self.with(|state| {
                state.stop_calls += 1;
                state.running = false;
            });
        }

        fn active(&self) -> Option<ActiveAudio> {
            self.with(|state| {
                state.running.then(|| ActiveAudio {
                    input_device: "モック入力".to_string(),
                    output_device: "モック出力".to_string(),
                    input_sample_rate: 48_000,
                    output_sample_rate: 48_000,
                    input_channels: 2,
                    output_channels: 2,
                    input_route: match state.pin_graph {
                        Some(graph) => crate::audio::AudioInputRoute::VideoPin { graph },
                        None => crate::audio::AudioInputRoute::Device,
                    },
                    widened_buffer: None,
                })
            })
        }

        fn resample_status(&self) -> Option<ResampleStatus> {
            None
        }

        fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>> {
            None
        }

        fn underrun_count(&self) -> Option<u32> {
            None
        }

        fn dropped_frame_count(&self) -> Option<u32> {
            None
        }

        fn xrun_count(&self) -> Option<u32> {
            None
        }

        fn take_stream_error(&self) -> bool {
            self.with(|state| std::mem::take(&mut state.stream_error))
        }
    }

    /// モックを組み立てる役。`DeviceBackends` として `worker_loop::run` へ渡す。
    #[derive(Debug, Clone, Default)]
    pub(in crate::app) struct MockBackends {
        pub(in crate::app) video: MockVideoBackend,
        pub(in crate::app) audio: MockAudioBackend,
    }

    impl DeviceBackends for MockBackends {
        fn create(
            self: Box<Self>,
            _shared: BackendShared,
        ) -> (Box<dyn VideoBackend>, Box<dyn AudioBackend>) {
            (Box::new(self.video.clone()), Box::new(self.audio.clone()))
        }
    }
}
