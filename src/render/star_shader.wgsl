// The star field: one camera-facing billboard per catalogue star, in an
// INERTIAL frame.
//
// See `render::space`'s module docs for the reference position. The short
// version: MapLibre GL JS has no star field at all (zero matches for
// `\bstars?\b` under its `src/`), so nothing here is ported -- the sky is
// ours, and the two things that make it look like a sky rather than noise
// are that the positions are real (Yale BSC5, `src/render/PROVENANCE.md`)
// and that the frame does not turn with the planet.

struct Sky {
    /// Sky DIRECTION -> clip space. Carries the camera's ORIENTATION and
    /// the planet's own rotation -- the same `Rx(lat) * Ry(-lon)` pair the
    /// globe matrix applies -- so the stars are fixed to the GROUND. No
    /// translation and no zoom: a star is at infinity. See
    /// `render::space::sky_view_proj`.
    sky_view_proj: mat4x4<f32>,
    /// LOGICAL pixels across the viewport. The billboard is sized in clip
    /// space from a pixel figure, so it needs the conversion.
    viewport_px: vec2<f32>,
    /// Multiplies every star's alpha. `0.0` draws nothing and is the
    /// default; `render::space::Space::star_intensity` is the knob, and the
    /// draw is skipped entirely at zero rather than submitted and
    /// multiplied out -- the gate MapLibre applies to its own globe-only
    /// atmosphere pass (`draw_sky.ts:91-95`, "Don't draw anything if
    /// atmosphere is fully transparent").
    intensity: f32,
    /// Multiplies every star's radius. Exists because "how big is a star"
    /// is a taste question and a display-density question, not a physical
    /// one; `star_half_size_px` decides the RELATIVE sizes from magnitude.
    size_scale: f32,
    /// `xyz`: the ground observer's up in the sky frame; `w`: the horizon
    /// fade's width as the sine of an altitude, `0` for no horizon at all
    /// (the space view). See `render::space::horizon_fade`.
    horizon: vec4<f32>,
}

@group(0) @binding(0) var<uniform> sky: Sky;

struct VertexInput {
    /// Unit direction in the sky frame, in exactly the convention
    /// `shader.wgsl`'s `project_to_sphere` returns for the planet:
    /// `(sin(ra)*cos(dec), sin(dec), cos(ra)*cos(dec))`, y north.
    @location(0) dir: vec3<f32>,
    /// Half-extent of the billboard in LOGICAL pixels, before `size_scale`.
    @location(1) half_size_px: f32,
    /// Relative flux, already clamped to `[0, 1]` on the CPU.
    @location(2) brightness: f32,
    /// Which corner of the quad this vertex is, in `[-1, 1]^2`. Carried per
    /// vertex rather than derived from `@builtin(vertex_index) % 4u`: the
    /// derivation is correct only while the index buffer's winding stays
    /// exactly `[0,1,2, 0,2,3]` per star, and 291 KB of a 1.24 MB static
    /// buffer is not worth a coupling no test would catch.
    @location(3) corner: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    /// `corner`, interpolated -- the fragment stage's distance from the
    /// star's centre, in units of its own radius.
    @location(0) corner: vec2<f32>,
    @location(1) brightness: f32,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.corner = input.corner;
    out.brightness = input.brightness * sky.intensity;

    // A ground observer's sky (`MapView::set_sky_stars`): nothing at or
    // below the horizon, a smoothstep up to full over `horizon.w` -- the
    // haze and the city's glow. `render::space::horizon_fade` mirrors it.
    if (sky.horizon.w > 0.0) {
        let altitude = dot(input.dir, sky.horizon.xyz);
        if (altitude <= 0.0) {
            out.clip_position = vec4<f32>(0.0, 0.0, -1.0, 1.0);
            out.brightness = 0.0;
            return out;
        }
        out.brightness = out.brightness * smoothstep(0.0, sky.horizon.w, altitude);
    }

    let centre = sky.sky_view_proj * vec4<f32>(input.dir, 1.0);
    // BEHIND the camera. A right-handed perspective divide makes `w` the
    // forward distance, so `w <= 0` is a star on the other side of the eye
    // -- and dividing by it would MIRROR the star onto the opposite side of
    // the screen, which is a whole extra sky nobody asked for. Pushing the
    // vertex to `z < 0` puts it outside wgpu's `0 <= z <= w` clip volume,
    // which is the same mechanism `shader.wgsl`'s `globe_clipping_z` uses
    // to drop the planet's far side.
    if (centre.w <= 0.0) {
        out.clip_position = vec4<f32>(0.0, 0.0, -1.0, 1.0);
        return out;
    }

    // Offset in clip space, scaled by `w` so the perspective divide leaves
    // exactly `half_size_px` on screen whatever distance the sky sits at.
    // This is the screen-space statement of "camera up and right vectors
    // multiplied by the star's size": up and right in VIEW space are the
    // clip-space x and y axes, so no basis has to be extracted from the
    // matrix and no aspect correction is needed beyond `viewport_px`.
    let half_px = input.half_size_px * sky.size_scale;
    var clip = centre;
    clip.x = clip.x + input.corner.x * half_px * 2.0 / sky.viewport_px.x * clip.w;
    clip.y = clip.y + input.corner.y * half_px * 2.0 / sky.viewport_px.y * clip.w;
    out.clip_position = clip;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // A star is a point source, so the disc on screen is a point-spread
    // function, not a shape: brightest at the centre and gone at the rim.
    // `1 - r^2` rather than `1 - r` because the quadratic has zero slope at
    // the centre, so the brightest stars do not read as flat-topped discs.
    let falloff = clamp(1.0 - dot(input.corner, input.corner), 0.0, 1.0);
    // Straight (non-premultiplied) alpha, matching every other pipeline in
    // this crate: the colour targets are all `BlendState::ALPHA_BLENDING`.
    return vec4<f32>(1.0, 1.0, 1.0, input.brightness * falloff);
}
