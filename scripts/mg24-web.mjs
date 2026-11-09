// MG-24 (world dot-density plan, Task 16): the browser's dot frame times,
// time to first dots and wasm heap, at the native presets, over CDP.
//
//   1. Serve the build: `web/build.sh` (with MAPBOX_ACCESS_TOKEN set for
//      Mapbox Streets) and `python3 web/serve.py 8097` (it serves the dot
//      archives at /grid/ and ClickHouse at /clickhouse/).
//   2. Start Chrome with a debugging port, e.g.
//        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
//          --remote-debugging-port=9337 --user-data-dir=/tmp/mg24-profile \
//          --window-size=1280,800 --no-first-run about:blank
//      (add --headless=new for a headless run). Keep the window in front:
//      a background tab throttles rAF.
//   3. node scripts/mg24-web.mjs 9337 http://127.0.0.1:8097/ \
//        'gl=1&grid=./grid/E2025_30ss.pmtiles' /tmp/mg24-gl
//      Drop `gl=1` for WebGPU; `clickhouse=ghs_cells_30ss` for the proxy;
//      add `&dot_budget=1000000` for the 1.0 M fallback. Writes
//      <out>.json (one row per preset), <out>-<preset>.png and .log.
//
// Per preset: a fresh page load, wait until the dot layer holds every
// wanted tile (first_dots_ms is from navigation, the page's own clock),
// then a 6 s circular pan (one map_jump_to per rAF) whose frame intervals
// (map_frame_samples_json) give p50/p95/max. The heap is
// map_memory_json's wasm_heap_bytes after the pan.
//
// MG24_PRESETS=race (race plan, MG-R15) runs the race dots' US views
// instead, with `grid=./grid/race-dots-2020.pmtiles`; nyc_z13 is where
// part tiles draw up to 2x the budget. Any other value is refused.
import fs from 'node:fs';

const [port, base, query, out] = process.argv.slice(2);
const presetSet = process.env.MG24_PRESETS ?? 'world';
if (!['world', 'race'].includes(presetSet)) {
  console.error(`MG24_PRESETS=${presetSet}: expected world or race`);
  process.exit(2);
}
const PRESETS = presetSet === 'race'
  ? [
      { name: 'us_z4', lon: -98.58, lat: 39.83, zoom: 4.0 },
      { name: 'nyc_z8', lon: -73.95, lat: 40.72, zoom: 8.0 },
      { name: 'nyc_z10', lon: -73.95, lat: 40.72, zoom: 10.0 },
      { name: 'nyc_z13', lon: -73.95, lat: 40.72, zoom: 13.0 },
    ]
  : [
      { name: 'world_z1', lon: 0.0, lat: 20.0, zoom: 1.0 },
      { name: 'india_z5', lon: 78.9, lat: 22.0, zoom: 5.0 },
      { name: 'dhaka_z10', lon: 90.4125, lat: 23.8103, zoom: 10.0 },
      { name: 'dhaka_z15', lon: 90.4125, lat: 23.8103, zoom: 15.0 },
    ];
const SETTLE_MS = 90000;
const PAN_MS = 6000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const page = targets.find((t) => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const logs = [];
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  } else if (msg.method === 'Runtime.consoleAPICalled') {
    logs.push(msg.params.args.map((a) => a.value ?? a.description).join(' '));
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
await send('Runtime.enable');
await send('Page.enable');

const results = [];
for (const p of PRESETS) {
  const url = `${base}?${query}&lon=${p.lon}&lat=${p.lat}&z=${p.zoom}`;
  logs.length = 0;
  await send('Page.navigate', { url });
  const t0 = Date.now();
  let stats = null;
  let ready = false;
  const dotsAsked = /grid=|clickhouse=/.test(query);
  if (!dotsAsked) await sleep(8000);
  while (dotsAsked && Date.now() - t0 < SETTLE_MS) {
    await sleep(500);
    try {
      stats = JSON.parse((await evaluate('window.gpuiMap ? window.gpuiMap.map_dot_stats_json() : "null"')) ?? 'null');
    } catch (e) {
      stats = null;
    }
    if (stats && stats.first_dots_ms && stats.tiles_ready >= stats.tiles_wanted && stats.tiles_wanted > 0) {
      await sleep(1500);
      stats = JSON.parse(await evaluate('window.gpuiMap.map_dot_stats_json()'));
      if (stats.tiles_ready >= stats.tiles_wanted) {
        ready = true;
        break;
      }
    }
  }
  const visibility = await evaluate('document.visibilityState');
  // Pan: a small circle around the preset centre, one jump per rAF.
  await evaluate('window.gpuiMap.map_frame_samples_json()');
  const pan = await evaluate(`new Promise((done) => {
    const g = window.gpuiMap, t0 = performance.now(), r = ${Math.max(0.02, 3 / Math.pow(2, p.zoom))};
    let frames = 0;
    const step = () => {
      const t = (performance.now() - t0) / ${PAN_MS};
      if (t >= 1) { done(frames); return; }
      g.map_jump_to(${p.lon} + r * Math.cos(t * 2 * Math.PI), ${p.lat} + r * Math.sin(t * 2 * Math.PI), ${p.zoom}, 0, 0);
      frames++;
      requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  })`);
  const samples = JSON.parse(await evaluate('window.gpuiMap.map_frame_samples_json()'));
  const after = JSON.parse(await evaluate('window.gpuiMap.map_dot_stats_json()'));
  const memory = JSON.parse(await evaluate('window.gpuiMap.map_memory_json()'));
  const iv = (samples?.interval_ms ?? []).slice(5).sort((a, b) => a - b);
  const q = (f) => (iv.length ? iv[Math.min(iv.length - 1, Math.floor(f * iv.length))] : null);
  const drawn = (samples?.drawn ?? []).filter((d) => d > 0).length;
  const cpu = (samples?.cpu_ms ?? []).slice(5).sort((a, b) => a - b);
  const qc = (f) => (cpu.length ? cpu[Math.min(cpu.length - 1, Math.floor(f * cpu.length))] : null);
  const row = {
    preset: p.name,
    ready,
    visibility,
    settle_s: (Date.now() - t0) / 1000,
    first_dots_ms: stats?.first_dots_ms,
    asked_ms: stats?.asked_ms,
    dots_settled: stats?.dots_drawn,
    ppd: stats?.ppd,
    level: stats?.level,
    tiles: `${stats?.tiles_ready}/${stats?.tiles_wanted}`,
    errors: stats?.source_errors,
    last_error: stats?.last_error,
    pan_frames: pan,
    samples: iv.length,
    frames_drawn: drawn,
    interval_p50: q(0.5),
    interval_p95: q(0.95),
    interval_max: iv.length ? iv[iv.length - 1] : null,
    over_33: iv.filter((v) => v > 33).length,
    cpu_p50: qc(0.5),
    cpu_p95: qc(0.95),
    fps_mean: iv.length ? 1000 / (iv.reduce((a, b) => a + b, 0) / iv.length) : null,
    backend: logs.find((l) => l.startsWith('gpu: ')) ?? null,
    dots_during_pan_end: after?.dots_drawn,
    wasm_heap_mb: memory ? memory.wasm_heap_bytes / 1048576 : null,
    js_heap_mb: memory ? memory.js_heap_bytes / 1048576 : null,
    gpu_buffer_mb: memory ? memory.gpu_buffer_bytes_live / 1048576 : null,
    dot_gpu_mb: after ? after.gpu_bytes / 1048576 : null,
  };
  results.push(row);
  console.log(JSON.stringify(row));
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) fs.writeFileSync(`${out}-${p.name}.png`, Buffer.from(shot.result.data, 'base64'));
  fs.writeFileSync(`${out}-${p.name}.log`, logs.join('\n'));
}
fs.writeFileSync(`${out}.json`, JSON.stringify(results, null, 1));
ws.close();
