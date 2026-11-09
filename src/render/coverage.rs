//! The import-coverage overlay: one translucent rectangle per region of a
//! data corpus, coloured by how completely that region has been imported.
//!
//! # What this answers, and what it deliberately does not
//!
//! Its sibling [`crate::render::debug_tiles`] reports RENDERER state -- is
//! this tile's fetch in flight, did it arrive, is a stand-in being drawn in
//! its place. That is the right question when a map is slow or torn, and
//! the wrong question entirely when a map is *thin*: over ground nobody has
//! imported, every tile fetch still succeeds (the ocean and landcover
//! layers render something for any coordinate inside the extract), so a
//! renderer-side view reads uniformly healthy across a country that is
//! three-quarters empty.
//!
//! This overlay answers the other question -- "is the information in this
//! area actually filled in" -- and it cannot be derived in the renderer at
//! all. The source of truth is the import pipeline's own database plus the
//! list of regions the corpus contains; the difference between the two (the
//! regions the database has never heard of) is the whole point, and nothing
//! reachable from a tile request knows it. So the answer is computed
//! offline and handed in: a separate ETL writes the JSON [`parse_coverage`]
//! reads.
//!
//! # Why the geometry is plain lon/lat here
//!
//! The generator applies whatever grid convention its corpus uses (for
//! Japan: a primary mesh code `NNMM` puts its south-west corner at
//! `NN / 1.5` degrees north, `MM + 100` degrees east, spanning one degree
//! of longitude by forty minutes of latitude). What arrives here is a list
//! of lon/lat rectangles with a status, so this module knows nothing about
//! Japanese mesh codes and a second corpus with a different grid needs no
//! change to it.
//!
//! # Why this is not built on the overlay/stroke path
//!
//! Same reason as `debug_tiles`, and more acutely: these rectangles are a
//! DEGREE across, which is exactly the large-coordinate regime
//! `docs/superpowers/specs/2026-08-25-overlay-stroke-precision-design.md`
//! describes, where lyon's stroke normals collapse to zero and the outline
//! draws at no width at all. Nothing here is tessellated -- the four
//! corners are projected to screen pixels on the CPU in f64 and the fill
//! and border quads are built from those pixels -- so the failure mode is
//! not avoided by tuning, it is structurally absent.

use crate::render::debug_tiles::DebugRgba;
use crate::render::Camera;

/// How completely one region has been imported.
///
/// The four states come from the generator, which reads them off the
/// pipeline's own database. They are ordered by "how much is there", which
/// is also the legend's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageStatus {
    /// Roads and buildings both present.
    Full,
    /// Roads, but no buildings yet. The building ingest runs region by
    /// region, so this is the state a corpus passes through rather than an
    /// error -- and it is why the overlay does not simply say
    /// imported/not-imported.
    Partial,
    /// The region is in the database but produced no features. Rare and
    /// legitimate: a mesh that is one lake has nothing else to import.
    /// Visually distinct from `Missing` because they look identical on the
    /// map (both draw nothing) and mean opposite things.
    Empty,
    /// Not in the database at all. Never imported.
    Missing,
}

impl CoverageStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "full" => Some(CoverageStatus::Full),
            "partial" => Some(CoverageStatus::Partial),
            "empty" => Some(CoverageStatus::Empty),
            "missing" => Some(CoverageStatus::Missing),
            _ => None,
        }
    }

    /// Straight sRGB. `Missing` is a warm red because on this overlay it IS
    /// the finding -- unlike a tile's `absent`, which is usually just the
    /// ocean, a region that was never imported is the work still to do.
    pub fn rgb(self) -> [f32; 3] {
        match self {
            CoverageStatus::Full => [0.16, 0.78, 0.36],
            CoverageStatus::Partial => [1.00, 0.72, 0.10],
            CoverageStatus::Empty => [0.30, 0.60, 0.96],
            CoverageStatus::Missing => [0.92, 0.24, 0.26],
        }
    }

    /// `Missing` fills a little harder than the rest. It is the majority of
    /// the map and the thing being looked for, and at the alpha the others
    /// use it reads as a wash rather than as an answer.
    pub fn fill_alpha(self) -> f32 {
        match self {
            CoverageStatus::Missing => 0.26,
            _ => 0.20,
        }
    }

    pub fn fill(self) -> DebugRgba {
        let [r, g, b] = self.rgb();
        [r, g, b, self.fill_alpha()]
    }

    /// Opaque, for the same corner-overlap reason as `debug_tiles`.
    pub fn border(self) -> DebugRgba {
        let [r, g, b] = self.rgb();
        [r, g, b, 1.0]
    }

    pub fn label(self) -> &'static str {
        match self {
            CoverageStatus::Full => "imported: roads + buildings",
            CoverageStatus::Partial => "partial: roads, no buildings",
            CoverageStatus::Empty => "imported, no features",
            CoverageStatus::Missing => "NOT imported",
        }
    }

    pub const ALL: [CoverageStatus; 4] =
        [CoverageStatus::Full, CoverageStatus::Partial, CoverageStatus::Empty, CoverageStatus::Missing];
}

/// One region of the corpus and how far its import got.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageCell {
    /// The region's own name in the corpus -- a primary mesh code for the
    /// Japanese data. Drawn on the rectangle when there is room.
    pub label: String,
    /// A human-readable detail, e.g. `roads, no buildings`. Carried so a
    /// future readout can show WHAT is missing without re-deriving it.
    pub note: String,
    pub status: CoverageStatus,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// Reads the JSON `etl/mesh_coverage.py` writes.
///
/// Hand-parsed off `serde_json::Value` rather than through a `Deserialize`
/// derive: the document is small, the shape is stable, and a malformed or
/// half-written file should cost the overlay and nothing else -- so an
/// unparseable cell is skipped with a warning rather than failing the whole
/// load and taking the map down with it.
pub fn parse_coverage(bytes: &[u8]) -> anyhow::Result<Vec<CoverageCell>> {
    let document: serde_json::Value = serde_json::from_slice(bytes)?;
    let cells = document
        .get("cells")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("coverage document has no `cells` array"))?;
    let mut out = Vec::with_capacity(cells.len());
    let mut skipped = 0usize;
    for cell in cells {
        let number = |key: &str| cell.get(key).and_then(serde_json::Value::as_f64);
        let (Some(min_lon), Some(min_lat), Some(max_lon), Some(max_lat)) =
            (number("min_lon"), number("min_lat"), number("max_lon"), number("max_lat"))
        else {
            skipped += 1;
            continue;
        };
        let Some(status) = cell.get("status").and_then(serde_json::Value::as_str).and_then(CoverageStatus::parse) else {
            skipped += 1;
            continue;
        };
        out.push(CoverageCell {
            label: cell.get("label").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
            note: cell.get("note").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
            status,
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        });
    }
    if skipped > 0 {
        eprintln!("gpui-maps: coverage overlay skipped {skipped} malformed cell(s)");
    }
    Ok(out)
}

/// How many cells hold each status -- the legend's counts, which are what
/// turn "some of it is red" into "149 of 194 regions have never been
/// imported".
pub fn counts(cells: &[CoverageCell]) -> [(CoverageStatus, usize); 4] {
    CoverageStatus::ALL.map(|status| (status, cells.iter().filter(|c| c.status == status).count()))
}

/// The cell's four corners in y-down LOGICAL window pixels, north-west
/// first, clockwise.
///
/// `None` when any corner is at or behind the camera's near plane -- see
/// `debug_tiles::tile_screen_corners`, which this mirrors exactly; the only
/// difference is that a cell's corners come from lon/lat rather than from a
/// tile coordinate.
pub fn cell_screen_corners(camera: &Camera, cell: &CoverageCell) -> Option<[(f32, f32); 4]> {
    // North is MAXIMUM latitude and MINIMUM world y -- Mercator's y grows
    // southwards. Getting this backwards would still draw a rectangle in
    // the right place (the four corners are the same set), so it is stated
    // rather than discovered.
    let corners = [
        (cell.min_lon, cell.max_lat),
        (cell.max_lon, cell.max_lat),
        (cell.max_lon, cell.min_lat),
        (cell.min_lon, cell.min_lat),
    ];
    // Four corners, one matrix: see `project_world_to_screen_f64_with_matrix`.
    let view_projection = camera.view_projection_f64();
    let mut screen = [(0.0f32, 0.0f32); 4];
    for (slot, (lon, lat)) in screen.iter_mut().zip(corners) {
        let world = crate::overlay::geom::lon_lat_to_world(lon, lat);
        *slot = super::project_world_to_screen_f64_with_matrix(camera, &view_projection, world)?;
    }
    Some(screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &[u8] = br#"{
      "counts": {"full": 1, "missing": 1},
      "cells": [
        {"label":"5339","status":"full","note":"roads + buildings",
         "min_lon":139.0,"min_lat":35.333333,"max_lon":140.0,"max_lat":36.0},
        {"label":"3523","status":"missing","note":"not imported",
         "min_lon":123.0,"min_lat":23.333333,"max_lon":124.0,"max_lat":24.0},
        {"label":"bad","status":"nonsense","min_lon":0,"min_lat":0,"max_lon":1,"max_lat":1}
      ]
    }"#;

    #[test]
    fn a_document_parses_and_an_unknown_status_is_skipped_not_fatal() {
        let cells = parse_coverage(DOCUMENT).expect("parses");
        assert_eq!(cells.len(), 2, "the third cell's status is not a status");
        assert_eq!(cells[0].label, "5339");
        assert_eq!(cells[0].status, CoverageStatus::Full);
        assert_eq!(cells[1].status, CoverageStatus::Missing);
    }

    #[test]
    fn counts_are_per_status_and_cover_every_variant() {
        let cells = parse_coverage(DOCUMENT).expect("parses");
        let counts = counts(&cells);
        assert_eq!(counts.len(), CoverageStatus::ALL.len(), "every status has a row, even at zero");
        for (status, n) in counts {
            let expected = match status {
                CoverageStatus::Full | CoverageStatus::Missing => 1,
                _ => 0,
            };
            assert_eq!(n, expected, "{status:?}");
        }
    }

    #[test]
    fn a_document_without_cells_is_an_error_not_an_empty_overlay() {
        assert!(parse_coverage(br#"{"counts":{}}"#).is_err());
    }

    /// The two statuses that draw NOTHING on the map -- a region nobody
    /// imported and a region that imported to nothing -- must not look
    /// alike, since telling them apart is the only way to read the map.
    #[test]
    fn missing_and_empty_are_not_the_same_colour() {
        assert_ne!(CoverageStatus::Missing.rgb(), CoverageStatus::Empty.rgb());
    }

    #[test]
    fn a_cell_projects_to_a_rectangle_the_right_way_up() {
        let camera = Camera {
            center_lon: 139.5,
            center_lat: 35.7,
            zoom: 6.0,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 800.0,
            viewport_height_px: 600.0,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let cell = CoverageCell {
            label: "5339".into(),
            note: String::new(),
            status: CoverageStatus::Full,
            min_lon: 139.0,
            min_lat: 35.333_333,
            max_lon: 140.0,
            max_lat: 36.0,
        };
        let corners = cell_screen_corners(&camera, &cell).expect("in front of the camera");
        // North-west corner is above and left of the south-east one, in
        // y-down screen space.
        assert!(corners[0].0 < corners[2].0, "west is left of east");
        assert!(corners[0].1 < corners[2].1, "north is above south");
    }
}
