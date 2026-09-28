//! エンコード済みのデータの中身を読む（③ リプレイバッファ）。純粋関数。
//!
//! エンコーダ MFT が出す H.264 は Annex B（`00 00 01` / `00 00 00 01` の開始コードで
//! NAL ユニットを区切る形）。ここで読むのは 2 つだけ。
//!
//! - **そのサンプルが IDR（そこから復号を始められるキーフレーム）か。** リングから古いものを
//!   捨てる境界と、録画の先頭に選ぶ位置になる。`MFSampleExtension_CleanPoint` を付けない
//!   エンコーダもあるので、中身で確かめる
//! - **SPS / PPS。** エンコードなしの Sink Writer に渡す `MF_MT_MPEG_SEQUENCE_HEADER` が
//!   エンコーダの出力のメディアタイプに無いとき、最初のキーフレームから取り出す
//!
//! AAC の `MF_MT_USER_DATA` を組み立てる予備の関数もここに置く（`aac_user_data`）。

/// NAL ユニットの種類（`nal_unit_type`、先頭のバイトの下位 5 ビット）
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// Annex B の NAL ユニットを、開始コードを除いた中身で順に返す。
/// 開始コードが 1 つも無ければ空（長さ前置きの形など、Annex B ではない）。
pub(super) fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let mut end = starts
                .get(position + 1)
                .map_or(data.len(), |&next| next - 3);
            // 4 バイトの開始コード（00 00 00 01）の先頭の 0 と、末尾の詰め物の 0 は中身に含めない
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            &data[start..end]
        })
        .filter(|unit| !unit.is_empty())
        .collect()
}

fn nal_type(unit: &[u8]) -> u8 {
    unit[0] & 0x1f
}

/// IDR の NAL ユニットを含むか。Annex B として読めなければ `None`（呼び出し側は
/// `MFSampleExtension_CleanPoint` に頼る）。
pub(super) fn is_idr(data: &[u8]) -> Option<bool> {
    let units = nal_units(data);
    if units.is_empty() {
        return None;
    }
    Some(units.iter().any(|unit| nal_type(unit) == NAL_IDR))
}

/// SPS と PPS を開始コード（`00 00 00 01`）付きで並べたもの。`MF_MT_MPEG_SEQUENCE_HEADER` の形。
/// どちらかが無ければ `None`。
pub(super) fn parameter_sets(data: &[u8]) -> Option<Vec<u8>> {
    let units = nal_units(data);
    let sps: Vec<&[u8]> = units
        .iter()
        .copied()
        .filter(|unit| nal_type(unit) == NAL_SPS)
        .collect();
    let pps: Vec<&[u8]> = units
        .iter()
        .copied()
        .filter(|unit| nal_type(unit) == NAL_PPS)
        .collect();
    if sps.is_empty() || pps.is_empty() {
        return None;
    }
    let mut header = Vec::new();
    for unit in sps.iter().chain(pps.iter()) {
        header.extend_from_slice(&[0, 0, 0, 1]);
        header.extend_from_slice(unit);
    }
    Some(header)
}

/// AAC-LC の `MF_MT_USER_DATA`。`HEAACWAVEINFO` のうち `WAVEFORMATEX` より後ろの 12 バイト
/// （生の AAC、プロファイルの指定なし）に、AudioSpecificConfig の 2 バイトを続けたもの。
///
/// Microsoft の AAC エンコーダは出力のメディアタイプにこれを付けるので、使うのは
/// 付いていなかったときだけ。表に無いレートなら `None`。
pub(super) fn aac_user_data(sample_rate: u32, channels: u16) -> Option<Vec<u8>> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let frequency_index = RATES.iter().position(|&rate| rate == sample_rate)? as u16;
    if channels == 0 || channels > 7 {
        return None;
    }
    // 5 ビットのオブジェクトタイプ（2 = AAC-LC）、4 ビットのレートの番号、
    // 4 ビットのチャンネル構成、残り 3 ビットは 0
    let config: u16 = (2 << 11) | (frequency_index << 7) | (channels << 3);
    let mut data = vec![
        0, 0, // wPayloadType: 0 = 生の AAC
        0xfe, 0, // wAudioProfileLevelIndication: 0xFE = 指定なし
        0, 0, // wStructType: 0 = 後ろに AudioSpecificConfig が続く
        0, 0, // wReserved1
        0, 0, 0, 0, // dwReserved2
    ];
    data.extend_from_slice(&config.to_be_bytes());
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPS（67 ..）、PPS（68 ..）、IDR のスライス（65 ..）を 4 バイトと 3 バイトの開始コードで並べたもの
    const IDR_ACCESS_UNIT: [u8; 19] = [
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, //
        0, 0, 0, 1, 0x68, 0xee, //
        0, 0, 1, 0x65, 0x88,
    ];

    #[test]
    fn nal_units_splits_three_and_four_byte_start_codes() {
        let units = nal_units(&IDR_ACCESS_UNIT);
        assert_eq!(
            units,
            vec![
                &[0x67, 0x64, 0x00, 0x28][..],
                &[0x68, 0xee][..],
                &[0x65, 0x88][..]
            ]
        );
    }

    #[test]
    fn nal_units_without_a_start_code_is_empty() {
        // 長さ前置き（AVCC）の形は Annex B として読まない
        assert!(nal_units(&[0, 0, 0, 2, 0x65, 0x88]).is_empty());
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn nal_units_drops_trailing_zero_padding() {
        let units = nal_units(&[0, 0, 1, 0x41, 0x9a, 0, 0]);
        assert_eq!(units, vec![&[0x41, 0x9a][..]]);
    }

    #[test]
    fn is_idr_detects_idr_slices_only() {
        assert_eq!(is_idr(&IDR_ACCESS_UNIT), Some(true));
        // P スライス（41）だけ
        assert_eq!(is_idr(&[0, 0, 0, 1, 0x41, 0x9a]), Some(false));
        // Annex B ではない
        assert_eq!(is_idr(&[0x41, 0x9a]), None);
    }

    #[test]
    fn parameter_sets_collects_sps_and_pps_with_start_codes() {
        assert_eq!(
            parameter_sets(&IDR_ACCESS_UNIT),
            Some(vec![
                0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xee
            ])
        );
    }

    #[test]
    fn parameter_sets_needs_both_sps_and_pps() {
        assert_eq!(parameter_sets(&[0, 0, 0, 1, 0x67, 0x64]), None);
        assert_eq!(parameter_sets(&[0, 0, 0, 1, 0x68, 0xee]), None);
        assert_eq!(parameter_sets(&[0, 0, 0, 1, 0x41, 0x9a]), None);
    }

    #[test]
    fn aac_user_data_for_48k_stereo_ends_with_the_known_config() {
        // AAC-LC 48kHz 2ch の AudioSpecificConfig は 0x11 0x90
        let data = aac_user_data(48_000, 2).expect("48kHz は表にある");
        assert_eq!(data.len(), 14);
        assert_eq!(&data[12..], &[0x11, 0x90]);
        assert_eq!(&data[..4], &[0, 0, 0xfe, 0]);
    }

    #[test]
    fn aac_user_data_for_44k_mono() {
        // 44.1kHz（番号 4）1ch: 00010 0100 0001 000 = 0x12 0x08
        let data = aac_user_data(44_100, 1).expect("44.1kHz は表にある");
        assert_eq!(&data[12..], &[0x12, 0x08]);
    }

    #[test]
    fn aac_user_data_rejects_unknown_rates_and_channel_counts() {
        assert_eq!(aac_user_data(47_999, 2), None);
        assert_eq!(aac_user_data(48_000, 0), None);
        assert_eq!(aac_user_data(48_000, 8), None);
    }
}
