//! 映像の左上に重ねる統計 OSD（情報表示）。何を出すか（`format_stats_lines`）と、
//! その描画（`show_stats_overlay`）。
//!
//! どの層へ描くかは `super::video_overlay` が持つ。値は描く直前に `VideoFrames::stats` と
//! ワーカーが書き出した観測値の複製から読み、デバイスへは問い合わせない。

use super::video_overlay::show_video_overlay;
use super::CaptureCardViewer;
use crate::i18n::{self, Text};
use crate::video::{format_display_latency, FrameStats};
use eframe::egui;
use std::time::Instant;

/// 統計オーバーレイを画面の左上からどれだけ離して置くか。
const STATS_OVERLAY_MARGIN: f32 = 8.0;

/// 統計オーバーレイに出す行を組み立てる。
///
/// 値が取れていない項目は数値を出さずに「-」や「なし」にする。
/// フレームが 1 枚も来ていない状態で平均を出そうとすると NaN や
/// 無限大になり、それがそのまま画面に出てしまうため。
///
/// `audio_line` は音声のアンダーランの行（`status::format_osd_audio_line`）。映像の統計では
/// ないが、**バッファ長を詰めたときに音が途切れていないかを、設定画面を開かずに
/// 見られるようにする**ためにここへ並べてある。`latency_line` は表示までの遅れの行
/// （`video::format_display_latency`、#455）で、映像の行の最後に置く。
fn format_stats_lines(stats: &FrameStats, latency_line: String, audio_line: String) -> Vec<String> {
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
    lines.push(latency_line);

    // 文言は「接続状態」タブと共通（`status::format_underrun_count`）。経路の印だけ OSD で足す
    lines.push(audio_line);

    lines
}

impl CaptureCardViewer {
    /// 統計 OSD に出す描画のバックエンドの説明を入れる。起動経路（`main.rs`）が
    /// 作った直後に 1 回だけ呼ぶ（`renderer::RendererChoice::label`）
    pub(crate) fn set_renderer_label(&mut self, label: String) {
        self.renderer_label = Some(label);
    }

    /// 映像の統計を左上へ半透明で重ねて描く。
    ///
    /// 統計の取り出しは 1 フレームにつきこの 1 回だけ。ロックの中では
    /// 値のコピーと最大 120 要素の集計しか起きないため、毎フレーム呼んでよい。
    ///
    /// 描いた枠の下端の y 座標を返す。フェイクデバイスの帯をその下へずらすため
    /// （`super::view` の `fake_devices_banner_top`）。
    pub(super) fn show_stats_overlay(&self, ctx: &egui::Context) -> f32 {
        let stats = self.frames.stats();
        // ワーカーが書き出した観測値の複製。ここでデバイスへは問い合わせない
        let latency = format_display_latency(self.display_latency.recent(Instant::now()));
        let mut lines = format_stats_lines(&stats, latency, self.device_snapshot.osd_audio_line());
        // どちらの描画バックエンドで描いているか（#456 の (2)）。撮り比べで取り違えないため
        if let Some(label) = &self.renderer_label {
            lines.push(i18n::stats_renderer(label));
        }
        // 録画中は録画の行を足す（経過時間、書いた枚数・捨てた枚数、エンコーダ）
        lines.extend(self.recording_stats_lines());

        // 設定ダイアログより下に描く（#284）。理由は `show_video_overlay` にある
        let screen = ctx.content_rect();
        let area = egui::Rect::from_min_max(
            screen.min + egui::Vec2::splat(STATS_OVERLAY_MARGIN),
            screen.max,
        );
        let shown = show_video_overlay(
            ctx,
            egui::Id::new("stats_overlay"),
            area,
            egui::Align2::LEFT_TOP,
            |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_black_alpha(160))
                    .corner_radius(4)
                    .inner_margin(egui::Margin::same(6))
                    .show(ui, |ui| {
                        for line in &lines {
                            ui.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .color(egui::Color32::WHITE),
                            );
                        }
                    });
            },
        );
        shown.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status;
    use crate::video::frame_buffer::IntervalStats;

    #[test]
    fn format_stats_lines_without_frames_shows_no_numbers() {
        // デバイスに接続できていない状態。0 除算の結果や NaN を
        // そのまま画面へ出さないことを確かめる
        let lines = format_stats_lines(
            &FrameStats::default(),
            format_display_latency(None),
            status::format_underrun_count(None),
        );
        assert!(lines.contains(&Text::StatsDisplayLatencyPending.get().to_string()));
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

        // 到着から 3.2ms でテクスチャへ取り込んだ 1 枚
        let mut latency = crate::video::DisplayLatency::default();
        let now = Instant::now();
        latency.record(now, std::time::Duration::from_micros(3_200), 0);
        let latency_line = format_display_latency(latency.recent(now));
        let lines =
            format_stats_lines(&stats, latency_line, status::format_underrun_count(Some(3)));
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
        assert!(joined.contains("平均 3.2ms / 最大 3.2ms"), "{}", joined);
    }
}
