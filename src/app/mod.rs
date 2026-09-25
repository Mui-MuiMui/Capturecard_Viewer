//! アプリの状態 `CaptureCardViewer` と、その振る舞い。
//!
//! `eframe::App` の実装（`update` / `on_exit`）と構造体の定義をここに置き、
//! 個々の処理は役割ごとの子モジュールへ分けてある。子モジュールはどれも
//! `impl CaptureCardViewer` を足す形で、状態そのものは増やさない。

mod audio_control;
mod backend;
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
mod worker;
mod worker_connect;
mod worker_loop;
mod worker_timers;

use self::menu::MenuLayout;
use self::screenshot::ScreenshotResult;
use self::window::needs_drag_move_guard;
use self::worker::{DeviceSnapshot, DeviceWorker};
use crate::audio::AudioControls;
use crate::hotkey::{HotkeyAction, HotkeyManager};
use crate::overlay::TransientOverlay;
use crate::repaint::{next_repaint_delay, should_wake_on_event, RepaintCondition, RepaintWaker};
use crate::screenshot::ScreenshotManager;
use crate::settings::{AppSettings, AutoSavePolicy, ColorRange, ColorSpace};
use crate::status::ErrorCenter;
use crate::ui;
use crate::video::{SharedColorConversion, VideoAdjustments, VideoFrames};
use eframe::egui;
use log::{debug, info, warn};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

pub struct CaptureCardViewer {
    settings: Arc<Mutex<AppSettings>>,
    // デバイスを開く・閉じる・列挙する処理の窓口。
    //
    // **`VideoCapture` / `AudioCapture` を UI スレッドが直接持たない。**
    // どちらもワーカースレッドの中にあり、ここからはコマンドを送って
    // イベントを受け取るだけ（`app::worker`）
    device: DeviceWorker,
    // ワーカーが定期的に更新する観測値。update() の先頭で 1 回だけ読む
    device_snapshot: DeviceSnapshot,
    // 映像フレームの共有ハンドル。**ここだけはチャネルを通さない。**
    // コマンドの列に並べると遅延が増えるため、フレームコールバックスレッドと
    // 直接共有する
    frames: VideoFrames,
    // 色空間・レンジ・明るさ・コントラスト・彩度。
    // フレームコールバックが毎フレーム読む Atomic で、デバイスを開き直さずに効く
    color_conversion: Arc<SharedColorConversion>,
    // 音量・ミュート・パススルー。出力コールバックが 1 回ごとに読む Atomic
    audio_controls: Arc<AudioControls>,
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
    // 最後に共有 Atomic へ入れた色変換の設定。
    // デバイスの開き直しは伴わないが、2 秒ごとの再適用で同じ値を
    // ログへ出さないよう差分で判定する
    last_color_conversion: Option<(ColorSpace, ColorRange)>,
    // 最後に共有 Atomic へ入れた映像調整（明るさ・コントラスト・彩度）。
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

    // 起動直後に 1 度だけ行う処理を済ませたか
    startup_applied: bool,

    // 設定ダイアログの選択肢に出すデバイス一覧。
    // 列挙はワーカーが行い、結果が届いたらここへ写す。
    // ビデオは (デバイス名, 説明) の組
    cached_video_devices: Vec<(String, String)>,
    cached_input_devices: Vec<String>,
    cached_output_devices: Vec<String>,
    // 最後に取り直しを要求した時刻。結果の到着ではなく要求で進める
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
        // 表示状態はここで読み込んだ値をそのままフィールドの初期値にする。
        // apply_settings(true) も最初の update() で同じ値を書き戻すが、
        // 構築時点で確定させておけば以降の初期化順序に依存せずに済む
        let show_stats_overlay = loaded_settings.ui.show_stats_overlay;
        let settings = Arc::new(Mutex::new(loaded_settings));

        // 再描画の窓口は、それを使う相手より先に作る。
        // ここではまだ egui::Context と結びついていないので何もしないが、
        // 複製した先にも最初の update() の bind がそのまま効く
        let repaint_waker = RepaintWaker::new();

        let mut hotkey_manager = HotkeyManager::new();
        hotkey_manager.set_repaint_waker(repaint_waker.clone());

        let screenshot_manager = Arc::new(Mutex::new(ScreenshotManager::new()));
        let (screenshot_tx, screenshot_rx) = std::sync::mpsc::channel();

        // デバイスに触るものは、すべてワーカースレッドの中で作る。
        // ここから渡すのは UI スレッドとも共有する 3 つだけ
        let frames = VideoFrames::new();
        let color_conversion = Arc::new(SharedColorConversion::new());
        let audio_controls = Arc::new(AudioControls::default());
        let device = DeviceWorker::spawn(
            frames.clone(),
            Arc::clone(&color_conversion),
            Arc::clone(&audio_controls),
            // **フレームコールバックへ渡る窓口。** キャプチャを開くより前に
            // 渡す必要があるので、ワーカーの起動時に持たせる
            repaint_waker.clone(),
        );

        let mut app = Self {
            settings,
            device,
            device_snapshot: DeviceSnapshot::default(),
            frames,
            color_conversion,
            audio_controls,
            screenshot_manager,
            hotkey_manager,
            repaint_waker,
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
            last_color_conversion: None,
            last_video_adjustments: None,
            last_sound_file: None,

            startup_applied: false,

            // デバイス一覧のキャッシュ。ワーカーの列挙結果が届いたら入る
            cached_video_devices: Vec::new(),
            cached_input_devices: Vec::new(),
            cached_output_devices: Vec::new(),
            last_device_list_update: None,

            // ウィンドウ管理
            always_on_top: false,
            borderless: false,

            screenshot_save_threads: Vec::new(),
        };

        // 最小化中のホットキーは UI スレッドを通せないので、リスナーから
        // 直接デバイスワーカーへコマンドを積ませる（#133）。
        // **ワーカーを起動したあとでしか渡せない**ので、ここで渡す
        app.hotkey_manager
            .set_background_runner(hotkeys::background_hotkey_runner(
                app.device.command_sender(),
            ));

        // **未設定のデバイス名はここで埋めない。** 列挙は映像で 1〜3ms、
        // 音声で 300ms 前後かかり、ウィンドウが出る前にその分だけ待たせる
        // ことになる。ワーカーが最初の `ApplyConfig` で列挙して決め、
        // `DefaultDevicesResolved` で返してくる（`device::store_resolved_devices`）。
        //
        // **入力デバイスは出力と違い、未設定のままにしない。** 出力の
        // 既定は「スピーカー」でまず無害だが、入力の既定は環境依存
        // （ノート PC ならほぼ確実に内蔵マイク）で、パススルーが
        // そのままマイクの音をスピーカーへ流してしまう。#134（PR #147）
        // で切断時に同じことが起きる不具合を直したばかりで、初回起動で
        // 同じ誤動作を起こすわけにいかない。
        //
        // この結果、入力側の「既定のデバイス」追従は、設定ファイルを手で
        // 編集して input_device_name を消した場合にだけ効く。設定画面の
        // コンボボックスに「デフォルト」の選択肢を足すかどうかは別 Issue で判断する
        {
            if let Ok(mut s) = app.settings.lock() {
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
        //
        // **要求を積むだけで、コマンドとして流すのは最初の `update()`。**
        // ワーカーはコマンドを受けた順に処理するので、ここで流すと
        // 数百 ms かかる能力取得の後ろで最初の接続が待たされる
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

        // 音声デバイスの対応設定はワーカーが開く直前に自分で取りに行くので、
        // ここからは要求しない。設定画面を開いたときの一覧にも、そのとき
        // 返ってきた結果がそのまま入る

        // 注: デバイスの接続は最初の update() の apply_settings(true) で
        // 要求され、失敗したらワーカー側のバックオフで繋がるまで再試行する
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

        // ワーカーから届いた結果（接続の成否、デバイス能力、デバイス一覧）を
        // 取り込む。設定ダイアログを開いていなくても受け取る
        self.drain_device_events();

        // デバイスワーカーの観測値を 1 回だけ読む。以降の描画や
        // 「接続状態」タブはここから引く（ロックを取り直さない）。
        // **イベントを取り込んだ後に読む。** ワーカーはイベントを送る前に
        // 観測値を書き出すので、この順なら少なくともそのイベントの時点の値が入る
        self.device_snapshot = self.device.snapshot();

        // 別スレッドで行ったスクリーンショットの保存結果を取り込む。
        // 失敗はここでトーストになる
        self.drain_screenshot_results();

        // 起動直後に 1 度だけ行う処理。
        //
        // 以前はここで「起動から 2 秒」待ってからデバイスを開いていた。
        // 待つ根拠がコードにもコミットにも残っておらず、実測でも接続自体は
        // 0.1 秒で終わるため、最初のフレームで要求する。開けなかった場合は
        // ワーカー側のバックオフが繋がるまで面倒を見る
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

        // ビデオフレームを更新。新着の時刻は末尾の再描画の予約で使う
        if self.update_video_texture(ctx) {
            self.last_new_frame_at = Some(Instant::now());
        }

        // 接続の再試行、フレームの途絶の検出、音声ストリームのエラーの回収、
        // Windows 側の既定デバイスの追従は、どれもワーカースレッドが自前の
        // タイマーで回す。**`update()` からは何も駆動しない。** 最小化中は
        // ここが呼ばれないため、駆動すると自動再接続が止まる（#133）

        // グローバルホットキーを処理
        self.handle_hotkeys(ctx);

        // 定期的に実行時設定が保存設定と一致することを確認（外部変更に対応）
        if self.last_settings_applied.elapsed().as_secs_f32() > 2.0 {
            self.apply_settings(false);
        }

        // 音量が変更された場合、出力コールバックが読む Atomic へ伝播
        if (self.volume - self.last_volume_sent).abs() > 0.5 {
            self.audio_controls.set_volume(self.volume);
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
            // ダイアログの描画はドラフトを可変で借りるため、`self` を不変で
            // 借りたままにできない。開いている間だけの複製なので、
            // 失敗の一覧が長くても負担にならない
            let hotkey_errors = self.hotkey_manager.errors().clone();
            // ドラフトがまだ無ければ描かない。`begin_edit` が済むまで待つ
            let events = match self.settings_dialog.split_for_draw() {
                Some((draft, view)) => ui::show_settings_dialog(
                    ctx,
                    draft,
                    &view,
                    &devices,
                    &connection,
                    &hotkey_errors,
                ),
                None => Vec::new(),
            };
            self.handle_settings_events(events);
        }

        // 溜まったデバイス能力の取得要求をワーカーへ流す。
        // **設定ダイアログの描画より後に置く。** 起動直後の先読み分も
        // ここで初めて流れるので、最初の `ApplyConfig`（＝接続の要求）が
        // 数百 ms かかる能力取得に追い越されない
        self.dispatch_capability_requests();

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

            let events = ui::show_hotkey_capture_dialog(
                ctx,
                action,
                &existing_hotkeys,
                self.settings_dialog.hotkey_capture().rejection(),
            );

            // **クローズを先に済ませてから確定を処理する。** 登録に失敗した
            // ときはこのあと開き直すので、順序が逆だと開き直した直後に閉じる
            let mut captured = None;
            let mut close_dialog = false;
            for event in events {
                match event {
                    ui::HotkeyDialogEvent::Rejected(reason) => {
                        self.settings_dialog
                            .hotkey_capture_mut()
                            .set_rejection(reason);
                    }
                    ui::HotkeyDialogEvent::Cancelled => {
                        self.settings_dialog.hotkey_capture_mut().reset();
                    }
                    ui::HotkeyDialogEvent::Close => close_dialog = true,
                    ui::HotkeyDialogEvent::Captured(candidate) => captured = Some(candidate),
                }
            }
            if close_dialog {
                self.show_hotkey_dialog = false;
            }

            if let Some(candidate) = captured {
                // 解釈できるかと、押下を観測するフックが使えているかを確かめる。
                // キーは奪わないので、他のアプリが同じキーを使っていても弾かない
                // （#202）。自分の他のアクションとの重複は、ダイアログが
                // 確定の前に弾いている
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
                            .set_rejection(reason.to_string());
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

        // 最小化しているかをホットキーのリスナーへ伝える。最小化すると
        // ここが呼ばれなくなるので、**最後に書いた値がそのまま残る**のが狙い。
        // リスナーは真の間だけ、画面の要らないアクションをワーカーへ回す（#133）。
        // フォーカスは「フォーカスがあるときだけ反応する」の判定に使う（#202）。
        // 出入りのたびに egui-winit が再描画を要求するので、ここで拾える。
        // 取れない環境では「フォーカスあり」に倒す（反応しなくなる側に倒さない）
        let focused = viewport.focused.unwrap_or(true);
        // テキスト欄に入力中かも伝える。キーを奪わないので、プリセット名などへ
        // 打った文字がホットキーとしても実行されてしまう（#206）。
        // **描画を全て終えたここで読む。** このフレームでフォーカスが移った分まで
        // 反映される。フックの中から egui へは問い合わせない
        let typing = ctx.wants_keyboard_input();
        self.hotkey_manager
            .set_window_state(minimized, focused, typing);
        ctx.request_repaint_after(next_repaint_delay(condition));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // デバイスワーカーにストリームを閉じさせ、終わるまで待つ。
        // 待たないと、閉じる途中でプロセスごと落ちる
        self.device.shutdown();

        // **止めたあとに、残っているイベントを取り込む。** 最小化中の
        // ホットキーで変えた音量・ミュートはワーカーが先に効かせ、UI 側の
        // 設定への反映は `DeviceEvent` を受け取ったときに行う（#133）。
        // 最小化したまま終了すると `update()` を通らないので、ここで
        // 取り込まないと操作が設定ファイルに残らない。
        // 止めてから読めば、この後に新しいイベントが積まれることもない
        self.drain_device_events();

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
