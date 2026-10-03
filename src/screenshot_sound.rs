//! スクリーンショットの効果音。埋め込みの既定音、設定のパスの解決、
//! 読み込み要求の番号の管理（`ScreenshotManager`）、読み込みと再生。
//!
//! 失敗の型はクリップボードへのコピーと共通の `crate::screenshot::ScreenshotError`。

use crate::screenshot::ScreenshotError;
use log::info;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// スクリーンショットの効果音を持つ。
///
/// **ホットキーの登録と押下の検出は持たない。** グローバルホットキーは
/// スクリーンショット以外のアクションにも割り当てられるため、`hotkey/` の
/// `HotkeyManager` が一手に扱う。
///
/// **ファイルの読み込みはしない。** 読み込みは `app::screenshot_sound` が
/// 別スレッドで行い、ここには番号の払い出し（`begin_load`）と結果の反映
/// （`finish_load`）だけを置く。大きなファイルや遅いドライブで UI スレッドが
/// 止まらないようにするため（Issue #214）。
pub struct ScreenshotManager {
    // 撮影のたびに再生スレッドへ渡すので `Arc` で持ち、中身は写さない（Issue #321）
    sound_data: Option<Arc<[u8]>>,
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
    pub fn finish_load(&mut self, id: u64, data: impl Into<Arc<[u8]>>) -> bool {
        if !self.loads.complete(id) {
            return false;
        }
        self.sound_data = Some(data.into());
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

    /// 撮影時に鳴らす音。`None` なら鳴らさない。返すのは `Arc` の複製だけなので、
    /// 呼び出し側はロックを離してから `play_sound_data` へ渡す。
    pub fn shot_sound(&self) -> Option<Arc<[u8]>> {
        select_shot_sound(self.sound_data.as_ref(), self.loads.is_pending())
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
fn select_shot_sound(applied: Option<&Arc<[u8]>>, loading: bool) -> Option<Arc<[u8]>> {
    match (applied, loading) {
        (Some(data), _) => Some(Arc::clone(data)),
        // 内蔵音は 12KB ほどで、読み込みを待っている間しか通らないので写してよい
        (None, true) => Some(Arc::from(EMBEDDED_SOUND)),
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
pub fn load_sound_data(sound_path: &Path) -> (Arc<[u8]>, Option<ScreenshotError>) {
    match resolve_sound_path(sound_path, exe_dir().as_deref(), |path| path.exists()) {
        SoundSource::Embedded => (Arc::from(EMBEDDED_SOUND), None),
        SoundSource::File(path) => match std::fs::read(&path) {
            Ok(data) => {
                // デコードできるかは選んだ時点（テスト再生と適用）で確かめる。
                // 撮影時の再生スレッドはデコードの失敗を黙って捨てるため、ここで
                // 見ないと無音の理由がどこにも出ない。データは差し替えない（撮影時は無音のまま）
                let error = check_decodable(&data).err().map(|source| {
                    ScreenshotError::SoundFileUndecodable {
                        path: path.clone(),
                        source,
                    }
                });
                (Arc::from(data), error)
            }
            Err(e) => (
                Arc::from(EMBEDDED_SOUND),
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
/// 再生の終わりを待たずに戻る。出力先を開けたかは `on_output` で返す。
/// **再生スレッドからはログを出さない**（保存スレッドと同じ。`docs/design/threads.md`）。
/// デコードできないデータは選んだ時点で知らせてあるので、ここでは黙って無音にする。
pub fn play_sound_data(
    sound_data: Arc<[u8]>,
    volume: f32,
    on_output: impl FnOnce(Result<(), ScreenshotError>) + Send + 'static,
) {
    let volume = (volume / 100.0).clamp(0.0, 2.0); // パーセンテージを0.0-2.0範囲に変換
    std::thread::spawn(move || play_blocking(open_output, sound_data, volume, on_output));
}

/// 出力先を開いて鳴らし終わるまで待つ。`open` を差し替えて失敗を注入できる。
fn play_blocking(
    open: impl FnOnce() -> Result<MixerDeviceSink, ScreenshotError>,
    sound_data: Arc<[u8]>,
    volume: f32,
    on_output: impl FnOnce(Result<(), ScreenshotError>),
) {
    // 出力先（`MixerDeviceSink`）は鳴り終わるまで持っておく。落とすと音が止まる
    let output = match open() {
        Ok(output) => output,
        Err(e) => return on_output(Err(e)),
    };
    on_output(Ok(()));
    let player = Player::connect_new(output.mixer());
    player.set_volume(volume);
    if let Ok(decoder) = Decoder::new(Cursor::new(sound_data)) {
        player.append(decoder);
        player.sleep_until_end();
    }
}

/// 既定の出力デバイスを開く。無い・開けないときは理由を返す。
///
/// 既定のデバイスで開けなければ、rodio が他の出力デバイスを順に試す（0.17 の
/// `OutputStream::try_default` と同じ振る舞い）。
fn open_output() -> Result<MixerDeviceSink, ScreenshotError> {
    let mut output = DeviceSinkBuilder::open_default_sink()
        .map_err(|e| ScreenshotError::SoundOutputUnavailable(e.to_string()))?;
    // 落とすときに rodio が標準エラーへ 1 行書く既定を切る。`println!` / `eprintln!` を
    // 足さない決まり（`docs/design/logging.md`）と同じ理由
    output.log_on_drop(false);
    Ok(output)
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
    fn load_sound_data_readable_file_returns_its_bytes() {
        // テスト再生でドラフトのファイルを選んだ場合。適用済みの音ではなく、
        // 渡したファイルの中身そのものが返ること（Issue #204）
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("custom.mp3");
        std::fs::write(&path, EMBEDDED_SOUND).expect("書き込めること");

        let (data, error) = load_sound_data(&path);

        assert_eq!(&*data, EMBEDDED_SOUND);
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

        assert_eq!(&*data, b"not really mp3");
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

        assert_eq!(&*data, EMBEDDED_SOUND);
        assert_eq!(error, None);
    }

    #[test]
    fn load_sound_data_unreadable_file_falls_back_with_reason() {
        // 存在するのに読めないもの（ここではディレクトリ）を渡した場合。
        // 内蔵音で鳴らせるデータと、トーストへ出す理由の両方が返ること
        let dir = tempdir().expect("一時ディレクトリを作れること");

        let (data, error) = load_sound_data(dir.path());

        assert_eq!(&*data, EMBEDDED_SOUND);
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
        // 読み込み中は直前の音で鳴らす。中身は写さず同じ領域を指すこと（Issue #321）
        let previous: Arc<[u8]> = Arc::from(&b"previous"[..]);
        for loading in [true, false] {
            let chosen = select_shot_sound(Some(&previous), loading).expect("鳴らすこと");
            assert!(Arc::ptr_eq(&chosen, &previous));
        }
    }

    #[test]
    fn select_shot_sound_loading_without_previous_uses_embedded() {
        // 起動直後や「鳴らさない」から切り替えた直後。無音にせず内蔵音で鳴らす
        assert_eq!(
            select_shot_sound(None, true).as_deref(),
            Some(EMBEDDED_SOUND)
        );
    }

    #[test]
    fn play_blocking_output_failure_is_returned_without_playing() {
        // 出力デバイスが無いとき。黙って捨てず、理由を呼び出し側へ返すこと（Issue #321）
        let mut outcome = None;
        let fail = || Err(ScreenshotError::SoundOutputUnavailable("NoDevice".into()));
        play_blocking(fail, Arc::from(EMBEDDED_SOUND), 1.0, |r| outcome = Some(r));
        assert_eq!(
            outcome,
            Some(Err(ScreenshotError::SoundOutputUnavailable(
                "NoDevice".into()
            )))
        );
    }

    #[test]
    #[ignore = "音声の出力デバイスが必要（鳴り終わるまで 1 秒ほど待つ）"]
    fn play_blocking_plays_the_embedded_sound_on_the_default_output() {
        // 実行: cargo test play_blocking_plays -- --ignored
        // 既定の出力デバイスを開いて内蔵音を鳴らし終わること（rodio 0.22 の経路、#299）
        let mut outcome = None;
        play_blocking(open_output, Arc::from(EMBEDDED_SOUND), 0.3, |r| {
            outcome = Some(r)
        });
        assert_eq!(outcome, Some(Ok(())));
    }

    #[test]
    fn select_shot_sound_nothing_loaded_and_idle_is_silent() {
        // 「効果音を鳴らさない」を選んでいる場合
        assert_eq!(select_shot_sound(None, false), None);
    }
}
