//! SGP4 state at a time, the TEME to Earth-fixed rotation, the WGS84
//! geodetic altitude, and the elements a card shows (spec §4.2, §5.1).

use super::catalog::SatObject;
use super::omm::OmmRecord;

/// Position (km) and velocity (km/s) in the TEME frame of the elements' epoch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemeState {
    /// Position, km, TEME.
    pub position_km: [f64; 3],
    /// Velocity, km/s, TEME.
    pub velocity_km_s: [f64; 3],
}

/// The object's TEME state at `unix_s`, by the IAU-mode `propagate` (the
/// mode `Catalog::build`'s `from_elements` initialised). `None` when sgp4
/// errs (decayed, diverging eccentricity) or returns a non-finite value.
///
/// Cost: stateless, so a 12 h or 24 h resonant orbit (Molniya, GPS, GEO) is
/// re-integrated from its epoch in 720-minute steps on every call -- 246 ns
/// at the epoch, ~2.9 us at +30 d (MG-S1b). sgp4's cached-state alternative,
/// `Constants::propagate_from_state` with a `ResonanceState`, avoids that,
/// but it PANICS if time runs backwards (`deep_space.rs:104` asserts the
/// integration is monotonic): a caller that caches the state must reset it
/// whenever the scene clock is scrubbed back.
#[must_use]
pub fn state_at(obj: &SatObject, unix_s: f64) -> Option<TemeState> {
    let minutes = (unix_s - obj.record.epoch_unix) / 60.0;
    let p = obj.constants.propagate(sgp4::MinutesSinceEpoch(minutes)).ok()?;
    let finite = p.position.iter().chain(p.velocity.iter()).all(|x| x.is_finite());
    finite.then_some(TemeState { position_km: p.position, velocity_km_s: p.velocity })
}

/// A [`super::catalog::Catalog`] as a keyframe source for
/// `MapView`'s keyframed altitude layers: TEME metres and m/s
/// (`RefFrame::Inertial`). An object sgp4 cannot propagate at that instant
/// (decayed, diverging, or a non-finite result) is
/// [`KeyState::HIDDEN`](crate::render::altitude::KeyState::HIDDEN): dropped,
/// never drawn at the origin.
///
/// Uses the stateless [`state_at`], never sgp4's cached deep-space state,
/// so a scene clock scrubbed backwards is just another instant (the cached
/// path panics on it; see [`state_at`]).
#[derive(Clone)]
pub struct CatalogSource(pub std::sync::Arc<super::catalog::Catalog>);

impl crate::render::altitude::KeyframeSource for CatalogSource {
    fn len(&self) -> usize {
        self.0.objects.len()
    }

    fn states_at(&self, unix_s: f64, range: std::ops::Range<usize>, out: &mut [crate::render::altitude::KeyState]) {
        use crate::render::altitude::KeyState;
        for (o, i) in out.iter_mut().zip(range) {
            *o = self.0.objects.get(i).and_then(|obj| state_at(obj, unix_s)).map_or(KeyState::HIDDEN, |st| KeyState {
                position_m: glam::DVec3::from_array(st.position_km) * 1000.0,
                velocity_m_s: glam::DVec3::from_array(st.velocity_km_s) * 1000.0,
                valid: true,
            });
        }
    }
}

/// TEME to Earth-fixed: `Rz(-gmst)` (polar motion ignored; it is metres).
/// `gmst` is [`super::gmst_rad`] at the same instant.
#[must_use]
pub fn teme_to_ecef_km(p: [f64; 3], gmst: f64) -> [f64; 3] {
    let (s, c) = gmst.sin_cos();
    [c * p[0] + s * p[1], -s * p[0] + c * p[1], p[2]]
}

/// One orbit of `obj` from `unix_s`, as `samples` TEME positions in metres:
/// `state_at(t + k P / samples)` for `k` in `0..samples`, `P = 1440 /
/// MEAN_MOTION` minutes. An instant sgp4 cannot propagate is skipped. For
/// an [`crate::render::altitude::AltitudeLine`] with `closed: true` in a
/// `RefFrame::Inertial` layer: the ring stays put in TEME (precession
/// aside) while the planet turns under it.
#[must_use]
pub fn orbit_ring_teme(obj: &SatObject, unix_s: f64, samples: usize) -> Vec<glam::DVec3> {
    let period_s = 1440.0 / obj.record.mean_motion * 60.0;
    if !period_s.is_finite() || period_s <= 0.0 || samples == 0 {
        return Vec::new();
    }
    (0..samples)
        .filter_map(|k| state_at(obj, unix_s + k as f64 * period_s / samples as f64))
        .map(|st| glam::DVec3::from_array(st.position_km) * 1000.0)
        .collect()
}

/// A longitude swing between two ground track samples, the short way
/// round, past which the track is broken rather than joined: only a pass
/// within a step of a pole turns this far (a 30 s step is ~220 km, so a
/// swing over 90° needs a pass within ~110 km of the pole at LEO speeds).
pub const POLAR_JUMP_DEG: f64 = 90.0;

/// The ground track's sampling step, seconds: the finest it takes. A long
/// period takes a coarser one (see [`GROUND_TRACK_MAX_STEPS`]).
pub const GROUND_TRACK_STEP_S: f64 = 30.0;

/// The most steps one ground track takes (so at most one more sample).
/// At [`GROUND_TRACK_STEP_S`] this is 1.5 periods of anything with a
/// period under 11 h 7 min (every LEO and MEO object); a longer period
/// (GEO, TESS's 13.7 days, the parser's floor of 0.05 rev/day) is sampled
/// every `1.5 P / 2000` instead, so no track costs more than ~2,000
/// `state_at` calls on the UI thread -- about 6 ms even for a deep-space
/// object re-integrated from a stale epoch (final review M5; uncapped, the
/// slowest admitted orbit took ~86,000).
pub const GROUND_TRACK_MAX_STEPS: usize = 2_000;

/// The step [`ground_track`] samples an orbit of `period_s` at:
/// [`GROUND_TRACK_STEP_S`], or coarser so 1.5 periods take at most
/// [`GROUND_TRACK_MAX_STEPS`] steps.
#[must_use]
pub fn ground_track_step_s(period_s: f64) -> f64 {
    GROUND_TRACK_STEP_S.max(1.5 * period_s / GROUND_TRACK_MAX_STEPS as f64)
}

/// The sub-satellite track from half an orbit before `unix_s` to one orbit
/// after it, `[t - P/2, t + P]` every [`ground_track_step_s`] (30 s, or
/// coarser past [`GROUND_TRACK_MAX_STEPS`]), as geocentric
/// `(lon, lat)` degrees (TEME to Earth-fixed by GMST at each sample), split
/// into pieces at the antimeridian: where consecutive samples' longitudes
/// differ by more than 180°, the latitude of the crossing is interpolated
/// linearly in unwrapped longitude, one piece ends at `±180` and the next
/// starts at `∓180` at that latitude, so no segment wraps across the map.
/// Near a pole, where one step can swing the longitude by more than
/// [`POLAR_JUMP_DEG`] the short way round (a near-90° orbit), the line is
/// broken instead, so no segment is a chord across the map. Pieces of fewer
/// than two points are dropped; so are instants sgp4 cannot propagate.
#[must_use]
pub fn ground_track(obj: &SatObject, unix_s: f64) -> Vec<Vec<(f64, f64)>> {
    let period_s = 1440.0 / obj.record.mean_motion * 60.0;
    if !period_s.is_finite() || period_s <= 0.0 {
        return Vec::new();
    }
    let step_s = ground_track_step_s(period_s);
    let n = ((1.5 * period_s / step_s).floor() as usize).min(GROUND_TRACK_MAX_STEPS);
    let start = unix_s - period_s / 2.0;
    let mut pieces: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut piece: Vec<(f64, f64)> = Vec::new();
    for k in 0..=n {
        let tau = start + k as f64 * step_s;
        let Some(st) = state_at(obj, tau) else {
            // A gap in propagation breaks the line.
            if piece.len() >= 2 {
                pieces.push(std::mem::take(&mut piece));
            }
            piece.clear();
            continue;
        };
        let [x, y, z] = teme_to_ecef_km(st.position_km, super::gmst_rad(tau));
        let r = (x * x + y * y + z * z).sqrt();
        let p = (y.atan2(x).to_degrees(), (z / r).asin().to_degrees());
        if let Some(&prev) = piece.last() {
            let d = p.0 - prev.0;
            // The short way round, in (-180, 180].
            let short = d - 360.0 * (d / 360.0).round();
            if short.abs() > POLAR_JUMP_DEG {
                // Over a polar cap: no real 30 s step turns this far except
                // past a pole, where a straight segment (or an interpolated
                // antimeridian point) would be a chord across the map. Break
                // the line; the cap itself is off the Mercator map.
                if piece.len() >= 2 {
                    pieces.push(std::mem::take(&mut piece));
                }
                piece.clear();
            } else if d.abs() > 180.0 {
                // Crossing the antimeridian: towards +180 when the jump is
                // negative (e.g. 179 -> -179), towards -180 otherwise.
                let edge = if d < 0.0 { 180.0 } else { -180.0 };
                let unwrapped = p.0 + 2.0 * edge;
                let f = (edge - prev.0) / (unwrapped - prev.0);
                let lat = prev.1 + f * (p.1 - prev.1);
                piece.push((edge, lat));
                if piece.len() >= 2 {
                    pieces.push(std::mem::take(&mut piece));
                }
                piece.clear();
                piece.push((-edge, lat));
            }
        }
        piece.push(p);
    }
    if piece.len() >= 2 {
        pieces.push(piece);
    }
    pieces
}

/// `pieces` (from [`ground_track`]) as a GeoJSON FeatureCollection of one
/// MultiLineString Feature, for `MapView::set_source` or
/// `CaptureRequest::overlay`: properties `kind: "ground-track"`, `color`
/// (`#rrggbb`), and the overlay's own `line-color` and `line-width` (1.5 px),
/// so it draws as a thin track in that colour without further paint.
#[must_use]
pub fn ground_track_geojson(pieces: &[Vec<(f64, f64)>], color_hex: &str) -> String {
    let coordinates: Vec<Vec<[f64; 2]>> = pieces.iter().map(|p| p.iter().map(|&(lon, lat)| [lon, lat]).collect()).collect();
    serde_json::json!({
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "properties": { "kind": "ground-track", "color": color_hex, "line-color": color_hex, "line-width": 1.5 },
            "geometry": { "type": "MultiLineString", "coordinates": coordinates },
        }],
    })
    .to_string()
}

const WGS84_A_KM: f64 = 6_378.137;
const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Height above the WGS84 ellipsoid of an Earth-fixed point, km, by
/// Bowring's method with two iterations (sub-millimetre for any orbit).
#[must_use]
pub fn geodetic_altitude_km(ecef_km: [f64; 3]) -> f64 {
    let a = WGS84_A_KM;
    let b = a * (1.0 - WGS84_F);
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let ep2 = (a * a - b * b) / (b * b);
    let [x, y, z] = ecef_km;
    let p = x.hypot(y);
    if p < 1e-9 {
        // On the axis: the latitude is ±90° and the height is |z| - b.
        return z.abs() - b;
    }
    let mut beta = z.atan2((1.0 - WGS84_F) * p);
    let mut phi = 0.0;
    for _ in 0..2 {
        let (sb, cb) = beta.sin_cos();
        phi = (z + ep2 * b * sb * sb * sb).atan2(p - e2 * a * cb * cb * cb);
        beta = ((1.0 - WGS84_F) * phi.sin()).atan2(phi.cos());
    }
    // h = p cos φ + z sin φ - a sqrt(1 - e² sin² φ): no division by cos φ.
    let (sp, cp) = phi.sin_cos();
    p * cp + z * sp - a * (1.0 - e2 * sp * sp).sqrt()
}

/// What an object's card shows, from its mean elements alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Derived {
    /// Orbital period, minutes (`1440 / MEAN_MOTION`).
    pub period_min: f64,
    /// Above the equatorial radius (6,378.137 km), km.
    pub perigee_km: f64,
    /// Above the equatorial radius, km.
    pub apogee_km: f64,
    /// Inclination, degrees.
    pub inclination_deg: f64,
}

/// Mean-motion semi-major axis; the period is 1440 / MEAN_MOTION (spec §4.2).
#[must_use]
pub fn derived(r: &OmmRecord) -> Derived {
    const MU: f64 = 398_600.441_8;
    const RE: f64 = WGS84_A_KM;
    let n = r.mean_motion * std::f64::consts::TAU / 86_400.0; // rad/s
    let a = (MU / (n * n)).cbrt();
    Derived {
        period_min: 1440.0 / r.mean_motion,
        perigee_km: a * (1.0 - r.eccentricity) - RE,
        apogee_km: a * (1.0 + r.eccentricity) - RE,
        inclination_deg: r.inclination_deg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orbit::{catalog::*, omm::parse_omm};
    const STATIONS: &[u8] = include_bytes!("../../tests/fixtures/orbit/stations.json");

    type Row = (f64, [f64; 3], [f64; 3]);

    /// Pairs the TLEs of `vallado_subset.tle` with the `"<norad> xx"` blocks
    /// of `vallado_subset.out` (rows `t x y z vx vy vz`), in file order.
    fn vallado_cases(tle: &str, out: &str) -> Vec<(sgp4::Elements, Vec<Row>)> {
        let lines: Vec<&str> = tle.lines().filter(|l| !l.trim().is_empty()).collect();
        let elements: Vec<sgp4::Elements> = lines
            .chunks(2)
            .map(|p| {
                // 11801's line 1 (the original STR#3 case) leaves the
                // ephemeris-type column 63 blank; Vallado's C++ reads a blank
                // as 0, the sgp4 crate wants the digit (its own test_cases.toml
                // writes the 0 in). Patch it here; the fixture stays verbatim.
                let mut l1 = p[0].as_bytes().to_vec();
                if l1.get(62) == Some(&b' ') {
                    l1[62] = b'0';
                }
                sgp4::Elements::from_tle(None, &l1, p[1].as_bytes()).expect("TLE parses")
            })
            .collect();
        let mut blocks: Vec<(u64, Vec<Row>)> = Vec::new();
        for l in out.lines().filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.get(1) == Some(&"xx") {
                blocks.push((f[0].parse().expect("norad"), Vec::new()));
            } else {
                let v: Vec<f64> = f[..7].iter().map(|s| s.parse().expect("number")).collect();
                blocks.last_mut().expect("a header first").1.push((v[0], [v[1], v[2], v[3]], [v[4], v[5], v[6]]));
            }
        }
        assert_eq!(elements.len(), blocks.len());
        elements
            .into_iter()
            .zip(blocks)
            .map(|(e, (id, rows))| {
                assert_eq!(e.norad_id, id);
                (e, rows)
            })
            .collect()
    }

    #[test]
    fn the_iss_at_its_own_epoch_is_where_its_elements_say() {
        let recs = parse_omm(STATIONS).unwrap().records;
        let c = Catalog::build(recs, &Membership::default());
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let s = state_at(iss, iss.record.epoch_unix).expect("propagates");
        let r = glam::DVec3::from(s.position_km);
        let v = glam::DVec3::from(s.velocity_km_s);
        assert!((6_700.0..6_850.0).contains(&r.length()), "|r| = {}", r.length());
        let h = r.cross(v);
        let incl = (h.z / h.length()).acos().to_degrees();
        assert!((incl - 51.6312).abs() < 0.05, "inclination from r x v = {incl}");
        // The period: the osculating semi-major axis swings by ~10 km over an
        // ISS orbit (J2 short-period terms), ±0.2 min of period, so one
        // instant cannot be held to 0.1 min. Its mean over one orbit can.
        const MU: f64 = 398_600.441_8;
        let osc_a = |s: TemeState| {
            let (r, v) = (glam::DVec3::from(s.position_km), glam::DVec3::from(s.velocity_km_s));
            1.0 / (2.0 / r.length() - v.length_squared() / MU)
        };
        let steps = 186; // 30 s steps over 93 min
        let a = (0..steps)
            .map(|k| osc_a(state_at(iss, iss.record.epoch_unix + 30.0 * f64::from(k)).expect("propagates")))
            .sum::<f64>()
            / f64::from(steps);
        let period_min = std::f64::consts::TAU * (a.powi(3) / MU).sqrt() / 60.0;
        assert!((period_min - 1440.0 / 15.48680135).abs() < 0.1, "period {period_min} min");
    }

    #[test]
    fn the_vallado_vectors_match() {
        // Kept rows (tcppver.out): 00005 at t = 0, 360, 1440 min; 08195
        // (MOLNIYA 2-14, 12 h resonant, deep space) at 0, 360, 1440; 11801
        // (STR#3 SDP4 test, e = 0.7318) at 0, 360, 1440.
        //
        // Mode: AFSPC compatibility (`from_elements_afspc_compatibility_mode` +
        // `propagate_afspc_compatibility_mode`), because that is what the sgp4
        // crate's own tests/propagate.rs uses against these vectors, which
        // were generated by Vallado's AFSPC-mode C++. Production code uses the
        // IAU `from_elements` + `propagate`.
        //
        // Tolerance: the crate's is per component (1e-6 km, 1e-9 km/s); this
        // test bounds the vector norm |dr| < 1e-6 km, |dv| < 1e-9 km/s.
        let tle = include_str!("../../tests/fixtures/orbit/vallado_subset.tle");
        let out = include_str!("../../tests/fixtures/orbit/vallado_subset.out");
        let cases = vallado_cases(tle, out);
        assert_eq!(cases.len(), 3, "00005, one deep-space resonant, one high-e");
        for (elements, rows) in cases {
            assert_eq!(rows.len(), 3, "{}: t = 0, 360, 1440", elements.norad_id);
            let k = sgp4::Constants::from_elements_afspc_compatibility_mode(&elements).unwrap();
            for (t, r_ref, v_ref) in rows {
                let p = k.propagate_afspc_compatibility_mode(sgp4::MinutesSinceEpoch(t)).unwrap();
                let dr = (glam::DVec3::from(p.position) - glam::DVec3::from(r_ref)).length();
                let dv = (glam::DVec3::from(p.velocity) - glam::DVec3::from(v_ref)).length();
                assert!(dr < 1e-6 && dv < 1e-9, "{} t={t}: dr {dr:e} km, dv {dv:e} km/s", elements.norad_id);
            }
        }
    }

    #[test]
    fn the_production_path_reproduces_the_vallado_near_earth_case() {
        // OMM JSON -> parse_omm -> to_elements, the path the demo runs,
        // pinned to 00005's reference rows in two steps:
        // 1. our `Elements` equal the TLE's field for field, and in AFSPC mode
        //    (WGS72, the mode the vectors were made in) reproduce them to
        //    1e-6 km;
        // 2. production's IAU mode (`from_elements` + `propagate`, WGS84) and
        //    `state_at` stay within 0.05 km of them. The two modes' gravity
        //    models differ, which moves 00005 by 19.9, 17.9 and 15.5 m at t = 0,
        //    360 and 1440 min (measured); a wrong unit or epoch would move it
        //    by kilometres.
        let tle = include_str!("../../tests/fixtures/orbit/vallado_subset.tle");
        let out = include_str!("../../tests/fixtures/orbit/vallado_subset.out");
        let (e, rows) = vallado_cases(tle, out).swap_remove(0);
        assert_eq!(e.norad_id, 5);
        let body = format!(
            r#"[{{"OBJECT_NAME":"TEME EXAMPLE","OBJECT_ID":"1958-002B","EPOCH":"{}","MEAN_MOTION":{:?},
              "ECCENTRICITY":{:?},"INCLINATION":{:?},"RA_OF_ASC_NODE":{:?},"ARG_OF_PERICENTER":{:?},
              "MEAN_ANOMALY":{:?},"NORAD_CAT_ID":5,"BSTAR":{:?},"MEAN_MOTION_DOT":{:?},
              "MEAN_MOTION_DDOT":{:?},"ELEMENT_SET_NO":{},"REV_AT_EPOCH":{}}}]"#,
            e.datetime.format("%Y-%m-%dT%H:%M:%S%.9f"),
            e.mean_motion, e.eccentricity, e.inclination, e.right_ascension, e.argument_of_perigee,
            e.mean_anomaly, e.drag_term, e.mean_motion_dot, e.mean_motion_ddot,
            e.element_set_number, e.revolution_number,
        );
        let rec = parse_omm(body.as_bytes()).unwrap().records.remove(0);
        let ours = crate::orbit::omm::to_elements(&rec);
        // Field for field, what sgp4 reads is what the TLE said.
        assert_eq!(
            (ours.norad_id, ours.datetime, ours.element_set_number, ours.revolution_number),
            (e.norad_id, e.datetime, e.element_set_number, e.revolution_number)
        );
        let f = |x: &sgp4::Elements| {
            [x.mean_motion, x.eccentricity, x.inclination, x.right_ascension, x.argument_of_perigee,
             x.mean_anomaly, x.drag_term, x.mean_motion_dot, x.mean_motion_ddot]
        };
        assert_eq!(f(&ours), f(&e));

        let afspc = sgp4::Constants::from_elements_afspc_compatibility_mode(&ours).unwrap();
        let iau = sgp4::Constants::from_elements(&ours).unwrap();
        let c = Catalog::build(vec![rec], &Membership::default());
        for (t, r_ref, v_ref) in rows {
            let dist = |p: [f64; 3], q: [f64; 3]| (glam::DVec3::from(p) - glam::DVec3::from(q)).length();
            let p = afspc.propagate_afspc_compatibility_mode(sgp4::MinutesSinceEpoch(t)).unwrap();
            let (dr, dv) = (dist(p.position, r_ref), dist(p.velocity, v_ref));
            assert!(dr < 1e-6 && dv < 1e-9, "AFSPC t={t}: dr {dr:e} km, dv {dv:e} km/s");
            let p = iau.propagate(sgp4::MinutesSinceEpoch(t)).unwrap();
            let dr_iau = dist(p.position, r_ref);
            let s = state_at(&c.objects[0], c.objects[0].record.epoch_unix + t * 60.0).unwrap();
            let dr_state = dist(s.position_km, r_ref);
            assert!(dr_iau < 0.05 && dr_state < 0.05, "t={t}: IAU {dr_iau} km, state_at {dr_state} km");
            // `state_at` is the IAU path plus the Unix-seconds epoch round trip.
            assert!(dist(s.position_km, p.position) < 1e-5, "t={t}: the epoch round trip moved it");
        }
    }

    #[test]
    fn a_polar_ground_track_splits_at_the_antimeridian_without_a_wrap_segment() {
        let recs: Vec<_> = crate::orbit::synthetic::synthetic_records(5).into_iter()
            .filter(|r| r.object_name.starts_with("SYN-POLAR")).take(20).collect();
        let c = Catalog::build(recs, &Membership::default());
        for o in &c.objects {
            let pieces = ground_track(o, o.record.epoch_unix);
            assert!(pieces.len() >= 2, "1.5 orbits of a polar track must cross the antimeridian");
            for piece in &pieces {
                assert!(piece.len() >= 2, "a piece of fewer than 2 points");
                for w in piece.windows(2) {
                    assert!((w[1].0 - w[0].0).abs() < 180.0, "a wrap segment {:?} -> {:?}", w[0], w[1]);
                    assert!((-90.0..=90.0).contains(&w[0].1));
                }
            }
            // Consecutive pieces meet at the antimeridian, at the same latitude.
            for w in pieces.windows(2) {
                let (end, start) = (w[0].last().unwrap(), w[1][0]);
                assert!((end.0.abs() - 180.0).abs() < 1e-9 && (start.0.abs() - 180.0).abs() < 1e-9 && (end.1 - start.1).abs() < 1e-9);
            }
        }
    }

    /// Task 7 review Minor 5: a near-90° orbit passing within one 30 s step
    /// of a pole jumps 90-180° of longitude between samples without
    /// crossing the antimeridian the short way. No piece may hold such a
    /// jump: it would draw a chord straight across the map.
    #[test]
    fn a_near_polar_track_never_draws_a_chord_over_the_cap() {
        let recs = crate::orbit::omm::parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let iss = recs.into_iter().find(|r| r.norad_id == ISS_NORAD).unwrap();
        let mut records = Vec::new();
        for (k, incl) in [89.9, 89.95, 89.99, 88.0, 86.4].into_iter().enumerate() {
            for j in 0..24u32 {
                let mut r = iss.clone();
                r.norad_id = 900_000 + (k as u32) * 100 + j;
                r.inclination_deg = incl;
                r.raan_deg = f64::from(j) * 15.0;
                r.mean_anomaly_deg = f64::from(j) * 7.3;
                r.arg_pericenter_deg = 0.0;
                records.push(r);
            }
        }
        let c = Catalog::build(records, &Membership::default());
        assert_eq!(c.objects.len(), 120);
        let mut polar_breaks = 0;
        for o in &c.objects {
            let pieces = ground_track(o, o.record.epoch_unix);
            for piece in &pieces {
                for w in piece.windows(2) {
                    let d = (w[1].0 - w[0].0).abs();
                    assert!(d <= 90.0, "incl {}: a {d:.1}° chord {:?} -> {:?}", o.record.inclination_deg, w[0], w[1]);
                }
            }
            // Pieces that do not meet at the antimeridian were broken over a cap.
            polar_breaks += pieces.windows(2).filter(|w| (w[0].last().unwrap().0.abs() - 180.0).abs() > 1e-9).count();
        }
        assert!(polar_breaks > 0, "the set must actually pass near a pole");
    }

    /// Task 7 review Minor 1: the track is Earth-fixed. Successive
    /// ascending equator crossings in ONE track fall about `P ω⊕` (23.3°
    /// for the ISS) further west. A track rotated by one fixed GMST, or a
    /// TEME track, shifts only by nodal precession (~0.35°) and fails.
    #[test]
    fn the_iss_ground_track_shifts_west_each_pass() {
        let recs = crate::orbit::omm::parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let c = Catalog::build(recs, &Membership::default());
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let period_s = 1440.0 / iss.record.mean_motion * 60.0;
        // An ascending node near the epoch, by bisection on TEME z.
        let z = |t: f64| state_at(iss, t).unwrap().position_km[2];
        let t0 = iss.record.epoch_unix;
        let mut a = (0..400).map(|k| t0 + f64::from(k) * 15.0).find(|&t| z(t) < 0.0 && z(t + 15.0) >= 0.0).expect("a node within 100 min");
        let mut b = a + 15.0;
        for _ in 0..40 {
            let m = 0.5 * (a + b);
            if z(m) < 0.0 { a = m } else { b = m }
        }
        // The window [t - P/2, t + P] then holds the node at t - P/4 and the next at t + 3P/4.
        let t = b + period_s / 4.0;
        let crossings: Vec<f64> = ground_track(iss, t)
            .iter()
            .flat_map(|piece| piece.windows(2).filter(|w| w[0].1 < 0.0 && w[1].1 >= 0.0).map(|w| {
                let f = -w[0].1 / (w[1].1 - w[0].1);
                w[0].0 + f * (w[1].0 - w[0].0)
            }).collect::<Vec<_>>())
            .collect();
        assert_eq!(crossings.len(), 2, "two ascending nodes in 1.5 orbits: {crossings:?}");
        let west = crate::orbit::omm::wrap_deg(crossings[0] - crossings[1]);
        let west = if west > 180.0 { west - 360.0 } else { west };
        let expected = period_s * 7.292_115_146_7e-5_f64.to_degrees();
        assert!((west - expected).abs() < 1.0, "the next pass is {west:.2}° west, expected {expected:.2}° (P ω⊕)");
    }

    #[test]
    fn the_iss_track_covers_one_and_a_half_orbits_at_thirty_seconds() {
        let recs = crate::orbit::omm::parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let c = Catalog::build(recs, &Membership::default());
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let pieces = ground_track(iss, iss.record.epoch_unix);
        let n: usize = pieces.iter().map(Vec::len).sum();
        let expected = (1.5_f64 * 1440.0 / 15.48680135 * 60.0 / 30.0).floor() as usize + 1;
        assert!(n >= expected && n <= expected + 2 * pieces.len(), "{n} points for {expected} samples");
        assert!(pieces.iter().flatten().all(|p| p.1.abs() <= 51.7), "the ISS never flies past its inclination");
    }

    /// Final review M5: a deep-space object's track is bounded. TESS (0.073
    /// rev/day, P ~ 13.7 d, e ~ 0.2) at 30 s would take ~59,000 samples on
    /// the UI thread; it takes at most `GROUND_TRACK_MAX_STEPS + 1` and still
    /// spans 1.5 periods. The ISS keeps its 30 s step.
    #[test]
    fn a_deep_space_ground_track_takes_a_bounded_number_of_samples() {
        let recs = crate::orbit::omm::parse_omm(STATIONS).unwrap().records;
        let mut tess = recs.into_iter().find(|r| r.norad_id == ISS_NORAD).unwrap();
        tess.norad_id = 43_435;
        tess.mean_motion = 0.0727;
        tess.eccentricity = 0.21;
        tess.inclination_deg = 37.0;
        tess.bstar = 0.0;
        tess.mean_motion_dot = 0.0;
        let c = Catalog::build(vec![tess], &Membership::default());
        let obj = &c.objects[0];
        let period_s = 1440.0 / obj.record.mean_motion * 60.0;
        assert!((1.5 * period_s / GROUND_TRACK_STEP_S) > 50_000.0, "the uncapped count this guards against");
        let pieces = ground_track(obj, obj.record.epoch_unix + 30.0 * 86_400.0);
        let n: usize = pieces.iter().map(Vec::len).sum();
        assert!(n >= GROUND_TRACK_MAX_STEPS / 2, "the track is drawn, not dropped: {n} points");
        assert!(n <= GROUND_TRACK_MAX_STEPS + 1 + 2 * pieces.len(), "{n} points: the sample count is not capped");
        let step = ground_track_step_s(period_s);
        assert!((step - 1.5 * period_s / GROUND_TRACK_MAX_STEPS as f64).abs() < 1e-9 && step > GROUND_TRACK_STEP_S);
        assert_eq!(ground_track_step_s(1440.0 / 15.48680135 * 60.0), GROUND_TRACK_STEP_S, "LEO keeps 30 s");
    }

    #[test]
    fn the_ring_is_one_closed_period_in_teme() {
        let recs = crate::orbit::omm::parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let c = Catalog::build(recs, &Membership::default());
        let iss = &c.objects[c.index_of(ISS_NORAD).unwrap()];
        let ring = orbit_ring_teme(iss, iss.record.epoch_unix, 256);
        assert_eq!(ring.len(), 256);
        let d = derived(&iss.record);
        for p in &ring { let alt = p.length() / 1000.0 - 6_378.137; assert!(alt > d.perigee_km - 30.0 && alt < d.apogee_km + 30.0, "{alt}"); }
        // Inertial: ten minutes later the ring barely moves in TEME (precession only).
        let later = orbit_ring_teme(iss, iss.record.epoch_unix + 600.0, 256);
        let min_gap = later.iter().map(|q| ring.iter().map(|p| (*p - *q).length()).fold(f64::MAX, f64::min)).fold(0.0, f64::max);
        assert!(min_gap < 200_000.0, "the TEME ring moved {min_gap} m in 10 min");
    }

    #[test]
    fn the_ground_track_geojson_is_one_multilinestring_feature() {
        let json = ground_track_geojson(&[vec![(170.0, 1.0), (180.0, 2.0)], vec![(-180.0, 2.0), (-170.0, 3.0)]], "#ff5a36");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "FeatureCollection");
        let f = &v["features"][0];
        assert_eq!((f["properties"]["kind"].as_str(), f["properties"]["color"].as_str()), (Some("ground-track"), Some("#ff5a36")));
        assert_eq!(f["geometry"]["type"], "MultiLineString");
        assert_eq!(f["geometry"]["coordinates"][1][0], serde_json::json!([-180.0, 2.0]));
    }

    #[test]
    fn derived_elements_of_the_iss() {
        let recs = parse_omm(STATIONS).unwrap().records;
        let iss = recs.iter().find(|r| r.norad_id == 25544).unwrap();
        let d = derived(iss);
        assert!((d.period_min - 92.98).abs() < 0.01);
        assert!((380.0..440.0).contains(&d.perigee_km) && (380.0..450.0).contains(&d.apogee_km));
        assert!(d.apogee_km >= d.perigee_km);
    }

    #[test]
    fn geodetic_altitude_at_the_equator_and_the_pole() {
        assert!((geodetic_altitude_km([6_378.137 + 400.0, 0.0, 0.0]) - 400.0).abs() < 1e-6);
        assert!((geodetic_altitude_km([0.0, 0.0, 6_356.752_314 + 400.0]) - 400.0).abs() < 1e-3);
    }

    #[test]
    fn teme_to_ecef_turns_the_x_axis_back_by_gmst() {
        let e = teme_to_ecef_km([7_000.0, 0.0, 100.0], std::f64::consts::FRAC_PI_2);
        assert!(e[0].abs() < 1e-9 && (e[1] + 7_000.0).abs() < 1e-9 && e[2] == 100.0, "{e:?}");
    }

    /// MG-S1 (spec §1.4, §5.1). Run in release: `-- --ignored --nocapture mg_s1`.
    #[test]
    #[ignore = "measurement: run explicitly in --release"]
    fn mg_s1_propagation_cost() {
        let recs = crate::orbit::synthetic::synthetic_records(1);
        let c = crate::orbit::catalog::Catalog::build(recs.clone(), &crate::orbit::synthetic::synthetic_membership(&recs));
        let mut runs = Vec::new();
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let mut sink = 0.0;
            let mut ok = 0usize;
            for step in 0..100 {
                let t = crate::orbit::synthetic::SYNTHETIC_EPOCH_UNIX + f64::from(step) * 60.0;
                for o in &c.objects {
                    if let Some(s) = state_at(o, t) {
                        sink += s.position_km[0];
                        ok += 1;
                    }
                }
            }
            std::hint::black_box(sink);
            // A fast figure from early-out errors would be a lie: every call must propagate.
            assert_eq!(ok, 100 * c.objects.len(), "some propagations failed");
            runs.push(t0.elapsed().as_nanos() as f64 / (100.0 * c.objects.len() as f64));
        }
        runs.sort_by(f64::total_cmp);
        let ns = runs[2];
        let keyframe_ms = ns * 10_000.0 / 1e6;
        let wasm_chunk = ((1.0e6 / (3.0 * ns)).floor() as usize).clamp(100, 1_000);
        println!("MG-S1 ns_per_propagation={ns:.1} keyframe_10k_ms={keyframe_ms:.2} wasm_chunk={wasm_chunk} runs={runs:?}");
        assert!(keyframe_ms <= 25.0, "MG-S1 FAIL: a 10k keyframe costs {keyframe_ms:.1} ms (criterion <= 25 ms)");
    }

    /// MG-S1b: the same cost with the scene clock days past the epoch, where
    /// the stateless `propagate` re-integrates resonant orbits from epoch
    /// (see `state_at`). Prints the whole set and the Molniya part alone.
    /// `-- --ignored --nocapture mg_s1b`.
    #[test]
    #[ignore = "measurement: run explicitly in --release"]
    fn mg_s1b_propagation_cost_days_after_epoch() {
        let recs = crate::orbit::synthetic::synthetic_records(1);
        let c = Catalog::build(recs.clone(), &crate::orbit::synthetic::synthetic_membership(&recs));
        let molniya: Vec<&SatObject> = c.objects.iter().filter(|o| o.record.object_name.starts_with("SYN-MOLNIYA")).collect();
        let all: Vec<&SatObject> = c.objects.iter().collect();
        let ns = |set: &[&SatObject], t0: f64| {
            let mut runs = Vec::new();
            for _ in 0..3 {
                let start = std::time::Instant::now();
                let (mut sink, mut ok) = (0.0, 0usize);
                for step in 0..20 {
                    let t = t0 + f64::from(step) * 60.0;
                    for o in set {
                        if let Some(s) = state_at(o, t) {
                            sink += s.position_km[0];
                            ok += 1;
                        }
                    }
                }
                std::hint::black_box(sink);
                assert_eq!(ok, 20 * set.len(), "some propagations failed");
                runs.push(start.elapsed().as_nanos() as f64 / (20.0 * set.len() as f64));
            }
            runs.sort_by(f64::total_cmp);
            runs[1]
        };
        for days in [0.0, 3.0, 7.0, 30.0] {
            let t0 = crate::orbit::synthetic::SYNTHETIC_EPOCH_UNIX + days * 86_400.0;
            let (n_all, n_mol) = (ns(&all, t0), ns(&molniya, t0));
            let keyframe_ms = n_all * 10_000.0 / 1e6;
            println!("MG-S1b +{days} d: all {n_all:.1} ns/prop, keyframe_10k_ms={keyframe_ms:.2}; molniya {n_mol:.1} ns/prop");
            assert!(keyframe_ms <= 25.0, "MG-S1b FAIL at +{days} d: {keyframe_ms:.1} ms");
        }
    }

    #[test]
    fn the_renderers_gmst_is_sgp4s_to_the_last_bit_that_matters() {
        for year in (1957..=2100).step_by(7) {
            let unix = (f64::from(year) - 1970.0) * 365.2425 * 86_400.0;
            let a = crate::render::altitude::gmst_rad(unix);
            let b = crate::orbit::gmst_rad(unix);
            let d = (a - b).rem_euclid(std::f64::consts::TAU);
            assert!(d.min(std::f64::consts::TAU - d) < 1e-12, "{year}: {a} vs {b}");
        }
    }

    /// MG-S2 (spec §3.2, §5.2): the worst Hermite position error against direct propagation.
    #[test]
    fn mg_s2_hermite_error() {
        use crate::render::altitude::{hermite, KEYFRAME_DELTA_MAX_S};
        let stations = crate::orbit::omm::parse_omm(include_bytes!("../../tests/fixtures/orbit/stations.json")).unwrap().records;
        let ecc = crate::orbit::synthetic::eccentric_records(3);
        let mut table = Vec::new();
        for (name, recs) in [("stations", stations), ("molniya+gto", ecc)] {
            let c = crate::orbit::catalog::Catalog::build(recs, &Default::default());
            for delta in [60.0, 120.0, 180.0, 240.0, 300.0] {
                let mut worst = 0.0f64;
                for o in &c.objects {
                    for k in 0..48 {
                        // t0 every 30 min over a day
                        let t0 = o.record.epoch_unix + f64::from(k) * 1_800.0;
                        let (Some(a), Some(b)) = (state_at(o, t0), state_at(o, t0 + delta)) else { continue };
                        for s in [0.25, 0.5, 0.75] {
                            let Some(d) = state_at(o, t0 + s * delta) else { continue };
                            let v = |x: [f64; 3]| glam::DVec3::from(x);
                            let h = hermite(v(a.position_km), v(a.velocity_km_s), v(b.position_km), v(b.velocity_km_s), delta, s);
                            worst = worst.max((h - v(d.position_km)).length());
                        }
                    }
                }
                table.push((name, delta, worst));
            }
        }
        for (name, delta, worst) in &table {
            println!("MG-S2 {name} delta={delta} worst_km={worst:.6}");
        }
        // The criterion is judged at the cap the renderer actually uses; all five rows are recorded.
        let at_max = table.iter().filter(|r| (r.1 - KEYFRAME_DELTA_MAX_S).abs() < 1e-9).map(|r| r.2).fold(0.0, f64::max);
        assert!(at_max < 1.0, "MG-S2 FAIL at delta {KEYFRAME_DELTA_MAX_S}: {at_max} km (criterion < 1 km)");
    }
}
