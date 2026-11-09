//! OGC Web Map Service (WMS) as a raster [`TileSource`]: one `GetMap`
//! request per map tile, its `BBOX` the tile's own square.
//!
//! WMS draws whatever box it is asked for, so a slippy-map tile is just a
//! box: [`WmsSource::get_map_url`] turns `z/x/y` into the tile's extent in
//! the source's CRS, and the answer (a transparent PNG by default) goes
//! down the ordinary imagery path -- decoded by `TileSource::fetch_rgba`'s
//! default, held in `MapView`'s bounded raster cache, drawn over the
//! basemap with `MapView::set_raster_above_basemap`. This is MapLibre's
//! `{bbox-epsg-3857}` tile-URL convention, spelled as a source.
//!
//! # Versions and axis order
//!
//! - **1.1.1** names the CRS with `SRS=` and always writes `BBOX` as
//!   `minx,miny,maxx,maxy` in the CRS's own units -- for `EPSG:4326` that
//!   is `lon,lat,lon,lat`.
//! - **1.3.0** names it with `CRS=` and honours the CRS's DECLARED axis
//!   order. `EPSG:3857` is easting/northing, so nothing changes; but
//!   `EPSG:4326` is latitude first, so its `BBOX` is `minlat,minlon,
//!   maxlat,maxlon` (WMS 1.3.0, OGC 06-042, 6.7.3.3). `CRS:84` is the
//!   lon/lat spelling of the same datum, for a client that wants the old
//!   order.
//!
//! `EPSG:3857` is the default and the one to prefer: a tile is then an
//! exact Mercator square and the image lands on the tile pixel for pixel.
//! A server without 3857 can be asked in geographic coordinates; the image
//! is then linear in latitude inside each tile, which is invisible past a
//! few zoom levels and a slight vertical stretch at continental zooms.
//!
//! # Time
//!
//! A layer with a `time` dimension (radar, satellite) serves its default
//! instant when a request names none, and that instant moves as the server
//! ingests. [`WmsSource::with_time`] pins one, so every tile of a frame is
//! the same instant and the instant can be printed beside the map;
//! [`WmsLayer::time`] carries the instants the capabilities list.
//!
//! # Errors
//!
//! A WMS server reports a bad request as `200 OK` with an XML
//! `ServiceExceptionReport` body. That is an error here, with the
//! exception's own text, never an image decode failure.

use crate::geo::TileId;
use crate::tile_source::TileSource;

/// Half the EPSG:3857 world's width in metres: `pi * 6378137`.
pub const MERCATOR_HALF_WORLD_M: f64 = 20_037_508.342_789_244;

/// A WMS protocol version: the two in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmsVersion {
    V1_1_1,
    V1_3_0,
}

impl WmsVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_1_1 => "1.1.1",
            Self::V1_3_0 => "1.3.0",
        }
    }

    /// `1.1.x` and older are 1.1.1's dialect; anything else is 1.3.0's.
    pub fn parse(s: &str) -> Self {
        if s.starts_with("1.1") || s.starts_with("1.0") {
            Self::V1_1_1
        } else {
            Self::V1_3_0
        }
    }
}

/// The CRS a `GetMap` asks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmsCrs {
    /// Web Mercator, metres. The tile is an exact square; the default.
    Epsg3857,
    /// WGS 84 geographic. Latitude first under 1.3.0.
    Epsg4326,
    /// WGS 84 geographic, longitude first in every version (1.3.0 only).
    Crs84,
}

impl WmsCrs {
    pub fn code(self) -> &'static str {
        match self {
            Self::Epsg3857 => "EPSG:3857",
            Self::Epsg4326 => "EPSG:4326",
            Self::Crs84 => "CRS:84",
        }
    }
}

/// What a [`WmsSource`] asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct WmsConfig {
    /// The service endpoint, with or without a query string of its own
    /// (`.../ows` and `.../ows?SERVICE=WMS&` both work).
    pub url: String,
    /// `LAYERS`, drawn bottom to top.
    pub layers: Vec<String>,
    /// `STYLES`, one per layer or empty for each layer's default.
    pub styles: Vec<String>,
    /// `FORMAT`: `image/png` unless the server only has something else.
    pub format: String,
    pub transparent: bool,
    pub version: WmsVersion,
    pub crs: WmsCrs,
    /// `TIME`, or `None` for the layer's default instant.
    pub time: Option<String>,
    /// `WIDTH` and `HEIGHT`: the tile's edge in pixels, which must be the
    /// `tile_size` the source is installed with.
    pub tile_size: u32,
    /// The data's extent as `[west, south, east, north]` in degrees: a
    /// tile outside it is absent (`Ok(None)`) without a request.
    pub bounds: Option<[f64; 4]>,
    pub max_zoom: u8,
}

impl WmsConfig {
    /// A 1.3.0, EPSG:3857, transparent-PNG, 256 px request for `layers`.
    pub fn new(url: impl Into<String>, layers: &[&str]) -> Self {
        Self {
            url: url.into(),
            layers: layers.iter().map(|l| (*l).to_string()).collect(),
            styles: Vec::new(),
            format: "image/png".into(),
            transparent: true,
            version: WmsVersion::V1_3_0,
            crs: WmsCrs::Epsg3857,
            time: None,
            tile_size: 256,
            bounds: None,
            max_zoom: 18,
        }
    }
}

/// `tile`'s extent in EPSG:3857 metres, `[minx, miny, maxx, maxy]`.
#[must_use]
pub fn tile_bbox_3857(tile: TileId) -> [f64; 4] {
    let size = 2.0 * MERCATOR_HALF_WORLD_M / f64::from(1u32 << tile.z.min(31));
    let minx = -MERCATOR_HALF_WORLD_M + f64::from(tile.x) * size;
    let maxy = MERCATOR_HALF_WORLD_M - f64::from(tile.y) * size;
    [minx, maxy - size, minx + size, maxy]
}

/// `tile`'s extent in degrees, `[west, south, east, north]`.
#[must_use]
pub fn tile_bbox_lonlat(tile: TileId) -> [f64; 4] {
    let b = crate::geo::tile_bounds_lonlat(tile);
    [b.min_lon, b.min_lat, b.max_lon, b.max_lat]
}

/// Percent-encodes a query value: everything but RFC 3986's unreserved
/// characters and `,` and `:` (which every WMS server reads literally in
/// `BBOX`, `LAYERS` and `CRS`).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b',' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `url` ready for more `&key=value` pairs.
fn base_with_query(url: &str) -> String {
    if !url.contains('?') {
        format!("{url}?")
    } else if url.ends_with('?') || url.ends_with('&') {
        url.to_string()
    } else {
        format!("{url}&")
    }
}

/// Whether `url`'s own query already says `key` (any case).
fn query_has(url: &str, key: &str) -> bool {
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&').any(|pair| pair.split('=').next().is_some_and(|k| k.eq_ignore_ascii_case(key)))
    })
}

/// A GetMap request URL for `tile` under `config`. Pure: what the source
/// fetches, and what the tests pin.
#[must_use]
pub fn get_map_url(config: &WmsConfig, tile: TileId) -> String {
    let bbox = match config.crs {
        WmsCrs::Epsg3857 => tile_bbox_3857(tile),
        WmsCrs::Crs84 => tile_bbox_lonlat(tile),
        WmsCrs::Epsg4326 => {
            let [w, s, e, n] = tile_bbox_lonlat(tile);
            match config.version {
                WmsVersion::V1_1_1 => [w, s, e, n],
                // 1.3.0 honours EPSG:4326's latitude-first axis order.
                WmsVersion::V1_3_0 => [s, w, n, e],
            }
        }
    };
    let crs_key = match config.version {
        WmsVersion::V1_1_1 => "SRS",
        WmsVersion::V1_3_0 => "CRS",
    };
    let mut url = base_with_query(&config.url);
    if !query_has(&config.url, "SERVICE") {
        url.push_str("SERVICE=WMS&");
    }
    let styles = if config.styles.is_empty() { String::new() } else { config.styles.join(",") };
    url.push_str(&format!(
        "VERSION={}&REQUEST=GetMap&LAYERS={}&STYLES={}&FORMAT={}&TRANSPARENT={}&{crs_key}={}&BBOX={},{},{},{}&WIDTH={}&HEIGHT={}",
        config.version.as_str(),
        encode(&config.layers.join(",")),
        encode(&styles),
        encode(&config.format),
        if config.transparent { "TRUE" } else { "FALSE" },
        config.crs.code(),
        bbox[0],
        bbox[1],
        bbox[2],
        bbox[3],
        config.tile_size,
        config.tile_size,
    ));
    if let Some(time) = &config.time {
        url.push_str("&TIME=");
        url.push_str(&encode(time));
    }
    url
}

/// A WMS layer as `GetCapabilities` describes it, with what it inherits
/// from its parents (CRS list, extent, time, attribution) already folded
/// in, as the specification says a client must.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WmsLayer {
    /// `None` for a category layer that cannot be requested by itself.
    pub name: Option<String>,
    pub title: String,
    pub abstract_text: String,
    /// `CRS` (1.3.0) or `SRS` (1.1.1) codes.
    pub crs: Vec<String>,
    /// `[west, south, east, north]` in degrees.
    pub bounds: Option<[f64; 4]>,
    pub time: Option<WmsTime>,
    /// The layer's `Attribution/Title`.
    pub attribution: Option<String>,
    pub queryable: bool,
    pub opaque: bool,
}

impl WmsLayer {
    /// Whether the layer can be asked for in `crs` (inheritance applied).
    pub fn supports(&self, crs: WmsCrs) -> bool {
        self.crs.iter().any(|c| c.eq_ignore_ascii_case(crs.code()))
    }
}

/// A `time` dimension.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WmsTime {
    /// The instant a request without `TIME` gets.
    pub default: Option<String>,
    /// The listed instants, in the server's order. Empty when the extent
    /// is an interval (`start/end/period`), kept in `extent`.
    pub values: Vec<String>,
    /// The extent as written.
    pub extent: String,
}

impl WmsTime {
    /// The newest listed instant, else the default.
    pub fn latest(&self) -> Option<&str> {
        self.values.iter().max().map(String::as_str).or(self.default.as_deref())
    }
}

/// What `GetCapabilities` says about a service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WmsCapabilities {
    pub version: String,
    pub title: String,
    /// `Fees` and `AccessConstraints`, as the operator states them: the
    /// terms of use a client has to read.
    pub fees: Option<String>,
    pub access_constraints: Option<String>,
    /// GetMap's `Format`s.
    pub formats: Vec<String>,
    /// GetMap's GET endpoint, when it differs from the capabilities URL.
    pub get_map_url: Option<String>,
    /// Every layer, depth first, parents before children.
    pub layers: Vec<WmsLayer>,
}

impl WmsCapabilities {
    /// The named layer `name`.
    pub fn layer(&self, name: &str) -> Option<&WmsLayer> {
        self.layers.iter().find(|l| l.name.as_deref() == Some(name))
    }

    pub fn wms_version(&self) -> WmsVersion {
        WmsVersion::parse(&self.version)
    }
}

/// Parses a `GetCapabilities` document, 1.1.1 or 1.3.0 (namespaced or not).
pub fn parse_capabilities(xml: &str) -> anyhow::Result<WmsCapabilities> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| anyhow::anyhow!("WMS capabilities are not XML: {e}"))?;
    let root = doc.root_element();
    let root_name = root.tag_name().name();
    if root_name == "ServiceExceptionReport" {
        anyhow::bail!("WMS service exception: {}", exception_text(root));
    }
    if !root_name.ends_with("Capabilities") {
        anyhow::bail!("not a WMS capabilities document (root <{root_name}>)");
    }
    let mut caps = WmsCapabilities { version: root.attribute("version").unwrap_or("1.3.0").to_string(), ..Default::default() };
    if let Some(service) = child(root, "Service") {
        caps.title = text_of(service, "Title").unwrap_or_default();
        caps.fees = text_of(service, "Fees");
        caps.access_constraints = text_of(service, "AccessConstraints");
    }
    let Some(capability) = child(root, "Capability") else { return Ok(caps) };
    if let Some(get_map) = child(capability, "Request").and_then(|r| child(r, "GetMap")) {
        caps.formats = children(get_map, "Format").filter_map(|f| f.text()).map(|t| t.trim().to_string()).collect();
        caps.get_map_url = get_map
            .descendants()
            .find(|n| n.tag_name().name() == "Get")
            .and_then(|get| child(get, "OnlineResource"))
            .and_then(href);
    }
    for layer in children(capability, "Layer") {
        walk_layer(layer, &WmsLayer::default(), &mut caps.layers);
    }
    Ok(caps)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == name)
}

fn children<'a, 'i: 'a>(node: roxmltree::Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> + 'a {
    node.children().filter(move |n| n.is_element() && n.tag_name().name() == name)
}

fn text_of(node: roxmltree::Node, name: &str) -> Option<String> {
    child(node, name).and_then(|n| n.text()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn href(node: roxmltree::Node) -> Option<String> {
    node.attribute(("http://www.w3.org/1999/xlink", "href")).or_else(|| node.attribute("href")).map(str::to_string)
}

fn exception_text(node: roxmltree::Node) -> String {
    let text: Vec<&str> = node.descendants().filter_map(|n| n.text()).map(str::trim).filter(|t| !t.is_empty()).collect();
    if text.is_empty() {
        "(no message)".into()
    } else {
        text.join(" ")
    }
}

fn num(node: roxmltree::Node, attr: &str) -> Option<f64> {
    node.attribute(attr)?.trim().parse().ok()
}

fn walk_layer(node: roxmltree::Node, parent: &WmsLayer, out: &mut Vec<WmsLayer>) {
    let mut layer = WmsLayer {
        name: text_of(node, "Name"),
        title: text_of(node, "Title").unwrap_or_default(),
        abstract_text: text_of(node, "Abstract").unwrap_or_default(),
        crs: parent.crs.clone(),
        bounds: parent.bounds,
        time: parent.time.clone(),
        attribution: parent.attribution.clone(),
        queryable: node.attribute("queryable").is_some_and(|v| v == "1" || v == "true"),
        opaque: node.attribute("opaque").is_some_and(|v| v == "1" || v == "true"),
    };
    // CRS lists ADD to the parent's (OGC 06-042 7.2.4.8, table 7).
    for c in node.children().filter(|n| n.is_element() && matches!(n.tag_name().name(), "CRS" | "SRS")) {
        for code in c.text().unwrap_or_default().split_whitespace() {
            if !layer.crs.iter().any(|have| have.eq_ignore_ascii_case(code)) {
                layer.crs.push(code.to_string());
            }
        }
    }
    // The extent REPLACES the parent's.
    if let Some(g) = child(node, "EX_GeographicBoundingBox") {
        let v = |name: &str| text_of(g, name).and_then(|t| t.parse::<f64>().ok());
        if let (Some(w), Some(e), Some(s), Some(n)) =
            (v("westBoundLongitude"), v("eastBoundLongitude"), v("southBoundLatitude"), v("northBoundLatitude"))
        {
            layer.bounds = Some([w, s, e, n]);
        }
    } else if let Some(g) = child(node, "LatLonBoundingBox") {
        if let (Some(w), Some(s), Some(e), Some(n)) = (num(g, "minx"), num(g, "miny"), num(g, "maxx"), num(g, "maxy")) {
            layer.bounds = Some([w, s, e, n]);
        }
    }
    // 1.3.0: `<Dimension name="time" default=..>values</Dimension>`.
    // 1.1.1: `<Dimension name="time"/>` declares it and `<Extent
    // name="time" default=..>values</Extent>` gives the values.
    let is_time = |n: &roxmltree::Node| n.attribute("name").is_some_and(|v| v.eq_ignore_ascii_case("time"));
    let time_node = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Extent")
        .find(is_time)
        .or_else(|| node.children().filter(|n| n.is_element() && n.tag_name().name() == "Dimension").find(is_time));
    if let Some(t) = time_node {
        let extent = t.text().unwrap_or_default().trim().to_string();
        let values = if extent.contains('/') {
            Vec::new()
        } else {
            extent.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).collect()
        };
        layer.time = Some(WmsTime { default: t.attribute("default").map(str::to_string), values, extent });
    }
    if let Some(title) = child(node, "Attribution").and_then(|a| text_of(a, "Title")) {
        layer.attribution = Some(title);
    }
    out.push(layer.clone());
    for sub in children(node, "Layer") {
        walk_layer(sub, &layer, out);
    }
}

/// The `GetCapabilities` URL for a service endpoint.
#[must_use]
pub fn capabilities_url(url: &str, version: WmsVersion) -> String {
    let mut out = base_with_query(url);
    if !query_has(url, "SERVICE") {
        out.push_str("SERVICE=WMS&");
    }
    out.push_str(&format!("REQUEST=GetCapabilities&VERSION={}", version.as_str()));
    out
}

/// Fetches and parses a service's capabilities, with the project's
/// User-Agent (native and browser alike: reqwest is `fetch` on wasm).
pub async fn fetch_capabilities(url: &str, version: WmsVersion) -> anyhow::Result<WmsCapabilities> {
    let client = crate::tile_source::http::tile_client();
    let response = client.get(capabilities_url(url, version)).send().await?.error_for_status()?;
    parse_capabilities(&response.text().await?)
}

/// A WMS layer (or layers) as a raster tile source.
pub struct WmsSource {
    config: WmsConfig,
    client: reqwest::Client,
    attribution: String,
}

impl WmsSource {
    /// `attribution` is the data's credit, as for
    /// [`crate::tile_source::http::HttpTileSource::new`]: required, `""`
    /// to state that none is owed.
    pub fn new(config: WmsConfig, attribution: impl Into<String>) -> Self {
        Self {
            config,
            client: crate::tile_source::http::tile_client(),
            attribution: crate::tile_source::attribution_text(&attribution.into()),
        }
    }

    /// A source for `layer` of an already-read capabilities document: its
    /// version, its GetMap endpoint, its extent, and its attribution when
    /// it states one (else `fallback_attribution`). EPSG:3857 when the
    /// layer offers it, otherwise geographic in the version's axis order.
    pub fn from_capabilities(
        url: &str,
        caps: &WmsCapabilities,
        layer: &str,
        fallback_attribution: &str,
    ) -> anyhow::Result<Self> {
        let found = caps.layer(layer).ok_or_else(|| anyhow::anyhow!("the WMS has no layer {layer}"))?;
        let version = caps.wms_version();
        let mut config = WmsConfig::new(caps.get_map_url.clone().unwrap_or_else(|| url.to_string()), &[layer]);
        config.version = version;
        config.bounds = found.bounds;
        config.crs = if found.supports(WmsCrs::Epsg3857) || found.crs.iter().any(|c| c == "EPSG:900913") {
            WmsCrs::Epsg3857
        } else if version == WmsVersion::V1_3_0 && found.supports(WmsCrs::Crs84) {
            WmsCrs::Crs84
        } else {
            WmsCrs::Epsg4326
        };
        if !caps.formats.is_empty() && !caps.formats.iter().any(|f| f == "image/png") {
            if let Some(f) = caps.formats.iter().find(|f| f.starts_with("image/")) {
                config.format = f.clone();
            }
        }
        let credit = found.attribution.clone().unwrap_or_else(|| fallback_attribution.to_string());
        Ok(Self::new(config, credit))
    }

    /// This source pinned to one instant of a time-dimension layer.
    #[must_use]
    pub fn with_time(mut self, time: Option<String>) -> Self {
        self.config.time = time;
        self
    }

    pub fn config(&self) -> &WmsConfig {
        &self.config
    }

    /// The GetMap URL for `tile`.
    pub fn get_map_url(&self, tile: TileId) -> String {
        get_map_url(&self.config, tile)
    }

    /// Whether `tile` misses the configured extent.
    fn outside(&self, tile: TileId) -> bool {
        let Some([w, s, e, n]) = self.config.bounds else { return false };
        let [tw, ts, te, tn] = tile_bbox_lonlat(tile);
        te < w || tw > e || tn < s || ts > n
    }
}

#[async_trait::async_trait]
impl TileSource for WmsSource {
    /// A tile outside the layer's extent is absent with no request. A
    /// `ServiceExceptionReport` answered with `200 OK` is an error carrying
    /// the server's message.
    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        if self.outside(tile) {
            return Ok(None);
        }
        let url = self.get_map_url(tile);
        let redacted = |e: reqwest::Error| anyhow::anyhow!("{}", crate::sprite::redact_url_credentials(&e.to_string()));
        let response = self.client.get(&url).send().await.map_err(redacted)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status().map_err(redacted)?;
        let xml = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("xml"));
        let body = response.bytes().await.map_err(redacted)?.to_vec();
        if xml || body.first() == Some(&b'<') {
            let text = String::from_utf8_lossy(&body);
            let message = roxmltree::Document::parse(&text)
                .map(|d| exception_text(d.root_element()))
                .unwrap_or_else(|_| text.chars().take(200).collect());
            anyhow::bail!("WMS service exception: {message}");
        }
        Ok(Some(body))
    }

    fn max_zoom(&self) -> u8 {
        self.config.max_zoom
    }

    fn attribution(&self) -> Option<&str> {
        (!self.attribution.is_empty()).then_some(self.attribution.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn query(url: &str) -> std::collections::HashMap<String, String> {
        url.split_once('?')
            .map(|(_, q)| q)
            .unwrap_or_default()
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn bbox(url: &str) -> Vec<f64> {
        query(url)["BBOX"].split(',').map(|v| v.parse().unwrap()).collect()
    }

    #[test]
    fn the_world_tile_is_the_whole_mercator_square() {
        let b = tile_bbox_3857(TileId { z: 0, x: 0, y: 0 });
        assert_eq!(b, [-MERCATOR_HALF_WORLD_M, -MERCATOR_HALF_WORLD_M, MERCATOR_HALF_WORLD_M, MERCATOR_HALF_WORLD_M]);
        // z1 x0 y0 is the north-west quarter.
        assert_eq!(tile_bbox_3857(TileId { z: 1, x: 0, y: 0 }), [-MERCATOR_HALF_WORLD_M, 0.0, 0.0, MERCATOR_HALF_WORLD_M]);
    }

    #[test]
    fn get_map_1_3_0_in_3857_names_crs_and_keeps_easting_first() {
        let config = WmsConfig::new("https://example.test/geoserver/ows", &["conus_cref_qcd"]);
        let tile = TileId { z: 4, x: 3, y: 6 };
        let url = get_map_url(&config, tile);
        let q = query(&url);
        assert!(url.starts_with("https://example.test/geoserver/ows?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap"), "{url}");
        assert_eq!(q["CRS"], "EPSG:3857");
        assert!(!q.contains_key("SRS"));
        assert_eq!(q["LAYERS"], "conus_cref_qcd");
        assert_eq!(q["FORMAT"], "image%2Fpng");
        assert_eq!(q["TRANSPARENT"], "TRUE");
        assert_eq!((q["WIDTH"].as_str(), q["HEIGHT"].as_str()), ("256", "256"));
        assert!(!q.contains_key("TIME"));
        let b = bbox(&url);
        let want = tile_bbox_3857(tile);
        assert_eq!(b, want.to_vec());
        // x=3 of 16 at z4: west of the antimeridian's opposite, south of north.
        assert!(b[0] < b[2] && b[1] < b[3]);
        assert!((b[2] - b[0] - 2.0 * MERCATOR_HALF_WORLD_M / 16.0).abs() < 1e-6);
    }

    #[test]
    fn get_map_1_1_1_names_srs_with_the_same_3857_box() {
        let mut config = WmsConfig::new("https://example.test/wms?SERVICE=WMS&", &["a", "b"]);
        config.version = WmsVersion::V1_1_1;
        config.styles = vec!["s1".into(), String::new()];
        let tile = TileId { z: 2, x: 1, y: 1 };
        let url = get_map_url(&config, tile);
        let q = query(&url);
        // The endpoint's own SERVICE is not repeated.
        assert_eq!(url.matches("SERVICE=").count(), 1, "{url}");
        assert_eq!(q["VERSION"], "1.1.1");
        assert_eq!(q["SRS"], "EPSG:3857");
        assert!(!q.contains_key("CRS"));
        assert_eq!(q["LAYERS"], "a,b");
        assert_eq!(q["STYLES"], "s1,");
        assert_eq!(bbox(&url), tile_bbox_3857(tile).to_vec());
    }

    #[test]
    fn epsg_4326_is_lat_first_under_1_3_0_and_lon_first_under_1_1_1() {
        let tile = TileId { z: 3, x: 1, y: 2 };
        let [w, s, e, n] = tile_bbox_lonlat(tile);
        let mut config = WmsConfig::new("https://example.test/wms", &["l"]);
        config.crs = WmsCrs::Epsg4326;
        config.version = WmsVersion::V1_3_0;
        assert_eq!(bbox(&get_map_url(&config, tile)), vec![s, w, n, e]);
        config.version = WmsVersion::V1_1_1;
        assert_eq!(bbox(&get_map_url(&config, tile)), vec![w, s, e, n]);
        // CRS:84 is lon/lat even under 1.3.0.
        config.version = WmsVersion::V1_3_0;
        config.crs = WmsCrs::Crs84;
        let url = get_map_url(&config, tile);
        assert_eq!(query(&url)["CRS"], "CRS:84");
        assert_eq!(bbox(&url), vec![w, s, e, n]);
        // Sanity: the tile is west of Greenwich, north of the equator.
        assert!(w < e && s < n && e < 0.0 && s > 0.0, "{w} {s} {e} {n}");
    }

    #[test]
    fn a_pinned_time_is_sent_encoded() {
        let mut config = WmsConfig::new("https://example.test/wms", &["radar"]);
        config.time = Some("2026-10-04T19:34:41.000Z".into());
        let url = get_map_url(&config, TileId { z: 0, x: 0, y: 0 });
        assert!(url.ends_with("&TIME=2026-10-04T19:34:41.000Z"), "{url}");
    }

    /// The live NCEP document's shape, trimmed: namespaced 1.3.0, the
    /// CRS list split between the root layer and the named one, a time
    /// dimension with a default.
    const NCEP_1_3_0: &str = r#"<?xml version="1.0" encoding="UTF-8"?><WMS_Capabilities version="1.3.0" xmlns="http://www.opengis.net/wms" xmlns:xlink="http://www.w3.org/1999/xlink">
  <Service><Name>WMS</Name><Title/><Fees>none</Fees><AccessConstraints>none</AccessConstraints></Service>
  <Capability>
    <Request><GetMap><Format>image/png</Format><Format>image/jpeg</Format>
      <DCPType><HTTP><Get><OnlineResource xlink:type="simple" xlink:href="https://opengeo.ncep.noaa.gov/geoserver/conus/conus_cref_qcd/ows?SERVICE=WMS&amp;"/></Get></HTTP></DCPType>
    </GetMap></Request>
    <Layer><Title/>
      <CRS>EPSG:3857</CRS><CRS>EPSG:4326</CRS><CRS>CRS:84</CRS>
      <EX_GeographicBoundingBox><westBoundLongitude>-130.0</westBoundLongitude><eastBoundLongitude>-60.0000015646219</eastBoundLongitude><southBoundLatitude>20.000000782311</southBoundLatitude><northBoundLatitude>55.0</northBoundLatitude></EX_GeographicBoundingBox>
      <Layer queryable="1" opaque="0">
        <Name>conus_cref_qcd</Name><Title>conus_cref_qcd</Title>
        <Abstract>Quality Controlled 1km x 1km CONUS Radar Composite Reflectivity.</Abstract>
        <CRS>EPSG:4326</CRS>
        <EX_GeographicBoundingBox><westBoundLongitude>-130.0</westBoundLongitude><eastBoundLongitude>-60.0000015646219</eastBoundLongitude><southBoundLatitude>20.000000782311</southBoundLatitude><northBoundLatitude>55.0</northBoundLatitude></EX_GeographicBoundingBox>
        <Dimension name="time" default="2026-10-04T19:34:41Z" units="ISO8601" nearestValue="1">2026-10-04T17:36:43.000Z,2026-10-04T19:34:41.000Z,2026-10-04T18:00:39.000Z</Dimension>
      </Layer>
    </Layer>
  </Capability>
</WMS_Capabilities>"#;

    const OTHER_1_1_1: &str = r#"<?xml version="1.0"?><WMT_MS_Capabilities version="1.1.1">
  <Service><Name>OGC:WMS</Name><Title>Test radar</Title><AccessConstraints>Public domain</AccessConstraints></Service>
  <Capability>
    <Request><GetMap><Format>image/png</Format></GetMap></Request>
    <Layer><Title>root</Title><SRS>EPSG:4326</SRS>
      <Attribution><Title>NOAA/NWS</Title></Attribution>
      <Layer><Name>radar</Name><Title>Radar</Title><SRS>EPSG:900913 EPSG:3857</SRS>
        <LatLonBoundingBox minx="-125" miny="24" maxx="-66" maxy="50"/>
        <Dimension name="time" units="ISO8601"/>
        <Extent name="time" default="2026-10-04T19:30:00Z">2026-10-04T19:00:00Z/2026-10-04T19:30:00Z/PT5M</Extent>
      </Layer>
    </Layer>
  </Capability>
</WMT_MS_Capabilities>"#;

    #[test]
    fn capabilities_1_3_0_parse_with_inheritance_and_time() {
        let caps = parse_capabilities(NCEP_1_3_0).expect("parses");
        assert_eq!(caps.version, "1.3.0");
        assert_eq!(caps.wms_version(), WmsVersion::V1_3_0);
        assert_eq!(caps.fees.as_deref(), Some("none"));
        assert_eq!(caps.access_constraints.as_deref(), Some("none"));
        assert_eq!(caps.formats, vec!["image/png", "image/jpeg"]);
        assert_eq!(
            caps.get_map_url.as_deref(),
            Some("https://opengeo.ncep.noaa.gov/geoserver/conus/conus_cref_qcd/ows?SERVICE=WMS&")
        );
        let layer = caps.layer("conus_cref_qcd").expect("the layer");
        // EPSG:3857 is the parent's; the layer inherits it.
        assert!(layer.supports(WmsCrs::Epsg3857));
        assert_eq!(layer.bounds, Some([-130.0, 20.000000782311, -60.0000015646219, 55.0]));
        assert!(layer.queryable && !layer.opaque);
        let time = layer.time.as_ref().expect("a time dimension");
        assert_eq!(time.default.as_deref(), Some("2026-10-04T19:34:41Z"));
        assert_eq!(time.values.len(), 3);
        // The newest, whatever the listed order.
        assert_eq!(time.latest(), Some("2026-10-04T19:34:41.000Z"));
        // The unnamed root is listed too, for its title and children.
        assert_eq!(caps.layers.len(), 2);
        assert!(caps.layers[0].name.is_none());
    }

    #[test]
    fn capabilities_1_1_1_parse_srs_latlon_box_extent_and_attribution() {
        let caps = parse_capabilities(OTHER_1_1_1).expect("parses");
        assert_eq!(caps.wms_version(), WmsVersion::V1_1_1);
        assert_eq!(caps.title, "Test radar");
        let layer = caps.layer("radar").expect("radar");
        assert_eq!(layer.crs, vec!["EPSG:4326", "EPSG:900913", "EPSG:3857"]);
        assert_eq!(layer.bounds, Some([-125.0, 24.0, -66.0, 50.0]));
        assert_eq!(layer.attribution.as_deref(), Some("NOAA/NWS"));
        let time = layer.time.as_ref().expect("time");
        assert!(time.values.is_empty(), "an interval is not a list");
        assert_eq!(time.extent, "2026-10-04T19:00:00Z/2026-10-04T19:30:00Z/PT5M");
        assert_eq!(time.latest(), Some("2026-10-04T19:30:00Z"));
        let source = WmsSource::from_capabilities("https://x.test/wms", &caps, "radar", "fallback").expect("a source");
        assert_eq!(source.config().version, WmsVersion::V1_1_1);
        assert_eq!(source.config().crs, WmsCrs::Epsg3857);
        assert_eq!(source.attribution(), Some("NOAA/NWS"));
    }

    #[test]
    fn a_service_exception_is_an_error_not_capabilities() {
        let xml = r#"<?xml version="1.0"?><ServiceExceptionReport version="1.3.0"><ServiceException code="LayerNotDefined">no such layer: nope</ServiceException></ServiceExceptionReport>"#;
        let err = parse_capabilities(xml).expect_err("an exception");
        assert!(format!("{err}").contains("no such layer: nope"), "{err}");
        assert!(parse_capabilities("not xml").is_err());
    }

    #[test]
    fn a_source_from_ncep_capabilities_asks_3857_and_skips_tiles_outside_conus() {
        let caps = parse_capabilities(NCEP_1_3_0).unwrap();
        let source = WmsSource::from_capabilities("https://unused.test", &caps, "conus_cref_qcd", "NOAA/NWS NCEP").unwrap();
        assert_eq!(source.config().crs, WmsCrs::Epsg3857);
        assert_eq!(source.attribution(), Some("NOAA/NWS NCEP"));
        assert!(source.get_map_url(TileId { z: 4, x: 3, y: 6 }).starts_with("https://opengeo.ncep.noaa.gov/"));
        // Europe at z4 is outside CONUS; Kansas is inside.
        assert!(source.outside(TileId { z: 4, x: 8, y: 5 }));
        assert!(!source.outside(crate::geo::tile_id_at(-98.0, 38.0, 4)));
    }

    /// A stub WMS: the capabilities, then a GetMap whose query the server
    /// matches, a 200 exception report, and a tile outside the extent
    /// that is never requested.
    #[tokio::test]
    async fn a_stub_server_answers_capabilities_and_get_map() {
        let server = MockServer::start().await;
        let caps_xml = NCEP_1_3_0.replace(
            "https://opengeo.ncep.noaa.gov/geoserver/conus/conus_cref_qcd/ows?SERVICE=WMS&amp;",
            &format!("{}/ows?SERVICE=WMS&amp;", server.uri()),
        );
        Mock::given(method("GET"))
            .and(path("/ows"))
            .and(query_param("REQUEST", "GetCapabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_string(caps_xml).insert_header("content-type", "text/xml"))
            .mount(&server)
            .await;
        let png = {
            let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([0, 200, 0, 128]));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let kansas = crate::geo::tile_id_at(-98.0, 38.0, 4);
        let want_bbox = {
            let b = tile_bbox_3857(kansas);
            format!("{},{},{},{}", b[0], b[1], b[2], b[3])
        };
        Mock::given(method("GET"))
            .and(path("/ows"))
            .and(query_param("REQUEST", "GetMap"))
            .and(query_param("CRS", "EPSG:3857"))
            .and(query_param("LAYERS", "conus_cref_qcd"))
            .and(query_param("TIME", "2026-10-04T19:34:41.000Z"))
            .and(query_param("BBOX", want_bbox.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png).insert_header("content-type", "image/png"))
            .expect(1)
            .mount(&server)
            .await;

        let caps = fetch_capabilities(&format!("{}/ows", server.uri()), WmsVersion::V1_3_0).await.expect("capabilities");
        let layer = caps.layer("conus_cref_qcd").unwrap();
        let latest = layer.time.as_ref().and_then(|t| t.latest()).map(str::to_string);
        let source = WmsSource::from_capabilities(&server.uri(), &caps, "conus_cref_qcd", "NOAA/NWS NCEP")
            .unwrap()
            .with_time(latest);
        let pixels = source.fetch_rgba(kansas, 256).await.expect("fetched").expect("a tile");
        assert_eq!(pixels.dimensions(), (256, 256));
        assert_eq!(pixels.get_pixel(10, 10).0, [0, 200, 0, 128]);
        // Outside CONUS: absent, and no request (`expect(1)` above).
        assert_eq!(source.fetch(TileId { z: 4, x: 8, y: 5 }).await.unwrap(), None);

        // Any other GetMap: the server's 200 exception report is an error.
        let mut other = source.config().clone();
        other.time = Some("1999-01-01T00:00:00Z".into());
        let server_err = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<ServiceExceptionReport><ServiceException>Invalid TIME</ServiceException></ServiceExceptionReport>"#)
                    .insert_header("content-type", "application/vnd.ogc.se_xml"),
            )
            .mount(&server_err)
            .await;
        other.url = format!("{}/ows", server_err.uri());
        let err = WmsSource::new(other, "").fetch(kansas).await.expect_err("an exception");
        assert!(format!("{err}").contains("Invalid TIME"), "{err}");
    }
}
