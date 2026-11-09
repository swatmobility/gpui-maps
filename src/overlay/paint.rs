//! A feature's paint, read from its GeoJSON `properties`.
//!
//! The keys are Mapbox's own paint property names. Borrowing them rather
//! than inventing a vocabulary means anyone who has written a style knows
//! this already, and it left the obvious upgrade path open: `line-width`
//! now accepts a zoom expression -- `interpolate`, `step`, `case`, `match`
//! on zoom -- through the very same compiler the basemap style uses
//! (`style_spec::expression::compile_zoom_value`), with no change to the
//! key or to what a bare number means.
//!
//! ONLY `line-width` is zoom-varying so far. Every other property here is
//! still a constant, deliberately: `line-width` is the one the user can
//! actually see get it wrong. A caller's production mapbox-gl style says
//! `"line-width": ["interpolate", ["linear"], ["zoom"], 8, 2.4, 18, 7.2]`,
//! and drawing that as one constant made our lines 50% too thick at z9 and
//! 31% too thin at z16 -- which, because each route draws a dark casing
//! under a vivid core, also put half again as much saturated colour on
//! screen and read to them as "colour more intense than mapbox". The rest
//! (`line-color`, `fill-color`, `circle-radius`, the text properties) can
//! follow the same route when something asks for it; each one costs only a
//! `ZoomValue` field and a `value_at_pair` at plan time, since the
//! machinery below is already generic.

use serde_json::{Map, Value};

use crate::style::{DashPattern, Filter, Rgba, ZoomValue};

const DEFAULT_BLUE: &str = "#3b82f6";
const DEFAULT_TEXT: &str = "#1a1a1a";
const DEFAULT_FILL_ALPHA: f32 = 0.3;
const DEFAULT_HALO: &str = "#ffffff";

/// Mapbox defaults `text-halo-width` to 0. This defaults it to 1 instead,
/// deliberately.
///
/// A basemap label is authored against the style it sits in, so its author
/// knows whether it needs a halo. An overlay label is drawn over whatever
/// basemap the application happens to be using, at whatever the camera
/// happens to be showing -- dark water, pale paper, a coloured road casing.
/// Nobody can author for that, and a halo-less default reproduces exactly
/// the complaint this was added for: dark text laid over a saturated line,
/// unreadable.
///
/// A caller who wants Mapbox's behaviour asks for `"text-halo-width": 0`.
const DEFAULT_HALO_WIDTH: f32 = 1.0;

/// Which of an icon's axes grow to wrap its label -- Mapbox's
/// `icon-text-fit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextFit {
    /// Draw the sprite at its authored size. Mapbox's default.
    #[default]
    None,
    Width,
    Height,
    Both,
}

/// `text-justify`: how a MULTI-line label's lines align against each
/// other. Independent of `text-anchor`/`text-offset`, which position the
/// whole block relative to the feature's point -- this only decides how
/// the lines sit within that block. Mapbox's own default is `center`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextJustify {
    #[default]
    Center,
    Left,
    Right,
}

impl TextJustify {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "center" => Self::Center,
            "left" => Self::Left,
            "right" => Self::Right,
            _ => return None,
        })
    }
}

impl TextFit {
    pub fn fits_width(self) -> bool {
        matches!(self, Self::Width | Self::Both)
    }

    pub fn fits_height(self) -> bool {
        matches!(self, Self::Height | Self::Both)
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "none" => Self::None,
            "width" => Self::Width,
            "height" => Self::Height,
            "both" => Self::Both,
            _ => return None,
        })
    }
}

/// Which point of an icon lands on the feature's own position -- Mapbox's
/// `icon-anchor`, with the style spec's own nine values.
///
/// Named for the part of the ICON that touches the anchor, which is the
/// opposite of the direction the icon moves: `Bottom` puts the icon's
/// bottom edge on the point, so the icon sits ABOVE it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconAnchor {
    #[default]
    Center,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl IconAnchor {
    /// The offset from the feature's point to the icon's CENTRE, as a
    /// fraction of the icon's own width and height.
    ///
    /// `Center` is `(0, 0)`; `Bottom` is `(0, -0.5)`, moving the icon up by
    /// half its height so its bottom edge lands on the point. y is DOWN, as
    /// everywhere else in the draw path.
    pub fn centre_offset(self) -> (f32, f32) {
        let x = match self {
            Self::Left | Self::TopLeft | Self::BottomLeft => 0.5,
            Self::Right | Self::TopRight | Self::BottomRight => -0.5,
            Self::Center | Self::Top | Self::Bottom => 0.0,
        };
        let y = match self {
            Self::Top | Self::TopLeft | Self::TopRight => 0.5,
            Self::Bottom | Self::BottomLeft | Self::BottomRight => -0.5,
            Self::Center | Self::Left | Self::Right => 0.0,
        };
        (x, y)
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "center" => Self::Center,
            "left" => Self::Left,
            "right" => Self::Right,
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "top-left" => Self::TopLeft,
            "top-right" => Self::TopRight,
            "bottom-left" => Self::BottomLeft,
            "bottom-right" => Self::BottomRight,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeaturePaint {
    pub line_color: Rgba,
    /// The one zoom-varying property here -- see this module's own doc
    /// comment for why it is the one. Resolve it with `value_at_pair(z)`,
    /// never `value_at(z)` alone: the pair is what `shader.wgsl` blends by
    /// `fract(camera.zoom)`, and taking a single value snaps the width at
    /// every integer bucket instead of tracking the curve.
    pub line_width: ZoomValue<f32>,
    pub line_dash: DashPattern,
    pub fill_color: Rgba,
    pub circle_radius: f32,
    pub circle_color: Rgba,
    pub icon_image: Option<String>,
    /// `icon-color`/`icon-halo-color`/`icon-halo-width`, applying ONLY when
    /// `icon_image` names an SDF sprite. A plain sprite draws its own
    /// pixels and ignores all three -- the style spec's own rule.
    ///
    /// This is the pair of properties a caller most wants data-driven: one
    /// pin sprite, recoloured per route. On the basemap side the same
    /// properties are deliberately zoom-only (see
    /// `style_spec::single_zoom_case`), because a style layer that varied
    /// them per feature would multiply into one compiled layer per case.
    /// Here every feature carries its own paint already, so per-feature is
    /// what it costs nothing to support.
    pub icon_color: Rgba,
    pub icon_halo_color: Rgba,
    /// In POINTS, like every other width here -- not in the sprite's own
    /// pixels, which is what MapLibre's `icon-halo-width` measures. The two
    /// agree only for a sprite at `pixel_ratio` 1.
    ///
    /// **A halo can reach at most `6 / pixel_ratio` points**: a distance
    /// field (`sprite::sdf_from_mask`'s, and every Mapbox/MapLibre SDF
    /// sprite's) encodes 6 of its own pixels outside the shape, so 2 pt on
    /// a 3x sprite, 3 pt on a 2x one. A wider halo saturates at that reach
    /// rather than growing; ask for a thicker border by giving the sprite
    /// a lower pixel ratio, or by drawing the border into the shape.
    pub icon_halo_width: f32,
    /// `icon-anchor`: which part of the icon sits ON the feature's point.
    ///
    /// Mapbox's default is `center`, which is right for a symmetric badge
    /// and wrong for a PIN -- a teardrop wants its tip on the stop, not its
    /// middle, so `bottom` puts the point it actually marks under the
    /// anchor.
    pub icon_anchor: IconAnchor,
    /// `icon-rotate`: the sprite's rotation in DEGREES, clockwise, in y-down
    /// screen space. Applied by `icon_shader.wgsl`'s `vs_main` and never on
    /// the CPU, so a caller that rewrites only this between frames re-uploads
    /// one float per feature and re-tessellates nothing.
    ///
    /// Degrees, and named with the unit, to match the field this crate
    /// already exposes for the identical style-spec property on the basemap
    /// side: `style::SymbolFeature::icon_rotate_deg`, which
    /// `symbol_pass::symbol_angle_rad` converts with `.to_radians()`. Two
    /// public fields for one style-spec property in different units -- one
    /// unit-suffixed and one not -- is a confusion worth more than the one
    /// `.to_radians()` call this costs at the draw site. Degrees is also the
    /// style spec's own unit AND the unit a compass bearing arrives in, which
    /// is the data a caller actually holds.
    ///
    /// Unlike a basemap symbol, an overlay feature has no line to align
    /// along, so this is the ONLY way its rotation can be expressed -- which
    /// is why the field exists: a caller with a compass bearing per vehicle
    /// previously had to tessellate a rotated polygon to show it.
    pub icon_rotate_deg: f32,
    /// `icon-text-fit`: which of the icon's axes grow so its content box
    /// wraps the label. `None` -- Mapbox's default -- draws the sprite at
    /// its authored size, which is right for a marker and wrong for a
    /// badge, since a badge that cannot grow clips or crowds anything
    /// longer than the label its author drew it around.
    pub icon_text_fit: TextFit,
    /// `icon-allow-overlap` (with `icon-ignore-placement`): draw this
    /// feature's ICON always, and let it block nothing.
    ///
    /// Default FALSE, matching Mapbox. It matters most on a caller's own
    /// data: stop pins along a route crowd together as the camera zooms
    /// out, and collision then drops most of them -- correct for a
    /// basemap's place names, wrong for features an application put on the
    /// map deliberately.
    pub icon_allow_overlap: bool,
    /// `text-allow-overlap` (with `text-ignore-placement`): the same, for
    /// the LABEL.
    ///
    /// Kept separate from `icon_allow_overlap` because a style writes the
    /// two separately, but the draw path does NOT take a symbol apart on
    /// them: either flag bypasses collision for the whole billboard, pin
    /// and number together.
    ///
    /// Honouring them independently was tried and is wrong. An icon that
    /// bypassed collision while its label lost the placement contest left
    /// an empty badge on the map -- a pin marking a stop, with no number on
    /// it. A pin and its number are one thing to a reader, and whatever is
    /// decided has to apply to both.
    ///
    /// Collision also cannot express "draw always but occupy space", so
    /// each flag means both of its Mapbox properties together -- the same
    /// limitation the basemap's `bypass_collision` documents.
    pub text_allow_overlap: bool,
    /// `icon-text-fit-padding`, `[top, right, bottom, left]` in logical
    /// points -- Mapbox's order, which is CSS's, not the
    /// `[left, top, right, bottom]` its `content` box uses.
    pub icon_text_fit_padding: [f32; 4],
    pub text_field: Option<String>,
    /// `text-offset`, in EMS of the label's own size -- Mapbox's unit, so
    /// the nudge scales with the text rather than drifting as it grows.
    ///
    /// Positive y is DOWN, matching the style spec. Needed as soon as
    /// `icon_anchor` is not `center`: the label hangs off the feature's
    /// point, so moving the icon off that point without moving the label
    /// leaves the two apart.
    pub text_offset: (f32, f32),
    /// `text-anchor`: which part of the label sits at the (offset) point.
    /// `left` hangs a name off to the right of its marker (a bus stop's
    /// name, the transit desktop's style). Default `center`, as before.
    pub text_anchor: crate::style::TextAnchor,
    pub text_justify: TextJustify,
    /// NOT a Mapbox property -- gpui-map's own escape hatch from
    /// `clip_labels_behind_later_icons` (`map_view.rs`), the workaround that
    /// function's own doc comment explains: glyphon has no per-glyph depth
    /// this crate's shared shape cache can use, so a later, allow-overlap
    /// icon covering an earlier label's text is handled by shrinking that
    /// label's clip rectangle instead. That is exactly right for its
    /// original case -- a stop pin's own short number, sized to sit ON the
    /// pin -- but it does not know a symbol from a caller with a much
    /// bigger footprint: an `icon-text-fit` badge sized around several lines
    /// of text, floating ABOVE its point rather than sitting on it. A
    /// neighbouring stop's ordinary pin, close enough on a busy route to
    /// intersect that badge's box, would get treated as "in front of" it
    /// and slice most of the badge's own text away -- confirmed live, where
    /// a two-line "Stop details" callout lost everything past its first
    /// line to an unrelated pin one stop over. Mapbox has no such property
    /// because mapbox-gl-js has no such clipping at all; this field only
    /// opts a caller's own oversized badges back OUT of a gpui-map-specific
    /// fix for a gpui-map-specific limitation. Default `true`, so every
    /// existing caller -- the stop-pin number this exists for -- is
    /// unaffected.
    pub text_clip_behind_icons: bool,
    /// `minzoom`, applied to this ONE feature rather than a whole style
    /// layer: below this camera zoom the symbol is skipped entirely --
    /// no icon, no text, no collision box, not merely clipped or
    /// de-prioritised. `None` draws at every zoom, matching a Mapbox
    /// layer with no `minzoom` set.
    ///
    /// Real Mapbox layers gate labels this way (Liberty's own road names
    /// start at z12.2, place labels lower); the overlay had no per-feature
    /// equivalent because nothing that pushed features needed one, until a
    /// caller's own dense stop-detail callouts -- one per stop, every stop,
    /// of every ticked vehicle -- turned a whole-fleet zoomed-out view into
    /// hundreds of overlapping badges: unreadable, and the shaping and
    /// upload behind that many simultaneous multi-line labels every frame
    /// is real work a de-cluttered zoomed-out view has no reason to pay
    /// for. Evaluated against the live camera zoom in the paint loop, not
    /// at push time, so panning or zooming needs no re-push to take effect.
    pub min_zoom: Option<f32>,
    pub text_color: Rgba,
    pub text_size: f32,
    pub text_halo_color: Rgba,
    pub text_halo_width: f32,
}

/// Every default this module documents, in ONE place -- and the value
/// `from_properties` starts from.
///
/// HAND-WRITTEN, and it must stay that way. `#[derive(Default)]` would
/// give `0`, `false` and `None` for every field, which is emphatically not
/// what an absent property means here: absent means the documented default
/// -- a 3pt line, a 5pt dot, a 12pt label, a blue that can be seen -- so a
/// derived impl would hand a caller a paint that draws a zero-width
/// invisible nothing while the very same absent properties, pushed through
/// `set_features` as `{}`, drew a map.
///
/// The two doors cannot drift, structurally rather than merely by test:
/// this IS `from_properties`'s starting point, and a property the map does
/// not carry is a field `from_properties` never writes. Every default is
/// therefore stated exactly once, here.
/// `the_default_paint_is_exactly_what_an_empty_property_map_produces`
/// grades that it stayed so.
impl Default for FeaturePaint {
    fn default() -> Self {
        let named = |hex: &str| parse_color(hex).expect("built-in default colours are valid");
        Self {
            line_color: named(DEFAULT_BLUE),
            line_width: ZoomValue::constant(3.0),
            line_dash: DashPattern::none(),
            // The one default that is not simply a named colour: a fill
            // drawn at full opacity would bury whatever it covers, so the
            // DEFAULT fill is translucent. A caller who names a
            // `fill-color` gets exactly the alpha they wrote, which is why
            // this alpha belongs to the default rather than to the
            // property.
            fill_color: named(DEFAULT_BLUE).scaled_alpha(DEFAULT_FILL_ALPHA),
            circle_radius: 5.0,
            circle_color: named(DEFAULT_BLUE),
            icon_image: None,
            // White fill, no halo. Mapbox's own defaults are opaque BLACK
            // and no halo; white is chosen here for the same reason
            // `DEFAULT_HALO` is white for text -- an overlay draws over a
            // basemap of unknown darkness, and a caller who sets
            // `icon-halo-color` to their route colour without also setting
            // `icon-color` wants the bordered-white-badge shape, which is
            // what every transit map uses.
            icon_color: named(DEFAULT_HALO),
            icon_halo_color: named(DEFAULT_BLUE),
            // Zero, so a caller who names an SDF sprite and sets nothing
            // else gets the plain shape rather than a border they did not
            // ask for. Mapbox's default too.
            icon_halo_width: 0.0,
            icon_anchor: IconAnchor::default(),
            // Upright. Mapbox's default too.
            icon_rotate_deg: 0.0,
            icon_text_fit: TextFit::default(),
            icon_allow_overlap: false,
            text_allow_overlap: false,
            icon_text_fit_padding: [0.0; 4],
            text_field: None,
            text_offset: (0.0, 0.0),
            text_anchor: crate::style::TextAnchor::Center,
            text_justify: TextJustify::Center,
            text_clip_behind_icons: true,
            min_zoom: None,
            text_color: named(DEFAULT_TEXT),
            text_size: 12.0,
            text_halo_color: named(DEFAULT_HALO),
            text_halo_width: DEFAULT_HALO_WIDTH,
        }
    }
}

/// Writes a field only when the property map actually carried it.
///
/// This one `if let` is the rule the whole no-drift argument rests on:
/// absent leaves the field untouched, and untouched is `Default`'s value.
/// Nothing in `from_properties` may state a default of its own.
fn set<T>(slot: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *slot = value;
    }
}

impl FeaturePaint {
    /// Reads every supported key, leaving every absent one at its
    /// `Default` value.
    ///
    /// A key that is PRESENT but unusable fails the whole call. Falling
    /// back to a default there would draw the caller something they did not
    /// ask for and never tell them -- the failure would look exactly like a
    /// working map.
    ///
    /// A key this does not recognise is ignored. GeoJSON `properties` is
    /// where a caller keeps their own data (a vehicle id, a timestamp), and
    /// refusing it would make the format unusable.
    ///
    /// The keys are read in a FIXED order, the one they are written in
    /// below, because the first bad property is the one reported and a
    /// collection with two of them must not report a different one after
    /// an unrelated edit.
    pub fn from_properties(properties: &Map<String, Value>) -> Result<Self, String> {
        // The opacities are not fields; they scale a colour's alpha and
        // leave nothing of their own behind. Absent means 1.0, which is
        // the identity for that multiplication rather than a default value
        // anybody could read back -- so they are the one thing here that
        // legitimately names its own fallback.
        let line_opacity = opacity(properties, "line-opacity")?;
        let fill_opacity = opacity(properties, "fill-opacity")?;

        let mut paint = Self::default();
        set(&mut paint.fill_color, color(properties, "fill-color")?);
        set(&mut paint.line_color, color(properties, "line-color")?);
        set(&mut paint.line_width, zoom_number(properties, "line-width")?);
        set(&mut paint.line_dash, dash(properties)?);
        set(&mut paint.circle_radius, positive_number(properties, "circle-radius")?);
        set(&mut paint.circle_color, color(properties, "circle-color")?);
        // The outer `Option` is "was the key there"; the inner one is the
        // field's own, which is `None` for a feature with no icon.
        set(&mut paint.icon_image, text(properties, "icon-image")?.map(Some));
        set(&mut paint.icon_color, color(properties, "icon-color")?);
        set(&mut paint.icon_halo_color, color(properties, "icon-halo-color")?);
        set(&mut paint.icon_halo_width, positive_number(properties, "icon-halo-width")?);
        set(&mut paint.icon_anchor, icon_anchor(properties)?);
        set(&mut paint.icon_text_fit, icon_text_fit(properties)?);
        set(&mut paint.icon_allow_overlap, allow_overlap(properties, "icon")?);
        set(&mut paint.text_allow_overlap, allow_overlap(properties, "text")?);
        set(&mut paint.icon_text_fit_padding, padding(properties, "icon-text-fit-padding")?);
        set(&mut paint.text_field, text(properties, "text-field")?.map(Some));
        set(&mut paint.text_offset, text_offset(properties)?);
        set(&mut paint.text_anchor, text_anchor(properties)?);
        set(&mut paint.text_justify, text_justify(properties)?);
        set(
            &mut paint.text_clip_behind_icons,
            bool_property(properties, "text-clip-behind-icons")?,
        );
        set(&mut paint.min_zoom, positive_number(properties, "min-zoom")?.map(Some));
        set(&mut paint.text_color, color(properties, "text-color")?);
        set(&mut paint.text_size, positive_number(properties, "text-size")?);
        set(&mut paint.text_halo_color, color(properties, "text-halo-color")?);
        set(&mut paint.text_halo_width, positive_number(properties, "text-halo-width")?);

        // Applied to whatever ended up in the field, declared or default.
        // `scaled_alpha` MULTIPLIES, so a colour carrying its own alpha and
        // an opacity property compose -- and the default fill's own
        // translucency composes with `fill-opacity` exactly as it did when
        // the two were one expression.
        paint.line_color = paint.line_color.scaled_alpha(line_opacity);
        paint.fill_color = paint.fill_color.scaled_alpha(fill_opacity);
        Ok(paint)
    }

    /// Re-states the numeric rules `from_properties` enforces on its way
    /// in, for a paint that did not come through it.
    ///
    /// `MapView::set_features_from` takes a `FeaturePaint` already
    /// resolved, which is most of what makes it fast -- and which means
    /// every guard written into `positive_number`, `zoom_number` and
    /// `dash` above is guard the structured path never runs. A negative
    /// `line-width` fails a `set_features` call today; it must fail a
    /// `set_features_from` call too, or the same data would be accepted
    /// through one door and refused at the other.
    ///
    /// COLOURS AND OPACITIES ARE NOT RE-CHECKED, and cannot be: `opacity`
    /// is folded into the colour's alpha by `scaled_alpha` and leaves no
    /// separate value behind, and a colour that is here at all was built as
    /// an `Rgba` rather than parsed from a string. Those two guards are
    /// about READING a property, not about the value it produces. What is
    /// checked here is every rule that constrains a stored field.
    ///
    /// Keyed by STYLE PROPERTY NAME (`line-width`, not `line_width`), so a
    /// caller reading the error can look it up in the same place they found
    /// the property.
    pub fn validate(&self) -> Result<(), String> {
        // The whole ramp, not just the value at one zoom: `zoom_number`
        // checks every stop in its leaf parser precisely because a ramp
        // that starts positive can still end below zero.
        for (zoom, width) in &self.line_width.stops {
            non_negative("line-width", *width)
                .map_err(|error| format!("{error} (at zoom {zoom})"))?;
        }
        non_negative("circle-radius", self.circle_radius)?;
        non_negative("icon-halo-width", self.icon_halo_width)?;
        non_negative("text-size", self.text_size)?;
        non_negative("text-halo-width", self.text_halo_width)?;

        // Only the runs the pattern actually has. Everything past `count`
        // is padding the shader never reads, and refusing it would reject
        // patterns that draw correctly.
        for run in &self.line_dash.runs[..self.line_dash.count as usize] {
            if !run.is_finite() || *run < 0.0 {
                return Err(format!("line-dasharray: runs must be non-negative, got {run}"));
            }
        }

        // Negative is ordinary for both of these -- a label nudged up and
        // left -- so only finiteness is at stake, exactly as `padding` and
        // `text_offset` have it.
        finite("text-offset", self.text_offset.0)?;
        finite("text-offset", self.text_offset.1)?;
        for value in self.icon_text_fit_padding {
            finite("icon-text-fit-padding", value)?;
        }
        Ok(())
    }
}

/// The `positive_number` rule, stated against a value rather than against a
/// properties map. Same message, so the two paths report a bad width
/// identically.
fn non_negative(key: &str, value: f32) -> Result<(), String> {
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{key}: expected a non-negative number, got {value}"));
    }
    Ok(())
}

fn finite(key: &str, value: f32) -> Result<(), String> {
    if !value.is_finite() {
        return Err(format!("{key}: expected finite numbers, got {value}"));
    }
    Ok(())
}

trait ScaledAlpha {
    fn scaled_alpha(self, factor: f32) -> Self;
}

impl ScaledAlpha for Rgba {
    /// Multiplies rather than replaces, so a colour that declared its own
    /// alpha and an opacity property compose instead of one silently
    /// winning.
    fn scaled_alpha(mut self, factor: f32) -> Self {
        self.a *= factor;
        self
    }
}

/// `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()` or `rgba()`. Named CSS colours are deliberately not
/// supported: a table of 148 names is a lot of surface for a format whose
/// callers are generating their styling programmatically anyway.
pub fn parse_color(text: &str) -> Option<Rgba> {
    // CSS `rgb(r, g, b)` / `rgba(r, g, b, a)` too, through the style
    // compiler's own parser: what a host app's colour helpers write.
    if text.starts_with("rgb") {
        return crate::style_spec::color::parse_css_color(text).ok();
    }
    let hex = text.strip_prefix('#')?;
    let byte = |s: &str| u8::from_str_radix(s, 16).ok().map(|v| v as f32 / 255.0);
    let nibble = |c: char| c.to_digit(16).map(|v| (v * 17) as f32 / 255.0);

    match hex.len() {
        3 => {
            let mut chars = hex.chars();
            Some(Rgba {
                r: nibble(chars.next()?)?,
                g: nibble(chars.next()?)?,
                b: nibble(chars.next()?)?,
                a: 1.0,
            })
        }
        6 => Some(Rgba {
            r: byte(&hex[0..2])?,
            g: byte(&hex[2..4])?,
            b: byte(&hex[4..6])?,
            a: 1.0,
        }),
        8 => Some(Rgba {
            r: byte(&hex[0..2])?,
            g: byte(&hex[2..4])?,
            b: byte(&hex[4..6])?,
            a: byte(&hex[6..8])?,
        }),
        _ => None,
    }
}

/// `None` when the key is absent -- the caller keeps whatever `Default`
/// put in the field. Every reader below has the same contract.
fn color(properties: &Map<String, Value>, key: &str) -> Result<Option<Rgba>, String> {
    match properties.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => parse_color(text)
            .map(Some)
            .ok_or_else(|| format!("{key}: {text:?} is not a #rgb, #rrggbb or #rrggbbaa colour")),
        Some(other) => Err(format!("{key}: expected a colour string, got {other}")),
    }
}

/// `icon-anchor`. Mapbox's own default, `center`, lives in `Default`.
///
/// An unrecognised value is an ERROR, not a silent fall back to `center`:
/// a caller who writes `"icon-anchor": "botom"` gets a pin half a badge out
/// of place, which looks like a rendering bug rather than a typo.
fn icon_anchor(properties: &Map<String, Value>) -> Result<Option<IconAnchor>, String> {
    match properties.get("icon-anchor") {
        None => Ok(None),
        Some(Value::String(text)) => IconAnchor::parse(text).map(Some).ok_or_else(|| {
            format!(
                "icon-anchor: {text:?} is not one of center, left, right, top, bottom, \
                 top-left, top-right, bottom-left, bottom-right"
            )
        }),
        Some(other) => Err(format!("icon-anchor: expected a string, got {other}")),
    }
}

/// `text-offset` as Mapbox writes it: a two-element array in ems, `[x, y]`,
/// y positive DOWNWARD.
fn text_offset(properties: &Map<String, Value>) -> Result<Option<(f32, f32)>, String> {
    let Some(value) = properties.get("text-offset") else { return Ok(None) };
    let Some(pair) = value.as_array() else {
        return Err(format!("text-offset: expected a two-element array, got {value}"));
    };
    if pair.len() != 2 {
        return Err(format!("text-offset: expected exactly two elements, got {value}"));
    }
    let mut out = [0.0f32; 2];
    for (slot, element) in out.iter_mut().zip(pair) {
        let number = element
            .as_f64()
            .ok_or_else(|| format!("text-offset: expected numbers, got {value}"))?;
        if !number.is_finite() {
            return Err(format!("text-offset: expected finite numbers, got {value}"));
        }
        *slot = number as f32;
    }
    Ok(Some((out[0], out[1])))
}

/// True when `kind` ("icon" or "text") asks to bypass collision, through
/// either of the two Mapbox properties that mean it.
///
/// Either ONE is enough. They are separate properties in a style because
/// Mapbox can honour them separately; here they collapse to one decision
/// per kind, so demanding both would reject a caller who set only the one
/// that applies to what they are drawing.
///
/// `None` only when NEITHER key is present. Once one of them is, the
/// answer is theirs -- `"icon-allow-overlap": false` means false, not
/// "unset".
fn allow_overlap(properties: &Map<String, Value>, kind: &str) -> Result<Option<bool>, String> {
    let mut any = None;
    for key in [format!("{kind}-allow-overlap"), format!("{kind}-ignore-placement")] {
        let key = key.as_str();
        match properties.get(key) {
            None => {}
            Some(Value::Bool(value)) => any = Some(any.unwrap_or(false) | *value),
            Some(other) => return Err(format!("{key}: expected true or false, got {other}")),
        }
    }
    Ok(any)
}

/// A single plain boolean property, present or absent -- unlike
/// `allow_overlap`, which merges two Mapbox keys into one field.
fn bool_property(properties: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match properties.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(format!("{key}: expected true or false, got {other}")),
    }
}

/// `text-justify`. Mapbox's own default, `center`, lives in `Default`.
fn text_anchor(properties: &Map<String, Value>) -> Result<Option<crate::style::TextAnchor>, String> {
    match properties.get("text-anchor") {
        None => Ok(None),
        Some(Value::String(text)) => crate::style::TextAnchor::from_keyword(text).map(Some).ok_or_else(|| format!("text-anchor: {text:?} is not a text anchor")),
        Some(other) => Err(format!("text-anchor: expected a string, got {other}")),
    }
}

fn text_justify(properties: &Map<String, Value>) -> Result<Option<TextJustify>, String> {
    match properties.get("text-justify") {
        None => Ok(None),
        Some(Value::String(text)) => TextJustify::parse(text)
            .map(Some)
            .ok_or_else(|| format!("text-justify: {text:?} is not one of center, left, right")),
        Some(other) => Err(format!("text-justify: expected a string, got {other}")),
    }
}

/// `icon-text-fit`. Mapbox's own default, `none`, lives in `Default`.
pub(crate) fn icon_text_fit(properties: &Map<String, Value>) -> Result<Option<TextFit>, String> {
    match properties.get("icon-text-fit") {
        None => Ok(None),
        Some(Value::String(text)) => TextFit::parse(text)
            .map(Some)
            .ok_or_else(|| format!("icon-text-fit: {text:?} is not one of none, width, height, both")),
        Some(other) => Err(format!("icon-text-fit: expected a string, got {other}")),
    }
}

/// A four-element padding array, `[top, right, bottom, left]`. Mapbox also
/// accepts a single number meaning all four sides, which is accepted here
/// for the same reason: a caller writing one number means the obvious
/// thing.
pub(crate) fn padding(properties: &Map<String, Value>, key: &str) -> Result<Option<[f32; 4]>, String> {
    let Some(value) = properties.get(key) else { return Ok(None) };
    if let Some(number) = value.as_f64() {
        if !number.is_finite() {
            return Err(format!("{key}: expected a finite number, got {value}"));
        }
        return Ok(Some([number as f32; 4]));
    }
    let Some(items) = value.as_array() else {
        return Err(format!("{key}: expected a number or a four-element array, got {value}"));
    };
    if items.len() != 4 {
        return Err(format!("{key}: expected exactly four elements, got {value}"));
    }
    let mut out = [0.0f32; 4];
    for (slot, element) in out.iter_mut().zip(items) {
        let number = element
            .as_f64()
            .ok_or_else(|| format!("{key}: expected numbers, got {value}"))?;
        if !number.is_finite() {
            return Err(format!("{key}: expected finite numbers, got {value}"));
        }
        *slot = number as f32;
    }
    Ok(Some(out))
}

fn positive_number(properties: &Map<String, Value>, key: &str) -> Result<Option<f32>, String> {
    match properties.get(key) {
        None => Ok(None),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("{key}: expected a number, got {value}"))?;
            if !number.is_finite() || number < 0.0 {
                return Err(format!("{key}: expected a non-negative number, got {number}"));
            }
            Ok(Some(number as f32))
        }
    }
}

/// A non-negative number that may vary with zoom.
///
/// Absent leaves the field at its `Default`. Present goes through
/// `style_spec::expression::compile_zoom_value` -- the SAME compiler the
/// basemap style's own `line-width` uses, so `interpolate`, `step`, `case`
/// and `match` all mean here exactly what they mean there, and a bare
/// number still compiles to `ZoomValue::constant` and behaves as it always
/// did.
///
/// Two things that compiler tolerates are refused here.
///
/// A DATA-DRIVEN expression compiles to more than one `(Filter, ZoomValue)`
/// pair, or to a single pair whose filter is not `Filter::Any` -- one
/// branch per feature-property value, for a caller to select between using
/// the feature's tags. Overlay paint is already per-feature: a caller who
/// wants a different width per route writes a different number in THAT
/// route's `properties`, so there is no tag set to select against and
/// nothing for such a filter to do. Taking the first pair (the obvious
/// shortcut) would silently paint every feature the first branch's width,
/// which looks exactly like a working map. Refusing says so instead.
///
/// EVERY STOP is checked for non-negative and finite, not just the bare
/// literal `positive_number` used to be the whole of. `line-width: -2` was
/// always an error; `["interpolate", ["linear"], ["zoom"], 8, 2.4, 18,
/// -7.2]` must be one too, and the check has to live in the leaf parser to
/// see the stops at all.
fn zoom_number(
    properties: &Map<String, Value>,
    key: &str,
) -> Result<Option<ZoomValue<f32>>, String> {
    let Some(value) = properties.get(key) else { return Ok(None) };

    // The `Scope` parameter is `compile_zoom_value`'s lexical-scope
    // thread; an overlay property is never compiled inside a `let`, so
    // this leaf ignores it.
    let leaf = |leaf: &Value, _scope: crate::style_spec::expression::Scope<'_>| -> anyhow::Result<f32> {
        let number = leaf
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("expected a number, got {leaf}"))?;
        if !number.is_finite() || number < 0.0 {
            anyhow::bail!("expected a non-negative number, got {number}");
        }
        Ok(number as f32)
    };

    let compiled = crate::style_spec::expression::compile_zoom_value(value, leaf, crate::style_spec::expression::Scope::root())
        .map_err(|error| format!("{key}: {error}"))?;

    let mut compiled = compiled.into_iter();
    match (compiled.next(), compiled.next()) {
        (Some((Filter::Any, zoom_value)), None) => Ok(Some(zoom_value)),
        // `compile_zoom_value` never returns an empty vector -- every branch
        // pushes at least the default pair -- but a total match here beats an
        // `expect` that would panic on a caller's input if that ever changed.
        (None, _) => Err(format!("{key}: {value} compiled to no value at all")),
        _ => Err(format!(
            "{key}: {value} is a data-driven expression, which an overlay cannot use -- \
             overlay paint is already per-feature, so put the value this feature wants in \
             its own properties instead"
        )),
    }
}

fn opacity(properties: &Map<String, Value>, key: &str) -> Result<f32, String> {
    match properties.get(key) {
        None => Ok(1.0),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("{key}: expected a number, got {value}"))?;
            if !(0.0..=1.0).contains(&number) {
                return Err(format!("{key}: expected a value in 0..=1, got {number}"));
            }
            Ok(number as f32)
        }
    }
}

fn dash(properties: &Map<String, Value>) -> Result<Option<DashPattern>, String> {
    let Some(value) = properties.get("line-dasharray") else { return Ok(None) };
    let array = value
        .as_array()
        .ok_or_else(|| format!("line-dasharray: expected an array, got {value}"))?;
    let mut runs = Vec::with_capacity(array.len());
    for entry in array {
        let run = entry
            .as_f64()
            .ok_or_else(|| format!("line-dasharray: expected numbers, got {entry}"))?;
        if !run.is_finite() || run < 0.0 {
            return Err(format!("line-dasharray: runs must be non-negative, got {run}"));
        }
        runs.push(run as f32);
    }
    // `[0, gap]`: a zero-length dash every `gap` widths. The overlay has
    // no `line-cap` to ask with, and a zero-length butt dash draws
    // nothing, so the only useful reading is MapLibre's round-cap one:
    // round dots (`DashPattern::round_dots`). `from_runs` would collapse
    // the dash and draw the line solid.
    if let [dash, gap] = runs[..] {
        if dash == 0.0 && gap > 0.0 {
            return Ok(Some(DashPattern::round_dots(gap)));
        }
    }
    Ok(Some(DashPattern::from_runs(&runs)))
}

fn text(properties: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match properties.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!("{key}: expected a string, got {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn properties(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    /// THE invariant. `Default` and an empty property map are two doors
    /// into the same struct, and they must arrive at the same value.
    ///
    /// A DERIVED `Default` would fail this and is the reason the impl is
    /// written by hand: `#[derive(Default)]` gives zeros -- a
    /// zero-width line, a 0pt label, an invisible black-on-black
    /// everything -- while an absent property means the documented
    /// default (`line-width: 3`, `text-size: 12`, a blue that can be
    /// seen). Two doors disagreeing about what "I said nothing" means is
    /// the drift every other equivalence test in this crate exists to
    /// prevent.
    #[test]
    fn the_default_paint_is_exactly_what_an_empty_property_map_produces() {
        assert_eq!(
            FeaturePaint::default(),
            FeaturePaint::from_properties(&serde_json::Map::new()).unwrap(),
        );
    }

    /// And it is not the derived one. Asserted positively, against the
    /// values the module documents, so that a future edit which quietly
    /// swapped the hand-written impl for `#[derive(Default)]` would fail
    /// HERE with a readable diff rather than only through the invariant
    /// above -- which a matching change to `from_properties` could keep
    /// green while both doors walked off together.
    #[test]
    fn the_default_paint_holds_the_documented_defaults_not_zeroes() {
        let paint = FeaturePaint::default();
        assert_eq!(paint.line_width.value_at(0), 3.0, "line-width");
        assert_eq!(paint.circle_radius, 5.0, "circle-radius");
        assert_eq!(paint.text_size, 12.0, "text-size");
        assert_eq!(paint.text_halo_width, DEFAULT_HALO_WIDTH, "text-halo-width");
        assert_eq!(paint.line_color, parse_color(DEFAULT_BLUE).unwrap(), "line-color");
        assert_eq!(paint.circle_color, parse_color(DEFAULT_BLUE).unwrap(), "circle-color");
        assert_eq!(paint.text_color, parse_color(DEFAULT_TEXT).unwrap(), "text-color");
        assert_eq!(paint.icon_color, parse_color(DEFAULT_HALO).unwrap(), "icon-color");
        assert_eq!(paint.icon_halo_color, parse_color(DEFAULT_BLUE).unwrap(), "icon-halo-color");
        assert_eq!(paint.text_halo_color, parse_color(DEFAULT_HALO).unwrap(), "text-halo-color");
        assert_eq!(paint.fill_color.a, DEFAULT_FILL_ALPHA, "fill-color carries its own alpha");
    }

    #[test]
    fn icon_rotate_deg_defaults_to_zero_and_is_carried_verbatim() {
        let paint = FeaturePaint::default();
        assert_eq!(paint.icon_rotate_deg, 0.0, "an unset icon-rotate must draw upright");
        let turned = FeaturePaint { icon_rotate_deg: 90.0, ..FeaturePaint::default() };
        assert_eq!(
            turned.icon_rotate_deg,
            90.0,
            "the paint stores DEGREES as given -- no radian conversion here, no normalisation; \
             the conversion belongs at the draw site, matching \
             `style::SymbolFeature::icon_rotate_deg`"
        );
    }

    /// A default paint is a LEGAL paint -- `replace_from` runs
    /// `validate` on every feature it is given, and a default that could
    /// not pass it would make `..Default::default()` useless at the one
    /// place it is meant to be written.
    #[test]
    fn the_default_paint_passes_validate() {
        FeaturePaint::default().validate().unwrap();
    }

    #[test]
    fn six_digit_hex_parses_to_opaque_colour() {
        let colour = parse_color("#2b7fff").unwrap();
        assert!((colour.r - 43.0 / 255.0).abs() < 1e-6);
        assert!((colour.g - 127.0 / 255.0).abs() < 1e-6);
        assert!((colour.b - 255.0 / 255.0).abs() < 1e-6);
        assert_eq!(colour.a, 1.0);
    }

    #[test]
    fn three_digit_hex_expands_each_nibble() {
        assert_eq!(parse_color("#f00"), parse_color("#ff0000"));
    }

    #[test]
    fn eight_digit_hex_carries_alpha() {
        assert_eq!(parse_color("#00000080").unwrap().a, 128.0 / 255.0);
    }

    #[test]
    fn a_colour_that_is_not_hex_is_none() {
        assert!(parse_color("rebeccapurple").is_none());
        assert!(parse_color("#12345").is_none());
        assert!(parse_color("#gggggg").is_none());
    }

    #[test]
    fn empty_properties_take_every_default() {
        let paint = FeaturePaint::from_properties(&properties("{}")).unwrap();
        assert_eq!(paint.line_width.value_at(12), 3.0);
        assert_eq!(paint.line_color, parse_color("#3b82f6").unwrap());
        assert_eq!(paint.circle_radius, 5.0);
        assert_eq!(paint.text_size, 12.0);
        assert!(paint.text_field.is_none());
        assert!(paint.icon_image.is_none());
        assert_eq!(paint.line_dash, crate::style::DashPattern::none());
    }

    #[test]
    fn declared_values_override_the_defaults() {
        let paint = FeaturePaint::from_properties(&properties(
            "{\"line-color\":\"#ff0000\",\"line-width\":8,\"text-field\":\"42\",\"icon-image\":\"pin\"}",
        ))
        .unwrap();
        assert_eq!(paint.line_color, parse_color("#ff0000").unwrap());
        assert_eq!(paint.line_width.value_at(12), 8.0);
        assert_eq!(paint.text_field.as_deref(), Some("42"));
        assert_eq!(paint.icon_image.as_deref(), Some("pin"));
    }

    /// Opacity multiplies the colour's own alpha rather than replacing it,
    /// so `#00000080` at `line-opacity` 0.5 ends up at a quarter.
    #[test]
    fn opacity_multiplies_the_colours_own_alpha() {
        let paint = FeaturePaint::from_properties(&properties(
            "{\"line-color\":\"#00000080\",\"line-opacity\":0.5}",
        ))
        .unwrap();
        assert!((paint.line_color.a - (128.0 / 255.0) * 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_dasharray_becomes_a_dash_pattern() {
        let paint =
            FeaturePaint::from_properties(&properties("{\"line-dasharray\":[2,1]}")).unwrap();
        assert_eq!(paint.line_dash, crate::style::DashPattern::from_runs(&[2.0, 1.0]));
    }

    #[test]
    fn a_zero_length_dash_is_round_dots() {
        let paint = FeaturePaint::from_properties(&properties("{\"line-dasharray\":[0,2.5]}")).unwrap();
        assert_eq!(paint.line_dash, crate::style::DashPattern::round_dots(2.5));
        assert!(paint.line_dash.is_round_dots());
        assert_eq!(paint.line_dash.period(), 2.5);
        assert!(!crate::style::DashPattern::from_runs(&[2.0, 1.0]).is_round_dots());
    }

    /// A property present but unusable fails the call. Drawing a default
    /// instead would show the caller something they did not ask for and
    /// never tell them.
    #[test]
    fn a_malformed_colour_is_an_error_not_a_default() {
        let error = FeaturePaint::from_properties(&properties("{\"line-color\":\"nope\"}"))
            .unwrap_err();
        assert!(error.contains("line-color"), "error was {error}");
    }

    #[test]
    fn a_negative_width_is_an_error() {
        let error =
            FeaturePaint::from_properties(&properties("{\"line-width\":-2}")).unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
    }

    #[test]
    fn a_non_numeric_width_is_an_error() {
        assert!(FeaturePaint::from_properties(&properties("{\"line-width\":\"wide\"}")).is_err());
    }

    /// A bare number keeps behaving exactly as it did before `line-width`
    /// became a `ZoomValue`: one width, at every zoom, with both brackets of
    /// `value_at_pair` equal so the shader's `mix` stays a no-op.
    #[test]
    fn a_bare_number_width_is_the_same_at_every_zoom() {
        let paint = FeaturePaint::from_properties(&properties("{\"line-width\":8}")).unwrap();
        assert_eq!(paint.line_width.value_at(0), 8.0);
        assert_eq!(paint.line_width.value_at(12), 8.0);
        assert_eq!(paint.line_width.value_at(22), 8.0);
        assert_eq!(paint.line_width.value_at_pair(12), (8.0, 8.0));
    }

    /// The caller's production mapbox-gl style, verbatim. Against it our
    /// constant 3.0px line was 50% too thick at z9 (2.88 wanted) and 31% too
    /// thin at z16 (6.24 wanted) -- the measurement this whole change exists
    /// to fix.
    #[test]
    fn an_interpolate_expression_gives_different_widths_at_two_zooms() {
        let paint = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}"#,
        ))
        .unwrap();
        let at = |zoom| paint.line_width.value_at(zoom);
        assert!((at(8) - 2.4).abs() < 1e-5, "z8 was {}", at(8));
        assert!((at(9) - 2.88).abs() < 1e-4, "z9 was {}", at(9));
        assert!((at(16) - 6.24).abs() < 1e-4, "z16 was {}", at(16));
        assert!((at(18) - 7.2).abs() < 1e-5, "z18 was {}", at(18));
        assert!(at(9) < at(16), "the ramp must actually ramp");
    }

    /// Outside the declared stops an `interpolate` clamps rather than
    /// extrapolating, exactly as the style spec says -- so a route stays
    /// visible at z2 instead of ramping down to a hairline.
    #[test]
    fn an_interpolate_expression_clamps_outside_its_stops() {
        let paint = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}"#,
        ))
        .unwrap();
        assert!((paint.line_width.value_at(2) - 2.4).abs() < 1e-5);
        assert!((paint.line_width.value_at(22) - 7.2).abs() < 1e-5);
    }

    /// The non-negative rule has to survive the move to expressions, and it
    /// has to reach EVERY stop -- not just the bare-literal case it used to
    /// be the whole of.
    #[test]
    fn a_negative_interpolation_stop_is_rejected() {
        let error = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,-7.2]}"#,
        ))
        .unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
        assert!(error.contains("non-negative"), "error was {error}");
    }

    #[test]
    fn a_non_numeric_interpolation_stop_is_rejected() {
        let error = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["interpolate",["linear"],["zoom"],8,"wide",18,7.2]}"#,
        ))
        .unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
    }

    /// Overlay paint is already per-feature: a caller who wants a different
    /// width for a different route puts a different number in that feature's
    /// own `properties`. A data-driven expression therefore has nothing to
    /// drive from, and taking its first branch silently would draw every
    /// feature the motorway width.
    #[test]
    fn a_data_driven_width_is_rejected_rather_than_silently_taking_one_branch() {
        let error = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["match",["get","class"],"motorway",8,3]}"#,
        ))
        .unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
        assert!(error.contains("per-feature"), "error was {error}");
    }

    /// A `step` expression is zoom-only and has no interpolator, so both
    /// brackets stay equal and the width snaps at the threshold instead of
    /// ramping -- MapLibre's own behaviour for `step`.
    #[test]
    fn a_step_expression_snaps_rather_than_ramping() {
        let paint = FeaturePaint::from_properties(&properties(
            r#"{"line-width":["step",["zoom"],2.0,14,6.0]}"#,
        ))
        .unwrap();
        assert_eq!(paint.line_width.value_at(13), 2.0);
        assert_eq!(paint.line_width.value_at(14), 6.0);
        assert_eq!(paint.line_width.value_at_pair(13), (2.0, 2.0));
    }

    #[test]
    fn an_out_of_range_opacity_is_an_error() {
        assert!(FeaturePaint::from_properties(&properties("{\"fill-opacity\":1.5}")).is_err());
    }

    /// An unknown property is ignored, not refused: GeoJSON `properties` is
    /// where a caller keeps their OWN data, and rejecting a vehicle id
    /// would make the format unusable.
    /// The halo defaults to white and ONE pixel, not to Mapbox's zero.
    /// An overlay label is drawn over a basemap nobody authored it against,
    /// so a halo-less default is unreadable the first time a label lands on
    /// a coloured road.
    #[test]
    fn the_text_halo_defaults_to_a_single_white_pixel() {
        let paint = FeaturePaint::from_properties(&properties("{}")).unwrap();
        assert_eq!(paint.text_halo_width, 1.0);
        assert_eq!(paint.text_halo_color, parse_color("#ffffff").unwrap());
    }

    #[test]
    fn a_declared_halo_overrides_the_default() {
        let paint = FeaturePaint::from_properties(&properties(
            r##"{"text-halo-color":"#000000","text-halo-width":2.5}"##,
        ))
        .unwrap();
        assert_eq!(paint.text_halo_color, parse_color("#000000").unwrap());
        assert_eq!(paint.text_halo_width, 2.5);
    }

    /// Zero is how a caller asks for Mapbox's own behaviour, and must be
    /// accepted rather than treated as absent.
    #[test]
    fn a_halo_width_of_zero_is_honoured_not_replaced_by_the_default() {
        let paint =
            FeaturePaint::from_properties(&properties(r#"{"text-halo-width":0}"#)).unwrap();
        assert_eq!(paint.text_halo_width, 0.0);
    }

    #[test]
    fn a_malformed_halo_colour_is_an_error() {
        assert!(FeaturePaint::from_properties(&properties(r#"{"text-halo-color":"nope"}"#)).is_err());
    }

    #[test]
    fn a_negative_halo_width_is_an_error() {
        assert!(FeaturePaint::from_properties(&properties(r#"{"text-halo-width":-1}"#)).is_err());
    }

    #[test]
    fn an_unrecognised_property_is_ignored() {
        let paint = FeaturePaint::from_properties(&properties(
            "{\"vehicle_id\":\"SBA1234X\",\"line-width\":4}",
        ))
        .unwrap();
        assert_eq!(paint.line_width.value_at(12), 4.0);
    }

    #[test]
    fn default_fill_color_has_alpha_0_3() {
        let paint = FeaturePaint::from_properties(&properties("{}")).unwrap();
        assert_eq!(paint.fill_color, parse_color("#3b82f6").unwrap().scaled_alpha(0.3));
        assert!((paint.fill_color.a - 0.3).abs() < 1e-6);
    }

    #[test]
    fn default_circle_and_text_colors_are_opaque() {
        let paint = FeaturePaint::from_properties(&properties("{}")).unwrap();
        assert_eq!(paint.circle_color, parse_color("#3b82f6").unwrap());
        assert_eq!(paint.circle_color.a, 1.0);
        assert_eq!(paint.text_color, parse_color("#1a1a1a").unwrap());
        assert_eq!(paint.text_color.a, 1.0);
    }

    /// Explicit fill colour reaches full opacity with fill-opacity: 1.0.
    /// This regression test guards against applying default alpha to all fills.
    #[test]
    fn explicit_fill_color_reaches_full_opacity() {
        let paint = FeaturePaint::from_properties(&properties(
            "{\"fill-color\":\"#ff0000\",\"fill-opacity\":1.0}",
        ))
        .unwrap();
        assert_eq!(paint.fill_color.a, 1.0);
    }

    #[test]
    fn fill_opacity_multiplies_explicit_fill_colors_alpha() {
        let paint = FeaturePaint::from_properties(&properties(
            "{\"fill-color\":\"#ff000080\",\"fill-opacity\":0.5}",
        ))
        .unwrap();
        assert!((paint.fill_color.a - (128.0 / 255.0) * 0.5).abs() < 1e-6);
    }
}

#[cfg(test)]
mod validate_tests {
    use super::tests::properties;
    use super::*;

    fn valid() -> FeaturePaint {
        FeaturePaint::from_properties(&serde_json::Map::new()).unwrap()
    }

    /// The paint `from_properties` produces from an empty properties object
    /// is every default this crate ships, so if the defaults themselves did
    /// not pass, no structured call could ever succeed.
    #[test]
    fn the_default_paint_is_valid() {
        valid().validate().unwrap();
    }

    /// The rule the spec names first, because it is the one the string path
    /// visibly enforces: `line-width: -3` fails the whole call rather than
    /// silently drawing something nobody asked for.
    #[test]
    fn a_negative_line_width_is_rejected() {
        let mut paint = valid();
        paint.line_width = crate::style::ZoomValue::constant(-3.0);
        let error = paint.validate().unwrap_err();
        assert!(error.contains("line-width"), "error was {error}");
        assert!(error.contains("non-negative"), "error was {error}");
    }

    /// EVERY STOP, not just the first -- the same rule `zoom_number`'s leaf
    /// parser applies, and the reason it lives in the leaf there.
    #[test]
    fn a_negative_line_width_stop_is_rejected() {
        let mut paint = valid();
        paint.line_width = crate::style::ZoomValue::interpolated(vec![(8, 2.4), (18, -7.2)]);
        assert!(paint.validate().is_err());
    }

    #[test]
    fn a_non_finite_line_width_is_rejected() {
        let mut paint = valid();
        paint.line_width = crate::style::ZoomValue::constant(f32::NAN);
        assert!(paint.validate().is_err());
    }

    #[test]
    fn a_negative_circle_radius_is_rejected() {
        let mut paint = valid();
        paint.circle_radius = -1.0;
        let error = paint.validate().unwrap_err();
        assert!(error.contains("circle-radius"), "error was {error}");
    }

    /// The other three non-negative numbers `positive_number` guards, each
    /// reachable only through this path now that a caller can write the
    /// field directly.
    #[test]
    fn the_other_non_negative_numbers_are_rejected_too() {
        for (name, set) in [
            ("icon-halo-width", (|p: &mut FeaturePaint| p.icon_halo_width = -1.0) as fn(&mut FeaturePaint)),
            ("text-size", |p: &mut FeaturePaint| p.text_size = -12.0),
            ("text-halo-width", |p: &mut FeaturePaint| p.text_halo_width = -1.0),
        ] {
            let mut paint = valid();
            set(&mut paint);
            let error = paint.validate().unwrap_err();
            assert!(error.contains(name), "expected {name} in {error}");
        }
    }

    /// `dash()` rejects a negative run rather than degrading it, and a
    /// `DashPattern` reached by writing `runs` directly bypasses
    /// `from_runs`'s own guard.
    #[test]
    fn a_negative_dash_run_is_rejected() {
        let mut paint = valid();
        paint.line_dash = DashPattern { runs: [2.0, -2.0, 0.0, 0.0], count: 2 };
        let error = paint.validate().unwrap_err();
        assert!(error.contains("line-dasharray"), "error was {error}");
    }

    /// Runs past `count` are not part of the pattern and are never read by
    /// the shader, so they are not the caller's problem.
    #[test]
    fn a_dash_run_past_the_count_is_not_inspected() {
        let mut paint = valid();
        paint.line_dash = DashPattern { runs: [2.0, 2.0, -9.0, 0.0], count: 2 };
        paint.validate().unwrap();
    }

    /// `padding` and `text_offset` permit negatives -- a label nudged up
    /// and left is ordinary -- but require finite numbers, because a NaN
    /// offset propagates into a vertex position and takes the whole
    /// billboard off screen.
    #[test]
    fn a_non_finite_offset_or_padding_is_rejected() {
        let mut paint = valid();
        paint.text_offset = (0.0, f32::INFINITY);
        assert!(paint.validate().unwrap_err().contains("text-offset"));

        let mut paint = valid();
        paint.icon_text_fit_padding = [0.0, f32::NAN, 0.0, 0.0];
        assert!(paint.validate().unwrap_err().contains("icon-text-fit-padding"));

        let mut paint = valid();
        paint.text_offset = (-0.5, -1.5);
        paint.icon_text_fit_padding = [-1.0; 4];
        paint.validate().expect("negative offsets and padding are ordinary");
    }

    /// Every paint the string path can produce passes, which is the
    /// property that matters: `validate` re-states `from_properties`'s
    /// rules, so it must never refuse something `from_properties` accepted.
    #[test]
    fn every_paint_from_properties_passes_validation() {
        for json in [
            r#"{}"#,
            r##"{"line-color":"#ff0000","line-width":6,"circle-radius":0}"##,
            r#"{"line-width":["interpolate",["linear"],["zoom"],8,2.4,18,7.2]}"#,
            r#"{"line-dasharray":[2,2,6,2]}"#,
            r#"{"text-offset":[-0.5,-1.5],"icon-text-fit-padding":[-1,-2,-3,-4]}"#,
            r#"{"text-size":0,"text-halo-width":0,"icon-halo-width":0}"#,
        ] {
            let paint = FeaturePaint::from_properties(&properties(json)).unwrap();
            paint.validate().unwrap_or_else(|error| panic!("{json} -> {error}"));
        }
    }
}
