// Instanced glyph & icon shader supporting:
//   mode == 0: R8Unorm monochrome text / monochrome SVG mask with gamma-aware contrast & tint
//   mode == 1: Rgba8UnormSrgb full-color emoji / brand SVG icon (opacity from color.a)
//   mode == 2: Rgba8UnormSrgb alpha-mask tinted by color.rgb

struct Globals {
    viewport_size: vec2<f32>,
    scale_factor: f32,
    flags: u32, // bit 0: target_is_srgb, bit 1: is_light_mode
};

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(1) @binding(0)
var mono_atlas: texture_2d<f32>;

@group(1) @binding(1)
var mono_sampler: sampler;

@group(1) @binding(2)
var color_atlas: texture_2d<f32>;

@group(1) @binding(3)
var color_sampler: sampler;

struct GlyphInput {
    @location(0) bounds: vec4<f32>,    // x, y, width, height (physical px)
    @location(1) uv_bounds: vec4<f32>, // u_min, v_min, u_max, v_max
    @location(2) clip_rect: vec4<f32>, // min_x, min_y, max_x, max_y
    @location(3) color: vec4<f32>,     // sRGB RGBA 0..1
    @location(4) mode: u32,            // 0 = mono R8, 1 = color RGBA8, 2 = tinted RGBA8
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel_pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) clip_rect: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) @interpolate(flat) mode: u32,
};

fn srgb_channel_to_linear(c: f32) -> f32 {
    if (c <= 0.04045) {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn srgb_to_linear(rgb: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        srgb_channel_to_linear(rgb.x),
        srgb_channel_to_linear(rgb.y),
        srgb_channel_to_linear(rgb.z),
    );
}

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

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    inst: GlyphInput,
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
    out.clip_rect = inst.clip_rect;
    out.color = inst.color;
    out.mode = inst.mode;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if (in.pixel_pos.x < in.clip_rect.x || in.pixel_pos.y < in.clip_rect.y ||
        in.pixel_pos.x > in.clip_rect.z || in.pixel_pos.y > in.clip_rect.w) {
        discard;
    }

    let target_is_srgb = (globals.flags & 1u) != 0u;
    let is_light_mode = (globals.flags & 2u) != 0u;

    if (in.mode == 0u) {
        let cov = textureSample(mono_atlas, mono_sampler, in.uv).r;
        if (cov <= 0.002) {
            discard;
        }

        // Gamma-aware stem contrast compensation:
        // Hardware linear blending in sRGB render targets thins dark-on-light strokes
        // and slightly thins mid-tone strokes on dark surfaces. Adjusting coverage
        // by perceived foreground luminance preserves Ghostty/CoreText-grade stem weight.
        let fg_luma = dot(in.color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        var gamma = 0.88;
        if (is_light_mode && fg_luma < 0.5) {
            // Dark glyphs on light background: crisp stem darkening.
            gamma = 0.74;
        } else if (!is_light_mode && fg_luma > 0.5) {
            // Bright glyphs on dark background: balanced crisp stems without haloing.
            gamma = 0.90;
        }
        let adjusted_cov = select(cov, pow(cov, gamma), target_is_srgb);
        let alpha = adjusted_cov * in.color.a;
        if (alpha <= 0.002) {
            discard;
        }
        let rgb = select(in.color.rgb, srgb_to_linear(in.color.rgb), target_is_srgb);
        return vec4<f32>(rgb, alpha);
    }

    // Color atlas is Rgba8UnormSrgb, so hardware textureSample returns linear RGB.
    let texel = textureSample(color_atlas, color_sampler, in.uv);
    let alpha = texel.a * in.color.a;
    if (alpha <= 0.002) {
        discard;
    }

    if (in.mode == 2u) {
        let rgb = select(in.color.rgb, srgb_to_linear(in.color.rgb), target_is_srgb);
        return vec4<f32>(rgb, alpha);
    }

    let rgb = select(linear_to_srgb(texel.rgb), texel.rgb, target_is_srgb);
    return vec4<f32>(rgb, alpha);
}
