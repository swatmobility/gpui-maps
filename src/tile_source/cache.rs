use std::num::NonZeroUsize;

use lru::LruCache;

use crate::geo::TileId;

pub struct TileCache<V> {
    entries: LruCache<TileId, Entry<V>>,
    /// `None` for a cache bounded by entry count alone -- the original
    /// behaviour, and what [`TileCache::new`] still builds. `Some` adds a
    /// SECOND, independent bound in bytes; see [`TileCache::with_byte_budget`].
    budget: Option<ByteBudget<V>>,
    /// Where an evicted or replaced value goes instead of being dropped in
    /// place. `None` (the default) drops it here. See
    /// [`TileCache::with_evicted_sink`].
    evicted_sink: Option<fn(V)>,
    /// The entry bound this cache was built with. `entries`' own capacity
    /// can rise above it only inside [`Self::put_with_bytes_keeping`],
    /// when every entry is kept, and comes back down as soon as they are
    /// not.
    base_capacity: NonZeroUsize,
}

/// One cached value and what it was weighed at.
///
/// The weight is carried per entry rather than recomputed on demand
/// because eviction has to subtract the cost of a value it is in the act
/// of dropping, and a weight recomputed from the value at that moment
/// would be recomputed once per eviction for the life of the process. It
/// is a `usize` beside a value that is measured in megabytes, so the
/// bookkeeping costs nothing worth counting.
///
/// A count-bounded cache stores `0` here and never reads it back.
struct Entry<V> {
    value: V,
    bytes: usize,
}

/// The byte half of a [`TileCache`]'s bound.
///
/// `weigh` is a plain `fn` pointer rather than a `V: HeapSize` trait bound
/// on `TileCache` itself, and that is deliberate: a bound on the struct
/// would have to be satisfied by every existing `TileCache<V>` in the
/// crate -- including `TileCache<Instant>` (`MapView::failed`) and
/// `TileCache<()>` (`MapView::absent`), for which a heap footprint is
/// meaningless -- and would be a breaking change to a public type for the
/// sake of caches that do not want the feature. A function pointer keeps
/// the byte budget entirely opt-in, per cache, at construction.
struct ByteBudget<V> {
    /// The ceiling `held_bytes` is trimmed back to after every insert.
    max_bytes: usize,
    /// The sum of every live entry's `Entry::bytes`. Maintained
    /// incrementally by `put`/`remove`/`clear`/`reweigh` rather than
    /// recomputed, so reading it is free and no caller is tempted to walk
    /// the cache to find out how full it is.
    held_bytes: usize,
    weigh: fn(&V) -> usize,
}

impl<V> TileCache<V> {
    /// A cache bounded by entry COUNT only.
    ///
    /// Correct for a cache whose values are all the same size -- `failed`'s
    /// `Instant`s, `absent`'s `()`s -- and wrong for one whose values are
    /// tiles, because a tile's cost varies by four orders of magnitude
    /// between an offshore tile and a dense city-centre one (measured:
    /// 0.005 MB against 94.2 MB across all seven per-tile caches, see
    /// `MapView`'s cache-budget constants). Use
    /// [`Self::with_byte_budget`] for those.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(capacity).expect("capacity must be > 0")),
            budget: None,
            evicted_sink: None,
            base_capacity: NonZeroUsize::new(capacity).expect("capacity must be > 0"),
        }
    }

    /// A cache bounded by entry count AND by bytes, whichever binds first.
    ///
    /// `weigh` is what one entry costs, in whatever unit the caller's
    /// budget is expressed in -- see `MapView`'s cache-budget constants
    /// for the unit this crate uses (a modelled heap-block sum, graded
    /// against the allocator by the stress suite) and why it is a computed
    /// figure rather than an estimate.
    ///
    /// # The two bounds are both real
    ///
    /// `max_entries` is not vestigial once a byte budget exists. A cache
    /// full of near-empty tiles would otherwise grow without limit -- the
    /// offshore tiles above weigh 16 bytes each, so a byte budget alone
    /// would admit millions of them and the `LruCache`'s own per-entry
    /// overhead (a `TileId`, two list pointers, a hash-table slot) would
    /// become the cost the budget was supposed to bound. Keeping both
    /// bounds means the cache holds `min(max_entries, what fits in
    /// max_bytes)`, which is the behaviour a count-based cap was reaching
    /// for and could not express.
    ///
    /// # One entry always survives
    ///
    /// A single value larger than `max_bytes` is KEPT, not evicted on
    /// arrival. This is not a leniency, it is a correctness requirement:
    /// every caller of `put` in this crate has just produced the value
    /// because something needs it now (a tile that is on screen this
    /// frame), and a cache that could answer `put` immediately followed by
    /// `get` with `None` would make that tile permanently unrenderable
    /// rather than merely expensive. So the budget bounds RETENTION -- how
    /// much history is kept behind the working set -- and the working set
    /// itself is bounded by `max_entries` and by the camera.
    pub fn with_byte_budget(
        max_entries: usize,
        max_bytes: usize,
        weigh: fn(&V) -> usize,
    ) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(max_entries).expect("capacity must be > 0")),
            budget: Some(ByteBudget { max_bytes, held_bytes: 0, weigh }),
            evicted_sink: None,
            base_capacity: NonZeroUsize::new(max_entries).expect("capacity must be > 0"),
        }
    }

    /// Hands every value this cache evicts or replaces to `sink` instead of
    /// dropping it on the caller's thread.
    ///
    /// For caches whose values are expensive to FREE: a decoded vector tile
    /// or a triangulation is tens of thousands of small allocations, and
    /// one tile install on the native frame thread measured 15-22 ms --
    /// almost all of it the evictions that install caused (block-groups
    /// zoom sweep, 2026-09-26). The native shell passes a sink that moves
    /// the value to a background thread; a shell without threads simply
    /// never sets one.
    pub fn with_evicted_sink(mut self, sink: fn(V)) -> Self {
        self.evicted_sink = Some(sink);
        self
    }

    fn discard(sink: Option<fn(V)>, value: V) {
        match sink {
            Some(sink) => sink(value),
            None => drop(value),
        }
    }

    pub fn get(&mut self, id: &TileId) -> Option<&V> {
        self.entries.get(id).map(|entry| &entry.value)
    }

    /// Same as `get`, but returns a mutable reference -- for a caller that
    /// needs to edit a cached value in place (e.g. `MapView::plan_for`
    /// rewriting a tile's cached triangulation's paint fields) rather than
    /// replace it outright via `put`.
    ///
    /// On a byte-budgeted cache the returned reference can be used to make
    /// the value bigger or smaller, and this method does NOT re-weigh it:
    /// the weight taken at `put` still stands until somebody calls
    /// [`Self::reweigh`]. That is the right default because the only
    /// in-place edit in this crate that changes a value's SIZE is
    /// `MapView::splice_cached_mesh` (which cuts ops out of a cached
    /// triangulation and stitches replacements in, changing the vertex and
    /// index counts) -- and it calls `reweigh` itself. Every other
    /// `get_mut` caller rewrites paint attributes, which touches the
    /// contents of existing vertices and not how many there are.
    pub fn get_mut(&mut self, id: &TileId) -> Option<&mut V> {
        self.entries.get_mut(id).map(|entry| &mut entry.value)
    }

    /// Inserts `value`, weighing it with this cache's own `weigh` function.
    ///
    /// On a byte-budgeted cache the weighing happens HERE, on the calling
    /// thread. For the caches whose weight is a walk over tens of
    /// thousands of features -- `decoded_cache`, `match_cache` -- that walk
    /// belongs off the UI thread, so `MapView::install_baked_tile` uses
    /// [`Self::put_with_bytes`] with a figure `bake_tile` already computed
    /// on the tokio worker instead.
    pub fn put(&mut self, id: TileId, value: V) {
        let bytes = self.budget.as_ref().map_or(0, |budget| (budget.weigh)(&value));
        self.put_with_bytes(id, value, bytes);
    }

    /// Inserts `value` at a weight the caller has already computed.
    ///
    /// Exists so the weighing walk can run wherever the value was
    /// produced -- off-thread, inside `bake_tile`, alongside the decode and
    /// the tessellation whose output it is measuring -- rather than on the
    /// UI thread at install time. The dense Tokyo z14 tile's decoded
    /// footprint walk is 123 microseconds and its `PaintPlan`'s is 127; a
    /// screen's worth across all five values is ~6 ms, which is not a cost
    /// a frame of 8.33 ms should carry for bookkeeping.
    ///
    /// `bytes` is trusted. A caller that passes a figure `weigh` would not
    /// have produced gets a cache whose `held_bytes` does not describe its
    /// contents, which is why both figures come from the same functions
    /// (`DecodedTile::footprint_bytes` and friends) and why
    /// `the_weight_install_passes_is_the_weight_the_cache_would_have_
    /// computed` asserts they agree.
    ///
    /// On a count-bounded cache `bytes` is recorded and never read, so
    /// there is no reason for a caller to branch on which kind of cache it
    /// holds.
    pub fn put_with_bytes(&mut self, id: TileId, value: V, bytes: usize) {
        // `push` and not `put`. `LruCache::put` returns the value it
        // REPLACED for the same key and says nothing when the insert
        // pushed a different entry out under the ENTRY bound -- so with
        // `put`, an eviction driven by the entry cap left its bytes on
        // `held_bytes` forever. That drifts upward for the life of the
        // process, and a cache whose running total exceeds its budget
        // while holding almost nothing evicts every tile that arrives.
        // `push` returns whichever entry left, for either reason, which is
        // exactly the accounting this needs. Caught by
        // `the_entry_bound_still_binds_for_cheap_entries`.
        let departed = self.entries.push(id, Entry { value, bytes });
        if let Some((_, left)) = departed {
            if let Some(budget) = &mut self.budget {
                budget.held_bytes = budget.held_bytes.saturating_sub(left.bytes);
            }
            Self::discard(self.evicted_sink, left.value);
        }
        if let Some(budget) = &mut self.budget {
            budget.held_bytes += bytes;
        }
        self.trim_to_budget();
    }

    /// Moves the byte bound to `max_bytes`, evicting LRU-first at once if
    /// the cache now holds more than that (keeping one entry, as ever). A
    /// no-op on a cache built by [`Self::new`], which has no byte bound.
    pub fn set_byte_budget(&mut self, max_bytes: usize) {
        if let Some(budget) = &mut self.budget {
            budget.max_bytes = max_bytes;
        }
        self.trim_to_budget();
    }

    /// Evicts LRU-first until the byte budget is met, keeping at least one
    /// entry -- see [`Self::with_byte_budget`] for why the last entry is
    /// never given up.
    ///
    /// `pop_lru` and not `pop` of a chosen key: the entry that just
    /// arrived is the MOST recently used, so it is the last thing this
    /// loop would reach, and the `len() > 1` guard stops it reaching it at
    /// all. A caller therefore never has to check whether its own insert
    /// survived.
    fn trim_to_budget(&mut self) {
        let Some(budget) = &mut self.budget else { return };
        while budget.held_bytes > budget.max_bytes && self.entries.len() > 1 {
            let Some((_, evicted)) = self.entries.pop_lru() else { break };
            budget.held_bytes = budget.held_bytes.saturating_sub(evicted.bytes);
            Self::discard(self.evicted_sink, evicted.value);
        }
    }

    /// [`Self::put_with_bytes`], except that neither bound ever evicts an
    /// entry `keep` answers true for -- each skips past them to the next
    /// least-recently-used one. When every entry is kept, the entry bound
    /// grows by one rather than drop one, and returns to the bound the
    /// cache was built with once the kept set allows. (The entry bound used
    /// to evict kept entries regardless: a 512-tile imagery cover at 256 px
    /// plus its floor overran a 512-entry cache and evicted itself.)
    ///
    /// For a cache whose working set is known to the caller and must not
    /// be traded for history: the tiled overlay keeps its cover and the
    /// stand-ins under it this way (MapLibre keeps in-view tiles out of its
    /// LRU altogether, `tile_manager.ts` `_inViewTiles`), so a pass through
    /// big low-zoom tiles cannot evict the tiles on screen. If the kept
    /// entries alone exceed the budget the cache runs over it rather than
    /// drop one.
    pub fn put_with_bytes_keeping(&mut self, id: TileId, value: V, bytes: usize, keep: impl Fn(&TileId) -> bool) {
        let sink = self.evicted_sink;
        if !self.entries.contains(&id) && self.entries.len() >= self.entries.cap().get() {
            // `iter().rev()` runs least- to most-recently used.
            let victim = self.entries.iter().rev().map(|(key, _)| *key).find(|key| !keep(key));
            match victim {
                Some(key) => {
                    if let Some(entry) = self.entries.pop(&key) {
                        if let Some(budget) = &mut self.budget {
                            budget.held_bytes = budget.held_bytes.saturating_sub(entry.bytes);
                        }
                        Self::discard(sink, entry.value);
                    }
                }
                None => {
                    let grown = self.entries.cap().saturating_add(1);
                    self.entries.resize(grown);
                }
            }
        }
        let departed = self.entries.push(id, Entry { value, bytes });
        // Back toward the built bound: unkept entries past it go
        // LRU-first, and the capacity follows what is left (`resize`
        // evicts only past the new capacity, so it drops nothing itself).
        if self.entries.cap() > self.base_capacity {
            let base = self.base_capacity.get();
            if self.entries.len() > base {
                let excess = self.entries.len() - base;
                let victims: Vec<TileId> = self
                    .entries
                    .iter()
                    .rev()
                    .map(|(key, _)| *key)
                    .filter(|key| *key != id && !keep(key))
                    .take(excess)
                    .collect();
                for key in victims {
                    if let Some(entry) = self.entries.pop(&key) {
                        if let Some(budget) = &mut self.budget {
                            budget.held_bytes = budget.held_bytes.saturating_sub(entry.bytes);
                        }
                        Self::discard(sink, entry.value);
                    }
                }
            }
            let cap = self.entries.len().max(base);
            self.entries.resize(NonZeroUsize::new(cap).expect("capacity must be > 0"));
        }
        let Some(budget) = &mut self.budget else {
            if let Some((_, left)) = departed {
                Self::discard(sink, left.value);
            }
            return;
        };
        if let Some((_, left)) = departed {
            budget.held_bytes = budget.held_bytes.saturating_sub(left.bytes);
            Self::discard(sink, left.value);
        }
        budget.held_bytes += bytes;
        if budget.held_bytes <= budget.max_bytes {
            return;
        }
        let mut over = budget.held_bytes - budget.max_bytes;
        let mut victims = Vec::new();
        // `iter` runs most- to least-recently used; the newcomer is first
        // and is never a victim.
        for (key, entry) in self.entries.iter().rev() {
            if over == 0 {
                break;
            }
            if *key == id || keep(key) {
                continue;
            }
            victims.push(*key);
            over = over.saturating_sub(entry.bytes);
        }
        for key in victims {
            if let Some(entry) = self.entries.pop(&key) {
                budget.held_bytes = budget.held_bytes.saturating_sub(entry.bytes);
                Self::discard(sink, entry.value);
            }
        }
    }

    pub fn contains(&self, id: &TileId) -> bool {
        self.entries.contains(id)
    }

    /// `get` without refreshing recency: a read that must not keep an
    /// entry alive (a per-frame check over the whole cover).
    pub fn peek(&self, id: &TileId) -> Option<&V> {
        self.entries.peek(id).map(|entry| &entry.value)
    }

    /// Makes `id` the next entry evicted (least recently used), if held:
    /// a tile kept only for a side purpose (the dot mask's ring) goes
    /// before anything drawn.
    pub fn demote(&mut self, id: &TileId) {
        self.entries.demote(id);
    }

    pub fn remove(&mut self, id: &TileId) -> Option<V> {
        let entry = self.entries.pop(id)?;
        if let Some(budget) = &mut self.budget {
            budget.held_bytes = budget.held_bytes.saturating_sub(entry.bytes);
        }
        Some(entry.value)
    }

    /// Re-weighs `id`'s entry after an in-place edit changed its size, and
    /// trims the cache if that pushed it over budget.
    ///
    /// Called by `MapView::splice_cached_mesh`, the one place in this crate
    /// where a `get_mut` changes how big a value is rather than what is in
    /// it. Without this, a tile spliced from 6,000 vertices to 600,000
    /// would go on being accounted at its old weight for as long as it
    /// stayed cached, and the budget would be a bound on a number that no
    /// longer described the cache.
    ///
    /// A no-op on a count-bounded cache, and on a key that is not present.
    pub fn reweigh(&mut self, id: &TileId) {
        let Some(budget) = &self.budget else { return };
        let weigh = budget.weigh;
        // `peek_mut`, not `get_mut`: re-weighing is bookkeeping, not a
        // read, and must not move the entry to the front of the LRU. An
        // entry that got cheaper should not also get a stay of execution.
        let Some(entry) = self.entries.peek_mut(id) else { return };
        let was = entry.bytes;
        let now = weigh(&entry.value);
        entry.bytes = now;
        let budget = self.budget.as_mut().expect("checked above");
        budget.held_bytes = budget.held_bytes.saturating_sub(was) + now;
        self.trim_to_budget();
    }

    /// What `id`'s entry was weighed at, or `None` if it is not held.
    /// Always `0` on a cache built by [`Self::new`].
    ///
    /// Does not disturb recency -- a measurement that changed what the
    /// next eviction picks would be observing a different cache than the
    /// one it reported on.
    #[must_use]
    pub fn entry_bytes(&self, id: &TileId) -> Option<usize> {
        self.entries.peek(id).map(|entry| entry.bytes)
    }

    /// The sum of every held entry's weight. Always `0` on a cache built
    /// by [`Self::new`], which weighs nothing.
    ///
    /// This is the figure the byte bound is enforced against, so it is
    /// also the only honest answer to "how full is this cache" for a
    /// cache whose entries differ in size by four orders of magnitude --
    /// [`Self::len`] against [`Self::capacity`] can read 25/512 while the
    /// cache holds 2.3 GB.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        self.budget.as_ref().map_or(0, |budget| budget.held_bytes)
    }

    /// The byte bound, or `None` for a cache bounded by entry count alone.
    #[must_use]
    pub fn byte_budget(&self) -> Option<usize> {
        self.budget.as_ref().map(|budget| budget.max_bytes)
    }

    /// How many tiles are held right now -- not the capacity.
    ///
    /// The two tables that exist to be SMALL -- `MapView`'s `failed` and
    /// `absent` -- are where this is read, and there the count IS the
    /// property under test: "a failure was recorded", "absence recorded
    /// none", "three covers' worth of puts left exactly one cover's
    /// worth". Nothing in the paint path reads it.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is held right now.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The ENTRY bound -- what `len()` may grow to. On a byte-budgeted
    /// cache this is the second of two bounds and usually not the one that
    /// binds; [`Self::byte_budget`] is the other, and
    /// [`Self::held_bytes`] is what it is enforced against.
    pub fn capacity(&self) -> usize {
        self.entries.cap().get()
    }

    /// Drops every entry, returning how many went.
    ///
    /// Exists for measurement (`MapView::drop_cache_for_measurement`): the
    /// heap this cache is holding is the difference between the allocator's
    /// in-use figure either side of this call, which needs the values
    /// actually dropped, not merely forgotten. `LruCache::clear` drops
    /// them, so the memory is released before this returns.
    pub fn clear(&mut self) -> usize {
        let dropped = self.entries.len();
        self.entries.clear();
        if let Some(budget) = &mut self.budget {
            // Not a subtraction of the summed weights: an empty cache
            // holds zero bytes by definition, and deriving that from the
            // entries would let a `saturating_sub` drift leave a non-zero
            // `held_bytes` behind on a cache with nothing in it -- which
            // would then evict the first tile put back into it.
            budget.held_bytes = 0;
        }
        dropped
    }

    /// Yields MRU->LRU, same as `LruCache::iter`.
    ///
    /// Previously widened to `impl DoubleEndedIterator`, for a caller (the
    /// eager `refresh_plans_for_display_zoom` bulk walk) that needed
    /// LRU->MRU order via `.rev()` rather than collecting and reversing by
    /// hand. That caller was deleted when eager rebake was replaced by
    /// lazy, per-tile rebake-on-read (see `MapView::plan_for`'s doc
    /// comment) -- no `.rev()` call on this method's result remains
    /// anywhere in the crate, so the plain forward-only `Iterator` `LruCache
    /// ::iter` already returns is narrowed back to.
    pub fn iter(&self) -> impl Iterator<Item = (&TileId, &V)> {
        self.entries.iter().map(|(id, entry)| (id, &entry.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::TileId;

    fn tile(x: u32) -> TileId {
        TileId { z: 1, x, y: 0 }
    }

    #[test]
    fn put_then_get_returns_the_value() {
        let mut cache = TileCache::new(2);
        cache.put(tile(1), "a");
        assert_eq!(cache.get(&tile(1)), Some(&"a"));
        assert!(cache.contains(&tile(1)));
    }

    #[test]
    fn missing_key_returns_none() {
        let mut cache: TileCache<&str> = TileCache::new(2);
        assert_eq!(cache.get(&tile(1)), None);
        assert!(!cache.contains(&tile(1)));
    }

    #[test]
    fn a_kept_entry_survives_the_byte_trim_and_the_next_lru_goes_instead() {
        let mut cache: TileCache<u8> = TileCache::with_byte_budget(10, 30, |_| 0);
        cache.put_with_bytes(tile(1), 1, 10);
        cache.put_with_bytes(tile(2), 2, 10);
        cache.put_with_bytes(tile(3), 3, 10);
        cache.put_with_bytes_keeping(tile(4), 4, 10, |t| *t == tile(1));
        assert!(cache.contains(&tile(1)), "kept, though least recently used");
        assert!(!cache.contains(&tile(2)), "the next LRU entry goes instead");
        assert!(cache.contains(&tile(3)) && cache.contains(&tile(4)));
        assert_eq!(cache.held_bytes(), 30);
        // Everything kept: the cache runs over budget rather than drop one.
        cache.put_with_bytes_keeping(tile(5), 5, 10, |_| true);
        assert_eq!((cache.len(), cache.held_bytes()), (4, 40));
    }

    #[test]
    fn a_kept_entry_survives_the_entry_bound_too_and_the_bound_comes_back() {
        let mut cache: TileCache<u8> = TileCache::with_byte_budget(2, 1000, |_| 0);
        cache.put_with_bytes(tile(1), 1, 1);
        cache.put_with_bytes(tile(2), 2, 1);
        cache.put_with_bytes_keeping(tile(3), 3, 1, |t| *t == tile(1));
        assert!(cache.contains(&tile(1)), "kept, though least recently used");
        assert!(!cache.contains(&tile(2)), "the next LRU entry goes instead");
        // Everything kept: the bound grows rather than drop one.
        cache.put_with_bytes_keeping(tile(4), 4, 1, |_| true);
        cache.put_with_bytes_keeping(tile(5), 5, 1, |_| true);
        assert_eq!((cache.len(), cache.capacity()), (4, 4));
        assert!([1, 3, 4, 5].iter().all(|&x| cache.contains(&tile(x))));
        // Nothing kept any more: back to the built bound, LRU-first.
        cache.put_with_bytes_keeping(tile(6), 6, 1, |_| false);
        cache.put_with_bytes_keeping(tile(7), 7, 1, |_| false);
        cache.put_with_bytes_keeping(tile(8), 8, 1, |_| false);
        assert_eq!((cache.len(), cache.capacity()), (2, 2));
        assert!(cache.contains(&tile(7)) && cache.contains(&tile(8)));
    }

    #[test]
    fn evicts_least_recently_used_when_over_capacity() {
        let mut cache = TileCache::new(2);
        cache.put(tile(1), "a");
        cache.put(tile(2), "b");
        cache.put(tile(3), "c"); // evicts tile(1), the least recently used
        assert_eq!(cache.get(&tile(1)), None);
        assert_eq!(cache.get(&tile(2)), Some(&"b"));
        assert_eq!(cache.get(&tile(3)), Some(&"c"));
    }

    /// Task 9 review I2: a demoted entry -- the dot mask's ring tile, just
    /// uploaded and so the newest -- is the next evicted, by count and by
    /// bytes, ahead of older entries.
    #[test]
    fn a_demoted_entry_is_evicted_first() {
        let mut cache = TileCache::new(3);
        cache.put(tile(1), "a");
        cache.put(tile(2), "b");
        cache.put(tile(3), "ring");
        cache.demote(&tile(3));
        cache.put(tile(4), "d");
        assert!(!cache.contains(&tile(3)), "the demoted newest entry went first");
        assert!(cache.contains(&tile(1)) && cache.contains(&tile(2)) && cache.contains(&tile(4)));
        let mut bytes: TileCache<u8> = TileCache::with_byte_budget(10, 30, |_| 0);
        bytes.put_with_bytes(tile(1), 1, 10);
        bytes.put_with_bytes(tile(2), 2, 10);
        bytes.put_with_bytes(tile(3), 3, 10);
        bytes.demote(&tile(3));
        bytes.put_with_bytes(tile(4), 4, 10);
        assert!(!bytes.contains(&tile(3)) && bytes.contains(&tile(1)));
        cache.demote(&tile(9)); // absent: nothing happens
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = TileCache::new(2);
        cache.put(tile(1), "a");
        cache.put(tile(2), "b");
        cache.get(&tile(1)); // touch tile(1), making tile(2) the least recently used
        cache.put(tile(3), "c"); // should evict tile(2), not tile(1)
        assert_eq!(cache.get(&tile(2)), None);
        assert_eq!(cache.get(&tile(1)), Some(&"a"));
        assert_eq!(cache.get(&tile(3)), Some(&"c"));
    }

    #[test]
    fn remove_evicts_only_the_given_entry() {
        let mut cache = TileCache::new(2);
        cache.put(tile(1), "a");
        cache.put(tile(2), "b");
        assert_eq!(cache.remove(&tile(1)), Some("a"));
        assert!(!cache.contains(&tile(1)));
        assert_eq!(cache.get(&tile(1)), None);
        // The other entry is untouched.
        assert!(cache.contains(&tile(2)));
        assert_eq!(cache.get(&tile(2)), Some(&"b"));
    }

    #[test]
    fn remove_of_a_missing_key_returns_none() {
        let mut cache: TileCache<&str> = TileCache::new(2);
        assert_eq!(cache.remove(&tile(1)), None);
    }

    #[test]
    fn iter_yields_every_entry_without_disturbing_recency() {
        let mut cache = TileCache::new(2);
        cache.put(tile(1), "a");
        cache.put(tile(2), "b");
        let mut seen: Vec<_> = cache.iter().map(|(&id, &v)| (id, v)).collect();
        seen.sort_by_key(|(id, _)| id.x);
        assert_eq!(seen, vec![(tile(1), "a"), (tile(2), "b")]);
        // iter() must not have touched recency: tile(1) is still the least
        // recently used (its last real access was the initial `put`, same
        // as tile(2)'s, but tile(2) was put second) -- putting a third
        // entry should evict tile(1), same as `evicts_least_recently_used_when_over_capacity`.
        cache.put(tile(3), "c");
        assert_eq!(cache.get(&tile(1)), None);
        assert_eq!(cache.get(&tile(2)), Some(&"b"));
    }

    /// `len` counts what is held, not what was put -- which is the whole
    /// reason `MapView`'s `failed` and `absent` tables are this type.
    #[test]
    fn len_reports_entries_held_and_stops_at_the_bound() {
        let mut cache = TileCache::new(2);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        cache.put(tile(1), "a");
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
        cache.put(tile(2), "b");
        cache.put(tile(3), "c");
        assert_eq!(
            cache.len(),
            2,
            "a third put into a cache of two evicts rather than grows"
        );
        cache.remove(&tile(3));
        assert_eq!(cache.len(), 1);
    }
}

#[cfg(test)]
mod byte_budget_tests {
    use super::*;
    use crate::geo::TileId;

    fn tile(x: u32) -> TileId {
        TileId { z: 14, x, y: 7 }
    }

    /// The value type these tests weigh: a tile whose cost is stated
    /// rather than derived, so a test can put a 100 MB tile next to a
    /// 16-byte one without needing either to exist.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct Weighed(usize);

    fn weigh(value: &Weighed) -> usize {
        value.0
    }

    fn budgeted(max_entries: usize, max_bytes: usize) -> TileCache<Weighed> {
        TileCache::with_byte_budget(max_entries, max_bytes, weigh)
    }

    #[test]
    fn a_count_bounded_cache_is_unchanged_and_weighs_nothing() {
        let mut cache: TileCache<Weighed> = TileCache::new(4);
        for x in 0..4 {
            cache.put(tile(x), Weighed(1_000_000_000));
        }
        assert_eq!(cache.len(), 4, "a count bound must not start caring about bytes");
        assert_eq!(cache.held_bytes(), 0);
        assert_eq!(cache.byte_budget(), None);
        assert_eq!(cache.entry_bytes(&tile(0)), Some(0));
    }

    #[test]
    fn the_byte_bound_evicts_before_the_entry_bound_is_reached() {
        // 100 entries permitted, 250 bytes permitted, entries of 100.
        let mut cache = budgeted(100, 250);
        cache.put(tile(1), Weighed(100));
        cache.put(tile(2), Weighed(100));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.held_bytes(), 200);
        cache.put(tile(3), Weighed(100));
        assert_eq!(
            cache.len(),
            2,
            "300 bytes in a 250-byte cache must evict, even at 3 of 100 entries -- this is \
             the whole defect: the old bound could not reach this state at all"
        );
        assert_eq!(cache.held_bytes(), 200);
        assert_eq!(cache.get(&tile(1)), None, "the LRU entry is the one that goes");
        assert_eq!(cache.get(&tile(2)), Some(&Weighed(100)));
        assert_eq!(cache.get(&tile(3)), Some(&Weighed(100)));
    }

    #[test]
    fn every_evicted_or_replaced_value_reaches_the_sink_and_nothing_else_does() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SUNK: AtomicUsize = AtomicUsize::new(0);
        fn sink(_: Weighed) {
            SUNK.fetch_add(1, Ordering::Relaxed);
        }
        let mut cache = budgeted(4, 100).with_evicted_sink(sink);
        for x in 0..4 {
            cache.put(tile(x), Weighed(10));
        }
        assert_eq!(SUNK.load(Ordering::Relaxed), 0, "nothing left yet");
        cache.put(tile(9), Weighed(10)); // entry bound
        assert_eq!(SUNK.load(Ordering::Relaxed), 1);
        cache.put(tile(9), Weighed(10)); // replaced
        assert_eq!(SUNK.load(Ordering::Relaxed), 2);
        cache.put(tile(20), Weighed(95)); // byte budget: every older entry goes
        assert_eq!(SUNK.load(Ordering::Relaxed), 6);
        assert_eq!(cache.len(), 1);
        cache.put_with_bytes_keeping(tile(21), Weighed(95), 95, |_| false);
        assert_eq!(SUNK.load(Ordering::Relaxed), 7, "the keeping put's victim too");
    }

    #[test]
    fn the_entry_bound_still_binds_for_cheap_entries() {
        // The sparse-tile case, and the reason both bounds are kept: a
        // cache of near-free entries must not grow without limit just
        // because its byte budget is nowhere near reached.
        let mut cache = budgeted(8, 1_000_000);
        for x in 0..40 {
            cache.put(tile(x), Weighed(16));
        }
        assert_eq!(cache.len(), 8, "forty 16-byte entries fit the budget; the count is the bound");
        assert_eq!(cache.held_bytes(), 128);
        assert!(cache.held_bytes() < cache.byte_budget().expect("budgeted"));
    }

    #[test]
    fn a_budget_that_holds_eight_dense_entries_holds_hundreds_of_sparse_ones() {
        // Stated as a property because it is the one that says the fix is
        // a fix and not a smaller version of the same mistake: the SAME
        // budget must be worth few dense tiles and many cheap ones. A
        // bound that kept eight cheap entries would be a regression.
        let dense = 66_560_000; // one real dense Tokyo z14 CachedMesh
        let sparse = 16; // one real offshore tile
        let budget = 512_000_000;

        let mut dense_cache = budgeted(512, budget);
        for x in 0..64 {
            dense_cache.put(tile(x), Weighed(dense));
        }
        assert_eq!(dense_cache.len(), 7, "512 MB holds seven 66.56 MB triangulations");

        let mut sparse_cache = budgeted(512, budget);
        for x in 0..512 {
            sparse_cache.put(tile(x), Weighed(sparse));
        }
        assert_eq!(
            sparse_cache.len(),
            512,
            "the same 512 MB budget must hold every one of 512 offshore tiles -- their whole \
             weight is 8 KB, and a byte bound that evicted them would have replaced one wrong \
             unit with another"
        );
    }

    #[test]
    fn one_entry_always_survives_even_over_budget() {
        // A `put` must never be answerable by `None` on the very next
        // `get`: every caller in the crate has just produced the value
        // because something needs it on this frame.
        let mut cache = budgeted(8, 1_000);
        cache.put(tile(1), Weighed(5_000_000));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&tile(1)), Some(&Weighed(5_000_000)));
        assert_eq!(cache.held_bytes(), 5_000_000, "and it is honestly reported as over budget");
        // The next arrival evicts the over-budget entry rather than
        // itself, so the cache converges instead of pinning one giant.
        cache.put(tile(2), Weighed(100));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&tile(1)), None);
        assert_eq!(cache.get(&tile(2)), Some(&Weighed(100)));
        assert_eq!(cache.held_bytes(), 100);
    }

    #[test]
    fn replacing_an_entry_replaces_its_weight_rather_than_adding_to_it() {
        let mut cache = budgeted(8, 1_000);
        cache.put(tile(1), Weighed(400));
        cache.put(tile(1), Weighed(100));
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.held_bytes(),
            100,
            "a rebake that made a tile cheaper must not leave the old weight behind -- that \
             drift is unbounded over a session, and would evict a cache that holds nothing"
        );
        assert_eq!(cache.entry_bytes(&tile(1)), Some(100));
    }

    #[test]
    fn removing_and_clearing_give_the_bytes_back() {
        let mut cache = budgeted(8, 10_000);
        cache.put(tile(1), Weighed(400));
        cache.put(tile(2), Weighed(600));
        assert_eq!(cache.held_bytes(), 1_000);
        assert_eq!(cache.remove(&tile(1)), Some(Weighed(400)));
        assert_eq!(cache.held_bytes(), 600);
        assert_eq!(cache.remove(&tile(1)), None, "removing twice must not credit twice");
        assert_eq!(cache.held_bytes(), 600);
        assert_eq!(cache.clear(), 1);
        assert_eq!(cache.held_bytes(), 0, "an empty cache holds zero bytes by definition");
        // And the next tile in is not evicted by a stale balance.
        cache.put(tile(3), Weighed(9_000));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn reweighing_picks_up_an_in_place_size_change_and_trims() {
        let mut cache = budgeted(8, 1_000);
        cache.put(tile(1), Weighed(300));
        cache.put(tile(2), Weighed(300));
        assert_eq!(cache.held_bytes(), 600);
        // Simulate `MapView::splice_cached_mesh`: an in-place edit that
        // changed how big the value is.
        cache.get_mut(&tile(2)).expect("present").0 = 900;
        assert_eq!(
            cache.held_bytes(),
            600,
            "get_mut alone must NOT re-weigh -- the weight is a snapshot until reweigh"
        );
        cache.reweigh(&tile(2));
        assert_eq!(cache.get(&tile(2)), Some(&Weighed(900)));
        assert_eq!(cache.held_bytes(), 900);
        assert_eq!(cache.len(), 1, "300 + 900 over a 1000-byte budget evicts the LRU entry");
        assert_eq!(cache.entry_bytes(&tile(1)), None);
    }

    #[test]
    fn reweighing_does_not_disturb_recency() {
        let mut cache = budgeted(2, 1_000_000);
        cache.put(tile(1), Weighed(10));
        cache.put(tile(2), Weighed(10));
        cache.reweigh(&tile(1));
        cache.put(tile(3), Weighed(10));
        assert_eq!(
            cache.get(&tile(1)),
            None,
            "re-weighing is bookkeeping, not a read: an entry that got cheaper must not also \
             get a stay of execution"
        );
        assert_eq!(cache.get(&tile(2)), Some(&Weighed(10)));
    }

    #[test]
    fn reweighing_a_missing_key_or_a_count_bounded_cache_does_nothing() {
        let mut budgeted_cache = budgeted(8, 1_000);
        budgeted_cache.reweigh(&tile(9));
        assert_eq!(budgeted_cache.held_bytes(), 0);
        let mut counted: TileCache<Weighed> = TileCache::new(8);
        counted.put(tile(1), Weighed(500));
        counted.reweigh(&tile(1));
        assert_eq!(counted.held_bytes(), 0);
        assert_eq!(counted.get(&tile(1)), Some(&Weighed(500)));
    }

    #[test]
    fn put_with_bytes_is_what_is_enforced_and_put_weighs_for_itself() {
        // The two insert paths must agree about the unit. `install_baked_
        // tile` uses the first (weights computed off-thread) and every
        // test helper uses the second.
        let mut supplied = budgeted(8, 10_000);
        supplied.put_with_bytes(tile(1), Weighed(700), 700);
        let mut computed = budgeted(8, 10_000);
        computed.put(tile(1), Weighed(700));
        assert_eq!(supplied.held_bytes(), computed.held_bytes());
        assert_eq!(supplied.entry_bytes(&tile(1)), computed.entry_bytes(&tile(1)));
    }

    #[test]
    fn iteration_and_lookup_see_through_the_weight() {
        // The weight is stored beside the value, so every read path has
        // to project past it. A missed projection would be a compile
        // error for `get`, and silently wrong for `iter`.
        let mut cache = budgeted(8, 10_000);
        cache.put(tile(1), Weighed(11));
        cache.put(tile(2), Weighed(22));
        let mut seen: Vec<_> = cache.iter().map(|(&id, &v)| (id.x, v.0)).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![(1, 11), (2, 22)]);
        assert_eq!(cache.get_mut(&tile(1)).map(|v| v.0), Some(11));
    }

    #[test]
    fn eviction_drops_the_value_rather_than_forgetting_it() {
        use std::sync::Arc;
        let witness = Arc::new(());
        let mut cache: TileCache<Arc<()>> =
            TileCache::with_byte_budget(8, 100, |_| 60);
        cache.put(tile(1), Arc::clone(&witness));
        assert_eq!(Arc::strong_count(&witness), 2);
        cache.put(tile(2), Arc::new(()));
        assert_eq!(
            Arc::strong_count(&witness),
            1,
            "a byte budget that only forgot its evictions would report bytes it had not freed, \
             which is the exact failure it exists to prevent"
        );
        assert_eq!(cache.held_bytes(), 60);
    }
}

#[cfg(test)]
mod clearing_tests {
    use super::*;
    use crate::geo::TileId;

    fn tile(x: u32) -> TileId {
        TileId { z: 14, x, y: 1 }
    }

    #[test]
    fn len_counts_what_is_held_not_what_could_be() {
        let mut cache: TileCache<u32> = TileCache::new(8);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        cache.put(tile(1), 1);
        cache.put(tile(2), 2);
        assert_eq!(cache.len(), 2, "two of eight slots used is a length of two, not eight");
    }

    #[test]
    fn capacity_reports_the_bound_and_does_not_move_with_the_occupancy() {
        let mut cache: TileCache<u32> = TileCache::new(8);
        assert_eq!(cache.capacity(), 8, "an empty cache still has its bound");
        for x in 0..12 {
            cache.put(tile(x), x);
        }
        assert_eq!(cache.len(), 8, "twelve puts into eight slots evict down to eight");
        assert_eq!(
            cache.capacity(),
            8,
            "the bound is what `len` may grow to, so filling and overfilling must not \
             move it -- an occupancy figure whose denominator drifts says nothing"
        );
    }

    #[test]
    fn clearing_reports_how_many_went_and_leaves_nothing_behind() {
        let mut cache: TileCache<u32> = TileCache::new(8);
        for x in 0..5 {
            cache.put(tile(x), x);
        }
        assert_eq!(cache.clear(), 5);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert!(!cache.contains(&tile(0)));
        assert_eq!(cache.clear(), 0, "clearing an empty cache drops nothing");
    }

    #[test]
    fn clearing_actually_drops_the_values() {
        use std::sync::Arc;
        let witness = Arc::new(());
        let mut cache: TileCache<Arc<()>> = TileCache::new(8);
        cache.put(tile(1), Arc::clone(&witness));
        assert_eq!(Arc::strong_count(&witness), 2);
        cache.clear();
        assert_eq!(
            Arc::strong_count(&witness),
            1,
            "a measurement of what a cache holds is worthless if clearing it only forgot \
             the values instead of freeing them"
        );
    }
}
