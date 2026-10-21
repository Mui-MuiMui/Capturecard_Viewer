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
use video::GpuContext;

fn main() -> Result<(), eframe::Error> {
    // 何よりも先にログ基盤を用意する。これ以降の失敗を記録できるようにするため。
    //
    // 失敗しても起動は続ける。ログが無いだけでアプリの機能には影響しない。
    // 失敗の理由を書き出す先はこの時点に存在しない（コンソールが無く、
    // ログファイルも開けていない）ので、戻り値はここで捨てるしかない。
    let _ = logging::init();

    // 設定を読み込む。**読むのはここの 1 回だけ。** ウィンドウの装飾・サイズ・位置に
    // 使ったあと、設定と読み込み結果（LoadOutcome）をそのまま CaptureCardViewer::new へ
    // 渡す。既定値の書き戻しと自動保存の可否はそちらが LoadOutcome で決める。
    //
    // 以前はアプリを作るときにもう一度読んでいた。ここで退避したあとは「ファイルが
    // 無い」を読んで Loaded になり、読めなかったことを利用者へ知らせる材料
    // （理由と退避先）が残らなかった（Issue #505）
    let (settings, load_outcome) = AppSettings::load();
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
    // 描画のバックエンド（wgpu / glow、#492）を決める。**`run_native` より前に。**
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
            let mut app = CaptureCardViewer::new(settings, &load_outcome);
            // 統計 OSD にどちらで描いているかを出す。wgpu のアダプターはここまでに選ばれている
            app.set_renderer_label(renderer.label());
            // YUY2 → RGB を GPU で行う準備（#456）。描画バックエンドのコンテキスト（GL が
            // current なのも wgpu のデバイスが作られているのも）ここ。wgpu を先に見る（#492）
            let context = match (&cc.wgpu_render_state, &cc.gl) {
                (Some(render_state), _) => GpuContext::Wgpu(render_state),
                (None, Some(gl)) => GpuContext::Glow(gl),
                (None, None) => GpuContext::None,
            };
            app.init_gpu_yuy2(context);
            // 画面の言語を設定と OS の表示言語から決める。**起動経路で 1 回だけ。**
            // `new()` の中では決めない。テストで作ったときにプロセス全体の
            // 言語を書き換えてしまうため（#256）。`new()` は文言を作らないので、
            // 作った直後に決めても最初の描画から設定の言語で出る
            app.apply_language();
            // 設定ファイルを読めなかったことを知らせる（#505）。文言を作るので、
            // 画面の言語を決めたあとに呼ぶ
            app.report_settings_load(&load_outcome);
            Ok(Box::new(app))
        }),
    );
    // 描画の初期化（wgpu のデバイスや surface、glow のコンテキスト）に失敗すると
    // ウィンドウが出ないまま終わる。理由をログに残す。wgpu で失敗したときは
    // `CAPTURECARD_VIEWER_RENDERER=glow` で起動できるかを試せる
    if let Err(e) = &result {
        log::error!("ウィンドウを開けずに終了する（描画 {renderer_name}）: {e}");
    }
    result
}
