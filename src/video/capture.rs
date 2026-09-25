//! 映像デバイスの開閉と、フレームコールバック。
//!
//! `VideoCapture` は**デバイスワーカースレッド（`app::worker_loop`）だけが
//! 触る。** UI スレッドが読むフレームと色変換の設定は、生成時に渡された
//! `VideoFrames` / `SharedColorConversion` を通して共有する。

use log::{debug, info, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::CallbackCamera;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::color::SharedColorConversion;
use super::frame_buffer::VideoFrames;
use super::frame_sink::{FirstTimeOnly, FrameSink};
use super::{elapsed_ms, VideoError};
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

/// nokhwa のフレームフォーマットを、設定画面と同じ語彙の表示名へ変換する。
fn frame_format_name(format: FrameFormat) -> &'static str {
    match format {
        FrameFormat::YUYV => "YUY2",
        FrameFormat::MJPEG => "MJPEG",
        FrameFormat::NV12 => "NV12",
        FrameFormat::GRAY => "GRAY",
        FrameFormat::RAWRGB => "RGB24",
        FrameFormat::RAWBGR => "BGR24",
    }
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
    /// 確定した解像度。取得できなければ `None`
    pub resolution: Option<(u32, u32)>,
    /// 確定したフレームフォーマット名。取得できなければ `None`
    pub format: Option<String>,
    /// 要求したフレームレート
    pub requested_fps: u32,
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

    pub fn list_devices() -> Vec<(String, String)> {
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
                devices
            }
            Err(e) => {
                warn!(
                    "映像デバイスの列挙に失敗した（{:.1}ms）: {}",
                    elapsed_ms(start),
                    e
                );
                Vec::new()
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

        // 実際に要求したフレームレート。接続状態の表示に使う。
        // 解像度が未指定のときに要求する 60 を初期値にしてある
        let mut requested_fps = 60;

        // Windows Media Foundationでの問題を回避するフォーマット設定
        let requested_format = if let Some((w, h)) = resolution {
            // 設定画面では MJPEG / RGB24 も選べるが、実装が追いついておらず
            // すべて YUYV で開いている。選んだ値と実際の値が食い違うので記録する
            let ff = match format.unwrap_or("") {
                "YUY2" => FrameFormat::YUYV,
                // 未指定: デフォルトフォーマット
                "" => FrameFormat::YUYV,
                other @ ("MJPEG" | "RGB24") => {
                    warn!("ビデオフォーマット {} は未実装のため YUY2 で開く", other);
                    FrameFormat::YUYV
                }
                other => {
                    warn!(
                        "未知のビデオフォーマット {} を指定されたので YUY2 で開く",
                        other
                    );
                    FrameFormat::YUYV
                }
            };
            let fps_value = fps.unwrap_or(60).clamp(15, 120);
            if let Some(requested) = fps.filter(|v| *v != fps_value) {
                warn!(
                    "fps {} は対応範囲外なので {} に丸める",
                    requested, fps_value
                );
            }
            requested_fps = fps_value;

            // フォールバック戦略: 安定したYUYVを使用
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
                Resolution::new(w, h),
                ff,
                fps_value,
            )))
        } else {
            // 高解像度優先（安定性のためYUYVを使用）
            debug!("解像度が未指定なので 1280x720 YUYV 60fps を要求する");
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
                Resolution::new(1280, 720),
                FrameFormat::YUYV,
                60,
            )))
        };

        let frame_callback = {
            // 変換して積む本体はフェイクと共有する（`super::frame_sink`）。
            // ここに残すのは nokhwa の `Buffer` からの取り出しと、
            // YUY2 以外をデコーダへ倒す分岐だけ
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
                // フレームフォーマットを取得して適切な処理を行う
                let source_format = frame.source_frame_format();

                match source_format {
                    FrameFormat::YUYV if width.is_multiple_of(2) => {
                        // YUY2の高速パス
                        sink.push_yuy2(width, height, &frame.buffer_bytes(), start);
                    }

                    _ => {
                        // その他のフォーマットも標準デコード。
                        //
                        // **この経路では色空間・色レンジ・映像調整が効かない。**
                        // 係数表はデコーダの内部にあり、外から差し替えられないため。
                        // 変換後の RGB へフィルタを掛ければ反映はできるが、
                        // もともと重い経路に 1 画素あたりの処理を足すことになるので
                        // 採っていない。設定が効かないことをログに残す
                        if fallback_notice.take() {
                            warn!(
                                "YUY2 の高速パスを使えないのでデコーダへフォールバックする（フォーマット: {:?}、{}x{}）。この経路では色空間・色レンジ・映像調整が反映されない。以降は記録しない",
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
                                    frame_format_name(source_format),
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
        // 要求した fps は直前の debug! に残してある
        let actual_format = camera.camera_format().ok();
        let actual_resolution = actual_format
            .as_ref()
            .map(|f| (f.resolution().width_x, f.resolution().height_y));
        let actual_format_name = actual_format.as_ref().map(|f| format!("{:?}", f.format()));

        let open_start = Instant::now();
        camera
            .open_stream()
            .map_err(|e| VideoError::StreamOpenFailed {
                device: device_info.human_name().to_string(),
                source: e.to_string(),
            })?;
        let open_ms = elapsed_ms(open_start);

        info!(
            "映像ストリームを開いた（デバイス: {}、実際の設定: {}、Camera::new {:.1}ms、open_stream {:.1}ms）",
            device_info.human_name(),
            format_actual_video(actual_resolution, actual_format_name.as_deref()),
            create_ms,
            open_ms
        );

        self.camera = Some(camera);
        // 接続状態の表示用に、実際に開いた内容を控える
        self.active = Some(ActiveVideo {
            device_name: device_info.human_name().to_string(),
            resolution: actual_resolution,
            format: actual_format_name,
            requested_fps,
        });

        Ok(())
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
}
