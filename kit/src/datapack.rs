//! Data a page needs that is too large to ship inside Studio, fetched by
//! the app itself: the missing-data card's "Download" button, never a
//! script for the user to run: getting data is one press in the app, never
//! a second toolchain.
//!
//! A [`Pack`] is one prebuilt file. [`start`] streams it to `<dest>.part`
//! (resuming a partial one with an HTTP `Range`), checks its SHA-256 (the
//! pack's own constant, else the host's `<url>.sha256` sidecar), then
//! renames it into place. [`progress`] is what the card draws; [`cancel`]
//! stops it, keeping the partial file for the next resume.
//!
//! Small data ships inside the app instead: embedded (`kit::asset_bytes`)
//! or in the `.app`'s `Contents/Resources/data` ([`bundled_data_dir`],
//! filled by `scripts/bundle-studio.sh`).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest as _, Sha256};

/// Where the packs are hosted. TODO(hosting): there is no public data host
/// yet; until there is, a download fails with a message naming
/// [`BASE_URL_VAR`], the override that points at one.
pub const DEFAULT_BASE_URL: Option<&str> = None;

/// Overrides [`DEFAULT_BASE_URL`]: `GPUI_MAPS_DATA_BASE_URL=https://host/packs/v1`.
pub const BASE_URL_VAR: &str = "GPUI_MAPS_DATA_BASE_URL";

/// One downloadable file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pack {
    /// The file name, on the host (under the base URL) and on disk.
    pub file: &'static str,
    /// The expected SHA-256 (hex), when known; else the host's
    /// `<url>.sha256` sidecar is required.
    pub sha256: Option<&'static str>,
}

/// Every pack the gallery can download, by file name.
pub const PACKS: &[Pack] = &[
    // US block groups, simplified (TIGER/Line 2023 + ACS 2023, public
    // domain): `tools/census-pack`'s output.
    Pack { file: "us_bg_2023.bgpk", sha256: Some("bc72aca39bad5181e8e200c46d74bc50156927b3a9231b0ef1117622ce765d31") },
    // GHSL population grid, 30" (European Commission, CC BY 4.0):
    // `tools/dotgrid-build`'s output.
    Pack { file: "E2025_30ss.pmtiles", sha256: None },
    // Census 2020 race and ethnicity (public domain): the race map's
    // three archives.
    Pack { file: "race-dots-2020.pmtiles", sha256: None },
    Pack { file: "race-hex-2020.pmtiles", sha256: None },
    Pack { file: "race-clusters-2020.pmtiles", sha256: None },
];

/// The pack that provides `dest` (matched by file name), if any.
pub fn pack_for(dest: &Path) -> Option<Pack> {
    let name = dest.file_name()?.to_str()?;
    PACKS.iter().copied().find(|p| p.file == name)
}

/// The base URL in effect, if any.
pub fn base_url() -> Option<String> {
    std::env::var(BASE_URL_VAR).ok().filter(|v| !v.trim().is_empty()).or(DEFAULT_BASE_URL.map(str::to_string)).map(|u| u.trim_end_matches('/').to_string())
}

/// `name`'s folder inside the running `.app`
/// (`Contents/Resources/data/<name>`), when the binary runs from a bundle
/// that ships it.
pub fn bundled_data_dir(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("Resources/data").join(name);
    dir.is_dir().then_some(dir)
}

/// Where a download is.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running,
    Verifying,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub state: State,
    pub done: u64,
    pub total: Option<u64>,
}

struct Job {
    progress: Mutex<Progress>,
    cancel: AtomicBool,
}

fn jobs() -> &'static Mutex<HashMap<PathBuf, Arc<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

/// The download into `dest`, if one was started this session.
pub fn progress(dest: &Path) -> Option<Progress> {
    let job = jobs().lock().ok()?.get(dest).cloned()?;
    let p = job.progress.lock().ok()?.clone();
    Some(p)
}

/// Stops the download into `dest`; the partial file stays for a resume.
pub fn cancel(dest: &Path) {
    if let Some(job) = jobs().lock().ok().and_then(|j| j.get(dest).cloned()) {
        job.cancel.store(true, Ordering::Relaxed);
    }
}

/// Starts (or resumes) downloading `pack` into `dest` on `runtime`. A
/// download already running for `dest` is left alone.
pub fn start(pack: Pack, dest: PathBuf, runtime: &tokio::runtime::Handle) {
    let job = Arc::new(Job { progress: Mutex::new(Progress { state: State::Running, done: 0, total: None }), cancel: AtomicBool::new(false) });
    {
        let Ok(mut all) = jobs().lock() else { return };
        if all.get(&dest).is_some_and(|j| j.progress.lock().is_ok_and(|p| matches!(p.state, State::Running | State::Verifying))) {
            return;
        }
        all.insert(dest.clone(), Arc::clone(&job));
    }
    runtime.spawn(async move {
        let outcome = run(pack, &dest, &job).await;
        let Ok(mut p) = job.progress.lock() else { return };
        p.state = match outcome {
            Ok(()) => State::Done,
            Err(_) if job.cancel.load(Ordering::Relaxed) => State::Cancelled,
            Err(e) => State::Failed(e),
        };
    });
}

fn set(job: &Job, f: impl FnOnce(&mut Progress)) {
    if let Ok(mut p) = job.progress.lock() {
        f(&mut p);
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

async fn run(pack: Pack, dest: &Path, job: &Job) -> Result<(), String> {
    let base = base_url().ok_or_else(|| format!("no download host is set up for this build yet (set {BASE_URL_VAR})"))?;
    let url = format!("{base}/{}", pack.file);
    let client = reqwest::Client::builder()
        .user_agent(gpui_maps::tile_source::http::TILE_USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;
    let expected = match pack.sha256 {
        Some(s) => s.to_string(),
        None => {
            let text = client.get(format!("{url}.sha256")).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("checksum: {e}"))?.text().await.map_err(|e| e.to_string())?;
            text.split_whitespace().next().unwrap_or("").to_ascii_lowercase()
        }
    };
    if expected.len() != 64 {
        return Err("the host gave no usable SHA-256 for this pack".into());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let part = part_path(dest);
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut request = client.get(&url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = request.send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    let resumed = have > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let start_at = if resumed { have } else { 0 };
    let total = response.content_length().map(|n| n + start_at);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)
        .map_err(|e| format!("{}: {e}", part.display()))?;
    set(job, |p| {
        p.done = start_at;
        p.total = total;
    });
    let mut done = start_at;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if job.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        set(job, |p| p.done = done);
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    set(job, |p| p.state = State::Verifying);
    let got = sha256_file(&part)?;
    if got != expected {
        let _ = std::fs::remove_file(&part);
        return Err(format!("checksum mismatch (got {got}); the partial file was removed, try again"));
    }
    std::fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// The SHA-256 of a file, in hex.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Read as _};

    /// A one-file HTTP server that honours `Range`, plus the sidecar.
    fn serve(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sum: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut path, mut from) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(rest) = line.strip_prefix("GET ") {
                        path = rest.split(' ').next().unwrap_or("").to_string();
                    }
                    if let Some(r) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        from = r.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                }
                let (status, payload) = if path.ends_with(".sha256") {
                    ("200 OK", format!("{sum}  pack.bin\n").into_bytes())
                } else if from > 0 {
                    ("206 Partial Content", body[from..].to_vec())
                } else {
                    ("200 OK", body.clone())
                };
                let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).into_bytes();
                r.extend_from_slice(&payload);
                let _ = std::io::Write::write_all(&mut stream, &r);
                let _ = stream.read(&mut [0u8; 1]);
            }
        });
        format!("http://{addr}")
    }

    fn wait(dest: &Path) -> Progress {
        for _ in 0..400 {
            let p = progress(dest).unwrap();
            if !matches!(p.state, State::Running | State::Verifying) {
                return p;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the download did not finish");
    }

    #[test]
    fn a_pack_downloads_resumes_and_is_checked() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let base = serve(body.clone());
        std::env::set_var(BASE_URL_VAR, &base);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pack.bin");
        // A partial file from an earlier, cancelled run: resumed, not refetched.
        std::fs::write(part_path(&dest), &body[..50_000]).unwrap();
        start(Pack { file: "pack.bin", sha256: None }, dest.clone(), rt.handle());
        let p = wait(&dest);
        assert_eq!(p.state, State::Done, "{p:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
        // A wrong checksum: refused, nothing put in place.
        let bad = dir.path().join("bad.bin");
        start(Pack { file: "bad.bin", sha256: Some("00".repeat(32).leak()) }, bad.clone(), rt.handle());
        assert!(matches!(wait(&bad).state, State::Failed(ref e) if e.contains("checksum")));
        assert!(!bad.exists());
        std::env::remove_var(BASE_URL_VAR);
    }

    #[test]
    fn every_pack_is_found_by_its_file_name() {
        for p in PACKS {
            assert_eq!(pack_for(&Path::new("/x").join(p.file)), Some(*p));
            assert!(p.sha256.is_none_or(|s| s.len() == 64));
        }
        assert_eq!(pack_for(Path::new("/x/manifest.json")), None);
    }
}
