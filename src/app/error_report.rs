//! 失敗の記録と、画面への出し方。
//!
//! 発生源ごとの直近の失敗は `crate::status::ErrorCenter` が持つ。ここは
//! そこへ記録してトーストを出す側と、映像のプレースホルダーや設定ダイアログの
//! 「接続状態」タブへ渡す形に整える側をまとめてある。

use super::CaptureCardViewer;
use crate::i18n;
use crate::overlay::OverlayContent;
use crate::status::{self, ConnectionStatus, ErrorSource, LinkStatus};
use chrono::Local;
use std::time::{Duration, Instant};

/// エラーをトーストで見せておく時間。
///
/// 音量 OSD（1.5 秒）より長い。音量は自分で操作した結果の確認なので一瞬で
/// よいが、エラーは予期していない内容を読ませるため。
const ERROR_TOAST_DURATION: Duration = Duration::from_secs(4);

impl CaptureCardViewer {
    /// 失敗を記録し、必要ならトーストで見せる。
    ///
    /// **ログは呼び出し側が従来どおり出す。** ここは画面へ出すための記録で、
    /// `error!` / `warn!` の置き換えではない。同じ発生源で同じ文言が続く間は
    /// `ErrorCenter` が間引くため、接続の再試行で連打にならない。
    ///
    /// 同じフレームで複数の発生源が失敗した場合、トーストは後に記録したものが
    /// 勝つ（`TransientOverlay` は 1 件しか持たない）。**どれを見せるかを
    /// 優先度で決めない。** 全てログと「接続状態」タブに残っており、
    /// 消えたほうも次の再試行でまた記録されるため。
    ///
    /// フェイクデバイスの知らせ（#252）をこの置き場に入れないのも同じ理由による。
    /// 入れると直後の接続失敗に上書きされて見えないので、`view.rs` の
    /// `draw_fake_devices_banner` が常設の帯として別に描く。
    pub(super) fn report_error(&mut self, source: ErrorSource, message: String) {
        let notify = self
            .errors
            .record(source, message, Instant::now(), Local::now());
        if !notify {
            return;
        }
        // 上で記録したので必ず取れる
        let Some(recorded) = self.errors.latest(source) else {
            return;
        };
        let text = status::truncate(
            &status::format_message(source, &recorded.message),
            status::TOAST_MESSAGE_LIMIT,
        );
        self.transient_overlay.show(
            OverlayContent::Text(text),
            ERROR_TOAST_DURATION,
            Instant::now(),
        );
    }

    /// 発生源の直近の失敗を、映像プレースホルダーへ添える 1 行にする。
    pub(super) fn error_detail(&self, source: ErrorSource) -> Option<String> {
        let recorded = self.errors.latest(source)?;
        Some(status::truncate(
            &status::format_message(source, &recorded.message),
            status::PLACEHOLDER_DETAIL_LIMIT,
        ))
    }

    /// 設定ダイアログの「接続状態」タブへ渡す観測値を作る。
    ///
    /// **デバイスへは問い合わせない。** ワーカーが定期的に更新している
    /// 観測値（`DeviceSnapshot`）を `update()` の先頭で 1 回読んであり、
    /// ここはその複製を組み替えるだけ。
    pub(super) fn connection_status(&self) -> ConnectionStatus {
        let active_video = self.device_snapshot.active_video.clone();
        let active_audio = self.device_snapshot.active_audio.clone();

        let mut video = LinkStatus {
            connected: active_video.is_some(),
            reconnecting: self.device_snapshot.video_retry.active,
            attempts: self.device_snapshot.video_retry.attempts,
            details: Vec::new(),
            error: self.status_error(ErrorSource::Video),
        };
        if let Some(active) = active_video {
            video.details.push(i18n::link_device(&active.device_name));
            // 開き方が自動でも、どちらで開いたかを確かめられるように出す（#237）
            video
                .details
                .push(i18n::link_capture_api(active.api.label()));
            video.details.push(i18n::link_video(active.summary()));
            // 実際の fps はデバイスから取れない（video/capture.rs の ActiveVideo の説明を参照）
            video
                .details
                .push(i18n::link_requested_fps(active.requested_fps));
        }

        let mut audio = LinkStatus {
            connected: active_audio.is_some(),
            reconnecting: self.device_snapshot.audio_retry.active,
            attempts: self.device_snapshot.audio_retry.attempts,
            details: Vec::new(),
            error: self.status_error(ErrorSource::Audio),
        };
        if let Some(active) = active_audio {
            audio.details.push(i18n::link_audio_input(
                &active.input_device,
                active.input_summary(),
            ));
            audio.details.push(i18n::link_audio_output(
                &active.output_device,
                active.output_summary(),
            ));
            audio.details.extend(status::format_resample_status(
                self.device_snapshot.audio_resample,
            ));
            audio.details.push(status::format_underrun_count(
                self.device_snapshot.audio_underruns,
            ));
            audio.details.push(status::format_dropped_frame_count(
                self.device_snapshot.audio_dropped_frames,
            ));
            audio
                .details
                .push(i18n::xrun_count(self.device_snapshot.audio_xruns));
        }

        ConnectionStatus {
            fake_devices: self.device.fake_devices(),
            video,
            audio,
            screenshot_sound_error: self.status_error(ErrorSource::ScreenshotSound),
        }
    }

    /// 「接続状態」タブに出す直近の失敗。`(整形済みの文言, 発生時刻)`。
    fn status_error(&self, source: ErrorSource) -> Option<(String, String)> {
        let recorded = self.errors.latest(source)?;
        Some(status::recorded_error_line(source, recorded))
    }
}
