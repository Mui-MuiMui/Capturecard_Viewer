//! デバイスが開ける解像度・フレームレートの組み合わせと、その問い合わせ。
//!
//! 設定ダイアログの「対応形式」に出すために、デバイスを一時的に開いて
//! 聞き出す。**キャプチャ中のデバイスをもう一度開くので、デバイス
//! ワーカースレッドから呼ぶ。**

use log::{debug, warn};
use nokhwa::utils::ApiBackend;
use std::time::Instant;

use super::{elapsed_ms, mf_format, VideoCapture, VideoError};

/// デバイスを開ける映像モード 1 件。解像度とフレームレートの組み合わせ。
///
/// 以前は `(u32, u32, u32)` のタプルだったが、どの要素が幅・高さ・fps なのかが
/// 型からは分からず、並べ替えや比較のたびに `.0` / `.1` / `.2` を読み解く必要が
/// あった。名前付きにして取り違えをコンパイル時に防ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoMode {
    /// 幅（ピクセル）
    pub width: u32,
    /// 高さ（ピクセル）
    pub height: u32,
    /// フレームレート（fps）
    pub fps: u32,
}

impl VideoMode {
    pub const fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps }
    }

    /// 画素数。解像度の大小を比べるのに使う。
    /// `u32` 同士の積が溢れないよう `u64` で返す
    pub fn pixel_count(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// 解像度だけを取り出す。設定の `video.resolution` が `(幅, 高さ)` のため
    pub fn resolution(self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// 1 つのビデオフォーマットが対応する能力。
/// フォーマット名は "YUY2" / "NV12" / "MJPEG" / "RGB24"（Media Foundation は `mf_format::MF_FORMATS`、DirectShow は I420 / YV12 も）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatCapability {
    /// フォーマット名
    pub name: String,
    /// そのフォーマットで開ける映像モードの一覧
    pub modes: Vec<VideoMode>,
    /// デバイスがいま出している解像度。`modes` の中にあるものだけ入る。
    ///
    /// DirectShow の経路だけが `IAMStreamConfig::GetFormat` から埋める（入力信号の
    /// 解像度を映すドライバーがある、#391）。Media Foundation とフェイクは `None`
    pub current_resolution: Option<(u32, u32)>,
    /// デバイスから取れた一覧ではなく、既定の組み合わせで埋めたものか。
    ///
    /// Media Foundation の経路で、どの形式の一覧も取れなかったときだけ真になる
    /// （`assemble_capabilities`、#446）。設定画面はこれを見て「既定の一覧」の
    /// 注意書きを出す
    pub assumed: bool,
}

impl FormatCapability {
    pub fn new(name: impl Into<String>, modes: Vec<VideoMode>) -> Self {
        Self {
            name: name.into(),
            modes,
            current_resolution: None,
            assumed: false,
        }
    }

    /// 取れなかったので既定の組み合わせで埋めた能力（`assumed` が真）
    fn assumed(name: impl Into<String>, modes: Vec<VideoMode>) -> Self {
        Self {
            assumed: true,
            ..Self::new(name, modes)
        }
    }

    /// いまの解像度を添える。`modes` に無い解像度は捨てる
    pub fn with_current_resolution(mut self, current: Option<(u32, u32)>) -> Self {
        self.current_resolution = current.filter(|&resolution| {
            self.modes
                .iter()
                .any(|mode| mode.resolution() == resolution)
        });
        self
    }
}

/// デバイスが対応する全フォーマットの能力一覧。
pub type DeviceCapabilities = Vec<FormatCapability>;

/// 一覧が既定の組み合わせ（`FormatCapability::assumed`）か。設定画面の注意書きに使う
pub fn is_assumed(capabilities: &DeviceCapabilities) -> bool {
    capabilities.iter().any(|capability| capability.assumed)
}

/// 1 つの形式で取れた組み合わせを、重複を除いて解像度の大きい順・同じ解像度なら
/// fps の高い順に並べる
fn normalize_modes(mut modes: Vec<VideoMode>) -> Vec<VideoMode> {
    // dedup は隣接する重複しか落とさないので、先に並べておく
    modes.sort_by_key(|mode| (mode.width, mode.height, mode.fps));
    modes.dedup();
    modes.sort_by(|a, b| match b.pixel_count().cmp(&a.pixel_count()) {
        std::cmp::Ordering::Equal => b.fps.cmp(&a.fps),
        other => other,
    });
    modes
}

/// 形式ごとに取れた組み合わせから、設定画面に出す一覧を作る（#446）。
///
/// **取れた形式だけを並べる。** 一覧が取れなかった形式（呼び出し側が `Err` を
/// 積まない）も、0 件だった形式も外す。以前は取れなかった形式を決め打ちの
/// 組み合わせで埋めていたので、デバイスが出していない形式が選択肢に出ていた。
/// #442 で形式を指定せずに仮に開けるようになり、取れた分だけで一覧が実態と合う。
///
/// どの形式も取れなかったときだけ、既定の組み合わせ（YUY2 / MJPEG）を
/// `assumed` を立てて返す。選択肢が空だと設定画面で何も選べないため。
/// 並びは `listed` の順（`MF_FORMATS` の順）のまま。
fn assemble_capabilities(listed: Vec<(&str, Vec<VideoMode>)>) -> DeviceCapabilities {
    let result: DeviceCapabilities = listed
        .into_iter()
        .filter(|(_, modes)| !modes.is_empty())
        .map(|(name, modes)| FormatCapability::new(name, normalize_modes(modes)))
        .collect();
    if result.is_empty() {
        return assumed_capabilities();
    }
    result
}

/// どの形式の一覧も取れなかったときに出す既定の組み合わせ
fn assumed_capabilities() -> DeviceCapabilities {
    vec![
        FormatCapability::assumed(
            "YUY2",
            vec![VideoMode::new(1280, 720, 60), VideoMode::new(640, 480, 30)],
        ),
        FormatCapability::assumed(
            "MJPEG",
            vec![
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1280, 720, 60),
                VideoMode::new(640, 480, 30),
            ],
        ),
    ]
}

// 能力の問い合わせは `VideoCapture` の関連関数として生えている。デバイスを
// 開く操作なので窓口を分けたくないが、置き場所は扱う型に合わせてここにする
impl VideoCapture {
    // デバイスの能力を取得するメソッド
    pub fn get_device_capabilities(
        device_name: Option<&str>,
    ) -> Result<DeviceCapabilities, VideoError> {
        use nokhwa::Camera;

        let start = Instant::now();
        debug!(
            "デバイス能力の取得を開始する: {}",
            device_name.unwrap_or("（未指定。先頭のデバイス）")
        );

        // 失敗をここで warn! にしない。呼び出し側（`app::worker_connect` の
        // query_video_capabilities）が、デバイス名付きで理由をログへ出し、
        // 結果はイベント経由で設定ダイアログにも表示される。ここで出すと
        // 同じ内容が 2 行並ぶ
        let devices = nokhwa::query(ApiBackend::MediaFoundation)
            .map_err(|e| VideoError::DeviceQueryFailed(e.to_string()))?;

        let device_info = if let Some(name) = device_name {
            devices
                .into_iter()
                .find(|d| d.human_name() == name)
                .ok_or_else(|| VideoError::DeviceNotFound(name.to_string()))?
        } else {
            devices.into_iter().next().ok_or(VideoError::NoDevices)?
        };

        // カメラを一時的に開いて能力を取得。nokhwa は開くときに必ず 1 つの形式を選ぶので、
        // 特定の形式に寄らず「一覧に出す形式のどれか」で開く（#442、`mf_format::capabilities_probe`）
        let requested_format = mf_format::capabilities_probe();

        // キャプチャ中のデバイスをもう一度開く。取得そのものはデバイス
        // ワーカースレッドで走るので UI は止まらないが、ここが伸びると設定
        // ダイアログの「対応形式を取得中...」が長く出たままになり、その間
        // ワーカーは次のコマンドを処理できない
        let open_start = Instant::now();
        let mut camera =
            Camera::new(device_info.index().clone(), requested_format).map_err(|e| {
                VideoError::CameraOpenFailed {
                    device: device_info.human_name().to_string(),
                    source: e.to_string(),
                }
            })?;
        debug!(
            "能力取得のためにデバイスを開いた（{:.1}ms）",
            elapsed_ms(open_start)
        );

        // 各フォーマットで対応解像度・FPSを取得。開くとき（`capture.rs`）と同じ表を引き、
        // 一覧に出る形式と開ける形式を揃える（#81）。以前は RGB24 を RAWRGB で引いて
        // いたので、Media Foundation の RGB24（nokhwa では RAWBGR）が一覧に出なかった
        let mut listed: Vec<(&str, Vec<VideoMode>)> = Vec::new();
        for (format_name, frame_format) in mf_format::MF_FORMATS {
            match camera.compatible_list_by_resolution(frame_format) {
                Ok(resolution_map) => {
                    let modes: Vec<VideoMode> = resolution_map
                        .iter()
                        .flat_map(|(resolution, fps_list)| {
                            fps_list.iter().map(|fps| {
                                VideoMode::new(resolution.width_x, resolution.height_y, *fps)
                            })
                        })
                        .collect();
                    debug!(
                        "{} の対応する組み合わせを {} 件取得した",
                        format_name,
                        modes.len()
                    );
                    listed.push((format_name, modes));
                }
                Err(e) => {
                    // 一覧から外す（#446）。どの形式も取れなかったときは既定の一覧に
                    // なるので、理由を追えるよう残す
                    warn!(
                        "{} の対応する組み合わせを取得できないので一覧から外す: {}",
                        format_name, e
                    );
                }
            }
        }

        let result = assemble_capabilities(listed);
        if is_assumed(&result) {
            warn!("どのフォーマットの能力も取得できなかったので既定の一覧を返す");
        }

        // 総所要時間とフォーマット数は呼び出し側が info! で出すので、
        // ここではフォーマットごとの内訳だけを debug! に残す
        debug!(
            "デバイス能力の内訳（{}、{:.1}ms）: {}",
            device_info.human_name(),
            elapsed_ms(start),
            result
                .iter()
                .map(|capability| {
                    format!("{}: {} 件", capability.name, capability.modes.len())
                })
                .collect::<Vec<_>>()
                .join("、")
        );

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(capabilities: &DeviceCapabilities) -> Vec<&str> {
        capabilities
            .iter()
            .map(|capability| capability.name.as_str())
            .collect()
    }

    #[test]
    fn assemble_lists_only_the_formats_that_were_obtained() {
        // NV12 と RGB24 は取れなかった（`Err` は呼び出し側で積まない）。
        // MJPEG は 0 件。YUY2 だけが残り、既定の組み合わせで埋めない
        let listed = vec![
            ("YUY2", vec![VideoMode::new(1920, 1080, 60)]),
            ("MJPEG", Vec::new()),
        ];

        let result = assemble_capabilities(listed);

        assert_eq!(names(&result), vec!["YUY2"]);
        assert_eq!(result[0].modes, vec![VideoMode::new(1920, 1080, 60)]);
        assert!(!is_assumed(&result));
    }

    #[test]
    fn assemble_keeps_the_table_order() {
        let listed = vec![
            ("YUY2", vec![VideoMode::new(1280, 720, 60)]),
            ("NV12", vec![VideoMode::new(1280, 720, 60)]),
            ("RGB24", vec![VideoMode::new(640, 480, 30)]),
        ];

        assert_eq!(
            names(&assemble_capabilities(listed)),
            vec!["YUY2", "NV12", "RGB24"]
        );
    }

    #[test]
    fn assemble_falls_back_to_assumed_defaults_only_when_nothing_was_obtained() {
        for listed in [
            Vec::new(),
            vec![("YUY2", Vec::new()), ("MJPEG", Vec::new())],
        ] {
            let result = assemble_capabilities(listed);

            assert_eq!(names(&result), vec!["YUY2", "MJPEG"]);
            assert!(result.iter().all(|capability| capability.assumed));
            assert!(is_assumed(&result));
        }
    }

    #[test]
    fn assemble_sorts_and_dedups_modes() {
        let listed = vec![(
            "YUY2",
            vec![
                VideoMode::new(640, 480, 30),
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(640, 480, 30),
            ],
        )];

        assert_eq!(
            assemble_capabilities(listed)[0].modes,
            vec![
                VideoMode::new(1920, 1080, 60),
                VideoMode::new(1920, 1080, 30),
                VideoMode::new(640, 480, 30),
            ]
        );
    }

    #[test]
    fn capabilities_from_the_device_are_not_assumed() {
        let capabilities = vec![FormatCapability::new(
            "YUY2",
            vec![VideoMode::new(1280, 720, 60)],
        )];

        assert!(!is_assumed(&capabilities));
        assert!(!is_assumed(&Vec::new()));
    }
}
