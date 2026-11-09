//! How the map reaches the screen off macOS: as an image in gpui's own scene.
//!
//! On macOS the map presents into a `CAMetalLayer` beside gpui's
//! (`render::MapLayer`). Linux and Windows have no counterpart, so there the
//! map draws each frame into a texture of its own on its own GPU
//! (`FrameTarget::Offscreen`, the same pass as a window frame), copies it
//! back (`render::frame_grab`, read off the UI thread) and hands the pixels
//! to gpui as a `RenderImage`, which `Window::paint_image` draws like any
//! other image. Clipping by scroll containers, rounded corners and z-order
//! with the host's own elements then come from gpui, for free.
//!
//! The cost is the copy: one frame's bytes GPU -> CPU -> gpui's atlas, and a
//! frame or two of latency. One frame is read at a time; while it is in
//! flight the last finished one stays on screen.
//!
//! This type only holds the images. `MapView::paint` decides when to draw a
//! frame (`MapView::render_offscreen_live`) and paints the newest one here
//! every time it paints, because gpui rebuilds its scene each frame and an
//! image not painted is an image gone.

use std::sync::Arc;

use gpui::{px, Bounds, Corners, Pixels, RenderImage, Window};

/// The newest finished frame, and the ones it replaced.
#[derive(Default)]
pub(crate) struct OffscreenPresenter {
    /// Painted on every `MapView::paint` until a newer frame replaces it.
    shown: Option<Arc<RenderImage>>,
    /// Replaced frames, still in gpui's sprite atlas: dropped at the next
    /// paint (dropping needs the `Window`, which a read's completion does
    /// not have). Without this every frame would stay in the atlas.
    retired: Vec<Arc<RenderImage>>,
    /// A frame is being read back. The next one waits for it.
    in_flight: bool,
    /// The GPU could not be brought up; said once, not on every paint.
    failed: bool,
    /// What the last `paint` drew, bottom first: gpui's test window keeps
    /// no scene a test can read, so the tests read this instead.
    #[cfg(test)]
    pub(crate) last_paint: Vec<Painted>,
}

/// One layer `OffscreenPresenter::paint` drew (tests only).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Painted {
    /// The backdrop colour, sRGB 0..1.
    Backdrop((f32, f32, f32, f32)),
    /// The newest frame, `(width, height)` in device pixels.
    Frame(u32, u32),
}

impl OffscreenPresenter {
    pub(crate) fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn begin_read(&mut self) {
        self.in_flight = true;
    }

    /// A read ended: `frame` is `(width, height, BGRA8 bytes)`, or `None`
    /// when the read failed (the last frame then stays on screen).
    pub(crate) fn finish_read(&mut self, frame: Option<(u32, u32, Vec<u8>)>) {
        self.in_flight = false;
        let Some((width, height, bgra)) = frame else { return };
        // gpui's `RenderImage` is BGRA in an `RgbaImage` buffer -- the type
        // carries no channel order, only the size.
        let Some(buffer) = image::RgbaImage::from_raw(width, height, bgra) else { return };
        let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
        if let Some(old) = self.shown.replace(image) {
            self.retired.push(old);
        }
    }

    /// Says once that there is no GPU for the map; `true` the first time.
    pub(crate) fn note_failed(&mut self) -> bool {
        !std::mem::replace(&mut self.failed, true)
    }

    /// Paints the newest frame over `bounds`, rounded by `corner_radius`
    /// logical pixels, after freeing the atlas space of the frames it
    /// replaced. A frame drawn for an earlier size is stretched until the
    /// one for this size lands.
    ///
    /// `backdrop` (`MapView::set_backdrop_color`, sRGB 0..1) is painted
    /// under the frame first: what the macOS layer's own backdrop shows --
    /// before the first frame lands, and wherever a frame is not opaque.
    pub(crate) fn paint(
        &mut self,
        bounds: Bounds<Pixels>,
        corner_radius: f32,
        backdrop: Option<(f32, f32, f32, f32)>,
        window: &mut Window,
    ) {
        for old in self.retired.drain(..) {
            let _ = window.drop_image(old);
        }
        #[cfg(test)]
        self.last_paint.clear();
        if let Some((r, g, b, a)) = backdrop {
            window.paint_quad(gpui::fill(bounds, gpui::Rgba { r, g, b, a }).corner_radii(Corners::all(px(corner_radius))));
            #[cfg(test)]
            self.last_paint.push(Painted::Backdrop((r, g, b, a)));
        }
        if let Some(image) = &self.shown {
            let _ = window.paint_image(bounds, bounds, Corners::all(px(corner_radius)), Arc::clone(image), 0, false);
            #[cfg(test)]
            {
                let size = image.size(0);
                self.last_paint.push(Painted::Frame(size.width.0 as u32, size.height.0 as u32));
            }
        }
    }
}
