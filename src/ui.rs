use eframe::egui;
use std::sync::{Arc, Mutex};
use crate::settings::AppSettings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;

static TEST_SOUND_FLAG: AtomicBool = AtomicBool::new(false);

// デバイス能力のキャッシュ
static DEVICE_CAPABILITIES_CACHE: std::sync::OnceLock<Mutex<HashMap<String, Vec<(String, Vec<(u32, u32, u32)>)>>>> = std::sync::OnceLock::new();

// 一時保存用の設定
static TEMP_SETTINGS: std::sync::OnceLock<Mutex<Option<AppSettings>>> = std::sync::OnceLock::new();

pub fn should_play_test_sound() -> bool {
    TEST_SOUND_FLAG.swap(false, Ordering::SeqCst)
}


// 設定が適用された場合にtrueを返す（適用またはOKボタンが押された）
pub fn show_settings_dialog(
    ctx: &egui::Context, 
    show_settings: &mut bool, 
    settings: &Arc<Mutex<AppSettings>>, 
    show_hotkey_dialog: &mut bool,
    input_devices: &[String],
    output_devices: &[String]
) -> bool {
    use std::sync::OnceLock;
    static SELECTED_TAB: OnceLock<Mutex<i32>> = OnceLock::new();
    let selected_tab = SELECTED_TAB.get_or_init(|| Mutex::new(0));
    
    // 一時設定の初期化（設定画面を開いたとき）
    let temp_settings = TEMP_SETTINGS.get_or_init(|| Mutex::new(None));
    if let Ok(mut temp) = temp_settings.lock() {
        if temp.is_none() {
            if let Ok(current_settings) = settings.lock() {
                *temp = Some(current_settings.clone());
            }
        }
    }
    
    let close_settings = false;
    let mut apply_settings = false;
    let mut ok_pressed = false;
    let mut cancel_pressed = false;
    
    egui::Window::new("設定")
        .open(show_settings)
        .default_size([650.0, 500.0])
        .resizable(true)
        .show(ctx, |ui| {
            if let Ok(mut settings) = settings.lock() {
                // タブ選択
                ui.horizontal(|ui| {
                    if let Ok(mut tab) = selected_tab.lock() {
                        ui.selectable_value(&mut *tab, 0, "デバイス設定");
                        ui.selectable_value(&mut *tab, 1, "スクリーンショット設定");
                    }
                });
                
                ui.separator();
                
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Ok(tab) = selected_tab.lock() {
                        match *tab {
                            0 => show_device_settings_tab(ui, &mut settings, input_devices, output_devices),
                            1 => show_screenshot_settings_tab(ui, &mut settings, show_hotkey_dialog),
                            _ => {}
                        }
                    }
                });
                
                ui.separator();
                
                // OK、キャンセル、適用ボタン
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        ok_pressed = true;
                    }
                    
                    if ui.button("キャンセル").clicked() {
                        cancel_pressed = true;
                    }
                    
                    if ui.button("適用").clicked() {
                        apply_settings = true;
                    }
                });
            }
        });
    
    // ボタン処理
    if ok_pressed {
        // OKボタン: 設定をファイルに保存して閉じる
        if let Ok(current_settings) = settings.lock() {
            current_settings.save();
        }
        if let Ok(mut temp) = temp_settings.lock() {
            *temp = None; // 一時設定をクリア
        }
        *show_settings = false;
        return true; // デバイス再接続を要求
    }
    
    if cancel_pressed {
        // キャンセルボタン: 元の設定に戻して閉じる
        if let Ok(mut temp) = temp_settings.lock() {
            if let Some(original_settings) = temp.take() {
                if let Ok(mut current_settings) = settings.lock() {
                    *current_settings = original_settings;
                }
            }
        }
        *show_settings = false;
        return false; // デバイス再接続なし
    }
    
    if apply_settings {
        // 適用ボタン: メモリ上のみで適用（ファイル保存なし）
        // 画面は閉じない
        return true; // デバイス再接続を要求
    }
    
    if close_settings { *show_settings = false; }
    false
}

fn show_device_settings_tab(ui: &mut egui::Ui, settings: &mut AppSettings, input_devices: &[String], output_devices: &[String]) {
    ui.heading("デバイス設定");
    ui.add_space(10.0);
    
    // キャッシュの初期化
    let capabilities_cache = DEVICE_CAPABILITIES_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    
    // ビデオ設定
    ui.group(|ui| {
    ui.strong("ビデオ設定");
        ui.add_space(5.0);
        
        // ビデオデバイス選択
        let video_devices = crate::video::VideoCapture::list_devices();
        let selected_device = settings.video.device_name.clone().unwrap_or_default();
        
        let mut device_changed = false;
        egui::ComboBox::from_label("ビデオデバイス")
            .selected_text(if selected_device.is_empty() { "デバイスを選択..." } else { &selected_device })
            .show_ui(ui, |ui| {
                for (name, description) in &video_devices {
                    let display_text = if description.is_empty() { 
                        name.clone() 
                    } else { 
                        format!("{} ({})", name, description) 
                    };
                    if ui.selectable_label(settings.video.device_name.as_ref() == Some(name), display_text).clicked()
                        && settings.video.device_name.as_ref() != Some(name) {
                        settings.video.device_name = Some(name.clone());
                        device_changed = true;
                    }
                }
            });

        // デバイス変更時の処理
        if device_changed {
            // 新しく選択されたデバイス名を取得
            let new_device = settings.video.device_name.clone().unwrap_or_default();
            
            // デバイス能力を取得（キャッシュ確認）
            if let Ok(mut cache) = capabilities_cache.lock() {
                if !cache.contains_key(&new_device) && !new_device.is_empty() {
                    // キャッシュにない場合は取得
                    ui.spinner(); // 読み込み中表示
                    if let Ok(caps) = crate::video::VideoCapture::get_device_capabilities(Some(&new_device)) {
                        cache.insert(new_device.clone(), caps);
                    }
                }
            }
            
            // デフォルトのフォーマットを設定
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&new_device) {
                    // 最初のフォーマットを選択
                    if let Some((format, _)) = caps.first() {
                        settings.video.format = Some(format.clone());
                    }
                }
            }
        }
        
        // フォーマット選択（フォーマットが起点）
        let mut format_changed = false;
        ui.horizontal(|ui| {
            ui.label("フォーマット:");
            let current_format = settings.video.format.clone().unwrap_or_else(|| "YUY2".to_string());
            
            egui::ComboBox::from_id_source("format_combo")
                .selected_text(&current_format)
                .show_ui(ui, |ui| {
                    // キャッシュからフォーマット一覧を取得
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            for (format, _) in caps {
                                if ui.selectable_value(&mut settings.video.format, Some(format.clone()), format).clicked() {
                                    format_changed = true;
                                }
                            }
                        } else {
                            // キャッシュがない場合はデフォルト
                            ui.selectable_value(&mut settings.video.format, Some("YUY2".to_string()), "YUY2");
                            ui.selectable_value(&mut settings.video.format, Some("MJPEG".to_string()), "MJPEG");
                            ui.selectable_value(&mut settings.video.format, Some("RGB24".to_string()), "RGB24");
                        }
                    }
                });
        });
        
        // フォーマット変更時に解像度をリセット
        if format_changed {
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&selected_device) {
                    if let Some(current_format) = &settings.video.format {
                        // 現在のフォーマットに対応する最初の解像度を選択
                        for (format, resolutions) in caps {
                            if format == current_format {
                                if let Some((w, h, fps)) = resolutions.first() {
                                    settings.video.resolution = Some((*w, *h));
                                    settings.video.fps = Some(*fps);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        
        // 解像度選択（フォーマットに応じて動的に変更）
        let mut resolution_changed = false;
        ui.horizontal(|ui| {
            ui.label("解像度:");
            let current_resolution = settings.video.resolution.unwrap_or((1280, 720));
            
            egui::ComboBox::from_id_source("resolution_combo")
                .selected_text(format!("{}x{}", current_resolution.0, current_resolution.1))
                .show_ui(ui, |ui| {
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            if let Some(current_format) = &settings.video.format {
                                // 現在のフォーマットに対応する解像度一覧
                                let mut unique_resolutions = std::collections::HashSet::<(u32, u32)>::new();
                                for (format, resolutions) in caps {
                                    if format == current_format {
                                        for (w, h, _) in resolutions {
                                            unique_resolutions.insert((*w, *h));
                                        }
                                    }
                                }
                                
                                // ソートして表示
                                let mut sorted_resolutions: Vec<_> = unique_resolutions.into_iter().collect();
                                sorted_resolutions.sort_by(|a, b| {
                                    let size_a = a.0 * a.1;
                                    let size_b = b.0 * b.1;
                                    size_b.cmp(&size_a)
                                });
                                
                                for (w, h) in sorted_resolutions {
                                    if ui.selectable_value(&mut settings.video.resolution, Some((w, h)), format!("{}x{}", w, h)).clicked() {
                                        resolution_changed = true;
                                    }
                                }
                            }
                        } else {
                            // デフォルトの解像度
                            ui.selectable_value(&mut settings.video.resolution, Some((1920, 1080)), "1920x1080");
                            ui.selectable_value(&mut settings.video.resolution, Some((1280, 720)), "1280x720");
                            ui.selectable_value(&mut settings.video.resolution, Some((640, 480)), "640x480");
                        }
                    }
                });
        });
        
        // 解像度変更時にFPSをリセット
        if resolution_changed {
            if let Ok(cache) = capabilities_cache.lock() {
                if let Some(caps) = cache.get(&selected_device) {
                    if let Some(current_format) = &settings.video.format {
                        if let Some((w, h)) = settings.video.resolution {
                            // 現在のフォーマットと解像度に対応する最初のFPSを選択
                            for (format, resolutions) in caps {
                                if format == current_format {
                                    for (res_w, res_h, fps) in resolutions {
                                        if *res_w == w && *res_h == h {
                                            settings.video.fps = Some(*fps);
                                            break;
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // FPS選択（フォーマットと解像度に応じて動的に変更）
        ui.horizontal(|ui| {
            ui.label("フレームレート:");
            let current_fps = settings.video.fps.unwrap_or(30);
            
            egui::ComboBox::from_id_source("fps_combo")
                .selected_text(format!("{} fps", current_fps))
                .show_ui(ui, |ui| {
                    if let Ok(cache) = capabilities_cache.lock() {
                        if let Some(caps) = cache.get(&selected_device) {
                            if let Some(current_format) = &settings.video.format {
                                if let Some((w, h)) = settings.video.resolution {
                                    // 現在のフォーマットと解像度に対応するFPS一覧
                                    let mut available_fps = Vec::new();
                                    for (format, resolutions) in caps {
                                        if format == current_format {
                                            for (res_w, res_h, fps) in resolutions {
                                                if *res_w == w && *res_h == h {
                                                    available_fps.push(*fps);
                                                }
                                            }
                                        }
                                    }
                                    
                                    // 重複を削除してソート
                                    available_fps.sort();
                                    available_fps.dedup();
                                    available_fps.reverse(); // 大きい順
                                    
                                    for fps in available_fps {
                                        ui.selectable_value(&mut settings.video.fps, Some(fps), format!("{} fps", fps));
                                    }
                                }
                            }
                        } else {
                            // デフォルトのFPS
                            ui.selectable_value(&mut settings.video.fps, Some(30), "30 fps");
                            ui.selectable_value(&mut settings.video.fps, Some(60), "60 fps");
                        }
                    }
                });
        });
    });
    
    ui.add_space(15.0);
    
    // オーディオ設定
    ui.group(|ui| {
    ui.strong("オーディオ設定");
        ui.add_space(5.0);
        
        // オーディオ入力デバイス選択 - キャッシュリストを使用
        let current_input_device = settings.audio.input_device_name.clone().unwrap_or_default();
        
        egui::ComboBox::from_label("オーディオ入力デバイス")
            .selected_text(if current_input_device.is_empty() { "デバイスを選択..." } else { &current_input_device })
            .show_ui(ui, |ui| {
                for device_name in input_devices {
                    ui.selectable_value(&mut settings.audio.input_device_name, Some(device_name.clone()), device_name);
                }
            });
        
        // オーディオ出力デバイス選択 - キャッシュリストを使用
        let current_output_device = settings.audio.output_device_name.clone().unwrap_or_default();
        
        egui::ComboBox::from_label("オーディオ出力デバイス")
            .selected_text(if current_output_device.is_empty() { "デフォルト" } else { &current_output_device })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.audio.output_device_name, None, "デフォルト");
                for device_name in output_devices {
                    ui.selectable_value(&mut settings.audio.output_device_name, Some(device_name.clone()), device_name);
                }
            });
        
        // サンプルレート
        ui.horizontal(|ui| {
            ui.label("サンプリングレート:");
            let sample_rates = vec![8000, 16000, 22050, 32000, 44100, 48000, 96000];
            let current_rate = settings.audio.sample_rate.unwrap_or(44100);
            egui::ComboBox::from_id_source("sample_rate_combo")
                .selected_text(format!("{} Hz", current_rate))
                .show_ui(ui, |ui| {
                    for rate in sample_rates {
                        ui.selectable_value(&mut settings.audio.sample_rate, Some(rate), format!("{} Hz", rate));
                    }
                });
        });
        
        // チャンネル数
        ui.horizontal(|ui| {
            ui.label("チャンネル数:");
            let current_channels = settings.audio.channels.unwrap_or(2);
            egui::ComboBox::from_id_source("channels_combo")
                .selected_text(format!("{}", current_channels))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.audio.channels, Some(1), "1 (Mono)");
                    ui.selectable_value(&mut settings.audio.channels, Some(2), "2 (Stereo)");
                });
        });
        
        ui.add_space(10.0);
        
        // オーディオパススルー制御
        ui.horizontal(|ui| {
            ui.label("音声パススルー:");
            if ui.checkbox(&mut settings.audio.passthrough_enabled, "有効").changed() {
                println!("Audio passthrough changed to: {}", settings.audio.passthrough_enabled);
            }
        });
        
        if !settings.audio.passthrough_enabled {
            ui.colored_label(egui::Color32::YELLOW, "⚠ 音声パススルーが無効です（ノイズ軽減のため）");
        }
    });
    
    ui.add_space(15.0);
    
    // UI設定
    ui.group(|ui| {
    ui.strong("ユーザーインターフェース");
        ui.add_space(5.0);
        
    ui.checkbox(&mut settings.ui.maintain_aspect_ratio, "アスペクト比を維持");
        
        ui.horizontal(|ui| {
            ui.label("初期音量:");
            ui.add(egui::Slider::new(&mut settings.ui.volume, 0.0..=200.0).suffix("%"));
        });
    });
}

fn show_screenshot_settings_tab(ui: &mut egui::Ui, settings: &mut AppSettings, show_hotkey_dialog: &mut bool) {
    ui.heading("スクリーンショット設定");
    ui.add_space(10.0);
    
    // 保存フォルダー
    ui.group(|ui| {
    ui.strong("保存場所");
        ui.add_space(5.0);
        
        ui.horizontal(|ui| {
            ui.label("保存フォルダ:");
            let mut folder_str = settings.screenshot.save_folder.to_string_lossy().to_string();
            ui.text_edit_singleline(&mut folder_str);
            settings.screenshot.save_folder = std::path::PathBuf::from(folder_str);
            
            if ui.button("参照...").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    settings.screenshot.save_folder = folder;
                }
            }
        });
    });
    
    ui.add_space(15.0);
    
    // サウンド設定
    ui.group(|ui| {
    ui.strong("効果音");
        ui.add_space(5.0);
        
        ui.horizontal(|ui| {
            ui.label("サウンドファイル:");
            let sound_file_str = settings.screenshot.sound_file
                .as_ref()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                .unwrap_or_else(|| "未選択".to_string());
            ui.label(&sound_file_str);
            
            if ui.button("ファイル選択...").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("音声ファイル", &["mp3", "wav", "ogg"])
                    .pick_file() 
                {
                    settings.screenshot.sound_file = Some(file);
                }
            }
        });
        
        if settings.screenshot.sound_file.is_some() {
            ui.horizontal(|ui| {
                ui.label("音量:");
                ui.add(egui::Slider::new(&mut settings.screenshot.sound_volume, 0.0..=200.0).suffix("%"));
            });
            
            ui.horizontal(|ui| {
                if ui.button("テスト再生").clicked() { TEST_SOUND_FLAG.store(true, Ordering::SeqCst); }
                if ui.button("クリア").clicked() {
                    settings.screenshot.sound_file = None;
                }
            });
        }
    });
    
    ui.add_space(15.0);
    
    // ホットキー設定
    ui.group(|ui| {
    ui.strong("ホットキー設定");
        ui.add_space(5.0);
        
        ui.horizontal(|ui| {
            ui.label("スクリーンショットホットキー:");
            let hotkey_str = settings.screenshot.hotkey
                .clone()
                .unwrap_or_else(|| "未設定".to_string());
            
            ui.label(&hotkey_str);
            
            if ui.button("ホットキー設定...").clicked() {
                *show_hotkey_dialog = true;
            }
        });
        
        if settings.screenshot.hotkey.is_some() {
            ui.horizontal(|ui| {
                if ui.button("ホットキー解除").clicked() {
                    settings.screenshot.hotkey = None;
                }
            });
        }
        
        ui.add_space(5.0);
    ui.small("『ホットキー設定...』を押して希望のキーコンビネーションを入力してください。");
    });
}

#[allow(static_mut_refs)]
pub fn show_hotkey_capture_dialog(ctx: &egui::Context, show_dialog: &mut bool, captured_hotkey: &mut String) -> bool {
    static mut CAPTURING: bool = false;
    static mut TEMP_HOTKEY: String = String::new();
    
    let mut close_dialog = false;
    
    egui::Window::new("ホットキー設定")
        .open(show_dialog)
        .fixed_size([350.0, 200.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("ホットキー設定");
                ui.add_space(10.0);
                
                if unsafe { !CAPTURING } {
                    ui.label("『キャプチャ開始』を押してスクリーンショット用のキーを入力してください");
                    
                    ui.add_space(10.0);
                    
                    ui.horizontal(|ui| {
                        ui.label("現在のホットキー:");
                        let hotkey_text = if captured_hotkey.is_empty() { "未設定" } else { captured_hotkey.as_str() };
                        ui.monospace(hotkey_text);
                    });
                    
                    ui.add_space(15.0);
                    
                    if ui.button("キャプチャ開始").clicked() {
                        unsafe { 
                            CAPTURING = true;
                            TEMP_HOTKEY.clear();
                        }
                    }
                } else {
                    ui.colored_label(egui::Color32::YELLOW, "キー入力待機中...");
                    ui.label("任意のキーコンビネーションを押してください");
                    
                    // キーボード入力をキャプチャ
                    ctx.input(|i| {
                        let mut keys = Vec::new();
                        
                        if i.modifiers.ctrl { keys.push("Ctrl"); }
                        if i.modifiers.shift { keys.push("Shift"); }
                        if i.modifiers.alt { keys.push("Alt"); }
                        
                        // 押されたキーをチェック
                        for key in &i.keys_down {
                            match key {
                                egui::Key::A => keys.push("A"),
                                egui::Key::B => keys.push("B"),
                                egui::Key::C => keys.push("C"),
                                egui::Key::D => keys.push("D"),
                                egui::Key::E => keys.push("E"),
                                egui::Key::F => keys.push("F"),
                                egui::Key::G => keys.push("G"),
                                egui::Key::H => keys.push("H"),
                                egui::Key::I => keys.push("I"),
                                egui::Key::J => keys.push("J"),
                                egui::Key::K => keys.push("K"),
                                egui::Key::L => keys.push("L"),
                                egui::Key::M => keys.push("M"),
                                egui::Key::N => keys.push("N"),
                                egui::Key::O => keys.push("O"),
                                egui::Key::P => keys.push("P"),
                                egui::Key::Q => keys.push("Q"),
                                egui::Key::R => keys.push("R"),
                                egui::Key::S => keys.push("S"),
                                egui::Key::T => keys.push("T"),
                                egui::Key::U => keys.push("U"),
                                egui::Key::V => keys.push("V"),
                                egui::Key::W => keys.push("W"),
                                egui::Key::X => keys.push("X"),
                                egui::Key::Y => keys.push("Y"),
                                egui::Key::Z => keys.push("Z"),
                                egui::Key::F1 => keys.push("F1"),
                                egui::Key::F2 => keys.push("F2"),
                                egui::Key::F3 => keys.push("F3"),
                                egui::Key::F4 => keys.push("F4"),
                                egui::Key::F5 => keys.push("F5"),
                                egui::Key::F6 => keys.push("F6"),
                                egui::Key::F7 => keys.push("F7"),
                                egui::Key::F8 => keys.push("F8"),
                                egui::Key::F9 => keys.push("F9"),
                                egui::Key::F10 => keys.push("F10"),
                                egui::Key::F11 => keys.push("F11"),
                                egui::Key::F12 => keys.push("F12"),
                                egui::Key::Space => keys.push("Space"),
                                egui::Key::Enter => keys.push("Enter"),
                                _ => {}
                            }
                        }
                        
                        if !keys.is_empty() && keys.len() > (if i.modifiers.any() { 1 } else { 0 }) {
                            unsafe {
                                TEMP_HOTKEY = keys.join("+");
                                CAPTURING = false;
                            }
                        }
                    });
                    
                    unsafe {
                        if !TEMP_HOTKEY.is_empty() {
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.label("取得:");
                                ui.monospace(&TEMP_HOTKEY);
                            });
                        }
                    }
                    
                    ui.add_space(10.0);
                    
                    if ui.button("停止").clicked() {
                        unsafe { CAPTURING = false; }
                    }
                }
                
                ui.add_space(20.0);
                
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        unsafe {
                            if !TEMP_HOTKEY.is_empty() {
                                *captured_hotkey = TEMP_HOTKEY.clone();
                                TEMP_HOTKEY.clear();
                            }
                            CAPTURING = false;
                        }
                        close_dialog = true;
                    }
                    
                    if ui.button("キャンセル").clicked() {
                        unsafe {
                            CAPTURING = false;
                            TEMP_HOTKEY.clear();
                        }
                        close_dialog = true;
                    }
                    
                    if ui.button("クリア").clicked() {
                        captured_hotkey.clear();
                        unsafe {
                            CAPTURING = false;
                            TEMP_HOTKEY.clear();
                        }
                        close_dialog = true;
                    }
                });
            });
        });
    
    let hotkey_captured = !captured_hotkey.is_empty() && close_dialog;
    
    if close_dialog {
        *show_dialog = false;
    }
    
    hotkey_captured
}