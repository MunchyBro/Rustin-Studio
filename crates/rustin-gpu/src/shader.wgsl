// One textured quad per layer. Pixel mapping mirrors the CPU compositor exactly:
// source = floor((pixel - origin) / scale), nearest-neighbour, straight-alpha "over".
struct Params {
    rect: vec4<f32>,   // x, y, out_w, out_h in canvas pixels
    info: vec4<f32>,   // scale, opacity (0..1), src_w, src_h
    canvas: vec4<f32>, // canvas_w, canvas_h
};

@group(0) @binding(0) var<uniform> u: Params;
@group(0) @binding(1) var tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0));
    let c = corners[i];
    let px = u.rect.x + c.x * u.rect.z;
    let py = u.rect.y + c.y * u.rect.w;
    return vec4<f32>(px / u.canvas.x * 2.0 - 1.0, 1.0 - py / u.canvas.y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let ox = floor(pos.x) - u.rect.x;
    let oy = floor(pos.y) - u.rect.y;
    let sx = clamp(floor(ox / u.info.x), 0.0, u.info.z - 1.0);
    let sy = clamp(floor(oy / u.info.x), 0.0, u.info.w - 1.0);
    let t = textureLoad(tex, vec2<i32>(i32(sx), i32(sy)), 0);
    return vec4<f32>(t.rgb, t.a * u.info.y);
}
