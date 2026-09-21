//! デバイスワーカーとやり取りする型と、UI 側の窓口。
//!
//! デバイスを開く・閉じる・列挙する・能力を問い合わせる処理は、すべて専用の
//! ワーカースレッド 1 本（`super::worker_loop`）が行う。UI スレッドは
//! `DeviceCommand` を送り、`DeviceEvent` を `update()` の中で非ブロックに
//! 受け取るだけにする。
//!
//! **チャネルを通さない共有が 3 つある。** 映像フレーム（`video::VideoFrames`）、
//! 色変換と映像調整（`video::SharedColorConversion`）、音量・ミュート・
//! パススルー（`audio::AudioControls`）。どれもデバイスを開く処理を挟まない
//! うえ、フレームはコマンドの列に並べると遅延が増える。
//!
//! これとは別に、「いま何に繋がっているか」のような軽い観測値も
//! チャネルを通さず `DeviceSnapshot` に写してあり、UI は `Arc<RwLock<..>>`
//! 越しに読む。イベントを取りこぼしても表示が食い違わないよう、状態は
//! 必ずこちらを正とする。

use crate::audio::{ActiveAudio, AudioCapabilities, AudioControls, AudioDirection, ResampleStatus};
use crate::repaint::RepaintWaker;
use crate::settings::AppSettings;
use crate::video::{ActiveVideo, DeviceCapabilities, SharedColorConversion, VideoFrames};
use log::{debug, warn};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

/// 映像の接続対象。これが変わったらバックオフを捨てて即座に開き直す。
/// `(デバイス名, 解像度, フォーマット, fps)`
pub(super) type VideoTarget = (
    Option<String>,
    Option<(u32, u32)>,
    Option<String>,
    Option<u32>,
);

/// 音声の接続対象。
/// `(入力デバイス名, 出力デバイス名, サンプリングレート, チャンネル数, バッファ長 ms)`
///
/// バッファ長を含めてあるのは、リングバッファの長さがストリームを開くときに
/// しか決まらないため。設定ダイアログで変えたら音声だけを開き直す
pub(super) type AudioTarget = (
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<u16>,
    u32,
);

/// ワーカーがデバイスを開くために要る設定。
///
/// **`AppSettings` を丸ごと渡さない。** ワーカーは設定の持ち主ではないので、
/// デバイスに関係する項目だけを写して渡す。差分の判定（開き直しが要るか）は
/// ワーカー側が行うため、UI は毎回そのまま送ればよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeviceConfig {
    pub(super) video: VideoTarget,
    pub(super) audio: AudioTarget,
    /// 切断を検出したときに自動で開き直すか（`video.auto_reconnect`）
    pub(super) auto_reconnect: bool,
}

impl DeviceConfig {
    /// 設定からデバイスに関係する項目だけを写す。
    pub(super) fn from_settings(settings: &AppSettings) -> Self {
        Self {
            video: (
                settings.video.device_name.clone(),
                settings.video.resolution,
                settings.video.format.clone(),
                settings.video.fps,
            ),
            audio: (
                settings.audio.input_device_name.clone(),
                settings.audio.output_device_name.clone(),
                settings.audio.sample_rate,
                settings.audio.channels,
                settings.audio.buffer_ms,
            ),
            auto_reconnect: settings.video.auto_reconnect,
        }
    }
}

/// UI スレッドからワーカーへ送る要求。
#[derive(Debug)]
pub(super) enum DeviceCommand {
    /// デバイスに関係する設定を渡す。差分の判定はワーカーが行う。
    ///
    /// `initial` はアプリの起動直後の 1 回だけ真にする。未設定のデバイス名を
    /// 列挙結果の先頭で埋めるのはこのときだけで、以降は設定の `None` を
    /// 「Windows の既定デバイス」の意味のまま扱う。
    ApplyConfig {
        config: Box<DeviceConfig>,
        initial: bool,
    },
    /// バックオフを飛ばして映像・音声とも開き直す（右クリックの「デバイス再接続」）
    ReconnectNow,
    /// デバイス一覧を取り直す。設定ダイアログの選択肢に使う
    RefreshDeviceLists,
    /// 映像デバイスの対応形式を問い合わせる
    QueryVideoCapabilities(String),
    /// 音声デバイスの対応設定を問い合わせる。キーは `audio::cache_key`
    QueryAudioCapabilities(AudioDirection, String),
    /// ストリームを閉じてスレッドを終える
    Shutdown,
}

/// ワーカーから UI スレッドへ返す結果。
#[derive(Debug)]
pub(super) enum DeviceEvent {
    /// 映像デバイスに接続した
    VideoConnected,
    /// 映像デバイスへの接続に失敗した
    VideoFailed(String),
    /// 音声デバイスに接続した
    AudioConnected,
    /// 音声デバイスへの接続に失敗した
    AudioFailed(String),
    /// 映像フレームが途絶えたので、表示中のテクスチャを捨ててほしい。
    /// 開き直すかどうかはワーカーが判断済みで、UI は表示を戻すだけ
    VideoSignalLost,
    /// 映像デバイスの対応形式が揃った
    VideoCapabilities(String, Box<Result<DeviceCapabilities, String>>),
    /// 音声デバイスの対応設定が揃った
    AudioCapabilities(
        AudioDirection,
        String,
        Box<Result<AudioCapabilities, String>>,
    ),
    /// デバイス一覧を取り直した
    DeviceLists {
        video: Vec<(String, String)>,
        input: Vec<String>,
        output: Vec<String>,
    },
    /// 未設定だったデバイス名を、列挙結果の先頭で埋めた（起動直後の 1 回だけ）。
    /// UI スレッドが設定へ書き戻す
    DefaultDevicesResolved {
        video: Option<String>,
        input: Option<String>,
    },
}

/// 再試行の進み具合。「接続状態」タブに出す。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RetryStatus {
    /// 接続を追いかけている最中か。繋がると `false` に戻る
    pub(super) active: bool,
    /// 連続して失敗した回数
    pub(super) attempts: u32,
}

/// ワーカーが定期的に更新する、軽い観測値。
///
/// UI スレッドは描画のたびにここを読む。**`VideoCapture` /
/// `AudioCapture` のロックを UI から取らないために置いてある。**
/// 中身はどれも小さな値の複製だけで、デバイスへの問い合わせを伴わない。
#[derive(Debug, Clone, Default)]
pub(super) struct DeviceSnapshot {
    /// 映像ストリームを開けているか
    pub(super) video_capturing: bool,
    /// 実際に開いた映像ストリームの内容
    pub(super) active_video: Option<ActiveVideo>,
    /// 実際に開いた音声ストリームの内容
    pub(super) active_audio: Option<ActiveAudio>,
    pub(super) video_retry: RetryStatus,
    pub(super) audio_retry: RetryStatus,
    /// 音声のクロックドリフト補正の現在値。「接続状態」タブへ出す想定だが、
    /// 表示側（`ui.rs`）はまだ実装していないので読まれていない（Issue #132）。
    /// 表示を足すまでの間、警告を黙らせる
    #[allow(dead_code)]
    pub(super) audio_resample: Option<ResampleStatus>,
}

/// ワーカースレッドと、UI スレッドが共有する読み取り専用のスナップショット。
pub(super) type SharedSnapshot = Arc<RwLock<DeviceSnapshot>>;

/// UI スレッド側の窓口。
///
/// コマンドの送信・イベントの受信・スナップショットの読み出しと、
/// 終了時の join をまとめる。
pub(super) struct DeviceWorker {
    commands: Sender<DeviceCommand>,
    events: Receiver<DeviceEvent>,
    snapshot: SharedSnapshot,
    /// ワーカースレッドのハンドル。`shutdown` で join したら `None` になる
    handle: Option<JoinHandle<()>>,
}

impl DeviceWorker {
    /// ワーカースレッドを起動する。
    ///
    /// フレームバッファ・色変換・音量は UI スレッドと共有するので、呼び出し側が
    /// 先に作って複製を渡す。**`VideoCapture` と `AudioCapture` はワーカー
    /// スレッドの中で作る。** `cpal::Stream` は `!Send` で、作ったスレッド以外へ
    /// 持ち出せないため。
    pub(super) fn spawn(
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        audio_controls: Arc<AudioControls>,
        repaint_waker: RepaintWaker,
    ) -> Self {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let snapshot: SharedSnapshot = Arc::new(RwLock::new(DeviceSnapshot::default()));

        let thread_snapshot = Arc::clone(&snapshot);
        let handle = std::thread::Builder::new()
            .name("device-worker".to_string())
            .spawn(move || {
                super::worker_loop::run(
                    command_rx,
                    event_tx,
                    thread_snapshot,
                    frames,
                    color_conversion,
                    audio_controls,
                    repaint_waker,
                );
            });

        let handle = match handle {
            Ok(handle) => {
                debug!("デバイスワーカースレッドを起動した");
                Some(handle)
            }
            Err(e) => {
                // 起動できないのはスレッドを作れないほど資源が尽きている場合だけ。
                // 映像も音声も出ないが、ウィンドウは開いたままにして理由を残す
                warn!("デバイスワーカースレッドを起動できない: {}", e);
                None
            }
        };

        Self {
            commands: command_tx,
            events: event_rx,
            snapshot,
            handle,
        }
    }

    /// コマンドを送る。ワーカーが落ちている場合はログへ残して捨てる。
    pub(super) fn send(&self, command: DeviceCommand) {
        if let Err(e) = self.commands.send(command) {
            warn!("デバイスワーカーへコマンドを送れない: {}", e);
        }
    }

    /// 届いているイベントを 1 つ取り出す。無ければ `None`。
    pub(super) fn try_recv(&self) -> Option<DeviceEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            // 送り手が居ないのはワーカーが終わったときだけ。
            // 毎フレーム通るのでログは出さない
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// 観測値の複製を返す。
    ///
    /// ロックを取れない（書き込み側がパニックした）場合は既定値を返す。
    /// 「何も繋がっていない」表示になるだけで、描画は止めない。
    pub(super) fn snapshot(&self) -> DeviceSnapshot {
        match self.snapshot.read() {
            Ok(snapshot) => snapshot.clone(),
            Err(_) => {
                warn!("デバイスの観測値を読めないので既定値で表示する");
                DeviceSnapshot::default()
            }
        }
    }

    /// 停止を伝えて、スレッドが終わるまで待つ。
    ///
    /// **`on_exit` から必ず呼ぶ。** 待たずに抜けると、ストリームを閉じる前に
    /// プロセスが落ちる。スクリーンショットの保存スレッドと同じ扱い。
    pub(super) fn shutdown(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.send(DeviceCommand::Shutdown);
        match handle.join() {
            Ok(()) => debug!("デバイスワーカースレッドの終了を待ち終えた"),
            Err(_) => warn!("デバイスワーカースレッドが異常終了していた"),
        }
    }
}
