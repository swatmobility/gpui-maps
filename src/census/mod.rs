//! The US census income demo's shared logic, compiled by the native demo
//! (`demos/us_block_groups.rs`) and by the browser shell (`#[path]`), and
//! tested here -- the browser shell has no running tests, so every decision
//! the page makes lives in this module.
//!
//! - [`pack`]: `BGPK` v1, the block groups a browser fetches by `Range`.
//! - [`ramp`]: the income ramp's paint, legend and label treatment.
//! - [`names`]: the popup's county/state/tract words.
//! - [`hover`]: what the popup shows, and when.
//! - [`slider`]: the income range slider's press-and-drag rules.
//! - [`hist`]: the income histogram, national and in view.
//! - [`view`]: which block groups the camera shows, and its histogram.
//! - [`breakdowns`]: the Breakdowns panel's per-state and per-metro maths.
//! - [`panel_json`]: the Breakdowns panel's words, order and JSON.

pub mod breakdowns;
pub mod hist;
pub mod hover;
pub mod names;
pub mod pack;
pub mod panel_json;
pub mod ramp;
pub mod slider;
pub mod view;

/// The per-state overlay source the page pushes a state's block groups as.
pub const SOURCE_PREFIX: &str = "block-groups-";

/// `block-groups-06` for FIPS 6: one source per state, as native names them.
pub fn source_name(fips: u8) -> String {
    format!("{SOURCE_PREFIX}{fips:02}")
}

/// `"block-groups-06"` -> `Some(6)`: the state a per-state source holds;
/// `None` for any other name (the demo's single national `"block-groups"`).
pub fn source_state(name: &str) -> Option<u32> {
    name.strip_prefix(SOURCE_PREFIX)?.parse().ok()
}

/// The camera's viewport as a lon/lat box `[west, south, east, north]`,
/// clamped to +-180 / +-85: what the page loads first. Flat Web Mercator at
/// the camera's zoom (512 px tiles); pitch widens it by the far edge's
/// stretch, bounded, since this only orders the fetches.
pub fn view_box(camera: &crate::render::Camera) -> [f64; 4] {
    let (cx, cy) = crate::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, camera.zoom);
    let tile = crate::geo::TILE_SIZE;
    let stretch = 1.0 / f64::from(camera.pitch).cos().max(0.25);
    let half_w = f64::from(camera.viewport_width_px) / 2.0 / tile * stretch;
    let half_h = f64::from(camera.viewport_height_px) / 2.0 / tile * stretch;
    let n = 2f64.powf(camera.zoom);
    let (west, north) = crate::geo::tile_xy_to_lon_lat((cx - half_w).max(0.0), (cy - half_h).max(0.0), camera.zoom);
    let (east, south) = crate::geo::tile_xy_to_lon_lat((cx + half_w).min(n), (cy + half_h).min(n), camera.zoom);
    [west.clamp(-180.0, 180.0), south.clamp(-85.0, 85.0), east.clamp(-180.0, 180.0), north.clamp(-85.0, 85.0)]
}

/// Where the census demo opens, on both shells (the native demo imports
/// it): the contiguous 48, centred near their geographic centre (Lebanon,
/// KS) as Kyle Walker's mapgl reference opens, at a zoom that fills a
/// 1280x800 window. MG-C11's parity camera.
pub const START: (f64, f64, f64) = (-98.35, 39.5, 3.7);

/// `?camera=lon,lat,zoom`: three finite numbers, the latitude within +-90
/// and the zoom within 0..=24; anything else is `None`.
pub fn parse_camera(text: &str) -> Option<(f64, f64, f64)> {
    let parts: Vec<f64> = text.split(',').map(|p| p.trim().parse::<f64>().ok().filter(|v| v.is_finite())).collect::<Option<_>>()?;
    match parts[..] {
        [lon, lat, zoom] if (-90.0..=90.0).contains(&lat) && (0.0..=24.0).contains(&zoom) => Some((lon, lat, zoom)),
        _ => None,
    }
}

/// The basemap a census page opens on when neither `?style=` nor `?tiles=`
/// chose one: Mapbox Light with a token, keyless Positron without -- the
/// native demo's pick. `None` leaves the page's own choice alone.
pub fn census_style_url(query_style: Option<&str>, query_tiles: Option<&str>, has_token: bool) -> Option<&'static str> {
    if query_style.is_some() || query_tiles.is_some() {
        return None;
    }
    Some(if has_token { "./mapbox-light-style.json" } else { "./positron-style.json" })
}

/// The sun a census page lights the globe with. A choropleth under the
/// night side hides its data, so by default the sun sits behind the viewer
/// (`Sun::Viewport` at tilt 0: the whole visible disc lit, no terminator).
/// `?sun=now` keeps the engine's wall-clock sun, and a pinned
/// `?sunlon=&sunlat=` (`pinned`) keeps that. `None`: leave the page's sun.
pub fn census_sun(sun_param: Option<&str>, pinned: bool) -> Option<crate::render::atmosphere::Sun> {
    if pinned || sun_param == Some("now") {
        return None;
    }
    Some(crate::render::atmosphere::Sun::Viewport { tilt_deg: 0.0, bearing_deg: 0.0 })
}

/// A `?census=` pack name: letters, digits, `_` or `-`, 1..=64 of them --
/// no dots or slashes, so it can only name a file in the server's census
/// directory.
pub fn valid_pack_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A style document with the census demo's label treatment applied
/// ([`ramp::readable_place_labels`]), as native applies it before
/// compiling. Bytes that are not a JSON object come back unchanged, for the
/// style compiler to refuse with its own error.
pub fn census_style_bytes(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(bytes) else { return bytes.to_vec() };
    if !json.is_object() {
        return bytes.to_vec();
    }
    ramp::readable_place_labels(&mut json);
    serde_json::to_vec(&json).unwrap_or_else(|_| bytes.to_vec())
}

/// Load progress, as the page's `map_census_status_json` reports it and the
/// frame loop advances it.
///
/// "Every state drew" cannot mean "all 56 sources drew in one frame": at
/// the opening camera Guam and the Northern Marianas are on the far side
/// of the globe and never draw. It means: every state is pushed, and the
/// overlay layer has no work left for this view (no index to build, no
/// bake queued, nothing landed and not yet uploaded) -- so the last frame
/// drawn holds every block group the view shows. [`Self::all_drawn_ms`]
/// is that last frame's time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LoadStatus {
    pub loaded: u32,
    pub total: u32,
    /// Milliseconds from `started_ms` to the frame that completed the
    /// view with every state pushed; `None` until then.
    pub all_drawn_ms: Option<f64>,
    /// Census sources that drew in the last frame drawn.
    pub sources_drawn: u32,
    /// Frames the page drew since the census layer started: the redraw
    /// counter the hover gate reads (a hover must not move it).
    pub frames_drawn: u64,
    last_draw_ms: Option<f64>,
}

impl LoadStatus {
    /// Nothing loaded yet, of `total` states.
    pub fn new(total: u32) -> Self {
        Self { total, ..Self::default() }
    }

    /// A frame drew at `now_ms`, `sources_drawn` of the census sources in it.
    pub fn drew(&mut self, sources_drawn: u32, now_ms: f64) {
        self.frames_drawn += 1;
        self.sources_drawn = sources_drawn;
        self.last_draw_ms = Some(now_ms);
    }

    /// The overlay layer's verdict for this tick (`ShellOverlays::update`'s
    /// `true` = work still owed). Stamps [`Self::all_drawn_ms`] once: the
    /// first idle tick with every state pushed and a frame drawn.
    pub fn overlays(&mut self, busy: bool, started_ms: f64) {
        if busy || self.all_drawn_ms.is_some() || self.total == 0 || self.loaded < self.total {
            return;
        }
        if let Some(at) = self.last_draw_ms {
            self.all_drawn_ms = Some(at - started_ms);
        }
    }

    pub fn json(&self) -> String {
        serde_json::json!({
            "loaded": self.loaded,
            "total": self.total,
            "all_drawn_ms": self.all_drawn_ms,
            "sources_drawn": self.sources_drawn,
            "frames_drawn": self.frames_drawn,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(lon: f64, lat: f64, zoom: f64) -> crate::render::Camera {
        crate::render::Camera {
            center_lon: lon,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: 1280.0,
            viewport_height_px: 800.0,
            fov_y: crate::render::Camera::DEFAULT_FOV_Y,
        }
    }

    #[test]
    fn view_box_of_the_start_camera_contains_kansas() {
        let [w, s, e, n] = view_box(&camera(-98.35, 39.5, 3.7));
        // Kansas: 102.05..94.59 W, 37..40 N -- and at z3.7 the whole
        // contiguous US.
        assert!(w < -102.05 && e > -94.59 && s < 37.0 && n > 40.0, "{w} {s} {e} {n}");
        assert!(w < -124.0 && e > -67.0, "the lower 48 fit: {w} {e}");
        assert!(e < 0.0, "not the whole world: {e}");
    }

    #[test]
    fn view_box_is_clamped_at_world_zoom() {
        assert_eq!(view_box(&camera(0.0, 0.0, 0.0)), [-180.0, -85.0, 180.0, 85.0]);
    }

    #[test]
    fn source_names_are_zero_padded_like_native() {
        assert_eq!(source_name(6), "block-groups-06");
        assert_eq!(source_name(72), "block-groups-72");
    }

    #[test]
    fn a_source_name_gives_its_state_back() {
        assert_eq!(source_state(&source_name(6)), Some(6));
        assert_eq!(source_state("block-groups-72"), Some(72));
        assert_eq!(source_state("block-groups"), None);
        assert_eq!(source_state("block-groups-"), None);
        assert_eq!(source_state("roads-06"), None);
    }

    #[test]
    fn all_drawn_is_the_last_frame_before_the_overlays_went_idle() {
        let mut s = LoadStatus::new(3);
        s.loaded = 2;
        s.drew(2, 900.0);
        s.overlays(false, 100.0);
        assert_eq!(s.all_drawn_ms, None, "a state is still to come");
        s.loaded = 3;
        s.overlays(true, 100.0);
        s.drew(2, 1_500.0);
        s.overlays(true, 100.0);
        assert_eq!(s.all_drawn_ms, None, "bakes still owed");
        s.overlays(false, 100.0);
        assert_eq!(s.all_drawn_ms, Some(1_400.0), "the last frame drawn completed the view");
        s.drew(3, 9_000.0);
        s.overlays(false, 100.0);
        assert_eq!(s.all_drawn_ms, Some(1_400.0), "never restamped");
        assert_eq!(s.frames_drawn, 3);
        let v: serde_json::Value = serde_json::from_str(&s.json()).unwrap();
        assert_eq!(v["sources_drawn"], 3);
        assert_eq!(v["all_drawn_ms"], 1_400.0);
        let empty: serde_json::Value = serde_json::from_str(&LoadStatus::default().json()).unwrap();
        assert!(empty["all_drawn_ms"].is_null());
        let mut never_drew = LoadStatus { loaded: 1, ..LoadStatus::new(1) };
        never_drew.overlays(false, 0.0);
        assert_eq!(never_drew.all_drawn_ms, None, "idle before any frame drew is not drawn");
    }

    #[test]
    fn a_camera_param_is_three_finite_numbers() {
        assert_eq!(parse_camera("-98.35,39.5,3.7"), Some((-98.35, 39.5, 3.7)));
        assert_eq!(parse_camera(" -73.98, 40.75 ,10"), Some((-73.98, 40.75, 10.0)));
        for bad in ["", "1,2", "1,2,3,4", "a,b,c", "0,91,3", "0,0,NaN", "0,0,99", "0,0,inf"] {
            assert_eq!(parse_camera(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_census_page_picks_light_with_a_token_and_positron_without() {
        assert_eq!(census_style_url(None, None, true), Some("./mapbox-light-style.json"));
        assert_eq!(census_style_url(None, None, false), Some("./positron-style.json"));
        assert_eq!(census_style_url(Some("./liberty-style.json"), None, true), None, "?style= wins");
        assert_eq!(census_style_url(None, Some("https://t/{z}/{x}/{y}.pbf"), false), None, "?tiles= wins");
    }

    #[test]
    fn a_pack_name_cannot_leave_the_census_directory() {
        assert!(valid_pack_name("us_bg_2023"));
        assert!(valid_pack_name("ri-dc"));
        for bad in ["", "../serve", "a/b", "x.bgpk", "a%2fb", &"a".repeat(65)] {
            assert!(!valid_pack_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_census_style_darkens_light_labels_and_passes_junk_through() {
        let style = br##"{"version":8,"layers":[{"id":"state-label","paint":{"text-color":"#999"}}]}"##;
        let out: serde_json::Value = serde_json::from_slice(&census_style_bytes(style)).unwrap();
        assert_eq!(out["layers"][0]["paint"]["text-color"], "#1f1f24");
        assert_eq!(census_style_bytes(b"not json"), b"not json");
        assert_eq!(census_style_bytes(b"[1]"), b"[1]");
    }

    #[test]
    fn a_census_page_is_daylit_unless_asked_for_the_real_sun() {
        use crate::render::atmosphere::Sun;
        assert_eq!(census_sun(None, false), Some(Sun::Viewport { tilt_deg: 0.0, bearing_deg: 0.0 }));
        assert_eq!(census_sun(Some("noon"), false), Some(Sun::Viewport { tilt_deg: 0.0, bearing_deg: 0.0 }));
        assert_eq!(census_sun(Some("now"), false), None, "?sun=now: the terminator, as on any page");
        assert_eq!(census_sun(None, true), None, "a pinned ?sunlon=&sunlat= wins");
    }
}
