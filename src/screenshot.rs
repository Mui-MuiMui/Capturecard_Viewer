use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use rodio::{Decoder, OutputStream, Sink};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// 既定の効果音。実行ファイルに埋め込む。
// 既定値が "sound/SS.mp3" というカレントディレクトリ基準の相対パスだったため、
// ショートカット経由など作業ディレクトリが exe の場所と異なる起動では鳴らなかった。
const EMBEDDED_SOUND: &[u8] = include_bytes!("../sound/SS.mp3");

// 設定に保存された効果音のパスを、何から読むかへ解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSource {
    // 実行ファイルに埋め込んだ既定の効果音を使う
    Embedded,
    // 指定されたファイルを読む。相対パスは解決済み
    File(PathBuf),
}

// 設定の効果音パスを、実際に読むパスへ解決する。
//
// 相対パスをカレントディレクトリではなく exe の置き場所を基準に解決するのが要点。
// 既存ユーザーの設定ファイルには、かつての既定値 "sound/SS.mp3" が相対パスのまま
// 保存されているため、exe の隣から探せるようにする。
//
// 見つからない場合は無音ではなく埋め込みの既定音へ倒す。ログの出口が無い現状では、
// 無音にするとユーザーに原因を伝える手段が無く、故障と区別が付かないため。
// 効果音そのものを止めたい場合は、設定の sound_file を None にする（設定画面の
// 「クリア」）。その場合はこの関数が呼ばれない。
//
// exists を引数で受けるのはテストのため。実行時は |path| path.exists() を渡す。
pub fn resolve_sound_path(
    configured: &Path,
    exe_dir: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> SoundSource {
    // 空のパスを exe_dir.join() に通すと exe のディレクトリ自身になり、
    // ディレクトリを効果音として読もうとしてしまう
    if configured.as_os_str().is_empty() {
        return SoundSource::Embedded;
    }

    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        match exe_dir {
            Some(dir) => dir.join(configured),
            // exe の場所が分からない場合、基準にできるのはカレントディレクトリ
            // しか残らない。そこを見に行くと修正前の挙動に戻るため、
            // 相対パスの解決自体を諦めて埋め込み音へ倒す
            None => return SoundSource::Embedded,
        }
    };

    if exists(&candidate) {
        SoundSource::File(candidate)
    } else {
        SoundSource::Embedded
    }
}

// 実行ファイルが置かれているディレクトリ。取得できない場合は None。
//
// カレントディレクトリへフォールバックしない。そうすると、この修正で
// 取り除いたはずのカレントディレクトリ依存が黙って復活するため。
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

pub struct ScreenshotManager {
    hotkey_manager: Option<GlobalHotKeyManager>,
    registered_hotkey: Option<HotKey>,
    registered_hotkey_id: Option<u32>, // ホットキーIDを保存（u32型）
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
            self.hotkey_manager = Some(
                GlobalHotKeyManager::new()
                    .map_err(|e| format!("Failed to create hotkey manager: {}", e))?,
            );
        }

        // 古いホットキーが存在する場合は登録解除
        if let (Some(manager), Some(old_hotkey)) = (&self.hotkey_manager, &self.registered_hotkey) {
            println!(
                "Unregistering old hotkey: {:?} (ID: {})",
                old_hotkey,
                old_hotkey.id()
            );
            let _ = manager.unregister(*old_hotkey);
            self.registered_hotkey = None;
            self.registered_hotkey_id = None;
        }

        // 新しいホットキーを登録
        if let Some(manager) = &self.hotkey_manager {
            println!("Registering new hotkey: {:?} (ID: {})", hotkey, hotkey.id());

            // F11/F12キーの場合、特別な注意事項をログ出力
            if hotkey_str.to_lowercase() == "f11" || hotkey_str.to_lowercase() == "f12" {
                println!(
                    "Note: Registering {} as global hotkey. Make sure no other app is using it.",
                    hotkey_str
                );
            }

            let result = manager.register(hotkey).map_err(|e| {
                format!(
                    "ホットキー {} の登録に失敗しました: {}。他のキーを試してください。",
                    hotkey_str, e
                )
            });

            match result {
                Ok(()) => {
                    self.registered_hotkey = Some(hotkey);
                    self.registered_hotkey_id = Some(hotkey.id()); // ホットキーIDを保存
                    println!(
                        "Hotkey {} registered successfully with ID: {}",
                        hotkey_str,
                        hotkey.id()
                    );
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

    // 効果音を読み込む。
    //
    // 相対パスは exe の置き場所を基準に解決し、見つからなければ埋め込みの
    // 既定音を使う。そのため呼び出し後は必ず鳴らせる状態になっている。
    // Err を返すのは、解決したファイルが存在したのに読めなかった場合だけ。
    // このときも既定音を入れてあるので、鳴らないという結果にはならない。
    pub fn set_sound_file(&mut self, sound_path: &Path) -> Result<(), String> {
        match resolve_sound_path(sound_path, exe_dir().as_deref(), |path| path.exists()) {
            SoundSource::Embedded => {
                self.sound_data = Some(EMBEDDED_SOUND.to_vec());
                Ok(())
            }
            SoundSource::File(path) => match std::fs::read(&path) {
                Ok(data) => {
                    self.sound_data = Some(data);
                    Ok(())
                }
                Err(e) => {
                    self.sound_data = Some(EMBEDDED_SOUND.to_vec());
                    Err(format!(
                        "効果音ファイル {} を読み込めないため既定の効果音を使う: {}",
                        path.display(),
                        e
                    ))
                }
            },
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
                        println!(
                            "Screenshot blocked by debounce ({}ms < {}ms)",
                            elapsed, DEBOUNCE_MS
                        );
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
        let registered_id = self.registered_hotkey_id; // 登録されたホットキーIDをキャプチャ

        std::thread::spawn(move || {
            println!(
                "Screenshot hotkey listener started for ID: {:?}",
                registered_id
            );
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
                        println!(
                            "Received hotkey event: ID={}, State={:?} (looking for ID={})",
                            event.id(),
                            event.state(),
                            registered_id.unwrap_or(0)
                        );
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
                                println!(
                                    "✗ Hotkey ID does not match ({} != {}), ignoring event",
                                    event.id(),
                                    expected_id
                                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // exe が置かれている想定のディレクトリ。実在しなくてよい
    const EXE_DIR: &str = "C:/Program Files/capturecard_viewer";

    #[test]
    fn resolve_sound_path_absolute_existing_uses_that_file() {
        // 設定画面からユーザーがファイルを選んだ場合。rfd は絶対パスを返す
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("custom.mp3");
        assert!(configured.is_absolute(), "テストの前提: 絶対パスであること");

        let resolved = resolve_sound_path(&configured, Some(Path::new(EXE_DIR)), |_| true);

        assert_eq!(resolved, SoundSource::File(configured));
    }

    #[test]
    fn resolve_sound_path_absolute_missing_falls_back_to_embedded() {
        // ユーザーが選んだファイルを後から移動・削除した場合
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("deleted.mp3");

        let resolved = resolve_sound_path(&configured, Some(Path::new(EXE_DIR)), |_| false);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_existing_resolves_against_exe_dir() {
        // 既存ユーザーの設定に残っている "sound/SS.mp3" を、exe の隣から見つける。
        // カレントディレクトリ基準で解決していると exists が false になり、
        // SoundSource::Embedded へ落ちる
        let expected = PathBuf::from("C:/Program Files/capturecard_viewer/sound/SS.mp3");

        let resolved = resolve_sound_path(
            Path::new("sound/SS.mp3"),
            Some(Path::new(EXE_DIR)),
            |path| path == expected,
        );

        assert_eq!(resolved, SoundSource::File(expected));
    }

    #[test]
    fn resolve_sound_path_relative_missing_falls_back_to_embedded() {
        // exe の隣にも sound/ が無い配布形態。埋め込みの既定音で鳴らす
        let resolved =
            resolve_sound_path(Path::new("sound/SS.mp3"), Some(Path::new(EXE_DIR)), |_| {
                false
            });

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_empty_falls_back_to_embedded() {
        // 設定に空文字が入っていた場合。exe のディレクトリ自身を
        // 効果音ファイルとして読もうとしないこと
        let resolved = resolve_sound_path(Path::new(""), Some(Path::new(EXE_DIR)), |_| true);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_ignores_current_dir() {
        // 不具合そのものの再現。カレントディレクトリ配下にだけファイルがある
        // 状況を作り、そこを見に行かないことを確かめる
        let cwd_candidate = PathBuf::from("sound/SS.mp3");

        let resolved = resolve_sound_path(
            Path::new("sound/SS.mp3"),
            Some(Path::new(EXE_DIR)),
            |path| path == cwd_candidate,
        );

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_relative_without_exe_dir_falls_back_to_embedded() {
        // current_exe() が失敗して exe の場所が分からない場合。ここで
        // カレントディレクトリを基準にすると修正前の挙動に戻るため、
        // ファイルが存在していても埋め込み音へ倒す
        let resolved = resolve_sound_path(Path::new("sound/SS.mp3"), None, |_| true);

        assert_eq!(resolved, SoundSource::Embedded);
    }

    #[test]
    fn resolve_sound_path_absolute_without_exe_dir_uses_that_file() {
        // 絶対パスの指定は基準ディレクトリを必要としないため、
        // exe の場所が分からなくてもそのまま使える
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let configured = dir.path().join("custom.mp3");

        let resolved = resolve_sound_path(&configured, None, |_| true);

        assert_eq!(resolved, SoundSource::File(configured));
    }

    #[test]
    fn embedded_sound_starts_with_mpeg_frame_sync() {
        // 埋め込む mp3 が空や別形式に差し替わると、例外も出ないまま無音になる。
        // MPEG のフレーム同期（11 ビットすべて 1）で最低限の形式を確かめる
        assert!(EMBEDDED_SOUND.len() > 2, "埋め込んだ効果音が短すぎる");
        assert_eq!(EMBEDDED_SOUND[0], 0xFF);
        assert_eq!(EMBEDDED_SOUND[1] & 0xE0, 0xE0);
    }
}
