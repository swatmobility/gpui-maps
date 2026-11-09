//! **The car's true dimensions**, one set for every camera: the 3D mesh
//! (`night_race_car_mesh.rs`) is built from them and the in-car cockpit
//! (`night_race_view.rs`'s `body`) draws them, so the two never disagree.
//!
//! Car frame, metres: the origin on the ground midway between the axles,
//! +x forward, +y left, +z up; the front axle at x 1.8. Proportioned to a
//! modern open-wheel single-seater: 2.0 m wide over the rear tyres, a 3.6 m
//! wheelbase, the cockpit rim at 0.655 m, the hoop's apex at 0.92 m, the
//! driver's eye at 0.80 m and the helmet's top at 0.93 m.
#![allow(dead_code)]

/// The axles.
pub const FRONT_AXLE_X: f64 = 1.8;
pub const REAR_AXLE_X: f64 = -1.8;
/// The front tyres: radius, width, and the tyre's centre off the
/// centreline (outer face at 0.98).
pub const FRONT_TYRE_R: f64 = 0.35;
pub const FRONT_TYRE_W: f64 = 0.30;
pub const FRONT_TYRE_Y: f64 = 0.83;
/// The rear tyres (outer face at 1.00).
pub const REAR_TYRE_R: f64 = 0.36;
pub const REAR_TYRE_W: f64 = 0.40;
pub const REAR_TYRE_Y: f64 = 0.80;

/// The driver's true eye.
pub const EYE: [f64; 3] = [-0.02, 0.0, 0.80];
/// The in-car camera: 0.20 m behind the axles' midpoint, 0.86 m up --
/// just behind and above the true eye, under the hoop.
pub const IN_CAR_CAMERA: [f64; 3] = [-0.20, 0.0, 0.86];
/// The helmet: centre and radius (its top at 0.93).
pub const HELMET_C: [f64; 3] = [-0.06, 0.0, 0.795];
pub const HELMET_R: f64 = 0.135;

/// The cockpit: its length (x from, to), the side walls' inner and outer
/// `|y|`, their top; the carbon rim on the walls' inner edge (`|y|` from,
/// to) and its top. The opening between the rims is 0.47-0.54 m wide.
pub const COCKPIT_X: (f64, f64) = (-0.35, 0.75);
pub const COCKPIT_WALL_Y: (f64, f64) = (0.24, 0.34);
pub const COCKPIT_TOP_Z: f64 = 0.64;
pub const RIM_Y: (f64, f64) = (0.235, 0.27);
pub const RIM_TOP_Z: f64 = 0.655;
/// The front rim of the opening: x from, to.
pub const RIM_FRONT_X: (f64, f64) = (0.70, 0.75);

/// The nose and forward monocoque, near the tip to the cockpit: `(x, half
/// width, z bottom, z top)`.
pub const NOSE: [(f64, f64, f64, f64); 7] = [
    (2.84, 0.035, 0.170, 0.225),
    (2.62, 0.085, 0.155, 0.295),
    (2.25, 0.125, 0.165, 0.395),
    (1.85, 0.165, 0.175, 0.495),
    (1.40, 0.220, 0.120, 0.580),
    (0.95, 0.290, 0.080, 0.630),
    (0.75, 0.340, 0.080, 0.640),
];
/// The camera antenna on the nose's top: its foot, height and thickness.
pub const ANTENNA: [f64; 3] = [1.18, 0.0, 0.604];
pub const ANTENNA_H: f64 = 0.06;
pub const ANTENNA_T: f64 = 0.008;

/// One sidepod's sections (left side), front to back: `(x, inner |y|,
/// outer |y|, z bottom, z top)`.
pub const SIDEPOD: [(f64, f64, f64, f64, f64); 5] = [
    (0.62, 0.36, 0.78, 0.12, 0.50),
    (0.20, 0.34, 0.80, 0.08, 0.56),
    (-0.40, 0.32, 0.72, 0.08, 0.52),
    (-0.95, 0.30, 0.52, 0.09, 0.42),
    (-1.40, 0.24, 0.34, 0.10, 0.32),
];

/// The mirrors (left side): the pod's centre, its size (deep along x,
/// wide, tall), the glass on its back face (wide, tall), and the stalk's
/// foot on the cockpit wall.
pub const MIRROR: [f64; 3] = [0.55, 0.46, 0.80];
pub const MIRROR_POD: [f64; 3] = [0.06, 0.18, 0.07];
pub const MIRROR_GLASS: [f64; 2] = [0.15, 0.05];
pub const MIRROR_STALK_FOOT: [f64; 3] = [0.60, 0.32, 0.64];

/// The hoop over the driver's head: a horseshoe of tube round the
/// cockpit, its plan an ellipse centred at `HOOP_C_X` with half-axes
/// `HOOP_RX` (forward) and `HOOP_RY` (across), its apex at the front
/// `HOOP_APEX_Z` high, dropping `HOOP_DROP` toward its rear ends; the tube
/// `HOOP_TUBE_R` in radius (50 mm).
pub const HOOP_C_X: f64 = -0.06;
pub const HOOP_RX: f64 = 0.42;
pub const HOOP_RY: f64 = 0.27;
pub const HOOP_APEX_Z: f64 = 0.92;
pub const HOOP_DROP: f64 = 0.015;
pub const HOOP_TUBE_R: f64 = 0.025;
/// The horseshoe sweeps this far either side of straight ahead, degrees.
pub const HOOP_SWEEP_DEG: f64 = 105.0;
/// Its rear legs down to the bodywork (left side), top to foot.
pub const HOOP_LEG: [[f64; 3]; 2] = [[-0.25, 0.268, 0.70], [-0.27, 0.27, 0.64]];
/// The centre pillar, foot to apex: 50 mm wide, ~0.56 m ahead of the eye
/// at its middle.
pub const HOOP_PILLAR: [[f64; 3]; 3] = [[0.70, 0.0, 0.63], [0.54, 0.0, 0.80], [HOOP_C_X + HOOP_RX, 0.0, HOOP_APEX_Z]];
pub const HOOP_PILLAR_R: f64 = 0.025;

/// The hoop's centreline at `phi` degrees round from straight ahead (left
/// positive).
#[must_use]
pub fn hoop_point(phi_deg: f64) -> [f64; 3] {
    let f = (phi_deg.abs() / HOOP_SWEEP_DEG).powi(2);
    let phi = phi_deg.to_radians();
    [HOOP_C_X + HOOP_RX * phi.cos(), HOOP_RY * phi.sin(), HOOP_APEX_Z - HOOP_DROP * f]
}

/// The hoop's whole centreline: the right leg up, the horseshoe round the
/// front, the left leg down.
#[must_use]
pub fn hoop_path() -> Vec<[f64; 3]> {
    let n = 14;
    let right = |p: [f64; 3]| [p[0], -p[1], p[2]];
    let mut path = vec![right(HOOP_LEG[1]), right(HOOP_LEG[0])];
    for k in 0..=n {
        path.push(hoop_point(-HOOP_SWEEP_DEG + 2.0 * HOOP_SWEEP_DEG * f64::from(k) / f64::from(n)));
    }
    path.push(HOOP_LEG[0]);
    path.push(HOOP_LEG[1]);
    path
}

/// The steering wheel: the centre of its face (0.52 m ahead of the eye,
/// 0.23 m below it), its lean back from upright, and its size over the
/// grips (wide, tall).
pub const WHEEL_C: [f64; 3] = [0.50, 0.0, 0.57];
pub const WHEEL_LEAN_DEG: f64 = 22.0;
pub const WHEEL_W: f64 = 0.28;
pub const WHEEL_H: f64 = 0.176;

/// The wheel's shapes in its own drawing plane: `u` to the driver's
/// right, `v` up the face. A rectangle wider than tall, flat on top
/// and underneath, a chunky grip at each end standing above the body
/// (the top is open between them). One side's points; the other side
/// mirrors `u`. Drawing units: [`YOKE_SCALE`] takes them to metres.
pub const YOKE_BODY: [(f64, f64); 8] = [(-0.118, 0.052), (-0.150, 0.040), (-0.150, -0.060), (-0.128, -0.082), (0.128, -0.082), (0.150, -0.060), (0.150, 0.040), (0.118, 0.052)];
/// The body's depth behind its face, metres.
pub const YOKE_DEPTH: f64 = 0.035;
/// A grip: `u` from the inner edge to the outer, `v` bottom to top,
/// the corner radius; it stands proud of the face and runs deeper.
pub const GRIP_U: (f64, f64) = (0.136, 0.192);
pub const GRIP_V: (f64, f64) = (-0.078, 0.098);
pub const GRIP_R: f64 = 0.02;
pub const GRIP_FRONT: f64 = 0.012;
pub const GRIP_BACK: f64 = -0.05;
/// The drawing's units to metres, across and up: the grips' outer
/// edges [`WHEEL_W`] apart, the body's foot to the grips' tops
/// [`WHEEL_H`].
pub const YOKE_SCALE: (f64, f64) = (WHEEL_W / (2.0 * GRIP_U.1), WHEEL_H / (GRIP_V.1 - YOKE_BODY[3].1));

/// The front suspension (left side): the wishbones' legs from the tub to
/// the upright (lower front, lower rear, upper front, upper rear), and the
/// push rod.
pub const FRONT_UPRIGHT_Y: f64 = 0.64;
pub const FRONT_WISHBONES: [[[f64; 3]; 2]; 4] = [
    [[2.00, 0.13, 0.24], [FRONT_AXLE_X, FRONT_UPRIGHT_Y, 0.21]],
    [[1.55, 0.17, 0.24], [FRONT_AXLE_X, FRONT_UPRIGHT_Y, 0.21]],
    [[2.00, 0.13, 0.44], [FRONT_AXLE_X, FRONT_UPRIGHT_Y, 0.49]],
    [[1.60, 0.18, 0.47], [FRONT_AXLE_X, FRONT_UPRIGHT_Y, 0.49]],
];
pub const FRONT_PUSH_ROD: [[f64; 3]; 2] = [[FRONT_AXLE_X - 0.02, 0.60, 0.23], [1.62, 0.14, 0.52]];

/// `p` on the car's other side.
#[must_use]
pub fn mirror_y(p: [f64; 3]) -> [f64; 3] {
    [p[0], -p[1], p[2]]
}

/// `p` in single precision, for the mesh.
#[must_use]
pub fn f32x3(p: [f64; 3]) -> [f32; 3] {
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference ranges the plan sets for a modern single-seater.
    #[test]
    fn the_dimensions_are_true_to_life() {
        assert!((0.27..=0.29).contains(&WHEEL_W), "the wheel {WHEEL_W} m wide");
        assert!((0.17..=0.20).contains(&WHEEL_H), "the wheel {WHEEL_H} m tall");
        let ahead = WHEEL_C[0] - EYE[0];
        let below = EYE[2] - WHEEL_C[2];
        assert!((0.45..=0.55).contains(&ahead) && (0.20..=0.25).contains(&below), "the wheel {ahead} m ahead, {below} m below the eye");
        assert!((20.0..=25.0).contains(&WHEEL_LEAN_DEG));
        assert!((0.78..=0.82).contains(&EYE[2]), "the eye {} m up", EYE[2]);
        assert!(((HELMET_C[2] + HELMET_R) - 0.93).abs() < 0.01, "the helmet's top");
        assert!((0.60..=0.65).contains(&COCKPIT_TOP_Z) && (0.60..=0.66).contains(&RIM_TOP_Z));
        let opening = 2.0 * RIM_Y.0;
        assert!((0.45..=0.55).contains(&opening), "the opening {opening} m");
        assert!((0.85..=0.92).contains(&HOOP_APEX_Z), "the hoop's apex {HOOP_APEX_Z}");
        let pillar = HOOP_PILLAR[1][0] - EYE[0];
        assert!((0.55..=0.65).contains(&pillar), "the pillar {pillar} m ahead of the eye");
        assert!((HOOP_PILLAR_R * 2.0 - 0.05).abs() < 0.005 && (HOOP_TUBE_R * 2.0 - 0.05).abs() < 0.005);
        assert!((0.45..=0.55).contains(&MIRROR[1]), "the mirrors {} m out", MIRROR[1]);
        assert!((MIRROR_GLASS[0] - 0.15).abs() < 0.01 && (MIRROR_GLASS[1] - 0.05).abs() < 0.01);
        assert!(((FRONT_TYRE_Y + FRONT_TYRE_W / 2.0) - 0.98).abs() < 1e-9, "the front tyres' outer face");
        let antenna = ANTENNA[0] - EYE[0];
        assert!((antenna - 1.2).abs() < 0.05 && (ANTENNA_H - 0.06).abs() < 0.005, "the antenna {antenna} m ahead");
        assert_eq!(hoop_point(0.0), HOOP_PILLAR[2], "the pillar meets the hoop at its apex");
    }
}
