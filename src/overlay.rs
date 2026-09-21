//! 一時的に映像へ重ねる通知（OSD）の共通部分。
//!
//! 「操作したときだけ数秒出て、放っておくと消える」表示をここでまとめる。
//! フルスクリーンの切替表示と音量の表示が該当する。
//!
//! **常時表示の統計オーバーレイ（`app/view.rs` の `show_stats_overlay`）は別物。**
//! あちらは期限を持たず、設定のオン / オフで出し入れするものなので、ここへは載せない。

use eframe::egui;
use std::time::{Duration, Instant};

/// OSD を出す位置。画面の下端からこの高さだけ上へ置く。
///
/// 左上は統計オーバーレイが使っているため、重ならない場所を選んでいる。
const BOTTOM_OFFSET: f32 = 48.0;

/// バーの大きさ
const BAR_WIDTH: f32 = 200.0;
const BAR_HEIGHT: f32 = 10.0;

/// OSD に出す中身。
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayContent {
    /// テキストだけを出す
    Text(String),
    /// テキストの下に横バーを添える。
    ///
    /// `ratio` はバーの塗り具合、`marker_ratio` は目盛りを引く位置で、
    /// どちらも 0.0〜1.0 で表す。音量のように既定値（100%）が範囲の途中にある
    /// 値で、「いまどのあたりか」と「基準はどこか」を一目で分かるようにするためにある。
    ///
    /// `dimmed` はバーを灰色で描く。値としては有効だが、いま効いていない状態
    /// （ミュート中の音量）を、数字を消さずに示すためにある。
    Bar {
        text: String,
        ratio: f32,
        marker_ratio: f32,
        dimmed: bool,
    },
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
                        OverlayContent::Bar {
                            text,
                            ratio,
                            marker_ratio,
                            dimmed,
                        } => {
                            draw_text(ui, text);
                            draw_bar(ui, *ratio, *marker_ratio, *dimmed);
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

/// バーの塗りの色を返す。`(基準まで, 基準を超えた分)`。
///
/// `dimmed` では 2 色とも同じ灰色にする。色で「基準より上げている」ことを
/// 示す意味が、鳴っていない状態では無いため。
fn bar_colors(dimmed: bool) -> (egui::Color32, egui::Color32) {
    if dimmed {
        let gray = egui::Color32::from_rgb(120, 120, 120);
        (gray, gray)
    } else {
        (
            egui::Color32::from_rgb(220, 220, 220),
            egui::Color32::from_rgb(240, 160, 60),
        )
    }
}

/// 横バーを描く。基準位置（音量なら 100%）に目盛りを引き、
/// そこを超えた分は色を変えて「基準より上げている」ことが分かるようにする。
///
/// `dimmed` のときは全体を灰色で描く。
fn draw_bar(ui: &mut egui::Ui, ratio: f32, marker_ratio: f32, dimmed: bool) {
    let ratio = normalized_ratio(ratio);
    let marker_ratio = normalized_ratio(marker_ratio);
    let (base_color, over_color) = bar_colors(dimmed);

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(BAR_WIDTH, BAR_HEIGHT),
        // 押せるものではないので当たり判定を持たせない
        egui::Sense::hover(),
    );
    let painter = ui.painter();

    // 溝
    painter.rect_filled(rect, 2.0, egui::Color32::from_white_alpha(40));

    // 基準までの塗り
    let base_end = ratio.min(marker_ratio);
    if base_end > 0.0 {
        painter.rect_filled(sub_rect(rect, 0.0, base_end), 2.0, base_color);
    }

    // 基準を超えた分
    if ratio > marker_ratio {
        painter.rect_filled(sub_rect(rect, marker_ratio, ratio), 2.0, over_color);
    }

    // 基準位置の目盛り
    let marker_x = rect.left() + rect.width() * marker_ratio;
    painter.line_segment(
        [
            egui::pos2(marker_x, rect.top()),
            egui::pos2(marker_x, rect.bottom()),
        ],
        egui::Stroke::new(1.0_f32, egui::Color32::WHITE),
    );
}

/// バーの `from`〜`to`（どちらも 0.0〜1.0）に当たる矩形を返す。
fn sub_rect(rect: egui::Rect, from: f32, to: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() + rect.width() * from, rect.top()),
        egui::pos2(rect.left() + rect.width() * to, rect.bottom()),
    )
}

/// バーの割合を 0.0〜1.0 に収める。NaN は 0.0 として扱う。
///
/// 割合は呼び出し側の割り算で作るため、上限が 0 の場合に NaN や無限大が
/// 紛れ込みうる。そのまま描くと矩形の座標が壊れる。
fn normalized_ratio(ratio: f32) -> f32 {
    if ratio.is_nan() {
        return 0.0;
    }
    ratio.clamp(0.0, 1.0)
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

    #[test]
    fn bar_colors_dimmed_uses_same_gray_for_both_parts() {
        // ミュート中は「基準より上げている」ことを色で示す意味が無いので、
        // 基準までと超えた分を同じ灰色にする
        let (base, over) = bar_colors(true);

        assert_eq!(base, over);
        assert_eq!(base, egui::Color32::from_rgb(120, 120, 120));
    }

    #[test]
    fn bar_colors_normal_separates_over_reference_part() {
        let (base, over) = bar_colors(false);

        assert_ne!(base, over);
    }

    #[test]
    fn normalized_ratio_clamps_out_of_range() {
        assert_eq!(normalized_ratio(-0.5), 0.0);
        assert_eq!(normalized_ratio(1.5), 1.0);
        assert_eq!(normalized_ratio(0.25), 0.25);
    }

    #[test]
    fn normalized_ratio_nan_returns_zero() {
        // 上限 0 での割り算などで NaN が来ても矩形を壊さない
        assert_eq!(normalized_ratio(f32::NAN), 0.0);
    }

    #[test]
    fn normalized_ratio_infinity_is_clamped() {
        assert_eq!(normalized_ratio(f32::INFINITY), 1.0);
        assert_eq!(normalized_ratio(f32::NEG_INFINITY), 0.0);
    }
}
