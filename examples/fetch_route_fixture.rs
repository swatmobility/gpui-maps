//! Captures one SWAT OSRM response as a test fixture, with the key stripped.
//!
//! ```text
//! cargo run --example fetch_route_fixture -- \
//!     --from 103.8559,1.3006 --to 103.8607,1.2834 \
//!     --out tests/fixtures/routes/singapore-bugis-marina-bay.json \
//!     [--via LON,LAT]... [--require roundabout,uturn]
//! ```
//!
//! The key comes from `SWAT_ROUTING_KEY` or `~/.config/gpui-map/swat-routing.env`
//! (`nav::config::RoutingConfig::resolve`). It is never printed: the one line
//! that shows the URL shows it through `redact_access_token`, and the bytes
//! written pass `route_source::strip_credentials`, which refuses to write if
//! the key survives anywhere in them.
//!
//! This is the ONLY code in the repository that calls SWAT. Every test reads
//! what this saved, offline.

use gpui_maps::nav::config::{RoutingConfig, CONFIG_FILE_RELATIVE};
use gpui_maps::nav::route_source::{describe_fetch_error, parse_route, request_url, strip_credentials, UrlOptions};

fn point(text: &str) -> anyhow::Result<(f64, f64)> {
    let (lon, lat) = text.split_once(',').ok_or_else(|| anyhow::anyhow!("{text:?} is not LON,LAT"))?;
    Ok((lon.trim().parse()?, lat.trim().parse()?))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let from = point(&value("--from").ok_or_else(|| anyhow::anyhow!("--from LON,LAT is required"))?)?;
    let to = point(&value("--to").ok_or_else(|| anyhow::anyhow!("--to LON,LAT is required"))?)?;
    let out = value("--out").ok_or_else(|| anyhow::anyhow!("--out PATH is required"))?;
    let vias = args.windows(2).filter(|w| w[0] == "--via").map(|w| point(&w[1])).collect::<anyhow::Result<Vec<_>>>()?;
    let require: Vec<String> =
        value("--require").map(|r| r.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default();

    let file = std::env::var("HOME")
        .ok()
        .and_then(|home| std::fs::read_to_string(std::path::Path::new(&home).join(CONFIG_FILE_RELATIVE)).ok());
    let config = RoutingConfig::resolve(&|name| std::env::var(name).ok(), file.as_deref())?;
    let key = config.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();

    let mut waypoints = vec![from];
    waypoints.extend(vias);
    waypoints.push(to);
    let url = request_url(&config, &waypoints, &UrlOptions::navigation());
    eprintln!("GET {}", describe_fetch_error(&url));
    let response = reqwest::get(&url).await.map_err(|e| anyhow::anyhow!("{}", describe_fetch_error(&format!("{e:#}"))))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|e| anyhow::anyhow!("{}", describe_fetch_error(&format!("{e:#}"))))?;
    let clean = strip_credentials(&body, &key).map_err(|e| anyhow::anyhow!("HTTP {status}: {e}"))?;
    let route = parse_route(&clean).map_err(|e| anyhow::anyhow!("HTTP {status}: {e}"))?;

    for step in &route.steps {
        eprintln!(
            "  {:<16} {:<13} exit={:<4} {:<28} {:>6.0} m  {}",
            step.maneuver.kind,
            step.maneuver.modifier.as_deref().unwrap_or("-"),
            step.maneuver.exit.map_or("-".to_string(), |e| e.to_string()),
            step.name.as_deref().unwrap_or(""),
            step.distance,
            step.driving_side.as_deref().unwrap_or(""),
        );
    }
    let has = |want: &str| match want {
        "roundabout" => route.steps.iter().any(|s| matches!(s.maneuver.kind.as_str(), "roundabout" | "rotary")),
        "uturn" => route.steps.iter().any(|s| s.maneuver.modifier.as_deref() == Some("uturn")),
        other => route.steps.iter().any(|s| s.maneuver.kind == other),
    };
    let missing: Vec<&String> = require.iter().filter(|want| !has(want)).collect();
    if !missing.is_empty() {
        anyhow::bail!("the route has no {missing:?}, so it was not saved; try other points");
    }
    std::fs::write(&out, &clean)?;
    eprintln!("saved {out}: {:.0} m, {} steps", route.route.distance, route.steps.len());
    Ok(())
}
