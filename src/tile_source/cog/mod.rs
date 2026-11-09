//! Cloud-Optimised GeoTIFF (COG) as a RASTER tile source: a sibling of
//! the imagery `HttpTileSource` and an MBTiles raster file, drawn through
//! the same [`MapView::set_raster_source`] path -- the same cover, the same
//! texture cache, the same credit line.
//!
//! A COG is a GeoTIFF whose pixels are cut into internal tiles, with
//! reduced-resolution copies (overviews), and whose directories sit at
//! the start of the file. Opening one reads that header (one or two range
//! requests); after that, any map tile at any zoom is made from the few
//! internal tiles of the overview that matches it:
//!
//! 1. **pick the level**: the coarsest overview whose pixels are no larger
//!    than the map tile's (see [`CogMetadata::level_for`]);
//! 2. **find the internal tiles** the map tile overlaps (one pixel of
//!    margin, for bilinear sampling across a tile edge);
//! 3. **fetch the missing ones**: cached decoded tiles are reused (a
//!    bounded LRU, [`CogOptions::cache_bytes`]); the rest are read as byte
//!    ranges, adjacent ranges COALESCED into one request
//!    ([`CogOptions::merge_gap`]) and the requests issued CONCURRENTLY;
//! 4. **decode** (deflate, LZW, JPEG, WebP, PackBits or none; horizontal and
//!    floating-point predictors) and **resample** into an RGBA map tile in
//!    Web Mercator, off the UI thread.
//!
//! # What it reads
//!
//! - **CRS**: EPSG:3857 (and its aliases 900913/3785/102100/102113) maps
//!   to map tiles linearly; geographic EPSG:4326 is resampled row by row
//!   (latitude is the only non-linear axis). EPSG:4269 (NAD83) and 4258
//!   (ETRS89) are read as WGS84 -- they differ by about a metre, under
//!   one pixel of any 1-arc-second DEM -- with a warning. Every other CRS
//!   is refused by its EPSG code ([`CogError::Crs`]); a rotated or sheared
//!   geotransform likewise.
//! - **Pixels**: 8-bit RGB / RGBA (or JPEG YCbCr) draws as imagery.
//!   Anything else -- one band of u8/i8/u16/i16/u32/i32/f32/f64 -- is
//!   coloured by a ramp ([`CogRamp`]) between `min` and `max`, taken from
//!   [`CogOptions`], else from the file's GDAL statistics
//!   (`STATISTICS_MINIMUM`/`MAXIMUM`), else refused at open with a message
//!   naming the options. `nodata` (options, else `GDAL_NODATA`) and NaN
//!   draw transparent.
//! - **Compression** other than the six above is refused by name (ZSTD,
//!   LERC, LZMA, JPEG 2000, CCITT...).
//!
//! # Opening
//!
//! - [`CogSource::open`] takes a path or an http(s) URL (async; native
//!   reads files by seek and URLs through this crate's `reqwest` client
//!   with the project User-Agent; the browser build reads URLs with
//!   `fetch`). [`CogSource::open_blocking`] for native callers outside a
//!   runtime.
//! - [`CogSource::from_bytes`] reads an in-memory file (every target).
//!
//! # Example
//!
//! ```
//! use gpui_maps::tile_source::cog::{write, CogOptions, CogSource};
//!
//! // A 512 x 512 single-band f32 COG in EPSG:3857 with two overviews.
//! let values: Vec<f32> = (0..512 * 512).map(|i| (i % 512) as f32).collect();
//! let bytes = write::write(&write::WriteImage {
//!     width: 512,
//!     height: 512,
//!     samples: write::WriteSamples::F32(values, 1),
//!     ..write::WriteImage::web_mercator_square(0.0, 0.0, 1000.0)
//! });
//! let cog = CogSource::from_bytes(bytes, CogOptions::default().with_range(0.0, 511.0)).unwrap();
//! let meta = cog.metadata();
//! assert_eq!((meta.width, meta.height), (512, 512));
//! assert_eq!(meta.overviews.len(), 2);
//! ```
//!
//! [`MapView::set_raster_source`]: crate::map_view::MapView::set_raster_source

mod decode;
pub mod ranges;
mod render;
mod tiff;
pub mod write;

#[cfg(all(not(target_arch = "wasm32"), not(feature = "cog_ureq")))]
mod fetch_native;
// The iOS crate's reads (`ureq`, no tokio): it includes this module by
// `#[path]` with `cog_ureq` on. Declared, and off, in every crate that
// reaches this source.
#[cfg(all(not(target_arch = "wasm32"), feature = "cog_ureq"))]
mod fetch_ureq;
#[cfg(target_arch = "wasm32")]
mod fetch_web;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::geo::TileId;
use tiff::{tag, Ifd, Parse, Value};

pub use ranges::{coalesce, ByteRange, RangeGroup};

/// The User-Agent a COG fetch sends: the project and its repository,
/// nothing about the user.
pub const COG_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");

/// Web Mercator's sphere radius (EPSG:3857), metres.
const EARTH_RADIUS: f64 = 6_378_137.0;
/// The deepest zoom a COG reports (`tile_source::DEFAULT_MAX_ZOOM`).
const MAX_ZOOM: u8 = 22;
/// Half the Web Mercator world's edge, metres.
const HALF_WORLD: f64 = std::f64::consts::PI * EARTH_RADIUS;
/// Web Mercator's latitude limit.
const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Why a COG could not be opened or a tile not made.
#[derive(Debug, Clone, PartialEq)]
pub enum CogError {
    /// Not a TIFF (byte-order mark, magic or IFD chain wrong).
    NotTiff(String),
    /// A TIFF this reader does not draw: stripped rather than tiled, a
    /// data type, a layout or a compression it does not read (named).
    Unsupported(String),
    /// The CRS is not one this reader maps (named by its EPSG code).
    Crs(String),
    /// Single-band data with no `min`/`max` in the options or the file.
    NoRange,
    /// A tile's bytes did not decode.
    Decode(String),
    /// More than a stated limit.
    TooLarge { limit: u64, got: u64 },
    /// Reading a file failed.
    Io(String),
    /// A URL fetch failed (status or transport; never the URL itself).
    Http(String),
    /// A URL fetch passed [`CogOptions::timeout`].
    Timeout,
}

impl std::fmt::Display for CogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTiff(e) => write!(f, "not a TIFF: {e}"),
            Self::Unsupported(e) => write!(f, "unsupported GeoTIFF: {e}"),
            Self::Crs(e) => write!(f, "unsupported CRS: {e}; reproject it (gdalwarp -t_srs EPSG:3857 -of COG)"),
            Self::NoRange => write!(
                f,
                "single-band data needs a value range: set CogOptions::with_range(min, max), or write GDAL statistics \
                 into the file (gdalinfo -stats)"
            ),
            Self::Decode(e) => write!(f, "a tile did not decode: {e}"),
            Self::TooLarge { limit, got } => write!(f, "{got} bytes is over the {limit}-byte limit"),
            Self::Io(e) => write!(f, "read failed: {e}"),
            Self::Http(e) => write!(f, "fetch failed: {e}"),
            Self::Timeout => write!(f, "fetch timed out"),
        }
    }
}

impl std::error::Error for CogError {}

/// A colour ramp for single-band data: `(t, rgba)` stops over `t` in
/// `0..=1` (`min` to `max`), interpolated linearly.
#[derive(Debug, Clone, PartialEq)]
pub enum CogRamp {
    /// Hypsometric: lowland green, through tan and brown, to grey rock and
    /// white summits. For elevation.
    Terrain,
    /// Matplotlib's viridis (CC0), five stops. For any continuous field.
    Viridis,
    /// Black to white.
    Greys,
    /// Red - yellow - green, for an index such as NDVI.
    RedYellowGreen,
    /// Caller's stops, `t` ascending in `0..=1`.
    Stops(Vec<(f32, [u8; 4])>),
}

impl CogRamp {
    fn stops(&self) -> Vec<(f32, [u8; 4])> {
        match self {
            Self::Terrain => vec![
                (0.0, [36, 110, 66, 255]),
                (0.18, [94, 150, 76, 255]),
                (0.36, [214, 200, 128, 255]),
                (0.55, [168, 112, 62, 255]),
                (0.75, [128, 100, 92, 255]),
                (0.9, [196, 192, 190, 255]),
                (1.0, [255, 255, 255, 255]),
            ],
            Self::Viridis => vec![
                (0.0, [68, 1, 84, 255]),
                (0.25, [59, 82, 139, 255]),
                (0.5, [33, 145, 140, 255]),
                (0.75, [94, 201, 98, 255]),
                (1.0, [253, 231, 37, 255]),
            ],
            Self::Greys => vec![(0.0, [0, 0, 0, 255]), (1.0, [255, 255, 255, 255])],
            Self::RedYellowGreen => vec![(0.0, [215, 48, 39, 255]), (0.5, [255, 255, 191, 255]), (1.0, [26, 152, 80, 255])],
            Self::Stops(s) => s.clone(),
        }
    }

    /// 256 colours, `t = i / 255`.
    pub(crate) fn lut(&self) -> Vec<[u8; 4]> {
        let stops = self.stops();
        (0..256)
            .map(|i| {
                let t = i as f32 / 255.0;
                let Some(first) = stops.first() else { return [0, 0, 0, 0] };
                if t <= first.0 {
                    return first.1;
                }
                for w in stops.windows(2) {
                    let ((t0, c0), (t1, c1)) = (w[0], w[1]);
                    if t <= t1 {
                        let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
                        let mut c = [0u8; 4];
                        for k in 0..4 {
                            c[k] = (f32::from(c0[k]) + (f32::from(c1[k]) - f32::from(c0[k])) * f).round() as u8;
                        }
                        return c;
                    }
                }
                stops.last().map_or([0; 4], |s| s.1)
            })
            .collect()
    }
}

/// How a map pixel is sampled from the source pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CogResampling {
    /// The nearest source pixel: hard pixel edges when magnified.
    Nearest,
    /// The four nearest, weighted (premultiplied for imagery; NaN-aware
    /// for values, falling back to the nearest).
    #[default]
    Bilinear,
}

/// How a COG is read and coloured. `Default` reads 8-bit RGB(A) as
/// imagery and single-band data with the terrain ramp over the file's own
/// statistics.
#[derive(Debug, Clone)]
pub struct CogOptions {
    /// The band (0-based) a ramp colours. `None`: RGB when the file is
    /// 8-bit with three or four bands, else band 0.
    pub band: Option<usize>,
    /// The ramp for single-band data.
    pub ramp: CogRamp,
    /// The value at the ramp's start; `None` takes the file's
    /// `STATISTICS_MINIMUM`.
    pub min: Option<f64>,
    /// The value at the ramp's end; `None` takes `STATISTICS_MAXIMUM`.
    pub max: Option<f64>,
    /// The value drawn transparent; `None` takes the file's `GDAL_NODATA`.
    pub nodata: Option<f64>,
    /// The credit drawn for this source (see `TileSource::attribution`).
    pub attribution: Option<String>,
    /// Map tile edge in px this source renders (pass the same value to
    /// `set_raster_source`). 512 is this engine's grid.
    pub tile_size: u32,
    /// Sampling.
    pub resampling: CogResampling,
    /// Shade single-band data as terrain (values in metres), with this
    /// vertical exaggeration; `None` draws the ramp flat.
    pub hillshade: Option<f32>,
    /// Decoded internal tiles kept, in bytes (an LRU).
    pub cache_bytes: usize,
    /// Two byte ranges closer than this are fetched as one request.
    pub merge_gap: u64,
    /// A coalesced request is not grown past this.
    pub max_request_bytes: u64,
    /// Requests in flight at once, per map tile.
    pub max_concurrent: usize,
    /// Per request.
    pub timeout: Duration,
    /// A server that ignores `Range` sends the whole file: refused above
    /// this.
    pub max_whole_file_bytes: u64,
}

impl Default for CogOptions {
    fn default() -> Self {
        Self {
            band: None,
            ramp: CogRamp::Terrain,
            min: None,
            max: None,
            nodata: None,
            attribution: None,
            tile_size: 512,
            resampling: CogResampling::Bilinear,
            hillshade: None,
            cache_bytes: 128 << 20,
            merge_gap: 32 << 10,
            max_request_bytes: 8 << 20,
            max_concurrent: 8,
            timeout: Duration::from_secs(30),
            max_whole_file_bytes: 256 << 20,
        }
    }
}

impl CogOptions {
    /// The ramp's value range.
    #[must_use]
    pub fn with_range(mut self, min: f64, max: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }
    /// The ramp.
    #[must_use]
    pub fn with_ramp(mut self, ramp: CogRamp) -> Self {
        self.ramp = ramp;
        self
    }
    /// The band a ramp colours.
    #[must_use]
    pub fn with_band(mut self, band: usize) -> Self {
        self.band = Some(band);
        self
    }
    /// The value drawn transparent.
    #[must_use]
    pub fn with_nodata(mut self, nodata: f64) -> Self {
        self.nodata = Some(nodata);
        self
    }
    /// The credit drawn for this source.
    #[must_use]
    pub fn with_attribution(mut self, credit: impl Into<String>) -> Self {
        self.attribution = Some(credit.into());
        self
    }
    /// Hillshade the ramp (values in metres) with this vertical
    /// exaggeration.
    #[must_use]
    pub fn with_hillshade(mut self, exaggeration: f32) -> Self {
        self.hillshade = Some(exaggeration);
        self
    }
    /// The map tile edge in px.
    #[must_use]
    pub fn with_tile_size(mut self, px: u32) -> Self {
        self.tile_size = px;
        self
    }
}

/// A sample type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CogDataType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
}

impl CogDataType {
    /// Bytes per sample.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
    /// `uint8`, `float32`, ...
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::U8 => "uint8",
            Self::I8 => "int8",
            Self::U16 => "uint16",
            Self::I16 => "int16",
            Self::U32 => "uint32",
            Self::I32 => "int32",
            Self::F32 => "float32",
            Self::F64 => "float64",
        }
    }
}

/// A tile compression this reader decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CogCompression {
    None,
    Lzw,
    Jpeg,
    Deflate,
    PackBits,
    WebP,
}

impl CogCompression {
    fn from_code(code: u64) -> Result<Self, CogError> {
        Ok(match code {
            1 => Self::None,
            5 => Self::Lzw,
            7 => Self::Jpeg,
            8 | 32946 => Self::Deflate,
            32773 => Self::PackBits,
            50001 => Self::WebP,
            other => {
                let name = match other {
                    2 => "CCITT RLE",
                    3 => "CCITT Group 3",
                    4 => "CCITT Group 4",
                    6 => "old-style JPEG",
                    34712 => "JPEG 2000",
                    34887 => "LERC",
                    34925 => "LZMA",
                    50000 => "ZSTD",
                    50002 => "JPEG XL",
                    _ => "an unknown scheme",
                };
                return Err(CogError::Unsupported(format!(
                    "compression {name} (code {other}); this reader decodes none, deflate, LZW, JPEG, WebP and PackBits"
                )));
            }
        })
    }
    /// `deflate`, `LZW`, ...
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lzw => "LZW",
            Self::Jpeg => "JPEG",
            Self::Deflate => "deflate",
            Self::PackBits => "PackBits",
            Self::WebP => "WebP",
        }
    }
}

/// The file's coordinate reference system, as this reader maps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CogCrs {
    /// EPSG:3857 (the code the file states, an alias included).
    WebMercator { epsg: u32 },
    /// Longitude/latitude in degrees (4326, or 4269/4258 read as it).
    Geographic { epsg: u32 },
}

impl CogCrs {
    /// The EPSG code the file states.
    #[must_use]
    pub fn epsg(self) -> u32 {
        match self {
            Self::WebMercator { epsg } | Self::Geographic { epsg } => epsg,
        }
    }
}

/// How the pixels are drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum CogRendering {
    /// 8-bit colour, as is.
    Rgb { alpha: bool },
    /// One band through the ramp.
    Ramp { band: usize, min: f64, max: f64 },
}

/// One resolution level: the full image (index 0) or an overview.
#[derive(Debug, Clone, PartialEq)]
pub struct CogLevel {
    pub width: u32,
    pub height: u32,
    /// CRS units per pixel, x and y (positive).
    pub resolution: (f64, f64),
}

/// What a COG says about itself, read at open.
#[derive(Debug, Clone)]
pub struct CogMetadata {
    pub width: u32,
    pub height: u32,
    pub bands: usize,
    pub dtype: CogDataType,
    pub compression: CogCompression,
    pub crs: CogCrs,
    /// `[west, south, east, north]`, degrees.
    pub bounds: [f64; 4],
    /// The full-resolution level then the overviews, finest first.
    pub levels: Vec<CogLevel>,
    /// The overviews' sizes (the levels after the first).
    pub overviews: Vec<(u32, u32)>,
    /// Internal tile edge, px.
    pub tile_width: u32,
    pub tile_height: u32,
    /// The nodata value in force (options, else the file's).
    pub nodata: Option<f64>,
    /// The file's GDAL statistics for the drawn band, `(min, max)`.
    pub stats: Option<(f64, f64)>,
    pub rendering: CogRendering,
    /// The deepest map zoom with new detail (at [`CogOptions::tile_size`]).
    pub max_zoom: u8,
    /// The header bytes read at open, and the requests that took.
    pub header_bytes: u64,
    pub header_requests: u32,
    /// The file's size, when known.
    pub file_bytes: Option<u64>,
    /// Things read but approximated (a datum read as WGS84, ...).
    pub warnings: Vec<String>,
}

impl CogMetadata {
    /// The level (index into [`Self::levels`]) a map tile `tile_size` px
    /// wide at `zoom` reads: the coarsest whose pixels are no larger than
    /// the map's (5% slack, so a file cut on the Web Mercator grid
    /// matches exactly despite float error); the full image when every
    /// level is coarser.
    #[must_use]
    pub fn level_for(&self, zoom: u8, tile_size: u32) -> usize {
        let target = self.map_resolution(zoom, tile_size);
        for (i, level) in self.levels.iter().enumerate().rev() {
            if level.resolution.0 <= target * 1.05 {
                return i;
            }
        }
        0
    }

    /// CRS units per map pixel along x at `zoom`.
    fn map_resolution(&self, zoom: u8, tile_size: u32) -> f64 {
        let tiles = f64::from(tile_size) * 2f64.powi(i32::from(zoom));
        match self.crs {
            CogCrs::WebMercator { .. } => 2.0 * HALF_WORLD / tiles,
            CogCrs::Geographic { .. } => 360.0 / tiles,
        }
    }

    /// One line: `10812 x 10812 float32 x1, deflate, EPSG:4269, 5 overviews`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} x {} {} x{}, {}, EPSG:{}, {} overviews, {}x{} tiles",
            self.width,
            self.height,
            self.dtype.name(),
            self.bands,
            self.compression.name(),
            self.crs.epsg(),
            self.overviews.len(),
            self.tile_width,
            self.tile_height
        )
    }
}

/// Counters since open, for a card or a perf row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CogStats {
    /// Map tiles made (absent ones -- outside the bounds -- not counted).
    pub map_tiles: u64,
    /// Range requests issued for tile data (the header's not included).
    pub requests: u64,
    /// Bytes those requests returned.
    pub bytes_read: u64,
    /// Internal tiles decoded.
    pub tiles_decoded: u64,
    /// Internal tiles found in the cache instead.
    pub cache_hits: u64,
}

/// Per-level layout: where each internal tile's bytes are.
#[derive(Debug, Clone)]
pub(crate) struct LevelLayout {
    width: u32,
    height: u32,
    tiles_across: u32,
    tiles_down: u32,
    offsets: Vec<u64>,
    counts: Vec<u64>,
    /// `x0, y0` of the top-left corner, CRS units, and pixel size.
    origin: (f64, f64),
    res: (f64, f64),
}

/// The decode parameters every tile shares.
#[derive(Debug, Clone)]
pub(crate) struct Format {
    pub compression: CogCompression,
    pub predictor: u16,
    pub dtype: CogDataType,
    pub spp: usize,
    pub planar_separate: bool,
    pub little_endian: bool,
    pub jpeg_tables: Option<Vec<u8>>,
    pub tile_w: u32,
    pub tile_h: u32,
}

/// An internal tile's identity: level, plane, index.
pub(crate) type Key = (u8, u16, u32);

/// A decoded internal tile, ready to sample.
pub(crate) enum Pixels {
    /// RGBA8, premultiplied alpha.
    Rgba(Vec<u8>),
    /// One band, NaN where nodata.
    Values(Vec<f32>),
    /// A sparse tile (no bytes in the file): all nodata.
    Empty,
}

impl Pixels {
    fn bytes(&self) -> usize {
        match self {
            Self::Rgba(v) => v.len(),
            Self::Values(v) => v.len() * 4,
            Self::Empty => 0,
        }
    }
}

/// What one map tile needs: the level, the internal tiles, and how the
/// map's pixel centres fall on the level's pixels.
pub(crate) struct Plan {
    level: usize,
    keys: Vec<Key>,
    /// Source-pixel x for each output column, y for each output row
    /// (pixel-centre coordinates: 0.0 is the centre of pixel 0), with one
    /// column/row of border each side: `xs[i + 1]` is column `i`.
    xs: Vec<f64>,
    ys: Vec<f64>,
    size: u32,
    tile: TileId,
}

/// The format-independent core: layout, options, cache, counters. The
/// glue in `fetch_native`/`fetch_web` drives its sans-IO steps.
pub(crate) struct Cog {
    meta: CogMetadata,
    levels: Vec<LevelLayout>,
    format: Format,
    options: CogOptions,
    lut: Vec<[u8; 4]>,
    cache: Mutex<lru::LruCache<Key, Arc<Pixels>>>,
    cache_bytes: AtomicU64,
    inflight: Mutex<HashMap<Key, Arc<Inflight>>>,
    map_tiles: AtomicU64,
    requests: AtomicU64,
    bytes_read: AtomicU64,
    tiles_decoded: AtomicU64,
    cache_hits: AtomicU64,
}

/// [`Cog::missing`]'s answer: what is in hand, what to fetch, what
/// another map tile is fetching, and the claim on this one's own.
pub(crate) struct Fetch {
    pub have: HashMap<Key, Arc<Pixels>>,
    pub need: Vec<ByteRange>,
    pub wait: Vec<(Key, Arc<Inflight>)>,
    pub claim: Claim,
}

/// One internal tile being fetched and decoded by some map tile: a
/// one-shot result any number of other map tiles can await (a waker list,
/// so it needs no runtime and works in the browser too).
#[derive(Default)]
pub(crate) struct Inflight {
    state: Mutex<(Option<Option<Arc<Pixels>>>, Vec<std::task::Waker>)>,
}

impl Inflight {
    fn complete(&self, pixels: Option<Arc<Pixels>>) {
        let wakers = {
            let mut s = self.state.lock().expect("in-flight");
            if s.0.is_some() {
                return;
            }
            s.0 = Some(pixels);
            std::mem::take(&mut s.1)
        };
        for w in wakers {
            w.wake();
        }
    }

    /// The decoded tile, or `None` when its fetch or decode failed.
    pub(crate) async fn wait(&self) -> Option<Arc<Pixels>> {
        std::future::poll_fn(|cx| {
            let mut s = self.state.lock().expect("in-flight");
            match &s.0 {
                Some(done) => std::task::Poll::Ready(done.clone()),
                None => {
                    s.1.push(cx.waker().clone());
                    std::task::Poll::Pending
                }
            }
        })
        .await
    }
}

/// The internal tiles one map tile has undertaken to fetch. Dropped
/// unfinished -- a failed fetch, a cancelled task -- it releases them as
/// failed, so no waiter hangs.
pub(crate) struct Claim {
    cog: Arc<Cog>,
    entries: Vec<(Key, Arc<Inflight>)>,
}

impl Claim {
    fn finish(&self, key: Key, pixels: &Arc<Pixels>) {
        let Some((_, mine)) = self.entries.iter().find(|(k, _)| *k == key) else { return };
        let mut map = self.cog.inflight.lock().expect("cog in-flight");
        if map.get(&key).is_some_and(|f| Arc::ptr_eq(f, mine)) {
            map.remove(&key);
        }
        drop(map);
        mine.complete(Some(pixels.clone()));
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut map = self.cog.inflight.lock().expect("cog in-flight");
        for (key, mine) in &self.entries {
            if map.get(key).is_some_and(|f| Arc::ptr_eq(f, mine)) {
                map.remove(key);
            }
        }
        drop(map);
        for (_, mine) in &self.entries {
            mine.complete(None);
        }
    }
}

/// Awaits the tiles other map tiles are fetching, into `have`.
pub(crate) async fn await_shared(wait: Vec<(Key, Arc<Inflight>)>, have: &mut HashMap<Key, Arc<Pixels>>) -> Result<(), CogError> {
    for (key, other) in wait {
        let pixels = other.wait().await.ok_or_else(|| CogError::Decode("a tile another map tile was fetching failed".into()))?;
        have.insert(key, pixels);
    }
    Ok(())
}

/// How a header read ended: the parsed IFDs and the prefix size read.
pub(crate) struct Header {
    ifds: Vec<Ifd>,
    little_endian: bool,
    prefix_len: u64,
    requests: u32,
}

/// The first header read: 64 KiB holds a GDAL COG's directories up to a
/// few thousand tiles per level.
pub(crate) const FIRST_HEADER_READ: u64 = 64 << 10;
/// A header (the IFDs and their arrays) over this is refused.
pub(crate) const MAX_HEADER_BYTES: u64 = 64 << 20;

/// Parses `prefix`, or names the larger prefix to read next.
pub(crate) fn header_step(prefix: &[u8]) -> Result<Option<u64>, CogError> {
    match tiff::parse(prefix)? {
        Parse::Done { .. } => Ok(None),
        Parse::NeedPrefix(end) => {
            if end > MAX_HEADER_BYTES {
                return Err(CogError::TooLarge { limit: MAX_HEADER_BYTES, got: end });
            }
            // Grow geometrically: one more request, not one per IFD.
            Ok(Some(end.max(prefix.len() as u64 * 4).min(MAX_HEADER_BYTES)))
        }
    }
}

pub(crate) fn header_from(prefix: &[u8], requests: u32) -> Result<Header, CogError> {
    match tiff::parse(prefix)? {
        Parse::Done { ifds, little_endian, .. } => Ok(Header { ifds, little_endian, prefix_len: prefix.len() as u64, requests }),
        Parse::NeedPrefix(_) => Err(CogError::NotTiff("truncated header".into())),
    }
}

fn geokeys(ifd: &Ifd) -> HashMap<u16, Value> {
    let mut out = HashMap::new();
    let Some(dir) = ifd.get(tag::GEO_KEY_DIRECTORY).and_then(Value::ints) else { return out };
    if dir.len() < 4 {
        return out;
    }
    let doubles = ifd.get(tag::GEO_DOUBLE_PARAMS).and_then(Value::reals).unwrap_or_default();
    let ascii = match ifd.get(tag::GEO_ASCII_PARAMS) {
        Some(Value::Ascii(s)) => s.clone(),
        _ => String::new(),
    };
    let n = dir[3] as usize;
    for k in 0..n {
        let Some(e) = dir.get(4 + k * 4..8 + k * 4) else { break };
        let (id, loc, count, val) = (e[0] as u16, e[1] as u16, e[2] as usize, e[3] as usize);
        let value = match loc {
            0 => Value::Ints(vec![val as u64]),
            t if t == tag::GEO_DOUBLE_PARAMS => Value::Reals(doubles.get(val..val + count).map(<[f64]>::to_vec).unwrap_or_default()),
            t if t == tag::GEO_ASCII_PARAMS => Value::Ascii(ascii.get(val..val + count).unwrap_or_default().trim_end_matches('|').to_string()),
            t if t == tag::GEO_KEY_DIRECTORY => Value::Ints(dir.get(val..val + count).map(<[u64]>::to_vec).unwrap_or_default()),
            _ => continue,
        };
        out.insert(id, value);
    }
    out
}

const WEB_MERCATOR_CODES: [u32; 5] = [3857, 900_913, 3785, 102_100, 102_113];

fn crs_of(keys: &HashMap<u16, Value>, warnings: &mut Vec<String>) -> Result<CogCrs, CogError> {
    let model = keys.get(&1024).and_then(Value::int);
    let projected = keys.get(&3072).and_then(Value::int).map(|v| v as u32);
    let geographic = keys.get(&2048).and_then(Value::int).map(|v| v as u32);
    if model == Some(1) || (model.is_none() && projected.is_some()) {
        return match projected {
            Some(code) if WEB_MERCATOR_CODES.contains(&code) => Ok(CogCrs::WebMercator { epsg: code }),
            Some(32767) | None => Err(CogError::Crs("a user-defined projected CRS (no EPSG code)".into())),
            Some(code) => Err(CogError::Crs(format!("EPSG:{code} (projected; only EPSG:3857 is mapped)"))),
        };
    }
    if model == Some(2) || geographic.is_some() {
        return match geographic {
            Some(4326) => Ok(CogCrs::Geographic { epsg: 4326 }),
            Some(code @ (4269 | 4258)) => {
                let name = if code == 4269 { "NAD83" } else { "ETRS89" };
                warnings.push(format!("EPSG:{code} ({name}) is read as WGS84: about a metre off"));
                Ok(CogCrs::Geographic { epsg: code })
            }
            Some(code) => Err(CogError::Crs(format!("EPSG:{code} (geographic; only 4326, 4269 and 4258 are read)"))),
            None => Err(CogError::Crs("a user-defined geographic CRS (no EPSG code)".into())),
        };
    }
    match model {
        Some(3) => Err(CogError::Crs("a geocentric CRS".into())),
        _ => Err(CogError::Crs("none stated (no GeoKeys): not a GeoTIFF".into())),
    }
}

/// `<Item name="STATISTICS_MINIMUM" sample="0">12.5</Item>` -> the value.
fn gdal_item(xml: &str, name: &str, sample: usize) -> Option<f64> {
    let needle = format!("name=\"{name}\"");
    let mut rest = xml;
    while let Some(at) = rest.find(&needle) {
        let tail = &rest[at..];
        let open_end = tail.find('>')?;
        let attrs = &tail[..open_end];
        let sample_ok = match attrs.find("sample=\"") {
            Some(s) => attrs[s + 8..].split('"').next().and_then(|v| v.parse::<usize>().ok()) == Some(sample),
            None => sample == 0,
        };
        let body = &tail[open_end + 1..];
        let close = body.find('<')?;
        if sample_ok {
            return body[..close].trim().parse().ok();
        }
        rest = &body[close..];
    }
    None
}

impl Cog {
    /// Builds the layout from a parsed header. Refusals happen here.
    pub(crate) fn new(header: Header, options: CogOptions, file_bytes: Option<u64>) -> Result<Self, CogError> {
        let mut warnings = Vec::new();
        // The full image, then its overviews (NewSubfileType bit 0);
        // masks (bit 2) are not drawn.
        let images: Vec<&Ifd> = header.ifds.iter().filter(|ifd| ifd.int(tag::NEW_SUBFILE_TYPE).unwrap_or(0) & 4 == 0).collect();
        let full = *images.first().ok_or_else(|| CogError::NotTiff("no image".into()))?;
        if full.get(tag::TILE_OFFSETS).is_none() {
            return Err(CogError::Unsupported(if full.get(tag::STRIP_OFFSETS).is_some() {
                "stripped, not tiled: not cloud-optimised (gdal_translate -of COG)".into()
            } else {
                "no tile offsets".into()
            }));
        }
        let int = |ifd: &Ifd, t: u16, what: &str| ifd.int(t).ok_or_else(|| CogError::NotTiff(format!("no {what}")));
        let spp = full.int(tag::SAMPLES_PER_PIXEL).unwrap_or(1) as usize;
        let bits = full.get(tag::BITS_PER_SAMPLE).and_then(Value::ints).map(<[u64]>::to_vec).unwrap_or_else(|| vec![1]);
        let sample_format = full.get(tag::SAMPLE_FORMAT).and_then(Value::int).unwrap_or(1);
        let b = bits.first().copied().unwrap_or(1);
        if bits.iter().any(|&x| x != b) {
            return Err(CogError::Unsupported("bands of different bit depths".into()));
        }
        let dtype = match (sample_format, b) {
            (1, 8) => CogDataType::U8,
            (2, 8) => CogDataType::I8,
            (1, 16) => CogDataType::U16,
            (2, 16) => CogDataType::I16,
            (1, 32) => CogDataType::U32,
            (2, 32) => CogDataType::I32,
            (3, 32) => CogDataType::F32,
            (3, 64) => CogDataType::F64,
            (f, b) => {
                let kind = match f {
                    1 => "unsigned",
                    2 => "signed",
                    3 => "float",
                    6 => "complex float",
                    5 => "complex int",
                    _ => "untyped",
                };
                return Err(CogError::Unsupported(format!("{b}-bit {kind} samples")));
            }
        };
        let compression = CogCompression::from_code(full.int(tag::COMPRESSION).unwrap_or(1))?;
        let photometric = full.int(tag::PHOTOMETRIC).unwrap_or(1);
        let predictor = full.int(tag::PREDICTOR).unwrap_or(1) as u16;
        if !matches!(predictor, 1..=3) {
            return Err(CogError::Unsupported(format!("predictor {predictor}")));
        }
        let planar_separate = full.int(tag::PLANAR_CONFIG).unwrap_or(1) == 2;
        let tile_w = int(full, tag::TILE_WIDTH, "tile width")? as u32;
        let tile_h = int(full, tag::TILE_LENGTH, "tile length")? as u32;
        if tile_w == 0 || tile_h == 0 || tile_w > 4096 || tile_h > 4096 {
            return Err(CogError::Unsupported(format!("{tile_w}x{tile_h} px internal tiles")));
        }
        if photometric == 3 && full.get(tag::COLOR_MAP).is_some() {
            warnings.push("a palette image: its indices are drawn through the ramp, not the palette".into());
        }

        // Georeferencing.
        let keys = geokeys(full);
        let crs = crs_of(&keys, &mut warnings)?;
        let pixel_is_point = keys.get(&1025).and_then(Value::int) == Some(2);
        let width = int(full, tag::IMAGE_WIDTH, "width")? as u32;
        let height = int(full, tag::IMAGE_LENGTH, "height")? as u32;
        let (mut x0, mut y0, sx, sy) = if let Some(m) = full.get(tag::MODEL_TRANSFORMATION).and_then(Value::reals) {
            if m.len() < 8 || m[1].abs() > 1e-12 || m[4].abs() > 1e-12 {
                return Err(CogError::Unsupported("a rotated or sheared geotransform".into()));
            }
            (m[3], m[7], m[0], -m[5])
        } else {
            let scale = full.get(tag::MODEL_PIXEL_SCALE).and_then(Value::reals).ok_or_else(|| CogError::Crs("no pixel scale".into()))?;
            let tie = full.get(tag::MODEL_TIEPOINT).and_then(Value::reals).ok_or_else(|| CogError::Crs("no tiepoint".into()))?;
            if scale.len() < 2 || tie.len() < 6 {
                return Err(CogError::Crs("a short tiepoint or pixel scale".into()));
            }
            (tie[3] - tie[0] * scale[0], tie[4] + tie[1] * scale[1], scale[0], scale[1])
        };
        if !(sx > 0.0 && sy > 0.0 && sx.is_finite() && sy.is_finite()) {
            return Err(CogError::Unsupported(format!("pixel size {sx} x {sy} (a flipped or empty geotransform)")));
        }
        if pixel_is_point {
            x0 -= sx / 2.0;
            y0 += sy / 2.0;
        }

        // Levels: the full image, then each overview at the same extent.
        let mut levels = Vec::new();
        let mut layouts = Vec::new();
        for ifd in &images {
            let w = int(ifd, tag::IMAGE_WIDTH, "width")? as u32;
            let h = int(ifd, tag::IMAGE_LENGTH, "height")? as u32;
            let tw = ifd.int(tag::TILE_WIDTH).unwrap_or(0) as u32;
            let th = ifd.int(tag::TILE_LENGTH).unwrap_or(0) as u32;
            if tw != tile_w || th != tile_h || w == 0 || h == 0 {
                // An overview cut differently from the image: skip it
                // rather than mis-sample it.
                if levels.is_empty() {
                    return Err(CogError::Unsupported("an empty image".into()));
                }
                warnings.push(format!("an overview of {w}x{h} with {tw}x{th} tiles is skipped"));
                continue;
            }
            let across = w.div_ceil(tile_w);
            let down = h.div_ceil(tile_h);
            let planes = if planar_separate { spp } else { 1 };
            let need = across as usize * down as usize * planes;
            let offsets = ifd.get(tag::TILE_OFFSETS).and_then(Value::ints).map(<[u64]>::to_vec).unwrap_or_default();
            let counts = ifd.get(tag::TILE_BYTE_COUNTS).and_then(Value::ints).map(<[u64]>::to_vec).unwrap_or_default();
            if offsets.len() < need || counts.len() < need {
                return Err(CogError::NotTiff(format!("{} tile offsets for {need} tiles", offsets.len().min(counts.len()))));
            }
            let res = (sx * f64::from(width) / f64::from(w), sy * f64::from(height) / f64::from(h));
            levels.push(CogLevel { width: w, height: h, resolution: res });
            layouts.push(LevelLayout { width: w, height: h, tiles_across: across, tiles_down: down, offsets, counts, origin: (x0, y0), res });
            if layouts.len() >= 255 {
                break;
            }
        }
        // Finest first, whatever order the file wrote them in.
        let mut order: Vec<usize> = (0..levels.len()).collect();
        order.sort_by(|&a, &b| levels[b].width.cmp(&levels[a].width));
        let levels: Vec<CogLevel> = order.iter().map(|&i| levels[i].clone()).collect();
        let layouts: Vec<LevelLayout> = order.iter().map(|&i| layouts[i].clone()).collect();

        // What is drawn.
        let rgb_capable = dtype == CogDataType::U8 && (spp == 3 || spp == 4) && !planar_separate && matches!(photometric, 2 | 6);
        let jpeg_rgb = compression == CogCompression::Jpeg && spp >= 3;
        let gdal_xml = match full.get(tag::GDAL_METADATA) {
            Some(Value::Ascii(s)) => s.clone(),
            _ => String::new(),
        };
        let file_nodata = match full.get(tag::GDAL_NODATA) {
            Some(Value::Ascii(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        let nodata = options.nodata.or(file_nodata);
        let (rendering, stats) = match options.band {
            None if rgb_capable || jpeg_rgb => {
                let alpha = spp == 4 || full.get(tag::EXTRA_SAMPLES).and_then(Value::int).is_some_and(|v| v == 1 || v == 2);
                (CogRendering::Rgb { alpha: alpha && compression != CogCompression::Jpeg }, None)
            }
            band => {
                let band = band.unwrap_or(0);
                if band >= spp {
                    return Err(CogError::Unsupported(format!("band {band} of a {spp}-band image")));
                }
                if compression == CogCompression::Jpeg {
                    return Err(CogError::Unsupported("a ramp over JPEG-compressed data".into()));
                }
                let stats = gdal_item(&gdal_xml, "STATISTICS_MINIMUM", band).zip(gdal_item(&gdal_xml, "STATISTICS_MAXIMUM", band));
                let min = options.min.or(stats.map(|s| s.0));
                let max = options.max.or(stats.map(|s| s.1));
                let (Some(min), Some(max)) = (min, max) else { return Err(CogError::NoRange) };
                (CogRendering::Ramp { band, min, max }, stats)
            }
        };

        // Bounds in degrees.
        let (x1, y1) = (x0 + sx * f64::from(width), y0 - sy * f64::from(height));
        let bounds = match crs {
            CogCrs::WebMercator { .. } => {
                let (w, n) = mercator_to_lonlat(x0, y0);
                let (e, s) = mercator_to_lonlat(x1, y1);
                [w, s, e, n]
            }
            CogCrs::Geographic { .. } => [x0, y1.max(-90.0), x1, y0.min(90.0)],
        };
        let jpeg_tables = match full.get(tag::JPEG_TABLES) {
            Some(Value::Bytes(b)) => Some(b.clone()),
            Some(Value::Ints(v)) => Some(v.iter().map(|&x| x as u8).collect()),
            _ => None,
        };
        let format = Format {
            compression,
            predictor,
            dtype,
            spp,
            planar_separate,
            little_endian: header.little_endian,
            jpeg_tables,
            tile_w,
            tile_h,
        };
        let mut meta = CogMetadata {
            width,
            height,
            bands: spp,
            dtype,
            compression,
            crs,
            bounds,
            overviews: levels.iter().skip(1).map(|l| (l.width, l.height)).collect(),
            levels,
            tile_width: tile_w,
            tile_height: tile_h,
            nodata,
            stats,
            rendering,
            max_zoom: 0,
            header_bytes: header.prefix_len,
            header_requests: header.requests,
            file_bytes,
            warnings,
        };
        // The zoom whose map pixel first matches the full resolution.
        let full_res = meta.levels[0].resolution.0;
        meta.max_zoom = (0..=MAX_ZOOM)
            .find(|&z| meta.map_resolution(z, options.tile_size) <= full_res * 1.05)
            .unwrap_or(MAX_ZOOM);
        let lut = options.ramp.lut();
        let cap = std::num::NonZeroUsize::new(4096).expect("non-zero");
        Ok(Self {
            meta,
            levels: layouts,
            format,
            options,
            lut,
            cache: Mutex::new(lru::LruCache::new(cap)),
            cache_bytes: AtomicU64::new(0),
            inflight: Mutex::new(HashMap::new()),
            map_tiles: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            tiles_decoded: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
        })
    }

    pub(crate) fn stats(&self) -> CogStats {
        CogStats {
            map_tiles: self.map_tiles.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            tiles_decoded: self.tiles_decoded.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
        }
    }

    /// The plane a tile's bytes come from: the ramp's band when bands are
    /// stored separately, else 0.
    fn plane(&self) -> u16 {
        match (&self.meta.rendering, self.format.planar_separate) {
            (CogRendering::Ramp { band, .. }, true) => *band as u16,
            _ => 0,
        }
    }

    /// Step 1: which level, which internal tiles, and where each map pixel
    /// lands. `None` when the map tile misses the image.
    pub(crate) fn plan(&self, tile: TileId, size: u32) -> Option<Plan> {
        let level = self.meta.level_for(tile.z, size);
        let lay = &self.levels[level];
        let n = f64::from(size) * 2f64.powi(i32::from(tile.z));
        let px0 = f64::from(tile.x) * f64::from(size);
        let py0 = f64::from(tile.y) * f64::from(size);
        let (ox, oy) = lay.origin;
        let (rx, ry) = lay.res;
        // One map pixel of border on each side (index 0 and size + 1), for
        // the hillshade's neighbours.
        let border = |count: u32| (0..count + 2).map(|i| f64::from(i) - 1.0);
        let xs: Vec<f64> = border(size)
            .map(|i| {
                let u = (px0 + i + 0.5) / n;
                let crs_x = match self.meta.crs {
                    CogCrs::WebMercator { .. } => (u - 0.5) * 2.0 * HALF_WORLD,
                    CogCrs::Geographic { .. } => u * 360.0 - 180.0,
                };
                (crs_x - ox) / rx - 0.5
            })
            .collect();
        let ys: Vec<f64> = border(size)
            .map(|j| {
                let v = (py0 + j + 0.5) / n;
                let crs_y = match self.meta.crs {
                    CogCrs::WebMercator { .. } => (0.5 - v) * 2.0 * HALF_WORLD,
                    CogCrs::Geographic { .. } => (std::f64::consts::PI * (1.0 - 2.0 * v)).sinh().atan().to_degrees(),
                };
                (oy - crs_y) / ry - 0.5
            })
            .collect();
        let (w, h) = (f64::from(lay.width), f64::from(lay.height));
        // Wholly outside the image (after the half-pixel edge): absent.
        let last = size as usize;
        if xs[last] < -0.5 || xs[1] > w - 0.5 || ys[last] < -0.5 || ys[1] > h - 0.5 {
            return None;
        }
        // Pixels touched: the border's, plus one for bilinear.
        let lo_x = xs[0].floor() - 1.0;
        let hi_x = xs[last + 1].ceil() + 1.0;
        let lo_y = ys[0].floor() - 1.0;
        let hi_y = ys[last + 1].ceil() + 1.0;
        let clamp = |v: f64, max: u32| v.max(0.0).min(f64::from(max) - 1.0) as u32;
        let (c0, c1) = (clamp(lo_x, lay.width), clamp(hi_x, lay.width));
        let (r0, r1) = (clamp(lo_y, lay.height), clamp(hi_y, lay.height));
        let (tw, th) = (self.format.tile_w, self.format.tile_h);
        let plane = self.plane();
        let mut keys = Vec::new();
        for ty in r0 / th..=r1 / th {
            for tx in c0 / tw..=c1 / tw {
                keys.push((level as u8, plane, self.tile_index(level, plane, tx, ty)));
            }
        }
        Some(Plan { level, keys, xs, ys, size, tile })
    }

    fn tile_index(&self, level: usize, plane: u16, tx: u32, ty: u32) -> u32 {
        let lay = &self.levels[level];
        u32::from(plane) * lay.tiles_across * lay.tiles_down + ty * lay.tiles_across + tx
    }

    /// Step 2: the plan's tiles already decoded, and the byte ranges of the
    /// rest (a sparse tile -- zero bytes -- is "decoded" as empty here).
    ///
    /// A tile another map tile is already fetching is not fetched again:
    /// it is returned in [`Fetch::wait`], to be awaited after this map
    /// tile's own tiles are decoded (own first, so two map tiles waiting on
    /// each other's tiles cannot deadlock). Neighbouring map tiles share
    /// internal tiles, and the map requests a whole view at once.
    pub(crate) fn missing(self: &Arc<Self>, plan: &Plan) -> Fetch {
        let mut have = HashMap::new();
        let mut need = Vec::new();
        let mut wait = Vec::new();
        let mut claim = Claim { cog: self.clone(), entries: Vec::new() };
        let mut cache = self.cache.lock().expect("cog cache");
        let mut inflight = self.inflight.lock().expect("cog in-flight");
        for &key in &plan.keys {
            if let Some(px) = cache.get(&key) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                have.insert(key, px.clone());
                continue;
            }
            let lay = &self.levels[key.0 as usize];
            let (offset, count) = (lay.offsets[key.2 as usize], lay.counts[key.2 as usize]);
            if offset == 0 || count == 0 {
                have.insert(key, Arc::new(Pixels::Empty));
            } else if let Some(other) = inflight.get(&key) {
                wait.push((key, other.clone()));
            } else {
                let mine = Arc::new(Inflight::default());
                inflight.insert(key, mine.clone());
                claim.entries.push((key, mine));
                need.push(ByteRange { key, start: offset, len: count });
            }
        }
        Fetch { have, need, wait, claim }
    }

    /// Notes a finished fetch: requests issued and bytes they returned.
    pub(crate) fn count_fetch(&self, requests: u64, bytes: u64) {
        self.requests.fetch_add(requests, Ordering::Relaxed);
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Step 3 (CPU): decodes each fetched group's members into `have` and
    /// the cache.
    pub(crate) fn decode_groups(
        &self,
        groups: &[RangeGroup],
        bodies: &[Vec<u8>],
        have: &mut HashMap<Key, Arc<Pixels>>,
        claim: &Claim,
    ) -> Result<(), CogError> {
        for (group, body) in groups.iter().zip(bodies) {
            for member in &group.members {
                let at = (member.start - group.start) as usize;
                let bytes = body.get(at..at + member.len as usize).ok_or_else(|| CogError::Decode("a short range response".into()))?;
                // Edge tiles are stored full size; only their valid part
                // is sampled (the plan clamps to the image).
                let pixels = decode::decode_tile(bytes, &self.format, &self.meta.rendering, self.meta.nodata)?;
                self.tiles_decoded.fetch_add(1, Ordering::Relaxed);
                let pixels = Arc::new(pixels);
                self.insert(member.key, pixels.clone());
                claim.finish(member.key, &pixels);
                have.insert(member.key, pixels);
            }
        }
        Ok(())
    }

    fn insert(&self, key: Key, pixels: Arc<Pixels>) {
        let mut cache = self.cache.lock().expect("cog cache");
        let add = pixels.bytes() as u64;
        if let Some(old) = cache.put(key, pixels) {
            self.cache_bytes.fetch_sub(old.bytes() as u64, Ordering::Relaxed);
        }
        let mut total = self.cache_bytes.fetch_add(add, Ordering::Relaxed) + add;
        while total > self.options.cache_bytes as u64 {
            let Some((_, old)) = cache.pop_lru() else { break };
            total = self.cache_bytes.fetch_sub(old.bytes() as u64, Ordering::Relaxed) - old.bytes() as u64;
        }
    }

    /// Step 4 (CPU), values instead of colours: `size^2`, NaN where
    /// nothing is.
    pub(crate) fn values(&self, plan: &Plan, have: &HashMap<Key, Arc<Pixels>>) -> Vec<f32> {
        let all = render::values(self, plan, have);
        let (size, stride) = (plan.size as usize, plan.size as usize + 2);
        (0..size).flat_map(|j| all[(j + 1) * stride + 1..(j + 1) * stride + 1 + size].to_vec()).collect()
    }

    /// Step 4 (CPU): the map tile.
    pub(crate) fn render(&self, plan: &Plan, have: &HashMap<Key, Arc<Pixels>>) -> image::RgbaImage {
        self.map_tiles.fetch_add(1, Ordering::Relaxed);
        render::render(self, plan, have)
    }

    /// The plan, the ranges to fetch, coalesced.
    pub(crate) fn groups(&self, need: &[ByteRange]) -> Vec<RangeGroup> {
        coalesce(need, self.options.merge_gap, self.options.max_request_bytes)
    }
}

/// Web Mercator metres to degrees.
fn mercator_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = x / EARTH_RADIUS;
    let lat = (y / EARTH_RADIUS).sinh().atan();
    (lon.to_degrees(), lat.to_degrees().clamp(-MAX_LAT, MAX_LAT))
}

/// A Cloud-Optimised GeoTIFF as a raster tile source. See the module docs.
///
/// Cheap to share (`Arc` inside); the decoded-tile cache and the counters
/// are shared by every clone.
#[derive(Clone)]
pub struct CogSource {
    pub(crate) cog: Arc<Cog>,
    pub(crate) reader: Arc<Reader>,
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "cog_ureq")))]
pub(crate) use fetch_native::Reader;
#[cfg(all(not(target_arch = "wasm32"), feature = "cog_ureq"))]
pub(crate) use fetch_ureq::Reader;
#[cfg(target_arch = "wasm32")]
pub(crate) use fetch_web::Reader;

impl std::fmt::Debug for CogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CogSource").field("metadata", &self.cog.meta.summary()).finish_non_exhaustive()
    }
}

impl CogSource {
    /// What the file says about itself.
    #[must_use]
    pub fn metadata(&self) -> &CogMetadata {
        &self.cog.meta
    }

    /// The options it was opened with.
    #[must_use]
    pub fn options(&self) -> &CogOptions {
        &self.cog.options
    }

    /// Counters since open.
    #[must_use]
    pub fn stats(&self) -> CogStats {
        self.cog.stats()
    }

    /// The map tile edge it renders (pass it to `set_raster_source`).
    #[must_use]
    pub fn tile_size(&self) -> u32 {
        self.cog.options.tile_size
    }

    /// The map camera that frames the image in a `w` x `h` px view:
    /// `(lon, lat, zoom)`.
    #[must_use]
    pub fn opening_camera(&self, w: f64, h: f64) -> (f64, f64, f64) {
        let [west, south, east, north] = self.cog.meta.bounds;
        let merc_y = |lat: f64| {
            let lat = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
            (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()
        };
        let span_x = ((east - west) / 360.0).max(1e-9);
        let span_y = ((merc_y(north) - merc_y(south)) / (2.0 * std::f64::consts::PI)).max(1e-9);
        let world = crate::geo::TILE_SIZE;
        let zoom = (w / (world * span_x)).log2().min((h / (world * span_y)).log2()) - 0.15;
        let mid_y = (merc_y(north) + merc_y(south)) / 2.0;
        let lat = mid_y.sinh().atan().to_degrees();
        ((west + east) / 2.0, lat, zoom.clamp(0.0, 20.0))
    }

    /// Renders one map tile, `Ok(None)` when it misses the image. Async:
    /// native drives it on the tile runtime (and decodes on a blocking
    /// worker); the browser on its event loop.
    pub async fn render_tile(&self, tile: TileId) -> Result<Option<image::RgbaImage>, CogError> {
        self.render_tile_sized(tile, self.cog.options.tile_size).await
    }
}

/// Polls a future that never waits (a slice or file reader's) to its end.
pub(crate) fn block_ready<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => unreachable!("a synchronous range reader never waits"),
    }
}

pub(crate) use crate::range_fetch::join_bounded;
