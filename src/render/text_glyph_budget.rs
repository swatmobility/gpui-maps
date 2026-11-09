//! A per-frame allowance of NEW glyph rasters for the glyphon-drawn labels.
//!
//! # Why
//!
//! `glyphon::TextRenderer::prepare` rasterises, on the frame thread and
//! inside the one call, every glyph of every text area that its atlas does
//! not hold yet, and uploads each with its own `write_texture`. Measured on
//! the census zoom sweep (2026-09-26, DC): at a zoom-bucket crossing every
//! point label comes back at a new size, which is 200-390 glyph images the
//! atlas has never seen -- 1.2-1.6 ms of raster plus 1.0-4.6 ms of upload
//! and eviction in a single `prepare`. That was the whole of the 3.5-7 ms
//! `text_prepare` spikes; a `prepare` with nothing new to raster costs about
//! 0.07 us per glyph.
//!
//! glyphon exposes no way to ask whether a glyph is cached, so this keeps
//! its own record of every glyph key a successful `prepare` was handed. A
//! label whose keys are all in that record costs nothing new; a label with
//! unseen keys is admitted while the frame's allowance lasts and deferred to
//! a later frame otherwise -- the same shape as the 2 ms new-label allowance
//! `symbol_collect` already has for shaping and SDF rasters.
//!
//! # What the record can get wrong, and why that is safe
//!
//! glyphon evicts least-recently-used glyphs once its atlas is full. A key
//! evicted behind this record's back is re-rasterised inside `prepare`
//! without being charged: a missed saving, never a missing label. The atlas
//! is pre-grown (see `symbol_pass::grow_text_atlas`) so that eviction needs
//! far more distinct glyphs than a map shows, and the record is cleared when
//! it outgrows [`KNOWN_CAP`], after which labels are simply re-charged.
//!
//! Deferral never changes WHICH labels are placed -- collision already ran
//! -- only the frame the text first draws on.

use std::collections::HashSet;
use std::hash::Hash;

/// New glyph images one frame may hand to `prepare`.
///
/// One new glyph cost 8-10 us measured (swash raster ~4 us, `write_texture`
/// ~4-5 us), so 64 is about 0.6 ms: under the 1.5 ms `text_prepare` target
/// with room for the steady per-glyph walk of a dense z12 frame (~1 ms at
/// 15-20 k glyphs). A bucket crossing's ~390 new glyphs then land over six
/// or seven frames, which the label fade-in hides.
pub(crate) const NEW_GLYPHS_PER_FRAME: usize = 64;

/// Above this many remembered keys the record is dropped and rebuilt. Well
/// under what a pre-grown 2048x2048 mask atlas holds (~14 k label-sized
/// glyphs), so the record stays a subset of what glyphon really has.
pub(crate) const KNOWN_CAP: usize = 6_000;

/// The record of glyph keys handed to a successful `prepare`.
pub(crate) struct TextGlyphBudget<K> {
    known: HashSet<K>,
    /// Keys admitted this frame, not yet confirmed by a `prepare`.
    pending: Vec<K>,
}

impl<K: Hash + Eq + Copy> Default for TextGlyphBudget<K> {
    fn default() -> Self {
        Self { known: HashSet::new(), pending: Vec::new() }
    }
}

impl<K: Hash + Eq + Copy> TextGlyphBudget<K> {
    /// Keeps, in order, every item whose glyphs are all known and every
    /// item with new glyphs while `allowance` new glyphs remain. The first
    /// item with new glyphs is always admitted, so a label larger than the
    /// allowance still lands. An item `keys_of` has nothing for (not
    /// shaped, or chrome) is kept and not charged. Returns how many items
    /// were deferred.
    pub(crate) fn admit<'k, T>(
        &mut self,
        items: &mut Vec<T>,
        allowance: usize,
        mut keys_of: impl FnMut(&T) -> Option<&'k [K]>,
    ) -> usize
    where
        K: 'k,
    {
        self.pending.clear();
        let mut spent = 0usize;
        let mut admitted_new = false;
        let known = &self.known;
        let pending = &mut self.pending;
        let before = items.len();
        items.retain(|item| {
            let Some(keys) = keys_of(item) else { return true };
            let fresh: Vec<K> = keys
                .iter()
                .copied()
                .filter(|key| !known.contains(key) && !pending.contains(key))
                .collect();
            if fresh.is_empty() {
                return true;
            }
            if admitted_new && spent + fresh.len() > allowance {
                return false;
            }
            admitted_new = true;
            spent += fresh.len();
            pending.extend(fresh);
            true
        });
        before - items.len()
    }

    /// `prepare` succeeded: what this frame admitted is now in the atlas.
    pub(crate) fn confirm(&mut self) {
        if self.known.len() + self.pending.len() > KNOWN_CAP {
            self.known.clear();
        }
        self.known.extend(self.pending.drain(..));
    }

    /// `prepare` failed: nothing admitted this frame is known to be cached.
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
    }

    #[cfg(test)]
    pub(crate) fn known_len(&self) -> usize {
        self.known.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The glyph keys of labels 0, 1, 2, ...; a frame's draw list is label
    /// indices, as `symbol_text` holds keys into the shape cache.
    struct Shapes(Vec<Vec<u32>>);

    impl Shapes {
        fn new(spec: &[&[u32]]) -> Self {
            Self(spec.iter().map(|keys| keys.to_vec()).collect())
        }
        fn frame(&self) -> Vec<usize> {
            (0..self.0.len()).collect()
        }
        fn admit(&self, budget: &mut TextGlyphBudget<u32>, items: &mut Vec<usize>, allowance: usize) -> usize {
            budget.admit(items, allowance, |index| Some(self.0[*index].as_slice()))
        }
    }

    #[test]
    fn known_glyphs_are_free_and_new_ones_are_charged_up_to_the_allowance() {
        let shapes = Shapes::new(&[&[1, 2, 3], &[4, 5], &[6, 7, 8]]);
        let mut budget = TextGlyphBudget::<u32>::default();
        let mut items = shapes.frame();
        assert_eq!(shapes.admit(&mut budget, &mut items, 5), 1, "3 + 2 fit in 5, the third label waits");
        assert_eq!(items, vec![0, 1]);
        budget.confirm();
        assert_eq!(budget.known_len(), 5);

        // Next frame: the first two are free, the third is charged.
        let mut items = shapes.frame();
        assert_eq!(shapes.admit(&mut budget, &mut items, 5), 0);
        assert_eq!(items, vec![0, 1, 2]);
    }

    #[test]
    fn the_first_new_label_is_admitted_even_over_the_allowance() {
        let shapes = Shapes::new(&[&[1, 2, 3, 4, 5, 6], &[7]]);
        let mut budget = TextGlyphBudget::<u32>::default();
        let mut items = shapes.frame();
        assert_eq!(shapes.admit(&mut budget, &mut items, 2), 1);
        assert_eq!(items, vec![0]);
    }

    #[test]
    fn a_glyph_shared_by_two_new_labels_is_charged_once() {
        let shapes = Shapes::new(&[&[1, 2], &[2, 3]]);
        let mut budget = TextGlyphBudget::<u32>::default();
        let mut items = shapes.frame();
        assert_eq!(shapes.admit(&mut budget, &mut items, 3), 0);
    }

    #[test]
    fn a_failed_prepare_teaches_nothing() {
        let shapes = Shapes::new(&[&[1, 2]]);
        let mut budget = TextGlyphBudget::<u32>::default();
        let mut items = shapes.frame();
        shapes.admit(&mut budget, &mut items, 8);
        budget.discard();
        budget.confirm();
        assert_eq!(budget.known_len(), 0);
    }

    #[test]
    fn a_zero_allowance_still_lands_one_label_a_frame_until_all_are_in() {
        let shapes = Shapes::new(&[&[1], &[2], &[3]]);
        let mut budget = TextGlyphBudget::<u32>::default();
        for frame in 1..=3 {
            let mut items = shapes.frame();
            shapes.admit(&mut budget, &mut items, 0);
            budget.confirm();
            assert_eq!(items.len(), frame);
        }
    }
}
