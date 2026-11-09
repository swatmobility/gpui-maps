use std::io::Write as _;

use super::*;

/// A real OpenFreeMap z14 tile (Singapore, 123,602 bytes raw; ODbL, see
/// `benches/fixtures/README.md`). Gzipped into the fixture as every real
/// MBTiles writer stores vector tiles, so the fixture is ~50 KB.
const SINGAPORE: &[u8] = include_bytes!("../../../benches/fixtures/singapore_z14.pbf");
const SINGAPORE_TILE: TileId = TileId { z: 14, x: 12918, y: 8134 };

/// The smallest valid PNG header prefix, enough for the sniffer and for a
/// byte-for-byte round trip.
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}

/// An MBTiles 1.3 file as a writer lays it out: the two tables, the unique
/// tile index, rows stored TMS (`y` flipped) unless `xyz_rows`.
fn write(path: &Path, metadata: &[(&str, &str)], tiles: &[(TileId, Vec<u8>)], xyz_rows: bool) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE metadata (name text, value text);
         CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob);
         CREATE UNIQUE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row);",
    )
    .unwrap();
    for (name, value) in metadata {
        conn.execute("INSERT INTO metadata VALUES (?1, ?2)", (name, value)).unwrap();
    }
    for (tile, bytes) in tiles {
        let row = if xyz_rows { tile.y } else { (1u32 << tile.z) - 1 - tile.y };
        conn.execute("INSERT INTO tiles VALUES (?1, ?2, ?3, ?4)", (tile.z, tile.x, row, bytes)).unwrap();
    }
}

const VECTOR_META: &[(&str, &str)] = &[
    ("name", "Singapore sample"),
    ("format", "pbf"),
    ("bounds", "103.8428,1.2524,103.8647,1.2743"),
    ("center", "103.85,1.26,14"),
    ("minzoom", "14"),
    ("maxzoom", "14"),
    ("attribution", r#"<a href="https://www.openmaptiles.org/">&copy; OpenMapTiles</a> <a href="https://www.openstreetmap.org/copyright">&copy; OpenStreetMap contributors</a>"#),
    ("json", r#"{"vector_layers":[{"id":"building","minzoom":13},{"id":"water","minzoom":0}]}"#),
];

fn singapore_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("singapore.mbtiles");
    write(&path, VECTOR_META, &[(SINGAPORE_TILE, gzip(SINGAPORE))], false);
    (dir, path)
}

#[test]
fn the_fixture_stays_small() {
    let (_dir, path) = singapore_fixture();
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len < 1_000_000, "fixture is {len} bytes");
}

#[tokio::test]
async fn a_gzipped_vector_tile_comes_back_decompressed_at_its_xyz_address() {
    let (_dir, path) = singapore_fixture();
    let source = MbtilesSource::open(&path).await.unwrap();
    let bytes = source.fetch(SINGAPORE_TILE).await.unwrap().expect("the tile is in the file");
    assert_eq!(bytes, SINGAPORE, "gunzipped back to the original MVT");
    // And it is a tile the decoder reads.
    let tile = crate::mvt::decode(bytes).expect("decodes");
    assert!(tile.layers.iter().any(|l| l.name == "building"), "has buildings");
}

#[tokio::test]
async fn the_metadata_is_read_once_at_open() {
    let (_dir, path) = singapore_fixture();
    let source = MbtilesSource::open(&path).await.unwrap();
    let m = source.metadata();
    assert_eq!(m.name.as_deref(), Some("Singapore sample"));
    assert_eq!(m.format, MbtilesFormat::Pbf);
    assert!(m.format.is_vector());
    assert_eq!((m.min_zoom, m.max_zoom), (14, 14));
    let b = m.bounds.expect("bounds");
    assert_eq!((b.min_lon, b.min_lat, b.max_lon, b.max_lat), (103.8428, 1.2524, 103.8647, 1.2743));
    assert_eq!(m.center, Some((103.85, 1.26, Some(14))));
    assert_eq!(m.vector_layers, ["building", "water"]);
    assert_eq!(source.max_zoom(), 14);
    assert_eq!(source.attribution(), Some("\u{a9} OpenMapTiles \u{a9} OpenStreetMap contributors"));
    assert_eq!(source.layer_min_zoom("building"), Some(13));
    assert_eq!(source.layer_min_zoom("poi"), None);
    let c = source.coverage();
    assert_eq!((c.center_lon, c.center_lat, c.center_zoom), (103.85, 1.26, Some(14)));
    assert_eq!(c.opening_camera(800.0, 600.0), (103.85, 1.26, 14.0));
}

/// The credit reaches the engine's credit list as the basemap's, with no
/// caller involvement.
#[tokio::test]
async fn the_attribution_reaches_the_credit_list() {
    let (_dir, path) = singapore_fixture();
    let source = MbtilesSource::open(&path).await.unwrap();
    let credits = crate::attribution::collect(
        source.attribution().map(|c| crate::attribution::Credit::new(crate::attribution::CreditRole::Basemap, c)),
    );
    assert_eq!(credits.len(), 1);
    assert!(credits[0].text.contains("OpenStreetMap contributors"));
}

#[tokio::test]
async fn rows_are_tms_unless_the_file_says_xyz() {
    // Two tiles at z1 in the same column: XYZ y=0 (north) and y=1 (south).
    let north = TileId { z: 1, x: 0, y: 0 };
    let south = TileId { z: 1, x: 0, y: 1 };
    let tiles = [(north, b"north".to_vec()), (south, b"south".to_vec())];
    for (scheme, xyz_rows) in [(None, false), (Some("xyz"), true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mbtiles");
        let mut meta = vec![("format", "png"), ("minzoom", "0"), ("maxzoom", "1")];
        if let Some(s) = scheme {
            meta.push(("scheme", s));
        }
        write(&path, &meta, &tiles, xyz_rows);
        let source = MbtilesSource::open(&path).await.unwrap();
        assert_eq!(source.fetch(north).await.unwrap().as_deref(), Some(&b"north"[..]), "scheme {scheme:?}");
        assert_eq!(source.fetch(south).await.unwrap().as_deref(), Some(&b"south"[..]), "scheme {scheme:?}");
    }
}

#[tokio::test]
async fn a_tile_the_file_does_not_hold_is_absent_and_a_bad_coordinate_is_an_error() {
    let (_dir, path) = singapore_fixture();
    let source = MbtilesSource::open(&path).await.unwrap();
    let neighbour = TileId { z: 14, x: SINGAPORE_TILE.x + 1, y: SINGAPORE_TILE.y };
    assert!(matches!(source.fetch(neighbour).await, Ok(None)));
    assert!(source.fetch(TileId { z: 2, x: 4, y: 0 }).await.is_err());
}

/// Out of the zoom range or bounds, a tile is absent without a query --
/// even when a row for it exists (a writer that stated narrower bounds or
/// zooms than it wrote is taken at its word).
#[tokio::test]
async fn the_stated_zoom_range_and_bounds_are_respected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.mbtiles");
    let far = TileId { z: 14, x: 0, y: 0 };
    let low = TileId { z: 13, x: SINGAPORE_TILE.x / 2, y: SINGAPORE_TILE.y / 2 };
    write(&path, VECTOR_META, &[(SINGAPORE_TILE, gzip(SINGAPORE)), (far, PNG.to_vec()), (low, PNG.to_vec())], false);
    let source = MbtilesSource::open(&path).await.unwrap();
    assert!(source.fetch(SINGAPORE_TILE).await.unwrap().is_some());
    assert!(source.fetch(far).await.unwrap().is_none(), "outside the bounds");
    assert!(source.fetch(low).await.unwrap().is_none(), "below minzoom");
}

/// A city cut from a country file that kept the country's metadata
/// (`minzoom 0`, centre at the country's middle, z1), holding only z15
/// tiles -- the real `munich-15.mbtiles` shape. The zoom range narrows to
/// what is stored and the camera frames the tiles, not an empty country.
#[tokio::test]
async fn overstated_metadata_is_narrowed_to_the_tiles_the_file_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("munich.mbtiles");
    let tiles: Vec<(TileId, Vec<u8>)> =
        (17421..=17423).flat_map(|x| (11360..=11362).map(move |y| (TileId { z: 15, x, y }, PNG.to_vec()))).collect();
    let meta = [("format", "png"), ("minzoom", "0"), ("maxzoom", "15"), ("bounds", "5.845,47.25,15.06,55.12"), ("center", "10.5,51.2,1")];
    write(&path, &meta, &tiles, false);
    let source = MbtilesSource::open(&path).await.unwrap();
    let m = source.metadata();
    assert_eq!((m.min_zoom, m.max_zoom), (15, 15));
    let data = m.data_bounds.expect("data extent");
    let c = source.coverage();
    assert!((data.min_lon..=data.max_lon).contains(&c.center_lon) && (data.min_lat..=data.max_lat).contains(&c.center_lat), "{c:?}");
    assert!((11.3..11.45).contains(&c.center_lon) && (48.1..48.3).contains(&c.center_lat), "Munich, not Germany: {c:?}");
    let (lon, lat, zoom) = c.opening_camera(800.0, 600.0);
    assert!(zoom >= 15.0, "{zoom}");
    assert!(tiles.iter().any(|(t, _)| *t == crate::geo::tile_id_at(lon, lat, 15)), "the camera centres on a stored tile");
}

/// A raster file: bytes come back exactly as stored. With no `format`,
/// `minzoom` or `maxzoom` rows the format is sniffed and the range read off
/// the tiles; with no `bounds` the coverage is the world.
#[tokio::test]
async fn a_raster_file_without_metadata_is_sniffed_and_served_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.mbtiles");
    let tiles = [(TileId { z: 3, x: 1, y: 2 }, PNG.to_vec()), (TileId { z: 5, x: 7, y: 9 }, PNG.to_vec())];
    write(&path, &[], &tiles, false);
    let source = MbtilesSource::open(&path).await.unwrap();
    let m = source.metadata();
    assert_eq!(m.format, MbtilesFormat::Png);
    assert!(!m.format.is_vector());
    assert_eq!((m.min_zoom, m.max_zoom), (3, 5));
    assert!(m.bounds.is_none() && m.name.is_none());
    assert_eq!(source.attribution(), None);
    assert_eq!(source.fetch(tiles[1].0).await.unwrap().as_deref(), Some(PNG));
    let c = source.coverage();
    // No bounds stated, so the world -- whose middle holds nothing here; the
    // coverage is the stored tiles at the lowest zoom (3/1/2) instead.
    assert_eq!((c.min_lon, c.max_lon), (-135.0, -90.0));
}

#[tokio::test]
async fn a_file_that_is_not_mbtiles_is_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let not_sqlite = dir.path().join("a.mbtiles");
    std::fs::write(&not_sqlite, b"definitely not a database, just text padding it out ....").unwrap();
    assert!(MbtilesSource::open(&not_sqlite).await.is_err());

    let no_tiles = dir.path().join("b.mbtiles");
    Connection::open(&no_tiles).unwrap().execute_batch("CREATE TABLE metadata (name text, value text);").unwrap();
    let err = MbtilesSource::open(&no_tiles).await.err().expect("refused").to_string();
    assert!(err.contains("not an MBTiles file"), "{err}");

    let missing = dir.path().join("missing.mbtiles");
    assert!(MbtilesSource::open(&missing).await.is_err());
    assert!(!missing.exists(), "a read-only open never creates the file");
}

/// The OpenMapTiles layout: `tiles` is a VIEW over `map` + `images`
/// (deduplicated blobs). The same query reads it.
#[tokio::test]
async fn a_deduplicating_writers_view_reads_like_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dedup.mbtiles");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE metadata (name text, value text);
         CREATE TABLE map (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_id TEXT);
         CREATE TABLE images (tile_data blob, tile_id text);
         CREATE VIEW tiles AS SELECT map.zoom_level AS zoom_level, map.tile_column AS tile_column,
           map.tile_row AS tile_row, images.tile_data AS tile_data FROM map JOIN images ON images.tile_id = map.tile_id;
         INSERT INTO metadata VALUES ('format', 'png');
         INSERT INTO images VALUES (x'89504e470d0a1a0a', 'a');
         INSERT INTO map VALUES (2, 1, 1, 'a');
         INSERT INTO map VALUES (2, 2, 1, 'a');",
    )
    .unwrap();
    drop(conn);
    let source = MbtilesSource::open(&path).await.unwrap();
    assert_eq!(source.metadata().max_zoom, 2);
    // TMS row 1 at z2 is XYZ y = 4 - 1 - 1 = 2.
    for x in [1, 2] {
        assert!(source.fetch(TileId { z: 2, x, y: 2 }).await.unwrap().is_some());
    }
}

/// Many fetches at once, from many tasks on a multi-thread runtime: every
/// one gets its own tile, and the pool never keeps more than its bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fetches_each_get_their_own_tile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grid.mbtiles");
    let tiles: Vec<(TileId, Vec<u8>)> = (0..16u32)
        .flat_map(|x| (0..16u32).map(move |y| (TileId { z: 4, x, y }, gzip(format!("tile {x}/{y}").as_bytes()))))
        .collect();
    write(&path, &[("format", "pbf"), ("minzoom", "4"), ("maxzoom", "4")], &tiles, false);
    let source = Arc::new(MbtilesSource::open(&path).await.unwrap());
    let handles: Vec<_> = tiles
        .iter()
        .map(|(tile, _)| {
            let (source, tile) = (source.clone(), *tile);
            tokio::spawn(async move { (tile, source.fetch(tile).await) })
        })
        .collect();
    for h in handles {
        let (tile, got) = h.await.unwrap();
        assert_eq!(got.unwrap().unwrap(), format!("tile {}/{}", tile.x, tile.y).into_bytes());
    }
    assert!(source.pool.idle.lock().unwrap().len() <= MAX_IDLE_CONNECTIONS);
}

/// Outside a tokio runtime the read runs inline rather than panicking.
#[test]
fn fetch_works_outside_a_tokio_runtime() {
    let (_dir, path) = singapore_fixture();
    let source = MbtilesSource::open_blocking(&path).unwrap();
    let got = pollster::block_on(source.fetch(SINGAPORE_TILE)).unwrap();
    assert_eq!(got.as_deref(), Some(SINGAPORE));
}

#[test]
fn zlib_and_plain_tiles_are_handled() {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(SINGAPORE).unwrap();
    assert_eq!(decompress(z.finish().unwrap()).unwrap(), SINGAPORE);
    assert_eq!(decompress(SINGAPORE.to_vec()).unwrap(), SINGAPORE);
}

#[test]
fn formats_parse_and_sniff() {
    assert_eq!(MbtilesFormat::parse("PBF"), MbtilesFormat::Pbf);
    assert_eq!(MbtilesFormat::parse("mvt"), MbtilesFormat::Pbf);
    assert_eq!(MbtilesFormat::parse("jpeg").as_str(), "jpg");
    assert_eq!(MbtilesFormat::parse("tiff"), MbtilesFormat::Other("tiff".into()));
    assert_eq!(MbtilesFormat::sniff(PNG), MbtilesFormat::Png);
    assert_eq!(MbtilesFormat::sniff(&[0xff, 0xd8, 0xff, 0xe0]), MbtilesFormat::Jpeg);
    assert_eq!(MbtilesFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "), MbtilesFormat::Webp);
    assert_eq!(MbtilesFormat::sniff(&gzip(b"x")), MbtilesFormat::Pbf);
}
