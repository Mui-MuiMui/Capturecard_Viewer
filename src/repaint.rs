//! 次の再描画をいつ要求するかを決める。
//!
//! eframe は `update()` の中で要求された再描画しか予約しない。**何も要求しなければ
//! 次の `update()` は来ない**ので、時間で動くもの（フレームの途絶の判定、接続の
//! 再試行、OSD の消滅、設定の遅延書き出し）は自分の都合で予約する必要がある。
//!
//! ここには 2 つのものを置いてある。
//!
//! - `next_repaint_delay` — 「次の `update()` までに最大どれだけ空けてよいか」を
//!   決める純粋関数。`update()` の末尾で 1 回だけ呼ぶ
//! - `RepaintWaker` — UI スレッド以外から再描画を促す窓口。映像フレームの到着を
//!   待たせないために使う
//!
//! egui の `request_repaint_after` は**同じフレーム内で要求された中で最も短い
//! 間隔を採る**。そのため、`overlay.rs` の OSD や `flush_settings_if_due` の
//! ように、もっと早く起きたい処理がそれぞれ勝手に予約してよい。ここが決めるのは
//! あくまで上限で、他の予約を邪魔しない。

use eframe::egui;
use log::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 映像フレームを取り込んだフレームで置く間隔。
///
/// **これは保険であって、映像の滑らかさを決める値ではない。** 通常は次の
/// フレームの到着が `RepaintWaker` 経由でこれより先に再描画を起こす。
/// 60fps なら 16.7ms 間隔で到着するので、この 100ms が実際に効くのは
/// 到着が途切れた直後だけ。
///
/// 16ms（＝ 60fps のポーリング）に戻さないこと。到着のたびに起きるうえに
/// 16ms でも起きることになり、1 枚あたり `update()` が 2 回走る。
const ACTIVE_FALLBACK_INTERVAL: Duration = Duration::from_millis(100);

/// 映像フレームが届いていないときの間隔。
///
/// ここで見ているのは「映像の途絶（3 秒）の判定」「接続の再試行の期限」
/// 「プレースホルダーの文言」「別スレッドから届く結果の取り込み」で、
/// どれも 250ms 遅れて気付いても困らない。
const IDLE_INTERVAL: Duration = Duration::from_millis(250);

/// 最小化しているときの間隔。
///
/// 画面に何も出ていないので、時間で動くものの反応が 1 秒遅れてよい。
/// 音声のパススルーは cpal のコールバックスレッドで動き続けるため、
/// ここを伸ばしても途切れない。
const MINIMIZED_INTERVAL: Duration = Duration::from_secs(1);

/// `RepaintWaker` が要求する再描画の遅延。
///
/// **`Duration::ZERO` にしないこと。** egui は遅延ゼロの要求を受けると
/// `outstanding` を立てて次のフレームも続けて描く（`egui::Context` の
/// `request_repaint_after`）。フレームの到着ごとにこれをやると `update()` が
/// 1 枚につき 2 回走り、到着駆動にした意味が消える。
/// 1ms 遅らせれば 1 回で済み、遅延は元の 16ms ポーリングよりはるかに小さい。
const WAKE_DELAY: Duration = Duration::from_millis(1);

/// 次の再描画までの間隔を決めるための状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintCondition {
    /// ウィンドウが最小化されている
    pub minimized: bool,
    /// このフレームで新しい映像フレームをテクスチャへ取り込んだ
    pub new_frame: bool,
}

/// 次の `update()` までに空けてよい時間を返す。
///
/// 実際の再描画はこれより早く起きうる。映像フレームの到着（`RepaintWaker`）、
/// マウスやキーの入力、OSD や設定の書き出しが個別に予約するため。
///
/// 最小化を最優先で見るのは、映像が流れていても画面に出ていないため。
/// 最小化中は `RepaintWaker` も止めるので、フレームが届いても起きない。
pub fn next_repaint_delay(condition: RepaintCondition) -> Duration {
    if condition.minimized {
        return MINIMIZED_INTERVAL;
    }
    if condition.new_frame {
        return ACTIVE_FALLBACK_INTERVAL;
    }
    IDLE_INTERVAL
}

/// UI スレッド以外から再描画を促すための窓口。
///
/// `egui::Context` は `Send + Sync` で、どのスレッドから
/// `request_repaint_after` を呼んでもよい（eframe が winit のイベントループを
/// 叩いて UI スレッドを起こす）。ただし `Context` が手に入るのは最初の
/// `update()` からなので、**入れ物だけ先に作って後から結びつける**形にしてある。
///
/// 複製しても中身は共有される。映像のフレームコールバックスレッドと
/// ホットキーのリスナースレッドへ複製を渡してある。
#[derive(Clone, Default)]
pub struct RepaintWaker {
    inner: Arc<WakerInner>,
}

struct WakerInner {
    /// 最初の `update()` で 1 度だけ入る。以降は変わらない
    ctx: OnceLock<egui::Context>,
    /// 最小化中は起こさない。既定は `true`（起動直後は最小化かどうかが
    /// 分からないので、起こす側に倒す）
    enabled: AtomicBool,
    /// 結びつく前に起こそうとしたことを 1 度だけ記録するための旗。
    /// フレームコールバックは毎秒 60 回呼ばれるので、記録を繰り返さない
    warned_unbound: AtomicBool,
}

impl Default for WakerInner {
    fn default() -> Self {
        Self {
            ctx: OnceLock::new(),
            enabled: AtomicBool::new(true),
            warned_unbound: AtomicBool::new(false),
        }
    }
}

impl RepaintWaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// UI スレッドの `egui::Context` と結びつける。
    ///
    /// **`update()` の先頭で毎フレーム呼んでよい。** 2 回目以降は何もしない。
    /// `Context` はアプリの生存期間を通して同じものが渡されるので、
    /// 入れ替える必要が無い。
    ///
    /// 保持する `Context` は内部が `Arc` なので、ここで複製しても実体は 1 つ。
    /// アプリが終わるまで生き、`RepaintWaker` を渡したスレッドより長く残る。
    pub fn bind(&self, ctx: &egui::Context) {
        if self.inner.ctx.get().is_some() {
            return;
        }
        // 直前の確認との間に別のスレッドが入れていても、その値が使われるだけ
        let _ = self.inner.ctx.set(ctx.clone());
    }

    /// 起こしてよいかを切り替える。UI スレッドから呼ぶ。
    ///
    /// 最小化中に `false` にするのは、eframe が最小化されたウィンドウの
    /// 再描画要求を捨てるため。要求してもイベントループを起こすだけで
    /// 何も描かれず、フレームの到着ごとにこれをやると無駄が残る。
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
    }

    /// UI スレッドを起こす。結びつく前や、止められている間は何もしない。
    pub fn wake(&self) {
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(ctx) = self.inner.ctx.get() else {
            // 最初の update() より前。まだ描く相手がいないので捨ててよい。
            // ここが続くようなら bind の呼び忘れなので、初回だけ記録する
            if !self.inner.warned_unbound.swap(true, Ordering::Relaxed) {
                warn!(
                    "再描画の要求先がまだ決まっていないので、起こす要求を捨てた。以降は記録しない"
                );
            }
            return;
        };
        ctx.request_repaint_after(WAKE_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_repaint_delay_with_a_new_frame_uses_the_short_fallback() {
        // 到着駆動が働いている前提の保険。ポーリングの 16ms には戻さない
        let delay = next_repaint_delay(RepaintCondition {
            minimized: false,
            new_frame: true,
        });
        assert_eq!(delay, Duration::from_millis(100));
    }

    #[test]
    fn next_repaint_delay_without_a_new_frame_waits_longer() {
        let delay = next_repaint_delay(RepaintCondition {
            minimized: false,
            new_frame: false,
        });
        assert_eq!(delay, Duration::from_millis(250));
    }

    #[test]
    fn next_repaint_delay_when_minimized_waits_a_second() {
        let delay = next_repaint_delay(RepaintCondition {
            minimized: true,
            new_frame: false,
        });
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn next_repaint_delay_when_minimized_ignores_arriving_frames() {
        // 最小化中はフレームが流れていても描かない。ここで new_frame を
        // 優先すると、裏で 60fps の映像が来ている間ずっと起き続ける
        let delay = next_repaint_delay(RepaintCondition {
            minimized: true,
            new_frame: true,
        });
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn next_repaint_delay_is_never_longer_than_a_second() {
        // 上限を伸ばすときは、映像の途絶（3 秒）の判定が間に合うかを確かめること
        for minimized in [false, true] {
            for new_frame in [false, true] {
                let delay = next_repaint_delay(RepaintCondition {
                    minimized,
                    new_frame,
                });
                assert!(
                    delay <= Duration::from_secs(1),
                    "間隔が長すぎる: minimized={minimized}, new_frame={new_frame}, delay={delay:?}"
                );
            }
        }
    }

    #[test]
    fn repaint_waker_without_a_context_does_not_panic() {
        // フレームコールバックは最初の update() より前にも動きうる。
        // 結びつく前に呼ばれても落ちないこと
        let waker = RepaintWaker::new();
        waker.wake();
    }

    #[test]
    fn repaint_waker_clones_share_the_enabled_flag() {
        // 複製を別スレッドへ渡しているので、UI スレッド側の切り替えが届くこと
        let waker = RepaintWaker::new();
        let clone = waker.clone();
        waker.set_enabled(false);
        assert!(!clone.inner.enabled.load(Ordering::Relaxed));
        waker.set_enabled(true);
        assert!(clone.inner.enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn repaint_waker_is_enabled_by_default() {
        // 起動直後は最小化かどうかが分からない。起こす側に倒す
        let waker = RepaintWaker::new();
        assert!(waker.inner.enabled.load(Ordering::Relaxed));
    }
}
