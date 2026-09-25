//! 映像の描画と、その上での操作の受け付け。
//!
//! ウィンドウ表示とフルスクリーンで同じことを行うため、どちらの描画も
//! ここに並べてある。ウィンドウそのものの操作（装飾・リサイズ・
//! フルスクリーン切替）は `super::window`、右クリックメニューは `super::menu`。

use super::CaptureCardViewer;
use crate::i18n::{self, Text};
use crate::status::{self, ErrorSource};
use crate::video::FrameStats;
use eframe::egui;
use log::warn;

/// 映像が出ていないときに画面へ出す文言を決める。
///
/// 「デバイスは開けているが信号が来ていない」と「デバイスそのものが消えた」は
/// ユーザーの取るべき行動が違う（入力機器の電源を見るのか、ケーブルを挿し直すのか）
/// ため、同じ文言にしない。
fn video_placeholder_message(capturing: bool, reconnecting: bool) -> &'static str {
    let text = match (capturing, reconnecting) {
        (true, _) => Text::PlaceholderNoSignal,
        (false, true) => Text::PlaceholderReconnecting,
        (false, false) => Text::PlaceholderDisconnected,
    };
    text.get()
}

/// 映像が出ていないときに画面へ出す文言を、理由の 1 行を添えて組み立てる。
///
/// `detail` は直近の失敗（`ErrorCenter` に記録されたもの）。**ストリームを
/// 開けている場合は添えない。** 映像信号が来ていないのはデバイスの手前の
/// 問題で、そこに古い接続エラーを出すと原因を取り違えさせる。
///
/// 理由の切り詰めは呼び出し側（`error_detail`）が済ませてある。ここで
/// 長さを見ないのは、切り詰めの基準を 1 か所に集めておくため。
fn video_placeholder_text(capturing: bool, reconnecting: bool, detail: Option<&str>) -> String {
    let head = video_placeholder_message(capturing, reconnecting);
    match detail {
        Some(detail) if !capturing => format!("{}\n{}", head, detail),
        _ => head.to_string(),
    }
}

/// 統計オーバーレイに出す行を組み立てる。
///
/// 値が取れていない項目は数値を出さずに「-」や「なし」にする。
/// フレームが 1 枚も来ていない状態で平均を出そうとすると NaN や
/// 無限大になり、それがそのまま画面に出てしまうため。
///
/// `audio_underruns` は `DeviceSnapshot.audio_underruns`。音声のアンダーランは
/// 映像の統計ではないが、**バッファ長を詰めたときに音が途切れていないかを、
/// 設定画面を開かずに見られるようにする**ためにここへ並べてある。
fn format_stats_lines(stats: &FrameStats, audio_underruns: Option<u32>) -> Vec<String> {
    let mut lines = Vec::new();

    match stats.intervals {
        Some(intervals) => {
            lines.push(i18n::stats_fps(
                intervals.fps,
                intervals.average_ms,
                intervals.samples,
            ));
            lines.push(i18n::stats_jitter(
                intervals.stddev_ms,
                intervals.min_ms,
                intervals.max_ms,
            ));
        }
        None => lines.push(Text::StatsFpsPending.get().to_string()),
    }

    match (stats.resolution, stats.source_format) {
        (Some((width, height)), Some(format)) => {
            // フレームが 1 枚でも届いていれば、変換の計測値は実測値
            lines.push(i18n::stats_decode(
                stats.last_decode_ms,
                stats.fast_count,
                stats.fallback_count,
            ));
            lines.push(format!("{}x{} {}", width, height, format));
        }
        _ => {
            // 計測前の 0 を実測値と読み違えられないようにする
            lines.push(Text::StatsDecodeUnknown.get().to_string());
            lines.push(Text::StatsNoFrame.get().to_string());
        }
    }

    if let Some(elapsed_ms) = stats.since_last_frame_ms {
        lines.push(i18n::stats_since_last_frame(elapsed_ms));
    }

    // 文言は「接続状態」タブと共通（`status::format_underrun_count`）
    lines.push(status::format_underrun_count(audio_underruns));

    lines
}

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

impl CaptureCardViewer {
    /// 新着フレームがあればテクスチャへ取り込む。取り込んだら `true`。
    ///
    /// **ここで再描画を予約しない。** 予約は `update()` の末尾で 1 か所にまとめる。
    /// 以前はここで無条件に 16ms（60fps）の再描画を予約していたため、映像が
    /// 来ていなくても、最小化していても描き続けていた（Issue #98）。
    pub(super) fn update_video_texture(&mut self, ctx: &egui::Context) -> bool {
        // 新着フレームが無ければ何もしない。既存のテクスチャをそのまま使い回す。
        // **フレームだけはワーカーのチャネルを通さない。** コマンドの列に
        // 並べると、接続や列挙の後ろで待たされて遅延が増える。
        //
        // **ロックが取れないときの警告も持たない。** `VideoFrames` の中で
        // 失敗を握り潰して「新着なし」に倒すだけなので、毎フレーム呼ばれる
        // この経路からログが出ることはない
        let new_frame = self.frames.newer_than(self.last_frame_generation);

        if let Some((frame, generation)) = new_frame {
            self.last_frame_generation = generation;

            // 最適化: テクスチャオプションをNearest（補間なし）に設定し、性能向上
            let texture_options = egui::TextureOptions {
                magnification: egui::TextureFilter::Nearest,
                minification: egui::TextureFilter::Linear,
                wrap_mode: egui::TextureWrapMode::ClampToEdge,
            };

            let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.data);
            if let Some(texture) = &mut self.video_texture {
                texture.set(image, texture_options);
            } else {
                self.video_texture = Some(ctx.load_texture("video_frame", image, texture_options));
            }

            return true;
        }

        false
    }

    pub(super) fn show_windowed_ui(&mut self, ctx: &egui::Context) {
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
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(2.0))) // マージンを2pxに設定
            .show(ctx, |ui| {
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

                    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)),
                        egui::Color32::WHITE,
                    );

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
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label(placeholder);
                    });

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

    pub(super) fn show_fullscreen_ui(&mut self, ctx: &egui::Context) {
        // ウィンドウ表示と同じ理由で、描画に入る前に文言を決める
        let placeholder = video_placeholder_text(
            self.device_snapshot.video_capturing,
            self.device_snapshot.video_retry.active,
            self.error_detail(ErrorSource::Video).as_deref(),
        );
        // フルスクリーンUI（装飾なし、ウィンドウ版と同等の機能）
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(0.0))) // フルスクリーンはマージン0
            .show(ctx, |ui| {
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

                    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(1.0)),
                        egui::Color32::WHITE,
                    );

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
                    let response =
                        ui.allocate_response(available_size, egui::Sense::click_and_drag());
                    ui.centered_and_justified(|ui| {
                        ui.label(placeholder);
                    });

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

    /// 映像の統計を左上へ半透明で重ねて描く。
    ///
    /// 統計の取り出しは 1 フレームにつきこの 1 回だけ。ロックの中では
    /// 値のコピーと最大 120 要素の集計しか起きないため、毎フレーム呼んでよい。
    pub(super) fn show_stats_overlay(&self, ctx: &egui::Context) {
        let stats = self.frames.stats();
        // ワーカーが書き出した観測値の複製。ここでデバイスへは問い合わせない
        let audio_underruns = self.device_snapshot.audio_underruns;

        egui::Area::new("stats_overlay")
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(8.0, 8.0))
            // 映像のドラッグや右クリックを吸わないようにする
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        for line in format_stats_lines(&stats, audio_underruns) {
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .color(egui::Color32::WHITE),
                            );
                        }
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::frame_buffer::IntervalStats;
    use egui::Vec2;

    #[test]
    fn format_stats_lines_without_frames_shows_no_numbers() {
        // デバイスに接続できていない状態。0 除算の結果や NaN を
        // そのまま画面へ出さないことを確かめる
        let lines = format_stats_lines(&FrameStats::default(), None);
        let joined = lines.join(
            "
",
        );

        assert!(joined.contains("FPS -"), "FPS が出ていない: {}", joined);
        assert!(
            joined.contains("デコード -"),
            "計測前の 0 を数値で出している: {}",
            joined
        );
        assert!(joined.contains("映像フレームなし"), "{}", joined);
        assert!(
            !joined.contains("NaN"),
            "NaN が表示に混ざっている: {}",
            joined
        );
        assert!(
            !joined.contains("inf"),
            "inf が表示に混ざっている: {}",
            joined
        );
        assert!(
            !joined.contains("最終フレーム"),
            "フレームが無いのに経過時間が出ている: {}",
            joined
        );
        // 音声を開いていないときに 0 と出すと、開いていて一度も
        // 途切れていない状態と区別が付かない
        assert!(joined.contains("アンダーラン: -"), "{}", joined);
    }

    #[test]
    fn format_stats_lines_with_frames_shows_all_items() {
        // 60fps 相当で動いている状態
        let stats = FrameStats {
            intervals: Some(IntervalStats {
                fps: 60.0,
                average_ms: 16.6667,
                min_ms: 15.0,
                max_ms: 18.0,
                stddev_ms: 1.25,
                samples: 120,
            }),
            last_decode_ms: 2.5,
            fast_count: 1200,
            fallback_count: 3,
            resolution: Some((1920, 1080)),
            source_format: Some("YUY2"),
            since_last_frame_ms: Some(12.4),
        };

        let lines = format_stats_lines(&stats, Some(3));
        let joined = lines.join(
            "
",
        );

        assert!(joined.contains("FPS 60.0"), "{}", joined);
        assert!(joined.contains("120 件"), "{}", joined);
        assert!(joined.contains("±1.25ms"), "{}", joined);
        assert!(joined.contains("最小 15.0 / 最大 18.0"), "{}", joined);
        assert!(joined.contains("デコード 2.50ms"), "{}", joined);
        assert!(joined.contains("高速 1200 / 汎用 3"), "{}", joined);
        assert!(joined.contains("1920x1080 YUY2"), "{}", joined);
        assert!(joined.contains("最終フレーム 12ms 前"), "{}", joined);
        assert!(joined.contains("アンダーラン: 3 回"), "{}", joined);
    }

    #[test]
    fn video_placeholder_message_capturing_says_no_signal() {
        // デバイスは開けている。ユーザーが見るべきは入力機器側
        assert_eq!(
            video_placeholder_message(true, false),
            "映像信号がありません"
        );
        // 開けている間は再接続の有無で文言を変えない
        assert_eq!(
            video_placeholder_message(true, true),
            "映像信号がありません"
        );
    }

    #[test]
    fn video_placeholder_message_not_capturing_says_device_is_gone() {
        assert_eq!(
            video_placeholder_message(false, false),
            "デバイスが接続されていません"
        );
        assert_eq!(
            video_placeholder_message(false, true),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn video_placeholder_text_without_detail_is_the_message_alone() {
        assert_eq!(
            video_placeholder_text(false, true, None),
            "デバイスが接続されていません（再接続を試しています）"
        );
    }

    #[test]
    fn video_placeholder_text_adds_the_reason_on_a_second_line() {
        assert_eq!(
            video_placeholder_text(false, true, Some("映像デバイスに接続できません: not found")),
            "デバイスが接続されていません（再接続を試しています）\n映像デバイスに接続できません: not found"
        );
    }

    #[test]
    fn video_placeholder_text_while_capturing_drops_the_reason() {
        // ストリームは開けている＝接続の失敗ではない。古い接続エラーを
        // 出すと、入力機器ではなく USB を疑わせてしまう
        assert_eq!(
            video_placeholder_text(true, false, Some("映像デバイスに接続できません: not found")),
            "映像信号がありません"
        );
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
