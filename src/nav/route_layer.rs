//! The route on the map: the part still to drive in blue over a darker
//! casing, the part already driven in grey, split where the puck is, and a
//! short arrow across the next maneuver once it is close. Plus the two
//! things the rest of the frame needs from the line: collision boxes, so a
//! road label never sits on it, and where the puck goes on screen.
//!
//! The line is a `style::PaintPlan` in an `overlay::geom::OverlayOrigin`'s
//! local grid, tessellated at `render::mesh::OVERLAY_COORD_SCALE` and drawn
//! through `render::combined_overlay_matrix` -- the runtime overlay path, and
//! for the same reason: coordinates relative to an origin at the route's
//! centre, never absolute Mercator, which is what
//! `docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md`
//! found collapses stroke normals to zero at high zoom.

use crate::collision::CollisionBox;
use crate::nav::route_source::NavRoute;
use crate::nav::simulate::metres_between;
use crate::nav::view_model::{NavigationState, Puck, TripPhase};
use crate::overlay::geom::{lon_lat_to_world, OverlayOrigin};
use crate::render::Camera;
use crate::style::{DashPattern, LineCap, LineJoin, PaintOp, PaintPlan, Rgba};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Every plan any layer builds gets a number no other plan has, so a GPU
/// copy keyed on it can never mistake a new trip's first mesh for the last
/// trip's.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

pub const REMAINING_COLOR: Rgba = Rgba { r: 0.165, g: 0.490, b: 0.941, a: 1.0 };
pub const REMAINING_CASING_COLOR: Rgba = Rgba { r: 0.078, g: 0.302, b: 0.643, a: 1.0 };
pub const DRIVEN_COLOR: Rgba = Rgba { r: 0.627, g: 0.655, b: 0.690, a: 1.0 };
pub const DRIVEN_CASING_COLOR: Rgba = Rgba { r: 0.459, g: 0.486, b: 0.522, a: 1.0 };
pub const ARROW_COLOR: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
pub const ARROW_CASING_COLOR: Rgba = REMAINING_CASING_COLOR;
/// Logical pixels.
pub const LINE_WIDTH_PX: f32 = 8.0;
pub const CASING_WIDTH_PX: f32 = 12.0;
/// The maneuver arrow's shaft is the route's own width over the route's
/// own casing colour, so shaft, head and route read as one drawing
/// (MapLibre Navigation Android's maneuver arrow: a shaft over the route
/// with a fixed-size head icon at its end).
pub const ARROW_WIDTH_PX: f32 = LINE_WIDTH_PX;
pub const ARROW_CASING_WIDTH_PX: f32 = CASING_WIDTH_PX;
/// The arrowhead's width across its base, casing included: twice the
/// shaft's, in screen pixels at every zoom (it is a screen-space icon, as
/// the nav SDKs draw it, so it never balloons to a car's length at a
/// close zoom nor shrinks to a speck far out).
pub const ARROW_HEAD_WIDTH_PX: f32 = 2.0 * ARROW_CASING_WIDTH_PX;
/// The head points along the shaft from this far back from its end, metres:
/// the last few metres' bearing, robust to a vertex a centimetre short.
pub const ARROW_HEAD_BEARING_M: f64 = 3.0;
/// The maneuver arrow shows inside this distance of the maneuver.
pub const MANEUVER_ARROW_SHOW_M: f64 = 150.0;
/// ...and spans this much road before and after it.
pub const MANEUVER_ARROW_BEFORE_M: f64 = 25.0;
pub const MANEUVER_ARROW_AFTER_M: f64 = 20.0;
/// The split is only moved (and the mesh rebuilt) once the puck has moved
/// this far along the line.
pub const REBUILD_STEP_M: f64 = 0.5;
/// One collision box every this many logical pixels of on-screen route.
pub const COLLISION_SAMPLE_PX: f32 = 10.0;
/// Clear space kept between the line's casing and any label.
pub const LABEL_CLEARANCE_PX: f32 = 2.0;
/// A pathological zoomed-out frame could otherwise ask for a box per pixel.
pub const MAX_COLLISION_BOXES: usize = 4_096;

pub struct RouteLayer {
    origin: OverlayOrigin,
    /// World coordinates of the overview line, and the metres along it at
    /// each vertex.
    world: Vec<(f64, f64)>,
    along_m: Vec<f64>,
    /// Metres along the line of each step's maneuver point.
    maneuver_m: Vec<f64>,
    /// Where driven meets remaining, metres along.
    split_m: f64,
    arrow_step: Option<usize>,
    /// The shaft's end and the point [`ARROW_HEAD_BEARING_M`] back along
    /// it, world units: where the head sits and which way it points.
    arrow_tail: Option<((f64, f64), (f64, f64))>,
    route_generation: u64,
    plan: PaintPlan,
    generation: u64,
}

impl RouteLayer {
    #[must_use]
    pub fn new(route: &NavRoute, route_generation: u64) -> Self {
        let lon_lat = route.line();
        let world: Vec<(f64, f64)> = lon_lat.iter().map(|&(lon, lat)| lon_lat_to_world(lon, lat)).collect();
        let mut along_m = Vec::with_capacity(lon_lat.len());
        let mut total = 0.0;
        for (index, point) in lon_lat.iter().enumerate() {
            if index > 0 {
                total += metres_between(lon_lat[index - 1], *point);
            }
            along_m.push(total);
        }
        let origin = OverlayOrigin::centred_on(world.iter().copied()).unwrap_or(OverlayOrigin::at(0.0, 0.0));
        let mut layer = Self {
            origin,
            world,
            along_m,
            maneuver_m: Vec::new(),
            split_m: 0.0,
            arrow_step: None,
            arrow_tail: None,
            route_generation,
            plan: PaintPlan { ops: Vec::new() },
            generation: 0,
        };
        let mut from = 0;
        layer.maneuver_m = route
            .steps
            .iter()
            .map(|step| {
                let Some([lon, lat]) = step.maneuver.location else { return layer.along_m.get(from).copied().unwrap_or(0.0) };
                let (segment, at_m) = layer.project((lon, lat), from, usize::MAX);
                from = segment;
                at_m
            })
            .collect();
        layer.rebuild();
        layer
    }

    /// Follows `state`: moves the split to the puck and shows or hides the
    /// maneuver arrow. Rebuilds the plan (and bumps [`Self::generation`])
    /// only when something visible changed.
    pub fn update(&mut self, route: &NavRoute, route_generation: u64, state: &NavigationState) {
        if route_generation != self.route_generation {
            *self = Self::new(route, route_generation);
        }
        let mut changed = false;
        if let Some(puck) = state.puck.filter(|p| p.on_route) {
            // Only the current step's stretch of line is searched, so a
            // route that doubles back (a U-turn) never snaps the split onto
            // the other carriageway.
            let first = self.segment_at(self.maneuver_m.get(state.step_index).copied().unwrap_or(0.0)).saturating_sub(1);
            let last = self.segment_at(self.maneuver_m.get(state.step_index + 1).copied().unwrap_or_else(|| self.length_m())) + 1;
            let (_, at_m) = self.project((puck.lon, puck.lat), first, last.saturating_sub(first).max(1));
            if (at_m - self.split_m).abs() >= REBUILD_STEP_M {
                self.split_m = at_m;
                changed = true;
            }
        }
        let upcoming = state.step_index + 1;
        let arrow = (state.phase == TripPhase::Navigating
            && state.maneuver.is_some()
            && state.distance_to_maneuver_m <= MANEUVER_ARROW_SHOW_M
            && upcoming + 1 < route.steps.len())
        .then_some(upcoming);
        if arrow != self.arrow_step {
            self.arrow_step = arrow;
            changed = true;
        }
        if changed {
            self.rebuild();
        }
    }

    #[must_use]
    pub fn plan(&self) -> &PaintPlan {
        &self.plan
    }

    /// The overlay origin, in world units: what `combined_overlay_matrix` takes.
    #[must_use]
    pub fn origin_world(&self) -> (f64, f64) {
        (self.origin.world_x, self.origin.world_y)
    }

    /// The coord scale [`Self::plan`] is tessellated at: its origin's
    /// (`overlay::geom::OverlayOrigin::coord_scale`).
    #[must_use]
    pub fn coord_scale(&self) -> f32 {
        self.origin.coord_scale
    }

    /// Bumped every time [`Self::plan`] changes, so a GPU copy knows when to
    /// re-tessellate.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn arrow_visible(&self) -> bool {
        self.arrow_step.is_some()
    }

    /// Where the maneuver arrow's head goes on screen this frame: centred
    /// on the shaft's end, turned clockwise (y-down, from straight up) to
    /// the shaft's last few metres. `None` with no arrow, or with the
    /// shaft's end off the visible globe. A pure function of the route,
    /// the progress and the camera.
    #[must_use]
    pub fn arrow_head_placement(&self, camera: &Camera) -> Option<ArrowHeadPlacement> {
        let (back, end) = self.arrow_tail?;
        let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(camera));
        let (x, y) = crate::render::globe::project_world_to_screen_on_frame(camera, &anchors, end)?;
        let (bx, by) = crate::render::globe::project_world_to_screen_on_frame(camera, &anchors, back)?;
        Some(ArrowHeadPlacement { x, y, rotation_rad: (x - bx).atan2(by - y) })
    }

    /// The shaft's end, world units (for tests that check the head sits on it).
    #[cfg(test)]
    pub(crate) fn arrow_shaft_end_world(&self) -> Option<(f64, f64)> {
        self.arrow_tail.map(|(_, end)| end)
    }

    #[must_use]
    pub fn split_m(&self) -> f64 {
        self.split_m
    }

    #[must_use]
    pub fn length_m(&self) -> f64 {
        self.along_m.last().copied().unwrap_or(0.0)
    }

    /// The whole route's world-space bounds `(min_x, min_y, max_x,
    /// max_y)`, driven part included: what the overview fits.
    #[must_use]
    pub fn bounds_world(&self) -> Option<(f64, f64, f64, f64)> {
        let first = *self.world.first()?;
        Some(self.world.iter().fold((first.0, first.1, first.0, first.1), |b, p| (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1))))
    }

    /// World-space `(min_x, min_y, max_x, max_y)` of the part still to drive.
    #[must_use]
    pub fn remaining_bounds_world(&self) -> Option<(f64, f64, f64, f64)> {
        let points = self.remaining_world();
        let first = *points.first()?;
        Some(points.iter().fold((first.0, first.1, first.0, first.1), |b, p| (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1))))
    }

    /// Physical-pixel boxes along the part still to drive, for
    /// `CollisionIndex::insert` before labels are placed. Each is the
    /// casing's width plus the clearance, every [`COLLISION_SAMPLE_PX`].
    #[must_use]
    pub fn collision_boxes(&self, camera: &Camera, scale_factor: f32) -> Vec<CollisionBox> {
        let side = (CASING_WIDTH_PX + 2.0 * LABEL_CLEARANCE_PX) * scale_factor;
        let (w, h) = (camera.viewport_width_px * scale_factor, camera.viewport_height_px * scale_factor);
        // On the sphere when the frame is, as the line itself is drawn.
        let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(camera));
        let screen: Vec<Option<(f32, f32)>> = self
            .remaining_world()
            .iter()
            .map(|&p| {
                crate::render::globe::project_world_to_screen_on_frame(camera, &anchors, p)
                    .map(|(x, y)| (x * scale_factor, y * scale_factor))
            })
            .collect();
        let step = COLLISION_SAMPLE_PX * scale_factor;
        let mut boxes = Vec::new();
        for pair in screen.windows(2) {
            let (Some(a), Some(b)) = (pair[0], pair[1]) else { continue };
            let length = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            let samples = (length / step).ceil().max(1.0) as usize;
            for i in 0..=samples {
                let t = i as f32 / samples as f32;
                let (x, y) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                if x < -side || y < -side || x > w + side || y > h + side {
                    continue;
                }
                boxes.push(CollisionBox::from_origin_size(x - side / 2.0, y - side / 2.0, side, side));
                if boxes.len() >= MAX_COLLISION_BOXES {
                    return boxes;
                }
            }
        }
        boxes
    }

    /// `(segment, metres along)` of the closest point on the line to `point`,
    /// searching `window` segments from `from`.
    fn project(&self, point: (f64, f64), from: usize, window: usize) -> (usize, f64) {
        let target = lon_lat_to_world(point.0, point.1);
        let last = self.world.len().saturating_sub(1);
        let end = from.saturating_add(window).min(last);
        let mut best = (from.min(last), self.along_m.get(from.min(last)).copied().unwrap_or(0.0), f64::INFINITY);
        for segment in from.min(last)..end {
            let (a, b) = (self.world[segment], self.world[segment + 1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 { (((target.0 - a.0) * dx + (target.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
            let (px, py) = (a.0 + dx * t, a.1 + dy * t);
            let d2 = (px - target.0).powi(2) + (py - target.1).powi(2);
            if d2 < best.2 {
                let along = self.along_m[segment] + (self.along_m[segment + 1] - self.along_m[segment]) * t;
                best = (segment, along, d2);
            }
        }
        (best.0, best.1)
    }

    /// The segment `at_m` metres along falls on.
    fn segment_at(&self, at_m: f64) -> usize {
        let last = self.world.len().saturating_sub(1);
        self.along_m.partition_point(|m| *m <= at_m.clamp(0.0, self.length_m())).saturating_sub(1).min(last.saturating_sub(1))
    }

    /// The world point `at_m` metres along, and the segment it is on.
    fn point_at(&self, at_m: f64) -> ((f64, f64), usize) {
        let last = self.world.len().saturating_sub(1);
        let at_m = at_m.clamp(0.0, self.length_m());
        let segment = self.segment_at(at_m);
        let (m0, m1) = (self.along_m[segment], self.along_m[(segment + 1).min(last)]);
        let t = if m1 > m0 { (at_m - m0) / (m1 - m0) } else { 0.0 };
        let (a, b) = (self.world[segment], self.world[(segment + 1).min(last)]);
        ((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t), segment)
    }

    /// World points from `from_m` to `to_m` along the line.
    fn slice(&self, from_m: f64, to_m: f64) -> Vec<(f64, f64)> {
        if self.world.len() < 2 || to_m <= from_m {
            return Vec::new();
        }
        let (start, first) = self.point_at(from_m);
        let (end, last) = self.point_at(to_m);
        let mut points = vec![start];
        points.extend(self.world[first + 1..=last].iter().copied());
        points.push(end);
        points.dedup();
        points
    }

    /// The world point `at_m` metres along the line, for render tests.
    #[cfg(test)]
    pub(crate) fn world_at(&self, at_m: f64) -> (f64, f64) {
        self.point_at(at_m).0
    }

    fn remaining_world(&self) -> Vec<(f64, f64)> {
        self.slice(self.split_m, self.length_m())
    }

    fn rebuild(&mut self) {
        let mut ops = Vec::new();
        let driven = self.slice(0.0, self.split_m);
        let remaining = self.remaining_world();
        self.push_line(&mut ops, &driven, DRIVEN_CASING_COLOR, CASING_WIDTH_PX);
        self.push_line(&mut ops, &driven, DRIVEN_COLOR, LINE_WIDTH_PX);
        self.push_line(&mut ops, &remaining, REMAINING_CASING_COLOR, CASING_WIDTH_PX);
        self.push_line(&mut ops, &remaining, REMAINING_COLOR, LINE_WIDTH_PX);
        self.arrow_tail = None;
        if let Some(step) = self.arrow_step {
            let at = self.maneuver_m[step];
            let (from_m, to_m) = ((at - MANEUVER_ARROW_BEFORE_M).max(self.split_m), (at + MANEUVER_ARROW_AFTER_M).min(self.length_m()));
            let shaft = self.slice(from_m, to_m);
            if shaft.len() >= 2 {
                // The shaft: route casing colour under white, the route's
                // own widths. The head is a screen-space icon at its end
                // (`arrow_head_placement`), drawn by the chrome.
                self.push_line(&mut ops, &shaft, ARROW_CASING_COLOR, ARROW_CASING_WIDTH_PX);
                self.push_line(&mut ops, &shaft, ARROW_COLOR, ARROW_WIDTH_PX);
                let end = shaft[shaft.len() - 1];
                let back = self.point_at((to_m - ARROW_HEAD_BEARING_M).max(from_m)).0;
                self.arrow_tail = Some((back, end));
            }
        }
        self.plan = PaintPlan { ops };
        self.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    fn local(&self, points: &[(f64, f64)]) -> Vec<(i32, i32)> {
        let mut local: Vec<(i32, i32)> = points.iter().map(|&p| self.origin.to_local(p)).collect();
        local.dedup();
        local
    }

    fn push_line(&self, ops: &mut Vec<PaintOp>, points: &[(f64, f64)], color: Rgba, width: f32) {
        let local = self.local(points);
        if local.len() < 2 {
            return;
        }
        ops.push(PaintOp::Line {
            metrics: false,
            color_from: color,
            color_to: color,
            width_from: width,
            width_to: width,
            gap_from: 0.0,
            gap_to: 0.0,
            cap: LineCap::Round,
            join: LineJoin::Round,
            dash: DashPattern::none(),
            geometry: Arc::new(geo_types::Geometry::LineString(geo_types::LineString::from(local))),
        });
    }
}

/// Where the maneuver arrow's head sits on screen (logical pixels) and how
/// it is turned: clockwise, y-down, from straight up -- the chrome icon's
/// own convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowHeadPlacement {
    pub x: f32,
    pub y: f32,
    pub rotation_rad: f32,
}

/// Where the puck sits on screen (logical pixels) and how it is turned:
/// clockwise, y-down, from "straight up", as `IconInstance` wants. A course
/// grows clockwise from north and north is at `-bearing` on screen (see
/// `compass::needle_rotation_rad`), so it is the sum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PuckPlacement {
    pub x: f32,
    pub y: f32,
    pub rotation_rad: f32,
    pub muted: bool,
}

#[must_use]
pub fn puck_placement(camera: &Camera, puck: &Puck) -> Option<PuckPlacement> {
    // On the sphere when the frame is, like the route line under it; hidden
    // (`None`) when the planet is in the way.
    let anchors = crate::render::globe::GlobeAnchors::new(&crate::render::globe::globe_frame(camera));
    let (x, y) = crate::render::globe::project_world_to_screen_on_frame(
        camera,
        &anchors,
        lon_lat_to_world(puck.lon, puck.lat),
    )?;
    Some(PuckPlacement {
        x,
        y,
        rotation_rad: crate::compass::needle_rotation_rad(camera.bearing) + (puck.course_deg as f32).to_radians(),
        muted: !puck.on_route,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::format::WallClock;
    use crate::nav::route_source::parse_route;
    use crate::nav::simulate::SimOptions;
    use crate::nav::trip::{NavTrip, TripClock};

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");
    const CLOCK: TripClock = TripClock { now_ms: 0.0, wall: WallClock { unix_ms: 0.0, utc_offset_min: 480 } };

    fn trip_at(metres: f64) -> NavTrip {
        let mut trip = NavTrip::simulated(
            parse_route(MANILA).unwrap(),
            SimOptions { start_at_m: Some(metres), paused: true, ..SimOptions::default() },
            CLOCK,
        )
        .unwrap();
        trip.advance(CLOCK);
        trip
    }

    fn layer_for(trip: &NavTrip) -> RouteLayer {
        let route = trip.session().route();
        let mut layer = RouteLayer::new(route, 0);
        layer.update(route, 0, trip.state());
        layer
    }

    fn camera_on(lon: f64, lat: f64, zoom: f64) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 800.0, viewport_height_px: 600.0, fov_y: Camera::DEFAULT_FOV_Y }
    }

    #[test]
    fn the_line_splits_at_the_puck_into_driven_and_remaining() {
        let trip = trip_at(1_200.0);
        let layer = layer_for(&trip);
        assert!((layer.length_m() - 3_211.7).abs() < 40.0, "the overview line is the route: {}", layer.length_m());
        assert!((layer.split_m() - 1_200.0).abs() < 25.0, "split near 1.2 km: {}", layer.split_m());
        let colors: Vec<Rgba> = layer
            .plan()
            .ops
            .iter()
            .map(|op| match op {
                PaintOp::Line { color_from, .. } | PaintOp::Fill { color_from, .. } => *color_from,
                PaintOp::Background { color } => *color,
                PaintOp::Extrusion { row, .. } => row.color_from,
            })
            .collect();
        assert_eq!(colors, [DRIVEN_CASING_COLOR, DRIVEN_COLOR, REMAINING_CASING_COLOR, REMAINING_COLOR], "no arrow 500 m out");
        assert!(!layer.arrow_visible());
    }

    #[test]
    fn the_maneuver_arrow_appears_inside_150_m() {
        // Step 0 ends at 171.7 m in the slight left; 60 m in is 112 m out.
        let near = layer_for(&trip_at(60.0));
        assert!(near.arrow_visible());
        assert_eq!(near.plan().ops.len(), 6, "four route lines, then the shaft's casing and the shaft");
        assert!(matches!(near.plan().ops[5], PaintOp::Line { color_from, .. } if color_from == ARROW_COLOR));
        assert!(!layer_for(&trip_at(0.0)).arrow_visible(), "171.7 m out is past the 150 m threshold");
    }

    /// The user's U-turn screenshot ("maybe arrow can be better"): the head
    /// was a world-space chevron ~9x the route's width, ~80 px past the
    /// shaft's end, its casing only on the V's two sides. Now: the shaft is
    /// the route's width over the route's casing colour and nothing else in
    /// the plan; the head is a screen-space icon centred ON the shaft's end,
    /// turned to the shaft's last metres, twice the shaft's width.
    #[test]
    fn the_maneuver_arrow_is_one_shape_with_its_head_on_the_shaft_end() {
        // 650 m in: 85 m before the U-turn onto Finance Road (735.5 m).
        let trip = trip_at(650.0);
        let near = layer_for(&trip);
        assert!(near.arrow_visible());
        let ops = &near.plan().ops;
        assert!(ops.iter().all(|op| matches!(op, PaintOp::Line { .. })), "no world-space head: every op is a line");
        let PaintOp::Line { color_from, width_from, .. } = &ops[4] else { unreachable!() };
        assert_eq!((*color_from, *width_from), (REMAINING_CASING_COLOR, CASING_WIDTH_PX), "the shaft's casing is the route's");
        let PaintOp::Line { color_from, width_from, .. } = &ops[5] else { unreachable!() };
        assert_eq!((*color_from, *width_from), (ARROW_COLOR, LINE_WIDTH_PX));
        assert!((ARROW_HEAD_WIDTH_PX / ARROW_CASING_WIDTH_PX - 2.0).abs() < 1e-6, "the head is twice the shaft");
        let puck = trip.state().puck.unwrap();
        for (zoom, bearing) in [(16.0, 0.0f32), (18.0, 0.0), (17.0, 40.0)] {
            let mut camera = camera_on(puck.lon, puck.lat, zoom);
            camera.bearing = bearing.to_radians();
            let head = near.arrow_head_placement(&camera).expect("the head is on screen");
            let end = near.arrow_shaft_end_world().unwrap();
            let (ex, ey) = crate::render::project_world_to_screen_f64(&camera, end).unwrap();
            assert!((head.x - ex).abs() < 0.5 && (head.y - ey).abs() < 0.5, "z{zoom}: the head sits on the shaft's end");
            // The shaft's last segment on screen, and the head's direction.
            let (back, _) = near.arrow_tail.unwrap();
            let (bx, by) = crate::render::project_world_to_screen_f64(&camera, back).unwrap();
            let shaft_dir = (ex - bx, ey - by);
            let head_dir = (head.rotation_rad.sin(), -head.rotation_rad.cos());
            let cos = (shaft_dir.0 * head_dir.0 + shaft_dir.1 * head_dir.1) / shaft_dir.0.hypot(shaft_dir.1);
            assert!(cos > 0.999, "z{zoom} bearing {bearing}: the head points along the shaft (cos {cos})");
        }
        assert!(layer_for(&trip_at(0.0)).arrow_head_placement(&camera_on(puck.lon, puck.lat, 16.0)).is_none(), "no arrow, no head");
    }

    #[test]
    fn nothing_rebuilds_while_the_puck_is_still() {
        let trip = trip_at(500.0);
        let mut layer = layer_for(&trip);
        let generation = layer.generation();
        layer.update(trip.session().route(), 0, trip.state());
        assert_eq!(layer.generation(), generation);
        layer.update(trip.session().route(), 1, trip.state());
        assert_ne!(layer.generation(), generation, "a new route rebuilds from scratch");
        assert_ne!(RouteLayer::new(trip.session().route(), 0).generation(), generation, "and no two plans share a number");
    }

    #[test]
    fn at_street_zoom_no_stroke_normal_collapses() {
        // The overlay-stroke-precision failure mode: normals of zero length.
        let layer = layer_for(&trip_at(60.0));
        let mesh = crate::render::tessellate_plan(
            layer.plan(),
            1,
            crate::render::mesh::OVERLAY_COORD_SCALE,
            crate::render::subdivide::Subdivision::NONE,
        );
        let strokes: Vec<_> = mesh.vertices.iter().filter(|v| v.is_stroke > 0.5).collect();
        assert!(strokes.len() > 100, "{}", strokes.len());
        let collapsed = strokes.iter().filter(|v| v.normal[0].hypot(v.normal[1]) < 0.9).count();
        assert_eq!(collapsed, 0, "{collapsed} of {} stroke normals collapsed", strokes.len());
    }

    #[test]
    fn collision_boxes_cover_the_visible_remaining_route_and_nothing_off_screen() {
        let trip = trip_at(1_200.0);
        let layer = layer_for(&trip);
        let puck = trip.state().puck.unwrap();
        let camera = camera_on(puck.lon, puck.lat, 16.0);
        let boxes = layer.collision_boxes(&camera, 2.0);
        assert!(boxes.len() > 20, "{}", boxes.len());
        let side = (CASING_WIDTH_PX + 2.0 * LABEL_CLEARANCE_PX) * 2.0;
        let centre = CollisionBox::from_origin_size(800.0 - side / 2.0, 600.0 - side / 2.0, side, side);
        assert!(boxes.iter().any(|b| b.intersects(&centre)), "a box sits under the puck at the screen centre");
        assert!(layer.collision_boxes(&camera_on(103.8, 1.35, 16.0), 2.0).is_empty(), "Singapore shows no Manila route");
    }

    #[test]
    fn a_puck_heading_where_the_camera_faces_points_straight_up() {
        let puck = Puck { lon: 120.98, lat: 14.59, course_deg: 90.0, on_route: false };
        let mut camera = camera_on(120.98, 14.59, 16.0);
        camera.bearing = 90f32.to_radians();
        let placed = puck_placement(&camera, &puck).unwrap();
        assert!((placed.x - 400.0).abs() < 0.5 && (placed.y - 300.0).abs() < 0.5);
        assert!(placed.rotation_rad.abs() < 1e-5, "{}", placed.rotation_rad);
        assert!(placed.muted);
        camera.bearing = 0.0;
        assert!((puck_placement(&camera, &puck).unwrap().rotation_rad - std::f32::consts::FRAC_PI_2).abs() < 1e-5, "east on a north-up map is a quarter turn clockwise");
    }

    #[test]
    fn a_u_turn_does_not_snap_the_split_onto_the_other_carriageway() {
        // Finance Road runs out to the U-turn (step 4 starts at 735.5 m) and
        // straight back over the same street: 25 m past it, a nearest-line
        // search anywhere on the route would find the outbound carriageway.
        let trip = trip_at(760.0);
        assert_eq!(trip.state().step_index, 4, "past the U-turn");
        let layer = layer_for(&trip);
        assert!(
            layer.split_m() >= layer.maneuver_m[4],
            "the split at {} m is behind the U-turn at {} m",
            layer.split_m(),
            layer.maneuver_m[4]
        );
    }

    #[test]
    fn a_puck_between_the_carriageways_past_the_u_turn_stays_on_the_return_one() {
        // Finance Road is divided: the two carriageways run ~10 m apart, and
        // Ferrostar's snapped puck sits on the return one, so the test above
        // passes with a whole-route search too. What a whole-route search gets
        // wrong is a puck that lands nearer the outbound carriageway -- a glide
        // interpolated across the U-turn, or a fix drifted sideways. Moved 70%
        // of the way across, it must still split past the U-turn.
        let trip = trip_at(760.0);
        let route = trip.session().route();
        let mut layer = RouteLayer::new(route, 0);
        let puck = trip.state().puck.unwrap();
        let outbound_first = layer.segment_at(layer.maneuver_m[3]);
        let outbound_len = layer.segment_at(layer.maneuver_m[4]) + 1 - outbound_first;
        let (_, outbound_m) = layer.project((puck.lon, puck.lat), outbound_first, outbound_len);
        let (outbound, _) = layer.point_at(outbound_m);
        let (o_lon, o_lat) = crate::camera_control::lon_lat_at_world(glam::DVec3::new(outbound.0, outbound.1, 0.0)).unwrap();
        let mut state = trip.state().clone();
        state.puck = Some(Puck { lon: puck.lon + (o_lon - puck.lon) * 0.7, lat: puck.lat + (o_lat - puck.lat) * 0.7, ..puck });
        assert!(outbound_m < layer.maneuver_m[4], "the outbound point is before the U-turn: {outbound_m}");
        layer.update(route, 0, &state);
        assert!(
            layer.split_m() >= layer.maneuver_m[4],
            "the split at {} m jumped back onto the outbound carriageway (U-turn at {} m)",
            layer.split_m(),
            layer.maneuver_m[4]
        );
    }
}
