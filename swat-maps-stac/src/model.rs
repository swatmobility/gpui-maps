//! One STAC document (catalog, collection or item), read leniently.
//!
//! [`Node::parse`] never refuses a document that is JSON and names an id:
//! every deviation from STAC 1.1 or the Portolan profile becomes a
//! [`Warning`] on the node and the reader carries on with its best guess.
//! portolan-nl alone breaks several rules (an `application/x-parquet` media
//! type, collections that hold catalogs, a style whose source is not named
//! `data`), and a reader that refused those would refuse the catalog.
//!
//! Every `href` is resolved against the document's own URL at parse time
//! ([`crate::url::join`]), so callers only ever see absolute URLs (or
//! absolute paths, for a catalog read from disk).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::licence::Licence;
use crate::url::join;

/// What a document is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Catalog,
    Collection,
    Item,
}

/// A spec deviation the reader worked around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    /// A stable code (`media_type`, `missing_roles`, ...), for tests and
    /// for filing the issue upstream.
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Link {
    pub rel: String,
    /// Absolute.
    pub href: String,
    pub media_type: Option<String>,
    pub title: Option<String>,
    /// `pmtiles:layers` on a web-map-links `pmtiles` link.
    pub pmtiles_layers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Asset {
    /// Its key in `assets`.
    pub key: String,
    /// Absolute.
    pub href: String,
    pub media_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub roles: Vec<String>,
}

impl Asset {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.eq_ignore_ascii_case(role))
    }

    /// Its format, from the media type (then the file extension).
    pub fn format(&self) -> Format {
        Format::of(self.media_type.as_deref(), &self.href)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Provider {
    pub name: String,
    pub roles: Vec<String>,
    pub url: Option<String>,
}

impl Provider {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.eq_ignore_ascii_case(role))
    }
}

/// A `table:columns` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Column {
    pub name: String,
    pub data_type: Option<String>,
    pub description: Option<String>,
}

/// A data format a host may or may not read.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    PmTiles,
    GeoParquet,
    Cog,
    /// A MapLibre / Mapbox GL style.
    Style,
    Image,
    Markdown,
    Json,
    Other(String),
}

impl Format {
    /// From a media type, falling back on the extension of `href`.
    pub fn of(media_type: Option<&str>, href: &str) -> Format {
        let m = media_type.unwrap_or("").to_ascii_lowercase();
        if m.contains("pmtiles") {
            return Format::PmTiles;
        }
        if m.contains("parquet") {
            return Format::GeoParquet;
        }
        if m.contains("geotiff") || m.starts_with("image/tiff") {
            return Format::Cog;
        }
        if m.contains("style+json") {
            return Format::Style;
        }
        if m.starts_with("image/") {
            return Format::Image;
        }
        if m.contains("markdown") {
            return Format::Markdown;
        }
        let path = href.split(['?', '#']).next().unwrap_or(href).to_ascii_lowercase();
        let ext = path.rsplit('.').next().unwrap_or("");
        match ext {
            "pmtiles" => Format::PmTiles,
            "parquet" | "geoparquet" => Format::GeoParquet,
            "tif" | "tiff" => Format::Cog,
            "png" | "jpg" | "jpeg" | "webp" => Format::Image,
            "md" => Format::Markdown,
            "json" if m.is_empty() || m.contains("json") => Format::Json,
            _ if !m.is_empty() => Format::Other(m),
            _ => Format::Other(ext.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Format::PmTiles => "PMTiles",
            Format::GeoParquet => "GeoParquet",
            Format::Cog => "COG",
            Format::Style => "MapLibre style",
            Format::Image => "image",
            Format::Markdown => "Markdown",
            Format::Json => "JSON",
            Format::Other(s) => s,
        }
    }
}

/// A catalog, collection or item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    pub kind: NodeKind,
    pub id: String,
    /// The URL (or path) this document was read from.
    pub url: String,
    pub title: Option<String>,
    pub description: String,
    pub stac_version: Option<String>,
    pub extensions: Vec<String>,
    /// The Portolan profile version from its schema URI (`0.1.0`).
    pub portolan_version: Option<String>,
    /// The `license` field and the `rel: license` link. Items inherit
    /// their collection's ([`crate::Catalog`] fills it in).
    pub licence: Licence,
    pub providers: Vec<Provider>,
    pub links: Vec<Link>,
    /// By key (serde_json's map order).
    pub assets: Vec<Asset>,
    /// `item_assets` of a collection: what each item carries.
    pub item_assets: Vec<Asset>,
    /// WGS84 `[west, south, east, north]` (the first `extent.spatial.bbox`,
    /// or an item's `bbox`).
    pub bbox: Option<[f64; 4]>,
    pub updated: Option<String>,
    /// The data's CRS as a code (`EPSG:28992`, `OGC:CRS84`), from
    /// `proj:code` or `proj:epsg`, on the node or its properties.
    pub crs: Option<String>,
    pub row_count: Option<u64>,
    pub columns: Vec<Column>,
    pub geometry_types: Vec<String>,
    /// `pmtiles:min_zoom` / `pmtiles:max_zoom`.
    pub pmtiles_zoom: Option<(u8, u8)>,
    /// `pmtiles:center` `[lon, lat, zoom]`.
    pub pmtiles_center: Option<[f64; 3]>,
    pub warnings: Vec<Warning>,
}

const PORTOLAN_SCHEMA: &str = "schemas.portolan-sdi.org/portolan/v";

impl Node {
    /// Reads the document at `url` from its bytes. Errs only when the bytes
    /// are not a JSON object or carry no id at all.
    pub fn parse(url: &str, bytes: &[u8]) -> Result<Node, String> {
        let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("{url}: not JSON ({e})"))?;
        Node::from_value(url, &v)
    }

    pub fn from_value(url: &str, v: &Value) -> Result<Node, String> {
        let o = v.as_object().ok_or_else(|| format!("{url}: not a JSON object"))?;
        let mut warnings = Vec::new();
        let mut warn = |code: &'static str, message: String| warnings.push(Warning { code, message });

        let kind = match o.get("type").and_then(Value::as_str) {
            Some("Catalog") => NodeKind::Catalog,
            Some("Collection") => NodeKind::Collection,
            Some("Feature") => NodeKind::Item,
            other => {
                // Guess from the shape: an extent is a collection's, a
                // geometry an item's.
                let guess = if o.contains_key("extent") || o.contains_key("license") {
                    NodeKind::Collection
                } else if o.contains_key("geometry") || o.contains_key("properties") {
                    NodeKind::Item
                } else {
                    NodeKind::Catalog
                };
                warn("type", format!("\"type\" is {other:?}; read as {guess:?}"));
                guess
            }
        };
        let id = match o.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => return Err(format!("{url}: no \"id\"")),
        };
        let props = o.get("properties").and_then(Value::as_object);
        // An item keeps most fields in `properties`; read either place.
        let field = |k: &str| o.get(k).or_else(|| props.and_then(|p| p.get(k)));

        let stac_version = o.get("stac_version").and_then(Value::as_str).map(str::to_string);
        match stac_version.as_deref() {
            None => warn("stac_version", "no \"stac_version\"".into()),
            Some(v) if !v.starts_with("1.") => warn("stac_version", format!("stac_version {v}: read as 1.1")),
            _ => {}
        }
        let extensions: Vec<String> = o.get("stac_extensions").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let portolan_version = extensions.iter().find_map(|e| {
            let i = e.find(PORTOLAN_SCHEMA)? + PORTOLAN_SCHEMA.len();
            Some(e[i..].split('/').next().unwrap_or("").to_string())
        });

        let mut links = Vec::new();
        for (i, l) in o.get("links").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let Some(lo) = l.as_object() else {
                warn("link", format!("links[{i}] is not an object"));
                continue;
            };
            let (Some(rel), Some(href)) = (lo.get("rel").and_then(Value::as_str), lo.get("href").and_then(Value::as_str)) else {
                warn("link", format!("links[{i}] has no rel or href"));
                continue;
            };
            if href.starts_with("s3://") && rel != "via" {
                warn("s3_href", format!("link {rel} is s3:// (Portolan wants https)"));
            }
            links.push(Link {
                rel: rel.to_string(),
                href: join(url, href),
                media_type: str_of(lo, "type"),
                title: str_of(lo, "title"),
                pmtiles_layers: strings(lo.get("pmtiles:layers")),
            });
        }
        if links.is_empty() {
            warn("links", "no links".into());
        }

        let assets = read_assets(url, o.get("assets"), "assets", &mut warn);
        let item_assets = read_assets(url, o.get("item_assets"), "item_assets", &mut warn);
        for a in &assets {
            if a.media_type.as_deref() == Some("application/x-parquet") {
                warn("media_type", format!("asset {} is application/x-parquet (Portolan: application/vnd.apache.parquet)", a.key));
            }
        }

        let licence_id = field("license").and_then(Value::as_str);
        let terms = links.iter().find(|l| l.rel == "license").map(|l| l.href.clone());
        if kind == NodeKind::Collection && licence_id.is_none() {
            warn("license", "a collection with no \"license\"".into());
        }
        if licence_id == Some("proprietary") {
            warn("license", "\"proprietary\" is not allowed by the spec; read as other".into());
        }
        let licence = Licence::new(licence_id, terms);

        let providers = field("providers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let po = p.as_object()?;
                        Some(Provider { name: str_of(po, "name")?, roles: strings(po.get("roles")), url: str_of(po, "url") })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let bbox = o
            .get("extent")
            .and_then(|e| e.get("spatial"))
            .and_then(|s| s.get("bbox"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(bbox_of)
            .or_else(|| o.get("bbox").and_then(bbox_of));
        if kind != NodeKind::Catalog && bbox.is_none() {
            warn("bbox", "no spatial extent".into());
        }

        let crs = field("proj:code")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| field("proj:epsg").and_then(epsg_code))
            .or_else(|| {
                // A collection of items states it in `summaries`.
                let s = o.get("summaries")?;
                s.get("proj:code").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string).or_else(|| s.get("proj:epsg").and_then(Value::as_array).and_then(|a| a.first()).and_then(epsg_code))
            });
        let columns = field("table:columns")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        let co = c.as_object()?;
                        Some(Column { name: str_of(co, "name")?, data_type: str_of(co, "type"), description: str_of(co, "description") })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut geometry_types = strings(field("geoparquet:geometry_type").map(|v| if v.is_string() { Value::Array(vec![v.clone()]) } else { v.clone() }).as_ref());
        if geometry_types.is_empty() {
            geometry_types = strings(o.get("summaries").and_then(|s| s.get("vector:geometry_types")));
        }
        let zoom = |k: &str| field(k).and_then(Value::as_u64).map(|z| z.min(30) as u8);
        let pmtiles_zoom = match (zoom("pmtiles:min_zoom"), zoom("pmtiles:max_zoom")) {
            (Some(a), Some(b)) => Some((a, b)),
            (None, Some(b)) => Some((0, b)),
            _ => None,
        };
        let pmtiles_center = field("pmtiles:center").and_then(Value::as_array).and_then(|a| {
            let n: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
            (n.len() >= 2).then(|| [n[0], n[1], n.get(2).copied().unwrap_or(10.0)])
        });

        let mut node = Node {
            kind,
            id,
            url: url.to_string(),
            title: field("title").and_then(Value::as_str).map(str::to_string),
            description: field("description").and_then(Value::as_str).unwrap_or("").to_string(),
            stac_version,
            extensions,
            portolan_version,
            licence,
            providers,
            links,
            assets,
            item_assets,
            bbox,
            updated: field("updated").and_then(Value::as_str).map(str::to_string),
            crs,
            row_count: field("table:row_count").or_else(|| field("geoparquet:feature_count")).and_then(Value::as_u64),
            columns,
            geometry_types,
            pmtiles_zoom,
            pmtiles_center,
            warnings,
        };
        node.check_profile();
        Ok(node)
    }

    /// The Portolan rules a reader can see without fetching anything else.
    fn check_profile(&mut self) {
        let mut w = Vec::new();
        if self.kind != NodeKind::Item {
            if self.link("agents").is_none() {
                w.push(Warning { code: "agents", message: "no rel: agents link (AGENTS.md)".into() });
            }
            if self.link("describedby").is_none() && !self.assets.iter().any(|a| a.format() == Format::Markdown) {
                w.push(Warning { code: "readme", message: "no rel: describedby link (README.md)".into() });
            }
        }
        if self.kind == NodeKind::Collection && self.links.iter().any(|l| l.rel == "child") {
            w.push(Warning { code: "nesting", message: "a collection with child links (Portolan: collections are leaves)".into() });
        }
        for a in &self.assets {
            if a.roles.is_empty() {
                w.push(Warning { code: "missing_roles", message: format!("asset {} has no roles", a.key) });
            }
            if a.media_type.is_none() {
                w.push(Warning { code: "missing_type", message: format!("asset {} has no type", a.key) });
            }
        }
        let styles: Vec<&Asset> = self.styles();
        if !styles.is_empty() && !styles.iter().any(|a| a.has_role("default")) {
            w.push(Warning { code: "default_style", message: "no style asset has the default role; the first is used".into() });
        }
        if self.pmtiles_url().is_some() && styles.is_empty() {
            w.push(Warning { code: "style", message: "PMTiles with no MapLibre style".into() });
        }
        self.warnings.extend(w);
    }

    pub fn display_title(&self) -> &str {
        self.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&self.id)
    }

    /// The first link with `rel`.
    pub fn link(&self, rel: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.rel == rel)
    }

    /// Every link with `rel`, in order.
    pub fn links_with(&self, rel: &str) -> Vec<&Link> {
        self.links.iter().filter(|l| l.rel == rel).collect()
    }

    /// `rel: child` links: sub-catalogs and collections (and, leniently,
    /// what a collection holds as children).
    pub fn child_links(&self) -> Vec<&Link> {
        self.links_with("child")
    }

    pub fn item_links(&self) -> Vec<&Link> {
        self.links_with("item")
    }

    pub fn agents_url(&self) -> Option<&str> {
        self.link("agents").map(|l| l.href.as_str())
    }

    pub fn readme_url(&self) -> Option<&str> {
        self.link("describedby").map(|l| l.href.as_str()).or_else(|| self.assets.iter().find(|a| a.format() == Format::Markdown).map(|a| a.href.as_str()))
    }

    /// The first thumbnail: an asset with the `thumbnail` role, else a
    /// `preview` link.
    pub fn thumbnail_url(&self) -> Option<&str> {
        self.assets.iter().find(|a| a.has_role("thumbnail")).map(|a| a.href.as_str()).or_else(|| self.link("preview").map(|l| l.href.as_str()))
    }

    /// The style assets, the `default` one first.
    pub fn styles(&self) -> Vec<&Asset> {
        let mut s: Vec<&Asset> = self.assets.iter().filter(|a| a.has_role("style") || a.format() == Format::Style).collect();
        s.sort_by_key(|a| !a.has_role("default"));
        s
    }

    pub fn default_style(&self) -> Option<&Asset> {
        self.styles().into_iter().next()
    }

    /// The PMTiles derivative: the web-map-links `pmtiles` link, else a
    /// PMTiles asset (`visual` first).
    pub fn pmtiles_url(&self) -> Option<&str> {
        if let Some(l) = self.link("pmtiles") {
            return Some(&l.href);
        }
        let mut pm: Vec<&Asset> = self.assets.iter().filter(|a| a.format() == Format::PmTiles).collect();
        pm.sort_by_key(|a| !a.has_role("visual"));
        pm.first().map(|a| a.href.as_str())
    }

    /// The `pmtiles:layers` of the PMTiles link.
    pub fn pmtiles_layers(&self) -> Vec<String> {
        self.link("pmtiles").map(|l| l.pmtiles_layers.clone()).unwrap_or_default()
    }

    /// The data formats this node (and its items) carry, deduplicated.
    pub fn formats(&self) -> Vec<Format> {
        let mut f: Vec<Format> = self
            .assets
            .iter()
            .chain(&self.item_assets)
            .filter(|a| !a.has_role("collection-mirror"))
            .filter(|a| a.has_role("data") || a.has_role("visual") || matches!(a.format(), Format::PmTiles | Format::GeoParquet | Format::Cog))
            .map(Asset::format)
            .filter(|f| matches!(f, Format::PmTiles | Format::GeoParquet | Format::Cog))
            .collect();
        if self.link("pmtiles").is_some() {
            f.push(Format::PmTiles);
        }
        f.sort();
        f.dedup();
        f
    }

    /// The `producer` (else `licensor`) providers' names.
    pub fn producers(&self) -> Vec<&str> {
        let p: Vec<&str> = self.providers.iter().filter(|p| p.has_role("producer")).map(|p| p.name.as_str()).collect();
        if p.is_empty() {
            self.providers.iter().filter(|p| p.has_role("licensor")).map(|p| p.name.as_str()).collect()
        } else {
            p
        }
    }

    pub fn host(&self) -> Option<&str> {
        self.providers.iter().find(|p| p.has_role("host")).map(|p| p.name.as_str())
    }

    /// A mirror: the host is not the producer (the spec derives it so).
    pub fn is_mirror(&self) -> bool {
        match self.host() {
            Some(h) => !self.producers().contains(&h),
            None => false,
        }
    }

    /// "Producer · CC0-1.0 · via Host", the map's credit line.
    pub fn credit_line(&self) -> String {
        let producers = self.producers();
        let mut parts = Vec::new();
        if !producers.is_empty() {
            parts.push(producers.join(", "));
        }
        parts.push(self.licence.label());
        if let Some(h) = self.host().filter(|h| !producers.contains(h)) {
            parts.push(format!("via {h}"));
        }
        parts.join(" · ")
    }

    /// The description's first paragraph, trimmed to `max` characters at a
    /// word boundary (for a card).
    pub fn short_description(&self, max: usize) -> String {
        short(&self.description, max)
    }
}

/// A text's first paragraph, cut at a word boundary under `max` chars.
pub fn short(text: &str, max: usize) -> String {
    let first = text.split("\n\n").next().unwrap_or("").trim().replace('\n', " ");
    if first.chars().count() <= max {
        return first;
    }
    let mut out = String::new();
    for word in first.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > max.saturating_sub(1) {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.push('…');
    out
}

fn epsg_code(v: &Value) -> Option<String> {
    v.as_i64().map(|n| format!("EPSG:{n}")).or_else(|| v.as_str().map(|s| if s.contains(':') { s.to_string() } else { format!("EPSG:{s}") }))
}

fn str_of(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_string)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn bbox_of(v: &Value) -> Option<[f64; 4]> {
    let n: Vec<f64> = v.as_array()?.iter().filter_map(Value::as_f64).collect();
    match n.len() {
        4 => Some([n[0], n[1], n[2], n[3]]),
        // 3D: [w, s, zmin, e, n, zmax].
        6 => Some([n[0], n[1], n[3], n[4]]),
        _ => None,
    }
}

fn read_assets(url: &str, v: Option<&Value>, what: &str, warn: &mut impl FnMut(&'static str, String)) -> Vec<Asset> {
    let Some(map) = v.and_then(Value::as_object) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, a) in map {
        let Some(ao) = a.as_object() else {
            warn("asset", format!("{what}.{key} is not an object"));
            continue;
        };
        // item_assets carry no href.
        let href = ao.get("href").and_then(Value::as_str).map(|h| join(url, h)).unwrap_or_default();
        if href.is_empty() && what == "assets" {
            warn("asset", format!("asset {key} has no href"));
            continue;
        }
        if href.starts_with("s3://") {
            warn("s3_href", format!("asset {key} is s3:// (Portolan wants https)"));
        }
        out.push(Asset {
            key: key.clone(),
            href,
            media_type: str_of(ao, "type"),
            title: str_of(ao, "title"),
            description: str_of(ao, "description"),
            roles: strings(ao.get("roles")),
        });
    }
    out
}
