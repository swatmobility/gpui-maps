//! The TIFF container, read from a byte prefix: the header (classic or
//! BigTIFF, either byte order), the IFD chain, and the tags a tiled image
//! needs. Sans-IO: [`parse`] either answers or says how many leading bytes
//! of the file it needs ([`Parse::NeedPrefix`]), and the caller fetches
//! them. A cloud-optimised GeoTIFF keeps every IFD and its arrays at the
//! start of the file, so one or two prefix reads open it.

use super::CogError;

/// A value of one tag, widened: integers to `u64`, reals to `f64`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Ints(Vec<u64>),
    Reals(Vec<f64>),
    Ascii(String),
    Bytes(Vec<u8>),
}

impl Value {
    pub(crate) fn ints(&self) -> Option<&[u64]> {
        match self {
            Value::Ints(v) => Some(v),
            _ => None,
        }
    }
    pub(crate) fn int(&self) -> Option<u64> {
        self.ints().and_then(|v| v.first().copied())
    }
    pub(crate) fn reals(&self) -> Option<Vec<f64>> {
        match self {
            Value::Reals(v) => Some(v.clone()),
            Value::Ints(v) => Some(v.iter().map(|&x| x as f64).collect()),
            _ => None,
        }
    }
}

/// One image file directory: its tags by number.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ifd {
    pub tags: std::collections::BTreeMap<u16, Value>,
}

impl Ifd {
    pub(crate) fn get(&self, tag: u16) -> Option<&Value> {
        self.tags.get(&tag)
    }
    pub(crate) fn int(&self, tag: u16) -> Option<u64> {
        self.get(tag).and_then(Value::int)
    }
}

pub(crate) mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const PLANAR_CONFIG: u16 = 284;
    pub const PREDICTOR: u16 = 317;
    pub const COLOR_MAP: u16 = 320;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const EXTRA_SAMPLES: u16 = 338;
    pub const SAMPLE_FORMAT: u16 = 339;
    pub const JPEG_TABLES: u16 = 347;
    pub const MODEL_PIXEL_SCALE: u16 = 33550;
    pub const MODEL_TIEPOINT: u16 = 33922;
    pub const MODEL_TRANSFORMATION: u16 = 34264;
    pub const GEO_KEY_DIRECTORY: u16 = 34735;
    pub const GEO_DOUBLE_PARAMS: u16 = 34736;
    pub const GEO_ASCII_PARAMS: u16 = 34737;
    pub const GDAL_METADATA: u16 = 42112;
    pub const GDAL_NODATA: u16 = 42113;
}

/// [`parse`]'s answer.
#[derive(Debug)]
pub(crate) enum Parse {
    /// Every IFD in the chain, in file order.
    Done { ifds: Vec<Ifd>, little_endian: bool },
    /// The file's first `n` bytes are needed (more than were given).
    NeedPrefix(u64),
}

struct Cursor<'a> {
    bytes: &'a [u8],
    le: bool,
}

/// Out of the prefix: the caller must read up to `end`.
struct Short(u64);

impl Cursor<'_> {
    fn slice(&self, at: u64, len: u64) -> Result<&[u8], Short> {
        let end = at.checked_add(len).ok_or(Short(u64::MAX))?;
        if end > self.bytes.len() as u64 {
            return Err(Short(end));
        }
        Ok(&self.bytes[at as usize..end as usize])
    }
    fn u16(&self, at: u64) -> Result<u16, Short> {
        let b = self.slice(at, 2)?;
        Ok(if self.le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    }
    fn u32(&self, at: u64) -> Result<u32, Short> {
        let b: [u8; 4] = self.slice(at, 4)?.try_into().expect("4 bytes");
        Ok(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn u64(&self, at: u64) -> Result<u64, Short> {
        let b: [u8; 8] = self.slice(at, 8)?.try_into().expect("8 bytes");
        Ok(if self.le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) })
    }
}

/// The byte size of one value of TIFF field type `ty`, `None` for a type
/// this reader does not know (the entry is skipped, as the spec says).
fn type_size(ty: u16) -> Option<u64> {
    Some(match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return None,
    })
}

/// IFD chains longer than this are refused as a loop or garbage.
const MAX_IFDS: usize = 64;
/// One tag's values beyond this many bytes are refused (a tile offset
/// array of a 1M x 1M px image at 256 px tiles is 128 MB; nothing real
/// reaches 64 MB in one IFD).
const MAX_TAG_BYTES: u64 = 64 << 20;

/// Reads the header and the whole IFD chain from `prefix` (the file's
/// first bytes).
pub(crate) fn parse(prefix: &[u8]) -> Result<Parse, CogError> {
    match parse_inner(prefix) {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(Short(end))) => Ok(Parse::NeedPrefix(end)),
        Err(e) => Err(e),
    }
}

fn parse_inner(prefix: &[u8]) -> Result<Result<Parse, Short>, CogError> {
    if prefix.len() < 8 {
        return Ok(Err(Short(16)));
    }
    let le = match &prefix[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(CogError::NotTiff("no II/MM byte-order mark".into())),
    };
    let c = Cursor { bytes: prefix, le };
    let magic = match c.u16(2) {
        Ok(m) => m,
        Err(s) => return Ok(Err(s)),
    };
    let big = match magic {
        42 => false,
        43 => true,
        other => return Err(CogError::NotTiff(format!("magic {other}, not 42 (TIFF) or 43 (BigTIFF)"))),
    };
    let first = if big {
        match c.u64(8) {
            Ok(v) => v,
            Err(s) => return Ok(Err(s)),
        }
    } else {
        u64::from(match c.u32(4) {
            Ok(v) => v,
            Err(s) => return Ok(Err(s)),
        })
    };
    let mut ifds = Vec::new();
    let mut next = first;
    let mut seen = std::collections::HashSet::new();
    while next != 0 {
        if ifds.len() >= MAX_IFDS || !seen.insert(next) {
            return Err(CogError::NotTiff("the IFD chain loops or exceeds 64 images".into()));
        }
        let (ifd, after) = match read_ifd(&c, next, big)? {
            Ok(v) => v,
            Err(s) => return Ok(Err(s)),
        };
        ifds.push(ifd);
        next = after;
    }
    if ifds.is_empty() {
        return Err(CogError::NotTiff("no image file directory".into()));
    }
    Ok(Ok(Parse::Done { ifds, little_endian: le }))
}

fn read_ifd(c: &Cursor, at: u64, big: bool) -> Result<Result<(Ifd, u64), Short>, CogError> {
    let (count, entries_at, entry_size) = if big {
        match c.u64(at) {
            Ok(n) => (n, at + 8, 20u64),
            Err(s) => return Ok(Err(s)),
        }
    } else {
        match c.u16(at) {
            Ok(n) => (u64::from(n), at + 2, 12u64),
            Err(s) => return Ok(Err(s)),
        }
    };
    if count > 4096 {
        return Err(CogError::NotTiff(format!("an IFD claims {count} entries")));
    }
    let next_at = entries_at + count * entry_size;
    // Ask for the whole directory and the next-IFD pointer at once.
    if let Err(s) = c.slice(entries_at, count * entry_size + if big { 8 } else { 4 }) {
        return Ok(Err(s));
    }
    let mut ifd = Ifd::default();
    for i in 0..count {
        let e = entries_at + i * entry_size;
        let read = || -> Result<Option<(u16, Value)>, Short> {
            let tag = c.u16(e)?;
            let ty = c.u16(e + 2)?;
            let n = if big { c.u64(e + 4)? } else { u64::from(c.u32(e + 4)?) };
            let Some(size) = type_size(ty) else { return Ok(None) };
            let total = n.saturating_mul(size);
            let inline = if big { 8 } else { 4 };
            let data_at = if total <= inline { e + if big { 12 } else { 8 } } else if big { c.u64(e + 12)? } else { u64::from(c.u32(e + 8)?) };
            if total > MAX_TAG_BYTES {
                return Ok(None);
            }
            let raw = c.slice(data_at, total)?;
            Ok(Some((tag, decode_value(raw, ty, n, c.le))))
        };
        match read() {
            Ok(Some((tag, value))) => {
                ifd.tags.insert(tag, value);
            }
            Ok(None) => {}
            Err(s) => return Ok(Err(s)),
        }
    }
    let next = if big {
        match c.u64(next_at) {
            Ok(v) => v,
            Err(s) => return Ok(Err(s)),
        }
    } else {
        match c.u32(next_at) {
            Ok(v) => u64::from(v),
            Err(s) => return Ok(Err(s)),
        }
    };
    Ok(Ok((ifd, next)))
}

fn decode_value(raw: &[u8], ty: u16, n: u64, le: bool) -> Value {
    let n = n as usize;
    let u16_at = |i: usize| {
        let b = [raw[i], raw[i + 1]];
        if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
    };
    let u32_at = |i: usize| {
        let b: [u8; 4] = raw[i..i + 4].try_into().expect("4");
        if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
    };
    let u64_at = |i: usize| {
        let b: [u8; 8] = raw[i..i + 8].try_into().expect("8");
        if le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) }
    };
    match ty {
        1 => Value::Ints(raw.iter().map(|&b| u64::from(b)).collect()),
        6 => Value::Ints(raw.iter().map(|&b| b as i8 as i64 as u64).collect()),
        7 => Value::Bytes(raw.to_vec()),
        2 => {
            let text = raw.split(|&b| b == 0).next().unwrap_or_default();
            Value::Ascii(String::from_utf8_lossy(text).into_owned())
        }
        3 => Value::Ints((0..n).map(|i| u64::from(u16_at(i * 2))).collect()),
        8 => Value::Ints((0..n).map(|i| u16_at(i * 2) as i16 as i64 as u64).collect()),
        4 | 13 => Value::Ints((0..n).map(|i| u64::from(u32_at(i * 4))).collect()),
        9 => Value::Ints((0..n).map(|i| u32_at(i * 4) as i32 as i64 as u64).collect()),
        16 | 18 => Value::Ints((0..n).map(|i| u64_at(i * 8)).collect()),
        17 => Value::Ints((0..n).map(|i| u64_at(i * 8)).collect()),
        5 => Value::Reals((0..n).map(|i| f64::from(u32_at(i * 8)) / f64::from(u32_at(i * 8 + 4)).max(1.0)).collect()),
        10 => Value::Reals((0..n).map(|i| f64::from(u32_at(i * 8) as i32) / f64::from(u32_at(i * 8 + 4) as i32)).collect()),
        11 => Value::Reals((0..n).map(|i| f64::from(f32::from_bits(u32_at(i * 4)))).collect()),
        12 => Value::Reals((0..n).map(|i| f64::from_bits(u64_at(i * 8))).collect()),
        _ => Value::Bytes(raw.to_vec()),
    }
}
