//! CelesTrak OMM data and SGP4 propagation (spec 2026-09-29-satellites-design.md §1, §3.2, §4.4).
//!
//! - [`omm`]: a bounded, never-panicking parse of a CelesTrak / Space-Track
//!   OMM JSON body into validated [`omm::OmmRecord`]s.
//! - [`catalog`]: the drawn set -- de-duplicated, co-located stacks collapsed
//!   onto one dot, classified into [`catalog::Group`]s, each with its SGP4
//!   constants.
//! - [`propagate`]: TEME state at a time, the TEME to Earth-fixed rotation and
//!   the WGS84 geodetic altitude.
//! - [`synthetic`]: a fixed 10,000-object set for tests and measurement, so no
//!   test ever touches the network.
pub mod catalog;
pub mod omm;
pub mod propagate;
pub mod synthetic;

/// J2000.0, 2000-01-01T12:00:00Z, as Unix seconds.
pub const J2000_UNIX_S: f64 = 946_728_000.0;

/// Julian years (365.25 days) since J2000.0 -- the argument sgp4's sidereal
/// time functions take.
#[must_use]
pub fn years_since_j2000(unix_s: f64) -> f64 {
    (unix_s - J2000_UNIX_S) / (86_400.0 * 365.25)
}

/// Greenwich mean sidereal time in radians, `[0, 2π)`, by the IAU formula
/// the sgp4 crate recommends (the same one its `propagate` assumes).
#[must_use]
pub fn gmst_rad(unix_s: f64) -> f64 {
    sgp4::iau_epoch_to_sidereal_time(years_since_j2000(unix_s))
}

#[cfg(test)]
mod tests {
    #[test]
    fn gmst_at_j2000_is_the_iau_constant() {
        let deg = super::gmst_rad(super::J2000_UNIX_S).to_degrees();
        assert!((deg - 280.460_618_37).abs() < 1e-6, "{deg}");
    }

    /// Vallado, *Fundamentals of Astrodynamics and Applications*, Example
    /// 3-5: 1992-08-20 12:14:00 UT (Unix 714,312,840) gives GMST
    /// 152.578787810°. At J2000 every time-dependent term is zero, so this
    /// point is what catches a wrong time scale: one day's error is ~0.99°,
    /// one second's ~0.004°, both far outside 1e-6°.
    #[test]
    fn gmst_matches_vallado_example_3_5() {
        let deg = super::gmst_rad(714_312_840.0).to_degrees();
        assert!((deg - 152.578_787_810).abs() < 1e-6, "{deg}");
    }
}
