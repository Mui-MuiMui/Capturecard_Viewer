//! デバイスを切り替えたときに選び直すビデオの既定値。
//!
//! 解像度と FPS は切り替え前の値に最も近い組み合わせを選ぶ
//! （`docs/design/video-pipeline.md`）。

use crate::video::{DeviceCapabilities, VideoMode};

/// デバイスを切り替えたときに選び直すビデオの既定値を決める。
///
/// 返すのは `(フォーマット名, (幅, 高さ), fps)`。能力が空なら `None` を返し、
/// 呼び出し側は設定を触らない。
///
/// フォーマットは能力一覧の先頭（組み合わせを 1 つ以上持つもの）を採る。
/// `video::get_device_capabilities` が YUY2 → MJPEG → RGB24 の順で積むため、
/// 実質 YUY2 が優先される。
///
/// 解像度と FPS は**切り替え前の値に最も近い組み合わせ**を選ぶ。デバイスを
/// 替えただけで 1080p60 が 640x480 まで落ちると使い物にならないため、
/// 直前の設定を手掛かりにする。前の値が無い（初回など）ときは、対応する中で
/// 最大の解像度・最高の FPS を選ぶ。
///
/// **フォーマットだけを入れ直してはいけない。** 解像度と FPS が前のデバイスの
/// 値のまま残ると、新しいデバイスが対応していない組み合わせが画面に出て、
/// `start_capture` が `Closest` で寄せた実際の設定と表示が食い違う。
pub fn select_default_video_mode(
    caps: &DeviceCapabilities,
    previous_resolution: Option<(u32, u32)>,
    previous_fps: Option<u32>,
) -> Option<(String, (u32, u32), u32)> {
    // 組み合わせを持たないフォーマットを選ぶと、解像度の選択肢が空になる
    let capability = caps
        .iter()
        .find(|capability| !capability.modes.is_empty())?;

    let &mode = capability
        .modes
        .iter()
        .min_by_key(|&&mode| video_mode_rank(mode, previous_resolution, previous_fps))?;

    Some((capability.name.clone(), mode.resolution(), mode.fps))
}

/// `select_default_video_mode` の並べ替えキー。小さいほど「望ましい」。
///
/// 1. 前の解像度との画素数の差（前の値が無ければ全て 0 で並ばない）
/// 2. 前の解像度との幅・高さの差の和（同上）
/// 3. 前の FPS との差（同上）
/// 4. 画素数の降順
/// 5. FPS の降順
///
/// 2 が要るのは、画素数だけでは縦横比の違う同面積の解像度が並んでしまうため。
/// 1280x720 と 960x960 はどちらも 921,600 画素なので、前の解像度に完全一致
/// する側が一覧の後ろにあると取りこぼす。
///
/// 4 と 5 は「前の値が無いときは最大の解像度・最高の FPS」という既定であり、
/// 同時に 1〜3 が並んだときの決着でもある。ここが無いと `HashMap` 由来の
/// 順序でフレームごとに違う値が選ばれうる。
fn video_mode_rank(
    mode: VideoMode,
    previous_resolution: Option<(u32, u32)>,
    previous_fps: Option<u32>,
) -> (
    u64,
    u64,
    u64,
    std::cmp::Reverse<u64>,
    std::cmp::Reverse<u32>,
) {
    let pixels = mode.pixel_count();

    let (pixel_distance, dimension_distance) = match previous_resolution {
        Some((prev_width, prev_height)) => (
            pixels.abs_diff(u64::from(prev_width) * u64::from(prev_height)),
            u64::from(mode.width.abs_diff(prev_width))
                + u64::from(mode.height.abs_diff(prev_height)),
        ),
        None => (0, 0),
    };
    let fps_distance = match previous_fps {
        Some(prev_fps) => u64::from(mode.fps.abs_diff(prev_fps)),
        None => 0,
    };

    (
        pixel_distance,
        dimension_distance,
        fps_distance,
        std::cmp::Reverse(pixels),
        std::cmp::Reverse(mode.fps),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::video::{DeviceCapabilities, FormatCapability, VideoMode};

    #[test]
    fn select_default_video_mode_without_previous_takes_largest_resolution_and_fps() {
        // 前の値が無いとき（初回など）は、対応する中で最大の解像度・最高の FPS
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "MJPEG",
            vec![
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1920, 1080, 24),
                VideoMode::new(1920, 1080, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, None, None),
            Some(("MJPEG".to_string(), (1920, 1080), 30))
        );
    }

    #[test]
    fn select_default_video_mode_keeps_previous_when_supported() {
        // 新しいデバイスが同じ組み合わせに対応していれば、そのまま据え置く
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_keeps_previous_over_same_pixel_count_resolution() {
        // 960x960 と 1280x720 はどちらも 921,600 画素で、画素数の差だけでは並ぶ。
        // 完全一致する 1280x720 が一覧の後ろにあっても取りこぼさないこと
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![VideoMode::new(960, 960, 60), VideoMode::new(1280, 720, 60)],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_picks_nearest_resolution() {
        // 1600x900（1,440,000 画素）に最も近いのは 1280x720（921,600 画素）。
        // 1920x1080 は 2,073,600 画素で差が大きい
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1600, 900)), Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_prefers_resolution_over_fps() {
        // 解像度が先。FPS を合わせるために解像度を落とさない
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(640, 480, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1920, 1080)), Some(60)),
            Some(("YUY2".to_string(), (1920, 1080), 30))
        );
    }

    #[test]
    fn select_default_video_mode_picks_nearest_fps_within_same_resolution() {
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1280, 720, 30),
                VideoMode::new(1280, 720, 24),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(25)),
            Some(("YUY2".to_string(), (1280, 720), 24))
        );
    }

    #[test]
    fn select_default_video_mode_equal_distance_takes_larger_resolution() {
        // 1,000,000 画素からの差がどちらも 200,000 で並ぶ。
        // 決着を付けないとフレームごとに違う値が選ばれうる
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(800, 1000, 30),
                VideoMode::new(1200, 1000, 30),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1000, 1000)), Some(30)),
            Some(("YUY2".to_string(), (1200, 1000), 30))
        );
    }

    #[test]
    fn select_default_video_mode_without_previous_fps_takes_highest_for_that_resolution() {
        // 解像度だけ分かっているとき。FPS は差で並ばないので最高のものになる
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1280, 720, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1920, 1080, 60),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), None),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_without_previous_resolution_takes_nearest_fps() {
        // FPS だけ分かっているとき。解像度は差で並ばないので、まず FPS が合う
        let caps: DeviceCapabilities = vec![FormatCapability::new(
            "YUY2",
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 60),
            ],
        )];

        assert_eq!(
            select_default_video_mode(&caps, None, Some(60)),
            Some(("YUY2".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_uses_first_format_even_if_another_matches_better() {
        // フォーマットは能力一覧の先頭を採る（YUY2 優先）。
        // 解像度と FPS はそのフォーマットが対応する中から選ぶので、
        // 他のフォーマットにもっと近い組み合わせがあっても移らない
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![VideoMode::new(640, 480, 30)]),
            FormatCapability::new("MJPEG", vec![VideoMode::new(1920, 1080, 60)]),
        ];

        assert_eq!(
            select_default_video_mode(&caps, Some((1920, 1080)), Some(60)),
            Some(("YUY2".to_string(), (640, 480), 30))
        );
    }

    #[test]
    fn select_default_video_mode_skips_format_without_any_mode() {
        // 組み合わせを持たないフォーマットを選ぶと、解像度の選択肢が空になる
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![]),
            FormatCapability::new("MJPEG", vec![VideoMode::new(1280, 720, 60)]),
        ];

        assert_eq!(
            select_default_video_mode(&caps, None, None),
            Some(("MJPEG".to_string(), (1280, 720), 60))
        );
    }

    #[test]
    fn select_default_video_mode_returns_none_for_empty_capabilities() {
        // 呼び出し側は設定を触らない。空の値で上書きしない
        let caps: DeviceCapabilities = Vec::new();

        assert_eq!(
            select_default_video_mode(&caps, Some((1280, 720)), Some(60)),
            None
        );
    }

    #[test]
    fn select_default_video_mode_returns_none_when_every_format_is_empty() {
        let caps: DeviceCapabilities = vec![
            FormatCapability::new("YUY2", vec![]),
            FormatCapability::new("MJPEG", vec![]),
        ];

        assert_eq!(select_default_video_mode(&caps, None, None), None);
    }
}
