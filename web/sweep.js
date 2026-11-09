// A scripted zoom sweep, and the readout around it.
//
// This exists so "tiles pop on zoom" becomes a number instead of an
// impression. It drives the camera through a FIXED zoom path from a FIXED
// centre at a FIXED viewport, so the tile set the sweep asks for is
// determined entirely by the path -- deterministic in what it LOADS, not
// merely in where it ends up. Nothing here touches the map's own input
// handlers; `map_set_camera` places the camera directly, so there is no
// anchoring, no easing and no wheel-delta rounding between the arms.
//
// Usage, from the page or from CDP:
//
//     await runSweep()                 // one sweep on whatever ?mode= loaded
//     window.__sweep                   // the last result object
//
// The retention arm is chosen by the URL (`?mode=0|1|2|3`) and NOT switched
// mid-run: switching in place would leave the previous arm's tiles resident,
// which is exactly the thing being measured.

/// Singapore, Marina Bay -- the app's own start position, restated so a sweep
/// after a manual pan still measures the same ground.
const CENTRE = { lon: 103.8555, lat: 1.2895 };

/// The path. Down to z11 and up to z17 around a z13 start, in quarter-level
/// steps, with a dwell at each reversal.
///
/// A quarter level per step because a whole level per step would cross every
/// integer boundary at a different phase in each arm; a quarter divides evenly
/// into 1 so every arm crosses the same boundaries at the same step index.
/// The dwell at the reversals is the "quiet moment" a real user gives a map
/// when they arrive somewhere and look at it -- it is where a proactive
/// prefetch is supposed to pay for itself, and leaving it out would measure
/// only the case that cannot help it.
const STEP = 0.25;
const FRAMES_PER_STEP = 6;
const DWELL_FRAMES = 40;

function zoomPath() {
    const path = [];
    const leg = (from, to) => {
        const dir = Math.sign(to - from);
        for (let z = from + dir * STEP; dir > 0 ? z <= to + 1e-9 : z >= to - 1e-9; z += dir * STEP) {
            path.push({ zoom: Math.round(z * 100) / 100, frames: FRAMES_PER_STEP });
        }
        path[path.length - 1].frames = DWELL_FRAMES;
    };
    leg(13, 17);
    leg(17, 11);
    leg(11, 13);
    return path;
}

const raf = () => new Promise(requestAnimationFrame);

async function waitFrames(n) {
    for (let i = 0; i < n; i++) await raf();
}

/// Waits until every cover tile is drawing its own mesh at the current zoom
/// bucket, or until `maxFrames` have passed. Waiting on the map's own settled
/// predicate rather than on a wall clock means a slow network lengthens the
/// run instead of silently corrupting it.
async function waitSettled(maxFrames = 900) {
    for (let i = 0; i < maxFrames; i++) {
        if (window.gpuiMap.map_is_settled()) return i;
        await raf();
    }
    return -1;
}

const metrics = () => JSON.parse(window.gpuiMap.map_metrics_json());

function delta(a, b) {
    const out = {};
    for (const k of Object.keys(b)) out[k] = k === 'mode' ? b[k] : b[k] - a[k];
    return out;
}

export async function runSweep() {
    const path = zoomPath();
    // Settle at the start position first, so every arm begins the sweep with
    // the same z13 cover resident and the sweep's own counters are not
    // charged with the page-load fill. The `before` snapshot is taken after
    // that settle; the reported numbers are the delta across the sweep, and
    // the page-load fill is reported separately as `settle`.
    window.gpuiMap.map_set_camera(CENTRE.lon, CENTRE.lat, 13);
    const settleFrames = await waitSettled();
    const before = metrics();
    const canvasAtStart = (() => {
        const c = document.getElementById('map');
        return [c.clientWidth, c.clientHeight, devicePixelRatio];
    })();

    const t0 = performance.now();
    for (const step of path) {
        window.gpuiMap.map_set_camera(CENTRE.lon, CENTRE.lat, step.zoom);
        await waitFrames(step.frames);
    }
    const wallMs = performance.now() - t0;
    const after = metrics();

    const sweep = delta(before, after);
    const canvas = document.getElementById('map');
    const result = {
        mode: after.mode,
        // Recorded, not assumed: the cover -- and therefore every count below
        // -- scales with the viewport, so two arms measured at different
        // window sizes are not comparable.
        viewport: [canvas.clientWidth, canvas.clientHeight, devicePixelRatio],
        // Asserted, not assumed. The canvas is a flex child; anything that
        // resizes it mid-sweep changes the cover and quietly makes two arms
        // incomparable. This caught exactly that once already.
        viewport_stable:
            canvasAtStart.join() ===
            [canvas.clientWidth, canvas.clientHeight, devicePixelRatio].join(),
        steps: path.length,
        wall_ms: Math.round(wallMs),
        settle_frames: settleFrames,
        settle: before,
        sweep,
        // The two headline numbers, named the way the report names them.
        holes_per_sweep: sweep.holes,
        hole_frame_pct: sweep.frames ? (100 * sweep.hole_frames) / sweep.frames : 0,
        requests_total: sweep.requests_cover + sweep.requests_prefetch,
        bytes_total: sweep.bytes_cover + sweep.bytes_prefetch,
    };
    window.__sweep = result;
    return result;
}

window.runSweep = runSweep;
