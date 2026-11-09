// Rounded-rect "plate" shader: an axis-aligned, anti-aliased rounded
// rectangle drawn as a flat colour, with no texture at all.
//
// This exists for exactly one caller today -- the source attribution
// credit's translucent backing plate (`map_view.rs`'s `ATTRIBUTION_PLATE_*`
// constants) -- but is written as its own small pipeline, sibling to
// `IconPipeline`, rather than folded into it: it needs no atlas bind group,
// no UV, no rotation and no SDF-sprite sampling, and bolting a "sometimes
// there's no texture" branch onto the icon shader would make BOTH paths
// harder to read for a feature only one of them needs.
//
// The corner rounding itself is the standard analytic rounded-box distance
// field (Inigo Quilez's `sdRoundedBox`), evaluated per-fragment against the
// quad's own half-size and radius -- no baked texture, so the same shader
// draws a crisp plate at any size the caller asks for, including a size that
// changes every frame as the credit's text re-shapes.

struct Viewport {
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0) var<uniform> viewport: Viewport;

struct VertexInput {
    // The quad's CENTRE in y-down physical screen pixels, replicated across
    // all six vertices -- same convention as `IconVertex::anchor_px`.
    @location(0) anchor_px: vec2<f32>,
    // This corner's offset from `anchor_px`, in physical pixels. Doubles as
    // the fragment stage's local coordinate for the rounded-box SDF, since a
    // plate is never rotated (unlike an icon, whose offset the vertex stage
    // rotates about its anchor).
    @location(1) offset_px: vec2<f32>,
    // Half the quad's (width, height) in physical pixels -- what the SDF
    // measures `offset_px` against.
    @location(2) half_size: vec2<f32>,
    @location(3) radius_px: f32,
    // Straight (non-premultiplied) sRGB, alpha included -- the plate's own
    // translucency (Mapbox's attribution control: `hsla(0,0%,100%,.5)`).
    @location(4) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local_pos: vec2<f32>,
    @location(1) half_size: vec2<f32>,
    @location(2) radius_px: f32,
    @location(3) color: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    let p = input.anchor_px + input.offset_px;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        (p.x / viewport.size.x) * 2.0 - 1.0,
        1.0 - (p.y / viewport.size.y) * 2.0,
        0.0,
        1.0,
    );
    out.local_pos = input.offset_px;
    out.half_size = input.half_size;
    out.radius_px = input.radius_px;
    out.color = input.color;
    return out;
}

/// # Colour space: this shader writes sRGB, and does not linearise
///
/// The render target is a UNORM format, not `*Srgb` (see `map_view.rs`'s
/// surface-format selection for the full reasoning). So the value written
/// here is the value stored, and the fixed-function blender composites
/// translucent layers on sRGB-ENCODED values -- which is what MapLibre GL
/// JS, Mapbox GL JS and all four MapLibre Native backends do, and what the
/// styles' alphas were chosen against.
///
/// This shader therefore writes the authored sRGB colour through unchanged.
/// It used to convert to linear here and let an `Srgb` target encode it back
/// on write; the two cancelled for every opaque pixel, and existed only to
/// move the blend into linear light. Removing both is what moved it back.
/// Do NOT reintroduce a transfer function here without changing the target
/// format in the same commit -- either one alone is a map about 2.2 gamma
/// out.

/// Inigo Quilez's `sdRoundedBox`: signed distance from `p` to a box of
/// half-extents `half_size` whose corners are rounded by `radius` (negative
/// inside the shape, positive outside, zero on its boundary).
fn rounded_box_sdf(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p) - half_size + vec2<f32>(radius, radius);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let distance = rounded_box_sdf(in.local_pos, in.half_size, in.radius_px);
    // One physical pixel of antialiasing either side of the edge -- the same
    // "soften across a pixel, not a fixed field-unit gamma" idea as the icon
    // shader's SDF branch, just simpler: a plate's edge is drawn at 1:1
    // scale, not sampled from a field authored at some other resolution, so
    // there is no separate scale to divide out.
    let coverage = 1.0 - smoothstep(-1.0, 1.0, distance);
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
