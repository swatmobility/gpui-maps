//! The altitude layers' state: objects above the globe (satellites; spec
//! 2026-09-29-satellites-design.md §2.8) -- the layers, their keyframe
//! workers, the scene clock, the hit test and what a frame hands
//! `AltitudePipeline`. No gpui: `map_view::altitude` puts it behind
//! `MapView`'s API and schedules its redraws; the iOS shell (`ios/`, by
//! `#[path]`) drives the same state from its own frame loop.
//!
//! Only on a globe frame: at `transition == 0` the layer is skipped
//! outright, so a flat frame is bit-identical with and without it (R3).


use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use glam::DVec3;
use crate::render::altitude::{
    geo_to_sphere, hidden_bounds, scene_time_at, teme_to_sphere, AltitudeLine, GeoPoint, Keyframe, KeyframeBuilder,
    KeyframeScheduler, KeyframeSource, RefFrame, KEYFRAME_DELTA_MIN_S,
};
use crate::render::altitude_pipeline::{
    emphasis_of, hit_test, keyed_instances_for, line_instances, pack_flags, static_instance, AltitudeBatch, AltitudeFrame,
    AltitudeLook, AltitudePipeline, AltitudeStyle, LayerMotion, LineBatch, LineInstance, PointInstance, FLAG_VISIBLE, HIT_RADIUS_PX,
    DotMotion,
};
use crate::render::globe::GLOBE_RADIUS_M;
use crate::render::gpu::GpuContext;

/// Where a layer's objects are.
#[derive(Clone)]
pub enum AltitudePositions {
    /// Earth-fixed and not moving (balloons, an aircraft snapshot): metres
    /// above the globe's sphere, narrowed to f32 Earth radii at upload.
    Static(Vec<GeoPoint>),
    /// Moving objects (satellites), `source.len()` of them, propagated on a
    /// worker thread into keyframes at the scene clock's interval
    /// (`render::altitude::keyframe_delta_s`) and interpolated between them
    /// on the GPU. `frame` says whether the states turn with the planet
    /// (`EarthFixed`) or are inertial (`Inertial`, TEME; rotated by the
    /// scene time's `teme_to_sphere`).
    Keyframed { frame: RefFrame, source: Arc<dyn KeyframeSource> },
}

impl std::fmt::Debug for AltitudePositions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(points) => f.debug_tuple("Static").field(&points.len()).finish(),
            Self::Keyframed { frame, source } => {
                f.debug_struct("Keyframed").field("frame", frame).field("len", &source.len()).finish()
            }
        }
    }
}

/// The scene clock: what time the map shows, and how fast it runs.
/// `rate` is scene seconds per wall second -- 1 real time, 60 a minute a
/// second, 0 paused, negative backwards. Re-anchor it (a new
/// `anchor_scene_unix` and `anchor_wall`) to scrub; change the rate with
/// [`Self::with_rate`], which keeps the scene time continuous.
#[derive(Clone, Copy, Debug)]
pub struct SceneClock {
    /// The scene time at `anchor_wall`, Unix seconds.
    pub anchor_scene_unix: f64,
    /// The wall-clock instant the scene time is measured from. The
    /// engine's portable clock ([`crate::timing::Instant`]), so the browser
    /// build can run a scene clock; native code converts a
    /// `std::time::Instant` with `.into()`.
    pub anchor_wall: crate::timing::Instant,
    /// Scene seconds per wall second: 1 is real time, 0 paused, negative
    /// runs backwards.
    pub rate: f64,
}

impl SceneClock {
    /// Now, at real time.
    #[must_use]
    pub fn realtime() -> Self {
        let now = crate::render::solar::now_unix_seconds().unwrap_or(0.0);
        Self { anchor_scene_unix: now, anchor_wall: crate::timing::Instant::now(), rate: 1.0 }
    }

    /// This clock at a new `rate`, re-anchored at its own time at `now`, so
    /// the scene time is continuous across the change. Writing
    /// `SceneClock { rate, ..clock }` instead jumps the scene time by
    /// `elapsed x (new - old)`.
    #[must_use]
    pub fn with_rate(self, rate: f64, now: impl Into<crate::timing::Instant>) -> Self {
        let now = now.into();
        Self { anchor_scene_unix: self.at(now), anchor_wall: now, rate }
    }

    /// The scene time at wall instant `wall` (which may be before the
    /// anchor).
    #[must_use]
    pub fn at(&self, wall: impl Into<crate::timing::Instant>) -> f64 {
        let wall = wall.into();
        let elapsed = if wall >= self.anchor_wall {
            wall.duration_since(self.anchor_wall).as_secs_f64()
        } else {
            -self.anchor_wall.duration_since(wall).as_secs_f64()
        };
        scene_time_at(self.anchor_scene_unix, elapsed, self.rate)
    }
}

/// Whether the map must keep painting for its altitude layers: a keyframed
/// layer under a running clock moves every frame, and one waiting on a
/// keyframe (a scrub while paused) must paint again when it lands. A paused
/// clock with nothing in flight asks for nothing -- no frames, no battery.
pub(crate) fn wants_frames(rate: f64, has_keyframed: bool, waiting: bool) -> bool {
    has_keyframed && ((rate != 0.0 && !rate.is_nan()) || waiting)
}

pub(crate) use crate::render::altitude::EARTH_ROTATION_RAD_S;

/// On-screen motion, device px, an inertial line layer may accumulate
/// since its last paint before it asks for a frame of its own.
pub(crate) const INERTIAL_REDRAW_PX: f64 = 0.5;

/// Wall seconds until an inertial line layer that moves `px_per_rad`
/// device px per radian of Earth rotation has moved
/// [`INERTIAL_REDRAW_PX`] under clock `rate`; `None` when it does not move
/// (a paused or NaN clock, or nothing drawn). An orbit ring alone at rate 1
/// moves about 0.01 px/s at z0, so it waits about a minute rather than
/// painting 60 times a second (Task 7 review Minor 3).
pub(crate) fn inertial_redraw_after_s(rate: f64, px_per_rad: f64) -> Option<f64> {
    let px_per_s = EARTH_ROTATION_RAD_S * rate.abs() * px_per_rad;
    (px_per_s > 0.0).then(|| INERTIAL_REDRAW_PX / px_per_s)
}

/// The fastest any vertex of `segments` (after `frame.m`) moves on screen
/// per radian the Earth turns, device px -- an upper bound for every
/// vertex in front of the near plane (occlusion is ignored).
///
/// A vertex at view `(X, Y, Z)`, depth `d = -Z`, projects to `f (X, Y)/d`;
/// moving by `v` it moves on screen by `f (v_xy / d + (X, Y) v_z / d²)`,
/// at most `f |v| / d · (1 + |(X, Y)| / d)`. Turning with the Earth,
/// `|v|` per radian is its distance from the polar (sphere `y`) axis, in
/// view units. The depth is clamped to the near plane (a segment the near
/// plane clips has an end there).
pub(crate) fn inertial_px_per_rad(segments: &[LineInstance], frame: &AltitudeFrame) -> f64 {
    let focal = f64::from(frame.viewport.1) / 2.0 / frame.tan_half_fov;
    // The view may scale sphere units: the lateral motion and the depth
    // must both be in view units.
    let scale = frame.view.transform_vector3(DVec3::X).length();
    let speed = |v: [f32; 3]| {
        let p = frame.m * glam::Vec3::from(v).as_dvec3();
        let v = frame.view.transform_point3(p);
        let depth = (-v.z).max(frame.near);
        p.x.hypot(p.z) * scale * focal / depth * (1.0 + v.x.hypot(v.y) / depth)
    };
    segments.iter().flat_map(|s| [speed(s.a), speed(s.b)]).filter(|v| v.is_finite()).fold(0.0, f64::max)
}


/// A keyframed layer's worker: one thread, fed keyframe times, answering
/// with filled keyframes. Dropping it drops `requests`, so the thread's
/// `recv` fails and it exits after at most the keyframe it is filling --
/// never a forever-thread.
struct Worker {
    requests: Sender<f64>,
    results: Receiver<Keyframe>,
}

impl Worker {
    fn spawn(source: Arc<dyn KeyframeSource>) -> Option<Self> {
        let (requests, rx) = std::sync::mpsc::channel::<f64>();
        let (tx, results) = std::sync::mpsc::channel::<Keyframe>();
        std::thread::Builder::new()
            .name("gpui-maps-keyframes".into())
            .spawn(move || {
                while let Ok(mut t) = rx.recv() {
                    // Only the newest request matters: the scheduler accepts
                    // only the one in flight, so older queued ones are stale.
                    while let Ok(newer) = rx.try_recv() {
                        t = newer;
                    }
                    let mut builder = KeyframeBuilder::new(t, source.len());
                    builder.step(&*source, usize::MAX);
                    if tx.send(builder.finish()).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self { requests, results })
    }
}

/// A keyframed layer's moving parts.
struct Keyed {
    frame: RefFrame,
    source: Arc<dyn KeyframeSource>,
    scheduler: KeyframeScheduler,
    /// The keyframes the scheduler holds (at most three).
    store: Vec<Keyframe>,
    /// The pair the instances were built from. Each object's `s` window
    /// over it (`hidden_bounds`, bisected once per pair) is cached in its
    /// `AltitudeLayer::objects` entry's `hidden_s`, which a hover or a mask
    /// never recomputes.
    pair: Option<(f64, f64)>,
    /// This frame's (or, while holding, the last drawn) fraction.
    s: f32,
    /// The interval the instances were built at.
    delta_s: f64,
    hold: bool,
    /// The scene time of this frame, for `M(t)`.
    scene_unix: f64,
    /// The scene clock's rate at the last advance: sizes the trails.
    rate: f64,
    worker: Option<Worker>,
}

impl Keyed {
    fn keyframe(&self, t: f64) -> Option<&Keyframe> {
        let d = self.scheduler.delta_s();
        self.store.iter().find(|k| (k.t_unix - t).abs() <= d * 1e-9)
    }

    /// While holding, the OLD pair at the last `s` is rotated by the NEW
    /// scene time's `M`: after a large scrub an inertial layer's dimmed
    /// dots sit at their old inertial positions under the new Earth
    /// orientation for the ~2 frames the hold lasts -- "the last positions",
    /// in the sky's frame.
    fn motion(&self) -> LayerMotion {
        LayerMotion {
            m: match self.frame {
                RefFrame::Inertial => teme_to_sphere(self.scene_unix),
                RefFrame::EarthFixed => glam::DMat3::IDENTITY,
            },
            s: self.s,
            delta_s: self.delta_s as f32,
            dim: if self.hold { 0.5 } else { 1.0 },
            rate: self.rate as f32,
        }
    }
}

type Emphasis = (Option<usize>, Option<usize>, Option<usize>);

struct AltitudeLayer {
    id: String,
    /// How many objects the layer has.
    count: usize,
    /// Keyframed: the scheduler, the worker and the held keyframes.
    keyed: Option<Keyed>,
    style: AltitudeStyle,
    /// Empty: all visible.
    visible: Vec<bool>,
    emphasis: Emphasis,
    /// One instance per object, in object order, before the emphasis
    /// reorder: what the hit test and the state queries read. Its flags are
    /// the paint and "visible" (masked in AND existing: finite, and not
    /// `hidden_s = [1, 0]`), never an emphasis bit. Rebuilt only when the
    /// positions change (a new static set, a new keyframe pair).
    objects: Vec<PointInstance>,
    /// The main draw: `objects` with the emphasised objects' visible bit
    /// cleared (their copies draw in `emphasis_instances`). A mask flips
    /// bits in place; an emphasis change touches at most six entries.
    instances: Vec<PointInstance>,
    emphasis_instances: Vec<PointInstance>,
    /// Bumped whenever the instances change, so the pipeline re-uploads
    /// only then (spec §2.8: "re-uploaded only when a toggle changes").
    generation: u64,
}

/// A line layer (`MapView::set_altitude_lines`): orbit rings. Every line is
/// drawn at the layer's one width, which rides in the layer's own uniform
/// beside its `M` (spec §2.9: two inter-stage slots).
struct LineLayer {
    id: String,
    frame: RefFrame,
    width_px: f32,
    look: AltitudeLook,
    segments: Vec<LineInstance>,
    generation: u64,
    /// The GPU side and the generation it holds: rebuilt when the segments
    /// change, otherwise only its uniform is rewritten per frame.
    batch: Option<(u64, LineBatch)>,
}

/// Every altitude layer, in the order first set (later draws on top).
#[derive(Default)]
pub(crate) struct AltitudeLayers {
    layers: Vec<AltitudeLayer>,
    /// Line layers, in the order first set; all drawn before every point
    /// layer, so dots sit on their rings.
    lines: Vec<LineLayer>,
    /// The next generation to hand out: unique across layers, so a pipeline
    /// slot that held another layer's instances never mistakes them.
    next_generation: u64,
    /// The last [`Self::advance`]'s "a layer is waiting on a keyframe".
    pub(crate) waiting: bool,
    /// Line looks by id ([`Self::set_line_look`]); unlisted: flat.
    line_looks: Vec<(String, AltitudeLook)>,
    /// The frame the layers were last drawn with (before each layer's own
    /// motion), for the hit test and the screen queries: what the user
    /// sees, not a recomputation for "now". `None` until the first globe
    /// frame, and again after a flat one (nothing is drawn there).
    drawn: Option<AltitudeFrame>,
    /// [`inertial_px_per_rad`] of the inertial line layers as last drawn;
    /// 0 when none was drawn.
    pub(crate) inertial_px_per_rad: f64,
    /// When the delayed wake queued for the moving dots or the inertial
    /// lines fires; a later paint queues another only if it needs one
    /// sooner, or this one has fired.
    pub(crate) redraw_wake_at: Option<std::time::Instant>,
    /// [`AltitudeFrame::dot_motion`] of the keyframed layers as last drawn
    /// under a running clock, with their keyframe events
    /// ([`AltitudeLayers::keyed_motion`]); still when none was (or paused).
    pub(crate) dot_motion: DotMotion,
    /// The last [`Self::advance`]'s "a layer is holding" (drawing its last
    /// pair dimmed, or nothing, until a keyframe lands): the one state
    /// that asks for every frame. A prefetch in flight alone does not --
    /// the pair-end event wakes a paint in time to take it.
    pub(crate) holding: bool,
}

impl AltitudeLayers {
    fn bump(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Replaces layer `id`'s objects in place (keeping its draw position),
    /// or adds it on top. Clears its mask and emphasis.
    pub(crate) fn set(&mut self, id: &str, positions: AltitudePositions, style: AltitudeStyle) {
        let emphasis = (None, None, None);
        let generation = self.bump();
        let (sphere, keyed) = match positions {
            AltitudePositions::Static(points) => (points.into_iter().map(geo_to_sphere).collect::<Vec<DVec3>>(), None),
            AltitudePositions::Keyframed { frame, source } => {
                let worker = Worker::spawn(Arc::clone(&source));
                if worker.is_none() {
                    eprintln!("gpui-maps: altitude layer {id:?}: could not start its keyframe thread; it will not draw");
                }
                let keyed = Keyed {
                    frame,
                    source,
                    scheduler: KeyframeScheduler::new(KEYFRAME_DELTA_MIN_S),
                    store: Vec::new(),
                    pair: None,
                    s: 0.0,
                    delta_s: KEYFRAME_DELTA_MIN_S,
                    hold: true,
                    scene_unix: 0.0,
                    rate: 0.0,
                    worker,
                };
                (Vec::new(), Some(keyed))
            }
        };
        let count = keyed.as_ref().map_or(sphere.len(), |k| k.source.len());
        // Static positions are converted once, here. A keyframed layer has
        // no objects until its first pair lands.
        let objects: Vec<PointInstance> = sphere.iter().enumerate().map(|(i, &p)| static_instance(p, pack_flags(style.paint_of(i), true, false, false, false))).collect();
        let mut layer = AltitudeLayer {
            id: id.to_string(),
            count,
            keyed,
            style,
            visible: Vec::new(),
            emphasis,
            objects,
            instances: Vec::new(),
            emphasis_instances: Vec::new(),
            generation,
        };
        layer.derive_draws();
        match self.layers.iter_mut().find(|l| l.id == id) {
            Some(existing) => *existing = layer,
            None => self.layers.push(layer),
        }
        // The last drawn frame no longer describes these objects: a query
        // before the next paint answers None rather than a stale object.
        self.drawn = None;
    }

    /// Replaces line layer `id` in place, or adds it on top of the other
    /// line layers. `lines` are metres in `frame` (see [`AltitudeLine`]).
    pub(crate) fn set_lines(&mut self, id: &str, frame: RefFrame, width_px: f32, lines: &[AltitudeLine]) {
        let generation = self.bump();
        let look = self.line_looks.iter().find(|(k, _)| k == id).map_or(AltitudeLook::Flat, |(_, l)| *l);
        let layer = LineLayer { id: id.to_string(), frame, width_px, look, segments: line_instances(lines), generation, batch: None };
        match self.lines.iter_mut().find(|l| l.id == id) {
            Some(existing) => *existing = layer,
            None => self.lines.push(layer),
        }
    }

    /// Line layer `id`'s look, now and for every later `set_lines` of that
    /// id (kept across `remove`).
    pub(crate) fn set_line_look(&mut self, id: &str, look: AltitudeLook) {
        match self.line_looks.iter_mut().find(|(k, _)| k == id) {
            Some(entry) => entry.1 = look,
            None => self.line_looks.push((id.to_string(), look)),
        }
        if let Some(l) = self.lines.iter_mut().find(|l| l.id == id) {
            if l.look != look {
                l.look = look;
                l.batch = None;
            }
        }
    }

    /// Whether a line layer is inertial and non-empty: it turns with the
    /// scene clock, so a running clock must repaint it -- but only as
    /// often as it moves a visible amount ([`inertial_redraw_after_s`]).
    pub(crate) fn has_inertial_lines(&self) -> bool {
        self.lines.iter().any(|l| l.frame == RefFrame::Inertial && !l.segments.is_empty())
    }

    /// Advances every keyframed layer to scene time `scene` under clock
    /// `rate`: takes the keyframes the workers finished, asks for the next,
    /// and rebuilds a layer's instances when its pair changes (once per
    /// interval). Returns `(any keyframed layer, any waiting on a
    /// keyframe)`, for [`wants_frames`].
    pub(crate) fn advance(&mut self, scene: f64, rate: f64) -> (bool, bool) {
        let (mut any, mut waiting, mut holding) = (false, false, false);
        for i in 0..self.layers.len() {
            let layer = &mut self.layers[i];
            let id = &layer.id;
            let Some(keyed) = layer.keyed.as_mut() else { continue };
            let Some(worker) = keyed.worker.as_ref() else { continue };
            let mut dead = false;
            loop {
                match worker.results.try_recv() {
                    Ok(k) => {
                        if keyed.scheduler.completed(k.t_unix) {
                            keyed.store.push(k);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            }
            let tick = keyed.scheduler.tick(scene, rate);
            let sched = &keyed.scheduler;
            keyed.store.retain(|k| sched.holds(k.t_unix));
            if let Some(t) = tick.request {
                dead |= worker.requests.send(t).is_err();
            }
            if dead {
                // The source panicked and took the thread with it. The
                // request in flight will never be answered: drop the worker,
                // so the layer stops counting as keyframed (no more frames
                // asked for) and freezes where it last drew -- nothing, if
                // it never drew.
                eprintln!("gpui-maps: altitude layer {id:?}: its keyframe thread died; the layer is frozen");
                keyed.worker = None;
                keyed.hold = true;
                continue;
            }
            any = true;
            keyed.scene_unix = scene;
            keyed.rate = rate;
            let pair = tick.pair.filter(|&(t0, t1)| keyed.keyframe(t0).is_some() && keyed.keyframe(t1).is_some());
            let mut rebuild = false;
            match pair {
                Some(pair) => {
                    keyed.s = tick.s;
                    keyed.hold = false;
                    if keyed.pair != Some(pair) {
                        keyed.pair = Some(pair);
                        keyed.delta_s = keyed.scheduler.delta_s();
                        rebuild = true;
                    }
                }
                // Hold: the last instances, at the last `s`, dimmed.
                None => keyed.hold = true,
            }
            holding |= keyed.hold;
            waiting |= keyed.hold || keyed.scheduler.waiting();
            if rebuild {
                let generation = self.bump();
                let layer = &mut self.layers[i];
                layer.rebuild_keyed();
                layer.generation = generation;
            }
        }
        self.waiting = waiting;
        self.holding = holding;
        (any, waiting)
    }
    /// Removes layer `id`: its points and its lines. Returns whether
    /// anything was removed.
    pub(crate) fn remove(&mut self, id: &str) -> bool {
        let before = (self.layers.len(), self.lines.len());
        self.layers.retain(|l| l.id != id);
        self.lines.retain(|l| l.id != id);
        let removed = (self.layers.len(), self.lines.len()) != before;
        if removed {
            self.drawn = None;
            if !self.has_keyframed() {
                self.waiting = false;
                self.holding = false;
            }
        }
        removed
    }

    /// Replaces layer `id`'s style (colours, sizes, look, trails), keeping
    /// its positions, keyframes, mask and emphasis. Returns whether the
    /// layer exists.
    pub(crate) fn set_style(&mut self, id: &str, style: AltitudeStyle) -> bool {
        let generation = self.bump();
        let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) else { return false };
        for (i, o) in layer.objects.iter_mut().enumerate() {
            o.flags = (o.flags & !15) | u32::from(style.paint_of(i));
        }
        layer.style = style;
        layer.derive_draws();
        layer.generation = generation;
        true
    }

    /// Flips only the visible bit on the kept instances; never reconverts a
    /// position. A mask whose length is not the layer's object count is
    /// ignored, with a warning. Returns whether anything changed.
    pub(crate) fn set_visibility(&mut self, id: &str, mask: &[bool]) -> bool {
        let generation = self.bump();
        let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) else { return false };
        if mask.len() != layer.count {
            eprintln!("gpui-maps: altitude layer {id:?} has {} objects; ignoring a mask of {}", layer.count, mask.len());
            return false;
        }
        layer.visible = mask.to_vec();
        let emphasis = layer.emphasis;
        for (i, (object, instance)) in layer.objects.iter_mut().zip(layer.instances.iter_mut()).enumerate() {
            let shown = layer.visible[i] && exists(object);
            object.flags = (object.flags & !FLAG_VISIBLE) | if shown { FLAG_VISIBLE } else { 0 };
            instance.flags = main_draw_flags(object, i, emphasis);
        }
        layer.emphasis_instances = emphasis_copies(&layer.objects, &layer.style, emphasis);
        layer.generation = generation;
        true
    }

    /// Moves at most three objects into the emphasis draw: their main copies
    /// lose [`FLAG_VISIBLE`], and emphasised copies are drawn last and
    /// larger. Returns whether anything changed.
    pub(crate) fn set_emphasis(&mut self, id: &str, emphasis: Emphasis) -> bool {
        let generation = self.bump();
        let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) else { return false };
        if layer.emphasis == emphasis {
            return false;
        }
        let old = layer.emphasis;
        layer.emphasis = emphasis;
        // A hover touches at most six main-draw flags and rebuilds the (at
        // most three) emphasis copies from the cached objects: no
        // re-propagation, no re-bisection, no O(N) rebuild.
        for i in [old.0, old.1, old.2, emphasis.0, emphasis.1, emphasis.2].into_iter().flatten() {
            if let (Some(instance), Some(object)) = (layer.instances.get_mut(i), layer.objects.get(i)) {
                instance.flags = main_draw_flags(object, i, emphasis);
            }
        }
        layer.emphasis_instances = emphasis_copies(&layer.objects, &layer.style, emphasis);
        layer.generation = generation;
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.instances.is_empty()) && self.lines.iter().all(|l| l.segments.is_empty())
    }

    /// Whether any layer is keyframed with a live worker (so the scene
    /// clock moves it). A layer whose worker died is frozen and counts not.
    pub(crate) fn has_keyframed(&self) -> bool {
        self.layers.iter().any(|l| l.keyed.as_ref().is_some_and(|k| k.worker.is_some()))
    }

    /// [`Self::has_keyframed`], counting only a layer whose mask shows at
    /// least one object: a keyframed layer masked off entirely (every
    /// group chip off) draws no dot, so a running clock owes it no frame
    /// (final review M3). Its keyframes still advance on any paint. O(1)
    /// for a layer with anything shown (the scan stops at the first shown
    /// object); an all-hidden mask is scanned once per paint, and asks for
    /// no paints.
    pub(crate) fn has_shown_keyframed(&self) -> bool {
        self.layers.iter().any(|l| {
            l.keyed.as_ref().is_some_and(|k| k.worker.is_some()) && (l.visible.is_empty() || l.visible.iter().any(|&v| v))
        })
    }

    /// Hands every layer to `pipeline` for this frame (one slot per point
    /// layer, each with its own palette; one [`LineBatch`] per line layer,
    /// with its own `M` -- `teme_to_sphere(scene_unix)` when inertial -- and
    /// width). Returns whether there is anything to draw.
    pub(crate) fn prepare(&mut self, pipeline: &mut AltitudePipeline, ctx: &GpuContext, frame: &AltitudeFrame, scene_unix: f64) -> bool {
        self.drawn = Some(*frame);
        let mut any_lines = false;
        self.inertial_px_per_rad = 0.0;
        for l in &mut self.lines {
            if l.segments.is_empty() {
                l.batch = None;
                continue;
            }
            any_lines = true;
            let m = match l.frame {
                RefFrame::Inertial => teme_to_sphere(scene_unix),
                RefFrame::EarthFixed => glam::DMat3::IDENTITY,
            };
            let own = AltitudeFrame { m, ..*frame };
            if l.frame == RefFrame::Inertial {
                self.inertial_px_per_rad = self.inertial_px_per_rad.max(inertial_px_per_rad(&l.segments, &own));
            }
            match l.batch.as_mut() {
                Some((generation, batch)) if *generation == l.generation => pipeline.refresh_lines(ctx, batch, &own, l.width_px),
                _ => l.batch = Some((l.generation, pipeline.prepare_lines(ctx, &own, l.width_px, l.look, &l.segments))),
            }
        }
        let batches: Vec<AltitudeBatch<'_>> = self
            .layers
            .iter()
            .filter(|l| !l.instances.is_empty())
            .map(|l| AltitudeBatch {
                style: &l.style,
                main: &l.instances,
                emphasis: &l.emphasis_instances,
                generation: Some(l.generation),
                motion: l.keyed.as_ref().map(Keyed::motion),
            })
            .collect();
        pipeline.prepare_layers(ctx, frame, &batches);
        self.note_motion(frame);
        pipeline.points_drawn() > 0 || any_lines
    }

    /// This frame's draws, in order: every line layer, then the points
    /// (`AltitudePipeline::draw_points`), so dots sit on their rings.
    pub(crate) fn draw(&self, pipeline: &AltitudePipeline, pass: &mut wgpu::RenderPass<'_>) {
        for l in &self.lines {
            if let Some((_, batch)) = l.batch.as_ref().filter(|_| !l.segments.is_empty()) {
                pipeline.draw_lines(pass, batch);
            }
        }
        pipeline.draw_points(pass);
    }

    /// Forgets the drawn frame: this frame draws no altitude layer (flat).
    pub(crate) fn not_drawn(&mut self) {
        self.drawn = None;
        self.inertial_px_per_rad = 0.0;
        self.dot_motion = DotMotion::STILL;
    }

    /// Records how the keyframed layers move on `frame`
    /// ([`Self::keyed_motion`]), for the next redraw's wait. `prepare`
    /// calls it; so does a headless test paint, which has no pipeline.
    pub(crate) fn note_motion(&mut self, frame: &AltitudeFrame) {
        self.dot_motion = self.keyed_motion(frame);
    }

    /// How the live keyframed layers move on `frame` (each with its own
    /// motion, [`AltitudeFrame::dot_motion`]), with each layer's keyframe
    /// events ahead in the clock's direction: the prefetch trigger
    /// (`s = 0.5`, where [`KeyframeScheduler::tick`] asks for the next
    /// keyframe) and the pair end (`s = 1`, or 0 in reverse). Still when
    /// paused -- nothing moves, and nothing is scanned.
    fn keyed_motion(&self, frame: &AltitudeFrame) -> DotMotion {
        self.layers
            .iter()
            .filter_map(|l| {
                let keyed = l.keyed.as_ref().filter(|k| k.worker.is_some() && k.rate != 0.0 && !k.rate.is_nan())?;
                let (s, delta) = (f64::from(keyed.s), keyed.delta_s);
                let (trigger, end) = if keyed.rate > 0.0 { ((0.5 - s) * delta, (1.0 - s) * delta) } else { ((s - 0.5) * delta, s * delta) };
                let motion = l.frame_on(frame).dot_motion(&l.objects, keyed.frame == RefFrame::Inertial, keyed.rate);
                Some(motion.with_event(trigger).with_event(end))
            })
            .fold(DotMotion::STILL, DotMotion::merge)
    }

    /// Layer `id`'s `(hovered, selected, halo)` emphasis.
    pub(crate) fn emphasis(&self, id: &str) -> Option<Emphasis> {
        self.layers.iter().find(|l| l.id == id).map(|l| l.emphasis)
    }

    /// The object under `(x, y)` (logical px) in the last drawn frame:
    /// topmost layer first, the nearest centre within [`HIT_RADIUS_PX`] in
    /// that layer (ties to the dot drawn on top). Lines are not hit.
    pub(crate) fn hit(&self, x: f32, y: f32) -> Option<(String, usize)> {
        self.hit_within(x, y, HIT_RADIUS_PX)
    }

    /// [`Self::hit`] within `radius_px` (logical px): a finger's reach on a
    /// touch screen, where [`HIT_RADIUS_PX`] is a pointer's.
    pub(crate) fn hit_within(&self, x: f32, y: f32, radius_px: f32) -> Option<(String, usize)> {
        let drawn = self.drawn?;
        self.layers.iter().rev().find_map(|l| {
            hit_test(&l.frame_on(&drawn), &l.objects, x, y, radius_px).map(|i| (l.id.clone(), i))
        })
    }

    /// Object `i` of layer `id` where it was last drawn, on screen (logical
    /// px): `None` if it was not drawn (masked off, not existing at this
    /// `s`, hidden by the planet, behind the eye) or nothing was drawn.
    pub(crate) fn screen(&self, id: &str, i: usize) -> Option<(f32, f32)> {
        let drawn = self.drawn?;
        let l = self.layers.iter().find(|l| l.id == id)?;
        let f = l.frame_on(&drawn);
        f.project(f.position(l.objects.get(i)?)?)
    }

    /// Object `i` of layer `id` where it was last drawn, as a sub-point and
    /// altitude. Unlike [`Self::screen`], an object behind the planet or the
    /// eye still has a state (a followed satellite keeps its card as it
    /// passes behind the Earth); one masked off or not existing does not.
    pub(crate) fn state(&self, id: &str, i: usize) -> Option<AltitudePointState> {
        let drawn = self.drawn?;
        let l = self.layers.iter().find(|l| l.id == id)?;
        l.frame_on(&drawn).placed(l.objects.get(i)?).map(AltitudePointState::from_sphere)
    }

    #[cfg(test)]
    fn layer(&self, id: &str) -> &AltitudeLayer {
        self.layers.iter().find(|l| l.id == id).expect("the layer exists")
    }
}

/// Whether an object has a drawable state at all: finite positions, and a
/// non-empty `hidden_s` window (`keyed_instance` writes `[1, 0]` for none).
fn exists(object: &PointInstance) -> bool {
    object.hidden_s[0] <= object.hidden_s[1] && object.p0.iter().chain(&object.p1).all(|c| c.is_finite())
}

/// Object `i`'s flags in the main draw: its own, minus the visible bit when
/// it is emphasised (its copy in the emphasis draw stands in for it).
fn main_draw_flags(object: &PointInstance, i: usize, emphasis: Emphasis) -> u32 {
    let (h, s, g) = emphasis_of(i, emphasis);
    if h || s || g { object.flags & !FLAG_VISIBLE } else { object.flags }
}

/// The emphasis draw: at most three copies, one per distinct emphasised
/// index in hovered, selected, halo order, each the object's own instance
/// with its state bits set. An index past the end is ignored.
fn emphasis_copies(objects: &[PointInstance], style: &AltitudeStyle, emphasis: Emphasis) -> Vec<PointInstance> {
    let mut out: Vec<PointInstance> = Vec::with_capacity(3);
    let mut seen: Vec<usize> = Vec::with_capacity(3);
    for i in [emphasis.0, emphasis.1, emphasis.2].into_iter().flatten() {
        let Some(object) = objects.get(i) else { continue };
        if seen.contains(&i) {
            continue;
        }
        seen.push(i);
        let (h, s, g) = emphasis_of(i, emphasis);
        out.push(PointInstance { flags: pack_flags(style.paint_of(i), object.flags & FLAG_VISIBLE != 0, h, s, g), ..*object });
    }
    out
}

/// Where an altitude object was drawn, geocentric: the drawn sphere point,
/// its sub-point in degrees and its height above the globe's sphere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AltitudePointState {
    /// The unit-sphere frame position (Earth radii).
    pub sphere: DVec3,
    /// The sub-point's longitude, degrees east.
    pub lon: f64,
    /// The sub-point's geocentric latitude, degrees north.
    pub lat: f64,
    /// Height above the globe's sphere (`GLOBE_RADIUS_M`), metres.
    pub alt_m: f64,
}

impl AltitudePointState {
    /// The inverse of `render::altitude::geo_to_sphere`.
    #[must_use]
    pub fn from_sphere(p: DVec3) -> Self {
        let r = p.length();
        Self { sphere: p, lon: p.x.atan2(p.z).to_degrees(), lat: (p.y / r).asin().to_degrees(), alt_m: (r - 1.0) * GLOBE_RADIUS_M }
    }
}

impl AltitudeLayer {
    /// The main and emphasis draws from `objects`, the mask and the
    /// emphasis: O(N) once, when the positions change.
    fn derive_draws(&mut self) {
        let emphasis = self.emphasis;
        self.instances = self.objects.iter().enumerate().map(|(i, o)| PointInstance { flags: main_draw_flags(o, i, emphasis), ..*o }).collect();
        self.emphasis_instances = emphasis_copies(&self.objects, &self.style, emphasis);
    }

    /// The last drawn frame as this layer drew it (its own `M`, `s` and
    /// interval when keyframed).
    fn frame_on(&self, drawn: &AltitudeFrame) -> AltitudeFrame {
        drawn.with_motion(self.keyed.as_ref().map(Keyed::motion))
    }

    /// A keyframed layer's objects (and so its draws) from its current
    /// pair: the window each object exists over (bisected only where its
    /// validity differs between the ends), then `keyed_instance` per
    /// object. About 600 KB for 10k objects, once per interval -- only on a
    /// pair change, never on a hover or a mask. Nothing to do before the
    /// first pair.
    fn rebuild_keyed(&mut self) {
        let Some(keyed) = self.keyed.as_mut() else { return };
        let Some((t0, t1)) = keyed.pair else { return };
        let (Some(k0), Some(k1)) = (keyed.keyframe(t0), keyed.keyframe(t1)) else { return };
        let delta = t1 - t0;
        let hidden: Vec<[f32; 2]> = k0
            .states
            .iter()
            .zip(&k1.states)
            .enumerate()
            .map(|(i, (a, b))| {
                if a.valid == b.valid {
                    if a.valid { [0.0, 1.0] } else { [1.0, 0.0] }
                } else {
                    hidden_bounds(&*keyed.source, i, a, b, t0, delta)
                }
            })
            .collect();
        let (objects, _) = keyed_instances_for(&k0.states, &k1.states, delta, &hidden, &self.style, &self.visible, (None, None, None));
        self.objects = objects;
        self.derive_draws();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::altitude_pipeline::{dot_redraw_after_s, FLAG_HALO, FLAG_HOVERED, FLAG_SELECTED};

    fn points(n: usize) -> AltitudePositions {
        AltitudePositions::Static((0..n).map(|i| GeoPoint { lon: i as f64, lat: 0.0, alt_m: 550_000.0 }).collect())
    }

    fn visible(l: &AltitudeLayer) -> Vec<bool> {
        l.instances.iter().map(|i| i.flags & FLAG_VISIBLE != 0).collect()
    }

    #[test]
    fn a_mask_flips_flag_bits_and_keeps_positions() {
        let mut layers = AltitudeLayers::default();
        let style = AltitudeStyle { paint: vec![3, 20, 1], ..Default::default() };
        layers.set("sats", points(3), style);
        let before: Vec<_> = layers.layer("sats").instances.iter().map(|i| i.p0).collect();
        assert_eq!(layers.layer("sats").instances[1].flags & 15, 14, "paint 20 clamps to 14");
        let g0 = layers.layer("sats").generation;
        assert!(layers.set_visibility("sats", &[true, false, true]));
        let l = layers.layer("sats");
        assert_eq!(visible(l), vec![true, false, true]);
        assert_eq!(l.instances.iter().map(|i| i.p0).collect::<Vec<_>>(), before);
        assert_eq!(l.instances[0].flags & 15, 3, "paint kept");
        assert_ne!(l.generation, g0, "a changed mask re-uploads");
        // Wrong length: ignored.
        assert!(!layers.set_visibility("sats", &[false]));
        assert_eq!(visible(layers.layer("sats")), vec![true, false, true]);
    }

    #[test]
    fn emphasis_moves_objects_into_their_own_draw_and_back() {
        let mut layers = AltitudeLayers::default();
        layers.set("sats", points(4), AltitudeStyle::default());
        assert!(layers.set_emphasis("sats", (Some(1), Some(1), Some(3))));
        let l = layers.layer("sats");
        assert_eq!(visible(l), vec![true, false, true, false]);
        assert_eq!(l.emphasis_instances.len(), 2, "one copy per distinct index");
        assert_eq!(l.emphasis_instances[0].flags & (FLAG_HOVERED | FLAG_SELECTED | FLAG_HALO), FLAG_HOVERED | FLAG_SELECTED);
        assert_eq!(l.emphasis_instances[1].flags & FLAG_HALO, FLAG_HALO);
        // A mask hides an emphasised object's copy too.
        layers.set_visibility("sats", &[true, true, true, false]);
        let l = layers.layer("sats");
        assert_eq!(l.emphasis_instances[1].flags & FLAG_VISIBLE, 0);
        assert_eq!(visible(l), vec![true, false, true, false]);
        // Cleared: every masked-in object is back in the main draw.
        layers.set_emphasis("sats", (None, None, Some(99)));
        let l = layers.layer("sats");
        assert_eq!(visible(l), vec![true, true, true, false]);
        assert!(l.emphasis_instances.is_empty(), "an index past the end is ignored");
    }

    #[test]
    fn set_replaces_in_place_and_remove_drops() {
        let mut layers = AltitudeLayers::default();
        layers.set("a", points(2), AltitudeStyle::default());
        layers.set("b", points(1), AltitudeStyle::default());
        layers.set("a", points(5), AltitudeStyle::default());
        assert_eq!(layers.layers.iter().map(|l| (l.id.as_str(), l.instances.len())).collect::<Vec<_>>(), vec![("a", 5), ("b", 1)]);
        assert!(layers.remove("a") && !layers.remove("a"));
        assert!(!layers.is_empty());
        assert!(layers.remove("b"));
        assert!(layers.is_empty());
    }

    #[test]
    fn the_clock_requests_frames_only_while_running_or_waiting() {
        assert!(wants_frames(1.0, true, false));
        assert!(wants_frames(-60.0, true, false));
        assert!(!wants_frames(0.0, true, false), "paused: no frames, no battery");
        assert!(wants_frames(0.0, true, true), "paused mid-scrub: keep painting until the keyframe lands");
        assert!(!wants_frames(1.0, false, false), "no keyframed layer: nothing moves");
    }

    #[test]
    fn scene_time_runs_at_the_rate() {
        let t0 = crate::timing::Instant::now();
        let c = SceneClock { anchor_scene_unix: 1_000.0, anchor_wall: t0, rate: 60.0 };
        assert!((c.at(t0.after(std::time::Duration::from_secs(2))) - 1_120.0).abs() < 1e-9);
        let paused = SceneClock { rate: 0.0, ..c };
        assert_eq!(paused.at(t0.after(std::time::Duration::from_secs(5))), 1_000.0);
        // Before the anchor: negative elapsed time, never a panic.
        let later = SceneClock { anchor_wall: t0.after(std::time::Duration::from_secs(10)), ..c };
        assert!((later.at(t0) - 400.0).abs() < 1e-9);
    }

    /// A circular 1-radius-per-minute object, valid until `until`.
    struct Circle(f64);
    impl KeyframeSource for Circle {
        fn len(&self) -> usize {
            2
        }
        fn states_at(&self, t: f64, range: std::ops::Range<usize>, out: &mut [crate::render::altitude::KeyState]) {
            use crate::render::altitude::KeyState;
            for (o, i) in out.iter_mut().zip(range) {
                *o = if i == 1 && t > self.0 {
                    KeyState::HIDDEN
                } else {
                    KeyState { position_m: glam::DVec3::new(7.0e6, 0.0, t), velocity_m_s: glam::DVec3::Z, valid: true }
                };
            }
        }
    }

    /// Advances until the layer stops holding AND no keyframe request is
    /// still in flight, with a deadline: the worker is a real thread.
    ///
    /// Waiting for `hold` alone is not enough: landing a pair can itself
    /// fire an eager prefetch for the interval after next (`tick`'s `s >
    /// 0.5` branch), and that request is still in flight the instant `hold`
    /// clears. A caller that reads `waiting` right after `settle` would
    /// otherwise race the worker thread answering that prefetch -- its
    /// timing (and so whether `try_recv` already saw the result) depends on
    /// OS thread scheduling, flaky under load and clean on an idle machine.
    /// Draining `scheduler.waiting()` here, not just in the caller, makes
    /// every `settle` leave a fully quiesced scheduler.
    fn settle(layers: &mut AltitudeLayers, scene: f64, rate: f64) {
        let started = crate::timing::Instant::now();
        loop {
            layers.advance(scene, rate);
            let keyed = layers.layer("sats").keyed.as_ref().unwrap();
            if !keyed.hold && !keyed.scheduler.waiting() {
                return;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the keyframe worker never answered");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_keyframed_layer_draws_once_its_pair_lands_and_survives_a_scrub_backwards() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Circle(1_000_000.0));
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        assert!(layers.layer("sats").instances.is_empty(), "nothing before the first pair");
        assert_eq!(layers.advance(1_000_010.0, 1.0), (true, true), "the first frame holds and waits");
        settle(&mut layers, 1_000_010.0, 1.0);
        let l = layers.layer("sats");
        assert_eq!(l.instances.len(), 2);
        assert_eq!(l.instances[0].hidden_s, [0.0, 1.0]);
        // Object 1 decays at 1_000_000, 40 s into the interval 999_960..1_000_020.
        let k = l.keyed.as_ref().unwrap();
        let (t0, t1) = k.pair.unwrap();
        assert!(t0 <= 1_000_010.0 && 1_000_010.0 < t1);
        let want = ((1_000_000.0 - t0) / (t1 - t0)) as f32;
        assert!((l.instances[1].hidden_s[1] - want).abs() <= 1.0 / 64.0, "{:?} vs {want}", l.instances[1].hidden_s);
        assert!(k.motion().m != glam::DMat3::IDENTITY, "inertial: rotated by the scene time");
        // Scrubbed back a day: holds (dimmed), then recovers, never panics.
        layers.advance(913_600.0, 1.0);
        assert_eq!(layers.layer("sats").keyed.as_ref().unwrap().motion().dim, 0.5);
        settle(&mut layers, 913_600.0, 1.0);
        let (t0, _) = layers.layer("sats").keyed.as_ref().unwrap().pair.unwrap();
        assert!(t0 <= 913_600.0);
        // Paused: once any prefetch in flight has landed, no frames wanted.
        let started = crate::timing::Instant::now();
        while layers.advance(913_600.0, 0.0).1 {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "still waiting while paused");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!wants_frames(0.0, layers.has_keyframed(), layers.waiting));
        // Dropping the layer drops the worker's sender, so its thread exits.
        assert!(layers.remove("sats"));
    }

    /// A ring of LEO-like objects: circular, 6,900 km from the centre, in
    /// planes tilted `k` degrees apart, each at 7.6 km/s.
    struct Leo;
    impl KeyframeSource for Leo {
        fn len(&self) -> usize {
            36
        }
        fn states_at(&self, t: f64, range: std::ops::Range<usize>, out: &mut [crate::render::altitude::KeyState]) {
            use crate::render::altitude::KeyState;
            let r = 6.9e6;
            let w = 7_600.0 / r;
            for (o, i) in out.iter_mut().zip(range) {
                let tilt = (i as f64 * 10.0).to_radians();
                let a = w * t + i as f64;
                let (x, y) = (r * a.cos(), r * a.sin());
                let (vx, vy) = (-r * w * a.sin(), r * w * a.cos());
                *o = KeyState {
                    position_m: DVec3::new(x, y * tilt.cos(), y * tilt.sin()),
                    velocity_m_s: DVec3::new(vx, vy * tilt.cos(), vy * tilt.sin()),
                    valid: true,
                };
            }
        }
    }

    /// A globe frame at `(zoom, pitch)` on a 1024x1024 LOGICAL target at
    /// device scale `scale`, as `MapView::paint` builds it.
    fn frame_at(zoom: f64, pitch: f32, scale: f32) -> AltitudeFrame {
        let c = crate::render::Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            pitch,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let g = crate::render::globe::globe_frame(&c);
        let f = AltitudeFrame::new(&c, &g, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame");
        AltitudeFrame { px_scale: scale, viewport: (1024.0 * scale, 1024.0 * scale), ..f }
    }

    /// Layer `sats`'s objects at scene time `t`, DEVICE px, as the frame
    /// `frame` would draw them with the layer's own motion at `t` (`None`:
    /// not drawn -- hidden, behind the planet or the eye).
    fn device_positions(layers: &AltitudeLayers, frame: &AltitudeFrame, t: f64) -> Vec<Option<(f64, f64)>> {
        let l = layers.layer("sats");
        let k = l.keyed.as_ref().unwrap();
        let f = AltitudeFrame {
            m: teme_to_sphere(t),
            s: ((t - k.pair.unwrap().0) / k.delta_s) as f32,
            delta_s: k.delta_s as f32,
            ..*frame
        };
        let scale = f64::from(frame.px_scale);
        l.objects.iter().map(|o| f.position(o).and_then(|p| f.project(p)).map(|(x, y)| (f64::from(x) * scale, f64::from(y) * scale))).collect()
    }

    fn on_screen(p: (f64, f64), frame: &AltitudeFrame) -> bool {
        (0.0..=f64::from(frame.viewport.0)).contains(&p.0) && (0.0..=f64::from(frame.viewport.1)).contains(&p.1)
    }

    /// Perf (sats idle CPU): moving dots at rate 1 on the whole globe no
    /// longer ask for every display refresh -- each asked-for frame redraws
    /// the host's whole gpui window. They ask once the fastest on-screen
    /// dot has moved `DOT_REDRAW_PX`, which the speed bound over-estimates
    /// (never late) but not wildly; on a 1x and a 2x target, flat and at
    /// pitch 45. A fast clock or a close camera is every frame again.
    #[test]
    fn moving_dots_ask_for_a_frame_only_once_they_moved_a_quarter_pixel() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Leo);
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        let scene = 1_000_010.0;
        settle(&mut layers, scene, 1.0);
        for (zoom, pitch, scale) in [(0.0, 0.0, 1.0), (3.0, 0.0, 1.0), (3.0, 45.0, 2.0)] {
            let frame = frame_at(zoom, pitch, scale);
            let motion = layers.keyed_motion(&frame);
            let px = motion.px_per_scene_s;
            let wait = dot_redraw_after_s(1.0, motion).unwrap();
            assert!(wait > 1.0 / 30.0, "z{zoom} p{pitch} x{scale}: {motion:?} at rate 1 waits {wait:.3} s: no 120 Hz");
            assert!(px * wait <= DOT_REDRAW_PX_FOR_TEST * (1.0 + 1e-9), "the wait is at most the threshold's");
            // Never late: over the wait, no dot on screen at both ends
            // moves more than the bound says (device px).
            let (a, b) = (device_positions(&layers, &frame, scene), device_positions(&layers, &frame, scene + wait));
            let moved = a
                .iter()
                .zip(&b)
                .filter_map(|(a, b)| Some(((*a)?, (*b)?)))
                .filter(|(a, b)| on_screen(*a, &frame) && on_screen(*b, &frame))
                .map(|(a, b)| (a.0 - b.0).hypot(a.1 - b.1))
                .fold(0.0, f64::max);
            let tag = format!("z{zoom} p{pitch} x{scale}");
            assert!(moved > 0.0, "{tag}: the dots move");
            assert!(moved <= px * wait * 1.001, "{tag}: moved {moved} px in {wait} s, over the bound {}", px * wait);
            // And the bound is not wildly loose: within 4x of the real motion.
            assert!(moved * 4.0 >= px * wait, "{tag}: moved {moved} px, bound says {}", px * wait);
        }
        let motion = layers.keyed_motion(&frame_at(3.0, 0.0, 1.0));
        assert!(dot_redraw_after_s(1_000.0, motion).unwrap() <= 1.0 / 60.0, "a fast clock is every frame again");
        assert_eq!(dot_redraw_after_s(0.0, motion), None, "paused: never");
        assert_eq!(dot_redraw_after_s(f64::NAN, motion), None);
        assert_eq!(
            dot_redraw_after_s(1.0, DotMotion::STILL),
            Some(crate::render::altitude_pipeline::DOT_REDRAW_MAX_WAIT_S),
            "nothing shown, nothing due: a slow heartbeat"
        );
        // Paused: nothing is scanned, nothing moves, nothing is due.
        layers.advance(scene, 0.0);
        assert_eq!(layers.keyed_motion(&frame_at(3.0, 0.0, 1.0)), DotMotion::STILL);
    }

    /// Review 1: an off-screen dot is timed to enter the view, and the wait
    /// never outlasts that. At z3 some of the ring is off screen; over its
    /// time to enter, none of those (far enough from the eye to be timed)
    /// comes on screen -- and the wait is at most it.
    #[test]
    fn an_off_screen_dot_is_waited_for_no_longer_than_it_takes_to_enter() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Leo);
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        let scene = 1_000_010.0;
        settle(&mut layers, scene, 1.0);
        let frame = frame_at(4.0, 0.0, 2.0);
        let motion = layers.keyed_motion(&frame);
        assert!(motion.enter_scene_s.is_finite() && motion.enter_scene_s > 0.0, "{motion:?}: some dot is off screen at z4");
        let (a, b) = (device_positions(&layers, &frame, scene), device_positions(&layers, &frame, scene + motion.enter_scene_s * 0.999));
        let entered = a.iter().zip(&b).filter(|(a, b)| a.is_some_and(|a| !on_screen(a, &frame)) && b.is_some_and(|b| on_screen(b, &frame))).count();
        assert_eq!(entered, 0, "a dot entered before its time to enter");
        // The wait takes the entry into account.
        let early = DotMotion { px_per_scene_s: 1e-6, enter_scene_s: 0.1, edge_scene_s: f64::INFINITY };
        assert!((dot_redraw_after_s(1.0, early).unwrap() - 0.1).abs() < 1e-12);
        assert!((dot_redraw_after_s(-10.0, early).unwrap() - 0.01).abs() < 1e-12, "in scene seconds: a fast clock shortens it");
    }

    /// Review 2: the wait lands a paint just past the prefetch trigger
    /// (s = 0.5) -- so the next keyframe is asked for in time -- and past an
    /// object's appearance or decay; and a prefetch in flight asks for no
    /// frames of its own (only a hold does).
    #[test]
    fn the_wait_lands_a_paint_past_the_prefetch_trigger_and_a_decay() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Leo);
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        let rate = 100.0;
        settle(&mut layers, 1_000_010.0, rate);
        let (t0, delta) = {
            let k = layers.layer("sats").keyed.as_ref().unwrap();
            (k.pair.unwrap().0, k.delta_s)
        };
        // Just before the trigger, on the same pair.
        let before = t0 + (0.5 - 1e-4) * delta;
        settle(&mut layers, before, rate);
        let motion = layers.keyed_motion(&frame_at(0.0, 0.0, 1.0));
        let to_trigger = (t0 + 0.5 * delta - before) / rate;
        let wait = dot_redraw_after_s(rate, motion).unwrap();
        assert!(wait <= to_trigger + crate::render::altitude_pipeline::DOT_EVENT_SLACK_S + 1e-9, "{wait} s vs the trigger in {to_trigger} s");
        assert!(!layers.waiting, "nothing asked for before the trigger");
        // The paint that wait lands asks for the next keyframe -- and no
        // frames while it is in flight: it is not a hold.
        layers.advance(before + (to_trigger + crate::render::altitude_pipeline::DOT_EVENT_SLACK_S) * rate, rate);
        let prefetched = layers.layer("sats").keyed.as_ref().unwrap().scheduler.holds(t0 + 2.0 * delta);
        assert!(layers.waiting || prefetched, "past the trigger: the next keyframe is asked for (or already held)");
        assert!(!layers.holding, "a prefetch is not a hold: no every-frame");

        // A decay ahead is an event too: `Circle`'s object 1 ends at 1_000_000.
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Circle(1_000_000.0));
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        settle(&mut layers, 999_995.0, 1.0);
        let k = layers.layer("sats").keyed.as_ref().unwrap();
        let (t0, delta) = (k.pair.unwrap().0, k.delta_s);
        assert!(t0 < 1_000_000.0 && 1_000_000.0 < t0 + delta, "the decay is inside the pair {t0}+{delta}");
        let motion = layers.keyed_motion(&frame_at(0.0, 0.0, 1.0));
        assert!((motion.edge_scene_s - 5.0).abs() <= delta / 64.0 + 1e-6, "{motion:?}: the decay 5 s ahead");
    }

    const DOT_REDRAW_PX_FOR_TEST: f64 = crate::render::altitude_pipeline::DOT_REDRAW_PX;

    /// Final review M3: a running clock with every object masked off asks
    /// for no frames; showing one object asks again.
    #[test]
    fn a_running_clock_with_everything_hidden_wants_no_frames() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Circle(1_000_000.0));
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        settle(&mut layers, 1_000_010.0, 1.0);
        assert!(wants_frames(1.0, layers.has_shown_keyframed(), layers.waiting), "shown and running: frames");
        assert!(layers.set_visibility("sats", &[false, false]));
        assert!(layers.has_keyframed(), "still keyframed: prefetch continues on any paint");
        assert!(!wants_frames(1.0, layers.has_shown_keyframed(), layers.waiting), "all hidden: no frames");
        assert!(!wants_frames(1.0, layers.has_shown_keyframed(), true), "not even while a keyframe is owed");
        assert!(layers.set_visibility("sats", &[false, true]));
        assert!(wants_frames(1.0, layers.has_shown_keyframed(), layers.waiting), "one shown: frames again");
        assert!(layers.remove("sats"));
    }

    #[test]
    fn with_rate_keeps_scene_time_continuous() {
        let t0 = crate::timing::Instant::now();
        let c = SceneClock { anchor_scene_unix: 1_000.0, anchor_wall: t0, rate: 1.0 };
        let now = t0.after(std::time::Duration::from_secs(60));
        let fast = c.with_rate(60.0, now);
        assert!((fast.at(now) - c.at(now)).abs() < 1e-9, "no jump at the change");
        assert!((fast.at(now.after(std::time::Duration::from_secs(1))) - 1_120.0).abs() < 1e-9);
        let naive = SceneClock { rate: 60.0, ..c };
        assert!((naive.at(now) - c.at(now)).abs() > 3_000.0, "the struct update jumps an hour; with_rate exists for this");
    }

    /// [`Circle`] that counts the `states_at` calls made from the thread
    /// that built it -- the paint thread's bisections, not the worker's
    /// keyframes.
    struct Counting {
        inner: Circle,
        owner: std::thread::ThreadId,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl KeyframeSource for Counting {
        fn len(&self) -> usize {
            self.inner.len()
        }
        fn states_at(&self, t: f64, range: std::ops::Range<usize>, out: &mut [crate::render::altitude::KeyState]) {
            if std::thread::current().id() == self.owner {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.states_at(t, range, out);
        }
    }

    /// Carry from Task 5's review (F4): a hover or a mask on a keyframed
    /// layer must not rebuild every instance nor re-run the decay
    /// bisections. Object 1 decays mid-interval, so building its window
    /// bisects on this thread; after that, a hover, a selection and a mask
    /// call the source zero times and touch only the flags they must.
    #[test]
    fn a_hover_on_a_keyframed_layer_does_not_repropagate_or_rebisect() {
        let mut layers = AltitudeLayers::default();
        let source = Arc::new(Counting {
            inner: Circle(1_000_000.0),
            owner: std::thread::current().id(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let dynamic: Arc<dyn KeyframeSource> = source.clone();
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source: dynamic }, AltitudeStyle::default());
        settle(&mut layers, 1_000_010.0, 0.0);
        let bisections = source.calls.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(bisections, 6, "building the pair bisects object 1's decay six times");
        let before = layers.layer("sats").instances.clone();
        let objects = layers.layer("sats").objects.clone();
        let hidden_before = objects[1].hidden_s;

        assert!(layers.set_emphasis("sats", (Some(1), None, None)));
        assert!(layers.set_emphasis("sats", (Some(0), Some(1), None)));
        assert!(layers.set_visibility("sats", &[true, false]));
        assert!(layers.set_visibility("sats", &[true, true]));
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::Relaxed), bisections, "a hover or a mask called the source");

        let l = layers.layer("sats");
        assert_eq!(l.objects, objects, "the cached objects (and their hidden_s windows) are untouched");
        assert_eq!(l.objects[1].hidden_s, hidden_before);
        // Only the flags of the emphasised objects moved; positions and
        // windows are the cached ones.
        for (a, b) in l.instances.iter().zip(&before) {
            assert_eq!(PointInstance { flags: 0, ..*a }, PointInstance { flags: 0, ..*b });
        }
        assert_eq!(l.instances[0].flags & FLAG_VISIBLE, 0, "hovered: drawn by its copy");
        assert_eq!(l.emphasis_instances.len(), 2);
        assert_eq!(l.emphasis_instances[1].hidden_s, hidden_before, "the copy keeps the cached window");
        // A new pair (the next interval) is what rebuilds.
        let generation = l.generation;
        settle(&mut layers, 1_000_100.0, 0.0);
        assert!(layers.layer("sats").generation > generation, "a new pair rebuilds the instances");
    }

    fn drawn_frame(zoom: f64) -> AltitudeFrame {
        let c = crate::render::Camera {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1024.0,
            viewport_height_px: 1024.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        };
        let g = crate::render::globe::globe_frame(&c);
        AltitudeFrame::new(&c, &g, glam::DMat3::IDENTITY, 0.0, 60.0, 1.0).expect("a globe frame")
    }

    #[test]
    fn the_queries_answer_from_the_last_drawn_frame_topmost_first() {
        let mut layers = AltitudeLayers::default();
        let p = |lon: f64, alt_m: f64| GeoPoint { lon, lat: 10.0, alt_m };
        layers.set("low", AltitudePositions::Static(vec![p(0.0, 550e3), p(180.0, 550e3)]), AltitudeStyle::default());
        layers.set("high", AltitudePositions::Static(vec![p(20.0, 20_200e3), p(0.0, 550e3)]), AltitudeStyle::default());
        assert_eq!(layers.hit(512.0, 512.0), None, "nothing drawn yet");
        assert_eq!(layers.state("low", 0), None);
        layers.drawn = Some(drawn_frame(0.0));

        // Both layers have an object at (0, 10, 550 km): the one on top wins.
        let (x, y) = layers.screen("low", 0).expect("drawn");
        assert_eq!(layers.screen("high", 1), Some((x, y)));
        assert_eq!(layers.hit(x + 2.0, y), Some(("high".to_string(), 1)));
        // Behind the planet: no screen spot, no hit, but a state.
        assert_eq!(layers.screen("low", 1), None);
        let s = layers.state("low", 1).expect("a state behind the planet");
        assert!((s.lon.abs() - 180.0).abs() < 1e-4 && (s.lat - 10.0).abs() < 1e-4, "{s:?}");
        assert!((s.alt_m - 550e3).abs() < 2.0, "{s:?}");
        // The drawn state of a visible object round-trips geo_to_sphere.
        let s = layers.state("high", 0).unwrap();
        assert!((s.lon - 20.0).abs() < 1e-4 && (s.lat - 10.0).abs() < 1e-4 && (s.alt_m - 20_200e3).abs() < 5.0, "{s:?}");
        // A masked-off object: no hit, no screen spot, no state.
        layers.set_visibility("high", &[true, false]);
        assert_eq!(layers.hit(x, y), Some(("low".to_string(), 0)), "the one under it is hit instead");
        assert_eq!((layers.screen("high", 1), layers.state("high", 1)), (None, None));
        // Emphasised objects stay hittable (their copy is the drawn dot).
        layers.set_emphasis("low", (Some(0), None, None));
        assert_eq!(layers.hit(x, y), Some(("low".to_string(), 0)));
        assert_eq!(layers.screen("nope", 0), None);
        assert_eq!(layers.screen("low", 9), None);
        // A flat frame draws nothing, so nothing is hit.
        layers.not_drawn();
        assert_eq!(layers.hit(x, y), None);
    }

    /// A group filter (the satellites panel's "Starlink only") is a mask:
    /// the hidden group is never hit, the shown one is; a restyle (glow,
    /// trails on) keeps the mask, the positions and the hit.
    #[test]
    fn a_group_mask_hides_from_the_hit_test_and_survives_a_restyle() {
        let mut layers = AltitudeLayers::default();
        let p = |lon: f64| GeoPoint { lon, lat: 10.0, alt_m: 550e3 };
        // Paint 1 is "Starlink", paint 0 the rest.
        let style = AltitudeStyle { paint: vec![0, 1, 0, 1], ..AltitudeStyle::default() };
        layers.set("sats", AltitudePositions::Static(vec![p(-20.0), p(-10.0), p(10.0), p(20.0)]), style.clone());
        layers.drawn = Some(drawn_frame(0.0));
        let at: Vec<(f32, f32)> = (0..4).map(|i| layers.screen("sats", i).expect("drawn")).collect();
        let only_starlink = [false, true, false, true];
        assert!(layers.set_visibility("sats", &only_starlink));
        for (i, &(x, y)) in at.iter().enumerate() {
            let want = only_starlink[i].then(|| ("sats".to_string(), i));
            assert_eq!(layers.hit(x, y), want, "object {i}");
        }
        let restyled = AltitudeStyle {
            look: AltitudeLook::Glow,
            trail: Some(crate::render::altitude_pipeline::TrailStyle::default()),
            ..style
        };
        assert!(layers.set_style("sats", restyled));
        assert!(!layers.set_style("nope", AltitudeStyle::default()));
        for (i, &(x, y)) in at.iter().enumerate() {
            assert_eq!(layers.screen("sats", i), only_starlink[i].then_some((x, y)), "object {i} kept its place and mask");
            assert_eq!(layers.hit(x, y).map(|h| h.1), only_starlink[i].then_some(i));
        }
        let hide_starlink = [true, false, true, false];
        assert!(layers.set_visibility("sats", &hide_starlink));
        assert_eq!(layers.hit(at[1].0, at[1].1), None);
        assert_eq!(layers.hit(at[0].0, at[0].1), Some(("sats".to_string(), 0)));
    }

    /// Task 6 review, Minor 1: a `set` or `remove` between a paint and a
    /// pointer event forgets the drawn frame, so the query answers None
    /// rather than an object of the new data through the old camera.
    #[test]
    fn set_and_remove_forget_the_drawn_frame() {
        let mut layers = AltitudeLayers::default();
        let p = GeoPoint { lon: 0.0, lat: 10.0, alt_m: 550e3 };
        layers.set("a", AltitudePositions::Static(vec![p]), AltitudeStyle::default());
        layers.drawn = Some(drawn_frame(0.0));
        let (x, y) = layers.screen("a", 0).expect("drawn");
        layers.set("b", AltitudePositions::Static(vec![p]), AltitudeStyle::default());
        assert_eq!(layers.hit(x, y), None, "set cleared the drawn frame");
        layers.drawn = Some(drawn_frame(0.0));
        assert!(layers.hit(x, y).is_some());
        assert!(layers.remove("b"));
        assert_eq!(layers.hit(x, y), None, "remove cleared the drawn frame");
        assert!(!layers.remove("nope"));
    }

    #[test]
    fn line_layers_count_toward_drawing_and_remove_with_their_id() {
        let mut layers = AltitudeLayers::default();
        assert!(layers.is_empty());
        let r = GLOBE_RADIUS_M * 1.1;
        let ring = AltitudeLine {
            points: (0..8).map(|k| { let a = f64::from(k) * std::f64::consts::TAU / 8.0; DVec3::new(a.cos() * r, 0.0, a.sin() * r) }).collect(),
            color: crate::style::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 },
            closed: true,
        };
        layers.set_lines("iss", RefFrame::Inertial, 1.5, std::slice::from_ref(&ring));
        assert!(!layers.is_empty(), "a line layer alone is something to draw");
        assert!(layers.has_inertial_lines());
        assert_eq!(layers.lines[0].segments.len(), 8);
        layers.set_lines("iss", RefFrame::EarthFixed, 2.0, &[ring]);
        assert_eq!(layers.lines.len(), 1, "replaced in place");
        assert!(!layers.has_inertial_lines());
        layers.set("iss", points(1), AltitudeStyle::default());
        assert!(layers.remove("iss"));
        assert!(layers.is_empty() && layers.lines.is_empty(), "remove drops the points and the lines");
    }

    /// Task 7 review Minor 3: an inertial ring alone, under a running
    /// clock, does not ask for 60 fps. It asks for a frame once it has
    /// turned 0.5 px since the last paint, which at rate 1 and z0 is about
    /// a minute away; a fast clock or a close camera brings it back to
    /// every frame.
    #[test]
    fn an_inertial_ring_alone_asks_for_a_frame_only_once_it_moved_half_a_pixel() {
        let mut layers = AltitudeLayers::default();
        let r = GLOBE_RADIUS_M * 1.066; // ISS-like
        let ring = AltitudeLine {
            points: (0..256)
                .map(|k| {
                    let a = f64::from(k) * std::f64::consts::TAU / 256.0;
                    DVec3::new(a.cos() * r, 0.5 * a.sin() * r, 0.85 * a.sin() * r)
                })
                .collect(),
            color: crate::style::Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 },
            closed: true,
        };
        layers.set_lines("iss", RefFrame::Inertial, 1.5, &[ring]);
        // Nothing keyframed: the dot term asks for nothing at any rate.
        assert!(!wants_frames(1.0, layers.has_keyframed(), layers.waiting));
        let f0 = drawn_frame(0.0);
        let k0 = inertial_px_per_rad(&layers.lines[0].segments, &f0);
        // |p| ~1.07 over a depth of ~8.5-10.5 at a focal of ~887 px: ~100 px a radian.
        assert!((80.0..130.0).contains(&k0), "{k0} px/rad at z0");
        let wait = inertial_redraw_after_s(1.0, k0).unwrap();
        assert!(wait > 30.0, "rate 1 at z0: the ring moves 0.5 px only after {wait:.1} s, so no 60 fps");
        // The wait is exactly the 0.5 px: displacement = omega * rate * wait * k.
        assert!((EARTH_ROTATION_RAD_S * wait * k0 - INERTIAL_REDRAW_PX).abs() < 1e-12);
        assert!(inertial_redraw_after_s(10_000.0, k0).unwrap() < 1.0 / 60.0, "a fast clock is every frame again");
        assert!(inertial_redraw_after_s(-10_000.0, k0).unwrap() < 1.0 / 60.0, "so is a fast reverse one");
        assert_eq!(inertial_redraw_after_s(0.0, k0), None, "paused: never");
        assert_eq!(inertial_redraw_after_s(f64::NAN, k0), None);
        assert_eq!(inertial_redraw_after_s(1.0, 0.0), None, "not drawn: never");
        // Closer in, the same ring moves faster on screen.
        let k6 = inertial_px_per_rad(&layers.lines[0].segments, &drawn_frame(6.0));
        assert!(k6 > 10.0 * k0, "{k6} px/rad at z6 vs {k0} at z0");
        // Review F7: it is an upper bound. Turn the ring by a small angle
        // about the polar axis and measure the real on-screen motion of
        // every vertex in front of the near plane.
        for (zoom, k) in [(0.0, k0), (6.0, k6)] {
            let f = drawn_frame(zoom);
            let delta = 1e-5;
            let turn = glam::DMat3::from_rotation_y(delta);
            let px = |p: DVec3| {
                let c = f.view_proj * p.extend(1.0);
                glam::DVec2::new(c.x / c.w * f64::from(f.viewport.0) / 2.0, c.y / c.w * f64::from(f.viewport.1) / 2.0)
            };
            let mut worst: f64 = 0.0;
            for s in &layers.lines[0].segments {
                let p = glam::Vec3::from(s.a).as_dvec3();
                if f.view.transform_point3(p).z < -f.near {
                    worst = worst.max((px(turn * p) - px(p)).length() / delta);
                }
            }
            assert!(worst > 0.0 && worst <= k * 1.000_1, "z{zoom}: a vertex moves {worst:.2} px/rad, over the bound {k:.2}");
            // Close in, the bound is set by vertices off screen near the
            // near plane, so it is loose (z6: ~3x) -- extra repaints, never
            // a missed one. Far out it is tight.
            if zoom == 0.0 {
                assert!(worst >= 0.5 * k, "z0: the bound {k:.2} is loose against {worst:.2}");
            }
        }
        // `prepare` is where the paint records it; a flat frame forgets it.
        layers.inertial_px_per_rad = k0;
        layers.not_drawn();
        assert_eq!(layers.inertial_px_per_rad, 0.0);
    }

    struct Panics;
    impl KeyframeSource for Panics {
        fn len(&self) -> usize {
            1
        }
        fn states_at(&self, _t: f64, _r: std::ops::Range<usize>, _out: &mut [crate::render::altitude::KeyState]) {
            panic!("a source that panics (expected in this test)");
        }
    }

    #[test]
    fn a_dead_worker_freezes_the_layer_and_stops_asking_for_frames() {
        let mut layers = AltitudeLayers::default();
        let source: Arc<dyn KeyframeSource> = Arc::new(Panics);
        layers.set("sats", AltitudePositions::Keyframed { frame: RefFrame::Inertial, source }, AltitudeStyle::default());
        assert!(layers.has_keyframed());
        let started = crate::timing::Instant::now();
        for rate in [0.0, 1.0] {
            while layers.has_keyframed() {
                layers.advance(2_000_000.0, rate);
                assert!(started.elapsed() < std::time::Duration::from_secs(10), "a dead worker still counts as keyframed");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(!wants_frames(rate, layers.has_keyframed(), layers.waiting), "rate {rate}: no frames for a frozen layer");
        }
        let l = layers.layer("sats");
        assert!(l.keyed.as_ref().is_some_and(|k| k.worker.is_none() && k.hold), "kept, frozen");
        assert!(l.instances.is_empty(), "it never drew, so it draws nothing");
    }
}
