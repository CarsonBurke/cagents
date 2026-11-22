// Fills the scissor rectangle with one color, as a clear would.
struct Clear {
    color: vec4<f32>,
}

@group(0) @binding(0) var<uniform> clear: Clear;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the whole target.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));

    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return clear.color;
}
