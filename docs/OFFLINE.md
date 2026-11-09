# Offline maps

gpui-maps renders with no network at all. Download one file for a region,
unplug, and the map still draws — tiles, icons, labels and all.

`examples/offline_map.rs` is the worked example. This document is the part
the example cannot be: where the file comes from, what it costs, and what
offline does *not* buy you.

---

## Run it

```bash
# 1. Get an archive (see "Getting an archive" below).
# 2. Point the example at it.
PMTILES_PATH=singapore.pmtiles cargo run --release --example offline_map
```

Verified with the network denied at the kernel, on macOS:

```bash
cargo build --release --example offline_map
sandbox-exec -p '(version 1)(allow default)(deny network*)' \
  env PMTILES_PATH=singapore.pmtiles ./target/release/examples/offline_map
```

A full map draws. The same sandbox run against `examples/basic_map`, which
fetches tiles over HTTPS, draws an empty page and logs a DNS failure per
tile — so the sandbox is real, and the offline example is not quietly
depending on it being fake.

## What is local, and why

| Piece | Where it comes from | Notes |
|---|---|---|
| Tiles | A `.pmtiles` archive on disk | `PmtilesSource::open` mmaps the file. `reqwest` appears in exactly two places in `src/` — `tile_source::http` and `sprite::fetch_sprite_atlas` — and the offline path calls neither. |
| Sprites | OpenFreeMap's sheet via `gpui_maps::cached_sprite_atlas` (fetched once into `~/.cache/gpui-map/sprites/`) | Not committed: its licence is unverified (`examples/assets/PROVENANCE.md`). With no cache and no network the map draws without icons. |
| Style | `examples/liberty-style.json`, vendored | Compiled by `style_spec::parse_and_compile`. Its `"glyphs"` and `"sprite"` URLs are strings the native renderer never dereferences. |
| Glyphs | The system font book | `glyphon::FontSystem::new()` enumerates installed fonts via fontdb. There is no network on the text path on **any** run, offline or not. |

The one thing to understand: **an archive is a closed set of tiles.** There
is no server behind it. Pan past its bounding box and you get blank, because
blank is the truth — nothing was downloaded there. `PmtilesSource` reports
the archive's own header `max_zoom` so that zooming in past the archive's
deepest level overzooms the tiles it *has* rather than asking for tiles that
cannot exist.

Expect `tile ... not found in archive` on stderr whenever the viewport
overlaps the edge of the box — even at the archive's own centre, if the
window is wider than the extract. That is the tile cover asking for a tile
outside the region, and it is the same shape of message an HTTP source
prints on a 404. Extract a slightly larger box than you intend to show if
you want the edge quiet.

---

## Getting an archive

### Schema first — this is the part that bites

A PMTiles archive is a container. What is *inside* it follows a **tile
schema**, and a style only draws layers that its tiles actually contain.
Two schemas dominate, and they share almost no layer names:

- **OpenMapTiles** — `transportation`, `water`, `building`, `place`, `poi`,
  `landcover`, … This is what `examples/liberty-style.json` is written
  against, and therefore what `offline_map` needs.
- **Protomaps basemap** — `earth`, `roads`, `water`, `places`, `pois`,
  `buildings`, … What Protomaps' own daily planet builds contain.

Point `offline_map` at a Protomaps-schema archive and it opens a window,
finds none of the layers the Liberty style names, and draws nothing. That is
not a bug in either file; they are simply not written for each other. Pair a
Protomaps archive with a [Protomaps basemap
style](https://github.com/protomaps/basemaps) instead.

### The tool

Everything below uses Protomaps' `pmtiles` CLI (Go, single binary):

```bash
brew install pmtiles
# or grab a release: https://github.com/protomaps/go-pmtiles/releases
```

### Route A — extract a region from Protomaps' daily planet

Protomaps publishes a full-planet build every day at
`https://build.protomaps.com/YYYYMMDD.pmtiles`. It is ~138 GB, and you do
not download it: `pmtiles extract` reads the archive's directory over HTTP
range requests and pulls only the tiles your bounding box needs.

```bash
# a country
pmtiles extract https://build.protomaps.com/20260901.pmtiles estonia.pmtiles \
  --bbox=21.6,57.5,28.3,59.8 --maxzoom=14

# a city
pmtiles extract https://build.protomaps.com/20260901.pmtiles singapore.pmtiles \
  --bbox=103.60,1.20,104.05,1.48 --maxzoom=14
```

`--region=file.geojson` takes a polygon instead of a box, which is a large
saving for a country that is not box-shaped.

Protomaps schema, so pair it with a Protomaps style, not with
`liberty-style.json`.

### Route B — build an OpenMapTiles archive

This is what `offline_map` was developed against, because it is what the
vendored Liberty style needs.

**Planetiler** is the maintained way. It builds an OpenMapTiles-schema
archive straight from an OSM extract, and writes PMTiles directly:

```bash
# needs Java 21+
java -Xmx4g -jar planetiler.jar --download --area=singapore \
  --output=singapore.pmtiles --maxzoom=14
```

`--area` accepts any Geofabrik extract name (`estonia`, `monaco`,
`singapore`, `bremen`, …), and downloads it for you.

**Or repackage tiles you already have.** Any XYZ tile tree or MBTiles file
in OpenMapTiles schema converts in one step:

```bash
pmtiles convert singapore.mbtiles singapore.pmtiles
```

This is how the Singapore archive in the measurements below was made: 434
tiles for z0–14 pulled from OpenFreeMap's public OpenMapTiles endpoint into
an MBTiles database, then converted. Fine for a test fixture; for anything
you ship, build with Planetiler rather than scraping someone's tile server.

### Check what you got

```bash
pmtiles show singapore.pmtiles
```

Confirm `max zoom`, `bounds`, and — the one people forget — `attribution`.

---

## What a region costs

Measured, not estimated. All at z0–14, on 2026-09-03.

| Region | Schema | Bounding box | Tiles | On disk | Time |
|---|---|---|---|---|---|
| Singapore | OpenMapTiles | 103.60,1.20 → 104.05,1.48 | 434 | **21.3 MiB** | 434 tile fetches + convert |
| Singapore | Protomaps | 103.60,1.20 → 104.05,1.48 | 434 | **11.2 MiB** | 12 s (38 range requests) |
| Estonia | Protomaps | 21.6,57.5 → 28.3,59.8 | 82,407 | **143.2 MiB** | 48 s (130 range requests) |

The Estonia extract needed **130 HTTP range requests** and transferred
158 MB for a 150 MB archive — an overfetch factor of 0.05. Pulling a country
out of a 138 GB planet file is genuinely cheap.

Extrapolating from Estonia: **a small-to-medium country at z0–14 lands in
the low hundreds of megabytes.** Denser or larger countries scale roughly
with populated area, not land area — most of a country's tiles are its
cities. Dropping `--maxzoom` to 12 cuts the archive by roughly an order of
magnitude; z14 is the level at which building footprints and street names
appear, so it is usually the level you want.

Note that the same box costs roughly half as much in Protomaps schema as in
OpenMapTiles — the schemas carry different amounts of detail, so budget
against the schema you are actually shipping.

For calibration at the other end: the whole planet, z0–15, is the 137.7 GB
daily build (1.43 billion addressed tiles).

---

## A disk cache for live tiles (opt-in)

Not offline maps, but the next thing to them: a live `HttpTileSource` can
keep what it fetched on disk, so a second launch draws from the cache.
**Off by default** -- the developer decides:

```rust
use gpui_maps::tile_source::disk_cache::DiskCache;
let source = HttpTileSource::new(url, 14, credit)
    .with_disk_cache(DiskCache::new(cache_dir, 512 << 20));
// optional: .with_max_stale(Duration::from_secs(3600)) on the DiskCache
```

What it does:

- Follows the server's `Cache-Control`: `max-age` (less `Age`), else
  `Expires - Date`, else 10% of the time since `Last-Modified` (at most a
  day). Lifetimes are capped at 2^31 s; dates outside 1970..=9999 are
  ignored. `no-store` and `private` are never written; `no-cache` is stored
  and revalidated on every use.
- Revalidates stale entries with `If-None-Match` / `If-Modified-Since`; a
  `304` re-dates the entry, a `404` drops it.
- **Stale-if-error:** when the server cannot be reached, answers 5xx or
  429, or a 200's body fails to arrive, a stale entry is served instead --
  for at most `max_stale` past its freshness (24 h by default,
  `DiskCache::with_max_stale`; `Duration::ZERO` turns it off), and never
  for an entry stored with `must-revalidate` or `proxy-revalidate`.
- Evicts least-recently-used to `max_bytes`; writes go to a `create_new`
  temp file and are renamed, and each entry records its body length, so a
  torn file is a miss. The directory scan and eviction unlinks run off the
  async workers (`spawn_blocking`).
- Native only: the browser build has the browser's HTTP cache.

**Keys and tokens.** Files are named by a hash of the URL with its
credentials redacted, and hold no URL; no token is ever written. Two
consequences, by design:

- Two tokens share one entry. Right for a public tileset; for a **private
  tileset** (per-account data behind the token) the only isolation between
  tokens is the server answering `Cache-Control: private`. **Do not enable
  the cache for a private tileset unless its server marks its tiles
  `private`.**
- The redaction blanks any parameter ending in `key=` or `token=`, so two
  URLs differing only in such a non-credential value (`monkey=`,
  `pagetoken=`) share an entry. Don't cache a template that varies one.

Give it a directory only your user can write (the gallery uses
`~/.cache/gpui-map/http-tiles` and disables the cache when there is no
`$HOME`, rather than fall back to a shared temp directory).

**Licensing.** OpenFreeMap, Natural Earth and Sentinel-2 cloudless send
week- to decade-long `max-age`s. Mapbox's terms restrict storing its tiles:
caching Mapbox (or any provider's) tiles is the developer's responsibility
under that provider's terms. The demo gallery's `GPUI_MAP_TILE_CACHE`
reflects this -- unset caches an allowlist of open licences (ODbL, CC BY
4.0, public domain, Singapore Open Data) and nothing else; `on` caches
everything, `off` nothing (`GPUI_MAP_TILE_CACHE_MB`, default 512, sets the
budget). The perf gate runs with it `off`.

Measured 2026-10-01 with `tile_latency_probe` (`PROBE_DISK_CACHE=<dir>`),
OpenFreeMap planet + Natural Earth covers at lon 78.28 lat 18.85, z3-z7:
cold, every cover resident 60-680 ms after the request; warm, 2-11 ms.

## What offline does **not** give you

This is offline **rendering**. Compare against Organic Maps, whose `.mwm`
files carry a routing graph and a search index alongside the geometry — a
PMTiles archive carries none of that:

- **No offline routing.** There is no road graph, no turn restrictions, no
  cost model. A tile's `transportation` layer holds line geometry for
  *drawing*, clipped per tile and simplified per zoom; it is not a network
  you can run a shortest path over.
- **No offline place search.** There is no geocoding index. You cannot type
  "Raffles Place" and get a coordinate. Labels exist as per-tile point
  features, which means you could search only what is currently decoded —
  not a search feature, an accident of the cache.
- **No offline reverse geocoding**, for the same reason.
- **No live data.** Traffic, transit arrivals, anything time-varying is out
  by definition.
- **A frozen snapshot.** The archive is as current as the day it was built.
  Refreshing means re-extracting the region.

If you need routing or search offline, those are separate data structures
and separate work. Nothing in this repository provides them, and nothing in
the PMTiles format implies them.

---

## Attribution is not optional

OpenStreetMap-derived tiles are ODbL. A map drawn from them must credit
OpenStreetMap on screen.

`PmtilesSource` reads `attribution` out of the archive's own TileJSON
metadata and hands it to `MapView`, so the credit travels *inside the file*
with the tiles it covers and cannot be left behind when an archive is
copied onto a device. `pmtiles extract` and Planetiler both preserve it.

If `pmtiles show` reports no attribution on an archive built from OSM data,
that is a licence problem in the archive. Fix it at the source:

```bash
pmtiles show --metadata archive.pmtiles > meta.json   # edit in the field
pmtiles edit archive.pmtiles --metadata=meta.json
```

`offline_map` prints the credit it found at startup, and warns on stderr
when an archive states none.
