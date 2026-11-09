//! The census demo's income breakdowns: the maths lives in
//! `gpui_maps::census::breakdowns`, shared with the browser shell; this
//! file adds only [`load`], which reads the tables from the demo's data
//! directory.

pub use gpui_maps::census::breakdowns::*;

/// Reads the population and metro tables under the demo's data `dir` and
/// builds the breakdowns for `features` (`(GEOID, income)`, in the demo's
/// feature order). `Err` names the missing file, or the table past its
/// bound.
pub fn load(dir: &std::path::Path, features: impl Iterator<Item = (u64, Option<f64>)>) -> Result<Breakdowns, String> {
    let read = |rel: &str| {
        let path = dir.join(rel);
        std::fs::read_to_string(&path)
            .map_err(|err| format!("{}: {err} -- run demos/data/fetch_us_block_groups.sh", path.display()))
    };
    let population = read("acs/population_bg_2023.csv")?;
    let cbsa = read("cbsa/county_metro_2023.tsv")?;
    let rows: Vec<Row> = features.map(|(geoid, income)| Row { geoid, income, population: None }).collect();
    Breakdowns::from_texts(&rows, Some(&population), &cbsa, BinEdges::default())
}
