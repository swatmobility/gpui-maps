//! Caller markers: a short list of labelled points a page hands the map
//! (`setMarkers([{lon, lat, label, kind}])` on the web shell).
//!
//! Two kinds, styled apart so a reader can tell them at a glance:
//!
//! * [`MarkerKind::Address`] -- where a place IS (an address search's
//!   display point). A larger red dot, label ABOVE.
//! * [`MarkerKind::Arrival`] -- where a vehicle stops for it (the
//!   road-side arrival or dispatch point). A smaller blue dot, label BELOW,
//!   so the two labels of one result never sit on each other when the
//!   points are a few metres apart.
//!
//! The dots are point sources ([`marker_layers`]): each dot sits on a white
//! ring (a second, larger circle in its own source drawn first -- point
//! paint has no `circle-stroke`), so it reads on light and dark basemaps
//! alike. The labels use the basemap's own settlement-label look
//! ([`LABEL_STYLE`]: `label_town` / `label_village` in the open style), so
//! marker text is not a second label style on the map; where they hang is
//! [`label_offset_px`].
//!
//! Everything here is pure and tested natively: the web shell's wrapper
//! (`web/src/app/markers_api.rs`) only parses, pushes the layers and moves
//! the label elements to [`label_offset_px`] from each projected point.

use serde::Deserialize;

use crate::overlay::FeaturePaint;
use crate::style::Rgba;

/// Prefix of every marker source name. `setMarkers` replaces all of them
/// and `clearMarkers` removes all of them.
pub const MARKER_SOURCE_PREFIX: &str = "markers:";

/// Which kind of point a marker is. See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerKind {
    #[default]
    Address,
    Arrival,
}

impl MarkerKind {
    /// `address` / `arrival`: the name a page passes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            MarkerKind::Address => "address",
            MarkerKind::Arrival => "arrival",
        }
    }

    /// Dot colour and radius (logical px).
    fn dot(self) -> (Rgba, f32) {
        match self {
            // #e0333d
            MarkerKind::Address => (Rgba { r: 0.878, g: 0.200, b: 0.239, a: 1.0 }, 7.0),
            // #1f6fe0
            MarkerKind::Arrival => (Rgba { r: 0.122, g: 0.435, b: 0.878, a: 1.0 }, 5.5),
        }
    }
}

/// One marker, as a page passes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Marker {
    pub lon: f64,
    pub lat: f64,
    /// Drawn next to the dot; empty for none.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: MarkerKind,
}

/// The most markers one call takes. A geocoder test page shows a handful;
/// a caller passing thousands wants a real point source instead.
pub const MAX_MARKERS: usize = 1000;

/// Parses a page's marker list (`[{lon, lat, label?, kind?}]`), refusing
/// a non-finite or out-of-range coordinate and an unknown `kind` with a
/// message that names the offending entry.
pub fn parse_markers(json_text: &str) -> Result<Vec<Marker>, String> {
    let markers: Vec<Marker> = serde_json::from_str(json_text).map_err(|e| format!("markers: {e}"))?;
    if markers.len() > MAX_MARKERS {
        return Err(format!("markers: {} given, at most {MAX_MARKERS}", markers.len()));
    }
    for (i, m) in markers.iter().enumerate() {
        if !(m.lon.is_finite() && (-180.0..=180.0).contains(&m.lon)) {
            return Err(format!("markers[{i}]: lon {} is not in -180..180", m.lon));
        }
        if !(m.lat.is_finite() && (-85.06..=85.06).contains(&m.lat)) {
            return Err(format!("markers[{i}]: lat {} is not in -85.06..85.06", m.lat));
        }
    }
    Ok(markers)
}

/// White ring width around each dot, in logical px.
pub const RING_PX: f32 = 2.0;

/// One point source of the marker set.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerLayer {
    /// The overlay source name ([`MARKER_SOURCE_PREFIX`] + a suffix).
    pub source: String,
    /// `[lon, lat]` per point.
    pub lon_lat: Vec<[f64; 2]>,
    pub paint: FeaturePaint,
    /// `members[i]` is the index (in the page's list) of point `i`: what a
    /// pick on this source maps back to ([`marker_for_pick`]).
    pub members: Vec<usize>,
}

/// The marker set as point sources, in DRAW order (a later source draws on
/// top): every ring first, then the arrival dots, then the address dots,
/// so an address dot is never hidden under a neighbour's ring. A kind with
/// no markers has no layers.
#[must_use]
pub fn marker_layers(markers: &[Marker]) -> Vec<MarkerLayer> {
    let mut layers = Vec::new();
    let kinds = [MarkerKind::Arrival, MarkerKind::Address];
    for ring in [true, false] {
        for kind in kinds {
            let members: Vec<usize> = (0..markers.len()).filter(|&i| markers[i].kind == kind).collect();
            if members.is_empty() {
                continue;
            }
            let (color, radius) = kind.dot();
            let paint = FeaturePaint {
                circle_radius: if ring { radius + RING_PX } else { radius },
                circle_color: if ring { Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 } } else { color },
                ..FeaturePaint::default()
            };
            layers.push(MarkerLayer {
                source: format!("{MARKER_SOURCE_PREFIX}{}{}", kind.name(), if ring { "-ring" } else { "" }),
                lon_lat: members.iter().map(|&i| [markers[i].lon, markers[i].lat]).collect(),
                paint,
                members,
            });
        }
    }
    layers
}

/// The page's marker index for a pick of point `index` in `source`, or
/// `None` when `source` is not a marker layer or the index is past it.
#[must_use]
pub fn marker_for_pick(layers: &[MarkerLayer], source: &str, index: usize) -> Option<usize> {
    layers.iter().find(|l| l.source == source)?.members.get(index).copied()
}

/// The label look, in CSS units: the open style's settlement labels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LabelStyle {
    /// Font size, logical px.
    pub size_px: f32,
    /// `text-color`, `#rrggbb`.
    pub color: &'static str,
    /// `text-halo-color`, as CSS.
    pub halo: &'static str,
    /// `text-halo-width`, logical px.
    pub halo_px: f32,
}

/// `label_town` / `label_village` in `open-streets-style.json`: `#3d3a37`
/// on a 90% white halo of width 1, at the size those labels take at street
/// zoom.
pub const LABEL_STYLE: LabelStyle =
    LabelStyle { size_px: 13.0, color: "#3d3a37", halo: "rgba(255,255,255,0.9)", halo_px: 1.0 };

/// Where a marker's label CENTRE sits relative to its projected point, in
/// logical px (y down): above the dot for an address, below it for an
/// arrival point, clear of the ring by a third of the text height.
#[must_use]
pub fn label_offset_px(kind: MarkerKind) -> (f32, f32) {
    let (_, radius) = kind.dot();
    let clear = radius + RING_PX + LABEL_STYLE.size_px * (0.5 + 0.35);
    match kind {
        MarkerKind::Address => (0.0, -clear),
        MarkerKind::Arrival => (0.0, clear),
    }
}

/// Padding in logical px, CSS order: `[top, right, bottom, left]`.
pub type Padding = [f64; 4];

/// The camera zoom that frames `markers` in a `width` x `height` (logical
/// px) viewport with `pad` px clear on each side (a page passes the room
/// its own floating chrome takes), capped at `max_zoom`; and the centre to
/// put the camera on so the markers sit in the middle of the padded box,
/// as `(lon, lat, zoom)`. `None` for an empty list. Web Mercator at
/// [`crate::geo::TILE_SIZE`]-px tiles.
#[must_use]
pub fn fit_markers(markers: &[Marker], width: f64, height: f64, pad: Padding, max_zoom: f64) -> Option<(f64, f64, f64)> {
    let [top, right, bottom, left] = pad.map(|p| if p.is_finite() { p.max(0.0) } else { 0.0 });
    let first = markers.first()?;
    let merc_y = |lat: f64| {
        let s = lat.to_radians().sin().clamp(-0.9999, 0.9999);
        0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI)
    };
    let merc_x = |lon: f64| (lon + 180.0) / 360.0;
    let (mut x0, mut x1, mut y0, mut y1) = (merc_x(first.lon), merc_x(first.lon), merc_y(first.lat), merc_y(first.lat));
    for m in markers {
        x0 = x0.min(merc_x(m.lon));
        x1 = x1.max(merc_x(m.lon));
        y0 = y0.min(merc_y(m.lat));
        y1 = y1.max(merc_y(m.lat));
    }
    let avail_w = (width - left - right).max(1.0);
    let avail_h = (height - top - bottom).max(1.0);
    let span_w = (x1 - x0).max(1e-12);
    let span_h = (y1 - y0).max(1e-12);
    let world = crate::geo::TILE_SIZE;
    let zoom = (avail_w / (world * span_w)).min(avail_h / (world * span_h)).log2().min(max_zoom);
    // The padded box's centre is (left - right)/2, (top - bottom)/2 px off
    // the viewport's: move the camera the other way by as much.
    let px = world * 2f64.powf(zoom);
    let cx = (x0 + x1) / 2.0 + (right - left) / 2.0 / px;
    let cy = ((y0 + y1) / 2.0 + (bottom - top) / 2.0 / px).clamp(0.0, 1.0);
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * cy)).sinh().atan().to_degrees();
    let lon = cx * 360.0 - 180.0;
    Some((lon, lat, zoom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two() -> Vec<Marker> {
        parse_markers(
            r#"[{"lon":140.6,"lat":38.1,"label":"住所","kind":"address"},
                {"lon":140.61,"lat":38.11,"label":"到着","kind":"arrival"}]"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_kinds_and_defaults() {
        let m = parse_markers(
            r#"[{"lon":140.6,"lat":38.1,"label":"宮南川添 1-1","kind":"address"},
                {"lon":140.61,"lat":38.11,"kind":"arrival"},{"lon":0,"lat":0}]"#,
        )
        .unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].kind, MarkerKind::Address);
        assert_eq!(m[0].label, "宮南川添 1-1");
        assert_eq!(m[1].kind, MarkerKind::Arrival);
        assert_eq!(m[1].label, "");
        assert_eq!(m[2].kind, MarkerKind::Address, "kind defaults to address");
    }

    #[test]
    fn refuses_bad_input_naming_the_entry() {
        assert!(parse_markers(r#"[{"lon":1,"lat":1,"kind":"parcel"}]"#).is_err());
        let e = parse_markers(r#"[{"lon":1,"lat":1},{"lon":200,"lat":1}]"#).unwrap_err();
        assert!(e.contains("markers[1]"), "{e}");
        assert!(parse_markers(r#"[{"lon":1,"lat":89}]"#).is_err());
        assert!(parse_markers(r#"{"lon":1}"#).is_err());
        assert_eq!(parse_markers("[]").unwrap(), vec![]);
    }

    #[test]
    fn rings_draw_first_and_address_dots_last() {
        let layers = marker_layers(&two());
        let names: Vec<&str> = layers.iter().map(|l| l.source.as_str()).collect();
        assert_eq!(names, ["markers:arrival-ring", "markers:address-ring", "markers:arrival", "markers:address"]);
        let by = |n: &str| layers.iter().find(|l| l.source == n).unwrap();
        // A ring is white and RING_PX wider than its dot.
        assert_eq!(by("markers:address-ring").paint.circle_color, Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
        assert_eq!(by("markers:address-ring").paint.circle_radius, by("markers:address").paint.circle_radius + RING_PX);
        // The two kinds differ in colour and size.
        assert_ne!(by("markers:address").paint.circle_color, by("markers:arrival").paint.circle_color);
        assert!(by("markers:address").paint.circle_radius > by("markers:arrival").paint.circle_radius);
        assert_eq!(by("markers:arrival").lon_lat, vec![[140.61, 38.11]]);
        // Every layer's paint passes the overlay's own numeric checks.
        for l in &layers {
            l.paint.validate().unwrap();
        }
    }

    #[test]
    fn a_kind_with_no_markers_has_no_layers() {
        let one = parse_markers(r#"[{"lon":1,"lat":1}]"#).unwrap();
        assert_eq!(marker_layers(&one).len(), 2);
        assert!(marker_layers(&[]).is_empty());
    }

    #[test]
    fn a_pick_maps_back_to_the_page_index() {
        let m = parse_markers(
            r#"[{"lon":1,"lat":1,"kind":"arrival"},{"lon":2,"lat":2},{"lon":3,"lat":3,"kind":"arrival"}]"#,
        )
        .unwrap();
        let layers = marker_layers(&m);
        assert_eq!(marker_for_pick(&layers, "markers:arrival", 1), Some(2));
        assert_eq!(marker_for_pick(&layers, "markers:arrival-ring", 0), Some(0));
        assert_eq!(marker_for_pick(&layers, "markers:address", 0), Some(1));
        assert_eq!(marker_for_pick(&layers, "markers:address", 1), None);
        assert_eq!(marker_for_pick(&layers, "parcels", 0), None);
    }

    #[test]
    fn labels_hang_above_an_address_and_below_an_arrival_clear_of_the_ring() {
        let (_, up) = label_offset_px(MarkerKind::Address);
        let (_, down) = label_offset_px(MarkerKind::Arrival);
        assert!(up < 0.0 && down > 0.0);
        let half_text = LABEL_STYLE.size_px / 2.0;
        assert!(-up - half_text > MarkerKind::Address.dot().1 + RING_PX);
        assert!(down - half_text > MarkerKind::Arrival.dot().1 + RING_PX);
    }

    #[test]
    fn fit_frames_both_points_and_caps_zoom() {
        let m = parse_markers(r#"[{"lon":140.0,"lat":38.0},{"lon":140.01,"lat":38.0}]"#).unwrap();
        let (lon, lat, z) = fit_markers(&m, 800.0, 600.0, [80.0; 4], 19.0).unwrap();
        assert!((lon - 140.005).abs() < 1e-9);
        assert!((lat - 38.0).abs() < 1e-6);
        // 0.01 deg of longitude = 1/36000 of the world; 640 px across it.
        let expect = (640.0 * 36000.0 / 512.0_f64).log2();
        assert!((z - expect).abs() < 1e-6, "{z} vs {expect}");
        let one = parse_markers(r#"[{"lon":140.0,"lat":38.0}]"#).unwrap();
        assert_eq!(fit_markers(&one, 800.0, 600.0, [80.0; 4], 17.0).unwrap().2, 17.0);
        assert!(fit_markers(&[], 800.0, 600.0, [80.0; 4], 17.0).is_none());
    }

    #[test]
    fn uneven_padding_moves_the_markers_into_the_free_box() {
        // A 400 px card on the left: the markers centre in x 400..800, so
        // the camera centre (x 400) sits 200 px WEST of their centre.
        let one = parse_markers(r#"[{"lon":140.0,"lat":38.0}]"#).unwrap();
        let (lon, lat, z) = fit_markers(&one, 800.0, 600.0, [0.0, 0.0, 0.0, 400.0], 16.0).unwrap();
        let deg_per_px = 360.0 / (512.0 * 2f64.powf(z));
        assert!((lon - (140.0 - 200.0 * deg_per_px)).abs() < 1e-9, "{lon}");
        assert!((lat - 38.0).abs() < 1e-9);
        // Top padding: the free box is lower, so the markers sit below the
        // camera centre and the camera is north of them.
        let (_, lat, _) = fit_markers(&one, 800.0, 600.0, [200.0, 0.0, 0.0, 0.0], 16.0).unwrap();
        assert!(lat > 38.0, "{lat}");
        // Garbage padding is no padding.
        assert_eq!(
            fit_markers(&one, 800.0, 600.0, [f64::NAN, -5.0, 0.0, 0.0], 16.0),
            fit_markers(&one, 800.0, 600.0, [0.0; 4], 16.0)
        );
    }
}
