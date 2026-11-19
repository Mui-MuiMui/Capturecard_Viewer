//! 起動中にモニターをオフにさせない・スクリーンセーバーを起動させない（#574）。
//!
//! 掛けるか外すかを毎フレームの終わり（`end_frame`）に決め、変わったときだけ
//! `platform::set_keep_display_on` を呼ぶ。終了時（`on_exit`）に外す。
//! **呼ぶのは UI スレッドだけ。** Windows の要求は呼んだスレッドに紐づくので、
//! 掛けたスレッドと別のスレッドからは外せない（`docs/design/window.md`）。
//!
//! 状態（設定の値と、いま掛けているか）は `app/mod.rs` の `CaptureCardViewer` が持ち、
//! ここは `impl` を足すだけ。

use super::CaptureCardViewer;
use crate::platform;
use log::{info, warn};

/// 要求を掛けておくべきかを返す。
///
/// 最小化している間は外す。見ていない映像のために画面を点けておく理由がなく、
/// 最小化したまま席を離れたときに Windows の電源設定どおり画面が消えるほうが自然。
fn wants_keep_display_on(enabled: bool, minimized: bool) -> bool {
    enabled && !minimized
}

impl CaptureCardViewer {
    /// 設定と最小化の状態に合わせて、要求を掛ける / 外す。変わらなければ何もしない。
    ///
    /// `end_frame` が毎フレーム呼ぶ。最小化すると `update()` が回らなくなるが、
    /// 最小化した瞬間と戻した瞬間には egui-winit が再描画を要求するので、
    /// その 2 回で切り替わる。
    pub(super) fn sync_display_sleep(&mut self, minimized: bool) {
        let wanted = wants_keep_display_on(self.prevent_display_sleep, minimized);
        if wanted == self.keep_display_on {
            return;
        }
        self.set_keep_display_on(wanted);
    }

    /// 終了時に要求を外す。UI スレッドが抜ければ Windows が外すが、`on_exit` の後に
    /// 新しい版の起動（更新の差し替え）や終了待ちが続くので、明示して外しておく。
    pub(super) fn release_display_sleep(&mut self) {
        if self.keep_display_on {
            self.set_keep_display_on(false);
        }
    }

    fn set_keep_display_on(&mut self, keep_on: bool) {
        if platform::set_keep_display_on(keep_on) {
            if keep_on {
                info!("モニターをオフにさせない要求を掛けた");
            } else {
                info!("モニターをオフにさせない要求を外した");
            }
        } else {
            // 失敗しても状態は求めた値へ進める。戻すと毎フレーム呼び直して
            // ログが溢れる。次に設定か最小化の状態が変わったときに呼び直す。
            // 画面が消えるだけで映像・音声・設定は壊れないので、トーストは出さない
            warn!(
                "モニターをオフにさせない要求を{}られなかった（SetThreadExecutionState が失敗）",
                if keep_on { "掛け" } else { "外せ" }
            );
        }
        self.keep_display_on = keep_on;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wants_keep_display_on_when_enabled_and_shown() {
        assert!(wants_keep_display_on(true, false));
    }

    #[test]
    fn wants_keep_display_on_is_false_while_minimized() {
        // 最小化中は外す。見ていないのに画面を点けっぱなしにしない
        assert!(!wants_keep_display_on(true, true));
    }

    #[test]
    fn wants_keep_display_on_is_false_when_disabled() {
        for minimized in [false, true] {
            assert!(!wants_keep_display_on(false, minimized));
        }
    }
}
