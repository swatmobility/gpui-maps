//! The capture session's baked-tile cache (map-stories plan, S0.2).
//!
//! A tile's bake -- symbol matching, static matching, plan resolution,
//! repaint, tessellation and the buffer upload -- depends only on the tile,
//! the style, the zoom it was resolved at and a few request switches. A
//! story renders the same tiles for many consecutive frames, so the session
//! keeps each bake under a [`MeshKey`] naming every one of those inputs and
//! draws it again on a hit. The per-frame model bind group (camera matrix,
//! blend `t`, dash scale, globe uniform, clip) is NOT cached: it is rebuilt
//! every frame exactly as before.
//!
//! **The tile's data is in the key** ([`MeshKey::data`], the decoded
//! tile's `fingerprint`, hashed from its bytes once at decode), so a source
//! switch -- a different `TileStore` under the same ids -- is safe by
//! construction. **The style is not**: `Style` has no cheap identity, so a
//! style edit relies on the caller bumping
//! [`CaptureRequest::style_generation`](super::CaptureRequest::style_generation).
//! In debug builds every entry records a fingerprint of the style it was
//! baked under and a hit under a different style fails a `debug_assert`,
//! so a missed bump fails every test and debug story run.
//!
//! The cache is bounded by a byte budget, LRU-evicted on insert.
use std::collections::HashMap;
use std::sync::Arc;

use crate::geo::TileId;

/// Every input a tile's bake reads besides the tile's own data and the
/// style (which [`Self::style_gen`] stands for).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct MeshKey {
    pub(crate) tile: TileId,
    /// `DecodedTile::fingerprint`: which data this tile id held.
    pub(crate) data: u64,
    pub(crate) display_zoom: u8,
    pub(crate) cover_tile_zoom: u8,
    pub(crate) style_gen: u64,
    /// `Paint` as a `u8`: `AsStyled` 0, `FlattenedFills` 1, `Wash` 2.
    pub(crate) paint: u8,
    /// The tile's `Subdivision`, packed: granularity in the low 30 bits,
    /// the north pole in bit 30, the south pole in bit 31.
    pub(crate) subdivision: u32,
    pub(crate) masked: bool,
    pub(crate) compact: bool,
    /// Whether a compact bake split its undashed strokes too
    /// (`CaptureRequest::basemap_compact_strokes`, resolved).
    pub(crate) compact_strokes: bool,
    /// Whether the entry carries the tile's `SymbolPlan`.
    pub(crate) labels: bool,
    /// Whether the frame it was baked for extrudes at all
    /// (`ExtrusionPlan::steps` non-empty): only then is the tile's
    /// `fill-extrusion` geometry tessellated into
    /// [`CachedMesh::extrusion`]. Keyed so a bake without buildings is
    /// never drawn on a frame that should raise them, nor one with them on
    /// a frame that should not. The per-frame extrusion inputs (opacity
    /// override, flatten, light, bearing) live in the params uniform, not
    /// the mesh, so they need no key.
    pub(crate) extrudes: bool,
}

/// `Subdivision` packed for [`MeshKey::subdivision`].
pub(crate) fn subdivision_key(s: crate::render::subdivide::Subdivision) -> u32 {
    debug_assert!(s.granularity < (1 << 30), "granularity {} does not pack", s.granularity);
    (s.granularity & 0x3fff_ffff) | (u32::from(s.north_pole) << 30) | (u32::from(s.south_pole) << 31)
}

/// `Paint` as a `u8` for [`MeshKey::paint`].
pub(crate) fn paint_key(paint: super::Paint) -> u8 {
    match paint {
        super::Paint::AsStyled => 0,
        super::Paint::FlattenedFills => 1,
        super::Paint::Wash => 2,
    }
}

/// One tile's bake. `buffers` is `None` for a tile whose mesh has no
/// indices (it draws no flat geometry, but its symbol plan still counts,
/// and its buildings, if any, still draw).
pub(crate) struct CachedMesh {
    pub(crate) buffers: Option<crate::render::TileBuffers>,
    /// The tile's building buffers (`create_extrusion_buffers`): `Some`
    /// only when the key's `extrudes` is set and the tile has any.
    pub(crate) extrusion: Option<crate::render::extrusion_gpu::ExtrusionGpu>,
    /// The tile's roofs as a height grid (an `ExtrusionMesh` with no
    /// chunks), for `CaptureRequest::label_occlusion`: `Some` exactly when
    /// [`Self::extrusion`] is.
    pub(crate) occluder: Option<std::sync::Arc<crate::render::extrusion::ExtrusionMesh>>,
    pub(crate) vertices: usize,
    pub(crate) triangles: usize,
    /// `Some` exactly when the key's `labels` is set.
    pub(crate) symbol_plan: Option<crate::style::SymbolPlan>,
    /// The fingerprint of the style this was baked under, checked on every
    /// hit in debug builds (see the module doc).
    #[cfg(debug_assertions)]
    pub(crate) style_fingerprint: u64,
}

impl CachedMesh {
    /// Whether the tile draws anything in the tile pass: flat geometry or
    /// buildings (a buildings-only tile has no flat buffers, review F3).
    pub(crate) fn draws_anything(&self) -> bool {
        self.buffers.is_some() || self.extrusion.is_some()
    }

    /// What this bake weighs against the budget: its device buffers, its
    /// symbol plan's heap, and a fixed per-entry charge.
    pub(crate) fn budget_bytes(&self) -> u64 {
        self.buffers.as_ref().map_or(0, crate::render::TileBuffers::device_bytes)
            + self.extrusion.as_ref().map_or(0, crate::render::extrusion_gpu::ExtrusionGpu::device_bytes)
            + self.symbol_plan.as_ref().map_or(0, |p| p.footprint_bytes() as u64)
            + self.occluder.as_ref().map_or(0, |m| (m.height_grid.cells.capacity() * std::mem::size_of::<f32>()) as u64)
            + ENTRY_OVERHEAD_BYTES
    }
}

/// A fingerprint of a whole style, for the debug-build stale-bake check:
/// a hash of its `Debug` form. Debug builds only -- it walks every layer.
#[cfg(debug_assertions)]
pub(crate) fn style_fingerprint(style: &crate::style::Style) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{style:?}").hash(&mut h);
    h.finish()
}

/// A fixed charge per entry on top of its device bytes, so tiles with no
/// buffers (empty meshes) still count against the budget and the entry
/// count stays bounded.
const ENTRY_OVERHEAD_BYTES: u64 = 256;

struct Entry {
    last_used: u64,
    bytes: u64,
    value: Arc<CachedMesh>,
}

/// Hit, miss and size counters for [`super::CaptureSession::mesh_cache_stats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MeshCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Device bytes held, plus a small fixed charge per entry.
    pub bytes: u64,
    pub entries: usize,
}

pub(crate) struct MeshCache {
    map: HashMap<MeshKey, Entry>,
    /// The use clock: bumped on every lookup and insert.
    lru: u64,
    bytes: u64,
    budget: u64,
    hits: u64,
    misses: u64,
}

/// The default budget: 512 MiB.
pub(crate) const DEFAULT_BUDGET: u64 = 512 << 20;

impl MeshCache {
    pub(crate) fn new(budget: u64) -> Self {
        Self { map: HashMap::new(), lru: 0, bytes: 0, budget, hits: 0, misses: 0 }
    }

    /// The bake under `key`, marking it used; counts a hit or a miss.
    pub(crate) fn get(&mut self, key: &MeshKey) -> Option<Arc<CachedMesh>> {
        self.lru += 1;
        if let Some(entry) = self.map.get_mut(key) {
            entry.last_used = self.lru;
            self.hits += 1;
            Some(Arc::clone(&entry.value))
        } else {
            self.misses += 1;
            None
        }
    }

    /// Store a bake and evict least-recently-used entries until the cache
    /// fits its budget -- never the entry just inserted. Returns it shared.
    pub(crate) fn insert(&mut self, key: MeshKey, mesh: CachedMesh) -> Arc<CachedMesh> {
        self.lru += 1;
        let bytes = mesh.budget_bytes();
        let value = Arc::new(mesh);
        if let Some(old) = self.map.insert(key, Entry { last_used: self.lru, bytes, value: Arc::clone(&value) }) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        self.evict_to_budget(Some(key));
        value
    }

    fn evict_to_budget(&mut self, keep: Option<MeshKey>) {
        while self.bytes > self.budget {
            let Some(victim) = self
                .map
                .iter()
                .filter(|(k, _)| Some(**k) != keep)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            let entry = self.map.remove(&victim).expect("just found");
            self.bytes -= entry.bytes;
        }
    }

    pub(crate) fn set_budget(&mut self, bytes: u64) {
        self.budget = bytes;
        self.evict_to_budget(None);
    }

    /// The heaviest entry's charge, for the bound test.
    #[cfg(test)]
    pub(crate) fn largest_entry_bytes(&self) -> u64 {
        self.map.values().map(|e| e.bytes).max().unwrap_or(0)
    }

    pub(crate) fn stats(&self) -> MeshCacheStats {
        MeshCacheStats { hits: self.hits, misses: self.misses, bytes: self.bytes, entries: self.map.len() }
    }
}
