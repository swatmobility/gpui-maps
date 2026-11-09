//! The contract between a demo panel and whatever hosts it: its own window
//! (a standalone example) or the gallery. See
//! `docs/superpowers/specs/2026-09-27-gallery-app-design.md` section 4.
//!
//! Mounted at the `swat-maps-kit` crate root as `kit`; panel modules name
//! it `crate::kit`, and a binary that `use`s it at its own root keeps that
//! path.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui::{AnyView, App, WeakEntity};
use gpui_maps::map_view::MapView;
use gpui_maps::render::{allocation_stats, paint_census, AllocationStats};
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

#[derive(Clone, Debug, PartialEq)]
pub enum Host {
    /// Its own window: today's behaviour, byte for byte.
    Standalone { title: String },
    /// Inside the gallery: no window-title writes, focus once, chrome above map.
    Gallery,
}

impl Host {
    pub fn is_gallery(&self) -> bool {
        matches!(self, Host::Gallery)
    }
    pub fn title(&self) -> Option<&str> {
        match self {
            Host::Standalone { title } => Some(title),
            Host::Gallery => None,
        }
    }
}

#[derive(Clone)]
pub struct PanelEnv {
    pub host: Host,
    pub runtime: tokio::runtime::Handle,
    /// Read once at launch. Never printed, logged or rendered.
    pub mapbox_token: Option<String>,
    pub viewport: (f64, f64),
    /// Raised by the gallery's `PanelSlot` when this panel is switched away
    /// from. A panel that loads in stages off the UI thread checks it
    /// between stages and stops; a standalone example never raises it.
    pub cancel: Arc<AtomicBool>,
    /// The ClickHouse server a page that reads one uses, as the host
    /// resolved it: Studio, from the saved connection the page names
    /// ([`ClickHouseEndpoint::connection`]) on its Connections page. `None`
    /// when the host has no such connection: the page's
    /// [`Requirement::ClickHouse`] is then `Missing` and says where to add
    /// it. A page never makes up an address of its own.
    pub clickhouse: Option<ClickHouseEndpoint>,
}

/// A ClickHouse server resolved by the host for a page: the saved
/// connection's name, its address (no user info) and the auth headers
/// its saved user and password become (`X-ClickHouse-User`,
/// `X-ClickHouse-Key`). `Debug` never prints the headers' values.
#[derive(Clone, PartialEq, Eq)]
pub struct ClickHouseEndpoint {
    pub connection: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for ClickHouseEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseEndpoint")
            .field("connection", &self.connection)
            .field("url", &gpui_maps::sprite::redact_url_credentials(&self.url))
            .field("headers", &self.headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>())
            .finish()
    }
}

/// Where the Mapbox token may live when the environment does not carry it,
/// relative to `$HOME`. The same directory and `KEY=VALUE` shape as
/// `gpui_maps::nav::host::CONFIG_FILE_RELATIVE` (`swat-routing.env`), so an
/// app launched from Finder -- which inherits no shell environment -- still
/// finds it.
pub const MAPBOX_ENV_FILE_RELATIVE: &str = ".config/gpui-map/mapbox.env";

/// The variable the token is read from, in the environment and in the file.
pub const MAPBOX_TOKEN_VAR: &str = "MAPBOX_ACCESS_TOKEN";

/// The opt-in that turns Mapbox back on. Mapbox is OFF by default (2026-10:
/// a usage spike on the key): without `GPUI_MAP_ALLOW_MAPBOX=1` every shell
/// treats the token as absent -- wherever it lives -- and draws its keyless
/// basemap (OpenFreeMap, or a local PMTiles where the page has one), so no
/// Mapbox tile, style, sprite or glyph is requested. The one switch: every
/// token read in the examples, demos, web and iOS shells goes through it.
pub const MAPBOX_ALLOW_VAR: &str = "GPUI_MAP_ALLOW_MAPBOX";

/// Whether the opt-in's value turns Mapbox on: exactly `1` (trimmed).
pub fn mapbox_allowed_value(value: Option<&str>) -> bool {
    value.map(str::trim) == Some("1")
}

/// Whether this process may use Mapbox: [`MAPBOX_ALLOW_VAR`] is `1`.
pub fn mapbox_allowed() -> bool {
    mapbox_allowed_value(std::env::var(MAPBOX_ALLOW_VAR).ok().as_deref())
}

/// The environment's token, gated: `None` unless [`mapbox_allowed`]. For
/// the call sites that read the environment only (no `mapbox.env`).
pub fn env_mapbox_token() -> Option<String> {
    if !mapbox_allowed() {
        return None;
    }
    std::env::var(MAPBOX_TOKEN_VAR).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Takes the environment's token out of this process's environment and
/// hands it back, for Studio to hold in memory: its "Use Mapbox tiles"
/// switch can then turn Mapbox on later in the session without the token
/// ever sitting in the environment while it is off -- so nothing that
/// reads the variable directly (the library's story loader, a child
/// process) can reach Mapbox. With the
/// [`MAPBOX_ALLOW_VAR`] override the variable stays (the session is on).
/// Call it first thing in `main`, before any thread exists. The value is
/// never printed.
pub fn take_env_mapbox_token() -> Option<String> {
    let token = std::env::var(MAPBOX_TOKEN_VAR).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if !mapbox_allowed() {
        std::env::remove_var(MAPBOX_TOKEN_VAR);
    }
    token
}

impl PanelEnv {
    pub fn from_process(host: Host, runtime: tokio::runtime::Handle, viewport: (f64, f64)) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mapbox_token = match mapbox_token(std::env::var(MAPBOX_TOKEN_VAR).ok(), home.as_deref()) {
            Ok(token) => token,
            Err(why) => {
                // `why` names the file and the reason, never the value.
                eprintln!("gpui-maps: {why}");
                None
            }
        };
        Self { host, runtime, mapbox_token, viewport, cancel: Arc::new(AtomicBool::new(false)), clickhouse: None }
    }
}

/// The Mapbox token: the environment's `MAPBOX_ACCESS_TOKEN` first, then
/// `$HOME/.config/gpui-map/mapbox.env`. `Ok(None)` when neither has one.
///
/// `Err` when the file exists but is readable by group or others: a token
/// in a world-readable file is refused rather than used, and the error says
/// why and how to fix it WITHOUT the value -- nothing in this function puts
/// the token in a string that could be printed.
///
/// Gated: `Ok(None)` without the [`MAPBOX_ALLOW_VAR`] opt-in, whatever the
/// environment or the file holds.
pub fn mapbox_token(env_value: Option<String>, home: Option<&Path>) -> Result<Option<String>, String> {
    mapbox_token_gated(mapbox_allowed(), env_value, home)
}

/// [`mapbox_token`] with the opt-in passed in, so a test never depends on
/// the ambient environment.
pub fn mapbox_token_gated(allowed: bool, env_value: Option<String>, home: Option<&Path>) -> Result<Option<String>, String> {
    if !allowed {
        return Ok(None);
    }
    read_mapbox_token(env_value, home)
}

/// [`mapbox_token_gated`] without the gate: where a token would come from,
/// read but not used. Studio's switch calls it to know whether it CAN turn
/// Mapbox on; nothing it returns is handed to a map while the switch is
/// off.
pub fn read_mapbox_token(env_value: Option<String>, home: Option<&Path>) -> Result<Option<String>, String> {
    if let Some(token) = env_value.filter(|t| !t.trim().is_empty()) {
        return Ok(Some(token.trim().to_string()));
    }
    let Some(home) = home else { return Ok(None) };
    let path = home.join(MAPBOX_ENV_FILE_RELATIVE);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // EACCES and the like: the file is there and could not be checked,
        // which is worth saying rather than silently meaning "no token".
        Err(e) => return Err(format!("could not check ~/{MAPBOX_ENV_FILE_RELATIVE}: {}", e.kind())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "ignoring ~/{MAPBOX_ENV_FILE_RELATIVE}: it is readable by group or others (mode {mode:03o}); \
                 run `chmod 600 ~/{MAPBOX_ENV_FILE_RELATIVE}` to use it"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read ~/{MAPBOX_ENV_FILE_RELATIVE}: {}", e.kind()))?;
    // The LAST assignment wins, as in a shell and as
    // `RoutingConfig::resolve` reads `swat-routing.env`.
    Ok(gpui_maps::nav::config::parse_env_file(&text)
        .into_iter()
        .rev()
        .find(|(name, value)| name == MAPBOX_TOKEN_VAR && !value.trim().is_empty())
        .map(|(_, value)| value.trim().to_string()))
}

/// Something a panel needs before it can build: a token, a file on disk, an
/// environment variable naming a path, a live ClickHouse server, or plain
/// network access. [`check`] evaluates one against the real environment (or
/// a fake, in a test) and never puts a secret in the [`Status`] it returns.
#[derive(Clone, Debug)]
pub enum Requirement {
    MapboxToken { fallback: Option<&'static str> },
    /// `then`: a second command to run after `fetch` that the panel prefers
    /// (the census: fetch the shapefiles, then build the faster pack).
    File {
        path: PathBuf,
        what: &'static str,
        fetch: &'static str,
        size: &'static str,
        fallback: Option<&'static str>,
        then: Option<&'static str>,
    },
    EnvPath { var: &'static str, how: &'static str },
    /// A ClickHouse server, through the host's saved connection named
    /// `connection`: `url` is its address when the host has one by that
    /// name, `None` when it does not (then `Missing`, pointing at the
    /// Connections page). `load` is the script that loads the page's
    /// tables, for the agent and the docs (the card shows no script).
    ClickHouse { connection: String, url: Option<String>, load: &'static str },
    Network,
}

/// `path` for a person to read: the home directory as `~`, so a card or
/// banner never prints `/Users/<name>/...` in full.
pub fn tilde(path: &Path) -> String {
    tilde_in(path, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

pub fn tilde_in(path: &Path, home: Option<&Path>) -> String {
    match home.filter(|h| !h.as_os_str().is_empty()).and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What [`check`] found. `Missing` and `Fallback` both carry a human-facing
/// `why` -- the gallery's missing-data card and fallback banner print it
/// verbatim -- and neither ever contains a token's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Met,
    Missing { why: String },
    Fallback { why: String },
}

/// Evaluates one [`Requirement`] against `env` (an environment-variable
/// lookup) and `exists` (a file-existence check), both injected so the
/// gallery's tests never touch the real filesystem or environment.
///
/// `ClickHouse` and `Network` are probed asynchronously by the gallery
/// itself (a later task): pure and synchronous here, they are always
/// `Met`.
pub fn check(req: &Requirement, env: &dyn Fn(&str) -> Option<String>, exists: &dyn Fn(&Path) -> bool) -> Status {
    match req {
        Requirement::MapboxToken { fallback } => {
            let token = env(MAPBOX_TOKEN_VAR).filter(|t| !t.trim().is_empty());
            let allowed = mapbox_allowed_value(env(MAPBOX_ALLOW_VAR).as_deref());
            if !allowed {
                return match fallback {
                    Some(f) => Status::Fallback { why: format!("{f} basemap: Mapbox is turned off (set {MAPBOX_ALLOW_VAR}=1 to use it)") },
                    None => Status::Missing {
                        why: format!("needs Mapbox, which is turned off: turn on Use Mapbox tiles in Settings (or set {MAPBOX_ALLOW_VAR}=1)"),
                    },
                };
            }
            match (token, fallback) {
                (Some(_), _) => Status::Met,
                (None, Some(f)) => {
                    Status::Fallback { why: format!("{f} basemap: set {MAPBOX_TOKEN_VAR} for the Mapbox one") }
                }
                (None, None) => Status::Missing {
                    why: format!(
                        "needs {MAPBOX_TOKEN_VAR} (a Mapbox public 'pk.' token): export it, then relaunch the gallery"
                    ),
                },
            }
        }
        Requirement::File { path, what, fallback, .. } if !exists(path) => match fallback {
            Some(f) => Status::Fallback { why: format!("{what} not found at {}; drawing the {f}", tilde(path)) },
            None => Status::Missing { why: format!("{what} not found at {}", tilde(path)) },
        },
        Requirement::File { .. } => Status::Met,
        Requirement::EnvPath { var, .. } => match env(var) {
            Some(p) if exists(Path::new(&p)) => Status::Met,
            Some(p) => Status::Missing { why: format!("{var} points at {}, which is not a readable file", tilde(Path::new(&p))) },
            None => Status::Missing { why: format!("{var} is not set") },
        },
        Requirement::ClickHouse { connection, url: None, .. } => Status::Missing { why: no_clickhouse_connection(connection) },
        // Probed asynchronously by the gallery (`probe_clickhouse`, which
        // `Gallery::open` awaits); Met until told otherwise.
        Requirement::ClickHouse { .. } | Requirement::Network => Status::Met,
    }
}

/// A dataset or basemap's licence, as the About pane's credits section
/// links it. `Unverified` and `PublicDomain` have no url to link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Licence {
    CcBy4,
    /// Attribution, share-alike: OpenCelliD's cell towers (and anything
    /// derived from them that is shared, the ClickHouse maps' recorded
    /// pack included).
    CcBySa4,
    /// Attribution, non-commercial, share-alike: EOX's Sentinel-2 cloudless
    /// layers from 2018 on (only the 2016 layer is CC BY 4.0). Nothing
    /// shipped here uses one; kept so a test can name what is refused.
    #[allow(dead_code)]
    CcByNcSa4,
    Odbl,
    PublicDomain,
    MapboxTos,
    Bsd2,
    Bsd3,
    /// Apache License 2.0: a code credit (the H3 grid library).
    Apache2,
    /// MIT: a code credit (the sgp4 propagator).
    Mit,
    /// Singapore Open Data Licence v1.0 (data.gov.sg): reuse with a
    /// conspicuous notice naming the dataset and linking the licence.
    SgOpenData,
    /// Community Data License Agreement -- Permissive 2.0: Overture Maps'
    /// places theme (attribution, no share-alike).
    CdlaPermissive2,
    /// UK Open Government Licence v3.0: ONS statistics, with the exact
    /// credit line ONS asks for.
    Ogl3,
    /// Open Data Commons Public Domain Dedication and License 1.0: DataSF's
    /// default (the Street Tree List carries it in its metadata).
    Pddl,
    /// County of Los Angeles Open Data / eGIS Terms of Use: a licence to
    /// copy, publish, distribute, adapt and exploit commercially; as-is; no
    /// suggestion of County endorsement; void on breach.
    LaCountyTou,
    /// The City of New York's Open Data terms of use: NYC TLC trip
    /// records (the ClickHouse maps' taxi page).
    NycOpenData,
    /// Open Government Licence - Canada 2.0: VIA Rail's GTFS feed.
    OglCanada,
    /// CC0 1.0 public-domain dedication (Higashiyamato's GTFS feed).
    Cc0,
    /// Abu Dhabi Government Open Data License v1.0 (data.abudhabi): a
    /// worldwide, royalty-free, perpetual, non-exclusive licence to copy,
    /// modify, publish, translate, adapt, distribute or otherwise use the
    /// information, commercially too, provided the source is acknowledged
    /// with the provider's attribution statement (or "Contains information
    /// licensed under this license") and, where possible, a link to the
    /// licence. Excludes personal information, third-party rights and the
    /// provider's names, crests and logos; no endorsement; "as is".
    /// Governed by the Abu Dhabi Government Open Data Policy (v1.0).
    AbuDhabiOpenData,
    /// A publisher's own open-data licence or terms, named and linked as
    /// the publisher states them (name, url): the Canadian municipal Open
    /// Government Licences and the transit agencies' GTFS terms of the
    /// bundled feeds (`examples/samples/data/gtfs/SOURCE.md` quotes each).
    Publisher(&'static str, &'static str),
    Unverified,
}

impl Licence {
    pub fn name(self) -> &'static str {
        match self {
            Licence::CcBy4 => "CC BY 4.0",
            Licence::CcBySa4 => "CC BY-SA 4.0",
            Licence::CcByNcSa4 => "CC BY-NC-SA 4.0",
            Licence::Odbl => "ODbL 1.0",
            Licence::PublicDomain => "Public domain",
            Licence::MapboxTos => "Mapbox Terms of Service",
            Licence::Bsd2 => "BSD-2-Clause",
            Licence::Bsd3 => "BSD-3-Clause",
            Licence::Apache2 => "Apache-2.0",
            Licence::Mit => "MIT",
            Licence::SgOpenData => "Singapore Open Data Licence v1.0",
            Licence::CdlaPermissive2 => "CDLA-Permissive-2.0",
            Licence::Ogl3 => "Open Government Licence v3.0",
            Licence::Pddl => "ODC PDDL 1.0",
            Licence::LaCountyTou => "County of Los Angeles Open Data Terms of Use",
            Licence::NycOpenData => "NYC Open Data Terms of Use",
            Licence::OglCanada => "Open Government Licence - Canada",
            Licence::Cc0 => "CC0 1.0",
            Licence::AbuDhabiOpenData => "Abu Dhabi Government Open Data License v1.0",
            Licence::Publisher(name, _) => name,
            Licence::Unverified => "Licence not independently verified",
        }
    }

    pub fn url(self) -> &'static str {
        match self {
            Licence::CcBy4 => "https://creativecommons.org/licenses/by/4.0/",
            Licence::CcBySa4 => "https://creativecommons.org/licenses/by-sa/4.0/",
            Licence::CcByNcSa4 => "https://creativecommons.org/licenses/by-nc-sa/4.0/",
            Licence::Odbl => "https://opendatacommons.org/licenses/odbl/1-0/",
            Licence::MapboxTos => "https://www.mapbox.com/legal/tos",
            Licence::Bsd2 => "https://opensource.org/license/bsd-2-clause",
            Licence::Bsd3 => "https://opensource.org/license/bsd-3-clause",
            Licence::Apache2 => "https://www.apache.org/licenses/LICENSE-2.0",
            Licence::Mit => "https://opensource.org/license/mit",
            Licence::SgOpenData => "https://data.gov.sg/open-data-licence",
            Licence::CdlaPermissive2 => "https://cdla.dev/permissive-2-0/",
            Licence::Ogl3 => "https://www.nationalarchives.gov.uk/doc/open-government-licence/version/3/",
            Licence::Pddl => "https://opendatacommons.org/licenses/pddl/1-0/",
            Licence::LaCountyTou => "https://egis-lacounty.hub.arcgis.com/pages/terms-of-use",
            Licence::NycOpenData => "https://opendata.cityofnewyork.us/overview/#termsofuse",
            Licence::OglCanada => "https://open.canada.ca/en/open-government-licence-canada",
            Licence::Cc0 => "https://creativecommons.org/publicdomain/zero/1.0/",
            Licence::AbuDhabiOpenData => "https://data.abudhabi/opendata/addata_open_license",
            Licence::Publisher(_, url) => url,
            Licence::PublicDomain | Licence::Unverified => "",
        }
    }
}

/// A dataset or basemap credit, typed rather than a blob of HTML: the
/// About pane composes `dataset — holder`, the licence (linked when it has
/// a url) and `changes`, and a test enforces that every CC BY credit names
/// all three of holder, licence link and changes made.
#[derive(Clone, Copy, Debug)]
pub struct Credit {
    pub what: &'static str,
    pub holder: &'static str,
    pub dataset: &'static str,
    pub licence: Licence,
    pub url: &'static str,
    pub changes: &'static str,
}

/// The attribution statement the Abu Dhabi Government Open Data License
/// v1.0 asks for when the provider gives none, or when several providers'
/// statements are impractical (the licence's own words, naming it).
pub const ABU_DHABI_ATTRIBUTION: &str = "Contains information licensed under the Abu Dhabi Government Open Data License.";

pub const CREDIT_OSM_OPENFREEMAP: Credit = Credit {
    what: "Basemap tiles",
    holder: "OpenFreeMap, OpenMapTiles, OpenStreetMap contributors",
    dataset: "OpenFreeMap planet (OpenMapTiles schema)",
    licence: Licence::Odbl,
    url: "https://www.openstreetmap.org/copyright",
    changes: "",
};
pub const CREDIT_MAPBOX: Credit = Credit {
    what: "Basemap tiles",
    holder: "Mapbox, OpenStreetMap contributors",
    dataset: "Mapbox Streets v8, Terrain v2, Bathymetry v2",
    licence: Licence::MapboxTos,
    url: "https://www.mapbox.com/about/maps",
    changes: "The Mapbox wordmark is also required by Mapbox's terms and is not drawn by this engine.",
};
/// The keyless satellite layer every `s` key draws: EOX's Sentinel-2
/// cloudless **2016** layer (`s2cloudless_3857`), the one EOX licenses CC BY
/// 4.0. Every later yearly layer (2018 on) is CC BY-NC-SA 4.0,
/// which a commercial product cannot ship (ruling 2026-09-29), so nothing
/// here draws one.
#[allow(dead_code)]
pub const S2CLOUDLESS_2016_URL: &str = "https://tiles.maps.eox.at/wmts/1.0.0/s2cloudless_3857/default/g/{z}/{y}/{x}.jpg";
/// EOX's required attribution, verbatim from cloudless.eox.at's licence
/// page (the site s2maps.eu now redirects to, 2026-09-29): "EOxCloudless
/// https://cloudless.eox.at by EOX IT Services GmbH (Contains modified
/// Copernicus Sentinel data <year>)", with the 2016 mosaic's years. The
/// link text carries the URL, so the tag-stripped credit reads verbatim.
#[allow(dead_code)]
pub const S2CLOUDLESS_2016_ATTRIBUTION: &str = r#"<a href="https://cloudless.eox.at" target="_blank">EOxCloudless https://cloudless.eox.at</a> by EOX IT Services GmbH (Contains modified Copernicus Sentinel data 2016 & 2017)"#;
pub const CREDIT_S2CLOUDLESS: Credit = Credit {
    what: "Satellite imagery (no token)",
    holder: "EOX IT Services GmbH",
    dataset: "EOxCloudless (Sentinel-2 cloudless 2016; contains modified Copernicus Sentinel data 2016 & 2017)",
    licence: Licence::CcBy4,
    url: "https://cloudless.eox.at",
    changes: "none; tiles displayed as served",
};
pub const CREDIT_NATURAL_EARTH: Credit = Credit {
    what: "Shaded relief (Liberty, z0-7)",
    holder: "Natural Earth",
    dataset: "Natural Earth II shaded relief",
    licence: Licence::PublicDomain,
    url: "https://www.naturalearthdata.com",
    changes: "",
};
pub const CREDIT_PROTOMAPS_FLORENCE: Credit = Credit {
    what: "Basemap archive",
    holder: "Protomaps, OpenStreetMap contributors",
    dataset: "Protomaps basemap, Florence demo archive",
    licence: Licence::Odbl,
    url: "https://www.openstreetmap.org/copyright",
    changes: "",
};
pub const CREDIT_OFM_SPRITES_UNVERIFIED: Credit = Credit {
    what: "Icon sprite sheet (fetched once, cached)",
    holder: "OpenFreeMap",
    dataset: "ofm_f384 sprite, fetched 2026-09-03",
    licence: Licence::Unverified,
    url: "https://openfreemap.org",
    changes: "",
};
/// The `nav-drive` demo route: a committed fixture by default, live OSRM
/// routing when `SWAT_ROUTING_*` is configured (right-click) -- see
/// `panels::streets::configure_nav`.
pub const CREDIT_OSRM_ROUTE: Credit = Credit {
    what: "Route (committed fixture; live routing when configured)",
    holder: "OpenStreetMap contributors (routing graph); Ferrostar (BSD-3) for navigation logic",
    dataset: "tests/fixtures/routes",
    licence: Licence::Odbl,
    url: "https://www.openstreetmap.org/copyright",
    changes: "API key stripped from the fixture",
};
/// The turn-by-turn instruction TEXT `nav-drive` reads out and displays --
/// a separate credit from [`CREDIT_OSRM_ROUTE`] above because it is a
/// separate licence: the route/graph data is ODbL, but this is vendored
/// CODE, Project OSRM's own `osrm-text-instructions` v0.13.4, ported to
/// Rust at `src/nav/osrm_text_instructions/` (see that module's own
/// `LICENSE.txt`, read directly for this exact holder text) under
/// BSD-2-Clause, which `src/nav/instructions.rs`'s module doc also names.
pub const CREDIT_OSRM_TEXT_INSTRUCTIONS: Credit = Credit {
    what: "Turn-by-turn instruction text",
    holder: "Project OSRM contributors",
    dataset: "osrm-text-instructions v0.13.4 (vendored, ported to Rust for English)",
    licence: Licence::Bsd2,
    url: "https://github.com/Project-OSRM/osrm-text-instructions",
    changes: "ported to Rust for English",
};

/// `big-points`'s dataset: generated in-process from a seed, so there is
/// no third party to attribute and no URL to link -- see
/// `every_cc_by_credit_names_holder_licence_link_and_changes`'s relaxed
/// URL rule (empty is allowed) in `examples/studio/registry.rs`.
pub const CREDIT_SYNTHETIC: Credit = Credit {
    what: "Points",
    holder: "generated in-process",
    dataset: "synthetic, seeded; no third-party data",
    licence: Licence::PublicDomain,
    url: "",
    changes: "",
};

/// `world-dots`' population grid: the Global Human Settlement Layer's
/// GHS-POP, CC BY 4.0. The holder and dataset are the engine's own wording
/// (the archive's `attribution`, which `MapView::frame_attribution` puts on
/// the map: "GHS-POP R2023A, European Commission JRC").
pub const CREDIT_GHSL: Credit = Credit {
    what: "Population grid",
    holder: "European Commission, Joint Research Centre (JRC)",
    dataset: "GHS-POP R2023A",
    licence: Licence::CcBy4,
    url: "https://human-settlement.emergency.copernicus.eu/ghs_pop2023.php",
    changes: "2025 epoch at 30″ aggregated into count tiles (tools/dotgrid-build); dots placed illustratively inside each cell",
};

/// `census`' block group boundaries: the Census Bureau's TIGER/Line
/// shapefiles, a work of the U.S. Government and so public domain.
pub const CREDIT_CENSUS_TIGER: Credit = Credit {
    what: "Block group boundaries",
    holder: "U.S. Census Bureau",
    dataset: "TIGER/Line Shapefiles 2023, block groups",
    licence: Licence::PublicDomain,
    url: "https://www.census.gov/geographies/mapping-files/time-series/geo/tiger-line-file.html",
    changes: "unsimplified; joined to income by GEOID",
};
/// `census`' fill: ACS 5-year median household income, public domain.
pub const CREDIT_CENSUS_ACS: Credit = Credit {
    what: "Median household income",
    holder: "U.S. Census Bureau",
    dataset: "ACS 5-year 2023, table B19013",
    licence: Licence::PublicDomain,
    url: "https://www.census.gov/programs-surveys/acs",
    changes: "negative sentinels shown as no data",
};

/// Whether a ClickHouse server answers at `url`: `GET {url}/ping` (before
/// any query string, which may carry a password) with a 1 s timeout, run on
/// `runtime`. The gallery awaits this before building a panel that
/// declares [`Requirement::ClickHouse`]; `false` for any failure, which
/// [`clickhouse_unreachable`] turns into the missing-data card.
pub fn probe_clickhouse(url: String, runtime: tokio::runtime::Handle) -> impl std::future::Future<Output = bool> {
    let ping = runtime.spawn(async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(1)).build().ok()?;
        let response = client.get(ping_url(&url)).send().await.ok()?;
        Some(response.status().is_success())
    });
    async move { matches!(ping.await, Ok(Some(true))) }
}

/// `{url}/ping`, the path inserted before any `?query`.
fn ping_url(url: &str) -> String {
    let (base, query) = url.split_once('?').map_or((url, None), |(b, q)| (b, Some(q)));
    let base = base.trim_end_matches('/');
    match query {
        Some(q) => format!("{base}/ping?{q}"),
        None => format!("{base}/ping"),
    }
}

/// Why a page that reads ClickHouse cannot open: no saved connection by
/// the name it uses. Names the Connections page, never an address.
pub fn no_clickhouse_connection(connection: &str) -> String {
    format!("needs the ClickHouse connection \"{connection}\", which is not on the Connections page. Add it there (or rename yours to it), then Check again.")
}

/// What a failed [`probe_clickhouse`] shows: the connection by name, the
/// Connections page to fix it on, and the address with every credential
/// redacted.
pub fn clickhouse_unreachable(connection: &str, url: &str, _load: &str) -> Status {
    Status::Missing {
        why: format!(
            "the ClickHouse connection \"{connection}\" ({}) did not answer (GET /ping, 1 s). Start the server with the cells loaded, or fix the connection on the Connections page, then Check again.",
            gpui_maps::sprite::redact_url_credentials(url)
        ),
    }
}

/// Whether a sidebar entry needs a one-time reachability check before it
/// is shown at all. Distinct from [`Requirement`], which gates a panel
/// that IS shown in the sidebar but may fall back or refuse to build: a
/// private Mapbox style (`swat-routes-dark`) is neither -- a token that
/// reaches every public Mapbox style can still 401/403/404 on it, so
/// showing the row and only then failing to build would be its own kind
/// of missing-data surprise. `Always` is every other panel, unchanged.
#[derive(Clone, Copy)]
pub enum Availability {
    Always,
    /// The sidebar shows this entry once a GET to the URL this returns
    /// answers 2xx ([`ProbeTarget::Get`]). Built from [`PanelEnv`] so a
    /// token can be spliced into the URL right before the probe and
    /// nowhere else. A tokenless session never makes the request: the
    /// page draws its keyless fallback ([`ProbeTarget::Keyless`]).
    Probe(fn(&PanelEnv) -> ProbeTarget),
}

/// What an [`Availability::Probe`] asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeTarget {
    /// Available when a GET to this URL answers 2xx.
    Get(String),
    /// Available with no request: there is no token, so the page draws its
    /// keyless OpenFreeMap fallback, which needs no probing.
    Keyless,
    /// Unavailable, with no request.
    #[allow(dead_code)] // not every example that includes the kit probes
    Unavailable,
}

/// Whether a GET to `url` answers 2xx: the check [`Availability::Probe`]
/// is evaluated with. 401/403/404 and any network error are both "no" --
/// the caller never learns which, so nothing here is tempted to log or
/// return the one case that might still carry a query-string token.
/// Project User-Agent, 3 s timeout, run on `runtime`.
pub fn probe_availability(url: String, runtime: tokio::runtime::Handle) -> impl std::future::Future<Output = bool> {
    let probe = runtime.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .user_agent(gpui_maps::tile_source::http::TILE_USER_AGENT)
            .build()
            .ok()?;
        let response = client.get(url).send().await.ok()?;
        Some(response.status().is_success())
    });
    async move { matches!(probe.await, Ok(Some(true))) }
}

/// [`Availability`] resolved against the real environment: `Always` is
/// always available; a `Probe` answers by its [`ProbeTarget`] -- keyless
/// is available and unavailable is not, both without a network call, and
/// a URL is up to [`probe_availability`].
pub async fn resolve_availability(availability: Availability, env: &PanelEnv) -> bool {
    match availability {
        Availability::Always => true,
        Availability::Probe(f) => match f(env) {
            ProbeTarget::Keyless => true,
            ProbeTarget::Unavailable => false,
            ProbeTarget::Get(url) => probe_availability(url, env.runtime.clone()).await,
        },
    }
}

/// What a built panel hands its host. `view` is the ONLY strong owner the
/// host keeps; `view.downgrade()` lets the host prove the panel went away.
pub struct Built {
    pub view: AnyView,
    pub map: Option<WeakEntity<MapView>>,
    pub start: (f64, f64, f64),
}

/// A page's map region when it is more than the one map in [`Built::map`]:
/// a split's two maps side by side. The host's floating chrome -- Studio's
/// agent chat card -- docks in the REGION's top-right corner, not the
/// primary map's, so on a split it stays at the window's right edge as on
/// every other page, not mid-screen. Coordinates are the page's own, which is the box the
/// host lays the panel out in.
#[derive(Clone)]
pub struct MapRegion {
    /// The region's size, logical px: the union of the page's maps.
    pub width: f32,
    pub height: f32,
    /// Every map in the region and its left edge in the region, left to
    /// right; the last stands at the region's right edge, so its cards are
    /// the ones that make room for a card docked there.
    pub maps: Vec<(gpui::Entity<MapView>, f32)>,
}

/// Reads a page's [`MapRegion`] as it is laid out now (`None`: the page
/// is gone, or it is one map again).
pub type RegionFn = std::rc::Rc<dyn Fn(&App) -> Option<MapRegion>>;

/// The pages that registered a [`MapRegion`], by their primary map.
#[derive(Default)]
struct MapRegions(Vec<(WeakEntity<MapView>, RegionFn)>);
impl gpui::Global for MapRegions {}

/// Registers `primary`'s page as spanning a [`MapRegion`] (a split page,
/// at build). Entries whose map has gone are dropped here.
#[allow(dead_code)] // the split pages only
pub fn set_map_region(primary: &gpui::Entity<MapView>, region: RegionFn, cx: &mut App) {
    let id = primary.entity_id();
    let regions = cx.default_global::<MapRegions>();
    regions.0.retain(|(m, _)| m.upgrade().is_some() && m.entity_id() != id);
    regions.0.push((primary.downgrade(), region));
}

/// The region `primary`'s page spans, if it registered one.
#[allow(dead_code)] // Studio only
pub fn map_region(primary: &gpui::Entity<MapView>, cx: &App) -> Option<MapRegion> {
    let id = primary.entity_id();
    let f = cx.try_global::<MapRegions>()?.0.iter().find(|(m, _)| m.entity_id() == id)?.1.clone();
    f(cx)
}

/// A small hint card for a panel's top-left corner: a few short lines
/// saying what the map shows and what the pointer does. Translucent, never
/// wider than 340 px, and clear of the map's centre; it has no mouse
/// listener and no `occlude`, so it takes no hitbox and the map under it
/// still gets every press and move.
pub fn hint_card(lines: &[&'static str]) -> gpui::Div {
    use gpui::{ParentElement as _, Styled as _};
    gpui::div()
        .absolute()
        .top(gpui::px(12.0))
        .left(gpui::px(12.0))
        .max_w(gpui::px(340.0))
        .px(gpui::px(10.0))
        .py(gpui::px(6.0))
        .rounded(gpui::px(6.0))
        .bg(gpui::rgba(0xffffffcc))
        .text_color(gpui::rgb(0x1f2933))
        .text_xs()
        .flex()
        .flex_col()
        .gap(gpui::px(2.0))
        .children(lines.iter().map(|line| gpui::div().child(*line)))
}

/// The repository files compiled into this crate by `kit/build.rs`.
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));
}

/// A dev-time override, OFF by default: a source checkout to read assets
/// from instead of the embedded copies, for live-editing a style without
/// a rebuild. `GPUI_MAPS_ASSET_ROOT=/path/to/gpui-map`.
pub const ASSET_ROOT_VAR: &str = "GPUI_MAPS_ASSET_ROOT";

/// Set by the self-contained guard (`examples/studio/self_contained.rs`):
/// [`repo_root`] panics, so any shipped path into the source tree fails
/// loudly in a test instead of silently in a copied binary.
pub const FORBID_SOURCE_TREE_VAR: &str = "GPUI_MAPS_FORBID_SOURCE_TREE";

fn asset_override() -> Option<PathBuf> {
    std::env::var_os(ASSET_ROOT_VAR).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn normalise(rel: &str) -> &str {
    rel.trim_start_matches("./")
}

/// The embedded bytes of `rel` (a path from the repository root), if this
/// build bundles it. Never touches the file system.
pub fn embedded_asset(rel: &str) -> Option<&'static [u8]> {
    let rel = normalise(rel);
    embedded::EMBEDDED.iter().find(|(p, _)| *p == rel).map(|(_, b)| *b)
}

/// The vendored files `kit/build.rs` leaves out on purpose (no shipped
/// code opens them), for the guard tests.
pub fn not_shipped_assets() -> &'static [&'static str] {
    embedded::NOT_SHIPPED
}

/// Every embedded path, for the guard tests.
pub fn embedded_asset_paths() -> impl Iterator<Item = &'static str> {
    embedded::EMBEDDED.iter().map(|(p, _)| *p)
}

/// The bytes of the bundled file `rel` (a path from the repository root,
/// e.g. `demos/positron-style.json`): the copy compiled into the binary,
/// or, only with [`ASSET_ROOT_VAR`] set, that checkout's file. Never the
/// build's source tree: a shipped binary has none.
pub fn asset_bytes(rel: &str) -> std::io::Result<Vec<u8>> {
    if let Some(root) = asset_override() {
        return std::fs::read(root.join(normalise(rel)));
    }
    embedded_asset(rel)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{rel} is not bundled with this build (kit/build.rs EMBED)")))
}

/// [`asset_bytes`] as UTF-8 text.
pub fn asset_string(rel: &str) -> std::io::Result<String> {
    String::from_utf8(asset_bytes(rel)?).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{rel}: {e}")))
}

/// A file path for the bundled file `rel`, for code that needs a path (an
/// image element, a renderer that opens a style by path): the embedded
/// copy written once under the system temp dir
/// (`gpui-maps-assets-<digest>/<rel>`), or the override checkout's file.
/// A `rel` this build does not bundle gets a path that does not exist, so
/// a read of it fails as missing -- it never falls back to the source tree.
pub fn asset(rel: &str) -> PathBuf {
    let rel = normalise(rel);
    if let Some(root) = asset_override() {
        return root.join(rel);
    }
    let dir = std::env::temp_dir().join(format!("gpui-maps-assets-{}", embedded::DIGEST));
    let path = dir.join(rel);
    if rel.split('/').any(|c| c == "..") {
        return path;
    }
    if let Some(bytes) = embedded_asset(rel) {
        let fresh = std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64);
        if !fresh {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Atomically: a temp file beside it, then a rename, so a
            // concurrent reader never sees half a file.
            let tmp = path.with_extension(format!("part-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
            if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    path
}

/// The repository root of the checkout this crate was BUILT from: the
/// parent of this crate's own directory. For tests and dev tools only --
/// a shipped binary has no source tree, so runtime code reads bundled
/// files through [`asset_bytes`] / [`asset`] (the self-contained guard
/// test forbids this in Studio's shipped code).
pub fn repo_root() -> PathBuf {
    if std::env::var_os(FORBID_SOURCE_TREE_VAR).is_some() {
        panic!("repo_root() called with {FORBID_SOURCE_TREE_VAR} set: shipped code must not read the build's source tree");
    }
    // This crate is `kit/`, one directory below the root.
    let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    kit.parent().unwrap_or(kit).to_path_buf()
}

pub fn compile_style(rel: &str) -> anyhow::Result<gpui_maps::style::Style> {
    let bytes = asset_bytes(rel).with_context(|| format!("reading {rel}"))?;
    gpui_maps::style_spec::parse_and_compile(&bytes).with_context(|| format!("compiling {rel}"))
}

/// An attribution's text with every tag dropped (`<a href=..>X</a>` -> `X`),
/// for a credit line drawn as plain text: the About pane's "On the map
/// now", a panel's own card.
#[allow(dead_code)] // not every example that includes the kit draws a credit
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// OpenFreeMap's credit, verbatim from its TileJSON: the library's
/// [`gpui_maps::basemaps::OPENFREEMAP_ATTRIBUTION_HTML`].
pub const OPENFREEMAP_CREDIT_HTML: &str = gpui_maps::basemaps::OPENFREEMAP_ATTRIBUTION_HTML;

/// Mapbox's credit for the composite source (was inline in mapbox_streets_demo.rs).
/// Mapbox's terms ALSO require the wordmark image, which the engine does not draw.
pub const MAPBOX_CREDIT_HTML: &str = concat!(
    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a> "#,
    r#"<a href="https://www.mapbox.com/contribute/" target="_blank">Improve this map</a>"#,
);

/// The library's [`gpui_maps::basemaps::openfreemap_planet`] behind the
/// gallery's disk cache ([`with_tile_cache`]).
pub fn openfreemap_planet() -> Arc<dyn TileSource> {
    Arc::new(with_tile_cache(gpui_maps::basemaps::openfreemap_planet(), Licence::Odbl))
}

/// The keyless style a page draws in place of its Mapbox one when Mapbox
/// is off or no token is configured -- a thin, example-side naming of the
/// library's [`gpui_maps::basemaps::Basemap`] ([`Self::basemap`]), which
/// embeds the documents and builds the source: [`Self::for_mapbox`] is the one
/// mapping every page reads, so "which open style stands in for Mapbox X"
/// is decided once.
///
/// Never Mapbox's own style document over OpenFreeMap tiles: the Mapbox
/// styles are Mapbox's (proprietary, `mapbox://` sources, a token), and
/// target Mapbox's tile schema besides. Every stand-in is an open style
/// over OpenFreeMap's planet tiles (OpenMapTiles schema), with OpenFreeMap
/// glyphs (Noto Sans, OFL) and OpenFreeMap's sprite:
///
/// - **Streets (open)**, **Dark (open)**, **Graphite** --
///   the look-alikes for Mapbox Streets (and Tokyo), Standard Dark, and
///   SWAT Routes Dark: OpenFreeMap Liberty recoloured by
///   `scripts/gen-open-styles.py` (see `examples/styles-open/README.md`).
/// - **Liberty** -- OpenFreeMap's own, a fork of maputnik/osm-liberty
///   (itself OSM Bright): code BSD-3-Clause, design CC BY 4.0
///   (OpenFreeMap styles' `LICENSE.md`).
/// - **Dark** -- for Mapbox Dark (v11). A fork of
///   openmaptiles/dark-matter-gl-style (CartoDB basemaps by Stamen and
///   Paul Norman): code BSD-3-Clause, design CC BY 4.0.
/// - **Positron** -- for Mapbox Light, where a pale minimal look is meant.
///   A fork of openmaptiles/positron-gl-style: same terms as Dark.
///
/// The tiles are OpenMapTiles-schema vector tiles of OpenStreetMap data
/// (ODbL), credited on the map by [`OPENFREEMAP_CREDIT_HTML`] -- verbatim
/// the `attribution` of `tiles.openfreemap.org/planet`'s TileJSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeylessStyle {
    Liberty,
    Dark,
    Positron,
    /// Mapbox Streets' look on open data.
    OpenStreets,
    /// Mapbox Standard Dark's look on open data.
    OpenDark,
    /// SWAT Routes Dark's look on open data.
    OpenSwatRoutesDark,
}

#[allow(dead_code)] // not every example that includes the kit uses every helper
impl KeylessStyle {
    pub const ALL: &'static [KeylessStyle] = &[
        KeylessStyle::Liberty,
        KeylessStyle::Dark,
        KeylessStyle::Positron,
        KeylessStyle::OpenStreets,
        KeylessStyle::OpenDark,
        KeylessStyle::OpenSwatRoutesDark,
    ];

    /// The look-alikes: our own styles, with no hosted copy.
    pub const LOOKALIKES: &'static [KeylessStyle] =
        &[KeylessStyle::OpenStreets, KeylessStyle::OpenDark, KeylessStyle::OpenSwatRoutesDark];

    /// The name a fallback banner and note print. Never says Mapbox: the
    /// map drawn is not Mapbox's.
    pub const fn label(self) -> &'static str {
        match self {
            KeylessStyle::Liberty => "OpenFreeMap Liberty",
            KeylessStyle::Dark => "OpenFreeMap Dark",
            KeylessStyle::Positron => "OpenFreeMap Positron",
            KeylessStyle::OpenStreets => "Streets (open)",
            KeylessStyle::OpenDark => "Dark (open)",
            KeylessStyle::OpenSwatRoutesDark => "Graphite",
        }
    }

    /// The library basemap this names.
    pub const fn basemap(self) -> gpui_maps::basemaps::Basemap {
        use gpui_maps::basemaps::Basemap;
        match self {
            KeylessStyle::Liberty => Basemap::Liberty,
            KeylessStyle::Dark => Basemap::Dark,
            KeylessStyle::Positron => Basemap::Positron,
            KeylessStyle::OpenStreets => Basemap::StreetsLight,
            KeylessStyle::OpenDark => Basemap::OpenDark,
            KeylessStyle::OpenSwatRoutesDark => Basemap::SwatDark,
        }
    }

    /// The vendored style document, an [`asset`] path (the file the
    /// library embeds).
    pub const fn style_file(self) -> &'static str {
        self.basemap().vendored_path()
    }

    /// The same style served live, for a MapLibre GL JS map elsewhere:
    /// `None` for a look-alike, which is ours and served by nobody.
    pub const fn hosted_url(self) -> Option<&'static str> {
        self.basemap().hosted_url()
    }

    /// The keyless stand-in for a Mapbox style, named by its vendored
    /// file or its `mapbox://styles/...` URL: SWAT Routes Dark gets its
    /// look-alike, Standard Dark (and its exported reference) the dark
    /// look-alike, any other dark one (Mapbox Dark) OpenFreeMap Dark, a
    /// light one Positron, and everything else -- Streets above all, Tokyo
    /// with it -- the Streets look-alike.
    pub fn for_mapbox(style: &str) -> KeylessStyle {
        let s = style.to_ascii_lowercase();
        if s.contains("swat-routes-dark") {
            KeylessStyle::OpenSwatRoutesDark
        } else if (s.contains("standard") && (s.contains("dark") || s.contains("night")))
            || s == "mapbox://styles/njachowski/cmf0r29zy00b301pldhj7gc7j"
        {
            KeylessStyle::OpenDark
        } else if s.contains("dark") || s.contains("night") {
            KeylessStyle::Dark
        } else if s.contains("light") {
            KeylessStyle::Positron
        } else {
            KeylessStyle::OpenStreets
        }
    }

    /// The embedded document, parsed.
    pub fn document(self) -> anyhow::Result<serde_json::Value> {
        self.basemap().document()
    }
}

/// A [`KeylessStyle`] ready to mount: the compiled style, OpenFreeMap's
/// planet source (whose attribution is OpenFreeMap's, never Mapbox's), the
/// style document, and its sprite URL (keyless: `spawn_sprites` with no
/// token).
pub struct KeylessBasemap {
    pub kind: KeylessStyle,
    pub style: gpui_maps::style::Style,
    pub source: Arc<dyn TileSource>,
    pub document: serde_json::Value,
    pub sprite: Option<String>,
}

/// [`KeylessBasemap`] for `kind`: [`gpui_maps::basemaps::load_with_disk_cache`]
/// behind the gallery's tile cache ([`tile_cache_for`]).
pub fn keyless_basemap(kind: KeylessStyle) -> anyhow::Result<KeylessBasemap> {
    let b = gpui_maps::basemaps::load_with_disk_cache(kind.basemap(), tile_cache_for(Licence::Odbl))?;
    Ok(KeylessBasemap { kind, style: b.style, source: b.source, document: b.document, sprite: b.sprite })
}

/// A page's basemap, resolved from its wanted Mapbox style and the token:
/// that style over Mapbox's composite with a token (exactly what the page
/// drew before keyless fallbacks existed), else its
/// [`KeylessStyle::for_mapbox`] stand-in. `sprite_token` is what
/// [`spawn_sprites`] gets: the token for a `mapbox://` sprite, none for
/// OpenFreeMap's.
pub struct PageBasemap {
    pub style: gpui_maps::style::Style,
    pub source: Arc<dyn TileSource>,
    pub sprite: Option<String>,
    pub sprite_token: Option<String>,
    /// `Some` when the keyless stand-in is what is drawn.
    pub keyless: Option<KeylessStyle>,
}

/// The shared resolver: [`PageBasemap`] for the Mapbox style document
/// `style_file` (an [`asset`] path), its composite tileset list read from
/// `composite_from` (usually the same file; see `panels::standard_dark`
/// for why not always).
#[allow(dead_code)] // not every example that includes the kit draws a Mapbox page
pub fn mapbox_or_keyless(style_file: &str, composite_from: &str, token: Option<&str>) -> anyhow::Result<PageBasemap> {
    // The Mapbox-authored styles are not redistributable, so the open
    // library tree has none: a token then still draws the keyless look-alike
    // (`scripts/fetch-mapbox-styles.sh` fetches the originals).
    let token = token.filter(|_| {
        let bundled = mapbox_style_bundled(style_file) && mapbox_style_bundled(composite_from);
        if !bundled {
            eprintln!("gpui-maps: {style_file} is not in this checkout; drawing its open look-alike (scripts/fetch-mapbox-styles.sh fetches it)");
        }
        bundled
    });
    match token.filter(|t| !t.trim().is_empty()) {
        Some(token) => {
            let style = compile_style(style_file)?;
            let composite_bytes = asset_bytes(composite_from).with_context(|| format!("reading {composite_from}"))?;
            let composite_json: serde_json::Value = serde_json::from_slice(&composite_bytes)?;
            let source = mapbox_composite(&composite_json, token)?;
            let sprite = style.sprite.clone();
            Ok(PageBasemap { style, source, sprite, sprite_token: Some(token.to_string()), keyless: None })
        }
        None => {
            let b = keyless_basemap(KeylessStyle::for_mapbox(style_file))?;
            Ok(PageBasemap { style: b.style, source: b.source, sprite: b.sprite, sprite_token: None, keyless: Some(b.kind) })
        }
    }
}

/// Whether a Mapbox-authored style `rel` is bundled with this build (it is
/// in the monorepo, not in the open library tree).
pub fn mapbox_style_bundled(rel: &str) -> bool {
    asset_bytes(rel).is_ok()
}

/// The basemap credit a page owes while it draws a [`KeylessStyle`]: the
/// tiles (OSM via OpenMapTiles, served by OpenFreeMap) and the style's
/// design. Listed beside the Mapbox credit on pages that draw either.
pub const CREDIT_OPENFREEMAP_KEYLESS: Credit = Credit {
    what: "Basemap tiles and style (without Mapbox)",
    holder: "OpenFreeMap, OpenMapTiles, OpenStreetMap contributors",
    dataset: "OpenFreeMap planet (OpenMapTiles schema); Liberty/Dark/Positron styles and our open look-alikes of them",
    licence: Licence::Odbl,
    url: "https://www.openstreetmap.org/copyright",
    changes: "Liberty/Dark/Positron drawn as served; the Streets (open), Dark (open) and Graphite look-alikes recolour Liberty (style code BSD-3-Clause, design CC BY 4.0)",
};

/// A [`Coverage`] framing plain data bounds (`[min_lon, min_lat, max_lon,
/// max_lat]`): a CSV's computed min/max, a FlatGeobuf header's envelope, or
/// a GeoParquet file's `geo` metadata bbox. `None` when the box is not
/// usable for framing: non-finite, inverted (`min > max` on either axis),
/// or wider than 180 degrees of longitude.
///
/// The last case is what `kit/src/panels/fgb.rs`'s default panel actually
/// hits: the FlatGeobuf project's `UScounties.fgb` states a header envelope
/// of roughly -179.15 .. 179.78 -- not a real 359-degree span, but the
/// Aleutian Islands crossing the antimeridian, read by a header that has no
/// notion of wraparound. Framing that literally opens the camera on the
/// whole planet at z1, which hides the counties behind a smudge -- worse
/// than the off-screen bug this function exists to fix. A bounded regional
/// extract legitimately spanning more than half the globe is rare enough
/// (Russia, Fiji, a world-cities sample) that erring toward "this bbox
/// isn't usable, fall back to the caller's own fixed default" is the safer
/// read of a file this wide; it is also why the bundled CSV/FlatGeobuf/
/// GeoParquet world-cities samples (real span: Honolulu to Fiji, ~330
/// degrees) keep opening on their existing whole-world `START` instead of
/// churning through this path for no benefit.
///
/// None of these sources state an opening zoom the way a PMTiles/MBTiles
/// archive's header can, so `center_zoom` is always `None` --
/// `Coverage::opening_camera` then frames the box itself -- and
/// `min_zoom`/`max_zoom` are left wide open (0..=22, overlay data has no
/// tile ceiling of its own) so the clamp in `zoom_that_frames` never binds.
pub fn coverage_from_bounds(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Option<gpui_maps::tile_source::pmtiles::Coverage> {
    let finite = [min_lon, min_lat, max_lon, max_lat].into_iter().all(f64::is_finite);
    if !finite || min_lon > max_lon || min_lat > max_lat || (max_lon - min_lon) > 180.0 {
        return None;
    }
    Some(gpui_maps::tile_source::pmtiles::Coverage {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
        center_lon: (min_lon + max_lon) / 2.0,
        center_lat: (min_lat + max_lat) / 2.0,
        center_zoom: None,
        min_zoom: 0,
        max_zoom: gpui_maps::tile_source::DEFAULT_MAX_ZOOM,
    })
}

/// [`coverage_from_bounds`] merged from two optional bboxes (`[min_lon,
/// min_lat, max_lon, max_lat]`), each from a panel's own source: `None`
/// when neither source stated one, so the caller can fall back to its own
/// fixed `START` rather than framing an empty box.
pub fn union_bbox(a: Option<[f64; 4]>, b: Option<[f64; 4]>) -> Option<[f64; 4]> {
    match (a, b) {
        (Some(a), Some(b)) => Some([a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The demos' disk cache for live tiles: `GPUI_MAP_TILE_CACHE`.
///
/// The engine caches nothing unless told to
/// (`HttpTileSource::with_disk_cache`); this is the gallery's decision:
///
/// - unset: ON for the open licences ([`tile_cache_on`]'s allowlist:
///   OpenFreeMap, Natural Earth, Sentinel-2 cloudless 2016, Singapore open
///   data), OFF for everything else, Mapbox included;
/// - `on`: on for every source, Mapbox included;
/// - `off`: off for every source.
///
/// **Licensing:** whether Mapbox tiles may be cached, and for how long, is
/// governed by Mapbox's terms of service, and complying with them is the
/// developer's responsibility -- which is why `on` is an explicit choice
/// and never the default for a `MapboxTos` source.
///
/// Entries live in `~/.cache/gpui-map/http-tiles` (no `$HOME`, no cache:
/// never a world-writable fallback), one directory shared by every source, held to
/// `GPUI_MAP_TILE_CACHE_MB` (default 512) least-recently-used. File names
/// are hashes of the URL with its token redacted; no token is written.
pub fn with_tile_cache(source: HttpTileSource, licence: Licence) -> HttpTileSource {
    match tile_cache_for(licence) {
        Some(cache) => source.with_disk_cache(cache),
        None => source,
    }
}

/// Whether `GPUI_MAP_TILE_CACHE=setting` caches a `licence` source: `on`
/// and `off` decide for every source, anything else is the default -- an
/// ALLOWLIST of the open data licences (ODbL, CC BY 4.0, public domain,
/// Singapore Open Data). Everything else -- Mapbox's terms, a non-commercial
/// licence, an unverified one -- stays off unless asked for.
pub fn tile_cache_on(setting: Option<&str>, licence: Licence) -> bool {
    match setting.map(str::trim) {
        Some("on" | "1" | "true") => true,
        Some("off" | "0" | "false") => false,
        _ => matches!(licence, Licence::Odbl | Licence::CcBy4 | Licence::PublicDomain | Licence::SgOpenData),
    }
}

/// [`with_tile_cache`]'s configuration, `None` when it is off.
pub fn tile_cache_for(licence: Licence) -> Option<gpui_maps::tile_source::disk_cache::DiskCache> {
    if !tile_cache_on(std::env::var("GPUI_MAP_TILE_CACHE").ok().as_deref(), licence) {
        return None;
    }
    // Never a shared, world-writable fallback such as `$TMPDIR`: another
    // user could plant or read entries there. No home, no cache.
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let dir = PathBuf::from(home).join(".cache/gpui-map/http-tiles");
    let mb = std::env::var("GPUI_MAP_TILE_CACHE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(512);
    Some(gpui_maps::tile_source::disk_cache::DiskCache::new(dir, mb.saturating_mul(1024 * 1024)))
}

/// Tiles for a Mapbox style's `sources.composite` (the tileset list is read
/// from the style, never hard-coded twice). Max zoom 16, as the demo found.
pub fn mapbox_composite(style_json: &serde_json::Value, token: &str) -> anyhow::Result<Arc<dyn TileSource>> {
    let tilesets = mapbox_tilesets(style_json)?;
    let url = format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}");
    Ok(Arc::new(with_tile_cache(HttpTileSource::new(url, 16, MAPBOX_CREDIT_HTML), Licence::MapboxTos)))
}

/// The comma-separated tileset list of a Mapbox style's
/// `sources.composite.url` (`mapbox://a,b,c` -> `a,b,c`). The one parse
/// [`mapbox_composite`] and [`composite_tile_url_template`] share.
pub fn mapbox_tilesets(style_json: &serde_json::Value) -> anyhow::Result<&str> {
    style_json["sources"]["composite"]["url"]
        .as_str()
        .and_then(|u| u.strip_prefix("mapbox://"))
        .context("the style has no mapbox:// sources.composite.url")
}

/// The tile URL template a committed Mapbox style's composite source
/// resolves to, WITHOUT a token (`Provenance::tile_url` must never carry
/// one) -- for a standalone main's own debug-port registration, ahead of
/// opening a window. `style` is an [`asset`] path.
pub fn composite_tile_url_template(style: &str) -> anyhow::Result<String> {
    let bytes = asset_bytes(style)?;
    let json: serde_json::Value = serde_json::from_slice(&bytes)?;
    let tilesets = mapbox_tilesets(&json).with_context(|| style.to_string())?;
    Ok(format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt"))
}

/// A panel's opening camera: `default`, unless `GPUI_MAP_LON`/`LAT`/`ZOOM`
/// override it. Read the same way in the gallery and standalone, so a
/// camera that has to be driven to a specific place or zoom needs no
/// recompile in either; a value that does not parse to a finite number is
/// ignored.
pub fn camera_from_env(default: (f64, f64, f64)) -> (f64, f64, f64) {
    fn or(name: &str, fallback: f64) -> f64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(fallback)
    }
    (or("GPUI_MAP_LON", default.0), or("GPUI_MAP_LAT", default.1), or("GPUI_MAP_ZOOM", default.2))
}

/// Fetches a style's sprite sheet off the UI thread and hands it to the
/// map when it lands; drops a sheet whose style was swapped meanwhile.
/// Both live in the library now ([`gpui_maps::basemaps`]).
#[allow(unused_imports)] // not every example that includes the kit fetches sprites
pub use gpui_maps::basemaps::{spawn_sprites, sprites_land};

/// One machine-readable metric for `examples/perf_gate.rs` (driven by
/// `scripts/perf-gate.sh`) and `scripts/gallery-parity.sh`, printed only
/// when `US_BG_GATE` is set: `gate: NAME VALUE UNIT`. Verbatim from
/// `demos/us_block_groups.rs`'s own `gate` -- the same env var, the same
/// format -- so a standalone demo and the gallery panel it shares produce
/// lines the parity script parses identically.
pub fn gate(name: &str, value: f64, unit: &str) {
    if std::env::var_os("US_BG_GATE").is_some() {
        println!("gate: {name} {value} {unit}");
    }
}

/// The peak resident set size this process has ever held, in bytes.
/// `getrusage(RUSAGE_SELF).ru_maxrss` is bytes on macOS (kilobytes on
/// Linux, which no example here targets). Duplicated out of
/// `demos/us_block_groups.rs`, `demos/big_points.rs` and
/// `demos/world_dots.rs` into one shared place.
#[cfg(target_os = "macos")]
pub fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct it is given and nothing else.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as u64
}

#[cfg(not(target_os = "macos"))]
pub fn peak_rss_bytes() -> u64 {
    0
}

pub fn gb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// This process's `phys_footprint` -- the kernel's own charge for its
/// memory (Activity Monitor's "Memory"), the field `task_info(TASK_VM_INFO)`
/// reports, read here through `proc_pid_rusage(RUSAGE_INFO_V2)` which
/// carries the same counter. Unlike RSS it does not count clean pages the
/// process could drop. 0 where it cannot be read.
#[cfg(target_os = "macos")]
pub fn phys_footprint_bytes() -> u64 {
    // SAFETY: `proc_pid_rusage` writes one `rusage_info_v2` for the
    // RUSAGE_INFO_V2 flavour through the pointer it is given, and nothing
    // else; the struct is plain data, so zeroed is a valid initial value.
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let r = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
        );
        if r == 0 { info.ri_phys_footprint } else { 0 }
    }
}
#[cfg(not(target_os = "macos"))]
pub fn phys_footprint_bytes() -> u64 {
    0
}

/// The census drift a scripted pan follows: `start + 12*sin(0.2 t)`,
/// verbatim from `demos/us_block_groups.rs`'s `--pan` task.
pub fn pan_lon(start_lon: f64, t_secs: f64) -> f64 {
    start_lon + 12.0 * (t_secs * 0.2).sin()
}

/// The census `--pan` task, verbatim: a 16 ms timer, `set_camera` at the
/// drifting longitude, `cx.notify()`, until the map is gone (the host
/// switched panels, or the window closed). `start` is `(lon, lat, zoom)`
/// at the moment the pan begins; only longitude drifts.
pub fn pan_task(map: WeakEntity<MapView>, start: (f64, f64, f64), cx: &mut App) {
    cx.spawn(async move |cx| {
        let started = Instant::now();
        loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            let t = started.elapsed().as_secs_f64();
            let lon = pan_lon(start.0, t);
            if map
                .update(cx, |view, cx| {
                    view.set_camera(lon, start.1, start.2);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        }
    })
    .detach();
}

/// A window opened over a scripted `--pan`, snapshotting `paint_census()`
/// and `allocation_stats()` at [`Self::open`] and diffing them at
/// [`Self::report`], which prints the census `pan.*` gate lines verbatim
/// (`demos/us_block_groups.rs` lines 3173-3183 at `76ce380`) with `prefix`
/// standing in for `"pan"`.
pub struct PanGate {
    opened: Instant,
    paints_submitted: u64,
    paints_skipped_occluded: u64,
    at: AllocationStats,
}

impl PanGate {
    pub fn open() -> Self {
        let census = paint_census();
        Self {
            opened: Instant::now(),
            paints_submitted: census.paints_submitted,
            paints_skipped_occluded: census.paints_skipped_occluded,
            at: allocation_stats(),
        }
    }

    pub fn report(&self, prefix: &str) {
        let secs = self.opened.elapsed().as_secs_f64();
        for (name, value, unit) in self.rows(prefix, secs, paint_census(), allocation_stats()) {
            gate(&name, value, unit);
        }
    }

    /// [`Self::report`]'s rows, `(name, value, unit)` in print order, for
    /// a window `secs` long ending at `census` and `now`. The GPU pass row
    /// is there only when the window took a GPU pass sample.
    pub fn rows(
        &self,
        prefix: &str,
        secs: f64,
        census: gpui_maps::render::PaintCensus,
        now: AllocationStats,
    ) -> Vec<(String, f64, &'static str)> {
        let frames = census.paints_submitted - self.paints_submitted;
        let mut rows = vec![
            (format!("{prefix}.window_s"), secs, "s"),
            (
                format!("{prefix}.occluded_paints"),
                (census.paints_skipped_occluded - self.paints_skipped_occluded) as f64,
                "paints",
            ),
            (format!("{prefix}.draw_calls"), (now.draw_calls_total - self.at.draw_calls_total) as f64, "draws"),
            (format!("{prefix}.fps"), frames as f64 / secs, "fps"),
            (format!("{prefix}.frame_interval_ms"), secs * 1e3 / frames.max(1) as f64, "ms"),
        ];
        let samples = now.gpu_pass_samples - self.at.gpu_pass_samples;
        if samples > 0 {
            let sum = now.gpu_tile_pass_nanos_sum - self.at.gpu_tile_pass_nanos_sum;
            rows.push((format!("{prefix}.gpu_pass_mean_ms"), sum as f64 / samples as f64 / 1e6, "ms"));
        }
        rows.push((format!("{prefix}.rss_mb"), now.rss_bytes as f64 / 1e6, "MB"));
        rows
    }
}

// ---------------------------------------------------------------------------
// The skip-cause compile diagnostics: one shared copy for every dark-basemap
// standalone main (`mapbox_standard_dark`, `swat_routes_dark`, and any
// future one) that used to carry its own ~150-line copy of this. `label`
// names the style in the printed header, the temp capture file and the
// panic message on a compile failure. `print_skip_census` is the only `pub`
// item: a standalone main calls it once, ahead of `build`, so the gallery
// (which compiles silently through `compile_style`) never repeats this
// `eprintln!` block on every switch to that panel.
// ---------------------------------------------------------------------------

/// Compiles `style_bytes` a SECOND time, with stderr captured, purely to
/// read back and summarize `parse_and_compile`'s own per-layer skip
/// notices -- the only place it records *why* a layer produced no output
/// is its own per-layer `eprintln!` (there is no structured, public API
/// for it -- see `style_spec::parse_and_compile`'s doc comment on the
/// compiled/skipped taxonomy).
pub fn print_skip_census(label: &str, style_bytes: &[u8]) {
    let (style, diagnostics) = compile_with_skip_diagnostics(label, style_bytes);
    print_skip_diagnostics(label, style_bytes, &style, &diagnostics);
}

/// Every per-layer skip notice `parse_and_compile` printed while compiling the one style,
/// still in the order they were emitted.
struct SkipDiagnostics {
    /// Raw, unparsed lines this run's stderr held that looked like a `style_spec:
    /// skipping ...` notice. Kept verbatim (not just the parsed `(id, cause)` pairs) so
    /// nothing this heuristic fails to parse is silently dropped from the total.
    skip_lines: Vec<String>,
}

/// Runs `parse_and_compile` with stderr captured, so the per-layer skip notices it prints
/// can be read back and summarized instead of just scrolling past.
fn compile_with_skip_diagnostics(label: &str, style_bytes: &[u8]) -> (gpui_maps::style::Style, SkipDiagnostics) {
    #[cfg(unix)]
    {
        let (result, captured) = capture_stderr(label, || gpui_maps::style_spec::parse_and_compile(style_bytes));
        let style = result.unwrap_or_else(|e| panic!("vendored {label} style must compile: {e:#}"));
        eprint!("{captured}");
        let skip_lines =
            captured.lines().filter(|line| line.starts_with("style_spec: skipping")).map(str::to_string).collect();
        (style, SkipDiagnostics { skip_lines })
    }
    #[cfg(not(unix))]
    {
        eprintln!("{label}: stderr capture (for the skip-cause breakdown) is only implemented for unix; compiling without it.");
        let style = gpui_maps::style_spec::parse_and_compile(style_bytes)
            .unwrap_or_else(|e| panic!("vendored {label} style must compile: {e:#}"));
        (style, SkipDiagnostics { skip_lines: Vec::new() })
    }
}

#[cfg(unix)]
extern "C" {
    fn dup(fd: i32) -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn close(fd: i32) -> i32;
}

#[cfg(unix)]
const STDERR_FD: std::os::fd::RawFd = 2;

/// Restores fd 2 on every exit path out of `capture_stderr`, panic included.
#[cfg(unix)]
struct StderrGuard {
    saved_stderr: i32,
}

#[cfg(unix)]
impl Drop for StderrGuard {
    fn drop(&mut self) {
        use std::io::Write;
        std::io::stderr().flush().ok();
        let restored = unsafe { dup2(self.saved_stderr, STDERR_FD) };
        if restored < 0 {
            eprintln!("gpui-maps: capture_stderr failed to restore the real stderr (dup2 error)");
        }
        unsafe { close(self.saved_stderr) };
    }
}

#[cfg(unix)]
fn capture_stderr<T>(label: &str, f: impl FnOnce() -> T) -> (T, String) {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let path = std::env::temp_dir().join(format!("gpui-maps-{label}-stderr-{}.txt", std::process::id()));
    let capture_file = File::create(&path).expect("create temp stderr-capture file");
    let capture_fd = capture_file.as_raw_fd();

    std::io::stderr().flush().ok();
    let saved_stderr = unsafe { dup(STDERR_FD) };
    assert!(saved_stderr >= 0, "dup(stderr) failed");
    let redirected = unsafe { dup2(capture_fd, STDERR_FD) };
    assert!(redirected >= 0, "dup2(capture -> stderr) failed");
    let guard = StderrGuard { saved_stderr };

    let result = f();

    drop(guard);
    drop(capture_file);

    let mut captured = String::new();
    if let Ok(mut reopened) = File::open(&path) {
        reopened.read_to_string(&mut captured).ok();
    }
    let _ = std::fs::remove_file(&path);

    (result, captured)
}

/// One `style_spec: skipping ...` line, split into the layer id and the message after it.
fn parse_skip_line(line: &str) -> Option<(&str, &str)> {
    let rest = line
        .strip_prefix("style_spec: skipping fill layer \"")
        .or_else(|| line.strip_prefix("style_spec: skipping layer \""))?;
    let quote_end = rest.find('"')?;
    let id = &rest[..quote_end];
    rest[quote_end + 1..].strip_prefix(": ").map(|message| (id, message))
}

/// Buckets one skip message into a human-readable cause.
fn classify_skip_cause(message: &str) -> String {
    if let Some(layer_type) = message.strip_prefix("unsupported layer type ") {
        return format!("unsupported layer type {layer_type}");
    }
    if message.contains("no \"fill-color\"") {
        return "no fill-color (pattern fills unsupported)".to_string();
    }
    if message.contains("\"pitch\"") {
        return "\"pitch\" filter operand (Phase 3, deferred)".to_string();
    }
    if message.contains("is-active-floor") {
        return "\"is-active-floor\" filter operator (indoor source not loaded)".to_string();
    }
    if message.contains("symbol-placement") {
        return "unsupported symbol-placement (line / line-center)".to_string();
    }
    if message.contains("\"coalesce\"") {
        return "\"coalesce\" filter/expression operand".to_string();
    }
    if message.contains("icon-image") {
        return "icon-image expression shape (layout path not folded)".to_string();
    }
    if message.contains("\"config\"") {
        return "config-driven filter operand".to_string();
    }
    if message.contains("\"in\" filter") {
        return "\"in\" filter operand".to_string();
    }
    if message.contains("\"number\"") {
        return "\"number\" filter operand".to_string();
    }
    let truncated: String = message.chars().take(96).collect();
    format!("other: {truncated}")
}

fn print_skip_diagnostics(label: &str, style_bytes: &[u8], style: &gpui_maps::style::Style, diagnostics: &SkipDiagnostics) {
    let root: serde_json::Value = serde_json::from_slice(style_bytes).expect("style JSON was already parsed above");
    let imported_layers = gpui_maps::style_spec::parse_imports(&root)
        .expect("imports must parse")
        .iter()
        .map(|import| import.data["layers"].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    let root_layers = root["layers"].as_array().map_or(0, Vec::len);
    let declared_layers = imported_layers + root_layers;

    eprintln!();
    eprintln!("== {label}: compile diagnostics ==");
    eprintln!("declared layers:  {declared_layers} ({imported_layers} imported basemap + {root_layers} root overlay)");
    eprintln!("compiled StyleLayers: {}", style.layers.len());

    let mut causes: std::collections::BTreeMap<String, Vec<&str>> = std::collections::BTreeMap::new();
    let mut unparsed = 0usize;
    for line in &diagnostics.skip_lines {
        match parse_skip_line(line) {
            Some((id, message)) => causes.entry(classify_skip_cause(message)).or_default().push(id),
            None => unparsed += 1,
        }
    }

    let mut by_count: Vec<(&String, &Vec<&str>)> = causes.iter().collect();
    by_count.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    eprintln!("skipped layers (per-layer notices seen this run): {}", diagnostics.skip_lines.len());
    for (cause, ids) in &by_count {
        eprintln!("  {:>3}  {cause}", ids.len());
        if ids.len() <= 6 {
            eprintln!("       {ids:?}");
        }
    }
    if unparsed > 0 {
        eprintln!("  {unparsed} skip notice(s) did not match the expected \"style_spec: skipping ...\" shape");
    }
    eprintln!(
        "Note: hidden layers (layout.visibility: \"none\") and \"slot\" markers are dropped \
         silently by design and are NOT among the skip notices above -- see \
         `style_spec::compile_single_layer`'s doc comment."
    );
    eprintln!("== end diagnostics ==");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every open look-alike parses, compiles through the engine's style
    /// pipeline, names no Mapbox host anywhere (sources, sprite, glyphs),
    /// draws its glyphs and sprite from OpenFreeMap, and says where it
    /// came from.
    #[test]
    fn open_lookalikes_compile_and_name_no_mapbox_host() {
        for kind in KeylessStyle::LOOKALIKES {
            let b = keyless_basemap(*kind).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
            assert!(b.style.layers.len() > 80, "{kind:?}: {} layers", b.style.layers.len());
            let text = std::fs::read_to_string(asset(kind.style_file())).expect("readable");
            for banned in ["mapbox://", "api.mapbox.com", "tiles.mapbox.com", "events.mapbox.com"] {
                assert!(!text.contains(banned), "{kind:?} names {banned}");
            }
            assert!(b.sprite.as_deref().is_some_and(|s| s.starts_with("https://tiles.openfreemap.org/sprites/")), "{kind:?}");
            assert!(b.document["glyphs"].as_str().is_some_and(|g| g.starts_with("https://tiles.openfreemap.org/fonts/")), "{kind:?}");
            assert_eq!(b.document["sources"]["openmaptiles"]["url"], "https://tiles.openfreemap.org/planet", "{kind:?}");
            assert!(b.document["metadata"]["gpui-map:provenance"].as_str().is_some_and(|p| p.contains("OpenFreeMap Liberty")), "{kind:?}");
            assert_eq!(b.document["name"], kind.label(), "{kind:?}: the document is named as the banner names it");
            assert!(kind.hosted_url().is_none(), "{kind:?}");
        }
    }

    /// Compiling is not rendering: each look-alike, matched against a real
    /// OpenMapTiles tile (Singapore z14, the committed bench fixture),
    /// paints water and land fills, road lines and labels.
    #[test]
    fn open_lookalikes_paint_water_roads_and_labels_on_a_real_tile() {
        use gpui_maps::style::PaintOp;
        // A test fixture, not a shipped asset (kit/build.rs does not embed
        // it), so it is read from the checkout.
        let bytes = std::fs::read(repo_root().join("benches/fixtures/singapore_z14.pbf")).expect("the bench fixture");
        let tile = gpui_maps::mvt::decode(bytes).expect("decodes");
        for kind in KeylessStyle::LOOKALIKES {
            let style = compile_style(kind.style_file()).expect("compiles");
            let plan = gpui_maps::style::match_tile(&tile, &style, 14);
            let fills = plan.ops.iter().filter(|o| matches!(o, PaintOp::Fill { .. })).count();
            let lines = plan.ops.iter().filter(|o| matches!(o, PaintOp::Line { .. })).count();
            let labels = gpui_maps::style::match_tile_symbols(&tile, &style, 14, 8.0).features.len();
            // Dark (open) hides POIs and road names as Standard Dark does, so its
            // labels are the place and water names alone.
            assert!(fills > 100 && lines > 100 && labels >= 3, "{kind:?}: fills {fills}, lines {lines}, labels {labels}");
        }
    }

    /// The look-alike mapping, by every name a page wants Mapbox by.
    #[test]
    fn each_mapbox_style_maps_to_its_lookalike() {
        for (style, want) in [
            ("examples/mapbox-streets-style.json", KeylessStyle::OpenStreets),
            ("mapbox://styles/mapbox/streets-v12", KeylessStyle::OpenStreets),
            ("examples/mapbox-standard-dark-style.json", KeylessStyle::OpenDark),
            ("mapbox://styles/njachowski/cmf0r29zy00b301pldhj7gc7j", KeylessStyle::OpenDark),
            ("examples/swat-routes-dark-style.json", KeylessStyle::OpenSwatRoutesDark),
            ("mapbox/dark-v11", KeylessStyle::Dark),
            ("demos/mapbox-light-style.json", KeylessStyle::Positron),
        ] {
            assert_eq!(KeylessStyle::for_mapbox(style), want, "{style}");
        }
    }

    #[test]
    fn parse_skip_line_splits_id_and_message() {
        assert_eq!(
            parse_skip_line(r#"style_spec: skipping layer "roads": unsupported layer type heatmap"#),
            Some(("roads", "unsupported layer type heatmap"))
        );
        assert_eq!(
            parse_skip_line(r#"style_spec: skipping fill layer "water": no "fill-color" (pattern fills are not supported)"#),
            Some(("water", "no \"fill-color\" (pattern fills are not supported)"))
        );
        assert_eq!(parse_skip_line("not a skip line"), None);
    }

    #[test]
    fn classify_skip_cause_buckets_known_causes_and_falls_back_to_other() {
        assert_eq!(classify_skip_cause("unsupported layer type heatmap"), "unsupported layer type heatmap");
        assert_eq!(classify_skip_cause("no \"fill-color\" set"), "no fill-color (pattern fills unsupported)");
        assert_eq!(classify_skip_cause("something nobody has seen before"), "other: something nobody has seen before");
    }

    #[test]
    fn the_pan_drift_is_the_census_one() {
        // demos/us_block_groups.rs: lon = START.0 + 12.0 * (t * 0.2).sin()
        for t in [0.0, 1.0, 7.5, 25.0] {
            assert_eq!(pan_lon(-98.35, t), -98.35 + 12.0 * (t * 0.2f64).sin());
        }
    }

    #[test]
    fn assets_resolve_from_any_working_directory() {
        for rel in ["examples/liberty-style.json", "examples/satellite-style.json", "demos/positron-style.json"] {
            assert!(asset(rel).is_file(), "{rel}");
        }
        // The Mapbox-authored styles are not in the open library tree
        // (`scripts/split-manifest.txt` marks them X); where the checkout has
        // one, it resolves like any other asset.
        let rel = "examples/mapbox-streets-style.json";
        if repo_root().join(rel).is_file() {
            assert!(asset(rel).is_file(), "{rel}");
        } else {
            eprintln!("skipped: {rel} is a Mapbox fixture and this checkout has none");
        }
        assert!(compile_style("examples/liberty-style.json").is_ok());
        assert!(compile_style("examples/no-such-style.json").is_err(), "a missing style is an Err, not a panic");
    }

    /// A throwaway `$HOME` with `mapbox.env` written at `mode`, or no file.
    fn temp_home(contents: Option<(&str, u32)>) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("temp HOME");
        if let Some((text, mode)) = contents {
            let path = home.path().join(MAPBOX_ENV_FILE_RELATIVE);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        home
    }

    const SECRET: &str = "pk.test-value-that-must-never-print";

    /// Mapbox is off by default: a token in the environment AND in a
    /// private `mapbox.env` is still no token without the opt-in.
    #[test]
    fn a_token_without_the_opt_in_is_no_token() {
        let home = temp_home(Some(("MAPBOX_ACCESS_TOKEN=pk.from-file\n", 0o600)));
        assert_eq!(mapbox_token_gated(false, Some("pk.from-env".into()), Some(home.path())), Ok(None));
        assert_eq!(mapbox_token_gated(false, None, Some(home.path())), Ok(None));
        // The public entry point, against the real environment: the gates
        // never export the opt-in, so a token there is ignored.
        if !mapbox_allowed() {
            assert_eq!(mapbox_token(Some("pk.from-env".into()), Some(home.path())), Ok(None));
        }
        assert!(!mapbox_allowed_value(None));
        assert!(!mapbox_allowed_value(Some("0")));
        assert!(!mapbox_allowed_value(Some("true")));
        assert!(mapbox_allowed_value(Some("1")));
        assert!(mapbox_allowed_value(Some(" 1\n")));
    }

    #[test]
    fn a_mapbox_requirement_without_the_opt_in_falls_back_or_says_it_is_off() {
        let token_only = |k: &str| (k == "MAPBOX_ACCESS_TOKEN").then(|| "pk.SECRET".to_string());
        let keyless = Requirement::MapboxToken { fallback: Some("OpenFreeMap Positron") };
        match check(&keyless, &token_only, &|_| false) {
            Status::Fallback { why } => assert!(why.contains("turned off") && why.contains(MAPBOX_ALLOW_VAR), "{why}"),
            other => panic!("a keyless page falls back: {other:?}"),
        }
        match check(&Requirement::MapboxToken { fallback: None }, &token_only, &|_| false) {
            Status::Missing { why } => assert!(why.contains("turned off") && !why.contains("SECRET"), "{why}"),
            other => panic!("a Mapbox-only page shows its missing state: {other:?}"),
        }
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let home = temp_home(Some(("MAPBOX_ACCESS_TOKEN=pk.from-file\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, Some("pk.from-env".into()), Some(home.path())), Ok(Some("pk.from-env".into())));
    }

    #[test]
    fn a_blank_environment_value_falls_through_to_a_private_file() {
        let home = temp_home(Some(("# comment\nexport MAPBOX_ACCESS_TOKEN=\"pk.from-file\"\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, Some("  ".into()), Some(home.path())), Ok(Some("pk.from-file".into())));
        assert_eq!(mapbox_token_gated(true, None, Some(home.path())), Ok(Some("pk.from-file".into())));
        let crlf = temp_home(Some(("MAPBOX_ACCESS_TOKEN=pk.crlf\r\nOTHER=1\r\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, None, Some(crlf.path())), Ok(Some("pk.crlf".into())), "CRLF line ends");
        let single = temp_home(Some(("MAPBOX_ACCESS_TOKEN='pk.single'\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, None, Some(single.path())), Ok(Some("pk.single".into())), "single quotes");
    }

    #[test]
    fn the_last_assignment_in_the_file_wins_like_a_shell() {
        let home = temp_home(Some(("MAPBOX_ACCESS_TOKEN=pk.first\nMAPBOX_ACCESS_TOKEN=pk.second\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, None, Some(home.path())), Ok(Some("pk.second".into())));
    }

    #[test]
    fn no_environment_and_no_file_is_no_token_not_an_error() {
        let home = temp_home(None);
        assert_eq!(mapbox_token_gated(true, None, Some(home.path())), Ok(None));
        assert_eq!(mapbox_token_gated(true, None, None), Ok(None));
        let other = temp_home(Some(("SOMETHING_ELSE=1\n", 0o600)));
        assert_eq!(mapbox_token_gated(true, None, Some(other.path())), Ok(None));
    }

    /// A file that is there but cannot be checked (EACCES on its directory)
    /// is reported, not read as "no token".
    #[cfg(unix)]
    #[test]
    fn a_token_file_that_cannot_be_checked_is_an_error_not_no_token() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = temp_home(Some((&format!("MAPBOX_ACCESS_TOKEN={SECRET}\n"), 0o600)));
        let dir = home.path().join(MAPBOX_ENV_FILE_RELATIVE).parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = mapbox_token_gated(true, None, Some(home.path()));
        // Restored BEFORE asserting, so the temp dir can always be removed.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if result.as_ref().is_ok_and(Option::is_some) {
            return; // running as root, which reads through mode 000: nothing to observe
        }
        let err = result.expect_err("an unreadable token file is reported");
        assert!(err.contains("could not check") && err.contains("mapbox.env"), "{err}");
        assert!(!err.contains(SECRET) && !err.contains("pk."), "never the value: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_group_or_world_readable_file_is_refused_without_printing_the_value() {
        for mode in [0o644, 0o640, 0o604] {
            let home = temp_home(Some((&format!("MAPBOX_ACCESS_TOKEN={SECRET}\n"), mode)));
            let err = mapbox_token_gated(true, None, Some(home.path())).expect_err("a readable token file is refused");
            assert!(err.contains("chmod 600") && err.contains("mapbox.env"), "says why and how: {err}");
            assert!(!err.contains(SECRET) && !err.contains("pk."), "never the value: {err}");
        }
    }

    #[test]
    fn a_path_under_home_is_shown_with_a_tilde() {
        let home = std::path::Path::new("/Users/someone");
        assert_eq!(tilde_in(std::path::Path::new("/Users/someone/.cache/x.pmtiles"), Some(home)), "~/.cache/x.pmtiles");
        assert_eq!(tilde_in(home, Some(home)), "~");
        assert_eq!(tilde_in(std::path::Path::new("/Users/someoneelse/x"), Some(home)), "/Users/someoneelse/x", "a prefix of a name is not the home");
        assert_eq!(tilde_in(std::path::Path::new("/opt/x"), Some(home)), "/opt/x");
        assert_eq!(tilde_in(std::path::Path::new("/opt/x"), None), "/opt/x");
    }

    #[test]
    fn requirement_checks_are_pure_and_name_the_fix() {
        let no_env = |_: &str| None;
        let with_token = |k: &str| match k {
            "MAPBOX_ACCESS_TOKEN" => Some("pk.x".to_string()),
            MAPBOX_ALLOW_VAR => Some("1".to_string()),
            _ => None,
        };
        let nothing_exists = |_: &std::path::Path| false;
        let everything_exists = |_: &std::path::Path| true;

        let token = Requirement::MapboxToken { fallback: None };
        assert!(matches!(check(&token, &no_env, &nothing_exists), Status::Missing { .. }));
        assert!(matches!(check(&token, &with_token, &nothing_exists), Status::Met));
        let keyless = Requirement::MapboxToken { fallback: Some("Positron") };
        assert!(matches!(check(&keyless, &no_env, &nothing_exists), Status::Fallback { .. }));

        let file = Requirement::File {
            path: "/x/E2025_30ss.pmtiles".into(),
            what: "GHSL archive",
            fetch: "scripts/fetch-ghsl.sh 2025 30ss",
            size: "~461 MB",
            fallback: Some("synthetic world"),
            then: None,
        };
        assert!(matches!(check(&file, &no_env, &everything_exists), Status::Met));
        match check(&file, &no_env, &nothing_exists) {
            Status::Fallback { why } => assert!(why.contains("GHSL archive")),
            other => panic!("{other:?}"),
        }
        let pm = Requirement::EnvPath { var: "PMTILES_PATH", how: "docs/OFFLINE.md" };
        assert!(matches!(check(&pm, &no_env, &everything_exists), Status::Missing { .. }));
        assert!(matches!(
            check(&pm, &|k| (k == "PMTILES_PATH").then(|| "/a.pmtiles".into()), &everything_exists),
            Status::Met
        ));
        assert!(matches!(check(&Requirement::Network, &no_env, &nothing_exists), Status::Met));
    }

    #[test]
    fn the_clickhouse_probe_pings_before_the_query_and_never_shows_a_credential() {
        assert_eq!(ping_url("http://localhost:8123"), "http://localhost:8123/ping");
        assert_eq!(ping_url("http://localhost:8123/"), "http://localhost:8123/ping");
        assert_eq!(ping_url("http://h:8123?password=x"), "http://h:8123/ping?password=x");
        let Status::Missing { why } = clickhouse_unreachable("Local ClickHouse", "http://admin:SECRET@h:8123?password=SECRET", "scripts/load-ghsl-epochs.sh")
        else {
            panic!("an unreachable server blocks")
        };
        assert!(!why.contains("SECRET"), "{why}");
        assert!(why.contains("h:8123") && !why.contains("scripts/"), "no script for the user to run: {why}");
        assert!(why.contains("\"Local ClickHouse\"") && why.contains("Connections page"), "{why}");
    }

    #[test]
    fn a_clickhouse_page_without_its_connection_points_at_the_connections_page() {
        let req = Requirement::ClickHouse { connection: "Local ClickHouse".into(), url: None, load: "scripts/load-ghsl-epochs.sh" };
        let Status::Missing { why } = check(&req, &|_| None, &|_| true) else { panic!("no connection blocks") };
        assert!(why.contains("\"Local ClickHouse\"") && why.contains("Connections page") && !why.contains("http"), "{why}");
        let named = Requirement::ClickHouse { connection: "Local ClickHouse".into(), url: Some("http://h:8123".into()), load: "x" };
        assert!(matches!(check(&named, &|_| None, &|_| true), Status::Met), "probed later, Met until then");
        let e = ClickHouseEndpoint { connection: "c".into(), url: "http://h:8123".into(), headers: vec![("X-ClickHouse-Key".into(), "SECRET".into())] };
        assert!(!format!("{e:?}").contains("SECRET"));
    }

    /// Nothing listens on port 9: the probe says no, well inside a second.
    #[test]
    fn a_closed_port_fails_the_clickhouse_probe_quickly() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = Instant::now();
        let up = rt.block_on(probe_clickhouse("http://127.0.0.1:9".to_string(), rt.handle().clone()));
        assert!(!up);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// [`probe_availability`]'s whole classification, against a stub
    /// server rather than the network: 2xx is available, everything else
    /// (401/403/404, and no server at all) is not. No real request ever
    /// leaves this process.
    #[tokio::test]
    async fn probe_availability_classifies_status_codes() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Handle::current();
        for status in [200, 204] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
            assert!(probe_availability(server.uri(), rt.clone()).await, "{status} should be available");
        }
        for status in [401, 403, 404, 500] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
            assert!(!probe_availability(server.uri(), rt.clone()).await, "{status} should not be available");
        }
        // Nothing listening at all: a network error, same as any non-2xx.
        assert!(!probe_availability("http://127.0.0.1:9".to_string(), rt.clone()).await);
    }

    /// [`resolve_availability`]: `Always` never calls out; a `Probe` with
    /// nothing to call (no token) is unavailable without one either.
    #[test]
    fn resolve_availability_is_always_true_for_always_and_skips_the_network_with_no_url() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let env = PanelEnv {
            host: Host::Gallery,
            runtime: rt.handle().clone(),
            mapbox_token: None,
            viewport: (100.0, 100.0),
            cancel: Default::default(),
            clickhouse: None,
        };
        assert!(rt.block_on(resolve_availability(Availability::Always, &env)));
        assert!(!rt.block_on(resolve_availability(Availability::Probe(|_| ProbeTarget::Unavailable), &env)));
        assert!(rt.block_on(resolve_availability(Availability::Probe(|_| ProbeTarget::Keyless), &env)));
    }

    #[test]
    fn a_token_never_appears_in_a_status_message() {
        let env = |k: &str| (k == "MAPBOX_ACCESS_TOKEN").then(|| "pk.SECRET".to_string());
        for req in [Requirement::MapboxToken { fallback: None }, Requirement::MapboxToken { fallback: Some("Positron") }]
        {
            let text = format!("{:?}", check(&req, &env, &|_| false));
            assert!(!text.contains("SECRET"), "{text}");
        }
    }
}
