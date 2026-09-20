//! 一時的に映像へ重ねる通知（OSD）の共通部分。
//!
//! 「操作したときだけ数秒出て、放っておくと消える」表示をここでまとめる。
//! フルスクリーンの切替表示と音量の表示が該当する。
//!
//! **常時表示の統計オーバーレイ（`main.rs` の `show_stats_overlay`）は別物。**
//! あちらは期限を持たず、設定のオン / オフで出し入れするものなので、ここへは載せない。

use eframe::egui;
use std::time::{Duration, Instant};

/// OSD を出す位置。画面の下端からこの高さだけ上へ置く。
///
/// 左上は統計オーバーレイが使っているため、重ならない場所を選んでいる。
const BOTTOM_OFFSET: f32 = 48.0;

/// OSD に出す中身。
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayContent {
    /// テキストだけを出す
    Text(String),
}

/// 一定時間で自動的に消える OSD の状態。
///
/// 表示中の中身と、いつまで出すかだけを持つ。描画は `draw` が行う。
#[derive(Default)]
pub struct TransientOverlay {
    active: Option<ActiveOverlay>,
}

struct ActiveOverlay {
    content: OverlayContent,
    shown_at: Instant,
    duration: Duration,
}

impl TransientOverlay {
    /// `now` から `duration` の間だけ `content` を表示する。
    ///
    /// 既に何か出ていれば差し替える。同じ内容を出し直した場合も期限が延びるので、
    /// ホイールを回し続けている間は表示が出たままになる。
    pub fn show(&mut self, content: OverlayContent, duration: Duration, now: Instant) {
        self.active = Some(ActiveOverlay {
            content,
            shown_at: now,
            duration,
        });
    }

    /// `now` 時点の残り表示時間。表示していなければ `None`。
    /// `Some` が「いま表示すべき」を表す。
    pub fn remaining_at(&self, now: Instant) -> Option<Duration> {
        let active = self.active.as_ref()?;
        remaining(active.shown_at, active.duration, now)
    }

    /// 期限内であれば画面へ描く。期限が過ぎていれば表示を捨てる。
    ///
    /// 消える時刻に再描画を予約する。映像が届いていないときは再描画が止まりうるため、
    /// これが無いと表示したまま `update()` が呼ばれず、OSD が消えずに残る。
    pub fn draw(&mut self, ctx: &egui::Context, now: Instant) {
        let Some(left) = self.remaining_at(now) else {
            // 期限切れ。次に出すまで持っていても意味が無いので捨てる
            self.active = None;
            return;
        };
        // 上で `Some` を確かめているので必ず取れる
        let Some(active) = self.active.as_ref() else {
            return;
        };

        ctx.request_repaint_after(left);

        egui::Area::new("transient_overlay")
            .order(egui::Order::Foreground)
            // 映像のドラッグや右クリックを吸わないようにする
            .interactable(false)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -BOTTOM_OFFSET))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| match &active.content {
                        OverlayContent::Text(text) => {
                            draw_text(ui, text);
                        }
                    });
            });
    }
}

/// OSD の文字を描く。
fn draw_text(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(16.0)
            .color(egui::Color32::WHITE),
    );
}

/// 表示を始めた時刻と表示時間から、`now` 時点の残り時間を返す。
///
/// 期限ちょうど、または過ぎていれば `None`。境界では消す側に倒している。
/// 1 フレーム長く出しても得るものが無く、「残り 0 秒を表示中と見なす」状態を
/// 作らないほうが呼び出し側の分岐が減るため。
///
/// `now` が `shown_at` より前でも落ちない。システムクロックではなく `Instant` を
/// 使っているので通常は起こらないが、呼び出し側が時刻を持ち回る形にしてあるため
/// 念のため飽和させている。
pub fn remaining(shown_at: Instant, duration: Duration, now: Instant) -> Option<Duration> {
    let elapsed = now.saturating_duration_since(shown_at);
    let left = duration.checked_sub(elapsed)?;
    if left.is_zero() {
        None
    } else {
        Some(left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_before_deadline_returns_rest() {
        let shown_at = Instant::now();
        let left = remaining(
            shown_at,
            Duration::from_millis(1500),
            shown_at + Duration::from_millis(500),
        );

        assert_eq!(left, Some(Duration::from_millis(1000)));
    }

    #[test]
    fn remaining_at_deadline_returns_none() {
        // 境界。期限ちょうどでは消す
        let shown_at = Instant::now();
        let left = remaining(
            shown_at,
            Duration::from_millis(1500),
            shown_at + Duration::from_millis(1500),
        );

        assert_eq!(left, None);
    }

    #[test]
    fn remaining_after_deadline_returns_none() {
        let shown_at = Instant::now();
        let left = remaining(
            shown_at,
            Duration::from_millis(1500),
            shown_at + Duration::from_secs(10),
        );

        assert_eq!(left, None);
    }

    #[test]
    fn remaining_with_now_before_shown_at_returns_full_duration() {
        // 時刻が巻き戻っても負の経過時間でパニックしないこと
        let now = Instant::now();
        let shown_at = now + Duration::from_secs(5);
        let left = remaining(shown_at, Duration::from_millis(1500), now);

        assert_eq!(left, Some(Duration::from_millis(1500)));
    }

    #[test]
    fn remaining_with_zero_duration_returns_none() {
        let shown_at = Instant::now();

        assert_eq!(remaining(shown_at, Duration::ZERO, shown_at), None);
    }

    #[test]
    fn transient_overlay_default_is_not_visible() {
        let overlay = TransientOverlay::default();

        assert_eq!(overlay.remaining_at(Instant::now()), None);
    }

    #[test]
    fn transient_overlay_show_extends_deadline() {
        let start = Instant::now();
        let mut overlay = TransientOverlay::default();
        overlay.show(
            OverlayContent::Text("音量: 80%".to_string()),
            Duration::from_millis(1000),
            start,
        );

        // 1 回目の期限を過ぎた時刻でも、出し直していれば表示が続く
        let second = start + Duration::from_millis(900);
        overlay.show(
            OverlayContent::Text("音量: 90%".to_string()),
            Duration::from_millis(1000),
            second,
        );

        assert_eq!(
            overlay.remaining_at(start + Duration::from_millis(1500)),
            Some(Duration::from_millis(400))
        );
    }

    #[test]
    fn transient_overlay_hides_after_duration() {
        let start = Instant::now();
        let mut overlay = TransientOverlay::default();
        overlay.show(
            OverlayContent::Text("フルスクリーン ON".to_string()),
            Duration::from_secs(1),
            start,
        );

        assert!(overlay.remaining_at(start).is_some());
        assert_eq!(overlay.remaining_at(start + Duration::from_secs(1)), None);
    }
}
