
/// The coordinate extent an MVT tile's geometry is expressed in.
///
/// Every vector tile this crate decodes declares `extent: 4096` -- the
/// value the spec makes the default and that every encoder in practice
/// writes -- so a tile-local coordinate runs `0..4096` on both axes. Both
/// the tessellator (`render::tessellate_plan_with_ranges`) and the symbol
/// pass (`symbol_pass::project_symbol_anchor`) need it to build a
/// tile-to-clip matrix, and it lived in `map_view.rs` until the symbol pass
/// moved out of that file and could no longer see it.
pub const MVT_EXTENT: u32 = 4096;

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum TagValue {
    String(String),
    Number(f64),
    Bool(bool),
}

/// One decoded layer's MVT tag dictionary: its keys, each distinct string
/// once, and its non-null values, in wire order.
///
/// Every feature of a decoded layer shares ONE of these through its
/// [`Tags`], which is what lets style matching work in integer ids: a
/// filter's key is resolved to a key id once per (tile layer, style), and
/// a predicate on a value is decided once per value id rather than once
/// per feature -- see `style::columnar`. MVT ships exactly this dictionary
/// on the wire (`Tile.Layer.keys`/`values`, features referencing them by
/// index); decoding used to expand it into a `HashMap<String, TagValue>`
/// per feature and throw the ids away.
#[derive(Debug, Default, PartialEq)]
pub struct TagDict {
    /// Distinct key strings. The wire's `keys` table may repeat a string;
    /// the decoder folds repeats onto the first id, so a key string has
    /// exactly one id here.
    pub keys: Vec<String>,
    /// The wire's `values` table with its NULLs removed (a NULL-valued tag
    /// is dropped at decode, as it always was).
    pub values: Vec<TagValue>,
}

impl TagDict {
    /// The id of `key`, if any feature in this layer can carry it.
    #[must_use]
    pub fn key_id(&self, key: &str) -> Option<u32> {
        self.keys.iter().position(|k| k == key).map(|i| i as u32)
    }
}

/// Read access to a feature's tags by key: implemented by the decoder's
/// dictionary-encoded [`Tags`] and by a plain `HashMap<String, TagValue>`
/// (overlay properties, tests), so style evaluation takes either through
/// `&dyn TagLookup`.
pub trait TagLookup: std::fmt::Debug {
    fn get(&self, key: &str) -> Option<&TagValue>;
    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

impl TagLookup for HashMap<String, TagValue> {
    fn get(&self, key: &str) -> Option<&TagValue> {
        HashMap::get(self, key)
    }
    fn contains_key(&self, key: &str) -> bool {
        HashMap::contains_key(self, key)
    }
}

/// A feature's tags as MVT sent them: `(key id, value id)` pairs into the
/// layer's shared [`TagDict`].
///
/// Reads like the `HashMap<String, TagValue>` it replaced (`get`,
/// `contains_key`, `iter`, `len`), with the map's semantics: a key appears
/// at most once (a repeated key on the wire keeps its LAST value, as
/// `HashMap::insert` did), and a NULL value is no tag at all.
///
/// `get` is a linear scan comparing key strings. A feature carries ~6 tags
/// on the Singapore fixture (11,687 entries over 1,950 features), where a
/// scan of short strings costs about what hashing the probe key did -- and
/// the hot consumer, style filtering, no longer calls `get` at all.
#[derive(Clone)]
pub struct Tags {
    dict: std::sync::Arc<TagDict>,
    pairs: Box<[(u32, u32)]>,
}

static EMPTY_TAG_DICT: std::sync::LazyLock<std::sync::Arc<TagDict>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(TagDict::default()));

impl Default for Tags {
    fn default() -> Self {
        Self { dict: EMPTY_TAG_DICT.clone(), pairs: Box::new([]) }
    }
}

impl Tags {
    /// Tags over `dict`. Every id must be in range and each key id may
    /// appear at most once.
    #[must_use]
    pub fn from_pairs(dict: std::sync::Arc<TagDict>, pairs: Box<[(u32, u32)]>) -> Self {
        debug_assert!(pairs.iter().all(|&(k, v)| (k as usize) < dict.keys.len() && (v as usize) < dict.values.len()));
        Self { dict, pairs }
    }

    /// The layer dictionary these tags index into.
    #[must_use]
    pub fn dict(&self) -> &std::sync::Arc<TagDict> {
        &self.dict
    }

    /// The raw `(key id, value id)` pairs.
    #[must_use]
    pub fn pairs(&self) -> &[(u32, u32)] {
        &self.pairs
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&TagValue> {
        self.pairs
            .iter()
            .find(|&&(k, _)| self.dict.keys[k as usize] == key)
            .map(|&(_, v)| &self.dict.values[v as usize])
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Pair slots held -- `len`, since the pairs are an exact-sized box.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.pairs.len()
    }

    /// `(key, value)` in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TagValue)> + '_ {
        self.pairs.iter().map(|&(k, v)| (self.dict.keys[k as usize].as_str(), &self.dict.values[v as usize]))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /// An owned map -- for a public result that hands tags out by value.
    #[must_use]
    pub fn to_map(&self) -> HashMap<String, TagValue> {
        self.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect()
    }
}

impl TagLookup for Tags {
    fn get(&self, key: &str) -> Option<&TagValue> {
        Tags::get(self, key)
    }
}

impl std::fmt::Debug for Tags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Order-insensitive, like the map it replaced.
impl PartialEq for Tags {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl PartialEq<HashMap<String, TagValue>> for Tags {
    fn eq(&self, other: &HashMap<String, TagValue>) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| HashMap::get(other, k) == Some(v))
    }
}

impl PartialEq<Tags> for HashMap<String, TagValue> {
    fn eq(&self, other: &Tags) -> bool {
        other == self
    }
}

/// Last value wins for a repeated key, as `HashMap::insert` would.
impl FromIterator<(String, TagValue)> for Tags {
    fn from_iter<I: IntoIterator<Item = (String, TagValue)>>(iter: I) -> Self {
        let mut dict = TagDict::default();
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for (key, value) in iter {
            let v = dict.values.len() as u32;
            dict.values.push(value);
            match dict.key_id(&key) {
                Some(k) => match pairs.iter_mut().find(|(pk, _)| *pk == k) {
                    Some(pair) => pair.1 = v,
                    None => pairs.push((k, v)),
                },
                None => {
                    pairs.push((dict.keys.len() as u32, v));
                    dict.keys.push(key);
                }
            }
        }
        Self { dict: std::sync::Arc::new(dict), pairs: pairs.into_boxed_slice() }
    }
}

impl From<HashMap<String, TagValue>> for Tags {
    fn from(map: HashMap<String, TagValue>) -> Self {
        map.into_iter().collect()
    }
}

impl<'a> IntoIterator for &'a Tags {
    type Item = (&'a str, &'a TagValue);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a TagValue)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[derive(Debug, Clone)]
pub struct Feature {
    pub geometry: geo_types::Geometry<i32>,
    pub tags: Tags,
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone)]
pub struct DecodedTile {
    /// A hash of the bytes [`decode`] read this tile from, taken once at
    /// decode so a consumer can tell two tiles' data apart without walking
    /// their geometry (the capture session's mesh cache keys on it). `0`
    /// means "built by hand, not fingerprinted"; [`Self::fingerprint`]
    /// then hashes the layers instead. A tile whose `layers` are edited
    /// after decode must reset this to `0`.
    pub data_hash: u64,
    pub layers: Vec<Layer>,
}

/// Every heap block one decoded geometry holds, in
/// [`crate::render::heap_block_bytes`]' unit.
///
/// Shared by [`DecodedTile::footprint_bytes`] and by
/// `style::TileMatch::footprint_bytes`, which weigh two different copies of
/// the same shapes: the decoder's, and the clipped/boundary-converted ones
/// `style::match_tile_static` derives from them. Both are real, separately
/// allocated, separately cached bytes -- on the dense Tokyo z14 tile, 2.39
/// MB in the decoded tile and 7.76 MB in its `TileMatch` -- so the same
/// walk has to be available to both rather than one guessing at the other.
///
/// The container `Vec`s count. A `MultiPolygon`'s `Vec<Polygon>` and a
/// polygon's interiors `Vec<LineString>` carry no coordinates of their own
/// and are heap blocks all the same; a walk that only summed coordinate
/// buffers would miss 2.43 MB of spine on that same tile.
///
/// The four variants that store their coordinates inside the enum
/// (`Point`, `Line`, `Triangle`, `Rect`) contribute nothing, which is not
/// an omission: `mvt::decode` produces a `Point` for a single-point
/// feature, and a tile of 25,264 point features genuinely holds no
/// geometry blocks at all.
#[must_use]
pub fn geometry_footprint_bytes(geometry: &geo_types::Geometry<i32>) -> usize {
    use crate::render::vec_block_bytes as block;
    use geo_types::Geometry as G;

    fn ring(line: &geo_types::LineString<i32>) -> usize {
        block::<geo_types::Coord<i32>>(line.0.capacity())
    }
    fn polygon(p: &geo_types::Polygon<i32>) -> usize {
        // `interiors()` yields a slice, which has a length but no
        // capacity. `shrink_geometry` has already trimmed this `Vec` to
        // its length, so the two agree -- and this walk runs on geometry
        // that came through `decode` or through `style`'s clipping, both
        // of which `collect` into an exact-sized `Vec`.
        let interiors = p.interiors();
        ring(p.exterior())
            + block::<geo_types::LineString<i32>>(interiors.len())
            + interiors.iter().map(ring).sum::<usize>()
    }
    match geometry {
        G::Point(_) | G::Line(_) | G::Triangle(_) | G::Rect(_) => 0,
        G::LineString(l) => ring(l),
        G::MultiPoint(m) => block::<geo_types::Point<i32>>(m.0.capacity()),
        G::MultiLineString(m) => {
            block::<geo_types::LineString<i32>>(m.0.capacity())
                + m.0.iter().map(ring).sum::<usize>()
        }
        G::Polygon(p) => polygon(p),
        G::MultiPolygon(m) => {
            block::<geo_types::Polygon<i32>>(m.0.capacity())
                + m.0.iter().map(polygon).sum::<usize>()
        }
        G::GeometryCollection(c) => {
            block::<geo_types::Geometry<i32>>(c.0.capacity())
                + c.0.iter().map(geometry_footprint_bytes).sum::<usize>()
        }
    }
}

impl DecodedTile {
    /// This tile's data fingerprint: [`Self::data_hash`] when the tile was
    /// decoded from bytes (free), else a hash of the layers' full `Debug`
    /// form (a hand-built tile: small, and correct whatever it holds).
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        if self.data_hash != 0 {
            return self.data_hash;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.layers).hash(&mut h);
        h.finish()
    }

    /// Every heap block this tile holds, in
    /// [`crate::render::heap_block_bytes`]' unit -- the weight
    /// `MapView`'s `decoded_cache` is bounded by.
    ///
    /// # Why the cache needs this at all
    ///
    /// `decoded_cache` used to be bounded at 512 ENTRIES, which is a bound
    /// on the wrong quantity: this figure is 16 bytes for an offshore tile
    /// carrying two point features and **11.81 MB** for the real dense
    /// Tokyo z14 tile (14/14550/6455 -- 713,612 bytes on the wire, 25,264
    /// features, 49,970 tag entries), a spread of nearly six orders of
    /// magnitude. 512 entries therefore expressed anything from 8 KB to
    /// 6.0 GB, and the session that was OS-killed twice at 3.4 GB RSS was
    /// well inside it. A count cannot say "keep as much as fits in a
    /// budget"; this is what lets it.
    ///
    /// # What it walks, and what that costs
    ///
    /// The layer spine, every layer name, the feature spine, every
    /// feature's geometry ([`geometry_footprint_bytes`]) and every
    /// feature's tag map -- the hashbrown table plus each key `String` and
    /// each `TagValue::String`'s buffer. The tag maps are the bulk of it:
    /// 6.99 MB of the dense tile's 11.81 MB, against 2.39 MB of geometry
    /// and 2.43 MB of spine, which is the same finding the interactivity
    /// study reported from the other direction (1.35 MB/tile of properties
    /// against 76.5 KB on the wire).
    ///
    /// **123 microseconds** on that tile, which is why
    /// `MapView::install_baked_tile` does not call this: `bake_tile`
    /// computes it on the tokio worker that produced the tile and passes
    /// the figure to `TileCache::put_with_bytes`. A screen's worth of
    /// installs, across all five values a `BakedTile` carries, is ~6 ms,
    /// and a frame has 8.33.
    ///
    /// # What it does not count
    ///
    /// The `DecodedTile` struct itself, and the `Arc` it is cached behind.
    /// Both are fixed-size -- 24 bytes and a 16-byte refcount header --
    /// and the cache's ENTRY bound is what covers per-entry overhead; see
    /// `TileCache::with_byte_budget` on why both bounds exist.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        use crate::render::{heap_block_bytes, vec_block_bytes};

        let mut bytes = vec_block_bytes::<Layer>(self.layers.capacity());
        for layer in &self.layers {
            let mut counted_dicts: Vec<std::sync::Arc<TagDict>> = Vec::new();
            bytes += heap_block_bytes(layer.name.capacity());
            bytes += vec_block_bytes::<Feature>(layer.features.capacity());
            for feature in &layer.features {
                bytes += geometry_footprint_bytes(&feature.geometry);
                bytes += vec_block_bytes::<(u32, u32)>(feature.tags.capacity());
                // The dictionary is shared by the layer's features: weigh
                // each distinct one once.
                let dict = feature.tags.dict();
                if !counted_dicts.iter().any(|d| std::sync::Arc::ptr_eq(d, dict)) {
                    counted_dicts.push(dict.clone());
                    bytes += vec_block_bytes::<String>(dict.keys.capacity());
                    bytes += dict.keys.iter().map(|k| heap_block_bytes(k.capacity())).sum::<usize>();
                    bytes += vec_block_bytes::<TagValue>(dict.values.capacity());
                    for value in &dict.values {
                        if let TagValue::String(text) = value {
                            bytes += heap_block_bytes(text.capacity());
                        }
                    }
                }
            }
        }
        bytes
    }
}

/// Trims a decoded geometry's coordinate `Vec`s to the coordinates they
/// actually hold.
///
/// This exists because of a measured fact, not a suspicion. `mvt-reader`
/// sizes every ring to the varint count of the WHOLE feature
/// (`mvt-reader-2.4.0/src/lib.rs:361,379,414` -- three
/// `Vec::with_capacity(geometry_data.len())` sites) and never shrinks the
/// result, so a polygon with twelve rings allocates twelve buffers each big
/// enough for all twelve. Measured across the 20 tiles of a z14 Singapore
/// screen: 270,240 coordinates held in 75,844,994 slots -- **281 allocated
/// slots per slot used**, 683 MB of heap for 2.2 MB of coordinates. That
/// over-allocation was 91% of this renderer's entire per-tile memory cost.
///
/// The trim costs one realloc-and-copy per `Vec`, which is the reason it is
/// done here and not left to whoever reads the geometry: doing it once at
/// decode is the only moment at which the copy is paid once per tile rather
/// than once per bake.
///
/// **It cannot change what is drawn.** Capacity is invisible to every
/// reader; `len` and every coordinate are untouched. `Polygon` is
/// deconstructed and rebuilt rather than mutated through `exterior_mut`
/// only because `into_inner` also exposes the interiors `Vec` itself, which
/// is a heap block too; `Polygon::new`'s ring-closing is idempotent on
/// rings the decoder has already closed.
fn shrink_geometry(geometry: &mut geo_types::Geometry<i32>) {
    use geo_types::Geometry as G;

    fn ring(line: &mut geo_types::LineString<i32>) {
        line.0.shrink_to_fit();
    }

    fn polygon(p: &mut geo_types::Polygon<i32>) {
        let taken = std::mem::replace(p, geo_types::Polygon::new(geo_types::LineString::new(Vec::new()), Vec::new()));
        let (mut exterior, mut interiors) = taken.into_inner();
        ring(&mut exterior);
        for interior in &mut interiors {
            ring(interior);
        }
        interiors.shrink_to_fit();
        *p = geo_types::Polygon::new(exterior, interiors);
    }

    match geometry {
        G::LineString(l) => ring(l),
        G::MultiPoint(m) => m.0.shrink_to_fit(),
        G::MultiLineString(m) => {
            for l in &mut m.0 {
                ring(l);
            }
            m.0.shrink_to_fit();
        }
        G::Polygon(p) => polygon(p),
        G::MultiPolygon(m) => {
            for p in &mut m.0 {
                polygon(p);
            }
            m.0.shrink_to_fit();
        }
        G::GeometryCollection(c) => {
            for g in &mut c.0 {
                shrink_geometry(g);
            }
            c.0.shrink_to_fit();
        }
        // Coordinates inside the enum, no heap block to trim.
        G::Point(_) | G::Line(_) | G::Triangle(_) | G::Rect(_) => {}
    }
}

/// Decodes one vector tile.
///
/// # Why this crate parses MVT itself
///
/// It used to be `mvt_reader::Reader::get_features_as::<i32>`, and the
/// change is not about taste. `mvt-reader`'s `parse_geometry` sizes EVERY
/// ring to the varint count of the WHOLE feature
/// (`mvt-reader-2.4.0/src/lib.rs:361,379,414`) and starts a fresh such `Vec`
/// at every `MoveTo` and every `ClosePath`. The peak that costs is
///
/// ```text
/// sum over features of  (parts x varints x 8 bytes)
/// ```
///
/// -- quadratic in the size of a feature, because a feature with more parts
/// also has more varints for each of those parts to be sized against.
///
/// `shrink_geometry` (below) already trimmed the RESULT, and on a 64-bit
/// target that was enough: the over-sized blocks were mostly never faulted
/// in, so the peak was address space rather than resident pages and did not
/// show up in RSS. **In a browser it is fatal.** wasm32 has 4 GiB of address
/// space in total, `memory.grow` fails when it is gone, and a failed Rust
/// allocation is `abort` -- a wasm `unreachable`, which cannot be caught,
/// leaves the frame loop's `RefCell` borrowed forever and freezes the map on
/// its last presented frame with no error a user can see.
///
/// Measured on the tiles this renderer actually asks for, at Tokyo Station,
/// counting the crate's own allocation rule against the tile's real
/// commands:
///
/// ```text
/// zoom   coordinates held   mvt-reader peak   this decoder's peak
/// z14             0.1 MB            0.3 MB                0.1 MB
/// z13             0.1 MB          123.8 MB                0.1 MB
/// z12             0.5 MB        3,363.6 MB                0.5 MB
/// z11             0.3 MB           19.4 MB                0.3 MB
/// z10             0.3 MB          106.3 MB                0.3 MB
/// z8              0.4 MB          437.7 MB                0.4 MB
/// z6              0.8 MB        1,750.0 MB                0.8 MB
/// ```
///
/// One z12 tile asks for 3.36 GB to hold 0.5 MB. That is why the browser
/// build died on the FIRST z12 tile it ever fetched, at any window size,
/// with or without the prefetch cover, on a cold load -- and why it is a
/// spike at z12 rather than a slope: z12 is where this tileset merges its
/// road network into a handful of thousand-part `MultiLineString`s, and
/// `parts x varints` is worst exactly there. Any tileset that generalises
/// by merging linework has the same shape, which is why the same freeze was
/// reported against OpenFreeMap.
///
/// # What is deliberately identical
///
/// The geometry algorithm below is `mvt-reader`'s, command for command,
/// including the details that are arguably wrong but are what the rest of
/// this renderer was built and fingerprinted against:
///
///   * a `Point` feature always decodes to `MultiPoint`, even with one point;
///   * a `Polygon` feature always decodes to `MultiPolygon`, because the
///     final ring is still pending in `linestrings` at the end;
///   * ring winding is `shoelace_formula`'s, evaluated in `f32`;
///   * a cursor that overflows `i32` clamps to `i32::MAX`;
///   * a feature with no `type` field is skipped, and a feature whose type
///     is `UNKNOWN` fails the whole tile.
///
/// `mvt-reader` is kept as a dev-dependency and
/// `agrees_with_mvt_reader_coordinate_for_coordinate` asserts that
/// agreement on every fixture rather than leaving it to this comment.
///
/// The only thing that changes is capacity, which no reader can observe.
pub fn decode(bytes: Vec<u8>) -> anyhow::Result<DecodedTile> {
    let data_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        // Never 0: that value means "not fingerprinted".
        h.finish().max(1)
    };
    let mut pbf = Pbf::new(&bytes);
    let mut layers = Vec::new();
    while !pbf.eof() {
        let (field, wire) = pbf.key()?;
        match (field, wire) {
            (3, 2) => layers.push(decode_layer(pbf.bytes()?)?),
            _ => pbf.skip(wire)?,
        }
    }
    Ok(DecodedTile { data_hash, layers })
}

/// One `Tile.Layer`.
///
/// Two passes, because protobuf fields may arrive in any order and a
/// feature's tags index into `keys`/`values` that may not have been read
/// yet. The first pass only remembers where each feature's bytes are.
fn decode_layer(bytes: &[u8]) -> anyhow::Result<Layer> {
    let mut name = String::new();
    let mut version: u32 = 1;
    let mut keys: Vec<String> = Vec::new();
    // `Option`, so "this value was NULL" stays distinguishable from "there
    // is no value at that index": the first is a tag the old code dropped,
    // the second is a malformed tile.
    let mut values: Vec<Option<TagValue>> = Vec::new();
    let mut feature_bytes: Vec<&[u8]> = Vec::new();

    let mut pbf = Pbf::new(bytes);
    while !pbf.eof() {
        let (field, wire) = pbf.key()?;
        match (field, wire) {
            (1, 2) => name = String::from_utf8(pbf.bytes()?.to_vec())?,
            (2, 2) => feature_bytes.push(pbf.bytes()?),
            (3, 2) => keys.push(String::from_utf8(pbf.bytes()?.to_vec())?),
            (4, 2) => values.push(decode_value(pbf.bytes()?)?),
            (15, 0) => version = pbf.varint()? as u32,
            _ => pbf.skip(wire)?,
        }
    }
    // `mvt-reader`'s `process_layers` refuses anything else, and refuses it
    // for the whole tile rather than the layer.
    anyhow::ensure!(
        matches!(version, 1 | 2),
        "unsupported vector tile version {version} in layer {name:?}"
    );
    // The wire dictionary, kept: repeated key strings fold onto one id,
    // NULL values are dropped (a NULL-valued tag was never a tag).
    let raw_keys = keys.len();
    let raw_values = values.len();
    let mut dict = TagDict { keys: Vec::with_capacity(raw_keys), values: Vec::with_capacity(raw_values) };
    let mut key_ids: Vec<u32> = Vec::with_capacity(raw_keys);
    for key in keys {
        let id = match dict.key_id(&key) {
            Some(id) => id,
            None => {
                dict.keys.push(key);
                (dict.keys.len() - 1) as u32
            }
        };
        key_ids.push(id);
    }
    let value_ids: Vec<Option<u32>> = values
        .into_iter()
        .map(|value| {
            value.map(|value| {
                dict.values.push(value);
                (dict.values.len() - 1) as u32
            })
        })
        .collect();
    let remap = TagRemap { key_ids: &key_ids, value_ids: &value_ids, raw_keys, raw_values };
    let dict = std::sync::Arc::new(dict);
    let mut features = Vec::with_capacity(feature_bytes.len());
    for raw in feature_bytes {
        if let Some(feature) = decode_feature(raw, &remap, &dict)? {
            features.push(feature);
        }
    }
    Ok(Layer { name, features })
}

/// Wire key/value indices to [`TagDict`] ids.
struct TagRemap<'a> {
    key_ids: &'a [u32],
    value_ids: &'a [Option<u32>],
    raw_keys: usize,
    raw_values: usize,
}

/// One `Tile.Feature`. `None` when the feature carries no `type`, which is
/// the case `mvt-reader` silently drops.
fn decode_feature(
    bytes: &[u8],
    remap: &TagRemap<'_>,
    dict: &std::sync::Arc<TagDict>,
) -> anyhow::Result<Option<Feature>> {
    let mut geom_type: Option<u32> = None;
    let mut tag_indices: Vec<u32> = Vec::new();
    let mut geometry_bytes: &[u8] = &[];

    let mut pbf = Pbf::new(bytes);
    while !pbf.eof() {
        let (field, wire) = pbf.key()?;
        match (field, wire) {
            (2, 2) => {
                let packed = pbf.bytes()?;
                let mut inner = Pbf::new(packed);
                while !inner.eof() {
                    tag_indices.push(inner.varint()? as u32);
                }
            }
            (2, 0) => tag_indices.push(pbf.varint()? as u32),
            (3, 0) => geom_type = Some(pbf.varint()? as u32),
            (4, 2) => geometry_bytes = pbf.bytes()?,
            _ => pbf.skip(wire)?,
        }
    }
    let Some(geom_type) = geom_type else { return Ok(None) };

    let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(tag_indices.len() / 2);
    for pair in tag_indices.chunks(2) {
        anyhow::ensure!(pair.len() == 2, "a vector tile feature has an odd number of tag indices");
        let (k, v) = (pair[0] as usize, pair[1] as usize);
        anyhow::ensure!(
            k < remap.raw_keys && v < remap.raw_values,
            "a vector tile feature indexes key {k} of {} / value {v} of {}",
            remap.raw_keys,
            remap.raw_values
        );
        if let Some(value_id) = remap.value_ids[v] {
            let key_id = remap.key_ids[k];
            // `HashMap::insert` semantics: a repeated key keeps its last value.
            match pairs.iter_mut().find(|(pk, _)| *pk == key_id) {
                Some(existing) => existing.1 = value_id,
                None => pairs.push((key_id, value_id)),
            }
        }
    }
    let tags = Tags::from_pairs(dict.clone(), pairs.into_boxed_slice());

    let mut geometry = parse_geometry(geometry_bytes, geom_type)?;
    // Growth is by doubling, so a ring can still hold up to twice what it
    // needs. The trim is what makes a resident `DecodedTile` cost its
    // coordinates and nothing else -- see [`shrink_geometry`]. What changed
    // is the PEAK it is trimming down FROM: bounded by the coordinates now,
    // rather than by `parts x varints`.
    shrink_geometry(&mut geometry);
    Ok(Some(Feature { geometry, tags }))
}

/// One `Tile.Value`. The precedence is `mvt-reader`'s `map_value`: the first
/// field present in declaration order wins. `None` is its `Value::Null`,
/// which `convert_value` drops.
fn decode_value(bytes: &[u8]) -> anyhow::Result<Option<TagValue>> {
    let (mut s, mut f, mut d, mut i, mut u, mut si, mut b) = (None, None, None, None, None, None, None);
    let mut pbf = Pbf::new(bytes);
    while !pbf.eof() {
        let (field, wire) = pbf.key()?;
        match (field, wire) {
            (1, 2) => s = Some(String::from_utf8(pbf.bytes()?.to_vec())?),
            (2, 5) => f = Some(f32::from_bits(pbf.fixed32()?)),
            (3, 1) => d = Some(pbf.fixed64()?),
            (4, 0) => i = Some(pbf.varint()? as i64),
            (5, 0) => u = Some(pbf.varint()?),
            (6, 0) => si = Some(zigzag64(pbf.varint()?)),
            (7, 0) => b = Some(pbf.varint()? != 0),
            _ => pbf.skip(wire)?,
        }
    }
    Ok(s.map(TagValue::String)
        .or_else(|| f.map(|v| TagValue::Number(v as f64)))
        .or_else(|| d.map(TagValue::Number))
        .or_else(|| i.map(|v| TagValue::Number(v as f64)))
        .or_else(|| u.map(|v| TagValue::Number(v as f64)))
        .or_else(|| si.map(|v| TagValue::Number(v as f64)))
        .or_else(|| b.map(TagValue::Bool)))
}

fn zigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

const GEOM_UNKNOWN: u32 = 0;
const GEOM_POINT: u32 = 1;
const GEOM_LINESTRING: u32 = 2;
const GEOM_POLYGON: u32 = 3;
/// Parameters per coordinate. `mvt-reader`'s `DIMENSION`.
const DIMENSION: u32 = 2;

/// `mvt-reader`'s `shoelace_formula`, including its `f32` accumulator.
///
/// Kept in `f32` deliberately: it decides ring winding, and winding decides
/// which rings become interiors. Widening it to `f64` would be a better
/// formula and a different map.
fn shoelace(points: &[geo_types::Coord<i32>]) -> f32 {
    let n = points.len();
    if n == 0 {
        return 0.0;
    }
    let mut area: f32 = 0.0;
    let mut v1 = points[n - 1];
    for v2 in points {
        area += (v2.y as f32 - v1.y as f32) * (v2.x as f32 + v1.x as f32);
        v1 = *v2;
    }
    area * 0.5
}

/// The geometry command stream, decoded straight out of its packed varints.
///
/// Structurally `mvt-reader`'s `parse_geometry` with ONE difference: every
/// coordinate `Vec` starts empty and grows, instead of being sized for the
/// whole feature. See [`decode`] for what that difference is worth.
fn parse_geometry(bytes: &[u8], geom_type: u32) -> anyhow::Result<geo_types::Geometry<i32>> {
    use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
    anyhow::ensure!(geom_type != GEOM_UNKNOWN, "a vector tile feature has geometry type UNKNOWN");
    anyhow::ensure!(geom_type <= GEOM_POLYGON, "unknown vector tile geometry type {geom_type}");

    let mut coordinates: Vec<Coord<i32>> = Vec::new();
    let mut polygons: Vec<Polygon<i32>> = Vec::new();
    let mut linestrings: Vec<LineString<i32>> = Vec::new();
    let mut cursor: [i32; 2] = [0, 0];
    let mut parameter_count: u32 = 0;

    let mut pbf = Pbf::new(bytes);
    while !pbf.eof() {
        let value = u32::try_from(pbf.varint()?)
            .map_err(|_| anyhow::anyhow!("a geometry varint does not fit in u32"))?;
        if parameter_count == 0 {
            match value & 0x7 {
                1 => {
                    // MoveTo
                    parameter_count = (value >> 3) * DIMENSION;
                    if geom_type == GEOM_LINESTRING && !coordinates.is_empty() {
                        linestrings.push(LineString::new(std::mem::take(&mut coordinates)));
                    }
                }
                2 => parameter_count = (value >> 3) * DIMENSION,
                7 => {
                    // ClosePath
                    let first = *coordinates
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("ClosePath with no coordinates"))?;
                    coordinates.push(first);
                    let ring = LineString::new(std::mem::take(&mut coordinates));
                    if shoelace(&ring.0) > 0.0 && !linestrings.is_empty() {
                        // An exterior ring: the rings gathered so far are a
                        // finished polygon.
                        let mut rings = linestrings.drain(..);
                        let exterior = rings.next().expect("non-empty, just checked");
                        polygons.push(Polygon::new(exterior, rings.collect()));
                    }
                    linestrings.push(ring);
                }
                _ => {}
            }
        } else {
            let integer_value = ((value >> 1) as i32) ^ -((value & 1) as i32);
            if parameter_count.is_multiple_of(DIMENSION) {
                cursor[0] = cursor[0].checked_add(integer_value).unwrap_or(i32::MAX);
            } else {
                cursor[1] = cursor[1].checked_add(integer_value).unwrap_or(i32::MAX);
                coordinates.push(Coord { x: cursor[0], y: cursor[1] });
            }
            parameter_count -= 1;
        }
    }

    Ok(match geom_type {
        GEOM_LINESTRING => {
            if linestrings.is_empty() {
                LineString::new(coordinates).into()
            } else {
                linestrings.push(LineString::new(coordinates));
                MultiLineString::new(linestrings).into()
            }
        }
        GEOM_POINT => {
            MultiPoint(coordinates.iter().map(|c| Point::new(c.x, c.y)).collect()).into()
        }
        _ => {
            if !linestrings.is_empty() {
                let mut rings = linestrings.drain(..);
                let exterior = rings.next().expect("non-empty, just checked");
                polygons.push(Polygon::new(exterior, rings.collect()));
                MultiPolygon::new(polygons).into()
            } else {
                polygons
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("a polygon feature closed no ring"))?
                    .into()
            }
        }
    })
}

/// The smallest protobuf reader that can read a vector tile.
///
/// Bounds-checked at every step and returning `Result` rather than
/// panicking, because the bytes come off the network: `decode` is called on
/// whatever a tile server answered with, and
/// `corrupt_bytes_produce_an_error_not_a_panic` holds it to that.
struct Pbf<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Pbf<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Pbf { bytes, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut value: u64 = 0;
        for shift in 0..10u32 {
            let byte = *self
                .bytes
                .get(self.pos)
                .ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(anyhow::anyhow!("varint longer than 10 bytes"))
    }

    /// `(field number, wire type)`.
    fn key(&mut self) -> anyhow::Result<(u32, u8)> {
        let key = self.varint()?;
        let field = u32::try_from(key >> 3)
            .map_err(|_| anyhow::anyhow!("protobuf field number out of range"))?;
        Ok((field, (key & 0x7) as u8))
    }

    fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let len = usize::try_from(self.varint()?)
            .map_err(|_| anyhow::anyhow!("length-delimited field longer than this target's usize"))?;
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("length-delimited field runs past the end of the message"))?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn fixed32(&mut self) -> anyhow::Result<u32> {
        let end = self
            .pos
            .checked_add(4)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated fixed32"))?;
        let out = u32::from_le_bytes(self.bytes[self.pos..end].try_into().expect("four bytes"));
        self.pos = end;
        Ok(out)
    }

    fn fixed64(&mut self) -> anyhow::Result<f64> {
        let end = self
            .pos
            .checked_add(8)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated fixed64"))?;
        let out = f64::from_le_bytes(self.bytes[self.pos..end].try_into().expect("eight bytes"));
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, wire: u8) -> anyhow::Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed64()?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.fixed32()?;
            }
            _ => anyhow::bail!("unsupported protobuf wire type {wire}"),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Probe-only instrumentation. See the `probe` feature in `Cargo.toml`.
// ---------------------------------------------------------------------------

/// Which half of a [`DecodedTile`] a measurement wants freed in place.
///
/// **Probe-only**, behind the `probe` cargo feature, because unlike
/// `MapView::drop_cache_for_measurement` -- which is safe at any moment,
/// since every cache it empties is already rebuilt lazily on the read that
/// misses -- this one is *destructive*. A tile whose tags have been freed
/// is still in `decoded_cache` and still looks like a tile; a rebake off it
/// would filter every data-driven expression against an empty property map
/// and draw a wrong map without saying so. A feature gate rather than a
/// doc comment is the difference between "nothing calls this" and "nothing
/// can call this", and only the second is worth relying on.
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedTilePart {
    /// The per-feature `tags`: every key `String`, every
    /// `TagValue::String`, and the `HashMap`'s own table.
    Tags,
    /// The per-feature geometry: every coordinate `Vec` under
    /// `geo_types::Geometry`.
    Geometry,
}

/// A structural count of what a set of [`DecodedTile`]s holds -- the
/// companion to the byte figures, because the bytes alone cannot say
/// whether interning would help.
///
/// `distinct_*` against the corresponding total is exactly the size of the
/// interning prize: today `decode` allocates a fresh `String` for every key
/// of every feature, so `key_bytes` is what is paid and
/// `distinct_key_bytes_overall` is what would be paid if one `Arc<str>` per
/// distinct key were shared across the whole cache.
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodedTileCensus {
    /// Tiles walked.
    pub tiles: usize,
    /// Layers across every tile walked.
    pub layers: usize,
    /// Features across every tile walked.
    pub features: usize,
    /// Key/value pairs across every feature -- i.e. how many key `String`s
    /// are allocated right now.
    pub tag_entries: usize,
    /// Summed over tiles: distinct keys WITHIN one tile. What per-tile
    /// interning would leave.
    pub distinct_keys_per_tile: usize,
    /// Distinct keys across every tile at once. What cache-wide interning
    /// would leave.
    pub distinct_keys_overall: usize,
    /// Total bytes of key text, counting every repeat.
    pub key_bytes: usize,
    /// Bytes of key text if each distinct key were stored once per cache.
    pub distinct_key_bytes_overall: usize,
    /// `TagValue::String` values, counting every repeat.
    pub string_values: usize,
    /// Distinct `TagValue::String` values across every tile at once.
    pub distinct_string_values_overall: usize,
    /// Total bytes of string-valued tag text, counting every repeat.
    pub string_value_bytes: usize,
    /// Bytes of string-valued tag text if each distinct value were stored
    /// once per cache.
    pub distinct_string_value_bytes_overall: usize,
    /// Coordinates across every feature's geometry -- `Vec::len`, i.e.
    /// the coordinates that are actually there.
    pub geometry_coords: usize,
    /// The same `Vec`s' `capacity`, i.e. the coordinate slots actually
    /// allocated. Counted separately from `geometry_coords` because a
    /// size-summing walk over `len` would report the data and miss the
    /// allocation, and it is the allocation the heap is holding.
    pub geometry_coord_capacity: usize,
    /// How many coordinate `Vec`s those figures are spread over -- one
    /// per `LineString`, one per polygon ring.
    pub geometry_vecs: usize,
    /// What the ALLOCATOR set aside for every heap block under those
    /// geometries -- `malloc_size` of each live `Vec` buffer, coordinate
    /// `Vec`s and the container `Vec`s that hold them alike.
    ///
    /// The third of three figures for the same thing, and the only one
    /// that is the truth: `geometry_coords * 8` is the data, `capacity *
    /// 8` is what was requested, and this is what was given. The gap
    /// between the second and the third is size-class rounding, which on
    /// tens of thousands of allocations is not a rounding error.
    pub geometry_alloc_bytes: usize,
}

/// How many bytes the allocator actually set aside for the block at
/// `ptr`, which is at least the size that was asked for and often more.
///
/// `malloc_size` is in `libSystem` alongside the `malloc_zone_statistics`
/// the heap counter already uses (see `render::heap_in_use_bytes`), so
/// this costs no new dependency. It is the per-block form of the same
/// truth that counter reports in aggregate, and it is what makes the
/// difference between "capacity times element size" and "what the heap is
/// holding" measurable rather than argued about.
///
/// # Safety
/// `ptr` must be a live block returned by this process's allocator. Every
/// caller here passes `Vec::as_ptr` of a `Vec` with non-zero capacity --
/// a zero-capacity `Vec` has a dangling pointer and is skipped, because
/// asking the allocator about an address it never handed out is exactly
/// the mistake this doc comment exists to prevent.
#[cfg(feature = "probe")]
fn allocated_bytes<T>(vec: &[T]) -> usize {
    extern "C" {
        fn malloc_size(ptr: *const std::ffi::c_void) -> usize;
    }
    if vec.is_empty() {
        // A `Vec` can hold capacity with zero length, so `is_empty` is not
        // the right guard in general -- but every `Vec` reached here comes
        // straight from the MVT decoder and is non-empty whenever it has
        // been allocated at all. `as_ptr` on a capacity-0 `Vec` is
        // dangling, and `malloc_size` on a dangling pointer is undefined.
        return 0;
    }
    // SAFETY: `vec` is non-empty, so `as_ptr` is the live allocation's
    // base address, which is what `malloc_size` requires.
    unsafe { malloc_size(vec.as_ptr().cast()) }
}

/// `(coordinates, capacity, coordinate `Vec`s)` in one geometry,
/// recursively.
///
/// `len` and `capacity` are counted separately on purpose. They are the
/// same number for a `Vec` built by `collect`; they are emphatically not
/// for one built by `push` into a `with_capacity` guess that was never
/// trimmed, and the gap between them is invisible to any walk that adds up
/// `len` and calls the result the memory.
///
/// The variants that carry their coordinates inline (`Point`, `Line`,
/// `Rect`, `Triangle`) allocate nothing, so they contribute to the
/// coordinate count and to neither of the other two.
#[cfg(feature = "probe")]
fn coords_in(geometry: &geo_types::Geometry<i32>) -> GeometryCost {
    use geo_types::Geometry as G;
    fn ring(l: &geo_types::LineString<i32>) -> GeometryCost {
        GeometryCost {
            coords: l.0.len(),
            capacity: l.0.capacity(),
            vecs: 1,
            alloc_bytes: allocated_bytes(&l.0),
        }
    }
    fn polygon(p: &geo_types::Polygon<i32>) -> GeometryCost {
        // The interiors `Vec<LineString>` is a heap block of its own, and
        // `geo_types` gives no borrow of it -- `interiors()` yields a
        // slice, whose pointer IS that block's base whenever it is
        // non-empty, which is all `allocated_bytes` needs.
        let interiors = p.interiors();
        std::iter::once(p.exterior())
            .chain(interiors)
            .map(ring)
            .fold(GeometryCost::container(interiors), GeometryCost::add)
    }
    match geometry {
        G::Point(_) => GeometryCost::inline(1),
        G::Line(_) => GeometryCost::inline(2),
        G::Triangle(_) => GeometryCost::inline(3),
        G::Rect(_) => GeometryCost::inline(2),
        G::LineString(l) => ring(l),
        G::MultiPoint(m) => GeometryCost {
            coords: m.0.len(),
            capacity: m.0.capacity(),
            vecs: 1,
            alloc_bytes: allocated_bytes(&m.0),
        },
        G::MultiLineString(m) => {
            m.0.iter().map(ring).fold(GeometryCost::container(&m.0), GeometryCost::add)
        }
        G::Polygon(p) => polygon(p),
        G::MultiPolygon(m) => {
            m.0.iter().map(polygon).fold(GeometryCost::container(&m.0), GeometryCost::add)
        }
        G::GeometryCollection(c) => {
            c.0.iter().map(coords_in).fold(GeometryCost::container(&c.0), GeometryCost::add)
        }
    }
}

/// What one geometry (or one part of one) costs, in the three units the
/// census reports it in.
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GeometryCost {
    coords: usize,
    capacity: usize,
    vecs: usize,
    alloc_bytes: usize,
}

#[cfg(feature = "probe")]
impl GeometryCost {
    /// A variant whose coordinates live inside the enum, allocating
    /// nothing: it has coordinates, and no heap.
    fn inline(coords: usize) -> Self {
        Self { coords, capacity: 0, vecs: 0, alloc_bytes: 0 }
    }

    /// The `Vec` that HOLDS other geometries -- a `MultiPolygon`'s
    /// polygons, a polygon's interior rings. It carries no coordinates of
    /// its own, but it is a heap block, and this measurement is about
    /// heap blocks.
    fn container<T>(items: &[T]) -> Self {
        Self { coords: 0, capacity: 0, vecs: 0, alloc_bytes: allocated_bytes(items) }
    }

    fn add(self, other: Self) -> Self {
        Self {
            coords: self.coords + other.coords,
            capacity: self.capacity + other.capacity,
            vecs: self.vecs + other.vecs,
            alloc_bytes: self.alloc_bytes + other.alloc_bytes,
        }
    }
}

impl DecodedTile {
    /// Frees one half of every feature in place, leaving the other half and
    /// the `Vec<Layer>`/`Vec<Feature>` spine exactly where they were.
    ///
    /// In place, and not via a stripped clone, on purpose: a clone would
    /// allocate the surviving half again before the original went, so the
    /// heap either side of the call would measure a copy rather than a
    /// free. `HashMap::clear` is likewise not enough -- it drops the
    /// entries but keeps the table allocation, and the table is part of
    /// what is being weighed -- so the map is REPLACED with a fresh empty
    /// one, whose table is not allocated until something is inserted.
    ///
    /// Probe-only; see [`DecodedTilePart`].
    #[cfg(feature = "probe")]
    pub fn strip(&mut self, part: DecodedTilePart) {
        // The layers no longer are what the bytes decoded to: per the
        // field's contract, the fingerprint falls back to the layers.
        self.data_hash = 0;
        for layer in &mut self.layers {
            for feature in &mut layer.features {
                match part {
                    DecodedTilePart::Tags => feature.tags = Tags::default(),
                    DecodedTilePart::Geometry => {
                        feature.geometry = geo_types::Geometry::Point(geo_types::Point::new(0, 0));
                    }
                }
            }
        }
    }
}

/// Counts what a set of tiles holds, without freeing any of it.
///
/// Separate from [`DecodedTile::strip`] and called BEFORE the heap is
/// sampled, because the distinct-key sets it builds are themselves
/// allocations: counting inside the measured window would fold the
/// instrument's own working set into the figure it reports.
///
/// Probe-only; see [`DecodedTilePart`].
#[cfg(feature = "probe")]
pub fn census<'a>(tiles: impl Iterator<Item = &'a DecodedTile>) -> DecodedTileCensus {
    use std::collections::HashSet;

    let mut census = DecodedTileCensus::default();
    let mut keys_overall: HashSet<&str> = HashSet::new();
    let mut values_overall: HashSet<&str> = HashSet::new();
    for tile in tiles {
        census.tiles += 1;
        let mut keys_in_tile: HashSet<&str> = HashSet::new();
        for layer in &tile.layers {
            census.layers += 1;
            for feature in &layer.features {
                census.features += 1;
                let cost = coords_in(&feature.geometry);
                census.geometry_coords += cost.coords;
                census.geometry_coord_capacity += cost.capacity;
                census.geometry_vecs += cost.vecs;
                census.geometry_alloc_bytes += cost.alloc_bytes;
                for (key, value) in &feature.tags {
                    census.tag_entries += 1;
                    census.key_bytes += key.len();
                    keys_in_tile.insert(key);
                    keys_overall.insert(key);
                    if let TagValue::String(s) = value {
                        census.string_values += 1;
                        census.string_value_bytes += s.len();
                        values_overall.insert(s.as_str());
                    }
                }
            }
        }
        census.distinct_keys_per_tile += keys_in_tile.len();
    }
    census.distinct_keys_overall = keys_overall.len();
    census.distinct_key_bytes_overall = keys_overall.iter().map(|k| k.len()).sum();
    census.distinct_string_values_overall = values_overall.len();
    census.distinct_string_value_bytes_overall = values_overall.iter().map(|v| v.len()).sum();
    census
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_fixture_tile() -> Vec<u8> {
        let mut tile = mvt::Tile::new(4096);
        let mut layer = tile.create_layer("test_layer");

        // Polygon: a 10x10 square, tagged kind=polygon.
        let mut poly = mvt::GeomEncoder::new(mvt::GeomType::Polygon);
        poly.add_point(0.0, 0.0).unwrap();
        poly.add_point(10.0, 0.0).unwrap();
        poly.add_point(10.0, 10.0).unwrap();
        poly.add_point(0.0, 10.0).unwrap();
        poly.complete_geom().unwrap();
        let poly_data = poly.complete().unwrap().encode().unwrap();
        let mut poly_feature = layer.into_feature(poly_data);
        poly_feature.add_tag_string("kind", "polygon");
        layer = poly_feature.into_layer();

        // Linestring: a diagonal, tagged kind=line.
        let mut line = mvt::GeomEncoder::new(mvt::GeomType::Linestring);
        line.add_point(0.0, 0.0).unwrap();
        line.add_point(20.0, 20.0).unwrap();
        line.complete_geom().unwrap();
        let line_data = line.complete().unwrap().encode().unwrap();
        let mut line_feature = layer.into_feature(line_data);
        line_feature.add_tag_string("kind", "line");
        layer = line_feature.into_layer();

        tile.add_layer(layer).unwrap();
        tile.to_bytes().unwrap()
    }

    #[test]
    fn decodes_layer_name_and_feature_count() {
        let decoded = decode(build_fixture_tile()).expect("decode should succeed");
        assert_eq!(decoded.layers.len(), 1);
        assert_eq!(decoded.layers[0].name, "test_layer");
        assert_eq!(decoded.layers[0].features.len(), 2);
    }

    #[test]
    fn decodes_tags_and_broad_geometry_kind() {
        let decoded = decode(build_fixture_tile()).expect("decode should succeed");
        let features = &decoded.layers[0].features;

        let polygon_feature = features
            .iter()
            .find(|f| f.tags.get("kind") == Some(&TagValue::String("polygon".to_string())))
            .expect("polygon feature present");
        assert!(matches!(
            polygon_feature.geometry,
            geo_types::Geometry::Polygon(_) | geo_types::Geometry::MultiPolygon(_)
        ));

        let line_feature = features
            .iter()
            .find(|f| f.tags.get("kind") == Some(&TagValue::String("line".to_string())))
            .expect("line feature present");
        assert!(matches!(
            line_feature.geometry,
            geo_types::Geometry::LineString(_) | geo_types::Geometry::MultiLineString(_)
        ));
    }

    #[test]
    fn corrupt_bytes_produce_an_error_not_a_panic() {
        let result = decode(vec![1, 2, 3, 4, 5]);
        assert!(result.is_err());
    }
    /// `(len, capacity)` of every coordinate `Vec` under a geometry.
    ///
    /// Written out here rather than reusing the probe census because the
    /// trim is not a probe-only concern: the default suite has to be able
    /// to fail if the trim stops happening.
    fn coord_vec_shapes(geometry: &geo_types::Geometry<i32>) -> Vec<(usize, usize)> {
        use geo_types::Geometry as G;
        fn ring(l: &geo_types::LineString<i32>, out: &mut Vec<(usize, usize)>) {
            out.push((l.0.len(), l.0.capacity()));
        }
        fn polygon(p: &geo_types::Polygon<i32>, out: &mut Vec<(usize, usize)>) {
            ring(p.exterior(), out);
            for i in p.interiors() {
                ring(i, out);
            }
        }
        let mut out = Vec::new();
        match geometry {
            G::LineString(l) => ring(l, &mut out),
            G::MultiPoint(m) => out.push((m.0.len(), m.0.capacity())),
            G::MultiLineString(m) => {
                for l in &m.0 {
                    ring(l, &mut out);
                }
            }
            G::Polygon(p) => polygon(p, &mut out),
            G::MultiPolygon(m) => {
                for p in &m.0 {
                    polygon(p, &mut out);
                }
            }
            G::GeometryCollection(c) => {
                for g in &c.0 {
                    out.extend(coord_vec_shapes(g));
                }
            }
            G::Point(_) | G::Line(_) | G::Triangle(_) | G::Rect(_) => {}
        }
        out
    }

    /// A polygon with a hole and a two-part linestring, i.e. the shape the
    /// over-allocation is worst on: several rings in one feature, each of
    /// which the decoder sizes for all of them.
    fn build_multi_ring_tile() -> Vec<u8> {
        let mut tile = mvt::Tile::new(4096);
        let mut layer = tile.create_layer("rings");

        let mut poly = mvt::GeomEncoder::new(mvt::GeomType::Polygon);
        for (x, y) in [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)] {
            poly.add_point(x, y).unwrap();
        }
        poly.complete_geom().unwrap();
        for (x, y) in [(20.0, 20.0), (20.0, 80.0), (80.0, 80.0), (80.0, 20.0)] {
            poly.add_point(x, y).unwrap();
        }
        poly.complete_geom().unwrap();
        let poly_data = poly.complete().unwrap().encode().unwrap();
        let mut poly_feature = layer.into_feature(poly_data);
        poly_feature.add_tag_string("kind", "holed");
        layer = poly_feature.into_layer();

        let mut lines = mvt::GeomEncoder::new(mvt::GeomType::Linestring);
        for (x, y) in [(0.0, 0.0), (10.0, 10.0), (20.0, 5.0)] {
            lines.add_point(x, y).unwrap();
        }
        lines.complete_geom().unwrap();
        for (x, y) in [(50.0, 50.0), (60.0, 70.0)] {
            lines.add_point(x, y).unwrap();
        }
        lines.complete_geom().unwrap();
        let line_data = lines.complete().unwrap().encode().unwrap();
        let mut line_feature = layer.into_feature(line_data);
        line_feature.add_tag_string("kind", "multiline");
        layer = line_feature.into_layer();

        tile.add_layer(layer).unwrap();
        tile.to_bytes().unwrap()
    }

    #[test]
    fn decode_leaves_no_spare_coordinate_capacity() {
        // The regression guard for the 281-slots-per-slot-used finding. It
        // is asserted on a MULTI-RING feature because that is where the
        // decoder's whole-feature sizing does its damage: a single-ring
        // feature can be near-tight by accident.
        for bytes in [build_fixture_tile(), build_multi_ring_tile()] {
            let decoded = decode(bytes).expect("decode should succeed");
            let mut vecs = 0;
            for layer in &decoded.layers {
                for feature in &layer.features {
                    for (len, capacity) in coord_vec_shapes(&feature.geometry) {
                        assert_eq!(
                            capacity, len,
                            "a coordinate Vec kept {} spare slots for {len} coordinates",
                            capacity - len
                        );
                        vecs += 1;
                    }
                }
            }
            assert!(vecs > 0, "the fixture must contain coordinate Vecs to be a test of anything");
        }
    }

    #[test]
    fn mvt_reader_still_over_allocates_which_is_why_this_crate_decodes_mvt_itself() {
        // Proof that the crate this decoder replaced really does
        // over-allocate, so that if a future `mvt-reader` fixes it upstream
        // this test fails and says so rather than `decode`'s doc comment
        // quietly describing a crate that no longer behaves that way.
        let reader = mvt_reader::Reader::new(build_multi_ring_tile()).expect("fixture parses");
        let raw = reader.get_features_as::<i32>(0).expect("layer 0 decodes");
        let shapes: Vec<(usize, usize)> =
            raw.iter().flat_map(|f| coord_vec_shapes(&f.geometry)).collect();
        let len: usize = shapes.iter().map(|(l, _)| l).sum();
        let capacity: usize = shapes.iter().map(|(_, c)| c).sum();
        assert!(
            capacity > len * 2,
            "expected the untrimmed decoder to over-allocate; got {capacity} slots for {len} \
             coordinates"
        );
    }

    #[test]
    fn agrees_with_mvt_reader_coordinate_for_coordinate() {
        // THE test for replacing a decoder. `decode` no longer calls
        // `mvt-reader`; it parses the protobuf and the command stream
        // itself, and the ONLY thing that was meant to change is capacity.
        // `geo_types`' `PartialEq` compares coordinates and never capacity,
        // so this is exactly the comparison wanted: same geometry variant,
        // same rings, same winding, same coordinates, same tags.
        for bytes in [build_fixture_tile(), build_multi_ring_tile(), build_many_part_tile(64)] {
            let ours = decode(bytes.clone()).expect("decode should succeed");
            let reader = mvt_reader::Reader::new(bytes).expect("fixture parses");
            let names = reader.get_layer_names().expect("layer names");
            assert_eq!(names.len(), ours.layers.len(), "layer count");
            for (index, layer) in ours.layers.iter().enumerate() {
                assert_eq!(names[index], layer.name, "layer name");
                let theirs = reader.get_features_as::<i32>(index).expect("layer decodes");
                assert_eq!(theirs.len(), layer.features.len(), "feature count");
                for (theirs, ours) in theirs.iter().zip(&layer.features) {
                    assert_eq!(theirs.geometry, ours.geometry, "geometry");
                    let expected: HashMap<String, TagValue> = theirs
                        .properties
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|(k, v)| match v {
                            mvt_reader::feature::Value::String(s) => {
                                Some((k, TagValue::String(s)))
                            }
                            mvt_reader::feature::Value::Float(f) => {
                                Some((k, TagValue::Number(f as f64)))
                            }
                            mvt_reader::feature::Value::Double(d) => {
                                Some((k, TagValue::Number(d)))
                            }
                            mvt_reader::feature::Value::Int(i) => {
                                Some((k, TagValue::Number(i as f64)))
                            }
                            mvt_reader::feature::Value::UInt(u) => {
                                Some((k, TagValue::Number(u as f64)))
                            }
                            mvt_reader::feature::Value::SInt(i) => {
                                Some((k, TagValue::Number(i as f64)))
                            }
                            mvt_reader::feature::Value::Bool(b) => Some((k, TagValue::Bool(b))),
                            mvt_reader::feature::Value::Null => None,
                        })
                        .collect();
                    assert_eq!(expected, ours.tags, "tags");
                }
            }
        }
    }

    /// One `LineString` feature of `parts` two-point parts, which is the
    /// shape that made a browser tab die: `mvt-reader` starts a fresh
    /// `Vec::with_capacity(whole feature's varint count)` at every `MoveTo`.
    fn build_many_part_tile(parts: usize) -> Vec<u8> {
        let mut geometry: Vec<u32> = Vec::new();
        for _ in 0..parts {
            geometry.push((1 << 3) | 1); // MoveTo, 1
            geometry.push(zigzag(4));
            geometry.push(zigzag(4));
            geometry.push((1 << 3) | 2); // LineTo, 1
            geometry.push(zigzag(3));
            geometry.push(zigzag(0));
        }
        let mut feature = Vec::new();
        feature.extend(field(3, 0));
        feature.push(2); // LINESTRING
        feature.extend(field(4, 2));
        let packed: Vec<u8> = geometry.iter().flat_map(|v| varint(*v as u64)).collect();
        feature.extend(varint(packed.len() as u64));
        feature.extend(packed);

        let mut layer = Vec::new();
        layer.extend(field(15, 0));
        layer.push(2); // version
        layer.extend(field(1, 2));
        layer.extend(varint(5));
        layer.extend(b"roads");
        layer.extend(field(2, 2));
        layer.extend(varint(feature.len() as u64));
        layer.extend(feature);

        let mut tile = Vec::new();
        tile.extend(field(3, 2));
        tile.extend(varint(layer.len() as u64));
        tile.extend(layer);
        tile
    }

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn field(number: u32, wire: u8) -> Vec<u8> {
        varint(((number as u64) << 3) | wire as u64)
    }

    fn zigzag(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    #[test]
    fn a_many_part_feature_costs_its_coordinates_and_not_parts_times_varints() {
        // The regression test for the freeze. `mvt-reader`'s peak on this
        // fixture is `parts x varints x 8` and grows with the SQUARE of the
        // feature; this decoder's is the coordinates it holds. The two are
        // compared at the same part count so the ratio is the finding.
        for parts in [16usize, 64, 256] {
            let bytes = build_many_part_tile(parts);
            let reader = mvt_reader::Reader::new(bytes.clone()).expect("fixture parses");
            let theirs: Vec<(usize, usize)> = reader
                .get_features_as::<i32>(0)
                .expect("layer 0 decodes")
                .iter()
                .flat_map(|f| coord_vec_shapes(&f.geometry))
                .collect();
            let ours: Vec<(usize, usize)> = decode(bytes)
                .expect("decode should succeed")
                .layers[0]
                .features
                .iter()
                .flat_map(|f| coord_vec_shapes(&f.geometry))
                .collect();

            let len: usize = ours.iter().map(|(l, _)| l).sum();
            let our_slots: usize = ours.iter().map(|(_, c)| c).sum();
            let their_slots: usize = theirs.iter().map(|(_, c)| c).sum();
            assert_eq!(
                len,
                theirs.iter().map(|(l, _)| l).sum::<usize>(),
                "the two decoders must hold the same number of coordinates"
            );
            assert_eq!(our_slots, len, "this decoder must allocate exactly what it holds");
            assert!(
                their_slots >= their_slots_floor(parts),
                "expected mvt-reader to over-allocate at {parts} parts; it asked for \
                 {their_slots} slots to hold {len} coordinates"
            );
        }
    }

    /// `parts x varints`, the shape of `mvt-reader`'s peak: each of `parts`
    /// rings is sized for the whole feature's 6 varints per part.
    fn their_slots_floor(parts: usize) -> usize {
        parts * parts * 6
    }

    #[test]
    fn shrink_geometry_is_idempotent_and_total() {
        // Every variant reached, including the inline ones that own no
        // heap: a match arm that panicked or lost data would be a wrong
        // map, and the enum is wide enough that "it compiles" is not
        // evidence it is handled.
        use geo_types::{
            Coord, Geometry, GeometryCollection, Line, LineString, MultiLineString, MultiPoint,
            MultiPolygon, Point, Polygon, Rect, Triangle,
        };
        let ring = || {
            let mut v: Vec<Coord<i32>> = Vec::with_capacity(64);
            v.extend([(0, 0), (4, 0), (4, 4), (0, 4), (0, 0)].map(|(x, y)| Coord { x, y }));
            LineString::new(v)
        };
        let polygon = || Polygon::new(ring(), vec![ring()]);
        let mut cases: Vec<Geometry<i32>> = vec![
            Geometry::Point(Point::new(1, 2)),
            Geometry::Line(Line::new(Coord { x: 0, y: 0 }, Coord { x: 1, y: 1 })),
            Geometry::Triangle(Triangle::new(
                Coord { x: 0, y: 0 },
                Coord { x: 1, y: 0 },
                Coord { x: 0, y: 1 },
            )),
            Geometry::Rect(Rect::new(Coord { x: 0, y: 0 }, Coord { x: 2, y: 2 })),
            Geometry::LineString(ring()),
            Geometry::MultiPoint(MultiPoint::new(Vec::with_capacity(32))),
            Geometry::MultiLineString(MultiLineString::new(vec![ring(), ring()])),
            Geometry::Polygon(polygon()),
            Geometry::MultiPolygon(MultiPolygon::new(vec![polygon(), polygon()])),
            Geometry::GeometryCollection(GeometryCollection::new_from(vec![
                Geometry::Polygon(polygon()),
                Geometry::LineString(ring()),
            ])),
        ];
        for geometry in &mut cases {
            let before = geometry.clone();
            shrink_geometry(geometry);
            assert_eq!(&before, geometry, "the trim moved a coordinate");
            for (len, capacity) in coord_vec_shapes(geometry) {
                assert_eq!(capacity, len);
            }
            // Twice: the second call must be a no-op, not a second copy of
            // a different shape.
            let once = geometry.clone();
            shrink_geometry(geometry);
            assert_eq!(&once, geometry);
        }
    }
}

#[cfg(all(test, feature = "probe"))]
mod probe_tests {
    use super::*;

    fn feature(coords: &[(i32, i32)], tags: &[(&str, &str)]) -> Feature {
        Feature {
            geometry: geo_types::Geometry::LineString(geo_types::LineString::from(
                coords.iter().map(|&(x, y)| (x, y)).collect::<Vec<_>>(),
            )),
            tags: tags
                .iter()
                .map(|&(k, v)| (k.to_owned(), TagValue::String(v.to_owned())))
                .collect(),
        }
    }

    fn two_tiles() -> Vec<DecodedTile> {
        vec![
            DecodedTile {
                data_hash: 0,
                layers: vec![Layer {
                    name: "road".to_owned(),
                    features: vec![
                        feature(&[(0, 0), (1, 1)], &[("class", "motorway")]),
                        feature(&[(2, 2), (3, 3), (4, 4)], &[("class", "street")]),
                    ],
                }],
            },
            DecodedTile {
                data_hash: 0,
                layers: vec![Layer {
                    name: "water".to_owned(),
                    features: vec![feature(&[(5, 5)], &[("class", "motorway"), ("name", "Nile")])],
                }],
            },
        ]
    }

    #[test]
    fn census_counts_the_spine_and_the_coordinates() {
        let tiles = two_tiles();
        let c = census(tiles.iter());
        assert_eq!(c.tiles, 2);
        assert_eq!(c.layers, 2);
        assert_eq!(c.features, 3);
        assert_eq!(c.geometry_coords, 2 + 3 + 1);
        assert_eq!(c.geometry_vecs, 3, "one LineString per feature");
        assert!(c.geometry_coord_capacity >= c.geometry_coords);
        assert!(c.geometry_alloc_bytes >= c.geometry_coord_capacity * 8);
    }

    #[test]
    fn census_separates_repeated_key_strings_from_distinct_ones() {
        // "class" is allocated three times (once per feature) and "name"
        // once, so four key strings exist; two are distinct. That ratio is
        // the whole point of the census -- it is the interning prize.
        let tiles = two_tiles();
        let c = census(tiles.iter());
        assert_eq!(c.tag_entries, 4);
        assert_eq!(c.distinct_keys_overall, 2);
        // Per tile: tile one has {class}, tile two has {class, name}.
        assert_eq!(c.distinct_keys_per_tile, 1 + 2);
        assert_eq!(c.key_bytes, "class".len() * 3 + "name".len());
        assert_eq!(c.distinct_key_bytes_overall, "class".len() + "name".len());
    }

    #[test]
    fn census_separates_repeated_string_values_from_distinct_ones() {
        let tiles = two_tiles();
        let c = census(tiles.iter());
        assert_eq!(c.string_values, 4);
        // "motorway" appears in both tiles; "street" and "Nile" once each.
        assert_eq!(c.distinct_string_values_overall, 3);
        assert_eq!(
            c.string_value_bytes,
            "motorway".len() * 2 + "street".len() + "Nile".len()
        );
        assert_eq!(
            c.distinct_string_value_bytes_overall,
            "motorway".len() + "street".len() + "Nile".len()
        );
    }

    #[test]
    fn stripping_tags_leaves_the_geometry_and_the_spine() {
        let mut tile = two_tiles().remove(0);
        tile.strip(DecodedTilePart::Tags);
        let c = census(std::iter::once(&tile));
        assert_eq!(c.tag_entries, 0, "tags must be gone");
        assert_eq!(c.features, 2, "the feature spine must survive");
        assert_eq!(c.layers, 1, "the layer spine must survive");
        assert_eq!(c.geometry_coords, 5, "geometry must survive untouched");
    }

    #[test]
    fn stripping_tags_releases_the_hashmap_table_not_just_its_entries() {
        // `HashMap::clear` would leave `capacity` where it was, and the
        // table is part of what the measurement is weighing. Replacement
        // is what makes the freed bytes the real ones.
        let mut tile = two_tiles().remove(0);
        assert!(tile.layers[0].features[0].tags.capacity() > 0);
        tile.strip(DecodedTilePart::Tags);
        assert_eq!(tile.layers[0].features[0].tags.capacity(), 0);
    }

    #[test]
    fn stripping_geometry_leaves_the_tags_and_the_spine() {
        let mut tile = two_tiles().remove(0);
        tile.strip(DecodedTilePart::Geometry);
        let c = census(std::iter::once(&tile));
        assert_eq!(c.geometry_coords, 2, "each geometry is now a single point");
        assert_eq!(c.tag_entries, 2, "tags must survive");
        assert_eq!(c.features, 2, "the feature spine must survive");
    }

    #[test]
    fn coords_in_counts_every_ring_of_a_polygon() {
        let ring = |n: i32| {
            geo_types::LineString::from(vec![(0, 0), (n, 0), (n, n), (0, 0)])
        };
        let polygon = geo_types::Polygon::new(ring(4), vec![ring(1), ring(2)]);
        let cost = coords_in(&geo_types::Geometry::Polygon(polygon));
        assert_eq!(cost.coords, 12);
        assert_eq!(cost.vecs, 3, "exterior plus two interiors");
        assert!(
            cost.alloc_bytes >= 12 * std::mem::size_of::<geo_types::Coord<i32>>(),
            "the allocator gave at least the coordinates, plus the interiors Vec"
        );
    }

    #[test]
    fn coords_in_reports_capacity_apart_from_length() {
        // The whole reason the census carries both: an over-allocated
        // `Vec` holds its capacity, not its length, and a walk that added
        // up lengths would report a fraction of the real memory.
        let mut over_allocated: Vec<geo_types::Coord<i32>> = Vec::with_capacity(500);
        over_allocated.push(geo_types::Coord { x: 1, y: 2 });
        let line = geo_types::LineString::new(over_allocated);
        let cost = coords_in(&geo_types::Geometry::LineString(line));
        assert_eq!((cost.coords, cost.capacity, cost.vecs), (1, 500, 1));
        assert!(
            cost.alloc_bytes >= 500 * std::mem::size_of::<geo_types::Coord<i32>>(),
            "the allocator holds the CAPACITY, not the one coordinate in it: {} bytes",
            cost.alloc_bytes
        );
    }

    #[test]
    fn coords_in_allocates_nothing_for_the_inline_variants() {
        let point = geo_types::Geometry::Point(geo_types::Point::new(1, 2));
        assert_eq!(coords_in(&point), GeometryCost::inline(1));
        let rect = geo_types::Geometry::Rect(geo_types::Rect::new(
            geo_types::Coord { x: 0, y: 0 },
            geo_types::Coord { x: 1, y: 1 },
        ));
        assert_eq!(coords_in(&rect), GeometryCost::inline(2));
        assert_eq!(GeometryCost::inline(2).alloc_bytes, 0);
    }
}

#[cfg(all(test, feature = "probe"))]
mod allocator_tests {
    use super::*;

    #[test]
    fn allocated_bytes_reports_at_least_what_was_asked_for() {
        let mut v: Vec<u8> = Vec::with_capacity(4096);
        v.push(1);
        let reported = allocated_bytes(&v);
        assert!(reported >= 4096, "malloc_size reported {reported} for a 4096-byte request");
        // A plausibility ceiling: the allocator rounds up to a size class,
        // it does not multiply. Without this the test would pass on a
        // function that returned `usize::MAX`.
        assert!(reported < 4096 * 4, "malloc_size reported {reported}, implausibly large");
    }

    #[test]
    fn allocated_bytes_is_zero_for_an_empty_slice() {
        // The guard that keeps a dangling `as_ptr` away from the
        // allocator. Deleting it is undefined behaviour, not a wrong
        // number, so it is asserted rather than assumed.
        let v: Vec<u8> = Vec::new();
        assert_eq!(allocated_bytes(&v), 0);
    }

    #[test]
    fn allocated_bytes_grows_with_the_request() {
        let small: Vec<u8> = vec![0; 64];
        let large: Vec<u8> = vec![0; 65536];
        assert!(allocated_bytes(&large) > allocated_bytes(&small));
    }
}
