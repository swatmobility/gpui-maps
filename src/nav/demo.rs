//! How each shell is asked for a simulated drive -- native launch flags, a
//! browser query string, iOS launch arguments or environment -- read into one
//! [`DemoRequest`] by one parser, so the three cannot disagree about what
//! `speed=4` means.

use crate::nav::simulate::SimOptions;

/// The committed fixtures every shell can drive (`tests/fixtures/routes/`).
pub const BUILTIN_ROUTES: [&str; 3] = ["manila", "singapore-bugis-marina-bay", "singapore-seletar-rotary-uturn"];
/// What a demo drives when nothing names a route.
pub const DEFAULT_ROUTE: &str = "manila";

#[derive(Debug, Clone, PartialEq)]
pub struct DemoRequest {
    /// A fixture name (browser, iOS) or a file path (native).
    pub route: String,
    /// Start driving at launch.
    pub autostart: bool,
    /// Offer the engine-drawn "Simulate" button instead (iOS).
    pub show_button: bool,
    pub options: SimOptions,
}

/// A name the browser may put in a fetch path: lowercase letters, digits and
/// dashes, 1 to 64 of them. Nothing that could climb out of `routes/`.
#[must_use]
pub fn valid_route_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The parameter names, per shell.
struct Keys {
    mode: &'static str,
    route: &'static str,
    speed: &'static str,
    at: &'static str,
    paused: &'static str,
    off_route: &'static str,
}

const QUERY: Keys = Keys { mode: "nav", route: "route", speed: "speed", at: "nav_at", paused: "nav_paused", off_route: "offroute" };
const ENV: Keys = Keys {
    mode: "GPUI_MAP_NAV",
    route: "GPUI_MAP_NAV_ROUTE",
    speed: "GPUI_MAP_NAV_SPEED",
    at: "GPUI_MAP_NAV_AT",
    paused: "GPUI_MAP_NAV_PAUSED",
    off_route: "GPUI_MAP_NAV_OFFROUTE",
};

fn from_lookup(get: &dyn Fn(&str) -> Option<String>, keys: &Keys) -> Option<DemoRequest> {
    let (autostart, show_button) = match get(keys.mode)?.trim() {
        "sim" => (true, false),
        "button" => (false, true),
        _ => return None,
    };
    let number = |key: &str| get(key).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
    let flag = |key: &str| get(key).is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    Some(DemoRequest {
        route: get(keys.route).map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| DEFAULT_ROUTE.to_string()),
        autostart,
        show_button,
        options: SimOptions {
            speed: number(keys.speed).map_or(1, |s| s.clamp(1.0, 8.0) as u8),
            paused: flag(keys.paused),
            off_route_after_step: number(keys.off_route).filter(|s| *s >= 0.0).map(|s| s as usize),
            start_at_m: number(keys.at).filter(|m| *m > 0.0),
        },
    })
}

impl DemoRequest {
    /// The browser: `?nav=sim&route=manila&speed=4&nav_at=1200&nav_paused=1&offroute=5`.
    #[must_use]
    pub fn from_query(pairs: &[(String, String)]) -> Option<Self> {
        from_lookup(&|key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()), &QUERY)
    }

    /// The environment: `GPUI_MAP_NAV=sim|button` and the `GPUI_MAP_NAV_*`
    /// siblings (iOS `SIMCTL_CHILD_`/`devicectl --environment-variables`).
    #[must_use]
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        from_lookup(lookup, &ENV)
    }

    /// Launch arguments: `--nav-sim` or `--nav-button`, then
    /// `--nav-route X`, `--nav-speed N`, `--nav-at M`, `--nav-paused`,
    /// `--nav-offroute K`.
    ///
    /// A bare `--nav-route X` (neither mode flag) is a request too: load X
    /// for `G`, start nothing, offer no button. Without it the flag was
    /// silently ignored and `G` drove [`DEFAULT_ROUTE`] instead.
    #[must_use]
    pub fn from_args(args: &[String]) -> Option<Self> {
        let has = |flag: &str| args.iter().any(|a| a == flag);
        let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
        let (mode, load_only) = if has("--nav-sim") {
            ("sim", false)
        } else if has("--nav-button") {
            ("button", false)
        } else if has("--nav-route") {
            ("button", true)
        } else {
            return None;
        };
        let request = from_lookup(
            &|key| match key {
                "nav" => Some(mode.to_string()),
                "route" => value("--nav-route"),
                "speed" => value("--nav-speed"),
                "nav_at" => value("--nav-at"),
                "nav_paused" => has("--nav-paused").then(|| "1".to_string()),
                "offroute" => value("--nav-offroute"),
                _ => None,
            },
            &QUERY,
        )?;
        Some(if load_only { Self { show_button: false, ..request } } else { request })
    }
}

/// A press released within 4 logical pixels and half a second of where and
/// when it went down: a click, not a drag. Right-click-to-route shares the
/// right button with orbiting, so this is what tells the two apart.
#[must_use]
pub fn is_destination_click(down: (f32, f32), up: (f32, f32), held_ms: f64) -> bool {
    (up.0 - down.0).hypot(up.1 - down.1) <= 4.0 && (0.0..=500.0).contains(&held_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(query: &[(&str, &str)]) -> Vec<(String, String)> {
        query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_query_string_asks_for_a_drive() {
        let request = DemoRequest::from_query(&pairs(&[("nav", "sim"), ("route", "singapore-bugis-marina-bay"), ("speed", "4"), ("nav_at", "1200"), ("nav_paused", "1")])).unwrap();
        assert_eq!(request.route, "singapore-bugis-marina-bay");
        assert!(request.autostart && !request.show_button);
        assert_eq!(request.options, SimOptions { speed: 4, paused: true, off_route_after_step: None, start_at_m: Some(1200.0) });
        assert_eq!(DemoRequest::from_query(&pairs(&[("nav", "sim")])).unwrap().route, "manila");
        assert!(DemoRequest::from_query(&pairs(&[("route", "manila")])).is_none(), "no nav=, no drive");
        assert_eq!(DemoRequest::from_query(&pairs(&[("nav", "sim"), ("speed", "99")])).unwrap().options.speed, 8);
    }

    #[test]
    fn args_and_env_read_the_same_way() {
        let args: Vec<String> = ["app", "--nav-sim", "--nav-speed", "2", "--nav-offroute", "5"].iter().map(|s| s.to_string()).collect();
        let from_args = DemoRequest::from_args(&args).unwrap();
        let from_env = DemoRequest::from_env(&|k| match k {
            "GPUI_MAP_NAV" => Some("sim".into()),
            "GPUI_MAP_NAV_SPEED" => Some("2".into()),
            "GPUI_MAP_NAV_OFFROUTE" => Some("5".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(from_args, from_env);
        assert_eq!(from_args.options.off_route_after_step, Some(5));
        let button = DemoRequest::from_args(&["app".to_string(), "--nav-button".to_string()]).unwrap();
        assert!(button.show_button && !button.autostart);
    }

    #[test]
    fn a_bare_route_flag_loads_that_route_and_starts_nothing() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let bare = DemoRequest::from_args(&args(&["app", "--nav-route", "singapore-bugis-marina-bay", "--nav-speed", "2"])).unwrap();
        assert_eq!(bare.route, "singapore-bugis-marina-bay", "not the default");
        assert!(!bare.autostart && !bare.show_button, "loaded for G, nothing more");
        assert_eq!(bare.options.speed, 2, "the other flags still apply");
        let button = DemoRequest::from_args(&args(&["app", "--nav-button", "--nav-route", "singapore-bugis-marina-bay"])).unwrap();
        assert_eq!(button.route, "singapore-bugis-marina-bay");
        assert!(button.show_button && !button.autostart);
        assert!(DemoRequest::from_args(&args(&["app", "--nav-speed", "2"])).is_none(), "no route and no mode, no request");
    }

    #[test]
    fn only_safe_names_reach_a_fetch_path() {
        for name in BUILTIN_ROUTES {
            assert!(valid_route_name(name));
        }
        for bad in ["", "../secrets", "Manila", "a/b", "x.json", &"a".repeat(65)] {
            assert!(!valid_route_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_click_is_short_and_still() {
        assert!(is_destination_click((10.0, 10.0), (12.0, 13.0), 120.0));
        assert!(!is_destination_click((10.0, 10.0), (20.0, 10.0), 120.0), "a drag");
        assert!(!is_destination_click((10.0, 10.0), (10.0, 10.0), 900.0), "a hold");
    }
}
