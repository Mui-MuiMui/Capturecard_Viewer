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

/// 映像が届いている間の間隔。60fps ぶん。
///
/// **ここは到着駆動にしない。** フレームの到着ごとに UI スレッドを起こす形も
/// 試したが、1080p60 の実測で CPU が 66% → 85%（1 コアあたり）へ増えた。
/// 描画が 16.7ms の到着間隔の大半を使うため、描いている最中に届いたフレームの
/// 通知が「新着なし」の `update()` をもう 1 回呼び、1 枚につき 2 回描く形に
/// なりやすい。映像が流れている間は 16ms のポーリングのほうが安い。
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// 最後のフレームからこの時間は、映像が続いている扱いで 16ms を保つ。
///
/// 間隔を広げた瞬間に次のフレームが届くと、`RepaintWaker` を有効にするより
/// 先に到着してしまい、その 1 枚が `IDLE_INTERVAL` ぶん遅れて出る。60fps の
/// 到着が 1〜2 枚飛んだだけでそうなるので、少し余裕を置いてから広げる。
const ACTIVE_GRACE: Duration = Duration::from_millis(200);

/// 映像フレームが届いていないときの間隔。
///
/// ここで見ているのは「映像の途絶（3 秒）の判定」「接続の再試行の期限」
/// 「プレースホルダーの文言」「別スレッドから届く結果の取り込み」で、
/// どれも 250ms 遅れて気付いても困らない。
///
/// この状態では `RepaintWaker` を有効にするので、映像が戻ったときは
/// 250ms 待たずにその場で描き始める。
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
/// `request_repaint_after`）。1 回の通知で `update()` が 2 回走ってしまう。
/// 1ms 遅らせれば 1 回で済み、待たされる側の 250ms に比べれば無視できる。
const WAKE_DELAY: Duration = Duration::from_millis(1);

/// 次の再描画までの間隔を決めるための状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintCondition {
    /// ウィンドウが最小化されている
    pub minimized: bool,
    /// 最後に映像フレームをテクスチャへ取り込んでからの経過時間。
    /// `None` は起動してから 1 枚も取り込んでいないことを表す
    pub since_new_frame: Option<Duration>,
}

impl RepaintCondition {
    /// 映像が流れている扱いか。`update()` を 60fps で回す条件。
    fn video_is_live(&self) -> bool {
        matches!(self.since_new_frame, Some(elapsed) if elapsed <= ACTIVE_GRACE)
    }
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
    if condition.video_is_live() {
        return ACTIVE_POLL_INTERVAL;
    }
    IDLE_INTERVAL
}

/// 別スレッドからの通知で UI スレッドを起こしてよいかを返す。
///
/// **起こすのは間隔を広げているときだけ。** 16ms で回している間は、通知で
/// 起こしても次のポーリングとほとんど変わらないうえ、描画中に届いた通知が
/// 「新着なし」の `update()` を 1 回増やすため、かえって高く付く。
///
/// ホットキーの通知も同じ旗で止まる。16ms で回っている間は次の `update()` が
/// 16ms 以内に来るので、反応は変わらない。
pub fn should_wake_on_event(condition: RepaintCondition) -> bool {
    !condition.minimized && !condition.video_is_live()
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

    /// 映像が届いてから `elapsed` 経った、最小化していない状態。
    fn live(elapsed: Duration) -> RepaintCondition {
        RepaintCondition {
            minimized: false,
            since_new_frame: Some(elapsed),
        }
    }

    #[test]
    fn next_repaint_delay_right_after_a_frame_keeps_sixty_fps() {
        assert_eq!(
            next_repaint_delay(live(Duration::ZERO)),
            Duration::from_millis(16)
        );
    }

    #[test]
    fn next_repaint_delay_within_the_grace_period_keeps_sixty_fps() {
        // 60fps の到着が数枚飛んだだけでは間隔を広げない
        assert_eq!(
            next_repaint_delay(live(Duration::from_millis(100))),
            Duration::from_millis(16)
        );
    }

    #[test]
    fn next_repaint_delay_at_the_end_of_the_grace_period_keeps_sixty_fps() {
        // 境界はまだ「映像が続いている」側に倒す
        assert_eq!(
            next_repaint_delay(live(Duration::from_millis(200))),
            Duration::from_millis(16)
        );
    }

    #[test]
    fn next_repaint_delay_after_the_grace_period_widens() {
        assert_eq!(
            next_repaint_delay(live(Duration::from_millis(201))),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn next_repaint_delay_without_any_frame_yet_widens() {
        // 入力信号が無いデバイスは開けても永久にフレームを出さない。
        // 起動直後からここへ来る
        let delay = next_repaint_delay(RepaintCondition {
            minimized: false,
            since_new_frame: None,
        });
        assert_eq!(delay, Duration::from_millis(250));
    }

    #[test]
    fn next_repaint_delay_when_minimized_waits_a_second() {
        let delay = next_repaint_delay(RepaintCondition {
            minimized: true,
            since_new_frame: None,
        });
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn next_repaint_delay_when_minimized_ignores_arriving_frames() {
        // 最小化中はフレームが流れていても描かない。映像を優先すると、
        // 裏で 60fps の映像が来ている間ずっと 60fps で回り続ける
        let delay = next_repaint_delay(RepaintCondition {
            minimized: true,
            since_new_frame: Some(Duration::ZERO),
        });
        assert_eq!(delay, Duration::from_secs(1));
    }

    #[test]
    fn next_repaint_delay_is_never_longer_than_a_second() {
        // 上限を伸ばすときは、映像の途絶（3 秒）の判定が間に合うかを確かめること
        for minimized in [false, true] {
            for since_new_frame in [None, Some(Duration::ZERO), Some(Duration::from_secs(10))] {
                let delay = next_repaint_delay(RepaintCondition {
                    minimized,
                    since_new_frame,
                });
                assert!(
                    delay <= Duration::from_secs(1),
                    "間隔が長すぎる: minimized={minimized}, since_new_frame={since_new_frame:?}, delay={delay:?}"
                );
            }
        }
    }

    #[test]
    fn should_wake_on_event_while_video_is_live_returns_false() {
        // 16ms で回っている間は通知で起こさない。描画中に届いた通知が
        // 「新着なし」の update() を増やす
        assert!(!should_wake_on_event(live(Duration::ZERO)));
    }

    #[test]
    fn should_wake_on_event_after_the_grace_period_returns_true() {
        assert!(should_wake_on_event(live(Duration::from_millis(201))));
    }

    #[test]
    fn should_wake_on_event_without_any_frame_yet_returns_true() {
        assert!(should_wake_on_event(RepaintCondition {
            minimized: false,
            since_new_frame: None,
        }));
    }

    #[test]
    fn should_wake_on_event_when_minimized_returns_false() {
        // 最小化中は eframe が再描画要求を捨てるので、起こしても描かれない
        for since_new_frame in [None, Some(Duration::ZERO), Some(Duration::from_secs(10))] {
            assert!(
                !should_wake_on_event(RepaintCondition {
                    minimized: true,
                    since_new_frame,
                }),
                "最小化中に起こそうとしている: since_new_frame={since_new_frame:?}"
            );
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
