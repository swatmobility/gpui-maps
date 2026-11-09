//! The SWAT OSRM proxy, as bytes in and bytes out: the request URL, and the
//! response parsed into a Ferrostar `Route` plus the OSRM fields Ferrostar
//! drops and turn text needs.
//!
//! No I/O. Each shell fetches with the client it already has (reqwest on
//! native, `fetch` in the browser); this module only builds the URL and reads
//! the answer. Every error string passes through
//! `sprite::redact_access_token`, because a URL with the key in it is the
//! thing most likely to end up inside one.

use crate::nav::config::RoutingConfig;
use crate::sprite::redact_access_token;
use ferrostar::models::Route;
use ferrostar::routing_adapters::osrm::OsrmResponseParser;
use ferrostar::routing_adapters::RouteResponseParser;
use serde::Deserialize;
use std::fmt;

/// `geometries=polyline6`: the Manila sample was fetched with it and decodes
/// to Manila at precision 6.
pub const POLYLINE_PRECISION: u32 = 6;

/// One OSRM `RouteStep`, reduced to what `instructions::compile` reads.
/// Every field defaults, so an `osrm-text-instructions` fixture step (which
/// has no geometry, distance or location) deserialises into it too.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsrmStep {
    #[serde(default)]
    pub maneuver: OsrmManeuver,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub driving_side: Option<String>,
    #[serde(default)]
    pub destinations: Option<String>,
    #[serde(default)]
    pub exits: Option<String>,
    #[serde(default)]
    pub rotary_name: Option<String>,
    #[serde(default)]
    pub intersections: Vec<OsrmIntersection>,
    #[serde(default)]
    pub distance: f64,
    #[serde(default)]
    pub duration: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsrmManeuver {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub modifier: Option<String>,
    #[serde(default)]
    pub exit: Option<u32>,
    #[serde(default)]
    pub bearing_after: Option<f64>,
    #[serde(default)]
    pub bearing_before: Option<f64>,
    /// `[lon, lat]`.
    #[serde(default)]
    pub location: Option<[f64; 2]>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsrmIntersection {
    #[serde(default)]
    pub lanes: Option<Vec<OsrmLane>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct OsrmLane {
    #[serde(default)]
    pub valid: bool,
}

/// SWAT's per-leg extras. Kept as the strings the proxy sent: the Manila
/// sample's `2026-03-17T21:50:00` carries no offset, so it is not RFC 3339
/// and must not be forced through a parser that would reject it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LegTimes {
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub slack: Option<f64>,
}

/// A parsed route: Ferrostar's model, and beside it, index for index, the
/// OSRM steps it was built from.
#[derive(Debug, Clone)]
pub struct NavRoute {
    pub route: Route,
    /// `steps[i]` is the OSRM step `route.steps[i]` came from. Ferrostar
    /// flattens legs in order; so does this.
    pub steps: Vec<OsrmStep>,
    /// The leg each step belongs to.
    pub step_leg: Vec<usize>,
    pub legs: Vec<LegTimes>,
}

impl NavRoute {
    /// The last waypoint, `(lon, lat)`: where a reroute still has to go.
    #[must_use]
    pub fn destination(&self) -> Option<(f64, f64)> {
        self.route.waypoints.last().map(|w| (w.coordinate.lng, w.coordinate.lat))
    }

    /// The overview geometry as `(lon, lat)`.
    #[must_use]
    pub fn line(&self) -> Vec<(f64, f64)> {
        self.route.geometry.iter().map(|c| (c.lng, c.lat)).collect()
    }
}

#[derive(Clone, PartialEq)]
pub enum RouteError {
    /// Not JSON, or not shaped like an OSRM response.
    Json(String),
    /// The service answered, with a code other than `Ok`.
    Service { code: String, message: Option<String> },
    /// `Ok`, and no route.
    NoRoute,
    /// Ferrostar could not build a route (a bad polyline, say).
    Geometry(String),
    /// Ferrostar and the raw parse disagree on the step count.
    Mismatch { ferrostar_steps: usize, osrm_steps: usize },
}

/// Redacted through `Display`: `parse_route` already redacts every field it
/// fills in, but a `RouteError` built elsewhere from a raw URL should not be
/// able to leak the key just by being logged with `{:?}` instead of `{}`.
impl fmt::Debug for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RouteError({self})")
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Json(e) => format!("the routing response is not OSRM JSON: {e}"),
            Self::Service { code, message } => match message {
                Some(message) => format!("the routing service answered {code}: {message}"),
                None => format!("the routing service answered {code}"),
            },
            Self::NoRoute => "the routing service found no route".to_string(),
            Self::Geometry(e) => format!("the route could not be built: {e}"),
            Self::Mismatch { ferrostar_steps, osrm_steps } => {
                format!("the route has {ferrostar_steps} steps but the response lists {osrm_steps}")
            }
        };
        f.write_str(&redact_access_token(&text))
    }
}

impl std::error::Error for RouteError {}

#[derive(Deserialize)]
struct RawResponse {
    code: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    routes: Vec<RawRoute>,
}

#[derive(Deserialize)]
struct RawRoute {
    #[serde(default)]
    legs: Vec<RawLeg>,
}

#[derive(Deserialize)]
struct RawLeg {
    #[serde(flatten)]
    times: LegTimes,
    #[serde(default)]
    steps: Vec<OsrmStep>,
}

/// An OSRM response, parsed.
pub fn parse_route(bytes: &[u8]) -> Result<NavRoute, RouteError> {
    let raw: RawResponse =
        serde_json::from_slice(bytes).map_err(|e| RouteError::Json(redact_access_token(&e.to_string())))?;
    if raw.code != "Ok" {
        return Err(RouteError::Service {
            code: redact_access_token(&raw.code),
            message: raw.message.map(|m| redact_access_token(&m)),
        });
    }
    let Some(first) = raw.routes.into_iter().next() else { return Err(RouteError::NoRoute) };
    let route = OsrmResponseParser::new(POLYLINE_PRECISION)
        .parse_response(bytes.to_vec())
        .map_err(|e| RouteError::Geometry(redact_access_token(&e.to_string())))?
        .into_iter()
        .next()
        .ok_or(RouteError::NoRoute)?;
    let mut steps = Vec::new();
    let mut step_leg = Vec::new();
    let mut legs = Vec::new();
    for (index, leg) in first.legs.into_iter().enumerate() {
        step_leg.extend(std::iter::repeat_n(index, leg.steps.len()));
        steps.extend(leg.steps);
        legs.push(leg.times);
    }
    if steps.len() != route.steps.len() {
        return Err(RouteError::Mismatch { ferrostar_steps: route.steps.len(), osrm_steps: steps.len() });
    }
    let mut parsed = NavRoute { route, steps, step_leg, legs };
    crate::nav::instructions::attach(&mut parsed);
    Ok(parsed)
}

/// What varies between one request and another, apart from the waypoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlOptions {
    /// Makes it a `/tdroute` (traffic at departure) rather than a `/route`.
    /// Already in the form the proxy wants (`2025-10-02T06:30:00+00:00`).
    pub start_time: Option<String>,
    /// First and last waypoint: `curb` when true.
    pub curb: Option<bool>,
    /// Every waypoint in between: `curb` unless false.
    pub intermediate_curb: Option<bool>,
    /// `geometries=polyline6`. The reference client asks for the default polyline5.
    pub polyline6: bool,
}

impl UrlOptions {
    /// A navigation request: polyline6, and a van arrives at the kerb.
    #[must_use]
    pub fn navigation() -> Self {
        Self { start_time: None, curb: Some(true), intermediate_curb: None, polyline6: true }
    }

    /// Exactly what the reference client's `request_url` sends for default
    /// settings.
    #[must_use]
    pub fn reference() -> Self {
        Self { start_time: None, curb: None, intermediate_curb: None, polyline6: false }
    }
}

/// The reference client's `approaches`, verbatim: the ends take `curb`, the middle
/// takes `intermediate_curb`, and a single coordinate is both ends.
#[must_use]
pub fn approaches(count: usize, curb: Option<bool>, intermediate_curb: Option<bool>) -> String {
    let mode = |curb: bool| if curb { "curb" } else { "unrestricted" };
    (0..count)
        .map(|index| {
            if index == 0 || index + 1 == count {
                mode(curb.unwrap_or(false))
            } else {
                mode(intermediate_curb.unwrap_or(true))
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// JavaScript's `encodeURIComponent`, which is what the reference escapes
/// query values with (the reference client's `encode_component`, verbatim).
#[must_use]
pub fn encode_component(value: &str) -> String {
    const UNRESERVED: &[u8] = b"-_.!~*'()";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The whole request, in the reference client's parameter order and escaping:
///
/// ```text
/// {url}/{route|tdroute}/v1/{road_network}/{lon},{lat};...
///     ?overview=full&alternatives=false&steps=true[&geometries=polyline6]
///     &approaches=...[&start_time=...][&access_token=...]
/// ```
///
/// `waypoints` are `(lon, lat)`. THE RETURNED STRING CARRIES THE KEY: log it
/// only through `redact_access_token`.
#[must_use]
pub fn request_url(config: &RoutingConfig, waypoints: &[(f64, f64)], options: &UrlOptions) -> String {
    let coordinates = waypoints.iter().map(|(lon, lat)| format!("{lon},{lat}")).collect::<Vec<_>>().join(";");
    let kind = if options.start_time.is_some() { "tdroute" } else { "route" };
    let mut url = format!("{}/{kind}/v1/{}/{coordinates}", config.url, config.road_network);
    let mut query: Vec<(&str, String)> =
        vec![("overview", "full".into()), ("alternatives", "false".into()), ("steps", "true".into())];
    if options.polyline6 {
        query.push(("geometries", "polyline6".into()));
    }
    query.push(("approaches", approaches(waypoints.len(), options.curb, options.intermediate_curb)));
    if let Some(start_time) = &options.start_time {
        query.push(("start_time", start_time.clone()));
    }
    if let Some(key) = &config.key {
        query.push(("access_token", key.expose().to_string()));
    }
    url.push('?');
    url.push_str(&query.iter().map(|(name, value)| format!("{name}={}", encode_component(value))).collect::<Vec<_>>().join("&"));
    url
}

/// One request a shell has to make. `Debug` never shows the key.
#[derive(Clone, PartialEq)]
pub struct RouteRequest {
    pub id: u64,
    /// Carries the key. See [`request_url`].
    pub url: String,
    pub waypoints: Vec<(f64, f64)>,
}

impl fmt::Debug for RouteRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteRequest")
            .field("id", &self.id)
            .field("url", &redact_access_token(&self.url))
            .field("waypoints", &self.waypoints)
            .finish()
    }
}

/// A fetch failure as a shell may show or log it: redacted.
#[must_use]
pub fn describe_fetch_error(error: &str) -> String {
    redact_access_token(error)
}

/// A response made safe to commit as a fixture: every `access_token` field
/// dropped, every string redacted, and refused outright if `key` -- in its
/// raw form, percent-encoded (as it would appear inside a URL quoted in some
/// other field), or JSON-escaped (as it would appear if a field held an
/// already-JSON-encoded copy of the request) -- still appears anywhere in
/// the result.
pub fn strip_credentials(bytes: &[u8], key: &str) -> Result<Vec<u8>, String> {
    fn scrub(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("access_token");
                map.values_mut().for_each(scrub);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(scrub),
            serde_json::Value::String(text) => *text = redact_access_token(text),
            _ => {}
        }
    }
    let mut json: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the response is not JSON: {e}"))?;
    scrub(&mut json);
    let mut out = serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())?;
    out.push(b'\n');
    let key = key.trim();
    if !key.is_empty() {
        let percent_encoded = encode_component(key);
        // `serde_json::to_string` on a plain string is the quoted, escaped
        // form (`"a+b\"c"`); the inner text between the quotes is what a
        // JSON-escaped copy of the key would look like embedded in a field.
        let json_quoted = serde_json::to_string(key).unwrap_or_default();
        let json_escaped = json_quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(&json_quoted);
        // Every character as its own `\u` escape: what an encoder that
        // escapes every non-ASCII-safe byte (rather than only `"`, `\` and
        // control characters) would have produced.
        let json_unicode_escaped: String = key.chars().map(|c| format!("\\u{:04x}", c as u32)).collect();
        let contains = |needle: &str| !needle.is_empty() && out.windows(needle.len()).any(|w| w == needle.as_bytes());
        if contains(key) || contains(&percent_encoded) || contains(json_escaped) || contains(&json_unicode_escaped) {
            return Err("the key is still in the response after stripping; refusing to save it".to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::config::RoutingKey;

    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");

    fn config(key: Option<&str>) -> RoutingConfig {
        RoutingConfig {
            url: "https://osrm.example.com".into(),
            road_network: "van".into(),
            key: key.and_then(RoutingKey::new),
        }
    }

    #[test]
    fn the_manila_fixture_parses_into_eleven_steps_with_the_swat_extras() {
        let route = parse_route(MANILA).expect("the Manila fixture parses");
        assert_eq!(route.route.steps.len(), 11);
        assert_eq!(route.steps.len(), 11);
        assert_eq!(route.route.geometry.len(), 119);
        assert!((route.route.distance - 3211.7).abs() < 1e-9);
        assert_eq!(route.step_leg, vec![0; 11]);
        assert_eq!(
            route.legs,
            vec![LegTimes {
                start_time: Some("2026-03-17T21:50:00".into()),
                end_time: Some("2026-03-17T21:59:25".into()),
                slack: Some(0.0),
            }]
        );
        assert_eq!(route.steps[0].reference.as_deref(), Some("150; 170"));
        assert_eq!((route.steps[4].maneuver.kind.as_str(), route.steps[4].maneuver.modifier.as_deref()), ("continue", Some("uturn")));
        assert_eq!(route.route.steps[0].road_name.as_deref(), Some("Padre Burgos Avenue"));
        let start = route.route.geometry[0];
        assert!((start.lat - 14.591513).abs() < 1e-6 && (start.lng - 120.980214).abs() < 1e-6, "polyline6 decodes to Manila: {start:?}");
        assert_eq!(route.destination(), Some((120.993246, 14.604056)));
    }

    #[test]
    fn a_service_error_is_reported_with_its_code() {
        let body = br#"{"message":"TDRoute query string malformed close to position 112","code":"InvalidQuery"}"#;
        assert_eq!(
            parse_route(body).unwrap_err(),
            RouteError::Service { code: "InvalidQuery".into(), message: Some("TDRoute query string malformed close to position 112".into()) }
        );
    }

    #[test]
    fn a_non_json_body_and_an_empty_route_list_are_errors() {
        assert!(matches!(parse_route(b"<html>502 Bad Gateway</html>"), Err(RouteError::Json(_))));
        assert_eq!(parse_route(br#"{"code":"Ok","routes":[]}"#).unwrap_err(), RouteError::NoRoute);
    }

    #[test]
    fn an_error_that_quotes_a_url_never_shows_the_key() {
        let body = br#"{"code":"InvalidUrl","message":"bad https://p.example.com/route?access_token=SECRET123&x=1"}"#;
        let shown = parse_route(body).unwrap_err().to_string();
        assert!(!shown.contains("SECRET123"), "{shown}");
        assert!(shown.contains("access_token=[REDACTED]"), "{shown}");
    }

    #[test]
    fn with_reference_options_the_url_is_byte_for_byte_the_reference_clients() {
        // The reference client's routing test
        // `the_request_is_one_get_over_every_waypoint`, same inputs.
        let waypoints: Vec<(f64, f64)> = (0..3).map(|n| (103.8 + n as f64 / 100., 1.3 + n as f64 / 100.)).collect();
        assert_eq!(
            request_url(&config(Some("secret")), &waypoints, &UrlOptions::reference()),
            "https://osrm.example.com/route/v1/van/103.8,1.3;103.81,1.31;103.82,1.32\
             ?overview=full&alternatives=false&steps=true\
             &approaches=unrestricted%3Bcurb%3Bunrestricted&access_token=secret"
        );
        assert_eq!(approaches(4, None, None), "unrestricted;curb;curb;unrestricted");
        assert_eq!(approaches(1, None, Some(true)), "unrestricted");
    }

    #[test]
    fn a_navigation_request_asks_for_polyline6_and_the_kerb() {
        assert_eq!(
            request_url(&config(Some("secret")), &[(103.8559, 1.3006), (103.8607, 1.2834)], &UrlOptions::navigation()),
            "https://osrm.example.com/route/v1/van/103.8559,1.3006;103.8607,1.2834\
             ?overview=full&alternatives=false&steps=true&geometries=polyline6\
             &approaches=curb%3Bcurb&access_token=secret"
        );
    }

    #[test]
    fn a_start_time_switches_to_tdroute_and_is_escaped() {
        let options = UrlOptions { start_time: Some("2025-10-02T06:30:00+00:00".into()), ..UrlOptions::navigation() };
        let url = request_url(&config(None), &[(103.8, 1.3), (103.81, 1.31)], &options);
        assert!(url.contains("/tdroute/v1/van/"), "{url}");
        assert!(url.contains("&start_time=2025-10-02T06%3A30%3A00%2B00%3A00"), "{url}");
        assert!(!url.contains("access_token"), "no key, no parameter: {url}");
    }

    #[test]
    fn a_request_debug_prints_no_key_even_an_escaped_one() {
        let url = request_url(&config(Some("a+b/c")), &[(103.8, 1.3), (103.81, 1.31)], &UrlOptions::navigation());
        assert!(url.ends_with("&access_token=a%2Bb%2Fc"), "{url}");
        let request = RouteRequest { id: 7, url, waypoints: vec![] };
        let shown = format!("{request:?}");
        assert!(!shown.contains("a%2Bb%2Fc") && shown.contains("[REDACTED]"), "{shown}");
    }

    #[test]
    fn a_fixture_is_saved_only_once_the_key_is_gone() {
        let body = br#"{"code":"Ok","access_token":"SECRETKEY","routes":[],"note":"see https://x/?access_token=SECRETKEY&y=1"}"#;
        let clean = String::from_utf8(strip_credentials(body, "SECRETKEY").unwrap()).unwrap();
        assert!(!clean.contains("SECRETKEY"), "{clean}");
        assert!(clean.contains("access_token=[REDACTED]"));
        let stubborn = br#"{"code":"Ok","waypoints":[{"name":"SECRETKEY"}]}"#;
        assert!(strip_credentials(stubborn, "SECRETKEY").is_err(), "a key in an ordinary field is caught by the final scan");
        assert!(strip_credentials(MANILA, "SECRETKEY").is_ok());
    }

    #[test]
    fn a_percent_encoded_copy_of_the_key_is_also_caught() {
        let body = br#"{"code":"Ok","message":"x?k=a%2Bb"}"#;
        assert!(strip_credentials(body, "a+b").is_err(), "a URL-escaped copy of the key must not survive");
    }

    #[test]
    fn a_json_escaped_copy_of_the_key_is_also_caught() {
        // Input already escapes the quote inside the key (`a\"b`, i.e. the
        // three bytes a, backslash, quote, b); parsing decodes it to `a"b`,
        // and re-serialising re-escapes the quote, so only the escaped form
        // -- not the raw key `a"b` -- ever appears in `out`. Only the new
        // `json_escaped` check catches this: the plain byte scan for `a"b`
        // does not, because the output has a backslash between `a` and `"`.
        let body: &[u8] = b"{\"code\":\"Ok\",\"message\":\"see a\\\"b here\"}";
        assert!(strip_credentials(body, "a\"b").is_err(), "a JSON-escaped copy of the key must not survive");
    }

    #[test]
    fn a_key_that_is_a_control_character_survives_only_as_a_fresh_u_escape_and_is_still_caught() {
        // A BEL (0x07) can only appear in JSON source already `\u`-escaped;
        // parsing decodes it to the raw control byte, and re-serialising
        // emits a FRESH `\u0007` (serde_json must escape control bytes).
        // The raw byte never appears in `out` at all -- only the escaped
        // text does -- so only the new `json_unicode_escaped` check catches
        // this.
        let body: &[u8] = b"{\"code\":\"Ok\",\"message\":\"bell: \\u0007 here\"}";
        assert!(strip_credentials(body, "\u{7}").is_err(), "a \\u-escaped copy of the key must not survive");
    }

    #[test]
    fn a_fetch_error_is_redacted_before_it_is_shown() {
        let shown = describe_fetch_error("GET https://p/route?access_token=SECRET123 timed out");
        assert!(!shown.contains("SECRET123"), "{shown}");
        assert!(shown.contains("access_token=[REDACTED]"), "{shown}");
    }

    const BUGIS: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-bugis-marina-bay.json");
    const SELETAR: &[u8] = include_bytes!("../../tests/fixtures/routes/singapore-seletar-rotary-uturn.json");

    #[test]
    fn the_singapore_fixtures_drive_on_the_left_and_the_seletar_one_has_a_rotary_and_a_u_turn() {
        for bytes in [BUGIS, SELETAR] {
            let route = parse_route(bytes).expect("a committed fixture parses");
            assert!(route.steps.len() > 5);
            assert!(route.steps.iter().all(|s| s.driving_side.as_deref() == Some("left")), "Singapore drives on the left");
            assert!(!String::from_utf8_lossy(bytes).contains("access_token"));
        }
        let seletar = parse_route(SELETAR).unwrap();
        assert!(seletar.steps.iter().any(|s| s.maneuver.kind == "rotary" && s.maneuver.exit.is_some()));
        assert!(seletar.steps.iter().any(|s| s.maneuver.modifier.as_deref() == Some("uturn")));
        let bugis = parse_route(BUGIS).unwrap();
        assert!(bugis.steps.iter().any(|s| s.maneuver.kind == "fork"));
    }
}
