//! Final review #5/#6: no tile of the real race archive trips the v2
//! decoders' trailing-bytes, repeated-index or header-id guards.
//!
//! `#[ignore]`d: it needs the real archive on disk,
//! `~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles` (or the path
//! in `GPUI_MAP_RACE_ARCHIVE`). It lives here, not beside
//! `pmtiles_source`, because that file must stay I/O-free (the browser
//! compiles it; `overlays_are_driven_through_the_host.rs` checks).
//!
//! Run: `cargo test --release --test race_archive_guards -- --ignored --nocapture`

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use gpui_maps::geo::TileId;
use gpui_maps::overlay::dotgrid::pmtiles_dir::zxy_to_tile_id;
use gpui_maps::overlay::dotgrid::pmtiles_source::{ArchiveOpener, PmtilesOpener};
use gpui_maps::overlay::dotgrid::{GridCountSource, GridError, GridRequest, GridTileKey};

/// Part tiles sampled; every cell tile is decoded.
const PART_SAMPLE: usize = 3000;

fn read(file: &mut File, req: &GridRequest) -> Vec<u8> {
    let GridRequest::Range { offset, length, .. } = req else { panic!("only Range requests, got {req:?}") };
    file.seek(SeekFrom::Start(*offset)).unwrap();
    let mut buf = Vec::new();
    file.take(*length).read_to_end(&mut buf).unwrap();
    buf
}

/// The PMTiles v3 Hilbert tile id, inverted. Checked against the forward
/// mapping (`zxy_to_tile_id`) for every id the test uses.
fn zxy(id: u64) -> (u8, u32, u32) {
    let (mut z, mut acc) = (0u8, 0u64);
    while id >= acc + (1u64 << (2 * z)) {
        acc += 1u64 << (2 * z);
        z += 1;
    }
    let (n, mut t) = (1u64 << z, id - acc);
    let (mut x, mut y, mut s) = (0u64, 0u64, 1u64);
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (z, x as u32, y as u32)
}

#[test]
fn the_hilbert_inverse_matches_the_forward_mapping() {
    for (z, x, y) in [(0, 0, 0), (1, 1, 0), (3, 5, 2), (9, 150, 192), (12, 1206, 1538), (12, 4095, 4095)] {
        assert_eq!(zxy(zxy_to_tile_id(z, x, y)), (z, x, y));
    }
}

#[test]
#[ignore = "needs the real race archive on disk"]
fn every_sampled_real_race_tile_passes_the_decoder_guards() {
    let path = std::env::var("GPUI_MAP_RACE_ARCHIVE")
        .unwrap_or_else(|_| format!("{}/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles", std::env::var("HOME").unwrap()));
    let mut file = File::open(&path).expect("the real race archive");

    // The source (descriptor, race key, decode) ...
    let mut opener = PmtilesOpener::new(path.clone());
    let first = read(&mut file, &opener.first_request());
    let bodies: Vec<Vec<u8>> = opener.after_first(&first).unwrap().iter().map(|r| read(&mut file, r)).collect();
    let source = opener.finish(&bodies).unwrap();
    // ... and its directory, which the source keeps private.
    let mut dir = ArchiveOpener::new(path.clone(), |_: &[u8], _: u8| Ok::<(), GridError>(()));
    let first = read(&mut file, &dir.first_request());
    let bodies: Vec<Vec<u8>> = dir.after_first(&first).unwrap().iter().map(|r| read(&mut file, r)).collect();
    let entries = dir.finish_archive(&bodies).unwrap().entries;

    let part_zoom = source.race().expect("a race archive").part_zoom;
    let epoch = source.descriptor().epochs[0];
    let stride = (entries.len() / PART_SAMPLE).max(1);
    let (mut parts, mut cells, mut failed) = (0usize, 0usize, Vec::new());
    for (i, e) in entries.iter().enumerate() {
        let (z, x, y) = zxy(e.tile_id);
        if z == part_zoom && i % stride != 0 {
            continue;
        }
        assert_eq!(zxy_to_tile_id(z, x, y), e.tile_id);
        let tile = TileId { z, x, y };
        let key = if z == part_zoom { GridTileKey::part(tile, epoch) } else { GridTileKey { tile, level: 0, epoch, epoch_b: None } };
        let body = read(&mut file, &GridRequest::Range { location: path.clone(), offset: e.offset, length: u64::from(e.length) });
        match source.decode(&[key], &body) {
            Ok(_) if key.is_part() => parts += 1,
            Ok(_) => cells += 1,
            Err(err) => failed.push((z, x, y, err)),
        }
    }
    eprintln!("{} entries, part stride {stride}: {parts} part tiles, {cells} cell tiles decoded, {} refused", entries.len(), failed.len());
    assert!(failed.is_empty(), "refused: {:?}", &failed[..failed.len().min(10)]);
    assert!(parts > 0 && cells > 0, "the sample must hold both kinds");
}
