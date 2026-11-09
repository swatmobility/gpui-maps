//! **Flat world projections** (spike): Equal Earth and Robinson drawn
//! from the same Mercator tiles the globe draws.
//!
//! # The seam
//!
//! The globe already reprojects every Mercator tile per vertex:
//! `render::subdivide` densifies tile geometry on the CPU, and
//! `shader.wgsl`'s `project_to_sphere` maps each vertex's Mercator position
//! to a unit-sphere point that `GlobeFrame::globe_view_proj` takes to clip
//! space. A flat world projection is the same pipeline with the sphere
//! point replaced by a PLANE point `(x, y, 0)` in projection units (radius
//! 1, so Equal Earth spans x in +-2.7066), and the globe matrix replaced by
//! a camera looking straight at that plane. Labels, the cover and picking
//! reach the sphere only through [`crate::render::globe::mercator_to_sphere`]
//! and `sphere_point_to_mercator`, so they follow for free.
//!
//! # Spike shortcut, NOT the shape to ship
//!
//! The active projection is one process-wide atomic ([`set_active`]),
//! because `Camera` has ~300 struct literals and the spike must not touch
//! them. The production design carries a `Projection` value on the frame
//! (replacing `globe_enabled: bool`), see the plan.
//!
//! The shader learns the projection from `Model::clipping_plane`: a planar
//! frame writes `(0, 0, 0, code / 4)`. The globe's `w` is never positive,
//! so `w > 0.1` is the branch the shader keys on; with a zero normal the
//! far-side clip is the constant `1 - w`, and nothing is clipped.
//!
//! # Formulas
//!
//! Equal Earth: Šavrič, Patterson & Jenny (2018), "The Equal Earth map
//! projection", IJGIS 33(3). Robinson: the PROJ table (`robin.cpp`, MIT),
//! as published by Robinson (1974) and tabulated by Snyder (1990); see
//! NOTICE.

use std::f64::consts::PI;
use std::sync::atomic::{AtomicU8, Ordering};

/// A flat world projection the renderer can draw Mercator tiles in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlatProjection {
    EqualEarth,
    Robinson,
}

impl FlatProjection {
    /// The code the shader keys on (`Model::clipping_plane.w * 4`). 0 = none.
    #[must_use]
    pub fn shader_code(self) -> u8 {
        match self {
            Self::EqualEarth => 1,
            Self::Robinson => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::EqualEarth),
            2 => Some(Self::Robinson),
            _ => None,
        }
    }

    /// `"equal-earth"` / `"robinson"` (style JSON `projection.type`), plus
    /// the PROJ short names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "equal-earth" | "equalEarth" | "eqearth" | "+proj=eqearth" => Some(Self::EqualEarth),
            "robinson" | "robin" | "+proj=robin" => Some(Self::Robinson),
            _ => None,
        }
    }

    /// lon/lat in radians -> plane `(x, y)`, y north-up, radius 1.
    #[must_use]
    pub fn forward(self, lon: f64, lat: f64) -> (f64, f64) {
        match self {
            Self::EqualEarth => equal_earth_forward(lon, lat),
            Self::Robinson => robinson_forward(lon, lat),
        }
    }

    /// Plane `(x, y)` -> lon/lat in radians, or `None` outside the
    /// projection's outline.
    #[must_use]
    pub fn inverse(self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (lon, lat) = match self {
            Self::EqualEarth => equal_earth_inverse(x, y)?,
            Self::Robinson => robinson_inverse(x, y)?,
        };
        (lon.abs() <= PI + 1e-9 && lat.abs() <= PI / 2.0 + 1e-9).then_some((lon, lat))
    }

    /// The projection's world outline: the two bounding meridians and,
    /// for these flat-pole projections, the pole lines. Closed ring, lon/lat
    /// degrees, counter-clockwise from the south-west corner.
    #[must_use]
    pub fn outline_lon_lat_deg(self, steps: usize) -> Vec<(f64, f64)> {
        let steps = steps.max(2);
        let mut ring = Vec::with_capacity(2 * steps + 3);
        for i in 0..=steps {
            ring.push((180.0, -90.0 + 180.0 * i as f64 / steps as f64));
        }
        for i in 0..=steps {
            ring.push((-180.0, 90.0 - 180.0 * i as f64 / steps as f64));
        }
        ring.push(ring[0]);
        ring
    }
}

static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Switch the process to a flat projection (or back to the globe/Mercator
/// with `None`). Spike-only; see the module docs.
pub fn set_active(projection: Option<FlatProjection>) {
    ACTIVE.store(projection.map_or(0, FlatProjection::shader_code), Ordering::Relaxed);
}

/// The process's active flat projection, if any.
#[must_use]
pub fn active() -> Option<FlatProjection> {
    FlatProjection::from_code(ACTIVE.load(Ordering::Relaxed))
}

/// Mercator `[0, 1]^2` (y down) -> lon/lat radians. Not wrapped: x = 1.0
/// is lon +180, the RIGHT edge of a flat world, not the left.
#[must_use]
pub fn mercator_to_lon_lat(mx: f64, my: f64) -> (f64, f64) {
    let lon = mx * 2.0 * PI - PI;
    let lat = 2.0 * (PI - my * 2.0 * PI).exp().atan() - PI / 2.0;
    (lon, lat)
}

/// lon/lat radians -> Mercator `[0, 1]^2`, latitude clamped to the
/// Mercator square.
#[must_use]
pub fn lon_lat_to_mercator(lon: f64, lat: f64) -> (f64, f64) {
    let lat = lat.clamp(-85.051_129f64.to_radians(), 85.051_129f64.to_radians());
    let x = (lon + PI) / (2.0 * PI);
    let y = (1.0 - (PI / 4.0 + lat / 2.0).tan().ln() / PI) / 2.0;
    (x, y)
}

// ---- Equal Earth ----------------------------------------------------------

const EE_A1: f64 = 1.340_264;
const EE_A2: f64 = -0.081_106;
const EE_A3: f64 = 0.000_893;
const EE_A4: f64 = 0.003_796;
const EE_M: f64 = 0.866_025_403_784_438_6; // sqrt(3) / 2

fn equal_earth_forward(lon: f64, lat: f64) -> (f64, f64) {
    let theta = (EE_M * lat.sin()).asin();
    let t2 = theta * theta;
    let t6 = t2 * t2 * t2;
    let x = lon * theta.cos() / (EE_M * (EE_A1 + 3.0 * EE_A2 * t2 + t6 * (7.0 * EE_A3 + 9.0 * EE_A4 * t2)));
    let y = theta * (EE_A1 + EE_A2 * t2 + t6 * (EE_A3 + EE_A4 * t2));
    (x, y)
}

fn equal_earth_inverse(x: f64, y: f64) -> Option<(f64, f64)> {
    let mut theta = y / EE_A1;
    for _ in 0..12 {
        let t2 = theta * theta;
        let t6 = t2 * t2 * t2;
        let f = theta * (EE_A1 + EE_A2 * t2 + t6 * (EE_A3 + EE_A4 * t2)) - y;
        let fp = EE_A1 + 3.0 * EE_A2 * t2 + t6 * (7.0 * EE_A3 + 9.0 * EE_A4 * t2);
        let delta = f / fp;
        theta -= delta;
        if delta.abs() < 1e-12 {
            break;
        }
    }
    let s = theta.sin() / EE_M;
    if s.is_nan() || s.abs() > 1.0 + 1e-12 {
        return None;
    }
    let t2 = theta * theta;
    let t6 = t2 * t2 * t2;
    let lon = EE_M * x * (EE_A1 + 3.0 * EE_A2 * t2 + t6 * (7.0 * EE_A3 + 9.0 * EE_A4 * t2)) / theta.cos();
    Some((lon, s.clamp(-1.0, 1.0).asin()))
}

// ---- Robinson -------------------------------------------------------------

/// Robinson's table every 5 degrees of latitude: `(X, Y)` with x =
/// FXC * X * lon and y = FYC * Y (PROJ `robin.cpp`, MIT).
const ROBIN: [(f64, f64); 19] = [
    (1.0000, 0.0000),
    (0.9986, 0.0620),
    (0.9954, 0.1240),
    (0.9900, 0.1860),
    (0.9822, 0.2480),
    (0.9730, 0.3100),
    (0.9600, 0.3720),
    (0.9427, 0.4340),
    (0.9216, 0.4958),
    (0.8962, 0.5571),
    (0.8679, 0.6176),
    (0.8350, 0.6769),
    (0.7986, 0.7346),
    (0.7597, 0.7903),
    (0.7186, 0.8435),
    (0.6732, 0.8936),
    (0.6213, 0.9394),
    (0.5722, 0.9761),
    (0.5322, 1.0000),
];
const ROBIN_FXC: f64 = 0.8487;
const ROBIN_FYC: f64 = 1.3523;

/// Linear-in-index interpolation of the table at |lat| (radians). PROJ
/// uses a cubic; linear is within 0.2% here and is what the shader does.
fn robin_xy(abs_lat: f64) -> (f64, f64) {
    let u = (abs_lat.to_degrees() / 5.0).clamp(0.0, 18.0);
    let i = (u.floor() as usize).min(17);
    let f = u - i as f64;
    let (x0, y0) = ROBIN[i];
    let (x1, y1) = ROBIN[i + 1];
    (x0 + (x1 - x0) * f, y0 + (y1 - y0) * f)
}

fn robinson_forward(lon: f64, lat: f64) -> (f64, f64) {
    let (tx, ty) = robin_xy(lat.abs());
    (ROBIN_FXC * tx * lon, ROBIN_FYC * ty * lat.signum())
}

fn robinson_inverse(x: f64, y: f64) -> Option<(f64, f64)> {
    let ty = y.abs() / ROBIN_FYC;
    if ty > 1.0 + 1e-12 {
        return None;
    }
    // Y is monotonic in the table: find the segment, invert linearly.
    let mut i = 0;
    while i < 17 && ROBIN[i + 1].1 < ty {
        i += 1;
    }
    let (_, y0) = ROBIN[i];
    let (_, y1) = ROBIN[i + 1];
    let f = ((ty - y0) / (y1 - y0)).clamp(0.0, 1.0);
    let lat = ((i as f64 + f) * 5.0).to_radians() * y.signum();
    let (tx, _) = robin_xy(lat.abs());
    Some((x / (ROBIN_FXC * tx), lat))
}

/// **The shader source for the frame's projection family.** Without an
/// active flat projection, every `// @flat-begin` .. `// @flat-end` block is
/// cut out, so the globe and Mercator pipelines compile from exactly the
/// source they compiled from before flat projections existed -- the
/// per-vertex branch must not cost (or perturb, under Metal's fast math)
/// the paths every map draws today. A flat projection keeps the blocks.
///
/// Spike: keyed on the process-wide [`active`] projection when the pipeline
/// is built. The plan makes it a pipeline variant per projection family.
#[must_use]
pub fn specialise_wgsl(source: &str) -> std::borrow::Cow<'_, str> {
    if active().is_some() || !source.contains("// @flat-begin") {
        return std::borrow::Cow::Borrowed(source);
    }
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("// @flat-begin") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after.find("// @flat-end").expect("an unclosed // @flat-begin block");
        rest = &after[end + "// @flat-end".len()..];
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_earth_matches_the_published_extent_and_round_trips() {
        let (x, _) = FlatProjection::EqualEarth.forward(PI, 0.0);
        assert!((x - 2.706_6).abs() < 1e-3, "{x}");
        let (_, y) = FlatProjection::EqualEarth.forward(0.0, PI / 2.0);
        assert!((y - 1.317_3).abs() < 1e-3, "{y}");
        for &(lon, lat) in &[(0.3, 0.2), (-2.9, 1.2), (3.0, -1.4), (0.0, 0.0)] {
            let (x, y) = FlatProjection::EqualEarth.forward(lon, lat);
            let (l2, p2) = FlatProjection::EqualEarth.inverse(x, y).unwrap();
            assert!((l2 - lon).abs() < 1e-9 && (p2 - lat).abs() < 1e-9);
        }
        assert!(FlatProjection::EqualEarth.inverse(2.8, 0.0).is_none());
    }

    #[test]
    fn robinson_matches_the_published_extent_and_round_trips() {
        let (x, _) = FlatProjection::Robinson.forward(PI, 0.0);
        assert!((x - 0.8487 * PI).abs() < 1e-9);
        for &(lon, lat) in &[(0.3, 0.2), (-2.9, 1.2), (3.0, -1.4)] {
            let (x, y) = FlatProjection::Robinson.forward(lon, lat);
            let (l2, p2) = FlatProjection::Robinson.inverse(x, y).unwrap();
            assert!((l2 - lon).abs() < 1e-9 && (p2 - lat).abs() < 1e-9, "{l2} {p2}");
        }
    }

    #[test]
    fn without_a_flat_projection_the_shaders_compile_from_their_pre_projection_source() {
        let src = "a\n    // @flat-begin\nflat\n    // @flat-end\nb";
        assert_eq!(specialise_wgsl(src), "a\n    \nb");
        for shader in [include_str!("render/shader.wgsl"), include_str!("render/raster_shader.wgsl")] {
            let cut = specialise_wgsl(shader);
            assert!(!cut.contains("project_to_flat") && !cut.contains("flat_pole") && !cut.contains("ROBIN_TABLE"));
        }
    }

    #[test]
    fn mercator_round_trips() {
        let (lon, lat) = mercator_to_lon_lat(0.25, 0.4);
        let (x, y) = lon_lat_to_mercator(lon, lat);
        assert!((x - 0.25).abs() < 1e-12 && (y - 0.4).abs() < 1e-12);
    }
}
