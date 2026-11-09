//! A bounded parse of an OMM JSON body (CelesTrak `FORMAT=json`, or
//! Space-Track `gp`, which quotes every number) into validated records.
//!
//! The body comes off the network, so every limit is explicit (spec §4.4,
//! R17): the body is refused whole past [`MAX_BODY_BYTES`], the array past
//! [`MAX_RECORDS`], every string is cut to [`MAX_STR_CHARS`] characters with
//! control characters removed, and a record whose numbers are missing,
//! non-finite or out of range is skipped and counted -- never a panic.

use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::io::Read;

/// A body larger than this is refused whole ([`OmmError::TooLarge`]). The
/// full CelesTrak `active` group is about 11 MB of JSON.
pub const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;
/// An array with more elements than this is refused ([`OmmError::TooManyRecords`]).
pub const MAX_RECORDS: usize = 50_000;
/// `OBJECT_NAME` and `OBJECT_ID` are cut to this many characters.
pub const MAX_STR_CHARS: usize = 64;

/// One validated OMM record. Angles are in degrees, taken mod 360; the mean
/// motion is in revolutions per day.
#[derive(Clone, Debug, PartialEq)]
pub struct OmmRecord {
    /// `OBJECT_NAME`, at most [`MAX_STR_CHARS`] characters.
    pub object_name: Box<str>,
    /// `OBJECT_ID` (the international designator), at most
    /// [`MAX_STR_CHARS`] characters.
    pub object_id: Box<str>,
    /// 1..=999,999,999 (the 6-digit catalogue numbers of spec §1.1 fit).
    pub norad_id: u32,
    /// `EPOCH`, UTC.
    pub epoch: chrono::NaiveDateTime,
    /// `epoch` as Unix seconds (UTC), microsecond precision.
    pub epoch_unix: f64,
    /// `MEAN_MOTION`, revolutions per day.
    pub mean_motion: f64,
    /// `ECCENTRICITY`, in [0, 1).
    pub eccentricity: f64,
    /// `INCLINATION`, degrees.
    pub inclination_deg: f64,
    /// `RA_OF_ASC_NODE`, degrees.
    pub raan_deg: f64,
    /// `ARG_OF_PERICENTER`, degrees.
    pub arg_pericenter_deg: f64,
    /// `MEAN_ANOMALY`, degrees.
    pub mean_anomaly_deg: f64,
    /// `BSTAR` drag term, per Earth radius.
    pub bstar: f64,
    /// `MEAN_MOTION_DOT` as published (rev/day², the TLE's field).
    pub mean_motion_dot: f64,
    /// `MEAN_MOTION_DDOT` as published (rev/day³, the TLE's field).
    pub mean_motion_ddot: f64,
    /// `ELEMENT_SET_NO` (0 when absent).
    pub element_set_no: u32,
    /// `REV_AT_EPOCH` (0 when absent).
    pub rev_at_epoch: u32,
}

/// The records that validated, and how many elements were skipped and why
/// (the key is the first field that failed, or `"not an object"`).
#[derive(Debug, Default)]
pub struct ParseReport {
    /// The records that validated, in document order.
    pub records: Vec<OmmRecord>,
    /// How many array elements were skipped.
    pub skipped: usize,
    /// Skips by reason: the first field that failed, or `"not an object"`.
    pub reasons: BTreeMap<&'static str, usize>,
}

/// Why a whole body was refused. A bad record is not an error: it is
/// skipped and counted in [`ParseReport`].
#[derive(Debug)]
pub enum OmmError {
    /// Over [`MAX_BODY_BYTES`].
    TooLarge,
    /// The body does not start with `[`; holds its first (up to) 80 bytes,
    /// control characters removed -- CelesTrak answers "No GP data found"
    /// in plain text.
    NotJsonArray(String),
    /// The JSON itself is broken (truncated, bad syntax) or the read failed.
    Malformed(String),
    /// More than [`MAX_RECORDS`] elements.
    TooManyRecords,
}

impl std::fmt::Display for OmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "OMM body is larger than {MAX_BODY_BYTES} bytes"),
            Self::NotJsonArray(head) => write!(f, "OMM body is not a JSON array: {head:?}"),
            Self::Malformed(why) => write!(f, "OMM body is malformed JSON: {why}"),
            Self::TooManyRecords => write!(f, "OMM body has more than {MAX_RECORDS} records"),
        }
    }
}

impl std::error::Error for OmmError {}

// # Memory
//
// The body is streamed, never built into a `serde_json::Value` tree: each
// array element is read into a fixed array of 15 `Field`s (the known keys; every other key,
// and every nested array or object, is skipped with `IgnoredAny`, which
// stores nothing), validated at once, and dropped. Strings are cut to
// `MAX_STR_CHARS` characters as they are read. So, for a body of `B` bytes,
// the parse holds at most:
//
// - the records kept: <= MAX_RECORDS x (size_of::<OmmRecord>() = 136 B + two
//   strings of <= 64 chars, <= 256 B each) = 50,000 x 648 B = 32.4 MB, and in
//   practice less, as that much kept text needs more input than the cap allows;
// - one element in flight: 15 fields x (<= 256 B of text + the enum), ~5 KB;
// - serde_json's scratch buffer for the one string being unescaped: <= B.
//
// Worst case B + 32.4 MB beside the caller's own copy of the body, whatever
// its shape. (A `Value` per element amplified a 32 MB `[[0,0,...]]` to
// 548 MB.) Measured in `tests::hostile_memory_*` (release, /usr/bin/time -l),
// over a 46.6 MB baseline that holds the 32 MB body: `[[0,0,...]]` +0.1 MB,
// 50,000 records of maximal kept text in a 29.3 MB body +23 MB, one 32 MB
// escaped string +7.7 MB. See docs/measurements/2026-09-29-satellites.md.

/// A string field's value as read: the display text (control characters
/// removed, cut to `MAX_STR_CHARS`) and, for short strings, the number it
/// holds (Space-Track quotes every number).
fn str_field(s: &str) -> Field {
    let text = s.chars().filter(|c| !c.is_control()).take(MAX_STR_CHARS).collect::<String>();
    let num = if s.len() <= MAX_STR_CHARS { s.trim().parse::<f64>().ok() } else { None };
    Field::Str { text: text.into_boxed_str(), num }
}

/// One OMM field's JSON value, reduced as it streams past.
#[derive(Debug, Default)]
enum Field {
    /// Missing, or JSON `null`.
    #[default]
    Absent,
    Num(f64),
    Str { text: Box<str>, num: Option<f64> },
    /// A bool, array or object: never a valid value here.
    Other,
}

impl<'de> serde::Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Field;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E>(self, _: bool) -> Result<Field, E> {
                Ok(Field::Other)
            }
            fn visit_i64<E>(self, v: i64) -> Result<Field, E> {
                Ok(Field::Num(v as f64))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Field, E> {
                Ok(Field::Num(v as f64))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Field, E> {
                Ok(Field::Num(v))
            }
            fn visit_str<E>(self, v: &str) -> Result<Field, E> {
                Ok(str_field(v))
            }
            fn visit_unit<E>(self) -> Result<Field, E> {
                Ok(Field::Absent)
            }
            fn visit_none<E>(self) -> Result<Field, E> {
                Ok(Field::Absent)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Field, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Field::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Field, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Field::Other)
            }
        }
        d.deserialize_any(V)
    }
}

/// The OMM keys read; every other key is skipped.
const KEYS: [&str; 15] = [
    "OBJECT_NAME",
    "OBJECT_ID",
    "EPOCH",
    "MEAN_MOTION",
    "ECCENTRICITY",
    "INCLINATION",
    "RA_OF_ASC_NODE",
    "ARG_OF_PERICENTER",
    "MEAN_ANOMALY",
    "NORAD_CAT_ID",
    "BSTAR",
    "MEAN_MOTION_DOT",
    "MEAN_MOTION_DDOT",
    "ELEMENT_SET_NO",
    "REV_AT_EPOCH",
];

/// An object key: its index in `KEYS`, or `None` for any other key. Read
/// without allocating.
struct Key(Option<usize>);

impl<'de> serde::Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Key;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object key")
            }
            fn visit_str<E>(self, v: &str) -> Result<Key, E> {
                Ok(Key(KEYS.iter().position(|k| *k == v)))
            }
        }
        d.deserialize_identifier(V)
    }
}

/// One array element: an object's known fields (a repeated key: the last
/// wins), or `None` for anything that is not an object.
struct Element(Option<[Field; KEYS.len()]>);

impl<'de> serde::Deserialize<'de> for Element {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Element;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an OMM object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Element, A::Error> {
                let mut fields: [Field; KEYS.len()] = Default::default();
                while let Some(Key(k)) = map.next_key::<Key>()? {
                    match k {
                        Some(i) => fields[i] = map.next_value::<Field>()?,
                        None => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(Element(Some(fields)))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_unit<E>(self) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_none<E>(self) -> Result<Element, E> {
                Ok(Element(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Element, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Element(None))
            }
        }
        d.deserialize_any(V)
    }
}

/// The top-level array, validated element by element as it streams. Past
/// `MAX_RECORDS` elements it keeps consuming (so the JSON is still checked)
/// but stores nothing, and flags the overflow.
struct Stream {
    report: ParseReport,
    overflow: bool,
}

impl<'de> serde::Deserialize<'de> for Stream {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Stream;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Stream, A::Error> {
                let mut report = ParseReport::default();
                let mut seen = 0usize;
                while seen < MAX_RECORDS {
                    let Some(Element(item)) = seq.next_element::<Element>()? else {
                        return Ok(Stream { report, overflow: false });
                    };
                    seen += 1;
                    match item.as_ref().ok_or("not an object").and_then(validate) {
                        Ok(r) => report.records.push(r),
                        Err(why) => {
                            report.skipped += 1;
                            *report.reasons.entry(why).or_default() += 1;
                        }
                    }
                }
                let mut overflow = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    overflow = true;
                }
                Ok(Stream { report, overflow })
            }
        }
        d.deserialize_seq(V)
    }
}

/// [`parse_omm`] over a reader, which is read to at most `MAX_BODY_BYTES + 1`
/// bytes -- one past the cap, so an oversized body is seen and refused
/// without reading the rest of it.
pub fn parse_omm_reader(r: impl Read) -> Result<ParseReport, OmmError> {
    let mut buf = Vec::new();
    r.take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| OmmError::Malformed(e.to_string()))?;
    parse_omm(&buf)
}

/// Parses an OMM JSON array. A leading UTF-8 BOM is allowed. Never panics.
pub fn parse_omm(bytes: &[u8]) -> Result<ParseReport, OmmError> {
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(OmmError::TooLarge);
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'[') {
        let head: String = String::from_utf8_lossy(&bytes[..bytes.len().min(80)])
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        return Err(OmmError::NotJsonArray(head));
    }
    let stream: Stream =
        serde_json::from_slice(bytes).map_err(|e| OmmError::Malformed(e.to_string()))?;
    if stream.overflow {
        return Err(OmmError::TooManyRecords);
    }
    Ok(stream.report)
}

type Fields = [Field; KEYS.len()];

fn field<'a>(f: &'a Fields, key: &'static str) -> &'a Field {
    &f[KEYS.iter().position(|k| *k == key).expect("a known key")]
}

/// A number, or a string holding one.
fn num(f: &Fields, key: &'static str) -> Result<f64, &'static str> {
    let v = match field(f, key) {
        Field::Num(n) => Some(*n),
        Field::Str { num, .. } => *num,
        _ => None,
    };
    v.filter(|x| x.is_finite()).ok_or(key)
}

/// [`num`] for a field that may be absent or null (then 0).
fn opt_num(f: &Fields, key: &'static str) -> Result<f64, &'static str> {
    if matches!(field(f, key), Field::Absent) {
        Ok(0.0)
    } else {
        num(f, key)
    }
}

/// A string field as read (already cleaned and capped); `""` when absent or
/// not a string.
fn text(f: &Fields, key: &'static str) -> Box<str> {
    match field(f, key) {
        Field::Str { text, .. } => text.clone(),
        _ => Box::from(""),
    }
}

/// `a` mod 360 in `[0, 360)`. `rem_euclid` alone returns exactly 360.0 for
/// a tiny negative `a` (360 - 1e-20 rounds to 360).
#[must_use]
pub(crate) fn wrap_deg(a: f64) -> f64 {
    let w = a.rem_euclid(360.0);
    if w >= 360.0 { 0.0 } else { w }
}

fn validate(m: &Fields) -> Result<OmmRecord, &'static str> {
    let norad = num(m, "NORAD_CAT_ID")?;
    if !(norad.fract() == 0.0 && (1.0..=999_999_999.0).contains(&norad)) {
        return Err("NORAD_CAT_ID");
    }
    let mean_motion = num(m, "MEAN_MOTION")?;
    if !(mean_motion > 0.05 && mean_motion <= 20.0) {
        return Err("MEAN_MOTION");
    }
    let eccentricity = num(m, "ECCENTRICITY")?;
    if !(0.0..0.999).contains(&eccentricity) {
        return Err("ECCENTRICITY");
    }
    let inclination_deg = num(m, "INCLINATION")?;
    if !(0.0..=180.0).contains(&inclination_deg) {
        return Err("INCLINATION");
    }
    let angle = |k| num(m, k).map(wrap_deg);
    let small =
        |k: &'static str| opt_num(m, k).and_then(|x| if x.abs() < 1.0 { Ok(x) } else { Err(k) });
    let Field::Str { text: epoch_s, .. } = field(m, "EPOCH") else { return Err("EPOCH") };
    let epoch = chrono::NaiveDateTime::parse_from_str(epoch_s.trim(), "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|_| "EPOCH")?;
    if !(1957..=2100).contains(&chrono::Datelike::year(&epoch)) {
        return Err("EPOCH");
    }
    // Whole seconds plus the fraction, so the epoch keeps the nanoseconds
    // sgp4 sees (`datetime`), to f64's ~0.2 us resolution at today's dates.
    let utc = epoch.and_utc();
    let epoch_unix = utc.timestamp() as f64 + f64::from(utc.timestamp_subsec_nanos()) * 1e-9;
    Ok(OmmRecord {
        object_name: text(m, "OBJECT_NAME"),
        object_id: text(m, "OBJECT_ID"),
        norad_id: norad as u32,
        epoch,
        epoch_unix,
        mean_motion,
        eccentricity,
        inclination_deg,
        raan_deg: angle("RA_OF_ASC_NODE")?,
        arg_pericenter_deg: angle("ARG_OF_PERICENTER")?,
        mean_anomaly_deg: angle("MEAN_ANOMALY")?,
        bstar: small("BSTAR")?,
        mean_motion_dot: small("MEAN_MOTION_DOT")?,
        mean_motion_ddot: small("MEAN_MOTION_DDOT")?,
        element_set_no: opt_num(m, "ELEMENT_SET_NO")?.clamp(0.0, u32::MAX as f64) as u32,
        rev_at_epoch: opt_num(m, "REV_AT_EPOCH")?.clamp(0.0, u32::MAX as f64) as u32,
    })
}

/// The sgp4 crate's element set for a record (`norad_id` widens to its `u64`).
///
/// **Unstable:** returns an `sgp4` type, so an `sgp4` upgrade can change
/// this signature within a 0.x minor of this crate.
#[must_use]
pub fn to_elements(r: &OmmRecord) -> sgp4::Elements {
    sgp4::Elements {
        object_name: Some(r.object_name.to_string()),
        international_designator: Some(r.object_id.to_string()),
        norad_id: u64::from(r.norad_id),
        classification: sgp4::Classification::Unclassified,
        datetime: r.epoch,
        mean_motion_dot: r.mean_motion_dot,
        mean_motion_ddot: r.mean_motion_ddot,
        drag_term: r.bstar,
        element_set_number: u64::from(r.element_set_no),
        inclination: r.inclination_deg,
        right_ascension: r.raan_deg,
        eccentricity: r.eccentricity,
        argument_of_perigee: r.arg_pericenter_deg,
        mean_anomaly: r.mean_anomaly_deg,
        mean_motion: r.mean_motion,
        revolution_number: u64::from(r.rev_at_epoch),
        ephemeris_type: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const STATIONS: &[u8] = include_bytes!("../../tests/fixtures/orbit/stations.json");

    #[test]
    fn the_stations_sample_parses_every_record() {
        let r = parse_omm(STATIONS).expect("the fixture parses");
        assert_eq!((r.records.len(), r.skipped), (22, 0));
        let iss = r.records.iter().find(|x| x.norad_id == 25544).expect("the ISS is in the sample");
        assert_eq!(&*iss.object_name, "ISS (ZARYA)");
        assert!((iss.mean_motion - 15.48680135).abs() < 1e-9);
        assert!((iss.inclination_deg - 51.6312).abs() < 1e-9);
        assert_eq!(iss.epoch.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(), "2026-09-28T11:09:16.121952");
        // A 6-digit catalogue number (spec §1.1) survives as-is.
        assert!(r.records.iter().any(|x| x.norad_id == 100_057));
    }

    #[test]
    fn space_track_string_valued_numbers_parse() {
        // Space-Track's `gp` JSON quotes every number (R14 imports it through this parser).
        let body = br#"[{"OBJECT_NAME":"ISS (ZARYA)","OBJECT_ID":"1998-067A","EPOCH":"2026-09-28T11:09:16.121952",
          "MEAN_MOTION":"15.48680135","ECCENTRICITY":"0.00071596","INCLINATION":"51.6312","RA_OF_ASC_NODE":"10.0",
          "ARG_OF_PERICENTER":"20.0","MEAN_ANOMALY":"30.0","NORAD_CAT_ID":"25544","BSTAR":"0.0001",
          "MEAN_MOTION_DOT":"0.0001","MEAN_MOTION_DDOT":"0"}]"#;
        let r = parse_omm(body).expect("parses");
        assert_eq!((r.records.len(), r.records[0].norad_id), (1, 25544));
    }

    #[test]
    fn a_plain_text_answer_is_an_error_not_a_panic() {
        assert!(matches!(parse_omm(b"No GP data found"), Err(OmmError::NotJsonArray(_))));
        assert!(matches!(parse_omm(b""), Err(OmmError::NotJsonArray(_))));
        assert!(matches!(parse_omm(b"{\"a\":1}"), Err(OmmError::NotJsonArray(_))));
    }

    const GOOD: &str = r#"{"OBJECT_NAME":"X","OBJECT_ID":"2000-001A","EPOCH":"2026-09-28T11:09:16","MEAN_MOTION":15.5,
      "ECCENTRICITY":0.001,"INCLINATION":51.6,"RA_OF_ASC_NODE":1,"ARG_OF_PERICENTER":2,"MEAN_ANOMALY":3,
      "NORAD_CAT_ID":@ID@,"BSTAR":0.0001,"MEAN_MOTION_DOT":0,"MEAN_MOTION_DDOT":0}"#;

    /// `GOOD` with id `norad` and one `"KEY":value` pair replaced; the value
    /// runs to the next ',' or '}'. The id placeholder is `@ID@`, not a
    /// word: `NORAD` would also match the `NORAD_CAT_ID` key and rename it.
    fn rec(norad: &str, patch: (&str, &str)) -> String {
        let base = GOOD.replace("@ID@", norad);
        let key = format!("\"{}\":", patch.0);
        let at = base.find(&key).expect("key") + key.len();
        let end = at + base[at..].find([',', '}']).expect("end");
        format!("{}{}{}", &base[..at], patch.1, &base[end..])
    }

    /// Parses `[record]` without panicking; `Some(reason)` when the one
    /// record was refused, `None` when it was accepted.
    fn verdict(body: &str) -> Option<&'static str> {
        let r = std::panic::catch_unwind(|| parse_omm(body.as_bytes()))
            .unwrap_or_else(|_| panic!("parse_omm panicked on {:.120}", body))
            .unwrap_or_else(|e| panic!("one bad record refused the whole body: {e}"));
        assert_eq!(r.records.len() + r.skipped, 1);
        if r.skipped == 1 {
            assert_eq!(r.reasons.len(), 1);
            Some(*r.reasons.keys().next().unwrap())
        } else {
            None
        }
    }

    #[test]
    fn the_hostile_corpus_is_refused_record_by_record_for_the_right_reason() {
        let refused: &[(&str, (&str, &str), &str)] = &[
            ("1", ("MEAN_MOTION", "\"NaN\""), "MEAN_MOTION"),
            ("2", ("MEAN_MOTION", "1e308"), "MEAN_MOTION"),
            ("3", ("MEAN_MOTION", "-15.5"), "MEAN_MOTION"),
            ("4", ("ECCENTRICITY", "1.0"), "ECCENTRICITY"),
            ("5", ("INCLINATION", "{\"a\":1}"), "INCLINATION"),
            ("5", ("INCLINATION", "[51.6]"), "INCLINATION"),
            ("5", ("INCLINATION", "true"), "INCLINATION"),
            ("5", ("INCLINATION", "null"), "INCLINATION"),
            ("7", ("EPOCH", "\"1899-01-01T00:00:00\""), "EPOCH"),
            ("7", ("EPOCH", "20260928"), "EPOCH"),
            ("8", ("BSTAR", "\"inf\""), "BSTAR"),
            ("8", ("BSTAR", "\"-inf\""), "BSTAR"),
            ("0", ("MEAN_ANOMALY", "3"), "NORAD_CAT_ID"),
            ("1.5", ("MEAN_ANOMALY", "3"), "NORAD_CAT_ID"),
            ("\"x\"", ("MEAN_ANOMALY", "3"), "NORAD_CAT_ID"),
            ("9", ("MEAN_ANOMALY", "\"NaN\""), "MEAN_ANOMALY"),
        ];
        for (id, patch, why) in refused {
            let body = format!("[{}]", rec(id, *patch));
            assert_eq!(verdict(&body), Some(*why), "{patch:?}");
        }
        // Boundary pairs: the last accepted value, then the first refused.
        let pairs: &[(&str, &str, &str, &str)] = &[
            ("ECCENTRICITY", "0.998", "0.999", "ECCENTRICITY"),
            ("ECCENTRICITY", "0", "-0.0001", "ECCENTRICITY"),
            ("MEAN_MOTION", "20.0", "20.0001", "MEAN_MOTION"),
            ("MEAN_MOTION", "0.0501", "0.05", "MEAN_MOTION"),
            ("INCLINATION", "180", "180.0001", "INCLINATION"),
            ("INCLINATION", "0", "-0.0001", "INCLINATION"),
            ("EPOCH", "\"1957-01-01T00:00:00\"", "\"1956-12-31T23:59:59\"", "EPOCH"),
            ("EPOCH", "\"2100-12-31T23:59:59\"", "\"2101-01-01T00:00:00\"", "EPOCH"),
            ("BSTAR", "0.9999", "1.0", "BSTAR"),
            ("BSTAR", "-0.9999", "-1.0", "BSTAR"),
            ("MEAN_MOTION_DOT", "0.9999", "1.0", "MEAN_MOTION_DOT"),
            ("MEAN_MOTION_DDOT", "0.9999", "1.0", "MEAN_MOTION_DDOT"),
        ];
        for (key, ok, bad, why) in pairs {
            assert_eq!(verdict(&format!("[{}]", rec("12", (key, ok)))), None, "{key} = {ok} accepted");
            assert_eq!(verdict(&format!("[{}]", rec("12", (key, bad)))), Some(*why), "{key} = {bad} refused");
        }
        assert_eq!(verdict(&format!("[{}]", GOOD.replace("@ID@", "999999999"))), None);
        assert_eq!(verdict(&format!("[{}]", GOOD.replace("@ID@", "1000000000"))), Some("NORAD_CAT_ID"));

        // Whole-body outcomes.
        let truncated = format!("[{}", GOOD.replace("@ID@", "9"));
        assert!(matches!(parse_omm(truncated.as_bytes()), Err(OmmError::Malformed(_))));
        let empty = parse_omm(b"[]").expect("an empty array is fine");
        assert_eq!((empty.records.len(), empty.skipped), (0, 0));
        // The 10 MB name is capped, not rejected, and the BOM body parses.
        let huge_name = format!("\"{}\"", "N".repeat(10 * 1024 * 1024));
        let r = parse_omm(format!("[{}]", rec("6", ("OBJECT_NAME", &huge_name))).as_bytes()).expect("a long name");
        assert_eq!(r.records[0].object_name.chars().count(), MAX_STR_CHARS);
        let bom = format!("\u{feff}[{}]", GOOD.replace("@ID@", "10"));
        assert_eq!(parse_omm(bom.as_bytes()).expect("BOM").records.len(), 1);
        // Non-object elements are skipped and counted.
        let mixed = format!("[1, \"x\", null, [1,[2]], {}]", GOOD.replace("@ID@", "11"));
        let r = parse_omm(mixed.as_bytes()).expect("parses");
        assert_eq!((r.records.len(), r.skipped, r.reasons["not an object"]), (1, 4, 4));
        // Unknown keys are ignored, a repeated key's last value wins.
        let extra = GOOD.replace("@ID@", "13").replace("{\"OBJECT_NAME\"", "{\"X\":{\"deep\":[1,2]},\"INCLINATION\":999,\"OBJECT_NAME\"");
        let r = parse_omm(format!("[{extra}]").as_bytes()).expect("parses");
        assert_eq!(r.records.len(), 1);
        assert!((r.records[0].inclination_deg - 51.6).abs() < 1e-12);
    }

    #[test]
    fn mutated_fixtures_never_panic() {
        // A cheap deterministic fuzz: flip, delete and duplicate bytes of the
        // real fixture, 3,000 times, and require Ok or Err, never a panic,
        // and every accepted record inside the limits.
        let mut x: u64 = 0x5EED;
        let mut next = move || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as usize
        };
        for _ in 0..3_000 {
            let mut b = STATIONS.to_vec();
            for _ in 0..1 + next() % 8 {
                let i = next() % b.len();
                match next() % 3 {
                    0 => b[i] = (next() % 256) as u8,
                    1 => {
                        b.remove(i);
                    }
                    _ => b.insert(i, b[i]),
                }
            }
            let out = std::panic::catch_unwind(|| parse_omm(&b)).expect("parse_omm panicked");
            if let Ok(r) = out {
                for x in &r.records {
                    assert!(x.mean_motion > 0.05 && x.mean_motion <= 20.0);
                    assert!((0.0..0.999).contains(&x.eccentricity) && (0.0..=180.0).contains(&x.inclination_deg));
                    assert!([x.raan_deg, x.arg_pericenter_deg, x.mean_anomaly_deg].iter().all(|a| (0.0..360.0).contains(a)));
                    assert!(x.bstar.abs() < 1.0 && x.norad_id >= 1 && x.object_name.chars().count() <= MAX_STR_CHARS);
                }
            }
        }
    }

    #[test]
    fn a_wrapped_angle_is_never_360() {
        assert_eq!(wrap_deg(-1e-20), 0.0);
        assert_eq!((-1e-20f64).rem_euclid(360.0), 360.0, "the case wrap_deg exists for");
        assert_eq!(wrap_deg(360.0), 0.0);
        assert_eq!(wrap_deg(-90.0), 270.0);
        assert_eq!(wrap_deg(719.5), 359.5);
    }

    /// Peak-memory probes on hostile bodies at the byte cap. Run each alone
    /// under `/usr/bin/time -l` (`-- --ignored --exact orbit::omm::tests::hostile_memory_...`);
    /// `hostile_memory_baseline` builds the same 32 MB body and does not parse it.
    fn body_of_zeros() -> Vec<u8> {
        // `[[0,0,0,...]]`: one element, just under the byte cap.
        let n = (MAX_BODY_BYTES as usize - 8) / 2;
        let mut b = Vec::with_capacity(MAX_BODY_BYTES as usize);
        b.extend_from_slice(b"[[0");
        for _ in 1..n {
            b.extend_from_slice(b",0");
        }
        b.extend_from_slice(b"]]");
        b
    }

    #[test]
    #[ignore = "measurement: run alone under /usr/bin/time -l"]
    fn hostile_memory_baseline() {
        let b = body_of_zeros();
        std::hint::black_box(&b);
        println!("body {} B", b.len());
    }

    #[test]
    #[ignore = "measurement: run alone under /usr/bin/time -l"]
    fn hostile_memory_nested_zeros() {
        let b = body_of_zeros();
        let r = parse_omm(&b).expect("parses");
        println!("body {} B, skipped {}", b.len(), r.skipped);
    }

    #[test]
    #[ignore = "measurement: run alone under /usr/bin/time -l"]
    fn hostile_memory_max_records_max_names() {
        // 50,000 valid records (the cap), each keeping as much text as fits
        // under the byte cap: a 64-char name of 4-byte chars and a 64-char
        // id of 2-byte chars (384 B of kept text a record).
        let name: String = std::iter::repeat_n('\u{1F6F0}', 64).collect();
        let id: String = std::iter::repeat_n('\u{E9}', 64).collect();
        let one = format!(
            r#"{{"OBJECT_NAME":"{name}","OBJECT_ID":"{id}","EPOCH":"2026-09-28T11:09:16","MEAN_MOTION":15.5,"ECCENTRICITY":0.001,"INCLINATION":51.6,"RA_OF_ASC_NODE":1,"ARG_OF_PERICENTER":2,"MEAN_ANOMALY":3,"NORAD_CAT_ID":@N@}}"#
        );
        let mut b = Vec::with_capacity(MAX_BODY_BYTES as usize);
        b.push(b'[');
        for i in 0..MAX_RECORDS {
            if i > 0 {
                b.push(b',');
            }
            b.extend_from_slice(one.replace("@N@", &(i + 1).to_string()).as_bytes());
        }
        b.push(b']');
        assert!(b.len() as u64 <= MAX_BODY_BYTES, "{}", b.len());
        let r = parse_omm(&b).expect("parses");
        println!("body {} B, records {} ({} B each)", b.len(), r.records.len(), std::mem::size_of::<OmmRecord>());
        assert_eq!(std::mem::size_of::<OmmRecord>(), 136, "update the # Memory arithmetic");
    }

    #[test]
    #[ignore = "measurement: run alone under /usr/bin/time -l"]
    fn hostile_memory_one_escaped_string() {
        // One name of `\u0041` escapes to the cap: serde_json must unescape it
        // into its scratch buffer (the one allocation that scales with B).
        // Built in place, one 32 MB allocation, like `body_of_zeros`, so
        // `hostile_memory_baseline` is its baseline too.
        let (head, tail) = GOOD.replace("@ID@", "1").split_once("\"X\"").map(|(h, t)| (h.to_owned(), t.to_owned())).unwrap();
        let n = (MAX_BODY_BYTES as usize - head.len() - tail.len() - 4) / 6;
        let mut b = Vec::with_capacity(MAX_BODY_BYTES as usize);
        b.push(b'[');
        b.extend_from_slice(head.as_bytes());
        b.push(b'"');
        for _ in 0..n {
            b.extend_from_slice(b"\\u0041");
        }
        b.push(b'"');
        b.extend_from_slice(tail.as_bytes());
        b.push(b']');
        let r = parse_omm(&b).expect("parses");
        println!("body {} B, records {}", b.len(), r.records.len());
    }

    #[test]
    fn more_than_fifty_thousand_records_is_refused() {
        let one = r#"{"NORAD_CAT_ID":1}"#;
        let body = format!("[{}]", vec![one; MAX_RECORDS + 1].join(","));
        assert!(matches!(parse_omm(body.as_bytes()), Err(OmmError::TooManyRecords)));
    }

    #[test]
    fn a_body_over_the_cap_is_refused_whole() {
        let big = vec![b' '; (MAX_BODY_BYTES + 1) as usize];
        assert!(matches!(parse_omm_reader(&big[..]), Err(OmmError::TooLarge)));
    }

    #[test]
    fn control_characters_are_stripped_from_names() {
        let body = br#"[{"OBJECT_NAME":"A\u0007B\nC","EPOCH":"2026-09-28T11:09:16","MEAN_MOTION":15.5,"ECCENTRICITY":0.001,
          "INCLINATION":51.6,"RA_OF_ASC_NODE":361,"ARG_OF_PERICENTER":-2,"MEAN_ANOMALY":3,"NORAD_CAT_ID":5}]"#;
        let r = parse_omm(body).expect("parses");
        assert_eq!(&*r.records[0].object_name, "ABC");
        assert!((r.records[0].raan_deg - 1.0).abs() < 1e-12, "angles are taken mod 360");
        assert!((r.records[0].arg_pericenter_deg - 358.0).abs() < 1e-12);
    }
}
