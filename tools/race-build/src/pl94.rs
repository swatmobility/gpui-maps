//! 2020 Census State Redistricting Data (Public Law 94-171) Summary File --
//! the legacy bulk format, one `.pl.zip` per state (+ DC + PR), pipe
//! delimited, no API key needed (spec §2.1/§2.3, this plan's Task 1).
//!
//! Field positions below were checked 2026-09-27 against the Census
//! Bureau's own technical documentation for this exact product (not
//! reconstructed from a 2010-vintage memory of the format, which used fixed
//! width for the geoheader -- 2020 changed it to pipe-delimited for
//! everything):
//! <https://www2.census.gov/programs-surveys/decennial/2020/technical-documentation/complete-tech-docs/summary-file/2020Census_PL94_171Redistricting_StatesTechDoc_English.pdf>
//! ("Figure 2-3. File Set Structure Schematic", "Figure 2-4. Geographic
//! Header Record" and its endnotes 1, 8, 9; "P1. RACE \[71\]" and
//! "P2. HISPANIC OR LATINO, AND NOT HISPANIC OR LATINO BY RACE \[73\]" in
//! the Data Dictionary chapter). The fetched PDF is not committed; see
//! `docs/measurements/2026-09-27-us-race-dot-map.md` for how it was read.
//!
//! `dhc.rs` reads the DHC file's geoheader with the same `GeoFields`,
//! because the geoheader "is standard across all electronic data products
//! from the 2020 Census" (both tech docs say so in the same words).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

/// Field positions (0-indexed) in the pipe-delimited geographic header
/// record. The record has 97 fields total (`FILEID` through `UGA`); we only
/// ever need six of them. Confirmed against Figure 2-4 (see module docs):
/// the "Record Codes" block runs `FILEID` 0, `STUSAB` 1, `SUMLEV` 2,
/// `GEOVAR` 3, `GEOCOMP` 4, `CHARITER` 5, `CIFSN` 6, `LOGRECNO` 7; then
/// "Geographic Area Codes" starts at `GEOID` 8, `GEOCODE` 9, and runs on
/// (through `REGION`, `DIVISION`, `STATE`, county/place/tract/block/AIANNH/
/// CBSA/legislative-district fields, ...) to `PUMA` at 83; then "Area
/// Characteristics" gives `AREALAND` 84, `AREAWATR` 85, `BASENAME` 86,
/// `NAME` 87, `FUNCSTAT` 88, `GCUNI` 89, `POP100` 90, `HU100` 91,
/// `INTPTLAT` 92, `INTPTLON` 93, `LSADC` 94, `PARTFLAG` 95, then `UGA` 96.
pub struct GeoFields;

impl GeoFields {
    pub const FILEID: usize = 0;
    pub const SUMLEV: usize = 2;
    pub const LOGRECNO: usize = 7;
    /// The tech doc's *GEOID* column (its endnote 8: `SUMLEV + GEOVAR +
    /// GEOCOMP + "US" + geocodes`, e.g. `"7500000US110010001011000"`), not
    /// its *GEOCODE* column (endnote 9, one field over, which omits the
    /// `SUMLEV`/`GEOVAR`/`GEOCOMP`/`"US"` prefix). This constant is named
    /// `GEOCODE` because that's the field name the plan's brief and its
    /// literal test fixture use, and because it's the field that actually
    /// carries a `<prefix>US<geocodes>` string a block GEOID is parsed out
    /// of -- the tech doc's own `GEOCODE` field would not parse that way.
    pub const GEOCODE: usize = 8;
    /// Part of the brief's required `GEO_FIELDS`, kept for completeness and
    /// for anyone cross-checking a block's `POP100` against its P2 sum by
    /// hand; `parse_parts` itself validates against `P2_001N` instead
    /// (ruling C4), so this position is not read anywhere yet.
    #[allow(dead_code)]
    pub const POP100: usize = 90;
    pub const INTPTLAT: usize = 92;
    pub const INTPTLON: usize = 93;
}

/// Leading identification fields common to every data segment record
/// (Figure 2-3): `FILEID`, `STUSAB`, `CHARITER`, `CIFSN`, `LOGRECNO`.
const SEG_LEADING_FIELDS: usize = 5;
const SEG_LOGRECNO: usize = 4;

/// Offset of `P2_001N` in segment 1's pipe-delimited row: the 5 leading
/// fields, then table P1 ("P1. RACE \[71\]"), then P2 starts.
pub const SEG1_P2_OFFSET: usize = SEG_LEADING_FIELDS + 71;

/// Offsets of the 9 P2 cells this crate keeps, relative to
/// `SEG1_P2_OFFSET`: `P2_001N` (the total, kept only to validate the group
/// sum), `P2_002N` (Hispanic or Latino), then `P2_005N`..`P2_011N` (White
/// alone, Black or African American alone, American Indian and Alaska
/// Native alone, Asian alone, Native Hawaiian and Other Pacific Islander
/// alone, Some Other Race alone, Population of two or more races) --
/// Walker's 8 groups. `P2_003N`/`P2_004N` are subtotals ("Not Hispanic or
/// Latino:" / "Population of one race:") we don't need.
const P2_CELL_OFFSETS: [usize; 9] = [0, 1, 4, 5, 6, 7, 8, 9, 10];

/// Sum of `US_GROUP_TOTALS`: the 2020 resident population of the 50 states
/// and DC (spec §2.1, cited from the Census Bureau).
pub const US_TOTAL: u64 = 331_449_281;

/// Hispanic, White, Black, AIAN, Asian, NHPI, Other, Two-or-more (spec
/// §2.1), same order as `BlockCounts::c` and `StateCounts::state_row[1..]`.
pub const US_GROUP_TOTALS: [u64; 8] = [
    62_080_044,
    191_697_647,
    39_940_338,
    2_251_699,
    19_618_719,
    622_018,
    1_689_833,
    13_548_983,
];

#[derive(Debug, Clone, PartialEq)]
pub struct BlockCounts {
    pub geoid: u64,
    pub state: u8,
    pub c: [u32; 8],
    /// (latitude, longitude).
    pub intpt: (f64, f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateCounts {
    pub state: u8,
    pub blocks: Vec<BlockCounts>,
    /// The `SUMLEV 040` (state) row's P2 cells: `P2_001N`, then the 8
    /// groups, same order as `BlockCounts::c`. NOT validated against its own
    /// group sum here (ruling C4) -- `validate-pl` checks it against the
    /// sum of `blocks`.
    pub state_row: [u32; 9],
}

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

fn parse_p2_cells(fields: &[&str]) -> Result<[u32; 9]> {
    let mut out = [0u32; 9];
    for (i, &rel) in P2_CELL_OFFSETS.iter().enumerate() {
        let idx = SEG1_P2_OFFSET + rel;
        let field = fields.get(idx).ok_or_else(|| {
            anyhow!(
                "segment-1 row has only {} fields, need P2 cell at index {idx}",
                fields.len()
            )
        })?;
        out[i] = parse_u32(field, "P2 cell")?;
    }
    Ok(out)
}

/// Splits `geo` (the geoheader file's contents) and `seg1` (`File01`'s
/// contents, table P1 then P2) on `|` and joins them on `LOGRECNO`. Errors,
/// naming the GEOID, when a `SUMLEV 750` (block) row's 8 groups don't sum
/// to its `P2_001N` (ruling C4: state `040` rows are not checked here).
#[cfg_attr(not(test), allow(dead_code))] // tests, and the readers of later tasks
pub fn parse_parts(geo: &str, seg1: &str) -> Result<StateCounts> {
    parse_streams(geo.as_bytes(), seg1.as_bytes())
}

/// Calls `f` with every non-empty line of `r` (lossily UTF-8; the files are
/// ASCII apart from place names this parser never reads).
fn for_each_line(mut r: impl BufRead, mut f: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.is_empty() {
            f(line)?;
        }
    }
}

/// [`parse_parts`] over line streams: segment 1 first (only each row's 9
/// P2 cells are kept), then the geoheader, one line at a time -- Task 5's
/// converter holds no whole file (Texas's are 306 and 362 MB).
pub fn parse_streams(geo: impl BufRead, seg1: impl BufRead) -> Result<StateCounts> {
    let mut seg_by_logrecno: HashMap<u32, [u32; 9]> = HashMap::new();
    for_each_line(seg1, |line| {
        let fields: Vec<&str> = line.split('|').collect();
        let logrecno: u32 = fields
            .get(SEG_LOGRECNO)
            .ok_or_else(|| anyhow!("segment-1 row has no LOGRECNO field"))?
            .trim()
            .parse()
            .context("segment-1 LOGRECNO is not a u32")?;
        seg_by_logrecno.insert(logrecno, parse_p2_cells(&fields)?);
        Ok(())
    })?;

    let mut blocks = Vec::new();
    let mut state_row: Option<[u32; 9]> = None;
    let mut state: Option<u8> = None;
    let mut first = true;

    for_each_line(geo, |line| {
        if std::mem::take(&mut first) {
            let fileid = line.split('|').nth(GeoFields::FILEID).ok_or_else(|| anyhow!("geoheader row has no FILEID field"))?;
            if fileid != "PLST" {
                bail!("unexpected FILEID {fileid:?}, want \"PLST\" (2020 P.L. 94-171 state file)");
            }
        }
        let fields: Vec<&str> = line.split('|').collect();
        let sumlev = *fields
            .get(GeoFields::SUMLEV)
            .ok_or_else(|| anyhow!("geoheader row has no SUMLEV field"))?;
        if sumlev != "040" && sumlev != "750" {
            return Ok(());
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
        let cells = *seg_by_logrecno
            .get(&logrecno)
            .ok_or_else(|| anyhow!("no segment-1 row for LOGRECNO {logrecno}"))?;

        if sumlev == "040" {
            let state_fips: u8 = suffix
                .get(0..2)
                .ok_or_else(|| anyhow!("040 GEOID suffix {suffix:?} too short for a state code"))?
                .parse()
                .context("state FIPS code is not a u8")?;
            state = Some(state_fips);
            state_row = Some(cells);
            return Ok(());
        }

        // sumlev == "750": a block. Validate the 8 groups sum to P2_001N.
        let total = cells[0];
        let c: [u32; 8] = cells[1..9].try_into().expect("9 cells, 8 after the total");
        let sum: u64 = c.iter().map(|&x| u64::from(x)).sum();
        if sum != u64::from(total) {
            bail!(
                "block {suffix}: the 8 groups sum to {sum}, but P2_001N is {total}"
            );
        }
        let geoid: u64 = suffix
            .parse()
            .with_context(|| format!("block GEOID suffix {suffix:?} is not a u64"))?;
        let block_state: u8 = suffix
            .get(0..2)
            .ok_or_else(|| anyhow!("750 GEOID suffix {suffix:?} too short for a state code"))?
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
            state: block_state,
            c,
            intpt: (lat, lon),
        });
        Ok(())
    })?;

    if first {
        bail!("geoheader is empty");
    }
    let state = state.ok_or_else(|| anyhow!("no SUMLEV 040 (state) row found"))?;
    let state_row = state_row.ok_or_else(|| anyhow!("no SUMLEV 040 (state) row found"))?;

    Ok(StateCounts {
        state,
        blocks,
        state_row,
    })
}

/// Opens `zip_path` (one state's `<st>2020.pl.zip`) and reads `xxgeo2020.pl`
/// and `xx000012020.pl` as streams -- there is no extraction to disk.
pub fn parse_state(zip_path: &Path) -> Result<StateCounts> {
    let open = || -> Result<zip::ZipArchive<std::fs::File>> {
        let file = std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
        zip::ZipArchive::new(file).with_context(|| format!("reading {} as a zip", zip_path.display()))
    };
    let member = |a: &zip::ZipArchive<std::fs::File>, suffix: &str| -> Result<String> {
        a.file_names()
            .find(|n| n.to_lowercase().ends_with(suffix))
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{}: no zip entry ending with {suffix:?}", zip_path.display()))
    };
    // Two handles, so both members stream at once.
    let (mut a, mut b) = (open()?, open()?);
    let (geo_name, seg_name) = (member(&a, "geo2020.pl")?, member(&a, "000012020.pl")?);
    let geo = BufReader::with_capacity(1 << 20, a.by_name(&geo_name)?);
    let seg1 = BufReader::with_capacity(1 << 20, b.by_name(&seg_name)?);
    parse_streams(geo, seg1)
}

/// Total field count in the 2020 P.L. 94-171 geoheader record (`FILEID`
/// through `UGA`, Figure 2-4 in full).
#[cfg(test)]
const GEO_FIELD_COUNT: usize = 97;

/// Builds one geoheader row with only the fields this crate reads set,
/// everything else blank -- same positions `GeoFields` reads back.
#[cfg(test)]
fn geo_row(sumlev: &str, logrecno: u32, geoid: &str, pop100: u32, lat: f64, lon: f64) -> String {
    let mut fields = vec![String::new(); GEO_FIELD_COUNT];
    fields[GeoFields::FILEID] = "PLST".to_string();
    fields[GeoFields::SUMLEV] = sumlev.to_string();
    fields[GeoFields::LOGRECNO] = logrecno.to_string();
    fields[GeoFields::GEOCODE] = geoid.to_string();
    fields[GeoFields::POP100] = pop100.to_string();
    fields[GeoFields::INTPTLAT] = format!("{lat}");
    fields[GeoFields::INTPTLON] = format!("{lon}");
    fields.join("|")
}

/// Builds one segment-1 row: the 5 leading fields, 71 zeroed P1 cells, then
/// P2's 73 cells with `p2 = [P2_001N, P2_002N, P2_005N..P2_011N]` written at
/// `P2_CELL_OFFSETS` and the rest (`P2_003N`, `P2_004N`, `P2_012N..P2_073N`)
/// zeroed.
#[cfg(test)]
fn seg1_row(logrecno: u32, p2: [u32; 9]) -> String {
    let mut fields = vec!["0".to_string(); SEG_LEADING_FIELDS + 71 + 73];
    fields[0] = "PLST".to_string();
    fields[1] = "XX".to_string();
    fields[2] = "000".to_string();
    fields[3] = "01".to_string();
    fields[SEG_LOGRECNO] = logrecno.to_string();
    for (i, &rel) in P2_CELL_OFFSETS.iter().enumerate() {
        fields[SEG1_P2_OFFSET + rel] = p2[i].to_string();
    }
    fields.join("|")
}

/// For tests elsewhere in the crate: writes a one-state P.L. zip holding
/// `blocks` (GEOID, 8 group counts, interior point (lat, lon)) and a state
/// row that is their sum.
#[cfg(test)]
pub fn write_test_zip(path: &Path, blocks: &[(u64, [u32; 8], (f64, f64))]) {
    use std::io::Write;
    let state = blocks[0].0 / 10u64.pow(13);
    let mut total = [0u32; 9];
    let mut geo = vec![geo_row("040", 1, &format!("0400000US{state:02}"), 0, 38.9, -77.0)];
    let mut seg = Vec::new();
    for (i, (geoid, c, (lat, lon))) in blocks.iter().enumerate() {
        let sum: u32 = c.iter().sum();
        let mut cells = [0u32; 9];
        cells[0] = sum;
        cells[1..].copy_from_slice(c);
        for (t, v) in total.iter_mut().zip(cells) {
            *t += v;
        }
        let logrecno = i as u32 + 2;
        geo.push(geo_row("750", logrecno, &format!("7500000US{geoid:015}"), sum, *lat, *lon));
        seg.push(seg1_row(logrecno, cells));
    }
    seg.insert(0, seg1_row(1, total));
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let o = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("xxgeo2020.pl", o).unwrap();
    z.write_all(geo.join("\n").as_bytes()).unwrap();
    z.start_file("xx000012020.pl", o).unwrap();
    z.write_all(seg.join("\n").as_bytes()).unwrap();
    z.finish().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A geoheader row and a segment-1 row, joined on LOGRECNO. Field positions come from
    /// `GEO_FIELDS` / `SEG1_P2_OFFSET`, never from literals in the test.
    fn fixture() -> (String, String) {
        let geo = [
            geo_row("040", 1, "0400000US11", 689_545, 38.9, -77.0),
            geo_row("750", 2, "7500000US110010001011000", 120, 38.91, -77.04),
            geo_row("750", 3, "7500000US110010001011001", 0, 38.92, -77.05),
        ]
        .join("\n");
        let seg = [
            seg1_row(1, [689_545, 77_760, 0, 0, 0, 0, 0, 0, 0]),
            seg1_row(2, [120, 10, 60, 30, 0, 15, 0, 1, 4]),
            seg1_row(3, [0; 9]),
        ]
        .join("\n");
        (geo, seg)
    }

    #[test]
    fn blocks_are_sumlev_750_and_groups_sum_to_the_total() {
        let (geo, seg) = fixture();
        let s = parse_parts(&geo, &seg).unwrap();
        assert_eq!(s.blocks.len(), 2);
        let b = &s.blocks[0];
        assert_eq!(b.geoid, 110010001011000);
        assert_eq!(b.c, [10, 60, 30, 0, 15, 0, 1, 4]);
        assert_eq!(b.c.iter().map(|&x| u64::from(x)).sum::<u64>(), 120);
    }

    #[test]
    fn a_group_sum_that_disagrees_with_p2_001n_is_an_error() {
        let (geo, _) = fixture();
        let seg = [
            seg1_row(1, [689_545, 0, 0, 0, 0, 0, 0, 0, 0]),
            seg1_row(2, [121, 10, 60, 30, 0, 15, 0, 1, 4]),
            seg1_row(3, [0; 9]),
        ]
        .join("\n");
        let err = parse_parts(&geo, &seg).unwrap_err().to_string();
        assert!(err.contains("110010001011000"), "{err}");
    }

    #[test]
    fn the_us_group_totals_sum_to_the_resident_population() {
        assert_eq!(US_GROUP_TOTALS.iter().sum::<u64>(), US_TOTAL);
    }

    #[test]
    fn the_040_row_is_returned_as_state_row_uninspected() {
        // Ruling C4: the state row's own group sum (77_760+0*7 = 77_760 !=
        // 689_545) is deliberately inconsistent in the fixture, and
        // parse_parts must not error on it -- only `validate-pl` checks it,
        // against the block sums.
        let (geo, seg) = fixture();
        let s = parse_parts(&geo, &seg).unwrap();
        assert_eq!(s.state, 11);
        assert_eq!(s.state_row, [689_545, 77_760, 0, 0, 0, 0, 0, 0, 0]);
    }
}
