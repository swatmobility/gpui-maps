//! `?census=<pack>`: the US census income choropleth, read from a `BGPK`
//! pack by `Range` (`src/census/pack.rs`, `tools/census-pack`), one state
//! per frame, with a hover popup. Plumbing only: every decision -- the
//! pack format, the load order, the paint, the popup's words, when it
//! shows -- is `crate::census`, tested there.

use super::*;
use crate::census::{breakdowns, hist, pack, panel_json, view};

pub(super) struct CensusState {
    pub counties: std::collections::HashMap<u32, String>,
    pub rows: Vec<pack::PackRow>,
    /// Source name -> first pack row of that source.
    pub first_row: std::collections::HashMap<String, u32>,
    pub hover: crate::census::hover::HoverTracker,
    pub status: crate::census::LoadStatus,
    /// When the first state's source first drew, in ms from navigation
    /// start (`performance.now()`'s origin), as every census time is.
    pub first_state_ms: Option<f64>,
    /// The income slider: the selection every census source is painted
    /// with (a source pushed later is declared with it), what a press
    /// holds, the track it was pressed on, and the paint paths so far.
    pub range: crate::census::ramp::IncomeRange,
    pub held: Option<crate::census::slider::Handle>,
    pub track: crate::census::slider::Track,
    pub paths: crate::census::slider::PathCounts,
    /// `ShellOverlays::update` times (ms), one per tick since the last
    /// `map_census_take_update_ms`: what a drag costs the frame loop.
    pub update_ms: Vec<f32>,
    /// Every pushed block group's bounding box and income bar
    /// (`census::view::view_items`, built in `push_state` from the
    /// decoded rings before they are dropped): what `in_view_hist`
    /// recounts over. Grows one state at a time as the pack loads.
    pub view_items: Vec<view::ViewItem>,
    /// The national histogram, built once from every row when the pack's
    /// tables land -- never recomputed.
    pub national: hist::Histogram,
    /// The in-view histogram as of the last camera settle counted
    /// (`hist_camera_rev`); `None` before the first count.
    pub in_view_hist: Option<hist::Histogram>,
    /// The `(camera, view_items.len())` last counted. The camera is
    /// compared by value, not `CameraController::input_generation`: a
    /// programmatic `map_jump_to` -- what `?camera=` and the perf harness's
    /// `charts` mode both use -- is not "input" and never bumps that
    /// counter, so a rev keyed on it would latch at the first settle and
    /// never recount again. The item count is part of the key too: the
    /// pack loads one state per frame, so the map can settle (no pending
    /// tiles) well before every state has landed -- without it, the very
    /// first settle at the opening camera would cache a partial count
    /// forever, since the camera never moves again on its own. Recounts
    /// only when either differs from this AND the camera is not moving
    /// right now, so a pan never walks `view_items` per frame
    /// (`map_census_histograms_json`).
    pub hist_camera_rev: Option<(crate::render::Camera, usize)>,
    /// `view_items[i]`'s row in `rows`: the pack pushes states in view
    /// order, not row order, and `Breakdowns::in_view` takes row indices.
    pub view_rows: Vec<u32>,
    /// `view_items`' coarse grid (`view::ViewIndex`), grown with it: a
    /// recount tests only the boxes the camera might see.
    pub view_index: view::ViewIndex,
    /// The Breakdowns panel's maths (`census::breakdowns`), built once
    /// when the tables land; `None` if the tables were refused. Its
    /// bounds (the fly-to's) are set once every state is pushed.
    pub breakdowns: Option<breakdowns::Breakdowns>,
    /// The In view tab's breakdown, recounted with `in_view_hist` on the
    /// same camera-settled key; `None` before the first count.
    pub in_view: Option<breakdowns::InView>,
    /// The state the panel's row hover points at: every other state's
    /// source is painted dimmed (`panel_json::source_paint_json`).
    pub pointed: Option<u32>,
    /// The paint paths every `map_census_dim_state` call took, summed:
    /// what the harness's row-hover gate reads (`map_census_dim_json`).
    pub dim_paths: crate::census::slider::PathCounts,
    /// `map_census_dim_state` calls that changed the pointed state.
    pub dim_changes: u64,
    /// Bumped whenever the slider's range changes: the panel redraws its
    /// open tab only when this moved (`map_census_range_rev`).
    pub range_rev: u32,
    /// The panel's redraw key (`panel_json::PanelRev`): the range AND the
    /// bounds landing (`map_census_panel_rev`).
    pub panel_rev: panel_json::PanelRev,
    /// The last in-view recount's own time (ms) and how many ran
    /// (`map_census_status_json`'s `recount_ms`, `recounts`).
    pub recount_ms: Option<f64>,
    /// Of `recount_ms`, the in-view walk and histogram alone (the rest is
    /// `Breakdowns::in_view`).
    pub recount_walk_ms: Option<f64>,
    pub recounts: u32,
}

pub(super) const ATTRIBUTION: &str = "US Census Bureau: TIGER/Line 2023, ACS 2023 5-year";

/// The page's `?census=` name, when it is one.
pub(super) fn census_param() -> Option<String> {
    query_param("census")
}

async fn fetch_range(url: &str, start: u64, len: u64) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let init = web_sys::RequestInit::new();
    let headers = web_sys::Headers::new().map_err(|_| "headers")?;
    let last = start.checked_add(len - 1).ok_or("a range past 2^64")?;
    headers.set("Range", &format!("bytes={start}-{last}")).map_err(|_| "range header")?;
    init.set_headers(&headers);
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(|_| "request")?;
    let window = web_sys::window().ok_or("no window")?;
    let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|_| format!("{url}: fetch failed"))?
        .dyn_into()
        .map_err(|_| "not a response")?;
    if resp.status() != 206 {
        return Err(format!("{url}: HTTP {} (a census pack must be served with Range support)", resp.status()));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(resp.array_buffer().map_err(|_| "body")?)
        .await
        .map_err(|_| "body")?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    if bytes.len() as u64 != len {
        return Err(format!("{url}: asked {len} bytes, got {}", bytes.len()));
    }
    Ok(bytes)
}

/// One `requestAnimationFrame`, as a future: the load yields here between
/// states so the frame loop builds and draws what was pushed.
async fn next_frame() {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.request_animation_frame(&resolve);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

pub(super) async fn load(app: Rc<RefCell<MapApp>>) {
    let Some(name) = census_param() else { return };
    if !crate::census::valid_pack_name(&name) {
        status("census: the pack name must be letters, digits, _ or -");
        return;
    }
    if let Err(e) = load_pack(&app, &format!("./census/{name}.bgpk")).await {
        log(&format!("census: {e}"));
        status(&format!("census: {e}"));
    }
}

async fn load_pack(app: &Rc<RefCell<MapApp>>, url: &str) -> Result<(), String> {
    let started = now_ms();
    let first = fetch_range(url, 0, pack::HEADER_LEN as u64).await?;
    let need = pack::header_len(&first).map_err(|e| e.to_string())?;
    let head = fetch_range(url, 0, need as u64).await?;
    let header = pack::parse_header(&head).map_err(|e| e.to_string())?;
    let tables_body = fetch_range(url, header.tables_offset, u64::from(header.tables_len)).await?;
    let tables = pack::decode_tables(&header, &tables_body).map_err(|e| e.to_string())?;
    log(&format!(
        "census: {} block groups in {} states, {:.1} MB, tolerance {} m; tables in {:.0} ms",
        header.feature_count,
        header.states.len(),
        (header.tables_offset + u64::from(header.tables_len)) as f64 / 1e6,
        header.tolerance_m,
        now_ms() - started
    ));
    let national = hist::Histogram::of_incomes(tables.rows.iter().map(|r| r.income.map(f64::from)));
    // The panel's breakdowns, from the pack's own rows (each carries its
    // population) and CBSA table: one sort of the block groups, here.
    let built = now_ms();
    let breakdowns = {
        let rows: Vec<breakdowns::Row> = tables
            .rows
            .iter()
            .map(|r| breakdowns::Row { geoid: r.geoid, income: r.income.map(f64::from), population: r.population.map(f64::from) })
            .collect();
        match breakdowns::Breakdowns::from_texts(&rows, None, &tables.cbsa_text, breakdowns::BinEdges::default()) {
            Ok(b) => {
                log(&format!(
                    "census: breakdowns for {} states and {} metros in {:.0} ms",
                    b.states.len(),
                    b.metros.len(),
                    now_ms() - built
                ));
                Some(b)
            }
            Err(e) => {
                log(&format!("census: no breakdowns: {e}"));
                None
            }
        }
    };
    {
        let mut a = app.borrow_mut();
        a.census = Some(CensusState {
            counties: crate::census::names::parse_county_names(&tables.county_text),
            rows: tables.rows,
            first_row: Default::default(),
            hover: Default::default(),
            status: crate::census::LoadStatus::new(header.states.len() as u32),
            first_state_ms: None,
            range: query_param("range")
                .and_then(|q| crate::census::slider::range_from_query(&q))
                .unwrap_or_else(crate::census::ramp::IncomeRange::full),
            held: None,
            track: crate::census::slider::Track::default_bar(),
            paths: Default::default(),
            update_ms: Vec::new(),
            view_items: Vec::new(),
            national,
            in_view_hist: None,
            hist_camera_rev: None,
            view_rows: Vec::new(),
            view_index: Default::default(),
            breakdowns,
            in_view: None,
            pointed: None,
            dim_paths: Default::default(),
            dim_changes: 0,
            range_rev: 0,
            panel_rev: Default::default(),
            recount_ms: None,
            recount_walk_ms: None,
            recounts: 0,
        });
        // `?paint_check_settle_ms=`: the A/B knob for the drag harness
        // (`scripts/census-web.mjs drag`); 0 is the check on every tick.
        if let Some(ms) = query_param("paint_check_settle_ms").and_then(|v| v.parse::<u64>().ok()) {
            a.overlays.set_paint_check_settle(std::time::Duration::from_millis(ms));
        }
        add_credit(ATTRIBUTION);
    }
    let view = {
        let a = app.borrow();
        crate::census::view_box(a.camera_control.camera())
    };
    let pushed = push_states(app, url, &header, view).await;
    // The fly-to's bounds, once, over whatever was pushed -- on a failed
    // or partial load too (`view::row_boxes` leaves the missing rows NaN),
    // so the states that did arrive can still be flown to.
    let mut a = app.borrow_mut();
    if let Some(census) = a.census.as_mut() {
        let boxes = view::row_boxes(&census.view_items, &census.view_rows, census.rows.len());
        if let Some(b) = census.breakdowns.as_mut() {
            // Bumps the panel's revision: the rows turn clickable.
            census.panel_rev.set_bounds(b, &boxes);
        }
    }
    pushed
}

/// Fetches, decodes and pushes each state, one per frame, in view order.
async fn push_states(app: &Rc<RefCell<MapApp>>, url: &str, header: &pack::PackHeader, view: [f64; 4]) -> Result<(), String> {
    for i in pack::load_order(&header.states, view) {
        let row = &header.states[i];
        let body = fetch_range(url, row.offset, u64::from(row.len)).await?;
        let geo = pack::decode_state(row, &body).map_err(|e| e.to_string())?;
        {
            let mut a = app.borrow_mut();
            push_state(&mut a, row, &geo)?;
            a.needs_redraw = true;
        }
        drop(geo);
        // One state per frame: yield so the frame loop builds and draws.
        next_frame().await;
    }
    log(&format!("census: all {} states pushed at {:.0} ms", header.states.len(), now_ms()));
    Ok(())
}

fn push_state(a: &mut MapApp, row: &pack::DirRow, geo: &pack::StateGeometry) -> Result<(), String> {
    use crate::overlay::host::{FeatureId, FeatureInput, FeaturePaint, GeometryInput, PropertyColumn};
    let zoom = a.display_zoom();
    let census = a.census.as_mut().ok_or("census state missing")?;
    let rows = (row.first_feature as usize)
        .checked_add(row.feature_count as usize)
        .and_then(|end| census.rows.get(row.first_feature as usize..end))
        .ok_or("a state's rows are past the table")?;
    if geo.features.len() != rows.len() {
        return Err(format!("state {:02}: {} features, {} rows", row.fips, geo.features.len(), rows.len()));
    }
    let rings: Vec<&[[f64; 2]]> = geo.rings.iter().map(|&(s, e)| &geo.coords[s as usize..e as usize]).collect();
    let polys: Vec<&[&[[f64; 2]]]> = geo.polygons.iter().map(|&(s, e)| &rings[s as usize..e as usize]).collect();
    let features: Vec<FeatureInput<'_>> = geo
        .features
        .iter()
        .zip(rows)
        .map(|(&(s, e), r)| FeatureInput {
            id: Some(FeatureId::Number(r.geoid as f64)),
            geometry: GeometryInput::Polygon(&polys[s as usize..e as usize]),
            paint: FeaturePaint::default(),
        })
        .collect();
    let income: Vec<Option<f64>> = rows.iter().map(|r| r.income.map(f64::from)).collect();
    // The in-view histogram needs each feature's rings for its bounding
    // box (`view::view_items`) -- built here, before `geo` (and its rings)
    // is dropped at the end of `load_pack`'s per-state loop.
    let items = view::view_items(rows, |i| {
        let (fs, fe) = geo.features[i];
        (fs..fe)
            .flat_map(|p| {
                let (rs, re) = geo.polygons[p as usize];
                (rs..re).map(|r| {
                    let (s, e) = geo.rings[r as usize];
                    geo.coords[s as usize..e as usize].to_vec()
                })
            })
            .collect()
    });
    census.view_items.extend(items);
    census.view_index.extend(&census.view_items);
    census.view_rows.extend(row.first_feature..row.first_feature + rows.len() as u32);
    let name = crate::census::source_name(row.fips);
    census.first_row.insert(name.clone(), row.first_feature);
    census.status.loaded += 1;
    let paint: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&panel_json::source_paint_json(census.range, census.pointed, u32::from(row.fips)))
            .map_err(|e| e.to_string())?;
    a.overlays.declare_source_paint(&name, &paint).map_err(|e| e.to_string())?;
    a.overlays
        .set_source_columns(&name, &features, &[("income", PropertyColumn::Number(&income))], zoom)
        .map_err(|e| e.to_string())
}

/// The frame loop's tally, after a frame drew: how many census sources
/// drew in it, stamping the first state's time.
pub(super) fn after_draw(a: &mut MapApp) {
    let Some(census) = a.census.as_ref() else { return };
    let drawn = census
        .first_row
        .keys()
        .filter(|name| a.overlays.source_draw_stats(name).is_some_and(|s| s.drew()))
        .count() as u32;
    let now = now_ms();
    let Some(census) = a.census.as_mut() else { return };
    if drawn > 0 && census.first_state_ms.is_none() {
        census.first_state_ms = Some(now);
        log(&format!("census: first state drew at {now:.0} ms"));
    }
    census.status.drew(drawn, now);
}

/// Every tick, after the overlays' update: `busy` is its verdict (work
/// still owed). Stamps the all-drawn time (`census::LoadStatus::overlays`).
pub(super) fn after_overlays(a: &mut MapApp, busy: bool, update_ms: f64) {
    let Some(census) = a.census.as_mut() else { return };
    if census.update_ms.len() < 100_000 {
        census.update_ms.push(update_ms as f32);
    }
    let before = census.status.all_drawn_ms;
    census.status.overlays(busy, 0.0);
    if before.is_none() {
        if let Some(ms) = census.status.all_drawn_ms {
            log(&format!(
                "census: all {} states pushed and the view complete at {ms:.0} ms ({} sources drew in that frame)",
                census.status.total, census.status.sources_drawn
            ));
        }
    }
}

/// Whether the camera is moving: easing, inertia, a drag or a pinch in hand.
fn camera_moving(a: &MapApp) -> bool {
    !a.camera_control.is_settled() || a.pan_inertia.is_some() || a.drag_from.is_some() || a.pinch.is_some()
}

/// The popup at CSS pixel `(x, y)` as JSON (`census::hover::popup_json`),
/// or `null`: off every block group, off the globe, or while the camera
/// moves. Reads only; never asks for a frame.
#[wasm_bindgen]
pub fn map_census_hover_json(x: f32, y: f32) -> String {
    with_app(|a| {
        let camera = *a.camera_control.camera();
        let moving = camera_moving(a);
        let picked = a.overlays.feature_at(&camera, x, y).and_then(|hit| {
            let census = a.census.as_ref()?;
            let first = *census.first_row.get(&*hit.source)?;
            let r = census.rows.get(first as usize + hit.index)?;
            Some(crate::census::hover::HoverInfo { geoid: r.geoid, income: r.income, aland_km2: r.aland_km2 })
        });
        let census = a.census.as_mut()?;
        let shown = census.hover.show(picked, moving)?;
        Some(crate::census::hover::popup_json(&shown, &census.counties))
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// `{"loaded", "total", "all_drawn_ms", "sources_drawn", "frames_drawn",
/// "first_state_ms"}`, or `null` before the pack's tables arrived.
#[wasm_bindgen]
pub fn map_census_status_json() -> String {
    with_app(|a| {
        let census = a.census.as_ref()?;
        let mut v: serde_json::Value = serde_json::from_str(&census.status.json()).ok()?;
        v["first_state_ms"] = census.first_state_ms.into();
        v["recount_ms"] = census.recount_ms.into();
        v["recount_walk_ms"] = census.recount_walk_ms.into();
        v["recounts"] = census.recounts.into();
        Some(v.to_string())
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The legend: the ramp's nine stops and five tick labels.
#[wasm_bindgen]
pub fn map_census_ramp_json() -> String {
    crate::census::ramp::ramp_json()
}

/// The histogram strip's data (`census::hist::histograms_json`): the
/// national bins (built once, when the tables land), the in-view bins,
/// the default caption, each bar's fraction along the ramp, and the paint
/// colours. `null` before the pack's tables arrive.
///
/// The in-view bins recount only when `view::in_view_needs_recount` says
/// so (`hist_camera_rev`, native's same camera-settled throttle, widened
/// for the pack's progressive load -- see that function's doc) -- a pan
/// never walks `view_items` per frame. The JS side adds its own throttle
/// on top, calling this after a slider move and once a frame newly
/// settled, not on every `requestAnimationFrame`.
#[wasm_bindgen]
pub fn map_census_histograms_json() -> String {
    with_app(|a| {
        let camera = *a.camera_control.camera();
        let moving = camera_moving(a);
        let census = a.census.as_mut()?;
        recount_in_view(census, camera, moving);
        Some(hist::histograms_json(&census.national, census.in_view_hist.as_ref()))
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// Recounts what the view shows -- the in-view histogram and the In view
/// tab's breakdown, over ONE walk of `view_items` -- when
/// `view::in_view_needs_recount` says so (camera settled, and the camera
/// or the pushed block groups changed since the last count).
fn recount_in_view(census: &mut CensusState, camera: crate::render::Camera, moving: bool) {
    let key = (camera, census.view_items.len());
    if !view::in_view_needs_recount(moving, census.hist_camera_rev, key) {
        return;
    }
    // `true`: the web shell never calls `CameraController::set_globe_enabled(false)`
    // (there is not even a public getter for it), so the globe is always
    // on here, matching `feature_at`'s own hard-coded `true` a few
    // functions up. Revisit both if a globe toggle ever reaches the web
    // shell.
    let started = now_ms();
    let visible = view::in_view_indices_indexed(&census.view_index, &census.view_items, &camera, true);
    census.in_view_hist = Some(hist::Histogram::of_bins(visible.iter().map(|&i| census.view_items[i as usize].bin)));
    census.recount_walk_ms = Some(now_ms() - started);
    if let Some(b) = census.breakdowns.as_ref() {
        let rows: Vec<u32> = visible.iter().filter_map(|&i| census.view_rows.get(i as usize).copied()).collect();
        census.in_view = Some(b.in_view(&rows, panel_json::IN_VIEW_GROUPS));
    }
    census.hist_camera_rev = Some(key);
    census.recount_ms = Some(now_ms() - started);
    census.recounts += 1;
}

/// Where the popup card goes for a pointer at `(x, y)` CSS px, the card
/// `w` x `h` and the viewport `vw` x `vh`: `[left, top]`, flipped at the
/// right and bottom edges (`census::hover::popup_origin`, native's rule).
#[wasm_bindgen]
pub fn map_census_popup_origin(x: f32, y: f32, w: f32, h: f32, vw: f32, vh: f32) -> Vec<f32> {
    let (left, top) = crate::census::hover::popup_origin((x, y), (w, h), (vw, vh));
    vec![left, top]
}


/// Paints every census source with `range` (`ShellOverlays::update_source_paint`,
/// the table-only path when the classes hold) and tallies the paths.
fn set_range(a: &mut MapApp, range: crate::census::ramp::IncomeRange) {
    let Some(census) = a.census.as_mut() else { return };
    if census.range == range {
        return;
    }
    census.range = range;
    census.range_rev = census.range_rev.wrapping_add(1);
    census.panel_rev.range_moved();
    let pointed = census.pointed;
    let mut paths = census.paths;
    repaint(a, range, pointed, |_| true, &mut paths);
    if let Some(census) = a.census.as_mut() {
        census.paths = paths;
    }
}

/// Paints every census source with `range`, dimmed but for `pointed`'s
/// state when the panel points at one (`panel_json::source_paint_json`),
/// through `ShellOverlays::update_source_paint`, tallying each path into
/// `paths`. Only the sources whose state `only` accepts (a hover moving
/// between two states touches those two). The two paints (plain and
/// dimmed) are parsed once.
fn repaint(
    a: &mut MapApp,
    range: crate::census::ramp::IncomeRange,
    pointed: Option<u32>,
    only: impl Fn(u32) -> bool,
    paths: &mut crate::census::slider::PathCounts,
) {
    use crate::overlay::host::SourcePaintPath;
    type Paint = serde_json::Map<String, serde_json::Value>;
    let Some(census) = a.census.as_ref() else { return };
    let parse = |text: String| serde_json::from_str::<Paint>(&text).ok();
    let (Some(plain), Some(dimmed)) =
        (parse(range.paint_json(true)), parse(crate::census::ramp::dimmed_paint_json(&range.paint_json(true))))
    else {
        return;
    };
    let names: Vec<String> = census.first_row.keys().cloned().collect();
    for name in names {
        let Some(fips) = crate::census::source_state(&name) else { continue };
        if !only(fips) {
            continue;
        }
        let paint = if panel_json::is_dimmed(pointed, fips) { &dimmed } else { &plain };
        match a.overlays.update_source_paint(&name, paint) {
            Ok(SourcePaintPath::TableOnly) => paths.table_only += 1,
            Ok(SourcePaintPath::Rebake) => paths.rebake += 1,
            Ok(SourcePaintPath::Replan) => paths.replan += 1,
            Err(e) => log(&format!("census: {name}: {e}")),
        }
    }
    a.needs_redraw = true;
}

fn slider_json(a: &MapApp) -> Option<String> {
    let census = a.census.as_ref()?;
    Some(crate::census::slider::state_json(census.range, census.paths))
}

/// A press at `frac` along the slider's track, `width_px` wide
/// (`census::slider::grab_on`). On the bare track outside the knobs it
/// jumps the nearer knob there, as native's does. The slider's state JSON
/// (`census::slider::state_json`), or `null` before the pack's tables.
#[wasm_bindgen]
pub fn map_census_slider_grab(frac: f64, width_px: f64) -> String {
    with_app(|a| {
        let census = a.census.as_mut()?;
        census.track = crate::census::slider::Track::of_px(width_px);
        census.held = crate::census::slider::grab_on(census.track, census.range, frac);
        if census.held.is_none() {
            let (held, range) = crate::census::slider::drag_on(census.track, census.range, None, frac);
            census.held = held;
            set_range(a, range);
        }
        slider_json(a)
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The pointer at `frac` with the slider held (`census::slider::drag_on`):
/// repaints every census source when the range moved. Nothing held: the
/// state unchanged.
#[wasm_bindgen]
pub fn map_census_slider_drag(frac: f64) -> String {
    with_app(|a| {
        let census = a.census.as_mut()?;
        if census.held.is_some() {
            let (held, range) = crate::census::slider::drag_on(census.track, census.range, census.held, frac);
            census.held = held;
            set_range(a, range);
        }
        slider_json(a)
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The pointer let go of the slider.
#[wasm_bindgen]
pub fn map_census_slider_release() -> String {
    with_app(|a| {
        a.census.as_mut()?.held = None;
        slider_json(a)
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// Selects handle positions `lo..hi` (`?range=`, the gate). Refused
/// unless `lo < hi` on the slider: the state unchanged.
#[wasm_bindgen]
pub fn map_census_set_range(lo: u32, hi: u32) -> String {
    with_app(|a| {
        if let Some(range) = crate::census::slider::range_of(lo as usize, hi as usize) {
            set_range(a, range);
        }
        slider_json(a)
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The slider's state JSON without changing it.
#[wasm_bindgen]
pub fn map_census_slider_json() -> String {
    with_app(|a| slider_json(a)).flatten().unwrap_or_else(|| "null".to_string())
}

/// The overlay update times (ms) since the last call, and the full paint
/// checks run so far: `{"update_ms": [...], "paint_checks": n, "epochs": n}`
/// (`epochs`: class-changing re-bakes summed over the census sources).
#[wasm_bindgen]
pub fn map_census_take_update_ms() -> String {
    with_app(|a| {
        let epochs: u64 = a
            .census
            .as_ref()?
            .first_row
            .keys()
            .filter_map(|name| a.overlays.source_paint_epoch(name))
            .sum();
        let checks = a.overlays.paint_checks_run();
        let census = a.census.as_mut()?;
        let ms = std::mem::take(&mut census.update_ms);
        Some(serde_json::json!({ "update_ms": ms, "paint_checks": checks, "epochs": epochs }).to_string())
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The States tab (`census::panel_json::states_json`) in `sort`
/// (`SORT_MEDIAN` 0, `SORT_SHARE` 1, `SORT_POPULATION` 2) at the slider's
/// range; `null` before the breakdowns exist.
#[wasm_bindgen]
pub fn map_census_states_json(sort: u32) -> String {
    with_app(|a| {
        let census = a.census.as_ref()?;
        Some(panel_json::states_json(census.breakdowns.as_ref()?, census.range, sort))
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The Metros tab's ridgeline (`census::panel_json::metros_json`) at the
/// slider's range; `null` before the breakdowns exist.
#[wasm_bindgen]
pub fn map_census_metros_json() -> String {
    with_app(|a| {
        let census = a.census.as_ref()?;
        Some(panel_json::metros_json(census.breakdowns.as_ref()?, census.range))
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// The In view tab (`census::panel_json::in_view_json`), recounted with
/// the in-view histogram on the same settled-camera key
/// (`recount_in_view`), so a pan never walks the block groups; `null`
/// before the breakdowns exist.
#[wasm_bindgen]
pub fn map_census_in_view_json() -> String {
    with_app(|a| {
        let camera = *a.camera_control.camera();
        let moving = camera_moving(a);
        let census = a.census.as_mut()?;
        census.breakdowns.as_ref()?;
        recount_in_view(census, camera, moving);
        Some(panel_json::in_view_json(census.breakdowns.as_ref()?, census.in_view.as_ref(), census.range))
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// A States row's hover: every census source but `block-groups-<fips>`
/// painted dimmed (`ramp::dimmed_paint_json`), that one plain; `-1` (any
/// non-FIPS) restores them all. The dimmed paint keeps every class, so
/// each source must take the table-only path; only the sources whose paint
/// changes are touched (`panel_json::source_needs_repaint`: the old and new
/// states' between two states, every one to or from none):
/// `{"pointed", "table_only", "rebake", "replan"}` counts this call's
/// paths (all zero when `fips` is already the one pointed at).
#[wasm_bindgen]
pub fn map_census_dim_state(fips: i32) -> String {
    with_app(|a| {
        let census = a.census.as_mut()?;
        let pointed = panel_json::pointed_state(fips);
        let mut paths = crate::census::slider::PathCounts::default();
        if census.pointed != pointed {
            let old = std::mem::replace(&mut census.pointed, pointed);
            let range = census.range;
            repaint(a, range, pointed, |fips| panel_json::source_needs_repaint(old, pointed, fips), &mut paths);
            let census = a.census.as_mut()?;
            census.dim_changes += 1;
            census.dim_paths.table_only += paths.table_only;
            census.dim_paths.rebake += paths.rebake;
            census.dim_paths.replan += paths.replan;
        }
        Some(
            serde_json::json!({
                "pointed": pointed,
                "table_only": paths.table_only,
                "rebake": paths.rebake,
                "replan": paths.replan,
            })
            .to_string(),
        )
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// Bumped on every slider range change: the panel skips a redraw when it
/// has not moved since the last one (a drag inside one stop).
#[wasm_bindgen]
pub fn map_census_range_rev() -> u32 {
    with_app(|a| a.census.as_ref().map(|c| c.range_rev)).flatten().unwrap_or(0)
}

/// The panel's revision (`panel_json::PanelRev`): the page redraws its
/// open tab when this moved -- a range change, or the bounds landing once
/// the load completes (rows turn clickable). Read every frame; cheap.
#[wasm_bindgen]
pub fn map_census_panel_rev() -> f64 {
    with_app(|a| a.census.as_ref().map(|c| c.panel_rev.value() as f64)).flatten().unwrap_or(0.0)
}

/// Forgets the in-view count, so the next histogram or In view call
/// recounts at the current camera: the harness's cold-recount timing.
#[wasm_bindgen]
pub fn map_census_invalidate_in_view() {
    with_app(|a| {
        if let Some(census) = a.census.as_mut() {
            census.hist_camera_rev = None;
        }
    });
}

/// Every `map_census_dim_state` so far: `{"pointed", "changes",
/// "table_only", "rebake", "replan"}`, the paths summed. Reads only.
#[wasm_bindgen]
pub fn map_census_dim_json() -> String {
    with_app(|a| {
        let census = a.census.as_ref()?;
        let p = census.dim_paths;
        Some(
            serde_json::json!({
                "pointed": census.pointed,
                "changes": census.dim_changes,
                "table_only": p.table_only,
                "rebake": p.rebake,
                "replan": p.replan,
            })
            .to_string(),
        )
    })
    .flatten()
    .unwrap_or_else(|| "null".to_string())
}

/// A States row's click: eases to the state's block groups' extent
/// (`MercatorBounds::fit`, 40 px padding, at most z9) in 900 ms. `false`
/// when the state is unknown or its bounds are not in yet (they are set
/// once every state is pushed).
#[wasm_bindgen]
pub fn map_census_fly_state(fips: u32) -> bool {
    fly_to_group(fips, |b| &b.states)
}

/// An In view metro row's click: as `map_census_fly_state`, for a CBSA code.
#[wasm_bindgen]
pub fn map_census_fly_metro(code: u32) -> bool {
    fly_to_group(code, |b| &b.metros)
}

fn fly_to_group(code: u32, groups: impl Fn(&breakdowns::Breakdowns) -> &Vec<breakdowns::Group>) -> bool {
    let target = with_app(|a| {
        let camera = a.camera_control.camera();
        let (w, h) = (f64::from(camera.viewport_width_px), f64::from(camera.viewport_height_px));
        let b = a.census.as_ref()?.breakdowns.as_ref()?;
        let bounds = groups(b).iter().find(|g| g.code == code)?.bounds?;
        Some(bounds.fit(w, h, 40.0, 9.0))
    })
    .flatten();
    let Some((lon, lat, zoom)) = target else { return false };
    map_ease_to(lon, lat, zoom, 0.0, 0.0, 900.0);
    true
}
