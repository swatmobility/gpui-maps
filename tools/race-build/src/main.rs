//! `race-build`: 2020 Census P.L. 94-171 (and DHC, for MG-R1) reader and
//! validator -- Task 1 of `docs/superpowers/plans/2026-09-27-us-race-dot-
//! map.md`. Later tasks build the dot archive from what this validates.
//!
//! ```text
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- validate-pl [--cache DIR]
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- compare-dhc [--cache DIR]
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- build [--cache DIR] [--out FILE]
//!     [--states tx,dc,...] [--check-inside-blocks 100] [--check-inside-dots 10000]
//!     [--work DIR] [--keep-zips] [--max-pass-parts N] [--fresh] [--tiles-only]
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- verify-geoids [--cache DIR] [--work DIR]
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- load-clickhouse [--cache DIR] [--work DIR] [--finish-exchange]
//! cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-clusters [--cache DIR] [--work DIR] [--out FILE]
//! ```
//!
//! `verify-geoids` checks, from `parts.spill` and the P.L. zips alone, that
//! every block's P.L. interior point lies on its own parts (see `verify.rs`).
//!
//! `build` (Task 5, `build.rs`) streams the TIGER block polygons one state
//! at a time and writes `race-dots-2020.pmtiles`, `parts.spill` and
//! `blocks.points` next to `--out` (default
//! `~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles`). The
//! inside-the-block check (MG-R8, ruling C9) runs inside `build`, per state,
//! before each zip is deleted. It resumes from its checkpoint when rerun.
//!
//! `race-build` is its own workspace root (see its `Cargo.toml`'s doc
//! comment), so it is not a member of the root `gpui-maps` workspace --
//! `cargo run -p race-build` from the repo root does not resolve it
//! (`error: package ID specification \`race-build\` did not match any
//! packages`). `--manifest-path tools/race-build/Cargo.toml` (or `cd
//! tools/race-build` first) is required.
//!
//! `load-clickhouse` (Task 9, `clickhouse_load.rs`) streams `parts.spill`
//! into `gpui_map_dots.race_parts_2020` and aggregates `race_hex_2020` at
//! H3 r4..=r8 (`CLICKHOUSE_URL`, default `http://localhost:8123`). Exit 3:
//! a foreign `gpui_map_dots`; 4: not enough disk; 5: a failed check; 6: a
//! validated build only half swapped in -- `--finish-exchange` finishes it
//! from the kept `_load` tables.
//!
//! Data lives under `~/.cache/gpui-map-data/census2020/` (Global
//! Constraints: "Data never enters git"), fetched by
//! `scripts/fetch-census-2020.sh` into `pl/<abbr>2020.pl.zip` and
//! `dhc/dc2020.dhc.zip`. Nothing here downloads anything itself.

mod build;
mod clickhouse_load;
mod cluster_export;
mod dhc;
mod hex_export;
mod pl94;
mod pyramid;
mod spill;
mod sql;
mod tiger;
mod verify;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// USPS abbreviations for the 50 states + DC, in the order the Census PL
/// index lists them (spec §2.3; Puerto Rico is excluded, spec §2.1).
const STATE_ABBRS: [&str; 51] = [
    "al", "ak", "az", "ar", "ca", "co", "ct", "de", "dc", "fl", "ga", "hi", "id", "il", "in", "ia",
    "ks", "ky", "la", "me", "md", "ma", "mi", "mn", "ms", "mo", "mt", "ne", "nv", "nh", "nj", "nm",
    "ny", "nc", "nd", "oh", "ok", "or", "pa", "ri", "sc", "sd", "tn", "tx", "ut", "vt", "va", "wa",
    "wv", "wi", "wy",
];

fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GPUI_MAP_CENSUS_CACHE") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME not set");
    Path::new(&home).join(".cache/gpui-map-data/census2020")
}

fn validate_pl(cache: &Path) -> Result<()> {
    let pl_dir = cache.join("pl");
    let mut us_total: u64 = 0;
    let mut us_groups = [0u64; 8];
    let mut total_blocks: u64 = 0;
    let mut total_populated: u64 = 0;
    let mut any_mismatch = false;

    for abbr in STATE_ABBRS {
        let zip_path = pl_dir.join(format!("{abbr}2020.pl.zip"));
        if !zip_path.exists() {
            bail!(
                "{} is missing -- run scripts/fetch-census-2020.sh --pl first",
                zip_path.display()
            );
        }
        let counts = pl94::parse_state(&zip_path)
            .with_context(|| format!("parsing {}", zip_path.display()))?;

        let block_sum: u64 = counts
            .blocks
            .iter()
            .flat_map(|b| b.c.iter())
            .map(|&x| u64::from(x))
            .sum();
        let populated = counts.blocks.iter().filter(|b| b.c.iter().any(|&x| x != 0)).count();
        let state_total = u64::from(counts.state_row[0]);

        let ok = block_sum == state_total;
        if !ok {
            any_mismatch = true;
        }
        println!(
            "{abbr}: {} blocks, {populated} populated, block sum {block_sum} vs state P2_001N {state_total} [{}]",
            counts.blocks.len(),
            if ok { "OK" } else { "MISMATCH" }
        );

        total_blocks += counts.blocks.len() as u64;
        total_populated += populated as u64;
        for b in &counts.blocks {
            us_total += u64::from(b.c.iter().sum::<u32>());
            for (g, &v) in us_groups.iter_mut().zip(b.c.iter()) {
                *g += u64::from(v);
            }
        }
    }

    println!("---");
    println!("total blocks: {total_blocks}");
    println!("populated blocks: {total_populated}");
    println!("US sum: {us_total} (want {})", pl94::US_TOTAL);
    for (i, (&got, &want)) in us_groups.iter().zip(pl94::US_GROUP_TOTALS.iter()).enumerate() {
        println!(
            "group {i}: {got} (want {want}) [{}]",
            if got == want { "OK" } else { "MISMATCH" }
        );
    }
    if us_total != pl94::US_TOTAL || us_groups != pl94::US_GROUP_TOTALS {
        any_mismatch = true;
    }

    if any_mismatch {
        bail!("validate-pl: one or more checks failed, see above");
    }
    Ok(())
}

fn compare_dhc(cache: &Path) -> Result<()> {
    let pl_zip = cache.join("pl").join("dc2020.pl.zip");
    let dhc_zip = cache.join("dhc").join("dc2020.dhc.zip");
    if !pl_zip.exists() {
        bail!("{} is missing -- run scripts/fetch-census-2020.sh --pl first", pl_zip.display());
    }
    if !dhc_zip.exists() {
        bail!(
            "{} is missing -- run scripts/fetch-census-2020.sh --dhc-dc first",
            dhc_zip.display()
        );
    }

    let pl = pl94::parse_state(&pl_zip).with_context(|| format!("parsing {}", pl_zip.display()))?;
    let dhc_blocks =
        dhc::parse_dc(&dhc_zip).with_context(|| format!("parsing {}", dhc_zip.display()))?;

    // A join that silently matched nothing must fail louder than a join
    // that matched everything and found differences (fix round 1, HIGH
    // finding: the first version printed a completed "differ" verdict when
    // it had in fact compared 0 blocks against anything).
    if dhc_blocks.is_empty() {
        bail!(
            "compare-dhc: parsed 0 blocks from {} -- dhc.rs's layout assumptions are wrong, or the file changed (see its module doc comment)",
            dhc_zip.display()
        );
    }

    let dhc_by_geoid: std::collections::HashMap<u64, [u32; 8]> =
        dhc_blocks.iter().map(|b| (b.geoid, b.c)).collect();

    let total = pl.blocks.len();
    let mut differing = 0usize;
    let mut missing_in_dhc = 0usize;
    for b in &pl.blocks {
        match dhc_by_geoid.get(&b.geoid) {
            Some(dhc_c) if *dhc_c == b.c => {}
            Some(_) => differing += 1,
            None => missing_in_dhc += 1,
        }
    }

    let missing_pct = 100.0 * missing_in_dhc as f64 / total as f64;
    if missing_pct > 1.0 {
        bail!(
            "compare-dhc: {missing_in_dhc} of {total} P.L. blocks ({missing_pct:.1}%) are missing from the DHC file -- over the 1% guard, the join is almost certainly broken rather than the two products genuinely disagreeing that much"
        );
    }

    let compared = total - missing_in_dhc;
    println!(
        "DC: {total} P.L. blocks, {compared} compared against DHC P9, {differing} differ, {missing_in_dhc} missing from the DHC file"
    );
    if differing == 0 {
        println!("P.L. 94-171 P2 and DHC P9 are identical at the block level for DC.");
    } else {
        println!(
            "{differing} of {compared} compared blocks differ; the map stays on P2 (spec §2.1)."
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut cache = cache_dir();
    let mut cmd = None;
    let mut out = None;
    let mut work = None;
    let mut states = Vec::new();
    let mut inside_blocks = 100u32;
    let mut inside_dots = 10_000u32;
    let mut keep_zips = false;
    let mut fresh = false;
    let mut tiles_only = false;
    let mut finish_exchange = false;
    let mut max_pass_parts = 3_000_000u64;
    let mut i = 1;
    let value = |i: usize, flag: &str| -> Result<String> { args.get(i).cloned().with_context(|| format!("{flag} needs a value")) };
    while i < args.len() {
        match args[i].as_str() {
            "--cache" => {
                i += 1;
                cache = PathBuf::from(value(i, "--cache")?);
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(value(i, "--out")?));
            }
            "--work" => {
                i += 1;
                work = Some(PathBuf::from(value(i, "--work")?));
            }
            "--states" => {
                i += 1;
                states = value(i, "--states")?.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
            }
            "--check-inside-blocks" => {
                i += 1;
                inside_blocks = value(i, "--check-inside-blocks")?.parse()?;
            }
            "--check-inside-dots" => {
                i += 1;
                inside_dots = value(i, "--check-inside-dots")?.parse()?;
            }
            "--max-pass-parts" => {
                i += 1;
                max_pass_parts = value(i, "--max-pass-parts")?.parse()?;
            }
            "--keep-zips" => keep_zips = true,
            "--fresh" => fresh = true,
            "--tiles-only" => tiles_only = true,
            "--finish-exchange" => finish_exchange = true,
            other if cmd.is_none() => cmd = Some(other.to_string()),
            other => bail!("unrecognized argument {other:?}"),
        }
        i += 1;
    }

    match cmd.as_deref() {
        Some("validate-pl") => validate_pl(&cache),
        Some("compare-dhc") => compare_dhc(&cache),
        Some("verify-geoids") => {
            let dir = work.unwrap_or_else(|| cache.clone());
            let r = verify::verify_geoids(&dir.join("parts.spill"), &cache)?;
            let (mut off, mut missing, mut blocks) = (0u64, 0u64, 0u64);
            let (mut own, mut sh) = (verify::Tally::default(), verify::Tally::default());
            for st in r.values() {
                println!(
                    "{}: {} blocks | own: {} on a part, {} in the parts' box, {} off | one-record shift: {} on, {} box, {} off | missing {} {:?}",
                    st.abbr, st.blocks, st.own.on_part, st.own.in_box, st.own.off, st.shifted.on_part, st.shifted.in_box, st.shifted.off, st.missing, st.off_examples
                );
                println!("    off: {} on a neighbour record's parts, {} people in them, {} with <= 5 people", st.off_on_neighbour, st.off_people, st.off_le_5_people);
                off += st.own.off;
                missing += st.missing;
                blocks += st.blocks;
                own.on_part += st.own.on_part;
                own.in_box += st.own.in_box;
                sh.on_part += st.shifted.on_part;
                sh.in_box += st.shifted.in_box;
                sh.off += st.shifted.off;
            }
            println!("---");
            println!("{} states, {blocks} blocks: own {} on a part, {} in box, {off} off; shifted {} on, {} box, {} off; missing {missing}", r.len(), own.on_part, own.in_box, sh.on_part, sh.in_box, sh.off);
            std::fs::write(dir.join("race-build.verify-geoids.json"), serde_json::to_vec_pretty(&r)?)?;
            // "Off" blocks are expected: the spill keeps only triangles with
            // people, so a small block's interior point can sit in a dropped
            // one. A pairing error instead puts the point on a neighbouring
            // record's parts, and moves nearly every block off (see the
            // shifted column). Fail on those signatures, not on "off" alone.
            let on_neighbour: u64 = r.values().map(|s| s.off_on_neighbour).sum();
            if missing > 0 || on_neighbour > 0 || off * 10 > sh.off {
                bail!("verify-geoids: {missing} missing, {on_neighbour} interior points on a neighbour record's parts, {off} off (shifted pairing: {})", sh.off);
            }
            println!("verify-geoids: PASS -- 0 missing, 0 on a neighbour record's parts; {off} off ({:.2}%) against {} ({:.2}%) under a one-record shift", 100.0 * off as f64 / blocks as f64, sh.off, 100.0 * sh.off as f64 / blocks as f64);
            Ok(())
        }
        Some("load-clickhouse") => {
            let dir = work.unwrap_or_else(|| cache.clone());
            let run = if finish_exchange {
                clickhouse_load::Ch::from_env().and_then(|ch| clickhouse_load::finish(&ch))
            } else {
                clickhouse_load::load(&dir.join("parts.spill"))
            };
            match run {
                Ok(report) => {
                    let text = serde_json::to_string_pretty(&report)?;
                    if !finish_exchange {
                        std::fs::write(dir.join("race-build.clickhouse.json"), &text)?;
                    }
                    println!("{text}");
                    Ok(())
                }
                Err(e) => match e.downcast_ref::<clickhouse_load::Refusal>() {
                    Some(r) => {
                        eprintln!("load-clickhouse: {}", r.msg);
                        std::process::exit(r.code);
                    }
                    None => Err(e),
                },
            }
        }
        Some("export-hex") => {
            // Task 10: race_hex_2020 -> race-hex-2020.pmtiles (read-only on
            // ClickHouse; CLICKHOUSE_URL, default localhost:8123).
            let out = out.unwrap_or_else(|| cache.join("race-hex-2020.pmtiles"));
            let run = clickhouse_load::Ch::from_env().and_then(|ch| hex_export::export(&ch, &out));
            match run {
                Ok(report) => {
                    let text = serde_json::to_string_pretty(&report)?;
                    std::fs::write(cache.join("race-build.hex.json"), &text)?;
                    println!("{text}");
                    Ok(())
                }
                Err(e) => match e.downcast_ref::<clickhouse_load::Refusal>() {
                    Some(r) => {
                        eprintln!("export-hex: {}", r.msg);
                        std::process::exit(r.code);
                    }
                    None => Err(e),
                },
            }
        }
        Some("export-clusters") => {
            // Task 12: blocks.points -> race-clusters-2020.pmtiles (the
            // donut view's offline Supercluster hierarchy, z3..=z11).
            let out = out.unwrap_or_else(|| cache.join("race-clusters-2020.pmtiles"));
            let dir = work.unwrap_or_else(|| cache.clone());
            match cluster_export::export(&dir, &out) {
                Ok(report) => {
                    let text = serde_json::to_string_pretty(&report)?;
                    std::fs::write(cache.join("race-build.clusters.json"), &text)?;
                    println!("{text}");
                    // MG-R13's gates fail the run, not only the report.
                    let failures: Vec<&str> = report["budget_failures"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).collect();
                    if !failures.is_empty() {
                        eprintln!("export-clusters: STOP -- {} (nothing written to {})", failures.join("; "), out.display());
                        std::process::exit(5);
                    }
                    Ok(())
                }
                Err(e) => match e.downcast_ref::<clickhouse_load::Refusal>() {
                    Some(r) => {
                        eprintln!("export-clusters: {}", r.msg);
                        std::process::exit(r.code);
                    }
                    None => Err(e),
                },
            }
        }
        Some("check-state") => {
            // Review F1 follow-up: one state's full build-time interior-point
            // check (needs its TIGER zip, --out names it), into --work.
            let zip = out.context("check-state needs --out <TIGER zip>")?;
            let dir = work.context("check-state needs --work DIR")?;
            let abbr = states.first().context("check-state needs --states XX")?.clone();
            let fips = build::STATES.iter().find(|s| s.0 == abbr).context("unknown state")?.1;
            std::fs::create_dir_all(&dir)?;
            let mut parts = std::io::BufWriter::new(std::fs::File::create(dir.join("parts.spill"))?);
            let mut points = std::io::BufWriter::new(std::fs::File::create(dir.join("blocks.points"))?);
            let mut hist = vec![0u64; pyramid::BANDS];
            let io = build::StateIo { parts: &mut parts, points: &mut points, hist: &mut hist };
            let st = build::process_state(fips, &cache.join("pl").join(format!("{abbr}2020.pl.zip")), &zip, io, 0, 0)?;
            drop((parts, points));
            println!("{abbr}: {} populated, intpt off the TIGER polygon (> {} m): {} {:?}", st.populated, build::SIMPLIFY_M, st.intpt_outside, st.intpt_outside_examples);
            let r = verify::verify_geoids(&dir.join("parts.spill"), &cache)?;
            for st in r.values() {
                println!("{}: verify-geoids on the same spill: {} on a part, {} box, {} off", st.abbr, st.own.on_part, st.own.in_box, st.own.off);
            }
            Ok(())
        }
        Some("build") => {
            let out = out.unwrap_or_else(|| cache.join("race-dots-2020.pmtiles"));
            build::build(&build::BuildOpts { cache, out, work, states, inside_blocks, inside_dots, keep_zips, max_pass_parts, fresh, tiles_only })
        }
        _ => {
            eprintln!(
                "usage: cargo run --release --manifest-path tools/race-build/Cargo.toml -- <validate-pl|compare-dhc|verify-geoids|build|load-clickhouse|export-hex|export-clusters> [--cache DIR] [build flags, see main.rs]"
            );
            std::process::exit(2);
        }
    }
}
