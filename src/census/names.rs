//! The census popup's words: a GEOID split into its parts, the tract as
//! the Census prints it, the county and state names. Shared by the native
//! demo and the browser shell. Moved from `demos/us_block_groups.rs`.

use std::collections::HashMap;

/// `state_fips` (6, 9, 72, ...) -> its name, for the hover popup's title
/// line: the 50 states, DC and Puerto Rico. The other TIGER-only "states"
/// (the island areas: 60 66 69 78) have no ACS presence and are `None`, so
/// the popup falls back to their bare FIPS code.
pub fn state_name(fips: u32) -> Option<&'static str> {
    super::breakdowns::state_name(fips).map(|(name, _)| name)
}

/// The one state table, owned by [`super::breakdowns`]: re-exported so
/// the popup and the Breakdowns panel read the same names.
pub use super::breakdowns::STATES;

/// Splits a 12-digit block-group GEOID into `(state FIPS, county FIPS,
/// TRACTCE, block-group digit)`. E.g. `060375432021` ->
/// `(6, 37, 543202, 1)`: Los Angeles County, tract 5432.02, block group 1.
pub fn split_geoid(geoid: u64) -> (u32, u32, u32, u32) {
    let bg = (geoid % 10) as u32;
    let tract = ((geoid / 10) % 1_000_000) as u32;
    let county = ((geoid / 10_000_000) % 1_000) as u32;
    let state = ((geoid / 10_000_000_000) % 100) as u32;
    (state, county, tract, bg)
}

/// TRACTCE's implied two decimals, the trailing `.00` dropped as the
/// Census itself prints it: `543202` -> `"5432.02"`, `500100` -> `"5001"`.
pub fn format_tract(tract_ce: u32) -> String {
    let (whole, frac) = (tract_ce / 100, tract_ce % 100);
    if frac == 0 { whole.to_string() } else { format!("{whole}.{frac:02}") }
}

/// The county (or county-equivalent -- a CT planning region, a PR
/// municipio, ...) name for `state_fips, county_fips`, from the table
/// `load_county_names` loaded, or a graceful fallback, `"County 037"`,
/// that never panics: an empty table (the names file was never fetched)
/// or a code the table doesn't have (a new one TIGER shipped since) both
/// take it.
pub fn county_label(state_fips: u32, county_fips: u32, counties: &HashMap<u32, String>) -> String {
    let key = state_fips * 1000 + county_fips;
    counties.get(&key).cloned().unwrap_or_else(|| format!("County {county_fips:03}"))
}

/// The popup's title line: `"Los Angeles County, California"`. Falls back
/// gracefully on both sides -- `"County 037, state 06"` if neither table
/// has an entry -- and never panics.
pub fn popup_title(geoid: u64, counties: &HashMap<u32, String>) -> String {
    let (state_fips, county_fips, _, _) = split_geoid(geoid);
    let county = county_label(state_fips, county_fips, counties);
    match state_name(state_fips) {
        Some(state) => format!("{county}, {state}"),
        None => format!("{county}, state {state_fips:02}"),
    }
}

/// The popup's secondary line: `"Tract 5432.02 · Block group 1"`.
pub fn popup_subtitle(geoid: u64) -> String {
    let (_, _, tract, bg) = split_geoid(geoid);
    format!("Tract {} · Block group {bg}", format_tract(tract))
}

/// At most this many lines are read. The 2023 gazetteer has 3,223 (a header
/// and 3,222 counties); the pack's `GEOID5\tNAME` form has one per county.
/// Lines past it are ignored, so the map holds at most this many entries.
pub const MAX_COUNTY_LINES: usize = 10_000;

/// A line with more tab-separated fields than this is skipped. The
/// gazetteer has 10; the pack's form has 2. Only the first four are looked
/// at, and the rest are counted without being collected.
pub const MAX_COUNTY_FIELDS: usize = 16;

/// County names keyed by the 5-digit state+county FIPS, from either form:
/// the pack's `GEOID5\tNAME` lines, or the Census gazetteer's own
/// `USPS\tGEOID\tANSICODE\tNAME\t...` rows (header included -- its GEOID
/// does not parse, so it drops out). Told apart by the column count.
///
/// The text is untrusted (the pack's tables section, itself capped at
/// `pack::MAX_SECTION_RAW` = 8 MiB). Worst case: at most
/// [`MAX_COUNTY_LINES`] entries, each a `u32` key and a `String` header in
/// the map (~40 B with the table's overhead: 10,000 x 40 B = 400 KB), plus
/// the names' bytes, which are copies of distinct parts of the input and so
/// sum to at most the text's length (<= 8 MiB). Under 9 MB in all. No
/// per-field allocation: each line is walked by an iterator, at most
/// [`MAX_COUNTY_FIELDS`] + 1 fields deep.
pub fn parse_county_names(text: &str) -> HashMap<u32, String> {
    text.lines()
        .take(MAX_COUNTY_LINES)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let f0 = fields.next()?;
            let f1 = fields.next()?;
            let (geoid, name) = match (fields.next(), fields.next()) {
                (None, _) => (f0, f1),
                (Some(_), None) => return None,
                (Some(_), Some(f3)) => {
                    // Four seen; more than MAX_COUNTY_FIELDS in all skips the line.
                    if fields.take(MAX_COUNTY_FIELDS - 3).count() > MAX_COUNTY_FIELDS - 4 {
                        return None;
                    }
                    (f1, f3)
                }
            };
            Some((geoid.trim().parse().ok()?, name.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
// GEOIDs are written as the Census publishes them, leading state zero
// included (`060_371_011_101` is LA County); they are decimal, not octal.
#[allow(clippy::zero_prefixed_literal)]
mod tests {
    use super::*;

    #[test]
    fn geoid_splits_into_state_county_tract_and_block_group() {
        // Los Angeles County tract 5432.02, block group 1.
        assert_eq!(split_geoid(060_375_432_021), (6, 37, 543_202, 1));
    }

    #[test]
    fn a_tract_ending_in_00_drops_its_decimals() {
        assert_eq!(format_tract(500_100), "5001");
        assert_eq!(format_tract(543_202), "5432.02");
        assert_eq!(format_tract(1), "0.01");
        assert_eq!(format_tract(100), "1");
    }

    #[test]
    fn a_leading_zero_state_still_names_california() {
        assert_eq!(state_name(6), Some("California"));
        let counties = HashMap::from([(6037, "Los Angeles County".to_string())]);
        assert_eq!(popup_title(060_375_432_021, &counties), "Los Angeles County, California");
        assert_eq!(popup_subtitle(060_375_432_021), "Tract 5432.02 · Block group 1");
    }

    #[test]
    fn a_connecticut_planning_region_names_as_a_region_not_a_county() {
        // CT replaced its eight counties with FIPS 09110-09190 planning
        // regions in 2022+ TIGER; the popup must resolve those codes, not
        // just the pre-2022 09001-09015 range.
        assert_eq!(state_name(9), Some("Connecticut"));
        let counties = HashMap::from([(9110, "Capitol Planning Region".to_string())]);
        let geoid = 091_100_001_001; // state 09, region 110, tract 0001.00, bg 1
        assert_eq!(popup_title(geoid, &counties), "Capitol Planning Region, Connecticut");
        // Missing from the table (file not fetched, or a code it lacks):
        // falls back to "County NNN" rather than panicking.
        assert_eq!(popup_title(geoid, &HashMap::new()), "County 110, Connecticut");
    }

    #[test]
    fn a_puerto_rico_municipio_names_correctly() {
        assert_eq!(state_name(72), Some("Puerto Rico"));
        let counties = HashMap::from([(72001, "Adjuntas Municipio".to_string())]);
        let geoid = 720_010_001_001;
        assert_eq!(popup_title(geoid, &counties), "Adjuntas Municipio, Puerto Rico");
    }

    #[test]
    fn an_unknown_state_falls_back_to_its_fips_code_never_panics() {
        // A territory outside the closed 50+DC+PR table (e.g. Guam, 66):
        // no panic, just its bare FIPS code.
        assert_eq!(state_name(66), None);
        assert_eq!(popup_title(660_100_001_001, &HashMap::new()), "County 010, state 66");
    }

    #[test]
    fn popup_title_of_los_angeles() {
        let counties = parse_county_names("06037\tLos Angeles County\n");
        assert_eq!(popup_title(60_375_432_021, &counties), "Los Angeles County, California");
    }

    #[test]
    fn popup_title_falls_back_without_names() {
        assert_eq!(popup_title(60_375_432_021, &HashMap::new()), "County 037, California");
    }

    #[test]
    fn parse_county_names_reads_both_forms() {
        let pack = parse_county_names("06037\tLos Angeles County\n09110\tCapitol Planning Region\n");
        let gazetteer = parse_county_names(
            "USPS\tGEOID\tANSICODE\tNAME\tALAND\tAWATER\n\
             CA\t06037\t00277283\tLos Angeles County\t1\t2\n\
             CT\t09110\t02830244\tCapitol Planning Region\t1\t2\n",
        );
        assert_eq!(pack, gazetteer);
        assert_eq!(pack.len(), 2);
        assert_eq!(pack.get(&6037).map(String::as_str), Some("Los Angeles County"));
        assert!(parse_county_names("").is_empty());
        assert!(parse_county_names("junk\n").is_empty());
    }

    /// `MAX_COUNTY_LINES`: the last line inside the cap is read, the first
    /// past it is not.
    #[test]
    fn county_lines_past_the_cap_are_ignored() {
        let lines = |n: usize| (0..n).map(|i| format!("{}\tC{i}\n", 10_000 + i)).collect::<String>();
        let at = parse_county_names(&lines(MAX_COUNTY_LINES));
        assert_eq!(at.len(), MAX_COUNTY_LINES);
        assert!(at.contains_key(&(10_000 + MAX_COUNTY_LINES as u32 - 1)));
        let over = parse_county_names(&lines(MAX_COUNTY_LINES + 1));
        assert_eq!(over.len(), MAX_COUNTY_LINES);
        assert!(!over.contains_key(&(10_000 + MAX_COUNTY_LINES as u32)));
    }

    /// `MAX_COUNTY_FIELDS`: a gazetteer row of exactly that many fields is
    /// read; one more field and it is skipped. One 8 MiB line of tabs is
    /// skipped without building a slice per field.
    #[test]
    fn a_row_with_too_many_fields_is_skipped() {
        let row = |n: usize| {
            let mut f = vec!["CA", "06037", "00277283", "Los Angeles County"];
            f.resize(n, "x");
            f.join("\t")
        };
        assert_eq!(parse_county_names(&row(MAX_COUNTY_FIELDS)).get(&6037).map(String::as_str), Some("Los Angeles County"));
        assert!(parse_county_names(&row(MAX_COUNTY_FIELDS + 1)).is_empty());
        assert_eq!(parse_county_names(&row(4)).len(), 1);
        assert!(parse_county_names("CA\t06037\t00277283\n").is_empty(), "three fields is neither form");
        let tabs = format!("CA\t06037\t0\tLos Angeles County{}", "\t".repeat(8 << 20));
        assert!(parse_county_names(&tabs).is_empty());
    }

    /// The real 2023 gazetteer fits both caps with room: 3,223 lines, 10
    /// fields.
    #[test]
    fn the_caps_hold_the_real_gazetteer_shape() {
        const { assert!(3_223 * 3 <= MAX_COUNTY_LINES) };
        const { assert!(10 < MAX_COUNTY_FIELDS) };
    }

    /// The popup text native prints at the five points the web gate
    /// (`scripts/census-web.mjs hover`) checks. The county names are the
    /// 2023 gazetteer's; the page must show exactly these strings. (The
    /// plan named `060375432021` and `720270101011`, which are not 2023
    /// block groups; these are their nearest real ones: LA tract 5432.01,
    /// and San Juan's tract 4.)
    #[test]
    fn native_popup_strings_for_the_gate_points() {
        let counties = parse_county_names(
            "06037\tLos Angeles County\n36061\tNew York County\n17031\tCook County\n\
             02020\tAnchorage Municipality\n72127\tSan Juan Municipio\n",
        );
        let expect = [
            (60_375_432_011u64, "Los Angeles County, California", "Tract 5432.01 · Block group 1"),
            (360_610_101_001, "New York County, New York", "Tract 101 · Block group 1"),
            (170_318_391_001, "Cook County, Illinois", "Tract 8391 · Block group 1"),
            (20_200_011_001, "Anchorage Municipality, Alaska", "Tract 11 · Block group 1"),
            (721_270_004_001, "San Juan Municipio, Puerto Rico", "Tract 4 · Block group 1"),
        ];
        for (geoid, title, subtitle) in expect {
            println!("{geoid:012}\t{}\t{}", popup_title(geoid, &counties), popup_subtitle(geoid));
            assert_eq!(popup_title(geoid, &counties), title);
            assert_eq!(popup_subtitle(geoid), subtitle);
        }
    }
}
