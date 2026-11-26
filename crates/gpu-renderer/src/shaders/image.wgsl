// Instanced textured image quad shader for Kitty Graphics Protocol layers & hover previews.
// Supports arbitrary UV sub-rects, opacity, rounded-corner SDF clipping, and scissor clip rect.

struct Globals {
    viewport_size: vec2<f32>,
    scale_factor: f32,
    flags: u32, // bit 0: target_is_srgb, bit 1: is_light_mode
};

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(1) @binding(0)
var image_tex: texture_2d<f32>;

@group(1) @binding(1)
var image_sampler: sampler;

struct ImageInput {
    @location(0) bounds: vec4<f32>,    // x, y, width, height (physical px)
    @location(1) uv_bounds: vec4<f32>, // u_min, v_min, u_max, v_max
    @location(2) clip_rect: vec4<f32>, // min_x, min_y, max_x, max_y
    @location(3) params: vec4<f32>,    // opacity, corner_radius, 0, 0
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel_pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) bounds: vec4<f32>,
    @location(3) clip_rect: vec4<f32>,
    @location(4) params: vec4<f32>,
};

fn linear_channel_to_srgb(c: f32) -> f32 {
    if (c <= 0.0031308) {
        return c * 12.92;
    }
    return 1.055 * pow(max(c, 0.0), 1.0 / 2.4) - 0.055;
}

fn linear_to_srgb(rgb: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        linear_channel_to_srgb(rgb.x),
        linear_channel_to_srgb(rgb.y),
        linear_channel_to_srgb(rgb.z),
    );
}

fn sd_rounded_box_uniform(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = clamp(radius, 0.0, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    inst: ImageInput,
) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let unit = corners[vertex_index];

    let pixel_pos = inst.bounds.xy + inst.bounds.zw * unit;
    let uv = mix(inst.uv_bounds.xy, inst.uv_bounds.zw, unit);
    let ndc = vec2<f32>(
        (pixel_pos.x / max(globals.viewport_size.x, 1.0)) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / max(globals.viewport_size.y, 1.0)) * 2.0,
    );

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.pixel_pos = pixel_pos;
    out.uv = uv;
    out.bounds = inst.bounds;
    out.clip_rect = inst.clip_rect;
    out.params = inst.params;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if (in.pixel_pos.x < in.clip_rect.x || in.pixel_pos.y < in.clip_rect.y ||
        in.pixel_pos.x > in.clip_rect.z || in.pixel_pos.y > in.clip_rect.w) {
        discard;
    }

    var mask = 1.0;
    let radius = in.params.y;
    if (radius > 0.0) {
        let half_size = in.bounds.zw * 0.5;
        let center = in.bounds.xy + half_size;
        let dist = sd_rounded_box_uniform(in.pixel_pos - center, half_size, radius);
        mask = clamp(0.5 - dist, 0.0, 1.0);
        if (mask <= 0.001) {
            discard;
        }
    }

    let texel = textureSample(image_tex, image_sampler, in.uv);
    let alpha = texel.a * in.params.x * mask;
    if (alpha <= 0.001) {
        discard;
    }

    let target_is_srgb = (globals.flags & 1u) != 0u;
    let rgb = select(linear_to_srgb(texel.rgb), texel.rgb, target_is_srgb);
    return vec4<f32>(rgb, alpha);
}
