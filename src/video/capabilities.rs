//! デバイスが開ける解像度・フレームレートの組み合わせと、その問い合わせ。
//!
//! 設定ダイアログの「対応形式」に出すために、デバイスを一時的に開いて
//! 聞き出す。**キャプチャ中のデバイスをもう一度開くので、デバイス
//! ワーカースレッドから呼ぶ。**

use log::{debug, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use std::time::Instant;

use super::{elapsed_ms, VideoCapture, VideoError};

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
/// フォーマット名は "YUY2" / "MJPEG" / "RGB24"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatCapability {
    /// フォーマット名
    pub name: String,
    /// そのフォーマットで開ける映像モードの一覧
    pub modes: Vec<VideoMode>,
}

impl FormatCapability {
    pub fn new(name: impl Into<String>, modes: Vec<VideoMode>) -> Self {
        Self {
            name: name.into(),
            modes,
        }
    }
}

/// デバイスが対応する全フォーマットの能力一覧。
pub type DeviceCapabilities = Vec<FormatCapability>;

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

        // カメラを一時的に開いて能力を取得
        let requested_format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(640, 480), FrameFormat::YUYV, 30),
        ));

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

        let mut result: DeviceCapabilities = Vec::new();

        // 各フォーマットで対応解像度・FPSを取得
        let formats = vec![
            ("YUY2", FrameFormat::YUYV),
            ("MJPEG", FrameFormat::MJPEG),
            ("RGB24", FrameFormat::RAWRGB),
        ];

        for (format_name, frame_format) in formats {
            match camera.compatible_list_by_resolution(frame_format) {
                Ok(resolution_map) => {
                    let mut modes: Vec<VideoMode> = Vec::new();

                    for (resolution, fps_list) in resolution_map.iter() {
                        // 各解像度に対して利用可能な全FPSを記録
                        for fps in fps_list.iter() {
                            modes.push(VideoMode::new(
                                resolution.width_x,
                                resolution.height_y,
                                *fps,
                            ));
                        }
                    }

                    // 重複を削除してユニークな組み合わせのみ保持。
                    // dedup は隣接する重複しか落とさないので、先に並べておく
                    modes.sort_by_key(|mode| (mode.width, mode.height, mode.fps));
                    modes.dedup();

                    // 解像度でソート（大きい順）、同じ解像度ならFPSでソート（大きい順）
                    modes.sort_by(|a, b| match b.pixel_count().cmp(&a.pixel_count()) {
                        std::cmp::Ordering::Equal => b.fps.cmp(&a.fps),
                        other => other,
                    });

                    debug!(
                        "{} の対応する組み合わせを {} 件取得した",
                        format_name,
                        modes.len()
                    );
                    if !modes.is_empty() {
                        result.push(FormatCapability::new(format_name, modes));
                    }
                }
                Err(e) => {
                    // エラーの場合、デフォルト値を設定。
                    // 画面に出る選択肢がデバイスの実際の能力ではなくなるので残す
                    warn!(
                        "{} の対応する組み合わせを取得できないので既定値を使う: {}",
                        format_name, e
                    );
                    let default_modes = match format_name {
                        "YUY2" => vec![VideoMode::new(1280, 720, 60), VideoMode::new(640, 480, 30)],
                        "MJPEG" => vec![
                            VideoMode::new(1920, 1080, 30),
                            VideoMode::new(1280, 720, 60),
                            VideoMode::new(640, 480, 30),
                        ],
                        "RGB24" => {
                            vec![VideoMode::new(1280, 720, 30), VideoMode::new(640, 480, 30)]
                        }
                        _ => vec![],
                    };
                    if !default_modes.is_empty() {
                        result.push(FormatCapability::new(format_name, default_modes));
                    }
                }
            }
        }

        // 結果が空の場合はデフォルト値を返す
        if result.is_empty() {
            warn!("どのフォーマットの能力も取得できなかったので既定値を返す");
            result = vec![
                FormatCapability::new(
                    "YUY2",
                    vec![VideoMode::new(1280, 720, 60), VideoMode::new(640, 480, 30)],
                ),
                FormatCapability::new(
                    "MJPEG",
                    vec![
                        VideoMode::new(1920, 1080, 30),
                        VideoMode::new(1280, 720, 60),
                        VideoMode::new(640, 480, 30),
                    ],
                ),
            ];
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
