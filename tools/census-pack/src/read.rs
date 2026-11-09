//! The census inputs `demos/data/fetch_us_block_groups.sh` writes, read in
//! place: TIGER/Line block-group shapefiles, the ACS income and population
//! CSVs, the county gazetteer and the county->CBSA table. The shapefile
//! loop and the income join are the native demo's
//! (`demos/us_block_groups.rs`: `load_shapefiles`, `join_income`).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use gpui_maps::census::pack::{Feature, PackRow};

/// A `GEOID,value` CSV (header skipped) as GEOID -> `Some(value)`, or
/// `None` for a row whose estimate is blank (the ACS's no-estimate). A
/// GEOID absent from the map is not in the table at all.
pub fn parse_table(text: &str) -> HashMap<u64, Option<u32>> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let (geoid, value) = line.split_once(',')?;
            Some((geoid.trim().parse().ok()?, value.trim().parse().ok()))
        })
        .collect()
}

pub fn read_table(path: &Path) -> Result<HashMap<u64, Option<u32>>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{} -- run demos/data/fetch_us_block_groups.sh", path.display()))?;
    Ok(parse_table(&text))
}

/// The gazetteer (`USPS GEOID ANSICODE NAME ...`) reduced to the pack's
/// `GEOID5\tNAME\n` lines, in file order.
pub fn county_text(gazetteer: &str) -> String {
    let mut out = String::new();
    for line in gazetteer.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 4 && cols[1].trim().len() == 5 && cols[1].trim().bytes().all(|b| b.is_ascii_digit()) {
            out.push_str(cols[1].trim());
            out.push('\t');
            out.push_str(cols[3].trim());
            out.push('\n');
        }
    }
    out
}

/// How the income join went, counted the way the native demo's
/// `income join:` line counts it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoinCounts {
    pub features: usize,
    pub matched: usize,
    pub no_estimate: usize,
    pub not_in_table: usize,
}

impl JoinCounts {
    pub fn add(&mut self, other: JoinCounts) {
        self.features += other.features;
        self.matched += other.matched;
        self.no_estimate += other.no_estimate;
        self.not_in_table += other.not_in_table;
    }
}

/// One state's block groups in shapefile order, and their pack rows.
pub fn read_state(
    dir: &Path,
    fips: u8,
    income: &HashMap<u64, Option<u32>>,
    population: &HashMap<u64, Option<u32>>,
) -> Result<(Vec<Feature>, Vec<PackRow>, JoinCounts)> {
    use shapefile::dbase::FieldValue;
    use shapefile::PolygonRing;
    let path = dir.join("shp").join(format!("tl_2023_{fips:02}_bg.shp"));
    let mut reader = shapefile::Reader::from_path(&path)
        .with_context(|| format!("{} -- run demos/data/fetch_us_block_groups.sh first", path.display()))?;
    let mut features = Vec::new();
    let mut rows = Vec::new();
    let mut counts = JoinCounts::default();
    for item in reader.iter_shapes_and_records_as::<shapefile::Polygon, shapefile::dbase::Record>() {
        let (shape, record) = item.with_context(|| format!("{}: a block group record", path.display()))?;
        let number = |key: &str| match record.get(key) {
            Some(FieldValue::Numeric(Some(v))) => *v,
            Some(FieldValue::Character(Some(s))) => s.trim().parse().unwrap_or(0.0),
            _ => 0.0,
        };
        // TIGER writes each outer ring followed by its own holes, so a hole
        // belongs to the outer ring before it.
        let mut feature: Feature = Vec::new();
        for ring in shape.rings() {
            let points: Vec<[f64; 2]> = ring.points().iter().map(|p| [p.x, p.y]).collect();
            if matches!(ring, PolygonRing::Outer(_)) || feature.is_empty() {
                feature.push(Vec::new());
            }
            feature.last_mut().expect("pushed above").push(points);
        }
        let geoid = number("GEOID") as u64;
        let income = match income.get(&geoid) {
            Some(Some(v)) => {
                counts.matched += 1;
                Some(*v)
            }
            Some(None) => {
                counts.no_estimate += 1;
                None
            }
            None => {
                counts.not_in_table += 1;
                None
            }
        };
        counts.features += 1;
        rows.push(PackRow {
            geoid,
            income,
            population: population.get(&geoid).copied().flatten(),
            aland_km2: (number("ALAND") / 1e6) as f32,
        });
        features.push(feature);
    }
    Ok((features, rows, counts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_estimate_is_none_and_a_zero_is_zero() {
        let t = parse_table("GEOID,income\n010010201001,63750\n010010201002,\n010010201003,0\n");
        assert_eq!(t.get(&10010201001), Some(&Some(63_750)));
        assert_eq!(t.get(&10010201002), Some(&None));
        assert_eq!(t.get(&10010201003), Some(&Some(0)));
        assert_eq!(t.get(&10010201004), None);
    }

    #[test]
    fn the_gazetteer_reduces_to_geoid_and_name() {
        let text = county_text(
            "USPS\tGEOID\tANSICODE\tNAME\tALAND\n\
             CA\t06037\t00277283\tLos Angeles County\t1\n\
             CT\t09110\t02830244\tCapitol Planning Region\t1\n",
        );
        assert_eq!(text, "06037\tLos Angeles County\n09110\tCapitol Planning Region\n");
        let names = gpui_maps::census::names::parse_county_names(&text);
        assert_eq!(names.get(&6037).map(String::as_str), Some("Los Angeles County"));
    }
}
