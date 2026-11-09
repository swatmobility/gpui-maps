//! FlatGeobuf sources on a `MapView`: the push ([`MapView::set_source_fgb`]),
//! the off-thread load ([`MapView::load_source_fgb`]) and, for a large
//! remote file, viewport streaming: the features in view are fetched by
//! range request on load and again each time the camera settles somewhere
//! new, de-duplicated by their position in the file, and held to
//! [`FgbOptions::stream_max_features`]. The reader is `overlay::fgb`'s;
//! this is where it meets the view's camera, write hooks and runtime.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::MapView;
use crate::overlay::fgb::{viewport_bbox, FgbError, FgbOptions, FgbQueryStats, FgbRemote, FgbReport, FgbSource};
use crate::overlay::{FeatureError, FeaturePaint};

/// Where [`MapView::load_source_fgb`] reads a FlatGeobuf from.
#[derive(Debug, Clone)]
pub enum FgbInput {
    /// The file's bytes, already in memory.
    Bytes(Vec<u8>),
    /// A local file.
    Path(PathBuf),
    /// An http(s) URL (needs the `sprite_http_fetch` feature, on by
    /// default). Streamed by viewport when large -- see
    /// [`FgbOptions::stream_above_bytes`].
    Url(String),
}

/// A streamed source's counters, for a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FgbStreamStats {
    /// Features in the file, by its header.
    pub features_total: u64,
    /// Features held now.
    pub features_loaded: usize,
    /// Viewport queries run (the load's first included).
    pub queries: u32,
    /// The last query's hits, re-uses and fetches.
    pub last_query: FgbQueryStats,
    /// HTTP bytes and requests since the open.
    pub bytes_read: u64,
    pub requests: u32,
    /// The file's size.
    pub file_bytes: u64,
}

/// One streamed source.
pub(super) struct FgbStream {
    remote: Arc<Mutex<FgbRemote>>,
    source: FgbSource,
    loaded: HashSet<u64>,
    paint: FeaturePaint,
    max_features: usize,
    /// The camera the last query was started for.
    queried: Option<crate::render::Camera>,
    /// The box the resident set covers (the last query's), and whether
    /// it was thinned (then a smaller box inside it still owes a query).
    covered: Option<[f64; 4]>,
    thinned: bool,
    in_flight: bool,
    stats: FgbStreamStats,
}

/// How far past the viewport a streamed query reaches, per side: a short
/// pan finds its features already loaded.
const STREAM_GROW: f64 = 0.25;

pub(super) type FgbStreams = HashMap<String, FgbStream>;

impl FgbInput {
    /// Reads whole (or `options.bbox`), blocking.
    fn load(self, options: &FgbOptions) -> Result<FgbSource, FgbError> {
        match self {
            Self::Bytes(bytes) => FgbSource::from_bytes(&bytes, options),
            Self::Path(path) => FgbSource::from_path(path, options),
            Self::Url(url) => FgbSource::from_url(&url, options),
        }
    }
}

/// What the off-thread half of a load hands the UI thread.
enum Loaded {
    Whole(FgbSource),
    Streamed { remote: Box<FgbRemote>, first: FgbSource, stats: FgbQueryStats },
}

fn contains(outer: &[f64; 4], inner: &[f64; 4]) -> bool {
    outer[0] <= inner[0] && outer[1] <= inner[1] && outer[2] >= inner[2] && outer[3] >= inner[3]
}

impl MapView {
    /// Replaces source `name` with a read FlatGeobuf -- see
    /// [`crate::overlay::fgb`]. A file of single points takes the point
    /// path; lines, polygons and multipoints take the feature path, tiled
    /// when large. `paint` paints every feature;
    /// [`Self::declare_source_paint`] expressions read its typed columns.
    ///
    /// ```ignore
    /// let fgb = FgbSource::from_bytes(bytes, &FgbOptions::default())?;
    /// map.declare_source_paint("parcels", &paint_json)?; // ["get", "area"] is a number
    /// map.set_source_fgb("parcels", &fgb, &FeaturePaint::default(), cx)?;
    /// ```
    pub fn set_source_fgb(
        &mut self,
        name: &str,
        fgb: &FgbSource,
        paint: &FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), FeatureError> {
        self.overlay_store.set_source_fgb(name, fgb, paint, self.display_zoom())?;
        self.after_source_write(name, cx);
        if fgb.points().is_some() {
            self.drive_point_sources(cx);
        }
        Ok(())
    }

    /// Reads and pushes a FlatGeobuf without blocking the UI thread; the
    /// task resolves to the read report once the source is installed.
    ///
    /// Bytes and paths are read whole (or [`FgbOptions::bbox`]). A URL is
    /// opened with one range request; if its index exists, no `bbox` is
    /// given, and the file is over [`FgbOptions::stream_above_bytes`], the
    /// source STREAMS: the current viewport's features load now, and each
    /// camera settle on a new view queries the index again, fetches only
    /// features not already held, and evicts features outside the view
    /// once [`FgbOptions::stream_max_features`] would be passed. The
    /// report's `streaming` says which; [`Self::fgb_stream_stats`] follows
    /// a stream. [`Self::remove_source`] stops it.
    ///
    /// ```ignore
    /// let task = map.load_source_fgb("counties", FgbInput::Url(url), FgbOptions::default(), paint, cx);
    /// cx.spawn(async move |_, _| match task.await {
    ///     Ok(report) => eprintln!("counties: {report}"),
    ///     Err(e) => eprintln!("counties: {e}"),
    /// }).detach();
    /// ```
    pub fn load_source_fgb(
        &mut self,
        name: &str,
        input: FgbInput,
        options: FgbOptions,
        paint: FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<Result<FgbReport, FgbError>> {
        let name = name.to_string();
        self.fgb_streams.remove(&name);
        let runtime = self.runtime.clone();
        let camera = *self.camera_control.camera();
        let view_box = viewport_bbox(&camera, STREAM_GROW);
        cx.spawn(async move |entity, cx| {
            let max_features = options.stream_max_features;
            let load = move || -> Result<Loaded, FgbError> {
                let FgbInput::Url(url) = input else { return input.load(&options).map(Loaded::Whole) };
                let mut remote = FgbRemote::open(&url, &options)?;
                let large = remote.file_len().is_some_and(|len| len > options.stream_above_bytes);
                if options.bbox.is_some() || !large || !remote.header().indexed() {
                    return remote.read(options.bbox).map(Loaded::Whole);
                }
                let (first, stats) = remote.query(view_box, &HashSet::new())?;
                Ok(Loaded::Streamed { remote: Box::new(remote), first, stats })
            };
            let loaded = match &runtime {
                Some(runtime) => runtime.spawn_blocking(load).await.map_err(|e| FgbError::Io(e.to_string()))??,
                None => cx.background_executor().spawn(async move { load() }).await?,
            };
            let pushed = entity.update(cx, |view, cx| match loaded {
                Loaded::Whole(fgb) => {
                    let report = fgb.report().clone();
                    view.set_source_fgb(&name, &fgb, &paint, cx).map(|()| report)
                }
                Loaded::Streamed { remote, first, stats } => {
                    let mut source = FgbSource::empty_like(remote.header());
                    source.merge_stream(first, view_box, max_features);
                    let (bytes_read, requests) = remote.counters();
                    let mut report = source.report().clone();
                    report.warnings.extend(remote.warnings());
                    report.bytes_read = bytes_read;
                    report.requests = requests;
                    let stream = FgbStream {
                        stats: FgbStreamStats {
                            features_total: remote.header().features_count,
                            features_loaded: source.len(),
                            queries: 1,
                            last_query: stats,
                            bytes_read,
                            requests,
                            file_bytes: remote.file_len().unwrap_or(0),
                        },
                        loaded: (0..source.len()).filter_map(|i| source.file_index(i)).collect(),
                        remote: Arc::new(Mutex::new(*remote)),
                        paint: paint.clone(),
                        max_features,
                        queried: Some(camera),
                        covered: Some(view_box),
                        thinned: stats.hits > max_features,
                        in_flight: false,
                        source,
                    };
                    let result = view.set_source_fgb(&name, &stream.source, &paint, cx).map(|()| report);
                    view.fgb_streams.insert(name.clone(), stream);
                    cx.notify();
                    result
                }
            });
            match pushed {
                Ok(Ok(report)) => Ok(report),
                Ok(Err(e)) => Err(FgbError::Push(e.to_string())),
                Err(_) => Err(FgbError::Push("the map was closed before the FlatGeobuf loaded".into())),
            }
        })
    }

    /// A streamed source's counters; `None` when `name` is not streaming.
    #[must_use]
    pub fn fgb_stream_stats(&self, name: &str) -> Option<FgbStreamStats> {
        self.fgb_streams.get(name).map(|s| s.stats)
    }

    /// Called from `paint` once the camera is final: schedules the
    /// streamed sources' queries when the camera has settled on a view
    /// they have not queried. Nothing when no source streams.
    pub(super) fn fgb_streams_frame(&mut self, cx: &mut gpui::App) {
        if self.fgb_streams.is_empty() || self.camera_in_motion() {
            return;
        }
        let camera = *self.camera_control.camera();
        if !self.fgb_streams.values().any(|s| !s.in_flight && s.queried != Some(camera)) {
            return;
        }
        let Some(entity) = self.self_handle.clone() else { return };
        cx.defer(move |cx| {
            let _ = entity.update(cx, |view, cx| view.query_fgb_streams(cx));
        });
    }

    fn query_fgb_streams(&mut self, cx: &mut gpui::Context<Self>) {
        if self.camera_in_motion() {
            return;
        }
        let camera = *self.camera_control.camera();
        let view_box = viewport_bbox(&camera, STREAM_GROW);
        let runtime = self.runtime.clone();
        for (name, stream) in &mut self.fgb_streams {
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
                        Err(e) => (Err(FgbError::Io(e.to_string())), (0, 0)),
                    },
                    None => cx.background_executor().spawn(async move { query() }).await,
                };
                let _ = entity.update(cx, |view, cx| view.land_fgb_query(&name, view_box, result, counters, cx));
            })
            .detach();
        }
    }

    fn land_fgb_query(
        &mut self,
        name: &str,
        view_box: [f64; 4],
        result: Result<(FgbSource, FgbQueryStats), FgbError>,
        counters: (u64, u32),
        cx: &mut gpui::Context<Self>,
    ) {
        // Out of the map for the push (which borrows the view), back after.
        let Some(mut stream) = self.fgb_streams.remove(name) else { return };
        stream.in_flight = false;
        match result {
            Ok((more, stats)) => {
                stream.stats.queries += 1;
                stream.stats.last_query = stats;
                (stream.stats.bytes_read, stream.stats.requests) = counters;
                stream.covered = Some(view_box);
                stream.thinned = stats.hits > stream.max_features;
                if !more.is_empty() {
                    let fresh: Vec<u64> = (0..more.len()).filter_map(|i| more.file_index(i)).collect();
                    let dropped = stream.source.merge_stream(more, view_box, stream.max_features);
                    stream.loaded.extend(fresh);
                    for id in &dropped {
                        stream.loaded.remove(id);
                    }
                    stream.stats.features_loaded = stream.source.len();
                    if let Err(e) = self.set_source_fgb(name, &stream.source, &stream.paint, cx) {
                        eprintln!("gpui-maps: FlatGeobuf source {name:?}: push failed: {e:?}");
                    }
                }
            }
            Err(e) => {
                eprintln!("gpui-maps: FlatGeobuf source {name:?}: viewport query failed: {e}");
                // Retried on the next settle.
                stream.queried = None;
            }
        }
        self.fgb_streams.insert(name.to_string(), stream);
        cx.notify();
    }
}
