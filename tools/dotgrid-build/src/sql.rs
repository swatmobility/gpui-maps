//! Every statement `--load-clickhouse` sends, in one place, so the tests below
//! can run all of them through the engine's `sql_guard`: none selects `*`,
//! none writes outside `gpui_map_dots`, and every `geoToH3` call reads its
//! arguments from named `lat`/`lon` aliases (this server's `geoToH3` takes
//! `(lat, lon, res)` since 25.5; the old `(lon, lat)` order returns a VALID
//! cell thousands of kilometres away, never an error).
//!
//! Values travel as ClickHouse query parameters (`{e:UInt16}` in the SQL,
//! `param_e=…` on the URL); only table names are spliced in, and only names
//! from [`TABLES`].

/// A statement and the query parameters it binds.
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<(&'static str, String)>,
}

impl Stmt {
    fn new(sql: impl Into<String>) -> Self {
        Stmt { sql: sql.into(), params: Vec::new() }
    }
    fn with(mut self, name: &'static str, value: impl ToString) -> Self {
        self.params.push((name, value.to_string()));
        self
    }
    /// A live test's own read (the tests still hold it to the rules).
    #[cfg(test)]
    pub fn test(sql: impl Into<String>) -> Self {
        let st = Self::new(sql);
        assert!(!gpui_maps::overlay::dotgrid::sql_guard::star_select_in(&st.sql), "{}", st.sql);
        st
    }
    #[cfg(test)]
    pub fn with_test(self, name: &'static str, value: impl ToString) -> Self {
        self.with(name, value)
    }
}

/// The tables the loader may create or write (a `_stage` twin of each is
/// derived from these). `_load` tables hold a new epoch until it passes its
/// checks; `_check` tables hold the parent-level cross-check.
pub const TABLES: [&str; 6] = [
    "ghs_cells_30ss",
    "ghs_cells_3ss",
    "ghs_cells_30ss_load",
    "ghs_cells_3ss_load",
    "ghs_cells_30ss_check",
    "ghs_cells_3ss_check",
];

/// The live 3″ test's tables (Task 13), in `gpui_map_dots` beside the real
/// ones and dropped by the test; not writable outside `cfg(test)`.
#[cfg(test)]
pub const TEST_TABLES: [&str; 3] = ["ghs_cells_3ss_test", "ghs_cells_3ss_test_load", "ghs_cells_3ss_test_check"];

fn checked(table: &str) -> &str {
    #[cfg(test)]
    if TEST_TABLES.contains(&table) {
        return table;
    }
    assert!(TABLES.contains(&table), "{table} is not a dotgrid table");
    table
}

/// No `IF NOT EXISTS`: the loader sends this only after `system.databases`
/// said the database is absent, so a concurrent creator makes the load fail
/// instead of being adopted without its marker.
pub const CREATE_DB: &str = "CREATE DATABASE gpui_map_dots";
pub const CREATE_OWNER: &str =
    "CREATE TABLE IF NOT EXISTS gpui_map_dots._dotgrid_owner (created DateTime) ENGINE = TinyLog";
pub const INSERT_OWNER: &str = "INSERT INTO gpui_map_dots._dotgrid_owner (created) VALUES (now())";

/// 1 when a database named `{db:String}` exists.
pub fn database_exists(db: &str) -> Stmt {
    Stmt::new("SELECT count() FROM system.databases WHERE name = {db:String}").with("db", db)
}

/// 1 when `{db:String}` carries the loader's ownership marker.
pub fn owner_marker_exists(db: &str) -> Stmt {
    Stmt::new("SELECT count() FROM system.tables WHERE database = {db:String} AND name = '_dotgrid_owner'").with("db", db)
}

pub const DISKS: &str = "SELECT name, path, free_space FROM system.disks FORMAT TabSeparated";

/// Spec §7.2's schema. `tile` is a plain column the loader writes (never
/// `MATERIALIZED`, which `SELECT *` would silently drop).
pub fn create_cells(table: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS gpui_map_dots.{} (\n\
         \x20   epoch  UInt16,\n\
         \x20   level  UInt8,\n\
         \x20   tile   UInt32,\n\
         \x20   row    UInt32,\n\
         \x20   col    UInt32,\n\
         \x20   pop    Float32 CODEC(Gorilla, ZSTD(3)),\n\
         \x20   h3_r10 UInt64  CODEC(ZSTD(3))\n\
         ) ENGINE = MergeTree PARTITION BY epoch ORDER BY (epoch, level, tile, row, col)",
        checked(table)
    )
}

/// `PARTITION BY epoch` is what lets a verified epoch replace the old one
/// in one step ([`replace_epoch`]); a table made before it cannot.
pub fn partition_key(table: &str) -> Stmt {
    Stmt::new("SELECT partition_key FROM system.tables WHERE database = 'gpui_map_dots' AND name = {t:String} FORMAT TabSeparated")
        .with("t", checked(table))
}

/// Swaps the verified epoch from `<table>_load` into `table` atomically:
/// the old epoch's rows stay readable until this statement, and are
/// untouched if the load fails before it. The epoch is a `u16`, spliced as
/// a number (a partition expression takes no query parameter).
pub fn replace_epoch(table: &str, epoch: u16) -> String {
    let t = checked(table);
    let load = checked(&format!("{t}_load")).to_string();
    format!("ALTER TABLE gpui_map_dots.{t} REPLACE PARTITION {epoch} FROM gpui_map_dots.{load}")
}

/// The RowBinary landing table: `create_cells` without `h3_r10`.
pub fn create_stage(table: &str) -> String {
    format!(
        "CREATE TABLE gpui_map_dots.{}_stage (epoch UInt16, level UInt8, tile UInt32, row UInt32, col UInt32, pop Float32) \
         ENGINE = MergeTree ORDER BY (epoch, level, tile, row, col)",
        checked(table)
    )
}

pub fn drop_stage(table: &str) -> String {
    format!("DROP TABLE IF EXISTS gpui_map_dots.{}_stage", checked(table))
}

pub fn drop_table(table: &str) -> String {
    format!("DROP TABLE IF EXISTS gpui_map_dots.{}", checked(table))
}

/// Rows the table already holds for an epoch (a reload replaces them).
pub fn count_epoch(table: &str, epoch: u16) -> Stmt {
    Stmt::new(format!("SELECT count() FROM gpui_map_dots.{} WHERE epoch = {{e:UInt16}}", checked(table))).with("e", epoch)
}

/// Into the `_stage` table; the body is RowBinary rows of
/// `u16 epoch, u8 level, u32 tile, u32 row, u32 col, f32 pop`, little-endian.
pub fn insert_level0(table: &str) -> String {
    format!(
        "INSERT INTO gpui_map_dots.{}_stage (epoch, level, tile, row, col, pop) FORMAT RowBinary",
        checked(table)
    )
}

/// Stage → `<table>_load`, computing `h3_r10` from the cell centre's named `lat`/`lon`.
/// Every level moves (the converter's own pyramid is loaded whole); only
/// level 0 gets an H3 cell, coarser levels 0 (spec §7.2). The cell size
/// doubles per level, hence `exp2(level)`.
pub fn fill_h3(table: &str, epoch: u16, base_arcsec: f64, origin_lat: f64) -> Stmt {
    let t = checked(table);
    checked(&format!("{t}_load"));
    Stmt::new(format!(
        "INSERT INTO gpui_map_dots.{t}_load (epoch, level, tile, row, col, pop, h3_r10)\n\
         SELECT epoch, level, tile, row, col, pop, if(level = 0, geoToH3(lat, lon, 10), toUInt64(0)) AS h3_r10\n\
         FROM (\n\
         \x20 SELECT epoch, level, tile, row, col, pop,\n\
         \x20        {{origin_lat:Float64}} - (row + 0.5) * {{arc_deg:Float64}} * exp2(level) AS lat,\n\
         \x20        -180 + (col + 0.5) * {{arc_deg:Float64}} * exp2(level)              AS lon\n\
         \x20 FROM gpui_map_dots.{t}_stage\n\
         \x20 WHERE epoch = {{e:UInt16}}\n\
         )"
    ))
    .with("e", epoch)
    .with("origin_lat", origin_lat)
    .with("arc_deg", base_arcsec / 3600.0)
}

/// Level `from + 1` as 2×2 sums of level `from`, with the parent's tile
/// computed in SQL (the 3″ path, where the converter does not hand over its
/// coarser levels). Parents whose centre lies beyond the Mercator limit are
/// dropped, as the converter drops them. `src` and `dst` differ only for
/// the cross-check against the converter's own sums.
pub fn insert_parent_level_into(
    src: &str,
    dst: &str,
    epoch: u16,
    from: u8,
    max_data_zoom: u8,
    base_arcsec: f64,
    origin_lat: f64,
) -> anyhow::Result<Stmt> {
    let (src, dst) = (checked(src), checked(dst));
    let to = from + 1;
    let Some(z) = max_data_zoom.checked_sub(to) else {
        anyhow::bail!("level {to} is coarser than data zoom 0 (max data zoom {max_data_zoom})");
    };
    Ok(Stmt::new(format!(
        "INSERT INTO gpui_map_dots.{dst} (epoch, level, tile, row, col, pop, h3_r10)\n\
         SELECT epoch, level,\n\
         \x20      toUInt32(bitShiftLeft(toUInt32(floor((lon + 180) / 360 * exp2({{z:UInt8}}))), 16)\n\
         \x20        + toUInt32(least(greatest(floor((1 - log(tan(radians(lat)) + 1 / cos(radians(lat))) / pi()) / 2 * exp2({{z:UInt8}})), 0), exp2({{z:UInt8}}) - 1))) AS tile,\n\
         \x20      row, col, pop, toUInt64(0) AS h3_r10\n\
         FROM (\n\
         \x20 SELECT epoch, {{to:UInt8}} AS level, prow AS row, pcol AS col, ppop AS pop,\n\
         \x20        {{origin_lat:Float64}} - (prow + 0.5) * {{arc_deg:Float64}} * exp2({{to:UInt8}}) AS lat,\n\
         \x20        -180 + (pcol + 0.5) * {{arc_deg:Float64}} * exp2({{to:UInt8}})              AS lon\n\
         \x20 FROM (\n\
         \x20   SELECT epoch, intDiv(row, 2) AS prow, intDiv(col, 2) AS pcol, toFloat32(sum(pop)) AS ppop\n\
         \x20   FROM gpui_map_dots.{src}\n\
         \x20   WHERE epoch = {{e:UInt16}} AND level = {{from:UInt8}}\n\
         \x20   GROUP BY epoch, prow, pcol\n\
         \x20 )\n\
         )\n\
         WHERE abs(lat) <= {{max_lat:Float64}} AND pop > 0"
    ))
    .with("e", epoch)
    .with("from", from)
    .with("to", to)
    .with("z", z)
    .with("origin_lat", origin_lat)
    .with("arc_deg", base_arcsec / 3600.0)
    .with("max_lat", gpui_maps::overlay::dotgrid::grid::MAX_MERCATOR_LAT))
}

/// The 3″ load's per-level step (Task 13): level `from + 1` of `table`
/// from its own level `from`, on `d`'s grid. The 30″ load exercises the
/// same SQL through [`insert_parent_level_into`]'s cross-check.
pub fn insert_parent_level(
    table: &str,
    epoch: u16,
    from: u8,
    d: &gpui_maps::overlay::dotgrid::GridDescriptor,
) -> anyhow::Result<Stmt> {
    insert_parent_level_into(table, table, epoch, from, d.max_data_zoom, d.base_arcsec, d.origin_lat)
}

/// Rows of level `level` that differ between `a` and `b` (missing on either
/// side, or `pop` off by more than 1e-4 relative).
pub fn level_mismatches(a: &str, b: &str, epoch: u16, level: u8) -> Stmt {
    let (a, b) = (checked(a), checked(b));
    Stmt::new(format!(
        "SELECT count() FROM (\n\
         \x20 SELECT tile, row, col, pop, 1 AS here FROM gpui_map_dots.{a} WHERE epoch = {{e:UInt16}} AND level = {{k:UInt8}}\n\
         ) AS x FULL OUTER JOIN (\n\
         \x20 SELECT tile, row, col, pop, 1 AS here FROM gpui_map_dots.{b} WHERE epoch = {{e:UInt16}} AND level = {{k:UInt8}}\n\
         ) AS y USING (tile, row, col)\n\
         WHERE x.here != y.here OR abs(x.pop - y.pop) > 1e-4 * greatest(abs(x.pop), abs(y.pop), 1.0)"
    ))
    .with("e", epoch)
    .with("k", level)
}

/// MG-13: `(level-0 rows checked, rows whose own H3 cell's centre is over
/// 150 m away)`. An r10 cell's edge is about 66 m, so a correct row is well
/// inside; the swapped argument order lands thousands of kilometres off. The
/// first count makes the gate non-vacuous: the loader requires it to equal
/// the converter's level-0 cell count.
/// `PREWHERE`, not `WHERE`: with `WHERE` the server evaluates `h3ToGeo` on
/// rows of the granule that straddles levels 0 and 1 before filtering, and
/// `h3ToGeo(0)` (a coarser level's `h3_r10`) throws. A level-0 row with an
/// invalid cell still throws, which fails the load.
pub fn h3_round_trip_gate(table: &str, epoch: u16, base_arcsec: f64, origin_lat: f64) -> Stmt {
    Stmt::new(format!(
        "SELECT count(), countIf(greatCircleDistance(lon, lat, g.2, g.1) > 150) FROM (\n\
         \x20 SELECT {{origin_lat:Float64}} - (row + 0.5) * {{arc_deg:Float64}} AS lat,\n\
         \x20        -180 + (col + 0.5) * {{arc_deg:Float64}}              AS lon,\n\
         \x20        h3ToGeo(h3_r10) AS g\n\
         \x20 FROM gpui_map_dots.{}\n\
         \x20 PREWHERE epoch = {{e:UInt16}} AND level = 0\n\
         ) FORMAT TabSeparated",
        checked(table)
    ))
    .with("e", epoch)
    .with("origin_lat", origin_lat)
    .with("arc_deg", base_arcsec / 3600.0)
}

/// 1,000 level-0 rows, spread by hash, for the `h3o` cross-check.
pub fn h3_sample(table: &str, epoch: u16) -> Stmt {
    Stmt::new(format!(
        "SELECT row, col, h3_r10 FROM gpui_map_dots.{} WHERE epoch = {{e:UInt16}} AND level = 0 \
         ORDER BY cityHash64(row, col) LIMIT 1000 FORMAT TabSeparated",
        checked(table)
    ))
    .with("e", epoch)
}

pub fn level_sums(table: &str, epoch: u16) -> Stmt {
    Stmt::new(format!(
        "SELECT level, count(), sum(pop) FROM gpui_map_dots.{} WHERE epoch = {{e:UInt16}} \
         GROUP BY level ORDER BY level FORMAT TabSeparated",
        checked(table)
    ))
    .with("e", epoch)
}

/// MG-12: bytes per row over the table's active parts.
pub fn parts_bytes(table: &str) -> Stmt {
    Stmt::new(
        "SELECT sum(rows), sum(bytes_on_disk), sum(bytes_on_disk) / sum(rows) FROM system.parts \
         WHERE database = 'gpui_map_dots' AND table = {t:String} AND active FORMAT TabSeparated",
    )
    .with("t", checked(table))
}

/// Live tests' scratch databases: only names under `gpui_map_dots_test_`,
/// and no `IF [NOT] EXISTS`, so a test can neither adopt nor drop a
/// database it did not create.
#[cfg(test)]
pub mod scratch {
    fn checked(db: &str) -> &str {
        assert!(db.starts_with("gpui_map_dots_test_"), "{db} is not a scratch database");
        db
    }
    pub fn create_db(db: &str) -> String {
        format!("CREATE DATABASE {}", checked(db))
    }
    pub fn create_owner(db: &str) -> String {
        format!("CREATE TABLE {}._dotgrid_owner (created DateTime) ENGINE = TinyLog", checked(db))
    }
    pub fn drop_db(db: &str) -> String {
        format!("DROP DATABASE {}", checked(db))
    }
    pub fn all(db: &str) -> Vec<String> {
        vec![create_db(db), create_owner(db), drop_db(db)]
    }
}

/// Every template, instantiated for both tables.
#[cfg(test)]
pub fn all_statements_for_test() -> Vec<String> {
    let mut out: Vec<String> = [CREATE_DB, CREATE_OWNER, INSERT_OWNER, DISKS].iter().map(|s| s.to_string()).collect();
    out.push(database_exists("gpui_map_dots").sql);
    out.push(owner_marker_exists("gpui_map_dots").sql);
    for (t, check, arc, z) in [("ghs_cells_30ss", "ghs_cells_30ss_check", 30.0, 6), ("ghs_cells_3ss", "ghs_cells_3ss_check", 3.0, 9)] {
        let load = format!("{t}_load");
        out.extend([create_cells(t), create_stage(t), drop_stage(t), drop_table(check), insert_level0(t)]);
        out.extend([create_cells(&load), drop_table(&load), replace_epoch(t, 2025), partition_key(t).sql]);
        out.extend(
            [
                count_epoch(t, 2025),
                fill_h3(t, 2025, arc, 90.0),
                insert_parent_level(t, 2025, 0, &crate::descriptor(arc, 2025)).unwrap(),
                insert_parent_level(&load, 2025, 0, &crate::descriptor(arc, 2025)).unwrap(),
                insert_parent_level_into(t, check, 2025, 0, z, arc, 90.0).unwrap(),
                level_mismatches(t, check, 2025, 1),
                insert_parent_level_into(&load, check, 2025, 0, z, arc, 90.0).unwrap(),
                level_mismatches(&load, check, 2025, 1),
                h3_round_trip_gate(&load, 2025, arc, 90.0),
                h3_sample(&load, 2025),
                level_sums(&load, 2025),
                parts_bytes(t),
            ]
            .into_iter()
            .map(|s| s.sql),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::overlay::dotgrid::sql_guard::{names_foreign_database, star_select_in, DOTS_DB};

    #[test]
    fn no_loader_statement_selects_star() {
        for s in all_statements_for_test() {
            assert!(!star_select_in(&s), "{s}");
        }
    }

    #[test]
    fn no_loader_statement_writes_outside_gpui_map_dots() {
        for s in all_statements_for_test() {
            let upper = s.trim_start().to_ascii_uppercase();
            if upper.starts_with("INSERT")
                || upper.starts_with("CREATE")
                || upper.starts_with("DROP")
                || upper.starts_with("ALTER")
                || upper.starts_with("TRUNCATE")
            {
                assert!(!names_foreign_database(&s, DOTS_DB), "{s}");
            }
        }
    }

    #[test]
    fn no_loader_statement_names_another_database_at_all() {
        // Stricter than the brief: reads too, `system.*` aside.
        for s in all_statements_for_test() {
            assert!(!names_foreign_database(&s, DOTS_DB), "{s}");
        }
    }

    #[test]
    fn scratch_statements_name_only_their_scratch_database() {
        let db = "gpui_map_dots_test_foreign";
        for s in scratch::all(db) {
            assert!(!star_select_in(&s), "{s}");
            assert!(!names_foreign_database(&s, db), "{s}");
            assert!(names_foreign_database(&s, DOTS_DB), "{s}");
        }
    }

    #[test]
    #[should_panic(expected = "is not a scratch database")]
    fn a_scratch_statement_refuses_a_real_database() {
        scratch::drop_db("gpui_map_dots");
    }

    #[test]
    fn a_parent_level_past_zoom_0_is_an_error() {
        let d = crate::descriptor(30.0, 2025);
        assert!(insert_parent_level("ghs_cells_30ss", 2025, 5, &d).is_ok());
        assert!(insert_parent_level("ghs_cells_30ss", 2025, 6, &d).is_err());
        let d = crate::descriptor(3.0, 2025);
        assert!(insert_parent_level("ghs_cells_3ss_load", 2025, 8, &d).is_ok());
        assert!(insert_parent_level("ghs_cells_3ss_load", 2025, 9, &d).is_err());
    }

    #[test]
    fn the_parent_level_reads_the_grid_it_is_given() {
        let d = gpui_maps::overlay::dotgrid::GridDescriptor { origin_lat: 85.0, ..crate::descriptor(3.0, 2025) };
        let st = insert_parent_level("ghs_cells_3ss_load", 2025, 0, &d).unwrap();
        let p = |k: &str| st.params.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(p("origin_lat"), "85");
        assert_eq!(p("arc_deg"), (3.0f64 / 3600.0).to_string());
        assert_eq!(p("z"), "8");
    }

    #[test]
    fn geo_to_h3_is_called_lat_first() {
        let mut seen = 0;
        for s in all_statements_for_test() {
            if s.contains("geoToH3") {
                seen += 1;
                assert!(s.contains("geoToH3(lat, lon, 10)"), "{s}");
                assert_eq!(s.matches("geoToH3").count(), 1, "{s}");
            }
        }
        assert!(seen > 0);
    }

    #[test]
    fn h3o_places_dhaka_where_clickhouse_does() {
        let c = h3o::LatLng::new(23.8103, 90.4125).unwrap().to_cell(h3o::Resolution::Ten);
        let back = h3o::LatLng::from(c);
        assert!((back.lat() - 23.8107).abs() < 5e-4 && (back.lng() - 90.4122).abs() < 5e-4);
    }

    #[test]
    #[should_panic(expected = "is not a dotgrid table")]
    fn a_table_outside_the_list_is_refused() {
        create_cells("default.t");
    }
}
