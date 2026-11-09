//! Numbers as the chrome shows them: a distance to the next maneuver, a
//! remaining duration, an arrival clock time. Metric and 24-hour, which is
//! what Singapore and Manila read.

/// The shell's wall clock, handed in: `unix_ms` from `Date.now()` or
/// `SystemTime::now()`, and the local UTC offset, because a phone's ETA is
/// read in its own time zone and Rust's std cannot say what that is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WallClock {
    pub unix_ms: f64,
    pub utc_offset_min: i32,
}

/// `15 m`, `170 m`, `1.2 km`, `12 km`: 5 m steps under 50 m, 10 m steps
/// under a kilometre, one decimal under ten.
#[must_use]
pub fn distance_text(metres: f64) -> String {
    if !metres.is_finite() || metres <= 0.0 {
        return "0 m".to_string();
    }
    if metres < 50.0 {
        return format!("{} m", ((metres / 5.0).round() * 5.0) as i64);
    }
    if metres < 1_000.0 {
        let rounded = ((metres / 10.0).round() * 10.0) as i64;
        return if rounded >= 1_000 { "1.0 km".to_string() } else { format!("{rounded} m") };
    }
    if metres < 10_000.0 {
        return format!("{:.1} km", metres / 1_000.0);
    }
    format!("{} km", (metres / 1_000.0).round() as i64)
}

/// The words the chrome writes around its numbers, in the app's language.
/// English unless a shell hands its own in with [`set_chrome_words`]: the
/// engine carries no translations, so a localised app (Studio) fills these
/// from its own string tables whenever its language changes. Templates use
/// `{n}`, `{h}`, `{m}`, `{distance}` and `{clock}` as placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeWords {
    /// `{n} min`: a duration under an hour.
    pub minutes: String,
    /// `{h} h {m} min`: a duration of an hour or more (`{m}` is two digits).
    pub hours_minutes: String,
    /// `{distance} · arrival {clock}`: the strip's second line.
    pub arrival: String,
    /// "Then", on the pill under the banner.
    pub then: String,
    /// "Off route" and "Rerouting…", the alert banner.
    pub off_route: String,
    pub rerouting: String,
    /// "You have arrived" and its "Done" button.
    pub arrived: String,
    pub done: String,
    /// The idle "Simulate" button.
    pub simulate: String,
}

impl Default for ChromeWords {
    fn default() -> Self {
        Self {
            minutes: "{n} min".into(),
            hours_minutes: "{h} h {m} min".into(),
            arrival: "{distance} \u{b7} arrival {clock}".into(),
            then: "Then".into(),
            off_route: "Off route".into(),
            rerouting: "Rerouting\u{2026}".into(),
            arrived: "You have arrived".into(),
            done: "Done".into(),
            simulate: "Simulate".into(),
        }
    }
}

static CHROME_WORDS: std::sync::RwLock<Option<ChromeWords>> = std::sync::RwLock::new(None);

/// Sets the chrome's words process-wide (`None` restores English). The
/// next frame's chrome reads them.
pub fn set_chrome_words(words: Option<ChromeWords>) {
    if let Ok(mut slot) = CHROME_WORDS.write() {
        *slot = words;
    }
}

/// The chrome's words now: the shell's, or English.
#[must_use]
pub fn chrome_words() -> ChromeWords {
    CHROME_WORDS.read().ok().and_then(|w| w.clone()).unwrap_or_default()
}

/// The strip's second line: `2.6 km · arrival 21:58`.
#[must_use]
pub fn arrival_text(distance: &str, clock: &str) -> String {
    chrome_words().arrival.replace("{distance}", distance).replace("{clock}", clock)
}

/// `0 min`, `10 min`, `1 h 05 min`. Rounded UP: a banner that says 9 min
/// when 9 min 26 s remain is promising too much.
#[must_use]
pub fn duration_text(seconds: f64) -> String {
    let minutes = if seconds.is_finite() && seconds > 0.0 { (seconds / 60.0).ceil() as i64 } else { 0 };
    let words = chrome_words();
    if minutes < 60 {
        words.minutes.replace("{n}", &minutes.to_string())
    } else {
        words.hours_minutes.replace("{h}", &(minutes / 60).to_string()).replace("{m}", &format!("{:02}", minutes % 60))
    }
}

/// `21:59`, in the clock's own zone.
#[must_use]
pub fn clock_text(unix_ms: f64, utc_offset_min: i32) -> String {
    let minutes = if unix_ms.is_finite() { (unix_ms / 60_000.0).floor() as i64 } else { 0 };
    let local = (minutes + i64::from(utc_offset_min)).rem_euclid(24 * 60);
    format!("{:02}:{:02}", local / 60, local % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_round_the_way_a_banner_reads() {
        for (metres, text) in [
            (0.0, "0 m"),
            (-3.0, "0 m"),
            (f64::NAN, "0 m"),
            (12.4, "10 m"),
            (13.0, "15 m"),
            (171.7, "170 m"),
            (996.0, "1.0 km"),
            (1_204.0, "1.2 km"),
            (9_940.0, "9.9 km"),
            (12_345.0, "12 km"),
        ] {
            assert_eq!(distance_text(metres), text, "{metres} m");
        }
    }

    #[test]
    fn durations_round_up_and_carry_hours() {
        assert_eq!(duration_text(0.0), "0 min");
        assert_eq!(duration_text(565.7), "10 min");
        assert_eq!(duration_text(3_600.0), "1 h 00 min");
        assert_eq!(duration_text(3_901.0), "1 h 06 min");
    }

    #[test]
    fn a_clock_time_is_local_and_wraps_at_midnight() {
        // 2026-03-17T13:50:00Z is 21:50 in Manila (UTC+8).
        assert_eq!(clock_text(1_773_755_400_000.0, 480), "21:50");
        assert_eq!(clock_text(1_773_755_400_000.0 + 3.0 * 3_600_000.0, 480), "00:50");
        assert_eq!(clock_text(0.0, -60), "23:00");
    }
}
