//! デバイスワーカーがデバイスに触るときの入口。
//!
//! **trait の境界は「ワーカーがデバイスへ触る場所」に置いてある。**
//! 具体的には開く・閉じる・列挙する・能力を問い合わせる・観測値を読む、の
//! 5 つだけで、`super::worker_connect` と `super::worker_timers` が呼ぶ操作が
//! そのまま並ぶ。実装は `crate::video::VideoCapture` / `crate::audio::AudioCapture`
//! で、どちらも中身には手を入れず、ここで trait に包んでいる。
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
    self, ActiveAudio, AudioCapabilities, AudioCapture, AudioControls, AudioDirection, AudioError,
    PassthroughRequest, ResampleStatus, ResampleTelemetry,
};
use crate::repaint::RepaintWaker;
use crate::video::{
    ActiveVideo, DeviceCapabilities, SharedColorConversion, VideoCapture, VideoError, VideoFrames,
    VideoLinkState,
};
use std::sync::Arc;

/// 映像デバイスの開閉・列挙・観測。
///
/// **開いた結果を別のハンドル型では返さない。** ストリームを持つのは実装
/// 自身で、`stop_capture` / `link_state` / `active` がその持ち物に対する窓口に
/// なる。`VideoCapture` は `CallbackCamera` を内部に抱えたまま開き直しや
/// 途絶の判定を行っており、開いた分だけを別の型へ切り出すには `video.rs` の
/// 中身を動かす必要がある。そこはこの抽象化の目的ではない。
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

/// 本番のバックエンド。映像は Media Foundation（nokhwa）、音声は WASAPI（cpal）。
pub(super) struct SystemBackends;

impl DeviceBackends for SystemBackends {
    fn create(
        self: Box<Self>,
        shared: BackendShared,
    ) -> (Box<dyn VideoBackend>, Box<dyn AudioBackend>) {
        let BackendShared {
            frames,
            color_conversion,
            audio_controls,
            repaint_waker,
        } = shared;
        (
            Box::new(VideoCapture::new(frames, color_conversion, repaint_waker)),
            Box::new(AudioCapture::new(audio_controls)),
        )
    }
}

impl VideoBackend for VideoCapture {
    fn list_devices(&self) -> Vec<(String, String)> {
        VideoCapture::list_devices()
    }

    fn capabilities(&self, device_name: Option<&str>) -> Result<DeviceCapabilities, VideoError> {
        VideoCapture::get_device_capabilities(device_name)
    }

    fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        VideoCapture::start_capture(self, device_name, resolution, format, fps)
    }

    fn stop_capture(&mut self) {
        VideoCapture::stop_capture(self);
    }

    fn link_state(&self) -> VideoLinkState {
        VideoCapture::link_state(self)
    }

    fn active(&self) -> Option<ActiveVideo> {
        VideoCapture::active(self)
    }
}

impl AudioBackend for AudioCapture {
    fn list_input_devices(&self) -> Vec<String> {
        AudioCapture::list_input_devices(self)
    }

    fn list_output_devices(&self) -> Vec<String> {
        AudioCapture::list_output_devices(self)
    }

    fn default_input_device_name(&self) -> Option<String> {
        AudioCapture::default_input_device_name(self)
    }

    fn default_output_device_name(&self) -> Option<String> {
        AudioCapture::default_output_device_name(self)
    }

    fn capabilities(
        &self,
        direction: AudioDirection,
        device_name: Option<&str>,
    ) -> Result<AudioCapabilities, AudioError> {
        // **`AudioCapture` のホストは使わない自由関数を呼ぶ。** 元から
        // そういう作りで、`AudioCapture` を持たないスレッドからも使える
        audio::query_capabilities(direction, device_name)
    }

    fn start_passthrough(&mut self, request: &PassthroughRequest<'_>) -> Result<(), AudioError> {
        AudioCapture::start_passthrough(self, request)
    }

    fn stop_capture(&mut self) {
        AudioCapture::stop_capture(self);
    }

    fn active(&self) -> Option<ActiveAudio> {
        AudioCapture::active(self)
    }

    fn resample_status(&self) -> Option<ResampleStatus> {
        AudioCapture::resample_status(self)
    }

    fn resample_telemetry(&self) -> Option<Arc<ResampleTelemetry>> {
        AudioCapture::resample_telemetry(self).cloned()
    }

    fn underrun_count(&self) -> Option<u32> {
        AudioCapture::underrun_count(self)
    }

    fn take_stream_error(&self) -> bool {
        AudioCapture::take_stream_error(self)
    }
}
