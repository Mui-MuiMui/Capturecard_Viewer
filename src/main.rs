#![windows_subsystem = "windows"]

use chrono::Local;
use eframe::egui;
use image::GenericImageView;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod audio;
mod screenshot;
mod settings;
mod ui;
mod video;

use audio::AudioCapture;
use screenshot::ScreenshotManager;
use settings::AppSettings;
use video::VideoCapture;

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

pub struct CaptureCardViewer {
    settings: Arc<Mutex<AppSettings>>,
    video_capture: Arc<Mutex<VideoCapture>>,
    audio_capture: Arc<Mutex<AudioCapture>>,
    screenshot_manager: Arc<Mutex<ScreenshotManager>>,

    // UI状態管理
    show_settings: bool,
    // 設定ダイアログのドラフト。共有設定を直接書き換えないための置き場所
    settings_dialog: ui::SettingsDialogState,
    show_context_menu: bool,
    show_hotkey_dialog: bool,
    context_menu_pos: egui::Pos2,
    is_fullscreen: bool,
    maintain_aspect_ratio: bool,
    volume: f32,
    last_volume_sent: f32,
    last_settings_applied: Instant,
    // 設定に未保存の変更があるときの、最後に変更された時刻。
    // None は保留中の変更が無いことを表す
    settings_dirty_since: Option<Instant>,

    // 映像表示関連
    video_texture: Option<egui::TextureHandle>,
    // テクスチャへ反映済みのフレーム世代。新着が無いフレームでは更新をまるごと省く
    last_frame_generation: u64,
    pending_hotkey: Option<String>,
    temp_hotkey: String, // ホットキーダイアログ用の一時保存
    // 最後に適用した実行時パラメータ（差分ベースの再起動回避用）
    last_video_device: Option<String>,
    last_video_res: Option<(u32, u32)>,
    last_video_format: Option<String>,
    last_audio_device: Option<String>,
    last_audio_rate: Option<u32>,
    last_audio_channels: Option<u16>,
    last_fullscreen_toggle: Option<Instant>,
    last_video_fps: Option<u32>,
    // 最後に適用したスクリーンショット関連の値
    // apply_settings が 2 秒ごとに呼ばれるため、差分がないときは再適用しない
    last_hotkey: Option<String>,
    last_sound_file: Option<PathBuf>,

    audio_last_error: Option<String>,

    // 起動時遅延接続
    startup_time: Option<Instant>,
    delayed_connection_triggered: bool,

    // UI性能向上のためのデバイスリストキャッシュ
    // ビデオは (デバイス名, 説明) の組
    cached_video_devices: Vec<(String, String)>,
    cached_input_devices: Vec<String>,
    cached_output_devices: Vec<String>,
    last_device_list_update: Option<Instant>,

    // ウィンドウ管理
    always_on_top: bool,
}

impl Default for CaptureCardViewer {
    fn default() -> Self {
        let (loaded_settings, load_outcome) = AppSettings::load();
        let settings = Arc::new(Mutex::new(loaded_settings));
        let video_capture = Arc::new(Mutex::new(VideoCapture::new()));
        #[allow(clippy::arc_with_non_send_sync)] // 音声キャプチャは非同期処理で必要
        let audio_capture = Arc::new(Mutex::new(AudioCapture::new()));
        let screenshot_manager = Arc::new(Mutex::new(ScreenshotManager::new()));

        let app = Self {
            settings,
            video_capture,
            audio_capture,
            screenshot_manager,
            show_settings: false,
            settings_dialog: ui::SettingsDialogState::default(),
            show_context_menu: false,
            show_hotkey_dialog: false,
            context_menu_pos: egui::Pos2::ZERO,
            is_fullscreen: false,
            maintain_aspect_ratio: true,
            volume: 100.0,
            last_volume_sent: -1.0,
            last_settings_applied: Instant::now(),
            settings_dirty_since: None,
            video_texture: None,
            last_frame_generation: 0,
            pending_hotkey: None,
            temp_hotkey: String::new(),
            last_video_device: None,
            last_video_res: None,
            last_video_format: None,
            last_audio_device: None,
            last_audio_rate: None,
            last_audio_channels: None,
            last_fullscreen_toggle: None,
            last_video_fps: None,
            last_hotkey: None,
            last_sound_file: None,

            audio_last_error: None,
            // 起動時遅延接続
            startup_time: Some(Instant::now()),
            delayed_connection_triggered: false,

            // UI性能向上のためのデバイスリストキャッシュ
            cached_video_devices: Vec::new(),
            cached_input_devices: Vec::new(),
            cached_output_devices: Vec::new(),
            last_device_list_update: None,

            // ウィンドウ管理
            always_on_top: false,
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
                    println!("Debug: Available input devices: {:?}", list);
                    if let Some(name) = list.first() {
                        s.audio.input_device_name = Some(name.clone());
                        println!("Debug: Set default input device: {}", name);
                    }
                }
                if s.audio.output_device_name.is_none() {
                    // 出力デバイスはデフォルト（None）で自動選択させる
                    s.audio.output_device_name = None;
                    println!("Debug: Using default output device");
                }
                // 読めなかった設定ファイルを退避できなかった場合は書き戻さない。
                // ここで上書きすると、ディスクに残っている壊れたファイルが既定値で
                // 潰れ、ユーザーが設定を取り戻す最後の手段が消える。
                if load_outcome.may_write_defaults_on_startup() {
                    s.save();
                }
            }
        }
        // 注: デバイス接続は起動から2秒後に遅延実行される
        app
    }
}

impl eframe::App for CaptureCardViewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 遅延デバイス接続（起動から3秒後に実行し、画面投影問題を解決）
        if !self.delayed_connection_triggered {
            if let Some(startup_time) = self.startup_time {
                if startup_time.elapsed().as_secs_f32() >= 2.0 {
                    println!("Starting delayed device connection and auto-refresh (2 seconds after startup)");
                    // 強制リフレッシュのため、last_*をクリアしてからapply_settings
                    println!("Debug: Clearing last device states for forced refresh");
                    self.last_video_device = None;
                    self.last_audio_device = None;
                    println!("Debug: Calling apply_settings(initial=true)");
                    self.apply_settings(true);

                    // 初期設定後にエラーハンドリング付きでウィンドウレベル設定を適用
                    if let Err(e) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            if self.always_on_top {
                                egui::WindowLevel::AlwaysOnTop
                            } else {
                                egui::WindowLevel::Normal
                            },
                        ));
                    })) {
                        eprintln!("Warning: Failed to set window level: {:?}", e);
                    }

                    self.delayed_connection_triggered = true;
                    println!("Debug: Delayed connection sequence completed");
                }
            }
        }

        // ビデオフレームを更新
        self.update_video_texture(ctx);

        // グローバルホットキーを処理
        self.handle_hotkeys();

        // 定期的に実行時設定が保存設定と一致することを確認（外部変更に対応）
        if self.last_settings_applied.elapsed().as_secs_f32() > 2.0 {
            if let Err(e) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.apply_settings(false);
            })) {
                eprintln!("Warning: Failed to apply settings: {:?}", e);
                // タイマーをリセットして連続的なエラー出力を防止
                self.last_settings_applied = Instant::now();
            }
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
            let action = ui::show_settings_dialog(
                ctx,
                &mut self.show_settings,
                &mut self.settings_dialog,
                &mut self.show_hotkey_dialog,
                &video_devices,
                &input_devices,
                &output_devices,
            );
            self.handle_settings_dialog_action(action);
        }

        // ホットキーキャプチャダイアログ
        if self.show_hotkey_dialog {
            // ダイアログが開かれた時に現在の設定値をtemp_hotkeyに設定。
            // 設定ダイアログから開かれた場合は、編集中のドラフトの値を見せる
            if self.temp_hotkey.is_empty() {
                let current = match self.settings_dialog.draft() {
                    Some(draft) => draft.screenshot.hotkey.clone(),
                    None => self
                        .settings
                        .lock()
                        .ok()
                        .and_then(|settings| settings.screenshot.hotkey.clone()),
                };
                self.temp_hotkey = current.unwrap_or_default();
            }

            let hotkey_captured = ui::show_hotkey_capture_dialog(
                ctx,
                &mut self.show_hotkey_dialog,
                &mut self.temp_hotkey,
            );

            // ホットキーがキャプチャされた場合、設定を更新
            if hotkey_captured && !self.temp_hotkey.is_empty() {
                // 設定ダイアログから開かれている場合はドラフトへ書く。
                // 共有設定へ直接書くと、ダイアログの OK がドラフトの古い値で
                // 上書きして、設定したホットキーが消える
                let wrote_to_draft = match self.settings_dialog.draft_mut() {
                    Some(draft) => {
                        draft.screenshot.hotkey = Some(self.temp_hotkey.clone());
                        true
                    }
                    None => false,
                };

                if !wrote_to_draft {
                    // 設定ダイアログが閉じられた状態でホットキーだけ確定した場合。
                    // ドラフトが無いので共有設定へ直接書き、その場で登録する
                    if let Ok(mut settings) = self.settings.lock() {
                        settings.screenshot.hotkey = Some(self.temp_hotkey.clone());
                    }
                    self.mark_settings_dirty();
                    self.pending_hotkey = Some(self.temp_hotkey.clone());
                }
                // ドラフトへ書いた場合はここで登録しない。
                // 登録すると、2 秒ごとの apply_settings が共有設定側の古い
                // ホットキーを見て登録し直し、「適用」も押していないのに
                // 効いたり戻ったりする。実際の登録は「適用」か「OK」で行う
            }

            // ダイアログが閉じられた時にtemp_hotkeyをクリア
            if !self.show_hotkey_dialog {
                self.temp_hotkey.clear();
            }
        }

        // コンテキストメニュー
        if self.show_context_menu {
            self.show_context_menu(ctx);
        }

        // フルスクリーン切替オーバーレイ (1秒表示)
        if let Some(t) = self.last_fullscreen_toggle {
            if t.elapsed().as_secs_f32() < 1.0 {
                egui::Area::new("fullscreen_overlay")
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(20.0, 20.0))
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(egui::Color32::from_black_alpha(160))
                            .rounding(5.0)
                            .show(ui, |ui| {
                                ui.label(if self.is_fullscreen {
                                    "フルスクリーン ON"
                                } else {
                                    "フルスクリーン OFF"
                                });
                            });
                    });
            }
        }

        // 新しくキャプチャされたホットキーを即座に登録
        if let Some(hk) = self.pending_hotkey.take() {
            println!("Registering new hotkey: {}", hk);
            if let Ok(mut ss) = self.screenshot_manager.lock() {
                match ss.set_hotkey(&hk) {
                    Ok(()) => {
                        println!("Hotkey registered successfully: {}", hk);
                        // apply_settings が同じホットキーを登録し直さないよう記録する
                        self.last_hotkey = Some(hk.clone());
                    }
                    Err(e) => {
                        println!("Failed to register hotkey {}: {}", hk, e);
                        // 登録できていないので apply_settings 側で再試行させる
                        self.last_hotkey = None;
                    }
                }
            } else {
                println!("Failed to lock screenshot_manager for hotkey registration");
            }
        }
        // テストサウンドリクエストを処理
        if crate::ui::should_play_test_sound() {
            // 設定ダイアログを開いている間はドラフトの音量で鳴らす。
            // スライダーを動かした結果をその場で確かめられるようにするため。
            // 効果音のファイル自体は「適用」か「OK」まで差し替わらない
            let volume = match self.settings_dialog.draft() {
                Some(draft) => Some(draft.screenshot.sound_volume),
                None => self
                    .settings
                    .lock()
                    .ok()
                    .map(|settings| settings.screenshot.sound_volume),
            };

            if let Some(volume) = volume {
                if let Ok(ss) = self.screenshot_manager.lock() {
                    ss.play_screenshot_sound(volume);
                }
            }
        }

        // 保留中の設定変更を、操作が落ち着いたところでまとめて書き出す
        self.flush_settings_if_due(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 終了時は必ず書き出す。デバウンスの待ち時間中に終了しても、
        // ウィンドウのサイズ・位置や音量の変更を取りこぼさないようにする
        self.save_settings_now();
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

    fn handle_hotkeys(&mut self) {
        let should_screenshot = {
            if let Ok(screenshot_manager) = self.screenshot_manager.lock() {
                let pressed = screenshot_manager.is_hotkey_pressed();
                if pressed {
                    println!("Main: Screenshot should be taken");
                }
                pressed
            } else {
                println!("Main: Failed to lock screenshot_manager");
                false
            }
        };

        if should_screenshot {
            println!("Main: Taking screenshot now");
            self.take_screenshot();
        }
    }

    fn take_screenshot(&mut self) {
        println!("take_screenshot: Starting screenshot process");

        // 最新フレームの生データを抽出。
        // スクリーンショットはいま画面に出ている画を保存するので、新着でなくてよい
        if let Ok(video) = self.video_capture.lock() {
            if let Some(frame) = video.get_latest_frame() {
                println!(
                    "take_screenshot: Got video frame {}x{}",
                    frame.width, frame.height
                );

                // タイムスタンプとパスを構築
                let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S-%3f").to_string();
                if let Ok(settings) = self.settings.lock() {
                    let path = settings.get_screenshot_path(&timestamp);
                    println!("take_screenshot: Saving to {:?}", path);

                    // 親ディレクトリを作成
                    if let Some(parent) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            println!("take_screenshot: Failed to create directories: {}", e);
                        }
                    }

                    // RGBデータを画像に変換して保存
                    // image クレートが Vec の所有権を要求するため、ここだけは複製が要る
                    if let Some(img_buf) = image::RgbImage::from_raw(
                        frame.width as u32,
                        frame.height as u32,
                        frame.data.clone(),
                    ) {
                        match img_buf.save(&path) {
                            Ok(()) => {
                                println!(
                                    "take_screenshot: Screenshot saved successfully to {:?}",
                                    path
                                );
                                let volume = settings.screenshot.sound_volume;
                                if let Ok(ss) = self.screenshot_manager.lock() {
                                    ss.play_screenshot_sound(volume);
                                }
                            }
                            Err(e) => println!("take_screenshot: Failed to save image: {}", e),
                        }
                    } else {
                        println!("take_screenshot: Failed to create RgbImage from raw data");
                    }
                } else {
                    println!("take_screenshot: Failed to lock settings");
                }
            } else {
                println!("take_screenshot: No video frame available");
            }
        } else {
            println!("take_screenshot: Failed to lock video_capture");
        }
    }

    fn show_windowed_ui(&mut self, ctx: &egui::Context) {
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
                    if response.dragged() {
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

                    // 音量調整のためのスクロールを処理
                    if response.hovered() {
                        ctx.input(|i| {
                            if i.raw_scroll_delta.y > 0.0 {
                                self.volume = (self.volume + 10.0).min(200.0);
                                // 設定に反映してリセットを防ぐ。書き出しはデバウンスする
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                }
                                self.mark_settings_dirty();
                            } else if i.raw_scroll_delta.y < 0.0 {
                                self.volume = (self.volume - 10.0).max(0.0);
                                // 設定に反映してリセットを防ぐ。書き出しはデバウンスする
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                }
                                self.mark_settings_dirty();
                            }
                        });
                    }
                } else {
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label("映像信号がありません");
                    });

                    // 空エリアでのウィンドウドラッグを処理（設定が有効な場合のみ）
                    if response.dragged() {
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
                }
            });
    }

    fn show_fullscreen_ui(&mut self, ctx: &egui::Context) {
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

                    // マウススクロールでの音量調整（ウィンドウ版と同じ機能）
                    if response.hovered() {
                        ctx.input(|i| {
                            if i.raw_scroll_delta.y > 0.0 {
                                self.volume = (self.volume + 10.0).min(200.0);
                                // 設定に反映してリセットを防ぐ。書き出しはデバウンスする
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                }
                                self.mark_settings_dirty();
                            } else if i.raw_scroll_delta.y < 0.0 {
                                self.volume = (self.volume - 10.0).max(0.0);
                                // 設定に反映してリセットを防ぐ。書き出しはデバウンスする
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                }
                                self.mark_settings_dirty();
                            }
                        });
                    }
                } else {
                    // 映像信号がない場合
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label("映像信号がありません");
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
                }
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
                    let volume_response =
                        ui.add(egui::Slider::new(&mut self.volume, 0.0..=200.0).suffix("%"));

                    // 音量が変更された場合、設定に反映する（書き出しはデバウンス）
                    if volume_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.volume = self.volume;
                        }
                        self.mark_settings_dirty();
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

                    // 最前面表示設定が変更された場合
                    if always_on_top_response.changed() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            if self.always_on_top {
                                egui::WindowLevel::AlwaysOnTop
                            } else {
                                egui::WindowLevel::Normal
                            },
                        ));

                        // 設定に反映する（書き出しはデバウンス）
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.always_on_top = self.always_on_top;
                        }
                        self.mark_settings_dirty();
                    }

                    // フルスクリーン表示のチェックボックス
                    let fullscreen_response =
                        ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");

                    // フルスクリーン状態が変更された場合
                    if fullscreen_response.changed() {
                        self.toggle_fullscreen(ctx, self.is_fullscreen);
                    }

                    // 画面ドラッグ移動のチェックボックス
                    let enable_drag_move = if let Ok(settings) = self.settings.lock() {
                        settings.ui.enable_drag_move
                    } else {
                        true
                    };
                    let mut temp_enable_drag_move = enable_drag_move;
                    let drag_move_response =
                        ui.checkbox(&mut temp_enable_drag_move, "画面ドラッグ移動");

                    // 画面ドラッグ移動設定が変更された場合（書き出しはデバウンス）
                    if drag_move_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.enable_drag_move = temp_enable_drag_move;
                        }
                        self.mark_settings_dirty();
                    }

                    ui.separator();
                    if ui.button("デバイス再接続").clicked() {
                        // 強制的にデバイス再接続（last_*をクリアして強制再接続）
                        self.last_video_device = None;
                        self.last_audio_device = None;
                        self.apply_settings(false);
                        close_menu = true;
                    }
                    ui.separator();
                    if ui.button("詳細設定...").clicked() {
                        self.show_settings = true;
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
    // 設定から保存されたウィンドウサイズと位置を読み込む。
    // ここでは読み込み結果を使わない。既定値の書き戻しは
    // CaptureCardViewer::default 側だけで行うため。
    let (settings, _) = AppSettings::load();
    let mut viewport_builder = egui::ViewportBuilder::default().with_icon(load_icon());

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

    fn apply_settings(&mut self, initial: bool) {
        if let Ok(settings) = self.settings.lock() {
            // Video - リトライ機能付き
            if let Ok(mut video) = self.video_capture.lock() {
                let need_video_restart = settings.video.device_name != self.last_video_device
                    || settings.video.resolution != self.last_video_res
                    || settings.video.format != self.last_video_format
                    || settings.video.fps != self.last_video_fps;

                if settings.video.device_name.is_some() && (need_video_restart || initial) {
                    println!(
                        "Debug: Starting video device connection: {:?}",
                        settings.video.device_name
                    );
                    let mut video_success = false;
                    let max_retries = if initial { 3 } else { 1 };

                    for attempt in 0..max_retries {
                        if attempt > 0 {
                            println!(
                                "Video device connection attempt {} of {}",
                                attempt + 1,
                                max_retries
                            );
                            std::thread::sleep(std::time::Duration::from_millis(1000));
                        }

                        match video.start_capture(
                            settings.video.device_name.as_deref(),
                            settings.video.resolution,
                            settings.video.format.as_deref(),
                            settings.video.fps,
                        ) {
                            Ok(_) => {
                                println!("Debug: Video device connected successfully");
                                video_success = true;
                                break;
                            }
                            Err(e) => {
                                println!("Video capture failed (attempt {}): {}", attempt + 1, e);
                                if attempt < max_retries - 1 {
                                    continue;
                                }
                            }
                        }
                    }

                    if video_success {
                        self.last_video_device = settings.video.device_name.clone();
                        self.last_video_res = settings.video.resolution;
                        self.last_video_format = settings.video.format.clone();
                        self.last_video_fps = settings.video.fps;
                    }
                }
            }

            // Audio - 改良されたリトライとデフォルト設定
            if let Ok(mut audio) = self.audio_capture.lock() {
                // ストリームを開始する前にパススルーの設定を反映する。
                // 開始後に反映すると、無効のまま起動したときに最初のバッファが出力されてしまう。
                audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);

                let need_audio_restart = settings.audio.input_device_name != self.last_audio_device
                    || settings.audio.sample_rate != self.last_audio_rate
                    || settings.audio.channels != self.last_audio_channels
                    || initial; // 起動時は必ず接続試行

                if need_audio_restart {
                    println!("Debug: Starting audio device connection");
                    println!(
                        "Debug: Input device: {:?}",
                        settings.audio.input_device_name
                    );
                    println!(
                        "Debug: Output device: {:?}",
                        settings.audio.output_device_name
                    );

                    // まずは利用可能なデバイスをリスト
                    let input_devices = audio.list_input_devices();
                    let output_devices = audio.list_output_devices();
                    println!("Debug: Available input devices: {:?}", input_devices);
                    println!("Debug: Available output devices: {:?}", output_devices);

                    let mut audio_success = false;
                    let max_retries = if initial { 5 } else { 2 }; // 起動時により多くリトライ

                    for attempt in 0..max_retries {
                        if attempt > 0 {
                            println!(
                                "Audio device connection attempt {} of {}",
                                attempt + 1,
                                max_retries
                            );
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }

                        // 接続試行
                        match audio.start_passthrough_with_settings(
                            settings.audio.input_device_name.as_deref(),
                            settings.audio.output_device_name.as_deref(),
                            settings.audio.sample_rate,
                            settings.audio.channels,
                        ) {
                            Ok(_) => {
                                println!("Debug: Audio devices connected successfully");
                                self.audio_last_error = None;
                                audio_success = true;
                                break;
                            }
                            Err(e) => {
                                println!("Audio capture failed (attempt {}): {}", attempt + 1, e);
                                self.audio_last_error = Some(e.clone());

                                // 3回目以降のリトライではデフォルトデバイスを試行
                                if attempt == 2 && initial {
                                    println!("Debug: Trying with default devices...");
                                    match audio
                                        .start_passthrough_with_settings(None, None, None, None)
                                    {
                                        Ok(_) => {
                                            println!("Debug: Audio connected with default devices");
                                            self.audio_last_error = None;
                                            audio_success = true;
                                            break;
                                        }
                                        Err(e2) => {
                                            println!(
                                                "Default audio connection also failed: {}",
                                                e2
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if audio_success {
                        self.last_audio_device = settings.audio.input_device_name.clone();
                        self.last_audio_rate = settings.audio.sample_rate;
                        self.last_audio_channels = settings.audio.channels;
                    } else {
                        println!("Debug: All audio connection attempts failed");
                    }
                }

                // 音量を適用
                self.volume = settings.ui.volume;
                audio.set_volume(self.volume);
            }

            // UI設定
            self.maintain_aspect_ratio = settings.ui.maintain_aspect_ratio;
            self.always_on_top = settings.ui.always_on_top;

            // スクリーンショット設定
            if let Ok(mut ss) = self.screenshot_manager.lock() {
                if let Some(hk) = &settings.screenshot.hotkey {
                    // 無条件に登録し直すと、2 秒ごとに unregister → register が走って
                    // その瞬間のキー入力を取りこぼし、リスナースレッドも作り直される
                    if Self::needs_reapply(initial, hk, &self.last_hotkey) {
                        match ss.set_hotkey(hk) {
                            Ok(()) => self.last_hotkey = Some(hk.clone()),
                            // 失敗すると古いホットキーは解除済みで何も登録されていない。
                            // last を空にして次の適用タイミングで再試行する
                            Err(_) => self.last_hotkey = None,
                        }
                    }
                }
                if let Some(sf) = &settings.screenshot.sound_file {
                    // 無条件に呼ぶと 2 秒ごとに効果音ファイル全体を読み直すことになる
                    if Self::needs_reapply(initial, sf, &self.last_sound_file) {
                        match ss.set_sound_file(sf) {
                            Ok(()) => self.last_sound_file = Some(sf.clone()),
                            // 見つからない場合は埋め込みの既定音へ倒して Ok になる。
                            // ここへ来るのはファイルがあるのに読めなかった場合なので、
                            // last を空にして次の適用タイミングで読み直す
                            Err(_) => self.last_sound_file = None,
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
            // デバウンスを待たずに書き出す
            self.save_settings_now();
        }

        if transition.close {
            self.settings_dialog.end_edit();
            self.show_settings = false;
        }
    }

    /// 設定に未保存の変更があることを記録する。
    /// 実際の書き出しは `flush_settings_if_due` がまとめて行う。
    fn mark_settings_dirty(&mut self) {
        self.settings_dirty_since = Some(Instant::now());
    }

    /// 保留の有無にかかわらず、いま設定をディスクへ書き出す。
    fn save_settings_now(&mut self) {
        // ロックが取れなかった場合は保留のままにして、次の機会に書き出す
        let Ok(settings) = self.settings.lock() else {
            return;
        };

        if settings.save() {
            self.settings_dirty_since = None;
        } else {
            // 書き出せなかった変更を保存済みとして捨てず、保留のまま残す。
            // 時刻を入れ直しているのは、失敗が続いたときに毎フレーム
            // 書き込みを試みる状態へ戻さないため
            self.settings_dirty_since = Some(Instant::now());
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

    fn toggle_fullscreen(&mut self, ctx: &egui::Context, to_full: bool) {
        use eframe::egui::ViewportCommand;

        if to_full {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
            self.is_fullscreen = true;
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.is_fullscreen = false;
        }

        self.last_fullscreen_toggle = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;

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
}
