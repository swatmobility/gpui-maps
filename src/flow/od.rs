//! Trip rows into places and corridors: both ends of every row snapped to
//! one hexagonal lattice, weights summed per ordered cell pair.
//!
//! # Why aggregate at all
//!
//! A commuting table is hundreds of thousands of rows between a few
//! thousand area centroids. Drawn one line per row it is a solid hairball
//! at any zoom that shows the whole country, and Edge-Path Bundling (one
//! Dijkstra per edge) cannot take it either. Snapping both ends to a cell
//! of a size chosen from the data's own extent turns "row" into "flow
//! between two places", and the heaviest few thousand of those are the
//! picture.
//!
//! # The cells: a lattice, places, or H3
//!
//! [`OdGrid::build`] snaps to the hexbin lattice, [`OdGrid::from_places`]
//! to data-driven places and, behind the `h3` feature,
//! [`OdGrid::from_h3`] to H3 cells at one resolution (the hex-demand
//! layer's default where H3 is built: the cells a downstream desktop app's
//! demand layer and ClickHouse's `geoToH3` use, so a cell id means the
//! same thing in every tool).
//!
//! # Why the hexbin lattice
//!
//! It is the one the Explorer's hexbin layer draws ([`crate::overlay::hexbin`]),
//! it needs no H3 build (so it works in every shell), and its cell size is
//! a ground distance a reader can be told. A cell's [`OdCell::centroid`]
//! is the weighted mean of the endpoints that fell in it, so a corridor
//! starts where the people are, not at a lattice point in a field.

use std::collections::HashMap;

use crate::overlay::hexbin::{from_mercator, hexagon_vertices, hexbin_center, point_to_hexbin, to_mercator};

/// Mercator's latitude limit, degrees.
const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Which way round to read a selected cell's flows.
///
/// A district is two different places depending on the question. "Where do
/// people from here go" and "where do the people here come from" have
/// genuinely different answers -- a dormitory town sends thousands out in
/// the morning and receives them back at night -- and summing the two
/// answers neither. So the reader picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FlowDirection {
    /// Journeys leaving the selected cell, by where they end.
    #[default]
    Out,
    /// Journeys arriving at the selected cell, by where they began.
    In,
    /// Both summed -- total connectedness, the right measure for "how
    /// strong is the link between these two places" and the wrong one for
    /// anything directional.
    Both,
}

impl FlowDirection {
    pub const ALL: [FlowDirection; 3] = [FlowDirection::Out, FlowDirection::In, FlowDirection::Both];

    pub fn key(self) -> &'static str {
        match self {
            FlowDirection::Out => "out",
            FlowDirection::In => "in",
            FlowDirection::Both => "both",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.key() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            FlowDirection::Out => "Outbound",
            FlowDirection::In => "Inbound",
            FlowDirection::Both => "Both",
        }
    }
}

/// One occupied cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OdCell {
    /// Lattice column and row (see [`crate::overlay::hexbin::HexBin::id`]).
    pub id: (i32, i32),
    /// The lattice centre, `[lon, lat]`.
    pub center: [f64; 2],
    /// The weighted mean of every endpoint that fell in the cell.
    pub centroid: [f64; 2],
    /// Weight of the rows starting here and ending in another cell.
    pub departures: f64,
    /// Weight of the rows ending here and starting in another cell.
    pub arrivals: f64,
    /// Weight of the rows starting and ending here.
    pub internal: f64,
}

/// The summed weight from one cell to another (never the same one).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OdPair {
    pub from: u32,
    pub to: u32,
    pub weight: f64,
    /// Input rows summed into it.
    pub rows: u32,
}

/// Aggregation options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OdOptions {
    /// Centre to vertex, metres on the ground at `reference_lat`.
    pub radius_m: f64,
    /// `None` takes the middle of the endpoints' latitude extent.
    pub reference_lat: Option<f64>,
}

/// What an [`OdGrid`]'s cells are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OdNodes {
    /// Hexagons of one lattice ([`OdGrid::build`]): what a hex-demand
    /// choropleth draws.
    #[default]
    Lattice,
    /// Data-driven places ([`crate::flow::places`]): a cell's id is
    /// `(place, 0)`, its centre the place's centroid.
    Places,
    /// H3 cells at one resolution ([`OdGrid::from_h3`], behind the `h3`
    /// feature): a cell's id is its index split into high and low halves
    /// ([`h3_id`]), its centre the cell's own, its outline the cell's
    /// boundary.
    H3 { resolution: u8 },
}

/// An H3 index as an [`OdCell::id`]: its high and low 32 bits.
#[must_use]
pub fn h3_id(index: u64) -> (i32, i32) {
    ((index >> 32) as u32 as i32, index as u32 as i32)
}

/// The H3 index an [`OdCell::id`] made by [`h3_id`] carries.
#[must_use]
pub fn id_h3(id: (i32, i32)) -> u64 {
    (u64::from(id.0 as u32) << 32) | u64::from(id.1 as u32)
}

/// H3 cell `cell` at `res` for `[lon, lat]`. `h3o::LatLng::new` takes
/// LATITUDE first: the one place the order flips, so it is said once here.
#[cfg(feature = "h3")]
fn h3_cell(p: [f64; 2], res: h3o::Resolution) -> Option<h3o::CellIndex> {
    h3o::LatLng::new(p[1], p[0]).ok().map(|ll| ll.to_cell(res))
}

/// `cell`'s centre and closed boundary, `[lon, lat]`, longitudes unwrapped
/// round the centre (a cell across the antimeridian stays one ring), as
/// [`crate::overlay::hexes::record_for_cell`] does.
#[cfg(feature = "h3")]
fn h3_outline(cell: h3o::CellIndex) -> ([f64; 2], Vec<[f64; 2]>) {
    let c = h3o::LatLng::from(cell);
    let mut ring: Vec<[f64; 2]> = cell
        .boundary()
        .iter()
        .map(|ll| {
            let mut lon = ll.lng();
            while lon - c.lng() > 180.0 {
                lon -= 360.0;
            }
            while lon - c.lng() < -180.0 {
                lon += 360.0;
            }
            [lon, ll.lat()]
        })
        .collect();
    if let Some(first) = ring.first().copied() {
        ring.push(first);
    }
    ([c.lng(), c.lat()], ring)
}

/// The per-cell sums both builds share, and the pairs.
struct Agg {
    acc: Vec<Acc>,
    pair_of: HashMap<(u32, u32), (f64, u32)>,
    skipped: usize,
}

struct Acc {
    sum: [f64; 2],
    w: f64,
    departures: f64,
    arrivals: f64,
    internal: f64,
}

impl Acc {
    const ZERO: Acc = Acc { sum: [0.0; 2], w: 0.0, departures: 0.0, arrivals: 0.0, internal: 0.0 };
}

impl Agg {
    fn new(cells: usize) -> Self {
        Self { acc: (0..cells).map(|_| Acc::ZERO).collect(), pair_of: HashMap::new(), skipped: 0 }
    }

    /// Row `a -> b` of weight `w` from cell `ca` to cell `cb`.
    fn add(&mut self, ca: u32, cb: u32, a: [f64; 2], b: [f64; 2], w: f64) {
        // The centroid weights each end by the row's weight, with a floor
        // so a zero-weight row still places its cell somewhere.
        let cw = w.max(1e-9);
        for (c, p) in [(ca, a), (cb, b)] {
            let e = &mut self.acc[c as usize];
            e.sum[0] += p[0] * cw;
            e.sum[1] += p[1] * cw;
            e.w += cw;
        }
        if ca == cb {
            self.acc[ca as usize].internal += w;
            return;
        }
        self.acc[ca as usize].departures += w;
        self.acc[cb as usize].arrivals += w;
        let e = self.pair_of.entry((ca, cb)).or_insert((0.0, 0));
        e.0 += w;
        e.1 += 1;
    }

    /// The cells (ids and centres given) and the pairs, heaviest first.
    fn finish(self, ids: &[(i32, i32)], centers: &[[f64; 2]]) -> (Vec<OdCell>, Vec<OdPair>, Vec<Vec<u32>>, Vec<Vec<u32>>, usize) {
        let cells: Vec<OdCell> = self
            .acc
            .iter()
            .zip(ids)
            .zip(centers)
            .map(|((a, id), center)| {
                let centroid = if a.w > 0.0 { [a.sum[0] / a.w, a.sum[1] / a.w] } else { *center };
                OdCell { id: *id, center: *center, centroid, departures: a.departures, arrivals: a.arrivals, internal: a.internal }
            })
            .collect();
        let mut pairs: Vec<OdPair> = self.pair_of.into_iter().map(|((from, to), (weight, rows))| OdPair { from, to, weight, rows }).collect();
        pairs.sort_unstable_by(|a, b| b.weight.total_cmp(&a.weight).then(a.from.cmp(&b.from)).then(a.to.cmp(&b.to)));
        let mut out_pairs = vec![Vec::new(); cells.len()];
        let mut in_pairs = vec![Vec::new(); cells.len()];
        for (k, p) in pairs.iter().enumerate() {
            out_pairs[p.from as usize].push(k as u32);
            in_pairs[p.to as usize].push(k as u32);
        }
        (cells, pairs, out_pairs, in_pairs, self.skipped)
    }
}

/// Rows aggregated onto the lattice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OdGrid {
    pub radius_m: f64,
    pub radius_mercator: f64,
    pub reference_lat: f64,
    pub cells: Vec<OdCell>,
    /// Every cell-to-other-cell pair, heaviest first (ties by cell order,
    /// so a given input always comes out the same).
    pub pairs: Vec<OdPair>,
    /// Rows read, and those skipped (a non-finite end or weight).
    pub rows: usize,
    pub skipped: usize,
    /// What the cells are: lattice hexagons, or data-driven places
    /// ([`OdGrid::from_places`]).
    pub nodes: OdNodes,
    index: HashMap<(i32, i32), u32>,
    /// Pair indices by `from` cell, and by `to` cell.
    out_pairs: Vec<Vec<u32>>,
    in_pairs: Vec<Vec<u32>>,
    /// Each cell's closed outline, `[lon, lat]`: H3 grids only (a lattice
    /// hexagon is computed from its id).
    rings: Vec<Vec<[f64; 2]>>,
}

impl OdGrid {
    /// Aggregates rows `origins[i] -> destinations[i]` (each `[lon, lat]`)
    /// with `weights[i]` (every row 1 when `None`; a negative or non-finite
    /// weight skips the row).
    #[must_use]
    pub fn build(origins: &[[f64; 2]], destinations: &[[f64; 2]], weights: Option<&[f64]>, options: OdOptions) -> Self {
        let rows = origins.len().min(destinations.len());
        let ok = |p: [f64; 2]| p[0].is_finite() && p[1].is_finite() && p[1].abs() <= MAX_LAT;
        let reference_lat = options.reference_lat.unwrap_or_else(|| {
            let (lo, hi) = origins[..rows].iter().chain(&destinations[..rows]).filter(|p| ok(**p)).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
            if lo.is_finite() { (lo + hi) / 2.0 } else { 0.0 }
        });
        let cos = reference_lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-6);
        let radius_m = options.radius_m.max(1.0);
        let radius = radius_m / cos;

        let mut index: HashMap<(i32, i32), u32> = HashMap::new();
        let mut agg = Agg::new(0);
        let mut ids: Vec<(i32, i32)> = Vec::new();
        let mut cell = |p: [f64; 2], index: &mut HashMap<(i32, i32), u32>, agg: &mut Agg| -> u32 {
            let id = point_to_hexbin(to_mercator(p[0], p[1]), radius);
            *index.entry(id).or_insert_with(|| {
                agg.acc.push(Acc::ZERO);
                ids.push(id);
                (agg.acc.len() - 1) as u32
            })
        };
        for i in 0..rows {
            let (a, b) = (origins[i], destinations[i]);
            let w = weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(f64::NAN));
            if !ok(a) || !ok(b) || !(w >= 0.0) || !w.is_finite() {
                agg.skipped += 1;
                continue;
            }
            let ca = cell(a, &mut index, &mut agg);
            let cb = cell(b, &mut index, &mut agg);
            agg.add(ca, cb, a, b, w);
        }
        let centers: Vec<[f64; 2]> = ids
            .iter()
            .map(|id| {
                let c = hexbin_center(*id, radius);
                from_mercator(c[0], c[1])
            })
            .collect();
        let (cells, pairs, out_pairs, in_pairs, skipped) = agg.finish(&ids, &centers);
        Self { radius_m, radius_mercator: radius, reference_lat, cells, pairs, rows, skipped, nodes: OdNodes::Lattice, index, out_pairs, in_pairs, rings: Vec::new() }
    }

    /// Aggregates rows `origins[i] -> destinations[i]` between `places`
    /// ([`crate::flow::places::cluster`] of the same rows): cell `k` is
    /// place `k`, centred on its centroid, and a pair sums the rows from
    /// one place to another. `radius_m` is what a click is matched within
    /// (the places' grouping radius, say).
    #[must_use]
    pub fn from_places(origins: &[[f64; 2]], destinations: &[[f64; 2]], weights: Option<&[f64]>, places: &crate::flow::places::Places, radius_m: f64) -> Self {
        use crate::flow::places::Places;
        let rows = origins.len().min(destinations.len());
        let mut agg = Agg::new(places.centroids.len());
        for i in 0..rows {
            let (ca, cb) = (places.origin.get(i).copied().unwrap_or(Places::NONE), places.destination.get(i).copied().unwrap_or(Places::NONE));
            if ca == Places::NONE || cb == Places::NONE {
                agg.skipped += 1;
                continue;
            }
            let w = weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(f64::NAN));
            agg.add(ca, cb, origins[i], destinations[i], w);
        }
        let ids: Vec<(i32, i32)> = (0..places.centroids.len() as i32).map(|k| (k, 0)).collect();
        let (lo, hi) = places.centroids.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        let reference_lat = if lo.is_finite() { (lo + hi) / 2.0 } else { 0.0 };
        let cos = reference_lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-6);
        let radius_m = radius_m.max(1.0);
        let index = ids.iter().enumerate().map(|(k, id)| (*id, k as u32)).collect();
        let (cells, pairs, out_pairs, in_pairs, skipped) = agg.finish(&ids, &places.centroids);
        Self { radius_m, radius_mercator: radius_m / cos, reference_lat, cells, pairs, rows, skipped, nodes: OdNodes::Places, index, out_pairs, in_pairs, rings: Vec::new() }
    }

    /// Aggregates rows `origins[i] -> destinations[i]` (each `[lon, lat]`)
    /// onto H3 cells at `resolution` (clamped to 0..=15): both ends snapped
    /// to their cell, the weights summed per ordered cell pair, each cell's
    /// outline kept for drawing. `weights` as [`OdGrid::build`]'s.
    ///
    /// `radius_m` is the resolution's average edge length, which on a
    /// hexagon is also centre to vertex: the size a reader is told.
    #[cfg(feature = "h3")]
    #[must_use]
    pub fn from_h3(origins: &[[f64; 2]], destinations: &[[f64; 2]], weights: Option<&[f64]>, resolution: u8) -> Self {
        let res = h3o::Resolution::try_from(resolution.min(15)).unwrap_or(h3o::Resolution::Fifteen);
        let rows = origins.len().min(destinations.len());
        let ok = |p: [f64; 2]| p[0].is_finite() && p[1].is_finite() && p[1].abs() <= MAX_LAT;
        let mut index: HashMap<(i32, i32), u32> = HashMap::new();
        let mut agg = Agg::new(0);
        let mut found: Vec<h3o::CellIndex> = Vec::new();
        let mut cell = |c: h3o::CellIndex, index: &mut HashMap<(i32, i32), u32>, agg: &mut Agg| -> u32 {
            *index.entry(h3_id(u64::from(c))).or_insert_with(|| {
                agg.acc.push(Acc::ZERO);
                found.push(c);
                (agg.acc.len() - 1) as u32
            })
        };
        for i in 0..rows {
            let (a, b) = (origins[i], destinations[i]);
            let w = weights.map_or(1.0, |w| w.get(i).copied().unwrap_or(f64::NAN));
            let ends = if ok(a) && ok(b) && w >= 0.0 && w.is_finite() { h3_cell(a, res).zip(h3_cell(b, res)) } else { None };
            let Some((ha, hb)) = ends else {
                agg.skipped += 1;
                continue;
            };
            let ca = cell(ha, &mut index, &mut agg);
            let cb = cell(hb, &mut index, &mut agg);
            agg.add(ca, cb, a, b, w);
        }
        let (centers, rings): (Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>) = found.iter().map(|c| h3_outline(*c)).unzip();
        let ids: Vec<(i32, i32)> = found.iter().map(|c| h3_id(u64::from(*c))).collect();
        let (lo, hi) = centers.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        let reference_lat = if lo.is_finite() { (lo + hi) / 2.0 } else { 0.0 };
        let cos = reference_lat.clamp(-MAX_LAT, MAX_LAT).to_radians().cos().max(1e-6);
        let radius_m = res.edge_length_m();
        let (cells, pairs, out_pairs, in_pairs, skipped) = agg.finish(&ids, &centers);
        Self { radius_m, radius_mercator: radius_m / cos, reference_lat, cells, pairs, rows, skipped, nodes: OdNodes::H3 { resolution: u8::from(res) }, index, out_pairs, in_pairs, rings }
    }

    /// The H3 resolution of an H3 grid ([`OdGrid::from_h3`]).
    #[must_use]
    pub fn h3_resolution(&self) -> Option<u8> {
        match self.nodes {
            OdNodes::H3 { resolution } => Some(resolution),
            _ => None,
        }
    }

    /// Cell `cell`'s H3 index, on an H3 grid.
    #[must_use]
    pub fn h3_index(&self, cell: u32) -> Option<u64> {
        self.h3_resolution()?;
        self.cells.get(cell as usize).map(|c| id_h3(c.id))
    }

    /// The occupied cell with H3 index `index`, on an H3 grid.
    #[must_use]
    pub fn cell_of_h3(&self, index: u64) -> Option<u32> {
        self.h3_resolution()?;
        self.index.get(&h3_id(index)).copied()
    }

    /// The occupied cell standing for H3 index `index` at THIS grid's
    /// resolution, whatever `index`'s own: when it is finer, the cell its
    /// centre falls in (H3 children do not tile their parent, so near an
    /// edge the parent is the wrong one), else its parent; when coarser,
    /// the busiest occupied cell whose centre falls in it (its centre
    /// child may hold nobody). A selection made at one zoom survives a
    /// change of resolution this way.
    #[cfg(feature = "h3")]
    #[must_use]
    pub fn resolve_h3(&self, index: u64) -> Option<u32> {
        let res = h3o::Resolution::try_from(self.h3_resolution()?).ok()?;
        let c = h3o::CellIndex::try_from(index).ok()?;
        match c.resolution().cmp(&res) {
            std::cmp::Ordering::Equal => self.cell_of_h3(index),
            // The cell its centre falls in (H3 children do not tile their
            // parent exactly: near an edge the parent is the neighbour),
            // else its parent.
            std::cmp::Ordering::Greater => self.cell_of_h3(u64::from(h3o::LatLng::from(c).to_cell(res))).or_else(|| self.cell_of_h3(u64::from(c.parent(res)?))),
            std::cmp::Ordering::Less => {
                // Overlapping it: its centre, its parent or a corner in it
                // (the hierarchy is approximate, so all three count).
                let inside = |k: &usize| {
                    let at = |p: [f64; 2]| h3_cell(p, c.resolution()) == Some(c);
                    at(self.cells[*k].center) || h3o::CellIndex::try_from(id_h3(self.cells[*k].id)).ok().and_then(|x| x.parent(c.resolution())) == Some(c) || self.rings[*k].iter().any(|v| at(*v))
                };
                let load = |k: usize| self.cells[k].departures + self.cells[k].arrivals + self.cells[k].internal;
                (0..self.cells.len()).filter(inside).max_by(|a, b| load(*a).total_cmp(&load(*b)).then(b.cmp(a))).map(|k| k as u32)
            }
        }
    }

    /// The occupied cell `[lon, lat]` falls in, if any.
    #[must_use]
    pub fn cell_at(&self, lon: f64, lat: f64) -> Option<u32> {
        if !lon.is_finite() || !lat.is_finite() || lat.abs() > MAX_LAT {
            return None;
        }
        if self.nodes == OdNodes::Places {
            // The nearest place within the radius.
            let q = to_mercator(lon, lat);
            let near = self.cells.iter().enumerate().map(|(k, c)| {
                let p = to_mercator(c.center[0], c.center[1]);
                (k as u32, (p[0] - q[0]).hypot(p[1] - q[1]))
            });
            return near.filter(|(_, d)| *d <= self.radius_mercator).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(k, _)| k);
        }
        if let OdNodes::H3 { resolution } = self.nodes {
            #[cfg(feature = "h3")]
            {
                let res = h3o::Resolution::try_from(resolution).ok()?;
                return self.cell_of_h3(u64::from(h3_cell([lon, lat], res)?));
            }
            #[cfg(not(feature = "h3"))]
            {
                let _ = resolution;
                return None;
            }
        }
        self.index.get(&point_to_hexbin(to_mercator(lon, lat), self.radius_mercator)).copied()
    }

    /// The cell with lattice id `id`, if occupied.
    #[must_use]
    pub fn cell_of_id(&self, id: (i32, i32)) -> Option<u32> {
        self.index.get(&id).copied()
    }

    /// Cell `cell`'s hexagon as a closed `[lon, lat]` ring, scaled by
    /// `coverage` (`0..=1`; 1 tiles with no gap).
    #[must_use]
    pub fn hexagon(&self, cell: u32, coverage: f64) -> Vec<[f64; 2]> {
        let k = coverage.clamp(0.0, 1.0);
        let c = match self.nodes {
            // The cell's own boundary, drawn in towards its centre.
            OdNodes::H3 { .. } => {
                let m = self.cells[cell as usize].center;
                return self.rings[cell as usize].iter().map(|v| [m[0] + (v[0] - m[0]) * k, m[1] + (v[1] - m[1]) * k]).collect();
            }
            OdNodes::Lattice => hexbin_center(self.cells[cell as usize].id, self.radius_mercator),
            // A place has no cell: a hexagon round its centre.
            OdNodes::Places => {
                let p = self.cells[cell as usize].center;
                to_mercator(p[0], p[1])
            }
        };
        let mut ring: Vec<[f64; 2]> = hexagon_vertices(c, self.radius_mercator * k).iter().map(|v| from_mercator(v[0], v[1])).collect();
        ring.push(ring[0]);
        ring
    }

    /// The flow between `selected` and `other`, read `direction` round:
    /// out of the selection into `other`, into the selection from it, or
    /// both summed. 0 for the selection itself.
    #[must_use]
    pub fn between(&self, selected: u32, other: u32, direction: FlowDirection) -> f64 {
        if selected == other {
            return 0.0;
        }
        let find = |from: u32, to: u32| self.out_pairs.get(from as usize).and_then(|ks| ks.iter().map(|k| &self.pairs[*k as usize]).find(|p| p.to == to)).map_or(0.0, |p| p.weight);
        match direction {
            FlowDirection::Out => find(selected, other),
            FlowDirection::In => find(other, selected),
            FlowDirection::Both => find(selected, other) + find(other, selected),
        }
    }

    /// Every cell exchanging journeys with `selected`, `direction` round,
    /// with how much: `(cell, weight)`, heaviest first.
    #[must_use]
    pub fn flows_of(&self, selected: u32, direction: FlowDirection) -> Vec<(u32, f64)> {
        let mut by: HashMap<u32, f64> = HashMap::new();
        let s = selected as usize;
        if matches!(direction, FlowDirection::Out | FlowDirection::Both) {
            for k in self.out_pairs.get(s).into_iter().flatten() {
                let p = &self.pairs[*k as usize];
                *by.entry(p.to).or_insert(0.0) += p.weight;
            }
        }
        if matches!(direction, FlowDirection::In | FlowDirection::Both) {
            for k in self.in_pairs.get(s).into_iter().flatten() {
                let p = &self.pairs[*k as usize];
                *by.entry(p.from).or_insert(0.0) += p.weight;
            }
        }
        let mut out: Vec<(u32, f64)> = by.into_iter().filter(|(_, w)| *w > 0.0).collect();
        out.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// The share of all between-cell weight the heaviest `n` pairs carry.
    #[must_use]
    pub fn top_share(&self, n: usize) -> f64 {
        let all: f64 = self.pairs.iter().map(|p| p.weight).sum();
        if all > 0.0 { self.pairs.iter().take(n).map(|p| p.weight).sum::<f64>() / all } else { 0.0 }
    }
}

/// A cell radius for `points` (`[lon, lat]`): the robust extent (the 2nd
/// to 98th percentile on each axis, so a stray (0, 0) row or an airport
/// run does not set the scale), divided into about `cells_across` cells
/// along its longer side, rounded to 1, 2 or 5 times a power of ten
/// metres so the number a reader is shown is a round one.
#[must_use]
pub fn auto_radius_m(points: &[[f64; 2]], cells_across: f64) -> f64 {
    let finite: Vec<[f64; 2]> = points.iter().copied().filter(|p| p[0].is_finite() && p[1].is_finite() && p[1].abs() <= MAX_LAT).collect();
    if finite.len() < 2 {
        return 1000.0;
    }
    let pick = |axis: usize| {
        let mut v: Vec<f64> = finite.iter().map(|p| p[axis]).collect();
        v.sort_unstable_by(f64::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        (at(0.02), at(0.98))
    };
    let (lon0, lon1) = pick(0);
    let (lat0, lat1) = pick(1);
    let mid = ((lat0 + lat1) / 2.0).to_radians().cos().abs().max(0.01);
    let m_per_deg = 111_320.0;
    let w = (lon1 - lon0).abs() * m_per_deg * mid;
    let h = (lat1 - lat0).abs() * m_per_deg;
    // A hexagon of radius r is sqrt(3) r across.
    let raw = (w.max(h) / cells_across.max(1.0) / 3f64.sqrt()).max(50.0);
    let p = 10f64.powf(raw.log10().floor());
    let m = raw / p;
    let nice = if m < 1.5 {
        1.0
    } else if m < 3.5 {
        2.0
    } else if m < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[([f64; 2], [f64; 2], f64)], radius_m: f64) -> OdGrid {
        let o: Vec<[f64; 2]> = rows.iter().map(|r| r.0).collect();
        let d: Vec<[f64; 2]> = rows.iter().map(|r| r.1).collect();
        let w: Vec<f64> = rows.iter().map(|r| r.2).collect();
        OdGrid::build(&o, &d, Some(&w), OdOptions { radius_m, reference_lat: None })
    }

    /// Rows a few hundred metres apart land in one cell; their weights sum
    /// per ordered pair; a self-flow counts as internal, never as a pair.
    #[test]
    fn rows_sum_per_ordered_cell_pair() {
        let a = [-0.10, 51.50];
        let a2 = [-0.101, 51.5005];
        let b = [-2.24, 53.48];
        let g = grid(&[(a, b, 10.0), (a2, b, 5.0), (b, a, 3.0), (a, a2, 7.0), ([f64::NAN, 0.0], b, 1.0)], 5_000.0);
        assert_eq!(g.cells.len(), 2);
        assert_eq!(g.pairs.len(), 2);
        assert_eq!(g.skipped, 1);
        let ca = g.cell_at(a[0], a[1]).unwrap();
        let cb = g.cell_at(b[0], b[1]).unwrap();
        assert_eq!(g.pairs[0].weight, 15.0, "heaviest first");
        assert_eq!((g.pairs[0].from, g.pairs[0].to, g.pairs[0].rows), (ca, cb, 2));
        assert_eq!(g.cells[ca as usize].internal, 7.0);
        assert_eq!(g.cells[ca as usize].departures, 15.0);
        assert_eq!(g.cells[ca as usize].arrivals, 3.0);
        // Directional reads.
        assert_eq!(g.between(ca, cb, FlowDirection::Out), 15.0);
        assert_eq!(g.between(ca, cb, FlowDirection::In), 3.0);
        assert_eq!(g.between(ca, cb, FlowDirection::Both), 18.0);
        assert_eq!(g.between(ca, ca, FlowDirection::Both), 0.0);
        assert_eq!(g.flows_of(cb, FlowDirection::Out), vec![(ca, 3.0)]);
        assert_eq!(g.flows_of(cb, FlowDirection::In), vec![(ca, 15.0)]);
        // The centroid sits among the endpoints, not on the lattice point.
        let c = g.cells[ca as usize].centroid;
        assert!((c[0] - -0.1003).abs() < 0.001 && (c[1] - 51.5).abs() < 0.001, "{c:?}");
    }

    #[test]
    fn a_cells_hexagon_contains_its_own_points() {
        let g = grid(&[([-0.1, 51.5], [-2.2, 53.4], 1.0)], 10_000.0);
        let ring = g.hexagon(0, 1.0);
        assert_eq!(ring.len(), 7);
        let (lo, hi) = ring.iter().fold(([f64::MAX; 2], [f64::MIN; 2]), |(lo, hi), p| ([lo[0].min(p[0]), lo[1].min(p[1])], [hi[0].max(p[0]), hi[1].max(p[1])]));
        let p = [-0.1, 51.5];
        let c = g.cell_at(p[0], p[1]).unwrap();
        let ring = g.hexagon(c, 1.0);
        let (lo2, hi2) = ring.iter().fold(([f64::MAX; 2], [f64::MIN; 2]), |(lo, hi), q| ([lo[0].min(q[0]), lo[1].min(q[1])], [hi[0].max(q[0]), hi[1].max(q[1])]));
        assert!(p[0] > lo2[0] && p[0] < hi2[0] && p[1] > lo2[1] && p[1] < hi2[1]);
        assert!(hi[1] > lo[1]);
    }

    /// The id split is lossless for any 64-bit index, high bit included.
    #[test]
    fn an_h3_index_survives_the_id_split() {
        for h in [0u64, 1, 0x0880_0000_1234_5678, 0x8a1f_b46e_5a5f_ffff, u64::MAX] {
            assert_eq!(id_h3(h3_id(h)), h);
        }
    }

    /// H3 aggregation preserves the weights (every kept row's weight is in
    /// exactly one cell's departures-or-internal, and in one pair or one
    /// internal sum), and the round trip holds: a row's origin finds the
    /// cell its weight went to, that cell's index is the one h3o gives for
    /// the point at the resolution (lat/lng in the right order: London's
    /// cell is centred near 51.5 N, 0.1 W, not in the Indian Ocean), and
    /// its outline contains the point.
    #[cfg(feature = "h3")]
    #[test]
    fn h3_aggregation_preserves_the_weights_and_round_trips() {
        let london = [-0.1276, 51.5072];
        // The same place twice: two rows of one pair, and a self-flow.
        let london2 = london;
        let manchester = [-2.2426, 53.4808];
        let leeds = [-1.5491, 53.8008];
        let rows = [(london, manchester, 10.0), (london2, manchester, 5.0), (manchester, london, 3.0), (london, london2, 7.0), (leeds, london, 2.5), ([f64::NAN, 0.0], leeds, 99.0), (leeds, manchester, -1.0)];
        let o: Vec<[f64; 2]> = rows.iter().map(|r| r.0).collect();
        let d: Vec<[f64; 2]> = rows.iter().map(|r| r.1).collect();
        let w: Vec<f64> = rows.iter().map(|r| r.2).collect();
        for res in [5u8, 6, 7, 8] {
            let g = OdGrid::from_h3(&o, &d, Some(&w), res);
            assert_eq!(g.h3_resolution(), Some(res));
            assert_eq!(g.skipped, 2, "a non-finite end and a negative weight");
            let kept: f64 = rows.iter().filter(|r| r.2 >= 0.0 && r.0[0].is_finite()).map(|r| r.2).sum();
            let dep: f64 = g.cells.iter().map(|c| c.departures).sum();
            let arr: f64 = g.cells.iter().map(|c| c.arrivals).sum();
            let int: f64 = g.cells.iter().map(|c| c.internal).sum();
            let pairs: f64 = g.pairs.iter().map(|p| p.weight).sum();
            assert!((dep + int - kept).abs() < 1e-9, "r{res}: every kept weight leaves a cell or stays in it");
            assert!((dep - pairs).abs() < 1e-9 && (arr - pairs).abs() < 1e-9, "r{res}: departures = arrivals = the pairs");
            assert_eq!(g.cells.len(), 3, "r{res}: London, Manchester, Leeds");
            let res_h = h3o::Resolution::try_from(res).unwrap();
            for p in [london, manchester, leeds] {
                let c = g.cell_at(p[0], p[1]).expect("the point's cell");
                let h = g.h3_index(c).expect("an index");
                assert_eq!(h, u64::from(h3o::LatLng::new(p[1], p[0]).unwrap().to_cell(res_h)), "r{res}: the cell h3o gives (lat, lng)");
                assert_eq!(g.cell_of_h3(h), Some(c), "r{res}: index -> cell");
                let centre = g.cells[c as usize].center;
                assert!((centre[0] - p[0]).abs() < 0.5 && (centre[1] - p[1]).abs() < 0.5, "r{res}: the centre {centre:?} is near {p:?}");
                let ring = g.hexagon(c, 1.0);
                assert_eq!(ring.first(), ring.last(), "closed");
                assert!(ring.len() >= 6, "a hexagon (or a pentagon), closed: {}", ring.len());
                let (lo, hi) = ring.iter().fold(([f64::MAX; 2], [f64::MIN; 2]), |(lo, hi), q| ([lo[0].min(q[0]), lo[1].min(q[1])], [hi[0].max(q[0]), hi[1].max(q[1])]));
                assert!(p[0] > lo[0] && p[0] < hi[0] && p[1] > lo[1] && p[1] < hi[1], "r{res}: the outline holds the point");
                // A finer index resolves to the cell holding it (London's
                // r10 cell's PARENT is the neighbouring r5 cell: H3's
                // children do not tile their parent).
                let fine = u64::from(h3o::LatLng::new(p[1], p[0]).unwrap().to_cell(h3o::Resolution::Ten));
                assert_eq!(g.resolve_h3(fine), Some(c), "r{res}: a finer selection maps to its parent");
                // A coarser one: the occupied cell inside it.
                let coarse = u64::from(h3o::LatLng::new(p[1], p[0]).unwrap().to_cell(h3o::Resolution::try_from(res - 1).unwrap()));
                assert_eq!(g.resolve_h3(coarse), Some(c), "r{res}: a coarser selection maps to the occupied cell inside it");
            }
            let (cl, cm) = (g.cell_at(london[0], london[1]).unwrap(), g.cell_at(manchester[0], manchester[1]).unwrap());
            assert_eq!(g.between(cl, cm, FlowDirection::Out), 15.0);
            assert_eq!(g.between(cl, cm, FlowDirection::In), 3.0);
            assert_eq!(g.cells[cl as usize].internal, 7.0);
            assert!((g.radius_m - res_h.edge_length_m()).abs() < 1e-9);
        }
    }

    #[test]
    fn the_auto_radius_follows_the_extent_and_ignores_strays() {
        // ~England: 6 degrees of longitude at 52N, 6 of latitude.
        let mut pts: Vec<[f64; 2]> = (0..1000).map(|i| [-5.0 + 6.0 * f64::from(i % 37) / 36.0, 50.0 + 6.0 * f64::from(i % 41) / 40.0]).collect();
        let r = auto_radius_m(&pts, 32.0);
        assert!((5_000.0..=20_000.0).contains(&r), "{r}");
        pts.push([0.0, 0.0]);
        assert_eq!(auto_radius_m(&pts, 32.0), r, "one stray row moves nothing");
        // A city is two orders of magnitude finer.
        let city: Vec<[f64; 2]> = pts.iter().map(|p| [-74.0 + (p[0] + 5.0) / 60.0, 40.7 + (p[1] - 50.0) / 60.0]).collect();
        assert!(auto_radius_m(&city, 32.0) < 500.0);
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    use crate::flow::edge_path_bundling::{bundle_paths_loaded, PathBundlingParams, PathEdge};

    /// Times aggregation and bundling on a real OD CSV:
    /// `GPUI_MAP_FLOW_PROBE_CSV=path` with columns named by
    /// `GPUI_MAP_FLOW_PROBE_COLS=lon0,lat0,lon1,lat1[,weight]`. A
    /// measurement, not an assertion: run with `--release --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "timing probe on a real CSV; see the doc comment"]
    fn probe_od_csv() {
        let Ok(path) = std::env::var("GPUI_MAP_FLOW_PROBE_CSV") else { return };
        let cols = std::env::var("GPUI_MAP_FLOW_PROBE_COLS").unwrap_or_default();
        let cols: Vec<&str> = cols.split(',').collect();
        let text = std::fs::read_to_string(&path).expect("the csv");
        let mut lines = text.lines();
        let header: Vec<&str> = lines.next().expect("a header").split(',').collect();
        let ix: Vec<usize> = cols.iter().map(|c| header.iter().position(|h| h == c).unwrap_or_else(|| panic!("no column {c}"))).collect();
        let (mut o, mut d, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for l in lines {
            let f: Vec<f64> = l.split(',').map(|v| v.trim().parse().unwrap_or(f64::NAN)).collect();
            o.push([f[ix[0]], f[ix[1]]]);
            d.push([f[ix[2]], f[ix[3]]]);
            w.push(ix.get(4).map_or(1.0, |i| f[*i]));
        }
        let ends: Vec<[f64; 2]> = o.iter().chain(&d).copied().collect();
        let t = std::time::Instant::now();
        let r = std::env::var("GPUI_MAP_FLOW_PROBE_KM").ok().and_then(|v| v.parse::<f64>().ok()).map_or_else(|| auto_radius_m(&ends, 40.0), |km| km * 1000.0);
        let ns: Vec<usize> = std::env::var("GPUI_MAP_FLOW_PROBE_NS").ok().map_or_else(|| vec![500, 1000, 2000, 3000, 5000, 8000], |v| v.split(',').filter_map(|n| n.parse().ok()).collect());
        let detour: f64 = std::env::var("GPUI_MAP_FLOW_PROBE_DETOUR").ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
        let auto_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let g = OdGrid::build(&o, &d, Some(&w), OdOptions { radius_m: r, reference_lat: None });
        println!("{} rows, radius {r} m (auto in {auto_ms:.0} ms): {} cells, {} pairs, aggregated in {:.0} ms", o.len(), g.cells.len(), g.pairs.len(), t.elapsed().as_secs_f64() * 1e3);
        for n in ns {
            let pairs: Vec<&OdPair> = g.pairs.iter().take(n).collect();
            let edges: Vec<PathEdge> = pairs.iter().map(|q| PathEdge { from: g.cells[q.from as usize].centroid, to: g.cells[q.to as usize].centroid }).collect();
            let weights: Vec<f64> = pairs.iter().map(|q| q.weight).collect();
            let t = std::time::Instant::now();
            let out = bundle_paths_loaded(&edges, &weights, PathBundlingParams { samples: 32, max_detour: detour, ..Default::default() });
            let segments: usize = out.iter().map(|p| p.points.len().saturating_sub(1)).sum();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let routed = out.iter().filter(|p| p.routed).count();
            println!("top {n:5}: share {:.1}% of between-cell weight, bundled in {ms:7.0} ms, {routed} routed (detour {detour}), {segments} segments", g.top_share(n) * 100.0);
        }
    }
}
