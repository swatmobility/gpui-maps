use std::ffi::c_void;

use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CAMetalLayer, CATransaction};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Owns a `CAMetalLayer` holding the map, and the `wgpu::Surface` created
/// directly from it. gpui's own layer must never be reused (it's cleared
/// every frame by gpui's own renderer) — this layer is entirely separate, so
/// wgpu and gpui's Metal renderer never fight over one drawable.
///
/// WHERE the layer goes decides whether gpui can draw over the map, and the
/// two placements are genuinely different modes:
///
/// - **Sublayer of gpui's view layer** (the default). Core Animation
///   composites a layer's sublayers above its own contents, so the map covers
///   every gpui element sharing its bounds. Chrome over the map has to be
///   drawn in the wgpu pass.
/// - **Sibling below gpui's view layer** (`chrome_above`). gpui's own
///   rendering then composites over the map, so chrome is ordinary gpui.
///   Requires a transparent window background as well, or gpui's opaque
///   backdrop hides the map — `MapLayer::chrome_above_active` reports whether
///   the placement actually took, so the caller only makes that window-wide
///   change when it will help.
pub struct MapLayer {
    layer: Retained<CAMetalLayer>,
    /// The layer the map sits in, framed to the part of the map element
    /// that is VISIBLE: the element's bounds clipped by gpui's content mask
    /// (a scroll container, an `overflow_hidden` card). `masksToBounds`, so
    /// a map scrolled half out of its container shows only the half inside
    /// it -- gpui's own clipping never reaches a `CAMetalLayer`. The map
    /// layer itself keeps the element's full size inside it, offset.
    clip: Retained<CALayer>,
    /// What the host asked through [`MapLayer::set_hidden`]; the clip is
    /// also hidden while nothing of the map is visible.
    hidden_by_host: std::cell::Cell<bool>,
    /// The layer the map was inserted into, when it went BELOW gpui's -- the
    /// only thing that can paint behind the map. See
    /// [`MapLayer::set_backdrop_color`]. `None` for the default placement,
    /// where nothing is behind the map that a caller could see.
    backdrop: Option<Retained<CALayer>>,
    surface: wgpu::Surface<'static>,
    chrome_above_active: bool,
}

/// Inserts `layer` as a sibling below `gpui_layer`, reporting whether it
/// worked.
///
/// Two things have to be true, and neither is by default. gpui's view layer
/// is the root of its own layer tree (`superlayer()` is `None`), so the
/// parent has to come from the VIEW hierarchy instead; and that superview is
/// not layer-backed until asked, so `.layer()` is `None` until
/// `setWantsLayer(true)` has been called on it.
///
/// Returns `false` rather than failing when the hierarchy is not what we
/// expect: falling back to the default placement costs the caller some
/// chrome, while a hard error costs them the whole map.
fn insert_below_gpui(
    layer: &CALayer,
    gpui_layer: &CALayer,
    ns_view: &NSView,
) -> Option<Retained<CALayer>> {
    let Some(superview) = (unsafe { ns_view.superview() }) else {
        eprintln!("gpui-maps: gpui's view has no superview; chrome-above-map is unavailable");
        return None;
    };
    if superview.layer().is_none() {
        superview.setWantsLayer(true);
    }
    let Some(parent) = superview.layer() else {
        eprintln!(
            "gpui-maps: gpui's superview would not become layer-backed; \
             chrome-above-map is unavailable"
        );
        return None;
    };
    parent.insertSublayer_below(layer, Some(gpui_layer));
    // Returned rather than dropped: it is the surface behind the map, and a
    // caller who has just been told that gpui draws OVER the map needs to be
    // able to say what shows through where neither of them draws.
    Some(parent)
}

impl MapLayer {
    /// Must be called on the main thread, from inside a paint callback (so
    /// `window`'s handle is valid).
    ///
    /// `chrome_above` asks for the map to be placed below gpui's layer so
    /// gpui elements can draw over it; check `chrome_above_active` for
    /// whether that succeeded.
    pub fn new(
        window: &gpui::Window,
        instance: &wgpu::Instance,
        chrome_above: bool,
    ) -> anyhow::Result<Self> {
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|e| anyhow::anyhow!("no window handle: {e}"))?;
        let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
            anyhow::bail!("gpui-maps: expected an AppKit window handle on macOS");
        };

        // SAFETY: gpui documents this as a valid NSView pointer for the
        // lifetime of the window, and we are on the main thread (required by
        // AppKit for all view/layer manipulation).
        let ns_view: &NSView = unsafe { appkit_handle.ns_view.cast::<NSView>().as_ref() };
        let gpui_layer: Retained<CALayer> =
            ns_view.layer().ok_or_else(|| anyhow::anyhow!("gpui view is not layer-backed"))?;

        let layer = CAMetalLayer::new();
        let clip = CALayer::new();
        clip.setMasksToBounds(true);
        clip.addSublayer(&layer);
        let backdrop =
            chrome_above.then(|| insert_below_gpui(&clip, &gpui_layer, ns_view)).flatten();
        let chrome_above_active = backdrop.is_some();
        if !chrome_above_active {
            gpui_layer.addSublayer(&clip);
        }
        layer.setContentsScale(window.scale_factor() as f64);

        // SAFETY: `layer` is a valid, freshly-created CAMetalLayer that we
        // keep alive for at least as long as `surface` (wgpu also retains it
        // internally, per its own documented behavior for this variant).
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                Retained::as_ptr(&layer) as *mut c_void,
            ))
        }?;

        Ok(Self { layer, clip, hidden_by_host: std::cell::Cell::new(false), backdrop, surface, chrome_above_active })
    }

    /// Paints `(r, g, b, a)` -- sRGB, 0..1 -- behind the map, and therefore
    /// behind everything gpui draws in this window.
    ///
    /// # Why a caller needs this
    ///
    /// With the map below gpui's layer, gpui draws OVER the map -- which is the
    /// point -- and the consequence is that nothing gpui draws can be *behind*
    /// it. Two things then show whatever is under this layer tree, which is the
    /// window's own backing, i.e. black:
    ///
    /// - a rounded map's four corners, outside the arc but inside its bounds
    ///   (see [`MapLayer::set_corner_radius`]);
    /// - any pixel of the window the application does not paint, since a
    ///   chrome-above window's background is transparent.
    ///
    /// An application cannot fix either from its own side: both regions are
    /// under the map, and everything it draws is over it. So the colour goes on
    /// the layer the map was inserted into -- one flat fill behind the whole
    /// window, covered by the application's own painting wherever it paints.
    ///
    /// A no-op in the default placement: with the map above gpui's layer,
    /// nothing shows behind it.
    pub fn set_backdrop_color(&self, red: f32, green: f32, blue: f32, alpha: f32) {
        let Some(backdrop) = &self.backdrop else { return };
        let colour = NSColor::colorWithSRGBRed_green_blue_alpha(red as f64, green as f64, blue as f64, alpha as f64);
        backdrop.setBackgroundColor(Some(&colour.CGColor()));
    }

    /// Rounds the layer's own corners, in points.
    ///
    /// **An application cannot round this layer from the outside.** A caller
    /// draws the map inside a card and gives that card a radius, and gpui's
    /// clipping applies to what gpui paints -- which the map is not: it is a
    /// `CAMetalLayer` composited by Core Animation, so it stays a rectangle and
    /// its corners sit outside the card's border. `cornerRadius` with
    /// `masksToBounds` is where the radius has to go, and it is the same fix a
    /// webview-hosted map needs in CSS on its own canvas.
    ///
    /// `0` restores a square layer and clears the mask, since masking costs an
    /// offscreen composite the overwhelming majority of callers do not need.
    pub fn set_corner_radius(&self, radius: f64) {
        self.layer.setCornerRadius(radius);
        self.layer.setMasksToBounds(radius > 0.0);
    }

    /// Shows or hides the layer, leaving every GPU resource it owns in place.
    ///
    /// An application whose map is one pane of a larger window needs this: an
    /// element that is no longer rendered stops being PAINTED, but a
    /// `CAMetalLayer` is in the window's layer tree, not in gpui's element
    /// tree, so it keeps compositing its last frame -- a map that will not go
    /// away. `setHidden` is what takes it off screen without tearing down the
    /// surface, the device and every fetched tile, which is what dropping the
    /// `MapView` would cost to get the same picture.
    pub fn set_hidden(&self, hidden: bool) {
        self.hidden_by_host.set(hidden);
        self.clip.setHidden(hidden);
    }

    pub fn surface(&self) -> &wgpu::Surface<'static> {
        &self.surface
    }

    /// Whether the map really is below gpui's layer, so gpui elements draw
    /// over it. False whenever the placement was not asked for OR could not
    /// be made; the caller must not make the window transparent unless this
    /// is true, since a transparent window over a map that is still on top
    /// is strictly worse than leaving both alone.
    pub fn chrome_above_active(&self) -> bool {
        self.chrome_above_active
    }

    /// Repositions and rescales the layer to match `bounds` (the map
    /// element's bounds within the window, from `Element::paint`/`prepaint`).
    /// Call this every frame before presenting — cheap, and correctly handles
    /// window resize without any separate resize-observer plumbing.
    ///
    /// The map is clipped to gpui's current content mask (the scroll
    /// container and cards it is painted inside): the user's "map is moving
    /// when I scroll" -- a map in a scrolled dashboard drew over the page
    /// title and under the filters, outside its card, because a
    /// `CAMetalLayer` is not clipped by gpui. Nothing visible: hidden.
    pub fn sync_frame(&self, bounds: gpui::Bounds<gpui::Pixels>, window: &gpui::Window) {
        let scale = window.scale_factor() as f64;
        let viewport_height = f64::from(window.viewport_size().height);
        let mask = window.content_mask().bounds;
        let rect = |b: gpui::Bounds<gpui::Pixels>| {
            (f64::from(b.origin.x), f64::from(b.origin.y), f64::from(b.size.width), f64::from(b.size.height))
        };
        let frames = clipped_frames(rect(bounds), rect(mask), viewport_height);

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        match frames {
            Some(LayerFrames { clip, map }) => {
                self.clip.setFrame(CGRect::new(CGPoint::new(clip.0, clip.1), CGSize::new(clip.2, clip.3)));
                self.layer.setFrame(CGRect::new(CGPoint::new(map.0, map.1), CGSize::new(map.2, map.3)));
                self.clip.setHidden(self.hidden_by_host.get());
            }
            None => self.clip.setHidden(true),
        }
        self.layer.setContentsScale(scale);
        CATransaction::commit();
    }
}

/// Takes the layer out of the window's layer tree.
///
/// **Without this a dropped `MapView` leaves its pixels on screen forever.**
/// The layer is retained by the window's own layer, not by this struct alone,
/// so releasing our `Retained` merely decrements a count -- Core Animation
/// keeps compositing the last frame that was drawn into it, over whatever the
/// application drew next. `removeFromSuperlayer` is the other half of the
/// `addSublayer`/`insertSublayer_below` in `MapLayer::new`.
impl Drop for MapLayer {
    fn drop(&mut self) {
        self.clip.removeFromSuperlayer();
    }
}

/// The two frames [`MapLayer::sync_frame`] sets, in Core Animation's
/// bottom-left coordinates: `clip` in the parent layer (the visible part
/// of the map), `map` inside the clip (the whole map, offset so its
/// pixels stay where gpui laid the element out). `(x, y, w, h)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LayerFrames {
    pub clip: (f64, f64, f64, f64),
    pub map: (f64, f64, f64, f64),
}

/// `bounds` (the element) and `mask` (gpui's content mask), both
/// top-left `(x, y, w, h)` in window points; `None` when no part of the
/// element is inside the mask.
pub(crate) fn clipped_frames(bounds: (f64, f64, f64, f64), mask: (f64, f64, f64, f64), viewport_height: f64) -> Option<LayerFrames> {
    let (x, y, w, h) = bounds;
    let (left, top) = (x.max(mask.0), y.max(mask.1));
    let (right, bottom) = ((x + w).min(mask.0 + mask.2), (y + h).min(mask.1 + mask.3));
    if right <= left || bottom <= top || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let clip = (left, viewport_height - bottom, right - left, bottom - top);
    // Inside the clip (bottom-left origin): the map's bottom edge is
    // `bottom - (y + h)` above the clip's bottom edge (negative when the
    // map hangs below it).
    let map = (x - left, bottom - (y + h), w, h);
    Some(LayerFrames { clip, map })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclipped_map_fills_its_clip_exactly() {
        let f = clipped_frames((100.0, 50.0, 400.0, 300.0), (0.0, 0.0, 1000.0, 800.0), 800.0).unwrap();
        assert_eq!(f.clip, (100.0, 800.0 - 350.0, 400.0, 300.0));
        assert_eq!(f.map, (0.0, 0.0, 400.0, 300.0));
    }

    #[test]
    fn a_map_scrolled_half_above_its_container_shows_only_the_part_inside() {
        // The container (a scrolled page under a 200 pt header) starts at
        // y = 200; the map starts 150 pt above it.
        let f = clipped_frames((100.0, 50.0, 400.0, 300.0), (0.0, 200.0, 1000.0, 600.0), 800.0).unwrap();
        // Visible: y 200..350 (top-left), so the clip is 150 tall and its
        // bottom sits 800 - 350 = 450 up from the window's bottom.
        assert_eq!(f.clip, (100.0, 450.0, 400.0, 150.0));
        // The whole map inside the clip: same bottom edge, full height --
        // its top 150 pt are cut off by the clip's mask.
        assert_eq!(f.map, (0.0, 0.0, 400.0, 300.0));
        // Scrolled below: visible y 600..800 of a map at 600..900.
        let g = clipped_frames((100.0, 600.0, 400.0, 300.0), (0.0, 0.0, 1000.0, 800.0), 800.0).unwrap();
        assert_eq!(g.clip, (100.0, 0.0, 400.0, 200.0));
        assert_eq!(g.map, (0.0, -100.0, 400.0, 300.0), "its bottom 100 pt hang below the clip");
    }

    #[test]
    fn a_map_scrolled_fully_out_or_collapsed_has_no_frame() {
        assert!(clipped_frames((100.0, -400.0, 400.0, 300.0), (0.0, 0.0, 1000.0, 800.0), 800.0).is_none());
        assert!(clipped_frames((100.0, 50.0, 0.0, 300.0), (0.0, 0.0, 1000.0, 800.0), 800.0).is_none());
    }
}
