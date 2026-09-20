use log::{debug, info, trace, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::CallbackCamera;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// 「最初の 1 回だけ」を判定するフラグ。
///
/// フレームコールバックは 1080p60 なら毎秒 60 回呼ばれるため、到着や
/// フォールバックをそのまま記録するとログが埋まる。初回だけ記録するための
/// 判定をここに閉じ込めて、単体テストできるようにしてある。
#[derive(Debug, Default)]
struct FirstTimeOnly {
    fired: bool,
}

impl FirstTimeOnly {
    /// 最初に呼ばれたときだけ `true` を返す。2 回目以降は常に `false`。
    fn take(&mut self) -> bool {
        let first = !self.fired;
        self.fired = true;
        first
    }
}

/// 経過時間をミリ秒で返す。ログの書式を揃えるための補助。
fn elapsed_ms(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
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

    /// 最後にフレームが届いてからの経過時間。1 枚も届いていなければ `None`。
    fn since_last_frame(&self) -> Option<Duration> {
        self.last_frame_instant.map(|at| at.elapsed())
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
    ) -> Result<(), String> {
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
            .map_err(|e| format!("Failed to query devices: {}", e))?;
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
                .ok_or_else(|| format!("Device '{}' not found", name))?
        } else {
            devices.into_iter().next().ok_or("No video devices found")?
        };

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
            let fb = self.frames.clone();
            // 直前に置き換えられたフレーム。UI スレッドが手放していれば
            // 中の Vec を次の変換先として回収し、毎フレームの確保を避ける。
            // 1 世代ぶん遅らせて回収するのは、置き換えた直後のフレームは
            // UI スレッドがテクスチャ化のために掴んでいることが多いため。
            let mut recyclable: Option<Arc<VideoFrame>> = None;
            // 毎フレーム流れる事象のうち、初回だけ記録したいもの。
            // 2 回目以降は trace! に落とすか、何も出さない
            let mut first_frame = FirstTimeOnly::default();
            let mut fallback_notice = FirstTimeOnly::default();
            let mut short_frame_notice = FirstTimeOnly::default();
            let mut decode_error_notice = FirstTimeOnly::default();
            let mut lock_error_notice = FirstTimeOnly::default();
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
                        if raw_data.len() < width * height * 2 {
                            // フレームを捨てるので画面が止まる。以降は同じ行が
                            // 毎フレーム出るため初回だけ残す
                            if short_frame_notice.take() {
                                warn!(
                                    "YUY2 のフレームが短いので破棄した（{}x{} に必要な {} バイトに対し {} バイト）。以降は記録しない",
                                    width,
                                    height,
                                    width * height * 2,
                                    raw_data.len()
                                );
                            }
                        } else {
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
                        if fallback_notice.take() {
                            warn!(
                                "YUY2 の高速パスを使えないのでデコーダへフォールバックする（フォーマット: {:?}、{}x{}）。以降は記録しない",
                                source_format, width, height
                            );
                        }
                        match frame.decode_image::<RgbFormat>() {
                            Ok(rgb_data) => rgb_vec = Some(rgb_data.into_raw()),
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
                if let Some(data) = rgb_vec {
                    let decode_ms = start.elapsed().as_secs_f32() * 1000.0;
                    let format_name = frame_format_name(source_format);
                    let vf = VideoFrame {
                        width,
                        height,
                        data,
                    };
                    match fb.lock() {
                        Ok(mut guard) => {
                            recyclable =
                                guard.push_back(vf, start, decode_ms, used_fast, format_name);
                            if first_frame.take() {
                                // 「接続した」と「映像が出ている」は別物なので、
                                // 最初の 1 枚が届いたことだけは info で残す
                                info!(
                                    "最初のフレームが届いた（{}x{}、フォーマット: {:?}、変換 {:.2}ms、経路: {}）",
                                    width,
                                    height,
                                    source_format,
                                    decode_ms,
                                    if used_fast { "高速パス" } else { "デコーダ" }
                                );
                            } else {
                                trace!(
                                    "フレームが届いた（{}x{}、変換 {:.2}ms）",
                                    width,
                                    height,
                                    decode_ms
                                );
                            }
                        }
                        Err(_) => {
                            if lock_error_notice.take() {
                                warn!(
                                    "フレームバッファのロックを取得できないのでフレームを捨てた。以降は記録しない"
                                );
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
        .map_err(|e| format!("Failed to create camera: {}", e))?;
        let create_ms = elapsed_ms(create_start);

        // 実際に確定したフォーマットは open_stream の前に読む。
        // ストリーム開始後は nokhwa のフレーム取得スレッドがカメラのロックを
        // 握り続けるため、待たされて UI スレッドが止まる
        //
        // fps は載せない。nokhwa のバインディングが MF_MT_FRAME_RATE
        // （上位 32 ビットが分子、下位 32 ビットが分母）を `fps as u32` で
        // 読んでおり、分母しか取れていない。整数フレームレートでは常に 1 になる
        // （nokhwa-bindings-windows 0.4.6 の `format_refreshed`）。
        // 要求した fps は直前の debug! に残してある
        let actual_format = camera.camera_format().ok();

        let open_start = Instant::now();
        camera
            .open_stream()
            .map_err(|e| format!("Failed to open camera stream: {}", e))?;
        let open_ms = elapsed_ms(open_start);

        info!(
            "映像ストリームを開いた（デバイス: {}、実際の設定: {}、Camera::new {:.1}ms、open_stream {:.1}ms）",
            device_info.human_name(),
            actual_format
                .map(|f| format!(
                    "{}x{} {:?}",
                    f.resolution().width_x,
                    f.resolution().height_y,
                    f.format()
                ))
                .unwrap_or_else(|| "（取得できない）".to_string()),
            create_ms,
            open_ms
        );

        self.camera = Some(camera);

        Ok(())
    }

    pub fn stop_capture(&mut self) {
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

        if let Ok(mut buf) = self.frames.lock() {
            buf.reset();
        } else {
            warn!("フレームバッファのロックを取得できないので統計を消せない");
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

    /// ストリームが開いているかと、フレームの途絶時間を返す。
    ///
    /// 切断の監視のために毎フレーム呼ばれる。ロックの中で行うのは
    /// `Instant` の減算だけで、フレームコールバックをほとんど待たせない。
    /// ロックを取れなかった場合は「まだ 1 枚も届いていない」として返す。
    /// 途絶時間が取れない状態で切断と判断させないため
    pub fn link_state(&self) -> VideoLinkState {
        VideoLinkState {
            capturing: self.camera.is_some(),
            since_last_frame: self.frames.lock().ok().and_then(|fb| fb.since_last_frame()),
        }
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

        let start = Instant::now();
        debug!(
            "デバイス能力の取得を開始する: {}",
            device_name.unwrap_or("（未指定。先頭のデバイス）")
        );

        // 失敗をここで warn! にしない。呼び出し側（main.rs の
        // dispatch_capability_requests）が、デバイス名付きで理由をログへ出し、
        // 設定ダイアログにも表示する。ここで出すと同じ内容が 2 行並ぶ
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

        // キャプチャ中のデバイスをもう一度開く。取得そのものは `capability-query`
        // スレッドで走るので UI は止まらないが、ここが伸びると設定ダイアログの
        // 「対応形式を取得中...」が長く出たままになる
        let open_start = Instant::now();
        let mut camera = Camera::new(device_info.index().clone(), requested_format)
            .map_err(|e| format!("Failed to create camera for capability query: {}", e))?;
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

                    debug!(
                        "{} の対応する組み合わせを {} 件取得した",
                        format_name,
                        resolutions_with_fps.len()
                    );
                    if !resolutions_with_fps.is_empty() {
                        result.push((format_name.to_string(), resolutions_with_fps));
                    }
                }
                Err(e) => {
                    // エラーの場合、デフォルト値を設定。
                    // 画面に出る選択肢がデバイスの実際の能力ではなくなるので残す
                    warn!(
                        "{} の対応する組み合わせを取得できないので既定値を使う: {}",
                        format_name, e
                    );
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
            warn!("どのフォーマットの能力も取得できなかったので既定値を返す");
            result = vec![
                ("YUY2".to_string(), vec![(1280, 720, 60), (640, 480, 30)]),
                (
                    "MJPEG".to_string(),
                    vec![(1920, 1080, 30), (1280, 720, 60), (640, 480, 30)],
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
                .map(|(format_name, list)| format!("{}: {} 件", format_name, list.len()))
                .collect::<Vec<_>>()
                .join("、")
        );

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
    fn first_time_only_first_take_returns_true() {
        let mut flag = FirstTimeOnly::default();
        assert!(flag.take());
    }

    #[test]
    fn first_time_only_subsequent_takes_return_false() {
        // 毎フレーム呼ばれる前提なので、2 回目以降は必ず false になること
        let mut flag = FirstTimeOnly::default();
        flag.take();
        assert!(!flag.take());
        assert!(!flag.take());
        assert!(!flag.take());
    }

    #[test]
    fn first_time_only_instances_are_independent() {
        // 「初回のフレーム」と「初回のフォールバック」を別々に数えるため、
        // 片方を消費してももう片方は初回のまま
        let mut first = FirstTimeOnly::default();
        let mut second = FirstTimeOnly::default();
        assert!(first.take());
        assert!(second.take());
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
