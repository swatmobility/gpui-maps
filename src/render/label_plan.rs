//! What one frame's placed symbols become: three draw-ready instance lists,
//! in draw order -- the orchestration around `symbol_pass`'s label
//! placement, extracted the way `frame_plan::space_plan` extracted the space
//! pass's decision.
//!
//! `symbol_pass::collect_candidates` already decides WHICH line labels place
//! and WHERE (it is shared with the browser build today -- see that
//! module's doc comment). What was still duplicated per shell was the step
//! AFTER placement: merge collision-placed symbols with the ones that
//! bypassed collision, sort into style-layer order, clip a label behind a
//! later icon, and split the survivors into the three lists a frame's
//! symbol-overlay pass actually draws from.
//!
//! `src/map_view.rs` had that step written out inline. `web/src/app.rs` had
//! a SHORTER copy of it -- shorter because it never collected the third
//! list at all. `collect_candidates` was called with `None` for
//! `symbol_pass::GlyphSink`, so every line-placed street name was measured,
//! placed along its road and given a collision box, and then drawn nowhere:
//! `LineLabelPlacement::quads` came back empty on the no-atlas path (see
//! `symbol_pass::line_glyph_quads`'s doc comment), and the browser had no
//! code path that would have collected a non-empty one if it existed. That
//! is the defect this module exists to make impossible to reintroduce
//! silently: a shell that stops calling [`label_draw_plan`] and goes back to
//! partitioning `DrawnSymbol`s itself can once again drop one of the three
//! lists without anything failing to compile.
//!
//! Deliberately free of `gpui`, `wgpu`, DOM and UIKit -- like `frame_plan`,
//! this is `Vec<SymbolCandidate>` in, three plain `Vec`s out. Uploading the
//! glyph atlas texture and drawing the batches stays in each shell, the same
//! way `SpaceDraw`'s GPU handles stay in each shell below `SpacePlan`: those
//! steps hold device/queue/encoder handles that are not portable data.

use crate::collision::{CollisionBox, CollisionIndex};
use crate::render::IconInstance;
use crate::symbol_pass::{
    clip_labels_behind_later_icons, place_visible_symbols, sort_symbols_for_drawing, DrawnSymbol,
    LabelDraw, SymbolCandidate,
};

/// The three lists a frame's symbol-overlay pass draws from, already in the
/// order native has always drawn them: icons first, then line-placed glyph
/// quads (its own atlas, its own batch), with `symbol_text` handed to
/// glyphon separately by the caller. See `map_view.rs`'s
/// `symbol-overlay-pass` for why icons and glyphs are two batches of ONE
/// pipeline call rather than two passes.
pub(crate) struct LabelDrawPlan {
    /// Every surviving symbol's sprite-atlas icon quads, in draw order. One
    /// quad for an ordinary icon; an SDF one contributes two (halo then
    /// fill) at draw time, so this is a count of instances, not triangles.
    pub(crate) icon_instances: Vec<IconInstance>,
    /// Every surviving point-and-viewport label, paired with the clipped
    /// collision box `clip_labels_behind_later_icons` computed for it (or
    /// `None` if nothing clipped it) -- glyphon's own draw list.
    pub(crate) symbol_text: Vec<(LabelDraw, Option<CollisionBox>)>,
    /// Every surviving line-placed label's rotated per-glyph quads,
    /// UNNORMALISED (`uv_min`/`uv_max` are still in atlas texels -- see
    /// `symbol_pass::normalise_glyph_uvs`, which the caller runs once the
    /// glyph atlas has stopped growing this frame). Empty when the caller
    /// gave `collect_candidates` no `GlyphSink`, or when every line-placed
    /// label present drew nothing (an all-space run, or a full atlas) --
    /// see `symbol_pass::LineLabelPlacement`'s own doc comment for why an
    /// empty list here is not the same claim as "no line label placed".
    pub(crate) glyph_instances: Vec<IconInstance>,
    /// How many SYMBOLS survived collision placement (icon-only ones
    /// included), before the split above -- `LabelCensus::placed`'s number,
    /// carried out because `drawn.len()` stops being answerable once this
    /// function has consumed `drawn` into the three lists above.
    ///
    /// `map_view.rs` has no `LabelCensus` on its live paint path (that
    /// struct is `web/src/app.rs`'s own instrument, and the test-only
    /// `symbol_pass::label_census`'s), so this field's only reader is the
    /// browser build -- `#[allow(dead_code)]` rather than a `cfg`, because
    /// dropping the field for native would make the two crates disagree
    /// about this type's shape for no reason tied to platform capability.
    #[allow(dead_code)]
    pub(crate) symbols_placed: usize,
    /// The sort key of every placed OVERLAY symbol (layer index
    /// [`OVERLAY_LAYER_INDEX`]), bypassed ones included. `map_view.rs`
    /// gives each overlay source a contiguous run of sort keys, so this is
    /// what attributes placed labels and icons back to their source
    /// (`MapView::source_draw_stats`). Empty -- and unallocated -- on a
    /// frame with no overlay symbols; the shells draw none.
    #[allow(dead_code)]
    pub(crate) overlay_sort_keys: Vec<f32>,
}

/// The collision layer index every overlay symbol is placed at: after
/// every basemap layer, so the caller's own data wins a contested
/// position. See `map_view.rs`'s `OVERLAY_COLLISION_LAYER_INDEX`.
pub(crate) const OVERLAY_LAYER_INDEX: usize = usize::MAX;

/// Turns `collect_candidates`' output into the three lists above.
///
/// `candidates` is what `collect_candidates` filled; `bypassed` is the
/// `overlay-allow-overlap` symbols it routed around collision entirely.
/// `index` is the caller's collision index, already `reset` for this frame
/// (see `place_visible_symbols`'s own precondition) -- retained across
/// frames on the caller's own state, not owned here.
///
/// This is exactly the sequence `map_view.rs`'s `paint` ran inline before
/// this module existed: place, merge bypassed in BEFORE sorting (an
/// overlap-allowed symbol still has a layer index and a sort key, and
/// appending it after sorting would draw every one of them above every
/// placed symbol regardless of layer), sort into style-layer order, clip
/// labels behind later icons, then split.
pub(crate) fn label_draw_plan(
    candidates: Vec<SymbolCandidate>,
    bypassed: Vec<DrawnSymbol>,
    index: &mut CollisionIndex,
) -> LabelDrawPlan {
    let mut drawn = place_visible_symbols(candidates, index);
    drawn.extend(bypassed);
    sort_symbols_for_drawing(&mut drawn);
    clip_labels_behind_later_icons(&mut drawn);
    let symbols_placed = drawn.len();
    let overlay_sort_keys: Vec<f32> =
        drawn.iter().filter(|symbol| symbol.order.0 == OVERLAY_LAYER_INDEX).map(|symbol| symbol.order.1).collect();

    let mut icon_instances = Vec::new();
    let mut symbol_text = Vec::new();
    let mut glyph_instances = Vec::new();
    for symbol in drawn {
        icon_instances.extend(symbol.icons);
        if let Some(text) = symbol.text {
            symbol_text.push((text, symbol.text_clip));
        }
        if let Some(run) = symbol.glyphs {
            glyph_instances.extend(run.quads);
        }
    }
    LabelDrawPlan { icon_instances, symbol_text, glyph_instances, symbols_placed, overlay_sort_keys }
}
