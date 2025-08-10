#![windows_subsystem = "windows"]

use eframe::egui;
use chrono::Local;
use std::sync::{Arc, Mutex};
use image::GenericImageView;
use std::time::Instant;
use std::panic::AssertUnwindSafe;

mod settings;
mod video;
mod audio;
mod screenshot;
mod ui;

use settings::AppSettings;
use video::VideoCapture;
use audio::AudioCapture;
use screenshot::ScreenshotManager;

pub struct CaptureCardViewer {
    settings: Arc<Mutex<AppSettings>>,
    video_capture: Arc<Mutex<VideoCapture>>,
    audio_capture: Arc<Mutex<AudioCapture>>,
    screenshot_manager: Arc<Mutex<ScreenshotManager>>,
    
    // UI状態管理
    show_settings: bool,
    show_context_menu: bool,
    show_hotkey_dialog: bool,
    context_menu_pos: egui::Pos2,
    is_fullscreen: bool,
    maintain_aspect_ratio: bool,
    volume: f32,
    last_volume_sent: f32,
    last_settings_applied: Instant,
    
    // 映像表示関連
    video_texture: Option<egui::TextureHandle>,
    pending_hotkey: Option<String>,
    temp_hotkey: String, // ホットキーダイアログ用の一時保存
    // 最後に適用した実行時パラメータ（差分ベースの再起動回避用）
    last_video_device: Option<String>,
    last_video_res: Option<(u32,u32)>,
    last_video_format: Option<String>,
    last_audio_device: Option<String>,
    last_audio_rate: Option<u32>,
    last_audio_channels: Option<u16>,
    last_fullscreen_toggle: Option<Instant>,
    last_video_fps: Option<u32>,

    audio_last_error: Option<String>,
    
    // 起動時遅延接続
    startup_time: Option<Instant>,
    delayed_connection_triggered: bool,
    
    // UI性能向上のためのデバイスリストキャッシュ
    cached_input_devices: Vec<String>,
    cached_output_devices: Vec<String>,
    last_device_list_update: Option<Instant>,
    
    // ウィンドウ管理
    always_on_top: bool,
}

impl Default for CaptureCardViewer {
    fn default() -> Self {
        let settings = Arc::new(Mutex::new(AppSettings::load()));
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
            show_context_menu: false,
            show_hotkey_dialog: false,
            context_menu_pos: egui::Pos2::ZERO,
            is_fullscreen: false,
            maintain_aspect_ratio: true,
            volume: 100.0,
            last_volume_sent: -1.0,
            last_settings_applied: Instant::now(),
            video_texture: None,
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

            audio_last_error: None,
            // 起動時遅延接続
            startup_time: Some(Instant::now()),
            delayed_connection_triggered: false,
            
            // UI性能向上のためのデバイスリストキャッシュ
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
                    if let Some((name,_)) = devices.first() { s.video.device_name = Some(name.clone()); }
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
                s.save();            }
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
                            }
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

        // メインUI
        // F11によるフルスクリーン切り替えを削除（スクリーンショット用に解放）

        if self.is_fullscreen {
            self.show_fullscreen_ui(ctx);
        } else {
            self.show_windowed_ui(ctx);
        }
        
        // 設定ダイアログ
        if self.show_settings {
            let input_devices = self.get_cached_input_devices().clone();
            let output_devices = self.get_cached_output_devices().clone();
            let applied = ui::show_settings_dialog(ctx, &mut self.show_settings, &self.settings, &mut self.show_hotkey_dialog, &input_devices, &output_devices);
            if applied { self.apply_settings(false); }
        }
        
        // ホットキーキャプチャダイアログ
        if self.show_hotkey_dialog {
            // ダイアログが開かれた時に現在の設定値をtemp_hotkeyに設定
            if self.temp_hotkey.is_empty() {
                if let Ok(settings) = self.settings.lock() {
                    self.temp_hotkey = settings.screenshot.hotkey.clone().unwrap_or_default();
                }
            }
            
            let hotkey_captured = ui::show_hotkey_capture_dialog(ctx, &mut self.show_hotkey_dialog, &mut self.temp_hotkey);
            
            // ホットキーがキャプチャされた場合、設定を更新
            if hotkey_captured && !self.temp_hotkey.is_empty() {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.screenshot.hotkey = Some(self.temp_hotkey.clone());
                    settings.save(); // 即座に保存
                }
                self.pending_hotkey = Some(self.temp_hotkey.clone());
            }
            
            // ダイアログが閉じられた時にtemp_hotkeyをクリア
            if !self.show_hotkey_dialog {
                self.temp_hotkey.clear();
            }
        }
        
        // コンテキストメニュー
        if self.show_context_menu { self.show_context_menu(ctx); }

        // フルスクリーン切替オーバーレイ (1秒表示)
        if let Some(t) = self.last_fullscreen_toggle {
            if t.elapsed().as_secs_f32() < 1.0 {
                egui::Area::new("fullscreen_overlay")
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(20.0, 20.0))
                    .show(ctx, |ui| {
                        egui::Frame::none().fill(egui::Color32::from_black_alpha(160)).rounding(5.0).show(ui, |ui| {
                            ui.label(if self.is_fullscreen { "フルスクリーン ON" } else { "フルスクリーン OFF" });
                        });
                    });
            }
        }

        // 新しくキャプチャされたホットキーを即座に登録
        if let Some(hk) = self.pending_hotkey.take() {
            println!("Registering new hotkey: {}", hk);
            if let Ok(mut ss) = self.screenshot_manager.lock() { 
                match ss.set_hotkey(&hk) {
                    Ok(()) => println!("Hotkey registered successfully: {}", hk),
                    Err(e) => println!("Failed to register hotkey {}: {}", hk, e)
                }
            } else {
                println!("Failed to lock screenshot_manager for hotkey registration");
            }
        }
        // テストサウンドリクエストを処理
        if crate::ui::should_play_test_sound() {
            if let Ok(settings) = self.settings.lock() {
                let volume = settings.screenshot.sound_volume;
                if let Ok(ss) = self.screenshot_manager.lock() { 
                    ss.play_screenshot_sound(volume); 
                }
            }
        }
    }
    
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 終了時に設定を保存
        if let Ok(settings) = self.settings.lock() {
            settings.save();
        }
    }
}

impl CaptureCardViewer {
    fn update_video_texture(&mut self, ctx: &egui::Context) {
        if let Ok(video) = self.video_capture.lock() {
            if let Some(frame) = video.get_latest_frame() {
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
        
        // 最新フレームの生データを抽出
        if let Ok(video) = self.video_capture.lock() {
            if let Some(frame) = video.get_latest_frame() {
                println!("take_screenshot: Got video frame {}x{}", frame.width, frame.height);
                
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
                    if let Some(img_buf) = image::RgbImage::from_raw(frame.width as u32, frame.height as u32, frame.data.clone()) {
                        match img_buf.save(&path) {
                            Ok(()) => {
                                println!("take_screenshot: Screenshot saved successfully to {:?}", path);
                                let volume = settings.screenshot.sound_volume;
                                if let Ok(ss) = self.screenshot_manager.lock() { 
                                    ss.play_screenshot_sound(volume); 
                                }
                            }
                            Err(e) => println!("take_screenshot: Failed to save image: {}", e)
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
                    self.calculate_aspect_ratio_size(image_size, available_size)
                } else {
                    available_size
                };
                
                let rect = egui::Rect::from_center_size(
                    ui.available_rect_before_wrap().center(),
                    display_size
                );
                
                let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                ui.painter().image(texture.id(), rect, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)), egui::Color32::WHITE);
                
                // ウィンドウドラッグを処理
                if response.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                
                // インタラクションを処理
                if response.double_clicked() {
                    self.toggle_fullscreen(ctx, true);
                }
                
                if response.secondary_clicked() {
                    self.show_context_menu = true;
                    self.context_menu_pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                }
                
                // 音量調整のためのスクロールを処理
                if response.hovered() {
                    ctx.input(|i| {
                        if i.raw_scroll_delta.y > 0.0 {
                            self.volume = (self.volume + 10.0).min(200.0);
                            // 設定に保存してリセットを防ぐ
                            if let Ok(mut settings) = self.settings.lock() {
                                settings.ui.volume = self.volume;
                                settings.save();
                            }
                        } else if i.raw_scroll_delta.y < 0.0 {
                            self.volume = (self.volume - 10.0).max(0.0);
                            // 設定に保存してリセットを防ぐ
                            if let Ok(mut settings) = self.settings.lock() {
                                settings.ui.volume = self.volume;
                                settings.save();
                            }
                        }
                    });
                }
            } else {
                let response = ui.allocate_response(available_size, egui::Sense::click_and_drag());
                ui.centered_and_justified(|ui| {
                    ui.label("映像信号がありません");
                });
                
                // 空エリアでのウィンドウドラッグを処理
                if response.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                
                // 空エリアでの右クリックを処理
                if response.secondary_clicked() {
                    self.show_context_menu = true;
                    self.context_menu_pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
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
                        self.calculate_aspect_ratio_size(image_size, available_size)
                    } else {
                        available_size
                    };
                    
                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size
                    );
                    
                    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                    ui.painter().image(texture.id(), rect, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)), egui::Color32::WHITE);
                    
                    // ウィンドウドラッグ機能（フルスクリーンでは無効だが一貫性のため実装）
                    if response.dragged() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    
                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() { self.toggle_fullscreen(ctx, false); }
                    
                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                    }
                    
                    // マウススクロールでの音量調整（ウィンドウ版と同じ機能）
                    if response.hovered() {
                        ctx.input(|i| {
                            if i.raw_scroll_delta.y > 0.0 {
                                self.volume = (self.volume + 10.0).min(200.0);
                                // 設定に保存してリセットを防ぐ
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                    settings.save();
                                }
                            } else if i.raw_scroll_delta.y < 0.0 {
                                self.volume = (self.volume - 10.0).max(0.0);
                                // 設定に保存してリセットを防ぐ
                                if let Ok(mut settings) = self.settings.lock() {
                                    settings.ui.volume = self.volume;
                                    settings.save();
                                }
                            }
                        });
                    }
                } else {
                    // 映像信号がない場合
                    let response = ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label("映像信号がありません");
                    });
                    
                    // ウィンドウドラッグ機能（フルスクリーンでは無効だが一貫性のため実装）
                    if response.dragged() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    
                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() { self.toggle_fullscreen(ctx, false); }
                    
                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        self.show_context_menu = true;
                        self.context_menu_pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
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
                    let volume_response = ui.add(egui::Slider::new(&mut self.volume, 0.0..=200.0).suffix("%"));
                    
                    // 音量が変更された場合、設定に反映し保存
                    if volume_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.volume = self.volume;
                            settings.save(); // 即座に保存
                        }
                    }

                    ui.separator();
                    let aspect_response = ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");
                    
                    // アスペクト比設定が変更された場合、設定に反映し保存
                    if aspect_response.changed() {
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.maintain_aspect_ratio = self.maintain_aspect_ratio;
                            settings.save(); // 即座に保存
                        }
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
                            }
                        ));
                        
                        // 設定に保存
                        if let Ok(mut settings) = self.settings.lock() {
                            settings.ui.always_on_top = self.always_on_top;
                            settings.save();
                        }
                    }

                    // フルスクリーン表示のチェックボックス
                    let fullscreen_response = ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");
                    
                    // フルスクリーン状態が変更された場合
                    if fullscreen_response.changed() {
                        self.toggle_fullscreen(ctx, self.is_fullscreen);
                    }

                    ui.separator();
                    if ui.button("リフレッシュ").clicked() {
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
                        if !r.contains(pos) { close_menu = true; }
                    } else {
                        close_menu = true;
                    }
                }
            }
            if i.key_pressed(egui::Key::Escape) { close_menu = true; }
        });

        if close_menu { self.show_context_menu = false; }
    }
    
    fn calculate_aspect_ratio_size(&self, image_size: egui::Vec2, available_size: egui::Vec2) -> egui::Vec2 {
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
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_icon(load_icon()),
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
    #[cfg(target_os = "windows")] {
        let candidate_paths = [
            "C:/Windows/Fonts/meiryo.ttc",
            "C:/Windows/Fonts/Meiryo.ttc",
            "C:/Windows/Fonts/meiryob.ttc",
        ];
        for p in candidate_paths.iter() {
            if let Ok(data) = std::fs::read(p) {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert("meiryo".to_string(), egui::FontData::from_owned(data));
                // 優先度のためにプロポーショナル・等幅フォントファミリーの先頭に挿入
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) { fam.insert(0, "meiryo".to_string()); }
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) { fam.insert(0, "meiryo".to_string()); }
                ctx.set_fonts(fonts);
                break;
            }
        }
    }
}

fn load_icon() -> egui::IconData {
    let icon_path = "icon.ico";
    if let Ok(icon_data) = std::fs::read(icon_path) {
        if let Ok(icon) = image::load_from_memory(&icon_data) {
            let icon_rgba = icon.to_rgba8();
            let (width, height) = icon.dimensions();
            return egui::IconData {
                rgba: icon_rgba.into_raw(),
                width,
                height,
            };
        }
    }
    
    // フォールバック: 単純な色付き四角形を作成
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
    fn apply_settings(&mut self, initial: bool) {
        if let Ok(settings) = self.settings.lock() {
            // Video - リトライ機能付き
            if let Ok(mut video) = self.video_capture.lock() {
                let need_video_restart =
                    settings.video.device_name != self.last_video_device ||
                    settings.video.resolution != self.last_video_res ||
                    settings.video.format != self.last_video_format ||
                    settings.video.fps != self.last_video_fps;
                    
                if settings.video.device_name.is_some() && (need_video_restart || initial) {
                    println!("Debug: Starting video device connection: {:?}", settings.video.device_name);
                    let mut video_success = false;
                    let max_retries = if initial { 3 } else { 1 };
                    
                    for attempt in 0..max_retries {
                        if attempt > 0 {
                            println!("Video device connection attempt {} of {}", attempt + 1, max_retries);
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
                let need_audio_restart =
                    settings.audio.input_device_name != self.last_audio_device ||
                    settings.audio.sample_rate != self.last_audio_rate ||
                    settings.audio.channels != self.last_audio_channels ||
                    initial; // 起動時は必ず接続試行
                    
                if need_audio_restart {
                    println!("Debug: Starting audio device connection");
                    println!("Debug: Input device: {:?}", settings.audio.input_device_name);
                    println!("Debug: Output device: {:?}", settings.audio.output_device_name);
                    
                    // まずは利用可能なデバイスをリスト
                    let input_devices = audio.list_input_devices();
                    let output_devices = audio.list_output_devices();
                    println!("Debug: Available input devices: {:?}", input_devices);
                    println!("Debug: Available output devices: {:?}", output_devices);
                    
                    let mut audio_success = false;
                    let max_retries = if initial { 5 } else { 2 }; // 起動時により多くリトライ
                    
                    for attempt in 0..max_retries {
                        if attempt > 0 {
                            println!("Audio device connection attempt {} of {}", attempt + 1, max_retries);
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }
                        
                        // 接続試行
                        match audio.start_passthrough_with_settings(
                            settings.audio.input_device_name.as_deref(), 
                            settings.audio.output_device_name.as_deref(), 
                            settings.audio.sample_rate, 
                            settings.audio.channels
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
                                    match audio.start_passthrough_with_settings(None, None, None, None) {
                                        Ok(_) => {
                                            println!("Debug: Audio connected with default devices");
                                            self.audio_last_error = None;
                                            audio_success = true;
                                            break;
                                        }
                                        Err(e2) => {
                                            println!("Default audio connection also failed: {}", e2);
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
                
                // 音量とパススルー設定を適用
                self.volume = settings.ui.volume;
                audio.set_volume(self.volume);
                audio.set_audio_passthrough_enabled(settings.audio.passthrough_enabled);
            }
            
            // UI設定
            self.maintain_aspect_ratio = settings.ui.maintain_aspect_ratio;
            self.always_on_top = settings.ui.always_on_top;
            
            // スクリーンショット設定
            if let Ok(mut ss) = self.screenshot_manager.lock() {
                if let Some(hk) = &settings.screenshot.hotkey { 
                    let _ = ss.set_hotkey(hk); 
                }
                if let Some(sf) = &settings.screenshot.sound_file { 
                    let _ = ss.set_sound_file(sf); 
                }
            }
        }
        
        if !initial { 
            self.last_settings_applied = Instant::now(); 
        }
    }

    fn update_cached_device_lists(&mut self) {
        // パフォーマンス影響を避けるため5秒ごとにのみデバイスリストを更新
        let should_update = self.last_device_list_update
            .map(|last| last.elapsed().as_secs() >= 5)
            .unwrap_or(true);
            
        if should_update {
            if let Ok(audio) = self.audio_capture.lock() {
                self.cached_input_devices = audio.list_input_devices();
                self.cached_output_devices = audio.list_output_devices();
                self.last_device_list_update = Some(Instant::now());
            }
        }
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