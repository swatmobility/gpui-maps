//! CSV/TSV sources on a `MapView`: the push ([`MapView::set_source_csv`])
//! and the off-thread load ([`MapView::load_source_csv`]). The parser and
//! the store door are `overlay::csv`'s; this is only where they meet the
//! view's write hooks and its runtime.

use std::path::PathBuf;

use super::MapView;
use crate::overlay::csv::{CsvError, CsvOptions, CsvReport, CsvSource};
use crate::overlay::{FeatureError, FeaturePaint};

/// Where [`MapView::load_source_csv`] reads a CSV from.
#[derive(Debug, Clone)]
pub enum CsvInput {
    /// The file's bytes, already in memory.
    Bytes(Vec<u8>),
    /// A local file.
    Path(PathBuf),
    /// An http(s) URL (needs the `sprite_http_fetch` feature, on by
    /// default).
    Url(String),
}

impl CsvInput {
    /// Reads and parses, blocking.
    fn load(self, options: &CsvOptions) -> Result<CsvSource, CsvError> {
        match self {
            Self::Bytes(bytes) => CsvSource::from_bytes(&bytes, options),
            Self::Path(path) => CsvSource::from_path(path, options),
            Self::Url(url) => CsvSource::from_url(&url, options),
        }
    }
}

impl MapView {
    /// Replaces source `name` with a parsed CSV -- see
    /// [`crate::overlay::csv`]. A CSV of single points takes the point path
    /// (declared [`crate::overlay::ClusterOptions::points_only`] unless
    /// [`Self::set_source_cluster_options`] declared clustering first);
    /// lines, polygons and multipoints take the feature path, tiled when
    /// large. `paint` paints every row; [`Self::declare_source_paint`]
    /// expressions read the CSV's typed columns.
    ///
    /// ```ignore
    /// let csv = CsvSource::from_bytes(bytes, &CsvOptions::default())?;
    /// map.declare_source_paint("cities", &paint_json)?; // ["get", "population"] is a number
    /// map.set_source_csv("cities", &csv, &FeaturePaint::default(), cx)?;
    /// map.set_source_credit("cities", Some("Cities: Natural Earth"));
    /// ```
    pub fn set_source_csv(
        &mut self,
        name: &str,
        csv: &CsvSource,
        paint: &FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), FeatureError> {
        self.overlay_store.set_source_csv(name, csv, paint, self.display_zoom())?;
        self.after_source_write(name, cx);
        if csv.points().is_some() {
            self.drive_point_sources(cx);
        }
        Ok(())
    }

    /// Reads, parses and pushes a CSV without blocking the UI thread: the
    /// read and the parse run on the view's runtime's blocking pool (or
    /// gpui's background executor in a view with no runtime), and the push
    /// ([`Self::set_source_csv`]) lands on the UI thread. The task resolves
    /// to the parse report once the source is installed; detach it to fire
    /// and forget.
    ///
    /// ```ignore
    /// let task = map.load_source_csv("stops", CsvInput::Url(url), CsvOptions::default(), paint, cx);
    /// cx.spawn(async move |_, _| match task.await {
    ///     Ok(report) => eprintln!("stops: {report}"),
    ///     Err(e) => eprintln!("stops: {e}"),
    /// }).detach();
    /// ```
    pub fn load_source_csv(
        &mut self,
        name: &str,
        input: CsvInput,
        options: CsvOptions,
        paint: FeaturePaint,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Task<Result<CsvReport, CsvError>> {
        let name = name.to_string();
        let runtime = self.runtime.clone();
        cx.spawn(async move |entity, cx| {
            let load = move || input.load(&options);
            let csv = match &runtime {
                Some(runtime) => runtime.spawn_blocking(load).await.map_err(|e| CsvError::Io(e.to_string()))??,
                None => cx.background_executor().spawn(async move { load() }).await?,
            };
            let report = csv.report().clone();
            let pushed = entity.update(cx, |view, cx| view.set_source_csv(&name, &csv, &paint, cx));
            // The rows are in the store now; a big parse is freed off the
            // UI thread.
            if let Some(runtime) = &runtime {
                drop(runtime.spawn_blocking(move || drop(csv)));
            }
            match pushed {
                Ok(Ok(())) => Ok(report),
                Ok(Err(e)) => Err(CsvError::Push(e.to_string())),
                Err(_) => Err(CsvError::Push("the map was closed before the CSV loaded".into())),
            }
        })
    }
}
