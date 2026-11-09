#!/usr/bin/env python3
"""Serves `dist/` for the browser build. A short file, and every line of it
is here for a reason `python3 -m http.server` gets wrong.

* **`application/wasm`.** `WebAssembly.instantiateStreaming` REFUSES a module
  served as `application/octet-stream`; wasm-bindgen then falls back to
  `arrayBuffer()`, which works but throws away streaming compilation -- on a
  4.5 MB module that is a measurable slice of time to first paint, and it is
  the slice a naive server silently takes from you. `mimetypes` on this
  machine does not map `.wasm`, so it is stated here.
* **No caching.** A measurement run reloads the page dozens of times, and a
  304 on the wasm makes the second load's "time to first paint" a different
  quantity from the first's.
* **Quiet.** One line per tile times a 16-tile cover times a reload is noise
  that hides the one line that matters. It also means no request line -- and
  so no upstream URL -- is ever logged.
* **`127.0.0.1`, never `0.0.0.0`.** The tiles this page draws are licensed
  commercial data. Binding the loopback interface is the difference between
  a local viewer and publishing them on the network.
* **`/swat-route/` holds the routing key.** The page's live-routing requests
  (`RoutingConfig::keyless("./swat-route")`) come here, and the key is added
  HERE, from the environment or ~/.config/gpui-map/swat-routing.env, so it
  stays on this machine's loopback server and never reaches the page: not in
  the wasm, not in dist/, not in any response (bodies and errors are
  redacted). Only OSRM route paths are forwarded, only for a loopback
  `Host` and only same-origin (`Sec-Fetch-Site`, `Origin`), so neither
  another path, a DNS-rebound page nor another site open in the same browser
  can spend the key. With no key it answers `{"code": "NoKey"}`, and the page
  treats live routing as off (`NavHost::routing_unavailable`).
  `python3 serve.py --selftest` checks the rewriting and redaction offline.
* **`Range`, answered `206`.** `SimpleHTTPRequestHandler` ignores `Range`
  and sends the whole file, and the page's dot source reads a PMTiles
  archive by byte range -- it refuses a `200` rather than pull 100 MB into
  the tab (`overlay::dotgrid::shell_fetch::range_status`). One range per
  request, the only shape the page asks for.
* **`/grid/<name>.pmtiles`** serves the dot archives from
  `$GPUI_MAP_GRID_DIR` (default `~/.cache/gpui-map-data/ghsl`), so a
  100 MB-2 GB archive never has to be copied into `dist/`: `?grid=./grid/E2025_30ss.pmtiles`.
  A name not found there is looked up in `$GPUI_MAP_RACE_DIR` (default
  `~/.cache/gpui-map-data/census2020`), where the race archives live
  (race plan, MG-R15): `?grid=./grid/race-dots-2020.pmtiles`; the same
  path serves `race-hex-2020.pmtiles` and `race-clusters-2020.pmtiles`.
  (Not `$GPUI_MAP_CENSUS_DIR`: that names the census packs' folder below.)
* **`/census/<name>.bgpk`** serves the census demo's block-group packs
  (`tools/census-pack`) the same way, from `$GPUI_MAP_CENSUS_DIR` (default
  `~/.cache/gpui-map-data/census-web`): `?census=us_bg_2023`.
* **`/clickhouse/` never forwards SQL.** `?clickhouse=ghs_cells_30ss` has
  the page ask `GET /clickhouse/tile?table=&epoch=&level=&tiles=` (and
  `/clickhouse/descriptor?table=` once). Every value is validated -- the
  table against a two-name whitelist, integers in range, at most 8 tiles,
  no other parameter -- and the SQL is built HERE from a copy of
  `src/overlay/dotgrid/clickhouse.rs`'s template, pinned to it by
  `web/tests/fixtures/ch_tile_query.sql`. It goes to `$CLICKHOUSE_URL`
  (default `http://localhost:8123`) with `readonly=1` as the LAST URL
  setting (the template's own `SETTINGS` travel ahead of it as URL
  settings: `readonly=1` refuses a `SETTINGS` clause). Loopback `Host`
  and same-origin only, like the route proxy -- AND every `/clickhouse/`
  request must carry `X-Gpui-Map-Proxy: 1` (`shell_fetch::PROXY_HEADER`,
  which the page sends). That closes the gap the Origin rules leave: a
  browser without Fetch Metadata sends neither `Origin` nor
  `Sec-Fetch-Site` on a cross-site `<img>` or form GET, but no cross-site
  page can add a custom header without a CORS preflight, and this server
  answers no preflight. From a shell: `curl -H 'X-Gpui-Map-Proxy: 1' ...`.
"""
import functools, http.server, json, math, os, pathlib, re, socketserver, sys, threading, urllib.error, urllib.parse, urllib.request

ROUTING_FILE = pathlib.Path.home() / ".config" / "gpui-map" / "swat-routing.env"
ROUTE_PREFIX = "/swat-route/"
# encodeURIComponent's unreserved set, as `route_source::encode_component`.
UNRESERVED = "-_.!~*'()"
# `request_url`'s shape: `/route/v1/<network>/<lon,lat;lon,lat...>?<query>`.
NUMBER = r"-?\d+(?:\.\d+)?"
PAIR = rf"{NUMBER},{NUMBER}"
ROUTE_PATH = re.compile(rf"^/(route|tdroute)/v1/[^/?]+/({PAIR}(?:;{PAIR})+)(\?[^#]*)?$")
LOOPBACK_HOSTS = {"127.0.0.1", "localhost", "::1"}


def parse_env_file(text):
    """KEY=VALUE lines, `#` comments, `export `, one pair of quotes -- `nav::config::parse_env_file`."""
    values = {}
    for line in (text or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export "):]
        name, sep, value = line.partition("=")
        if sep:
            value = value.strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                value = value[1:-1]
            values[name.strip()] = value
    return values


def routing_config(environ=os.environ, file_text=None):
    """The environment wins over the file, variable by variable -- `RoutingConfig::resolve`."""
    values = parse_env_file(file_text)
    get = lambda name: (environ.get(name) or "").strip() or values.get(name, "").strip()
    return {"url": get("SWAT_ROUTING_URL").rstrip("/"), "network": get("SWAT_ROUTING_ROAD_NETWORK") or "van",
            "key": get("SWAT_ROUTING_KEY")}


def missing_variable(config):
    """The first variable live routing lacks, by NAME -- never a value -- or None."""
    if not config["key"]:
        return "SWAT_ROUTING_KEY"
    if not config["url"]:
        return "SWAT_ROUTING_URL"
    return None


def redact(text, key):
    """Every spelling of `key` an echo could carry: raw, percent-encoded in
    either hex case, and JSON-escaped (`\\/`, as `route_source::strip_credentials`)."""
    if not key:
        return text
    quoted = urllib.parse.quote(key, safe=UNRESERVED)
    lower_hex = re.sub(r"%[0-9A-F]{2}", lambda m: m.group(0).lower(), quoted)
    spellings = {key, quoted, lower_hex, json.dumps(key)[1:-1], json.dumps(key)[1:-1].replace("/", "\\/")}
    for spelling in sorted(spellings, key=len, reverse=True):
        text = text.replace(spelling, "[REDACTED]")
    return text


def is_same_origin(headers, port):
    """Only this page may spend the key. A cross-site `fetch` (even `no-cors`),
    an `<img src>` or a form post from any other site the user has open
    carries `Sec-Fetch-Site: cross-site` / `same-site` or a foreign `Origin`;
    curl and other non-browser clients carry neither."""
    site = (headers.get("Sec-Fetch-Site") or "").strip().lower()
    if site and site != "same-origin":
        return False
    origin = (headers.get("Origin") or "").strip().lower()
    return not origin or origin in {f"http://127.0.0.1:{port}", f"http://localhost:{port}"}


def upstream_url(path, config):
    """`/swat-route/route/v1/<any>/<coords>?...` -> SWAT, with THIS server's road network and key."""
    rest = path[len(ROUTE_PREFIX) - 1:]
    # Decoded until stable, so `access%5Ftoken` (or a doubly encoded one) is caught too.
    decoded = rest
    for _ in range(4):
        decoded = urllib.parse.unquote(decoded)
    if "access_token" in rest.lower() or "access_token" in decoded.lower():
        raise ValueError("the page must never send a key")
    match = ROUTE_PATH.match(rest)
    if not match:
        raise ValueError("only OSRM route requests are forwarded")
    kind, coordinates, query = match.group(1), match.group(2), match.group(3) or "?"
    sep = "" if query == "?" else "&"
    return (f"{config['url']}/{kind}/v1/{config['network']}/{coordinates}{query}{sep}"
            f"access_token={urllib.parse.quote(config['key'], safe=UNRESERVED)}")


def is_loopback_host(host_header):
    """The `Host` a request names, loopback only: a DNS-rebound page names its own."""
    host = (host_header or "").strip().lower()
    if host.startswith("["):
        host = host[1:host.find("]")] if "]" in host else ""
    elif host.count(":") == 1:
        host = host.split(":")[0]
    return host in LOOPBACK_HOSTS


# ---------------------------------------------------------------------------
# `/clickhouse/`: population dots from ClickHouse, by parameters, read-only.
# ---------------------------------------------------------------------------

CLICKHOUSE_PREFIX = "/clickhouse/"
CLICKHOUSE_URL = (os.environ.get("CLICKHOUSE_URL") or "http://localhost:8123").rstrip("/")
# `clickhouse::table_grid`: the tables served, and each one's base cell (arc-seconds).
CH_TABLES = {"ghs_cells_30ss": 30.0, "ghs_cells_3ss": 3.0}
# Both GHSL grids are snapped to this origin (`clickhouse::table_grid`, `GridDescriptor`).
ORIGIN_LAT, ORIGIN_LON = 90.0, -180.0
MAX_TILES = 8  # `ClickHouseGridSource::max_batch`
# `shell_fetch::PROXY_HEADER`: required on every /clickhouse/ request.
PROXY_HEADER = ("X-Gpui-Map-Proxy", "1")
EPOCHS = range(1900, 2101)
INDEX_BITS = 24
MAX_BODY_BYTES = 64 << 20  # `shell_fetch::MAX_BODY_BYTES`
CH_TIMEOUT_S = 30

# `ClickHouseGridSource::tile_query` (single epoch, the query cache on, plain),
# `epochs_query` and `max_level_query`, verbatim. `test_proxy_template_matches_rust`
# holds these to `web/tests/fixtures/ch_tile_query.sql`, which the Rust test
# `web_proxy_template_fixture` holds to clickhouse.rs.
K_SQL = ("toUInt32(row * transform(tile, [{tiles}], [{cols}], 0) + col + "
         "transform(tile, [{tiles}], [{offsets}], 0)) AS k")
TILE_SQL = ("SELECT {k},\n"
            "       toUInt32(floor(pop * 10 + 0.5)) AS deci\n"
            "FROM gpui_map_dots.{table}\n"
            "WHERE epoch = {epoch} AND level = {level} AND tile IN ({tiles}) AND deci > 0\n"
            "ORDER BY tile, row, col\n"
            "FORMAT RowBinary\n"
            "SETTINGS use_query_cache = 1, enable_http_compression = 0")
EPOCHS_SQL = "SELECT DISTINCT epoch FROM gpui_map_dots.{table} ORDER BY epoch FORMAT TabSeparated"
MAX_LEVEL_SQL = "SELECT max(level) FROM gpui_map_dots.{table} FORMAT TabSeparated"


def base_data_zoom(base_arcsec):
    """`grid::base_data_zoom`: 6 for 30″, 9 for 3″."""
    return math.floor(math.log2(40_075_016.686 / 512.0 / 30.87 / base_arcsec))


def tile_xy_to_lon_lat(x, y, zoom):
    """`geo::tile_xy_to_lon_lat`, operation for operation (f64 both sides)."""
    n = 2.0 ** zoom
    lon = x / n * 360.0 - 180.0
    lat_rad = math.atan(math.sinh(math.pi * (1.0 - 2.0 * y / n)))
    return lon, lat_rad * (180.0 / math.pi)


def cell_window(base_arcsec, level, z, x, y):
    """`grid::cell_window`: (row0, col0, rows, cols) of the cells whose centres fall in tile z/x/y."""
    arc = base_arcsec * float(1 << level)
    arc_deg = arc / 3600.0
    min_lon, max_lat = tile_xy_to_lon_lat(float(x), float(y), float(z))
    max_lon, min_lat = tile_xy_to_lon_lat(float(x) + 1.0, float(y) + 1.0, float(z))
    rows_total, cols_total = int(180.0 * 3600.0 / arc), int(360.0 * 3600.0 / arc)
    clamp = lambda v, total: min(int(max(float(math.ceil(v)), 0.0)), total)
    row0 = clamp((ORIGIN_LAT - max_lat) / arc_deg - 0.5, rows_total)
    row_end = clamp((ORIGIN_LAT - min_lat) / arc_deg - 0.5, rows_total)
    col0 = clamp((min_lon - ORIGIN_LON) / arc_deg - 0.5, cols_total)
    col_end = clamp((max_lon - ORIGIN_LON) / arc_deg - 0.5, cols_total)
    return row0, col0, max(row_end - row0, 0), max(col_end - col0, 0)


def strict_int(text, lo, hi, name):
    """A bare decimal integer in lo..=hi -- no sign, no spaces, no SQL."""
    if not re.fullmatch(r"[0-9]{1,10}", text or ""):
        raise ValueError(f"{name} must be an integer")
    value = int(text)
    if not lo <= value <= hi:
        raise ValueError(f"{name} {value} is outside {lo}..{hi}")
    return value


def clickhouse_params(query, allowed):
    """The query string as one value per name, refusing any name outside `allowed` or given twice."""
    pairs = urllib.parse.parse_qsl(query, keep_blank_values=True, strict_parsing=bool(query))
    params = {}
    for name, value in pairs:
        if name not in allowed:
            raise ValueError(f"unknown parameter {name!r}: the proxy takes {', '.join(sorted(allowed))}, never SQL")
        if name in params:
            raise ValueError(f"{name} given twice")
        params[name] = value
    missing = sorted(set(allowed) - set(params))
    if missing:
        raise ValueError(f"missing {', '.join(missing)}")
    return params


def checked_table(name):
    if name not in CH_TABLES:
        raise ValueError(f"table must be one of {', '.join(sorted(CH_TABLES))}")
    return name


def tile_sql(table, epoch, level, tile_ids):
    """`ClickHouseGridSource::tile_query` for one epoch: slots are the distinct ids, ascending."""
    base = CH_TABLES[table]
    z = base_data_zoom(base) - level
    ids = sorted(set(tile_ids))
    windows = [cell_window(base, level, z, t >> 16, t & 0xFFFF) for t in ids]
    offsets = [(slot << INDEX_BITS) - row0 * cols - col0 for slot, (row0, col0, _rows, cols) in enumerate(windows)]
    tiles = ", ".join(str(t) for t in ids)
    k = K_SQL.format(tiles=tiles, cols=", ".join(str(w[3]) for w in windows), offsets=", ".join(str(o) for o in offsets))
    return TILE_SQL.format(k=k, table=table, epoch=epoch, level=level, tiles=tiles)


def clickhouse_sql(path):
    """The SQL for a `/clickhouse/...` request path, or ValueError. Nothing from the
    page reaches the SQL but validated integers and a whitelisted table name."""
    parts = urllib.parse.urlsplit(path)
    if parts.path == CLICKHOUSE_PREFIX + "tile":
        params = clickhouse_params(parts.query, {"table", "epoch", "level", "tiles"})
        table = checked_table(params["table"])
        max_zoom = base_data_zoom(CH_TABLES[table])
        epoch = strict_int(params["epoch"], EPOCHS.start, EPOCHS.stop - 1, "epoch")
        level = strict_int(params["level"], 0, max_zoom, "level")
        z = max_zoom - level
        texts = params["tiles"].split(",")
        if not 1 <= len(texts) <= MAX_TILES:
            raise ValueError(f"1..{MAX_TILES} tiles per request, not {len(texts)}")
        ids = []
        for text in texts:
            tile = strict_int(text, 0, 0xFFFFFFFF, "tile")
            x, y = tile >> 16, tile & 0xFFFF
            if x >= 1 << z or y >= 1 << z:
                raise ValueError(f"tile {tile} is not a tile of data zoom {z}")
            ids.append(tile)
        if len(set(ids)) != len(ids):
            raise ValueError("a tile is named twice")
        return tile_sql(table, epoch, level, ids)
    if parts.path == CLICKHOUSE_PREFIX + "descriptor":
        table = checked_table(clickhouse_params(parts.query, {"table"})["table"])
        return [EPOCHS_SQL.format(table=table), MAX_LEVEL_SQL.format(table=table)]
    raise ValueError("only /clickhouse/tile and /clickhouse/descriptor are served")


def clickhouse_forward(sql, url=None):
    """(url, body) for one statement: its `SETTINGS` line moved into URL settings,
    AHEAD of `readonly=1` (ClickHouse refuses a SETTINGS clause under readonly=1,
    and refuses to change readonly itself), which is always the last."""
    body, sep, settings = sql.rpartition("\nSETTINGS ")
    if not sep:
        body, settings = sql, ""
    params = []
    for item in filter(None, (i.strip() for i in settings.split(","))):
        name, _, value = (p.strip() for p in item.partition("="))
        params.append((name, value))
    params += [("wait_end_of_query", "1"), ("max_execution_time", str(CH_TIMEOUT_S)), ("readonly", "1")]
    base = url or CLICKHOUSE_URL
    joiner = "&" if "?" in base else "/?"
    return f"{base}{joiner}{urllib.parse.urlencode(params)}", body.encode()


def clickhouse_run(sql):
    """(status, body, exception_code) from ClickHouse for one statement."""
    target, body = clickhouse_forward(sql)
    request = urllib.request.Request(target, data=body, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=CH_TIMEOUT_S) as response:
            data = response.read(MAX_BODY_BYTES + 1)
            if len(data) > MAX_BODY_BYTES:
                return 502, b"over the proxy's body limit", None
            return response.status, data, response.headers.get("X-ClickHouse-Exception-Code")
    except urllib.error.HTTPError as err:
        return err.code, err.read(4096), err.headers.get("X-ClickHouse-Exception-Code") or "?"


# ---------------------------------------------------------------------------
# `Range` (one range, `bytes=a-b` / `bytes=a-` / `bytes=-n`) and `/grid/`.
# ---------------------------------------------------------------------------

GRID_PREFIX = "/grid/"
GRID_DIR = pathlib.Path(os.environ.get("GPUI_MAP_GRID_DIR") or pathlib.Path.home() / ".cache" / "gpui-map-data" / "ghsl")
GRID_NAME = re.compile(r"^[A-Za-z0-9_.-]+\.pmtiles$")
# The race archives (MG-R15), the `/grid/` fallback folder.
RACE_DIR = pathlib.Path(os.environ.get("GPUI_MAP_RACE_DIR") or pathlib.Path.home() / ".cache" / "gpui-map-data" / "census2020")
# The census demo's `BGPK` packs (`?census=<name>`, `tools/census-pack`),
# read by `Range` like the dot archives, from `$GPUI_MAP_CENSUS_DIR`.
CENSUS_PREFIX = "/census/"
CENSUS_DIR = pathlib.Path(os.environ.get("GPUI_MAP_CENSUS_DIR") or pathlib.Path.home() / ".cache" / "gpui-map-data" / "census-web")
CENSUS_NAME = re.compile(r"^[A-Za-z0-9_.-]+\.bgpk$")


def parse_range(header, size):
    """(start, end) inclusive for a single `bytes=` range of a `size`-byte file;
    None when there is no usable Range header (serve the whole file);
    ValueError when it cannot be satisfied (416)."""
    # ASCII digits only (`\d` would take other scripts' digits), at most 19
    # (an int() of thousands of digits is refused by Python, not ignored).
    match = re.fullmatch(r"[ \t]*bytes=([0-9]{0,19})-([0-9]{0,19})[ \t]*", header or "")
    if not match or (not match.group(1) and not match.group(2)):
        return None
    first, last = match.group(1), match.group(2)
    if first and last and int(last) < int(first):
        return None  # RFC 9110 14.1.1: an invalid range-spec; the header is ignored
    if not first:
        length = int(last)
        if length == 0 or size == 0:
            raise ValueError("an empty suffix range")
        return max(size - length, 0), size - 1
    start = int(first)
    end = min(int(last), size - 1) if last else size - 1
    if start >= size:
        raise ValueError(f"bytes {start}- of a {size}-byte file")
    return start, end


def grid_file(path):
    """The archive a `/grid/<name>.pmtiles` path names, or None."""
    name = urllib.parse.unquote(urllib.parse.urlsplit(path).path[len(GRID_PREFIX):])
    if not GRID_NAME.fullmatch(name):
        return None
    for folder in (GRID_DIR, RACE_DIR):
        found = contained_file(folder, name)
        if found:
            return found
    return None


def contained_file(folder, name):
    """`folder / name` if it is a file that resolves inside `folder`, else None:
    a symlink (or anything else) resolving outside its folder is refused."""
    candidate = folder / name
    if candidate.is_file() and candidate.resolve().parent == folder.resolve():
        return candidate
    return None


def census_file(path):
    """The pack a `/census/<name>.bgpk` path names, or None."""
    name = urllib.parse.unquote(urllib.parse.urlsplit(path).path[len(CENSUS_PREFIX):])
    if not CENSUS_NAME.fullmatch(name):
        return None
    return contained_file(CENSUS_DIR, name)


def test_route_proxy_rewrites_and_redacts():
    config = routing_config(environ={"SWAT_ROUTING_KEY": "k+y/z"},
                            file_text="SWAT_ROUTING_URL=https://r.example.com/\nSWAT_ROUTING_ROAD_NETWORK=truck\n")
    assert config == {"url": "https://r.example.com", "network": "truck", "key": "k+y/z"}, config
    assert missing_variable(config) is None
    assert missing_variable(routing_config(environ={}, file_text="SWAT_ROUTING_URL=https://r\n")) == "SWAT_ROUTING_KEY"
    assert missing_variable(routing_config(environ={"SWAT_ROUTING_KEY": "k"}, file_text=None)) == "SWAT_ROUTING_URL"
    url = upstream_url("/swat-route/route/v1/van/103.8,1.3;103.81,1.31?overview=full", config)
    assert url == "https://r.example.com/route/v1/truck/103.8,1.3;103.81,1.31?overview=full&access_token=k%2By%2Fz", url
    url = upstream_url("/swat-route/route/v1/van/103.8,1.3;103.81,1.31", config)
    assert url == "https://r.example.com/route/v1/truck/103.8,1.3;103.81,1.31?access_token=k%2By%2Fz", url
    for bad in ["/swat-route/route/v1/van/1,2;3,4?access_token=x", "/swat-route/route/v1/van/1,2;3,4?ACCESS_TOKEN=x",
                "/swat-route/route/v1/van/1,2;3,4?access%5Ftoken=x", "/swat-route/route/v1/van/1,2;3,4?Access%255Ftoken=x",
                "/swat-route/route/v1/van/1,2;3,4?%61ccess_token=x",
                "/swat-route/../../etc/passwd", "/swat-route/table/v1/van/1,2;3,4", "/swat-route/route/v1/van/1,2/../x", "/swat-route/route/v1/van/..;..",
                "/swat-route/route/v1/van/1,2"]:
        try:
            upstream_url(bad, config)
            raise AssertionError(f"{bad} must be refused")
        except ValueError:
            pass
    assert "k+y/z" not in redact(f"GET {url} failed", "k+y/z") and "k%2By%2Fz" not in redact(f"GET {url} failed", "k+y/z")
    for echo in ["k%2by%2fz", "k+y\\/z", '{"url": "https://r/?access_token=k%2By%2Fz"}', "k+y/z"]:
        assert redact(echo, "k+y/z").count("[REDACTED]") == 1 and "y" not in redact(echo, "k+y/z").replace("https://r/", ""), echo
    port = 8090
    same = [{}, {"Sec-Fetch-Site": "same-origin"}, {"Origin": "http://127.0.0.1:8090"},
            {"Sec-Fetch-Site": "same-origin", "Origin": "http://localhost:8090"}]
    cross = [{"Origin": "https://evil.example.com"}, {"Origin": "http://127.0.0.1:9999"}, {"Origin": "null"},
             {"Sec-Fetch-Site": "cross-site"}, {"Sec-Fetch-Site": "same-site"},
             # A `fetch(..., {mode: "no-cors"})` or `<img src>` from another site: no Origin on a GET, but the site header.
             {"Sec-Fetch-Site": "cross-site", "Sec-Fetch-Mode": "no-cors"},
             {"Sec-Fetch-Site": "none"}, {"Sec-Fetch-Site": "same-origin", "Origin": "https://evil.example.com"}]
    assert all(is_same_origin(h, port) for h in same), [h for h in same if not is_same_origin(h, port)]
    assert not any(is_same_origin(h, port) for h in cross), [h for h in cross if is_same_origin(h, port)]
    assert all(is_loopback_host(h) for h in ["127.0.0.1:8090", "localhost:8090", "localhost", "[::1]:8090"])
    assert not any(is_loopback_host(h) for h in ["evil.example.com:8090", "", None, "127.0.0.1.evil.com:8090"])


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map,
                      ".wasm": "application/wasm", ".json": "application/json",
                      # Not read by any CSS `@font-face` -- the page fetches
                      # these as bytes and hands them to `fontdb` -- but a
                      # wrong type on a font is the kind of thing a future
                      # `<link rel=preload>` would silently ignore.
                      ".otf": "font/otf"}

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path.startswith(ROUTE_PREFIX):
            return self.proxy_route()
        if self.path.startswith(CLICKHOUSE_PREFIX):
            return self.proxy_clickhouse()
        if self.path.startswith(GRID_PREFIX):
            found = grid_file(self.path)
            if found is None:
                return self.send_error(404)
            return self.send_file(found)
        if self.path.startswith(CENSUS_PREFIX):
            found = census_file(self.path)
            if found is None:
                return self.send_error(404)
            return self.send_file(found)
        if self.headers.get("Range"):
            local = pathlib.Path(self.translate_path(self.path))
            if local.is_file():
                return self.send_file(local)
        return super().do_GET()

    def send_file(self, path):
        """`path`, whole or the one byte range asked for (206 / 416)."""
        size = path.stat().st_size
        try:
            span = parse_range(self.headers.get("Range"), size)
        except ValueError:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        start, end = span if span else (0, size - 1)
        self.send_response(206 if span else 200)
        self.send_header("Content-Type", self.guess_type(str(path)))
        self.send_header("Accept-Ranges", "bytes")
        if span:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(max(end - start + 1, 0)))
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            left = end - start + 1
            while left > 0:
                chunk = f.read(min(left, 1 << 20))
                if not chunk:
                    break
                self.wfile.write(chunk)
                left -= len(chunk)

    def proxy_clickhouse(self):
        """Population dots from ClickHouse: validated parameters in, RowBinary (or the
        descriptor as JSON) out, read-only. Raw SQL from the page is never forwarded."""
        if not is_loopback_host(self.headers.get("Host")):
            return self.reply_json(403, {"code": "Forbidden", "message": "loopback only"})
        if not is_same_origin(self.headers, self.server.server_address[1]):
            return self.reply_json(403, {"code": "Forbidden", "message": "same-origin only"})
        if (self.headers.get(PROXY_HEADER[0]) or "").strip() != PROXY_HEADER[1]:
            return self.reply_json(403, {"code": "Forbidden", "message": f"{PROXY_HEADER[0]}: {PROXY_HEADER[1]} required"})
        try:
            sql = clickhouse_sql(self.path)
        except ValueError as err:
            return self.reply_json(400, {"code": "InvalidQuery", "message": str(err)})
        try:
            if isinstance(sql, list):
                replies = [clickhouse_run(statement) for statement in sql]
                failed = next((r for r in replies if r[0] != 200 or r[2]), None)
                if failed:
                    return self.reply_clickhouse_error(*failed)
                epochs = [int(line) for line in replies[0][1].decode().split()]
                max_level = int(replies[1][1].decode().strip())
                return self.reply_json(200, {"epochs": epochs, "max_level": max_level})
            status, body, code = clickhouse_run(sql)
        except Exception as err:  # noqa: BLE001 -- unreachable, reset, a reply that is not numbers
            return self.reply_json(502, {"code": "ProxyError", "message": type(err).__name__ + ": ClickHouse did not answer usably"})
        if status != 200 or code:
            return self.reply_clickhouse_error(status, body, code)
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def reply_clickhouse_error(self, status, body, code):
        text = body[:512].decode("utf-8", "replace")
        self.send_response(502 if status == 200 else status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        if code:
            self.send_header("X-ClickHouse-Exception-Code", re.sub(r"[^0-9?]", "", str(code))[:8] or "?")
        payload = text.encode()
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def reply_json(self, status, payload):
        self.reply_body(status, json.dumps(payload).encode())

    def reply_body(self, status, body):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def proxy_route(self):
        """The page's route requests, with the key added HERE: the browser never holds it."""
        if not is_loopback_host(self.headers.get("Host")):
            return self.reply_json(403, {"code": "Forbidden", "message": "loopback only"})
        if not is_same_origin(self.headers, self.server.server_address[1]):
            return self.reply_json(403, {"code": "Forbidden", "message": "same-origin only"})
        config = routing_config(file_text=ROUTING_FILE.read_text() if ROUTING_FILE.exists() else None)
        missing = missing_variable(config)
        if missing:
            # Short on purpose: the page shows it after "No route: the routing
            # service answered NoKey: ", on a phone-width notice.
            return self.reply_json(503, {"code": "NoKey", "message": f"{missing} not set in serve.py"})
        try:
            target = upstream_url(self.path, config)
        except ValueError as err:
            return self.reply_json(400, {"code": "InvalidQuery", "message": str(err)})
        try:
            # 15 s: under `NavHost::REQUEST_TIMEOUT_MS`, so the page's own
            # deadline never fires on an answer that is still coming.
            with urllib.request.urlopen(target, timeout=15) as response:
                status, body = response.status, response.read()
        except urllib.error.HTTPError as err:
            try:
                status, body = err.code, err.read()
            except Exception as read_err:  # noqa: BLE001 -- a body that stalls or resets mid-read
                return self.reply_json(502, {"code": "ProxyError", "message": redact(str(read_err), config["key"])})
        except Exception as err:  # noqa: BLE001 -- any failure becomes a redacted JSON error
            return self.reply_json(502, {"code": "ProxyError", "message": redact(str(err), config["key"])})
        self.reply_body(status, redact(body.decode("utf-8", "replace"), config["key"]).encode())


# ---------------------------------------------------------------------------
# Tests: `python3 serve.py --selftest` runs every `test_*` below (offline: the
# ClickHouse they talk to is a fake on loopback). pytest collects them too.
# ---------------------------------------------------------------------------

FIXTURE = pathlib.Path(__file__).resolve().parent / "tests" / "fixtures" / "ch_tile_query.sql"
GOOD_TILE = "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=2883611,2949147"


def fixture_cases():
    """[(header, sql)] from the Rust-written fixture."""
    cases, header, lines = [], None, []
    for line in FIXTURE.read_text().splitlines():
        if line.startswith("-- case "):
            if header:
                cases.append((header, "\n".join(lines)))
            header, lines = line[len("-- case "):], []
        elif header is not None:
            lines.append(line)
    if header:
        cases.append((header, "\n".join(lines)))
    return cases


class FakeClickHouse:
    """A loopback stand-in that records what the proxy sends it."""

    def __init__(self):
        self.seen = []
        fake = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode()
                fake.seen.append((self.path, body))
                reply = b"1975\n2025\n" if "DISTINCT epoch" in body else b"6\n" if "max(level)" in body else b"\x01\x00\x00\x00\x0a\x00\x00\x00"
                self.send_response(200)
                self.send_header("Content-Length", str(len(reply)))
                self.end_headers()
                self.wfile.write(reply)

        self.server = Server(("127.0.0.1", 0), H)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Page:
    """This file's own server, on an ephemeral loopback port, over `root`."""

    def __init__(self, root):
        self.server = Server(("127.0.0.1", 0), functools.partial(Handler, directory=str(root)))
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def get(self, path, headers=None, method="GET", proxy_header=True):
        """GET `path`; a /clickhouse/ request carries the page's proxy header unless `proxy_header` is False."""
        import http.client
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        extra = {PROXY_HEADER[0]: PROXY_HEADER[1]} if proxy_header and path.startswith(CLICKHOUSE_PREFIX) else {}
        conn.request(method, path, headers={"Host": f"127.0.0.1:{self.port}", **extra, **(headers or {})})
        response = conn.getresponse()
        result = response.status, dict(response.getheaders()), response.read()
        conn.close()
        return result

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def with_proxy(check):
    """Runs `check(page, fake)` against this server and a fake ClickHouse."""
    import tempfile
    global CLICKHOUSE_URL
    fake, saved = FakeClickHouse(), CLICKHOUSE_URL
    CLICKHOUSE_URL = fake.url
    with tempfile.TemporaryDirectory() as root:
        page = Page(root)
        try:
            check(page, fake)
        finally:
            page.close()
            fake.close()
            CLICKHOUSE_URL = saved


def refused(path):
    try:
        clickhouse_sql(path)
    except ValueError:
        return True
    return False


def test_proxy_rejects_raw_sql():
    for path in ["/clickhouse/?query=SELECT%201", "/clickhouse/query?sql=SELECT+1",
                 "/clickhouse/tile?query=SELECT+1+FROM+system.users",
                 GOOD_TILE + "&query=DROP+TABLE+gpui_map_dots.ghs_cells_30ss",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025+OR+1%3D1&level=0&tiles=1",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=1)+UNION+SELECT+name+FROM+system.users--",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=-2025&level=0&tiles=1",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=0x1",
                 "/clickhouse/tile?table=ghs_cells_30ss&table=ghs_cells_3ss&epoch=2025&level=0&tiles=1",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=7&tiles=0",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=4194304",
                 "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=1,1",
                 "/clickhouse/descriptor?table=ghs_cells_30ss&sql=SELECT+1"]:
        assert refused(path), path
    assert not refused(GOOD_TILE)

    def check(page, fake):
        status, _, body = page.get("/clickhouse/?query=SELECT%201")
        assert status == 400, (status, body)
        status, _, body = page.get("/clickhouse/tile?query=SELECT%201")
        assert status == 400 and b"never SQL" in body, (status, body)
        status, _, _ = page.get("/clickhouse/tile", method="POST")
        assert status == 501, status
        assert fake.seen == [], fake.seen
    with_proxy(check)


def test_proxy_rejects_unknown_table():
    for table in ["system.users", "ghs_cells_30ss_x", "gpui_map_dots.ghs_cells_30ss", "GHS_CELLS_30SS", "", "_dotgrid_owner"]:
        assert refused(f"/clickhouse/tile?table={urllib.parse.quote(table)}&epoch=2025&level=0&tiles=1"), table
        assert refused(f"/clickhouse/descriptor?table={urllib.parse.quote(table)}"), table
    assert "FROM gpui_map_dots.ghs_cells_3ss" in clickhouse_sql("/clickhouse/tile?table=ghs_cells_3ss&epoch=2025&level=0&tiles=1")


def test_proxy_rejects_foreign_origin():
    def check(page, fake):
        for headers in [{"Origin": "https://evil.example.com"}, {"Origin": "http://127.0.0.1:1"}, {"Origin": "null"},
                        {"Sec-Fetch-Site": "cross-site"}, {"Sec-Fetch-Site": "same-site"},
                        {"Host": "evil.example.com"}]:
            status, _, _ = page.get(GOOD_TILE, headers)
            assert status == 403, (headers, status)
        assert fake.seen == [], fake.seen
        # Same-origin: forwarded, read-only, and the rows come back as they are.
        status, _, body = page.get(GOOD_TILE, {"Origin": f"http://127.0.0.1:{page.port}", "Sec-Fetch-Site": "same-origin"})
        assert status == 200 and body == b"\x01\x00\x00\x00\x0a\x00\x00\x00", (status, body)
        (path, sql), = fake.seen
        assert path.endswith("&readonly=1") and "use_query_cache=1" in path and "wait_end_of_query=1" in path, path
        assert "SETTINGS" not in sql and sql.endswith("FORMAT RowBinary"), sql
        status, _, body = page.get("/clickhouse/descriptor?table=ghs_cells_30ss")
        assert status == 200 and json.loads(body) == {"epochs": [1975, 2025], "max_level": 6}, (status, body)
        assert all(p.endswith("&readonly=1") for p, _ in fake.seen[1:]), fake.seen
    with_proxy(check)


def test_proxy_requires_its_header():
    """Review F4: a GET with no Origin and no Sec-Fetch-Site (a cross-site <img> in a browser
    without Fetch Metadata, or curl) is refused unless it carries X-Gpui-Map-Proxy: 1."""
    def check(page, fake):
        for path in [GOOD_TILE, "/clickhouse/descriptor?table=ghs_cells_30ss"]:
            status, _, body = page.get(path, proxy_header=False)
            assert status == 403 and b"X-Gpui-Map-Proxy" in body, (path, status, body)
            status, _, _ = page.get(path, {"Accept": "image/avif,image/webp,*/*"}, proxy_header=False)
            assert status == 403, (path, status)
            status, _, _ = page.get(path, {PROXY_HEADER[0]: "0"}, proxy_header=False)
            assert status == 403, (path, status)
        assert fake.seen == [], fake.seen
        status, _, _ = page.get(GOOD_TILE)
        assert status == 200 and len(fake.seen) == 1, status
    with_proxy(check)


def test_proxy_rejects_nine_tiles():
    ids = [(x << 16) | 27 for x in range(40, 49)]
    nine = "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=" + ",".join(map(str, ids))
    eight = "/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=" + ",".join(map(str, ids[:8]))
    assert refused(nine) and not refused(eight)
    assert refused("/clickhouse/tile?table=ghs_cells_30ss&epoch=2025&level=0&tiles=")


def star_or_foreign(sql):
    """A `*` column list, or a FROM/JOIN of anything outside gpui_map_dots (or any `system.`)."""
    star = re.search(r"\bSELECT\s+(DISTINCT\s+)?\*|,\s*\*|\.\*|\bCOLUMNS\s*\(", sql, re.I)
    sources = re.findall(r"\b(?:FROM|JOIN)\s+([A-Za-z0-9_.`\"]+)", sql, re.I)
    foreign = [t for t in sources if not re.fullmatch(r"gpui_map_dots\.[A-Za-z0-9_]+", t)]
    return bool(star) or bool(foreign) or "system." in sql.lower()


def test_proxy_sql_has_no_star_and_only_gpui_map_dots():
    assert star_or_foreign("SELECT * FROM gpui_map_dots.t") and star_or_foreign("SELECT a FROM system.users")
    assert star_or_foreign("SELECT a FROM gpui_map_dots.t JOIN other.t USING a")
    statements = [clickhouse_sql(GOOD_TILE)] + clickhouse_sql("/clickhouse/descriptor?table=ghs_cells_3ss")
    statements += [sql for _, sql in fixture_cases()]
    for sql in statements:
        assert not star_or_foreign(sql), sql
        assert re.match(r"SELECT\s", sql), sql


def test_proxy_template_matches_rust():
    cases = fixture_cases()
    assert len(cases) >= 6, f"{FIXTURE} lost its cases"
    for header, want in cases:
        kind, *fields = header.split()
        params = dict(f.split("=", 1) for f in fields)
        if kind == "tile":
            ids = [int(t) for t in params["tiles"].split(",")]
            got = tile_sql(params["table"], int(params["epoch"]), int(params["level"]), ids)
            path = (f"/clickhouse/tile?table={params['table']}&epoch={params['epoch']}"
                    f"&level={params['level']}&tiles={params['tiles']}")
            assert clickhouse_sql(path) == want, header
        elif kind == "epochs":
            got = EPOCHS_SQL.format(table=params["table"])
        else:
            got = MAX_LEVEL_SQL.format(table=params["table"])
        assert got == want, f"{header}\n--- rust\n{want}\n--- python\n{got}"


def test_forward_moves_settings_ahead_of_readonly():
    url, body = clickhouse_forward("SELECT 1\nFORMAT RowBinary\nSETTINGS use_query_cache = 1, enable_http_compression = 0", "http://ch:8123")
    assert url == ("http://ch:8123/?use_query_cache=1&enable_http_compression=0&wait_end_of_query=1"
                   "&max_execution_time=30&readonly=1"), url
    assert body == b"SELECT 1\nFORMAT RowBinary"
    url, _ = clickhouse_forward("SELECT 1 FORMAT TabSeparated", "http://ch:8123/?user=dots")
    assert url == "http://ch:8123/?user=dots&wait_end_of_query=1&max_execution_time=30&readonly=1", url


def test_range_is_answered_206():
    assert parse_range(None, 10) is None and parse_range("items=0-1", 10) is None
    assert parse_range("bytes=2-5", 10) == (2, 5) and parse_range("bytes=8-", 10) == (8, 9)
    assert parse_range("bytes=-3", 10) == (7, 9) and parse_range("bytes=0-99", 10) == (0, 9)
    # Review F5/F6: other scripts' digits and a reversed range are ignored (whole file).
    assert parse_range("bytes=\u0660-\u0665", 10) is None and parse_range("bytes=5-2", 10) is None
    assert parse_range("bytes=" + "1" * 5000 + "-", 10) is None
    for bad in ["bytes=10-", "bytes=-0"]:
        try:
            parse_range(bad, 10)
            raise AssertionError(bad)
        except ValueError:
            pass
    import tempfile
    global GRID_DIR, RACE_DIR
    with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as census:
        (pathlib.Path(root) / "a.bin").write_bytes(bytes(range(100)))
        (pathlib.Path(root) / "w.pmtiles").write_bytes(bytes(range(50)))
        (pathlib.Path(census) / "race-dots-2020.pmtiles").write_bytes(bytes(range(30)))
        (pathlib.Path(census) / "w.pmtiles").write_bytes(b"census copy")
        saved, GRID_DIR = GRID_DIR, pathlib.Path(root)
        saved_race, RACE_DIR = RACE_DIR, pathlib.Path(census)
        page = Page(root)
        try:
            status, headers, body = page.get("/a.bin", {"Range": "bytes=10-19"})
            assert status == 206 and body == bytes(range(10, 20)) and headers["Content-Range"] == "bytes 10-19/100", (status, headers)
            status, _, body = page.get("/a.bin")
            assert status == 200 and len(body) == 100
            status, headers, _ = page.get("/a.bin", {"Range": "bytes=100-"})
            assert status == 416 and headers["Content-Range"] == "bytes */100", status
            status, _, body = page.get("/grid/w.pmtiles", {"Range": "bytes=0-6"})
            assert status == 206 and body == bytes(range(7)), status
            # The race archives (MG-R15) come from the race folder, by range; the grid folder wins a name clash.
            status, headers, body = page.get("/grid/race-dots-2020.pmtiles", {"Range": "bytes=3-5"})
            assert status == 206 and body == bytes([3, 4, 5]) and headers["Content-Range"] == "bytes 3-5/30", (status, headers)
            assert page.get("/grid/w.pmtiles", {"Range": "bytes=0-6"})[2] == bytes(range(7))
            # Review N5: a `.pmtiles` symlink to a file outside the folder is a 404; one inside it is served.
            with tempfile.TemporaryDirectory() as outside:
                (pathlib.Path(outside) / "secret.pmtiles").write_bytes(b"outside")
                (pathlib.Path(census) / "escape.pmtiles").symlink_to(pathlib.Path(outside) / "secret.pmtiles")
                (pathlib.Path(root) / "escape2.pmtiles").symlink_to(pathlib.Path(outside) / "secret.pmtiles")
                (pathlib.Path(census) / "alias.pmtiles").symlink_to(pathlib.Path(census) / "race-dots-2020.pmtiles")
                for bad in ["/grid/escape.pmtiles", "/grid/escape2.pmtiles"]:
                    assert page.get(bad, {"Range": "bytes=0-1"})[0] == 404, bad
                assert page.get("/grid/alias.pmtiles", {"Range": "bytes=0-1"})[0] == 206
            for bad in ["/grid/../a.bin", "/grid/%2e%2e%2fa.bin", "/grid/a.bin", "/grid/missing.pmtiles", "/grid/w.pmtiles%0A",
                        "/grid/..%2f" + pathlib.Path(census).name + "%2frace-dots-2020.pmtiles"]:
                assert page.get(bad, {"Range": "bytes=0-1"})[0] == 404, bad
        finally:
            page.close()
            GRID_DIR = saved
            RACE_DIR = saved_race


def test_census_route_serves_only_bgpk_inside_the_dir():
    import tempfile
    global CENSUS_DIR
    with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as census:
        (pathlib.Path(root) / "serve.py").write_bytes(b"secret")
        (pathlib.Path(census) / "us_bg_2023.bgpk").write_bytes(bytes(range(200)))
        (pathlib.Path(census) / "x.pmtiles").write_bytes(bytes(range(10)))
        (pathlib.Path(census) / "a").mkdir()
        (pathlib.Path(census) / "a" / "b.bgpk").write_bytes(bytes(range(10)))
        # A `.bgpk` symlink resolving outside the folder is refused, as on `/grid/`.
        (pathlib.Path(census) / "escape.bgpk").symlink_to(pathlib.Path(root) / "serve.py")
        saved, CENSUS_DIR = CENSUS_DIR, pathlib.Path(census)
        page = Page(root)
        try:
            status, headers, body = page.get("/census/us_bg_2023.bgpk", {"Range": "bytes=0-63"})
            assert status == 206 and body == bytes(range(64)), status
            assert headers["Content-Range"] == "bytes 0-63/200", headers
            for bad in ["/census/../serve.py", "/census/%2e%2e%2fserve.py", "/census/x.pmtiles",
                        "/census/a/b.bgpk", "/census/missing.bgpk", "/census/escape.bgpk"]:
                assert page.get(bad, {"Range": "bytes=0-1"})[0] == 404, bad
        finally:
            page.close()
            CENSUS_DIR = saved


class Server(socketserver.ThreadingTCPServer):
    # Threaded, so a route request (up to 30 s upstream) never stalls tiles.
    allow_reuse_address = True
    daemon_threads = True


def selftest():
    tests = [(name, fn) for name, fn in sorted(globals().items()) if name.startswith("test_") and callable(fn)]
    for name, fn in tests:
        fn()
        print(f"  ok  {name}")
    print(f"serve.py selftest: ok ({len(tests)} tests)")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8090
    root = pathlib.Path(__file__).resolve().parent / "dist"
    if not (root / "index.html").exists():
        sys.exit(f"{root} has no index.html -- run ./build.sh first")
    with Server(("127.0.0.1", port), functools.partial(Handler, directory=str(root))) as httpd:
        print(f"http://127.0.0.1:{port}/  (serving {root}; dot archives /grid/ from {GRID_DIR} and {RACE_DIR}; "
              f"census packs /census/ from {CENSUS_DIR}; /clickhouse/ read-only)", flush=True)
        httpd.serve_forever()


if __name__ == "__main__":
    main()
