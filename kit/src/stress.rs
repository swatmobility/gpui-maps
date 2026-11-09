//! The measurement discipline `examples/stress_suite.rs` is built on.
//!
//! This module holds no scenario and measures nothing by itself. It exists
//! because **five measurement artifacts were produced against this
//! renderer in one week**, and every one of them came from the instrument
//! rather than from the renderer:
//!
//! | the artifact | what the instrument did wrong |
//! |---|---|
//! | "111 MB/s, so the 3.76 MB/s report is wrong" | read a warm page cache and called it a disk |
//! | "swap is at 95%, we are thrashing" | read a LEVEL where the question was a RATE |
//! | "38 ms is inside your map view" | measured somebody else's tick interval |
//! | "the frame time decays over the session" (twice) | a cumulative mean over growing content |
//! | "the overlay push degrades 8.6x" | one run, no payload check, no environment |
//!
//! So the primitives here are shaped to make each of those hard to
//! reproduce:
//!
//! - [`Samples`] reports **p50, p99 and the worst sample, and has no `mean`
//!   at all.** A mean over frame times hides exactly the stalls a user
//!   feels, and a *cumulative* mean over growing content climbs for ever
//!   with no defect anywhere -- that is two of the five, and the second one
//!   happened twice.
//! - [`Samples::over_budget`] grades against [`FRAME_BUDGET_MS`], which is
//!   **8.333 ms and not 16.7**: this display runs at 120 Hz, so a 9 ms
//!   stage misses a frame on its own.
//! - [`EnvWindow`] records the environment either side of every reported
//!   window: load average, the paging **RATE** from `vm_stat` deltas (never
//!   the swap level), and the resident set of the largest processes on the
//!   machine. A result without its environment is not reproducible, and a
//!   machine with a 4.5 GB virtual machine on it is not the machine the
//!   baseline was taken on.
//! - [`Payload`] fingerprints what is being measured and re-checks it every
//!   window, so "identical content" is a checked fact rather than an
//!   assumption. Two of the five artifacts died on exactly this point.
//! - [`Baseline`] holds the committed numbers with a per-metric tolerance,
//!   so a regression **fails a run** instead of waiting for somebody to
//!   notice a number drifting.
//!
//! Nothing here spawns a thread or holds a lock across an await. Every
//! reader is a `std::process::Command` on a shell utility, called at the
//! edges of a window and never inside one, so the instrument cannot appear
//! in its own measurement.

#![allow(dead_code)] // Each scenario uses a different subset.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The frame budget.
// ---------------------------------------------------------------------------

/// One frame at 120 Hz, in milliseconds.
///
/// **8.333, not 16.7.** The display this suite was calibrated on runs at
/// 120 Hz, so a stage that takes 9 ms drops a frame on its own even if
/// every other stage is free. Grading against a 60 Hz budget would have
/// passed a stage that is visibly janky here, which is the whole reason
/// this is a named constant rather than a literal at each call site.
pub const FRAME_BUDGET_MS: f64 = 1000.0 / 120.0;

/// The 60 Hz budget, reported alongside so a number can be read by someone
/// on other hardware. Never used as the pass/fail line.
pub const FRAME_BUDGET_60HZ_MS: f64 = 1000.0 / 60.0;

// ---------------------------------------------------------------------------
// Samples: percentiles, never means.
// ---------------------------------------------------------------------------

/// A window's own timings, in milliseconds.
///
/// # There is deliberately no `mean`
///
/// Two of the five artifacts this suite is built against were means. One
/// was a *cumulative* mean over content that grew during the session --
/// which rises monotonically for ever with no defect present, and was read
/// as time-based decay twice, once by us and once by a peer team. The other
/// was a per-frame mean that hid a 40 ms stall inside a 9 ms average.
///
/// p50 says what the common case is, p99 says what the user notices, and
/// [`Self::worst`] says what the worst single frame in the window was. None
/// of those can be manufactured by adding more samples of the same thing,
/// which is the property a mean lacks.
#[derive(Debug, Clone, Default)]
pub struct Samples {
    values: Vec<f64>,
}

impl Samples {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_ms(&mut self, ms: f64) {
        self.values.push(ms);
    }

    pub fn push(&mut self, elapsed: Duration) {
        self.values.push(elapsed.as_secs_f64() * 1e3);
    }

    #[must_use]
    pub fn n(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every sample, in the order they were taken.
    ///
    /// Exposed because a scenario is required to be able to **log every
    /// frame in its suspect window**, not just the ones over a threshold: a
    /// threshold-triggered log can show that a stall happened but can never
    /// show that one did not, and "no line was printed" is equally
    /// consistent with "the instrument was not running".
    #[must_use]
    pub fn raw(&self) -> &[f64] {
        &self.values
    }

    fn percentile(&self, q: f64) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        // Nearest-rank, which is the only definition that cannot invent a
        // value that no sample had. With n < 100 an interpolated p99 is a
        // weighted guess between the two worst frames; this reports one of
        // them.
        let rank = ((q * sorted.len() as f64).ceil() as usize).max(1) - 1;
        sorted[rank.min(sorted.len() - 1)]
    }

    #[must_use]
    pub fn p50(&self) -> f64 {
        self.percentile(0.50)
    }

    #[must_use]
    pub fn p99(&self) -> f64 {
        self.percentile(0.99)
    }

    #[must_use]
    pub fn worst(&self) -> f64 {
        self.values.iter().copied().fold(0.0, f64::max)
    }

    #[must_use]
    pub fn best(&self) -> f64 {
        self.values.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// How many samples missed a 120 Hz frame on their own.
    #[must_use]
    pub fn over_budget(&self) -> usize {
        self.values.iter().filter(|v| **v > FRAME_BUDGET_MS).count()
    }

    /// `p50 / p99 / worst`, with the budget overrun spelled out. The only
    /// formatter any scenario should use for a timing.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "n {:>5}  p50 {:>8.3}  p99 {:>8.3}  worst {:>9.3} ms  \
             | over 8.33ms: {}/{} ({:.0}%)",
            self.n(),
            self.p50(),
            self.p99(),
            self.worst(),
            self.over_budget(),
            self.n(),
            if self.n() == 0 {
                0.0
            } else {
                100.0 * self.over_budget() as f64 / self.n() as f64
            },
        )
    }

    /// `p50 / p99 / worst` in a unit that is not milliseconds.
    ///
    /// Separate from [`Self::line`] because the budget column is
    /// meaningless off the time axis: scenario 3 reports megabytes per
    /// retained tile through this type for its percentiles, and printing
    /// "over 8.33ms: 6/6 (100%)" against a megabyte figure is worse than
    /// useless -- it is a number that looks like a verdict and is not one.
    #[must_use]
    pub fn line_in(&self, unit: &str) -> String {
        format!(
            "n {:>5}  p50 {:>8.3}  p99 {:>8.3}  worst {:>9.3} {unit}  (best {:>8.3})",
            self.n(),
            self.p50(),
            self.p99(),
            self.worst(),
            if self.is_empty() { 0.0 } else { self.best() },
        )
    }

}

// ---------------------------------------------------------------------------
// Payload constancy.
// ---------------------------------------------------------------------------

/// A fingerprint of the bytes a scenario is measuring against, so
/// "identical content" is checked rather than assumed.
///
/// # Why this is not optional
///
/// The retracted "8.6x overlay push degradation" compared two windows whose
/// content had grown between them. The retracted peer frame-time decay did
/// the same. Both would have been caught here at zero cost: hashing a
/// payload already in cache is one pass, and it turns an assumption into a
/// failed run.
///
/// It matters for a second reason this suite discovered while being built.
/// A tile server backed by a database need not be **byte-stable**: the
/// same `z/x/y` fetched four times can return four different digests when
/// the database does not pin its feature order. So a scenario that
/// re-fetched its tile per iteration would be measuring a different tile
/// each time. Every scenario here fetches ONCE, pins the bytes for the
/// whole run, and re-asserts this fingerprint at each window boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payload {
    pub digest: u64,
    pub bytes: usize,
    /// A scenario-defined count: features, vehicles, stops, tiles. Recorded
    /// beside the digest because the digest is *not* comparable across runs
    /// on a non-deterministic source, and this usually is.
    pub items: usize,
}

impl Payload {
    /// FNV-1a over the bytes. Not cryptographic and not meant to be: the
    /// question is "did this change", against an adversary that is a race
    /// condition rather than a person.
    #[must_use]
    pub fn of(bytes: &[u8], items: usize) -> Self {
        let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            digest ^= u64::from(*byte);
            digest = digest.wrapping_mul(0x1000_0000_01b3);
        }
        Self { digest, bytes: bytes.len(), items }
    }

    /// The same, over a list of coordinate runs -- the overlay case, where
    /// there are no wire bytes to hash.
    #[must_use]
    pub fn of_coords(coords: &[Vec<[f64; 2]>]) -> Self {
        let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
        let mut count = 0usize;
        for run in coords {
            for point in run {
                for value in point {
                    digest ^= value.to_bits();
                    digest = digest.wrapping_mul(0x1000_0000_01b3);
                }
                count += 1;
            }
        }
        Self { digest, bytes: count * 16, items: coords.len() }
    }

    /// Panics if the payload moved. Called at every window boundary.
    pub fn assert_unchanged(&self, now: Payload, where_: &str) {
        assert_eq!(
            *self, now,
            "{where_}: the payload changed mid-run -- every comparison in \
             this run is void. expected {self:?}, found {now:?}"
        );
    }

    #[must_use]
    pub fn line(&self) -> String {
        format!("{} items, {} bytes, digest {:#018x}", self.items, self.bytes, self.digest)
    }
}

// ---------------------------------------------------------------------------
// The environment, recorded with every result.
// ---------------------------------------------------------------------------

/// What the machine was doing while a window was measured.
///
/// # Rate, not level
///
/// `paging` is a **rate**, computed from two `vm_stat` readings and the
/// elapsed seconds between them. One of the five artifacts was a swap
/// LEVEL of 95% read as thrashing while the paging RATE was 0 KB/s -- a
/// level says how much has ever been paged out and stayed out, which on a
/// long-lived macOS session is always high and says nothing at all about
/// whether the machine is paging *now*.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub load_1m: f64,
    pub load_5m: f64,
    pub cpus: usize,
    /// Pages per second across the window, from `vm_stat` deltas.
    pub pageins_per_s: f64,
    pub pageouts_per_s: f64,
    pub swapins_per_s: f64,
    pub swapouts_per_s: f64,
    /// Pages per second the compressor took in.
    ///
    /// Reported beside the swap rates because **on this platform it is the
    /// one that moves first.** macOS compresses memory long before it
    /// swaps, so a machine under real pressure shows compressions in the
    /// thousands while `Swapouts` is still flat -- and a suite that only
    /// watched swap would call that machine quiet. The 95%-swap-level
    /// artifact was the mirror image of this mistake: the wrong statistic,
    /// read confidently.
    pub compressions_per_s: f64,
    /// `(command, rss_bytes)` for the largest few processes, at close.
    pub top_rss: Vec<(String, u64)>,
    pub own_rss_bytes: u64,
    pub own_heap_bytes: u64,
    pub window_secs: f64,
    /// Bytes per page, so a pages-per-second rate can be read as a
    /// bandwidth without a second syscall. 16,384 on this machine, which is
    /// worth knowing before comparing a page rate with one taken on a 4 KiB
    /// platform.
    pub page_size: u64,
}

impl Env {
    /// One line, and it must be printed with every result. The tolerance in
    /// the committed baseline is wide precisely because this line varies,
    /// and a number without it cannot be told from a number taken under a
    /// different load.
    #[must_use]
    pub fn line(&self) -> String {
        let top = self
            .top_rss
            .iter()
            .map(|(name, rss)| format!("{name} {:.1}GB", *rss as f64 / 1e9))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "env: load {:.2}/{:.2} on {} cpus | RATE/s (not level): pagein {:.0} \
             pageout {:.0} swapin {:.0} swapout {:.0} compress {:.0} x {} B \
             | own rss {:.0} MB heap {:.0} MB | top: {top}",
            self.load_1m,
            self.load_5m,
            self.cpus,
            self.pageins_per_s,
            self.pageouts_per_s,
            self.swapins_per_s,
            self.swapouts_per_s,
            self.compressions_per_s,
            self.page_size,
            self.own_rss_bytes as f64 / 1e6,
            self.own_heap_bytes as f64 / 1e6,
        )
    }

    /// True when the machine was quiet enough that a timing taken on it can
    /// be compared with a committed baseline.
    ///
    /// Not a pass/fail gate on the scenario -- it is a statement about
    /// whether the scenario's own verdict is worth believing. A peer saw
    /// **6.7x variance between runs from system contention alone**, so a
    /// suite that reported a regression without saying whether the machine
    /// was busy would reproduce artifact number one on a schedule.
    /// The thresholds are deliberately loose. A window marked not-quiet
    /// downgrades every failure in it to advisory, which WEAKENS the suite,
    /// so a jumpy definition of "busy" would quietly turn the whole thing
    /// into a report nobody fails. 2,000 compressions/s at 16 KiB a page is
    /// 32 MB/s of memory being squeezed, which is real pressure and not
    /// background housekeeping.
    #[must_use]
    pub fn quiet(&self) -> bool {
        self.load_1m < self.cpus as f64 * 0.75
            && self.pageouts_per_s < 100.0
            && self.swapouts_per_s < 100.0
            && self.compressions_per_s < 2000.0
    }
}

/// A window being measured. [`Self::close`] turns it into an [`Env`].
pub struct EnvWindow {
    opened: Instant,
    vm: BTreeMap<String, u64>,
}

impl EnvWindow {
    #[must_use]
    pub fn open() -> Self {
        Self { opened: Instant::now(), vm: read_vm_stat() }
    }

    #[must_use]
    pub fn close(self) -> Env {
        let secs = self.opened.elapsed().as_secs_f64().max(1e-9);
        let now = read_vm_stat();
        let rate = |key: &str| {
            let before = self.vm.get(key).copied().unwrap_or(0);
            let after = now.get(key).copied().unwrap_or(0);
            after.saturating_sub(before) as f64 / secs
        };
        let (load_1m, load_5m) = read_loadavg();
        Env {
            load_1m,
            load_5m,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            pageins_per_s: rate("Pageins"),
            pageouts_per_s: rate("Pageouts"),
            swapins_per_s: rate("Swapins"),
            swapouts_per_s: rate("Swapouts"),
            compressions_per_s: rate("Compressions"),
            top_rss: read_top_rss(3),
            own_rss_bytes: gpui_maps::render::rss_bytes(),
            own_heap_bytes: gpui_maps::render::heap_in_use_bytes(),
            window_secs: secs,
            page_size: now.get("__page_size").copied().unwrap_or(4096),
        }
    }
}

/// `vm_stat`'s counters, by the label it prints them under.
///
/// Parsed rather than read from `host_statistics64` because a shelling-out
/// reader is auditable from the terminal: anyone doubting a number here can
/// run `vm_stat` twice themselves and subtract. The page size is stashed
/// under `__page_size` so a pages-per-second figure can be turned into
/// bytes without a second syscall.
fn read_vm_stat() -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(result) = std::process::Command::new("vm_stat").output() else {
        return out;
    };
    let text = String::from_utf8_lossy(&result.stdout);
    for line in text.lines() {
        if let Some(size) = line.strip_prefix("Mach Virtual Memory Statistics: (page size of ") {
            if let Some(n) = size.split_whitespace().next().and_then(|s| s.parse().ok()) {
                out.insert("__page_size".to_string(), n);
            }
            continue;
        }
        let Some((label, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_end_matches('.');
        if let Ok(n) = value.parse::<u64>() {
            out.insert(label.trim().to_string(), n);
        }
    }
    out
}

fn read_loadavg() -> (f64, f64) {
    let Ok(result) = std::process::Command::new("sysctl").arg("-n").arg("vm.loadavg").output()
    else {
        return (0.0, 0.0);
    };
    let text = String::from_utf8_lossy(&result.stdout);
    let mut fields = text.trim().trim_matches(|c| c == '{' || c == '}').split_whitespace();
    let one = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let five = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    (one, five)
}

/// The `n` largest resident processes, as `(basename, bytes)`.
///
/// Here because the machine this suite was calibrated on had a 4.5 GB
/// virtual machine resident throughout, and a timing taken beside it is not
/// a timing taken without it. `ps` reports RSS in KiB.
fn read_top_rss(n: usize) -> Vec<(String, u64)> {
    let Ok(result) =
        std::process::Command::new("ps").arg("-Ao").arg("rss=,comm=").output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&result.stdout);
    let mut rows: Vec<(String, u64)> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (rss, comm) = line.split_once(char::is_whitespace)?;
            let rss: u64 = rss.trim().parse().ok()?;
            let name = comm.trim().rsplit('/').next().unwrap_or(comm).to_string();
            Some((name, rss * 1024))
        })
        .collect();
    rows.sort_by_key(|(_, rss)| std::cmp::Reverse(*rss));
    rows.truncate(n);
    rows
}

// ---------------------------------------------------------------------------
// Cold and warm as separate arms.
// ---------------------------------------------------------------------------

/// Which arm a figure belongs to. Never summed together.
///
/// "Cold and warm are one run" is how the 111 MB/s artifact happened: a
/// read off the page cache was reported as a disk figure and used to
/// dismiss a correct 3.76 MB/s measurement. The cold arm is where the
/// defects live and it has a sample size of one by construction, so it is
/// reported as a single number with no percentile and never averaged into
/// the warm arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// The first touch, on an instance that has never done this before.
    /// One sample, reported as itself.
    Cold,
    /// Every touch after that.
    Warm,
}

impl Arm {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Arm::Cold => "cold",
            Arm::Warm => "warm",
        }
    }
}

// ---------------------------------------------------------------------------
// One reported window.
// ---------------------------------------------------------------------------

/// One disjoint window's own numbers.
///
/// **Disjoint, never cumulative.** Every counter in here is a delta read
/// at this window's own two edges, and the windows do not overlap. A
/// running total divided by a running count is what produced two of the
/// five artifacts, and it is not expressible with this type.
pub struct Window {
    pub label: String,
    pub arm: Arm,
    pub samples: Samples,
    pub env: Env,
    pub payload: Payload,
    /// Scenario-specific counter deltas across this window alone.
    pub counters: BTreeMap<&'static str, u64>,
}

impl Window {
    #[must_use]
    pub fn counter(&self, key: &str) -> u64 {
        self.counters.get(key).copied().unwrap_or(0)
    }

    /// Prints the window: the timing line, the counters, then the
    /// environment. In that order, always, so two runs can be diffed.
    pub fn report(&self) {
        println!("  {:<22} [{}] {}", self.label, self.arm.label(), self.samples.line());
        if !self.counters.is_empty() {
            let counters = self
                .counters
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join("  ");
            println!("    counters: {counters}");
        }
        println!("    {}", self.env.line());
        if !self.env.quiet() {
            println!(
                "    NOTE: the machine was NOT quiet during this window -- \
                 treat this figure as an upper bound, not a baseline sample"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The committed baseline.
// ---------------------------------------------------------------------------

/// One metric's committed value and the band a run may land in.
///
/// Deserialised from `stress/baseline.json`, which is the committed file a
/// future run fails against. Owned `String`s rather than `&'static str`
/// because the rationale comes out of that file: a tolerance whose
/// justification lived in the source and whose number lived in JSON would
/// drift apart, and the justification is the half that stops a wide
/// tolerance from looking arbitrary.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Bound {
    pub baseline: f64,
    /// Multiplicative ceiling. `2.0` means "twice the baseline fails".
    pub tolerance: f64,
    pub unit: String,
    /// Why this tolerance and not a tighter one. Printed on failure, so a
    /// person reading a red run learns whether to believe it.
    pub rationale: String,
    /// Optional multiplicative FLOOR: `observed < baseline * floor` fails.
    /// For a count that is exact by construction (vertices baked at a fixed
    /// camera), where fewer is as much a change as more. Absent in every
    /// older baseline, which therefore grades exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// Absolute allowance added to the ceiling: `baseline * tolerance +
    /// slack`. For a count whose baseline is 0 (frames over 16.7 ms), where
    /// no multiplier admits anything. 0 in every older baseline.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub slack: f64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

impl Bound {
    /// The largest observed value that passes.
    #[must_use]
    pub fn ceiling(&self) -> f64 {
        self.baseline * self.tolerance + self.slack
    }
}

/// The committed baseline: every graded metric, by name.
///
/// # Why a file and not constants in the source
///
/// A number in the source is edited by whoever is annoyed by a red run. A
/// number in a committed file next to a rationale, read by a suite that
/// exits non-zero, is a decision somebody has to make on purpose and
/// explain in a diff. That is the whole difference between a baseline and a
/// comment.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Baseline {
    /// Free-form provenance: the machine, the date, the load it was taken
    /// under. Printed with the grades, because a baseline whose environment
    /// is unknown grades nothing.
    #[serde(default)]
    pub taken_on: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub metrics: BTreeMap<String, Bound>,
}

impl Baseline {
    /// The committed baseline, or an empty one with the reason printed.
    ///
    /// A missing baseline is NOT an error: the first run on a new machine
    /// has nothing to compare against, and it should still print its
    /// numbers so they can become one. What it must not do is silently
    /// report "0 regressions".
    #[must_use]
    pub fn load(path: &str) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Baseline>(&bytes) {
                Ok(baseline) => baseline,
                Err(error) => {
                    println!("baseline {path}: {error} -- grading nothing this run");
                    Baseline::default()
                }
            },
            Err(error) => {
                println!("baseline {path}: {error} -- grading nothing this run");
                Baseline::default()
            }
        }
    }

    #[must_use]
    pub fn bound(&self, metric: &str) -> Option<&Bound> {
        self.metrics.get(metric)
    }
}

/// The result of grading one metric.
pub struct Verdict {
    pub metric: String,
    pub observed: f64,
    pub bound: Bound,
    pub pass: bool,
    /// True when [`Env::quiet`] was false for the window this came from, in
    /// which case a failure is advisory. A busy machine producing a slow
    /// number is not a regression, and a suite that could not tell the
    /// difference would cry wolf until nobody ran it.
    pub advisory: bool,
}

impl Verdict {
    pub fn report(&self) {
        let ceiling = self.bound.ceiling();
        let state = match (self.pass, self.advisory) {
            (true, _) => "PASS",
            (false, true) => "SLOW (advisory: machine was busy)",
            (false, false) => "FAIL",
        };
        println!(
            "  {:<44} {:>10.3} {:<6} vs {:.3} x{:.1} = {:.3}  {state}",
            self.metric,
            self.observed,
            self.bound.unit,
            self.bound.baseline,
            self.bound.tolerance,
            ceiling,
        );
        if !self.pass {
            println!("      tolerance: {}", self.bound.rationale);
        }
    }
}

/// Grades `observed` against `bound`, honestly.
#[must_use]
pub fn grade(metric: impl Into<String>, observed: f64, bound: Bound, quiet: bool) -> Verdict {
    let pass = observed <= bound.ceiling() && bound.floor.is_none_or(|f| observed >= bound.baseline * f);
    Verdict { metric: metric.into(), observed, bound, pass, advisory: !quiet }
}

/// Accumulates one run's grades, and its ungraded observations.
///
/// The second half matters as much as the first. A scenario that reports a
/// number the baseline has no entry for is producing something nobody can
/// regress against, and a suite that printed it beside graded numbers would
/// make the coverage look better than it is. So they are counted
/// separately and named in the summary.
pub struct Grader {
    pub baseline: Baseline,
    pub verdicts: Vec<Verdict>,
    pub ungraded: Vec<(String, f64, String)>,
    pub skipped: Vec<String>,
}

impl Grader {
    #[must_use]
    pub fn new(baseline: Baseline) -> Self {
        Self { baseline, verdicts: Vec::new(), ungraded: Vec::new(), skipped: Vec::new() }
    }

    /// Grades `observed` if the baseline has a bound for `metric`, and
    /// records it as ungraded if not.
    pub fn observe(&mut self, metric: &str, observed: f64, unit: &str, quiet: bool) {
        match self.baseline.bound(metric).cloned() {
            Some(bound) => self.verdicts.push(grade(metric, observed, bound, quiet)),
            None => self.ungraded.push((metric.to_string(), observed, unit.to_string())),
        }
    }

    pub fn skip(&mut self, scenario: &str) {
        self.skipped.push(scenario.to_string());
    }

    /// Prints the whole run's verdict and returns the process exit code.
    #[must_use]
    pub fn finish(&self) -> i32 {
        println!("\n{}", "=".repeat(78));
        println!("GRADED AGAINST stress/baseline.json");
        if !self.baseline.taken_on.is_empty() {
            println!("baseline taken on: {}", self.baseline.taken_on);
        }
        println!("{}", "=".repeat(78));
        for verdict in &self.verdicts {
            verdict.report();
        }
        if !self.ungraded.is_empty() {
            println!("\nobserved but NOT graded (no baseline entry -- nothing can regress here):");
            for (metric, value, unit) in &self.ungraded {
                println!("  {metric:<44} {value:>10.3} {unit}");
            }
        }
        if !self.skipped.is_empty() {
            println!("\nSKIPPED scenarios (data unreachable -- no number was invented):");
            for scenario in &self.skipped {
                println!("  {scenario}");
            }
        }
        exit_code(&self.verdicts)
    }
}

/// The suite's own exit discipline: a hard failure is a non-zero exit, an
/// advisory one is not.
#[must_use]
pub fn exit_code(verdicts: &[Verdict]) -> i32 {
    let hard = verdicts.iter().filter(|v| !v.pass && !v.advisory).count();
    let advisory = verdicts.iter().filter(|v| !v.pass && v.advisory).count();
    println!(
        "\n{} metric(s) graded: {} pass, {} FAIL, {} slow-but-advisory",
        verdicts.len(),
        verdicts.iter().filter(|v| v.pass).count(),
        hard,
        advisory,
    );
    if hard > 0 {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

#[must_use]
pub fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[must_use]
pub fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[must_use]
pub fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[must_use]
pub fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key).map_or(default, |v| v == "1" || v == "true")
}

/// A banner, so a scenario's output can be found in a long run.
pub fn banner(scenario: &str, what: &str) {
    println!("\n{}", "=".repeat(78));
    println!("{scenario}");
    println!("{what}");
    println!("{}", "=".repeat(78));
}

/// The tile every Mercator cover calculation needs, the same way
/// `examples/label_parity.rs` computes it: one integer zoom bucket, every
/// tile the viewport's ground footprint touches.
#[must_use]
pub fn cover(camera: &gpui_maps::render::Camera) -> Vec<gpui_maps::geo::TileId> {
    let z = camera.zoom.floor() as u8;
    let scale = 2f64.powi(i32::from(z));
    let (cx, cy) =
        gpui_maps::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, f64::from(z));
    let half_x = f64::from(camera.viewport_width_px) / 2.0 / gpui_maps::geo::TILE_SIZE;
    let half_y = f64::from(camera.viewport_height_px) / 2.0 / gpui_maps::geo::TILE_SIZE;
    let max = scale as i64 - 1;
    let clamp = |v: f64| (v.floor() as i64).clamp(0, max) as u32;
    let (x0, x1) = (clamp(cx - half_x), clamp(cx + half_x));
    let (y0, y1) = (clamp(cy - half_y), clamp(cy + half_y));
    let mut out = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            out.push(gpui_maps::geo::TileId { z, x, y });
        }
    }
    out
}

/// A camera at a place and zoom, with the suite's standard viewport.
#[must_use]
pub fn camera_at(lon: f64, lat: f64, zoom: f64, width: f32, height: f32) -> gpui_maps::render::Camera {
    gpui_maps::render::Camera {
        center_lon: lon,
        center_lat: lat,
        zoom,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: width,
        viewport_height_px: height,
        fov_y: gpui_maps::render::Camera::DEFAULT_FOV_Y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // The percentile definition. Tested because a p99 that interpolates
    // would report a number no frame ever took, which is the opposite of
    // what this suite is for.
    // ---------------------------------------------------------------------

    #[test]
    fn a_percentile_is_always_a_sample_that_was_actually_taken() {
        let mut s = Samples::new();
        for v in [1.0, 2.0, 3.0, 100.0] {
            s.push_ms(v);
        }
        assert!(s.raw().contains(&s.p50()));
        assert!(s.raw().contains(&s.p99()));
        assert_eq!(s.worst(), 100.0);
    }

    #[test]
    fn the_p99_is_the_worst_sample_when_there_are_fewer_than_a_hundred() {
        let mut s = Samples::new();
        for v in [1.0, 1.0, 1.0, 40.0] {
            s.push_ms(v);
        }
        // With n=4 the 99th percentile IS the worst frame -- there is no
        // honest way to report a value between the top two, and a suite
        // that interpolated one would understate every short window.
        assert_eq!(s.p99(), 40.0);
        assert_eq!(s.p50(), 1.0);
    }

    #[test]
    fn an_empty_sample_set_reports_zero_rather_than_panicking() {
        let s = Samples::new();
        assert_eq!(s.p50(), 0.0);
        assert_eq!(s.p99(), 0.0);
        assert_eq!(s.worst(), 0.0);
        assert_eq!(s.over_budget(), 0);
    }

    // ---------------------------------------------------------------------
    // The frame budget. A literal here is a bug.
    // ---------------------------------------------------------------------

    #[test]
    fn the_budget_is_a_hundred_and_twenty_hertz_and_not_sixty() {
        assert!((FRAME_BUDGET_MS - 8.3333).abs() < 1e-3, "{FRAME_BUDGET_MS}");
        const { assert!(FRAME_BUDGET_MS < FRAME_BUDGET_60HZ_MS / 1.9) };
    }

    #[test]
    fn a_nine_millisecond_stage_is_over_budget() {
        let mut s = Samples::new();
        s.push_ms(9.0);
        // The whole reason the constant is 8.333: on a 60 Hz budget this
        // sample passes, and it does not pass here.
        assert_eq!(s.over_budget(), 1);
        const { assert!(9.0 < FRAME_BUDGET_60HZ_MS) };
    }

    // ---------------------------------------------------------------------
    // Payload constancy.
    // ---------------------------------------------------------------------

    #[test]
    fn a_changed_payload_is_a_different_fingerprint() {
        let a = Payload::of(b"abcdef", 2);
        let b = Payload::of(b"abcdeg", 2);
        assert_ne!(a, b);
    }

    #[test]
    fn the_item_count_is_part_of_the_fingerprint() {
        // Same bytes, different claim about what they are. A scenario that
        // reported 3,000 vehicles while pushing 300 would be a silent
        // fourfold speedup, so the count is inside the comparison.
        assert_ne!(Payload::of(b"same", 3000), Payload::of(b"same", 300));
    }

    #[test]
    fn coordinate_order_is_part_of_the_fingerprint() {
        let a = vec![vec![[1.0, 2.0], [3.0, 4.0]]];
        let b = vec![vec![[3.0, 4.0], [1.0, 2.0]]];
        assert_ne!(Payload::of_coords(&a), Payload::of_coords(&b));
    }

    #[test]
    #[should_panic(expected = "the payload changed mid-run")]
    fn an_unchanged_assertion_fails_loudly() {
        let pinned = Payload::of(b"first", 1);
        pinned.assert_unchanged(Payload::of(b"second", 1), "test");
    }

    // ---------------------------------------------------------------------
    // Grading.
    // ---------------------------------------------------------------------

    fn bound(baseline: f64, tolerance: f64) -> Bound {
        Bound {
            baseline,
            tolerance,
            unit: "ms".to_string(),
            rationale: "test".to_string(),
            floor: None,
            slack: 0.0,
        }
    }

    #[test]
    fn a_floor_fails_a_count_that_fell_and_slack_admits_a_few_from_zero() {
        let exact = Bound { floor: Some(1.0), ..bound(100.0, 1.0) };
        assert!(grade("v", 100.0, exact.clone(), true).pass);
        assert!(!grade("v", 99.0, exact.clone(), true).pass, "fewer vertices is a change too");
        assert!(!grade("v", 101.0, exact, true).pass);
        let spikes = Bound { slack: 3.0, ..bound(0.0, 1.0) };
        assert!(grade("s", 3.0, spikes.clone(), true).pass);
        assert!(!grade("s", 4.0, spikes, true).pass);
        let old: Bound = serde_json::from_str(r#"{"baseline":1,"tolerance":2,"unit":"ms","rationale":"x"}"#).unwrap();
        assert_eq!((old.floor, old.slack), (None, 0.0), "stress/baseline.json predates both fields");
    }

    #[test]
    fn a_missing_baseline_file_grades_nothing_rather_than_reporting_no_regressions() {
        let baseline = Baseline::load("/nonexistent/stress-baseline.json");
        assert!(baseline.metrics.is_empty());
        let mut grader = Grader::new(baseline);
        grader.observe("some.metric", 999.0, "ms", true);
        // The number is reported, but as UNGRADED. A suite that silently
        // passed here would tell a new machine that nothing regressed when
        // it had compared nothing at all.
        assert!(grader.verdicts.is_empty());
        assert_eq!(grader.ungraded.len(), 1);
        assert_eq!(grader.finish(), 0);
    }

    #[test]
    fn a_metric_with_a_bound_is_graded_and_one_without_is_not() {
        let mut metrics = BTreeMap::new();
        metrics.insert("graded".to_string(), bound(1.0, 2.0));
        let baseline = Baseline { metrics, ..Baseline::default() };
        let mut grader = Grader::new(baseline);
        grader.observe("graded", 5.0, "ms", true);
        grader.observe("ungraded", 5.0, "ms", true);
        assert_eq!(grader.verdicts.len(), 1);
        assert_eq!(grader.ungraded.len(), 1);
        assert_eq!(grader.finish(), 1);
    }

    #[test]
    fn the_committed_baseline_parses_and_every_bound_carries_a_rationale() {
        // The file the suite ships with. A tolerance with no stated reason
        // is how a 6x band becomes permanent, so the presence of the
        // rationale is a test and not a convention.
        let path = concat!(concat!(env!("CARGO_MANIFEST_DIR"), "/.."), "/stress/baseline.json");
        let baseline = Baseline::load(path);
        assert!(!baseline.metrics.is_empty(), "stress/baseline.json has no metrics");
        assert!(!baseline.taken_on.is_empty(), "a baseline must say where it was taken");
        for (name, bound) in &baseline.metrics {
            // `>= 0.0`, not `> 0.0`. A baseline of exactly zero is the
            // only way this grader can express "this must STAY at zero":
            // `pass = observed <= baseline * tolerance` makes a zero
            // baseline a ceiling of zero at any tolerance, so the metric
            // passes at 0 and fails at anything else.
            // `memory_ceiling.tour_recent_tiles_evicted` is the case --
            // how many tiles of the current cover a cache's byte budget
            // gave up, which is a guard and not a reading.
            //
            // Safe despite the obvious hazard (a metric accidentally
            // committed at zero would have an unreachable ceiling),
            // because such a metric then fails on EVERY run rather than
            // passing silently -- which is the failure mode this suite
            // was built to prefer. A negative baseline has no such
            // reading and is still rejected.
            assert!(bound.baseline >= 0.0, "{name}: baseline must not be negative");
            assert!(bound.tolerance >= 1.0, "{name}: a tolerance below 1.0 fails every run");
            assert!(!bound.unit.is_empty(), "{name}: no unit");
            assert!(
                bound.rationale.len() > 20,
                "{name}: a tolerance of x{} needs a stated reason, not '{}'",
                bound.tolerance,
                bound.rationale
            );
        }
    }

    #[test]
    fn a_value_inside_the_band_passes() {
        let v = grade("m", 1.9, bound(1.0, 2.0), true);
        assert!(v.pass);
        assert!(!v.advisory);
    }

    #[test]
    fn a_value_outside_the_band_on_a_quiet_machine_is_a_hard_failure() {
        let v = grade("m", 2.1, bound(1.0, 2.0), true);
        assert!(!v.pass);
        assert!(!v.advisory);
        assert_eq!(exit_code(&[v]), 1);
    }

    #[test]
    fn the_same_value_on_a_busy_machine_is_advisory_and_does_not_fail_the_run() {
        // A peer measured 6.7x run-to-run variance from contention alone.
        // A suite that turned that into a red build would be ignored within
        // a week, so contention downgrades a failure rather than clearing
        // it.
        let v = grade("m", 2.1, bound(1.0, 2.0), false);
        assert!(!v.pass);
        assert!(v.advisory);
        assert_eq!(exit_code(&[v]), 0);
    }

    #[test]
    fn a_run_with_one_hard_failure_among_passes_still_exits_nonzero() {
        let verdicts = vec![
            grade("a", 0.5, bound(1.0, 2.0), true),
            grade("b", 5.0, bound(1.0, 2.0), true),
            grade("c", 0.5, bound(1.0, 2.0), true),
        ];
        assert_eq!(exit_code(&verdicts), 1);
    }

    // ---------------------------------------------------------------------
    // The environment readers. These shell out, so the tests assert the
    // PARSE rather than the value -- the value is whatever the machine is
    // doing.
    // ---------------------------------------------------------------------

    #[test]
    fn vm_stat_parses_into_the_counters_a_rate_needs() {
        let vm = read_vm_stat();
        // Skipped rather than failed on a platform without vm_stat: the
        // suite is macOS-calibrated, and a green CI elsewhere must not
        // depend on a mach utility.
        if vm.is_empty() {
            return;
        }
        for key in ["Pageins", "Pageouts", "Swapins", "Swapouts"] {
            assert!(vm.contains_key(key), "vm_stat lost {key}: {vm:?}");
        }
        assert!(vm.get("__page_size").copied().unwrap_or(0) >= 4096);
    }

    #[test]
    fn a_paging_rate_is_a_rate_and_not_a_level() {
        let window = EnvWindow::open();
        let env = window.close();
        if read_vm_stat().is_empty() {
            return;
        }
        // The artifact this guards: `Pageins` on a long-lived session is in
        // the tens of millions, and a level read as a rate looks like
        // catastrophe. Over a window of microseconds the RATE can be large
        // or zero, but the counter behind it must not have leaked through
        // as an absolute.
        assert!(env.window_secs > 0.0);
        assert!(env.pageins_per_s.is_finite());
        assert!(env.pageouts_per_s.is_finite());
    }

    #[test]
    fn the_top_processes_are_read_with_their_sizes() {
        let top = read_top_rss(3);
        if top.is_empty() {
            return;
        }
        assert!(top.len() <= 3);
        // Descending, so the line reads biggest-first.
        for pair in top.windows(2) {
            assert!(pair[0].1 >= pair[1].1);
        }
        assert!(top[0].1 > 0);
    }

    #[test]
    fn quiet_is_a_statement_about_contention_and_not_about_speed() {
        let busy = Env { load_1m: 12.0, cpus: 14, ..Env::default() };
        assert!(!busy.quiet());
        let paging = Env { load_1m: 0.1, cpus: 14, pageouts_per_s: 5000.0, ..Env::default() };
        assert!(!paging.quiet());
        // The macOS case: the compressor is working hard and swap has not
        // moved at all. A definition of "busy" that only watched swap would
        // call this machine quiet.
        let compressing =
            Env { load_1m: 0.1, cpus: 14, compressions_per_s: 40_000.0, ..Env::default() };
        assert!(!compressing.quiet());
        let calm = Env { load_1m: 0.5, cpus: 14, ..Env::default() };
        assert!(calm.quiet());
    }

    #[test]
    fn a_quiet_machine_is_still_quiet_under_background_housekeeping() {
        // The other half of the contract: a window marked not-quiet
        // downgrades every failure inside it to advisory, so a definition
        // that tripped on ordinary background activity would turn the suite
        // into a report nobody can fail. These rates were all observed on
        // an idle run of this suite.
        let ordinary = Env {
            load_1m: 5.5,
            cpus: 14,
            pageins_per_s: 13_588.0,
            swapins_per_s: 38.0,
            compressions_per_s: 100.0,
            ..Env::default()
        };
        assert!(ordinary.quiet(), "{}", ordinary.line());
    }

    // ---------------------------------------------------------------------
    // The cover. Wrong here means every scenario measures the wrong tiles.
    // ---------------------------------------------------------------------

    #[test]
    fn a_1024x768_viewport_at_z14_covers_a_handful_of_tiles() {
        let camera = camera_at(139.7671, 35.6812, 14.0, 1024.0, 768.0);
        let tiles = cover(&camera);
        // 1024x768 CSS px over 512-px tiles is 2x1.5 tiles, so 3x3 at
        // worst once the centre lands mid-tile.
        assert!((4..=12).contains(&tiles.len()), "{} tiles", tiles.len());
        assert!(tiles.iter().all(|t| t.z == 14));
    }

    #[test]
    fn the_cover_grows_with_the_viewport_and_not_with_the_zoom_fraction() {
        let small = cover(&camera_at(139.7671, 35.6812, 14.0, 512.0, 512.0));
        let large = cover(&camera_at(139.7671, 35.6812, 14.0, 2048.0, 2048.0));
        assert!(large.len() > small.len());
        // A fractional zoom stays in the same integer bucket, so the cover
        // must not move. A scenario sweeping zoom relies on this: if the
        // cover changed at 14.5 the "per crossing" figure would be counting
        // two different tile sets.
        let whole = cover(&camera_at(139.7671, 35.6812, 14.0, 1024.0, 768.0));
        let fractional = cover(&camera_at(139.7671, 35.6812, 14.9, 1024.0, 768.0));
        assert_eq!(whole, fractional);
    }

    #[test]
    fn the_cover_is_clamped_at_the_antimeridian_rather_than_wrapping_negative() {
        let tiles = cover(&camera_at(-180.0, 0.0, 3.0, 1024.0, 768.0));
        assert!(tiles.iter().all(|t| t.x < 8 && t.y < 8));
    }
}
