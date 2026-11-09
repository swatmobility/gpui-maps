//! Where the SWAT routing proxy is and what authorises it: `SWAT_ROUTING_URL`,
//! `SWAT_ROUTING_ROAD_NETWORK` and `SWAT_ROUTING_KEY`, from the environment
//! first and `~/.config/gpui-map/swat-routing.env` second.
//!
//! No I/O here. The caller reads the environment and the file and hands both
//! in, which is what lets the browser and the phone compile this file and
//! what lets the tests below pin the precedence without touching `HOME`.
//!
//! The key is a credential. [`RoutingKey`] never prints its value, not even
//! through `{:?}`; the only way to read it is [`RoutingKey::expose`], and the
//! only caller of that is `route_source::request_url`.

use std::fmt;

/// The OSRM profile when nothing names one -- the reference client's
/// `DEFAULT_ROAD_NETWORK`, and what the SWAT vans route on.
pub const DEFAULT_ROAD_NETWORK: &str = "van";
/// The config file, relative to `$HOME`.
pub const CONFIG_FILE_RELATIVE: &str = ".config/gpui-map/swat-routing.env";

pub const URL_VAR: &str = "SWAT_ROUTING_URL";
pub const ROAD_NETWORK_VAR: &str = "SWAT_ROUTING_ROAD_NETWORK";
pub const KEY_VAR: &str = "SWAT_ROUTING_KEY";

/// The proxy's `access_token`. Opaque on purpose.
#[derive(Clone, PartialEq, Eq)]
pub struct RoutingKey(String);

impl RoutingKey {
    /// `None` for an absent or blank value: `SWAT_ROUTING_KEY=` exported by a
    /// script is "no key", not a key that makes every request 401.
    #[must_use]
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_string()))
    }

    /// The value, for the one place that has to put it on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RoutingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutingKey([REDACTED])")
    }
}

/// Everything a route request needs apart from the waypoints.
#[derive(Clone, PartialEq, Eq)]
pub struct RoutingConfig {
    /// No trailing slash.
    pub url: String,
    pub road_network: String,
    /// `None` only for a proxy that adds the key itself (the browser build's
    /// `web/serve.py`), never for a direct request to SWAT.
    pub key: Option<RoutingKey>,
}

/// Redacts `url`: a misconfigured `SWAT_ROUTING_URL` that already carries
/// `?access_token=...` should not be able to leak it just by logging the
/// config with `{:?}`.
impl fmt::Debug for RoutingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoutingConfig")
            .field("url", &crate::sprite::redact_access_token(&self.url))
            .field("road_network", &self.road_network)
            .field("key", &self.key)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    MissingUrl,
    MissingKey,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (var, what) = match self {
            Self::MissingUrl => (URL_VAR, "the routing proxy's address"),
            Self::MissingKey => (KEY_VAR, "the routing proxy's key"),
        };
        write!(
            f,
            "live routing is off: {var} ({what}) is not set in the environment or in ~/{CONFIG_FILE_RELATIVE}; \
             simulated drives still work"
        )
    }
}

impl std::error::Error for ConfigError {}

/// `KEY=VALUE` lines. `#` starts a comment line, a leading `export ` is
/// dropped, and one pair of matching quotes around the value is removed --
/// the three things a shell-style env file actually contains.
#[must_use]
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (name, value) = line.split_once('=')?;
            let value = value.trim();
            let value = [('"', '"'), ('\'', '\'')]
                .iter()
                .find_map(|(open, close)| value.strip_prefix(*open).and_then(|v| v.strip_suffix(*close)))
                .unwrap_or(value);
            Some((name.trim().to_string(), value.to_string()))
        })
        .collect()
}

impl RoutingConfig {
    /// The environment wins over the file, per variable; a blank value in
    /// either counts as absent. The key is required: this is the config for
    /// a request that goes straight to SWAT.
    pub fn resolve(env: &dyn Fn(&str) -> Option<String>, file: Option<&str>) -> Result<Self, ConfigError> {
        let from_file = file.map(parse_env_file).unwrap_or_default();
        let lookup = |name: &str| -> Option<String> {
            env(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    from_file
                        .iter()
                        .rev()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                })
        };
        let url = lookup(URL_VAR).ok_or(ConfigError::MissingUrl)?;
        let key = lookup(KEY_VAR).and_then(|k| RoutingKey::new(&k)).ok_or(ConfigError::MissingKey)?;
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            road_network: lookup(ROAD_NETWORK_VAR).unwrap_or_else(|| DEFAULT_ROAD_NETWORK.to_string()),
            key: Some(key),
        })
    }

    /// A proxy that holds the key itself (`web/serve.py`'s `/swat-route`).
    #[must_use]
    pub fn keyless(url: &str, road_network: Option<&str>) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            road_network: road_network.filter(|r| !r.trim().is_empty()).unwrap_or(DEFAULT_ROAD_NETWORK).to_string(),
            key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn the_environment_wins_over_the_file_variable_by_variable() {
        let file = "SWAT_ROUTING_URL=https://file.example.com/\nSWAT_ROUTING_KEY=file-key\nSWAT_ROUTING_ROAD_NETWORK=truck\n";
        let env = env_of(&[("SWAT_ROUTING_KEY", "env-key")]);
        let config = RoutingConfig::resolve(&env, Some(file)).unwrap();
        assert_eq!(config.url, "https://file.example.com", "trailing slash dropped");
        assert_eq!(config.road_network, "truck");
        assert_eq!(config.key.as_ref().map(RoutingKey::expose), Some("env-key"));
    }

    #[test]
    fn the_road_network_defaults_to_van_and_a_blank_env_value_is_absent() {
        let env = env_of(&[("SWAT_ROUTING_URL", "https://r.example.com"), ("SWAT_ROUTING_KEY", "k"), ("SWAT_ROUTING_ROAD_NETWORK", "  ")]);
        let config = RoutingConfig::resolve(&env, None).unwrap();
        assert_eq!(config.road_network, "van");
    }

    #[test]
    fn the_env_file_takes_comments_export_and_quotes() {
        let parsed = parse_env_file("# a comment\n\nexport SWAT_ROUTING_URL=\"https://q.example.com\"\nSWAT_ROUTING_KEY='k=1'\n");
        assert_eq!(
            parsed,
            vec![
                ("SWAT_ROUTING_URL".to_string(), "https://q.example.com".to_string()),
                ("SWAT_ROUTING_KEY".to_string(), "k=1".to_string()),
            ]
        );
    }

    #[test]
    fn a_missing_key_is_an_error_that_names_the_variable_and_no_value() {
        let env = env_of(&[("SWAT_ROUTING_URL", "https://r.example.com")]);
        let err = RoutingConfig::resolve(&env, None).unwrap_err();
        assert_eq!(err, ConfigError::MissingKey);
        assert!(err.to_string().contains("SWAT_ROUTING_KEY"));
        assert_eq!(RoutingConfig::resolve(&env_of(&[]), None).unwrap_err(), ConfigError::MissingUrl);
    }

    #[test]
    fn neither_the_key_nor_the_config_prints_the_key() {
        let env = env_of(&[("SWAT_ROUTING_URL", "https://r.example.com"), ("SWAT_ROUTING_KEY", "sk-live-DO-NOT-PRINT")]);
        let config = RoutingConfig::resolve(&env, None).unwrap();
        let shown = format!("{config:?} {:?}", config.key);
        assert!(!shown.contains("DO-NOT-PRINT"), "{shown}");
        assert!(shown.contains("[REDACTED]"));
    }

    #[test]
    fn a_keyless_proxy_config_has_no_key() {
        let config = RoutingConfig::keyless("/swat-route/", None);
        assert_eq!((config.url.as_str(), config.road_network.as_str(), config.key.is_none()), ("/swat-route", "van", true));
    }
}
