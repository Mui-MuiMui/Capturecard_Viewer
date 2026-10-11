// YUY2 → RGB の変換の wgpu（WGSL）版（#492）。GL の版は yuy2_to_rgb.vert / yuy2_to_rgb.frag。
//
// **CPU の変換（src/video/convert.rs の yuy2_to_rgb_naive）と同じ式・同じ係数表を、
// 同じ整数の演算で行う。** 係数は 1024 倍の固定小数点（src/video/color.rs の
// ColorMatrix）で、色空間・レンジ・映像調整はそこへ畳み込んである。浮動小数点で
// 書くと丸めが CPU とずれるので、計算は整数だけで行う。
//
// 入力の source は YUY2 を Rgba8Uint のテクスチャとして上げたもの（幅は画素数の半分）。
// 1 テクセルが 2 画素ぶんの Y0 U Y1 V を 0〜255 の整数のまま持つ（GL の版と違い、
// 正規化された値から戻す手間が無い）。描画先は映像と同じ大きさの Rgba8Unorm で、
// 画素 (x, y) はテクセル (x / 2, y) から作る。
// 係数の並びは src/video/gpu_yuy2_gl.rs の shader_coefficients と同じ。

struct Coefficients {
    // Y に掛ける係数と、Y の原点・明るさ・コントラストの定数をまとめた bias
    // （cy * (Y - y_offset) + offset = cy * Y - bias）
    y: i32,
    bias: i32,
    r_v: i32,
    g_u: i32,
    g_v: i32,
    b_u: i32,
}

@group(0) @binding(0) var source: texture_2d<u32>;
@group(0) @binding(1) var<uniform> coefficients: Coefficients;

// 頂点属性を持たず、頂点の番号から描画先全体を覆う三角形 1 枚を作る
// 0 → (-1, -1)、1 → (3, -1)、2 → (-1, 3)
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index & 2u) * 2 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

// 1024 倍の値を 0〜255 にする。CPU は (v >> 10).clamp(0, 255) だが、負の値の右シフトの
// 扱いに頼らないよう、先に 0〜255.999 の範囲へ収めてからシフトする（結果は同じ）
fn to_channel(value: i32) -> i32 {
    return clamp(value, 0, 262143) >> 10u;
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    let texel = vec4<i32>(textureLoad(source, vec2<i32>(pixel.x / 2, pixel.y), 0));
    var y = texel.r;
    if (pixel.x % 2 != 0) {
        y = texel.b;
    }
    let d = texel.g - 128;
    let e = texel.a - 128;
    let c = coefficients;
    let l = c.y * y - c.bias;
    let r = to_channel(l + c.r_v * e);
    let g = to_channel(l - c.g_u * d - c.g_v * e);
    let b = to_channel(l + c.b_u * d);
    return vec4<f32>(f32(r), f32(g), f32(b), 255.0) / 255.0;
}
