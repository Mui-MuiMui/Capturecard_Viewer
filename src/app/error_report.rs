//! 失敗の記録と、画面への出し方。
//!
//! 発生源ごとの直近の失敗は `crate::status::ErrorCenter` が持つ。ここは
//! そこへ記録してトーストを出す側と、映像のプレースホルダーや設定ダイアログの
//! 「接続状態」タブへ渡す形に整える側をまとめてある。

use super::CaptureCardViewer;
use crate::overlay::OverlayContent;
use crate::status::{self, ConnectionStatus, ErrorSource, LinkStatus};
use chrono::Local;
use log::warn;
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
    /// **ダイアログを開いている間だけ呼ぶ。** ロックは video → audio の順に
    /// 1 つずつ取り、中では小さな構造体の複製しか行わない
    /// （`stats()` / `link_state()` と同じ流儀）。
    pub(super) fn connection_status(&self) -> ConnectionStatus {
        let active_video = match self.video_capture.lock() {
            Ok(video) => video.active(),
            Err(_) => {
                warn!("接続状態の表示で video_capture のロックを取得できない");
                None
            }
        };
        let active_audio = match self.audio_capture.lock() {
            Ok(audio) => audio.active(),
            Err(_) => {
                warn!("接続状態の表示で audio_capture のロックを取得できない");
                None
            }
        };

        let mut video = LinkStatus {
            connected: active_video.is_some(),
            reconnecting: self.video_retry.is_active(),
            attempts: self.video_retry.attempts(),
            details: Vec::new(),
            error: self.status_error(ErrorSource::Video),
        };
        if let Some(active) = active_video {
            video
                .details
                .push(format!("デバイス: {}", active.device_name));
            video.details.push(format!("映像: {}", active.summary()));
            // 実際の fps はデバイスから取れない（video.rs の start_capture を参照）
            video
                .details
                .push(format!("要求フレームレート: {} fps", active.requested_fps));
        }

        let mut audio = LinkStatus {
            connected: active_audio.is_some(),
            reconnecting: self.audio_retry.is_active(),
            attempts: self.audio_retry.attempts(),
            details: Vec::new(),
            error: self.status_error(ErrorSource::Audio),
        };
        if let Some(active) = active_audio {
            audio.details.push(format!(
                "入力: {}（{}）",
                active.input_device,
                active.input_summary()
            ));
            audio.details.push(format!(
                "出力: {}（{}）",
                active.output_device,
                active.output_summary()
            ));
        }

        ConnectionStatus { video, audio }
    }

    /// 「接続状態」タブに出す直近の失敗。`(整形済みの文言, 発生時刻)`。
    ///
    /// トーストやプレースホルダーと違い、ここでは切り詰めない。
    /// 原因を調べるための場所なので、全文が読めるほうがよい。
    fn status_error(&self, source: ErrorSource) -> Option<(String, String)> {
        let recorded = self.errors.latest(source)?;
        Some((
            status::format_message(source, &recorded.message),
            recorded.time_text(),
        ))
    }
}
