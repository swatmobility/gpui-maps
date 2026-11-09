//! H3 hexagon tiles for the race map's hexagon view (spec
//! `2026-09-27-us-race-dot-map-design.md` §4): each hexagon coloured by its
//! largest group, with its 8-group breakdown behind a click.
//!
//! - [`HexRecord`] is one hexagon: its resolution, H3 index, the 8 group
//!   counts and its boundary, **baked in** as lon/lat vertices (5 to 10 of
//!   them: H3 cells on an icosahedron edge are distorted and gain vertices,
//!   and r5 and r7 cells of that kind cross North America). The shells
//!   therefore need no `h3o`.
//! - [`codec`] is the wire format of one hex tile (`HXT1`), sans-IO and
//!   hardened like `dotgrid::codec_cat`: hex tiles reach the web and iOS
//!   shells over the network.
//! - [`source`] holds the two [`HexSource`]s: a PMTiles archive
//!   (`race-hex-2020.pmtiles`, written by `tools/race-build export-hex`)
//!   and, native only, ClickHouse's `gpui_map_dots.race_hex_2020`.
//!
//! **Where the < 10 filter lives (Walker's rule, [`HEX_MIN_TOTAL`]).** In
//! the export: [`codec::encode_tile`] folds every hexagon under 10 people
//! into its tile's `hidden` counts instead of writing a record, so an
//! archive still carries every person of every resolution (the export
//! checks 331,449,281 per resolution, hidden people included) while the
//! hidden hexagons cost no bytes. The ClickHouse source filters in its SQL
//! (`total >= 10`). Both decoders refuse a record under 10 as corrupt, so
//! what reaches a layer never needs filtering again.
//!
//! **Tiles.** Each resolution is fetched at one tile zoom
//! ([`HEX_TILE_ZOOM`], from Walker's windows); a hexagon belongs to the
//! tile holding its centre, so a tile's hexagons may overhang its edges by
//! up to one hexagon. The five zooms are distinct, so one archive keys all
//! five resolutions by plain `z/x/y`.

pub mod codec;
pub mod source;

use crate::geo::TileId;
use crate::overlay::dotgrid::{GridError, GridRequest};

/// Groups per hexagon: the race map's 8 (`dotgrid::composition::GROUPS`).
pub const GROUPS: usize = crate::overlay::dotgrid::composition::GROUPS;

/// `(H3 resolution, tile zoom)`: each resolution's tiles are cut at one
/// zoom, from Walker's windows (spec §4.2): r4 at z3, r5 at z5, r6 at z7,
/// r7 at z9, r8 at z10 (overzoomed to z12.75).
pub const HEX_TILE_ZOOM: [(u8, u8); 5] = [(4, 3), (5, 5), (6, 7), (7, 9), (8, 10)];

/// Hexagons with fewer people than this are not drawn (Walker's filter,
/// `point_count >= 10`). See this module's doc for where it is applied.
pub const HEX_MIN_TOTAL: u32 = 10;

/// The tile zoom of resolution `res`'s tiles, or `None` for a resolution
/// the race map does not draw.
pub fn hex_tile_zoom(res: u8) -> Option<u8> {
    HEX_TILE_ZOOM.iter().find(|(r, _)| *r == res).map(|(_, z)| *z)
}

/// The resolution whose tiles are cut at zoom `z` (the inverse of
/// [`hex_tile_zoom`]; the five zooms are distinct).
pub fn res_for_tile_zoom(z: u8) -> Option<u8> {
    HEX_TILE_ZOOM.iter().find(|(_, tz)| *tz == z).map(|(r, _)| *r)
}

/// The tile owning a hexagon of resolution `res` whose centre is at
/// `(lon, lat)`: the tile at [`hex_tile_zoom`] holding the centre.
pub fn owning_tile(res: u8, lon: f64, lat: f64) -> Option<TileId> {
    hex_tile_zoom(res).map(|z| crate::geo::tile_id_at(lon, lat, z))
}

/// One H3 hexagon: its resolution and index, the people of each of the 8
/// groups in it, and its boundary as `[lon, lat]` vertices in degrees, in
/// H3's order (counter-clockwise), not closed (the first vertex is not
/// repeated). 5 to 10 vertices ([`codec::MIN_VERTICES`]..=[`codec::MAX_VERTICES`]).
///
/// Longitudes are unwrapped around the cell's centre, so a cell crossing
/// the antimeridian (the Aleutians) has vertices past ±180° rather than a
/// polygon spanning the world.
#[derive(Debug, Clone, PartialEq)]
pub struct HexRecord {
    pub res: u8,
    pub h3: u64,
    pub counts: [u32; GROUPS],
    pub boundary: Vec<[f64; 2]>,
}

/// A hexagon's largest group, or a tie when two or more groups share the
/// maximum (spec §4.1: "two or more groups share the maximum"). Ties are
/// drawn `#D9D9D9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Group(u8),
    Tie,
}

impl HexRecord {
    /// Every person in the hexagon. Saturates rather than wraps: a decoded
    /// record's counts are checked to sum within `u32`.
    pub fn total(&self) -> u32 {
        self.counts.iter().fold(0u32, |a, &c| a.saturating_add(c))
    }

    /// The largest group, or [`Modal::Tie`] when the maximum is shared (an
    /// all-zero record, which no source emits, is a tie of all 8).
    pub fn modal(&self) -> Modal {
        let max = self.counts.iter().copied().max().unwrap_or(0);
        let mut at = self.counts.iter().enumerate().filter(|(_, &c)| c == max).map(|(g, _)| g as u8);
        match (at.next(), at.next()) {
            (Some(g), None) => Modal::Group(g),
            _ => Modal::Tie,
        }
    }
}

/// One source of hex tiles -- the PMTiles archive or ClickHouse --
/// described sans-IO, like `dotgrid::GridCountSource`: `request` says what
/// bytes are needed, the shell fetches them (`dotgrid::fetch_native::execute`
/// on native), and `decode` turns them into records.
///
/// `tile` must be at `hex_tile_zoom(res)`; anything else is
/// [`GridRequest::Absent`] (and decodes to nothing). `decode` takes the
/// same `res` and `tile` as the request did: a PMTiles tile's own header
/// must name them (a mis-served tile is an error, not hexagons in the
/// wrong place), and ClickHouse's rows are cut to the hexagons whose
/// centre the tile holds.
pub trait HexSource: Send + Sync {
    fn request(&self, res: u8, tile: TileId) -> GridRequest;
    fn decode(&self, res: u8, tile: TileId, body: &[u8]) -> Result<Vec<HexRecord>, GridError>;
    /// The archive's own credit (its metadata `attribution`), for the map's
    /// attribution (`MapView::set_source_credit`). `None` by default.
    fn attribution(&self) -> Option<&str> {
        None
    }
}

/// The record of H3 cell `h3` with `counts`: its resolution and its
/// boundary from `h3o`, longitudes unwrapped around the centre (see
/// [`HexRecord`]). Behind the `h3` feature: the converter and the native
/// ClickHouse source bake boundaries with this; the shells never do.
#[cfg(feature = "h3")]
pub fn record_for_cell(h3: u64, counts: [u32; GROUPS]) -> Result<HexRecord, GridError> {
    let cell = h3o::CellIndex::try_from(h3).map_err(|e| GridError::Server(format!("{h3:#x} is not an H3 cell: {e}")))?;
    let centre = h3o::LatLng::from(cell);
    let boundary = cell
        .boundary()
        .iter()
        .map(|ll| {
            let mut lon = ll.lng();
            while lon - centre.lng() > 180.0 {
                lon -= 360.0;
            }
            while lon - centre.lng() < -180.0 {
                lon += 360.0;
            }
            [lon, ll.lat()]
        })
        .collect();
    Ok(HexRecord { res: u8::from(cell.resolution()), h3, counts, boundary })
}

/// The centre `(lon, lat)` of H3 cell `h3`, from `h3o`.
#[cfg(feature = "h3")]
pub fn cell_centre(h3: u64) -> Result<(f64, f64), GridError> {
    let cell = h3o::CellIndex::try_from(h3).map_err(|e| GridError::Server(format!("{h3:#x} is not an H3 cell: {e}")))?;
    let c = h3o::LatLng::from(cell);
    Ok((c.lng(), c.lat()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(c: [u32; GROUPS]) -> HexRecord {
        HexRecord { res: 7, h3: 0, counts: c, boundary: vec![[0.0; 2]; 6] }
    }

    #[test]
    fn modal_is_the_largest_group_and_ties_are_ties() {
        assert_eq!(r([1, 5, 2, 0, 0, 0, 0, 0]).modal(), Modal::Group(1));
        assert_eq!(r([5, 5, 2, 0, 0, 0, 0, 0]).modal(), Modal::Tie);
        assert_eq!(r([0, 0, 0, 0, 0, 0, 0, 3]).modal(), Modal::Group(7));
        assert_eq!(r([3, 0, 0, 0, 0, 0, 0, 3]).modal(), Modal::Tie);
        assert_eq!(r([0; GROUPS]).modal(), Modal::Tie);
    }

    #[test]
    fn total_sums_the_groups() {
        assert_eq!(r([6, 4, 0, 0, 0, 0, 0, 1]).total(), 11);
        assert_eq!(r([u32::MAX, 1, 0, 0, 0, 0, 0, 0]).total(), u32::MAX);
    }

    #[test]
    fn tile_zooms_are_walkers_and_distinct() {
        assert_eq!(HEX_TILE_ZOOM, [(4, 3), (5, 5), (6, 7), (7, 9), (8, 10)]);
        assert_eq!(HEX_MIN_TOTAL, 10);
        for (res, z) in HEX_TILE_ZOOM {
            assert_eq!(hex_tile_zoom(res), Some(z));
            assert_eq!(res_for_tile_zoom(z), Some(res));
        }
        assert_eq!(hex_tile_zoom(9), None);
        assert_eq!(res_for_tile_zoom(4), None);
        assert_eq!(owning_tile(7, -73.97, 40.78), Some(TileId { z: 9, x: 150, y: 192 }));
    }

    /// C18: a distorted cell on an icosahedron edge has more than 6
    /// vertices. 608661216683884543 is an r7 cell near Springfield, IL
    /// (3,256 people), 8 vertices by ClickHouse's `h3ToGeoBoundary` too.
    #[cfg(feature = "h3")]
    #[test]
    fn a_distorted_cell_keeps_all_its_vertices() {
        let rec = record_for_cell(608_661_216_683_884_543, [10, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(rec.res, 7);
        assert_eq!(rec.boundary.len(), 8);
    }

    /// A cell across the antimeridian keeps its vertices on one side.
    #[cfg(feature = "h3")]
    #[test]
    fn an_antimeridian_cell_is_unwrapped_around_its_centre() {
        let cell = h3o::LatLng::new(51.8, 179.999).unwrap().to_cell(h3o::Resolution::Four);
        let rec = record_for_cell(u64::from(cell), [10, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let (lon, _) = cell_centre(rec.h3).unwrap();
        assert!(rec.boundary.iter().all(|v| (v[0] - lon).abs() < 1.0), "{:?} around {lon}", rec.boundary);
        assert!(rec.boundary.iter().any(|v| v[0].abs() > 180.0) || lon.abs() < 179.0, "{:?}", rec.boundary);
    }
}
