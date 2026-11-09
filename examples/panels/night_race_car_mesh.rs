//! Procedural mesh of a modern open-wheel single-seater race car for the
//! 3D model layer.
//!
//! Frame: metres, +x forward, +y left, +z up. The origin sits on the ground
//! midway between the axles (front axle x = +1.8, rear axle x = -1.8), and the
//! tyres touch z = 0. Triangles wind counter-clockwise seen from outside and
//! every normal is unit length and points outward. Sharp creases use
//! duplicated vertices (flat faces); tyres, the halo tube and the helmet are
//! smooth around their circumference.
//!
//! Overall: ~5.64 m long, 2.0 m wide over the rear tyres, ~0.96 m tall at the
//! rear wing endplates, airbox top at 0.95 m.
#![allow(dead_code)]

use gpui_maps::render::model::{ModelMaterial, ModelMesh, ModelVertex, MAX_MATERIALS};
use gpui_maps::style::Rgba;

use super::car_dims as dims;

/// Team paint: the only slot tinted by each instance's colour.
pub const PAINT: u32 = 0;
/// Bare carbon: floor, wings, suspension, halo, inlets.
pub const CARBON: u32 = 1;
/// Slick tyre rubber.
pub const TYRE: u32 = 2;
/// White accents: nose tip and the top wing flaps.
pub const ACCENT: u32 = 3;
/// Driver's helmet.
pub const HELMET: u32 = 4;
/// Wheel rims.
pub const RIM: u32 = 5;
/// Coloured stripe ring on each tyre's outer sidewall.
pub const SIDEWALL: u32 = 6;

fn materials() -> Vec<ModelMaterial> {
    let m = |r: f32, g: f32, b: f32, paint: bool, gloss: f32| ModelMaterial {
        color: Rgba { r, g, b, a: 1.0 },
        paint,
        gloss,
        emissive: 0.0,
    };
    vec![
        m(1.0, 1.0, 1.0, true, 0.7),     // PAINT (white, so the tint is the colour)
        m(0.08, 0.08, 0.09, false, 0.35), // CARBON
        m(0.03, 0.03, 0.03, false, 0.08), // TYRE
        m(0.93, 0.93, 0.93, false, 0.5),  // ACCENT
        m(0.98, 0.92, 0.45, false, 0.8),  // HELMET
        m(0.24, 0.24, 0.26, false, 0.65), // RIM
        m(0.95, 0.78, 0.10, false, 0.15), // SIDEWALL
    ]
}

/// The front wheels' parts in [`own_car_mesh`] (left, right): steered by
/// the layer's part table (`gpui_maps::render::model::ModelVertex::pack`).
pub const PART_FRONT_LEFT: u32 = 1;
pub const PART_FRONT_RIGHT: u32 = 2;

/// Builds the car. Deterministic: same mesh on every call.
pub fn car_mesh() -> ModelMesh {
    build(false)
}

/// **The player's own car, for the in-car near pass**: the same car with
/// no helmet (the camera sits just behind it), the front wheels as parts
/// [`PART_FRONT_LEFT`]/[`PART_FRONT_RIGHT`] (they steer), and the
/// cockpit's recess open down to the seat so the driver's legs show under
/// the wheel (the driver is `night_race_driver.rs`).
pub fn own_car_mesh() -> ModelMesh {
    build(true)
}

fn build(own: bool) -> ModelMesh {
    let mut b = Builder::default();
    chassis(&mut b);
    cockpit(&mut b, own);
    engine_cover(&mut b);
    for s in [1.0f32, -1.0] {
        sidepod(&mut b, s);
    }
    floor_and_diffuser(&mut b);
    front_wing(&mut b);
    rear_wing(&mut b);
    for s in [1.0f32, -1.0] {
        if own {
            b.part = if s > 0.0 { PART_FRONT_LEFT } else { PART_FRONT_RIGHT };
        }
        wheel(&mut b, [FRONT_X, s * FRONT_Y, FRONT_R], s, FRONT_R, FRONT_W);
        b.part = 0;
        wheel(&mut b, [REAR_X, s * REAR_Y, REAR_R], s, REAR_R, REAR_W);
        suspension(&mut b, s);
    }
    let materials = materials();
    debug_assert!(materials.len() <= MAX_MATERIALS);
    ModelMesh {
        vertices: b.v,
        indices: b.i,
        materials,
    }
}

// ---------------------------------------------------------------------------
// Proportions
// ---------------------------------------------------------------------------

// All from the shared dimensions (`night_race_car_dims.rs`), which the
// in-car cockpit draws too.
const FRONT_X: f32 = dims::FRONT_AXLE_X as f32;
const REAR_X: f32 = dims::REAR_AXLE_X as f32;
const FRONT_R: f32 = dims::FRONT_TYRE_R as f32;
const FRONT_W: f32 = dims::FRONT_TYRE_W as f32;
const FRONT_Y: f32 = dims::FRONT_TYRE_Y as f32; // tyre centre; outer face at 0.98
const REAR_R: f32 = dims::REAR_TYRE_R as f32;
const REAR_W: f32 = dims::REAR_TYRE_W as f32;
const REAR_Y: f32 = dims::REAR_TYRE_Y as f32; // tyre centre; outer face at 1.00
const WHEEL_SEGS: usize = 24;

// ---------------------------------------------------------------------------
// Car parts
// ---------------------------------------------------------------------------

/// Nose and forward monocoque: low pointed tip rising to the cockpit.
fn chassis(b: &mut Builder) {
    // (x, half width, z bottom, z top)
    let rings: Vec<Vec<V3>> = dims::NOSE
        .iter()
        .map(|&(x, hw, zb, zt)| section(x as f32, 0.0, hw as f32, zb as f32, zt as f32, 12))
        .collect();
    for k in 0..rings.len() - 1 {
        let m = if k == 0 { ACCENT } else { PAINT };
        b.loft(&rings[k], &rings[k + 1], m, Side::Out);
    }
    b.face(&rings[0], ACCENT, [1.0, 0.0, 0.0]);
    b.face(rings.last().unwrap(), PAINT, [-1.0, 0.0, 0.0]);
    // Pylons holding the front wing under the nose.
    for s in [1.0f32, -1.0] {
        b.aabb([2.45, s * 0.045 - 0.007, 0.09], [2.72, s * 0.045 + 0.007, 0.19], CARBON);
    }
    // Mirrors: a pod on a stalk either side of the cockpit, the glass on
    // its back face.
    let [px, py, pz] = dims::f32x3(dims::MIRROR);
    let [dx, dy, dz] = dims::f32x3(dims::MIRROR_POD);
    let [gw, gh] = dims::MIRROR_GLASS.map(|v| v as f32);
    for s in [1.0f32, -1.0] {
        let [fx, fy, fz] = dims::f32x3(dims::MIRROR_STALK_FOOT);
        b.strut([fx, s * fy, fz], [px, s * (py - dy * 0.3), pz - dz * 0.3], 0.03, 0.012, CARBON);
        // The pod: a rounded (bevelled) box, its back face flat for the
        // glass.
        let ring = |x: f32| section(x, s * py, dy / 2.0, pz - dz / 2.0, pz + dz / 2.0, 12);
        let (back, mid, front) = (ring(px - dx / 2.0), ring(px), section(px + dx / 2.0, s * py, dy / 2.0 - 0.012, pz - dz / 2.0 + 0.01, pz + dz / 2.0 - 0.01, 12));
        b.loft(&back, &mid, PAINT, Side::Out);
        b.loft(&mid, &front, PAINT, Side::Out);
        b.face(&back, PAINT, [-1.0, 0.0, 0.0]);
        b.face(&front, PAINT, [1.0, 0.0, 0.0]);
        let (a, c) = (s * py - gw / 2.0, s * py + gw / 2.0);
        b.aabb([px - dx / 2.0 - 0.004, a, pz - gh / 2.0], [px - dx / 2.0, c, pz + gh / 2.0], RIM);
    }
    // The camera antenna on the nose.
    let [ax, ay, az] = dims::f32x3(dims::ANTENNA);
    let (h, t) = (dims::ANTENNA_H as f32, dims::ANTENNA_T as f32 / 2.0);
    b.aabb([ax - t, ay - t, az - 0.01], [ax + t, ay + t, az + h], CARBON);
}

/// Cockpit opening: painted side walls, dark recess, rim, helmet and halo.
/// `own`: the in-car car -- no helmet, the recess open down to the seat.
fn cockpit(b: &mut Builder, own: bool) {
    let (x0, x1) = (dims::COCKPIT_X.0 as f32, dims::COCKPIT_X.1 as f32);
    let (wall_in, wall_out, top) = (dims::COCKPIT_WALL_Y.0 as f32, dims::COCKPIT_WALL_Y.1 as f32, dims::COCKPIT_TOP_Z as f32);
    let (rim_in, rim_out, rim_top) = (dims::RIM_Y.0 as f32, dims::RIM_Y.1 as f32, dims::RIM_TOP_Z as f32);
    let (front0, front1) = (dims::RIM_FRONT_X.0 as f32, dims::RIM_FRONT_X.1 as f32);
    for s in [1.0f32, -1.0] {
        let (ya, yb) = (s * wall_in, s * wall_out);
        b.aabb([x0, ya.min(yb), 0.08], [x1, ya.max(yb), top], PAINT);
        // Carbon rim along the inner edge of the opening.
        let (ra, rb) = (s * rim_in, s * rim_out);
        b.aabb([x0, ra.min(rb), top], [front0, ra.max(rb), rim_top], CARBON);
    }
    // Recess floor of the opening (in the own car the seat's floor, low:
    // the driver sits in it).
    b.aabb([x0, -wall_in, 0.08], [x1, wall_in, if own { 0.12 } else { 0.42 }], CARBON);
    // Front rim of the opening.
    b.aabb([front0, -rim_out, top], [front1, rim_out, rim_top], CARBON);

    // Helmet with a dark visor band.
    if !own {
        b.helmet(dims::f32x3(dims::HELMET_C), dims::HELMET_R as f32);
    }

    // The hoop: a horseshoe over the driver's head plus a centre pillar.
    // Not on the own car: from the in-car camera, just under its apex, the
    // hoop is a heavy band across the top and its pillar splits the view.
    if !own {
        let path: Vec<V3> = dims::hoop_path().into_iter().map(dims::f32x3).collect();
        b.tube(&path, dims::HOOP_TUBE_R as f32, 6, CARBON);
        b.tube(&dims::HOOP_PILLAR.map(dims::f32x3), dims::HOOP_PILLAR_R as f32, 6, CARBON);
    }
}

/// Engine cover with the airbox above the driver's head and a shark fin.
fn engine_cover(b: &mut Builder) {
    let secs = [
        (-0.35, 0.33, 0.08, 0.66),
        (-0.75, 0.32, 0.08, 0.66),
        (-1.25, 0.22, 0.10, 0.52),
        (-1.75, 0.12, 0.12, 0.38),
        (-2.10, 0.07, 0.15, 0.30),
    ];
    let rings: Vec<Vec<V3>> = secs
        .iter()
        .map(|&(x, hw, zb, zt)| section(x, 0.0, hw, zb, zt, 12))
        .collect();
    for k in 0..rings.len() - 1 {
        b.loft(&rings[k], &rings[k + 1], PAINT, Side::Out);
    }
    b.face(&rings[0], PAINT, [1.0, 0.0, 0.0]);
    b.face(rings.last().unwrap(), CARBON, [-1.0, 0.0, 0.0]);

    // Airbox (roll hoop intake) with a recessed carbon inlet.
    let air = [
        (-0.36, 0.115, 0.62, 0.95),
        (-0.55, 0.140, 0.60, 0.95),
        (-0.95, 0.090, 0.55, 0.80),
        (-1.45, 0.040, 0.42, 0.55),
    ];
    let rings: Vec<Vec<V3>> = air
        .iter()
        .map(|&(x, hw, zb, zt)| section(x, 0.0, hw, zb, zt, 10))
        .collect();
    for k in 0..rings.len() - 1 {
        b.loft(&rings[k], &rings[k + 1], PAINT, Side::Out);
    }
    b.face(rings.last().unwrap(), PAINT, [-1.0, 0.0, 0.0]);
    let lip = section(-0.36, 0.0, 0.075, 0.70, 0.92, 10);
    let deep = section(-0.44, 0.0, 0.075, 0.70, 0.92, 10);
    b.loft(&rings[0], &lip, PAINT, Side::Dir([1.0, 0.0, 0.0]));
    b.loft(&lip, &deep, CARBON, Side::In);
    b.face(&deep, CARBON, [1.0, 0.0, 0.0]);

    // Shark fin.
    b.prism_y(
        &[(-0.66, 0.925), (-1.80, 0.60), (-1.80, 0.40), (-1.10, 0.66)],
        -0.006,
        0.006,
        PAINT,
    );
}

/// One sidepod (s = +1 left, -1 right): inlet up front, coke-bottle taper.
fn sidepod(b: &mut Builder, s: f32) {
    // (x, inner |y|, outer |y|, z bottom, z top)
    let secs = dims::SIDEPOD.map(|(x, yi, yo, zb, zt)| (x as f32, yi as f32, yo as f32, zb as f32, zt as f32));
    let rings: Vec<Vec<V3>> = secs
        .iter()
        .map(|&(x, yi, yo, zb, zt)| section(x, s * (yi + yo) / 2.0, (yo - yi) / 2.0, zb, zt, 12))
        .collect();
    for k in 0..rings.len() - 1 {
        b.loft(&rings[k], &rings[k + 1], PAINT, Side::Out);
    }
    b.face(rings.last().unwrap(), PAINT, [-1.0, 0.0, 0.0]);
    // Recessed inlet.
    let (x, yi, yo, zb, zt) = secs[0];
    let cy = s * (yi + yo) / 2.0;
    let hw = (yo - yi) / 2.0 - 0.035;
    let lip = section(x, cy, hw, zb + 0.035, zt - 0.03, 12);
    let deep = section(x - 0.10, cy, hw, zb + 0.035, zt - 0.03, 12);
    b.loft(&rings[0], &lip, PAINT, Side::Dir([1.0, 0.0, 0.0]));
    b.loft(&lip, &deep, CARBON, Side::In);
    b.face(&deep, CARBON, [1.0, 0.0, 0.0]);
}

/// Floor, gearbox, rear crash structure, diffuser ramp and strakes.
fn floor_and_diffuser(b: &mut Builder) {
    b.aabb([-1.45, -0.82, 0.03], [1.10, 0.82, 0.06], CARBON);
    b.aabb([1.10, -0.20, 0.06], [1.45, 0.20, 0.12], CARBON); // plank under the nose
    b.aabb([-2.15, -0.12, 0.13], [-1.70, 0.12, 0.32], CARBON); // gearbox
    b.aabb([-2.34, -0.05, 0.19], [-2.15, 0.05, 0.27], CARBON); // crash structure
    b.aabb([-2.36, -0.03, 0.20], [-2.34, 0.03, 0.26], ACCENT); // rain light
    // Diffuser ramp rising from the floor to the rear.
    let (xa, za, xb, zb) = (-1.45f32, 0.06f32, -2.25f32, 0.28f32);
    let c = [(xa + xb) / 2.0, 0.0, (za + zb) / 2.0];
    let along = [(xb - xa) / 2.0, 0.0, (zb - za) / 2.0];
    let len = norm(along);
    let up = scale([-(zb - za), 0.0, xb - xa], -0.0075 / (2.0 * len));
    b.obox(c, along, [0.0, 0.53, 0.0], up, CARBON);
    for y in [-0.52f32, -0.26, 0.0, 0.26, 0.52] {
        b.prism_y(&[(-1.50, 0.06), (-2.25, 0.275), (-2.25, 0.05), (-1.80, 0.04)], y - 0.006, y + 0.006, CARBON);
    }
}

fn front_wing(b: &mut Builder) {
    let main = airfoil((2.88, 0.085), 0.34, 4.0, 0.03);
    b.prism_y(&main, -0.965, 0.965, CARBON);
    let flap1 = airfoil((2.58, 0.135), 0.24, 16.0, 0.025);
    let flap2 = airfoil((2.40, 0.205), 0.17, 30.0, 0.02);
    for s in [1.0f32, -1.0] {
        let (a, c) = (s * 0.22, s * 0.965);
        b.prism_y(&flap1, a.min(c), a.max(c), CARBON);
        let (a, c) = (s * 0.30, s * 0.965);
        b.prism_y(&flap2, a.min(c), a.max(c), ACCENT);
        let (a, c) = (s * 0.965, s * 0.98);
        b.prism_y(&[(2.92, 0.04), (2.30, 0.04), (2.24, 0.32), (2.70, 0.27)], a.min(c), a.max(c), CARBON);
    }
}

fn rear_wing(b: &mut Builder) {
    // Swan-neck pylon from the crash structure to the main plane.
    b.prism_y(&[(-2.12, 0.28), (-2.30, 0.26), (-2.46, 0.80), (-2.34, 0.82)], -0.01, 0.01, CARBON);
    let main = airfoil((-2.30, 0.80), 0.30, 8.0, 0.035);
    b.prism_y(&main, -0.50, 0.50, PAINT);
    let flap = airfoil((-2.50, 0.86), 0.20, 25.0, 0.025);
    b.prism_y(&flap, -0.50, 0.50, ACCENT);
    let beam = airfoil((-2.20, 0.40), 0.22, 10.0, 0.025);
    b.prism_y(&beam, -0.48, 0.48, CARBON);
    for s in [1.0f32, -1.0] {
        let (a, c) = (s * 0.50, s * 0.512);
        b.prism_y(&[(-2.24, 0.30), (-2.72, 0.26), (-2.72, 0.965), (-2.27, 0.92)], a.min(c), a.max(c), PAINT);
    }
}

/// Wishbones and push/pull rods on one side (s = +1 left, -1 right).
fn suspension(b: &mut Builder, s: f32) {
    let (w, h) = (0.035, 0.016);
    // Front: upright inside the wheel, legs to the nose.
    let uy = dims::FRONT_UPRIGHT_Y as f32;
    b.aabb([FRONT_X - 0.04, s * uy - 0.02, 0.17], [FRONT_X + 0.04, s * uy + 0.02, 0.52], CARBON);
    let side = |p: [f64; 3]| {
        let [x, y, z] = dims::f32x3(p);
        [x, s * y, z]
    };
    for [a, c] in dims::FRONT_WISHBONES {
        b.strut(side(a), side(c), w, h, CARBON);
    }
    b.strut(side(dims::FRONT_PUSH_ROD[0]), side(dims::FRONT_PUSH_ROD[1]), 0.03, 0.03, CARBON); // push rod
    // Rear: upright, legs to the gearbox, pull rod.
    let up = [REAR_X, s * 0.57, 0.0];
    b.aabb([REAR_X - 0.04, s * 0.57 - 0.02, 0.16], [REAR_X + 0.04, s * 0.57 + 0.02, 0.54], CARBON);
    let lo = [up[0], up[1], 0.19];
    let hi = [up[0], up[1], 0.51];
    b.strut([-1.58, s * 0.26, 0.18], lo, w, h, CARBON);
    b.strut([-2.05, s * 0.11, 0.20], lo, w, h, CARBON);
    b.strut([-1.62, s * 0.22, 0.44], hi, w, h, CARBON);
    b.strut([-2.05, s * 0.11, 0.40], hi, w, h, CARBON);
    b.strut([up[0] + 0.02, s * 0.54, 0.49], [-1.92, s * 0.11, 0.20], 0.03, 0.03, CARBON); // pull rod
}

/// A wheel: slick tread, rounded shoulders, coloured stripe ring and a
/// recessed rim on the outer face (`s` = sign of the outward y direction).
fn wheel(b: &mut Builder, c: V3, s: f32, r: f32, w: f32) {
    let h = w / 2.0;
    let rim = r * 0.56;
    // Profile in (radius, axial offset towards the outer face), traversed
    // from the outer centre over the tread to the inner centre.
    let prof: [((f32, f32), (f32, f32), u32); 10] = [
        ((0.0, h - 0.035), (rim, h - 0.035), RIM),
        ((rim, h - 0.035), (rim, h), RIM),
        ((rim, h), (r * 0.70, h), TYRE),
        ((r * 0.70, h), (r * 0.80, h), SIDEWALL),
        ((r * 0.80, h), (r * 0.92, h), TYRE),
        ((r * 0.92, h), (r, h - 0.04), TYRE),
        ((r, h - 0.04), (r, -h + 0.04), TYRE),
        ((r, -h + 0.04), (r * 0.92, -h), TYRE),
        ((r * 0.92, -h), (r * 0.50, -h), TYRE),
        ((r * 0.50, -h), (0.0, -h), RIM),
    ];
    let axis = [0.0, s, 0.0];
    let at = |rad: f32, off: f32, th: f32| -> V3 {
        let radial = [th.cos(), 0.0, th.sin()];
        add(c, add(scale(radial, rad), scale(axis, off)))
    };
    for &((r0, a0), (r1, a1), m) in &prof {
        // Outward normal in the (radial, axial) half plane.
        let (dr, da) = (r1 - r0, a1 - a0);
        let l = (dr * dr + da * da).sqrt();
        let (nr, na) = (-da / l, dr / l);
        let nrm = |th: f32| -> V3 {
            normalize(add(scale([th.cos(), 0.0, th.sin()], nr), scale(axis, na)))
        };
        for k in 0..WHEEL_SEGS {
            let t0 = std::f32::consts::TAU * k as f32 / WHEEL_SEGS as f32;
            let t1 = std::f32::consts::TAU * (k + 1) as f32 / WHEEL_SEGS as f32;
            let (p00, p01, p10, p11) = (at(r0, a0, t0), at(r0, a0, t1), at(r1, a1, t0), at(r1, a1, t1));
            if r0 == 0.0 {
                b.tri_smooth([p00, p10, p11], [nrm(t0), nrm(t0), nrm(t1)], m);
            } else if r1 == 0.0 {
                b.tri_smooth([p00, p01, p10], [nrm(t0), nrm(t1), nrm(t0)], m);
            } else {
                b.tri_smooth([p00, p10, p11], [nrm(t0), nrm(t0), nrm(t1)], m);
                b.tri_smooth([p00, p11, p01], [nrm(t0), nrm(t1), nrm(t1)], m);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shape helpers
// ---------------------------------------------------------------------------

/// A rounded-rectangle cross section (superellipse) at `x`, centred on `cy`.
pub fn section(x: f32, cy: f32, hw: f32, zb: f32, zt: f32, n: usize) -> Vec<V3> {
    let zc = (zb + zt) / 2.0;
    let hh = (zt - zb) / 2.0;
    let sp = |v: f32| v.signum() * v.abs().powf(0.55);
    (0..n)
        .map(|k| {
            let t = std::f32::consts::TAU * (k as f32 + 0.5) / n as f32;
            [x, cy + hw * sp(t.cos()), zc + hh * sp(t.sin())]
        })
        .collect()
}

/// A convex wing section in the xz plane: leading edge `le`, chord running
/// rearward (-x) and up by `angle_deg`, thickness `t`.
fn airfoil(le: (f32, f32), chord: f32, angle_deg: f32, t: f32) -> Vec<(f32, f32)> {
    let a = angle_deg.to_radians();
    let dir = (-a.cos(), a.sin());
    let up = (a.sin(), a.cos());
    [
        (0.0, 0.0),
        (0.08, 0.40),
        (0.35, 0.50),
        (0.75, 0.30),
        (1.0, 0.04),
        (1.0, -0.04),
        (0.60, -0.20),
        (0.12, -0.30),
    ]
    .iter()
    .map(|&(u, v)| {
        (
            le.0 + dir.0 * u * chord + up.0 * v * t,
            le.1 + dir.1 * u * chord + up.1 * v * t,
        )
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Mesh builder
// ---------------------------------------------------------------------------

pub type V3 = [f32; 3];

pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub fn scale(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
pub fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
pub fn norm(a: V3) -> f32 {
    dot(a, a).sqrt()
}
pub fn normalize(a: V3) -> V3 {
    scale(a, 1.0 / norm(a))
}
fn centroid(pts: &[V3]) -> V3 {
    let s = pts.iter().fold([0.0; 3], |acc, &p| add(acc, p));
    scale(s, 1.0 / pts.len() as f32)
}

/// Which way a loft band faces.
pub enum Side {
    /// Away from the rings' centre line (an outer skin).
    Out,
    /// Towards the centre line (the wall of an inlet tunnel).
    In,
    /// Along a fixed direction (a lip between rings in the same plane).
    Dir(V3),
}

#[derive(Default)]
pub struct Builder {
    pub v: Vec<ModelVertex>,
    pub i: Vec<u32>,
    /// The part every vertex added now belongs to (0: the rigid body).
    pub part: u32,
}

impl Builder {
    pub fn vert(&mut self, p: V3, n: V3, m: u32) -> u32 {
        self.v.push(ModelVertex { position: p, normal: n, material: ModelVertex::pack(m, self.part) });
        (self.v.len() - 1) as u32
    }

    /// A triangle with per-vertex normals; winding is made to agree with them.
    pub fn tri_smooth(&mut self, p: [V3; 3], n: [V3; 3], m: u32) {
        let g = cross(sub(p[1], p[0]), sub(p[2], p[0]));
        if norm(g) < 1e-12 {
            return;
        }
        let avg = add(add(n[0], n[1]), n[2]);
        let order = if dot(g, avg) >= 0.0 { [0, 1, 2] } else { [0, 2, 1] };
        for k in order {
            let id = self.vert(p[k], normalize(n[k]), m);
            self.i.push(id);
        }
    }

    /// A flat convex polygon facing roughly along `hint`.
    pub fn face(&mut self, pts: &[V3], m: u32, hint: V3) {
        // Newell normal follows the CCW order of `pts`.
        let mut n = [0.0f32; 3];
        for k in 0..pts.len() {
            let (a, c) = (pts[k], pts[(k + 1) % pts.len()]);
            n[0] += (a[1] - c[1]) * (a[2] + c[2]);
            n[1] += (a[2] - c[2]) * (a[0] + c[0]);
            n[2] += (a[0] - c[0]) * (a[1] + c[1]);
        }
        if norm(n) < 1e-12 {
            return;
        }
        let mut n = normalize(n);
        let mut pts = pts.to_vec();
        if dot(n, hint) < 0.0 {
            n = scale(n, -1.0);
            pts.reverse();
        }
        let base = self.v.len() as u32;
        for &p in &pts {
            self.vert(p, n, m);
        }
        for k in 1..pts.len() as u32 - 1 {
            let g = cross(sub(pts[k as usize], pts[0]), sub(pts[k as usize + 1], pts[0]));
            if norm(g) < 1e-12 {
                continue;
            }
            self.i.extend_from_slice(&[base, base + k, base + k + 1]);
        }
    }

    /// Flat quads joining two rings with the same point count.
    pub fn loft(&mut self, a: &[V3], b: &[V3], m: u32, side: Side) {
        let axis_c = scale(add(centroid(a), centroid(b)), 0.5);
        let n = a.len();
        for k in 0..n {
            let j = (k + 1) % n;
            let q = [a[k], a[j], b[j], b[k]];
            let hint = match side {
                Side::Out => sub(centroid(&q), axis_c),
                Side::In => sub(axis_c, centroid(&q)),
                Side::Dir(d) => d,
            };
            self.face(&q, m, hint);
        }
    }

    /// Oriented box: centre plus three half-axis vectors.
    pub fn obox(&mut self, c: V3, ax: V3, ay: V3, az: V3, m: u32) {
        let p = |sx: f32, sy: f32, sz: f32| add(c, add(scale(ax, sx), add(scale(ay, sy), scale(az, sz))));
        let faces: [(V3, [V3; 4]); 6] = [
            (ax, [p(1., -1., -1.), p(1., 1., -1.), p(1., 1., 1.), p(1., -1., 1.)]),
            (scale(ax, -1.), [p(-1., -1., -1.), p(-1., 1., -1.), p(-1., 1., 1.), p(-1., -1., 1.)]),
            (ay, [p(-1., 1., -1.), p(1., 1., -1.), p(1., 1., 1.), p(-1., 1., 1.)]),
            (scale(ay, -1.), [p(-1., -1., -1.), p(1., -1., -1.), p(1., -1., 1.), p(-1., -1., 1.)]),
            (az, [p(-1., -1., 1.), p(1., -1., 1.), p(1., 1., 1.), p(-1., 1., 1.)]),
            (scale(az, -1.), [p(-1., -1., -1.), p(1., -1., -1.), p(1., 1., -1.), p(-1., 1., -1.)]),
        ];
        for (hint, q) in faces {
            self.face(&q, m, hint);
        }
    }

    pub fn aabb(&mut self, lo: V3, hi: V3, m: u32) {
        let c = scale(add(lo, hi), 0.5);
        let h = scale(sub(hi, lo), 0.5);
        self.obox(c, [h[0], 0.0, 0.0], [0.0, h[1], 0.0], [0.0, 0.0, h[2]], m);
    }

    /// A thin rectangular bar from `p` to `q`, `w` wide and `h` tall.
    pub fn strut(&mut self, p: V3, q: V3, w: f32, h: f32, m: u32) {
        let d = sub(q, p);
        let u = normalize(d);
        let mut side = cross(u, [0.0, 0.0, 1.0]);
        if norm(side) < 1e-3 {
            side = cross(u, [1.0, 0.0, 0.0]);
        }
        let side = normalize(side);
        let up = cross(side, u);
        let c = scale(add(p, q), 0.5);
        self.obox(c, scale(d, 0.5), scale(side, w / 2.0), scale(up, h / 2.0), m);
    }

    /// Extrudes a convex polygon given in the xz plane between y0 and y1.
    pub fn prism_y(&mut self, poly: &[(f32, f32)], y0: f32, y1: f32, m: u32) {
        let lo: Vec<V3> = poly.iter().map(|&(x, z)| [x, y0, z]).collect();
        let hi: Vec<V3> = poly.iter().map(|&(x, z)| [x, y1, z]).collect();
        self.face(&lo, m, [0.0, -1.0, 0.0]);
        self.face(&hi, m, [0.0, 1.0, 0.0]);
        self.loft(&lo, &hi, m, Side::Out);
    }

    /// A round tube along a polyline, smooth around its circumference.
    pub fn tube(&mut self, path: &[V3], r: f32, sides: usize, m: u32) {
        let rings: Vec<(Vec<V3>, Vec<V3>)> = (0..path.len())
            .map(|k| {
                let a = path[k.saturating_sub(1)];
                let c = path[(k + 1).min(path.len() - 1)];
                let t = normalize(sub(c, a));
                let mut s = cross(t, [0.0, 0.0, 1.0]);
                if norm(s) < 0.3 {
                    s = cross(t, [1.0, 0.0, 0.0]);
                }
                let s = normalize(s);
                let u = cross(s, t);
                let mut pts = Vec::new();
                let mut nrm = Vec::new();
                for j in 0..sides {
                    let ph = std::f32::consts::TAU * j as f32 / sides as f32;
                    let n = add(scale(s, ph.cos()), scale(u, ph.sin()));
                    pts.push(add(path[k], scale(n, r)));
                    nrm.push(n);
                }
                (pts, nrm)
            })
            .collect();
        for k in 0..rings.len() - 1 {
            let ((pa, na), (pb, nb)) = (&rings[k], &rings[k + 1]);
            for j in 0..sides {
                let i = (j + 1) % sides;
                self.tri_smooth([pa[j], pa[i], pb[i]], [na[j], na[i], nb[i]], m);
                self.tri_smooth([pa[j], pb[i], pb[j]], [na[j], nb[i], nb[j]], m);
            }
        }
        let t0 = sub(path[0], path[1]);
        let t1 = sub(path[path.len() - 1], path[path.len() - 2]);
        self.face(&rings[0].0, m, t0);
        self.face(&rings[rings.len() - 1].0, m, t1);
    }

    /// A UV sphere in HELMET material with a dark visor band facing forward.
    pub fn helmet(&mut self, c: V3, r: f32) {
        let (lat_n, lon_n) = (8usize, 14usize);
        let pt = |i: usize, j: usize| -> V3 {
            let lat = std::f32::consts::PI * (i as f32 / lat_n as f32 - 0.5);
            let lon = std::f32::consts::TAU * j as f32 / lon_n as f32;
            [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
        };
        for i in 0..lat_n {
            for j in 0..lon_n {
                let (a, bq, cq, d) = (pt(i, j), pt(i, j + 1), pt(i + 1, j + 1), pt(i + 1, j));
                let mid = normalize(add(add(a, bq), add(cq, d)));
                let m = if mid[0] > 0.55 && mid[2] > -0.05 && mid[2] < 0.40 { CARBON } else { HELMET };
                let w = |n: V3| add(c, scale(n, r));
                self.tri_smooth([w(a), w(bq), w(cq)], [a, bq, cq], m);
                self.tri_smooth([w(a), w(cq), w(d)], [a, cq, d], m);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(m: &ModelMesh) -> (V3, V3) {
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for v in &m.vertices {
            for k in 0..3 {
                lo[k] = lo[k].min(v.position[k]);
                hi[k] = hi[k].max(v.position[k]);
            }
        }
        (lo, hi)
    }

    #[test]
    fn triangle_budget() {
        let m = car_mesh();
        assert_eq!(m.indices.len() % 3, 0);
        let tris = m.indices.len() / 3;
        assert!((2_500..=4_000).contains(&tris), "{tris} triangles");
    }

    #[test]
    fn indices_in_range() {
        let m = car_mesh();
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
    }

    #[test]
    fn normals_are_unit() {
        for v in &car_mesh().vertices {
            assert!((norm(v.normal) - 1.0).abs() < 1e-3, "{:?}", v.normal);
        }
    }

    #[test]
    fn winding_agrees_with_normals() {
        let m = car_mesh();
        for t in m.indices.chunks(3) {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| m.vertices[i as usize]);
            let g = cross(sub(b.position, a.position), sub(c.position, a.position));
            let n = add(add(a.normal, b.normal), c.normal);
            assert!(dot(g, n) > 0.0, "triangle wound against its normals");
        }
    }

    #[test]
    fn real_proportions() {
        let (lo, hi) = bounds(&car_mesh());
        let (len, wid, ht) = (hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]);
        assert!((5.4..=5.8).contains(&len), "length {len}");
        assert!((1.9..=2.05).contains(&wid), "width {wid}");
        assert!((0.85..=1.0).contains(&ht), "height {ht}");
        assert!(lo[2] >= -1e-4, "min z {}", lo[2]);
        assert!(lo[2] < 0.01, "tyres must reach the ground, min z {}", lo[2]);
    }

    #[test]
    fn materials_are_valid() {
        let m = car_mesh();
        assert!(m.materials.len() <= MAX_MATERIALS);
        assert!(m.vertices.iter().all(|v| (v.material as usize) < m.materials.len()));
        assert_eq!(m.materials.iter().filter(|x| x.paint).count(), 1);
        assert!(m.materials[PAINT as usize].paint);
    }

    #[test]
    fn deterministic() {
        assert_eq!(car_mesh(), car_mesh());
    }

    /// **The car through the engine's model pipeline, headless.** Three
    /// cars (two tinted, one in the mesh's own paint) at street level,
    /// rendered offscreen over a dark ground with a depth buffer: the
    /// team colours are on screen, and so is black rubber. With
    /// `NIGHT_RACE_CAR_PNG=<path>` set the frame is written there.
    #[test]
    fn the_car_renders_through_the_model_pipeline() {
        use gpui_maps::render::model::{ModelFrame, ModelInstance, ModelInstances, ModelLayerDraw, ModelLight, ModelPipeline};
        let Some(ctx) = pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.ok()?;
            let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor::default()).await.ok()?;
            let max_buffer_size = device.limits().max_buffer_size;
            Some(gpui_maps::render::GpuContext { device, queue, max_buffer_size })
        }) else {
            return;
        };
        let (w, h) = (960u32, 540u32);
        let camera = gpui_maps::render::Camera {
            center_lon: 103.86,
            center_lat: 1.29,
            zoom: 21.3,
            pitch: 1.0,
            bearing: 0.6,
            viewport_width_px: w as f32,
            viewport_height_px: h as f32,
            fov_y: gpui_maps::render::Camera::DEFAULT_FOV_Y,
        };
        let frame = ModelFrame::new(&camera, &gpui_maps::render::globe::globe_frame(&camera).flattened(), 1.0);
        let m_per_deg = 111_320.0;
        let at = |east: f64, north: f64| (103.86 + east / (m_per_deg * 1.29f64.to_radians().cos()), 1.29 + north / m_per_deg);
        let orange = Rgba { r: 1.0, g: 0.45, b: 0.05, a: 1.0 };
        let teal = Rgba { r: 0.0, g: 0.62, b: 0.62, a: 1.0 };
        let (a, b, c) = (at(0.0, 0.0), at(-3.0, 7.0), at(4.0, -6.5));
        let instances = ModelInstances::new(&[
            ModelInstance::at(a.0, a.1, 20.0).with_color(orange),
            ModelInstance::at(b.0, b.1, 35.0).with_color(teal),
            ModelInstance::at(c.0, c.1, 5.0),
        ]);
        let mesh = std::sync::Arc::new(car_mesh());
        let depth_format = wgpu::TextureFormat::Depth24PlusStencil8;
        let mut pipeline = ModelPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm, Some(depth_format));
        pipeline.prepare(&ctx, &frame, &[ModelLayerDraw { id: "cars", mesh: &mesh, generation: 1, instances: &instances, light: ModelLight::NIGHT, parts: &[], near: None }]);
        assert_eq!(pipeline.stats().triangles, 3 * mesh.triangles() as u64);
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let tex = |format, usage| {
            ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let color = tex(wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let depth = tex(depth_format, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let read = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(w * h * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let view = color.create_view(&Default::default());
            let depth_view = depth.create_view(&Default::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.05, g: 0.055, b: 0.07, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0), store: wgpu::StoreOp::Discard }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            assert_eq!(pipeline.draw(&mut pass), 1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &read, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) } },
            size,
        );
        ctx.queue.submit(Some(encoder.finish()));
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        ctx.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let px = read.slice(..).get_mapped_range().expect("mapped").to_vec();
        if let Some(path) = std::env::var_os("NIGHT_RACE_CAR_PNG") {
            image::save_buffer(&path, &px, w, h, image::ColorType::Rgba8).expect("png written");
        }
        let count = |f: &dyn Fn(&[u8]) -> bool| px.chunks(4).filter(|p| f(p)).count();
        let orange_px = count(&|p| p[0] > 150 && p[1] > 40 && p[1] < 140 && p[2] < 60);
        let teal_px = count(&|p| p[0] < 60 && p[1] > 80 && p[2] > 80 && (i32::from(p[1]) - i32::from(p[2])).abs() < 40);
        let rubber = count(&|p| p[0] < 30 && p[1] < 30 && p[2] < 30 && !(p[0] == 13 && p[1] == 14 && p[2] == 18));
        assert!(orange_px > 1_000, "the orange car is on screen: {orange_px} px");
        assert!(teal_px > 500, "the teal car is on screen: {teal_px} px");
        assert!(rubber > 300, "and black tyres: {rubber} px");
    }
}
