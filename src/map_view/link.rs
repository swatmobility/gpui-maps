//! Two (or more) maps that move as one: [`CameraLink`], and the one memory
//! budget they share, [`CacheBudget`]. The primitives a host needs for a
//! compare view (Kepler.gl's split map, mapbox-gl-compare's swipe); the
//! divider and the layout are the host's.
//!
//! # How the cameras stay locked without a loop
//!
//! The link holds one canonical camera, a generation, and a CLAIMANT: the
//! member that last took input (a drag, a wheel, a pinch, or a host's own
//! `jump_to`/`set_camera`/`ease_to`). Only the claimant publishes; every
//! other member follows. Each member keeps the camera it last synced at
//! (`seen`) and the generation it saw. Once a frame:
//!
//! - **the claimant moved** (its gesture, or the glide or ease after it):
//!   it publishes its camera;
//! - **a follower sees a newer publish**: it adopts it (a jump, which also
//!   cancels any glide or ease of its own);
//! - **a follower moved on its own** (a glide left over from before another
//!   member took input): it yields to the link's camera, so two views can
//!   never take turns overwriting each other.
//!
//! A member that adopted records the camera it now HAS as `seen`, so the
//! adoption never reads as a move of its own: nothing echoes back. A
//! clamped zoom (a narrower zoom range on one side) is recorded the same
//! way and does not bounce either. Before anyone has claimed, the first
//! member to sync seeds the link and the others adopt.
//!
//! # Same frame on both sides
//!
//! gpui prepaints every element of a window before it paints any. A linked
//! map advances its camera (coalesced scroll zoom, eases, glide) and
//! publishes in PREPAINT, and adopts in PAINT, so whichever side the
//! pointer is over, both sides draw the same camera in the same frame. An
//! unlinked map does all of that in `paint`, as before.
//!
//! # Anchors: mirror or swipe
//!
//! Each member has an anchor: where its centre sits in the host's shared
//! frame (logical px). Equal anchors (the default) MIRROR the camera --
//! Kepler's split map, the same ground on both halves. Anchors at each
//! half's own centre (`divider / 2` and `divider + right_w / 2`) make the
//! two halves one continuous map across the divider (a swipe compare): a
//! follower's centre is the ground under the leader's pixel at the anchor
//! offset. Exact at pitch 0 (any bearing); under pitch the two halves have
//! their own vanishing points and meet only along the horizon row of the
//! centre.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::render::Camera;

/// The pose a link carries: what a member last published.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Published {
    from: u64,
    camera: Camera,
    anchor: (f32, f32),
}

#[derive(Debug, Default)]
struct LinkState {
    generation: u64,
    current: Option<Published>,
    next_id: u64,
    /// The member whose moves are published (`LinkMember::claim`).
    claimant: Option<u64>,
    publishes: u64,
    adoptions: u64,
}

/// Cameras that move together. Cheap to clone (a shared handle); every
/// [`crate::map_view::MapView`] joined to it with
/// [`crate::map_view::MapView::join_camera_link`] follows the others, and
/// gestures on any of them drive all of them.
#[derive(Debug, Clone, Default)]
pub struct CameraLink(Rc<RefCell<LinkState>>);

impl CameraLink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times a member published a move (a view that led a frame).
    #[must_use]
    pub fn publishes(&self) -> u64 {
        self.0.borrow().publishes
    }

    /// How many times a member adopted another's camera.
    #[must_use]
    pub fn adoptions(&self) -> u64 {
        self.0.borrow().adoptions
    }

    /// The member id that published last, if any.
    #[must_use]
    pub fn leader(&self) -> Option<u64> {
        self.0.borrow().current.map(|p| p.from)
    }

    fn register(&self) -> u64 {
        let mut s = self.0.borrow_mut();
        s.next_id += 1;
        s.next_id
    }

    fn same(&self, other: &CameraLink) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// One map's membership of a link.
#[derive(Debug, Clone)]
pub(crate) struct LinkMember {
    link: CameraLink,
    pub(crate) id: u64,
    anchor: (f32, f32),
    seen_generation: u64,
    seen: Option<Camera>,
}

/// What a member does this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LinkStep {
    Nothing,
    Published,
    Adopt { center: (f64, f64), zoom: f64, pitch: f32, bearing: f32 },
}

fn pose_eq(a: &Camera, b: &Camera) -> bool {
    a.center_lon == b.center_lon && a.center_lat == b.center_lat && a.zoom == b.zoom && a.pitch == b.pitch && a.bearing == b.bearing
}

impl LinkMember {
    pub(crate) fn join(link: &CameraLink) -> Self {
        Self { link: link.clone(), id: link.register(), anchor: (0.0, 0.0), seen_generation: 0, seen: None }
    }

    pub(crate) fn is(&self, link: &CameraLink) -> bool {
        self.link.same(link)
    }

    /// A new anchor re-derives this member -- the leader included -- from
    /// the link's pose: the shared frame holds still and the member's view
    /// of it moves (a swipe divider dragged leaves the ground where it is).
    pub(crate) fn set_anchor(&mut self, anchor: (f32, f32)) {
        if self.anchor != anchor {
            self.anchor = anchor;
            self.seen_generation = 0;
        }
    }

    /// Takes the lead: this member's moves are published from now on, and
    /// the others follow (input reached this map, or the host moved it).
    pub(crate) fn claim(&mut self) {
        self.link.0.borrow_mut().claimant = Some(self.id);
    }

    /// One sync against `camera` (this map's camera now). `prepaint`:
    /// publish a move, adopt nothing yet (every member's prepaint runs
    /// before any paint, so the paint half adopts what this frame
    /// published).
    pub(crate) fn step(&mut self, camera: &Camera, prepaint: bool) -> LinkStep {
        let mut s = self.link.0.borrow_mut();
        let may_lead = s.claimant.is_none_or(|c| c == self.id);
        // Someone else's publish, or my own at an anchor I no longer have.
        let newer = s.generation > self.seen_generation && s.current.is_some_and(|p| p.from != self.id || p.anchor != self.anchor);
        let moved = self.seen.is_none_or(|seen| !pose_eq(&seen, camera));
        let stale_anchor = s.current.is_some_and(|p| p.from == self.id && p.anchor != self.anchor);
        if s.current.is_none() || (may_lead && moved && !stale_anchor && !(s.claimant.is_none() && newer)) {
            s.generation += 1;
            s.publishes += 1;
            s.current = Some(Published { from: self.id, camera: *camera, anchor: self.anchor });
            self.seen_generation = s.generation;
            self.seen = Some(*camera);
            return LinkStep::Published;
        }
        if (newer || moved) && !prepaint {
            let p = s.current.expect("checked");
            s.adoptions += 1;
            self.seen_generation = s.generation;
            let (center, zoom, pitch, bearing) = follow(&p, self.anchor);
            return LinkStep::Adopt { center, zoom, pitch, bearing };
        }
        LinkStep::Nothing
    }

    /// After an adoption: the camera the map now has (clamps included).
    pub(crate) fn adopted(&mut self, camera: &Camera) {
        self.seen = Some(*camera);
        // The leader re-derived at a new anchor: the link's pose is now
        // stated at that anchor (the same ground, so the others' adoptions
        // of either agree), and its next move publishes as usual.
        let mut s = self.link.0.borrow_mut();
        if let Some(p) = s.current.as_mut().filter(|p| p.from == self.id && p.anchor != self.anchor) {
            p.camera = *camera;
            p.anchor = self.anchor;
        }
    }
}

/// The follower's pose for `p` at anchor `anchor`: the leader's, with the
/// centre moved to the ground under the leader's pixel at the anchor
/// offset (none when the anchors agree).
fn follow(p: &Published, anchor: (f32, f32)) -> ((f64, f64), f64, f32, f32) {
    let c = p.camera;
    let (dx, dy) = (anchor.0 - p.anchor.0, anchor.1 - p.anchor.1);
    let mut center = (c.center_lon, c.center_lat);
    if dx != 0.0 || dy != 0.0 {
        let px = c.viewport_width_px * 0.5 + dx;
        let py = c.viewport_height_px * 0.5 + dy;
        if let Some(w) = crate::render::unproject_to_ground_f64(&c, px, py) {
            center = world_to_lon_lat(w.x, w.y);
        }
    }
    (center, c.zoom, c.pitch, c.bearing)
}

/// Mercator world space (`overlay::geom::lon_lat_to_world`'s `[0, 512)`
/// square) back to lon/lat.
fn world_to_lon_lat(x: f64, y: f64) -> (f64, f64) {
    let size = f64::from(crate::render::WORLD_SIZE);
    let lon = x / size * 360.0 - 180.0;
    let n = std::f64::consts::PI * (1.0 - 2.0 * y / size);
    (lon, n.sinh().atan().to_degrees())
}

/// One memory budget for several maps: each member's tile caches (CPU and
/// GPU) get `1 / members` of what one map alone would, so two linked views
/// hold what one would rather than double it. Cheap to clone; a map holds
/// a share from [`crate::map_view::MapView::share_cache_budget`] until it
/// is dropped or leaves.
#[derive(Debug, Clone, Default)]
pub struct CacheBudget(Arc<AtomicUsize>);

impl CacheBudget {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps holding a share now.
    #[must_use]
    pub fn members(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn share(&self) -> BudgetShare {
        self.0.fetch_add(1, Ordering::SeqCst);
        BudgetShare(Arc::clone(&self.0))
    }
}

/// A map's share of a [`CacheBudget`]; gives itself back on drop.
#[derive(Debug)]
pub(crate) struct BudgetShare(Arc<AtomicUsize>);

impl BudgetShare {
    pub(crate) fn members(&self) -> usize {
        self.0.load(Ordering::SeqCst).max(1)
    }

    pub(crate) fn is(&self, budget: &CacheBudget) -> bool {
        Arc::ptr_eq(&self.0, &budget.0)
    }
}

impl Drop for BudgetShare {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(lon: f64, lat: f64, zoom: f64) -> Camera {
        Camera { center_lon: lon, center_lat: lat, zoom, pitch: 0.0, bearing: 0.0, viewport_width_px: 600.0, viewport_height_px: 400.0, fov_y: Camera::DEFAULT_FOV_Y }
    }

    /// A move on either side reaches the other once, and adopting never
    /// reads as a move: no echo, however many idle frames follow.
    #[test]
    fn a_move_reaches_the_other_member_once_and_never_echoes() {
        let link = CameraLink::new();
        let (mut a, mut b) = (LinkMember::join(&link), LinkMember::join(&link));
        let mut ca = cam(-118.4, 34.1, 9.4);
        let mut cb = cam(0.0, 0.0, 2.0);
        assert_eq!(a.step(&ca, false), LinkStep::Published, "the first member seeds the link");
        let apply = |m: &mut LinkMember, c: &mut Camera, step: LinkStep| {
            if let LinkStep::Adopt { center, zoom, pitch, bearing } = step {
                c.center_lon = center.0;
                c.center_lat = center.1;
                c.zoom = zoom;
                c.pitch = pitch;
                c.bearing = bearing;
                m.adopted(c);
            }
        };
        let s = b.step(&cb, false);
        apply(&mut b, &mut cb, s);
        assert!(pose_eq(&ca, &cb));
        for _ in 0..10 {
            assert_eq!(a.step(&ca, false), LinkStep::Nothing);
            assert_eq!(b.step(&cb, false), LinkStep::Nothing);
        }
        // A drag on B: B claims and publishes in prepaint; A adopts in paint.
        b.claim();
        cb.center_lon += 0.01;
        assert_eq!(a.step(&ca, true), LinkStep::Nothing);
        assert_eq!(b.step(&cb, true), LinkStep::Published);
        let s = a.step(&ca, false);
        apply(&mut a, &mut ca, s);
        assert_eq!(b.step(&cb, false), LinkStep::Nothing);
        assert!(pose_eq(&ca, &cb));
        assert_eq!((link.publishes(), link.adoptions()), (2, 2));
        for _ in 0..10 {
            assert_eq!(a.step(&ca, false), LinkStep::Nothing);
            assert_eq!(b.step(&cb, false), LinkStep::Nothing);
        }
    }

    /// A glide left on A when input reaches B: B claims, B's moves are
    /// published, and A's glide yields to them -- no tug of war.
    #[test]
    fn the_member_with_input_leads_and_a_leftover_glide_yields() {
        let link = CameraLink::new();
        let (mut a, mut b) = (LinkMember::join(&link), LinkMember::join(&link));
        let ca = cam(10.0, 10.0, 5.0);
        a.claim();
        a.step(&ca, false);
        assert!(matches!(b.step(&ca, false), LinkStep::Adopt { .. }));
        b.adopted(&ca);
        // B takes input; A is still gliding east.
        b.claim();
        let ga = cam(10.1, 10.0, 5.0);
        let db = cam(9.9, 10.0, 5.0);
        assert_eq!(a.step(&ga, true), LinkStep::Nothing, "A no longer leads");
        assert_eq!(b.step(&db, true), LinkStep::Published);
        let s = a.step(&ga, false);
        assert!(matches!(s, LinkStep::Adopt { center, .. } if center == (9.9, 10.0)), "{s:?}");
        a.adopted(&db);
        assert_eq!(b.step(&db, false), LinkStep::Nothing);
        assert_eq!(link.leader(), Some(b.id));
    }

    /// Swipe anchors: at pitch 0 the follower's centre is the ground the
    /// leader shows at the anchor offset -- the halves continue each other.
    #[test]
    fn offset_anchors_continue_the_map_across_a_divider() {
        let link = CameraLink::new();
        let (mut a, mut b) = (LinkMember::join(&link), LinkMember::join(&link));
        a.set_anchor((300.0, 200.0));
        b.set_anchor((900.0, 200.0));
        let mut ca = cam(0.0, 0.0, 10.0);
        ca.bearing = 0.0;
        a.step(&ca, false);
        let LinkStep::Adopt { center, zoom, .. } = b.step(&cam(0.0, 0.0, 1.0), false) else { panic!("adopts") };
        assert_eq!(zoom, 10.0);
        // 600 px at z10 = 600 / 2^10 world units of 512 = 600/1024/512 of
        // the world's 360 degrees.
        let want = 600.0 / 1024.0 / 512.0 * 360.0;
        assert!((center.0.abs() - want).abs() < 1e-6 && center.1.abs() < 1e-6, "{center:?} vs {want}");
        // And mirrored anchors copy the pose exactly.
        let link = CameraLink::new();
        let (mut a, mut b) = (LinkMember::join(&link), LinkMember::join(&link));
        a.step(&cam(-80.0, 40.4, 8.7), false);
        let LinkStep::Adopt { center, .. } = b.step(&cam(0.0, 0.0, 1.0), false) else { panic!() };
        assert_eq!(center, (-80.0, 40.4));
    }

    /// A swipe divider dragged: the anchors move and every member,
    /// leader included, re-derives from the one pose, so the ground under
    /// the shared frame does not move.
    #[test]
    fn moving_the_anchors_keeps_the_shared_frame_still() {
        let link = CameraLink::new();
        let (mut a, mut b) = (LinkMember::join(&link), LinkMember::join(&link));
        a.set_anchor((300.0, 200.0));
        b.set_anchor((900.0, 200.0));
        let ca = cam(0.0, 0.0, 10.0);
        a.claim();
        a.step(&ca, false);
        let LinkStep::Adopt { center: b0, .. } = b.step(&cam(0.0, 0.0, 1.0), false) else { panic!() };
        // The divider moves right by 200 px: left half 800 wide, right 400.
        a.set_anchor((400.0, 200.0));
        b.set_anchor((1000.0, 200.0));
        let mut wide = ca;
        wide.viewport_width_px = 800.0;
        assert_eq!(a.step(&wide, true), LinkStep::Nothing, "the leader does not publish at a stale anchor");
        let LinkStep::Adopt { center: a1, .. } = a.step(&wide, false) else { panic!("the leader re-derives") };
        let mut a1cam = wide;
        a1cam.center_lon = a1.0;
        a.adopted(&a1cam);
        let LinkStep::Adopt { center: b1, .. } = b.step(&cam(0.0, 0.0, 1.0), false) else { panic!("the follower re-derives") };
        // And the leader's next move publishes again.
        let mut moved = a1cam;
        moved.zoom = 10.5;
        assert_eq!(a.step(&moved, true), LinkStep::Published);
        let px = 360.0 / 1024.0 / 512.0;
        assert!((a1.0.abs() - 100.0 * px).abs() < 1e-6, "the left centre moved 100 px of ground: {a1:?}");
        assert!((b1.0.abs() - b0.0.abs() - 100.0 * px).abs() < 1e-6, "{b0:?} -> {b1:?}");
    }

    #[test]
    fn a_budget_counts_its_live_shares() {
        let b = CacheBudget::new();
        let one = b.share();
        let two = b.share();
        assert_eq!((b.members(), one.members()), (2, 2));
        drop(two);
        assert_eq!(one.members(), 1);
        assert!(one.is(&b) && !one.is(&CacheBudget::new()));
    }
}
