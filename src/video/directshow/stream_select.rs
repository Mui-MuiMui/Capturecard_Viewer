//! DirectShow の対応形式の一覧から「どれで開くか」を決める判定（#414 で `devices.rs` から分けた）。
//!
//! 開く解像度（`target_resolution`）、fps の範囲と選択肢（`fps_range` / `fps_list` /
//! `fps_choices`）、設定画面向けの形への並べ替え（`capabilities_from_candidates`）、
//! 開く候補の選び方（`choose_candidate`）。どれも純粋関数で、デバイスなしで単体テストできる。

use super::devices::StreamCandidate;
use super::media_type::{fps_from_interval, SampleKind};
use crate::video::capabilities::{DeviceCapabilities, FormatCapability, VideoMode};

/// 解像度が未指定のときに開く形。Media Foundation の経路（`VideoCapture`）と同じ
pub(super) const DEFAULT_RESOLUTION: (u32, u32) = (1280, 720);
pub(super) const DEFAULT_FPS: u32 = 60;

/// 受け付ける fps の範囲。Media Foundation の経路と同じ
pub(super) const MIN_FPS: u32 = 15;
pub(super) const MAX_FPS: u32 = 120;

/// 開く解像度を決める（#391）。`choose_candidate` へ渡す解像度を返す。
///
/// 入力信号と違う解像度で開くと、映像の代わりにボード自前の警告画面
/// （「Signal Out of Range」）を出すボードがある（AVerMedia GC551）。警告画面も
/// 正常なフレームとして届くのでアプリからは見分けられない。そこで、要求が
/// このデバイスで開けないときは、ドライバーが返すいまの解像度（`current`、
/// 入力信号を映していることが多い）を使う。
///
/// - 要求した解像度の候補がある（形式を指定していてその形式があれば、その形式の
///   中で）なら、要求のまま。**利用者が選んだ解像度は上書きしない**
/// - 要求が無い、または候補に無いなら、`current` が候補にあればそれ
/// - どちらでもなければ要求のまま（`choose_candidate` が近いものを選ぶ）
pub(super) fn target_resolution(
    candidates: &[StreamCandidate],
    requested: Option<(u32, u32)>,
    format: Option<&str>,
    current: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    let requested_kind = format.and_then(SampleKind::from_name);
    let has_requested_kind = requested_kind.is_some_and(|kind| {
        candidates
            .iter()
            .any(|candidate| candidate.format.kind == kind)
    });
    let listed = |resolution: (u32, u32)| {
        candidates.iter().any(|candidate| {
            (!has_requested_kind || Some(candidate.format.kind) == requested_kind)
                && (candidate.format.width, candidate.format.height) == resolution
        })
    };
    match requested {
        Some(resolution) if listed(resolution) => Some(resolution),
        _ => current
            .filter(|&resolution| listed(resolution))
            .or(requested),
    }
}

/// 範囲を持つデバイスで、設定画面に並べる代表の fps（#389）
const REPRESENTATIVE_FPS: [u32; 7] = [15, 24, 25, 30, 50, 60, 120];

/// `MinFrameInterval` / `MaxFrameInterval` を fps の `(最小, 最大)` に直す。
///
/// 範囲を持たない（両端が同じ fps になる）か、どちらかを読めないなら `None`。
/// 最短の間隔が最大の fps になる。
pub(super) fn fps_range(min_interval: i64, max_interval: i64) -> Option<(u32, u32)> {
    let max = fps_from_interval(min_interval)?;
    let min = fps_from_interval(max_interval)?;
    (min < max).then_some((min, max))
}

/// 1 件の対応形式で開ける fps を並べる。
///
/// メディアタイプの既定値と、`VIDEO_STREAM_CONFIG_CAPS` の最短・最長の間隔を
/// 候補にする。範囲を持つなら、その中にある代表値（`REPRESENTATIVE_FPS`）も
/// 足す（#389）。どれも読めなければ空（その形式は選択肢に出さない）。
pub(super) fn fps_list(avg: i64, min_interval: i64, max_interval: i64) -> Vec<u32> {
    let mut fps: Vec<u32> = [avg, min_interval, max_interval]
        .into_iter()
        .filter_map(fps_from_interval)
        .collect();
    if let Some((min, max)) = fps_range(min_interval, max_interval) {
        fps.extend(
            REPRESENTATIVE_FPS
                .into_iter()
                .filter(|fps| (min..=max).contains(fps)),
        );
    }
    fps.sort_unstable_by(|a, b| b.cmp(a));
    fps.dedup();
    fps
}

/// 設定画面に並べる 1 件の対応形式の fps（#410）。
///
/// 範囲を持つ候補では、どれを選んでも届くのは入力信号の fps なので、
/// `GetFormat` が返すいまの fps（`current_fps`）だけを出す。いまの fps が
/// 読めないか範囲の外なら、範囲の代表値（`StreamCandidate::fps`）へ戻す。
/// 範囲を持たない候補はそのまま。
pub(super) fn fps_choices(candidate: &StreamCandidate, current_fps: Option<u32>) -> Vec<u32> {
    match (candidate.fps_range, current_fps) {
        (Some((min, max)), Some(fps)) if (min..=max).contains(&fps) => vec![fps],
        _ => candidate.fps.clone(),
    }
}

/// 対応形式を、設定画面の「対応形式」に出す形へ並べ替える。
///
/// 形式ごとにまとめ、解像度の大きい順、同じ解像度なら fps の大きい順にする
/// （Media Foundation の経路の `get_device_capabilities` と同じ並び）。
/// 形式の並びは `SampleKind::ALL` の順（YUY2・NV12・I420・YV12・MJPEG・RGB24）。
///
/// `current` はドライバーが返すいまの解像度（`current_resolution`）。その解像度を
/// 開ける形式にだけ `FormatCapability::current_resolution` として添える（#391）。
/// デバイスを切り替えたときの既定（`ui::video_mode`）が前の解像度より優先する。
/// fps の並べ方は `fps_choices`（`current_fps` はいまの fps）。
pub(super) fn capabilities_from_candidates(
    candidates: &[StreamCandidate],
    current: Option<(u32, u32)>,
    current_fps: Option<u32>,
) -> DeviceCapabilities {
    let mut result = Vec::new();
    for kind in SampleKind::ALL {
        let mut modes: Vec<VideoMode> = candidates
            .iter()
            .filter(|candidate| candidate.format.kind == kind)
            .flat_map(|candidate| {
                fps_choices(candidate, current_fps)
                    .into_iter()
                    .map(|fps| VideoMode::new(candidate.format.width, candidate.format.height, fps))
            })
            .collect();
        modes.sort_by(|a, b| {
            b.pixel_count()
                .cmp(&a.pixel_count())
                .then(b.width.cmp(&a.width))
                .then(b.fps.cmp(&a.fps))
        });
        modes.dedup();
        if !modes.is_empty() {
            result.push(FormatCapability::new(kind.name(), modes).with_current_resolution(current));
        }
    }
    result
}

/// どの対応形式で開くかを決める。`(候補の添字, 開く fps)`。候補が無ければ `None`。
///
/// 優先順は次のとおり。
///
/// 1. 形式が指定されていて、その形式の候補があれば、その形式だけから選ぶ
/// 2. 解像度が近いもの（画素数の差が小さいもの。一致が最優先）
/// 3. 形式が未指定なら YUY2・NV12・I420・YV12・MJPEG・RGB24 の順（YUV の 4 つだけが
///    色空間と映像調整の効く高速パスを通るため）
/// 4. 開ける fps が要求に近いもの
///
/// fps は、候補が範囲（`fps_range`）を持ちその中にあるなら要求のまま開く
/// （#389）。範囲が無ければ一覧（`fps`）の中で最も近いもの。
///
/// 解像度が未指定なら 1280x720 60fps を要求したものとして扱う（Media
/// Foundation の経路と同じ）。fps は 15〜120 へ丸める。
pub(super) fn choose_candidate(
    candidates: &[StreamCandidate],
    resolution: Option<(u32, u32)>,
    format: Option<&str>,
    fps: Option<u32>,
) -> Option<(usize, u32)> {
    let (width, height) = resolution.unwrap_or(DEFAULT_RESOLUTION);
    let requested_pixels = u64::from(width) * u64::from(height);
    let requested_fps = if resolution.is_some() {
        fps.unwrap_or(DEFAULT_FPS)
    } else {
        DEFAULT_FPS
    }
    .clamp(MIN_FPS, MAX_FPS);
    let requested_kind = format.and_then(SampleKind::from_name);
    let has_requested_kind = requested_kind.is_some_and(|kind| {
        candidates
            .iter()
            .any(|candidate| candidate.format.kind == kind)
    });

    let closest_fps = |candidate: &StreamCandidate| {
        if let Some((min, max)) = candidate.fps_range {
            if (min..=max).contains(&requested_fps) {
                return Some(requested_fps);
            }
        }
        candidate
            .fps
            .iter()
            .copied()
            .min_by_key(|fps| (fps.abs_diff(requested_fps), u32::MAX - fps))
    };

    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            !has_requested_kind || Some(candidate.format.kind) == requested_kind
        })
        .filter_map(|(index, candidate)| {
            let fps = closest_fps(candidate)?;
            let pixels = u64::from(candidate.format.width) * u64::from(candidate.format.height);
            let exact = candidate.format.width == width && candidate.format.height == height;
            let key = (
                !exact,
                pixels.abs_diff(requested_pixels),
                candidate.format.kind,
                fps.abs_diff(requested_fps),
            );
            Some((key, index, fps))
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, index, fps)| (index, fps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::directshow::media_type::SampleFormat;

    fn candidate(
        index: i32,
        kind: SampleKind,
        width: u32,
        height: u32,
        fps: &[u32],
    ) -> StreamCandidate {
        StreamCandidate {
            index,
            format: SampleFormat {
                kind,
                width,
                height,
                bottom_up: false,
                avg_time_per_frame: 0,
            },
            fps: fps.to_vec(),
            fps_range: None,
        }
    }

    /// 範囲を持つ候補。`fps` は `fps_list` が並べたもの
    fn ranged(
        index: i32,
        width: u32,
        height: u32,
        min_interval: i64,
        max_interval: i64,
    ) -> StreamCandidate {
        StreamCandidate {
            fps: fps_list(min_interval, min_interval, max_interval),
            fps_range: fps_range(min_interval, max_interval),
            ..candidate(index, SampleKind::Yuy2, width, height, &[])
        }
    }

    #[test]
    fn fps_range_reads_min_and_max() {
        // GC551: 最短 166666（60.0002fps）、最長 666666（15fps）
        assert_eq!(fps_range(166_666, 666_666), Some((15, 60)));
        // 範囲を持たない
        assert_eq!(fps_range(333_333, 333_333), None);
        // 丸めると同じ fps になるものも範囲なし
        assert_eq!(fps_range(333_333, 333_667), None);
        // 読めない
        assert_eq!(fps_range(0, 666_666), None);
        assert_eq!(fps_range(166_666, 0), None);
    }

    #[test]
    fn fps_list_adds_representatives_inside_the_range() {
        assert_eq!(
            fps_list(166_666, 166_666, 666_666),
            vec![60, 50, 30, 25, 24, 15]
        );
        // 端の値（85）は代表値に無くても残る
        assert_eq!(
            fps_list(117_647, 117_647, 666_666),
            vec![85, 60, 50, 30, 25, 24, 15]
        );
    }

    #[test]
    fn choose_candidate_opens_the_requested_fps_inside_the_range() {
        // #389: GC551 で 30fps を要求したら 60 ではなく 30
        let candidates = vec![ranged(0, 1920, 1080, 166_666, 666_666)];
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(30)),
            Some((0, 30))
        );
        // 一覧に無い値でも範囲の中ならそのまま
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(48)),
            Some((0, 48))
        );
        // 範囲の外は端へ寄る
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(120)),
            Some((0, 60))
        );
    }

    #[test]
    fn choose_candidate_without_range_keeps_the_closest_listed_fps() {
        // 範囲を持たないデバイスは、これまでどおり一覧の中で最も近いもの
        let candidates = vec![candidate(0, SampleKind::Yuy2, 1920, 1080, &[60, 24])];
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(30)),
            Some((0, 24))
        );
    }

    #[test]
    fn capabilities_from_candidates_lists_range_representatives() {
        let caps =
            capabilities_from_candidates(&[ranged(0, 1920, 1080, 166_666, 666_666)], None, None);
        let fps: Vec<u32> = caps[0].modes.iter().map(|mode| mode.fps).collect();
        assert_eq!(fps, vec![60, 50, 30, 25, 24, 15]);
    }

    #[test]
    fn capabilities_from_candidates_lists_only_the_current_fps_inside_the_range() {
        // #410: GC551 は入力が 60Hz なら、どれを選んでも 60fps で届く
        let candidates = vec![
            ranged(0, 1920, 1080, 166_666, 666_666),
            candidate(1, SampleKind::Yuy2, 1280, 720, &[60, 30]),
        ];
        let caps = capabilities_from_candidates(&candidates, Some((1920, 1080)), Some(60));
        let modes: Vec<(u32, u32)> = caps[0].modes.iter().map(|m| (m.width, m.fps)).collect();
        // 範囲を持たない 1280x720 は今までどおり
        assert_eq!(modes, vec![(1920, 60), (1280, 60), (1280, 30)]);
    }

    #[test]
    fn fps_choices_falls_back_to_representatives() {
        let ranged = ranged(0, 1920, 1080, 166_666, 666_666);
        assert_eq!(fps_choices(&ranged, Some(30)), vec![30]);
        // 読めない・範囲の外は代表値
        let all = vec![60, 50, 30, 25, 24, 15];
        assert_eq!(fps_choices(&ranged, None), all);
        assert_eq!(fps_choices(&ranged, Some(120)), all);
        assert_eq!(fps_choices(&ranged, Some(10)), all);
        // 範囲を持たない候補は、いまの fps があっても一覧のまま
        let fixed = candidate(1, SampleKind::Yuy2, 1280, 720, &[60, 30]);
        assert_eq!(fps_choices(&fixed, Some(60)), vec![60, 30]);
    }

    fn sample_candidates() -> Vec<StreamCandidate> {
        vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[30]),
            candidate(1, SampleKind::Yuy2, 1280, 720, &[60, 30]),
            candidate(2, SampleKind::Mjpeg, 1920, 1080, &[60, 30]),
            candidate(3, SampleKind::Rgb24, 640, 480, &[30]),
        ]
    }

    #[test]
    fn choose_candidate_exact_match_wins() {
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("YUY2"), Some(60)),
            Some((1, 60))
        );
    }

    #[test]
    fn choose_candidate_requested_format_takes_priority_over_resolution() {
        // MJPEG を指定したら、解像度が合わなくても MJPEG から選ぶ
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("MJPEG"), Some(60)),
            Some((2, 60))
        );
    }

    #[test]
    fn choose_candidate_unavailable_format_falls_back_to_any_format() {
        // デバイスに無い形式を指定されたら、形式を問わず解像度で選ぶ
        let candidates = vec![candidate(0, SampleKind::Yuy2, 1280, 720, &[30])];
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("MJPEG"), Some(30)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_without_format_prefers_yuy2_at_the_same_resolution() {
        // 同じ 1920x1080 に YUY2 と MJPEG があれば YUY2
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, Some((1920, 1080)), None, Some(30)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_without_format_prefers_420_over_mjpeg() {
        // YUY2 の無い仮想カメラ。同じ解像度なら係数表を通る NV12 を選ぶ
        let candidates = vec![
            candidate(0, SampleKind::Mjpeg, 1280, 720, &[30]),
            candidate(1, SampleKind::I420, 1280, 720, &[30]),
            candidate(2, SampleKind::Nv12, 1280, 720, &[30]),
        ];
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), None, Some(30)),
            Some((2, 30))
        );
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), Some("I420"), Some(30)),
            Some((1, 30))
        );
    }

    #[test]
    fn capabilities_from_candidates_lists_420_after_yuy2() {
        let candidates = vec![
            candidate(0, SampleKind::Rgb24, 640, 480, &[30]),
            candidate(1, SampleKind::I420, 640, 480, &[30]),
            candidate(2, SampleKind::Nv12, 640, 480, &[30]),
            candidate(3, SampleKind::Yuy2, 640, 480, &[30]),
        ];
        let names: Vec<String> = capabilities_from_candidates(&candidates, None, None)
            .into_iter()
            .map(|capability| capability.name)
            .collect();
        assert_eq!(names, vec!["YUY2", "NV12", "I420", "RGB24"]);
    }

    #[test]
    fn choose_candidate_without_resolution_requests_720p60() {
        let candidates = sample_candidates();
        assert_eq!(
            choose_candidate(&candidates, None, None, None),
            Some((1, 60))
        );
    }

    #[test]
    fn choose_candidate_picks_the_closest_resolution() {
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[30]),
            candidate(1, SampleKind::Yuy2, 640, 480, &[30]),
        ];
        // 800x600 は 640x480 のほうが画素数が近い
        assert_eq!(
            choose_candidate(&candidates, Some((800, 600)), Some("YUY2"), Some(30)),
            Some((1, 30))
        );
    }

    #[test]
    fn choose_candidate_fps_is_clamped_and_matched_to_the_closest() {
        let candidates = vec![candidate(0, SampleKind::Yuy2, 1280, 720, &[60, 30])];
        // 240 は 120 へ丸められ、開ける中で最も近い 60 になる
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), None, Some(240)),
            Some((0, 60))
        );
        // 5 は 15 へ丸められ、最も近い 30 になる
        assert_eq!(
            choose_candidate(&candidates, Some((1280, 720)), None, Some(5)),
            Some((0, 30))
        );
    }

    #[test]
    fn choose_candidate_empty_list_is_none() {
        assert_eq!(
            choose_candidate(&[], Some((1280, 720)), None, Some(60)),
            None
        );
    }

    #[test]
    fn fps_list_collects_distinct_rates_in_descending_order() {
        // 既定 30fps、最短 60fps、最長 15fps。範囲の中の代表値も並ぶ（#389）
        assert_eq!(
            fps_list(333_333, 166_666, 666_666),
            vec![60, 50, 30, 25, 24, 15]
        );
        // 同じ値は 1 つにする
        assert_eq!(fps_list(333_333, 333_333, 333_333), vec![30]);
        // 読めない値は捨てる
        assert_eq!(fps_list(0, 0, 0), Vec::<u32>::new());
    }

    #[test]
    fn capabilities_from_candidates_groups_by_format_in_fixed_order() {
        let caps = capabilities_from_candidates(&sample_candidates(), None, None);
        let names: Vec<&str> = caps.iter().map(|cap| cap.name.as_str()).collect();
        assert_eq!(names, vec!["YUY2", "MJPEG", "RGB24"]);
        assert_eq!(
            caps[0].modes,
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(1280, 720, 30),
            ]
        );
    }

    #[test]
    fn capabilities_from_candidates_removes_duplicates() {
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 640, 480, &[30]),
            candidate(1, SampleKind::Yuy2, 640, 480, &[30]),
        ];
        let caps = capabilities_from_candidates(&candidates, None, None);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].modes, vec![VideoMode::new(640, 480, 30)]);
    }

    /// 実機の AVerMedia GC551 が返す対応形式（#391 で実測）
    fn gc551_candidates() -> Vec<StreamCandidate> {
        vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[60, 15]),
            candidate(1, SampleKind::Yuy2, 1280, 720, &[60, 15]),
            candidate(2, SampleKind::Yuy2, 720, 576, &[50, 25]),
            candidate(3, SampleKind::Yuy2, 720, 480, &[60, 30]),
            candidate(4, SampleKind::Yuy2, 640, 480, &[85, 15]),
        ]
    }

    #[test]
    fn target_resolution_keeps_a_listed_request() {
        // 利用者が選んだ解像度は、いまの解像度と違っても上書きしない
        assert_eq!(
            target_resolution(
                &gc551_candidates(),
                Some((1280, 720)),
                Some("YUY2"),
                Some((1920, 1080))
            ),
            Some((1280, 720))
        );
    }

    #[test]
    fn target_resolution_unlisted_request_uses_current() {
        // 2560x1440 はこのボードに無い。近いものへ寄せず、入力の 1920x1080 にする
        assert_eq!(
            target_resolution(
                &gc551_candidates(),
                Some((2560, 1440)),
                Some("YUY2"),
                Some((1920, 1080))
            ),
            Some((1920, 1080))
        );
    }

    #[test]
    fn target_resolution_without_request_uses_current() {
        assert_eq!(
            target_resolution(&gc551_candidates(), None, None, Some((1920, 1080))),
            Some((1920, 1080))
        );
    }

    #[test]
    fn target_resolution_without_current_keeps_request() {
        // いまの解像度を読めなければ、これまでどおり（近いもの / 1280x720）
        assert_eq!(
            target_resolution(&gc551_candidates(), Some((2560, 1440)), None, None),
            Some((2560, 1440))
        );
        assert_eq!(
            target_resolution(&gc551_candidates(), None, None, None),
            None
        );
    }

    #[test]
    fn target_resolution_ignores_unlisted_current() {
        // いまの解像度が一覧に無いなら使わない
        assert_eq!(
            target_resolution(&gc551_candidates(), None, None, Some((1024, 768))),
            None
        );
    }

    #[test]
    fn target_resolution_checks_within_the_requested_format() {
        // 1280x720 は MJPEG にしか無い。YUY2 を指定したら「一覧に無い」扱いで、
        // いまの解像度にする（`choose_candidate` も YUY2 の中から選ぶため）
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[30]),
            candidate(1, SampleKind::Mjpeg, 1280, 720, &[30]),
        ];
        assert_eq!(
            target_resolution(
                &candidates,
                Some((1280, 720)),
                Some("YUY2"),
                Some((1920, 1080))
            ),
            Some((1920, 1080))
        );
        // 形式が未指定なら、どの形式にあってもよい
        assert_eq!(
            target_resolution(&candidates, Some((1280, 720)), None, Some((1920, 1080))),
            Some((1280, 720))
        );
    }

    #[test]
    fn target_resolution_then_choose_candidate_opens_the_input_resolution() {
        // 解像度が未指定のとき、これまでは 1280x720 で開いて警告画面になっていた
        let candidates = gc551_candidates();
        let resolution = target_resolution(&candidates, None, None, Some((1920, 1080)));
        assert_eq!(
            choose_candidate(&candidates, resolution, None, None),
            Some((0, 60))
        );
    }

    #[test]
    fn capabilities_from_candidates_marks_the_current_resolution() {
        let candidates = vec![
            candidate(0, SampleKind::Yuy2, 1920, 1080, &[60]),
            candidate(1, SampleKind::Mjpeg, 1280, 720, &[60]),
        ];
        let caps = capabilities_from_candidates(&candidates, Some((1920, 1080)), None);
        assert_eq!(caps[0].name, "YUY2");
        assert_eq!(caps[0].current_resolution, Some((1920, 1080)));
        // MJPEG は 1920x1080 を開けないので添えない
        assert_eq!(caps[1].name, "MJPEG");
        assert_eq!(caps[1].current_resolution, None);
    }

    #[test]
    fn capabilities_from_candidates_empty_input_is_empty() {
        assert!(capabilities_from_candidates(&[], None, None).is_empty());
    }
}
