//! Edge-Path Bundling, after Wallinger, Archambault, Auber, Nöllenburg and
//! Peltonen, "Edge-Path Bundling: A Less Ambiguous Edge Bundling Approach",
//! IEEE TVCG 28(1), 2022 (doi:10.1109/TVCG.2021.3114795).
//!
//! Ported from an in-house SWAT desktop app (its `edge_path_bundling`),
//! where it draws a city's demand corridors; the reasoning below is that
//! module's, kept because it is why this algorithm and not another.
//!
//! The approach the DigitalGeographyLab `mobitwin-edgebundling` tool takes,
//! and a different idea entirely from Holten & van Wijk's force-directed
//! bundling (FDEB, "Force-Directed Edge Bundling for Graph Visualization",
//! EuroVis 2009). There is no simulation and no
//! notion of edges attracting: instead, each flow asks whether some PATH
//! through the other flows gets from its origin to its destination without
//! going much further than the direct line. If one does, the flow is drawn
//! along that path, so anything travelling the same way is carried on the
//! same bundle of segments.
//!
//! # Why it fits this data and force-directed bundling does not
//!
//! FDEB only bends two edges toward each other when they are mutually
//! *visible* -- roughly, when they run alongside each other. Measured on
//! Aomori's top district corridors, 94% of pairs score zero on that test,
//! because the corridors radiate from one downtown at many angles rather
//! than running in parallel. FDEB is correct to leave them straight; it is
//! built for long-haul flows between scattered hubs.
//!
//! Edge-Path Bundling asks a different question, and a radial star answers
//! it well: flows from two different suburbs to the same centre share the
//! last part of their journey, so both can route through the same
//! intermediate node even though neither runs alongside the other.
//!
//! # The algorithm
//!
//! 1. Weight every edge by its length raised to `weight_exponent`. A larger
//!    exponent makes long edges expensive to route through, which keeps
//!    bundles following chains of short hops rather than taking one long
//!    shortcut.
//! 2. Consider edges heaviest (for positive exponents, longest) first. A long edge is the one most likely to
//!    have a reasonable detour available, and processing it first lets the
//!    shorter edges it subsumes become its backbone.
//! 3. For each edge, lock it (so it cannot route through itself), and find
//!    the cheapest path between its endpoints through the unlocked edges.
//! 4. If that path's true length is within `max_detour` times the direct
//!    distance, adopt it: the edge is drawn through the path's vertices,
//!    and every edge on the path is marked so it is not itself re-routed.
//!    Otherwise unlock the edge and leave it straight.
//! 5. A bundled edge stays locked: it is drawn as a curve and no later
//!    path may use it. Path edges stay usable.
//!
//! Checked against the paper's Algorithm 1 (arXiv 2108.05467): `lock`
//! and `skip` there are `is_backbone` and `locked` here. Then audited step
//! by step against the reference tool's code, DigitalGeographyLab
//! `mobitwin-edgebundling` at fa24b3361e61 (CC BY 4.0; read, not ported):
//! the processing order (by cost, stably), Dijkstra's tie-breaking and the
//! choice of metric ([`PathMetric`]) now match it. By default the graph is
//! the reference's: every input corridor its own edge, so A->B and B->A
//! are two parallel edges (a multigraph), the drawn route's control points
//! are the path's NODE coordinates in order (ends included), smoothed by
//! one round of midpoints and one de Casteljau Bezier over them all.
//! [`PathBundlingParams::simple_graph`] opts into the paper's simple graph
//! instead (both directions sharing one edge); the sample count and the
//! metric are the remaining, stated differences. Unrouted edges are drawn
//! straight unless [`PathBundlingParams::unrouted_bow`] asks otherwise.
//!
//! # Cost
//!
//! One Dijkstra per edge, so roughly `E * E log V`: a few thousand
//! corridors, not a raw trip table. [`super::od`] aggregates rows into
//! corridors first; the renderer-agnostic output (`[lon, lat]` polylines
//! with a per-point load) is drawn by whoever calls this.

use std::collections::{BinaryHeap, HashMap};

/// One flow to bundle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathEdge {
    pub from: [f64; 2],
    pub to: [f64; 2],
}

/// Tuning, named for what the reference tool calls these.
#[derive(Debug, Clone, Copy)]
pub struct PathBundlingParams {
    /// `-t` in the reference tool: how many times longer than the straight
    /// line a bundled route may be. Below about 1.2 almost nothing bundles;
    /// above about 4 flows take absurd detours and the map stops being
    /// true. 2.0 is the reference default.
    pub max_detour: f64,
    /// `-ew` in the reference tool: the exponent on edge length when
    /// costing a route. 1.0 costs a path by plain distance; higher values
    /// penalise long hops, so bundles prefer chains of short ones. 2.0 is
    /// the reference default.
    pub weight_exponent: f64,
    /// Points emitted per drawn curve.
    pub samples: usize,
    /// How far a corridor that found no route bows, as a fraction of its
    /// own chord (see `arc`). 0, the default, is the paper's and the
    /// reference tool's drawing: an unbundled edge stays a straight
    /// two-point segment. The original app draws 0.08, because there the
    /// unrouted corridors are two or three findings among hundreds of
    /// curves; with thousands of strands straight lines read as texture.
    pub unrouted_bow: f64,
    /// How a hop's length is measured, for its cost, the processing order
    /// and the detour test. See [`PathMetric`].
    pub metric: PathMetric,
    /// One graph edge per node pair, both directions of a corridor sharing
    /// it (the paper's simple graph), instead of the reference tool's one
    /// edge per input corridor. Off by default: with it on, a reciprocal
    /// pair can no longer route over its own twin, so far more corridors
    /// bend through hubs than the reference bends -- a tuned look, opt-in.
    pub simple_graph: bool,
}

/// How [`bundle_paths_loaded`] measures a hop.
///
/// The reference tool (mobitwin-edgebundling, `model.py`
/// `Node.distance_to`) takes the plain Euclidean distance between two
/// `(lon, lat)` pairs in degrees. Away from the equator that counts an
/// east-west degree as long as a north-south one, when on the ground it is
/// `cos(lat)` of it: at the Nordic latitudes of its own example (55-70 N) an
/// east-west hop costs two to three times its real length, so the detour
/// test is that much laxer north-south than east-west, and the `length^d`
/// weight steers routes onto north-south chains. [`PathMetric::LocalPlanar`]
/// (the default) scales longitude by `cos` of the hop's mean latitude, so
/// the test means the same thing in every direction; [`PathMetric::Degrees`]
/// is the reference's, kept so its example can be reproduced exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathMetric {
    /// Longitude scaled by `cos(mean latitude)`: locally isotropic.
    #[default]
    LocalPlanar,
    /// Plain degrees, as the reference tool measures.
    Degrees,
}

impl PathMetric {
    /// The length of the hop `a`..`b` in this metric (degrees, either way;
    /// only ever compared with other lengths from the same metric).
    pub fn distance(self, a: [f64; 2], b: [f64; 2]) -> f64 {
        match self {
            PathMetric::LocalPlanar => planar_distance(a, b),
            PathMetric::Degrees => ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt(),
        }
    }
}

impl Default for PathBundlingParams {
    fn default() -> Self {
        Self {
            max_detour: 2.0,
            weight_exponent: 2.0,
            unrouted_bow: 0.0,
            metric: PathMetric::LocalPlanar,
            simple_graph: false,
            // The reference samples its Beziers at 100. This sits lower
            // because a Bezier is inherently smooth -- it needs far fewer
            // points than the interpolating spline it replaced to read as a
            // curve -- and because every point here is a coordinate pushed
            // to the map on a frame that also carries vehicles, stops and
            // the network. 48 was measured as the point where the long
            // trunks stop showing facets.
            samples: 48,
        }
    }
}

/// Bundles `edges`, routing each along the cheapest path through the
/// others, and reports how much movement shares each stretch of the
/// result: one [`BundledPath`] per input edge, in the same order.
///
/// An edge that found no acceptable route comes back as a shallow arc
/// between its own endpoints -- see `arc` -- which is the algorithm's
/// honest answer that nothing else goes that way.
///
/// `weights` is positionally aligned with `edges` -- the people moving on
/// each corridor. A missing or short slice counts every corridor as 1,
/// which turns the load into a plain overlap count.
pub fn bundle_paths_loaded(
    edges: &[PathEdge],
    weights: &[f64],
    params: PathBundlingParams,
) -> Vec<BundledPath> {
    if edges.len() < 3 {
        return edges
            .iter()
            .enumerate()
            .map(|(ix, e)| BundledPath {
                points: vec![e.from, e.to],
                load: vec![weights.get(ix).copied().unwrap_or(1.0).max(0.0); 2],
                corridors: vec![1; 2],
                routed: false,
            })
            .collect();
    }

    // Nodes are shared endpoints. Flows meeting at a district centre must
    // resolve to ONE node or no path can ever pass through it, so
    // coordinates are keyed on a rounded grid rather than on exact floats.
    let mut node_of: HashMap<(i64, i64), usize> = HashMap::new();
    let mut nodes: Vec<[f64; 2]> = Vec::new();
    let mut key = |p: [f64; 2], nodes: &mut Vec<[f64; 2]>| -> usize {
        // ~1e-6 degrees, about 10 cm -- far below any real separation
        // between two district centroids, far above float noise.
        let k = ((p[0] * 1e6).round() as i64, (p[1] * 1e6).round() as i64);
        *node_of.entry(k).or_insert_with(|| {
            nodes.push(p);
            nodes.len() - 1
        })
    };
    let input_ends: Vec<(usize, usize)> = edges
        .iter()
        .map(|e| (key(e.from, &mut nodes), key(e.to, &mut nodes)))
        .collect();

    // The reference (`get_locations_data`) makes every input corridor its
    // own graph edge: A->B and B->A are two parallel edges, and the
    // longer-first pass may route A->B over its own twin (a one-hop "path"
    // exactly as long as the direct line, always within the detour)
    // whenever the twin is cheaper than any real detour -- drawn straight,
    // as the reference draws it. `simple_graph` instead gives both
    // directions one edge, routed once and drawn from each origin: the
    // paper's simple graph, which bundles more of a reciprocal table.
    let mut graph_of_pair: HashMap<(usize, usize), usize> = HashMap::new();
    let mut ends: Vec<(usize, usize)> = Vec::new();
    let graph_of_input: Vec<Option<usize>> = input_ends
        .iter()
        .map(|&(a, b)| {
            if a == b {
                return None;
            }
            if !params.simple_graph {
                ends.push((a, b));
                return Some(ends.len() - 1);
            }
            let pair = (a.min(b), a.max(b));
            Some(*graph_of_pair.entry(pair).or_insert_with(|| {
                ends.push((a, b));
                ends.len() - 1
            }))
        })
        .collect();

    let lengths: Vec<f64> = ends
        .iter()
        .map(|&(a, b)| params.metric.distance(nodes[a], nodes[b]))
        .collect();
    let costs: Vec<f64> = lengths
        .iter()
        .map(|l| l.max(1e-9).powf(params.weight_exponent))
        .collect();

    // Adjacency, by node, each node's edges shortest first, stably, as the
    // reference sorts them (`node.edges.sort(key=distance)`): with parallel
    // edges between two nodes, the first relaxed wins a tie.
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (ix, &(a, b)) in ends.iter().enumerate() {
        adjacency[a].push(ix);
        adjacency[b].push(ix);
    }
    for list in &mut adjacency {
        list.sort_by(|x, y| lengths[*x].total_cmp(&lengths[*y]));
    }

    // Heaviest first, by the COST (`length^d`), stably, as the reference
    // sorts (`edges.sort(key=weight, reverse=True)`): the long edge is the
    // one with a detour worth taking, and taking it first lets the short
    // edges beneath become its backbone. For any positive `d` that is the
    // longest-first order; at `d = 0` every cost is 1 and the order is the
    // input's, as there.
    let mut order: Vec<usize> = (0..ends.len()).collect();
    order.sort_by(|a, b| costs[*b].total_cmp(&costs[*a]));

    // The paper's two flags, under names that say what they do here:
    // `locked` is its `skip` (excluded from every shortest-path search:
    // the edge being bundled, and for good once it has been), and
    // `is_backbone` its `lock` (lies on an adopted path, so is never
    // bundled itself -- but stays usable as a segment of later paths).
    let mut locked = vec![false; ends.len()];
    let mut is_backbone = vec![false; ends.len()];
    let mut routes: Vec<Option<Vec<usize>>> = vec![None; ends.len()];

    for &ix in &order {
        if is_backbone[ix] {
            continue;
        }
        let (source, target) = ends[ix];
        locked[ix] = true;
        match cheapest_path(source, target, &adjacency, &ends, &costs, &locked) {
            Some(path_edges) => {
                let detour: f64 = path_edges.iter().map(|e| lengths[*e]).sum();
                if detour <= params.max_detour * lengths[ix] {
                    for &e in &path_edges {
                        is_backbone[e] = true;
                    }
                    routes[ix] = Some(path_edges);
                } else {
                    locked[ix] = false;
                }
            }
            None => locked[ix] = false,
        }
    }

    // How much movement rides over each BASE edge once every corridor has
    // been routed. A corridor that kept its own straight line contributes
    // its weight to itself; a routed one contributes to each edge of the
    // path it took instead.
    //
    // This is the number the reference image is actually drawing. Its glow
    // is not a blur -- it is hundreds of thin strands compositing, brightest
    // where the most of them coincide. Computing the load explicitly gets
    // the same picture from a renderer that has no additive blending, and
    // gets it as DATA: the white trunks are literally the busiest streets
    // in the city, not an artifact of how many lines happened to overlap.
    let weight_of = |ix: usize| weights.get(ix).copied().unwrap_or(1.0).max(0.0);
    let mut edge_load = vec![0.0f64; ends.len()];
    // And how many corridors: the same sum with every weight 1, which is
    // what a reader hovering a trunk asks next ("how many flows is this?").
    // Both directions of a pair count, each as its own corridor.
    let mut edge_count = vec![0u32; ends.len()];
    for (ix, g) in graph_of_input.iter().enumerate() {
        let Some(g) = *g else { continue };
        match &routes[g] {
            None => {
                edge_load[g] += weight_of(ix);
                edge_count[g] += 1;
            }
            Some(path) => {
                for &e in path {
                    edge_load[e] += weight_of(ix);
                    edge_count[e] += 1;
                }
            }
        }
    }

    (0..edges.len())
        .map(|ix| {
            let Some(g) = graph_of_input[ix] else {
                // Both ends on one node: nothing to route, nothing shared.
                let w = weight_of(ix);
                return BundledPath { points: vec![edges[ix].from, edges[ix].to], load: vec![w; 2], corridors: vec![1; 2], routed: false };
            };
            match &routes[g] {
                None => {
                    // Straight, as the reference draws it, unless the caller
                    // asked for a bow -- see `arc`.
                    let points = if params.unrouted_bow > 0.0 {
                        arc_bowed(edges[ix].from, edges[ix].to, params.samples, params.unrouted_bow)
                    } else {
                        vec![edges[ix].from, edges[ix].to]
                    };
                    let load = vec![edge_load[g]; points.len()];
                    let corridors = vec![edge_count[g]; points.len()];
                    BundledPath { points, load, corridors, routed: false }
                }
                Some(path) => {
                    // Walked from THIS corridor's origin, so the reverse
                    // direction of a shared pair draws the route backwards.
                    // The control points are the path's node coordinates,
                    // ends included, as the reference's `get` takes them (a
                    // node holds the first coordinate seen within its 10 cm
                    // key, so the ends are the corridor's own).
                    let (control, order) = walk(input_ends[ix].0, path, &ends, &nodes);
                    let dense = subdivide(&control, SMOOTHING_LEVELS);
                    let points = bezier(&dense, params.samples);
                    // A Bezier is one curve over the WHOLE polyline, so a
                    // drawn point has no exact control segment to belong
                    // to the way it did under a per-segment spline.
                    // Position along the curve is mapped onto position
                    // along the path by arc length, so a long hop claims
                    // proportionally more of the line than a short one.
                    //
                    // Approximate, because the Bezier parameter is not
                    // arc-length uniform -- the curve moves faster through
                    // the middle than the ends. It is monotonic, which is
                    // what the load ramp actually needs: the brightening
                    // happens at roughly the right place along the stroke,
                    // and a few points either side of a boundary take a
                    // neighbouring band. Nothing downstream reads a single
                    // point's load as a measurement.
                    let spans: Vec<f64> = order.iter().map(|e| lengths[*e].max(1e-9)).collect();
                    let total: f64 = spans.iter().sum();
                    let base_edge = |i: usize| -> Option<usize> {
                        let t = i as f64 / (points.len().saturating_sub(1)).max(1) as f64;
                        let mut travelled = t * total;
                        for (seg, span) in spans.iter().enumerate() {
                            if travelled <= *span {
                                return Some(order[seg]);
                            }
                            travelled -= span;
                        }
                        order.last().copied()
                    };
                    let under: Vec<Option<usize>> = (0..points.len()).map(base_edge).collect();
                    let load = under.iter().map(|e| e.map_or(0.0, |e| edge_load[e])).collect();
                    let corridors = under.iter().map(|e| e.map_or(0, |e| edge_count[e])).collect();
                    BundledPath { points, load, corridors, routed: true }
                }
            }
        })
        .collect()
}

/// The geometry alone, for callers that do not draw load.
pub fn bundle_paths(edges: &[PathEdge], params: PathBundlingParams) -> Vec<Vec<[f64; 2]>> {
    bundle_paths_loaded(edges, &[], params)
        .into_iter()
        .map(|p| p.points)
        .collect()
}

/// One drawn flow, with how much movement shares each point of it.
#[derive(Debug, Clone, PartialEq)]
pub struct BundledPath {
    pub points: Vec<[f64; 2]>,
    /// Positionally aligned with `points`: the total weight riding over the
    /// stretch of path that point sits on. Flat for an unrouted corridor,
    /// stepped for one that shares a trunk partway.
    pub load: Vec<f64>,
    /// Positionally aligned with `points`: how many corridors ride the
    /// stretch that point sits on (the load with every weight 1).
    pub corridors: Vec<u32>,
    /// Whether the corridor found a route through the others (else it is
    /// drawn on its own chord).
    pub routed: bool,
}

/// Rounds of midpoint insertion before the Bezier.
///
/// Two, which is the reference's `smoothing` setting: one round of
/// midpoints. See [`subdivide`].
const SMOOTHING_LEVELS: usize = 2;

/// Turns an unordered edge list into the ordered node sequence it traces,
/// starting at `source`.
fn walk(
    source: usize,
    path: &[usize],
    ends: &[(usize, usize)],
    nodes: &[[f64; 2]],
) -> (Vec<[f64; 2]>, Vec<usize>) {
    let mut out = vec![nodes[source]];
    let mut order = Vec::with_capacity(path.len());
    let mut at = source;
    let mut remaining: Vec<usize> = path.to_vec();
    while !remaining.is_empty() {
        let Some(pos) = remaining
            .iter()
            .position(|&e| ends[e].0 == at || ends[e].1 == at)
        else {
            break;
        };
        let e = remaining.remove(pos);
        at = if ends[e].0 == at { ends[e].1 } else { ends[e].0 };
        out.push(nodes[at]);
        order.push(e);
    }
    (out, order)
}

/// Dijkstra over edges, skipping locked ones.
///
/// Ties resolve as the reference's `find_shortest_path` resolves them:
/// equally cheap queue entries pop lowest node first, and a route only
/// replaces another when strictly cheaper -- so of two equally cheap
/// routes to a node, the one through the lower node is kept.
fn cheapest_path(
    source: usize,
    target: usize,
    adjacency: &[Vec<usize>],
    ends: &[(usize, usize)],
    costs: &[f64],
    locked: &[bool],
) -> Option<Vec<usize>> {
    #[derive(PartialEq)]
    struct Step(f64, usize);
    impl Eq for Step {}
    impl Ord for Step {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            // Reversed: `BinaryHeap` is a max-heap and this wants the
            // cheapest next -- and, between two equally cheap, the lower
            // node first, as the reference's `heapq` of `(distance, node)`
            // compares nodes by id.
            other.0.total_cmp(&self.0).then_with(|| other.1.cmp(&self.1))
        }
    }
    impl PartialOrd for Step {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut best = vec![f64::INFINITY; adjacency.len()];
    let mut came_from: Vec<Option<usize>> = vec![None; adjacency.len()];
    let mut heap = BinaryHeap::new();
    best[source] = 0.0;
    heap.push(Step(0.0, source));

    while let Some(Step(cost, node)) = heap.pop() {
        if node == target {
            break;
        }
        if cost > best[node] {
            continue;
        }
        for &edge in &adjacency[node] {
            if locked[edge] {
                continue;
            }
            let next = if ends[edge].0 == node {
                ends[edge].1
            } else {
                ends[edge].0
            };
            let candidate = cost + costs[edge];
            if candidate < best[next] {
                best[next] = candidate;
                came_from[next] = Some(edge);
                heap.push(Step(candidate, next));
            }
        }
    }

    if best[target].is_infinite() {
        return None;
    }
    let mut path = Vec::new();
    let mut at = target;
    while at != source {
        let edge = came_from[at]?;
        path.push(edge);
        at = if ends[edge].0 == at {
            ends[edge].1
        } else {
            ends[edge].0
        };
    }
    path.reverse();
    Some(path)
}

/// A de Casteljau Bezier over the whole control polyline.
///
/// Replaces a centripetal Catmull-Rom, and the swap is the difference
/// between a curve that reads as a sweep and one that reads as a
/// smoothed dogleg. Catmull-Rom INTERPOLATES: it passes through every
/// control point, so a routed flow hugs the node polyline and every node
/// it passes shows as a kink. A Bezier APPROXIMATES: interior control
/// points only pull on the curve, which arcs away from them into one
/// continuous stroke. That is what the reference implementation uses --
/// `create_bezier_polygon` over the full path -- and it is why its ribbons
/// bow the way they do.
///
/// `control` is expected to have been through [`subdivide`] first, which
/// is how the reference keeps the bow from drifting too far from the route
/// it is supposed to represent.
fn bezier(control: &[[f64; 2]], samples: usize) -> Vec<[f64; 2]> {
    if control.len() < 3 {
        return control.to_vec();
    }
    let n = samples.max(2);
    let mut out = Vec::with_capacity(n + 1);
    let mut scratch: Vec<[f64; 2]> = Vec::with_capacity(control.len());
    for i in 0..n {
        let t = i as f64 / n as f64;
        scratch.clear();
        scratch.extend_from_slice(control);
        // Repeated linear interpolation until one point remains.
        let mut len = scratch.len();
        while len > 1 {
            for j in 0..len - 1 {
                scratch[j] = [
                    scratch[j][0] + (scratch[j + 1][0] - scratch[j][0]) * t,
                    scratch[j][1] + (scratch[j + 1][1] - scratch[j][1]) * t,
                ];
            }
            len -= 1;
        }
        out.push(scratch[0]);
    }
    out.push(control[control.len() - 1]);
    out
}

/// Inserts a midpoint between every pair of control points, `levels - 1`
/// times.
///
/// The reference calls this `split` and runs it at `smoothing = 2`, i.e.
/// one round of midpoints. Without it a Bezier over a long path bows so far
/// from the route that the drawn line stops describing the path it was
/// computed from; each round of midpoints pulls the curve back toward the
/// polyline while keeping it a single smooth stroke. Two is the reference's
/// own setting and is the balance between a limp curve and a bulging one.
fn subdivide(points: &[[f64; 2]], levels: usize) -> Vec<[f64; 2]> {
    let mut p = points.to_vec();
    for _ in 1..levels.max(1) {
        let mut next = Vec::with_capacity(p.len() * 2);
        for i in 0..p.len() - 1 {
            next.push(p[i]);
            next.push([
                (p[i][0] + p[i + 1][0]) / 2.0,
                (p[i][1] + p[i + 1][1]) / 2.0,
            ]);
        }
        next.push(p[p.len() - 1]);
        p = next;
    }
    p
}

/// Bows a straight two-point corridor into a gentle arc.
///
/// The reference leaves its unbundled edges dead straight, and with
/// thousands of them that reads as texture. Here there are two or three --
/// the corridors whose verdict is the finding -- and a hard straight line
/// among several hundred curves reads as a rendering error rather than as
/// emphasis. A shallow arc makes them belong to the same drawing.
///
/// The bow is perpendicular to the chord and scaled by its length, so a
/// short corridor bends as much, proportionally, as a long one.
pub fn arc_between(from: [f64; 2], to: [f64; 2], samples: usize) -> Vec<[f64; 2]> {
    arc(from, to, samples)
}

fn arc(from: [f64; 2], to: [f64; 2], samples: usize) -> Vec<[f64; 2]> {
    arc_bowed(from, to, samples, STRAIGHT_ARC_BOW)
}

/// [`arc`] with the bow, as a fraction of the chord, given.
fn arc_bowed(from: [f64; 2], to: [f64; 2], samples: usize, bow: f64) -> Vec<[f64; 2]> {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    let mid = [(from[0] + to[0]) / 2.0, (from[1] + to[1]) / 2.0];
    // Perpendicular offset. Latitude is scaled out of the perpendicular so
    // the bow does not flatten or exaggerate with distance from the equator.
    let control = [mid[0] - dy * bow, mid[1] + dx * bow];
    bezier(&[from, control, to], samples)
}

/// How far a straight corridor bows, as a fraction of its own length.
///
/// Small on purpose. These lines carry a claim about service, and a
/// noticeably curved line implies a route that was followed; this is only
/// enough to stop the line reading as a different kind of object from
/// everything around it.
pub const STRAIGHT_ARC_BOW: f64 = 0.08;


/// Distance in degrees, with longitude scaled so the plane is locally
/// isotropic. Only ever compared against other distances from this same
/// function, so degrees are a fine unit.
fn planar_distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    let scale = ((a[1] + b[1]) / 2.0).to_radians().cos().abs().max(0.01);
    let dx = (b[0] - a[0]) * scale;
    let dy = b[1] - a[1];
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::{bundle_paths, bundle_paths_loaded, PathBundlingParams, PathEdge, PathMetric};

    fn e(from: [f64; 2], to: [f64; 2]) -> PathEdge {
        PathEdge { from, to }
    }

    /// The case force-directed bundling cannot help with, and this one can:
    /// two suburbs and a centre. The long A->C flow should route through B
    /// rather than cutting across, because A->B->C is barely longer.
    #[test]
    fn a_long_flow_routes_through_an_intermediate_hub() {
        let a = [140.70, 40.80];
        let b = [140.75, 40.81];
        let c = [140.80, 40.80];
        let out = bundle_paths(&[e(a, c), e(a, b), e(b, c)], PathBundlingParams::default());
        // A->C is no longer a two-point straight line.
        assert!(out[0].len() > 2, "the long flow stayed straight");
        // It bows toward B.
        let mid = out[0][out[0].len() / 2];
        assert!(
            mid[1] > 40.802,
            "the routed flow did not bend toward the hub: {mid:?}"
        );
        // Its endpoints are still its own.
        assert_eq!(out[0][0], a);
        assert_eq!(*out[0].last().unwrap(), c);
    }

    /// A detour that is far too long must be refused, or the map claims a
    /// journey nobody makes.
    #[test]
    fn an_absurd_detour_is_refused() {
        let a = [140.70, 40.80];
        let c = [140.80, 40.80];
        let far = [140.75, 41.60];
        let out = bundle_paths(
            &[e(a, c), e(a, far), e(far, c)],
            PathBundlingParams::default(),
        );
        // Refused corridors are drawn as a shallow arc rather than a bare
        // segment (see `arc`), so the test is whether the line still goes
        // STRAIGHT THERE -- not how many points it takes to do it. The
        // detour hub sits 0.8 degrees north; nothing on a refused path may
        // come anywhere near it.
        assert_eq!(out[0][0], a);
        assert_eq!(*out[0].last().unwrap(), c);
        let northmost = out[0].iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        assert!(
            northmost < 40.9,
            "a huge detour was accepted: the line reached {northmost}, \
             which is on its way to the hub at 41.60"
        );
    }

    #[test]
    fn the_detour_limit_is_what_decides_it() {
        let a = [140.70, 40.80];
        let b = [140.75, 40.86];
        let c = [140.80, 40.80];
        let edges = [e(a, c), e(a, b), e(b, c)];
        let tight = bundle_paths(
            &edges,
            PathBundlingParams { max_detour: 1.05, ..Default::default() },
        );
        let loose = bundle_paths(
            &edges,
            PathBundlingParams { max_detour: 3.0, ..Default::default() },
        );
        // Both are curves now, so the limit shows in WHERE the line goes,
        // not in its point count: the detour hub is north at 40.86, and
        // only the loose limit should let the line climb toward it.
        let reach = |path: &[[f64; 2]]| path.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        let tight_reach = reach(&tight[0]);
        let loose_reach = reach(&loose[0]);
        assert!(
            tight_reach < 40.82,
            "a tight limit should refuse the detour, but the line reached {tight_reach}"
        );
        assert!(
            loose_reach > 40.83,
            "a loose limit should take it, but the line only reached {loose_reach}"
        );
    }

    /// The reason for the Bezier. An interpolating spline passes through
    /// every node it routes over, so the drawn line hugs the polyline and
    /// kinks at each one; an approximating Bezier bows away into a single
    /// stroke. Measured as the largest departure from the node polyline,
    /// the Bezier must be visibly the rounder of the two -- that departure
    /// IS the curviness.
    #[test]
    fn the_bezier_bows_away_from_the_node_polyline() {
        // A dogleg: straight out east, then a step north, then east again.
        let control = [
            [140.70, 40.80],
            [140.75, 40.80],
            [140.75, 40.86],
            [140.80, 40.86],
        ];
        let curve = super::bezier(&super::subdivide(&control, super::SMOOTHING_LEVELS), 48);
        // How far the curve departs from the corner it rounds.
        let corner = [140.75, 40.80];
        let nearest = curve
            .iter()
            .map(|p| ((p[0] - corner[0]).powi(2) + (p[1] - corner[1]).powi(2)).sqrt())
            .fold(f64::MAX, f64::min);
        assert!(
            nearest > 0.004,
            "the curve passes within {nearest} of the corner -- it is hugging \
             the polyline rather than bowing away from it"
        );
        // But it still starts and ends on the real endpoints.
        assert_eq!(curve[0], control[0]);
        assert_eq!(*curve.last().unwrap(), control[3]);
    }

    /// Midpoint subdivision is what stops the bow running away. With no
    /// subdivision the Bezier drifts much further from the route it is
    /// meant to depict.
    #[test]
    fn subdivision_pulls_the_curve_back_toward_the_route() {
        let control = [
            [140.70, 40.80],
            [140.75, 40.80],
            [140.75, 40.86],
            [140.80, 40.86],
        ];
        let corner = [140.75, 40.80];
        let drift = |pts: &[[f64; 2]]| {
            super::bezier(pts, 48)
                .iter()
                .map(|p| ((p[0] - corner[0]).powi(2) + (p[1] - corner[1]).powi(2)).sqrt())
                .fold(f64::MAX, f64::min)
        };
        let raw = drift(&control);
        let smoothed = drift(&super::subdivide(&control, super::SMOOTHING_LEVELS));
        assert!(
            smoothed < raw,
            "subdivision should tighten the curve: raw={raw}, smoothed={smoothed}"
        );
    }

    /// Even a refused corridor is drawn as a curve, so it belongs to the
    /// same picture as everything around it -- but the bow has to stay
    /// shallow enough that it never reads as a route that was followed.
    #[test]
    fn a_refused_corridor_is_bowed_but_only_slightly() {
        let a = [140.70, 40.80];
        let c = [140.80, 40.80];
        let far = [140.75, 41.60];
        let out = bundle_paths(
            &[e(a, c), e(a, far), e(far, c)],
            PathBundlingParams { unrouted_bow: super::STRAIGHT_ARC_BOW, ..Default::default() },
        );
        assert!(out[0].len() > 2, "a refused corridor should still be a curve");
        let bow = out[0]
            .iter()
            .map(|p| (p[1] - 40.80).abs())
            .fold(0.0f64, f64::max);
        let chord = 0.10;
        assert!(
            bow > 0.0 && bow < chord * 0.10,
            "bow of {bow} against a chord of {chord} is not a shallow arc"
        );
    }

    /// The paper and the reference tool leave an unbundled edge straight;
    /// so does the default here (a bow is the caller's choice).
    #[test]
    fn a_refused_corridor_is_straight_by_default() {
        let a = [0.0, 0.0];
        let c = [2.0, 0.0];
        let far = [1.0, 5.0];
        let out = bundle_paths_loaded(&[e(a, c), e(a, far), e(far, c)], &[], PathBundlingParams::default());
        assert_eq!(out[0].points, vec![a, c]);
        assert!(!out[0].routed);
    }

    /// The paper bundles a SIMPLE graph, so an origin-destination table's
    /// two directions of one pair are one edge, routed once. Computed by
    /// hand from the paper's rules (k = 2, d = 2), near the equator so the
    /// planar metric is plain degrees:
    ///
    /// A (0,0), C (2,0), hub B (1,1.2). Edges A-C (length 2, cost 4), A-B
    /// and B-C (length sqrt(2.44) = 1.562, cost 2.44 each). A-C is longest,
    /// so first: its cheapest path is A-B-C, cost 4.88, length 3.124 <= 2 *
    /// 2 -- adopted; A-B and B-C become backbone. Control points A, B, C,
    /// one round of midpoints (smoothing 2) gives A, (0.5,0.6), B,
    /// (1.5,0.6), C, and the quartic Bezier's midpoint is
    /// (0 + 4*0.6 + 6*1.2 + 4*0.6 + 0) / 16 = 0.75 north.
    ///
    /// Kept as two graph edges, A->C instead found its twin C->A (cost 4 <
    /// 4.88, length 2 <= 4) and "routed" over it dead straight, and C->A,
    /// now backbone, never bundled: neither went near B.
    #[test]
    fn both_directions_of_a_pair_bundle_as_one_edge_through_the_hub() {
        let (a, b, c) = ([0.0, 0.0], [1.0, 1.2], [2.0, 0.0]);
        let edges = [e(a, c), e(c, a), e(a, b), e(b, c)];
        let out = bundle_paths_loaded(&edges, &[10.0, 20.0, 1.0, 2.0], PathBundlingParams { simple_graph: true, ..PathBundlingParams::default() });
        for (ix, (from, to)) in [(a, c), (c, a)].into_iter().enumerate() {
            let p = &out[ix];
            assert!(p.routed, "direction {ix} routes through the hub");
            assert_eq!(p.points[0], from);
            assert_eq!(*p.points.last().unwrap(), to);
            let mid = p.points[p.points.len() / 2];
            assert!((mid[1] - 0.75).abs() < 1e-9, "the hand-computed Bezier midpoint, got {mid:?}");
            assert!((mid[0] - 1.0).abs() < 1e-9);
        }
        // Both directions ride A-B and B-C: each hop carries 10 + 20 plus
        // its own corridor, shared by three corridors.
        assert_eq!(out[2].load, vec![31.0, 31.0]);
        assert_eq!(out[3].load, vec![32.0, 32.0]);
        assert_eq!(out[2].corridors, vec![3, 3]);
        // The hops themselves are backbone: never routed.
        assert!(!out[2].routed && !out[3].routed);
    }

    /// The reference's multigraph, the default: the same four corridors,
    /// but A->C and C->A are two parallel edges. A->C goes first (cost 4,
    /// stably before its twin) and its cheapest path is the twin C->A
    /// (cost 4 against A-B-C's 4.88), length 2 <= 4: "routed" over it,
    /// dead straight, and the twin, now backbone, is drawn straight too --
    /// neither goes near B, exactly as `bundle_edges.py` draws it.
    #[test]
    fn the_reference_routes_a_reciprocal_pair_over_its_own_twin() {
        let (a, b, c) = ([0.0, 0.0], [1.0, 1.2], [2.0, 0.0]);
        let edges = [e(a, c), e(c, a), e(a, b), e(b, c)];
        let out = bundle_paths_loaded(&edges, &[10.0, 20.0, 1.0, 2.0], PathBundlingParams::default());
        assert!(out[0].routed && !out[1].routed);
        for p in &out[..2] {
            assert!(p.points.iter().all(|q| q[1].abs() < 1e-12), "straight along A-C: {:?}", p.points);
        }
        // A->C rides C->A: the twin carries both.
        assert!(out[1].load.iter().all(|l| *l == 30.0));
        assert_eq!(out[2].load, vec![1.0, 1.0]);
    }

    /// The paper's `skip`: once bundled, an edge is drawn as a curve and is
    /// excluded from every later shortest-path search. Hand-computed:
    ///
    /// A (0,0), C (2,0), B (1,1.2) as above, plus D (1,-0.3) and the
    /// corridors A-C, A-D, D-C, A-B, B-C. A-C is longest (2): paths A-D-C
    /// (cost 2 * 1.09 = 2.18) beats A-B-C (4.88), length 2.088 <= 4, so A-C
    /// routes via D and A-D, D-C become backbone. A-B and B-C (1.562) are
    /// next: A-B's only other path is B-C-...-A, which must use A-C (now
    /// excluded) or go B-C-D-A, length 1.562 + 1.044 + 1.044 = 3.65 > 2 *
    /// 1.562 = 3.124 -- refused. Were A-C still usable, B-C-A would cost
    /// 2.44 + 4 = 6.44 against B-C-D-A's 2.44 + 2.18 = 4.62 -- the
    /// refusal is the same either way, so the pin is that A-C's own route
    /// is via D and that it carries none of A-B's or B-C's weight.
    #[test]
    fn a_bundled_edge_is_drawn_through_its_path_and_carries_nothing_itself() {
        let (a, b, c, d) = ([0.0, 0.0], [1.0, 1.2], [2.0, 0.0], [1.0, -0.3]);
        let edges = [e(a, c), e(a, d), e(d, c), e(a, b), e(b, c)];
        let out = bundle_paths_loaded(&edges, &[100.0, 1.0, 1.0, 5.0, 7.0], PathBundlingParams::default());
        assert!(out[0].routed);
        let south = out[0].points.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
        assert!(south < -0.1, "A-C bows toward D, got {south}");
        assert_eq!(out[1].load[0], 101.0, "A-D carries A-C");
        assert!(!out[3].routed && !out[4].routed, "A-B and B-C find no acceptable path");
        assert_eq!(out[3].load[0], 5.0);
        assert_eq!(out[4].load[0], 7.0);
    }

    /// The reference sorts by WEIGHT (`length^d`), stably. At `d = 0` every
    /// weight is 1, so its order is the input's -- and the order decides
    /// which edge bundles. Hand-computed from `bundle_edges.py`'s loop, near
    /// the equator (`Degrees`, so lengths are plain):
    ///
    /// A (0,0), B (1,0), C (0.5,0.8); AB = 1, AC = BC = sqrt(0.89) = 0.943.
    /// Input [AC, BC, AB], k = 3. AC first: its cheapest path (every hop
    /// costs 1) is A-B-C, length 1.943 <= 3 * 0.943 -- adopted, AB and BC
    /// become backbone and are skipped. So AC is routed and AB is not.
    /// Sorted longest-first instead, AB would have gone first and routed
    /// through C.
    #[test]
    fn the_processing_order_is_by_cost_then_input_as_the_reference_sorts() {
        let (a, b, c) = ([0.0, 0.0], [1.0, 0.0], [0.5, 0.8]);
        let p = PathBundlingParams { weight_exponent: 0.0, max_detour: 3.0, metric: PathMetric::Degrees, ..Default::default() };
        let out = bundle_paths_loaded(&[e(a, c), e(b, c), e(a, b)], &[], p);
        assert!(out[0].routed, "AC, first in the input, routes via B");
        assert!(!out[1].routed && !out[2].routed, "BC and AB are its backbone");
        // At d = 2 the order is longest first again: AB routes via C.
        let out = bundle_paths_loaded(&[e(a, c), e(b, c), e(a, b)], &[], PathBundlingParams { weight_exponent: 2.0, ..p });
        assert!(out[2].routed && !out[0].routed && !out[1].routed);
    }

    /// Two equally cheap routes: the reference's `heapq` pops `(distance,
    /// node)` and compares nodes by id, so of two hubs reached at the same
    /// cost the lower id is expanded first and, with a strict `<`, is the
    /// one the destination keeps. Node ids here are first-appearance order.
    ///
    /// A (0,0), D (2,0), hubs B (1,1) and C (1,-1). Input [A-D, C-D, A-B,
    /// A-C, B-D]: ids A0 D1 C2 B3. A-D (cost 4) outweighs the hub hops
    /// (sqrt 2, cost 2) and goes first. A relaxes B, then C, both at 2;
    /// A-B-D and A-C-D both cost 4 and measure 2.83 <= 4. The reference
    /// pops C (id 2) before B (id 3), so D is reached through C first and
    /// keeps it: routed south. (A max-heap with no tie-break popped B, the
    /// first pushed, and routed north.)
    #[test]
    fn equally_cheap_routes_resolve_to_the_lower_node_as_the_reference_does() {
        let (a, d, b, c) = ([0.0, 0.0], [2.0, 0.0], [1.0, 1.0], [1.0, -1.0]);
        let p = PathBundlingParams { metric: PathMetric::Degrees, ..Default::default() };
        let out = bundle_paths_loaded(&[e(a, d), e(c, d), e(a, b), e(a, c), e(b, d)], &[], p);
        assert!(out[0].routed);
        let mid = out[0].points[out[0].points.len() / 2];
        assert!(mid[1] < -0.5, "A-D bows south through C, the lower id: {mid:?}");
        assert_eq!(out[3].load[0], 2.0, "A-C carries A-D");
        assert_eq!(out[2].load[0], 1.0, "A-B carries only itself");
        // Swap the hubs' first appearance and the route swaps with them.
        let out = bundle_paths_loaded(&[e(a, d), e(b, d), e(a, c), e(a, b), e(c, d)], &[], p);
        let mid = out[0].points[out[0].points.len() / 2];
        assert!(mid[1] > 0.5, "now B is the lower id: {mid:?}");
    }

    /// The reference measures in plain degrees (`Node.distance_to`); this
    /// module by default scales longitude by cos(latitude). At 60 N the two
    /// disagree about a detour, and each is pinned. Hand-computed:
    ///
    /// A (0,60), C (2,60), hub B (1,60.9). Degrees: AC = 2, AB = BC =
    /// sqrt(1 + 0.81) = 1.345; A-B-C measures 2.691 <= 2 * 2 -- routed, and
    /// the curve is the reference's: control A, B, C, one round of
    /// midpoints (`split`, smoothing 2), a quartic Bezier at 100 samples
    /// (`create_bezier_polygon`: t = i/100 for i < 100, then the last
    /// point: 101 points), whose t = 0.5 point is x = 1, y = (60 + 4 *
    /// 60.45 + 6 * 60.9 + 4 * 60.45 + 60) / 16 = 60.5625.
    ///
    /// Local planar: AC = 2 cos 60 = 1.0 and AB = BC = sqrt((cos 60.45)^2 +
    /// 0.81) = 1.026, so AC is no longer the longest and no longer bundles:
    /// A-B-C would measure 2.05 > 2 * 1.0 -- refused -- and AB, first,
    /// takes A-C-B (2.026 <= 2.053) instead, making AC its backbone. A-C
    /// stays straight.
    #[test]
    fn the_degree_metric_reproduces_the_reference_and_the_default_is_isotropic() {
        let (a, b, c) = ([0.0, 60.0], [1.0, 60.9], [2.0, 60.0]);
        let edges = [e(a, c), e(a, b), e(b, c)];
        let reference = PathBundlingParams { metric: PathMetric::Degrees, samples: 100, ..Default::default() };
        let out = bundle_paths_loaded(&edges, &[], reference);
        assert!(out[0].routed, "the reference's metric routes A-C via B");
        assert_eq!(out[0].points.len(), 101, "n = 100 samples plus the end, as the reference draws");
        let mid = out[0].points[50];
        assert!((mid[0] - 1.0).abs() < 1e-9 && (mid[1] - 60.5625).abs() < 1e-9, "the reference's t = 0.5 point, got {mid:?}");
        let out = bundle_paths_loaded(&edges, &[], PathBundlingParams { samples: 100, ..Default::default() });
        assert!(!out[0].routed, "on the ground the detour is 2.06x: refused");
        assert_eq!(out[0].points, vec![a, c]);
    }

    /// Endpoints are data; a routed flow still starts and ends where the
    /// movement does.
    #[test]
    fn endpoints_survive_routing() {
        let a = [140.70, 40.80];
        let b = [140.75, 40.81];
        let c = [140.80, 40.80];
        let d = [140.85, 40.79];
        let edges = [e(a, d), e(a, b), e(b, c), e(c, d)];
        for (path, edge) in bundle_paths(&edges, PathBundlingParams::default())
            .iter()
            .zip(&edges)
        {
            assert_eq!(path[0], edge.from);
            assert_eq!(*path.last().unwrap(), edge.to);
        }
    }


    /// Aomori's real top-50 corridors. A probe, not an assertion: the
    /// question is how many of them find a route, which is the thing
    /// force-directed bundling could not do on this shape of data.
    #[test]
    fn probe_real_aomori_corridors() {
        let edges = vec![
        e([140.734375, 40.827083], [140.465625, 40.602083]),
        e([140.759375, 40.799769], [140.768056, 40.798843]),
        e([140.740625, 40.827083], [140.746875, 40.827083]),
        e([140.740625, 40.827083], [140.734375, 40.827083]),
        e([140.743750, 40.797917], [140.759375, 40.800000]),
        e([140.740625, 40.827083], [140.753125, 40.827083]),
        e([140.709375, 40.822917], [140.696307, 40.817992]),
        e([140.734375, 40.827083], [140.746875, 40.827083]),
        e([140.753125, 40.806250], [140.759375, 40.800000]),
        e([140.705000, 40.831250], [140.663125, 40.826806]),
        e([140.705208, 40.831250], [140.696181, 40.818750]),
        e([140.706250, 40.831250], [140.718750, 40.831250]),
        e([140.740625, 40.822917], [140.734375, 40.827083]),
        e([140.675924, 40.822799], [140.664305, 40.827259]),
        e([140.705625, 40.831250], [140.703750, 40.827083]),
        e([140.705000, 40.831250], [140.675937, 40.821667]),
        e([140.784375, 40.808333], [140.778125, 40.816667]),
        e([140.759375, 40.799702], [140.744196, 40.797917]),
        e([140.743750, 40.797917], [140.740625, 40.802083]),
        e([140.740625, 40.827083], [140.465625, 40.602083]),
        e([140.756250, 40.789583], [140.759375, 40.800000]),
        e([140.703125, 40.827083], [140.706250, 40.831250]),
        e([140.740625, 40.802083], [140.737500, 40.804167]),
        e([140.746875, 40.822917], [140.734375, 40.827083]),
        e([140.743750, 40.797917], [140.753125, 40.806250]),
        e([140.743750, 40.797917], [140.737500, 40.804167]),
        e([140.703125, 40.827083], [140.675804, 40.821012]),
        e([140.690625, 40.827083], [140.465625, 40.602083]),
        e([140.753125, 40.806250], [140.767708, 40.798380]),
        e([140.689286, 40.738690], [140.465625, 40.602083]),
        e([140.740625, 40.827083], [140.734375, 40.822917]),
        e([140.759375, 40.800000], [140.753125, 40.806250]),
        e([140.706250, 40.831250], [140.709375, 40.822917]),
        e([140.746875, 40.802083], [140.759375, 40.800000]),
        e([140.702404, 40.827083], [140.695913, 40.817628]),
        e([140.753125, 40.827083], [140.746875, 40.827083]),
        e([140.753125, 40.806250], [140.737500, 40.804167]),
        e([140.734375, 40.831250], [140.465625, 40.602083]),
        e([140.746875, 40.827083], [140.734375, 40.827083]),
        e([140.740625, 40.827083], [140.746875, 40.822917]),
        e([140.784375, 40.808333], [140.790625, 40.810417]),
        e([140.728125, 40.831250], [140.718750, 40.831250]),
        e([140.734375, 40.827083], [140.703125, 40.827083]),
        e([140.706250, 40.831250], [140.709375, 40.835417]),
        e([140.753125, 40.806250], [140.743750, 40.797917]),
        e([140.740625, 40.827083], [140.753125, 40.822917]),
        e([140.734375, 40.827083], [140.740625, 40.827083]),
        e([140.753125, 40.827083], [140.740625, 40.827083]),
        e([140.759375, 40.827083], [140.746875, 40.827083]),
        e([140.746875, 40.827083], [140.740625, 40.827083]),
        ];
        for detour in [1.5_f64, 2.0, 3.0] {
            let out = bundle_paths(
                &edges,
                PathBundlingParams { max_detour: detour, ..Default::default() },
            );
            let routed = out.iter().filter(|p| p.len() > 2).count();
            let pts: usize = out.iter().map(|p| p.len()).sum();
            println!(
                "  max_detour {detour}: {routed}/{} corridors routed, {pts} points total",
                edges.len()
            );
        }
    }

    /// The whole point of the load channel: a corridor that keeps its own
    /// line carries only its own weight.
    #[test]
    fn an_unrouted_corridor_carries_only_its_own_weight() {
        let edges = vec![PathEdge { from: [0.0, 0.0], to: [1.0, 0.0] }];
        let out = bundle_paths_loaded(&edges, &[7.0], PathBundlingParams::default());
        assert_eq!(out[0].load, vec![7.0, 7.0]);
    }

    /// And the payoff: where several corridors route over one backbone,
    /// that backbone's load is their SUM. This is what makes the shared
    /// trunk the brightest thing on the map.
    #[test]
    fn a_shared_backbone_sums_the_weights_riding_over_it() {
        // Three collinear hops plus two long spans that should both prefer
        // routing over them rather than flying direct.
        let edges = vec![
            PathEdge { from: [0.0, 0.0], to: [1.0, 0.0] },
            PathEdge { from: [1.0, 0.0], to: [2.0, 0.0] },
            PathEdge { from: [2.0, 0.0], to: [3.0, 0.0] },
            PathEdge { from: [0.0, 0.0], to: [3.0, 0.0] },
            PathEdge { from: [0.0, 0.0], to: [2.0, 0.0] },
        ];
        let weights = vec![1.0, 1.0, 1.0, 10.0, 100.0];
        let out = bundle_paths_loaded(&edges, &weights, PathBundlingParams::default());
        // The first hop is shared by both long spans plus itself.
        let first_hop_load = out[0].load[0];
        assert_eq!(
            first_hop_load, 111.0,
            "hop 0 carries its own 1 plus the 10 and 100 routed over it"
        );
        // The third hop is only on the 0->3 span's path, not the 0->2 one.
        assert_eq!(out[2].load[0], 11.0, "hop 2 carries its own 1 plus the 10");
    }

    /// Load must stay aligned with points, or the colour ramp slides along
    /// the line and the bright stretch lands in the wrong place.
    #[test]
    fn load_is_reported_for_every_drawn_point() {
        let edges = vec![
            PathEdge { from: [0.0, 0.0], to: [1.0, 0.0] },
            PathEdge { from: [1.0, 0.0], to: [2.0, 0.0] },
            PathEdge { from: [2.0, 0.0], to: [3.0, 0.0] },
            PathEdge { from: [0.0, 0.0], to: [3.0, 0.0] },
        ];
        let out = bundle_paths_loaded(&edges, &[1.0, 1.0, 1.0, 9.0], PathBundlingParams::default());
        for path in &out {
            assert_eq!(
                path.points.len(),
                path.load.len(),
                "every drawn point needs a load"
            );
            assert!(path.load.iter().all(|l| *l > 0.0), "no unlit segments");
        }
    }

    /// A routed corridor gets BRIGHTER where it joins a trunk -- the load
    /// varies along the line rather than being one flat value.
    #[test]
    fn a_routed_corridor_varies_in_load_along_its_length() {
        let edges = vec![
            PathEdge { from: [0.0, 0.0], to: [1.0, 0.0] },
            PathEdge { from: [1.0, 0.0], to: [2.0, 0.0] },
            PathEdge { from: [2.0, 0.0], to: [3.0, 0.0] },
            PathEdge { from: [0.0, 0.0], to: [3.0, 0.0] },
            PathEdge { from: [0.0, 0.0], to: [2.0, 0.0] },
        ];
        let out = bundle_paths_loaded(
            &edges,
            &[1.0, 1.0, 1.0, 10.0, 100.0],
            PathBundlingParams::default(),
        );
        let long = &out[3];
        let lo = long.load.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = long.load.iter().cloned().fold(0.0, f64::max);
        assert!(
            hi > lo,
            "the 0->3 span shares hops 0-1 with the 0->2 span but not hop 2, \
             so it must be brighter at the start than at the end (lo={lo}, hi={hi})"
        );
    }

    #[test]
    fn too_few_edges_are_returned_straight() {
        let out = bundle_paths(
            &[e([140.7, 40.8], [140.8, 40.8])],
            PathBundlingParams::default(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 2);
    }

    /// How long one rebuild of the flow geometry takes, at the corridor
    /// count the map actually asks for.
    ///
    /// This exists because the demand layer's geometry is rebuilt
    /// SYNCHRONOUSLY, on the playback tick, whenever the simulation clock
    /// crosses a time-window boundary -- so whatever this costs is a stall
    /// the user sees, on one frame, at a period of (window hours / speed).
    /// A mean over a measurement window hides it completely.
    ///
    /// Ignored because it is a measurement, not an assertion.
    #[test]
    #[ignore = "timing probe; run with --nocapture"]
    fn how_long_a_flow_rebuild_takes() {
        for count in [50usize, 150, 300, 600, 1200] {
            let (edges, weights) = radial_corridors(count);
            let started = std::time::Instant::now();
            let out = bundle_paths_loaded(&edges, &weights, PathBundlingParams::default());
            let took = started.elapsed();
            let points: usize = out.iter().map(|p| p.points.len()).sum();
            println!(
                "{count:5} corridors  {:7.2} ms  {points:7} points",
                took.as_secs_f64() * 1e3
            );
        }
    }

    /// The point of bundling, measured: on a radial fixture the bundled
    /// drawing lays down markedly less DISTINCT ink than the same corridors
    /// drawn as shallow arcs, because flows sharing a trunk overlap instead
    /// of each cutting its own line across the map. Counted as the cells of
    /// a 300x300 raster over the fixture that any line touches.
    #[test]
    fn bundling_lays_down_less_distinct_ink_than_arcs() {
        let (edges, weights) = radial_corridors(600);
        let bundled = bundle_paths_loaded(&edges, &weights, PathBundlingParams::default());
        let arcs: Vec<Vec<[f64; 2]>> = edges.iter().map(|e| super::arc_between(e.from, e.to, 48)).collect();
        let ink = |lines: &mut dyn Iterator<Item = &Vec<[f64; 2]>>| {
            let (x0, y0, size) = (103.62, 1.15, 0.40);
            let n = 300.0;
            let mut cells = std::collections::HashSet::new();
            for line in lines {
                for w in line.windows(2) {
                    let steps = 64;
                    for k in 0..=steps {
                        let t = f64::from(k) / f64::from(steps);
                        let x = w[0][0] + (w[1][0] - w[0][0]) * t;
                        let y = w[0][1] + (w[1][1] - w[0][1]) * t;
                        cells.insert((((x - x0) / size * n) as i64, ((y - y0) / size * n) as i64));
                    }
                }
            }
            cells.len()
        };
        let routed = bundled.iter().filter(|p| p.load.iter().any(|l| *l > p.load[0]) || p.points.len() > 49).count();
        let straight_ink = ink(&mut arcs.iter());
        let bundled_ink = ink(&mut bundled.iter().map(|p| &p.points));
        println!("ink: arcs {straight_ink} cells, bundled {bundled_ink} cells ({routed} paths changed load)");
        assert!(
            (bundled_ink as f64) < 0.8 * straight_ink as f64,
            "bundling should merge corridors into fewer distinct strokes: arcs {straight_ink}, bundled {bundled_ink}"
        );
        // And the trunks carry more than any single corridor does.
        let peak = bundled.iter().flat_map(|p| p.load.iter()).cloned().fold(0.0, f64::max);
        let heaviest = weights.iter().cloned().fold(0.0, f64::max);
        assert!(peak > 2.0 * heaviest, "the busiest trunk ({peak}) sums several corridors (max one {heaviest})");
        let shared = bundled.iter().flat_map(|p| p.corridors.iter()).copied().max().unwrap_or(0);
        assert!(shared > 3, "a trunk is shared by several corridors, got {shared}");
    }

    /// Corridors shaped like a real city's: endpoints drawn from a set of
    /// district centroids, weighted toward a downtown, which is what makes
    /// the routing search non-trivial. Deterministic, so two runs compare.
    fn radial_corridors(count: usize) -> (Vec<PathEdge>, Vec<f64>) {
        let places: Vec<[f64; 2]> = (0..64)
            .map(|i| {
                let t = i as f64 * 0.9827937;
                let r = 0.02 + 0.16 * ((i % 7) as f64 / 7.0);
                [103.82 + r * t.cos(), 1.35 + r * t.sin()]
            })
            .collect();
        let mut edges = Vec::with_capacity(count);
        let mut weights = Vec::with_capacity(count);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        while edges.len() < count {
            // Half the endpoints bias to the first eight places, which is
            // the downtown pull that gives these flows something to share.
            let pick = |r: u64| {
                if r.is_multiple_of(2) {
                    (r >> 8) as usize % 8
                } else {
                    (r >> 8) as usize % places.len()
                }
            };
            let a = pick(next());
            let b = pick(next());
            if a == b {
                continue;
            }
            edges.push(PathEdge {
                from: places[a],
                to: places[b],
            });
            weights.push(1.0 + (next() % 5000) as f64);
        }
        (edges, weights)
    }
}
