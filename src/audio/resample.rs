//! クロックドリフト補正。
//!
//! 出力コールバック（`convert::PassthroughConverter`）がリングバッファの水位を
//! 書き、デバイスワーカー（`app::worker_timers`）が数秒ごとに補正係数を書く。
//! その受け渡しの器（`ResampleTelemetry`）と、係数の決め方
//! （`decide_resample_correction`）を持つ。

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

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

/// リングバッファの水位と目標から、次に使うレート比の補正係数を決める。
///
/// **比例制御（P制御）だけで足りる。** 積分を持たないので、呼ぶたびに水位と
/// 目標から作り直すだけで、前回の値は参照しない。目標を追い越しても次の
/// 呼び出しで符号が反転して自然に戻るので、これで十分。
///
/// 水位が目標より高い（溜まっている）ときは 1.0 より大きくして出力側の
/// 消費を早め、低い（枯れかけている）ときは 1.0 より小さくして消費を遅らせる。
///
/// - `is_identity`（入出力の形が揃っている）が真なら常に `1.0`
/// - 相対誤差が `RESAMPLE_DEAD_ZONE_RATIO` 未満なら `1.0`（目標付近では変えない）
/// - 相対誤差が `RESAMPLE_SATURATION_RATIO` 以上は `RESAMPLE_MAX_CORRECTION` に頭打ち
pub(crate) fn decide_resample_correction(
    is_identity: bool,
    water_level: usize,
    target_level: usize,
) -> f32 {
    if is_identity || target_level == 0 {
        return 1.0;
    }

    let relative_error = (water_level as f64 - target_level as f64) / target_level as f64;
    if relative_error.abs() < RESAMPLE_DEAD_ZONE_RATIO {
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
/// ロックを取れない／待たされると音が途切れる。水位は出力コールバックが
/// 毎回書き、補正係数はデバイスワーカーが数秒ごとに書く（`AudioControls` の
/// 音量と同じ、ビット表現のまま出し入れする流儀）。
///
/// **入出力の形が揃っている（identity）ストリームでは作らない。** 補正の
/// しようがないので、水位を追う意味がない。
#[derive(Debug)]
pub struct ResampleTelemetry {
    /// リングバッファの水位（サンプル数、インターリーブ）
    water_level: AtomicUsize,
    /// 目標水位（サンプル数）。ストリームを開いたときに決め、以降は変えない
    target_level: usize,
    /// レート比への補正係数（1.0 が無補正）。f32 のビット表現で持つ
    correction: AtomicU32,
}

impl ResampleTelemetry {
    pub(super) fn new(target_level: usize) -> Self {
        Self {
            // 開いた直後は目標どおりとみなす。0 から始めると、最初の
            // 調整が「空っぽ」と誤認して的外れな補正をかけてしまう
            water_level: AtomicUsize::new(target_level),
            target_level,
            correction: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    /// 出力コールバックが呼ぶ。水位を書く。
    pub(super) fn record_water_level(&self, level: usize) {
        self.water_level.store(level, Ordering::Relaxed);
    }

    /// 出力コールバックが呼ぶ。補正係数を読む。
    pub(super) fn correction(&self) -> f32 {
        f32::from_bits(self.correction.load(Ordering::Relaxed))
    }

    /// デバイスワーカーが呼ぶ。水位を読む。
    pub fn water_level(&self) -> usize {
        self.water_level.load(Ordering::Relaxed)
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
    /// リングバッファの水位（サンプル数）
    pub water_level: usize,
    /// 目標水位（サンプル数）
    pub target_level: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_resample_correction_identity_stays_at_one() {
        // 揃っている組み合わせでは、水位がどれだけずれていても補正しない
        assert_eq!(decide_resample_correction(true, 10_000, 500), 1.0);
    }

    #[test]
    fn decide_resample_correction_zero_target_returns_identity() {
        // 理論上は到達しない（target_level は buffer_size から作る）が、
        // ゼロ除算を避ける防御として 1.0 に倒す
        assert_eq!(decide_resample_correction(false, 100, 0), 1.0);
    }

    #[test]
    fn decide_resample_correction_near_target_does_not_change() {
        assert_eq!(decide_resample_correction(false, 500, 500), 1.0);
        // 目標の 1% 未満のずれはデッドゾーン内
        assert_eq!(decide_resample_correction(false, 504, 500), 1.0);
        assert_eq!(decide_resample_correction(false, 496, 500), 1.0);
    }

    #[test]
    fn decide_resample_correction_buffer_too_full_speeds_up() {
        // 水位が目標を上回る（溜まっている）ときは 1.0 より大きくして早く消費する
        let ratio = decide_resample_correction(false, 600, 500);
        assert!(ratio > 1.0, "{ratio}");
        assert!(ratio <= 1.0 + RESAMPLE_MAX_CORRECTION, "{ratio}");
    }

    #[test]
    fn decide_resample_correction_buffer_too_empty_slows_down() {
        // 水位が目標を下回る（枯れかけている）ときは 1.0 より小さくして消費を遅らせる
        let ratio = decide_resample_correction(false, 400, 500);
        assert!(ratio < 1.0, "{ratio}");
        assert!(ratio >= 1.0 - RESAMPLE_MAX_CORRECTION, "{ratio}");
    }

    #[test]
    fn decide_resample_correction_saturates_at_the_bound() {
        // 目標から大きく外れていても ±0.1% を超えない
        assert_eq!(
            decide_resample_correction(false, 10_000, 500),
            1.0 + RESAMPLE_MAX_CORRECTION
        );
        assert_eq!(
            decide_resample_correction(false, 1, 500),
            1.0 - RESAMPLE_MAX_CORRECTION
        );
    }

    #[test]
    fn decide_resample_correction_scales_between_dead_zone_and_saturation() {
        // デッドゾーンと頭打ちの間では、ずれの大きさに応じて滑らかに動く
        let small = decide_resample_correction(false, 525, 500); // 相対誤差 5%
        let large = decide_resample_correction(false, 550, 500); // 相対誤差 10%（頭打ち）
        assert!(small > 1.0 && small < large, "{small} {large}");
        assert_eq!(large, 1.0 + RESAMPLE_MAX_CORRECTION);
    }
}
