//! Reading a GeoParquet file from a local file or an http(s) URL,
//! blocking: call these from a worker thread
//! (`MapView::load_source_geoparquet` does), never from a frame callback.
//!
//! The HTTP half is the FlatGeobuf source's reader (this crate's `reqwest`
//! client, the project User-Agent, `identity` encoding so ranges line up),
//! behind `sprite_http_fetch` like every other `reqwest` caller here.

use std::collections::HashSet;
use std::io::{Read as _, Seek as _, SeekFrom};

use super::{open, read_source, read_with, GeoParquetError, GeoParquetOptions, GeoParquetQueryStats, GeoParquetRemote, GeoParquetSource};
use crate::overlay::fgb::{block_ready, run_blocking, FgbError, HttpReader, RangeRead};

impl GeoParquetSource {
    /// Reads a local GeoParquet file by seek: the footer, then only the
    /// projected column chunks of the row groups a
    /// [`GeoParquetOptions::bbox`] reaches (all of them without one).
    pub fn from_path(path: impl AsRef<std::path::Path>, options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        let path = path.as_ref();
        let io = |e: std::io::Error| GeoParquetError::Io(format!("{}: {e}", path.display()));
        let len = std::fs::metadata(path).map_err(io)?.len();
        let file = std::fs::File::open(path).map_err(io)?;
        let mut reader = FileReader { file, len, read: 0, requests: 0 };
        block_ready(read_source(&mut reader, options))
    }

    /// Fetches a GeoParquet file over http(s), blocking, by `Range`
    /// requests: the footer, then the projected column chunks of the row
    /// groups [`GeoParquetOptions::bbox`] reaches. A server that ignores
    /// `Range` (a 200) is read whole, within
    /// [`GeoParquetOptions::max_bytes`].
    pub fn from_url(url: &str, options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        let mut remote = GeoParquetRemote::open(url, options)?;
        remote.read(options.bbox)
    }
}

impl GeoParquetRemote {
    /// Opens a remote GeoParquet file: two range requests (the magic, then
    /// the footer's last 64 KiB), more only for a larger footer. Blocking.
    pub fn open(url: &str, options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        let mut reader = HttpReader::new(url, &options.http())?;
        let layout = run_blocking(open(&mut reader, options))?;
        Ok(Self { reader, layout, max_bytes: options.max_bytes })
    }

    /// Reads the whole file (`bbox` `None`) or one box's rows. Blocking.
    pub fn read(&mut self, bbox: Option<[f64; 4]>) -> Result<GeoParquetSource, GeoParquetError> {
        let (layout, max_bytes) = (&self.layout, self.max_bytes);
        let reader = &mut self.reader;
        run_blocking(read_with(reader, layout, bbox, max_bytes))
    }

    /// The rows in `bbox` (`[west, south, east, north]`) that are not in
    /// `loaded` (file rows, [`GeoParquetSource::file_row`]): the row groups
    /// the box reaches, less those wholly held, by range request. Blocking.
    pub fn query(
        &mut self,
        bbox: [f64; 4],
        loaded: &HashSet<u64>,
    ) -> Result<(GeoParquetSource, GeoParquetQueryStats), GeoParquetError> {
        run_blocking(self.query_async(bbox, loaded))
    }
}

/// A local file by seek.
struct FileReader {
    file: std::fs::File,
    len: u64,
    read: u64,
    requests: u32,
}

impl RangeRead for FileReader {
    async fn read(&mut self, start: u64, len: u64) -> Result<Vec<u8>, FgbError> {
        let start = start.min(self.len);
        let len = len.min(self.len - start);
        let io = |e: std::io::Error| FgbError::Io(e.to_string());
        self.file.seek(SeekFrom::Start(start)).map_err(io)?;
        let mut out = vec![0; usize::try_from(len).map_err(|_| FgbError::TooLarge { limit: usize::MAX as u64, got: len })?];
        self.file.read_exact(&mut out).map_err(io)?;
        self.read += len;
        self.requests += 1;
        Ok(out)
    }
    fn file_len(&self) -> Option<u64> {
        Some(self.len)
    }
    fn counters(&self) -> (u64, u32) {
        (self.read, self.requests)
    }
}
