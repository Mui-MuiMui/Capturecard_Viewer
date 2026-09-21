//! アプリの状態 `CaptureCardViewer` と、その振る舞い。
//!
//! `eframe::App` の実装（`update` / `on_exit`）と構造体の定義をここに置き、
//! 個々の処理は役割ごとの子モジュールへ分けてある。子モジュールはどれも
//! `impl CaptureCardViewer` を足す形で、状態そのものは増やさない。

mod capabilities;
mod device;
mod monitor;
mod retry;

use self::capabilities::{AudioCapabilityResult, CapabilityResult};
use self::device::{AudioTarget, VideoTarget};
use self::monitor::VideoLinkAction;
use self::retry::ConnectRetry;
use crate::audio::AudioCapture;
use crate::hotkey::{HotkeyAction, HotkeyError, HotkeyManager};
use crate::overlay::{OverlayContent, TransientOverlay};
use crate::platform::DEFAULT_WINDOW_SIZE;
use crate::repaint::{next_repaint_delay, should_wake_on_event, RepaintCondition, RepaintWaker};
use crate::screenshot::{self, ScreenshotManager};
use crate::settings::{
    self, AppSettings, AutoSavePolicy, ColorRange, ColorSpace, ScreenshotEncoding, MAX_VOLUME,
    MIN_VOLUME,
};
use crate::status::{self, ConnectionStatus, ErrorCenter, ErrorSource, LinkStatus};
use crate::ui;
use crate::video::{self, FrameStats, VideoAdjustments, VideoCapture};
use chrono::Local;
use eframe::egui;
use log::{debug, error, info, trace, warn};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// 右クリックメニューの幅。項目名が折り返さない程度に取ってある。
/// サブメニューにも同じ値を使う（egui の既定は 150px で、
/// 「アスペクト比を維持」のような項目名が折り返してしまう）
const CONTEXT_MENU_WIDTH: f32 = 240.0;

/// 右クリックメニューの大きさを決めるときに、画面の端へ空けておく余白。
/// 端にぴったり貼り付くと、収まっているのかはみ出しているのかが見分けにくい
const CONTEXT_MENU_SCREEN_MARGIN: f32 = 24.0;

/// 外側クリックの判定でサブメニューの矩形に足す余白。
/// `Ui::min_rect` はポップアップの枠の内側なので、枠の上を押しただけで
/// メニュー全体が閉じるのを防ぐ
const CONTEXT_MENU_HIT_MARGIN: f32 = 8.0;

/// 音量を変えたときに OSD を出しておく時間。
/// ホイールを回している間は回すたびに延びるので、これは「手を止めてから」の長さ
const VOLUME_OSD_DURATION: Duration = Duration::from_millis(1500);

/// プリセットを切り替えたときに OSD を出しておく時間。
///
/// 音量と同じ長さにしてある。どちらも「押した結果がこれで合っているか」を
/// 確かめるための表示で、読み終わる前に消えても困るし、残り続けても邪魔になる
const PRESET_OSD_DURATION: Duration = Duration::from_millis(1500);

/// 音量の基準値。OSD のバーはこの位置に目盛りを引く
const VOLUME_REFERENCE: f32 = 100.0;

/// ホイール 1 段、またはホットキー 1 回で動かす音量
const VOLUME_SCROLL_STEP: f32 = 10.0;

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

    // UI スレッド以外から再描画を促すための窓口。
    // 映像のフレームコールバックとホットキーのリスナーへ複製を渡してある。
    // 最初の update() で egui::Context と結びつく
    repaint_waker: RepaintWaker,

    // デバイス能力の取得結果を受け取るチャネル。
    // 取得はデバイスを開く重い処理なので使い捨てのスレッドへ投げ、
    // UI スレッドは update() で try_recv するだけにする
    capability_tx: Sender<CapabilityResult>,
    capability_rx: Receiver<CapabilityResult>,

    // オーディオデバイスの対応設定を受け取るチャネル。ビデオと同じ仕組み。
    // WASAPI の列挙は実測 300ms 前後かかるため、UI スレッドでは行わない
    audio_capability_tx: Sender<AudioCapabilityResult>,
    audio_capability_rx: Receiver<AudioCapabilityResult>,
    // 対応設定が届くのを待ち始めた時刻。`AUDIO_CAPABILITY_WAIT_LIMIT` を
    // 超えたら待つのをやめ、その場で列挙してでも音声を開く
    audio_capability_wait_since: Option<Instant>,

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
    // 右クリックメニューを平らな一覧にするかサブメニューへ折りたたむか。
    // メニューを開いた瞬間に決めて、開いている間は変えない（詳細は
    // `open_context_menu` のコメント）
    context_menu_layout: MenuLayout,
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
    // 最後に新しいフレームをテクスチャへ取り込んだ時刻。
    // None は起動してから 1 枚も取り込んでいないことを表す。
    // 再描画の間隔（`repaint::next_repaint_delay`）を決めるために持つ
    last_new_frame_at: Option<Instant>,
    // フレームの途絶に対して最後に行った処置。
    // 毎フレーム同じ判定に当たるため、同じ処置を繰り返さないための番人。
    // 判定が変わったとき（自動再接続を有効にし直したとき）は動けるように、
    // 真偽値ではなく「何をしたか」で持つ。新しいフレームが届いた時点で
    // `Keep` へ戻す
    last_video_link_action: VideoLinkAction,
    // 途絶を検出して映像を開き直している最中か。
    // 次に映像が繋がったときだけ音声の再接続も要求するための目印で、
    // 起動時の接続と区別するために持つ（`should_resync_audio_after_video`）
    video_reconnect_after_loss: bool,
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
    // 最後に VideoCapture へ渡した映像調整（明るさ・コントラスト・彩度）。
    // 色変換と同じ理由で差分を取る
    last_video_adjustments: Option<VideoAdjustments>,
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

    // 「既定のデバイス」設定（音声）が Windows 側の既定切り替えに
    // 追従しているかを確認した最後の時刻。設定ダイアログの開閉に関係なく
    // 動くので、`last_device_list_update` とは別に持つ
    last_default_audio_check: Option<Instant>,

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

        // 再描画の窓口は、それを使う相手より先に作る。
        // ここではまだ egui::Context と結びついていないので何もしないが、
        // 複製した先にも最初の update() の bind がそのまま効く
        let repaint_waker = RepaintWaker::new();

        let video_capture = {
            let mut video_capture = VideoCapture::new();
            // **start_capture より前に渡すこと。** フレームコールバックは
            // 開始時点の複製を持つため、あとから渡しても届かない
            video_capture.set_repaint_waker(repaint_waker.clone());
            Arc::new(Mutex::new(video_capture))
        };

        let mut hotkey_manager = HotkeyManager::new();
        hotkey_manager.set_repaint_waker(repaint_waker.clone());

        #[allow(clippy::arc_with_non_send_sync)] // 音声キャプチャは非同期処理で必要
        let audio_capture = Arc::new(Mutex::new(AudioCapture::new()));
        let screenshot_manager = Arc::new(Mutex::new(ScreenshotManager::new()));
        let (capability_tx, capability_rx) = std::sync::mpsc::channel();
        let (audio_capability_tx, audio_capability_rx) = std::sync::mpsc::channel();
        let (screenshot_tx, screenshot_rx) = std::sync::mpsc::channel();

        let mut app = Self {
            settings,
            video_capture,
            audio_capture,
            screenshot_manager,
            hotkey_manager,
            repaint_waker,
            capability_tx,
            capability_rx,
            audio_capability_tx,
            audio_capability_rx,
            audio_capability_wait_since: None,
            screenshot_tx,
            screenshot_rx,
            errors: ErrorCenter::default(),
            last_screenshot_outcome_at: None,
            show_settings: false,
            settings_dialog: ui::SettingsDialogState::default(),
            show_context_menu: false,
            show_hotkey_dialog: false,
            context_menu_pos: egui::Pos2::ZERO,
            // メニューが閉じている間は使われない。開くときに必ず
            // open_context_menu が上書きする
            context_menu_layout: MenuLayout::Collapsed,
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
            last_new_frame_at: None,
            last_video_link_action: VideoLinkAction::Keep,
            video_reconnect_after_loss: false,
            video_capturing: false,
            last_audio_error_reconnect: None,
            audio_stream_error_pending: false,
            last_video_device: None,
            last_video_res: None,
            last_video_format: None,
            last_audio_device: None,
            last_audio_output: None,
            last_audio_rate: None,
            last_audio_channels: None,
            last_video_fps: None,
            last_color_conversion: None,
            last_video_adjustments: None,
            last_sound_file: None,

            video_retry: ConnectRetry::default(),
            audio_retry: ConnectRetry::default(),
            startup_applied: false,

            // UI性能向上のためのデバイスリストキャッシュ
            cached_video_devices: Vec::new(),
            cached_input_devices: Vec::new(),
            cached_output_devices: Vec::new(),
            last_device_list_update: None,
            last_default_audio_check: None,

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
                // **入力デバイスは出力と違い、未設定のままにしない。** 出力の
                // 既定は「スピーカー」でまず無害だが、入力の既定は環境依存
                // （ノート PC ならほぼ確実に内蔵マイク）で、パススルーが
                // そのままマイクの音をスピーカーへ流してしまう。#134（PR #147）
                // で切断時に同じことが起きる不具合を直したばかりで、初回起動で
                // 同じ誤動作を起こすわけにいかない。列挙した先頭のデバイスへ
                // 書き換えて確定させる。
                //
                // この結果、入力側の「既定のデバイス」追従（poll_default_audio_device
                // の track_input）は、設定ファイルを手で編集して
                // input_device_name を消した場合にだけ効く。設定画面の
                // コンボボックスに「デフォルト」の選択肢を足すかどうかは
                // 別 Issue で判断する
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
        }

        // 音声デバイスの対応設定も先に取りに行く。
        //
        // **設定画面のためだけではない。** 音声を開く `start_passthrough` は
        // 対応設定の一覧を要るので、キャッシュが無いと UI スレッドで列挙する
        // ことになる（実測 300ms）。最初の接続はこの結果が届くまで待つ
        app.request_audio_capabilities();
        app.dispatch_capability_requests();

        // 注: デバイスの接続は最初の update() で始まり、失敗したら
        // ConnectRetry のバックオフで繋がるまで再試行する
        app
    }
}

impl eframe::App for CaptureCardViewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ホットキー入力ダイアログの開閉を検出するため、このフレームに入る前の
        // 状態を控えておく。設定ダイアログの描画（一覧の「設定...」）で
        // `show_hotkey_dialog` が変わるより前に取る必要がある
        let hotkey_dialog_was_open = self.show_hotkey_dialog;

        // 再描画の窓口を Context と結びつける。2 回目以降は何もしない。
        // **デバイスを開くより先に済ませること。** 開いたあとだと、
        // 最初のフレームの到着を知らせる先が無い
        self.repaint_waker.bind(ctx);

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

        // ビデオフレームを更新。新着の時刻は末尾の再描画の予約で使う
        if self.update_video_texture(ctx) {
            self.last_new_frame_at = Some(Instant::now());
        }

        // フレームの途絶と音声ストリームのエラーを見て、必要なら開き直しを要求する。
        // 実際に開くのは次のフレームの poll_device_connection
        self.monitor_device_health();

        // 「既定のデバイス」設定が Windows 側の既定切り替えに追従しているかを
        // 確認する。設定ダイアログの開閉に関係なく、数秒おきに動く
        self.poll_default_audio_device();

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

        // 最小化しているか。Windows では egui-winit が毎フレーム入れてくれる。
        // 取れない環境では「最小化していない」に倒す（描きすぎる側は安全）
        let minimized = viewport.minimized.unwrap_or(false);

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

        // ホットキー入力ダイアログを開いた最初のフレームで、グローバルホットキーを
        // 一時解除する。**ダイアログの描画より前に行う。** こうしないと、開いた
        // 最初のフレームで押されたキーがグローバルホットキーとしても実行されうる
        if self.show_hotkey_dialog && !hotkey_dialog_was_open {
            self.hotkey_manager.pause();
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

            // 重複判定に使う現在の割り当て一覧。設定ダイアログから開かれている
            // 場合はドラフトを、そうでなければ共有設定を見る（一覧の表示と
            // 同じ基準に揃える）
            let existing_hotkeys = match self.settings_dialog.draft() {
                Some(draft) => draft.hotkeys.clone(),
                None => self
                    .settings
                    .lock()
                    .map(|settings| settings.hotkeys.clone())
                    .unwrap_or_default(),
            };

            let outcome = ui::show_hotkey_capture_dialog(
                ctx,
                &mut self.show_hotkey_dialog,
                action,
                &existing_hotkeys,
                self.settings_dialog.hotkey_capture_mut(),
            );

            if let ui::HotkeyDialogOutcome::Captured(candidate) = outcome {
                // 一時停止で自分自身の登録は解除済みなので、ここでの試し登録が
                // 自分の他のアクションと衝突することはない。他のアプリが既に
                // 使っているキー（F12 など）だけを弾ける
                match self.hotkey_manager.try_register(&candidate) {
                    Ok(()) => {
                        debug!("{} に {} を割り当てた", action.label(), candidate);

                        // 設定ダイアログから開かれている場合はドラフトへ書く。
                        // 共有設定へ直接書くと、ダイアログの OK がドラフトの古い値で
                        // 上書きして、設定したホットキーが消える
                        let wrote_to_draft = match self.settings_dialog.draft_mut() {
                            Some(draft) => {
                                draft.set_hotkey(action, Some(candidate.clone()));
                                true
                            }
                            None => false,
                        };

                        if !wrote_to_draft {
                            // 設定ダイアログが閉じられた状態でホットキーだけ確定した場合。
                            // ドラフトが無いので共有設定へ直接書く。実際の登録は
                            // ダイアログが閉じたあとの再開（resume）で行う
                            if let Ok(mut settings) = self.settings.lock() {
                                settings.set_hotkey(action, Some(candidate));
                            }
                            self.mark_settings_dirty();
                        }
                        // ドラフトへ書いた場合はここで登録しない。登録すると、
                        // 2 秒ごとの apply_settings が共有設定側の古いホットキーを
                        // 見て登録し直し、「適用」も押していないのに効いたり
                        // 戻ったりする。実際の登録は「適用」か「OK」で行う
                    }
                    Err(reason) => {
                        warn!(
                            "{} に {} を割り当てられない: {}",
                            action.label(),
                            candidate,
                            reason
                        );
                        // 閉じずにダイアログを開き直し、理由を表示する
                        self.show_hotkey_dialog = true;
                        self.settings_dialog
                            .hotkey_capture_mut()
                            .set_rejection(reason);
                    }
                }
            }
        }

        // ホットキー入力ダイアログを閉じたフレームで、一時解除していた
        // グローバルホットキーを登録し直す
        if !self.show_hotkey_dialog && hotkey_dialog_was_open {
            self.resume_hotkeys_after_capture();
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

        // 次の update() をいつ呼ぶかを、このフレームの状態から 1 か所で決める。
        //
        // **ここは上限であって下限ではない。** もっと早く起きたい処理
        // （OSD の消滅、設定の書き出し、フレームの到着）はそれぞれ自分で
        // 予約しており、egui は同じフレームで要求された中の最短を採る
        let condition = RepaintCondition {
            minimized,
            since_new_frame: self.last_new_frame_at.map(|at| at.elapsed()),
        };
        // 間隔を広げている間だけ、別スレッドからの通知で起こしてもらう
        self.repaint_waker
            .set_enabled(should_wake_on_event(condition));
        ctx.request_repaint_after(next_repaint_delay(condition));
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
    /// 新着フレームがあればテクスチャへ取り込む。取り込んだら `true`。
    ///
    /// **ここで再描画を予約しない。** 予約は `update()` の末尾で 1 か所にまとめる。
    /// 以前はここで無条件に 16ms（60fps）の再描画を予約していたため、映像が
    /// 来ていなくても、最小化していても描き続けていた（Issue #98）。
    fn update_video_texture(&mut self, ctx: &egui::Context) -> bool {
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

            return true;
        }

        false
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
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
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
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
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
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
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
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
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

    /// 右クリックメニューを描く。
    ///
    /// 中身は `context_menu_items` が描く。ここは置き場所と閉じ方だけを持つ。
    ///
    /// **画面に収まらなくなるのを 3 段で防いでいる。** まず `constrain_to` で
    /// メニューごと画面内へ押し戻し、幅は画面より広くならないように縮め、
    /// それでも足りない高さは `ScrollArea` でスクロールできるようにする。
    /// 項目を足すときはどれも壊さないこと。
    /// 右クリックメニューを開く。位置と、平らな一覧にするかサブメニューへ
    /// 折りたたむかをこの時点で確定させる。
    ///
    /// **判定は開いた瞬間の 1 回だけ行い、開いている間は毎フレーム描画に
    /// 合わせて計算し直さない。** ウィンドウをリサイズしながらメニューを
    /// 出しっぱなしにできる egui の仕様上、毎フレーム判定すると境界付近で
    /// 開閉のたびにレイアウトが入れ替わってちらつく。
    fn open_context_menu(&mut self, ctx: &egui::Context, pos: egui::Pos2) {
        self.show_context_menu = true;
        self.context_menu_pos = pos;

        let frame = egui::Frame::popup(&ctx.style());
        let (_, max_height) =
            context_menu_size_limits(ctx.screen_rect().size(), frame.inner_margin.sum());
        // プリセットが 1 つでもあれば、平らな一覧に「プリセット」の行が
        // 1 行増える（preset_submenu、詳細は estimate_flat_menu_height）
        let has_presets = self
            .settings
            .lock()
            .map(|settings| !settings.presets.is_empty())
            .unwrap_or(false);
        let flat_height = estimate_flat_menu_height(&ctx.style().spacing, has_presets);
        self.context_menu_layout = context_menu_layout(max_height, flat_height);
    }

    fn show_context_menu(&mut self, ctx: &egui::Context) {
        let mut close_menu = false;
        // メニュー本体と、開いているサブメニューの矩形。外側クリックの判定に使う。
        // **サブメニューは別の Area に描かれ本体の矩形に含まれない。** ここへ
        // 足しておかないと、サブメニューを押しただけでメニュー全体が閉じる
        let mut menu_rects: Vec<egui::Rect> = Vec::new();

        // ポップアップの枠が食う分を引いてから、中身に使える大きさを決める
        let frame = egui::Frame::popup(&ctx.style());
        let (width, max_height) =
            context_menu_size_limits(ctx.screen_rect().size(), frame.inner_margin.sum());

        egui::Area::new("context_menu")
            .fixed_pos(self.context_menu_pos)
            .order(egui::Order::Foreground)
            // 画面の下端や右端の近くで開いたときに、メニューごと画面内へ押し戻す
            .constrain_to(ctx.screen_rect())
            .show(ctx, |outer_ui| {
                // 固定幅でポップアップコンテンツをラップ
                frame.show(outer_ui, |ui| {
                    ui.set_min_width(width);
                    ui.set_max_width(width);

                    // 折りたたみ判定は開いた時点で確定済み（open_context_menu）。
                    // ここでは保険として ScrollArea と constrain_to をどちらの
                    // レイアウトでも残す。見積もりが外れて平らな一覧が実際には
                    // 収まらなかった場合の逃げ道になる
                    egui::ScrollArea::vertical()
                        .max_height(max_height)
                        // 横は縮めない。縮むと項目の幅が中身ごとに変わって揃わない
                        .auto_shrink([false, true])
                        .show(ui, |ui| match self.context_menu_layout {
                            MenuLayout::Flat => {
                                self.context_menu_items_flat(
                                    ctx,
                                    ui,
                                    width,
                                    &mut menu_rects,
                                    &mut close_menu,
                                );
                            }
                            MenuLayout::Collapsed => {
                                self.context_menu_items(
                                    ctx,
                                    ui,
                                    width,
                                    &mut menu_rects,
                                    &mut close_menu,
                                );
                            }
                        });
                });
                // 構築後、エリアの完全な矩形をキャプチャ
                menu_rects.push(outer_ui.min_rect());
            });

        // 外側をクリック、またはEscapeキー押下時のみ閉じる
        ctx.input(|i| {
            if i.pointer.primary_clicked() {
                if let Some(pos) = i.pointer.latest_pos() {
                    if !menu_rects.iter().any(|rect| rect.contains(pos)) {
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

    /// 右クリックメニューの項目を、折りたたまずに平らな一覧として描く。
    ///
    /// **項目順は PR #146 より前と同じ。** サブメニューへ分けたところ、
    /// 隠れて操作性が落ちるという実機確認の指摘を受けたため、ウィンドウの
    /// 高さが十分なときは使い慣れたこちらの並びへ戻す。動作そのものは
    /// `context_menu_items` / `view_submenu` / `window_submenu` と同じで、
    /// 見せ方（階層に分けるかどうか）だけが違う。
    ///
    /// **「プリセット」だけは折りたたみの有無に関わらずサブメニューのまま
    /// 出す。** プリセットは固定の切り替えではなく可変長の一覧なので、平らな
    /// 一覧に展開すると項目数がプリセットの数だけ増減し、高さの見積もり
    /// （`estimate_flat_menu_height` は固定の行数を前提にしている）と
    /// 食い違う。そのため `menu_rects` を受け取る（`preset_submenu` が開く
    /// サブメニューの矩形を外側クリックの判定に含めるため）。
    fn context_menu_items_flat(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.label(format!("音量: {}%", self.volume as i32));
        let volume_response =
            ui.add(egui::Slider::new(&mut self.volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"));
        if volume_response.changed() {
            self.set_volume_from_ui(self.volume);
        }

        let mut muted = self.muted;
        if ui.checkbox(&mut muted, "ミュート").changed() {
            self.set_muted_from_ui(muted);
        }

        ui.separator();

        let aspect_response = ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");
        if aspect_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.maintain_aspect_ratio = self.maintain_aspect_ratio;
            }
            self.mark_settings_dirty();
        }

        let always_on_top_response = ui.checkbox(&mut self.always_on_top, "最前面表示");
        if always_on_top_response.changed() {
            self.set_always_on_top(ctx, self.always_on_top);
        }

        let fullscreen_response = ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");
        if fullscreen_response.changed() {
            self.toggle_fullscreen(ctx, self.is_fullscreen);
        }

        let mut temp_borderless = self.borderless;
        let borderless_response = ui
            .add_enabled(
                !self.is_fullscreen,
                egui::Checkbox::new(&mut temp_borderless, "タイトルバーを隠す"),
            )
            .on_hover_text(
                "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います",
            )
            .on_disabled_hover_text("フルスクリーン中は元から装飾がないため切り替えられません");
        if borderless_response.changed() {
            self.set_borderless(ctx, temp_borderless);
        }

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
        if drag_move_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.enable_drag_move = temp_enable_drag_move;
            }
            self.mark_settings_dirty();
        }

        let stats_response = ui.checkbox(&mut self.show_stats_overlay, "情報表示");
        if stats_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.show_stats_overlay = self.show_stats_overlay;
            }
            self.mark_settings_dirty();
        }

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

        if ui
            .add_enabled(
                !self.is_fullscreen,
                egui::Button::new("ウィンドウサイズをリセット"),
            )
            .on_disabled_hover_text("フルスクリーン中は変更できません")
            .clicked()
        {
            self.reset_window_size(ctx);
            *close_menu = true;
        }
        if ui.button("デバイス再接続").clicked() {
            self.reconnect_devices();
            *close_menu = true;
        }

        // プリセットは映像と音声の取り込み方の切替なので、
        // 「デバイス再接続」のすぐそばに置く（collapsed 側と同じ理由）
        self.preset_submenu(ui, width, menu_rects, close_menu);

        ui.separator();
        if ui.button("詳細設定...").clicked() {
            self.show_settings = true;
            *close_menu = true;
        }

        ui.separator();
        if ui.button("終了").clicked() {
            info!("右クリックメニューから終了する");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            *close_menu = true;
        }
    }

    /// 右クリックメニューの項目を描く。
    ///
    /// 項目が増えて縦に伸びると低い解像度で下端が画面外へ出るため、切り替え系は
    /// 「表示」「ウィンドウ」のサブメニューへ分けてある。**直下に残すのは、映像が
    /// 出ないときの復帰手段（デバイス再接続）と、装飾を消しているときに他の手段が
    /// 無い操作（フルスクリーン、終了）。** 探し回らずに押せることを優先する。
    ///
    /// サブメニューの中身を足したときは、閉じるボタンに `ui.close_menu()` を
    /// 忘れないこと。呼ばないと開いた状態が egui 側に残り、次に右クリックした
    /// ときにサブメニューが開いたまま出る。
    fn context_menu_items(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.label(format!("音量: {}%", self.volume as i32));
        let volume_response =
            ui.add(egui::Slider::new(&mut self.volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"));

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

        // フルスクリーン表示のチェックボックス。
        // ダブルクリックとホットキーでも切り替えられるが、装飾を消していると
        // ここが唯一目に見える切り替え手段になるので直下に残す
        let fullscreen_response = ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");

        // フルスクリーン状態が変更された場合
        if fullscreen_response.changed() {
            self.toggle_fullscreen(ctx, self.is_fullscreen);
        }

        // サブメニューのボタンは既定だと文字の幅しか取らず、上下のチェック
        // ボックスと縁が揃わない。幅いっぱいに広げて 1 つの並びに見せる
        ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
            self.view_submenu(ctx, ui, width, menu_rects);
            self.window_submenu(ctx, ui, width, menu_rects, close_menu);
            // プリセットは映像と音声の取り込み方の切替なので、本来は下の
            // 「デバイス再接続」に近い。それでも他のサブメニューと並べて
            // あるのは、justified の並びから外すとボタンの幅が揃わないため
            self.preset_submenu(ui, width, menu_rects, close_menu);
        });

        ui.separator();
        // デバイス再接続。映像が出なくなったときの復帰手段なので、
        // サブメニューへ入れずに直下へ置く
        if ui.button("デバイス再接続").clicked() {
            self.reconnect_devices();
            *close_menu = true;
        }

        // デバイスの自動再接続のチェックボックス。
        // 設定は VideoSettings に持たせているが、音声ストリームの
        // エラーからの復帰にも効く（利用者から見て 1 つの機能なので
        // スイッチも 1 つにしてある）。上の「デバイス再接続」と紛らわしい
        // 項目なので、離さずに隣へ置いてある
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
        if ui.button("詳細設定...").clicked() {
            self.show_settings = true;
            *close_menu = true;
        }

        ui.separator();
        // 終了。装飾なしでは × が無いので、ここが閉じる手段になる。
        // 押すと on_exit が走り、保留中の設定も書き出される
        if ui.button("終了").clicked() {
            info!("右クリックメニューから終了する");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            *close_menu = true;
        }
    }

    /// 右クリックメニューの「表示」サブメニュー。
    ///
    /// 映像の見え方に関わる切り替えを集めてある。**どれを押してもメニューは
    /// 閉じない。** 続けて切り替えることがあるため。
    fn view_submenu(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
    ) {
        ui.menu_button("表示  ⏵", |ui| {
            // サブメニューの幅は egui の既定が 150px で、項目名が折り返す。
            // 本体と同じ幅に揃える（狭いウィンドウでは本体ごと縮んでいる）
            ui.set_max_width(width);

            let aspect_response = ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");

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

            // 情報表示（統計オーバーレイ）のチェックボックス
            let stats_response = ui.checkbox(&mut self.show_stats_overlay, "情報表示");

            // 情報表示の設定が変更された場合（書き出しはデバウンス）
            if stats_response.changed() {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.show_stats_overlay = self.show_stats_overlay;
                }
                self.mark_settings_dirty();
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

            menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
        });
    }

    /// 右クリックメニューの「ウィンドウ」サブメニュー。
    ///
    /// ウィンドウの動かし方と大きさに関わる項目を集めてある。
    /// **「ウィンドウサイズをリセット」は押したらメニューを閉じる。**
    /// 結果がウィンドウ全体に出るので、メニューが被ったままだと確かめられない。
    fn window_submenu(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.menu_button("ウィンドウ  ⏵", |ui| {
            ui.set_max_width(width);

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
                // サブメニュー側も閉じる。閉じないと開いた状態が egui に残り、
                // 次に右クリックしたときにサブメニューが開いたまま出る
                ui.close_menu();
                *close_menu = true;
            }

            menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
        });
    }

    /// 右クリックメニューの「プリセット」サブメニュー。
    ///
    /// **プリセットが 1 つも無いときは項目ごと出さない。** 押しても何も
    /// 起きない空のサブメニューを見せるより、無いことが分かるほうがよい。
    /// 作る場所は設定ダイアログの「その他」タブなので、ここには誘導を置かない。
    ///
    /// 選ぶとメニューを閉じる。デバイスを開き直すことがあり、結果は映像に
    /// 出るため、メニューが被ったままでは確かめられない。
    fn preset_submenu(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        // 名前と選択状態はここで 1 度だけ読む。サブメニューの中で
        // ロックを取ると、毎フレーム描画のたびに取り直すことになる
        let (names, active) = match self.settings.lock() {
            Ok(settings) => (
                settings
                    .presets
                    .iter()
                    .map(|preset| preset.name.clone())
                    .collect::<Vec<_>>(),
                settings::resolved_active_preset(&settings).map(str::to_string),
            ),
            Err(_) => {
                warn!("プリセットの一覧で settings のロックを取得できない");
                return;
            }
        };

        if names.is_empty() {
            return;
        }

        let mut selected: Option<String> = None;
        ui.menu_button("プリセット  ⏵", |ui| {
            ui.set_max_width(width);

            for name in &names {
                // 選択中のものにチェックを付ける。手で値を変えたあとは
                // どれも選択中にならない（resolved_active_preset が None）
                let is_active = active.as_deref() == Some(name.as_str());
                if ui.selectable_label(is_active, name).clicked() {
                    selected = Some(name.clone());
                    ui.close_menu();
                }
            }

            menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
        });

        if let Some(name) = selected {
            self.apply_preset_by_name(&name);
            *close_menu = true;
        }
    }

    /// プリセットを実行中の設定へ適用する。
    ///
    /// 変わるのは `video` と `audio` だけ。デバイスを開き直すかどうかは
    /// `apply_settings` の差分判定に任せるので、同じ内容のプリセットを
    /// 選び直しても映像は途切れない。
    fn apply_preset_by_name(&mut self, name: &str) {
        // ロックはここで手放す。apply_settings が同じロックを取る
        let applied = match self.settings.lock() {
            Ok(mut settings) => settings.apply_preset(name),
            Err(_) => {
                warn!("プリセットの適用で settings のロックを取得できない");
                return;
            }
        };

        if !applied {
            // 一覧を読んでから選ぶまでの間に消える経路は無いが、
            // 名前で引いている以上は起こりうるものとして扱う
            warn!("プリセット「{}」が見つからない", name);
            return;
        }

        info!("プリセット「{}」へ切り替えた", name);
        self.mark_settings_dirty();
        self.apply_settings(false);
        self.transient_overlay.show(
            OverlayContent::Text(format!("プリセット: {}", name)),
            PRESET_OSD_DURATION,
            Instant::now(),
        );
    }
}

/// 右クリックメニューの中身に使える幅と、高さの上限を決める。
///
/// 引数は egui の画面（＝ウィンドウ）の大きさと、ポップアップの枠が
/// 左右・上下で食う幅。戻り値は `(幅, 高さの上限)` で、どちらも枠の内側の値。
///
/// **`Area::constrain_to` は位置を画面内へ戻すだけで、確定した矩形の幅も
/// 高さも縮めない。** 画面より大きいメニューはそのままはみ出すので、
/// 幅はここで縮め、高さは呼び出し側が `ScrollArea` の上限に使う。
///
/// 画面が極端に小さい場合は 0 まで落とす。**「これ以下にはしない」という
/// 下限を置かない。** 置くと、下限を割る画面では必ずはみ出す側へ倒れ、
/// 画面に収めるという目的と逆になる。
fn context_menu_size_limits(screen_size: egui::Vec2, frame_margin: egui::Vec2) -> (f32, f32) {
    let available = screen_size - frame_margin - egui::Vec2::splat(CONTEXT_MENU_SCREEN_MARGIN);
    (
        CONTEXT_MENU_WIDTH.min(available.x).max(0.0),
        available.y.max(0.0),
    )
}

/// 右クリックメニューの見せ方。
///
/// `Flat` は PR #146 より前と同じ、切り替え系も含めた 1 階層の一覧。
/// `Collapsed` は PR #146 のサブメニュー構成（「表示」「ウィンドウ」）。
/// 実機確認でサブメニューは操作性が落ちるという指摘を受けたため、
/// **ウィンドウが十分に高いときは `Flat` を使う。** 収まらないときだけ
/// `Collapsed` へ落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuLayout {
    Flat,
    Collapsed,
}

/// 平らな一覧（PR #146 以前の構成）の見積もり行数とセパレータ数。
///
/// 内訳は `CaptureCardViewer::context_menu_items_flat` の並びと対応させて
/// あるので、あちらの項目を増減したときはこちらも直すこと。
///
/// 行: 音量ラベル / 音量スライダー / ミュート / アスペクト比を維持 /
/// 最前面表示 / フルスクリーン表示 / タイトルバーを隠す / 画面ドラッグ移動 /
/// 情報表示 / デバイスの自動再接続 / ウィンドウサイズをリセット /
/// デバイス再接続 / 詳細設定... / 終了 の 14 行。**プリセットが 1 つでも
/// あれば「プリセット」の行が 1 つ増える。** プリセットの有無は起動後にいつ
/// 変わるか分からないため固定の行数には含めず、`estimate_flat_menu_height`
/// の引数で足す。
/// セパレータ: ミュートの下 / 自動再接続の下（ウィンドウサイズをリセットの上）/
/// デバイス再接続の下 / 詳細設定の下 の 4 本。プリセットの行はセパレータを
/// 増やさない（デバイス再接続の直後に挟まるだけ）。
const FLAT_MENU_ROW_COUNT: usize = 14;
const FLAT_MENU_SEPARATOR_COUNT: usize = 4;

/// 平らな一覧の高さを、描画前に見積もる。
///
/// 実測はしない。実測しようとすると「一度サブメニュー構成で描いてから
/// 高さを比べる」といった余分な描画が要る。行の高さは
/// `interact_size.y + item_spacing.y`（チェックボックスやボタンの
/// クリック領域＋行間）、セパレータは egui の実装に合わせて
/// `item_spacing.y * 2.0 + 1.0`（線の上下の余白＋線そのものの太さ）で
/// 見積もる。多少のずれは境界の判定に影響するだけで、実際に描画した
/// ときにスクロールへ倒れる分には安全側（`context_menu_layout` 側で
/// 境界を `Flat` 寄りにしてあるのはこのため）。
///
/// `has_presets` はプリセットが 1 つ以上あるかどうか。あれば行数に 1 を
/// 足す（`preset_submenu` が平らな一覧にも「プリセット」の行を描くため）。
/// ここを固定 14 行のままにすると、プリセットがある状態でちょうど境界の
/// 高さのとき、実際には収まらない `Flat` を選んでしまう
fn estimate_flat_menu_height(spacing: &egui::style::Spacing, has_presets: bool) -> f32 {
    let row_count = FLAT_MENU_ROW_COUNT + usize::from(has_presets);
    let row_height = spacing.interact_size.y + spacing.item_spacing.y;
    let separator_height = spacing.item_spacing.y * 2.0 + 1.0;
    row_count as f32 * row_height + FLAT_MENU_SEPARATOR_COUNT as f32 * separator_height
}

/// 右クリックメニューを平らな一覧にするかサブメニューへ折りたたむかを決める。
///
/// 平らな一覧の見積もり高さ（`flat_height`）が使える高さ（`available_height`、
/// `context_menu_size_limits` の高さ側）に収まるなら `Flat`。
///
/// **境界（ちょうど収まる）は `Flat` に倒す。** `flat_height` は見積もりで
/// あり、実測より大きめに出ることはあっても小さめに出ることは想定していない
/// ため、同点なら操作性の良い平らな一覧を優先してよい。
fn context_menu_layout(available_height: f32, flat_height: f32) -> MenuLayout {
    if flat_height <= available_height {
        MenuLayout::Flat
    } else {
        MenuLayout::Collapsed
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

impl CaptureCardViewer {
    /// 設定ダイアログの操作を処理する。
    ///
    /// ドラフトの反映・保存・クローズをここで行うのは、UI 側に状態と副作用を
    /// 持たせないため（`docs/ARCHITECTURE.md` の「UI は状態を持たない」）。
    fn handle_settings_dialog_action(&mut self, action: ui::SettingsDialogAction) {
        match action {
            ui::SettingsDialogAction::TestSound => self.play_test_sound(),
            ui::SettingsDialogAction::ExportSettings => self.export_settings_to_file(),
            ui::SettingsDialogAction::ImportSettings => self.import_settings_into_draft(),
            ui::SettingsDialogAction::ResetDraft => self.reset_draft_to_defaults(),
            _ => {}
        }

        let transition = ui::SettingsDialogState::transition_for(action);

        if transition.commit_draft {
            if let Ok(mut settings) = self.settings.lock() {
                self.settings_dialog.commit_into(&mut settings);
            }
            // 反映した内容でデバイスを開き直す
            self.apply_settings(false);
            // 「読み込みました。適用してください」の類の案内は役目を終えている。
            // 残すと、反映済みなのにまだ何かする必要があるように読める
            self.settings_dialog.clear_management_message();
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

    /// ホットキーの割り当てを登録し直し、失敗を画面へ出す。
    ///
    /// **`HotkeyManager::apply` を直接呼ばないこと。** 直接呼ぶと、失敗の
    /// 通知と、直ったときのエラー表示の取り下げが抜ける。
    fn apply_hotkey_assignments(&mut self, desired: &BTreeMap<HotkeyAction, String>) {
        self.hotkey_manager.apply(desired);
        self.report_hotkey_errors();
    }

    /// ホットキー入力ダイアログを閉じたときに、一時解除していたホットキーを
    /// 共有設定の内容で登録し直す。
    ///
    /// ダイアログを開いている間に確定した分は既に共有設定（またはドラフト）へ
    /// 書き込まれているので、ここでは常に**共有設定**を見る。ドラフトへ
    /// 書いた分（設定ダイアログが開いたままの場合）はまだ「適用」されていない
    /// ので、共有設定には反映されておらず、ここでも登録し直されない。
    /// 「適用」「OK」を押すまで効かない、という既存の約束どおりの挙動になる
    fn resume_hotkeys_after_capture(&mut self) {
        let desired = match self.settings.lock() {
            Ok(settings) => settings.hotkeys.clone(),
            Err(_) => {
                warn!("ホットキーの再開で settings のロックを取得できない");
                return;
            }
        };
        self.hotkey_manager.resume(&desired);
        self.report_hotkey_errors();
    }

    /// 登録できないものが残っているかを、いまの `hotkey_manager` の状態から
    /// まとめて画面へ反映する。1 件ずつ通知すると、複数まとめて失敗したときに
    /// トーストが上書きされて最後の 1 件しか読めない
    fn report_hotkey_errors(&mut self) {
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

    /// 設定ダイアログの「設定を書き出す」。
    ///
    /// 書き出すのは**実行中の設定**で、編集中のドラフトではない。ドラフトは
    /// まだ「適用」されていない下書きなので、それをファイルとして配ると、
    /// 手元で動いている設定と中身が食い違う。
    ///
    /// `rfd` の保存ダイアログは UI スレッドを止めるモーダルで、出している間は
    /// 映像の更新も止まる。既存の効果音ファイル選択と同じ割り切り。
    /// **設定のロックは先に手放す。** 握ったままダイアログを出すと、
    /// ユーザーが閉じるまで設定に触る全ての経路が止まる。
    fn export_settings_to_file(&mut self) {
        // ロックの結果を先に畳んでから self を可変で借りる。match の中で
        // 失敗を報告しようとすると、MutexGuard の一時値が生きたままになる
        let settings = self.settings.lock().ok().map(|settings| settings.clone());
        let Some(settings) = settings else {
            warn!("設定の書き出しで settings のロックを取得できない");
            self.report_settings_error("設定を読み取れない".to_string());
            return;
        };

        let Some(path) = rfd::FileDialog::new()
            .set_file_name(&settings::export_file_name(&Local::now()))
            .add_filter("設定ファイル", &["toml"])
            .save_file()
        else {
            debug!("設定の書き出しがキャンセルされた");
            return;
        };

        match settings::export_to(&path, &settings) {
            Ok(()) => {
                info!("設定を {} へ書き出した", path.display());
                self.settings_dialog
                    .set_management_message(format!("{} へ書き出しました", path.display()), false);
            }
            Err(e) => {
                error!("設定を {} へ書き出せない: {}", path.display(), e);
                self.report_settings_error(e);
            }
        }
    }

    /// 設定ダイアログの「設定を読み込む」。
    ///
    /// 読めた内容は**ドラフトへ入れるだけ**で、実行中の設定には触らない。
    /// 読み込んだ瞬間に反映すると「キャンセル」で取り消せないため、
    /// 他の編集と同じく「適用」「OK」を通す。
    ///
    /// 読めなかった場合はドラフトを一切動かさない。半分だけ読み込んだ状態を
    /// 作ると、どこまでが元の値か分からなくなる。
    fn import_settings_into_draft(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("設定ファイル", &["toml"])
            .pick_file()
        else {
            debug!("設定の読み込みがキャンセルされた");
            return;
        };

        let imported = match settings::import_from(&path) {
            Ok(imported) => imported,
            Err(e) => {
                error!("設定ファイル {} を読み込めない: {}", path.display(), e);
                self.report_settings_error(e);
                return;
            }
        };

        // この操作が返るのはダイアログを描画しているときだけなので、ドラフトは必ずある
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で設定の読み込みが要求された");
            return;
        };
        let merged = ui::draft_from_imported(imported, draft);
        *draft = merged;

        info!("設定ファイル {} を編集中の設定へ読み込んだ", path.display());
        self.settings_dialog.set_management_message(
            format!(
                "{} を読み込みました。「適用」または「OK」で反映します",
                path.display()
            ),
            false,
        );
    }

    /// 設定ダイアログの「設定を初期化」。
    ///
    /// 読み込みと同じくドラフトを差し替えるだけ。確認は `show_other_tab` の
    /// 2 段階ボタンで済んでいるので、ここでは聞き直さない。
    fn reset_draft_to_defaults(&mut self) {
        // 読み込みと同じく、返るのはダイアログを描画しているときだけ
        let Some(draft) = self.settings_dialog.draft_mut() else {
            warn!("ドラフトが無い状態で設定の初期化が要求された");
            return;
        };
        let defaults = ui::draft_from_defaults(draft);
        *draft = defaults;

        info!("編集中の設定を初期値へ戻した");
        self.settings_dialog.set_management_message(
            "初期値に戻しました。「適用」または「OK」で反映します".to_string(),
            false,
        );
    }

    /// 設定ファイルの読み書きの失敗を、トーストとダイアログの両方へ出す。
    ///
    /// トーストは画面下部に出るため、設定ダイアログの位置によっては隠れる。
    /// 操作したその場にも理由が残るようにする。
    fn report_settings_error(&mut self, reason: String) {
        self.settings_dialog
            .set_management_message(status::format_message(ErrorSource::Settings, &reason), true);
        self.report_error(ErrorSource::Settings, reason);
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
    use image::GenericImageView;
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
    fn context_menu_size_limits_wide_screen_keeps_the_fixed_width() {
        // 1280x720 の内側。幅は既定のまま、高さだけ画面から決まる
        let (width, max_height) =
            context_menu_size_limits(egui::vec2(1280.0, 720.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 240.0);
        assert_eq!(max_height, 684.0);
    }

    #[test]
    fn context_menu_size_limits_narrow_screen_shrinks_the_width() {
        // 幅 200px のウィンドウ。既定の 240px のままだと右側が画面外へ出る
        let (width, _) = context_menu_size_limits(egui::vec2(200.0, 720.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 164.0);
    }

    #[test]
    fn context_menu_size_limits_tiny_screen_clamps_to_zero() {
        // 枠と余白だけで画面を使い切る大きさ。負にはしない
        let (width, max_height) =
            context_menu_size_limits(egui::vec2(20.0, 30.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 0.0);
        assert_eq!(max_height, 0.0);
    }

    #[test]
    fn context_menu_layout_fits_within_available_height_is_flat() {
        assert_eq!(context_menu_layout(500.0, 480.0), MenuLayout::Flat);
    }

    #[test]
    fn context_menu_layout_exact_fit_is_flat() {
        // ちょうど収まる境界は、はみ出す側ではなく平らな一覧を優先する
        assert_eq!(context_menu_layout(480.0, 480.0), MenuLayout::Flat);
    }

    #[test]
    fn context_menu_layout_overflow_by_a_hair_is_collapsed() {
        assert_eq!(context_menu_layout(480.0, 480.1), MenuLayout::Collapsed);
    }

    #[test]
    fn context_menu_layout_zero_available_height_is_collapsed() {
        // 高さが取れない画面では、平らな一覧は絶対に収まらない
        assert_eq!(context_menu_layout(0.0, 1.0), MenuLayout::Collapsed);
    }

    #[test]
    fn context_menu_layout_huge_available_height_is_flat() {
        assert_eq!(context_menu_layout(f32::MAX, 480.0), MenuLayout::Flat);
    }

    #[test]
    fn estimate_flat_menu_height_with_default_style_is_positive() {
        let spacing = egui::Style::default().spacing;
        assert!(estimate_flat_menu_height(&spacing, false) > 0.0);
    }

    #[test]
    fn estimate_flat_menu_height_grows_with_row_height() {
        // 行が高くなるほど見積もりも大きくなること。逆行すると、
        // フォントサイズを上げたときに折りたたみ判定が正しく働かなくなる
        let mut spacing = egui::Style::default().spacing;
        let base = estimate_flat_menu_height(&spacing, false);

        spacing.interact_size.y *= 2.0;
        let taller = estimate_flat_menu_height(&spacing, false);

        assert!(taller > base);
    }

    #[test]
    fn estimate_flat_menu_height_with_presets_adds_one_row() {
        // プリセットがあると「プリセット」の行が 1 つ増える。ここが
        // ずれると、プリセットがある状態でだけ折りたたみ判定を誤る
        let spacing = egui::Style::default().spacing;
        let without_presets = estimate_flat_menu_height(&spacing, false);
        let with_presets = estimate_flat_menu_height(&spacing, true);

        let row_height = spacing.interact_size.y + spacing.item_spacing.y;
        assert!((with_presets - without_presets - row_height).abs() < 1e-3);
    }

    #[test]
    fn context_menu_layout_presets_row_tips_the_boundary_to_collapsed() {
        // プリセットが無ければちょうど収まる高さでも、プリセットの分だけ
        // 見積もりが増えると収まらなくなり、Collapsed へ倒れる
        let spacing = egui::Style::default().spacing;
        let flat_without_presets = estimate_flat_menu_height(&spacing, false);
        let flat_with_presets = estimate_flat_menu_height(&spacing, true);

        assert_eq!(
            context_menu_layout(flat_without_presets, flat_without_presets),
            MenuLayout::Flat
        );
        assert_eq!(
            context_menu_layout(flat_without_presets, flat_with_presets),
            MenuLayout::Collapsed
        );
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
