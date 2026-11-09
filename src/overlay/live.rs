//! Incremental live updates for large tiled overlay sources: a
//! [`SourceDiff`] (remove, add, update, by feature id) applied to what a
//! source already holds -- its rows, columns, classes and tile index --
//! without rebuilding any of them.
//!
//! See `docs/superpowers/specs/2026-09-26-incremental-overlay-updates-design.md`.
//!
//! The shape is MapLibre's updateable GeoJSON source (`GeoJSONSource.updateData`,
//! `geojson_source_diff.ts`, BSD-3): remove, then add, then update; an
//! unknown id in `remove`/`update` is skipped; an add whose id exists
//! replaces that feature; diffs that arrive while one is being applied are
//! merged, and at most one is in flight. Where MapLibre's worker rebuilds
//! geojson-vt from the updated features, this applies the diff to the
//! index in place (`TileIndex::applied`).
//!
//! Split in two: [`crate::overlay::OverlayStore::accept_source_diff`]
//! validates and resolves ids to rows on the caller's thread (cheap, and
//! the only fallible step), and [`apply`] does the work on a worker from
//! the source's current `Arc`s, returning new ones for the UI thread to
//! swap in.

use std::sync::Arc;

use crate::mvt::TagValue;
use crate::overlay::geojson::OverlayGeometry;
use crate::overlay::input::{FeatureInput, GeometryInput};
use crate::overlay::plan::{BakeBracket, FeatureClasses};
use crate::overlay::tile_index::{IndexEdit, TileIndex, NO_CLASS};
use crate::overlay::{hit, source_paint, FeatureId, OverlayFeature, PropertyTable, SourceId};

/// Changes to one source, by feature id. Applied in MapLibre's order:
/// every `remove_ids`, then every `add`, then every `update`.
#[derive(Debug, Clone, Default)]
pub struct SourceDiff<'a> {
    /// Features to remove. An id the source does not hold is skipped.
    pub remove_ids: Vec<FeatureId>,
    /// Features to add. Each must have an id; one whose id the source
    /// already holds REPLACES that feature (a remove, then this add).
    pub add: Vec<AddFeature<'a>>,
    /// Features to change in place. An id the source does not hold is
    /// skipped.
    pub update: Vec<FeatureUpdate<'a>>,
}

/// One added feature: the feature (geometry, own paint, id) and its
/// properties.
#[derive(Debug, Clone)]
pub struct AddFeature<'a> {
    pub feature: FeatureInput<'a>,
    pub properties: Vec<(String, TagValue)>,
}

/// One in-place change: new geometry (`None` keeps it), and property
/// writes -- `Some` sets the key, `None` clears it; keys not named keep
/// their values (MapLibre's `addOrUpdateProperties` / `removeProperties`).
#[derive(Debug, Clone)]
pub struct FeatureUpdate<'a> {
    pub id: FeatureId,
    pub geometry: Option<GeometryInput<'a>>,
    pub properties: Vec<(String, Option<TagValue>)>,
}

/// Which path a diff took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePath {
    /// Applied in place: same generation, only the touched tiles re-bake.
    Incremental,
    /// The source was not on the incremental path (untiled, its index not
    /// landed yet, or it has point features): the post-diff collection
    /// was installed as a push, at a new generation.
    Push,
}

/// What `update_source_features` did with a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveUpdate {
    pub path: LivePath,
    /// Ids in `remove_ids` or `update` the source did not hold.
    pub skipped: usize,
    /// This diff's number within its source, for `MapView::live_samples`.
    pub seq: u64,
}

/// A diff with its ids resolved to rows: owned, so it can cross to a
/// worker. Rows are stable (see the spec): an added feature's row is
/// assigned at resolve time from the source's `next_row`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedDiff {
    /// Rows removed (tombstoned), in order.
    pub kill: Vec<usize>,
    /// Added features; each one's `index` is its new row.
    pub add: Vec<OverlayFeature>,
    /// Rows whose geometry changes, and to what (lon/lat).
    pub geometry: Vec<(usize, OverlayGeometry)>,
    /// Property writes, in order, for existing and added rows.
    pub writes: Vec<(usize, String, Option<TagValue>)>,
    pub seq: u64,
}

/// A source's state that a diff applies to: the current `Arc`s.
#[derive(Clone)]
pub(crate) struct LiveBase {
    pub source: SourceId,
    pub features: Arc<Vec<OverlayFeature>>,
    pub properties: Arc<PropertyTable>,
    pub index: Arc<TileIndex>,
    pub classes: Arc<FeatureClasses>,
    pub hit_index: Arc<hit::HitIndex>,
    pub paint: Arc<source_paint::SourcePaint>,
    pub bracket: BakeBracket,
}

/// What [`apply`] hands back.
pub(crate) struct LiveResult {
    pub features: Arc<Vec<OverlayFeature>>,
    pub properties: Arc<PropertyTable>,
    pub index: Arc<TileIndex>,
    pub classes: Arc<FeatureClasses>,
    pub hit_index: Arc<hit::HitIndex>,
    /// Every changed feature's box before and after (fixed point), for
    /// tile invalidation: the tiles they touch re-bake.
    pub boxes: Vec<[u32; 4]>,
    /// Rows whose ONLY change is a class a baked compact tile can take by
    /// rewriting their vertices' paint index -- both classes fills with
    /// the same outline presence (see `tile_plan::FeatureSpans`): `(row,
    /// new class, box)`, ascending by row. The class-only fast path.
    pub repaint: Vec<(u32, u16, [u32; 4])>,
    /// When rows were compacted (tombstones past
    /// [`COMPACT_DEAD_PERCENT`]): `remap[old row]` is the new row
    /// (`u32::MAX`: dropped). `repaint` and `boxes` are in the OLD rows --
    /// what the cached tiles hold -- and everything else in the new.
    pub remap: Option<Arc<Vec<u32>>>,
    /// Whether classes were appended (the paint tables must grow).
    pub grew: bool,
    /// The seqs this result covers.
    pub seqs: Vec<u64>,
    pub rows_changed: usize,
    pub took: std::time::Duration,
}

/// Row compaction runs when tombstoned rows pass this percentage of all
/// rows (a removed feature's row is a tombstone until then).
pub(crate) const COMPACT_DEAD_PERCENT: usize = 10;

/// Below this many rows the percentage is taken of this many instead (as
/// `TileIndex::wants_compaction` does): a small source's tombstones cost
/// nothing worth a renumbering.
const COMPACT_MIN_ROWS: usize = 1 << 16;

#[cfg(test)]
thread_local! {
    /// Tests compact small fixtures: `COMPACT_MIN_ROWS` on this thread.
    pub(crate) static COMPACT_MIN_ROWS_FOR_TEST: std::cell::Cell<usize> = const { std::cell::Cell::new(COMPACT_MIN_ROWS) };
}

/// Whether `dead` tombstones of `rows` call for a row compaction.
fn compaction_due(dead: usize, rows: usize) -> bool {
    #[cfg(test)]
    let min = COMPACT_MIN_ROWS_FOR_TEST.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let min = COMPACT_MIN_ROWS;
    dead > 0 && dead * 100 > rows.max(min) * COMPACT_DEAD_PERCENT
}

/// `row` in the numbering after a compaction by `remap`: rows past it
/// (added by diffs accepted while the compaction ran) shift down by the
/// rows it dropped. `None` for a dropped row.
pub(crate) fn remapped_row(remap: &[u32], row: usize) -> Option<usize> {
    match remap.get(row) {
        Some(&r) => (r != u32::MAX).then_some(r as usize),
        None => {
            let kept = remap.iter().rev().find(|&&r| r != u32::MAX).map_or(0, |&r| r as usize + 1);
            Some(row - remap.len() + kept)
        }
    }
}

impl ResolvedDiff {
    /// This diff's rows after a compaction by `remap` (it was resolved
    /// against the rows before it). A row the compaction dropped cannot be
    /// named by a diff accepted after its removal; one is skipped.
    pub(crate) fn remap_rows(&mut self, remap: &[u32]) {
        let m = |r: usize| remapped_row(remap, r);
        self.kill = self.kill.iter().filter_map(|&r| m(r)).collect();
        for f in &mut self.add {
            f.index = m(f.index).expect("an added row is past the compaction");
        }
        self.geometry = std::mem::take(&mut self.geometry).into_iter().filter_map(|(r, g)| Some((m(r)?, g))).collect();
        self.writes = std::mem::take(&mut self.writes).into_iter().filter_map(|(r, k, v)| Some((m(r)?, k, v))).collect();
    }
}

/// Whether a baked compact tile can move a feature from class `old` to
/// `new` by rewriting its vertices' paint index: both fills, both with or
/// both without a hairline (a tile bakes outline indices only for a class
/// that has one).
fn repaints_in_place(classes: &[crate::overlay::plan::ClassPaint], old: u16, new: u16) -> bool {
    use crate::overlay::plan::ClassPaint;
    match (classes.get(usize::from(old)), classes.get(usize::from(new))) {
        (Some(ClassPaint::Fill { outline: a, .. }), Some(ClassPaint::Fill { outline: b, .. })) => a.is_some() == b.is_some(),
        _ => false,
    }
}

/// Applies `diffs`, in order, to `base`. `None` when the class set would
/// pass 65,534 classes: the caller falls back to a push.
pub(crate) fn apply(base: &LiveBase, diffs: &[ResolvedDiff]) -> Option<LiveResult> {
    let started = crate::timing::Instant::now();
    let reshapes = diffs.iter().any(|d| !d.kill.is_empty() || !d.add.is_empty() || !d.geometry.is_empty());
    let mut features: Option<Vec<OverlayFeature>> = reshapes.then(|| base.features.as_ref().clone());
    let before_rows = base.features.len();
    let mut rows = before_rows;
    let mut dead: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut geom_rows: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut written: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut writes: Vec<(usize, &str, Option<&TagValue>)> = Vec::new();
    for d in diffs {
        for &row in &d.kill {
            dead.insert(row);
            geom_rows.remove(&row);
        }
        for f in &d.add {
            let fs = features.as_mut().expect("an add reshapes");
            debug_assert_eq!(f.index, fs.len(), "rows are assigned in order");
            fs.push(f.clone());
            rows += 1;
            geom_rows.insert(f.index);
        }
        for (row, g) in &d.geometry {
            if !dead.contains(row) {
                features.as_mut().expect("a geometry update reshapes")[*row].geometry = g.clone();
                geom_rows.insert(*row);
            }
        }
        for (row, key, value) in &d.writes {
            writes.push((*row, key.as_str(), value.as_ref()));
            written.insert(*row);
        }
    }
    if let Some(fs) = features.as_mut() {
        for &row in &dead {
            fs[row].geometry = OverlayGeometry::Empty;
            fs[row].id = None;
        }
    }
    let kill: Vec<usize> = dead.iter().copied().collect();
    let properties = Arc::new(base.properties.edited(rows, &writes, &kill));
    let feats: &[OverlayFeature] = features.as_deref().unwrap_or(&base.features);

    // Re-class only the changed rows, into the existing class set.
    let picks: Vec<usize> =
        written.union(&geom_rows).copied().filter(|r| !dead.contains(r)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let (classes, of_pick) = crate::overlay::plan::classify_into(
        &base.classes.classes,
        feats,
        &picks,
        &properties,
        base.bracket,
        &base.paint,
    )?;
    let mut of_feature = base.classes.of_feature.clone();
    of_feature.resize(rows, NO_CLASS);
    for &row in &dead {
        of_feature[row] = NO_CLASS;
    }
    let mut edits: Vec<IndexEdit<'_>> = Vec::with_capacity(kill.len() + picks.len());
    edits.extend(kill.iter().map(|&r| IndexEdit::Remove(r as u32)));
    for (&row, &class) in picks.iter().zip(&of_pick) {
        let old = base.classes.of_feature.get(row).copied().unwrap_or(NO_CLASS);
        of_feature[row] = class;
        if geom_rows.contains(&row) {
            edits.push(IndexEdit::Upsert { row: row as u32, class, geometry: Some(&feats[row].geometry) });
        } else if class != old {
            edits.push(IndexEdit::Upsert { row: row as u32, class, geometry: None });
        }
    }
    let (index, mut delta) = base.index.applied(&edits);
    drop(edits);
    let mut repaint = Vec::with_capacity(delta.recolored.len());
    for (row, old, bbox) in std::mem::take(&mut delta.recolored) {
        let new = of_feature[row as usize];
        if repaints_in_place(&classes, old, new) {
            repaint.push((row, new, bbox));
        } else {
            delta.boxes.push(bbox);
        }
    }
    repaint.sort_unstable_by_key(|r| r.0);
    // The index is the one geometry copy of what it holds: as at adoption.
    if let Some(fs) = features.as_mut() {
        for &row in &geom_rows {
            let line = matches!(fs[row].geometry, OverlayGeometry::Line(_));
            if let Some(bbox_lon_lat) = index.feature_bbox_lon_lat(row as u32) {
                fs[row].geometry = OverlayGeometry::Indexed { bbox_lon_lat, line };
            }
        }
    }
    // Row compaction: tombstones past COMPACT_DEAD_PERCENT of the rows.
    // Here, on the worker, and with no geometry or class changed: no tile
    // re-bakes for it and nothing leaves the screen.
    let dead_rows = properties.dead_rows();
    let remap: Option<Vec<u32>> = (reshapes && compaction_due(dead_rows, rows)).then(|| {
        let mut next = 0u32;
        (0..rows)
            .map(|r| {
                if properties.is_dead(r) {
                    u32::MAX
                } else {
                    next += 1;
                    next - 1
                }
            })
            .collect()
    });
    let index = match &remap {
        Some(m) => index.compacted(Some(m)),
        None if index.wants_compaction() => index.compacted(None),
        None => index,
    };
    let (properties, of_feature) = match &remap {
        Some(m) => {
            let keep: Vec<bool> = m.iter().map(|&r| r != u32::MAX).collect();
            let of: Vec<u16> = of_feature.iter().zip(&keep).filter(|(_, &k)| k).map(|(&c, _)| c).collect();
            (Arc::new(properties.compacted(&keep)), of)
        }
        None => (properties, of_feature),
    };
    if let Some(m) = &remap {
        let fs = features.get_or_insert_with(|| base.features.as_ref().clone());
        let kept: Vec<OverlayFeature> = std::mem::take(fs)
            .into_iter()
            .zip(m)
            .filter(|(_, &r)| r != u32::MAX)
            .map(|(mut f, &r)| {
                f.index = r as usize;
                f
            })
            .collect();
        *fs = kept;
    }
    let (features, hit_index) = match features {
        Some(fs) if remap.is_some() => {
            let hit = Arc::new(hit::HitIndex::build(&base.source, &fs));
            (Arc::new(fs), hit)
        }
        Some(fs) => {
            let hit = Arc::new(hit::HitIndex::build(&base.source, &fs));
            (Arc::new(fs), hit)
        }
        None => (Arc::clone(&base.features), Arc::clone(&base.hit_index)),
    };
    let grew = classes.len() > base.classes.classes.len();
    Some(LiveResult {
        features,
        properties,
        index: Arc::new(index),
        classes: Arc::new(FeatureClasses { classes, of_feature, data_driven: base.classes.data_driven }),
        hit_index,
        boxes: delta.boxes,
        repaint,
        remap: remap.map(Arc::new),
        grew,
        seqs: diffs.iter().map(|d| d.seq).collect(),
        rows_changed: picks.len() + kill.len(),
        took: started.elapsed(),
    })
}
