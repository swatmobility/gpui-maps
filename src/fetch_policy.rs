//! What a shell does with a tile request that did not come back with data:
//! remember it as absent for good, or try again later -- and when.
//!
//! # Why this exists
//!
//! Both phone and browser shells used to put EVERY failed tile into the same
//! `missing` set a 404 goes into, and never asked for a `missing` tile again.
//! One timeout, one connection reset, one TLS hiccup on weak cellular, or one
//! 429/5xx from the tile server blanked that tile for the rest of the session,
//! with no coarse stand-in either once its ancestors had gone the same way.
//! On a phone on a bad connection that is whole columns of the map, a whole
//! street-zoom screen, or the entire globe.
//!
//! # The rule, and where it comes from
//!
//! MapLibre GL JS separates the two answers the same way. In
//! `src/tile/tile_manager.ts` `_loadTile`, a failed load sets
//! `tile.state = 'errored'` and, for anything but a 404, fires an error; in
//! `src/source/vector_tile_source.ts` `loadTile`, a 404 is swallowed and the
//! tile is finished as loaded-and-empty. An errored tile has no data, so
//! `_removeTile` does not keep it in the out-of-view cache, and the next time
//! the camera wants that tile `_addTile` builds a fresh `Tile` and loads it
//! again. An error is a statement about a moment; a 404 is a statement about
//! the data. (Native `MapView::should_request_tile` already made the same
//! split, with a flat `FAILED_BACKOFF`.)
//!
//! This module keeps that split and adds a capped exponential backoff on
//! top, so a still camera over a failing tile retries on its own -- MapLibre
//! only retries on re-entry -- without hammering a server that is already
//! answering 429:
//!
//! - [`classify_status`] says which HTTP answers are ABSENT (404, 410, 204)
//!   and which are worth retrying (everything else: 408, 429, 5xx, and the
//!   rest, which cost at most one request per [`MAX_BACKOFF_MS`]).
//! - [`RetryTable`] holds the retry state per tile: how many consecutive
//!   failures, and when the next request is allowed. A success, or an
//!   absent answer, clears it.
//!
//! # Time is a parameter
//!
//! Every method takes `now_ms`, a monotonic millisecond clock the shell
//! owns (`performance.now()` in the browser, `Instant`-since-launch on the
//! phone). Nothing here reads a clock, because the browser build compiles
//! this file and `std::time::Instant::now` panics there -- see
//! `tests/wasm_portability.rs`. The jitter is a hash of the tile and the
//! attempt number rather than a random draw, for the same reason and so the
//! tests can pin it.
//!
//! # The redraw contract
//!
//! A tile waiting out its backoff must not keep a frame loop spinning.
//! [`RetryTable::next_wake_ms`] is the one deadline a `ControlFlow::Wait`
//! loop needs to sleep until, and [`RetryTable::park_overdue`] is what makes
//! that deadline strictly in the future after every request pass: a tile
//! whose backoff has expired but that the camera no longer wants is parked,
//! not re-woken for. It keeps its failure count, so if the camera comes back
//! the tile is asked for at once (as MapLibre would) and a further failure
//! still backs off from where it was.

use std::collections::HashMap;

use crate::geo::TileId;

/// The first retry's delay, before jitter.
pub const BASE_BACKOFF_MS: f64 = 1_000.0;
/// No retry waits longer than this, jitter included -- unless the server
/// itself asked for longer with `Retry-After`.
pub const MAX_BACKOFF_MS: f64 = 30_000.0;
/// A `Retry-After` longer than this is clamped to it. A server asking for
/// an hour should not blank a map for an hour.
pub const MAX_RETRY_AFTER_MS: f64 = 120_000.0;
/// A parked entry (backoff expired, camera no longer asking) older than
/// this is forgotten, which is what keeps the table bounded over a long
/// session of panning over a flaky connection. Forgetting one costs at most
/// that tile's failure count, which is the same fresh start MapLibre gives
/// every errored tile on re-entry.
pub const FORGET_PARKED_AFTER_MS: f64 = 5.0 * 60_000.0;

/// What an HTTP status that is not a data-bearing success means for a tile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StatusVerdict {
    /// The server says there is no data here. Remember it for the session;
    /// asking again cannot change the answer.
    Absent,
    /// Anything else. Retry after a backoff, or after the server's own
    /// `Retry-After` in milliseconds if it sent a usable one.
    Retry { retry_after_ms: Option<f64> },
}

/// Classifies a response status. `retry_after` is the raw `Retry-After`
/// header value, if the shell could read one (a browser only exposes it
/// cross-origin when the server lists it in `Access-Control-Expose-Headers`).
///
/// 404 and 410 are absence -- MapLibre's own rule for 404, and 410 is the
/// same statement made permanently. 204 is a server saying "no content" for
/// a tile, which is what some tile servers answer instead of 404. Every other
/// status is retried: 408, 429 and 5xx are transient by definition, and a
/// 401/403 (a token problem) costs at most one request per tile per
/// [`MAX_BACKOFF_MS`] -- the same price MapLibre pays by re-requesting an
/// errored tile each time it comes back into view.
pub fn classify_status(status: u16, retry_after: Option<&str>) -> StatusVerdict {
    match status {
        204 | 404 | 410 => StatusVerdict::Absent,
        _ => StatusVerdict::Retry { retry_after_ms: retry_after.and_then(parse_retry_after_ms) },
    }
}

/// `Retry-After` in its delta-seconds form, in milliseconds, clamped to
/// [`MAX_RETRY_AFTER_MS`]. The HTTP-date form is not parsed -- that would
/// need a wall clock this module does not read -- and falls back to the
/// ordinary backoff.
pub fn parse_retry_after_ms(value: &str) -> Option<f64> {
    let seconds: u64 = value.trim().parse().ok()?;
    Some((seconds as f64 * 1000.0).min(MAX_RETRY_AFTER_MS))
}

/// The backoff before the `attempt`-th retry (1-based: the delay after the
/// first failure is `attempt == 1`), jittered by `seed` into
/// `[0.8, 1.2)` of the nominal delay and never above [`MAX_BACKOFF_MS`].
/// Nominal delays run 1 s, 2 s, 4 s, 8 s, 16 s, 30 s, 30 s, ...
pub fn backoff_ms(attempt: u32, seed: u64) -> f64 {
    let doublings = attempt.saturating_sub(1).min(16);
    let nominal = (BASE_BACKOFF_MS * f64::from(1u32 << doublings)).min(MAX_BACKOFF_MS);
    // The top 53 bits of a mixed seed, as a uniform fraction in [0, 1).
    let unit = (splitmix64(seed) >> 11) as f64 / (1u64 << 53) as f64;
    (nominal * (0.8 + 0.4 * unit)).min(MAX_BACKOFF_MS)
}

/// SplitMix64's finaliser: a cheap, well-mixed hash, so neighbouring tiles
/// that all failed in the same burst spread their retries out.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn jitter_seed(tile: TileId, attempt: u32) -> u64 {
    (u64::from(tile.z) << 56) ^ (u64::from(tile.x) << 28) ^ u64::from(tile.y) ^ (u64::from(attempt) << 60)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Entry {
    /// Consecutive failures so far.
    failures: u32,
    /// The earliest `now_ms` a new request is allowed at.
    due_ms: f64,
    /// Not waiting for a wake: either a request is in flight, or the backoff
    /// expired while the camera was not asking for the tile.
    parked: bool,
}

/// Per-tile retry state for one tile grid (a shell keeps one for vector
/// tiles and one for imagery). See the module doc.
#[derive(Debug, Default)]
pub struct RetryTable {
    entries: HashMap<TileId, Entry>,
    /// Every failure recorded since the table was made, for the status line.
    failures_total: u64,
}

impl RetryTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the shell may send a request for `tile` now, as far as retry
    /// state goes. The shell still checks its own decoded/in-flight/absent
    /// sets; this only answers "is it still backing off".
    pub fn may_request(&self, tile: TileId, now_ms: f64) -> bool {
        self.entries.get(&tile).is_none_or(|e| now_ms >= e.due_ms)
    }

    /// The shell sent a request for `tile`. A tile with retry state stops
    /// counting towards [`Self::next_wake_ms`] until it answers.
    pub fn on_requested(&mut self, tile: TileId) {
        if let Some(e) = self.entries.get_mut(&tile) {
            e.parked = true;
        }
    }

    /// The request for `tile` failed transiently (network error, timeout,
    /// truncated body, or a status [`classify_status`] said to retry).
    /// Returns the delay, in milliseconds, before it may be asked for again:
    /// the jittered exponential backoff, or the server's `Retry-After` if
    /// that is longer.
    pub fn on_failure(&mut self, tile: TileId, now_ms: f64, retry_after_ms: Option<f64>) -> f64 {
        self.failures_total += 1;
        let entry = self.entries.entry(tile).or_insert(Entry { failures: 0, due_ms: now_ms, parked: false });
        entry.failures = entry.failures.saturating_add(1);
        let backoff = backoff_ms(entry.failures, jitter_seed(tile, entry.failures));
        let delay = retry_after_ms.map_or(backoff, |ra| ra.clamp(0.0, MAX_RETRY_AFTER_MS).max(backoff));
        entry.due_ms = now_ms + delay;
        entry.parked = false;
        delay
    }

    /// The request for `tile` came back with data, or with a definitive
    /// absent answer. Either way there is nothing left to retry.
    pub fn on_settled(&mut self, tile: TileId) {
        self.entries.remove(&tile);
    }

    /// Call once per frame AFTER the request pass. Any tile whose backoff
    /// has expired and that the pass did not ask for is one the camera no
    /// longer wants: park it, so [`Self::next_wake_ms`] does not keep waking
    /// the loop for it. Also forgets parked entries older than
    /// [`FORGET_PARKED_AFTER_MS`].
    pub fn park_overdue(&mut self, now_ms: f64) {
        self.entries.retain(|_, e| {
            if !e.parked && now_ms >= e.due_ms {
                e.parked = true;
            }
            // In flight entries have `due_ms` in the past too, but their
            // answer is coming and will either clear or re-arm them; only
            // forget ones that have been sitting for a long time.
            !(e.parked && now_ms - e.due_ms > FORGET_PARKED_AFTER_MS)
        });
    }

    /// The earliest `now_ms` at which a tile's backoff expires, if any tile
    /// is backing off. A `ControlFlow::Wait` loop sleeps until this and no
    /// longer; `None` means no retry needs a wake at all.
    pub fn next_wake_ms(&self) -> Option<f64> {
        self.entries.values().filter(|e| !e.parked).map(|e| e.due_ms).reduce(f64::min)
    }

    /// Tiles currently waiting out a backoff.
    pub fn retrying(&self) -> usize {
        self.entries.values().filter(|e| !e.parked).count()
    }

    /// Every transient failure recorded since the table was made.
    pub fn failures_total(&self) -> u64 {
        self.failures_total
    }

    /// Tiles with any retry state (backing off, in flight after a failure,
    /// or parked).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forgets everything, as a style or source swap does for the shell's
    /// own tile sets.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(z: u8, x: u32, y: u32) -> TileId {
        TileId { z, x, y }
    }

    #[test]
    fn a_404_is_absent_for_good() {
        assert_eq!(classify_status(404, None), StatusVerdict::Absent);
        assert_eq!(classify_status(410, None), StatusVerdict::Absent);
        assert_eq!(classify_status(204, None), StatusVerdict::Absent);
    }

    #[test]
    fn timeouts_rate_limits_and_server_errors_are_retried() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify_status(status, None),
                StatusVerdict::Retry { retry_after_ms: None },
                "HTTP {status} is transient"
            );
        }
    }

    #[test]
    fn retry_after_seconds_are_parsed_and_clamped() {
        assert_eq!(classify_status(429, Some(" 7 ")), StatusVerdict::Retry { retry_after_ms: Some(7_000.0) });
        assert_eq!(parse_retry_after_ms("86400"), Some(MAX_RETRY_AFTER_MS));
        // HTTP-date form: not parsed, falls back to the backoff.
        assert_eq!(parse_retry_after_ms("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn backoff_grows_then_stops_at_the_cap() {
        let tile = t(7, 90, 40);
        let mut table = RetryTable::new();
        let mut now = 0.0;
        let mut last = 0.0;
        let mut delays = Vec::new();
        for _ in 0..10 {
            let delay = table.on_failure(tile, now, None);
            delays.push(delay);
            assert!(delay <= MAX_BACKOFF_MS, "{delay} exceeds the cap");
            now += delay;
        }
        // First retry is about a second.
        assert!((800.0..1200.0).contains(&delays[0]), "{delays:?}");
        // Strictly growing until the cap is reached (jitter is +-20%, each
        // step doubles, so jitter cannot reverse a step below the cap).
        for d in &delays[..5] {
            assert!(*d > last, "{delays:?}");
            last = *d;
        }
        // At the cap it stays in the jittered band just below it.
        for d in &delays[5..] {
            assert!(*d >= 0.8 * MAX_BACKOFF_MS, "{delays:?}");
        }
    }

    #[test]
    fn a_backing_off_tile_is_not_requested_until_due() {
        let tile = t(3, 1, 2);
        let mut table = RetryTable::new();
        assert!(table.may_request(tile, 0.0), "no state yet");
        let delay = table.on_failure(tile, 1_000.0, None);
        assert!(!table.may_request(tile, 1_000.0));
        assert!(!table.may_request(tile, 1_000.0 + delay - 1.0));
        assert!(table.may_request(tile, 1_000.0 + delay));
        assert_eq!(table.retrying(), 1);
        assert_eq!(table.failures_total(), 1);
    }

    #[test]
    fn retry_after_is_honoured_when_longer_than_the_backoff() {
        let tile = t(5, 5, 5);
        let mut table = RetryTable::new();
        let delay = table.on_failure(tile, 0.0, Some(10_000.0));
        assert_eq!(delay, 10_000.0);
        assert!(!table.may_request(tile, 9_999.0));
        assert!(table.may_request(tile, 10_000.0));
        // A shorter Retry-After never undercuts the backoff.
        let other = t(5, 6, 5);
        let delay = table.on_failure(other, 0.0, Some(0.0));
        assert!(delay >= 0.8 * BASE_BACKOFF_MS);
    }

    #[test]
    fn a_success_clears_the_state() {
        let tile = t(9, 9, 9);
        let mut table = RetryTable::new();
        let d1 = table.on_failure(tile, 0.0, None);
        table.on_requested(tile);
        table.on_settled(tile);
        assert!(table.is_empty());
        assert!(table.may_request(tile, 0.0));
        // And the next failure starts the backoff from the bottom again.
        let d2 = table.on_failure(tile, 0.0, None);
        assert_eq!(d1, d2);
        assert!(d2 < 1_200.0);
    }

    #[test]
    fn the_wake_is_the_earliest_pending_retry_and_ignores_in_flight() {
        let a = t(4, 1, 1);
        let b = t(4, 2, 1);
        let mut table = RetryTable::new();
        assert_eq!(table.next_wake_ms(), None, "nothing to wake for");
        let da = table.on_failure(a, 0.0, None);
        let db = table.on_failure(b, 0.0, Some(20_000.0));
        assert_eq!(table.next_wake_ms(), Some(da.min(db)));
        // `a` came due and was re-requested: only `b` is waited on now.
        table.on_requested(a);
        assert_eq!(table.next_wake_ms(), Some(db));
    }

    #[test]
    fn a_tile_no_longer_wanted_is_not_retried_and_does_not_keep_waking() {
        let tile = t(6, 3, 3);
        let mut table = RetryTable::new();
        let delay = table.on_failure(tile, 0.0, None);
        // The loop wakes at `delay`, the request pass does NOT ask for the
        // tile (the camera moved on), and `park_overdue` runs.
        table.park_overdue(delay);
        assert_eq!(table.next_wake_ms(), None, "an unwanted tile must not keep the loop awake");
        assert_eq!(table.retrying(), 0);
        // If the camera comes back, it is asked for at once...
        assert!(table.may_request(tile, delay + 1.0));
        // ...and a further failure backs off from where it left off.
        table.on_requested(tile);
        let next = table.on_failure(tile, delay + 1.0, None);
        assert!(next > 1.5 * BASE_BACKOFF_MS, "{next}");
    }

    #[test]
    fn park_overdue_leaves_future_retries_and_in_flight_requests_alone() {
        let waiting = t(8, 1, 1);
        let flying = t(8, 2, 2);
        let mut table = RetryTable::new();
        let d = table.on_failure(waiting, 0.0, None);
        table.on_failure(flying, 0.0, None);
        table.on_requested(flying);
        table.park_overdue(d - 1.0);
        assert_eq!(table.next_wake_ms(), Some(d));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn long_parked_entries_are_forgotten() {
        let tile = t(2, 1, 1);
        let mut table = RetryTable::new();
        let d = table.on_failure(tile, 0.0, None);
        table.park_overdue(d);
        table.park_overdue(d + FORGET_PARKED_AFTER_MS + 1.0);
        assert!(table.is_empty());
    }
}
