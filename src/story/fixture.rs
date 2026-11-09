//! Offline tile sources for the story tests: [`FixtureSource`] serves the
//! vendored Singapore z14 tile for every id, and [`FakeSource`] scripts
//! latency, failures, absences and tiles that never arrive, logging every
//! request. No network, and the same answer every run.
//!
//! Doc-hidden: public only so integration tests and examples can use it.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::geo::TileId;
use crate::tile_source::TileSource;

/// The vendored Singapore z14 vector tile.
pub const FIXTURE_TILE: &[u8] = include_bytes!("../../benches/fixtures/singapore_z14.pbf");

/// Serves the Singapore fixture for every tile id. Offline, deterministic.
pub struct FixtureSource;

#[async_trait::async_trait]
impl TileSource for FixtureSource {
    async fn fetch(&self, _tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(Some(FIXTURE_TILE.to_vec()))
    }

    /// The vendored tile is OpenMapTiles-schema OpenStreetMap data, and
    /// says so like any real source: a fixture story burns in a credit.
    fn attribution(&self) -> Option<&str> {
        Some(FIXTURE_CREDIT)
    }
}

/// [`FixtureSource`]'s credit.
pub const FIXTURE_CREDIT: &str = "© OpenMapTiles © OpenStreetMap contributors";

/// Scripted latency, failures, absences and a request log, for the wait tests.
///
/// Every request is logged when it starts. A tile in `never` does not
/// answer at all; one in `fail` is an `Err` and one in `absent` is
/// `Ok(None)`, both at once; one in `panic` panics inside the fetch (for
/// the slot-release test); one in `held` waits for a permit on its
/// semaphore, which the test adds when it wants the tile to land -- a late
/// arrival with no clock in it. Every other tile is [`FIXTURE_TILE`] after
/// its `slow` delay, or `latency`. Waits are `tokio::time::sleep`, never
/// a thread sleep, so a waiting fetch does not hold a worker.
pub struct FakeSource {
    pub latency: Duration,
    pub never: HashSet<TileId>,
    pub fail: HashSet<TileId>,
    pub absent: HashSet<TileId>,
    pub slow: HashMap<TileId, Duration>,
    pub panic: HashSet<TileId>,
    pub held: HashMap<TileId, Arc<tokio::sync::Semaphore>>,
    pub log: Mutex<Vec<TileId>>,
}

impl FakeSource {
    /// Every tile is the fixture after `latency`; nothing scripted.
    #[must_use]
    pub fn fixture(latency: Duration) -> Self {
        Self {
            latency,
            never: HashSet::new(),
            fail: HashSet::new(),
            absent: HashSet::new(),
            slow: HashMap::new(),
            panic: HashSet::new(),
            held: HashMap::new(),
            log: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl TileSource for FakeSource {
    /// The fixture tile's credit: every tile it serves is [`FIXTURE_TILE`].
    fn attribution(&self) -> Option<&str> {
        Some(FIXTURE_CREDIT)
    }

    async fn fetch(&self, tile: TileId) -> anyhow::Result<Option<Vec<u8>>> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).push(tile);
        if self.never.contains(&tile) {
            std::future::pending::<()>().await;
        }
        #[allow(clippy::panic)]
        if self.panic.contains(&tile) {
            panic!("scripted panic for {}/{}/{}", tile.z, tile.x, tile.y);
        }
        if self.fail.contains(&tile) {
            anyhow::bail!("scripted failure for {}/{}/{}", tile.z, tile.x, tile.y);
        }
        if self.absent.contains(&tile) {
            return Ok(None);
        }
        if let Some(gate) = self.held.get(&tile) {
            // A closed semaphore releases too: the test is over either way.
            let _permit = gate.acquire().await;
        }
        let wait = self.slow.get(&tile).copied().unwrap_or(self.latency);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(Some(FIXTURE_TILE.to_vec()))
    }
}
