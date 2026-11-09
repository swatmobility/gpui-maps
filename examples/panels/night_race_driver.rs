//! **The cockpit's insides and the steering wheel as 3D**, for the in-car
//! view's first-person near pass (`gpui_maps::render::model::ModelNearPass`):
//! a ROUND wheel (a rim 0.28 m across with the grips thickened into it, a
//! slim centre hub holding the screen, rotaries and buttons, three spokes,
//! the column), the flat padded rims along the opening and across its
//! front, the planar inner sides and the dash. One mesh in the car's frame
//! (`night_race_car_dims.rs`: metres, +x forward, +y left, +z up, the
//! origin on the ground midway between the axles), drawn as one model
//! layer over the player's own car (`night_race_car_mesh.rs`'s
//! `own_car_mesh`). No driver's body: the in-car view shows the car and
//! the wheel only.
//!
//! **Moving parts** (the engine's per-part transforms, [`rig`]): the wheel
//! ([`PART_WHEEL`]) turned about the column by the steering angle
//! ([`wheel_matrix`]); and, on the car's own mesh, the front wheels
//! steered by that angle over [`STEER_RATIO`].
#![allow(dead_code)]

use gpui_maps::render::model::{ModelMaterial, ModelMesh, PartMatrix, MAX_MATERIALS};
use gpui_maps::style::Rgba;

use super::car_dims as d;
use super::car_mesh::{section, Builder, Side, V3};

/// Materials.
pub const BUTTON: u32 = 0;
pub const PANEL: u32 = 1;
pub const CARBON: u32 = 2;
pub const SCREEN: u32 = 3;
pub const ACCENT: u32 = 4;
pub const PAD: u32 = 5;
pub const GRIP: u32 = 6;

/// The wheel's part, and the part table's length.
pub const PART_WHEEL: u32 = 1;
pub const PARTS: usize = 1;

/// Front wheels' steer = steering-wheel angle / this.
pub const STEER_RATIO: f64 = 11.0;

/// The round wheel: the rim's outer diameter is [`d::WHEEL_W`] (0.28 m);
/// its tube's radius, thicker over the grips (within [`GRIP_SPAN_DEG`] of
/// 9 and 3 o'clock); the rim's plane sits this far behind the hub's face.
pub const RIM_TUBE_R: f64 = 0.012;
pub const GRIP_TUBE_R: f64 = 0.019;
pub const GRIP_SPAN_DEG: f64 = 38.0;
pub const RIM_W: f64 = -0.018;
/// The rim's centreline radius.
pub const RIM_R: f64 = d::WHEEL_W / 2.0 - GRIP_TUBE_R;
/// The hub: across, from its foot to its top (metres, in the face).
pub const HUB_U: f64 = 0.062;
pub const HUB_V: (f64, f64) = (-0.048, 0.034);

type P = [f64; 3];

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn mul(a: P, s: f64) -> P {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: P, b: P) -> P {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn len(a: P) -> f64 {
    dot(a, a).sqrt()
}
fn unit(a: P) -> P {
    mul(a, 1.0 / len(a).max(1e-12))
}
/// `p` on the car's other side.
fn flip(p: P) -> P {
    [p[0], -p[1], p[2]]
}
fn f32x3(p: P) -> V3 {
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

// ---------------------------------------------------------------------------
// The wheel's frame
// ---------------------------------------------------------------------------

/// The wheel's axes in the car: to the driver's right, up its face, and
/// off its face toward the driver (the column's axis). The wheel leans
/// back [`d::WHEEL_LEAN_DEG`] like a reclined seat back -- its top
/// further from the driver than its foot -- so the face looks up the
/// column at the driver's eye.
#[must_use]
pub fn axes() -> (P, P, P) {
    let l = d::WHEEL_LEAN_DEG.to_radians();
    ([0.0, -1.0, 0.0], [l.sin(), 0.0, l.cos()], [-l.cos(), 0.0, l.sin()])
}

/// A point of the wheel -- `u` metres to the driver's right of the column,
/// `v` up the face, `w` off the face toward the driver -- turned by `angle`
/// (radians, right positive: the top goes right) about the column, in the
/// car's frame.
#[must_use]
pub fn wheel_point(u: f64, v: f64, w: f64, angle: f64) -> P {
    let (s, c) = angle.sin_cos();
    let (right, up) = (u * c + v * s, -u * s + v * c);
    let (r, vv, n) = axes();
    add(d::WHEEL_C, add(mul(r, right), add(mul(vv, up), mul(n, w))))
}

/// The wheel's part matrix at `angle`: a turn about the column through the
/// wheel's centre.
#[must_use]
pub fn wheel_matrix(angle: f64) -> PartMatrix {
    let (r, vv, n) = axes();
    let (s, c) = angle.sin_cos();
    // The image of a vector `e`: its (right, up, column) coordinates
    // turned in the face's plane.
    let turn = |e: P| -> P {
        let (x, y, z) = (dot(e, r), dot(e, vv), dot(e, n));
        add(mul(r, x * c + y * s), add(mul(vv, -x * s + y * c), mul(n, z)))
    };
    let cols = [turn([1.0, 0.0, 0.0]), turn([0.0, 1.0, 0.0]), turn([0.0, 0.0, 1.0])];
    let rc = add(mul(cols[0], d::WHEEL_C[0]), add(mul(cols[1], d::WHEEL_C[1]), mul(cols[2], d::WHEEL_C[2])));
    let t = sub(d::WHEEL_C, rc);
    std::array::from_fn(|k| [cols[0][k] as f32, cols[1][k] as f32, cols[2][k] as f32, t[k] as f32])
}

/// `m` applied to `p`.
#[must_use]
pub fn apply(m: &PartMatrix, p: P) -> P {
    gpui_maps::render::model::part_point(m, p)
}

/// **The rig at steering angle `angle_deg`** (right positive): the
/// cockpit layer's part table (the wheel) and the car's (the front
/// wheels, steered by [`STEER_RATIO`]).
#[must_use]
pub fn rig(angle_deg: f64) -> (Vec<PartMatrix>, Vec<PartMatrix>) {
    let a = angle_deg.to_radians();
    let driver = vec![wheel_matrix(a)];
    // The front wheels turn the way the wheel does: right is clockwise
    // seen from above.
    let steer = -a / STEER_RATIO;
    let wheel = |y: f64| -> PartMatrix {
        let (s, c) = steer.sin_cos();
        let (cx, cy) = (d::FRONT_AXLE_X, y);
        [
            [c as f32, -s as f32, 0.0, (cx - c * cx + s * cy) as f32],
            [s as f32, c as f32, 0.0, (cy - s * cx - c * cy) as f32],
            [0.0, 0.0, 1.0, 0.0],
        ]
    };
    (driver, vec![wheel(d::FRONT_TYRE_Y), wheel(-d::FRONT_TYRE_Y)])
}

// ---------------------------------------------------------------------------
// The mesh
// ---------------------------------------------------------------------------

fn materials() -> Vec<ModelMaterial> {
    let m = |r: f32, g: f32, b: f32, gloss: f32| ModelMaterial::new(Rgba { r, g, b, a: 1.0 }, gloss);
    vec![
        m(0.30, 0.31, 0.34, 0.30),    // BUTTON
        m(0.20, 0.38, 0.60, 0.22),    // PANEL
        m(0.075, 0.08, 0.09, 0.55),   // CARBON
        m(0.05, 0.22, 0.16, 0.90),    // SCREEN
        m(0.95, 0.56, 0.16, 0.40),    // ACCENT
        m(0.085, 0.087, 0.10, 0.06),  // PAD
        m(0.045, 0.046, 0.05, 0.12),  // GRIP (rubber)
    ]
}

/// A tube along `path` with a radius per point, `sides` round, smooth
/// round its circumference, its ends capped; `mat` picks each vertex's
/// material from its outward normal.
fn tube(b: &mut Builder, path: &[P], radii: &[f64], sides: usize, mat: &dyn Fn(P) -> u32) {
    let rings: Vec<(Vec<V3>, Vec<V3>)> = (0..path.len())
        .map(|k| {
            let a = path[k.saturating_sub(1)];
            let c = path[(k + 1).min(path.len() - 1)];
            let t = unit(sub(c, a));
            let mut s = cross(t, [0.0, 0.0, 1.0]);
            if len(s) < 0.3 {
                s = cross(t, [1.0, 0.0, 0.0]);
            }
            let s = unit(s);
            let u = cross(s, t);
            let mut pts = Vec::new();
            let mut nrm = Vec::new();
            for j in 0..sides {
                let ph = std::f64::consts::TAU * j as f64 / sides as f64;
                let n = add(mul(s, ph.cos()), mul(u, ph.sin()));
                pts.push(f32x3(add(path[k], mul(n, radii[k]))));
                nrm.push(f32x3(n));
            }
            (pts, nrm)
        })
        .collect();
    let m = |n: V3| mat([f64::from(n[0]), f64::from(n[1]), f64::from(n[2])]);
    for k in 0..rings.len() - 1 {
        let ((pa, na), (pb, nb)) = (&rings[k], &rings[k + 1]);
        for j in 0..sides {
            let i = (j + 1) % sides;
            b.tri_smooth([pa[j], pa[i], pb[i]], [na[j], na[i], nb[i]], m(na[j]));
            b.tri_smooth([pa[j], pb[i], pb[j]], [na[j], nb[i], nb[j]], m(na[j]));
        }
    }
    let t0 = f32x3(sub(path[0], path[1]));
    let t1 = f32x3(sub(path[path.len() - 1], path[path.len() - 2]));
    b.face(&rings[0].0, m(t0), t0);
    b.face(&rings[rings.len() - 1].0, m(t1), t1);
}

/// A slab of the wheel: `outline` in its face (metres, `u` right, `v`
/// up), from `w0` to `w1` off the face, at rest.
fn slab(b: &mut Builder, outline: &[(f64, f64)], (w0, w1): (f64, f64), m: u32) {
    let ring = |w: f64| -> Vec<V3> { outline.iter().map(|&(u, v)| f32x3(wheel_point(u, v, w, 0.0))).collect() };
    let (front, back) = (ring(w1), ring(w0));
    let (_, _, n) = axes();
    b.face(&front, m, f32x3(n));
    b.face(&back, m, f32x3(mul(n, -1.0)));
    b.loft(&back, &front, m, Side::Out);
}

/// A rounded rectangle, anticlockwise.
fn rounded(u: (f64, f64), v: (f64, f64), r: f64) -> Vec<(f64, f64)> {
    let corners = [(u.1 - r, v.0 + r, -90.0f64), (u.1 - r, v.1 - r, 0.0), (u.0 + r, v.1 - r, 90.0), (u.0 + r, v.0 + r, 180.0)];
    corners
        .iter()
        .flat_map(|&(cu, cv, a0)| {
            (0..=3).map(move |k| {
                let a = (a0 + 30.0 * f64::from(k)).to_radians();
                (cu + r * a.cos(), cv + r * a.sin())
            })
        })
        .collect()
}

/// **The round steering wheel** at rest, all in [`PART_WHEEL`]: the rim
/// (a ring of tube, thicker and rubber over the grips at 9 and 3 o'clock),
/// three spokes to the hub, the hub with its screen, rotaries and
/// buttons; and the column (rigid with the car).
fn wheel(b: &mut Builder) {
    b.part = PART_WHEEL;
    // The rim: a ring of rings round the face.
    let (r, vv, n) = axes();
    let segs: u32 = 48;
    let sides: u32 = 10;
    let grip = |phi: f64| {
        let deg = phi.to_degrees().rem_euclid(180.0);
        let off = deg.min(180.0 - deg);
        off < GRIP_SPAN_DEG
    };
    let rings: Vec<(Vec<V3>, Vec<V3>, u32)> = (0..=segs)
        .map(|k| {
            let phi = std::f64::consts::TAU * f64::from(k) / f64::from(segs);
            let (u, v) = (RIM_R * phi.cos(), RIM_R * phi.sin());
            let c = wheel_point(u, v, RIM_W, 0.0);
            let radial = add(mul(r, phi.cos()), mul(vv, phi.sin()));
            let tr = if grip(phi) { GRIP_TUBE_R } else { RIM_TUBE_R };
            let mut pts = Vec::new();
            let mut nrm = Vec::new();
            for j in 0..sides {
                let a = std::f64::consts::TAU * f64::from(j) / f64::from(sides);
                let nn = add(mul(radial, a.cos()), mul(n, a.sin()));
                pts.push(f32x3(add(c, mul(nn, tr))));
                nrm.push(f32x3(nn));
            }
            (pts, nrm, if grip(phi) { GRIP } else { CARBON })
        })
        .collect();
    for k in 0..segs as usize {
        let ((pa, na, m), (pb, nb, _)) = (&rings[k], &rings[k + 1]);
        for j in 0..sides as usize {
            let i = (j + 1) % sides as usize;
            b.tri_smooth([pa[j], pa[i], pb[i]], [na[j], na[i], nb[i]], *m);
            b.tri_smooth([pa[j], pb[i], pb[j]], [na[j], nb[i], nb[j]], *m);
        }
    }
    // Three spokes: left, right and down, from the hub to the rim.
    for phi in [0.0f64, 180.0, 270.0] {
        let t = phi.to_radians();
        // From inside the hub's edge on that side.
        let from = ((HUB_U - 0.01) * t.cos(), (-HUB_V.0 - 0.01) * t.sin() - 0.004);
        let to = (RIM_R * t.cos(), RIM_R * t.sin());
        let a = wheel_point(from.0, from.1, RIM_W + 0.004, 0.0);
        let c = wheel_point(to.0, to.1, RIM_W, 0.0);
        tube(b, &[a, c], &[0.010, 0.008], 8, &|_| CARBON);
    }
    // The hub: a rounded slab, the screen in a bezel, rotaries, buttons.
    slab(b, &rounded((-HUB_U, HUB_U), HUB_V, 0.014), (-0.030, 0.0), CARBON);
    slab(b, &rounded((-0.042, 0.042), (-0.041, 0.025), 0.005), (0.0, 0.002), CARBON);
    slab(b, &rounded((-0.035, 0.035), (-0.034, 0.0185), 0.003), (0.002, 0.0028), SCREEN);
    for (u, v) in [(-0.052, 0.012), (0.052, 0.012), (-0.052, -0.030), (0.052, -0.030)] {
        let knob: Vec<(f64, f64)> = (0..8)
            .map(|k| {
                let a = std::f64::consts::TAU * (f64::from(k) + 0.5) / 8.0;
                (u + 0.0065 * a.cos(), v + 0.0065 * a.sin())
            })
            .collect();
        slab(b, &knob, (0.0, 0.008), ACCENT);
    }
    for ((u, v), m) in [((-0.024, -0.044), PANEL), ((0.024, -0.044), BUTTON), ((-0.052, 0.028), BUTTON), ((0.052, 0.028), PANEL)] {
        slab(b, &rounded((u - 0.006, u + 0.006), (v - 0.004, v + 0.004), 0.002), (0.0, 0.004), m);
    }
    b.part = 0;
    // The column, from the back of the hub down to the dash.
    let c0 = add(d::WHEEL_C, mul(n, -0.03));
    let c1 = add(d::WHEEL_C, mul(n, -0.24));
    tube(b, &[c0, c1], &[0.017, 0.017], 8, &|_| CARBON);
}

/// A rounded bar along x from `x0` to `x1`: its section a rounded
/// rectangle `hw` half wide (across y, centred `cy`) from `zb` to `zt`.
fn pad_x(b: &mut Builder, (x0, x1): (f64, f64), cy: f64, hw: f64, (zb, zt): (f64, f64), m: u32) {
    let rings: Vec<Vec<V3>> = [x0, x1].iter().map(|&x| section(x as f32, cy as f32, hw as f32, zb as f32, zt as f32, 16)).collect();
    b.loft(&rings[0], &rings[1], m, Side::Out);
    b.face(&rings[0], m, [-1.0, 0.0, 0.0]);
    b.face(&rings[1], m, [1.0, 0.0, 0.0]);
}

/// The cockpit's insides: the flat padded rims along the opening and
/// across its front (wide rounded boxes, not tubes), the planar inner
/// sides and the dash.
fn cockpit(b: &mut Builder) {
    let x0 = d::COCKPIT_X.0;
    let (rim_in, rim_out) = d::RIM_Y;
    let (front0, front1) = d::RIM_FRONT_X;
    let top = d::RIM_TOP_Z;
    for s in [1.0f64, -1.0] {
        // The side pad: 75 mm wide, 30 mm proud, its inner edge on the rim.
        pad_x(b, (x0 + 0.02, front0), s * (rim_in + 0.034), 0.0375, (top - 0.012, top + 0.018), PAD);
        let y = s * (d::COCKPIT_WALL_Y.0 - 0.003);
        let q: [V3; 4] = [[x0 as f32, y as f32, 0.12], [front0 as f32, y as f32, 0.12], [front0 as f32, y as f32, d::COCKPIT_TOP_Z as f32], [x0 as f32, y as f32, d::COCKPIT_TOP_Z as f32]];
        b.face(&q, PAD, [0.0, -s as f32, 0.0]);
    }
    // The front rim's pad across the opening, a flat rounded box.
    let (yw, zb, zt) = (rim_out + 0.005, top - 0.010, top + 0.016);
    let ring = |y: f64| -> Vec<V3> {
        (0..16)
            .map(|k| {
                let t = std::f64::consts::TAU * (f64::from(k) + 0.5) / 16.0;
                let sp = |v: f64| v.signum() * v.abs().powf(0.55);
                let xc = 0.5 * (front0 + front1) - 0.01;
                [(xc + 0.035 * sp(t.cos())) as f32, y as f32, (0.5 * (zb + zt) + 0.5 * (zt - zb) * sp(t.sin())) as f32]
            })
            .collect()
    };
    let (ra, rb) = (ring(-yw), ring(yw));
    b.loft(&ra, &rb, PAD, Side::Out);
    b.face(&ra, PAD, [0.0, -1.0, 0.0]);
    b.face(&rb, PAD, [0.0, 1.0, 0.0]);
    let w = d::COCKPIT_WALL_Y.0 as f32;
    let fx = (front0 - 0.005) as f32;
    b.face(&[[fx, -w, 0.12], [fx, w, 0.12], [fx, w, (d::COCKPIT_TOP_Z - 0.01) as f32], [fx, -w, (d::COCKPIT_TOP_Z - 0.01) as f32]], CARBON, [-1.0, 0.0, 0.0]);
}

/// **The cockpit mesh** (see the module docs): the cockpit's insides and
/// the round wheel. Deterministic.
#[must_use]
pub fn driver_mesh() -> ModelMesh {
    let mut b = Builder::default();
    cockpit(&mut b);
    wheel(&mut b);
    let materials = materials();
    debug_assert!(materials.len() <= MAX_MATERIALS);
    ModelMesh { vertices: b.v, indices: b.i, materials }
}

/// A car-frame point at rest back to the wheel's frame `(u, v, w)`.
#[must_use]
pub fn to_wheel(p: P) -> P {
    let (r, vv, n) = axes();
    let q = sub(p, d::WHEEL_C);
    [dot(q, r), dot(q, vv), dot(q, n)]
}

/// The night key light for the cockpit's layer: the floodlights high to
/// one side, a stronger sky fill than the street's cars get (so the dark
/// cockpit reads as shapes, not a void), and a soft rim.
pub const LIGHT: gpui_maps::render::model::ModelLight = gpui_maps::render::model::ModelLight {
    sun_dir: [0.30, -0.35, 0.89],
    sun_color: Rgba { r: 1.0, g: 0.95, b: 0.88, a: 1.0 },
    sun_intensity: 1.05,
    ambient: Rgba { r: 0.62, g: 0.66, b: 0.78, a: 1.0 },
    rim: 0.30,
    contact_shadow: 0.0,
};
