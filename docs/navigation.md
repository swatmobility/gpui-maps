# Turn-by-turn navigation

A simulated or live driving trip, drawn over the map: the route line, the
puck, the maneuver card and the End/Simulate controls. This document is the
part `examples/mapbox_streets_demo.rs`'s comments, `ios/run-device.sh`'s
comments and `web/serve.py`'s docstring cannot be on their own: one place
that says how to run the demo on each shell, and how to point it at SWAT's
own routing.

## What it is

- **Route following and drive state** come from
  [Ferrostar](https://github.com/stadiamaps/ferrostar)'s Rust core
  (`src/nav/`): where the puck is along the route, which maneuver is next,
  whether the trip has drifted off route.
- **Routes themselves** come from a SWAT OSRM-compatible proxy (`src/nav/
  route_source.rs`, `src/nav/config.rs`) when live routing is configured, or
  from a committed OSRM-shaped fixture (`tests/fixtures/routes/`) for a
  simulated drive. Nothing under `src/nav/` prints a URL or a key.
- **Every pixel is engine-drawn.** The route line, the puck and the chrome
  (maneuver card, ETA, End/Simulate/pause controls) are meshes and glyphs
  the renderer builds and paints itself (`src/render/nav_layer.rs`,
  `src/render/route_layer.rs`, `src/nav/chrome.rs`) — there is no native
  MapKit/Google Navigation SDK view and no webview involved.
- **Voice prompts** pick the best installed English voice (`nav::voice::pick_voice`); on macOS/iOS, installing Enhanced/Premium voices (System Settings → Accessibility → Spoken Content → System Voice → Manage Voices) makes the engine pick them automatically.

Turning it on costs nothing when it is off: no `NavHost` state changes, no
chrome mesh is built, and (on native) the input that drives it is opt-in —
see [For integrators](#for-integrators) below.

## Running the simulated demo

A simulated drive replays a committed fixture on a timer, so it works with
no network and no key. It is the same drive on every shell (see
[The built-in routes](#the-built-in-routes)).

### Native (`mapbox_streets_demo`)

```bash
cargo run --release --example mapbox_streets_demo
```

With the window focused (the demo's window root hands the map keyboard
focus on every render; a host embedding `MapView` must do the same with
`MapView::focus_handle()`, because the library never takes focus and gpui
delivers no key to an unfocused map):

| Key | Does |
|---|---|
| `G` | Starts the demo route's simulated drive (`--nav-route`, else the default); pressing it again ends the trip |
| `Shift+G` | Cycles playback speed 1x -> 2x -> 4x -> 8x -> 1x |
| `P` | Pauses / resumes the running drive |

Right-clicking the map asks the configured routing proxy (see
[Live routing](#live-routing)) for a route from the camera's centre to the
ground under the pointer; with no proxy configured it shows a notice saying
so instead of failing silently.

Launch flags (`gpui_maps::nav::host::DemoRequest::from_args`, read once at
startup):

| Flag | Does |
|---|---|
| `--nav-sim` | Starts the demo drive immediately at launch |
| `--nav-button` | Shows the engine-drawn Simulate button; clicking it starts the drive (`MapView::set_nav_show_simulate`) |
| `--nav-route <name-or-path>` | A built-in name (see below) or a path to any OSRM-shaped JSON file. On its own it loads that route for `G` without starting anything |
| `--nav-speed <1-8>` | Initial playback speed |
| `--nav-at <metres>` | Starts the puck this far along the route |
| `--nav-paused` | Starts paused |
| `--nav-offroute <step>` | Forces an off-route deviation after the given step, for testing reroute |

```bash
cargo run --release --example mapbox_streets_demo -- --nav-sim --nav-route singapore-bugis-marina-bay
```

### Web (`./web/build.sh`)

```bash
./web/build.sh
cd web && python3 serve.py
# open http://127.0.0.1:8090/
```

The query string drives the same `DemoRequest` parser as native's flags,
with different names (`src/nav/demo.rs`):

| Query parameter | Does |
|---|---|
| `nav=sim` | Starts the demo drive immediately |
| `nav=button` | Offers an engine-drawn Simulate button instead of auto-starting |
| `route=<name>` | One of the built-in route names |
| `speed=<1-8>` | Initial playback speed |
| `nav_at=<metres>` | Starts the puck this far along the route |
| `nav_paused=1` | Starts paused |
| `offroute=<step>` | Forces an off-route deviation after the given step |

```
http://127.0.0.1:8090/?nav=sim&route=singapore-bugis-marina-bay&speed=4&nav_at=1200&offroute=5
http://127.0.0.1:8090/?nav=button
```

`build.sh` copies every fixture in `tests/fixtures/routes/` into
`dist/routes/` and fails the build if any of them still contains
`access_token=` — a simulated drive in the browser never carries a key.

### iOS (`./ios/run-device.sh`)

A device launch from the home screen gets no launch arguments, so
`--nav-sim`/`--nav-button` never reach it that way. Set the equivalent
environment variables at deploy time instead — `run-device.sh` bakes them
into the bundle's `Info.plist` (`LSEnvironment`) before signing:

```bash
GPUI_MAP_NAV=button GPUI_MAP_NAV_ROUTE=singapore-bugis-marina-bay ./ios/run-device.sh
```

`GPUI_MAP_NAV_SPEED`, `GPUI_MAP_NAV_AT`, `GPUI_MAP_NAV_PAUSED` and
`GPUI_MAP_NAV_OFFROUTE` are the same options as the web query string, as
environment variables. Without `GPUI_MAP_NAV` set, the bundle is exactly as
before: no `LSEnvironment` is written.

The app also reads `--nav-sim`/`--nav-button` from its own process
arguments first, before falling back to the environment — useful if you
launch it directly with extra argv rather than through `run-device.sh`/
`run.sh` (neither of which forwards any), e.g. on the simulator:

```bash
xcrun simctl launch --console booted dev.gpuimaps.iosdemo --nav-sim
```

iOS has no live routing: right-click has no touch equivalent, and nothing
in `ios/` reads `SWAT_ROUTING_*`.

## The camera

The follow camera has three framings (`NavCameraMode::Follow3d`, the
default, tilted and heading-up; `Follow2d`, top-down heading-up;
`NorthUp2d`) and the whole-route `Overview`
(`MapView::set_nav_camera_mode`). Studio's drive page shows one icon
button per mode beside the sim tray; one tap switches.

**Distance is the user's zoom.** While the camera follows, a wheel,
trackpad or pinch zoom does not free it: it keeps following, zooms about
the vehicle rather than the pointer, and the change becomes the follow
zoom offset (`MapView::nav_follow_zoom_offset`, zoom levels; +1 is a level
closer, clamped to -1.5..+1.5). The offset moves the whole
speed-and-maneuver zoom -- the camera still opens out at speed and
tightens before a turn, around the user's distance -- and in `Follow3d`
the tilt follows it from 45 deg (offset 0 and below) to 50 deg (+1 and
above). It holds for the rest of the trip, through the overview and back,
a pan and Recenter, and on the next trip. A pan (or a rotate or tilt)
still frees the camera; Recenter, or 20 s untouched, brings it back.
Set it from code with `MapView::set_nav_follow_zoom_offset`, or at
launch with `--nav-zoom-offset Z` or the presets `--nav-proximity
close|standard|far` (+1 / 0 / -1).

Precedent: Organic Maps keeps its follow mode through a scale gesture in
routing (`MyPositionController::ScaleEnded`; only a scroll or a twist
leaves it). MapLibre GL JS's `GeolocateControl` drops its lock on any
`movestart`, a zoom included -- the behaviour this replaces.

## The built-in routes

`gpui_maps::nav::demo::BUILTIN_ROUTES` is the one list every shell reads
from — native falls back to it by name, `web/build.sh` reads it out of
`src/nav/demo.rs` at build time so a fixture it lists can never be missing
from `dist/`, and iOS mirrors it (`ios/src/app.rs`). As of this writing it
is:

- `manila` — the default when no route is named
- `singapore-bugis-marina-bay`
- `singapore-seletar-rotary-uturn`

Each is a committed, key-stripped OSRM response under
`tests/fixtures/routes/<name>.json`.

## Live routing

Right-click-to-route (native) and the browser's `/swat-route/` proxy both
ask a real SWAT OSRM-compatible service for a route, rather than replaying
a fixture. Configuring it needs three variables (`src/nav/config.rs`):

| Variable | Meaning |
|---|---|
| `SWAT_ROUTING_URL` | The proxy's base URL, no trailing slash |
| `SWAT_ROUTING_KEY` | The proxy's access token |
| `SWAT_ROUTING_ROAD_NETWORK` | Optional; the OSRM profile name (defaults to `van`) |

Native reads these from the process environment first, then from
`~/.config/gpui-map/swat-routing.env` — a plain `KEY=VALUE` file, `#`
comments allowed, an optional `export ` prefix stripped, one pair of
matching quotes around a value removed. **Keep this file at mode `600`**
(`chmod 600 ~/.config/gpui-map/swat-routing.env`) since it holds a live
credential:

```bash
# ~/.config/gpui-map/swat-routing.env
# Placeholders only -- never commit or paste a real value here.
SWAT_ROUTING_URL=https://your-routing-proxy.example.com
SWAT_ROUTING_KEY=REPLACE_WITH_YOUR_KEY
SWAT_ROUTING_ROAD_NETWORK=van
```

With neither the environment nor the file set, right-click still works —
`NavHost` shows a notice ("live routing is off: ...") naming the missing
variable, and a simulated drive is unaffected.

**The browser never holds the key.** The page always talks to
`./swat-route` (`RoutingConfig::keyless`), same-origin, no token attached.
`web/serve.py` is the key-holding proxy: it reads the same environment
variables and config file, adds the key to the upstream request itself,
and forwards only `/swat-route/*` — checking that the request's `Host` is
loopback and that `Sec-Fetch-Site`/`Origin` are same-origin, rejecting a
request that already carries an `access_token`, and redacting the key from
every logged or returned error. Run it with:

```bash
cd web && python3 serve.py            # binds 127.0.0.1:8090
SWAT_ROUTING_URL=... SWAT_ROUTING_KEY=... python3 web/serve.py
```

or rely on `~/.config/gpui-map/swat-routing.env`, same as native. With no
key configured, `/swat-route/` answers `{"code": "NoKey"}` and the page
treats live routing as off, the same as native's notice.

## For integrators

A new `MapView` has navigation input **off**: right-click does nothing
nav-related and `G`/`Shift+G`/`P` are free for the host's own key
bindings, no notice ever appears, and no nav chrome mesh is ever built.
This matters for any app embedding `MapView` directly — an admin tool with
its own `G`/`P` shortcuts, say — since the input handlers live inside
`MapView`'s own mouse/keyboard event registration and would otherwise
intercept them unconditionally.

Opt in explicitly:

```rust
map_view.set_nav_enabled(true);
```

`MapView::nav_enabled() -> bool` reads the flag back. This gates **input**
only — right-click and the three keys. The direct API
(`MapView::set_nav_demo_route`, `MapView::start_nav_simulation`,
`MapView::set_nav_routing`) is unaffected by it, since a caller reaching
for those has already opted in by calling them. `examples/
mapbox_streets_demo.rs` calls `set_nav_enabled(true)` because it is the one
example meant to demonstrate navigation; an example that does not want the
keys and the click leaves it unset.
