//! アプリの状態 `CaptureCardViewer` と、その振る舞い。
//!
//! `eframe::App` の実装（`update` / `on_exit`）と構造体の定義をここに置き、
//! 個々の処理は役割ごとの子モジュールへ分けてある。子モジュールはどれも
//! `impl CaptureCardViewer` を足す形で、状態そのものは増やさない。

mod capabilities;
mod device;
mod menu;
mod monitor;
mod retry;
mod view;
mod window;

use self::capabilities::{AudioCapabilityResult, CapabilityResult};
use self::device::{AudioTarget, VideoTarget};
use self::menu::MenuLayout;
use self::monitor::VideoLinkAction;
use self::retry::ConnectRetry;
use self::window::needs_drag_move_guard;
use crate::audio::AudioCapture;
use crate::hotkey::{HotkeyAction, HotkeyError, HotkeyManager};
use crate::overlay::{OverlayContent, TransientOverlay};
use crate::repaint::{next_repaint_delay, should_wake_on_event, RepaintCondition, RepaintWaker};
use crate::screenshot::{self, ScreenshotManager};
use crate::settings::{
    self, AppSettings, AutoSavePolicy, ColorRange, ColorSpace, ScreenshotEncoding, MAX_VOLUME,
    MIN_VOLUME,
};
use crate::status::{self, ConnectionStatus, ErrorCenter, ErrorSource, LinkStatus};
use crate::ui;
use crate::video::{self, VideoAdjustments, VideoCapture};
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;
    use tempfile::tempdir;

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
