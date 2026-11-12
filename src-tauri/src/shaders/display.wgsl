struct Transform {
    rect: vec4<f32>,
    clip: vec4<f32>,
    window: vec2<f32>,
    image_size: vec2<f32>,
    texture_size: vec2<f32>,
    pixelated: f32,
    has_display_lut: f32,
    bg_primary: vec4<f32>,
    bg_secondary: vec4<f32>,
    original_size: vec2<f32>,
    split_origin: vec2<f32>,
    split_normal: vec2<f32>,
    split_offset: vec2<f32>,
};

@group(0) @binding(0) var<uniform> transform: Transform;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var original_tex: texture_2d<f32>;
@group(0) @binding(4) var display_lut: texture_3d<f32>;
@group(0) @binding(5) var display_lut_samp: sampler;

struct VertexOutput {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) id: u32) -> VertexOutput {
    let uvs = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0)
    );
    let pos = uvs[id];

    let uv_x = transform.clip.x + pos.x * transform.clip.z;
    let uv_y = transform.clip.y + pos.y * transform.clip.w;

    let half_pixel_x = 0.5;
    let half_pixel_y = 0.5;
    let outset_x = (pos.x * 2.0 - 1.0) * half_pixel_x;
    let outset_y = (pos.y * 2.0 - 1.0) * half_pixel_y;

    let screen_x = uv_x + outset_x;
    let screen_y = uv_y + outset_y;

    let ndc_x = (screen_x / transform.window.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (screen_y / transform.window.y) * 2.0;

    var out: VertexOutput;
    out.pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);

    out.uv = vec2<f32>(
        (uv_x - transform.rect.x) / transform.rect.z,
        (uv_y - transform.rect.y) / transform.rect.w
    );

    return out;
}

fn sample_layer(layer: texture_2d<f32>, uv: vec2<f32>, image_size: vec2<f32>, texture_size: vec2<f32>) -> vec4<f32> {
    let adjusted_uv = uv * (image_size / texture_size);
    let half_texel = vec2<f32>(0.5, 0.5) / texture_size;

    let min_uv = half_texel;
    let max_uv = (image_size / texture_size) - half_texel;

    if (transform.pixelated > 0.5) {
        let texel_coords = floor(adjusted_uv * texture_size);
        let nearest_uv = (texel_coords + vec2<f32>(0.5, 0.5)) / texture_size;

        let clamped_nearest = clamp(nearest_uv, min_uv, max_uv);
        return textureSampleLevel(layer, samp, clamped_nearest, 0.0);
    } else {
        let clamped_uv = clamp(adjusted_uv, min_uv, max_uv);
        return textureSampleLevel(layer, samp, clamped_uv, 0.0);
    }
}

const MAT_P3_TO_SRGB: mat3x3<f32> = mat3x3<f32>(
    vec3<f32>(1.2249402, -0.0420570, -0.0196376),
    vec3<f32>(-0.2249402, 1.0420570, -0.0786361),
    vec3<f32>(0.0000000, 0.0000000, 1.0982736)
);

fn p3_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.04045);
    let a = vec3<f32>(0.055);
    let higher = pow((c + a) / (1.0 + a), vec3<f32>(2.4));
    let lower = c / 12.92;
    return select(higher, lower, c <= cutoff);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let c_clamped = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let cutoff = vec3<f32>(0.0031308);
    let a = vec3<f32>(0.055);
    let higher = (1.0 + a) * pow(c_clamped, vec3<f32>(1.0 / 2.4)) - a;
    let lower = c_clamped * 12.92;
    return select(higher, lower, c_clamped <= cutoff);
}

fn p3_to_srgb(p3_color: vec3<f32>) -> vec3<f32> {
    let lin_p3 = p3_to_linear(clamp(p3_color, vec3<f32>(0.0), vec3<f32>(1.0)));
    let lin_srgb = MAT_P3_TO_SRGB * lin_p3;
    return linear_to_srgb(lin_srgb);
}

fn sample_display_lut(color: vec4<f32>) -> vec4<f32> {
    if (transform.has_display_lut < 0.5) {
        return vec4<f32>(p3_to_srgb(color.rgb), color.a);
    }
    let lut_size = vec3<f32>(textureDimensions(display_lut));
    let scale = (lut_size - vec3<f32>(1.0)) / lut_size;
    let offset = vec3<f32>(0.5) / lut_size;
    let uv = clamp(color.rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * scale + offset;
    let mapped = textureSampleLevel(display_lut, display_lut_samp, uv, 0.0).rgb;
    return vec4<f32>(mapped, color.a);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let is_original = dot(in.pos.xy - transform.split_origin, transform.split_normal) < 0.0;
    let uv = select(in.uv, in.uv + transform.split_offset / transform.rect.zw, is_original);

    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        return transform.bg_secondary;
    }

    var base_color: vec4<f32>;
    if (is_original) {
        base_color = sample_layer(original_tex, uv, transform.original_size, transform.original_size);
    } else {
        base_color = sample_layer(tex, uv, transform.image_size, transform.texture_size);
    }
    return sample_display_lut(base_color);
}
