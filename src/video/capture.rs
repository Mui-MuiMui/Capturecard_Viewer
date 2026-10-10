//! 映像デバイスの開閉と、フレームコールバック。
//!
//! `VideoCapture` は**デバイスワーカースレッド（`app::worker_loop`）だけが
//! 触る。** UI スレッドが読むフレームと色変換の設定は、生成時に渡された
//! `VideoFrames` / `SharedColorConversion` を通して共有する。

use log::{debug, info, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::CameraInfo;
use nokhwa::utils::{
    ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::CallbackCamera;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::color::SharedColorConversion;
use super::frame_buffer::VideoFrames;
use super::frame_sink::{FirstTimeOnly, FrameSink};
use super::mf_format::{self, SinkRoute};
use super::{elapsed_ms, VideoError};
use crate::audio::AudioPinState;
use crate::i18n::{self, Text};
use crate::repaint::RepaintWaker;

/// 実際に開いた解像度とフォーマットを 1 行にまとめる。
///
/// ログと設定ダイアログの「接続状態」タブで同じ文言を使う。
/// 片方しか取れていない場合も、取れているほうは出す。取れなかったことと
/// 「値が無い」ことを画面上で区別できるようにするため。
fn format_actual_video(resolution: Option<(u32, u32)>, format: Option<&str>) -> String {
    match (resolution, format) {
        (Some((width, height)), Some(format)) => format!("{}x{} {}", width, height, format),
        (Some((width, height)), None) => i18n::video_actual_format_unknown(width, height),
        (None, Some(format)) => i18n::video_actual_resolution_unknown(format),
        (None, None) => Text::VideoActualUnknown.get().to_string(),
    }
}

/// `VideoCapture::open_camera` が開いたカメラと、確定した内容。
struct OpenedCamera {
    camera: CallbackCamera,
    /// 確定した解像度。取得できなければ `None`
    resolution: Option<(u32, u32)>,
    /// 確定した形式の表示名（`mf_format::format_name`）。取得できなければ `None`
    format: Option<&'static str>,
    create_ms: f32,
    open_ms: f32,
}

/// 映像リンクの観測値。切断の判定に使う。
///
/// `FrameStats` と分けてあるのは、こちらが毎フレーム読まれるため。
/// 間隔の集計（最大 120 要素の走査）を伴わない 2 つの値だけを持たせている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoLinkState {
    /// 映像ストリームを開けているか。`start_capture` が成功した状態
    pub capturing: bool,
    /// 最後にフレームが届いてからの経過時間。1 枚も届いていなければ `None`
    pub since_last_frame: Option<Duration>,
    /// デバイスが消えたことを、デバイス側（DirectShow のグラフのイベント
    /// `EC_DEVICE_LOST` など）が知らせてきたか。**フレームの途絶を待たずに
    /// 切断と判断するための値で、知らせる手段を持たない経路（Media Foundation、
    /// フェイク）は常に `false`。** 一度立ったらストリームを閉じるまで下ろさない
    pub device_lost: bool,
}

/// 実際に開いた映像ストリームの内容。
///
/// 設定ダイアログの「接続状態」タブに出すために持つ。**設定に書かれた値では
/// なく、デバイスが確定させた値を入れる。** 設定画面では対応していない
/// 組み合わせも選べるため、要求した値と実際の値は食い違いうる。
///
/// `fps` だけは要求した値を持つ。nokhwa のバインディングが
/// `MF_MT_FRAME_RATE` の分母しか読んでおらず、実際の値を取れないため
/// （`start_capture` のコメントを参照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveVideo {
    /// 実際に開いたデバイス名
    pub device_name: String,
    /// 実際に開いた経路。設定の「映像の開き方」が自動のときも、どちらで
    /// 開いたかを「接続状態」タブで確かめられるように持つ（#237）
    pub api: CaptureApi,
    /// 確定した解像度。取得できなければ `None`
    pub resolution: Option<(u32, u32)>,
    /// 確定したフレームフォーマット名。取得できなければ `None`
    pub format: Option<String>,
    /// 設定の形式で開けず YUY2 で開いたときの、設定の形式名（#81）。
    ///
    /// Media Foundation の経路だけが埋める。デバイスがその形式を出さない、
    /// 経路が扱えない名前だった、のどちらか。「接続状態」タブに出す
    pub format_fallback: Option<String>,
    /// 要求したフレームレート
    pub requested_fps: u32,
    /// 映像デバイスの音声ピンの状態（#388）。DirectShow で開いたときだけ意味を持ち、
    /// それ以外は `NotApplicable`。ワーカーは音声を開くか待つかをこれで決める
    pub audio_pin: AudioPinState,
}

/// 映像ストリームを開いた経路。
///
/// 設定の `VideoBackendSetting` と違い「自動」を持たない。開いた結果なので
/// 必ずどれか 1 つに決まっている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureApi {
    MediaFoundation,
    DirectShow,
    /// 実機なしで動くフェイク（`video::fake`）
    Fake,
}

impl CaptureApi {
    /// 「接続状態」タブに出す名前
    pub fn label(self) -> &'static str {
        match self {
            CaptureApi::MediaFoundation => Text::VideoBackendMediaFoundation.get(),
            CaptureApi::DirectShow => Text::VideoBackendDirectShow.get(),
            CaptureApi::Fake => Text::CaptureApiFake.get(),
        }
    }
}

impl ActiveVideo {
    /// 解像度とフォーマットを 1 行で表す。ログに出しているものと同じ文言。
    pub fn summary(&self) -> String {
        format_actual_video(self.resolution, self.format.as_deref())
    }
}

pub struct VideoCapture {
    camera: Option<CallbackCamera>,
    frames: VideoFrames,
    /// いま開いているストリームの内容。閉じているときは `None`
    active: Option<ActiveVideo>,
    // フレームコールバックと共有する色変換の設定。
    // キャプチャを開き直さずに切り替えられるよう、開始時に固定せず共有する
    color_conversion: Arc<SharedColorConversion>,
    // フレームが届いたことを UI スレッドへ知らせる窓口。
    // `start_capture` のたびにフレームコールバックへ複製を渡す
    repaint_waker: RepaintWaker,
}

impl VideoCapture {
    /// フレームバッファ・色変換・再描画の窓口を受け取って作る。
    ///
    /// **どれも UI スレッドが先に作り、複製を持ち続ける。** `VideoCapture`
    /// 自身はデバイスワーカースレッド（`app::worker_loop`）だけが触るが、
    /// フレームと色変換は UI スレッドから直に読み書きするため。
    ///
    /// 再描画の窓口は**キャプチャを開くより前に渡す必要がある。**
    /// フレームコールバックは開始時点の複製を持つので、開いたあとに
    /// 差し替えてもそのストリームには届かない。既定の `RepaintWaker` は
    /// 何もしないため、渡し忘れても映像は止まらない。ただし `update()` の
    /// 保険の間隔（`repaint::ACTIVE_FALLBACK_INTERVAL`）でしか更新されず、
    /// 10fps 程度まで落ちる。
    pub fn new(
        frames: VideoFrames,
        color_conversion: Arc<SharedColorConversion>,
        repaint_waker: RepaintWaker,
    ) -> Self {
        Self {
            camera: None,
            frames,
            active: None,
            color_conversion,
            repaint_waker,
        }
    }

    /// 映像デバイスの一覧。`(名前, 説明)`。失敗しても空の一覧を返す。
    pub fn list_devices() -> Vec<(String, String)> {
        match Self::try_list_devices() {
            Ok(devices) => devices,
            Err(e) => {
                warn!("{}", e);
                Vec::new()
            }
        }
    }

    /// 映像デバイスの一覧。`list_devices` と違い、列挙に失敗した理由を返す。
    ///
    /// **ワーカーが列挙の結果をログへ残すため**（`app::worker_connect`）。
    /// 0 台と「列挙そのものが失敗した」を区別したい。
    pub fn try_list_devices() -> Result<Vec<(String, String)>, VideoError> {
        let start = Instant::now();
        match nokhwa::query(ApiBackend::MediaFoundation) {
            Ok(devices) => {
                let devices: Vec<(String, String)> = devices
                    .into_iter()
                    .map(|info| {
                        (
                            info.human_name().to_string(),
                            info.description().to_string(),
                        )
                    })
                    .collect();
                // 設定の device_name と実際に見えている名前を突き合わせられるよう、
                // 件数だけでなく名前もそのまま残す
                debug!(
                    "映像デバイスの一覧を取得した（{} 件、{:.1}ms）: {:?}",
                    devices.len(),
                    elapsed_ms(start),
                    devices.iter().map(|(name, _)| name).collect::<Vec<_>>()
                );
                Ok(devices)
            }
            Err(e) => {
                debug!("映像デバイスの列挙に失敗した（{:.1}ms）", elapsed_ms(start));
                Err(VideoError::DeviceQueryFailed(e.to_string()))
            }
        }
    }

    pub fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), VideoError> {
        self.stop_capture();

        debug!(
            "映像キャプチャの要求 - デバイス: {}、解像度: {}、フォーマット: {}、fps: {}",
            device_name.unwrap_or("（未指定。先頭のデバイス）"),
            resolution
                .map(|(w, h)| format!("{}x{}", w, h))
                .unwrap_or_else(|| "（未指定）".to_string()),
            format.unwrap_or("（未指定）"),
            fps.map(|v| v.to_string())
                .unwrap_or_else(|| "（未指定）".to_string()),
        );

        let query_start = Instant::now();
        let devices = nokhwa::query(ApiBackend::MediaFoundation)
            .map_err(|e| VideoError::DeviceQueryFailed(e.to_string()))?;
        debug!(
            "映像デバイスを {} 件列挙した（{:.1}ms）: {:?}",
            devices.len(),
            elapsed_ms(query_start),
            devices.iter().map(|d| d.human_name()).collect::<Vec<_>>()
        );

        let device_info = if let Some(name) = device_name {
            devices
                .into_iter()
                .find(|d| d.human_name() == name)
                .ok_or_else(|| VideoError::DeviceNotFound(name.to_string()))?
        } else {
            devices.into_iter().next().ok_or(VideoError::NoDevices)?
        };

        // 設定の形式を、能力の一覧と同じ表（`mf_format::MF_FORMATS`）で引いて
        // そのまま要求する。表に無い名前（DirectShow でだけ選べる I420 など）は
        // YUY2 で開き、そのことを「接続状態」タブにも出す（#81）
        let request = mf_format::request_for(format);
        if let Some(unknown) = request.unknown {
            warn!(
                "ビデオフォーマット {} は Media Foundation の経路で扱えないので YUY2 で開く",
                unknown
            );
        }

        let (target, fps_value) = if let Some((w, h)) = resolution {
            let fps_value = fps.unwrap_or(60).clamp(15, 120);
            if let Some(requested) = fps.filter(|v| *v != fps_value) {
                warn!(
                    "fps {} は対応範囲外なので {} に丸める",
                    requested, fps_value
                );
            }
            ((w, h), fps_value)
        } else {
            debug!(
                "解像度が未指定なので 1280x720 {} 60fps を要求する",
                mf_format::format_name(request.format)
            );
            ((1280, 720), 60)
        };

        // 要求した形式で開けなければ YUY2 で開き直す。YUY2 で失敗したときは
        // 形式ではなくデバイス側の問題なので、そのまま失敗を返す
        let mut format_fallback = request.unknown.map(str::to_string);
        let opened = match self.open_camera(&device_info, request.format, target, fps_value) {
            Ok(opened) => opened,
            Err(e) => match mf_format::fallback_for(request.format) {
                Some(fallback) => {
                    let requested_name = mf_format::format_name(request.format);
                    warn!(
                        "ビデオフォーマット {} で開けなかったので {} で開き直す（デバイス: {}）: {}",
                        requested_name,
                        mf_format::format_name(fallback),
                        device_info.human_name(),
                        e
                    );
                    format_fallback = Some(requested_name.to_string());
                    self.open_camera(&device_info, fallback, target, fps_value)?
                }
                None => return Err(e),
            },
        };

        info!(
            "映像ストリームを開いた（デバイス: {}、実際の設定: {}、Camera::new {:.1}ms、open_stream {:.1}ms）",
            device_info.human_name(),
            format_actual_video(opened.resolution, opened.format),
            opened.create_ms,
            opened.open_ms
        );

        self.camera = Some(opened.camera);
        // 接続状態の表示用に、実際に開いた内容を控える
        self.active = Some(ActiveVideo {
            device_name: device_info.human_name().to_string(),
            api: CaptureApi::MediaFoundation,
            resolution: opened.resolution,
            format: opened.format.map(str::to_string),
            format_fallback,
            requested_fps: fps_value,
            // Media Foundation で開いた映像には音声ピンが無い
            audio_pin: AudioPinState::NotApplicable,
        });

        Ok(())
    }

    /// 1 つの形式でカメラを作り、ストリームを開く。
    ///
    /// `start_capture` が、要求した形式と YUY2 へ代えたときの 2 回まで呼ぶ。
    /// フレームコールバック（`FrameSink`）は呼ぶたびに作る。
    fn open_camera(
        &self,
        device_info: &CameraInfo,
        format: FrameFormat,
        (width, height): (u32, u32),
        fps: u32,
    ) -> Result<OpenedCamera, VideoError> {
        let requested_format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(width, height), format, fps),
        ));

        let frame_callback = {
            // 変換して積む本体はフェイク・DirectShow と共有する（`super::frame_sink`）。
            // ここに残すのは nokhwa の `Buffer` からの取り出しと、形式ごとの受け口の
            // 振り分け（`mf_format::sink_route`）だけ
            let mut sink = FrameSink::new(
                &self.frames,
                self.color_conversion.clone(),
                self.repaint_waker.clone(),
            );
            let mut fallback_notice = FirstTimeOnly::default();
            let mut decode_error_notice = FirstTimeOnly::default();
            move |frame: nokhwa::Buffer| {
                // 変換時間の計測と、フレーム間隔の基準を兼ねる受信時刻
                let start = Instant::now();
                let res = frame.resolution();
                let width = res.width_x as usize;
                let height = res.height_y as usize;
                let source_format = frame.source_frame_format();
                let src = frame.buffer();

                match mf_format::sink_route(source_format, width) {
                    SinkRoute::Yuy2 => {
                        sink.push_yuy2(width, height, src, start);
                    }
                    SinkRoute::Yuv420(layout) => {
                        sink.push_yuv420(layout, width, height, src, start);
                    }
                    SinkRoute::Mjpeg => {
                        // DirectShow の経路と同じ展開（`convert::mjpeg_to_rgb`）
                        sink.push_mjpeg(width, height, src, start);
                    }
                    SinkRoute::Bgr24 => {
                        sink.push_bgr24(width, height, mf_format::MF_RGB24_BOTTOM_UP, src, start);
                    }
                    SinkRoute::Decoder => {
                        // 受け口を持たない形式（幅が奇数の YUY2 など）は nokhwa のデコーダへ。
                        //
                        // **この経路では色空間・色レンジ・映像調整が効かない。**
                        // 係数表はデコーダの内部にあり、外から差し替えられないため。
                        // 変換後の RGB へフィルタを掛ければ反映はできるが、
                        // もともと重い経路に 1 画素あたりの処理を足すことになるので
                        // 採っていない。設定が効かないことをログに残す
                        if fallback_notice.take() {
                            warn!(
                                "このフォーマットの受け口が無いのでデコーダへフォールバックする（フォーマット: {:?}、{}x{}）。この経路では色空間・色レンジ・映像調整が反映されない。以降は記録しない",
                                source_format, width, height
                            );
                        }
                        match frame.decode_image::<RgbFormat>() {
                            Ok(rgb_data) => {
                                sink.push_decoded(
                                    width,
                                    height,
                                    rgb_data.into_raw(),
                                    start,
                                    mf_format::format_name(source_format),
                                );
                            }
                            Err(e) => {
                                if decode_error_notice.take() {
                                    warn!(
                                        "フレームのデコードに失敗した（フォーマット: {:?}、{}x{}）: {}。以降は記録しない",
                                        source_format, width, height, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
        };

        let create_start = Instant::now();
        let mut camera = CallbackCamera::new(
            device_info.index().clone(),
            requested_format,
            frame_callback,
        )
        .map_err(|e| VideoError::CameraOpenFailed {
            device: device_info.human_name().to_string(),
            source: e.to_string(),
        })?;
        let create_ms = elapsed_ms(create_start);

        // 実際に確定したフォーマットは open_stream の前に読む。
        // ストリーム開始後は nokhwa のフレーム取得スレッドがカメラのロックを
        // 握り続けるため、待たされてデバイスワーカースレッドが止まる
        //
        // fps は載せない。nokhwa のバインディングが MF_MT_FRAME_RATE
        // （上位 32 ビットが分子、下位 32 ビットが分母）を `fps as u32` で
        // 読んでおり、分母しか取れていない。整数フレームレートでは常に 1 になる
        // （nokhwa-bindings-windows 0.4.6 の `format_refreshed`）。
        // 要求した fps は `start_capture` の debug! に残してある
        let actual_format = camera.camera_format().ok();
        let resolution = actual_format
            .as_ref()
            .map(|f| (f.resolution().width_x, f.resolution().height_y));
        let format = actual_format
            .as_ref()
            .map(|f| mf_format::format_name(f.format()));

        let open_start = Instant::now();
        camera
            .open_stream()
            .map_err(|e| VideoError::StreamOpenFailed {
                device: device_info.human_name().to_string(),
                source: e.to_string(),
            })?;
        let open_ms = elapsed_ms(open_start);

        Ok(OpenedCamera {
            camera,
            resolution,
            format,
            create_ms,
            open_ms,
        })
    }

    /// いま開いているストリームの内容。開いていなければ `None`。
    ///
    /// 設定ダイアログを開いている間だけ呼ばれる。小さな構造体の複製だけで、
    /// フレームバッファのロックも取らない。
    pub fn active(&self) -> Option<ActiveVideo> {
        self.active.clone()
    }

    pub fn stop_capture(&mut self) {
        self.active = None;
        if let Some(mut camera) = self.camera.take() {
            let stop_start = Instant::now();
            match camera.stop_stream() {
                Ok(()) => info!("映像ストリームを閉じた（{:.1}ms）", elapsed_ms(stop_start)),
                Err(e) => warn!(
                    "映像ストリームを閉じられなかった（{:.1}ms）: {}",
                    elapsed_ms(stop_start),
                    e
                ),
            }
        }

        self.frames.reset();
    }

    /// ストリームが開いているかと、フレームの途絶時間を返す。
    ///
    /// 切断の監視のためにデバイスワーカーが定期的に呼ぶ。ロックの中で行うのは
    /// `Instant` の減算だけで、フレームコールバックをほとんど待たせない。
    pub fn link_state(&self) -> VideoLinkState {
        VideoLinkState {
            capturing: self.camera.is_some(),
            since_last_frame: self.frames.since_last_frame(),
            // nokhwa は Media Foundation のデバイス喪失を知らせてこない。
            // 途絶の検出（`VIDEO_SIGNAL_TIMEOUT`）に任せる
            device_lost: false,
        }
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_actual_video_with_both_values_joins_them() {
        assert_eq!(
            format_actual_video(Some((1920, 1080)), Some("YUYV")),
            "1920x1080 YUYV"
        );
    }

    #[test]
    fn format_actual_video_without_format_keeps_the_resolution() {
        assert_eq!(
            format_actual_video(Some((640, 480)), None),
            "640x480 （フォーマット不明）"
        );
    }

    #[test]
    fn format_actual_video_without_resolution_keeps_the_format() {
        assert_eq!(
            format_actual_video(None, Some("MJPEG")),
            "（解像度不明） MJPEG"
        );
    }

    #[test]
    fn format_actual_video_without_any_value_says_so() {
        // 取得できないことと「値が無い」ことを画面上で区別する
        assert_eq!(format_actual_video(None, None), "（取得できない）");
    }

    /// `media_foundation_capture_timestamp_to_callback_lag` が 1 サンプルごとに控える値
    struct LagSample {
        /// コールバックの入口の UNIX 時刻
        unix: Duration,
        /// nokhwa の `Buffer::capture_timestamp`（ストリームを開いたときの UNIX 時刻 + サンプル時刻）
        capture: Option<Duration>,
        /// コールバックの入口の `MFGetSystemTime`（100ns、QPC 基準）
        system_100ns: i64,
    }

    fn unix_now() -> Duration {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("時計が 1970 年より後")
    }

    /// 2 つの時刻の差（a − b、ms）。負にもなる
    fn diff_ms(a: Duration, b: Duration) -> f64 {
        (a.as_nanos() as i128 - b.as_nanos() as i128) as f64 / 1e6
    }

    #[test]
    #[ignore = "Media Foundation のキャプチャーボード（CAPTURECARD_VIEWER_PIN_TEST_DEVICE、既定 GC551）に 1920x1080 60Hz の入力信号を入れておく"]
    fn media_foundation_capture_timestamp_to_callback_lag() {
        // 実行: cargo test media_foundation_capture_timestamp_to_callback_lag -- --ignored --nocapture
        // #476。1920x1080 60fps YUY2 で開き、2 秒待ってから 10 秒のあいだ、コールバックの
        // 入口の UNIX 時刻と nokhwa の `capture_timestamp` の差を測る。2 回開き直して
        // 再現性を見る。コールバックではアプリと同じ仕事（`FrameSink::push_yuy2`）もする。
        //
        // `capture_timestamp` は nokhwa-bindings-windows 0.4.6 が `start_stream` の時点の
        // UNIX 時刻（`stream_epoch`）に `ReadSample` のサンプル時刻を足したもの。
        // サンプル時刻の原点がストリームを開いた時刻でなければ、差にはその分のずれが乗る。
        // そこで `open_stream` の前後の UNIX 時刻で `stream_epoch` を挟み、サンプル時刻を
        // 推定して `MFGetSystemTime`（QPC 基準）と比べた値も出す。DirectShow の経路は
        // `video::directshow::timestamp_probe` の `directshow_sample_time_to_receive_lag`
        use crate::video::directshow::timestamp_probe::{
            print_lag_summary, report_lag, test_device,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        use windows::Win32::Media::MediaFoundation::MFGetSystemTime;

        let device = test_device();
        let info = nokhwa::query(ApiBackend::MediaFoundation)
            .expect("列挙できる")
            .into_iter()
            .find(|info| info.human_name().contains(&device))
            .unwrap_or_else(|| panic!("Media Foundation のデバイス {device} がある"));
        let mut runs = Vec::new();
        for run in 1..=2 {
            let rows: Arc<Mutex<Vec<LagSample>>> = Arc::new(Mutex::new(Vec::with_capacity(4096)));
            let recording = Arc::new(AtomicBool::new(false));
            let frames = VideoFrames::new();
            let callback = {
                let rows = rows.clone();
                let recording = recording.clone();
                let mut sink = FrameSink::new(
                    &frames,
                    Arc::new(SharedColorConversion::new()),
                    RepaintWaker::default(),
                );
                move |frame: nokhwa::Buffer| {
                    let start = Instant::now();
                    let unix = unix_now();
                    let system_100ns = unsafe { MFGetSystemTime() };
                    if recording.load(Ordering::Acquire) {
                        if let Ok(mut rows) = rows.lock() {
                            rows.push(LagSample {
                                unix,
                                capture: frame.capture_timestamp(),
                                system_100ns,
                            });
                        }
                    }
                    let res = frame.resolution();
                    let (width, height) = (res.width_x as usize, res.height_y as usize);
                    sink.push_yuy2(width, height, frame.buffer(), start);
                }
            };
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
                CameraFormat::new(Resolution::new(1920, 1080), FrameFormat::YUYV, 60),
            ));
            let mut camera =
                CallbackCamera::new(info.index().clone(), requested, callback).expect("開ける");
            let format = camera.camera_format().expect("形式を読める");
            println!("{run} 回目: {format:?}");
            assert_eq!(format.format(), FrameFormat::YUYV, "YUY2 で開ける");
            let before = unix_now();
            camera.open_stream().expect("ストリームを開ける");
            let after = unix_now();
            std::thread::sleep(Duration::from_secs(2));
            recording.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_secs(10));
            recording.store(false, Ordering::Release);
            camera.stop_stream().expect("止められる");
            let rows = std::mem::take(&mut *rows.lock().expect("ロックできる"));

            let label = format!("Media Foundation {run} 回目");
            let stamped: Vec<(&LagSample, Duration)> = rows
                .iter()
                .filter_map(|row| row.capture.map(|capture| (row, capture)))
                .collect();
            println!(
                "== {label}: サンプル {} 個（capture_timestamp なし {} 個）、stream_epoch を挟む幅 {:.3}ms",
                rows.len(),
                rows.len() - stamped.len(),
                diff_ms(after, before)
            );
            let intervals: Vec<f64> = rows
                .windows(2)
                .map(|w| diff_ms(w[1].unix, w[0].unix))
                .collect();
            let longest = intervals.iter().copied().fold(0.0, f64::max);
            println!(
                "コールバックの間隔 ms: 平均 {:.3} 最大 {longest:.3}",
                intervals.iter().sum::<f64>() / intervals.len().max(1) as f64
            );
            if let Some((first, capture)) = stamped.first() {
                // サンプル時刻（推定）。stream_epoch は before と after の間にある
                let sample_ms = diff_ms(*capture, before);
                println!(
                    "最初のサンプル: サンプル時刻（推定）{sample_ms:.3}ms〜{:.3}ms、そのときの MFGetSystemTime {:.3}ms",
                    diff_ms(*capture, after),
                    first.system_100ns as f64 / 1e4
                );
            }
            // サンプル時刻が QPC 基準（MFGetSystemTime と同じ原点）なら、こちらが遅れそのもの
            let system_lags: Vec<f64> = stamped
                .iter()
                .map(|(row, capture)| row.system_100ns as f64 / 1e4 - diff_ms(*capture, before))
                .collect();
            report_lag(
                &format!("{label}（MFGetSystemTime − 推定のサンプル時刻。stream_epoch を before とした場合）"),
                &system_lags,
            );
            let lags: Vec<f64> = stamped
                .iter()
                .map(|(row, capture)| diff_ms(row.unix, *capture))
                .collect();
            runs.push(report_lag(
                &format!("{label}（UNIX 時刻 − capture_timestamp）"),
                &lags,
            ));
        }
        print_lag_summary("Media Foundation", &runs);
        assert!(
            runs.iter().all(Option::is_some),
            "capture_timestamp が付かない"
        );
    }
}
