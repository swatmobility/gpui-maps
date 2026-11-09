//! The browser's GeoParquet reads: the FlatGeobuf source's `fetch` range
//! reader (each request aborted at [`super::GeoParquetOptions::timeout`])
//! under the same async read core as native, so a viewport query is the
//! same footer-then-column-chunks handful of range requests in a tab.
//!
//! The server must allow the `Range` header cross-origin (CORS); S3 and
//! GitHub Pages do. A zstd-compressed file (Overture's are) is refused by
//! codec name here: zstd is C, and this shell links no C toolchain.

use std::collections::HashSet;

use super::{open, read_with, GeoParquetError, GeoParquetOptions, GeoParquetQueryStats, GeoParquetRemote, GeoParquetSource};
use crate::overlay::fgb::HttpReader;

impl GeoParquetSource {
    /// Fetches a GeoParquet file in the browser by range requests: the
    /// footer, then the projected column chunks of the row groups
    /// [`GeoParquetOptions::bbox`] reaches.
    pub async fn from_url(url: &str, options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        let mut remote = GeoParquetRemote::open(url, options).await?;
        remote.read(options.bbox).await
    }
}

impl GeoParquetRemote {
    /// Opens a remote GeoParquet file (its footer).
    pub async fn open(url: &str, options: &GeoParquetOptions) -> Result<Self, GeoParquetError> {
        let mut reader = HttpReader::new(url, &options.http());
        let layout = open(&mut reader, options).await?;
        Ok(Self { reader, layout, max_bytes: options.max_bytes })
    }

    /// Reads the whole file (`bbox` `None`) or one box's rows.
    pub async fn read(&mut self, bbox: Option<[f64; 4]>) -> Result<GeoParquetSource, GeoParquetError> {
        read_with(&mut self.reader, &self.layout, bbox, self.max_bytes).await
    }

    /// The rows in `bbox` not in `loaded` -- see the native `query`.
    pub async fn query(
        &mut self,
        bbox: [f64; 4],
        loaded: &HashSet<u64>,
    ) -> Result<(GeoParquetSource, GeoParquetQueryStats), GeoParquetError> {
        self.query_async(bbox, loaded).await
    }
}
