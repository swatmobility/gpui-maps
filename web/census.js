// The census demo's DOM half. No logic lives here: every number comes from a
// map_census_* export over tested src/census code (see the plan's Global Constraints).
// The popup is built with textContent, never innerHTML: its strings come from data files.
export function startCensus(wasm) {
  document.body.classList.add("census");
  // `?census_ui=0`: the map alone -- legend, panel, popup, HUD, credit and
  // zoom buttons hidden (index.html's `body.census-noui` rule) -- for the
  // MG-C11 parity capture against native `--snapshot`. Never a default: the
  // credit is a condition of using the data.
  if (new URLSearchParams(location.search).get("census_ui") === "0") document.body.classList.add("census-noui");
  const panel = startPanel(wasm);
  const canvas = document.getElementById("map");
  const pop = document.createElement("div");
  pop.id = "census-pop";
  pop.hidden = true;
  document.body.append(pop);
  let pending = null;
  canvas.addEventListener("pointermove", (e) => {
    if (e.buttons) { pending = null; pop.hidden = true; return; }
    pending = [e.offsetX, e.offsetY, e.clientX, e.clientY];
  });
  canvas.addEventListener("pointerleave", () => { pending = null; pop.hidden = true; });
  // A wheel or key zoom moves the map under a still pointer: the card would
  // describe what is no longer there, so it goes until the pointer moves.
  const hide = () => { pending = null; pop.hidden = true; };
  canvas.addEventListener("wheel", hide, { passive: true });
  addEventListener("keydown", hide);
  // `#rrggbb` -> `rgba(r, g, b, alpha)`: the out-of-range in-view bars keep
  // their viridis hue at map_census_histograms_json's `colors.out_alpha`.
  function withAlpha(hex, alpha) {
    const n = parseInt(hex.slice(1), 16);
    return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
  }
  // The histogram strip: the national bars behind (own peak), the in-view
  // bars in front (their own peak), each split at the slider's selection --
  // every number from map_census_histograms_json, nothing computed here but
  // pixel positions. `wasSettled` gates the poll in `tick` to once per
  // settle, not once a frame.
  let wasSettled = true;
  function drawHist() {
    const h = JSON.parse(wasm.map_census_histograms_json());
    if (!h) return;
    const dpr = window.devicePixelRatio || 1;
    const rect = hist.getBoundingClientRect();
    const w = Math.max(1, Math.round(rect.width)), hpx = Math.max(1, Math.round(rect.height));
    if (hist.width !== Math.round(w * dpr) || hist.height !== Math.round(hpx * dpr)) {
      hist.width = Math.round(w * dpr);
      hist.height = Math.round(hpx * dpr);
    }
    const ctx = hist.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, hpx);
    const s = JSON.parse(wasm.map_census_slider_json());
    const [selLo, selHi] = s ? [s.lo_frac * w, s.hi_frac * w] : [0, w];
    const bar = (x0f, x1f, barH, inside, outside, inset) => {
      if (barH <= 0) return;
      const x0 = x0f * w + inset, x1 = x1f * w - inset;
      if (x1 <= x0) return;
      const cuts = [x0, Math.min(Math.max(selLo, x0), x1), Math.min(Math.max(selHi, x0), x1), x1];
      for (let i = 0; i < 3; i++) {
        const a = cuts[i], b = cuts[i + 1];
        if (b > a) { ctx.fillStyle = i === 1 ? inside : outside; ctx.fillRect(a, hpx - barH, b - a, barH); }
      }
    };
    const overallPeak = h.max || 1;
    for (let k = 0; k < h.bins.length; k++) {
      const [a, b] = h.bin_fracs[k];
      const barH = h.bins[k] ? Math.max(1, (h.bins[k] / overallPeak) * hpx) : 0;
      bar(a, b, barH, h.colors.overall, h.colors.overall_out, 0);
    }
    if (h.in_view) {
      const inViewPeak = h.in_view_max || 1;
      for (let k = 0; k < h.in_view.length; k++) {
        const [a, b] = h.bin_fracs[k];
        const barH = h.in_view[k] ? Math.max(1, (h.in_view[k] / inViewPeak) * hpx) : 0;
        const color = h.colors.in_view[k];
        const inset = Math.min((b - a) * w * 0.2, 0.8);
        bar(a, b, barH, color, withAlpha(color, h.colors.out_alpha), inset);
      }
    }
    histCaption.textContent = h.caption;
  }
  function tick() {
    if (pending) {
      const [x, y, cx, cy] = pending;
      pending = null;
      const info = JSON.parse(wasm.map_census_hover_json(x, y));
      if (!info) { pop.hidden = true; } else {
        pop.replaceChildren();
        for (const [cls, text] of [["t", info.title], ["s", info.subtitle], ["i", info.income], ["a", info.aland], ["g", info.geoid_line]]) {
          const d = document.createElement("div"); d.className = cls; d.textContent = text; pop.append(d);
        }
        pop.hidden = false;
        const [left, top] = wasm.map_census_popup_origin(cx, cy, pop.offsetWidth, pop.offsetHeight, innerWidth, innerHeight);
        pop.style.transform = `translate(${left}px, ${top}px)`;
      }
    }
    // Cheap: one boolean read. A camera newly settled recounts the in-view
    // histogram (map_census_histograms_json's own throttle) and redraws;
    // a still-settled or still-moving frame does neither.
    const settledNow = wasm.map_is_settled();
    if (settledNow && !wasSettled) { drawHist(); panel.settled(); }
    panel.poll();
    wasSettled = settledNow;
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
  const legend = document.createElement("div");
  legend.id = "census-legend";
  const ramp = JSON.parse(wasm.map_census_ramp_json());
  const bar = document.createElement("div");
  bar.className = "bar";
  bar.style.background = `linear-gradient(to right, ${ramp.stops.join(", ")})`;
  // The histogram strip: national bars behind, in-view bars in front, both
  // drawn in `drawHist` from map_census_histograms_json -- a canvas, not one
  // element a bar (83 x 2), so a redraw is one clear-and-fill, not a diff.
  const hist = document.createElement("canvas");
  hist.className = "hist";
  const histCaption = document.createElement("div");
  histCaption.className = "hist-caption";
  const ticks = document.createElement("div");
  ticks.className = "ticks";
  for (const [label, frac] of ramp.ticks) {
    const t = document.createElement("span"); t.textContent = label; t.style.left = `${frac * 100}%`; ticks.append(t);
  }
  // The range slider under the bar: two knobs and the band between them.
  // Every position and the caption come from the map_census_slider_* exports
  // (src/census/slider.rs); this only turns the pointer into a fraction.
  const track = document.createElement("div");
  track.className = "track";
  const band = document.createElement("div");
  band.className = "band";
  const lo = document.createElement("div");
  const hi = document.createElement("div");
  lo.className = hi.className = "knob";
  track.append(band, lo, hi);
  const caption = document.createElement("div");
  caption.className = "caption";
  // What each show() costs, for the harness (scripts/census-web.mjs
  // charts): a measurement, bounded, never read by the page.
  const showMs = (window.censusShowMs = []);
  function show(json) {
    const t0 = performance.now();
    showInner(json);
    if (showMs.length < 10000) showMs.push(performance.now() - t0);
  }
  function showInner(json) {
    const s = JSON.parse(json);
    if (!s) return;
    lo.style.left = `${s.lo_frac * 100}%`;
    hi.style.left = `${s.hi_frac * 100}%`;
    band.style.left = `${s.lo_frac * 100}%`;
    band.style.width = `${(s.hi_frac - s.lo_frac) * 100}%`;
    caption.textContent = s.label;
    drawHist();
    panel.range();
  }
  const frac = (e) => { const r = track.getBoundingClientRect(); return [(e.clientX - r.left) / r.width, r.width]; };
  track.addEventListener("pointerdown", (e) => {
    track.setPointerCapture(e.pointerId);
    const [f, w] = frac(e);
    show(wasm.map_census_slider_grab(f, w));
    e.preventDefault();
  });
  track.addEventListener("pointermove", (e) => {
    if (!track.hasPointerCapture(e.pointerId)) return;
    show(wasm.map_census_slider_drag(frac(e)[0]));
  });
  const release = () => show(wasm.map_census_slider_release());
  track.addEventListener("pointerup", release);
  track.addEventListener("pointercancel", release);
  legend.append(
    Object.assign(document.createElement("div"), { className: "title", textContent: "Median household income" }),
    bar,
    track,
    hist,
    ticks,
    histCaption,
    caption,
  );
  document.body.append(legend);
  // For a driver that moved the range through the exports directly
  // (scripts/census-web.mjs drag): redraw the slider from the map's state.
  window.censusSliderRefresh = () => show(wasm.map_census_slider_json());
  // The pack's tables arrive after this runs: show the slider (and the
  // national histogram -- the in-view half fills in on the first settle)
  // once they have.
  (function wait() {
    const s = wasm.map_census_slider_json();
    if (s === "null") { setTimeout(wait, 100); return; }
    show(s);
  })();
}

// The Breakdowns panel: a collapsible card with three tabs over the
// map_census_{states,metros,in_view}_json exports (src/census/panel_json.rs).
// Every number, label, order, colour and ridge coordinate is in the JSON;
// this lays it out in pixels. Data strings (state and metro names) only ever
// reach textContent, title or a canvas's fillText, never innerHTML.
function startPanel(wasm) {
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const root = el("div");
  root.id = "census-panel";
  // Closed at first on a phone-width page, where it would cover the map.
  if (innerWidth <= 760) root.classList.add("closed");
  const head = el("div", "head");
  const toggle = el("button", "toggle");
  head.append(el("div", "title", "Breakdowns"), toggle);
  const body = el("div", "body");
  const tabs = el("div", "tabs");
  const pane = el("div", "pane");
  body.append(tabs, pane);
  root.append(head, body);
  document.body.append(root);
  const TABS = [["states", "States"], ["metros", "Metros"], ["inview", "In view"]];
  let tab = "states", sort = 0, pointed = -1, drawnKey = null;
  const tabButtons = new Map();
  for (const [key, label] of TABS) {
    const b = el("button", null, label);
    b.addEventListener("click", () => { tab = key; draw(); });
    tabs.append(b);
    tabButtons.set(key, b);
  }
  const showToggle = () => { toggle.textContent = root.classList.contains("closed") ? "+" : "–"; };
  toggle.addEventListener("click", () => { root.classList.toggle("closed"); showToggle(); draw(); });
  showToggle();
  // Hover and click are delegated to the persistent pane, so a redraw
  // never strands a listener on a replaced element.
  const point = (fips) => { if (fips !== pointed) { pointed = fips; wasm.map_census_dim_state(fips); } };
  pane.addEventListener("mouseover", (e) => {
    const tr = tab === "states" ? e.target.closest("tr[data-fips]") : null;
    point(tr ? Number(tr.dataset.fips) : -1);
  });
  pane.addEventListener("mouseleave", () => point(-1));
  pane.addEventListener("click", (e) => {
    const row = e.target.closest("[data-fly]");
    if (!row || row.classList.contains("nofly")) return;
    const code = Number(row.dataset.code);
    if (row.dataset.fly === "metro") wasm.map_census_fly_metro(code); else wasm.map_census_fly_state(code);
  });
  const dpr = () => window.devicePixelRatio || 1;
  const sizeCanvas = (c) => {
    const r = c.getBoundingClientRect();
    const w = Math.max(1, Math.round(r.width)), h = Math.max(1, Math.round(r.height));
    if (c.width !== Math.round(w * dpr()) || c.height !== Math.round(h * dpr())) {
      c.width = Math.round(w * dpr());
      c.height = Math.round(h * dpr());
    }
    const ctx = c.getContext("2d");
    ctx.setTransform(dpr(), 0, 0, dpr(), 0, 0);
    ctx.clearRect(0, 0, w, h);
    return [ctx, w, h];
  };
  // A 20-bar strip: each bar split at the slider's selection (outside it
  // faded), a dark tick at the median. Pixels only.
  const drawStrip = (c, strip, fracs, medianFrac, loFrac, hiFrac) => {
    const [ctx, w, h] = sizeCanvas(c);
    const lo = loFrac * w, hi = hiFrac * w;
    for (let k = 0; k < strip.length; k++) {
      const { h: bh, color } = strip[k];
      if (bh <= 0) continue;
      const x0 = fracs[k][0] * w, x1 = fracs[k][1] * w - 0.5, y = h - Math.max(1, bh * h);
      const cuts = [x0, Math.min(Math.max(lo, x0), x1), Math.min(Math.max(hi, x0), x1), x1];
      for (let i = 0; i < 3; i++) {
        if (cuts[i + 1] <= cuts[i]) continue;
        ctx.globalAlpha = i === 1 ? 1 : 0.22;
        ctx.fillStyle = color;
        ctx.fillRect(cuts[i], y, cuts[i + 1] - cuts[i], h - y);
      }
    }
    ctx.globalAlpha = 1;
    if (medianFrac != null) { ctx.fillStyle = "#111827"; ctx.fillRect(medianFrac * w - 0.5, 0, 1, h); }
  };
  const flyable = (node, r) => { node.classList.toggle("nofly", !r.can_fly); node.title = r.title; };
  // The States tab is built once -- 53 rows, each with its canvas -- and
  // then updated in place: a slider move rewrites share bars, percents
  // and strips; a sort change reorders the same rows.
  let states = null;
  const buildStates = (j) => {
    const container = el("div");
    const sorts = el("div", "sorts");
    sorts.append(el("span", "lbl", j.sort_label));
    const sortButtons = j.sorts.map(({ code, label }) => {
      const b = el("button", null, label);
      b.addEventListener("click", () => { sort = code; draw(); });
      sorts.append(b);
      return [code, b];
    });
    const table = el("table");
    const thead = el("thead");
    const hr = el("tr");
    j.columns.forEach((text, i) => hr.append(el("th", i === 2 || i === 4 ? "n" : null, text)));
    thead.append(hr);
    const tbody = el("tbody");
    table.append(thead, tbody);
    const note = el("div", "note");
    for (const line of j.footnote) note.append(el("p", null, line));
    container.append(sorts, table, note);
    const rows = new Map();
    for (const r of j.rows) {
      const tr = el("tr");
      tr.dataset.fips = r.fips;
      tr.dataset.code = r.fips;
      tr.dataset.fly = "state";
      const strip = el("canvas", "strip");
      const share = el("span", "share");
      const fill = el("i");
      share.append(fill);
      const median = el("td", "n", r.median_text);
      const pct = el("td", "n");
      const cells = [el("td", "code", r.postal), el("td"), median, el("td"), pct];
      cells[1].append(strip);
      cells[3].append(share);
      tr.append(...cells);
      rows.set(r.fips, { tr, strip, fill, pct, median });
    }
    return { container, sortButtons, tbody, rows, order: "" };
  };
  const drawStates = () => {
    const j = JSON.parse(wasm.map_census_states_json(sort));
    if (!j) { pane.replaceChildren(el("div", "muted", "Loading…")); return false; }
    if (!states) states = buildStates(j);
    if (pane.firstChild !== states.container) pane.replaceChildren(states.container);
    for (const [code, b] of states.sortButtons) b.className = code === j.sort ? "on" : "";
    const order = j.rows.map((r) => r.fips).join(",");
    if (order !== states.order) {
      for (const r of j.rows) states.tbody.append(states.rows.get(r.fips).tr);
      states.order = order;
    }
    for (const r of j.rows) {
      const row = states.rows.get(r.fips);
      if (!row) continue;
      row.tr.classList.toggle("apart", r.apart);
      flyable(row.tr, r);
      row.fill.style.width = `${r.share * 100}%`;
      row.pct.textContent = r.share_text;
      row.median.textContent = r.median_text;
      drawStrip(row.strip, r.strip, j.bar_fracs, r.median_frac, j.lo_frac, j.hi_frac);
    }
    return true;
  };
  const drawMetros = () => {
    const j = JSON.parse(wasm.map_census_metros_json());
    if (!j) { pane.replaceChildren(el("div", "muted", "Loading…")); return false; }
    const key = el("div", "key");
    const gini = el("span", null, "median · Gini");
    gini.title = j.gini_label;
    key.append(el("span", null, j.key), gini);
    const c = el("canvas", "ridges");
    c.title = j.gini_label;
    const AXIS = 16, plotH = j.height_px, height = plotH + AXIS;
    c.style.height = `${height}px`;
    pane.replaceChildren(key, c);
    const [ctx, w] = sizeCanvas(c);
    // Pixels: the label and number columns, and the plot between them,
    // into which the JSON's unit-square coordinates are scaled.
    const LABEL = 104, NUM = 72, rw = Math.max(40, w - LABEL - NUM - 8), x0 = LABEL + 4;
    const X = (f) => x0 + f * rw, Y = (f) => f * plotH;
    const font = getComputedStyle(root).fontFamily;
    ctx.font = `11px ${font}`;
    for (const m of j.metros) {
      const base = Y(m.baseline);
      ctx.beginPath();
      m.points.forEach(([x, y], k) => (k ? ctx.lineTo(X(x), Y(y)) : ctx.moveTo(X(x), Y(y))));
      ctx.closePath();
      ctx.fillStyle = "#fff";
      ctx.fill();
      ctx.globalAlpha = 0.8;
      ctx.fillStyle = m.color;
      ctx.fill();
      ctx.globalAlpha = 1;
      ctx.strokeStyle = "#1f2937";
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.fillStyle = "#1f2937";
      ctx.textAlign = "left";
      ctx.fillText(m.title, 0, base - 3, LABEL - 2);
      ctx.fillStyle = "#374151";
      ctx.textAlign = "right";
      ctx.fillText(m.median_text, w - 32, base - 3);
      ctx.fillStyle = "#9ca3af";
      ctx.fillText(m.gini_text, w, base - 3);
    }
    if (!j.full) {
      const lo = X(j.lo_frac), hi = X(j.hi_frac);
      ctx.fillStyle = "rgba(31, 41, 55, 0.10)";
      ctx.fillRect(lo, 0, Math.max(1, hi - lo), plotH);
      ctx.fillStyle = "rgba(31, 41, 55, 0.6)";
      ctx.fillRect(lo - 0.5, 0, 1, plotH);
      ctx.fillRect(hi - 0.5, 0, 1, plotH);
    }
    ctx.fillStyle = "#9ca3af";
    ctx.font = `10px ${font}`;
    for (const [label, f] of j.axis) {
      ctx.textAlign = f === 0 ? "left" : f === 1 ? "right" : "center";
      ctx.fillText(label, X(f), height - 3);
    }
    c.dataset.ridges = j.metros.length;
    return true;
  };
  const drawInView = () => {
    const j = JSON.parse(wasm.map_census_in_view_json());
    if (!j) { pane.replaceChildren(el("div", "muted", "Loading…")); return false; }
    if (j.counting) { pane.replaceChildren(el("div", "muted", j.text)); return false; }
    const out = [el("div", "iv-head", j.heading)];
    const strips = [];
    const section = (title, rows) => {
      const sec = el("div", "iv-sec");
      for (const t of [title, ...j.columns]) sec.append(el("span", null, t));
      out.push(sec);
      for (const r of rows) {
        const row = el("div", "iv-row");
        row.dataset.fly = r.kind;
        row.dataset.code = r.code;
        flyable(row, r);
        const line = el("div", "iv-line");
        for (const t of [r.name, r.people_text, r.median_text, r.share_text]) line.append(el("span", null, t));
        const c = el("canvas", "strip");
        row.append(line, c);
        out.push(row);
        strips.push([c, r]);
      }
    };
    section(j.sections[0], j.states);
    section(j.sections[1], j.metros);
    if (j.no_metro) out.push(el("div", "muted", j.no_metro));
    pane.replaceChildren(...out);
    for (const [c, r] of strips) drawStrip(c, r.strip, j.bar_fracs, r.median_frac, j.lo_frac, j.hi_frac);
    return true;
  };
  // Redraws the open tab unless nothing it shows moved: the tab, the
  // sort, the open state and the panel's revision (Rust bumps it when the
  // range changes -- so a drag inside one stop is free -- and when the
  // load completes and the rows turn clickable).
  // `force`: the In view tab after a settle, whose count changed.
  function draw(force) {
    if (root.classList.contains("closed")) return;
    const key = `${tab}|${sort}|${wasm.map_census_panel_rev()}`;
    if (!force && key === drawnKey) return;
    for (const [name, b] of tabButtons) b.className = name === tab ? "on" : "";
    if (tab !== "states") point(-1);
    const drawn = tab === "states" ? drawStates() : tab === "metros" ? drawMetros() : drawInView();
    drawnKey = drawn ? key : null;
  }
  // The pack's tables arrive after this runs: draw once they have.
  (function wait() {
    if (wasm.map_census_states_json(0) === "null") { setTimeout(wait, 150); return; }
    draw();
  })();
  return {
    range: () => draw(false),
    settled: () => { if (tab === "inview") draw(true); },
    // Every frame: one number read; a redraw only when it moved.
    poll: () => draw(false),
  };
}
