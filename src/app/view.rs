//! 映像の描画と、その上での操作の受け付け。
//!
//! ウィンドウ表示とフルスクリーンで同じことを行うため、どちらの描画も
//! ここに並べてある。ウィンドウそのものの操作（装飾・リサイズ・
//! フルスクリーン切替）は `super::window`、右クリックメニューは `super::menu`。

mod area;

use super::placeholder::{show_video_placeholder, video_placeholder_text};
use super::video_overlay::show_video_overlay;
use super::CaptureCardViewer;
use crate::status::{self, ErrorSource};
use crate::video::{
    DisplaySummaryLog, GpuContext, GpuYuy2, PixelFormat, VideoFrame, GPU_CONVERT_ENV,
};
use area::{context_menu_request, drag_move, video_rect};
use eframe::egui;
use log::{debug, info};
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

/// GPU で変換するフレーム（#456）のために、映像のテクスチャを作り直す（黒で確保する）か。
/// まだ無いか、大きさが変わったときだけ。同じ大きさなら前の中身へシェーダーが上書きする
fn needs_new_texture(current: Option<[usize; 2]>, frame: [usize; 2]) -> bool {
    current != Some(frame)
}

/// 映像のテクスチャの拡大縮小。拡大は Nearest（補間なし）で軽く、縮小は Linear。
/// GPU で変換するとき（#456）も同じテクスチャを egui が描くので、見え方は変わらない
const VIDEO_TEXTURE_OPTIONS: egui::TextureOptions = egui::TextureOptions {
    magnification: egui::TextureFilter::Nearest,
    minification: egui::TextureFilter::Linear,
    wrap_mode: egui::TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

/// 表示の観測値の 5 分の要約（#501）をログへ出す。描画の CPU 時間は swap / present の
/// 待ちを含まないので、上書きが増えたのにここが変わらなければ UI スレッドは CPU の外で
/// 止まっている見込み（`docs/design/video-pipeline.md` の「表示までの遅れの計測」）。
/// フルスクリーンの出入りで区切った窓（`window.rs` の `toggle_fullscreen`）もここから出す
pub(super) fn log_display_summary(summary: &DisplaySummaryLog) {
    let mode = if summary.fullscreen {
        "フルスクリーン"
    } else {
        "ウィンドウ"
    };
    let cut = if summary.cut_by_mode_change {
        "、フルスクリーンの切り替えで区切った"
    } else {
        ""
    };
    let minimized = if summary.minimized {
        "。最小化を挟んだので、枚/秒・上書き・新着なしの update() は比べられない"
    } else {
        ""
    };
    let frame_time = summary.frame_time.map_or_else(
        || "-".to_string(),
        |t| format!("平均 {:.2}ms / 最大 {:.2}ms", t.average_ms, t.max_ms),
    );
    let s = summary.latency;
    info!(
        "表示の観測値（{:.0} 秒、{}{}）: 取り込み {} 枚（{:.1} 枚/秒）、取り込む前に上書き {} 枚、新着なしの update() {} 回、到着→テクスチャ更新 平均 {:.2}ms / 最大 {:.2}ms、描画の CPU 時間 {}{}",
        summary.elapsed.as_secs_f32(),
        mode,
        cut,
        s.samples,
        summary.frames_per_second(),
        summary.skipped_frames,
        summary.idle_passes,
        s.average_ms,
        s.max_ms,
        frame_time,
        minimized
    );
}

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
            // 更新した直後で測る。GPU で変換するとき（#456）は変換を預けた直後で止まり、
            // このあと描画の中で走る YUY2 の転送とシェーダーの CPU 時間を含まない（CPU の経路は
            // RGB → Color32 の詰め直しを含む）。CPU の経路との差の一部は計測点の移動による見かけ。
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
            // 同じ数の 5 分ぶんを INFO で出す（#501）。長時間動かしたときに表示の側が
            // 時間とともに変わったかを、利用者の手元のログ（既定は INFO）から追うため
            if let Some(summary) = self.display_latency.take_summary(now) {
                log_display_summary(&summary);
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
                    if needs_new_texture(self.video_texture.as_ref().map(|t| t.size()), size) {
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
    /// `context` は描画バックエンド（glow / wgpu、#492）のコンテキスト。どちらも無い・環境変数で
    /// 切ってある・シェーダーを使えないときは CPU のまま
    pub(crate) fn init_gpu_yuy2(&mut self, context: GpuContext<'_>) {
        let env = std::env::var(GPU_CONVERT_ENV).ok();
        let preference = self
            .settings
            .lock()
            .map(|settings| settings.video.convert)
            .unwrap_or_default();
        self.gpu_yuy2 = GpuYuy2::init(
            context,
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

    /// 映像を表示領域の中央へ置き、当たり判定を取って描く。表示領域が潰れている間は
    /// 描画も当たり判定も行わず `None`（`video_rect`）。ウィンドウ表示とフルスクリーンで共通
    fn show_video(
        &self,
        ui: &mut egui::Ui,
        texture: &egui::TextureHandle,
    ) -> Option<egui::Response> {
        let rect = video_rect(
            ui.available_size(),
            ui.available_rect_before_wrap().center(),
            texture.size_vec2(),
            self.maintain_aspect_ratio,
        )?;
        let response = ui.allocate_rect(rect, VIDEO_AREA_SENSE);
        self.paint_video(ui, texture, rect);
        Some(response)
    }

    /// 映像エリア（映像が無いときはプレースホルダー）のドラッグでウィンドウを動かす
    /// （`drag_move`）。設定のロックはドラッグ中で帯の外のときだけ取る。ウィンドウ表示だけで使う
    fn handle_drag_move(
        &self,
        ctx: &egui::Context,
        response: &egui::Response,
        on_resize_edge: bool,
    ) {
        drag_move(ctx, response, on_resize_edge, || {
            self.settings
                .lock()
                .ok()
                .map(|settings| settings.ui.enable_drag_move)
        });
    }

    /// 右クリックされたら、ポインターの位置に右クリックメニューを開く
    fn handle_secondary_click_menu(&mut self, ctx: &egui::Context, response: &egui::Response) {
        if let Some(pos) = context_menu_request(ctx, response) {
            self.open_context_menu(ctx, pos);
        }
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
                    let Some(response) = self.show_video(ui, texture) else {
                        return;
                    };

                    self.handle_drag_move(ctx, &response, on_resize_edge);

                    // インタラクションを処理
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, true);
                    }

                    self.handle_secondary_click_menu(ctx, &response);
                    self.handle_middle_click_mute(&response);

                    // 音量調整のためのスクロールを処理
                    if response.hovered() {
                        self.handle_volume_scroll(ctx);
                    }
                } else {
                    let response = show_video_placeholder(ui, available_size, &placeholder);

                    // 空エリアでも映像と同じくドラッグ移動と右クリックメニューを受ける
                    self.handle_drag_move(ctx, &response, on_resize_edge);
                    self.handle_secondary_click_menu(ctx, &response);
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
                    let Some(response) = self.show_video(ui, texture) else {
                        return;
                    };

                    // フルスクリーンではドラッグ移動を完全に無効化
                    // （フルスクリーンでは画面の移動自体が意味をなさないため）

                    // ダブルクリックでウィンドウモードに戻る
                    if response.double_clicked() {
                        self.toggle_fullscreen(ctx, false);
                    }

                    // 右クリックでコンテキストメニュー
                    self.handle_secondary_click_menu(ctx, &response);
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
                    self.handle_secondary_click_menu(ctx, &response);
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

    #[test]
    fn needs_new_texture_only_when_missing_or_resized() {
        assert!(needs_new_texture(None, [1920, 1080]));
        assert!(needs_new_texture(Some([1280, 720]), [1920, 1080]));
        // 同じ大きさなら作り直さない（毎フレーム 8MB の黒を確保しない）
        assert!(!needs_new_texture(Some([1920, 1080]), [1920, 1080]));
    }
}
