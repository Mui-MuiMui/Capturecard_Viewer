//! フィルターグラフの組み立て・開始・停止・破棄。
//!
//! **すべてデバイスワーカースレッドの上で行う**（`DirectShowCapture` を
//! 触るのがワーカーだけのため）。グラフが動き出すと、上流のフィルターが
//! 自分のストリーミングスレッドから `filter::Renderer` の入力ピンへ
//! サンプルを渡してくる。
//!
//! 組み立ての流れは次のとおり。
//!
//! 1. 名札からキャプチャーのフィルターを作る（ここでデバイスを掴む）
//! 2. `IGraphBuilder` と `ICaptureGraphBuilder2` を作り、フィルターを入れる
//! 3. キャプチャーピンの `IAMStreamConfig` に、選んだ形式を `SetFormat` する
//! 4. 自前のレンダラーを入れ、`RenderStream` で繋ぐ（直接繋がらなければ
//!    DirectShow が間に変換フィルターを挟む）
//! 5. グラフの基準時計を外し（届いたサンプルを待たせずに渡させるため）、
//!    `IMediaControl::Run` で動かす

use std::ptr;
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2, IGraphBuilder, IMediaControl, IMediaFilter,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::MediaFoundation::{
    CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, MEDIATYPE_Video, AM_MEDIA_TYPE,
    PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use super::devices::{self, choose_candidate, DeviceEntry, StreamCandidate};
use super::filter::Renderer;
use super::media_type::{
    delete_media_type, interval_within_caps, set_avg_time_per_frame, SampleFormat,
};
use crate::video::elapsed_ms;
use crate::video::frame_sink::FrameSink;

/// 開く形式の要求。`VideoBackend::start_capture` の引数そのまま。
#[derive(Debug, Clone, Copy)]
pub(super) struct FormatRequest<'a> {
    pub(super) resolution: Option<(u32, u32)>,
    pub(super) format: Option<&'a str>,
    pub(super) fps: Option<u32>,
}

/// グラフのどこで失敗したか。`VideoError` のどちらの種類にするかを決める。
#[derive(Debug)]
pub(super) enum GraphError {
    /// デバイスを掴めない・グラフを作れない（`VideoError::CameraOpenFailed`）
    Open(windows::core::Error),
    /// 繋げない・動かせない（`VideoError::StreamOpenFailed`）
    Stream(windows::core::Error),
}

/// キャプチャー用のグラフ 1 本。**ワーカースレッドから出さない。**
pub(super) struct CaptureGraph {
    graph: IGraphBuilder,
    control: IMediaControl,
    source: IBaseFilter,
    renderer: Renderer,
    /// 上流と接続できた形式
    pub(super) format: SampleFormat,
    /// 要求した fps（`SetFormat` に使った値。使えなかったら接続した形式の値）
    pub(super) requested_fps: u32,
}

/// グラフとキャプチャー用の組み立て役を作る。
fn create_graph() -> windows::core::Result<(IGraphBuilder, ICaptureGraphBuilder2)> {
    let graph: IGraphBuilder =
        unsafe { CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)? };
    let builder: ICaptureGraphBuilder2 =
        unsafe { CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)? };
    unsafe { builder.SetFiltergraph(&graph)? };
    Ok((graph, builder))
}

/// デバイスの対応形式を読む。能力の問い合わせ（`DirectShowCapture::capabilities`）用。
///
/// フィルターをグラフに入れてから `IAMStreamConfig` を探す。グラフに
/// 入っていないと `FindInterface` が答えないフィルターがあるため。
pub(super) fn query_candidates(
    entry: &DeviceEntry,
) -> Result<Option<Vec<StreamCandidate>>, GraphError> {
    let source = devices::bind_filter(entry).map_err(GraphError::Open)?;
    let (graph, builder) = create_graph().map_err(GraphError::Open)?;
    unsafe { graph.AddFilter(&source, windows::core::w!("Capture Source")) }
        .map_err(GraphError::Open)?;
    let candidates =
        devices::stream_config(&builder, &source).map(|config| devices::read_candidates(&config));
    // グラフから外してから手放す。外すとピンの接続も切れる
    let _ = unsafe { graph.RemoveFilter(&source) };
    Ok(candidates)
}

/// 選んだ対応形式を `SetFormat` する。成功したら要求した fps を返す。
///
/// 失敗してもグラフは組める（フィルターの既定の形式で流れる）ので、
/// ここでは記録するだけで止めない。
fn apply_format(config: &IAMStreamConfig, request: FormatRequest<'_>) -> Option<u32> {
    let candidates = devices::read_candidates(config);
    let Some((chosen, fps)) =
        choose_candidate(&candidates, request.resolution, request.format, request.fps)
    else {
        log::warn!(
            "DirectShow のデバイスに受け取れる形式が無いので、フィルターの既定の形式で開く（{} 件の対応形式を読めた）",
            candidates.len()
        );
        return None;
    };
    let candidate = &candidates[chosen];
    if let Some(requested) = request.format.filter(|f| !f.is_empty()) {
        if requested != candidate.format.kind.name() {
            log::warn!(
                "ビデオフォーマット {} はこのデバイスに無いので {} で開く",
                requested,
                candidate.format.kind.name()
            );
        }
    }

    let mut pmt: *mut AM_MEDIA_TYPE = ptr::null_mut();
    let mut caps = windows::Win32::Media::DirectShow::VIDEO_STREAM_CONFIG_CAPS::default();
    let read =
        unsafe { config.GetStreamCaps(candidate.index, &mut pmt, &mut caps as *mut _ as *mut u8) };
    if read.is_err() || pmt.is_null() {
        log::warn!("DirectShow の対応形式を読み直せないので、フィルターの既定の形式で開く");
        return None;
    }
    let interval = interval_within_caps(fps, caps.MinFrameInterval, caps.MaxFrameInterval);
    unsafe { set_avg_time_per_frame(&mut *pmt, interval) };
    let set = unsafe { config.SetFormat(pmt) };
    unsafe { delete_media_type(pmt) };
    match set {
        Ok(()) => {
            log::debug!(
                "DirectShow の形式を {} {}x{} {}fps にした",
                candidate.format.kind.name(),
                candidate.format.width,
                candidate.format.height,
                fps
            );
            Some(fps)
        }
        Err(e) => {
            log::warn!(
                "DirectShow の形式を設定できないので、フィルターの既定の形式で開く: {}",
                e
            );
            None
        }
    }
}

impl CaptureGraph {
    /// グラフを組んで動かす。
    pub(super) fn start(
        entry: &DeviceEntry,
        request: FormatRequest<'_>,
        sink: FrameSink,
    ) -> Result<Self, GraphError> {
        let bind_start = Instant::now();
        let source = devices::bind_filter(entry).map_err(GraphError::Open)?;
        let (graph, builder) = create_graph().map_err(GraphError::Open)?;
        unsafe { graph.AddFilter(&source, windows::core::w!("Capture Source")) }
            .map_err(GraphError::Open)?;
        let bind_ms = elapsed_ms(bind_start);

        let requested_fps = devices::stream_config(&builder, &source)
            .and_then(|config| apply_format(&config, request));
        if requested_fps.is_none() {
            log::debug!(
                "DirectShow のデバイスは IAMStreamConfig を持たないか、形式を設定できなかった"
            );
        }

        let connect_start = Instant::now();
        let renderer = Renderer::new(sink);
        let built = Self::connect(&graph, &builder, &source, &renderer);
        if let Err(e) = built {
            Self::tear_down(&graph, &source, &renderer.filter);
            return Err(GraphError::Stream(e));
        }
        let Some(format) = renderer.connected_format() else {
            Self::tear_down(&graph, &source, &renderer.filter);
            return Err(GraphError::Stream(windows::core::Error::from_hresult(
                windows::Win32::Media::DirectShow::VFW_E_NOT_CONNECTED,
            )));
        };
        let connect_ms = elapsed_ms(connect_start);

        let run_start = Instant::now();
        let control: IMediaControl = match graph.cast() {
            Ok(control) => control,
            Err(e) => {
                Self::tear_down(&graph, &source, &renderer.filter);
                return Err(GraphError::Stream(e));
            }
        };
        // Run は状態の遷移が済む前に S_FALSE で戻ることがある。失敗だけを見る
        if let Err(e) = unsafe { control.Run() } {
            let _ = unsafe { control.Stop() };
            Self::tear_down(&graph, &source, &renderer.filter);
            return Err(GraphError::Stream(e));
        }
        log::debug!(
            "DirectShow のグラフを動かした（デバイスを開く {:.1}ms、接続 {:.1}ms、Run {:.1}ms）",
            bind_ms,
            connect_ms,
            elapsed_ms(run_start)
        );

        let requested_fps = requested_fps
            .or_else(|| super::media_type::fps_from_interval(format.avg_time_per_frame))
            .unwrap_or(0);
        Ok(Self {
            graph,
            control,
            source,
            renderer,
            format,
            requested_fps,
        })
    }

    /// キャプチャーピンとレンダラーを繋ぎ、基準時計を外す。
    fn connect(
        graph: &IGraphBuilder,
        builder: &ICaptureGraphBuilder2,
        source: &IBaseFilter,
        renderer: &Renderer,
    ) -> windows::core::Result<()> {
        unsafe {
            graph.AddFilter(
                &renderer.filter,
                windows::core::w!("Capturecard_Viewer Renderer"),
            )?
        };
        // キャプチャーのカテゴリで繋がらなければ、カテゴリを問わず映像のピンで
        // 繋ぐ。仮想カメラにはピンのカテゴリを名乗らないものがある
        let with_category = unsafe {
            builder.RenderStream(
                Some(&PIN_CATEGORY_CAPTURE),
                &MEDIATYPE_Video,
                source,
                None::<&IBaseFilter>,
                &renderer.filter,
            )
        };
        if let Err(e) = with_category {
            log::debug!(
                "キャプチャーのカテゴリで繋げなかったので、カテゴリを問わず繋ぐ: {}",
                e
            );
            unsafe {
                builder.RenderStream(
                    None,
                    &MEDIATYPE_Video,
                    source,
                    None::<&IBaseFilter>,
                    &renderer.filter,
                )?
            };
        }

        // 基準時計を外す。付けたままだとレンダラー以外のフィルター（間に入った
        // 変換フィルターなど）がタイムスタンプまで待ってから渡してくることがあり、
        // その分だけ遅れる。キャプチャーは届いた順にすぐ出せばよい
        let filter: IMediaFilter = graph.cast()?;
        unsafe { filter.SetSyncSource(None::<&IReferenceClock>)? };
        Ok(())
    }

    /// 止めずに（止めた後に）フィルターをグラフから外す。外すとピンの接続が切れ、
    /// フィルター・ピン・グラフの間の参照の循環がほどける。
    fn tear_down(graph: &IGraphBuilder, source: &IBaseFilter, renderer: &IBaseFilter) {
        let _ = unsafe { graph.RemoveFilter(renderer) };
        let _ = unsafe { graph.RemoveFilter(source) };
    }

    /// 開いている形式の名前（`ActiveVideo::format` に入れる）。
    pub(super) fn format_name(&self) -> &'static str {
        self.format.kind.name()
    }
}

impl Drop for CaptureGraph {
    fn drop(&mut self) {
        let stop_start = Instant::now();
        // Stop は上流のストリーミングスレッドが止まるまで待つ。これ以降
        // レンダラーへサンプルは届かない
        match unsafe { self.control.Stop() } {
            Ok(()) => log::info!(
                "DirectShow のグラフを止めた（{:.1}ms）",
                elapsed_ms(stop_start)
            ),
            Err(e) => log::warn!(
                "DirectShow のグラフを止められなかった（{:.1}ms）: {}",
                elapsed_ms(stop_start),
                e
            ),
        }
        Self::tear_down(&self.graph, &self.source, &self.renderer.filter);
    }
}
