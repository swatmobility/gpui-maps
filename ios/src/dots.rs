//! Population dots on the phone (Task 17, MG-25): the shell half of the
//! engine's `DotLayers`, which decides everything -- which count tiles a
//! frame wants, which request goes next, the view's people per dot, the
//! budget (`DotStyle::for_platform(Platform::Ios)`, `IOS_DOT_BUDGET`).
//! A race archive (race plan, MG-R15: `GPUI_MAP_DOTS=race-dots-2020.pmtiles`)
//! draws Walker's categorical dots on `race_budget_for(Platform::Ios)`
//! (`shell_fetch::shell_dot_style`).
//!
//! What is here is only what a shell has to own: opening the archive (a
//! local PMTiles file in the app's `Documents` folder, chosen by
//! `shell_fetch::phone_dot_archive`), and running the layers' jobs off the
//! main thread. The jobs are native's (`fetch_native::execute`, a file
//! range), run on [`DotWorker`]'s threads the way `MapView` runs them on
//! tokio's blocking pool; each landing wakes the `Wait` loop through the
//! same kind of `EventLoopProxy` wake `net::Fetcher` uses.
//!
//! The archive is COPIED onto the phone, not bundled: the 30″ world is
//! ~102 MB, and a bundle carrying it would re-sign and re-install it on
//! every deploy. `run-device.sh`'s `GPUI_MAP_DOTS_ARCHIVE=<file>` copies
//! it into the app's data container once (`devicectl device copy to`);
//! it survives re-installs of the same bundle id.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::overlay::host::{fetch_native, run_job_catching_panic, shell_fetch, DotJob, DotLanded, DotStyle, Platform};

/// The layer id the phone draws its one dot source as.
pub const DOT_LAYER: &str = "population";

/// Worker threads for count-tile jobs. `DotLayers` caps a source's
/// requests in flight on its own; two keeps one decode running while the
/// next file range is read, and keeps the phone's cores for the frame.
const WORKERS: usize = 2;

/// Threads that run `DotJob`s and hand their results back.
pub struct DotWorker {
    jobs: mpsc::Sender<DotJob>,
    landed: mpsc::Receiver<DotLanded>,
}

impl DotWorker {
    /// Starts the workers; `wake` is called after every landing.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (jobs, job_rx) = mpsc::channel::<DotJob>();
        let (landed_tx, landed) = mpsc::channel::<DotLanded>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        for i in 0..WORKERS {
            let (job_rx, landed_tx, wake) = (job_rx.clone(), landed_tx.clone(), wake.clone());
            std::thread::Builder::new()
                .name(format!("dots-{i}"))
                .spawn(move || loop {
                    // The lock is held only to take one job, never across
                    // the IO.
                    let job = match job_rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    // A job whose run panics must still land (as a
                    // failure), or its request slot is held forever --
                    // `MapView::drive_dot_sources`' rule, shared and
                    // tested (native has no need to call it itself: its
                    // runtime already catches a `spawn_blocking` panic for
                    // it) as `run_job_catching_panic`.
                    let landed = run_job_catching_panic(job, fetch_native::execute);
                    if landed_tx.send(landed).is_err() {
                        return;
                    }
                    wake();
                })
                .expect("failed to start a dot worker thread");
        }
        Self { jobs, landed }
    }

    /// Queues one job.
    pub fn submit(&self, job: DotJob) {
        // The workers only stop when this sender is dropped.
        let _ = self.jobs.send(job);
    }

    /// Every result that has landed since the last call.
    pub fn drain(&self) -> Vec<DotLanded> {
        self.landed.try_iter().collect()
    }
}

/// The app's `Documents` folder: `$HOME/Documents`, where `HOME` is the
/// app's data container on iOS.
fn documents() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join("Documents"))
}

/// The phone's dot source, when one is set up: the archive
/// `shell_fetch::phone_dot_archive` picks for `setting` (`GPUI_MAP_DOTS`),
/// opened, with the phone's style at the archive's epoch (2025 when it has
/// it). `None` -- logged -- when there is no archive or it will not open.
pub fn open_phone_source(
    setting: Option<&str>,
) -> Option<(Arc<dyn crate::overlay::host::GridCountSource>, DotStyle)> {
    let Some(documents) = documents() else {
        crate::mlog!("[dots] no HOME, so no Documents folder: no dots");
        return None;
    };
    let path = match shell_fetch::phone_dot_archive(setting, &documents, |p| p.is_file()) {
        Ok(Some(path)) => path,
        Ok(None) => {
            crate::mlog!(
                "[dots] off: no {} in {} (copy one with GPUI_MAP_DOTS_ARCHIVE=<file> ./run-device.sh)",
                shell_fetch::PHONE_DOT_ARCHIVE,
                documents.display()
            );
            return None;
        }
        Err(why) => {
            crate::mlog!("[dots] off: {why}");
            return None;
        }
    };
    let started = std::time::Instant::now();
    let source = match fetch_native::open_pmtiles_grid(&path.to_string_lossy()) {
        Ok(source) => source,
        Err(err) => {
            crate::mlog!("[dots] could not open {}: {err}", path.display());
            return None;
        }
    };
    let descriptor = crate::overlay::host::GridCountSource::descriptor(&source).clone();
    let Some(epoch) = shell_fetch::pick_epoch(&descriptor.epochs, None) else {
        crate::mlog!("[dots] {} has no epoch", path.display());
        return None;
    };
    // A race archive (`GPUI_MAP_DOTS=race-dots-2020.pmtiles`, race plan
    // MG-R15) draws Walker's categorical dots; anything else the density ramp.
    let race = crate::overlay::host::GridCountSource::race(&source).is_some();
    let style = shell_fetch::shell_dot_style(Platform::Ios, race, epoch, None);
    crate::mlog!(
        "[dots] {} open in {:.0} ms: {}{}\u{2033} grid, {} levels, epochs {:?}, drawing {epoch}, budget {} dots | {}",
        path.display(),
        started.elapsed().as_secs_f64() * 1000.0,
        if race { "race archive, " } else { "" },
        descriptor.base_arcsec,
        descriptor.levels,
        descriptor.epochs,
        style.budget_dots,
        descriptor.attribution,
    );
    Some((Arc::new(source), style))
}
