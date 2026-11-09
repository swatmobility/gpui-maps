//! A story script's camera segments resolved once into a path, and the
//! camera pose on it as a pure function of story time (spec §3, plan S2.2).
//!
//! **Exact and reproducible.** [`CameraPath::pose_at`] takes `t` as f64
//! story seconds and reads nothing else: no clock, no accumulated
//! `Duration`, no state from the previous call. The same `t` always gives
//! a bit-identical pose, so frame `n` of a render is `pose_at(n / fps)`
//! whether it is rendered first, last or alone.
//!
//! The arithmetic is not re-derived here. Each segment's end pose is
//! [`resolve_pose`] (the same rules `CameraController::jump_to` and
//! `ease_to` apply to a live camera), and an `ease` segment's in-between
//! pose is [`ease_pose`], the function `ease_to` itself advances through.
//!
//! A `fly` segment is planned once by [`super::fly::plan`] against the
//! output viewport (from [`ResolveCtx`]) and evaluated by
//! [`super::fly::pose`] at the eased fraction.
//!
//! A `follow` segment ([`FollowPlan`]) puts the camera on a track from
//! [`super::tracks`] at every `t`: the centre is the track's position at
//! story time `t` (the first position before the track starts, the last
//! after it ends), and with `bearing: "track"` the bearing is its heading.
//! Each pose is [`resolve_pose`] of that target over the pose the segment
//! started from, so the follow is as exact as a `jump`. [`CameraPath`]
//! keeps the tracks behind an `Arc` (their samples are shared, never
//! copied), so `pose_at(t)` needs no `tracks` argument (the spec's
//! `pose_at(t, tracks)`, narrowed by ruling).
//!
//! Like `script.rs`, this module denies `unwrap`, `expect` and indexing
//! outside tests (spec §2, "No-panic contract").
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use super::script::{Diagnostic, Easing, PoseSpec, Script, Segment, Severity};
use super::script::FollowBearing;
use super::tiers::Rect;
use super::tracks::{Track, TrackSet, DRAW_TRACK_PREFIX};
use std::sync::Arc;
use crate::camera_control::{
    ease_pose, lon_lat_at_world, radians_from_degrees, resolve_pose, wrap_bearing, CameraTarget, MAX_MERCATOR_LAT, MAX_ZOOM,
    MIN_ZOOM,
};
use crate::render::globe::{
    build_globe_frame, center_placing_world_at, globe_active, mercator_to_sphere, project_world_to_screen_on_frame, GlobeAnchors,
};
use crate::render::Camera;

/// A camera pose plus whether the globe projection is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub camera: Camera,
    pub globe: bool,
}

/// One camera segment, resolved: its place on the timeline and its end
/// poses, in story seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSegment {
    pub label: Option<String>,
    /// Story seconds at which this segment begins.
    pub start: f64,
    /// Seconds. Zero for a `jump`.
    pub duration: f64,
    pub from: Pose,
    /// The pose at the segment's end. For an `orbit`, `to.camera.bearing`
    /// is deliberately NOT wrapped (`from.bearing + degrees`), so the plan
    /// records how far it turns; [`CameraPath::pose_at`] wraps its output.
    pub to: Pose,
    pub kind: Kind,
    /// `true` for a `hold` with `settle: true`: its first frame is a
    /// settle point (spec §4.3).
    pub settle: bool,
}

/// How a segment moves from `from` to `to`.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Instantaneous: duration zero, the pose is `to` from its start on.
    Jump,
    /// `to == from` for the whole duration.
    Hold,
    /// [`ease_pose`] at the eased fraction.
    Ease(Easing),
    /// The bearing turns by `degrees` (any amount, 540 is one and a half
    /// turns; negative turns the other way). The centre stays put, or with
    /// `around` (lon, lat) moves so that point stays at the same place on
    /// screen: rotated about it in flat world space on a flat frame, and on
    /// the sphere about its radial axis on a globe frame (spec §3.1).
    Orbit { degrees: f64, easing: Easing, around: Option<(f64, f64)> },
    /// The van Wijk flight ([`super::fly`]) at the eased fraction. A
    /// zero-duration fly (`max_duration` exceeded) is complete at its
    /// start, like a `jump`.
    Fly(super::fly::FlyPlan),
    /// The camera on a track, sampled at story time `t` (not at the eased
    /// fraction): see [`follow_pose`].
    Follow(FollowPlan),
}

/// A resolved `follow` segment. `from` and `to` of its
/// [`ResolvedSegment`] are [`follow_pose`] at the segment's start and end,
/// so the camera cuts to the track as the segment begins.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowPlan {
    /// Index of the track in the path's [`TrackSet`].
    pub track: usize,
    /// The pose before the segment: whatever the follow leaves unset
    /// (zoom, pitch, bearing without `bearing`) comes from here.
    pub base: Camera,
    pub globe: bool,
    pub zoom: Option<f64>,
    /// Radians.
    pub pitch: Option<f32>,
    pub bearing: FollowHeading,
    /// Where the track's point sits on screen, from the viewport centre.
    pub offset_px: (f32, f32),
}

/// A `follow` segment's bearing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FollowHeading {
    /// The track's heading at `t` ([`Track::heading_at`]); the base
    /// bearing for a track that never moves.
    Track,
    /// Radians.
    Fixed(f32),
    /// No `bearing`: the base bearing throughout.
    Keep,
}

/// The follow camera at story time `t` on `track`: pure in `t`.
fn follow_pose(plan: &FollowPlan, track: &Track, t: f64) -> Camera {
    let bearing = match plan.bearing {
        FollowHeading::Track => track.heading_at(t),
        FollowHeading::Fixed(b) => Some(b),
        FollowHeading::Keep => None,
    };
    let target = CameraTarget {
        center: Some(track.position_clamped(t)),
        zoom: plan.zoom,
        pitch: plan.pitch,
        bearing,
        anchor_offset_px: plan.offset_px,
    };
    resolve_pose(&plan.base, &target, (MIN_ZOOM, MAX_ZOOM), plan.globe)
}

/// The default easing for `ease` and `orbit` segments that name none
/// (spec §2.1: `easing?` defaults to `"ease"`).
const DEFAULT_EASING: Easing = Easing::Ease;

/// One card's camera box (spec §5.3, §5.7) while it is on screen,
/// `from..to` story seconds: the box the card leaves the subject, in the
/// frame's units (`frame_layout::layout_card`'s `camera_box`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardBox {
    pub from: f64,
    pub to: f64,
    pub camera_box: Rect,
}

/// Every card's camera box over a story, for
/// [`CameraPath::with_card_boxes`].
#[derive(Clone, Debug, PartialEq)]
pub struct CardBoxes {
    pub boxes: Vec<CardBox>,
    /// The box with no card on screen: above the fine print.
    pub none: Rect,
    /// The frame the boxes are laid out on, units.
    pub frame: (f32, f32),
    /// The path's viewport pixels per frame unit: `anchor_offset_px` is
    /// in the viewport's logical pixels.
    pub px_per_unit: f32,
}

impl CardBoxes {
    /// The box a segment spanning `start..end` (pass 1's seconds) centres
    /// its pose in (spec §5.3): of the cards visible during it, the one
    /// with the smallest `camera_box.3` -- the highest card top -- the
    /// first of equals; [`Self::none`] when no card is. Visible during
    /// `start..end` is `from < end && to > start`, so a card that starts
    /// where the segment ends, or ends where it starts, is not; at an
    /// instant (a `jump`) it is `from <= start < to`.
    #[must_use]
    pub fn chosen(&self, start: f64, end: f64) -> Rect {
        let visible = |b: &&CardBox| if end > start { b.from < end && b.to > start } else { b.from <= start && b.to > start };
        self.boxes.iter().filter(visible).map(|b| b.camera_box).reduce(|best, r| if r.3 < best.3 { r } else { best }).unwrap_or(self.none)
    }

    /// [`Self::chosen`]'s pose offset, viewport pixels.
    fn offset_px(&self, start: f64, end: f64) -> (f32, f32) {
        let (dx, dy) = anchor_offset_units(self.chosen(start, end), self.frame);
        (dx * self.px_per_unit, dy * self.px_per_unit)
    }
}

/// The pose offset that centres the subject in `box_` (spec §5.7): the
/// box's centre less the frame's, units, positive right and down.
#[must_use]
pub fn anchor_offset_units(box_: Rect, frame: (f32, f32)) -> (f32, f32) {
    ((box_.0 + box_.2) / 2.0 - frame.0 / 2.0, (box_.1 + box_.3) / 2.0 - frame.1 / 2.0)
}

/// What [`CameraPath::with_card_boxes`] injects into one segment as it is
/// resolved: the card's pose offset for a pose that names none, and a
/// `fly`'s pass-1 seconds.
#[derive(Clone, Copy, Debug, Default)]
struct Pin {
    offset_px: Option<(f32, f32)>,
    seconds: Option<f64>,
}

/// The whole camera timeline of a script.
#[derive(Clone, Debug)]
pub struct CameraPath {
    /// The story's tracks, shared so `pose_at(t)` can sample them without
    /// a `tracks` argument. A [`FollowPlan`] indexes into it.
    tracks: Arc<TrackSet>,
    /// The pose before the first segment: the origin at zoom 0, in the
    /// scene's projection. `pose_at` returns it when there are no segments.
    start: Pose,
    segments: Vec<ResolvedSegment>,
    duration: f64,
}

impl CameraPath {
    /// Resolve every camera segment of `script` for an output of
    /// `viewport` (width, height) logical pixels. Errors are the segments
    /// this path cannot (yet) evaluate, one diagnostic each, pointing at
    /// `/camera/<i>`.
    ///
    /// The tracks are shared, not copied: each [`Track`]'s samples sit
    /// behind an `Arc`. [`with_tracks`](Self::with_tracks) takes the set
    /// itself behind one.
    pub fn new(viewport: (f32, f32), script: &Script, tracks: &TrackSet) -> Result<Self, Vec<Diagnostic>> {
        Self::with_tracks(viewport, script, Arc::new(tracks.share()))
    }

    /// [`new`](Self::new) over a track set the caller already shares.
    pub fn with_tracks(viewport: (f32, f32), script: &Script, tracks: Arc<TrackSet>) -> Result<Self, Vec<Diagnostic>> {
        Self::resolved(viewport, script, tracks, &[])
    }

    /// [`with_tracks`](Self::with_tracks) with each card's camera box
    /// solved into the poses (spec §5.3, §5.7), in two passes:
    ///
    /// 1. `with_tracks`, for each segment's seconds;
    /// 2. the script resolved again, each `jump`, `ease` and `fly` pose
    ///    that gives no `anchor_offset_px` taking the offset of
    ///    [`CardBoxes::chosen`] over its pass-1 span, extended through the
    ///    holds and orbits that follow it (they keep its pose) -- at
    ///    resolution, so
    ///    a `fly` plans its flight to the offset centre and the next
    ///    segment starts from it. A pose with its own offset keeps it.
    ///    Every `fly` keeps its pass-1 seconds, so no segment boundary,
    ///    label time or event moves. `hold`, `orbit` and `follow` inject
    ///    nothing: a hold keeps the pose before it, an orbit turns it and
    ///    a follow has its own `offset_px`.
    ///
    /// A `jump` takes its box at once; an `ease` or a `fly` moves from the
    /// previous pose's offset to its own over its duration, with its own
    /// easing. Pure, so seeking any `t` gives the pose playing it does.
    pub fn with_card_boxes(viewport: (f32, f32), script: &Script, tracks: Arc<TrackSet>, cards: &CardBoxes) -> Result<Self, Vec<Diagnostic>> {
        let first = Self::with_tracks(viewport, script, Arc::clone(&tracks))?;
        let segments = first.segments();
        let pins: Vec<Pin> = script
            .camera
            .iter()
            .zip(segments)
            .enumerate()
            .map(|(k, (segment, r))| {
                // Ruling R5-a (spec §5.3: a hold or an orbit keeps its
                // segment's offset): the pose is on screen through the
                // trailing run of holds and orbits, so the cards it must
                // clear are those visible until that run ends. Never into
                // a follow, which reads its own track.
                let held = script
                    .camera
                    .iter()
                    .zip(segments)
                    .skip(k + 1)
                    .take_while(|(next, _)| matches!(next, Segment::Hold { .. } | Segment::Orbit { .. }))
                    .last()
                    .map_or(r.start + r.duration, |(_, h)| h.start + h.duration);
                let offset_px = || Some(cards.offset_px(r.start, held));
                match segment {
                    Segment::Jump { .. } | Segment::Ease { .. } => Pin { offset_px: offset_px(), seconds: None },
                    Segment::Fly { .. } => Pin { offset_px: offset_px(), seconds: Some(r.duration) },
                    Segment::Hold { .. } | Segment::Orbit { .. } | Segment::Follow { .. } => Pin::default(),
                }
            })
            .collect();
        Self::resolved(viewport, script, tracks, &pins)
    }

    /// Every segment resolved in order, segment `i` with `pins[i]` (none
    /// past the end).
    fn resolved(viewport: (f32, f32), script: &Script, tracks: Arc<TrackSet>, pins: &[Pin]) -> Result<Self, Vec<Diagnostic>> {
        let ctx = ResolveCtx { viewport, tracks: &tracks };
        let start = Pose {
            camera: Camera {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 0.0,
                pitch: 0.0,
                bearing: 0.0,
                viewport_width_px: viewport.0,
                viewport_height_px: viewport.1,
                fov_y: Camera::DEFAULT_FOV_Y,
            },
            globe: script.scene.globe,
        };
        let mut segments = Vec::with_capacity(script.camera.len());
        let mut errors = Vec::new();
        let mut prev = start;
        let mut clock = 0.0_f64;
        for (i, segment) in script.camera.iter().enumerate() {
            match resolve(segment, prev, clock, i, &ctx, pins.get(i).copied().unwrap_or_default()) {
                Ok(resolved) => {
                    prev = end_pose(&resolved);
                    clock = resolved.start + resolved.duration;
                    segments.push(resolved);
                }
                Err(diagnostic) => errors.push(diagnostic),
            }
        }
        if errors.is_empty() {
            Ok(CameraPath { tracks, start, segments, duration: clock })
        } else {
            Err(errors)
        }
    }

    /// The pose at story time `t` seconds. `t` below zero, or not finite,
    /// is `0`; past [`duration`](Self::duration) it is the duration. Pure:
    /// the same `t` always gives a bit-identical pose.
    ///
    /// The pose is always drawable: zoom is clamped to
    /// `[MIN_ZOOM, MAX_ZOOM]` and latitude to the Mercator limit (Task 11
    /// re-review R1). Stories never opt into `SPACE_MIN_ZOOM`, so the floor
    /// is `MIN_ZOOM`. Today this is a backstop: an overshooting bezier
    /// (`y` up to +-10) does not overshoot the pose, because a segment
    /// whose eased fraction leaves `[0, 1]` shows its end (or start) pose
    /// (`pose_in`), and every segment's end poses are resolved within the
    /// limits. The clamp keeps that true if a segment kind ever does
    /// extrapolate.
    pub fn pose_at(&self, t: f64) -> Pose {
        let t = self.clamp_t(t);
        let Some(segment) = self.segment_index_at(t).and_then(|i| self.segments.get(i)) else {
            return clamp_pose(self.start);
        };
        let k = progress(segment, t);
        clamp_pose(self.pose_in(segment, k, t))
    }

    /// Total story seconds the camera path spans.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    pub fn segments(&self) -> &[ResolvedSegment] {
        &self.segments
    }

    /// The tracks this path was built over.
    pub fn tracks(&self) -> &TrackSet {
        &self.tracks
    }

    /// The end (or, with `at_start`, the start) of the segment labelled
    /// `label`, in story seconds. `None` if no segment has that label.
    pub fn label_time(&self, label: &str, at_start: bool) -> Option<f64> {
        let s = self.segments.iter().find(|s| s.label.as_deref() == Some(label))?;
        Some(if at_start { s.start } else { s.start + s.duration })
    }

    /// The segment labelled `label`'s own `(start, end)` in story seconds
    /// (`end` is `start + duration`), for a chapter's `from`/`to` (spec
    /// §2.1: a label `from` is the labelled segment's START, a label `to`
    /// is its END -- unlike an event's `after`/`at_start`, which
    /// [`label_time`](Self::label_time) resolves to one or the other by a
    /// caller-chosen flag). `None` if no segment has that label.
    pub fn segment_span(&self, label: &str) -> Option<(f64, f64)> {
        let s = self.segments.iter().find(|s| s.label.as_deref() == Some(label))?;
        Some((s.start, s.start + s.duration))
    }

    /// The start time of every settle hold, in timeline order.
    pub fn settle_times(&self) -> Vec<f64> {
        self.segments.iter().filter(|s| s.settle).map(|s| s.start).collect()
    }

    /// The segment in effect at `t` (clamped as in [`pose_at`](Self::pose_at)):
    /// the LAST one whose start is `<= t`, so where a `jump` and the
    /// segment after it share a start, the later one wins. `None` only
    /// for a path with no segments.
    pub fn segment_index_at(&self, t: f64) -> Option<usize> {
        let t = self.clamp_t(t);
        self.segments.partition_point(|s| s.start <= t).checked_sub(1)
    }

    /// `segment`'s pose at eased fraction `k` (and story time `t`, which
    /// only a `follow` reads). `k == 0` is `from` and `k == 1` is `to`
    /// exactly (bit for bit), whatever the curve does in between; an
    /// orbit's end has its bearing wrapped on the way out.
    fn pose_in(&self, segment: &ResolvedSegment, k: f64, t: f64) -> Pose {
        if k <= 0.0 {
            return segment.from;
        }
        if k >= 1.0 {
            return end_pose(segment);
        }
        // The globe flag switches at the start of the segment that changes it.
        let globe = segment.to.globe;
        let camera = match &segment.kind {
            Kind::Jump => segment.to.camera,
            Kind::Hold => segment.from.camera,
            Kind::Ease(_) => ease_pose(&segment.from.camera, &segment.to.camera, k),
            Kind::Orbit { degrees, around, .. } => {
                let mut c = orbit_pose(&segment.from, *degrees, *around, k);
                c.bearing = wrap_bearing(c.bearing);
                c
            }
            Kind::Fly(plan) => super::fly::pose(plan, k),
            Kind::Follow(plan) => match self.tracks.tracks.get(plan.track) {
                Some(track) => follow_pose(plan, track, t),
                None => segment.from.camera,
            },
        };
        Pose { camera, globe }
    }

    fn clamp_t(&self, t: f64) -> f64 {
        if !t.is_finite() || t < 0.0 {
            0.0
        } else {
            t.min(self.duration)
        }
    }
}

/// A pose clamped into the drawable range (see [`CameraPath::pose_at`]).
/// In-range values pass through bit-identical.
fn clamp_pose(mut pose: Pose) -> Pose {
    pose.camera.zoom = pose.camera.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    pose.camera.center_lat = pose.camera.center_lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    pose
}

/// The eased fraction of `segment` at `t` (already clamped into the path).
/// A zero-length segment (a `jump`, or a zero-duration `hold`/`ease`) is
/// complete the instant it starts.
fn progress(segment: &ResolvedSegment, t: f64) -> f64 {
    if segment.duration <= 0.0 {
        return 1.0;
    }
    let raw = ((t - segment.start) / segment.duration).clamp(0.0, 1.0);
    match &segment.kind {
        Kind::Jump | Kind::Hold | Kind::Follow(_) => raw,
        Kind::Ease(easing) | Kind::Orbit { easing, .. } => easing.apply(raw),
        Kind::Fly(plan) => plan.easing.apply(raw),
    }
}

/// The pose a segment leaves the camera in: `to`, with an orbit's
/// unwrapped bearing wrapped.
fn end_pose(segment: &ResolvedSegment) -> Pose {
    let mut end = segment.to;
    end.camera.bearing = wrap_bearing(end.camera.bearing);
    end
}

/// `from` turned by `degrees * k`. The bearing is NOT wrapped. Without
/// `around` the centre stays exactly where it is. With `around`, the
/// centre moves so `around` keeps its place on screen:
///
/// - **Flat frame** (globe off, or zoom above the globe band): the centre
///   rotates about `around` in world space by the same angle. The screen
///   offset of a world vector `w` is `R(-bearing) w` (x east, y down), so
///   a centre offset `R(delta) (C - P)` from `P` shows `P` where it was.
/// - **Globe frame** ([`globe_active`]): see [`orbit_centre_on_globe`].
///
/// The rotated centre's latitude is clamped to the Mercator limit. A pivot
/// close enough to a pole that the orbit swings the centre past the limit
/// loses the pivot property there: the clamp moves the centre, and the
/// pivot drifts on screen for those frames. Accepted: a story orbiting
/// within a few degrees of a pole has no map to show anyway.
fn orbit_pose(from: &Pose, degrees: f64, around: Option<(f64, f64)>, k: f64) -> Camera {
    let delta = degrees.to_radians() * k;
    let mut c = from.camera;
    c.bearing = (f64::from(from.camera.bearing) + delta) as f32;
    let Some((lon, lat)) = around else { return c };
    let size = crate::geo::TILE_SIZE;
    let (px, py) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
    let (px, py) = (px * size, py * size);
    let centre = from.camera.world_center_f64();
    // The copy of the pivot nearest the centre: the short way across the
    // antimeridian, as `ease_pose` does.
    let mut dx = centre.x - px;
    if dx > size / 2.0 {
        dx -= size;
    } else if dx < -size / 2.0 {
        dx += size;
    }
    let px = centre.x - dx;
    let placed = if globe_active(&from.camera, from.globe) {
        orbit_centre_on_globe(&from.camera, &c, (px, py), delta)
    } else {
        let dy = centre.y - py;
        let (sin, cos) = delta.sin_cos();
        lon_lat_at_world(glam::DVec3::new(px + dx * cos - dy * sin, py + dx * sin + dy * cos, centre.z))
    };
    if let Some((lon, lat)) = placed {
        c.center_lon = lon;
        c.center_lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    }
    c
}

/// The centre of `turned` (the orbit's camera at this instant: `from` with
/// its bearing turned by `delta`) on a globe frame, such that the pivot at
/// world `pivot` is drawn where `from` drew it.
///
/// On the sphere the orbit is a rotation about the pivot's radial axis:
/// the centre's unit-sphere point turns by `delta` about that axis. That
/// alone is not exact in this renderer, because the drawn sphere's radius
/// follows the centre's latitude (`globe_radius_world`, `1/cos(lat)`) and
/// a rigid turn about the pivot changes the heading by more than `delta`
/// away from the equator. So the turned point is only the starting guess:
/// [`center_placing_world_at`] then solves, by Newton's method against the
/// renderer's own forward projection, for the centre that puts the pivot
/// back on its pixel at the turned bearing. That is the same solve every
/// cursor-anchored globe gesture uses (MapLibre's `setLocationAtPoint`).
/// A pure function of its inputs, so frames stay reproducible.
///
/// Falls back to the turned point when the pivot is hidden behind the
/// planet at `from`, or no centre within the solver's tolerance exists.
///
/// **A known one-frame jump near the limb.** The fallback is decided per
/// frame. With the pivot close to the planet's edge, one frame's solve can
/// miss the tolerance while its neighbours converge, so that frame shows
/// the seed and the pivot sits a few pixels off for that one frame. It is
/// deterministic (the same `t` always makes the same choice, so a re-render
/// repeats it exactly), but visible. Carried to the golden-frame work
/// (spec §8.3); keep an `around` pivot well inside the disc.
fn orbit_centre_on_globe(from: &Camera, turned: &Camera, pivot: (f64, f64), delta: f64) -> Option<(f64, f64)> {
    let size = crate::geo::TILE_SIZE;
    let centre = from.world_center_f64();
    let axis = mercator_to_sphere(pivot.0 / size, pivot.1 / size);
    // Right-handed about the outward normal is counter-clockwise in
    // (east, north); a positive bearing turn moves the centre clockwise
    // (see the flat form, whose y points south), hence `-delta`.
    let on_sphere = glam::DQuat::from_axis_angle(axis, -delta) * mercator_to_sphere(centre.x / size, centre.y / size);
    let (lon, lat) = sphere_to_lon_lat(on_sphere)?;
    let seed = Camera { center_lon: lon, center_lat: lat, ..*turned };
    let anchors = GlobeAnchors::new(&build_globe_frame(from));
    project_world_to_screen_on_frame(from, &anchors, pivot)
        .and_then(|pixel| center_placing_world_at(&seed, glam::DVec2::new(pivot.0, pivot.1), pixel))
        .or(Some((lon, lat)))
}

/// A unit-sphere point back to (lon, lat) degrees, the inverse of
/// [`mercator_to_sphere`]; `None` if it is not finite.
fn sphere_to_lon_lat(p: glam::DVec3) -> Option<(f64, f64)> {
    if !p.is_finite() {
        return None;
    }
    // mercator_to_sphere: spherical_x = 2*PI*mx + PI = lon + 2*PI (radians).
    let lon = crate::camera_control::wrap_lon(p.x.atan2(p.z).to_degrees());
    let lat = p.y.clamp(-1.0, 1.0).asin().to_degrees();
    Some((lon, lat))
}

/// A script pose as a [`CameraTarget`] over `prev`: degrees to radians,
/// and a centre with only one of lon/lat given keeps the other from `prev`.
fn target_from(spec: &PoseSpec, prev: &Camera) -> CameraTarget {
    let center = match (spec.lon, spec.lat) {
        (None, None) => None,
        (lon, lat) => Some((lon.unwrap_or(prev.center_lon), lat.unwrap_or(prev.center_lat))),
    };
    CameraTarget {
        center,
        zoom: spec.zoom,
        pitch: spec.pitch.and_then(radians_from_degrees),
        bearing: spec.bearing.and_then(radians_from_degrees),
        anchor_offset_px: spec.anchor_offset_px.unwrap_or((0.0, 0.0)),
    }
}

/// A script pose resolved over `prev`. The anchor offset is solved in the
/// projection the pose is SHOWN in, so a pose that switches `globe` is
/// solved under its own flag, not the previous one.
fn to_pose(spec: &PoseSpec, prev: Pose) -> Pose {
    let globe = spec.globe.unwrap_or(prev.globe);
    let camera = resolve_pose(&prev.camera, &target_from(spec, &prev.camera), (MIN_ZOOM, MAX_ZOOM), globe);
    Pose { camera, globe }
}

/// What [`resolve`] needs beyond the segment itself: the output viewport
/// (Task 9's `fly::plan`) and the story's tracks (Task 10's `follow`).
struct ResolveCtx<'a> {
    viewport: (f32, f32),
    tracks: &'a TrackSet,
}

/// An error at `path` (`/camera/<i>`, or a field under it).
fn error(path: String, code: &'static str, message: String, hint: Option<&str>) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path, code, message, hint: hint.map(str::to_string) }
}

/// A usable duration: finite and above zero, the rule and code `parse`
/// applies (`duration_not_positive`). `parse` already refuses anything
/// else; this keeps a hand-built `Script` from putting NaN on the timeline.
fn duration_of(d: f64, i: usize) -> Result<f64, Diagnostic> {
    if d.is_finite() && d > 0.0 {
        Ok(d)
    } else {
        Err(error(format!("/camera/{i}/duration"), "duration_not_positive", format!("duration must be greater than 0, got {d}"), None))
    }
}

fn resolve(segment: &Segment, from: Pose, start: f64, i: usize, ctx: &ResolveCtx, pin: Pin) -> Result<ResolvedSegment, Diagnostic> {
    // The card's offset, for a pose that gives none: read on the
    // `PoseSpec`, before `target_from`'s `unwrap_or((0, 0))` erases
    // whether it was given.
    let pinned = |to: &PoseSpec| -> PoseSpec {
        match pin.offset_px {
            Some(offset) if to.anchor_offset_px.is_none() => PoseSpec { anchor_offset_px: Some(offset), ..to.clone() },
            _ => to.clone(),
        }
    };
    let base = |label: &Option<String>, duration: f64, to: Pose, kind: Kind, settle: bool| ResolvedSegment {
        label: label.clone(),
        start,
        duration,
        from,
        to,
        kind,
        settle,
    };
    match segment {
        Segment::Jump { label, to } => Ok(base(label, 0.0, to_pose(&pinned(to), from), Kind::Jump, false)),
        Segment::Hold { label, duration, settle } => Ok(base(label, duration_of(*duration, i)?, from, Kind::Hold, *settle)),
        Segment::Ease { label, to, duration, easing } => {
            let kind = Kind::Ease(easing.clone().unwrap_or(DEFAULT_EASING));
            Ok(base(label, duration_of(*duration, i)?, to_pose(&pinned(to), from), kind, false))
        }
        Segment::Orbit { label, degrees, duration, easing, around } => {
            if !degrees.is_finite() {
                return Err(error(format!("/camera/{i}/degrees"), "orbit_degrees_invalid", format!("orbit degrees {degrees} must be finite"), None));
            }
            let to = Pose { camera: orbit_pose(&from, *degrees, *around, 1.0), globe: from.globe };
            let kind = Kind::Orbit { degrees: *degrees, easing: easing.clone().unwrap_or(DEFAULT_EASING), around: *around };
            Ok(base(label, duration_of(*duration, i)?, to, kind, false))
        }
        Segment::Fly { label, to, duration, speed, screen_speed, curve, min_zoom, max_duration, easing } => {
            let spec = super::fly::FlySpec {
                duration: *duration,
                speed: *speed,
                screen_speed: *screen_speed,
                curve: *curve,
                min_zoom: *min_zoom,
                max_duration: *max_duration,
                easing: easing.clone(),
            };
            let to = to_pose(&pinned(to), from);
            let (plan, seconds) = super::fly::plan(from, to, ctx.viewport, &spec).map_err(|mut d| {
                d.path = format!("/camera/{i}{}", d.path);
                d
            })?;
            Ok(base(label, pin.seconds.unwrap_or(seconds), to, Kind::Fly(plan), false))
        }
        Segment::Follow { label, track, follow_draw, duration, offset_px, bearing, zoom, pitch } => {
            let duration = duration_of(*duration, i)?;
            // `follow_draw: "route-line"` follows the synthetic track
            // `draw:route-line` (spec §3), which the caller builds from the
            // layer's `draw` plan (`tracks::draw_track`) and adds to the
            // set; see `timeline::path_and_timeline` for the build order.
            let (track_name, field) = match (track, follow_draw) {
                (Some(track), _) => (track.clone(), "track"),
                (None, Some(layer)) => (format!("{DRAW_TRACK_PREFIX}{layer}"), "follow_draw"),
                (None, None) => (String::new(), "track"),
            };
            let (index, found) = match ctx.tracks.index_of(&track_name).and_then(|n| Some((n, ctx.tracks.tracks.get(n)?))) {
                Some(hit) => hit,
                None if field == "follow_draw" => {
                    let layer = follow_draw.as_deref().unwrap_or_default();
                    return Err(error(
                        format!("/camera/{i}/follow_draw"),
                        "unknown_track",
                        format!("layer `{layer}` has no draw head to follow"),
                        Some("the layer needs a `draw` event with `until`, and its overlay data must load with a LineString"),
                    ));
                }
                None => {
                    let known: Vec<&str> = ctx.tracks.drawn().map(|t| t.id.as_str()).collect();
                    let hint = if known.is_empty() { "no tracks were loaded".to_string() } else { format!("loaded tracks: {}", known.join(", ")) };
                    return Err(error(format!("/camera/{i}/track"), "unknown_track", format!("no track named `{track_name}` was loaded"), Some(&hint)));
                }
            };
            let plan = FollowPlan {
                track: index,
                base: from.camera,
                globe: from.globe,
                zoom: *zoom,
                pitch: pitch.and_then(radians_from_degrees),
                bearing: match bearing {
                    Some(FollowBearing::Track) => FollowHeading::Track,
                    Some(FollowBearing::Fixed(deg)) => match radians_from_degrees(*deg) {
                        Some(radians) => FollowHeading::Fixed(radians),
                        None => {
                            return Err(error(
                                format!("/camera/{i}/bearing"),
                                "follow_bearing_invalid",
                                format!("follow bearing {deg} must be a finite number of degrees or \"track\""),
                                None,
                            ));
                        }
                    },
                    None => FollowHeading::Keep,
                },
                offset_px: offset_px.unwrap_or((0.0, 0.0)),
            };
            let pose_at = |t: f64| Pose { camera: follow_pose(&plan, found, t), globe: from.globe };
            let (first, last) = (pose_at(start), pose_at(start + duration));
            Ok(ResolvedSegment { label: label.clone(), start, duration, from: first, to: last, kind: Kind::Follow(plan), settle: false })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use super::super::test_util::test_script as script;
    fn path(camera: serde_json::Value) -> CameraPath {
        CameraPath::new((800.0, 600.0), &script(camera), &Default::default()).expect("path")
    }

    #[test]
    fn jump_then_hold_is_constant_and_exact() {
        let p = path(serde_json::json!([{"type":"jump","to":{"lon":10,"lat":20,"zoom":5,"pitch":30,"bearing":45}},{"type":"hold","duration":2}]));
        assert_eq!(p.duration(), 2.0);
        for t in [0.0, 0.5, 2.0, 99.0] {
            let c = p.pose_at(t).camera;
            assert_eq!((c.center_lon, c.center_lat, c.zoom), (10.0, 20.0, 5.0));
            assert!((c.pitch - 30f32.to_radians()).abs() < 1e-6 && (c.bearing - 45f32.to_radians()).abs() < 1e-6);
        }
    }

    #[test]
    fn ease_endpoints_are_bit_equal_and_the_middle_matches_ease_pose() {
        let p = path(serde_json::json!([{"type":"jump","to":{"lon":170,"lat":0,"zoom":3}},{"type":"ease","to":{"lon":-170,"zoom":5},"duration":2,"easing":"linear"}]));
        let s = &p.segments()[1];
        assert_eq!(p.pose_at(0.0), s.from);
        assert_eq!(p.pose_at(2.0), s.to);
        let mid = p.pose_at(1.0).camera;
        assert_eq!(mid, crate::camera_control::ease_pose(&s.from.camera, &s.to.camera, 0.5));
        let lon = if mid.center_lon < 0.0 { mid.center_lon + 360.0 } else { mid.center_lon };
        assert!((lon - 180.0).abs() < 1e-6, "the short way across the antimeridian: {}", mid.center_lon);
    }

    #[test]
    fn a_540_degree_orbit_turns_one_and_a_half_times() {
        let p = path(serde_json::json!([{"type":"jump","to":{"lon":0,"lat":0,"zoom":10,"bearing":0}},{"type":"orbit","degrees":540,"duration":3,"easing":"linear"}]));
        let turned: f64 = (1..=300)
            .map(|i| {
                let a = p.pose_at((i - 1) as f64 / 100.0).camera.bearing;
                let b = p.pose_at(i as f64 / 100.0).camera.bearing;
                f64::from(crate::camera_control::shortest_bearing_delta(a, b))
            })
            .sum();
        assert!((turned.to_degrees() - 540.0).abs() < 1e-3, "{}", turned.to_degrees());
    }

    /// Task 11 re-review R1. An overshooting bezier (y up to +-10) on a
    /// z10 -> z19 ease toward the pole: where the eased fraction leaves
    /// [0, 1] the pose is the segment's end (or start) pose, so zoom and
    /// latitude stay in range at every sample; and `clamp_pose` (the
    /// backstop `pose_at` applies) pulls an out-of-range pose back while
    /// passing an in-range one through bit-identical.
    #[test]
    fn an_overshooting_easing_stays_within_zoom_and_latitude_limits() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":0,"lat":80,"zoom":10}},
            {"type":"ease","to":{"lat":84,"zoom":19},"duration":2,"easing":[0.2,10,0.8,-9]}
        ]));
        let s = p.segments()[1].clone();
        let (mut above, mut below) = (0, 0);
        for i in 0..=200 {
            let t = 2.0 * i as f64 / 200.0;
            let k = Easing::Bezier([0.2, 10.0, 0.8, -9.0]).apply(t / 2.0);
            let c = p.pose_at(t).camera;
            assert!((MIN_ZOOM..=MAX_ZOOM).contains(&c.zoom), "t {t}: zoom {}", c.zoom);
            assert!(c.center_lat.abs() <= MAX_MERCATOR_LAT, "t {t}: lat {}", c.center_lat);
            if k >= 1.0 && t < 2.0 {
                above += 1;
                assert_eq!(p.pose_at(t), end_pose(&s), "t {t}: past 1 shows the end pose");
            }
            if k < 0.0 {
                below += 1;
                assert_eq!(p.pose_at(t), s.from, "t {t}: below 0 shows the start pose");
            }
        }
        assert!(above > 0 && below > 0, "the curve leaves [0, 1] both ways ({above}, {below})");

        let inside = Pose { camera: s.from.camera, globe: false };
        assert_eq!(clamp_pose(inside), inside);
        let mut outside = inside;
        outside.camera.zoom = 55.0;
        outside.camera.center_lat = -89.9;
        let clamped = clamp_pose(outside).camera;
        assert_eq!((clamped.zoom, clamped.center_lat), (MAX_ZOOM, -MAX_MERCATOR_LAT));
        outside.camera.zoom = -40.0;
        assert_eq!(clamp_pose(outside).camera.zoom, MIN_ZOOM);
    }

    /// Every frame of `p` at 30 fps, sampled in order, then again in a
    /// shuffled order: each must be bit-identical.
    fn assert_random_access_is_bitwise(p: &CameraPath) {
        let frames = (p.duration() * 30.0).round() as usize;
        let ts: Vec<f64> = (0..=frames).map(|i| i as f64 / 30.0).collect();
        let seq: Vec<Pose> = ts.iter().map(|&t| p.pose_at(t)).collect();
        let mut order: Vec<usize> = (0..ts.len()).collect();
        order.sort_by_key(|&i| (i * 7919) % ts.len());
        for i in order {
            assert_eq!(p.pose_at(ts[i]), seq[i], "t = {}", ts[i]);
        }
    }

    #[test]
    fn random_access_equals_sequential_bitwise() {
        // Flat jump/ease/orbit, then a globe orbit `around` a pivot, whose
        // centre comes from the Newton solve (task 8 re-review R2).
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":103.8,"lat":1.35,"zoom":10}},
            {"type":"ease","to":{"zoom":12,"pitch":45},"duration":2},
            {"type":"orbit","degrees":90,"duration":2},
            {"type":"jump","to":{"lon":10.0,"lat":45.0,"zoom":5,"pitch":40,"bearing":20,"globe":true}},
            {"type":"orbit","degrees":120,"duration":2,"around":[12.0,46.0]}
        ]));
        assert_eq!(p.duration(), 6.0);
        assert!(crate::render::globe::globe_active(&p.pose_at(5.0).camera, p.pose_at(5.0).globe), "the last orbit is on a globe frame");
        assert!((p.pose_at(5.0).camera.center_lon - 10.0).abs() > 1e-4, "and it moves the centre");
        assert_random_access_is_bitwise(&p);
    }

    #[test]
    fn random_access_equals_sequential_bitwise_for_fly() {
        // Flat: a short hop at z14. Globe: Singapore to Tokyo.
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":-0.1276,"lat":51.5072,"zoom":14,"globe":true}},
            {"type":"fly","to":{"lon":-0.12,"lat":51.51,"zoom":15,"pitch":30},"duration":2},
            {"type":"jump","to":{"lon":103.82,"lat":1.35,"zoom":10,"pitch":0}},
            {"type":"fly","to":{"lon":139.76,"lat":35.68,"zoom":11,"bearing":30},"speed":1.2}
        ]));
        let form = |i: usize| match &p.segments()[i].kind {
            Kind::Fly(plan) => plan.form,
            other => panic!("segment {i} is {other:?}"),
        };
        assert_eq!((form(1), form(3)), (super::super::fly::Form::Flat, super::super::fly::Form::Globe));
        assert_random_access_is_bitwise(&p);
    }

    #[test]
    fn a_fly_segment_is_planned_and_its_endpoints_are_exact() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":103.82,"lat":1.35,"zoom":10,"globe":true}},
            {"type":"fly","to":{"lon":139.76,"lat":35.68,"zoom":11},"speed":1.2,"easing":"linear"},
            {"type":"hold","duration":1}
        ]));
        let s = &p.segments()[1];
        let Kind::Fly(plan) = &s.kind else { panic!("{:?}", s.kind) };
        let vw = plan.vw.expect("a real flight");
        assert!((s.duration - vw.s_total / 1.2).abs() < 1e-12);
        assert_eq!(p.pose_at(0.0), s.from);
        assert_eq!(p.pose_at(s.duration), s.to);
        assert_eq!(p.pose_at(s.duration / 2.0).camera, super::super::fly::pose(plan, 0.5));
        assert!(p.pose_at(s.duration / 2.0).camera.zoom < 9.0, "it climbs out");
    }

    #[test]
    fn a_fly_past_max_duration_is_a_jump() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":0,"lat":0,"zoom":3}},
            {"type":"fly","to":{"lon":100,"zoom":3},"speed":0.01,"max_duration":1},
            {"type":"hold","duration":1}
        ]));
        assert_eq!(p.segments()[1].duration, 0.0);
        assert_eq!(p.pose_at(0.0).camera.center_lon, 100.0);
    }

    #[test]
    fn a_bad_fly_option_is_refused_at_its_field() {
        let mut s = script(serde_json::json!([{"type":"jump","to":{"zoom":2}}]));
        s.camera.push(Segment::Fly {
            label: None,
            to: PoseSpec { zoom: Some(5.0), ..Default::default() },
            duration: None,
            speed: Some(-1.0),
            screen_speed: None,
            curve: None,
            min_zoom: None,
            max_duration: None,
            easing: None,
        });
        let errors = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).expect_err("negative speed");
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].code, errors[0].path.as_str()), ("speed_not_positive", "/camera/1/speed"));
    }

    #[test]
    fn label_times_resolve() {
        let p = path(serde_json::json!([{"type":"jump","label":"a","to":{"zoom":3}},{"type":"hold","label":"b","duration":1.5},{"type":"hold","duration":1}]));
        assert_eq!(p.label_time("b", false), Some(1.5));
        assert_eq!(p.label_time("b", true), Some(0.0));
        assert_eq!(p.label_time("zzz", false), None);
    }

    #[test]
    fn segment_span_is_the_labelled_segments_own_start_and_end() {
        let p = path(serde_json::json!([{"type":"jump","label":"a","to":{"zoom":3}},{"type":"hold","label":"b","duration":1.5},{"type":"hold","duration":1}]));
        assert_eq!(p.segment_span("a"), Some((0.0, 0.0)), "a jump has zero duration");
        assert_eq!(p.segment_span("b"), Some((0.0, 1.5)));
        assert_eq!(p.segment_span("zzz"), None);
    }

    #[test]
    fn a_non_finite_t_is_the_start_pose_not_nan() {
        let p = path(serde_json::json!([{"type":"jump","to":{"zoom":3}},{"type":"hold","duration":1}]));
        for t in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0] {
            let c = p.pose_at(t).camera;
            assert!(c.zoom.is_finite() && c.center_lon.is_finite());
            if t != f64::INFINITY {
                assert_eq!(p.pose_at(t), p.pose_at(0.0), "t = {t}");
            }
        }
        // +inf is past the end, so it clamps to the duration, not to 0.
        assert_eq!(p.pose_at(f64::INFINITY), p.pose_at(p.duration()));
    }

    // ---- beyond the brief ----

    #[test]
    fn an_orbit_around_a_point_keeps_that_point_still_on_screen() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":103.85,"lat":1.28,"zoom":14,"pitch":40,"bearing":10}},
            {"type":"orbit","degrees":120,"duration":4,"easing":"linear","around":[103.86,1.285]}
        ]));
        let (x, y) = crate::geo::lon_lat_to_tile_xy(103.86, 1.285, 0.0);
        let world = (x * crate::geo::TILE_SIZE, y * crate::geo::TILE_SIZE);
        let at = |t: f64| crate::render::project_world_to_screen_f64(&p.pose_at(t).camera, world).expect("on screen");
        let start = at(0.0);
        for t in [1.0, 2.0, 3.0, 4.0] {
            let (sx, sy) = at(t);
            assert!((sx - start.0).abs() < 0.05 && (sy - start.1).abs() < 0.05, "t={t}: {start:?} -> {:?}", (sx, sy));
        }
        // And the centre really moved: this is not a plain bearing turn.
        assert!((p.pose_at(2.0).camera.center_lon - 103.85).abs() > 1e-4);
    }

    #[test]
    fn an_orbit_plan_keeps_the_unwrapped_bearing_and_the_next_segment_starts_wrapped() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"zoom":4,"bearing":0}},
            {"type":"orbit","degrees":540,"duration":3},
            {"type":"hold","duration":1}
        ]));
        let orbit = &p.segments()[1];
        assert!((f64::from(orbit.to.camera.bearing).to_degrees() - 540.0).abs() < 1e-3);
        let end = p.pose_at(3.0).camera.bearing;
        assert!((-std::f32::consts::PI..std::f32::consts::PI).contains(&end));
        assert_eq!(p.segments()[2].from, p.pose_at(3.0));
        assert!((f64::from(end).to_degrees().abs() - 180.0).abs() < 1e-3, "{}", f64::from(end).to_degrees());
    }

    #[test]
    fn a_jump_after_a_hold_takes_effect_at_its_start_and_owns_that_instant() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"zoom":3}},
            {"type":"hold","duration":2},
            {"type":"jump","to":{"zoom":7}},
            {"type":"hold","duration":1}
        ]));
        assert_eq!(p.pose_at(1.999).camera.zoom, 3.0);
        assert_eq!(p.pose_at(2.0).camera.zoom, 7.0);
        assert_eq!(p.segment_index_at(2.0), Some(3));
        assert_eq!(p.segment_index_at(1.0), Some(1));
    }

    #[test]
    fn a_trailing_jump_is_the_final_pose() {
        let p = path(serde_json::json!([{"type":"jump","to":{"zoom":2}},{"type":"hold","duration":1},{"type":"jump","to":{"zoom":9}}]));
        assert_eq!(p.duration(), 1.0);
        assert_eq!(p.pose_at(1.0).camera.zoom, 9.0);
        assert_eq!(p.pose_at(0.5).camera.zoom, 2.0);
        assert_eq!(p.pose_at(99.0).camera.zoom, 9.0);
    }

    #[test]
    fn settle_times_are_the_starts_of_settle_holds() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"zoom":3}},
            {"type":"hold","duration":1,"settle":true},
            {"type":"ease","to":{"zoom":5},"duration":2},
            {"type":"hold","duration":1,"settle":true}
        ]));
        assert_eq!(p.settle_times(), vec![0.0, 3.0]);
    }

    #[test]
    fn a_pose_with_only_lon_keeps_the_previous_lat() {
        let p = path(serde_json::json!([{"type":"jump","to":{"lon":10,"lat":20,"zoom":3}},{"type":"jump","to":{"lon":30}}]));
        let c = p.pose_at(0.0).camera;
        assert_eq!((c.center_lon, c.center_lat), (30.0, 20.0));
    }

    #[test]
    fn globe_carries_forward_until_a_pose_changes_it() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"zoom":2}},
            {"type":"hold","duration":1},
            {"type":"ease","to":{"zoom":4,"globe":true},"duration":2},
            {"type":"hold","duration":1}
        ]));
        assert!(!p.pose_at(0.5).globe);
        assert!(!p.pose_at(1.0).globe, "k == 0 is exactly `from`");
        assert!(p.pose_at(1.5).globe);
        assert!(p.pose_at(4.0).globe);
    }

    // ---- fix round 1 ----

    /// The pivot's pixel on `pose`'s frame, drawn the way the renderer
    /// draws it (globe or flat), using the copy of the pivot nearest the
    /// centre.
    fn pivot_pixel(pose: Pose, lon: f64, lat: f64) -> (f32, f32) {
        let size = crate::geo::TILE_SIZE;
        let (x, y) = crate::geo::lon_lat_to_tile_xy(lon, lat, 0.0);
        let (x, y) = (x * size, y * size);
        let cx = pose.camera.world_center_f64().x;
        let x = x + ((cx - x) / size).round() * size;
        if crate::render::globe::globe_active(&pose.camera, pose.globe) {
            crate::render::globe::drawn_pixel_on_globe(&pose.camera, (x, y)).expect("pivot drawn on the globe")
        } else {
            crate::render::project_world_to_screen_f64(&pose.camera, (x, y)).expect("pivot on screen")
        }
    }

    fn assert_pivot_still(p: &CameraPath, lon: f64, lat: f64, duration: f64) {
        let start = pivot_pixel(p.pose_at(0.0), lon, lat);
        for i in 1..=8 {
            let t = duration * f64::from(i) / 8.0;
            let (sx, sy) = pivot_pixel(p.pose_at(t), lon, lat);
            assert!((sx - start.0).abs() < 0.05 && (sy - start.1).abs() < 0.05, "t={t}: {start:?} -> {:?}", (sx, sy));
        }
    }

    #[test]
    fn a_negative_orbit_around_a_point_keeps_it_still_and_turns_the_other_way() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":103.85,"lat":1.28,"zoom":14,"pitch":40,"bearing":10}},
            {"type":"orbit","degrees":-120,"duration":4,"easing":"linear","around":[103.86,1.285]}
        ]));
        assert_pivot_still(&p, 103.86, 1.285, 4.0);
        let turned = crate::camera_control::shortest_bearing_delta(p.pose_at(0.0).camera.bearing, p.pose_at(1.0).camera.bearing);
        assert!((f64::from(turned).to_degrees() + 30.0).abs() < 1e-3, "{}", f64::from(turned).to_degrees());
    }

    #[test]
    fn an_orbit_around_a_pivot_across_the_antimeridian_keeps_it_still() {
        // Centre just west of 180, pivot just east of it (lon -179.99).
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":179.995,"lat":10.0,"zoom":15,"pitch":30}},
            {"type":"orbit","degrees":180,"duration":4,"easing":"linear","around":[-179.995,10.0]}
        ]));
        assert_pivot_still(&p, -179.995, 10.0, 4.0);
        // Half a turn about a pivot 0.01 degrees east puts the centre 0.01
        // degrees east of the pivot, not a world away.
        let end = p.pose_at(4.0).camera.center_lon;
        assert!((end - -179.985).abs() < 1e-6, "{end}");
    }

    #[test]
    fn an_orbit_around_a_point_on_the_globe_keeps_it_still() {
        for zoom in [4.0, 7.5, 11.5] {
            let p = path(serde_json::json!([
                {"type":"jump","to":{"lon":10.0,"lat":45.0,"zoom":zoom,"pitch":40,"bearing":20,"globe":true}},
                {"type":"orbit","degrees":120,"duration":4,"easing":"linear","around":[10.0 + 8.0 / 2f64.powf(zoom - 4.0), 46.0]}
            ]));
            assert!(crate::render::globe::globe_active(&p.pose_at(0.0).camera, true), "z{zoom} is a globe frame");
            let lon = 10.0 + 8.0 / 2f64.powf(zoom - 4.0);
            assert_pivot_still(&p, lon, 46.0, 4.0);
            let turned = crate::camera_control::shortest_bearing_delta(p.pose_at(0.0).camera.bearing, p.pose_at(1.0).camera.bearing);
            assert!((f64::from(turned).to_degrees() - 30.0).abs() < 1e-3, "the bearing turns by exactly degrees * k");
            assert!((p.pose_at(2.0).camera.center_lon - 10.0).abs() > 1e-4, "the centre moved");
        }
    }

    #[test]
    fn a_pivotless_orbit_on_the_globe_keeps_the_centre_exactly() {
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":10.0,"lat":45.0,"zoom":4,"pitch":40,"bearing":0,"globe":true}},
            {"type":"orbit","degrees":90,"duration":3,"easing":"linear"}
        ]));
        for t in [0.0, 1.0, 2.0, 3.0] {
            let pose = p.pose_at(t);
            assert!(pose.globe);
            assert_eq!((pose.camera.center_lon, pose.camera.center_lat), (10.0, 45.0));
            assert!((f64::from(pose.camera.bearing).to_degrees() - 30.0 * t).abs() < 1e-3);
        }
    }

    #[test]
    fn a_pose_that_switches_globe_is_solved_in_its_own_projection() {
        // The anchor offset is what makes the projection matter: without one
        // both projections put the centre on the same point (re-review R1).
        let p = path(serde_json::json!([
            {"type":"jump","to":{"lon":0,"lat":0,"zoom":4}},
            {"type":"jump","to":{"lon":5,"lat":10,"globe":true,"anchor_offset_px":[0,150]}}
        ]));
        let s = &p.segments()[1];
        assert!(!s.from.globe && s.to.globe && p.pose_at(0.0).globe);
        let target = CameraTarget { center: Some((5.0, 10.0)), zoom: None, pitch: None, bearing: None, anchor_offset_px: (0.0, 150.0) };
        let on_globe = resolve_pose(&s.from.camera, &target, (MIN_ZOOM, MAX_ZOOM), true);
        let on_flat = resolve_pose(&s.from.camera, &target, (MIN_ZOOM, MAX_ZOOM), false);
        assert_eq!(s.to.camera, on_globe);
        assert_ne!(s.to.camera, on_flat, "the two projections must disagree for this test to mean anything");
    }

    // ---- follow (Task 10) ----

    fn bus(csv: &[u8]) -> TrackSet {
        TrackSet { tracks: vec![super::super::tracks::parse_track(csv, "bus", Default::default(), 0.0).unwrap()] }
    }

    /// North for 10 s, a 5 s stop, then east for 10 s, crossing the
    /// antimeridian: a turn, a stop and a wrap in one track.
    const TURNING: &[u8] = b"t,lon,lat\n2,179.99,10.00\n12,179.99,10.01\n17,179.99,10.01\n27,-179.99,10.01\n";

    #[test]
    fn follow_with_an_unloaded_track_is_refused_at_its_field() {
        let s = script(serde_json::json!([{"type":"jump","to":{"zoom":2}},{"type":"follow","track":"bus","duration":5}]));
        let errors = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).expect_err("no tracks loaded");
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].code, errors[0].path.as_str()), ("unknown_track", "/camera/1/track"));
    }

    #[test]
    fn the_task_6_follow_fixture_parses_and_resolves() {
        let s = super::super::script::parse(include_bytes!("../../tests/fixtures/stories/task10-follow.json")).expect("parses");
        let set = bus(b"t,lon,lat\n0,103.80,1.30\n20,103.82,1.30\n");
        let p = CameraPath::new((480.0, 270.0), &s, &set).expect("resolves");
        assert_eq!(p.duration(), 20.0);
        assert!(matches!(p.segments()[1].kind, Kind::Follow(_)));
        let c = p.pose_at(10.0).camera;
        assert!((c.center_lon - 103.81).abs() < 1e-9 && (c.zoom - 15.0).abs() < 1e-12);
        assert!((f64::from(c.bearing).to_degrees() - 90.0).abs() < 1e-3);
    }

    #[test]
    fn random_access_equals_sequential_bitwise_for_follow() {
        let set = bus(TURNING);
        let s = script(serde_json::json!([
            {"type":"jump","to":{"lon":179.9,"lat":10,"zoom":14,"pitch":40}},
            {"type":"hold","duration":1},
            {"type":"follow","track":"bus","duration":30,"bearing":"track","zoom":16,"offset_px":[0,120]},
            {"type":"jump","to":{"globe":true,"zoom":5}},
            {"type":"follow","track":"bus","duration":3,"bearing":30,"pitch":20}
        ]));
        let p = CameraPath::new((800.0, 600.0), &s, &set).unwrap();
        assert_eq!(p.duration(), 34.0);
        assert!(p.pose_at(33.0).globe);
        assert_random_access_is_bitwise(&p);
    }

    #[test]
    fn follow_holds_the_ends_turns_with_the_track_and_keeps_its_heading_through_a_stop() {
        let set = bus(TURNING);
        let p = CameraPath::new(
            (800.0, 600.0),
            &script(serde_json::json!([{"type":"jump","to":{"lon":0,"lat":0,"zoom":14}},{"type":"follow","track":"bus","duration":30,"bearing":"track"}])),
            &set,
        )
        .unwrap();
        let at = |t: f64| p.pose_at(t).camera;
        let deg = |t: f64| f64::from(at(t).bearing).to_degrees();
        // Before the track starts (t < 2): its first position, heading north.
        assert_eq!((at(0.0).center_lon, at(0.0).center_lat), (179.99, 10.0));
        assert!(deg(0.0).abs() < 1e-3);
        assert!(deg(7.0).abs() < 1e-3, "north");
        assert!(deg(14.5).abs() < 1e-3, "stopped: keeps north");
        assert!((deg(22.0) - 90.0).abs() < 1e-3, "east");
        // Across the antimeridian the centre wraps, the short way.
        let mid = at(22.0).center_lon;
        assert!(mid.abs() > 179.98 && mid.abs() <= 180.0, "{mid}");
        // After the track ends (t > 27): its last position.
        assert_eq!((at(30.0).center_lon, at(30.0).center_lat), (-179.99, 10.01));
    }

    #[test]
    fn a_non_finite_fixed_follow_bearing_is_refused_at_its_field() {
        let set = bus(b"t,lon,lat\n0,0,0\n1,1,0\n");
        for bad in [f64::NAN, f64::INFINITY, 1e300] {
            let mut s = script(serde_json::json!([{"type":"jump","to":{"zoom":2}}]));
            s.camera.push(Segment::Follow {
                label: None,
                track: Some("bus".to_string()),
                follow_draw: None,
                duration: 5.0,
                offset_px: None,
                bearing: Some(FollowBearing::Fixed(bad)),
                zoom: None,
                pitch: None,
            });
            let errors = CameraPath::new((800.0, 600.0), &s, &set).expect_err("bad bearing");
            assert_eq!((errors[0].code, errors[0].path.as_str()), ("follow_bearing_invalid", "/camera/1/bearing"), "{bad}");
        }
    }

    #[test]
    fn a_follow_without_zoom_pitch_or_bearing_keeps_the_pose_before_it() {
        let set = bus(b"t,lon,lat\n0,103.80,1.30\n20,103.82,1.31\n");
        let p = CameraPath::new(
            (800.0, 600.0),
            &script(serde_json::json!([
                {"type":"jump","to":{"lon":0,"lat":0,"zoom":13.5,"pitch":35,"bearing":-20}},
                {"type":"follow","track":"bus","duration":20}
            ])),
            &set,
        )
        .unwrap();
        for t in [0.0, 5.0, 20.0] {
            let c = p.pose_at(t).camera;
            assert_eq!(c.zoom, 13.5);
            assert!((f64::from(c.pitch).to_degrees() - 35.0).abs() < 1e-4 && (f64::from(c.bearing).to_degrees() + 20.0).abs() < 1e-4);
        }
    }

    #[test]
    fn a_multi_lap_track_keeps_following_round_the_world() {
        // Satellite-style: eastward at 120 degrees a step, three laps.
        let mut csv = String::from("t,lon,lat\n");
        for i in 0..=9 {
            csv.push_str(&format!("{i},{},0\n", -170.0 + 120.0 * f64::from(i)));
        }
        let set = bus(csv.as_bytes());
        let p = CameraPath::new(
            (800.0, 600.0),
            &script(serde_json::json!([{"type":"jump","to":{"zoom":3}},{"type":"follow","track":"bus","duration":9,"bearing":"track"}])),
            &set,
        )
        .unwrap();
        for i in 0..=90 {
            let t = f64::from(i) / 10.0;
            let c = p.pose_at(t).camera;
            let want = crate::camera_control::wrap_lon(-170.0 + 120.0 * t);
            let d = (c.center_lon - want).rem_euclid(360.0);
            assert!(d.min(360.0 - d) < 1e-9, "t={t}: {} vs {want}", c.center_lon);
            assert!((f64::from(c.bearing).to_degrees() - 90.0).abs() < 1e-3, "t={t}");
        }
    }

    #[test]
    fn a_globe_follow_puts_the_track_at_its_offset() {
        let set = bus(b"t,lon,lat\n0,10,45\n20,12,46\n");
        let p = CameraPath::new(
            (800.0, 600.0),
            &script(serde_json::json!([
                {"type":"jump","to":{"lon":10,"lat":45,"zoom":5,"pitch":30,"globe":true}},
                {"type":"follow","track":"bus","duration":20,"offset_px":[0,100]}
            ])),
            &set,
        )
        .unwrap();
        for t in [2.0, 10.0, 18.0] {
            let pose = p.pose_at(t);
            assert!(crate::render::globe::globe_active(&pose.camera, pose.globe));
            let (lon, lat) = set.tracks[0].position_at(t).unwrap();
            let (sx, sy) = pivot_pixel(pose, lon, lat);
            assert!((sx - 400.0).abs() < 0.5 && (sy - 400.0).abs() < 0.5, "t={t}: {:?}", (sx, sy));
        }
    }

    #[test]
    fn follow_puts_the_track_at_its_offset_and_a_fixed_bearing_holds() {
        let set = bus(b"t,lon,lat\n0,103.80,1.30\n20,103.82,1.30\n");
        let p = CameraPath::new(
            (800.0, 600.0),
            &script(serde_json::json!([
                {"type":"jump","to":{"lon":103.8,"lat":1.3,"zoom":15,"pitch":30}},
                {"type":"follow","track":"bus","duration":20,"bearing":45,"offset_px":[0,150]}
            ])),
            &set,
        )
        .unwrap();
        for t in [1.0, 10.0, 19.0] {
            let c = p.pose_at(t).camera;
            assert!((f64::from(c.bearing).to_degrees() - 45.0).abs() < 1e-3);
            let (x, y) = crate::geo::lon_lat_to_tile_xy(103.80 + 0.001 * t, 1.30, 0.0);
            let size = crate::geo::TILE_SIZE;
            let (sx, sy) = crate::render::project_world_to_screen_f64(&c, (x * size, y * size)).expect("on screen");
            assert!((sx - 400.0).abs() < 0.05 && (sy - 450.0).abs() < 0.05, "t={t}: {:?}", (sx, sy));
        }
    }

    #[test]
    fn a_hand_built_bad_duration_is_refused_at_its_field() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut s = script(serde_json::json!([{"type":"jump","to":{"zoom":2}}]));
            s.camera.push(Segment::Hold { label: None, duration: bad, settle: false });
            let errors = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).expect_err("bad duration");
            assert_eq!(errors.len(), 1, "{bad}");
            assert_eq!((errors[0].code, errors[0].path.as_str()), ("duration_not_positive", "/camera/1/duration"), "{bad}");
        }
    }

    #[test]
    fn a_hand_built_non_finite_orbit_is_refused_at_its_field() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut s = script(serde_json::json!([{"type":"jump","to":{"zoom":2}}]));
            s.camera.push(Segment::Orbit { label: None, degrees: bad, duration: 2.0, easing: None, around: None });
            let errors = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).expect_err("bad degrees");
            assert_eq!(errors.len(), 1, "{bad}");
            assert_eq!((errors[0].code, errors[0].path.as_str()), ("orbit_degrees_invalid", "/camera/1/degrees"), "{bad}");
        }
    }

    // ---- follow_draw (Task 4) ----

    /// A story over `routes` (one line layer `route-line`, line metrics on)
    /// whose camera jumps to (0, 0), holds 1 s, then follows the draw of
    /// `route-line` for 4 s, and whose events draw it over [1, 5]
    /// (linear); `events` is appended.
    fn follow_draw_story(extra_events: serde_json::Value) -> Script {
        let mut v = follow_draw_json(5.0);
        v["events"].as_array_mut().unwrap().extend(extra_events.as_array().cloned().unwrap_or_default());
        super::super::script::parse(v.to_string().as_bytes()).expect("the follow_draw story parses")
    }

    /// The JSON of [`follow_draw_story`] with the draw over `[1, draw_until]`.
    fn follow_draw_json(draw_until: f64) -> serde_json::Value {
        serde_json::json!({
            "story": 1,
            "output": {"width": 800, "height": 600, "fps": 30},
            "scene": {
                "style": "x.json",
                "basemap": {"source": "openfreemap"},
                "overlays": [{"id": "routes", "data": "routes.geojson", "line_metrics": true,
                              "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#e33"}}]}]
            },
            "camera": [
                {"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 4}},
                {"type": "hold", "duration": 1},
                {"type": "follow", "follow_draw": "route-line", "duration": 4}
            ],
            "events": [{"at": 1.0, "until": draw_until, "overlay": {"layer": "route-line", "effect": "draw", "easing": "linear", "head": {"radius_px": 6, "color": "#fff"}}}]
        })
    }

    fn route_line_sources(script: &Script) -> Vec<super::super::overlay_layers::LayerSource> {
        let doc = serde_json::json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": [[0.0, 0.0], [10.0, 0.0]]}}]});
        super::super::overlay_layers::layer_sources(0, &script.scene.overlays[0], &doc).unwrap()
    }

    #[test]
    fn follow_draw_puts_the_camera_on_the_draw_head() {
        // A draw over [1, 5] on (0,0)->(10,0) and a follow segment with follow_draw over [1, 5]:
        // pose_at(3.0) is at lon 5.0 +- 1e-6.
        let s = follow_draw_story(serde_json::json!([]));
        let layers = route_line_sources(&s);
        let empty = serde_json::json!({"version": 8, "layers": []});
        let (p, timeline) =
            super::super::timeline::path_and_timeline((800.0, 600.0), &s, Arc::new(TrackSet::default()), &layers, empty, &|_| None).unwrap();
        assert_eq!((p.segments()[2].start, p.segments()[2].duration), (1.0, 4.0));
        let c = p.pose_at(3.0).camera;
        assert!((c.center_lon - 5.0).abs() < 1e-6 && c.center_lat.abs() < 1e-6, "{c:?}");
        // The camera sits exactly under the drawn head, at every frame of the draw.
        for n in 30..150 {
            let t = f64::from(n) / 30.0;
            let head = timeline.state_at(t, 30).layer_fx[0].head.expect("a head while drawing");
            let c = p.pose_at(t).camera;
            assert!((c.center_lon - head.0).abs() < 1e-6 && (c.center_lat - head.1).abs() < 1e-6, "t {t}: camera {c:?} head {head:?}");
        }
        // At and after the end: the line's end.
        assert!((p.pose_at(5.0).camera.center_lon - 10.0).abs() < 1e-6);
        // The draw track is camera-only: nothing extra is drawn for it.
        assert!(!p.tracks().any_live_at(3.0));
        let drawn: serde_json::Value = serde_json::from_str(&p.tracks().feature_collection_at(3.0)).unwrap();
        assert_eq!(drawn["features"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn follow_draw_without_a_loaded_line_is_refused_at_its_field() {
        let s = follow_draw_story(serde_json::json!([]));
        let (stage, errors) =
            super::super::timeline::path_and_timeline((800.0, 600.0), &s, Arc::new(TrackSet::default()), &[], serde_json::json!({"version": 8, "layers": []}), &|_| None)
                .unwrap_err();
        assert_eq!(stage, "camera path");
        assert_eq!((errors[0].code, errors[0].path.as_str()), ("unknown_track", "/camera/2/follow_draw"));
    }

    #[test]
    fn a_story_without_follow_draw_builds_one_path_exactly_as_before() {
        let s = script(serde_json::json!([{"type":"jump","to":{"lon":10,"lat":20,"zoom":5}},{"type":"hold","duration":2}]));
        let before = CameraPath::new((800.0, 600.0), &s, &TrackSet::default()).unwrap();
        let (after, _) =
            super::super::timeline::path_and_timeline((800.0, 600.0), &s, Arc::new(TrackSet::default()), &[], serde_json::json!({"version": 8, "layers": []}), &|_| None)
                .unwrap();
        assert_eq!(before.segments(), after.segments());
        for n in 0..=60 {
            assert_eq!(before.pose_at(f64::from(n) / 30.0), after.pose_at(f64::from(n) / 30.0));
        }
    }

    /// [`follow_draw_story`]'s scene with the draw over `[1, draw_until]`
    /// and a `fly` at `speed: 1.2` after the follow.
    fn follow_draw_then_fly(draw_until: f64) -> Script {
        let mut v = follow_draw_json(draw_until);
        v["camera"].as_array_mut().unwrap().push(serde_json::json!({"type": "fly", "to": {"lon": 20.0, "lat": 10.0, "zoom": 3}, "speed": 1.2}));
        super::super::script::parse(v.to_string().as_bytes()).expect("parses")
    }

    #[test]
    fn follow_draw_then_a_speed_fly_builds_when_the_draw_ends_inside_the_follow() {
        let s = follow_draw_then_fly(5.0);
        let layers = route_line_sources(&s);
        let (p, _) =
            super::super::timeline::path_and_timeline((800.0, 600.0), &s, Arc::new(TrackSet::default()), &layers, serde_json::json!({"version": 8, "layers": []}), &|_| None)
                .expect("builds");
        let fly = &p.segments()[3];
        assert_eq!(fly.start, 5.0);
        assert!((fly.from.camera.center_lon - 10.0).abs() < 1e-6, "the fly starts at the line's end: {:?}", fly.from.camera);
        assert_eq!(p.duration(), 5.0 + fly.duration);
    }

    #[test]
    fn follow_draw_then_a_speed_fly_builds_when_the_draw_ends_after_the_follow() {
        // The follow ends at 5 s with the head 4/6 of the way along: the
        // placeholder's end (the line's end) is wrong, and a second round
        // settles it.
        let s = follow_draw_then_fly(7.0);
        let layers = route_line_sources(&s);
        let (p, tl) =
            super::super::timeline::path_and_timeline((800.0, 600.0), &s, Arc::new(TrackSet::default()), &layers, serde_json::json!({"version": 8, "layers": []}), &|_| None)
                .expect("builds");
        let fly = &p.segments()[3];
        assert!((fly.from.camera.center_lon - 20.0 / 3.0).abs() < 1e-6, "{:?}", fly.from.camera);
        // The fly's duration is the one from where the head really was.
        let direct = CameraPath::new((800.0, 600.0), &s, &TrackSet { tracks: vec![super::super::tracks::draw_track(tl.effect_plans()[0], &layers[0]).unwrap()] }).unwrap();
        assert_eq!(fly.duration, direct.segments()[3].duration);
        assert_eq!(tl.state_at(6.0, 30).layer_fx[0].trim, Some([0.0, (5.0f64 / 6.0) as f32]));
    }

    /// F1b: a speed fly whose placeholder distance is SHORTER than the real
    /// one (the placeholder ends at the line's end, lon 10, near the fly's
    /// target at lon 12; the real follow ends at the head, lon 20/3), and an
    /// event between the placeholder path's end and the real end. Event
    /// times are validated only against the final path, so it builds; an
    /// event after the real end is still refused.
    #[test]
    fn an_event_past_the_placeholder_end_but_inside_the_real_end_builds() {
        let with_event = |at: Option<f64>| {
            let mut v = follow_draw_json(7.0);
            v["camera"].as_array_mut().unwrap().push(serde_json::json!({"type": "fly", "to": {"lon": 12.0, "lat": 0.0, "zoom": 4}, "speed": 1.2}));
            if let Some(at) = at {
                v["events"].as_array_mut().unwrap().push(serde_json::json!({"at": at, "set_param": {"param": "raster_opacity", "value": 0.5}}));
            }
            super::super::script::parse(v.to_string().as_bytes()).expect("parses")
        };
        let build = |s: &Script| {
            let layers = route_line_sources(s);
            super::super::timeline::path_and_timeline((800.0, 600.0), s, Arc::new(TrackSet::default()), &layers, serde_json::json!({"version": 8, "layers": []}), &|_| None)
        };
        let s = with_event(None);
        // The first round's path: the placeholder, (0,0) at 0 s to (10,0) at 1 s.
        let placeholder = super::super::tracks::Track::from_samples("draw:route-line", &[(0.0, 0.0, 0.0), (1.0, 10.0, 0.0)]).unwrap();
        let first = CameraPath::new((800.0, 600.0), &s, &TrackSet { tracks: vec![placeholder] }).unwrap().duration();
        let real = build(&s).unwrap().0.duration();
        assert!(first < real, "the placeholder's fly is the shorter: {first} vs {real}");
        let between = (first + real) / 2.0;
        let (p, tl) = build(&with_event(Some(between))).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(p.duration(), real);
        assert_eq!(tl.state_at(real, 30).params.raster_opacity, 0.5, "the event is in the final timeline");
        let (stage, errors) = build(&with_event(Some(real + 1.0))).unwrap_err();
        assert_eq!(stage, "timeline");
        assert_eq!(errors[0].code, "event_after_end");
    }

    // ---- Task R5 (spec §5.3, §5.7): per-segment camera boxes ----

    const TALL: (f32, f32) = (1080.0, 1920.0);
    const WIDE: (f32, f32) = (1920.0, 1080.0);
    /// The no-card box: above the fine print.
    const NONE_BOX: super::super::tiers::Rect = (64.0, 64.0, 1016.0, 1700.0);

    fn with_cards_on(frame: (f32, f32), camera: serde_json::Value, boxes: Vec<CardBox>) -> (CameraPath, CameraPath) {
        let s = script(camera);
        let plain = CameraPath::with_tracks(frame, &s, Default::default()).expect("path");
        let cards = CardBoxes { boxes, none: NONE_BOX, frame, px_per_unit: 1.0 };
        (plain, CameraPath::with_card_boxes(frame, &s, Default::default(), &cards).expect("path"))
    }

    fn with_cards(camera: serde_json::Value, boxes: Vec<CardBox>) -> (CameraPath, CameraPath) {
        with_cards_on(TALL, camera, boxes)
    }

    /// What `to_pose` makes of `{lon, lat, zoom: 14}` with the offset `off`.
    fn solved(from: &Camera, lon: f64, lat: f64, off: (f32, f32)) -> Camera {
        let target = CameraTarget { center: Some((lon, lat)), zoom: Some(14.0), pitch: None, bearing: None, anchor_offset_px: off };
        resolve_pose(from, &target, (MIN_ZOOM, MAX_ZOOM), false)
    }

    fn card(from: f64, to: f64, camera_box: super::super::tiers::Rect) -> CardBox {
        CardBox { from, to, camera_box }
    }

    #[test]
    fn card_offsets_are_solved_into_each_segments_pose() {
        // jump at 0, hold 0..2, ease 2..4, hold 4..6. Card A [0, 2): box
        // bottom 900; card B [2, 6): box bottom 700. A ends where the ease
        // starts, so the ease sees only B; the jump sees only A.
        let (plain, p) = with_cards(
            serde_json::json!([
                {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
                {"type": "hold", "duration": 2},
                {"type": "ease", "to": {"lon": 103.86, "lat": 1.30, "zoom": 14}, "duration": 2},
                {"type": "hold", "duration": 2}
            ]),
            vec![card(0.0, 2.0, (64.0, 200.0, 1016.0, 900.0)), card(2.0, 6.0, (64.0, 200.0, 1016.0, 700.0))],
        );
        let seg = p.segments();
        assert_eq!(seg[0].to.camera, solved(&seg[0].from.camera, 103.85, 1.29, (0.0, (200.0 + 900.0) / 2.0 - 960.0)));
        assert_eq!(seg[2].to.camera, solved(&seg[2].from.camera, 103.86, 1.30, (0.0, (200.0 + 700.0) / 2.0 - 960.0)));
        assert_ne!(seg[0].to.camera, plain.segments()[0].to.camera, "the offset moved the centre");
        // The hold keeps the jump's pose, and the ease starts from it.
        assert_eq!(p.pose_at(1.0).camera, seg[0].to.camera);
        assert_eq!(seg[2].from.camera, seg[0].to.camera);
        assert_eq!(p.pose_at(5.0).camera, seg[2].to.camera);
    }

    #[test]
    fn the_highest_card_top_wins_when_two_cards_share_a_segment() {
        let (_, p) = with_cards(
            serde_json::json!([
                {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
                {"type": "ease", "to": {"lon": 103.86, "lat": 1.30, "zoom": 14}, "duration": 4}
            ]),
            vec![card(0.0, 4.0, (64.0, 200.0, 1016.0, 900.0)), card(1.0, 3.0, (64.0, 200.0, 1016.0, 600.0))],
        );
        let seg = p.segments();
        assert_eq!(seg[1].to.camera, solved(&seg[1].from.camera, 103.86, 1.30, (0.0, (200.0 + 600.0) / 2.0 - 960.0)));
    }

    /// Spec §5.7: a side card leaves the *beside* box, so the subject
    /// moves right of the card, not only up.
    #[test]
    fn a_side_card_offsets_the_camera_sideways() {
        let beside = (720.0 + 32.0 + 96.0, 140.0, 1920.0 - 64.0, 948.0 - 24.0);
        let (_, p) = with_cards_on(
            WIDE,
            serde_json::json!([{"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}}, {"type": "hold", "duration": 3}]),
            vec![card(0.0, 3.0, beside)],
        );
        let (dx, dy) = ((beside.0 + beside.2) / 2.0 - 960.0, (beside.1 + beside.3) / 2.0 - 540.0);
        assert!(dx > 100.0, "the box sits right of centre: {dx}");
        let seg = &p.segments()[0];
        assert_eq!(seg.to.camera, solved(&seg.from.camera, 103.85, 1.29, (dx, dy)));
        // The subject is drawn at the box's centre, right of the frame's.
        let shown = pivot_pixel(seg.to, 103.85, 1.29);
        assert!((shown.0 - (beside.0 + beside.2) / 2.0).abs() < 0.5 && (shown.1 - (beside.1 + beside.3) / 2.0).abs() < 0.5, "{shown:?}");
    }

    #[test]
    fn an_explicit_anchor_offset_is_kept() {
        let camera = serde_json::json!([
            {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14, "anchor_offset_px": [5, 7]}},
            {"type": "hold", "duration": 9}
        ]);
        let (plain, p) = with_cards(camera, vec![card(0.0, 9.0, (0.0, 0.0, 10.0, 10.0))]);
        assert_eq!(p.segments()[0].to, plain.segments()[0].to);
    }

    /// Every boundary is pass 1's, for a fly given a `duration`, a
    /// `speed`, a `screen_speed`, and one over its `max_duration` (zero
    /// seconds, a cut): the card
    /// offsets change where each fly goes, never how long it takes, so
    /// no label time or event moves.
    #[test]
    fn fly_durations_do_not_move_when_cards_offset_the_path() {
        let camera = serde_json::json!([
            {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
            {"type": "fly", "label": "speed", "to": {"lon": 103.99, "lat": 1.36, "zoom": 13}, "speed": 1.2},
            {"type": "hold", "duration": 1},
            {"type": "fly", "label": "screen", "to": {"lon": 104.2, "lat": 1.1, "zoom": 12}, "screen_speed": 0.8},
            {"type": "fly", "label": "capped", "to": {"lon": 100.6, "lat": 3.4, "zoom": 11}, "speed": 0.1, "max_duration": 1},
            {"type": "fly", "label": "given", "to": {"lon": 103.8, "lat": 1.3, "zoom": 14}, "duration": 2.5},
            {"type": "hold", "duration": 1}
        ]);
        let (plain, p) = with_cards(
            camera,
            vec![card(0.0, 3.0, (64.0, 200.0, 1016.0, 600.0)), card(3.0, 99.0, (64.0, 200.0, 1016.0, 1200.0))],
        );
        assert_eq!(p.duration(), plain.duration());
        for (a, b) in p.segments().iter().zip(plain.segments()) {
            assert_eq!((a.start, a.duration), (b.start, b.duration), "{:?}", a.label);
        }
        assert_ne!(p.segments()[1].to, plain.segments()[1].to, "the fly's end took the card's offset");
        assert_eq!(p.label_time("capped", true), p.label_time("capped", false), "the capped fly stays a cut");
    }

    /// Seek = play (1a R8): every frame sampled in order, then in a
    /// shuffled order and backwards, is bit-identical.
    #[test]
    fn seeking_the_camera_equals_playing_it() {
        let (_, p) = with_cards(
            serde_json::json!([
                {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
                {"type": "hold", "duration": 1},
                {"type": "fly", "to": {"lon": 103.99, "lat": 1.36, "zoom": 13}, "speed": 1.2},
                {"type": "orbit", "degrees": 90, "duration": 2},
                {"type": "ease", "to": {"lon": 103.9, "lat": 1.3, "zoom": 12}, "duration": 2}
            ]),
            vec![card(0.0, 2.0, (64.0, 200.0, 1016.0, 900.0)), card(2.0, 99.0, (64.0, 200.0, 1016.0, 700.0))],
        );
        assert_random_access_is_bitwise(&p);
        let ts: Vec<f64> = (0..=200).map(|i| p.duration() * f64::from(i) / 200.0).collect();
        let fwd: Vec<_> = ts.iter().map(|&t| p.pose_at(t)).collect();
        let back: Vec<_> = ts.iter().rev().map(|&t| p.pose_at(t)).collect();
        assert_eq!(fwd, back.into_iter().rev().collect::<Vec<_>>());
    }

    /// A segment no card is visible during takes the no-card box (spec
    /// §5.3: above the fine print); a card that ends exactly where a
    /// segment starts is not visible during it.
    #[test]
    fn no_card_uses_the_fine_print_box() {
        let (_, p) = with_cards(
            serde_json::json!([
                {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
                {"type": "hold", "duration": 2},
                {"type": "ease", "to": {"lon": 103.86, "lat": 1.30, "zoom": 14}, "duration": 2}
            ]),
            vec![card(0.0, 2.0, (64.0, 200.0, 1016.0, 600.0))],
        );
        let seg = p.segments();
        let (dx, dy) = ((NONE_BOX.0 + NONE_BOX.2) / 2.0 - 540.0, (NONE_BOX.1 + NONE_BOX.3) / 2.0 - 960.0);
        assert_eq!(seg[2].to.camera, solved(&seg[2].from.camera, 103.86, 1.30, (dx, dy)));
        // With no cards at all, every pose takes it.
        let (_, q) = with_cards(serde_json::json!([{"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}}]), vec![]);
        assert_eq!(q.segments()[0].to.camera, solved(&q.segments()[0].from.camera, 103.85, 1.29, (dx, dy)));
    }

    /// `hold`, `orbit` and `follow` inject nothing: a follow keeps its
    /// own `offset_px`.
    #[test]
    fn a_follow_keeps_its_own_offset() {
        let s = script(serde_json::json!([
            {"type": "jump", "to": {"lon": 0, "lat": 0, "zoom": 10}},
            {"type": "follow", "track": "bus", "duration": 2}
        ]));
        let tracks = bus(b"t,lon,lat\n0,103.8,1.3\n10,103.9,1.35\n");
        let tracks = Arc::new(tracks);
        let cards = CardBoxes { boxes: vec![card(0.0, 9.0, (64.0, 200.0, 1016.0, 600.0))], none: NONE_BOX, frame: TALL, px_per_unit: 1.0 };
        let p = CameraPath::with_card_boxes(TALL, &s, Arc::clone(&tracks), &cards).expect("path");
        let Kind::Follow(plan) = &p.segments()[1].kind else { panic!("a follow") };
        assert_eq!(plan.offset_px, (0.0, 0.0));
    }

    /// Ruling R5-a: a held pose clears the card that appears during its
    /// hold -- `jump; hold 4` with a card over [1, 4) takes its box.
    #[test]
    fn a_held_jump_takes_the_box_of_a_card_that_appears_during_its_hold() {
        let card_box = (64.0, 200.0, 1016.0, 700.0);
        let (_, p) = with_cards(
            serde_json::json!([{"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}}, {"type": "hold", "duration": 4}]),
            vec![card(1.0, 4.0, card_box)],
        );
        let seg = &p.segments()[0];
        assert_eq!(seg.to.camera, solved(&seg.from.camera, 103.85, 1.29, anchor_offset_units(card_box, TALL)));
    }

    /// Ruling R5-a: an ease followed by a hold clears a card that starts
    /// mid-hold; the run stops at a follow, which reads its own track.
    #[test]
    fn an_eased_pose_takes_the_box_of_a_card_that_starts_mid_hold() {
        let card_box = (64.0, 200.0, 1016.0, 650.0);
        let (_, p) = with_cards(
            serde_json::json!([
                {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
                {"type": "ease", "to": {"lon": 103.86, "lat": 1.30, "zoom": 14}, "duration": 2},
                {"type": "hold", "duration": 4}
            ]),
            vec![card(4.0, 6.0, card_box)],
        );
        let seg = p.segments();
        assert_eq!(seg[1].to.camera, solved(&seg[1].from.camera, 103.86, 1.30, anchor_offset_units(card_box, TALL)));
        // The jump's own hold-less instant sees no card.
        assert_eq!(seg[0].to.camera, solved(&seg[0].from.camera, 103.85, 1.29, anchor_offset_units(NONE_BOX, TALL)));
        // A follow after the jump is not part of its run.
        let s = script(serde_json::json!([
            {"type": "jump", "to": {"lon": 103.85, "lat": 1.29, "zoom": 14}},
            {"type": "follow", "track": "bus", "duration": 4}
        ]));
        let cards = CardBoxes { boxes: vec![card(1.0, 4.0, card_box)], none: NONE_BOX, frame: TALL, px_per_unit: 1.0 };
        let q = CameraPath::with_card_boxes(TALL, &s, Arc::new(bus(b"t,lon,lat\n0,103.8,1.3\n10,103.9,1.35\n")), &cards).expect("path");
        assert_eq!(q.segments()[0].to.camera, solved(&q.segments()[0].from.camera, 103.85, 1.29, anchor_offset_units(NONE_BOX, TALL)));
    }
}
