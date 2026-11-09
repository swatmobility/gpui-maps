//! The SWAT Mobility mark drawn in the map's BOTTOM-LEFT corner.
//!
//! On by default, on every map this crate draws. It is the product's own
//! mark on the product's own renderer -- the same arrangement Mapbox has,
//! where a map drawn by their engine says so -- and not a decoration a caller
//! is expected to deal with.
//!
//! "Every map this crate draws" is the desktop `MapView`, which is where the
//! draw call lives (`map_view::MapView::paint`). The iOS and browser shells
//! reach the engine by `#[path]` and neither includes this module, so neither
//! draws the mark today; adding it to either is a `#[path]` line plus a call
//! in that shell's own frame loop, and nothing in this module would have to
//! change to support it.
//!
//! # Two corners, two different things
//!
//! The credit in the bottom-RIGHT corner
//! (`symbol_pass::attribution_origin` and the `ATTRIBUTION_*` constants
//! beside it) is a LICENCE obligation that belongs to the TILE SOURCE: a
//! source states terms, this renderer draws them, and both the ink colour
//! and the plate colour are `pub(crate)` so a host app can neither recolour
//! the credit nor suppress it. It changes when the source changes.
//!
//! This mark belongs to the ENGINE, and so it does not change when the
//! source does: point the same map at OpenFreeMap, at a local `.pmtiles`
//! archive or at nothing at all and the bottom-right credit follows the
//! data while the bottom-left mark stays put. That is the distinction the
//! two corners encode, and it is why they are two separate mechanisms
//! rather than one list of chrome.
//!
//! [`crate::map_view::MapView::set_brand_mark`] exists for placement, not
//! for opting out -- see its own doc comment for what it is for.
//!
//! # Why this is a raster and not an SVG
//!
//! The artworks upstream are SVG (`mark-{dark,light}.svg` and
//! `wordmark-{dark,light}.svg`, the same files SWAT's own desktop app
//! draws on its own maps), and this crate rasterises none of them. The four
//! are composed into one lockup here and vendored as two PNGs, one per
//! background. `resvg` IS in the dependency graph --
//! but only underneath `gpui`, i.e. only on the platforms gpui builds for.
//! The browser build reaches these same sources by `#[path]` with its own
//! dependency set (`web/Cargo.toml`), where `image` is taken with
//! `default-features = false, features = ["png"]` and there is no SVG
//! rasteriser at all, and the iOS shell does the same. A vendored PNG
//! decodes identically in all three, costs no new dependency line in a crate
//! four applications compile, and -- because the bytes are fixed -- is the
//! only form a pixel test can pin. See `src/assets/PROVENANCE.md` for how
//! the two files were produced.

use crate::style;

/// The vendored artwork for a LIGHT background: near-black ink (`#11182B`).
///
/// 174x36 straight-alpha RGBA PNG, i.e. 2x of this mark's default drawn size
/// ([`BrandMark::DEFAULT_HEIGHT_PX`] is 18 logical px, and 18 x 4.8333 = 87).
/// The symbol and the "SWAT" wordmark composed into one lockup -- see
/// [`GRID_UNIT_PX`] for the geometry and `src/assets/PROVENANCE.md` for how
/// the two source SVGs were set into it.
///
/// Every texel carries the SAME rgb and varies only in alpha, which is what
/// makes "monochrome" a property of the file rather than a claim about it;
/// `the_artwork_is_monochrome_by_construction` is that check.
pub(crate) const SWAT_ON_LIGHT_PNG: &[u8] = include_bytes!("assets/swat-on-light@2x.png");

/// The vendored artwork for a DARK background: white ink, same geometry.
///
/// Byte-identical to [`SWAT_ON_LIGHT_PNG`] in its ALPHA channel -- the two are
/// one composition rendered twice with one fill changed, not two drawings, so
/// neither can drift from the other. Pinned by
/// `both_vendored_artworks_decode_to_the_same_geometry_with_an_alpha_channel`.
pub(crate) const SWAT_ON_DARK_PNG: &[u8] = include_bytes!("assets/swat-on-dark@2x.png");

/// The layout module every figure in the lockup is a multiple of, in LOGICAL
/// pixels.
///
/// **Three.** The only other object in this band is the licence credit's
/// plate, and that plate is built on threes: `ATTRIBUTION_PLATE_PADDING_X_PX` 6, its line box
/// `ATTRIBUTION_TEXT_SIZE_PX * LABEL_LINE_HEIGHT_RATIO` 12, and its height
/// 15 (its vertical padding is the half unit 1.5 -- see that constant). The
/// top corners' inset, `chrome::CORNER_INSET_PX`, is 12: four units; the
/// bottom row's, `chrome::BOTTOM_ROW_INSET_PX`, is 9: three. The one length off the grid
/// is the type SIZE (10), which is not a layout module.
///
/// So the grid was read out of the credit rather than imported: an 8-pt grid
/// would have been a convention nothing here uses, and 4 would have put the
/// credit's own plate off its own grid. At the 2x density the artwork is cut
/// for, 3 logical px is 6 texels -- every grid line lands on a whole texel.
///
/// `the_lockup_sits_on_its_own_grid` measures this off the vendored pixels.
pub(crate) const GRID_UNIT_PX: f32 = 3.0;

/// Which of the two vendored artworks to draw.
///
/// Named for the BACKGROUND each one is for, not for its own ink, and the
/// upstream files are named the OTHER WAY ROUND -- for the ink. The
/// SWAT desktop app's `mark-dark.svg` carries the dark `#11182B`
/// ink and is what it draws on a LIGHT basemap, so it is this crate's
/// [`BrandMarkVariant::OnLight`]; its `mark-light.svg` carries white ink
/// for a DARK basemap, and is this crate's [`BrandMarkVariant::OnDark`].
/// The two schemes are exact inversions of each other, which is the kind of
/// ambiguity that gets a white logo drawn on a white map -- the mapping is
/// verified by reading the PNG's own dominant ink in
/// `each_variant_carries_the_ink_its_name_promises`, not by trusting either
/// naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrandMarkVariant {
    /// Choose from the style's own `background` layer colour, every frame.
    /// The default; see [`BrandMarkVariant::resolve`] for the rule and the
    /// measured crossover.
    #[default]
    Auto,
    /// Force the dark-ink artwork.
    OnLight,
    /// Force the white-ink artwork.
    OnDark,
}

/// [`BrandMarkVariant`] with `Auto` already decided -- what the draw loop
/// actually indexes an uploaded texture with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedVariant {
    OnLight,
    OnDark,
}

impl BrandMarkVariant {
    /// Decides which artwork a background colour calls for.
    ///
    /// # The rule
    ///
    /// Not a luminance threshold picked by eye: whichever of the two inks has
    /// the HIGHER WCAG 2.x contrast ratio against `background` wins. That is
    /// the same relative-luminance maths
    /// `the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
    /// already grades the credit with (`map_view.rs`, `luminance`), applied to
    /// the one question this has to answer, so there is no second, private
    /// notion of "dark enough" in the crate to drift out of step with the
    /// first.
    ///
    /// # Where it crosses over
    ///
    /// The dark ink is `#11182B` (relative luminance 0.0095) and the light
    /// ink is pure white (1.0), so the two ratios are equal at a background
    /// relative luminance of 0.1999 -- sRGB ~#7C7C7C. Anything darker than
    /// that gets the white artwork. Both figures are computed and pinned by
    /// `the_variant_crossover_sits_where_the_two_inks_contrast_equally`;
    /// neither is a constant in this file, so neither can be stated here and
    /// be wrong in the code.
    ///
    /// # No background layer
    ///
    /// A style with no `background` layer renders onto a transparent surface
    /// and tells this function nothing, so it gets `OnLight`. That is the
    /// safer of the two guesses rather than a coin toss: a transparent map
    /// surface is composited over whatever the HOST app draws behind it, and
    /// a host that has gone to the trouble of a transparent map is far more
    /// often putting it on that app's own light chrome than on black.
    pub(crate) fn resolve(self, background: Option<style::Rgba>) -> ResolvedVariant {
        match self {
            BrandMarkVariant::OnLight => ResolvedVariant::OnLight,
            BrandMarkVariant::OnDark => ResolvedVariant::OnDark,
            BrandMarkVariant::Auto => {
                let Some(background) = background else {
                    return ResolvedVariant::OnLight;
                };
                let bg = relative_luminance(background);
                if contrast_ratio(bg, relative_luminance(ON_LIGHT_INK))
                    >= contrast_ratio(bg, relative_luminance(ON_DARK_INK))
                {
                    ResolvedVariant::OnLight
                } else {
                    ResolvedVariant::OnDark
                }
            }
        }
    }
}

/// What is actually BEHIND the mark, which is what
/// [`BrandMarkVariant::Auto`] has to decide against.
///
/// **This is not the style's `background` colour once the globe is on.**
/// Past `render::globe::GLOBE_ZOOM_MIN` the planet is a disc on a field of
/// `Space::color`, and `MapView::paint` clears to
/// [`render::space::space_clear_color`] rather than to the style — so on a
/// LIGHT style at full globeness the bottom-left corner is the default pure
/// black while `background` still reads near-white. Handing `resolve` the
/// style colour there picked the dark `#11182B` ink for a black corner:
/// 1.19:1, where the white ink would have given 21:1. Invisible, and
/// nothing went red, because the mark and the globe were built on separate
/// branches and no test rendered them together.
///
/// So this calls `space_clear_color` itself rather than re-deriving the
/// blend. Same argument `render::space::draws_backdrop`'s own doc comment
/// makes for being shared with the pixel tests: a second, private notion of
/// "what colour is behind this" is the thing that drifts.
///
/// # The flat path returns `background` UNCHANGED, `None` included
///
/// Not `space_clear_color(..)`, which is not the identity on `None`: it
/// substitutes transparent black, and `resolve` reads an opaque-black
/// backdrop as "use the white ink". A style with no `background` layer is
/// documented on [`BrandMarkVariant::resolve`] to get `OnLight`, because a
/// transparent surface is composited over the HOST's chrome and that is
/// more often light. Routing the flat case through the substitution would
/// have inverted exactly that, silently.
///
/// # What this is still only a proxy for
///
/// The clear colour, not the pixel under the mark. Two gaps, both narrower
/// than the one above and neither closed here:
///
///  - at PARTIAL globeness the disc is larger and may still cover the
///    bottom-left corner, so the corner can be basemap where this says it
///    is a blend toward space;
///  - on a FLAT frame the mark sits over tiles, not over the clear, so the
///    style `background` was only ever a proxy too -- a dark landuse
///    polygon under the corner has always been able to disagree with it.
///
/// Reading the rendered pixel back would close both and costs a readback
/// of the frame the mark is about to be drawn INTO, one frame late. The
/// clear colour is strictly the better proxy of the two available without
/// that, which is why it is the one chosen, and it is a proxy.
pub(crate) fn backdrop(
    background: Option<style::Rgba>,
    space: &crate::render::space::Space,
    transition: f32,
) -> Option<style::Rgba> {
    if transition <= 0.0 {
        return background;
    }
    Some(crate::render::space::space_clear_color(background, space, transition))
}

/// [`backdrop`] and [`BrandMarkVariant::resolve`], composed -- **the only
/// way a frame is allowed to pick its artwork.**
///
/// The two halves were correct and separately tested, and the frame still
/// drew the wrong ink, because nothing tested them TOGETHER in the order
/// `paint` uses: `resolve` was handed the style's `background` directly and
/// `backdrop` was not in the path at all. That is the shape of the bug
/// `a_light_style_on_a_full_globe_resolves_against_space_and_takes_the_white_ink`
/// is about, and the shape of the one a downstream desktop app shipped
/// for three days on the same feature -- their `dark_basemap` flag reached
/// the asset picker negated, so the white-ink file was chosen for light
/// backgrounds and the mark drew black on black. A logo that is correct
/// except for being invisible fails nothing: it decodes, it uploads, it
/// draws, it is simply not there.
///
/// So the composition is one named function rather than two calls a caller
/// arranges. `paint` calls it, the headless chrome fixture calls it, and
/// neither can arrange the two halves differently from the other -- the
/// failure mode is a fixture that agrees with itself while disagreeing with
/// the screen.
///
/// `paint` calls it EARLY, beside the clear colour it is about and a
/// thousand lines above the draw, for the reason recorded there: the draw
/// sits inside a `self.gpu` borrow and cannot reach `self.space`. The
/// answer is a `ResolvedVariant`, which is two cases and carries no colour,
/// so nothing downstream can re-decide it against something else.
pub(crate) fn resolve_for_frame(
    variant: BrandMarkVariant,
    background: Option<style::Rgba>,
    space: &crate::render::space::Space,
    transition: f32,
) -> ResolvedVariant {
    variant.resolve(backdrop(background, space, transition))
}

/// The dominant ink of `swat-on-light@2x.png`, `#11182B` -- the symbol's own
/// colour, straight out of `mark-dark.svg`'s single `fill="#11182B"` path.
/// Only [`BrandMarkVariant::resolve`] reads it, and only to decide which of
/// the two files to draw; nothing recolours a pixel from it.
const ON_LIGHT_INK: style::Rgba =
    style::Rgba { r: 17.0 / 255.0, g: 24.0 / 255.0, b: 43.0 / 255.0, a: 1.0 };

/// The dominant ink of `swat-on-dark@2x.png`: `mark-light.svg`'s single
/// `fill="white"` path.
const ON_DARK_INK: style::Rgba = style::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

/// WCAG 2.x relative luminance of an sRGB-encoded colour, alpha ignored.
///
/// Verbatim the formula
/// `the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
/// uses; kept here as a free function rather than copied into a test module
/// because this one runs in the draw path.
fn relative_luminance(color: style::Rgba) -> f64 {
    fn linearise(channel: f32) -> f64 {
        let c = channel as f64;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearise(color.r) + 0.7152 * linearise(color.g) + 0.0722 * linearise(color.b)
}

/// WCAG 2.x contrast ratio between two relative luminances.
fn contrast_ratio(a: f64, b: f64) -> f64 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The brand mark drawn in the map's bottom-left corner.
///
/// Every map has one already -- this is what to hand
/// [`crate::map_view::MapView::set_brand_mark`] when the default size or the
/// automatic variant is not what a particular surface needs.
///
/// ```no_run
/// # use gpui_maps::brand_mark::{BrandMark, BrandMarkVariant};
/// # fn demo(map: &mut gpui_maps::map_view::MapView) {
/// // Force the white-ink artwork, for a map whose background this crate
/// // cannot see -- a satellite raster, say.
/// map.set_brand_mark(Some(BrandMark {
///     variant: BrandMarkVariant::OnDark,
///     ..BrandMark::default()
/// }));
///
/// // Larger, for a surface that is not map chrome -- a splash or an export.
/// map.set_brand_mark(Some(BrandMark { height_px: 36.0, ..BrandMark::default() }));
///
/// // Full strength, for a host that knows its own basemap and does not want
/// // the mark held back -- see `BrandMark::DEFAULT_OPACITY` for why it is.
/// map.set_brand_mark(Some(BrandMark { opacity: 1.0, ..BrandMark::default() }));
/// # }
/// ```
///
/// Going the other way has a floor. The artwork contains a wordmark, and at
/// [`BrandMark::DEFAULT_HEIGHT_PX`] its cap height is 12 logical px, three
/// above the measured legibility floor of 9; scale the mark below 13.5 px
/// and the caps drop under that floor and stop resolving on a 1x display.
/// Nothing here enforces it -- a caller who wants a 12 px mark gets one --
/// but the artwork will be mush rather than small.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrandMark {
    /// Which artwork to draw. [`BrandMarkVariant::Auto`] by default.
    pub variant: BrandMarkVariant,
    /// Drawn height in LOGICAL pixels -- multiplied by the display's scale
    /// factor at draw time, exactly like `ATTRIBUTION_TEXT_SIZE_PX` and
    /// `chrome::CORNER_INSET_PX` are. The width follows from the artwork's
    /// own aspect ratio; there is no width field, because a mark you can
    /// stretch is a mark someone will stretch.
    pub height_px: f32,
    /// How strongly the mark is inked, 0.0 (invisible) to 1.0 (the
    /// artwork's own ink, undiluted). [`BrandMark::DEFAULT_OPACITY`] by
    /// default; see it for what the figure is and how it was arrived at.
    ///
    /// Applied to ALPHA alone, in the icon shader's non-SDF branch, so the
    /// mark fades toward whatever is behind it rather than toward black.
    /// Clamped to `0.0..=1.0` at the draw, so an out-of-range value from a
    /// host is a quiet clamp rather than a blend artefact.
    pub opacity: f32,
}

impl BrandMark {
    /// 18 logical pixels tall, so 87 wide at the vendored lockup's 4.8333
    /// aspect -- symbol, gap and "SWAT" wordmark together.
    ///
    /// # Its place in the corner
    ///
    /// Six [`GRID_UNIT_PX`]. It stands in the bottom-left corner with its
    /// INK -- not its box -- `chrome::BOTTOM_ROW_INSET_PX` (9) in from the
    /// left and up from the bottom, mirroring the credit's plate in the
    /// bottom-right, and it IS the bottom row's band
    /// (`chrome::BOTTOM_ROW_HEIGHT_PX`): the 15 px plate is centred on the
    /// mark's vertical centre, 1.5 px inside it top and bottom.
    /// `both_bottom_corners_put_their_ink_on_the_bottom_row_inset` and
    /// `map_view`'s `the_mark_and_the_credit_share_one_centre_line` measure
    /// both.
    ///
    /// It used to be the credit's plate height exactly, 18 against 18, and
    /// sat in the same band 4..22 px up; then both stood on a 12 px inset with
    /// a shared foot and baseline; now, because the credit sat too high,
    /// they share a centre line and their own inset: 6 px, then 9 when the
    /// mark sat too low at 6.
    ///
    /// # What is inside those 18, and why the ink fills them vertically
    ///
    /// HORIZONTALLY the artwork carries 6 px of margin before its visible
    /// ink (`ARTWORK_SIDE_MARGIN_FRACTION`), which [`instance`] subtracts so
    /// the ink, not the box, lands on the bottom-row inset.
    ///
    /// VERTICALLY it carries none. An earlier cut baked the credit's vertical
    /// plate padding (then 3) in above and below by symmetry with the
    /// horizontal margin, but the two do not do the same job: the credit's
    /// vertical padding holds its text off its own PLATE EDGE, and the mark
    /// has no plate, so 3 px of transparent nothing only made its ink smaller
    /// than the object beside it. So the SYMBOL fills the box: 18 px of ink,
    /// 0..18 in artwork coordinates.
    ///
    /// # The wordmark: the credit's line box, optically centred
    ///
    /// 12 px of cap height -- `ATTRIBUTION_TEXT_SIZE_PX *
    /// LABEL_LINE_HEIGHT_RATIO`, the credit's own line box, the figure the
    /// symbol used to be. It is NOT centred arithmetically in the box: the
    /// symbol is a pin, top-heavy, whose alpha-weighted centroid measures
    /// 47.06% down rather than 50%, so arithmetic centring would set the word
    /// 0.53 px low against it. The wordmark is centred on that CENTROID
    /// instead and rounded to the nearest half logical pixel -- a whole texel
    /// at the 2x density the artwork is cut for -- landing at 2.5..14.5,
    /// 0.03 px from the optical centre.
    ///
    /// That leaves the symbol overshooting the caps by 2.5 px above and
    /// 3.5 px below, which is what keeps the lockup reading as a symbol and
    /// a word rather than as five glyphs.
    ///
    /// # The ratio is 67%, and it is the first one that was ever chosen
    ///
    /// The previous cut's 75% was not a judgement, it was arithmetic with no
    /// freedom in it: the wordmark's measured legibility floor is 9 px of cap
    /// height (see below) and the symbol was pinned at 12 by the line-box
    /// reasoning, so 9 of 12 was the only figure available. Its own doc
    /// comment said as much, and noted that 75% sits above the 50-70% a
    /// horizontal lockup usually takes.
    ///
    /// With the symbol at 18 the floor stops binding and the ratio is a free
    /// choice for the first time. On [`GRID_UNIT_PX`] the candidates are 9
    /// (50%), 12 (67%) and 15 (83%), and they were cut and looked at rather
    /// than argued about: at 9 the word is a caption under a pin, at 15 the
    /// symbol overshoots by only 1.5 px and the lockup flattens into five
    /// glyphs, and 12 is the one where the symbol still leads and the word
    /// still reads. 13.5 -- 75% held for its own sake -- is off the grid and
    /// was the weakest of the four to look at, the word crowding the symbol
    /// top and bottom.
    ///
    /// # Why 9 px of wordmark is the floor, and why this clears it
    ///
    /// Measured rather than chosen. Rasterising the wordmark through the path
    /// that actually ships -- the 2x artwork, premultiplied-bilinear-halved
    /// to 1x -- and sweeping cap heights at both sub-pixel phases: at 8 px
    /// and below no stroke anywhere in the word reaches full ink and the A's
    /// counter is more than a fifth filled at every phase, which is mush; at
    /// 9 the counter opens (0.05) and every letter clears 0.91; it is fully
    /// crisp at every phase only from 11 px.
    ///
    /// The previous cut sat ON that floor at 9 and paid for it -- legible but
    /// soft on a 1x display, with the cost stated rather than glossed. At
    /// 12 the wordmark is past the crisp threshold at every phase, so that
    /// cost is simply gone, and it is gone because the symbol grew rather
    /// than because the box did.
    ///
    /// # Measured against the thing beside it
    ///
    /// Three cuts ago this was the symbol alone at 25 px: 1.39x the credit's
    /// plate, overtopping it by 7 px, which is what stopped it reading as
    /// chrome. Two cuts ago it was a 69x18 lockup whose ink was 12 px in an
    /// 18 px box -- the right box, two thirds of the ink. This is the same
    /// 18 px box with the ink filling it.
    ///
    /// For scale against the one real precedent: Mapbox GL JS's own logo
    /// control is 88x23 CSS px (`.mapboxgl-ctrl-logo`), also a
    /// symbol-plus-wordmark lockup. 87x18 is within a pixel of its width and
    /// five short of its height.
    ///
    /// Written as six [`GRID_UNIT_PX`] rather than as `18.0`, because that is
    /// what it is: the credit's plate is six units, and this is the same six.
    pub const DEFAULT_HEIGHT_PX: f32 = 6.0 * GRID_UNIT_PX;

    /// **0.80 -- the mark is held back, and by a measured amount.**
    ///
    /// # Why it is held back at all
    ///
    /// This mark is drawn on every map this engine renders, which is to say
    /// on every CONSUMER's map, over data this crate did not author and has
    /// never seen. A signature at full strength competes with the thing it
    /// is a signature on. That argument is SWAT's desktop app's
    /// (`BRAND_OPACITY` in `panels/vehicle_map.rs`: "it is a signature over
    /// somebody's map, and at full strength it competes with the data it
    /// sits on"), and it applies harder here than there: they stamp their
    /// own app, this stamps everyone's.
    ///
    /// Their FIGURE is not taken with it. Theirs is 0.85, which they hold to
    /// be a picked number rather than a measured one -- the first value
    /// tried, on one dark basemap, over vector data. That is not evidence
    /// about any other background, so the number below is measured here
    /// instead and lands somewhere else.
    ///
    /// # How 0.80 was arrived at
    ///
    /// The floor is legibility, and the standard is the one this crate
    /// already holds the licence credit to:
    /// `the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
    /// grades the credit at WCAG AA, 4.5:1. The mark is graded the same way,
    /// against the whole colour vocabulary of the styles in `examples/` --
    /// not just each style's `background` layer but every fill it paints,
    /// because what is actually under the bottom-left corner is a polygon,
    /// not the clear.
    ///
    /// The ink composites in sRGB-encoded space (`ALPHA_BLENDING` onto a
    /// `Unorm` target; see `icon_shader.wgsl`'s own note on the colour
    /// space), so `blend = a * ink + (1 - a) * background` per channel, and
    /// the contrast is WCAG 2.x on that. The binding case is the DARKEST
    /// fill any light style paints, `#88aaa2` in `swat-biz-light-style.json`:
    ///
    /// | background | what it is | ink | AA at a >= |
    /// | --- | --- | --- | --- |
    /// | `#88aaa2` | swat-biz light, darkest fill | dark | **0.756** |
    /// | `#5fd064` | liberty, darkest fill | dark | 0.684 |
    /// | `#a3c1de` | protomaps, darkest fill | dark | 0.679 |
    /// | `#cdd2d4` | protomaps, `background` layer | dark | 0.642 |
    /// | `#1a1e23` | swat-routes-dark, `background` | white | 0.456 |
    /// | `#000000` | `Space::color`, the globe's field | white | 0.455 |
    ///
    /// 0.756 is the floor, so 0.80 is the first twentieth above it, and at
    /// 0.80 the worst shipped case measures 4.93:1 while the dark end sits
    /// at 11.1:1 and 13.1:1. 0.75 would put the binding case at 4.44:1,
    /// under AA. The dark end never binds -- white on near-black has four
    /// times the headroom -- which is exactly why a figure measured on a
    /// dark basemap alone says nothing about this one.
    /// `the_default_opacity_clears_wcag_aa_on_every_shipped_basemap`
    /// recomputes the table and both bounds, so the figures above cannot be
    /// written here and be wrong.
    ///
    /// Cut and looked at as well as computed: at 0.75 and below the wordmark
    /// greys out and reads as disabled chrome rather than as quiet chrome,
    /// which is the qualitative edge the arithmetic puts at 0.756.
    ///
    /// # What is still only a proxy
    ///
    /// The same one [`backdrop`] names. A style can be pointed at a raster
    /// or satellite source this crate cannot sample, and a mid-grey
    /// background defeats a monochrome mark at ANY opacity -- at sRGB
    /// `#7C7C7C` both inks measure 4.2:1 with no opacity applied at all.
    /// That is a property of a mark with no plate behind it, and upstream
    /// considered the plate and rejected it (`src/assets/PROVENANCE.md`); it
    /// is not something this constant can fix. A host that knows its own
    /// basemap can raise this field, which is why it is a field.
    pub const DEFAULT_OPACITY: f32 = 0.80;
}

impl Default for BrandMark {
    fn default() -> Self {
        Self {
            variant: BrandMarkVariant::Auto,
            height_px: Self::DEFAULT_HEIGHT_PX,
            opacity: Self::DEFAULT_OPACITY,
        }
    }
}

/// Host override for the Mapbox case: whether the SWAT mark still draws
/// when a Mapbox wordmark is already on screen next to it.
///
/// # Why this exists, and why it is narrower than it looks
///
/// The mark is documented ([`BrandMarkVariant::resolve`]'s module, and
/// `crate::brand_mark`'s own doc) as not removable: "every map this crate
/// draws" gets one, and no public API turns that off for a map's own sake.
/// That stands. What changed is narrower: Mapbox GL JS's terms REQUIRE its
/// own logo, and a webview panel on Mapbox was stacking the SWAT mark
/// directly above it -- two marks, bottom-left, on a map whose basemap
/// already carries a mandatory credit mark of its own. [`Auto`] undoes
/// only that stacking, and only where a Mapbox wordmark is actually drawn;
/// every other map -- MapLibre, a local style, satellite with no logo --
/// keeps the mark exactly as before.
///
/// # No `Never`
///
/// A `Never` variant would let a host turn the mark off on every map by
/// simply never showing a Mapbox basemap, which is indistinguishable from
/// the removability this type is not here to grant. Only two cases exist:
/// follow the Mapbox rule ([`Auto`]), or keep both marks regardless
/// ([`Always`]) for a host that wants the belt-and-suspenders look back.
///
/// [`Auto`]: BrandMarkPolicy::Auto
/// [`Always`]: BrandMarkPolicy::Always
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrandMarkPolicy {
    /// Skip the SWAT mark exactly where a Mapbox wordmark is drawn
    /// alongside it; draw it everywhere else. The default.
    #[default]
    Auto,
    /// Draw the SWAT mark regardless of whether a Mapbox wordmark is also
    /// on screen -- the pre-existing, always-both behaviour.
    Always,
}

impl BrandMarkPolicy {
    /// Whether the SWAT mark should draw, given whether a Mapbox wordmark
    /// is drawn on this same surface this frame.
    #[must_use]
    pub fn draws_mark(self, mapbox_wordmark_shown: bool) -> bool {
        match self {
            BrandMarkPolicy::Always => true,
            BrandMarkPolicy::Auto => !mapbox_wordmark_shown,
        }
    }

    /// `mark` when this policy draws it (see [`Self::draws_mark`]),
    /// `None` otherwise -- a host that already turned the mark off
    /// (`mark` is `None`) stays off regardless of policy.
    #[must_use]
    pub fn effective(
        self,
        mark: Option<BrandMark>,
        mapbox_wordmark_shown: bool,
    ) -> Option<BrandMark> {
        mark.filter(|_| self.draws_mark(mapbox_wordmark_shown))
    }
}

/// Where the mark's top-left corner sits, in PHYSICAL pixels, so that its
/// INK stands `physical_inset` in from the left edge AND up from the bottom
/// edge -- `chrome::BOTTOM_ROW_INSET_PX * scale_factor`, the same figure
/// `symbol_pass::attribution_origin` stands the credit's plate on (from the
/// right) in the opposite corner.
///
/// **Ink, not box.** The artwork carries `side_margin` physical px of
/// margin left of its visible ink and none above or below it
/// ([`ARTWORK_SIDE_MARGIN_FRACTION`]). An earlier cut aligned the BOX: the
/// ink stood 4 + 6 = 10 px in from the left but 7 up from the bottom, and
/// the credit's plate 4 in from its corner -- the asymmetric corners a user
/// read as "not on a grid". So the margin is subtracted on the left, and
/// nothing on the bottom.
///
/// The INK edge is what is rounded to a whole physical pixel; the quad
/// follows it by the margin, which is a whole 6 px at 1x and 12 at 2x.
///
/// `top` keeps `attribution_origin`'s `max(0.0)` and `round()`: a viewport
/// shorter than the mark clips the mark's top rather than drawing above the
/// window. `left` needs no clamp: an over-wide mark runs off the right and
/// still shows its beginning.
///
/// The zero vertical ink margin is an assumption about a FILE, so
/// `the_artwork_ink_runs_to_its_own_top_and_bottom_edge` asserts it: a recut
/// that quietly adds vertical padding would otherwise lift the mark's ink off
/// the grid with nothing to report it.
pub(crate) fn brand_mark_origin(
    physical_viewport_height: f32,
    content_height: f32,
    physical_inset: f32,
    side_margin: f32,
) -> (f32, f32) {
    let top = physical_viewport_height - content_height - physical_inset;
    (physical_inset.round() - side_margin, top.max(0.0).round())
}

/// The artwork's margin left of its VISIBLE ink, as a fraction of its
/// HEIGHT: 12 of 36 device px in `swat-on-{light,dark}@2x.png`, so 6 logical
/// px at the default 18 -- the credit plate's own `ATTRIBUTION_PLATE_PADDING_X_PX`.
///
/// Twelve and not the nine the alpha bounding box says: columns 9..12 are the
/// symbol's antialiasing fringe, at most 17/255 alpha (4% under the 0.80
/// opacity), and column 12 is the first at 237. Aligning the bbox would hang
/// the ink the eye reads 1.5 px further in than the bottom-row inset -- the
/// box-not-ink mistake again, one level down.
///
/// [`instance`] subtracts it so the INK, not the quad, stands on
/// `chrome::BOTTOM_ROW_INSET_PX`; the vertical margin is zero.
/// `the_artwork_ink_runs_to_its_own_top_and_bottom_edge` pins both.
pub(crate) const ARTWORK_SIDE_MARGIN_FRACTION: f32 = 12.0 / 36.0;

/// Decodes one vendored artwork.
///
/// `load_from_memory_with_format` rather than `load_from_memory`, and the
/// reason is the browser build: `web/Cargo.toml` takes `image` with
/// `default-features = false, features = ["png"]`, where format GUESSING is
/// compiled out along with every other decoder. Naming PNG explicitly is what
/// makes this one line compile in both dependency sets.
pub(crate) fn decode(png: &[u8]) -> anyhow::Result<image::RgbaImage> {
    Ok(image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8())
}

impl ResolvedVariant {
    /// The PNG bytes this variant draws.
    pub(crate) fn png(self) -> &'static [u8] {
        match self {
            ResolvedVariant::OnLight => SWAT_ON_LIGHT_PNG,
            ResolvedVariant::OnDark => SWAT_ON_DARK_PNG,
        }
    }

    /// This variant's slot in `GpuState::brand_mark_atlases`.
    pub(crate) fn index(self) -> usize {
        match self {
            ResolvedVariant::OnLight => 0,
            ResolvedVariant::OnDark => 1,
        }
    }
}

/// How many artworks [`ResolvedVariant::index`] can return a slot for.
pub(crate) const RESOLVED_VARIANTS: usize = 2;

/// The single quad this mark draws, in PHYSICAL pixels.
///
/// Extracted from `map_view`'s paint loop rather than inlined there so the
/// acceptance test renders the SAME geometry production does, instead of a
/// second copy of the arithmetic that could agree with the doc comment while
/// disagreeing with the screen.
///
/// `artwork` is the decoded PNG's own pixel size, and it is what fixes the
/// WIDTH: the caller asks for a height in logical px and gets the artwork's
/// aspect ratio back, so the mark can never be stretched by a viewport, a
/// scale factor, or a caller.
///
/// `sdf: None` -- these are ordinary colour pixels, not a distance field, so
/// the shader draws the artwork's own ink and takes no `icon-color` from
/// anywhere. That is also what keeps the mark out of the SDF branch's
/// two-quads-per-instance path: one instance, one quad, six vertices.
pub(crate) fn instance(
    mark: &BrandMark,
    artwork: (u32, u32),
    physical_viewport_height: f32,
    scale_factor: f32,
) -> crate::render::IconInstance {
    let height = mark.height_px * scale_factor;
    let width = height * artwork.0 as f32 / artwork.1.max(1) as f32;
    // The INK, not the quad, stands on the bottom-row inset: the artwork's
    // transparent side margin is subtracted on the left, and its ink runs
    // to its own bottom edge so nothing is subtracted there.
    let (left, top) = brand_mark_origin(
        physical_viewport_height,
        height,
        crate::chrome::BOTTOM_ROW_INSET_PX * scale_factor,
        height * ARTWORK_SIDE_MARGIN_FRACTION,
    );
    crate::render::IconInstance {
        screen_x: left,
        screen_y: top,
        width,
        height,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: 0.0,
        sdf: None,
        // Clamped rather than trusted: this is a public field on a `Copy`
        // struct a host fills in, and an alpha outside 0..1 does not fail,
        // it blends wrongly. See `BrandMark::DEFAULT_OPACITY`.
        opacity: mark.opacity.clamp(0.0, 1.0),
    }
}

/// Moves the mark's `quad` up to sit `gap` above the credit plate `plate`
/// `(x, y, w, h)` when the two would meet (a frame too narrow for both on
/// one row), so the mark never draws over the licence credit; leaves it at
/// its corner otherwise. One rule for every shell (`map_view`'s
/// `lift_mark_clear_of_credit`, the iOS shell).
pub(crate) fn lift_clear_of_plate(quad: &mut crate::render::IconInstance, plate: (f32, f32, f32, f32), gap: f32) {
    let (x, y, w, h) = plate;
    let meets = quad.screen_x < x + w + gap
        && x - gap < quad.screen_x + quad.width
        && quad.screen_y < y + h + gap
        && y - gap < quad.screen_y + quad.height;
    if meets {
        quad.screen_y = (y - gap - quad.height).max(0.0).round();
    }
}

/// Where the wordmark's baseline sits, as a fraction of the artwork's height
/// up from its bottom edge: the lowest row of the four capitals' ink at 50%
/// alpha (S, W, A, T have no descenders, so their lowest ink IS the
/// baseline). Measured off the vendored pixels for `map_view`'s
/// `the_mark_and_the_credit_share_one_centre_line`, which prints it.
#[cfg(test)]
pub(crate) fn wordmark_baseline_fraction() -> f32 {
    let art = decode(SWAT_ON_LIGHT_PNG).expect("light artwork decodes");
    let (w, h) = art.dimensions();
    let inked = |x: u32| (0..h).any(|y| art.get_pixel(x, y).0[3] > 127);
    // The wordmark starts past the first clear column after the symbol.
    let symbol_start = (0..w).find(|&x| inked(x)).expect("symbol ink");
    let symbol_end = (symbol_start..w).find(|&x| !inked(x)).expect("a gap after the symbol");
    let lowest = (0..h)
        .rev()
        .find(|&y| (symbol_end..w).any(|x| art.get_pixel(x, y).0[3] > 127))
        .expect("wordmark ink");
    (h - 1 - lowest) as f32 / h as f32
}

/// The vendored artwork's pixel size, read from its PNG header (`IHDR`'s
/// width and height, big-endian at bytes 16..24) rather than decoded: both
/// artworks share one geometry
/// (`both_vendored_artworks_decode_to_the_same_geometry_with_an_alpha_channel`),
/// and this is on the frame path of callers that only want the rectangle.
pub(crate) fn artwork_size() -> (u32, u32) {
    let be = |at: usize| {
        u32::from_be_bytes([
            SWAT_ON_LIGHT_PNG[at],
            SWAT_ON_LIGHT_PNG[at + 1],
            SWAT_ON_LIGHT_PNG[at + 2],
            SWAT_ON_LIGHT_PNG[at + 3],
        ])
    };
    (be(16), be(20))
}

impl BrandMark {
    /// Where this mark draws on a map `map_height_px` tall, as `(x, y,
    /// width, height)` in LOGICAL pixels from the map's own top-left corner.
    ///
    /// For anything laid out over a map -- a host's legend or hint card, the
    /// navigation chrome -- that must keep clear of the mark. It is
    /// [`instance`] at a scale factor of 1, the geometry the draw itself
    /// uses, so a card placed against it cannot drift from the mark it is
    /// avoiding (`the_footprint_is_the_drawn_quad`).
    #[must_use]
    pub fn footprint(&self, map_height_px: f32) -> (f32, f32, f32, f32) {
        let quad = instance(self, artwork_size(), map_height_px, 1.0);
        (quad.screen_x, quad.screen_y, quad.width, quad.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Policy times basemap gives which mark is drawn: `Auto` skips the
    /// SWAT mark exactly when a Mapbox wordmark is shown, `Always` never
    /// does, and a host that already turned the mark off (`mark: None`)
    /// stays off under either policy.
    #[test]
    fn policy_times_basemap_decides_whether_the_mark_draws() {
        let mark = Some(BrandMark::default());
        assert_eq!(BrandMarkPolicy::Auto.effective(mark, true), None, "Mapbox wordmark shown: skip the mark");
        assert_eq!(BrandMarkPolicy::Auto.effective(mark, false), mark, "no Mapbox wordmark: keep the mark");
        assert_eq!(BrandMarkPolicy::Always.effective(mark, true), mark, "Always keeps it regardless");
        assert_eq!(BrandMarkPolicy::Always.effective(mark, false), mark);
        assert_eq!(BrandMarkPolicy::Auto.effective(None, true), None, "already off stays off");
        assert_eq!(BrandMarkPolicy::Auto.effective(None, false), None);
        assert_eq!(BrandMarkPolicy::Always.effective(None, true), None);
    }

    #[test]
    fn auto_is_the_default_policy() {
        assert_eq!(BrandMarkPolicy::default(), BrandMarkPolicy::Auto);
    }

    /// `footprint` is the quad `paint` draws, at scale 1, from the header
    /// read -- checked against a real decode so the header offsets cannot be
    /// wrong -- and the default lands where the module doc says: its box 0
    /// 3 in (ink 9), 9 up, 18 tall (12 / 12 before the bottom row got its
    /// own inset, then 6 / 6).
    #[test]
    fn the_footprint_is_the_drawn_quad() {
        let decoded = decode(SWAT_ON_LIGHT_PNG).expect("decodes");
        assert_eq!(artwork_size(), decoded.dimensions(), "the IHDR read agrees with the decoder");
        let mark = BrandMark::default();
        let quad = instance(&mark, decoded.dimensions(), 600.0, 1.0);
        assert_eq!(mark.footprint(600.0), (quad.screen_x, quad.screen_y, quad.width, quad.height));
        let (x, y, w, h) = mark.footprint(600.0);
        // Ink on the bottom-row inset: the box 9 - 6 in (its 6 px side
        // margin) and 9 up (no vertical margin).
        assert_eq!((x, y, h), (3.0, 600.0 - 9.0 - 18.0, 18.0));
        assert!((w - 87.0).abs() < 0.01, "18 tall at the lockup's 4.8333 aspect, got {w}");
    }

    fn grey(v: f32) -> style::Rgba {
        style::Rgba { r: v, g: v, b: v, a: 1.0 }
    }

    /// The two figures `BrandMarkVariant::resolve`'s doc comment states, so
    /// neither can be written there and be wrong here. Solved by bisection
    /// rather than asserted at a guessed number: the crossover is a property
    /// of the two inks, and if either artwork is ever recoloured this test
    /// reports the new one instead of merely going red.
    #[test]
    fn the_variant_crossover_sits_where_the_two_inks_contrast_equally() {
        let dark_ink = relative_luminance(ON_LIGHT_INK);
        assert!(
            (dark_ink - 0.0095).abs() < 0.0001,
            "#11182B's relative luminance is stated as 0.0095 in `resolve`'s doc comment, \
             measured {dark_ink:.4}"
        );
        assert_eq!(relative_luminance(ON_DARK_INK), 1.0, "the other ink is pure white");

        // Bisect on the sRGB channel value, since that is what a style
        // authors -- the answer is reported in BOTH spaces.
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            match BrandMarkVariant::Auto.resolve(Some(grey(mid))) {
                ResolvedVariant::OnDark => lo = mid,
                ResolvedVariant::OnLight => hi = mid,
            }
        }
        let crossover_srgb = 0.5 * (lo + hi);
        let crossover_luma = relative_luminance(grey(crossover_srgb));
        assert!(
            (crossover_luma - 0.1999).abs() < 0.0005,
            "`resolve`'s doc comment states the crossover at relative luminance 0.1999, \
             measured {crossover_luma:.4} (sRGB {:.0}/255)",
            crossover_srgb * 255.0
        );
        assert!(
            (crossover_srgb * 255.0 - 124.0).abs() < 1.0,
            "`resolve`'s doc comment states the crossover at sRGB ~#7C7C7C (124), measured \
             {:.1}",
            crossover_srgb * 255.0
        );
    }

    /// The end the mistake would actually be made at: a dark basemap must
    /// not get the dark-ink mark.
    #[test]
    fn auto_takes_the_white_mark_onto_a_dark_basemap_and_the_dark_mark_onto_a_light_one() {
        // `#141414`, the darkest basemap the attribution's own contrast test
        // reasons about, and pure white.
        assert_eq!(
            BrandMarkVariant::Auto.resolve(Some(grey(20.0 / 255.0))),
            ResolvedVariant::OnDark
        );
        assert_eq!(BrandMarkVariant::Auto.resolve(Some(grey(1.0))), ResolvedVariant::OnLight);
        // No `background` layer at all.
        assert_eq!(BrandMarkVariant::Auto.resolve(None), ResolvedVariant::OnLight);
        // An explicit variant ignores the basemap entirely -- that is what
        // "explicit" is for.
        assert_eq!(
            BrandMarkVariant::OnDark.resolve(Some(grey(1.0))),
            ResolvedVariant::OnDark
        );
        assert_eq!(
            BrandMarkVariant::OnLight.resolve(Some(grey(0.0))),
            ResolvedVariant::OnLight
        );
    }

    /// **The combination case, and the reason this file has a `backdrop` at
    /// all.** A LIGHT style and a FULL globe: the corner the mark is drawn
    /// into is `Space::color`, pure black by default, while the style's
    /// `background` is still near-white. Deciding against the style there is
    /// what put the dark `#11182B` ink on a black field.
    ///
    /// Neither branch could fail this alone, which is why nothing did: the
    /// brand mark never saw a globe, and the globe never drew a mark. The
    /// assertion is on what gets HANDED to `resolve`, not on `resolve` --
    /// `resolve` was always right about the colour it was given.
    #[test]
    fn a_light_style_on_a_full_globe_resolves_against_space_and_takes_the_white_ink() {
        let space = crate::render::space::Space::default();
        assert_eq!(
            space.color,
            grey(0.0),
            "this test's premise is the default space colour being black; if that default \
             moved, the contrast it is about moved with it"
        );
        // The style says white. The screen, at transition 1.0, is black.
        let seen = backdrop(Some(grey(1.0)), &space, 1.0);
        assert_eq!(
            seen,
            Some(grey(0.0)),
            "at full globeness the corner is `Space::color`, not the style's background"
        );
        assert_eq!(
            BrandMarkVariant::Auto.resolve(seen),
            ResolvedVariant::OnDark,
            "a black corner wants the WHITE ink (21:1); the style's own near-white would \
             have chosen the dark ink against black, which measures 1.19:1"
        );
        // And the bug, stated as the thing that must not come back: the raw
        // style colour chooses the other artwork.
        assert_eq!(
            BrandMarkVariant::Auto.resolve(Some(grey(1.0))),
            ResolvedVariant::OnLight,
            "pinned so the line above is visibly a DIFFERENT answer -- if these two ever \
             agree, this test has stopped testing anything"
        );

        // --- and the same thing again through the COMPOSITION ---
        //
        // Everything above grades `backdrop` and `resolve` arranged by this
        // test. That is not what a frame does, and it is the gap the bug
        // lived in: both halves were right and the frame still drew the
        // wrong ink, because nothing checked the ORDER they are used in.
        // `resolve_for_frame` is the single call `paint` and the headless
        // chrome fixture both make, so this is the arrangement that ships.
        for (what, transition, expected) in [
            ("a flat frame decides against the style", 0.0, ResolvedVariant::OnLight),
            ("a full globe decides against space", 1.0, ResolvedVariant::OnDark),
        ] {
            assert_eq!(
                resolve_for_frame(BrandMarkVariant::Auto, Some(grey(1.0)), &space, transition),
                expected,
                "{what}: one near-white style, two frames, and the composition must answer \
                 differently for them -- if it stops doing that, the two halves have been \
                 composed the wrong way round"
            );
        }

        // An explicit variant must survive the composition untouched: a host
        // that has pinned `OnDark` for a satellite raster gets `OnDark` on a
        // globe frame and on a flat one.
        for transition in [0.0, 0.5, 1.0] {
            assert_eq!(
                resolve_for_frame(BrandMarkVariant::OnDark, Some(grey(1.0)), &space, transition),
                ResolvedVariant::OnDark,
                "an explicit variant is not a suggestion the backdrop may overrule"
            );
        }
    }

    /// The flat path must be the identity, `None` included.
    ///
    /// `space_clear_color` is NOT the identity on `None` -- it substitutes
    /// transparent black, which `resolve` reads as "dark basemap, use the
    /// white ink". Routing the flat case through it would have inverted the
    /// documented no-background behaviour (`OnLight`) without touching a
    /// single assertion about `resolve` itself.
    #[test]
    fn a_flat_frame_decides_against_the_style_exactly_as_it_did_before() {
        let space = crate::render::space::Space::default();
        assert_eq!(backdrop(None, &space, 0.0), None, "no background layer stays no answer");
        assert_eq!(
            BrandMarkVariant::Auto.resolve(backdrop(None, &space, 0.0)),
            ResolvedVariant::OnLight,
            "a transparent surface is composited over the host's chrome; `resolve` documents \
             `OnLight` as the safer guess and the flat path must still reach it"
        );
        for g in [0.0, 20.0 / 255.0, 0.5, 1.0] {
            assert_eq!(
                backdrop(Some(grey(g)), &space, 0.0),
                Some(grey(g)),
                "a flat frame passes the style background through unchanged"
            );
        }
    }

    /// Both artworks must decode, be the same geometry (the draw loop picks
    /// between them per frame and a size change would make the mark jump when
    /// a style is switched), and carry real transparency -- a mark shipped as
    /// an opaque rectangle would draw a white or black box over the map.
    #[test]
    fn both_vendored_artworks_decode_to_the_same_geometry_with_an_alpha_channel() {
        let light = decode(SWAT_ON_LIGHT_PNG).expect("light artwork decodes");
        let dark = decode(SWAT_ON_DARK_PNG).expect("dark artwork decodes");
        assert_eq!(light.dimensions(), (174, 36));
        assert_eq!(dark.dimensions(), light.dimensions());

        // Stronger than equal dimensions: equal ALPHA, texel for texel. The
        // two files are one composition rendered twice with a single fill
        // changed, which is what upstream's own pair is, so a redraw of one
        // that silently moved a letter or a margin fails here rather than
        // being discovered on a dark basemap months later.
        let differing = light
            .pixels()
            .zip(dark.pixels())
            .filter(|(l, d)| l.0[3] != d.0[3])
            .count();
        assert_eq!(
            differing, 0,
            "the two variants must be one shape in two inks, but {differing} texels differ in \
             alpha -- they have become two drawings that can drift"
        );
        for (what, image) in [("light", &light), ("dark", &dark)] {
            let transparent = image.pixels().filter(|p| p.0[3] == 0).count();
            assert!(
                transparent > image.pixels().len() / 10,
                "{what} artwork is {transparent} of {} texels transparent -- a brand mark with \
                 no cut-out is a box drawn over the map",
                image.pixels().len()
            );
        }
    }

    /// The half of this module a wrong answer would be silent in: which
    /// artwork each variant actually holds.
    ///
    /// The upstream files are named for their INK (`mark-dark.svg` IS the
    /// dark-ink one, drawn on a light basemap) and these two for their
    /// BACKGROUND, so vendoring them crossed the names over. No filename can
    /// catch that having been done backwards and no rendering test would go
    /// red for it either -- a white mark on a white map draws, it is merely
    /// invisible. Only the pixels can say, so this reads the dominant
    /// fully-opaque colour out of each PNG and checks it against the ink
    /// constant [`BrandMarkVariant::resolve`] reasons with.
    #[test]
    fn each_variant_carries_the_ink_its_name_promises() {
        for (variant, ink, what) in [
            (ResolvedVariant::OnLight, ON_LIGHT_INK, "a light basemap wants the dark ink"),
            (ResolvedVariant::OnDark, ON_DARK_INK, "a dark basemap wants the white ink"),
        ] {
            let art = decode(variant.png()).expect("artwork decodes");
            let mut counts: std::collections::HashMap<[u8; 3], usize> =
                std::collections::HashMap::new();
            for p in art.pixels().filter(|p| p.0[3] == 255) {
                *counts.entry([p.0[0], p.0[1], p.0[2]]).or_default() += 1;
            }
            let (dominant, n) = counts
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .expect("the artwork has fully opaque ink");
            let expected = [
                (ink.r * 255.0).round() as u8,
                (ink.g * 255.0).round() as u8,
                (ink.b * 255.0).round() as u8,
            ];
            assert_eq!(
                dominant, expected,
                "{what}: {variant:?}'s dominant opaque colour over {n} texels is \
                 #{:02X}{:02X}{:02X}, not the #{:02X}{:02X}{:02X} `resolve` reasons with",
                dominant[0], dominant[1], dominant[2], expected[0], expected[1], expected[2]
            );
        }
    }

    /// The artwork's inked elements, left to right, in LOGICAL pixels of the
    /// drawn box -- `(left, right, top, bottom)` each.
    ///
    /// Segmented at the 50% ALPHA boundary rather than at "any alpha", and
    /// that is the whole reason this helper exists rather than a bbox call.
    /// A threshold near zero measures the outermost antialiasing texel, which
    /// on a 9 px wordmark bleeds adjacent letters into one run and reports
    /// gaps half a pixel narrow; the 50% crossing is where the coverage
    /// integral says the edge actually is, and it reproduces the composition's
    /// authored figures to within a texel.
    fn elements() -> Vec<(f32, f32, f32, f32)> {
        let art = decode(SWAT_ON_LIGHT_PNG).expect("light artwork decodes");
        let (w, h) = art.dimensions();
        // The artwork is 2x, so a texel is half a logical pixel.
        let scale = BrandMark::DEFAULT_HEIGHT_PX / h as f32;
        let inked = |x: u32| (0..h).any(|y| art.get_pixel(x, y).0[3] > 127);

        let mut runs: Vec<(u32, u32)> = Vec::new();
        let mut start: Option<u32> = None;
        for x in 0..w {
            match (inked(x), start) {
                (true, None) => start = Some(x),
                (false, Some(s)) => {
                    runs.push((s, x));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            runs.push((s, w));
        }

        runs.into_iter()
            .map(|(x0, x1)| {
                let rows: Vec<u32> = (0..h)
                    .filter(|&y| (x0..x1).any(|x| art.get_pixel(x, y).0[3] > 127))
                    .collect();
                (
                    x0 as f32 * scale,
                    x1 as f32 * scale,
                    rows[0] as f32 * scale,
                    (rows[rows.len() - 1] + 1) as f32 * scale,
                )
            })
            .collect()
    }

    /// The lockup's geometry, measured off the vendored pixels -- every figure
    /// [`GRID_UNIT_PX`] and [`BrandMark::DEFAULT_HEIGHT_PX`] claim.
    ///
    /// This is the test that makes "grid-disciplined" a property of the
    /// artwork rather than a sentence in a doc comment. Five inked elements
    /// (the symbol, then S, W, A, T), and the intervals between them and the
    /// box edges are the authored ones: 6 px of margin on the left (the
    /// credit's own `ATTRIBUTION_PLATE_PADDING_X_PX`), NONE above or below,
    /// and a 6 px gap.
    ///
    /// The vertical margin being zero is the point of this cut, not an
    /// omission -- see [`BrandMark::DEFAULT_HEIGHT_PX`] on why the two
    /// margins are not the same figure -- so it is asserted as an equality
    /// with 0 rather than left unchecked.
    ///
    /// The tolerance is half a logical pixel throughout, because the artwork
    /// is a 2x raster: one texel is the finest distinction it can carry, and
    /// asking for better would be asserting on antialiasing.
    #[test]
    fn the_lockup_sits_on_its_own_grid() {
        let e = elements();
        assert_eq!(
            e.len(),
            5,
            "the lockup is a symbol and four capitals; {} inked runs were found, so either the \
             artwork changed or the letters have merged",
            e.len()
        );

        let t = 0.5;
        let symbol = e[0];
        let pad_x = crate::symbol_pass::ATTRIBUTION_PLATE_PADDING_X_PX;
        // The vertical margin an earlier cut baked in by symmetry with the
        // credit's plate padding of the day; see below.
        let pad_y = 3.0_f32;

        // --- the margins are the credit's own paddings ---
        assert!(
            (symbol.0 - pad_x).abs() < t,
            "the ink must start {pad_x} px in from the box's left edge, the same inset the \
             credit's plate gives its text; measured {:.2}",
            symbol.0
        );
        assert!(
            symbol.2.abs() < t && (BrandMark::DEFAULT_HEIGHT_PX - symbol.3).abs() < t,
            "the symbol must fill the box vertically -- no transparent margin above or below, \
             because there is no plate edge for one to hold the ink away from; measured top \
             {:.2}, bottom {:.2} of {}",
            symbol.2,
            symbol.3,
            BrandMark::DEFAULT_HEIGHT_PX
        );
        // Named as the figure this cut deliberately does NOT use: the
        // previous one baked `pad_y` in above and below by symmetry with
        // the horizontal margin, and the symmetry was false -- see
        // `BrandMark::DEFAULT_HEIGHT_PX`. Asserted as an inequality so that
        // re-baking it fails here with the reason attached rather than
        // merely tripping the equality above.
        assert!(
            (symbol.2 - pad_y).abs() > t,
            "the symbol's top margin has gone back to the credit's vertical plate padding \
             ({pad_y}); the mark has no plate edge for that padding to hold its ink off, and \
             3 px of transparent nothing is what made the mark read smaller than the credit \
             it shares the band with"
        );

        // --- the symbol's ink is the whole box: six grid units ---
        assert!(
            (symbol.3 - symbol.2 - BrandMark::DEFAULT_HEIGHT_PX).abs() < t,
            "the symbol's ink must stand the whole {} px box; measured {:.2}",
            BrandMark::DEFAULT_HEIGHT_PX,
            symbol.3 - symbol.2
        );

        // --- the wordmark: cap height, and where its baseline sits ---
        let (wm_left, wm_right) = (e[1].0, e[4].1);
        let wm_top = e[1..].iter().fold(f32::MAX, |a, r| a.min(r.2));
        let wm_bottom = e[1..].iter().fold(f32::MIN, |a, r| a.max(r.3));
        let cap = wm_bottom - wm_top;
        let line_box = crate::symbol_pass::ATTRIBUTION_TEXT_SIZE_PX
            * crate::symbol_pass::LABEL_LINE_HEIGHT_RATIO;
        assert!(
            (cap - line_box).abs() < t,
            "the wordmark's cap height is the credit's own {line_box} px line box; measured \
             {cap:.2}"
        );
        assert!(
            cap >= 9.0 - t,
            "9 px is the MEASURED legibility floor for this wordmark at 1x (see \
             `BrandMark::DEFAULT_HEIGHT_PX`), and the previous cut sat on it. This one clears \
             it by three, which is what bought back the crispness -- measured {cap:.2}"
        );
        assert!(
            (cap / (symbol.3 - symbol.2) - 2.0 / 3.0).abs() < 0.02,
            "the wordmark is two thirds of the symbol -- 67%, chosen among the grid's 50/67/83 \
             by cutting all three; measured {:.1}%",
            cap / (symbol.3 - symbol.2) * 100.0
        );

        // The symbol must overshoot the caps at BOTH ends, or the lockup
        // reads as five letters rather than a symbol and a word.
        assert!(
            wm_top > symbol.2 + 0.5 && wm_bottom < symbol.3 - 0.5,
            "the symbol must overshoot the caps top and bottom; symbol {:.2}..{:.2}, caps \
             {wm_top:.2}..{wm_bottom:.2}",
            symbol.2,
            symbol.3
        );

        // --- the gap, against the wordmark's OWN letterspacing ---
        //
        // The gap is a grid figure and the letterspacing scales with the cap
        // height, so growing the wordmark from 9 px to 12 narrowed this
        // ratio from ~3.7x to ~2.8x without the gap moving. It still clears
        // the bound, and the bound is where it is because 2.5x is roughly
        // where a reader stops seeing two objects; the figure is printed so
        // a future cut that grows the caps again sees how much is left.
        let gap = wm_left - symbol.1;
        assert!(
            (gap - 2.0 * GRID_UNIT_PX).abs() < t,
            "the symbol-to-wordmark gap is two grid units, {}; measured {gap:.2}",
            2.0 * GRID_UNIT_PX
        );
        let letter_gaps: Vec<f32> = (1..4).map(|i| e[i + 1].0 - e[i].1).collect();
        let mean_letter_gap = letter_gaps.iter().sum::<f32>() / letter_gaps.len() as f32;
        assert!(
            gap > mean_letter_gap * 2.5,
            "the gap must be decisively wider than the wordmark's own letterspacing or the \
             symbol reads as a fifth glyph: gap {gap:.2}, mean letter gap \
             {mean_letter_gap:.2} ({:.1}x)",
            gap / mean_letter_gap
        );

        // --- every authored figure is a whole number of grid units ---
        for (what, value) in [
            ("the box height", BrandMark::DEFAULT_HEIGHT_PX),
            ("the left margin", symbol.0),
            ("the top margin", symbol.2),
            ("the symbol's height", symbol.3 - symbol.2),
            ("the wordmark's cap height", cap),
            ("the gap", gap),
        ] {
            let units = value / GRID_UNIT_PX;
            assert!(
                (units - units.round()).abs() < t / GRID_UNIT_PX,
                "{what} is {value:.2} px, which is {units:.3} grid units of {GRID_UNIT_PX} -- \
                 not a whole one"
            );
        }

        // The drawn width follows from the artwork, so it is measured, not
        // chosen -- but it must still land on the grid, or the right-hand
        // margin is carrying a fraction the eye can see against the box.
        let art = decode(SWAT_ON_LIGHT_PNG).expect("decodes");
        let width = BrandMark::DEFAULT_HEIGHT_PX * art.dimensions().0 as f32
            / art.dimensions().1 as f32;
        assert!(
            (width - 87.0).abs() < 0.01,
            "the doc comment states 87 logical px wide at the default height, measured \
             {width:.3}"
        );
        assert!(
            (width / GRID_UNIT_PX - 29.0).abs() < 0.01,
            "87 is 29 grid units; measured {:.3}",
            width / GRID_UNIT_PX
        );
        assert!(
            wm_right < width,
            "the wordmark must end inside the box; {wm_right:.2} of {width:.2}"
        );
    }

    /// Monochrome as a property of the FILE, not a claim about it.
    ///
    /// Every texel of each artwork carries exactly one rgb -- the variant's
    /// own ink -- and varies only in alpha. Two things follow, and both are
    /// why this is asserted rather than assumed. A second colour could not
    /// enter the mark without failing here, whatever a future re-cut does
    /// upstream; and the grey fringe a straight-alpha downscale puts around
    /// white ink (`PROVENANCE.md` describes the one that bit the previous
    /// cut) is impossible by construction, because there is no second colour
    /// available to average toward.
    #[test]
    fn the_artwork_is_monochrome_by_construction() {
        for (variant, ink, what) in [
            (ResolvedVariant::OnLight, ON_LIGHT_INK, "the dark ink"),
            (ResolvedVariant::OnDark, ON_DARK_INK, "the white ink"),
        ] {
            let art = decode(variant.png()).expect("artwork decodes");
            let expected = [
                (ink.r * 255.0).round() as u8,
                (ink.g * 255.0).round() as u8,
                (ink.b * 255.0).round() as u8,
            ];
            let mut seen: std::collections::HashSet<[u8; 3]> = std::collections::HashSet::new();
            for p in art.pixels() {
                seen.insert([p.0[0], p.0[1], p.0[2]]);
            }
            assert_eq!(
                seen.len(),
                1,
                "{variant:?} must be one colour and an alpha channel ({what}), but {} distinct \
                 rgb values are present",
                seen.len()
            );
            assert_eq!(
                *seen.iter().next().expect("one colour"),
                expected,
                "{variant:?}'s single colour must be {what}"
            );
        }
    }

    /// Both bottom corners on ONE row, measured on INK at 1x, 2x and 3x:
    ///
    /// * the mark's visible ink stands `chrome::BOTTOM_ROW_INSET_PX` (9) in
    ///   from the left AND up from the bottom -- through [`instance`], the
    ///   production path, and the artwork's own measured ink edges
    ///   ([`elements`]);
    /// * the credit's plate -- the object the eye reads in that corner --
    ///   stands the same inset in from the right, and its foot
    ///   `chrome::bottom_row_foot_px(15)` = 10.5 up, through
    ///   `symbol_pass::attribution_origin`: centred on the mark's centre.
    ///
    /// Was `CORNER_INSET_PX` (12) for both until the bottom row got its own
    /// inset, first 6 and then 9. The shaped-credit half of the claim
    /// is `map_view`'s `the_mark_and_the_credit_share_one_centre_line`.
    #[test]
    fn both_bottom_corners_put_their_ink_on_the_bottom_row_inset() {
        let inset = crate::chrome::BOTTOM_ROW_INSET_PX;
        let foot = crate::chrome::bottom_row_foot_px(15.0);
        assert_eq!(foot, 10.5);
        let e = elements();
        let symbol = e[0];
        // The helper's 50%-alpha left edge IS the margin `instance` subtracts.
        assert!(
            (symbol.0 - BrandMark::DEFAULT_HEIGHT_PX * ARTWORK_SIDE_MARGIN_FRACTION).abs() < 0.01,
            "the margin instance() subtracts ({}) must be the artwork's measured ink edge ({:.2})",
            BrandMark::DEFAULT_HEIGHT_PX * ARTWORK_SIDE_MARGIN_FRACTION,
            symbol.0
        );
        let artwork = artwork_size();
        for scale_factor in [1.0f32, 2.0, 3.0] {
            let (viewport_w, viewport_h) = (1200.0 * scale_factor, 800.0 * scale_factor);
            let quad = instance(&BrandMark::default(), artwork, viewport_h, scale_factor);
            let per_px = quad.height / BrandMark::DEFAULT_HEIGHT_PX;
            let ink_left = (quad.screen_x + symbol.0 * per_px) / scale_factor;
            let ink_bottom = (viewport_h - (quad.screen_y + symbol.3 * per_px)) / scale_factor;
            assert!(
                (ink_left - inset).abs() < 0.01 && (ink_bottom - inset).abs() < 0.01,
                "at {scale_factor}x the mark's ink stands {ink_left:.2} px in from the left and \
                 {ink_bottom:.2} up from the bottom; both must be the bottom-row inset {inset}"
            );

            let (plate_w, plate_h) = (180.0 * scale_factor, 15.0 * scale_factor);
            let (plate_left, plate_top) = crate::symbol_pass::attribution_origin(
                viewport_w,
                viewport_h,
                plate_w,
                plate_h,
                inset * scale_factor,
                foot * scale_factor,
            );
            let right = (viewport_w - (plate_left + plate_w)) / scale_factor;
            let bottom = (viewport_h - (plate_top + plate_h)) / scale_factor;
            // The foot is 10.5: rounded to a whole physical pixel at 1x and 3x
            // (31.5 -> 31 at 3x is exactly half a physical pixel, so the bound
            // carries a float epsilon).
            let half_px = 0.5 / scale_factor + 1e-3;
            assert!(
                (right - inset).abs() < 0.01 && (bottom - foot).abs() <= half_px,
                "at {scale_factor}x the credit's plate stands {right:.2} px in from the right \
                 and {bottom:.2} up from the bottom; must be {inset} and {foot}"
            );
            let (mark_centre, plate_centre) = (ink_bottom + 9.0, bottom + 7.5);
            assert!(
                (mark_centre - plate_centre).abs() <= half_px,
                "at {scale_factor}x the mark is centred {mark_centre:.2} up and the plate {plate_centre:.2}"
            );
        }

        // A lockup is mostly counterform and letterspacing where a solid
        // symbol was mostly ink. Pinned so that swapping the artwork for
        // something near-solid -- a filled plate, a box -- is noticed here
        // rather than on a screen.
        let art = decode(SWAT_ON_LIGHT_PNG).expect("light artwork decodes");
        let ink =
            art.pixels().filter(|p| p.0[3] > 8).count() as f32 / art.pixels().len() as f32;
        assert!(
            (ink - 0.314).abs() < 0.02,
            "a 31.4%-ink lockup, measured {:.1}%",
            ink * 100.0
        );
    }

    /// `chrome` is dependency-free and states the bottom row's band as 18;
    /// it is the default mark's height, and the lift gap is the row's inset.
    #[test]
    fn the_bottom_row_band_is_the_default_mark() {
        assert_eq!(crate::chrome::BOTTOM_ROW_HEIGHT_PX, BrandMark::DEFAULT_HEIGHT_PX);
        assert_eq!(crate::chrome::BOTTOM_ROW_INSET_PX, 3.0 * GRID_UNIT_PX);
        assert_eq!(crate::chrome::BOTTOM_ROW_TOP_PX, 27.0);
    }

    /// What one frame's brand mark costs on the CPU, and what its very first
    /// frame costs once.
    ///
    /// `#[ignore]`d for the reason every other timing in this crate is
    /// (`overlay::plan::the_merged_bake_against_the_resolved_one`,
    /// `map_view::paint_update_versus_a_full_push_on_the_simulation_dataset`):
    /// a timing is evidence, not an assertion, it belongs to whoever asks for
    /// it, and it would be flaky as a gate on a shared machine. Run it with
    ///
    ///     cargo test --release --lib -- --ignored the_per_frame_cost --nocapture
    ///
    /// and only in release -- a debug build measures the optimiser's absence,
    /// not this code.
    ///
    /// # What is and is not in the number
    ///
    /// The per-frame arm is exactly what `map_view::paint` runs on a frame
    /// whose texture is already uploaded: resolve the variant against the
    /// style's own background colour, and build the one quad. The texture
    /// upload is NOT per-frame -- it happens on the first frame that draws a
    /// given variant and is measured separately below.
    ///
    /// What no CPU stopwatch here can see is the draw: six vertices appended
    /// to a vertex buffer the symbol pass already writes and already submits,
    /// one extra `set_bind_group` and one extra `pass.draw` of two triangles
    /// over 87x18 logical px. That is one draw call added to a frame that
    /// already issues one per tile mesh, per icon batch and per overlay
    /// source.
    #[test]
    #[ignore = "a timing, not an assertion"]
    fn the_per_frame_cost_of_a_brand_mark() {
        // `crate::timing::Instant`, not `std::time`: web compiles this file (`tests/wasm_portability.rs`).

        let mark = BrandMark::default();
        let background = Some(style::Rgba { r: 0.95, g: 0.94, b: 0.91, a: 1.0 });
        let artwork = decode(SWAT_ON_LIGHT_PNG).expect("decodes").dimensions();

        // Warm, then measure. 1e6 iterations because one is far below the
        // clock's own resolution.
        const N: u32 = 1_000_000;
        for _ in 0..1000 {
            let v = mark.variant.resolve(background);
            std::hint::black_box(instance(&mark, artwork, 1600.0, 2.0));
            std::hint::black_box(v);
        }
        let started = crate::timing::Instant::now();
        for _ in 0..N {
            let v = std::hint::black_box(mark.variant.resolve(std::hint::black_box(background)));
            std::hint::black_box(instance(
                std::hint::black_box(&mark),
                artwork,
                1600.0,
                2.0,
            ));
            std::hint::black_box(v);
        }
        let per_frame = started.elapsed() / N;
        println!("brand mark, per frame (resolve + build the quad): {per_frame:?}");

        // The one-time half: decoding the vendored PNG, which happens once
        // per variant per view, on the first frame that draws it.
        const D: u32 = 200;
        let started = crate::timing::Instant::now();
        for _ in 0..D {
            std::hint::black_box(decode(SWAT_ON_LIGHT_PNG).expect("decodes"));
        }
        let per_decode = started.elapsed() / D;
        println!("brand mark, once per variant (PNG decode): {per_decode:?}");
    }

    /// **The figure on [`BrandMark::DEFAULT_OPACITY`], recomputed.**
    ///
    /// Not an assertion that the constant equals 0.80 -- that would only
    /// check it against itself. This re-derives the floor the constant
    /// claims to sit above, by bisection, from the same WCAG maths
    /// `BrandMarkVariant::resolve` already uses, against the colours the
    /// styles in `examples/` actually paint. If either bound moves, this
    /// reports the new one rather than merely going red.
    ///
    /// The backgrounds are the DARKEST fill of each light style and the
    /// background layer of each dark one, because the binding case for a
    /// mark with no plate is the darkest thing a light basemap can put under
    /// the corner, not the style's own clear. They are written out here
    /// rather than parsed out of `examples/` at test time: a style file is
    /// data this crate renders, not a fixture it is allowed to depend on,
    /// and the table in the doc comment is the thing that has to stay true.
    #[test]
    fn the_default_opacity_clears_wcag_aa_on_every_shipped_basemap() {
        /// WCAG AA for normal text -- the same bar
        /// `the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
        /// holds the licence credit to, in the opposite corner of the same
        /// band.
        const AA: f64 = 4.5;

        fn srgb(hex: u32) -> style::Rgba {
            style::Rgba {
                r: ((hex >> 16) & 0xFF) as f32 / 255.0,
                g: ((hex >> 8) & 0xFF) as f32 / 255.0,
                b: (hex & 0xFF) as f32 / 255.0,
                a: 1.0,
            }
        }

        // What the GPU does: straight `ALPHA_BLENDING` onto a `Unorm`
        // target, so the mix is in sRGB-ENCODED space, not in linear light.
        // `icon_shader.wgsl` carries the warning about changing one without
        // the other; this must stay the same arithmetic.
        fn composite(ink: style::Rgba, background: style::Rgba, opacity: f32) -> style::Rgba {
            style::Rgba {
                r: opacity * ink.r + (1.0 - opacity) * background.r,
                g: opacity * ink.g + (1.0 - opacity) * background.g,
                b: opacity * ink.b + (1.0 - opacity) * background.b,
                a: 1.0,
            }
        }

        fn ratio(ink: style::Rgba, background: style::Rgba, opacity: f32) -> f64 {
            contrast_ratio(
                relative_luminance(composite(ink, background, opacity)),
                relative_luminance(background),
            )
        }

        /// The lowest opacity at which this ink still clears AA on this
        /// background, by bisection -- `ratio` is monotonic in `opacity`
        /// on either side of the crossover, which is what makes that valid.
        fn aa_floor(ink: style::Rgba, background: style::Rgba) -> Option<f32> {
            if ratio(ink, background, 1.0) < AA {
                return None;
            }
            let (mut lo, mut hi) = (0.0f32, 1.0f32);
            for _ in 0..40 {
                let mid = 0.5 * (lo + hi);
                if ratio(ink, background, mid) >= AA {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            Some(hi)
        }

        // (what it is, sRGB, the ink `resolve` picks for it, the floor the
        // doc comment states)
        let cases = [
            ("swat-biz-light, darkest fill", 0x88AAA2u32, ON_LIGHT_INK, 0.756f32),
            ("liberty, darkest fill", 0x5FD064, ON_LIGHT_INK, 0.684),
            ("protomaps, darkest fill", 0xA3C1DE, ON_LIGHT_INK, 0.679),
            ("protomaps, background layer", 0xCDD2D4, ON_LIGHT_INK, 0.642),
            ("swat-routes-dark, background layer", 0x1A1E23, ON_DARK_INK, 0.456),
            ("Space::color, the globe's field", 0x000000, ON_DARK_INK, 0.455),
        ];

        let mut binding: Option<(&str, f32)> = None;
        for (what, hex, ink, stated) in cases {
            let background = srgb(hex);

            // The ink each background calls for is not asserted from the
            // table -- it is asked of `resolve`, so a table that drifted out
            // of step with the crossover fails here instead of quietly
            // grading the wrong artwork.
            let wanted = match BrandMarkVariant::Auto.resolve(Some(background)) {
                ResolvedVariant::OnLight => ON_LIGHT_INK,
                ResolvedVariant::OnDark => ON_DARK_INK,
            };
            assert_eq!(
                (wanted.r, wanted.g, wanted.b),
                (ink.r, ink.g, ink.b),
                "{what} (#{hex:06X}) is listed with the wrong ink: `resolve` picks the other one"
            );

            let floor = aa_floor(ink, background)
                .unwrap_or_else(|| panic!("{what} cannot reach {AA}:1 at any opacity"));
            assert!(
                (floor - stated).abs() < 0.005,
                "`BrandMark::DEFAULT_OPACITY`'s table states an AA floor of {stated:.3} for \
                 {what} (#{hex:06X}); measured {floor:.3}"
            );
            assert!(
                ratio(ink, background, BrandMark::DEFAULT_OPACITY) >= AA,
                "at the default opacity {} the mark measures {:.2}:1 over {what} \
                 (#{hex:06X}) -- under WCAG AA, which is the bar the licence credit in the \
                 other corner is held to",
                BrandMark::DEFAULT_OPACITY,
                ratio(ink, background, BrandMark::DEFAULT_OPACITY)
            );
            if binding.is_none_or(|(_, f)| floor > f) {
                binding = Some((what, floor));
            }
        }

        let (binding_what, binding_floor) =
            binding.expect("at least one background was graded");
        assert_eq!(
            binding_what, "swat-biz-light, darkest fill",
            "the doc comment names swat-biz-light's darkest fill as the binding case; the \
             measured worst is {binding_what} at {binding_floor:.3}"
        );

        // --- both bounds on the constant, and neither is a restatement ---
        assert!(
            BrandMark::DEFAULT_OPACITY > binding_floor,
            "the default opacity {} is at or under the measured floor {binding_floor:.3} \
             ({binding_what}) -- the mark would be under WCAG AA on a basemap this repository \
             ships a style for",
            BrandMark::DEFAULT_OPACITY
        );
        // Held back, and OBSERVABLY so. Asserted against the binding case's
        // own full-strength contrast rather than as `DEFAULT_OPACITY < 1.0`,
        // which is a sentence about a constant that costs nothing to satisfy
        // -- this fails if the mark is drawn at full strength and also if it
        // is drawn so near it that "held back" is a claim the constant does
        // not cash.
        let binding_bg = srgb(0x88AAA2);
        let at_full = ratio(ON_LIGHT_INK, binding_bg, 1.0);
        let as_drawn = ratio(ON_LIGHT_INK, binding_bg, BrandMark::DEFAULT_OPACITY);
        assert!(
            as_drawn < at_full * 0.85,
            "the mark is a signature over somebody else's data and must be visibly held back \
             from full strength, but over the binding background it measures {as_drawn:.2}:1 \
             against {at_full:.2}:1 undiluted -- see `BrandMark::DEFAULT_OPACITY`"
        );
        // And that 0.80 is the FIRST twentieth above the floor, which is
        // what makes it derived rather than picked. 0.75 must fail.
        assert!(
            (BrandMark::DEFAULT_OPACITY - 0.80).abs() < f32::EPSILON,
            "the constant is {}, but the measured floor {binding_floor:.3} rounds up to 0.80 \
             on a twentieths scale",
            BrandMark::DEFAULT_OPACITY
        );
        let one_step_down = BrandMark::DEFAULT_OPACITY - 0.05;
        assert!(
            ratio(ON_LIGHT_INK, srgb(0x88AAA2), one_step_down) < AA,
            "{one_step_down} must be UNDER AA on the binding case, or the default is one step \
             louder than it needs to be; measured {:.2}:1",
            ratio(ON_LIGHT_INK, srgb(0x88AAA2), one_step_down)
        );

        // The proxy the constant's doc comment ends on: a monochrome mark
        // with no plate is defeated by a mid-grey at ANY opacity, so this is
        // not a claim about arbitrary raster imagery.
        let mid = grey(0x7C as f32 / 255.0);
        for ink in [ON_LIGHT_INK, ON_DARK_INK] {
            assert!(
                ratio(ink, mid, 1.0) < AA,
                "sRGB #7C7C7C is the crossover, where neither ink clears AA even undiluted -- \
                 if that stopped being true the doc comment's last section is wrong"
            );
        }
    }

    /// The opacity is a FIELD, it reaches the quad, and a host cannot blend
    /// with it out of range.
    ///
    /// `instance` is the only place the figure is read, so this is the whole
    /// of the plumbing: a `BrandMark` in, an `IconInstance` whose `opacity`
    /// the icon shader multiplies into alpha out.
    #[test]
    fn the_marks_opacity_reaches_the_quad_and_is_clamped() {
        let artwork = decode(SWAT_ON_LIGHT_PNG).expect("decodes").dimensions();

        let default = instance(&BrandMark::default(), artwork, 600.0, 2.0);
        assert_eq!(
            default.opacity,
            BrandMark::DEFAULT_OPACITY,
            "a default mark draws at the measured default opacity, not at full strength"
        );
        assert!(
            default.opacity < 1.0,
            "the default must be held back from full strength -- that is the whole point of \
             the field"
        );

        // A host that knows its own basemap turns it up.
        let loud = instance(
            &BrandMark { opacity: 1.0, ..BrandMark::default() },
            artwork,
            600.0,
            2.0,
        );
        assert_eq!(loud.opacity, 1.0);

        // Out of range is clamped, not passed through: an alpha above 1 or
        // below 0 does not fail anywhere, it blends wrongly.
        for (given, expected) in [(1.5f32, 1.0f32), (-0.2, 0.0), (f32::INFINITY, 1.0)] {
            let mark = BrandMark { opacity: given, ..BrandMark::default() };
            assert_eq!(
                instance(&mark, artwork, 600.0, 2.0).opacity,
                expected,
                "an opacity of {given} must clamp to {expected}"
            );
        }

        // Nothing else about the quad moves with it -- the mark's geometry
        // and its ink strength are independent.
        let quiet = instance(
            &BrandMark { opacity: 0.2, ..BrandMark::default() },
            artwork,
            600.0,
            2.0,
        );
        assert_eq!((quiet.screen_x, quiet.screen_y), (default.screen_x, default.screen_y));
        assert_eq!((quiet.width, quiet.height), (default.width, default.height));
    }

    /// The mark's ink sits `inset` in from the left (its quad that less the
    /// artwork's side margin) and its quad's bottom `inset` up from the
    /// bottom, mirroring `attribution_origin`'s conventions -- and never
    /// scrolls off the top of a viewport too short to hold it.
    #[test]
    fn the_mark_sits_an_inset_in_from_the_bottom_left_corner() {
        let (left, top) = brand_mark_origin(600.0, 50.0, 12.0, 6.0);
        assert_eq!((left, top), (6.0, 538.0));

        // A viewport shorter than the mark: clamped to the top edge, exactly
        // as `attribution_origin` clamps, so the mark's own top is what is
        // cut off rather than being drawn above the window.
        let (_left, top) = brand_mark_origin(10.0, 50.0, 12.0, 6.0);
        assert_eq!(top, 0.0);
    }

    /// `brand_mark_origin`'s vertical padding assumes the artwork's ink runs
    /// to the very top and bottom of its own bitmap, so that insetting the
    /// BOX by the credit's plate padding lands the INK where the credit's ink
    /// is. That is a fact about a FILE, not about this module, and a recut
    /// that quietly added vertical padding would lift the mark again with
    /// nothing to report it -- this is what reports it.
    ///
    /// The horizontal margin is asserted as a RANGE rather than a figure: it
    /// is allowed to exist (it is what lands the ink at 8.5 against the
    /// credit's 10) and only a gross change should fail here. The doc comment
    /// on this module once said 6 px where the file measured 4.5, which is
    /// exactly the drift a range catches and a prose comment does not.
    #[test]
    fn the_artwork_ink_runs_to_its_own_top_and_bottom_edge() {
        for (name, png) in
            [("on-light", SWAT_ON_LIGHT_PNG), ("on-dark", SWAT_ON_DARK_PNG)]
        {
            let art = decode(png).expect("the vendored artwork decodes");
            let (w, h) = art.dimensions();
            let opaque = |x: u32, y: u32| art.get_pixel(x, y).0[3] != 0;

            let top = (0..h).find(|&y| (0..w).any(|x| opaque(x, y))).expect("ink");
            let bottom =
                (0..h).rev().find(|&y| (0..w).any(|x| opaque(x, y))).expect("ink");
            assert_eq!(
                (top, h - 1 - bottom),
                (0, 0),
                "{name}: the artwork grew a vertical ink margin, so the mark no \
                 longer lands its ink on the credit's -- either trim the file or \
                 subtract the new margin in `brand_mark_origin`"
            );

            let left = (0..w).find(|&x| (0..h).any(|y| opaque(x, y))).expect("ink");
            let right =
                (0..w).rev().find(|&x| (0..h).any(|y| opaque(x, y))).expect("ink");
            for (side, margin) in [("left", left), ("right", w - 1 - right)] {
                assert!(
                    (6..=14).contains(&margin),
                    "{name}: {side} ink margin is {margin} device px, outside the \
                     range the horizontal inset was reasoned against"
                );
            }
            // The VISIBLE ink -- the first column past half alpha -- is what
            // `ARTWORK_SIDE_MARGIN_FRACTION` subtracts, and it must say so
            // exactly: a recut that moves it moves the mark's ink off the
            // bottom-row inset.
            let visible = (0..w).find(|&x| (0..h).any(|y| art.get_pixel(x, y).0[3] > 127)).expect("ink");
            assert_eq!(
                visible as f32 / h as f32,
                ARTWORK_SIDE_MARGIN_FRACTION,
                "{name}: the visible ink starts at column {visible} of a {h} px tall artwork"
            );
        }
    }

}
