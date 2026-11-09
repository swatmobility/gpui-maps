//! TIGER/Line 2023 block groups + ACS 2023 -> one `BGPK` v1 pack for the
//! census demo in a browser (`src/census/pack.rs`).
//!
//!     census-pack [--data DIR] --out FILE [--tolerance-m 5] [--states 44,11]
//!
//! Refuses (exit 3) when the output's disk has under 2,048 MB free. Reads
//! every state, simplifies them together with shared arcs (`topo`), then
//! prints `gate: MG-C3 unmatched_interior_edges=<n>`, one
//! `gate: MG-C1 <state> coords=<n> bytes=<n>` line per state, and the
//! totals.

mod read;
mod simplify;
mod topo;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use gpui_maps::census::pack::{self, EncodeState, PackRow};

/// Every FIPS the TIGER 2023 block-group release ships: the 50 states, DC,
/// Puerto Rico and the four island areas.
const ALL_STATES: [u8; 56] = [
    1, 2, 4, 5, 6, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
    35, 36, 37, 38, 39, 40, 41, 42, 44, 45, 46, 47, 48, 49, 50, 51, 53, 54, 55, 56, 60, 66, 69, 72, 78,
];

/// Arcs one ring uses in the national 5 m build of TIGER/ACS 2023
/// (2026-09-28): the outline and shore. See the `single_use_arcs` gate line.
const SINGLE_USE_ARCS_BASELINE: usize = 979;

/// The least free space, in MB, the converter writes into.
const MIN_FREE_MB: u64 = 2_048;

struct Args {
    data: PathBuf,
    out: PathBuf,
    tolerance_m: f32,
    states: Vec<u8>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut args = Args {
        data: home().join(".cache/gpui-map/demos/us-block-groups"),
        out: PathBuf::new(),
        tolerance_m: 5.0,
        states: ALL_STATES.to_vec(),
    };
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--data" => args.data = PathBuf::from(value()?),
            "--out" => args.out = PathBuf::from(value()?),
            "--tolerance-m" => args.tolerance_m = value()?.parse().context("--tolerance-m")?,
            "--states" => {
                args.states = value()?.split(',').map(|s| s.trim().parse().context("--states")).collect::<Result<_>>()?;
            }
            other => bail!("unknown argument {other}"),
        }
    }
    if args.out.as_os_str().is_empty() {
        bail!("--out FILE is required");
    }
    args.states.sort_unstable();
    args.states.dedup();
    Ok(args)
}

/// The "Available" column of `df -m`'s output, in MB.
fn df_available_mb(df_output: &str) -> Option<u64> {
    let mut lines = df_output.lines();
    let header: Vec<&str> = lines.next()?.split_whitespace().collect();
    let col = header.iter().position(|h| *h == "Available" || *h == "Avail")?;
    let row: Vec<&str> = lines.next()?.split_whitespace().collect();
    row.get(col)?.parse().ok()
}

/// Exit 3 when `dir`'s disk has under [`MIN_FREE_MB`] free.
fn disk_guard(dir: &Path) -> Result<Option<u64>> {
    let out = std::process::Command::new("df").arg("-m").arg(dir).output().context("running df -m")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let available = df_available_mb(&text).with_context(|| format!("could not read df -m's Available column:\n{text}"))?;
    println!("disk: {available} MB available at {} (the guard needs {MIN_FREE_MB} MB)", dir.display());
    Ok((available < MIN_FREE_MB).then_some(available))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("census-pack: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(argv: &[String]) -> Result<ExitCode> {
    let args = parse_args(argv)?;
    let out_dir = args.out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    if let Some(available) = disk_guard(out_dir)? {
        eprintln!("census-pack: refusing: {available} MB available, {MIN_FREE_MB} MB needed");
        return Ok(ExitCode::from(3));
    }
    let started = std::time::Instant::now();
    let income = read::read_table(&args.data.join("acs/income_bg_2023.csv"))?;
    let population = read::read_table(&args.data.join("acs/population_bg_2023.csv"))?;
    let gazetteer = args.data.join("names/2023_Gaz_counties_national.txt");
    let county_text = read::county_text(
        &std::fs::read_to_string(&gazetteer).with_context(|| format!("{}", gazetteer.display()))?,
    );
    let cbsa_path = args.data.join("cbsa/county_metro_2023.tsv");
    let cbsa_text = std::fs::read_to_string(&cbsa_path).with_context(|| format!("{}", cbsa_path.display()))?;

    let mut read_states: Vec<(u8, Vec<pack::Feature>)> = Vec::new();
    let mut rows: Vec<PackRow> = Vec::new();
    let mut counts = read::JoinCounts::default();
    let coords = |features: &[pack::Feature]| features.iter().flatten().flatten().map(Vec::len).sum::<usize>();
    let mut before_total = 0usize;
    for &fips in &args.states {
        let (features, state_rows, state_counts) = read::read_state(&args.data, fips, &income, &population)?;
        let before = coords(&features);
        println!(
            "state {fips:02}: {} block groups, {before} coordinates ({:.1} s)",
            features.len(),
            started.elapsed().as_secs_f64()
        );
        before_total += before;
        counts.add(state_counts);
        rows.extend(state_rows);
        read_states.push((fips, features));
    }
    // Nationally, so an arc on a state line is shared like any other.
    let topo = topo::simplify_national(&mut read_states, f64::from(args.tolerance_m));
    println!(
        "topology: {} unique vertices, {} junctions, {} arcs ({} used by one ring) holding {} points, {} collapsed rings ({:.1} s)",
        topo.unique_vertices,
        topo.junctions,
        topo.arcs,
        topo.single_use_arcs,
        topo.arc_points,
        topo.collapsed_rings,
        started.elapsed().as_secs_f64()
    );
    println!("gate: MG-C3 unmatched_interior_edges={}", topo.unmatched_interior_edges);
    // MG-C3 cannot see neighbours whose shared boundary is not vertex-
    // identical in the input: they become two single-use arcs, simplified
    // apart (a sliver). The national 5 m build's count is the baseline; a
    // rise means new such pairs (or new shore), worth a look.
    if args.states == ALL_STATES && args.tolerance_m == 5.0 {
        let flag = if topo.single_use_arcs > SINGLE_USE_ARCS_BASELINE { " RISE: check for mismatched neighbours" } else { "" };
        println!("gate: MG-C3 single_use_arcs={} (baseline {SINGLE_USE_ARCS_BASELINE}){flag}", topo.single_use_arcs);
    }
    let per_state_coords: Vec<(u8, usize)> = read_states.iter().map(|(fips, f)| (*fips, coords(f))).collect();
    let after_total: usize = per_state_coords.iter().map(|(_, n)| n).sum();
    let states: Vec<EncodeState> = read_states.into_iter().map(|(fips, features)| EncodeState { fips, features }).collect();
    println!(
        "income join: {} of {} block groups have an income; {} missing ({} no estimate, {} not in the ACS table)",
        counts.matched,
        counts.features,
        counts.no_estimate + counts.not_in_table,
        counts.no_estimate,
        counts.not_in_table
    );
    let bytes = pack::encode(&states, &rows, &county_text, &cbsa_text, args.tolerance_m);
    // Before anything is written: the pack decodes, as the page will, to
    // exactly the rings that went in.
    let stored = verify_round_trip(&bytes, &states)?;
    println!("round trip: every ring of {} states decodes point for point ({:.1} s)", states.len(), started.elapsed().as_secs_f64());
    std::fs::write(&args.out, &bytes).with_context(|| format!("writing {}", args.out.display()))?;
    let header = pack::parse_header(&bytes).context("re-reading the written header")?;
    for ((row, (fips, coords)), stored) in header.states.iter().zip(&per_state_coords).zip(&stored) {
        debug_assert_eq!(row.fips, *fips);
        println!("gate: MG-C1 {fips:02} coords={stored} ring_coords={coords} bytes={}", row.len);
    }
    println!("gate: MG-C1 tables bytes={} raw={}", header.tables_len, header.tables_raw_len);
    println!(
        "gate: MG-C1 total coords={} (stored, each arc once) ring_coords={after_total} bytes={} (from {before_total} coordinates, tolerance {} m) in {:.1} s -> {}",
        stored.iter().sum::<usize>(),
        bytes.len(),
        args.tolerance_m,
        started.elapsed().as_secs_f64(),
        args.out.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Decodes every state of `bytes` and compares it with what `states` gave
/// the encoder: the same features, each with the same polygons, each with
/// the same rings, point for point at the pack's quantum; and no section
/// (nor the tables) past the decoder's `MAX_SECTION_RAW`. Returns each
/// state's stored point count, in FIPS order.
fn verify_round_trip(bytes: &[u8], states: &[EncodeState]) -> Result<Vec<usize>> {
    let header = pack::parse_header(bytes).context("re-reading the header")?;
    let mut sorted: Vec<&EncodeState> = states.iter().collect();
    sorted.sort_by_key(|s| s.fips);
    if header.states.len() != sorted.len() {
        bail!("{} states written, {} in the directory", sorted.len(), header.states.len());
    }
    let largest = header.states.iter().map(|r| r.raw_len).chain([header.tables_raw_len]).max().unwrap_or(0);
    if largest > pack::MAX_SECTION_RAW {
        bail!("a section is {largest} B raw, past the decoder's {} B cap", pack::MAX_SECTION_RAW);
    }
    let q = |p: [f64; 2]| ((p[0] / pack::QUANTUM_DEG).round() as i64, (p[1] / pack::QUANTUM_DEG).round() as i64);
    let mut stored = Vec::with_capacity(sorted.len());
    for (row, state) in header.states.iter().zip(sorted) {
        let fips = row.fips;
        if fips != state.fips {
            bail!("directory row {fips:02} where state {:02} was written", state.fips);
        }
        let end = row.offset as usize + row.len as usize;
        let geo = pack::decode_state(row, &bytes[row.offset as usize..end]).with_context(|| format!("state {fips:02}"))?;
        if geo.features.len() != state.features.len() {
            bail!("state {fips:02}: {} features decoded, {} written", geo.features.len(), state.features.len());
        }
        for (f, (feature, &(p0, p1))) in state.features.iter().zip(&geo.features).enumerate() {
            if feature.len() != (p1 - p0) as usize {
                bail!("state {fips:02} feature {f}: the polygon count differs");
            }
            for (polygon, &(r0, r1)) in feature.iter().zip(&geo.polygons[p0 as usize..p1 as usize]) {
                if polygon.len() != (r1 - r0) as usize {
                    bail!("state {fips:02} feature {f}: a polygon's ring count differs");
                }
                for (ring, &(a, b)) in polygon.iter().zip(&geo.rings[r0 as usize..r1 as usize]) {
                    let got = &geo.coords[a as usize..b as usize];
                    if got.len() != ring.len() || got.iter().zip(ring).any(|(g, r)| q(*g) != q(*r)) {
                        bail!("state {fips:02} feature {f}: a ring does not decode to what was encoded");
                    }
                }
            }
        }
        stored.push(geo.stored_points);
    }
    println!("round trip: largest section {largest} B raw (cap {} B)", pack::MAX_SECTION_RAW);
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn the_disk_guard_parses_df() {
        let mac = "Filesystem     1M-blocks   Used Available Capacity iused      ifree %iused  Mounted on\n\
                   /dev/disk3s5      471482 428525     10095    98% 5342450  103381680    5%   /System/Volumes/Data\n";
        assert_eq!(df_available_mb(mac), Some(10_095));
        let linux = "Filesystem     1M-blocks  Used Avail Use% Mounted on\n/dev/sda1          50000 49000  1000  98% /\n";
        assert_eq!(df_available_mb(linux), Some(1_000));
        assert_eq!(df_available_mb(""), None);
        assert_eq!(df_available_mb("Filesystem Size\n/dev/x 1\n"), None);
    }

    #[test]
    fn args_default_to_every_state_at_five_metres() {
        let args = parse_args(&["--out".into(), "/tmp/x.bgpk".into()]).unwrap();
        assert_eq!(args.states.len(), 56);
        assert_eq!(args.tolerance_m, 5.0);
        let args = parse_args(&["--out".into(), "x".into(), "--states".into(), "44,11".into()]).unwrap();
        assert_eq!(args.states, vec![11, 44]);
        assert!(parse_args(&[]).is_err(), "--out is required");
    }

    /// MG-C4: every row of the written national pack against the CSVs it
    /// came from. Needs the real data and a pack built by `--out` at the
    /// default path (or `CENSUS_PACK`); `-- --ignored` to run.
    #[test]
    #[ignore]
    fn the_pack_round_trips_the_real_tables() {
        let data = home().join(".cache/gpui-map/demos/us-block-groups");
        let pack_path = std::env::var_os("CENSUS_PACK")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache/gpui-map-data/census-web/us_bg_2023.bgpk"));
        let bytes = std::fs::read(&pack_path).expect("the national pack");
        let header = pack::parse_header(&bytes).expect("header");
        let t = &bytes[header.tables_offset as usize..(header.tables_offset + u64::from(header.tables_len)) as usize];
        let tables = pack::decode_tables(&header, t).expect("tables");
        // The oracle is NOT the converter's parser: the CSVs read the way
        // the native demo's `join_income` reads them (`f64`, blank = none),
        // so a parse divergence (a decimal, a negative sentinel) shows.
        let native = |name: &str| -> HashMap<u64, Option<f64>> {
            std::fs::read_to_string(data.join("acs").join(name))
                .unwrap()
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let (geoid, value) = line.split_once(',')?;
                    Some((geoid.parse().ok()?, value.trim().parse().ok()))
                })
                .collect()
        };
        let income = native("income_bg_2023.csv");
        let population = native("population_bg_2023.csv");
        let mut matched = 0usize;
        for row in &tables.rows {
            let want_income = income.get(&row.geoid).copied().flatten();
            assert_eq!(row.income.map(f64::from), want_income, "income of {:012}", row.geoid);
            assert_eq!(
                row.population.map(f64::from),
                population.get(&row.geoid).copied().flatten(),
                "population of {:012}",
                row.geoid
            );
            matched += usize::from(want_income.is_some());
        }
        // Native's own line: `cargo run --release --example us_block_groups
        // -- --all --measure` printed `income join: 224390 of 242748 ...`
        // (docs/measurements/2026-09-28-census-web.md, MG-C4).
        assert_eq!((matched, tables.rows.len()), (224_390, 242_748), "the native income join's counts");
        let mut geoids: Vec<u64> = tables.rows.iter().map(|r| r.geoid).collect();
        geoids.sort_unstable();
        geoids.dedup();
        assert_eq!(geoids.len(), tables.rows.len(), "every GEOID once");
        // Each state's rows are that state's: its first two GEOID digits.
        for s in &header.states {
            let rows = &tables.rows[s.first_feature as usize..(s.first_feature + s.feature_count) as usize];
            assert!(rows.iter().all(|r| r.geoid / 10_000_000_000 == u64::from(s.fips)), "state {:02}", s.fips);
        }
        println!("gate: MG-C4 rows={} matched={matched}", tables.rows.len());
    }

    /// A tiny end-to-end: a written two-state pack re-reads.
    #[test]
    fn an_encoded_pack_decodes_with_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.bgpk");
        let sq = vec![[0.0, 0.0], [1e-3, 0.0], [1e-3, 1e-3], [0.0, 1e-3], [0.0, 0.0]];
        let states = vec![EncodeState { fips: 44, features: vec![vec![vec![sq.clone()]]] }];
        let rows = vec![PackRow { geoid: 440010301001, income: Some(5), population: None, aland_km2: 1.0 }];
        std::fs::write(&path, pack::encode(&states, &rows, "", "", 5.0)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(pack::parse_header(&bytes).unwrap().feature_count, 1);
        assert_eq!(verify_round_trip(&bytes, &states).unwrap(), vec![5]);
        let other = vec![EncodeState { fips: 44, features: vec![vec![vec![vec![[0.0, 0.0], [2e-3, 0.0], [0.0, 2e-3], [0.0, 0.0]]]]] }];
        assert!(verify_round_trip(&bytes, &other).is_err(), "a different ring is caught");
        // The same flat ring sequence and feature count, grouped differently
        // (the feature gains an empty second polygon): only a per-polygon
        // comparison sees it.
        let regrouped = vec![EncodeState { fips: 44, features: vec![vec![vec![sq.clone()], vec![]]] }];
        assert!(verify_round_trip(&bytes, &regrouped).is_err(), "a different grouping is caught");
    }
}
