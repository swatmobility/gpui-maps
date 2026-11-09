use std::collections::HashMap;

/// `Clone` but not `Copy`: `geometry` owns two `Vec`s. Manifest entries are
/// read once at startup and cloned into the atlas, so nothing copies these
/// per frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpriteRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_ratio: f32,
    pub sdf: bool,
    /// Mapbox's `stretchX`/`stretchY`/`content`, when the entry declares
    /// them. Default (no bands, no content box) for an ordinary icon.
    pub geometry: IconGeometry,
}

/// `Clone` because `MapView::add_image` adds to an atlas it is already
/// sharing behind an `Arc`, via `Arc::make_mut`. The copy that costs is one
/// per `add_image` call on an atlas the renderer still holds -- a few MB
/// for a full sprite sheet, once, at a moment the caller chose. Adding
/// images every frame would be a mistake for other reasons.
#[derive(Default, Clone)]
pub struct SpriteAtlas {
    icons: HashMap<String, image::RgbaImage>,
    /// Each sliced icon's own `pixelRatio` from the manifest it came from,
    /// kept alongside the pixels because the sliced `RgbaImage` on its own
    /// cannot say whether its dimensions are logical points or physical
    /// device pixels. The `@2x` sheet this crate fetches declares
    /// `"pixelRatio": 2` with doubled `width`/`height`, so the draw loop
    /// has to divide it back out to recover the icon's LOGICAL size
    /// (see `map_view::icon_logical_size`).
    pixel_ratios: HashMap<String, f32>,
    /// Each sliced icon's own `sdf` flag from the manifest, kept for the
    /// same reason `pixel_ratios` is: the sliced `RgbaImage` cannot say
    /// whether its alpha channel is coverage or a signed distance field,
    /// and the two are drawn by completely different fragment code.
    ///
    /// An SDF sprite is a monochrome mask whose ALPHA encodes distance from
    /// the shape's edge (128 = on the edge, higher inside). It carries no
    /// colour of its own -- `icon-color` and `icon-halo-color` supply that
    /// at draw time, which is what lets one pin image serve every route
    /// colour. See `render::icon_shader.wgsl`.
    sdf: HashMap<String, bool>,
    /// Each icon's stretch bands and content box, for the icons that
    /// declare them. Absent for the overwhelming majority, which are drawn
    /// at a fixed size and have no need of either.
    geometry: HashMap<String, IconGeometry>,
}

/// How an icon may be RESIZED, for an icon meant to wrap a label.
///
/// A badge with rounded corners and a tail cannot simply be scaled to fit
/// its text -- that distorts the corners and the tail. Mapbox's answer, and
/// this one, is to name the bands that may stretch and the box the text
/// goes in, and to leave everything else at its authored size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IconGeometry {
    /// Horizontal bands, in IMAGE PIXELS as `(start, end)` half-open
    /// ranges, that absorb any extra width. Mapbox's `stretchX`.
    ///
    /// Deliberately placed in the flat parts of a shape: stretching an SDF
    /// band distorts its gradient along the stretch axis, which is
    /// invisible where the shape's edge runs parallel to that axis and very
    /// visible if it does not.
    pub stretch_x: Vec<(u32, u32)>,
    /// The same, vertically. Mapbox's `stretchY`.
    pub stretch_y: Vec<(u32, u32)>,
    /// `[left, top, right, bottom]` in image pixels: the box the label sits
    /// in, and therefore the box `icon-text-fit` grows to wrap it.
    /// Mapbox's `content`.
    pub content: Option<[u32; 4]>,
}

/// Counts, not pixels: a derived `Debug` would print every pixel of a
/// sheet (tens of MB for a full @2x sheet) wherever an atlas, a
/// `CaptureRequest` or a view holding one is printed.
impl std::fmt::Debug for SpriteAtlas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpriteAtlas")
            .field("icons", &self.icons.len())
            .field("sdf", &self.sdf.values().filter(|sdf| **sdf).count())
            .field("with_geometry", &self.geometry.len())
            .finish()
    }
}

impl SpriteAtlas {
    /// An atlas with no icons, for a caller who supplies their own rather
    /// than fetching a style's sheet. Add to it with [`SpriteAtlas::insert`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one icon under `name`, replacing any icon already there.
    ///
    /// This is how a caller gets an image the fetched sprite sheet does not
    /// contain -- a stop pin, a vehicle marker, a company logo -- into the
    /// same atlas everything else draws from. Mapbox's `map.addImage`, with
    /// the same two pieces of metadata attached to the pixels, and for the
    /// same reason: neither can be recovered from an `RgbaImage`.
    ///
    /// `pixel_ratio` is how many of `image`'s pixels make one LOGICAL point
    /// -- pass `2.0` for an image drawn at retina density, so a 40x40 image
    /// occupies 20x20 points. `sdf` says the alpha channel is a signed
    /// distance field rather than coverage, which makes the icon take its
    /// colour from `icon-color`/`icon-halo-color` instead of from its own
    /// pixels; [`sdf_from_mask`] builds one from a shape.
    ///
    /// For an SDF icon, `pixel_ratio` also bounds its halo: the field holds
    /// 6 image pixels outside the shape, so `icon-halo-width` can reach at
    /// most `6 / pixel_ratio` points (2 pt at 3x). See
    /// `overlay::FeaturePaint::icon_halo_width`.
    ///
    /// Replacing an icon also drops any stretch geometry the old one had
    /// ([`SpriteAtlas::set_geometry`]): the bands and content box name
    /// parts of particular pixels, and new pixels do not inherit them. Set
    /// the new icon's geometry after inserting it.
    ///
    /// The icon reaches the GPU on the next `MapView::set_sprites` or
    /// `MapView::add_image` -- inserting into an atlas the view is already
    /// holding does nothing on its own, because the packed atlas texture is
    /// built once at that point.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        image: image::RgbaImage,
        pixel_ratio: f32,
        sdf: bool,
    ) {
        let name = name.into();
        self.geometry.remove(&name);
        self.icons.insert(name.clone(), image);
        self.pixel_ratios.insert(name.clone(), pixel_ratio);
        self.sdf.insert(name, sdf);
    }

    pub fn get(&self, name: &str) -> Option<&image::RgbaImage> {
        self.icons.get(name)
    }

    /// The manifest-declared `pixelRatio` of `name`'s sprite, i.e. how many
    /// image pixels of `get(name)` make up one logical point.
    pub fn pixel_ratio(&self, name: &str) -> Option<f32> {
        self.pixel_ratios.get(name).copied()
    }

    /// Whether `name`'s sprite is a signed-distance field rather than a
    /// plain RGBA image. `None` for a name this atlas does not hold.
    pub fn is_sdf(&self, name: &str) -> Option<bool> {
        self.sdf.get(name).copied()
    }

    /// `name`'s stretch bands and content box, for an icon that declares
    /// them. `None` for one that does not, which is drawn at a fixed size.
    pub fn geometry(&self, name: &str) -> Option<&IconGeometry> {
        self.geometry.get(name)
    }

    /// Declares `name` stretchable, so `icon-text-fit` can grow it around a
    /// label.
    ///
    /// Separate from `insert` rather than another pair of arguments to it:
    /// almost no icon has this, and a caller registering a plain marker
    /// should not have to say so. Setting it for a name the atlas does not
    /// hold is harmless -- it simply has no icon to apply to.
    pub fn set_geometry(&mut self, name: impl Into<String>, geometry: IconGeometry) {
        self.geometry.insert(name.into(), geometry);
    }

    /// Every icon name present in this atlas, in arbitrary order. Used by
    /// `render::icon_pipeline::pack_icon_atlas`'s only real caller
    /// (`map_view.rs`, once per `set_sprites`/`ensure_gpu` call) to pack
    /// the WHOLE fetched sprite sheet into one shared atlas texture, since
    /// which icon names a data-driven `icon-image` expression will resolve
    /// to at runtime isn't statically knowable from the compiled style
    /// alone (e.g. `["get", "maki"]` depends on each feature's own tags).
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.icons.keys()
    }
}

/// The alpha a distance field carries exactly ON the shape's edge.
///
/// Must equal `render::icon_pipeline::SDF_EDGE`, which is what the fragment
/// shader thresholds the fill against. The two constants are the encode and
/// decode ends of one convention; `sdf_encoding_matches_the_shaders_decoding`
/// pins them together.
const SDF_EDGE_ALPHA: f32 = (256.0 - 64.0) / 256.0;

/// How many image pixels of distance one unit of encoded alpha spans -- the
/// shader's `SDF_PX`, from the other side.
const SDF_PX: f32 = 8.0;

/// Builds an SDF sprite from a shape.
///
/// `mask` is the shape as coverage: a pixel is INSIDE where its value is at
/// least 128. The result is an `RgbaImage` whose alpha encodes signed
/// distance from that shape's edge, ready for
/// [`SpriteAtlas::insert`] with `sdf: true`.
///
/// # Why this exists
///
/// An SDF sprite cannot be authored as an ordinary image: its alpha is not
/// opacity but distance, in a convention the fragment shader has to agree
/// with exactly. Without a generator, the SDF path is unreachable for
/// anyone not already holding a field produced by Mapbox's own tooling.
///
/// # The encoding
///
/// `alpha = SDF_EDGE_ALPHA + distance / SDF_PX`, with distance in image
/// pixels and positive INSIDE the shape. That is precisely what
/// `icon_shader.wgsl` inverts: it draws the fill where alpha exceeds
/// `SDF_EDGE_ALPHA`, and the halo where alpha exceeds
/// `(6 - halo_width_texels) / SDF_PX`, which is the alpha this function
/// writes `halo_width_texels` pixels OUTSIDE the shape.
///
/// The encoding therefore fixes how much room a halo has: alpha saturates
/// at 2 pixels inside the shape and bottoms out at 6 outside, so a halo can
/// reach 6 image pixels and no further. Leave that much transparent margin
/// around the shape in `mask`, or the border will be clipped by the
/// sprite's own edge rather than by the field.
///
/// RGB is white throughout. The shader ignores it for an SDF sprite -- the
/// colour comes from the style -- and white is the least surprising thing
/// for anything else that samples it.
///
/// # How the distances are found
///
/// An exact Euclidean distance transform -- Felzenszwalb & Huttenlocher's
/// separable lower-envelope pass, the one MapLibre's TinySDF runs for every
/// glyph -- twice: once to the nearest OUTSIDE pixel, once to the nearest
/// INSIDE one. That is O(pixels), where the windowed search it replaced
/// (kept as the test oracle `sdf_from_mask_brute_force`) was O(pixels x
/// 21²) and cost ~1 ms per 48 px glyph: the first z4 frame of the census
/// zoom sweep spent 10.6 ms of a 12 ms `symbol_collect` in it, on ONE
/// eleven-glyph label (2026-09-26, labels-off-main-thread).
///
/// The output is bit-identical to the windowed search. Both are the exact
/// minimum of `sqrt(dx² + dy²)` over opposite-class pixel centres, capped
/// at the same radius: squared distances between integer centres are
/// integers, exact in `f64`, and are rooted in `f32` the same way. The grid
/// is padded by the radius with OUTSIDE pixels, because the search treated
/// a pixel past the image edge as outside.
pub fn sdf_from_mask(mask: &image::GrayImage) -> image::RgbaImage {
    let (width, height) = mask.dimensions();
    let radius = SDF_PX as i32 + 2;
    let pad = radius as usize;
    let (w, h) = (width as usize, height as usize);
    let (pw, ph) = (w + 2 * pad, h + 2 * pad);
    let inside_at = |px: usize, py: usize| -> bool {
        px >= pad && py >= pad && px < pad + w && py < pad + h && mask.get_pixel((px - pad) as u32, (py - pad) as u32).0[0] >= 128
    };
    // Squared distance to the nearest pixel where `target(px, py)` holds.
    let to_nearest = |target: &dyn Fn(usize, usize) -> bool| -> Vec<f64> {
        let mut grid = vec![0.0f64; pw * ph];
        for py in 0..ph {
            for px in 0..pw {
                grid[py * pw + px] = if target(px, py) { 0.0 } else { EDT_INF };
            }
        }
        let n = pw.max(ph);
        let (mut f, mut d, mut v, mut z) = (vec![0.0; n], vec![0.0; n], vec![0usize; n], vec![0.0; n + 1]);
        for px in 0..pw {
            for py in 0..ph {
                f[py] = grid[py * pw + px];
            }
            edt_1d(&f[..ph], &mut d[..ph], &mut v, &mut z);
            for py in 0..ph {
                grid[py * pw + px] = d[py];
            }
        }
        for py in 0..ph {
            f[..pw].copy_from_slice(&grid[py * pw..(py + 1) * pw]);
            edt_1d(&f[..pw], &mut d[..pw], &mut v, &mut z);
            grid[py * pw..(py + 1) * pw].copy_from_slice(&d[..pw]);
        }
        grid
    };
    let to_outside = to_nearest(&|px, py| !inside_at(px, py));
    let to_inside = to_nearest(&|px, py| inside_at(px, py));

    let mut out = image::RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let here = mask.get_pixel(x, y).0[0] >= 128;
            let at = (y as usize + pad) * pw + x as usize + pad;
            let squared = if here { to_outside[at] } else { to_inside[at] };
            let found = if squared >= EDT_INF { f32::INFINITY } else { (squared as i64 as f32).sqrt() };
            let nearest = found.min(f32::from(radius as u16));
            let magnitude = nearest - 0.5;
            let distance = if here { magnitude } else { -magnitude };
            let alpha = (SDF_EDGE_ALPHA + distance / SDF_PX).clamp(0.0, 1.0);
            out.put_pixel(x, y, image::Rgba([255, 255, 255, (alpha * 255.0).round() as u8]));
        }
    }
    out
}

/// "No such pixel on this line" for [`edt_1d`]: large enough that no real
/// squared distance reaches it, small enough that arithmetic on it stays
/// finite.
const EDT_INF: f64 = 1e20;

/// One row or column of the Felzenszwalb & Huttenlocher distance
/// transform: `d[q] = min over p of (q - p)² + f[p]`, in O(n) via the lower
/// envelope of the parabolas rooted at each `p`. `v` and `z` are scratch of
/// length >= n and n + 1.
fn edt_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64]) {
    let n = f.len();
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        let qf = q as f64;
        let mut s;
        loop {
            let p = v[k] as f64;
            s = ((f[q] + qf * qf) - (f[v[k]] + p * p)) / (2.0 * qf - 2.0 * p);
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        if s <= z[k] {
            // k == 0 and the new parabola is below everywhere.
            v[0] = q;
            z[0] = f64::NEG_INFINITY;
            z[1] = f64::INFINITY;
            continue;
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, dq) in d[..n].iter_mut().enumerate() {
        let qf = q as f64;
        while z[k + 1] < qf {
            k += 1;
        }
        let p = v[k] as f64;
        *dq = (qf - p) * (qf - p) + f[v[k]];
    }
}

/// The windowed search [`sdf_from_mask`] replaced, kept as its oracle.
#[cfg(test)]
pub(crate) fn sdf_from_mask_brute_force(mask: &image::GrayImage) -> image::RgbaImage {
    let (width, height) = mask.dimensions();
    let inside = |x: u32, y: u32| mask.get_pixel(x, y).0[0] >= 128;

    // The field is only meaningful within the range the encoding can
    // express, so the search stops there instead of scanning the whole
    // image for every pixel. `+2` covers the saturating end and the
    // half-pixel edge offset below.
    let radius = SDF_PX as i32 + 2;

    let mut out = image::RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let here = inside(x, y);
            // Distance to the nearest pixel of the OPPOSITE class: that is
            // where the edge runs. Unfound within `radius` means the pixel
            // is deep in the interior or far outside, and the encoding
            // saturates either way.
            let mut nearest = f32::from(radius as u16);
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    // Outside the image counts as OUTSIDE the shape, so a
                    // shape running to the image's edge is bounded there
                    // rather than treated as continuing forever.
                    let neighbour = if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                        false
                    } else {
                        inside(nx as u32, ny as u32)
                    };
                    if neighbour == here {
                        continue;
                    }
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    if d < nearest {
                        nearest = d;
                    }
                }
            }
            // Half a pixel, so the edge lands BETWEEN the last inside pixel
            // and the first outside one rather than on top of one of them.
            // Without it every shape comes out half a pixel too fat.
            let magnitude = nearest - 0.5;
            let distance = if here { magnitude } else { -magnitude };
            let alpha = (SDF_EDGE_ALPHA + distance / SDF_PX).clamp(0.0, 1.0);
            out.put_pixel(x, y, image::Rgba([255, 255, 255, (alpha * 255.0).round() as u8]));
        }
    }
    out
}

pub fn slice_atlas(atlas_png_bytes: &[u8], manifest: &HashMap<String, SpriteRect>) -> anyhow::Result<SpriteAtlas> {
    let full = image::load_from_memory(atlas_png_bytes)?.to_rgba8();
    let atlas_width = full.width();
    let atlas_height = full.height();

    let mut icons = HashMap::new();
    let mut pixel_ratios = HashMap::new();
    let mut sdf = HashMap::new();
    let mut geometry = HashMap::new();
    for (name, rect) in manifest {
        // Validate that the manifest entry's bounds fit within the decoded atlas
        if rect.x + rect.width > atlas_width || rect.y + rect.height > atlas_height {
            anyhow::bail!("sprite rect for {name:?} exceeds atlas dimensions: rect=[{},{}..{}x{}] atlas={}x{}",
                rect.x, rect.y, rect.x + rect.width, rect.y + rect.height, atlas_width, atlas_height);
        }

        let mut icon = image::RgbaImage::new(rect.width, rect.height);
        for y in 0..rect.height {
            for x in 0..rect.width {
                let px = *full.get_pixel(rect.x + x, rect.y + y);
                icon.put_pixel(x, y, px);
            }
        }
        icons.insert(name.clone(), icon);
        pixel_ratios.insert(name.clone(), rect.pixel_ratio);
        sdf.insert(name.clone(), rect.sdf);
        if rect.geometry != IconGeometry::default() {
            geometry.insert(name.clone(), rect.geometry.clone());
        }
    }
    Ok(SpriteAtlas { icons, pixel_ratios, sdf, geometry })
}

pub fn parse_sprite_manifest(json_bytes: &[u8]) -> anyhow::Result<HashMap<String, SpriteRect>> {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        #[serde(rename = "pixelRatio", default = "default_pixel_ratio")]
        pixel_ratio: f32,
        #[serde(default)]
        sdf: bool,
        #[serde(rename = "stretchX", default)]
        stretch_x: Vec<(u32, u32)>,
        #[serde(rename = "stretchY", default)]
        stretch_y: Vec<(u32, u32)>,
        #[serde(default)]
        content: Option<[u32; 4]>,
    }
    fn default_pixel_ratio() -> f32 {
        1.0
    }

    let raw: HashMap<String, RawEntry> = serde_json::from_slice(json_bytes)?;
    Ok(raw
        .into_iter()
        .map(|(name, e)| {
            (
                name,
                SpriteRect {
                    x: e.x,
                    y: e.y,
                    width: e.width,
                    height: e.height,
                    pixel_ratio: e.pixel_ratio,
                    sdf: e.sdf,
                    geometry: IconGeometry {
                        stretch_x: e.stretch_x,
                        stretch_y: e.stretch_y,
                        content: e.content,
                    },
                },
            )
        })
        .collect())
}

/// The `(manifest, sheet)` request URLs to fetch for a style's sprite
/// `base_url`, always asking for the `@2x` sheet.
///
/// The 1x sheet is the wrong source on a retina display: Mapbox serves
/// `sprite.png` at 512x468 with every manifest entry at `"pixelRatio": 1`
/// (e.g. `stadium` is a 16x16 rect), and drawing that 16x16 texture onto
/// the 32-physical-pixel quad a 16-point icon occupies at a 2x scale
/// factor is a bilinear upscale -- the visible fuzz this fetch is fixing.
/// The `@2x` sheet is 1024x936 with `"pixelRatio": 2` and doubled
/// `width`/`height`, which `map_view::icon_logical_size` divides back out,
/// so this stays correct on a 1x display too (it just downsamples, which
/// is still sharper than upscaling). The window's own scale factor
/// deliberately does NOT select the sheet: sprites are fetched before any
/// window exists, and a window can move between displays anyway.
///
/// Mapbox's real sprite endpoints are `{base}@2x.json?access_token=...`
/// and `{base}@2x.png?access_token=...` -- both suffixes go *before* any
/// query string, not after it. A caller passing a `base_url` that already
/// includes `?access_token=...` (as `examples/mapbox_streets_demo.rs`
/// does, since Mapbox requires the token on every request) needs that
/// query string preserved but moved to the end of each request URL,
/// otherwise a naive `format!("{base_url}.json")` appends `.json`/`.png`
/// onto the token value itself -- corrupting it and getting a 401, not a
/// 404, from the real API (confirmed live against api.mapbox.com while
/// building `fetch_sprite_atlas`'s only caller).
/// `pub(crate)`, not private: the browser build fetches through
/// `web_sys::window().fetch_with_str` rather than through `reqwest`, so it
/// cannot call [`fetch_sprite_atlas`] -- but it must ask for the SAME two
/// URLs, or the `@2x` reasoning above silently stops holding on one of the
/// two frontends. One function knows what a sprite request looks like.
pub(crate) fn sprite_urls(base_url: &str) -> (String, String) {
    let (path, query) = match base_url.split_once('?') {
        Some((path, query)) => (path, format!("?{query}")),
        None => (base_url, String::new()),
    };
    (format!("{path}@2x.json{query}"), format!("{path}@2x.png{query}"))
}

/// Turns a style document's top-level `"sprite"` value (`Style::sprite`)
/// into the base URL `fetch_sprite_atlas` wants, resolving the one scheme
/// that is not already a URL.
///
/// Two forms appear in the styles vendored under `examples/`:
///
/// * `mapbox://sprites/mapbox/streets-v12` — Mapbox's own scheme, which
///   its Styles API resolves to
///   `https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite`. Every
///   request to that API needs a token, so `access_token` is required here
///   and appended as a query parameter (`sprite_urls` moves it after the
///   `@2x.json`/`@2x.png` suffix, which is where the real API wants it).
/// * `https://tiles.openfreemap.org/sprites/ofm_f384/ofm` — OpenFreeMap's
///   plain HTTPS base, already a URL and keyless. Used verbatim, and
///   `access_token` is ignored: appending a Mapbox token to a third party's
///   host would leak it to a server with no business seeing it.
///
/// `access_token` is therefore `Option`, not a required argument: a demo
/// that has no token at all (`examples/basic_map.rs`) can still resolve an
/// HTTPS-based style's sheet. `Err` for a `mapbox://` reference with no
/// token, and for any other scheme — a relative sprite path (legal in the
/// style spec, resolved against the style document's own URL) has no
/// meaning for a style read off the local disk, which is how both examples
/// load theirs.
pub fn resolve_sprite_url(sprite: &str, access_token: Option<&str>) -> anyhow::Result<String> {
    if let Some(path) = sprite.strip_prefix("mapbox://sprites/") {
        let token = access_token.ok_or_else(|| {
            anyhow::anyhow!("a mapbox:// sprite reference needs an access token")
        })?;
        return Ok(format!("https://api.mapbox.com/styles/v1/{path}/sprite?access_token={token}"));
    }
    if sprite.starts_with("https://") || sprite.starts_with("http://") {
        return Ok(sprite.to_string());
    }
    anyhow::bail!("unsupported style \"sprite\" reference {sprite:?}: expected mapbox://sprites/... or an http(s) URL")
}

/// The sprite base URL a caller should actually fetch, given a page-level
/// `?sprite=` override, an optional `?token=` override, the loaded style's
/// own `"sprite"` field, and a `fallback` base for when neither of those
/// resolves to something fetchable.
///
/// This is the whole resolution rule behind `web/src/app.rs`'s
/// `sprite_base` -- pulled out here, rather than written against
/// `web_sys::window()` query-string reads directly, so it is one function
/// both frontends can share and this file's ordinary `cargo test` can
/// exercise without a browser. In order:
///
/// 1. `query_sprite == Some("none")`, or an empty string -- `None`: icons
///    off entirely, the icons-off arm of a cost measurement.
/// 2. Any other `query_sprite` -- that value, verbatim and unconditionally.
///    An explicit override always wins, including over a style whose own
///    `"sprite"` would otherwise resolve.
/// 3. `style_sprite`, resolved through [`resolve_sprite_url`] with
///    `query_token`. This is the same function `examples/basic_map.rs`
///    calls natively, so a plain `http(s)://` reference (OpenFreeMap's
///    Liberty sheet) resolves and is used exactly as it is there. A
///    `mapbox://` reference resolves only when `query_token` is `Some`.
/// 4. `fallback`, for a `mapbox://` reference with no token, for any other
///    reference `resolve_sprite_url` refuses, or for a style with no
///    `"sprite"` field at all (`style_sprite` is `None`). This is what
///    keeps a public build from ever fetching Mapbox's sprite art or
///    needing a baked-in Mapbox token: see `web/src/app.rs`'s `SPRITE_URL`
///    doc for the licensing reasoning `fallback` exists to preserve.
pub fn resolve_sprite_base(
    query_sprite: Option<&str>,
    query_token: Option<&str>,
    style_sprite: Option<&str>,
    fallback: &str,
) -> Option<String> {
    match query_sprite {
        Some(value) if value == "none" || value.is_empty() => return None,
        Some(value) => return Some(value.to_string()),
        None => {}
    }
    match style_sprite {
        Some(sprite) => match resolve_sprite_url(sprite, query_token) {
            Ok(url) => Some(url),
            Err(_) => Some(fallback.to_string()),
        },
        None => Some(fallback.to_string()),
    }
}

/// `cfg`-gated on the target for wasm, and on a FEATURE for iOS -- two
/// different mechanisms for two different reasons, both about the one
/// thing this whole file is otherwise free of: `reqwest`.
///
/// wasm is `target_arch = "wasm32"` because that is genuinely a property of
/// the target: `reqwest` has no `wasm32-unknown-unknown` build, and a page
/// fetches through `web_sys::window().fetch_with_request` anyway (see
/// `web/src/app.rs`).
///
/// iOS is `feature = "sprite_http_fetch"`, NOT `target_os = "ios"`, and the
/// difference is load-bearing. `ios/Cargo.toml` carries no `reqwest`
/// dependency on purpose (see `ios/src/net.rs`'s module doc -- tile and
/// sprite bytes there go through `ureq` on a small blocking thread pool
/// instead), so these two functions must not compile there -- but
/// `ios/Cargo.toml` is also built with a plain `cargo build --manifest-path
/// ios/Cargo.toml`, no `--target`, as a fast host-side sanity check (see
/// that manifest's own doc comment), and `target_os` on THAT build is
/// `"macos"`, not `"ios"` -- a `cfg(target_os = "ios")` would silently pass
/// that check while failing the real `--target aarch64-apple-ios` one
/// checked by `scripts/check-ios.sh`. A feature reads the same regardless
/// of which target compiles it, which is what makes the two checks agree;
/// see the root `Cargo.toml`'s own comment on `sprite_http_fetch` (on by
/// default there, so native's behaviour is unchanged; declared and left
/// off in `ios/Cargo.toml`; declared and unused in `web/Cargo.toml`,
/// exactly as `probe` already is in all three).
///
/// Everything else in this file -- `slice_atlas`, `sdf_from_mask`, the
/// manifest parser -- is arithmetic over bytes already in hand and crosses
/// untouched, which is what lets `ios/src/lib.rs` include this whole file
/// (`glyph_atlas` and `icon_pipeline` name `crate::sprite` directly;
/// `symbol_pass::label_census` names it too, so `label_plan`'s own use of
/// `symbol_pass` reaches it transitively) without adding `reqwest` to that
/// crate's graph, on either build.
#[cfg(all(not(target_arch = "wasm32"), feature = "sprite_http_fetch"))]
pub async fn fetch_sprite_atlas(base_url: &str, http: &reqwest::Client) -> anyhow::Result<SpriteAtlas> {
    let (json_url, png_url) = sprite_urls(base_url);
    let json_bytes = fetch_bytes(http, &json_url).await?;
    let png_bytes = fetch_bytes(http, &png_url).await?;
    let manifest = parse_sprite_manifest(&json_bytes)?;
    slice_atlas(&png_bytes, &manifest)
}

/// GETs `url`, with any `access_token` in a failure's message redacted.
///
/// `reqwest`'s own errors embed the full request URL in their `Display`, and
/// the caller (`examples/mapbox_streets_demo.rs`) logs that error to stderr
/// on a failed sprite fetch — which would otherwise copy a live `pk.` token
/// into whatever log file is capturing the process's output.
#[cfg(all(not(target_arch = "wasm32"), feature = "sprite_http_fetch"))]
async fn fetch_bytes(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    async {
        let response = http.get(url).send().await?.error_for_status()?;
        Ok::<Vec<u8>, reqwest::Error>(response.bytes().await?.to_vec())
    }
    .await
    .map_err(|err| anyhow::anyhow!("{}", redact_access_token(&format!("{:#}", anyhow::Error::new(err)))))
}

/// Replaces the value of every `access_token=` occurrence in `text` with
/// `[REDACTED]`, leaving the rest of the message (including the URL itself,
/// which is the useful part when diagnosing a 404 vs. a 401) intact.
///
/// `pub(crate)`, not private: `map_view.rs`'s tile-fetch error notice needs
/// this exact redaction too (`HttpTileSource::fetch`'s errors are `reqwest`
/// errors as well, with the same URL-in-`Display` behavior) -- reused here
/// rather than re-implemented, so there is exactly one place that knows
/// what an access-token query parameter looks like.
pub(crate) fn redact_access_token(text: &str) -> String {
    const KEY: &str = "access_token=";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        let value_start = at + KEY.len();
        out.push_str(&rest[..value_start]);
        out.push_str("[REDACTED]");
        let after_key = &rest[value_start..];
        // A token value ends at the next URL/message delimiter.
        let value_len = after_key
            .find(|c: char| c == '&' || c == ')' || c == '"' || c == '\'' || c == ',' || c.is_whitespace())
            .unwrap_or(after_key.len());
        rest = &after_key[value_len..];
    }
    out.push_str(rest);
    out
}

/// [`redact_access_token`], and also every other credential a URL can
/// carry: a `password=`, `key=` (MapTiler, Google, `api_key=`),
/// `token=`, `signature=`, `credential=` or `sig=` argument's value and a `user:pass@` (or `token@`) authority,
/// all replaced with `[REDACTED]`. For text that may hold a ClickHouse,
/// tile or archive URL -- `reqwest::Error` prints the request URL,
/// password included.
pub fn redact_url_credentials(text: &str) -> String {
    let text = redact_access_token(text);
    let text = redact_param_value(&text, "password=");
    let text = redact_param_value(&text, "key=");
    let text = redact_param_value(&text, "token=");
    // Presigned URLs (S3 `X-Amz-Signature`/`X-Amz-Credential`, CloudFront
    // `Signature`, Google `X-Goog-Signature`/`X-Goog-Credential`, Azure SAS
    // `sig`); `X-Amz-Security-Token` is a `token=` above.
    let text = redact_param_value(&text, "signature=");
    let text = redact_param_value(&text, "credential=");
    let text = redact_param_value(&text, "sig=");
    // `scheme://userinfo@host`: everything between `://` and the last `@`
    // before the authority ends.
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find("://") {
        let start = at + 3;
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after
            .find(|c: char| matches!(c, '/' | '?' | '#' | ')' | '"' | '\'' | ',') || c.is_whitespace())
            .unwrap_or(after.len());
        match after[..end].rfind('@') {
            Some(userinfo_end) => {
                out.push_str("[REDACTED]");
                rest = &after[userinfo_end..];
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// Replaces the value after every case-insensitive `key` (which ends in
/// `=`) with `[REDACTED]`; the value ends where an access token's does.
fn redact_param_value(text: &str, key: &str) -> String {
    // ASCII lowercasing keeps every byte offset.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(at) = lower[from..].find(key) {
        let value_start = from + at + key.len();
        out.push_str(&text[from..value_start]);
        out.push_str("[REDACTED]");
        let after = &text[value_start..];
        let value_len = after
            .find(|c: char| c == '&' || c == ')' || c == '"' || c == '\'' || c == ',' || c.is_whitespace())
            .unwrap_or(after.len());
        from = value_start + value_len;
    }
    out.push_str(&text[from..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Final review I1: a URL's `user:SECRET@` and `password=SECRET`
    /// never survive, and the host stays readable.
    #[test]
    fn url_credentials_are_redacted_and_the_host_kept() {
        let text = "error sending request for url (http://user:SECRET@127.0.0.1:9/?password=SECRET&user=u&access_token=SECRET): refused";
        let shown = redact_url_credentials(text);
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("127.0.0.1:9") && shown.contains(": refused"), "{shown}");
        assert_eq!(redact_url_credentials("https://SECRET@h.example/x.pmtiles"), "https://[REDACTED]@h.example/x.pmtiles");
        assert_eq!(redact_url_credentials("/Users/a@b/x.pmtiles"), "/Users/a@b/x.pmtiles");
        assert_eq!(redact_url_credentials("http://h:8123/?PASSWORD=SECRET"), "http://h:8123/?PASSWORD=[REDACTED]");
        // F1: a MapTiler-style `key=` and a bare `token=` too.
        assert_eq!(redact_url_credentials("https://t.example/{z}.pbf?key=SECRET&x=1"), "https://t.example/{z}.pbf?key=[REDACTED]&x=1");
        assert_eq!(redact_url_credentials("https://t.example/a?api_key=SECRET"), "https://t.example/a?api_key=[REDACTED]");
        assert_eq!(redact_url_credentials("https://t.example/a?token=SECRET"), "https://t.example/a?token=[REDACTED]");
        // Presigned URLs (Task 12 review F10).
        let presigned = "https://b.s3.amazonaws.com/a.png?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA%2FSECRET\
                         &X-Amz-Security-Token=SECRET&X-Amz-Signature=SECRET&Signature=SECRET&sig=SECRET\
                         &X-Goog-Signature=SECRET&X-Goog-Credential=SECRET";
        let shown = redact_url_credentials(presigned);
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{shown}");
    }

    /// The distance transform is a speedup, not a new field: on masks of
    /// every shape the glyph atlas and the sprite path feed it -- blobs,
    /// thin strokes, noise, empty, full, touching the border, 1 px wide --
    /// every byte equals the windowed search's.
    #[test]
    fn the_distance_transform_is_bit_identical_to_the_windowed_search() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut masks: Vec<image::GrayImage> = vec![
            image::GrayImage::new(1, 1),
            image::GrayImage::from_pixel(1, 1, image::Luma([255])),
            image::GrayImage::from_pixel(9, 3, image::Luma([255])),
            image::GrayImage::new(40, 30),
            image::GrayImage::from_pixel(40, 30, image::Luma([200])),
            disc_mask(32, 8.0),
            disc_mask(70, 30.0),
        ];
        for _ in 0..60 {
            let w = 1 + (next() % 72) as u32;
            let h = 1 + (next() % 72) as u32;
            let density = next() % 5;
            let mut m = image::GrayImage::new(w, h);
            // Random strokes and noise, so inside and outside both have
            // near and far pixels.
            for _ in 0..(1 + next() % 6) {
                let (x0, y0) = ((next() % u64::from(w)) as i64, (next() % u64::from(h)) as i64);
                let (dx, dy) = ((next() % 5) as i64 - 2, (next() % 5) as i64 - 2);
                let thick = (next() % 4) as i64;
                for t in 0..(next() % 80) as i64 {
                    for ox in -thick..=thick {
                        for oy in -thick..=thick {
                            let (x, y) = (x0 + dx * t / 2 + ox, y0 + dy * t / 2 + oy);
                            if x >= 0 && y >= 0 && x < i64::from(w) && y < i64::from(h) {
                                m.put_pixel(x as u32, y as u32, image::Luma([255]));
                            }
                        }
                    }
                }
            }
            for p in m.pixels_mut() {
                if next() % 16 < density {
                    p.0[0] = (next() % 256) as u8;
                }
            }
            masks.push(m);
        }
        for (i, mask) in masks.iter().enumerate() {
            let fast = sdf_from_mask(mask);
            let slow = sdf_from_mask_brute_force(mask);
            assert_eq!(fast.as_raw(), slow.as_raw(), "mask {i} ({}x{}) differs", mask.width(), mask.height());
        }
    }

    #[test]
    fn parses_a_real_shaped_sprite_manifest() {
        let json = br#"{
            "marker": {"width": 20, "height": 20, "x": 0, "y": 0, "pixelRatio": 1, "sdf": false},
            "poi-icon": {"width": 15, "height": 15, "x": 20, "y": 0, "pixelRatio": 2, "sdf": true}
        }"#;
        let manifest = parse_sprite_manifest(json).unwrap();
        assert_eq!(manifest.len(), 2);
        let marker = manifest.get("marker").unwrap();
        assert_eq!(*marker, SpriteRect { x: 0, y: 0, width: 20, height: 20, pixel_ratio: 1.0, sdf: false, ..Default::default() });
        let poi = manifest.get("poi-icon").unwrap();
        assert_eq!(*poi, SpriteRect { x: 20, y: 0, width: 15, height: 15, pixel_ratio: 2.0, sdf: true, ..Default::default() });
    }

    #[test]
    fn requests_the_2x_sprite_sheet_so_retina_icons_are_not_upscaled() {
        // Mapbox serves `sprite.png` at 512x468 with every manifest entry
        // at `"pixelRatio": 1`, and `sprite@2x.png` at 1024x936 with
        // `"pixelRatio": 2`. Fetching the 1x sheet meant a 16x16 texture
        // was bilinearly stretched over a 32-physical-pixel quad on a 2x
        // display -- visibly fuzzy POI icons. Always ask for `@2x`; a 1x
        // display just downsamples it, which is correct (and sharp).
        let (json_url, png_url) = sprite_urls("https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite");
        assert_eq!(json_url, "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite@2x.json");
        assert_eq!(png_url, "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite@2x.png");
    }

    #[test]
    fn keeps_an_existing_query_string_last_with_the_2x_suffix_before_the_extension() {
        // The real caller passes a `base_url` that already carries
        // `?access_token=...` (Mapbox requires the token on every
        // request). Both the `@2x` and the extension have to land on the
        // PATH, with the query string moved back to the end -- otherwise
        // the suffixes get appended onto the token value itself and the
        // API answers 401 rather than 404. Placeholder token only: never
        // a real `pk.` value in a test (see the redaction test below).
        let (json_url, png_url) = sprite_urls("https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite?access_token=pk.test");
        assert_eq!(
            json_url,
            "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite@2x.json?access_token=pk.test"
        );
        assert_eq!(
            png_url,
            "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite@2x.png?access_token=pk.test"
        );
    }

    #[test]
    fn resolves_a_mapbox_sprite_reference_through_the_styles_api_with_the_token() {
        // `mapbox://sprites/<user>/<style>` is not a URL: the Styles API
        // resolves it to `/styles/v1/<user>/<style>/sprite`, and every
        // request to that API carries the token. Placeholder token only.
        let url = resolve_sprite_url("mapbox://sprites/mapbox/streets-v12", Some("pk.test")).unwrap();
        assert_eq!(url, "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite?access_token=pk.test");
        // ...and `sprite_urls` is what finally puts `@2x` before the
        // extension and the query string last, so the two compose.
        let (json_url, _) = sprite_urls(&url);
        assert_eq!(
            json_url,
            "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite@2x.json?access_token=pk.test"
        );
    }

    #[test]
    fn uses_a_plain_https_sprite_base_verbatim_and_never_appends_a_token_to_it() {
        // What the vendored OpenFreeMap Liberty style declares. It is
        // already a URL and the host is keyless -- and appending a Mapbox
        // token here would hand it to a server that has no business
        // seeing it, so `access_token` is ignored even when one is at hand.
        let liberty = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";
        assert_eq!(resolve_sprite_url(liberty, None).unwrap(), liberty);
        assert_eq!(resolve_sprite_url(liberty, Some("pk.test")).unwrap(), liberty);
        // The two URLs actually fetched (both confirmed live, 200):
        let (json_url, png_url) = sprite_urls(&resolve_sprite_url(liberty, None).unwrap());
        assert_eq!(json_url, "https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.json");
        assert_eq!(png_url, "https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.png");
    }

    #[test]
    fn rejects_a_mapbox_sprite_reference_with_no_token_and_an_unresolvable_scheme() {
        // A `mapbox://` reference cannot be fetched without a token, and a
        // relative sprite path (legal in the style spec, resolved against
        // the style document's own URL) has no meaning for a style read
        // off local disk. Both are errors rather than a silently broken
        // request URL.
        assert!(resolve_sprite_url("mapbox://sprites/mapbox/streets-v12", None).is_err());
        assert!(resolve_sprite_url("sprites/ofm", Some("pk.test")).is_err());
    }

    // -- `resolve_sprite_base` -----------------------------------------
    //
    // The rule behind `web/src/app.rs::sprite_base`. Fixed at
    // "fallback-sheet" throughout, mirroring `SPRITE_URL` on that page.

    const FALLBACK: &str = "./japan-sprite";

    #[test]
    fn a_plain_https_style_sprite_is_used_over_the_fallback() {
        // Liberty's case: the style names a keyless URL, so the page should
        // draw OpenFreeMap's real icons instead of the local sheet -- this
        // is the whole point of the fix.
        let liberty = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";
        assert_eq!(resolve_sprite_base(None, None, Some(liberty), FALLBACK), Some(liberty.to_string()));
        // A token in play must not change this or leak onto a third party's
        // host -- see `uses_a_plain_https_sprite_base_verbatim_...` above.
        assert_eq!(
            resolve_sprite_base(None, Some("pk.test"), Some(liberty), FALLBACK),
            Some(liberty.to_string())
        );
    }

    #[test]
    fn a_mapbox_style_sprite_falls_back_to_the_local_sheet_without_a_token() {
        // The licensing case this fix must not touch: no committed file
        // supplies a Mapbox token, so the mapbox-streets style still draws
        // this repository's own sheet, never Mapbox's art.
        assert_eq!(
            resolve_sprite_base(None, None, Some("mapbox://sprites/mapbox/streets-v12"), FALLBACK),
            Some(FALLBACK.to_string())
        );
    }

    #[test]
    fn a_mapbox_style_sprite_resolves_through_an_opt_in_runtime_token() {
        // `?token=` is the escape hatch requirement #4 asks for: supplied
        // only at run time, it lets a `mapbox://` reference resolve for a
        // local demo -- and the resolved URL is the real Mapbox Styles API
        // one, not the local fallback.
        assert_eq!(
            resolve_sprite_base(None, Some("pk.test"), Some("mapbox://sprites/mapbox/streets-v12"), FALLBACK),
            Some(
                "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite?access_token=pk.test"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_missing_sprite_field_falls_back_to_the_local_sheet() {
        // No `"sprite"` at all in the style document -- same fallback as a
        // `mapbox://` reference with no token, and for the same reason:
        // there is nothing to safely resolve.
        assert_eq!(resolve_sprite_base(None, None, None, FALLBACK), Some(FALLBACK.to_string()));
    }

    #[test]
    fn query_sprite_none_turns_icons_off_regardless_of_the_style() {
        // The A arm of the icon cost measurement: `?sprite=none` must win
        // even when the style itself declares a perfectly resolvable
        // sprite.
        let liberty = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";
        assert_eq!(resolve_sprite_base(Some("none"), None, Some(liberty), FALLBACK), None);
        // An empty `?sprite=` value is treated the same way.
        assert_eq!(resolve_sprite_base(Some(""), None, Some(liberty), FALLBACK), None);
    }

    #[test]
    fn query_sprite_overrides_everything_including_a_resolvable_style_sprite() {
        // `?sprite=<base>` must win even over a style whose own `"sprite"`
        // would otherwise resolve to something else entirely.
        let liberty = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";
        let overridden = "https://example.test/my-sprites/custom";
        assert_eq!(
            resolve_sprite_base(Some(overridden), None, Some(liberty), FALLBACK),
            Some(overridden.to_string())
        );
        // ...and over a style with no `"sprite"` field too.
        assert_eq!(
            resolve_sprite_base(Some(overridden), None, None, FALLBACK),
            Some(overridden.to_string())
        );
    }

    #[test]
    fn keeps_each_sliced_icons_own_pixel_ratio_alongside_its_pixels() {
        // A sliced `RgbaImage` cannot say on its own whether its
        // dimensions are logical points or physical device pixels, and the
        // `@2x` sheet's are the latter -- so the ratio has to survive
        // slicing for the draw loop to divide it back out.
        let mut img = image::RgbaImage::new(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                img.put_pixel(x, y, image::Rgba([1, 2, 3, 255]));
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let mut manifest = HashMap::new();
        manifest.insert("retina".to_string(), SpriteRect { x: 0, y: 0, width: 2, height: 2, pixel_ratio: 2.0, sdf: false, ..Default::default() });
        manifest.insert("plain".to_string(), SpriteRect { x: 2, y: 0, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() });

        let atlas = slice_atlas(&png_bytes, &manifest).unwrap();
        assert_eq!(atlas.pixel_ratio("retina"), Some(2.0));
        assert_eq!(atlas.pixel_ratio("plain"), Some(1.0));
        assert_eq!(atlas.pixel_ratio("not-in-this-sheet"), None);
    }

    #[test]
    fn redacts_access_tokens_out_of_error_messages() {
        let message = "error sending request for url (https://api.mapbox.com/styles/v1/mapbox/\
                       streets-v12/sprite.json?access_token=pk.eyJreallysecret)";
        let redacted = redact_access_token(message);
        assert!(!redacted.contains("pk.eyJreallysecret"), "token survived redaction: {redacted}");
        assert!(redacted.contains("access_token=[REDACTED])"), "unexpected redaction: {redacted}");
        assert!(redacted.contains("sprite.json"), "the URL path must survive: {redacted}");

        // Mid-query-string token, and more than one occurrence.
        let two = redact_access_token("a?access_token=pk.one&b=1 and ?access_token=pk.two");
        assert_eq!(two, "a?access_token=[REDACTED]&b=1 and ?access_token=[REDACTED]");

        // No token: unchanged.
        assert_eq!(redact_access_token("plain 404"), "plain 404");
    }

    #[test]
    fn defaults_missing_pixel_ratio_and_sdf() {
        let json = br#"{"plain": {"width": 10, "height": 10, "x": 0, "y": 0}}"#;
        let manifest = parse_sprite_manifest(json).unwrap();
        let plain = manifest.get("plain").unwrap();
        assert_eq!(plain.pixel_ratio, 1.0);
        assert!(!plain.sdf);
    }

    #[test]
    fn slices_named_icons_out_of_an_atlas_png() {
        // Build a real 4x2 RGBA image: left 2x2 block red, right 2x2 block blue.
        let mut img = image::RgbaImage::new(4, 2);
        for y in 0..2 {
            for x in 0..2 {
                img.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
            for x in 2..4 {
                img.put_pixel(x, y, image::Rgba([0, 0, 255, 255]));
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let mut manifest = HashMap::new();
        manifest.insert("red".to_string(), SpriteRect { x: 0, y: 0, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() });
        manifest.insert("blue".to_string(), SpriteRect { x: 2, y: 0, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() });

        let atlas = slice_atlas(&png_bytes, &manifest).unwrap();
        let red = atlas.get("red").unwrap();
        assert_eq!(red.dimensions(), (2, 2));
        assert_eq!(*red.get_pixel(0, 0), image::Rgba([255, 0, 0, 255]));
        let blue = atlas.get("blue").unwrap();
        assert_eq!(*blue.get_pixel(0, 0), image::Rgba([0, 0, 255, 255]));
    }

    #[test]
    fn rejects_manifest_entry_exceeding_atlas_bounds() {
        // Build a small 3x3 RGBA image.
        let mut img = image::RgbaImage::new(3, 3);
        for y in 0..3 {
            for x in 0..3 {
                img.put_pixel(x, y, image::Rgba([100, 100, 100, 255]));
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        // Manifest entry that exceeds the atlas: width goes past x+width > atlas_width
        let mut manifest = HashMap::new();
        manifest.insert("valid".to_string(), SpriteRect { x: 0, y: 0, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() });
        manifest.insert("out_of_bounds".to_string(), SpriteRect { x: 2, y: 2, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() });

        let result = slice_atlas(&png_bytes, &manifest);
        assert!(result.is_err(), "slice_atlas should return Err for out-of-bounds manifest entries");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("exceeds atlas dimensions"), "error should mention bounds validation");
    }

    fn disc_mask(size: u32, radius: f32) -> image::GrayImage {
        let centre = (size as f32 - 1.0) / 2.0;
        image::GrayImage::from_fn(size, size, |x, y| {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            image::Luma([if (dx * dx + dy * dy).sqrt() <= radius { 255 } else { 0 }])
        })
    }

    /// Decodes an encoded alpha back to the signed distance it stands for --
    /// the inverse of `sdf_from_mask`'s own formula, written independently
    /// so the tests below are not just restating the implementation.
    fn decoded_distance(alpha: u8) -> f32 {
        (alpha as f32 / 255.0 - SDF_EDGE_ALPHA) * SDF_PX
    }

    /// The whole feature rests on this: the alpha `sdf_from_mask` WRITES has
    /// to be the alpha `icon_shader.wgsl` READS, in the same units, or every
    /// icon comes out the wrong size with a border in the wrong place. The
    /// two constants live in different modules and nothing but this test
    /// connects them.
    #[test]
    fn sdf_encoding_matches_the_shaders_decoding() {
        assert_eq!(
            SDF_EDGE_ALPHA,
            crate::render::SDF_EDGE,
            "the encoder's edge alpha and the shader's fill threshold are one value"
        );
        assert_eq!(SDF_PX, crate::render::SDF_PX, "and so is the pixels-per-unit scale");
        // The shader's halo threshold at zero width must also land exactly
        // on the edge -- `(6 - 0) / 8` -- or a zero-width halo would draw a
        // ring anyway.
        assert!(
            (crate::render::SDF_HALO_BASE / crate::render::SDF_PX - SDF_EDGE_ALPHA).abs() < 1e-6,
            "a zero-width halo must threshold exactly at the shape's edge"
        );
    }

    /// A pixel deep inside the shape encodes as more than the edge alpha, a
    /// pixel outside as less. Backwards, and every icon would render as its
    /// own negative.
    #[test]
    fn inside_encodes_above_the_edge_and_outside_below_it() {
        let sdf = sdf_from_mask(&disc_mask(32, 8.0));
        let centre = sdf.get_pixel(16, 16).0[3];
        let corner = sdf.get_pixel(0, 0).0[3];
        assert!(
            f32::from(centre) / 255.0 > SDF_EDGE_ALPHA,
            "the disc's centre must read as inside, got {centre}"
        );
        assert!(
            f32::from(corner) / 255.0 < SDF_EDGE_ALPHA,
            "a far corner must read as outside, got {corner}"
        );
    }

    /// The encoded distance must actually track distance, not merely have
    /// the right sign: this is what makes `icon-halo-width` a real width
    /// rather than a knob that either shows a border or does not.
    ///
    /// Sampled along a row through a disc of known radius and compared with
    /// the analytic distance, within the encoding's own quantisation.
    #[test]
    fn the_encoded_distance_tracks_the_true_distance_to_the_edge() {
        let size = 48;
        let radius = 12.0;
        let sdf = sdf_from_mask(&disc_mask(size, radius));
        let centre = (size as f32 - 1.0) / 2.0;
        let row = 24;
        // Only where the encoding has room: it saturates 2px inside and 6px
        // outside, so sampling beyond that would be testing the clamp.
        for x in 12..36u32 {
            let true_distance = radius - ((x as f32 - centre).abs());
            if !(-5.0..=1.0).contains(&true_distance) {
                continue;
            }
            let got = decoded_distance(sdf.get_pixel(x, row).0[3]);
            assert!(
                (got - true_distance).abs() < 1.0,
                "at x={x} the edge is {true_distance:.2}px away but encoded {got:.2}px"
            );
        }
    }

    /// A shape that reaches the image's border is bounded there. Treating
    /// off-image as "inside" would make the field think the shape continues
    /// forever and encode the whole margin as interior.
    #[test]
    fn the_shape_is_bounded_by_the_images_edge() {
        // Every pixel inside: there is no edge except the image's own.
        let mask = image::GrayImage::from_pixel(16, 16, image::Luma([255]));
        let sdf = sdf_from_mask(&mask);
        let corner = decoded_distance(sdf.get_pixel(0, 0).0[3]);
        assert!(corner.abs() < 1.5, "the corner pixel is right at the boundary, got {corner:.2}");
    }

    /// An atlas a caller builds from nothing still carries the two pieces of
    /// metadata the draw path needs -- without them an SDF pin is drawn as
    /// its own blank pixels at twice the intended size.
    #[test]
    fn an_inserted_icon_keeps_its_pixel_ratio_and_sdf_flag() {
        let mut atlas = SpriteAtlas::new();
        atlas.insert("stop-pin", sdf_from_mask(&disc_mask(16, 6.0)), 2.0, true);
        atlas.insert("plain", image::RgbaImage::new(4, 4), 1.0, false);

        assert_eq!(atlas.pixel_ratio("stop-pin"), Some(2.0));
        assert_eq!(atlas.is_sdf("stop-pin"), Some(true));
        assert_eq!(atlas.is_sdf("plain"), Some(false));
        assert_eq!(atlas.get("stop-pin").expect("inserted").dimensions(), (16, 16));
        assert_eq!(atlas.is_sdf("absent"), None);
    }

    /// Stretch geometry names parts of particular pixels: an icon replaced
    /// under the same name must not inherit the old one's bands, while
    /// every other icon's geometry is untouched.
    #[test]
    fn replacing_an_icon_drops_its_old_geometry_and_no_other() {
        let mut atlas = SpriteAtlas::new();
        let bands = IconGeometry { stretch_x: vec![(4, 6)], stretch_y: vec![(4, 6)], content: Some([2, 2, 8, 8]) };
        atlas.insert("chip", image::RgbaImage::new(10, 10), 3.0, true);
        atlas.set_geometry("chip", bands.clone());
        atlas.insert("pin", image::RgbaImage::new(10, 10), 3.0, true);
        atlas.set_geometry("pin", bands.clone());

        atlas.insert("pin", image::RgbaImage::new(12, 12), 3.0, true);
        assert_eq!(atlas.geometry("pin"), None, "the new pixels carry no bands");
        assert_eq!(atlas.geometry("chip"), Some(&bands), "a neighbour keeps its own");
    }

    /// Inserting must ADD, not replace the sheet. This is the whole reason
    /// `insert`/`add_image` exist rather than a second `set_sprites` call:
    /// a caller adding a stop pin must not lose the basemap's icons.
    #[test]
    fn inserting_keeps_every_icon_already_in_the_atlas() {
        let mut manifest = HashMap::new();
        manifest.insert(
            "existing".to_string(),
            SpriteRect { x: 0, y: 0, width: 2, height: 2, pixel_ratio: 1.0, sdf: false, ..Default::default() },
        );
        let mut png = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encodes");
        let mut atlas = slice_atlas(&png, &manifest).expect("slices");

        atlas.insert("added", image::RgbaImage::new(2, 2), 1.0, true);

        assert!(atlas.get("existing").is_some(), "the fetched sheet must survive");
        assert!(atlas.get("added").is_some());
        assert_eq!(atlas.is_sdf("existing"), Some(false));
        assert_eq!(atlas.is_sdf("added"), Some(true));
    }

    /// Re-inserting the same name replaces that one icon and nothing else.
    #[test]
    fn inserting_the_same_name_twice_replaces_only_that_icon() {
        let mut atlas = SpriteAtlas::new();
        atlas.insert("pin", image::RgbaImage::new(2, 2), 1.0, false);
        atlas.insert("other", image::RgbaImage::new(2, 2), 1.0, false);
        atlas.insert("pin", image::RgbaImage::new(8, 8), 2.0, true);

        assert_eq!(atlas.get("pin").expect("replaced").dimensions(), (8, 8));
        assert_eq!(atlas.pixel_ratio("pin"), Some(2.0));
        assert_eq!(atlas.is_sdf("pin"), Some(true));
        assert_eq!(atlas.get("other").expect("untouched").dimensions(), (2, 2));
    }
}
