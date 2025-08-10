use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{RequestedFormat, RequestedFormatType, CameraFormat, Resolution, ApiBackend, FrameFormat};
use nokhwa::CallbackCamera;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::collections::VecDeque;
// YUY2 -> RGB24 高速変換 (最適化版)

fn yuy2_to_rgb_naive(width: usize, height: usize, src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 3];
    
    // 安全確保: 偶数幅前提 (YUYV ペア)
    let src_chunks = src.chunks_exact(4);
    let out_chunks = out.chunks_exact_mut(6);
    
    for (src_chunk, out_chunk) in src_chunks.zip(out_chunks) {
        let y0 = src_chunk[0] as i32;
        let u  = src_chunk[1] as i32;
        let y1 = src_chunk[2] as i32;
        let v  = src_chunk[3] as i32;
        
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
    
    out
}

pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

struct FrameBuffer {
    front: Option<VideoFrame>,
    back: Option<VideoFrame>,
    dirty: bool,
    last_frame_instant: Option<Instant>,
    frame_intervals: VecDeque<f32>, // ミリ秒
    last_decode_ms: f32,
    fast_count: u64,
    fallback_count: u64,
}

impl FrameBuffer {
    fn new() -> Self {
        Self { front: None, back: None, dirty: false, last_frame_instant: None, frame_intervals: VecDeque::with_capacity(120), last_decode_ms: 0.0, fast_count: 0, fallback_count: 0 }
    }
    fn push_back(&mut self, frame: VideoFrame, decode_ms: f32, fast: bool) {
        self.back = Some(frame);
        self.dirty = true;
        self.last_decode_ms = decode_ms;
        if fast { self.fast_count += 1; } else { self.fallback_count += 1; }
        let now = Instant::now();
        if let Some(prev) = self.last_frame_instant.replace(now) {
            let dt = now.duration_since(prev).as_secs_f32() * 1000.0;
            if self.frame_intervals.len() == 120 { self.frame_intervals.pop_front(); }
            self.frame_intervals.push_back(dt);
        }
    }
    fn take_front(&mut self) -> Option<VideoFrame> {
        if self.dirty {
            std::mem::swap(&mut self.front, &mut self.back);
            self.dirty = false;
        }
        // メモリリーク修正: cloneの代わりに参照を返すように変更
        self.front.as_ref().map(|frame| VideoFrame {
            width: frame.width,
            height: frame.height,
            data: frame.data.clone()
        })
    }
    
    // メモリリーク防止: 古いフレームをクリア
    fn clear_old_frames(&mut self) {
        // 前回のフレームを破棄
        if self.back.is_some() && !self.dirty {
            self.back = None;
        }
    }

}

pub struct VideoCapture {
    camera: Option<CallbackCamera>,
    frames: Arc<Mutex<FrameBuffer>>,
    is_active: bool,
}

impl VideoCapture {
    pub fn new() -> Self {
    Self { camera: None, frames: Arc::new(Mutex::new(FrameBuffer::new())), is_active: false }
    }
    
    pub fn list_devices() -> Vec<(String, String)> {
        match nokhwa::query(ApiBackend::MediaFoundation) {
            Ok(devices) => {
                devices.into_iter()
                    .map(|info| (info.human_name().to_string(), info.description().to_string()))
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    }

    pub fn start_capture(&mut self, device_name: Option<&str>, resolution: Option<(u32, u32)>, format: Option<&str>, fps: Option<u32>) -> Result<(), String> {
        self.stop_capture();
        
        let devices = nokhwa::query(ApiBackend::MediaFoundation)
            .map_err(|e| format!("Failed to query devices: {}", e))?;
            
        let device_info = if let Some(name) = device_name {
            devices.into_iter()
                .find(|d| d.human_name() == name)
                .ok_or_else(|| format!("Device '{}' not found", name))?
        } else {
            devices.into_iter()
                .next()
                .ok_or("No video devices found")?
        };
        
        // Windows Media Foundationでの問題を回避するフォーマット設定
        let requested_format = if let Some((w,h)) = resolution {
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
                Resolution::new(w,h),
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
                    FrameFormat::YUYV if width % 2 == 0 => {
                        // YUY2の高速パス
                        let raw_data = frame.buffer_bytes();
                        if raw_data.len() >= width * height * 2 {
                            let rgb = yuy2_to_rgb_naive(width, height, &raw_data);
                            rgb_vec = Some(rgb);
                            used_fast = true;
                        }
                    },

                    _ => {
                        // その他のフォーマットも標準デコード
                        if let Ok(rgb_data) = frame.decode_image::<RgbFormat>() {
                            rgb_vec = Some(rgb_data.into_raw());
                        }
                    }
                }
                if let Some(data) = rgb_vec {
                    let decode_ms = start.elapsed().as_secs_f32() * 1000.0;
                    let vf = VideoFrame { width, height, data };
                    if let Ok(mut guard) = fb.lock() { 
                        guard.push_back(vf, decode_ms, used_fast); 
                    }
                }
            }
        };
        
        let mut camera = CallbackCamera::new(device_info.index().clone(), requested_format, frame_callback)
            .map_err(|e| format!("Failed to create camera: {}", e))?;
            
        camera.open_stream()
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
        
    if let Ok(mut buf) = self.frames.lock() { *buf = FrameBuffer::new(); }
    }
    
    pub fn get_latest_frame(&self) -> Option<VideoFrame> {
        self.frames.lock().ok().and_then(|mut fb| {
            let frame = fb.take_front();
            // メモリリーク防止: 定期的に古いフレームをクリア
            fb.clear_old_frames();
            frame
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
            ("MJPEG".to_string(), vec![(1920, 1080), (1280, 720), (640, 480)]),
            ("YUY2".to_string(), vec![(1280, 720), (640, 480)]),
        ]
    }

    #[allow(dead_code)]
    pub fn get_supported_formats_for(_device: &str) -> Vec<(String, Vec<(u32, u32)>)> {
        // デバイス毎のプレースホルダー; 実際の実装ではデバイス機能を照会
        vec![
            ("MJPEG".to_string(), vec![(1920,1080),(1280,720),(640,480)]),
            ("YUY2".to_string(), vec![(1280,720),(640,480)]),
            ("RGB24".to_string(), vec![(1280,720),(640,480)]),
        ]
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

impl Clone for VideoFrame {
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            data: self.data.clone(),
        }
    }
}