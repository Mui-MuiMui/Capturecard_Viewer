use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::CallbackCamera;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 1 つのビデオフォーマットが対応する能力。
/// `(フォーマット名, [(幅, 高さ, fps)])` の組で、フォーマット名は "YUY2" / "MJPEG" / "RGB24"。
pub type FormatCapability = (String, Vec<(u32, u32, u32)>);

/// デバイスが対応する全フォーマットの能力一覧。
pub type DeviceCapabilities = Vec<FormatCapability>;

/// YUY2 -> RGB24 の高速変換 (最適化版)。
///
/// 変換結果は `out` へ書き込む。`out` は呼び出し側が使い回す前提で、
/// 毎フレームの確保・ゼロクリア・解放を避けるために `&mut Vec<u8>` で受け取る。
///
/// `width * height * 3` バイトへリサイズしたうえで全域を書き切る。
/// 変換できなかった領域（幅が奇数で余る 1 画素、入力が足りない画素）は
/// 使い回した Vec に残る前フレームの画素が見えないよう 0 で埋める。
fn yuy2_to_rgb_naive(width: usize, height: usize, src: &[u8], out: &mut Vec<u8>) {
    // 既に確保済みの容量はそのまま使う。0 埋めが走るのは伸ばした分だけ
    out.resize(width * height * 3, 0);

    // 安全確保: 偶数幅前提 (YUYV ペア)
    // 4 バイト / 6 バイトに満たない端数は変換しない
    let converted_len = {
        let (src_chunks, _) = src.as_chunks::<4>();
        let (out_chunks, _) = out.as_chunks_mut::<6>();
        let pair_count = src_chunks.len().min(out_chunks.len());

        for (src_chunk, out_chunk) in src_chunks.iter().zip(out_chunks.iter_mut()) {
            let y0 = src_chunk[0] as i32;
            let u = src_chunk[1] as i32;
            let y1 = src_chunk[2] as i32;
            let v = src_chunk[3] as i32;

            // BT.601 変換 (整数演算で高速化)
            let c0 = y0 - 16;
            let c1 = y1 - 16;
            let d = u - 128;
            let e = v - 128;

            // 係数を1024倍して整数演算に変換 (1.164 ≈ 1192/1024)
            let r0 = (1192 * c0 + 1634 * e) >> 10;
            let g0 = (1192 * c0 - 401 * d - 833 * e) >> 10;
            let b0 = (1192 * c0 + 2066 * d) >> 10;
            let r1 = (1192 * c1 + 1634 * e) >> 10;
            let g1 = (1192 * c1 - 401 * d - 833 * e) >> 10;
            let b1 = (1192 * c1 + 2066 * d) >> 10;

            out_chunk[0] = r0.clamp(0, 255) as u8;
            out_chunk[1] = g0.clamp(0, 255) as u8;
            out_chunk[2] = b0.clamp(0, 255) as u8;
            out_chunk[3] = r1.clamp(0, 255) as u8;
            out_chunk[4] = g1.clamp(0, 255) as u8;
            out_chunk[5] = b1.clamp(0, 255) as u8;
        }

        pair_count * 6
    };

    // 変換しなかった領域は 0 で埋める。使い回した Vec では
    // 前フレームの画素が残っているため、埋めないと画面に出てしまう
    out[converted_len..].fill(0);
}

pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// フレームコールバックスレッドと UI スレッドの間でフレームを受け渡す。
///
/// 画素データは `Arc` で共有するため、取り出しても複製は発生しない。
/// `generation` は push のたびに進み、取り出し側が新着の有無を判別するために使う。
struct FrameBuffer {
    latest: Option<Arc<VideoFrame>>,
    generation: u64,
    last_frame_instant: Option<Instant>,
    frame_intervals: VecDeque<f32>, // ミリ秒
    last_decode_ms: f32,
    fast_count: u64,
    fallback_count: u64,
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            latest: None,
            generation: 0,
            last_frame_instant: None,
            frame_intervals: VecDeque::with_capacity(120),
            last_decode_ms: 0.0,
            fast_count: 0,
            fallback_count: 0,
        }
    }
    /// 新しいフレームを格納し、置き換えられた古いフレームを返す。
    ///
    /// 返した `Arc` の参照が呼び出し側だけになっていれば、中の `Vec` を
    /// 次の変換先として回収できる。回収しない場合はそのまま捨ててよい。
    fn push_back(
        &mut self,
        frame: VideoFrame,
        decode_ms: f32,
        fast: bool,
    ) -> Option<Arc<VideoFrame>> {
        let replaced = self.latest.replace(Arc::new(frame));
        self.generation += 1;
        self.last_decode_ms = decode_ms;
        if fast {
            self.fast_count += 1;
        } else {
            self.fallback_count += 1;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_frame_instant.replace(now) {
            let dt = now.duration_since(prev).as_secs_f32() * 1000.0;
            if self.frame_intervals.len() == 120 {
                self.frame_intervals.pop_front();
            }
            self.frame_intervals.push_back(dt);
        }
        replaced
    }
    /// 直近のフレームとその世代番号を返す。新着かどうかは問わない。
    ///
    /// 返すのは `Arc` の複製なので、画素データはコピーされない。
    fn latest_frame(&self) -> Option<(Arc<VideoFrame>, u64)> {
        self.latest
            .as_ref()
            .map(|frame| (Arc::clone(frame), self.generation))
    }

    /// 保持しているフレームと統計を捨てる。キャプチャの停止時に呼ぶ。
    ///
    /// 世代番号は巻き戻さない。巻き戻すと、再接続後の最初のフレームが
    /// 取り出し側の記録している世代と一致して、新着と見なされなくなる。
    fn reset(&mut self) {
        self.latest = None;
        self.generation += 1;
        self.last_frame_instant = None;
        self.frame_intervals.clear();
        self.last_decode_ms = 0.0;
        self.fast_count = 0;
        self.fallback_count = 0;
    }
}

pub struct VideoCapture {
    camera: Option<CallbackCamera>,
    frames: Arc<Mutex<FrameBuffer>>,
    is_active: bool,
}

impl VideoCapture {
    pub fn new() -> Self {
        Self {
            camera: None,
            frames: Arc::new(Mutex::new(FrameBuffer::new())),
            is_active: false,
        }
    }

    pub fn list_devices() -> Vec<(String, String)> {
        match nokhwa::query(ApiBackend::MediaFoundation) {
            Ok(devices) => devices
                .into_iter()
                .map(|info| {
                    (
                        info.human_name().to_string(),
                        info.description().to_string(),
                    )
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn start_capture(
        &mut self,
        device_name: Option<&str>,
        resolution: Option<(u32, u32)>,
        format: Option<&str>,
        fps: Option<u32>,
    ) -> Result<(), String> {
        self.stop_capture();

        let devices = nokhwa::query(ApiBackend::MediaFoundation)
            .map_err(|e| format!("Failed to query devices: {}", e))?;

        let device_info = if let Some(name) = device_name {
            devices
                .into_iter()
                .find(|d| d.human_name() == name)
                .ok_or_else(|| format!("Device '{}' not found", name))?
        } else {
            devices.into_iter().next().ok_or("No video devices found")?
        };

        // Windows Media Foundationでの問題を回避するフォーマット設定
        let requested_format = if let Some((w, h)) = resolution {
            let ff = match format.unwrap_or("") {
                "YUY2" => FrameFormat::YUYV,
                // MJPEGとRGB24はWindows MFで問題があるため、YUYVフォールバック
                "MJPEG" => FrameFormat::YUYV, // YUYVで代替してMJPEGシミュレート
                "RGB24" => FrameFormat::YUYV, // YUYVで代替してRGB変換
                // 未指定: デフォルトフォーマット
                "" => FrameFormat::YUYV,
                _ => FrameFormat::YUYV,
            };
            let fps_value = fps.unwrap_or(60).clamp(15, 120);

            // フォールバック戦略: 安定したYUYVを使用
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
                Resolution::new(w, h),
                ff,
                fps_value,
            )))
        } else {
            // 高解像度優先（安定性のためYUYVを使用）
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
                Resolution::new(1280, 720),
                FrameFormat::YUYV,
                60,
            )))
        };

        let frame_callback = {
            let fb = self.frames.clone();
            // 直前に置き換えられたフレーム。UI スレッドが手放していれば
            // 中の Vec を次の変換先として回収し、毎フレームの確保を避ける。
            // 1 世代ぶん遅らせて回収するのは、置き換えた直後のフレームは
            // UI スレッドがテクスチャ化のために掴んでいることが多いため。
            let mut recyclable: Option<Arc<VideoFrame>> = None;
            move |frame: nokhwa::Buffer| {
                let start = Instant::now();
                let res = frame.resolution();
                let width = res.width_x as usize;
                let height = res.height_y as usize;
                // YUY2 高速パス (naive) 試行
                let mut used_fast = false;
                #[allow(unused_mut)]
                let mut rgb_vec: Option<Vec<u8>> = None;
                // フレームフォーマットを取得して適切な処理を行う
                let source_format = frame.source_frame_format();

                match source_format {
                    FrameFormat::YUYV if width.is_multiple_of(2) => {
                        // YUY2の高速パス
                        let raw_data = frame.buffer_bytes();
                        if raw_data.len() >= width * height * 2 {
                            // 回収できた Vec があれば使い回し、無ければ新規に確保する
                            let mut rgb = recyclable
                                .take()
                                .and_then(|previous| Arc::try_unwrap(previous).ok())
                                .map(|previous| previous.data)
                                .unwrap_or_default();
                            yuy2_to_rgb_naive(width, height, &raw_data, &mut rgb);
                            rgb_vec = Some(rgb);
                            used_fast = true;
                        }
                    }

                    _ => {
                        // その他のフォーマットも標準デコード
                        if let Ok(rgb_data) = frame.decode_image::<RgbFormat>() {
                            rgb_vec = Some(rgb_data.into_raw());
                        }
                    }
                }
                if let Some(data) = rgb_vec {
                    let decode_ms = start.elapsed().as_secs_f32() * 1000.0;
                    let vf = VideoFrame {
                        width,
                        height,
                        data,
                    };
                    if let Ok(mut guard) = fb.lock() {
                        recyclable = guard.push_back(vf, decode_ms, used_fast);
                    }
                }
            }
        };

        let mut camera = CallbackCamera::new(
            device_info.index().clone(),
            requested_format,
            frame_callback,
        )
        .map_err(|e| format!("Failed to create camera: {}", e))?;

        camera
            .open_stream()
            .map_err(|e| format!("Failed to open camera stream: {}", e))?;

        self.camera = Some(camera);
        self.is_active = true;

        Ok(())
    }

    pub fn stop_capture(&mut self) {
        if let Some(mut camera) = self.camera.take() {
            let _ = camera.stop_stream();
        }
        self.is_active = false;

        if let Ok(mut buf) = self.frames.lock() {
            buf.reset();
        }
    }

    /// 直近のフレームを新着かどうかに関わらず返す。
    ///
    /// スクリーンショットは「いま画面に出ている画」を保存するものなので、
    /// 新着でなくても最後に届いたフレームを返す必要がある。
    pub fn get_latest_frame(&self) -> Option<Arc<VideoFrame>> {
        self.frames
            .lock()
            .ok()
            .and_then(|fb| fb.latest_frame().map(|(frame, _)| frame))
    }

    /// 世代番号が `last_generation` と異なるフレームがある場合だけ、
    /// フレームと世代番号を返す。
    ///
    /// 新着がなければ `None` を返すので、呼び出し側は前回の結果を使い回せる。
    pub fn get_frame_if_newer(&self, last_generation: u64) -> Option<(Arc<VideoFrame>, u64)> {
        self.frames
            .lock()
            .ok()
            .and_then(|fb| match fb.latest_frame() {
                Some((frame, generation)) if generation != last_generation => {
                    Some((frame, generation))
                }
                _ => None,
            })
    }

    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    #[allow(dead_code)]
    pub fn get_supported_formats(&self) -> Vec<(String, Vec<(u32, u32)>)> {
        // 簡略化された実装 - 実際のフォーマットには、より複雑なロジックが必要
        vec![
            (
                "MJPEG".to_string(),
                vec![(1920, 1080), (1280, 720), (640, 480)],
            ),
            ("YUY2".to_string(), vec![(1280, 720), (640, 480)]),
        ]
    }

    #[allow(dead_code)]
    pub fn get_supported_formats_for(_device: &str) -> Vec<(String, Vec<(u32, u32)>)> {
        // デバイス毎のプレースホルダー; 実際の実装ではデバイス機能を照会
        vec![
            (
                "MJPEG".to_string(),
                vec![(1920, 1080), (1280, 720), (640, 480)],
            ),
            ("YUY2".to_string(), vec![(1280, 720), (640, 480)]),
            ("RGB24".to_string(), vec![(1280, 720), (640, 480)]),
        ]
    }

    // デバイスの能力を取得するメソッド
    pub fn get_device_capabilities(
        device_name: Option<&str>,
    ) -> Result<DeviceCapabilities, String> {
        use nokhwa::Camera;

        // デバイス情報を取得
        let devices = nokhwa::query(ApiBackend::MediaFoundation)
            .map_err(|e| format!("Failed to query devices: {}", e))?;

        let device_info = if let Some(name) = device_name {
            devices
                .into_iter()
                .find(|d| d.human_name() == name)
                .ok_or_else(|| format!("Device '{}' not found", name))?
        } else {
            devices.into_iter().next().ok_or("No video devices found")?
        };

        // カメラを一時的に開いて能力を取得
        let requested_format = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
            CameraFormat::new(Resolution::new(640, 480), FrameFormat::YUYV, 30),
        ));

        let mut camera = Camera::new(device_info.index().clone(), requested_format)
            .map_err(|e| format!("Failed to create camera for capability query: {}", e))?;

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
                    let mut resolutions_with_fps: Vec<(u32, u32, u32)> = Vec::new();

                    for (resolution, fps_list) in resolution_map.iter() {
                        // 各解像度に対して利用可能な全FPSを記録
                        for fps in fps_list.iter() {
                            resolutions_with_fps.push((
                                resolution.width_x,
                                resolution.height_y,
                                *fps,
                            ));
                        }
                    }

                    // 重複を削除してユニークな組み合わせのみ保持
                    resolutions_with_fps.sort();
                    resolutions_with_fps.dedup();

                    // 解像度でソート（大きい順）、同じ解像度ならFPSでソート（大きい順）
                    resolutions_with_fps.sort_by(|a, b| {
                        let size_a = a.0 * a.1;
                        let size_b = b.0 * b.1;
                        match size_b.cmp(&size_a) {
                            std::cmp::Ordering::Equal => b.2.cmp(&a.2),
                            other => other,
                        }
                    });

                    if !resolutions_with_fps.is_empty() {
                        result.push((format_name.to_string(), resolutions_with_fps));
                    }
                }
                Err(_) => {
                    // エラーの場合、デフォルト値を設定
                    let default_resolutions = match format_name {
                        "YUY2" => vec![(1280, 720, 60), (640, 480, 30)],
                        "MJPEG" => vec![(1920, 1080, 30), (1280, 720, 60), (640, 480, 30)],
                        "RGB24" => vec![(1280, 720, 30), (640, 480, 30)],
                        _ => vec![],
                    };
                    if !default_resolutions.is_empty() {
                        result.push((format_name.to_string(), default_resolutions));
                    }
                }
            }
        }

        // 結果が空の場合はデフォルト値を返す
        if result.is_empty() {
            result = vec![
                ("YUY2".to_string(), vec![(1280, 720, 60), (640, 480, 30)]),
                (
                    "MJPEG".to_string(),
                    vec![(1920, 1080, 30), (1280, 720, 60), (640, 480, 30)],
                ),
            ];
        }

        Ok(result)
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

    const TEST_WIDTH: usize = 2;
    const TEST_HEIGHT: usize = 2;
    const TEST_FRAME_LEN: usize = TEST_WIDTH * TEST_HEIGHT * 3;

    /// 変換結果を新しい Vec で受け取るテスト用ヘルパー。
    /// 出力先の使い回しそのものを見るテストは `yuy2_to_rgb_naive` を直接呼ぶ
    fn convert_yuy2(width: usize, height: usize, src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        yuy2_to_rgb_naive(width, height, src, &mut out);
        out
    }

    /// 識別しやすいように全画素を marker で埋めたフレームを作る
    fn test_frame(marker: u8) -> VideoFrame {
        VideoFrame {
            width: TEST_WIDTH,
            height: TEST_HEIGHT,
            data: vec![marker; TEST_FRAME_LEN],
        }
    }

    #[test]
    fn frame_buffer_latest_frame_after_push_returns_newest_frame() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), 1.0, true);
        buffer.push_back(test_frame(2), 1.0, true);

        let (frame, generation) = buffer
            .latest_frame()
            .expect("push 済みなのでフレームが取れる");
        assert_eq!(frame.width, TEST_WIDTH);
        assert_eq!(frame.height, TEST_HEIGHT);
        assert_eq!(frame.data, vec![2u8; TEST_FRAME_LEN]);
        assert_eq!(generation, 2);
    }

    #[test]
    fn frame_buffer_latest_frame_without_push_returns_none() {
        let buffer = FrameBuffer::new();
        assert!(buffer.latest_frame().is_none());
    }

    #[test]
    fn frame_buffer_latest_frame_without_new_push_keeps_generation() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), 1.0, true);

        let (_, first) = buffer.latest_frame().expect("1 枚目が取れる");
        let (_, second) = buffer.latest_frame().expect("取り出しても消えない");
        assert_eq!(first, second, "push が無ければ世代は進まない");

        buffer.push_back(test_frame(2), 1.0, true);
        let (_, third) = buffer.latest_frame().expect("2 枚目が取れる");
        assert_eq!(third, second + 1, "push すれば世代が 1 つ進む");
    }

    #[test]
    fn frame_buffer_latest_frame_twice_shares_same_allocation() {
        // 取り出しで画素データが複製されないこと（このタスクの本題）
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), 1.0, true);

        let (first, _) = buffer.latest_frame().expect("1 回目");
        let (second, _) = buffer.latest_frame().expect("2 回目");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn frame_buffer_reset_drops_frame_and_advances_generation() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), 1.0, true);
        let (_, before) = buffer.latest_frame().expect("push 済み");

        buffer.reset();
        assert!(buffer.latest_frame().is_none());

        // 再接続後の最初のフレームが「新着」と判別できること
        buffer.push_back(test_frame(2), 1.0, true);
        let (_, after) = buffer.latest_frame().expect("再接続後の 1 枚目");
        assert!(after > before);
    }

    #[test]
    fn frame_buffer_concurrent_push_returns_latest_frame_without_panic() {
        // コールバックスレッドが push し続ける裏で UI スレッドが取り出す状況を模す
        const PUSH_COUNT: usize = 500;
        let buffer = Arc::new(Mutex::new(FrameBuffer::new()));

        let writer = {
            let buffer = Arc::clone(&buffer);
            std::thread::spawn(move || {
                for i in 0..PUSH_COUNT {
                    buffer.lock().expect("書き込み側のロックに失敗").push_back(
                        test_frame(i as u8),
                        1.0,
                        true,
                    );
                }
            })
        };

        let mut last_generation = 0;
        while !writer.is_finished() {
            if let Some((_, generation)) = buffer
                .lock()
                .expect("読み出し側のロックに失敗")
                .latest_frame()
            {
                assert!(generation >= last_generation, "世代は巻き戻らない");
                last_generation = generation;
            }
        }
        writer.join().expect("書き込みスレッドがパニックした");

        let (frame, generation) = buffer
            .lock()
            .expect("読み出し側のロックに失敗")
            .latest_frame()
            .expect("最後に push したフレームが残っている");
        assert_eq!(frame.data, vec![(PUSH_COUNT - 1) as u8; TEST_FRAME_LEN]);
        assert_eq!(generation, PUSH_COUNT as u64);
    }
    // 期待値は BT.601 の整数近似式を手計算した結果をベタ書きする。
    // 実装と同じ式で計算すると、実装が誤っていてもテストが通ってしまうため。

    #[test]
    fn yuy2_to_rgb_naive_known_pattern_converts_two_pixels() {
        // Y0=81, U=90, Y1=145, V=240 (赤寄りの YUYV ペア)
        let src = [81u8, 90, 145, 240];
        let out = convert_yuy2(2, 1, &src);
        assert_eq!(out, vec![254, 0, 0, 255, 73, 73]);
    }

    #[test]
    fn yuy2_to_rgb_naive_output_length_is_width_times_height_times_three() {
        let src = [235u8, 128, 235, 128, 235, 128, 235, 128];
        let out = convert_yuy2(2, 2, &src);
        assert_eq!(out.len(), 2 * 2 * 3);
        assert_eq!(
            out,
            vec![254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254, 254]
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_odd_width_leaves_last_pixel_black() {
        // 幅が奇数だと出力が 6 バイト単位で割り切れず、最後の 1 画素は変換されず 0 のまま残る
        let src = [235u8, 128, 235, 128, 0, 0];
        let out = convert_yuy2(3, 1, &src);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_short_source_leaves_remaining_pixels_black() {
        // 入力が 1 ペア分しかない場合、残りの画素は 0 のまま (パニックしない)
        let src = [235u8, 128, 235, 128];
        let out = convert_yuy2(4, 1, &src);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えるため飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src);
        assert_eq!(out, vec![255, 125, 255, 255, 125, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_min_input_saturates_at_0() {
        // Y=0, U=0, V=0 では R と B が負になるため 0 に飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src);
        assert_eq!(out, vec![0, 135, 0, 0, 135, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_zero_size_returns_empty() {
        let out = convert_yuy2(0, 0, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_clears_unconverted_area() {
        // 使い回した Vec に前フレームの画素が残っていても、
        // 変換されない領域は 0 になること
        let mut out = vec![0xFFu8; 12];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(4, 1, &src, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_shrinks_to_new_size() {
        // 解像度が小さくなっても出力長が追従し、前の内容が残らないこと
        let mut out = vec![0xFFu8; 24];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(2, 1, &src, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_keeps_allocation() {
        // 同じ解像度で呼び直したときに再確保が起きないこと（このタスクの本題）
        let src = [81u8, 90, 145, 240, 81, 90, 145, 240];
        let mut out = Vec::new();
        yuy2_to_rgb_naive(2, 2, &src, &mut out);
        let first_ptr = out.as_ptr();
        let first_capacity = out.capacity();

        yuy2_to_rgb_naive(2, 2, &src, &mut out);
        assert_eq!(out.as_ptr(), first_ptr, "確保済みの領域を使い回す");
        assert_eq!(out.capacity(), first_capacity);
    }

    #[test]
    fn frame_buffer_push_back_returns_replaced_frame() {
        // 置き換えられたフレームを受け取れること。
        // コールバック側はこれを回収して変換先に使い回す
        let mut buffer = FrameBuffer::new();
        assert!(
            buffer.push_back(test_frame(1), 1.0, true).is_none(),
            "1 枚目は置き換える対象が無い"
        );

        let replaced = buffer
            .push_back(test_frame(2), 1.0, true)
            .expect("2 枚目は 1 枚目を置き換える");
        assert_eq!(replaced.data, vec![1u8; TEST_FRAME_LEN]);
        assert!(
            Arc::try_unwrap(replaced).is_ok(),
            "取り出し側が保持していなければ Vec を回収できる"
        );
    }

    #[test]
    #[ignore = "計測用"]
    fn yuy2_to_rgb_naive_1080p_conversion_time() {
        // 実行: cargo test --release -- --ignored --nocapture
        // 毎フレームの新規確保と、確保済み Vec の使い回しを比べる
        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1080;
        const FRAMES: usize = 120;

        // 1080p 相当のダミー YUYV。定数畳み込みを避けるため画素ごとに値を変える
        let src: Vec<u8> = (0..WIDTH * HEIGHT * 2).map(|i| (i % 251) as u8).collect();

        let allocating_start = Instant::now();
        for _ in 0..FRAMES {
            let mut out = Vec::new();
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &mut out);
            std::hint::black_box(&out);
        }
        let allocating_ms = allocating_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        let mut out = Vec::new();
        yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &mut out);
        let reusing_start = Instant::now();
        for _ in 0..FRAMES {
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &mut out);
            std::hint::black_box(&out);
        }
        let reusing_ms = reusing_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        println!(
            "1080p YUY2->RGB {} frames: allocate={:.3} ms/frame, reuse={:.3} ms/frame",
            FRAMES, allocating_ms, reusing_ms
        );
    }
}
