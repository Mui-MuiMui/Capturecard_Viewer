//! 録画の開始・停止と、録画スレッドから届いた結果の取り込み。リプレイバッファの設定の反映。
//! 録画中の印と統計 OSD の行。
//!
//! 録画スレッドそのものは `crate::recording` にある。ここは UI スレッドの側で、
//! 右クリックメニューとホットキーから同じ `toggle_recording` を呼ぶ
//! （`docs/design/hotkeys.md` の「ホットキーのアクションは右クリックメニューと同じメソッドを呼ぶ」）。
//!
//! **録画スレッドは直接ログにも画面にも失敗を出さない。** `RecordingEvent` をここで
//! 受け取り、ログと `report_error(ErrorSource::Recording, ..)` を出す
//! （`docs/design/recording.md` の「失敗の扱い」）。

use super::menu::RecordingMenuState;
use super::video_overlay::show_video_overlay;
use super::CaptureCardViewer;
use crate::i18n;
use crate::overlay::OverlayContent;
use crate::recording::{
    format_elapsed, resolve_file_stem, RecordingAudioStats, RecordingEvent, RecordingRequest,
    RecordingSummary, ReplayConfig, ReplayRingStats,
};
use crate::settings::RecordingSettings;
use crate::status::ErrorSource;
use chrono::Local;
use eframe::egui;
use log::{debug, error, info, warn};
use std::time::{Duration, Instant};

/// 録画を保存したことを知らせるトーストを出しておく時間。エラーのトーストと同じ長さ
const SAVED_TOAST_DURATION: Duration = Duration::from_secs(4);

/// 録画中の印（赤い丸）の半径
const INDICATOR_DOT_RADIUS: f32 = 6.0;

/// 録画中の印と画面の端との間隔
const INDICATOR_MARGIN: f32 = 8.0;

impl CaptureCardViewer {
    /// 録画を始める、または止める。右クリックメニューとホットキーが呼ぶ。
    ///
    /// 前の録画の `Finalize` を待っている間は何もしない。差し込み口（`VideoTap`）は
    /// 1 つしか無く、同時に 2 本は録れない。
    pub(super) fn toggle_recording(&mut self) {
        if !self.recorder.is_recording() {
            self.start_recording();
        } else if self.recorder.is_stopping() {
            debug!("前の録画を保存している最中なので、録画を始めない");
        } else {
            info!("録画の停止を頼んだ");
            self.recorder.request_stop();
        }
    }

    /// 設定から録画の要求を組み立て、録画スレッドへ渡す。リプレイバッファが ON なら、
    /// 録画スレッドがリングからさかのぼって書き出す。
    fn start_recording(&mut self) {
        let settings = match self.settings.lock() {
            Ok(settings) => settings.recording.clone(),
            Err(_) => {
                warn!("録画の開始で settings のロックを取得できない");
                return;
            }
        };
        let (file_stem, format_error) =
            resolve_file_stem(&settings.file_name_format, &Local::now());
        if let Some(reason) = format_error {
            warn!(
                "録画のファイル名の書式 \"{}\" は使えないので既定の書式にする: {}",
                settings.file_name_format, reason
            );
        }
        let request = RecordingRequest {
            folder: settings.folder.clone(),
            file_stem,
            video_bitrate_kbps: settings.clamped_bitrate_kbps(),
            hardware_encoder: settings.hardware_encoder,
            // 使うのはエンコーダのレート制御の目安だけ。実際の時間は PTS が決める
            nominal_fps: self.nominal_fps(),
            // 音声を録らない設定なら None（映像だけの MP4）
            audio_bitrate_kbps: settings.audio_bitrate_for_recording(),
        };
        if let Err(reason) = self.recorder.start(request) {
            error!("録画を始められない: {}", reason);
            self.report_error(ErrorSource::Recording, reason.to_string());
        }
    }

    /// 映像の公称 fps。デバイスへ要求した fps。映像が無ければ `None`。
    fn nominal_fps(&self) -> Option<u32> {
        self.device_snapshot
            .active_video
            .as_ref()
            .map(|video| video.requested_fps)
    }

    /// リプレイバッファの設定を録画スレッドへ渡す。`apply_settings`（起動時と 2 秒ごと、
    /// 設定ダイアログの適用）が呼ぶ。**変わったときだけ**送るので、毎回呼んでよい。
    ///
    /// ON にしたらその時点から溜め始め、OFF にしたら（録画していなければ）スレッドごと止める。
    /// 映像の公称 fps（キーフレームの間隔の元）が変わったときもここで伝わる。
    pub(super) fn sync_replay_buffer(&mut self, settings: &RecordingSettings) {
        let config = settings
            .replay_seconds_for_recording()
            .map(|seconds| ReplayConfig {
                seconds,
                video_bitrate_kbps: settings.clamped_bitrate_kbps(),
                hardware_encoder: settings.hardware_encoder,
                audio_bitrate_kbps: settings.audio_bitrate_for_recording(),
                nominal_fps: self.nominal_fps(),
            });
        if let Err(reason) = self.recorder.set_replay(config) {
            error!("リプレイバッファを始められない: {}", reason);
            self.report_error(
                ErrorSource::Recording,
                i18n::recording_replay_failed(reason),
            );
        }
    }

    /// 録画スレッドから届いた結果を取り込む。`update()` の先頭で呼ぶ。
    ///
    /// 最小化中に起きた失敗は、復帰してここを通ったときに出る。録画スレッド自身は
    /// 最小化に関係なく止まる。
    pub(super) fn drain_recording_events(&mut self) {
        while let Some(event) = self.recorder.try_recv() {
            match event {
                RecordingEvent::Started => {
                    info!("録画スレッドが録画を始めた");
                    self.errors.clear(ErrorSource::Recording);
                }
                // 名前はログに録画スレッドが残している。統計 OSD は `Recorder` から読む
                RecordingEvent::EncoderSelected(_) => {}
                RecordingEvent::Stopped(summary) => {
                    log_summary("録画を保存した", &summary, self.recorder.replay_ring());
                    let name = summary
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.transient_overlay.show(
                        OverlayContent::Text(i18n::recording_saved(name)),
                        SAVED_TOAST_DURATION,
                        Instant::now(),
                    );
                }
                RecordingEvent::Failed { error, summary } => {
                    error!("録画を始められなかった、または途中で止まった: {}", error);
                    if let Some(summary) = &summary {
                        log_summary(
                            "止まるまでの録画は保存した",
                            summary,
                            self.recorder.replay_ring(),
                        );
                    }
                    self.report_error(ErrorSource::Recording, error.to_string());
                }
                RecordingEvent::ReplayFailed(reason) => {
                    error!("リプレイバッファを続けられない: {}", reason);
                    self.report_error(
                        ErrorSource::Recording,
                        i18n::recording_replay_failed(reason),
                    );
                }
            }
        }
    }

    /// 終了時に録画を止め、`Finalize` が終わるまで待つ。リプレイバッファも止める。
    /// **デバイスワーカーを止める前に呼ぶ。**
    pub(super) fn stop_recording_for_exit(&mut self) {
        if self.recorder.is_recording() {
            info!("終了するので録画を止める");
        }
        // 画面はもう出ないが、結果はログに残す
        for event in self.recorder.shutdown() {
            match event {
                RecordingEvent::Stopped(summary) => {
                    log_summary("録画を保存した", &summary, None);
                }
                RecordingEvent::Failed { error, summary } => {
                    error!("終了時に録画を閉じられなかった: {}", error);
                    if let Some(summary) = &summary {
                        log_summary("止まるまでの録画は保存した", summary, None);
                    }
                }
                RecordingEvent::ReplayFailed(reason) => {
                    error!("リプレイバッファを続けられなかった: {}", reason);
                }
                RecordingEvent::Started | RecordingEvent::EncoderSelected(_) => {}
            }
        }
    }

    /// 右クリックメニューに出す録画の項目の状態。
    pub(super) fn recording_menu_state(&self) -> RecordingMenuState {
        if !self.recorder.is_recording() {
            RecordingMenuState::Idle
        } else if self.recorder.is_stopping() {
            RecordingMenuState::Finishing
        } else {
            RecordingMenuState::Recording(self.recorder.elapsed())
        }
    }

    /// 統計 OSD に足す録画の行。録画していなければ空（リプレイバッファが ON なだけでは
    /// 何も出さない。#182 の決定）。
    pub(super) fn recording_stats_lines(&self) -> Vec<String> {
        let recorder = &self.recorder;
        if !recorder.is_recording() {
            return Vec::new();
        }
        let mut first = i18n::stats_recording(
            format_elapsed(recorder.elapsed()),
            recorder.frames_written(),
            recorder.frames_dropped(),
        );
        first.push_str(&replay_suffix(recorder.replay_lead()));
        let mut lines = vec![first];
        if let Some(encoder) = recorder.encoder() {
            lines.push(i18n::stats_recording_encoder(
                encoder.name.as_deref(),
                encoder.hardware,
            ));
        }
        lines.extend(audio_stats_line(recorder.audio_stats()));
        lines
    }

    /// 録画中の印を右上に描く。**統計 OSD とは別に、録画中は常に出す**（フルスクリーンでも）。
    ///
    /// 録画の失敗で一番困るのは「録っているつもりで止まっていた」「止め忘れた」なので、
    /// 情報表示を切っていても見えるようにする。経過時間の更新は 1 秒ごとでよく、
    /// 再描画の間隔（`next_repaint_delay`）は変えない。映像が来ていれば 16ms、
    /// 来ていなければ 250ms で回っている。
    pub(super) fn draw_recording_indicator(&self, ctx: &egui::Context) {
        if !self.recorder.is_recording() {
            return;
        }
        let elapsed = format_elapsed(self.recorder.elapsed());
        // 設定ダイアログより下に描く（#284）。理由は `show_video_overlay` にある
        show_video_overlay(
            ctx,
            egui::Id::new("recording_indicator"),
            ctx.screen_rect().shrink(INDICATOR_MARGIN),
            egui::Align2::RIGHT_TOP,
            |ui| {
                // 統計 OSD と同じ半透明の黒地に白の文字（映像の上で読めるように）
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::Vec2::splat(INDICATOR_DOT_RADIUS * 2.0),
                                egui::Sense::hover(),
                            );
                            // 固定色でよい: 映像の上の OSD で統計 OSD の白と同じ扱い（warning_label 系は設定ダイアログ用）
                            ui.painter().circle_filled(
                                rect.center(),
                                INDICATOR_DOT_RADIUS,
                                egui::Color32::from_rgb(0xe5, 0x39, 0x35),
                            );
                            ui.label(
                                egui::RichText::new(&elapsed)
                                    .monospace()
                                    .color(egui::Color32::WHITE),
                            );
                        });
                    });
            },
        );
    }
}

/// 統計 OSD の録画の 1 行目に添える、さかのぼった長さ。リプレイバッファを通していなければ空。
fn replay_suffix(lead: Option<Duration>) -> String {
    lead.map(|lead| i18n::stats_recording_replay(lead.as_secs()))
        .unwrap_or_default()
}

/// 統計 OSD の録画の音声の行。音声を録らない設定（`None`）なら出さない。
/// 値が 0 でも出す（「音声を録っていて、揃え直しも溢れも起きていない」ことが分かるように）。
fn audio_stats_line(stats: Option<RecordingAudioStats>) -> Option<String> {
    let stats = stats?;
    Some(i18n::stats_recording_audio(
        stats.silence_ms,
        stats.trimmed_ms,
        stats.overflows,
    ))
}

/// 閉じた録画の内容をログへ 1 行で残す。回収に失敗した回数は、生データを渡す方式へ
/// 切り替えるかの判断材料（`docs/design/recording.md`）。リプレイバッファを通した録画なら、
/// さかのぼった長さとリングの状態（持っている長さ、古い GOP を捨てた回数）も残す。
fn log_summary(prefix: &str, summary: &RecordingSummary, ring: Option<ReplayRingStats>) {
    let replay = match (summary.replay_lead, ring) {
        (Some(lead), Some(ring)) => format!(
            "、さかのぼり {:.1} 秒（リングの映像 {:.1} 秒、捨てた GOP {} 回）",
            lead.as_secs_f64(),
            ring.held.as_secs_f64(),
            ring.discarded_gops
        ),
        (Some(lead), None) => format!("、さかのぼり {:.1} 秒", lead.as_secs_f64()),
        (None, _) => String::new(),
    };
    info!(
        "{}: {}（長さ {}、書いた {} 枚、捨てた {} 枚、Vec の回収に失敗した回数 {}{}）",
        prefix,
        summary.path.display(),
        format_elapsed(summary.duration),
        summary.frames_written,
        summary.frames_dropped,
        summary.recycle_misses,
        replay
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{with_language, Language};

    #[test]
    fn audio_stats_line_is_absent_when_audio_is_off() {
        assert_eq!(audio_stats_line(None), None);
    }

    #[test]
    fn audio_stats_line_shows_zero_values() {
        let line = with_language(Language::Japanese, || {
            audio_stats_line(Some(RecordingAudioStats::default()))
        });
        assert_eq!(line.as_deref(), Some("音声: 無音 +0 ms / 溢れ 0 回"));
    }

    #[test]
    fn audio_stats_line_shows_silence_trim_and_overflows() {
        let stats = RecordingAudioStats {
            silence_ms: 1_250,
            trimmed_ms: 6,
            overflows: 3,
        };
        let line = with_language(Language::Japanese, || audio_stats_line(Some(stats)));
        assert_eq!(
            line.as_deref(),
            Some("音声: 無音 +1250 ms / 削除 -6 ms / 溢れ 3 回")
        );
    }

    #[test]
    fn replay_suffix_is_empty_without_the_replay_buffer() {
        assert_eq!(replay_suffix(None), "");
    }

    #[test]
    fn replay_suffix_shows_whole_seconds() {
        let text = with_language(Language::Japanese, || {
            replay_suffix(Some(Duration::from_millis(28_900)))
        });
        assert_eq!(text, " / さかのぼり 28 秒");
    }
}
