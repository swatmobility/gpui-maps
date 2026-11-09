//! PMTiles v3 header and directory parsing, from scratch, with no
//! dependency on the `pmtiles` crate: `web/` and `ios/` include this whole
//! module (see `overlay::dotgrid`'s own doc comment) and neither links
//! `pmtiles` (its `mmap-async-tokio` backend and `objc2` chain are
//! native-only -- see the root `Cargo.toml`'s comment on that dependency).
//!
//! Every read here is bounds-checked: a short buffer is
//! [`GridError::Truncated`], never a panic, matching `codec`'s own
//! contract (`the review focus's` "a corrupt, truncated or foreign archive
//! ... must be an error for that tile, never a panic").
//!
//! Layout and field order follow the PMTiles v3 spec exactly as the
//! `pmtiles` crate's own (private) `Header`/`Directory` types read them --
//! see that crate's `src/header.rs` and `src/directory.rs` for the
//! reference this was checked against.

use super::GridError;

/// The fixed-size header every v3 archive starts with.
pub const HEADER_SIZE: usize = 127;

const MAGIC: &[u8; 7] = b"PMTiles";

/// The fields of a v3 header this crate actually needs. See the PMTiles v3
/// spec for the rest (`data_length`, the three `n_*` counters, `clustered`,
/// the bounding box and centre) -- none of them are read by anything in
/// `dotgrid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmHeader {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub internal_compression: u8,
    pub tile_compression: u8,
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// Parses the first [`HEADER_SIZE`] bytes of a v3 archive. `first_127` may
/// be longer (a caller handing over a whole first-fetch buffer); only the
/// header-sized prefix is read.
pub fn parse_header(first_127: &[u8]) -> Result<PmHeader, GridError> {
    if first_127.len() < HEADER_SIZE {
        return Err(GridError::Truncated);
    }
    if &first_127[0..7] != MAGIC {
        return Err(GridError::BadMagic);
    }
    let u64_at = |off: usize| u64::from_le_bytes(first_127[off..off + 8].try_into().expect("8-byte window"));

    Ok(PmHeader {
        // byte 7 is the version, unread: any v3-shaped magic is accepted
        // regardless of the minor version stamped there.
        root_offset: u64_at(8),
        root_length: u64_at(16),
        metadata_offset: u64_at(24),
        metadata_length: u64_at(32),
        leaf_offset: u64_at(40),
        leaf_length: u64_at(48),
        data_offset: u64_at(56),
        // byte 64..96 holds data_length and the three n_* counters, unread.
        // byte 96 is `clustered`, unread.
        internal_compression: first_127[97],
        tile_compression: first_127[98],
        tile_type: first_127[99],
        min_zoom: first_127[100],
        max_zoom: first_127[101],
        // byte 102..127 holds the bounding box and centre, unread.
    })
}

/// One entry in a root or leaf directory: either a real tile
/// (`run_length` at least 1) or a pointer to a child leaf directory
/// (`run_length == 0`, per the spec -- `offset`/`length` then locate the
/// child directory's own compressed bytes rather than a tile's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// Inflates `bytes` per the header's own `internal_compression`. `1`
/// (none) and `2` (gzip) are the only values a `dotgrid` archive may use
/// (see `overlay::dotgrid`'s module doc); anything else is refused rather
/// than guessed at.
pub fn decompress(bytes: &[u8], compression: u8) -> Result<Vec<u8>, GridError> {
    match compression {
        1 => Ok(bytes.to_vec()),
        2 => super::codec::inflate_capped(bytes, super::codec::MAX_INFLATED_BYTES),
        other => Err(GridError::Server(format!("unsupported internal_compression {other}"))),
    }
}

/// Parses one (root or leaf) directory: a varint entry count, delta-varint
/// tile ids (first absolute), then a varint run-length, a varint length
/// and a varint offset per entry -- offset `0` meaning "immediately after
/// the previous entry's bytes" (`prev.offset + prev.length`), any other
/// value meaning the literal offset minus one. `bytes` is compressed with
/// `compression` (the header's `internal_compression`); this decompresses
/// it itself, exactly as [`decompress`] would.
pub fn parse_directory(bytes: &[u8], compression: u8) -> Result<Vec<PmEntry>, GridError> {
    let raw = decompress(bytes, compression)?;
    let mut c = Cursor::new(&raw);
    // `usize::try_from`, not `as usize`: on a 32-bit target (this parser
    // compiles for `wasm32-unknown-unknown`) a raw `as` cast SILENTLY
    // truncates a huge varint to a small, wrong `usize` instead of
    // erroring -- the exact failure mode `u32::try_from` below is also
    // guarding against, one size class up.
    let n = usize::try_from(c.varint()?).map_err(|_| GridError::Truncated)?;

    // Every entry costs at least one byte in EACH of the four varint
    // columns below (a LEB128 varint's minimum encoding is one byte), so a
    // directory claiming more entries than the remaining bytes could
    // possibly hold four bytes for is corrupt. Caught here, before
    // allocating four `n`-sized `Vec`s for a huge, attacker-controlled
    // `n` -- otherwise a single crafted leading varint could ask this
    // function to allocate gigabytes before the first real bounds check
    // ever runs.
    let remaining = raw.len().saturating_sub(c.pos);
    if n > remaining / 4 {
        return Err(GridError::Truncated);
    }

    let mut ids = Vec::with_capacity(n);
    let mut tile_id = 0u64;
    for i in 0..n {
        let delta = c.varint()?;
        tile_id = if i == 0 {
            delta
        } else {
            tile_id.checked_add(delta).ok_or(GridError::Truncated)?
        };
        ids.push(tile_id);
    }

    let mut run_lengths = Vec::with_capacity(n);
    for _ in 0..n {
        run_lengths.push(u32::try_from(c.varint()?).map_err(|_| GridError::Truncated)?);
    }

    let mut lengths = Vec::with_capacity(n);
    for _ in 0..n {
        lengths.push(u32::try_from(c.varint()?).map_err(|_| GridError::Truncated)?);
    }

    let mut entries = Vec::with_capacity(n);
    let mut last: Option<(u64, u32)> = None;
    for i in 0..n {
        let wire = c.varint()?;
        let offset = if wire == 0 {
            let (prev_offset, prev_length) = last.ok_or(GridError::Truncated)?;
            prev_offset
                .checked_add(u64::from(prev_length))
                .ok_or(GridError::Truncated)?
        } else {
            wire - 1
        };
        last = Some((offset, lengths[i]));
        entries.push(PmEntry {
            tile_id: ids[i],
            offset,
            length: lengths[i],
            run_length: run_lengths[i],
        });
    }
    Ok(entries)
}

/// The v3 spec's tile id: a Hilbert-curve index within zoom `z`'s
/// `2^z x 2^z` grid, offset by the number of tiles at every shallower
/// zoom (`(4^z - 1) / 3`) so ids are globally increasing zoom-major. `z ==
/// 0` is the single tile at the root, id `0`.
///
/// The Hilbert step (`hilbert_xy2d`/`rotate`) is the textbook
/// quadrant-rotation algorithm, not the `pmtiles` crate's own
/// (`fast_hilbert`'s bit-sliced LUT form) -- the two were checked to agree
/// bit for bit across 2,400 pseudo-random `(order, x, y)` triples at
/// orders 1..=12 before writing this, and `pmtiles_source`'s own
/// `hilbert_ids_match_the_writer` test pins the same agreement against
/// the real crate for 1,000 more.
pub fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    pyramid_size(z) + hilbert_xy2d(z, x, y)
}

/// `sum_{i=0}^{z-1} 4^i`, the count of tiles at every zoom shallower than
/// `z`. Accumulated rather than computed as `(4^z - 1) / 3` so it never
/// depends on `4u64.pow(z)` overflowing for a `z` this format does not
/// support anyway (the spec's own ceiling is z31; every level this crate
/// ever asks for is z <= 9).
fn pyramid_size(z: u8) -> u64 {
    let mut total = 0u64;
    let mut p = 1u64;
    for _ in 0..z {
        total += p;
        p *= 4;
    }
    total
}

/// The classic quadrant-rotation Hilbert curve (`x, y) -> d` mapping.
fn hilbert_xy2d(order: u8, mut x: u32, mut y: u32) -> u64 {
    let n: u32 = 1 << order;
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u32::from((x & s) > 0);
        let ry = u32::from((y & s) > 0);
        d += u64::from(s) * u64::from(s) * u64::from((3 * rx) ^ ry);
        rotate(n, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    d
}

/// Rotates (and, for the `rx == 0` quadrants, reflects and swaps) the
/// coordinate frame one quadrant-step of the curve, exactly as the
/// textbook algorithm does.
fn rotate(n: u32, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

/// A cursor over an already-inflated directory buffer, whose every read is
/// bounds checked -- a short read is [`GridError::Truncated`], never a
/// panic. Deliberately not shared with `codec`'s own private `Reader`:
/// that type is `codec`'s alone, and duplicating six lines here is cheaper
/// than making it `pub(crate)` across an otherwise unrelated format.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, GridError> {
        let b = *self.bytes.get(self.pos).ok_or(GridError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    /// LEB128 unsigned varint.
    fn varint(&mut self) -> Result<u64, GridError> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift >= 64 {
                return Err(GridError::Truncated);
            }
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_buffer_is_truncated_not_a_panic() {
        for cut in 0..HEADER_SIZE {
            assert!(matches!(parse_header(&vec![0u8; cut]), Err(GridError::Truncated)));
        }
    }

    #[test]
    fn foreign_magic_is_refused() {
        let bytes = vec![0u8; HEADER_SIZE];
        assert!(matches!(parse_header(&bytes), Err(GridError::BadMagic)));
    }

    #[test]
    fn an_unsupported_compression_is_refused() {
        assert!(matches!(decompress(&[], 3), Err(GridError::Server(_))));
        assert!(matches!(parse_directory(&[], 4), Err(GridError::Server(_))));
    }

    #[test]
    fn zero_zoom_is_the_single_root_tile() {
        assert_eq!(zxy_to_tile_id(0, 0, 0), 0);
    }

    /// A directory whose leading entry-count varint claims far more
    /// entries than the (tiny) remainder of the buffer could possibly
    /// hold four bytes for each. Must be refused up front, not turned into
    /// an attempt to allocate four `Vec`s of that length.
    #[test]
    fn a_huge_entry_count_is_refused_before_allocating() {
        let mut raw = Vec::new();
        write_varint(&mut raw, u64::MAX / 2);
        assert!(matches!(parse_directory(&raw, 1), Err(GridError::Truncated)));
    }

    fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(byte);
                break;
            }
            buf.push(byte | 0x80);
        }
    }

    /// Final review minor: the gzip arm of `decompress` (an archive's
    /// directories) is wired to the inflate cap, not only the shared
    /// primitive -- a zip bomb directory is refused.
    #[test]
    fn a_zip_bomb_directory_is_refused() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..(super::super::codec::MAX_INFLATED_BYTES >> 20) + 1 {
            std::io::Write::write_all(&mut gz, &zeros).unwrap();
        }
        let bomb = gz.finish().unwrap();
        match decompress(&bomb, 2) {
            Err(GridError::Inflate(why)) => assert!(why.contains("limit"), "{why}"),
            other => panic!("a zip bomb directory inflated: {:?}", other.map(|b| b.len())),
        }
        assert!(matches!(parse_directory(&bomb, 2), Err(GridError::Inflate(_))));
    }
}
