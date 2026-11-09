//! The map's corner chrome grid: two insets, one for the top corners and
//! one for the bottom row.
//!
//! The compass (top-right), the navigation chrome and a host's floating
//! cards stand [`CORNER_INSET_PX`] in from both edges of their corner. The brand mark
//! (bottom-left) and the attribution plate (bottom-right) form the BOTTOM
//! ROW: they stand [`BOTTOM_ROW_INSET_PX`] in from their side edge and share
//! one horizontal centre line, the centre of a [`BOTTOM_ROW_HEIGHT_PX`] band
//! whose foot is that inset up from the bottom. Every figure is measured to
//! what the eye reads -- the mark's ink, the credit's plate, the compass's
//! disc -- and never to a transparent box around it. Two objects stacked in
//! one corner (the mark lifted above a credit too wide to share a row) are
//! separated by the bottom-row inset, as MapLibre stacks its controls.
//!
//! gpui-free and dependency-free, so the browser and phone shells compile it
//! by `#[path]` with the modules that read it.

/// The TOP corners' chrome inset from the map's edges, in LOGICAL pixels
/// (multiplied by the display's scale factor at draw time, like every other
/// chrome length).
///
/// **Twelve, and why.** The references are MapLibre and Mapbox GL JS, whose
/// `.maplibregl-ctrl-bottom-left .maplibregl-ctrl { margin: 0 0 10px 10px }`
/// (`maplibre-gl.css:105`) puts the logo and every control 10 CSS px in,
/// with the expanded attribution flush in its corner (`margin: 0`, line 484)
/// and the compact one back at 10 (line 491). Ten is the right ORDER, and
/// twelve is the figure on this engine's own grids:
///
/// * it is four `brand_mark::GRID_UNIT_PX` (3), the module the mark and the
///   credit's plate are cut on, where 10 is off it;
/// * it was already the inset of everything else in a corner -- the compass
///   (`compass::COMPASS_MARGIN_PX`), the navigation chrome
///   (`nav::chrome::MARGIN`) and the demo panels' floating cards (`CARD_INSET`,
///   `.top(px(12.0)).left(px(12.0))`) -- so the two odd ones out (the mark and
///   the credit at 4) join the majority rather than the majority moving;
/// * at 2x it is 24 device px, a whole number of the artwork's texels, and at
///   1x it is 12 whole pixels: no half-pixel inset at either density.
///
/// It replaces a 4 px window inset that had the mark's ink 8.5 px from the
/// left but 7 from the bottom and the credit's plate flush-ish at 4: three
/// different distances in two corners, which is what read as "not on a grid".
///
/// **The bottom row no longer stands on it.**
/// With the mark's ink and the credit's plate both 12 up, the credit sat
/// too high for a licence line, and its plate (15 tall) and the
/// mark (18 tall) shared a foot but not a middle, so the two bottom corners
/// did not read as one row. MapLibre itself puts the expanded attribution
/// flush in its corner (`margin: 0`, line 484): the credit is not a control
/// and does not take a control's margin. So the bottom row has its own,
/// tighter [`BOTTOM_ROW_INSET_PX`] and one centre line ([`BOTTOM_ROW_HEIGHT_PX`]);
/// twelve stays the inset of everything above it. (The row first took 6,
/// which the user then read as the logo sitting "quite low"; it is 9, the
/// half-way figure on the same grid -- see [`BOTTOM_ROW_INSET_PX`].)
pub const CORNER_INSET_PX: f32 = 12.0;

/// The bottom row's inset, in LOGICAL pixels: the brand mark's ink stands
/// this far in from the left AND up from the bottom, the credit's plate this
/// far in from the right, and a mark lifted above a too-wide credit stands
/// this far above the plate.
///
/// **Nine**: three `brand_mark::GRID_UNIT_PX`, on the module the mark and
/// the plate are cut on. Between MapLibre's flush expanded attribution (0)
/// and its 10 px control margin: the credit sits low, as a licence line
/// should, while the mark keeps a visible margin off the window's rounded
/// corner.
///
/// **Two user datapoints bracket it** (both 2026-10-08). At twelve -- the
/// [`CORNER_INSET_PX`] the row shared with the compass -- the credit read as
/// "quite high". At six (two units) the mark then read as "again quite low".
/// Nine is the one grid line between them. At 1x the plate's foot,
/// `9 + (18 - 15) / 2` = 10.5, is a half pixel that the plate's origin rounds
/// to a whole physical pixel (within half a pixel of the mark's centre line);
/// at 2x every figure in the row is a whole device pixel.
pub const BOTTOM_ROW_INSET_PX: f32 = 9.0;

/// The bottom row's band height, in LOGICAL pixels: the default brand mark's
/// height (`brand_mark::BrandMark::DEFAULT_HEIGHT_PX`, 18), whose foot stands
/// on [`BOTTOM_ROW_INSET_PX`]. Shorter objects in the row -- the credit's
/// one-line plate (15) and its compact "i" button (15) -- are centred in it,
/// so the plate's foot stands `BOTTOM_ROW_INSET_PX + (18 - 15) / 2` = 10.5 up
/// and the mark and the credit share one horizontal centre line, 18 up. A
/// stacked (multi-line) plate keeps its BOTTOM line on that centre line and
/// grows upward.
///
/// Dependency-free like the rest of this module, so it states the 18 rather
/// than reading it from `brand_mark`; `brand_mark`'s
/// `the_bottom_row_band_is_the_default_mark` pins the two equal.
pub const BOTTOM_ROW_HEIGHT_PX: f32 = 18.0;

/// The bottom row band's TOP, up from the map's bottom edge, in LOGICAL
/// pixels: `BOTTOM_ROW_INSET_PX + BOTTOM_ROW_HEIGHT_PX` = 27. Floating UI a
/// host puts in a bottom corner (a legend, a time slider) stands its own
/// gap above this, so it never covers the mark or the credit.
pub const BOTTOM_ROW_TOP_PX: f32 = BOTTOM_ROW_INSET_PX + BOTTOM_ROW_HEIGHT_PX;

/// How far up from the bottom edge an object `height` logical px tall stands
/// so that it is centred on the bottom row's centre line -- never below
/// [`BOTTOM_ROW_INSET_PX`] (an object taller than the band stands on the
/// inset and grows up). LOGICAL pixels.
#[must_use]
pub fn bottom_row_foot_px(height: f32) -> f32 {
    BOTTOM_ROW_INSET_PX + ((BOTTOM_ROW_HEIGHT_PX - height) / 2.0).max(0.0)
}
