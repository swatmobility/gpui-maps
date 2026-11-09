# ADR 0001: Map projections beyond Mercator and Globe

- **Status:** Proposed, deferred (2026-10-18). The design is agreed in principle; the work waits until it is scheduled.
- **Plan:** [docs/plans/2026-10-18-map-projections.md](../plans/2026-10-18-map-projections.md)
- **Spike:** `feat/projections` at c6d0875c (reference only, not for merge)

## Context

Users want world maps in projections other than Web Mercator:

- **Equal-area world maps** (Equal Earth, Mollweide) for thematic and choropleth maps. On Mercator, Greenland looks as large as Africa, so a coloured-region or hexbin map misleads.
- **Compromise world maps** (Robinson, Natural Earth) that simply look right for a world view.
- **Country maps** in a conic projection (Albers, Lambert conformal conic).

MapLibre GL JS 6.15 (October 2026) shipped experimental custom coordinate reference systems. Its demo shows the world in Robinson. That prompted this ADR.

### How MapLibre 6.15 does it

- A caller registers a projection with `addProjection`: a forward and an inverse function (usually from proj4js), plus the tile grid the sources are cut in. The style selects it by name.
- **It does not reproject tiles.** Every tiled source must already be cut in that CRS's own quad grid. A Mercator tile source lands in the wrong place. GPU reprojection is an open issue (maplibre#491).
- GeoJSON is projected in the workers, position by position. Edges stay straight lines in the plane. Latitudes that stretch too much are clamped. There is no antimeridian cutting, so data must be pre-split.
- Picking outside the projection's outline uses a forward/inverse round-trip check, then bisects towards the edge (#8803).
- Flat only: no globe, no world copies, no switching CRS after sources load.

### Our engine today

- Two views: Mercator ("Plain") and Globe, switched with the G key. The mode is a bool, `globe_enabled`, with about 113 uses in `src/`, plus `globe_active` call sites.
- **Globe already reprojects every Mercator tile on the GPU, per vertex.**
  - `src/render/subdivide.rs` densifies tile geometry on the CPU.
  - `project_to_sphere` in `shader.wgsl`, `raster_shader.wgsl` and `extrusion_shader.wgsl` maps each vertex to a point on the sphere.
  - `GlobeFrame.globe_view_proj` takes that point to clip space.
- Labels, the globe cover and picking go through `mercator_to_sphere` / `sphere_point_to_mercator` on the CPU.
- Mercator assumptions remain in `plain_constrain` (camera_control.rs), `flat_cover` (globe.rs), `pick_world_at` (map_view.rs), `project_line_thickness` (shader.wgsl, uses the globe's 1/cos(lat)), the story scale bar, the per-shader globe transition branches, and the web and iOS frame loops.

## Decision drivers

1. **Reuse what we have.** The basemap, overlays, H3/hexbin, arcs and live ClickHouse tiles must work in any projection without re-tiling per CRS.
2. **Runtime switch.** The user and the agent can change the projection on an open map. Every map is agentic.
3. **Desktop, web and iOS are equal.** One implementation in `src/`, no shell-specific maths.
4. **GPU performance.** A world view at 60 fps on a phone; no per-frame CPU reprojection of tile geometry.
5. **Licences.** Only permissive sources (MIT, BSD, ISC, Apache-2.0), credited in NOTICE. No Mapbox GL JS 3.x code.
6. **Correctness people can see.** Equal-area must really be equal-area; seams must not leave slivers or streaks.

## Considered options

### (a) Tiles pre-cut per CRS (the MapLibre 6.15 approach)

- **Pros:** exact for national-grid data; the renderer stays flat; no reprojection cost.
- **Cons:** every source must be re-tiled per projection, so our basemap, H3 and live tiles do not work; zoom levels stop matching our tiles; no runtime switch; needs a tile server per CRS.

### (b) CPU reprojection of tiles

- **Pros:** simple to reason about; any projection library works; exact f64 maths.
- **Cons:** reprojects every vertex of every tile on the CPU, and again on each lon_0 change; slow on phones; duplicates the globe's GPU path; more memory per tile.

### (c) Analytic GPU per-vertex reprojection of Mercator tiles, reusing the globe path

- **Pros:** works on all existing sources unchanged; runtime switch is a uniform change; the globe path already provides densification, cover and label anchors; the spike proves it renders (Equal Earth and Robinson, labels placed); one implementation on all shells.
- **Cons:** each projection needs WGSL formulas plus an f64 CPU twin, with parity tests; line width needs each projection's local scale; seams need tile splitting; only the projections we write ourselves.

### (d) A PROJ-string lookup texture

- **Pros:** any projection PROJ can describe (via the pure-Rust `proj4rs`), without hand-written WGSL.
- **Cons:** precision is limited by the texture resolution, fine at world and country zoom but not at street zoom; `proj4rs` lacks Robinson, Equal Earth and Natural Earth; one more texture per projection change.

### (e) Do nothing

- **Pros:** no cost.
- **Cons:** no honest world choropleths; we fall behind MapLibre on a visible feature.

## Decision outcome

- **(c) is the primary path.** A fixed set of analytic projections runs on the GPU and reprojects our Mercator tiles per vertex, through the existing globe pipeline with a plane point in place of a sphere point.
- **(d) comes later** for arbitrary PROJ strings, at world and country zoom.
- **(a) is only an optional "native CRS source" mode,** added if a customer brings national-grid WMTS data.
- (b) and (e) are rejected.

### What we take, change or reject

| Source | Idea | Verdict | Why |
|---|---|---|---|
| MapLibre | Round-trip check, then bisect to the edge, for picking outside the outline | Adopt | Simple and robust; ported as an idea, credited in NOTICE |
| MapLibre | Projection named in the style JSON | Adopt | Same as our document field; the agent sets it |
| MapLibre | Clamp latitudes that stretch too much | Adapt | Becomes clamping of our pole caps onto the pole line |
| MapLibre | Sources pre-cut per CRS as the main path | Reject | Breaks driver 1; kept only as option (a) later |
| MapLibre | User-supplied proj4js functions | Reject | CPU callbacks cannot run in our shaders; analytic WGSL instead |
| MapLibre | Straight edges between projected positions | Reject | Long edges must follow the curve; we densify adaptively |
| MapLibre | No antimeridian cutting | Reject | We cut tiles at seams and cut GeoJSON properly |
| d3-geo (ISC) | Adaptive resampling test | Adapt | Run on the CPU per (projection, tile zoom) to choose a subdivision level, not per feature |
| d3-geo (ISC) | Antimeridian clip and rejoin | Adopt | For GeoJSON and overlays, once per source and lon_0 |
| d3-geo (ISC) | Clip to the sphere's outline | Adapt | Done per tile half at the seam, never per vertex |
| PROJ (MIT) | Projection definitions and the Robinson table | Adopt | Reference values for formulas and for parity tests |
| PROJ / proj4rs | Arbitrary PROJ strings | Later | Via the lookup texture, option (d) |
| Organic Maps | Path text laid out along the screen polyline | Adapt | Line labels follow the projected line each frame |

### Design in brief

1. **A `Projection` value replaces `globe_enabled`:** `Mercator | Globe | Flat(FlatProjection { kind, lon_0, params })`. It is carried on MapView into GlobeFrame and the GPU upload, the same on all shells.
2. **Pipeline variants per projection family.** The WGSL is specialised at pipeline build, so the Mercator and Globe pipelines contain no flat-projection code. Projection code and parameters live in a real uniform field.
3. **Every projection has a CPU f64 twin** for anchors, picking and cover, with a GPU/CPU parity test.
4. **Tiles stay Mercator.** Density is chosen per tile from the adaptive resampling test. Tile level of detail follows the projection's local scale at the screen centre.
5. **Seams are cut per tile.** With lon_0 = 0 the seam is the ±180 tile edge and nothing is cut. Otherwise only the tile column holding the seam is split. lon_0 snaps to a step so cached meshes stay valid.
6. **Zooming in works.** Cover is a frustum test on projected tile bounds; the camera keeps the centre inside the outline; deep-zoom precision comes from an f64 tile origin and a local offset in the shader.
7. **Furniture.** A built-in graticule and outline layer; a scale bar that says "scale varies" at world zoom.
8. **3D extrusion is off** in flat projections.

## Consequences

**Positive**

- Equal-area world maps for choropleths, hexbins and H3, on all existing data.
- One projection setting the agent and the style menu both change at runtime.
- The Globe and Mercator paths gain a proper type in place of a bool.

**Negative**

- Each projection costs WGSL plus an f64 twin plus tests.
- More pipeline variants to compile and cache.
- `globe_enabled` has about 113 uses; replacing it touches many files.

**Risks**

- **Line width.** `project_line_thickness` uses the globe's 1/cos(lat). Each projection needs its own local scale (Jacobian), or lines thicken towards the poles. The spike shows this.
- **Seams.** A wrong split leaves slivers or streaks across the map.
- **Cover cost.** Without culling, a zoomed view draws the whole world's tiles. The spike draws all 256 tiles at z4.
- **Deep-zoom precision.** f32 in the shader jitters at street zoom unless the tile origin is projected in f64.
- **Goldens.** Globe and Mercator goldens must stay bit-identical; each projection needs its own goldens and a GPU/CPU parity test.

## Library vs Studio

- **Library (`src/`):** the `Projection` type and formulas, shaders, cover, picking, labels, graticule and outline, the style/document `projection` field, the agent command `map.set_projection` next to `map.set_globe`, and `proj4rs` as an optional feature.
- **Studio:** a Projection row in the existing style/basemap menu (next to Globe/Plain in `examples/studio/map_options.rs`; no on-map button), saving the projection in the document, the agent-chat tool, and a gallery page. G stays the globe key.

## Open questions

These are recommendations, awaiting the user's answer.

| # | Question | Recommendation |
|---|---|---|
| 1 | Which projections ship first? | Equal Earth, Robinson, Natural Earth, Mollweide, plus Albers for country maps |
| 2 | What does G toggle? | Globe ⇄ the chosen flat projection (Mercator or any other) |
| 3 | Zoom to street level, or world and country only? | Country level first; street level later (most of the P2 cost) |
| 4 | Pacific-centred maps (lon_0 ≠ 0) and interrupted Goode in v1? | Pacific-centred yes; Goode later |
| 5 | Arbitrary PROJ strings in v1? | No; a fixed analytic set with parameters first |
| 6 | A native-CRS tile mode for national-grid sources? | Later, only if a customer needs it |

## References

- MapLibre GL JS changelog, 6.15: experimental `addProjection` (issues [#168](https://github.com/maplibre/maplibre-gl-js/issues/168), [#5764](https://github.com/maplibre/maplibre-gl-js/issues/5764)); GeoJSON projected in workers ([#8736](https://github.com/maplibre/maplibre-gl-js/issues/8736), [#8738](https://github.com/maplibre/maplibre-gl-js/pull/8738)); world projections wrapping past their edge ([#8803](https://github.com/maplibre/maplibre-gl-js/pull/8803)); registered CRS vs tile grid docs ([#8807](https://github.com/maplibre/maplibre-gl-js/pull/8807)).
- GPU reprojection discussion: [maplibre/maplibre#491](https://github.com/maplibre/maplibre/issues/491).
- d3-geo (ISC): adaptive resampling, antimeridian clip and rejoin. https://github.com/d3/d3-geo
- Šavrič, Patterson & Jenny (2018), "The Equal Earth map projection", IJGIS 33(3).
- Robinson (1974); Snyder (1990); PROJ `robin` table (MIT). https://proj.org
- proj4rs 0.2.1 (MIT/Apache-2.0), pure Rust, builds for wasm; has moll, eqc, aea, lcc, laea, stere, tmerc; lacks robin, eqearth, natearth. https://crates.io/crates/proj4rs
