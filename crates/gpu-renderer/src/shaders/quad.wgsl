// Instanced SDF rounded rectangle, border, drop shadow, and underline decoration shader.
//
// Coordinates are in physical pixels with origin (0, 0) at top-left.
//
// `params` layout:
//   x = border_width (or stroke thickness for underline modes)
//   y = mode/shadow:
//       > 0.0  => soft Gaussian drop shadow with blur radius `params.y`
//       == 0.0 => standard SDF rounded rectangle + optional border
//       == -1.0 => curly sine-wave underline (wavelength in `params.z`)
//       == -2.0 => dotted underline (period in `params.z`)
//       == -3.0 => dashed underline (period in `params.z`)
//       == -4.0 => diagonal copy shine (band in `params.x`, progress in `params.z`, tilt in `params.w`)
//   z = shadow_offset_x (or wave/dash period in physical pixels)
//   w = shadow_offset_y (or wave amplitude in physical pixels)

struct Globals {
    viewport_size: vec2<f32>,
    scale_factor: f32,
    flags: u32, // bit 0: target_is_srgb, bit 1: is_light_mode
};

@group(0) @binding(0)
var<uniform> globals: Globals;

struct QuadInput {
    @location(0) bounds: vec4<f32>,       // x, y, width, height
    @location(1) clip_rect: vec4<f32>,    // min_x, min_y, max_x, max_y
    @location(2) fill_color: vec4<f32>,   // sRGB RGBA 0..1
    @location(3) border_color: vec4<f32>, // sRGB RGBA 0..1
    @location(4) corner_radii: vec4<f32>, // tl, tr, br, bl
    @location(5) params: vec4<f32>,       // border_width, shadow_blur/mode, offset_x/period, offset_y/amp
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) pixel_pos: vec2<f32>,
    @location(1) bounds: vec4<f32>,
    @location(2) clip_rect: vec4<f32>,
    @location(3) fill_color: vec4<f32>,
    @location(4) border_color: vec4<f32>,
    @location(5) corner_radii: vec4<f32>,
    @location(6) params: vec4<f32>,
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

fn encode_target_color(srgb_rgb: vec3<f32>) -> vec3<f32> {
    if ((globals.flags & 1u) != 0u) {
        return srgb_to_linear(srgb_rgb);
    }
    return srgb_rgb;
}

// Signed distance function for a box centered at origin with per-corner radii.
// `radii`: x = top-left, y = top-right, z = bottom-right, w = bottom-left.
fn sd_rounded_box(p: vec2<f32>, half_size: vec2<f32>, radii: vec4<f32>) -> f32 {
    let r_right = select(radii.y, radii.z, p.y > 0.0);
    let r_left = select(radii.x, radii.w, p.y > 0.0);
    let r = clamp(select(r_left, r_right, p.x > 0.0), 0.0, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

// Abramowitz & Stegun 7.1.26 approximation of erf(x) (max error ~1.5e-7).
fn erf_approx(x: f32) -> f32 {
    let s = sign(x);
    let a = abs(x);
    let t = 1.0 / (1.0 + 0.3275911 * a);
    let poly = (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t;
    return s * (1.0 - poly * exp(-a * a));
}

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    inst: QuadInput,
) -> VertexOutput {
    // Two triangles covering [0, 1] x [0, 1].
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let unit = corners[vertex_index];

    let shadow_blur = inst.params.y;
    var quad_min = inst.bounds.xy;
    var quad_max = inst.bounds.xy + inst.bounds.zw;

    if (shadow_blur > 0.0) {
        let pad = ceil(shadow_blur * 2.5) + 1.0;
        let offset = inst.params.zw;
        quad_min = quad_min + offset - vec2<f32>(pad, pad);
        quad_max = quad_max + offset + vec2<f32>(pad, pad);
    } else {
        // 1px expansion for smooth SDF antialiasing on rounded edges.
        quad_min = quad_min - vec2<f32>(1.0, 1.0);
        quad_max = quad_max + vec2<f32>(1.0, 1.0);
    }

    // Clamp geometry to clip rect when valid to save fragment fillrate.
    let raw_pos = mix(quad_min, quad_max, unit);
    let clip_valid = inst.clip_rect.z > inst.clip_rect.x && inst.clip_rect.w > inst.clip_rect.y;
    let clamped_pos = clamp(raw_pos, inst.clip_rect.xy, inst.clip_rect.zw);
    let pixel_pos = select(raw_pos, clamped_pos, clip_valid);

    let ndc = vec2<f32>(
        (pixel_pos.x / max(globals.viewport_size.x, 1.0)) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / max(globals.viewport_size.y, 1.0)) * 2.0,
    );

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.pixel_pos = pixel_pos;
    out.bounds = inst.bounds;
    out.clip_rect = inst.clip_rect;
    out.fill_color = inst.fill_color;
    out.border_color = inst.border_color;
    out.corner_radii = inst.corner_radii;
    out.params = inst.params;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if (in.pixel_pos.x < in.clip_rect.x || in.pixel_pos.y < in.clip_rect.y ||
        in.pixel_pos.x > in.clip_rect.z || in.pixel_pos.y > in.clip_rect.w) {
        discard;
    }

    let size = in.bounds.zw;
    if (size.x <= 0.0 || size.y <= 0.0) {
        discard;
    }

    let half_size = size * 0.5;
    let center = in.bounds.xy + half_size;
    let mode = in.params.y;

    // Soft Gaussian drop shadow mode (mode > 0.0).
    if (mode > 0.0) {
        let shadow_center = center + in.params.zw;
        let dist = sd_rounded_box(in.pixel_pos - shadow_center, half_size, in.corner_radii);
        let sigma = max(mode * 0.5, 0.5);
        // 1D Gaussian CDF across SDF edge produces silky, CSS-accurate shadow falloff.
        let coverage = 0.5 * (1.0 - erf_approx(dist / (sigma * 1.41421356)));
        let alpha = clamp(coverage, 0.0, 1.0) * in.fill_color.a;
        if (alpha <= 0.001) {
            discard;
        }
        return vec4<f32>(encode_target_color(in.fill_color.rgb), alpha);
    }

    // Diagonal Copy Shine sweep mode (mode == -4.0).
    if (mode < -3.5) {
        let p = in.pixel_pos - center;
        let dist = sd_rounded_box(p, half_size, in.corner_radii);
        let box_cov = clamp(0.5 - dist, 0.0, 1.0);
        if (box_cov <= 0.001) {
            discard;
        }
        let progress = clamp(in.params.z, 0.0, 1.0);
        let band = max(in.params.x, 0.08);
        let tilt = max(in.params.w, 0.0);
        let diag_span = max(size.x + size.y * tilt, 1.0);
        let diag_pos = ((in.pixel_pos.x - in.bounds.x) + (in.pixel_pos.y - in.bounds.y) * tilt) / diag_span;
        let sweep_center = mix(-band, 1.0 + band, progress);
        let beam = pow(clamp(1.0 - abs(diag_pos - sweep_center) / band, 0.0, 1.0), 1.8);
        let env = smoothstep(0.0, 0.12, progress) * (1.0 - smoothstep(0.72, 1.0, progress));
        let base_flash = sin(progress * 3.14159265) * 0.22;
        let intensity = clamp(beam * 0.85 + base_flash, 0.0, 1.0) * env;
        let alpha = box_cov * intensity * in.fill_color.a;
        if (alpha <= 0.001) {
            discard;
        }
        return vec4<f32>(encode_target_color(in.fill_color.rgb), alpha);
    }

    // Special underline decoration modes (mode < 0.0).
    if (mode < -0.5) {
        if (in.pixel_pos.x < in.bounds.x || in.pixel_pos.x > in.bounds.x + size.x ||
            in.pixel_pos.y < in.bounds.y || in.pixel_pos.y > in.bounds.y + size.y) {
            discard;
        }
        let local_x = in.pixel_pos.x - in.bounds.x;
        let stroke = max(in.params.x, 1.0);
        var coverage = 0.0;

        if (mode > -1.5) {
            // Curly sine-wave underline.
            let wavelength = max(in.params.z, 4.0);
            let amp = max(in.params.w, 1.0);
            let phase = (local_x / wavelength) * 6.2831853;
            let wave_y = center.y + amp * sin(phase);
            // Compensate perpendicular distance using derivative of sine wave.
            let slope = amp * (6.2831853 / wavelength) * cos(phase);
            let dist = abs(in.pixel_pos.y - wave_y) / sqrt(1.0 + slope * slope);
            coverage = clamp(stroke * 0.5 + 0.5 - dist, 0.0, 1.0);
        } else if (mode > -2.5) {
            // Dotted underline.
            let period = max(in.params.z, 3.0);
            let cell = fract(local_x / period);
            let on_dot = select(0.0, 1.0, cell < 0.5);
            let dist_y = abs(in.pixel_pos.y - center.y);
            coverage = on_dot * clamp(stroke * 0.5 + 0.5 - dist_y, 0.0, 1.0);
        } else {
            // Dashed underline.
            let period = max(in.params.z, 6.0);
            let cell = fract(local_x / period);
            let on_dash = select(0.0, 1.0, cell < 0.62);
            let dist_y = abs(in.pixel_pos.y - center.y);
            coverage = on_dash * clamp(stroke * 0.5 + 0.5 - dist_y, 0.0, 1.0);
        }

        let alpha = coverage * in.fill_color.a;
        if (alpha <= 0.001) {
            discard;
        }
        return vec4<f32>(encode_target_color(in.fill_color.rgb), alpha);
    }

    // Standard SDF rounded rectangle with optional border.
    let p = in.pixel_pos - center;
    let dist = sd_rounded_box(p, half_size, in.corner_radii);
    let outer_alpha = clamp(0.5 - dist, 0.0, 1.0);
    if (outer_alpha <= 0.001) {
        discard;
    }

    let fill_rgb = encode_target_color(in.fill_color.rgb);
    let border_width = in.params.x;

    if (border_width > 0.0 && in.border_color.a > 0.001) {
        let inner_dist = dist + border_width;
        let inner_alpha = clamp(0.5 - inner_dist, 0.0, 1.0);
        let border_cov = clamp(outer_alpha - inner_alpha, 0.0, 1.0);

        let fill_a = in.fill_color.a * inner_alpha;
        let border_a = in.border_color.a * border_cov;
        let border_rgb = encode_target_color(in.border_color.rgb);

        // Premultiplied union of inner fill + border band.
        let out_a = fill_a + border_a;
        if (out_a <= 0.001) {
            discard;
        }
        let out_rgb = (fill_rgb * fill_a + border_rgb * border_a) / out_a;
        return vec4<f32>(out_rgb, out_a);
    }

    let final_a = in.fill_color.a * outer_alpha;
    if (final_a <= 0.001) {
        discard;
    }
    return vec4<f32>(fill_rgb, final_a);
}
