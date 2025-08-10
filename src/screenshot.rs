use global_hotkey::{GlobalHotKeyManager, GlobalHotKeyEvent, HotKeyState, hotkey::{HotKey, Modifiers, Code}};
use std::sync::{Arc, Mutex};
use std::path::Path;
use rodio::{Decoder, OutputStream, Sink};
use std::io::Cursor;

pub struct ScreenshotManager {
    hotkey_manager: Option<GlobalHotKeyManager>,
    registered_hotkey: Option<HotKey>,
    registered_hotkey_id: Option<u32>,  // ホットキーIDを保存（u32型）
    is_hotkey_pressed: Arc<Mutex<bool>>,
    sound_data: Option<Vec<u8>>,
    last_trigger_time: Arc<Mutex<std::time::Instant>>,
    // メモリリーク修正: スレッド管理用の終了フラグ
    listener_shutdown: Arc<Mutex<bool>>,
}

impl ScreenshotManager {
    pub fn new() -> Self {
        Self {
            hotkey_manager: None,
            registered_hotkey: None,
            registered_hotkey_id: None,
            is_hotkey_pressed: Arc::new(Mutex::new(false)),
            sound_data: None,
            last_trigger_time: Arc::new(Mutex::new(std::time::Instant::now())),
            listener_shutdown: Arc::new(Mutex::new(false)),
        }
    }
    
    pub fn set_hotkey(&mut self, hotkey_str: &str) -> Result<(), String> {
        println!("Setting hotkey: {}", hotkey_str);
        
        // "F12", "Ctrl+S" などのホットキー文字列をパース
        let hotkey = self.parse_hotkey(hotkey_str)?;
        println!("Parsed hotkey: {:?}", hotkey);
        
        // ホットキーマネージャーが存在しない場合は作成
        if self.hotkey_manager.is_none() {
            println!("Creating new hotkey manager");
            self.hotkey_manager = Some(GlobalHotKeyManager::new()
                .map_err(|e| format!("Failed to create hotkey manager: {}", e))?);
        }
        
        // 古いホットキーが存在する場合は登録解除
        if let (Some(manager), Some(old_hotkey)) = (&self.hotkey_manager, &self.registered_hotkey) {
            println!("Unregistering old hotkey: {:?} (ID: {})", old_hotkey, old_hotkey.id());
            let _ = manager.unregister(*old_hotkey);
            self.registered_hotkey = None;
            self.registered_hotkey_id = None;
        }
        
        // 新しいホットキーを登録
        if let Some(manager) = &self.hotkey_manager {
            println!("Registering new hotkey: {:?} (ID: {})", hotkey, hotkey.id());
            
            // F11/F12キーの場合、特別な注意事項をログ出力
            if hotkey_str.to_lowercase() == "f11" || hotkey_str.to_lowercase() == "f12" {
                println!("Note: Registering {} as global hotkey. Make sure no other app is using it.", hotkey_str);
            }
            
            let result = manager.register(hotkey)
                .map_err(|e| format!("ホットキー {} の登録に失敗しました: {}。他のキーを試してください。", hotkey_str, e));
            
            match result {
                Ok(()) => {
                    self.registered_hotkey = Some(hotkey);
                    self.registered_hotkey_id = Some(hotkey.id());  // ホットキーIDを保存
                    println!("Hotkey {} registered successfully with ID: {}", hotkey_str, hotkey.id());
                }
                Err(e) => {
                    println!("Hotkey registration error: {}", e);
                    return Err(e);
                }
            }
        }
        
        // ホットキーイベントのリスニングを開始
        self.start_hotkey_listener();
        
        Ok(())
    }
    
    pub fn set_sound_file(&mut self, sound_path: &Path) -> Result<(), String> {
        match std::fs::read(sound_path) {
            Ok(data) => {
                self.sound_data = Some(data);
                Ok(())
            }
            Err(e) => Err(format!("Failed to load sound file: {}", e))
        }
    }
    
    pub fn is_hotkey_pressed(&self) -> bool {
        const DEBOUNCE_MS: u64 = 200; // デバウンス時間を200msに短縮
        
        if let Ok(mut pressed) = self.is_hotkey_pressed.lock() {
            if *pressed {
                println!("Screenshot hotkey detected!"); // デバッグログ追加
                
                // 最後のトリガー時刻をチェック
                if let Ok(mut last_time) = self.last_trigger_time.lock() {
                    let now = std::time::Instant::now();
                    let elapsed = now.duration_since(*last_time).as_millis();
                    
                    println!("Time since last trigger: {}ms", elapsed); // デバッグログ
                    
                    if elapsed > DEBOUNCE_MS as u128 {
                        *pressed = false; // フラグをリセット
                        *last_time = now; // 最後のトリガー時刻を更新
                        println!("Screenshot triggered!"); // デバッグログ
                        return true;
                    } else {
                        *pressed = false; // フラグをリセット（ただし false を返す）
                        println!("Screenshot blocked by debounce ({}ms < {}ms)", elapsed, DEBOUNCE_MS);
                        return false;
                    }
                } else {
                    println!("Failed to lock last_trigger_time");
                }
            }
        } else {
            println!("Failed to lock is_hotkey_pressed");
        }
        false
    }
    
    // 後方互換性のために保持される非推奨プレースホルダー（何もしない）

    
    fn parse_hotkey(&self, hotkey_str: &str) -> Result<HotKey, String> {
        let parts: Vec<&str> = hotkey_str.split('+').collect();
        let mut modifiers = Modifiers::empty();
        let mut key_code = None;
        
        for part in parts {
            let part = part.trim().to_lowercase();
            match part.as_str() {
                "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
                "alt" => modifiers |= Modifiers::ALT,
                "shift" => modifiers |= Modifiers::SHIFT,
                "win" | "windows" | "super" => modifiers |= Modifiers::SUPER,
                key => {
                    key_code = Some(self.parse_key_code(key)?);
                }
            }
        }
        
        let code = key_code.ok_or_else(|| "No key code specified".to_string())?;
        Ok(HotKey::new(Some(modifiers), code))
    }
    
    fn parse_key_code(&self, key: &str) -> Result<Code, String> {
        println!("Parsing key code: '{}'", key);
        let result = match key {
            "f1" => Ok(Code::F1),
            "f2" => Ok(Code::F2),
            "f3" => Ok(Code::F3),
            "f4" => Ok(Code::F4),
            "f5" => Ok(Code::F5),
            "f6" => Ok(Code::F6),
            "f7" => Ok(Code::F7),
            "f8" => Ok(Code::F8),
            "f9" => Ok(Code::F9),
            "f10" => Ok(Code::F10),
            "f11" => Ok(Code::F11),
            "f12" => Ok(Code::F12),
            "a" => Ok(Code::KeyA),
            "b" => Ok(Code::KeyB),
            "c" => Ok(Code::KeyC),
            "d" => Ok(Code::KeyD),
            "e" => Ok(Code::KeyE),
            "f" => Ok(Code::KeyF),
            "g" => Ok(Code::KeyG),
            "h" => Ok(Code::KeyH),
            "i" => Ok(Code::KeyI),
            "j" => Ok(Code::KeyJ),
            "k" => Ok(Code::KeyK),
            "l" => Ok(Code::KeyL),
            "m" => Ok(Code::KeyM),
            "n" => Ok(Code::KeyN),
            "o" => Ok(Code::KeyO),
            "p" => Ok(Code::KeyP),
            "q" => Ok(Code::KeyQ),
            "r" => Ok(Code::KeyR),
            "s" => Ok(Code::KeyS),
            "t" => Ok(Code::KeyT),
            "u" => Ok(Code::KeyU),
            "v" => Ok(Code::KeyV),
            "w" => Ok(Code::KeyW),
            "x" => Ok(Code::KeyX),
            "y" => Ok(Code::KeyY),
            "z" => Ok(Code::KeyZ),
            "space" => Ok(Code::Space),
            "enter" => Ok(Code::Enter),
            "escape" => Ok(Code::Escape),
            _ => Err(format!("Unknown key: {}", key)),
        };
        println!("Key code parsing result for '{}': {:?}", key, result);
        result
    }
    
    fn start_hotkey_listener(&mut self) {
        // 既存のリスナーを停止
        if let Ok(mut shutdown) = self.listener_shutdown.lock() {
            *shutdown = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10)); // 既存スレッドの終了を待機
        
        // 新しいリスナー用の終了フラグをリセット
        self.listener_shutdown = Arc::new(Mutex::new(false));
        
        let pressed_flag = self.is_hotkey_pressed.clone();
        let shutdown_flag = self.listener_shutdown.clone();
        let registered_id = self.registered_hotkey_id;  // 登録されたホットキーIDをキャプチャ
        
        std::thread::spawn(move || {
            println!("Screenshot hotkey listener started for ID: {:?}", registered_id);
            let global_hotkey_channel = GlobalHotKeyEvent::receiver();
            loop {
                // 終了フラグをチェック
                if let Ok(should_shutdown) = shutdown_flag.lock() {
                    if *should_shutdown {
                        println!("Screenshot hotkey listener shutting down");
                        break;
                    }
                }
                
                match global_hotkey_channel.try_recv() {
                    Ok(event) => {
                        println!("Received hotkey event: ID={}, State={:?} (looking for ID={})", 
                                event.id(), event.state(), registered_id.unwrap_or(0));
                        // イベントが登録されたホットキーと一致するかチェック
                        if let Some(expected_id) = registered_id {
                            if event.id() == expected_id {
                                println!("✓ Hotkey ID matches! State: {:?}", event.state());
                                // Pressedイベントのみに反応（Releasedは無視）
                                if event.state() == HotKeyState::Pressed {
                                    if let Ok(mut pressed) = pressed_flag.lock() {
                                        *pressed = true;
                                        println!("✓ Screenshot hotkey flag set to true");
                                    } else {
                                        println!("✗ Failed to set hotkey flag - mutex lock failed");
                                    }
                                } else {
                                    println!("- Ignoring Released event");
                                }
                            } else {
                                println!("✗ Hotkey ID does not match ({} != {}), ignoring event", 
                                        event.id(), expected_id);
                            }
                        } else {
                            println!("✗ No registered hotkey ID, ignoring event");
                        }
                    }
                    Err(_) => {
                        // イベントが受信されないため、リスニングを継続
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10)); // CPU使用量を抑制
            }
        });
    }
    
    pub fn play_screenshot_sound(&self, volume: f32) {
        if let Some(sound_data) = &self.sound_data {
            let sound_data = sound_data.clone();
            let volume = (volume / 100.0).clamp(0.0, 2.0); // パーセンテージを0.0-2.0範囲に変換
            std::thread::spawn(move || {
                if let Ok((_stream, stream_handle)) = OutputStream::try_default() {
                    if let Ok(sink) = Sink::try_new(&stream_handle) {
                        sink.set_volume(volume);
                        let cursor = Cursor::new(sound_data);
                        if let Ok(decoder) = Decoder::new(cursor) {
                            sink.append(decoder);
                            sink.sleep_until_end();
                        }
                    }
                }
            });
        }
    }
}

impl Drop for ScreenshotManager {
    fn drop(&mut self) {
        // スレッドを適切に終了
        if let Ok(mut shutdown) = self.listener_shutdown.lock() {
            *shutdown = true;
        }
        
        // ホットキーの登録解除
        if let (Some(manager), Some(hotkey)) = (&self.hotkey_manager, &self.registered_hotkey) {
            let _ = manager.unregister(*hotkey);
        }
        
        // 終了確認のため少し待機
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}