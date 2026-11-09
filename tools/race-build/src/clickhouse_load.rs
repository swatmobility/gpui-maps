//! `load-clickhouse` (Task 9, spec §4.1, MG-R9): `parts.spill` streamed as
//! RowBinary into `gpui_map_dots.race_parts_2020`, then aggregated in SQL
//! into `race_hex_2020` at H3 r4..=r8 (Walker's hexagon resolutions).
//!
//! The rules are `tools/dotgrid-build`'s loader's:
//! - It owns `gpui_map_dots` and nothing else. A `gpui_map_dots` without the
//!   `_dotgrid_owner` marker was not made by these tools: refuse (exit 3).
//! - Disk: a sample of the spill is loaded first and measured; the full load
//!   refuses (exit 4) unless ClickHouse's disk has the estimate + 10 GB free.
//! - Everything is built in `*_load` tables and checked there. Only when
//!   every check passes do `EXCHANGE TABLES` put them live; on any failure
//!   before the swap the `_load` tables are dropped and the live tables are
//!   untouched.
//! - Both `_load` tables carry a build stamp in their comment (`building`,
//!   then `validated` once every check passes); stamps travel through
//!   EXCHANGE. `finish_exchange` makes whichever EXCHANGE is still owed
//!   (3 attempts each). If one keeps failing, the validated `_load` tables
//!   are KEPT and the load exits 6: `load-clickhouse --finish-exchange`
//!   finishes the swap without a reload. Every load reports at startup
//!   whether the live parts and hex tables carry the same build.
//! - An empty `gpui_map_dots` with no marker (a first load killed between
//!   `CREATE DATABASE` and the marker) is adopted, not refused.
//! - Checks (exit 5 on failure): the part count; the 8 group sums and the
//!   total, exact, against the P.L. US totals, both as streamed and in
//!   ClickHouse; the same at every resolution; the `h3ToGeo` round trip
//!   (0 parts over 1.5 km from their r8 cell's centre, 0 whose cell is
//!   missing); and 1,000 sampled parts' cells at r4..=r8 against `h3o`,
//!   bit for bit.
//!
//! `parts.spill` is read only through `spill::for_each_part`
//! (`SpillPart::decode`).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gpui_maps::sprite::redact_url_credentials;
use serde_json::{json, Value};

use crate::pl94::{US_GROUP_TOTALS, US_TOTAL};
use crate::spill::{for_each_part, SpillPart, PART_RECORD_BYTES};
use crate::sql;
use crate::tiger::unproject;

/// RowBinary rows per POST.
const CHUNK_ROWS: usize = 1_000_000;
/// `u8 state, u64 block_geoid, u32 part, f64 lat, f64 lon, 8 × u32`.
pub const ROW_BYTES: usize = 1 + 8 + 4 + 8 + 8 + 8 * 4;
/// Headroom every load keeps free (global constraints).
pub const DISK_FLOOR_MB: u64 = 10 * 1024;
/// Walker's hexagon resolutions.
pub const RESOLUTIONS: std::ops::RangeInclusive<u8> = 4..=8;
/// The round-trip gate's bound: the r8 edge bound, in metres.
pub const GATE_M: f64 = 1500.0;
/// Upper bound on `race_hex_2020`'s bytes before it exists: spec §4.1's
/// estimated cells over all five resolutions (14.5 M, the whole US land
/// area, not only the populated share) at 45 B, the row uncompressed.
const HEX_ESTIMATE_BYTES: u64 = 15_000_000 * 45;
/// The disk sample: this many runs of [`SAMPLE_RUN`] consecutive parts,
/// spread evenly over the spill (consecutive, so the sample compresses
/// the way the sorted table does).
const SAMPLE_RUNS: u64 = 20;
const SAMPLE_RUN: u64 = 50_000;

/// A refusal or a failed gate: `main` exits with `code`, not 1.
#[derive(Debug)]
pub struct Refusal {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Refusal {}

pub(crate) fn refuse(code: i32, msg: impl Into<String>) -> anyhow::Error {
    Refusal { code, msg: msg.into() }.into()
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text = format!("{text}: {c}");
        cause = c.source();
    }
    redact_url_credentials(&text)
}

/// A ClickHouse HTTP endpoint (`CLICKHOUSE_URL`, default
/// `http://localhost:8123`). Statements go as POST bodies, values as
/// `param_*` arguments, a RowBinary insert as `query=` with the rows as body.
pub struct Ch {
    url: String,
    http: reqwest::blocking::Client,
}

impl Ch {
    pub fn new(url: &str) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(3600))
            .build()?;
        Ok(Ch { url: url.trim_end_matches('/').to_string(), http })
    }

    pub fn from_env() -> Result<Self> {
        Ch::new(&std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into()))
    }

    fn send(&self, args: Vec<(String, String)>, body: Vec<u8>, what: &str) -> Result<String> {
        let bytes = self.send_bytes(args, body, what)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn send_bytes(&self, mut args: Vec<(String, String)>, body: Vec<u8>, what: &str) -> Result<Vec<u8>> {
        // A mid-query exception then arrives as an HTTP error, never as a
        // 200 whose body ends in `Code: …`.
        args.push(("wait_end_of_query".into(), "1".into()));
        let shown = redact_url_credentials(&self.url);
        let mut resp = self
            .http
            .post(&self.url)
            .query(&args)
            .body(body)
            .send()
            .map_err(|e| anyhow!("ClickHouse at {shown}: {what}: {}", error_chain(&e.without_url())))?;
        let mut bytes = Vec::new();
        resp.read_to_end(&mut bytes)
            .map_err(|e| anyhow!("ClickHouse at {shown}: {what}: reading the response: {}", redact_url_credentials(&e.to_string())))?;
        if !resp.status().is_success() || resp.headers().contains_key("x-clickhouse-exception-code") {
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
            bail!("ClickHouse {}: {}\n  statement: {what}", resp.status(), redact_url_credentials(text.trim()));
        }
        Ok(bytes)
    }

    /// Runs `sql` with `param_<name>` values and returns the raw body (a
    /// `RowBinary` result).
    pub fn run_bytes(&self, sql: &str, params: &[(&str, String)]) -> Result<Vec<u8>> {
        let args = params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())).collect();
        self.send_bytes(args, sql.as_bytes().to_vec(), sql)
    }

    /// Runs `sql` with `param_<name>` values.
    pub fn run(&self, sql: &str, params: &[(&str, String)]) -> Result<String> {
        let args = params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())).collect();
        self.send(args, sql.as_bytes().to_vec(), sql)
    }

    fn insert(&self, sql: &str, rows: Vec<u8>) -> Result<()> {
        self.send(vec![("query".into(), sql.into())], rows, sql).map(|_| ())
    }

    /// One TabSeparated row of unsigned integers.
    pub fn ints(&self, sql: &str, params: &[(&str, String)]) -> Result<Vec<u64>> {
        let text = self.run(sql, params)?;
        text.trim().split('\t').map(|c| c.parse().with_context(|| format!("not integers: {text:?} from {sql}"))).collect()
    }
}

/// Whose `gpui_map_dots` is it?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// No such database.
    Absent,
    /// It carries the `_dotgrid_owner` marker.
    Ours,
    /// It exists, holds no table at all and no marker: a first load that
    /// was killed between `CREATE DATABASE` and the marker. Adopted.
    EmptyUnmarked,
}

/// What [`check_owner`] decides from `(database exists, marker exists,
/// tables in it)`. Refuses (exit 3) a database with tables but no marker.
pub fn owner_from(exists: bool, marker: bool, tables: u64) -> Result<Owner> {
    match (exists, marker, tables) {
        (false, _, _) => Ok(Owner::Absent),
        (true, true, _) => Ok(Owner::Ours),
        (true, false, 0) => Ok(Owner::EmptyUnmarked),
        (true, false, n) => Err(refuse(
            3,
            format!("gpui_map_dots exists with {n} tables and no _dotgrid_owner marker: not created by these tools; not touching it"),
        )),
    }
}

/// Reads only `system.*`.
pub fn check_owner(ch: &Ch) -> Result<Owner> {
    let exists = ch.ints(sql::DATABASE_EXISTS, &[])?[0] != 0;
    let marker = exists && ch.ints(sql::OWNER_MARKER_EXISTS, &[])?[0] == 1;
    let tables = if exists && !marker { ch.ints(sql::TABLES_IN_DB, &[])?[0] } else { 0 };
    owner_from(exists, marker, tables)
}

pub fn df_free_mb(path: &Path) -> Result<u64> {
    let out = std::process::Command::new("df").arg("-m").arg(path).output().context("df")?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|a| a.parse().ok())
        .with_context(|| format!("df -m {}: {text}", path.display()))
}

/// Refuses (exit 4) when any ClickHouse disk has less than `need_mb` + 10 GB
/// free, by `system.disks` or by `df`.
fn check_disk(ch: &Ch, need_mb: u64) -> Result<Value> {
    let mut seen = Vec::new();
    let mut short = Vec::new();
    for line in ch.run(sql::DISKS, &[])?.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [name, path, free] = f[..] else { bail!("system.disks row {line:?}") };
        let free_mb = free.parse::<u64>().context("system.disks free_space")? >> 20;
        let df_mb = df_free_mb(Path::new(path))?;
        eprintln!("disk {name} {path}: system.disks {free_mb} MB free, df {df_mb} MB free; need {need_mb} + {DISK_FLOOR_MB} MB");
        seen.push(json!({"disk": name, "path": path, "system_disks_free_mb": free_mb, "df_free_mb": df_mb}));
        for mb in [free_mb, df_mb] {
            if mb < need_mb + DISK_FLOOR_MB {
                short.push(format!("{path}: {mb} MB free"));
            }
        }
    }
    if !short.is_empty() {
        return Err(refuse(4, format!("not enough disk: need {need_mb} MB + {DISK_FLOOR_MB} MB floor; {}", short.join("; "))));
    }
    Ok(json!({"need_mb": need_mb, "floor_mb": DISK_FLOOR_MB, "disks": seen}))
}

/// A part's row: its centroid (world-grid quanta, unprojected) as
/// `lat`/`lon`, and its counts widened to u32. Little-endian, in the
/// column order of [`sql::INSERT_PARTS_LOAD`].
pub fn encode_row(p: &SpillPart, out: &mut Vec<u8>) {
    let (lat, lon) = unproject(p.centroid());
    out.push(p.state);
    out.extend_from_slice(&p.geoid.to_le_bytes());
    out.extend_from_slice(&p.part.to_le_bytes());
    out.extend_from_slice(&lat.to_le_bytes());
    out.extend_from_slice(&lon.to_le_bytes());
    for n in p.c {
        out.extend_from_slice(&u32::from(n).to_le_bytes());
    }
}

/// What a stream sent: rows and the 8 group sums.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Streamed {
    pub rows: u64,
    pub groups: [u64; 8],
}

/// Streams `reader`'s parts into `race_parts_2020_load`, [`CHUNK_ROWS`]
/// per POST, encoding on this thread while another POSTs.
fn stream_parts(ch: &Ch, reader: impl Read) -> Result<Streamed> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(2);
    std::thread::scope(|s| {
        let poster = s.spawn(move || -> Result<()> {
            for body in rx {
                ch.insert(sql::INSERT_PARTS_LOAD, body)?;
            }
            Ok(())
        });
        let mut st = Streamed::default();
        let mut buf = Vec::with_capacity(CHUNK_ROWS * ROW_BYTES);
        let read = for_each_part(reader, |p| {
            encode_row(&p, &mut buf);
            st.rows += 1;
            for (g, &n) in st.groups.iter_mut().zip(p.c.iter()) {
                *g += u64::from(n);
            }
            if buf.len() >= CHUNK_ROWS * ROW_BYTES {
                let body = std::mem::replace(&mut buf, Vec::with_capacity(CHUNK_ROWS * ROW_BYTES));
                tx.send(body).map_err(|_| anyhow!("the insert thread stopped"))?;
                if st.rows % 10_000_000 < CHUNK_ROWS as u64 {
                    eprintln!("  {} rows sent", st.rows);
                }
            }
            Ok(())
        });
        if read.is_ok() && !buf.is_empty() {
            let _ = tx.send(std::mem::take(&mut buf));
        }
        drop(tx);
        let posted = poster.join().map_err(|_| anyhow!("the insert thread panicked"))?;
        // The insert's own error is the cause when both failed.
        posted?;
        read?;
        Ok(st)
    })
}

fn table_bytes(ch: &Ch, t: &str) -> Result<(u64, u64, u64)> {
    let v = ch.ints(sql::TABLE_BYTES, &[("t", t.to_string())])?;
    let [rows, bytes, raw] = v[..] else { bail!("system.parts row {v:?}") };
    Ok((rows, bytes, raw))
}

fn drop_load_tables(ch: &Ch) -> Result<()> {
    ch.run(sql::DROP_PARTS_LOAD, &[])?;
    ch.run(sql::DROP_HEX_LOAD, &[])?;
    Ok(())
}

/// Loads [`SAMPLE_RUNS`] runs of consecutive parts spread over the spill
/// into a fresh `race_parts_2020_load`, merges it, and returns its bytes
/// per row on disk. Drops the table after.
fn measure_sample(ch: &Ch, spill: &Path, total: u64) -> Result<Value> {
    ch.run(sql::DROP_PARTS_LOAD, &[])?;
    ch.run(sql::CREATE_PARTS_LOAD, &[])?;
    let mut f = File::open(spill)?;
    let mut sent = 0;
    for k in 0..SAMPLE_RUNS {
        let first = total * k / SAMPLE_RUNS;
        f.seek(SeekFrom::Start(first * PART_RECORD_BYTES as u64))?;
        let run = (&mut f).take(SAMPLE_RUN.min(total - first) * PART_RECORD_BYTES as u64);
        sent += stream_parts(ch, BufReader::new(run))?.rows;
    }
    ch.run(sql::OPTIMIZE_PARTS_LOAD, &[])?;
    let (rows, bytes, raw) = table_bytes(ch, sql::PARTS_LOAD)?;
    ch.run(sql::DROP_PARTS_LOAD, &[])?;
    if rows != sent {
        bail!("sample: sent {sent} rows, the table holds {rows}");
    }
    let per_row = bytes as f64 / rows as f64;
    let parts_estimate = (per_row * total as f64) as u64;
    eprintln!("sample: {rows} rows, {bytes} B on disk ({per_row:.2} B/row, {raw} B uncompressed); estimate for {total} rows: {parts_estimate} B");
    Ok(json!({"rows": rows, "bytes_on_disk": bytes, "uncompressed_bytes": raw, "bytes_per_row": per_row,
              "parts_estimate_bytes": parts_estimate, "hex_estimate_bytes": HEX_ESTIMATE_BYTES}))
}

/// `h3o`'s cell of `(lat, lon)` at `res`.
pub fn h3o_cell(lat: f64, lon: f64, res: u8) -> Result<u64> {
    let r = h3o::Resolution::try_from(res)?;
    Ok(u64::from(h3o::LatLng::new(lat, lon)?.to_cell(r)))
}

pub fn load(spill: &Path) -> Result<Value> {
    let started = Instant::now();
    let ch = Ch::from_env()?;
    let len = std::fs::metadata(spill).with_context(|| format!("{}", spill.display()))?.len();
    if len % PART_RECORD_BYTES as u64 != 0 {
        bail!("{} is {len} B, not whole {PART_RECORD_BYTES} B records", spill.display());
    }
    let total = len / PART_RECORD_BYTES as u64;

    // Nothing is written until the owner and the disk check out.
    let owner = check_owner(&ch)?;
    check_disk(&ch, 0)?;
    if owner == Owner::Absent {
        ch.run(sql::CREATE_DB, &[])?;
        eprintln!("created gpui_map_dots");
    }
    if owner != Owner::Ours {
        if owner == Owner::EmptyUnmarked {
            eprintln!("gpui_map_dots exists, empty and unmarked (an interrupted first load): adopting it");
        }
        ch.run(sql::CREATE_OWNER, &[])?;
        ch.run(sql::INSERT_OWNER, &[])?;
        eprintln!("marked gpui_map_dots with _dotgrid_owner");
    }
    // Whether the live parts and hex tables come from the same build.
    let before = swap_state(&ch)?;
    let before_report = report_state(&before);
    let build = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let result = (|| {
        let sample = measure_sample(&ch, spill, total)?;
        let need = sample["parts_estimate_bytes"].as_u64().unwrap() + HEX_ESTIMATE_BYTES;
        let disk = check_disk(&ch, need.div_ceil(1 << 20))?;
        load_tables(&ch, spill, total, sample, disk, started, build)
    })();
    match &result {
        Ok(_) => {}
        // A validated build is half swapped in: its `_load` tables are the
        // way to finish, so they stay.
        Err(e) if e.downcast_ref::<Refusal>().is_some_and(|r| r.code == EXIT_FINISH_EXCHANGE) => {
            eprintln!("{e:#}");
        }
        Err(e) => {
            eprintln!("load failed ({e:#}); the live race tables are as they were; dropping the _load tables");
            if let Err(c) = drop_load_tables(&ch) {
                eprintln!("cleanup also failed: {c:#}");
            }
        }
    }
    result.map(|mut v| {
        v["before"] = before_report;
        v
    })
}

/// Exit code of a load whose validated tables are not all swapped in.
pub const EXIT_FINISH_EXCHANGE: i32 = 6;
/// Attempts per `EXCHANGE TABLES` before handing over to `--finish-exchange`.
const EXCHANGE_ATTEMPTS: u32 = 3;

/// The four race tables' existence and build stamps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SwapState {
    /// `None`: the table does not exist; `Some(None)`: no (or a foreign) stamp.
    pub parts: Option<Option<(u64, bool)>>,
    pub hex: Option<Option<(u64, bool)>>,
    pub parts_load: Option<Option<(u64, bool)>>,
    pub hex_load: Option<Option<(u64, bool)>>,
}

impl SwapState {
    fn from_rows(text: &str) -> Result<Self> {
        let mut st = SwapState::default();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (name, comment) = line.split_once('\t').unwrap_or((line, ""));
            let v = Some(sql::parse_stamp(comment));
            match name {
                n if n == sql::PARTS => st.parts = v,
                n if n == sql::HEX => st.hex = v,
                n if n == sql::PARTS_LOAD => st.parts_load = v,
                n if n == sql::HEX_LOAD => st.hex_load = v,
                other => bail!("unexpected table {other:?}"),
            }
        }
        Ok(st)
    }

    fn build(s: Option<Option<(u64, bool)>>) -> Option<u64> {
        s.flatten().map(|(b, _)| b)
    }

    fn validated(s: Option<Option<(u64, bool)>>) -> Option<u64> {
        s.flatten().filter(|&(_, v)| v).map(|(b, _)| b)
    }

    /// The live pair: both absent, or both from one stamped build.
    pub fn live_consistent(&self) -> bool {
        match (self.parts, self.hex) {
            (None, None) => true,
            (Some(_), Some(_)) => Self::build(self.parts).is_some() && Self::build(self.parts) == Self::build(self.hex),
            _ => false,
        }
    }
}

/// What the swap step talks to: the server, or a fake in the tests.
pub trait SwapDb {
    fn state(&self) -> Result<SwapState>;
    fn exec(&self, sql: &str) -> Result<()>;
}

impl SwapDb for Ch {
    fn state(&self) -> Result<SwapState> {
        SwapState::from_rows(&self.run(sql::TABLE_COMMENTS, &[])?)
    }
    fn exec(&self, sql: &str) -> Result<()> {
        self.run(sql, &[]).map(|_| ())
    }
}

fn swap_state(ch: &Ch) -> Result<SwapState> {
    ch.state()
}

fn report_state(st: &SwapState) -> Value {
    let (p, h) = (SwapState::build(st.parts), SwapState::build(st.hex));
    let consistent = st.live_consistent();
    let pending = SwapState::validated(st.parts_load).or(SwapState::validated(st.hex_load));
    if !consistent {
        eprintln!(
            "WARNING: the live race tables are not confirmed as one build: race_parts_2020 {}, race_hex_2020 {} (build stamps); a full load-clickhouse replaces and stamps both",
            p.map_or("unstamped or absent".into(), |b| format!("build {b}")),
            h.map_or("unstamped or absent".into(), |b| format!("build {b}"))
        );
    }
    if let Some(b) = pending {
        eprintln!("NOTE: validated build {b} is waiting in the _load tables: `load-clickhouse --finish-exchange` swaps it in; a full load replaces it");
    }
    json!({"parts_build": p, "hex_build": h, "live_consistent": consistent, "pending_validated_build": pending})
}

/// Puts the validated `_load` tables live, whichever of the two EXCHANGEs
/// is still owed, then drops the `_load` names (which then hold the
/// previous tables). Idempotent: a rerun after a partial swap finishes it.
/// Refuses ([`EXIT_FINISH_EXCHANGE`]) when an EXCHANGE keeps failing, with
/// the validated tables kept; refuses (5), before any EXCHANGE, when the
/// newest validated build cannot be reached (an owed `_load` table is not
/// that build: only a full load fixes that).
pub fn finish_exchange(db: &dyn SwapDb, retry_pause: Duration) -> Result<Value> {
    let st = db.state()?;
    // Builds are load start times in ms, so the newest validated stamp on
    // any of the four tables is the build that should be live. (After one
    // EXCHANGE a `_load` name holds the previous live table, validated too:
    // older, so never chosen.)
    let newest = [st.parts, st.hex, st.parts_load, st.hex_load].into_iter().filter_map(SwapState::validated).max();
    let Some(b) = newest.filter(|&b| SwapState::build(st.parts) != Some(b) || SwapState::build(st.hex) != Some(b)) else {
        return Ok(json!({"exchanged": [], "build": newest, "live_consistent": st.live_consistent(), "note": "nothing pending"}));
    };
    let pairs = [
        (sql::PARTS, st.parts, st.parts_load, sql::CREATE_PARTS, sql::EXCHANGE_PARTS),
        (sql::HEX, st.hex, st.hex_load, sql::CREATE_HEX, sql::EXCHANGE_HEX),
    ];
    // Every owed EXCHANGE must have build b validated under its `_load`
    // name, checked before the first one runs.
    let owed: Vec<_> = pairs.into_iter().filter(|p| SwapState::build(p.1) != Some(b)).collect();
    if let Some(p) = owed.iter().find(|p| SwapState::validated(p.2) != Some(b)) {
        return Err(refuse(
            5,
            format!("{} is not build {b} and its _load table is not validated build {b} ({st:?}): rerun load-clickhouse", p.0),
        ));
    }
    let mut exchanged = Vec::new();
    for (name, _, _, create, exchange) in owed {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let r = db.exec(create).and_then(|_| db.exec(exchange));
            match r {
                Ok(()) => break,
                Err(e) if attempt < EXCHANGE_ATTEMPTS => {
                    eprintln!("{name}: EXCHANGE attempt {attempt} failed ({e:#}); retrying");
                    std::thread::sleep(retry_pause);
                }
                Err(e) => {
                    return Err(refuse(
                        EXIT_FINISH_EXCHANGE,
                        format!(
                            "validated build {b}: EXCHANGE into {name} failed {attempt} times ({e:#}). Swapped so far: {exchanged:?}. \
                             The validated _load tables are kept: run `race-build load-clickhouse --finish-exchange` to finish"
                        ),
                    ))
                }
            }
        }
        exchanged.push(name);
    }
    let after = db.state()?;
    if SwapState::build(after.parts) != Some(b) || SwapState::build(after.hex) != Some(b) {
        return Err(refuse(EXIT_FINISH_EXCHANGE, format!("after the swap the live tables are not both build {b}: {after:?}")));
    }
    db.exec(sql::DROP_PARTS_LOAD)?;
    db.exec(sql::DROP_HEX_LOAD)?;
    eprintln!("build {b} live in race_parts_2020 and race_hex_2020 (exchanged now: {exchanged:?})");
    Ok(json!({"exchanged": exchanged, "build": b}))
}

/// `load-clickhouse --finish-exchange`.
pub fn finish(ch: &Ch) -> Result<Value> {
    if check_owner(ch)? != Owner::Ours {
        return Err(refuse(5, "gpui_map_dots has no race tables to finish (no marker)"));
    }
    let before = report_state(&ch.state()?);
    let mut v = finish_exchange(ch, Duration::from_secs(2))?;
    v["before"] = before;
    Ok(v)
}

#[allow(clippy::too_many_arguments)]
fn load_tables(ch: &Ch, spill: &Path, total: u64, sample: Value, disk: Value, started: Instant, build: u64) -> Result<Value> {
    drop_load_tables(ch)?;
    ch.run(sql::CREATE_PARTS_LOAD, &[])?;
    ch.run(sql::CREATE_HEX_LOAD, &[])?;
    ch.run(&sql::stamp(sql::STAMP_PARTS_LOAD, build, false), &[])?;
    ch.run(&sql::stamp(sql::STAMP_HEX_LOAD, build, false), &[])?;

    // Stream every part.
    let t = Instant::now();
    let streamed = stream_parts(ch, BufReader::with_capacity(1 << 22, File::open(spill)?))?;
    let stream_s = t.elapsed().as_secs_f64();
    eprintln!("streamed {} rows in {stream_s:.1} s", streamed.rows);

    // Aggregate r4..=r8.
    let mut agg_s = Vec::new();
    for res in RESOLUTIONS {
        let t = Instant::now();
        ch.run(sql::INSERT_HEX_LOAD, &[("res", res.to_string())])?;
        agg_s.push(t.elapsed().as_secs_f64());
        eprintln!("r{res} aggregated in {:.1} s", agg_s.last().unwrap());
    }

    let mut failed = Vec::new();
    let want: Vec<u64> = US_GROUP_TOTALS.iter().copied().chain([US_TOTAL]).collect();

    // The parts: count, sums and the coordinate box.
    if streamed.rows != total || streamed.groups != US_GROUP_TOTALS {
        failed.push(format!("streamed {} rows (file {total}), groups {:?}", streamed.rows, streamed.groups));
    }
    let p = ch.ints(sql::PARTS_SUMS, &[])?;
    let (p_rows, p_sums, p_outside) = (p[0], &p[1..10], p[10]);
    eprintln!("race_parts_2020: {p_rows} rows, sums {p_sums:?}, {p_outside} outside the US box");
    if p_rows != total || p_sums != want.as_slice() || p_outside != 0 {
        failed.push(format!("race_parts_2020: {p_rows} rows, sums {p_sums:?}, {p_outside} outside"));
    }

    // Every resolution: exact sums, total = the groups' sum.
    let mut per_res = Vec::new();
    for line in ch.run(sql::HEX_SUMS, &[])?.lines() {
        let v: Vec<u64> = line.split('\t').map(|x| x.parse()).collect::<Result<_, _>>()?;
        let (res, rows, ge10, sums, bad_total) = (v[0], v[1], v[2], &v[3..12], v[12]);
        eprintln!("r{res}: {rows} cells ({ge10} with >= 10 people), sums {sums:?}, {bad_total} rows with total != groups");
        if sums != want.as_slice() || bad_total != 0 {
            failed.push(format!("r{res}: sums {sums:?}, {bad_total} bad totals"));
        }
        per_res.push(json!({"res": res, "cells": rows, "cells_ge_10": ge10, "group_sums": &sums[..8], "total": sums[8]}));
    }
    if per_res.len() != RESOLUTIONS.count() {
        failed.push(format!("{} resolutions in race_hex_2020, want 5", per_res.len()));
    }

    // The round-trip gate.
    let t = Instant::now();
    let g = ch.run(sql::ROUND_TRIP_GATE, &[])?;
    let gf: Vec<&str> = g.trim().split('\t').collect();
    let [checked, over, missing, max_m] = gf[..] else { bail!("gate row {g:?}") };
    let (checked, over, missing, max_m): (u64, u64, u64, f64) = (checked.parse()?, over.parse()?, missing.parse()?, max_m.parse()?);
    eprintln!("round trip: {checked} parts, {over} over {GATE_M} m, {missing} with no r8 row, max {max_m:.0} m ({:.1} s)", t.elapsed().as_secs_f64());
    if checked != total || over != 0 || missing != 0 {
        failed.push(format!("round trip: {over} over {GATE_M} m, {missing} missing, {checked} checked of {total}"));
    }

    // h3o on 1,000 parts, every resolution.
    let mut sampled = 0u32;
    let mut disagree = Vec::new();
    for line in ch.run(sql::H3_SAMPLE, &[])?.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [lat, lon, ref cells @ ..] = f[..] else { bail!("h3 sample row {line:?}") };
        let (lat, lon): (f64, f64) = (lat.parse()?, lon.parse()?);
        sampled += 1;
        for (res, cell) in RESOLUTIONS.zip(cells) {
            let server: u64 = cell.parse()?;
            let ours = h3o_cell(lat, lon, res)?;
            if server != ours {
                disagree.push(json!({"lat": lat, "lon": lon, "res": res, "clickhouse": format!("{server:x}"), "h3o": format!("{ours:x}")}));
            }
        }
        if cells.len() != 5 {
            bail!("h3 sample row {line:?}");
        }
    }
    let matched = sampled as usize - disagree.iter().map(|d| (d["lat"].to_string(), d["lon"].to_string())).collect::<std::collections::BTreeSet<_>>().len();
    eprintln!("h3o: {matched}/{sampled} parts agree at every resolution ({} cell disagreements)", disagree.len());
    if sampled != 1000 || !disagree.is_empty() {
        failed.push(format!("h3o: {} of {} cells disagree", disagree.len(), sampled * 5));
    }

    if !failed.is_empty() {
        return Err(refuse(5, failed.join("; ")));
    }

    // Every check passed: merge, measure, swap in.
    let t = Instant::now();
    ch.run(sql::OPTIMIZE_PARTS_LOAD, &[])?;
    ch.run(sql::OPTIMIZE_HEX_LOAD, &[])?;
    let optimize_s = t.elapsed().as_secs_f64();
    let parts_bytes = table_bytes(ch, sql::PARTS_LOAD)?;
    let hex_bytes = table_bytes(ch, sql::HEX_LOAD)?;
    ch.run(&sql::stamp(sql::STAMP_PARTS_LOAD, build, true), &[])?;
    ch.run(&sql::stamp(sql::STAMP_HEX_LOAD, build, true), &[])?;
    let swap = finish_exchange(ch, Duration::from_secs(2))?;
    let (live_parts, live_hex) = (ch.ints(sql::COUNT_PARTS, &[])?[0], ch.ints(sql::COUNT_HEX, &[])?[0]);
    if live_parts != parts_bytes.0 || live_hex != hex_bytes.0 {
        bail!("after EXCHANGE: {live_parts} parts and {live_hex} hex rows live, the load had {} and {}", parts_bytes.0, hex_bytes.0);
    }
    let bytes_total = parts_bytes.1 + hex_bytes.1;
    eprintln!(
        "MG-R9: race_parts_2020 {} rows {} B; race_hex_2020 {} rows {} B; {bytes_total} B total",
        parts_bytes.0, parts_bytes.1, hex_bytes.0, hex_bytes.1
    );

    Ok(json!({
        "build": build,
        "swap": swap,
        "spill_parts": total,
        "sample": sample,
        "disk": disk,
        "stream_s": stream_s,
        "aggregate_s": agg_s,
        "optimize_s": optimize_s,
        "parts": {"rows": parts_bytes.0, "bytes_on_disk": parts_bytes.1, "uncompressed_bytes": parts_bytes.2,
                  "bytes_per_row": parts_bytes.1 as f64 / parts_bytes.0 as f64, "group_sums": &p_sums[..8], "total": p_sums[8]},
        "hex": {"rows": hex_bytes.0, "bytes_on_disk": hex_bytes.1, "uncompressed_bytes": hex_bytes.2, "per_res": per_res},
        "bytes_on_disk_total": bytes_total,
        "round_trip": {"checked": checked, "over_1500_m": over, "missing_r8_row": missing, "max_m": max_m},
        "h3o": {"sampled": sampled, "matched": matched, "disagree": disagree},
        "wall_s": started.elapsed().as_secs_f64(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiger::project;
    use gpui_maps::overlay::dotgrid::parts::{Part, Tri};

    fn part_at(lat: f64, lon: f64) -> SpillPart {
        let p = project(lon, lat);
        let tri = Tri { a: p, b: [p[0] + 3, p[1]], c: [p[0], p[1] + 3] };
        SpillPart::new(110_010_001_011_000, 11, 7, 0, &Part { tri, c: [1, 2, 3, 4, 5, 6, 7, 65_535] })
    }

    type Stamp = Option<Option<(u64, bool)>>;

    /// A fake server: the four race tables' stamps, EXCHANGE / CREATE IF
    /// NOT EXISTS / DROP semantics, and `fail_hex` failing EXCHANGEs into
    /// `race_hex_2020` that many times.
    struct Fake {
        st: std::cell::RefCell<SwapState>,
        fail_hex: std::cell::Cell<u32>,
        log: std::cell::RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(parts: Stamp, hex: Stamp, parts_load: Stamp, hex_load: Stamp, fail_hex: u32) -> Self {
            Fake {
                st: SwapState { parts, hex, parts_load, hex_load }.into(),
                fail_hex: fail_hex.into(),
                log: Vec::new().into(),
            }
        }
    }

    impl SwapDb for Fake {
        fn state(&self) -> Result<SwapState> {
            Ok(self.st.borrow().clone())
        }
        fn exec(&self, q: &str) -> Result<()> {
            let mut st = self.st.borrow_mut();
            let st = &mut *st;
            let name = [
                (sql::CREATE_PARTS, "create parts"),
                (sql::CREATE_HEX, "create hex"),
                (sql::EXCHANGE_PARTS, "exchange parts"),
                (sql::EXCHANGE_HEX, "exchange hex"),
                (sql::DROP_PARTS_LOAD, "drop parts_load"),
                (sql::DROP_HEX_LOAD, "drop hex_load"),
            ]
            .iter()
            .find(|(s, _)| *s == q)
            .map(|&(_, n)| n)
            .unwrap_or_else(|| panic!("unexpected statement {q}"));
            match name {
                "create parts" => st.parts = st.parts.or(Some(None)),
                "create hex" => st.hex = st.hex.or(Some(None)),
                "exchange parts" => {
                    assert!(st.parts.is_some() && st.parts_load.is_some());
                    std::mem::swap(&mut st.parts, &mut st.parts_load);
                }
                "exchange hex" => {
                    if self.fail_hex.get() > 0 {
                        self.fail_hex.set(self.fail_hex.get() - 1);
                        bail!("simulated: Code: 999. timeout");
                    }
                    assert!(st.hex.is_some() && st.hex_load.is_some());
                    std::mem::swap(&mut st.hex, &mut st.hex_load);
                }
                "drop parts_load" => st.parts_load = None,
                _ => st.hex_load = None,
            }
            self.log.borrow_mut().push(name);
            Ok(())
        }
    }

    const OLD: Stamp = Some(Some((1, true)));
    const NEW: Stamp = Some(Some((2, true)));

    #[test]
    fn a_failed_second_exchange_keeps_the_validated_hex_table_and_finish_exchange_completes_it() {
        // EXCHANGE_HEX fails every attempt: parts are new, hex is old.
        let db = Fake::new(OLD, OLD, NEW, NEW, EXCHANGE_ATTEMPTS);
        let err = finish_exchange(&db, Duration::ZERO).unwrap_err();
        let r = err.downcast_ref::<Refusal>().expect("a refusal");
        assert_eq!(r.code, EXIT_FINISH_EXCHANGE);
        assert!(r.msg.contains("--finish-exchange") && r.msg.contains("race_hex_2020"), "{}", r.msg);
        let st = db.state().unwrap();
        assert_eq!((st.parts, st.hex, st.hex_load), (NEW, OLD, NEW), "the validated hex _load table is kept");
        assert!(!st.live_consistent());
        assert!(!db.log.borrow().iter().any(|s| s.starts_with("drop")), "nothing dropped: {:?}", db.log.borrow());

        // `--finish-exchange`: only the hex EXCHANGE is owed.
        db.log.borrow_mut().clear();
        let v = finish_exchange(&db, Duration::ZERO).unwrap();
        assert_eq!(v["exchanged"], json!(["race_hex_2020"]));
        let st = db.state().unwrap();
        assert_eq!((st.parts, st.hex, st.parts_load, st.hex_load), (NEW, NEW, None, None));
        assert!(st.live_consistent());
        // A third run has nothing to do.
        assert_eq!(finish_exchange(&db, Duration::ZERO).unwrap()["note"], json!("nothing pending"));
    }

    #[test]
    fn an_older_validated_table_left_under_a_load_name_is_never_swapped_back() {
        // Both swaps done, killed before the drops: the `_load` names hold
        // the previous (validated) build.
        let db = Fake::new(NEW, NEW, OLD, OLD, 0);
        assert_eq!(finish_exchange(&db, Duration::ZERO).unwrap()["note"], json!("nothing pending"));
        assert!(db.log.borrow().is_empty());
        // Two different validated builds pending and neither live: refuse.
        let db = Fake::new(OLD, OLD, Some(Some((3, true))), NEW, 0);
        let err = finish_exchange(&db, Duration::ZERO).unwrap_err();
        assert_eq!(err.downcast_ref::<Refusal>().unwrap().code, 5);
        assert!(db.log.borrow().is_empty(), "refused before any EXCHANGE: {:?}", db.log.borrow());
    }

    #[test]
    fn a_transient_exchange_failure_is_retried() {
        let db = Fake::new(None, None, NEW, NEW, EXCHANGE_ATTEMPTS - 1);
        let v = finish_exchange(&db, Duration::ZERO).unwrap();
        assert_eq!(v["exchanged"], json!(["race_parts_2020", "race_hex_2020"]));
        let st = db.state().unwrap();
        assert_eq!((st.parts, st.hex, st.parts_load, st.hex_load), (NEW, NEW, None, None));
    }

    #[test]
    fn an_unvalidated_build_is_never_swapped_in() {
        let building = Some(Some((2, false)));
        let db = Fake::new(OLD, OLD, building, building, 0);
        assert_eq!(finish_exchange(&db, Duration::ZERO).unwrap()["note"], json!("nothing pending"));
        assert!(db.log.borrow().is_empty());
        // Build 2 is live for parts, but hex has nothing validated to finish it: refuse.
        let db = Fake::new(NEW, OLD, None, building, 0);
        let err = finish_exchange(&db, Duration::ZERO).unwrap_err();
        assert_eq!(err.downcast_ref::<Refusal>().unwrap().code, 5);
        assert!(db.log.borrow().is_empty());
    }

    #[test]
    fn the_live_pair_is_consistent_only_when_both_carry_one_build() {
        let st = |parts, hex| SwapState { parts, hex, ..Default::default() };
        assert!(st(None, None).live_consistent());
        assert!(st(NEW, Some(Some((2, false)))).live_consistent());
        assert!(!st(NEW, OLD).live_consistent());
        assert!(!st(NEW, None).live_consistent());
        assert!(!st(Some(None), Some(None)).live_consistent(), "unstamped (loaded before stamps): unknown");
        let rows = "race_parts_2020\trace-build build=2 state=validated\nrace_hex_2020\t\nrace_hex_2020_load\trace-build build=3 state=building\n";
        let parsed = SwapState::from_rows(rows).unwrap();
        assert_eq!(parsed, SwapState { parts: NEW, hex: Some(None), parts_load: None, hex_load: Some(Some((3, false))) });
    }

    #[test]
    fn an_empty_unmarked_database_is_adopted_and_a_populated_one_refused() {
        assert_eq!(owner_from(false, false, 0).unwrap(), Owner::Absent);
        assert_eq!(owner_from(true, true, 3).unwrap(), Owner::Ours);
        assert_eq!(owner_from(true, false, 0).unwrap(), Owner::EmptyUnmarked);
        let err = owner_from(true, false, 1).unwrap_err();
        assert_eq!(err.downcast_ref::<Refusal>().unwrap().code, 3);
    }

    #[test]
    fn a_server_error_body_is_redacted() {
        // The body path of `Ch::send` goes through the same redaction as the URL.
        let text = redact_url_credentials("Code: 516. http://user:SECRET@host:8123/?password=SECRET");
        assert!(!text.contains("SECRET"), "{text}");
    }

    /// The stamp templates on the real server: `race_hex_2020_load` (absent
    /// unless a load is running, which this refuses) is created, stamped
    /// `building` then `validated`, read back through `TABLE_COMMENTS` and
    /// `SwapState`, and dropped. The live tables are only read.
    #[test]
    #[ignore = "needs the local ClickHouse with gpui_map_dots; run with CLICKHOUSE_URL"]
    fn live_a_stamp_reads_back_through_table_comments() {
        let ch = Ch::from_env().unwrap();
        assert_eq!(check_owner(&ch).unwrap(), Owner::Ours);
        let before = ch.state().unwrap();
        assert!(before.parts_load.is_none() && before.hex_load.is_none(), "a load is in progress; not touching its tables");
        ch.run(sql::CREATE_HEX_LOAD, &[]).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ch.run(&sql::stamp(sql::STAMP_HEX_LOAD, 42, false), &[]).unwrap();
            assert_eq!(ch.state().unwrap().hex_load, Some(Some((42, false))));
            ch.run(&sql::stamp(sql::STAMP_HEX_LOAD, 42, true), &[]).unwrap();
            let st = ch.state().unwrap();
            assert_eq!(st.hex_load, Some(Some((42, true))));
            assert_eq!((st.parts, st.hex), (before.parts, before.hex), "live tables unchanged");
            println!("live: {st:?}");
        }));
        ch.run(sql::DROP_HEX_LOAD, &[]).unwrap();
        assert!(ch.state().unwrap().hex_load.is_none());
        result.unwrap();
    }

    #[test]
    fn a_row_is_sixty_one_little_endian_bytes_in_column_order() {
        let p = part_at(38.8951, -77.0365);
        let mut b = Vec::new();
        encode_row(&p, &mut b);
        assert_eq!(b.len(), ROW_BYTES);
        assert_eq!(ROW_BYTES, 61);
        assert_eq!(b[0], 11);
        assert_eq!(&b[1..9], &110_010_001_011_000u64.to_le_bytes());
        assert_eq!(&b[9..13], &7u32.to_le_bytes());
        let lat = f64::from_le_bytes(b[13..21].try_into().unwrap());
        let lon = f64::from_le_bytes(b[21..29].try_into().unwrap());
        // lat first, lon second, and the centroid is where the part is.
        assert!((lat - 38.8951).abs() < 1e-5 && (lon + 77.0365).abs() < 1e-5, "{lat} {lon}");
        assert_eq!(&b[29..33], &1u32.to_le_bytes());
        assert_eq!(&b[57..61], &65_535u32.to_le_bytes());
    }

    #[test]
    fn h3o_pins_the_argument_order() {
        // New York's r8 cell centre is near New York; the swapped call is
        // a valid cell elsewhere.
        let (lat, lon) = (40.7128, -74.006);
        let c = h3o::CellIndex::try_from(h3o_cell(lat, lon, 8).unwrap()).unwrap();
        let ll = h3o::LatLng::from(c);
        assert!((ll.lat() - lat).abs() < 0.01 && (ll.lng() - lon).abs() < 0.01);
        assert_ne!(h3o_cell(lat, lon, 8).unwrap(), h3o_cell(lon, lat, 8).unwrap());
    }

    /// The server's own argument order: `geoToH3(lat, lon)` round-trips to
    /// within the gate's bound, the swapped call lands > 1,000 km away, and
    /// `h3o` agrees bit for bit. Creates no tables.
    #[test]
    #[ignore = "needs the local ClickHouse; run with CLICKHOUSE_URL"]
    fn live_geo_to_h3_round_trips_lat_then_lon_and_matches_h3o() {
        let ch = Ch::from_env().unwrap();
        for (lat, lon) in [(40.7128, -74.006), (41.8781, -87.6298), (25.7617, -80.1918), (42.3601, -71.0589)] {
            let q = format!(
                "SELECT geoToH3(p.lat, p.lon, 8), greatCircleDistance(p.lon, p.lat, h3ToGeo(geoToH3(p.lat, p.lon, 8)).2, h3ToGeo(geoToH3(p.lat, p.lon, 8)).1), \
                 greatCircleDistance(p.lon, p.lat, h3ToGeo(geoToH3(p.lon, p.lat, 8)).2, h3ToGeo(geoToH3(p.lon, p.lat, 8)).1) \
                 FROM (SELECT {lat} AS lat, {lon} AS lon) AS p FORMAT TabSeparated"
            );
            let text = ch.run(&q, &[]).unwrap();
            let f: Vec<&str> = text.trim().split('\t').collect();
            let (cell, d, swapped): (u64, f64, f64) = (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].parse().unwrap());
            println!("({lat}, {lon}): r8 centre {d:.0} m away; swapped {:.0} km", swapped / 1000.0);
            assert!(d < GATE_M, "{d}");
            assert!(swapped > 1_000_000.0, "{swapped}");
            assert_eq!(cell, h3o_cell(lat, lon, 8).unwrap());
        }
    }
}
