//! 音量・パススルー・ミュートの共有状態。
//!
//! 出力コールバック（`stream::build_output_stream_with`）が 1 回ごとに読み、
//! UI スレッドが書く。ストリームを開き直しても中身は引き継ぐ。

use log::trace;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// 音量の既定値（100%）。設定を読めなかった場合もここへ倒す。
const DEFAULT_VOLUME: f32 = 1.0;

/// 出力コールバックが 1 回ごとに読む共有の値。
///
/// **ロックを使わない。** `Mutex` だと、値を書き換えている最中にリアルタイム
/// スレッドが待たされ、バッファを埋め損ねて音が途切れる。
///
/// ストリームを開き直しても中身は引き継ぐので、`AudioCapture` より長く生きる。
/// デバイス操作はワーカースレッドが行うが、**ここへ書くのは UI スレッドで
/// よい。** デバイスを開く処理を挟まないため、チャネルを経由させる理由がない。
///
/// フィールドが `pub(super)` なのは、出力コールバック（`stream`）がロックも
/// 関数呼び出しも挟まずに読むため。`audio` の外へは出さない。
#[derive(Debug)]
pub struct AudioControls {
    /// 出力に掛ける倍率。`0.0`〜`2.0`。
    ///
    /// f32 の値を直接持てる Atomic 型が無いため、`to_bits` / `from_bits` で
    /// ビット表現のまま出し入れする。
    pub(super) volume: AtomicU32,
    pub(super) passthrough_enabled: AtomicBool,
    /// ミュート中か。
    ///
    /// **音量とは独立に持つ。** 音量 0% で代用すると、ミュートを解除したときに
    /// 戻すべき値が残らない。
    pub(super) muted: AtomicBool,
}

impl Default for AudioControls {
    fn default() -> Self {
        Self {
            volume: AtomicU32::new(DEFAULT_VOLUME.to_bits()),
            // 既定では音声パススルーを有効にする（音が出る状態で起動する）
            passthrough_enabled: AtomicBool::new(true),
            // 既定はミュート解除。設定から読んだ値は apply_settings が入れ直す
            muted: AtomicBool::new(false),
        }
    }
}

impl AudioControls {
    /// 音量をパーセント指定で入れる。範囲外や `nan` は `normalize_volume` が倒す。
    pub fn set_volume(&self, volume_percent: f32) {
        // apply_settings から 2 秒ごとに呼ばれるため trace に落とす
        trace!("音量を設定する: {}%", volume_percent);
        store_volume(&self.volume, normalize_volume(volume_percent));
    }

    pub fn set_passthrough_enabled(&self, enabled: bool) {
        // apply_settings から 2 秒ごとに呼ばれる。変化の有無を判別できないので trace に落とす
        trace!("音声パススルーの有効/無効を設定する: {}", enabled);
        self.passthrough_enabled.store(enabled, Ordering::Relaxed);
    }

    /// ミュートの入切を設定する。
    ///
    /// **音量には触らない。** ミュート中も `volume` は元の値のまま残り、
    /// 解除するとその音量で鳴り始める。
    pub fn set_muted(&self, muted: bool) {
        // apply_settings から 2 秒ごとに呼ばれるため trace に落とす
        trace!("ミュートを設定する: {}", muted);
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// いまの音量をパーセントで返す。
    ///
    /// **最小化中のホットキーで増減の基準にするために置いてある**
    /// （`app::worker_loop::adjust_volume`）。普段は UI スレッドが持つ値が
    /// 正で、ここを読む必要はない。内部は 0.0〜2.0 の倍率なので、戻す際に
    /// 端数が動きうる（60% が 60.000004% になる程度）
    pub fn volume_percent(&self) -> f32 {
        load_volume(&self.volume) * 100.0
    }

    /// ミュート中か。`volume_percent` と同じく最小化中のホットキー用。
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

/// 共有している音量へ書き込む。
fn store_volume(cell: &AtomicU32, volume: f32) {
    cell.store(volume.to_bits(), Ordering::Relaxed);
}

/// 共有している音量を読み出す。
pub(super) fn load_volume(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

/// パーセント指定の音量を、出力サンプルに掛ける倍率へ直す。
///
/// 設定ファイルは手で編集できるため、範囲外の値や `nan` も入りうる。
/// NaN をそのまま掛けると出力が全て NaN になり、デバイスによっては
/// 耳障りな雑音になるので、有限でない値は既定値へ倒す。
fn normalize_volume(volume_percent: f32) -> f32 {
    if !volume_percent.is_finite() {
        return DEFAULT_VOLUME;
    }
    (volume_percent / 100.0).clamp(0.0, 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_volume_percent_maps_to_multiplier() {
        assert_eq!(normalize_volume(0.0), 0.0);
        assert_eq!(normalize_volume(100.0), 1.0);
        assert_eq!(normalize_volume(200.0), 2.0);
    }

    #[test]
    fn normalize_volume_out_of_range_is_clamped() {
        // 設定ファイルを手で編集すれば UI の上限を超えた値も入る
        assert_eq!(normalize_volume(-50.0), 0.0);
        assert_eq!(normalize_volume(1000.0), 2.0);
    }

    #[test]
    fn normalize_volume_non_finite_falls_back_to_default() {
        // NaN を掛けると出力が全て NaN になるので既定値へ倒す
        assert_eq!(normalize_volume(f32::NAN), DEFAULT_VOLUME);
        assert_eq!(normalize_volume(f32::INFINITY), DEFAULT_VOLUME);
    }

    #[test]
    fn store_volume_and_load_volume_round_trip() {
        // 出力コールバックは f32 をビット表現のまま受け取る。
        // 0.0 が別の値に化けると、音量 0% でも音が出てしまう
        let cell = AtomicU32::new(DEFAULT_VOLUME.to_bits());
        store_volume(&cell, 0.0);
        assert_eq!(load_volume(&cell), 0.0);
        store_volume(&cell, 1.75);
        assert_eq!(load_volume(&cell), 1.75);
    }
}
