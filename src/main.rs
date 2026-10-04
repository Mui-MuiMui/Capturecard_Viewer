#![windows_subsystem = "windows"]
// テストの中だけ println! を許す。Cargo.toml の [lints.clippy] で
// print_stdout / print_stderr を warn にしてアプリ本体への再混入を止めているが、
// テストバイナリの標準出力は cargo が受け取るため cargo test -- --nocapture で読める。
// 計測結果の出力（src/video/convert.rs など）はそれを利用している。
// クレートルートに置いているのは、テスト対象のモジュール側を触らずに済ませるため
#![cfg_attr(test, allow(clippy::print_stdout))]

use eframe::egui;

mod app;
mod audio;
mod com;
mod config_path;
mod hotkey;
mod i18n;
mod keyboard_hook;
mod logging;
mod overlay;
mod platform;
mod recording;
mod renderer;
mod repaint;
mod screenshot;
mod screenshot_sound;
mod settings;
mod status;
mod ui;
mod update;
mod video;

use app::CaptureCardViewer;
use platform::{
    configure_japanese_font, is_position_visible, load_icon, monitor_work_areas,
    redirect_misdirected_close, window_size_or_default,
};
use settings::AppSettings;

fn main() -> Result<(), eframe::Error> {
    // 何よりも先にログ基盤を用意する。これ以降の失敗を記録できるようにするため。
    //
    // 失敗しても起動は続ける。ログが無いだけでアプリの機能には影響しない。
    // 失敗の理由を書き出す先はこの時点に存在しない（コンソールが無く、
    // ログファイルも開けていない）ので、戻り値はここで捨てるしかない。
    let _ = logging::init();

    // 設定から保存されたウィンドウの装飾・サイズ・位置を読み込む。
    // ここでは読み込み結果（LoadOutcome）を使わない。既定値の書き戻しと
    // 自動保存の可否は CaptureCardViewer::default 側だけで決めるため。
    //
    // 読み込みは default でもう一度走る。読めなかったファイルの error! と退避は
    // こちらで先に起き、退避できたときは default 側が「ファイルが無い」を読んで
    // Loaded になる（既定値で起動し、書き戻すのは同じ）。こちらで退避できなかった
    // ときは default 側でもう一度退避を試みる（error! はその分 2 回出る）。
    // そこでも退避できなければ BrokenFileLeftBehind で書き戻さず、そこで退避できれば
    // FellBackToDefaults で書き戻す。どちらも default 側の結果どおりに正しく判定される。
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

    // 前回最大化して終了していたら最大化で起動する。上の大きさと位置は
    // 最大化の前のもの（最大化中は記録しない）で、最大化を解除したときの戻り先になる。
    // 位置を捨てたときは OS の既定の配置のモニタで最大化される
    viewport_builder = viewport_builder.with_maximized(settings.ui.maximized);

    let mut options = eframe::NativeOptions {
        viewport: viewport_builder,
        // winit のイベント用のウィンドウへ届いた閉じる要求を、本来のウィンドウへ
        // 回す。最小化中の taskkill（/F なし）がそちらへ WM_CLOSE を送るため（#420、
        // docs/design/window.md の「閉じる要求の取り違え」）
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_msg_hook(redirect_misdirected_close);
        })),
        ..Default::default()
    };
    // 描画のバックエンド（wgpu / glow）を決める。**`run_native` より前に。**
    // イベントループは 1 回しか作れず、起動に失敗してから選び直せないため
    let renderer = renderer::configure(&mut options);
    let renderer_name = renderer.kind.name();

    let result = eframe::run_native(
        "Capturecard Viewer",
        options,
        Box::new(move |cc| {
            // イベントループを作り終えたここで外す。winit はイベントループを
            // 作るときにキーボードの Raw Input を登録し、それが残っていると
            // このアプリが前面にある間ホットキーのフックが呼ばれない（#238）
            if let Err(e) = keyboard_hook::stop_raw_keyboard_input() {
                log::warn!(
                    "キーボードの Raw Input を外せないので、前面にいる間はホットキーが効かないかもしれない: {e}"
                );
            }
            configure_japanese_font(&cc.egui_ctx);
            let mut app = CaptureCardViewer::default();
            // 統計 OSD にどちらで描いているかを出す。wgpu のアダプターはここまでに選ばれている
            app.set_renderer_label(renderer.label());
            // 画面の言語を設定と OS の表示言語から決める。**起動経路で 1 回だけ。**
            // `default()` の中では決めない。テストで作ったときにプロセス全体の
            // 言語を書き換えてしまうため（#256）。`default()` は文言を作らないので、
            // 作った直後に決めても最初の描画から設定の言語で出る
            app.apply_language();
            Ok(Box::new(app))
        }),
    );
    // 描画の初期化（wgpu のデバイスや surface、glow のコンテキスト）に失敗すると
    // ウィンドウが出ないまま終わる。理由をログに残す。wgpu で失敗したときは
    // `CAPTURECARD_VIEWER_RENDERER=glow` で起動できるかを試せる
    if let Err(e) = &result {
        log::error!(
            "ウィンドウを開けずに終了する（描画 {}）: {e}",
            renderer_name
        );
    }
    result
}
