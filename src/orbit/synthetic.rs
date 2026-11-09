//! A fixed 10,000-object catalogue (spec §5.4) for tests and measurement,
//! so nothing under test depends on the network or on today's sky.
//!
//! All records share [`SYNTHETIC_EPOCH_UNIX`], have no drag (`BSTAR = 0`,
//! both mean-motion derivatives 0) and take NORAD ids from 900,000,001
//! upward, in this order:
//!
//! | Part | Count | Orbit |
//! |---|---|---|
//! | `STARLINK-SYN-nnnnn` | 7,000 | Walker 53°:7000/70/1 at 550 km |
//! | `SYN-POLAR-nnnn` | 1,500 | 500–1,200 km uniform, i = 97°, random RAAN and anomaly |
//! | `SYN-GPS-nn` | 31 | 20,200 km, i = 55°, 6 planes |
//! | `SYN-GEO-nnn` | 500 | 35,786 km, i = 0.05°, e = 1e-4, spread evenly in longitude |
//! | `SYN-MOLNIYA-nnn` | 485 | 600 × 39,700 km, i = 63.4°, ω = 270° |
//! | `SYN-GTO-nnn` | 484 | 250 × 35,786 km, i = 27° |
//!
//! Each part draws from its own generator (seeded from `seed` and the part),
//! so [`eccentric_records`] is exactly the tail of [`synthetic_records`].

use super::catalog::Membership;
use super::omm::OmmRecord;
use std::collections::HashSet;

/// 2026-09-21T14:13:20Z, fixed.
pub const SYNTHETIC_EPOCH_UNIX: f64 = 1_790_000_000.0;

const FIRST_ID: u32 = 900_000_001;
const MU: f64 = 398_600.441_8;
const RE: f64 = 6_378.137;
/// "Circular" orbits get this eccentricity, the GEO one of spec §5.4.
const E_CIRCULAR: f64 = 1e-4;

const N_STARLINK: usize = 7_000;
const N_POLAR: usize = 1_500;
const N_GPS: usize = 31;
const N_GEO: usize = 500;
const N_MOLNIYA: usize = 485;
const N_GTO: usize = 484;
const N_CIRCULAR: usize = N_STARLINK + N_POLAR + N_GPS + N_GEO;

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64, part: u64) -> Self {
        Self(seed ^ part.wrapping_mul(0xA076_1D64_78BD_642F))
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

/// Revolutions per day of an orbit with semi-major axis `a_km`.
fn mean_motion(a_km: f64) -> f64 {
    86_400.0 / (std::f64::consts::TAU * (a_km.powi(3) / MU).sqrt())
}

struct Orbit {
    a_km: f64,
    e: f64,
    incl: f64,
    raan: f64,
    argp: f64,
    ma: f64,
}

fn circular(alt_km: f64, incl: f64, raan: f64, ma: f64) -> Orbit {
    Orbit { a_km: RE + alt_km, e: E_CIRCULAR, incl, raan, argp: 0.0, ma }
}

fn eccentric(perigee_km: f64, apogee_km: f64, incl: f64, raan: f64, argp: f64, ma: f64) -> Orbit {
    let a_km = RE + (perigee_km + apogee_km) / 2.0;
    Orbit { a_km, e: (apogee_km - perigee_km) / (2.0 * a_km), incl, raan, argp, ma }
}

fn record(id: u32, name: String, o: &Orbit) -> OmmRecord {
    let epoch = chrono::DateTime::from_timestamp(SYNTHETIC_EPOCH_UNIX as i64, 0)
        .expect("a fixed, valid timestamp")
        .naive_utc();
    OmmRecord {
        object_name: name.into_boxed_str(),
        object_id: format!("SYN-{}", id - FIRST_ID + 1).into_boxed_str(),
        norad_id: id,
        epoch,
        epoch_unix: SYNTHETIC_EPOCH_UNIX,
        mean_motion: mean_motion(o.a_km),
        eccentricity: o.e,
        inclination_deg: o.incl,
        raan_deg: super::omm::wrap_deg(o.raan),
        arg_pericenter_deg: super::omm::wrap_deg(o.argp),
        mean_anomaly_deg: super::omm::wrap_deg(o.ma),
        bstar: 0.0,
        mean_motion_dot: 0.0,
        mean_motion_ddot: 0.0,
        element_set_no: 999,
        rev_at_epoch: 0,
    }
}

fn circular_part(seed: u64, out: &mut Vec<OmmRecord>) {
    let mut id = FIRST_ID;
    let mut push = |out: &mut Vec<OmmRecord>, name: String, o: Orbit| {
        out.push(record(id, name, &o));
        id += 1;
    };
    // Walker 53°:7000/70/1 -- 70 planes of 100, phase offset 360·F/T per plane.
    for p in 0..70 {
        for s in 0..100 {
            let k = p * 100 + s;
            let (pf, sf) = (p as f64, s as f64);
            let o = circular(550.0, 53.0, 360.0 * pf / 70.0, 360.0 * (sf / 100.0) + 360.0 * pf / 7_000.0);
            push(out, format!("STARLINK-SYN-{k:05}"), o);
        }
    }
    let mut rng = SplitMix64::new(seed, 1);
    for k in 0..N_POLAR {
        let o = circular(rng.range(500.0, 1_200.0), 97.0, rng.range(0.0, 360.0), rng.range(0.0, 360.0));
        push(out, format!("SYN-POLAR-{k:04}"), o);
    }
    // 31 GPS in 6 planes (60° apart), 6 slots a plane, the planes staggered.
    for k in 0..N_GPS {
        let (plane, slot) = ((k % 6) as f64, (k / 6) as f64);
        let o = circular(20_200.0, 55.0, 60.0 * plane, 60.0 * slot + 10.0 * plane);
        push(out, format!("SYN-GPS-{k:02}"), o);
    }
    // GEO: one node, the longitudes set by the mean anomaly, evenly spread.
    for k in 0..N_GEO {
        let o = circular(35_786.0, 0.05, 0.0, 360.0 * k as f64 / N_GEO as f64);
        push(out, format!("SYN-GEO-{k:03}"), o);
    }
}

fn eccentric_part(seed: u64) -> Vec<OmmRecord> {
    let mut out = Vec::with_capacity(N_MOLNIYA + N_GTO);
    let mut id = FIRST_ID + N_CIRCULAR as u32;
    let mut rng = SplitMix64::new(seed, 2);
    for k in 0..N_MOLNIYA {
        let o = eccentric(600.0, 39_700.0, 63.4, rng.range(0.0, 360.0), 270.0, rng.range(0.0, 360.0));
        out.push(record(id, format!("SYN-MOLNIYA-{k:03}"), &o));
        id += 1;
    }
    for k in 0..N_GTO {
        let o = eccentric(250.0, 35_786.0, 27.0, rng.range(0.0, 360.0), rng.range(0.0, 360.0), rng.range(0.0, 360.0));
        out.push(record(id, format!("SYN-GTO-{k:03}"), &o));
        id += 1;
    }
    out
}

/// Exactly 10,000 records, the same for the same `seed`.
#[must_use]
pub fn synthetic_records(seed: u64) -> Vec<OmmRecord> {
    let mut out = Vec::with_capacity(N_CIRCULAR + N_MOLNIYA + N_GTO);
    circular_part(seed, &mut out);
    out.extend(eccentric_part(seed));
    out
}

/// The Molniya and GTO records alone (969), identical to the tail of
/// [`synthetic_records`] for the same `seed`.
#[must_use]
pub fn eccentric_records(seed: u64) -> Vec<OmmRecord> {
    eccentric_part(seed)
}

/// `SYN-GPS*` in `gps`, `SYN-GEO*` in `geo`, and an empty (downloaded but
/// empty) `stations`.
#[must_use]
pub fn synthetic_membership(records: &[OmmRecord]) -> Membership {
    let ids = |p: &str| -> HashSet<u32> {
        records.iter().filter(|r| r.object_name.starts_with(p)).map(|r| r.norad_id).collect()
    };
    Membership {
        stations: Some(HashSet::new()),
        gps: Some(ids("SYN-GPS")),
        geo: Some(ids("SYN-GEO")),
        galileo: Some(HashSet::new()),
        glonass: Some(HashSet::new()),
        beidou: Some(HashSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_set_has_the_spec_composition_and_is_deterministic() {
        let a = synthetic_records(7);
        assert_eq!(a.len(), 10_000);
        assert_eq!(a, synthetic_records(7));
        let count = |p: &str| a.iter().filter(|r| r.object_name.starts_with(p)).count();
        assert_eq!(count("STARLINK-SYN"), 7_000);
        assert_eq!(count("SYN-POLAR"), 1_500);
        assert_eq!(count("SYN-GPS"), 31);
        assert_eq!(count("SYN-GEO"), 500);
        assert_eq!(count("SYN-MOLNIYA") + count("SYN-GTO"), 969);
        let c = crate::orbit::catalog::Catalog::build(a.clone(), &synthetic_membership(&a));
        assert_eq!(c.skipped_by_sgp4, 0, "every synthetic record is accepted by sgp4");
        assert_eq!(c.objects.len(), 10_000, "no two synthetic objects share elements");
    }

    #[test]
    fn the_eccentric_records_are_the_tail_of_the_full_set() {
        let all = synthetic_records(3);
        assert_eq!(eccentric_records(3), all[N_CIRCULAR..]);
        assert_ne!(synthetic_records(3), synthetic_records(4));
    }

    #[test]
    fn the_orbits_have_the_asked_for_shape() {
        let a = synthetic_records(7);
        let d = |p: &str| crate::orbit::propagate::derived(a.iter().find(|r| r.object_name.starts_with(p)).unwrap());
        let s = d("STARLINK-SYN");
        assert!((s.perigee_km - 549.0).abs() < 2.0 && (s.apogee_km - 551.0).abs() < 2.0, "{s:?}");
        let m = d("SYN-MOLNIYA");
        assert!((m.perigee_km - 600.0).abs() < 1e-6 && (m.apogee_km - 39_700.0).abs() < 1e-6, "{m:?}");
        let g = d("SYN-GEO");
        assert!((g.period_min - 1_436.1).abs() < 0.1, "a sidereal day: {g:?}");
    }
}
