//! 2020 Census Demographic and Housing Characteristics File (DHC) reader,
//! table P9 only -- used solely by `compare-dhc` (MG-R1: is P.L. 94-171 P2
//! identical to DHC P9 at the block level, for DC? spec §2.1).
//!
//! # Verified against the real downloaded file (fix round 1)
//!
//! The first version of this module reasoned entirely from documentation
//! (the Census Bureau's DHC tech doc and its Table Matrix spreadsheet) and
//! was never run against a real DHC zip (ruling C23 at the time stopped the
//! implementer short of downloading one). That guesswork was half right and
//! half wrong. With `~/.cache/gpui-map-data/census2020/dhc/dc2020.dhc.zip`
//! now on disk, here's what direct inspection (`unzip -l`, and reading the
//! real `dcgeo2020.dhc` / `dc000052020.dhc` rows by hand) showed:
//!
//! - **Wrong:** the block-level `SUMLEV` is **`"100"`** (plain
//!   State-County-Tract-Block), not `"750"`. DHC has no voting-district
//!   compound levels the way the P.L. redistricting file does, so it never
//!   emits a `750` row at all -- every one of DC's 6,012 blocks is `SUMLEV
//!   100` in `dcgeo2020.dhc`, confirmed by `awk -F'|' '{print $3}' | sort |
//!   uniq -c`. This, not a filename or offset mistake, is why the first
//!   version of `compare-dhc` read exactly 0 blocks: it was filtering on a
//!   summary level that doesn't exist in this file.
//! - **Wrong:** `FILEID` is **`"DHCST"`**, not `"PLST"` (that guess was
//!   never actually checked in code -- this module had no `FILEID` guard at
//!   all, one of the review's findings).
//! - **Right:** the geoheader is still byte-for-byte `pl94::GeoFields`'s
//!   97-field layout. Field-by-field on the real `SUMLEV 040` (DC state)
//!   row: `SUMLEV`=`"040"`, `LOGRECNO`=`"0000001"`, `GEOCODE`(GEOID)=
//!   `"0400000US11"`, `POP100`=`"689545"`, `INTPTLAT`=`"+38.9042474"`,
//!   `INTPTLON`=`"-077.0165167"` -- all at the exact indices `GeoFields`
//!   already used. A block (`SUMLEV 100`) row's GEOID is
//!   `"1000000US110010001011000"`, same `<SUMLEV><GEOVAR><GEOCOMP>US<geocode>`
//!   shape as P.L.'s, so `geoid_suffix` needs no change either.
//! - **Right:** segment 5 (`dc000052020.dhc`), and `P9_OFFSET = 131` within
//!   it. Verified by reading the real state-row (`LOGRECNO 0000001`) record
//!   in that file: field 131 is `689545` (DC's total population, matching
//!   `POP100` exactly), field 132 `77652` (Hispanic), 133 `611893`
//!   (not-Hispanic subtotal, `689545-77652`), 134 `582408` (one-race
//!   subtotal), then 135..141 = `261771, 282066, 1277, 33192, 349, 3753,
//!   29485` (White, Black, AIAN, Asian, NHPI, Other, Two-or-more alone/
//!   combination cells) -- **these 8 sum to exactly 689,545**, the same
//!   identity `pl94::parse_parts` checks for P2. `P9_CELL_OFFSETS` (which
//!   cells within the table to keep) needed no change: the guess reusing
//!   `pl94`'s `P2`-relative offsets was correct.
//! - **Right:** both zip member names guessed by suffix,
//!   `dcgeo2020.dhc` and `dc000052020.dhc`, are exactly the real member
//!   names (`unzip -l` lists 44 segment files, `dc000012020.dhc` through
//!   `dc000442020.dhc`, plus `dcgeo2020.dhc`).
//!
//! So the fix here is two constants (`BLOCK_SUMLEV`, `EXPECTED_FILEID`) and
//! two guards this module was missing relative to `pl94.rs`: a `FILEID`
//! check on the first geoheader row, and a P9-groups-sum-to-P9-total check
//! per block (mirroring `pl94::parse_parts`'s `P2_001N` check) -- both are
//! exactly the kind of self-consistency check that would have caught a
//! wrong offset guess immediately instead of silently returning 0 rows.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::pl94::{BlockCounts, GeoFields};

const SEG_LEADING_FIELDS: usize = 5;
const SEG_LOGRECNO: usize = 4;

/// The DHC state file's `FILEID` (verified against the real
/// `dcgeo2020.dhc`; see module docs), the DHC analogue of P.L.'s `"PLST"`.
const EXPECTED_FILEID: &str = "DHCST";

/// DHC has no voting-district compound levels, so a block is plain
/// State-County-Tract-Block, `SUMLEV "100"` -- NOT `"750"` (that was P.L.'s
/// code, wrongly reused here before this fix; see module docs).
const BLOCK_SUMLEV: &str = "100";

/// Cells before P9 in segment 5: P1(1) + P2(4) + P3(8) + P4(3) + P5(17) +
/// P6(7) + P7(15) + P8(71) = 126 (Bureau's DHC Table Matrix spreadsheet),
/// now also confirmed by reading the real segment-5 state row (module
/// docs): field 131 is DC's total population.
const P9_OFFSET: usize = SEG_LEADING_FIELDS + 126;

/// Mirrors `pl94::P2_CELL_OFFSETS` (P9_001N total, P9_002N Hispanic,
/// P9_005N..P9_011N the 6 alone categories and "two or more"). Confirmed
/// against the real segment-5 state row: these 9 cells are
/// `[689545, 77652, 261771, 282066, 1277, 33192, 349, 3753, 29485]`, and
/// the 8 non-total cells sum to the total exactly.
const P9_CELL_OFFSETS: [usize; 9] = [0, 1, 4, 5, 6, 7, 8, 9, 10];

fn geoid_suffix(field: &str) -> Result<&str> {
    let pos = field
        .find("US")
        .ok_or_else(|| anyhow!("GEOID field has no \"US\" delimiter: {field:?}"))?;
    Ok(&field[pos + 2..])
}

fn parse_u32(field: &str, what: &str) -> Result<u32> {
    field
        .trim()
        .parse()
        .with_context(|| format!("{what}: {field:?} is not a u32"))
}

/// Parses a DHC geoheader (`geo`) and segment-5 (`seg5`) pair into one
/// `BlockCounts` per block (`SUMLEV 100`) row, keyed by the same 8-group P9
/// cells `pl94::parse_parts` keeps for P2. Like `pl94::parse_parts`, checks
/// `FILEID` on the first row and that the 8 groups sum to the P9 total for
/// every block, erroring loudly (naming the GEOID) on a mismatch.
pub fn parse_blocks(geo: &str, seg5: &str) -> Result<Vec<BlockCounts>> {
    let geo_lines: Vec<&str> = geo.lines().filter(|l| !l.is_empty()).collect();
    let seg_lines: Vec<&str> = seg5.lines().filter(|l| !l.is_empty()).collect();

    let first = geo_lines
        .first()
        .ok_or_else(|| anyhow!("DHC geoheader is empty"))?;
    let first_fields: Vec<&str> = first.split('|').collect();
    let fileid = *first_fields
        .get(GeoFields::FILEID)
        .ok_or_else(|| anyhow!("DHC geoheader row has no FILEID field"))?;
    if fileid != EXPECTED_FILEID {
        bail!("unexpected FILEID {fileid:?}, want {EXPECTED_FILEID:?} (2020 DHC state file)");
    }

    let mut seg_by_logrecno: std::collections::HashMap<u32, Vec<&str>> = std::collections::HashMap::new();
    for line in &seg_lines {
        let fields: Vec<&str> = line.split('|').collect();
        let logrecno: u32 = fields
            .get(SEG_LOGRECNO)
            .ok_or_else(|| anyhow!("segment-5 row has no LOGRECNO field"))?
            .trim()
            .parse()
            .context("segment-5 LOGRECNO is not a u32")?;
        seg_by_logrecno.insert(logrecno, fields);
    }

    let mut blocks = Vec::new();
    for line in &geo_lines {
        let fields: Vec<&str> = line.split('|').collect();
        let sumlev = *fields
            .get(GeoFields::SUMLEV)
            .ok_or_else(|| anyhow!("geoheader row has no SUMLEV field"))?;
        if sumlev != BLOCK_SUMLEV {
            continue;
        }
        let logrecno: u32 = fields
            .get(GeoFields::LOGRECNO)
            .ok_or_else(|| anyhow!("geoheader row has no LOGRECNO field"))?
            .trim()
            .parse()
            .context("geoheader LOGRECNO is not a u32")?;
        let geoid_field = *fields
            .get(GeoFields::GEOCODE)
            .ok_or_else(|| anyhow!("geoheader row has no GEOID field"))?;
        let suffix = geoid_suffix(geoid_field)?;
        let seg_fields = seg_by_logrecno
            .get(&logrecno)
            .ok_or_else(|| anyhow!("no segment-5 row for LOGRECNO {logrecno}"))?;

        let mut cells = [0u32; 9];
        for (i, &rel) in P9_CELL_OFFSETS.iter().enumerate() {
            let idx = P9_OFFSET + rel;
            let field = seg_fields.get(idx).ok_or_else(|| {
                anyhow!(
                    "segment-5 row has only {} fields, need P9 cell at index {idx}",
                    seg_fields.len()
                )
            })?;
            cells[i] = parse_u32(field, "P9 cell")?;
        }
        let total = cells[0];
        let c: [u32; 8] = cells[1..9].try_into().expect("9 cells, 8 after the total");
        let sum: u64 = c.iter().map(|&x| u64::from(x)).sum();
        if sum != u64::from(total) {
            bail!("block {suffix}: the 8 P9 groups sum to {sum}, but P9_001N is {total}");
        }

        let geoid: u64 = suffix
            .parse()
            .with_context(|| format!("block GEOID suffix {suffix:?} is not a u64"))?;
        let state: u8 = suffix
            .get(0..2)
            .ok_or_else(|| anyhow!("block GEOID suffix {suffix:?} too short for a state code"))?
            .parse()
            .context("state FIPS code is not a u8")?;
        let lat: f64 = fields
            .get(GeoFields::INTPTLAT)
            .ok_or_else(|| anyhow!("geoheader row has no INTPTLAT field"))?
            .trim()
            .parse()
            .context("INTPTLAT is not an f64")?;
        let lon: f64 = fields
            .get(GeoFields::INTPTLON)
            .ok_or_else(|| anyhow!("geoheader row has no INTPTLON field"))?
            .trim()
            .parse()
            .context("INTPTLON is not an f64")?;
        blocks.push(BlockCounts {
            geoid,
            state,
            c,
            intpt: (lat, lon),
        });
    }
    Ok(blocks)
}

fn read_zip_member_ending_with(
    archive: &mut zip::ZipArchive<std::fs::File>,
    suffix: &str,
) -> Result<String> {
    let name = (0..archive.len())
        .map(|i| archive.by_index(i).and_then(|f| f.name().map(|n| n.into_owned())))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|n| n.to_lowercase().ends_with(suffix))
        .ok_or_else(|| anyhow!("no zip entry ending with {suffix:?}"))?;
    let mut f = archive.by_name(&name)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Opens `zip_path` (`dc2020.dhc.zip`) and reads its geoheader and segment-5
/// members as streams. Member names (`geo2020.dhc`, `000052020.dhc`) are
/// confirmed against the real zip's member listing (module docs).
pub fn parse_dc(zip_path: &Path) -> Result<Vec<BlockCounts>> {
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("reading {} as a zip", zip_path.display()))?;
    let geo = read_zip_member_ending_with(&mut archive, "geo2020.dhc")
        .with_context(|| format!("{}: geoheader member", zip_path.display()))?;
    let seg5 = read_zip_member_ending_with(&mut archive, "000052020.dhc")
        .with_context(|| format!("{}: segment-5 member", zip_path.display()))?;
    parse_blocks(&geo, &seg5)
}
