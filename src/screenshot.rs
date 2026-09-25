use crate::video::VideoFrame;
use log::info;
use rodio::{Decoder, OutputStream, Sink};
use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// スクリーンショットまわりの処理が失敗した理由。
///
/// クリップボードへのコピーと効果音の読み込みを 1 つの enum にまとめてある。
/// どちらも `ErrorSource::Screenshot` として同じ経路で表示され、呼び出し側は
/// 出力先の種類で処理を分けないため（`app::screenshot` の
/// `summarize_screenshot_delivery`）。
///
/// **表示用の日本語はこの型の `Display` が持つ。** 定型文
/// （`status::ErrorSource::headline`）との連結だけが `status.rs` の仕事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenshotError {
    /// 幅か高さが 0 のフレームを渡された
    EmptyFrame { width: usize, height: usize },
    /// 幅 × 高さ × 3 が `usize` に収まらない
    FrameTooLarge { width: usize, height: usize },
    /// 幅と高さから決まる長さに対して画素が足りない
    FrameTooShort {
        width: usize,
        height: usize,
        len: usize,
    },
    /// クリップボードを開けない（他のアプリが掴んでいる場合など）
    ClipboardOpenFailed(String),
    /// クリップボードを開けたが画像を書き込めない
    ClipboardWriteFailed(String),
    /// 効果音ファイルが見つかったのに読めない。既定の効果音へ倒してある
    SoundFileUnreadable { path: PathBuf, source: String },
    /// 効果音ファイルは読めたが音声としてデコードできない（拡張子だけ mp3 など）。
    /// 既定音へは倒さず、撮影時は無音になる（`docs/design/assets.md`）
    SoundFileUndecodable { path: PathBuf, source: String },
}

impl fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScreenshotError::EmptyFrame { width, height } => write!(
                f,
                "大きさのない映像フレームはクリップボードへコピーできない: {width}x{height}"
            ),
            ScreenshotError::FrameTooLarge { width, height } => {
                write!(f, "画像として扱えない大きさのフレーム: {width}x{height}")
            }
            ScreenshotError::FrameTooShort { width, height, len } => write!(
                f,
                "映像フレームの画素が足りない: {width}x{height} に対して {len} バイト"
            ),
            ScreenshotError::ClipboardOpenFailed(source) => {
                write!(f, "クリップボードを開けない: {source}")
            }
            ScreenshotError::ClipboardWriteFailed(source) => {
                write!(f, "クリップボードへ画像を書き込めない: {source}")
            }
            ScreenshotError::SoundFileUnreadable { path, source } => write!(
                f,
                "効果音ファイル {} を読み込めないため既定の効果音を使う: {source}",
                path.display()
            ),
            ScreenshotError::SoundFileUndecodable { path, source } => write!(
                f,
                "効果音ファイル {} を音声として読めないため、撮影時は効果音が鳴らない: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ScreenshotError {}

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
// 「効果音を鳴らさない」）。その場合はこの関数が呼ばれない。
// 既定値 settings::DEFAULT_SOUND_FILE はファイルとして配布していないので、
// ここで埋め込みの既定音へ倒れることが「既定（内蔵）」の効果音になる。
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

/// 映像フレームをクリップボードへ画像としてコピーする。
///
/// アプリの状態にも共有ロックにも触れないので、そのまま別スレッドで実行できる。
/// `app/screenshot.rs` の `save_frame` と対になる、撮影スレッドから呼ぶ関数。
///
/// **UI スレッドから呼ばない。** Windows のクリップボードは一度に 1 つの
/// プロセスしか開けず、他のアプリが掴んでいる間は待たされる（arboard は
/// 5ms 間隔で 5 回まで再試行する）。
///
/// **コピーしたスレッドが終わっても内容は残る。** Windows の
/// `SetClipboardData` は渡したメモリの所有権をシステムへ移すため、
/// `arboard::Clipboard` を落としてもクリップボードの中身は失われない
/// （遅延レンダリングを使っていないので、貼り付けのたびに元のスレッドへ
/// 問い合わせに行くこともない）。撮影ごとに spawn する短命のスレッドから
/// 呼んでよいのはこのため。
///
/// 画は圧縮せずそのまま渡す。保存形式と JPEG 品質はファイルへ出すときだけの
/// 設定で、クリップボードには効かない。
pub fn copy_frame_to_clipboard(frame: &VideoFrame) -> Result<(), ScreenshotError> {
    let bytes = rgb_to_rgba(&frame.data, frame.width, frame.height)?;

    // Clipboard はスレッドごとに作る。Windows では OpenClipboard が呼んだ
    // スレッドに紐づくため、他スレッドで作ったものを持ち回せない
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| ScreenshotError::ClipboardOpenFailed(e.to_string()))?;

    clipboard
        .set_image(arboard::ImageData {
            width: frame.width,
            height: frame.height,
            bytes: Cow::Owned(bytes),
        })
        .map_err(|e| ScreenshotError::ClipboardWriteFailed(e.to_string()))
}

/// RGB の画素列を、`arboard` が要求する RGBA へ広げる。
///
/// 不透明として扱うのでアルファは常に 255。キャプチャーした映像に透過は無く、
/// 0 を入れると貼り付け先によっては全面が透明になる。
fn rgb_to_rgba(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ScreenshotError> {
    if width == 0 || height == 0 {
        return Err(ScreenshotError::EmptyFrame { width, height });
    }

    // 1080p でも 1920*1080*3 で usize には十分収まるが、壊れた値が来たときに
    // 掛け算が一周して短い長さを要求してしまうのを避ける
    let needed = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(ScreenshotError::FrameTooLarge { width, height })?;

    if rgb.len() < needed {
        return Err(ScreenshotError::FrameTooShort {
            width,
            height,
            len: rgb.len(),
        });
    }

    let mut rgba = Vec::with_capacity(needed / 3 * 4);
    // needed は 3 の倍数なので端数は出ない
    let (pixels, _) = rgb[..needed].as_chunks::<3>();
    for pixel in pixels {
        rgba.extend_from_slice(pixel);
        rgba.push(u8::MAX);
    }
    Ok(rgba)
}

/// スクリーンショットの効果音を持つ。
///
/// **ホットキーの登録と押下の検出は持たない。** グローバルホットキーは
/// スクリーンショット以外のアクションにも割り当てられるため、`hotkey.rs` の
/// `HotkeyManager` が一手に扱う。
///
/// **ファイルの読み込みはしない。** 読み込みは `app::screenshot_sound` が
/// 別スレッドで行い、ここには番号の払い出し（`begin_load`）と結果の反映
/// （`finish_load`）だけを置く。大きなファイルや遅いドライブで UI スレッドが
/// 止まらないようにするため（Issue #214）。
pub struct ScreenshotManager {
    sound_data: Option<Vec<u8>>,
    // 適用の読み込み要求。テスト再生の要求とは別に数える
    loads: SoundLoadRequests,
    // 設定画面の「テスト再生」の要求。適用済みの音には触れない
    test_plays: SoundLoadRequests,
}

impl ScreenshotManager {
    pub fn new() -> Self {
        Self {
            sound_data: None,
            loads: SoundLoadRequests::default(),
            test_plays: SoundLoadRequests::default(),
        }
    }

    /// 効果音を捨て、以降スクリーンショットを無音にする。
    ///
    /// 設定画面で「効果音を鳴らさない」を選んだときに呼ぶ。読み込み
    /// （`load_sound_data`）はファイルが見つからなければ埋め込みの既定音へ
    /// 倒すため、「鳴らさない」は設定を `None` にすることでしか表せない。
    /// その `None` をここで実行時へ反映する。
    ///
    /// **読み込み中の要求も取り消す。** 取り消さないと、ファイルを選んだ直後に
    /// 「鳴らさない」へ切り替えたとき、後から届いた読み込みで音が戻る。
    pub fn clear_sound(&mut self) {
        self.loads.cancel();
        if self.sound_data.is_none() {
            return;
        }
        info!("効果音を破棄した。以降スクリーンショットは無音になる");
        self.sound_data = None;
    }

    /// 適用する効果音の読み込みを始める。返した番号を読み込みの結果に添えて
    /// `finish_load` へ渡す。これより前に始めた読み込みの結果は以降捨てられる。
    ///
    /// 結果が届くまでは直前の音（無ければ内蔵音）で鳴らす（`select_shot_sound`）。
    pub fn begin_load(&mut self) -> u64 {
        self.loads.issue()
    }

    /// 読み込んだ効果音を反映する。最新の要求の結果でなければ何もせず
    /// `false` を返す。
    pub fn finish_load(&mut self, id: u64, data: Vec<u8>) -> bool {
        if !self.loads.complete(id) {
            return false;
        }
        self.sound_data = Some(data);
        true
    }

    /// テスト再生の読み込みを始める。適用済みの音には触れない。
    pub fn begin_test_play(&mut self) -> u64 {
        self.test_plays.issue()
    }

    /// テスト再生の読み込み結果を鳴らしてよいかを判定する。
    ///
    /// 「テスト再生」を続けて押した場合、鳴らすのは最後の 1 回だけ。
    /// 古い結果まで鳴らすと、ファイルを選び直す前の音が重なって聞こえる。
    pub fn finish_test_play(&mut self, id: u64) -> bool {
        self.test_plays.complete(id)
    }

    pub fn play_screenshot_sound(&self, volume: f32) {
        if let Some(sound_data) =
            select_shot_sound(self.sound_data.as_deref(), self.loads.is_pending())
        {
            play_sound_data(sound_data.to_vec(), volume);
        }
    }
}

/// 効果音の読み込み要求に振る番号と、結果を受け入れてよいかの判定。
///
/// 読み込みは要求ごとに別スレッドで行うので、先に出した要求の結果が後から
/// 届くことがある（大きいファイルから小さいファイルへ選び直した場合など）。
/// **受け入れるのは最後に出した要求の結果だけ。** 古い結果を反映すると、
/// ユーザーが最後に選んだものと違う音に戻ってしまう。
#[derive(Debug, Default)]
struct SoundLoadRequests {
    // 次に振る番号。巻き戻さない
    next: u64,
    // 結果を待っている要求。None は待っていないか、取り消したことを表す
    pending: Option<u64>,
}

impl SoundLoadRequests {
    /// 新しい要求に番号を振る。これより前の要求の結果は以降すべて捨てられる。
    fn issue(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.pending = Some(id);
        id
    }

    /// 届いた結果を受け入れてよいかを判定し、受け入れるなら待ちを終える。
    fn complete(&mut self, id: u64) -> bool {
        if !is_latest_sound_load(self.pending, id) {
            return false;
        }
        self.pending = None;
        true
    }

    /// 待っている要求を取り消す。以降に届いた結果はどれも受け入れない。
    fn cancel(&mut self) {
        self.pending = None;
    }

    /// 結果を待っている要求があるか。
    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// 届いた読み込み結果 `id` が、待っている最新の要求 `pending` のものかを判定する。
///
/// 待っていない（`None`）ときは何も受け入れない。取り消し（「効果音を鳴らさない」
/// への切り替え）のあとに古い読み込みが届いても、音を復活させないため。
fn is_latest_sound_load(pending: Option<u64>, id: u64) -> bool {
    pending == Some(id)
}

/// 撮影時に鳴らす音を選ぶ。`None` なら鳴らさない。
///
/// `applied` は適用済みの音（`None` は「鳴らさない」か、まだ何も読み込めて
/// いない）、`loading` は適用の読み込みが終わっていないか。
///
/// **読み込み中は直前の音で鳴らし、直前の音が無ければ内蔵音で鳴らす。**
/// 読み込みは別スレッドなので、起動直後や適用の直後に撮ると結果がまだ
/// 届いていないことがある。そこで無音にすると、撮れたのかが分からない。
/// 読み込み中でなく音も無いのは「鳴らさない」を選んだときだけ。
fn select_shot_sound(applied: Option<&[u8]>, loading: bool) -> Option<&[u8]> {
    match (applied, loading) {
        (Some(data), _) => Some(data),
        (None, true) => Some(EMBEDDED_SOUND),
        (None, false) => None,
    }
}

/// 設定の効果音パスから、鳴らす音のデータを読み込む。
///
/// **`ScreenshotManager` の状態には触れない。** 適用（`apply_settings`）と
/// 設定画面の「テスト再生」の両方がこれを通すので、テスト再生と撮影時で
/// 音の選び方が揃う。
///
/// **UI スレッドから呼ばない。** ファイル全体を読んでデコードを試すので、
/// 大きなファイルや遅いドライブでは描画が止まる。`app::screenshot_sound` が
/// 別スレッドから呼ぶ（Issue #214）。
///
/// 解決の仕方は `resolve_sound_path` と同じで、見つからなければ埋め込みの
/// 既定音を返す。ファイルがあるのに読めなかった場合も既定音を返し、
/// その理由を 2 つ目の値で添える。どの場合もデータは必ず返るので、
/// 呼び出し側は失敗を報告したうえでそのまま鳴らしてよい。
pub fn load_sound_data(sound_path: &Path) -> (Vec<u8>, Option<ScreenshotError>) {
    match resolve_sound_path(sound_path, exe_dir().as_deref(), |path| path.exists()) {
        SoundSource::Embedded => (EMBEDDED_SOUND.to_vec(), None),
        SoundSource::File(path) => match std::fs::read(&path) {
            Ok(data) => {
                // デコードできるかは選んだ時点（テスト再生と適用）で確かめる。
                // 撮影時の再生スレッドは失敗を黙って捨てるため、ここで見ないと
                // 無音の理由がどこにも出ない。データは差し替えない（撮影時は無音のまま）
                let error = check_decodable(&data).err().map(|source| {
                    ScreenshotError::SoundFileUndecodable {
                        path: path.clone(),
                        source,
                    }
                });
                (data, error)
            }
            Err(e) => (
                EMBEDDED_SOUND.to_vec(),
                Some(ScreenshotError::SoundFileUnreadable {
                    path,
                    source: e.to_string(),
                }),
            ),
        },
    }
}

/// 効果音のデータを rodio がデコードできるかを確かめる。失敗時は理由を返す。
fn check_decodable(data: &[u8]) -> Result<(), String> {
    Decoder::new(Cursor::new(data.to_vec()))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 効果音のデータを、別スレッドで 1 回鳴らす。
///
/// `volume` は設定画面と同じパーセント表記（100 で等倍、上限 200）。
/// 再生の終わりを待たずに戻る。
pub fn play_sound_data(sound_data: Vec<u8>, volume: f32) {
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
    fn rgb_to_rgba_inserts_opaque_alpha_after_every_pixel() {
        // 2x1 の赤と緑。並びを変えずにアルファだけが挟まること
        let rgb = vec![255, 0, 0, 0, 255, 0];

        let rgba = rgb_to_rgba(&rgb, 2, 1).expect("変換できること");

        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn rgb_to_rgba_ignores_trailing_bytes_beyond_the_frame() {
        // 幅と高さから決まる長さだけを使う。余りが付いていても無視する
        let rgb = vec![1, 2, 3, 9, 9, 9];

        let rgba = rgb_to_rgba(&rgb, 1, 1).expect("変換できること");

        assert_eq!(rgba, vec![1, 2, 3, 255]);
    }

    #[test]
    fn rgb_to_rgba_short_data_returns_error() {
        // 2x2 なら 12 バイト要る。足りない分を 0 で埋めて渡すと、
        // 画面に出ている画と違うものがクリップボードへ入る
        let rgb = vec![0; 11];

        let err = rgb_to_rgba(&rgb, 2, 2).expect_err("エラーになること");

        assert_eq!(
            err,
            ScreenshotError::FrameTooShort {
                width: 2,
                height: 2,
                len: 11,
            }
        );
    }

    #[test]
    fn rgb_to_rgba_zero_sized_frame_returns_error() {
        assert!(rgb_to_rgba(&[], 0, 10).is_err());
        assert!(rgb_to_rgba(&[], 10, 0).is_err());
    }

    #[test]
    fn rgb_to_rgba_overflowing_size_returns_error() {
        // 壊れた値が来ても掛け算が一周して短い長さを通さないこと
        let err = rgb_to_rgba(&[0; 8], usize::MAX, 2).expect_err("エラーになること");

        assert_eq!(
            err,
            ScreenshotError::FrameTooLarge {
                width: usize::MAX,
                height: 2,
            }
        );
    }

    #[test]
    fn screenshot_error_display_keeps_the_numbers_and_the_underlying_reason() {
        // 文言はそのままトーストに出る。大きさや下位のエラー文が落ちると
        // 何が起きたのか分からなくなる
        let too_short = ScreenshotError::FrameTooShort {
            width: 2,
            height: 2,
            len: 11,
        };
        assert_eq!(
            too_short.to_string(),
            "映像フレームの画素が足りない: 2x2 に対して 11 バイト"
        );

        let clipboard = ScreenshotError::ClipboardOpenFailed("access denied".to_string());
        assert_eq!(
            clipboard.to_string(),
            "クリップボードを開けない: access denied"
        );

        let sound = ScreenshotError::SoundFileUnreadable {
            path: PathBuf::from("C:/sounds/SS.mp3"),
            source: "permission denied".to_string(),
        };
        assert_eq!(
            sound.to_string(),
            "効果音ファイル C:/sounds/SS.mp3 を読み込めないため既定の効果音を使う: permission denied"
        );
    }

    #[test]
    fn screenshot_error_display_is_japanese_for_every_variant() {
        // 英語の文言が混ざると、定型文と繋げたときに日本語と英語が並ぶ
        let all = [
            ScreenshotError::EmptyFrame {
                width: 0,
                height: 10,
            },
            ScreenshotError::FrameTooLarge {
                width: usize::MAX,
                height: 2,
            },
            ScreenshotError::FrameTooShort {
                width: 2,
                height: 2,
                len: 11,
            },
            ScreenshotError::ClipboardOpenFailed("busy".to_string()),
            ScreenshotError::ClipboardWriteFailed("busy".to_string()),
            ScreenshotError::SoundFileUnreadable {
                path: PathBuf::from("C:/sounds/SS.mp3"),
                source: "missing".to_string(),
            },
            ScreenshotError::SoundFileUndecodable {
                path: PathBuf::from("C:/sounds/SS.mp3"),
                source: "unrecognized format".to_string(),
            },
        ];

        for error in all {
            let text = error.to_string();
            assert!(!text.is_ascii(), "日本語が含まれていない: {text}");
        }
    }

    #[test]
    fn load_sound_data_readable_file_returns_its_bytes() {
        // テスト再生でドラフトのファイルを選んだ場合。適用済みの音ではなく、
        // 渡したファイルの中身そのものが返ること（Issue #204）
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("custom.mp3");
        std::fs::write(&path, EMBEDDED_SOUND).expect("書き込めること");

        let (data, error) = load_sound_data(&path);

        assert_eq!(data, EMBEDDED_SOUND);
        assert_eq!(error, None);
    }

    #[test]
    fn load_sound_data_undecodable_file_keeps_bytes_with_reason() {
        // 拡張子だけ mp3 のファイルを選んだ場合（Issue #213）。
        // 既定音へは倒さず中身をそのまま返し、トーストへ出す理由を添えること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("fake.mp3");
        std::fs::write(&path, b"not really mp3").expect("書き込めること");

        let (data, error) = load_sound_data(&path);

        assert_eq!(data, b"not really mp3");
        assert!(
            matches!(error, Some(ScreenshotError::SoundFileUndecodable { path: ref p, .. }) if *p == path),
            "デコードできない理由が返ること: {error:?}"
        );
    }

    #[test]
    fn load_sound_data_default_path_returns_embedded_sound() {
        // 「既定に戻す」が書く値。既定値のファイルは配布していないので、
        // テスト再生でも撮影時と同じく内蔵音が鳴ること
        let (data, error) = load_sound_data(Path::new(crate::settings::DEFAULT_SOUND_FILE));

        assert_eq!(data, EMBEDDED_SOUND);
        assert_eq!(error, None);
    }

    #[test]
    fn load_sound_data_unreadable_file_falls_back_with_reason() {
        // 存在するのに読めないもの（ここではディレクトリ）を渡した場合。
        // 内蔵音で鳴らせるデータと、トーストへ出す理由の両方が返ること
        let dir = tempdir().expect("一時ディレクトリを作れること");

        let (data, error) = load_sound_data(dir.path());

        assert_eq!(data, EMBEDDED_SOUND);
        assert!(
            matches!(error, Some(ScreenshotError::SoundFileUnreadable { ref path, .. }) if path == dir.path()),
            "読めなかった理由が返ること: {error:?}"
        );
    }

    #[test]
    fn clear_sound_discards_loaded_sound() {
        // 設定の効果音を「クリア」したセッションで鳴り続けていた不具合の再現。
        let mut manager = ScreenshotManager::new();
        let id = manager.begin_load();
        assert!(manager.finish_load(id, EMBEDDED_SOUND.to_vec()));
        assert!(manager.sound_data.is_some());

        manager.clear_sound();

        assert!(manager.sound_data.is_none());
    }

    #[test]
    fn is_latest_sound_load_matching_pending_returns_true() {
        assert!(is_latest_sound_load(Some(3), 3));
    }

    #[test]
    fn is_latest_sound_load_older_request_returns_false() {
        // 先に出した要求の結果が後から届いた場合。最後の要求を上書きさせない
        assert!(!is_latest_sound_load(Some(3), 2));
    }

    #[test]
    fn is_latest_sound_load_nothing_pending_returns_false() {
        // 取り消し後や、受け入れ済みの要求の結果がもう一度来た場合
        assert!(!is_latest_sound_load(None, 0));
    }

    #[test]
    fn finish_load_overlapping_requests_keeps_only_the_latest() {
        // 同じファイルの読み込みが重なり、先の要求が後から終わった場合（Issue #214）。
        // 後の要求の結果だけが反映され、遅れて届いた先の結果は捨てられること
        let mut manager = ScreenshotManager::new();
        let first = manager.begin_load();
        let second = manager.begin_load();

        assert!(manager.finish_load(second, b"second".to_vec()));
        assert!(!manager.finish_load(first, b"first".to_vec()));

        assert_eq!(manager.sound_data.as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn finish_load_after_clear_sound_is_discarded() {
        // ファイルを選んだ直後に「効果音を鳴らさない」へ切り替えた場合。
        // 後から届いた読み込みで音が戻らないこと
        let mut manager = ScreenshotManager::new();
        let id = manager.begin_load();

        manager.clear_sound();

        assert!(!manager.finish_load(id, EMBEDDED_SOUND.to_vec()));
        assert!(manager.sound_data.is_none());
    }

    #[test]
    fn finish_test_play_repeated_clicks_play_only_the_last() {
        // 「テスト再生」を続けて押した場合、鳴らすのは最後の 1 回だけ
        let mut manager = ScreenshotManager::new();
        let first = manager.begin_test_play();
        let second = manager.begin_test_play();

        assert!(!manager.finish_test_play(first));
        assert!(manager.finish_test_play(second));
        // 同じ結果が 2 度来ても 2 度は鳴らさない
        assert!(!manager.finish_test_play(second));
    }

    #[test]
    fn begin_test_play_does_not_disturb_applied_load() {
        // テスト再生と適用は番号を別に数える。テスト再生を押しても
        // 読み込み中の適用の結果が捨てられないこと
        let mut manager = ScreenshotManager::new();
        let load = manager.begin_load();
        let _ = manager.begin_test_play();

        assert!(manager.finish_load(load, b"applied".to_vec()));
    }

    #[test]
    fn select_shot_sound_applied_sound_is_used_even_while_loading() {
        // 読み込み中は直前の音で鳴らす
        assert_eq!(
            select_shot_sound(Some(b"previous"), true),
            Some(&b"previous"[..])
        );
        assert_eq!(
            select_shot_sound(Some(b"previous"), false),
            Some(&b"previous"[..])
        );
    }

    #[test]
    fn select_shot_sound_loading_without_previous_uses_embedded() {
        // 起動直後や「鳴らさない」から切り替えた直後。無音にせず内蔵音で鳴らす
        assert_eq!(select_shot_sound(None, true), Some(EMBEDDED_SOUND));
    }

    #[test]
    fn select_shot_sound_nothing_loaded_and_idle_is_silent() {
        // 「効果音を鳴らさない」を選んでいる場合
        assert_eq!(select_shot_sound(None, false), None);
    }
}
