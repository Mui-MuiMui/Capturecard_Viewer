//! 映像エリアの置き場所と操作の判定（#512）。ウィンドウ表示とフルスクリーンの描画
//! （`super`）が共通に使う。`CaptureCardViewer` の状態を持たず、受け取った値と
//! egui の応答だけで決めるので、egui_kittest で単体に確かめられる。

use eframe::egui;
use log::warn;

// 映像の縦横比を保ったまま、表示領域に収まる大きさを求める。
//
// self を使わない純粋な計算なので、ユニットテストできるよう
// impl の外へ出してある。
//
// 幅か高さが 0 以下の入力に対しては egui::Vec2::ZERO を返す。最小化や
// ウィンドウの極端な縮小で available_size が潰れると 0 除算で縦横比が
// inf / NaN になり、そのまま Rect へ渡すと描画が壊れるため。
// 呼び出し側は ZERO を「描画するものがない」と解釈して描画を飛ばす。
fn calculate_aspect_ratio_size(image_size: egui::Vec2, available_size: egui::Vec2) -> egui::Vec2 {
    if image_size.x <= 0.0
        || image_size.y <= 0.0
        || available_size.x <= 0.0
        || available_size.y <= 0.0
    {
        return egui::Vec2::ZERO;
    }

    let image_aspect = image_size.x / image_size.y;
    let available_aspect = available_size.x / available_size.y;

    if image_aspect > available_aspect {
        // 画像が横長 - 横幅に合わせる
        egui::Vec2::new(available_size.x, available_size.x / image_aspect)
    } else {
        // 画像が縦長 - 高さに合わせる
        egui::Vec2::new(available_size.y * image_aspect, available_size.y)
    }
}

/// 映像を置く矩形。`center` を中心に、縦横比を保つなら `calculate_aspect_ratio_size`、
/// 保たないなら表示領域いっぱいの大きさにする。
///
/// 表示領域が潰れている間は `None` を返し、呼び出し側は描画も当たり判定も行わない。
/// 大きさ 0 や負の矩形を割り当てても映像は見えず、ドラッグや右クリックの判定だけが
/// 残ると誤作動の元になる
pub(super) fn video_rect(
    available_size: egui::Vec2,
    center: egui::Pos2,
    image_size: egui::Vec2,
    maintain_aspect_ratio: bool,
) -> Option<egui::Rect> {
    let display_size = if maintain_aspect_ratio {
        calculate_aspect_ratio_size(image_size, available_size)
    } else {
        available_size
    };
    (display_size.x > 0.0 && display_size.y > 0.0)
        .then(|| egui::Rect::from_center_size(center, display_size))
}

/// 映像エリアのドラッグでウィンドウを動かす（`ViewportCommand::StartDrag` を送る）。
///
/// リサイズの帯の上にいる間は動かさない（両方が効くと、端を掴んだつもりでウィンドウごと
/// 動く）。`enable_drag_move` は `ui.enable_drag_move` を読む。ドラッグ中で帯の外のときだけ
/// 呼ぶので、設定のロックを毎フレーム取らない。`None` はロックを取れなかったとき
pub(super) fn drag_move(
    ctx: &egui::Context,
    response: &egui::Response,
    on_resize_edge: bool,
    enable_drag_move: impl FnOnce() -> Option<bool>,
) {
    if !response.dragged() || on_resize_edge {
        return;
    }
    match enable_drag_move() {
        Some(true) => ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag),
        Some(false) => {}
        None => warn!("ウィンドウドラッグの判定で settings のロックを取得できない"),
    }
}

/// 右クリックされたら、右クリックメニューを開く位置（ポインターの位置）
pub(super) fn context_menu_request(
    ctx: &egui::Context,
    response: &egui::Response,
) -> Option<egui::Pos2> {
    response
        .secondary_clicked()
        .then(|| ctx.input(|i| i.pointer.latest_pos().unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::super::VIDEO_AREA_SENSE;
    use super::*;
    use egui::Vec2;
    use egui_kittest::Harness;

    #[test]
    fn video_rect_centers_the_aspect_fitted_size() {
        // 2:1 の映像を 400x400 へ。縦横比を保つなら 400x200 を中央へ置く
        let center = egui::pos2(210.0, 210.0);
        let available = Vec2::new(400.0, 400.0);
        let image = Vec2::new(1600.0, 800.0);
        assert_eq!(
            video_rect(available, center, image, true),
            Some(egui::Rect::from_center_size(
                center,
                Vec2::new(400.0, 200.0)
            ))
        );
        // 保たないなら表示領域いっぱい
        assert_eq!(
            video_rect(available, center, image, false),
            Some(egui::Rect::from_center_size(center, available))
        );
    }

    #[test]
    fn video_rect_is_none_while_the_area_is_collapsed() {
        let center = egui::pos2(0.0, 0.0);
        let image = Vec2::new(1600.0, 800.0);
        for maintain in [true, false] {
            assert_eq!(
                video_rect(Vec2::new(400.0, 0.0), center, image, maintain),
                None
            );
            assert_eq!(
                video_rect(Vec2::new(-10.0, 400.0), center, image, maintain),
                None
            );
        }
        // 映像の側が潰れていても、縦横比を保つなら描かない
        assert_eq!(
            video_rect(Vec2::new(400.0, 400.0), center, Vec2::ZERO, true),
            None
        );
    }

    // ---- 操作の判定（egui_kittest、#512） ----

    /// 映像エリアの矩形
    const AREA: egui::Rect =
        egui::Rect::from_min_max(egui::pos2(20.0, 20.0), egui::pos2(220.0, 140.0));

    /// 映像エリアを 1 つ置き、`drag_move` と `context_menu_request` を呼ぶ `Harness` の状態
    struct AreaProbe {
        /// `drag_move` へ渡す `ui.enable_drag_move`。`None` はロックを取れなかった形
        enable_drag_move: Option<bool>,
        on_resize_edge: bool,
        /// `drag_move` が設定を読んだ回数
        reads: usize,
        /// `context_menu_request` が返した位置
        menu: Option<egui::Pos2>,
    }

    impl AreaProbe {
        fn new(enable_drag_move: Option<bool>, on_resize_edge: bool) -> Self {
            Self {
                enable_drag_move,
                on_resize_edge,
                reads: 0,
                menu: None,
            }
        }

        fn draw(&mut self, ui: &mut egui::Ui) {
            let ctx = ui.ctx().clone();
            let response = ui.allocate_rect(AREA, VIDEO_AREA_SENSE);
            drag_move(&ctx, &response, self.on_resize_edge, || {
                self.reads += 1;
                self.enable_drag_move
            });
            if let Some(pos) = context_menu_request(&ctx, &response) {
                self.menu = Some(pos);
            }
        }
    }

    fn area_harness(probe: AreaProbe) -> Harness<'static, AreaProbe> {
        // 日本語のフォントは読み込まず、字形の無い文字は豆腐で描く（0.36 までと同じ）。
        // egui_kittest 0.37 は既定で字形の無い文字に当たると panic する
        Harness::builder()
            .allow_missing_glyphs()
            .with_size(Vec2::new(240.0, 160.0))
            .build_ui_state(|ui, probe: &mut AreaProbe| probe.draw(ui), probe)
    }

    fn start_drag_sent(harness: &Harness<'static, AreaProbe>) -> bool {
        harness.output().viewport_output.values().any(|viewport| {
            viewport
                .commands
                .contains(&egui::ViewportCommand::StartDrag)
        })
    }

    /// 映像エリアの中でドラッグし、どこかのフレームで `StartDrag` が出たか
    fn drag_inside(harness: &mut Harness<'static, AreaProbe>) -> bool {
        let start = AREA.center();
        harness.hover_at(start);
        harness.step();
        harness.drag_at(start);
        harness.step();
        let mut sent = false;
        for dx in [10.0, 20.0, 30.0] {
            harness.hover_at(start + Vec2::new(dx, 0.0));
            harness.step();
            sent |= start_drag_sent(harness);
        }
        harness.drop_at(start + Vec2::new(30.0, 0.0));
        harness.step();
        sent
    }

    #[test]
    fn dragging_the_video_area_starts_moving_the_window() {
        let mut harness = area_harness(AreaProbe::new(Some(true), false));
        assert!(drag_inside(&mut harness));
    }

    #[test]
    fn dragging_does_not_move_the_window_when_drag_move_is_off_or_on_the_resize_edge() {
        // 設定で切ってある
        let mut harness = area_harness(AreaProbe::new(Some(false), false));
        assert!(!drag_inside(&mut harness));
        assert!(harness.state().reads > 0);
        // 設定のロックを取れない
        let mut harness = area_harness(AreaProbe::new(None, false));
        assert!(!drag_inside(&mut harness));
        assert!(harness.state().reads > 0);
        // リサイズの帯の上。設定も読まない
        let mut harness = area_harness(AreaProbe::new(Some(true), true));
        assert!(!drag_inside(&mut harness));
        assert_eq!(harness.state().reads, 0);
    }

    #[test]
    fn hovering_without_dragging_does_not_read_the_setting() {
        // ドラッグしていない間は設定のロックを取らない
        let mut harness = area_harness(AreaProbe::new(Some(true), false));
        harness.hover_at(AREA.center());
        harness.run_steps(3);
        assert_eq!(harness.state().reads, 0);
        assert!(!start_drag_sent(&harness));
    }

    #[test]
    fn right_clicking_the_video_area_requests_the_menu_at_the_pointer() {
        let mut harness = area_harness(AreaProbe::new(Some(true), false));
        let pos = AREA.center() + Vec2::new(15.0, -10.0);
        harness.hover_at(pos);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            harness.step();
        }
        assert_eq!(harness.state().menu, Some(pos));
    }

    #[test]
    fn right_clicking_outside_the_video_area_requests_nothing() {
        let mut harness = area_harness(AreaProbe::new(Some(true), false));
        let pos = egui::pos2(230.0, 150.0);
        harness.hover_at(pos);
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            harness.step();
        }
        assert_eq!(harness.state().menu, None);
    }

    // calculate_aspect_ratio_size のテストで使う値は、期待値が 2 進小数で
    // 割り切れるように選んである。誤差を許容する比較にすると、桁落ちが
    // 起きても気付けないため。
    #[test]
    fn calculate_aspect_ratio_size_wide_image_fits_to_width() {
        // 2:1 の映像を正方形の領域へ。横幅いっぱいに広げて上下を余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_tall_image_fits_to_height() {
        // 1:2 の映像を正方形の領域へ。高さいっぱいに広げて左右を余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(800.0, 1600.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::new(200.0, 400.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_same_aspect_fills_area() {
        // 縦横比が一致するときは領域をそのまま埋める
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 200.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_area_wider_than_image_fits_to_height() {
        // 領域のほうが横長。高さに合わせ、横幅は余らせる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(1000.0, 200.0));

        assert_eq!(size, Vec2::new(400.0, 200.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_upscales_to_fill_area() {
        // 映像より領域が大きいときは拡大する。縮小専用ではない
        let size = calculate_aspect_ratio_size(Vec2::new(400.0, 200.0), Vec2::new(1600.0, 1600.0));

        assert_eq!(size, Vec2::new(1600.0, 800.0));
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_height_area_returns_zero() {
        // 最小化やウィンドウの極端な縮小で高さが 0 になる。
        // available_size.x / available_size.y が inf になるケース
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(400.0, 0.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_width_area_returns_zero() {
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(0.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_area_returns_zero() {
        // 幅も高さも 0。0.0 / 0.0 が NaN になるケース
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::ZERO);

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_negative_area_returns_zero() {
        // egui のレイアウトは余白が足りないと負の available_size を返すことがある。
        // 負の大きさの矩形を描画に渡さないよう、ここで潰す
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 800.0), Vec2::new(-10.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_height_image_returns_zero() {
        // テクスチャ側が潰れている場合。image_size.x / image_size.y が inf になる
        let size = calculate_aspect_ratio_size(Vec2::new(1600.0, 0.0), Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_zero_image_returns_zero() {
        // 0.0 / 0.0 で image_aspect が NaN になり、
        // 掛け算の結果として NaN が呼び出し側へ漏れるケース
        let size = calculate_aspect_ratio_size(Vec2::ZERO, Vec2::new(400.0, 400.0));

        assert_eq!(size, Vec2::ZERO);
    }

    #[test]
    fn calculate_aspect_ratio_size_degenerate_input_never_returns_nan_or_inf() {
        // 描画へ渡る値が NaN / inf にならないことを、退化した入力の組で一括して確かめる
        let degenerate = [
            (Vec2::ZERO, Vec2::ZERO),
            (Vec2::new(1600.0, 800.0), Vec2::new(400.0, 0.0)),
            (Vec2::new(1600.0, 800.0), Vec2::new(0.0, 400.0)),
            (Vec2::new(1600.0, 0.0), Vec2::new(400.0, 400.0)),
            (Vec2::new(0.0, 800.0), Vec2::new(400.0, 400.0)),
            (Vec2::new(1600.0, 800.0), Vec2::new(-10.0, -10.0)),
        ];

        for (image_size, available_size) in degenerate {
            let size = calculate_aspect_ratio_size(image_size, available_size);

            assert!(
                size.x.is_finite() && size.y.is_finite(),
                "image={:?} available={:?} で {:?} を返した",
                image_size,
                available_size,
                size
            );
            assert!(
                size.x >= 0.0 && size.y >= 0.0,
                "image={:?} available={:?} で負の大きさ {:?} を返した",
                image_size,
                available_size,
                size
            );
        }
    }
}
