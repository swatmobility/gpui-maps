//! Map colouring: which polygons touch, and a colour index per polygon so
//! that no two neighbours share one -- for a layer with nothing numeric to
//! choropleth (the FlatGeobuf page's US counties: name, state, FIPS), where
//! one flat colour hides every border.
//!
//! Adjacency is read off shared vertices: polygons cut from one
//! topologically clean source (a census boundary file) share their border
//! vertices exactly, so two features that share at least two of them share
//! an edge. One shared vertex alone is a corner touch (the Four Corners) and
//! does not count, as in the four-colour theorem's own statement. Vertices
//! are compared on a 1e-6 degree grid (~10 cm), which absorbs float noise
//! and nothing else.
//!
//! Colours come from DSatur (Brélaz 1979): the next polygon coloured is the
//! one whose neighbours already use the most distinct colours, ties to the
//! one with the most neighbours, and it takes the lowest index free among
//! them. On a planar map that lands in five or six colours where plain
//! greedy in file order can need eight.
//!
//! Cost on the 3,221 US counties: one pass over their vertices into a hash
//! map, then an O(n^2) DSatur -- a fraction of a second, run off the UI
//! thread.
#![allow(dead_code)] // not every binary that includes the helper uses all of it

use std::collections::{HashMap, HashSet};

/// One feature's polygons: each a ring list, exterior first.
pub type Polygons = Vec<Vec<Vec<(f64, f64)>>>;

const GRID: f64 = 1e6;

/// Each feature's neighbours (sorted, no duplicates, never itself): the
/// features it shares at least two vertices with.
#[must_use]
pub fn adjacency(features: &[Polygons]) -> Vec<Vec<u32>> {
    let mut at: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
    for (i, polygons) in features.iter().enumerate() {
        let mut seen: HashSet<(i64, i64)> = HashSet::new();
        for ring in polygons.iter().flatten() {
            for &(lon, lat) in ring {
                let key = ((lon * GRID).round() as i64, (lat * GRID).round() as i64);
                if seen.insert(key) {
                    at.entry(key).or_default().push(i as u32);
                }
            }
        }
    }
    let mut shared: HashMap<(u32, u32), u32> = HashMap::new();
    for owners in at.values().filter(|o| o.len() > 1) {
        for (a, &x) in owners.iter().enumerate() {
            for &y in &owners[a + 1..] {
                *shared.entry((x.min(y), x.max(y))).or_default() += 1;
            }
        }
    }
    let mut adj = vec![Vec::new(); features.len()];
    for ((x, y), n) in shared {
        if n >= 2 {
            adj[x as usize].push(y);
            adj[y as usize].push(x);
        }
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    adj
}

/// The graph of groups (states) whose members (counties) touch: group `g`
/// neighbours group `h` when any member of one neighbours a member of the
/// other. `group_of[i]` is feature `i`'s group, below `groups`.
#[must_use]
pub fn group_adjacency(adj: &[Vec<u32>], group_of: &[u32], groups: usize) -> Vec<Vec<u32>> {
    let mut out: Vec<HashSet<u32>> = vec![HashSet::new(); groups];
    for (i, list) in adj.iter().enumerate() {
        let g = group_of[i];
        for &j in list {
            let h = group_of[j as usize];
            if h != g {
                out[g as usize].insert(h);
            }
        }
    }
    out.into_iter()
        .map(|s| {
            let mut v: Vec<u32> = s.into_iter().collect();
            v.sort_unstable();
            v
        })
        .collect()
}

/// A colour index per node (DSatur), such that no two neighbours share
/// one. The indices are `0..k` for the smallest `k` DSatur finds; a caller
/// with a palette of `n` colours checks `k <= n` (the max of the result
/// plus one) rather than wrapping, which would put a colour on two
/// neighbours.
#[must_use]
pub fn colour(adj: &[Vec<u32>]) -> Vec<u8> {
    let n = adj.len();
    let mut colour: Vec<Option<u8>> = vec![None; n];
    // Per node: the colours its coloured neighbours use (a bit set; 64 is
    // far more than a planar map needs).
    let mut used: Vec<u64> = vec![0; n];
    for _ in 0..n {
        let next = (0..n)
            .filter(|&i| colour[i].is_none())
            .max_by_key(|&i| (used[i].count_ones(), adj[i].len(), std::cmp::Reverse(i)))
            .expect("an uncoloured node");
        let c = (!used[next]).trailing_zeros().min(63) as u8;
        colour[next] = Some(c);
        for &j in &adj[next] {
            used[j as usize] |= 1 << c;
        }
    }
    colour.into_iter().map(|c| c.unwrap_or(0)).collect()
}

/// Whether `colours` gives no two neighbours the same index.
#[must_use]
pub fn is_proper(adj: &[Vec<u32>], colours: &[u8]) -> bool {
    adj.iter().enumerate().all(|(i, list)| list.iter().all(|&j| colours[i] != colours[j as usize]))
}

#[cfg(test)]
mod tests {
    use super::{adjacency, colour, group_adjacency, is_proper, Polygons};

    /// An `n x n` grid of unit squares, each its own feature, sharing their
    /// edges' vertices exactly.
    fn grid(n: usize) -> Vec<Polygons> {
        let mut out = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let (x, y) = (x as f64, y as f64);
                out.push(vec![vec![vec![(x, y), (x + 1.0, y), (x + 1.0, y + 1.0), (x, y + 1.0), (x, y)]]]);
            }
        }
        out
    }

    #[test]
    fn a_grid_reads_edge_neighbours_not_corner_touches() {
        let adj = adjacency(&grid(3));
        // The centre square: four edge neighbours, not the four diagonal
        // corners it touches at one vertex each.
        assert_eq!(adj[4], vec![1, 3, 5, 7]);
        assert_eq!(adj[0], vec![1, 3]);
    }

    #[test]
    fn no_two_neighbours_share_a_colour_on_a_grid() {
        let adj = adjacency(&grid(12));
        let colours = colour(&adj);
        assert!(is_proper(&adj, &colours));
        // A grid is bipartite: DSatur finds the two colours.
        assert_eq!(colours.iter().max(), Some(&1));
    }

    #[test]
    fn groups_of_a_grid_are_coloured_apart_too() {
        // Three vertical stripes of a 6x6 grid as three "states".
        let adj = adjacency(&grid(6));
        let group_of: Vec<u32> = (0..36).map(|i| ((i % 6) / 2) as u32).collect();
        let groups = group_adjacency(&adj, &group_of, 3);
        assert_eq!(groups, vec![vec![1], vec![0, 2], vec![1]]);
        let c = colour(&groups);
        assert!(is_proper(&groups, &c));
    }
}
