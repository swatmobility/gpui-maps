//! `MapView`'s half of the point path: building a point source's index and
//! its views off the UI thread, and the public cluster API.
//!
//! See `docs/superpowers/specs/2026-09-26-big-point-sources-design.md` and
//! `overlay::points`. The store holds the data, the index and the installed
//! view; this decides WHEN to build them, from the camera, and hands the
//! installed view to the ordinary whole-source bake.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::MapView;
use crate::overlay::points::{self, ViewKey};
use crate::overlay::{ClusterItem, ClusterLeaf, ClusterOptions, FeatureError, FeatureId, SourceId};

/// How far a view reaches past the viewport, as a fraction of the
/// viewport's size on EACH side: 0.5 makes a view twice the viewport's
/// width and height, so a zoom-out of up to one level stays covered while
/// the next view builds. A clustered view is a few thousand features at
/// most, so it reaches twice as far (3x the viewport); the level
/// of detail's views are the big ones and keep the smaller margin.
const VIEW_GROW: f64 = 0.5;
const CLUSTER_VIEW_GROW: f64 = 0.5;

/// A view is rebuilt EARLY, before the viewport leaves it: as soon as the
/// viewport grown by this fraction on each side is no longer inside it. A
/// zoom-out then asks for the next view while the current one still covers
/// the screen, instead of when a hole has already opened.
const REBUILD_SLACK: f64 = 0.25;

/// A view's mesh, tessellated with it: origin, bracket, and the bake's
/// result.
type BakedView = (
    crate::overlay::geom::OverlayOrigin,
    crate::overlay::plan::BakeBracket,
    Result<
        (crate::render::TileMesh, Vec<crate::render::mesh::OpSpan>, crate::render::mesh::OverlayExtras),
        crate::render::TileMesh,
    >,
);

/// Levels below the camera's integer zoom a STAND-IN view is built at.
/// Three: at that zoom the stand-in's box, eight viewports wide, holds
/// about what one viewport of the camera's own view holds.
const STANDIN_LEVELS: u8 = 3;

/// How far the stand-in reaches, per side, as a fraction of the viewport:
/// 3.5 is eight viewports wide -- three zoom levels out.
const STANDIN_GROW: f64 = 3.5;

/// The stand-in is rebuilt once the viewport grown this much (4x: two
/// levels out) leaves it, so it is ahead of a zoom-out by two levels.
const STANDIN_SLACK: f64 = 1.5;

/// The FLOOR: the whole world at this zoom, built once per push. What a
/// zoom-out faster than both views can follow still shows, the way a
/// basemap keeps a low-zoom parent tile under a missing one.
const FLOOR_ZOOM: u8 = 3;

/// One drawn layer of a point source: the key it was built for, the data
/// revision, and its circles (`None`: a view with no dots, or a view that
/// fell back to tessellation).
#[derive(Debug, Clone)]
pub(crate) struct PointTier {
    pub(crate) key: ViewKey,
    pub(crate) revision: u64,
    pub(crate) circles: Option<Arc<points::PointCircles>>,
}

/// A point source's drawn layers, coarse to fine: the world floor, the
/// wide stand-in three levels up, and the view at the camera's zoom. Each
/// stays drawn until its replacement lands; a coarser one draws only its
/// instances outside every finer one's box (`CircleParams`), as a
/// basemap parent tile draws only where its children are missing.
#[derive(Debug, Default, Clone)]
pub(crate) struct PointTiers {
    pub(crate) floor: Option<PointTier>,
    pub(crate) standin: Option<PointTier>,
    pub(crate) primary: Option<PointTier>,
}

/// Which layer a tier build is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TierKind {
    Standin,
    Floor,
}

/// What the point path is doing and has done, per `MapView`.
#[derive(Debug, Default)]
pub(crate) struct PointSources {
    /// Revision whose index is being built, per source.
    indexing: HashMap<SourceId, u64>,
    /// The view being built, per source.
    viewing: HashMap<SourceId, (u64, ViewKey)>,
    /// The stand-in and floor being built, per source.
    standin_building: HashMap<SourceId, ViewKey>,
    floor_building: HashMap<SourceId, u64>,
    pub(crate) tiers: HashMap<SourceId, PointTiers>,
    /// Moves on every paint change of a source: a stand-in or floor built
    /// before it carries the old paint and is discarded when it lands.
    paint_epoch: HashMap<SourceId, u64>,
    stats: HashMap<SourceId, PointSourceStats>,
    /// Each source's filter window (`overlay::filter`), absolute.
    filter_ranges: HashMap<SourceId, [f64; 2]>,
    /// Window writes, per source: what a playback costs (uniform writes
    /// only), for tests and `--measure`.
    filter_range_writes: HashMap<SourceId, u64>,
    /// A packed filter's per-field windows (`set_source_filter_ranges`).
    filter_field_ranges: HashMap<SourceId, Vec<Option<[f64; 2]>>>,
}

/// Whether `key`'s box contains the Mercator box `min..max` (clamped to
/// the world), whatever the zooms.
fn key_contains(key: &ViewKey, min: [f64; 2], max: [f64; 2]) -> bool {
    let (kmin, kmax) = key.mercator_box();
    kmin[0] <= min[0].max(0.0) && kmin[1] <= min[1].max(0.0) && kmax[0] >= max[0].min(1.0) && kmax[1] >= max[1].min(1.0)
}

/// `key`'s box in the lyon units of a draw at `origin`, as a
/// `CircleParams` exclusion box.
fn exclusion_box(key: &ViewKey, origin: &crate::overlay::geom::OverlayOrigin) -> [f32; 4] {
    let (min, max) = key.mercator_box();
    let world = f64::from(crate::render::WORLD_SIZE);
    let scale = crate::overlay::geom::OVERLAY_UNITS_PER_WORLD * f64::from(crate::render::mesh::OVERLAY_COORD_SCALE);
    let x = |m: f64| ((m * world - origin.world_x) * scale) as f32;
    let y = |m: f64| ((m * world - origin.world_y) * scale) as f32;
    [x(min[0]), y(min[1]), x(max[0]), y(max[1])]
}

/// What a point source cost and is drawing: for `--measure`, the probe and
/// a host's own perf endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PointSourceStats {
    /// Points pushed.
    pub points: usize,
    /// Resident bytes of the columns and the index.
    pub data_bytes: usize,
    pub index_bytes: usize,
    /// Wall time of the index build, off the UI thread, and its phases.
    pub index_build_ms: f64,
    pub index_phases: crate::overlay::cluster::BuildPhases,
    /// Views installed, and the last one's build time (off the UI thread)
    /// and size.
    pub views: u64,
    pub last_view_build_ms: f64,
    pub max_view_build_ms: f64,
    pub view_features: usize,
    pub view_clusters: usize,
    /// Frames part of whose viewport lay outside the installed view's box
    /// (after the first view landed): the point path's holes.
    pub hole_frames: u64,
    /// Frames whose viewport the installed view covered, but at another
    /// zoom's level, while the next view built: drawn, at the old level.
    pub stale_frames: u64,
    /// Frames the camera's view did not cover but the stand-in (three
    /// levels coarser, eight viewports wide) did: drawn, coarser.
    pub standin_frames: u64,
    /// Frames only the world floor (zoom 3) covered.
    pub floor_frames: u64,
    /// Stand-in and floor views built.
    pub standins: u64,
    /// Bytes the last (and the largest) view uploaded: instances and paint
    /// table, or the tessellated mesh when a view fell back to it.
    pub last_view_upload_bytes: usize,
    pub max_view_upload_bytes: usize,
}

impl MapView {
    /// The zoom a point source's view is built at: the camera's integer
    /// zoom, as a tile of a `geojson` source in MapLibre.
    fn point_view_zoom(&self) -> u8 {
        self.camera_control.camera().zoom.floor().clamp(0.0, 30.0) as u8
    }

    /// The viewport's Mercator box, grown by `grow` of its size on every
    /// side. Unprojects the corners; a corner above the horizon falls back
    /// to four viewports around the centre.
    fn point_viewport_box(&self, grow: f64) -> ([f64; 2], [f64; 2]) {
        crate::overlay::host::point_viewport_box(self.camera_control.camera(), grow)
    }

    fn point_view_key(&self, grow: f64) -> ViewKey {
        let (min, max) = self.point_viewport_box(grow);
        ViewKey::for_box(self.point_view_zoom(), min, max, 0)
    }

    /// Called from `paint` once the camera is final: counts a hole frame
    /// for a point source whose installed view does not cover the viewport,
    /// and schedules [`Self::drive_point_sources`] when any source needs an
    /// index or a view. One key comparison per point source otherwise.
    pub(super) fn point_sources_frame(&mut self, cx: &mut gpui::App) {
        let names = self.overlay_store.point_source_names();
        if names.is_empty() {
            return;
        }
        let want = self.point_view_key(0.0);
        let early = self.point_view_key(REBUILD_SLACK);
        let (view_min, view_max) = self.point_viewport_box(0.0);
        let mut needs_work = false;
        for name in &names {
            if self.overlay_store.point_index_job(name).is_some() {
                needs_work |= !self.point_sources.indexing.contains_key(name);
                continue;
            }
            let drawn = self.overlay_store.point_view_key(name);
            let covered = drawn.is_some_and(|k| k.covers(&want));
            let tiers = self.point_sources.tiers.get(name);
            let standin_covers = tiers
                .and_then(|t| t.standin.as_ref())
                .is_some_and(|t| key_contains(&t.key, view_min, view_max));
            let floor_covers = tiers
                .and_then(|t| t.floor.as_ref())
                .is_some_and(|t| key_contains(&t.key, view_min, view_max));
            if let Some(key) = drawn {
                // A hole: part of the viewport outside the installed view's
                // box (clamped to the world) AND outside both stand-ins'.
                // Stale: covered, but at another zoom's level while the next
                // view builds.
                let inside = key_contains(&key, view_min, view_max);
                let stats = self.point_sources.stats.entry(Arc::clone(name)).or_default();
                if !inside && standin_covers {
                    stats.standin_frames += 1;
                } else if !inside && floor_covers {
                    stats.floor_frames += 1;
                } else if !inside {
                    stats.hole_frames += 1;
                    if std::env::var_os("GPUI_MAP_POINT_HOLES").is_some() {
                        eprintln!(
                            "point hole: zoom {:.2} view {key:?} viewport {view_min:?}..{view_max:?} pending {}",
                            self.camera_control.camera().zoom,
                            self.point_sources.viewing.contains_key(name)
                        );
                    }
                } else if !covered {
                    stats.stale_frames += 1;
                }
            }
            let pending = self.point_sources.viewing.contains_key(name);
            let covered_early = drawn.is_some_and(|k| k.covers(&early));
            needs_work |= !covered_early && !pending;
            needs_work |= self.point_tier_wanted(name, TierKind::Standin).is_some()
                || self.point_tier_wanted(name, TierKind::Floor).is_some();
        }
        if !needs_work {
            return;
        }
        let Some(entity) = self.self_handle.clone() else { return };
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.drive_point_sources(cx));
        });
    }

    /// Starts whatever the point sources need: an index build for a source
    /// whose push has none, else a view build when the installed view (and
    /// any in flight) does not cover the viewport. At most one of each per
    /// source in flight; a landing re-drives, so a camera that moved on
    /// while a view was building gets the next one then.
    pub(crate) fn drive_point_sources(&mut self, cx: &mut gpui::Context<Self>) {
        let names = self.overlay_store.point_source_names();
        self.point_sources.tiers.retain(|name, _| names.contains(name));
        for name in names {
            if let Some(data) = self.overlay_store.point_index_job(&name) {
                if self.point_sources.indexing.get(&name) != Some(&data.revision) {
                    self.point_sources.indexing.insert(Arc::clone(&name), data.revision);
                    self.spawn_point_index(name, data, cx);
                }
                continue;
            }
            for kind in [TierKind::Floor, TierKind::Standin] {
                if let Some(key) = self.point_tier_wanted(&name, kind) {
                    let Some(job) = self.overlay_store.point_view_job(&name) else { continue };
                    match kind {
                        TierKind::Standin => {
                            self.point_sources.standin_building.insert(Arc::clone(&name), key);
                        }
                        TierKind::Floor => {
                            self.point_sources.floor_building.insert(Arc::clone(&name), job.1.revision);
                        }
                    }
                    self.spawn_point_tier(job, key, kind, cx);
                }
            }
            let want = self.point_view_key(REBUILD_SLACK);
            if self.overlay_store.point_view_key(&name).is_some_and(|k| k.covers(&want)) {
                continue;
            }
            if self.point_sources.viewing.contains_key(&name) {
                continue;
            }
            let Some(job) = self.overlay_store.point_view_job(&name) else { continue };
            // Measured on the 2 M sweep: a 1.5 margin while zooming out
            // halves the zoom-out holes but uploads meshes big enough to cost
            // 29 ms frames; 0.5 keeps every frame under 16.7 ms.
            let grow = if job.1.options.cluster { CLUSTER_VIEW_GROW } else { VIEW_GROW };
            let key = self.point_view_key(grow);
            self.point_sources.viewing.insert(Arc::clone(&name), (job.1.revision, key));
            self.spawn_point_view(job, key, cx);
        }
    }

    /// Drops a point source's stand-in and floor (a paint change: they were
    /// resolved with the old paint), and discards any still building. The
    /// view at the camera's zoom stays drawn until its replacement lands.
    pub(super) fn forget_point_standins(&mut self, name: &str) {
        let Some(key) = self.overlay_store.point_source_names().into_iter().find(|k| &**k == name) else { return };
        *self.point_sources.paint_epoch.entry(Arc::clone(&key)).or_default() += 1;
        if let Some(tiers) = self.point_sources.tiers.get_mut(&key) {
            tiers.standin = None;
            tiers.floor = None;
        }
        self.point_sources.standin_building.remove(&key);
        self.point_sources.floor_building.remove(&key);
    }

    /// The key a stand-in or floor build should be started for, or `None`
    /// when the current one serves (or one is already building, or the
    /// source has no index yet).
    fn point_tier_wanted(&self, name: &SourceId, kind: TierKind) -> Option<ViewKey> {
        let data = self.overlay_store.source_point_data(name)?;
        if !self.overlay_store.source_point_index_ready(name) {
            return None;
        }
        let revision = data.revision;
        let tiers = self.point_sources.tiers.get(name);
        match kind {
            TierKind::Floor => {
                if self.point_sources.floor_building.get(name) == Some(&revision) {
                    return None;
                }
                let current = tiers.and_then(|t| t.floor.as_ref());
                (current.is_none_or(|t| t.revision != revision))
                    .then(|| ViewKey::for_box(FLOOR_ZOOM, [0.0, 0.0], [1.0, 1.0], 0))
            }
            TierKind::Standin => {
                if self.point_sources.standin_building.contains_key(name) {
                    return None;
                }
                let z = self.point_view_zoom().saturating_sub(STANDIN_LEVELS);
                // At or below the floor's zoom the floor IS the stand-in.
                if z <= FLOOR_ZOOM {
                    return None;
                }
                let (emin, emax) = self.point_viewport_box(STANDIN_SLACK);
                let stale = match tiers.and_then(|t| t.standin.as_ref()) {
                    None => true,
                    Some(t) => {
                        t.revision != revision
                            || !key_contains(&t.key, emin, emax)
                            || t.key.z > z
                            || t.key.z + STANDIN_LEVELS < z
                    }
                };
                stale.then(|| {
                    let (min, max) = self.point_viewport_box(STANDIN_GROW);
                    ViewKey::for_box(z, min, max, 0)
                })
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn spawn_point_tier(
        &mut self,
        job: (
            SourceId,
            Arc<points::PointData>,
            Arc<crate::overlay::cluster::ClusterIndex>,
            Arc<crate::overlay::source_paint::SourcePaint>,
        ),
        key: ViewKey,
        kind: TierKind,
        cx: &mut gpui::Context<Self>,
    ) {
        let (name, data, index, paint) = job;
        let for_build = Arc::clone(&name);
        let revision = data.revision;
        let epoch = self.point_sources.paint_epoch.get(&name).copied().unwrap_or(0);
        let build = move || points::build_view(&for_build, &data, &index, &paint, key).circles;
        let Some(runtime) = self.runtime.clone() else {
            let circles = build();
            self.land_point_tier(&name, kind, key, (revision, epoch), circles, cx);
            return;
        };
        cx.spawn(async move |entity, cx| {
            let Ok(circles) = runtime.spawn_blocking(build).await else { return };
            let _ = entity.update(cx, |this, cx| this.land_point_tier(&name, kind, key, (revision, epoch), circles, cx));
        })
        .detach();
    }

    fn land_point_tier(
        &mut self,
        name: &SourceId,
        kind: TierKind,
        key: ViewKey,
        (revision, epoch): (u64, u64),
        circles: Option<Arc<points::PointCircles>>,
        cx: &mut gpui::Context<Self>,
    ) {
        let current_epoch = self.point_sources.paint_epoch.get(name).copied().unwrap_or(0);
        if epoch != current_epoch {
            // Built with a paint that has since changed; its slot was
            // already released by `forget_point_standins`.
            self.drive_point_sources(cx);
            return;
        }
        match kind {
            TierKind::Standin => {
                self.point_sources.standin_building.remove(name);
            }
            TierKind::Floor => {
                self.point_sources.floor_building.remove(name);
            }
        }
        let live = self.overlay_store.source_point_data(name).is_some_and(|d| d.revision == revision);
        if live {
            let tier = Some(PointTier { key, revision, circles });
            let tiers = self.point_sources.tiers.entry(Arc::clone(name)).or_default();
            match kind {
                TierKind::Standin => tiers.standin = tier,
                TierKind::Floor => tiers.floor = tier,
            }
            self.point_sources.stats.entry(Arc::clone(name)).or_default().standins += 1;
        }
        self.drive_point_sources(cx);
        cx.notify();
    }

    /// A point source's circle layers to draw this frame, coarse to fine,
    /// each with the exclusion boxes of the finer layers over it. Nothing
    /// for a source that is no longer a point source.
    pub(super) fn point_circle_draws(
        &self,
        name: &str,
    ) -> Vec<(Arc<points::PointCircles>, crate::render::circles::CircleParams)> {
        let Some(tiers) = self.point_sources.tiers.get(name) else { return Vec::new() };
        if self.overlay_store.source_point_data(name).is_none() {
            return Vec::new();
        }
        let layers = [&tiers.floor, &tiers.standin, &tiers.primary];
        // While a window is set, a layer built before the filter column was
        // declared (its instances carry no values) is not drawn: drawing it
        // unfiltered would flash every point until the rebuild lands.
        let data = self.overlay_store.source_point_data(name);
        let data_filter = data.as_ref().and_then(|d| d.filter());
        let packed = data_filter.and_then(|f| f.packed.as_ref());
        let field_ranges = self.point_sources.filter_field_ranges.get(name);
        let filtering = data_filter.is_some()
            && if packed.is_some() { field_ranges.is_some() } else { self.point_sources.filter_ranges.contains_key(name) };
        let mut out = Vec::new();
        for (at, layer) in layers.iter().enumerate() {
            let Some(circles) = layer.as_ref().and_then(|t| t.circles.as_ref()) else { continue };
            if filtering && circles.filter_base.is_none() {
                continue;
            }
            let mut params = crate::render::circles::CircleParams::NONE;
            if let (Some(layout), Some(ranges), Some(_)) = (packed, field_ranges, circles.filter_base) {
                params = params.with_packed_filter(layout.uniform(ranges));
            } else if let (None, Some(base), Some(range)) = (packed, circles.filter_base, self.point_sources.filter_ranges.get(name)) {
                params = params.with_filter(crate::overlay::filter::relative_window(base, *range));
            }
            let finer: Vec<&PointTier> = layers[at + 1..].iter().filter_map(|t| t.as_ref()).collect();
            if let Some(t) = finer.first() {
                params.exclude_a = exclusion_box(&t.key, &circles.origin);
            }
            if let Some(t) = finer.get(1) {
                params.exclude_b = exclusion_box(&t.key, &circles.origin);
            }
            out.push((Arc::clone(circles), params));
        }
        out
    }

    fn spawn_point_index(&mut self, name: SourceId, data: Arc<points::PointData>, cx: &mut gpui::Context<Self>) {
        let revision = data.revision;
        let build = move || {
            let started = Instant::now();
            let (index, phases) = points::build_index_timed(&data);
            (Arc::new(index), started.elapsed().as_secs_f64() * 1e3, phases)
        };
        let Some(runtime) = self.runtime.clone() else {
            let (index, ms, phases) = build();
            self.land_point_index(&name, revision, index, ms, phases, cx);
            return;
        };
        cx.spawn(async move |entity, cx| {
            let Ok((index, ms, phases)) = runtime.spawn_blocking(build).await else { return };
            let _ = entity.update(cx, |view, cx| view.land_point_index(&name, revision, index, ms, phases, cx));
        })
        .detach();
    }

    fn land_point_index(
        &mut self,
        name: &SourceId,
        revision: u64,
        index: Arc<crate::overlay::cluster::ClusterIndex>,
        ms: f64,
        phases: crate::overlay::cluster::BuildPhases,
        cx: &mut gpui::Context<Self>,
    ) {
        self.point_sources.indexing.remove(name);
        let bytes = index.bytes();
        if self.overlay_store.adopt_point_index(name, revision, index) {
            let data_bytes = self.overlay_store.source_point_data(name).map_or(0, |d| d.bytes());
            let points = self.overlay_store.source_point_data(name).map_or(0, |d| d.len());
            let stats = self.point_sources.stats.entry(Arc::clone(name)).or_default();
            stats.index_build_ms = ms;
            stats.index_phases = phases;
            stats.index_bytes = bytes;
            stats.data_bytes = data_bytes;
            stats.points = points;
        }
        self.drive_point_sources(cx);
        cx.notify();
    }

    #[allow(clippy::type_complexity)]
    fn spawn_point_view(
        &mut self,
        job: (
            SourceId,
            Arc<points::PointData>,
            Arc<crate::overlay::cluster::ClusterIndex>,
            Arc<crate::overlay::source_paint::SourcePaint>,
        ),
        key: ViewKey,
        cx: &mut gpui::Context<Self>,
    ) {
        let (name, data, index, paint) = job;
        let for_build = Arc::clone(&name);
        // The view AND its tessellation, in one blocking task: the landing
        // installs the mesh directly, rather than going back through the
        // bake queue for a second hop (a frame or two of latency that a fast
        // zoom-out shows as a hole).
        let build = move || {
            let started = Instant::now();
            let view = points::build_view(&for_build, &data, &index, &paint, key);
            let ms = started.elapsed().as_secs_f64() * 1e3;
            let baked = view.plan.as_ref().map(|plan| {
                (plan.origin, plan.bracket, super::tessellate_overlay(plan, &|| false, crate::render::TileMesh::default()))
            });
            (view, ms, baked)
        };
        let Some(runtime) = self.runtime.clone() else {
            let (view, ms, baked) = build();
            self.land_point_view(&name, view, ms, baked, cx);
            return;
        };
        cx.spawn(async move |entity, cx| {
            let Ok((view, ms, baked)) = runtime.spawn_blocking(build).await else { return };
            let _ = entity.update(cx, |this, cx| this.land_point_view(&name, view, ms, baked, cx));
        })
        .detach();
    }

    fn land_point_view(
        &mut self,
        name: &SourceId,
        view: points::PointView,
        ms: f64,
        baked: Option<BakedView>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.point_sources.viewing.remove(name);
        let (len, clusters) = (view.len(), view.clusters);
        let (key, revision) = (view.key, view.revision);
        let circles = view.circles.clone();
        let upload = match (&circles, &baked) {
            (Some(c), _) => c.set.upload_bytes(),
            (None, Some((_, _, Ok((mesh, _, _))))) => {
                mesh.vertices.len() * std::mem::size_of::<crate::render::Vertex>() + mesh.indices.len() * 4
            }
            _ => 0,
        };
        if self.overlay_store.install_point_view(name, view) {
            let tiers = self.point_sources.tiers.entry(Arc::clone(name)).or_default();
            tiers.primary = Some(PointTier { key, revision, circles });
            // A new push: the coarser layers of the old data go with it.
            if tiers.standin.as_ref().is_some_and(|t| t.revision != revision) {
                tiers.standin = None;
            }
            if tiers.floor.as_ref().is_some_and(|t| t.revision != revision) {
                tiers.floor = None;
            }
            let stats = self.point_sources.stats.entry(Arc::clone(name)).or_default();
            stats.views += 1;
            stats.last_view_build_ms = ms;
            stats.max_view_build_ms = stats.max_view_build_ms.max(ms);
            stats.view_features = len;
            stats.view_clusters = clusters;
            stats.last_view_upload_bytes = upload;
            stats.max_view_upload_bytes = stats.max_view_upload_bytes.max(upload);
            // The mesh built with the view, installed for the generation the
            // install just stamped; the previous view drew until now. An
            // empty view has no plan, and goes through the ordinary path,
            // which drops the old mesh.
            let generation = self.overlay_store.source_generation(name).unwrap_or(0);
            match baked {
                Some((origin, bracket, Ok((mesh, ranges, extras)))) => {
                    let displaced = self.install_overlay_mesh(
                        name,
                        generation,
                        super::DrawnOverlayMesh::new(origin, mesh, ranges, bracket).with_extras(extras),
                    );
                    // Freed off the UI thread, as the bake path does.
                    if let (Some(displaced), Some(runtime)) = (displaced, self.runtime.clone()) {
                        drop(runtime.spawn_blocking(move || drop(displaced)));
                    }
                }
                _ => self.bake_overlay_source(name, cx),
            }
        }
        self.drive_point_sources(cx);
        cx.notify();
    }

    /// Declares (or, with `None`, withdraws) that `name` is a POINT source:
    /// every later push to it -- through `set_source`, `set_source_from`,
    /// `set_source_data_from`, `set_source_columns` or
    /// [`Self::set_source_points`] -- keeps its points as columns, builds a
    /// clustering index off the UI thread, and draws a view of it at the
    /// camera's zoom. MapLibre's `geojson` source `cluster`,
    /// `clusterRadius`, `clusterMaxZoom`, `clusterMinPoints` and
    /// `clusterProperties`; `cluster: false` draws every point, with points
    /// under one pixel of each other drawn once below zoom 17.
    ///
    /// Sticky. A source that already holds points re-indexes them; one
    /// withdrawn goes back to ordinary features. A point source's
    /// `FeatureRef::index` addresses its current VIEW; `id` and the
    /// properties are the point's (or, for a cluster, `cluster_id` and the
    /// cluster's properties).
    pub fn set_source_cluster_options(
        &mut self,
        name: &str,
        options: Option<ClusterOptions>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), FeatureError> {
        let withdrawn = options.is_none() && self.overlay_store.source_point_data(name).is_some();
        self.overlay_store.set_source_cluster_options(name, options, self.display_zoom())?;
        if withdrawn {
            self.after_source_write(name, cx);
        } else {
            self.drive_point_sources(cx);
            cx.notify();
        }
        Ok(())
    }

    /// The lean door for a big point source: positions, optional ids, one
    /// paint for every point, and typed property columns -- no
    /// `FeatureInput` per point. Declares [`ClusterOptions::points_only`]
    /// if nothing was declared.
    pub fn set_source_points(
        &mut self,
        name: &str,
        lon_lat: &[[f64; 2]],
        ids: Option<&[Option<FeatureId>]>,
        paint: &crate::overlay::FeaturePaint,
        columns: &[(&str, crate::overlay::PropertyColumn<'_>)],
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), FeatureError> {
        self.overlay_store.set_source_points(name, lon_lat, ids, paint, columns)?;
        self.after_source_write(name, cx);
        self.drive_point_sources(cx);
        Ok(())
    }

    /// MapLibre's `getClusterExpansionZoom`: the zoom at which cluster
    /// `cluster_id` of `name` breaks apart. `None` for an unknown source,
    /// an index still building, or an id that is no cluster.
    pub fn get_cluster_expansion_zoom(&self, name: &str, cluster_id: u32) -> Option<u8> {
        self.overlay_store.cluster_expansion_zoom(name, cluster_id)
    }

    /// MapLibre's `getClusterChildren`.
    pub fn get_cluster_children(&self, name: &str, cluster_id: u32) -> Option<Vec<ClusterItem>> {
        self.overlay_store.cluster_children(name, cluster_id)
    }

    /// MapLibre's `getClusterLeaves`: up to `limit` of the cluster's points,
    /// after skipping `offset`.
    pub fn get_cluster_leaves(
        &self,
        name: &str,
        cluster_id: u32,
        limit: usize,
        offset: usize,
    ) -> Option<Vec<ClusterLeaf>> {
        self.overlay_store.cluster_leaves(name, cluster_id, limit, offset)
    }

    /// Declares point source `name`'s range filter column -- a number, or
    /// text read as date-times (`overlay::filter::parse_time`) -- or with
    /// `None` clears it. Reads the column once and rebuilds the source's
    /// views from the index it already has, so each circle carries its
    /// row's value; nothing is re-pushed. Returns the column's `[min,
    /// max]` (milliseconds since 1970 for a time column), or `None` when
    /// `name` is not a point source or the column holds no value.
    ///
    /// Then [`Self::set_source_filter_range`] moves the window, which is a
    /// uniform write per draw: a 55k-point playback re-uploads nothing.
    pub fn set_source_filter_field(
        &mut self,
        name: &str,
        field: Option<&str>,
        cx: &mut gpui::Context<Self>,
    ) -> Option<[f64; 2]> {
        let before = self.overlay_store.source_point_data(name).map(|d| d.revision());
        let domain = self.overlay_store.set_source_filter_field(name, field);
        if self.overlay_store.source_point_data(name).map(|d| d.revision()) != before {
            // The coarse layers carry no values; drop them as a paint
            // change does, and build the views again.
            self.forget_point_standins(name);
            self.drive_point_sources(cx);
            cx.notify();
        }
        domain
    }

    /// Shows only the points of `name` whose filter value lies in the
    /// closed window `range` (absolute, in the column's units), or with
    /// `None` all of them. A uniform write on the next frame: no data,
    /// view or instance is touched, and a call with the window unchanged
    /// does not even ask for a frame.
    pub fn set_source_filter_range(&mut self, name: &str, range: Option<[f64; 2]>, cx: &mut gpui::Context<Self>) {
        let Some(key) = self.overlay_store.point_source_names().into_iter().find(|k| &**k == name) else { return };
        let changed = match range {
            Some(r) => self.point_sources.filter_ranges.insert(Arc::clone(&key), r) != Some(r),
            None => self.point_sources.filter_ranges.remove(&key).is_some(),
        };
        if changed {
            *self.point_sources.filter_range_writes.entry(key).or_default() += 1;
            cx.notify();
        }
    }

    /// Declares up to four filter columns of point source `name` at once
    /// (deck.gl's `DataFilterExtension` with `filterSize` > 1): each point
    /// carries every field's level packed into its one filter value
    /// ([`crate::overlay::filter::PackedLayout`]), read once and rebuilt
    /// into the views from the index already built, as
    /// [`Self::set_source_filter_field`] does. One field is exactly that
    /// call; an empty slice clears the filter. Returns each field's
    /// `[min, max]`.
    ///
    /// Then [`Self::set_source_filter_ranges`] moves any field's window: a
    /// uniform write, nothing re-pushed or re-uploaded.
    pub fn set_source_filter_fields(
        &mut self,
        name: &str,
        fields: &[&str],
        cx: &mut gpui::Context<Self>,
    ) -> Option<Vec<[f64; 2]>> {
        let before = self.overlay_store.source_point_data(name).map(|d| d.revision());
        let domains = self.overlay_store.set_source_filter_fields(name, fields);
        if self.overlay_store.source_point_data(name).map(|d| d.revision()) != before {
            self.forget_point_standins(name);
            self.drive_point_sources(cx);
            cx.notify();
        }
        domains
    }

    /// Each field's window for a filter declared with
    /// [`Self::set_source_filter_fields`], in declaration order (`None`: that
    /// field unfiltered, rows without a value still hidden). A point shows
    /// only when it is inside every window. A uniform write on the next
    /// frame; an unchanged set asks for no frame. A source with one field
    /// takes `ranges[0]` as [`Self::set_source_filter_range`].
    pub fn set_source_filter_ranges(&mut self, name: &str, ranges: &[Option<[f64; 2]>], cx: &mut gpui::Context<Self>) {
        let packed = self.overlay_store.source_point_data(name).is_some_and(|d| d.filter().is_some_and(|f| f.packed.is_some()));
        if !packed {
            self.set_source_filter_range(name, ranges.first().copied().flatten(), cx);
            return;
        }
        let Some(key) = self.overlay_store.point_source_names().into_iter().find(|k| &**k == name) else { return };
        if self.point_sources.filter_field_ranges.get(&key).map(Vec::as_slice) == Some(ranges) {
            return;
        }
        self.point_sources.filter_field_ranges.insert(Arc::clone(&key), ranges.to_vec());
        *self.point_sources.filter_range_writes.entry(key).or_default() += 1;
        cx.notify();
    }

    /// The fields point source `name`'s filter is declared on, in
    /// declaration order (one for [`Self::set_source_filter_field`], up to
    /// four for [`Self::set_source_filter_fields`]); `None` when it has no
    /// filter column.
    pub fn source_filter_fields(&self, name: &str) -> Option<Vec<String>> {
        let data = self.overlay_store.source_point_data(name)?;
        let filter = data.filter()?;
        Some(match &filter.packed {
            Some(layout) => layout.fields.iter().map(|f| f.field.clone()).collect(),
            None => vec![filter.field.clone()],
        })
    }

    /// Moves ONE declared field's window, by name, keeping every other
    /// field's: two independent controls (a time slider and a histogram)
    /// over one packed filter each write their own field. A single-field
    /// filter takes it as [`Self::set_source_filter_range`]. Returns
    /// `false`, writing nothing, when `field` is not one `name` filters on.
    pub fn set_source_filter_field_range(&mut self, name: &str, field: &str, range: Option<[f64; 2]>, cx: &mut gpui::Context<Self>) -> bool {
        let Some(fields) = self.source_filter_fields(name) else { return false };
        let Some(k) = fields.iter().position(|f| f == field) else { return false };
        if fields.len() == 1 {
            self.set_source_filter_range(name, range, cx);
            return true;
        }
        let mut ranges = self.source_filter_ranges(name).map(<[_]>::to_vec).unwrap_or_default();
        ranges.resize(fields.len(), None);
        ranges[k] = range;
        self.set_source_filter_ranges(name, &ranges, cx);
        true
    }

    /// Which features of point source `name` its filter window draws, as
    /// the shader decides it (`vs_circle`'s CPU mirror:
    /// `PointFilter::passes_all` with the per-field windows for a packed
    /// filter, `PointFilter::passes` with the window for a single one), by
    /// feature index. `None` when nothing is filtered: every point draws.
    /// What `feature_at` skips hidden points with.
    pub(super) fn point_filter_keeps(&self, name: &str) -> Option<Box<dyn Fn(usize) -> bool>> {
        let data = self.overlay_store.source_point_data(name)?;
        let filter = data.filter()?;
        if filter.packed.is_some() {
            let ranges = self.point_sources.filter_field_ranges.get(name)?.clone();
            Some(Box::new(move |i| data.filter().is_some_and(|f| f.passes_all(i, &ranges))))
        } else {
            let range = *self.point_sources.filter_ranges.get(name)?;
            Some(Box::new(move |i| data.filter().is_some_and(|f| f.passes(i, range))))
        }
    }

    /// The windows [`Self::set_source_filter_ranges`] set.
    pub fn source_filter_ranges(&self, name: &str) -> Option<&[Option<[f64; 2]>]> {
        self.point_sources.filter_field_ranges.get(name).map(Vec::as_slice)
    }

    /// The window [`Self::set_source_filter_range`] set.
    pub fn source_filter_range(&self, name: &str) -> Option<[f64; 2]> {
        self.point_sources.filter_ranges.get(name).copied()
    }

    /// How many times `name`'s window moved.
    pub fn source_filter_range_writes(&self, name: &str) -> u64 {
        self.point_sources.filter_range_writes.get(name).copied().unwrap_or(0)
    }

    /// The source's data stamp: moves on every push, option or filter
    /// column change -- and never on a filter window move.
    pub fn source_generation(&self, name: &str) -> Option<u64> {
        self.overlay_store.source_generation(name)
    }

    /// Whether a point source's index has landed.
    pub fn source_point_index_ready(&self, name: &str) -> bool {
        self.overlay_store.source_point_index_ready(name)
    }

    /// What a point source cost and is drawing. `None` for a source that is
    /// not a point source or has not indexed yet.
    pub fn point_source_stats(&self, name: &str) -> Option<PointSourceStats> {
        self.point_sources.stats.get(name).copied()
    }

    /// Whether a point source's installed view covers the viewport, and no
    /// view or index is building for it: the point path has settled.
    pub fn point_source_settled(&self, name: &str) -> bool {
        let want = self.point_view_key(0.0);
        self.overlay_store.point_view_key(name).is_some_and(|k| k.covers(&want))
            && !self.point_sources.viewing.contains_key(name)
            && !self.point_sources.indexing.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    fn map(cx: &mut gpui::TestAppContext) -> gpui::Entity<MapView> {
        let handle = crate::map_view::test_runtime_handle();
        cx.new(|_| {
            MapView::new(
                -93.6,
                41.9,
                12.3,
                800.0,
                600.0,
                crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                Arc::new(crate::tile_source::EmptyTileSource),
                handle,
            )
        })
    }

    /// Drives the point path until `done` holds, landing the off-thread
    /// builds; bounded at 20 s.
    fn drive_until(
        cx: &mut gpui::TestAppContext,
        map: &gpui::Entity<MapView>,
        done: impl Fn(&MapView) -> bool,
    ) {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        loop {
            map.update(cx, |v, cx| v.drive_point_sources(cx));
            cx.run_until_parked();
            if map.read_with(cx, |v, _| done(v)) {
                return;
            }
            assert!(Instant::now() < deadline, "the point path did not settle in 20 s");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// **A point source draws three layers, and each coarser one hides
    /// its instances under the finer ones.** The world floor, the stand-in
    /// three levels up and the view land off-thread; the view covers the
    /// viewport; the stand-in's exclusion box is the view's, the floor's
    /// are the stand-in's and the view's. A paint change drops the two
    /// coarse layers (they carry the old paint) and they rebuild.
    #[test]
    fn a_point_source_draws_a_floor_a_standin_and_a_view_coarse_under_fine() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        let points: Vec<[f64; 2]> =
            (0..20_000).map(|i| [-94.0 + (i % 200) as f64 * 0.004, 41.5 + (i / 200) as f64 * 0.008]).collect();
        let paint = crate::overlay::FeaturePaint { circle_radius: 3.0, ..Default::default() };
        map.update(&mut app, |v, cx| v.set_source_points("p", &points, None, &paint, &[], cx).unwrap());
        let all_three = |v: &MapView| {
            v.point_sources.tiers.get("p").is_some_and(|t| t.floor.is_some() && t.standin.is_some() && t.primary.is_some())
        };
        drive_until(&mut app, &map, all_three);
        map.read_with(&app, |v, _| {
            let tiers = &v.point_sources.tiers["p"];
            let (floor, standin, primary) =
                (tiers.floor.as_ref().unwrap(), tiers.standin.as_ref().unwrap(), tiers.primary.as_ref().unwrap());
            assert_eq!(floor.key.z, FLOOR_ZOOM);
            assert_eq!(primary.key.z, 12);
            assert_eq!(standin.key.z, 12 - STANDIN_LEVELS);
            let (min, max) = v.point_viewport_box(0.0);
            assert!(key_contains(&primary.key, min, max), "the view covers the viewport");
            assert!(key_contains(&standin.key, min, max) && key_contains(&floor.key, min, max));
            let draws = v.point_circle_draws("p");
            assert_eq!(draws.len(), 3, "floor, stand-in and view each draw");
            let none = crate::render::circles::CircleParams::NONE;
            let (f, s, p) = (&draws[0], &draws[1], &draws[2]);
            assert_eq!(p.1, none, "the view excludes nothing");
            assert_eq!(s.1.exclude_a, exclusion_box(&primary.key, &s.0.origin));
            assert_eq!(s.1.exclude_b, none.exclude_b);
            assert_eq!(f.1.exclude_a, exclusion_box(&standin.key, &f.0.origin));
            assert_eq!(f.1.exclude_b, exclusion_box(&primary.key, &f.0.origin));
            assert!(p.0.set.len() > 1000, "the view holds the points: {}", p.0.set.len());
            assert_eq!(v.point_source_stats("p").unwrap().hole_frames, 0);
        });

        // A paint change: the coarse layers go at once, then come back.
        let paint: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r##"{"circle-color":"#ff0000"}"##).unwrap();
        map.update(&mut app, |v, cx| v.update_source_paint("p", &paint, cx).unwrap());
        map.read_with(&app, |v, _| {
            let tiers = &v.point_sources.tiers["p"];
            assert!(tiers.floor.is_none() && tiers.standin.is_none(), "the old paint's coarse layers are dropped");
            assert!(tiers.primary.is_some(), "the view draws until its replacement lands");
        });
        drive_until(&mut app, &map, all_three);
    }

    /// **A filter window moves on the GPU alone.** A time column read once
    /// (the views rebuild with values, from the index already built); then
    /// 120 playback frames of window moves change neither the data stamp
    /// nor the views installed -- each is one uniform per draw, carrying
    /// the window relative to the column's minimum.
    #[test]
    fn a_filter_window_moves_without_a_repush_or_a_view_rebuild() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        let n = 20_000usize;
        let points: Vec<[f64; 2]> =
            (0..n).map(|i| [-94.0 + (i % 200) as f64 * 0.004, 41.5 + (i / 200) as f64 * 0.008]).collect();
        let times: Vec<String> = (0..n).map(|i| format!("{}-01-01 00:00:00", 1967 + i % 50)).collect();
        let column: Vec<Option<&str>> = times.iter().map(|t| Some(t.as_str())).collect();
        let paint = crate::overlay::FeaturePaint { circle_radius: 3.0, ..Default::default() };
        map.update(&mut app, |v, cx| {
            v.set_source_points("q", &points, None, &paint, &[("when", crate::overlay::PropertyColumn::Str(&column))], cx)
                .unwrap()
        });
        // Declared straight after the push, before the index lands (as the
        // Studio does): the index in flight is for the old revision and is
        // rebuilt for the new one.
        let domain = map.update(&mut app, |v, cx| v.set_source_filter_field("q", Some("when"), cx)).expect("a time column");
        let t1967 = crate::overlay::filter::parse_time("1967-01-01").unwrap();
        assert_eq!(domain, [t1967, crate::overlay::filter::parse_time("2016-01-01").unwrap()]);
        let filtered = |v: &MapView| {
            v.point_sources.tiers.get("q").and_then(|t| t.primary.as_ref()).and_then(|t| t.circles.as_ref()).is_some_and(|c| c.filter_base.is_some())
                && v.point_source_settled("q")
        };
        drive_until(&mut app, &map, filtered);
        let (generation, views) = map.read_with(&app, |v, _| (v.source_generation("q"), v.point_source_stats("q").unwrap().views));
        let mut playback = crate::overlay::filter::Playback::new(domain, [domain[0], domain[0] + 5.0 * 365.25 * 86_400_000.0]);
        playback.play();
        for frame in 0..120 {
            let window = playback.tick(f64::from(frame) / 60.0).expect("playing");
            map.update(&mut app, |v, cx| v.set_source_filter_range("q", Some(window), cx));
        }
        app.run_until_parked();
        map.read_with(&app, |v, _| {
            assert_eq!(v.source_generation("q"), generation, "a window move re-pushed the data");
            assert_eq!(v.point_source_stats("q").unwrap().views, views, "a window move rebuilt a view");
            assert_eq!(v.source_filter_range_writes("q"), 120, "one write per frame");
            let window = v.source_filter_range("q").unwrap();
            let draws = v.point_circle_draws("q");
            assert!(!draws.is_empty());
            for (circles, params) in &draws {
                let base = circles.filter_base.expect("every drawn layer carries values");
                assert_eq!(base, t1967);
                assert_eq!(params.filter[2], 1.0, "the window is on");
                let want = crate::overlay::filter::relative_window(base, window);
                assert_eq!([params.filter[0], params.filter[1]], want);
            }
        });
        // An unchanged window asks for nothing; clearing turns it off.
        map.update(&mut app, |v, cx| {
            let w = v.source_filter_range("q");
            v.set_source_filter_range("q", w, cx);
            assert_eq!(v.source_filter_range_writes("q"), 120);
            v.set_source_filter_range("q", None, cx);
            assert!(v.point_circle_draws("q").iter().all(|(_, p)| p.filter[2] == 0.0));
        });
    }

    /// `vs_circle`'s packed test, line for line, on one instance.
    fn shader_passes(params: &crate::render::circles::CircleParams, value: f32) -> bool {
        let code = value.to_bits();
        (0..4).all(|k| {
            let level = (code >> params.packed_shift[k]) & params.packed_mask[k];
            level >= params.packed_lo[k] && level <= params.packed_hi[k]
        })
    }

    /// **Three filters at once, each a uniform write.** Kepler's LA homes
    /// filter year built, value and bedrooms together: declared once, every
    /// point carries one packed code word, and each window move is a write
    /// -- no re-push, no view rebuilt -- whose GPU test (emulated line for
    /// line) agrees with the CPU's exact `lo <= v <= hi` on every row.
    #[test]
    #[allow(clippy::type_complexity)]
    fn three_filter_windows_move_together_without_a_repush() {
        let mut app = gpui::TestAppContext::single();
        let map = map(&mut app);
        let n = 12_000usize;
        let points: Vec<[f64; 2]> =
            (0..n).map(|i| [-94.0 + (i % 120) as f64 * 0.006, 41.5 + (i / 120) as f64 * 0.008]).collect();
        let year: Vec<Option<f64>> = (0..n).map(|i| Some(2006.0 + (i % 11) as f64)).collect();
        let value: Vec<Option<f64>> = (0..n).map(|i| if i % 97 == 0 { None } else { Some(2.0e6 + ((i * 7919) % n) as f64 * 1234.5) }).collect();
        let beds: Vec<Option<f64>> = (0..n).map(|i| Some(1.0 + (i % 17) as f64)).collect();
        let paint = crate::overlay::FeaturePaint { circle_radius: 3.0, ..Default::default() };
        use crate::overlay::PropertyColumn::Number;
        map.update(&mut app, |v, cx| {
            v.set_source_points("la", &points, None, &paint, &[("year", Number(&year)), ("value", Number(&value)), ("beds", Number(&beds))], cx)
                .unwrap()
        });
        let domains = map.update(&mut app, |v, cx| v.set_source_filter_fields("la", &["year", "value", "beds"], cx)).expect("three columns");
        assert_eq!(domains.len(), 3);
        assert_eq!(domains[0], [2006.0, 2016.0]);
        assert_eq!(domains[2], [1.0, 17.0]);
        let ready = |v: &MapView| {
            v.point_sources.tiers.get("la").and_then(|t| t.primary.as_ref()).and_then(|t| t.circles.as_ref()).is_some_and(|c| c.filter_base.is_some())
                && v.point_source_settled("la")
        };
        drive_until(&mut app, &map, ready);
        let (generation, views) = map.read_with(&app, |v, _| (v.source_generation("la"), v.point_source_stats("la").unwrap().views));
        let data = map.read_with(&app, |v, _| v.overlay_store.source_point_data("la").unwrap());
        let filter = data.filter().unwrap();
        assert!(filter.packed.as_ref().unwrap().fields.iter().all(|f| f.exact), "these columns fit exactly");
        let windows: [[Option<[f64; 2]>; 3]; 4] = [
            [Some([2008.0, 2012.0]), None, None],
            [Some([2006.0, 2016.0]), Some([2.5e6, 9.0e6]), Some([3.0, 5.0])],
            [None, Some([3.3e6, 3.3e6 + 0.1]), None],
            [Some([2020.0, 2030.0]), None, None],
        ];
        for ranges in &windows {
            map.update(&mut app, |v, cx| v.set_source_filter_ranges("la", ranges, cx));
            let want = (0..n).filter(|&i| filter.passes_all(i, ranges)).count();
            let exact = (0..n)
                .filter(|&i| {
                    let vals = [year[i], value[i], beds[i]];
                    vals.iter().all(Option::is_some)
                        && ranges.iter().zip(vals).all(|(r, v)| r.is_none_or(|r| v.is_some_and(|v| v >= r[0] && v <= r[1])))
                })
                .count();
            assert_eq!(want, exact, "the packed CPU test is the exact one for {ranges:?}");
            map.read_with(&app, |v, _| {
                let draws = v.point_circle_draws("la");
                let (circles, params) = draws.last().expect("the primary view");
                assert_eq!(params.filter[2], 2.0, "the packed lanes are on");
                // The view holds the points it covers; each instance's
                // verdict must be its row's (a row's code is its values').
                let passing: std::collections::HashSet<u32> = (0..n).filter(|&i| filter.passes_all(i, ranges)).map(|i| filter.value(i).to_bits()).collect();
                assert!(circles.set.instances.len() > 100);
                for c in &circles.set.instances {
                    assert_eq!(shader_passes(params, c.filter), passing.contains(&c.filter.to_bits()), "the GPU test agrees for {ranges:?}");
                }
            });
        }
        app.run_until_parked();
        map.read_with(&app, |v, _| {
            assert_eq!(v.source_generation("la"), generation, "a window move re-pushed the data");
            assert_eq!(v.point_source_stats("la").unwrap().views, views, "a window move rebuilt a view");
            assert_eq!(v.source_filter_range_writes("la"), 4);
        });
    }

    /// The gallery's layout around a map: a title strip across the top and
    /// a sidebar on the left, so the map's element sits at a non-zero
    /// window offset (the offset two gallery panels already mis-mapped).
    struct GalleryLikeHost {
        map: gpui::Entity<MapView>,
    }

    const TITLE_H: f32 = 34.0;
    const SIDEBAR_W: f32 = 260.0;

    impl gpui::Render for GalleryLikeHost {
        fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div()
                .flex()
                .flex_col()
                .size_full()
                .child(gpui::div().h(gpui::px(TITLE_H)).w_full())
                .child(
                    gpui::div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .child(gpui::div().w(gpui::px(SIDEBAR_W)).h_full())
                        .child(gpui::div().flex_1().h_full().child(self.map.clone())),
                )
        }
    }

    /// A map at big_points' opening camera in [`GalleryLikeHost`], with
    /// `points` pushed as one clustered source `"p"` painted with
    /// big_points' cluster radii, driven until the point view is drawn.
    fn clusters_in_gallery_layout<'a>(
        app: &'a mut gpui::TestAppContext,
        points: &[[f64; 2]],
    ) -> (gpui::Entity<MapView>, &'a mut gpui::VisualTestContext) {
        use gpui::VisualTestContext;
        super::super::HEADLESS_PAINT.with(|h| h.set(true));
        let handle = crate::map_view::test_runtime_handle();
        let (host, cx) = app.add_window_view(|_, cx| {
            let map = cx.new(|_| {
                MapView::new(
                    -96.0,
                    38.5,
                    3.7,
                    800.0,
                    600.0,
                    crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                    Arc::new(crate::tile_source::EmptyTileSource),
                    handle,
                )
            });
            GalleryLikeHost { map }
        });
        let map = host.read_with(cx, |h, _| h.map.clone());
        let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            r##"{"circle-radius": ["step", ["get", "point_count"], 10, 100, 14, 1000, 19, 10000, 25]}"##,
        )
        .unwrap();
        map.update(cx, |v, cx| {
            v.set_source_cluster_options("p", Some(ClusterOptions::default()), cx).unwrap();
            v.declare_source_paint("p", &paint).unwrap();
            let dot = crate::overlay::FeaturePaint { circle_radius: 2.5, ..Default::default() };
            v.set_source_points("p", points, None, &dot, &[], cx).unwrap();
        });
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !map.read_with(cx, |v, _| v.point_source_settled("p") && v.overlay_drawn_source("p").is_some()) {
            assert!(Instant::now() < deadline, "the point view did not land in 60 s");
            draw(cx);
            map.update(cx, |v, cx| v.drive_point_sources(cx));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        draw(cx);
        (map, cx)
    }

    /// 40,000 points in eight tight spirals: eight big clusters at z3.7.
    fn eight_spirals() -> Vec<[f64; 2]> {
        let mut points = Vec::new();
        for c in 0..8 {
            let (lon, lat) = (-120.0 + f64::from(c) * 6.0, 30.0 + f64::from(c % 4) * 3.0);
            for i in 0..5000 {
                let (a, r) = (f64::from(i) * 0.37, f64::from(i) / 5000.0 * 0.8);
                points.push([lon + r * a.cos(), lat + r * a.sin()]);
            }
        }
        points
    }

    /// The first cluster found on an 8 px grid, in the map's OWN pixels.
    fn a_cluster_on_screen(map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext) -> Option<(f32, f32)> {
        let (vw, vh) = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
        map.read_with(cx, |v, _| {
            (0..(vh as i32) / 8)
                .flat_map(|y| (0..(vw as i32) / 8).map(move |x| (x as f32 * 8.0, y as f32 * 8.0)))
                .find(|&(x, y)| {
                    v.feature_at(x, y)
                        .and_then(|f| v.feature_properties(&f))
                        .is_some_and(|p| p.contains_key("cluster_id"))
                })
        })
    }

    /// **A click on a cluster that eases the camera still eases once the
    /// button comes up** -- `examples/panels/big_points.rs`'s "click a
    /// cluster to zoom into it", in the gallery's layout (the map offset by
    /// a title strip and a sidebar), through gpui's own dispatch.
    ///
    /// `FeatureEvent::Click` fires on the PRESS, and the host's ease starts
    /// then. The release that follows sent `CameraIntent::GestureEnded`,
    /// and `CameraController::apply` cancelled any programmatic motion on
    /// EVERY intent -- so the ease died a few milliseconds after it began,
    /// and the user saw the click do nothing. A release moves nothing; only
    /// an intent that moves the camera may stop an ease.
    #[test]
    fn a_click_that_eases_to_a_cluster_survives_the_release_in_the_gallery_layout() {
        let mut app = gpui::TestAppContext::single();
        let (map, cx) = clusters_in_gallery_layout(&mut app, &eight_spirals());
        // What big_points' subscriber does with a cluster: ease towards it.
        let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = clicks.clone();
        cx.update(|_, cx| {
            cx.subscribe(&map, move |map, e: &crate::overlay::FeatureEvent, cx| {
                let crate::overlay::FeatureEvent::Click { lon_lat, .. } = e else { return };
                seen.set(seen.get() + 1);
                map.update(cx, |v, cx| {
                    let zoom = v.camera().zoom + 2.0;
                    v.ease_to(
                        crate::camera_control::CameraTarget { center: Some(*lon_lat), zoom: Some(zoom), ..Default::default() },
                        std::time::Duration::from_millis(500),
                    );
                    cx.notify();
                });
            })
            .detach()
        });
        let cluster = a_cluster_on_screen(&map, cx);
        let (x, y) = cluster.expect("a cluster is drawn");
        let zoom_before = map.read_with(cx, |v, _| v.camera().zoom);
        // The same point in WINDOW pixels: the map sits under the title
        // strip and right of the sidebar.
        let at = gpui::point(gpui::px(x + SIDEBAR_W), gpui::px(y + TITLE_H));
        cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        cx.simulate_click(at, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "the press on a cluster announced exactly one Click");
        assert!(
            map.read_with(cx, |v, _| !v.camera_control.is_settled()),
            "the ease the click started is still running after the release (zoom {zoom_before})"
        );
    }

    /// The census panel's hover, in the gallery's layout: a mouse move over
    /// a block group (a polygon pushed through `set_source_columns`, as the
    /// census pushes every state) reaches the map's hover path and
    /// announces `HoverEnter` for it -- with the map offset by a title
    /// strip and a sidebar, through gpui's own dispatch.
    #[test]
    fn a_move_over_a_polygon_announces_hover_in_the_gallery_layout() {
        use gpui::VisualTestContext;
        super::super::HEADLESS_PAINT.with(|h| h.set(true));
        let mut app = gpui::TestAppContext::single();
        let handle = crate::map_view::test_runtime_handle();
        let (host, cx) = app.add_window_view(|_, cx| {
            let map = cx.new(|_| {
                MapView::new(
                    -98.0,
                    39.0,
                    3.5,
                    800.0,
                    600.0,
                    crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                    Arc::new(crate::tile_source::EmptyTileSource),
                    handle,
                )
            });
            GalleryLikeHost { map }
        });
        let map = host.read_with(cx, |h, _| h.map.clone());
        let entered = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = entered.clone();
        cx.update(|_, cx| {
            cx.subscribe(&map, move |_, e: &crate::overlay::FeatureEvent, _| {
                if let crate::overlay::FeatureEvent::HoverEnter { feature } = e {
                    seen.borrow_mut().push(feature.id.clone());
                }
            })
            .detach()
        });
        let ring: Vec<[f64; 2]> = vec![[-100.0, 37.0], [-96.0, 37.0], [-96.0, 41.0], [-100.0, 41.0], [-100.0, 37.0]];
        let rings: Vec<&[[f64; 2]]> = vec![&ring];
        let polygons: Vec<&[&[[f64; 2]]]> = vec![&rings];
        let income = [Some(50_000.0)];
        map.update(cx, |v, cx| {
            let paint: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(r##"{"fill-color": "#3b528b", "fill-outline-color": "#ffffff"}"##).unwrap();
            v.declare_source_paint("bg-20", &paint).unwrap();
            v.set_source_below_labels("bg-20", true);
            let fills = [crate::overlay::FeatureInput {
                id: Some(FeatureId::Number(123_456.0)),
                geometry: crate::overlay::GeometryInput::Polygon(&polygons),
                paint: crate::overlay::FeaturePaint::default(),
            }];
            v.set_source_columns("bg-20", &fills, &[("income", crate::overlay::PropertyColumn::Number(&income))], cx)
                .unwrap();
        });
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !map.read_with(cx, |v, _| v.overlay_drawn_source("bg-20").is_some()) {
            assert!(Instant::now() < deadline, "the polygon did not land in 20 s");
            draw(cx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        draw(cx);
        let (vw, vh) = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
        assert!(map.read_with(cx, |v, _| v.feature_at(vw / 2.0, vh / 2.0).is_some()), "the engine hits the polygon");
        assert!(map.read_with(cx, |v, _| !v.camera_in_motion()), "the camera is settled, so a move is hit-tested");
        let at = gpui::point(gpui::px(vw / 2.0 + SIDEBAR_W), gpui::px(vh / 2.0 + TITLE_H));
        cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(*entered.borrow(), vec![Some(FeatureId::Number(123_456.0))], "a move over the block group hovers it");
    }

    /// `examples/panels/big_points.rs`'s pointing-hand cursor, engine side:
    /// in the gallery's layout (the map offset by a title strip and a
    /// sidebar), a mouse move onto a cluster announces `HoverEnter` for
    /// that cluster, and a move off it onto empty map announces the
    /// matching `HoverLeave` -- the two events the panel's `HoverCursor`
    /// turns into `CursorStyle::PointingHand` and back to the default.
    #[test]
    fn a_move_over_a_cluster_hovers_it_and_a_move_off_it_leaves_in_the_gallery_layout() {
        let mut app = gpui::TestAppContext::single();
        let (map, cx) = clusters_in_gallery_layout(&mut app, &eight_spirals());
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&map, move |_, e: &crate::overlay::FeatureEvent, _| seen.borrow_mut().push(e.clone())).detach()
        });
        let (x, y) = a_cluster_on_screen(&map, cx).expect("a cluster is drawn");
        let over = gpui::point(gpui::px(x + SIDEBAR_W), gpui::px(y + TITLE_H));
        cx.simulate_mouse_move(over, None, gpui::Modifiers::default());
        cx.run_until_parked();
        let entered = match events.borrow().as_slice() {
            [crate::overlay::FeatureEvent::HoverEnter { feature }] => feature.clone(),
            other => panic!("one HoverEnter for the cluster, got {other:?}"),
        };
        assert_eq!(&*entered.source, "p");
        assert!(
            map.read_with(cx, |v, _| v.feature_properties(&entered).is_some_and(|p| p.contains_key("cluster_id"))),
            "the hovered feature is a cluster"
        );
        // A second move within the same cluster: no event, no re-render.
        cx.simulate_mouse_move(gpui::point(over.x + gpui::px(1.0), over.y), None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(events.borrow().len(), 1, "hover is edge-triggered");
        // Off every cluster: the top-left corner of the map, over Canada's
        // empty north at z3.7 (no spiral is within 25 px of it).
        let (vw, vh) = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
        assert!(map.read_with(cx, |v, _| v.feature_at(2.0, 2.0).is_none()), "the corner is empty map ({vw}x{vh})");
        cx.simulate_mouse_move(gpui::point(gpui::px(2.0 + SIDEBAR_W), gpui::px(2.0 + TITLE_H)), None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            events.borrow().last(),
            Some(&crate::overlay::FeatureEvent::HoverLeave { feature: entered }),
            "leaving the cluster ends the hover"
        );
        assert_eq!(events.borrow().len(), 2);
    }

    /// What one hover hit test costs on big_points' own 2,000,000 points
    /// (an approximation of its generator: 40 Gaussian cities on a Zipf
    /// curve plus a 10% rural spread), at its opening z3.7 and at z10 over
    /// the biggest city. Run by hand:
    /// `cargo test --release --lib -- --ignored --nocapture hover_hit_test_cost`.
    /// The engine hit-tests only on a mouse move (and once when a moving
    /// camera settles), never per frame, so this is per mouse-move cost.
    #[test]
    #[ignore = "a timing, run by hand"]
    fn hover_hit_test_cost_on_two_million_points() {
        let mut s = 1u64;
        let mut next = move || {
            s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        };
        let cities: Vec<(f64, f64, f64)> =
            (0..40).map(|i| (-122.0 + next() * 50.0, 27.0 + next() * 20.0, 0.04 + 0.25 / (1.0 + f64::from(i)).sqrt())).collect();
        let points: Vec<[f64; 2]> = (0..2_000_000)
            .map(|_| {
                if next() < 0.1 {
                    return [-124.0 + next() * 57.0, 25.0 + next() * 24.0];
                }
                let c = ((next() * next()) * 40.0) as usize;
                let (lon, lat, sigma) = cities[c.min(39)];
                let (u, v) = (next().max(1e-12), next());
                let g = (-2.0 * u.ln()).sqrt();
                [lon + g * (std::f64::consts::TAU * v).cos() * sigma, lat + g * (std::f64::consts::TAU * v).sin() * sigma]
            })
            .collect();
        let mut app = gpui::TestAppContext::single();
        let (map, cx) = clusters_in_gallery_layout(&mut app, &points);
        let time = |map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext, label: &str| {
            let (vw, vh) = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
            map.read_with(cx, |v, _| {
                let grid: Vec<(f32, f32)> = (0..30)
                    .flat_map(|y| (0..40).map(move |x| (x as f32 * vw / 40.0, y as f32 * vh / 30.0)))
                    .collect();
                let mut hits = 0;
                let mut worst = 0.0f64;
                let started = Instant::now();
                for &(x, y) in &grid {
                    let one = Instant::now();
                    hits += usize::from(v.feature_at(x, y).is_some());
                    worst = worst.max(one.elapsed().as_secs_f64());
                }
                let mean = started.elapsed().as_secs_f64() / grid.len() as f64;
                println!(
                    "hover hit test, {label}: {} probes, {hits} hits, mean {:.1} us, worst {:.1} us",
                    grid.len(),
                    mean * 1e6,
                    worst * 1e6
                );
            });
        };
        time(&map, cx, "z3.7 opening view");
        let (lon, lat, _) = cities[0];
        map.update(cx, |v, cx| {
            v.jump_to(crate::camera_control::CameraTarget { center: Some((lon, lat)), zoom: Some(10.0), ..Default::default() });
            cx.notify();
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            map.update(cx, |v, cx| v.drive_point_sources(cx));
            cx.run_until_parked();
            if map.read_with(cx, |v, _| v.point_source_settled("p")) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        time(&map, cx, "z10 over the biggest city");
    }

    /// Paints and drives the point path until `done` holds; bounded at 20 s.
    fn paint_until(map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext, done: impl Fn(&MapView) -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        loop {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            map.update(cx, |v, cx| v.drive_point_sources(cx));
            cx.run_until_parked();
            if map.read_with(cx, |v, _| done(v)) {
                return;
            }
            assert!(Instant::now() < deadline, "did not settle in 20 s");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// **A point its filter window hides is not picked** -- with the
    /// shader's own predicate, single-field and packed -- and a hidden
    /// point above the labels over an arc yields to the arc.
    #[test]
    fn a_point_the_filter_hides_is_not_picked() {
        let mut app = gpui::TestAppContext::single();
        let (map, cx) = app.add_window_view(|_, _| {
            MapView::new(
                -93.6,
                41.9,
                12.3,
                800.0,
                600.0,
                crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                Arc::new(crate::tile_source::EmptyTileSource),
                crate::map_view::test_runtime_handle(),
            )
        });
        let (vw, vh) = map.read_with(cx, |v, _| (v.camera().viewport_width_px, v.camera().viewport_height_px));
        let centre = (vw / 2.0, vh / 2.0);
        // Point 0 under the centre (value 5, flag 1); point 1 off it.
        let points = [[-93.6, 41.9], [-93.58, 41.9]];
        let value = [Some(5.0), Some(50.0)];
        let flag = [Some(1.0), Some(0.0)];
        let paint = crate::overlay::FeaturePaint { circle_radius: 6.0, ..Default::default() };
        map.update(cx, |v, cx| {
            v.set_source_points(
                "q",
                &points,
                None,
                &paint,
                &[("value", crate::overlay::PropertyColumn::Number(&value)), ("flag", crate::overlay::PropertyColumn::Number(&flag))],
                cx,
            )
            .unwrap();
            v.set_source_filter_field("q", Some("value"), cx);
        });
        paint_until(&map, cx, |v| v.point_source_settled("q") && v.feature_at(centre.0, centre.1).is_some());
        let picked = |cx: &mut gpui::VisualTestContext| map.read_with(cx, |v, _| v.feature_at(centre.0, centre.1).map(|f| f.index));
        assert_eq!(picked(cx), Some(0), "no window: point 0");
        map.update(cx, |v, cx| v.set_source_filter_range("q", Some([0.0, 10.0]), cx));
        assert_eq!(picked(cx), Some(0), "inside the window");
        map.update(cx, |v, cx| v.set_source_filter_range("q", Some([20.0, 60.0]), cx));
        assert_eq!(picked(cx), None, "hidden by the window");

        // Above the labels, over an arc through the centre: the visible
        // point wins, the hidden one yields to the arc.
        map.update(cx, |v, cx| {
            v.set_source_below_labels("q", false);
            let set = crate::overlay::arcs::build(
                &[[-93.7, 41.9]],
                &[[-93.5, 41.9]],
                &crate::overlay::arcs::ArcColors::Constant { source: crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, target: crate::style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 } },
                &crate::overlay::arcs::ArcWidths::Constant(2.0),
                None,
            );
            v.set_arc_layer("a", Arc::new(set), crate::render::arcs::ArcStyle { height: 0.1, ..Default::default() }, cx);
        });
        crate::map_view::pick::tests::wait_for_arc_index(&map, cx, "a");
        let kind = |cx: &mut gpui::VisualTestContext| map.read_with(cx, |v, _| v.pick(centre.0, centre.1).map(|p| p.object));
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Arc { .. })), "the hidden point yields to the arc: {:?}", kind(cx));
        map.update(cx, |v, cx| v.set_source_filter_range("q", None, cx));
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Feature(_))), "shown, it covers the arc: {:?}", kind(cx));

        // Packed: two fields, each window the shader's.
        map.update(cx, |v, cx| {
            v.set_source_filter_fields("q", &["value", "flag"], cx);
        });
        paint_until(&map, cx, |v| v.point_source_settled("q"));
        map.update(cx, |v, cx| v.set_source_filter_ranges("q", &[None, Some([0.5, 1.0])], cx));
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Feature(_))), "flag 1 passes: {:?}", kind(cx));
        map.update(cx, |v, cx| v.set_source_filter_ranges("q", &[None, Some([-0.5, 0.5])], cx));
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Arc { .. })), "flag 1 fails [-0.5, 0.5]: {:?}", kind(cx));

        // By name: each field's window moves alone, the other's kept.
        assert_eq!(map.read_with(cx, |v, _| v.source_filter_fields("q")), Some(vec!["value".to_string(), "flag".to_string()]));
        map.update(cx, |v, cx| assert!(v.set_source_filter_field_range("q", "flag", None, cx)));
        map.update(cx, |v, cx| assert!(v.set_source_filter_field_range("q", "value", Some([0.0, 10.0]), cx)));
        assert_eq!(map.read_with(cx, |v, _| v.source_filter_ranges("q").map(<[_]>::to_vec)), Some(vec![Some([0.0, 10.0]), None]));
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Feature(_))), "value 5 in [0, 10], flag free: {:?}", kind(cx));
        map.update(cx, |v, cx| assert!(v.set_source_filter_field_range("q", "flag", Some([-0.5, 0.5]), cx)));
        assert_eq!(map.read_with(cx, |v, _| v.source_filter_ranges("q").map(<[_]>::to_vec)), Some(vec![Some([0.0, 10.0]), Some([-0.5, 0.5])]), "value's window kept");
        assert!(matches!(kind(cx), Some(crate::map_view::MapObject::Arc { .. })), "both windows apply: {:?}", kind(cx));
        map.update(cx, |v, cx| assert!(!v.set_source_filter_field_range("q", "nope", Some([0.0, 1.0]), cx), "not a filter field"));
    }

    /// **The window is tested on the pushed ROW, not the view's feature
    /// index**: a view lists its points in the spatial index's order, so
    /// view feature `i` is generally not row `i`. Pushed east to west, a
    /// hidden row is never picked and a shown row's properties are its own
    /// (the California earthquakes page picked a 1981 quake under a
    /// 1991-2011 window before this).
    #[test]
    fn the_filter_window_is_tested_on_the_pushed_row() {
        let mut app = gpui::TestAppContext::single();
        let (map, cx) = app.add_window_view(|_, _| {
            MapView::new(
                -93.6,
                41.9,
                12.3,
                800.0,
                600.0,
                crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
                Arc::new(crate::tile_source::EmptyTileSource),
                crate::map_view::test_runtime_handle(),
            )
        });
        // Forty points along a line, pushed EAST to WEST, value = row.
        let n = 40;
        let points: Vec<[f64; 2]> = (0..n).map(|i| [-93.55 - f64::from(i) * 0.0025, 41.9]).collect();
        let value: Vec<Option<f64>> = (0..n).map(|i| Some(f64::from(i))).collect();
        let paint = crate::overlay::FeaturePaint { circle_radius: 4.0, ..Default::default() };
        map.update(cx, |v, cx| {
            v.set_source_points("q", &points, None, &paint, &[("value", crate::overlay::PropertyColumn::Number(&value))], cx).unwrap();
            v.set_source_filter_field("q", Some("value"), cx);
        });
        let at = |cx: &mut gpui::VisualTestContext, row: usize| {
            map.read_with(cx, |v, _| {
                crate::render::project_world_to_screen_f64(v.camera_control.camera(), crate::overlay::geom::lon_lat_to_world(points[row][0], points[row][1])).expect("on screen")
            })
        };
        paint_until(&map, cx, |v| v.point_source_settled("q"));
        let picked_value = |cx: &mut gpui::VisualTestContext, row: usize| {
            let p = at(cx, row);
            map.read_with(cx, |v, _| v.feature_at(p.0, p.1).and_then(|f| v.feature_properties(&f)).and_then(|p| p.get("value").cloned()))
        };
        let views_differ = map.read_with(cx, |v, _| {
            let p = crate::render::project_world_to_screen_f64(v.camera_control.camera(), crate::overlay::geom::lon_lat_to_world(points[30][0], points[30][1])).unwrap();
            v.feature_at(p.0, p.1).is_some_and(|f| f.index != 30)
        });
        assert!(views_differ, "the view's order differs from the push order (else this test proves nothing)");
        map.update(cx, |v, cx| v.set_source_filter_range("q", Some([0.0, 19.0]), cx));
        for row in [0, 5, 19] {
            assert_eq!(picked_value(cx, row), Some(crate::mvt::TagValue::Number(row as f64)), "row {row} is shown: its own properties");
        }
        for row in [20, 30, 39] {
            assert_eq!(picked_value(cx, row), None, "row {row} is hidden: not picked");
        }
    }
}
