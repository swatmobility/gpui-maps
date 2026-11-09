//! The one brush state machine every 1-D selection in the Studio uses: the
//! linked chart's histogram range (`linked_chart.rs`) and the time slider's
//! window (`time_filter.rs`). d3-brush's behaviour, as kepler.gl's range
//! and time filters have it:
//!
//! - **Press inside the selection**: a drag moves the whole window, its
//!   width fixed, clamped to the domain.
//! - **Press within [`EDGE_PX`] of an edge**: a drag moves that edge only
//!   (the cursor is `ew-resize` there); dragged past the other edge, the
//!   two swap.
//! - **Press outside**: a drag draws a new selection from the press.
//! - **A click (no drag)**: outside, the host's own click (a histogram
//!   selects the bin); inside or on an edge, nothing changes.
//!
//! Pure: positions are numbers in whatever space the host measures (a
//! fraction of the plot's width, or the time axis's own values), the edge
//! tolerance given in that space. The host converts the pointer, writes
//! the window it gets back (a uniform write, no re-push) and snaps on
//! release.
//!
//! Declared once per binary (`#[path = ".../common/brush.rs"] mod brush;`)
//! beside the hosts, which `use crate::brush`.
#![allow(dead_code)]

/// How near an edge, in logical pixels, a press grabs that edge.
pub const EDGE_PX: f32 = 6.0;

/// Which edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Low,
    High,
}

/// What a pointer at a position is over: decides the cursor and the grab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// No selection under it (or none at all).
    Outside,
    /// Inside the selection, clear of both edges: a drag moves it.
    Inside,
    /// Within the tolerance of an edge: a drag resizes from it.
    Edge(Side),
}

/// What a held press is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Grab {
    /// Drawing a new selection from `anchor`.
    New { anchor: f64 },
    /// Moving the window, held `offset` after its low edge.
    Move { offset: f64, width: f64 },
    /// Resizing: `fixed` is the edge that stays.
    Edge { fixed: f64 },
}

/// A held press: what it grabbed, where, and whether it has moved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drag {
    pub grab: Grab,
    pub start: f64,
    pub moved: bool,
}

/// How a release ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// The pointer moved: the window is what the last [`drag`] gave.
    Dragged,
    /// A click outside the selection: the host's own click.
    ClickOutside,
    /// A click inside or on an edge: nothing changes.
    ClickInside,
}

/// What `v` is over, with `window` the selection (sorted, or `None`) and
/// `tol` the edge tolerance in `v`'s space.
pub fn hit(window: Option<[f64; 2]>, v: f64, tol: f64) -> Hit {
    let Some([lo, hi]) = window else { return Hit::Outside };
    let (dl, dh) = ((v - lo).abs(), (v - hi).abs());
    if dl <= tol || dh <= tol {
        // The nearer edge (a narrow window: both are near).
        return Hit::Edge(if dl <= dh { Side::Low } else { Side::High });
    }
    if v > lo && v < hi {
        Hit::Inside
    } else {
        Hit::Outside
    }
}

/// A press at `v`.
pub fn press(window: Option<[f64; 2]>, v: f64, tol: f64) -> Drag {
    let grab = match (hit(window, v, tol), window) {
        (Hit::Inside, Some([lo, hi])) => Grab::Move { offset: v - lo, width: hi - lo },
        (Hit::Edge(Side::Low), Some([_, hi])) => Grab::Edge { fixed: hi },
        (Hit::Edge(Side::High), Some([lo, _])) => Grab::Edge { fixed: lo },
        _ => Grab::New { anchor: v },
    };
    Drag { grab, start: v, moved: false }
}

/// The pointer at `v` with the press held: the window now, sorted and
/// inside `domain`. `slop` is how far (in `v`'s space) the pointer must go
/// before a press counts as a drag.
pub fn drag(d: &mut Drag, v: f64, domain: [f64; 2], slop: f64) -> Option<[f64; 2]> {
    if !d.moved && (v - d.start).abs() < slop {
        return None;
    }
    d.moved = true;
    let [d0, d1] = domain;
    let v = v.clamp(d0, d1);
    Some(match d.grab {
        Grab::New { anchor } => [anchor.min(v), anchor.max(v)],
        Grab::Move { offset, width } => {
            let lo = (v - offset).clamp(d0, (d1 - width).max(d0));
            [lo, lo + width]
        }
        Grab::Edge { fixed } => [fixed.min(v), fixed.max(v)],
    })
}

/// The press let go.
pub fn release(d: Drag) -> Release {
    match (d.moved, d.grab) {
        (true, _) => Release::Dragged,
        (false, Grab::New { .. }) => Release::ClickOutside,
        (false, _) => Release::ClickInside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOMAIN: [f64; 2] = [0.0, 1.0];

    /// A press inside drags the whole window: the width is kept and the
    /// window stops at the domain's ends.
    #[test]
    fn a_move_keeps_the_width_and_clamps_to_the_domain() {
        let w = Some([0.4, 0.6]);
        let mut d = press(w, 0.5, 0.01);
        assert!(matches!(d.grab, Grab::Move { .. }));
        let got = drag(&mut d, 0.6, DOMAIN, 0.0).expect("moved");
        assert!((got[0] - 0.5).abs() < 1e-12 && (got[1] - 0.7).abs() < 1e-12, "{got:?}");
        let got = drag(&mut d, 0.99, DOMAIN, 0.0).expect("moved");
        assert!((got[1] - 1.0).abs() < 1e-12 && ((got[1] - got[0]) - 0.2).abs() < 1e-12, "clamped right, width kept: {got:?}");
        let got = drag(&mut d, -3.0, DOMAIN, 0.0).expect("moved");
        assert!(got[0].abs() < 1e-12 && ((got[1] - got[0]) - 0.2).abs() < 1e-12, "clamped left: {got:?}");
        assert_eq!(release(d), Release::Dragged);
    }

    /// A press on an edge moves that edge only.
    #[test]
    fn an_edge_drag_resizes_from_that_edge() {
        let w = Some([0.4, 0.6]);
        assert_eq!(hit(w, 0.605, 0.01), Hit::Edge(Side::High));
        let mut d = press(w, 0.605, 0.01);
        assert_eq!(drag(&mut d, 0.8, DOMAIN, 0.0), Some([0.4, 0.8]));
        let mut d = press(w, 0.395, 0.01);
        assert_eq!(drag(&mut d, 0.2, DOMAIN, 0.0), Some([0.2, 0.6]));
    }

    /// Dragged past the other edge, the two swap.
    #[test]
    fn crossing_edges_swap() {
        let mut d = press(Some([0.4, 0.6]), 0.6, 0.01);
        assert_eq!(drag(&mut d, 0.3, DOMAIN, 0.0), Some([0.3, 0.4]));
    }

    /// A press outside draws a new selection.
    #[test]
    fn an_outside_drag_starts_a_new_selection() {
        let mut d = press(Some([0.4, 0.6]), 0.1, 0.01);
        assert_eq!(d.grab, Grab::New { anchor: 0.1 });
        assert_eq!(drag(&mut d, 0.25, DOMAIN, 0.0), Some([0.1, 0.25]));
        let mut d = press(None, 0.9, 0.01);
        assert_eq!(drag(&mut d, 0.7, DOMAIN, 0.0), Some([0.7, 0.9]), "leftward is the same range");
    }

    /// A click inside (or on an edge) changes nothing; one outside is the
    /// host's click. Under the slop a press is still a click.
    #[test]
    fn a_click_inside_changes_nothing() {
        let w = Some([0.4, 0.6]);
        let mut d = press(w, 0.5, 0.01);
        assert_eq!(drag(&mut d, 0.502, DOMAIN, 0.01), None, "under the slop");
        assert_eq!(release(d), Release::ClickInside);
        assert_eq!(release(press(w, 0.4, 0.01)), Release::ClickInside, "an edge");
        assert_eq!(release(press(w, 0.1, 0.01)), Release::ClickOutside);
    }
}
