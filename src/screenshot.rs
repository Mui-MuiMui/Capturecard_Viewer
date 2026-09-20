use log::info;
use rodio::{Decoder, OutputStream, Sink};
use std::io::Cursor;
use std::path::{Path, PathBuf};

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

/// スクリーンショットの効果音を持つ。
///
/// **ホットキーの登録と押下の検出は持たない。** グローバルホットキーは
/// スクリーンショット以外のアクションにも割り当てられるため、`hotkey.rs` の
/// `HotkeyManager` が一手に扱う。
pub struct ScreenshotManager {
    sound_data: Option<Vec<u8>>,
}

impl ScreenshotManager {
    pub fn new() -> Self {
        Self { sound_data: None }
    }

    /// 効果音を捨て、以降スクリーンショットを無音にする。
    ///
    /// 設定画面で効果音を「クリア」したときに呼ぶ。`set_sound_file` は
    /// ファイルが見つからなければ埋め込みの既定音へ倒すため、「鳴らさない」は
    /// 設定を `None` にすることでしか表せない。その `None` をここで実行時へ
    /// 反映する。
    pub fn clear_sound(&mut self) {
        if self.sound_data.is_none() {
            return;
        }
        info!("効果音を破棄した。以降スクリーンショットは無音になる");
        self.sound_data = None;
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

impl Default for ScreenshotManager {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn clear_sound_discards_loaded_sound() {
        // 設定の効果音を「クリア」したセッションで鳴り続けていた不具合の再現。
        // 空パスは埋め込みの既定音へ倒れるので、実ファイルは要らない
        let mut manager = ScreenshotManager::new();
        manager
            .set_sound_file(Path::new(""))
            .expect("埋め込みの既定音は必ず読める");
        assert!(manager.sound_data.is_some());

        manager.clear_sound();

        assert!(manager.sound_data.is_none());
    }
}
