//! スクリーンショットの撮影と出力。
//!
//! エンコードと書き出し（クリップボードへの転送も）は撮影ごとに起こす
//! スレッドが行い、UI スレッドは結果をチャネルで受け取るだけにする。
//! 効果音の再生は `crate::screenshot::ScreenshotManager` の担当。

use super::CaptureCardViewer;
use crate::screenshot;
use crate::settings::ScreenshotEncoding;
use crate::status::ErrorSource;
use crate::video;
use chrono::Local;
use log::{debug, error, info, warn};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Instant;

/// スクリーンショットの出力結果。`(撮影を始めた時刻, 何をしたか / 失敗なら理由)`。
///
/// 保存スレッドから UI スレッドへ、この形でチャネル越しに返す。
/// **失敗だけでなく成功も送る。** 成功で直近の失敗の記録を消さないと、
/// 一度失敗したあとは設定画面に古い失敗が残り続ける。
///
/// **撮影時刻を添えるのは、結果が撮影順に届くとは限らないため。** 保存は
/// 撮影ごとにスレッドを起動するので、先に始めた保存が後から終わりうる。
/// 古い結果で新しい記録を上書きしないよう、受け取る側が時刻で弾く
pub(super) type ScreenshotResult = (Instant, ScreenshotOutcome);

/// 1 回の撮影の結末。成功なら何をしたかの文、失敗なら理由。
///
/// **出力先ごとの内訳ではなく、文字列 1 つに畳んである。** 受け取る UI
/// スレッドがすることは「ログに出す」と「失敗ならトーストに出す」だけで、
/// 出力先の種類で処理を分けないため。畳む規則は
/// `summarize_screenshot_delivery` が持つ。
type ScreenshotOutcome = Result<String, String>;

/// 出力先が「両方」のときに、片方だけ失敗した場合の結果の作り方。
///
/// 失敗が 1 つでもあれば全体を失敗として扱い、理由を並べる。成功したほうを
/// 黙って捨てないよう、文言には成功した出力先も残す。
///
/// 引数の `None` は「その出力先が設定に含まれていない」を表す。`Some` は
/// 実際に試した結果。
///
/// アプリの状態に触れないのでそのまま別スレッドで実行でき、テストからも呼べる。
fn summarize_screenshot_delivery(
    clipboard: Option<Result<(), String>>,
    file: Option<Result<PathBuf, String>>,
) -> ScreenshotOutcome {
    let (copied, clipboard_error) = match clipboard {
        Some(Ok(())) => (true, None),
        Some(Err(reason)) => (false, Some(reason)),
        None => (false, None),
    };
    let (saved_to, file_error) = match file {
        Some(Ok(path)) => (Some(path), None),
        Some(Err(reason)) => (None, Some(reason)),
        None => (None, None),
    };

    let succeeded = match (copied, &saved_to) {
        (true, Some(path)) => Some(format!(
            "クリップボードへコピーし、{} へ保存した",
            path.display()
        )),
        (true, None) => Some("クリップボードへコピーした".to_string()),
        (false, Some(path)) => Some(format!("{} へ保存した", path.display())),
        (false, None) => None,
    };

    let failures: Vec<String> = [clipboard_error, file_error]
        .into_iter()
        .flatten()
        .collect();
    if !failures.is_empty() {
        let mut reason = failures.join(" / ");
        // 片方だけ失敗した場合に、成功したほうを黙って捨てない。
        // 「クリップボードには入っているのか」が分からないと次の操作を選べない
        if let Some(done) = succeeded {
            reason = format!("{}（{}）", reason, done);
        }
        return Err(reason);
    }

    // 出力先の enum が必ずどちらかを含むので通常は起きない。
    // 黙って成功にすると、何も出力していないのに撮れたように見える
    succeeded.ok_or_else(|| "出力先が 1 つも設定されていません".to_string())
}

/// 届いた結果を画面の記録へ反映してよいかを判定する。
///
/// `last` は画面の記録へ反映済みの中で最も新しい撮影の開始時刻で、`None` は
/// 「まだ何も反映していない」を表す。`started_at` は届いた結果の撮影時刻。
///
/// **同時刻は反映する側に倒す。** `Instant` は単調増加するので、別の撮影が
/// まったく同じ時刻になることは通常起きないが、起きたとしても取りこぼす
/// より出すほうがよい。
fn screenshot_outcome_supersedes(last: Option<Instant>, started_at: Instant) -> bool {
    match last {
        None => true,
        Some(last) => started_at >= last,
    }
}

/// 完了済みのスレッドハンドルを取り除く。
///
/// `JoinHandle` を持ち続けるのは終了時に `join` するためだけなので、
/// 終わったものは落としてよい。落とさないと撮影のたびに要素が増え続ける
fn drop_finished_threads<T>(handles: &mut Vec<JoinHandle<T>>) {
    handles.retain(|handle| !handle.is_finished());
}

/// 映像フレームを `encoding` の形式で `path` へ書き出す。
///
/// アプリの状態にも共有ロックにも触れないので、そのまま別スレッドで実行でき、
/// テストからも呼べる。保存スレッドはこの関数だけを呼ぶ。
fn save_frame(
    frame: &video::VideoFrame,
    path: &Path,
    encoding: ScreenshotEncoding,
) -> Result<(), String> {
    // 大きさのないフレームは画像として書き出せてしまうが、開けない
    // ファイルが残るだけなので、ディレクトリを作る前に弾く
    if frame.width == 0 || frame.height == 0 {
        return Err(format!(
            "大きさのない映像フレームは保存できない: {}x{}",
            frame.width, frame.height
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "保存先のディレクトリ {} を作成できない: {}",
                parent.display(),
                e
            )
        })?;
    }

    let (Ok(width), Ok(height)) = (u32::try_from(frame.width), u32::try_from(frame.height)) else {
        return Err(format!(
            "画像として扱えない大きさのフレーム: {}x{}",
            frame.width, frame.height
        ));
    };

    // image クレートが Vec の所有権を要求するため、ここだけは複製が要る。
    // UI スレッドの外なので、1080p で 6MB の複製が描画を止めることはない
    let img = image::RgbImage::from_raw(width, height, frame.data.clone()).ok_or_else(|| {
        format!(
            "映像フレームから画像を組み立てられない: {}x{} に対して {} バイト",
            width,
            height,
            frame.data.len()
        )
    })?;

    // image の save() は拡張子から形式を決めるうえ、JPEG は品質 75 固定に
    // なるため使わない。形式は encoding で決め、書き出し先は自分で開く
    let file = std::fs::File::create(path)
        .map_err(|e| format!("{} を作成できない: {}", path.display(), e))?;
    let mut writer = std::io::BufWriter::new(file);

    let encoded = match encoding {
        ScreenshotEncoding::Jpeg { quality } => img.write_with_encoder(
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality),
        ),
        ScreenshotEncoding::Png => {
            img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut writer))
        }
    };

    // BufWriter は drop のときにも書き出すが、そこで起きた失敗は捨てられる。
    // 取りこぼすと、書き切れていないファイルを保存できたものとして扱ってしまう
    let result = encoded
        .map_err(|e| format!("{} へ書き出せない: {}", path.display(), e))
        .and_then(|()| {
            writer
                .into_inner()
                .map_err(|e| format!("{} へ書き出せない: {}", path.display(), e))
        })
        .and_then(|file| {
            file.sync_all()
                .map_err(|e| format!("{} を書き切れない: {}", path.display(), e))
        });

    if result.is_err() {
        // 途中まで書けたファイルを残さない。残すと開けない画像が
        // 保存先に紛れ込み、次の撮影では連番の相手にもなる
        if let Err(e) = std::fs::remove_file(path) {
            warn!(
                "書き出しに失敗した {} を削除できない: {}",
                path.display(),
                e
            );
        }
    }

    result
}

impl CaptureCardViewer {
    /// いま表示しているフレームを、設定した出力先（ファイル / クリップボード /
    /// 両方）へ出す。ファイルへは設定した形式（JPEG / PNG）で保存する。
    ///
    /// ロックは settings → video → screenshot の順に 1 つずつ取り、重ねない。
    /// エンコードと書き出しは別スレッドへ逃がす。1080p のエンコードは
    /// JPEG でも数十 ms かかり、UI スレッドで行うと映像が一瞬止まるため
    /// （PNG は可逆圧縮のぶんさらに時間がかかる）。クリップボードへの転送も
    /// 同じスレッドで行う。こちらは他のアプリがクリップボードを掴んでいると
    /// 待たされるため、UI スレッドに置けない
    pub(super) fn take_screenshot(&mut self) {
        debug!("スクリーンショットの出力を開始する");

        // この撮影を識別する時刻。結果が撮影順に届かないときの追い越し判定に使う。
        // ファイル名のタイムスタンプはミリ秒までなので同一ミリ秒で並びうるが、
        // `Instant` は単調増加するのでこちらは必ず順序が付く
        let started_at = Instant::now();

        // 出力先と効果音の音量だけを取り出してロックを手放す。
        // get_screenshot_path は連番を決めるためにファイルの有無を見るが、
        // ファイルを作るのは保存スレッドなので、ここでは何も書かない
        let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S-%3f").to_string();
        let save_params = self.settings.lock().ok().map(|settings| {
            let destination = settings.screenshot.destination;
            // クリップボードだけのときはファイル名を作らない。
            // get_screenshot_path は連番を決めるために保存先フォルダを
            // 走査するので、使わない名前のために I/O を走らせない
            let file_target = destination.saves_file().then(|| {
                (
                    settings.get_screenshot_path(&timestamp),
                    settings.screenshot.encoding(),
                )
            });
            (
                destination.copies_to_clipboard(),
                file_target,
                settings.screenshot.sound_volume,
            )
        });
        let Some((to_clipboard, file_target, sound_volume)) = save_params else {
            warn!("スクリーンショットの出力で settings のロックを取得できない");
            return;
        };

        // 最新フレームを取り出したらすぐロックを手放す。Arc の複製なので
        // 画素データは複製されず、フレームコールバック側の push を待たせない。
        // スクリーンショットはいま画面に出ている画を保存するので、新着でなくてよい
        let Some(frame) = self.frames.latest() else {
            warn!("映像フレームが無いのでスクリーンショットを撮れない");
            // ホットキーを押しても何も起きないように見えるので画面にも出す。
            // 非同期の結果と同じ経路を通して、先に始めた保存の結果に
            // 追い越されないようにする
            self.apply_screenshot_outcome(started_at, Err("表示中の映像がありません".to_string()));
            return;
        };
        debug!(
            "出力対象の映像フレームを取得した: {}x{}、クリップボード: {}、保存先: {}",
            frame.width,
            frame.height,
            to_clipboard,
            file_target.as_ref().map_or_else(
                || "なし".to_string(),
                |(path, _)| path.display().to_string()
            )
        );

        // 効果音は保存の完了を待たずに鳴らす。撮った手応えをその場で返すため。
        // 保存まで待つと、エンコードにかかる数十 ms だけシャッター音が遅れる。
        // 保存に失敗した場合は音だけ鳴ることになるが、失敗はログに残す
        if let Ok(ss) = self.screenshot_manager.lock() {
            ss.play_screenshot_sound(sound_volume);
        } else {
            warn!("スクリーンショットの効果音で screenshot_manager のロックを取得できない");
        }

        // エンコードと書き出しは UI スレッドから外す。
        // ホットキーを連打するとスレッドが並ぶが、撮るたびに 1 枚残るほうを優先して
        // 進行中の保存があっても捨てない。ファイル名は撮影時刻をミリ秒まで含むので、
        // 人が連打できる間隔なら衝突しない（同一ミリ秒の衝突は元からある別の問題）
        // 結果は UI スレッドへ返す。失敗をログだけに出すと、保存先が書き込み
        // 不可のときにホットキーを押しても何も起きないように見える。
        // ログ出力も UI スレッド側（drain_screenshot_results）へ寄せてある
        let result_tx = self.screenshot_tx.clone();
        let handle = std::thread::spawn(move || {
            // 両方のときはクリップボードを先にする。撮ってすぐ貼る使い方で、
            // ディスクへの書き出しを待たせないため。
            // **片方が失敗しても他方は行う。** クリップボードを他のアプリが
            // 掴んでいてコピーできなくても、ファイルは残したい
            // `summarize_screenshot_delivery` は理由を 1 本の文へ畳むだけなので、
            // ここで日本語の 1 行に落として渡す
            let clipboard = to_clipboard
                .then(|| screenshot::copy_frame_to_clipboard(&frame).map_err(|e| e.to_string()));
            let file = file_target
                .map(|(path, encoding)| save_frame(&frame, &path, encoding).map(|()| path));

            let result = summarize_screenshot_delivery(clipboard, file);
            if result_tx.send((started_at, result)).is_err() {
                // 受信側が無いのはアプリが終了したときだけ。結果は捨ててよい
                debug!("スクリーンショットの結果の送り先が既に無いので捨てる");
            }
        });

        // ハンドルを持っておく。捨てるとスレッドが切り離され、終了時に
        // 書き出しの完了を待てなくなる（壊れた画像ファイルが残りうる）。
        // 溜め込まないよう、積む前に終わった分を落とす
        drop_finished_threads(&mut self.screenshot_save_threads);
        self.screenshot_save_threads.push(handle);
    }

    /// 進行中のスクリーンショット保存がすべて終わるまで待つ。
    ///
    /// 待ち時間はエンコードとディスクへの書き出し（出力先にクリップボードが
    /// 含まれる場合はその転送も）が終わるまでで、1080p の JPEG なら通常は
    /// 数十 ms。終了時に呼ぶ
    pub(super) fn join_screenshot_save_threads(&mut self) {
        let handles = std::mem::take(&mut self.screenshot_save_threads);
        if handles.is_empty() {
            return;
        }

        debug!(
            "スクリーンショットの保存スレッド {} 件を待つ",
            handles.len()
        );
        for handle in handles {
            if handle.join().is_err() {
                // release ビルドは panic = "abort" なのでここには来ない
                warn!("スクリーンショットの保存スレッドがパニックした");
            }
        }
    }

    /// 別スレッドから届いたスクリーンショットの保存結果を取り込む。
    ///
    /// 保存は撮影ごとに spawn したスレッドが行うため、失敗をその場で画面に
    /// 出せない。結果をここで受け取って、失敗ならトーストにする。
    ///
    /// **ログは届いた結果すべてについて出す。** 画面の記録は追い越しを弾くが、
    /// ログまで落とすと何が起きたか追えなくなる。
    pub(super) fn drain_screenshot_results(&mut self) {
        while let Ok((started_at, result)) = self.screenshot_rx.try_recv() {
            match &result {
                Ok(done) => info!("スクリーンショットを{}", done),
                Err(reason) => error!("スクリーンショットを出力できない: {}", reason),
            }
            self.apply_screenshot_outcome(started_at, result);
        }
    }

    /// スクリーンショットの結果を画面の記録へ反映する。
    ///
    /// **先に始めた保存の結果が後から届いても、新しい記録を上書きしない。**
    /// 保存は撮影ごとにスレッドを起動するため、エンコードにかかる時間が
    /// 違えば終わる順も入れ替わる。そのまま反映すると、古い保存の成功が
    /// 新しい保存の失敗を消してしまう。
    ///
    /// ログ出力は呼び出し側が済ませてある。ここは画面へ出す記録だけを扱う。
    fn apply_screenshot_outcome(&mut self, started_at: Instant, result: ScreenshotOutcome) {
        if !screenshot_outcome_supersedes(self.last_screenshot_outcome_at, started_at) {
            debug!("先に始めた保存の結果が後から届いたので、画面の記録は更新しない");
            return;
        }
        self.last_screenshot_outcome_at = Some(started_at);

        match result {
            // 直前の失敗が解消したので記録を消す。残すと古い失敗が出続ける
            Ok(_) => self.errors.clear(ErrorSource::Screenshot),
            Err(reason) => self.report_error(ErrorSource::Screenshot, reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn screenshot_outcome_supersedes_without_previous_result_returns_true() {
        assert!(screenshot_outcome_supersedes(None, Instant::now()));
    }

    #[test]
    fn screenshot_outcome_supersedes_newer_result_returns_true() {
        let first = Instant::now();
        assert!(screenshot_outcome_supersedes(
            Some(first),
            first + Duration::from_millis(1)
        ));
    }

    #[test]
    fn screenshot_outcome_supersedes_older_result_returns_false() {
        // 先に始めた保存が後から終わった場合。新しい記録を上書きさせない
        let first = Instant::now();
        let second = first + Duration::from_millis(50);
        assert!(!screenshot_outcome_supersedes(Some(second), first));
    }

    #[test]
    fn screenshot_outcome_supersedes_same_instant_returns_true() {
        // 境界。取りこぼすより出すほうに倒す
        let now = Instant::now();
        assert!(screenshot_outcome_supersedes(Some(now), now));
    }

    #[test]
    fn summarize_screenshot_delivery_file_only_reports_the_path() {
        let outcome =
            summarize_screenshot_delivery(None, Some(Ok(PathBuf::from(r"C:\shots\a.jpg"))));

        assert_eq!(outcome, Ok(r"C:\shots\a.jpg へ保存した".to_string()));
    }

    #[test]
    fn summarize_screenshot_delivery_clipboard_only_reports_the_copy() {
        let outcome = summarize_screenshot_delivery(Some(Ok(())), None);

        assert_eq!(outcome, Ok("クリップボードへコピーした".to_string()));
    }

    #[test]
    fn summarize_screenshot_delivery_both_reports_the_copy_before_the_path() {
        // 実際の処理順（クリップボード → ファイル）と同じ並びにする
        let outcome =
            summarize_screenshot_delivery(Some(Ok(())), Some(Ok(PathBuf::from(r"C:\shots\a.png"))));

        assert_eq!(
            outcome,
            Ok(r"クリップボードへコピーし、C:\shots\a.png へ保存した".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_clipboard_failure_keeps_the_saved_path_in_the_reason() {
        // 片方だけ失敗した場合。全体は失敗だが、成功したほうも文言に残す。
        // クリップボードに入っていないことと、ファイルは残っていることの
        // 両方が分からないと、ユーザーは次に何をすればよいか決められない
        let outcome = summarize_screenshot_delivery(
            Some(Err("クリップボードを開けない: occupied".to_string())),
            Some(Ok(PathBuf::from(r"C:\shots\a.jpg"))),
        );

        assert_eq!(
            outcome,
            Err(r"クリップボードを開けない: occupied（C:\shots\a.jpg へ保存した）".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_file_failure_keeps_the_copy_in_the_reason() {
        let outcome = summarize_screenshot_delivery(
            Some(Ok(())),
            Some(Err(
                r"C:\shots\a.jpg を作成できない: access denied".to_string()
            )),
        );

        assert_eq!(
            outcome,
            Err(
                r"C:\shots\a.jpg を作成できない: access denied（クリップボードへコピーした）"
                    .to_string()
            )
        );
    }

    #[test]
    fn summarize_screenshot_delivery_both_failures_are_joined() {
        let outcome = summarize_screenshot_delivery(
            Some(Err("クリップボードを開けない".to_string())),
            Some(Err("書き込めない".to_string())),
        );

        assert_eq!(
            outcome,
            Err("クリップボードを開けない / 書き込めない".to_string())
        );
    }

    #[test]
    fn summarize_screenshot_delivery_without_any_destination_is_an_error() {
        // 出力先の enum が必ずどちらかを含むので通常は起きないが、
        // 何もしていないのに成功として扱わないこと
        assert!(summarize_screenshot_delivery(None, None).is_err());
    }

    // 書き出したファイルの中身から画像形式を判定する。
    // 拡張子ではなく実際のバイト列を見る
    fn detect_format(path: &Path) -> image::ImageFormat {
        let reader = image::io::Reader::open(path)
            .expect("保存したファイルを開けること")
            .with_guessed_format()
            .expect("形式を判定できること");
        reader.format().expect("形式が分かること")
    }

    // 2x2 の RGB フレーム。赤・緑・青・白を 1 画素ずつ並べてある
    fn test_frame_2x2() -> video::VideoFrame {
        video::VideoFrame {
            width: 2,
            height: 2,
            data: vec![
                255, 0, 0, // 左上: 赤
                0, 255, 0, // 右上: 緑
                0, 0, 255, // 左下: 青
                255, 255, 255, // 右下: 白
            ],
        }
    }

    // 品質の差がファイルサイズに出るように、細かく変化する模様を敷いた画像。
    // 一様な色だとどの品質でもほぼ同じ大きさに圧縮され、差を見られない
    fn detailed_frame_64x64() -> video::VideoFrame {
        let mut data = Vec::with_capacity(64 * 64 * 3);
        for y in 0..64u32 {
            for x in 0..64u32 {
                data.push((x * 37 + y * 11) as u8);
                data.push((x * 7 + y * 53) as u8);
                data.push((x * 91 + y * 29) as u8);
            }
        }
        video::VideoFrame {
            width: 64,
            height: 64,
            data,
        }
    }

    const JPEG_Q90: ScreenshotEncoding = ScreenshotEncoding::Jpeg { quality: 90 };

    #[test]
    fn save_frame_jpeg_writes_decodable_file() {
        // JPEG は非可逆なので画素値は比較せず、読み戻せることと大きさだけを見る
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, JPEG_Q90).expect("保存できること");

        let decoded = image::open(&path).expect("保存した JPEG を読み戻せること");
        assert_eq!(decoded.dimensions(), (2, 2));
        // 拡張子ではなく指定した形式で書けていること
        assert_eq!(image::ImageFormat::Jpeg, detect_format(&path));
    }

    #[test]
    fn save_frame_png_writes_pixels_without_loss() {
        // PNG は可逆なので、元の画素がそのまま戻ることまで確かめる
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.png");
        let frame = test_frame_2x2();

        save_frame(&frame, &path, ScreenshotEncoding::Png).expect("保存できること");

        let decoded = image::open(&path).expect("保存した PNG を読み戻せること");
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(image::ImageFormat::Png, detect_format(&path));
        assert_eq!(decoded.to_rgb8().into_raw(), frame.data);
    }

    #[test]
    fn save_frame_png_ignores_jpg_extension() {
        // 拡張子は get_screenshot_path が形式に合わせるので普段は一致するが、
        // 書き出す形式を決めるのは encoding だけであることを固定しておく
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, ScreenshotEncoding::Png).expect("保存できること");

        assert_eq!(image::ImageFormat::Png, detect_format(&path));
    }

    #[test]
    fn save_frame_lower_jpeg_quality_produces_smaller_file() {
        // 品質の指定がエンコーダまで届いていること
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let low_path = dir.path().join("low.jpg");
        let high_path = dir.path().join("high.jpg");
        let frame = detailed_frame_64x64();

        save_frame(&frame, &low_path, ScreenshotEncoding::Jpeg { quality: 10 })
            .expect("保存できること");
        save_frame(
            &frame,
            &high_path,
            ScreenshotEncoding::Jpeg { quality: 100 },
        )
        .expect("保存できること");

        let low = std::fs::metadata(&low_path)
            .expect("大きさを取れること")
            .len();
        let high = std::fs::metadata(&high_path)
            .expect("大きさを取れること")
            .len();
        assert!(
            low < high,
            "品質 10 が品質 100 より小さくない: {} >= {}",
            low,
            high
        );
    }

    #[test]
    fn save_frame_creates_missing_parent_directory() {
        // 保存先フォルダが無い状態で撮影されることがあるため、親ごと作る
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shots").join("2026").join("shot.jpg");

        save_frame(&test_frame_2x2(), &path, JPEG_Q90).expect("保存できること");

        assert!(path.exists());
    }

    #[test]
    fn save_frame_short_data_returns_error_without_creating_file() {
        // 画素数に対してデータが足りないフレーム。壊れたファイルを残さないこと
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");
        let frame = video::VideoFrame {
            width: 2,
            height: 2,
            data: vec![0; 11],
        };

        let err = save_frame(&frame, &path, JPEG_Q90).expect_err("エラーになること");

        assert!(err.contains("組み立てられない"), "想定外のエラー: {}", err);
        assert!(!path.exists());
    }

    #[test]
    fn save_frame_zero_sized_frame_returns_error() {
        // フレームが来ていない状態を取り違えて保存しようとした場合
        let dir = tempdir().expect("一時ディレクトリを作れること");
        let path = dir.path().join("shot.jpg");
        let frame = video::VideoFrame {
            width: 0,
            height: 0,
            data: Vec::new(),
        };

        let err = save_frame(&frame, &path, JPEG_Q90).expect_err("エラーになること");

        assert!(err.contains("大きさのない"), "想定外のエラー: {}", err);
        assert!(!path.exists());
    }

    #[test]
    fn drop_finished_threads_empty_stays_empty() {
        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        drop_finished_threads(&mut handles);

        assert!(handles.is_empty());
    }

    #[test]
    fn drop_finished_threads_removes_only_completed_handles() {
        // 合図が来るまで終わらないスレッドを 1 本混ぜ、
        // 終わった分だけが落ちることを見る
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut handles = vec![
            std::thread::spawn(|| {}),
            std::thread::spawn(move || {
                let _ = release_rx.recv();
            }),
        ];

        // is_finished はスレッドが抜けきってから true になるため、待ち合わせる
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handles[0].is_finished() {
            assert!(Instant::now() < deadline, "1 本目のスレッドが終わらない");
            std::thread::sleep(Duration::from_millis(1));
        }

        drop_finished_threads(&mut handles);

        assert_eq!(handles.len(), 1, "終わっていないスレッドだけが残ること");
        assert!(
            !handles[0].is_finished(),
            "残ったのは実行中のスレッドであること"
        );

        // 後始末。合図を送ってからでないとスレッドが残る
        release_tx.send(()).expect("合図を送れること");
        handles
            .remove(0)
            .join()
            .expect("実行中だったスレッドを回収できること");
    }
}
