//! クロックドリフト補正。
//!
//! 出力コールバック（`convert::PassthroughConverter`）がリングバッファの水位を
//! 書き、デバイスワーカー（`app::worker_timers`）が数秒ごとに補正係数を書く。
//! その受け渡しの器（`ResampleTelemetry`）と、観測の窓（`WaterLevelWindow`）、
//! 係数の決め方（`decide_resample_correction`）を持つ。

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// 目標水位からの相対誤差がこの割合未満なら補正しない（デッドゾーン）。
///
/// 揺らぎのたびに補正を動かすと、かえって位相が揺れる。
const RESAMPLE_DEAD_ZONE_RATIO: f64 = 0.01;
/// 相対誤差がこの割合以上で補正が頭打ちになる。
const RESAMPLE_SATURATION_RATIO: f64 = 0.10;
/// 補正係数の最大のずれ（±0.1%）。
///
/// 水晶発振子のずれは通常 100ppm（0.01%）以下なので、10 倍の余裕を持たせてある。
const RESAMPLE_MAX_CORRECTION: f32 = 0.001;

/// 観測の窓を 1 つの `AtomicU64` に詰めるときの、水位の合計に使う下位ビット数。
/// その上の 23 ビットが観測の回数、最上位の 1 ビットがアンダーランの印。
///
/// 別々の Atomic にすると、デバイスワーカーが読み出す間に出力コールバックが
/// 片方だけ足し、平均がずれる。1 語にまとめれば読み出し（`swap`）も
/// 足し込み（`fetch_update` / `fetch_or`）も 1 回の不可分操作で済む。
///
/// 40 ビットは 1 兆サンプル強。192kHz 8ch の 200ms（容量 400ms、61 万サンプル）を
/// 3 秒間（約 300 回）足しても 2 億に届かない。23 ビットの回数は 10ms ごとの
/// コールバックで約 23 時間ぶん。どちらもデバイスワーカーが数秒ごとに読み出して
/// 0 へ戻すので届かないが、届いたらそれ以上は足さない（`with_observation`）。
const WINDOW_SUM_BITS: u32 = 40;
const WINDOW_SUM_MAX: u64 = (1 << WINDOW_SUM_BITS) - 1;
const WINDOW_COUNT_BITS: u32 = 23;
const WINDOW_COUNT_MAX: u32 = (1 << WINDOW_COUNT_BITS) - 1;
const WINDOW_UNDERRUN_BIT: u64 = 1 << (WINDOW_SUM_BITS + WINDOW_COUNT_BITS);

/// デバイスワーカーが前回読み出してから今回までに、出力コールバックが観測した
/// 水位（Issue #308）。
///
/// **補正は瞬間の水位 1 点ではなく、この窓の平均で決める。** 入力は 10ms 前後の
/// 塊で届き、出力も同じくらいの塊で取り出すので、水位は目標の周りで塊 1 つぶん
/// 上下する。50ms の目標に対して ±20% ほどで、デッドゾーン（1%）も頭打ち（10%）も
/// 超えるため、1 点で決めると補正の向きが読んだ瞬間の位相で決まってしまう。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WaterLevelWindow {
    /// 観測した水位の合計（サンプル数）
    sum: u64,
    /// 観測した回数（出力コールバックの回数）
    count: u32,
    /// この窓の間にアンダーランが起きた（`decide_resample_correction`）
    underran: bool,
}

impl WaterLevelWindow {
    fn from_packed(packed: u64) -> Self {
        Self {
            sum: packed & WINDOW_SUM_MAX,
            count: ((packed >> WINDOW_SUM_BITS) & u64::from(WINDOW_COUNT_MAX)) as u32,
            underran: packed & WINDOW_UNDERRUN_BIT != 0,
        }
    }

    fn to_packed(self) -> u64 {
        let underran = if self.underran {
            WINDOW_UNDERRUN_BIT
        } else {
            0
        };
        underran | (u64::from(self.count) << WINDOW_SUM_BITS) | self.sum
    }

    /// 観測を 1 回足した窓。回数か合計が上限に届くなら `None`（足さない）。
    ///
    /// 上限に届いた窓は、そこまでの平均のまま読み出されるのを待つ。途中で
    /// 桁あふれさせると回数のビットへ繰り上がり、平均が壊れる。
    fn with_observation(self, level: usize) -> Option<Self> {
        let sum = self.sum.checked_add(level as u64)?;
        if sum > WINDOW_SUM_MAX || self.count >= WINDOW_COUNT_MAX {
            return None;
        }
        Some(Self {
            sum,
            count: self.count + 1,
            ..self
        })
    }

    /// 平均の水位（サンプル数、切り捨て）。観測が無ければ `None`。
    pub fn mean(self) -> Option<usize> {
        (self.count > 0).then(|| (self.sum / u64::from(self.count)) as usize)
    }

    /// 観測の回数。ログに出すため
    pub fn count(self) -> u32 {
        self.count
    }

    /// この窓の間にアンダーランが起きたか。ログに出すため
    pub fn underran(self) -> bool {
        self.underran
    }
}

/// 観測の窓と目標から、次に使うレート比の補正係数を決める。
///
/// **比例制御（P制御）だけで足りる。** 積分を持たないので、呼ぶたびに水位と
/// 目標から作り直すだけで、前回の値は参照しない。目標を追い越しても次の
/// 呼び出しで符号が反転して自然に戻るので、これで十分。
///
/// 水位（窓の平均）が目標より高い（溜まっている）ときは 1.0 より大きくして
/// 出力側の消費を早め、低い（枯れかけている）ときは 1.0 より小さくして消費を遅らせる。
///
/// 入出力の形が揃っている組み合わせも対象にする（Issue #308）。公称レートが
/// 同じでも入出力は別の時計で動くので、揃っていることは補正を省く理由にならない。
///
/// - 窓に観測が無い（出力コールバックが回っていない）なら `1.0`
/// - 相対誤差が `RESAMPLE_DEAD_ZONE_RATIO` 未満なら `1.0`（目標付近では変えない）
/// - 相対誤差が `RESAMPLE_SATURATION_RATIO` 以上は `RESAMPLE_MAX_CORRECTION` に頭打ち
/// - **アンダーランが起きた窓では速める側へは補正しない**（`1.0`）。アンダーランの
///   間は出力が取り出さずに無音を書くので、そのぶん水位が上がって平均は目標を
///   超える。それを速めて削ると水位の底がまた下がり、次のアンダーランを招く
///   （フェイクの 20ms で、速める補正が頭打ちに張り付いたままアンダーランが
///   毎秒数回ずつ増え続けた）。削るのはアンダーランが止まった窓からでよい。
///   遅らせる側は、アンダーランを減らす向きなのでそのまま掛ける
pub(crate) fn decide_resample_correction(window: WaterLevelWindow, target_level: usize) -> f32 {
    let Some(water_level) = window.mean() else {
        return 1.0;
    };
    if target_level == 0 {
        return 1.0;
    }

    let relative_error = (water_level as f64 - target_level as f64) / target_level as f64;
    if relative_error.abs() < RESAMPLE_DEAD_ZONE_RATIO {
        return 1.0;
    }
    if window.underran && relative_error > 0.0 {
        return 1.0;
    }

    let clamped = relative_error.clamp(-RESAMPLE_SATURATION_RATIO, RESAMPLE_SATURATION_RATIO);
    let magnitude =
        (clamped.abs() / RESAMPLE_SATURATION_RATIO) * f64::from(RESAMPLE_MAX_CORRECTION);
    (1.0 + magnitude.copysign(relative_error)) as f32
}

/// 出力コールバックとデバイスワーカーが共有する、リサンプル補正の状態。
///
/// **ロックを使わない。** 出力コールバックはリアルタイムスレッドなので、
/// ロックを取れない／待たされると音が途切れる。水位と観測の窓は出力コールバックが
/// 毎回書き、補正係数はデバイスワーカーが数秒ごとに書く（`AudioControls` の
/// 音量と同じ、ビット表現のまま出し入れする流儀）。
///
/// **入出力の形が揃っているストリームでも作る**（Issue #308）。以前は
/// 「補正のしようがない」として作らなかったが、揃っていても補間の経路
/// （step 1.0 × 補正係数）に載せれば補正できる。
#[derive(Debug)]
pub struct ResampleTelemetry {
    /// 直近の水位（サンプル数、インターリーブ）。「接続状態」タブへ出す
    water_level: AtomicUsize,
    /// 観測の窓（`WaterLevelWindow` を詰めたもの）。補正はこちらで決める
    window: AtomicU64,
    /// 目標水位（サンプル数）。ストリームを開いたときに決め、以降は変えない
    target_level: usize,
    /// レート比への補正係数（1.0 が無補正）。f32 のビット表現で持つ
    correction: AtomicU32,
}

impl ResampleTelemetry {
    pub(super) fn new(target_level: usize) -> Self {
        Self {
            // 開いた直後は目標どおりとみなす。0 から始めると、最初の水位が
            // 書かれるまでの表示が「空っぽ」になる
            water_level: AtomicUsize::new(target_level),
            window: AtomicU64::new(0),
            target_level,
            correction: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    /// 出力コールバックが呼ぶ。直近の水位を書く。
    pub(super) fn record_water_level(&self, level: usize) {
        self.water_level.store(level, Ordering::Relaxed);
    }

    /// 出力コールバックが呼ぶ。観測の窓へ水位を 1 回足す。
    ///
    /// `fetch_update` は比較と交換の繰り返しで、ロックもアロケーションもしない
    /// （アンダーランの数え方と同じ）。上限に届いた窓には足さない。
    pub(super) fn add_to_window(&self, level: usize) {
        let _ = self
            .window
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |packed| {
                WaterLevelWindow::from_packed(packed)
                    .with_observation(level)
                    .map(WaterLevelWindow::to_packed)
            });
    }

    /// 出力コールバックが呼ぶ。この窓の間にアンダーランが起きたことを印す
    /// （`decide_resample_correction`）。`fetch_or` 1 回で、ロックもアロケーションもしない。
    pub(super) fn mark_underrun(&self) {
        self.window.fetch_or(WINDOW_UNDERRUN_BIT, Ordering::Relaxed);
    }

    /// 出力コールバックが呼ぶ。補正係数を読む。
    pub(super) fn correction(&self) -> f32 {
        f32::from_bits(self.correction.load(Ordering::Relaxed))
    }

    /// デバイスワーカーが呼ぶ。直近の水位を読む。
    pub fn water_level(&self) -> usize {
        self.water_level.load(Ordering::Relaxed)
    }

    /// デバイスワーカーが呼ぶ。観測の窓を読み出し、空に戻す。
    ///
    /// 読み出しと空に戻すのを 1 回の `swap` で行うので、その間に出力コールバックが
    /// 足した観測は、今回か次回のどちらかに必ず入る。
    pub fn take_window(&self) -> WaterLevelWindow {
        WaterLevelWindow::from_packed(self.window.swap(0, Ordering::Relaxed))
    }

    /// デバイスワーカーが呼ぶ。目標水位を読む。
    pub fn target_level(&self) -> usize {
        self.target_level
    }

    /// デバイスワーカーが呼ぶ。補正係数を書く。
    pub fn set_correction(&self, correction: f32) {
        self.correction
            .store(correction.to_bits(), Ordering::Relaxed);
    }
}

/// 「接続状態」タブへ出すための、リサンプル補正の現在値のスナップショット。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResampleStatus {
    /// 現在の補正係数（1.0 が無補正）
    pub ratio: f32,
    /// リングバッファの直近の水位（サンプル数）
    pub water_level: usize,
    /// 目標水位（サンプル数）
    pub target_level: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同じ水位を 1 回だけ観測した窓
    fn level(water_level: usize) -> WaterLevelWindow {
        window_of(&[water_level])
    }

    /// 水位を順に観測した窓
    fn window_of(levels: &[usize]) -> WaterLevelWindow {
        levels
            .iter()
            .fold(WaterLevelWindow::default(), |window, &l| {
                window.with_observation(l).expect("上限に届かない")
            })
    }

    #[test]
    fn decide_resample_correction_zero_target_returns_identity() {
        // 理論上は到達しない（target_level は buffer_size から作る）が、
        // ゼロ除算を避ける防御として 1.0 に倒す
        assert_eq!(decide_resample_correction(level(100), 0), 1.0);
    }

    #[test]
    fn decide_resample_correction_empty_window_does_not_correct() {
        // 観測が無い（出力コールバックが回っていない）なら、根拠が無いので補正しない
        assert_eq!(
            decide_resample_correction(WaterLevelWindow::default(), 500),
            1.0
        );
    }

    #[test]
    fn decide_resample_correction_near_target_does_not_change() {
        assert_eq!(decide_resample_correction(level(500), 500), 1.0);
        // 目標の 1% 未満のずれはデッドゾーン内
        assert_eq!(decide_resample_correction(level(504), 500), 1.0);
        assert_eq!(decide_resample_correction(level(496), 500), 1.0);
    }

    #[test]
    fn decide_resample_correction_dead_zone_boundary() {
        // ちょうど 1% のずれはデッドゾーンの外（補正する）
        assert!(decide_resample_correction(level(505), 500) > 1.0);
        assert!(decide_resample_correction(level(495), 500) < 1.0);
    }

    #[test]
    fn decide_resample_correction_buffer_too_full_speeds_up() {
        // 水位が目標を上回る（溜まっている）ときは 1.0 より大きくして早く消費する
        let ratio = decide_resample_correction(level(600), 500);
        assert!(ratio > 1.0, "{ratio}");
        assert!(ratio <= 1.0 + RESAMPLE_MAX_CORRECTION, "{ratio}");
    }

    #[test]
    fn decide_resample_correction_buffer_too_empty_slows_down() {
        // 水位が目標を下回る（枯れかけている）ときは 1.0 より小さくして消費を遅らせる
        let ratio = decide_resample_correction(level(400), 500);
        assert!(ratio < 1.0, "{ratio}");
        assert!(ratio >= 1.0 - RESAMPLE_MAX_CORRECTION, "{ratio}");
    }

    #[test]
    fn decide_resample_correction_saturates_at_the_bound() {
        // 目標から大きく外れていても ±0.1% を超えない
        assert_eq!(
            decide_resample_correction(level(10_000), 500),
            1.0 + RESAMPLE_MAX_CORRECTION
        );
        assert_eq!(
            decide_resample_correction(level(1), 500),
            1.0 - RESAMPLE_MAX_CORRECTION
        );
    }

    #[test]
    fn decide_resample_correction_scales_between_dead_zone_and_saturation() {
        // デッドゾーンと頭打ちの間では、ずれの大きさに応じて滑らかに動く
        let small = decide_resample_correction(level(525), 500); // 相対誤差 5%
        let large = decide_resample_correction(level(550), 500); // 相対誤差 10%（頭打ち）
        assert!(small > 1.0 && small < large, "{small} {large}");
        assert_eq!(large, 1.0 + RESAMPLE_MAX_CORRECTION);
    }

    #[test]
    fn decide_resample_correction_uses_the_mean_of_the_window() {
        // Issue #308。水位が塊 1 つぶん（目標の ±20%）上下していても、平均が
        // 目標にあれば補正しない。1 点で決めると、どちらの端を読んだかで
        // 頭打ちの補正が逆向きに掛かっていた
        let window = window_of(&[600, 400, 600, 400]);
        assert_eq!(decide_resample_correction(window, 500), 1.0);
        assert_eq!(
            decide_resample_correction(level(600), 500),
            1.0 + RESAMPLE_MAX_CORRECTION
        );
        assert_eq!(
            decide_resample_correction(level(400), 500),
            1.0 - RESAMPLE_MAX_CORRECTION
        );

        // 平均が目標より上なら速める
        let window = window_of(&[700, 400, 700, 400]);
        assert!(decide_resample_correction(window, 500) > 1.0);
    }

    #[test]
    fn water_level_window_mean_rounds_down_and_counts() {
        let window = window_of(&[1, 2]);
        assert_eq!(window.mean(), Some(1));
        assert_eq!(window.count(), 2);
        assert_eq!(WaterLevelWindow::default().mean(), None);
    }

    #[test]
    fn water_level_window_survives_packing() {
        // 1 語に詰めて戻しても同じ窓になる。上限いっぱいの値でも回数・合計・
        // アンダーランの印が混ざらない
        let window = window_of(&[123, 456, 789]);
        assert_eq!(WaterLevelWindow::from_packed(window.to_packed()), window);
        let full = WaterLevelWindow {
            sum: WINDOW_SUM_MAX,
            count: WINDOW_COUNT_MAX,
            underran: false,
        };
        assert_eq!(WaterLevelWindow::from_packed(full.to_packed()), full);
        let marked = WaterLevelWindow {
            underran: true,
            ..full
        };
        assert_eq!(WaterLevelWindow::from_packed(marked.to_packed()), marked);
        assert!(!WaterLevelWindow::from_packed(WINDOW_UNDERRUN_BIT - 1).underran());
    }

    #[test]
    fn water_level_window_stops_adding_at_the_limits() {
        // 合計が上限を超える観測は足さない（回数のビットへ繰り上がらせない）
        let near_sum = WaterLevelWindow {
            sum: WINDOW_SUM_MAX - 10,
            count: 1,
            underran: false,
        };
        assert_eq!(
            near_sum.with_observation(10).map(|w| w.sum),
            Some(WINDOW_SUM_MAX)
        );
        assert_eq!(near_sum.with_observation(11), None);

        // 回数が上限に届いたらそれ以上は足さない
        let full_count = WaterLevelWindow {
            sum: 0,
            count: WINDOW_COUNT_MAX,
            underran: false,
        };
        assert_eq!(full_count.with_observation(0), None);
    }

    #[test]
    fn decide_resample_correction_does_not_speed_up_in_a_window_with_underruns() {
        // アンダーランの間は出力が止まって水位が上がる。それを速めて削ると、
        // 水位の底がまた下がって次のアンダーランを招く
        let window = WaterLevelWindow {
            underran: true,
            ..window_of(&[600, 600])
        };
        assert_eq!(decide_resample_correction(window, 500), 1.0);
    }

    #[test]
    fn decide_resample_correction_still_slows_down_in_a_window_with_underruns() {
        // 遅らせる向きはアンダーランを減らすので、印があっても掛ける
        let window = WaterLevelWindow {
            underran: true,
            ..window_of(&[400, 400])
        };
        assert_eq!(
            decide_resample_correction(window, 500),
            1.0 - RESAMPLE_MAX_CORRECTION
        );
    }

    #[test]
    fn telemetry_mark_underrun_sets_the_flag_until_the_window_is_taken() {
        let telemetry = ResampleTelemetry::new(500);
        telemetry.add_to_window(600);
        telemetry.mark_underrun();
        // 印を付けたあとの観測も同じ窓に入る
        telemetry.add_to_window(600);

        let window = telemetry.take_window();
        assert!(window.underran());
        assert_eq!(window.count(), 2);
        assert_eq!(window.mean(), Some(600));
        // 読み出したら印も消える
        assert!(!telemetry.take_window().underran());
    }

    #[test]
    fn telemetry_take_window_returns_the_observations_and_empties_the_window() {
        let telemetry = ResampleTelemetry::new(500);
        telemetry.add_to_window(400);
        telemetry.add_to_window(600);

        let window = telemetry.take_window();
        assert_eq!(window.mean(), Some(500));
        assert_eq!(window.count(), 2);
        // 読み出したら空に戻る。次の窓は次の観測だけで決まる
        assert_eq!(telemetry.take_window(), WaterLevelWindow::default());
    }

    #[test]
    fn telemetry_window_is_separate_from_the_latest_level() {
        // 直近の水位（表示用）を書いても窓には入らない。窓に入れるかは
        // 出力コールバックが最初の水位に達したかで決める（`convert`）
        let telemetry = ResampleTelemetry::new(500);
        telemetry.record_water_level(42);

        assert_eq!(telemetry.water_level(), 42);
        assert_eq!(telemetry.take_window().count(), 0);
    }
}
