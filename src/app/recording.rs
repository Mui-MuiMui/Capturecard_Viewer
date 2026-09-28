//! 録画の開始・停止と、録画スレッドから届いた結果の取り込み。録画中の印と統計 OSD の行。
//!
//! 録画スレッドそのものは `crate::recording` にある。ここは UI スレッドの側で、
//! 右クリックメニューとホットキーから同じ `toggle_recording` を呼ぶ
//! （`docs/design/hotkeys.md` の「ホットキーのアクションは右クリックメニューと同じメソッドを呼ぶ」）。
//!
//! **録画スレッドは直接ログにも画面にも失敗を出さない。** `RecordingEvent` をここで
//! 受け取り、ログと `report_error(ErrorSource::Recording, ..)` を出す
//! （`docs/design/recording.md` の「失敗の扱い」）。

use super::menu::RecordingMenuState;
use super::CaptureCardViewer;
use crate::i18n;
use crate::overlay::OverlayContent;
use crate::recording::{
    format_elapsed, resolve_file_stem, Recorder, RecordingEvent, RecordingRequest, RecordingSummary,
};
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
        match self.recorder.as_mut() {
            Some(recorder) if !recorder.is_stopping() => {
                info!("録画の停止を頼んだ");
                recorder.request_stop();
            }
            Some(_) => debug!("前の録画を保存している最中なので、録画を始めない"),
            None => self.start_recording(),
        }
    }

    /// 設定から録画の要求を組み立て、録画スレッドを起こす。
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
            nominal_fps: self
                .device_snapshot
                .active_video
                .as_ref()
                .map(|video| video.requested_fps),
            // 設定（[recording] の音声の項目）は次の段で足す。それまでは既定の 160kbps で録る
            audio_bitrate_kbps: Some(160),
        };
        match Recorder::start(request, self.frames.tap(), self.audio_tap.clone()) {
            Ok(recorder) => self.recorder = Some(recorder),
            Err(reason) => {
                error!("録画を始められない: {}", reason);
                self.report_error(ErrorSource::Recording, reason.to_string());
            }
        }
    }

    /// 録画スレッドから届いた結果を取り込む。`update()` の先頭で呼ぶ。
    ///
    /// 最小化中に起きた失敗は、復帰してここを通ったときに出る。録画スレッド自身は
    /// 最小化に関係なく止まる。
    pub(super) fn drain_recording_events(&mut self) {
        loop {
            let Some(event) = self.recorder.as_mut().and_then(Recorder::try_recv) else {
                return;
            };
            match event {
                RecordingEvent::Started => {
                    info!("録画スレッドが映像を待ち始めた");
                    self.errors.clear(ErrorSource::Recording);
                }
                // 名前はログに録画スレッドが残している。統計 OSD は `Recorder` から読む
                RecordingEvent::EncoderSelected(_) => {}
                RecordingEvent::Stopped(summary) => {
                    log_summary("録画を保存した", &summary);
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
                    self.finish_recorder();
                    return;
                }
                RecordingEvent::Failed { error, summary } => {
                    error!("録画を始められなかった、または途中で止まった: {}", error);
                    if let Some(summary) = &summary {
                        log_summary("止まるまでの録画は保存した", summary);
                    }
                    self.report_error(ErrorSource::Recording, error.to_string());
                    self.finish_recorder();
                    return;
                }
            }
        }
    }

    /// 終わった録画スレッドを片付ける。スレッドはイベントを送った直後に抜けるので、
    /// join はすぐ返る。
    fn finish_recorder(&mut self) {
        // `Recorder` を落とすと join する（スレッドを切り離さない）
        self.recorder = None;
    }

    /// 終了時に録画を止め、`Finalize` が終わるまで待つ。**デバイスワーカーを止める前に呼ぶ。**
    pub(super) fn stop_recording_for_exit(&mut self) {
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        info!("終了するので録画を止める");
        // 画面はもう出ないが、結果はログに残す
        for event in recorder.stop_and_wait() {
            match event {
                RecordingEvent::Stopped(summary) => log_summary("録画を保存した", &summary),
                RecordingEvent::Failed { error, summary } => {
                    error!("終了時に録画を閉じられなかった: {}", error);
                    if let Some(summary) = &summary {
                        log_summary("止まるまでの録画は保存した", summary);
                    }
                }
                RecordingEvent::Started | RecordingEvent::EncoderSelected(_) => {}
            }
        }
    }

    /// 右クリックメニューに出す録画の項目の状態。
    pub(super) fn recording_menu_state(&self) -> RecordingMenuState {
        match &self.recorder {
            None => RecordingMenuState::Idle,
            Some(recorder) if recorder.is_stopping() => RecordingMenuState::Finishing,
            Some(recorder) => RecordingMenuState::Recording(recorder.elapsed()),
        }
    }

    /// 統計 OSD に足す録画の行。録画していなければ空。
    pub(super) fn recording_stats_lines(&self) -> Vec<String> {
        let Some(recorder) = &self.recorder else {
            return Vec::new();
        };
        let mut lines = vec![i18n::stats_recording(
            format_elapsed(recorder.elapsed()),
            recorder.frames_written(),
            recorder.frames_dropped(),
        )];
        if let Some(encoder) = recorder.encoder() {
            lines.push(i18n::stats_recording_encoder(
                encoder.name.as_deref(),
                encoder.hardware,
            ));
        }
        lines
    }

    /// 録画中の印を右上に描く。**統計 OSD とは別に、録画中は常に出す**（フルスクリーンでも）。
    ///
    /// 録画の失敗で一番困るのは「録っているつもりで止まっていた」「止め忘れた」なので、
    /// 情報表示を切っていても見えるようにする。経過時間の更新は 1 秒ごとでよく、
    /// 再描画の間隔（`next_repaint_delay`）は変えない。映像が来ていれば 16ms、
    /// 来ていなければ 250ms で回っている。
    pub(super) fn draw_recording_indicator(&self, ctx: &egui::Context) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let elapsed = format_elapsed(recorder.elapsed());
        egui::Area::new(egui::Id::new("recording_indicator"))
            .order(egui::Order::Foreground)
            .anchor(
                egui::Align2::RIGHT_TOP,
                egui::vec2(-INDICATOR_MARGIN, INDICATOR_MARGIN),
            )
            // 映像のドラッグや右クリックを吸わないようにする
            .interactable(false)
            .show(ctx, |ui| {
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
                                egui::RichText::new(elapsed)
                                    .monospace()
                                    .color(egui::Color32::WHITE),
                            );
                        });
                    });
            });
    }
}

/// 閉じた録画の内容をログへ 1 行で残す。回収に失敗した回数は、生データを渡す方式へ
/// 切り替えるかの判断材料（`docs/design/recording.md`）。
fn log_summary(prefix: &str, summary: &RecordingSummary) {
    info!(
        "{}: {}（長さ {}、書いた {} 枚、捨てた {} 枚、Vec の回収に失敗した回数 {}）",
        prefix,
        summary.path.display(),
        format_elapsed(summary.duration),
        summary.frames_written,
        summary.frames_dropped,
        summary.recycle_misses
    );
}
