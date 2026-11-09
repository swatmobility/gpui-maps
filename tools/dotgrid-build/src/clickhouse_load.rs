//! `--load-clickhouse`: the converter's own pyramid, streamed as RowBinary
//! into `gpui_map_dots.<table>`, then checked.
//!
//! At 3″ (`LoadOptions::sql_levels`, Task 13) only level 0 is streamed:
//! levels 1..=9 are built in `<t>_load` by `sql::insert_parent_level`, each
//! from the one below, and checked against the converter's per-level cell
//! counts and sums like a streamed level. Streaming all ten levels would
//! send 706 M more rows (+50 %) for sums the server can make itself.
//!
//! The loader owns one database and nothing else on the server. Before
//! anything it probes `system.databases`: a `gpui_map_dots` without the
//! `_dotgrid_owner` marker was not made by this tool, and the load refuses
//! (exit 3). It refuses too (exit 4) when either the ClickHouse data disk or
//! the local data directory has less free space than the table estimate plus
//! 10 GB. A failed check after the load exits 5: the H3 round trip (MG-13,
//! 0 rows over the converter's level-0 cell count), 1,000 sampled `h3_r10`
//! values against `h3o` bit-for-bit, the per-level sums against the
//! converter's, and level 1 recomputed in SQL (`sql::insert_parent_level_into`,
//! the 3″ path's aggregation) against the converter's level 1.
//!
//! **A failed load never touches the epoch already in the table.**
//! - Every input is validated before the first write: each TIFF opens and
//!   passes the scale and lattice checks (`validate_inputs`), `--table`
//!   matches `--arcsec` (`main`), the owner marker and the disk are checked.
//! - The new epoch is built in scratch tables: RowBinary into `<t>_stage`,
//!   `h3_r10` filled into `<t>_load`, and every check runs on `<t>_load`.
//! - Only when all checks pass does `ALTER TABLE <t> REPLACE PARTITION
//!   <epoch> FROM <t>_load` swap it in, in one step (`<t>` is `PARTITION BY
//!   epoch` for that reason). Before that statement the previous rows are
//!   untouched and readable; a failure at any point, or a killed process,
//!   leaves them as they were.
//! - On failure the scratch tables (`_stage`, `_load`, `_check`) are
//!   dropped. A failed FIRST load of an epoch therefore leaves no rows for it.

use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gpui_maps::geo::TileId;
use gpui_maps::sprite::redact_url_credentials;
use gpui_maps::overlay::dotgrid::{grid, sql_guard::DOTS_DB, GridDescriptor};
use serde_json::{json, Value};

use crate::sql::{self, Stmt};
use crate::Options;

/// RowBinary rows per POST.
const CHUNK_ROWS: u64 = 1_000_000;
/// `u16 epoch, u8 level, u32 tile, u32 row, u32 col, f32 pop`.
const ROW_BYTES: usize = 2 + 1 + 4 + 4 + 4 + 4;
/// Headroom every load keeps free on both disks (global constraints).
pub const DISK_FLOOR_MB: u64 = 10 * 1024;
/// The table estimate the brief gives for one 30″ epoch.
pub const ESTIMATE_30SS_MB: u64 = 1024;
/// One 3″ epoch's peak disk (Task 13 step 5), in MB (MiB). The table
/// itself: MG-12's 6.68 B/row (30″, `system.parts`) × MG-17's 2,119,386,739
/// cells over all ten levels (`cells_per_level` of `E2025_3ss.report.json`)
/// = 14.15 GB = 13,498 MiB. The load's phases peak above that, all on one
/// volume on this machine (ClickHouse's data path and the spill directory):
/// - streaming: the converter's spill (≈ 11 GB, Task 12) + the scratch
///   archive (1.9 GB) + the RowBinary stage table filling with level 0
///   (1.41 B rows, default codecs; its bytes/row are unmeasured);
/// - the `h3_r10` fill: the stage and `_load`'s level 0 side by side;
/// - the SQL levels: `_load` whole + the GROUP BY's external spill.
/// Each phase is ≈ 20-22 GB by these figures, the unmeasured stage the
/// largest unknown; Task 10's review said the staged swap needs 2-3× the
/// table, and this takes 2×. `check_disk` adds the 10 GB floor.
pub const ESTIMATE_3SS_MB: u64 = 2 * 13_498;
/// The level-0 → 1 aggregation of a 3″ epoch groups 1.41 B rows into
/// 455 M parents: spill the GROUP BY to disk past 4 GB rather than hold it.
const PARENT_LEVEL_SETTINGS: [(&str, &str); 1] = [("max_bytes_before_external_group_by", "4000000000")];

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

pub fn refuse(code: i32, msg: impl Into<String>) -> anyhow::Error {
    Refusal { code, msg: msg.into() }.into()
}

/// `e` and its causes (`reqwest::Error` alone says "error sending
/// request", not "Connection refused"), credentials redacted.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text = format!("{text}: {c}");
        cause = c.source();
    }
    redact_url_credentials(&text)
}

/// A ClickHouse HTTP endpoint. Every statement goes as a POST body (or, for
/// a RowBinary insert, as `query=` with the rows as the body), values as
/// `param_*` URL arguments.
pub struct Ch {
    url: String,
    http: reqwest::blocking::Client,
}

impl Ch {
    pub fn new(url: &str) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(1800))
            .build()?;
        Ok(Ch { url: url.trim_end_matches('/').to_string(), http })
    }

    /// The URL for an error message: `CLICKHOUSE_URL` may carry
    /// `user:pass@` or `?password=`, and an error is printed.
    fn shown_url(&self) -> String {
        redact_url_credentials(&self.url)
    }

    pub fn from_env() -> Result<Self> {
        Ch::new(&std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into()))
    }

    fn send(&self, args: Vec<(String, String)>, body: Vec<u8>, what: &str) -> Result<String> {
        let mut args = args;
        // An exception mid-query then arrives as an HTTP error, never as a
        // 200 whose body ends in `Code: …`.
        args.push(("wait_end_of_query".into(), "1".into()));
        let mut resp = self
            .http
            .post(&self.url)
            .query(&args)
            .body(body)
            .send()
            .map_err(|e| anyhow!("ClickHouse at {}: {what}: {}", self.shown_url(), error_chain(&e.without_url())))?;
        let mut text = String::new();
        resp.read_to_string(&mut text)
            .map_err(|e| anyhow!("ClickHouse at {}: {what}: reading the response: {}", self.shown_url(), redact_url_credentials(&e.to_string())))?;
        if !resp.status().is_success() || resp.headers().contains_key("x-clickhouse-exception-code") {
            bail!("ClickHouse {}: {}\n  statement: {what}", resp.status(), text.trim());
        }
        Ok(text)
    }

    /// Runs `st` with its parameters and any `settings` (e.g. `mutations_sync`).
    pub fn run(&self, st: &Stmt, settings: &[(&str, &str)]) -> Result<String> {
        let mut args: Vec<(String, String)> = st.params.iter().map(|(k, v)| (format!("param_{k}"), v.clone())).collect();
        args.extend(settings.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        self.send(args, st.sql.clone().into_bytes(), &st.sql)
    }

    pub fn run_sql(&self, sql: &str) -> Result<String> {
        self.send(Vec::new(), sql.as_bytes().to_vec(), sql)
    }

    fn insert(&self, sql: &str, rows: Vec<u8>) -> Result<()> {
        self.send(vec![("query".into(), sql.into())], rows, sql).map(|_| ())
    }

    fn count(&self, st: &Stmt) -> Result<u64> {
        let text = self.run(st, &[])?;
        text.trim().parse().with_context(|| format!("not a count: {text:?} from {}", st.sql))
    }

    /// One TabSeparated row of counts.
    fn counts(&self, st: &Stmt) -> Result<Vec<u64>> {
        let text = self.run(st, &[])?;
        text.trim()
            .split('\t')
            .map(|c| c.parse().with_context(|| format!("not counts: {text:?} from {}", st.sql)))
            .collect()
    }
}

/// Refuses (exit 3) when `db` exists without the `_dotgrid_owner` marker.
/// Returns whether it exists. Read-only: only `system.*` is queried.
pub fn check_owner(ch: &Ch, db: &str) -> Result<bool> {
    if ch.count(&sql::database_exists(db))? == 0 {
        return Ok(false);
    }
    if ch.count(&sql::owner_marker_exists(db))? != 1 {
        return Err(refuse(
            3,
            format!(
                "{db} exists and was not created by dotgrid-build (it has no _dotgrid_owner table). If a first \
                 dotgrid-build load crashed between CREATE DATABASE and the marker, the database is empty: check \
                 with SELECT name FROM system.tables WHERE database = '{db}', and if nothing is listed, \
                 DROP DATABASE {db} by hand and rerun"
            ),
        ));
    }
    Ok(true)
}

/// `df -m <path>`'s available megabytes.
pub fn df_free_mb(path: &Path) -> Result<(u64, String)> {
    let out = std::process::Command::new("df").arg("-m").arg(path).output().context("df")?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let line = text.lines().nth(1).with_context(|| format!("df -m {}: {text}", path.display()))?;
    let avail = line.split_whitespace().nth(3).and_then(|a| a.parse().ok());
    Ok((avail.with_context(|| format!("df -m {}: {line}", path.display()))?, text.trim().to_string()))
}

/// Refuses (exit 4) when any ClickHouse disk, or the local `data_dir`, has
/// less than `need_mb + 10 GB` free. Returns what it read, for the report.
pub fn check_disk(ch: &Ch, data_dir: &Path, need_mb: u64) -> Result<Value> {
    let mut seen = Vec::new();
    let mut short = Vec::new();
    for line in ch.run_sql(sql::DISKS)?.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [name, path, free] = f[..] else { bail!("system.disks row {line:?}") };
        let free_mb = free.parse::<u64>().context("system.disks free_space")? >> 20;
        let (df_mb, df) = df_free_mb(Path::new(path))?;
        eprintln!("disk {name} {path}: system.disks {free_mb} MB free; df {df_mb} MB free");
        seen.push(json!({"disk": name, "path": path, "system_disks_free_mb": free_mb, "df_free_mb": df_mb, "df": df}));
        for mb in [free_mb, df_mb] {
            if mb < need_mb + DISK_FLOOR_MB {
                short.push(format!("{path}: {mb} MB free"));
            }
        }
    }
    let (local_mb, df) = df_free_mb(data_dir)?;
    eprintln!("local {}: df {local_mb} MB free (need {need_mb} MB + {DISK_FLOOR_MB} MB floor)", data_dir.display());
    seen.push(json!({"path": data_dir, "df_free_mb": local_mb, "df": df}));
    if local_mb < need_mb + DISK_FLOOR_MB {
        short.push(format!("{}: {local_mb} MB free", data_dir.display()));
    }
    if !short.is_empty() {
        return Err(refuse(4, format!("not enough disk: need {need_mb} MB + {DISK_FLOOR_MB} MB floor; {}", short.join("; "))));
    }
    Ok(json!({"need_mb": need_mb, "floor_mb": DISK_FLOOR_MB, "disks": seen}))
}

/// Appends RowBinary rows and POSTs them to the stage table every
/// [`CHUNK_ROWS`]. Shared by the pyramid's worker threads.
struct RowStream<'a> {
    ch: &'a Ch,
    insert: String,
    epoch: u16,
    /// Levels below this are streamed; the rest are left to SQL.
    levels: u8,
    buf: Mutex<(Vec<u8>, u64)>,
    rows_per_level: Mutex<Vec<u64>>,
}

impl RowStream<'_> {
    fn push(&self, level: u8, tile: TileId, w: &grid::CellWindow, cells: &[(u32, f64)]) -> Result<()> {
        if level >= self.levels {
            return Ok(());
        }
        let tile_key = (tile.x << 16) | tile.y;
        let mut rows = Vec::with_capacity(cells.len() * ROW_BYTES);
        for &(idx, v) in cells {
            let (row, col) = grid::row_col(w, idx);
            rows.extend_from_slice(&self.epoch.to_le_bytes());
            rows.push(level);
            rows.extend_from_slice(&tile_key.to_le_bytes());
            rows.extend_from_slice(&row.to_le_bytes());
            rows.extend_from_slice(&col.to_le_bytes());
            rows.extend_from_slice(&(v as f32).to_le_bytes());
        }
        {
            let mut per = self.rows_per_level.lock().unwrap();
            if per.len() <= level as usize {
                per.resize(level as usize + 1, 0);
            }
            per[level as usize] += cells.len() as u64;
        }
        let full = {
            let mut b = self.buf.lock().unwrap();
            b.0.extend_from_slice(&rows);
            b.1 += cells.len() as u64;
            (b.1 >= CHUNK_ROWS).then(|| (std::mem::take(&mut b.0), std::mem::take(&mut b.1)))
        };
        if let Some((body, _)) = full {
            self.ch.insert(&self.insert, body)?;
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        let (body, n) = std::mem::take(&mut *self.buf.lock().unwrap());
        if n > 0 {
            self.ch.insert(&self.insert, body)?;
        }
        Ok(())
    }
}

/// `h3o`'s r10 cell of a level-0 cell's centre, as the table's `h3_r10`.
pub fn h3o_r10(d: &GridDescriptor, row: u32, col: u32) -> Result<u64> {
    let (lat, lon) = grid::cell_centre(d, 0, row, col);
    Ok(u64::from(h3o::LatLng::new(lat, lon)?.to_cell(h3o::Resolution::Ten)))
}

pub struct LoadOptions {
    pub build: Options,
    pub table: String,
    pub estimate_mb: u64,
    /// Levels 1.. from `sql::insert_parent_level` on the table's own level
    /// 0 instead of the converter's stream (the 3″ path).
    pub sql_levels: bool,
}

pub fn load(ch: &Ch, o: &LoadOptions) -> Result<Value> {
    let started = Instant::now();
    let (t, epoch) = (o.table.as_str(), o.build.epoch);
    let d = crate::descriptor(o.build.arcsec, epoch);

    // Nothing is written until every input and precondition checks out.
    crate::validate_inputs(&o.build)?;
    let existed = check_owner(ch, DOTS_DB)?;
    std::fs::create_dir_all(&o.build.spill_dir)?;
    let disk = check_disk(ch, &o.build.spill_dir, o.estimate_mb)?;
    if !existed {
        // The marker is the first object in the new database, and CREATE
        // DATABASE has no IF NOT EXISTS: a concurrent creator fails this load.
        ch.run_sql(sql::CREATE_DB)?;
        ch.run_sql(sql::CREATE_OWNER)?;
        ch.run_sql(sql::INSERT_OWNER)?;
        eprintln!("created {DOTS_DB} with its _dotgrid_owner marker");
    }
    ch.run_sql(&sql::create_cells(t))?;
    let key = ch.run(&sql::partition_key(t), &[])?;
    if key.trim() != "epoch" {
        return Err(refuse(
            3,
            format!(
                "{DOTS_DB}.{t} is partitioned by {:?}, not epoch (made by an earlier dotgrid-build), so an epoch \
                 cannot be swapped in atomically: DROP TABLE {DOTS_DB}.{t} by hand and reload",
                key.trim()
            ),
        ));
    }
    let before = ch.count(&sql::count_epoch(t, epoch))?;
    let result = load_epoch(ch, o, &d, disk, started);
    match &result {
        Ok(_) => eprintln!("{t}: epoch {epoch} replaced ({before} rows before)"),
        Err(e) => {
            eprintln!("load failed ({e:#}); epoch {epoch} in {DOTS_DB}.{t} left as it was ({before} rows); dropping the scratch tables");
            let cleanup = [
                ch.run_sql(&sql::drop_stage(t)).map(|_| ()),
                ch.run_sql(&sql::drop_table(&format!("{t}_load"))).map(|_| ()),
                ch.run_sql(&sql::drop_table(&format!("{t}_check"))).map(|_| ()),
            ];
            for c in cleanup.into_iter().filter_map(Result::err) {
                eprintln!("cleanup also failed: {c:#}");
            }
        }
    }
    result
}

/// Builds and checks the epoch in `<t>_load`, then swaps it into `<t>`.
/// Any error before the swap leaves `<t>` untouched.
fn load_epoch(ch: &Ch, o: &LoadOptions, d: &GridDescriptor, disk: Value, started: Instant) -> Result<Value> {
    let (t, epoch) = (o.table.as_str(), o.build.epoch);
    let load = format!("{t}_load");
    ch.run_sql(&sql::drop_stage(t))?;
    ch.run_sql(&sql::drop_table(&load))?;
    ch.run_sql(&sql::create_stage(t))?;
    ch.run_sql(&sql::create_cells(&load))?;

    // Stream the converter's pyramid into the stage table.
    let stream = RowStream {
        ch,
        insert: sql::insert_level0(t),
        epoch,
        levels: if o.sql_levels { 1 } else { u8::MAX },
        buf: Mutex::new((Vec::new(), 0)),
        rows_per_level: Mutex::new(Vec::new()),
    };
    let t_stream = Instant::now();
    let sink = |level: u8, tile: TileId, w: &grid::CellWindow, cells: &[(u32, f64)]| stream.push(level, tile, w, cells);
    let report = crate::build_with(&o.build, Some(&sink))?;
    stream.finish()?;
    let stream_s = t_stream.elapsed().as_secs_f64();
    let rows_per_level = stream.rows_per_level.into_inner().unwrap();
    eprintln!("streamed {} rows in {stream_s:.1} s: {rows_per_level:?}", rows_per_level.iter().sum::<u64>());

    let t_fill = Instant::now();
    ch.run(&sql::fill_h3(t, epoch, d.base_arcsec, d.origin_lat), &[])?;
    ch.run_sql(&sql::drop_stage(t))?;
    let fill_s = t_fill.elapsed().as_secs_f64();

    // The 3″ path: levels 1.. aggregated in SQL, each from the one below.
    let mut sql_levels_s = Vec::new();
    if o.sql_levels {
        for from in 0..d.levels - 1 {
            let t_level = Instant::now();
            ch.run(&sql::insert_parent_level(&load, epoch, from, d)?, &PARENT_LEVEL_SETTINGS)?;
            sql_levels_s.push(t_level.elapsed().as_secs_f64());
            eprintln!("level {} in SQL: {:.1} s", from + 1, sql_levels_s.last().unwrap());
        }
    }

    // MG-13 on `_load`: the round trip, then h3o on 1,000 sampled rows.
    let gate = ch.counts(&sql::h3_round_trip_gate(&load, epoch, d.base_arcsec, d.origin_lat))?;
    let [gate_checked, gate_rows] = gate[..] else { bail!("gate returned {gate:?}") };
    let want_level0 = report["cells_per_level"][0].as_u64().context("report has no level-0 cell count")?;
    let mut sampled = 0u32;
    let mut disagree = Vec::new();
    for line in ch.run(&sql::h3_sample(&load, epoch), &[])?.lines() {
        let f: Vec<u64> = line.split('\t').map(|x| x.parse()).collect::<Result<_, _>>()?;
        let [row, col, h3] = f[..] else { bail!("h3 sample row {line:?}") };
        sampled += 1;
        let want = h3o_r10(d, row as u32, col as u32)?;
        if want != h3 {
            disagree.push(json!({"row": row, "col": col, "clickhouse": format!("{h3:x}"), "h3o": format!("{want:x}")}));
        }
    }
    eprintln!("MG-13: gate checked {gate_checked} level-0 rows (converter {want_level0}), {gate_rows} over 150 m; h3o agrees on {}/{sampled}", sampled as usize - disagree.len());

    // Per-level sums against the converter's own.
    let mut sums = Vec::new();
    let mut sum_bad = Vec::new();
    for line in ch.run(&sql::level_sums(&load, epoch), &[])?.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [level, rows, sum] = f[..] else { bail!("level sums row {line:?}") };
        let (level, rows, sum): (usize, u64, f64) = (level.parse()?, rows.parse()?, sum.parse()?);
        let want = report["sum_per_level"][level].as_f64().unwrap_or(f64::NAN);
        let want_enc = report["sum_encoded_per_level"][level].as_f64().unwrap_or(f64::NAN);
        let want_cells = report["cells_per_level"][level].as_u64().unwrap_or(u64::MAX);
        let rel = (sum - want) / want;
        let rel_enc = (sum - want_enc) / want_enc;
        // The archive rounds every cell to 0.1 (and drops those under
        // 0.05): up to 0.05 per row apart. At 3″ that is 2.6e-4 of the
        // world at level 0 (Task 12), past any fixed relative bound.
        let enc_bound = 0.05 * rows as f64 + 1e-6 * want_enc.abs();
        eprintln!("level {level}: {rows} rows ({want_cells} cells), sum {sum:.1} vs converter {want:.1} ({rel:+.2e}), archive {want_enc:.1} ({rel_enc:+.2e}, bound {enc_bound:.1})");
        if rel.abs() > 1e-4 || (sum - want_enc).abs() > enc_bound || rows != want_cells {
            sum_bad.push(level);
        }
        sums.push(json!({"level": level, "rows": rows, "sum": sum, "converter_sum": want, "archive_sum": want_enc,
                         "rel": rel, "rel_archive": rel_enc}));
    }
    if sums.len() != d.levels as usize {
        sum_bad.push(usize::MAX);
    }

    // The SQL parent aggregation (the 3″ path) against the converter's
    // level 1, when the converter's level 1 is in the table. On the 3″
    // path level 1 IS the SQL's, checked above by its row count and sum.
    let (mut parent_rows, mut parent_mismatches) = (None, 0);
    if !o.sql_levels {
        let check = format!("{t}_check");
        ch.run_sql(&sql::drop_table(&check))?;
        ch.run_sql(&sql::create_cells(&check))?;
        ch.run(&sql::insert_parent_level_into(&load, &check, epoch, 0, d.max_data_zoom, d.base_arcsec, d.origin_lat)?, &[])?;
        parent_rows = Some(ch.count(&sql::count_epoch(&check, epoch))?);
        parent_mismatches = ch.count(&sql::level_mismatches(&load, &check, epoch, 1))?;
        ch.run_sql(&sql::drop_table(&check))?;
        eprintln!("level 1 in SQL: {parent_rows:?} rows, {parent_mismatches} differ from the converter's");
    }

    let mut failed = Vec::new();
    if gate_rows != 0 || gate_checked != want_level0 {
        failed.push(format!("MG-13 gate: {gate_rows} rows over 150 m, {gate_checked} checked of {want_level0}"));
    }
    if sampled != 1000 || !disagree.is_empty() {
        failed.push(format!("MG-13 h3o: {} of {sampled} disagree", disagree.len()));
    }
    if !sum_bad.is_empty() {
        failed.push(format!("sums off at levels {sum_bad:?}"));
    }
    if parent_mismatches != 0 {
        failed.push(format!("SQL level 1: {parent_mismatches} rows differ"));
    }
    if !failed.is_empty() {
        return Err(refuse(5, failed.join("; ")));
    }

    // Every check passed: swap the epoch in, in one step.
    let rows_loaded = ch.count(&sql::count_epoch(&load, epoch))?;
    ch.run_sql(&sql::replace_epoch(t, epoch))?;
    ch.run_sql(&sql::drop_table(&load))?;
    let rows_now = ch.count(&sql::count_epoch(t, epoch))?;
    if rows_now != rows_loaded {
        bail!("after REPLACE PARTITION {t} holds {rows_now} rows of epoch {epoch}, the load had {rows_loaded}");
    }

    // MG-12.
    let parts = ch.run(&sql::parts_bytes(t), &[])?;
    let p: Vec<&str> = parts.trim().split('\t').collect();
    let [rows, bytes, per_row] = p[..] else { bail!("system.parts row {parts:?}") };
    let (rows, bytes, per_row): (u64, u64, f64) = (rows.parse()?, bytes.parse()?, per_row.parse()?);
    eprintln!("MG-12: {rows} rows, {bytes} B on disk, {per_row:.3} B per row (whole table, active parts)");

    let out = json!({
        "table": format!("{DOTS_DB}.{t}"),
        "epoch": epoch,
        "disk": disk,
        "rows_streamed_per_level": rows_per_level,
        "stream_s": stream_s,
        "fill_h3_s": fill_s,
        "levels_from": if o.sql_levels { "sql" } else { "stream" },
        "sql_levels_s": sql_levels_s,
        "mg13_gate_rows_checked": gate_checked,
        "mg13_gate_rows": gate_rows,
        "mg13_sampled": sampled,
        "mg13_h3o_disagree": disagree,
        "level_sums": sums,
        "sql_level1_rows": parent_rows,
        "sql_level1_mismatches": parent_mismatches,
        "mg12_rows": rows,
        "mg12_bytes_on_disk": bytes,
        "mg12_bytes_per_row": per_row,
        "converter": report,
        "wall_s": started.elapsed().as_secs_f64(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
        let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
        let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
        2.0 * 6_371_008.8 * h.sqrt().asin()
    }

    /// A scratch database this test run created; dropped on drop, so a
    /// panicking assertion still cleans up. Never adopts an existing one.
    struct ScratchDb<'a> {
        ch: &'a Ch,
        name: &'static str,
    }

    impl<'a> ScratchDb<'a> {
        fn create(ch: &'a Ch, name: &'static str) -> Self {
            assert_eq!(
                ch.count(&sql::database_exists(name)).unwrap(),
                0,
                "{name} already exists and this test did not create it; not touching it"
            );
            ch.run_sql(&sql::scratch::create_db(name)).unwrap();
            ScratchDb { ch, name }
        }
    }

    impl Drop for ScratchDb<'_> {
        fn drop(&mut self) {
            if let Err(e) = self.ch.run_sql(&sql::scratch::drop_db(self.name)) {
                eprintln!("could not drop scratch database {}: {e:#}", self.name);
            }
        }
    }

    fn centre_of(ch: &Ch, sql: &str) -> (f64, f64) {
        let text = ch.run_sql(sql).unwrap();
        let t = text.trim().trim_start_matches('(').trim_end_matches(')');
        let (a, b) = t.split_once(',').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    }

    /// `h3ToGeo(geoToH3(…))` at `(lat, lon)`, arguments in the given order.
    fn round_trip(ch: &Ch, lat: f64, lon: f64, args: &str) -> (f64, f64) {
        centre_of(ch, &format!("SELECT h3ToGeo(geoToH3({args}, 10)) FROM (SELECT {lat} AS lat, {lon} AS lon) FORMAT TabSeparated"))
    }

    /// Final review I1: a `CLICKHOUSE_URL` carrying `user:SECRET@` or
    /// `password=SECRET` never reaches the loader's error (`reqwest::Error`
    /// prints the URL, password included).
    #[test]
    fn a_failed_statement_never_carries_the_url_credentials() {
        let ch = Ch::new("http://user:SECRET@127.0.0.1:9/?password=SECRET").unwrap();
        let err = ch.run_sql("SELECT 1").expect_err("nothing listens on port 9");
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("SECRET"), "a credential leaked: {text}");
        assert!(text.contains("127.0.0.1:9"), "the host is still named: {text}");
    }

    #[test]
    #[ignore = "needs the local ClickHouse; run with CLICKHOUSE_URL"]
    fn live_geo_to_h3_pins_dhaka_and_london() {
        let ch = Ch::from_env().unwrap();
        for (name, lat, lon) in [("Dhaka", 23.8103, 90.4125), ("London", 51.5074, -0.1278)] {
            let right = round_trip(&ch, lat, lon, "lat, lon");
            let d = haversine_m((lat, lon), right);
            println!("{name}: geoToH3(lat, lon, 10) -> h3ToGeo {right:?}, {d:.1} m away");
            assert!(d < 100.0, "{name}: {d} m");
            // The swapped call is still a valid cell, far away: the pitfall.
            let wrong = round_trip(&ch, lat, lon, "lon, lat");
            let dw = haversine_m((lat, lon), wrong);
            println!("{name}: geoToH3(lon, lat, 10) -> h3ToGeo {wrong:?}, {:.0} km away", dw / 1000.0);
            if name == "Dhaka" {
                assert!(dw > 5_000_000.0, "swapped Dhaka only {dw} m away");
            }
            // And h3o agrees with the server bit for bit.
            let server: u64 = ch
                .run_sql(&format!("SELECT geoToH3(lat, lon, 10) FROM (SELECT {lat} AS lat, {lon} AS lon)"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let ours = u64::from(h3o::LatLng::new(lat, lon).unwrap().to_cell(h3o::Resolution::Ten));
            assert_eq!(server, ours, "{name}");
        }
    }

    #[test]
    #[ignore = "needs the local ClickHouse"]
    fn the_loader_refuses_a_foreign_gpui_map_dots() {
        let ch = Ch::from_env().unwrap();
        // Only ever a database this run creates, named under gpui_map_dots_test_.
        let db = "gpui_map_dots_test_foreign";
        assert!(!check_owner(&ch, db).unwrap(), "absent: nothing to refuse");

        let scratch = ScratchDb::create(&ch, db);
        let err = check_owner(&ch, db).unwrap_err();
        let refusal = err.downcast_ref::<Refusal>().expect("a refusal, not an HTTP error");
        println!("refused: exit {} -- {}", refusal.code, refusal.msg);
        assert_eq!(refusal.code, 3);
        assert!(refusal.msg.starts_with(&format!("{db} exists and was not created by dotgrid-build")), "{}", refusal.msg);
        assert!(refusal.msg.contains(&format!("DROP DATABASE {db}")), "{}", refusal.msg);

        // With the marker it is ours.
        ch.run_sql(&sql::scratch::create_owner(db)).unwrap();
        assert!(check_owner(&ch, db).unwrap());
        drop(scratch);
        assert_eq!(ch.count(&sql::database_exists(db)).unwrap(), 0, "the scratch database is gone");
    }

    fn load_options(tiffs: Vec<std::path::PathBuf>, spill: &Path) -> LoadOptions {
        LoadOptions {
            build: Options {
                tiffs,
                tiff_list: None,
                delete_consumed: false,
                snap_to_lattice: true,
                arcsec: 30.0,
                epoch: 2025,
                out: spill.join("never-written.pmtiles"),
                spill_dir: spill.to_path_buf(),
                spill_cap: 1 << 20,
                resume: false,
                list_idle: crate::LIST_IDLE_DEADLINE,
                disk_floor_mb: 0,
            },
            table: "ghs_cells_30ss".into(),
            estimate_mb: ESTIMATE_30SS_MB,
            sql_levels: false,
        }
    }

    #[test]
    #[ignore = "needs the local ClickHouse and a loaded gpui_map_dots.ghs_cells_30ss epoch 2025"]
    fn a_missing_tiff_leaves_an_existing_epoch_untouched() {
        let ch = Ch::from_env().unwrap();
        let count = || ch.count(&sql::count_epoch("ghs_cells_30ss", 2025)).unwrap();
        let before = count();
        assert!(before > 0, "load epoch 2025 first (dotgrid-build --load-clickhouse --arcsec 30 --epoch 2025 …)");
        let tables = || ch.run_sql("SELECT name FROM system.tables WHERE database = 'gpui_map_dots' ORDER BY name").unwrap();
        let tables_before = tables();

        let dir = tempfile::tempdir().unwrap();
        let err = load(&ch, &load_options(vec![dir.path().join("missing.tif")], dir.path())).unwrap_err();
        println!("missing TIFF: {err:#}");
        assert!(format!("{err:#}").contains("missing.tif"), "{err:#}");
        assert_eq!(count(), before, "epoch 2025's rows changed");
        assert_eq!(tables(), tables_before, "tables changed");

        // A present but foreign TIFF (a 1° raster where 30″ is declared) is
        // refused by the header checks, before any write, too.
        let tif = dir.path().join("wrong_scale.tif");
        crate::tiff_stream::tests::write_geotiff(&tif, 360, 180, 1.0, (-180.0, 90.0), &vec![1.0; 360 * 180]);
        let err = load(&ch, &load_options(vec![tif], dir.path())).unwrap_err();
        println!("wrong scale: {err:#}");
        assert_eq!(count(), before, "epoch 2025's rows changed");
        assert_eq!(tables(), tables_before, "tables changed");
    }

    /// Test tables in `gpui_map_dots` (the loader's own database): refuses
    /// to start when any exists, drops all of them on drop, so a failed
    /// assertion still cleans up.
    struct TestTables<'a> {
        ch: &'a Ch,
        names: Vec<String>,
    }

    impl<'a> TestTables<'a> {
        fn claim(ch: &'a Ch, t: &str) -> Self {
            let names: Vec<String> = ["", "_stage", "_load", "_check"].iter().map(|s| format!("{t}{s}")).collect();
            for n in &names {
                let st = sql::Stmt::test("SELECT count() FROM system.tables WHERE database = 'gpui_map_dots' AND name = {t:String}")
                    .with_test("t", n);
                assert_eq!(ch.count(&st).unwrap(), 0, "gpui_map_dots.{n} exists and this test did not create it; not touching it");
            }
            TestTables { ch, names }
        }
    }

    impl Drop for TestTables<'_> {
        fn drop(&mut self) {
            for n in &self.names {
                let drop = if n.ends_with("_stage") { sql::drop_stage(n.trim_end_matches("_stage")) } else { sql::drop_table(n) };
                if let Err(e) = self.ch.run_sql(&drop) {
                    eprintln!("could not drop gpui_map_dots.{n}: {e:#}");
                }
            }
        }
    }

    /// Task 13 step 1: a 3″ synthetic 1°×1° raster (1200×1200, on GHSL's
    /// 1.5″-offset lattice, over Dhaka) loads with level 0 from the
    /// converter's stream and levels 1..=9 made by `insert_parent_level` in
    /// SQL; every level, tile and cell then matches the converter's archive.
    #[test]
    #[ignore = "needs the local ClickHouse with gpui_map_dots (made by a dotgrid-build load)"]
    fn insert_parent_level_matches_the_converter_pyramid() {
        use gpui_maps::overlay::dotgrid::{fetch_native, GridCountSource, GridRequest, GridTileKey};
        use std::collections::{BTreeMap, BTreeSet};

        let ch = Ch::from_env().unwrap();
        assert!(check_owner(&ch, DOTS_DB).unwrap(), "{DOTS_DB} must exist (a dotgrid-build load makes it)");
        let t = "ghs_cells_3ss_test";
        let _tables = TestTables::claim(&ch, t);
        ch.run_sql(&sql::create_cells(t)).unwrap();

        // Dense, sparse, sub-0.05 (dropped by the codec) and a
        // .x5-heavy tail, over 23..24°N 90..91°E.
        let (w, h) = (1200u32, 1200u32);
        let mut data = vec![0.0f64; (w * h) as usize];
        let mut s = 0x9e37_79b9u32;
        for (i, v) in data.iter_mut().enumerate() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let (r, c) = (i as u32 / w, i as u32 % w);
            let dense = (r / 100 + c / 100) % 3 == 0;
            *v = match (s >> 24) % 8 {
                0 | 1 if dense => ((s >> 8) % 3000) as f64 / 13.0,
                2 => ((s >> 8) % 5) as f64 / 100.0,
                3 if !dense => ((s >> 8) % 400) as f64 / 7.0,
                _ => 0.0,
            };
        }
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("dhaka_3s.tif");
        let tie = (90.0 + 1.5 / 3600.0, 24.0 - 1.5 / 3600.0);
        crate::tiff_stream::tests::write_geotiff(&tif, w, h, 3.0 / 3600.0, tie, &data);

        let mut o = load_options(vec![tif], dir.path());
        o.build.arcsec = 3.0;
        o.build.out = dir.path().join("converter.pmtiles");
        o.table = t.into();
        o.estimate_mb = 1;
        o.sql_levels = true;
        let d = crate::descriptor(3.0, 2025);
        let out = load_epoch(&ch, &o, &d, Value::Null, Instant::now()).unwrap();

        // Only level 0 crossed the wire; every level is in the table.
        let conv = &out["converter"];
        let cells: Vec<u64> = conv["cells_per_level"].as_array().unwrap().iter().map(|c| c.as_u64().unwrap()).collect();
        assert_eq!(cells.len(), 10);
        assert_eq!(out["rows_streamed_per_level"], json!([cells[0]]), "levels 1.. must come from SQL, not the stream");

        let source = fetch_native::open_pmtiles_grid(&o.build.out.to_string_lossy()).unwrap();
        let (mut exact, mut boundary, mut compared, mut drift) = (0u64, 0u64, 0u64, 0f64);
        const DRIFT: f64 = 1e-5;
        for level in 0..d.levels {
            let z = grid::data_zoom(&d, level);
            let st = sql::Stmt::test(format!(
                "SELECT tile, row, col, pop FROM gpui_map_dots.{t} WHERE epoch = 2025 AND level = {level} FORMAT TabSeparated"
            ));
            // tile -> (row, col) -> pop
            let mut ch_tiles: BTreeMap<u32, BTreeMap<(u32, u32), f32>> = BTreeMap::new();
            let mut rows = 0u64;
            for line in ch.run(&st, &[]).unwrap().lines() {
                let f: Vec<&str> = line.split('\t').collect();
                let (tile, row, col, pop): (u32, u32, u32, f32) =
                    (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap());
                assert!(ch_tiles.entry(tile).or_default().insert((row, col), pop).is_none(), "level {level}: duplicate cell");
                rows += 1;
            }
            assert_eq!(rows, cells[level as usize], "level {level}: rows vs the converter's cells");

            // The archive's tiles around every ClickHouse tile.
            let n = 1u32 << z;
            let mut keys = BTreeSet::new();
            for &k in ch_tiles.keys() {
                let (x, y) = (k >> 16, k & 0xffff);
                for dx in -1i64..=1 {
                    for dy in -1i64..=1 {
                        let (xx, yy) = (x as i64 + dx, y as i64 + dy);
                        if (0..n as i64).contains(&xx) && (0..n as i64).contains(&yy) {
                            keys.insert((xx as u32, yy as u32));
                        }
                    }
                }
            }
            let (mut arch_cells, mut arch_sum) = (0u64, 0f64);
            for (x, y) in keys {
                let tile = TileId { z, x, y };
                let key = GridTileKey { tile, level, epoch: 2025, epoch_b: None };
                let req = source.request(&[key]);
                let arch: BTreeMap<u32, f32> = if req == GridRequest::Absent {
                    BTreeMap::new()
                } else {
                    let body = fetch_native::execute(&req).unwrap();
                    source.decode(&[key], &body).unwrap().remove(0).1.cells.into_iter().map(|(i, p, _)| (i, p)).collect()
                };
                arch_cells += arch.len() as u64;
                arch_sum += arch.values().map(|&p| p as f64).sum::<f64>();
                let win = grid::cell_window(&d, level, tile);
                let empty = BTreeMap::new();
                let mine = ch_tiles.get(&((x << 16) | y)).unwrap_or(&empty);
                let mut seen = 0usize;
                let (mut sum_ch, mut sum_arch) = (0f64, 0f64);
                for (&(row, col), &pop) in mine {
                    assert!(
                        row >= win.row0 && row < win.row0 + win.rows && col >= win.col0 && col < win.col0 + win.cols,
                        "level {level}: cell ({row}, {col}) keyed to tile {tile:?} lies outside its window {win:?}"
                    );
                    let deci = (pop as f64 * 10.0).round();
                    let got = arch.get(&grid::local_index(&win, row, col)).copied();
                    compared += 1;
                    sum_ch += pop as f64;
                    match got {
                        Some(a) => {
                            seen += 1;
                            sum_arch += a as f64;
                            // The archive holds round(10 v) / 10 of the
                            // converter's f32 value; the SQL sums the same
                            // f32 children in another order, one f32
                            // rounding per level: 0.05 plus a float drift.
                            let off = (pop as f64 - a as f64).abs();
                            drift = drift.max((off - 0.05).max(0.0) / (pop as f64).max(1.0));
                            assert!(
                                off <= 0.05 + DRIFT * (pop as f64).max(1.0),
                                "level {level} tile {tile:?} ({row}, {col}): {pop} vs archive {a}"
                            );
                            // `a` is an f32 of a deci-count: compare in
                            // its own precision.
                            if (a as f64 * 10.0 - deci).abs() < 1e-3 + 1e-6 * deci {
                                exact += 1;
                            } else {
                                // Only a value on a .x5 edge may round the
                                // other way.
                                let edge = ((pop as f64 * 10.0).fract() - 0.5).abs();
                                assert!(
                                    edge <= 10.0 * DRIFT * (pop as f64).max(1.0),
                                    "level {level} tile {tile:?} ({row}, {col}): {pop} rounds away from archive {a}"
                                );
                                boundary += 1;
                            }
                        }
                        None => {
                            assert!(
                                pop as f64 <= 0.05 + DRIFT,
                                "level {level} tile {tile:?} ({row}, {col}): {pop} missing from the archive"
                            );
                            if deci > 0.0 {
                                boundary += 1;
                            }
                        }
                    }
                }
                assert_eq!(seen, arch.len(), "level {level} tile {tile:?}: the archive holds cells ClickHouse does not");
                assert!(
                    (sum_ch - sum_arch).abs() <= 0.05 * mine.len() as f64 + 1e-6 * sum_ch,
                    "level {level} tile {tile:?}: {sum_ch} vs archive {sum_arch}"
                );
            }
            let want_cells = conv["cells_encoded_per_level"][level as usize].as_u64().unwrap();
            let want_sum = conv["sum_encoded_per_level"][level as usize].as_f64().unwrap();
            assert_eq!(arch_cells, want_cells, "level {level}: archive tiles checked hold every encoded cell");
            assert!((arch_sum - want_sum).abs() <= 1e-6 * want_sum, "level {level}: {arch_sum} vs {want_sum}");
            println!("level {level} (z{z}): {rows} rows in {} tiles, archive {arch_cells} cells", ch_tiles.len());
        }
        println!("{compared} cells compared: {exact} exact, {boundary} a rounding step apart; worst drift past 0.05: {drift:.2e} of the value");
        assert_eq!(compared, cells.iter().sum::<u64>(), "every table row was compared");
    }

    #[test]
    #[ignore = "needs the local ClickHouse"]
    fn a_server_error_is_an_error_not_a_body() {
        let ch = Ch::from_env().unwrap();
        let err = ch.run_sql("SELECT throwIf(1, 'boom')").unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
    }

    #[test]
    fn a_row_is_nineteen_little_endian_bytes() {
        let ch = Ch::new("http://127.0.0.1:9").unwrap();
        let s = RowStream {
            ch: &ch,
            insert: String::new(),
            epoch: 2025,
            levels: u8::MAX,
            buf: Mutex::new((Vec::new(), 0)),
            rows_per_level: Mutex::new(Vec::new()),
        };
        let w = grid::CellWindow { row0: 100, col0: 200, rows: 10, cols: 10 };
        s.push(2, TileId { z: 4, x: 3, y: 5 }, &w, &[(12, 1.5)]).unwrap();
        let (b, n) = s.buf.into_inner().unwrap();
        assert_eq!(n, 1);
        let mut want = Vec::new();
        want.extend_from_slice(&2025u16.to_le_bytes());
        want.push(2);
        want.extend_from_slice(&((3u32 << 16) | 5).to_le_bytes());
        want.extend_from_slice(&101u32.to_le_bytes());
        want.extend_from_slice(&202u32.to_le_bytes());
        want.extend_from_slice(&1.5f32.to_le_bytes());
        assert_eq!(b, want);
        assert_eq!(b.len(), ROW_BYTES);
    }
}
