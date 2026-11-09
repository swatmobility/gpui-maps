//! Every SQL statement `load-clickhouse` sends (Task 9, spec §4.1), as
//! fixed strings: table names are literals and values go as ClickHouse
//! query parameters (`{res:UInt8}`), so [`TEMPLATES`] is the whole surface
//! and the tests below check all of it against the two global rules (no
//! `SELECT *`, no database but `gpui_map_dots`) and the `geoToH3(lat, lon)`
//! argument order.
//!
//! The load builds into `*_load` tables and only an `EXCHANGE TABLES` after
//! every check passes puts them live, so a failed load never touches the
//! tables already there.

/// The `race_parts_2020` schema: one row per block part (`parts.spill`),
/// its centroid as named `lat`/`lon`, and the 8 group counts.
macro_rules! create_parts {
    ($if_not:literal, $t:literal) => {
        concat!(
            "CREATE TABLE ",
            $if_not,
            "gpui_map_dots.",
            $t,
            " (\n",
            "    state       UInt8,\n",
            "    block_geoid UInt64 CODEC(Delta, ZSTD(1)),\n",
            "    part        UInt32 CODEC(T64, ZSTD(1)),\n",
            "    lat         Float64 CODEC(ZSTD(3)),\n",
            "    lon         Float64 CODEC(ZSTD(3)),\n",
            "    c1 UInt32 CODEC(T64, ZSTD(1)), c2 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c3 UInt32 CODEC(T64, ZSTD(1)), c4 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c5 UInt32 CODEC(T64, ZSTD(1)), c6 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c7 UInt32 CODEC(T64, ZSTD(1)), c8 UInt32 CODEC(T64, ZSTD(1))\n",
            ") ENGINE = MergeTree ORDER BY (state, block_geoid, part)"
        )
    };
}

/// The `race_hex_2020` schema: one row per populated H3 cell per
/// resolution, the 8 group sums and their total.
macro_rules! create_hex {
    ($if_not:literal, $t:literal) => {
        concat!(
            "CREATE TABLE ",
            $if_not,
            "gpui_map_dots.",
            $t,
            " (\n",
            "    res   UInt8,\n",
            "    h3    UInt64 CODEC(Delta, ZSTD(1)),\n",
            "    c1 UInt32 CODEC(T64, ZSTD(1)), c2 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c3 UInt32 CODEC(T64, ZSTD(1)), c4 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c5 UInt32 CODEC(T64, ZSTD(1)), c6 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    c7 UInt32 CODEC(T64, ZSTD(1)), c8 UInt32 CODEC(T64, ZSTD(1)),\n",
            "    total UInt32 CODEC(T64, ZSTD(1))\n",
            ") ENGINE = MergeTree ORDER BY (res, h3)"
        )
    };
}

pub const PARTS: &str = "race_parts_2020";
pub const HEX: &str = "race_hex_2020";
pub const PARTS_LOAD: &str = "race_parts_2020_load";
pub const HEX_LOAD: &str = "race_hex_2020_load";

// --- ownership and disk (the dotgrid-build loader's rules, same marker) ---

pub const DATABASE_EXISTS: &str = "SELECT count() FROM system.databases WHERE name = 'gpui_map_dots' FORMAT TabSeparated";
pub const OWNER_MARKER_EXISTS: &str =
    "SELECT count() FROM system.tables WHERE database = 'gpui_map_dots' AND name = '_dotgrid_owner' FORMAT TabSeparated";
/// No `IF NOT EXISTS`: only run after [`DATABASE_EXISTS`] said 0, so a
/// concurrent creator fails the load instead of being adopted.
pub const CREATE_DB: &str = "CREATE DATABASE gpui_map_dots";
pub const CREATE_OWNER: &str = "CREATE TABLE IF NOT EXISTS gpui_map_dots._dotgrid_owner (created DateTime) ENGINE = TinyLog";
pub const INSERT_OWNER: &str = "INSERT INTO gpui_map_dots._dotgrid_owner (created) VALUES (now())";
/// Tables in `gpui_map_dots` at all: 0 with no marker means a first load
/// crashed between [`CREATE_DB`] and [`CREATE_OWNER`], and the empty
/// database is adopted rather than refused as foreign.
pub const TABLES_IN_DB: &str = "SELECT count() FROM system.tables WHERE database = 'gpui_map_dots' FORMAT TabSeparated";
pub const DISKS: &str = "SELECT name, path, free_space FROM system.disks FORMAT TabSeparated";

// --- tables ---

pub const CREATE_PARTS: &str = create_parts!("IF NOT EXISTS ", "race_parts_2020");
pub const CREATE_HEX: &str = create_hex!("IF NOT EXISTS ", "race_hex_2020");
pub const CREATE_PARTS_LOAD: &str = create_parts!("", "race_parts_2020_load");
pub const CREATE_HEX_LOAD: &str = create_hex!("", "race_hex_2020_load");
pub const DROP_PARTS_LOAD: &str = "DROP TABLE IF EXISTS gpui_map_dots.race_parts_2020_load SYNC";
pub const DROP_HEX_LOAD: &str = "DROP TABLE IF EXISTS gpui_map_dots.race_hex_2020_load SYNC";
pub const OPTIMIZE_PARTS_LOAD: &str = "OPTIMIZE TABLE gpui_map_dots.race_parts_2020_load FINAL";
pub const OPTIMIZE_HEX_LOAD: &str = "OPTIMIZE TABLE gpui_map_dots.race_hex_2020_load FINAL";

/// The RowBinary stream's target; the row layout is `clickhouse_load::encode_row`.
pub const INSERT_PARTS_LOAD: &str = "INSERT INTO gpui_map_dots.race_parts_2020_load \
     (state, block_geoid, part, lat, lon, c1, c2, c3, c4, c5, c6, c7, c8) FORMAT RowBinary";

/// Spec §4.1 step 2, once per resolution (`{res:UInt8}` in 4..=8).
pub const INSERT_HEX_LOAD: &str = "INSERT INTO gpui_map_dots.race_hex_2020_load (res, h3, c1, c2, c3, c4, c5, c6, c7, c8, total)\n\
     SELECT {res:UInt8} AS res, geoToH3(p.lat, p.lon, {res:UInt8}) AS h3,\n\
     \x20      sum(p.c1), sum(p.c2), sum(p.c3), sum(p.c4), sum(p.c5), sum(p.c6), sum(p.c7), sum(p.c8),\n\
     \x20      sum(p.c1 + p.c2 + p.c3 + p.c4 + p.c5 + p.c6 + p.c7 + p.c8)\n\
     FROM gpui_map_dots.race_parts_2020_load AS p\n\
     GROUP BY h3";

// --- checks, all on the `_load` tables ---

/// Rows, the 8 group sums, their total, and rows whose centroid lies
/// outside the 50 states + DC's lat/lon box (a swapped or mis-projected
/// coordinate): lat 18..72, lon -180..-64 or 170..180 (the Aleutians).
pub const PARTS_SUMS: &str = "SELECT count(), sum(p.c1), sum(p.c2), sum(p.c3), sum(p.c4), sum(p.c5), sum(p.c6), sum(p.c7), sum(p.c8),\n\
     \x20      sum(p.c1 + p.c2 + p.c3 + p.c4 + p.c5 + p.c6 + p.c7 + p.c8),\n\
     \x20      countIf(p.lat < 18 OR p.lat > 72 OR (p.lon > -64 AND p.lon < 170))\n\
     FROM gpui_map_dots.race_parts_2020_load AS p FORMAT TabSeparated";

/// Per resolution: rows, rows with >= 10 people, the 8 group sums, the
/// total's sum, and rows whose `total` is not the sum of its groups.
pub const HEX_SUMS: &str = "SELECT h.res, count(), countIf(h.total >= 10),\n\
     \x20      sum(h.c1), sum(h.c2), sum(h.c3), sum(h.c4), sum(h.c5), sum(h.c6), sum(h.c7), sum(h.c8), sum(h.total),\n\
     \x20      countIf(h.total != h.c1 + h.c2 + h.c3 + h.c4 + h.c5 + h.c6 + h.c7 + h.c8)\n\
     FROM gpui_map_dots.race_hex_2020_load AS h GROUP BY h.res ORDER BY h.res FORMAT TabSeparated";

/// The round-trip gate (MG-R9): every part's r8 cell centre from
/// `h3ToGeo` (which returns `(lat, lon)`) must lie within 1.5 km of the
/// part (the r8 edge bound), and that cell must be a row of the r8
/// aggregation. Returns parts checked, parts over 1.5 km, parts whose cell
/// is missing, and the largest distance in metres. A `geoToH3(lon, lat)`
/// swap still returns a valid cell, thousands of km away.
pub const ROUND_TRIP_GATE: &str = "SELECT count(), countIf(greatCircleDistance(lon, lat, g.2, g.1) > 1500),\n\
     \x20      countIf(h3 NOT IN (SELECT h.h3 FROM gpui_map_dots.race_hex_2020_load AS h WHERE h.res = 8)),\n\
     \x20      max(greatCircleDistance(lon, lat, g.2, g.1))\n\
     FROM (\n\
     \x20 SELECT p.lat AS lat, p.lon AS lon, geoToH3(p.lat, p.lon, 8) AS h3, h3ToGeo(h3) AS g\n\
     \x20 FROM gpui_map_dots.race_parts_2020_load AS p\n\
     ) FORMAT TabSeparated";

/// 1,000 parts spread by hash, with the server's cell at every resolution,
/// for the `h3o` cross-check.
pub const H3_SAMPLE: &str = "SELECT p.lat, p.lon, geoToH3(p.lat, p.lon, 4), geoToH3(p.lat, p.lon, 5), geoToH3(p.lat, p.lon, 6),\n\
     \x20      geoToH3(p.lat, p.lon, 7), geoToH3(p.lat, p.lon, 8)\n\
     FROM gpui_map_dots.race_parts_2020_load AS p\n\
     ORDER BY cityHash64(p.block_geoid, p.part) LIMIT 1000 FORMAT TabSeparated";

/// Active parts of `{t:String}` in `gpui_map_dots`: rows, bytes on disk,
/// uncompressed bytes.
pub const TABLE_BYTES: &str = "SELECT sum(rows), sum(bytes_on_disk), sum(data_uncompressed_bytes) FROM system.parts \
     WHERE database = 'gpui_map_dots' AND table = {t:String} AND active FORMAT TabSeparated";

// --- build stamps (table comments; they travel with a table through EXCHANGE) ---

/// `{build}` (a u64) and `{state}` (`building` or `validated`) are filled by
/// [`stamp`]: ClickHouse takes no query parameter in `COMMENT`.
pub const STAMP_PARTS_LOAD: &str =
    "ALTER TABLE gpui_map_dots.race_parts_2020_load MODIFY COMMENT 'race-build build={build} state={state}'";
pub const STAMP_HEX_LOAD: &str =
    "ALTER TABLE gpui_map_dots.race_hex_2020_load MODIFY COMMENT 'race-build build={build} state={state}'";
/// Name and comment of whichever of the four race tables exist.
pub const TABLE_COMMENTS: &str = "SELECT name, comment FROM system.tables WHERE database = 'gpui_map_dots' \
     AND name IN ('race_parts_2020', 'race_hex_2020', 'race_parts_2020_load', 'race_hex_2020_load') FORMAT TabSeparated";

/// A stamp statement with its build id and state filled in. Only digits
/// and one of two fixed words reach the SQL.
pub fn stamp(template: &str, build: u64, validated: bool) -> String {
    template.replace("{build}", &build.to_string()).replace("{state}", if validated { "validated" } else { "building" })
}

/// `(build, validated)` from a table comment written by [`stamp`].
pub fn parse_stamp(comment: &str) -> Option<(u64, bool)> {
    let rest = comment.trim().strip_prefix("race-build build=")?;
    let (build, state) = rest.split_once(" state=")?;
    let validated = match state {
        "validated" => true,
        "building" => false,
        _ => return None,
    };
    Some((build.parse().ok()?, validated))
}

// --- the swap ---

pub const EXCHANGE_PARTS: &str = "EXCHANGE TABLES gpui_map_dots.race_parts_2020_load AND gpui_map_dots.race_parts_2020";
pub const EXCHANGE_HEX: &str = "EXCHANGE TABLES gpui_map_dots.race_hex_2020_load AND gpui_map_dots.race_hex_2020";
pub const COUNT_PARTS: &str = "SELECT count() FROM gpui_map_dots.race_parts_2020 FORMAT TabSeparated";
pub const COUNT_HEX: &str = "SELECT count() FROM gpui_map_dots.race_hex_2020 FORMAT TabSeparated";

/// Every statement above.
/// `export-hex` (Task 10), read-only: one resolution's row count, rows of
/// at least 10 people, the 8 group sums and the total.
pub const HEX_EXPORT_COUNTS: &str = "SELECT count(), countIf(h.total >= 10), sum(h.c1), sum(h.c2), sum(h.c3), sum(h.c4), \
sum(h.c5), sum(h.c6), sum(h.c7), sum(h.c8), sum(h.total)\n\
FROM gpui_map_dots.race_hex_2020 AS h WHERE h.res = {res:UInt8} FORMAT TabSeparated";
/// `export-hex`: every cell of one resolution (the < 10 ones too: their
/// people are carried as each tile's hidden counts), in H3 order.
pub const HEX_EXPORT_ROWS: &str = "SELECT h.h3, h.c1, h.c2, h.c3, h.c4, h.c5, h.c6, h.c7, h.c8\n\
FROM gpui_map_dots.race_hex_2020 AS h WHERE h.res = {res:UInt8} ORDER BY h.h3 FORMAT RowBinary";
/// `export-hex`'s cross-check of the baked boundaries: ClickHouse's own
/// `h3ToGeoBoundary` for a 1-in-500 sample of one resolution's cells.
pub const HEX_BOUNDARY_SAMPLE: &str = "SELECT h.h3, h3ToGeoBoundary(h.h3)\n\
FROM gpui_map_dots.race_hex_2020 AS h WHERE h.res = {res:UInt8} AND cityHash64(h.h3) % 500 = 0\n\
ORDER BY h.h3 FORMAT TabSeparated";

pub const TEMPLATES: &[&str] = &[
    DATABASE_EXISTS,
    OWNER_MARKER_EXISTS,
    CREATE_DB,
    CREATE_OWNER,
    INSERT_OWNER,
    TABLES_IN_DB,
    DISKS,
    CREATE_PARTS,
    CREATE_HEX,
    CREATE_PARTS_LOAD,
    CREATE_HEX_LOAD,
    DROP_PARTS_LOAD,
    DROP_HEX_LOAD,
    OPTIMIZE_PARTS_LOAD,
    OPTIMIZE_HEX_LOAD,
    INSERT_PARTS_LOAD,
    INSERT_HEX_LOAD,
    PARTS_SUMS,
    HEX_SUMS,
    ROUND_TRIP_GATE,
    H3_SAMPLE,
    TABLE_BYTES,
    STAMP_PARTS_LOAD,
    STAMP_HEX_LOAD,
    TABLE_COMMENTS,
    EXCHANGE_PARTS,
    EXCHANGE_HEX,
    COUNT_PARTS,
    COUNT_HEX,
    HEX_EXPORT_COUNTS,
    HEX_EXPORT_ROWS,
    HEX_BOUNDARY_SAMPLE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_maps::overlay::dotgrid::sql_guard::{names_foreign_database, star_select_in};

    #[test]
    fn no_template_selects_star_or_names_another_database() {
        for t in TEMPLATES {
            assert!(!star_select_in(t), "{t}");
            assert!(!names_foreign_database(t, "gpui_map_dots"), "{t}");
        }
    }

    #[test]
    fn geo_to_h3_is_called_with_named_lat_then_lon() {
        for t in TEMPLATES.iter().filter(|t| t.contains("geoToH3")) {
            assert!(t.contains("geoToH3(p.lat, p.lon,"), "{t}");
            assert!(!t.contains("geoToH3(p.lon"), "{t}");
        }
    }

    /// Review round 1: the guard now reads both operands of `EXCHANGE
    /// TABLES`, so the swap statements are really covered by the test above.
    #[test]
    fn the_exchange_statements_are_checked_by_the_guard() {
        for t in [EXCHANGE_PARTS, EXCHANGE_HEX] {
            assert!(!names_foreign_database(t, "gpui_map_dots"), "{t}");
            let foreign = t.replacen("gpui_map_dots.", "default.", 1);
            assert!(names_foreign_database(&foreign, "gpui_map_dots"), "{foreign}");
            let foreign = t.replace("AND gpui_map_dots.", "AND pop.");
            assert!(names_foreign_database(&foreign, "gpui_map_dots"), "{foreign}");
        }
    }

    #[test]
    fn a_stamp_round_trips_and_fills_only_digits_and_a_state() {
        let s = stamp(STAMP_PARTS_LOAD, 1_790_000_000_123, true);
        assert!(s.ends_with("COMMENT 'race-build build=1790000000123 state=validated'"), "{s}");
        assert!(!names_foreign_database(&s, "gpui_map_dots") && !star_select_in(&s));
        let comment = s.split('\'').nth(1).unwrap();
        assert_eq!(parse_stamp(comment), Some((1_790_000_000_123, true)));
        assert_eq!(parse_stamp(&stamp("race-build build={build} state={state}", 7, false)), Some((7, false)));
        assert_eq!(parse_stamp(""), None);
        assert_eq!(parse_stamp("race-build build=x state=validated"), None);
    }

    /// The aggregation, the gate and the sample all call `geoToH3`, so the
    /// order test above is not vacuous; and every table name the loader
    /// writes is one of the four race tables.
    #[test]
    fn the_templates_cover_the_aggregation_and_the_checks() {
        assert!(TEMPLATES.iter().filter(|t| t.contains("geoToH3")).count() >= 3);
        assert!(INSERT_HEX_LOAD.contains("FROM gpui_map_dots.race_parts_2020_load AS p"));
        for t in [PARTS, HEX, PARTS_LOAD, HEX_LOAD] {
            assert!(TEMPLATES.iter().any(|s| s.contains(&format!("gpui_map_dots.{t} "))), "{t}");
        }
        assert!(CREATE_PARTS.contains("ORDER BY (state, block_geoid, part)"));
        assert!(CREATE_HEX.contains("ORDER BY (res, h3)"));
    }
}
