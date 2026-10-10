// YUY2 → RGB の変換（#456）のフラグメントシェーダー。
//
// **CPU の変換（src/video/convert.rs の yuy2_to_rgb_naive）と同じ式・同じ係数表を、
// 同じ整数の演算で行う。** 係数は 1024 倍の固定小数点（src/video/color.rs の
// ColorMatrix）で、色空間・レンジ・映像調整はそこへ畳み込んである。浮動小数点で
// 書くと丸めが CPU とずれるので、画素の値を 0〜255 の整数へ戻してから計算する。
//
// 入力の u_yuy2 は YUY2 を RGBA8 のテクスチャとして上げたもの（幅は画素数の半分）。
// 1 テクセルが 2 画素ぶんの Y0 U Y1 V を持つ。描画先は映像と同じ大きさの RGBA8 で、
// 画素 (x, y) はテクセル (x / 2, y) から作る。
// 先頭の #version は src/video/gpu_yuy2_gl.rs が GL の版に合わせて足す。

#ifdef GL_ES
precision highp float;
precision highp int;
precision highp sampler2D;
#endif

uniform sampler2D u_yuy2;
// Y に掛ける係数と、Y の原点・明るさ・コントラストの定数をまとめた bias
// （cy * (Y - y_offset) + offset = cy * Y - bias）
uniform int u_y;
uniform int u_bias;
uniform int u_r_v;
uniform int u_g_u;
uniform int u_g_v;
uniform int u_b_u;

out vec4 out_color;

// 正規化された 8 ビットの値を 0〜255 の整数へ戻す
int to_byte(float value) {
    return int(floor(value * 255.0 + 0.5));
}

// 1024 倍の値を 0〜255 にする。CPU は (v >> 10).clamp(0, 255) だが、負の値の右シフトの
// 扱いに頼らないよう、先に 0〜255.999 の範囲へ収めてからシフトする（結果は同じ）
int to_channel(int value) {
    return clamp(value, 0, 262143) >> 10;
}

void main() {
    ivec2 pixel = ivec2(gl_FragCoord.xy);
    vec4 texel = texelFetch(u_yuy2, ivec2(pixel.x / 2, pixel.y), 0);
    int y = to_byte((pixel.x % 2 == 0) ? texel.r : texel.b);
    int d = to_byte(texel.g) - 128;
    int e = to_byte(texel.a) - 128;
    int l = u_y * y - u_bias;
    int r = to_channel(l + u_r_v * e);
    int g = to_channel(l - u_g_u * d - u_g_v * e);
    int b = to_channel(l + u_b_u * d);
    out_color = vec4(float(r), float(g), float(b), 255.0) / 255.0;
}
