use std::f64::consts::PI;

pub const TILE_SIZE: f64 = 512.0;

/// The Web Mercator equator, metres (`2 * pi * 6378137`).
pub const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.685_578_49;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LonLatBounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

/// Fractional Web Mercator tile coordinates (not floored to an integer tile).
pub fn lon_lat_to_tile_xy(lon: f64, lat: f64, zoom: f64) -> (f64, f64) {
    let n = 2f64.powf(zoom);
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.clamp(-85.05, 85.05).to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0 * n;
    (x, y)
}

/// Inverse of `lon_lat_to_tile_xy`.
pub fn tile_xy_to_lon_lat(x: f64, y: f64, zoom: f64) -> (f64, f64) {
    let n = 2f64.powf(zoom);
    let lon = x / n * 360.0 - 180.0;
    let lat_rad = (PI * (1.0 - 2.0 * y / n)).sinh().atan();
    (lon, lat_rad.to_degrees())
}

pub fn tile_id_at(lon: f64, lat: f64, zoom: u8) -> TileId {
    let (x, y) = lon_lat_to_tile_xy(lon, lat, zoom as f64);
    let n = 1u32 << zoom;
    TileId {
        z: zoom,
        // Longitude is a cylinder: it repeats every 360 degrees, so a
        // column index past the edge of the tile grid names a real tile on
        // the OTHER side -- x=-1 at z2 is the same column as x=3, not a
        // clamp to column 0. `rem_euclid` wraps it there.
        x: (x.floor() as i64).rem_euclid(n as i64) as u32,
        // Latitude has no such wrap -- there is nothing past a pole -- so y
        // stays clamped into the valid range.
        y: (y.floor() as i64).clamp(0, n as i64 - 1) as u32,
    }
}

pub fn tile_bounds_lonlat(tile: TileId) -> LonLatBounds {
    let (min_lon, max_lat) = tile_xy_to_lon_lat(tile.x as f64, tile.y as f64, tile.z as f64);
    let (max_lon, min_lat) = tile_xy_to_lon_lat(tile.x as f64 + 1.0, tile.y as f64 + 1.0, tile.z as f64);
    LonLatBounds { min_lon, min_lat, max_lon, max_lat }
}

pub fn visible_tiles(center: (f64, f64), zoom: u8, viewport_size_px: (f64, f64)) -> Vec<TileId> {
    let (center_lon, center_lat) = center;
    let (center_x, center_y) = lon_lat_to_tile_xy(center_lon, center_lat, zoom as f64);
    let half_tiles_x = (viewport_size_px.0 / TILE_SIZE) / 2.0;
    let half_tiles_y = (viewport_size_px.1 / TILE_SIZE) / 2.0;

    let n = 1i64 << zoom;
    let min_x = ((center_x - half_tiles_x).floor() as i64).clamp(0, n - 1);
    let max_x = ((center_x + half_tiles_x).ceil() as i64).clamp(0, n - 1);
    let min_y = ((center_y - half_tiles_y).floor() as i64).clamp(0, n - 1);
    let max_y = ((center_y + half_tiles_y).ceil() as i64).clamp(0, n - 1);

    let mut tiles = Vec::new();
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            tiles.push(TileId { z: zoom, x: x as u32, y: y as u32 });
        }
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_round_trips_through_lon_lat() {
        for (z, x, y) in [(0u8, 0u32, 0u32), (2, 1, 2), (5, 10, 11), (10, 500, 300)] {
            let tile = TileId { z, x, y };
            let bounds = tile_bounds_lonlat(tile);
            let center_lon = (bounds.min_lon + bounds.max_lon) / 2.0;
            let center_lat = (bounds.min_lat + bounds.max_lat) / 2.0;
            let recovered = tile_id_at(center_lon, center_lat, z);
            assert_eq!(recovered, tile, "round trip failed for {tile:?}");
        }
    }

    #[test]
    fn whole_world_is_one_tile_at_zoom_zero() {
        let tiles = visible_tiles((0.0, 0.0), 0, (100_000.0, 100_000.0));
        assert_eq!(tiles, vec![TileId { z: 0, x: 0, y: 0 }]);
    }

    #[test]
    fn visible_tile_count_matches_viewport_size() {
        // A viewport exactly TILE_SIZE x TILE_SIZE, centered on a tile's own center,
        // should need only that one tile (plus possibly its neighbors due to rounding
        // at the edges) — assert it's a small, non-empty set, not the whole pyramid.
        let tile = TileId { z: 6, x: 20, y: 20 };
        let bounds = tile_bounds_lonlat(tile);
        let center = (
            (bounds.min_lon + bounds.max_lon) / 2.0,
            (bounds.min_lat + bounds.max_lat) / 2.0,
        );
        let tiles = visible_tiles(center, 6, (TILE_SIZE, TILE_SIZE));
        assert!(!tiles.is_empty());
        assert!(tiles.len() <= 9, "expected a small tile set, got {}", tiles.len());
        assert!(tiles.contains(&tile));
    }

    #[test]
    fn tile_id_at_wraps_longitude_across_the_antimeridian() {
        // A longitude and the same meridian named ±360° apart (a full turn
        // around the globe) must resolve to the same tile column. Latitude
        // has no such wrap (there is nothing past a pole), only longitude.
        for z in [2u8, 5, 10] {
            for turns in [-2i64, -1, 0, 1, 2] {
                let lon = 73.2749 + 360.0 * turns as f64;
                assert_eq!(tile_id_at(lon, 22.2212, z), tile_id_at(73.2749, 22.2212, z));
            }
        }
    }

    #[test]
    fn visible_tiles_are_clamped_to_valid_range() {
        // Centering near the north pole at a low zoom must not panic or produce
        // out-of-range tile y values.
        let tiles = visible_tiles((0.0, 89.9), 2, (2000.0, 2000.0));
        let n = 1u32 << 2;
        for tile in tiles {
            assert!(tile.x < n && tile.y < n, "{tile:?} out of range for zoom 2");
        }
    }
}
