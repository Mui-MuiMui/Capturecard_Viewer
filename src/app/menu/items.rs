//! 右クリックメニューの項目の描画。
//!
//! **ここにある関数はアプリ状態を書き換えない。** 読み取り専用の
//! `MenuView` を借りて、起きたことを `MenuAction` として `actions` へ積む
//! だけで、処理は `menu::CaptureCardViewer::handle_menu_action` が行う
//! （設定ダイアログと同じ流儀。`docs/design/settings-dialog.md`）。
//!
//! 平らな一覧（`menu_items_flat`）とサブメニュー構成（`menu_items_collapsed`）は
//! **同じ項目の関数を並び替えて呼ぶ**だけにしてある。項目ごとの文言・有効条件・
//! 返すアクションを 1 か所に集め、片方だけ直す事故を起こさないため。

use super::MenuView;
use crate::i18n::{self, Text};
use crate::settings::{MAX_VOLUME, MIN_VOLUME};
use eframe::egui;

/// 外側クリックの判定でサブメニューの矩形に足す余白。
/// `Ui::min_rect` はポップアップの枠の内側なので、枠の上を押しただけで
/// メニュー全体が閉じるのを防ぐ
const CONTEXT_MENU_HIT_MARGIN: f32 = 8.0;

/// 右クリックメニューで起きた操作。
///
/// 描画関数はこれを列に積むだけで、実際の処理は
/// `CaptureCardViewer::handle_menu_action` が行う。**平らな一覧と
/// サブメニュー構成のどちらから来たかは区別しない。** 見せ方が違うだけで
/// 起きることは同じなので、処理を 2 つ持たせない。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MenuAction {
    /// 音量スライダーを動かした
    SetVolume(f32),
    /// ミュートのチェックを変えた
    SetMuted(bool),
    /// 「アスペクト比を維持」を変えた
    SetMaintainAspectRatio(bool),
    /// 「最前面表示」を変えた
    SetAlwaysOnTop(bool),
    /// 「フルスクリーン表示」を変えた
    SetFullscreen(bool),
    /// 「タイトルバーを隠す」を変えた
    SetBorderless(bool),
    /// 「画面ドラッグ移動」を変えた
    SetEnableDragMove(bool),
    /// 「情報表示」を変えた
    SetShowStatsOverlay(bool),
    /// 「デバイスの自動再接続」を変えた
    SetAutoReconnect(bool),
    /// 「ウィンドウサイズをリセット」を押した
    ResetWindowSize,
    /// 「デバイス再接続」を押した
    ReconnectDevices,
    /// 「プリセット」から 1 つ選んだ
    ApplyPreset(String),
    /// 「詳細設定...」を押した
    OpenSettings,
    /// 「終了」を押した
    Quit,
}

impl MenuAction {
    /// この操作でメニューを閉じるか。
    ///
    /// **閉じるのは、結果がメニューの外（ウィンドウ全体や映像）に出るもの
    /// だけ。** 切り替え系は閉じない。続けて別の項目を切り替えることが
    /// あるため、1 つ押すたびに開き直させない。
    pub(super) fn closes_menu(&self) -> bool {
        match self {
            MenuAction::ResetWindowSize
            | MenuAction::ReconnectDevices
            | MenuAction::ApplyPreset(_)
            | MenuAction::OpenSettings
            | MenuAction::Quit => true,
            MenuAction::SetVolume(_)
            | MenuAction::SetMuted(_)
            | MenuAction::SetMaintainAspectRatio(_)
            | MenuAction::SetAlwaysOnTop(_)
            | MenuAction::SetFullscreen(_)
            | MenuAction::SetBorderless(_)
            | MenuAction::SetEnableDragMove(_)
            | MenuAction::SetShowStatsOverlay(_)
            | MenuAction::SetAutoReconnect(_) => false,
        }
    }
}

/// 音量スライダーとミュート。どちらのレイアウトでも先頭に置く。
fn volume_items(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    ui.label(i18n::volume_percent(view.volume as i32));

    // スライダーが書き換えるのはスナップショットの複製。実際の反映は
    // `handle_menu_action` が `set_volume_from_ui` で行う（書き出しはデバウンス）
    let mut volume = view.volume;
    let volume_response =
        ui.add(egui::Slider::new(&mut volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"));
    if volume_response.changed() {
        actions.push(MenuAction::SetVolume(volume));
    }

    // ミュートはスライダーのすぐ下に置く。音量 0% にする代わりの
    // 操作なので、離すと探されない
    let mut muted = view.muted;
    if ui.checkbox(&mut muted, Text::Mute.get()).changed() {
        actions.push(MenuAction::SetMuted(muted));
    }
}

/// 「アスペクト比を維持」のチェックボックス。
fn aspect_ratio_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.maintain_aspect_ratio;
    if ui
        .checkbox(&mut value, Text::MaintainAspectRatio.get())
        .changed()
    {
        actions.push(MenuAction::SetMaintainAspectRatio(value));
    }
}

/// 「最前面表示」のチェックボックス。
fn always_on_top_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.always_on_top;
    if ui
        .checkbox(&mut value, Text::MenuAlwaysOnTop.get())
        .changed()
    {
        actions.push(MenuAction::SetAlwaysOnTop(value));
    }
}

/// 「フルスクリーン表示」のチェックボックス。
///
/// ダブルクリックとホットキーでも切り替えられるが、装飾を消していると
/// ここが唯一目に見える切り替え手段になるので、折りたたむ構成でも
/// サブメニューへ入れずに直下へ置く。
fn fullscreen_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.is_fullscreen;
    if ui
        .checkbox(&mut value, Text::MenuFullscreen.get())
        .changed()
    {
        actions.push(MenuAction::SetFullscreen(value));
    }
}

/// 「タイトルバーを隠す」のチェックボックス。
///
/// フルスクリーン中は OS が元から装飾を外しているので触らせない。
/// ここで切り替えても見た目は変わらず、フルスクリーンを抜けた
/// ときに初めて効くので、操作と結果が結びつかない。
fn borderless_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.borderless;
    let response = ui
        .add_enabled(
            !view.is_fullscreen,
            egui::Checkbox::new(&mut value, Text::MenuHideTitleBar.get()),
        )
        .on_hover_text(Text::MenuHideTitleBarHint.get())
        .on_disabled_hover_text(Text::MenuHideTitleBarDisabledHint.get());
    if response.changed() {
        actions.push(MenuAction::SetBorderless(value));
    }
}

/// 「画面ドラッグ移動」のチェックボックス。
///
/// 装飾なしの間は切らせない。切ると動かす手段が残らない。
fn drag_move_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.enable_drag_move;
    let response = ui
        .add_enabled(
            !view.borderless,
            egui::Checkbox::new(&mut value, Text::MenuDragMove.get()),
        )
        .on_disabled_hover_text(Text::MenuDragMoveDisabledHint.get());
    if response.changed() {
        actions.push(MenuAction::SetEnableDragMove(value));
    }
}

/// 「情報表示」（統計オーバーレイ）のチェックボックス。
fn stats_overlay_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.show_stats_overlay;
    if ui.checkbox(&mut value, Text::MenuStats.get()).changed() {
        actions.push(MenuAction::SetShowStatsOverlay(value));
    }
}

/// 「デバイスの自動再接続」のチェックボックス。
///
/// 設定は `VideoSettings` に持たせているが、音声ストリームの
/// エラーからの復帰にも効く（利用者から見て 1 つの機能なので
/// スイッチも 1 つにしてある）。
fn auto_reconnect_item(ui: &mut egui::Ui, view: &MenuView, actions: &mut Vec<MenuAction>) {
    let mut value = view.auto_reconnect;
    let response = ui
        .checkbox(&mut value, Text::MenuAutoReconnect.get())
        .on_hover_text(Text::MenuAutoReconnectHint.get());
    if response.changed() {
        actions.push(MenuAction::SetAutoReconnect(value));
    }
}

/// 「ウィンドウサイズをリセット」のボタン。
///
/// 装飾なしで小さくしすぎて端の帯を掴めなくなったときの復帰手段。
///
/// 戻り値は押されたかどうか。**サブメニューの中から呼ぶ側は、`true` の
/// ときに `ui.close_menu()` を呼ぶこと。** 呼ばないと開いた状態が egui 側に
/// 残り、次に右クリックしたときにサブメニューが開いたまま出る。
fn reset_window_size_item(
    ui: &mut egui::Ui,
    view: &MenuView,
    actions: &mut Vec<MenuAction>,
) -> bool {
    let clicked = ui
        .add_enabled(
            !view.is_fullscreen,
            egui::Button::new(Text::MenuResetWindowSize.get()),
        )
        .on_disabled_hover_text(Text::MenuResetWindowSizeDisabledHint.get())
        .clicked();
    if clicked {
        actions.push(MenuAction::ResetWindowSize);
    }
    clicked
}

/// 「デバイス再接続」のボタン。
///
/// 映像が出なくなったときの復帰手段なので、どちらのレイアウトでも
/// サブメニューへ入れずに直下へ置く。
fn reconnect_item(ui: &mut egui::Ui, actions: &mut Vec<MenuAction>) {
    if ui.button(Text::ActionReconnectDevices.get()).clicked() {
        actions.push(MenuAction::ReconnectDevices);
    }
}

/// 「詳細設定...」のボタン。
fn settings_item(ui: &mut egui::Ui, actions: &mut Vec<MenuAction>) {
    if ui.button(Text::MenuAdvancedSettings.get()).clicked() {
        actions.push(MenuAction::OpenSettings);
    }
}

/// 「終了」のボタン。
///
/// 装飾なしでは × が無いので、ここが閉じる手段になる。
/// 押すと `on_exit` が走り、保留中の設定も書き出される。
fn quit_item(ui: &mut egui::Ui, actions: &mut Vec<MenuAction>) {
    if ui.button(Text::MenuQuit.get()).clicked() {
        actions.push(MenuAction::Quit);
    }
}

/// 右クリックメニューの「表示」サブメニュー。
///
/// 映像の見え方に関わる切り替えを集めてある。**どれを押してもメニューは
/// 閉じない。** 続けて切り替えることがあるため。
fn view_submenu(
    ui: &mut egui::Ui,
    view: &MenuView,
    width: f32,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    ui.menu_button(Text::MenuViewSubmenu.get(), |ui| {
        // サブメニューの幅は egui の既定が 150px で、項目名が折り返す。
        // 本体と同じ幅に揃える（狭いウィンドウでは本体ごと縮んでいる）
        ui.set_max_width(width);

        aspect_ratio_item(ui, view, actions);
        always_on_top_item(ui, view, actions);
        stats_overlay_item(ui, view, actions);
        borderless_item(ui, view, actions);

        menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
    });
}

/// 右クリックメニューの「ウィンドウ」サブメニュー。
///
/// ウィンドウの動かし方と大きさに関わる項目を集めてある。
/// **「ウィンドウサイズをリセット」は押したらメニューを閉じる。**
/// 結果がウィンドウ全体に出るので、メニューが被ったままだと確かめられない。
fn window_submenu(
    ui: &mut egui::Ui,
    view: &MenuView,
    width: f32,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    ui.menu_button(Text::MenuWindowSubmenu.get(), |ui| {
        ui.set_max_width(width);

        drag_move_item(ui, view, actions);
        if reset_window_size_item(ui, view, actions) {
            // サブメニュー側も閉じる。本体を閉じるのは
            // `MenuAction::closes_menu` の判定が行う
            ui.close_menu();
        }

        menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
    });
}

/// 右クリックメニューの「プリセット」サブメニュー。
///
/// **プリセットが 1 つも無いときは項目ごと出さない。** 押しても何も
/// 起きない空のサブメニューを見せるより、無いことが分かるほうがよい。
/// 作る場所は設定ダイアログの「その他」タブなので、ここには誘導を置かない。
///
/// 選ぶとメニューを閉じる。デバイスを開き直すことがあり、結果は映像に
/// 出るため、メニューが被ったままでは確かめられない。
fn preset_submenu(
    ui: &mut egui::Ui,
    view: &MenuView,
    width: f32,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    if view.preset_names.is_empty() {
        return;
    }

    ui.menu_button(Text::MenuPresetSubmenu.get(), |ui| {
        ui.set_max_width(width);

        for name in &view.preset_names {
            // 選択中のものにチェックを付ける。手で値を変えたあとは
            // どれも選択中にならない（resolved_active_preset が None）
            let is_active = view.active_preset.as_deref() == Some(name.as_str());
            if ui.selectable_label(is_active, name).clicked() {
                actions.push(MenuAction::ApplyPreset(name.clone()));
                ui.close_menu();
            }
        }

        menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
    });
}

/// 右クリックメニューの項目を、折りたたまずに平らな一覧として描く。
///
/// **項目順は PR #146 より前と同じ。** サブメニューへ分けたところ、
/// 隠れて操作性が落ちるという実機確認の指摘を受けたため、ウィンドウの
/// 高さが十分なときは使い慣れたこちらの並びへ戻す。動作そのものは
/// `menu_items_collapsed` と同じで、見せ方（階層に分けるかどうか）だけが違う。
///
/// **「プリセット」だけは折りたたみの有無に関わらずサブメニューのまま
/// 出す。** プリセットは固定の切り替えではなく可変長の一覧なので、平らな
/// 一覧に展開すると項目数がプリセットの数だけ増減し、高さの見積もり
/// （`estimate_flat_menu_height` は固定の行数を前提にしている）と
/// 食い違う。そのため `menu_rects` を受け取る（`preset_submenu` が開く
/// サブメニューの矩形を外側クリックの判定に含めるため）。
pub(super) fn menu_items_flat(
    ui: &mut egui::Ui,
    view: &MenuView,
    width: f32,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    volume_items(ui, view, actions);

    ui.separator();

    aspect_ratio_item(ui, view, actions);
    always_on_top_item(ui, view, actions);
    fullscreen_item(ui, view, actions);
    borderless_item(ui, view, actions);
    drag_move_item(ui, view, actions);
    stats_overlay_item(ui, view, actions);
    auto_reconnect_item(ui, view, actions);

    ui.separator();

    reset_window_size_item(ui, view, actions);
    reconnect_item(ui, actions);

    // プリセットは映像と音声の取り込み方の切替なので、
    // 「デバイス再接続」のすぐそばに置く（collapsed 側と同じ理由）
    preset_submenu(ui, view, width, menu_rects, actions);

    ui.separator();
    settings_item(ui, actions);

    ui.separator();
    quit_item(ui, actions);
}

/// 右クリックメニューの項目を、サブメニューへ折りたたんで描く。
///
/// 項目が増えて縦に伸びると低い解像度で下端が画面外へ出るため、切り替え系は
/// 「表示」「ウィンドウ」のサブメニューへ分けてある。**直下に残すのは、映像が
/// 出ないときの復帰手段（デバイス再接続）と、装飾を消しているときに他の手段が
/// 無い操作（フルスクリーン、終了）。** 探し回らずに押せることを優先する。
///
/// サブメニューの中身を足したときは、閉じるボタンに `ui.close_menu()` を
/// 忘れないこと。呼ばないと開いた状態が egui 側に残り、次に右クリックした
/// ときにサブメニューが開いたまま出る。
pub(super) fn menu_items_collapsed(
    ui: &mut egui::Ui,
    view: &MenuView,
    width: f32,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    volume_items(ui, view, actions);

    ui.separator();

    fullscreen_item(ui, view, actions);

    // サブメニューのボタンは既定だと文字の幅しか取らず、上下のチェック
    // ボックスと縁が揃わない。幅いっぱいに広げて 1 つの並びに見せる
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        view_submenu(ui, view, width, menu_rects, actions);
        window_submenu(ui, view, width, menu_rects, actions);
        // プリセットは映像と音声の取り込み方の切替なので、本来は下の
        // 「デバイス再接続」に近い。それでも他のサブメニューと並べて
        // あるのは、justified の並びから外すとボタンの幅が揃わないため
        preset_submenu(ui, view, width, menu_rects, actions);
    });

    ui.separator();
    reconnect_item(ui, actions);
    // 上の「デバイス再接続」と紛らわしい項目なので、離さずに隣へ置いてある
    auto_reconnect_item(ui, view, actions);

    ui.separator();
    settings_item(ui, actions);

    ui.separator();
    quit_item(ui, actions);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_action_one_shot_items_close_the_menu() {
        // 結果がメニューの外に出る操作は、押したらメニューを閉じる
        for action in [
            MenuAction::ResetWindowSize,
            MenuAction::ReconnectDevices,
            MenuAction::ApplyPreset("既定".to_string()),
            MenuAction::OpenSettings,
            MenuAction::Quit,
        ] {
            assert!(action.closes_menu(), "閉じるべき操作: {:?}", action);
        }
    }

    #[test]
    fn menu_action_toggles_keep_the_menu_open() {
        // 切り替え系は閉じない。続けて別の項目を触ることがある
        for action in [
            MenuAction::SetVolume(80.0),
            MenuAction::SetMuted(true),
            MenuAction::SetMaintainAspectRatio(false),
            MenuAction::SetAlwaysOnTop(true),
            MenuAction::SetFullscreen(true),
            MenuAction::SetBorderless(true),
            MenuAction::SetEnableDragMove(false),
            MenuAction::SetShowStatsOverlay(true),
            MenuAction::SetAutoReconnect(false),
        ] {
            assert!(!action.closes_menu(), "閉じてはいけない操作: {:?}", action);
        }
    }
}
