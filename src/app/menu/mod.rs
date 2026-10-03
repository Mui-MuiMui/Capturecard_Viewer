//! 右クリックメニュー。
//!
//! ウィンドウの高さが足りるときは平らな一覧（`items::menu_items_flat`）、
//! 足りないときはサブメニューへ折りたたんだ構成（`items::menu_items_collapsed`）を
//! 描く。どちらを使うかはメニューを開いた瞬間に決めて、開いている間は変えない。
//!
//! **描画はアプリ状態を書き換えない。** 設定ダイアログ（`docs/design/settings-dialog.md`）と
//! 同じ流儀で、`items` の描画関数は読み取り専用のスナップショット `MenuView` を
//! 借りて、起きたことを `MenuAction` の列として返す。状態を変えるのは、この
//! ファイルにある `CaptureCardViewer::handle_menu_action` だけ。

mod items;

use self::items::{menu_items_collapsed, menu_items_flat, FlatEntry, MenuAction, FLAT_MENU};
use super::CaptureCardViewer;
use crate::settings;
use eframe::egui;
use log::{info, warn};

/// 右クリックメニューの幅。項目名が折り返さない程度に取ってある。
/// サブメニューにも同じ値を使う（egui の既定は 150px で、
/// 「アスペクト比を維持」のような項目名が折り返してしまう）
const CONTEXT_MENU_WIDTH: f32 = 240.0;

/// 右クリックメニューの大きさを決めるときに、画面の端へ空けておく余白。
/// 端にぴったり貼り付くと、収まっているのかはみ出しているのかが見分けにくい
const CONTEXT_MENU_SCREEN_MARGIN: f32 = 24.0;

/// 右クリックメニューの中身に使える幅と、高さの上限を決める。
///
/// 引数は egui の画面（＝ウィンドウ）の大きさと、ポップアップの枠が
/// 左右・上下で食う幅。戻り値は `(幅, 高さの上限)` で、どちらも枠の内側の値。
///
/// **`Area::constrain_to` は位置を画面内へ戻すだけで、確定した矩形の幅も
/// 高さも縮めない。** 画面より大きいメニューはそのままはみ出すので、
/// 幅はここで縮め、高さは呼び出し側が `ScrollArea` の上限に使う。
///
/// 画面が極端に小さい場合は 0 まで落とす。**「これ以下にはしない」という
/// 下限を置かない。** 置くと、下限を割る画面では必ずはみ出す側へ倒れ、
/// 画面に収めるという目的と逆になる。
fn context_menu_size_limits(screen_size: egui::Vec2, frame_margin: egui::Vec2) -> (f32, f32) {
    let available = screen_size - frame_margin - egui::Vec2::splat(CONTEXT_MENU_SCREEN_MARGIN);
    (
        CONTEXT_MENU_WIDTH.min(available.x).max(0.0),
        available.y.max(0.0),
    )
}

/// 右クリックメニューの見せ方。
///
/// `Flat` は PR #146 より前と同じ、切り替え系も含めた 1 階層の一覧。
/// `Collapsed` は PR #146 のサブメニュー構成（「表示」「ウィンドウ」）。
/// 実機確認でサブメニューは操作性が落ちるという指摘を受けたため、
/// **ウィンドウが十分に高いときは `Flat` を使う。** 収まらないときだけ
/// `Collapsed` へ落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuLayout {
    Flat,
    Collapsed,
}

/// `ui.separator()` が確保する高さ（線の上下の余白を含む）。egui 0.36 では
/// `Style::separator_style` が固定の 6.0 を返し、`Spacing` からは引けない
const EGUI_SEPARATOR_SPACING: f32 = 6.0;

/// 平らな一覧の高さを、描画前に見積もる。
///
/// 実測はしない。実測しようとすると「一度サブメニュー構成で描いてから
/// 高さを比べる」といった余分な描画が要る。
///
/// **行数とセパレータの数は `items::FLAT_MENU` の表から数える。** 描画も
/// 同じ表を順に描くので、項目を足しても見積もりがずれない（#448 の前は
/// 行数を定数で持っていて、項目が増えるたびに直す必要があった）。
///
/// 1 行の高さは、チェックボックスやボタンのクリック領域（`interact_size.y`）と、
/// 文字の行の高さ（`text_row_height`）にボタンの上下の余白を足したもの、の
/// 大きいほうに行間（`item_spacing.y`）を足す。**文字の行の高さを入れないと、
/// 日本語フォント（Meiryo など）で 1 行が `interact_size.y` より高くなり、
/// 見積もりが実測を下回る**（#448 の実測で 392 に対して 449）。
/// セパレータは egui が線の上下に取る `EGUI_SEPARATOR_SPACING` に行間を足したもの
/// （#448 の前は `item_spacing.y * 2.0 + 1.0` で、1 本あたり 2 小さかった）。
///
/// 見積もりは実測より大きめに出る（音量ラベルの行もボタンの高さで数える）。
/// 小さめに出ると、収まらない `Flat` を選んでスクロールになる。
///
/// `has_presets` はプリセットが 1 つ以上あるかどうか。あれば「プリセット」の
/// 行が 1 つ増える（`items::preset_submenu`）
fn estimate_flat_menu_height(
    spacing: &egui::style::Spacing,
    text_row_height: f32,
    has_presets: bool,
) -> f32 {
    let row_count: usize = FLAT_MENU.iter().map(|entry| entry.rows(has_presets)).sum();
    let separator_count = FLAT_MENU
        .iter()
        .filter(|entry| **entry == FlatEntry::Separator)
        .count();
    let widget_height = spacing
        .interact_size
        .y
        .max(text_row_height + 2.0 * spacing.button_padding.y);
    let row_height = widget_height + spacing.item_spacing.y;
    let separator_height = EGUI_SEPARATOR_SPACING + spacing.item_spacing.y;
    row_count as f32 * row_height + separator_count as f32 * separator_height
}

/// 本文（`TextStyle::Body`）の 1 行の高さ。見積もり（`estimate_flat_menu_height`）に渡す
fn body_row_height(ctx: &egui::Context) -> f32 {
    let font_id = egui::TextStyle::Body.resolve(&ctx.global_style());
    ctx.fonts_mut(|fonts| fonts.row_height(&font_id))
}

/// 右クリックメニューを平らな一覧にするかサブメニューへ折りたたむかを決める。
///
/// 平らな一覧の見積もり高さ（`flat_height`）が使える高さ（`available_height`、
/// `context_menu_size_limits` の高さ側）に収まるなら `Flat`。
///
/// **境界（ちょうど収まる）は `Flat` に倒す。** `flat_height` は見積もりで
/// あり、実測より大きめに出ることはあっても小さめに出ることは想定していない
/// ため、同点なら操作性の良い平らな一覧を優先してよい。
fn context_menu_layout(available_height: f32, flat_height: f32) -> MenuLayout {
    if flat_height <= available_height {
        MenuLayout::Flat
    } else {
        MenuLayout::Collapsed
    }
}

/// 右クリックメニューの録画の項目の状態。`app::recording` が作る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordingMenuState {
    /// 録画していない。「録画を開始」
    Idle,
    /// 録画中。「録画を停止（00:12:34）」。中身は開始からの経過時間
    Recording(std::time::Duration),
    /// 停止を頼んで `Finalize` を待っている。押せない
    Finishing,
}

/// 右クリックメニューの「リプレイを保存（直近 N 秒）」の状態（#438）。`app::recording` が作る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SaveReplayMenuState {
    /// さかのぼる長さの設定（秒）。項目名に出す
    pub(super) seconds: u32,
    /// 保存できない理由。あれば灰色にしてホバーに出す
    pub(super) block: Option<crate::recording::SaveReplayBlock>,
}

/// 右クリックメニューの描画に要る状態のスナップショット。
///
/// **描画へ渡すのはこれだけで、`CaptureCardViewer` も共有設定の
/// `Arc<Mutex<AppSettings>>` も渡さない。** 設定ダイアログの
/// `SettingsDialogView` と同じ考え方（`docs/design/settings-dialog.md`）。
///
/// 設定から採る項目（`enable_drag_move` / `auto_reconnect` / プリセット）は
/// `CaptureCardViewer::menu_view` が**ロックを 1 度だけ取って**まとめて読む。
/// 描画の途中で読むと、1 フレームの中でロックを何度も取り直すことになる。
struct MenuView {
    /// 録画の項目の状態
    recording: RecordingMenuState,
    /// 「リプレイを保存」の項目の状態
    save_replay: SaveReplayMenuState,
    volume: f32,
    muted: bool,
    maintain_aspect_ratio: bool,
    always_on_top: bool,
    is_fullscreen: bool,
    borderless: bool,
    enable_drag_move: bool,
    show_stats_overlay: bool,
    auto_reconnect: bool,
    /// プリセットの名前。空ならサブメニューごと出さない
    preset_names: Vec<String>,
    /// 選択中のプリセット名。手で値を変えたあとは `None`
    active_preset: Option<String>,
}

/// 右クリックメニューの `Area` を描く。中身は `items` が描き、起きた操作は
/// `actions` へ、外側クリックの判定に使う矩形は `menu_rects` へ積む。
///
/// **画面に収まらなくなるのを 3 段で防いでいる。** まず `constrain_to` で
/// メニューごと画面内へ押し戻し、幅は画面より広くならないように縮め、
/// それでも足りない高さは `ScrollArea` でスクロールできるようにする。
///
/// **`Area` は前のフレームで測った大きさを、中身に使える大きさの上限にする。**
/// 初めて描くフレームはそれが無いので `default_size` を使い、egui の既定は
/// 高さ 400（`Spacing::default_area_size`）。一度 400 で測ると、中身が
/// それより高くても `ScrollArea` が 400 に収まって測り直されないので、
/// 画面の高さに関わらずスクロールになっていた（#448）。そこで
/// - 初めの高さを画面の高さにする（`default_height`）
/// - 開くたびに測り直す（`sizing_pass`。`open_context_menu` で立てる）。
///   折りたたみの有無は開くたびに変わりうるので、前に開いたときの大きさを
///   使い回すと、低いほうの大きさに縛られる
fn draw_context_menu(
    ctx: &egui::Context,
    pos: egui::Pos2,
    layout: MenuLayout,
    view: &MenuView,
    sizing_pass: bool,
    menu_rects: &mut Vec<egui::Rect>,
    actions: &mut Vec<MenuAction>,
) {
    // ポップアップの枠が食う分を引いてから、中身に使える大きさを決める
    let frame = egui::Frame::popup(&ctx.global_style());
    let screen = ctx.content_rect();
    let (width, max_height) = context_menu_size_limits(screen.size(), frame.inner_margin.sum());

    egui::Area::new(egui::Id::new("context_menu"))
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        // 画面の下端や右端の近くで開いたときに、メニューごと画面内へ押し戻す
        .constrain_to(screen)
        .default_height(screen.height())
        .sizing_pass(sizing_pass)
        .show(ctx, |outer_ui| {
            // 固定幅でポップアップコンテンツをラップ
            frame.show(outer_ui, |ui| {
                ui.set_min_width(width);
                ui.set_max_width(width);

                // 折りたたみ判定は開いた時点で確定済み（open_context_menu）。
                // ここでは保険として ScrollArea と constrain_to をどちらの
                // レイアウトでも残す。見積もりが外れて平らな一覧が実際には
                // 収まらなかった場合の逃げ道になる
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    // 横は縮めない。縮むと項目の幅が中身ごとに変わって揃わない
                    .auto_shrink([false, true])
                    .show(ui, |ui| match layout {
                        MenuLayout::Flat => {
                            menu_items_flat(ui, view, width, menu_rects, actions);
                        }
                        MenuLayout::Collapsed => {
                            menu_items_collapsed(ui, view, width, menu_rects, actions);
                        }
                    });
            });
            // 構築後、エリアの完全な矩形をキャプチャ
            menu_rects.push(outer_ui.min_rect());
        });
}

impl CaptureCardViewer {
    /// 右クリックメニューを開く。位置と、平らな一覧にするかサブメニューへ
    /// 折りたたむかをこの時点で確定させる。
    ///
    /// **判定は開いた瞬間の 1 回だけ行い、開いている間は毎フレーム描画に
    /// 合わせて計算し直さない。** ウィンドウをリサイズしながらメニューを
    /// 出しっぱなしにできる egui の仕様上、毎フレーム判定すると境界付近で
    /// 開閉のたびにレイアウトが入れ替わってちらつく。
    pub(super) fn open_context_menu(&mut self, ctx: &egui::Context, pos: egui::Pos2) {
        self.show_context_menu = true;
        self.context_menu_pos = pos;

        let frame = egui::Frame::popup(&ctx.global_style());
        let (_, max_height) =
            context_menu_size_limits(ctx.content_rect().size(), frame.inner_margin.sum());
        // プリセットが 1 つでもあれば、平らな一覧に「プリセット」の行が
        // 1 行増える（preset_submenu、詳細は estimate_flat_menu_height）
        let has_presets = match self.settings.lock() {
            Ok(settings) => !settings.presets.is_empty(),
            Err(_) => {
                warn!("右クリックメニューの高さ見積もりで settings のロックを取得できない");
                false
            }
        };
        let flat_height = estimate_flat_menu_height(
            &ctx.global_style().spacing,
            body_row_height(ctx),
            has_presets,
        );
        self.context_menu_layout = context_menu_layout(max_height, flat_height);
        // 次に描くときに大きさを測り直させる（draw_context_menu の sizing_pass）
        self.context_menu_needs_sizing = true;
    }

    /// 右クリックメニューを描き、起きた操作を処理する。
    ///
    /// 中身は `items` が描く。ここは置き場所と閉じ方、そして描画が返した
    /// `MenuAction` の処理だけを持つ。描き方（画面に収める工夫）は
    /// `draw_context_menu`。
    pub(super) fn show_context_menu(&mut self, ctx: &egui::Context) {
        // メニュー本体と、開いているサブメニューの矩形。外側クリックの判定に使う。
        // **サブメニューは別の Area に描かれ本体の矩形に含まれない。** ここへ
        // 足しておかないと、サブメニューを押しただけでメニュー全体が閉じる
        let mut menu_rects: Vec<egui::Rect> = Vec::new();
        // 描画中に起きた操作。**描き終えてから受け取った順に処理する。**
        // 描画の途中で状態を変えると、同じフレームの後続の項目が
        // 変更前と変更後の混ざった状態を見てしまう
        let mut actions: Vec<MenuAction> = Vec::new();

        // 描画へ渡すのは読み取り専用のスナップショットだけ
        let view = self.menu_view();
        let sizing_pass = std::mem::take(&mut self.context_menu_needs_sizing);
        draw_context_menu(
            ctx,
            self.context_menu_pos,
            self.context_menu_layout,
            &view,
            sizing_pass,
            &mut menu_rects,
            &mut actions,
        );

        let mut close_menu = actions.iter().any(MenuAction::closes_menu);
        for action in actions {
            self.handle_menu_action(ctx, action);
        }

        // 外側をクリック、またはEscapeキー押下時のみ閉じる
        ctx.input(|i| {
            if i.pointer.primary_clicked() {
                if let Some(pos) = i.pointer.latest_pos() {
                    if !menu_rects.iter().any(|rect| rect.contains(pos)) {
                        close_menu = true;
                    }
                }
            }
            if i.key_pressed(egui::Key::Escape) {
                close_menu = true;
            }
        });

        if close_menu {
            self.show_context_menu = false;
        }
    }

    /// 右クリックメニューの描画へ渡すスナップショットを作る。
    ///
    /// **設定のロックはここで 1 度だけ取る。** 取れなかったときは、切り替えを
    /// 妨げない側（有効）へ倒す。プリセットは名前が読めない以上出しようが
    /// ないので、空のまま（サブメニューごと出ない）にする。
    fn menu_view(&self) -> MenuView {
        let mut enable_drag_move = true;
        let mut auto_reconnect = true;
        let mut preset_names = Vec::new();
        let mut active_preset = None;
        let mut replay_seconds = settings::RecordingSettings::default().replay_seconds;
        match self.settings.lock() {
            Ok(settings) => {
                enable_drag_move = settings.ui.enable_drag_move;
                auto_reconnect = settings.video.auto_reconnect;
                replay_seconds = settings
                    .recording
                    .replay_seconds
                    .clamp(settings::MIN_REPLAY_SECONDS, settings::MAX_REPLAY_SECONDS);
                preset_names = settings
                    .presets
                    .iter()
                    .map(|preset| preset.name.clone())
                    .collect();
                active_preset = settings::resolved_active_preset(&settings).map(str::to_string);
            }
            Err(_) => {
                warn!("右クリックメニューの描画で settings のロックを取得できない");
            }
        }

        MenuView {
            recording: self.recording_menu_state(),
            save_replay: self.save_replay_menu_state(replay_seconds),
            volume: self.volume,
            muted: self.muted,
            maintain_aspect_ratio: self.maintain_aspect_ratio,
            always_on_top: self.always_on_top,
            is_fullscreen: self.is_fullscreen,
            borderless: self.borderless,
            enable_drag_move,
            show_stats_overlay: self.show_stats_overlay,
            auto_reconnect,
            preset_names,
            active_preset,
        }
    }

    /// 右クリックメニューで起きた操作を 1 つ処理する。
    ///
    /// **実処理は既存のメソッドへ委ねる。** ホットキーも同じメソッドを呼ぶ
    /// 決まりなので（`docs/design/hotkeys.md`）、ここに独自の処理を書くと
    /// 経路によって設定の保存や OSD の有無が変わってしまう。
    fn handle_menu_action(&mut self, ctx: &egui::Context, action: MenuAction) {
        match action {
            MenuAction::SetVolume(volume) => self.set_volume_from_ui(volume),
            MenuAction::SetMuted(muted) => self.set_muted_from_ui(muted),
            MenuAction::SetMaintainAspectRatio(enabled) => {
                // アスペクト比の設定を反映する（書き出しはデバウンス）
                self.maintain_aspect_ratio = enabled;
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.maintain_aspect_ratio = enabled;
                } else {
                    warn!("アスペクト比の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }
            // ウィンドウレベルの適用と保存は set_always_on_top が持つ
            MenuAction::SetAlwaysOnTop(enabled) => self.set_always_on_top(ctx, enabled),
            MenuAction::SetFullscreen(to_full) => self.toggle_fullscreen(ctx, to_full),
            MenuAction::SetBorderless(enabled) => self.set_borderless(ctx, enabled),
            MenuAction::SetEnableDragMove(enabled) => {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.enable_drag_move = enabled;
                } else {
                    warn!("画面ドラッグ移動の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }
            MenuAction::SetShowStatsOverlay(enabled) => {
                self.show_stats_overlay = enabled;
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.show_stats_overlay = enabled;
                } else {
                    warn!("情報表示の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }
            MenuAction::SetAutoReconnect(enabled) => {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.video.auto_reconnect = enabled;
                    info!(
                        "デバイスの自動再接続を{}にした",
                        if enabled { "有効" } else { "無効" }
                    );
                } else {
                    warn!("デバイスの自動再接続の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
                // **デバイスワーカーへも伝える。** 切断を検出したときに開き直すかの
                // 判断はワーカー側が持っているので、次の 2 秒ごとの再適用を待つと
                // その間に起きた切断が切り替え前の設定で扱われる
                self.apply_settings(false);
            }
            MenuAction::ResetWindowSize => self.reset_window_size(ctx),
            MenuAction::ReconnectDevices => self.reconnect_devices(),
            MenuAction::ToggleRecording => self.toggle_recording(),
            MenuAction::SaveReplay => self.save_replay(),
            MenuAction::ApplyPreset(name) => self.apply_preset_by_name(&name),
            MenuAction::OpenSettings => self.show_settings = true,
            MenuAction::Quit => {
                info!("右クリックメニューから終了する");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_menu_size_limits_wide_screen_keeps_the_fixed_width() {
        // 1280x720 の内側。幅は既定のまま、高さだけ画面から決まる
        let (width, max_height) =
            context_menu_size_limits(egui::vec2(1280.0, 720.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 240.0);
        assert_eq!(max_height, 684.0);
    }

    #[test]
    fn context_menu_size_limits_narrow_screen_shrinks_the_width() {
        // 幅 200px のウィンドウ。既定の 240px のままだと右側が画面外へ出る
        let (width, _) = context_menu_size_limits(egui::vec2(200.0, 720.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 164.0);
    }

    #[test]
    fn context_menu_size_limits_tiny_screen_clamps_to_zero() {
        // 枠と余白だけで画面を使い切る大きさ。負にはしない
        let (width, max_height) =
            context_menu_size_limits(egui::vec2(20.0, 30.0), egui::vec2(12.0, 12.0));

        assert_eq!(width, 0.0);
        assert_eq!(max_height, 0.0);
    }

    #[test]
    fn context_menu_layout_fits_within_available_height_is_flat() {
        assert_eq!(context_menu_layout(500.0, 480.0), MenuLayout::Flat);
    }

    #[test]
    fn context_menu_layout_exact_fit_is_flat() {
        // ちょうど収まる境界は、はみ出す側ではなく平らな一覧を優先する
        assert_eq!(context_menu_layout(480.0, 480.0), MenuLayout::Flat);
    }

    #[test]
    fn context_menu_layout_overflow_by_a_hair_is_collapsed() {
        assert_eq!(context_menu_layout(480.0, 480.1), MenuLayout::Collapsed);
    }

    #[test]
    fn context_menu_layout_zero_available_height_is_collapsed() {
        // 高さが取れない画面では、平らな一覧は絶対に収まらない
        assert_eq!(context_menu_layout(0.0, 1.0), MenuLayout::Collapsed);
    }

    #[test]
    fn context_menu_layout_huge_available_height_is_flat() {
        assert_eq!(context_menu_layout(f32::MAX, 480.0), MenuLayout::Flat);
    }

    /// 既定のフォントの本文の 1 行の高さ（egui の既定は 12.5pt）
    const DEFAULT_TEXT_ROW: f32 = 15.0;

    #[test]
    fn estimate_flat_menu_height_with_default_style_is_positive() {
        let spacing = egui::Style::default().spacing;
        assert!(estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false) > 0.0);
    }

    #[test]
    fn estimate_flat_menu_height_grows_with_row_height() {
        // 行が高くなるほど見積もりも大きくなること。逆行すると、
        // フォントサイズを上げたときに折りたたみ判定が正しく働かなくなる
        let mut spacing = egui::Style::default().spacing;
        let base = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);

        spacing.interact_size.y *= 2.0;
        let taller = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);

        assert!(taller > base);
    }

    #[test]
    fn estimate_flat_menu_height_grows_with_text_row_height() {
        // 日本語フォントで文字の行が interact_size.y より高くなったら、
        // 見積もりもそれに合わせて伸びる（#448）
        let spacing = egui::Style::default().spacing;
        let base = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);
        let taller = estimate_flat_menu_height(&spacing, 24.0, false);

        assert!(taller > base);
    }

    #[test]
    fn estimate_flat_menu_height_counts_rows_from_the_flat_menu_table() {
        // 行数とセパレータの数は items::FLAT_MENU から数える。いまの並びは
        // 16 行（音量の 3 行を含む）とセパレータ 5 本
        let spacing = egui::Style::default().spacing;
        let row = spacing.interact_size.y + spacing.item_spacing.y;
        let separator = EGUI_SEPARATOR_SPACING + spacing.item_spacing.y;

        let estimate = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);

        assert!((estimate - (16.0 * row + 5.0 * separator)).abs() < 1e-3);
    }

    #[test]
    fn estimate_flat_menu_height_with_presets_adds_one_row() {
        // プリセットがあると「プリセット」の行が 1 つ増える。ここが
        // ずれると、プリセットがある状態でだけ折りたたみ判定を誤る
        let spacing = egui::Style::default().spacing;
        let without_presets = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);
        let with_presets = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, true);

        let row_height = spacing.interact_size.y + spacing.item_spacing.y;
        assert!((with_presets - without_presets - row_height).abs() < 1e-3);
    }

    #[test]
    fn context_menu_layout_presets_row_tips_the_boundary_to_collapsed() {
        // プリセットが無ければちょうど収まる高さでも、プリセットの分だけ
        // 見積もりが増えると収まらなくなり、Collapsed へ倒れる
        let spacing = egui::Style::default().spacing;
        let flat_without_presets = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, false);
        let flat_with_presets = estimate_flat_menu_height(&spacing, DEFAULT_TEXT_ROW, true);

        assert_eq!(
            context_menu_layout(flat_without_presets, flat_without_presets),
            MenuLayout::Flat
        );
        assert_eq!(
            context_menu_layout(flat_without_presets, flat_with_presets),
            MenuLayout::Collapsed
        );
    }

    // ---- 描いたときの大きさ（egui_kittest、#448） ----

    /// 既定の設定と同じ見た目のスナップショット。プリセットは 1 つ
    fn sample_view() -> MenuView {
        MenuView {
            recording: RecordingMenuState::Idle,
            save_replay: SaveReplayMenuState {
                seconds: 30,
                block: None,
            },
            volume: 100.0,
            muted: false,
            maintain_aspect_ratio: true,
            always_on_top: false,
            is_fullscreen: false,
            borderless: false,
            enable_drag_move: true,
            show_stats_overlay: false,
            auto_reconnect: true,
            preset_names: vec!["配信用".to_string()],
            active_preset: None,
        }
    }

    /// 右クリックメニューを描く `Harness` の状態
    struct MenuArea {
        /// 本番と同じく日本語フォントを使うか（見つからなければ既定のまま）
        japanese_font: bool,
        fonts_ready: bool,
        pos: egui::Pos2,
        layout: MenuLayout,
        /// 次のフレームで測り直すか。`open_context_menu` が立てるのと同じ
        sizing_pass: bool,
        /// 最後に描いたメニュー本体の矩形（枠を含む）
        menu_rect: Option<egui::Rect>,
        /// 開いたときの見積もり（`open_context_menu` と同じ計算）
        estimate: f32,
        /// 平らな一覧を Area なしで描いたときの実際の高さ
        flat_actual: f32,
    }

    impl MenuArea {
        fn new(pos: egui::Pos2, layout: MenuLayout, japanese_font: bool) -> Self {
            Self {
                japanese_font,
                fonts_ready: false,
                pos,
                layout,
                sizing_pass: true,
                menu_rect: None,
                estimate: 0.0,
                flat_actual: 0.0,
            }
        }

        fn draw(&mut self, ui: &mut egui::Ui) {
            let ctx = ui.ctx().clone();
            if !self.fonts_ready {
                if self.japanese_font {
                    crate::platform::configure_japanese_font(&ctx);
                }
                // フォントは次のフレームから効く。このフレームは測らない
                self.fonts_ready = true;
                return;
            }
            let view = sample_view();
            self.estimate =
                estimate_flat_menu_height(&ctx.global_style().spacing, body_row_height(&ctx), true);
            // 平らな一覧そのものの高さ。スクロールも Area も挟まずに測る
            let mut rects = Vec::new();
            let mut actions = Vec::new();
            let flat = ui
                .scope(|ui| {
                    ui.set_width(CONTEXT_MENU_WIDTH);
                    ui.set_invisible();
                    menu_items_flat(ui, &view, CONTEXT_MENU_WIDTH, &mut rects, &mut actions);
                })
                .response
                .rect;
            self.flat_actual = flat.height();

            let mut rects = Vec::new();
            let sizing_pass = std::mem::take(&mut self.sizing_pass);
            draw_context_menu(
                &ctx,
                self.pos,
                self.layout,
                &view,
                sizing_pass,
                &mut rects,
                &mut actions,
            );
            // 本体の矩形は最後に積まれる
            self.menu_rect = rects.last().copied();
        }
    }

    fn menu_harness(size: egui::Vec2, state: MenuArea) -> egui_kittest::Harness<'static, MenuArea> {
        egui_kittest::Harness::builder()
            .with_size(size)
            .build_ui_state(|ui, state: &mut MenuArea| state.draw(ui), state)
    }

    /// 枠（`Frame::popup` の上下の余白）を含めた平らな一覧の高さ
    fn flat_with_frame(harness: &egui_kittest::Harness<'static, MenuArea>) -> f32 {
        let frame = egui::Frame::popup(&harness.ctx.global_style());
        harness.state().flat_actual + frame.inner_margin.sum().y
    }

    #[test]
    fn draw_context_menu_flat_is_not_capped_at_the_default_area_height() {
        // #448: egui の Area の既定の高さ 400 に縛られて、画面が足りていても
        // スクロールになっていた。1280x720 なら平らな一覧が丸ごと出る
        for japanese_font in [false, true] {
            let mut harness = menu_harness(
                egui::vec2(1280.0, 720.0),
                MenuArea::new(egui::pos2(100.0, 100.0), MenuLayout::Flat, japanese_font),
            );
            harness.run_steps(5);

            let rect = harness.state().menu_rect.expect("描いたはず");
            let needed = flat_with_frame(&harness);
            assert!(
                rect.height() >= needed - 0.5,
                "日本語フォント {japanese_font}: メニュー {} < 中身 {needed}",
                rect.height()
            );
        }
    }

    #[test]
    fn draw_context_menu_near_the_bottom_moves_up_instead_of_scrolling() {
        // 下端の近くで開いても、縮めずに上へずらして画面内へ収める
        let mut harness = menu_harness(
            egui::vec2(1280.0, 720.0),
            MenuArea::new(egui::pos2(100.0, 700.0), MenuLayout::Flat, true),
        );
        harness.run_steps(5);

        let rect = harness.state().menu_rect.expect("描いたはず");
        assert!(rect.height() >= flat_with_frame(&harness) - 0.5);
        assert!(rect.bottom() <= 720.0 + 0.5, "{rect:?}");
        assert!(rect.top() < 700.0, "{rect:?}");
    }

    #[test]
    fn draw_context_menu_reopened_as_flat_measures_again() {
        // 前に折りたたんだ構成（低い）で開いたあとに平らな一覧で開き直しても、
        // 前の大きさに縛られない（open_context_menu が sizing_pass を立てる）
        let mut harness = menu_harness(
            egui::vec2(1280.0, 720.0),
            MenuArea::new(egui::pos2(100.0, 100.0), MenuLayout::Collapsed, false),
        );
        harness.run_steps(5);
        let collapsed = harness.state().menu_rect.expect("描いたはず");

        harness.state_mut().layout = MenuLayout::Flat;
        harness.state_mut().sizing_pass = true;
        harness.run_steps(5);
        let flat = harness.state().menu_rect.expect("描いたはず");

        assert!(flat.height() > collapsed.height());
        assert!(flat.height() >= flat_with_frame(&harness) - 0.5);
    }

    #[test]
    fn estimate_flat_menu_height_is_not_below_the_drawn_height() {
        // 見積もりが実測を下回ると、収まらない Flat を選んでスクロールになる。
        // 本番と同じ日本語フォントでも確かめる（#448 の前は 392 に対して 449）
        for japanese_font in [false, true] {
            let mut harness = menu_harness(
                egui::vec2(1280.0, 720.0),
                MenuArea::new(egui::pos2(100.0, 100.0), MenuLayout::Flat, japanese_font),
            );
            harness.run_steps(3);

            let state = harness.state();
            assert!(
                state.estimate >= state.flat_actual,
                "日本語フォント {japanese_font}: 見積もり {} < 実測 {}",
                state.estimate,
                state.flat_actual
            );
        }
    }
}
