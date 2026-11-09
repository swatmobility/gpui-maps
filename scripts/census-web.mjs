// MG-C5, MG-C6 and MG-C7 (census-in-the-browser plan, Task 1): the census
// page's load time, wasm heap and hover, over CDP. The CDP plumbing is
// scripts/mg24-web.mjs's.
//
//   1. Build and serve: `web/build.sh`, then `python3 web/serve.py 8090`
//      (it serves the pack at /census/ from ~/.cache/gpui-map-data/census-web).
//   2. Start Chrome with a debugging port, e.g.
//        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
//          --remote-debugging-port=9337 --user-data-dir=/tmp/census-web-profile \
//          --window-size=1280,800 --no-first-run about:blank
//      (add --headless=new, with `gl=1`, for a headless run). Keep the window
//      in front: a background tab throttles rAF.
//   3. node scripts/census-web.mjs 9337 http://127.0.0.1:8090/ 'census=us_bg_2023' load,hover,drag /tmp/census-gpu
//      Add `&gl=1` for WebGL2. Writes <out>-load.json/.png/.log and
//      <out>-hover.json.
//
// load:  navigate; poll map_census_status_json every 100 ms until every
//        state's source drew (or 30 s); first_state_ms and all_drawn_ms are
//        from navigation (the page's own clock); the worst frame interval
//        during the load; map_memory_json's wasm_heap_bytes after it.
// hover: at two cameras, 500 map_census_hover_json calls on a grid, one per
//        rAF, each timed with performance.now() in the page; the page's
//        drawn-frame counter before and after (a hover must not redraw).
//        Then the popup at five block groups' TIGER internal points, which
//        must equal the native strings (src/census/names.rs,
//        native_popup_strings_for_the_gate_points) and the ACS income.
// charts: MG-C10 (histogram part). `map_jump_to` to 3 cameras (national, LA
//        z9, NYC z11), settling at each, then times map_census_histograms_json
//        five times per camera with performance.now(). Pass: every camera's
//        max <= 30 ms AND its in_view is present with a nonzero total (a
//        stuck-null or empty in_view fails the gate instead of just
//        reporting a suspiciously fast, empty row).
//        Then MG-C10 (panel part): map_census_in_view_json after each
//        settle, timed COLD (map_census_invalidate_in_view first) together
//        with the histogram (<= 30 ms, and the page's own recount_ms
//        <= 30 ms), with at least one state listed; map_census_states_json +
//        map_census_metros_json once per rAF through a 3 s band drag
//        (p95 <= 3 ms), each non-empty (52 states, 25 metros); and a real
//        pointer hover over three States rows then off the table, which
//        must repaint table-only (rebake == 0, replan == 0, summed by
//        map_census_dim_json) and restore.
//        First, after the load: no States row left `nofly`, and a real
//        click on California's row flies the camera there.
//        Then a real pointer drag on the slider track (90 moves) with the
//        States tab open, each page show() timed by the page (p95 <= 8 ms);
//        the Metros tab must draw 25 ridges with real pixels.
//        Screenshots: <out>-charts.png, <out>-panel-{states,states-range,metros,inview}.png.
//
// Every mode records uncaught page exceptions and console.error calls
// (Runtime.exceptionThrown): charts and drag fail on any.
// drag:  MG-C8. Settle at the national view, select stops 20..60, grab the
//        band's middle and drag it to the track's far end and back over
//        6 s, one map_census_slider_drag per rAF. Reports the export's p95
//        and max, the rAF interval p95 and max, the overlay update times
//        during the drag (the full paint check included), the paint paths
//        taken, and whether any class-changing re-bake happened by 2 s
//        after release. Pass: rebake == 0, replan == 0, no epoch moved,
//        export p95 <= 4 ms; frame p95 is judged per backend (16.7 ms
//        WebGPU, 33 ms gl=1). `&paint_check_settle_ms=0` restores the
//        full check on every tick, for the A/B.
// pan:   MG-C9. Settle at camera=-98.35,39.5,3.7, then a 6 s circular pan
//        (one full ellipse, 16 deg of longitude by 8 of latitude, starting
//        and ending at the start camera), one map_jump_to per rAF. Reports the rAF interval p50, p95
//        and max, the page's own cpu_ms samples and the frames drawn (a run
//        that drew nothing proves nothing). Pass: frame p95 <= 33 ms WebGPU,
//        <= 50 ms gl=1 (judged from the query), frames drawn > 0, no errors.
// capture: MG-C11's web half. The viewport forced to 1280x800 CSS px at dpr 1
//        (CENSUS_CAPTURE_DPR=2 for a Retina native snapshot's density),
//        `&census_ui=0&camera=-98.35,39.5,3.7`, settle (all states drawn,
//        the map settled, the drawn-frame count still for 1.5 s), then a
//        screenshot to <out>-capture.png. With MAPBOX_ACCESS_TOKEN in the
//        environment the page gets it as `&access_token=` (the census page
//        then picks Mapbox Light, as native `--basemap mapbox` does); the
//        URL is never printed or written, and the report records only
//        whether a token was passed. Without one the page draws Positron.
import fs from 'node:fs';

const [port, base, query, modesArg, out] = process.argv.slice(2);
const modes = new Set((modesArg ?? 'load').split(','));
const LOAD_TIMEOUT_MS = Number(process.env.CENSUS_LOAD_TIMEOUT_MS ?? 30000);
const HOVER_CAMERAS = [
  { name: 'national', camera: '-98.35,39.5,3.7' },
  { name: 'nyc_z10', camera: '-73.98,40.75,10' },
];
// GEOID, TIGER 2023 INTPTLON/INTPTLAT, the native popup's title and
// subtitle, the ACS 2023 median household income as the popup prints it.
const GATE_POINTS = [
  ['060375432011', -118.2387765, 33.8845984, 'Los Angeles County, California', 'Tract 5432.01 · Block group 1', 'Median household income: $100,250'],
  ['360610101001', -73.9915412, 40.7497314, 'New York County, New York', 'Tract 101 · Block group 1', 'Median household income: $106,528'],
  ['170318391001', -87.6316474, 41.8828966, 'Cook County, Illinois', 'Tract 8391 · Block group 1', 'Median household income: $124,641'],
  ['020200011001', -149.8912614, 61.2177089, 'Anchorage Municipality, Alaska', 'Tract 11 · Block group 1', 'Median household income: $44,975'],
  ['721270004001', -66.1154577, 18.4688707, 'San Juan Municipio, Puerto Rico', 'Tract 4 · Block group 1', 'Median household income: $18,377'],
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const page = targets.find((t) => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const logs = [];
// Every uncaught page exception and console.error since the last
// loadPage: a throw inside the page's own handlers (a tab's draw, a
// hover, a rAF callback) never reaches an `evaluate`, so each mode's
// pass requires this empty.
const errors = [];
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  } else if (msg.method === 'Runtime.consoleAPICalled') {
    const text = msg.params.args.map((a) => a.value ?? a.description).join(' ');
    logs.push(text);
    if (msg.params.type === 'error') errors.push(`console.error: ${text}`.slice(0, 400));
  } else if (msg.method === 'Runtime.exceptionThrown') {
    const d = msg.params.exceptionDetails;
    errors.push(`exception: ${d?.exception?.description ?? d?.text ?? 'unknown'}`.slice(0, 400));
  }
};
const send = (method, params = {}) =>
  new Promise((r) => {
    const i = ++id;
    pending.set(i, r);
    ws.send(JSON.stringify({ id: i, method, params }));
  });
const evaluate = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 400));
  return r.result?.result?.value;
};
// A page-side animation (a rAF loop resolving a promise) with a Node-side
// deadline: a throttled or wedged page never resolves it, and an unbounded
// await would hang the harness forever. On timeout the mode's row is written
// as a FAIL with the reason, the report saved, and the process exits 4.
// CENSUS_ANIMATION_DEADLINE_MS overrides the 30 s (a short one proves the path).
const ANIMATION_DEADLINE_MS = Number(process.env.CENSUS_ANIMATION_DEADLINE_MS ?? 30000);
async function evaluateAnimation(mode, expr, ms = ANIMATION_DEADLINE_MS) {
  let timer;
  const deadline = new Promise((resolve) => {
    timer = setTimeout(() => resolve({ timedOut: true }), ms);
  });
  const r = await Promise.race([evaluate(expr).then((value) => ({ value })), deadline]);
  clearTimeout(timer);
  if (!r.timedOut) return r.value;
  const row = { mode, query, pass: false, error: `timeout: the page's ${mode} animation did not finish within ${ms} ms`, errors: [...errors] };
  report[mode] = row;
  console.error(`census-web: ${mode} FAILED -- ${row.error}`);
  console.log(JSON.stringify(row));
  fs.writeFileSync(`${out}.json`, JSON.stringify(report, null, 1));
  ws.close();
  process.exit(4);
}
const json = async (expr) => {
  try {
    return JSON.parse((await evaluate(expr)) ?? 'null');
  } catch (e) {
    return null;
  }
};
await send('Runtime.enable');
await send('Page.enable');

const status = () => json('window.gpuiMap && window.gpuiMap.map_census_status_json ? window.gpuiMap.map_census_status_json() : "null"');

// Navigates and waits until every state's source drew, sampling the frame
// intervals as it goes. Returns what it saw.
async function loadPage(url) {
  logs.length = 0;
  errors.length = 0;
  await send('Page.navigate', { url });
  const t0 = Date.now();
  let st = null;
  let worst = 0;
  let tileDraws = 0;
  let intervals = 0;
  while (Date.now() - t0 < LOAD_TIMEOUT_MS) {
    await sleep(100);
    st = await status();
    const samples = await json('window.gpuiMap ? window.gpuiMap.map_frame_samples_json() : "null"');
    for (const v of samples?.interval_ms ?? []) {
      worst = Math.max(worst, v);
      intervals++;
    }
    const ov = await json('window.gpuiMap ? window.gpuiMap.map_overlay_stats_json() : "null"');
    tileDraws = Math.max(tileDraws, ov?.tile_draws ?? 0);
    if (st && st.total > 0 && st.all_drawn_ms != null) break;
  }
  return { st, worst, intervals, tileDraws, wall_s: (Date.now() - t0) / 1000 };
}

async function settle(maxMs = 20000) {
  const t0 = Date.now();
  let last = -1;
  let stableSince = Date.now();
  while (Date.now() - t0 < maxMs) {
    await sleep(250);
    const st = await status();
    const settled = await evaluate('window.gpuiMap.map_is_settled()');
    const f = st?.frames_drawn ?? -1;
    if (f !== last) {
      last = f;
      stableSince = Date.now();
    }
    if (settled && Date.now() - stableSince > 1500) return true;
  }
  return false;
}

const report = {};

if (modes.has('load')) {
  const r = await loadPage(`${base}?${query}`);
  await sleep(1000);
  const memory = await json('window.gpuiMap.map_memory_json()');
  const ov = await json('window.gpuiMap.map_overlay_stats_json()');
  const row = {
    mode: 'load',
    query,
    visibility: await evaluate('document.visibilityState'),
    loaded: r.st?.loaded,
    total: r.st?.total,
    sources_drawn: r.st?.sources_drawn,
    first_state_ms: r.st?.first_state_ms,
    all_drawn_ms: r.st?.all_drawn_ms,
    worst_load_interval_ms: r.worst,
    load_intervals: r.intervals,
    tile_draws_max: Math.max(r.tileDraws, ov?.tile_draws ?? 0),
    wasm_heap_bytes: memory?.wasm_heap_bytes,
    wasm_heap_mb: memory ? memory.wasm_heap_bytes / 1048576 : null,
    js_heap_mb: memory ? memory.js_heap_bytes / 1048576 : null,
    gpu_buffer_mb: memory ? memory.gpu_buffer_bytes_live / 1048576 : null,
    backend: logs.find((l) => l.startsWith('gpu: ')) ?? null,
    census_log: logs.filter((l) => l.startsWith('census:')),
    errors: [...errors],
    wall_s: r.wall_s,
  };
  report.load = row;
  console.log(JSON.stringify(row));
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-load.png`, Buffer.from(shot.result.data, 'base64'));
  fs.writeFileSync(`${out}-load.log`, logs.join('\n'));
}

if (modes.has('hover')) {
  const rows = [];
  for (const c of HOVER_CAMERAS) {
    const r = await loadPage(`${base}?${query}&camera=${c.camera}`);
    const settled = await settle();
    const before = (await status())?.frames_drawn;
    // One hover per rAF on a 25 x 20 grid over the canvas, as a pointer
    // would arrive; each call timed in the page.
    const times = await evaluate(`new Promise((done) => {
      const g = window.gpuiMap, cv = document.getElementById('map');
      const w = cv.clientWidth, h = cv.clientHeight, out = [];
      let i = 0, hits = 0;
      const step = () => {
        if (i >= 500) { done({ out, hits }); return; }
        const x = ((i % 25) + 0.5) * w / 25, y = (Math.floor(i / 25) + 0.5) * h / 20;
        const t = performance.now();
        const s = g.map_census_hover_json(x, y);
        out.push(performance.now() - t);
        if (s !== 'null') hits++;
        i++;
        requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    })`);
    const after = (await status())?.frames_drawn;
    const sorted = [...times.out].sort((a, b) => a - b);
    const row = {
      camera: c.name,
      all_drawn_ms: r.st?.all_drawn_ms,
      settled,
      hovers: times.out.length,
      hits: times.hits,
      mean_ms: times.out.reduce((a, b) => a + b, 0) / times.out.length,
      p95_ms: sorted[Math.floor(0.95 * sorted.length)],
      max_ms: sorted[sorted.length - 1],
      frames_before: before,
      frames_after: after,
      redraws: after - before,
    };
    rows.push(row);
    console.log(JSON.stringify(row));
  }
  // The five gate points: jump there, settle, hover the centre.
  const points = [];
  for (const [geoid, lon, lat, title, subtitle, income] of GATE_POINTS) {
    await evaluate(`window.gpuiMap.map_jump_to(${lon}, ${lat}, 14, 0, 0)`);
    await settle(10000);
    const info = await json(`(() => { const cv = document.getElementById('map');
      return window.gpuiMap.map_census_hover_json(cv.clientWidth / 2, cv.clientHeight / 2); })()`);
    const ok = !!info && info.geoid === geoid && info.title === title && info.subtitle === subtitle && info.income === income;
    points.push({ geoid, ok, got: info });
    console.log(JSON.stringify({ geoid, ok, got: info }));
  }
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-hover-last.png`, Buffer.from(shot.result.data, 'base64'));
  report.hover = { rows, points, all_points_ok: points.every((p) => p.ok), errors: [...errors] };
}

if (modes.has('charts')) {
  // MG-C10 (histogram part): the national histogram is built once at load,
  // so what this times is `view::in_view_indices` over every pushed
  // block group at 3 cameras of increasing detail -- the in-view recount's
  // worst case, not a cold path.
  const CHART_CAMERAS = [
    { name: 'national', camera: [-98.35, 39.5, 3.7] },
    { name: 'la_z9', camera: [-118.25, 34.05, 9] },
    { name: 'nyc_z11', camera: [-73.98, 40.75, 11] },
  ];
  const r = await loadPage(`${base}?${query}`);
  await settle();

  // Fly-to after a normal load: the States tab was drawn while loading
  // (every row "still loading"); once the load completed, no row may be
  // left `nofly`, and a real click on one must move the camera to it.
  await sleep(500);
  const flyGeo = await json(`(() => {
    const rows = [...document.querySelectorAll('#census-panel tbody tr[data-fips]')];
    const ca = rows.find((tr) => tr.dataset.fips === '6');
    const b = ca ? ca.getBoundingClientRect() : null;
    return JSON.stringify({ rows: rows.length, nofly: document.querySelectorAll('#census-panel tr.nofly').length,
      title: ca ? ca.title : null, x: b ? b.left + b.width / 2 : null, y: b ? b.top + b.height / 2 : null,
      camera: JSON.parse(window.gpuiMap.map_camera_json()) });
  })()`);
  if (flyGeo?.x != null) {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await send('Input.dispatchMouseEvent', { type, x: flyGeo.x, y: flyGeo.y, button: 'left', buttons: type === 'mousePressed' ? 1 : 0, clickCount: 1 });
    }
  }
  // The ease is 900 ms of frames; a slow headless frame rate stretches
  // it, so wait for the camera to settle rather than a fixed time.
  await sleep(300);
  await settle(10000);
  const flownTo = await json('window.gpuiMap.map_camera_json()');
  // Off the panel again, so the hover gate below starts from no state.
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 400, y: 400, buttons: 0 });
  const flyTo = {
    rows: flyGeo?.rows ?? 0,
    nofly_rows: flyGeo?.nofly ?? null,
    title: flyGeo?.title ?? null,
    from: flyGeo?.camera ?? null,
    to: flownTo,
  };
  // California: its centre is near -119.4, 37.2 (lon, lat) at z~5.
  flyTo.pass = flyTo.rows >= 52 && flyTo.nofly_rows === 0 && !!flownTo
    && Math.abs(flownTo.lon - -119.4) < 3 && Math.abs(flownTo.lat - 37.2) < 3 && flownTo.zoom > flyGeo.camera.zoom;
  console.log(JSON.stringify({ fly_to: flyTo }));

  const rows = [];
  for (const c of CHART_CAMERAS) {
    const [lon, lat, zoom] = c.camera;
    await evaluate(`window.gpuiMap.map_jump_to(${lon}, ${lat}, ${zoom}, 0, 0)`);
    await settle(10000);
    // The first call after a settle is the recount: the histogram and the
    // In view breakdown share one walk, so time them together.
    // Cold: the page's own settle handler has already recounted, so the
    // cache is dropped first -- this times the in-view walk plus
    // Breakdowns::in_view, not JSON over a cached count.
    const together = await evaluate(`(() => {
      const g = window.gpuiMap;
      g.map_census_invalidate_in_view();
      const t = performance.now();
      g.map_census_histograms_json();
      const iv = JSON.parse(g.map_census_in_view_json());
      const ms = performance.now() - t;
      const st = JSON.parse(g.map_census_status_json());
      return { ms, recount_ms: st?.recount_ms ?? null, recount_walk_ms: st?.recount_walk_ms ?? null, states: iv?.states?.length ?? 0, metros: iv?.metros?.length ?? 0, counting: iv?.counting ?? null };
    })()`);
    const times = await evaluate(`(() => {
      const g = window.gpuiMap, out = [];
      for (let i = 0; i < 5; i++) {
        const t = performance.now();
        g.map_census_histograms_json();
        out.push(performance.now() - t);
      }
      return out;
    })()`);
    const j = await json('window.gpuiMap.map_census_histograms_json()');
    const sorted = [...times].sort((a, b) => a - b);
    const row = {
      camera: c.name,
      calls: times.length,
      mean_ms: times.reduce((a, b) => a + b, 0) / times.length,
      p95_ms: sorted[Math.floor(0.95 * sorted.length)],
      max_ms: sorted[sorted.length - 1],
      bins_len: j?.bins?.length,
      // `in_view` must be non-null by the time this settles at a real
      // camera (every state has long since landed): a regression that
      // left it stuck null -- the "moving" gate wedged, or the cache-key
      // bug this histogram work found and fixed reappearing -- must fail
      // this gate, not just show up as a fast, empty row.
      in_view_present: j?.in_view != null,
      in_view_len: j?.in_view?.length ?? 0,
      in_view_total: j?.in_view ? j.in_view.reduce((a, b) => a + b, 0) : null,
      hist_plus_in_view_ms: together.ms,
      recount_ms: together.recount_ms,
      recount_walk_ms: together.recount_walk_ms,
      in_view_states: together.states,
      in_view_metros: together.metros,
      in_view_counting: together.counting,
    };
    rows.push(row);
    console.log(JSON.stringify(row));
  }
  const histPass = rows.every((row) => row.max_ms <= 30 && row.in_view_present && row.in_view_total > 0);
  const inViewPass = rows.every((row) => row.hist_plus_in_view_ms <= 30 && row.recount_ms != null && row.recount_ms <= 30
    && row.in_view_counting === false && row.in_view_states > 0);
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-charts.png`, Buffer.from(shot.result.data, 'base64'));

  // The panel during a drag: back at the national view, a 40-stop band
  // dragged over 3 s, the States and Metros JSON built once per rAF (what
  // the open tab redraws from on every slider move), each timed.
  await evaluate('window.gpuiMap.map_jump_to(-98.35, 39.5, 3.7, 0, 0)');
  await settle(10000);
  const drag = await evaluate(`new Promise((done) => {
    const g = window.gpuiMap;
    let s = JSON.parse(g.map_census_set_range(20, 60));
    const mid0 = (s.lo_frac + s.hi_frac) / 2, far = 1 - (s.hi_frac - s.lo_frac) / 2;
    g.map_census_slider_grab(mid0, 300);
    const ms = [], sizes = new Set();
    let minStates = Infinity, minMetros = Infinity, shareMoved = false, firstShare = null;
    const t0 = performance.now();
    // A throw inside a rAF callback would leave this promise pending
    // forever: every step reports it instead.
    const step = (now) => { try { stepBody(now); } catch (e) { done({ ms, minStates, minMetros, shareMoved, error: String(e) }); } };
    const stepBody = (now) => {
      const t = (now - t0) / 3000;
      if (t >= 1) {
        g.map_census_slider_release();
        g.map_census_set_range(0, 249);
        if (window.censusSliderRefresh) window.censusSliderRefresh();
        done({ ms, minStates, minMetros, shareMoved });
        return;
      }
      g.map_census_slider_drag(mid0 + (far - mid0) * (t < 0.5 ? t * 2 : 2 - t * 2));
      const a = performance.now();
      const st = JSON.parse(g.map_census_states_json(1));
      const me = JSON.parse(g.map_census_metros_json());
      ms.push(performance.now() - a);
      minStates = Math.min(minStates, st?.rows?.length ?? 0);
      minMetros = Math.min(minMetros, me?.metros?.length ?? 0);
      const share = st?.rows?.[0]?.share;
      if (firstShare === null) firstShare = share; else if (share !== firstShare) shareMoved = true;
      requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  })`);
  const pct = (xs, p) => {
    const v = [...xs].sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.floor(p * v.length))] : null;
  };
  const panelDrag = {
    calls: drag.ms.length,
    p95_ms: pct(drag.ms, 0.95),
    max_ms: pct(drag.ms, 1),
    min_states: drag.minStates,
    min_metros: drag.minMetros,
    share_moved: drag.shareMoved,
    error: drag.error ?? null,
  };
  panelDrag.pass = !panelDrag.error && panelDrag.p95_ms <= 3 && panelDrag.min_states >= 52 && panelDrag.min_metros >= 25 && panelDrag.share_moved;
  console.log(JSON.stringify({ panel_drag: panelDrag }));

  // A real pointer over the States tab: three rows, then off the table.
  // Every source repaints table-only; the last move restores them all.
  await sleep(500);
  const framesBefore = (await status())?.frames_drawn;
  const probe = await json(`(() => {
    const p = document.getElementById('census-panel');
    const rows = [...p.querySelectorAll('tbody tr')].slice(0, 3).map((tr) => {
      const r = tr.getBoundingClientRect();
      return { fips: Number(tr.dataset.fips), x: r.left + r.width / 2, y: r.top + r.height / 2 };
    });
    const pr = p.getBoundingClientRect();
    return JSON.stringify({ rows, off: { x: pr.left - 40, y: pr.top + 40 }, open: !p.classList.contains('closed') });
  })()`);
  // The page's own calls, summed in Rust (map_census_dim_json), before and after.
  const dimBefore = await json('window.gpuiMap.map_census_dim_json()');
  const pointedSeen = [];
  const move = (x, y) => send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, buttons: 0 });
  for (const row of probe?.rows ?? []) {
    await move(row.x, row.y);
    await sleep(150);
    pointedSeen.push((await json('window.gpuiMap.map_census_dim_json()'))?.pointed ?? null);
  }
  const shotHover = await send('Page.captureScreenshot', { format: 'png' });
  if (shotHover.result?.data) fs.writeFileSync(`${out}-panel-states.png`, Buffer.from(shotHover.result.data, 'base64'));
  if (probe) await move(probe.off.x, probe.off.y);
  await sleep(300);
  const dimAfter = await json('window.gpuiMap.map_census_dim_json()');
  const d = (k) => (dimAfter?.[k] ?? 0) - (dimBefore?.[k] ?? 0);
  const hover = {
    panel_open: probe?.open ?? false,
    rows_hovered: probe?.rows?.length ?? 0,
    row_fips: (probe?.rows ?? []).map((r) => r.fips),
    pointed_seen: pointedSeen,
    pointed_after: dimAfter?.pointed ?? null,
    dim_changes: d('changes'),
    table_only: d('table_only'),
    rebake: d('rebake'),
    replan: d('replan'),
    frames: ((await status())?.frames_drawn ?? 0) - (framesBefore ?? 0),
  };
  hover.pass = hover.rows_hovered === 3 && hover.dim_changes === 4 && hover.pointed_after === null
    && hover.pointed_seen.every((p, i) => p === hover.row_fips[i])
    && hover.table_only > 0 && hover.rebake === 0 && hover.replan === 0;
  console.log(JSON.stringify({ panel_hover: hover }));

  // The real show() path: a pointer drag on the slider's track with the
  // States tab open -- what a user's drag costs the page per move (the
  // slider, the legend's histogram, the panel's in-place update). The page
  // times each show() itself (window.censusShowMs).
  await evaluate('window.gpuiMap.map_census_set_range(20, 60); window.censusSliderRefresh()');
  await sleep(300);
  const trackGeo = await json(`(() => {
    const t = document.querySelector('#census-legend .track').getBoundingClientRect();
    const s = JSON.parse(window.gpuiMap.map_census_slider_json());
    window.censusShowMs.length = 0;
    return JSON.stringify({ left: t.left, top: t.top, width: t.width, height: t.height, s });
  })()`);
  const revBefore = await evaluate('window.gpuiMap.map_census_range_rev()');
  {
    const y = trackGeo.top + trackGeo.height / 2;
    const x0 = trackGeo.left + trackGeo.width * (trackGeo.s.lo_frac + trackGeo.s.hi_frac) / 2;
    const span = trackGeo.width * (1 - trackGeo.s.hi_frac) - 2;
    const m = (type, x) => send('Input.dispatchMouseEvent', { type, x, y, button: 'left', buttons: type === 'mouseReleased' ? 0 : 1, clickCount: 1 });
    await m('mouseMoved', x0);
    await m('mousePressed', x0);
    const STEPS = 90;
    for (let i = 1; i <= STEPS; i++) {
      const k = i <= STEPS / 2 ? i / (STEPS / 2) : 2 - i / (STEPS / 2);
      await m('mouseMoved', x0 + span * k);
      await sleep(16);
    }
    await m('mouseReleased', x0);
  }
  await sleep(300);
  const showMs = await json('JSON.stringify(window.censusShowMs)');
  const revAfter = await evaluate('window.gpuiMap.map_census_range_rev()');
  const showDrag = {
    shows: showMs?.length ?? 0,
    range_changes: revAfter - revBefore,
    p95_ms: pct(showMs ?? [], 0.95),
    max_ms: pct(showMs ?? [], 1),
    mean_ms: showMs?.length ? showMs.reduce((a, b) => a + b, 0) / showMs.length : null,
  };
  showDrag.pass = showDrag.shows >= 45 && showDrag.range_changes > 0 && showDrag.p95_ms <= 8;
  console.log(JSON.stringify({ show_drag: showDrag }));
  const shotDrag = await send('Page.captureScreenshot', { format: 'png' });
  if (shotDrag.result?.data) fs.writeFileSync(`${out}-panel-states-range.png`, Buffer.from(shotDrag.result.data, 'base64'));

  // The other two tabs: their screenshots, and the Metros canvas must
  // hold 25 ridges and real pixels.
  const tab = (label) => evaluate(`[...document.querySelectorAll('#census-panel .tabs button')].find((b) => b.textContent === ${JSON.stringify(label)}).click()`);
  await tab('Metros');
  await sleep(400);
  const metrosDom = await json(`(() => {
    const c = document.querySelector('#census-panel canvas.ridges');
    if (!c) return JSON.stringify({ present: false });
    const px = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    let inked = 0;
    for (let i = 3; i < px.length; i += 4) if (px[i] > 0) inked++;
    return JSON.stringify({ present: true, ridges: Number(c.dataset.ridges), height: c.getBoundingClientRect().height, inked });
  })()`);
  metrosDom.pass = metrosDom.present && metrosDom.ridges === 25 && metrosDom.height > 0 && metrosDom.inked > 1000;
  console.log(JSON.stringify({ metros_dom: metrosDom }));
  const shM = await send('Page.captureScreenshot', { format: 'png' });
  if (shM.result?.data) fs.writeFileSync(`${out}-panel-metros.png`, Buffer.from(shM.result.data, 'base64'));
  await tab('In view');
  await sleep(400);
  const shI = await send('Page.captureScreenshot', { format: 'png' });
  if (shI.result?.data) fs.writeFileSync(`${out}-panel-inview.png`, Buffer.from(shI.result.data, 'base64'));
  const inViewDom = await json(`JSON.stringify({ rows: document.querySelectorAll('#census-panel .iv-row').length, head: document.querySelector('#census-panel .iv-head')?.textContent ?? null })`);
  console.log(JSON.stringify({ in_view_dom: inViewDom }));

  const pass = flyTo.pass && histPass && inViewPass && panelDrag.pass && hover.pass && showDrag.pass && metrosDom.pass
    && (inViewDom?.rows ?? 0) > 0 && errors.length === 0;
  report.charts = { fly_to: flyTo, rows, hist_pass: histPass, in_view_pass: inViewPass, panel_drag: panelDrag, panel_hover: hover, show_drag: showDrag,
    metros_dom: metrosDom, in_view_dom: inViewDom, errors: [...errors], pass, all_drawn_ms: r.st?.all_drawn_ms };
  console.log(JSON.stringify({ errors }));
  console.log(JSON.stringify({ charts_pass: pass }));
}

if (modes.has('drag')) {
  // MG-C8: a 40-stop band dragged across the track and back over 6 s, one
  // map_census_slider_drag per rAF, as a pointer would arrive. Each export
  // is timed in the page; the rAF intervals are the frames; the overlay
  // update times (map_census_take_update_ms) are what the frame loop paid,
  // the full paint check included.
  const r = await loadPage(`${base}?${query}`);
  const settled = await settle();
  const before = await json('window.gpuiMap.map_census_take_update_ms()');
  await json('window.gpuiMap.map_frame_samples_json()');
  const framesBefore = (await status())?.frames_drawn;
  const res = await evaluateAnimation('drag', `new Promise((done) => {
    const g = window.gpuiMap;
    const width = document.querySelector('#census-legend .track').getBoundingClientRect().width;
    let s = JSON.parse(g.map_census_set_range(20, 60));
    const mid0 = (s.lo_frac + s.hi_frac) / 2, half = (s.hi_frac - s.lo_frac) / 2;
    const far = 1 - half;
    s = JSON.parse(g.map_census_slider_grab(mid0, width));
    const pathsBefore = s.paths;
    const exportMs = [], intervals = [], ranges = new Set();
    const t0 = performance.now();
    let last = t0;
    const step = (now) => {
      intervals.push(now - last);
      last = now;
      const t = (now - t0) / 6000;
      if (t >= 1) {
        const end = JSON.parse(g.map_census_slider_release());
        if (window.censusSliderRefresh) window.censusSliderRefresh();
        done({ exportMs, intervals: intervals.slice(1), ranges: ranges.size, pathsBefore, pathsAfter: end.paths, label: end.label });
        return;
      }
      const k = t < 0.5 ? t * 2 : 2 - t * 2;
      const frac = mid0 + (far - mid0) * k;
      const a = performance.now();
      const js = g.map_census_slider_drag(frac);
      exportMs.push(performance.now() - a);
      const st = JSON.parse(js);
      ranges.add(st.lo);
      requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  })`);
  const framesAfter = (await status())?.frames_drawn;
  const during = await json('window.gpuiMap.map_census_take_update_ms()');
  const samples = await json('window.gpuiMap.map_frame_samples_json()');
  await sleep(2000);
  const after = await json('window.gpuiMap.map_census_take_update_ms()');
  const pct = (xs, p) => {
    const v = [...xs].sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.floor(p * v.length))] : null;
  };
  const d = (k) => res.pathsAfter[k] - res.pathsBefore[k];
  const row = {
    mode: 'drag',
    query,
    visibility: await evaluate('document.visibilityState'),
    all_drawn_ms: r.st?.all_drawn_ms,
    settled,
    moves: res.exportMs.length,
    distinct_lo: res.ranges,
    export_p95_ms: pct(res.exportMs, 0.95),
    export_max_ms: pct(res.exportMs, 1),
    frame_p95_ms: pct(res.intervals, 0.95),
    frame_max_ms: pct(res.intervals, 1),
    update_p95_ms: pct(during?.update_ms ?? [], 0.95),
    update_max_ms: pct(during?.update_ms ?? [], 1),
    cpu_p95_ms: pct(samples?.cpu_ms ?? [], 0.95),
    cpu_max_ms: pct(samples?.cpu_ms ?? [], 1),
    frames_drawn: framesAfter - framesBefore,
    paint_checks_during: during.paint_checks - before.paint_checks,
    paint_checks_after: after.paint_checks - during.paint_checks,
    after_update_max_ms: pct(after?.update_ms ?? [], 1),
    epochs_moved: after.epochs - before.epochs,
    table_only: d('table_only'),
    rebake: d('rebake'),
    replan: d('replan'),
    label: res.label,
    backend: logs.find((l) => l.startsWith('gpu: ')) ?? null,
  };
  row.errors = [...errors];
  row.pass = row.rebake === 0 && row.replan === 0 && row.epochs_moved === 0 && row.export_p95_ms <= 4 && errors.length === 0;
  report.drag = row;
  console.log(JSON.stringify(row));
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-drag.png`, Buffer.from(shot.result.data, 'base64'));

  // The DOM's own wiring, with real input: press the band's middle, move
  // 40 px right in 8 steps, release. The knobs, band and caption must
  // follow, both handles moving by the same stops.
  const geo = await json(`(() => {
    const t = document.querySelector('#census-legend .track').getBoundingClientRect();
    const s = JSON.parse(window.gpuiMap.map_census_slider_json());
    return JSON.stringify({ left: t.left, top: t.top, width: t.width, height: t.height, s });
  })()`);
  const y = geo.top + geo.height / 2;
  const x0 = geo.left + geo.width * (geo.s.lo_frac + geo.s.hi_frac) / 2;
  const mouse = (type, x) => send('Input.dispatchMouseEvent', { type, x, y, button: 'left', buttons: type === 'mouseReleased' ? 0 : 1, clickCount: 1 });
  await mouse('mouseMoved', x0);
  await mouse('mousePressed', x0);
  for (let i = 1; i <= 8; i++) {
    await mouse('mouseMoved', x0 + i * 5);
    await sleep(20);
  }
  await mouse('mouseReleased', x0 + 40);
  await sleep(500);
  const dom = await json(`(() => {
    const q = (s) => document.querySelector('#census-legend ' + s);
    const s = JSON.parse(window.gpuiMap.map_census_slider_json());
    return JSON.stringify({ s, lo_left: q('.knob').style.left, band_width: q('.band').style.width, caption: q('.caption').textContent });
  })()`);
  const pointer = {
    before: { lo: geo.s.lo, hi: geo.s.hi },
    after: { lo: dom.s.lo, hi: dom.s.hi },
    moved_together: dom.s.lo - geo.s.lo === dom.s.hi - geo.s.hi && dom.s.lo > geo.s.lo,
    dom_follows: Math.abs(parseFloat(dom.lo_left) - dom.s.lo_frac * 100) < 1e-3 && dom.caption === dom.s.label,
    caption: dom.caption,
  };
  report.drag.pointer = pointer;
  console.log(JSON.stringify({ pointer }));
  const shot2 = await send('Page.captureScreenshot', { format: 'png' });
  if (shot2.result?.data) fs.writeFileSync(`${out}-pointer.png`, Buffer.from(shot2.result.data, 'base64'));
}

if (modes.has('pan')) {
  const CAMERA = [-98.35, 39.5, 3.7];
  const r = await loadPage(`${base}?${query}&camera=${CAMERA.join(',')}`);
  const settled = await settle();
  await json('window.gpuiMap.map_frame_samples_json()');
  const framesBefore = (await status())?.frames_drawn;
  const res = await evaluateAnimation('pan', `new Promise((done) => {
    const g = window.gpuiMap, [lon0, lat0, zoom] = ${JSON.stringify(CAMERA)};
    const intervals = [];
    let t0 = null, last = null;
    const step = (now) => {
      try {
        if (t0 === null) { t0 = now; last = now; }
        else { intervals.push(now - last); last = now; }
        const t = (now - t0) / 6000;
        if (t >= 1) {
          g.map_jump_to(lon0, lat0, zoom, 0, 0);
          done({ intervals });
          return;
        }
        const a = 2 * Math.PI * t;
        g.map_jump_to(lon0 + 8 * Math.sin(a), lat0 + 4 * (1 - Math.cos(a)), zoom, 0, 0);
        requestAnimationFrame(step);
      } catch (e) { done({ intervals, error: String(e) }); }
    };
    requestAnimationFrame(step);
  })`);
  const framesAfter = (await status())?.frames_drawn;
  const samples = await json('window.gpuiMap.map_frame_samples_json()');
  const pct = (xs, p) => {
    const v = [...xs].sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.floor(p * v.length))] : null;
  };
  const gl = /(^|&)gl=1(&|$)/.test(query);
  const row = {
    mode: 'pan',
    query,
    backend: logs.find((l) => l.startsWith('gpu: ')) ?? null,
    visibility: await evaluate('document.visibilityState'),
    all_drawn_ms: r.st?.all_drawn_ms,
    settled,
    rafs: res.intervals.length,
    frame_p50_ms: pct(res.intervals, 0.5),
    frame_p95_ms: pct(res.intervals, 0.95),
    frame_max_ms: pct(res.intervals, 1),
    cpu_p50_ms: pct(samples?.cpu_ms ?? [], 0.5),
    cpu_p95_ms: pct(samples?.cpu_ms ?? [], 0.95),
    cpu_max_ms: pct(samples?.cpu_ms ?? [], 1),
    frames_drawn: (framesAfter ?? 0) - (framesBefore ?? 0),
    bound_p95_ms: gl ? 50 : 33,
    error: res.error ?? null,
    errors: [...errors],
  };
  row.pass = !row.error && errors.length === 0 && row.frames_drawn > 0 && row.frame_p95_ms <= row.bound_p95_ms;
  report.pan = row;
  console.log(JSON.stringify(row));
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-pan.png`, Buffer.from(shot.result.data, 'base64'));
}

if (modes.has('capture')) {
  const CAMERA = '-98.35,39.5,3.7';
  // CENSUS_CAPTURE_DPR=2 matches a Retina native `--snapshot` pixel for pixel.
  const dpr = Number(process.env.CENSUS_CAPTURE_DPR ?? 1);
  await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 800, deviceScaleFactor: dpr, mobile: false });
  // Mapbox is off by default: only with GPUI_MAP_ALLOW_MAPBOX=1 (and a build made with it).
  const token = process.env.GPUI_MAP_ALLOW_MAPBOX === '1' ? (process.env.MAPBOX_ACCESS_TOKEN ?? '') : '';
  // Appended here, never echoed: `row.query` below is the query without it.
  const tokenPart = token ? `&access_token=${encodeURIComponent(token)}` : '';
  const r = await loadPage(`${base}?${query}&census_ui=0&camera=${CAMERA}${tokenPart}`);
  const settled = await settle(30000);
  // The basemap's last tiles can land after the overlay settles.
  await sleep(2000);
  await settle(10000);
  const geo = await json(`JSON.stringify({ w: innerWidth, h: innerHeight, dpr: devicePixelRatio,
    ui: [...document.querySelectorAll('#census-legend, #census-panel, #hud, #credit, #controls')]
      .filter((e) => getComputedStyle(e).display !== 'none').map((e) => e.id),
    camera: JSON.parse(window.gpuiMap.map_camera_json()) })`);
  const ov = await json('window.gpuiMap.map_overlay_stats_json()');
  const row = {
    mode: 'capture',
    query,
    token_passed: !!token,
    backend: logs.find((l) => l.startsWith('gpu: ')) ?? null,
    visibility: await evaluate('document.visibilityState'),
    loaded: r.st?.loaded,
    total: r.st?.total,
    all_drawn_ms: r.st?.all_drawn_ms,
    settled,
    viewport: geo ? { w: geo.w, h: geo.h, dpr: geo.dpr } : null,
    ui_visible: geo?.ui ?? null,
    camera: geo?.camera ?? null,
    tile_draws: ov?.tile_draws ?? null,
    errors: [...errors].map((e) => (token ? e.split(token).join('<token>') : e)),
  };
  row.pass = row.loaded === row.total && row.total > 0 && (row.ui_visible?.length ?? 1) === 0 && (row.tile_draws ?? 0) > 0;
  report.capture = row;
  console.log(JSON.stringify(row));
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-capture.png`, Buffer.from(shot.result.data, 'base64'));
}

fs.writeFileSync(`${out}.json`, JSON.stringify(report, null, 1));
ws.close();
