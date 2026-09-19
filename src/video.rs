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

/// YCbCr -> RGB 変換の係数。
///
/// リミテッドレンジ（Y 16〜235、Cb/Cr 16〜240）の信号をフルレンジ RGB（0〜255）へ
/// 展開する行列を、1024 倍の固定小数点（`>> 10` で戻す）で保持する。
///
/// 各係数の導出は以下。Kr / Kb は色空間ごとの輝度の重み、Kg = 1 - Kr - Kb。
///
/// ```text
/// y   = 255/219                        (Y のレンジ 219 段を 255 段へ伸ばす)
/// r_v = 255/224 * 2 * (1 - Kr)
/// g_u = 255/224 * 2 * Kb * (1 - Kb) / Kg
/// g_v = 255/224 * 2 * Kr * (1 - Kr) / Kg
/// b_u = 255/224 * 2 * (1 - Kb)
/// ```
///
/// `g_u` と `g_v` は減算に使うため、符号を除いた大きさを持つ。
#[derive(Debug, PartialEq, Eq)]
struct ColorMatrix {
    /// Y - 16 に掛ける係数
    y: i32,
    /// R への Cr - 128 の寄与
    r_v: i32,
    /// G から引く Cb - 128 の寄与
    g_u: i32,
    /// G から引く Cr - 128 の寄与
    g_v: i32,
    /// B への Cb - 128 の寄与
    b_u: i32,
}

/// BT.601（SD 向け。Kr = 0.299、Kb = 0.114）。
///
/// 1.164 / 1.596 / 0.392 / 0.813 / 2.017 に相当する。
/// `g_v` だけは上式の丸め（832）ではなく 833 を使っている。古くから出回っている
/// 整数版の定数をそのまま引き継いだもので、1/1024 の差しかないため変えていない。
static BT601: ColorMatrix = ColorMatrix {
    y: 1192,
    r_v: 1634,
    g_u: 401,
    g_v: 833,
    b_u: 2066,
};

/// BT.709（HD 向け。Kr = 0.2126、Kb = 0.0722）。
///
/// 上式に代入すると 1.16438 / 1.79274 / 0.21325 / 0.53291 / 2.11240 となり、
/// 1024 倍して四捨五入すると 1192 / 1836 / 218 / 546 / 2163 になる。
static BT709: ColorMatrix = ColorMatrix {
    y: 1192,
    r_v: 1836,
    g_u: 218,
    g_v: 546,
    b_u: 2163,
};

/// HD とみなす境界。これ以上なら BT.709 を使う。
///
/// HD の放送規格（ITU-R BT.709）は 1280x720 以上を対象としており、
/// それ未満の SD 解像度は BT.601 で符号化される。キャプチャーボードは
/// 入力信号の色空間を通知してこないため、解像度から推定するしかない。
const HD_MIN_WIDTH: usize = 1280;
const HD_MIN_HEIGHT: usize = 720;

/// 解像度から色空間を推定して係数を選ぶ。
///
/// 幅と高さのどちらかが HD の境界に達していれば BT.709 とみなす。
/// 1440x1080 のようにアスペクト比が 1:1 でない HD 形式があるため、
/// 片方だけを見ると取りこぼす。
fn color_matrix_for(width: usize, height: usize) -> &'static ColorMatrix {
    if width >= HD_MIN_WIDTH || height >= HD_MIN_HEIGHT {
        &BT709
    } else {
        &BT601
    }
}

/// YUY2 -> RGB24 の高速変換 (最適化版)。
///
/// 変換に使う係数は `matrix` で受け取る。解像度から選ぶ場合は
/// `color_matrix_for` を通す。
///
/// 変換結果は `out` へ書き込む。`out` は呼び出し側が使い回す前提で、
/// 毎フレームの確保・ゼロクリア・解放を避けるために `&mut Vec<u8>` で受け取る。
///
/// `width * height * 3` バイトへリサイズしたうえで全域を書き切る。
/// 変換できなかった領域（幅が奇数で余る 1 画素、入力が足りない画素）は
/// 使い回した Vec に残る前フレームの画素が見えないよう 0 で埋める。
fn yuy2_to_rgb_naive(
    width: usize,
    height: usize,
    src: &[u8],
    matrix: &ColorMatrix,
    out: &mut Vec<u8>,
) {
    // 既に確保済みの容量はそのまま使う。0 埋めが走るのは伸ばした分だけ
    out.resize(width * height * 3, 0);

    // 安全確保: 偶数幅前提 (YUYV ペア)
    // 4 バイト / 6 バイトに満たない端数は変換しない
    let converted_len = {
        // ループの外に出して、毎画素の間接参照を避ける
        let ColorMatrix {
            y: cy,
            r_v,
            g_u,
            g_v,
            b_u,
        } = *matrix;

        let (src_chunks, _) = src.as_chunks::<4>();
        let (out_chunks, _) = out.as_chunks_mut::<6>();
        let pair_count = src_chunks.len().min(out_chunks.len());

        for (src_chunk, out_chunk) in src_chunks.iter().zip(out_chunks.iter_mut()) {
            let y0 = src_chunk[0] as i32;
            let u = src_chunk[1] as i32;
            let y1 = src_chunk[2] as i32;
            let v = src_chunk[3] as i32;

            // リミテッドレンジの原点へ寄せる (整数演算で高速化)
            let c0 = y0 - 16;
            let c1 = y1 - 16;
            let d = u - 128;
            let e = v - 128;

            // 係数は 1024 倍の固定小数点なので >> 10 で戻す
            let r0 = (cy * c0 + r_v * e) >> 10;
            let g0 = (cy * c0 - g_u * d - g_v * e) >> 10;
            let b0 = (cy * c0 + b_u * d) >> 10;
            let r1 = (cy * c1 + r_v * e) >> 10;
            let g1 = (cy * c1 - g_u * d - g_v * e) >> 10;
            let b1 = (cy * c1 + b_u * d) >> 10;

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

/// フレーム間隔から求めたばらつきの指標。単位はミリ秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntervalStats {
    /// 平均間隔から求めた実効 FPS。
    ///
    /// nokhwa の `camera_format().frame_rate()` は当てにならない値を返すため、
    /// 実際に届いたフレームの間隔から計算する
    pub fps: f32,
    /// 平均間隔
    pub average_ms: f32,
    /// 最小間隔
    pub min_ms: f32,
    /// 最大間隔
    pub max_ms: f32,
    /// 間隔の母標準偏差。コマ落ちや取り込みの詰まりでここが膨らむ
    pub stddev_ms: f32,
    /// 集計に使ったサンプル数
    pub samples: usize,
}

/// 映像パイプラインの観測値。OSD の表示に使う。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameStats {
    /// フレーム間隔の集計。2 枚目が届くまでは `None`
    pub intervals: Option<IntervalStats>,
    /// 直近 1 フレームの RGB 変換にかかった時間（ミリ秒）
    pub last_decode_ms: f32,
    /// 自前の YUY2 変換（高速パス）を通ったフレーム数
    pub fast_count: u64,
    /// デコーダ任せの汎用パスを通ったフレーム数
    pub fallback_count: u64,
    /// 直近フレームの画素数。フレームが無ければ `None`
    pub resolution: Option<(usize, usize)>,
    /// 直近フレームの入力フォーマット名。フレームが無ければ `None`
    pub source_format: Option<&'static str>,
    /// 最後にフレームが届いてからの経過時間（ミリ秒）
    pub since_last_frame_ms: Option<f32>,
}

/// フレーム間隔の列から実効 FPS とばらつきを求める。
///
/// 要素が無ければ `None` を返す。フレームがまだ 1 枚も来ていない状態と、
/// 間隔が取れている状態を呼び出し側で区別できるようにするため。
///
/// 平均間隔が 0 の場合は FPS を 0 にする。そのまま割ると無限大になり、
/// 表示にそれが出てしまう。
fn interval_stats(intervals: &VecDeque<f32>) -> Option<IntervalStats> {
    let samples = intervals.len();
    if samples == 0 {
        return None;
    }

    let count = samples as f32;
    let average_ms = intervals.iter().sum::<f32>() / count;
    let mut min_ms = f32::MAX;
    let mut max_ms = f32::MIN;
    for &interval in intervals {
        min_ms = min_ms.min(interval);
        max_ms = max_ms.max(interval);
    }
    let variance = intervals
        .iter()
        .map(|interval| {
            let diff = interval - average_ms;
            diff * diff
        })
        .sum::<f32>()
        / count;

    Some(IntervalStats {
        fps: if average_ms > 0.0 {
            1000.0 / average_ms
        } else {
            0.0
        },
        average_ms,
        min_ms,
        max_ms,
        stddev_ms: variance.sqrt(),
        samples,
    })
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
    // 直近フレームの入力フォーマット。デバイスが要求どおりに開けたとは
    // 限らないため、設定値ではなく実際に届いたフレームのものを持つ
    source_format: Option<&'static str>,
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
            source_format: None,
        }
    }
    /// 新しいフレームを格納し、置き換えられた古いフレームを返す。
    ///
    /// 返した `Arc` の参照が呼び出し側だけになっていれば、中の `Vec` を
    /// 次の変換先として回収できる。回収しない場合はそのまま捨ててよい。
    fn push_back(
        &mut self,
        frame: VideoFrame,
        received_at: Instant,
        decode_ms: f32,
        fast: bool,
        source_format: &'static str,
    ) -> Option<Arc<VideoFrame>> {
        let replaced = self.latest.replace(Arc::new(frame));
        self.generation += 1;
        self.last_decode_ms = decode_ms;
        self.source_format = Some(source_format);
        if fast {
            self.fast_count += 1;
        } else {
            self.fallback_count += 1;
        }
        // 間隔は RGB 変換が終わった時刻ではなく、フレームを受け取った時刻で測る。
        // 変換時間が揺れると、その差が間隔へそのまま乗ってばらつきが実態より
        // 大きく出る
        let now = received_at;
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
        self.source_format = None;
    }

    /// OSD に出す観測値をまとめて返す。
    ///
    /// 間隔の集計は最大 120 要素の走査で済むため、表示中に毎フレーム
    /// 呼んでも問題にならない。表示していないときは呼ばない。
    fn stats(&self) -> FrameStats {
        FrameStats {
            intervals: interval_stats(&self.frame_intervals),
            last_decode_ms: self.last_decode_ms,
            fast_count: self.fast_count,
            fallback_count: self.fallback_count,
            resolution: self.latest.as_ref().map(|f| (f.width, f.height)),
            source_format: self.source_format,
            since_last_frame_ms: self
                .last_frame_instant
                .map(|at| at.elapsed().as_secs_f32() * 1000.0),
        }
    }
}

pub struct VideoCapture {
    camera: Option<CallbackCamera>,
    frames: Arc<Mutex<FrameBuffer>>,
}

impl VideoCapture {
    pub fn new() -> Self {
        Self {
            camera: None,
            frames: Arc::new(Mutex::new(FrameBuffer::new())),
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
                // 変換時間の計測と、フレーム間隔の基準を兼ねる受信時刻
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
                            // 入力信号の色空間は通知されないため解像度から推定する
                            let matrix = color_matrix_for(width, height);
                            yuy2_to_rgb_naive(width, height, &raw_data, matrix, &mut rgb);
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
                    let format_name = frame_format_name(source_format);
                    let vf = VideoFrame {
                        width,
                        height,
                        data,
                    };
                    if let Ok(mut guard) = fb.lock() {
                        recyclable = guard.push_back(vf, start, decode_ms, used_fast, format_name);
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

        Ok(())
    }

    pub fn stop_capture(&mut self) {
        if let Some(mut camera) = self.camera.take() {
            let _ = camera.stop_stream();
        }

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

    /// 映像パイプラインの観測値を返す。
    ///
    /// ロックの中では値のコピーと最大 120 要素の集計しか行わない。
    /// フレームコールバックを待たせないため、ここで重い処理をしない。
    /// ロックを取れなかった場合は既定値（フレーム無し）を返す。
    pub fn stats(&self) -> FrameStats {
        self.frames
            .lock()
            .ok()
            .map(|fb| fb.stats())
            .unwrap_or_default()
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
    use std::time::Duration;

    const TEST_WIDTH: usize = 2;
    const TEST_HEIGHT: usize = 2;
    const TEST_FRAME_LEN: usize = TEST_WIDTH * TEST_HEIGHT * 3;

    /// 変換結果を新しい Vec で受け取るテスト用ヘルパー。
    /// 出力先の使い回しそのものを見るテストは `yuy2_to_rgb_naive` を直接呼ぶ
    fn convert_yuy2(width: usize, height: usize, src: &[u8], matrix: &ColorMatrix) -> Vec<u8> {
        let mut out = Vec::new();
        yuy2_to_rgb_naive(width, height, src, matrix, &mut out);
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
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");
        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");

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
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");

        let (_, first) = buffer.latest_frame().expect("1 枚目が取れる");
        let (_, second) = buffer.latest_frame().expect("取り出しても消えない");
        assert_eq!(first, second, "push が無ければ世代は進まない");

        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");
        let (_, third) = buffer.latest_frame().expect("2 枚目が取れる");
        assert_eq!(third, second + 1, "push すれば世代が 1 つ進む");
    }

    #[test]
    fn frame_buffer_latest_frame_twice_shares_same_allocation() {
        // 取り出しで画素データが複製されないこと（このタスクの本題）
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");

        let (first, _) = buffer.latest_frame().expect("1 回目");
        let (second, _) = buffer.latest_frame().expect("2 回目");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn frame_buffer_reset_drops_frame_and_advances_generation() {
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2");
        let (_, before) = buffer.latest_frame().expect("push 済み");

        buffer.reset();
        assert!(buffer.latest_frame().is_none());

        // 再接続後の最初のフレームが「新着」と判別できること
        buffer.push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2");
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
                        Instant::now(),
                        1.0,
                        true,
                        "YUY2",
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
    // 期待値は係数表から手計算した結果をベタ書きする。
    // 実装と同じ式で計算すると、実装が誤っていてもテストが通ってしまうため。
    //
    // 計算式（`>> 10` は負の値では負の無限大方向へ丸められる）:
    //   c = Y - 16, d = Cb - 128, e = Cr - 128
    //   R = (y*c + r_v*e) >> 10
    //   G = (y*c - g_u*d - g_v*e) >> 10
    //   B = (y*c + b_u*d) >> 10

    #[test]
    fn yuy2_to_rgb_naive_bt601_known_pattern_converts_two_pixels() {
        // Y0=81, U=90, Y1=145, V=240 (赤寄りの YUYV ペア)
        let src = [81u8, 90, 145, 240];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![254, 0, 0, 255, 73, 73]);
    }

    #[test]
    fn yuy2_to_rgb_naive_output_length_is_width_times_height_times_three() {
        let src = [235u8, 128, 235, 128, 235, 128, 235, 128];
        let out = convert_yuy2(2, 2, &src, &BT601);
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
        let out = convert_yuy2(3, 1, &src, &BT601);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_short_source_leaves_remaining_pixels_black() {
        // 入力が 1 ペア分しかない場合、残りの画素は 0 のまま (パニックしない)
        let src = [235u8, 128, 235, 128];
        let out = convert_yuy2(4, 1, &src, &BT601);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt601_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えるため飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![255, 125, 255, 255, 125, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt601_min_input_saturates_at_0() {
        // Y=0, U=0, V=0 では R と B が負になるため 0 に飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src, &BT601);
        assert_eq!(out, vec![0, 135, 0, 0, 135, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_zero_size_returns_empty() {
        let out = convert_yuy2(0, 0, &[], &BT601);
        assert!(out.is_empty());
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_clears_unconverted_area() {
        // 使い回した Vec に前フレームの画素が残っていても、
        // 変換されない領域は 0 になること
        let mut out = vec![0xFFu8; 12];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(4, 1, &src, &BT601, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_shrinks_to_new_size() {
        // 解像度が小さくなっても出力長が追従し、前の内容が残らないこと
        let mut out = vec![0xFFu8; 24];
        let src = [235u8, 128, 235, 128];
        yuy2_to_rgb_naive(2, 1, &src, &BT601, &mut out);
        assert_eq!(out, vec![254, 254, 254, 254, 254, 254]);
    }

    #[test]
    fn yuy2_to_rgb_naive_reused_buffer_keeps_allocation() {
        // 同じ解像度で呼び直したときに再確保が起きないこと（このタスクの本題）
        let src = [81u8, 90, 145, 240, 81, 90, 145, 240];
        let mut out = Vec::new();
        yuy2_to_rgb_naive(2, 2, &src, &BT601, &mut out);
        let first_ptr = out.as_ptr();
        let first_capacity = out.capacity();

        yuy2_to_rgb_naive(2, 2, &src, &BT601, &mut out);
        assert_eq!(out.as_ptr(), first_ptr, "確保済みの領域を使い回す");
        assert_eq!(out.capacity(), first_capacity);
    }

    /// 期待値との差が許容範囲に収まっているか調べる。
    /// f32 の演算誤差を吸収するためだけのもので、判定の緩和には使わない
    fn assert_close(actual: f32, expected: f32, label: &str) {
        assert!(
            (actual - expected).abs() < 0.001,
            "{}: 実際の値 {} が期待値 {} と離れている",
            label,
            actual,
            expected
        );
    }

    #[test]
    fn interval_stats_no_samples_returns_none() {
        // フレームがまだ 1 枚も来ていない状態。
        // 0 サンプルで平均を出すと NaN になるので、ここで弾く
        assert_eq!(interval_stats(&VecDeque::new()), None);
    }

    #[test]
    fn interval_stats_single_sample_has_no_spread() {
        // サンプルが 1 つだけでも標準偏差の計算で落ちないこと
        let stats = interval_stats(&VecDeque::from(vec![20.0])).expect("1 件でも集計できる");
        assert_eq!(stats.samples, 1);
        assert_close(stats.fps, 50.0, "fps");
        assert_close(stats.average_ms, 20.0, "average_ms");
        assert_close(stats.min_ms, 20.0, "min_ms");
        assert_close(stats.max_ms, 20.0, "max_ms");
        assert_close(stats.stddev_ms, 0.0, "stddev_ms");
    }

    #[test]
    fn interval_stats_constant_interval_reports_zero_stddev() {
        // 16ms 間隔がきれいに並んでいる状態。62.5fps でばらつきは 0
        let stats =
            interval_stats(&VecDeque::from(vec![16.0, 16.0, 16.0, 16.0])).expect("集計できる");
        assert_eq!(stats.samples, 4);
        assert_close(stats.fps, 62.5, "fps");
        assert_close(stats.average_ms, 16.0, "average_ms");
        assert_close(stats.stddev_ms, 0.0, "stddev_ms");
    }

    #[test]
    fn interval_stats_varying_interval_reports_spread() {
        // 10ms と 20ms が交互に来る状態。平均 15ms、母標準偏差 5ms
        let stats =
            interval_stats(&VecDeque::from(vec![10.0, 20.0, 10.0, 20.0])).expect("集計できる");
        assert_eq!(stats.samples, 4);
        assert_close(stats.average_ms, 15.0, "average_ms");
        assert_close(stats.min_ms, 10.0, "min_ms");
        assert_close(stats.max_ms, 20.0, "max_ms");
        assert_close(stats.stddev_ms, 5.0, "stddev_ms");
        assert_close(stats.fps, 66.6667, "fps");
    }

    #[test]
    fn interval_stats_zero_average_reports_zero_fps() {
        // 間隔の計測が全て 0 になった場合。1000 / 0 は無限大になるため、
        // そのまま表示へ流さず 0 にする
        let stats = interval_stats(&VecDeque::from(vec![0.0, 0.0])).expect("集計できる");
        assert_close(stats.fps, 0.0, "fps");
        assert_close(stats.average_ms, 0.0, "average_ms");
    }

    #[test]
    fn frame_buffer_stats_reports_latest_frame_and_paths() {
        // 高速パスと汎用パスの回数、解像度、フォーマット名が
        // 押し込んだとおりに読み出せること
        let mut buffer = FrameBuffer::new();
        // 受信時刻を 16ms 離して渡す。間隔は変換にかかった時間ではなく、
        // この差から計算されなければならない
        let now = Instant::now();
        buffer.push_back(
            test_frame(1),
            now - Duration::from_millis(32),
            2.5,
            true,
            "YUY2",
        );
        buffer.push_back(
            test_frame(2),
            now - Duration::from_millis(16),
            3.5,
            false,
            "MJPEG",
        );

        let stats = buffer.stats();
        let intervals = stats.intervals.expect("2 枚押し込めば間隔が 1 つ取れる");
        assert_eq!(intervals.samples, 1);
        assert_close(intervals.average_ms, 16.0, "average_ms");
        assert_close(intervals.fps, 62.5, "fps");
        assert_eq!(stats.fast_count, 1);
        assert_eq!(stats.fallback_count, 1);
        assert_close(stats.last_decode_ms, 3.5, "last_decode_ms");
        assert_eq!(stats.resolution, Some((TEST_WIDTH, TEST_HEIGHT)));
        assert_eq!(stats.source_format, Some("MJPEG"));
        assert!(stats.since_last_frame_ms.is_some());
    }

    #[test]
    fn frame_buffer_stats_after_reset_has_no_frame() {
        // キャプチャを止めたあとに前回の統計が残らないこと
        let mut buffer = FrameBuffer::new();
        buffer.push_back(test_frame(1), Instant::now(), 2.5, true, "YUY2");
        buffer.push_back(test_frame(2), Instant::now(), 3.5, true, "YUY2");
        buffer.reset();

        let stats = buffer.stats();
        assert_eq!(stats.intervals, None);
        assert_eq!(stats.resolution, None);
        assert_eq!(stats.source_format, None);
        assert_eq!(stats.since_last_frame_ms, None);
        assert_eq!(stats.fast_count, 0);
        assert_eq!(stats.fallback_count, 0);
        assert_close(stats.last_decode_ms, 0.0, "last_decode_ms");
    }

    #[test]
    fn frame_buffer_push_back_returns_replaced_frame() {
        // 置き換えられたフレームを受け取れること。
        // コールバック側はこれを回収して変換先に使い回す
        let mut buffer = FrameBuffer::new();
        assert!(
            buffer
                .push_back(test_frame(1), Instant::now(), 1.0, true, "YUY2")
                .is_none(),
            "1 枚目は置き換える対象が無い"
        );

        let replaced = buffer
            .push_back(test_frame(2), Instant::now(), 1.0, true, "YUY2")
            .expect("2 枚目は 1 枚目を置き換える");
        assert_eq!(replaced.data, vec![1u8; TEST_FRAME_LEN]);
        assert!(
            Arc::try_unwrap(replaced).is_ok(),
            "取り出し側が保持していなければ Vec を回収できる"
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_known_pattern_converts_two_pixels() {
        // BT.601 のテストと同じ入力。Y0=81, U=90, Y1=145, V=240
        let src = [81u8, 90, 145, 240];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709),
            vec![255, 24, 0, 255, 98, 69]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601),
            vec![254, 0, 0, 255, 73, 73],
            "同じ入力でも係数が違えば結果が変わる"
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_neutral_gray_matches_bt601() {
        // Cb = Cr = 128 の無彩色では色差の項が 0 になるため、
        // Y の係数が同じ 1192 である両者の結果は一致する
        let src = [235u8, 128, 235, 128];
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT709),
            vec![254, 254, 254, 254, 254, 254]
        );
        assert_eq!(
            convert_yuy2(2, 1, &src, &BT601),
            convert_yuy2(2, 1, &src, &BT709)
        );
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_max_input_saturates_at_255() {
        // Y=255, U=255, V=255 では R と B が 255 を超えるため飽和する
        let src = [255u8, 255, 255, 255];
        let out = convert_yuy2(2, 1, &src, &BT709);
        assert_eq!(out, vec![255, 183, 255, 255, 183, 255]);
    }

    #[test]
    fn yuy2_to_rgb_naive_bt709_min_input_saturates_at_0() {
        // Y=0, U=0, V=0 では R と B が負になるため 0 に飽和する
        let src = [0u8, 0, 0, 0];
        let out = convert_yuy2(2, 1, &src, &BT709);
        assert_eq!(out, vec![0, 76, 0, 0, 76, 0]);
    }

    #[test]
    fn color_matrix_for_sd_resolution_returns_bt601() {
        // 640x480 (VGA)、720x480 (NTSC)、720x576 (PAL) はいずれも SD
        assert_eq!(color_matrix_for(640, 480), &BT601);
        assert_eq!(color_matrix_for(720, 480), &BT601);
        assert_eq!(color_matrix_for(720, 576), &BT601);
    }

    #[test]
    fn color_matrix_for_hd_resolution_returns_bt709() {
        assert_eq!(color_matrix_for(1280, 720), &BT709);
        assert_eq!(color_matrix_for(1920, 1080), &BT709);
        assert_eq!(color_matrix_for(3840, 2160), &BT709);
    }

    #[test]
    fn color_matrix_for_just_below_hd_threshold_returns_bt601() {
        // 幅・高さの両方が境界に届かない場合だけ BT.601
        assert_eq!(color_matrix_for(1279, 719), &BT601);
    }

    #[test]
    fn color_matrix_for_either_dimension_at_threshold_returns_bt709() {
        // 1440x1080 のようにアスペクト比が 1:1 でない HD 形式を取りこぼさないため、
        // 幅と高さのどちらかが境界に達していれば BT.709 とみなす
        assert_eq!(color_matrix_for(1280, 719), &BT709);
        assert_eq!(color_matrix_for(1279, 720), &BT709);
        assert_eq!(color_matrix_for(1440, 1080), &BT709);
    }

    #[test]
    fn color_matrix_for_zero_size_returns_bt601() {
        // 解像度が取れない異常系。どちらかに倒すしかないので SD 側へ倒す
        assert_eq!(color_matrix_for(0, 0), &BT601);
    }

    #[test]
    #[ignore = "計測用"]
    fn yuy2_to_rgb_naive_1080p_conversion_time() {
        // 実行: cargo test --release -- --ignored --nocapture
        // 毎フレームの新規確保と、確保済み Vec の使い回しを比べる
        //
        // 計測値の出力に println! を使う。アプリ本体では
        // #![windows_subsystem = "windows"] のため標準出力はどこにも届かないが、
        // テストバイナリの標準出力は cargo がパイプで受け取るため
        // --nocapture を付ければ表示される（実測で確認済み）
        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1080;
        const FRAMES: usize = 120;

        // 1080p 相当のダミー YUYV。定数畳み込みを避けるため画素ごとに値を変える
        let src: Vec<u8> = (0..WIDTH * HEIGHT * 2).map(|i| (i % 251) as u8).collect();

        let allocating_start = Instant::now();
        for _ in 0..FRAMES {
            let mut out = Vec::new();
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
            std::hint::black_box(&out);
        }
        let allocating_ms = allocating_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        let mut out = Vec::new();
        yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
        let reusing_start = Instant::now();
        for _ in 0..FRAMES {
            yuy2_to_rgb_naive(WIDTH, HEIGHT, &src, &BT709, &mut out);
            std::hint::black_box(&out);
        }
        let reusing_ms = reusing_start.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;

        println!(
            "1080p YUY2->RGB {} frames: allocate={:.3} ms/frame, reuse={:.3} ms/frame",
            FRAMES, allocating_ms, reusing_ms
        );
    }
}
