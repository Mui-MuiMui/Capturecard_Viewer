//! Windows 固有の処理と、起動時にウィンドウを置く位置の判定。
//!
//! 日本語フォントの探索、埋め込みアイコンの読み込み、モニタの作業領域の列挙と、
//! 保存されたウィンドウの大きさ・位置が使えるかの判定を集めてある。
//! いずれも `main()` が `eframe::run_native` より前に呼ぶ（`monitor_work_areas`
//! のコメントを参照）。

use crate::i18n::Language;
use eframe::egui;
use image::GenericImageView;
use log::{info, warn};
use std::path::PathBuf;

/// 保存されたウィンドウサイズが使えない場合に使う大きさ
pub(crate) const DEFAULT_WINDOW_SIZE: (f32, f32) = (1280.0, 720.0);

/// 復元したウィンドウを「画面内にある」と見なすために必要な、モニタの作業領域との
/// 重なりの最小幅と最小高さ。タイトルバーを掴んでウィンドウを動かせる程度の
/// 大きさを見えていることの条件にしている
const MIN_VISIBLE_WINDOW_WIDTH: f32 = 120.0;
const MIN_VISIBLE_WINDOW_HEIGHT: f32 = 32.0;

/// 保存されたウィンドウサイズのうち、ウィンドウとして成立する値だけを採用して返す。
///
/// 設定ファイルは手で編集できるため、0 や負数や NaN が入りうる。検証せずに
/// `with_inner_size` へ渡すと、winit の先の OS の API 次第で操作できない大きさの
/// ウィンドウになったり、起動そのものに失敗したりする。**採用できない値は
/// 既定のサイズへ倒す。**
pub(crate) fn window_size_or_default(saved: Option<(f32, f32)>) -> (f32, f32) {
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
pub(crate) fn is_position_visible(
    pos: (f32, f32),
    size: (f32, f32),
    monitors: &[egui::Rect],
) -> bool {
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
pub(crate) fn monitor_work_areas() -> Vec<egui::Rect> {
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
pub(crate) fn monitor_work_areas() -> Vec<egui::Rect> {
    Vec::new()
}

/// OS の表示言語から、画面に出す言語を推定する。設定の言語が「自動」のときに使う。
///
/// 見るのは**表示言語**（`GetUserDefaultUILanguage`）で、地域の書式
/// （`GetUserDefaultLocaleName`）ではない。英語の Windows で日付や通貨の書式だけ
/// 日本にしている人に、日本語の画面を出さないため。
#[cfg(windows)]
pub(crate) fn os_ui_language() -> Language {
    use winapi::um::winnls::GetUserDefaultUILanguage;

    // 失敗を返さない API。取れなかった場合も何かしらの LANGID が返る
    let langid = unsafe { GetUserDefaultUILanguage() };
    let language = language_from_langid(langid);
    info!(
        "OS の表示言語は LANGID 0x{:04x}、自動のときの言語は {:?}",
        langid, language
    );
    language
}

/// Windows 以外では OS の言語を問い合わせない。英語として扱う
#[cfg(not(windows))]
pub(crate) fn os_ui_language() -> Language {
    Language::English
}

/// LANGID から画面の言語を決める。主言語が日本語なら日本語、それ以外は英語。
///
/// 下位 10 ビットが主言語（`PRIMARYLANGID`）で、上位 6 ビットの副言語（地域）は見ない。
fn language_from_langid(langid: u16) -> Language {
    // winnt.h の LANG_JAPANESE
    const LANG_JAPANESE: u16 = 0x11;
    if langid & 0x3ff == LANG_JAPANESE {
        Language::Japanese
    } else {
        Language::English
    }
}

/// 日本語フォントの候補。優先度順（先頭ほど優先）。
/// (ログ・フォント名として使う表示名, フォントファイル名)
///
/// Meiryo が入っていない環境（Windows の言語パックを最小構成にした場合など）でも
/// 日本語が豆腐（□）にならないよう、Windows に同梱されていることが多いフォントを
/// 順に候補として並べてある。
const JAPANESE_FONT_CANDIDATES: &[(&str, &str)] = &[
    ("Meiryo", "meiryo.ttc"),
    ("Yu Gothic UI (Medium)", "YuGothM.ttc"),
    ("Yu Gothic UI (Regular)", "YuGothR.ttc"),
    ("Yu Gothic", "yugothic.ttf"),
    ("MS Gothic", "msgothic.ttc"),
    ("BIZ UDGothic", "BIZ-UDGothicR.ttc"),
];

/// `JAPANESE_FONT_CANDIDATES` を優先度順に、`font_dirs` を引数の順に探し、
/// 最初に実在したファイルを返す。デバイスや `%AppData%` に触らない純粋関数にするため、
/// 探索対象のディレクトリ一覧は呼び出し側から渡す。
fn find_japanese_font(font_dirs: &[PathBuf]) -> Option<(&'static str, PathBuf)> {
    for (name, filename) in JAPANESE_FONT_CANDIDATES {
        for dir in font_dirs {
            let path = dir.join(filename);
            if path.is_file() {
                return Some((name, path));
            }
        }
    }
    None
}

/// 日本語フォントを探すディレクトリ一覧。システム共通のフォントディレクトリに加えて、
/// 管理者権限なしでユーザー単位にインストールされたフォント（「自分のみにインストール」）
/// も見る。パスは `%WINDIR%` / `%LOCALAPPDATA%` から組み立て、`C:\Windows` のように
/// 決め打ちにしない。通常の Windows 環境ではどちらも設定されているが、
/// 万一 `%WINDIR%` が取れない場合はシステム共通のフォントディレクトリを諦め、
/// ユーザー単位のフォントディレクトリだけを見る
#[cfg(target_os = "windows")]
fn japanese_font_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("WINDIR") {
        Some(windir) => dirs.push(PathBuf::from(windir).join("Fonts")),
        None => warn!(
            "環境変数 WINDIR が取得できないため、システム共通のフォントディレクトリは探索しません"
        ),
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local_app_data).join(r"Microsoft\Windows\Fonts"));
    }
    dirs
}

pub(crate) fn configure_japanese_font(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    {
        let font_dirs = japanese_font_search_dirs();
        match find_japanese_font(&font_dirs) {
            Some((name, path)) => match std::fs::read(&path) {
                Ok(data) => {
                    info!(
                        "日本語フォントとして {} を使用します ({})",
                        name,
                        path.display()
                    );
                    let mut fonts = egui::FontDefinitions::default();
                    fonts
                        .font_data
                        .insert("japanese".to_string(), egui::FontData::from_owned(data));
                    // 優先度のためにプロポーショナル・等幅フォントファミリーの先頭に挿入する。
                    // egui 既定の絵文字フォント等はそのまま残るため、Meiryo 等に無い記号は
                    // 引き続きフォールバックで描画される
                    if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                        fam.insert(0, "japanese".to_string());
                    }
                    if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                        fam.insert(0, "japanese".to_string());
                    }
                    ctx.set_fonts(fonts);
                }
                Err(e) => {
                    warn!(
                        "日本語フォント候補 {} の読み込みに失敗しました: {}",
                        path.display(),
                        e
                    );
                }
            },
            None => {
                let tried: Vec<&str> = JAPANESE_FONT_CANDIDATES
                    .iter()
                    .map(|(_, filename)| *filename)
                    .collect();
                warn!(
                    "日本語フォント候補が 1 つも見つかりませんでした（探索先: {:?}, 候補: {}）。既定フォントのまま起動します",
                    font_dirs,
                    tried.join(", ")
                );
            }
        }
    }
}

// ウィンドウアイコン。実行ファイルに埋め込む。
// 以前はカレントディレクトリ基準で "icon.ico" を読んでいたため、ショートカット経由など
// 作業ディレクトリが exe の場所と異なる起動ではファイルを見つけられず、
// フォールバックの赤い四角が表示されていた。
const EMBEDDED_ICON: &[u8] = include_bytes!("../icon.ico");

pub(crate) fn load_icon() -> egui::IconData {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn language_from_langid_japanese_only_for_japanese() {
        // ja-JP（0x0411）
        assert_eq!(language_from_langid(0x0411), Language::Japanese);
        // 副言語（上位ビット）が違っても主言語が日本語なら日本語
        assert_eq!(language_from_langid(0x0011), Language::Japanese);
        // en-US（0x0409）、en-GB（0x0809）、zh-CN（0x0804）、ko-KR（0x0412）
        for langid in [0x0409, 0x0809, 0x0804, 0x0412] {
            assert_eq!(
                language_from_langid(langid),
                Language::English,
                "{langid:#06x}"
            );
        }
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

    #[test]
    fn find_japanese_font_prefers_earlier_candidate_when_multiple_exist() {
        // Meiryo と Yu Gothic が両方入っている環境では、優先度が高い Meiryo を選ぶこと
        let dir = tempdir().expect("tempdir を作れること");
        std::fs::write(dir.path().join("meiryo.ttc"), b"dummy").unwrap();
        std::fs::write(dir.path().join("YuGothR.ttc"), b"dummy").unwrap();

        let found = find_japanese_font(&[dir.path().to_path_buf()]);

        assert_eq!(
            found,
            Some(("Meiryo", dir.path().join("meiryo.ttc"))),
            "候補リストで先に並ぶ Meiryo が選ばれること"
        );
    }

    #[test]
    fn find_japanese_font_returns_none_when_no_candidate_exists() {
        // 候補が 1 つも無い環境（フォントを削除・最小構成にした等）では落ちずに None を返すこと
        let dir = tempdir().expect("tempdir を作れること");

        let found = find_japanese_font(&[dir.path().to_path_buf()]);

        assert_eq!(found, None);
    }

    #[test]
    fn find_japanese_font_finds_font_in_user_installed_directory() {
        // システム共通のフォントディレクトリ（1 つ目）には無く、
        // ユーザー単位でインストールされたフォントディレクトリ（2 つ目）にだけある場合も見つかること
        let system_dir = tempdir().expect("tempdir を作れること");
        let user_dir = tempdir().expect("tempdir を作れること");
        std::fs::write(user_dir.path().join("BIZ-UDGothicR.ttc"), b"dummy").unwrap();

        let found = find_japanese_font(&[
            system_dir.path().to_path_buf(),
            user_dir.path().to_path_buf(),
        ]);

        assert_eq!(
            found,
            Some(("BIZ UDGothic", user_dir.path().join("BIZ-UDGothicR.ttc"))),
        );
    }
}
