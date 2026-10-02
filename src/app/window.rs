//! ウィンドウそのものの操作。
//!
//! 最前面表示、タイトルバーの有無、装飾なしのときの端のドラッグによる
//! リサイズ、大きさのリセット、フルスクリーンの切り替え。起動時の位置と
//! 大きさの判定は `crate::platform`。

use super::CaptureCardViewer;
use crate::i18n::Text;
use crate::overlay::OverlayContent;
use crate::platform::DEFAULT_WINDOW_SIZE;
use eframe::egui;
use log::{info, trace, warn};
use std::time::{Duration, Instant};

/// フルスクリーンを切り替えたときに OSD を出しておく時間
const FULLSCREEN_OSD_DURATION: Duration = Duration::from_secs(1);

/// 装飾なしにしたときにドラッグ移動を自動で有効にした、と知らせる OSD の表示時間。
/// フルスクリーンの表示より長いのは、こちらが「設定を勝手に変えた」報告で、
/// 読ませる必要があるため
const DRAG_MOVE_GUARD_OSD_DURATION: Duration = Duration::from_secs(2);

/// 装飾なしのとき、ウィンドウの端を「リサイズを始める場所」と見なす幅。
/// 掴みやすさと、映像のドラッグ移動を邪魔しないことの兼ね合いで決めている
const RESIZE_BORDER: f32 = 8.0;

/// タイトルバーを消すときに「画面ドラッグ移動」を自動で有効にする必要があるかを返す。
///
/// 装飾なしではタイトルバーが無いため、ドラッグ移動も切れているとウィンドウを
/// 動かす手段が残らない。**その状態を作らせない。** 端のドラッグはリサイズに
/// 割り当ててあり、移動には使えない。
///
/// タイトルバーを戻すときは何もしない。ユーザーが自分で切ったドラッグ移動を
/// 勝手に戻すことになるため。
pub(super) fn needs_drag_move_guard(to_borderless: bool, enable_drag_move: bool) -> bool {
    to_borderless && !enable_drag_move
}

/// フルスクリーンでないときに、ウィンドウの状態から設定へ何を記録するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeometryRecord {
    /// 何も記録しない
    Skip,
    /// 最大化していることだけを記録し、位置と大きさは前の値を残す
    Maximized,
    /// 最大化していないことと、位置と大きさを記録する
    Normal,
}

/// ウィンドウの状態（OS からの報告。`None` は不明）から、何を記録するかを決める。
///
/// - **最小化中は何も記録しない。** 報告される位置は画面外の仮の値で、
///   最大化したまま最小化すると最大化も外れて報告される。記録すると、
///   最小化したまま終了したときにその前の状態を失う
/// - **最大化中は最大化したことだけを記録し、位置と大きさは残す。**
///   最大化中の矩形は作業領域いっぱい（Windows では枠の分だけ外の (-8, -8) から）
///   なので、記録すると次回は最大化でないウィンドウがその大きさで出る（#318）。
///   残した位置と大きさは、次回起動して最大化を解除したときの戻り先になる
/// - 状態が分からないときは通常のウィンドウとして扱う。最大化を扱う前と同じ動き
fn geometry_to_record(minimized: Option<bool>, maximized: Option<bool>) -> GeometryRecord {
    if minimized == Some(true) {
        GeometryRecord::Skip
    } else if maximized == Some(true) {
        GeometryRecord::Maximized
    } else {
        GeometryRecord::Normal
    }
}

/// ウィンドウ端の当たり判定。`pos` が `rect` の縁から `margin` 以内なら、
/// その縁に対応する `ResizeDirection` を返す。縁から離れていれば `None`。
///
/// 装飾なしのときにだけ使う。OS が描く枠の代わりに、自前で掴める帯を作る。
///
/// ウィンドウが `margin` の 2 倍より細いと左右（上下）の帯が重なる。
/// その場合は左と上を優先する。どちらを選んでも掴めることに変わりはなく、
/// 「どちらとも言えない」を返して掴めなくするほうが困るため。
fn resize_direction_at(
    pos: egui::Pos2,
    rect: egui::Rect,
    margin: f32,
) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection;

    // ポインタ位置は egui から来るが、NaN が紛れ込むと比較がすべて false になり
    // 判定が静かに壊れる。先に弾いておく
    if !pos.x.is_finite() || !pos.y.is_finite() || !margin.is_finite() || margin <= 0.0 {
        return None;
    }
    if !rect.contains(pos) {
        return None;
    }

    let left = pos.x - rect.left() <= margin;
    let right = !left && rect.right() - pos.x <= margin;
    let top = pos.y - rect.top() <= margin;
    let bottom = !top && rect.bottom() - pos.y <= margin;

    match (top, bottom, left, right) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (true, _, _, true) => Some(ResizeDirection::NorthEast),
        (true, ..) => Some(ResizeDirection::North),
        (_, true, true, _) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (_, true, ..) => Some(ResizeDirection::South),
        (_, _, true, _) => Some(ResizeDirection::West),
        (_, _, _, true) => Some(ResizeDirection::East),
        _ => None,
    }
}

/// リサイズの向きに対応するカーソル。装飾ありのウィンドウ枠と同じ見た目にする。
fn resize_cursor(direction: egui::ResizeDirection) -> egui::CursorIcon {
    use egui::{CursorIcon, ResizeDirection};

    match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
    }
}

impl CaptureCardViewer {
    /// ウィンドウの位置とサイズを設定へ記録してよいかを判定する。
    ///
    /// フルスクリーン中に報告される矩形は画面全体なので、記録すると
    /// 次回起動時に画面全体のサイズで復元されてしまう。
    ///
    /// `app_fullscreen` はアプリが持つフラグ、`viewport_fullscreen` は OS から
    /// 報告された状態（`None` は不明）。`ViewportCommand::Fullscreen` の効果は
    /// 次のフレーム以降に現れるため、解除した直後はアプリ側のフラグが false でも
    /// OS 側はまだフルスクリーンを報告している。**この 1 フレームで記録すると
    /// 画面全体の矩形を掴んでしまうので、両方がフルスクリーンでないときだけ
    /// 記録する。**
    pub(super) fn should_record_window_geometry(
        app_fullscreen: bool,
        viewport_fullscreen: Option<bool>,
    ) -> bool {
        !app_fullscreen && viewport_fullscreen != Some(true)
    }

    /// ウィンドウのサイズと位置が変わっていたら設定へ記録する。`update()` が毎フレーム呼ぶ。
    ///
    /// フルスクリーン中は画面全体の矩形しか取れないため記録しない
    /// （`should_record_window_geometry`）。こうすることで、フルスクリーンへ入る
    /// 直前のジオメトリが設定に残り、フルスクリーンのまま終了しても次回は
    /// ウィンドウ表示で復元される。最小化と最大化の扱いは `geometry_to_record`。
    ///
    /// **ここでは書き出さない。** ウィンドウのドラッグ中は毎フレーム値が変わるため、
    /// 変わるたびに保存すると最大 60 回/秒のディスク書き込みになる。
    pub(super) fn record_window_geometry(&mut self, viewport: &egui::ViewportInfo) {
        // 起動時に最大化を送ったフレームは、まだ最大化前の状態が報告される。
        // そのフレームだけは maximized を書き換えない。書き換えると、最大化が効く前に
        // 終了したとき（on_exit は必ず保存する）に最大化を失う。位置と大きさは
        // 戻り先そのものなので、このフレームも記録してよい
        let startup_maximize_pending = std::mem::take(&mut self.startup_maximize_pending);
        if !Self::should_record_window_geometry(self.is_fullscreen, viewport.fullscreen) {
            return;
        }
        let record = geometry_to_record(viewport.minimized, viewport.maximized);
        if record == GeometryRecord::Skip {
            return;
        }
        let maximized = record == GeometryRecord::Maximized;
        let current_size = viewport.inner_rect.map(|r| (r.width(), r.height()));
        let current_pos = viewport.outer_rect.map(|r| (r.left(), r.top()));
        let changed = match self.settings.lock() {
            Ok(mut settings) => {
                let mut changed = false;
                let keep_flag = startup_maximize_pending && !maximized;
                if !keep_flag && settings.ui.maximized != maximized {
                    settings.ui.maximized = maximized;
                    changed = true;
                }
                // 最大化中の矩形は残さない。前の位置と大きさが、次回起動して
                // 最大化を解除したときの戻り先になる
                if !maximized {
                    if let Some(size) = current_size {
                        if settings.ui.last_window_size != Some(size) {
                            settings.ui.last_window_size = Some(size);
                            changed = true;
                        }
                    }
                    if let Some(pos) = current_pos {
                        if settings.ui.last_window_pos != Some(pos) {
                            settings.ui.last_window_pos = Some(pos);
                            changed = true;
                        }
                    }
                }
                changed
            }
            Err(_) => {
                warn!("ウィンドウの位置・大きさの記録で settings のロックを取得できない");
                false
            }
        };
        if changed {
            self.mark_settings_dirty();
        }
    }

    /// 起動直後の最初のフレームで、最前面表示と最大化をウィンドウへ適用する。
    /// `apply_settings` で設定を取り込んだあとに呼ぶ。
    ///
    /// - ウィンドウレベルは `always_on_top` を取り込んだあとに送る。順序を
    ///   入れ替えると、既定値の false で 1 度適用されてしまう
    /// - **最大化は `ViewportBuilder::with_maximized` では効かない。** eframe 0.26 は
    ///   ウィンドウを作ったあとで `with_inner_size` / `with_position` の値を
    ///   ウィンドウへ設定し直し、winit はそれを受けて最大化を外す。大きさと位置は
    ///   最大化を解除したときの戻り先として要るので、ビルダーには残したまま、
    ///   ここで最大化を送る（#318、`docs/design/window.md`）
    pub(super) fn apply_startup_window_state(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));

        let maximized = match self.settings.lock() {
            Ok(settings) => settings.ui.maximized,
            Err(_) => {
                warn!("起動時の最大化の判定で settings のロックを取得できない");
                false
            }
        };
        if maximized {
            info!("前回は最大化して終了していたので最大化する");
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.startup_maximize_pending = true;
        }
    }

    /// 最前面表示を切り替える。
    ///
    /// 右クリックメニューのチェックボックスと同じことを行う。ウィンドウレベルの
    /// 適用、設定への反映、デバウンス保存までを 1 か所にまとめてある。
    pub(super) fn set_always_on_top(&mut self, ctx: &egui::Context, enabled: bool) {
        self.always_on_top = enabled;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if enabled {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));

        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.always_on_top = enabled;
        } else {
            warn!("最前面表示の設定の反映で settings のロックを取得できない");
        }
        info!(
            "最前面表示を{}にした",
            if enabled { "オン" } else { "オフ" }
        );
        self.mark_settings_dirty();
    }

    /// タイトルバーと枠の表示を切り替える。
    ///
    /// **装飾を外すときは「画面ドラッグ移動」も併せて見る。** どちらも無い状態に
    /// すると、ウィンドウを動かす手段が残らない。自動で有効にしたうえで、
    /// 設定を勝手に変えたことを OSD で伝える。
    pub(super) fn set_borderless(&mut self, ctx: &egui::Context, enabled: bool) {
        self.borderless = enabled;
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(!enabled));

        let mut enabled_drag_move = false;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.borderless = enabled;
            if needs_drag_move_guard(enabled, settings.ui.enable_drag_move) {
                settings.ui.enable_drag_move = true;
                enabled_drag_move = true;
            }
        } else {
            warn!("タイトルバーの切替で settings のロックを取得できない");
        }

        info!(
            "タイトルバーの表示を{}にした",
            if enabled { "オフ" } else { "オン" }
        );
        self.mark_settings_dirty();

        if enabled_drag_move {
            info!("ウィンドウを動かせなくなるため、画面ドラッグ移動を自動で有効にした");
            self.transient_overlay.show(
                OverlayContent::Text(Text::DragMoveEnabledNotice.get().to_string()),
                DRAG_MOVE_GUARD_OSD_DURATION,
                Instant::now(),
            );
        }
    }

    /// 装飾なしのときに、ウィンドウ端のドラッグでリサイズを始める。
    ///
    /// 戻り値は「ポインタがいまリサイズ用の帯にいるか」。**`true` の間、
    /// 呼び出し側は映像のドラッグによるウィンドウ移動を行わない。** 端を掴んだ
    /// つもりでウィンドウごと動いてしまうため。
    ///
    /// メニューやダイアログが開いている間は何もしない。ウィンドウ端に重なった
    /// ボタンを押そうとしてリサイズが始まるのを防ぐ。
    pub(super) fn handle_borderless_resize(&self, ctx: &egui::Context) -> bool {
        if !self.borderless || self.is_fullscreen {
            return false;
        }
        if self.show_context_menu || self.show_settings || self.show_hotkey_dialog {
            return false;
        }

        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
            return false;
        };
        let Some(direction) = resize_direction_at(pos, ctx.content_rect(), RESIZE_BORDER) else {
            return false;
        };

        ctx.set_cursor_icon(resize_cursor(direction));

        if ctx.input(|i| i.pointer.primary_pressed()) {
            trace!("装飾なしのウィンドウ端をつかんだ: {:?}", direction);
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }

        true
    }

    /// ウィンドウの大きさを既定に戻す。
    ///
    /// 装飾なしでは端の帯でしかリサイズできず、小さくしすぎると掴む場所を
    /// 見失う。そこからの復帰手段として右クリックメニューに置いてある。
    pub(super) fn reset_window_size(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            DEFAULT_WINDOW_SIZE.0,
            DEFAULT_WINDOW_SIZE.1,
        )));
        info!(
            "ウィンドウサイズを既定（{}x{}）に戻した",
            DEFAULT_WINDOW_SIZE.0, DEFAULT_WINDOW_SIZE.1
        );
        // 設定への記録は update() のウィンドウ監視が拾う。ここで書くと
        // OS が要求どおりの大きさにできなかった場合に実際とずれる
    }

    pub(super) fn toggle_fullscreen(&mut self, ctx: &egui::Context, to_full: bool) {
        use eframe::egui::ViewportCommand;

        if to_full {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
            self.is_fullscreen = true;
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.is_fullscreen = false;
        }

        let text = if self.is_fullscreen {
            Text::FullscreenOn
        } else {
            Text::FullscreenOff
        };
        self.transient_overlay.show(
            OverlayContent::Text(text.get().to_string()),
            FULLSCREEN_OSD_DURATION,
            Instant::now(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;

    #[test]
    fn should_record_window_geometry_windowed_returns_true() {
        // 通常のウィンドウ表示中は記録する
        assert!(CaptureCardViewer::should_record_window_geometry(
            false,
            Some(false)
        ));
    }

    #[test]
    fn should_record_window_geometry_fullscreen_returns_false() {
        // フルスクリーン中の矩形は画面全体。記録すると次回起動時に
        // 画面全体サイズで復元されてしまう
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true,
            Some(true)
        ));
    }

    #[test]
    fn should_record_window_geometry_just_entered_fullscreen_returns_false() {
        // フルスクリーンへ入った直後。アプリ側のフラグだけが先に立ち、
        // OS 側はまだウィンドウ表示を報告している
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true,
            Some(false)
        ));
    }

    #[test]
    fn should_record_window_geometry_just_left_fullscreen_returns_false() {
        // フルスクリーンを解除した直後。アプリ側のフラグだけが先に降り、
        // OS 側はまだ画面全体の矩形を報告している
        assert!(!CaptureCardViewer::should_record_window_geometry(
            false,
            Some(true)
        ));
    }

    #[test]
    fn should_record_window_geometry_unknown_viewport_state_follows_app_flag() {
        // OS から状態が取れない場合はアプリ側のフラグに従う
        assert!(CaptureCardViewer::should_record_window_geometry(
            false, None
        ));
        assert!(!CaptureCardViewer::should_record_window_geometry(
            true, None
        ));
    }

    #[test]
    fn geometry_to_record_normal_window_records_geometry() {
        assert_eq!(
            geometry_to_record(Some(false), Some(false)),
            GeometryRecord::Normal
        );
    }

    #[test]
    fn geometry_to_record_maximized_keeps_previous_geometry() {
        // 最大化中の矩形は作業領域いっぱい。記録すると次回は最大化でない
        // ウィンドウがその大きさで出る（#318）
        assert_eq!(
            geometry_to_record(Some(false), Some(true)),
            GeometryRecord::Maximized
        );
    }

    #[test]
    fn geometry_to_record_minimized_records_nothing() {
        // 最大化したまま最小化すると、最大化も外れて報告される。
        // 記録すると最小化のまま終了したときに最大化を失う
        assert_eq!(
            geometry_to_record(Some(true), Some(false)),
            GeometryRecord::Skip
        );
        assert_eq!(
            geometry_to_record(Some(true), Some(true)),
            GeometryRecord::Skip
        );
    }

    #[test]
    fn geometry_to_record_unknown_state_is_treated_as_normal() {
        // 状態を報告しない環境では、最大化を扱う前と同じく位置と大きさを記録する
        assert_eq!(geometry_to_record(None, None), GeometryRecord::Normal);
    }

    #[test]
    fn needs_drag_move_guard_borderless_without_drag_move_returns_true() {
        // タイトルバーもドラッグ移動も無い状態は作らせない
        assert!(needs_drag_move_guard(true, false));
    }

    #[test]
    fn needs_drag_move_guard_borderless_with_drag_move_returns_false() {
        // 既に動かせるなら何も変えない
        assert!(!needs_drag_move_guard(true, true));
    }

    #[test]
    fn needs_drag_move_guard_decorated_window_never_guards() {
        // タイトルバーがあれば掴んで動かせるので、ユーザーが切った
        // ドラッグ移動を勝手に戻さない
        assert!(!needs_drag_move_guard(false, false));
        assert!(!needs_drag_move_guard(false, true));
    }

    /// リサイズの当たり判定に使う、原点が (0, 0) でない矩形。
    /// 左上が原点だと `left()` と 0 の取り違えに気付けない
    fn resize_test_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 50.0), Vec2::new(400.0, 300.0))
    }

    #[test]
    fn resize_direction_at_center_returns_none() {
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(rect.center(), rect, RESIZE_BORDER),
            None
        );
    }

    #[test]
    fn resize_direction_at_each_edge_returns_that_edge() {
        use egui::ResizeDirection;
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(102.0, 200.0), rect, 8.0),
            Some(ResizeDirection::West)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(498.0, 200.0), rect, 8.0),
            Some(ResizeDirection::East)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 52.0), rect, 8.0),
            Some(ResizeDirection::North)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 348.0), rect, 8.0),
            Some(ResizeDirection::South)
        );
    }

    #[test]
    fn resize_direction_at_each_corner_returns_the_diagonal() {
        use egui::ResizeDirection;
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(101.0, 51.0), rect, 8.0),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(499.0, 51.0), rect, 8.0),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(101.0, 349.0), rect, 8.0),
            Some(ResizeDirection::SouthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(499.0, 349.0), rect, 8.0),
            Some(ResizeDirection::SouthEast)
        );
    }

    #[test]
    fn resize_direction_at_exactly_on_the_margin_still_resizes() {
        use egui::ResizeDirection;
        // 境界。帯の内側に含める側へ倒している
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(108.0, 200.0), rect, 8.0),
            Some(ResizeDirection::West)
        );
        // 帯の 1 つ外は掴めない
        assert_eq!(
            resize_direction_at(egui::pos2(108.1, 200.0), rect, 8.0),
            None
        );
    }

    #[test]
    fn resize_direction_at_outside_the_window_returns_none() {
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(99.0, 200.0), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(300.0, 400.0), rect, 8.0),
            None
        );
    }

    #[test]
    fn resize_direction_at_tiny_window_prefers_the_top_left() {
        use egui::ResizeDirection;
        // 帯の 2 倍より小さいウィンドウでは左右（上下）の判定が重なる。
        // どちらとも言えないからと None を返すと、縮めすぎたウィンドウを
        // 二度と広げられなくなる
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(10.0, 10.0));

        assert_eq!(
            resize_direction_at(egui::pos2(5.0, 5.0), rect, 8.0),
            Some(ResizeDirection::NorthWest)
        );
    }

    #[test]
    fn resize_direction_at_non_finite_input_returns_none() {
        // NaN は比較がすべて false になり、判定が静かに壊れる
        let rect = resize_test_rect();

        assert_eq!(
            resize_direction_at(egui::pos2(f32::NAN, 200.0), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(102.0, f32::INFINITY), rect, 8.0),
            None
        );
        assert_eq!(
            resize_direction_at(egui::pos2(102.0, 200.0), rect, f32::NAN),
            None
        );
    }

    #[test]
    fn resize_direction_at_zero_margin_returns_none() {
        // 帯の幅が 0 なら掴める場所は無い
        let rect = resize_test_rect();

        assert_eq!(resize_direction_at(rect.min, rect, 0.0), None);
        assert_eq!(resize_direction_at(rect.min, rect, -4.0), None);
    }

    #[test]
    fn resize_cursor_matches_the_direction() {
        use egui::{CursorIcon, ResizeDirection};

        assert_eq!(
            resize_cursor(ResizeDirection::North),
            CursorIcon::ResizeVertical
        );
        assert_eq!(
            resize_cursor(ResizeDirection::South),
            CursorIcon::ResizeVertical
        );
        assert_eq!(
            resize_cursor(ResizeDirection::East),
            CursorIcon::ResizeHorizontal
        );
        assert_eq!(
            resize_cursor(ResizeDirection::West),
            CursorIcon::ResizeHorizontal
        );
        assert_eq!(
            resize_cursor(ResizeDirection::NorthEast),
            CursorIcon::ResizeNeSw
        );
        assert_eq!(
            resize_cursor(ResizeDirection::SouthWest),
            CursorIcon::ResizeNeSw
        );
        assert_eq!(
            resize_cursor(ResizeDirection::NorthWest),
            CursorIcon::ResizeNwSe
        );
        assert_eq!(
            resize_cursor(ResizeDirection::SouthEast),
            CursorIcon::ResizeNwSe
        );
    }
}
