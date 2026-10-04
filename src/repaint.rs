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
//! - `RepaintWaker` — UI スレッド以外から再描画を促す窓口。**映像フレームは
//!   これで到着したその場で取り込む**（#459）
//!
//! egui の `request_repaint_after` は**同じフレーム内で要求された中で最も短い
//! 間隔を採る**。そのため、`overlay.rs` の OSD や `flush_settings_if_due` の
//! ように、もっと早く起きたい処理がそれぞれ勝手に予約してよい。ここが決めるのは
//! あくまで上限で、他の予約を邪魔しない。
//!
//! **egui は要求された遅延から 1 フレームぶん（`predicted_dt`、eframe では 1/60 秒）を
//! 引いてから予約する。** 16ms を要求すると 0ms になり、描き終えたらすぐ次の
//! `update()` が来る。#459 までは映像が流れている間 16ms を要求して到着では起こさず、
//! 実際には「垂直同期で止まる swap の直後に次の `update()`」の形で回っていた。
//! 到着からの待ちがその位相で決まり、0〜1 フレーム（平均で半フレーム）待っていた。

use eframe::egui;
use log::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 最小化していないときの間隔。映像が流れていてもいなくても同じ。
///
/// ここで見ているのは「映像の途絶（3 秒）の判定」「接続の再試行の期限」
/// 「プレースホルダーの文言」「別スレッドから届く結果の取り込み」で、
/// どれも 250ms 遅れて気付いても困らない。
///
/// **映像フレームはこの間隔では取り込まない。** 届いたその場で `RepaintWaker` が
/// 起こす（#459）。映像が流れている間に 16ms のポーリングを足すと、到着とは
/// 関係のない位相で `update()` が回り、届いたフレームを次のポーリングまで待たせる。
/// 通知と併用すると、描いている最中に届いた通知のぶん `update()` が増える
/// （1080p60 で CPU が 66% → 85% に増えた、`docs/design/video-pipeline.md`）。
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
/// 1ms なら `outstanding` は立たず、`predicted_dt` を引かれて 0ms で予約されるので、
/// 待たされることもない。
const WAKE_DELAY: Duration = Duration::from_millis(1);

/// 次の再描画までの間隔を決めるための状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintCondition {
    /// ウィンドウが最小化されている
    pub minimized: bool,
}

/// 次の `update()` までに空けてよい時間を返す。
///
/// 実際の再描画はこれより早く起きうる。映像フレームの到着（`RepaintWaker`）、
/// マウスやキーの入力、OSD や設定の書き出しが個別に予約するため。
///
/// 最小化中は `RepaintWaker` も止めるので、フレームが届いても起きない。
pub fn next_repaint_delay(condition: RepaintCondition) -> Duration {
    if condition.minimized {
        return MINIMIZED_INTERVAL;
    }
    IDLE_INTERVAL
}

/// 別スレッドからの通知で UI スレッドを起こしてよいかを返す。
///
/// **最小化していなければ、映像が流れている間も起こす**（#459）。映像フレームの
/// 取り込みは通知だけが駆動するので、ここを止めると 250ms ごとにしか描かれない。
///
/// 描いている最中に届いた通知は、その `update()` が終わってから次の `update()` を
/// 1 回呼ぶ。そのフレームを取り込む前に届いた通知なら次は「新着なし」になるが、
/// フレームを取り込むのは `update()` の先頭近くなので、そうなるのはまれ
/// （フェイク 1080p60 で 30 秒に 0〜5 回。16ms の予約の間は約 160 回あった）。
pub fn should_wake_on_event(condition: RepaintCondition) -> bool {
    !condition.minimized
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
    ///
    /// **`false` → `true` の切り替えでは、その場で 1 回再描画を予約する。**
    /// 呼び出し側（`update()` の末尾）がフレームバッファを読んでからここへ
    /// 来るまでの間に届いたフレームは、まだ `false` なので捨てられている。
    /// 拾い直さないと、その 1 枚が `IDLE_INTERVAL` ぶん遅れて出る。
    /// 切り替えは最小化から戻ったときに起きるだけなので、費用は無視できる。
    pub fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.inner.enabled.swap(enabled, Ordering::Relaxed);
        if enabled && !was_enabled {
            if let Some(ctx) = self.inner.ctx.get() {
                ctx.request_repaint_after(WAKE_DELAY);
            }
        }
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

    /// 最小化していない状態。
    fn shown() -> RepaintCondition {
        RepaintCondition { minimized: false }
    }

    /// 最小化している状態。
    fn minimized() -> RepaintCondition {
        RepaintCondition { minimized: true }
    }

    #[test]
    fn next_repaint_delay_when_shown_waits_a_quarter_second() {
        // 映像が流れていてもいなくても同じ。映像は到着で起こす（#459）
        assert_eq!(next_repaint_delay(shown()), Duration::from_millis(250));
    }

    #[test]
    fn next_repaint_delay_when_shown_does_not_poll_at_the_video_rate() {
        // 60fps のポーリングへ戻すと、到着とは関係のない位相で update() が回り、
        // 届いたフレームを次のポーリングまで待たせる。egui が 1 フレームぶん
        // （1/60 秒）を引くので、2 フレームより短い間隔は実質ポーリングになる
        assert!(
            next_repaint_delay(shown()) > Duration::from_millis(34),
            "映像の速さでポーリングしている: {:?}",
            next_repaint_delay(shown())
        );
    }

    #[test]
    fn next_repaint_delay_when_minimized_waits_a_second() {
        assert_eq!(next_repaint_delay(minimized()), Duration::from_secs(1));
    }

    #[test]
    fn next_repaint_delay_is_never_longer_than_a_second() {
        // 上限を伸ばすときは、映像の途絶（3 秒）の判定が間に合うかを確かめること
        for condition in [shown(), minimized()] {
            let delay = next_repaint_delay(condition);
            assert!(
                delay <= Duration::from_secs(1),
                "間隔が長すぎる: {condition:?}, delay={delay:?}"
            );
        }
    }

    #[test]
    fn should_wake_on_event_when_shown_returns_true() {
        // 映像フレームの取り込みは通知だけが駆動する。止めると 250ms ごとにしか描かれない
        assert!(should_wake_on_event(shown()));
    }

    #[test]
    fn should_wake_on_event_when_minimized_returns_false() {
        // 最小化中は eframe が再描画要求を捨てるので、起こしても描かれない
        assert!(!should_wake_on_event(minimized()));
    }

    #[test]
    fn egui_shortens_a_requested_delay_by_one_predicted_frame() {
        // 間隔の決め方の前提（モジュールの先頭）。egui は要求された遅延から
        // `predicted_dt`（既定 1/60 秒）を引く。16ms の要求は 0ms になり、
        // 「16ms ごとのポーリング」は実際には「描き終えたらすぐ次」だった。
        // egui を上げてここが落ちたら、間隔の決め方を見直すこと
        let ctx = settled_context();
        let delay_for = |requested: Duration| {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.ctx().request_repaint_after(requested);
            });
            let delay = output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .expect("ルートのビューポートがある")
                .repaint_delay;
            output.drop_without_applying_deltas();
            delay
        };
        assert_eq!(delay_for(Duration::from_millis(16)), Duration::ZERO);
        assert_eq!(delay_for(WAKE_DELAY), Duration::ZERO);
        let idle = delay_for(Duration::from_millis(250));
        assert!(
            idle > Duration::from_millis(200) && idle < Duration::from_millis(250),
            "{idle:?}"
        );
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

    /// 何も要求していない状態の `egui::Context` を作る。
    ///
    /// 生成直後の `Context` は最初の描画を要求した状態で、egui 0.36 は 1 回目の終わりに
    /// ウィンドウのテーマを送るのでもう 1 回要求する（`ViewportCommand::SetTheme`）。3 回空回しして
    /// 落ち着かせる。画面も入力も無い状態で回せるので、実機は要らない。
    fn settled_context() -> egui::Context {
        let ctx = egui::Context::default();
        for _ in 0..3 {
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .drop_without_applying_deltas();
        }
        assert!(
            !ctx.has_requested_repaint(),
            "前提が崩れている: 何も要求していないのに再描画が予約されている {:?}",
            ctx.repaint_causes()
        );
        ctx
    }

    #[test]
    fn repaint_waker_re_enabling_requests_a_repaint() {
        // 止めている間に届いたフレームは捨てられている。再開のときに
        // 拾い直さないと、その 1 枚が IDLE_INTERVAL ぶん遅れて出る
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);

        waker.set_enabled(false);
        assert!(!ctx.has_requested_repaint(), "止めるときに予約している");

        waker.set_enabled(true);
        assert!(ctx.has_requested_repaint(), "再開のときに予約していない");
    }

    #[test]
    fn repaint_waker_enabling_while_already_enabled_requests_nothing() {
        // 最小化していない間は毎フレーム set_enabled(true) が呼ばれる。
        // そのたびに予約すると、update() が止まらずに回り続ける
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);

        waker.set_enabled(true);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        assert!(!ctx.has_requested_repaint(), "前提が崩れている");

        waker.set_enabled(true);
        assert!(
            !ctx.has_requested_repaint(),
            "同じ値を入れ直しただけで予約している"
        );
    }

    #[test]
    fn repaint_waker_wake_while_disabled_requests_nothing() {
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);
        waker.set_enabled(false);

        waker.wake();
        assert!(!ctx.has_requested_repaint(), "止めているのに起こしている");
    }

    #[test]
    fn repaint_waker_wake_while_enabled_requests_a_repaint() {
        let ctx = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&ctx);

        waker.wake();
        assert!(ctx.has_requested_repaint(), "起こす要求が届いていない");
    }

    #[test]
    fn repaint_waker_binds_only_once() {
        // update() の先頭で毎フレーム呼ぶので、2 回目以降が無視されること。
        // 差し替わると、フレームコールバックが握っている複製の向き先も変わる
        let first = settled_context();
        let second = settled_context();
        let waker = RepaintWaker::new();
        waker.bind(&first);
        waker.bind(&second);

        waker.wake();
        assert!(
            first.has_requested_repaint(),
            "最初の Context へ届いていない"
        );
        assert!(
            !second.has_requested_repaint(),
            "あとから渡した Context へ差し替わっている"
        );
    }
}
