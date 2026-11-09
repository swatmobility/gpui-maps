//! Byte-range coalescing: the internal tiles one map tile needs, as few
//! HTTP requests as their layout allows.
//!
//! A COG writes each level's tiles row by row, so the tiles of one map
//! tile are a handful of runs of adjacent ranges. The merging itself is
//! [`crate::range_fetch::coalesce`], shared with the GeoParquet and
//! FlatGeobuf readers; these are its types keyed by a COG's own identity
//! for an internal tile, `(level, plane, index)`.

/// One internal tile's bytes in the file, keyed `(level, plane, index)`.
pub type ByteRange = crate::range_fetch::ByteRange<(u8, u16, u32)>;

/// One request covering several internal tiles.
pub type RangeGroup = crate::range_fetch::RangeGroup<(u8, u16, u32)>;

/// [`crate::range_fetch::coalesce`] on a COG's internal tiles.
#[must_use]
pub fn coalesce(ranges: &[ByteRange], merge_gap: u64, max_request: u64) -> Vec<RangeGroup> {
    crate::range_fetch::coalesce(ranges, merge_gap, max_request)
}
