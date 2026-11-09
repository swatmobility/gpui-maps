//! Where the sun actually is, for a given instant.
//!
//! The one thing `render::atmosphere::Sun::Map` needed and did not have: a
//! clock. Task 9 shipped the subsolar-point variant and refused to make it
//! the default on the stated grounds that "a map-anchored sun without a
//! clock is an arbitrary claim". This module is the clock, so the claim
//! stops being arbitrary.
//!
//! # The algorithm, and its stated accuracy
//!
//! **The *Astronomical Almanac*'s low-precision formulae for the Sun**
//! (U.S. Naval Observatory / H.M. Nautical Almanac Office), the short series
//! that the Almanac's own Section C carries for computer use. Reproduced
//! here as:
//!
//! ```text
//!   n  = JD(TT) - 2451545.0                      days since J2000.0
//!   L  = 280.460 deg + 0.9856474 deg * n          mean longitude
//!   g  = 357.528 deg + 0.9856003 deg * n          mean anomaly
//!   lambda = L + 1.915 deg sin g + 0.020 deg sin 2g    ecliptic longitude
//!   eps = 23.439 deg - 0.0000004 deg * n          obliquity of the ecliptic
//!   delta = asin(sin eps * sin lambda)            declination
//!   alpha = atan2(cos eps * sin lambda, cos lambda)   right ascension
//! ```
//!
//! **Accuracy as stated by the source: about 0.01 degrees (36 arcseconds)
//! in ecliptic longitude, for dates between 1950 and 2050.** Nothing here
//! invents a coefficient; every number above is the published one, and the
//! declination and right-ascension expressions are the published ones too.
//!
//! 0.01 degrees is far more accuracy than the thing it is used for can
//! resolve. A 700 px globe has 2,199 px of circumference, so one degree of
//! longitude is 6.1 px and 0.01 degrees is **0.06 px** — a fifteenth of a
//! pixel of terminator position. See
//! [`the_low_precision_formulae_agree_with_a_higher_order_series`] for what
//! was measured rather than quoted.
//!
//! # Why this is written out rather than taken from a crate
//!
//! It is six lines of arithmetic and two trig calls with no state, no
//! allocation and no error case. The candidate crates (`sun`, `spa`,
//! `sun-times`, `astro`) each bring a time library with them —
//! `chrono` or `time` — which this crate does not currently link and which
//! would reach all four applications compiling it. That is a dependency
//! edge, a version-unification hazard and a second date arithmetic for the
//! sake of the six lines above. The *reference* is cited instead of the
//! code being borrowed, which is the same trade `render::space`'s star
//! catalogue makes.
//!
//! # The subsolar point, derived rather than asserted
//!
//! The subsolar latitude is the declination: by definition the sun is
//! overhead where its zenith distance is zero, and on a sphere that is the
//! parallel equal to its declination.
//!
//! The longitude is the negated Greenwich hour angle of the sun. Writing
//! `GHA_mean` for the mean sun's and `EoT` for the equation of time in
//! degrees,
//!
//! ```text
//!   GHA_mean = 15 deg * (UTC hours - 12)
//!   EoT      = alpha_mean_sun - alpha  ~=  L - alpha
//!   GHA      = GHA_mean + EoT
//!   subsolar longitude = -GHA
//!                      = -15 deg * (UTC hours - 12) - EoT
//! ```
//!
//! The mean sun is the fictitious body whose right ascension is the real
//! sun's *mean* longitude, which is the `L` above — so `EoT = L - alpha`
//! needs no extra series. The sign is pinned by a fact independent of this
//! file: in early November the equation of time is at its yearly maximum of
//! about +16.4 minutes, which means apparent noon at Greenwich falls
//! *before* 12:00 UTC (NOAA's solar calculator gives 11:43:31 UTC for
//! 2024-11-03), so at 12:00 UTC the subsolar point has already crossed the
//! meridian and lies **west** — a negative longitude.
//! [`the_equation_of_time_has_its_published_extremes_on_its_published_dates`]
//! asserts both the magnitude and that sign.
//!
//! # Terrestrial Time, and the correction deliberately not made
//!
//! `n` above is counted in Terrestrial Time, and the input here is UTC. The
//! two differ by TT − UTC ≈ 69 s in the 2020s, i.e. 0.0008 days, which moves
//! `L` by 0.0008 degrees and `alpha` by about the same — and in `L - alpha`
//! most of that cancels. It is two orders of magnitude inside the formula's
//! own 0.01-degree error and four orders inside a pixel, so no ΔT table is
//! carried. Said here rather than left to be discovered.

/// Seconds from the Unix epoch to J2000.0 (2000-01-01 12:00 TT), in days.
///
/// `JD(Unix 0) = 2440587.5` and `J2000.0 = JD 2451545.0`, so
/// `n = unix_days - 10957.5`. Checked rather than asserted:
/// `the_epoch_offset_puts_n_at_zero_on_the_j2000_instant`.
const J2000_UNIX_DAYS: f64 = 10_957.5;

/// Where the sun is directly overhead at `unix_seconds`, as
/// `(longitude_deg, latitude_deg)`.
///
/// Longitude is east-positive and reduced to `(-180, 180]`; latitude is the
/// sun's declination and never leaves `[-23.45, 23.45]`. `unix_seconds` is
/// UTC seconds since 1970-01-01, the same scale [`now_unix_seconds`]
/// returns — leap seconds are absent from that scale, which is what makes
/// `(t mod 86400) / 3600` the UTC hour of the day.
///
/// See the module docs for the algorithm and its stated accuracy.
#[must_use]
pub fn subsolar_point(unix_seconds: f64) -> (f64, f64) {
    let (declination, right_ascension, mean_longitude) = solar_equatorial(unix_seconds);
    let equation_of_time = wrap_180(mean_longitude - right_ascension);
    // `rem_euclid`, not `%`: a negative `unix_seconds` (a date before 1970,
    // which `the_formulae_span_the_sources_own_1950_to_2050_validity` does
    // reach) must still give an hour of the day in [0, 24).
    let utc_hours = unix_seconds.rem_euclid(86_400.0) / 3_600.0;
    let longitude = wrap_180(-15.0 * (utc_hours - 12.0) - equation_of_time);
    (longitude, declination)
}

/// The equation of time at `unix_seconds`, in **degrees** of longitude —
/// positive when apparent solar time runs ahead of mean solar time.
///
/// Multiply by 4 for minutes of time (15 degrees per hour). Public because
/// it is the half of [`subsolar_point`] that can be checked against a
/// published table, and a caller that wants it should not have to re-derive
/// it from a longitude.
#[must_use]
pub fn equation_of_time_deg(unix_seconds: f64) -> f64 {
    let (_, right_ascension, mean_longitude) = solar_equatorial(unix_seconds);
    wrap_180(mean_longitude - right_ascension)
}

/// The sun's declination at `unix_seconds`, in degrees — the subsolar
/// latitude, and the quantity the solstices and equinoxes pin.
#[must_use]
pub fn declination_deg(unix_seconds: f64) -> f64 {
    solar_equatorial(unix_seconds).0
}

/// `(declination_deg, right_ascension_deg, mean_longitude_deg)`, all three
/// from one evaluation of the series.
///
/// One function rather than three, because the equation of time is a
/// *difference* of two of them and computing `L` twice from two different
/// `n` values would put a spurious drift into it.
fn solar_equatorial(unix_seconds: f64) -> (f64, f64, f64) {
    let n = unix_seconds / 86_400.0 - J2000_UNIX_DAYS;
    // Reduced to [0, 360) before the trig, not because the trig needs it —
    // `f64::sin` is exact enough over the raw range — but so that
    // `mean_longitude` is a bounded quantity the equation of time's
    // `wrap_180` can subtract a right ascension from.
    let mean_longitude = (280.460 + 0.985_647_4 * n).rem_euclid(360.0);
    let mean_anomaly = (357.528 + 0.985_600_3 * n).rem_euclid(360.0).to_radians();
    let ecliptic_longitude =
        (mean_longitude + 1.915 * mean_anomaly.sin() + 0.020 * (2.0 * mean_anomaly).sin())
            .to_radians();
    let obliquity = (23.439 - 0.000_000_4 * n).to_radians();
    let declination =
        (obliquity.sin() * ecliptic_longitude.sin()).clamp(-1.0, 1.0).asin().to_degrees();
    let right_ascension =
        (obliquity.cos() * ecliptic_longitude.sin()).atan2(ecliptic_longitude.cos()).to_degrees();
    (declination, right_ascension, mean_longitude)
}

/// Reduces an angle in degrees to `(-180, 180]`.
fn wrap_180(degrees: f64) -> f64 {
    (degrees + 180.0).rem_euclid(360.0) - 180.0
}

/// UTC seconds since the Unix epoch, or `None` where this platform has no
/// wall clock to ask.
///
/// # Two platforms, two clocks, and why the wasm arm is `cfg`-gated
///
/// `std::time::SystemTime::now()` compiles for
/// `wasm32-unknown-unknown` and then **panics** at run time: the target has
/// no clock for std to read, and the panic message is
/// "time not implemented on this platform". So a call that looks portable is
/// the worst of the three options — it builds, it ships, and it aborts the
/// browser's first globe frame.
///
/// The browser arm therefore reads `js_sys::Date::now()`, which is
/// milliseconds since the Unix epoch by the `Date` specification, the same
/// route `web/src/app.rs` already takes to `js_sys` for
/// `decodeURIComponent` and `performance.memory`. **`js-sys` is not in this
/// package's manifest and deliberately is not**: `gpui-maps` links gpui,
/// tokio's multi-threaded runtime, reqwest, pmtiles' mmap and objc2 and
/// cannot be built for wasm at all (see the comment at the top of
/// `web/Cargo.toml`). The only manifest that ever compiles this file for
/// wasm is `web/Cargo.toml`, which has `js-sys = "0.3"`; adding it here
/// would put `js-sys` and `wasm-bindgen` into four applications'
/// lockfiles to satisfy a `cfg` arm their targets never take.
/// `./scripts/check-wasm.sh` is what proves the arm compiles.
///
/// # `Option`, not a panic and not a zero
///
/// A zero would be 1970-01-01T00:00Z, which is a perfectly plausible-looking
/// subsolar point (lon 178.58, lat −23.01) and therefore the wrong failure:
/// the caller could not tell a broken clock from midnight on New Year's Day.
/// `None` lets `render::atmosphere::Sun::Now` fall back to a sun it can
/// name.
#[must_use]
pub fn now_unix_seconds() -> Option<f64> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs_f64())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let millis = js_sys::Date::now();
        // `Date.now()` returns NaN in no specified circumstance, but it is a
        // `f64` crossing a language boundary and a NaN here would propagate
        // into a matrix. Cheaper to reject it than to find it later.
        if millis.is_finite() {
            Some(millis / 1_000.0)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds since the Unix epoch for a UTC calendar instant, computed
    /// here rather than taken from a date library — this crate links none,
    /// and a test helper that needed one would be a reason to add one.
    ///
    /// Proleptic Gregorian, via the standard days-from-civil algorithm
    /// (Howard Hinnant's `days_from_civil`, public domain, and the same
    /// arithmetic every `chrono` and `time` uses underneath). Checked
    /// against three instants whose Unix values are independently known in
    /// `the_test_helper_agrees_with_independently_known_unix_timestamps`.
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> f64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3_600 + minute * 60 + second) as f64
    }

    #[test]
    fn the_test_helper_agrees_with_independently_known_unix_timestamps() {
        // Three Unix timestamps that are known independently of this file:
        // the epoch itself; the J2000.0 instant, whose Unix value
        // 946,728,000 is 10,957.5 days x 86,400 by the offset this module
        // declares; and 2001-09-09T01:46:40Z, the famous 1,000,000,000.
        assert_eq!(utc(1970, 1, 1, 0, 0, 0), 0.0);
        assert_eq!(utc(2000, 1, 1, 12, 0, 0), 946_728_000.0);
        assert_eq!(utc(2001, 9, 9, 1, 46, 40), 1_000_000_000.0);
        // A leap-year boundary in each direction, since the helper's era
        // arithmetic is the part most likely to be wrong by a day.
        assert_eq!(utc(2000, 3, 1, 0, 0, 0) - utc(2000, 2, 29, 0, 0, 0), 86_400.0);
        assert_eq!(utc(1900, 3, 1, 0, 0, 0) - utc(1900, 2, 28, 0, 0, 0), 86_400.0);
    }

    #[test]
    fn the_epoch_offset_puts_n_at_zero_on_the_j2000_instant() {
        let n = utc(2000, 1, 1, 12, 0, 0) / 86_400.0 - J2000_UNIX_DAYS;
        assert_eq!(n, 0.0, "J2000.0 is 2000-01-01 12:00, and n must be exactly 0 there");
    }

    /// **The two solstices and the two equinoxes, against declinations
    /// nobody here derived.**
    ///
    /// The instants are the published ones for 2024 and 2026 (UTC, to the
    /// minute) and the expected declinations are the definitions rather than
    /// this file's output: at a solstice the sun's declination is the
    /// obliquity of the ecliptic, 23.44 degrees (IAU 2006 gives 23.4393
    /// degrees at J2000, decreasing by 0.013 degrees per century, so
    /// 23.436 degrees in the 2020s); at an equinox it crosses zero, by
    /// definition of the word.
    ///
    /// **Tolerance 0.02 degrees**, stated and argued: the formula's own
    /// claimed error is 0.01 degrees, the published instants are rounded to
    /// the minute, and declination moves fastest at the equinoxes at 0.4
    /// degrees per day — so a minute of rounding is 0.0003 degrees there and
    /// nothing at all at a solstice, where declination is at an extremum.
    /// 0.02 degrees is twice the claimed error and 60x the rounding.
    ///
    /// A solstice is a weak test on its own: declination is stationary
    /// there, so a formula wrong by a week in phase would still pass it.
    /// The equinoxes are the strong half — declination is at its steepest,
    /// so an error of one day in phase shows up as 0.4 degrees, twenty
    /// times the tolerance.
    #[test]
    fn the_solstices_and_equinoxes_have_the_declinations_they_are_defined_by() {
        const TOLERANCE: f64 = 0.02;
        let cases = [
            // (label, instant, expected declination)
            ("March equinox 2024", utc(2024, 3, 20, 3, 6, 0), 0.0),
            ("June solstice 2024", utc(2024, 6, 20, 20, 51, 0), 23.44),
            ("September equinox 2024", utc(2024, 9, 22, 12, 44, 0), 0.0),
            ("December solstice 2024", utc(2024, 12, 21, 9, 21, 0), -23.44),
            ("March equinox 2026", utc(2026, 3, 20, 14, 46, 0), 0.0),
            ("June solstice 2026", utc(2026, 6, 21, 8, 25, 0), 23.44),
        ];
        for (label, instant, expected) in cases {
            let declination = declination_deg(instant);
            assert!(
                (declination - expected).abs() < TOLERANCE,
                "{label}: declination came out {declination} degrees, not {expected} \
                 within {TOLERANCE}"
            );
            // ...and the subsolar LATITUDE is that declination, which is the
            // claim `subsolar_point` makes and the one a caller depends on.
            assert_eq!(subsolar_point(instant).1, declination);
        }

        // The extremes are extremes: no instant anywhere in the validity
        // window may put the sun outside the obliquity, which is a property
        // of the whole series rather than of six samples. Sampled every 6
        // hours over 1950-2050 -- 146,100 evaluations, which is 4 ms.
        let (start, end) = (utc(1950, 1, 1, 0, 0, 0), utc(2050, 1, 1, 0, 0, 0));
        let mut worst: f64 = 0.0;
        let mut t = start;
        while t < end {
            worst = worst.max(declination_deg(t).abs());
            t += 6.0 * 3_600.0;
        }
        // Measured: **23.44624 degrees**, and where it comes from is worth
        // writing down because it is NOT 23.439. `asin(sin eps sin lambda)`
        // can never exceed `eps`, so the largest declination in the window
        // is the largest `eps` in it -- and `eps = 23.439 - 0.0000004 n`
        // grows as `n` goes negative: at 1950-01-01, `n = -18262.5`, so
        // `eps = 23.44631`. The measured 23.44624 is a solstice in 1950
        // sampled 6 hours off its exact instant. Nothing here is evidence
        // about the obliquity's true value; it is the series' own linear
        // term, run 50 years backwards.
        assert!(
            (23.40..=23.50).contains(&worst),
            "the largest declination over 1950-2050 came out {worst} degrees, where the \
             obliquity of the ecliptic is 23.44"
        );
    }

    /// **The equation of time's published extremes, on their published
    /// dates, with the published SIGN.**
    ///
    /// The independent facts: the equation of time peaks at about +16.4
    /// minutes in the first days of November and bottoms at about −14.2
    /// minutes around 11 February. Cross-checked against NOAA's solar
    /// calculator, which for latitude 0, longitude 0 in 2024 gives apparent
    /// noon at **11:43:31 UTC on 3 November** (so `+16 min 29 s`) and
    /// **12:14:14 UTC on 11 February** (so `−14 min 14 s`). Neither number
    /// came out of this file.
    ///
    /// The SIGN is the half that a plausible-looking implementation gets
    /// wrong, and it is what decides which way round the terminator sits:
    /// get it backwards and the subsolar point is 8 degrees out at the worst
    /// time of year, with nothing about the picture to say so.
    #[test]
    fn the_equation_of_time_has_its_published_extremes_on_its_published_dates() {
        // Degrees -> minutes of time: 15 degrees per hour, so 4 minutes per
        // degree.
        let minutes = |t: f64| equation_of_time_deg(t) * 4.0;

        let november = minutes(utc(2024, 11, 3, 11, 43, 31));
        assert!(
            (november - 16.483).abs() < 0.1,
            "at NOAA's apparent noon of 11:43:31 UTC on 2024-11-03 the equation of time \
             is +16.483 minutes; this gave {november}"
        );
        let february = minutes(utc(2024, 2, 11, 12, 14, 14));
        assert!(
            (february + 14.233).abs() < 0.1,
            "at NOAA's apparent noon of 12:14:14 UTC on 2024-02-11 the equation of time \
             is -14.233 minutes; this gave {february}"
        );

        // The extremes are where they are said to be, found by search
        // rather than asserted at one instant -- a formula shifted by a
        // fortnight would pass the two samples above and fail this.
        let (mut best_max, mut best_max_day) = (f64::NEG_INFINITY, 0i64);
        let (mut best_min, mut best_min_day) = (f64::INFINITY, 0i64);
        for day in 0..366 {
            let t = utc(2024, 1, 1, 12, 0, 0) + (day as f64) * 86_400.0;
            let e = minutes(t);
            if e > best_max {
                (best_max, best_max_day) = (e, day);
            }
            if e < best_min {
                (best_min, best_min_day) = (e, day);
            }
        }
        // 2024 is a leap year, so day 0 is 1 January, day 41 is 11
        // February and day 307 is 3 November.
        //
        // The maximum lands on day **306**, 2 November, one day before the
        // published date -- and that is the sampling's resolution rather
        // than a phase error: the equation of time is stationary at its
        // extremum, so noon on 2 and 3 November differ by 0.003 minutes,
        // twenty times finer than the 0.1-minute tolerance below. The range
        // is written wide enough to say so.
        assert!(
            (305..=309).contains(&best_max_day),
            "the yearly maximum landed on day {best_max_day} of 2024, which is more than \
             two days from the 3 November (day 307) the published analemma puts it on"
        );
        assert!(
            (39..=43).contains(&best_min_day),
            "the yearly minimum landed on day {best_min_day} of 2024, not the 11 February \
             (day 41) the published analemma puts it on"
        );
        assert!(
            (16.3..16.6).contains(&best_max),
            "the yearly maximum came out {best_max} minutes, not the published +16.4"
        );
        assert!(
            (-14.4..-14.0).contains(&best_min),
            "the yearly minimum came out {best_min} minutes, not the published -14.2"
        );
    }

    /// **The subsolar point walks west at 15 degrees an hour, and stands on
    /// Greenwich at apparent noon.**
    ///
    /// Both halves are geometry, not astronomy: the planet turns once a day,
    /// and "apparent noon at Greenwich" is the definition of the sun being
    /// on the Greenwich meridian. The second is what ties the longitude
    /// half of [`subsolar_point`] to the equation of time with no shared
    /// arithmetic — NOAA's apparent-noon times are the input and zero is
    /// the expected output.
    #[test]
    fn the_subsolar_point_is_on_greenwich_at_apparent_noon_and_walks_west() {
        // NOAA solar calculator, latitude 0 longitude 0, 2024. At each of
        // these instants the sun is ON the Greenwich meridian.
        let noons = [
            ("2024-11-03", utc(2024, 11, 3, 11, 43, 31)),
            ("2024-02-11", utc(2024, 2, 11, 12, 14, 14)),
            ("2024-03-20", utc(2024, 3, 20, 12, 7, 26)),
            ("2024-09-22", utc(2024, 9, 22, 11, 52, 45)),
        ];
        let mut worst = 0.0f64;
        for (label, t) in noons {
            let (lon, _) = subsolar_point(t);
            worst = worst.max(lon.abs());
            assert!(
                lon.abs() < 0.06,
                "{label}: NOAA puts apparent noon at this instant, so the subsolar \
                 longitude must be 0; it came out {lon}"
            );
        }
        // Recorded rather than left implicit: the worst disagreement with
        // NOAA's table over the four dates. It is 0.0535 degrees, which is
        // 0.33 px on a 700 px globe -- and it is NOT this formula's error.
        // The higher-order series in
        // `the_low_precision_formulae_agree_with_a_higher_order_series`
        // disagrees with NOAA's table by the same amount at the same
        // instants, so what is being measured here is NOAA's own
        // whole-second solar-noon iteration.
        assert!(
            (0.04..0.06).contains(&worst),
            "the worst disagreement with NOAA's apparent-noon table came out {worst} \
             degrees, where 0.0535 was measured -- if this moved, one of the two \
             changed"
        );

        // 15 degrees an hour, westward, over a whole day. The equation of
        // time drifts by at most 0.33 degrees a day (its steepest slope is
        // 21 s of time per day), so the hour-to-hour step is 15 degrees to
        // within 0.02.
        let t0 = utc(2026, 9, 18, 0, 0, 0);
        for hour in 0..24 {
            let a = subsolar_point(t0 + (hour as f64) * 3_600.0).0;
            let b = subsolar_point(t0 + ((hour + 1) as f64) * 3_600.0).0;
            let step = wrap_180(b - a);
            assert!(
                (step + 15.0).abs() < 0.02,
                "hour {hour}: the subsolar point moved {step} degrees, not -15"
            );
        }
        // And six hours is a quarter turn, which is the figure this plan's
        // acceptance criterion names.
        let quarter = wrap_180(subsolar_point(t0 + 6.0 * 3_600.0).0 - subsolar_point(t0).0);
        assert!(
            (quarter + 90.0).abs() < 0.1,
            "six hours must move the subsolar point 90 degrees west; it moved {quarter}"
        );
    }

    /// **The low-precision series against a higher-order one**, because
    /// "accurate to 0.01 degrees" is a claim about the source and not a
    /// measurement of this transcription.
    ///
    /// The reference arm is Meeus, *Astronomical Algorithms*, chapter 25
    /// ("Solar Coordinates") with the nutation and aberration terms of
    /// chapter 28's equation of time — a **different series with different
    /// coefficients**, quadratic in the Julian century where the Almanac's
    /// is linear in the day, and carrying a third-order equation of centre,
    /// the longitude of the ascending node, and the nutation in obliquity.
    /// It is written out inside the test rather than in the module, because
    /// its only job is to disagree.
    ///
    /// A transcription error in any one of the six published coefficients
    /// above fails this by orders of magnitude; the two series agreeing to
    /// arcseconds over a century is not something two independently wrong
    /// transcriptions do.
    #[test]
    fn the_low_precision_formulae_agree_with_a_higher_order_series() {
        /// Meeus ch. 25 + ch. 28: `(declination_deg, equation_of_time_deg)`.
        fn meeus(unix_seconds: f64) -> (f64, f64) {
            let jd = unix_seconds / 86_400.0 + 2_440_587.5;
            let t = (jd - 2_451_545.0) / 36_525.0;
            let l0 = (280.466_46 + 36_000.769_83 * t + 0.000_303_2 * t * t).rem_euclid(360.0);
            let m = (357.529_11 + 35_999.050_29 * t - 0.000_153_7 * t * t).rem_euclid(360.0);
            let mr = m.to_radians();
            let centre = (1.914_602 - 0.004_817 * t - 0.000_014 * t * t) * mr.sin()
                + (0.019_993 - 0.000_101 * t) * (2.0 * mr).sin()
                + 0.000_289 * (3.0 * mr).sin();
            let omega = (125.04 - 1_934.136 * t).to_radians();
            // Apparent longitude: the true longitude less aberration
            // (-0.00569) and the nutation in longitude (-0.00478 sin omega).
            let lambda = (l0 + centre - 0.005_69 - 0.004_78 * omega.sin()).to_radians();
            let e0 = 23.0
                + 26.0 / 60.0
                + 21.448 / 3_600.0
                - (46.815_0 * t + 0.000_59 * t * t - 0.001_813 * t * t * t) / 3_600.0;
            let eps = (e0 + 0.002_56 * omega.cos()).to_radians();
            let declination = (eps.sin() * lambda.sin()).clamp(-1.0, 1.0).asin().to_degrees();
            let alpha = (eps.cos() * lambda.sin()).atan2(lambda.cos()).to_degrees();
            // Meeus 28.1: E = L0 - 0.0057183 - alpha + delta_psi cos eps.
            let eot = (l0 - 0.005_718_3 - alpha - 0.004_78 * omega.sin() * eps.cos() + 180.0)
                .rem_euclid(360.0)
                - 180.0;
            (declination, eot)
        }

        let (start, end) = (utc(1950, 1, 1, 0, 0, 0), utc(2050, 1, 1, 0, 0, 0));
        let (mut worst_dec, mut worst_eot) = (0.0f64, 0.0f64);
        let mut t = start;
        while t < end {
            let (mine_dec, mine_eot) = (declination_deg(t), equation_of_time_deg(t));
            let (ref_dec, ref_eot) = meeus(t);
            worst_dec = worst_dec.max((mine_dec - ref_dec).abs());
            worst_eot = worst_eot.max((mine_eot - ref_eot).abs());
            t += 6.0 * 3_600.0;
        }
        // Measured, on the 146,100 samples above: **0.003619 degrees** of
        // declination and **0.003120 degrees** of equation of time, i.e.
        // 13.0 and 11.2 arcseconds -- both a third of the 0.01 degrees the
        // Almanac claims, over the whole window it claims it for. 0.0031
        // degrees of longitude is 0.02 px on a 700 px globe.
        assert!(
            worst_dec < 0.01,
            "declination disagreed with Meeus by {worst_dec} degrees over 1950-2050, \
             against the 0.01 the source claims"
        );
        assert!(
            worst_eot < 0.01,
            "the equation of time disagreed with Meeus by {worst_eot} degrees over \
             1950-2050"
        );
        assert!(
            worst_dec > 1e-4 && worst_eot > 1e-4,
            "the two series agreed to {worst_dec} / {worst_eot} degrees, which is closer \
             than two genuinely different series get -- the reference arm is probably \
             evaluating this module's own code"
        );
    }

    /// The longitude is a longitude: reduced, east-positive and continuous
    /// across the antimeridian, over a year of samples. A `%` where a
    /// `rem_euclid` belonged is the defect this catches, and it only shows
    /// on one side of the wrap.
    #[test]
    fn the_subsolar_longitude_stays_a_reduced_east_positive_longitude() {
        let mut t = utc(2026, 1, 1, 0, 0, 0);
        let end = utc(2027, 1, 1, 0, 0, 0);
        while t < end {
            let (lon, lat) = subsolar_point(t);
            assert!((-180.0..=180.0).contains(&lon), "longitude {lon} at unix {t}");
            assert!((-23.5..=23.5).contains(&lat), "latitude {lat} at unix {t}");
            t += 907.0; // A prime-ish stride, so samples do not land on the hour.
        }
        // Dates before the epoch, where `%` and `rem_euclid` differ: 1960
        // is inside the source's own validity window and a negative
        // `unix_seconds` there must still give an hour of the day in
        // [0, 24), not a negative one.
        let before = utc(1960, 7, 4, 18, 0, 0);
        assert!(before < 0.0, "the pre-epoch case must actually be pre-epoch");
        let (lon, lat) = subsolar_point(before);
        assert!((-180.0..=180.0).contains(&lon), "pre-epoch longitude {lon}");
        assert!((15.0..25.0).contains(&lat), "4 July is northern summer; got latitude {lat}");
        // 18:00 UTC puts the sun six hours west of Greenwich, near 90 W.
        assert!((lon + 90.0).abs() < 2.0, "at 18:00 UTC the subsolar point is near 90 W, not {lon}");
    }

    /// The source claims 1950-2050 and the series is evaluated over exactly
    /// that, so the ends are sampled rather than assumed: no NaN, no
    /// unwrapped angle, and a declination still inside the obliquity.
    #[test]
    fn the_formulae_span_the_sources_own_1950_to_2050_validity() {
        for (label, t) in [
            ("1950 start", utc(1950, 1, 1, 0, 0, 0)),
            ("2050 end", utc(2050, 1, 1, 0, 0, 0)),
        ] {
            let (lon, lat) = subsolar_point(t);
            assert!(lon.is_finite() && lat.is_finite(), "{label} gave {lon}, {lat}");
            assert!((-180.0..=180.0).contains(&lon), "{label} longitude {lon}");
            assert!((-23.5..=23.5).contains(&lat), "{label} latitude {lat}");
            // 1 January is close to the December solstice, so the sun is
            // well south in both cases -- a check with a sign in it.
            assert!(lat < -22.0, "{label}: 1 January is deep northern winter; got {lat}");
        }
    }

    /// The wall clock, such as it can be tested: it answers, it answers
    /// something in this century, and it moves forward.
    ///
    /// Not a test of the wasm arm -- `cargo test` builds the native one. What
    /// covers the wasm arm is `./scripts/check-wasm.sh`.
    #[test]
    fn the_wall_clock_answers_with_a_plausible_instant() {
        let now = now_unix_seconds().expect("a desktop has a wall clock");
        // 2020-01-01 .. 2100-01-01, which is wide enough that a machine with
        // a wrong date still passes and narrow enough to catch a unit error:
        // milliseconds instead of seconds would read as the year 57,000.
        assert!(
            (1_577_836_800.0..4_102_444_800.0).contains(&now),
            "the clock said {now} seconds since the epoch, which is not this century -- \
             a milliseconds-for-seconds mix-up is the usual cause"
        );
        // And the subsolar point of NOW is a subsolar point.
        let (lon, lat) = subsolar_point(now);
        assert!((-180.0..=180.0).contains(&lon) && (-23.5..=23.5).contains(&lat));
    }
}

