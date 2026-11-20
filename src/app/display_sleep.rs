//! 起動中にモニターをオフにさせない（#574）。
//!
//! 設定を取り込んだとき（`apply_settings`）に、値が変わっていれば
//! `platform::set_keep_display_on` を呼ぶ。終了時（`on_exit`）に外す。
//! **呼ぶのは UI スレッドだけ。** Windows の要求は呼んだスレッドに紐づくので、
//! 掛けたスレッドと別のスレッドからは外せない（`docs/design/window.md`）。
//!
//! 最小化中も掛けたままにする。最小化中は `update()` が回らず、最小化したことを
//! UI スレッドの処理で拾えないため（`docs/design/window.md`）。
//!
//! 状態（いま掛けているか）は `app/mod.rs` の `CaptureCardViewer` が持ち、
//! ここは `impl` を足すだけ。

use super::CaptureCardViewer;
use crate::platform;
use log::{info, warn};

impl CaptureCardViewer {
    /// 設定の値に合わせて要求を掛ける / 外す。いまの状態と同じなら何もしない。
    ///
    /// `apply_settings` が呼ぶ。2 秒ごとの再適用でも呼ばれるが、値が変わらなければ
    /// Windows を呼ばない。
    pub(super) fn sync_display_sleep(&mut self, prevent_display_sleep: bool) {
        if prevent_display_sleep == self.keep_display_on {
            return;
        }
        self.set_keep_display_on(prevent_display_sleep);
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
            self.keep_display_on = keep_on;
            self.display_sleep_failed = None;
        } else if self.display_sleep_failed != Some(keep_on) {
            // 状態は進めない。進めると、外せなかった要求が掛かったままなのに
            // 「外した」扱いになり、終了まで外し直さない。2 秒ごとの再適用で呼び直す。
            // 警告は同じ失敗が続く間は 1 回だけにして、ログを溢れさせない。
            // 画面が消えるだけで映像・音声・設定は壊れないので、トーストは出さない
            warn!(
                "モニターをオフにさせない要求を{}られなかった（SetThreadExecutionState が失敗）",
                if keep_on { "掛け" } else { "外せ" }
            );
            self.display_sleep_failed = Some(keep_on);
        }
    }
}
