//! Spec §8.3's wall-clock ban, as a text scan over EVERY file under
//! `src/story`: frame `n` is a pure function of `t = n / fps`, and a clock
//! read anywhere that decides a frame would make it a function of when it
//! was rendered.
//!
//! - **Banned everywhere:** `SystemTime`, `Sun::Now` (the wall-clock sun),
//!   `now_unix` (`solar::now_unix_seconds`), `Local::now`, `Utc::now`.
//!   That includes `player.rs`, which builds every request's sun.
//! - **`Instant` and `now(`** are banned too, except in the named
//!   exemptions below, which may read `Instant::now()` for the reason
//!   given and for nothing else.
//!
//! A new story file is scanned by default; exempting it takes a line here
//! with a reason.

/// Files that may read `Instant::now()`, and why.
const INSTANT_EXEMPT: &[(&str, &str)] = &[
    ("player.rs", "measures a frame (ms, wait_ms, progress); never decides one"),
    ("prefetch.rs", "bounds a frame's tile wait; what a frame waits for never depends on it"),
    ("sink.rs", "bounds how long ffmpeg may stall (kills it); frame bytes never depend on it"),
];

const ALWAYS_BANNED: &[&str] = &["SystemTime", "Sun::Now", "now_unix", "Local::now", "Utc::now"];
const INSTANT_BANNED: &[&str] = &["Instant", "now("];

/// Every clock read in `text` as `(line number, token, line)`, comments
/// stripped. `instant_ok` allows `Instant` and `Instant::now()` only.
fn clock_hits(text: &str, instant_ok: bool) -> Vec<(usize, &'static str, String)> {
    let mut hits = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let checked = if instant_ok { code.replace("Instant::now(", "") } else { code.to_string() };
        let banned = ALWAYS_BANNED.iter().chain(if instant_ok { &[][..] } else { INSTANT_BANNED });
        for token in banned {
            if checked.contains(token) {
                hits.push((i + 1, *token, line.trim().to_string()));
            }
        }
    }
    hits
}

#[test]
fn no_story_file_reads_a_wall_clock_to_decide_a_frame() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/story");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "the scan found the story files: {files:?}");
    let mut report = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_str().unwrap();
        let instant_ok = INSTANT_EXEMPT.iter().any(|(f, _)| *f == name);
        let text = std::fs::read_to_string(path).unwrap();
        for (line, token, code) in clock_hits(&text, instant_ok) {
            report.push(format!("src/story/{name}:{line}: `{token}` in {code}"));
        }
    }
    assert!(report.is_empty(), "a story file reads a wall clock:\n{}", report.join("\n"));
}

/// The real scanner on a synthetic file: each hazard is caught, a comment
/// is not, and an exemption allows `Instant::now()` but nothing else.
#[test]
fn the_scanner_catches_each_clock_and_ignores_comments() {
    let file = "\
let t0 = std::time::Instant::now();
let wall = std::time::SystemTime::now();
let sun = Sun::Now;
let unix = solar::now_unix_seconds();
// Instant, SystemTime and Sun::Now in a comment are fine
let fine = n as f64 / fps;
";
    let strict: Vec<(usize, &str)> = clock_hits(file, false).into_iter().map(|(l, t, _)| (l, t)).collect();
    assert_eq!(
        strict,
        vec![(1, "Instant"), (1, "now("), (2, "SystemTime"), (2, "now("), (3, "Sun::Now"), (4, "now_unix")]
    );
    let exempt: Vec<(usize, &str)> = clock_hits(file, true).into_iter().map(|(l, t, _)| (l, t)).collect();
    assert_eq!(exempt, vec![(2, "SystemTime"), (3, "Sun::Now"), (4, "now_unix")]);
}
