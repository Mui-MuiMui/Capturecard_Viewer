// YUY2 → RGB の変換（#456）の頂点シェーダー。頂点属性を持たず、gl_VertexID から
// 描画先（ビューポート）全体を覆う三角形 1 枚を作る。
// 先頭の #version は src/video/gpu_yuy2_gl.rs が GL の版に合わせて足す。

void main() {
    // 0 → (-1, -1)、1 → (3, -1)、2 → (-1, 3)
    vec2 position = vec2(
        float((gl_VertexID & 1) * 4 - 1),
        float((gl_VertexID & 2) * 2 - 1)
    );
    gl_Position = vec4(position, 0.0, 1.0);
}
