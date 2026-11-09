//! The layer a freshly loaded dataset gets (spec §2.3): Kepler's
//! `findDefaultLayer`, made deterministic. The n-th layer takes the n-th
//! colour of a fixed list (Kepler picks at random), so tests and
//! screenshots reproduce.

use std::collections::BTreeMap;

use crate::doc::{Hover, HoverField, Layer, LayerType, Vis};
use crate::table::{FieldType, Geometry, Table};

/// Eight qualitative colours, chosen to read on light and dark basemaps.
pub const CYCLE: [[u8; 3]; 8] = [
    [0x2e, 0x86, 0xab], // blue
    [0xe4, 0x57, 0x2e], // vermilion
    [0x2a, 0x9d, 0x8f], // teal
    [0xf2, 0xa5, 0x41], // amber
    [0x8e, 0x5a, 0xc8], // violet
    [0xd6, 0x45, 0x7a], // rose
    [0x5b, 0x8c, 0x3a], // green
    [0x5c, 0x67, 0x7d], // slate
];

/// A point layer's opening radius (Kepler units) for a table of at most
/// [`DENSE_ROWS`] rows. Kepler opens at 10, which its own conversion
/// (`2^14 / (78271.5 * cos(lat))` px per unit) draws at about 2 px: a
/// few dozen cities on a world view all but vanish. 24 is about 5 px, and
/// holds at every zoom under 14, as Kepler's radius does.
pub const POINT_RADIUS: f64 = 24.0;
/// Kepler's own default, kept for dense tables, where 5 px dots would
/// merge into a blanket.
pub const DENSE_POINT_RADIUS: f64 = 10.0;
pub const DENSE_ROWS: usize = 10_000;

/// Kepler's `MAX_DEFAULT_TOOLTIPS`.
pub const MAX_DEFAULT_TOOLTIPS: usize = 5;

/// The colour of the `n`-th layer.
pub fn cycle_color(n: usize) -> [u8; 3] {
    CYCLE[n % CYCLE.len()]
}

/// The first [`MAX_DEFAULT_TOOLTIPS`] fields that are not geometry.
pub fn tooltip_defaults(table: &Table) -> Vec<String> {
    // Coordinates say little on a card (an arc's card states its ends).
    let pairs = point_pairs(table);
    let coordinate = |n: &str| pairs.iter().any(|(a, b)| a == n || b == n);
    table
        .fields
        .iter()
        .filter(|f| !matches!(f.ty, FieldType::GeoJson | FieldType::Point) && !coordinate(&f.name))
        .take(MAX_DEFAULT_TOOLTIPS)
        .map(|f| f.name.clone())
        .collect()
}

/// The default layers for `table`, the first of which is layer number
/// `existing` in the document (it picks its colour from that).
/// A point layer's outline width once switched on: 1 px of an automatic
/// (a shade darker) ring vanished against its own fill in the P3 frames.
pub const POINT_OUTLINE_PX: f64 = 2.0;

/// Points open outlined, 1 px a shade darker than their fill, up to this
/// many rows. Our basemap is light (Kepler's is dark): the pale end of a
/// sequential palette (Global Warming's `#FFC300`) on Streets' cream land
/// and pale water all but vanishes, and a filter that leaves only the top
/// class leaves the whole map pale. The ring gives every class an edge
/// (the top class's is `#B28800`). Past this many rows the dots merge into
/// a blanket where an edge adds nothing but a second source to draw.
pub const OUTLINE_ROWS: usize = 100_000;
/// The default ring's width.
pub const DEFAULT_OUTLINE_PX: f64 = 1.0;

pub fn default_layers(table: &Table, existing: usize) -> Vec<Layer> {
    let kind = match &table.geometry {
        Geometry::Points(_) => LayerType::Points,
        Geometry::Features {
            all_points: true, ..
        } => LayerType::Points,
        Geometry::Features { .. } => LayerType::GeoJson,
        Geometry::None => return Vec::new(),
    };
    let suffix = match kind {
        LayerType::Points => "points",
        _ => "geojson",
    };
    let vis = match kind {
        LayerType::Points => {
            let ring = table.rows <= OUTLINE_ROWS;
            Vis {
                radius: if table.rows > DENSE_ROWS {
                    DENSE_POINT_RADIUS
                } else {
                    POINT_RADIUS
                },
                opacity: 0.8,
                filled: true,
                outline: ring,
                thickness: if ring {
                    DEFAULT_OUTLINE_PX
                } else {
                    POINT_OUTLINE_PX
                },
                color: cycle_color(existing),
                ..Vis::default()
            }
        }
        _ => Vis {
            radius: 10.0,
            opacity: 0.8,
            filled: true,
            outline: true,
            thickness: 1.0,
            color: cycle_color(existing),
            ..Vis::default()
        },
    };
    let hover = Hover {
        card: tooltip_defaults(table)
            .into_iter()
            .map(HoverField::new)
            .collect(),
        highlight: true,
        ..Hover::default()
    };
    let first = Layer {
        id: format!("{}-{suffix}", table.id),
        kind,
        source: table.id.clone(),
        label: table.label.clone(),
        visible: true,
        columns: BTreeMap::new(),
        vis,
        color: None,
        radius: None,
        height: None,
        hover,
        extra: Default::default(),
    };
    // Two coordinate pairs read as origins and destinations: an arc layer
    // waits, hidden, under the points (Kepler adds one too).
    let mut out = vec![first];
    if kind == LayerType::Points && point_pairs(table).len() >= 2 {
        let mut arc = retyped(&out[0], LayerType::Arc, table);
        arc.id = format!("{}-arc", table.id);
        arc.label = format!("{} arcs", table.label);
        arc.visible = false;
        out.push(arc);
    }
    out
}

/// The numeric `(lat, lon)` column pairs of `table`, in column order:
/// Kepler's `findPointFieldPairs`, by name. A latitude column is one whose
/// lower-case name is, starts or ends with `lat` / `latitude`; its partner
/// has the same name with `lon` / `lng` / `long` / `longitude` in its place.
pub fn point_pairs(table: &Table) -> Vec<(String, String)> {
    let numeric: Vec<&str> = table
        .fields
        .iter()
        .filter(|f| f.ty.is_numeric())
        .map(|f| f.name.as_str())
        .collect();
    pairs_by_name(&numeric)
}

/// [`point_pairs`] over bare column names (a header, before any value is
/// read).
pub fn pairs_by_name(numeric: &[&str]) -> Vec<(String, String)> {
    const LAT: [&str; 2] = ["latitude", "lat"];
    const LON: [&str; 4] = ["longitude", "long", "lng", "lon"];
    let mut out = Vec::new();
    for &name in numeric {
        let lower = name.to_ascii_lowercase();
        let Some(lat) = LAT
            .iter()
            .find(|l| lower == **l || lower.starts_with(*l) || lower.ends_with(*l))
        else {
            continue;
        };
        // The partner: the same name with the latitude word swapped.
        let (head, tail) = match (lower.strip_prefix(lat), lower.strip_suffix(lat)) {
            (Some(rest), _) => (String::new(), rest.to_string()),
            (None, Some(rest)) => (rest.to_string(), String::new()),
            (None, None) => continue,
        };
        let partner = numeric.iter().find(|n| {
            let l = n.to_ascii_lowercase();
            LON.iter().any(|lon| l == format!("{head}{lon}{tail}"))
        });
        if let Some(lon) = partner {
            out.push((name.to_string(), lon.to_string()));
        }
    }
    out
}

/// The first column that looks like H3 cell ids.
pub fn h3_column(table: &Table) -> Option<String> {
    table
        .fields
        .iter()
        .find(|f| f.hint == Some(crate::table::FieldHint::H3))
        .map(|f| f.name.clone())
}

/// A binned or cluster layer's colour: the point count (an empty field)
/// through Kepler's default sequential range.
pub fn count_binding() -> crate::doc::ChannelBinding {
    let mut b = crate::doc::ChannelBinding::new("", "quantize");
    b.palette = Some(crate::scales::DEFAULT_SEQUENTIAL.into());
    b.steps = Some(crate::scales::DEFAULT_SEQUENTIAL_STEPS);
    b
}

/// `layer` switched to `kind` (spec §8.2's type select), keeping its id,
/// dataset, name, colour, hover card and visibility. Columns are chosen
/// from `table` as Kepler does when a layer's type changes: the point pair
/// for points, bins and clusters; two pairs (source, then target) for arcs
/// and lines; an H3 column for H3. A colour by field that the new type
/// cannot read is dropped; binned and cluster layers colour by count.
pub fn retyped(layer: &Layer, kind: LayerType, table: &Table) -> Layer {
    let mut out = layer.clone();
    out.kind = kind;
    out.columns = BTreeMap::new();
    out.radius = None;
    out.height = None;
    let pairs = point_pairs(table);
    match kind {
        LayerType::Arc | LayerType::Line | LayerType::Flow | LayerType::HexDemand => {
            let (src, dst) = (pairs.first(), pairs.get(1));
            if kind.is_flow() {
                if let Some(w) = weight_column(table) {
                    out.columns.insert("weight".into(), w);
                }
            }
            if let Some((lat, lon)) = src {
                out.columns.insert("lat0".into(), lat.clone());
                out.columns.insert("lon0".into(), lon.clone());
            }
            if let Some((lat, lon)) = dst {
                out.columns.insert("lat1".into(), lat.clone());
                out.columns.insert("lon1".into(), lon.clone());
            }
            if out.vis.target_color.is_none() {
                out.vis.target_color = Some(target_for(out.vis.color));
            }
            out.color = None;
            // Kepler's arc thickness (a point's outline width means
            // nothing to an arc).
            if !layer.kind.is_od() {
                out.vis.thickness = 2.0;
            }
            // A flow layer's strands are hairlines; its brightness, not
            // its width, carries the load.
            if kind.is_flow() && !layer.kind.is_flow() {
                out.vis.thickness = 1.0;
            }
        }
        LayerType::H3 => {
            if let Some(c) = h3_column(table) {
                out.columns.insert("hex_id".into(), c);
            }
            out.vis.enable3d = false;
        }
        LayerType::Hexbin | LayerType::Grid | LayerType::Cluster => {
            // A colour by a numeric field aggregates it; anything else
            // colours by the point count.
            let numeric = out
                .color
                .as_ref()
                .and_then(|b| table.field(&b.field))
                .is_some_and(|f| f.ty.is_numeric());
            if numeric && kind != LayerType::Cluster {
                if let Some(b) = out.color.as_mut() {
                    if !matches!(b.scale.as_str(), "quantile" | "quantize" | "linear") {
                        b.scale = "quantize".into();
                    }
                }
                if out.vis.aggregation.as_deref().is_none_or(|a| a == "count") {
                    out.vis.aggregation = Some("average".into());
                }
            } else {
                out.color = Some(count_binding());
                out.vis.aggregation = None;
            }
            if kind == LayerType::Cluster {
                out.vis.enable3d = false;
            }
        }
        LayerType::Heatmap => {
            // Colour is density through a palette; a numeric size binding
            // would be the weight, but a point's radius field is not one.
            let palette = out.color.as_ref().filter(|b| b.field.is_empty()).cloned();
            out.color = Some(palette.unwrap_or_else(count_binding));
            out.vis.aggregation = None;
            out.vis.enable3d = false;
            if !matches!(layer.kind, LayerType::Heatmap) {
                out.vis.radius = crate::doc::DEFAULT_HEATMAP_RADIUS;
            }
            if let Some((lat, lon)) = pairs.first() {
                out.columns.insert("lat".into(), lat.clone());
                out.columns.insert("lon".into(), lon.clone());
            }
        }
        LayerType::Trip => {
            // Rows of points: grouped by an id column, ordered by a time
            // column. GeoJSON lines carry their own times.
            if matches!(table.geometry, Geometry::Points(_)) {
                if let Some((lat, lon)) = pairs.first() {
                    out.columns.insert("lat".into(), lat.clone());
                    out.columns.insert("lon".into(), lon.clone());
                }
                if let Some(t) = trip_time_column(table) {
                    out.columns.insert("timestamp".into(), t);
                }
                if let Some(id) = trip_id_column(table) {
                    out.columns.insert("id".into(), id);
                }
            }
            if out.color.as_ref().is_some_and(|b| b.field.is_empty()) {
                out.color = None;
            }
            out.vis.aggregation = None;
            out.vis.enable3d = false;
            if !layer.kind.is_od() && layer.kind != LayerType::Trip {
                out.vis.thickness = 2.0;
            }
        }
        LayerType::Points | LayerType::GeoJson => {
            if out.color.as_ref().is_some_and(|b| b.field.is_empty()) {
                out.color = None;
            }
            out.vis.aggregation = None;
            if kind == LayerType::Points {
                out.vis.enable3d = false;
            }
        }
    }
    if out.vis.enable3d && kind == LayerType::GeoJson {
        out.height = first_numeric(table).map(|f| crate::doc::ChannelBinding::new(f, "linear"));
    }
    out
}

/// An OD table's weight column: a number named like a count of people or
/// trips (`all_flows`, `count`, `trips`, `weight`, ...); `None` counts
/// every row as one.
pub fn weight_column(table: &Table) -> Option<String> {
    const WORDS: [&str; 11] = [
        "flow",
        "count",
        "trips",
        "weight",
        "volume",
        "people",
        "persons",
        "commuters",
        "passengers",
        "journeys",
        "riders",
    ];
    let pairs = point_pairs(table);
    table
        .fields
        .iter()
        .filter(|f| f.ty.is_numeric() && !pairs.iter().any(|(a, b)| *a == f.name || *b == f.name))
        .find(|f| {
            let lower = f.name.to_ascii_lowercase();
            WORDS.iter().any(|w| lower.contains(w))
        })
        .map(|f| f.name.clone())
}

/// A trip table's time column: the first timestamp (or date) column.
pub fn trip_time_column(table: &Table) -> Option<String> {
    table
        .fields
        .iter()
        .find(|f| {
            matches!(
                f.ty,
                crate::table::FieldType::Timestamp | crate::table::FieldType::Date
            )
        })
        .map(|f| f.name.clone())
}

/// A trip table's grouping column: one named like an id (`id`, `trip_id`,
/// `vehicle`, ...), else the first text column.
pub fn trip_id_column(table: &Table) -> Option<String> {
    const NAMES: [&str; 7] = [
        "id",
        "trip_id",
        "tripid",
        "trip",
        "vehicle",
        "vehicle_id",
        "track_id",
    ];
    let lower = |f: &crate::table::Field| f.name.to_ascii_lowercase();
    table
        .fields
        .iter()
        .find(|f| NAMES.contains(&lower(f).as_str()))
        .or_else(|| {
            table
                .fields
                .iter()
                .find(|f| f.ty == crate::table::FieldType::String)
        })
        .map(|f| f.name.clone())
}

/// One choice of the Add layer chooser: a type (a GeoJSON layer extruded
/// or flat), and why it cannot draw this dataset, if it cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeOption {
    pub kind: LayerType,
    pub extruded: bool,
    /// `None` when the type can draw the dataset; else a short reason the
    /// chooser shows on the greyed chip.
    pub reason: Option<String>,
}

impl TypeOption {
    pub fn label(&self) -> &'static str {
        option_label(self.kind, self.extruded)
    }

    /// The chip's selector stem: the type's Kepler name, `polygon-3d` for
    /// the extruded GeoJSON.
    pub fn key(&self) -> String {
        match (self.kind, self.extruded) {
            (LayerType::GeoJson, true) => "polygon-3d".into(),
            (LayerType::GeoJson, false) => "polygon".into(),
            (k, _) => k.kepler_name().to_string(),
        }
    }
}

/// A chooser type's name: as a new layer's name ends ("Pickups · Hexbin").
pub fn option_label(kind: LayerType, extruded: bool) -> &'static str {
    match (kind, extruded) {
        (LayerType::Points, _) => "Points",
        (LayerType::GeoJson, true) => "3D polygon",
        (LayerType::GeoJson, false) => "Polygon",
        (k, _) => k.label(),
    }
}

/// Every type the Add layer chooser lists for `table`, each valid or with
/// the reason it is not: point data gets points, hexbin, grid, heatmap and
/// cluster, plus arc and line with a second coordinate pair and trip with
/// an id and a time column; shapes get polygon (flat or 3D); H3 needs a
/// column of cell ids and a build that draws them (`h3_unavailable` is
/// the build's reason, or `None`). `timed_lines`: the features are lines
/// whose coordinates carry times (Kepler's trip shape).
pub fn type_options(
    table: &Table,
    timed_lines: bool,
    h3_unavailable: Option<&str>,
) -> Vec<TypeOption> {
    let points = matches!(
        table.geometry,
        Geometry::Points(_)
            | Geometry::Features {
                all_points: true,
                ..
            }
    );
    let shapes = matches!(
        table.geometry,
        Geometry::Features {
            all_points: false,
            ..
        }
    );
    // Point geometry is a coordinate pair even when its columns are not
    // named as one (or are not columns at all).
    let pairs = point_pairs(table).len().max(usize::from(points));
    let no_points = || Some("needs point positions (a lat/lon column pair)".to_string());
    let opt = |kind, extruded, reason: Option<String>| TypeOption {
        kind,
        extruded,
        reason,
    };
    let mut out = Vec::new();
    for kind in [
        LayerType::Points,
        LayerType::Hexbin,
        LayerType::Grid,
        LayerType::Heatmap,
        LayerType::Cluster,
    ] {
        out.push(opt(kind, false, if points { None } else { no_points() }));
    }
    for kind in [
        LayerType::Arc,
        LayerType::Line,
        LayerType::Flow,
        LayerType::HexDemand,
    ] {
        let reason = match pairs {
            0 => Some("needs two lat/lon column pairs (origin and destination)".to_string()),
            1 => Some("needs a second lat/lon column pair (a destination)".to_string()),
            _ => None,
        };
        out.push(opt(kind, false, reason));
    }
    let trip = if matches!(table.geometry, Geometry::Points(_)) {
        match (trip_id_column(table), trip_time_column(table)) {
            (Some(_), Some(_)) => None,
            (None, _) => Some("needs an id column to group the rows into trips".to_string()),
            (_, None) => Some("needs a time column".to_string()),
        }
    } else if timed_lines {
        None
    } else {
        Some("needs rows with an id and a time, or timed lines".to_string())
    };
    out.push(opt(LayerType::Trip, false, trip));
    let no_shapes = || Some("needs polygon or line features (GeoJSON or WKT)".to_string());
    out.push(opt(
        LayerType::GeoJson,
        false,
        if shapes { None } else { no_shapes() },
    ));
    let solid = if !shapes {
        no_shapes()
    } else if first_numeric(table).is_none() {
        Some("needs a number column for the height".to_string())
    } else {
        None
    };
    out.push(opt(LayerType::GeoJson, true, solid));
    let h3 = match (h3_column(table), h3_unavailable) {
        (None, _) => Some("needs a column of H3 cell ids".to_string()),
        (Some(_), Some(why)) => Some(why.to_string()),
        (Some(_), None) => None,
    };
    out.push(opt(LayerType::H3, false, h3));
    out
}

/// `label` without its file extension: a new layer's name starts with it.
pub fn dataset_stem(label: &str) -> &str {
    match label.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && !ext.contains(' ') =>
        {
            stem
        }
        _ => label,
    }
}

/// A layer the user (or the agent) adds to `table`'s dataset: the
/// dataset's default layer (colour `existing` of the cycle, its hover
/// card) switched to `kind` by [`retyped`], extruded when asked, visible,
/// with id `id` and the name "<dataset> · <Type>". The Add layer chooser
/// and the agent's `layer_add` both build layers here, so the two match.
pub fn new_layer(
    table: &Table,
    kind: LayerType,
    extruded: bool,
    existing: usize,
    id: String,
) -> Layer {
    let base = default_layers(table, existing)
        .into_iter()
        .next()
        .unwrap_or_else(|| Layer {
            id: id.clone(),
            kind: LayerType::Points,
            source: table.id.clone(),
            label: table.label.clone(),
            visible: true,
            columns: BTreeMap::new(),
            vis: Vis {
                color: cycle_color(existing),
                opacity: 0.8,
                filled: true,
                ..Vis::default()
            },
            color: None,
            radius: None,
            height: None,
            hover: Hover {
                card: tooltip_defaults(table)
                    .into_iter()
                    .map(HoverField::new)
                    .collect(),
                highlight: true,
                ..Hover::default()
            },
            extra: Default::default(),
        });
    let mut l = if base.kind == kind {
        base
    } else {
        retyped(&base, kind, table)
    };
    l.id = id;
    l.visible = true;
    l.label = format!(
        "{} · {}",
        dataset_stem(&table.label),
        option_label(kind, extruded)
    );
    if extruded
        && matches!(
            kind,
            LayerType::GeoJson | LayerType::Hexbin | LayerType::Grid
        )
    {
        l.vis.enable3d = true;
        if kind == LayerType::GeoJson && l.height.is_none() {
            l.height = first_numeric(table).map(|f| crate::doc::ChannelBinding::new(f, "linear"));
        }
    }
    l
}

/// `layer` moved onto dataset `table`: the same type, look and name, its
/// columns chosen again from the new dataset (as a type switch chooses
/// them), and its colour and hover fields kept only where the new dataset
/// has those columns. `None` when the type cannot draw that dataset.
pub fn moved_to(layer: &Layer, table: &Table, timed_lines: bool) -> Option<Layer> {
    let ok = type_options(table, timed_lines, None).into_iter().any(|o| {
        o.kind == layer.kind
            && o.extruded == (layer.kind == LayerType::GeoJson && layer.vis.enable3d)
            && o.reason.is_none()
    });
    // H3 is judged by the column only: the build's own reason is the
    // chooser's business.
    let ok = ok || (layer.kind == LayerType::H3 && h3_column(table).is_some());
    if !ok {
        return None;
    }
    let mut out = layer.clone();
    out.source = table.id.clone();
    let has = |f: &str| f.is_empty() || table.field(f).is_some();
    if out.color.as_ref().is_some_and(|b| !has(&b.field)) {
        out.color = None;
    }
    let mut out = retyped(&out, layer.kind, table);
    // `retyped` dropped the size and height bindings; keep the ones the new
    // dataset can read.
    out.radius = layer.radius.clone().filter(|b| has(&b.field));
    if layer.kind == LayerType::GeoJson && layer.vis.enable3d {
        out.height =
            layer.height.clone().filter(|b| has(&b.field)).or_else(|| {
                first_numeric(table).map(|f| crate::doc::ChannelBinding::new(f, "linear"))
            });
    }
    out.hover.card.retain(|h| has(&h.field));
    if out.hover.card.is_empty() {
        out.hover.card = tooltip_defaults(table)
            .into_iter()
            .map(HoverField::new)
            .collect();
    }
    Some(out)
}

/// The target end's colour for an arc whose source is `c`: the next
/// colour of the cycle, so the direction reads.
pub fn target_for(c: [u8; 3]) -> [u8; 3] {
    let i = CYCLE.iter().position(|x| *x == c).unwrap_or(0);
    CYCLE[(i + 1) % CYCLE.len()]
}

/// The first numeric column that is not a coordinate.
pub fn first_numeric(table: &Table) -> Option<String> {
    let pairs = point_pairs(table);
    table
        .fields
        .iter()
        .find(|f| f.ty.is_numeric() && !pairs.iter().any(|(a, b)| *a == f.name || *b == f.name))
        .map(|f| f.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::field;
    use crate::table::Column;

    #[test]
    fn two_coordinate_pairs_make_a_hidden_arc_and_retype_into_arcs() {
        let numeric = |mut t: Table| {
            t.fields.iter_mut().for_each(|f| f.ty = FieldType::Real);
            t
        };
        let t = numeric(table(
            Geometry::Points(vec![[0.0, 0.0]]),
            &[
                "trip",
                "origin_lat",
                "origin_lng",
                "dest_lat",
                "dest_lng",
                "fare",
            ],
        ));
        assert_eq!(
            point_pairs(&t),
            vec![
                ("origin_lat".to_string(), "origin_lng".to_string()),
                ("dest_lat".to_string(), "dest_lng".to_string())
            ]
        );
        let layers = default_layers(&t, 0);
        assert_eq!(layers.len(), 2);
        let arc = &layers[1];
        assert_eq!((arc.kind, arc.visible), (LayerType::Arc, false));
        assert_eq!(
            arc.columns.get("lat1").map(String::as_str),
            Some("dest_lat")
        );
        assert_eq!(
            arc.columns.get("lon0").map(String::as_str),
            Some("origin_lng")
        );
        // A hexbin colours by the count; back to points, the count goes.
        let hex = retyped(&layers[0], LayerType::Hexbin, &t);
        assert!(hex.color.as_ref().is_some_and(|b| b.field.is_empty()));
        assert!(retyped(&hex, LayerType::Points, &t).color.is_none());
        assert_eq!(first_numeric(&t).as_deref(), Some("trip"));
        // `lat`/`lon` alone is one pair.
        let one = numeric(table(
            Geometry::Points(vec![[0.0, 0.0]]),
            &["lat", "lon", "Latitude2"],
        ));
        assert_eq!(point_pairs(&one).len(), 1);
        assert_eq!(default_layers(&one, 0).len(), 1);
    }

    fn table(geometry: Geometry, names: &[&str]) -> Table {
        Table {
            id: "cities".into(),
            label: "world_cities.csv".into(),
            rows: 1,
            geometry,
            fields: names
                .iter()
                .map(|n| field(n, Column::Number(vec![Some(1.0)])))
                .collect(),
        }
    }

    #[test]
    fn the_chooser_offers_the_types_a_dataset_can_draw_and_says_why_not() {
        let mut od = table(
            Geometry::Points(vec![[0.0, 0.0]]),
            &[
                "trip_id",
                "origin_lat",
                "origin_lng",
                "dest_lat",
                "dest_lng",
                "fare",
            ],
        );
        od.fields
            .iter_mut()
            .skip(1)
            .for_each(|f| f.ty = FieldType::Real);
        od.fields[0].ty = FieldType::String;
        let valid = |opts: &[TypeOption]| {
            opts.iter()
                .filter(|o| o.reason.is_none())
                .map(|o| o.label())
                .collect::<Vec<_>>()
        };
        let opts = type_options(&od, false, None);
        assert_eq!(
            valid(&opts),
            [
                "Points",
                "Hexbin",
                "Grid",
                "Heatmap",
                "Cluster",
                "Arc",
                "Line",
                "Bundled flows",
                "Hex demand"
            ]
        );
        // A flow layer reads both pairs and, with no count-like column,
        // weighs every row as one; a riders column is its weight.
        let flow = retyped(&default_layers(&od, 0).remove(0), LayerType::Flow, &od);
        assert_eq!(
            (
                flow.columns.get("lat1").map(String::as_str),
                flow.columns.get("weight")
            ),
            (Some("dest_lat"), None)
        );
        assert_eq!(flow.vis.thickness, 1.0, "flow strands are hairlines");
        let mut riders = od.clone();
        riders
            .fields
            .push(field("riders", Column::Number(vec![Some(2.0)])));
        riders.fields.last_mut().unwrap().ty = FieldType::Integer;
        assert_eq!(weight_column(&riders).as_deref(), Some("riders"));
        assert_eq!(
            retyped(
                &default_layers(&riders, 0).remove(0),
                LayerType::HexDemand,
                &riders
            )
            .columns
            .get("weight")
            .map(String::as_str),
            Some("riders")
        );
        let trip = opts.iter().find(|o| o.kind == LayerType::Trip).unwrap();
        assert_eq!(trip.reason.as_deref(), Some("needs a time column"));
        assert!(opts
            .iter()
            .find(|o| o.kind == LayerType::H3)
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("H3 cell ids"));
        assert!(opts
            .iter()
            .filter(|o| o.kind == LayerType::GeoJson)
            .all(|o| o.reason.as_deref().unwrap().contains("polygon")));
        // A time column makes trips; one pair alone makes no arcs.
        let mut timed = od.clone();
        timed
            .fields
            .push(field("depart", Column::Number(vec![Some(1.0)])));
        timed.fields.last_mut().unwrap().ty = FieldType::Timestamp;
        assert!(valid(&type_options(&timed, false, None)).contains(&"Trip"));
        let mut one = table(Geometry::Points(vec![[0.0, 0.0]]), &["lat", "lon"]);
        one.fields.iter_mut().for_each(|f| f.ty = FieldType::Real);
        let one = type_options(&one, false, None);
        assert!(one
            .iter()
            .find(|o| o.kind == LayerType::Line)
            .unwrap()
            .reason
            .as_deref()
            .unwrap()
            .contains("second"));
        // Polygons: flat and 3D, no point types.
        let mut polys = table(
            Geometry::Features {
                count: 1,
                bounds: None,
                all_points: false,
            },
            &["height"],
        );
        polys.fields[0].ty = FieldType::Real;
        assert_eq!(
            valid(&type_options(&polys, false, None)),
            ["Polygon", "3D polygon"]
        );
        // H3: the column, then the build's reason.
        let mut cells = table(Geometry::None, &["hex"]);
        cells.fields[0].hint = Some(crate::table::FieldHint::H3);
        assert_eq!(valid(&type_options(&cells, false, None)), ["H3"]);
        assert_eq!(
            type_options(&cells, false, Some("native only"))
                .iter()
                .find(|o| o.kind == LayerType::H3)
                .unwrap()
                .reason
                .as_deref(),
            Some("native only")
        );
    }

    #[test]
    fn a_new_layer_is_named_by_its_dataset_and_type() {
        let mut t = table(Geometry::Points(vec![[0.0, 0.0]]), &["lat", "lon", "fare"]);
        t.id = "pickups".into();
        t.label = "Pickups.csv".into();
        let l = new_layer(&t, LayerType::Hexbin, false, 1, "pickups-hexbin".into());
        assert_eq!(
            (
                l.id.as_str(),
                l.label.as_str(),
                l.source.as_str(),
                l.kind,
                l.visible
            ),
            (
                "pickups-hexbin",
                "Pickups · Hexbin",
                "pickups",
                LayerType::Hexbin,
                true
            )
        );
        assert_eq!(l.vis.color, CYCLE[1], "the next colour of the cycle");
        assert!(
            l.color.as_ref().is_some_and(|b| b.field.is_empty()),
            "a hexbin colours by count"
        );
        let mut polys = table(
            Geometry::Features {
                count: 1,
                bounds: None,
                all_points: false,
            },
            &["height"],
        );
        polys.fields[0].ty = FieldType::Real;
        let solid = new_layer(&polys, LayerType::GeoJson, true, 0, "p".into());
        assert!(solid.vis.enable3d && solid.height.as_ref().is_some_and(|h| h.field == "height"));
        assert_eq!(solid.label, "world_cities · 3D polygon");
        assert_eq!(dataset_stem("a.b.geojson"), "a.b");
        assert_eq!(dataset_stem("no extension"), "no extension");
    }

    #[test]
    fn the_default_layer_for_lat_lon_is_points() {
        let layers = default_layers(&table(Geometry::Points(vec![[1.0, 2.0]]), &["a"]), 0);
        assert_eq!(layers.len(), 1);
        let l = &layers[0];
        assert_eq!(
            (l.kind, l.id.as_str(), l.source.as_str()),
            (LayerType::Points, "cities-points", "cities")
        );
        assert_eq!(
            (l.vis.radius, l.vis.opacity, l.vis.color),
            (POINT_RADIUS, 0.8, CYCLE[0])
        );
        // A dense table keeps Kepler's smaller dots.
        let mut dense = table(Geometry::Points(vec![]), &[]);
        dense.rows = DENSE_ROWS + 1;
        assert_eq!(default_layers(&dense, 0)[0].vis.radius, DENSE_POINT_RADIUS);
        // A GeoJSON file of nothing but points is points too.
        let all_points = Geometry::Features {
            count: 1,
            bounds: None,
            all_points: true,
        };
        assert_eq!(
            default_layers(&table(all_points, &[]), 0)[0].kind,
            LayerType::Points
        );
    }

    #[test]
    fn the_default_layer_for_polygons_is_geojson() {
        let polys = Geometry::Features {
            count: 3,
            bounds: Some([0.0, 0.0, 1.0, 1.0]),
            all_points: false,
        };
        let l = &default_layers(&table(polys, &["a"]), 0)[0];
        assert_eq!(l.kind, LayerType::GeoJson);
        assert!(l.vis.filled && l.vis.outline);
        assert_eq!(l.vis.thickness, 1.0);
        let points = &default_layers(&table(Geometry::Points(vec![[0.0, 0.0]]), &["a"]), 0)[0];
        assert_eq!(
            (points.vis.outline, points.vis.thickness),
            (true, DEFAULT_OUTLINE_PX),
            "a 1 px ring by default, for a light basemap"
        );
        let mut big = table(Geometry::Points(vec![]), &[]);
        big.rows = OUTLINE_ROWS + 1;
        let big = &default_layers(&big, 0)[0];
        assert_eq!(
            (big.vis.outline, big.vis.thickness),
            (false, POINT_OUTLINE_PX),
            "a blanket of dots has no ring; 2 px when switched on"
        );
        assert!(
            default_layers(&table(Geometry::None, &["a"]), 0).is_empty(),
            "no geometry, no layer"
        );
    }

    #[test]
    fn default_tooltips_are_the_first_five_non_geometry_fields() {
        let mut t = table(
            Geometry::Points(vec![]),
            &["a", "geom", "b", "c", "d", "e", "f"],
        );
        t.fields[1].ty = FieldType::GeoJson;
        assert_eq!(tooltip_defaults(&t), ["a", "b", "c", "d", "e"]);
        let l = &default_layers(&t, 0)[0];
        assert_eq!(
            l.hover
                .card
                .iter()
                .map(|h| h.field.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn layer_colours_cycle_deterministically() {
        let t = table(Geometry::Points(vec![]), &[]);
        let colours: Vec<[u8; 3]> = (0..10)
            .map(|n| default_layers(&t, n)[0].vis.color)
            .collect();
        assert_eq!(colours[..8], CYCLE);
        assert_eq!(colours[8], CYCLE[0]);
        assert_eq!(colours[9], CYCLE[1]);
        assert_eq!(default_layers(&t, 3), default_layers(&t, 3));
    }
}
