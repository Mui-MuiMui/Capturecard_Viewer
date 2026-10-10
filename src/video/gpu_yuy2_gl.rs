//! YUY2 → RGB を GPU で行う変換器の GL の部分（#456）。シェーダーの組み立て、YUY2 の
//! アップロード、egui のテクスチャへの描き込み、起動時の自己診断。
//!
//! **UI スレッドだけが触る。** 作るのはアプリの起動時（`eframe` の `CreationContext`、
//! GL のコンテキストが current）、使うのは egui の描画中のコールバック
//! （`egui_glow::CallbackFn`）。どちらも UI スレッド。
//!
//! 変換の結果は**映像と同じ大きさの egui のテクスチャ**へフレームバッファ経由で書く。
//! 画面へはそのテクスチャを egui が普通に描くので、拡大縮小のフィルタ（拡大は Nearest、
//! 縮小は Linear）も CPU で変換していたころと同じになる。テクスチャの中身も
//! CPU の変換と 1 画素も違わない（`self_test` が起動時に確かめる）。

use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext as _};
use log::warn;
use std::fmt;

use super::color::{
    adjusted_color_matrix, ColorMatrix, VideoAdjustments, BT601, BT601_FULL, BT709, BT709_FULL,
};
use super::convert::yuy2_to_rgb_naive;
use super::frame_buffer::VideoFrame;
use crate::i18n;

/// GPU で変換できない理由。ログと「接続状態」タブ・統計 OSD に出す。
///
/// **文言は `Display` から `crate::i18n` を引く**（`docs/design/error-reporting.md`）。
/// 持たせる値は GL が返した文（シェーダーのログなど）と数値だけで、言語に依らない
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuFailure {
    /// GLSL 1.40 / ES 3.00 に満たない。egui_glow が判定した版
    ShaderVersion(String),
    /// GL の資源（プログラム・シェーダー・頂点配列・テクスチャ・フレームバッファ）を作れない
    Resources(String),
    /// シェーダーをコンパイルできない。GL のログ
    Compile(String),
    /// シェーダーをリンクできない。GL のログ
    Link(String),
    /// 起動時の自己診断で CPU の変換と食い違った画素の数
    SelfTestMismatch(usize),
    /// 描画先のフレームバッファが不完全。`glCheckFramebufferStatus` の値
    FramebufferIncomplete(u32),
    /// 描画で GL のエラーが出た。`glGetError` の値
    GlError(u32),
    /// テクスチャの上限を超える大きさ
    TooLarge {
        width: usize,
        height: usize,
        max: usize,
    },
    /// 画素データの長さが YUY2 の `幅 × 高さ × 2` と合わない
    LengthMismatch {
        width: usize,
        height: usize,
        len: usize,
    },
}

impl fmt::Display for GpuFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&i18n::gpu_failure(self))
    }
}

const VERTEX_SOURCE: &str = include_str!("shaders/yuy2_to_rgb.vert");
const FRAGMENT_SOURCE: &str = include_str!("shaders/yuy2_to_rgb.frag");

/// シェーダーへ渡す係数。`[u_y, u_bias, u_r_v, u_g_u, u_g_v, u_b_u]` の順。
///
/// `bias` の畳み方は `convert::yuy2_to_rgb_naive` と同じ（`cy * Y - bias`）。
pub(super) fn shader_coefficients(matrix: &ColorMatrix) -> [i32; 6] {
    [
        matrix.y,
        matrix.y * matrix.y_offset - matrix.offset,
        matrix.r_v,
        matrix.g_u,
        matrix.g_v,
        matrix.b_u,
    ]
}

/// シェーダーの uniform の名前。`shader_coefficients` の順
const COEFFICIENT_UNIFORMS: [&str; 6] = ["u_y", "u_bias", "u_r_v", "u_g_u", "u_g_v", "u_b_u"];

/// GL の版から、シェーダーの先頭に置く宣言を決める。整数の演算・`texelFetch`・
/// `gl_VertexID` が要るので GLSL 1.40 か GLSL ES 3.00 以上に限る。それ未満なら `None`
pub(super) fn version_declaration(version: ShaderVersion) -> Option<&'static str> {
    match version {
        ShaderVersion::Gl140 | ShaderVersion::Es300 => Some(version.version_declaration()),
        ShaderVersion::Gl120 | ShaderVersion::Es100 => None,
    }
}

/// 自己診断の入力の大きさ。1 テクセル = 2 画素で、横 256 テクセル × 縦 256 行に
/// Y と Cb の全ての組を並べる
pub(super) const SELF_TEST_WIDTH: usize = 512;
pub(super) const SELF_TEST_HEIGHT: usize = 256;

/// 自己診断の入力（YUY2）。テクセル (i, j) が `Y0 = i, Cb = j, Y1 = 255 - i,
/// Cr = (31i + 17j) mod 256` を持つ。Y と Cb は全ての値、Cr も全ての値が出る
pub(super) fn self_test_source() -> Vec<u8> {
    let mut data = Vec::with_capacity(SELF_TEST_WIDTH * SELF_TEST_HEIGHT * 2);
    for j in 0..SELF_TEST_HEIGHT {
        for i in 0..SELF_TEST_WIDTH / 2 {
            data.extend_from_slice(&[
                i as u8,
                j as u8,
                (255 - i) as u8,
                ((31 * i + 17 * j) % 256) as u8,
            ]);
        }
    }
    data
}

/// 自己診断で試す係数表。素の 4 つと、映像調整で係数と定数が大きく動く 2 つ
/// （0 未満・255 超への飽和、色差の係数の倍増を通す）
pub(super) fn self_test_matrices() -> Vec<ColorMatrix> {
    vec![
        BT601,
        BT709,
        BT601_FULL,
        BT709_FULL,
        adjusted_color_matrix(&BT709, VideoAdjustments::new(37, -80, 100)),
        adjusted_color_matrix(&BT601_FULL, VideoAdjustments::new(-100, 100, -100)),
    ]
}

/// GPU が書いた RGBA と CPU の RGB を比べ、食い違う画素の数を返す（アルファは見ない）
pub(super) fn count_mismatches(rgba: &[u8], rgb: &[u8]) -> usize {
    let (gpu, _) = rgba.as_chunks::<4>();
    let (cpu, _) = rgb.as_chunks::<3>();
    let compared = gpu.len().min(cpu.len());
    let differing = gpu.iter().zip(cpu).filter(|(g, c)| g[..3] != c[..]).count();
    differing + gpu.len().max(cpu.len()) - compared
}

/// GL の資源一式。作れたら変換に使える。
pub(super) struct GlConverter {
    program: glow::Program,
    vertex_array: glow::VertexArray,
    /// YUY2 を RGBA8 として上げる先（幅は画素数の半分）
    source: glow::Texture,
    /// `source` をいまの大きさで確保済みなら、その大きさ（テクセル）
    source_size: Option<(i32, i32)>,
    framebuffer: glow::Framebuffer,
    sampler: Option<glow::UniformLocation>,
    coefficients: [Option<glow::UniformLocation>; 6],
}

impl GlConverter {
    /// シェーダーをコンパイルして資源を作る。失敗したら理由（ログと「接続状態」タブに出す）。
    ///
    /// # Safety
    /// `gl` のコンテキストが current であること（UI スレッドの起動時か描画中）
    pub(super) unsafe fn new(gl: &glow::Context) -> Result<Self, GpuFailure> {
        let version = ShaderVersion::get(gl);
        let declaration = version_declaration(version)
            .ok_or_else(|| GpuFailure::ShaderVersion(format!("{version:?}")))?;
        unsafe {
            let program = link_program(gl, declaration, version.is_embedded())?;
            let resources = (
                gl.create_vertex_array(),
                gl.create_texture(),
                gl.create_framebuffer(),
            );
            let (Ok(vertex_array), Ok(source), Ok(framebuffer)) = resources else {
                gl.delete_program(program);
                return Err(GpuFailure::Resources(
                    "vertex array / texture / framebuffer".to_string(),
                ));
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(source));
            for (parameter, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let coefficients =
                COEFFICIENT_UNIFORMS.map(|name| gl.get_uniform_location(program, name));
            Ok(Self {
                program,
                vertex_array,
                source,
                source_size: None,
                framebuffer,
                sampler: gl.get_uniform_location(program, "u_yuy2"),
                coefficients,
            })
        }
    }

    /// YUY2 のフレームを `target`（映像と同じ大きさの RGBA8 のテクスチャ）へ RGB にして書く。
    /// 書いたあとは描画先を `restore` へ戻す。書けなければ理由。
    ///
    /// **egui の描画のコールバックから呼ぶ。** egui はコールバックのあとで自分の状態
    /// （ビューポート・シザー・ブレンド・プログラム・頂点配列）を設定し直す。
    ///
    /// # Safety
    /// `gl` のコンテキストが current であること
    pub(super) unsafe fn convert(
        &mut self,
        gl: &glow::Context,
        frame: &VideoFrame,
        matrix: &ColorMatrix,
        target: glow::Texture,
        restore: Option<glow::Framebuffer>,
        max_texture_side: usize,
    ) -> Result<(), GpuFailure> {
        let too_large = GpuFailure::TooLarge {
            width: frame.width,
            height: frame.height,
            max: max_texture_side,
        };
        if frame.width > max_texture_side || frame.height > max_texture_side {
            // GL のテクスチャの上限を超える。映像のテクスチャ自体を作れないので変換もしない
            return Err(too_large);
        }
        let (Ok(width), Ok(height)) = (i32::try_from(frame.width), i32::try_from(frame.height))
        else {
            return Err(too_large);
        };
        if !frame.has_exact_len() {
            // 上げるときに GL が `幅 / 2 × 高さ × 4` バイトを読むので、足りないものは渡さない
            return Err(GpuFailure::LengthMismatch {
                width: frame.width,
                height: frame.height,
                len: frame.data.len(),
            });
        }
        unsafe {
            // egui や他の描画が残したエラーを、この変換の失敗と取り違えない
            clear_gl_errors(gl);
            self.upload(gl, width / 2, height, &frame.data);
            let result = self.draw(gl, target, width, height, matrix);
            gl.bind_framebuffer(glow::FRAMEBUFFER, restore);
            result
        }
    }

    /// YUY2 を `source` へ上げる。大きさが変わったときだけ確保し直す
    unsafe fn upload(&mut self, gl: &glow::Context, texels: i32, rows: i32, data: &[u8]) {
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.source));
            // 1 行は 4 バイトのテクセルの並びなので詰め物は無い
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            let pixels = glow::PixelUnpackData::Slice(Some(data));
            if self.source_size == Some((texels, rows)) {
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    texels,
                    rows,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    pixels,
                );
            } else {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    texels,
                    rows,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    pixels,
                );
                self.source_size = Some((texels, rows));
            }
        }
    }

    /// `source` を読んで `target` へ描く。描画先は `framebuffer` のまま返す
    unsafe fn draw(
        &self,
        gl: &glow::Context,
        target: glow::Texture,
        width: i32,
        height: i32,
        matrix: &ColorMatrix,
    ) -> Result<(), GpuFailure> {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(target),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(GpuFailure::FramebufferIncomplete(status));
            }
            // egui はクリップのためにシザーを、半透明のためにブレンドを有効にしている。
            // テクスチャへの書き込みには両方とも要らない（書き残しと混色を避ける）
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.viewport(0, 0, width, height);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(self.sampler.as_ref(), 0);
            for (location, value) in self.coefficients.iter().zip(shader_coefficients(matrix)) {
                gl.uniform_1_i32(location.as_ref(), value);
            }
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.source));
            gl.bind_vertex_array(Some(self.vertex_array));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            match gl.get_error() {
                glow::NO_ERROR => Ok(()),
                error => Err(GpuFailure::GlError(error)),
            }
        }
    }

    /// 起動時の自己診断。`self_test_source` を係数表ごとに GPU で変換して読み戻し、
    /// CPU の変換と 1 画素でも違えば理由を返す。描画先は `restore` へ戻す。
    ///
    /// # Safety
    /// `gl` のコンテキストが current であること
    pub(super) unsafe fn self_test(
        &mut self,
        gl: &glow::Context,
        restore: Option<glow::Framebuffer>,
    ) -> Result<(), GpuFailure> {
        let source = self_test_source();
        let (width, height) = (SELF_TEST_WIDTH as i32, SELF_TEST_HEIGHT as i32);
        unsafe {
            let target = gl.create_texture().map_err(GpuFailure::Resources)?;
            gl.bind_texture(glow::TEXTURE_2D, Some(target));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            clear_gl_errors(gl);
            let mut gpu = vec![0u8; SELF_TEST_WIDTH * SELF_TEST_HEIGHT * 4];
            let mut cpu = Vec::new();
            let mut result = Ok(());
            for matrix in self_test_matrices() {
                self.upload(gl, width / 2, height, &source);
                if let Err(e) = self.draw(gl, target, width, height, &matrix) {
                    result = Err(e);
                    break;
                }
                gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
                gl.read_pixels(
                    0,
                    0,
                    width,
                    height,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut gpu)),
                );
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
                        "自己診断で CPU の変換と {} 画素が食い違った（{}）",
                        mismatches, matrix.name
                    );
                    result = Err(GpuFailure::SelfTestMismatch(mismatches));
                    break;
                }
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, restore);
            gl.delete_texture(target);
            result
        }
    }

    /// GL の資源を消す。アプリの終了時に呼ぶ
    ///
    /// # Safety
    /// `gl` のコンテキストが current であること
    pub(super) unsafe fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_program(self.program);
            gl.delete_vertex_array(self.vertex_array);
            gl.delete_texture(self.source);
            gl.delete_framebuffer(self.framebuffer);
        }
    }
}

/// 前に積まれていた GL のエラーを読み捨てる。`draw` の末尾の `get_error` が、呼ぶ前から
/// 残っていた別のエラー（egui や他の描画のもの）を拾い、変換の失敗と取り違えないようにする。
/// コンテキストを失うと同じエラーを返し続ける実装があるので、読む回数に上限を置く
unsafe fn clear_gl_errors(gl: &glow::Context) {
    const MAX_PENDING_ERRORS: usize = 16;
    for _ in 0..MAX_PENDING_ERRORS {
        // SAFETY: 呼び出し側が GL のコンテキストを current にしている
        if unsafe { gl.get_error() } == glow::NO_ERROR {
            break;
        }
    }
}

/// 頂点・フラグメントのシェーダーをコンパイルしてリンクする
unsafe fn link_program(
    gl: &glow::Context,
    declaration: &str,
    embedded: bool,
) -> Result<glow::Program, GpuFailure> {
    unsafe {
        let program = gl.create_program().map_err(GpuFailure::Resources)?;
        let mut shaders = Vec::new();
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX_SOURCE),
            (glow::FRAGMENT_SHADER, FRAGMENT_SOURCE),
        ] {
            let shader = gl.create_shader(kind).map_err(GpuFailure::Resources)?;
            gl.shader_source(shader, &format!("{declaration}{source}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                for shader in shaders {
                    gl.delete_shader(shader);
                }
                gl.delete_program(program);
                return Err(GpuFailure::Compile(log.trim().to_string()));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        if !embedded {
            // デスクトップの GLSL 1.40 は出力の場所を指定できないので、リンク前に 0 番へ結ぶ
            gl.bind_frag_data_location(program, 0, "out_color");
        }
        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        let log = gl.get_program_info_log(program);
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !linked {
            gl.delete_program(program);
            return Err(GpuFailure::Link(log.trim().to_string()));
        }
        Ok(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// シェーダーの 1 画素の計算を Rust で写したもの。`yuy2_to_rgb.frag` と同じ順・同じ
    /// 飽和のさせ方（先に 0〜262143 へ収めてから右シフト）にしてある
    fn shader_pixel(texel: [u8; 4], odd: bool, coefficients: [i32; 6]) -> [u8; 3] {
        let [cy, bias, r_v, g_u, g_v, b_u] = coefficients;
        let y = i32::from(if odd { texel[2] } else { texel[0] });
        let d = i32::from(texel[1]) - 128;
        let e = i32::from(texel[3]) - 128;
        let l = cy * y - bias;
        let channel = |v: i32| (v.clamp(0, 262_143) >> 10) as u8;
        [
            channel(l + r_v * e),
            channel(l - g_u * d - g_v * e),
            channel(l + b_u * d),
        ]
    }

    #[test]
    fn shader_arithmetic_matches_the_cpu_conversion_for_every_self_test_matrix() {
        // シェーダーの式（飽和させてからシフトする）が CPU の式（シフトしてから飽和）と
        // 全ての入力で一致することを、自己診断と同じ入力と係数表で確かめる
        let source = self_test_source();
        let (texels, _) = source.as_chunks::<4>();
        for matrix in self_test_matrices() {
            let mut cpu = Vec::new();
            yuy2_to_rgb_naive(
                SELF_TEST_WIDTH,
                SELF_TEST_HEIGHT,
                &source,
                &matrix,
                &mut cpu,
            );
            let coefficients = shader_coefficients(&matrix);
            let shader: Vec<u8> = texels
                .iter()
                .flat_map(|texel| {
                    let [r0, g0, b0] = shader_pixel(*texel, false, coefficients);
                    let [r1, g1, b1] = shader_pixel(*texel, true, coefficients);
                    [r0, g0, b0, r1, g1, b1]
                })
                .collect();
            assert_eq!(shader, cpu, "{} で食い違う", matrix.name);
        }
    }

    #[test]
    fn shader_coefficients_fold_the_offsets_like_the_cpu() {
        // BT.601 リミテッド: bias = 1192 * 16 - 0
        assert_eq!(
            shader_coefficients(&BT601),
            [1192, 1192 * 16, 1634, 401, 833, 2066]
        );
        // 明るさは offset に入り、bias から引かれる
        let brighter = adjusted_color_matrix(&BT601_FULL, VideoAdjustments::new(10, 0, 0));
        assert_eq!(shader_coefficients(&brighter)[1], -10 * 1024);
    }

    #[test]
    fn self_test_source_covers_every_luma_and_chroma_value() {
        let source = self_test_source();
        assert_eq!(source.len(), SELF_TEST_WIDTH * SELF_TEST_HEIGHT * 2);
        let (texels, _) = source.as_chunks::<4>();
        for channel in 0..4 {
            let mut seen = [false; 256];
            for texel in texels {
                seen[usize::from(texel[channel])] = true;
            }
            assert!(seen.iter().all(|s| *s), "{} 番目の値に抜けがある", channel);
        }
    }

    #[test]
    fn version_declaration_requires_glsl_140_or_es_300() {
        assert_eq!(
            version_declaration(ShaderVersion::Gl140),
            Some("#version 140\n")
        );
        assert_eq!(
            version_declaration(ShaderVersion::Es300),
            Some("#version 300 es\n")
        );
        assert_eq!(version_declaration(ShaderVersion::Gl120), None);
        assert_eq!(version_declaration(ShaderVersion::Es100), None);
    }

    #[test]
    fn count_mismatches_ignores_alpha_and_counts_missing_pixels() {
        let rgb = [1, 2, 3, 4, 5, 6];
        assert_eq!(count_mismatches(&[1, 2, 3, 0, 4, 5, 6, 255], &rgb), 0);
        assert_eq!(count_mismatches(&[1, 2, 3, 0, 4, 5, 7, 255], &rgb), 1);
        // 読み戻しが短ければ足りない画素も食い違いに数える
        assert_eq!(count_mismatches(&[1, 2, 3, 255], &rgb), 1);
    }
}
