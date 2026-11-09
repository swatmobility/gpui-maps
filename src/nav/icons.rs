//! The chrome's artwork, computed rather than vendored -- `compass::compass_artwork`'s
//! rule, for its reason: the pixels have to come out the same from three
//! dependency sets, and a formula is the one form a test can pin without a
//! file. Every shape is this file's own geometry (strokes, triangles,
//! discs), drawn in the maneuver vocabulary Ferrostar and Mapbox use; no
//! third-party icon is copied.
//!
//! One atlas of [`ICON_CELL_PX`] cells, [`ATLAS_COLUMNS`] to a row, one cell
//! per [`ChromeGlyph`] in [`ATLAS_ORDER`]. Straight alpha, as `IconPipeline`
//! expects. Rows rather than one strip so it stays inside
//! [`MAX_ATLAS_SIDE_PX`], the side WebGL2 guarantees.

use crate::nav::chrome::ChromeGlyph;
use crate::nav::view_model::ManeuverGlyph;

/// 192 so the 64 pt maneuver icon is drawn 1:1 on a 3x phone, not upscaled.
pub const ICON_CELL_PX: u32 = 192;
/// Cells per atlas row.
pub const ATLAS_COLUMNS: u32 = 5;
/// WebGL2's guaranteed `MAX_TEXTURE_SIZE`: the atlas must fit it on both axes.
pub const MAX_ATLAS_SIDE_PX: u32 = 2_048;

const fn atlas_rows() -> u32 {
    (ATLAS_ORDER.len() as u32).div_ceil(ATLAS_COLUMNS)
}

/// Every glyph the chrome draws, in atlas order.
pub const ATLAS_ORDER: [ChromeGlyph; 24] = [
    ChromeGlyph::Maneuver(ManeuverGlyph::Straight),
    ChromeGlyph::Maneuver(ManeuverGlyph::SlightLeft),
    ChromeGlyph::Maneuver(ManeuverGlyph::Left),
    ChromeGlyph::Maneuver(ManeuverGlyph::SharpLeft),
    ChromeGlyph::Maneuver(ManeuverGlyph::SlightRight),
    ChromeGlyph::Maneuver(ManeuverGlyph::Right),
    ChromeGlyph::Maneuver(ManeuverGlyph::SharpRight),
    ChromeGlyph::Maneuver(ManeuverGlyph::UTurnLeft),
    ChromeGlyph::Maneuver(ManeuverGlyph::UTurnRight),
    ChromeGlyph::Maneuver(ManeuverGlyph::RoundaboutCounterClockwise),
    ChromeGlyph::Maneuver(ManeuverGlyph::RoundaboutClockwise),
    ChromeGlyph::Maneuver(ManeuverGlyph::Depart),
    ChromeGlyph::Maneuver(ManeuverGlyph::Arrive),
    ChromeGlyph::Puck { muted: false },
    ChromeGlyph::Puck { muted: true },
    ChromeGlyph::Recenter,
    ChromeGlyph::Overview,
    ChromeGlyph::Close,
    ChromeGlyph::Play,
    ChromeGlyph::Speaker { muted: false },
    ChromeGlyph::Speaker { muted: true },
    ChromeGlyph::Pause,
    ChromeGlyph::Resume,
    ChromeGlyph::ArrowHead,
];

const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
const INK: [f32; 3] = [0.118, 0.122, 0.137];
const PUCK_BLUE: [f32; 3] = [0.165, 0.490, 0.941];
const PUCK_MUTED: [f32; 3] = [0.560, 0.585, 0.620];

/// Geometry in cell units, `(0, 0)` top-left to `(1, 1)` bottom-right.
#[derive(Debug, Clone)]
enum Shape {
    Stroke { points: Vec<(f32, f32)>, width: f32 },
    Triangle([(f32, f32); 3]),
    Disc { centre: (f32, f32), radius: f32 },
    Ring { centre: (f32, f32), radius: f32, width: f32 },
}

impl Shape {
    fn mirrored(&self) -> Self {
        let m = |(x, y): (f32, f32)| (1.0 - x, y);
        match self {
            Self::Stroke { points, width } => Self::Stroke { points: points.iter().copied().map(m).collect(), width: *width },
            Self::Triangle(t) => Self::Triangle([m(t[0]), m(t[1]), m(t[2])]),
            Self::Disc { centre, radius } => Self::Disc { centre: m(*centre), radius: *radius },
            Self::Ring { centre, radius, width } => Self::Ring { centre: m(*centre), radius: *radius, width: *width },
        }
    }

    /// Signed distance in cell units: negative inside.
    fn distance(&self, p: (f32, f32)) -> f32 {
        match self {
            Self::Stroke { points, width } => {
                points.windows(2).map(|s| segment_distance(p, s[0], s[1])).fold(f32::INFINITY, f32::min) - width / 2.0
            }
            Self::Triangle([a, b, c]) => {
                let edge = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
                let area = edge(*c, *a, *b);
                let inside = [edge(p, *a, *b), edge(p, *b, *c), edge(p, *c, *a)].iter().all(|e| e * area >= 0.0);
                let outline = segment_distance(p, *a, *b).min(segment_distance(p, *b, *c)).min(segment_distance(p, *c, *a));
                if inside { -outline } else { outline }
            }
            Self::Disc { centre, radius } => ((p.0 - centre.0).hypot(p.1 - centre.1)) - radius,
            Self::Ring { centre, radius, width } => ((p.0 - centre.0).hypot(p.1 - centre.1) - radius).abs() - width / 2.0,
        }
    }
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    (p.0 - (a.0 + dx * t)).hypot(p.1 - (a.1 + dy * t))
}

/// A shaft through `points` ending in an arrowhead that points along its
/// last segment.
fn arrow(points: &[(f32, f32)]) -> Vec<Shape> {
    const SHAFT: f32 = 0.12;
    const HEAD_LENGTH: f32 = 0.22;
    const HEAD_HALF: f32 = 0.17;
    let (a, b) = (points[points.len() - 2], points[points.len() - 1]);
    let len = (b.0 - a.0).hypot(b.1 - a.1).max(1e-6);
    let (ux, uy) = ((b.0 - a.0) / len, (b.1 - a.1) / len);
    let tip = (b.0 + ux * HEAD_LENGTH, b.1 + uy * HEAD_LENGTH);
    vec![
        Shape::Stroke { points: points.to_vec(), width: SHAFT },
        Shape::Triangle([(b.0 - uy * HEAD_HALF, b.1 + ux * HEAD_HALF), tip, (b.0 + uy * HEAD_HALF, b.1 - ux * HEAD_HALF)]),
    ]
}

/// A U-turn's shaft: up, over a half circle to the left, and back down.
fn u_turn_left() -> Vec<Shape> {
    let (cx, cy, r) = (0.5, 0.40, 0.16);
    let mut points = vec![(cx + r, 0.88), (cx + r, cy)];
    for i in 1..=12 {
        let a = std::f32::consts::PI * i as f32 / 12.0;
        points.push((cx + r * a.cos(), cy - r * a.sin()));
    }
    points.push((cx - r, 0.58));
    arrow(&points)
}

fn roundabout_counter_clockwise() -> Vec<Shape> {
    let mut shapes = vec![
        Shape::Ring { centre: (0.5, 0.42), radius: 0.17, width: 0.08 },
        Shape::Stroke { points: vec![(0.5, 0.90), (0.5, 0.62)], width: 0.12 },
    ];
    shapes.extend(arrow(&[(0.64, 0.32), (0.74, 0.22)]));
    shapes
}

fn shapes(glyph: ChromeGlyph) -> Vec<([f32; 3], Shape)> {
    let white = |s: Vec<Shape>| s.into_iter().map(|s| (WHITE, s)).collect::<Vec<_>>();
    let mirror = |s: Vec<([f32; 3], Shape)>| s.into_iter().map(|(c, s)| (c, s.mirrored())).collect::<Vec<_>>();
    match glyph {
        ChromeGlyph::Maneuver(m) => match m {
            ManeuverGlyph::Straight => white(arrow(&[(0.5, 0.90), (0.5, 0.34)])),
            ManeuverGlyph::SlightRight => white(arrow(&[(0.42, 0.90), (0.42, 0.62), (0.60, 0.36)])),
            ManeuverGlyph::Right => white(arrow(&[(0.36, 0.90), (0.36, 0.50), (0.66, 0.50)])),
            ManeuverGlyph::SharpRight => white(arrow(&[(0.34, 0.90), (0.34, 0.34), (0.58, 0.62)])),
            ManeuverGlyph::SlightLeft => mirror(shapes(ChromeGlyph::Maneuver(ManeuverGlyph::SlightRight))),
            ManeuverGlyph::Left => mirror(shapes(ChromeGlyph::Maneuver(ManeuverGlyph::Right))),
            ManeuverGlyph::SharpLeft => mirror(shapes(ChromeGlyph::Maneuver(ManeuverGlyph::SharpRight))),
            ManeuverGlyph::UTurnLeft => white(u_turn_left()),
            ManeuverGlyph::UTurnRight => mirror(white(u_turn_left())),
            ManeuverGlyph::RoundaboutCounterClockwise => white(roundabout_counter_clockwise()),
            ManeuverGlyph::RoundaboutClockwise => mirror(white(roundabout_counter_clockwise())),
            ManeuverGlyph::Depart => {
                let mut s = white(arrow(&[(0.5, 0.74), (0.5, 0.34)]));
                s.push((WHITE, Shape::Disc { centre: (0.5, 0.84), radius: 0.07 }));
                s
            }
            ManeuverGlyph::Arrive => vec![
                (WHITE, Shape::Disc { centre: (0.5, 0.38), radius: 0.24 }),
                (WHITE, Shape::Triangle([(0.30, 0.50), (0.5, 0.90), (0.70, 0.50)])),
                (BANNER_INK, Shape::Disc { centre: (0.5, 0.38), radius: 0.09 }),
            ],
        },
        ChromeGlyph::Puck { muted } => {
            let body = if muted { PUCK_MUTED } else { PUCK_BLUE };
            vec![
                (WHITE, Shape::Disc { centre: (0.5, 0.5), radius: 0.46 }),
                (body, Shape::Disc { centre: (0.5, 0.5), radius: 0.38 }),
                (WHITE, Shape::Triangle([(0.5, 0.20), (0.72, 0.72), (0.28, 0.72)])),
            ]
        }
        ChromeGlyph::Recenter => vec![
            (INK, Shape::Triangle([(0.5, 0.14), (0.80, 0.86), (0.20, 0.86)])),
            (BUTTON_INK_HOLE, Shape::Triangle([(0.5, 0.64), (0.66, 0.80), (0.34, 0.80)])),
        ],
        ChromeGlyph::Overview => vec![
            (INK, Shape::Stroke { points: vec![(0.22, 0.80), (0.38, 0.36), (0.62, 0.62), (0.78, 0.20)], width: 0.10 }),
            (INK, Shape::Disc { centre: (0.22, 0.80), radius: 0.09 }),
            (INK, Shape::Disc { centre: (0.78, 0.20), radius: 0.09 }),
        ],
        ChromeGlyph::Close => vec![
            (WHITE, Shape::Stroke { points: vec![(0.26, 0.26), (0.74, 0.74)], width: 0.12 }),
            (WHITE, Shape::Stroke { points: vec![(0.74, 0.26), (0.26, 0.74)], width: 0.12 }),
        ],
        ChromeGlyph::Play => vec![(WHITE, Shape::Triangle([(0.30, 0.20), (0.82, 0.5), (0.30, 0.80)]))],
        ChromeGlyph::Pause => vec![
            (INK, Shape::Stroke { points: vec![(0.36, 0.26), (0.36, 0.74)], width: 0.14 }),
            (INK, Shape::Stroke { points: vec![(0.64, 0.26), (0.64, 0.74)], width: 0.14 }),
        ],
        ChromeGlyph::Resume => vec![(INK, Shape::Triangle([(0.34, 0.22), (0.78, 0.5), (0.34, 0.78)]))],
        ChromeGlyph::ArrowHead => arrow_head(),
        ChromeGlyph::Speaker { muted } => {
            // A box and a cone, then sound waves -- or, muted, a cross.
            let mut s = vec![
                (INK, Shape::Triangle([(0.14, 0.38), (0.32, 0.38), (0.32, 0.62)])),
                (INK, Shape::Triangle([(0.14, 0.38), (0.32, 0.62), (0.14, 0.62)])),
                (INK, Shape::Triangle([(0.28, 0.38), (0.50, 0.18), (0.50, 0.82)])),
                (INK, Shape::Triangle([(0.28, 0.38), (0.50, 0.82), (0.28, 0.62)])),
            ];
            if muted {
                s.push((INK, Shape::Stroke { points: vec![(0.62, 0.38), (0.86, 0.62)], width: 0.08 }));
                s.push((INK, Shape::Stroke { points: vec![(0.86, 0.38), (0.62, 0.62)], width: 0.08 }));
            } else {
                for radius in [0.16f32, 0.30] {
                    let points = (0..=8)
                        .map(|i| {
                            let a = (-50.0 + 12.5 * i as f32).to_radians();
                            (0.52 + radius * a.cos(), 0.5 + radius * a.sin())
                        })
                        .collect();
                    s.push((INK, Shape::Stroke { points, width: 0.07 }));
                }
            }
            s
        }
    }
}

/// The route's casing colour (`route_layer::REMAINING_CASING_COLOR`), for
/// the arrowhead's outline: the shaft's casing, continued round the head.
const ROUTE_CASING: [f32; 3] = {
    let c = crate::nav::route_layer::REMAINING_CASING_COLOR;
    [c.r, c.g, c.b]
};

/// The maneuver arrowhead, pointing up, its base centred just ahead of the
/// cell's centre -- where the shaft ends (the icon is centred on that
/// point). A casing triangle, a stub of the shaft's white running from the
/// centre into the head (so the shaft's white and the head's white meet
/// with no casing seam between them), and the white head inset by the
/// casing's own thickness: one outlined shape with the shaft, the casing
/// all the way round, its top edge included.
fn arrow_head() -> Vec<([f32; 3], Shape)> {
    use crate::nav::chrome::{ARROW_HEAD_FILL, ARROW_HEAD_SIZE};
    use crate::nav::route_layer::{ARROW_CASING_WIDTH_PX, ARROW_WIDTH_PX};
    let half = ARROW_HEAD_FILL / 2.0;
    let base_y = 0.52;
    let tip_y = base_y - 1.6 * half;
    let outer = [(0.5 - half, base_y), (0.5, tip_y), (0.5 + half, base_y)];
    // Inset by the casing's thickness: scale about the incentre by (r - d) / r.
    let casing = (ARROW_CASING_WIDTH_PX - ARROW_WIDTH_PX) / 2.0 / ARROW_HEAD_SIZE;
    let side = half.hypot(base_y - tip_y);
    let (base, height) = (2.0 * half, base_y - tip_y);
    let r = base * height / (base + 2.0 * side);
    let incentre = (0.5, base_y - r);
    let k = ((r - casing) / r).max(0.0);
    let inner = outer.map(|(x, y)| (incentre.0 + (x - incentre.0) * k, incentre.1 + (y - incentre.1) * k));
    let shaft = ARROW_WIDTH_PX / ARROW_HEAD_SIZE;
    vec![
        (ROUTE_CASING, Shape::Triangle(outer)),
        (WHITE, Shape::Stroke { points: vec![(0.5, 0.5 + shaft / 2.0), (0.5, inner[0].1 - shaft / 2.0)], width: shaft }),
        (WHITE, Shape::Triangle(inner)),
    ]
}

/// The banner's own colour, for the hole in the arrival pin.
const BANNER_INK: [f32; 3] = [0.090, 0.353, 0.247];
/// The button's own colour, for the notch in the recenter arrow.
const BUTTON_INK_HOLE: [f32; 3] = [0.918, 0.925, 0.937];

/// One glyph at `size_px` square.
#[must_use]
pub fn glyph_artwork(glyph: ChromeGlyph, size_px: u32) -> image::RgbaImage {
    let shapes = shapes(glyph);
    let size = size_px as f32;
    let mut art = image::RgbaImage::new(size_px, size_px);
    for (x, y, pixel) in art.enumerate_pixels_mut() {
        let p = ((x as f32 + 0.5) / size, (y as f32 + 0.5) / size);
        let (mut rgb, mut alpha) = ([0.0f32; 3], 0.0f32);
        for (color, shape) in &shapes {
            // One pixel of antialiasing.
            let coverage = (0.5 - shape.distance(p) * size).clamp(0.0, 1.0);
            if coverage > 0.0 {
                for i in 0..3 {
                    rgb[i] = rgb[i] * (1.0 - coverage) + color[i] * coverage;
                }
                alpha = alpha * (1.0 - coverage) + coverage;
            }
        }
        if alpha > 0.0 {
            // `rgb` accumulated over a transparent start; un-premultiply.
            let a = alpha.max(1e-6);
            let straight = |c: f32| ((c / a).clamp(0.0, 1.0) * 255.0).round() as u8;
            *pixel = image::Rgba([straight(rgb[0]), straight(rgb[1]), straight(rgb[2]), (alpha * 255.0).round() as u8]);
        }
    }
    art
}

/// Every glyph, one cell each, in [`ATLAS_ORDER`]: left to right, then the
/// next row down.
#[must_use]
pub fn atlas() -> image::RgbaImage {
    let mut atlas = image::RgbaImage::new(ICON_CELL_PX * ATLAS_COLUMNS, ICON_CELL_PX * atlas_rows());
    for (index, glyph) in ATLAS_ORDER.iter().enumerate() {
        let (column, row) = (index as u32 % ATLAS_COLUMNS, index as u32 / ATLAS_COLUMNS);
        let art = glyph_artwork(*glyph, ICON_CELL_PX);
        for (x, y, pixel) in art.enumerate_pixels() {
            atlas.put_pixel(column * ICON_CELL_PX + x, row * ICON_CELL_PX + y, *pixel);
        }
    }
    atlas
}

/// `glyph`'s UV rectangle in [`atlas`], half a texel in from each edge so
/// linear filtering never reads the neighbouring cell.
#[must_use]
pub fn uv(glyph: ChromeGlyph) -> ((f32, f32), (f32, f32)) {
    let index = ATLAS_ORDER.iter().position(|g| *g == glyph).unwrap_or(0) as u32;
    let (column, row) = ((index % ATLAS_COLUMNS) as f32, (index / ATLAS_COLUMNS) as f32);
    let (columns, rows) = (ATLAS_COLUMNS as f32, atlas_rows() as f32);
    let half_u = 0.5 / (ICON_CELL_PX as f32 * columns);
    let half_v = 0.5 / (ICON_CELL_PX as f32 * rows);
    ((column / columns + half_u, row / rows + half_v), ((column + 1.0) / columns - half_u, (row + 1.0) / rows - half_v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old head's casing covered only the V's two sides ("reads as a
    /// cut-off triangle"): this one is outlined in the route's casing
    /// colour on all three sides, white inside, and the white runs out of
    /// its base to the centre, where the shaft's white ends.
    #[test]
    fn the_arrowhead_is_cased_all_round_in_the_route_casing_and_white_inside() {
        let n = 192u32;
        let art = glyph_artwork(ChromeGlyph::ArrowHead, n);
        let at = |x: f32, y: f32| *art.get_pixel((x * n as f32) as u32, (y * n as f32) as u32);
        let casing = ROUTE_CASING.map(|c| (c * 255.0).round() as u8);
        let is_casing = |p: image::Rgba<u8>| p[3] > 200 && (0..3).all(|i| (i32::from(p[i]) - i32::from(casing[i])).abs() <= 6);
        let is_white = |p: image::Rgba<u8>| p[3] > 200 && p[0] > 245 && p[1] > 245 && p[2] > 245;
        let half = crate::nav::chrome::ARROW_HEAD_FILL / 2.0;
        let (base_y, tip_y) = (0.52, 0.52 - 1.6 * half);
        assert!(is_casing(at(0.5, tip_y + 0.012)), "the tip is cased: {:?}", at(0.5, tip_y + 0.012));
        // Just inside each slanted side, halfway up, and along the base beside the stub.
        let mid_y = (base_y + tip_y) / 2.0;
        assert!(is_casing(at(0.5 - half / 2.0 + 0.012, mid_y)), "left side cased");
        assert!(is_casing(at(0.5 + half / 2.0 - 0.012, mid_y)), "right side cased");
        assert!(is_casing(at(0.5 - half * 0.6, base_y - 0.012)), "the base is cased (no open top edge)");
        assert!(is_white(at(0.5, (base_y + 2.0 * tip_y) / 3.0 + 0.05)), "white inside");
        assert!(is_white(at(0.5, base_y - 0.005)), "the shaft's white runs through the base");
        assert!(at(0.5, 0.97)[3] == 0 && at(0.03, 0.5)[3] == 0, "nothing behind the shaft's end or beside the head");
    }

    fn ink(art: &image::RgbaImage) -> (usize, f32) {
        let mut count = 0;
        let mut sum_x = 0.0;
        for (x, _, p) in art.enumerate_pixels() {
            if p[3] > 128 {
                count += 1;
                sum_x += x as f32;
            }
        }
        (count, if count > 0 { sum_x / count as f32 } else { 0.0 })
    }

    #[test]
    fn every_glyph_has_ink_and_a_cell() {
        for glyph in ATLAS_ORDER {
            let (count, _) = ink(&glyph_artwork(glyph, ICON_CELL_PX));
            assert!(count > 400, "{glyph:?} has only {count} inked pixels");
        }
    }

    #[test]
    fn the_atlas_fits_webgl2s_guaranteed_texture_size() {
        let (w, h) = atlas().dimensions();
        assert_eq!((w, h), (ICON_CELL_PX * ATLAS_COLUMNS, ICON_CELL_PX * atlas_rows()));
        assert!(w <= MAX_ATLAS_SIDE_PX && h <= MAX_ATLAS_SIDE_PX, "{w} x {h}");
        assert!(atlas_rows() * ATLAS_COLUMNS >= ATLAS_ORDER.len() as u32, "every glyph has a cell");
    }

    #[test]
    fn every_uv_samples_its_own_glyph_from_the_atlas() {
        let atlas = atlas();
        let (w, h) = (atlas.width() as f32, atlas.height() as f32);
        for glyph in ATLAS_ORDER {
            let art = glyph_artwork(glyph, ICON_CELL_PX);
            let ((u0, v0), _) = uv(glyph);
            let (x0, y0) = ((u0 * w).floor() as u32, (v0 * h).floor() as u32);
            assert_eq!((x0 % ICON_CELL_PX, y0 % ICON_CELL_PX), (0, 0), "{glyph:?} starts on a cell corner");
            for (x, y, p) in art.enumerate_pixels() {
                assert_eq!(atlas.get_pixel(x0 + x, y0 + y), p, "{glyph:?} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn a_right_turn_leans_right_and_its_left_is_its_mirror() {
        let right = glyph_artwork(ChromeGlyph::Maneuver(ManeuverGlyph::Right), 64);
        let left = glyph_artwork(ChromeGlyph::Maneuver(ManeuverGlyph::Left), 64);
        assert!(ink(&right).1 > 32.0 && ink(&left).1 < 32.0);
        for (x, y, p) in right.enumerate_pixels() {
            let mirrored = left.get_pixel(63 - x, y)[3];
            assert!(p[3].abs_diff(mirrored) <= 1, "pixel ({x}, {y}): {} vs {mirrored}", p[3]);
        }
    }

    #[test]
    fn uvs_address_their_own_cell() {
        let ((u0, v0), (u1, v1)) = uv(ChromeGlyph::Close);
        let index = ATLAS_ORDER.iter().position(|g| *g == ChromeGlyph::Close).unwrap() as u32;
        let (column, row) = ((index % ATLAS_COLUMNS) as f32, (index / ATLAS_COLUMNS) as f32);
        let (columns, rows) = (ATLAS_COLUMNS as f32, atlas_rows() as f32);
        assert!(u0 > column / columns && u1 < (column + 1.0) / columns, "({u0}, {u1})");
        assert!(v0 > row / rows && v1 < (row + 1.0) / rows, "({v0}, {v1})");
    }

    #[test]
    fn the_muted_speaker_is_crossed_and_the_live_one_has_waves() {
        let live = glyph_artwork(ChromeGlyph::Speaker { muted: false }, 64);
        let muted = glyph_artwork(ChromeGlyph::Speaker { muted: true }, 64);
        assert!(live != muted);
        // The cross's centre, (0.74, 0.5), is inked only when muted; the outer wave's crest, (0.82, 0.5), only when live.
        assert!(muted.get_pixel(47, 32)[3] > 128 && live.get_pixel(47, 32)[3] < 128);
        assert!(live.get_pixel(52, 32)[3] > 128);
    }

    #[test]
    fn the_muted_puck_is_grey_and_the_live_one_blue() {
        let live = glyph_artwork(ChromeGlyph::Puck { muted: false }, 64);
        let muted = glyph_artwork(ChromeGlyph::Puck { muted: true }, 64);
        let (l, m) = (live.get_pixel(12, 32), muted.get_pixel(12, 32));
        assert!(l[2] > 200 && l[0] < 80, "{l:?}");
        assert!((i16::from(m[2]) - i16::from(m[0])).abs() < 40, "{m:?}");
    }

    #[test]
    fn pause_is_two_bars_and_resume_a_dark_triangle() {
        let pause = glyph_artwork(ChromeGlyph::Pause, 64);
        let resume = glyph_artwork(ChromeGlyph::Resume, 64);
        // Between the bars is empty; the triangle's middle is inked.
        assert!(pause.get_pixel(32, 32)[3] < 128 && pause.get_pixel(23, 32)[3] > 128 && pause.get_pixel(41, 32)[3] > 128);
        assert!(resume.get_pixel(32, 32)[3] > 128);
        assert!(resume.get_pixel(32, 32)[0] < 80, "dark, for the light button");
    }
}
