#![windows_subsystem = "windows"]
// テストの中だけ println! を許す。Cargo.toml の [lints.clippy] で
// print_stdout / print_stderr を warn にしてアプリ本体への再混入を止めているが、
// テストバイナリの標準出力は cargo が受け取るため cargo test -- --nocapture で読める。
// 計測結果の出力（src/video.rs）はそれを利用している。
// クレートルートに置いているのは、テスト対象のモジュール側を触らずに済ませるため
#![cfg_attr(test, allow(clippy::print_stdout))]

use chrono::Local;
use eframe::egui;
use image::GenericImageView;
use log::{debug, error, info, trace, warn};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod audio;
mod hotkey;
mod logging;
mod overlay;
mod screenshot;
mod settings;
mod status;
mod ui;
mod video;

use audio::AudioCapture;
use hotkey::{HotkeyAction, HotkeyError, HotkeyManager};
use overlay::{OverlayContent, TransientOverlay};
use screenshot::ScreenshotManager;
use settings::{
    AppSettings, AutoSavePolicy, ColorRange, ColorSpace, ScreenshotEncoding, MAX_VOLUME, MIN_VOLUME,
};
use status::{ConnectionStatus, ErrorCenter, ErrorSource, LinkStatus};
use video::{FrameStats, VideoCapture};

/// デバイスリストのキャッシュを更新する間隔
const DEVICE_LIST_CACHE_INTERVAL: Duration = Duration::from_secs(5);

/// 保存されたウィンドウサイズが使えない場合に使う大きさ
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1280.0, 720.0);

/// 復元したウィンドウを「画面内にある」と見なすために必要な、モニタの作業領域との
/// 重なりの最小幅と最小高さ。タイトルバーを掴んでウィンドウを動かせる程度の
/// 大きさを見えていることの条件にしている
const MIN_VISIBLE_WINDOW_WIDTH: f32 = 120.0;
const MIN_VISIBLE_WINDOW_HEIGHT: f32 = 32.0;

/// 設定をディスクへ書き出すまでに待つ時間。
/// ウィンドウのドラッグ中や音量スクロール中は設定が毎フレーム変わるため、
/// 最後の変更からこの時間が空くまで書き出しをまとめる
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

/// フルスクリーンを切り替えたときに OSD を出しておく時間
const FULLSCREEN_OSD_DURATION: Duration = Duration::from_secs(1);

/// 装飾なしにしたときにドラッグ移動を自動で有効にした、と知らせる OSD の表示時間。
/// フルスクリーンの表示より長いのは、こちらが「設定を勝手に変えた」報告で、
/// 読ませる必要があるため
const DRAG_MOVE_GUARD_OSD_DURATION: Duration = Duration::from_secs(2);

/// 上記の OSD に出す文言
const DRAG_MOVE_GUARD_MESSAGE: &str = "ウィンドウを動かすため、画面ドラッグ移動を有効にしました";

/// 装飾なしのとき、ウィンドウの端を「リサイズを始める場所」と見なす幅。
/// 掴みやすさと、映像のドラッグ移動を邪魔しないことの兼ね合いで決めている
const RESIZE_BORDER: f32 = 8.0;

/// 音量を変えたときに OSD を出しておく時間。
/// ホイールを回している間は回すたびに延びるので、これは「手を止めてから」の長さ
const VOLUME_OSD_DURATION: Duration = Duration::from_millis(1500);

/// 音量の基準値。OSD のバーはこの位置に目盛りを引く
const VOLUME_REFERENCE: f32 = 100.0;

/// ホイール 1 段、またはホットキー 1 回で動かす音量
const VOLUME_SCROLL_STEP: f32 = 10.0;

/// デバイス能力の取得結果。`(問い合わせたデバイス名, 結果)`。
/// 取得スレッドから UI スレッドへ、この形でチャネル越しに返す
type CapabilityResult = (String, Result<video::DeviceCapabilities, String>);

/// スクリーンショットの出力結果。`(撮影を始めた時刻, 何をしたか / 失敗なら理由)`。
///
/// 保存スレッドから UI スレッドへ、この形でチャネル越しに返す。
/// **失敗だけでなく成功も送る。** 成功で直近の失敗の記録を消さないと、
/// 一度失敗したあとは設定画面に古い失敗が残り続ける。
///
/// **撮影時刻を添えるのは、結果が撮影順に届くとは限らないため。** 保存は
/// 撮影ごとにスレッドを起動するので、先に始めた保存が後から終わりうる。
/// 古い結果で新しい記録を上書きしないよう、受け取る側が時刻で弾く
type ScreenshotResult = (Instant, ScreenshotOutcome);

/// 1 回の撮影の結末。成功なら何をしたかの文、失敗なら理由。
///
/// **出力先ごとの内訳ではなく、文字列 1 つに畳んである。** 受け取る UI
/// スレッドがすることは「ログに出す」と「失敗ならトーストに出す」だけで、
/// 出力先の種類で処理を分けないため。畳む規則は
/// `summarize_screenshot_delivery` が持つ。
type ScreenshotOutcome = Result<String, String>;

/// 出力先が「両方」のときに、片方だけ失敗した場合の結果の作り方。
///
/// 失敗が 1 つでもあれば全体を失敗として扱い、理由を並べる。成功したほうを
/// 黙って捨てないよう、文言には成功した出力先も残す。
///
/// 引数の `None` は「その出力先が設定に含まれていない」を表す。`Some` は
/// 実際に試した結果。
///
/// アプリの状態に触れないのでそのまま別スレッドで実行でき、テストからも呼べる。
fn summarize_screenshot_delivery(
    clipboard: Option<Result<(), String>>,
    file: Option<Result<PathBuf, String>>,
) -> ScreenshotOutcome {
    let (copied, clipboard_error) = match clipboard {
        Some(Ok(())) => (true, None),
        Some(Err(reason)) => (false, Some(reason)),
        None => (false, None),
    };
    let (saved_to, file_error) = match file {
        Some(Ok(path)) => (Some(path), None),
        Some(Err(reason)) => (None, Some(reason)),
        None => (None, None),
    };

    let succeeded = match (copied, &saved_to) {
        (true, Some(path)) => Some(format!(
            "クリップボードへコピーし、{} へ保存した",
            path.display()
        )),
        (true, None) => Some("クリップボードへコピーした".to_string()),
        (false, Some(path)) => Some(format!("{} へ保存した", path.display())),
        (false, None) => None,
    };

    let failures: Vec<String> = [clipboard_error, file_error]
        .into_iter()
        .flatten()
        .collect();
    if !failures.is_empty() {
        let mut reason = failures.join(" / ");
        // 片方だけ失敗した場合に、成功したほうを黙って捨てない。
        // 「クリップボードには入っているのか」が分からないと次の操作を選べない
        if let Some(done) = succeeded {
            reason = format!("{}（{}）", reason, done);
        }
        return Err(reason);
    }

    // 出力先の enum が必ずどちらかを含むので通常は起きない。
    // 黙って成功にすると、何も出力していないのに撮れたように見える
    succeeded.ok_or_else(|| "出力先が 1 つも設定されていません".to_string())
}

/// 届いた結果を画面の記録へ反映してよいかを判定する。
///
/// `last` は画面の記録へ反映済みの中で最も新しい撮影の開始時刻で、`None` は
/// 「まだ何も反映していない」を表す。`started_at` は届いた結果の撮影時刻。
///
/// **同時刻は反映する側に倒す。** `Instant` は単調増加するので、別の撮影が
/// まったく同じ時刻になることは通常起きないが、起きたとしても取りこぼす
/// より出すほうがよい。
fn screenshot_outcome_supersedes(last: Option<Instant>, started_at: Instant) -> bool {
    match last {
        None => true,
        Some(last) => started_at >= last,
    }
}

/// エラーをトーストで見せておく時間。
///
/// 音量 OSD（1.5 秒）より長い。音量は自分で操作した結果の確認なので一瞬で
/// よいが、エラーは予期していない内容を読ませるため。
const ERROR_TOAST_DURATION: Duration = Duration::from_secs(4);

/// 接続に失敗したあと、最初に待つ時間。
///
/// 実測ではデバイスの列挙が 1〜2ms、`Camera::new` が 28〜90ms なので、
/// 1 回目の再試行を 200ms 後に置いても取りこぼしはほぼ無い。
const CONNECT_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// 再試行の間隔の上限。
///
/// 無限に再試行するので、間隔を伸ばし続けると「後からデバイスを挿した」
/// ときの反応が悪くなる。5 秒で頭打ちにして、挿してから最大 5 秒で繋がるようにする。
const CONNECT_BACKOFF_MAX: Duration = Duration::from_millis(5000);

/// 音声で、この回数だけ連続して失敗したあとに既定のデバイスを試す。
///
/// 設定に残っているデバイス名が古くて存在しない場合、そのまま待ち続けても
/// 永久に音が出ない。元の実装と同じ 3 回目に合わせてある。
const AUDIO_DEFAULT_FALLBACK_AFTER: u32 = 3;

/// フレームが途絶えてから「映像が切れた」と判断するまでの時間。
///
/// 60fps なら 1 枚あたり 16ms、30fps でも 33ms なので、3 秒は 100 枚近い
/// 欠落にあたる。一時的なコマ落ちで表示が消えない程度に長く、ユーザーが
/// 「固まった」と気付くより先に反応する程度に短い値として置いている。
const VIDEO_SIGNAL_TIMEOUT: Duration = Duration::from_secs(3);

/// ストリームのエラーを理由に音声を開き直すときの、最短の間隔。
///
/// 開いた直後に必ず落ちるデバイスでは、エラー → 開き直し → エラーの繰り返しに
/// なる。音声を開く処理は実測で 300ms 前後かかり、その間 UI スレッドが止まる
/// ため、下限を置いて毎フレーム開き直さないようにする。
const AUDIO_ERROR_RECONNECT_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// 連続 `attempt` 回失敗したあとに待つ時間を返す。
///
/// `CONNECT_BACKOFF_BASE` から倍々に伸ばし、`CONNECT_BACKOFF_MAX` で頭打ちにする。
/// 200ms → 400 → 800 → 1600 → 3200 → 5000ms（以降は 5000ms のまま）。
///
/// `attempt` は失敗が続く限り際限なく増えるため、シフトでは桁あふれを起こす。
/// 頭打ちに達する回数で先に打ち切って、パニックしないようにしてある。
fn backoff_delay(attempt: u32) -> Duration {
    let Some(shift) = attempt.checked_sub(1) else {
        // まだ 1 度も失敗していない。待たずに試す
        return Duration::ZERO;
    };
    // 1u32 << 32 は未定義。頭打ちには 6 回目で届くので、ここへ来た時点で上限でよい
    if shift >= u32::BITS {
        return CONNECT_BACKOFF_MAX;
    }
    match CONNECT_BACKOFF_BASE.checked_mul(1u32 << shift) {
        Some(delay) if delay < CONNECT_BACKOFF_MAX => delay,
        _ => CONNECT_BACKOFF_MAX,
    }
}

/// 次に試してよい時刻が来ているかを判定する。
///
/// `None` は「期限が無い＝いますぐ試してよい」を表す。境界（期限ちょうど）では
/// 試す側に倒す。1 フレーム遅らせても得るものが無いため。
fn should_retry_now(next_attempt_at: Option<Instant>, now: Instant) -> bool {
    match next_attempt_at {
        None => true,
        Some(deadline) => now >= deadline,
    }
}

/// フレームの途絶を見たあと、そのフレームで何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoLinkAction {
    /// 何もしない。フレームが流れている、まだ 1 枚も届いていない、
    /// またはストリームを開けていない
    Keep,
    /// 表示中のテクスチャを捨てて「映像信号がありません」に戻す
    ClearTexture,
    /// テクスチャを捨てたうえで、ストリームを閉じて開き直す
    ClearTextureAndReconnect,
}

/// 映像が途絶えたかを判定する。
///
/// 判定をここへ切り出してあるのは、実機でしか作れない状況（USB を抜く、
/// 入力信号を落とす）をテストで代替するため。時計もデバイスも触らない。
///
/// - **ストリームを開けていない場合は何もしない。** 接続は `ConnectRetry` の
///   担当で、ここが二重に面倒を見ると起動時の接続と競合する
/// - **1 枚も届いていない場合も何もしない。** 開けた直後は 1 枚目まで実測で
///   0.8 秒かかるうえ、入力信号が無いデバイスは開けても永久にフレームを
///   出さない。ここで切断と見なすと、開き直しを延々と繰り返すことになる
/// - 期限ちょうどは切断とみなす側に倒す。1 フレーム待って得るものが無いため
fn decide_video_link(
    state: video::VideoLinkState,
    auto_reconnect: bool,
    timeout: Duration,
) -> VideoLinkAction {
    if !state.capturing {
        return VideoLinkAction::Keep;
    }
    let Some(elapsed) = state.since_last_frame else {
        return VideoLinkAction::Keep;
    };
    if elapsed < timeout {
        return VideoLinkAction::Keep;
    }
    if auto_reconnect {
        VideoLinkAction::ClearTextureAndReconnect
    } else {
        VideoLinkAction::ClearTexture
    }
}

/// ストリームのエラーを理由に、いま音声を開き直してよいかを判定する。
///
/// `since_last_reconnect` は前回この理由で開き直してからの経過時間で、
/// `None` は「まだ一度も開き直していない」を表す。
fn should_reconnect_after_stream_error(since_last_reconnect: Option<Duration>) -> bool {
    match since_last_reconnect {
        None => true,
        Some(elapsed) => elapsed >= AUDIO_ERROR_RECONNECT_MIN_INTERVAL,
    }
}

/// 保留中の音声ストリームのエラーに対して、そのフレームで何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioErrorAction {
    /// 何もしない。保留しているエラーが無い
    Idle,
    /// 保留したまま待つ。自動再接続が無効か、開き直しの下限に達していない
    Wait,
    /// ストリームを閉じて開き直す
    Reconnect,
}

/// 保留中の音声エラーの扱いを決める。
///
/// **見送るときも保留を落とさない（`Wait` で持ち越す）。** エラーの通知は
/// `take_stream_error` が読んだ時点で消えるため、ここで捨てると誰も
/// 開き直さないまま音が戻らなくなる。自動再接続を有効にし直したとき、
/// または下限に達したときのフレームで `Reconnect` に変わる。
fn decide_audio_reconnect(
    error_pending: bool,
    auto_reconnect: bool,
    since_last_reconnect: Option<Duration>,
) -> AudioErrorAction {
    if !error_pending {
        return AudioErrorAction::Idle;
    }
    if !auto_reconnect {
        return AudioErrorAction::Wait;
    }
    if !should_reconnect_after_stream_error(since_last_reconnect) {
        return AudioErrorAction::Wait;
    }
    AudioErrorAction::Reconnect
}

/// 映像が出ていないときに画面へ出す文言を決める。
///
/// 「デバイスは開けているが信号が来ていない」と「デバイスそのものが消えた」は
/// ユーザーの取るべき行動が違う（入力機器の電源を見るのか、ケーブルを挿し直すのか）
/// ため、同じ文言にしない。
fn video_placeholder_message(capturing: bool, reconnecting: bool) -> &'static str {
    match (capturing, reconnecting) {
        (true, _) => "映像信号がありません",
        (false, true) => "デバイスが接続されていません（再接続を試しています）",
        (false, false) => "デバイスが接続されていません",
    }
}

/// 映像が出ていないときに画面へ出す文言を、理由の 1 行を添えて組み立てる。
///
/// `detail` は直近の失敗（`ErrorCenter` に記録されたもの）。**ストリームを
/// 開けている場合は添えない。** 映像信号が来ていないのはデバイスの手前の
/// 問題で、そこに古い接続エラーを出すと原因を取り違えさせる。
///
/// 理由の切り詰めは呼び出し側（`error_detail`）が済ませてある。ここで
/// 長さを見ないのは、切り詰めの基準を 1 か所に集めておくため。
fn video_placeholder_text(capturing: bool, reconnecting: bool, detail: Option<&str>) -> String {
    let head = video_placeholder_message(capturing, reconnecting);
    match detail {
        Some(detail) if !capturing => format!("{}\n{}", head, detail),
        _ => head.to_string(),
    }
}

/// 映像の接続対象。これが変わったらバックオフを捨てて即座に開き直す。
/// `(デバイス名, 解像度, フォーマット, fps)`
type VideoTarget = (
    Option<String>,
    Option<(u32, u32)>,
    Option<String>,
    Option<u32>,
);

/// 音声の接続対象。`(入力デバイス名, 出力デバイス名, サンプリングレート, チャンネル数)`
type AudioTarget = (Option<String>, Option<String>, Option<u32>, Option<u16>);

/// 設定から映像の接続対象を取り出す。
fn video_target(settings: &AppSettings) -> VideoTarget {
    (
        settings.video.device_name.clone(),
        settings.video.resolution,
        settings.video.format.clone(),
        settings.video.fps,
    )
}

/// 設定から音声の接続対象を取り出す。
fn audio_target(settings: &AppSettings) -> AudioTarget {
    (
        settings.audio.input_device_name.clone(),
        settings.audio.output_device_name.clone(),
        settings.audio.sample_rate,
        settings.audio.channels,
    )
}

/// 登録できなかったホットキーを、通知 1 件ぶんの文字列にまとめる。
/// すべて登録できていれば `None`。
///
/// 定型文（「ホットキーを登録できません」）は `status::format_message` が
/// 前に付けるので、ここでは付けない。どのアクションのどのキーが駄目だったかを
/// 並べるところまでを受け持つ。
fn hotkey_error_summary(errors: &BTreeMap<HotkeyAction, HotkeyError>) -> Option<String> {
    if errors.is_empty() {
        return None;
    }

    let detail = errors
        .iter()
        .map(|(action, error)| format!("{}（{}）: {}", action.label(), error.hotkey, error.message))
        .collect::<Vec<_>>()
        .join(" / ");
    Some(detail)
}

/// デバイス接続の再試行を、UI スレッドを止めずに回すための状態。
///
/// `update()` から毎フレーム `is_due()` を見て、期限が来ていれば 1 回だけ試す。
/// **`thread::sleep` を使わない。** 待つ代わりに次に試してよい時刻を覚えておく。
/// 以前は UI スレッドで最大 3 秒眠っていたため、接続に失敗する環境では
/// その間ウィンドウが固まっていた。
///
/// `T` は「いま何へ繋ごうとしているか」を表す値（デバイス名や解像度の組）。
/// 2 秒ごとの設定の再適用は同じ対象を何度も要求してくるため、対象が同じなら
/// 進行中のバックオフを維持する。これをしないと待ち時間が毎回巻き戻り、
/// 繋がらないデバイスへ 2 秒間に 4 回も接続を試みることになる。
#[derive(Debug)]
struct ConnectRetry<T> {
    /// いま繋ごうとしている対象。`None` は「接続を要求されていない」
    target: Option<T>,
    /// 連続して失敗した回数。成功と、対象が変わったときに 0 へ戻る
    attempts: u32,
    /// 次に試してよい時刻。`None` は「いますぐ試してよい」
    next_attempt_at: Option<Instant>,
}

impl<T> Default for ConnectRetry<T> {
    fn default() -> Self {
        Self {
            target: None,
            attempts: 0,
            next_attempt_at: None,
        }
    }
}

impl<T: PartialEq> ConnectRetry<T> {
    /// 接続を要求する。
    ///
    /// **同じ対象を既に追いかけている場合は何もしない。** 2 秒ごとの設定の
    /// 再適用がここを通るため、毎回やり直すとバックオフが伸びなくなる。
    /// 対象が変わった場合（設定画面でデバイスを選び直した等）は数え直して
    /// 即座に試す。ユーザーの操作に対して最大 5 秒待たせる理由が無いため。
    fn request(&mut self, target: T) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        self.request_now(target);
    }

    /// 対象が同じでもバックオフを捨てて即座に試す。
    ///
    /// 右クリックメニューの「デバイス再接続」のように、ユーザーが明示的に
    /// やり直しを求めた場合に使う。
    fn request_now(&mut self, target: T) {
        self.target = Some(target);
        self.attempts = 0;
        self.next_attempt_at = None;
    }

    /// 接続の要求を取り下げる。繋ぐ相手が無い（デバイス名が未設定）ときに使う。
    fn cancel(&mut self) {
        self.target = None;
        self.next_attempt_at = None;
    }

    /// このフレームで接続を試してよいか。
    fn is_due(&self, now: Instant) -> bool {
        self.target.is_some() && should_retry_now(self.next_attempt_at, now)
    }

    /// 接続を追いかけている最中か。繋がると `false` に戻る。
    /// 「再接続を試しています」という表示の出し分けに使う
    fn is_active(&self) -> bool {
        self.target.is_some()
    }

    /// 連続して失敗した回数。
    fn attempts(&self) -> u32 {
        self.attempts
    }

    /// 成功を記録する。以降は要求があるまで試さない。
    fn record_success(&mut self) {
        self.target = None;
        self.attempts = 0;
        self.next_attempt_at = None;
    }

    /// 失敗を記録し、次に試してよい時刻を決める。
    ///
    /// 対象は保持したままにする。繋がるまで無限に再試行し、後からデバイスを
    /// 挿した場合に何もしなくても繋がるようにするため。
    ///
    /// **失敗の理由はここでは持たない。** 画面へ出す記録は `ErrorCenter` が
    /// 発生源ごとにまとめて持っており、両方に置くと消し忘れた片方が古い
    /// 理由を出し続ける。
    fn record_failure(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.next_attempt_at = now.checked_add(backoff_delay(self.attempts));
    }
}

pub struct CaptureCardViewer {
    settings: Arc<Mutex<AppSettings>>,
    video_capture: Arc<Mutex<VideoCapture>>,
    audio_capture: Arc<Mutex<AudioCapture>>,
    screenshot_manager: Arc<Mutex<ScreenshotManager>>,
    // グローバルホットキーの登録と押下の検出。
    //
    // **`Arc<Mutex<..>>` にしていない。** 触るのは UI スレッドだけで、
    // リスナースレッドと共有するのは `HotkeyManager` の内部にある
    // 共有状態（登録中の ID と押下の記録）だけのため
    hotkey_manager: HotkeyManager,

    // デバイス能力の取得結果を受け取るチャネル。
    // 取得はデバイスを開く重い処理なので使い捨てのスレッドへ投げ、
    // UI スレッドは update() で try_recv するだけにする
    capability_tx: Sender<CapabilityResult>,
    capability_rx: Receiver<CapabilityResult>,

    // スクリーンショットの保存結果を受け取るチャネル。
    // 保存は別スレッドで行うため、失敗をその場で画面に出せない。
    // 能力取得と同じく、UI スレッドが update() で try_recv するだけにする
    screenshot_tx: Sender<ScreenshotResult>,
    screenshot_rx: Receiver<ScreenshotResult>,

    // 発生源ごとの直近の失敗。トーストの間引きもここが判断する
    errors: ErrorCenter,
    // 画面の記録へ反映した中で、最も新しい撮影の開始時刻。
    // 保存スレッドの結果は撮影順に届くとは限らないため、これより古い結果は
    // ログだけ残して記録には触らない
    last_screenshot_outcome_at: Option<Instant>,

    // UI状態管理
    show_settings: bool,
    // 設定ダイアログのドラフト。共有設定を直接書き換えないための置き場所
    settings_dialog: ui::SettingsDialogState,
    show_context_menu: bool,
    show_hotkey_dialog: bool,
    context_menu_pos: egui::Pos2,
    is_fullscreen: bool,
    maintain_aspect_ratio: bool,
    // 映像に統計を重ねて表示するか。設定の ui.show_stats_overlay と対応する
    show_stats_overlay: bool,
    // フルスクリーン切替や音量変更のときだけ出て、数秒で消えるオーバーレイ。
    // 常時表示の show_stats_overlay とは別物
    transient_overlay: TransientOverlay,
    volume: f32,
    // ミュート中か。設定の ui.muted と対応する。音量とは独立で、
    // ミュート中も volume は元の値を保つ
    muted: bool,
    last_volume_sent: f32,
    last_settings_applied: Instant,
    // 設定に未保存の変更があるときの、最後に変更された時刻。
    // None は保留中の変更が無いことを表す
    settings_dirty_since: Option<Instant>,
    // 自動保存（デバウンス保存と終了時保存）を許してよいか。
    // 読めなかった設定ファイルを退避できなかった場合は止める
    autosave: AutoSavePolicy,

    // 映像表示関連
    video_texture: Option<egui::TextureHandle>,
    // テクスチャへ反映済みのフレーム世代。新着が無いフレームでは更新をまるごと省く
    last_frame_generation: u64,
    // フレームの途絶に対して最後に行った処置。
    // 毎フレーム同じ判定に当たるため、同じ処置を繰り返さないための番人。
    // 判定が変わったとき（自動再接続を有効にし直したとき）は動けるように、
    // 真偽値ではなく「何をしたか」で持つ。新しいフレームが届いた時点で
    // `Keep` へ戻す
    last_video_link_action: VideoLinkAction,
    // 直近に観測した「映像ストリームを開けているか」。
    // 描画のたびに video_capture のロックを取らずに済ませるため、
    // 毎フレームの監視で拾った値をここに写しておく
    video_capturing: bool,
    // ストリームのエラーを理由に音声を開き直した時刻。
    // 開いた直後に必ず落ちるデバイスで、毎フレーム開き直さないための下限
    last_audio_error_reconnect: Option<Instant>,
    // 未処理の音声ストリームのエラーがあるか。
    // `take_stream_error` は読んだ時点で旗を下ろすため、見送ったエラーを
    // ここへ移しておかないと、そのまま音が戻らなくなる
    audio_stream_error_pending: bool,
    // ホットキー入力ダイアログで編集中の内容。
    // 確定した文字列で、まだ設定（ドラフトまたは共有設定）へ書いていないもの
    temp_hotkey: String,
    // `temp_hotkey` がどのアクションのものか。
    //
    // 入力ダイアログはモーダルではないので、開いたまま一覧の別の行の
    // 「設定...」を押せる。編集対象が変わったことをここで検出して
    // `temp_hotkey` を捨てないと、前のアクションのキーが残ったまま確定する
    temp_hotkey_action: Option<HotkeyAction>,
    // 最後に適用した実行時パラメータ（差分ベースの再起動回避用）
    last_video_device: Option<String>,
    last_video_res: Option<(u32, u32)>,
    last_video_format: Option<String>,
    last_audio_device: Option<String>,
    last_audio_output: Option<String>,
    last_audio_rate: Option<u32>,
    last_audio_channels: Option<u16>,
    last_video_fps: Option<u32>,
    // 最後に VideoCapture へ渡した色変換の設定。
    // キャプチャの開き直しは伴わないが、2 秒ごとに video_capture の
    // ロックを取らずに済むよう、他と同じく差分で判定する
    last_color_conversion: Option<(ColorSpace, ColorRange)>,
    // 最後に適用したスクリーンショットの効果音。
    // apply_settings が 2 秒ごとに呼ばれるため、差分がないときは再適用しない。
    //
    // **設定と同じ `Option` を丸ごと包んでいる。** 外側の `None` は
    // 「まだ適用できていない（次の適用でやり直す）」、内側の `None` は
    // 「未設定を適用済み＝クリア済み」を表す。内側を潰して `Option<PathBuf>` に
    // すると、クリア（設定が `None`）と未適用が同じ値になり、クリアを
    // 差分として検出できない。
    //
    // ホットキーに同じ仕組みが要らないのは、`HotkeyManager::apply` が
    // 自分で差分を取るため
    last_sound_file: Option<Option<PathBuf>>,

    // デバイス接続の再試行。映像と音声で別々に持ち、片方が失敗しても
    // もう片方の再試行に引きずられないようにする
    video_retry: ConnectRetry<VideoTarget>,
    audio_retry: ConnectRetry<AudioTarget>,

    // 起動直後に 1 度だけ行う処理を済ませたか
    startup_applied: bool,

    // UI性能向上のためのデバイスリストキャッシュ
    // ビデオは (デバイス名, 説明) の組
    cached_video_devices: Vec<(String, String)>,
    cached_input_devices: Vec<String>,
    cached_output_devices: Vec<String>,
    last_device_list_update: Option<Instant>,

    // ウィンドウ管理
    always_on_top: bool,
    // タイトルバーと枠を消しているか。設定の ui.borderless と対応する。
    // フルスクリーン中は OS 側が元から装飾を外しているので、この値は
    // 「フルスクリーンから戻ったときにどちらへ戻すか」を保持しているだけになる
    borderless: bool,

    // 進行中のスクリーンショット保存スレッド。クリップボードへの転送も
    // このスレッドが行う。
    // 終了時に join して、書き出し途中の画像ファイルが残らないようにする
    screenshot_save_threads: Vec<JoinHandle<()>>,
}

impl Default for CaptureCardViewer {
    fn default() -> Self {
        let (loaded_settings, load_outcome) = AppSettings::load();
        // 表示状態は apply_settings を待たずに反映する。
        // apply_settings は起動から 2 秒後が最初なので、待つと
        // オンで終了したのに起動直後だけ出ていない、という見え方になる
        let show_stats_overlay = loaded_settings.ui.show_stats_overlay;
        let settings = Arc::new(Mutex::new(loaded_settings));
        let video_capture = Arc::new(Mutex::new(VideoCapture::new()));
        #[allow(clippy::arc_with_non_send_sync)] // 音声キャプチャは非同期処理で必要
        let audio_capture = Arc::new(Mutex::new(AudioCapture::new()));
        let screenshot_manager = Arc::new(Mutex::new(ScreenshotManager::new()));
        let (capability_tx, capability_rx) = std::sync::mpsc::channel();
        let (screenshot_tx, screenshot_rx) = std::sync::mpsc::channel();

        let mut app = Self {
            settings,
            video_capture,
            audio_capture,
            screenshot_manager,
            hotkey_manager: HotkeyManager::new(),
            capability_tx,
            capability_rx,
            screenshot_tx,
            screenshot_rx,
            errors: ErrorCenter::default(),
            last_screenshot_outcome_at: None,
            show_settings: false,
            settings_dialog: ui::SettingsDialogState::default(),
            show_context_menu: false,
            show_hotkey_dialog: false,
            context_menu_pos: egui::Pos2::ZERO,
            is_fullscreen: false,
            maintain_aspect_ratio: true,
            show_stats_overlay,
            transient_overlay: TransientOverlay::default(),
            volume: 100.0,
            muted: false,
            last_volume_sent: -1.0,
            last_settings_applied: Instant::now(),
            settings_dirty_since: None,
            autosave: AutoSavePolicy::from_load_outcome(load_outcome),
            video_texture: None,
            last_frame_generation: 0,
            last_video_link_action: VideoLinkAction::Keep,
            video_capturing: false,
            last_audio_error_reconnect: None,
            audio_stream_error_pending: false,
            temp_hotkey: String::new(),
            temp_hotkey_action: None,
            last_video_device: None,
            last_video_res: None,
            last_video_format: None,
            last_audio_device: None,
            last_audio_output: None,
            last_audio_rate: None,
            last_audio_channels: None,
            last_video_fps: None,
            last_color_conversion: None,
            last_sound_file: None,

            video_retry: ConnectRetry::default(),
            audio_retry: ConnectRetry::default(),
            startup_applied: false,

            // UI性能向上のためのデバイスリストキャッシュ
            cached_video_devices: Vec::new(),
            cached_input_devices: Vec::new(),
            cached_output_devices: Vec::new(),
            last_device_list_update: None,

            // ウィンドウ管理
            always_on_top: false,
            borderless: false,

            screenshot_save_threads: Vec::new(),
        };

        // 保存されたデバイスがない場合は自動選択
        {
            if let Ok(mut s) = app.settings.lock() {
                if s.video.device_name.is_none() {
                    let devices = VideoCapture::list_devices();
                    if let Some((name, _)) = devices.first() {
                        s.video.device_name = Some(name.clone());
                    }
                }
                if s.audio.input_device_name.is_none() {
                    let ac = AudioCapture::new();
                    let list = ac.list_input_devices();
                    debug!("利用できる入力デバイス: {:?}", list);
                    if let Some(name) = list.first() {
                        s.audio.input_device_name = Some(name.clone());
                        info!("入力デバイスの既定を {} にした", name);
                    }
                }
                if s.audio.output_device_name.is_none() {
                    // 出力デバイスはデフォルト（None）で自動選択させる
                    s.audio.output_device_name = None;
                    debug!("出力デバイスは既定（自動選択）にする");
                }
                // タイトルバーなしで保存されているのに画面ドラッグ移動が切れている
                // 場合を直す。右クリックメニューからの切替では set_borderless が
                // 同じ判定で守っているが、設定ファイルは手で編集できるため、
                // 起動した時点で動かせないウィンドウが出てくる組み合わせを作れる
                if needs_drag_move_guard(s.ui.borderless, s.ui.enable_drag_move) {
                    s.ui.enable_drag_move = true;
                    warn!(
                        "タイトルバーなしで画面ドラッグ移動が無効だったため、ドラッグ移動を有効にした"
                    );
                }
                // 読めなかった設定ファイルを退避できなかった場合は書き戻さない。
                // ここで上書きすると、ディスクに残っている壊れたファイルが既定値で
                // 潰れ、ユーザーが設定を取り戻す最後の手段が消える。
                if load_outcome.may_write_defaults_on_startup() {
                    s.save();
                } else {
                    warn!(
                        "読めなかった設定ファイルが残っているため、設定の自動保存を止める。設定画面の「適用」か「OK」で保存すると再開する"
                    );
                }
            }
        }

        // 保存済みのビデオデバイスの能力を先に取りに行く。
        // 設定画面を開いた時点で選択肢が揃っているようにするためで、
        // 以前はデバイスを切り替えたときしか取得していなかったため、
        // 起動後に設定画面を開いても解像度や FPS の選択肢が出なかった。
        // 接続と並行して走るので、設定画面を開く頃には揃っている
        let saved_video_device = app
            .settings
            .lock()
            .ok()
            .and_then(|s| s.video.device_name.clone());
        if let Some(device) = saved_video_device {
            app.settings_dialog.capabilities_mut().request(&device);
            app.dispatch_capability_requests();
        }

        // 注: デバイスの接続は最初の update() で始まり、失敗したら
        // ConnectRetry のバックオフで繋がるまで再試行する
        app
    }
}

impl eframe::App for CaptureCardViewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 別スレッドで取得したデバイス能力を取り込む。
        // 設定ダイアログを開いていなくても受け取る（起動時の先読み分があるため）
        self.drain_capability_results();

        // 別スレッドで行ったスクリーンショットの保存結果を取り込む。
        // 失敗はここでトーストになる
        self.drain_screenshot_results();

        // 起動直後に 1 度だけ行う処理。
        //
        // 以前はここで「起動から 2 秒」待ってからデバイスを開いていた。
        // 待つ根拠がコードにもコミットにも残っておらず、実測でも接続自体は
        // 0.1 秒で終わるため、最初のフレームで始める。開けなかった場合は
        // ConnectRetry のバックオフが繋がるまで面倒を見る
        if !self.startup_applied {
            self.startup_applied = true;
            info!("起動直後の設定適用とデバイスの接続を始める");
            self.apply_settings(true);

            // ウィンドウレベルは always_on_top を設定から取り込んだあとに適用する。
            // 順序を入れ替えると、既定値の false で 1 度適用されてしまう
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.always_on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }));
        }

        // 期限が来ているデバイスの接続を 1 回だけ試す。
        // 繋がっている間は bool を 2 つ見るだけで抜ける
        self.poll_device_connection();

        // ビデオフレームを更新
        self.update_video_texture(ctx);

        // フレームの途絶と音声ストリームのエラーを見て、必要なら開き直しを要求する。
        // 実際に開くのは次のフレームの poll_device_connection
        self.monitor_device_health();

        // グローバルホットキーを処理
        self.handle_hotkeys(ctx);

        // 定期的に実行時設定が保存設定と一致することを確認（外部変更に対応）
        if self.last_settings_applied.elapsed().as_secs_f32() > 2.0 {
            self.apply_settings(false);
        }

        // 音量が変更された場合、オーディオバックエンドに伝播
        if (self.volume - self.last_volume_sent).abs() > 0.5 {
            if let Ok(mut audio) = self.audio_capture.lock() {
                audio.set_volume(self.volume);
            }
            self.last_volume_sent = self.volume;
        }

        // ウィンドウサイズと位置を監視して設定に保存
        let viewport = ctx.input(|i| i.viewport().clone());
        let current_size = viewport.inner_rect.map(|r| (r.width(), r.height()));
        let current_pos = viewport.outer_rect.map(|r| (r.left(), r.top()));

        // サイズまたは位置が変更された場合、設定を更新。
        // フルスクリーン中は画面全体の矩形しか取れないため記録しない。
        // こうすることで、フルスクリーンへ入る直前のジオメトリが設定に残り、
        // フルスクリーンのまま終了しても次回はウィンドウ表示で復元される
        let mut window_geometry_changed = false;
        if Self::should_record_window_geometry(self.is_fullscreen, viewport.fullscreen) {
            if let Ok(mut settings) = self.settings.lock() {
                let mut changed = false;

                if let Some((width, height)) = current_size {
                    if settings.ui.last_window_size != Some((width, height)) {
                        settings.ui.last_window_size = Some((width, height));
                        changed = true;
                    }
                }

                if let Some((x, y)) = current_pos {
                    if settings.ui.last_window_pos != Some((x, y)) {
                        settings.ui.last_window_pos = Some((x, y));
                        changed = true;
                    }
                }

                window_geometry_changed = changed;
            }
        }

        // ここでは書き出さない。ウィンドウのドラッグ中は毎フレーム値が変わるため、
        // 変わるたびに保存すると最大 60 回/秒のディスク書き込みになる
        if window_geometry_changed {
            self.mark_settings_dirty();
        }

        // メインUI
        // F11によるフルスクリーン切り替えを削除（スクリーンショット用に解放）

        if self.is_fullscreen {
            self.show_fullscreen_ui(ctx);
        } else {
            self.show_windowed_ui(ctx);
        }

        // 統計オーバーレイ。ウィンドウ表示とフルスクリーンで同じものを出すため、
        // どちらの描画のあとでもここで 1 回だけ描く
        if self.show_stats_overlay {
            self.show_stats_overlay(ctx);
        }

        // 設定ダイアログ
        if self.show_settings {
            // 開いた最初のフレームで、実行中の設定からドラフトを作る
            if !self.settings_dialog.has_draft() {
                if let Ok(settings) = self.settings.lock() {
                    self.settings_dialog.begin_edit(&settings);
                }
            }

            let video_devices = self.get_cached_video_devices().clone();
            let input_devices = self.get_cached_input_devices().clone();
            let output_devices = self.get_cached_output_devices().clone();
            let devices = ui::DeviceLists {
                video: &video_devices,
                input: &input_devices,
                output: &output_devices,
            };
            // 接続状態はダイアログを開いている間だけ集める
            let connection = self.connection_status();
            let action = ui::show_settings_dialog(
                ctx,
                &mut self.show_settings,
                &mut self.settings_dialog,
                &mut self.show_hotkey_dialog,
                &devices,
                &connection,
                self.hotkey_manager.errors(),
            );
            self.handle_settings_dialog_action(action);

            // ダイアログが積んだ取得要求を別スレッドへ渡す
            self.dispatch_capability_requests();
        }

        // ホットキーキャプチャダイアログ
        if self.show_hotkey_dialog {
            // どのアクションを編集しているかは、一覧の「設定...」が
            // HotkeyCaptureState へ入れている。他に開く経路が無いので通常は
            // 必ず入っているが、取れない場合もダイアログを無反応にせず
            // スクリーンショットとして扱う
            let action = self
                .settings_dialog
                .hotkey_capture()
                .editing()
                .unwrap_or(HotkeyAction::Screenshot);

            // 編集対象が変わったら、前のアクションで見せていた値を捨てる。
            //
            // ホットキー入力ダイアログはモーダルではないため、開いたまま
            // 一覧の別の行の「設定...」を押せる。捨てないと、前のアクションの
            // キーが表示に残ったまま OK で確定し、押した覚えのないキーが
            // 新しいアクションへ入る
            if self.temp_hotkey_action != Some(action) {
                self.temp_hotkey_action = Some(action);
                self.temp_hotkey.clear();
            }

            // ダイアログが開かれた時に現在の設定値をtemp_hotkeyに設定。
            // 設定ダイアログから開かれた場合は、編集中のドラフトの値を見せる
            if self.temp_hotkey.is_empty() {
                let current = match self.settings_dialog.draft() {
                    Some(draft) => draft.hotkey(action).map(str::to_string),
                    None => self
                        .settings
                        .lock()
                        .ok()
                        .and_then(|settings| settings.hotkey(action).map(str::to_string)),
                };
                self.temp_hotkey = current.unwrap_or_default();
            }

            let outcome = ui::show_hotkey_capture_dialog(
                ctx,
                &mut self.show_hotkey_dialog,
                action,
                &mut self.temp_hotkey,
                self.settings_dialog.hotkey_capture_mut(),
            );

            // 確定またはクリアされた場合、設定を更新。
            // クリアは「ホットキーを使わない」という明示の指定なので、
            // 確定と同じ経路で `None` を書き込む
            let new_hotkey = match outcome {
                ui::HotkeyDialogOutcome::None => None,
                ui::HotkeyDialogOutcome::Captured if self.temp_hotkey.is_empty() => None,
                ui::HotkeyDialogOutcome::Captured => Some(Some(self.temp_hotkey.clone())),
                ui::HotkeyDialogOutcome::Cleared => Some(None),
            };

            if let Some(hotkey) = new_hotkey {
                // 設定ダイアログから開かれている場合はドラフトへ書く。
                // 共有設定へ直接書くと、ダイアログの OK がドラフトの古い値で
                // 上書きして、設定したホットキーが消える
                let wrote_to_draft = match self.settings_dialog.draft_mut() {
                    Some(draft) => {
                        draft.set_hotkey(action, hotkey.clone());
                        true
                    }
                    None => false,
                };

                if !wrote_to_draft {
                    // 設定ダイアログが閉じられた状態でホットキーだけ確定した場合。
                    // ドラフトが無いので共有設定へ直接書き、その場で登録（解除）する
                    if let Ok(mut settings) = self.settings.lock() {
                        settings.set_hotkey(action, hotkey.clone());
                    }
                    self.mark_settings_dirty();
                    self.apply_hotkeys_now();
                }
                // ドラフトへ書いた場合はここで登録しない。
                // 登録すると、2 秒ごとの apply_settings が共有設定側の古い
                // ホットキーを見て登録し直し、「適用」も押していないのに
                // 効いたり戻ったりする。実際の登録は「適用」か「OK」で行う
            }

            // ダイアログが閉じられた時にtemp_hotkeyをクリア
            if !self.show_hotkey_dialog {
                self.temp_hotkey.clear();
                self.temp_hotkey_action = None;
            }
        }

        // コンテキストメニュー
        if self.show_context_menu {
            self.show_context_menu(ctx);
        }

        // 一時表示のオーバーレイ（フルスクリーン切替・音量）。
        // 期限が来れば自分で消え、消える時刻の再描画も自分で予約する
        self.transient_overlay.draw(ctx, Instant::now());

        // 保留中の設定変更を、操作が落ち着いたところでまとめて書き出す
        self.flush_settings_if_due(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 終了時は書き出す。デバウンスの待ち時間中に終了しても、
        // ウィンドウのサイズ・位置や音量の変更を取りこぼさないようにする。
        //
        // 例外は、読めなかった設定ファイルを退避できずディスクに残している場合。
        // ここで書き出すと、起動時の書き戻しを止めた意味が無くなる
        if self.autosave.is_allowed() {
            self.save_settings_now();
        } else {
            warn!("読めなかった設定ファイルを残しているため、終了時の保存を行わない");
        }

        // 撮った直後に閉じても最後の 1 枚が残るように、保存の完了を待ってから抜ける。
        // ここで待たないと、main が返った時点でプロセスごと落ちて
        // 書きかけの画像ファイルがディスクに残る
        self.join_screenshot_save_threads();

        // 終了中に終わった保存の結果をログへ残す。**待ったあとに読むこと。**
        // 画面はもう出ないので通知はされないが、閉じる直前に撮った 1 枚が
        // 保存できなかったことは、ログにだけは残しておかないと追えない
        self.drain_screenshot_results();
    }
}

impl CaptureCardViewer {
    fn update_video_texture(&mut self, ctx: &egui::Context) {
        // 新着フレームが無ければ何もしない。既存のテクスチャをそのまま使い回す
        let new_frame = self
            .video_capture
            .lock()
            .ok()
            .and_then(|video| video.get_frame_if_newer(self.last_frame_generation));

        if let Some((frame, generation)) = new_frame {
            self.last_frame_generation = generation;

            // 途絶から戻ってきた。次の途絶をもう一度検出できるように番人を戻す
            if self.last_video_link_action != VideoLinkAction::Keep {
                info!("映像フレームが再び届き始めたので表示を再開する");
                self.last_video_link_action = VideoLinkAction::Keep;
            }

            // 最適化: テクスチャオプションをNearest（補間なし）に設定し、性能向上
            let texture_options = egui::TextureOptions {
                magnification: egui::TextureFilter::Nearest,
                minification: egui::TextureFilter::Linear,
                wrap_mode: egui::TextureWrapMode::ClampToEdge,
            };

            let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.data);
            if let Some(texture) = &mut self.video_texture {
                texture.set(image, texture_options);
            } else {
                self.video_texture = Some(ctx.load_texture("video_frame", image, texture_options));
            }

            // より積極的な再描画要求
            ctx.request_repaint();
        }
        // フレームがない場合でも定期的に再チェック
        ctx.request_repaint_after(std::time::Duration::from_millis(16)); // ~60fps
    }

    /// 押されたホットキーのアクションを実行する。
    ///
    /// 1 フレームに複数のアクションが押されていた場合は、`HotkeyAction` の
    /// 宣言順に実行する。
    fn handle_hotkeys(&mut self, ctx: &egui::Context) {
        for action in self.hotkey_manager.take_pressed() {
            trace!("ホットキーの押下を受け取った: {}", action.label());
            self.run_hotkey_action(ctx, action);
        }
    }

    /// ホットキーに割り当てられたアクションを 1 つ実行する。
    ///
    /// **実処理は右クリックメニューや映像上の操作と同じ経路を通す。**
    /// ここに独自の処理を書くと、同じ操作なのに設定の保存やオーバーレイ表示の
    /// 有無が経路によって変わってしまう。
    fn run_hotkey_action(&mut self, ctx: &egui::Context, action: HotkeyAction) {
        match action {
            HotkeyAction::Screenshot => {
                debug!("スクリーンショットの処理に入る");
                self.take_screenshot();
            }
            HotkeyAction::ToggleFullscreen => {
                let to_full = !self.is_fullscreen;
                self.toggle_fullscreen(ctx, to_full);
            }
            HotkeyAction::ToggleAlwaysOnTop => {
                let enabled = !self.always_on_top;
                self.set_always_on_top(ctx, enabled);
            }
            HotkeyAction::ReconnectDevices => self.reconnect_devices(),
            HotkeyAction::VolumeUp => self.adjust_volume(VOLUME_SCROLL_STEP),
            HotkeyAction::VolumeDown => self.adjust_volume(-VOLUME_SCROLL_STEP),
            HotkeyAction::ToggleMute => self.toggle_mute(),
        }
    }

    /// いま表示しているフレームを、設定した出力先（ファイル / クリップボード /
    /// 両方）へ出す。ファイルへは設定した形式（JPEG / PNG）で保存する。
    ///
    /// ロックは settings → video → screenshot の順に 1 つずつ取り、重ねない。
    /// エンコードと書き出しは別スレッドへ逃がす。1080p のエンコードは
    /// JPEG でも数十 ms かかり、UI スレッドで行うと映像が一瞬止まるため
    /// （PNG は可逆圧縮のぶんさらに時間がかかる）。クリップボードへの転送も
    /// 同じスレッドで行う。こちらは他のアプリがクリップボードを掴んでいると
    /// 待たされるため、UI スレッドに置けない
    fn take_screenshot(&mut self) {
        debug!("スクリーンショットの出力を開始する");

        // この撮影を識別する時刻。結果が撮影順に届かないときの追い越し判定に使う。
        // ファイル名のタイムスタンプはミリ秒までなので同一ミリ秒で並びうるが、
        // `Instant` は単調増加するのでこちらは必ず順序が付く
        let started_at = Instant::now();

        // 出力先と効果音の音量だけを取り出してロックを手放す。
        // get_screenshot_path は連番を決めるためにファイルの有無を見るが、
        // ファイルを作るのは保存スレッドなので、ここでは何も書かない
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S-%3f").to_string();
        let save_params = self.settings.lock().ok().map(|settings| {
            let destination = settings.screenshot.destination;
            // クリップボードだけのときはファイル名を作らない。
            // get_screenshot_path は連番を決めるために保存先フォルダを
            // 走査するので、使わない名前のために I/O を走らせない
            let file_target = destination.saves_file().then(|| {
                (
                    settings.get_screenshot_path(&timestamp),
                    settings.screenshot.encoding(),
                )
            });
            (
                destination.copies_to_clipboard(),
                file_target,
                settings.screenshot.sound_volume,
            )
        });
        let Some((to_clipboard, file_target, sound_volume)) = save_params else {
            warn!("スクリーンショットの出力で settings のロックを取得できない");
            return;
        };

        // 最新フレームを取り出したらすぐロックを手放す。Arc の複製なので
        // 画素データは複製されず、フレームコールバック側の push を待たせない。
        // スクリーンショットはいま画面に出ている画を保存するので、新着でなくてよい
        let latest_frame = match self.video_capture.lock() {
            Ok(video) => video.get_latest_frame(),
            Err(_) => {
                warn!("スクリーンショットの出力で video_capture のロックを取得できない");
                return;
            }
        };
        let Some(frame) = latest_frame else {
            warn!("映像フレームが無いのでスクリーンショットを撮れない");
            // ホットキーを押しても何も起きないように見えるので画面にも出す。
            // 非同期の結果と同じ経路を通して、先に始めた保存の結果に
            // 追い越されないようにする
            self.apply_screenshot_outcome(started_at, Err("表示中の映像がありません".to_string()));
            return;
        };
        debug!(
            "出力対象の映像フレームを取得した: {}x{}、クリップボード: {}、保存先: {}",
            frame.width,
            frame.height,
            to_clipboard,
            file_target.as_ref().map_or_else(
                || "なし".to_string(),
                |(path, _)| path.display().to_string()
            )
        );

        // 効果音は保存の完了を待たずに鳴らす。撮った手応えをその場で返すため。
        // 保存まで待つと、エンコードにかかる数十 ms だけシャッター音が遅れる。
        // 保存に失敗した場合は音だけ鳴ることになるが、失敗はログに残す
        if let Ok(ss) = self.screenshot_manager.lock() {
            ss.play_screenshot_sound(sound_volume);
        } else {
            warn!("スクリーンショットの効果音で screenshot_manager のロックを取得できない");
        }

        // エンコードと書き出しは UI スレッドから外す。
        // ホットキーを連打するとスレッドが並ぶが、撮るたびに 1 枚残るほうを優先して
        // 進行中の保存があっても捨てない。ファイル名は撮影時刻をミリ秒まで含むので、
        // 人が連打できる間隔なら衝突しない（同一ミリ秒の衝突は元からある別の問題）
        // 結果は UI スレッドへ返す。失敗をログだけに出すと、保存先が書き込み
        // 不可のときにホットキーを押しても何も起きないように見える。
        // ログ出力も UI スレッド側（drain_screenshot_results）へ寄せてある
        let result_tx = self.screenshot_tx.clone();
        let handle = std::thread::spawn(move || {
            // 両方のときはクリップボードを先にする。撮ってすぐ貼る使い方で、
            // ディスクへの書き出しを待たせないため。
            // **片方が失敗しても他方は行う。** クリップボードを他のアプリが
            // 掴んでいてコピーできなくても、ファイルは残したい
            let clipboard = to_clipboard.then(|| screenshot::copy_frame_to_clipboard(&frame));
            let file = file_target
                .map(|(path, encoding)| save_frame(&frame, &path, encoding).map(|()| path));

            let result = summarize_screenshot_delivery(clipboard, file);
            if result_tx.send((started_at, result)).is_err() {
                // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                debug!("スクリーンショットの結果の送り先が既に無いので捨てる");
            }
        });

        // ハンドルを持っておく。捨てるとスレッドが切り離され、終了時に
        // 書き出しの完了を待てなくなる（壊れた画像ファイルが残りうる）。
        // 溜め込まないよう、積む前に終わった分を落とす
        drop_finished_threads(&mut self.screenshot_save_threads);
        self.screenshot_save_threads.push(handle);
    }

    /// 進行中のスクリーンショット保存がすべて終わるまで待つ。
    ///
    /// 待ち時間はエンコードとディスクへの書き出し（出力先にクリップボードが
    /// 含まれる場合はその転送も）が終わるまでで、1080p の JPEG なら通常は
    /// 数十 ms。終了時に呼ぶ
    fn join_screenshot_save_threads(&mut self) {
        let handles = std::mem::take(&mut self.screenshot_save_threads);
        if handles.is_empty() {
            return;
        }

        debug!(
            "スクリーンショットの保存スレッド {} 件を待つ",
            handles.len()
        );
        for handle in handles {
            if handle.join().is_err() {
                // release ビルドは panic = "abort" なのでここには来ない
                warn!("スクリーンショットの保存スレッドがパニックした");
            }
        }
    }

    fn show_windowed_ui(&mut self, ctx: &egui::Context) {
        // 映像が無いときの文言は描画に入る前に決める。
        // 描画のクロージャの中でロックを取らないため
        let placeholder = video_placeholder_text(
            self.video_capturing,
            self.video_retry.is_active(),
            self.error_detail(ErrorSource::Video).as_deref(),
        );

        // 装飾なしのときだけ、ウィンドウ端のドラッグをリサイズに割り当てる。
        // 帯の上にいる間は映像のドラッグ移動を止める（両方が効くと、
        // 端を掴んだつもりでウィンドウごと動く）
        let on_resize_edge = self.handle_borderless_resize(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(2.0))) // マージンを2pxに設定
            .show(ctx, |ui| {
                // 映像表示エリア
                let available_size = ui.available_size();

                if let Some(texture) = &self.video_texture {
                    let image_size = texture.size_vec2();
                    let display_size = if self.maintain_aspect_ratio {
                        calculate_aspect_ratio_size(image_size, available_size)
                    } else {
                        available_size
                    };

                    // 表示領域が潰れている間は描画も当たり判定も行わない。
                    // 大きさ 0 や負の矩形を割り当てても映像は見えず、
                    // ドラッグや右クリックの判定だけが残ると誤作動の元になる。
                    if display_size.x <= 0.0 || display_size.y <= 0.0 {
                        return;
                    }

                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size,
                    );

                    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)),
                        egui::Color32::WHITE,
                    );

                    // ウィンドウドラッグを処理（設定が有効な場合のみ）
                    if response.dragged() && !on_resize_edge {
                        if let Ok(settings) = self.settings.lock() {
                            if settings.ui.enable_drag_move {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        }
                    }

                    // インタラクションを処理
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, true);
                    }

                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos =
                            ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                    }

                    self.handle_middle_click_mute(&response);

                    // 音量調整のためのスクロールを処理
                    if response.hovered() {
                        self.handle_volume_scroll(ctx);
                    }
                } else {
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label(placeholder);
                    });

                    // 空エリアでのウィンドウドラッグを処理（設定が有効な場合のみ）
                    if response.dragged() && !on_resize_edge {
                        if let Ok(settings) = self.settings.lock() {
                            if settings.ui.enable_drag_move {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        }
                    }

                    // 空エリアでの右クリックを処理
                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos =
                            ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                    }

                    self.handle_middle_click_mute(&response);
                }
            });
    }

    fn show_fullscreen_ui(&mut self, ctx: &egui::Context) {
        // ウィンドウ表示と同じ理由で、描画に入る前に文言を決める
        let placeholder = video_placeholder_text(
            self.video_capturing,
            self.video_retry.is_active(),
            self.error_detail(ErrorSource::Video).as_deref(),
        );
        // フルスクリーンUI（装飾なし、ウィンドウ版と同等の機能）
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(0.0))) // フルスクリーンはマージン0
            .show(ctx, |ui| {
                let available_size = ui.available_size();

                if let Some(texture) = &self.video_texture {
                    let image_size = texture.size_vec2();
                    let display_size = if self.maintain_aspect_ratio {
                        calculate_aspect_ratio_size(image_size, available_size)
                    } else {
                        available_size
                    };

                    // 表示領域が潰れている間は描画も当たり判定も行わない。
                    // 大きさ 0 や負の矩形を割り当てても映像は見えず、
                    // ドラッグや右クリックの判定だけが残ると誤作動の元になる。
                    if display_size.x <= 0.0 || display_size.y <= 0.0 {
                        return;
                    }

                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size,
                    );

                    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)),
                        egui::Color32::WHITE,
                    );

                    // フルスクリーンではドラッグ移動を完全に無効化
                    // （フルスクリーンでは画面の移動自体が意味をなさないため）

                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, false);
                    }

                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos =
                            ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                    }

                    self.handle_middle_click_mute(&response);

                    // マウススクロールでの音量調整（ウィンドウ版と同じ機能）
                    if response.hovered() {
                        self.handle_volume_scroll(ctx);
                    }
                } else {
                    // 映像信号がない場合
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label(placeholder);
                    });

                    // フルスクリーンではドラッグ移動を完全に無効化
                    // （フルスクリーンでは画面の移動自体が意味をなさないため）

                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, false);
                    }

                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos =
                            ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                    }

                    self.handle_middle_click_mute(&response);
                }
            });
    }

    /// 映像の上でのホイール操作を音量へ反映する。
    ///
    /// ウィンドウ表示とフルスクリーンの両方から呼ぶ。以前は同じ処理が両方に
    /// 写してあり、片方だけ直す事故が起きやすかった。
    fn handle_volume_scroll(&mut self, ctx: &egui::Context) {
        let scroll_y = ctx.input(|i| i.raw_scroll_delta.y);
        if scroll_y == 0.0 {
            return;
        }

        // 段の大きさ、上下限、ミュートの扱いを「音量を上げる / 下げる」の
        // ホットキーと同じにするため、同じ経路へ寄せる
        self.adjust_volume(if scroll_y > 0.0 {
            VOLUME_SCROLL_STEP
        } else {
            -VOLUME_SCROLL_STEP
        });
    }

    /// UI の操作で音量が変わったときの共通処理。
    ///
    /// 設定へ反映して OSD を出す。**ここでディスクへは書かない。**
    /// ホイールを回している間は毎フレーム値が変わるため、書き出しは
    /// `mark_settings_dirty` のデバウンスに任せる。
    ///
    /// 上限・下限に貼り付いたまま操作を続けた場合も OSD の期限は延びる。
    /// 「これ以上は上がらない」ことが分かるほうがよいので、値が変わったかは見ない。
    fn set_volume_from_ui(&mut self, volume: f32) {
        self.volume = volume;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.volume = volume;
        }
        self.mark_settings_dirty();
        self.show_volume_overlay();
    }

    /// いまの音量を OSD に出す。ミュート中はバーを灰色にして数字だけ残す。
    fn show_volume_overlay(&mut self) {
        self.transient_overlay.show(
            volume_overlay_content(self.volume, self.muted),
            VOLUME_OSD_DURATION,
            Instant::now(),
        );
    }

    /// 映像や空きエリアの上でのミドルクリックをミュートの切り替えへ回す。
    ///
    /// ウィンドウ表示とフルスクリーンの、映像あり / なしの 4 か所から呼ぶ。
    /// 映像が出ていないときも切り替えられるようにしてあるのは、音だけ先に
    /// 来ている状態でも黙らせられるようにするため。
    fn handle_middle_click_mute(&mut self, response: &egui::Response) {
        if response.middle_clicked() {
            self.toggle_mute();
        }
    }

    /// 映像の統計を左上へ半透明で重ねて描く。
    ///
    /// 統計の取り出しは 1 フレームにつきこの 1 回だけ。ロックの中では
    /// 値のコピーと最大 120 要素の集計しか起きないため、毎フレーム呼んでよい。
    fn show_stats_overlay(&self, ctx: &egui::Context) {
        let Ok(video) = self.video_capture.lock() else {
            // ロックを取れないのは他所が長く掴んでいるときだけ。
            // 表示のために待たず、このフレームは描かない
            return;
        };
        let stats = video.stats();
        drop(video);

        egui::Area::new("stats_overlay")
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(8.0, 8.0))
            // 映像のドラッグや右クリックを吸わないようにする
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        for line in format_stats_lines(&stats) {
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .color(egui::Color32::WHITE),
                            );
                        }
                    });
            });
    }

    fn show_context_menu(&mut self, ctx: &egui::Context) {
        let mut close_menu = false;
        let mut final_rect: Option<egui::Rect> = None;

        egui::Area::new("context_menu")
            .fixed_pos(self.context_menu_pos)
            .order(egui::Order::Foreground)
            .show(ctx, |outer_ui| {
                // 固定幅でポップアップコンテンツをラップ
                egui::Frame::popup(&ctx.style()).show(outer_ui, |ui| {
                    // メニューの幅を240pxに固定
                    ui.set_min_width(240.0);
                    ui.set_max_width(240.0);

                    ui.label(format!("音量: {}%", self.volume as i32));
                    let volume_response = ui.add(
                        egui::Slider::new(&mut self.volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"),
                    );

                    // 音量が変更された場合、設定に反映する（書き出しはデバウンス）。
                    // スライダーが self.volume を書き換えたあとなので、同じ値を
                    // 渡し直して設定への反映と OSD の表示だけを行わせる
                    if volume_response.changed() {
                        self.set_volume_from_ui(self.volume);
                    }

                    // ミュートはスライダーのすぐ下に置く。音量 0% にする代わりの
                    // 操作なので、離すと探されない
                    let mut muted = self.muted;
                    if ui.checkbox(&mut muted, "ミュート").changed() {
                        self.set_muted_from_ui(muted);
                    }

                    ui.separator();
                    let aspect_response =
                        ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");

                    // アスペクト比設定が変更された場合、設定に反映する（書き出しはデバウンス）
                    if aspect_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.maintain_aspect_ratio = self.maintain_aspect_ratio;
                        }
                        self.mark_settings_dirty();
                    }

                    // 最前面表示のチェックボックス
                    let always_on_top_response = ui.checkbox(&mut self.always_on_top, "最前面表示");

                    // 最前面表示設定が変更された場合。
                    // チェックボックスが self.always_on_top を書き換えたあとなので、
                    // 同じ値を渡してウィンドウレベルの適用と保存だけを行わせる
                    if always_on_top_response.changed() {
                        self.set_always_on_top(ctx, self.always_on_top);
                    }

                    // フルスクリーン表示のチェックボックス
                    let fullscreen_response =
                        ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");

                    // フルスクリーン状態が変更された場合
                    if fullscreen_response.changed() {
                        self.toggle_fullscreen(ctx, self.is_fullscreen);
                    }

                    // タイトルバーを隠すチェックボックス。
                    // フルスクリーン中は OS が元から装飾を外しているので触らせない。
                    // ここで切り替えても見た目は変わらず、フルスクリーンを抜けた
                    // ときに初めて効くので、操作と結果が結びつかない
                    let mut temp_borderless = self.borderless;
                    let borderless_response = ui
                        .add_enabled(
                            !self.is_fullscreen,
                            egui::Checkbox::new(&mut temp_borderless, "タイトルバーを隠す"),
                        )
                        .on_hover_text(
                            "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います",
                        )
                        .on_disabled_hover_text(
                            "フルスクリーン中は元から装飾がないため切り替えられません",
                        );

                    if borderless_response.changed() {
                        self.set_borderless(ctx, temp_borderless);
                    }

                    // 画面ドラッグ移動のチェックボックス。
                    // 装飾なしの間は切らせない。切ると動かす手段が残らない
                    let enable_drag_move = if let Ok(settings) = self.settings.lock() {
                        settings.ui.enable_drag_move
                    } else {
                        true
                    };
                    let mut temp_enable_drag_move = enable_drag_move;
                    let drag_move_response = ui
                        .add_enabled(
                            !self.borderless,
                            egui::Checkbox::new(&mut temp_enable_drag_move, "画面ドラッグ移動"),
                        )
                        .on_disabled_hover_text(
                            "タイトルバーを隠している間は、ウィンドウを動かす唯一の手段なので切れません",
                        );

                    // 画面ドラッグ移動設定が変更された場合（書き出しはデバウンス）
                    if drag_move_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.enable_drag_move = temp_enable_drag_move;
                        }
                        self.mark_settings_dirty();
                    }

                    // 情報表示（統計オーバーレイ）のチェックボックス
                    let stats_response = ui.checkbox(&mut self.show_stats_overlay, "情報表示");

                    // 情報表示の設定が変更された場合（書き出しはデバウンス）
                    if stats_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.show_stats_overlay = self.show_stats_overlay;
                        }
                        self.mark_settings_dirty();
                    }

                    // デバイスの自動再接続のチェックボックス。
                    // 設定は VideoSettings に持たせているが、音声ストリームの
                    // エラーからの復帰にも効く（利用者から見て 1 つの機能なので
                    // スイッチも 1 つにしてある）
                    let auto_reconnect = if let Ok(settings) = self.settings.lock() {
                        settings.video.auto_reconnect
                    } else {
                        true
                    };
                    let mut temp_auto_reconnect = auto_reconnect;
                    let auto_reconnect_response = ui
                        .checkbox(&mut temp_auto_reconnect, "デバイスの自動再接続")
                        .on_hover_text(
                            "映像が途切れたり音声デバイスが消えたときに、自動でデバイスを開き直します",
                        );

                    // 自動再接続の設定が変更された場合（書き出しはデバウンス）
                    if auto_reconnect_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.video.auto_reconnect = temp_auto_reconnect;
                        }
                        info!(
                            "デバイスの自動再接続を{}にした",
                            if temp_auto_reconnect {
                                "有効"
                            } else {
                                "無効"
                            }
                        );
                        self.mark_settings_dirty();
                    }

                    ui.separator();
                    // ウィンドウサイズのリセット。装飾なしで小さくしすぎて
                    // 端の帯を掴めなくなったときの復帰手段
                    if ui
                        .add_enabled(
                            !self.is_fullscreen,
                            egui::Button::new("ウィンドウサイズをリセット"),
                        )
                        .on_disabled_hover_text("フルスクリーン中は変更できません")
                        .clicked()
                    {
                        self.reset_window_size(ctx);
                        close_menu = true;
                    }
                    if ui.button("デバイス再接続").clicked() {
                        self.reconnect_devices();
                        close_menu = true;
                    }
                    ui.separator();
                    if ui.button("詳細設定...").clicked() {
                        self.show_settings = true;
                        close_menu = true;
                    }
                    ui.separator();
                    // 終了。装飾なしでは × が無いので、ここが閉じる手段になる。
                    // 押すと on_exit が走り、保留中の設定も書き出される
                    if ui.button("終了").clicked() {
                        info!("右クリックメニューから終了する");
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        close_menu = true;
                    }
                });
                // 構築後、エリアの完全な矩形をキャプチャ
                final_rect = Some(outer_ui.min_rect());
            });

        // 外側をクリック、またはEscapeキー押下時のみ閉じる
        ctx.input(|i| {
            if i.pointer.primary_clicked() {
                if let Some(pos) = i.pointer.latest_pos() {
                    if let Some(r) = final_rect {
                        if !r.contains(pos) {
                            close_menu = true;
                        }
                    } else {
                        close_menu = true;
                    }
                }
            }
            if i.key_pressed(egui::Key::Escape) {
                close_menu = true;
            }
        });

        if close_menu {
            self.show_context_menu = false;
        }
    }
}

/// 統計オーバーレイに出す行を組み立てる。
///
/// 値が取れていない項目は数値を出さずに「-」や「なし」にする。
/// フレームが 1 枚も来ていない状態で平均を出そうとすると NaN や
/// 無限大になり、それがそのまま画面に出てしまうため。
fn format_stats_lines(stats: &FrameStats) -> Vec<String> {
    let mut lines = Vec::new();

    match stats.intervals {
        Some(intervals) => {
            lines.push(format!(
                "FPS {:.1} (平均間隔 {:.1}ms / {} 件)",
                intervals.fps, intervals.average_ms, intervals.samples
            ));
            lines.push(format!(
                "ばらつき ±{:.2}ms (最小 {:.1} / 最大 {:.1})",
                intervals.stddev_ms, intervals.min_ms, intervals.max_ms
            ));
        }
        None => lines.push("FPS - (フレーム間隔の計測待ち)".to_string()),
    }

    match (stats.resolution, stats.source_format) {
        (Some((width, height)), Some(format)) => {
            // フレームが 1 枚でも届いていれば、変換の計測値は実測値
            lines.push(format!(
                "デコード {:.2}ms (高速 {} / 汎用 {})",
                stats.last_decode_ms, stats.fast_count, stats.fallback_count
            ));
            lines.push(format!("{}x{} {}", width, height, format));
        }
        _ => {
            // 計測前の 0 を実測値と読み違えられないようにする
            lines.push("デコード -".to_string());
            lines.push("映像フレームなし".to_string());
        }
    }

    if let Some(elapsed_ms) = stats.since_last_frame_ms {
        lines.push(format!("最終フレーム {:.0}ms 前", elapsed_ms));
    }

    lines
}

/// 音量 OSD に出す内容を組み立てる。
///
/// バーは 0〜`MAX_VOLUME`% を全体とし、100% の位置に目盛りを引く。
/// 上限が 200% なので、数字だけでは「上げすぎているのか」が分かりにくいため。
///
/// 数字は右クリックメニューの「音量: N%」と同じ `as i32` で作る。丸め方を
/// 変えると、メニューのスライダーを動かしている間だけ OSD と 1% ずれて見える。
///
/// ミュート中はバーを灰色にし、文言に「（ミュート中）」を添える。**数字は消さない。**
/// ミュートを解除したときに戻る音量がそのまま見えているほうが、操作の結果を
/// 予想しやすいため。
fn volume_overlay_content(volume: f32, muted: bool) -> OverlayContent {
    let text = if muted {
        format!("音量: {}%（ミュート中）", volume as i32)
    } else {
        format!("音量: {}%", volume as i32)
    };
    OverlayContent::Bar {
        text,
        ratio: volume / MAX_VOLUME,
        marker_ratio: VOLUME_REFERENCE / MAX_VOLUME,
        dimmed: muted,
    }
}

/// ミュートを切り替えたときに OSD へ出す内容を組み立てる。
///
/// 解除したときだけ音量を添える。ミュート中にスライダーで音量を変えている
/// ことがあるため、「解除したら何%で鳴るのか」が分かるようにしている。
fn mute_overlay_content(muted: bool, volume: f32) -> OverlayContent {
    if muted {
        OverlayContent::Text("ミュート".to_string())
    } else {
        OverlayContent::Text(format!("ミュート解除（音量: {}%）", volume as i32))
    }
}

/// 映像上のホイール操作と「音量を上げる / 下げる」のホットキーで音量を変えたときの、
/// 適用すべき `(音量, ミュート状態)`。
///
/// **ミュート中でも解除する。** これらの操作の近くにはミュートの表示が無く、
/// 解除しないと「音量を上げたのに鳴らない」状態になって原因が分からない。
/// 右クリックメニューのスライダーはすぐ下にミュートのチェックが見えているので、
/// そちらは解除せず、灰色のバーで「効いていない」ことだけを示す。
fn volume_change_result(current: f32, delta: f32) -> (f32, bool) {
    ((current + delta).clamp(MIN_VOLUME, MAX_VOLUME), false)
}

/// 完了済みのスレッドハンドルを取り除く。
///
/// `JoinHandle` を持ち続けるのは終了時に `join` するためだけなので、
/// 終わったものは落としてよい。落とさないと撮影のたびに要素が増え続ける
fn drop_finished_threads<T>(handles: &mut Vec<JoinHandle<T>>) {
    handles.retain(|handle| !handle.is_finished());
}

/// 映像フレームを `encoding` の形式で `path` へ書き出す。
///
/// アプリの状態にも共有ロックにも触れないので、そのまま別スレッドで実行でき、
/// テストからも呼べる。保存スレッドはこの関数だけを呼ぶ。
fn save_frame(
    frame: &video::VideoFrame,
    path: &Path,
    encoding: ScreenshotEncoding,
) -> Result<(), String> {
    // 大きさのないフレームは画像として書き出せてしまうが、開けない
    // ファイルが残るだけなので、ディレクトリを作る前に弾く
    if frame.width == 0 || frame.height == 0 {
        return Err(format!(
            "大きさのない映像フレームは保存できない: {}x{}",
            frame.width, frame.height
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "保存先のディレクトリ {} を作成できない: {}",
                parent.display(),
                e
            )
        })?;
    }

    let (Ok(width), Ok(height)) = (u32::try_from(frame.width), u32::try_from(frame.height)) else {
        return Err(format!(
            "画像として扱えない大きさのフレーム: {}x{}",
            frame.width, frame.height
        ));
    };

    // image クレートが Vec の所有権を要求するため、ここだけは複製が要る。
    // UI スレッドの外なので、1080p で 6MB の複製が描画を止めることはない
    let img = image::RgbImage::from_raw(width, height, frame.data.clone()).ok_or_else(|| {
        format!(
            "映像フレームから画像を組み立てられない: {}x{} に対して {} バイト",
            width,
            height,
            frame.data.len()
        )
    })?;

    // image の save() は拡張子から形式を決めるうえ、JPEG は品質 75 固定に
    // なるため使わない。形式は encoding で決め、書き出し先は自分で開く
    let file = std::fs::File::create(path)
        .map_err(|e| format!("{} を作成できない: {}", path.display(), e))?;
    let mut writer = std::io::BufWriter::new(file);

    let encoded = match encoding {
        ScreenshotEncoding::Jpeg { quality } => img.write_with_encoder(
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality),
        ),
        ScreenshotEncoding::Png => {
            img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut writer))
        }
    };

    // BufWriter は drop のときにも書き出すが、そこで起きた失敗は捨てられる。
    // 取りこぼすと、書き切れていないファイルを保存できたものとして扱ってしまう
    let result = encoded
        .map_err(|e| format!("{} へ書き出せない: {}", path.display(), e))
        .and_then(|()| {
            writer
                .into_inner()
                .map_err(|e| format!("{} へ書き出せない: {}", path.display(), e))
        })
        .and_then(|file| {
            file.sync_all()
                .map_err(|e| format!("{} を書き切れない: {}", path.display(), e))
        });

    if result.is_err() {
        // 途中まで書けたファイルを残さない。残すと開けない画像が
        // 保存先に紛れ込み、次の撮影では連番の相手にもなる
        if let Err(e) = std::fs::remove_file(path) {
            warn!(
                "書き出しに失敗した {} を削除できない: {}",
                path.display(),
                e
            );
        }
    }

    result
}

// 映像の縦横比を保ったまま、表示領域に収まる大きさを求める。
//
// self を使わない純粋な計算なので、ユニットテストできるよう
// impl の外へ出してある。
//
// 幅か高さが 0 以下の入力に対しては egui::Vec2::ZERO を返す。最小化や
// ウィンドウの極端な縮小で available_size が潰れると 0 除算で縦横比が
// inf / NaN になり、そのまま Rect へ渡すと描画が壊れるため。
// 呼び出し側は ZERO を「描画するものがない」と解釈して描画を飛ばす。
fn calculate_aspect_ratio_size(image_size: egui::Vec2, available_size: egui::Vec2) -> egui::Vec2 {
    if image_size.x <= 0.0
        || image_size.y <= 0.0
        || available_size.x <= 0.0
        || available_size.y <= 0.0
    {
        return egui::Vec2::ZERO;
    }

    let image_aspect = image_size.x / image_size.y;
    let available_aspect = available_size.x / available_size.y;

    if image_aspect > available_aspect {
        // 画像が横長 - 横幅に合わせる
        egui::Vec2::new(available_size.x, available_size.x / image_aspect)
    } else {
        // 画像が縦長 - 高さに合わせる
        egui::Vec2::new(available_size.y * image_aspect, available_size.y)
    }
}

/// タイトルバーを消すときに「画面ドラッグ移動」を自動で有効にする必要があるかを返す。
///
/// 装飾なしではタイトルバーが無いため、ドラッグ移動も切れているとウィンドウを
/// 動かす手段が残らない。**その状態を作らせない。** 端のドラッグはリサイズに
/// 割り当ててあり、移動には使えない。
///
/// タイトルバーを戻すときは何もしない。ユーザーが自分で切ったドラッグ移動を
/// 勝手に戻すことになるため。
fn needs_drag_move_guard(to_borderless: bool, enable_drag_move: bool) -> bool {
    to_borderless && !enable_drag_move
}

/// ウィンドウ端の当たり判定。`pos` が `rect` の縁から `margin` 以内なら、
/// その縁に対応する `ResizeDirection` を返す。縁から離れていれば `None`。
///
/// 装飾なしのときにだけ使う。OS が描く枠の代わりに、自前で掴める帯を作る。
///
/// ウィンドウが `margin` の 2 倍より細いと左右（上下）の帯が重なる。
/// その場合は左と上を優先する。どちらを選んでも掴めることに変わりはなく、
/// 「どちらとも言えない」を返して掴めなくするほうが困るため。
fn resize_direction_at(
    pos: egui::Pos2,
    rect: egui::Rect,
    margin: f32,
) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection;

    // ポインタ位置は egui から来るが、NaN が紛れ込むと比較がすべて false になり
    // 判定が静かに壊れる。先に弾いておく
    if !pos.x.is_finite() || !pos.y.is_finite() || !margin.is_finite() || margin <= 0.0 {
        return None;
    }
    if !rect.contains(pos) {
        return None;
    }

    let left = pos.x - rect.left() <= margin;
    let right = !left && rect.right() - pos.x <= margin;
    let top = pos.y - rect.top() <= margin;
    let bottom = !top && rect.bottom() - pos.y <= margin;

    match (top, bottom, left, right) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (true, _, _, true) => Some(ResizeDirection::NorthEast),
        (true, ..) => Some(ResizeDirection::North),
        (_, true, true, _) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (_, true, ..) => Some(ResizeDirection::South),
        (_, _, true, _) => Some(ResizeDirection::West),
        (_, _, _, true) => Some(ResizeDirection::East),
        _ => None,
    }
}

/// リサイズの向きに対応するカーソル。装飾ありのウィンドウ枠と同じ見た目にする。
fn resize_cursor(direction: egui::ResizeDirection) -> egui::CursorIcon {
    use egui::{CursorIcon, ResizeDirection};

    match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
    }
}

/// 保存されたウィンドウサイズのうち、ウィンドウとして成立する値だけを採用して返す。
///
/// 設定ファイルは手で編集できるため、0 や負数や NaN が入りうる。検証せずに
/// `with_inner_size` へ渡すと、winit の先の OS の API 次第で操作できない大きさの
/// ウィンドウになったり、起動そのものに失敗したりする。**採用できない値は
/// 既定のサイズへ倒す。**
fn window_size_or_default(saved: Option<(f32, f32)>) -> (f32, f32) {
    match saved {
        Some((width, height))
            if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 =>
        {
            (width, height)
        }
        _ => DEFAULT_WINDOW_SIZE,
    }
}

/// 保存されたウィンドウの位置が、いずれかのモニタの作業領域と十分に重なるかを判定する。
///
/// サブモニタを外した、解像度を変えた、といった理由で保存値が画面外になることがある。
/// そのまま復元するとウィンドウが見えず、タイトルバーも掴めないので復帰できない。
///
/// `monitors` はモニタの作業領域の一覧。**空の場合は false を返す。** モニタの構成が
/// 分からないまま位置を指定するより、OS に任せたほうが安全なため。
fn is_position_visible(pos: (f32, f32), size: (f32, f32), monitors: &[egui::Rect]) -> bool {
    if monitors.is_empty() {
        return false;
    }

    // 設定ファイルは手で編集できるので、NaN や inf が入っていることを想定する
    if ![pos.0, pos.1, size.0, size.1].iter().all(|v| v.is_finite()) {
        return false;
    }

    // 大きさが潰れているウィンドウは、どこに置いても見えない
    if size.0 <= 0.0 || size.1 <= 0.0 {
        return false;
    }

    let window = egui::Rect::from_min_size(egui::pos2(pos.0, pos.1), egui::vec2(size.0, size.1));

    // ウィンドウ自体が最小値より小さい場合は、その全体が収まることを求める
    let required_width = MIN_VISIBLE_WINDOW_WIDTH.min(window.width());
    let required_height = MIN_VISIBLE_WINDOW_HEIGHT.min(window.height());

    monitors.iter().any(|monitor| {
        let overlap = monitor.intersect(window);
        // 重なりが無い場合、intersect は負の幅・高さを持つ矩形を返す
        overlap.width() >= required_width && overlap.height() >= required_height
    })
}

/// 各モニタの作業領域（タスクバーなどを除いた領域）を返す。取得できなければ空の `Vec`。
///
/// **この関数は `eframe::run_native` より前に呼ぶ前提で書いてある。** winit が
/// プロセスの DPI 認識を設定するのは `run_native` の中なので、ここで得られる座標は
/// Windows が仮想化した座標、つまり既定の拡大率で割った論理座標になる。
/// 設定に保存されているウィンドウ位置も egui のポイント（論理座標）なので、
/// そのまま比較できる。**DPI 認識を宣言するマニフェストを追加すると前提が崩れる。**
#[cfg(windows)]
fn monitor_work_areas() -> Vec<egui::Rect> {
    use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, TRUE};
    use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
    use winapi::um::winuser::{EnumDisplayMonitors, GetMonitorInfoW, MONITORINFO};

    /// `EnumDisplayMonitors` のコールバック。`lparam` で受け取った `Vec` へ作業領域を積む
    unsafe extern "system" fn collect_work_area(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: LPRECT,
        lparam: LPARAM,
    ) -> BOOL {
        let areas = &mut *(lparam as *mut Vec<egui::Rect>);

        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as DWORD;
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            let work = info.rcWork;
            areas.push(egui::Rect::from_min_max(
                egui::pos2(work.left as f32, work.top as f32),
                egui::pos2(work.right as f32, work.bottom as f32),
            ));
        }

        // 列挙を続ける
        TRUE
    }

    let mut areas: Vec<egui::Rect> = Vec::new();
    // hdc と lprcClip を null にすると仮想画面全体のモニタが列挙される
    let enumerated = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_work_area),
            &mut areas as *mut Vec<egui::Rect> as LPARAM,
        )
    };

    if enumerated == 0 {
        // 途中まで積んだ内容は信用できない。取得できなかったものとして扱う
        return Vec::new();
    }

    areas
}

/// Windows 以外ではモニタ情報を取得しない。位置の復元は OS に任せる
#[cfg(not(windows))]
fn monitor_work_areas() -> Vec<egui::Rect> {
    Vec::new()
}

fn main() -> Result<(), eframe::Error> {
    // 何よりも先にログ基盤を用意する。これ以降の失敗を記録できるようにするため。
    //
    // 失敗しても起動は続ける。ログが無いだけでアプリの機能には影響しない。
    // 失敗の理由を書き出す先はこの時点に存在しない（コンソールが無く、
    // ログファイルも開けていない）ので、戻り値はここで捨てるしかない。
    let _ = logging::init();

    // 設定から保存されたウィンドウサイズと位置を読み込む。
    // ここでは読み込み結果を使わない。既定値の書き戻しは
    // CaptureCardViewer::default 側だけで行うため。
    let (settings, _) = AppSettings::load();
    let mut viewport_builder = egui::ViewportBuilder::default().with_icon(load_icon());

    // タイトルバーと枠の有無は最初のウィンドウ生成時に決める。
    // 生成後に ViewportCommand::Decorations で戻すと、装飾ありのウィンドウが
    // 一瞬見えてから消える
    viewport_builder = viewport_builder.with_decorations(!settings.ui.borderless);

    // 保存されたウィンドウサイズがあれば適用する。値が壊れていれば既定のサイズにする
    let inner_size = window_size_or_default(settings.ui.last_window_size);
    viewport_builder = viewport_builder.with_inner_size([inner_size.0, inner_size.1]);

    // 保存されたウィンドウ位置は、モニタ構成が変わって画面外を指していることがある。
    // 作業領域と十分に重なるときだけ適用し、そうでなければ位置指定ごと捨てて
    // OS の既定の配置に任せる。見えないウィンドウで起動するよりは良い
    if let Some(pos) = settings.ui.last_window_pos {
        if is_position_visible(pos, inner_size, &monitor_work_areas()) {
            viewport_builder = viewport_builder.with_position([pos.0, pos.1]);
        }
    }

    let options = eframe::NativeOptions {
        viewport: viewport_builder,
        ..Default::default()
    };

    eframe::run_native(
        "Capturecard Viewer",
        options,
        Box::new(|cc| {
            configure_japanese_font(&cc.egui_ctx);
            Box::new(CaptureCardViewer::default())
        }),
    )
}

fn configure_japanese_font(ctx: &egui::Context) {
    // WindowsフォントディレクトリからMeiryoの読み込みを試行
    #[cfg(target_os = "windows")]
    {
        let candidate_paths = [
            "C:/Windows/Fonts/meiryo.ttc",
            "C:/Windows/Fonts/Meiryo.ttc",
            "C:/Windows/Fonts/meiryob.ttc",
        ];
        for p in candidate_paths.iter() {
            if let Ok(data) = std::fs::read(p) {
                let mut fonts = egui::FontDefinitions::default();
                fonts
                    .font_data
                    .insert("meiryo".to_string(), egui::FontData::from_owned(data));
                // 優先度のためにプロポーショナル・等幅フォントファミリーの先頭に挿入
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    fam.insert(0, "meiryo".to_string());
                }
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    fam.insert(0, "meiryo".to_string());
                }
                ctx.set_fonts(fonts);
                break;
            }
        }
    }
}

// ウィンドウアイコン。実行ファイルに埋め込む。
// 以前はカレントディレクトリ基準で "icon.ico" を読んでいたため、ショートカット経由など
// 作業ディレクトリが exe の場所と異なる起動ではファイルを見つけられず、
// フォールバックの赤い四角が表示されていた。
const EMBEDDED_ICON: &[u8] = include_bytes!("../icon.ico");

fn load_icon() -> egui::IconData {
    if let Ok(icon) = image::load_from_memory(EMBEDDED_ICON) {
        let icon_rgba = icon.to_rgba8();
        let (width, height) = icon.dimensions();
        return egui::IconData {
            rgba: icon_rgba.into_raw(),
            width,
            height,
        };
    }

    // フォールバック: 単純な色付き四角形を作成。
    // 埋め込みデータのデコードに失敗した場合だけ通る。
    let mut rgba_data = Vec::new();
    for _ in 0..(32 * 32) {
        rgba_data.extend_from_slice(&[255, 0, 0, 255]);
    }
    egui::IconData {
        rgba: rgba_data,
        width: 32,
        height: 32,
    }
}

impl CaptureCardViewer {
    /// 設定値を適用し直す必要があるかを判定する。
    /// `last` は最後に適用できた値で、`None` は「まだ適用できていない」を表す。
    /// `initial` が真なら値が変わっていなくても適用する。
    fn needs_reapply<T: PartialEq>(initial: bool, current: &T, last: &Option<T>) -> bool {
        initial || last.as_ref() != Some(current)
    }

    /// 期限が来ているデバイスの接続を 1 回だけ試す。`update()` から毎フレーム呼ぶ。
    ///
    /// **ここで `thread::sleep` を使わない。** UI スレッドを止めると、接続に
    /// 失敗し続ける間ウィンドウが固まる。待つ代わりに次に試してよい時刻を
    /// `ConnectRetry` に覚えておき、そのフレームが来るまで何もしない。
    ///
    /// 繋がっている間は `bool` を 2 つ見るだけで戻るので、設定の複製もしない。
    fn poll_device_connection(&mut self) {
        let now = Instant::now();
        let video_due = self.video_retry.is_due(now);
        let audio_due = self.audio_retry.is_due(now);
        if !video_due && !audio_due {
            return;
        }

        // 設定はここで 1 度だけ複製する。デバイスを開いている間 settings の
        // ロックを握らないための措置で、apply_settings と同じ考え方
        let snapshot = match self.settings.lock() {
            Ok(settings) => settings.clone(),
            Err(_) => {
                warn!("デバイスの接続で settings のロックを取得できない");
                return;
            }
        };

        if video_due {
            self.try_connect_video(&snapshot, now);
        }
        if audio_due {
            self.try_connect_audio(&snapshot, now);
        }
    }

    /// 稼働中のデバイスが生きているかを見る。`update()` から毎フレーム呼ぶ。
    ///
    /// 起動時の接続は `poll_device_connection` の担当で、ここは「一度繋がった
    /// あとに消えた」場合だけを扱う。判断がついたら `ConnectRetry` へ要求を
    /// 積むところまでで、実際に開き直すのは次のフレームの
    /// `poll_device_connection`。開く処理を 2 か所に持たないため。
    ///
    /// ロックは settings → video → audio の順に 1 つずつ取り、重ねない。
    fn monitor_device_health(&mut self) {
        // 設定からは真偽値を 1 つ読むだけで手放す。ここで設定を丸ごと複製すると
        // デバイス名の String が毎フレーム複製される
        let auto_reconnect = match self.settings.lock() {
            Ok(settings) => settings.video.auto_reconnect,
            Err(_) => {
                warn!("デバイスの監視で settings のロックを取得できない");
                return;
            }
        };

        self.monitor_video_link(auto_reconnect);
        self.monitor_audio_stream(auto_reconnect);
    }

    /// フレームの途絶を見て、表示を落とし、必要なら映像を開き直す。
    fn monitor_video_link(&mut self, auto_reconnect: bool) {
        let state = match self.video_capture.lock() {
            Ok(video) => video.link_state(),
            Err(_) => {
                warn!("デバイスの監視で video_capture のロックを取得できない");
                return;
            }
        };
        // 描画側が参照する値をここで更新する。ロックは既に手放している
        self.video_capturing = state.capturing;

        let action = decide_video_link(state, auto_reconnect, VIDEO_SIGNAL_TIMEOUT);
        if action == VideoLinkAction::Keep {
            // 途絶が解消した（開き直した、ストリームを閉じた）。記録も戻して、
            // 次の途絶をもう一度検出できるようにする
            self.last_video_link_action = action;
            return;
        }
        // 途絶は毎フレーム同じ判定に当たる。同じ扱いが続く間は 1 度だけ動く。
        // **「動いたかどうか」ではなく「何をしたか」で見る。** 自動再接続を
        // 切ったまま途絶（ClearTexture）したあとに有効化すると判定が
        // ClearTextureAndReconnect へ変わるので、そこで開き直せる
        if action == self.last_video_link_action {
            return;
        }
        self.last_video_link_action = action;

        let elapsed_ms = state
            .since_last_frame
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        info!(
            "映像フレームが {} ms 途絶えたので、表示を落として切断として扱う",
            elapsed_ms
        );
        // 最後のフレームが残り続けると、止まっているのか映っているのか判らない。
        // テクスチャを捨てて「映像信号がありません」の表示へ戻す
        self.video_texture = None;

        if action != VideoLinkAction::ClearTextureAndReconnect {
            debug!("自動再接続が無効なので映像は開き直さない");
            return;
        }

        // 開き直しの対象は設定から取り直す
        let target = match self.settings.lock() {
            Ok(settings) => video_target(&settings),
            Err(_) => {
                warn!("映像の再接続で settings のロックを取得できない");
                return;
            }
        };

        // ストリームを閉じてから要求する。閉じておくと表示が
        // 「デバイスが接続されていません」へ変わり、信号だけが無い状態と区別できる
        match self.video_capture.lock() {
            Ok(mut video) => video.stop_capture(),
            Err(_) => {
                warn!("映像の再接続で video_capture のロックを取得できない");
                return;
            }
        }
        self.video_capturing = false;

        // 既存のバックオフへ乗せる。**ここでデバイスを列挙しない。**
        // 対象が戻っているかは `start_capture` の中の列挙（実測 1〜3ms）が
        // 確かめる。再試行の間隔は最大 5 秒で頭打ちなので、
        // MediaFoundation への問い合わせもその頻度を超えない
        self.last_video_device = None;
        self.video_retry.request_now(target);
        info!("映像デバイスの再接続を要求した");
    }

    /// 音声ストリームのエラーを拾って、必要なら開き直す。
    ///
    /// cpal のエラーコールバックはストリームのスレッドから呼ばれるため、
    /// そこでは旗を立てるだけにしてある（`audio::AudioCapture::take_stream_error`）。
    fn monitor_audio_stream(&mut self, auto_reconnect: bool) {
        let errored = match self.audio_capture.lock() {
            Ok(audio) => audio.take_stream_error(),
            Err(_) => {
                warn!("デバイスの監視で audio_capture のロックを取得できない");
                return;
            }
        };
        if errored {
            // エラーの内容自体は audio.rs が error! で残している
            warn!("音声ストリームのエラーを検出したので切断として扱う");
            // **旗は読んだ時点で下りている。** ここへ移しておかないと、
            // 自動再接続が無効な間や下限に達していない間のエラーが消え、
            // 誰も開き直さないまま音が戻らなくなる
            self.audio_stream_error_pending = true;
        }

        let since_last = self
            .last_audio_error_reconnect
            .map(|reconnected_at| reconnected_at.elapsed());
        match decide_audio_reconnect(self.audio_stream_error_pending, auto_reconnect, since_last) {
            // 保留しているエラーが無い
            AudioErrorAction::Idle => return,
            // 保留したまま待つ。自動再接続を有効にし直したとき、または
            // 下限に達したときのフレームでここを抜ける。
            // 毎フレーム通るのでログは出さない
            AudioErrorAction::Wait => return,
            AudioErrorAction::Reconnect => {}
        }

        let target = match self.settings.lock() {
            Ok(settings) => audio_target(&settings),
            Err(_) => {
                warn!("音声の再接続で settings のロックを取得できない");
                return;
            }
        };

        match self.audio_capture.lock() {
            Ok(mut audio) => audio.stop_capture(),
            Err(_) => {
                warn!("音声の再接続で audio_capture のロックを取得できない");
                return;
            }
        }

        self.audio_stream_error_pending = false;
        self.last_audio_error_reconnect = Some(Instant::now());
        self.last_audio_device = None;
        self.audio_retry.request_now(target);
        info!("音声デバイスの再接続を要求した");
    }

    /// 映像デバイスへの接続を 1 回だけ試す。
    fn try_connect_video(&mut self, settings: &AppSettings, now: Instant) {
        let Some(device_name) = settings.video.device_name.clone() else {
            // 繋ぐ相手が無い。要求を取り下げて、デバイスが選ばれるまで待つ
            debug!("映像デバイスが未設定なので接続の要求を取り下げる");
            self.video_retry.cancel();
            return;
        };

        let attempt = self.video_retry.attempts() + 1;
        info!(
            "映像デバイスへの接続を試す（{} 回目）: {}",
            attempt, device_name
        );

        // Arc を複製してから開く。self を借りたまま開くと、結果を書き戻すときに
        // 借用が衝突する
        let video_capture = Arc::clone(&self.video_capture);
        let result = match video_capture.lock() {
            Ok(mut video) => video.start_capture(
                Some(&device_name),
                settings.video.resolution,
                settings.video.format.as_deref(),
                settings.video.fps,
            ),
            Err(_) => Err("video_capture のロックを取得できない".to_string()),
        };

        match result {
            Ok(()) => {
                info!("映像デバイスに接続した");
                self.video_retry.record_success();
                // 繋がったので直前の失敗は消す。プレースホルダーと
                // 「接続状態」タブに古い理由が残らないようにする
                self.errors.clear(ErrorSource::Video);
                self.last_video_device = settings.video.device_name.clone();
                self.last_video_res = settings.video.resolution;
                self.last_video_format = settings.video.format.clone();
                self.last_video_fps = settings.video.fps;
            }
            Err(e) => {
                warn!("映像デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                self.video_retry.record_failure(now);
                self.report_error(ErrorSource::Video, e);
                debug!(
                    "映像デバイスへの再試行は {} ms 後",
                    backoff_delay(self.video_retry.attempts()).as_millis()
                );
            }
        }
    }

    /// 音声デバイスへの接続を 1 回だけ試す。
    ///
    /// 設定のデバイス名で開けない状態が続くと永久に音が出ないため、
    /// `AUDIO_DEFAULT_FALLBACK_AFTER` 回目の失敗の直後だけ、Windows の
    /// 既定デバイスで 1 度開き直す。
    fn try_connect_audio(&mut self, settings: &AppSettings, now: Instant) {
        let attempt = self.audio_retry.attempts() + 1;
        info!(
            "音声デバイスへの接続を試す（{} 回目）- 入力: {:?}、出力: {:?}",
            attempt, settings.audio.input_device_name, settings.audio.output_device_name
        );

        let audio_capture = Arc::clone(&self.audio_capture);
        let Ok(mut audio) = audio_capture.lock() else {
            let reason = "audio_capture のロックを取得できない".to_string();
            warn!(
                "音声デバイスへの接続に失敗した（{} 回目）: {}",
                attempt, reason
            );
            self.audio_retry.record_failure(now);
            self.report_error(ErrorSource::Audio, reason);
            return;
        };

        // デバイスの列挙は実測で 300ms 前後かかる。設定値との突き合わせに要るのは
        // 最初の 1 回だけなので、再試行のたびには出さない
        if attempt == 1 {
            debug!("利用できる入力デバイス: {:?}", audio.list_input_devices());
            debug!("利用できる出力デバイス: {:?}", audio.list_output_devices());
        }

        // **音量とパススルーの反映は、ストリームを開く前に必ず済ませる。**
        // 開いたあとに反映すると、最初のバッファだけ AudioCapture の既定値
        // （100%・パススルー有効）で鳴ってしまう。音量 0% を保存して
        // 再起動したときに、起動直後だけ音が出るのがこの窓。
        // apply_settings でも同じ値を入れているが、そちらは「接続の要求を
        // 立てる」だけで実際に開くのはこの関数なので、開く直前でも入れておく
        audio.set_volume(settings.ui.volume);
        audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);

        let result = audio.start_passthrough_with_settings(
            settings.audio.input_device_name.as_deref(),
            settings.audio.output_device_name.as_deref(),
            settings.audio.sample_rate,
            settings.audio.channels,
        );

        let error = match result {
            Ok(()) => {
                info!("音声デバイスに接続した");
                None
            }
            Err(e) => {
                warn!("音声デバイスへの接続に失敗した（{} 回目）: {}", attempt, e);
                Some(e)
            }
        };

        // 既定デバイスへのフォールバック。何を試したかをログに残す
        let fallback_error = if error.is_some() && attempt == AUDIO_DEFAULT_FALLBACK_AFTER {
            info!(
                "設定のデバイスで {} 回続けて失敗したので、既定のデバイス（入力・出力とも Windows の既定、レートとチャンネル数もデバイス任せ）で試す",
                attempt
            );
            match audio.start_passthrough_with_settings(None, None, None, None) {
                Ok(()) => {
                    info!("既定のデバイスで音声に接続した");
                    None
                }
                Err(e2) => {
                    warn!("既定のデバイスでも音声に接続できない: {}", e2);
                    Some(e2)
                }
            }
        } else {
            error.clone()
        };

        drop(audio);

        match fallback_error {
            None => {
                self.audio_retry.record_success();
                // 繋がったので直前の失敗は消す
                self.errors.clear(ErrorSource::Audio);
                // 既定のデバイスで繋がった場合も、設定に書かれている値を記録する。
                // ここで実際に開いた値（None）を入れると、設定のデバイスが
                // 現れても need_audio_restart が立たず繋ぎ直せなくなる
                self.last_audio_device = settings.audio.input_device_name.clone();
                self.last_audio_output = settings.audio.output_device_name.clone();
                self.last_audio_rate = settings.audio.sample_rate;
                self.last_audio_channels = settings.audio.channels;
            }
            Some(reason) => {
                self.audio_retry.record_failure(now);
                self.report_error(ErrorSource::Audio, reason);
                debug!(
                    "音声デバイスへの再試行は {} ms 後",
                    backoff_delay(self.audio_retry.attempts()).as_millis()
                );
            }
        }
    }

    fn apply_settings(&mut self, initial: bool) {
        // 設定はここで 1 度だけ複製し、以降はこの複製だけを見る。
        // デバイスの開き直しはリトライの sleep を含めて秒単位かかるため、
        // その間 settings のロックを握っていると他の経路が止まる。
        // 複製しておけば video / audio / screenshot のロックをネストせずに済み、
        // 複数のロックを重ねて取る箇所がこの関数から無くなる
        let snapshot = match self.settings.lock() {
            Ok(settings) => Some(settings.clone()),
            Err(_) => {
                warn!("設定の適用で settings のロックを取得できない");
                None
            }
        };

        if let Some(settings) = snapshot {
            // Video
            //
            // ここではデバイスを開かない。要求を立てるだけにして、実際に開くのは
            // update() から呼ばれる poll_device_connection に任せる。
            // この関数は設定ダイアログや右クリックメニューからも呼ばれるため、
            // ここで開くと失敗したときにその場で UI が止まる
            let need_video_restart = settings.video.device_name != self.last_video_device
                || settings.video.resolution != self.last_video_res
                || settings.video.format != self.last_video_format
                || settings.video.fps != self.last_video_fps;

            if settings.video.device_name.is_some() && (need_video_restart || initial) {
                self.video_retry.request(video_target(&settings));
            }

            // 色空間とレンジはデバイスの開き直しを伴わない。共有の Atomic へ
            // 書くだけで次のフレームから効くので、ここで反映する。
            // 2 秒ごとに video_capture のロックを取らないよう差分で判定する
            let color_conversion = (settings.video.color_space, settings.video.color_range);
            if Self::needs_reapply(initial, &color_conversion, &self.last_color_conversion) {
                if let Ok(video) = self.video_capture.lock() {
                    video.set_color_conversion(color_conversion.0, color_conversion.1);
                    self.last_color_conversion = Some(color_conversion);
                } else {
                    // 次の適用タイミングで入れ直す
                    warn!("色変換の設定で video_capture のロックを取得できない");
                    self.last_color_conversion = None;
                }
            }

            // Audio
            //
            // 映像と同じく、ここでは要求を立てるだけ。パススルーの有効・無効と
            // 音量は開き直しを伴わないので、その場で反映する
            let previous_volume = self.volume;
            if let Ok(mut audio) = self.audio_capture.lock() {
                // パススルーと音量は、下の audio_retry.request より前に反映する。
                // ストリームを開いたあとに反映すると、無効のまま（あるいは
                // 音量 0% で）起動したときに最初のバッファだけ出力されてしまう。
                // 実際に開く try_connect_audio でも開く直前に入れ直している
                audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);

                // 音量を適用
                self.volume = settings.ui.volume;
                audio.set_volume(self.volume);

                // ミュートも同じ扱い。ストリームの開き直しは伴わない
                self.muted = settings.ui.muted;
                audio.set_muted(self.muted);
            }

            // 設定ダイアログの「適用」「OK」で音量が変わったときも OSD を出す。
            // ホイールや右クリックメニューでの変更は設定側も同時に更新しているため、
            // 2 秒ごとの再適用ではここに入らず、OSD が出っぱなしにはならない。
            // 起動時は変更ではないので出さない
            if !initial && (self.volume - previous_volume).abs() > 0.01 {
                self.show_volume_overlay();
            }

            // 出力デバイスも比較する。入れないと、設定画面で出力先だけを
            // 変えたときに要求が立たず、古い出力先のまま鳴り続ける
            let need_audio_restart = settings.audio.input_device_name != self.last_audio_device
                || settings.audio.output_device_name != self.last_audio_output
                || settings.audio.sample_rate != self.last_audio_rate
                || settings.audio.channels != self.last_audio_channels
                || initial; // 起動時は必ず接続試行

            if need_audio_restart {
                self.audio_retry.request(audio_target(&settings));
            }

            // UI設定
            self.maintain_aspect_ratio = settings.ui.maintain_aspect_ratio;
            self.always_on_top = settings.ui.always_on_top;
            self.show_stats_overlay = settings.ui.show_stats_overlay;
            // 装飾の有無は値を取り込むだけで、ここでは ViewportCommand を送らない。
            // 実際の切替は右クリックメニュー（set_borderless）と起動時の
            // ViewportBuilder が行う。2 秒ごとにコマンドを送ると、フルスクリーン中に
            // 装飾を付け直そうとして表示がちらつく
            self.borderless = settings.ui.borderless;

            // ホットキーの割り当て。
            //
            // 差分は `HotkeyManager::apply` が取る。無条件に登録し直すと、
            // 2 秒ごとに unregister → register が走ってその瞬間のキー入力を
            // 取りこぼす
            self.apply_hotkey_assignments(&settings.hotkeys);

            // スクリーンショットの効果音
            //
            // **`None`（クリア）も差分として扱う。** 以前は `if let Some(..)` で
            // 包んでいたため、設定画面で「クリア」してもそのセッション中は
            // 効果音が鳴り続けていた
            if let Ok(mut ss) = self.screenshot_manager.lock() {
                // 無条件に呼ぶと 2 秒ごとに効果音ファイル全体を読み直すことになる
                if Self::needs_reapply(
                    initial,
                    &settings.screenshot.sound_file,
                    &self.last_sound_file,
                ) {
                    match &settings.screenshot.sound_file {
                        Some(sf) => match ss.set_sound_file(sf) {
                            Ok(()) => self.last_sound_file = Some(Some(sf.clone())),
                            // 見つからない場合は埋め込みの既定音へ倒して Ok になる。
                            // ここへ来るのはファイルがあるのに読めなかった場合なので、
                            // last を空にして次の適用タイミングで読み直す
                            Err(_) => self.last_sound_file = None,
                        },
                        None => {
                            // 未選択は「鳴らさない」の意味。set_sound_file は
                            // 見つからないファイルを既定音へ倒すので、無音は
                            // ここでしか表せない
                            ss.clear_sound();
                            self.last_sound_file = Some(None);
                        }
                    }
                }
            }
        }

        if !initial {
            self.last_settings_applied = Instant::now();
        }
    }

    /// 設定ダイアログの操作を処理する。
    ///
    /// ドラフトの反映・保存・クローズをここで行うのは、UI 側に状態と副作用を
    /// 持たせないため（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
    fn handle_settings_dialog_action(&mut self, action: ui::SettingsDialogAction) {
        if action == ui::SettingsDialogAction::TestSound {
            self.play_test_sound();
        }

        let transition = ui::SettingsDialogState::transition_for(action);

        if transition.commit_draft {
            if let Ok(mut settings) = self.settings.lock() {
                self.settings_dialog.commit_into(&mut settings);
            }
            // 反映した内容でデバイスを開き直す
            self.apply_settings(false);
        }

        if transition.save_to_file {
            // 「適用」と「OK」はユーザーの明示的な保存操作なので、
            // デバウンスを待たずに書き出す。読めなかった設定ファイルが
            // 残っている場合も、上書きするかはユーザーが決めることなので止めない
            let saved = self.save_settings_now();
            // 明示的な保存が通ったなら、守るべき壊れたファイルはもう無い。
            // 以降はウィンドウ位置や音量の自動保存も通常どおり行う
            self.autosave.note_explicit_save(saved);
        }

        if transition.close {
            self.settings_dialog.end_edit();
            self.show_settings = false;
        }
    }

    /// 共有設定のホットキー割り当てを、その場で登録し直す。
    ///
    /// 2 秒ごとの `apply_settings` を待たずに反映したい経路（設定ダイアログを
    /// 閉じた状態でホットキー入力ダイアログだけを操作した場合）で使う。
    fn apply_hotkeys_now(&mut self) {
        // 登録はデバイスを開くような重い処理ではないが、`apply` の中で
        // ログを出すため settings のロックは先に手放しておく
        let desired = match self.settings.lock() {
            Ok(settings) => settings.hotkeys.clone(),
            Err(_) => {
                warn!("ホットキーの適用で settings のロックを取得できない");
                return;
            }
        };
        self.apply_hotkey_assignments(&desired);
    }

    /// ホットキーの割り当てを登録し直し、失敗を画面へ出す。
    ///
    /// **`HotkeyManager::apply` を直接呼ばないこと。** 直接呼ぶと、失敗の
    /// 通知と、直ったときのエラー表示の取り下げが抜ける。
    fn apply_hotkey_assignments(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        self.hotkey_manager.apply(desired);

        // 登録できないものが残っているかは apply のあとにまとめて見る。
        // 1 件ずつ通知すると、複数まとめて失敗したときにトーストが
        // 上書きされて最後の 1 件しか読めない
        let summary = hotkey_error_summary(self.hotkey_manager.errors());
        match summary {
            Some(reason) => self.report_error(ErrorSource::Hotkey, reason),
            // 他のアプリがキーを離して登録できるようになった場合も通る。
            // 残しておくと、直ったのに接続状態の表示が古いままになる
            None => self.errors.clear(ErrorSource::Hotkey),
        }
    }

    /// 設定ダイアログの「テスト再生」で効果音を鳴らす。
    ///
    /// ダイアログを開いている間はドラフトの音量で鳴らす。スライダーを
    /// 動かした結果をその場で確かめられるようにするため。
    /// 効果音のファイル自体は「適用」か「OK」まで差し替わらない。
    fn play_test_sound(&self) {
        // この操作が返るのはダイアログを描画しているときだけなので、ドラフトは必ずある
        let Some(volume) = self
            .settings_dialog
            .draft()
            .map(|draft| draft.screenshot.sound_volume)
        else {
            return;
        };

        if let Ok(ss) = self.screenshot_manager.lock() {
            ss.play_screenshot_sound(volume);
        }
    }

    /// 設定に未保存の変更があることを記録する。
    /// 実際の書き出しは `flush_settings_if_due` がまとめて行う。
    fn mark_settings_dirty(&mut self) {
        // 自動保存を止めている間は保留として積まない。積むと
        // flush_settings_if_due が書き出す時刻へ再描画を予約し続け、
        // 書き出さないまま 2 秒ごとに起こされることになる
        if !self.autosave.is_allowed() {
            trace!("自動保存を止めているので設定の変更を保留しない");
            return;
        }
        self.settings_dirty_since = Some(Instant::now());
    }

    /// 保留の有無にかかわらず、いま設定をディスクへ書き出す。
    /// 書き出せたかを返す。
    fn save_settings_now(&mut self) -> bool {
        // ロックが取れなかった場合は保留のままにして、次の機会に書き出す
        let Ok(settings) = self.settings.lock() else {
            warn!("設定の保存で settings のロックを取得できない");
            return false;
        };

        if settings.save() {
            self.settings_dirty_since = None;
            true
        } else {
            // 書き出せなかった変更を保存済みとして捨てず、保留のまま残す。
            // 時刻を入れ直しているのは、失敗が続いたときに毎フレーム
            // 書き込みを試みる状態へ戻さないため
            self.settings_dirty_since = Some(Instant::now());
            false
        }
    }

    /// 保留中の設定変更を書き出すべきかを判定する。
    /// `since_last_change` は最後の変更からの経過時間で、
    /// `None` は「保留中の変更が無い」を表す。
    fn should_flush_settings(since_last_change: Option<Duration>) -> bool {
        match since_last_change {
            None => false,
            Some(elapsed) => elapsed >= SETTINGS_SAVE_DEBOUNCE,
        }
    }

    /// 保留中の設定変更を、最後の変更から一定時間が空いていれば書き出す。
    ///
    /// 書き出しはディスク I/O だが、デバウンスにより数秒に 1 回までしか走らない
    /// ため UI スレッドで行っている。ウィンドウのドラッグや音量の連続操作のように
    /// 毎フレーム値が変わる間は、変更が止まるまで 1 度も書き出さない。
    fn flush_settings_if_due(&mut self, ctx: &egui::Context) {
        // 読めなかった設定ファイルが残っている間は書き出さない。
        // mark_settings_dirty 側でも積まないようにしてあるが、
        // 保留を直接立てる経路が増えても止まるようにここでも見る
        if !self.autosave.is_allowed() {
            return;
        }

        let elapsed = self.settings_dirty_since.map(|since| since.elapsed());
        if !Self::should_flush_settings(elapsed) {
            // 書き出す時刻に再描画を予約する。映像が来ていないときは再描画が
            // 止まりうるため、これが無いと update() が呼ばれず書き出しが遅れる
            if self.settings_dirty_since.is_some() {
                ctx.request_repaint_after(SETTINGS_SAVE_DEBOUNCE);
            }
            return;
        }

        self.save_settings_now();
    }

    /// ウィンドウの位置とサイズを設定へ記録してよいかを判定する。
    ///
    /// フルスクリーン中に報告される矩形は画面全体なので、記録すると
    /// 次回起動時に画面全体のサイズで復元されてしまう。
    ///
    /// `app_fullscreen` はアプリが持つフラグ、`viewport_fullscreen` は OS から
    /// 報告された状態（`None` は不明）。`ViewportCommand::Fullscreen` の効果は
    /// 次のフレーム以降に現れるため、解除した直後はアプリ側のフラグが false でも
    /// OS 側はまだフルスクリーンを報告している。**この 1 フレームで記録すると
    /// 画面全体の矩形を掴んでしまうので、両方がフルスクリーンでないときだけ
    /// 記録する。**
    fn should_record_window_geometry(
        app_fullscreen: bool,
        viewport_fullscreen: Option<bool>,
    ) -> bool {
        !app_fullscreen && viewport_fullscreen != Some(true)
    }

    /// デバイスリストのキャッシュを更新すべきかを判定する。
    /// `elapsed` は前回更新からの経過時間で、`None` は「一度も取得していない」を表す。
    fn should_refresh_device_list(elapsed: Option<Duration>) -> bool {
        match elapsed {
            None => true,
            Some(elapsed) => elapsed >= DEVICE_LIST_CACHE_INTERVAL,
        }
    }

    fn update_cached_device_lists(&mut self) {
        // パフォーマンス影響を避けるため一定間隔でのみデバイスリストを更新
        let elapsed = self.last_device_list_update.map(|last| last.elapsed());
        if !Self::should_refresh_device_list(elapsed) {
            return;
        }

        // ビデオデバイスの列挙は MediaFoundation への問い合わせで重いため、
        // オーディオデバイスと同じ間隔でキャッシュする
        self.cached_video_devices = VideoCapture::list_devices();

        if let Ok(audio) = self.audio_capture.lock() {
            self.cached_input_devices = audio.list_input_devices();
            self.cached_output_devices = audio.list_output_devices();
        }

        // ロック取得に失敗した場合も時刻は更新する。
        // 更新しないと次のフレームでビデオデバイスの列挙が再び走ってしまう
        self.last_device_list_update = Some(Instant::now());
    }

    /// 別スレッドから届いたデバイス能力の取得結果を設定ダイアログへ反映する。
    /// キャッシュを触るのは UI スレッドだけなのでロックは要らない。
    fn drain_capability_results(&mut self) {
        while let Ok((device, result)) = self.capability_rx.try_recv() {
            self.settings_dialog
                .capabilities_mut()
                .apply_result(device, result);
        }
    }

    /// 別スレッドから届いたスクリーンショットの保存結果を取り込む。
    ///
    /// 保存は撮影ごとに spawn したスレッドが行うため、失敗をその場で画面に
    /// 出せない。結果をここで受け取って、失敗ならトーストにする。
    ///
    /// **ログは届いた結果すべてについて出す。** 画面の記録は追い越しを弾くが、
    /// ログまで落とすと何が起きたか追えなくなる。
    fn drain_screenshot_results(&mut self) {
        while let Ok((started_at, result)) = self.screenshot_rx.try_recv() {
            match &result {
                Ok(done) => info!("スクリーンショットを{}", done),
                Err(reason) => error!("スクリーンショットを出力できない: {}", reason),
            }
            self.apply_screenshot_outcome(started_at, result);
        }
    }

    /// スクリーンショットの結果を画面の記録へ反映する。
    ///
    /// **先に始めた保存の結果が後から届いても、新しい記録を上書きしない。**
    /// 保存は撮影ごとにスレッドを起動するため、エンコードにかかる時間が
    /// 違えば終わる順も入れ替わる。そのまま反映すると、古い保存の成功が
    /// 新しい保存の失敗を消してしまう。
    ///
    /// ログ出力は呼び出し側が済ませてある。ここは画面へ出す記録だけを扱う。
    fn apply_screenshot_outcome(&mut self, started_at: Instant, result: ScreenshotOutcome) {
        if !screenshot_outcome_supersedes(self.last_screenshot_outcome_at, started_at) {
            debug!("先に始めた保存の結果が後から届いたので、画面の記録は更新しない");
            return;
        }
        self.last_screenshot_outcome_at = Some(started_at);

        match result {
            // 直前の失敗が解消したので記録を消す。残すと古い失敗が出続ける
            Ok(_) => self.errors.clear(ErrorSource::Screenshot),
            Err(reason) => self.report_error(ErrorSource::Screenshot, reason),
        }
    }

    /// 失敗を記録し、必要ならトーストで見せる。
    ///
    /// **ログは呼び出し側が従来どおり出す。** ここは画面へ出すための記録で、
    /// `error!` / `warn!` の置き換えではない。同じ発生源で同じ文言が続く間は
    /// `ErrorCenter` が間引くため、接続の再試行で連打にならない。
    ///
    /// 同じフレームで複数の発生源が失敗した場合、トーストは後に記録したものが
    /// 勝つ（`TransientOverlay` は 1 件しか持たない）。**どれを見せるかを
    /// 優先度で決めない。** 全てログと「接続状態」タブに残っており、
    /// 消えたほうも次の再試行でまた記録されるため。
    fn report_error(&mut self, source: ErrorSource, message: String) {
        let notify = self
            .errors
            .record(source, message, Instant::now(), Local::now());
        if !notify {
            return;
        }
        // 上で記録したので必ず取れる
        let Some(recorded) = self.errors.latest(source) else {
            return;
        };
        let text = status::truncate(
            &status::format_message(source, &recorded.message),
            status::TOAST_MESSAGE_LIMIT,
        );
        self.transient_overlay.show(
            OverlayContent::Text(text),
            ERROR_TOAST_DURATION,
            Instant::now(),
        );
    }

    /// 発生源の直近の失敗を、映像プレースホルダーへ添える 1 行にする。
    fn error_detail(&self, source: ErrorSource) -> Option<String> {
        let recorded = self.errors.latest(source)?;
        Some(status::truncate(
            &status::format_message(source, &recorded.message),
            status::PLACEHOLDER_DETAIL_LIMIT,
        ))
    }

    /// 設定ダイアログの「接続状態」タブへ渡す観測値を作る。
    ///
    /// **ダイアログを開いている間だけ呼ぶ。** ロックは video → audio の順に
    /// 1 つずつ取り、中では小さな構造体の複製しか行わない
    /// （`stats()` / `link_state()` と同じ流儀）。
    fn connection_status(&self) -> ConnectionStatus {
        let active_video = match self.video_capture.lock() {
            Ok(video) => video.active(),
            Err(_) => {
                warn!("接続状態の表示で video_capture のロックを取得できない");
                None
            }
        };
        let active_audio = match self.audio_capture.lock() {
            Ok(audio) => audio.active(),
            Err(_) => {
                warn!("接続状態の表示で audio_capture のロックを取得できない");
                None
            }
        };

        let mut video = LinkStatus {
            connected: active_video.is_some(),
            reconnecting: self.video_retry.is_active(),
            attempts: self.video_retry.attempts(),
            details: Vec::new(),
            error: self.status_error(ErrorSource::Video),
        };
        if let Some(active) = active_video {
            video
                .details
                .push(format!("デバイス: {}", active.device_name));
            video.details.push(format!("映像: {}", active.summary()));
            // 実際の fps はデバイスから取れない（video.rs の start_capture を参照）
            video
                .details
                .push(format!("要求フレームレート: {} fps", active.requested_fps));
        }

        let mut audio = LinkStatus {
            connected: active_audio.is_some(),
            reconnecting: self.audio_retry.is_active(),
            attempts: self.audio_retry.attempts(),
            details: Vec::new(),
            error: self.status_error(ErrorSource::Audio),
        };
        if let Some(active) = active_audio {
            audio.details.push(format!(
                "入力: {}（{}）",
                active.input_device,
                active.input_summary()
            ));
            audio.details.push(format!(
                "出力: {}（{}）",
                active.output_device,
                active.output_summary()
            ));
        }

        ConnectionStatus { video, audio }
    }

    /// 「接続状態」タブに出す直近の失敗。`(整形済みの文言, 発生時刻)`。
    ///
    /// トーストやプレースホルダーと違い、ここでは切り詰めない。
    /// 原因を調べるための場所なので、全文が読めるほうがよい。
    fn status_error(&self, source: ErrorSource) -> Option<(String, String)> {
        let recorded = self.errors.latest(source)?;
        Some((
            status::format_message(source, &recorded.message),
            recorded.time_text(),
        ))
    }

    /// 溜まったデバイス能力の取得要求を、使い捨てのスレッドへ渡す。
    ///
    /// `get_device_capabilities` は `Camera::new` でデバイスを開いたうえで
    /// 3 フォーマット分の対応表を引くため数百 ms 以上かかる。以前は設定ダイアログの
    /// 描画中に直接呼んでいたため、デバイスを切り替えるたびにアプリ全体が固まっていた。
    fn dispatch_capability_requests(&mut self) {
        for device in self.settings_dialog.capabilities_mut().take_requests() {
            let tx = self.capability_tx.clone();
            let name = device.clone();
            let spawned = std::thread::Builder::new()
                .name("capability-query".to_string())
                .spawn(move || {
                    let started = Instant::now();
                    let result = VideoCapture::get_device_capabilities(Some(&name));
                    match &result {
                        Ok(caps) => info!(
                            "デバイス能力を取得した: {}（{} フォーマット, {} ms）",
                            name,
                            caps.len(),
                            started.elapsed().as_millis()
                        ),
                        Err(e) => warn!("デバイス能力を取得できない: {}: {}", name, e),
                    }
                    if tx.send((name, result)).is_err() {
                        // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                        debug!("デバイス能力の送り先が既に無いので結果を捨てる");
                    }
                });

            if let Err(e) = spawned {
                warn!("デバイス能力を取得するスレッドを起動できない: {}", e);
                // 投げられなかった要求を Pending のまま残すと、再取得もできずに
                // 「取得中...」が出続ける
                self.settings_dialog.capabilities_mut().apply_result(
                    device,
                    Err(format!("取得用のスレッドを起動できませんでした: {}", e)),
                );
            }
        }
    }

    fn get_cached_video_devices(&mut self) -> &Vec<(String, String)> {
        self.update_cached_device_lists();
        &self.cached_video_devices
    }

    fn get_cached_input_devices(&mut self) -> &Vec<String> {
        self.update_cached_device_lists();
        &self.cached_input_devices
    }

    fn get_cached_output_devices(&mut self) -> &Vec<String> {
        self.update_cached_device_lists();
        &self.cached_output_devices
    }

    /// 最前面表示を切り替える。
    ///
    /// 右クリックメニューのチェックボックスと同じことを行う。ウィンドウレベルの
    /// 適用、設定への反映、デバウンス保存までを 1 か所にまとめてある。
    fn set_always_on_top(&mut self, ctx: &egui::Context, enabled: bool) {
        self.always_on_top = enabled;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if enabled {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));

        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.always_on_top = enabled;
        }
        info!(
            "最前面表示を{}にした",
            if enabled { "オン" } else { "オフ" }
        );
        self.mark_settings_dirty();
    }

    /// タイトルバーと枠の表示を切り替える。
    ///
    /// **装飾を外すときは「画面ドラッグ移動」も併せて見る。** どちらも無い状態に
    /// すると、ウィンドウを動かす手段が残らない。自動で有効にしたうえで、
    /// 設定を勝手に変えたことを OSD で伝える。
    fn set_borderless(&mut self, ctx: &egui::Context, enabled: bool) {
        self.borderless = enabled;
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!enabled));

        let mut enabled_drag_move = false;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.borderless = enabled;
            if needs_drag_move_guard(enabled, settings.ui.enable_drag_move) {
                settings.ui.enable_drag_move = true;
                enabled_drag_move = true;
            }
        } else {
            warn!("タイトルバーの切替で settings のロックを取得できない");
        }

        info!(
            "タイトルバーの表示を{}にした",
            if enabled { "オフ" } else { "オン" }
        );
        self.mark_settings_dirty();

        if enabled_drag_move {
            info!("ウィンドウを動かせなくなるため、画面ドラッグ移動を自動で有効にした");
            self.transient_overlay.show(
                OverlayContent::Text(DRAG_MOVE_GUARD_MESSAGE.to_string()),
                DRAG_MOVE_GUARD_OSD_DURATION,
                Instant::now(),
            );
        }
    }

    /// 装飾なしのときに、ウィンドウ端のドラッグでリサイズを始める。
    ///
    /// 戻り値は「ポインタがいまリサイズ用の帯にいるか」。**`true` の間、
    /// 呼び出し側は映像のドラッグによるウィンドウ移動を行わない。** 端を掴んだ
    /// つもりでウィンドウごと動いてしまうため。
    ///
    /// メニューやダイアログが開いている間は何もしない。ウィンドウ端に重なった
    /// ボタンを押そうとしてリサイズが始まるのを防ぐ。
    fn handle_borderless_resize(&self, ctx: &egui::Context) -> bool {
        if !self.borderless || self.is_fullscreen {
            return false;
        }
        if self.show_context_menu || self.show_settings || self.show_hotkey_dialog {
            return false;
        }

        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
            return false;
        };
        let Some(direction) = resize_direction_at(pos, ctx.screen_rect(), RESIZE_BORDER) else {
            return false;
        };

        ctx.set_cursor_icon(resize_cursor(direction));

        if ctx.input(|i| i.pointer.primary_pressed()) {
            trace!("装飾なしのウィンドウ端をつかんだ: {:?}", direction);
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }

        true
    }

    /// ウィンドウの大きさを既定に戻す。
    ///
    /// 装飾なしでは端の帯でしかリサイズできず、小さくしすぎると掴む場所を
    /// 見失う。そこからの復帰手段として右クリックメニューに置いてある。
    fn reset_window_size(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            DEFAULT_WINDOW_SIZE.0,
            DEFAULT_WINDOW_SIZE.1,
        )));
        info!(
            "ウィンドウサイズを既定（{}x{}）に戻した",
            DEFAULT_WINDOW_SIZE.0, DEFAULT_WINDOW_SIZE.1
        );
        // 設定への記録は update() のウィンドウ監視が拾う。ここで書くと
        // OS が要求どおりの大きさにできなかった場合に実際とずれる
    }

    /// 音量を `delta`%（負なら下げる）変える。
    ///
    /// 反映は `set_volume_from_ui` に任せる。映像上のホイール操作や
    /// 右クリックメニューのスライダーと同じ経路を通すことで、設定への
    /// 反映も OSD の表示も同じになる。
    fn adjust_volume(&mut self, delta: f32) {
        let (volume, muted) = volume_change_result(self.volume, delta);
        if self.muted != muted {
            // 解除の OSD は出さない。直後の音量 OSD が新しい状態を示す
            self.apply_muted(muted);
        }
        self.set_volume_from_ui(volume);
    }

    /// ミュートの状態を反映する。設定へ書き、音声へ伝えるところまで。
    ///
    /// **OSD はここでは出さない。** 音量変更に巻き込まれた解除では、
    /// ミュートの OSD ではなく音量の OSD を出したいため。
    ///
    /// ロックは settings → audio の順に 1 つずつ取り、重ねない。
    fn apply_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.muted = muted;
        }
        if let Ok(mut audio) = self.audio_capture.lock() {
            audio.set_muted(muted);
        }
        self.mark_settings_dirty();
    }

    /// UI の操作でミュートが変わったときの共通処理。反映して OSD を出す。
    fn set_muted_from_ui(&mut self, muted: bool) {
        self.apply_muted(muted);
        info!("ミュートを{}にした", if muted { "オン" } else { "オフ" });
        self.transient_overlay.show(
            mute_overlay_content(muted, self.volume),
            VOLUME_OSD_DURATION,
            Instant::now(),
        );
    }

    /// ミュートを切り替える。
    ///
    /// 右クリックメニューのチェック、映像上のミドルクリック、ホットキーが
    /// すべてここを通る。
    fn toggle_mute(&mut self) {
        self.set_muted_from_ui(!self.muted);
    }

    /// デバイスを強制的に開き直す。右クリックメニューの「デバイス再接続」と同じ。
    fn reconnect_devices(&mut self) {
        info!("デバイスの再接続を要求された");
        // 強制的にデバイス再接続（last_*をクリアして強制再接続）
        self.last_video_device = None;
        self.last_audio_device = None;
        // 途絶の記録も落とす。開き直したあとの途絶を、改めて
        // 検出してログに残せるようにする
        self.last_video_link_action = VideoLinkAction::Keep;
        // 保留していた音声のエラーも、ここで開き直すので落とす
        self.audio_stream_error_pending = false;
        // ユーザーが明示的にやり直しを求めているので、
        // バックオフの待ち時間を飛ばして次のフレームで試す
        if let Ok(settings) = self.settings.lock() {
            self.video_retry.request_now(video_target(&settings));
            self.audio_retry.request_now(audio_target(&settings));
        }
        self.apply_settings(false);
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context, to_full: bool) {
        use eframe::egui::ViewportCommand;

        if to_full {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
            self.is_fullscreen = true;
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.is_fullscreen = false;
        }

        let text = if self.is_fullscreen {
            "フルスクリーン ON"
        } else {
            "フルスクリーン OFF"
        };
        self.transient_overlay.show(
            OverlayContent::Text(text.to_string()),
            FULLSCREEN_OSD_DURATION,
            Instant::now(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;
    use tempfile::tempdir;
    use video::IntervalStats;

    /// 音量 OSD のバーの中身を取り出す。テキスト以外の形で返ってきたら落とす
    fn volume_bar(volume: f32) -> (String, f32, f32) {
        let (text, ratio, marker_ratio, _) = volume_bar_with_mute(volume, false);
        (text, ratio, marker_ratio)
    }

    /// ミュート状態を指定して音量 OSD のバーの中身を取り出す。
    /// 最後の要素は「灰色で描くか」
    fn volume_bar_with_mute(volume: f32, muted: bool) -> (String, f32, f32, bool) {
        match volume_overlay_content(volume, muted) {
            OverlayContent::Bar {
                text,
                ratio,
                marker_ratio,
                dimmed,
            } => (text, ratio, marker_ratio, dimmed),
            other => panic!("音量 OSD がバー付きになっていない: {:?}", other),
        }
    }

    /// ミュート OSD の文言を取り出す。バーが付いていたら落とす
    fn mute_text(muted: bool, volume: f32) -> String {
        match mute_overlay_content(muted, volume) {
            OverlayContent::Text(text) => text,
            other => panic!("ミュート OSD がテキストになっていない: {:?}", other),
        }
    }

    #[test]
    fn volume_overlay_content_shows_percentage_and_ratio() {
        let (text, ratio, marker_ratio) = volume_bar(80.0);

        assert_eq!(text, "音量: 80%");
        // 0〜200% を全体とするので 80% は 0.4、目盛りの 100% は 0.5
        assert!((ratio - 0.4).abs() < 1e-6, "バーの長さが違う: {}", ratio);
        assert!(
            (marker_ratio - 0.5).abs() < 1e-6,
            "目盛りの位置が違う: {}",
            marker_ratio
        );
    }

    #[test]
    fn volume_overlay_content_at_minimum_is_empty_bar() {
        let (text, ratio, _) = volume_bar(0.0);

        assert_eq!(text, "音量: 0%");
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn volume_overlay_content_at_maximum_fills_bar() {
        let (text, ratio, _) = volume_bar(200.0);

        assert_eq!(text, "音量: 200%");
        assert_eq!(ratio, 1.0);
    }

    #[test]
    fn volume_overlay_content_rounds_down_like_context_menu() {
        // 右クリックメニューの「音量: N%」と同じ丸め方であること。
        // 食い違うと、スライダーを動かしている間だけ 1% ずれて見える
        let (text, _, _) = volume_bar(79.6);

        assert_eq!(text, "音量: 79%");
    }

    #[test]
    fn volume_overlay_content_while_muted_is_dimmed_and_labelled() {
        // ミュート中でも数字は残す。解除したときに戻る音量が見えているほうが
        // 操作の結果を予想しやすい
        let (text, ratio, _, dimmed) = volume_bar_with_mute(80.0, true);

        assert_eq!(text, "音量: 80%（ミュート中）");
        assert!((ratio - 0.4).abs() < 1e-6, "バーの長さが違う: {}", ratio);
        assert!(dimmed);
    }

    #[test]
    fn volume_overlay_content_without_mute_is_not_dimmed() {
        let (_, _, _, dimmed) = volume_bar_with_mute(80.0, false);

        assert!(!dimmed);
    }

    #[test]
    fn mute_overlay_content_muted_shows_only_the_state() {
        assert_eq!(mute_text(true, 80.0), "ミュート");
    }

    #[test]
    fn mute_overlay_content_unmuted_shows_restored_volume() {
        assert_eq!(mute_text(false, 80.0), "ミュート解除（音量: 80%）");
    }

    #[test]
    fn mute_overlay_content_rounds_volume_like_the_volume_osd() {
        // 音量 OSD と丸め方を揃える。食い違うと解除の前後で 1% ずれて見える
        assert_eq!(mute_text(false, 79.6), "ミュート解除（音量: 79%）");
    }

    #[test]
    fn volume_change_result_releases_mute() {
        // ホイールや音量ホットキーで音量を変えたらミュートは解除する
        let (volume, muted) = volume_change_result(50.0, VOLUME_SCROLL_STEP);

        assert_eq!(volume, 60.0);
        assert!(!muted);
    }

    #[test]
    fn volume_change_result_clamps_to_maximum() {
        let (volume, _) = volume_change_result(MAX_VOLUME, VOLUME_SCROLL_STEP);

        assert_eq!(volume, MAX_VOLUME);
    }

    #[test]
    fn volume_change_result_clamps_to_minimum() {
        let (volume, _) = volume_change_result(MIN_VOLUME, -VOLUME_SCROLL_STEP);

        assert_eq!(volume, MIN_VOLUME);
    }

    #[test]
    fn volume_change_result_from_muted_state_still_releases_mute() {
        // 下げる方向でも解除する。「鳴らないまま下げ続ける」状態を作らない
        let (volume, muted) = volume_change_result(50.0, -VOLUME_SCROLL_STEP);

        assert_eq!(volume, 40.0);
        assert!(!muted);
    }

    #[test]
    fn format_stats_lines_without_frames_shows_no_numbers() {
        // デバイスに接続できていない状態。0 除算の結果や NaN を
        // そのまま画面へ出さないことを確かめる
        let lines = format_stats_lines(&FrameStats::default());
        let joined = lines.join(
            "
",
        );

        assert!(joined.contains("FPS -"), "FPS が出ていない: {}", joined);
        assert!(
            joined.contains("デコード -"),
            "計測前の 0 を数値で出している: {}",
            joined
        );
        assert!(joined.contains("映像フレームなし"), "{}", joined);
        assert!(
            !joined.contains("NaN"),
            "NaN が表示に混ざっている: {}",
            joined
        );
        assert!(
            !joined.contains("inf"),
            "inf が表示に混ざっている: {}",
            joined
        );
        assert!(
            !joined.contains("最終フレーム"),
            "フレームが無いのに経過時間が出ている: {}",
            joined
        );
    }

    #[test]
    fn format_stats_lines_with_frames_shows_all_items() {
        // 60fps 相当で動いている状態
        let stats = FrameStats {
            intervals: Some(IntervalStats {
                fps: 60.0,
                average_ms: 16.6667,
                min_ms: 15.0,
                max_ms: 18.0,
                stddev_ms: 1.25,
                samples: 120,
            }),
            last_decode_ms: 2.5,
            fast_count: 1200,
            fallback_count: 3,
            resolution: Some((1920, 1080)),
            source_format: Some("YUY2"),
            since_last_frame_ms: Some(12.4),
        };

        let lines = format_stats_lines(&stats);
        let joined = lines.join(
            "
",
        );

        assert!(joined.contains("FPS 60.0"), "{}", joined);
        assert!(joined.contains("120 件"), "{}", joined);
        assert!(joined.contains("±1.25ms"), "{}", joined);
        assert!(joined.contains("最小 15.0 / 最大 18.0"), "{}", joined);
        assert!(joined.contains("デコード 2.50ms"), "{}", joined);
        assert!(joined.contains("高速 1200 / 汎用 3"), "{}", joined);
        assert!(joined.contains("1920x1080 YUY2"), "{}", joined);
        assert!(joined.contains("最終フレーム 12ms 前"), "{}", joined);
    }

    #[test]
    fn backoff_delay_first_failure_waits_base_interval() {
        // 1 回目の失敗の後は 200ms。固定 2 秒待ちの代わりになる短さであること
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
    }

    #[test]
    fn backoff_delay_doubles_until_it_reaches_the_cap() {
        // 期待値は表としてベタ書きする。実装と同じ式で作ると、式が誤っていても通る
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        assert_eq!(backoff_delay(3), Duration::from_millis(800));
        assert_eq!(backoff_delay(4), Duration::from_millis(1600));
        assert_eq!(backoff_delay(5), Duration::from_millis(3200));
    }

    #[test]
    fn backoff_delay_beyond_the_cap_stays_at_the_cap() {
        // 6 回目は倍にすると 6400ms になるので頭打ちの 5000ms へ落ちる
        assert_eq!(backoff_delay(6), Duration::from_millis(5000));
        assert_eq!(backoff_delay(7), Duration::from_millis(5000));
        assert_eq!(backoff_delay(100), Duration::from_millis(5000));
    }

    #[test]
    fn backoff_delay_huge_attempt_count_does_not_overflow() {
        // 無限に再試行するので attempts は際限なく増える。
        // シフト量が u32 の幅を超えてもパニックしないこと
        assert_eq!(backoff_delay(31), Duration::from_millis(5000));
        assert_eq!(backoff_delay(32), Duration::from_millis(5000));
        assert_eq!(backoff_delay(33), Duration::from_millis(5000));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_millis(5000));
    }

    #[test]
    fn backoff_delay_zero_attempts_is_zero() {
        // まだ 1 度も失敗していない状態。待たずに試す
        assert_eq!(backoff_delay(0), Duration::ZERO);
    }

    #[test]
    fn should_retry_now_without_deadline_returns_true() {
        // 期限が無い＝いますぐ試してよい。初回接続がこれに当たる
        assert!(should_retry_now(None, Instant::now()));
    }

    #[test]
    fn should_retry_now_before_deadline_returns_false() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(200);
        assert!(!should_retry_now(Some(deadline), now));
    }

    #[test]
    fn should_retry_now_exactly_at_deadline_returns_true() {
        // 境界。期限ちょうどでは試す
        let now = Instant::now();
        assert!(should_retry_now(Some(now), now));
    }

    #[test]
    fn should_retry_now_after_deadline_returns_true() {
        let deadline = Instant::now();
        let now = deadline + Duration::from_millis(1);
        assert!(should_retry_now(Some(deadline), now));
    }

    #[test]
    fn connect_retry_request_with_the_same_target_keeps_the_backoff() {
        // 2 秒ごとの再適用が、進行中のバックオフを巻き戻してしまう不具合の再現。
        // 同じ対象を追いかけ続けている間は、待ち時間も失敗回数も維持されること
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        retry.record_failure(now);
        assert_eq!(retry.attempts(), 3);

        // 設定は何も変わっていないのに再度要求された状況
        retry.request("デバイス A");

        assert_eq!(retry.attempts(), 3, "失敗回数が巻き戻らない");
        assert!(!retry.is_due(now), "待ち時間も巻き戻らない");
        assert!(
            !retry.is_due(now + Duration::from_millis(799)),
            "3 回失敗したので 800ms 待ち続ける"
        );
        assert!(retry.is_due(now + Duration::from_millis(800)));
    }

    #[test]
    fn connect_retry_request_with_a_different_target_retries_immediately() {
        // 設定画面でデバイスを変えたときは、前の対象のバックオフを引きずらない
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        assert!(!retry.is_due(now));

        retry.request("デバイス B");

        assert_eq!(retry.attempts(), 0, "対象が変われば数え直す");
        assert!(retry.is_due(now), "すぐ試す");
    }

    #[test]
    fn connect_retry_request_now_ignores_the_backoff() {
        // 右クリックの「デバイス再接続」。同じ対象でも待たずに試す
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_failure(now);
        assert!(!retry.is_due(now));

        retry.request_now("デバイス A");

        assert_eq!(retry.attempts(), 0);
        assert!(retry.is_due(now));
    }

    #[test]
    fn connect_retry_request_after_success_starts_a_new_cycle() {
        // 一度成功した対象をもう一度要求したら、また試しにいくこと
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_success();
        assert!(!retry.is_due(now));

        retry.request("デバイス A");
        assert!(retry.is_due(now), "成功済みでも要求されたら試す");
    }

    #[test]
    fn connect_retry_new_is_not_due() {
        // 接続を要求していない間は毎フレームの判定を素通りする
        let retry = ConnectRetry::<&str>::default();
        assert!(!retry.is_due(Instant::now()));
    }

    #[test]
    fn connect_retry_request_is_due_immediately() {
        // 固定 2 秒待ちの廃止そのもの。要求した時点で試せること
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        assert!(retry.is_due(Instant::now()));
    }

    #[test]
    fn connect_retry_failure_blocks_until_the_backoff_elapses() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);

        assert!(!retry.is_due(now), "失敗直後は待つ");
        assert!(
            !retry.is_due(now + Duration::from_millis(199)),
            "200ms の手前ではまだ待つ"
        );
        assert!(
            retry.is_due(now + Duration::from_millis(200)),
            "200ms 経てば試す"
        );
    }

    #[test]
    fn connect_retry_consecutive_failures_widen_the_interval() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");

        retry.record_failure(now);
        assert!(!retry.is_due(now + Duration::from_millis(199)));

        retry.record_failure(now);
        assert!(
            !retry.is_due(now + Duration::from_millis(399)),
            "2 回目の失敗では 400ms 待つ"
        );
        assert!(retry.is_due(now + Duration::from_millis(400)));
    }

    #[test]
    fn connect_retry_success_stops_further_attempts() {
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        retry.record_failure(now);
        retry.record_success();

        assert!(
            !retry.is_due(now + Duration::from_secs(60)),
            "成功したら次のフレーム以降は試さない"
        );
    }

    #[test]
    fn connect_retry_success_resets_the_interval() {
        // 一度成功してから再び要求したときに、前回の attempts を引きずらないこと
        let now = Instant::now();
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        for _ in 0..8 {
            retry.record_failure(now);
        }
        retry.record_success();

        retry.request("デバイス A");
        retry.record_failure(now);
        assert!(
            retry.is_due(now + Duration::from_millis(200)),
            "再要求後は 200ms から数え直す"
        );
    }

    #[test]
    fn connect_retry_new_is_not_active() {
        // 要求していない状態を「再接続中」と表示しないこと
        let retry = ConnectRetry::<&str>::default();
        assert!(!retry.is_active());
    }

    #[test]
    fn connect_retry_is_active_until_it_succeeds() {
        let mut retry = ConnectRetry::default();
        retry.request("デバイス A");
        assert!(retry.is_active());

        retry.record_failure(Instant::now());
        // 失敗しても追いかけ続けている間は「再接続中」
        assert!(retry.is_active());

        retry.record_success();
        assert!(!retry.is_active());
    }

    /// 映像リンクの観測値を組み立てる補助。
    fn link_state(capturing: bool, since_last_frame: Option<Duration>) -> video::VideoLinkState {
        video::VideoLinkState {
            capturing,
            since_last_frame,
        }
    }

    #[test]
    fn decide_video_link_not_capturing_keeps_current_state() {
        // ストリームを開けていない間の面倒は ConnectRetry が見る。
        // ここで手を出すと起動時の接続と二重になる
        let state = link_state(false, Some(Duration::from_secs(60)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_no_frame_yet_keeps_current_state() {
        // 開いた直後は 1 枚目まで実測で 0.8 秒かかる。入力信号が無いデバイスは
        // 開けても永久にフレームを出さないので、切断と見なすと開き直しが止まらない
        let state = link_state(true, None);
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_just_before_timeout_keeps_current_state() {
        let state = link_state(true, Some(Duration::from_millis(2999)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::Keep
        );
    }

    #[test]
    fn decide_video_link_exactly_at_timeout_disconnects() {
        // 境界は切断とみなす側に倒す
        let state = link_state(true, Some(Duration::from_secs(3)));
        assert_eq!(
            decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn decide_video_link_after_timeout_without_auto_reconnect_only_clears() {
        // 自動再接続を切っていても、止まった画を残し続けない
        let state = link_state(true, Some(Duration::from_secs(10)));
        assert_eq!(
            decide_video_link(state, false, VIDEO_SIGNAL_TIMEOUT),
            VideoLinkAction::ClearTexture
        );
    }

    #[test]
    fn decide_video_link_zero_timeout_disconnects_on_any_gap() {
        // 閾値を 0 にした場合、経過が 0 でも切断側へ倒れる（境界の確認）
        let state = link_state(true, Some(Duration::ZERO));
        assert_eq!(
            decide_video_link(state, true, Duration::ZERO),
            VideoLinkAction::ClearTextureAndReconnect
        );
    }

    #[test]
    fn video_placeholder_message_capturing_says_no_signal() {
        // デバイスは開けている。ユーザーが見るべきは入力機器側
        assert_eq!(
            video_placeholder_message(true, false),
            "映像信号がありません"
        );
        // 開けている間は再接続の有無で文言を変えない
        assert_eq!(
            video_placeholder_message(true, true),
            "映像信号がありません"
        );
    }

    #[test]
    fn video_placeholder_message_not_capturing_says_device_is_gone() {
        assert_eq!(
            video_placeholder_message(false, false),
            "デバイスが接続されていません"
        );
        assert_eq!(
            video_placeholder_message(false, true),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn screenshot_outcome_supersedes_without_previous_result_returns_true() {
        assert!(screenshot_outcome_supersedes(None, Instant::now()));
    }

    #[test]
    fn screenshot_outcome_supersedes_newer_result_returns_true() {
        let first = Instant::now();
        assert!(screenshot_outcome_supersedes(
            Some(first),
            first + Duration::from_millis(1)
        ));
    }

    #[test]
    fn screenshot_outcome_supersedes_older_result_returns_false() {
        // 先に始めた保存が後から終わった場合。新しい記録を上書きさせない
        let first = Instant::now();
        let second = first + Duration::from_millis(50);
        assert!(!screenshot_outcome_supersedes(Some(second), first));
    }

    #[test]
    fn screenshot_outcome_supersedes_same_instant_returns_true() {
        // 境界。取りこぼすより出すほうに倒す
        let now = Instant::now();
        assert!(screenshot_outcome_supersedes(Some(now), now));
    }

    #[test]
    fn summarize_screenshot_delivery_file_only_reports_the_path() {
        let outcome =
            summarize_screenshot_delivery(None, Some(Ok(PathBuf::from(r"C:\shots\a.jpg"))));

        assert_eq!(outcome, Ok(r"C:\shots\a.jpg へ保存した".to_string()));
    }

    #[test]
    fn summarize_screenshot_delivery_clipboard_only_reports_the_copy() {
        let outcome = summarize_screenshot_delivery(Some(Ok(())), None);

        assert_eq!(outcome, Ok("クリップボードへコピーした".to_string()));
    }

    #[test]
    fn summarize_screenshot_delivery_both_reports_the_copy_before_the_path() {
        // 実際の処理順（クリップボード → ファイル）と同じ並びにする
        let outcome =
            summarize_screenshot_delivery(Some(Ok(())), Some(Ok(PathBuf::from(r"C:\shots\a.png"))));

        assert_eq!(
            outcome,
            Ok(r"クリップボードへコピーし、C:\shots\a.png へ保存した".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_clipboard_failure_keeps_the_saved_path_in_the_reason() {
        // 片方だけ失敗した場合。全体は失敗だが、成功したほうも文言に残す。
        // クリップボードに入っていないことと、ファイルは残っていることの
        // 両方が分からないと、ユーザーは次に何をすればよいか決められない
        let outcome = summarize_screenshot_delivery(
            Some(Err("クリップボードを開けない: occupied".to_string())),
            Some(Ok(PathBuf::from(r"C:\shots\a.jpg"))),
        );

        assert_eq!(
            outcome,
            Err(r"クリップボードを開けない: occupied（C:\shots\a.jpg へ保存した）".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_file_failure_keeps_the_copy_in_the_reason() {
        let outcome = summarize_screenshot_delivery(
            Some(Ok(())),
            Some(Err(
                r"C:\shots\a.jpg を作成できない: access denied".to_string()
            )),
        );

        assert_eq!(
            outcome,
            Err(
                r"C:\shots\a.jpg を作成できない: access denied（クリップボードへコピーした）"
                    .to_string()
            )
        );
    }

    #[test]
    fn summarize_screenshot_delivery_both_failures_are_joined() {
        let outcome = summarize_screenshot_delivery(
            Some(Err("クリップボードを開けない".to_string())),
            Some(Err("書き込めない".to_string())),
        );

        assert_eq!(
            outcome,
            Err("クリップボードを開けない / 書き込めない".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_without_any_destination_is_an_error() {
        // 出力先の enum が必ずどちらかを含むので通常は起きないが、
        // 何もしていないのに成功として扱わないこと
        assert!(summarize_screenshot_delivery(None, None).is_err());
    }

    #[test]
    fn video_placeholder_text_without_detail_is_the_message_alone() {
        assert_eq!(
            video_placeholder_text(false, true, None),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn video_placeholder_text_adds_the_reason_on_a_second_line() {
        assert_eq!(
            video_placeholder_text(false, true, Some("映像デバイスに接続できません: not found")),
            "デバイスが接続されていません（再接続を試しています）\n映像デバイスに接続できません: not found"
        );
    }

    #[test]
    fn video_placeholder_text_while_capturing_drops_the_reason() {
        // ストリームは開けている＝接続の失敗ではない。古い接続エラーを
        // 出すと、入力機器ではなく USB を疑わせてしまう
        assert_eq!(
            video_placeholder_text(true, false, Some("映像デバイスに接続できません: not found")),
            "映像信号がありません"
        );
    }

    #[test]
    fn decide_video_link_action_changes_when_auto_reconnect_is_turned_on() {
        // 自動再接続を切ったまま途絶したあとに有効化した場合。
        // 呼び出し側は「何をしたか」と比べて動くので、判定が変われば
        // 開き直しへ進める（真偽値のラッチだと遮られてしまう）
        let state = link_state(true, Some(Duration::from_secs(10)));
        let before = decide_video_link(state, false, VIDEO_SIGNAL_TIMEOUT);
        let after = decide_video_link(state, true, VIDEO_SIGNAL_TIMEOUT);

        assert_eq!(before, VideoLinkAction::ClearTexture);
        assert_eq!(after, VideoLinkAction::ClearTextureAndReconnect);
        assert_ne!(before, after);
    }

    #[test]
    fn decide_audio_reconnect_without_pending_error_is_idle() {
        assert_eq!(
            decide_audio_reconnect(false, true, None),
            AudioErrorAction::Idle
        );
        // 保留が無ければ、間隔の下限に達していても何もしない
        assert_eq!(
            decide_audio_reconnect(false, true, Some(Duration::from_secs(600))),
            AudioErrorAction::Idle
        );
    }

    #[test]
    fn decide_audio_reconnect_pending_error_reconnects() {
        assert_eq!(
            decide_audio_reconnect(true, true, None),
            AudioErrorAction::Reconnect
        );
    }

    #[test]
    fn decide_audio_reconnect_without_auto_reconnect_waits() {
        // 見送るだけで、保留は呼び出し側に残る。捨てると音が戻らなくなる
        assert_eq!(
            decide_audio_reconnect(true, false, None),
            AudioErrorAction::Wait
        );
    }

    #[test]
    fn decide_audio_reconnect_within_minimum_interval_waits() {
        assert_eq!(
            decide_audio_reconnect(true, true, Some(Duration::from_millis(4999))),
            AudioErrorAction::Wait
        );
    }

    #[test]
    fn decide_audio_reconnect_after_minimum_interval_reconnects() {
        // 下限に達したフレームで、保留していたエラーが処理される
        assert_eq!(
            decide_audio_reconnect(true, true, Some(Duration::from_secs(5))),
            AudioErrorAction::Reconnect
        );
    }

    #[test]
    fn should_reconnect_after_stream_error_first_time_returns_true() {
        // 一度も開き直していないなら待たせない
        assert!(should_reconnect_after_stream_error(None));
    }

    #[test]
    fn should_reconnect_after_stream_error_just_reconnected_returns_false() {
        assert!(!should_reconnect_after_stream_error(Some(
            Duration::from_millis(10)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_just_before_interval_returns_false() {
        assert!(!should_reconnect_after_stream_error(Some(
            Duration::from_millis(4999)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_at_interval_returns_true() {
        assert!(should_reconnect_after_stream_error(Some(
            Duration::from_secs(5)
        )));
    }

    #[test]
    fn should_reconnect_after_stream_error_long_after_returns_true() {
        assert!(should_reconnect_after_stream_error(Some(
            Duration::from_secs(600)
        )));
    }

    #[test]
    fn should_refresh_device_list_never_updated_returns_true() {
        // 一度も列挙していない状態では必ず取得する
        assert!(CaptureCardViewer::should_refresh_device_list(None));
    }

    #[test]
    fn should_refresh_device_list_just_updated_returns_false() {
        assert!(!CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_secs(0)
        )));
    }

    #[test]
    fn should_refresh_device_list_just_before_interval_returns_false() {
        // 境界の手前。4999ms では更新しない
        assert!(!CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_millis(4999)
        )));
    }

    #[test]
    fn should_refresh_device_list_at_interval_returns_true() {
        // 境界。ちょうど 5000ms で更新する
        assert!(CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_millis(5000)
        )));
    }

    #[test]
    fn should_refresh_device_list_long_after_interval_returns_true() {
        assert!(CaptureCardViewer::should_refresh_device_list(Some(
            Duration::from_secs(3600)
        )));
    }

    #[test]
    fn should_flush_settings_no_pending_change_returns_false() {
        // 保留中の変更が無ければ書き出さない
        assert!(!CaptureCardViewer::should_flush_settings(None));
    }

    #[test]
    fn should_flush_settings_just_changed_returns_false() {
        // 変更した直後は書き出さない（ドラッグ中の毎フレーム書き込みを防ぐ肝）
        assert!(!CaptureCardViewer::should_flush_settings(Some(
            Duration::from_secs(0)
        )));
    }

    #[test]
    fn should_flush_settings_just_before_interval_returns_false() {
        // 境界の手前。1999ms では書き出さない
        assert!(!CaptureCardViewer::should_flush_settings(Some(
            Duration::from_millis(1999)
        )));
    }

    #[test]
    fn should_flush_settings_at_interval_returns_true() {
        // 境界。ちょうど 2000ms で書き出す
        assert!(CaptureCardViewer::should_flush_settings(Some(
            Duration::from_millis(2000)
        )));
    }

    #[test]
    fn should_flush_settings_long_after_interval_returns_true() {
        assert!(CaptureCardViewer::should_flush_settings(Some(
            Duration::from_secs(3600)
        )));
    }

    #[test]
    fn should_record_window_geometry_windowed_returns_true() {
        // 通常のウィンドウ表示中は記録する
        assert!(CaptureCardViewer::should_record_window_geometry(
            false,
            Some(false)
        ));
    }

    #[test]
    fn should_record_window_geometry_fullscreen_returns_false() {
        // フルスクリーン中の矩形は画面全体。記録すると次回起動時に
        // 画面全体サイズで復元されてしまう
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true,
            Some(true)
        ));
    }

    #[test]
    fn should_record_window_geometry_just_entered_fullscreen_returns_false() {
        // フルスクリーンへ入った直後。アプリ側のフラグだけが先に立ち、
        // OS 側はまだウィンドウ表示を報告している
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true,
            Some(false)
        ));
    }

    #[test]
    fn should_record_window_geometry_just_left_fullscreen_returns_false() {
        // フルスクリーンを解除した直後。アプリ側のフラグだけが先に降り、
        // OS 側はまだ画面全体の矩形を報告している
        assert!(!CaptureCardViewer::should_record_window_geometry(
            false,
            Some(true)
        ));
    }

    #[test]
    fn should_record_window_geometry_unknown_viewport_state_follows_app_flag() {
        // OS から状態が取れない場合はアプリ側のフラグに従う
        assert!(CaptureCardViewer::should_record_window_geometry(
            false, None
        ));
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true, None
        ));
    }

    #[test]
    fn needs_drag_move_guard_borderless_without_drag_move_returns_true() {
        // タイトルバーもドラッグ移動も無い状態は作らせない
        assert!(needs_drag_move_guard(true, false));
    }

    #[test]
    fn needs_drag_move_guard_borderless_with_drag_move_returns_false() {
        // 既に動かせるなら何も変えない
        assert!(!needs_drag_move_guard(true, true));
    }

    #[test]
    fn needs_drag_move_guard_decorated_window_never_guards() {
        // タイトルバーがあれば掴んで動かせるので、ユーザーが切った
        // ドラッグ移動を勝手に戻さない
        assert!(!needs_drag_move_guard(false, false));
        assert!(!needs_drag_move_guard(false, true));
    }

    /// リサイズの当たり判定に使う、原点が (0, 0) でない矩形。
    /// 左上が原点だと `left()` と 0 の取り違えに気付けない
    fn resize_test_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 50.0), Vec2::new(400.0, 300.0))
    }

    #[test]
    fn resize_direction_at_center_returns_none() {
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(rect.center(), rect, RESIZE_BORDER),
            None
        );
    }

    #[test]
    fn resize_direction_at_each_edge_returns_that_edge() {
        use egui::ResizeDirection;
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(102.0, 200.0), rect, 8.0),
            Some(ResizeDirection::West)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(498.0, 200.0), rect, 8.0),
            Some(ResizeDirection::East)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 52.0), rect, 8.0),
            Some(ResizeDirection::North)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 348.0), rect, 8.0),
            Some(ResizeDirection::South)
        );
    }

    #[test]
    fn resize_direction_at_each_corner_returns_the_diagonal() {
        use egui::ResizeDirection;
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(101.0, 51.0), rect, 8.0),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(499.0, 51.0), rect, 8.0),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(101.0, 349.0), rect, 8.0),
            Some(ResizeDirection::SouthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(499.0, 349.0), rect, 8.0),
            Some(ResizeDirection::SouthEast)
        );
    }

    #[test]
    fn resize_direction_at_exactly_on_the_margin_still_resizes() {
        use egui::ResizeDirection;
        // 境界。帯の内側に含める側へ倒している
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(108.0, 200.0), rect, 8.0),
            Some(ResizeDirection::West)
        );
        // 帯の 1 つ外は掴めない
        assert_eq!(
            resize_direction_at(egui::pos2(108.1, 200.0), rect, 8.0),
            None
        );
    }

    #[test]
    fn resize_direction_at_outside_the_window_returns_none() {
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(99.0, 200.0), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 400.0), rect, 8.0),
            None
        );
    }

    #[test]
    fn resize_direction_at_tiny_window_prefers_the_top_left() {
        use egui::ResizeDirection;
        // 帯の 2 倍より小さいウィンドウでは左右（上下）の判定が重なる。
        // どちらとも言えないからと None を返すと、縮めすぎたウィンドウを
        // 二度と広げられなくなる
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(10.0, 10.0));

        assert_eq!(
            resize_direction_at(egui::pos2(5.0, 5.0), rect, 8.0),
            Some(ResizeDirection::NorthWest)
        );
    }

    #[test]
    fn resize_direction_at_non_finite_input_returns_none() {
        // NaN は比較がすべて false になり、判定が静かに壊れる
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(f32::NAN, 200.0), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(102.0, f32::INFINITY), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(102.0, 200.0), rect, f32::NAN),
            None
        );
    }

    #[test]
    fn resize_direction_at_zero_margin_returns_none() {
        // 帯の幅が 0 なら掴める場所は無い
        let rect = resize_test_rect();

        assert_eq!(resize_direction_at(rect.min, rect, 0.0), None);
        assert_eq!(resize_direction_at(rect.min, rect, -4.0), None);
    }

    #[test]
    fn resize_cursor_matches_the_direction() {
        use egui::{CursorIcon, ResizeDirection};

        assert_eq!(
            resize_cursor(ResizeDirection::North),
            CursorIcon::ResizeVertical
        );
        assert_eq!(
            resize_cursor(ResizeDirection::South),
            CursorIcon::ResizeVertical
        );
        assert_eq!(
            resize_cursor(ResizeDirection::East),
            CursorIcon::ResizeHorizontal
        );
        assert_eq!(
            resize_cursor(ResizeDirection::West),
            CursorIcon::ResizeHorizontal
        );
        assert_eq!(
            resize_cursor(ResizeDirection::NorthEast),
            CursorIcon::ResizeNeSw
        );
        assert_eq!(
            resize_cursor(ResizeDirection::SouthWest),
            CursorIcon::ResizeNeSw
        );
        assert_eq!(
            resize_cursor(ResizeDirection::NorthWest),
            CursorIcon::ResizeNwSe
        );
        assert_eq!(
            resize_cursor(ResizeDirection::SouthEast),
            CursorIcon::ResizeNwSe
        );
    }

    #[test]
    fn needs_reapply_not_applied_yet_returns_true() {
        // まだ一度も適用できていない場合は適用する
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &"F5".to_string(),
            &None
        ));
    }

    #[test]
    fn needs_reapply_same_value_returns_false() {
        // 値が変わっていなければ再適用しない（2 秒ごとの再登録を防ぐ肝）
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &"F5".to_string(),
            &Some("F5".to_string())
        ));
    }

    #[test]
    fn needs_reapply_changed_value_returns_true() {
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &"F7".to_string(),
            &Some("F5".to_string())
        ));
    }

    #[test]
    fn needs_reapply_cleared_value_returns_true() {
        // 設定画面で「クリア」した場合。設定は None になるが、実行中は
        // 古いホットキーが登録されたまま。ここを差分として拾えないと、
        // そのセッションの間ずっと解除されない
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &Some(Some("F5".to_string()))
        ));
    }

    #[test]
    fn needs_reapply_already_cleared_returns_false() {
        // 解除済みの状態。2 秒ごとに解除し直さない
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &Some(None)
        ));
    }

    #[test]
    fn needs_reapply_cleared_but_not_applied_yet_returns_true() {
        // 未適用（外側の None）と解除済み（Some(None)）を区別する。
        // 区別できないと、起動直後の 1 回が飛ぶ
        assert!(CaptureCardViewer::needs_reapply(
            false,
            &None::<String>,
            &None
        ));
    }

    #[test]
    fn hotkey_error_summary_without_errors_is_none() {
        // すべて登録できている状態。通知を出さないだけでなく、
        // 呼び出し側が既存の通知を取り下げる合図にもなる
        assert_eq!(hotkey_error_summary(&BTreeMap::new()), None);
    }

    #[test]
    fn hotkey_error_summary_one_error_names_the_action_and_key() {
        let errors = BTreeMap::from([(
            HotkeyAction::Screenshot,
            HotkeyError {
                hotkey: "F12".to_string(),
                message: "他のアプリと競合しています".to_string(),
            },
        )]);

        assert_eq!(
            hotkey_error_summary(&errors),
            Some("スクリーンショット（F12）: 他のアプリと競合しています".to_string())
        );
    }

    #[test]
    fn hotkey_error_summary_multiple_errors_are_joined() {
        // 1 件ずつ通知するとトーストが上書きされて最後の 1 件しか読めない。
        // 並び順はアクションの宣言順（BTreeMap）で安定する
        let errors = BTreeMap::from([
            (
                HotkeyAction::VolumeUp,
                HotkeyError {
                    hotkey: "F8".to_string(),
                    message: "理由 B".to_string(),
                },
            ),
            (
                HotkeyAction::Screenshot,
                HotkeyError {
                    hotkey: "F5".to_string(),
                    message: "理由 A".to_string(),
                },
            ),
        ]);

        assert_eq!(
            hotkey_error_summary(&errors),
            Some("スクリーンショット（F5）: 理由 A / 音量を上げる（F8）: 理由 B".to_string())
        );
    }

    #[test]
    fn hotkey_error_summary_does_not_repeat_the_headline() {
        // 定型文は status::format_message が前に付ける。ここで付けると
        // 「ホットキーを登録できません: ホットキーを登録できません: ...」になる
        let errors = BTreeMap::from([(
            HotkeyAction::Screenshot,
            HotkeyError {
                hotkey: "F5".to_string(),
                message: "理由".to_string(),
            },
        )]);

        let summary = hotkey_error_summary(&errors).expect("理由があること");

        assert!(!summary.contains(ErrorSource::Hotkey.headline()));
    }

    #[test]
    fn needs_reapply_initial_same_value_returns_true() {
        // 起動直後は値が同じでも適用する
        assert!(CaptureCardViewer::needs_reapply(
            true,
            &"F5".to_string(),
            &Some("F5".to_string())
        ));
    }

    #[test]
    fn needs_reapply_path_same_value_returns_false() {
        // PathBuf でも同じ判定になること
        assert!(!CaptureCardViewer::needs_reapply(
            false,
            &PathBuf::from("sound/SS.mp3"),
            &Some(PathBuf::from("sound/SS.mp3"))
        ));
    }

    #[test]
    fn window_size_or_default_valid_size_is_kept() {
        assert_eq!(window_size_or_default(Some((800.0, 600.0))), (800.0, 600.0));
    }

    #[test]
    fn window_size_or_default_none_returns_default() {
        // 初回起動。保存された値がまだ無い
        assert_eq!(window_size_or_default(None), (1280.0, 720.0));
    }

    #[test]
    fn window_size_or_default_unusable_size_returns_default() {
        // 設定ファイルを手で編集すると、ウィンドウとして成立しない値が入りうる。
        // そのまま with_inner_size へ渡さないことを確かめる
        let unusable = [
            (0.0, 720.0),
            (1280.0, 0.0),
            (-1280.0, 720.0),
            (1280.0, -720.0),
            (f32::NAN, 720.0),
            (1280.0, f32::NAN),
            (f32::INFINITY, 720.0),
            (1280.0, f32::NEG_INFINITY),
        ];

        for size in unusable {
            assert_eq!(
                window_size_or_default(Some(size)),
                (1280.0, 720.0),
                "size={:?} をそのまま採用した",
                size
            );
        }
    }

    // is_position_visible のテストで使うモニタ構成。
    // 1920x1080 の下端 40px をタスクバーが占めている想定で、作業領域は 1920x1040。
    // 副モニタは主モニタの右隣に並べてある
    fn primary_monitor() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1040.0))
    }

    fn secondary_monitor() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(1920.0, 0.0), egui::pos2(3840.0, 1040.0))
    }

    #[test]
    fn is_position_visible_inside_primary_monitor_returns_true() {
        assert!(is_position_visible(
            (100.0, 100.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_far_off_screen_returns_false() {
        // 外したサブモニタの上にウィンドウがあった場合に相当する
        assert!(!is_position_visible(
            (-5000.0, 300.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_barely_overlapping_returns_false() {
        // 右端から 10px だけ覗いている状態。タイトルバーを掴めないので不可とする
        assert!(!is_position_visible(
            (1910.0, 500.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_exactly_minimum_overlap_returns_true() {
        // 境界。右下に 120x32 だけ残る位置（作業領域の右端 1920 / 下端 1040 から引いた値）
        assert!(is_position_visible(
            (1800.0, 1008.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_one_pixel_short_of_minimum_returns_false() {
        // 境界の外側。幅の重なりが 119px しかない
        assert!(!is_position_visible(
            (1801.0, 1008.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_negative_side_minimum_overlap_returns_true() {
        // 左へはみ出した側の境界。幅 800 のウィンドウを -680 に置くと 120px 残る
        assert!(is_position_visible(
            (-680.0, 0.0),
            (800.0, 600.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_on_secondary_monitor_returns_true() {
        // 副モニタが繋がっている間はそのまま復元してよい
        assert!(is_position_visible(
            (2000.0, 100.0),
            (1280.0, 720.0),
            &[primary_monitor(), secondary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_secondary_monitor_removed_returns_false() {
        // 同じ位置でも副モニタを外した構成では画面外になる
        assert!(!is_position_visible(
            (2000.0, 100.0),
            (1280.0, 720.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_no_monitors_returns_false() {
        // モニタ情報が取れなかった場合。位置指定を諦めて OS に任せる
        assert!(!is_position_visible((100.0, 100.0), (1280.0, 720.0), &[]));
    }

    #[test]
    fn is_position_visible_window_smaller_than_minimum_overlap_returns_true() {
        // 最小の重なりより小さいウィンドウは、全体が収まっていれば見えている
        assert!(is_position_visible(
            (100.0, 100.0),
            (50.0, 20.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_zero_size_returns_false() {
        // 大きさが潰れていると、どこに置いても見えない
        assert!(!is_position_visible(
            (100.0, 100.0),
            (0.0, 0.0),
            &[primary_monitor()]
        ));
    }

    #[test]
    fn is_position_visible_non_finite_values_return_false() {
        // 設定ファイルは手で編集できるため、NaN や inf が入りうる
        let broken = [
            ((f32::NAN, 100.0), (1280.0, 720.0)),
            ((100.0, f32::INFINITY), (1280.0, 720.0)),
            ((100.0, 100.0), (f32::NAN, 720.0)),
            ((100.0, 100.0), (1280.0, f32::NEG_INFINITY)),
        ];

        for (pos, size) in broken {
            assert!(
                !is_position_visible(pos, size, &[primary_monitor()]),
                "pos={:?} size={:?} を画面内と判定した",
                pos,
                size
            );
        }
    }

    #[test]
    fn load_icon_decodes_embedded_icon() {
        // 埋め込みアイコンが読めなくなるとフォールバックの赤い四角（32x32）に
        // なる。大きさで両者を見分けられるため、寸法を直接確かめる。
        let icon = load_icon();

        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    // calculate_aspect_ratio_size のテストで使う値は、期待値が 2 進小数で
    // 割り切れるように選んである。誤差を許容する比較にすると、桁落ちが
    // 起きても気付けないため。
    #[test]
    fn calculate_aspect_ratio_size_wide_image_fits_to_width() {
        // 2:1 の映像を正方形の領域へ。横幅いっぱいに広げて上下を余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_tall_image_fits_to_height() {
        // 1:2 の映像を正方形の領域へ。高さいっぱいに広げて左右を余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(800.0, 1600.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::new(200.0, 400.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_same_aspect_fills_area() {
        // 縦横比が一致するときは領域をそのまま埋める
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 200.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_area_wider_than_image_fits_to_height() {
        // 領域のほうが横長。高さに合わせ、横幅は余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(1000.0, 200.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_upscales_to_fill_area() {
        // 映像より領域が大きいときは拡大する。縮小専用ではない
        let size = calculate_aspect_ratio_size(Vec2::new(400.0, 200.0), Vec2::new(1600.0, 1600.0));

        assert_eq!(size, Vec2::new(1600.0, 800.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_height_area_returns_zero() {
        // 最小化やウィンドウの極端な縮小で高さが 0 になる。
        // available_size.x / available_size.y が inf になるケース
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 0.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_width_area_returns_zero() {
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(0.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_area_returns_zero() {
        // 幅も高さも 0。0.0 / 0.0 が NaN になるケース
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::ZERO);

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_negative_area_returns_zero() {
        // egui のレイアウトは余白が足りないと負の available_size を返すことがある。
        // 負の大きさの矩形を描画に渡さないよう、ここで潰す
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(-10.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_height_image_returns_zero() {
        // テクスチャ側が潰れている場合。image_size.x / image_size.y が inf になる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 0.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_image_returns_zero() {
        // 0.0 / 0.0 で image_aspect が NaN になり、
        // 掛け算の結果として NaN が呼び出し側へ漏れるケース
        let size = calculate_aspect_ratio_size(Vec2::ZERO, Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_degenerate_input_never_returns_nan_or_inf() {
        // 描画へ渡る値が NaN / inf にならないことを、退化した入力の組で一括して確かめる
        let degenerate = [
            (Vec2::ZERO, Vec2::ZERO),
            (Vec2::new(1600.0, 800.0), Vec2::new(400.0, 0.0)),
            (Vec2::new(1600.0, 800.0), Vec2::new(0.0, 400.0)),
            (Vec2::new(1600.0, 0.0), Vec2::new(400.0, 400.0)),
            (Vec2::new(0.0, 800.0), Vec2::new(400.0, 400.0)),
            (Vec2::new(1600.0, 800.0), Vec2::new(-10.0, -10.0)),
        ];

        for (image_size, available_size) in degenerate {
            let size = calculate_aspect_ratio_size(image_size, available_size);

            assert!(
                size.x.is_finite() && size.y.is_finite(),
                "image={:?} available={:?} で {:?} を返した",
                image_size,
                available_size,
                size
            );
            assert!(
                size.x >= 0.0 && size.y >= 0.0,
                "image={:?} available={:?} で負の大きさ {:?} を返した",
                image_size,
                available_size,
                size
            );
        }
    }

    #[test]
    fn load_icon_is_not_the_red_square_fallback() {
        // フォールバックは全画素が不透明な赤。埋め込みアイコンがそれと
        // 一致しないことを確かめ、デコード失敗を見逃さないようにする。
        let icon = load_icon();

        assert!(
            icon.rgba.chunks(4).any(|px| px != [255, 0, 0, 255]),
            "アイコンが赤一色になっている"
        );
    }
    // 書き出したファイルの中身から画像形式を判定する。
    // 拡張子ではなく実際のバイト列を見る
    fn detect_format(path: &Path) -> image::ImageFormat {
        let reader = image::io::Reader::open(path)
            .expect("保存したファイルを開けること")
            .with_guessed_format()
            .expect("形式を判定できること");
        reader.format().expect("形式が分かること")
    }

    // 2x2 の RGB フレーム。赤・緑・青・白を 1 画素ずつ並べてある
    fn test_frame_2x2() -> video::VideoFrame {
        video::VideoFrame {
            width: 2,
            height: 2,
            data: vec![
                255, 0, 0, // 左上: 赤
                0, 255, 0, // 右上: 緑
                0, 0, 255, // 左下: 青
                255, 255, 255, // 右下: 白
            ],
        }
    }

    // 品質の差がファイルサイズに出るように、細かく変化する模様を敷いた画像。
    // 一様な色だとどの品質でもほぼ同じ大きさに圧縮され、差を見られない
    fn detailed_frame_64x64() -> video::VideoFrame {
        let mut data = Vec::with_capacity(64 * 64 * 3);
        for y in 0..64u32 {
            for x in 0..64u32 {
                data.push((x * 37 + y * 11) as u8);
                data.push((x * 7 + y * 53) as u8);
                data.push((x * 91 + y * 29) as u8);
            }
        }
        video::VideoFrame {
            width: 64,
            height: 64,
            data,
        }
    }

    const JPEG_Q90: ScreenshotEncoding = ScreenshotEncoding::Jpeg { quality: 90 };

    #[test]
    fn save_frame_jpeg_writes_decodable_file() {
        // JPEG は非可逆なので画素値は比較せず、読み戻せることと大きさだけを見る
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, JPEG_Q90).expect("保存できること");

        let decoded = image::open(&path).expect("保存した JPEG を読み戻せること");
        assert_eq!(decoded.dimensions(), (2, 2));
        // 拡張子ではなく指定した形式で書けていること
        assert_eq!(image::ImageFormat::Jpeg, detect_format(&path));
    }

    #[test]
    fn save_frame_png_writes_pixels_without_loss() {
        // PNG は可逆なので、元の画素がそのまま戻ることまで確かめる
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.png");
        let frame = test_frame_2x2();

        save_frame(&frame, &path, ScreenshotEncoding::Png).expect("保存できること");

        let decoded = image::open(&path).expect("保存した PNG を読み戻せること");
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(image::ImageFormat::Png, detect_format(&path));
        assert_eq!(decoded.to_rgb8().into_raw(), frame.data);
    }

    #[test]
    fn save_frame_png_ignores_jpg_extension() {
        // 拡張子は get_screenshot_path が形式に合わせるので普段は一致するが、
        // 書き出す形式を決めるのは encoding だけであることを固定しておく
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, ScreenshotEncoding::Png).expect("保存できること");

        assert_eq!(image::ImageFormat::Png, detect_format(&path));
    }

    #[test]
    fn save_frame_lower_jpeg_quality_produces_smaller_file() {
        // 品質の指定がエンコーダまで届いていること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let low_path = dir.path().join("low.jpg");
        let high_path = dir.path().join("high.jpg");
        let frame = detailed_frame_64x64();

        save_frame(&frame, &low_path, ScreenshotEncoding::Jpeg { quality: 10 })
            .expect("保存できること");
        save_frame(
            &frame,
            &high_path,
            ScreenshotEncoding::Jpeg { quality: 100 },
        )
        .expect("保存できること");

        let low = std::fs::metadata(&low_path)
            .expect("大きさを取れること")
            .len();
        let high = std::fs::metadata(&high_path)
            .expect("大きさを取れること")
            .len();
        assert!(
            low < high,
            "品質 10 が品質 100 より小さくない: {} >= {}",
            low,
            high
        );
    }

    #[test]
    fn save_frame_creates_missing_parent_directory() {
        // 保存先フォルダが無い状態で撮影されることがあるため、親ごと作る
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shots").join("2026").join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, JPEG_Q90).expect("保存できること");

        assert!(path.exists());
    }

    #[test]
    fn save_frame_short_data_returns_error_without_creating_file() {
        // 画素数に対してデータが足りないフレーム。壊れたファイルを残さないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");
        let frame = video::VideoFrame {
            width: 2,
            height: 2,
            data: vec![0; 11],
        };

        let err = save_frame(&frame, &path, JPEG_Q90).expect_err("エラーになること");

        assert!(err.contains("組み立てられない"), "想定外のエラー: {}", err);
        assert!(!path.exists());
    }

    #[test]
    fn save_frame_zero_sized_frame_returns_error() {
        // フレームが来ていない状態を取り違えて保存しようとした場合
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");
        let frame = video::VideoFrame {
            width: 0,
            height: 0,
            data: Vec::new(),
        };

        let err = save_frame(&frame, &path, JPEG_Q90).expect_err("エラーになること");

        assert!(err.contains("大きさのない"), "想定外のエラー: {}", err);
        assert!(!path.exists());
    }

    #[test]
    fn drop_finished_threads_empty_stays_empty() {
        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        drop_finished_threads(&mut handles);

        assert!(handles.is_empty());
    }

    #[test]
    fn drop_finished_threads_removes_only_completed_handles() {
        // 合図が来るまで終わらないスレッドを 1 本混ぜ、
        // 終わった分だけが落ちることを見る
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = vec![
            std::thread::spawn(|| {}),
            std::thread::spawn(move || {
                let _ = release_rx.recv();
            }),
        ];

        // is_finished はスレッドが抜けきってから true になるため、待ち合わせる
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handles[0].is_finished() {
            assert!(Instant::now() < deadline, "1 本目のスレッドが終わらない");
            std::thread::sleep(Duration::from_millis(1));
        }

        drop_finished_threads(&mut handles);

        assert_eq!(handles.len(), 1, "終わっていないスレッドだけが残ること");
        assert!(
            !handles[0].is_finished(),
            "残ったのは実行中のスレッドであること"
        );

        // 後始末。合図を送ってからでないとスレッドが残る
        release_tx.send(()).expect("合図を送れること");
        handles
            .remove(0)
            .join()
            .expect("実行中だったスレッドを回収できること");
    }
}
