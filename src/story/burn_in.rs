//! What a story burns into its frames besides the map, and whether the
//! result may be published (map-stories spec §5.4).
//!
//! The drawing itself is the capture's chrome pass
//! (`capture::session::encode_frame`, fed `CaptureRequest::captions`,
//! `attribution` and `brand_mark`): this module decides the TEXT of the
//! credit and the licence verdict the manifest carries, and refuses an
//! output path inside a git work tree.
//!
//! # The licence table (spec §5.4)
//!
//! | Source | Credit | Verdict |
//! |---|---|---|
//! | OpenFreeMap (`openfreemap`, or a URL on `openfreemap.org`) | [`OFM_CREDIT`] | `true` |
//! | Protomaps (a URL on `*.protomaps.com`, or a PMTiles archive whose stated credit names Protomaps) | [`PROTOMAPS_CREDIT`] | `true` |
//! | a PMTiles archive whose stated credit names OpenStreetMap (ODbL) | its stated credit | `true` |
//! | Mapbox vector (by URL host: `api.mapbox.com`, `*.tiles.mapbox.com`, `mapbox://`) | [`MAPBOX_CREDIT`] | `false`, "Mapbox wordmark not drawn", unless a wordmark is drawn |
//! | Mapbox satellite | [`MAPBOX_SATELLITE_CREDIT`] | as Mapbox vector |
//! | the fixture tile (OpenMapTiles-schema OSM) | its stated credit | `true` |
//! | any other source (a `tiles` URL, a PMTiles archive) | its stated credit | `false`: "unknown licence", unless the script's `output.acknowledge` names it (`"source-licence:<source id or host>"`: the author holds the rights), then `true` |
//! | no credit at all | -- | `false`: a story always credits its data (R11) |
//! | Sentinel-2 cloudless 2020 | [`S2CLOUDLESS_CREDIT`] | `"non-commercial only"` |
//! | Sentinel-2 cloudless 2016 (`s2cloudless-2016`, the gallery's imagery) | [`S2CLOUDLESS_2016_CREDIT`] | `true` (CC BY 4.0) |
//! | Natural Earth | [`NATURAL_EARTH_CREDIT`] | `true` |
//! | GHSL dots | [`GHSL_CREDIT`] | `true` |
//! | script `credits` | verbatim | unchanged |
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::path::Path;

/// OpenFreeMap's credit: `globe_capture`'s `ATTRIBUTION`, HTML stripped.
pub const OFM_CREDIT: &str = "OpenFreeMap © OpenMapTiles Data from OpenStreetMap";
/// The Protomaps basemap's credit: Protomaps' own "Protomaps ©
/// OpenStreetMap" (docs.protomaps.com/basemaps/maplibre, read
/// 2026-09-30), with OpenStreetMap's "contributors" wording that the
/// ODbL attribution guideline asks for where the credit cannot be a
/// link, as in a video frame.
pub const PROTOMAPS_CREDIT: &str = "Protomaps © OpenStreetMap contributors";
/// Mapbox's credit for its vector tiles.
pub const MAPBOX_CREDIT: &str = "© Mapbox © OpenStreetMap";
/// Mapbox Satellite's credit: the vector credit plus the imagery's
/// supplier, as Mapbox's attribution guide requires.
pub const MAPBOX_SATELLITE_CREDIT: &str = "© Mapbox © OpenStreetMap © Maxar";
/// EOX's Sentinel-2 cloudless 2020 mosaic (CC BY-NC-SA 4.0).
pub const S2CLOUDLESS_CREDIT: &str =
    "Sentinel-2 cloudless 2020 by EOX IT Services GmbH (Contains modified Copernicus Sentinel data 2020)";
/// EOX's Sentinel-2 cloudless 2016 mosaic (`s2cloudless_3857`, CC BY
/// 4.0): the only EOX layer the gallery draws (`kit::S2CLOUDLESS_2016_URL`),
/// credited as its source's display text.
pub const S2CLOUDLESS_2016_CREDIT: &str =
    "EOxCloudless https://cloudless.eox.at by EOX IT Services GmbH (Contains modified Copernicus Sentinel data 2016 & 2017)";
/// Natural Earth: public domain, credited anyway.
pub const NATURAL_EARTH_CREDIT: &str = "Made with Natural Earth";
/// The GHSL population grid behind the dot layers (CC BY 4.0).
pub const GHSL_CREDIT: &str = "GHS-POP R2023A, European Commission JRC";

/// The reason a Mapbox scene is not publishable without its wordmark.
pub const MAPBOX_WORDMARK_REASON: &str = "Mapbox wordmark not drawn";

/// Whether a render may be published: `true`, `false` (with a reason on
/// the [`Verdict`]), or `"non-commercial only"`. Serialised untagged, so
/// the manifest reads `true`, `false` or the string.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum Publishable {
    Yes(bool),
    NonCommercial(&'static str),
}

/// The licence verdict and the credit line for one scene.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub publishable: Publishable,
    /// Why not publishable (or why only non-commercially); `None` when it is.
    pub reason: Option<String>,
    /// Every credit, deduplicated, joined with `" | "`: the vector source,
    /// then the raster, then the dots, then `credits`, then `layer_credits`.
    pub attribution: String,
    /// The `output.acknowledge` entry that made an unknown source
    /// publishable (the author confirmed they hold the rights), for the
    /// manifest's `licence_acknowledged`. `None` otherwise.
    pub acknowledged: Option<String>,
}

/// What a raster named by `scene.raster` owes: its credit, and whether it
/// is non-commercial or needs the Mapbox wordmark.
enum RasterTerms {
    Open(&'static str),
    NonCommercial(&'static str),
    Mapbox,
    Unknown,
}

fn raster_terms(raster: &str) -> RasterTerms {
    let name = raster.to_ascii_lowercase();
    // The 2016 layer first: it is CC BY 4.0, unlike every later year.
    if name.starts_with("s2cloudless-2016") || name.starts_with("eoxcloudless") {
        RasterTerms::Open(S2CLOUDLESS_2016_CREDIT)
    } else if name.starts_with("s2cloudless") || name.starts_with("sentinel-2") {
        RasterTerms::NonCommercial(S2CLOUDLESS_CREDIT)
    } else if name.starts_with("mapbox") {
        RasterTerms::Mapbox
    } else if name.replace(['-', '_', ' '], "").starts_with("naturalearth") {
        RasterTerms::Open(NATURAL_EARTH_CREDIT)
    } else {
        RasterTerms::Unknown
    }
}

/// The licence-table class of a vector source, from its manifest label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceClass {
    /// Mapbox's tiles, however named: the `mapbox` source's
    /// `mapbox:<ids>` label, or any URL on a Mapbox host.
    Mapbox,
    OpenFreeMap,
    /// The Protomaps basemap (OpenStreetMap data, ODbL).
    Protomaps,
    /// A PMTiles archive whose own stated credit names OpenStreetMap:
    /// ODbL data, publishable with that credit.
    OsmArchive,
    /// The vendored test tile (OpenMapTiles-schema OSM, credited).
    Fixture,
    /// Not in the licence table: its terms are the author's to check.
    Unknown,
}

pub use crate::story::script::is_mapbox_url;

/// The host of a URL label (a `tiles` URL, `pmtiles:<url>`), lowercased,
/// with no userinfo or port; `None` for a label that is not a URL.
#[must_use]
pub fn label_host(label: &str) -> Option<String> {
    let label = label.trim();
    let url = label.strip_prefix("pmtiles:").unwrap_or(label);
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_port.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn on_domain(host: Option<&str>, domain: &str) -> bool {
    host.is_some_and(|h| h == domain || h.strip_suffix(domain).is_some_and(|p| p.ends_with('.')))
}

/// [`SourceClass`] of a manifest source label (`"openfreemap"`,
/// `"mapbox:<ids>"`, `"fixture"`, `"pmtiles:<url or path>"`, or a `tiles`
/// URL) and the source's own stated credit (`TileSource::attribution`,
/// reduced to text; a PMTiles archive's metadata `attribution`). The host
/// decides for a URL; for a PMTiles archive not on a known host, the
/// stated credit does.
#[must_use]
pub fn source_class(source_label: &str, stated: Option<&str>) -> SourceClass {
    let host = label_host(source_label);
    let stated = stated.unwrap_or("").to_ascii_lowercase();
    if source_label == "mapbox" || source_label.starts_with("mapbox:") || is_mapbox_url(source_label) {
        SourceClass::Mapbox
    } else if source_label == "openfreemap" || on_domain(host.as_deref(), "openfreemap.org") {
        SourceClass::OpenFreeMap
    } else if on_domain(host.as_deref(), "protomaps.com") {
        SourceClass::Protomaps
    } else if source_label == "fixture" {
        SourceClass::Fixture
    } else if source_label.starts_with("pmtiles:") && stated.contains("protomaps") {
        SourceClass::Protomaps
    } else if source_label.starts_with("pmtiles:") && stated.contains("openstreetmap") {
        SourceClass::OsmArchive
    } else {
        SourceClass::Unknown
    }
}

/// The id an `output.acknowledge: ["source-licence:<id>"]` entry names
/// `source_label` by, as the unknown-licence hint prints it: a URL's host
/// (`tiles.example.com`); a local archive's file name
/// (`pmtiles:city.pmtiles`), which survives the manifest's path scrubbing
/// (a local path is never recorded) and which [`acknowledgement_for`]
/// matches; else the label itself.
#[must_use]
pub fn acknowledgement_id(source_label: &str) -> String {
    if let Some(host) = label_host(source_label) {
        return host;
    }
    if let Some(name) = local_archive_name(source_label) {
        return format!("pmtiles:{name}");
    }
    source_label.to_string()
}

/// A local `pmtiles:<path>` archive's file name; `None` for a URL or any
/// other label.
fn local_archive_name(source_label: &str) -> Option<String> {
    let path = source_label.strip_prefix("pmtiles:")?;
    if path.contains("://") {
        return None;
    }
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned())
}

/// The `output.acknowledge` entry naming `source_label` -- by the label
/// itself, its URL's host, or (a local archive) its
/// [`acknowledgement_id`] -- if any.
#[must_use]
pub fn acknowledgement_for(source_label: &str, acknowledge: &[String]) -> Option<String> {
    let host = label_host(source_label);
    let id = acknowledgement_id(source_label);
    acknowledge
        .iter()
        .find(|a| {
            a.strip_prefix("source-licence:").map(str::trim).is_some_and(|named| {
                !named.is_empty() && (named == source_label || named == id || host.as_deref() == Some(&*named.to_ascii_lowercase()))
            })
        })
        .cloned()
}

/// The verdict for a scene (spec §5.4, exactly): `source_label` is the
/// manifest's name for the vector source (`"openfreemap"`,
/// `"mapbox:<ids>"`, `"fixture"` ...), `raster` the scene's raster name,
/// `dots` whether a GHSL dot layer draws, `credits` the script's (and the
/// loader's) credits verbatim, `layer_credits` each `StoryLayer::credit`,
/// and `wordmark` the user-supplied Mapbox wordmark PNG **that is drawn**
/// (pass `None` when it is not, whatever the script names).
pub fn verdict(
    source_label: &str,
    raster: Option<&str>,
    dots: bool,
    credits: &[String],
    layer_credits: &[String],
    wordmark: Option<&Path>,
) -> Verdict {
    verdict_with(&VerdictSource { label: source_label, stated: None, acknowledge: &[] }, raster, dots, credits, layer_credits, wordmark)
}

/// The vector source as [`verdict_with`] judges it: its manifest label,
/// its own stated credit (which also joins the credit line when the
/// caller lists it in `credits`), and the script's `output.acknowledge`.
pub struct VerdictSource<'a> {
    pub label: &'a str,
    pub stated: Option<&'a str>,
    pub acknowledge: &'a [String],
}

/// [`verdict`] with the source's stated credit (so a PMTiles archive can
/// be classed by it) and the script's acknowledgements.
pub fn verdict_with(
    source: &VerdictSource<'_>,
    raster: Option<&str>,
    dots: bool,
    credits: &[String],
    layer_credits: &[String],
    wordmark: Option<&Path>,
) -> Verdict {
    let source_label = source.label;
    let mut ordered: Vec<String> = Vec::new();
    let mut needs_wordmark = false;
    let mut non_commercial = false;
    let class = source_class(source_label, source.stated);
    match class {
        SourceClass::Mapbox => {
            ordered.push(MAPBOX_CREDIT.to_string());
            needs_wordmark = true;
        }
        SourceClass::OpenFreeMap => ordered.push(OFM_CREDIT.to_string()),
        SourceClass::Protomaps => ordered.push(PROTOMAPS_CREDIT.to_string()),
        SourceClass::OsmArchive | SourceClass::Fixture | SourceClass::Unknown => {}
    }
    match raster.map(raster_terms) {
        Some(RasterTerms::Open(credit)) => ordered.push(credit.to_string()),
        Some(RasterTerms::NonCommercial(credit)) => {
            ordered.push(credit.to_string());
            non_commercial = true;
        }
        Some(RasterTerms::Mapbox) => {
            ordered.push(MAPBOX_SATELLITE_CREDIT.to_string());
            needs_wordmark = true;
        }
        Some(RasterTerms::Unknown) | None => {}
    }
    if dots {
        ordered.push(GHSL_CREDIT.to_string());
    }
    ordered.extend(credits.iter().cloned());
    ordered.extend(layer_credits.iter().cloned());
    // The engine's one collection rule (`attribution::collect`, as
    // `MapView::attributions` uses): deduplicated, a credit already stated
    // inside a longer one (normalised: "(c) OpenStreetMap" inside "(c)
    // OpenStreetMap contributors") is not repeated, and a longer credit
    // that states an earlier one whole takes its place (the satellite
    // credit over the vector one). One role for all: the order above is
    // the story's own.
    use crate::attribution::{Credit, CreditRole};
    let seen = crate::attribution::collect(ordered.into_iter().map(|c| Credit::new(CreditRole::Host, c)));
    let attribution = crate::attribution::join(&seen).unwrap_or_default();
    let acknowledged =
        (class == SourceClass::Unknown && !attribution.is_empty()).then(|| acknowledgement_for(source_label, source.acknowledge)).flatten();
    let (publishable, reason) = if needs_wordmark && wordmark.is_none() {
        (Publishable::Yes(false), Some(MAPBOX_WORDMARK_REASON.to_string()))
    } else if attribution.is_empty() {
        (
            Publishable::Yes(false),
            Some(format!("no credit: {source_label} states none and the script names none (a story always credits its data)")),
        )
    } else if class == SourceClass::Unknown && acknowledged.is_none() {
        let id = acknowledgement_id(source_label);
        (
            Publishable::Yes(false),
            Some(format!(
                "unknown licence: {source_label} is not in the licence table; check its terms, and if you hold the rights add \
                 output.acknowledge: [\"source-licence:{id}\"]"
            )),
        )
    } else if non_commercial {
        (
            Publishable::NonCommercial("non-commercial only"),
            Some("Sentinel-2 cloudless 2020 is CC BY-NC-SA 4.0 (non-commercial, share-alike)".to_string()),
        )
    } else {
        (Publishable::Yes(true), None)
    };
    Verdict { publishable, reason, attribution, acknowledged }
}

/// The `output.acknowledge` entry with which the author states that their
/// agreement with Mapbox permits map content in video (spec §6, R6).
pub const MAPBOX_VIDEO_ACK: &str = "mapbox-video";
/// Why a Mapbox basemap in a video needs [`MAPBOX_VIDEO_ACK`].
pub const MAPBOX_VIDEO_REASON: &str = "Mapbox's terms require that your agreement permits map content in video";

/// One publish-table row's verdict (spec §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowVerdict {
    Ok,
    /// Publishable once the author adds the `output.acknowledge` entry the
    /// reason names.
    NeedsAcknowledgement,
    /// Not publishable whatever is acknowledged.
    Blocked,
}

/// One source of a render and what publishing it owes (spec §6).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PublishRow {
    /// `"basemap"` | `"raster"` | `"overlay:<id>"` | `"image:<src>"` |
    /// `"voice"` | `"music"` (credentials redacted).
    pub source: String,
    /// The credit this source owes (credentials redacted); `None` when it
    /// states none.
    pub credit: Option<String>,
    pub verdict: RowVerdict,
    /// Why the row is not `Ok`; `None` when it is.
    pub reason: Option<String>,
    /// The `output.acknowledge` entry that made this row `Ok`, recorded
    /// with the render.
    pub acknowledged: Option<String>,
}

/// What [`publish_table`] judges.
pub struct PublishInputs<'a> {
    /// The vector basemap, its stated credit and the script's
    /// acknowledgements (which every row reads).
    pub basemap: &'a VerdictSource<'a>,
    /// Credits that name the basemap besides its stated one (the loader's
    /// and the script's `scene.credits`): an archive that states no credit
    /// is credited by the author here, as the frame's plate shows.
    pub credits: &'a [String],
    /// `scene.raster` when it is drawn (the player draws none yet).
    pub raster: Option<&'a str>,
    /// The output is a video (more than one frame), not a still.
    pub video: bool,
    /// A basemap wordmark is loaded and drawn on every frame.
    pub wordmark_drawn: bool,
    /// `(overlay id, its attribution)` per overlay.
    pub overlays: &'a [(String, Option<String>)],
    /// `(image src, its credit)` per drawn image.
    pub images: &'a [(String, Option<String>)],
    /// `(layer id, its credit)` per story layer plugin
    /// (`StoryLayer::credit`), credited on the plate as the table rows them.
    pub layers: &'a [(String, Option<String>)],
    /// The style draws Mapbox's sprite (`sprite` is a `mapbox://` or Mapbox
    /// URL): Mapbox's icons owe Mapbox's terms under any basemap.
    pub mapbox_sprite: bool,
}

/// Whether `style` names a Mapbox-hosted sprite sheet: its `sprite` is a
/// `mapbox://` or Mapbox-host URL, or (the multi-sprite form) any entry's
/// `url` is.
#[must_use]
pub fn style_has_mapbox_sprite(style: &serde_json::Value) -> bool {
    match style.get("sprite") {
        Some(serde_json::Value::String(url)) => is_mapbox_url(url),
        Some(serde_json::Value::Array(sprites)) => {
            sprites.iter().any(|s| s.get("url").and_then(serde_json::Value::as_str).is_some_and(is_mapbox_url))
        }
        _ => false,
    }
}

fn row(source: String, credit: Option<String>, verdict: RowVerdict, reason: Option<String>, acknowledged: Option<String>) -> PublishRow {
    let clean = |s: String| crate::story::redact_url_secrets(s.trim());
    PublishRow {
        source: crate::story::redact_url_secrets(&source),
        credit: credit.map(clean).filter(|c| !c.is_empty()),
        verdict,
        reason: reason.map(clean),
        acknowledged,
    }
}

/// A Mapbox source's row once its wordmark is drawn: `Ok` for a still;
/// in a video [`RowVerdict::NeedsAcknowledgement`] ([`MAPBOX_VIDEO_REASON`])
/// until `acknowledge` holds [`MAPBOX_VIDEO_ACK`], then `Ok` with it
/// recorded. Without the wordmark: [`RowVerdict::Blocked`]
/// ([`MAPBOX_WORDMARK_REASON`]), whatever is acknowledged.
fn mapbox_row(source: &str, credit: String, inputs: &PublishInputs<'_>) -> PublishRow {
    let credit = Some(credit);
    if !inputs.wordmark_drawn {
        return row(source.into(), credit, RowVerdict::Blocked, Some(MAPBOX_WORDMARK_REASON.into()), None);
    }
    if !inputs.video {
        return row(source.into(), credit, RowVerdict::Ok, None, None);
    }
    match inputs.basemap.acknowledge.iter().find(|a| a.trim() == MAPBOX_VIDEO_ACK) {
        Some(_) => row(source.into(), credit, RowVerdict::Ok, None, Some(MAPBOX_VIDEO_ACK.into())),
        None => row(
            source.into(),
            credit,
            RowVerdict::NeedsAcknowledgement,
            Some(MAPBOX_VIDEO_REASON.into()),
            None,
        ),
    }
}

/// One row per source (spec §6), in this order: the basemap, the raster
/// (when drawn), the Mapbox sprite (under a non-Mapbox basemap), each
/// story layer plugin, each overlay, each image (each distinct
/// `(src, credit)` once).
///
/// - **Basemap:** the [`verdict_with`] rules. Mapbox: see [`mapbox_row`]'s
///   rules (wordmark, then the video acknowledgement). A source outside
///   the licence table with a credit is `NeedsAcknowledgement` until
///   `output.acknowledge` names it (`"source-licence:<id>"`); no credit at
///   all is `Blocked`.
/// - **Raster:** an open layer is `Ok` with its credit; Sentinel-2
///   cloudless 2020 is `Blocked` as `"non-commercial only: ..."`; Mapbox
///   Satellite as the Mapbox basemap; any other `NeedsAcknowledgement`
///   until `"source-licence:<raster>"` is acknowledged, and then `Blocked`
///   unless the script's credits name it (as an unknown basemap).
/// - **Sprite:** a Mapbox sprite under another basemap: the Mapbox rules.
/// - **Layer:** `Ok` with its credit; without one `Blocked` unless
///   `"source-licence:layer:<id>"` is acknowledged.
/// - **Overlay:** `Ok`, crediting its attribution, or nothing when it
///   states none (F16: only images require a credit).
/// - **Image:** `Ok` with its credit; `Blocked` "image <src> has no credit"
///   without one (R7).
///
/// Every source, credit and reason is passed through
/// [`crate::story::redact_url_secrets`].
#[must_use]
pub fn publish_table(inputs: &PublishInputs<'_>) -> Vec<PublishRow> {
    let mut rows = Vec::new();
    let source = inputs.basemap;
    let class = source_class(source.label, source.stated);
    let wordmark = inputs.wordmark_drawn.then_some(Path::new("wordmark"));
    let v = verdict_with(source, None, false, inputs.credits, &[], wordmark);
    let credit = (!v.attribution.is_empty()).then(|| v.attribution.clone());
    rows.push(if class == SourceClass::Mapbox {
        mapbox_row("basemap", v.attribution, inputs)
    } else if credit.is_none() {
        row("basemap".into(), None, RowVerdict::Blocked, v.reason, None)
    } else if class == SourceClass::Unknown && v.acknowledged.is_none() {
        row("basemap".into(), credit, RowVerdict::NeedsAcknowledgement, v.reason, None)
    } else {
        row("basemap".into(), credit, RowVerdict::Ok, None, v.acknowledged)
    });
    if let Some(raster) = inputs.raster {
        rows.push(match raster_terms(raster) {
            RasterTerms::Open(c) => row("raster".into(), Some(c.into()), RowVerdict::Ok, None, None),
            RasterTerms::NonCommercial(c) => row(
                "raster".into(),
                Some(c.into()),
                RowVerdict::Blocked,
                Some("non-commercial only: Sentinel-2 cloudless 2020 is CC BY-NC-SA 4.0 (non-commercial, share-alike)".into()),
                None,
            ),
            RasterTerms::Mapbox => mapbox_row("raster", MAPBOX_SATELLITE_CREDIT.into(), inputs),
            // As the basemap: an acknowledged unknown raster still owes a
            // credit (R11), which only the script's credits can give.
            RasterTerms::Unknown => match acknowledgement_for(raster, source.acknowledge) {
                Some(ack) => {
                    let credit: Vec<&str> = inputs.credits.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).collect();
                    if credit.is_empty() {
                        row(
                            "raster".into(),
                            None,
                            RowVerdict::Blocked,
                            Some(format!("no credit: raster `{raster}` states none and the script names none (a story always credits its data)")),
                            None,
                        )
                    } else {
                        row("raster".into(), Some(credit.join(" | ")), RowVerdict::Ok, None, Some(ack))
                    }
                }
                None => row(
                    "raster".into(),
                    None,
                    RowVerdict::NeedsAcknowledgement,
                    Some(format!(
                        "unknown licence: raster `{raster}` is not in the licence table; check its terms, and if you hold the \
                         rights add output.acknowledge: [\"source-licence:{raster}\"]"
                    )),
                    None,
                ),
            },
        });
    }
    if inputs.mapbox_sprite && class != SourceClass::Mapbox {
        rows.push(mapbox_row("sprite", MAPBOX_CREDIT.into(), inputs));
    }
    for (id, credit) in inputs.layers {
        let credit = credit.as_deref().map(str::trim).filter(|c| !c.is_empty());
        rows.push(match (credit, acknowledgement_for(&format!("layer:{id}"), source.acknowledge)) {
            (Some(c), _) => row(format!("layer:{id}"), Some(c.into()), RowVerdict::Ok, None, None),
            (None, Some(ack)) => row(format!("layer:{id}"), None, RowVerdict::Ok, None, Some(ack)),
            (None, None) => row(
                format!("layer:{id}"),
                None,
                RowVerdict::Blocked,
                Some(format!(
                    "layer {id} states no credit; if its data needs none, add output.acknowledge: [\"source-licence:layer:{id}\"]"
                )),
                None,
            ),
        });
    }
    for (id, attribution) in inputs.overlays {
        rows.push(row(format!("overlay:{id}"), attribution.clone(), RowVerdict::Ok, None, None));
    }
    let mut seen: Vec<(&str, Option<&str>)> = Vec::new();
    for (src, credit) in inputs.images {
        let credit = credit.as_deref().map(str::trim).filter(|c| !c.is_empty());
        if seen.contains(&(src.as_str(), credit)) {
            continue;
        }
        seen.push((src.as_str(), credit));
        rows.push(match credit {
            Some(c) => row(format!("image:{src}"), Some(c.into()), RowVerdict::Ok, None, None),
            None => row(
                format!("image:{src}"),
                None,
                RowVerdict::Blocked,
                Some(format!("image {} has no credit", crate::story::redact_url_secrets(src))),
                None,
            ),
        });
    }
    rows
}

/// Whether a render may be published: every row of its table is `Ok`.
#[must_use]
pub fn publishable(rows: &[PublishRow]) -> bool {
    rows.iter().all(|r| r.verdict == RowVerdict::Ok)
}

/// Why `rows` are not publishable: each row that is not `Ok`, as
/// `"<source>: <reason>"`, joined with `"; "`. `None` when publishable.
#[must_use]
pub fn publishable_reason(rows: &[PublishRow]) -> Option<String> {
    let why: Vec<String> = rows
        .iter()
        .filter(|r| r.verdict != RowVerdict::Ok)
        .map(|r| format!("{}: {}", r.source, r.reason.as_deref().unwrap_or("not publishable")))
        .collect();
    (!why.is_empty()).then(|| why.join("; "))
}

/// Refuse an output path inside a git work tree unless `allow`: rendered
/// frames of licensed tiles are licensed output, and a repository is the
/// easiest place to publish one by accident (the same reason `capture.rs`
/// keeps the Mapbox cache out of the repo).
///
/// Decided on the REAL location, not the spelling: the deepest part of
/// `out` that exists is canonicalised (the output itself usually does not
/// exist yet), so a symlink into a work tree is followed, and the rest of
/// the path is appended. Then `out` itself and every ancestor is checked
/// for a `.git` entry, a directory or a file (a linked worktree's), so an
/// output that IS a repository root is refused too. No `git` binary runs.
///
/// # Errors
/// The path is inside a work tree and `allow` is false, or the current
/// directory cannot be read for a relative `out`.
pub fn refuse_repo_output(out: &Path, allow: bool) -> anyhow::Result<()> {
    if allow {
        return Ok(());
    }
    let absolute = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()?.join(out)
    };
    let real = real_location(&absolute);
    for dir in real.ancestors() {
        if dir.join(".git").exists() {
            anyhow::bail!(
                "refusing to write {} inside the git work tree at {}: rendered frames of licensed tiles are licensed \
                 output; write outside the repository, or pass --allow-repo-output",
                out.display(),
                dir.display()
            );
        }
    }
    Ok(())
}

/// `path` with its deepest existing ancestor canonicalised (symlinks
/// resolved) and the missing tail appended unchanged.
pub(crate) fn real_location(path: &Path) -> std::path::PathBuf {
    let mut tail = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ofm_is_publishable() {
        assert_eq!(verdict("openfreemap", None, false, &[], &[], None).publishable, Publishable::Yes(true));
    }

    #[test]
    fn a_mapbox_scene_without_a_wordmark_is_not_publishable() {
        let v = verdict("mapbox:mapbox.mapbox-streets-v8", None, false, &[], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        assert_eq!(v.reason.as_deref(), Some("Mapbox wordmark not drawn"));
        assert!(v.attribution.contains("© Mapbox © OpenStreetMap"));
        // A drawn wordmark lifts it.
        let with = verdict("mapbox:mapbox.mapbox-streets-v8", None, false, &[], &[], Some(Path::new("wm.png")));
        assert_eq!(with.publishable, Publishable::Yes(true));
        // Mapbox satellite under OpenFreeMap owes it too, and both credits.
        let sat = verdict("openfreemap", Some("mapbox-satellite"), false, &[], &[], None);
        assert_eq!(sat.publishable, Publishable::Yes(false));
        assert_eq!(sat.attribution, format!("{OFM_CREDIT} | {MAPBOX_SATELLITE_CREDIT}"));
        assert!(sat.attribution.contains("© Maxar"), "F6: Mapbox Satellite credits its imagery supplier");
    }

    /// Final review F2: Mapbox is recognised by the URL's HOST, not by a
    /// label prefix: a `tiles` URL on api.mapbox.com (the label the loader
    /// gives it is that URL, redacted) is a Mapbox scene.
    #[test]
    fn a_mapbox_tiles_url_is_mapbox_by_its_host() {
        for label in [
            "https://api.mapbox.com/v4/mapbox.mapbox-streets-v8",
            "https://a.tiles.mapbox.com/v4/mapbox.mapbox-streets-v8",
            "HTTPS://API.MAPBOX.COM:443/v4/x",
            "https://user@api.mapbox.com/v4/x",
            "mapbox://mapbox.mapbox-streets-v8",
            "pmtiles:https://api.mapbox.com/x.pmtiles",
        ] {
            assert_eq!(source_class(label, None), SourceClass::Mapbox, "{label}");
            let v = verdict(label, None, false, &["© Someone".into()], &[], None);
            assert_eq!(v.publishable, Publishable::Yes(false), "{label}");
            assert_eq!(v.reason.as_deref(), Some(MAPBOX_WORDMARK_REASON), "{label}");
            assert!(v.attribution.starts_with(MAPBOX_CREDIT), "{label}: {}", v.attribution);
        }
        // A host that merely mentions Mapbox is not Mapbox.
        for label in ["https://mapbox.com.evil.example/x", "https://notmapbox.com/x", "https://t.example/mapbox/{z}", "mapbox-lookalike"] {
            assert_ne!(source_class(label, None), SourceClass::Mapbox, "{label}");
        }
    }

    /// F2: a source outside the licence table is "unknown" (not
    /// publishable, the reason says why), and a scene with no credit at
    /// all is refused.
    #[test]
    fn an_unknown_source_or_an_empty_credit_is_not_publishable() {
        let v = verdict("https://tiles.example/v1", None, false, &["© Example tiles".into()], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        assert!(v.reason.as_deref().unwrap_or("").contains("unknown licence"), "{:?}", v.reason);
        assert_eq!(v.attribution, "© Example tiles");
        let v = verdict("pmtiles:data/city.pmtiles", None, false, &[], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        assert!(v.reason.as_deref().unwrap_or("").contains("no credit"), "{:?}", v.reason);
        assert!(v.attribution.is_empty());
        // Whitespace is no credit either.
        let v = verdict("fixture", None, false, &["  ".into()], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        assert!(v.reason.as_deref().unwrap_or("").contains("no credit"), "{:?}", v.reason);
        // The table's own sources stay publishable.
        assert_eq!(verdict("fixture", None, false, &["© OpenMapTiles".into()], &[], None).publishable, Publishable::Yes(true));
    }

    /// Follow-up to F2: OpenFreeMap and Protomaps are the first-choice
    /// video basemaps, and both are publishable with their credits.
    #[test]
    fn openfreemap_and_protomaps_are_publishable_with_their_credits() {
        for label in ["openfreemap", "https://tiles.openfreemap.org/planet"] {
            let v = verdict(label, None, false, &[], &[], None);
            assert_eq!(v.publishable, Publishable::Yes(true), "{label}");
            assert_eq!(v.attribution, OFM_CREDIT, "{label}");
        }
        for label in [
            "https://api.protomaps.com/tiles/v4",
            "pmtiles:https://build.protomaps.com/20260901.pmtiles",
            "https://demo-bucket.protomaps.com/v4",
        ] {
            assert_eq!(source_class(label, None), SourceClass::Protomaps, "{label}");
            let v = verdict(label, None, false, &[], &[], None);
            assert_eq!(v.publishable, Publishable::Yes(true), "{label}: {:?}", v.reason);
            assert_eq!(v.attribution, PROTOMAPS_CREDIT, "{label}");
        }
        // A look-alike host is not Protomaps.
        assert_eq!(source_class("https://protomaps.com.evil.example/x", None), SourceClass::Unknown);
        // A local archive: its own metadata credit decides.
        let stated = "Protomaps © OpenStreetMap";
        let src = VerdictSource { label: "pmtiles:data/city.pmtiles", stated: Some(stated), acknowledge: &[] };
        let v = verdict_with(&src, None, false, &[stated.into()], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(true), "{:?}", v.reason);
        assert_eq!(v.attribution, PROTOMAPS_CREDIT, "the archive's shorter text folds into the table credit");
        let osm = "© OpenMapTiles © OpenStreetMap contributors";
        let src = VerdictSource { label: "pmtiles:data/omt.pmtiles", stated: Some(osm), acknowledge: &[] };
        assert_eq!(source_class(src.label, src.stated), SourceClass::OsmArchive);
        let v = verdict_with(&src, None, false, &[osm.into()], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(true));
        assert_eq!(v.attribution, osm, "credited with its own words, not Protomaps'");
        // A `tiles` URL on an unknown host is not classed by its credit.
        let src = VerdictSource { label: "https://t.example/v1", stated: Some(osm), acknowledge: &[] };
        assert_eq!(source_class(src.label, src.stated), SourceClass::Unknown);
    }

    /// Follow-up to F2: an unknown source with a credit is publishable
    /// only when `output.acknowledge` names it (by id or host), and the
    /// verdict carries the acknowledgement; an empty credit stays blocked.
    #[test]
    fn an_acknowledged_unknown_source_is_publishable_and_says_so() {
        let credit = vec!["© Example tiles".to_string()];
        let none = VerdictSource { label: "https://tiles.example.com/v1", stated: None, acknowledge: &[] };
        let v = verdict_with(&none, None, false, &credit, &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        let reason = v.reason.unwrap_or_default();
        assert!(reason.contains("unknown licence") && reason.contains("output.acknowledge") && reason.contains("source-licence:tiles.example.com"), "{reason}");
        assert!(v.acknowledged.is_none());

        for ack in ["source-licence:tiles.example.com", "source-licence:TILES.EXAMPLE.COM", "source-licence:https://tiles.example.com/v1"] {
            let acks = vec![ack.to_string()];
            let src = VerdictSource { label: "https://tiles.example.com/v1", stated: None, acknowledge: &acks };
            let v = verdict_with(&src, None, false, &credit, &[], None);
            assert_eq!(v.publishable, Publishable::Yes(true), "{ack}");
            assert_eq!(v.acknowledged.as_deref(), Some(ack));
            assert!(v.reason.is_none());
        }
        // A local archive by its id.
        let acks = vec!["source-licence:pmtiles:data/mine.pmtiles".to_string()];
        let src = VerdictSource { label: "pmtiles:data/mine.pmtiles", stated: None, acknowledge: &acks };
        assert_eq!(verdict_with(&src, None, false, &credit, &[], None).publishable, Publishable::Yes(true));
        // Another source's acknowledgement does not count.
        let acks = vec!["source-licence:other.example.com".to_string()];
        let src = VerdictSource { label: "https://tiles.example.com/v1", stated: None, acknowledge: &acks };
        assert_eq!(verdict_with(&src, None, false, &credit, &[], None).publishable, Publishable::Yes(false));
        // No credit: blocked whatever is acknowledged.
        let acks = vec!["source-licence:tiles.example.com".to_string()];
        let src = VerdictSource { label: "https://tiles.example.com/v1", stated: None, acknowledge: &acks };
        let v = verdict_with(&src, None, false, &[], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(false));
        assert!(v.reason.unwrap_or_default().contains("no credit"));
        assert!(v.acknowledged.is_none());
    }

    #[test]
    fn s2cloudless_is_non_commercial() {
        assert_eq!(
            verdict("openfreemap", Some("s2cloudless"), false, &[], &[], None).publishable,
            Publishable::NonCommercial("non-commercial only")
        );
        // The wordmark rule wins over non-commercial: false is stricter.
        let both = verdict("mapbox:x", Some("s2cloudless"), false, &[], &[], None);
        assert_eq!(both.publishable, Publishable::Yes(false));
    }

    /// Master's gallery draws only the 2016 layer (CC BY 4.0), which
    /// `MapView::scene_snapshot` names `s2cloudless-2016`: a story recorded
    /// over it is publishable and carries that layer's own credit.
    #[test]
    fn s2cloudless_2016_is_open_and_credited() {
        let v = verdict("openfreemap", Some("s2cloudless-2016"), false, &[], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(true));
        assert_eq!(v.attribution, format!("{OFM_CREDIT} | {S2CLOUDLESS_2016_CREDIT}"));
    }

    #[test]
    fn natural_earth_is_credited_and_publishable() {
        let v = verdict("openfreemap", Some("natural_earth"), false, &[], &[], None);
        assert_eq!(v.publishable, Publishable::Yes(true));
        assert!(v.attribution.contains(NATURAL_EARTH_CREDIT));
    }

    #[test]
    fn credits_are_deduplicated_and_joined() {
        let v = verdict(
            "openfreemap",
            None,
            true,
            &["Route data: SWAT Mobility".into(), "Route data: SWAT Mobility".into()],
            &[],
            None,
        );
        assert_eq!(v.attribution.matches("SWAT Mobility").count(), 1);
        assert!(v.attribution.contains("GHS-POP R2023A"));
        assert_eq!(v.attribution, format!("{OFM_CREDIT} | {GHSL_CREDIT} | Route data: SWAT Mobility"));
        // Layer credits last, deduplicated against everything before them.
        let l = verdict("fixture", None, false, &["A".into()], &["B".into(), "A".into()], None);
        assert_eq!(l.attribution, "A | B");
    }

    #[test]
    fn the_verdict_serialises_as_the_spec_spells_it() {
        assert_eq!(serde_json::to_value(Publishable::Yes(true)).unwrap(), serde_json::json!(true));
        assert_eq!(
            serde_json::to_value(Publishable::NonCommercial("non-commercial only")).unwrap(),
            serde_json::json!("non-commercial only")
        );
    }

    /// A temporary "repository": a directory holding a `.git` entry --
    /// a directory, or a file as a linked worktree has.
    fn fake_repo(git_file: bool) -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        if git_file {
            std::fs::write(repo.path().join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        } else {
            std::fs::create_dir(repo.path().join(".git")).unwrap();
        }
        repo
    }

    #[test]
    fn an_output_inside_a_git_work_tree_is_refused() {
        for git_file in [false, true] {
            let repo = fake_repo(git_file);
            let inside = repo.path().join("renders").join("out.mp4");
            assert!(refuse_repo_output(&inside, false).is_err(), "git file {git_file}");
            assert!(refuse_repo_output(&inside, true).is_ok());
        }
    }

    #[test]
    fn an_output_that_is_a_repository_root_is_refused() {
        let repo = fake_repo(false);
        assert!(refuse_repo_output(repo.path(), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_a_work_tree_is_followed() {
        let repo = fake_repo(false);
        std::fs::create_dir(repo.path().join("renders")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = outside.path().join("renders");
        std::os::unix::fs::symlink(repo.path().join("renders"), &link).unwrap();
        assert!(refuse_repo_output(&link.join("x.mp4"), false).is_err(), "the link's target is in the repo");
        assert!(refuse_repo_output(&link.join("deeper/x.mp4"), false).is_err(), "a missing tail under the link");
        assert!(refuse_repo_output(&outside.path().join("x.mp4"), false).is_ok());
    }

    #[test]
    fn a_temp_dir_outside_any_repository_is_accepted() {
        let plain = tempfile::tempdir().unwrap();
        // Only meaningful when the temp dir is not itself inside a repo.
        if real_location(plain.path()).ancestors().any(|d| d.join(".git").exists()) {
            eprintln!("skipped: the temp dir is inside a git work tree");
            return;
        }
        assert!(refuse_repo_output(&plain.path().join("x.mp4"), false).is_ok());
        assert!(refuse_repo_output(&plain.path().join("a/b/x.mp4"), false).is_ok());
    }

    fn table(label: &str, acks: &[String], video: bool, wordmark_drawn: bool, overlays: &[(String, Option<String>)], images: &[(String, Option<String>)]) -> Vec<PublishRow> {
        let src = VerdictSource { label, stated: None, acknowledge: acks };
        publish_table(&PublishInputs { basemap: &src, credits: &[], raster: None, video, wordmark_drawn, overlays, images, layers: &[], mapbox_sprite: false })
    }

    fn by_source<'r>(rows: &'r [PublishRow], source: &str) -> &'r PublishRow {
        rows.iter().find(|r| r.source == source).unwrap_or_else(|| panic!("no `{source}` row in {rows:#?}"))
    }

    /// R7: an image's credit is required; without one its row is blocked,
    /// and so is publishing. One with a credit is `Ok` and carries it.
    #[test]
    fn an_image_without_a_credit_blocks_publishing() {
        let images = vec![("img/a.png".to_string(), None), ("img/b.png".to_string(), Some("Photo: B".to_string()))];
        let rows = table("openfreemap", &[], true, false, &[], &images);
        let a = by_source(&rows, "image:img/a.png");
        assert_eq!(a.verdict, RowVerdict::Blocked);
        assert_eq!(a.reason.as_deref(), Some("image img/a.png has no credit"));
        assert_eq!(a.credit, None);
        let b = by_source(&rows, "image:img/b.png");
        assert_eq!((b.verdict, b.credit.as_deref()), (RowVerdict::Ok, Some("Photo: B")));
        assert_eq!(by_source(&rows, "basemap").verdict, RowVerdict::Ok);
        assert!(!publishable(&rows));
        assert_eq!(publishable_reason(&rows).as_deref(), Some("image:img/a.png: image img/a.png has no credit"));
        // A whitespace credit is no credit.
        let rows = table("openfreemap", &[], true, false, &[], &[("img/c.png".into(), Some("  ".into()))]);
        assert_eq!(by_source(&rows, "image:img/c.png").verdict, RowVerdict::Blocked);
        // Credited, it publishes.
        let rows = table("openfreemap", &[], true, false, &[], &[("img/b.png".into(), Some("Photo: B".into()))]);
        assert!(publishable(&rows), "{rows:#?}");
        // A credentialed src is redacted in the row and its reason.
        let rows = table("openfreemap", &[], true, false, &[], &[("https://u:SECRET@x.example/a.png?key=SECRET".into(), None)]);
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("SECRET"), "{text}");
    }

    /// Spec §6 / R6: a Mapbox basemap in a video needs the user's
    /// acknowledgement; with it the row is `Ok` and records it. The
    /// wordmark rule still applies: no drawn wordmark is `Blocked`
    /// whatever is acknowledged.
    #[test]
    fn mapbox_in_a_video_needs_the_acknowledgement_and_with_it_is_ok() {
        let rows = table("mapbox:mapbox.mapbox-streets-v8", &[], true, true, &[], &[]);
        let basemap = by_source(&rows, "basemap");
        assert_eq!(basemap.verdict, RowVerdict::NeedsAcknowledgement);
        assert_eq!(basemap.reason.as_deref(), Some("Mapbox's terms require that your agreement permits map content in video"));
        assert_eq!(basemap.credit.as_deref(), Some(MAPBOX_CREDIT));
        assert_eq!(basemap.acknowledged, None);
        assert!(!publishable(&rows));

        let acks = vec![MAPBOX_VIDEO_ACK.to_string()];
        let rows = table("mapbox:mapbox.mapbox-streets-v8", &acks, true, true, &[], &[]);
        let basemap = by_source(&rows, "basemap");
        assert_eq!(basemap.verdict, RowVerdict::Ok);
        assert_eq!(basemap.acknowledged.as_deref(), Some("mapbox-video"));
        assert_eq!(basemap.reason, None);
        assert!(publishable(&rows));
        let json = serde_json::to_value(basemap).unwrap();
        assert_eq!(json["verdict"], "ok");
        assert_eq!(json["acknowledged"], "mapbox-video");

        // No wordmark: blocked, the acknowledgement notwithstanding.
        let rows = table("https://api.mapbox.com/v4/mapbox.mapbox-streets-v8", &acks, true, false, &[], &[]);
        let basemap = by_source(&rows, "basemap");
        assert_eq!(basemap.verdict, RowVerdict::Blocked);
        assert_eq!(basemap.reason.as_deref(), Some(MAPBOX_WORDMARK_REASON));
        assert_eq!(basemap.acknowledged, None);
        assert_eq!(serde_json::to_value(basemap.verdict).unwrap(), "blocked");
        assert_eq!(serde_json::to_value(RowVerdict::NeedsAcknowledgement).unwrap(), "needs_acknowledgement");
    }

    /// A still is not a video: a Mapbox basemap with its wordmark drawn is
    /// `Ok` with no acknowledgement.
    #[test]
    fn a_mapbox_still_is_not_a_video() {
        let rows = table("mapbox:mapbox.mapbox-streets-v8", &[], false, true, &[], &[]);
        let basemap = by_source(&rows, "basemap");
        assert_eq!((basemap.verdict, basemap.acknowledged.as_deref()), (RowVerdict::Ok, None));
        assert!(publishable(&rows));
        // Still or not, the wordmark is owed.
        assert_eq!(by_source(&table("mapbox", &[], false, false, &[], &[]), "basemap").verdict, RowVerdict::Blocked);
    }

    /// OpenFreeMap with an attributed overlay, an overlay that states no
    /// attribution (F16: Ok, credits nothing) and a credited image: every
    /// row is `Ok`, and publishable.
    #[test]
    fn openfreemap_and_an_attributed_overlay_are_publishable() {
        let overlays = vec![("route".to_string(), Some("Route: LTA DataMall".to_string())), ("plain".to_string(), None)];
        let images = vec![("img/a.png".to_string(), Some("Photo: A".to_string()))];
        let rows = table("openfreemap", &[], true, false, &overlays, &images);
        assert_eq!(rows.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(), vec!["basemap", "overlay:route", "overlay:plain", "image:img/a.png"]);
        assert!(rows.iter().all(|r| r.verdict == RowVerdict::Ok && r.reason.is_none()), "{rows:#?}");
        assert_eq!(by_source(&rows, "basemap").credit.as_deref(), Some(OFM_CREDIT));
        assert_eq!(by_source(&rows, "overlay:route").credit.as_deref(), Some("Route: LTA DataMall"));
        assert_eq!(by_source(&rows, "overlay:plain").credit, None, "an overlay without attribution credits nothing");
        assert!(publishable(&rows));
        assert_eq!(publishable_reason(&rows), None);
        // Protomaps too.
        assert!(publishable(&table("https://api.protomaps.com/tiles/v4", &[], true, false, &[], &[])));
    }

    /// The basemap row follows the licence table: an unknown source with a
    /// credit needs its `source-licence:` acknowledgement, no credit at all
    /// is blocked, and the raster rows follow the raster terms.
    #[test]
    fn unknown_sources_and_rasters_follow_the_licence_table() {
        let credits = vec!["© Example tiles".to_string()];
        let unknown = VerdictSource { label: "https://tiles.example.com/v1", stated: None, acknowledge: &[] };
        let base = PublishInputs {
            basemap: &unknown,
            credits: &credits,
            raster: None,
            video: true,
            wordmark_drawn: false,
            overlays: &[],
            images: &[],
            layers: &[],
            mapbox_sprite: false,
        };
        let row = &publish_table(&base)[0];
        assert_eq!(row.verdict, RowVerdict::NeedsAcknowledgement);
        assert!(row.reason.as_deref().unwrap_or("").contains("source-licence:tiles.example.com"), "{row:?}");
        let acks = vec!["source-licence:tiles.example.com".to_string()];
        let known = VerdictSource { acknowledge: &acks, ..unknown };
        let row = &publish_table(&PublishInputs { basemap: &known, ..base })[0];
        assert_eq!((row.verdict, row.acknowledged.as_deref()), (RowVerdict::Ok, Some("source-licence:tiles.example.com")));
        let bare = VerdictSource { label: "pmtiles:data/city.pmtiles", stated: None, acknowledge: &acks };
        let row = &publish_table(&PublishInputs { basemap: &bare, credits: &[], ..base })[0];
        assert_eq!(row.verdict, RowVerdict::Blocked);
        assert!(row.reason.as_deref().unwrap_or("").contains("no credit"), "{row:?}");

        let ofm = VerdictSource { label: "openfreemap", stated: None, acknowledge: &[] };
        let with = |raster| publish_table(&PublishInputs { basemap: &ofm, credits: &[], raster: Some(raster), ..base });
        let r = with("s2cloudless-2016");
        assert_eq!((r[1].verdict, r[1].credit.as_deref()), (RowVerdict::Ok, Some(S2CLOUDLESS_2016_CREDIT)));
        let r = with("s2cloudless");
        assert_eq!(r[1].verdict, RowVerdict::Blocked);
        assert!(publishable_reason(&r).unwrap_or_default().contains("non-commercial only"));
        assert_eq!(with("mapbox-satellite")[1].reason.as_deref(), Some(MAPBOX_WORDMARK_REASON));
        assert_eq!(with("my-imagery")[1].verdict, RowVerdict::NeedsAcknowledgement);
        // Review finding 7: acknowledged, an unknown raster still owes a
        // credit, as an unknown basemap does.
        let acks = vec!["source-licence:my-imagery".to_string()];
        let ofm_ack = VerdictSource { label: "openfreemap", stated: None, acknowledge: &acks };
        let r = publish_table(&PublishInputs { basemap: &ofm_ack, credits: &[], raster: Some("my-imagery"), ..base });
        assert_eq!(r[1].verdict, RowVerdict::Blocked);
        assert!(r[1].reason.as_deref().unwrap_or("").contains("no credit"), "{:?}", r[1]);
        let credit = vec!["Imagery © Me".to_string()];
        let r = publish_table(&PublishInputs { basemap: &ofm_ack, credits: &credit, raster: Some("my-imagery"), ..base });
        assert_eq!((r[1].verdict, r[1].credit.as_deref(), r[1].acknowledged.as_deref()), (RowVerdict::Ok, Some("Imagery © Me"), Some("source-licence:my-imagery")));
    }

    /// Review finding 6: a local archive's hint names the id that matches
    /// -- `pmtiles:<file name>`, which survives the manifest's scrubbing of
    /// local paths -- and acknowledging it makes the source publishable.
    #[test]
    fn a_local_archives_hint_names_an_id_that_matches() {
        let credit = vec!["© Me".to_string()];
        let src = VerdictSource { label: "pmtiles:/Volumes/Maps/city.pmtiles", stated: None, acknowledge: &[] };
        let reason = verdict_with(&src, None, false, &credit, &[], None).reason.unwrap_or_default();
        assert!(reason.contains("[\"source-licence:pmtiles:city.pmtiles\"]"), "{reason}");
        let acks = vec!["source-licence:pmtiles:city.pmtiles".to_string()];
        let src = VerdictSource { acknowledge: &acks, ..src };
        assert_eq!(verdict_with(&src, None, false, &credit, &[], None).publishable, Publishable::Yes(true));
        assert_eq!(acknowledgement_id("https://t.example/v1"), "t.example");
        assert_eq!(acknowledgement_id("pmtiles:data/city.pmtiles"), "pmtiles:city.pmtiles");
        // The full label still matches, as before.
        let acks = vec!["source-licence:pmtiles:data/city.pmtiles".to_string()];
        assert!(acknowledgement_for("pmtiles:data/city.pmtiles", &acks).is_some());
    }

    /// Review finding 4: a story layer plugin has a row, as its credit is
    /// on the plate; without a credit it is blocked unless acknowledged.
    #[test]
    fn a_story_layer_has_a_row_and_blocks_without_a_credit() {
        let layers = vec![("dots".to_string(), Some("GHS-POP".to_string())), ("plain".to_string(), None)];
        let src = VerdictSource { label: "openfreemap", stated: None, acknowledge: &[] };
        let inputs = PublishInputs {
            basemap: &src,
            credits: &[],
            raster: None,
            video: true,
            wordmark_drawn: false,
            overlays: &[],
            images: &[],
            layers: &layers,
            mapbox_sprite: false,
        };
        let rows = publish_table(&inputs);
        assert_eq!((by_source(&rows, "layer:dots").verdict, by_source(&rows, "layer:dots").credit.as_deref()), (RowVerdict::Ok, Some("GHS-POP")));
        let plain = by_source(&rows, "layer:plain");
        assert_eq!(plain.verdict, RowVerdict::Blocked);
        assert!(plain.reason.as_deref().unwrap_or("").contains("source-licence:layer:plain"), "{plain:?}");
        assert!(!publishable(&rows));
        let acks = vec!["source-licence:layer:plain".to_string()];
        let src = VerdictSource { acknowledge: &acks, ..src };
        let rows = publish_table(&PublishInputs { basemap: &src, ..inputs });
        assert_eq!(by_source(&rows, "layer:plain").acknowledged.as_deref(), Some("source-licence:layer:plain"));
        assert!(publishable(&rows));
    }

    /// Review finding 8: a Mapbox sprite under a non-Mapbox basemap is a
    /// Mapbox row with the wordmark and video rules; under a Mapbox
    /// basemap the basemap row already covers it.
    #[test]
    fn a_mapbox_sprite_owes_mapbox_terms_under_any_basemap() {
        assert!(style_has_mapbox_sprite(&serde_json::json!({"sprite": "mapbox://sprites/mapbox/streets-v12"})));
        assert!(style_has_mapbox_sprite(&serde_json::json!({"sprite": [{"id": "x", "url": "https://api.mapbox.com/styles/v1/a/b/sprite"}]})));
        assert!(!style_has_mapbox_sprite(&serde_json::json!({"sprite": "https://tiles.openfreemap.org/sprites/ofm_f384/ofm"})));
        let src = VerdictSource { label: "openfreemap", stated: None, acknowledge: &[] };
        let inputs = PublishInputs {
            basemap: &src,
            credits: &[],
            raster: None,
            video: true,
            wordmark_drawn: true,
            overlays: &[],
            images: &[],
            layers: &[],
            mapbox_sprite: true,
        };
        let rows = publish_table(&inputs);
        let sprite = by_source(&rows, "sprite");
        assert_eq!((sprite.verdict, sprite.reason.as_deref()), (RowVerdict::NeedsAcknowledgement, Some(MAPBOX_VIDEO_REASON)));
        assert_eq!(by_source(&publish_table(&PublishInputs { wordmark_drawn: false, ..inputs }), "sprite").verdict, RowVerdict::Blocked);
        let mapbox = VerdictSource { label: "mapbox", ..src };
        let rows = publish_table(&PublishInputs { basemap: &mapbox, ..inputs });
        assert!(rows.iter().all(|r| r.source != "sprite"), "{rows:#?}");
    }

    #[test]
    fn the_mapbox_credit_is_never_stated_twice() {
        let v = verdict("mapbox:x", Some("mapbox-satellite"), false, &[MAPBOX_CREDIT.into()], &[], None);
        assert_eq!(v.attribution.matches("Mapbox").count(), 1, "{}", v.attribution);
    }
}
