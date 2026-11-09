//! GeoParquet sources on a `MapView`: the push
//! ([`MapView::set_source_geoparquet`]), the off-thread load
//! ([`MapView::load_source_geoparquet`]) and, for a large remote file,
//! viewport streaming: the row groups in view are fetched by range request
//! on load and again each time the camera settles somewhere new, rows
//! de-duplicated by their position in the file (a row group wholly held is
//! not fetched again), and held to
//! [`GeoParquetOptions::stream_max_features`]. The FlatGeobuf stream's
//! shape (`map_view::fgb`), over `overlay::geoparquet`'s reader.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::MapView;
use crate::overlay::geoparquet::{
    viewport_bbox, GeoParquetError, GeoParquetOptions, GeoParquetPushdown, GeoParquetQueryStats, GeoParquetRemote,
    GeoParquetReport, GeoParquetSource,
};
use crate::overlay::{FeatureError, FeaturePaint};

/// Where [`MapView::load_source_geoparquet`] reads a GeoParquet file from.
#[derive(Debug, Clone)]
pub enum GeoParquetInput {
    /// The file's bytes, already in memory.
    Bytes(Vec<u8>),
    /// A local file (read by seek: only the projected column chunks of
    /// the row groups needed).
    Path(PathBuf),
    /// An http(s) URL (needs the `sprite_http_fetch` feature, on by
    /// default). Streamed by viewport when large -- see
    /// [`GeoParquetOptions::stream_above_bytes`].
    Url(String),
}

/// A streamed source's counters, for a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeoParquetStreamStats {
    /// Rows and row groups in the file.
    pub rows_total: u64,
    pub row_groups_total: usize,
    /// Rows held now.
    pub rows_loaded: usize,
    /// Viewport queries run (the load's first included).
    pub queries: u32,
    /// Row groups read and skipped over every query.
    pub row_groups_read: usize,
    pub row_groups_skipped: usize,
    /// The last query's row groups, re-uses and fetches.
    pub last_query: GeoParquetQueryStats,
    /// HTTP bytes and requests since the open.
    pub bytes_read: u64,
    pub requests: u32,
    /// The file's size.
    pub file_bytes: u64,
}

/// One streamed source.
pub(super) struct GeoParquetStream {
    remote: Arc<Mutex<GeoParquetRemote>>,
    source: GeoParquetSource,
    loaded: HashSet<u64>,
    paint: FeaturePaint,
    max_features: usize,
    /// The camera the last query was started for.
    queried: Option<crate::render::Camera>,
    /// The box the resident set covers (the last query's), and whether it
    /// was thinned (then a smaller box inside it still owes a query).
    covered: Option<[f64; 4]>,
    thinned: bool,
    in_flight: bool,
    stats: GeoParquetStreamStats,
}

/// How far past the viewport a streamed query reaches, per side.
const STREAM_GROW: f64 = 0.25;

pub(super) type GeoParquetStreams = HashMap<String, GeoParquetStream>;

impl GeoParquetInput {
    /// Reads whole (or `options.bbox`), blocking.
    fn load(self, options: &GeoParquetOptions) -> Result<GeoParquetSource, GeoParquetError> {
        match self {
            Self::Bytes(bytes) => GeoParquetSource::from_bytes(&bytes, options),
            Self::Path(path) => GeoParquetSource::from_path(path, options),
            Self::Url(url) => GeoParquetSource::from_url(&url, options),
        }
    }
}

/// What the off-thread half of a load hands the UI thread.
enum Loaded {
    Whole(GeoParquetSource),
    Streamed { remote: Box<GeoParquetRemote>, first: GeoParquetSource, stats: GeoParquetQueryStats },
}

fn contains(outer: &[f64; 4], inner: &[f64; 4]) -> bool {
    outer[0] <= inner[0] && outer[1] <= inner[1] && outer[2] >= inner[2] && outer[3] >= inner[3]
}

impl MapView {
    /// Replaces source `name` with a read GeoParquet file -- see
    /// [`crate::overlay::geoparquet`]. A file of single points takes the
    /// point path; lines, polygons and multipoints take the feature path,
    /// tiled when large. `paint` paints every feature;
    /// [`Self::declare_source_paint`] expressions read its typed columns.
    ///
    /// ```ignore
    /// let gpq = GeoParquetSource::from_bytes(bytes, &GeoParquetOptions::default())?;
    /// map.declare_source_paint("countries", &paint_json)?; // ["get", "pop_est"] is a number
    /// map.set_source_geoparquet("countries", &gpq, &FeaturePaint::default(), cx)?;
    /// ```
    pub fn set_source_geoparquet(
        &mut self,
        name: &str,
        source: &GeoParquetSource,
        paint: &FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), FeatureError> {
        self.overlay_store.set_source_geoparquet(name, source, paint, self.display_zoom())?;
        self.after_source_write(name, cx);
        if source.points().is_some() {
            self.drive_point_sources(cx);
        }
        Ok(())
    }

    /// Reads and pushes a GeoParquet file without blocking the UI thread;
    /// the task resolves to the read report once the source is installed.
    ///
    /// Bytes and paths are read whole (or [`GeoParquetOptions::bbox`]),
    /// projected to [`GeoParquetOptions::columns`]. A URL is opened with
    /// two range requests (magic, footer); if its row groups carry extents,
    /// no `bbox` is given, and the file is over
    /// [`GeoParquetOptions::stream_above_bytes`], the source STREAMS: the
    /// row groups in the current viewport load now, and each camera settle
    /// on a new view reads the row groups it reaches that are not already
    /// held, evicting rows outside the view once
    /// [`GeoParquetOptions::stream_max_features`] would be passed. The
    /// report's `streaming` says which; [`Self::geoparquet_stream_stats`]
    /// follows a stream. [`Self::remove_source`] stops it.
    ///
    /// ```ignore
    /// let options = GeoParquetOptions::default().with_columns(["names", "confidence"]);
    /// let task = map.load_source_geoparquet("places", GeoParquetInput::Url(url), options, paint, cx);
    /// cx.spawn(async move |_, _| match task.await {
    ///     Ok(report) => eprintln!("places: {report}"),
    ///     Err(e) => eprintln!("places: {e}"),
    /// }).detach();
    /// ```
    pub fn load_source_geoparquet(
        &mut self,
        name: &str,
        input: GeoParquetInput,
        options: GeoParquetOptions,
        paint: FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<Result<GeoParquetReport, GeoParquetError>> {
        let name = name.to_string();
        self.geoparquet_streams.remove(&name);
        let runtime = self.runtime.clone();
        let camera = *self.camera_control.camera();
        let view_box = viewport_bbox(&camera, STREAM_GROW);
        cx.spawn(async move |entity, cx| {
            let max_features = options.stream_max_features;
            let load = move || -> Result<Loaded, GeoParquetError> {
                let GeoParquetInput::Url(url) = input else { return input.load(&options).map(Loaded::Whole) };
                let mut remote = GeoParquetRemote::open(&url, &options)?;
                let large = remote.file_len() > options.stream_above_bytes;
                if options.bbox.is_some() || !large || remote.pushdown() == GeoParquetPushdown::None {
                    return remote.read(options.bbox).map(Loaded::Whole);
                }
                let (first, stats) = remote.query(view_box, &HashSet::new())?;
                Ok(Loaded::Streamed { remote: Box::new(remote), first, stats })
            };
            let loaded = match &runtime {
                Some(runtime) => runtime.spawn_blocking(load).await.map_err(|e| GeoParquetError::Io(e.to_string()))??,
                None => cx.background_executor().spawn(async move { load() }).await?,
            };
            let pushed = entity.update(cx, |view, cx| match loaded {
                Loaded::Whole(source) => {
                    let report = source.report().clone();
                    view.set_source_geoparquet(&name, &source, &paint, cx).map(|()| report)
                }
                Loaded::Streamed { remote, first, stats } => {
                    let mut source = GeoParquetSource::empty_like(&remote);
                    let first_report = first.report().clone();
                    source.merge_stream(first, view_box, max_features);
                    let (bytes_read, requests) = remote.counters();
                    let mut report = source.report().clone();
                    report.row_groups_skipped = stats.row_groups_skipped;
                    report.bytes_read = bytes_read;
                    report.requests = requests;
                    let stream = GeoParquetStream {
                        stats: GeoParquetStreamStats {
                            rows_total: remote.rows_total(),
                            row_groups_total: remote.row_groups(),
                            rows_loaded: source.len(),
                            queries: 1,
                            row_groups_read: first_report.row_groups_read,
                            row_groups_skipped: stats.row_groups_skipped,
                            last_query: stats,
                            bytes_read,
                            requests,
                            file_bytes: remote.file_len(),
                        },
                        loaded: (0..source.len()).filter_map(|i| source.file_row(i)).collect(),
                        remote: Arc::new(Mutex::new(*remote)),
                        paint: paint.clone(),
                        max_features,
                        queried: Some(camera),
                        covered: Some(view_box),
                        thinned: stats.fetched > max_features,
                        in_flight: false,
                        source,
                    };
                    let result = view.set_source_geoparquet(&name, &stream.source, &paint, cx).map(|()| report);
                    view.geoparquet_streams.insert(name.clone(), stream);
                    cx.notify();
                    result
                }
            });
            match pushed {
                Ok(Ok(report)) => Ok(report),
                Ok(Err(e)) => Err(GeoParquetError::Push(e.to_string())),
                Err(_) => Err(GeoParquetError::Push("the map was closed before the GeoParquet loaded".into())),
            }
        })
    }

    /// A streamed source's counters; `None` when `name` is not streaming.
    #[must_use]
    pub fn geoparquet_stream_stats(&self, name: &str) -> Option<GeoParquetStreamStats> {
        self.geoparquet_streams.get(name).map(|s| s.stats)
    }

    /// Called from `paint` once the camera is final: schedules the
    /// streamed sources' queries when the camera has settled on a view
    /// they have not queried. Nothing when no source streams.
    pub(super) fn geoparquet_streams_frame(&mut self, cx: &mut gpui::App) {
        if self.geoparquet_streams.is_empty() || self.camera_in_motion() {
            return;
        }
        let camera = *self.camera_control.camera();
        if !self.geoparquet_streams.values().any(|s| !s.in_flight && s.queried != Some(camera)) {
            return;
        }
        let Some(entity) = self.self_handle.clone() else { return };
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.query_geoparquet_streams(cx));
        });
    }

    fn query_geoparquet_streams(&mut self, cx: &mut gpui::Context<Self>) {
        if self.camera_in_motion() {
            return;
        }
        let camera = *self.camera_control.camera();
        let view_box = viewport_bbox(&camera, STREAM_GROW);
        let runtime = self.runtime.clone();
        for (name, stream) in &mut self.geoparquet_streams {
            if stream.in_flight || stream.queried == Some(camera) {
                continue;
            }
            stream.queried = Some(camera);
            // Inside what is held, and nothing was thinned away: no query.
            if !stream.thinned && stream.covered.is_some_and(|c| contains(&c, &view_box)) {
                continue;
            }
            stream.in_flight = true;
            let remote = Arc::clone(&stream.remote);
            let loaded = stream.loaded.clone();
            let name = name.clone();
            let runtime = runtime.clone();
            cx.spawn(async move |entity, cx| {
                let query = move || {
                    let mut remote = remote.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = remote.query(view_box, &loaded);
                    (result, remote.counters())
                };
                let (result, counters) = match &runtime {
                    Some(runtime) => match runtime.spawn_blocking(query).await {
                        Ok(done) => done,
                        Err(e) => (Err(GeoParquetError::Io(e.to_string())), (0, 0)),
                    },
                    None => cx.background_executor().spawn(async move { query() }).await,
                };
                let _ = entity.update(cx, |view, cx| view.land_geoparquet_query(&name, view_box, result, counters, cx));
            })
            .detach();
        }
    }

    fn land_geoparquet_query(
        &mut self,
        name: &str,
        view_box: [f64; 4],
        result: Result<(GeoParquetSource, GeoParquetQueryStats), GeoParquetError>,
        counters: (u64, u32),
        cx: &mut gpui::Context<Self>,
    ) {
        // Out of the map for the push (which borrows the view), back after.
        let Some(mut stream) = self.geoparquet_streams.remove(name) else { return };
        stream.in_flight = false;
        match result {
            Ok((more, stats)) => {
                stream.stats.queries += 1;
                stream.stats.last_query = stats;
                stream.stats.row_groups_read += more.report().row_groups_read;
                stream.stats.row_groups_skipped += stats.row_groups_skipped;
                (stream.stats.bytes_read, stream.stats.requests) = counters;
                stream.covered = Some(view_box);
                stream.thinned = stats.fetched + stats.already_loaded > stream.max_features;
                if !more.is_empty() {
                    let fresh: Vec<u64> = (0..more.len()).filter_map(|i| more.file_row(i)).collect();
                    let dropped = stream.source.merge_stream(more, view_box, stream.max_features);
                    stream.loaded.extend(fresh);
                    for id in &dropped {
                        stream.loaded.remove(id);
                    }
                    stream.stats.rows_loaded = stream.source.len();
                    if let Err(e) = self.set_source_geoparquet(name, &stream.source, &stream.paint, cx) {
                        eprintln!("gpui-maps: GeoParquet source {name:?}: push failed: {e:?}");
                    }
                }
            }
            Err(e) => {
                eprintln!("gpui-maps: GeoParquet source {name:?}: viewport query failed: {e}");
                // Retried on the next settle.
                stream.queried = None;
            }
        }
        self.geoparquet_streams.insert(name.to_string(), stream);
        cx.notify();
    }
}
