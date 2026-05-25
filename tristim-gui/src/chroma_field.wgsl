// Chromaticity color field.
//
// Paints each pixel of the plot square with the true color of the chromaticity
// at that point, clipped to the presenter's gamut. Because the gamut equals the
// renderer's working color space, the gamut's XYZ->RGB matrix yields values
// already in working space (linear), which is exactly what @location(0) wants.
// Outside the gamut (any linear channel < 0) the pixel is transparent, so the
// fill is the gamut triangle with no explicit boundary test.
//
// Vertex envelope matches stock::rounded_rect / the gradient exemplar.
// Uniform slots (packed by the host as Vec4 — verbatim, no color conversion):
//   vec_a = m00, m01, m02, m10   (rows of XYZ->RGB)
//   vec_b = m11, m12, m20, m21
//   vec_c = m22, space_flag (0 = xy, 1 = u'v'), _, _
//   vec_d = x_min, x_max, y_min, y_max   (plot view window)

struct FrameUniforms {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;

struct VertexInput {
    @location(0) corner_uv: vec2<f32>,
};

struct InstanceInput {
    @location(1) rect: vec4<f32>,  // xy = top-left px, zw = size px
    @location(2) vec_a: vec4<f32>,
    @location(3) vec_b: vec4<f32>,
    @location(4) vec_c: vec4<f32>,
    @location(6) vec_d: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv01: vec2<f32>,
    @location(1) m_a: vec4<f32>,
    @location(2) m_b: vec4<f32>,
    @location(3) m_c: vec4<f32>,
    @location(4) view: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput, inst: InstanceInput) -> VertexOutput {
    let pos_px = in.corner_uv * inst.rect.zw + inst.rect.xy;
    var out: VertexOutput;
    out.clip_pos = vec4<f32>(
        pos_px.x / frame.viewport.x * 2.0 - 1.0,
        1.0 - pos_px.y / frame.viewport.y * 2.0,
        0.0,
        1.0,
    );
    out.uv01 = in.corner_uv;
    out.m_a = inst.vec_a;
    out.m_b = inst.vec_b;
    out.m_c = inst.vec_c;
    out.view = inst.vec_d;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Element 0..1 -> plot coordinate (y flipped: uv01.y = 0 is the top = y_max).
    let x_min = in.view.x;
    let x_max = in.view.y;
    let y_min = in.view.z;
    let y_max = in.view.w;
    let px = x_min + in.uv01.x * (x_max - x_min);
    let py = y_min + (1.0 - in.uv01.y) * (y_max - y_min);

    // Plot coordinate -> CIE xy. space_flag: 0 = already xy, 1 = u'v'.
    var xy: vec2<f32>;
    if (in.m_c.y < 0.5) {
        xy = vec2<f32>(px, py);
    } else {
        let d = 6.0 * px - 16.0 * py + 12.0;
        xy = vec2<f32>(9.0 * px / d, 4.0 * py / d);
    }
    if (xy.y <= 1e-4) {
        return vec4<f32>(0.0);
    }

    // xy -> XYZ at unit luminance.
    let bx = xy.x / xy.y;
    let by = 1.0;
    let bz = (1.0 - xy.x - xy.y) / xy.y;

    // XYZ -> linear RGB in the working space (matrix rows from m_a / m_b / m_c).
    let r = in.m_a.x * bx + in.m_a.y * by + in.m_a.z * bz;
    let g = in.m_a.w * bx + in.m_b.x * by + in.m_b.y * bz;
    let b = in.m_b.z * bx + in.m_b.w * by + in.m_c.x * bz;

    let mx = max(r, max(g, b));
    if (mx <= 1e-6) {
        return vec4<f32>(0.0);
    }
    // Normalize so the brightest channel is full (vivid chromaticity fill).
    let rgb = clamp(vec3<f32>(r, g, b) / mx, vec3<f32>(0.0), vec3<f32>(1.0));

    // Antialiased gamut edge: fade out as the minimum channel crosses zero.
    let lo = min(r, min(g, b));
    let alpha = clamp(lo / max(fwidth(lo), 1e-5), 0.0, 1.0);
    return vec4<f32>(rgb, alpha);
}
