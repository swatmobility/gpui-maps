//! The sans-IO half of reading a "dotgrid" PMTiles archive: everything
//! here parses only bytes it is handed, and describes what to fetch next
//! as a [`GridRequest`] rather than fetching anything itself --
//! `fetch_native::open_pmtiles_grid` (native) or a wasm/iOS-side driver
//! executes those requests and feeds the bytes back in.
//!
//! # The open: one fetch, then up to two MORE -- never one that spans them
//!
//! [`PmtilesOpener::first_request`] always asks for the archive's first 16
//! KiB, which by the v3 spec's own layout rule always holds the header and
//! the root directory (a writer that cannot fit its root directory in
//! that budget is required to push entries into leaf directories instead
//! -- see the spec's "root directory" section). It does NOT reliably hold
//! the metadata block: a plain `pmtiles::PmTilesWriter` (this crate's own
//! dev-dependency, used by every test below) reserves exactly 16 KiB for
//! (header + root directory) and then writes metadata starting at byte
//! 16384 -- one past the end of the first fetch, every time, regardless of
//! how small the root directory actually is.
//!
//! So [`PmtilesOpener::after_first`] checks metadata and the leaf section
//! SEPARATELY, and asks for whichever of the two is not already inside the
//! bytes it was given -- as up to two INDEPENDENT `GridRequest`s, never one
//! request spanning from one to the other. That independence matters: the
//! v3 layout is `| Header | Root Directory | Metadata | Tile Data | Leaf
//! Directories |`, so a single combined range from metadata's start
//! through the end of the leaf section would also cover the ENTIRE tile
//! data section sitting between them -- fine for this file's own
//! kilobyte-scale test archives, but for a real `dotgrid` archive (the
//! design doc's own estimate: 130 MB-5 GB) that is nearly the whole file,
//! downloaded just to reach a few hundred bytes of JSON and a leaf
//! directory. [`PmtilesOpener::finish`] resolves whichever of
//! metadata/entries [`Self::after_first`] could not, from the bodies of
//! whichever requests it asked for.
//!
//! The `dotgrid` metadata key this looks for is JSON:
//! `{"dotgrid":{"version":1,"base_arcsec":30,"origin_lat":90,"origin_lon":-180,
//! "levels":7,"max_data_zoom":6,"epochs":[2025]},"attribution":"GHS-POP
//! R2023A, European Commission JRC"}` -- `attribution` is a SIBLING of
//! `dotgrid`, not nested inside it. An archive holds one epoch (the
//! `epochs` array is a single-element description of which one, not a
//! menu); tile ids in the archive are keyed by data zoom directly (`z`),
//! which already determines the pyramid level
//! (`level = max_data_zoom - z`), so [`PmtilesGridSource::request`] never
//! needs to consult `descriptor.max_data_zoom` to resolve a key.
//!
//! # Race archives (C7)
//!
//! A race-dots archive carries a second sibling key, `"race"`:
//! `{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":9.72}`. It
//! becomes the source's [`RaceDescriptor`] ([`GridCountSource::race`]).
//! Its part tiles (`DGP1`) sit at `part_zoom` in the same archive, keyed
//! by their own `z/x/y`, as the cell tiles are; [`PmtilesGridSource::decode`]
//! reads every tile through `codec_cat::decode_any`, which picks the
//! decoder by the tile's magic (`DGT1`, `DGT2` or `DGP1`).

use super::pmtiles_dir::{self, PmEntry, PmHeader};
use super::{GridCountSource, GridDescriptor, GridError, GridRequest, GridTile, GridTileKey, RaceDescriptor};

/// What an archive's metadata describes: its grid, and -- for a race
/// archive -- its part tiles and crossover.
type Described = (GridDescriptor, Option<RaceDescriptor>);

/// The archive's first fetch: header + root directory, and (see this
/// module's doc comment) metadata too when the archive happens to be
/// small enough. `16_384` is the v3 spec's own root-directory budget, not
/// a guess -- see `pmtiles::header::MAX_INITIAL_BYTES` (private to that
/// crate, hence the literal here) and this module's doc comment.
const FIRST_REQUEST_LEN: u64 = 16_384;

/// Drives a PMTiles archive open to completion: one fetch, then up to two
/// more, INDEPENDENT of each other (see this module's doc comment), parsed
/// sans-IO. Generic over what the archive's metadata describes (`M`, read
/// by the `parse` function it is built with): [`PmtilesOpener`] is the
/// `dotgrid` count-tile archive's, and `overlay::hexes` opens its hexagon
/// archive through the same three steps.
pub struct ArchiveOpener<M> {
    location: String,
    /// Reads the metadata block's raw (still-compressed) bytes, given the
    /// header's `internal_compression`.
    parse: fn(&[u8], u8) -> Result<M, GridError>,
    header: Option<PmHeader>,
    /// The root directory, as read from the first fetch -- some entries
    /// real tiles, some (`run_length == 0`) pointers into the leaf
    /// section. Consumed once [`Self::entries`] can be resolved (which may
    /// happen inside `after_first` itself, or have to wait for `finish`).
    root: Vec<PmEntry>,
    /// `Some` once resolved -- either directly from the first fetch (an
    /// archive small enough to meet the guarantee this module's doc
    /// comment describes), or in [`Self::finish_archive`] from the metadata
    /// request's own body.
    descriptor: Option<M>,
    /// `Some` once resolved -- either directly from the first fetch, or
    /// (when there is no leaf section at all) immediately in
    /// `after_first` with no further IO, or in [`Self::finish_archive`] from the
    /// leaf request's own body.
    entries: Option<Vec<PmEntry>>,
}

/// What [`ArchiveOpener::finish_archive`] resolves: the parsed metadata
/// and the flat, tile-id-sorted list of real tile entries with absolute
/// offsets.
pub struct OpenedArchive<M> {
    pub meta: M,
    pub entries: Vec<PmEntry>,
    pub location: String,
}

impl<M> ArchiveOpener<M> {
    pub fn new(location: impl Into<String>, parse: fn(&[u8], u8) -> Result<M, GridError>) -> Self {
        Self { location: location.into(), parse, header: None, root: Vec::new(), descriptor: None, entries: None }
    }

    /// Bytes `[0, 16384)`: header, root directory, and (the converter
    /// guarantees, for an archive small enough) metadata.
    pub fn first_request(&self) -> GridRequest {
        GridRequest::Range { location: self.location.clone(), offset: 0, length: FIRST_REQUEST_LEN }
    }

    /// Parses header + root directory from `bytes` (the
    /// [`Self::first_request`] response). Returns the follow-up requests
    /// still needed: metadata's own `[metadata_offset, metadata_end)`
    /// range if it is not already inside `bytes`, and the leaf section's
    /// own `[leaf_offset, leaf_end)` range if there is one and it is not
    /// already inside `bytes` either -- in that order, each independent of
    /// the other (see this module's doc comment for why they are never
    /// merged into one). An empty `Vec` means the archive is already fully
    /// resolved; [`Self::finish_archive`] can then be called with an empty slice.
    pub fn after_first(&mut self, bytes: &[u8]) -> Result<Vec<GridRequest>, GridError> {
        if bytes.len() < pmtiles_dir::HEADER_SIZE {
            return Err(GridError::Truncated);
        }
        let header = pmtiles_dir::parse_header(&bytes[..pmtiles_dir::HEADER_SIZE])?;
        if !matches!(header.internal_compression, 1 | 2) {
            return Err(GridError::Server(format!(
                "unsupported internal_compression {}",
                header.internal_compression
            )));
        }

        let root_end = header
            .root_offset
            .checked_add(header.root_length)
            .ok_or(GridError::Truncated)?;
        if (bytes.len() as u64) < root_end {
            // The v3 spec guarantees the root directory fits in the first
            // 16 KiB; a caller handing over fewer bytes than that is the
            // truncation case, not a format violation.
            return Err(GridError::Truncated);
        }
        self.root = pmtiles_dir::parse_directory(
            &bytes[header.root_offset as usize..root_end as usize],
            header.internal_compression,
        )?;

        let mut requests = Vec::new();

        let meta_end = header
            .metadata_offset
            .checked_add(header.metadata_length)
            .ok_or(GridError::Truncated)?;
        if (bytes.len() as u64) >= meta_end {
            let meta_bytes = &bytes[header.metadata_offset as usize..meta_end as usize];
            self.descriptor = Some((self.parse)(meta_bytes, header.internal_compression)?);
        } else {
            requests.push(GridRequest::Range {
                location: self.location.clone(),
                offset: header.metadata_offset,
                length: header.metadata_length,
            });
        }

        if header.leaf_length == 0 {
            // No leaf section at all -- every root entry must be a real
            // tile. A `run_length == 0` entry here would be a malformed
            // archive claiming a leaf pointer into a section the header
            // says does not exist; `resolve_entries` reports that as an
            // error via this closure rather than this branch silently
            // assuming it can never happen.
            self.entries = Some(resolve_entries(&self.root, &header, |_, _| {
                Err(GridError::Server(
                    "directory has a leaf-pointer entry but the archive states no leaf section".to_string(),
                ))
            })?);
        } else {
            let leaf_end = header.leaf_offset.checked_add(header.leaf_length).ok_or(GridError::Truncated)?;
            if (bytes.len() as u64) >= leaf_end {
                let leaf_bytes = &bytes[header.leaf_offset as usize..leaf_end as usize];
                self.entries = Some(resolve_entries(&self.root, &header, |offset, length| {
                    slice_absolute(leaf_bytes, header.leaf_offset, offset, length)
                })?);
            } else {
                requests.push(GridRequest::Range {
                    location: self.location.clone(),
                    offset: header.leaf_offset,
                    length: header.leaf_length,
                });
            }
        }

        self.header = Some(header);
        Ok(requests)
    }

    /// Finishes the open. `bodies` holds exactly the responses to the
    /// requests [`Self::after_first`] returned, in the SAME ORDER
    /// (metadata's body first if it asked for one, then the leaf
    /// section's) -- an empty slice when it returned no requests at all.
    pub fn finish_archive(mut self, bodies: &[Vec<u8>]) -> Result<OpenedArchive<M>, GridError> {
        let header = self
            .header
            .take()
            .ok_or_else(|| GridError::Server("ArchiveOpener::finish_archive called before after_first".to_string()))?;
        let mut bodies = bodies.iter();

        let meta = match self.descriptor.take() {
            Some(descriptor) => descriptor,
            None => {
                let body = bodies.next().ok_or_else(|| {
                    GridError::Server("ArchiveOpener::finish_archive needs the metadata bytes after_first asked for".to_string())
                })?;
                (self.parse)(body, header.internal_compression)?
            }
        };

        let entries = match self.entries.take() {
            Some(entries) => entries,
            None => {
                let body = bodies.next().ok_or_else(|| {
                    GridError::Server("ArchiveOpener::finish_archive needs the leaf bytes after_first asked for".to_string())
                })?;
                resolve_entries(&self.root, &header, |offset, length| {
                    slice_absolute(body, header.leaf_offset, offset, length)
                })?
            }
        };

        Ok(OpenedArchive { meta, entries, location: self.location })
    }
}

/// Drives a `dotgrid` PMTiles archive open to completion: an
/// [`ArchiveOpener`] whose metadata is the `dotgrid` (and `race`) keys.
pub struct PmtilesOpener(ArchiveOpener<Described>);

impl PmtilesOpener {
    pub fn new(location: impl Into<String>) -> Self {
        Self(ArchiveOpener::new(location, parse_descriptor))
    }

    /// See [`ArchiveOpener::first_request`].
    pub fn first_request(&self) -> GridRequest {
        self.0.first_request()
    }

    /// See [`ArchiveOpener::after_first`].
    pub fn after_first(&mut self, bytes: &[u8]) -> Result<Vec<GridRequest>, GridError> {
        self.0.after_first(bytes)
    }

    /// See [`ArchiveOpener::finish_archive`].
    pub fn finish(self, bodies: &[Vec<u8>]) -> Result<PmtilesGridSource, GridError> {
        let OpenedArchive { meta: (descriptor, race), entries, location } = self.0.finish_archive(bodies)?;
        Ok(PmtilesGridSource { descriptor, race, entries, location })
    }
}

/// The entry covering `tile_id` in a flat, tile-id-sorted entry list, via
/// the same "exact hit, or the preceding entry's run" search PMTiles
/// readers use (a `DirEntry` with `run_length > 1` covers every id in
/// `[tile_id, tile_id + run_length)` without a directory entry of its own
/// for each).
pub fn find_entry(entries: &[PmEntry], tile_id: u64) -> Option<PmEntry> {
    match entries.binary_search_by(|e| e.tile_id.cmp(&tile_id)) {
        Ok(idx) => Some(entries[idx]),
        Err(next) if next > 0 => {
            let prev = entries[next - 1];
            (tile_id - prev.tile_id < u64::from(prev.run_length)).then_some(prev)
        }
        Err(_) => None,
    }
}

/// Turns a root directory's entries into the final, flat, tile-id-sorted
/// list of REAL tile entries with ABSOLUTE file offsets: a plain entry
/// (`run_length >= 1`) passes through, and each leaf pointer (`run_length
/// == 0`) is replaced by the entries found inside the child directory
/// `byte_range` reads and decompresses.
///
/// Per the v3 spec, a directory entry's `offset` is never itself an
/// absolute file position: a leaf-pointer entry's `offset` is relative to
/// `header.leaf_offset` (the leaf SECTION's own start -- see the `pmtiles`
/// crate's `writer::build_roots_leaves`, which begins accumulating leaf
/// offsets from 0), while an ordinary tile entry's `offset` -- whether it
/// came straight from the root directory or from inside a resolved leaf --
/// is relative to `header.data_offset` (see that crate's
/// `async_reader.rs`: `header.data_offset + entry.offset`). This resolves
/// leaf pointers using the first convention and then rebases every
/// surviving tile entry to an absolute offset using the second, once, so
/// nothing downstream has to remember either rule.
///
/// One level of leaf directories only: a leaf entry that is itself another
/// leaf pointer is refused rather than silently dropped. A world-wide
/// `dotgrid` archive at the coarsest levels this format serves (z <= 9,
/// per the design doc) never approaches the entry count where PMTiles
/// writers grow a second level -- that only happens in the hundreds of
/// millions of entries, vector-planet territory.
fn resolve_entries(
    root: &[PmEntry],
    header: &PmHeader,
    byte_range: impl Fn(u64, u64) -> Result<Vec<u8>, GridError>,
) -> Result<Vec<PmEntry>, GridError> {
    let mut out = Vec::with_capacity(root.len());
    for entry in root {
        if entry.run_length > 0 {
            out.push(*entry);
            continue;
        }
        // `entry.offset` here is untrusted (parsed straight out of
        // archive bytes) and relative to `header.leaf_offset` -- checked,
        // not `+`, so a crafted offset near `u64::MAX` is an error rather
        // than a wrapping-add panic in a debug build.
        let leaf_pos = header.leaf_offset.checked_add(entry.offset).ok_or(GridError::Truncated)?;
        let leaf_bytes = byte_range(leaf_pos, u64::from(entry.length))?;
        let child = pmtiles_dir::parse_directory(&leaf_bytes, header.internal_compression)?;
        if child.iter().any(|c| c.run_length == 0) {
            return Err(GridError::Server("multi-level leaf directories are not supported".to_string()));
        }
        out.extend(child);
    }
    for entry in &mut out {
        // Same reasoning as `leaf_pos` above: `entry.offset` came from
        // (possibly untrusted) archive bytes.
        entry.offset = entry.offset.checked_add(header.data_offset).ok_or(GridError::Truncated)?;
    }
    out.sort_by_key(|e| e.tile_id);
    Ok(out)
}

/// Reads `[offset, offset + length)` (an absolute file range) out of
/// `buf`, where `buf[0]` is file offset `base`. Bounds-checked: a range
/// this crate's own header/directory bookkeeping asked for but which
/// `buf` does not actually cover is [`GridError::Truncated`], never a
/// panic -- the same contract every other reader in this module keeps.
fn slice_absolute(buf: &[u8], base: u64, offset: u64, length: u64) -> Result<Vec<u8>, GridError> {
    let start = offset.checked_sub(base).ok_or(GridError::Truncated)?;
    let start = usize::try_from(start).map_err(|_| GridError::Truncated)?;
    let length = usize::try_from(length).map_err(|_| GridError::Truncated)?;
    let end = start.checked_add(length).ok_or(GridError::Truncated)?;
    if end > buf.len() {
        return Err(GridError::Truncated);
    }
    Ok(buf[start..end].to_vec())
}

/// Reads the `dotgrid` [`GridDescriptor`] out of a metadata block's raw
/// (still-compressed) bytes, and the `race` key's [`RaceDescriptor`] when
/// there is one. `attribution` and `race` are sibling keys of `dotgrid`
/// at the metadata root -- see this module's doc comment.
fn parse_descriptor(meta_bytes: &[u8], compression: u8) -> Result<Described, GridError> {
    let raw = pmtiles_dir::decompress(meta_bytes, compression)?;
    let root: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| GridError::Server(format!("archive metadata is not JSON: {e}")))?;
    let dotgrid = root
        .get("dotgrid")
        .ok_or_else(|| GridError::Server("archive metadata has no \"dotgrid\" key".to_string()))?;

    let field_f64 = |name: &str| -> Result<f64, GridError> {
        dotgrid
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| GridError::Server(format!("dotgrid metadata missing numeric \"{name}\"")))
    };
    let field_u8 = |name: &str| -> Result<u8, GridError> {
        dotgrid
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GridError::Server(format!("dotgrid metadata missing a small integer \"{name}\"")))
    };

    let version = field_u8("version")?;
    if version != 1 {
        return Err(GridError::Server(format!("unsupported dotgrid metadata version {version}")));
    }
    let base_arcsec = field_f64("base_arcsec")?;
    let origin_lat = field_f64("origin_lat")?;
    let origin_lon = field_f64("origin_lon")?;
    let levels = field_u8("levels")?;
    let max_data_zoom = field_u8("max_data_zoom")?;
    let epochs = dotgrid
        .get("epochs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| GridError::Server("dotgrid metadata missing \"epochs\"".to_string()))?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| GridError::Server("dotgrid metadata \"epochs\" entry is not a small integer".to_string()))
        })
        .collect::<Result<Vec<u16>, GridError>>()?;
    let attribution = root.get("attribution").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();

    let descriptor = GridDescriptor { base_arcsec, origin_lat, origin_lon, levels, max_data_zoom, epochs, attribution };
    descriptor.validate()?;
    let race = match root.get("race") {
        None => None,
        Some(race) => Some(parse_race(race, &descriptor)?),
    };
    Ok((descriptor, race))
}

/// The `"race"` metadata key (C7): `kind` must be
/// [`RaceDescriptor::KIND`]; the rest is checked by
/// [`RaceDescriptor::validate`].
fn parse_race(race: &serde_json::Value, grid: &GridDescriptor) -> Result<RaceDescriptor, GridError> {
    let missing = |name: &str| GridError::Server(format!("race metadata missing \"{name}\""));
    let kind = race.get("kind").and_then(serde_json::Value::as_str).ok_or_else(|| missing("kind"))?;
    if kind != RaceDescriptor::KIND {
        return Err(GridError::Server(format!("unsupported race metadata kind {kind:?}")));
    }
    let small = |name: &str| race.get(name).and_then(serde_json::Value::as_u64).and_then(|v| u8::try_from(v).ok());
    let groups = small("groups").ok_or_else(|| missing("groups"))?;
    let part_zoom = small("part_zoom").ok_or_else(|| missing("part_zoom"))?;
    let crossover = race.get("crossover").and_then(serde_json::Value::as_f64).ok_or_else(|| missing("crossover"))?;
    let race = RaceDescriptor { grid: grid.clone(), groups, part_zoom, crossover };
    race.validate()?;
    Ok(race)
}

/// A "dotgrid" count-tile archive, opened via [`PmtilesOpener`]: a
/// descriptor and a flat, tile-id-sorted entry list, resolved once at open
/// so every later [`GridCountSource::request`] is a binary search with no
/// further IO.
pub struct PmtilesGridSource {
    descriptor: GridDescriptor,
    race: Option<RaceDescriptor>,
    entries: Vec<PmEntry>,
    location: String,
}

impl PmtilesGridSource {
    /// The entry covering `tile_id`, via the same "exact hit, or the
    /// preceding entry's run" search PMTiles readers use (a `DirEntry`
    /// with `run_length > 1` covers every id in
    /// `[tile_id, tile_id + run_length)` without a directory entry of its
    /// own for each).
    fn find(&self, tile_id: u64) -> Option<PmEntry> {
        find_entry(&self.entries, tile_id)
    }
}

impl GridCountSource for PmtilesGridSource {
    fn descriptor(&self) -> &GridDescriptor {
        &self.descriptor
    }

    /// `keys` is one key (see `max_batch`'s default): the archive's tile
    /// id is `key.tile`'s own `(z, x, y)` directly -- an archive's z
    /// already implies the level, per this module's doc comment.
    fn request(&self, keys: &[GridTileKey]) -> GridRequest {
        let Some(key) = keys.first() else {
            return GridRequest::Absent;
        };
        let tile_id = pmtiles_dir::zxy_to_tile_id(key.tile.z, key.tile.x, key.tile.y);
        match self.find(tile_id) {
            Some(entry) => GridRequest::Range {
                location: self.location.clone(),
                offset: entry.offset,
                length: u64::from(entry.length),
            },
            None => GridRequest::Absent,
        }
    }

    /// An empty `body` is the [`GridRequest::Absent`] case: no IO was
    /// done, so there is nothing to inflate, and the answer is an empty
    /// tile. A real archived tile is never zero bytes -- `codec::encode`
    /// always emits at least its own header -- so this reading is
    /// unambiguous. Any tile version decodes (`codec_cat::decode_any`
    /// picks by magic): a v1 `DGT1` exactly as `codec::decode` did.
    fn decode(&self, keys: &[GridTileKey], body: &[u8]) -> Result<Vec<(GridTileKey, GridTile)>, GridError> {
        let Some(key) = keys.first() else {
            return Ok(Vec::new());
        };
        if body.is_empty() {
            return Ok(vec![(*key, GridTile::default())]);
        }
        // A part tile's header must name the tile it was fetched for, as
        // the hex source checks (review final #6).
        let tile = super::codec_cat::decode_any_at(body, key.tile)?;
        // A part key must get a part tile, a cell key a cell tile: a
        // mis-built archive is an error, not empty blocks under a total
        // that raises the ppd (review F6). An empty tile of either kind
        // passes.
        let parts = !tile.parts.is_empty();
        let cells = !tile.cells.is_empty();
        if (key.is_part() && cells) || (!key.is_part() && parts) {
            return Err(GridError::Server(format!(
                "archive tile {}/{}/{} is a {} tile under a {} key",
                key.tile.z,
                key.tile.x,
                key.tile.y,
                if parts { "part" } else { "cell" },
                if key.is_part() { "part" } else { "cell" }
            )));
        }
        Ok(vec![(*key, tile)])
    }

    fn race(&self) -> Option<&RaceDescriptor> {
        self.race.as_ref()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use ::pmtiles::{Compression, PmTilesWriter, TileCoord, TileId as PmTileId, TileType};

    use super::*;
    use crate::geo::TileId;

    const DOTGRID_METADATA: &str = r#"{"dotgrid":{"version":1,"base_arcsec":30,"origin_lat":90,"origin_lon":-180,"levels":7,"max_data_zoom":6,"epochs":[2025]},"attribution":"GHS-POP R2023A, European Commission JRC"}"#;

    /// Builds a real v3 archive with the plain `pmtiles` crate writer --
    /// the same helper shape as `src/tile_source/pmtiles.rs:475`'s
    /// `build_archive`. When `leaves` is true, 20,000 tiny, distinct
    /// tiles are added at z9 so the writer's root directory overflows
    /// into leaf directories (asserted below via the header's own
    /// `leaf_length`, at the byte offset the v3 spec puts it -- the same
    /// check `tile_source::pmtiles`'s test uses).
    fn archive(tiles: &[(u8, u32, u32, Vec<u8>)], metadata: &str, leaves: bool) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = PmTilesWriter::new(TileType::Unknown)
                .metadata(metadata)
                .tile_compression(Compression::None)
                .create(&mut buffer)
                .expect("failed to create pmtiles writer");
            for (z, x, y, data) in tiles {
                let coord = TileCoord::new(*z, *x, *y).expect("valid coord");
                writer.add_tile(coord, data).expect("failed to add tile");
            }
            if leaves {
                for i in 0..20_000u32 {
                    let coord = TileCoord::new(9, i % 512, i / 512).expect("valid coord");
                    writer.add_tile(coord, &i.to_le_bytes()).expect("failed to add tile");
                }
            }
            writer.finalize().expect("failed to finalize archive");
        }
        let bytes = buffer.into_inner();
        if leaves {
            let leaf_length = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
            assert!(leaf_length > 0, "expected this archive to spill into leaf directories");
        }
        bytes
    }

    /// Stands in for a real fetch: slices the in-memory `archive` bytes at
    /// whatever range a [`GridRequest::Range`] asks for.
    fn fetch(archive: &[u8], req: &GridRequest) -> Vec<u8> {
        match req {
            GridRequest::Range { offset, length, .. } => {
                let start = *offset as usize;
                let end = (start + *length as usize).min(archive.len());
                archive[start..end].to_vec()
            }
            other => panic!("test only drives Range requests, got {other:?}"),
        }
    }

    /// Drives a full open (every fetch it takes) against in-memory
    /// `archive` bytes, the way `fetch_native::open_pmtiles_grid` would
    /// against a real one.
    fn open(archive: &[u8]) -> Result<PmtilesGridSource, GridError> {
        let mut opener = PmtilesOpener::new("mem://test");
        let first = fetch(archive, &opener.first_request());
        let requests = opener.after_first(&first)?;
        let bodies: Vec<Vec<u8>> = requests.iter().map(|req| fetch(archive, req)).collect();
        opener.finish(&bodies)
    }

    fn key(z: u8, x: u32, y: u32) -> GridTileKey {
        GridTileKey { tile: TileId { z, x, y }, level: 0, epoch: 2025, epoch_b: None }
    }

    #[test]
    fn a_tile_round_trips_through_an_archive_with_leaf_directories() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(3, 5.0)]);
        let bytes = archive(&[(6, 10, 20, payload.clone())], DOTGRID_METADATA, true);

        let source = open(&bytes).expect("archive should open");
        let key = key(6, 10, 20);
        let req = source.request(&[key]);
        let GridRequest::Range { offset, length, .. } = req else {
            panic!("expected a Range request for a present tile, got {req:?}")
        };
        let body = &bytes[offset as usize..(offset + length) as usize];
        let decoded = source.decode(&[key], body).unwrap();
        assert_eq!(decoded, vec![(key, super::super::codec::decode(&payload).unwrap())]);
    }

    #[test]
    fn a_missing_tile_is_absent_without_io() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(1, 1.0)]);
        let bytes = archive(&[(6, 10, 20, payload)], DOTGRID_METADATA, false);

        let source = open(&bytes).expect("archive should open");
        let key = key(6, 11, 20);
        assert_eq!(source.request(&[key]), GridRequest::Absent);
        assert_eq!(source.decode(&[key], &[]).unwrap(), vec![(key, GridTile::default())]);
    }

    #[test]
    fn a_non_dotgrid_archive_is_refused_at_open() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(1, 1.0)]);
        let bytes = archive(&[(6, 10, 20, payload)], r#"{"name":"not a dotgrid archive"}"#, false);

        assert!(open(&bytes).is_err(), "an archive with no \"dotgrid\" metadata key must not open");
    }

    /// Final review I3: an archive whose metadata says `levels: 0` (or
    /// more levels than data zooms) is refused at open, not left to
    /// underflow `levels - 1` on the first frame.
    #[test]
    fn an_archive_with_an_unusable_descriptor_is_refused_at_open() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(1, 1.0)]);
        for (levels, base_arcsec) in [(0, 30), (8, 30), (7, 0)] {
            let metadata = DOTGRID_METADATA.replace(r#""levels":7"#, &format!(r#""levels":{levels}"#)).replace(r#""base_arcsec":30"#, &format!(r#""base_arcsec":{base_arcsec}"#));
            let bytes = archive(&[(6, 10, 20, payload.clone())], &metadata, false);
            assert!(matches!(open(&bytes), Err(GridError::Server(_))), "levels {levels} base_arcsec {base_arcsec} opened");
        }
    }

    #[test]
    fn run_length_entries_resolve() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(2, 3.0)]);
        // Five consecutive tile ids at z6, added in increasing id order
        // with an IDENTICAL payload: the writer's own dedup collapses
        // them into one directory entry with run_length == 5 (see
        // `WriterState::add_tile_by_id`), covering every one of the five
        // (x, y) pairs without five separate entries.
        let base_id = pmtiles_dir::zxy_to_tile_id(6, 0, 0);
        let coords: Vec<TileCoord> = (0..5)
            .map(|i| PmTileId::new(base_id + i).unwrap().into())
            .collect();
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = PmTilesWriter::new(TileType::Unknown)
                .metadata(DOTGRID_METADATA)
                .tile_compression(Compression::None)
                .create(&mut buffer)
                .unwrap();
            for coord in &coords {
                writer.add_tile(*coord, &payload).unwrap();
            }
            writer.finalize().unwrap();
        }
        let bytes = buffer.into_inner();

        let source = open(&bytes).expect("archive should open");
        for coord in &coords {
            let key = key(coord.z(), coord.x(), coord.y());
            let req = source.request(&[key]);
            let GridRequest::Range { offset, length, .. } = req else {
                panic!("expected a Range request for {key:?}, got {req:?}")
            };
            let body = &bytes[offset as usize..(offset + length) as usize];
            assert_eq!(body, payload.as_slice(), "run-length entry did not resolve for {key:?}");
        }
    }

    /// `zxy_to_tile_id` bit for bit against the real `pmtiles` crate's own
    /// `TileCoord -> TileId` conversion (which the archives above are
    /// written and looked up through) -- see this module's `resolve_entries`
    /// doc comment for why the two algorithms are expected to agree.
    #[test]
    fn hilbert_ids_match_the_writer() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next_u32 = || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };

        for _ in 0..1_000 {
            let z = 1 + (next_u32() % 12) as u8;
            let n = 1u32 << z;
            let x = next_u32() % n;
            let y = next_u32() % n;

            let coord = TileCoord::new(z, x, y).unwrap();
            let id: PmTileId = coord.into();
            let want: u64 = id.into();

            assert_eq!(pmtiles_dir::zxy_to_tile_id(z, x, y), want, "z={z} x={x} y={y}");
        }
    }

    /// The controller's own ruling (Task 5 review, C1): opening an archive
    /// with leaf directories must never issue a follow-up request that
    /// overlaps the tile DATA section sitting between metadata and the
    /// leaves in the v3 layout -- a combined "metadata through leaves"
    /// range would, on a real multi-hundred-megabyte archive, pull down
    /// nearly the whole file just to reach a few hundred bytes of JSON and
    /// a leaf directory.
    #[test]
    fn no_follow_up_request_overlaps_the_tile_data_section() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(3, 5.0)]);
        let bytes = archive(&[(6, 10, 20, payload)], DOTGRID_METADATA, true);

        // `PmHeader` does not carry `data_length` (nothing else in
        // `dotgrid` reads it) -- read it directly at the byte offset the
        // v3 spec puts it, the same trick `tile_source::pmtiles`'s own
        // test uses for `leaf_length`.
        let data_offset = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
        let data_length = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        assert!(data_length > 0, "expected this archive to actually store tile bytes");
        let data_start = data_offset;
        let data_end = data_offset + data_length;

        let mut opener = PmtilesOpener::new("mem://test");
        let first = fetch(&bytes, &opener.first_request());
        let requests = opener.after_first(&first).expect("archive should parse");
        assert!(!requests.is_empty(), "this archive's metadata and leaves both sit past the first 16 KiB");

        for req in &requests {
            let GridRequest::Range { offset, length, .. } = req else {
                panic!("expected a Range request, got {req:?}")
            };
            let (req_start, req_end) = (*offset, *offset + *length);
            let overlaps = req_start < data_end && data_start < req_end;
            assert!(
                !overlaps,
                "follow-up request [{req_start}, {req_end}) overlaps the tile data section \
                 [{data_start}, {data_end})"
            );
        }

        let bodies: Vec<Vec<u8>> = requests.iter().map(|req| fetch(&bytes, req)).collect();
        assert!(opener.finish(&bodies).is_ok(), "the archive should still open with independent fetches");
    }

    /// I2 (Task 5 review): a leaf-pointer entry's `offset` is untrusted
    /// (parsed straight out of archive bytes) and is added to
    /// `header.leaf_offset` before being read. A crafted offset near
    /// `u64::MAX` must be an error, not a panic.
    #[test]
    fn a_leaf_pointer_offset_near_u64_max_errors_rather_than_panics() {
        let header = test_header(u64::MAX - 5, 0);
        let root = vec![PmEntry { tile_id: 0, offset: 100, length: 1, run_length: 0 }];

        let result = resolve_entries(&root, &header, |_, _| Ok(Vec::new()));
        assert!(matches!(result, Err(GridError::Truncated)), "expected Truncated, got {result:?}");
    }

    /// Same as above, for the other checked addition `resolve_entries`
    /// performs: rebasing a resolved tile entry's offset onto
    /// `header.data_offset`.
    #[test]
    fn an_entry_offset_near_u64_max_errors_rather_than_panics() {
        let header = test_header(0, u64::MAX - 10);
        let root = vec![PmEntry { tile_id: 0, offset: 100, length: 1, run_length: 1 }];

        let result = resolve_entries(&root, &header, |_, _| unreachable!("no leaf-pointer entries in this root"));
        assert!(matches!(result, Err(GridError::Truncated)), "expected Truncated, got {result:?}");
    }

    /// The converter's metadata (`tools/race-build/src/pyramid.rs`'s
    /// `METADATA`), verbatim.
    const RACE_METADATA: &str = r#"{"dotgrid":{"version":1,"base_arcsec":3,"origin_lat":90,"origin_lon":-180,"levels":10,"max_data_zoom":9,"epochs":[2020]},"race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":9.72},"attribution":"U.S. Census Bureau"}"#;

    /// C7: a race archive opens with its race descriptor next to its grid,
    /// and serves a categorical cell tile and a part tile from the same
    /// archive, each decoded by its magic.
    #[test]
    fn a_race_archive_opens_with_its_race_descriptor_and_decodes_parts_and_cells() {
        use crate::overlay::dotgrid::codec_cat;
        use crate::overlay::dotgrid::parts::{Part, Tri};
        let part_tile = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let part = Part { tri: Tri { a: [x0 + 5, y0 + 7], b: [x0 + 900, y0 + 7], c: [x0 + 5, y0 + 1200] }, c: [1, 2, 0, 0, 0, 0, 0, 4] };
        let parts = codec_cat::encode_parts(part_tile, &[part]);
        let cells = codec_cat::encode_cells(0, 100, &[(3, [0, 5, 0, 0, 0, 0, 0, 1])]);
        let bytes = archive(&[(9, 150, 192, cells), (12, 1206, 1538, parts)], RACE_METADATA, false);

        let source = open(&bytes).expect("a race archive opens");
        let race = source.race().expect("its race descriptor");
        assert_eq!((race.groups, race.part_zoom, race.crossover), (8, 12, 9.72));
        assert_eq!(&race.grid, source.descriptor());
        assert_eq!((race.grid.base_arcsec, race.grid.levels, race.grid.max_data_zoom), (3.0, 10, 9));

        let read = |key: GridTileKey| {
            let GridRequest::Range { offset, length, .. } = source.request(&[key]) else { panic!("{key:?} is in the archive") };
            source.decode(&[key], &bytes[offset as usize..(offset + length) as usize]).unwrap().remove(0).1
        };
        let p = read(GridTileKey::part(part_tile, 2020));
        assert_eq!(p.parts.len(), 1);
        assert_eq!(p.parts[0].counts, [1, 2, 0, 0, 0, 0, 0, 4]);
        assert_eq!(p.total, 7.0);
        let c = read(GridTileKey { tile: TileId { z: 9, x: 150, y: 192 }, level: 0, epoch: 2020, epoch_b: None });
        assert_eq!(c.cats, vec![[0, 5, 0, 0, 0, 0, 0, 1]]);
        assert_eq!(c.total, 6.0);
    }

    #[test]
    fn a_plain_dotgrid_archive_has_no_race_descriptor() {
        let payload = super::super::codec::encode(0, 2025, 100, &[(1, 1.0)]);
        let source = open(&archive(&[(6, 10, 20, payload)], DOTGRID_METADATA, false)).unwrap();
        assert!(source.race().is_none());
    }

    /// A `race` key the layer cannot plan is refused at open: an unknown
    /// kind, the wrong group count, a part zoom inside the cell levels, or
    /// a missing field.
    #[test]
    fn an_unusable_race_key_is_refused_at_open() {
        let good = r#""race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":9.72}"#;
        for bad in [
            r#""race":{"kind":"other","groups":8,"part_zoom":12,"crossover":9.72}"#,
            r#""race":{"kind":"race-dots","groups":7,"part_zoom":12,"crossover":9.72}"#,
            r#""race":{"kind":"race-dots","groups":8,"part_zoom":9,"crossover":9.72}"#,
            r#""race":{"kind":"race-dots","groups":8,"part_zoom":12}"#,
            // Review F1: parts for a continental view, or past their zoom.
            r#""race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":3}"#,
            r#""race":{"kind":"race-dots","groups":8,"part_zoom":12,"crossover":12.5}"#,
        ] {
            let payload = super::super::codec::encode(0, 2020, 100, &[(1, 1.0)]);
            let bytes = archive(&[(6, 10, 20, payload)], &RACE_METADATA.replace(good, bad), false);
            assert!(matches!(open(&bytes), Err(GridError::Server(_))), "{bad} opened");
        }
    }

    fn test_header(leaf_offset: u64, data_offset: u64) -> PmHeader {
        PmHeader {
            root_offset: 127,
            root_length: 0,
            metadata_offset: 16_384,
            metadata_length: 0,
            leaf_offset,
            leaf_length: 0,
            data_offset,
            internal_compression: 1,
            tile_compression: 1,
            tile_type: 0,
            min_zoom: 0,
            max_zoom: 0,
        }
    }

    /// Review F6: a tile whose kind does not match its key's -- a cell
    /// body under a part key, a part body under a cell key -- is an error.
    #[test]
    fn a_tile_of_the_wrong_kind_for_its_key_is_refused() {
        use crate::overlay::dotgrid::codec_cat;
        use crate::overlay::dotgrid::parts::{Part, Tri};
        let t = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let part = Part { tri: Tri { a: [x0 + 5, y0 + 7], b: [x0 + 900, y0 + 7], c: [x0 + 5, y0 + 1200] }, c: [1, 0, 0, 0, 0, 0, 0, 0] };
        let parts = codec_cat::encode_parts(t, &[part]);
        let cells = codec_cat::encode_cells(0, 100, &[(3, [0, 5, 0, 0, 0, 0, 0, 1])]);
        let source = open(&archive(&[(12, 1206, 1538, cells.clone()), (9, 150, 192, parts.clone())], RACE_METADATA, false)).unwrap();
        let cell_key = GridTileKey { tile: TileId { z: 9, x: 150, y: 192 }, level: 0, epoch: 2020, epoch_b: None };
        assert!(matches!(source.decode(&[GridTileKey::part(t, 2020)], &cells), Err(GridError::Server(_))));
        assert!(matches!(source.decode(&[cell_key], &parts), Err(GridError::Server(_))));
        assert!(source.decode(&[GridTileKey::part(t, 2020)], &parts).is_ok());
        assert!(source.decode(&[cell_key], &cells).is_ok());
    }

    /// Final review #6: a part tile whose header names another tile than
    /// the key it was fetched for is refused, as the hex source refuses a
    /// mis-keyed hex tile; under its own key it decodes.
    #[test]
    fn a_part_tile_under_another_part_key_is_refused() {
        use crate::overlay::dotgrid::codec_cat;
        use crate::overlay::dotgrid::parts::{Part, Tri};
        let t = TileId { z: 12, x: 1206, y: 1538 };
        let (x0, y0) = (1206i64 << 19, 1538i64 << 19);
        let part = Part { tri: Tri { a: [x0 + 5, y0 + 7], b: [x0 + 900, y0 + 7], c: [x0 + 5, y0 + 1200] }, c: [1, 0, 0, 0, 0, 0, 0, 0] };
        let parts = codec_cat::encode_parts(t, &[part]);
        let source = open(&archive(&[(12, 1206, 1538, parts.clone())], RACE_METADATA, false)).unwrap();
        assert!(source.decode(&[GridTileKey::part(t, 2020)], &parts).is_ok());
        let wrong = GridTileKey::part(TileId { x: 1207, ..t }, 2020);
        assert!(matches!(source.decode(&[wrong], &parts), Err(GridError::Server(_))));
    }
}
