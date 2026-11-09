//! The compass button: where it sits, when it shows, what its needle looks
//! like, and whether a tap hit it -- decided once, for every shell.
//!
//! A shell's only jobs are drawing it (native and iOS: one `IconInstance`
//! through the icon pipeline they already run; the browser: a DOM button in
//! its control stack) and turning a hit into `CameraIntent::ResetNorth`. The
//! needle's angle comes from `camera().bearing`, so no shell computes it.
//!
//! Shown only while the map is turned off north -- Apple Maps' rule. It is
//! also what keeps a north-up frame pixel-identical to before the compass
//! existed, which the 3D camera plan's pitch-0 captures check.

/// Button size, logical pixels.
pub const COMPASS_SIZE_PX: f32 = 36.0;
/// Corner margin, logical pixels: the corner chrome's one inset. The disc's
/// ink runs to within half an artwork texel (0.14 px) of its quad, so the
/// quad's margin IS the ink's (`the_discs_outer_edge_stands_on_the_corner_inset`).
pub const COMPASS_MARGIN_PX: f32 = crate::chrome::CORNER_INSET_PX;
/// The artwork's raster size: 36 pt at 3x is 108 px, rounded up to a
/// power of two so the one mip level is clean.
pub const COMPASS_ARTWORK_PX: u32 = 128;

#[must_use]
pub fn compass_visible(bearing: f32) -> bool {
    bearing != 0.0
}

/// `(left, top, size)` in logical pixels: the top-right corner, pushed down
/// by `top_inset_px` (iOS's status bar and Dynamic Island; 0 elsewhere).
#[must_use]
pub fn compass_rect(viewport_width_px: f32, top_inset_px: f32) -> (f32, f32, f32) {
    compass_rect_at(viewport_width_px, top_inset_px, (0.0, 0.0))
}

/// [`compass_rect`] moved by a host's `offset`: `.0` logical px LEFT and
/// `.1` DOWN of its corner (`MapView::set_compass_offset`), so a host that
/// floats cards in the top-right corner can stand the compass beside them
/// on the same top line instead of pushing the cards below it.
#[must_use]
pub fn compass_rect_at(viewport_width_px: f32, top_inset_px: f32, offset: (f32, f32)) -> (f32, f32, f32) {
    (viewport_width_px - COMPASS_MARGIN_PX - COMPASS_SIZE_PX - offset.0, top_inset_px + COMPASS_MARGIN_PX + offset.1, COMPASS_SIZE_PX)
}

/// [`compass_rect`] for a layout direction: the top-right corner left to
/// right, mirrored to the top-left right to left (an RTL host's chrome).
#[must_use]
pub fn compass_rect_dir(viewport_width_px: f32, top_inset_px: f32, rtl: bool) -> (f32, f32, f32) {
    let (left, top, size) = compass_rect(viewport_width_px, top_inset_px);
    if rtl {
        (viewport_width_px - left - size, top, size)
    } else {
        (left, top, size)
    }
}

/// [`compass_hit`] on the disc [`compass_rect_dir`] places.
#[must_use]
pub fn compass_hit_dir(x_px: f32, y_px: f32, viewport_width_px: f32, top_inset_px: f32, rtl: bool) -> bool {
    let (left, top, size) = compass_rect_dir(viewport_width_px, top_inset_px, rtl);
    let (dx, dy) = (x_px - (left + size / 2.0), y_px - (top + size / 2.0));
    dx * dx + dy * dy <= (size / 2.0) * (size / 2.0)
}

/// Whether logical point `(x_px, y_px)` is on the drawn disc.
#[must_use]
pub fn compass_hit(x_px: f32, y_px: f32, viewport_width_px: f32, top_inset_px: f32) -> bool {
    compass_hit_at(x_px, y_px, viewport_width_px, top_inset_px, (0.0, 0.0))
}

/// [`compass_hit`] on the disc [`compass_rect_at`] places.
#[must_use]
pub fn compass_hit_at(x_px: f32, y_px: f32, viewport_width_px: f32, top_inset_px: f32, offset: (f32, f32)) -> bool {
    let (left, top, size) = compass_rect_at(viewport_width_px, top_inset_px, offset);
    let (dx, dy) = (x_px - (left + size / 2.0), y_px - (top + size / 2.0));
    dx * dx + dy * dy <= (size / 2.0) * (size / 2.0)
}

/// The needle's clockwise, y-down rotation from "straight up". Bearing
/// grows the other way (at +90 degrees the camera faces east and north is
/// to the LEFT), so it is simply the negation.
#[must_use]
pub fn needle_rotation_rad(bearing: f32) -> f32 {
    -bearing
}

/// The artwork, drawn here rather than vendored, for the reason
/// `brand_mark` gives for vendoring PNGs instead of SVGs: it has to decode
/// identically in three dependency sets, and pixels computed from a formula
/// are the one form a test can pin without a file. A white disc and a
/// two-tone diamond needle -- red north, grey south -- antialiased over one
/// pixel. Straight alpha, as `IconPipeline` expects.
#[must_use]
pub fn compass_artwork(size_px: u32) -> image::RgbaImage {
    let size = size_px as f32;
    let centre = size / 2.0;
    let radius = centre - 1.0;
    let (half_w, half_h) = (size * 0.14, size * 0.38);
    let mut art = image::RgbaImage::new(size_px, size_px);
    for (x, y, pixel) in art.enumerate_pixels_mut() {
        let (px, py) = (x as f32 + 0.5 - centre, y as f32 + 0.5 - centre);
        let disc = (radius - (px * px + py * py).sqrt() + 0.5).clamp(0.0, 1.0);
        if disc == 0.0 {
            continue;
        }
        let inside = (1.0 - (px.abs() / half_w + py.abs() / half_h)) * half_w;
        let needle = (inside + 0.5).clamp(0.0, 1.0);
        let ink: [f32; 3] = if py < 0.0 { [226.0, 60.0, 50.0] } else { [120.0, 128.0, 140.0] };
        let mix = |paper: f32, ink: f32| (paper + (ink - paper) * needle).round() as u8;
        *pixel = image::Rgba([mix(255.0, ink[0]), mix(255.0, ink[1]), mix(255.0, ink[2]), (disc * 235.0).round() as u8]);
    }
    art
}

/// The one quad, in PHYSICAL pixels, rotated by the needle.
#[allow(dead_code)] // the iOS shell's corner disc; the native map draws `compass_instance_at`, the browser a DOM button
#[must_use]
pub(crate) fn compass_instance(bearing: f32, viewport_width_px: f32, top_inset_px: f32, scale_factor: f32) -> crate::render::IconInstance {
    compass_instance_at(bearing, viewport_width_px, top_inset_px, (0.0, 0.0), scale_factor)
}

/// [`compass_instance`] on the disc [`compass_rect_dir`] places.
/// The iOS shell's (native and the browser have no RTL chrome yet).
#[allow(dead_code)]
#[must_use]
pub(crate) fn compass_instance_dir(bearing: f32, viewport_width_px: f32, top_inset_px: f32, rtl: bool, scale_factor: f32) -> crate::render::IconInstance {
    let (left, _, _) = compass_rect_dir(viewport_width_px, top_inset_px, rtl);
    crate::render::IconInstance {
        screen_x: left * scale_factor,
        ..compass_instance(bearing, viewport_width_px, top_inset_px, scale_factor)
    }
}

/// [`compass_instance`] on the disc [`compass_rect_at`] places.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[must_use]
pub(crate) fn compass_instance_at(bearing: f32, viewport_width_px: f32, top_inset_px: f32, offset: (f32, f32), scale_factor: f32) -> crate::render::IconInstance {
    let (left, top, size) = compass_rect_at(viewport_width_px, top_inset_px, offset);
    crate::render::IconInstance {
        screen_x: left * scale_factor,
        screen_y: top * scale_factor,
        width: size * scale_factor,
        height: size * scale_factor,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        rotation_rad: needle_rotation_rad(bearing),
        sdf: None,
        opacity: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_mirrors_the_disc_its_hit_target_and_its_quad_to_the_top_left() {
        let (w, top) = (390.0, 47.0);
        let (l, t, s) = compass_rect_dir(w, top, false);
        let (rl, rt, rs) = compass_rect_dir(w, top, true);
        assert_eq!((t, s), (rt, rs));
        assert!((rl - (w - l - s)).abs() < 1e-4, "mirrored about the centre line");
        assert!(rl < w / 2.0 && l > w / 2.0);
        let centre = |left: f32| (left + s / 2.0, t + s / 2.0);
        assert!(compass_hit_dir(centre(rl).0, centre(rl).1, w, top, true));
        assert!(!compass_hit_dir(centre(l).0, centre(l).1, w, top, true));
        assert!(compass_hit_dir(centre(l).0, centre(l).1, w, top, false));
        assert_eq!(compass_instance_dir(0.5, w, top, true, 3.0).screen_x, rl * 3.0);
        let (ltr, plain) = (compass_instance_dir(0.5, w, top, false, 3.0), compass_instance(0.5, w, top, 3.0));
        assert_eq!((ltr.screen_x, ltr.screen_y, ltr.rotation_rad), (plain.screen_x, plain.screen_y, plain.rotation_rad));
    }

    #[test]
    fn a_host_offset_moves_the_disc_and_its_hit_target_together() {
        let (l0, t0, s) = compass_rect(800.0, 0.0);
        let (l, t, s2) = compass_rect_at(800.0, 0.0, (304.0, 0.0));
        assert_eq!((l, t, s2), (l0 - 304.0, t0, s));
        let centre = (l + s / 2.0, t + s / 2.0);
        assert!(compass_hit_at(centre.0, centre.1, 800.0, 0.0, (304.0, 0.0)));
        assert!(!compass_hit(centre.0, centre.1, 800.0, 0.0), "the corner's disc is not there any more");
        let inst = compass_instance_at(0.3, 800.0, 0.0, (304.0, 10.0), 2.0);
        assert_eq!((inst.screen_x, inst.screen_y), (l * 2.0, (t + 10.0) * 2.0));
    }

    #[test]
    fn the_compass_shows_only_off_north() {
        assert!(!compass_visible(0.0));
        assert!(compass_visible(0.001));
        assert!(compass_visible(-3.0));
    }

    #[test]
    fn the_needle_points_where_north_is_on_screen() {
        // Through the real camera, not a restated formula: project a point
        // due north of the centre and compare its screen direction with the
        // needle's (a y-down clockwise rotation of "straight up").
        for bearing_deg in [0.0f32, 90.0, -135.0, 30.0] {
            let camera = crate::render::Camera {
                center_lon: 103.8198,
                center_lat: 1.3521,
                zoom: 14.0,
                pitch: 0.0,
                bearing: bearing_deg.to_radians(),
                viewport_width_px: 800.0,
                viewport_height_px: 800.0,
                fov_y: crate::render::Camera::DEFAULT_FOV_Y,
            };
            let (x, y) = crate::geo::lon_lat_to_tile_xy(103.8198, 1.3621, 0.0);
            let world = f64::from(crate::render::WORLD_SIZE);
            let (sx, sy) = crate::render::project_world_to_screen_f64(&camera, (x * world, y * world)).unwrap();
            let (dx, dy) = (sx - 400.0, sy - 400.0);
            let len = (dx * dx + dy * dy).sqrt();
            let r = needle_rotation_rad(camera.bearing);
            let (nx, ny) = (r.sin(), -r.cos());
            assert!((dx / len - nx).abs() < 1e-3 && (dy / len - ny).abs() < 1e-3, "bearing {bearing_deg}: north is ({dx}, {dy}), needle ({nx}, {ny})");
        }
    }

    #[test]
    fn the_hit_target_is_the_drawn_disc_in_the_top_right_corner() {
        let (left, top, size) = compass_rect(800.0, 0.0);
        assert_eq!((left, top, size), (800.0 - COMPASS_MARGIN_PX - COMPASS_SIZE_PX, COMPASS_MARGIN_PX, COMPASS_SIZE_PX));
        let (cx, cy) = (left + size / 2.0, top + size / 2.0);
        assert!(compass_hit(cx, cy, 800.0, 0.0));
        assert!(compass_hit(cx + size / 2.0 - 0.5, cy, 800.0, 0.0));
        assert!(!compass_hit(left + 1.0, top + 1.0, 800.0, 0.0), "the square's corner is outside the disc");
        assert!(!compass_hit(cx, cy, 800.0, 50.0), "a safe-area inset moves it down");
        assert!(compass_hit(cx, cy + 50.0, 800.0, 50.0));
    }

    /// The disc's OUTER EDGE -- its ink, not its quad -- stands
    /// `chrome::CORNER_INSET_PX` in from the top and the right edge, at 1x
    /// and 2x: measured off the artwork's own alpha along the row and the
    /// column through its centre.
    #[test]
    fn the_discs_outer_edge_stands_on_the_corner_inset() {
        let art = compass_artwork(COMPASS_ARTWORK_PX);
        let n = COMPASS_ARTWORK_PX;
        let mid = n / 2;
        let top_texel = (0..n).find(|&y| art.get_pixel(mid, y)[3] > 0).expect("ink");
        let right_texel = (0..n).rev().find(|&x| art.get_pixel(x, mid)[3] > 0).expect("ink");
        for scale in [1.0f32, 2.0] {
            let viewport = 800.0;
            let inst = compass_instance(0.5, viewport, 0.0, scale);
            let texel = inst.width / n as f32;
            let ink_top = (inst.screen_y + top_texel as f32 * texel) / scale;
            let ink_right = (viewport * scale - (inst.screen_x + (right_texel + 1) as f32 * texel)) / scale;
            for (side, inset) in [("top", ink_top), ("right", ink_right)] {
                assert!(
                    (inset - crate::chrome::CORNER_INSET_PX).abs() <= 0.5,
                    "at {scale}x the disc's {side} edge stands {inset:.2} px in, not {}",
                    crate::chrome::CORNER_INSET_PX
                );
            }
        }
    }

    #[test]
    fn the_artwork_is_a_disc_with_a_red_north_half() {
        let art = compass_artwork(COMPASS_ARTWORK_PX);
        assert_eq!(art.dimensions(), (COMPASS_ARTWORK_PX, COMPASS_ARTWORK_PX));
        assert_eq!(art.get_pixel(0, 0)[3], 0, "outside the disc is transparent");
        let mid = COMPASS_ARTWORK_PX / 2;
        let north = art.get_pixel(mid, mid / 2);
        let south = art.get_pixel(mid, mid + mid / 2);
        assert!(north[0] > 200 && north[1] < 100, "north half of the needle is red: {north:?}");
        assert!(south[0] < 160 && (i16::from(south[2]) - i16::from(south[0])).abs() < 40, "south half is grey: {south:?}");
    }

    #[test]
    fn the_instance_is_the_rect_in_physical_pixels_turned_by_the_needle() {
        let inst = compass_instance(1.0, 800.0, 20.0, 2.0);
        let (left, top, size) = compass_rect(800.0, 20.0);
        assert_eq!((inst.screen_x, inst.screen_y, inst.width, inst.height), (left * 2.0, top * 2.0, size * 2.0, size * 2.0));
        assert_eq!(inst.rotation_rad, needle_rotation_rad(1.0));
        assert!(inst.sdf.is_none());
    }
}
