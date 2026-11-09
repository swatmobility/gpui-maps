//! What a host can do with a Portolan collection today, and the licence
//! gate a curated listing applies.
//!
//! [`assess`] says, for one collection and a host's [`Support`], whether
//! it goes on the map (its PMTiles and its publisher's default style), and
//! whether its GeoParquet (the table, filters and charts) and its COGs
//! can be read, or need a reprojection the host does not have yet. The
//! answer is never silent: a collection the host cannot draw says why.

use serde::Serialize;

use crate::catalog::Tree;
use crate::model::{Format, Node, NodeKind};

/// The CRSs a host reads without reprojecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    pub geoparquet_crs: Vec<&'static str>,
    pub cog_crs: Vec<&'static str>,
}

impl Default for Support {
    /// gpui-maps today: GeoParquet in CRS84 only, COGs in 3857 or 4326.
    fn default() -> Support {
        Support { geoparquet_crs: vec!["OGC:CRS84", "EPSG:4326"], cog_crs: vec!["EPSG:3857", "EPSG:4326"] }
    }
}

impl Support {
    fn reads(list: &[&str], crs: Option<&str>) -> bool {
        // No stated CRS: GeoParquet's default is CRS84.
        let crs = crs.unwrap_or("OGC:CRS84");
        list.iter().any(|c| c.eq_ignore_ascii_case(crs))
    }
}

/// How a collection reaches the map.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MapPath {
    /// The PMTiles derivative with the publisher's style.
    PmTiles { pmtiles_url: String, style_url: Option<String>, style_title: Option<String> },
    /// Only COGs, in a CRS the host cannot draw yet.
    NeedsReprojection { format: Format, crs: Option<String> },
    /// Nothing drawable (tabular data, or no data asset at all).
    None,
}

/// How a collection's table (GeoParquet) can be read.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TablePath {
    Ready { url: String },
    NeedsReprojection { url: String, crs: String },
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    pub map: MapPath,
    pub table: TablePath,
    pub formats: Vec<Format>,
}

impl Assessment {
    pub fn can_add_to_map(&self) -> bool {
        matches!(self.map, MapPath::PmTiles { .. })
    }

    /// "needs reprojection" anywhere (the map or the table).
    pub fn needs_reprojection(&self) -> bool {
        matches!(self.map, MapPath::NeedsReprojection { .. }) || matches!(self.table, TablePath::NeedsReprojection { .. })
    }
}

/// What `node` (a collection) offers a host with `support`.
pub fn assess(node: &Node, support: &Support) -> Assessment {
    let formats = node.formats();
    let map = if let Some(pm) = node.pmtiles_url() {
        let style = node.default_style();
        MapPath::PmTiles { pmtiles_url: pm.to_string(), style_url: style.map(|s| s.href.clone()), style_title: style.and_then(|s| s.title.clone()) }
    } else if formats.contains(&Format::Cog) {
        // A COG in a CRS the host reads would draw in principle, but the
        // host has no multi-COG mosaic from a catalog's items yet: both
        // cases are listed as coming soon, with the CRS stated.
        let _ = Support::reads(&support.cog_crs, node.crs.as_deref());
        MapPath::NeedsReprojection { format: Format::Cog, crs: node.crs.clone() }
    } else {
        MapPath::None
    };
    // The data's GeoParquet, never a stac-geoparquet item mirror.
    let tables = || node.assets.iter().filter(|a| a.format() == Format::GeoParquet && !a.has_role("collection-mirror"));
    let parquet = tables().find(|a| a.has_role("data")).or_else(|| tables().next());
    let table = match parquet {
        None => TablePath::None,
        Some(a) if Support::reads(&support.geoparquet_crs, node.crs.as_deref()) => TablePath::Ready { url: a.href.clone() },
        Some(a) => TablePath::NeedsReprojection { url: a.href.clone(), crs: node.crs.clone().unwrap_or_default() },
    };
    Assessment { map, table, formats }
}

/// The collections a curated listing shows: those whose licence a person
/// need not read first ([`crate::Licence::curated_ok`]).
pub fn visible_collections(tree: &Tree) -> Vec<&Tree> {
    tree.collections().into_iter().filter(|t| t.node.licence.curated_ok()).collect()
}

/// The collections a curated listing hides, with their licence.
pub fn hidden_collections(tree: &Tree) -> Vec<(&str, String)> {
    tree.collections().into_iter().filter(|t| !t.node.licence.curated_ok()).map(|t| (t.node.id.as_str(), t.node.licence.label())).collect()
}

/// Whether a catalog may be a curated card at all: at least one of its
/// collections has a licence that needs no reading first.
pub fn catalog_visible(tree: &Tree) -> bool {
    !visible_collections(tree).is_empty()
}

/// A catalog node's sub-catalogs and collections that pass the gate, in
/// link order (a sub-catalog with nothing visible under it is dropped).
pub fn visible_children(tree: &Tree) -> Vec<&Tree> {
    tree.children
        .iter()
        .filter(|c| match c.node.kind {
            NodeKind::Collection => c.node.licence.curated_ok(),
            NodeKind::Catalog => catalog_visible(c),
            NodeKind::Item => false,
        })
        .collect()
}
