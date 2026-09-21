//! アプリの状態 `CaptureCardViewer` と、その振る舞い。
//!
//! `eframe::App` の実装（`update` / `on_exit`）と構造体の定義をここに置き、
//! 個々の処理は役割ごとの子モジュールへ分けてある。子モジュールはどれも
//! `impl CaptureCardViewer` を足す形で、状態そのものは増やさない。

mod audio_control;
mod capabilities;
mod device;
mod error_report;
mod hotkeys;
mod menu;
mod monitor;
mod retry;
mod screenshot;
mod settings_dialog;
mod settings_store;
mod view;
mod window;

use self::capabilities::{AudioCapabilityResult, CapabilityResult};
use self::device::{AudioTarget, VideoTarget};
use self::menu::MenuLayout;
use self::monitor::VideoLinkAction;
use self::retry::ConnectRetry;
use self::screenshot::ScreenshotResult;
use self::window::needs_drag_move_guard;
use crate::audio::AudioCapture;
use crate::hotkey::{HotkeyAction, HotkeyManager};
use crate::overlay::TransientOverlay;
use crate::repaint::{next_repaint_delay, should_wake_on_event, RepaintCondition, RepaintWaker};
use crate::screenshot::ScreenshotManager;
use crate::settings::{AppSettings, AutoSavePolicy, ColorRange, ColorSpace};
use crate::status::ErrorCenter;
use crate::ui;
use crate::video::{VideoAdjustments, VideoCapture};
use eframe::egui;
use log::{debug, info, warn};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

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
            } else {
                warn!("起動時のデバイス自動選択で settings のロックを取得できない");
            }
        }

        // 保存済みのビデオデバイスの能力を先に取りに行く。
        // 設定画面を開いた時点で選択肢が揃っているようにするためで、
        // 以前はデバイスを切り替えたときしか取得していなかったため、
        // 起動後に設定画面を開いても解像度や FPS の選択肢が出なかった。
        // 接続と並行して走るので、設定画面を開く頃には揃っている
        let saved_video_device = match app.settings.lock() {
            Ok(s) => s.video.device_name.clone(),
            Err(_) => {
                warn!("保存済みビデオデバイスの能力の先読みで settings のロックを取得できない");
                None
            }
        };
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
            } else {
                warn!("音量の伝播で audio_capture のロックを取得できない");
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
            } else {
                warn!("ウィンドウの位置・大きさの記録で settings のロックを取得できない");
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
                } else {
                    warn!("設定ダイアログのドラフト作成で settings のロックを取得できない");
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
                None => match self.settings.lock() {
                    Ok(settings) => settings.hotkeys.clone(),
                    Err(_) => {
                        warn!("ホットキーの重複判定で settings のロックを取得できない");
                        Default::default()
                    }
                },
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
                            } else {
                                warn!("ホットキーの確定で settings のロックを取得できない");
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
