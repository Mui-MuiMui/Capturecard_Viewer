#![windows_subsystem = "windows"]
// テストの中だけ println! を許す。Cargo.toml の [lints.clippy] で
// print_stdout / print_stderr を warn にしてアプリ本体への再混入を止めているが、
// テストバイナリの標準出力は cargo が受け取るため cargo test -- --nocapture で読める。
// 計測結果の出力（src/video.rs）はそれを利用している。
// クレートルートに置いているのは、テスト対象のモジュール側を触らずに済ませるため
#![cfg_attr(test, allow(clippy::print_stdout))]

use eframe::egui;

mod app;
mod audio;
mod hotkey;
mod logging;
mod overlay;
mod platform;
mod repaint;
mod screenshot;
mod settings;
mod status;
mod ui;
mod video;

use app::CaptureCardViewer;
use platform::{
    configure_japanese_font, is_position_visible, load_icon, monitor_work_areas,
    window_size_or_default,
};
use settings::AppSettings;

fn main() -> Result<(), eframe::Error> {
    // 何よりも先にログ基盤を用意する。これ以降の失敗を記録できるようにするため。
    //
    // 失敗しても起動は続ける。ログが無いだけでアプリの機能には影響しない。
    // 失敗の理由を書き出す先はこの時点に存在しない（コンソールが無く、
    // ログファイルも開けていない）ので、戻り値はここで捨てるしかない。
    let _ = logging::init();

    // 設定から保存されたウィンドウサイズと位置を読み込む。
    // ここでは読み込み結果を使わない。既定値の書き戻しは
    // CaptureCardViewer::default 側だけで行うため。
    let (settings, _) = AppSettings::load();
    let mut viewport_builder = egui::ViewportBuilder::default().with_icon(load_icon());

    // タイトルバーと枠の有無は最初のウィンドウ生成時に決める。
    // 生成後に ViewportCommand::Decorations で戻すと、装飾ありのウィンドウが
    // 一瞬見えてから消える
    viewport_builder = viewport_builder.with_decorations(!settings.ui.borderless);

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
