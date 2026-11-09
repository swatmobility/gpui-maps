// The launch page's two behaviours: copy the install command, and play the
// agent session once when it scrolls into view. Without JavaScript the page
// is complete: the session shows in full.
document.documentElement.classList.add("js");

for (const b of document.querySelectorAll("[data-copy]")) {
  const state = b.querySelector(".install-state");
  const idle = state.textContent;
  b.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(b.dataset.copy);
      state.textContent = b.dataset.copied;
    } catch {
      state.textContent = b.dataset.failed;
    }
    setTimeout(() => { state.textContent = idle; }, 2000);
  });
}

// The language menu: opens on click, closes on Escape or a click outside,
// and remembers the language picked (lang.js reads it on the next visit).
const langBtn = document.querySelector(".lang-btn");
const langMenu = document.getElementById("lang-menu");
if (langBtn && langMenu) {
  const setOpen = (open) => {
    langMenu.hidden = !open;
    langBtn.setAttribute("aria-expanded", String(open));
  };
  langBtn.addEventListener("click", () => setOpen(langMenu.hidden));
  document.addEventListener("click", (e) => {
    if (!langMenu.hidden && !e.target.closest(".lang")) setOpen(false);
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !langMenu.hidden) { setOpen(false); langBtn.focus(); }
  });
  for (const a of langMenu.querySelectorAll("a[data-lang]")) {
    a.addEventListener("click", () => {
      try { localStorage.setItem("gpui-maps-lang", a.dataset.lang); } catch {}
    });
  }
}

// The player: the hero's live map. Tabs pick a demo, chips a map style,
// over one stage. The engine loads once: after that a tab only flies its
// camera (out to the globe, across, and in, so no frame sweeps street-level
// tiles across a sea), swaps its style in place (`map_set_style`) and
// starts or ends the drive (`map_nav_start` / `map_nav_end`); the frame is
// reloaded only if the engine is not up yet. One GPU context, whatever is
// clicked. On a desktop that can draw it (WebGPU or WebGL2, a mouse, a wide
// window, no data saver) it starts by itself; elsewhere a poster waits for
// Start. A style picked in the chips stays across the tabs. "Satellite" is
// Liberty over the imagery; every other chip is its style alone. The engine
// draws the imagery it was loaded with under any style that declares no
// raster layer, so "alone" means every raster layer pushed past the
// deepest zoom (minzoom 24), and one such layer added where a style had
// none: imagery declared, never drawn.
const stage = document.getElementById("try-stage");
if (stage) {
  const tabs = [...document.querySelectorAll(".try-tab")];
  const chips = [...document.querySelectorAll(".style-chip")];
  const poster = stage.querySelector("img");
  const start = stage.querySelector(".try-start");
  // The poster's credit: the live map draws its own, so this goes with the poster.
  const credit = stage.querySelector(".try-credit");
  const desc = document.querySelector(".try-desc");
  const open = desc.querySelector(".try-open");
  const canDraw = "gpu" in navigator || "WebGL2RenderingContext" in window;
  let frame = null;
  let chosen = null;
  // What the running engine has now, so a tab changes only what differs.
  let engineStyle = null;
  let engineNav = false;
  // A tab that is its own page of the demo (`data-page`, the census) needs
  // the engine loaded for it, and loaded again to leave it: the one switch
  // that reloads the frame.
  let enginePage = null;
  // Bumped by every tab press: a flight's later legs check it and stop
  // if another press came in between.
  let flight = 0;
  const current = () => tabs.find((t) => t.getAttribute("aria-selected") === "true");
  const styleOf = (tab) => chosen || tab.dataset.style;
  const fileOf = (style) => (style === "satellite" ? "./liberty-style.json" : style);
  // The demo directory, for fetching a style to strip.
  const demoBase = new URL(stage.dataset.boot, location.href);
  const plain = new Map();
  // `style` with its imagery off (see above), as a blob URL the engine can load.
  const plainUrl = async (style) => {
    if (!plain.has(style)) {
      const doc = await (await fetch(new URL(style, demoBase))).json();
      const rasters = doc.layers.filter((l) => l.type === "raster");
      for (const l of rasters) l.minzoom = 24;
      const source = Object.keys(doc.sources).find((k) => doc.sources[k].type === "raster");
      if (!rasters.length && source) doc.layers.unshift({ id: "no-imagery", type: "raster", source, minzoom: 24 });
      plain.set(style, URL.createObjectURL(new Blob([JSON.stringify(doc)], { type: "application/json" })));
    }
    return plain.get(style);
  };
  const swapStyle = async (map, style) => {
    engineStyle = style;
    await map.map_set_style(style === "satellite" ? fileOf(style) : await plainUrl(style));
  };
  // The globe's size on screen follows the zoom, so a narrow stage opens
  // further out: the whole planet fits at any width.
  const globeZoom = () => Math.max(0.3, 2.0 + Math.log2((stage.getBoundingClientRect().width || 1200) / 1400));
  const camOf = (tab) => {
    const [lon, lat, z, pitch, bearing] = tab.dataset.cam.split(",");
    return { lon: +lon, lat: +lat, zoom: z === "globe" ? globeZoom() : +z, pitch: +pitch, bearing: +bearing };
  };
  const cameraQuery = (c) => `&lon=${c.lon}&lat=${c.lat}&z=${c.zoom.toFixed(2)}&pitch=${c.pitch}&bearing=${c.bearing}`;
  const navQuery = (tab) => `&nav=sim&route=${tab.dataset.route}&speed=2`;
  // The engine's first load: the globe's imagery and space look ride along
  // on every tab (both fade out long before street level), so any tab can
  // fly back to the globe later.
  const bootSrc = (tab) => {
    const style = "&style=" + encodeURIComponent(fileOf(styleOf(tab)));
    if (tab.dataset.page) {
      // A page of its own opens on its own view, with no globe look.
      return stage.dataset.boot.replace(/&(look|look_time|imagery)=[^&]*/g, "") + "&" + tab.dataset.page + style;
    }
    return stage.dataset.boot + style + (tab.dataset.route ? navQuery(tab) : cameraQuery(camOf(tab)));
  };
  const fullScreenSrc = (tab) => {
    const style = styleOf(tab);
    let src = tab.dataset.q.replace("?embed&", "?site&") + "&style=" + encodeURIComponent(fileOf(style));
    if (style !== "satellite") src = src.replace(/&imagery=[^&]*/, "");
    if (tab.dataset.cam && tab.dataset.cam.includes("globe")) src += "&z=" + globeZoom().toFixed(2);
    return src;
  };
  const show = (tab) => {
    const style = styleOf(tab);
    for (const c of chips) c.setAttribute("aria-pressed", String(c.dataset.style === style));
    desc.firstChild.textContent = tab.dataset.desc + " ";
    if (credit && !frame) credit.textContent = tab.dataset.credit;
    open.href = fullScreenSrc(tab);
  };
  const engine = () => frame && frame.contentWindow && frame.contentWindow.gpuiMap;
  const boot = (tab) => {
    engineStyle = styleOf(tab);
    engineNav = !!tab.dataset.route;
    enginePage = tab.dataset.page || null;
    frame.src = bootSrc(tab);
  };
  // The engine loads its style from the URL as a file, rasters and all;
  // once it is up, a non-Satellite style is swapped for its plain form.
  const settle = async () => {
    for (let n = 0; n < 150 && !engine(); n++) await wait(200);
    const map = engine();
    if (map && engineStyle !== "satellite") {
      await map.map_set_style(await plainUrl(engineStyle)).catch(() => {});
    }
  };
  const play = () => {
    if (frame || !canDraw) return;
    frame = document.createElement("iframe");
    frame.title = stage.dataset.frameTitle;
    poster.remove();
    start.remove();
    credit.remove();
    stage.append(frame);
    frame.addEventListener("load", settle);
    boot(current());
  };
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const fly = async (map, to, id) => {
    const from = JSON.parse(map.map_camera_json());
    const dLon = Math.abs(((to.lon - from.lon + 540) % 360) - 180);
    const far = dLon + Math.abs(to.lat - from.lat) > 1;
    if (far) {
      const high = 3;
      if (from.zoom > high) {
        map.map_ease_to(from.lon, from.lat, high, 0, from.bearing, 700);
        await wait(700);
        if (id !== flight) return;
      }
      map.map_ease_to(to.lon, to.lat, Math.min(high, to.zoom), 0, 0, 1100);
      await wait(1100);
      if (id !== flight) return;
    }
    map.map_ease_to(to.lon, to.lat, to.zoom, to.pitch, to.bearing, far ? 1700 : 1200);
    // The page's Home button returns here, not to where the engine loaded.
    frame.contentWindow.gpuiHome = [to.lon, to.lat, to.zoom, to.pitch, to.bearing];
  };
  const go = async (tab) => {
    const id = ++flight;
    const map = engine();
    if (!map || (tab.dataset.page || null) !== enginePage) return boot(tab);
    try {
      if (engineNav && !tab.dataset.route) {
        map.map_nav_end();
        engineNav = false;
      }
      const style = styleOf(tab);
      if (style !== engineStyle) await swapStyle(map, style);
      if (id !== flight) return;
      if (tab.dataset.route) {
        await map.map_nav_start(tab.dataset.route, 2);
        engineNav = true;
      } else {
        await fly(map, camOf(tab), id);
      }
    } catch (e) {
      console.warn("player: reloading the engine", e);
      boot(tab);
    }
  };
  for (const t of tabs) {
    t.addEventListener("click", () => {
      for (const u of tabs) u.setAttribute("aria-selected", String(u === t));
      show(t);
      if (frame) go(t);
      else poster.src = t.dataset.poster;
    });
  }
  for (const c of chips) {
    c.addEventListener("click", () => {
      chosen = c.dataset.style;
      show(current());
      if (!frame) return play();
      const map = engine();
      if (!map) return boot(current());
      swapStyle(map, chosen).catch(() => boot(current()));
    });
  }
  // A page whose data is not on this server (the census pack is built
  // separately) loses its tab rather than opening on an error.
  for (const t of tabs.filter((t) => t.dataset.page)) {
    const [key, name] = t.dataset.page.split("=");
    fetch(new URL(`${key}/${name}.bgpk`, demoBase), { method: "HEAD" })
      .then((r) => { if (!r.ok) t.remove(); })
      .catch(() => t.remove());
  }
  if (canDraw) start.querySelector("button").addEventListener("click", play);
  else start.querySelector("button").remove();
  show(current());
  const roomy = window.matchMedia("(pointer: fine)").matches && window.innerWidth >= 900;
  if (canDraw && roomy && !(navigator.connection && navigator.connection.saveData)) play();
}

// The headline's turning word (see `.rot` in style.css): each letter its
// own span, so they can leave and arrive one after another. Held on the
// first word for anyone who asked for less motion.
const rot = document.querySelector(".rot[data-words]");
if (rot && !window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
  const words = rot.dataset.words.split("|");
  const spell = (word) => {
    rot.replaceChildren(...[...word].map((c, n) => {
      const ch = document.createElement("span");
      ch.className = "ch";
      ch.style.setProperty("--i", n);
      ch.textContent = c;
      return ch;
    }));
  };
  spell(words[0]);
  let i = 0;
  setInterval(() => {
    const leaving = rot.children.length;
    rot.classList.add("out");
    setTimeout(() => {
      i = (i + 1) % words.length;
      rot.classList.replace("out", "pre");
      spell(words[i]);
      void rot.offsetWidth;
      rot.classList.remove("pre");
    }, 420 + leaving * 22);
  }, 2800);
}

const term = document.getElementById("agent-term");
if (term) {
  term.querySelectorAll(".line").forEach((l, i) => l.style.setProperty("--i", i));
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (reduced || !("IntersectionObserver" in window)) {
    term.classList.add("play");
  } else {
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        term.classList.add("play");
        io.disconnect();
      }
    }, { threshold: 0.35 });
    io.observe(term);
  }
}
