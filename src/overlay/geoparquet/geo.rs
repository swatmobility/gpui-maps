//! The `geo` file metadata: the GeoParquet version, the primary geometry
//! column, its encoding, CRS, extent and (1.1) bbox covering column.
//! <https://geoparquet.org/releases/v1.1.0/>

use serde_json::Value;

use super::GeoParquetError;

/// How the geometry column is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoParquetEncoding {
    /// Well-known binary in a binary column (every version).
    Wkb,
    /// GeoArrow native (1.1): a struct of `x`/`y` per point, nested in
    /// lists for the other types.
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl GeoParquetEncoding {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "wkb" => Self::Wkb,
            "point" => Self::Point,
            "linestring" => Self::LineString,
            "polygon" => Self::Polygon,
            "multipoint" => Self::MultiPoint,
            "multilinestring" => Self::MultiLineString,
            "multipolygon" => Self::MultiPolygon,
            _ => return None,
        })
    }

    /// The name as the metadata writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Wkb => "WKB",
            Self::Point => "point",
            Self::LineString => "linestring",
            Self::Polygon => "polygon",
            Self::MultiPoint => "multipoint",
            Self::MultiLineString => "multilinestring",
            Self::MultiPolygon => "multipolygon",
        }
    }
}

/// A 1.1 bbox covering: the column paths of the four per-row bounds, each
/// `[struct column, field]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoParquetCovering {
    /// The struct column (`bbox`).
    pub column: String,
    /// The fields of `xmin`, `ymin`, `xmax`, `ymax`, in that order.
    pub fields: [String; 4],
}

/// The parts of the `geo` metadata this reader uses.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoParquetMetadata {
    /// `1.0.0`, `1.1.0`, ...
    pub version: String,
    /// The primary geometry column's name.
    pub primary_column: String,
    pub encoding: GeoParquetEncoding,
    /// The writer's declared geometry types (`Point`, `Polygon Z`, ...);
    /// empty means any.
    pub geometry_types: Vec<String>,
    /// The column's extent `[xmin, ymin, xmax, ymax]`, when given.
    pub bbox: Option<[f64; 4]>,
    /// The bbox covering column, when the writer added one (1.1).
    pub covering: Option<GeoParquetCovering>,
    /// The CRS as written: `None` when absent (the spec's default, OGC
    /// CRS84), `Some(Value::Null)` when explicitly unknown.
    pub crs: Option<Value>,
}

impl GeoParquetMetadata {
    /// Parses the `geo` key's JSON and checks the version.
    pub(crate) fn parse(json: &str) -> Result<Self, GeoParquetError> {
        let bad = |what: &str| GeoParquetError::NotGeoParquet(format!("the `geo` metadata {what}"));
        let v: Value = serde_json::from_str(json).map_err(|e| bad(&format!("is not JSON ({e})")))?;
        let version = v.get("version").and_then(Value::as_str).ok_or_else(|| bad("has no version"))?.to_string();
        check_version(&version)?;
        let primary_column = v.get("primary_column").and_then(Value::as_str).ok_or_else(|| bad("has no primary_column"))?.to_string();
        let column = v
            .get("columns")
            .and_then(|c| c.get(&primary_column))
            .ok_or_else(|| bad(&format!("does not describe its primary column {primary_column:?}")))?;
        let encoding_name = column.get("encoding").and_then(Value::as_str).ok_or_else(|| bad("names no encoding"))?;
        let encoding = GeoParquetEncoding::parse(encoding_name)
            .ok_or_else(|| GeoParquetError::Unsupported(format!("geometry encoding {encoding_name:?}")))?;
        let geometry_types = column
            .get("geometry_types")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let bbox = column.get("bbox").and_then(Value::as_array).and_then(|a| {
            let n: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
            match n.len() {
                4 => Some([n[0], n[1], n[2], n[3]]),
                // 3D: [xmin, ymin, zmin, xmax, ymax, zmax].
                6 => Some([n[0], n[1], n[3], n[4]]),
                _ => None,
            }
        });
        let covering = column.get("covering").and_then(|c| c.get("bbox")).and_then(|b| {
            let path = |k: &str| -> Option<(String, String)> {
                let p = b.get(k)?.as_array()?;
                match p.as_slice() {
                    [Value::String(col), Value::String(field)] => Some((col.clone(), field.clone())),
                    _ => None,
                }
            };
            let parts = [path("xmin")?, path("ymin")?, path("xmax")?, path("ymax")?];
            let column = parts[0].0.clone();
            parts.iter().all(|(c, _)| *c == column).then(|| GeoParquetCovering {
                column,
                fields: parts.map(|(_, f)| f),
            })
        });
        Ok(Self { version, primary_column, encoding, geometry_types, bbox, covering, crs: column.get("crs").cloned() })
    }

    /// The CRS check: `Ok((description, warning))` to read, `Err` to
    /// refuse. Absent is OGC:CRS84 by the spec; `null` is unknown (read as
    /// longitude/latitude with a warning, refused when the extent plainly
    /// is not).
    pub(crate) fn check_crs(&self, accept_any_crs: bool) -> Result<(String, Option<String>), GeoParquetError> {
        let Some(crs) = &self.crs else { return Ok(("OGC:CRS84 (by default)".into(), None)) };
        if crs.is_null() {
            let projected = self.bbox.is_some_and(|e| e[0] < -180.5 || e[2] > 180.5 || e[1] < -90.5 || e[3] > 90.5);
            if projected && !accept_any_crs {
                return Err(GeoParquetError::Crs(format!(
                    "unknown (null), and the extent {:?} is not longitude/latitude",
                    self.bbox.unwrap_or_default()
                )));
            }
            return Ok(("unknown".into(), Some("the CRS is null (unknown): coordinates read as longitude/latitude".into())));
        }
        let describe = describe(crs);
        if is_wgs84(crs) {
            return Ok((describe, None));
        }
        if accept_any_crs {
            let warning = format!("CRS {describe} drawn as longitude/latitude (accept_any_crs)");
            return Ok((describe, Some(warning)));
        }
        Err(GeoParquetError::Crs(describe))
    }
}

/// 1.0.x and 1.1.x (betas and release candidates included); anything else
/// is refused by number.
fn check_version(version: &str) -> Result<(), GeoParquetError> {
    let mut parts = version.split(['.', '-']);
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
    match (major, minor) {
        (Some(1), Some(0 | 1)) => Ok(()),
        _ => Err(GeoParquetError::Unsupported(format!("GeoParquet version {version} (this reader reads 1.0 and 1.1)"))),
    }
}

/// `{"authority": "EPSG", "code": 4326}` -> `("EPSG", "4326")`.
fn id_of(crs: &Value) -> Option<(String, String)> {
    let id = crs.get("id")?;
    let authority = id.get("authority")?.as_str()?.to_string();
    let code = match id.get("code")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some((authority, code))
}

fn is_wgs84(crs: &Value) -> bool {
    let wgs84 = |authority: &str, code: &str| {
        let (a, c) = (authority.to_ascii_uppercase(), code.to_ascii_uppercase());
        (a == "EPSG" && c == "4326") || (a == "OGC" && (c == "CRS84" || c == "84"))
    };
    match crs {
        // A writer that put the identifier where PROJJSON belongs.
        Value::String(s) => {
            let s = s.trim().to_ascii_uppercase();
            s == "EPSG:4326" || s == "OGC:CRS84" || s == "CRS84" || s.ends_with("/OGC/1.3/CRS84")
        }
        _ => id_of(crs).is_some_and(|(a, c)| wgs84(&a, &c)),
    }
}

fn describe(crs: &Value) -> String {
    if let Value::String(s) = crs {
        return s.clone();
    }
    let name = crs.get("name").and_then(Value::as_str);
    match (id_of(crs), name) {
        (Some((a, c)), Some(n)) => format!("{a}:{c} ({n})"),
        (Some((a, c)), None) => format!("{a}:{c}"),
        (None, Some(n)) => n.to_string(),
        (None, None) => "an unnamed CRS".into(),
    }
}
