// Copies a frame drawn on the CPU, uploaded to `t_canvas`, to the swapchain
// image: one triangle covering the target, each pixel loaded unfiltered from
// the texel at the same position.

@group(0) @binding(0) var t_canvas: texture_2d<f32>;

@vertex
fn vs_blit(@builtin(vertex_index) vertex_id: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vertex_id << 1u) & 2u), f32(vertex_id & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_blit(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(t_canvas, vec2<i32>(position.xy), 0);
}
