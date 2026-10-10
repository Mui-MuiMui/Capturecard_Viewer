//! 映像の描画と、その上での操作の受け付け。
//!
//! ウィンドウ表示とフルスクリーンで同じことを行うため、どちらの描画も
//! ここに並べてある。ウィンドウそのものの操作（装飾・リサイズ・
//! フルスクリーン切替）は `super::window`、右クリックメニューは `super::menu`。

use super::placeholder::{show_video_placeholder, video_placeholder_text};
use super::video_overlay::show_video_overlay;
use super::CaptureCardViewer;
use crate::status::{self, ErrorSource};
use crate::video::{GpuYuy2, PixelFormat, VideoFrame, GPU_CONVERT_ENV};
use eframe::egui;
use log::{debug, warn};
use std::sync::Arc;
use std::time::Instant;

/// 映像エリア（映像が無いときのプレースホルダーを含む）が受け付ける操作。
///
/// クリック（右クリックメニュー・ダブルクリック・中クリック）とドラッグ
/// （ウィンドウの移動）だけを受け、**キーボードフォーカスは受けない**
/// （`Sense::FOCUSABLE` を立てない）。`Sense::click_and_drag()` はフォーカスを受けるため、
/// Tab キーで映像エリアにフォーカスが移ると、以後ずっと「何かのウィジェットに
/// フォーカスがある」状態が続く（#238）。映像エリアにはキーボードで操作する
/// ものが無いので、フォーカスを受ける理由も無い。
pub(super) const VIDEO_AREA_SENSE: egui::Sense = egui::Sense::CLICK.union(egui::Sense::DRAG);

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

/// テクスチャへ取り込めるフレームか。画素データの長さが形式どおり（RGB なら
/// `幅 × 高さ × 3`、YUY2 なら `幅 × 高さ × 2`）ちょうどのときだけ真
/// （`egui::ColorImage::from_rgb` と GPU へのアップロードの前提）。
fn is_drawable_frame(frame: &VideoFrame) -> bool {
    frame.has_exact_len()
}

/// テクスチャへ渡す画像を作る。前に渡した画像を egui が手放していれば、その画素の
/// Vec へ詰め直して返す（容量が足りていれば確保は起きない）。握られていれば新しく作る。
/// egui はテクスチャの更新を描画の終わりにレンダラーへ渡し、渡し終えたら `Arc` を
/// 手放すので、次のフレームでは普通は使い回せる。**呼び出し側は `is_drawable_frame` で長さを確かめてから呼ぶこと**
/// （`as_chunks` は余りを黙って捨てるので、ここでは長さの食い違いに気づけない）。
fn reuse_or_new_color_image(
    previous: Option<Arc<egui::ColorImage>>,
    frame: &VideoFrame,
) -> Arc<egui::ColorImage> {
    if let Some(mut image) = previous {
        if let Some(target) = Arc::get_mut(&mut image) {
            target.size = [frame.width, frame.height];
            target.pixels.clear();
            let (rgb, _) = frame.data.as_chunks::<3>();
            target.pixels.extend(
                rgb.iter()
                    .map(|&[r, g, b]| egui::Color32::from_rgb(r, g, b)),
            );
            return image;
        }
    }
    let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.data);
    Arc::new(image)
}

/// 映像のテクスチャの拡大縮小。拡大は Nearest（補間なし）で軽く、縮小は Linear。
/// GPU で変換するとき（#456）も同じテクスチャを egui が描くので、見え方は変わらない
const VIDEO_TEXTURE_OPTIONS: egui::TextureOptions = egui::TextureOptions {
    magnification: egui::TextureFilter::Nearest,
    minification: egui::TextureFilter::Linear,
    wrap_mode: egui::TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

impl CaptureCardViewer {
    /// 新着フレームがあればテクスチャへ取り込む。取り込んだら `true`。
    ///
    /// **ここで再描画を予約しない。** 予約は `update()` の末尾で 1 か所にまとめる。
    /// 以前はここで無条件に 16ms（60fps）の再描画を予約していたため、映像が
    /// 来ていなくても、最小化していても描き続けていた（Issue #98）。
    pub(super) fn update_video_texture(&mut self, ctx: &egui::Context) -> bool {
        // GPU から CPU へ切り替わった直後（#456）は、新着を待たずに手元の最新フレームを
        // CPU で描き直す。GPU で描けなかったフレームの代わりに、黒や前の画を残さない
        if self.gpu_yuy2.take_switched_to_cpu() {
            if let Some(frame) = self.frames.latest().filter(|f| is_drawable_frame(f)) {
                self.draw_frame(ctx, frame);
            }
        }

        // 新着フレームが無ければ何もしない。既存のテクスチャをそのまま使い回す。
        // **フレームだけはワーカーのチャネルを通さない。** コマンドの列に
        // 並べると、接続や列挙の後ろで待たされて遅延が増える。
        //
        // **ロックが取れないときの警告も持たない。** `VideoFrames` の中で
        // 失敗を握り潰して「新着なし」に倒すだけなので、毎フレーム呼ばれる
        // この経路からログが出ることはない
        let new_frame = self.frames.newer_than(self.last_frame_generation);

        if let Some((frame, generation, received_at)) = new_frame {
            // 前に取り込んでからこのフレームまでに上書きされた枚数。ログにだけ出す（#459）。
            // 1 枚目は比べる相手が無いので数えない
            let skipped_frames = if self.last_frame_generation == 0 {
                0
            } else {
                generation.saturating_sub(self.last_frame_generation + 1)
            };
            self.last_frame_generation = generation;

            // 長さが合わないフレームは描かず、前のテクスチャを保つ。`from_rgb` は
            // 長さが違うと assert で落ちる（#309）。積む側（`FrameSink`）で揃えて
            // あるので通常は来ないが、二重の守りとして置いておく。世代は進めて
            // あるので、同じフレームで毎回ここへ来ることはない
            if !is_drawable_frame(&frame) {
                return false;
            }
            self.draw_frame(ctx, frame);

            // 表示までの遅れ（#455）。GPU が画面へ出した時刻は取れないので、テクスチャを
            // 更新した直後で測る（GPU で変換するときは、変換を預けた直後。CPU の経路でも
            // テクスチャへの転送は描画の終わりなので、測っている区間は同じ）。
            // 到着時刻は世代番号と同じロックの中で読んだもの
            let now = Instant::now();
            let latency = now.saturating_duration_since(received_at);
            if let Some(log) = self.display_latency.record(now, latency, skipped_frames) {
                let s = log.latency;
                debug!(
                    "表示までの遅れ（到着→テクスチャ更新、30 秒）: 平均 {:.2}ms、最大 {:.2}ms、{} 枚（取り込む前に上書き {} 枚、新着なしの update() {} 回）",
                    s.average_ms, s.max_ms, s.samples, log.skipped_frames, log.idle_passes
                );
            }

            return true;
        }

        false
    }

    /// フレームを映像のテクスチャへ取り込む。呼び出し側は `is_drawable_frame` で確かめてから呼ぶ。
    ///
    /// YUY2 のフレーム（#456）は、GPU を使っていれば描画のコールバックへ預けるだけ。
    /// 使っていなければ（GPU から CPU へ切り替わった前後に届いたものだけ）ここで CPU で変換する
    fn draw_frame(&mut self, ctx: &egui::Context, frame: Arc<VideoFrame>) {
        let rgb = match frame.format {
            PixelFormat::Rgb24 => frame,
            PixelFormat::Yuy2(_) => {
                let size = [frame.width, frame.height];
                if self.gpu_yuy2.queue(Arc::clone(&frame)) {
                    // テクスチャは大きさが変わったときだけ作り直す（中身は黒。同じ描画の
                    // コールバックが映像を描く前に上書きする）
                    if self.video_texture.as_ref().map(|t| t.size()) != Some(size) {
                        let blank = egui::ColorImage::filled(size, egui::Color32::BLACK);
                        self.set_video_texture(ctx, Arc::new(blank));
                    }
                    return;
                }
                frame.into_rgb()
            }
        };
        // 前に渡した画像の Vec を使い回す。1080p で約 8MB を毎フレーム確保・
        // 解放しないため。手元にも `Arc` を 1 つ残しておき、egui が手放したら
        // 次のフレームで詰め直す
        let image = reuse_or_new_color_image(self.video_image.take(), &rgb);
        self.video_image = Some(Arc::clone(&image));
        self.set_video_texture(ctx, image);
    }

    /// 映像のテクスチャへ画像を渡す。まだ無ければ作る
    fn set_video_texture(&mut self, ctx: &egui::Context, image: Arc<egui::ColorImage>) {
        if let Some(texture) = &mut self.video_texture {
            texture.set(image, VIDEO_TEXTURE_OPTIONS);
        } else {
            self.video_texture =
                Some(ctx.load_texture("video_frame", image, VIDEO_TEXTURE_OPTIONS));
        }
    }

    /// YUY2 → RGB を GPU で行う準備をする（#456）。**起動経路で 1 回だけ**呼ぶ（`main.rs`）。
    /// GL のコンテキストが無い・環境変数で切ってある・シェーダーを使えないときは CPU のまま
    pub(crate) fn init_gpu_yuy2(&mut self, gl: Option<&Arc<eframe::glow::Context>>) {
        let env = std::env::var(GPU_CONVERT_ENV).ok();
        let preference = self
            .settings
            .lock()
            .map(|settings| settings.video.convert)
            .unwrap_or_default();
        self.gpu_yuy2 = GpuYuy2::init(
            gl,
            &self.color_conversion,
            &self.repaint_waker,
            preference,
            env.as_deref(),
        );
    }

    /// 映像を `rect` へ描く。GPU で変換するフレームを預かっていれば、変換のコールバックを
    /// 映像より前に置く（egui は置いた順に描くので、変換してからテクスチャを描く）
    fn paint_video(&self, ui: &egui::Ui, texture: &egui::TextureHandle, rect: egui::Rect) {
        if let Some(callback) = self.gpu_yuy2.paint_callback(texture.id(), rect) {
            ui.painter().add(callback);
        }
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)),
            egui::Color32::WHITE,
        );
    }

    pub(super) fn show_windowed_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        // 映像が無いときの文言は描画に入る前に決める。
        // 描画のクロージャの中でロックを取らないため
        let placeholder = video_placeholder_text(
            self.device_snapshot.video_capturing,
            self.device_snapshot.video_retry.active,
            self.error_detail(ErrorSource::Video).as_deref(),
        );

        // 装飾なしのときだけ、ウィンドウ端のドラッグをリサイズに割り当てる。
        // 帯の上にいる間は映像のドラッグ移動を止める（両方が効くと、
        // 端を掴んだつもりでウィンドウごと動く）
        let on_resize_edge = self.handle_borderless_resize(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(2))) // マージンを2pxに設定
            .show(ui, |ui| {
                // 映像表示エリア
                let available_size = ui.available_size();

                if let Some(texture) = &self.video_texture {
                    let image_size = texture.size_vec2();
                    let display_size = if self.maintain_aspect_ratio {
                        calculate_aspect_ratio_size(image_size, available_size)
                    } else {
                        available_size
                    };

                    // 表示領域が潰れている間は描画も当たり判定も行わない。
                    // 大きさ 0 や負の矩形を割り当てても映像は見えず、
                    // ドラッグや右クリックの判定だけが残ると誤作動の元になる。
                    if display_size.x <= 0.0 || display_size.y <= 0.0 {
                        return;
                    }

                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size,
                    );

                    let response = ui.allocate_rect(rect, VIDEO_AREA_SENSE);
                    self.paint_video(ui, texture, rect);

                    // ウィンドウドラッグを処理（設定が有効な場合のみ）
                    if response.dragged() && !on_resize_edge {
                        if let Ok(settings) = self.settings.lock() {
                            if settings.ui.enable_drag_move {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        } else {
                            warn!("ウィンドウドラッグの判定で settings のロックを取得できない");
                        }
                    }

                    // インタラクションを処理
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, true);
                    }

                    if response.secondary_clicked() {
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
                    }

                    self.handle_middle_click_mute(&response);

                    // 音量調整のためのスクロールを処理
                    if response.hovered() {
                        self.handle_volume_scroll(ctx);
                    }
                } else {
                    let response = show_video_placeholder(ui, available_size, &placeholder);

                    // 空エリアでのウィンドウドラッグを処理（設定が有効な場合のみ）
                    if response.dragged() && !on_resize_edge {
                        if let Ok(settings) = self.settings.lock() {
                            if settings.ui.enable_drag_move {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        } else {
                            warn!("ウィンドウドラッグの判定で settings のロックを取得できない");
                        }
                    }

                    // 空エリアでの右クリックを処理
                    if response.secondary_clicked() {
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
                    }

                    self.handle_middle_click_mute(&response);
                }
            });
    }

    pub(super) fn show_fullscreen_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        // ウィンドウ表示と同じ理由で、描画に入る前に文言を決める
        let placeholder = video_placeholder_text(
            self.device_snapshot.video_capturing,
            self.device_snapshot.video_retry.active,
            self.error_detail(ErrorSource::Video).as_deref(),
        );
        // フルスクリーンUI（装飾なし、ウィンドウ版と同等の機能）
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::same(0))) // フルスクリーンはマージン0
            .show(ui, |ui| {
                let available_size = ui.available_size();

                if let Some(texture) = &self.video_texture {
                    let image_size = texture.size_vec2();
                    let display_size = if self.maintain_aspect_ratio {
                        calculate_aspect_ratio_size(image_size, available_size)
                    } else {
                        available_size
                    };

                    // 表示領域が潰れている間は描画も当たり判定も行わない。
                    // 大きさ 0 や負の矩形を割り当てても映像は見えず、
                    // ドラッグや右クリックの判定だけが残ると誤作動の元になる。
                    if display_size.x <= 0.0 || display_size.y <= 0.0 {
                        return;
                    }

                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size,
                    );

                    let response = ui.allocate_rect(rect, VIDEO_AREA_SENSE);
                    self.paint_video(ui, texture, rect);

                    // フルスクリーンではドラッグ移動を完全に無効化
                    // （フルスクリーンでは画面の移動自体が意味をなさないため）

                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, false);
                    }

                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
                    }

                    self.handle_middle_click_mute(&response);

                    // マウススクロールでの音量調整（ウィンドウ版と同じ機能）
                    if response.hovered() {
                        self.handle_volume_scroll(ctx);
                    }
                } else {
                    // 映像信号がない場合
                    let response = show_video_placeholder(ui, available_size, &placeholder);

                    // フルスクリーンではドラッグ移動を完全に無効化
                    // （フルスクリーンでは画面の移動自体が意味をなさないため）

                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, false);
                    }

                    // 右クリックでコンテキストメニュー
                    if response.secondary_clicked() {
                        let pos = ctx.input(|i| i.pointer.latest_pos().unwrap_or_default());
                        self.open_context_menu(ctx, pos);
                    }

                    self.handle_middle_click_mute(&response);
                }
            });
    }

    /// フェイクデバイスで動いている間、映像の上端に常設の帯を描く（#252）。
    ///
    /// **トースト（`transient_overlay`）には入れない。** あちらは 1 件しか持たず、
    /// 起動直後に保存済みの実機名で接続に失敗すると、その `report_error` に
    /// 上書きされて見えなくなる。帯はフェイクで動いている間ずっと出す。
    /// 文言は「接続状態」タブの注意書きと同じもの。
    pub(super) fn draw_fake_devices_banner(&self, ctx: &egui::Context, stats_bottom: Option<f32>) {
        let Some(text) = status::fake_devices_notice(self.device.fake_devices()) else {
            return;
        };
        // 設定ダイアログより下に描く（#284）。理由は `show_video_overlay` にある
        let screen = ctx.content_rect();
        let area = egui::Rect::from_min_max(
            egui::pos2(
                screen.left(),
                screen.top() + fake_devices_banner_top(stats_bottom),
            ),
            screen.max,
        );
        show_video_overlay(
            ctx,
            egui::Id::new("fake_devices_banner"),
            area,
            egui::Align2::CENTER_TOP,
            |ui| {
                // 映像の上でも読めるよう、テーマの不透明な地（popup と同じ）に
                // 設定ダイアログと同じ注意書きを載せる
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(2))
                    .show(ui, |ui| {
                        crate::ui::warning_label(ui, text);
                    });
            },
        );
    }
}

/// フェイクデバイスの帯と、画面の上端や統計オーバーレイとの間隔。
const FAKE_DEVICES_BANNER_MARGIN: f32 = 8.0;

/// フェイクデバイスの帯を画面の上端から何 px 下げて描くか。
///
/// 統計オーバーレイ（左上）を出しているときはその下へずらす。狭いウィンドウでは
/// 上端中央の帯と左上の統計が横に重なるため。`stats_bottom` は統計の枠の下端で、
/// 出していなければ `None`。
fn fake_devices_banner_top(stats_bottom: Option<f32>) -> f32 {
    match stats_bottom {
        Some(bottom) => bottom + FAKE_DEVICES_BANNER_MARGIN,
        None => FAKE_DEVICES_BANNER_MARGIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Vec2;

    fn frame(width: usize, height: usize, len: usize) -> VideoFrame {
        VideoFrame {
            width,
            height,
            data: vec![0; len],
            format: PixelFormat::Rgb24,
        }
    }

    #[test]
    fn is_drawable_frame_accepts_exact_length() {
        assert!(is_drawable_frame(&frame(3, 2, 18)));
    }

    #[test]
    fn is_drawable_frame_rejects_mismatched_length() {
        // `ColorImage::from_rgb` は長さが違うと assert で落ちるので、どちらも描かない
        assert!(!is_drawable_frame(&frame(3, 2, 17)));
        assert!(!is_drawable_frame(&frame(3, 2, 19)));
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

    #[test]
    fn fake_devices_banner_sits_at_top_without_stats() {
        assert_eq!(fake_devices_banner_top(None), FAKE_DEVICES_BANNER_MARGIN);
    }

    #[test]
    fn fake_devices_banner_goes_below_stats_overlay() {
        // 統計の枠の下端より下に来れば、横幅が狭くても重ならない
        let stats_bottom = 120.0;
        let top = fake_devices_banner_top(Some(stats_bottom));
        assert!(
            top > stats_bottom,
            "統計の下端 {} に対して {}",
            stats_bottom,
            top
        );
        assert_eq!(top, stats_bottom + FAKE_DEVICES_BANNER_MARGIN);
    }

    #[test]
    fn fake_devices_banner_shown_only_with_fake_devices() {
        // 帯を出すかどうかは「接続状態」タブの注意書きと同じ判定を使う
        assert!(status::fake_devices_notice(false).is_none());
        assert!(status::fake_devices_notice(true).is_some());
    }

    fn rgb_frame(width: usize, height: usize, data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            data,
            ..frame(width, height, 0)
        }
    }

    // 画像の大きさと、画素を RGBA の並びにしたもの。期待値をベタ書きで比べるため
    fn size_and_rgba(image: &egui::ColorImage) -> ([usize; 2], Vec<[u8; 4]>) {
        let pixels = image.pixels.iter().map(|pixel| pixel.to_array()).collect();
        (image.size, pixels)
    }

    #[test]
    fn reuse_or_new_color_image_without_previous_converts_rgb() {
        let frame = rgb_frame(2, 1, vec![10, 20, 30, 40, 50, 60]);
        let image = reuse_or_new_color_image(None, &frame);
        assert_eq!(
            size_and_rgba(&image),
            ([2, 1], vec![[10, 20, 30, 255], [40, 50, 60, 255]])
        );
    }

    #[test]
    fn reuse_or_new_color_image_refills_unshared_previous_in_place() {
        // egui が手放した後（手元の `Arc` だけ）なら、同じ画像を詰め直して返す
        let first = reuse_or_new_color_image(None, &rgb_frame(2, 1, vec![0; 6]));
        let first_ptr = Arc::as_ptr(&first);
        let pixels_ptr = first.pixels.as_ptr();

        let frame = rgb_frame(1, 2, vec![1, 2, 3, 4, 5, 6]);
        let second = reuse_or_new_color_image(Some(first), &frame);

        assert_eq!(Arc::as_ptr(&second), first_ptr);
        // 同じ画素数なので Vec も確保し直していない
        assert_eq!(second.pixels.as_ptr(), pixels_ptr);
        assert_eq!(
            size_and_rgba(&second),
            ([1, 2], vec![[1, 2, 3, 255], [4, 5, 6, 255]])
        );
    }

    #[test]
    fn reuse_or_new_color_image_grows_when_frame_gets_larger() {
        // 解像度が上がったら同じ画像のまま広げる。古い画素が残らない
        let first = reuse_or_new_color_image(None, &rgb_frame(1, 1, vec![9, 9, 9]));
        let second = reuse_or_new_color_image(Some(first), &rgb_frame(2, 1, (0..6).collect()));
        assert_eq!(
            size_and_rgba(&second),
            ([2, 1], vec![[0, 1, 2, 255], [3, 4, 5, 255]])
        );
    }

    #[test]
    fn reuse_or_new_color_image_leaves_shared_previous_untouched() {
        // egui がまだ握っている画像は書き換えず、新しく作る
        let first = reuse_or_new_color_image(None, &rgb_frame(1, 1, vec![7, 7, 7]));
        let held_by_egui = Arc::clone(&first);

        let frame = rgb_frame(1, 1, vec![1, 2, 3]);
        let second = reuse_or_new_color_image(Some(first), &frame);

        assert!(!Arc::ptr_eq(&second, &held_by_egui));
        assert_eq!(size_and_rgba(&held_by_egui), ([1, 1], vec![[7, 7, 7, 255]]));
        assert_eq!(size_and_rgba(&second), ([1, 1], vec![[1, 2, 3, 255]]));
    }
}
