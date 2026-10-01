//! サンプル型の変換。
//!
//! リングバッファの内部表現は f32 に統一してあるので、デバイス側のサンプル型は
//! 入力で f32 へ正規化し、出力で書き戻す（`i16_to_f32` などの一連の関数）。
//! 入出力の形（レート・チャンネル数）の違いは `convert::PassthroughConverter` が吸収する。

/// 整数サンプルの振幅の基準。f32 の -1.0 が型の最小値、+1.0 が最大値 + 1 に対応する。
/// 2 のべき乗なので f32 の除算・乗算で誤差が出ない。
const I16_SCALE: f32 = 32_768.0;
const I32_SCALE: f32 = 2_147_483_648.0;
/// u16 の原点。無音は 0 ではなく 32768。
const U16_ORIGIN: f32 = 32_768.0;

/// i16 のサンプルを f32（-1.0..1.0）へ正規化する。
pub(super) fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / I16_SCALE
}

/// f32 のサンプルを i16 へ変換する。
///
/// 音量 200% では 1.0 を超える値が来る。Rust の float → int キャストは飽和するので、
/// 折り返して最大音量が最小音量に化けることはない。
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    (sample * I16_SCALE) as i16
}

/// u16 のサンプルを f32（-1.0..1.0）へ正規化する。
///
/// u16 は 32768 が原点なので、そのまま符号付きとして読むと最大振幅の直流になる。
pub(super) fn u16_to_f32(sample: u16) -> f32 {
    (sample as f32 - U16_ORIGIN) / U16_ORIGIN
}

/// f32 のサンプルを u16 へ変換する。
pub(super) fn f32_to_u16(sample: f32) -> u16 {
    (sample * U16_ORIGIN + U16_ORIGIN) as u16
}

/// i32 のサンプルを f32（-1.0..1.0）へ正規化する。
pub(super) fn i32_to_f32(sample: i32) -> f32 {
    sample as f32 / I32_SCALE
}

/// f32 のサンプルを i32 へ変換する。
pub(super) fn f32_to_i32(sample: f32) -> i32 {
    (sample * I32_SCALE) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(-32768), -1.0);
        assert_eq!(i16_to_f32(32767), 0.999_969_5);
        assert_eq!(i16_to_f32(16384), 0.5);
    }

    #[test]
    fn f32_to_i16_boundaries_saturate() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(-1.0), -32768);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(0.5), 16384);
    }

    #[test]
    fn f32_to_i16_out_of_range_clamps_instead_of_wrapping() {
        // 音量 200% で 1.0 のサンプルが 2.0 になることがある。
        // 折り返すと最大音量が最小音量に化けるため、飽和させる
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32768);
    }

    #[test]
    fn u16_to_f32_midpoint_is_silence() {
        // u16 は 32768 が原点。ここを 0.0 にできないと無音が直流になる
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(u16_to_f32(0), -1.0);
        assert_eq!(u16_to_f32(65535), 0.999_969_5);
        assert_eq!(u16_to_f32(49152), 0.5);
    }

    #[test]
    fn f32_to_u16_boundaries_map_to_full_range() {
        assert_eq!(f32_to_u16(0.0), 32768);
        assert_eq!(f32_to_u16(-1.0), 0);
        assert_eq!(f32_to_u16(1.0), 65535);
        assert_eq!(f32_to_u16(0.5), 49152);
    }

    #[test]
    fn f32_to_u16_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_u16(2.0), 65535);
        assert_eq!(f32_to_u16(-2.0), 0);
    }

    #[test]
    fn i32_to_f32_boundaries_map_to_unit_range() {
        assert_eq!(i32_to_f32(0), 0.0);
        assert_eq!(i32_to_f32(-2147483648), -1.0);
        assert_eq!(i32_to_f32(1073741824), 0.5);
    }

    #[test]
    fn f32_to_i32_boundaries_saturate() {
        assert_eq!(f32_to_i32(0.0), 0);
        assert_eq!(f32_to_i32(-1.0), -2147483648);
        assert_eq!(f32_to_i32(1.0), 2147483647);
        assert_eq!(f32_to_i32(0.5), 1073741824);
    }

    #[test]
    fn f32_to_i32_out_of_range_clamps_instead_of_wrapping() {
        assert_eq!(f32_to_i32(2.0), 2147483647);
        assert_eq!(f32_to_i32(-2.0), -2147483648);
    }

    #[test]
    fn u16_silence_read_as_i16_becomes_full_scale_dc() {
        // 修正前は F32 以外をすべて i16 として扱っていた。
        // u16 の無音 (32768) を i16 として読むと最大振幅の直流になり、
        // ストリームが構築できた場合でも正しい音にならない
        assert_eq!(u16_to_f32(32768), 0.0);
        assert_eq!(i16_to_f32(32768u16 as i16), -1.0);
    }

    #[test]
    fn f32_round_trip_preserves_sample_within_quantization_error() {
        // リングバッファの表現は f32。入力側で正規化した値が出力側で元の量子化値へ戻る
        for raw in [-32768i16, -1, 0, 1, 32767] {
            assert_eq!(f32_to_i16(i16_to_f32(raw)), raw);
        }
        for raw in [0u16, 1, 32768, 65535] {
            assert_eq!(f32_to_u16(u16_to_f32(raw)), raw);
        }
    }
}
