//! 音量とミュートの操作、およびその OSD。
//!
//! 映像上のホイール・ミドルクリック、右クリックメニュー、ホットキーの
//! いずれもここを通す。経路ごとに設定の保存や OSD の有無が変わらないようにする。

use super::CaptureCardViewer;
use crate::overlay::OverlayContent;
use crate::settings::{MAX_VOLUME, MIN_VOLUME};
use eframe::egui;
use log::info;
use std::time::{Duration, Instant};

/// 音量を変えたときに OSD を出しておく時間。
/// ホイールを回している間は回すたびに延びるので、これは「手を止めてから」の長さ
const VOLUME_OSD_DURATION: Duration = Duration::from_millis(1500);

/// 音量の基準値。OSD のバーはこの位置に目盛りを引く
const VOLUME_REFERENCE: f32 = 100.0;

/// ホイール 1 段、またはホットキー 1 回で動かす音量
pub(super) const VOLUME_SCROLL_STEP: f32 = 10.0;

/// 音量 OSD に出す内容を組み立てる。
///
/// バーは 0〜`MAX_VOLUME`% を全体とし、100% の位置に目盛りを引く。
/// 上限が 200% なので、数字だけでは「上げすぎているのか」が分かりにくいため。
///
/// 数字は右クリックメニューの「音量: N%」と同じ `as i32` で作る。丸め方を
/// 変えると、メニューのスライダーを動かしている間だけ OSD と 1% ずれて見える。
///
/// ミュート中はバーを灰色にし、文言に「（ミュート中）」を添える。**数字は消さない。**
/// ミュートを解除したときに戻る音量がそのまま見えているほうが、操作の結果を
/// 予想しやすいため。
fn volume_overlay_content(volume: f32, muted: bool) -> OverlayContent {
    let text = if muted {
        format!("音量: {}%（ミュート中）", volume as i32)
    } else {
        format!("音量: {}%", volume as i32)
    };
    OverlayContent::Bar {
        text,
        ratio: volume / MAX_VOLUME,
        marker_ratio: VOLUME_REFERENCE / MAX_VOLUME,
        dimmed: muted,
    }
}

/// ミュートを切り替えたときに OSD へ出す内容を組み立てる。
///
/// 解除したときだけ音量を添える。ミュート中にスライダーで音量を変えている
/// ことがあるため、「解除したら何%で鳴るのか」が分かるようにしている。
fn mute_overlay_content(muted: bool, volume: f32) -> OverlayContent {
    if muted {
        OverlayContent::Text("ミュート".to_string())
    } else {
        OverlayContent::Text(format!("ミュート解除（音量: {}%）", volume as i32))
    }
}

/// 映像上のホイール操作と「音量を上げる / 下げる」のホットキーで音量を変えたときの、
/// 適用すべき `(音量, ミュート状態)`。
///
/// **ミュート中でも解除する。** これらの操作の近くにはミュートの表示が無く、
/// 解除しないと「音量を上げたのに鳴らない」状態になって原因が分からない。
/// 右クリックメニューのスライダーはすぐ下にミュートのチェックが見えているので、
/// そちらは解除せず、灰色のバーで「効いていない」ことだけを示す。
fn volume_change_result(current: f32, delta: f32) -> (f32, bool) {
    ((current + delta).clamp(MIN_VOLUME, MAX_VOLUME), false)
}

impl CaptureCardViewer {
    /// 映像の上でのホイール操作を音量へ反映する。
    ///
    /// ウィンドウ表示とフルスクリーンの両方から呼ぶ。以前は同じ処理が両方に
    /// 写してあり、片方だけ直す事故が起きやすかった。
    pub(super) fn handle_volume_scroll(&mut self, ctx: &egui::Context) {
        let scroll_y = ctx.input(|i| i.raw_scroll_delta.y);
        if scroll_y == 0.0 {
            return;
        }

        // 段の大きさ、上下限、ミュートの扱いを「音量を上げる / 下げる」の
        // ホットキーと同じにするため、同じ経路へ寄せる
        self.adjust_volume(if scroll_y > 0.0 {
            VOLUME_SCROLL_STEP
        } else {
            -VOLUME_SCROLL_STEP
        });
    }

    /// UI の操作で音量が変わったときの共通処理。
    ///
    /// 設定へ反映して OSD を出す。**ここでディスクへは書かない。**
    /// ホイールを回している間は毎フレーム値が変わるため、書き出しは
    /// `mark_settings_dirty` のデバウンスに任せる。
    ///
    /// 上限・下限に貼り付いたまま操作を続けた場合も OSD の期限は延びる。
    /// 「これ以上は上がらない」ことが分かるほうがよいので、値が変わったかは見ない。
    pub(super) fn set_volume_from_ui(&mut self, volume: f32) {
        self.volume = volume;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.volume = volume;
        }
        self.mark_settings_dirty();
        self.show_volume_overlay();
    }

    /// いまの音量を OSD に出す。ミュート中はバーを灰色にして数字だけ残す。
    pub(super) fn show_volume_overlay(&mut self) {
        self.transient_overlay.show(
            volume_overlay_content(self.volume, self.muted),
            VOLUME_OSD_DURATION,
            Instant::now(),
        );
    }

    /// 映像や空きエリアの上でのミドルクリックをミュートの切り替えへ回す。
    ///
    /// ウィンドウ表示とフルスクリーンの、映像あり / なしの 4 か所から呼ぶ。
    /// 映像が出ていないときも切り替えられるようにしてあるのは、音だけ先に
    /// 来ている状態でも黙らせられるようにするため。
    pub(super) fn handle_middle_click_mute(&mut self, response: &egui::Response) {
        if response.middle_clicked() {
            self.toggle_mute();
        }
    }

    /// 音量を `delta`%（負なら下げる）変える。
    ///
    /// 反映は `set_volume_from_ui` に任せる。映像上のホイール操作や
    /// 右クリックメニューのスライダーと同じ経路を通すことで、設定への
    /// 反映も OSD の表示も同じになる。
    pub(super) fn adjust_volume(&mut self, delta: f32) {
        let (volume, muted) = volume_change_result(self.volume, delta);
        if self.muted != muted {
            // 解除の OSD は出さない。直後の音量 OSD が新しい状態を示す
            self.apply_muted(muted);
        }
        self.set_volume_from_ui(volume);
    }

    /// ミュートの状態を反映する。設定へ書き、音声へ伝えるところまで。
    ///
    /// **OSD はここでは出さない。** 音量変更に巻き込まれた解除では、
    /// ミュートの OSD ではなく音量の OSD を出したいため。
    ///
    /// ロックは settings → audio の順に 1 つずつ取り、重ねない。
    fn apply_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Ok(mut settings) = self.settings.lock() {
            settings.ui.muted = muted;
        }
        if let Ok(mut audio) = self.audio_capture.lock() {
            audio.set_muted(muted);
        }
        self.mark_settings_dirty();
    }

    /// UI の操作でミュートが変わったときの共通処理。反映して OSD を出す。
    pub(super) fn set_muted_from_ui(&mut self, muted: bool) {
        self.apply_muted(muted);
        info!("ミュートを{}にした", if muted { "オン" } else { "オフ" });
        self.transient_overlay.show(
            mute_overlay_content(muted, self.volume),
            VOLUME_OSD_DURATION,
            Instant::now(),
        );
    }

    /// ミュートを切り替える。
    ///
    /// 右クリックメニューのチェック、映像上のミドルクリック、ホットキーが
    /// すべてここを通る。
    pub(super) fn toggle_mute(&mut self) {
        self.set_muted_from_ui(!self.muted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 音量 OSD のバーの中身を取り出す。テキスト以外の形で返ってきたら落とす
    fn volume_bar(volume: f32) -> (String, f32, f32) {
        let (text, ratio, marker_ratio, _) = volume_bar_with_mute(volume, false);
        (text, ratio, marker_ratio)
    }

    /// ミュート状態を指定して音量 OSD のバーの中身を取り出す。
    /// 最後の要素は「灰色で描くか」
    fn volume_bar_with_mute(volume: f32, muted: bool) -> (String, f32, f32, bool) {
        match volume_overlay_content(volume, muted) {
            OverlayContent::Bar {
                text,
                ratio,
                marker_ratio,
                dimmed,
            } => (text, ratio, marker_ratio, dimmed),
            other => panic!("音量 OSD がバー付きになっていない: {:?}", other),
        }
    }

    /// ミュート OSD の文言を取り出す。バーが付いていたら落とす
    fn mute_text(muted: bool, volume: f32) -> String {
        match mute_overlay_content(muted, volume) {
            OverlayContent::Text(text) => text,
            other => panic!("ミュート OSD がテキストになっていない: {:?}", other),
        }
    }

    #[test]
    fn volume_overlay_content_shows_percentage_and_ratio() {
        let (text, ratio, marker_ratio) = volume_bar(80.0);

        assert_eq!(text, "音量: 80%");
        // 0〜200% を全体とするので 80% は 0.4、目盛りの 100% は 0.5
        assert!((ratio - 0.4).abs() < 1e-6, "バーの長さが違う: {}", ratio);
        assert!(
            (marker_ratio - 0.5).abs() < 1e-6,
            "目盛りの位置が違う: {}",
            marker_ratio
        );
    }

    #[test]
    fn volume_overlay_content_at_minimum_is_empty_bar() {
        let (text, ratio, _) = volume_bar(0.0);

        assert_eq!(text, "音量: 0%");
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn volume_overlay_content_at_maximum_fills_bar() {
        let (text, ratio, _) = volume_bar(200.0);

        assert_eq!(text, "音量: 200%");
        assert_eq!(ratio, 1.0);
    }

    #[test]
    fn volume_overlay_content_rounds_down_like_context_menu() {
        // 右クリックメニューの「音量: N%」と同じ丸め方であること。
        // 食い違うと、スライダーを動かしている間だけ 1% ずれて見える
        let (text, _, _) = volume_bar(79.6);

        assert_eq!(text, "音量: 79%");
    }

    #[test]
    fn volume_overlay_content_while_muted_is_dimmed_and_labelled() {
        // ミュート中でも数字は残す。解除したときに戻る音量が見えているほうが
        // 操作の結果を予想しやすい
        let (text, ratio, _, dimmed) = volume_bar_with_mute(80.0, true);

        assert_eq!(text, "音量: 80%（ミュート中）");
        assert!((ratio - 0.4).abs() < 1e-6, "バーの長さが違う: {}", ratio);
        assert!(dimmed);
    }

    #[test]
    fn volume_overlay_content_without_mute_is_not_dimmed() {
        let (_, _, _, dimmed) = volume_bar_with_mute(80.0, false);

        assert!(!dimmed);
    }

    #[test]
    fn mute_overlay_content_muted_shows_only_the_state() {
        assert_eq!(mute_text(true, 80.0), "ミュート");
    }

    #[test]
    fn mute_overlay_content_unmuted_shows_restored_volume() {
        assert_eq!(mute_text(false, 80.0), "ミュート解除（音量: 80%）");
    }

    #[test]
    fn mute_overlay_content_rounds_volume_like_the_volume_osd() {
        // 音量 OSD と丸め方を揃える。食い違うと解除の前後で 1% ずれて見える
        assert_eq!(mute_text(false, 79.6), "ミュート解除（音量: 79%）");
    }

    #[test]
    fn volume_change_result_releases_mute() {
        // ホイールや音量ホットキーで音量を変えたらミュートは解除する
        let (volume, muted) = volume_change_result(50.0, VOLUME_SCROLL_STEP);

        assert_eq!(volume, 60.0);
        assert!(!muted);
    }

    #[test]
    fn volume_change_result_clamps_to_maximum() {
        let (volume, _) = volume_change_result(MAX_VOLUME, VOLUME_SCROLL_STEP);

        assert_eq!(volume, MAX_VOLUME);
    }

    #[test]
    fn volume_change_result_clamps_to_minimum() {
        let (volume, _) = volume_change_result(MIN_VOLUME, -VOLUME_SCROLL_STEP);

        assert_eq!(volume, MIN_VOLUME);
    }

    #[test]
    fn volume_change_result_from_muted_state_still_releases_mute() {
        // 下げる方向でも解除する。「鳴らないまま下げ続ける」状態を作らない
        let (volume, muted) = volume_change_result(50.0, -VOLUME_SCROLL_STEP);

        assert_eq!(volume, 40.0);
        assert!(!muted);
    }
}
