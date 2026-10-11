//! YUY2 → RGB を GPU で行う変換器の wgpu の部分（#492）。シェーダー（WGSL）の組み立て、
//! YUY2 のアップロード、egui のテクスチャへの書き込み、起動時の自己診断。
//! GL の版は `super::gpu_yuy2_gl`。どちらを使うかは起動時の描画バックエンドで決まる
//! （`crate::renderer`、窓口は `super::gpu_yuy2`）。
//!
//! **UI スレッドだけが触る。** 作るのはアプリの起動時（`eframe` の `CreationContext`）、
//! 使うのは egui の描画の前段のコールバック（`egui_wgpu::CallbackTrait::prepare`）。
//! どちらも UI スレッド。
//!
//! egui-wgpu のテクスチャ（`Rgba8Unorm`）は描画先にできない（`RENDER_ATTACHMENT` が無い）。
//! そこで映像と同じ大きさの自前のテクスチャへ描き、egui のテクスチャへ丸ごと写す。
//! 画面へはそのテクスチャを egui が普通に描くので、拡大縮小のフィルタも GL の版・CPU の
//! 変換と同じになる。中身も CPU の変換と 1 画素も違わない（`self_test` が起動時に確かめる）。
//!
//! wgpu は捕まえていない検証エラーを panic にする（release は `panic = "abort"`）。
//! 自前の資源づくり・描画・送信はすべてエラースコープ（`with_error_scope`）の中で行い、
//! 失敗は `GpuFailure::Wgpu` で返して CPU へ戻す。

use eframe::egui_wgpu::wgpu;
use log::warn;
use std::num::NonZeroU64;
use std::sync::mpsc;

use super::color::ColorMatrix;
use super::convert::yuy2_to_rgb_naive;
use super::frame_buffer::VideoFrame;
use super::gpu_yuy2_gl::{
    count_mismatches, self_test_matrices, self_test_source, shader_coefficients, GpuFailure,
    SELF_TEST_HEIGHT, SELF_TEST_WIDTH,
};

/// シェーダーへ渡す係数のバッファの大きさ（i32 を 6 つ。16 バイトの倍数に揃える）
const UNIFORM_SIZE: u64 = 32;

/// 係数をバッファへ書く形にする。並びは `shader_coefficients` と WGSL の `Coefficients`
fn uniform_bytes(matrix: &ColorMatrix) -> [u8; UNIFORM_SIZE as usize] {
    let mut bytes = [0u8; UNIFORM_SIZE as usize];
    let (slots, _) = bytes.as_chunks_mut::<4>();
    for (slot, value) in slots.iter_mut().zip(shader_coefficients(matrix)) {
        *slot = value.to_le_bytes();
    }
    bytes
}

/// 処理をエラースコープで囲み、検証・内部・メモリ不足のエラーを `GpuFailure::Wgpu` にする。
/// スコープはスレッドごとなので、同じスレッド（UI スレッド）で呼ぶこと
fn with_error_scope<T>(device: &wgpu::Device, f: impl FnOnce() -> T) -> Result<T, GpuFailure> {
    let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let value = f();
    // 積んだのと逆の順に外す。ネイティブ（wgpu-core）では外した時点で結果が決まっている
    let errors = [
        pollster::block_on(validation.pop()),
        pollster::block_on(internal.pop()),
        pollster::block_on(out_of_memory.pop()),
    ];
    match errors.into_iter().flatten().next() {
        Some(error) => Err(GpuFailure::Wgpu(error.to_string().trim().to_string())),
        None => Ok(value),
    }
}

/// 映像の大きさごとに作り直す資源。
struct SizedResources {
    width: u32,
    height: u32,
    /// YUY2 を Rgba8Uint として上げる先（幅は画素数の半分）
    source: wgpu::Texture,
    /// 変換の描画先（映像と同じ大きさの Rgba8Unorm）。ここから egui のテクスチャへ写す
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// wgpu の資源一式。作れたら変換に使える。
pub(super) struct WgpuConverter {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    sized: Option<SizedResources>,
    max_texture_side: u32,
}

impl WgpuConverter {
    /// シェーダーをコンパイルして資源を作る。失敗したら理由
    pub(super) fn new(device: &wgpu::Device) -> Result<Self, GpuFailure> {
        with_error_scope(device, || Self::create(device))
    }

    fn create(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::include_wgsl!("shaders/yuy2_to_rgb.wgsl"));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuy2_to_rgb"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(UNIFORM_SIZE),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuy2_to_rgb"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuy2_to_rgb"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                // 混色しない（書き残しと半透明の混ざりを避ける。GL の版がブレンドを切るのと同じ）
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuy2_to_rgb coefficients"),
            size: UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            uniform,
            sized: None,
            max_texture_side: device.limits().max_texture_dimension_2d,
        }
    }

    /// 大きさに合う資源を返す。大きさが変わったときだけ作り直す
    fn sized(&mut self, device: &wgpu::Device, width: u32, height: u32) -> &SizedResources {
        let reuse = self
            .sized
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height);
        if !reuse {
            let texture = |label, size: (u32, u32), format, usage| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
            };
            let source = texture(
                "yuy2_to_rgb source",
                (width / 2, height),
                wgpu::TextureFormat::Rgba8Uint,
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            );
            let target = texture(
                "yuy2_to_rgb target",
                (width, height),
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            );
            let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("yuy2_to_rgb"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&source_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.uniform.as_entire_binding(),
                    },
                ],
            });
            self.sized = Some(SizedResources {
                width,
                height,
                target_view: target.create_view(&wgpu::TextureViewDescriptor::default()),
                source,
                target,
                bind_group,
            });
        }
        self.sized.as_ref().expect("直前で用意した")
    }

    /// YUY2（`幅 × 高さ × 2` バイト、幅は偶数）を上げ、自前の描画先へ RGB で描く
    /// コマンドを `encoder` へ積む。描画先のテクスチャを返す
    fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &[u8],
        (width, height): (u32, u32),
        matrix: &ColorMatrix,
        encoder: &mut wgpu::CommandEncoder,
    ) -> wgpu::Texture {
        queue.write_buffer(&self.uniform, 0, &uniform_bytes(matrix));
        let pipeline = self.pipeline.clone();
        let sized = self.sized(device, width, height);
        queue.write_texture(
            sized.source.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // 1 行は 4 バイトのテクセルの並びなので詰め物は無い
                bytes_per_row: Some(width * 2),
                rows_per_image: Some(height),
            },
            sized.source.size(),
        );
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("yuy2_to_rgb"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &sized.target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &sized.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        sized.target.clone()
    }

    /// YUY2 のフレームを `destination`（egui の映像のテクスチャ、`Rgba8Unorm`）へ RGB にして
    /// 書くコマンドを作って返す。egui はこれを自分の描画より先に送る。
    /// `destination` の大きさがフレームと違えば何もしない（`None`。次の描画でやり直す）
    pub(super) fn convert(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &VideoFrame,
        matrix: &ColorMatrix,
        destination: &wgpu::Texture,
    ) -> Result<Option<wgpu::CommandBuffer>, GpuFailure> {
        let max = self.max_texture_side as usize;
        let too_large = GpuFailure::TooLarge {
            width: frame.width,
            height: frame.height,
            max,
        };
        if frame.width > max || frame.height > max {
            return Err(too_large);
        }
        let (Ok(width), Ok(height)) = (u32::try_from(frame.width), u32::try_from(frame.height))
        else {
            return Err(too_large);
        };
        if !frame.has_exact_len() {
            return Err(GpuFailure::LengthMismatch {
                width: frame.width,
                height: frame.height,
                len: frame.data.len(),
            });
        }
        if (destination.width(), destination.height()) != (width, height)
            || destination.format() != wgpu::TextureFormat::Rgba8Unorm
        {
            return Ok(None);
        }
        with_error_scope(device, || {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("yuy2_to_rgb"),
            });
            let target = self.encode(
                device,
                queue,
                &frame.data,
                (width, height),
                matrix,
                &mut encoder,
            );
            encoder.copy_texture_to_texture(
                target.as_image_copy(),
                destination.as_image_copy(),
                target.size(),
            );
            encoder.finish()
        })
        .map(Some)
    }

    /// 起動時の自己診断。`self_test_source` を係数表ごとに GPU で変換して読み戻し、
    /// CPU の変換と 1 画素でも違えば理由を返す。GL の版（`GlConverter::self_test`）と
    /// 同じ入力・同じ係数表
    pub(super) fn self_test(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), GpuFailure> {
        let source = self_test_source();
        let size = (SELF_TEST_WIDTH as u32, SELF_TEST_HEIGHT as u32);
        // 1 行 2048 バイトで、読み戻しの行の揃え（256 バイト）を満たす
        let bytes_per_row = size.0 * 4;
        let readback = with_error_scope(device, || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("yuy2_to_rgb self-test"),
                size: u64::from(bytes_per_row * size.1),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        })?;
        let mut cpu = Vec::new();
        for matrix in self_test_matrices() {
            with_error_scope(device, || {
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("yuy2_to_rgb self-test"),
                });
                let target = self.encode(device, queue, &source, size, &matrix, &mut encoder);
                encoder.copy_texture_to_buffer(
                    target.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &readback,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(bytes_per_row),
                            rows_per_image: None,
                        },
                    },
                    target.size(),
                );
                queue.submit([encoder.finish()]);
            })?;
            let gpu = read_back(device, &readback)?;
            yuy2_to_rgb_naive(
                SELF_TEST_WIDTH,
                SELF_TEST_HEIGHT,
                &source,
                &matrix,
                &mut cpu,
            );
            let mismatches = count_mismatches(&gpu, &cpu);
            if mismatches > 0 {
                warn!(
                    "自己診断で CPU の変換と {} 画素が食い違った（wgpu、{}）",
                    mismatches, matrix.name
                );
                return Err(GpuFailure::SelfTestMismatch(mismatches));
            }
        }
        Ok(())
    }
}

/// 読み戻し用のバッファを CPU から読めるようにして、中身を写して返す。GPU の完了まで待つ。
/// 起動時の自己診断（とテスト）でだけ使うので待ちは許容する。マップ・待ち・読み取りも
/// ほかの wgpu の呼び出しと同じくエラースコープの中で行う
fn read_back(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Result<Vec<u8>, GpuFailure> {
    let wgpu_error = |e: &dyn std::fmt::Display| GpuFailure::Wgpu(e.to_string());
    with_error_scope(device, || {
        let slice = buffer.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| wgpu_error(&e))?;
        receiver
            .recv()
            .map_err(|e| wgpu_error(&e))?
            .map_err(|e| wgpu_error(&e))?;
        let data = slice
            .get_mapped_range()
            .map_err(|e| wgpu_error(&e))?
            .to_vec();
        buffer.unmap();
        Ok(data)
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::color::{BT601, BT709};
    use crate::video::frame_format::PixelFormat;

    #[test]
    fn uniform_bytes_follow_the_shader_order_and_pad_with_zero() {
        // WGSL の `Coefficients` は y, bias, r_v, g_u, g_v, b_u の順の i32（リトルエンディアン）
        let bytes = uniform_bytes(&BT601);
        let values: Vec<i32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes(*b))
            .collect();
        assert_eq!(values, [1192, 1192 * 16, 1634, 401, 833, 2066, 0, 0]);
    }

    /// テスト用に DX12 のデバイスを作る。GPU が無ければ WARP（CPU で描く DX12）を使う。
    /// Windows 10/11 と GitHub Actions の windows-latest には WARP が入っている
    fn test_device() -> (wgpu::Device, wgpu::Queue, String) {
        let instance = wgpu::Instance::new(crate::renderer::dx12_instance_descriptor());
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
        let names: Vec<_> = adapters.iter().map(|a| a.get_info().name).collect();
        for adapter in adapters {
            if let Ok((device, queue)) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            {
                return (device, queue, adapter.get_info().name);
            }
        }
        panic!("DX12 のデバイスを作れない（列挙できたもの: {names:?}）");
    }

    #[test]
    fn self_test_matches_the_cpu_conversion_on_dx12() {
        // WGSL を naga → HLSL → FXC で通し、自己診断（全画素が CPU と一致）が通ることを
        // CI（GPU なし、WARP）でも確かめる。本番の起動時と同じ関数
        let (device, queue, name) = test_device();
        let mut converter = WgpuConverter::new(&device).expect("シェーダーを用意できる");
        converter
            .self_test(&device, &queue)
            .unwrap_or_else(|e| panic!("{name} で自己診断が通らない: {e:?}"));
    }

    #[test]
    fn convert_writes_the_same_pixels_as_the_cpu_into_the_destination_texture() {
        // 自己診断が見るのは中間の描画先まで。ここでは egui のテクスチャへ写したあとの
        // 最終の書き込み先を読み戻し、CPU の変換と全画素が一致することを確かめる
        let (device, queue, _) = test_device();
        let mut converter = WgpuConverter::new(&device).expect("シェーダーを用意できる");
        // 1 行 64 画素 × 4 バイト = 256 バイトで、読み戻しの行の揃えを満たす
        let (width, height) = (64u32, 4u32);
        let destination = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            // egui-wgpu の映像のテクスチャと同じ用途に、読み戻すための COPY_SRC だけ足す
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Y・Cb・Cr がばらける並び（飽和する値も入る）
        let data: Vec<u8> = (0..width * height * 2)
            .map(|i| ((i * 37 + i / 7) % 256) as u8)
            .collect();
        let frame = VideoFrame {
            width: width as usize,
            height: height as usize,
            data,
            format: PixelFormat::Yuy2(BT601),
        };
        let command = converter
            .convert(&device, &queue, &frame, &BT601, &destination)
            .expect("変換できる")
            .expect("同じ大きさなので書く");
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(width * 4 * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            destination.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: None,
                },
            },
            destination.size(),
        );
        // egui と同じく、変換のコマンドを先に送る
        queue.submit([command, encoder.finish()]);
        let gpu = read_back(&device, &readback).expect("読み戻せる");
        let mut cpu = Vec::new();
        yuy2_to_rgb_naive(frame.width, frame.height, &frame.data, &BT601, &mut cpu);
        assert_eq!(gpu.len(), frame.width * frame.height * 4);
        assert_eq!(count_mismatches(&gpu, &cpu), 0);
    }

    #[test]
    fn convert_writes_the_frame_into_a_texture_of_the_same_size_only() {
        let (device, queue, _) = test_device();
        let mut converter = WgpuConverter::new(&device).expect("シェーダーを用意できる");
        let destination = |width, height| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                // egui-wgpu の映像のテクスチャと同じ用途
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        // 4x2 の YUY2。白（Y=235）と黒（Y=16）、色差なし
        let frame = VideoFrame {
            width: 4,
            height: 2,
            data: [235, 128, 16, 128].repeat(4),
            format: PixelFormat::Yuy2(BT709),
        };
        let written = converter
            .convert(&device, &queue, &frame, &BT709, &destination(4, 2))
            .expect("変換できる");
        assert!(written.is_some());
        // 大きさの違うテクスチャ（作り直しの前のもの）には書かない
        let skipped = converter
            .convert(&device, &queue, &frame, &BT709, &destination(8, 2))
            .expect("失敗にはしない");
        assert!(skipped.is_none());
        // 長さが合わないフレームは失敗
        let short = VideoFrame {
            data: vec![0; 3],
            ..frame
        };
        assert!(matches!(
            converter.convert(&device, &queue, &short, &BT709, &destination(4, 2)),
            Err(GpuFailure::LengthMismatch { .. })
        ));
    }
}
