//! Label collision avoidance: a uniform grid over a padded viewport, plus
//! the priority-ordered greedy placement pass that fills it.
//!
//! This is a pure geometric unit -- no GPU, no gpui, no style types. It
//! knows nothing about labels beyond an axis-aligned box and an ordering
//! key, which is exactly what makes it testable without a window.
//!
//! Modelled on MapLibre GL JS: `src/symbol/grid_index.ts` +
//! `src/symbol/collision_index.ts` for the geometry, and
//! `src/style/pauseable_placement.ts` for the order symbols are offered in
//! (see `place_symbols`). The deliberate divergences from those are called
//! out at the declarations they affect (`symbol_box`'s text/icon union, and
//! the edge-touch rule on `CollisionBox::intersects`).
//!
//! Explicitly out of scope, and left for follow-up work: fade/opacity
//! transitions when a label enters or leaves placement (MapLibre's
//! `JointOpacityState` ramp -- the intended follow-up for whatever
//! residual pop-in survives the padded viewport), placement state carried
//! across frames, variable label anchors, Organic Maps' displacement
//! arrows, and line-placement labels.

/// The uniform grid's cell edge, in PHYSICAL pixels.
///
/// The grid exists so a candidate box is only ever compared box-vs-box
/// against boxes that share at least one cell with it -- MapLibre's stated
/// principle in `grid_index.ts`. The cell size is therefore a pure
/// time/space tradeoff and not a correctness knob: too small and a wide
/// label registers itself into dozens of cells, too large and every
/// candidate compares against most of the screen.
///
/// 64 physical pixels is roughly the height of two label lines on a 2x
/// display, so a typical label spans one or two cells vertically and a
/// handful horizontally.
pub const COLLISION_GRID_CELL_PX: f32 = 64.0;

/// An axis-aligned bounding box in PHYSICAL pixels -- the same space pass 1
/// of the symbol loop already computes `TextArea` positions and icon quads
/// in, so no conversion happens anywhere in this module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionBox {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl CollisionBox {
    /// A box from an origin and a size, which is the shape both callers
    /// have: a `TextArea` origin plus its measured width and line height,
    /// and an icon quad's top-left plus its physical size.
    pub fn from_origin_size(left: f32, top: f32, width: f32, height: f32) -> Self {
        CollisionBox { left, top, right: left + width, bottom: top + height }
    }

    /// The smallest box containing both.
    pub fn union(self, other: Self) -> Self {
        CollisionBox {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Grown by `padding` on every side.
    pub fn padded(self, padding: f32) -> Self {
        CollisionBox {
            left: self.left - padding,
            top: self.top - padding,
            right: self.right + padding,
            bottom: self.bottom + padding,
        }
    }

    /// Whether the two boxes overlap.
    ///
    /// The rule chosen here is STRICT: boxes that merely touch edge-to-edge
    /// (one's `right` exactly equal to the other's `left`) do NOT collide.
    /// Two reasons. Labels are already separated by
    /// `SYMBOL_COLLISION_PADDING_LOGICAL_PX` of padding before they get
    /// here, so an exact edge touch means the padded gap is already
    /// satisfied and rejecting would waste a label for nothing. And
    /// `text_area_origin` snaps every label origin to a whole physical
    /// pixel, which makes exact-equality edges a genuinely reachable case
    /// rather than a float-comparison curiosity.
    pub fn intersects(&self, other: &Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    fn is_finite(&self) -> bool {
        self.left.is_finite() && self.top.is_finite() && self.right.is_finite() && self.bottom.is_finite()
    }
}

/// The union of a symbol's text box and its icon box, padded -- or `None`
/// when the symbol has neither.
///
/// **Deliberate divergence from MapLibre/Mapbox.** Both keep the text box
/// and the icon box as SEPARATE collision features and will happily place
/// one without the other (MapLibre's `placeCollisionBox` is called once per
/// feature, and `text-optional`/`icon-optional` exist precisely to control
/// what happens when only one fits). Taking the union instead means a city
/// dot can never end up on screen orphaned from the name it belongs to, at
/// the cost of losing a few symbols that would have fit with only one half
/// drawn. This is a choice, not an oversight.
pub fn symbol_box(text: Option<CollisionBox>, icon: Option<CollisionBox>, padding: f32) -> Option<CollisionBox> {
    let combined = match (text, icon) {
        (Some(t), Some(i)) => t.union(i),
        (Some(t), None) => t,
        (None, Some(i)) => i,
        (None, None) => return None,
    };
    Some(combined.padded(padding))
}

/// A uniform grid of already-placed boxes over a fixed region of screen
/// space.
///
/// Mirrors MapLibre's `GridIndex`: a box is registered into EVERY cell it
/// overlaps, and a hit test only ever compares full box-vs-box against the
/// boxes registered in the cells the query box itself overlaps.
///
/// The region is the viewport expanded by a margin on all four sides (see
/// `CollisionIndex::new`), never the bare viewport.
#[derive(Debug)]
pub struct CollisionIndex {
    bounds: CollisionBox,
    cell_size: f32,
    columns: usize,
    rows: usize,
    /// CSR-style bucket storage, replacing a `Vec<Vec<u32>>`.
    ///
    /// `cell_starts` has `columns * rows + 1` entries and `cell_items` holds
    /// every cell's slots back to back, so cell `c` owns
    /// `cell_items[cell_starts[c]..cell_starts[c + 1]]`. One allocation for
    /// the whole grid instead of one per cell, and a cell's items are
    /// contiguous rather than a pointer away.
    ///
    /// A true CSR array cannot be appended to in the middle, and insertion
    /// here is incremental (greedy placement inserts as it goes), so this is
    /// the bucketed variant: the span above is cell `c`'s CAPACITY, and
    /// `cell_fill[c]` says how much of it is live. The live items are
    /// `cell_items[cell_starts[c]..cell_starts[c] + cell_fill[c]]`; the rest
    /// of the span is stale bytes that are never read. See `insert`.
    cell_starts: Vec<u32>,
    cell_fill: Vec<u32>,
    cell_items: Vec<u32>,
    /// The placed boxes in SoA form: `lefts[i]`, `tops[i]`, `rights[i]`,
    /// `bottoms[i]` are one box. An overlap test over a cell's items then
    /// reads four contiguous runs instead of striding a 16-byte struct,
    /// which is what lets the compiler vectorise it. `lefts.len()` is the
    /// old `boxes.len()`: the count of everything placed.
    lefts: Vec<f32>,
    tops: Vec<f32>,
    rights: Vec<f32>,
    bottoms: Vec<f32>,
    /// How many of the SoA entries are OBSTACLES seeded this frame
    /// (`insert_obstacle`) rather than placed symbols. Obstacles are
    /// something labels avoid -- the navigation route line -- and they go in
    /// after `reset` and before placement, so they are not "placements from
    /// the previous frame". `placement_count` subtracts them.
    obstacles: usize,
}

/// Slots every cell starts life with, and the floor every doubling grows
/// from.
///
/// A whole grid's worth of this is one allocation of
/// `columns * rows * 4` u32 -- ~24 KB for a 2x 1600x1000 viewport at the
/// default 64px cell -- which buys the common cell never growing at all.
/// Labels cluster, so most cells hold nothing and a few hold many; the
/// doubling path below is what covers the few.
const INITIAL_CELL_CAPACITY: usize = 4;

impl CollisionIndex {
    /// An empty index covering exactly `bounds` (which the caller is
    /// expected to have already padded -- see
    /// `map_view::symbol_collision_bounds`).
    pub fn new(bounds: CollisionBox) -> Self {
        Self::with_cell_size(bounds, COLLISION_GRID_CELL_PX)
    }

    /// `new` with an explicit cell size, so tests can exercise
    /// multi-cell spanning without building screen-sized boxes.
    pub fn with_cell_size(bounds: CollisionBox, cell_size: f32) -> Self {
        let cell_size = if cell_size.is_finite() && cell_size > 0.0 { cell_size } else { COLLISION_GRID_CELL_PX };
        let (columns, rows) = Self::grid_dims(&bounds, cell_size);
        let cells = columns * rows;
        CollisionIndex {
            bounds,
            cell_size,
            columns,
            rows,
            cell_starts: (0..=cells).map(|cell| (cell * INITIAL_CELL_CAPACITY) as u32).collect(),
            cell_fill: vec![0; cells],
            cell_items: vec![0; cells * INITIAL_CELL_CAPACITY],
            lefts: Vec::new(),
            tops: Vec::new(),
            rights: Vec::new(),
            bottoms: Vec::new(),
            obstacles: 0,
        }
    }

    /// Every cell's capacity, which this index keeps UNIFORM.
    ///
    /// Uniform because the alternative -- growing one hot cell at a time --
    /// has to shift every later cell's span along on each growth, and
    /// placement overflows cells one at a time, hundreds of times a frame.
    /// That is O(grid) work per overflow. Doubling every cell at once
    /// instead makes the number of rebuilds `log2(fullest cell)`, about five
    /// for a dense frame, at the cost of some slack in the cells that never
    /// fill. `cell_starts` is still a real array rather than implied
    /// arithmetic, so the read path in `collides` is a plain CSR slice and a
    /// future non-uniform policy stays a change to this file's private half.
    fn cell_capacity(&self) -> usize {
        // `cell_starts` always has at least two entries, because `grid_dims`
        // floors both dimensions at 1.
        (self.cell_starts[1] - self.cell_starts[0]) as usize
    }

    /// Doubles every cell's capacity in place, keeping each cell's live
    /// prefix.
    ///
    /// Walks the cells BACKWARDS: every cell's new span starts at or after
    /// its old one, so moving the last cell first means a copy never lands
    /// on a cell that has not been moved yet.
    fn grow_cells(&mut self) {
        let cells = self.columns * self.rows;
        let new_capacity = (self.cell_capacity() * 2).max(INITIAL_CELL_CAPACITY);
        self.cell_items.resize(cells * new_capacity, 0);
        for cell in (0..cells).rev() {
            let fill = self.cell_fill[cell] as usize;
            let source = self.cell_starts[cell] as usize;
            self.cell_items.copy_within(source..source + fill, cell * new_capacity);
            self.cell_starts[cell] = (cell * new_capacity) as u32;
        }
        self.cell_starts[cells] = (cells * new_capacity) as u32;
    }

    /// Appends `item` to one cell's live prefix, growing the whole grid
    /// first if that cell is full.
    fn push_into_cell(&mut self, cell: usize, item: u32) {
        if self.cell_fill[cell] as usize == self.cell_capacity() {
            self.grow_cells();
        }
        let slot = self.cell_starts[cell] as usize + self.cell_fill[cell] as usize;
        self.cell_items[slot] = item;
        self.cell_fill[cell] += 1;
    }

    /// Cell `cell`'s live items.
    #[inline]
    fn cell_slice(&self, cell: usize) -> &[u32] {
        let start = self.cell_starts[cell] as usize;
        &self.cell_items[start..start + self.cell_fill[cell] as usize]
    }

    /// Whether any of `items` -- indices into the SoA arrays -- overlaps the
    /// box `(l, t, r, b)`.
    ///
    /// The four inequalities are `CollisionBox::intersects` transcribed with
    /// `self` as the QUERY box and the indexed box as `other`, strictness
    /// included: an exact shared edge is not an overlap, which is the
    /// documented rule there and is reachable because label origins are
    /// snapped to whole physical pixels.
    #[inline]
    fn any_overlap(&self, items: &[u32], l: f32, t: f32, r: f32, b: f32) -> bool {
        items.iter().any(|&i| {
            let i = i as usize;
            l < self.rights[i] && self.lefts[i] < r && t < self.bottoms[i] && self.tops[i] < b
        })
    }

    /// The `(columns, rows)` a grid over `bounds` needs at `cell_size`,
    /// each at least 1 -- shared by `with_cell_size` (which allocates a
    /// fresh grid at this size) and `reset` (which only allocates when the
    /// size actually changed).
    fn grid_dims(bounds: &CollisionBox, cell_size: f32) -> (usize, usize) {
        let width = (bounds.right - bounds.left).max(0.0);
        let height = (bounds.bottom - bounds.top).max(0.0);
        let columns = ((width / cell_size).ceil() as usize).max(1);
        let rows = ((height / cell_size).ceil() as usize).max(1);
        (columns, rows)
    }

    /// Reuses this index for a new frame: every previously placed box is
    /// forgotten, but the bucket array's allocated capacity is KEPT --
    /// including whatever per-cell capacity the last frame's doubling
    /// settled on -- so a frame that places roughly the same number of
    /// labels per cell as the last one allocates nothing here at all.
    ///
    /// The grid itself (column/row count) is resized only when `bounds`
    /// implies a different one than what this index already has -- a
    /// window resize or a display-scale change, not every frame. Getting
    /// this comparison wrong in the "resize too often" direction is cheap;
    /// getting it wrong the other way would silently keep stale grid
    /// dimensions after a real viewport change and misplace every label
    /// against the wrong cell, so the comparison is on the derived
    /// `(columns, rows)`, not on `bounds` floats directly (which could
    /// differ by a sub-pixel amount, e.g. camera jitter, without changing
    /// the grid at all).
    ///
    /// See `GpuState::collision_index` (`map_view.rs`) for the production
    /// caller -- one retained index per `GpuState`, reset here once per
    /// frame instead of rebuilt with `new`.
    pub fn reset(&mut self, bounds: CollisionBox) {
        let (columns, rows) = Self::grid_dims(&bounds, self.cell_size);
        if columns != self.columns || rows != self.rows {
            // The per-cell capacity the last frame grew to is worth keeping
            // across a resize: the window changed shape, not the label
            // density. Read it BEFORE `cell_starts` is rewritten.
            let capacity = self.cell_capacity();
            self.columns = columns;
            self.rows = rows;
            let cells = columns * rows;
            // `clear` + `extend` and `resize` rather than fresh `vec![...]`s:
            // every one of these keeps its existing allocation when the new
            // grid is no larger than the old (a shrink allocates nothing at
            // all). The old contents are invalidated by the dimension change
            // regardless, so only the zeroing of `cell_fill` below matters
            // for correctness -- `cell_items`' stale slots are never read
            // past a cell's fill.
            self.cell_starts.clear();
            self.cell_starts.extend((0..=cells).map(|cell| (cell * capacity) as u32));
            self.cell_fill.clear();
            self.cell_fill.resize(cells, 0);
            self.cell_items.resize(cells * capacity, 0);
        } else {
            self.cell_fill.fill(0);
        }
        self.lefts.clear();
        self.tops.clear();
        self.rights.clear();
        self.bottoms.clear();
        self.obstacles = 0;
        self.bounds = bounds;
    }

    /// Whether nothing has been placed into this index since it was built
    /// or last `reset`.
    ///
    /// Exists for callers that document "the index handed to me must be
    /// clear" as a precondition and want to assert it rather than trust it
    /// -- see `map_view::place_visible_symbols`. The SoA arrays are the
    /// whole record of what has been placed (`insert` pushes there and into
    /// the cells together, `reset` clears both), so this is exact as well as
    /// O(1): a stale index cannot read as empty.
    pub fn is_empty(&self) -> bool {
        self.lefts.is_empty()
    }

    /// How many symbol boxes have been placed (`insert`/`place`) since this
    /// index was built or last `reset` -- everything in it except the
    /// obstacles `insert_obstacle` seeded.
    ///
    /// This, not `is_empty`, is the "no placements from a previous frame"
    /// precondition `symbol_pass::place_visible_symbols` asserts: a frame
    /// that seeds the route line as an obstacle hands placement an index
    /// that is not empty and is nonetheless exactly as clean as it should
    /// be.
    pub fn placement_count(&self) -> usize {
        self.lefts.len() - self.obstacles
    }

    /// Seeds a box that labels must avoid but that is not itself a placed
    /// label -- the navigation route's screen path
    /// (`NavHost::collision_boxes`). Call after `reset` and before
    /// placement. It collides exactly as a placed box does; the only
    /// difference is that it does not count towards `placement_count`.
    pub fn insert_obstacle(&mut self, obstacle: CollisionBox) {
        let before = self.lefts.len();
        self.insert(obstacle);
        // `insert` drops a non-finite box; count only what went in.
        self.obstacles += self.lefts.len() - before;
    }

    /// Whether `candidate` overlaps anything already placed.
    pub fn collides(&self, candidate: &CollisionBox) -> bool {
        // A non-finite box has no meaningful cell range (and `f32::clamp`
        // would propagate the NaN into the index arithmetic), so treat it
        // as colliding: dropping one malformed label beats corrupting the
        // whole frame's placement.
        if !candidate.is_finite() {
            return true;
        }
        let (col_start, col_end, row_start, row_end) = self.cell_range(candidate);
        // Same traversal as the `Vec<Vec<u32>>` version had -- row-major
        // over the same `cell_range`, ascending insertion index within a
        // cell. `intersects` is symmetric so any overlap would be found
        // either way and nothing downstream depends on WHICH box answers,
        // but keeping the span identical is what keeps greedy placement's
        // order-dependent outcome identical too.
        for row in row_start..=row_end {
            let row_base = row * self.columns;
            for column in col_start..=col_end {
                let items = self.cell_slice(row_base + column);
                if self.any_overlap(items, candidate.left, candidate.top, candidate.right, candidate.bottom) {
                    return true;
                }
            }
        }
        false
    }

    /// Registers `candidate` into every cell it overlaps. Does NOT hit-test
    /// first -- `place` is the combined operation callers want.
    pub fn insert(&mut self, candidate: CollisionBox) {
        if !candidate.is_finite() {
            return;
        }
        let (col_start, col_end, row_start, row_end) = self.cell_range(&candidate);
        let index = self.lefts.len() as u32;
        self.lefts.push(candidate.left);
        self.tops.push(candidate.top);
        self.rights.push(candidate.right);
        self.bottoms.push(candidate.bottom);
        for row in row_start..=row_end {
            let row_base = row * self.columns;
            for column in col_start..=col_end {
                self.push_into_cell(row_base + column, index);
            }
        }
    }

    /// Hit-test, and on a miss insert. Returns whether the symbol was
    /// placed.
    pub fn place(&mut self, candidate: CollisionBox) -> bool {
        if self.collides(&candidate) {
            return false;
        }
        self.insert(candidate);
        true
    }

    /// The inclusive `(col_start, col_end, row_start, row_end)` cell span of
    /// `candidate`, clamped to the grid.
    ///
    /// Clamping rather than rejecting is deliberate: a label anchored just
    /// inside the padded viewport can still extend past its edge, and such a
    /// label must still collide with its neighbours. Clamping folds the
    /// overhanging part into the edge cells, which over-approximates the
    /// occupied area very slightly at the border and never under-reports a
    /// collision.
    fn cell_range(&self, candidate: &CollisionBox) -> (usize, usize, usize, usize) {
        let column_of = |x: f32| {
            let raw = ((x - self.bounds.left) / self.cell_size).floor();
            (raw.max(0.0) as usize).min(self.columns - 1)
        };
        let row_of = |y: f32| {
            let raw = ((y - self.bounds.top) / self.cell_size).floor();
            (raw.max(0.0) as usize).min(self.rows - 1)
        };
        (column_of(candidate.left), column_of(candidate.right), row_of(candidate.top), row_of(candidate.bottom))
    }
}

/// One symbol offered to the placement pass.
///
/// The priority is `(layer_index, sort_key)` and NOT glyph size: MapLibre,
/// Mapbox and Organic Maps all use an explicit priority, never the rendered
/// size of the label.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacementCandidate {
    /// Index of the symbol's layer in the compiled `Style` -- the PRIMARY
    /// key, and a LARGER one places first (see `place_symbols`).
    ///
    /// Our compiled `Style` is in style-document order, and in the Mapbox
    /// style spec a later layer draws ON TOP of an earlier one. So the last
    /// layer is the one the author put in front, and it gets first pick of
    /// collision slots.
    pub layer_index: usize,
    /// The layer's `symbol-sort-key` resolved against this feature, or
    /// `0.0` when the layer declares none. Mapbox semantics: LOWER places
    /// first. `settlement-major-label` sets `["get", "symbolrank"]`, where a
    /// lower rank is a more important city.
    pub sort_key: f32,
    pub bounds: CollisionBox,
}

/// Greedily places `candidates` into `index`, highest priority first.
///
/// Priority is `layer_index` DESCENDING, then `sort_key` ASCENDING. The two
/// directions really are different, and both are MapLibre's.
///
/// The layer direction is `pauseable_placement.ts` in
/// `@maplibre/maplibre-gl-js` 5.6.0, which walks the style's layer order
/// BACKWARDS: `this._currentPlacementIndex = order.length - 1` (:80),
/// `while (this._currentPlacementIndex >= 0)` (:101),
/// `this._currentPlacementIndex--` (:125). The last layer in the document
/// is the one drawn on top, so it is the one that gets first pick of the
/// screen.
///
/// The sort-key direction is the style spec's own sentence about
/// `symbol-sort-key`: "Features with lower sort keys are drawn and placed
/// first." That is a WITHIN-layer rule and it is untouched by the above --
/// only the layer dimension runs backwards.
///
/// Returns one `bool` per candidate, IN THE CALLER'S ORIGINAL ORDER, so a
/// caller holding several parallel lists (text areas and icon quads, say)
/// can filter all of them against the same answer and never orphan one from
/// the other.
///
/// Ties on `(layer_index, sort_key)` fall back to the candidate's position
/// in `candidates`, which makes placement deterministic frame to frame
/// rather than dependent on hash iteration order somewhere upstream.
pub fn place_symbols(index: &mut CollisionIndex, candidates: &[PlacementCandidate]) -> Vec<bool> {
    place_symbols_vetoed(index, candidates, &mut ())
}

/// A second opinion, asked of each candidate in PLACEMENT ORDER, about
/// something a `CollisionBox` cannot express.
///
/// `symbol_pass::CrossLayerTextVeto` is the one implementor: it holds each
/// candidate's text and anchor and drops a label that merely repeats one
/// another layer already placed at the same place (a city-state carried as
/// both a country and a capital).
///
/// A hook rather than a second greedy loop beside this one because the
/// priority order is the point: a veto that saw candidates in any other
/// order would keep the wrong one of a pair. There is one walk, in
/// [`place_symbols_vetoed`], and callers that need no veto pass `()`.
pub trait PlacementVeto {
    /// Asked before `candidate` is offered a slot. `true` drops it: it takes
    /// no slot and [`Self::accepted`] is not called for it.
    fn rejects(&mut self, candidate: usize) -> bool;

    /// Called for each candidate that PLACED -- passed the veto and fitted
    /// in the index. Only those, so a veto accumulates state over what is
    /// actually on screen (MapLibre's `placement.ts:472-474` makes the same
    /// distinction for its cross-tile duplicates).
    fn accepted(&mut self, candidate: usize);
}

/// The veto that vetoes nothing, which [`place_symbols`] passes.
impl PlacementVeto for () {
    fn rejects(&mut self, _candidate: usize) -> bool {
        false
    }

    fn accepted(&mut self, _candidate: usize) {}
}

/// [`place_symbols`] with a [`PlacementVeto`] consulted in placement order.
/// Every word of `place_symbols`' doc comment applies.
pub fn place_symbols_vetoed(
    index: &mut CollisionIndex,
    candidates: &[PlacementCandidate],
    veto: &mut dyn PlacementVeto,
) -> Vec<bool> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    // `sort_by` is stable, so equal `(layer_index, sort_key)` pairs keep
    // their original relative order -- the documented final tiebreak, with
    // no explicit sequence field needed.
    order.sort_by(|&a, &b| {
        candidates[b]
            .layer_index
            // REVERSED operands, not `.reverse()` on the whole comparison:
            // only this half runs backwards. Chaining `.reverse()` after the
            // `then_with` would flip the sort key too, which is the one
            // mistake this comparator is shaped to make hard.
            .cmp(&candidates[a].layer_index)
            // `total_cmp` rather than `partial_cmp`: a sort key read from
            // tile data can be NaN, and a comparator that returns
            // `Ordering::Equal` for NaN is not a total order, which
            // `sort_by` is entitled to treat as a bug.
            .then_with(|| candidates[a].sort_key.total_cmp(&candidates[b].sort_key))
    });
    let mut placed = vec![false; candidates.len()];
    for candidate_index in order {
        if veto.rejects(candidate_index) {
            continue;
        }
        placed[candidate_index] = index.place(candidates[candidate_index].bounds);
        if placed[candidate_index] {
            veto.accepted(candidate_index);
        }
    }
    placed
}

/// A tiny counting allocator, alive ONLY inside `cargo test`'s test binary.
///
/// Gated behind `#[cfg(test)]` on the module itself, so `#[global_allocator]`
/// below has zero effect on the shipped `gpui-maps` rlib, on any
/// `examples/*.rs` binary, or on a plain `cargo build` -- the same reasoning
/// `benches/perf.rs` documents for its own bench-local `GlobalAlloc` impl
/// (`cfg(test)` there is really "this is `benches/perf.rs`'s own crate",
/// here it is really "this is the `cargo test --lib` binary", but the
/// safety argument is identical: the attribute only takes effect in the one
/// process that compiles this module in). This one is smaller than the
/// bench's: Task 3's claim is "did the retained index allocate fewer
/// TIMES", not "how many bytes churned", so it tracks a single running
/// count and nothing else.
///
/// The count is per-THREAD. `benches/perf.rs` can afford a process-global
/// atomic because a bench harness runs its measured window alone; a test
/// binary does not -- `cargo test` runs the suite across as many threads as
/// the machine has, so a global counter diffed across a window would
/// include every allocation every OTHER test made during it. Against the
/// ~5-allocation margin the reset test asserts on, that is not noise it can
/// survive on purpose, only by luck. A thread-local counter is what makes
/// the delta the caller's own work rather than the suite's, and it is a
/// stronger guarantee than serialising the readers with a mutex would be:
/// a lock can only order tests that agree to take it, while the allocator
/// hook fires for every thread in the process whether it took one or not.
#[cfg(test)]
pub(crate) mod alloc_counting {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;

    thread_local! {
        /// `const`-initialised, and holding a type with no `Drop`, both
        /// deliberately: a lazily-initialised or destructor-registering
        /// `thread_local!` ALLOCATES on first touch, and this slot is
        /// touched from inside `GlobalAlloc::alloc` -- which would recurse
        /// into itself. This form has no init path and no destructor to
        /// register, so reading it cannot allocate.
        static ALLOC_COUNT: Cell<u64> = const { Cell::new(0) };
        /// Bytes this thread allocated minus the bytes it freed, and the
        /// most that ever reached since [`reset_peak`] (the census decoder's
        /// memory-bound test). Same `const`, no-`Drop` form as the count.
        static LIVE_BYTES: Cell<i64> = const { Cell::new(0) };
        static PEAK_BYTES: Cell<i64> = const { Cell::new(0) };
    }

    /// `delta` bytes more (or fewer) live on the calling thread.
    fn track(delta: i64) {
        let _ = LIVE_BYTES.try_with(|live| {
            let now = live.get().wrapping_add(delta);
            live.set(now);
            let _ = PEAK_BYTES.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }

    /// One allocation event on the calling thread.
    ///
    /// `try_with`, never `with`: the allocator is also live during thread
    /// teardown, when this slot can already be gone, and `with`'s panic
    /// there would unwind out of `GlobalAlloc::alloc` and abort the
    /// process. Dropping such an increment is harmless -- no test's
    /// measured window is open on a thread that is being destroyed.
    fn bump() {
        let _ = ALLOC_COUNT.try_with(|count| count.set(count.get().wrapping_add(1)));
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump();
            track(layout.size() as i64);
            System.alloc(layout)
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            track(-(layout.size() as i64));
            System.dealloc(ptr, layout)
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            bump();
            track(layout.size() as i64);
            System.alloc_zeroed(layout)
        }

        // Counted as an allocation event, same as `benches/perf.rs`: a
        // `Vec` growth path is exactly the churn this test exists to
        // catch, and it must not hide behind an opaque "realloc" bucket
        // that neither counter sees.
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            bump();
            // Counted as the new block arriving before the old one leaves:
            // the copy a moving realloc makes is part of the peak.
            track(new_size as i64);
            track(-(layout.size() as i64));
            System.realloc(ptr, layout, new_size)
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    /// The running allocation count for THIS THREAD so far.
    ///
    /// Callers diff two snapshots taken close together -- the same
    /// discipline `benches/perf.rs` uses -- and because the counter is
    /// per-thread and a `#[test]` body runs on one thread, the delta really
    /// is that window's own alloc traffic and not the rest of the suite's,
    /// however many tests happen to be running beside it.
    pub(crate) fn count() -> u64 {
        ALLOC_COUNT.try_with(Cell::get).unwrap_or(0)
    }

    /// Starts a peak window on this thread: the peak becomes the bytes
    /// live now, which the return value is.
    pub(crate) fn reset_peak() -> i64 {
        let live = LIVE_BYTES.try_with(Cell::get).unwrap_or(0);
        let _ = PEAK_BYTES.try_with(|peak| peak.set(live));
        live
    }

    /// The most bytes live on this thread since [`reset_peak`].
    pub(crate) fn peak_bytes() -> i64 {
        PEAK_BYTES.try_with(Cell::get).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comfortably large padded-viewport stand-in. Big enough that no test
    /// box below is anywhere near the clamped border, so tests exercise the
    /// geometry rather than the edge behaviour (which has its own test).
    fn test_index() -> CollisionIndex {
        CollisionIndex::new(CollisionBox { left: -100.0, top: -100.0, right: 1000.0, bottom: 800.0 })
    }

    fn candidate(layer_index: usize, sort_key: f32, bounds: CollisionBox) -> PlacementCandidate {
        PlacementCandidate { layer_index, sort_key, bounds }
    }

    #[test]
    fn two_boxes_that_overlap_cannot_both_be_placed() {
        let mut index = test_index();
        let first = CollisionBox { left: 10.0, top: 10.0, right: 110.0, bottom: 40.0 };
        // Overlaps `first` by 60px horizontally and 20px vertically.
        let second = CollisionBox { left: 50.0, top: 20.0, right: 150.0, bottom: 50.0 };
        assert!(index.place(first), "the first box into an empty index always places");
        assert!(!index.place(second), "an overlapping box must be rejected");
    }

    /// Obstacles collide like placements but are not counted as them, and
    /// `reset` forgets both.
    #[test]
    fn an_obstacle_blocks_placement_without_counting_as_one_and_reset_clears_both() {
        let mut index = test_index();
        index.insert_obstacle(CollisionBox { left: 10.0, top: 10.0, right: 110.0, bottom: 40.0 });
        index.insert_obstacle(CollisionBox { left: f32::NAN, top: 0.0, right: 1.0, bottom: 1.0 });
        assert!(!index.is_empty());
        assert_eq!(index.placement_count(), 0, "an obstacle is not a placement; a NaN one is not anything");
        assert!(!index.place(CollisionBox { left: 50.0, top: 20.0, right: 150.0, bottom: 50.0 }), "it blocks");
        assert!(index.place(CollisionBox { left: 300.0, top: 300.0, right: 400.0, bottom: 330.0 }));
        assert_eq!(index.placement_count(), 1);
        index.reset(CollisionBox { left: 0.0, top: 0.0, right: 512.0, bottom: 512.0 });
        assert!(index.is_empty());
        assert_eq!(index.placement_count(), 0);
    }

    #[test]
    fn a_box_clear_of_everything_placed_so_far_is_accepted() {
        let mut index = test_index();
        assert!(index.place(CollisionBox { left: 10.0, top: 10.0, right: 110.0, bottom: 40.0 }));
        // Well clear on both axes, and in a different grid cell.
        assert!(index.place(CollisionBox { left: 300.0, top: 300.0, right: 400.0, bottom: 330.0 }));
    }

    #[test]
    fn boxes_that_touch_edge_to_edge_do_not_collide_because_the_overlap_test_is_strict() {
        // The documented rule (see `CollisionBox::intersects`): an exact
        // shared edge is NOT an overlap. This case is reachable rather than
        // theoretical because `text_area_origin` snaps every label origin to
        // a whole physical pixel.
        let mut index = test_index();
        assert!(index.place(CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }));
        // `left` exactly equals the placed box's `right`.
        assert!(index.place(CollisionBox { left: 100.0, top: 0.0, right: 200.0, bottom: 20.0 }));
        // And the same on the vertical axis.
        assert!(index.place(CollisionBox { left: 0.0, top: 20.0, right: 100.0, bottom: 40.0 }));
    }

    #[test]
    fn padding_widens_the_effective_box_enough_to_reject_a_neighbour_the_bare_boxes_would_clear() {
        // Two boxes with a 2px gap between them: unpadded they are clear of
        // each other, but 2px of padding on EACH closes the gap and makes
        // them overlap by 2px. This is what `text-padding` buys -- labels
        // that are technically non-overlapping but too close to read as two.
        let bare_first = CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 };
        let bare_second = CollisionBox { left: 102.0, top: 0.0, right: 200.0, bottom: 20.0 };
        assert!(!bare_first.intersects(&bare_second), "the unpadded boxes are genuinely clear of each other");

        let mut unpadded = test_index();
        assert!(unpadded.place(bare_first));
        assert!(unpadded.place(bare_second), "without padding both fit");

        let mut padded = test_index();
        assert!(padded.place(bare_first.padded(2.0)));
        assert!(!padded.place(bare_second.padded(2.0)), "2px of padding on each side closes the 2px gap");
    }

    #[test]
    fn a_higher_priority_symbol_wins_even_when_it_is_offered_second() {
        // Constructed so that first-come-first-served gives the WRONG
        // answer: the low-priority symbol is offered first. Only a pass that
        // sorts by priority before placing gets this right.
        let mut index = test_index();
        let overlapping = CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 };
        let candidates = [
            candidate(1, 0.0, overlapping),                                                    // low priority, offered FIRST
            candidate(9, 0.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }), // high priority, offered second
        ];
        let placed = place_symbols(&mut index, &candidates);
        assert_eq!(
            placed,
            vec![false, true],
            "the later style layer (index 9) must win over the earlier one (index 1) \
             regardless of the order the candidates were offered in"
        );
    }

    #[test]
    fn the_later_style_layer_wins_a_contested_position_because_placement_runs_backwards() {
        // The direct statement of MapLibre's rule, with nothing else moving:
        // same sort key, boxes that overlap, and the only difference is the
        // layer index. `pauseable_placement.ts` walks the layer order from
        // `order.length - 1` down to 0 (:80/:101/:125), so index 20 -- drawn
        // on top of index 10 -- also places ahead of it.
        let mut index = test_index();
        let candidates = [
            candidate(10, 0.0, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }),
            candidate(20, 0.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }),
        ];
        assert!(
            candidates[0].bounds.intersects(&candidates[1].bounds),
            "precondition: the two candidates must actually contest one position"
        );
        assert_eq!(
            place_symbols(&mut index, &candidates),
            vec![false, true],
            "layer 20 places first and layer 10 is dropped"
        );
    }

    #[test]
    fn flipping_the_layer_direction_did_not_also_flip_the_sort_key_direction() {
        // The one thing easiest to get wrong alongside the layer flip. Both
        // candidates sit in the SAME layer, so only `symbol-sort-key`
        // decides, and the spec's rule there is unchanged: "features with
        // lower sort keys are drawn and placed first". 1.0 beats 2.0.
        let mut index = test_index();
        let candidates = [
            candidate(7, 2.0, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }),
            candidate(7, 1.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }),
        ];
        assert_eq!(
            place_symbols(&mut index, &candidates),
            vec![false, true],
            "sort key 1.0 must still beat 2.0 -- only the LAYER dimension runs backwards"
        );
    }

    #[test]
    fn within_one_layer_the_lower_symbol_sort_key_places_first() {
        // Mapbox semantics, which are the opposite of "bigger number wins":
        // a LOWER `symbol-sort-key` places first. `settlement-major-label`
        // uses `["get", "symbolrank"]`, where rank 6 is a bigger city than
        // rank 12.
        let mut index = test_index();
        let candidates = [
            candidate(3, 12.0, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }),
            candidate(3, 6.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }),
        ];
        let placed = place_symbols(&mut index, &candidates);
        assert_eq!(placed, vec![false, true], "symbolrank 6 must beat symbolrank 12 in the same layer");
    }

    #[test]
    fn the_layer_index_outranks_the_sort_key_because_layer_order_is_the_primary_key() {
        // A very attractive sort key in an EARLIER layer must still lose to
        // a worse sort key in a LATER one -- the two keys are lexicographic,
        // not summed or otherwise blended.
        let mut index = test_index();
        let candidates = [
            candidate(2, -999.0, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }),
            candidate(7, 999.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }),
        ];
        let placed = place_symbols(&mut index, &candidates);
        assert_eq!(placed, vec![false, true]);
    }

    #[test]
    fn candidates_tied_on_layer_and_sort_key_fall_back_to_offer_order_so_placement_is_deterministic() {
        // With no tiebreak the winner between two identically-ranked
        // overlapping labels would depend on whatever order the sort
        // happened to leave them in, and could differ frame to frame. The
        // stable sort pins it to the order the symbol loop visited them.
        let mut index = test_index();
        let candidates = [
            candidate(4, 5.0, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 20.0 }),
            candidate(4, 5.0, CollisionBox { left: 50.0, top: 5.0, right: 150.0, bottom: 25.0 }),
        ];
        let placed = place_symbols(&mut index, &candidates);
        assert_eq!(placed, vec![true, false], "the first-offered of two tied candidates wins");
    }

    #[test]
    fn a_symbol_with_text_and_an_icon_collides_as_the_union_of_the_two_boxes() {
        // The documented divergence from MapLibre/Mapbox (see `symbol_box`):
        // one box for the pair, so an icon can never be orphaned from its
        // label. `intruder` misses the text box entirely and misses the icon
        // box entirely, but lands inside their union -- which is exactly the
        // case that distinguishes a union from two separate boxes.
        let text = CollisionBox { left: 0.0, top: 30.0, right: 100.0, bottom: 50.0 };
        let icon = CollisionBox { left: 40.0, top: 0.0, right: 60.0, bottom: 20.0 };
        let union = symbol_box(Some(text), Some(icon), 0.0).expect("text + icon produces a box");
        assert_eq!(union, CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 50.0 });

        let intruder = CollisionBox { left: 5.0, top: 22.0, right: 30.0, bottom: 28.0 };
        assert!(!intruder.intersects(&text), "the intruder clears the text box on its own");
        assert!(!intruder.intersects(&icon), "and clears the icon box on its own");

        let mut index = test_index();
        assert!(index.place(union));
        assert!(!index.place(intruder), "but it must still be rejected by the union");
    }

    #[test]
    fn a_symbol_with_only_text_or_only_an_icon_still_produces_a_box_and_one_with_neither_does_not() {
        let text = CollisionBox { left: 0.0, top: 0.0, right: 10.0, bottom: 10.0 };
        let icon = CollisionBox { left: 20.0, top: 20.0, right: 30.0, bottom: 30.0 };
        assert_eq!(symbol_box(Some(text), None, 0.0), Some(text));
        assert_eq!(symbol_box(None, Some(icon), 0.0), Some(icon));
        // A feature with neither must not consume a placement slot at all.
        assert_eq!(symbol_box(None, None, 2.0), None);
    }

    #[test]
    fn symbol_box_pads_the_union_rather_than_each_half_separately() {
        let text = CollisionBox { left: 0.0, top: 30.0, right: 100.0, bottom: 50.0 };
        let icon = CollisionBox { left: 40.0, top: 0.0, right: 60.0, bottom: 20.0 };
        assert_eq!(
            symbol_box(Some(text), Some(icon), 2.0),
            Some(CollisionBox { left: -2.0, top: -2.0, right: 102.0, bottom: 52.0 })
        );
    }

    #[test]
    fn a_box_spanning_several_cells_is_found_from_every_cell_it_spans() {
        // The whole point of registering a box in each cell it overlaps: a
        // query touching ANY of those cells must find it. With a 10px cell,
        // the wide box below spans columns 0..=9 of row 0, and each probe
        // starts in a different one of those columns.
        let bounds = CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 100.0 };
        let wide = CollisionBox { left: 1.0, top: 1.0, right: 99.0, bottom: 9.0 };
        for probe_left in [0.0_f32, 25.0, 55.0, 95.0] {
            let mut index = CollisionIndex::with_cell_size(bounds, 10.0);
            assert!(index.place(wide));
            let probe = CollisionBox { left: probe_left, top: 2.0, right: probe_left + 4.0, bottom: 8.0 };
            assert!(!index.place(probe), "a probe at x={probe_left} must find the wide box registered in that cell");
        }
        // And a probe in a row the wide box does NOT span still gets placed,
        // proving the multi-cell registration didn't just fill the grid.
        let mut index = CollisionIndex::with_cell_size(bounds, 10.0);
        assert!(index.place(wide));
        assert!(index.place(CollisionBox { left: 40.0, top: 50.0, right: 44.0, bottom: 56.0 }));
    }

    #[test]
    fn a_tall_box_spanning_several_rows_is_also_found_from_every_row_it_spans() {
        let bounds = CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 100.0 };
        let tall = CollisionBox { left: 1.0, top: 1.0, right: 9.0, bottom: 99.0 };
        for probe_top in [0.0_f32, 25.0, 55.0, 95.0] {
            let mut index = CollisionIndex::with_cell_size(bounds, 10.0);
            assert!(index.place(tall));
            let probe = CollisionBox { left: 2.0, top: probe_top, right: 8.0, bottom: probe_top + 4.0 };
            assert!(!index.place(probe), "a probe at y={probe_top} must find the tall box registered in that row");
        }
    }

    #[test]
    fn a_box_hanging_off_the_edge_of_the_padded_viewport_still_collides_with_its_neighbours() {
        // A label anchored just inside the padded viewport can still stick
        // out past it; `cell_range` clamps rather than rejecting so such a
        // label keeps taking part in placement instead of silently becoming
        // invisible to (and to) everything else.
        let bounds = CollisionBox { left: 0.0, top: 0.0, right: 100.0, bottom: 100.0 };
        let mut index = CollisionIndex::with_cell_size(bounds, 10.0);
        let overhanging = CollisionBox { left: -50.0, top: -50.0, right: 20.0, bottom: 20.0 };
        assert!(index.place(overhanging));
        assert!(!index.place(CollisionBox { left: 5.0, top: 5.0, right: 15.0, bottom: 15.0 }));
        // Entirely outside on the far side: still answered, no panic.
        assert!(index.place(CollisionBox { left: 400.0, top: 400.0, right: 500.0, bottom: 500.0 }));
    }

    #[test]
    fn a_non_finite_box_is_rejected_rather_than_poisoning_the_grid_arithmetic() {
        // Box coordinates are style-driven and then multiplied by a
        // window-supplied scale factor, so a NaN is reachable without a bug
        // on our side. Losing one label beats NaN-indexing the grid.
        let mut index = test_index();
        assert!(!index.place(CollisionBox { left: f32::NAN, top: 0.0, right: 10.0, bottom: 10.0 }));
        assert!(!index.place(CollisionBox { left: 0.0, top: 0.0, right: f32::INFINITY, bottom: 10.0 }));
        // ...and nothing was actually registered, so a real box still fits.
        assert!(index.place(CollisionBox { left: 0.0, top: 0.0, right: 10.0, bottom: 10.0 }));
    }

    #[test]
    fn an_empty_candidate_list_places_nothing_and_does_not_panic() {
        let mut index = test_index();
        assert!(place_symbols(&mut index, &[]).is_empty());
    }

    #[test]
    fn every_candidate_gets_an_answer_in_its_original_position_so_parallel_lists_stay_aligned() {
        // The contract the symbol loop depends on to filter its text list
        // and its icon list against the same decision.
        let mut index = test_index();
        let candidates = [
            candidate(0, 0.0, CollisionBox { left: 0.0, top: 0.0, right: 50.0, bottom: 20.0 }),
            candidate(0, 0.0, CollisionBox { left: 10.0, top: 0.0, right: 60.0, bottom: 20.0 }),
            candidate(0, 0.0, CollisionBox { left: 200.0, top: 0.0, right: 250.0, bottom: 20.0 }),
        ];
        let placed = place_symbols(&mut index, &candidates);
        assert_eq!(placed.len(), candidates.len());
        assert_eq!(placed, vec![true, false, true]);
    }

    // ---- Task 3: retaining `CollisionIndex` across frames -------------
    //
    // Production keeps ONE `CollisionIndex` on `GpuState` and calls
    // `reset(bounds)` on it every frame instead of `CollisionIndex::new`.
    // The two tests below are RULING R2's two halves: correctness first
    // (a retained, reset index must place EXACTLY what a fresh one would,
    // every frame -- the one that matters, since a leak here would
    // silently change which labels draw) and the allocation count second
    // (a warm, reset index must allocate strictly less than building fresh
    // does).

    /// Five overlapping "frames" of candidates, run through a retained,
    /// `reset` index and through a brand-new `CollisionIndex::new` each
    /// time, must agree frame for frame.
    #[test]
    fn a_retained_reset_index_places_identically_to_a_fresh_index_every_frame() {
        let bounds = CollisionBox { left: -100.0, top: -100.0, right: 1000.0, bottom: 800.0 };
        // Shifted a little each "frame" so which pair collides keeps
        // changing, not just whether anything does.
        let frames: Vec<[PlacementCandidate; 3]> = (0..5)
            .map(|frame| {
                let shift = frame as f32 * 3.0;
                [
                    candidate(2, 0.0, CollisionBox { left: shift, top: 0.0, right: 60.0 + shift, bottom: 20.0 }),
                    candidate(
                        1,
                        0.0,
                        CollisionBox { left: 30.0 + shift, top: 5.0, right: 90.0 + shift, bottom: 25.0 },
                    ),
                    candidate(3, 1.0, CollisionBox { left: 300.0, top: 300.0, right: 340.0, bottom: 320.0 }),
                ]
            })
            .collect();

        let mut retained = CollisionIndex::new(bounds);
        for candidates in &frames {
            retained.reset(bounds);
            let retained_placed = place_symbols(&mut retained, candidates);

            let mut fresh = CollisionIndex::new(bounds);
            let fresh_placed = place_symbols(&mut fresh, candidates);

            assert_eq!(
                retained_placed, fresh_placed,
                "a retained index cleared with reset() must place the same accepted set, in the \
                 same order, as a brand-new CollisionIndex::new would -- every frame, not just the \
                 first"
            );
        }
    }

    /// A `reset` index that has already placed once (so its cells' `Vec`s
    /// already hold whatever capacity that took) must allocate strictly
    /// fewer times on its next use than constructing `CollisionIndex::new`
    /// fresh does -- the allocation win Task 3 exists for.
    ///
    /// The margin is a handful of allocations, so what makes the two
    /// windows below readable at all is that `alloc_counting`'s counter is
    /// PER-THREAD: this test body runs on one thread, and the ~1200 other
    /// tests `cargo test` runs beside it allocate into their own counters,
    /// not this one. Nothing here needs a lock, and a lock would not have
    /// been enough anyway -- the allocator hook fires on every thread in
    /// the process, including the ones that never agreed to take it.
    #[test]
    fn a_warm_reset_index_allocates_less_than_a_fresh_index() {
        let bounds = CollisionBox { left: -100.0, top: -100.0, right: 1000.0, bottom: 800.0 };
        let candidates = [
            candidate(2, 0.0, CollisionBox { left: 0.0, top: 0.0, right: 60.0, bottom: 20.0 }),
            candidate(1, 0.0, CollisionBox { left: 30.0, top: 5.0, right: 90.0, bottom: 25.0 }),
            candidate(3, 1.0, CollisionBox { left: 300.0, top: 300.0, right: 340.0, bottom: 320.0 }),
        ];

        // Warm-up: the first use of a freshly built index still has to
        // grow every cell's Vec from its initial zero capacity, exactly
        // like `paint()`'s very first frame does. Steady state -- every
        // frame after that -- is the case this task optimises.
        let mut retained = CollisionIndex::new(bounds);
        retained.reset(bounds);
        let _ = place_symbols(&mut retained, &candidates);

        let before = alloc_counting::count();
        retained.reset(bounds);
        let retained_placed = place_symbols(&mut retained, &candidates);
        let retained_allocs = alloc_counting::count() - before;

        let before = alloc_counting::count();
        let mut fresh = CollisionIndex::new(bounds);
        let fresh_placed = place_symbols(&mut fresh, &candidates);
        let fresh_allocs = alloc_counting::count() - before;

        assert_eq!(
            retained_placed, fresh_placed,
            "the allocation win must not come from placing a different set of symbols"
        );
        assert!(
            retained_allocs < fresh_allocs,
            "a warm, reset index should allocate strictly fewer times than building fresh does: \
             retained={retained_allocs} fresh={fresh_allocs}"
        );
    }

    /// A cell that outgrows `INITIAL_CELL_CAPACITY` keeps every item it
    /// already held, including the items of the cells the rebuild shifts
    /// along behind it.
    ///
    /// The bucketed-CSR layout grows by doubling EVERY cell's span and
    /// moving each cell's live prefix into its new one. If that move dropped
    /// or misplaced a prefix, the boxes it held would simply stop colliding
    /// -- silently, since nothing checks the array's shape at runtime. So
    /// this fills one cell well past several doublings and then probes for
    /// boxes inserted BEFORE the first growth: one in that same hot cell,
    /// and one in an earlier cell whose span the rebuild had to leave alone.
    #[test]
    fn a_cell_that_grows_past_its_initial_capacity_keeps_everything_already_in_it() {
        // 3 columns x 1 row at this cell size, so the hot cell (column 2)
        // has cells before it that the backwards rebuild walk must not
        // clobber.
        let bounds = CollisionBox { left: 0.0, top: 0.0, right: 300.0, bottom: 100.0 };
        let mut index = CollisionIndex::with_cell_size(bounds, 100.0);

        // Column 0, inserted first and never touched again.
        let early = CollisionBox { left: 10.0, top: 10.0, right: 14.0, bottom: 14.0 };
        assert!(index.place(early));

        // Nineteen mutually clear boxes, all inside column 2 -- comfortably
        // past 4, 8 and 16 slots, so the doubling path runs at least three
        // times.
        let hot: Vec<CollisionBox> =
            (0..19).map(|i| CollisionBox::from_origin_size(200.0 + i as f32 * 5.0, 10.0, 4.0, 4.0)).collect();
        for &box_ in &hot {
            assert!(index.place(box_), "the hot-cell boxes are 1px apart and must all fit: {box_:?}");
        }

        assert!(
            !index.place(CollisionBox { left: 12.0, top: 12.0, right: 20.0, bottom: 20.0 }),
            "the box placed before any growth, in an untouched earlier cell, must still be found"
        );
        assert!(
            !index.place(CollisionBox { left: 202.0, top: 12.0, right: 206.0, bottom: 16.0 }),
            "the hot cell's FIRST item must survive every rebuild, not just its most recent ones"
        );
        assert!(
            !index.place(CollisionBox { left: 291.0, top: 12.0, right: 295.0, bottom: 16.0 }),
            "and so must its last"
        );
        // Column 1 was never written to, and the rebuilds must not have
        // spilled anything into it.
        assert!(index.place(CollisionBox { left: 120.0, top: 10.0, right: 180.0, bottom: 14.0 }));
    }

    /// Pins the ORDER-DEPENDENT outcome of greedy placement, which is what
    /// a layout change could silently break. Deterministic input, exact
    /// expected output: `place_symbols` sorts by key then places in that
    /// order, so the set of survivors is a fact about the algorithm and not
    /// about the layout the index happens to store its boxes in.
    #[test]
    fn greedy_placement_outcome_is_pinned_against_a_layout_change() {
        let bounds = CollisionBox { left: 0.0, top: 0.0, right: 256.0, bottom: 256.0 };
        // A deterministic lattice with deliberate partial overlaps, spanning
        // cell boundaries at the default cell size so multi-cell insertion
        // is exercised rather than assumed.
        //
        // Both priority dimensions move: `layer_index` DESCENDING is the
        // primary key and `sort_key` ASCENDING the secondary (see
        // `place_symbols`), and ties fall back to position in `candidates`.
        // A lattice that varied only one of them would pass while the other
        // silently reordered.
        let candidates: Vec<PlacementCandidate> = (0..200)
            .map(|i| PlacementCandidate {
                layer_index: i % 5,
                sort_key: (i % 7) as f32,
                bounds: CollisionBox::from_origin_size(
                    ((i * 37) % 240) as f32,
                    ((i * 53) % 240) as f32,
                    18.0,
                    12.0,
                ),
            })
            .collect();
        let mut index = CollisionIndex::new(bounds);
        let placed = place_symbols(&mut index, &candidates);
        let survivors: Vec<usize> = placed.iter().enumerate().filter(|(_, p)| **p).map(|(i, _)| i).collect();
        // Recorded from the implementation as it stood BEFORE the flat-array
        // restructure, and asserted verbatim afterwards.
        const PINNED_SURVIVORS: &[usize] = &[
            0, 2, 4, 7, 9, 12, 13, 14, 17, 18, 19, 22, 24, 26, 29, 32, 34, 37, 39, 41, 42, 44, 46, 47, 49, 51,
            52, 54, 56, 57, 59, 61, 62, 64, 66, 67, 68, 69, 71, 72, 73, 74, 76, 77, 78, 79, 81, 82, 83, 84, 86,
            87, 88, 89, 91, 92, 93, 94, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 107, 109, 110, 112, 114,
            115, 119, 120, 124, 125, 127, 129, 131, 132, 134, 135, 137, 139, 142, 144, 147, 149, 152, 154, 157,
            159, 162, 163, 164, 167, 169, 172, 174, 179, 181, 184, 189, 194, 199,
        ];
        assert_eq!(survivors.len(), PINNED_SURVIVORS.len(), "greedy outcome changed");
        assert_eq!(survivors, PINNED_SURVIVORS, "the SET of survivors changed, not just the count");
    }

    /// The resize branch inside `reset` (`columns`/`rows`/`cells` all
    /// updated together when the derived grid size actually changes) had
    /// ZERO coverage before this test: both tests above call `reset` with
    /// the SAME bounds every time, so `columns != self.columns` never
    /// fires. This drives `reset` across bounds that genuinely change
    /// size -- growing, then shrinking, then growing back -- and checks
    /// both the structural fields directly AND that placement still
    /// matches a fresh index built at each size, so a stale dimension
    /// cannot pass by accident on either axis.
    #[test]
    fn reset_across_changing_bounds_grows_and_shrinks_the_grid_correctly() {
        // A coarse cell size so the column/row arithmetic below is easy
        // to state and hand-check, rather than reasoning about 64px
        // cells over near-1000px viewports.
        let cell_size = 100.0;

        // (bounds, expected columns, expected rows). Step 2 grows from
        // step 1 by a DIFFERENT amount on each axis (+7 columns, +1 row)
        // so a transposed or reused dimension cannot pass; step 3 then
        // SHRINKS from step 2, again by a different amount per axis (-9
        // columns, -2 rows) -- shrinking is exactly where a stale `cells`
        // length or a missed `self.columns =`/`self.rows =` update would
        // hide, per the brief's own "silently misplace every label"
        // warning. Step 4 returns to step 1's own size, so a bug that
        // only shows up growing back up after a shrink is covered too.
        let steps: [(CollisionBox, usize, usize); 4] = [
            (CollisionBox { left: 0.0, top: 0.0, right: 500.0, bottom: 300.0 }, 5, 3),
            (CollisionBox { left: 0.0, top: 0.0, right: 1200.0, bottom: 350.0 }, 12, 4),
            (CollisionBox { left: 0.0, top: 0.0, right: 250.0, bottom: 150.0 }, 3, 2),
            (CollisionBox { left: 0.0, top: 0.0, right: 500.0, bottom: 300.0 }, 5, 3),
        ];

        // Two candidates that genuinely overlap, kept well inside the
        // SMALLEST bounds above (250x150) so the same pair exercises
        // every grid size without border clamping -- which has its own
        // dedicated test -- muddying this one.
        let overlapping = [
            candidate(0, 0.0, CollisionBox { left: 10.0, top: 10.0, right: 110.0, bottom: 60.0 }),
            candidate(0, 0.0, CollisionBox { left: 60.0, top: 20.0, right: 160.0, bottom: 70.0 }),
        ];
        assert!(
            overlapping[0].bounds.intersects(&overlapping[1].bounds),
            "precondition: these two really do overlap, or this test proves nothing about \
             collision, only resizing"
        );

        let mut index = CollisionIndex::with_cell_size(steps[0].0, cell_size);
        for &(bounds, expected_columns, expected_rows) in &steps {
            index.reset(bounds);
            assert_eq!(
                index.columns, expected_columns,
                "reset to {bounds:?} must land on the right column count"
            );
            assert_eq!(index.rows, expected_rows, "reset to {bounds:?} must land on the right row count");
            assert_eq!(
                index.cell_fill.len(),
                expected_columns * expected_rows,
                "the cell grid itself must be resized to match, not just the columns/rows fields"
            );
            assert_eq!(
                index.cell_starts.len(),
                expected_columns * expected_rows + 1,
                "the CSR start array carries one extra entry, and must be resized with the grid"
            );
            assert_eq!(
                *index.cell_starts.last().expect("cell_starts is never empty") as usize,
                index.cell_items.len(),
                "the last start must be the total slot count, or some cell's span runs off the end"
            );

            let retained_placed = place_symbols(&mut index, &overlapping);

            let mut fresh = CollisionIndex::with_cell_size(bounds, cell_size);
            let fresh_placed = place_symbols(&mut fresh, &overlapping);

            assert_eq!(
                retained_placed, fresh_placed,
                "reset() to {bounds:?} must place identically to a fresh index built at the same \
                 bounds -- a stale dimension surviving the resize would show up here as a \
                 divergence, not just in the columns/rows fields above"
            );
            assert_eq!(
                retained_placed.iter().filter(|&&p| p).count(),
                1,
                "exactly one of the two overlapping candidates should be placed at every grid size"
            );
        }
    }
}
