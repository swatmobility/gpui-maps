//! The tile-status debug overlay: one translucent rectangle per visible
//! tile, coloured by whether that tile's OWN data is loaded and bordered by
//! what the renderer is actually drawing in its place.
//!
//! # Why two colours per tile and not one
//!
//! This renderer retains tiles and substitutes stand-ins: when a visible
//! tile has no mesh of its own it draws a covering set of cached
//! descendants, or failing that a cached ancestor magnified up. That is
//! deliberate (it is what keeps a pan continuous) and it is *invisible* --
//! the map looks the same whether you are seeing a tile's real geometry or
//! its grandparent's, stretched 4x. "Loaded" and "looks loaded" are not
//! distinguishable by eye, and telling them apart is the entire point of
//! this overlay.
//!
//! Those are two independent facts, so they get two independent channels
//! rather than one colour ramp that has to pick a winner:
//!
//! - **Fill** is [`TileDataState`] -- what is known about *this* tile's own
//!   data. Loaded, decoded but with no mesh of its own, in flight, absent,
//!   failed, unknown.
//! - **Border** is [`TileDrawSource`] -- where the pixels inside the
//!   rectangle actually came from this frame. Its own mesh, an ancestor
//!   (with the zoom distance), a descendant set, or nothing at all.
//!
//! Collapsing them would lose real information in both directions. Past the
//! tileset's maxzoom every tile is simultaneously *absent* (the server has
//! no z16 tile and said so) and *drawn from an ancestor*; over unimported
//! regions a tile is absent with nothing standing in. A single colour would
//! have to hide one of those, and which one it hid would change with the
//! priority order rather than with the map.
//!
//! # Why this is not built on the overlay/stroke path
//!
//! `docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md`
//! records what happens when rectangle outlines go through the overlay's
//! lyon stroke tessellator at large coordinates: the stroke normal is a
//! difference of two points a fixed 50 lyon units apart, and once the
//! coordinate magnitude puts f32's spacing above that, the normals collapse
//! to zero and the line draws at zero width. Running the map today still
//! logs `N% of a stroke op's normals are not unit vectors`.
//!
//! Nothing here can hit that, because nothing here is tessellated. Tile
//! corners are projected to screen pixels ON THE CPU in f64
//! ([`project_world_to_screen_f64`](super::project_world_to_screen_f64),
//! the same path symbol anchors take), and the fill and border quads are
//! then built directly from those pixel coordinates. The largest number
//! that reaches a vertex buffer is a screen coordinate -- a few thousand at
//! most -- so the precision regime the spec describes is never entered.
//! That is also why it is cheaper: no plan, no tessellation, no mesh cache,
//! one draw call for the whole overlay.

use crate::geo::TileId;
use crate::render::{Camera, WORLD_SIZE};

/// What is known about a tile's OWN data -- the fill colour.
///
/// Every variant is read from a state the engine already keeps; this module
/// invents no bookkeeping of its own. See `MapView::tile_debug_statuses`
/// for the field each one is derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileDataState {
    /// This tile's own mesh is uploaded and drawn at its own zoom. The good
    /// state.
    Loaded,
    /// The tile's own data is in cache, but no mesh of its own is
    /// currently uploaded for it -- so a stand-in may well be drawn over
    /// it. Two causes, and the overlay deliberately does not claim to tell
    /// them apart: the tile tessellated to nothing for this style at this
    /// zoom (an ocean tile under a style with no water layer), or its GPU
    /// mesh was evicted from a bounded cache while its decoded data stayed.
    ///
    /// Distinct from `Absent` in the way that matters: the source *did*
    /// have this tile and did answer with it.
    Decoded,
    /// A fetch is in flight.
    Pending,
    /// The source stated it holds no data here -- `Ok(None)` from
    /// `TileSource::fetch`, i.e. a 404 from a sparse tileset or a
    /// coordinate outside a downloaded region. **Not a failure**: most of
    /// the ocean, and every region nobody imported, is legitimately absent
    /// and the map is working exactly as intended.
    ///
    /// # What this does NOT distinguish
    ///
    /// "Outside the tileset's coverage" and "inside its coverage, but there
    /// is nothing at this coordinate" are the same answer here, because
    /// they are the same answer from the source: a tile server that has not
    /// imported a region and one asked for a tile of open ocean both say
    /// "no data at this coordinate", and nothing in the reply separates
    /// them. The legend says "no data here" rather than "outside coverage"
    /// for exactly that reason. Telling the two apart would take a coverage
    /// bbox the source published and the client checked -- a real
    /// improvement, and a different feature from this one.
    Absent,
    /// The fetch or decode failed. A fault.
    Failed,
    /// Nothing is known yet: no data, no fetch in flight, not absent, not
    /// failed. The one-frame gap before a request is issued, and the state
    /// a tile evicted from every cache falls back to.
    Unknown,
}

/// Where the pixels inside a visible tile's rectangle actually came from
/// this frame -- the border colour.
///
/// These are not a parallel guess at the renderer's behaviour: they are
/// recorded by the stand-in selection loop in `MapView::paint` itself, at
/// the same branches that already call `note_descendant_stand_in`,
/// `note_ancestor_stand_in` and `note_visible_tile_without_stand_in`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileDrawSource {
    /// The tile's own mesh.
    Own,
    /// A cached ancestor, magnified. `zoom_distance` is how many zoom
    /// levels up -- the same figure `note_ancestor_stand_in` is given.
    Ancestor { zoom_distance: u8 },
    /// A complete cached set of descendants, drawn at their own finer zoom.
    Descendant,
    /// Neither -- a hole. Whatever is behind it is the previous frame's
    /// clear colour or a lower layer, not this tile's data.
    Nothing,
}

/// One visible tile's two-channel status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileDebugStatus {
    pub tile: TileId,
    pub data: TileDataState,
    pub draw: TileDrawSource,
}

/// Straight (non-premultiplied) sRGB, alpha included -- the same convention
/// `PlateInstance::color` and every other screen-space colour in this
/// renderer uses.
pub type DebugRgba = [f32; 4];

/// Fill alpha. Low enough to read the basemap through (roads and labels
/// stay legible), high enough that the hue is unambiguous against both the
/// light ground and dark water.
const FILL_ALPHA: f32 = 0.16;

/// Border alpha. Deliberately 1.0, not "nearly opaque": the four edge quads
/// overlap at the corners, and any alpha below 1 makes those overlaps
/// double-blend into visibly darker corner pips.
const BORDER_ALPHA: f32 = 1.0;

impl TileDataState {
    /// The hue this state fills with. Chosen mid-saturation so the same
    /// colour reads on the light basemap and on dark water at
    /// [`FILL_ALPHA`], and so `Absent` (a neutral slate) can never be
    /// mistaken for `Failed` (red) -- most of an unimported country is
    /// absent, and an overlay that painted that red would be reporting a
    /// catastrophe that is not happening.
    pub fn rgb(self) -> [f32; 3] {
        match self {
            TileDataState::Loaded => [0.16, 0.78, 0.36],
            TileDataState::Decoded => [0.25, 0.60, 0.95],
            TileDataState::Pending => [1.00, 0.75, 0.10],
            TileDataState::Absent => [0.58, 0.61, 0.68],
            TileDataState::Failed => [0.95, 0.20, 0.22],
            TileDataState::Unknown => [0.65, 0.35, 0.92],
        }
    }

    pub fn fill(self) -> DebugRgba {
        let [r, g, b] = self.rgb();
        [r, g, b, FILL_ALPHA]
    }

    /// The word the legend prints.
    pub fn label(self) -> &'static str {
        match self {
            TileDataState::Loaded => "loaded",
            TileDataState::Decoded => "decoded, no mesh",
            TileDataState::Pending => "pending",
            TileDataState::Absent => "absent -- no data here",
            TileDataState::Failed => "failed",
            TileDataState::Unknown => "unknown",
        }
    }

    /// Every variant, in legend order.
    pub const ALL: [TileDataState; 6] = [
        TileDataState::Loaded,
        TileDataState::Decoded,
        TileDataState::Pending,
        TileDataState::Absent,
        TileDataState::Failed,
        TileDataState::Unknown,
    ];
}

impl TileDrawSource {
    /// The border hue. `Own` returns `None` because a tile drawing its own
    /// data has nothing extra to say -- its border takes the fill's hue at
    /// full alpha, which is what makes a fully-loaded map read as a plain
    /// green grid rather than a two-colour one.
    pub fn border_rgb(self) -> Option<[f32; 3]> {
        match self {
            TileDrawSource::Own => None,
            TileDrawSource::Ancestor { .. } => Some([1.00, 0.45, 0.00]),
            TileDrawSource::Descendant => Some([0.90, 0.25, 0.82]),
            TileDrawSource::Nothing => Some([1.00, 1.00, 1.00]),
        }
    }

    /// The suffix appended to a tile's `z/x/y` label, or `""` for `Own`.
    /// `^2` reads as "two zoom levels up"; `v` as "down".
    pub fn label_suffix(self) -> String {
        match self {
            TileDrawSource::Own => String::new(),
            TileDrawSource::Ancestor { zoom_distance } => format!(" ^{zoom_distance}"),
            TileDrawSource::Descendant => " v".to_string(),
            TileDrawSource::Nothing => " --".to_string(),
        }
    }

    pub fn legend_label(self) -> &'static str {
        match self {
            TileDrawSource::Own => "own mesh",
            TileDrawSource::Ancestor { .. } => "ancestor ^n",
            TileDrawSource::Descendant => "descendant v",
            TileDrawSource::Nothing => "nothing drawn",
        }
    }

    /// Every variant, in legend order. The `Ancestor` sample carries a
    /// placeholder distance; only its colour and legend text are read.
    pub const ALL: [TileDrawSource; 4] = [
        TileDrawSource::Own,
        TileDrawSource::Ancestor { zoom_distance: 1 },
        TileDrawSource::Descendant,
        TileDrawSource::Nothing,
    ];
}

impl TileDebugStatus {
    pub fn fill(&self) -> DebugRgba {
        self.data.fill()
    }

    /// The border: the stand-in's colour where there is one, and the fill's
    /// own hue at full alpha where the tile is drawing itself.
    pub fn border(&self) -> DebugRgba {
        let [r, g, b] = self.draw.border_rgb().unwrap_or_else(|| self.data.rgb());
        [r, g, b, BORDER_ALPHA]
    }

    /// `14/14552/6451`, plus the stand-in marker when one applies.
    pub fn label(&self) -> String {
        format!("{}/{}/{}{}", self.tile.z, self.tile.x, self.tile.y, self.draw.label_suffix())
    }
}

/// A screen-space triangle vertex for the debug pipeline: y-down PHYSICAL
/// pixels, straight sRGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DebugVertex {
    pub position_px: [f32; 2],
    pub color: [f32; 4],
}

/// The four corners of `tile`, projected to y-down LOGICAL window pixels in
/// north-west, north-east, south-east, south-west order.
///
/// `None` when any corner falls at or behind the camera's near plane, which
/// is reachable at any nonzero pitch for a tile beyond the horizon (see
/// [`project_clip_to_screen`](super::project_clip_to_screen)). Skipping such
/// a tile is the honest answer: three of its corners projecting fine tells
/// you nothing about where the fourth belongs, and drawing the quad anyway
/// would put a mirrored rectangle somewhere it does not exist.
///
/// The corners are computed in world space directly from the tile's
/// coordinates rather than via `geo::tile_bounds_lonlat` and back: a tile
/// occupies exactly `WORLD_SIZE / 2^z` world units per side by definition,
/// so the lon/lat round trip would only add two transcendental functions
/// and their error to a quantity that is already exact.
pub fn tile_screen_corners(camera: &Camera, tile: TileId) -> Option<[(f32, f32); 4]> {
    let tile_world_size = WORLD_SIZE as f64 / 2f64.powi(i32::from(tile.z));
    let x0 = f64::from(tile.x) * tile_world_size;
    let y0 = f64::from(tile.y) * tile_world_size;
    let x1 = x0 + tile_world_size;
    let y1 = y0 + tile_world_size;
    let world = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    // Four corners, one matrix: see `project_world_to_screen_f64_with_matrix`.
    let view_projection = camera.view_projection_f64();
    let mut screen = [(0.0f32, 0.0f32); 4];
    for (slot, point) in screen.iter_mut().zip(world) {
        *slot = super::project_world_to_screen_f64_with_matrix(camera, &view_projection, point)?;
    }
    Some(screen)
}

/// Two triangles covering `corners`.
pub fn push_fill(vertices: &mut Vec<DebugVertex>, corners: &[(f32, f32); 4], color: DebugRgba) {
    if color[3] <= 0.0 {
        return;
    }
    let v = |i: usize| DebugVertex { position_px: [corners[i].0, corners[i].1], color };
    vertices.extend_from_slice(&[v(0), v(1), v(2), v(0), v(2), v(3)]);
}

/// A border of `width_px` drawn INSIDE `corners`: one quad per edge, inset
/// along that edge's own inward normal.
///
/// Per-edge normals rather than "pull each corner toward the centroid",
/// which is the obvious shortcut and is wrong for the shape this actually
/// draws: under pitch a tile projects to a strongly trapezoidal quad, and a
/// corner-toward-centroid inset there is mostly along the long axis, which
/// thins the short edges' borders to nothing exactly where the tiles are
/// smallest on screen and the border is the only thing still legible.
///
/// The four quads overlap at the corners. That is why [`BORDER_ALPHA`] is
/// 1.0 -- see its comment.
pub fn push_border(
    vertices: &mut Vec<DebugVertex>,
    corners: &[(f32, f32); 4],
    color: DebugRgba,
    width_px: f32,
) {
    if width_px <= 0.0 || color[3] <= 0.0 {
        return;
    }
    let centroid = (
        corners.iter().map(|c| c.0).sum::<f32>() / 4.0,
        corners.iter().map(|c| c.1).sum::<f32>() / 4.0,
    );
    for i in 0..4 {
        let a = corners[i];
        let b = corners[(i + 1) % 4];
        let (ex, ey) = (b.0 - a.0, b.1 - a.1);
        let len = (ex * ex + ey * ey).sqrt();
        if !len.is_finite() || len < f32::EPSILON {
            continue; // degenerate edge: a tile projected to a sliver
        }
        // Either perpendicular would do; pick the one pointing at the
        // centroid so the border lands inside the rectangle rather than
        // bleeding into the neighbouring tile's.
        let mut nx = -ey / len;
        let mut ny = ex / len;
        let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        if nx * (centroid.0 - mid.0) + ny * (centroid.1 - mid.1) < 0.0 {
            nx = -nx;
            ny = -ny;
        }
        let (ox, oy) = (nx * width_px, ny * width_px);
        let inner_a = (a.0 + ox, a.1 + oy);
        let inner_b = (b.0 + ox, b.1 + oy);
        let v = |p: (f32, f32)| DebugVertex { position_px: [p.0, p.1], color };
        vertices.extend_from_slice(&[v(a), v(b), v(inner_b), v(a), v(inner_b), v(inner_a)]);
    }
}

/// An axis-aligned rectangle in screen pixels -- the legend's backing panel
/// and its colour swatches.
pub fn push_rect(vertices: &mut Vec<DebugVertex>, left: f32, top: f32, width: f32, height: f32, color: DebugRgba) {
    let corners = [
        (left, top),
        (left + width, top),
        (left + width, top + height),
        (left, top + height),
    ];
    push_fill(vertices, &corners, color);
}

/// The axis-aligned screen bounding box of a projected quad, as
/// `(left, top, width, height)`. Used to decide whether a tile is big
/// enough on screen to carry a legible `z/x/y` label, and to place it.
pub fn screen_bounds(corners: &[(f32, f32); 4]) -> (f32, f32, f32, f32) {
    let left = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min);
    let right = corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max);
    let top = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min);
    let bottom = corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max);
    (left, top, right - left, bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn straight_down_camera(zoom: f64) -> Camera {
        Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        }
    }

    /// At zoom 0 the single world tile spans the whole Mercator square, so
    /// its projected corners must bracket the viewport centre symmetrically
    /// -- the cheapest end-to-end check that the world-space corner
    /// derivation and the shared projection agree.
    #[test]
    fn the_zoom_zero_tile_projects_symmetrically_about_the_viewport_centre() {
        let camera = straight_down_camera(1.0);
        let corners = tile_screen_corners(&camera, TileId { z: 0, x: 0, y: 0 }).expect("in front of the camera");
        let (left, top, width, height) = screen_bounds(&corners);
        assert!((left + width / 2.0 - 400.0).abs() < 0.5, "centred horizontally, got {left} + {width}");
        assert!((top + height / 2.0 - 300.0).abs() < 0.5, "centred vertically, got {top} + {height}");
        // One world tile at zoom 1 is half the world, i.e. 2 screen tiles
        // of 512 logical px across the full world -- the tile itself
        // therefore spans 1024 px.
        assert!((width - 1024.0).abs() < 1.0, "width was {width}");
    }

    /// Four sibling tiles must tile the plane with no gap and no overlap:
    /// the right edge of `x` is the left edge of `x + 1`, to the pixel.
    #[test]
    fn sibling_tiles_share_an_edge_exactly() {
        let camera = straight_down_camera(3.0);
        let left_tile = tile_screen_corners(&camera, TileId { z: 3, x: 3, y: 4 }).expect("visible");
        let right_tile = tile_screen_corners(&camera, TileId { z: 3, x: 4, y: 4 }).expect("visible");
        // NE corner of the left tile == NW corner of the right tile.
        assert!((left_tile[1].0 - right_tile[0].0).abs() < 0.01);
        assert!((left_tile[1].1 - right_tile[0].1).abs() < 0.01);
    }

    #[test]
    fn a_fill_is_two_triangles_in_the_requested_colour() {
        let corners = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let mut vertices = Vec::new();
        push_fill(&mut vertices, &corners, [1.0, 0.0, 0.0, 0.5]);
        assert_eq!(vertices.len(), 6);
        assert!(vertices.iter().all(|v| v.color == [1.0, 0.0, 0.0, 0.5]));
    }

    /// The whole reason `push_border` uses per-edge normals: every edge's
    /// quad must be `width_px` thick, including on a quad far from square.
    #[test]
    fn every_edge_of_a_border_is_the_requested_width() {
        // A deliberately elongated quad -- the shape a pitched tile
        // projects to, and the case a corner-toward-centroid inset gets
        // wrong.
        let corners = [(0.0, 0.0), (400.0, 0.0), (400.0, 20.0), (0.0, 20.0)];
        let mut vertices = Vec::new();
        push_border(&mut vertices, &corners, [1.0, 1.0, 1.0, 1.0], 2.0);
        assert_eq!(vertices.len(), 4 * 6, "one quad per edge");

        // Each edge quad's two inner vertices must sit exactly 2 px from
        // its two outer ones. Vertices come out in (a, b, inner_b, a,
        // inner_b, inner_a) order, so slots 0/1 are outer and 2/5 inner.
        for edge in vertices.chunks(6) {
            let outer_a = edge[0].position_px;
            let inner_a = edge[5].position_px;
            let d = ((outer_a[0] - inner_a[0]).powi(2) + (outer_a[1] - inner_a[1]).powi(2)).sqrt();
            assert!((d - 2.0).abs() < 1e-4, "edge inset was {d}, expected 2.0");
        }
    }

    /// A border must land inside its own rectangle, never over the
    /// neighbour's -- otherwise adjacent tiles' borders stack and the grid
    /// reads as one colour.
    #[test]
    fn a_border_is_inset_inwards() {
        let corners = [(100.0, 100.0), (200.0, 100.0), (200.0, 200.0), (100.0, 200.0)];
        let mut vertices = Vec::new();
        push_border(&mut vertices, &corners, [1.0, 1.0, 1.0, 1.0], 3.0);
        for v in &vertices {
            let [x, y] = v.position_px;
            assert!((100.0..=200.0).contains(&x), "x {x} escaped the rectangle");
            assert!((100.0..=200.0).contains(&y), "y {y} escaped the rectangle");
        }
    }

    /// A tile projected to a degenerate sliver (every corner collapsed to
    /// one point) must emit nothing rather than NaNs -- reachable at
    /// extreme pitch, and a NaN vertex takes the whole draw call with it.
    #[test]
    fn a_collapsed_quad_emits_no_border_and_no_nans() {
        let corners = [(50.0, 50.0); 4];
        let mut vertices = Vec::new();
        push_border(&mut vertices, &corners, [1.0, 1.0, 1.0, 1.0], 2.0);
        assert!(vertices.is_empty(), "no finite edge to draw");
    }

    /// `Absent` and `Failed` must not be confusable: most of an unimported
    /// country is absent, and reading that as a fault is the specific
    /// failure this overlay exists to avoid.
    #[test]
    fn absent_is_not_a_shade_of_the_failure_colour() {
        let absent = TileDataState::Absent.rgb();
        let failed = TileDataState::Failed.rgb();
        // Failure is strongly red-dominant; absent is neutral.
        assert!(failed[0] - failed[1] > 0.5, "failed must read as red");
        assert!((absent[0] - absent[2]).abs() < 0.2, "absent must read as neutral");
    }

    /// The border is what distinguishes "loaded" from "looks loaded", so a
    /// stand-in must never borrow the fill's own hue.
    #[test]
    fn a_stand_in_borders_in_its_own_colour_not_the_fills() {
        let loaded_own = TileDebugStatus {
            tile: TileId { z: 1, x: 0, y: 0 },
            data: TileDataState::Loaded,
            draw: TileDrawSource::Own,
        };
        assert_eq!(&loaded_own.border()[..3], &loaded_own.fill()[..3], "own data borders in its own hue");

        let absent_from_ancestor = TileDebugStatus {
            tile: TileId { z: 1, x: 0, y: 0 },
            data: TileDataState::Absent,
            draw: TileDrawSource::Ancestor { zoom_distance: 2 },
        };
        assert_ne!(
            &absent_from_ancestor.border()[..3],
            &absent_from_ancestor.fill()[..3],
            "a stand-in must be visible as one"
        );
        assert_eq!(absent_from_ancestor.label(), "1/0/0 ^2");
    }

    #[test]
    fn a_descendant_stand_in_is_marked_in_the_label() {
        let status = TileDebugStatus {
            tile: TileId { z: 12, x: 3637, y: 1612 },
            data: TileDataState::Pending,
            draw: TileDrawSource::Descendant,
        };
        assert_eq!(status.label(), "12/3637/1612 v");
    }
}
