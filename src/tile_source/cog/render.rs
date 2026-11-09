//! Resampling: the decoded internal tiles of one level into a map tile.
//!
//! Both mapped CRSs are separable on the Web Mercator grid -- a map
//! column is one source x, a map row one source y (EPSG:3857 linearly,
//! EPSG:4326 through the inverse Mercator of the row's latitude) -- so the
//! plan carries one coordinate per column and per row, and the inner loop
//! is a table lookup and a weighted sum.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Cog, CogRendering, CogResampling, Key, Pixels, Plan, HALF_WORLD};

/// One level's internal tiles, addressed by source pixel.
struct Grid<'a> {
    tiles: Vec<Option<&'a Pixels>>,
    across: u32,
    tw: u32,
    th: u32,
}

impl Grid<'_> {
    fn tile(&self, x: u32, y: u32) -> Option<(&Pixels, usize)> {
        let (tx, ty) = (x / self.tw, y / self.th);
        let t = self.tiles.get((ty * self.across + tx) as usize).copied().flatten()?;
        Some((t, ((y % self.th) * self.tw + (x % self.tw)) as usize))
    }
    fn rgba(&self, x: u32, y: u32) -> [f32; 4] {
        match self.tile(x, y) {
            Some((Pixels::Rgba(px), i)) => {
                let p = &px[i * 4..i * 4 + 4];
                [f32::from(p[0]), f32::from(p[1]), f32::from(p[2]), f32::from(p[3])]
            }
            _ => [0.0; 4],
        }
    }
    fn value(&self, x: u32, y: u32) -> f32 {
        match self.tile(x, y) {
            Some((Pixels::Values(v), i)) => v[i],
            _ => f32::NAN,
        }
    }
}

/// Per output coordinate: the two source pixels and the weight of the
/// second, or `None` outside the image.
type Axis = Vec<Option<(u32, u32, f32)>>;

fn grid_and_axes<'a>(cog: &Cog, plan: &Plan, have: &'a HashMap<Key, Arc<Pixels>>) -> (Grid<'a>, Axis, Axis) {
    let lay = &cog.levels[plan.level];
    let per_plane = lay.tiles_across * lay.tiles_down;
    let plane_base = plan.keys.first().map_or(0, |k| k.2 - k.2 % per_plane);
    let mut tiles = vec![None; per_plane as usize];
    for (key, px) in have {
        if key.0 as usize == plan.level {
            if let Some(slot) = tiles.get_mut((key.2 - plane_base) as usize) {
                *slot = Some(px.as_ref());
            }
        }
    }
    let grid = Grid { tiles, across: lay.tiles_across, tw: cog.format.tile_w, th: cog.format.tile_h };
    let bilinear = cog.options.resampling == CogResampling::Bilinear;
    let axis = |coords: &[f64], extent: f64| -> Axis {
        coords
            .iter()
            .map(|&c| {
                if c < -0.5 || c > extent - 0.5 {
                    return None;
                }
                let max = extent - 1.0;
                if bilinear {
                    let c = c.clamp(0.0, max);
                    let lo = c.floor();
                    Some((lo as u32, (lo + 1.0).min(max) as u32, (c - lo) as f32))
                } else {
                    let n = c.round().clamp(0.0, max) as u32;
                    Some((n, n, 0.0))
                }
            })
            .collect()
    };
    let cols = axis(&plan.xs, f64::from(lay.width));
    let rows = axis(&plan.ys, f64::from(lay.height));
    (grid, cols, rows)
}

/// The resampled values over the plan's grid, border included:
/// `(size + 2)^2`, row-major, NaN outside the image and at nodata.
pub(super) fn values(cog: &Cog, plan: &Plan, have: &HashMap<Key, Arc<Pixels>>) -> Vec<f32> {
    let (grid, cols, rows) = grid_and_axes(cog, plan, have);
    let mut out = Vec::with_capacity(cols.len() * rows.len());
    for row in &rows {
        for col in &cols {
            let (Some((y0, y1, fy)), Some((x0, x1, fx))) = (*row, *col) else {
                out.push(f32::NAN);
                continue;
            };
            let v = if fx == 0.0 && fy == 0.0 {
                grid.value(x0, y0)
            } else {
                let (a, b, c, d) = (grid.value(x0, y0), grid.value(x1, y0), grid.value(x0, y1), grid.value(x1, y1));
                if a.is_nan() || b.is_nan() || c.is_nan() || d.is_nan() {
                    // At a nodata edge: the nearest, or nothing.
                    let nx = if fx < 0.5 { x0 } else { x1 };
                    let ny = if fy < 0.5 { y0 } else { y1 };
                    grid.value(nx, ny)
                } else {
                    let top = a + (b - a) * fx;
                    let bottom = c + (d - c) * fx;
                    top + (bottom - top) * fy
                }
            };
            out.push(v);
        }
    }
    out
}

pub(super) fn render(cog: &Cog, plan: &Plan, have: &HashMap<Key, Arc<Pixels>>) -> image::RgbaImage {
    let size = plan.size as usize;
    let mut out = vec![0u8; size * size * 4];
    match &cog.meta.rendering {
        CogRendering::Rgb { .. } => {
            let (grid, cols, rows) = grid_and_axes(cog, plan, have);
            for (j, row) in rows[1..=size].iter().enumerate() {
                let Some((y0, y1, fy)) = *row else { continue };
                for (i, col) in cols[1..=size].iter().enumerate() {
                    let Some((x0, x1, fx)) = *col else { continue };
                    let px = if fx == 0.0 && fy == 0.0 {
                        grid.rgba(x0, y0)
                    } else {
                        let (a, b, c, d) = (grid.rgba(x0, y0), grid.rgba(x1, y0), grid.rgba(x0, y1), grid.rgba(x1, y1));
                        let mut p = [0.0f32; 4];
                        for k in 0..4 {
                            let top = a[k] + (b[k] - a[k]) * fx;
                            let bottom = c[k] + (d[k] - c[k]) * fx;
                            p[k] = top + (bottom - top) * fy;
                        }
                        p
                    };
                    // Premultiplied in the tile; straight alpha out.
                    let alpha = px[3];
                    if alpha <= 0.5 {
                        continue;
                    }
                    let o = (j * size + i) * 4;
                    let un = 255.0 / alpha;
                    out[o] = (px[0] * un).round().clamp(0.0, 255.0) as u8;
                    out[o + 1] = (px[1] * un).round().clamp(0.0, 255.0) as u8;
                    out[o + 2] = (px[2] * un).round().clamp(0.0, 255.0) as u8;
                    out[o + 3] = alpha.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        CogRendering::Ramp { min, max, .. } => {
            let v = values(cog, plan, have);
            let stride = size + 2;
            let (min, span) = (*min as f32, (*max - *min) as f32);
            let scale = if span.abs() > f32::EPSILON { 255.0 / span } else { 0.0 };
            let shade = cog.options.hillshade.map(|exaggeration| Shade::new(plan, exaggeration));
            for j in 0..size {
                for i in 0..size {
                    let at = (j + 1) * stride + i + 1;
                    let value = v[at];
                    if value.is_nan() {
                        continue;
                    }
                    let t = ((value - min) * scale).clamp(0.0, 255.0) as usize;
                    let mut c = cog.lut[t];
                    if let Some(shade) = &shade {
                        let f = shade.factor(&v, at, stride, j);
                        for ch in &mut c[..3] {
                            *ch = (f32::from(*ch) * f).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                    let o = (j * size + i) * 4;
                    out[o..o + 4].copy_from_slice(&c);
                }
            }
        }
    }
    image::RgbaImage::from_raw(plan.size, plan.size, out).expect("size * size * 4 bytes")
}

/// A hillshade over the resampled values: light from the north-west
/// (azimuth 315 deg) at 45 deg, slopes from the map grid's own ground
/// spacing (Mercator pixel size times the row's cos(latitude)), so the
/// relief reads the same at every zoom. Values are taken as metres.
struct Shade {
    /// Ground metres per map pixel, per output row.
    spacing: Vec<f32>,
    exaggeration: f32,
    light: [f32; 3],
}

impl Shade {
    fn new(plan: &Plan, exaggeration: f32) -> Self {
        let n = f64::from(plan.size) * 2f64.powi(i32::from(plan.tile.z));
        let equator = 2.0 * HALF_WORLD / n;
        let spacing = (0..plan.size)
            .map(|j| {
                let v = (f64::from(plan.tile.y) * f64::from(plan.size) + f64::from(j) + 0.5) / n;
                let lat = (std::f64::consts::PI * (1.0 - 2.0 * v)).sinh().atan();
                (equator * lat.cos()) as f32
            })
            .collect();
        let (az, alt) = (315f32.to_radians(), 45f32.to_radians());
        Self { spacing, exaggeration, light: [alt.cos() * az.sin(), alt.cos() * az.cos(), alt.sin()] }
    }

    /// The brightness factor at `at` (output row `j`): about 1 on flat
    /// ground, darker facing away from the light, brighter toward it.
    fn factor(&self, v: &[f32], at: usize, stride: usize, j: usize) -> f32 {
        let centre = v[at];
        let get = |k: usize| if v[k].is_nan() { centre } else { v[k] };
        let c = self.spacing[j].max(1e-3);
        let dzdx = (get(at + 1) - get(at - 1)) / (2.0 * c) * self.exaggeration;
        // Row j - 1 is north.
        let dzdy = (get(at - stride) - get(at + stride)) / (2.0 * c) * self.exaggeration;
        let norm = (dzdx * dzdx + dzdy * dzdy + 1.0).sqrt();
        let lit = (-dzdx * self.light[0] - dzdy * self.light[1] + self.light[2]) / norm;
        (0.35 + 0.92 * lit.max(0.0)).min(1.3)
    }
}
