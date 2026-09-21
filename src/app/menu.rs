//! 右クリックメニュー。
//!
//! ウィンドウの高さが足りるときは平らな一覧（`context_menu_items_flat`）、
//! 足りないときはサブメニューへ折りたたんだ構成（`context_menu_items`）を
//! 描く。どちらを使うかはメニューを開いた瞬間に決めて、開いている間は変えない。

use super::CaptureCardViewer;
use crate::settings::{self, MAX_VOLUME, MIN_VOLUME};
use eframe::egui;
use log::{info, warn};

/// 右クリックメニューの幅。項目名が折り返さない程度に取ってある。
/// サブメニューにも同じ値を使う（egui の既定は 150px で、
/// 「アスペクト比を維持」のような項目名が折り返してしまう）
const CONTEXT_MENU_WIDTH: f32 = 240.0;

/// 右クリックメニューの大きさを決めるときに、画面の端へ空けておく余白。
/// 端にぴったり貼り付くと、収まっているのかはみ出しているのかが見分けにくい
const CONTEXT_MENU_SCREEN_MARGIN: f32 = 24.0;

/// 外側クリックの判定でサブメニューの矩形に足す余白。
/// `Ui::min_rect` はポップアップの枠の内側なので、枠の上を押しただけで
/// メニュー全体が閉じるのを防ぐ
const CONTEXT_MENU_HIT_MARGIN: f32 = 8.0;

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

/// 平らな一覧（PR #146 以前の構成）の見積もり行数とセパレータ数。
///
/// 内訳は `CaptureCardViewer::context_menu_items_flat` の並びと対応させて
/// あるので、あちらの項目を増減したときはこちらも直すこと。
///
/// 行: 音量ラベル / 音量スライダー / ミュート / アスペクト比を維持 /
/// 最前面表示 / フルスクリーン表示 / タイトルバーを隠す / 画面ドラッグ移動 /
/// 情報表示 / デバイスの自動再接続 / ウィンドウサイズをリセット /
/// デバイス再接続 / 詳細設定... / 終了 の 14 行。**プリセットが 1 つでも
/// あれば「プリセット」の行が 1 つ増える。** プリセットの有無は起動後にいつ
/// 変わるか分からないため固定の行数には含めず、`estimate_flat_menu_height`
/// の引数で足す。
/// セパレータ: ミュートの下 / 自動再接続の下（ウィンドウサイズをリセットの上）/
/// デバイス再接続の下 / 詳細設定の下 の 4 本。プリセットの行はセパレータを
/// 増やさない（デバイス再接続の直後に挟まるだけ）。
const FLAT_MENU_ROW_COUNT: usize = 14;
const FLAT_MENU_SEPARATOR_COUNT: usize = 4;

/// 平らな一覧の高さを、描画前に見積もる。
///
/// 実測はしない。実測しようとすると「一度サブメニュー構成で描いてから
/// 高さを比べる」といった余分な描画が要る。行の高さは
/// `interact_size.y + item_spacing.y`（チェックボックスやボタンの
/// クリック領域＋行間）、セパレータは egui の実装に合わせて
/// `item_spacing.y * 2.0 + 1.0`（線の上下の余白＋線そのものの太さ）で
/// 見積もる。多少のずれは境界の判定に影響するだけで、実際に描画した
/// ときにスクロールへ倒れる分には安全側（`context_menu_layout` 側で
/// 境界を `Flat` 寄りにしてあるのはこのため）。
///
/// `has_presets` はプリセットが 1 つ以上あるかどうか。あれば行数に 1 を
/// 足す（`preset_submenu` が平らな一覧にも「プリセット」の行を描くため）。
/// ここを固定 14 行のままにすると、プリセットがある状態でちょうど境界の
/// 高さのとき、実際には収まらない `Flat` を選んでしまう
fn estimate_flat_menu_height(spacing: &egui::style::Spacing, has_presets: bool) -> f32 {
    let row_count = FLAT_MENU_ROW_COUNT + usize::from(has_presets);
    let row_height = spacing.interact_size.y + spacing.item_spacing.y;
    let separator_height = spacing.item_spacing.y * 2.0 + 1.0;
    row_count as f32 * row_height + FLAT_MENU_SEPARATOR_COUNT as f32 * separator_height
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

impl CaptureCardViewer {
    /// 右クリックメニューを描く。
    ///
    /// 中身は `context_menu_items` が描く。ここは置き場所と閉じ方だけを持つ。
    ///
    /// **画面に収まらなくなるのを 3 段で防いでいる。** まず `constrain_to` で
    /// メニューごと画面内へ押し戻し、幅は画面より広くならないように縮め、
    /// それでも足りない高さは `ScrollArea` でスクロールできるようにする。
    /// 項目を足すときはどれも壊さないこと。
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

        let frame = egui::Frame::popup(&ctx.style());
        let (_, max_height) =
            context_menu_size_limits(ctx.screen_rect().size(), frame.inner_margin.sum());
        // プリセットが 1 つでもあれば、平らな一覧に「プリセット」の行が
        // 1 行増える（preset_submenu、詳細は estimate_flat_menu_height）
        let has_presets = match self.settings.lock() {
            Ok(settings) => !settings.presets.is_empty(),
            Err(_) => {
                warn!("右クリックメニューの高さ見積もりで settings のロックを取得できない");
                false
            }
        };
        let flat_height = estimate_flat_menu_height(&ctx.style().spacing, has_presets);
        self.context_menu_layout = context_menu_layout(max_height, flat_height);
    }

    pub(super) fn show_context_menu(&mut self, ctx: &egui::Context) {
        let mut close_menu = false;
        // メニュー本体と、開いているサブメニューの矩形。外側クリックの判定に使う。
        // **サブメニューは別の Area に描かれ本体の矩形に含まれない。** ここへ
        // 足しておかないと、サブメニューを押しただけでメニュー全体が閉じる
        let mut menu_rects: Vec<egui::Rect> = Vec::new();

        // ポップアップの枠が食う分を引いてから、中身に使える大きさを決める
        let frame = egui::Frame::popup(&ctx.style());
        let (width, max_height) =
            context_menu_size_limits(ctx.screen_rect().size(), frame.inner_margin.sum());

        egui::Area::new("context_menu")
            .fixed_pos(self.context_menu_pos)
            .order(egui::Order::Foreground)
            // 画面の下端や右端の近くで開いたときに、メニューごと画面内へ押し戻す
            .constrain_to(ctx.screen_rect())
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
                        .show(ui, |ui| match self.context_menu_layout {
                            MenuLayout::Flat => {
                                self.context_menu_items_flat(
                                    ctx,
                                    ui,
                                    width,
                                    &mut menu_rects,
                                    &mut close_menu,
                                );
                            }
                            MenuLayout::Collapsed => {
                                self.context_menu_items(
                                    ctx,
                                    ui,
                                    width,
                                    &mut menu_rects,
                                    &mut close_menu,
                                );
                            }
                        });
                });
                // 構築後、エリアの完全な矩形をキャプチャ
                menu_rects.push(outer_ui.min_rect());
            });

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

    /// 右クリックメニューの項目を、折りたたまずに平らな一覧として描く。
    ///
    /// **項目順は PR #146 より前と同じ。** サブメニューへ分けたところ、
    /// 隠れて操作性が落ちるという実機確認の指摘を受けたため、ウィンドウの
    /// 高さが十分なときは使い慣れたこちらの並びへ戻す。動作そのものは
    /// `context_menu_items` / `view_submenu` / `window_submenu` と同じで、
    /// 見せ方（階層に分けるかどうか）だけが違う。
    ///
    /// **「プリセット」だけは折りたたみの有無に関わらずサブメニューのまま
    /// 出す。** プリセットは固定の切り替えではなく可変長の一覧なので、平らな
    /// 一覧に展開すると項目数がプリセットの数だけ増減し、高さの見積もり
    /// （`estimate_flat_menu_height` は固定の行数を前提にしている）と
    /// 食い違う。そのため `menu_rects` を受け取る（`preset_submenu` が開く
    /// サブメニューの矩形を外側クリックの判定に含めるため）。
    fn context_menu_items_flat(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.label(format!("音量: {}%", self.volume as i32));
        let volume_response =
            ui.add(egui::Slider::new(&mut self.volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"));
        if volume_response.changed() {
            self.set_volume_from_ui(self.volume);
        }

        let mut muted = self.muted;
        if ui.checkbox(&mut muted, "ミュート").changed() {
            self.set_muted_from_ui(muted);
        }

        ui.separator();

        let aspect_response = ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");
        if aspect_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.maintain_aspect_ratio = self.maintain_aspect_ratio;
            } else {
                warn!("アスペクト比の設定の反映で settings のロックを取得できない");
            }
            self.mark_settings_dirty();
        }

        let always_on_top_response = ui.checkbox(&mut self.always_on_top, "最前面表示");
        if always_on_top_response.changed() {
            self.set_always_on_top(ctx, self.always_on_top);
        }

        let fullscreen_response = ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");
        if fullscreen_response.changed() {
            self.toggle_fullscreen(ctx, self.is_fullscreen);
        }

        let mut temp_borderless = self.borderless;
        let borderless_response = ui
            .add_enabled(
                !self.is_fullscreen,
                egui::Checkbox::new(&mut temp_borderless, "タイトルバーを隠す"),
            )
            .on_hover_text(
                "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います",
            )
            .on_disabled_hover_text("フルスクリーン中は元から装飾がないため切り替えられません");
        if borderless_response.changed() {
            self.set_borderless(ctx, temp_borderless);
        }

        let enable_drag_move = if let Ok(settings) = self.settings.lock() {
            settings.ui.enable_drag_move
        } else {
            warn!("画面ドラッグ移動の設定の読み取りで settings のロックを取得できない");
            true
        };
        let mut temp_enable_drag_move = enable_drag_move;
        let drag_move_response = ui
            .add_enabled(
                !self.borderless,
                egui::Checkbox::new(&mut temp_enable_drag_move, "画面ドラッグ移動"),
            )
            .on_disabled_hover_text(
                "タイトルバーを隠している間は、ウィンドウを動かす唯一の手段なので切れません",
            );
        if drag_move_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.enable_drag_move = temp_enable_drag_move;
            } else {
                warn!("画面ドラッグ移動の設定の反映で settings のロックを取得できない");
            }
            self.mark_settings_dirty();
        }

        let stats_response = ui.checkbox(&mut self.show_stats_overlay, "情報表示");
        if stats_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.ui.show_stats_overlay = self.show_stats_overlay;
            } else {
                warn!("情報表示の設定の反映で settings のロックを取得できない");
            }
            self.mark_settings_dirty();
        }

        let auto_reconnect = if let Ok(settings) = self.settings.lock() {
            settings.video.auto_reconnect
        } else {
            warn!("デバイスの自動再接続の設定の読み取りで settings のロックを取得できない");
            true
        };
        let mut temp_auto_reconnect = auto_reconnect;
        let auto_reconnect_response = ui
            .checkbox(&mut temp_auto_reconnect, "デバイスの自動再接続")
            .on_hover_text(
                "映像が途切れたり音声デバイスが消えたときに、自動でデバイスを開き直します",
            );
        if auto_reconnect_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.video.auto_reconnect = temp_auto_reconnect;
            } else {
                warn!("デバイスの自動再接続の設定の反映で settings のロックを取得できない");
            }
            info!(
                "デバイスの自動再接続を{}にした",
                if temp_auto_reconnect {
                    "有効"
                } else {
                    "無効"
                }
            );
            self.mark_settings_dirty();
        }

        ui.separator();

        if ui
            .add_enabled(
                !self.is_fullscreen,
                egui::Button::new("ウィンドウサイズをリセット"),
            )
            .on_disabled_hover_text("フルスクリーン中は変更できません")
            .clicked()
        {
            self.reset_window_size(ctx);
            *close_menu = true;
        }
        if ui.button("デバイス再接続").clicked() {
            self.reconnect_devices();
            *close_menu = true;
        }

        // プリセットは映像と音声の取り込み方の切替なので、
        // 「デバイス再接続」のすぐそばに置く（collapsed 側と同じ理由）
        self.preset_submenu(ui, width, menu_rects, close_menu);

        ui.separator();
        if ui.button("詳細設定...").clicked() {
            self.show_settings = true;
            *close_menu = true;
        }

        ui.separator();
        if ui.button("終了").clicked() {
            info!("右クリックメニューから終了する");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            *close_menu = true;
        }
    }

    /// 右クリックメニューの項目を描く。
    ///
    /// 項目が増えて縦に伸びると低い解像度で下端が画面外へ出るため、切り替え系は
    /// 「表示」「ウィンドウ」のサブメニューへ分けてある。**直下に残すのは、映像が
    /// 出ないときの復帰手段（デバイス再接続）と、装飾を消しているときに他の手段が
    /// 無い操作（フルスクリーン、終了）。** 探し回らずに押せることを優先する。
    ///
    /// サブメニューの中身を足したときは、閉じるボタンに `ui.close_menu()` を
    /// 忘れないこと。呼ばないと開いた状態が egui 側に残り、次に右クリックした
    /// ときにサブメニューが開いたまま出る。
    fn context_menu_items(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.label(format!("音量: {}%", self.volume as i32));
        let volume_response =
            ui.add(egui::Slider::new(&mut self.volume, MIN_VOLUME..=MAX_VOLUME).suffix("%"));

        // 音量が変更された場合、設定に反映する（書き出しはデバウンス）。
        // スライダーが self.volume を書き換えたあとなので、同じ値を
        // 渡し直して設定への反映と OSD の表示だけを行わせる
        if volume_response.changed() {
            self.set_volume_from_ui(self.volume);
        }

        // ミュートはスライダーのすぐ下に置く。音量 0% にする代わりの
        // 操作なので、離すと探されない
        let mut muted = self.muted;
        if ui.checkbox(&mut muted, "ミュート").changed() {
            self.set_muted_from_ui(muted);
        }

        ui.separator();

        // フルスクリーン表示のチェックボックス。
        // ダブルクリックとホットキーでも切り替えられるが、装飾を消していると
        // ここが唯一目に見える切り替え手段になるので直下に残す
        let fullscreen_response = ui.checkbox(&mut self.is_fullscreen, "フルスクリーン表示");

        // フルスクリーン状態が変更された場合
        if fullscreen_response.changed() {
            self.toggle_fullscreen(ctx, self.is_fullscreen);
        }

        // サブメニューのボタンは既定だと文字の幅しか取らず、上下のチェック
        // ボックスと縁が揃わない。幅いっぱいに広げて 1 つの並びに見せる
        ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
            self.view_submenu(ctx, ui, width, menu_rects);
            self.window_submenu(ctx, ui, width, menu_rects, close_menu);
            // プリセットは映像と音声の取り込み方の切替なので、本来は下の
            // 「デバイス再接続」に近い。それでも他のサブメニューと並べて
            // あるのは、justified の並びから外すとボタンの幅が揃わないため
            self.preset_submenu(ui, width, menu_rects, close_menu);
        });

        ui.separator();
        // デバイス再接続。映像が出なくなったときの復帰手段なので、
        // サブメニューへ入れずに直下へ置く
        if ui.button("デバイス再接続").clicked() {
            self.reconnect_devices();
            *close_menu = true;
        }

        // デバイスの自動再接続のチェックボックス。
        // 設定は VideoSettings に持たせているが、音声ストリームの
        // エラーからの復帰にも効く（利用者から見て 1 つの機能なので
        // スイッチも 1 つにしてある）。上の「デバイス再接続」と紛らわしい
        // 項目なので、離さずに隣へ置いてある
        let auto_reconnect = if let Ok(settings) = self.settings.lock() {
            settings.video.auto_reconnect
        } else {
            warn!("デバイスの自動再接続の設定の読み取りで settings のロックを取得できない");
            true
        };
        let mut temp_auto_reconnect = auto_reconnect;
        let auto_reconnect_response = ui
            .checkbox(&mut temp_auto_reconnect, "デバイスの自動再接続")
            .on_hover_text(
                "映像が途切れたり音声デバイスが消えたときに、自動でデバイスを開き直します",
            );

        // 自動再接続の設定が変更された場合（書き出しはデバウンス）
        if auto_reconnect_response.changed() {
            if let Ok(mut settings) = self.settings.lock() {
                settings.video.auto_reconnect = temp_auto_reconnect;
            } else {
                warn!("デバイスの自動再接続の設定の反映で settings のロックを取得できない");
            }
            info!(
                "デバイスの自動再接続を{}にした",
                if temp_auto_reconnect {
                    "有効"
                } else {
                    "無効"
                }
            );
            self.mark_settings_dirty();
        }

        ui.separator();
        if ui.button("詳細設定...").clicked() {
            self.show_settings = true;
            *close_menu = true;
        }

        ui.separator();
        // 終了。装飾なしでは × が無いので、ここが閉じる手段になる。
        // 押すと on_exit が走り、保留中の設定も書き出される
        if ui.button("終了").clicked() {
            info!("右クリックメニューから終了する");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            *close_menu = true;
        }
    }

    /// 右クリックメニューの「表示」サブメニュー。
    ///
    /// 映像の見え方に関わる切り替えを集めてある。**どれを押してもメニューは
    /// 閉じない。** 続けて切り替えることがあるため。
    fn view_submenu(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
    ) {
        ui.menu_button("表示  ⏵", |ui| {
            // サブメニューの幅は egui の既定が 150px で、項目名が折り返す。
            // 本体と同じ幅に揃える（狭いウィンドウでは本体ごと縮んでいる）
            ui.set_max_width(width);

            let aspect_response = ui.checkbox(&mut self.maintain_aspect_ratio, "アスペクト比を維持");

            // アスペクト比設定が変更された場合、設定に反映する（書き出しはデバウンス）
            if aspect_response.changed() {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.maintain_aspect_ratio = self.maintain_aspect_ratio;
                } else {
                    warn!("アスペクト比の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }

            // 最前面表示のチェックボックス
            let always_on_top_response = ui.checkbox(&mut self.always_on_top, "最前面表示");

            // 最前面表示設定が変更された場合。
            // チェックボックスが self.always_on_top を書き換えたあとなので、
            // 同じ値を渡してウィンドウレベルの適用と保存だけを行わせる
            if always_on_top_response.changed() {
                self.set_always_on_top(ctx, self.always_on_top);
            }

            // 情報表示（統計オーバーレイ）のチェックボックス
            let stats_response = ui.checkbox(&mut self.show_stats_overlay, "情報表示");

            // 情報表示の設定が変更された場合（書き出しはデバウンス）
            if stats_response.changed() {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.show_stats_overlay = self.show_stats_overlay;
                } else {
                    warn!("情報表示の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }

            // タイトルバーを隠すチェックボックス。
            // フルスクリーン中は OS が元から装飾を外しているので触らせない。
            // ここで切り替えても見た目は変わらず、フルスクリーンを抜けた
            // ときに初めて効くので、操作と結果が結びつかない
            let mut temp_borderless = self.borderless;
            let borderless_response = ui
                .add_enabled(
                    !self.is_fullscreen,
                    egui::Checkbox::new(&mut temp_borderless, "タイトルバーを隠す"),
                )
                .on_hover_text(
                    "タイトルバーと枠を消します。移動は映像のドラッグ、サイズ変更はウィンドウ端のドラッグ、終了はこのメニューの「終了」か Alt+F4 で行います",
                )
                .on_disabled_hover_text(
                    "フルスクリーン中は元から装飾がないため切り替えられません",
                );

            if borderless_response.changed() {
                self.set_borderless(ctx, temp_borderless);
            }

            menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
        });
    }

    /// 右クリックメニューの「ウィンドウ」サブメニュー。
    ///
    /// ウィンドウの動かし方と大きさに関わる項目を集めてある。
    /// **「ウィンドウサイズをリセット」は押したらメニューを閉じる。**
    /// 結果がウィンドウ全体に出るので、メニューが被ったままだと確かめられない。
    fn window_submenu(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        ui.menu_button("ウィンドウ  ⏵", |ui| {
            ui.set_max_width(width);

            // 画面ドラッグ移動のチェックボックス。
            // 装飾なしの間は切らせない。切ると動かす手段が残らない
            let enable_drag_move = if let Ok(settings) = self.settings.lock() {
                settings.ui.enable_drag_move
            } else {
                warn!("画面ドラッグ移動の設定の読み取りで settings のロックを取得できない");
                true
            };
            let mut temp_enable_drag_move = enable_drag_move;
            let drag_move_response = ui
                .add_enabled(
                    !self.borderless,
                    egui::Checkbox::new(&mut temp_enable_drag_move, "画面ドラッグ移動"),
                )
                .on_disabled_hover_text(
                    "タイトルバーを隠している間は、ウィンドウを動かす唯一の手段なので切れません",
                );

            // 画面ドラッグ移動設定が変更された場合（書き出しはデバウンス）
            if drag_move_response.changed() {
                if let Ok(mut settings) = self.settings.lock() {
                    settings.ui.enable_drag_move = temp_enable_drag_move;
                } else {
                    warn!("画面ドラッグ移動の設定の反映で settings のロックを取得できない");
                }
                self.mark_settings_dirty();
            }

            // ウィンドウサイズのリセット。装飾なしで小さくしすぎて
            // 端の帯を掴めなくなったときの復帰手段
            if ui
                .add_enabled(
                    !self.is_fullscreen,
                    egui::Button::new("ウィンドウサイズをリセット"),
                )
                .on_disabled_hover_text("フルスクリーン中は変更できません")
                .clicked()
            {
                self.reset_window_size(ctx);
                // サブメニュー側も閉じる。閉じないと開いた状態が egui に残り、
                // 次に右クリックしたときにサブメニューが開いたまま出る
                ui.close_menu();
                *close_menu = true;
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
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        menu_rects: &mut Vec<egui::Rect>,
        close_menu: &mut bool,
    ) {
        // 名前と選択状態はここで 1 度だけ読む。サブメニューの中で
        // ロックを取ると、毎フレーム描画のたびに取り直すことになる
        let (names, active) = match self.settings.lock() {
            Ok(settings) => (
                settings
                    .presets
                    .iter()
                    .map(|preset| preset.name.clone())
                    .collect::<Vec<_>>(),
                settings::resolved_active_preset(&settings).map(str::to_string),
            ),
            Err(_) => {
                warn!("プリセットの一覧で settings のロックを取得できない");
                return;
            }
        };

        if names.is_empty() {
            return;
        }

        let mut selected: Option<String> = None;
        ui.menu_button("プリセット  ⏵", |ui| {
            ui.set_max_width(width);

            for name in &names {
                // 選択中のものにチェックを付ける。手で値を変えたあとは
                // どれも選択中にならない（resolved_active_preset が None）
                let is_active = active.as_deref() == Some(name.as_str());
                if ui.selectable_label(is_active, name).clicked() {
                    selected = Some(name.clone());
                    ui.close_menu();
                }
            }

            menu_rects.push(ui.min_rect().expand(CONTEXT_MENU_HIT_MARGIN));
        });

        if let Some(name) = selected {
            self.apply_preset_by_name(&name);
            *close_menu = true;
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

    #[test]
    fn estimate_flat_menu_height_with_default_style_is_positive() {
        let spacing = egui::Style::default().spacing;
        assert!(estimate_flat_menu_height(&spacing, false) > 0.0);
    }

    #[test]
    fn estimate_flat_menu_height_grows_with_row_height() {
        // 行が高くなるほど見積もりも大きくなること。逆行すると、
        // フォントサイズを上げたときに折りたたみ判定が正しく働かなくなる
        let mut spacing = egui::Style::default().spacing;
        let base = estimate_flat_menu_height(&spacing, false);

        spacing.interact_size.y *= 2.0;
        let taller = estimate_flat_menu_height(&spacing, false);

        assert!(taller > base);
    }

    #[test]
    fn estimate_flat_menu_height_with_presets_adds_one_row() {
        // プリセットがあると「プリセット」の行が 1 つ増える。ここが
        // ずれると、プリセットがある状態でだけ折りたたみ判定を誤る
        let spacing = egui::Style::default().spacing;
        let without_presets = estimate_flat_menu_height(&spacing, false);
        let with_presets = estimate_flat_menu_height(&spacing, true);

        let row_height = spacing.interact_size.y + spacing.item_spacing.y;
        assert!((with_presets - without_presets - row_height).abs() < 1e-3);
    }

    #[test]
    fn context_menu_layout_presets_row_tips_the_boundary_to_collapsed() {
        // プリセットが無ければちょうど収まる高さでも、プリセットの分だけ
        // 見積もりが増えると収まらなくなり、Collapsed へ倒れる
        let spacing = egui::Style::default().spacing;
        let flat_without_presets = estimate_flat_menu_height(&spacing, false);
        let flat_with_presets = estimate_flat_menu_height(&spacing, true);

        assert_eq!(
            context_menu_layout(flat_without_presets, flat_without_presets),
            MenuLayout::Flat
        );
        assert_eq!(
            context_menu_layout(flat_without_presets, flat_with_presets),
            MenuLayout::Collapsed
        );
    }
}
