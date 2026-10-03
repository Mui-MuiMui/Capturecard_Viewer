//! Media Foundation の経路で使う、形式の対応表とフレームの渡し先の判定（#81）。
//!
//! 設定画面の形式名（`video.format`）と、nokhwa へ要求する `FrameFormat` を
//! **1 つの表（`MF_FORMATS`）で対応させる。** 能力の一覧に出す形式
//! （`capabilities.rs`）と、開くときに要求する形式（`capture.rs`）が同じ表を引くので、
//! 「一覧に出るのに開けない」「開けるのに一覧に出ない」が起きない。
//!
//! nokhwa-bindings-windows 0.4.6 は Media Foundation のサブタイプをこう対応させている
//! （`guid_to_frameformat`）。ここに無いサブタイプは列挙にも出ず、開けもしない。
//!
//! | Media Foundation | nokhwa の `FrameFormat` |
//! |---|---|
//! | `MFVideoFormat_YUY2` | `YUYV` |
//! | `MFVideoFormat_NV12` | `NV12` |
//! | `MFVideoFormat_MJPG` | `MJPEG` |
//! | `MFVideoFormat_RGB24` | `RAWBGR`（`RAWRGB` ではない） |
//! | `MFVideoFormat_L8` | `GRAY` |
//!
//! 判定はどれも純粋関数。フェイクには Media Foundation の形式が無いので、
//! 実際に開く経路はテストで通せない。

use nokhwa::utils::FrameFormat;

use super::yuv420::Yuv420Layout;

/// 設定画面の形式名と、Media Foundation で要求する `FrameFormat` の対応。
///
/// **並びは能力の一覧に出す順。** 係数表（色空間・レンジ・映像調整）の効く形式を
/// 先にしてある（DirectShow の `SampleKind` の並びと同じ考え方）。GRAY は
/// キャプチャーボードが出さないので載せていない。
pub(super) const MF_FORMATS: [(&str, FrameFormat); 4] = [
    ("YUY2", FrameFormat::YUYV),
    ("NV12", FrameFormat::NV12),
    ("MJPEG", FrameFormat::MJPEG),
    // Media Foundation の RGB24 は B・G・R の順に並ぶので、nokhwa は RAWBGR と呼ぶ。
    // RAWRGB で引くと一覧が空の `Ok` になり、RGB24 が選択肢に出なかった
    ("RGB24", FrameFormat::RAWBGR),
];

/// 形式が未指定のとき、または要求した形式で開けなかったときに使う形式
pub(super) const FALLBACK_FORMAT: FrameFormat = FrameFormat::YUYV;

/// 設定画面の形式名から、要求する `FrameFormat` を引く。表に無ければ `None`
pub(super) fn frame_format_for(name: &str) -> Option<FrameFormat> {
    MF_FORMATS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|&(_, format)| format)
}

/// `FrameFormat` を設定画面と同じ語彙の表示名へ直す。
///
/// `MF_FORMATS` にある形式は表と同じ名前（テストで突き合わせている）。表に無いもの
/// （GRAY と、Media Foundation からは来ない RAWRGB）は nokhwa の名前のまま
pub(super) fn format_name(format: FrameFormat) -> &'static str {
    match format {
        FrameFormat::YUYV => "YUY2",
        FrameFormat::NV12 => "NV12",
        FrameFormat::MJPEG => "MJPEG",
        FrameFormat::RAWBGR => "RGB24",
        FrameFormat::GRAY => "GRAY",
        FrameFormat::RAWRGB => "RAWRGB",
    }
}

/// 設定の形式から決めた、nokhwa へ要求する形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FormatRequest<'a> {
    /// 要求する形式
    pub(super) format: FrameFormat,
    /// 設定の形式名が表に無く、YUY2 で代えたときの元の名前。
    ///
    /// DirectShow のデバイスで選んだ I420 / YV12 が、同じ名前の Media Foundation の
    /// デバイスへ切り替えたときに残っている場合など
    pub(super) unknown: Option<&'a str>,
}

/// 設定の形式名から、要求する形式を決める。未指定（空も含む）は YUY2。
pub(super) fn request_for(name: Option<&str>) -> FormatRequest<'_> {
    match name.filter(|name| !name.is_empty()) {
        None => FormatRequest {
            format: FALLBACK_FORMAT,
            unknown: None,
        },
        Some(name) => match frame_format_for(name) {
            Some(format) => FormatRequest {
                format,
                unknown: None,
            },
            None => FormatRequest {
                format: FALLBACK_FORMAT,
                unknown: Some(name),
            },
        },
    }
}

/// 要求した形式で開けなかったときに、代わりに開き直す形式。
///
/// YUY2 以外で失敗したときだけ YUY2 を返す。YUY2 で失敗したなら、形式ではなく
/// デバイス側（使用中・抜けた）の問題なので、開き直しても同じ結果になる。
pub(super) fn fallback_for(requested: FrameFormat) -> Option<FrameFormat> {
    (requested != FALLBACK_FORMAT).then_some(FALLBACK_FORMAT)
}

/// 届いたフレームを `FrameSink` のどの受け口へ渡すか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SinkRoute {
    /// YUY2 の高速パス（`push_yuy2`）
    Yuy2,
    /// 4:2:0 の YUV（`push_yuv420`）。Media Foundation からは NV12 だけ
    Yuv420(Yuv420Layout),
    /// MJPEG の展開（`push_mjpeg`）
    Mjpeg,
    /// B・G・R の並びの RGB24（`push_bgr24`）
    Bgr24,
    /// nokhwa のデコーダ（`push_decoded`）。上のどれにも当たらないもの
    Decoder,
}

/// 届いたフレームの形式と幅から、渡し先を決める。
///
/// YUY2 は 2 画素で 1 組なので、幅が奇数なら高速パスに通さずデコーダへ倒す
/// （以前からの扱い）。
pub(super) fn sink_route(format: FrameFormat, width: usize) -> SinkRoute {
    match format {
        FrameFormat::YUYV if width.is_multiple_of(2) => SinkRoute::Yuy2,
        FrameFormat::NV12 => SinkRoute::Yuv420(Yuv420Layout::Nv12),
        FrameFormat::MJPEG => SinkRoute::Mjpeg,
        FrameFormat::RAWBGR => SinkRoute::Bgr24,
        _ => SinkRoute::Decoder,
    }
}

/// Media Foundation の RGB24 を下の行から並んだものとして読むか。
///
/// **nokhwa は `MF_MT_DEFAULT_STRIDE`（負なら下から）を読まず、渡す手段も無い。**
/// nokhwa 自身のデコーダは上から並んだものとして読むので、それに合わせる。
/// 実機で上下が逆さまに出たら、ここを `true` にする（`docs/MANUAL-TEST.md`）
pub(super) const MF_RGB24_BOTTOM_UP: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_format_for_maps_every_listed_name() {
        assert_eq!(frame_format_for("YUY2"), Some(FrameFormat::YUYV));
        assert_eq!(frame_format_for("NV12"), Some(FrameFormat::NV12));
        assert_eq!(frame_format_for("MJPEG"), Some(FrameFormat::MJPEG));
        assert_eq!(frame_format_for("I420"), None);
    }

    #[test]
    fn frame_format_for_rgb24_is_rawbgr_like_the_bindings() {
        // nokhwa-bindings-windows 0.4.6 は MF_VIDEO_FORMAT_RGB24 を RAWBGR に対応させる。
        // RAWRGB で引くと一覧が空になり、開くときも合うものが見つからない
        assert_eq!(frame_format_for("RGB24"), Some(FrameFormat::RAWBGR));
    }

    #[test]
    fn format_name_round_trips_through_the_table() {
        for (name, format) in MF_FORMATS {
            assert_eq!(format_name(format), name);
            assert_eq!(frame_format_for(name), Some(format));
        }
    }

    #[test]
    fn format_name_outside_the_table_keeps_the_nokhwa_name() {
        assert_eq!(format_name(FrameFormat::GRAY), "GRAY");
        assert_eq!(format_name(FrameFormat::RAWRGB), "RAWRGB");
    }

    #[test]
    fn request_for_unspecified_is_yuy2() {
        let expected = FormatRequest {
            format: FrameFormat::YUYV,
            unknown: None,
        };
        assert_eq!(request_for(None), expected);
        assert_eq!(request_for(Some("")), expected);
    }

    #[test]
    fn request_for_listed_format_requests_it_as_is() {
        // 以前は MJPEG / RGB24 を選んでも YUYV を要求していた（#81）
        assert_eq!(
            request_for(Some("MJPEG")),
            FormatRequest {
                format: FrameFormat::MJPEG,
                unknown: None,
            }
        );
        assert_eq!(request_for(Some("NV12")).format, FrameFormat::NV12);
        assert_eq!(request_for(Some("RGB24")).format, FrameFormat::RAWBGR);
    }

    #[test]
    fn request_for_unknown_name_falls_back_to_yuy2_and_keeps_the_name() {
        assert_eq!(
            request_for(Some("I420")),
            FormatRequest {
                format: FrameFormat::YUYV,
                unknown: Some("I420"),
            }
        );
    }

    #[test]
    fn fallback_for_non_yuy2_is_yuy2() {
        assert_eq!(fallback_for(FrameFormat::MJPEG), Some(FrameFormat::YUYV));
        assert_eq!(fallback_for(FrameFormat::NV12), Some(FrameFormat::YUYV));
        assert_eq!(fallback_for(FrameFormat::RAWBGR), Some(FrameFormat::YUYV));
    }

    #[test]
    fn fallback_for_yuy2_does_not_retry() {
        // 形式ではなくデバイス側の問題なので、同じ形式で開き直しても変わらない
        assert_eq!(fallback_for(FrameFormat::YUYV), None);
    }

    #[test]
    fn sink_route_sends_each_format_to_its_receiver() {
        assert_eq!(sink_route(FrameFormat::YUYV, 1920), SinkRoute::Yuy2);
        assert_eq!(
            sink_route(FrameFormat::NV12, 1920),
            SinkRoute::Yuv420(Yuv420Layout::Nv12)
        );
        assert_eq!(sink_route(FrameFormat::MJPEG, 1920), SinkRoute::Mjpeg);
        assert_eq!(sink_route(FrameFormat::RAWBGR, 1920), SinkRoute::Bgr24);
    }

    #[test]
    fn sink_route_odd_width_yuy2_goes_to_the_decoder() {
        assert_eq!(sink_route(FrameFormat::YUYV, 1279), SinkRoute::Decoder);
    }

    #[test]
    fn sink_route_formats_without_a_receiver_go_to_the_decoder() {
        assert_eq!(sink_route(FrameFormat::GRAY, 640), SinkRoute::Decoder);
        assert_eq!(sink_route(FrameFormat::RAWRGB, 640), SinkRoute::Decoder);
    }

    #[test]
    fn every_listed_format_has_a_dedicated_receiver() {
        // 一覧に出す形式は、どれもデコーダ任せにならない
        for (_, format) in MF_FORMATS {
            assert_ne!(sink_route(format, 1920), SinkRoute::Decoder);
        }
    }
}
