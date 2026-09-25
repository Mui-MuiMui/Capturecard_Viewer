//! デバイスワーカーがデバイスに触るときの入口。
//!
//! **trait の境界は「ワーカーがデバイスへ触る場所」に置いてある。**
//! 具体的には開く・閉じる・列挙する・能力を問い合わせる・観測値を読む、の
//! 5 つだけで、`super::worker_connect` と `super::worker_timers` が呼ぶ操作が
//! そのまま並ぶ。本番の実装は `crate::video::VideoCapture` /
//! `crate::audio::AudioCapture` で、どちらも中身には手を入れず、`system` で
//! trait に包んでいる。
//!
//! **フレームコールバックと cpal のコールバックの経路には挟まない。**
//! 映像フレームは `VideoFrames`、音量とミュートは `AudioControls` の共有
//! ハンドル越しに流れ続ける。あのコールバックはロックもアロケーションも
//! しない決まりなので、動的ディスパッチを足す場所ではない
//! （`docs/design/video-pipeline.md` / `docs/design/audio.md`）。
//!
//! テスト用のモックは同じファイルの `mock`（`#[cfg(test)]`）にある。
//! 実機なしで動くフェイクデバイス（カラーバーや正弦波を吐く実装）は
//! #142 でこの trait の実装として足す予定で、ここには置かない。

use crate::audio::{
    ActiveAudio, AudioCapabilities, AudioControls, AudioDirection, AudioError, PassthroughRequest,
    ResampleStatus, ResampleTelemetry,
};
use crate::repaint::RepaintWaker;
use crate::video::{
    ActiveVideo, DeviceCapabilities, SharedColorConversion, VideoError, VideoFrames, VideoLinkState,
};
use std::sync::Arc;

mod system;

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

    /// デバイスが対応する形式の一覧。`None` なら先頭のデバイス
    fn capabilities(&self, device_name: Option<&str>) -> Result<DeviceCapabilities, VideoError>;

    /// ストリームを開く。既に開いていれば閉じてから開き直す
    fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError>;

    /// ストリームを閉じる。開いていなければ何もしない
    fn stop_capture(&mut self);

    /// 開けているかと、最後のフレームからの経過時間。切断の判定に使う
    fn link_state(&self) -> VideoLinkState;

    /// 実際に開いたストリームの内容。開いていなければ `None`
    fn active(&self) -> Option<ActiveVideo>;
}

/// 音声デバイスの開閉・列挙・観測。
pub(super) trait AudioBackend {
    fn list_input_devices(&self) -> Vec<String>;
    fn list_output_devices(&self) -> Vec<String>;

    /// Windows 側の既定デバイス名。切り替えの追従に使う（`super::worker_timers`）
    fn default_input_device_name(&self) -> Option<String>;
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

    /// クロックドリフト補正の共有状態。変換が要らない組み合わせでは `None`。
    ///
    /// **借用ではなく複製を返す。** trait オブジェクト越しでも扱いを揃える
    /// ためで、呼び出し側（`super::worker_timers`）はどのみち複製していた
    fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>>;

    /// 出力のアンダーラン累計。開いていなければ `None`
    fn underrun_count(&self) -> Option<u32>;

    /// ストリームのエラー旗を読んで落とす。**読んだ時点で下りる**ので、
    /// 見送る場合は呼び出し側が保持する（`super::worker_timers`）
    fn take_stream_error(&self) -> bool;
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

/// テスト用のモック。**実機なしでワーカーの再試行と切断検出を回すためだけのもの。**
///
/// 映像や音声の中身は作らない（カラーバーや正弦波を吐くフェイクは #142）。
/// ここにあるのは「指定回数失敗してから成功する」「列挙結果を差し替える」
/// 「フレームが止まったことにする」「音声ストリームのエラーを起こす」の
/// 4 つだけ。
///
/// 状態は `Arc<Mutex<..>>` で外に出してある。バックエンドはワーカーへ
/// 渡してしまうと手元に残らないので、テスト側は共有した中身を覗く。
#[cfg(test)]
pub(super) mod mock {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    /// 映像モックの中身。テストが直接読み書きする。
    #[derive(Debug, Default)]
    pub(in crate::app) struct MockVideoState {
        /// 成功させるまでに失敗させる回数。0 なら最初から成功する
        pub(in crate::app) failures_before_success: u32,
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
        /// 最後に開こうとしたデバイス名
        pub(in crate::app) last_device_name: Option<String>,
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
        ) -> Result<DeviceCapabilities, VideoError> {
            Ok(Vec::new())
        }

        fn start_capture(
            &mut self,
            device_name: Option<&str>,
            _resolution: Option<(u32, u32)>,
            _format: Option<&str>,
            _fps: Option<u32>,
        ) -> Result<(), VideoError> {
            self.with(|state| {
                state.start_calls += 1;
                state.last_device_name = device_name.map(str::to_string);
                if state.failures_before_success > 0 {
                    state.failures_before_success -= 1;
                    state.capturing = false;
                    return Err(VideoError::DeviceNotFound(
                        device_name.unwrap_or("（未指定）").to_string(),
                    ));
                }
                state.capturing = true;
                // 開き直したら途絶の記録も消える（実装と同じ）
                state.since_last_frame = None;
                Ok(())
            })
        }

        fn stop_capture(&mut self) {
            self.with(|state| {
                state.stop_calls += 1;
                state.capturing = false;
                state.since_last_frame = None;
            });
        }

        fn link_state(&self) -> VideoLinkState {
            self.with(|state| VideoLinkState {
                capturing: state.capturing,
                since_last_frame: state.since_last_frame,
            })
        }

        fn active(&self) -> Option<ActiveVideo> {
            self.with(|state| {
                state.capturing.then(|| ActiveVideo {
                    device_name: state.last_device_name.clone().unwrap_or_default(),
                    resolution: None,
                    format: None,
                    requested_fps: 0,
                })
            })
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

        fn default_input_device_name(&self) -> Option<String> {
            None
        }

        fn default_output_device_name(&self) -> Option<String> {
            None
        }

        fn capabilities(
            &self,
            direction: AudioDirection,
            _device_name: Option<&str>,
        ) -> Result<AudioCapabilities, AudioError> {
            Err(AudioError::NoDefaultDevice(direction))
        }

        fn start_passthrough(
            &mut self,
            _request: &PassthroughRequest<'_>,
        ) -> Result<(), AudioError> {
            self.with(|state| {
                state.start_calls += 1;
                if state.failures_before_success > 0 {
                    state.failures_before_success -= 1;
                    state.running = false;
                    return Err(AudioError::NoDefaultDevice(AudioDirection::Input));
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
