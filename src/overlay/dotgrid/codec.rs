//! The count-tile wire format: a header, delta-varint indices, varint
//! deci-counts, the whole body gzipped.

use std::io::{Read, Write};

use super::{GridError, GridTile};

pub const MAGIC: [u8; 4] = *b"DGT1";

/// Header (little-endian, before compression): MAGIC, version u8 = 1,
/// level u8, epoch u16, cells u32 (window rows*cols), n u32 (populated
/// cells), total f64. Then n delta-varint indices (first absolute), then n
/// varint deci-counts (`round(pop*10)`). Whole body gzip.
///
/// A cell whose deci-count rounds to 0 is dropped -- it costs a byte to
/// store and nothing to omit, since an absent index already decodes as
/// population 0.
///
/// `cells` must be sorted by index (debug-asserted): indices are stored as
/// deltas from the previous one, first absolute.
pub fn encode(level: u8, epoch: u16, window_cells: u32, cells: &[(u32, f32)]) -> Vec<u8> {
    debug_assert!(
        cells.windows(2).all(|w| w[0].0 <= w[1].0),
        "codec::encode requires cells sorted by index"
    );

    let mut idx_bytes = Vec::new();
    let mut count_bytes = Vec::new();
    let mut n: u32 = 0;
    let mut total = 0f64;
    let mut prev = 0u32;

    for &(idx, pop) in cells {
        let deci = (pop as f64 * 10.0).round();
        if deci <= 0.0 {
            continue;
        }
        let deci = deci as u64;
        let delta = if n == 0 { idx } else { idx - prev };
        write_varint(&mut idx_bytes, delta as u64);
        write_varint(&mut count_bytes, deci);
        prev = idx;
        n += 1;
        total += deci as f64 / 10.0;
    }

    let mut body = Vec::with_capacity(4 + 1 + 1 + 2 + 4 + 4 + 8 + idx_bytes.len() + count_bytes.len());
    body.extend_from_slice(&MAGIC);
    body.push(1u8); // version
    body.push(level);
    body.extend_from_slice(&epoch.to_le_bytes());
    body.extend_from_slice(&window_cells.to_le_bytes());
    body.extend_from_slice(&n.to_le_bytes());
    body.extend_from_slice(&total.to_le_bytes());
    body.extend_from_slice(&idx_bytes);
    body.extend_from_slice(&count_bytes);

    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&body).expect("gzip write to a Vec sink never fails");
    gz.finish().expect("gzip finish on a Vec sink never fails")
}

/// The most a count tile (or a PMTiles directory) may inflate to: 64 MiB.
/// The largest real tile holds 359,602 cells (MG-7, 30″ level 0), and a
/// cell inflates to at most 10 B (a delta index and a deci-count, two
/// varints of at most 5 B) -- 3.6 MB at worst; the archive stores about
/// 1.63 B a cell gzipped (MG-5). So this is 18x headroom over the worst
/// real body, and a bound on what one untrusted gzip stream can make a
/// worker allocate (gzip inflates ~1000:1, so a 64 KiB body could
/// otherwise ask for 64 MiB, and a 64 MiB one for 64 GiB).
pub const MAX_INFLATED_BYTES: u64 = 64 << 20;

/// Inflates a gzip stream, refusing it ([`GridError::Inflate`], "over the
/// limit") once it passes `cap` bytes.
///
/// **The body's capacity never exceeds `cap`** -- the figure every
/// decoder's worst-case memory is stated in. `read_to_end` cannot promise
/// that: it grows by doubling, so a body of exactly `cap` (accepted) came
/// back in a `2 * cap` allocation (review of Task 12, M1). Here the buffer
/// still grows geometrically, but each growth is a `reserve_exact` clamped
/// to `cap`, and the one byte that proves a stream is over the limit is
/// read into a stack probe, not the body. The transient while it grows is
/// at most `cap / 2 + cap` (the last realloc's old and new buffers), below
/// the body plus anything decoded from it.
pub(crate) fn inflate_capped(bytes: &[u8], cap: u64) -> Result<Vec<u8>, GridError> {
    const CHUNK: usize = 16 << 10;
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut gz = flate2::read::GzDecoder::new(bytes);
    let mut chunk = [0u8; CHUNK];
    let mut body = Vec::new();
    loop {
        // At the cap, one more byte is the probe that refuses the stream.
        let want = CHUNK.min(cap - body.len()).max(1);
        let n = match gz.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(GridError::Inflate(e.to_string())),
        };
        if n > cap - body.len() {
            return Err(GridError::Inflate(format!("inflates past the {cap} B limit")));
        }
        if body.capacity() - body.len() < n {
            let grown = body.capacity().saturating_mul(2).max(body.len() + n).max(CHUNK).min(cap);
            body.reserve_exact(grown - body.len());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

/// Inverse of [`encode`]. Never indexes a slice without a bounds check --
/// every short read returns [`GridError::Truncated`] rather than
/// panicking -- and an index outside the header's own `cells` window
/// returns [`GridError::IndexOutOfTile`] rather than being trusted.
pub fn decode(bytes: &[u8]) -> Result<GridTile, GridError> {
    decode_body(&inflate_capped(bytes, MAX_INFLATED_BYTES)?)
}

/// [`decode`] after the inflate: `body` is the gzip stream's content.
/// `codec_cat::decode_any` inflates once, reads the magic, and hands a
/// `DGT1` body here.
pub(crate) fn decode_body(body: &[u8]) -> Result<GridTile, GridError> {
    let mut r = Reader::new(body);
    if r.take(4)? != MAGIC {
        return Err(GridError::BadMagic);
    }
    let version = r.u8()?;
    if version != 1 {
        return Err(GridError::BadVersion(version));
    }
    let _level = r.u8()?;
    let _epoch = r.u16_le()?;
    let cells_total = r.u32_le()?;
    let n = r.u32_le()?;
    let total = r.f64_le()?;
    let n_cells = claimed_cells(n, r.remaining())?;

    let mut indices = Vec::with_capacity(n_cells);
    let mut idx = 0u32;
    for i in 0..n {
        let delta = r.varint()? as u32;
        idx = if i == 0 { delta } else { idx.wrapping_add(delta) };
        indices.push(idx);
    }

    let mut cells = Vec::with_capacity(n_cells);
    for idx in indices {
        let deci = r.varint()?;
        if idx >= cells_total {
            return Err(GridError::IndexOutOfTile { idx, cells: cells_total });
        }
        let pop = (deci as f64 / 10.0) as f32;
        cells.push((idx, pop, pop));
    }

    Ok(GridTile { cells, total, ..Default::default() })
}

/// How many cells a header's `n` may reserve room for: `n` itself, when
/// the `body_left` bytes after the header can hold that many -- every
/// cell costs at least two (a one-byte index varint and a one-byte count
/// varint) -- and [`GridError::Truncated`] otherwise, before anything is
/// sized from an untrusted count.
fn claimed_cells(n: u32, body_left: usize) -> Result<usize, GridError> {
    if u64::from(n) * 2 > body_left as u64 {
        return Err(GridError::Truncated);
    }
    Ok(n as usize)
}

pub(crate) fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
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

/// A cursor over the (already-inflated) body, whose every read is bounds
/// checked -- a short read is `GridError::Truncated`, never a panic.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], GridError> {
        let end = self.pos.checked_add(n).ok_or(GridError::Truncated)?;
        if end > self.bytes.len() {
            return Err(GridError::Truncated);
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub(crate) fn u8(&mut self) -> Result<u8, GridError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16_le(&mut self) -> Result<u16, GridError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32_le(&mut self) -> Result<u32, GridError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn f64_le(&mut self) -> Result<f64, GridError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// LEB128 unsigned varint.
    pub(crate) fn varint(&mut self) -> Result<u64, GridError> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            if shift >= 64 {
                // A varint that never terminates within a real header
                // is corrupt input, not a real 64-bit value: treat it
                // the same as running out of bytes.
                return Err(GridError::Truncated);
            }
            result |= ((byte & 0x7f) as u64) << shift;
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
    fn a_tile_round_trips_to_a_tenth_of_a_person() {
        let cells = vec![(0u32, 0.04f32), (3, 1.0), (4, 3.45), (70_000, 7_032.0), (714_000, 0.26)];
        let t = decode(&encode(0, 2025, 714_025, &cells)).unwrap();
        let want: Vec<(u32, f32, f32)> = vec![(3, 1.0, 1.0), (4, 3.5, 3.5), (70_000, 7032.0, 7032.0), (714_000, 0.3, 0.3)];
        // 0.04 rounds to 0 and is dropped: a zero deci-count is never stored.
        assert_eq!(t.cells, want);
    }

    #[test]
    fn the_header_total_equals_the_sum_of_decoded_cells() {
        let cells: Vec<(u32, f32)> = (0..10_000).map(|i| (i * 7, (i % 97) as f32 * 0.37 + 0.1)).collect();
        let t = decode(&encode(2, 2025, 70_000, &cells)).unwrap();
        let sum: f64 = t.cells.iter().map(|c| c.1 as f64).sum();
        assert!((t.total - sum).abs() < 1e-6 * sum.max(1.0));
    }

    #[test]
    fn sums_are_conserved_to_rounding() {
        // `wrapping_mul`, not `*`: this is a multiplicative hash spreading
        // `i` across the u32 range, and is meant to wrap -- plain `*`
        // panics in a debug build ("attempt to multiply with overflow"),
        // which is a bug in this literal test fixture, not in the codec.
        let cells: Vec<(u32, f32)> = (0..50_000u32)
            .map(|i| (i, ((i.wrapping_mul(2_654_435_761u32)) >> 20) as f32 / 97.0))
            .collect();
        let src: f64 = cells.iter().map(|c| c.1 as f64).sum();
        let t = decode(&encode(0, 2025, 50_000, &cells)).unwrap();
        assert!((t.total - src).abs() <= 0.05 * cells.len() as f64);
    }

    #[test]
    fn an_empty_tile_round_trips() {
        let t = decode(&encode(0, 2025, 1, &[])).unwrap();
        assert!(t.cells.is_empty());
        assert_eq!(t.total, 0.0);
    }

    #[test]
    fn a_truncated_payload_is_an_error_not_a_panic() {
        let bytes = encode(0, 2025, 1000, &[(1, 2.0), (500, 3.0)]);
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_err(), "cut at {cut} decoded");
        }
    }

    /// Final review I4: a header claiming u32::MAX populated cells over a
    /// near-empty body is an error before anything is sized from the
    /// claim (it was `Vec::with_capacity(n)` twice: 17 GB + 51 GB).
    #[test]
    fn a_header_claiming_more_cells_than_its_body_holds_is_refused_up_front() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&2025u16.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&0f64.to_le_bytes());
        body.extend_from_slice(&[1, 1]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &body).unwrap();
        let bytes = gz.finish().unwrap();
        assert!(matches!(decode(&bytes), Err(GridError::Truncated)));
        // The observable half: macOS reserves 68 GB of virtual memory
        // without complaint, so the check is on the capacity decode asks for.
        assert_eq!(claimed_cells(u32::MAX, 2), Err(GridError::Truncated));
        assert_eq!(claimed_cells(1, 2), Ok(1));
        assert_eq!(claimed_cells(2, 3), Err(GridError::Truncated));
    }

    /// Deferred from the final review: a count tile is untrusted bytes, and
    /// gzip inflates ~1000:1 -- 64 KiB of zeros became 64 MiB. The inflate
    /// stops at [`MAX_INFLATED_BYTES`] and refuses the tile instead of
    /// allocating what the stream claims.
    #[test]
    fn a_zip_bomb_is_refused_at_the_inflate_cap() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..(MAX_INFLATED_BYTES >> 20) + 1 {
            std::io::Write::write_all(&mut gz, &zeros).unwrap();
        }
        let bomb = gz.finish().unwrap();
        assert!(bomb.len() < 1 << 20, "the bomb is small on the wire: {} B", bomb.len());
        match decode(&bomb) {
            Err(GridError::Inflate(why)) => assert!(why.contains("limit"), "{why}"),
            other => panic!("a {} MiB body decoded as {other:?}", (MAX_INFLATED_BYTES >> 20) + 1),
        }
        // At the cap exactly is still a body (then judged on its bytes).
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &[7u8; 1000]).unwrap();
        let small = gz.finish().unwrap();
        assert_eq!(inflate_capped(&small, 1000).map(|b| b.len()), Ok(1000));
        assert!(matches!(inflate_capped(&small, 999), Err(GridError::Inflate(_))));
    }

    #[test]
    fn an_index_outside_the_window_is_refused() {
        let bytes = encode(0, 2025, 10, &[(11, 2.0)]);
        assert!(matches!(decode(&bytes), Err(GridError::IndexOutOfTile { .. })));
    }

    #[test]
    fn foreign_bytes_are_bad_magic() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, b"\x1a\x03not a dot tile").unwrap();
        assert!(matches!(decode(&gz.finish().unwrap()), Err(GridError::BadMagic)));
    }
}
