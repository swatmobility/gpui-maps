//! Golden frame hashes: the Stage 0 bit-identity gate (map-stories plan,
//! Task 1). Blessed ONCE on the pre-session code; never re-blessed to make
//! a later task pass.
//!
//! **One re-derivation, from master, not a re-bless.** Merging master
//! abcce6c (3D buildings: near plane 0.5 -> 0.05, depth-stencil format,
//! Liberty's extrusions) changed `sg_z14_flat_labels` and
//! `sg_z15_pitch45_bearing30`. Their new hashes are master abcce6c's own
//! one-shot `capture` of the same scenes (the other five scenes hashed
//! unchanged there), and the merged session path reproduces all seven
//! bit for bit.
use super::*;
use crate::geo::TileId;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/capture_golden.txt");

fn fixture_decoded() -> crate::mvt::DecodedTile {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/singapore_z14.pbf"))
        .expect("vendored Singapore tile");
    crate::mvt::decode(bytes).expect("fixture decodes")
}

fn liberty() -> Style {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).expect("style");
    crate::style_spec::parse_and_compile(&bytes).expect("liberty compiles")
}

fn noto() -> Option<LabelFonts> {
    let f = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/NotoSans-Regular.latin-subset.ttf"))
        .expect("shipped font");
    Some((vec![f].into(), "Noto Sans".to_string()))
}

/// R1 fix round 1: `fonts_for`'s `label_fonts`-override branch -- the one
/// every story golden's deterministic `noto()` fixture takes -- must carry
/// the story card's faces exactly as the `None` branch's `new_font_system`
/// does. The design goldens only reach this indirectly (through a hash
/// comparison); this test names the gap directly, the same bug class the
/// review flagged (one `FontSystem` path loaded `STORY_FONTS`, the other
/// did not until `load_story_fonts` unified them).
#[cfg(not(any(target_os = "ios", target_arch = "wasm32")))]
#[test]
fn fonts_for_with_a_label_override_still_carries_the_story_family() {
    let mut state: Option<super::session::FontState> = None;
    let fonts = super::session::fonts_for(&mut state, &noto());
    let families = crate::font_fallback::installed_families(fonts.system.db());
    assert!(
        families.contains(&crate::font_fallback::STORY_FAMILY.to_ascii_lowercase())
            && families.contains(&crate::font_fallback::STORY_ARABIC_FAMILY.to_ascii_lowercase()),
        "{families:?}"
    );
}

/// The fixture tile placed at every id of the request's cover (and its
/// ring), with the building floor resolved against Liberty as the capture
/// resolves it (master's `CaptureRequest::resolved`): a caller fetches
/// `request.resolved(style).cover()`, and a test that does not is failed
/// by the capture itself.
fn store_for(request: &CaptureRequest) -> TileStore {
    let tile = fixture_decoded();
    request.resolved(&liberty()).cover().into_iter().chain(request.mask_ring()).map(|id| (id, tile.clone())).collect()
}

pub(super) fn scenes() -> Vec<(&'static str, CaptureRequest, TileStore, Style)> {
    let sg = |zoom: f64, pitch_deg: f32, bearing_deg: f32| CaptureRequest {
        center_lon: 103.851, center_lat: 1.2905, zoom,
        pitch: pitch_deg.to_radians(), bearing: bearing_deg.to_radians(),
        width: 640, height: 360, flat: true, labels: true, label_fonts: noto(),
        ..CaptureRequest::default()
    };
    let mut out = Vec::new();
    for (name, req) in [
        ("sg_z14_flat_labels", sg(14.5, 0.0, 0.0)),
        ("sg_z15_pitch45_bearing30", sg(15.2, 45.0, 30.0)),
        ("sg_z11_globe_band", sg(11.4, 20.0, 0.0)),
        ("globe_z3_default", CaptureRequest { width: 384, height: 384, ..CaptureRequest::default() }),
        ("globe_z2_lit_atmo_stars", CaptureRequest {
            center_lon: 110.0, center_lat: 10.0, zoom: 2.0, width: 384, height: 384,
            sun: Some(crate::render::atmosphere::Sun::at_utc(1_774_000_000.0)),
            atmosphere: Some(crate::render::atmosphere::Atmosphere {
                sun: crate::render::atmosphere::Sun::at_utc(1_774_000_000.0),
                // Named, not defaulted: this scene is the LIT globe, and the
                // default night brightness is 1.0 (no terminator) since
                // master's plain defaults (2026-09-29). The blessed hash is
                // the terminator-on frame it always was.
                night_brightness: crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
                ..crate::render::atmosphere::Atmosphere::default()
            }),
            stars: true,
            ..CaptureRequest::default()
        }),
    ] {
        let store = store_for(&req);
        out.push((name, req, store, liberty()));
    }
    // A GeoJSON overlay on the globe over no tiles (the overlay test's scene).
    let overlay = r##"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"fill-color":"#00ff00","fill-opacity":1.0},
         "geometry":{"type":"Polygon","coordinates":[[[100.0,0.0],[106.0,0.0],[106.0,4.0],[100.0,4.0],[100.0,0.0]]]}},
        {"type":"Feature","properties":{"circle-color":"#0000ff","circle-radius":6},
         "geometry":{"type":"Point","coordinates":[139.7,35.7]}}]}"##;
    out.push(("overlay_globe_z2", CaptureRequest {
        center_lon: 120.0, center_lat: 15.0, zoom: 2.0, width: 384, height: 384,
        clear: Clear::SENTINEL, backdrop: false, overlay: Some(overlay.to_string()),
        ..CaptureRequest::default()
    }, TileStore::new(), Style { layers: Vec::new(), ..Style::default() }));
    // Raster imagery at z0 over the whole sphere.
    let mut raster = std::collections::HashMap::new();
    raster.insert(TileId { z: 0, x: 0, y: 0 }, image::RgbaImage::from_pixel(256, 256, image::Rgba([40, 90, 160, 255])));
    out.push(("raster_z0", CaptureRequest {
        zoom: 0.0, width: 256, height: 256, clear: Clear::SENTINEL, backdrop: false, raster,
        ..CaptureRequest::default()
    }, TileStore::new(), Style { layers: Vec::new(), ..Style::default() }));
    out
}

/// Every test here renders Singapore with labels, whose line labels bump
/// the process-wide `LINE_LABELS_REFUSED` counter; the tests that read a
/// delta of it hold this lock, so these hold it too
/// (`symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK`'s contract).
fn refused_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_golden() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(GOLDEN).unwrap_or_default().lines()
        .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.trim().to_string())))
        .collect()
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn every_golden_scene_is_bit_identical_to_the_blessed_hash() {
    let _refused = refused_lock();
    let bless = std::env::var("GPUI_MAP_BLESS_GOLDEN").is_ok_and(|v| v == "1");
    let golden = read_golden();
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for (name, request, store, style) in scenes() {
        let shot = capture(&request, &store, &style).expect("a headless device");
        assert!(shot.tiles_drawn > 0 || store.is_empty(), "{name}: drew no tile");
        let hash = shot.sha256_hex();
        eprintln!("golden {name} {hash} tiles={} labels={}", shot.tiles_drawn, shot.labels_drawn);
        lines.push(format!("{name} {hash}"));
        match golden.get(name) {
            Some(want) if *want == hash => {}
            Some(want) => failures.push(format!("{name}: want {want} got {hash}")),
            None if bless => {}
            None => failures.push(format!("{name}: no blessed hash (run once with GPUI_MAP_BLESS_GOLDEN=1)")),
        }
    }
    if bless {
        assert!(golden.is_empty(), "the golden file already exists; it is blessed once, never again");
        std::fs::write(GOLDEN, lines.join("\n") + "\n").expect("write golden");
        return;
    }
    assert!(failures.is_empty(), "frames changed:\n{}", failures.join("\n"));
}

/// The enhanced, occluded building scene's hash. **Re-derived once, not
/// re-blessed to pass:** master 695e442's one-shot hash was
/// `62dbec00…71dfa1f0`, and on Metal about one first frame in fifty drew
/// the edge highlight differently (`fwidth` evaluated by the driver's early
/// pipeline variant; 2026-09-30). The highlight's width is now computed per
/// vertex (`extrusion_shader.wgsl` `edge_px`), which moved 232 of 518,400
/// pixels by at most 3/255, all on the wall line under a roof edge; every
/// other golden scene hashed unchanged.
/// Re-blessed for symbol occlusion of every symbol layer (was POI-only):
/// `50a591fc…` -> `362daae6…`, differing=1450 worst=190 pixels=518400,
/// bbox (0, 10, 960, 114) -- labels behind the Marina Bay towers. Proof
/// row: `docs/superpowers/proof/2026-10-story-design-rebless.md`.
const ENHANCED_BUILDINGS_HASH: &str = "362daae67c88eb9f9bc1ae85aeeeb44ecf06ad3653510420056e26598d9b0426";

/// **Master's denser 3D buildings, through the session path.** The merge
/// of master 695e442 moved master's `capture_async` changes (building
/// floor, enhanced look and depth fade, label occlusion) into
/// `session::encode_frame`. These hashes are master 695e442's OWN one-shot
/// `capture` of the same three scenes (a temporary probe on master's tree,
/// store = `request.resolved(&style).cover()`), not a bless of this code:
/// the merged path must reproduce them bit for bit. The floor/no-floor pair
/// differs, so the floor is live here, not a no-op.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn master_building_scenes_match_master_one_shot_bit_for_bit() {
    let _refused = refused_lock();
    let sg = |zoom: f64, pitch_deg: f32, bearing_deg: f32| CaptureRequest {
        center_lon: 103.851, center_lat: 1.2905, zoom,
        pitch: pitch_deg.to_radians(), bearing: bearing_deg.to_radians(),
        width: 640, height: 360, flat: true, labels: true, label_fonts: noto(),
        ..CaptureRequest::default()
    };
    let scenes = [
        (
            "enhanced + occlusion, z16.2 pitch 60",
            CaptureRequest {
                extrusion_look: crate::render::extrusion::ExtrusionLook::ENHANCED,
                label_occlusion: true,
                width: 960,
                height: 540,
                ..sg(16.2, 60.0, -25.0)
            },
            ENHANCED_BUILDINGS_HASH,
        ),
        ("MapLibre look, no floor", CaptureRequest { disable_building_floor: true, ..sg(15.5, 55.0, 40.0) }, "bd26f68c97c137bdcd1c71fb5d97eb07c6f117e61ee8bde2275a28182064b7bf"),
        ("MapLibre look, floor", sg(15.5, 55.0, 40.0), "facd96fc86d6ebc4f5b9cc3b45e04546c5f5002e94105cf1f30981177303340b"),
    ];
    let style = liberty();
    let mut session = CaptureSession::new_blocking(None).expect("a headless device");
    for (name, request, want) in scenes {
        let store = store_for(&request);
        let shot = capture(&request, &store, &style).expect("a headless device");
        dump_for_eyes(&shot, &format!("{}.png", name.replace([' ', ',', '+'], "_")));
        assert_eq!(shot.sha256_hex(), want, "{name}: one-shot differs from master's");
        assert_eq!(session.render(&request, &store, &style).expect("session").sha256_hex(), want, "{name}: session differs");
        if request.label_occlusion {
            assert!(shot.label_occlusion.0 > 0, "{name}: occlusion tested no label");
        }
    }
}

/// Per-frame cost of the one-shot path: 30 frames of `sg_z14_flat_labels`
/// at 1920x1080. Run alone, in release (see the plan's Task 1 commands).
#[test]
#[ignore = "timing; run alone in release"]
fn per_frame_cost_at_1080p() {
    let (_, mut request, _, style) = scenes().remove(0);
    request.width = 1920;
    request.height = 1080;
    let store = store_for(&request);
    let mut ms = Vec::new();
    let mut drawn = 0;
    for i in 0..31 {
        let start = std::time::Instant::now();
        let shot = capture(&request, &store, &style).expect("device");
        if i > 0 { ms.push(start.elapsed().as_secs_f64() * 1000.0); }
        drawn += shot.tiles_drawn;
    }
    assert!(drawn > 0, "a timing over frames that drew nothing is not a timing");
    ms.sort_by(f64::total_cmp);
    eprintln!("PER_FRAME_MS p50={:.1} p95={:.1} min={:.1} n={}", ms[ms.len() / 2], ms[ms.len() * 95 / 100], ms[0], ms.len());
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn one_session_renders_every_golden_scene_bit_identical_to_the_one_shot_path() {
    let _refused = refused_lock();
    let golden = read_golden();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    // Twice: on the second pass every label is a cache hit of its own
    // previous frame (or was trimmed and re-shaped), and must still hash
    // the same.
    for pass in 0..2 {
        for (name, request, store, style) in scenes() {
            let shot = session.render(&request, &store, &style).expect("render");
            assert_eq!(
                Some(&shot.sha256_hex()),
                golden.get(name),
                "{name} changed through a reused session (pass {pass})"
            );
        }
    }
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_session_reused_in_reverse_order_gives_the_same_hashes() {
    let _refused = refused_lock();
    let golden = read_golden();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    for (name, request, store, style) in scenes().into_iter().rev() {
        let shot = session.render(&request, &store, &style).expect("render");
        assert_eq!(Some(&shot.sha256_hex()), golden.get(name), "{name} depends on the previous frame");
    }
}

fn mean_abs_diff(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    a.pixels().zip(b.pixels()).map(|(p, q)| (0..3).map(|c| (f64::from(p[c]) - f64::from(q[c])).abs()).sum::<f64>() / 3.0).sum::<f64>() / f64::from(a.width() * a.height())
}

/// A `pixel_ratio` of 2 at double the physical size is the same logical
/// camera (`camera()` divides by the ratio), so it must ask for the same
/// tiles and, downscaled back to the 1x frame, draw substantially the same
/// picture.
#[test]
fn ratio_two_at_double_size_is_the_same_map_as_ratio_one() {
    let (_, base, _, style) = scenes().remove(0);
    let one = CaptureRequest { width: 480, height: 270, pixel_ratio: 1.0, ..base.clone() };
    let two = CaptureRequest { width: 960, height: 540, pixel_ratio: 2.0, ..base.clone() };
    assert_eq!(one.cover(), two.cover(), "the same logical camera must ask for the same tiles");
    let store = store_for(&one);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let a = session.render(&one, &store, &style).expect("1x");
    let b = session.render(&two, &store, &style).expect("2x");
    let a_img = image::RgbaImage::from_raw(a.width, a.height, a.rgba.clone()).unwrap();
    let b_img = image::imageops::resize(&image::RgbaImage::from_raw(b.width, b.height, b.rgba.clone()).unwrap(), 480, 270, image::imageops::FilterType::Triangle);
    let mad = mean_abs_diff(&a_img, &b_img);
    eprintln!("RATIO2_MAD {mad:.2}");
    assert!(mad < 0.03 * 255.0, "2x downscaled differs from 1x by {mad:.2}/255 (bound: 0.97 similarity)");
}

/// `globe: false` flattens the frame to pure Mercator, and its cover goes
/// straight to `flat_cover` without needing `flat: true` set for that
/// reason -- see `CaptureRequest::cover`.
#[test]
fn globe_off_at_z3_draws_a_flat_frame() {
    let request = CaptureRequest { width: 384, height: 384, flat: true, globe: false, ..CaptureRequest::default() };
    let cover = request.cover();
    assert!(!cover.is_empty(), "a flat z3 camera asks for a flat cover");
    let store = store_for(&request);
    let shot = capture(&request, &store, &liberty()).expect("device");
    assert_eq!(shot.transition, 0.0, "globe off must be a flat projection");
    // A different cover (flat_cover's rectangle vs the sphere's visible
    // hemisphere -- 4 tiles against 12 here) drawn onto a projection with
    // no clipping plane and no backdrop dressing: some sampled pixel must
    // land where the two disagree, even though the corners of THIS camera
    // (lon 88.8, lat 61, z3, pitch 0) are background in both -- the sphere
    // is large enough at this zoom to fill the whole silhouette, so the
    // backdrop and the flat clear resolve to the same style background at
    // the frame's edge.
    let on = capture(
        &CaptureRequest { globe: true, ..CaptureRequest { width: 384, height: 384, flat: true, ..CaptureRequest::default() } },
        &store_for(&CaptureRequest { width: 384, height: 384, ..CaptureRequest::default() }),
        &liberty(),
    )
    .expect("device");
    assert_eq!(on.transition, 1.0, "the globe-on control must actually be a globe frame");
    let differs = (0..384)
        .flat_map(|y| (0..384).map(move |x| (x, y)))
        .any(|(x, y)| shot.pixel(x, y) != on.pixel(x, y));
    assert!(differs, "a flat cover of {} tiles and a globe cover of {} tiles must draw a different frame somewhere", cover.len(), on.tiles_drawn);
}

/// `pixel_ratio` and `globe` are per-frame switches the mesh bake never
/// reads: `MeshKey` has no field for either, because neither changes what
/// `tessellate_plan_with_ranges` produces -- subdivision keys off the
/// tile's own zoom (`SubdivisionPolicy::for_tile`), not the camera or the
/// viewport (see `mesh_cache`'s module doc and Task 3's ruling on
/// `MeshKey::data`). The model matrix and the `GlobeUpload` uniform carry
/// both values instead, rebuilt fresh every frame regardless of a cache
/// hit. So toggling either through one session must still match the
/// one-shot frame at every step: a stale bake would show up as a mismatch
/// here even though the key was never touched.
#[test]
fn pixel_ratio_and_globe_toggle_through_one_session_never_reuses_a_stale_bake() {
    let _refused = refused_lock();
    let mut all = scenes();
    let (_, globe, globe_store, globe_style) = all.remove(3); // globe_z3_default
    // Same logical camera as `globe` (`camera()` divides by `pixel_ratio`):
    // a guaranteed cache hit on every bake, not just a correct miss.
    let ratio2 =
        CaptureRequest { width: globe.width * 2, height: globe.height * 2, pixel_ratio: 2.0, ..globe.clone() };
    assert_eq!(ratio2.cover(), globe.cover(), "pixel_ratio must not change the logical camera's cover");
    let flat = CaptureRequest { flat: true, globe: false, ..globe.clone() };

    for variant in [&ratio2, &flat] {
        let base_want = capture(&globe, &globe_store, &globe_style).expect("one-shot base").sha256_hex();
        let variant_want = capture(variant, &globe_store, &globe_style).expect("one-shot variant").sha256_hex();
        let mut session = CaptureSession::new_blocking(None).expect("device");
        assert_eq!(session.render(&globe, &globe_store, &globe_style).expect("base").sha256_hex(), base_want, "base");
        assert_eq!(
            session.render(variant, &globe_store, &globe_style).expect("variant").sha256_hex(),
            variant_want,
            "variant after base"
        );
        assert_eq!(
            session.render(&globe, &globe_store, &globe_style).expect("base2").sha256_hex(),
            base_want,
            "base after variant must not draw a stale bake"
        );
        assert_eq!(
            session.render(variant, &globe_store, &globe_style).expect("variant2").sha256_hex(),
            variant_want,
            "variant after base2"
        );
    }
    // The pixel_ratio variant shares every bake with the base by
    // construction (identical cover, identical `MeshKey` fields) -- prove
    // it actually reused them rather than coincidentally rebaking the same
    // answer.
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.render(&globe, &globe_store, &globe_style).expect("base");
    let misses = session.mesh_cache_stats().misses;
    session.render(&ratio2, &globe_store, &globe_style).expect("ratio2");
    assert_eq!(session.mesh_cache_stats().misses, misses, "pixel_ratio must reuse every one of the base's bakes");
}

/// The first Singapore scene with system fonts in place of the shipped Noto.
fn sg_system_fonts() -> (CaptureRequest, TileStore, Style) {
    let (_, mut request, store, style) = scenes().remove(0);
    request.label_fonts = None;
    (request, store, style)
}

/// A nav frame (the Manila fixture route, followed, chrome drawn) over no
/// tiles: the nav test in `capture::tests`, at a golden-scene size.
fn nav_scene() -> (CaptureRequest, TileStore, Style) {
    use crate::nav::host::{NavFrame, NavHost, SafeInsets, SimOptions, WallClock};
    const MANILA: &[u8] = include_bytes!("../../tests/fixtures/routes/manila.json");
    let base = CaptureRequest {
        center_lon: 120.98, center_lat: 14.59, zoom: 16.0, width: 400, height: 400, flat: true,
        clear: Clear::SENTINEL, backdrop: false,
        ..CaptureRequest::default()
    };
    let mut host = NavHost::new();
    host.set_demo_route(std::sync::Arc::from(MANILA), SimOptions { start_at_m: Some(700.0), paused: true, ..Default::default() });
    let mut camera = crate::camera_control::CameraController::new(
        base.camera(), crate::camera_control::MIN_ZOOM, crate::camera_control::MAX_ZOOM,
    );
    let frame_at = |now_ms: f64| NavFrame {
        now_ms,
        wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
        viewport: (400.0, 400.0),
        insets: SafeInsets::default(),
    };
    host.start_simulation(&camera, &frame_at(0.0)).expect("the Manila fixture drives");
    let _ = host.advance(&mut camera, &frame_at(0.0));
    let _ = host.advance(&mut camera, &frame_at(1000.0));
    let c = *camera.camera();
    let request = CaptureRequest {
        center_lon: c.center_lon, center_lat: c.center_lat, zoom: c.zoom, pitch: c.pitch, bearing: c.bearing,
        nav: Some(NavCapture { host: std::sync::Arc::new(host), frame: frame_at(1000.0) }),
        ..base
    };
    (request, TileStore::new(), Style { layers: Vec::new(), ..Style::default() })
}

/// Overlay labels on the globe (the overlay-label test's Houston donut
/// count, and one past the horizon), over no tiles.
fn overlay_labels_scene() -> (CaptureRequest, TileStore, Style) {
    let label = |lon: f64, lat: f64, text: &str| format!(
        r##"{{"type":"Feature","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}},"properties":{{"text-field":"{text}","text-size":28,"text-color":"#000000ff","text-halo-width":0,"text-allow-overlap":true}}}}"##
    );
    let json = format!(r#"{{"type":"FeatureCollection","features":[{},{}]}}"#, label(-95.37, 29.76, "88"), label(20.0, 20.0, "88"));
    let request = CaptureRequest {
        center_lon: -98.0, center_lat: 39.0, zoom: 2.5, width: 384, height: 384, backdrop: false,
        clear: Clear::Sentinel(crate::style::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }),
        overlay_labels: Some(json),
        ..CaptureRequest::default()
    };
    (request, TileStore::new(), Style { layers: Vec::new(), ..Style::default() })
}

/// Labels in system fonts, a Noto -> system -> Noto switch, a nav frame and
/// an overlay-label frame, all through one session between Noto frames:
/// each is the one-shot frame, and the Noto frames are still the goldens.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_session_switching_fonts_and_drawing_nav_and_overlay_labels_matches_one_shot() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (noto_name, noto_req, noto_store, noto_style) = scenes().remove(0);
    let others = [("system_fonts", sg_system_fonts()), ("nav", nav_scene()), ("overlay_labels", overlay_labels_scene())];
    let one_shot: Vec<String> = others
        .iter()
        .map(|(_, (r, t, s))| capture(r, t, s).expect("device").sha256_hex())
        .collect();
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    for ((name, (r, t, s)), want) in others.iter().zip(&one_shot) {
        let noto_shot = session.render(&noto_req, &noto_store, &noto_style).expect("render");
        assert_eq!(Some(&noto_shot.sha256_hex()), golden.get(noto_name), "{noto_name} before {name}");
        let shot = session.render(r, t, s).expect("render");
        assert_eq!(&shot.sha256_hex(), want, "{name} differs through a session");
        match *name {
            "system_fonts" => assert!(shot.labels_drawn > 0, "system-font labels drew"),
            "overlay_labels" => assert_eq!(shot.overlay_labels_drawn, 1, "the near-side overlay label drew"),
            _ => {}
        }
    }
    // The nav frame drew chrome, so it really went through the nav path.
    let (nav_req, nav_tiles, nav_style) = &others[1].1;
    assert!(nav_req.nav.is_some() && nav_req.label_fonts.is_none());
    let nav_shot = session.render(nav_req, nav_tiles, nav_style).expect("render");
    assert!(nav_shot.rgba.as_chunks::<4>().0.iter().any(|p| *p != [255, 0, 255, 255]), "the nav frame drew nothing");
    // A system-font nav frame leaves the Noto label state (and its shapes)
    // in place: the next Noto frame shapes nothing new.
    session.render(&noto_req, &noto_store, &noto_style).expect("render");
    session.render(nav_req, nav_tiles, nav_style).expect("render");
    let state = session.fonts.as_ref().expect("font state");
    assert_eq!(state.key, noto_req.label_fonts, "the nav frame evicted the Noto state");
    let shaped = state.shapes.shape_count;
    session.render(&noto_req, &noto_store, &noto_style).expect("render");
    assert_eq!(session.fonts.as_ref().expect("font state").shapes.shape_count, shaped, "Noto shapes were lost");
}

/// The session's shape cache is trimmed every frame, as the live view's is:
/// over frames whose labels change (a zoom and pan sweep), it holds at most
/// the last frame's shapes plus the expensive few, and its per-frame used
/// set is empty after every render.
#[test]
fn a_session_shape_cache_stays_bounded_over_frames_with_changing_labels() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    let (cap, expensive_kept) = (8, 2);
    session.shape_cache_caps = (cap, expensive_kept);
    let mut peak = 0;
    for i in 0..12 {
        let request = CaptureRequest {
            zoom: 14.0 + 0.2 * f64::from(i),
            center_lon: base.center_lon + 0.001 * f64::from(i),
            ..base.clone()
        };
        let store = store_for(&request);
        let shot = session.render(&request, &store, &style).expect("render");
        assert!(shot.labels_drawn > 0, "frame {i} drew labels");
        let shapes = &session.fonts.as_ref().expect("font state").shapes;
        assert!(shapes.used_this_frame.is_empty(), "frame {i}: the used set was not cleared");
        let bound = cap.max(session.last_frame_shapes_used) + expensive_kept;
        assert!(shapes.entries.len() <= bound, "frame {i}: {} entries > {bound}", shapes.entries.len());
        peak = peak.max(shapes.entries.len());
    }
    let shapes = &session.fonts.as_ref().expect("font state").shapes;
    assert!(
        shapes.shape_count > peak,
        "the sweep shaped {} labels and the cache never held more than {peak}: the trim evicted",
        shapes.shape_count
    );
}

/// [`per_frame_cost_at_1080p`] through one [`CaptureSession`] built before
/// the loop (fonts pre-warmed): the S0.1 row, and since S0.2 the mesh
/// cache's all-hit row (it prints the cache's counters).
#[test]
#[ignore = "timing; run alone in release"]
fn per_frame_cost_at_1080p_session() {
    let (_, mut request, _, style) = scenes().remove(0);
    request.width = 1920;
    request.height = 1080;
    let store = store_for(&request);
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    let mut ms = Vec::new();
    let mut drawn = 0;
    let mut labels = 0;
    for i in 0..31 {
        let start = std::time::Instant::now();
        let shot = session.render(&request, &store, &style).expect("render");
        if i > 0 { ms.push(start.elapsed().as_secs_f64() * 1000.0); }
        drawn += shot.tiles_drawn;
        labels += shot.labels_drawn;
    }
    assert!(drawn > 0, "a timing over frames that drew nothing is not a timing");
    ms.sort_by(f64::total_cmp);
    eprintln!("PER_FRAME_MS p50={:.1} p95={:.1} min={:.1} n={}", ms[ms.len() / 2], ms[ms.len() * 95 / 100], ms[0], ms.len());
    eprintln!("PER_FRAME_DRAWN tiles={drawn} labels={labels} over 31 frames");
    let m = session.mesh_cache_stats();
    eprintln!("MESH_CACHE hits={} misses={} entries={} bytes={}", m.hits, m.misses, m.entries, m.bytes);
}

/// A moving 1080p story through one session: 60 frames panning east (0.18
/// deg, about eight z14 tile widths) and
/// zooming 13.6 -> 15.4, across two integer zooms, so frames that enter new
/// tiles or a new `display_zoom` bake and the rest hit. The miss-frame cost
/// next to [`per_frame_cost_at_1080p_session`]'s all-hit one.
#[test]
#[ignore = "timing; run alone in release"]
fn per_frame_cost_at_1080p_session_pan_and_zoom() {
    let (_, base, _, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    let frames: Vec<(CaptureRequest, TileStore)> = (0..61)
        .map(|i| {
            let request = CaptureRequest {
                width: 1920,
                height: 1080,
                zoom: 13.6 + 0.03 * f64::from(i),
                center_lon: base.center_lon + 0.003 * f64::from(i),
                ..base.clone()
            };
            let store = store_for(&request);
            (request, store)
        })
        .collect();
    let mut ms = Vec::new();
    let mut drawn = 0;
    let mut bake_frames = 0;
    for (i, (request, store)) in frames.iter().enumerate() {
        let misses = session.mesh_cache_stats().misses;
        let start = std::time::Instant::now();
        let shot = session.render(request, store, &style).expect("render");
        if i > 0 {
            ms.push(start.elapsed().as_secs_f64() * 1000.0);
            bake_frames += usize::from(session.mesh_cache_stats().misses > misses);
        }
        drawn += shot.tiles_drawn;
    }
    assert!(drawn > 0, "a timing over frames that drew nothing is not a timing");
    let m = session.mesh_cache_stats();
    ms.sort_by(f64::total_cmp);
    eprintln!(
        "PAN_ZOOM_MS p50={:.1} p95={:.1} max={:.1} n={} frames_that_baked={bake_frames}",
        ms[ms.len() / 2], ms[ms.len() * 95 / 100], ms[ms.len() - 1], ms.len()
    );
    eprintln!(
        "MESH_CACHE hits={} misses={} hit_rate={:.2} entries={} bytes={}",
        m.hits, m.misses, m.hits as f64 / (m.hits + m.misses) as f64, m.entries, m.bytes
    );
}

/// [`per_frame_cost_at_1080p_session`] with the label shape cache emptied
/// before every frame (outside the timing): the same scene, tiles and
/// labels, but every label shaped cold, as a moving story's new strings
/// and sizes are. What the session saves here is construction alone.
#[test]
#[ignore = "timing; run alone in release"]
fn per_frame_cost_at_1080p_session_cold_labels() {
    let (_, mut request, _, style) = scenes().remove(0);
    request.width = 1920;
    request.height = 1080;
    let store = store_for(&request);
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    let mut ms = Vec::new();
    let (mut drawn, mut labels, mut shaped) = (0, 0, 0);
    for i in 0..31 {
        session.fonts.as_mut().expect("pre-warmed").shapes =
            crate::symbol_pass::LabelShapeCache::with_default_family("Noto Sans");
        let start = std::time::Instant::now();
        let shot = session.render(&request, &store, &style).expect("render");
        if i > 0 { ms.push(start.elapsed().as_secs_f64() * 1000.0); }
        drawn += shot.tiles_drawn;
        labels += shot.labels_drawn;
        shaped += session.fonts.as_ref().expect("font state").shapes.shape_count;
    }
    assert!(drawn > 0 && shaped > 0, "a timing over frames that drew or shaped nothing is not a timing");
    ms.sort_by(f64::total_cmp);
    eprintln!("PER_FRAME_MS p50={:.1} p95={:.1} min={:.1} n={}", ms[ms.len() / 2], ms[ms.len() * 95 / 100], ms[0], ms.len());
    eprintln!("PER_FRAME_DRAWN tiles={drawn} labels={labels} shaped={shaped} over 31 frames");
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_b_a_through_one_session_gives_hash_a_twice() {
    let _refused = refused_lock();
    let scenes = scenes();
    let (a, b) = (&scenes[0], &scenes[1]);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let a1 = session.render(&a.1, &a.2, &a.3).expect("a1").sha256_hex();
    let _ = session.render(&b.1, &b.2, &b.3).expect("b");
    let a2 = session.render(&a.1, &a.2, &a.3).expect("a2").sha256_hex();
    assert_eq!(a1, a2);
    assert!(session.mesh_cache_stats().hits > 0, "the second A must hit the cache");
    assert_eq!(Some(&a1), read_golden().get(a.0));
}

#[test]
fn a_new_style_generation_never_draws_an_old_mesh() {
    let _refused = refused_lock();
    let (_, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let before = session.render(&request, &store, &style).expect("gen 0").sha256_hex();
    // Generation 1 is a style with the water layer hidden: a different frame.
    let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap()).unwrap();
    for layer in doc["layers"].as_array_mut().unwrap() {
        if layer["id"] == "water" { layer["layout"]["visibility"] = "none".into(); }
    }
    let edited = crate::style_spec::parse_and_compile(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let gen1 = CaptureRequest { style_generation: 1, ..request.clone() };
    let after = session.render(&gen1, &store, &edited).expect("gen 1").sha256_hex();
    let fresh = capture(&gen1, &store, &edited).expect("fresh").sha256_hex();
    assert_ne!(before, after, "hiding water must change the frame");
    assert_eq!(after, fresh, "a cached gen-0 mesh leaked into gen 1");
}

/// A bake at one zoom is never drawn at another: the session goes
/// z14 -> z15 -> z14 over the same tile ids (the fixture tile is at every
/// id), and each frame is the one-shot frame. The z15 frame shares tile ids
/// with the z14 cover's children only through the key's `display_zoom`,
/// so a key without it would draw z14's plan at z15.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_mesh_baked_at_one_zoom_is_never_drawn_at_another() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let at = |zoom: f64| {
        let request = CaptureRequest { zoom, ..base.clone() };
        let store = store_for(&request);
        (request, store)
    };
    let (z14, z14_store) = at(14.5);
    let (z15, z15_store) = at(15.5);
    // The same tile ids under both zooms: the z15 cover is overzoomed
    // (flat_tile_zoom_max 14), so both frames ask the z14 ids.
    assert_eq!(z14.flat_tile_zoom(), z15.flat_tile_zoom());
    let shared = z14.cover().iter().filter(|t| z15.cover().contains(t)).count();
    assert!(shared > 0, "the two frames must share tile ids for this test to bite");
    let want15 = capture(&z15, &z15_store, &style).expect("one-shot").sha256_hex();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let a = session.render(&z14, &z14_store, &style).expect("z14").sha256_hex();
    let b = session.render(&z15, &z15_store, &style).expect("z15").sha256_hex();
    assert_eq!(b, want15, "a z14 bake was drawn at z15");
    assert_ne!(a, b);
    let a2 = session.render(&z14, &z14_store, &style).expect("z14 again").sha256_hex();
    assert_eq!(a, a2);
    assert_eq!(Some(&a), read_golden().get("sg_z14_flat_labels"));
}

#[test]
fn a_sixty_frame_orbit_hits_the_cache_and_prints_the_rate() {
    let _refused = refused_lock();
    let (_, mut request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    for i in 0..60 {
        request.bearing = (i as f32 * 1.5).to_radians();
        let shot = session.render(&request, &store, &style).expect("frame");
        assert!(shot.tiles_drawn > 0);
    }
    let s = session.mesh_cache_stats();
    eprintln!("MESH_CACHE hits={} misses={} entries={} bytes={}", s.hits, s.misses, s.entries, s.bytes);
    assert!(s.hits as f64 / (s.hits + s.misses) as f64 > 0.8, "an orbit re-uses almost every tile");
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_tiny_budget_evicts_and_stays_correct() {
    let _refused = refused_lock();
    let golden = read_golden();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.set_mesh_cache_budget(1);
    for (name, request, store, style) in scenes() {
        assert_eq!(Some(&session.render(&request, &store, &style).expect("r").sha256_hex()), golden.get(name));
    }
    assert!(session.mesh_cache_stats().bytes <= 1 || session.mesh_cache_stats().entries <= 1);
}

/// Over a long pan-and-zoom sweep that keeps asking new tiles, the cache
/// never holds more than its budget -- or, when one bake alone is over it,
/// that one bake (the entry just inserted is never evicted) -- and it did
/// evict: it saw more than it kept.
#[test]
fn the_mesh_cache_stays_bounded_over_a_long_run() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    // Measure one frame's bakes, then budget for about two frames' worth.
    let first = store_for(&base);
    session.render(&base, &first, &style).expect("probe");
    let one_frame = session.mesh_cache_stats();
    assert!(one_frame.entries > 0 && one_frame.bytes > 0);
    let budget = one_frame.bytes * 2;
    session.set_mesh_cache_budget(budget);
    let mut peak = 0;
    for i in 0..40 {
        let request = CaptureRequest {
            center_lon: base.center_lon + 0.02 * f64::from(i),
            zoom: 13.5 + 0.05 * f64::from(i % 20),
            ..base.clone()
        };
        let store = store_for(&request);
        session.render(&request, &store, &style).expect("render");
        let s = session.mesh_cache_stats();
        let largest = session.mesh_cache_largest_entry();
        assert!(s.bytes <= budget.max(largest), "frame {i}: {} bytes > max(budget {budget}, largest entry {largest})", s.bytes);
        peak = peak.max(s.entries);
    }
    let s = session.mesh_cache_stats();
    assert!(s.misses as usize > peak, "the sweep baked {} tiles and never held more than {peak}: nothing was evicted", s.misses);
}

/// `bytes` (a raw MVT tile) with every layer named `drop` left out, at the
/// protobuf level: a real second source's bytes for the same tile id.
fn tile_bytes_without_layer(bytes: &[u8], drop: &str) -> Vec<u8> {
    fn varint(b: &[u8], at: &mut usize) -> u64 {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let byte = b[*at];
            *at += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 { return v; }
            shift += 7;
        }
    }
    let layer_name = |layer: &[u8]| -> String {
        let mut at = 0;
        while at < layer.len() {
            let key = varint(layer, &mut at);
            match key & 7 {
                0 => { varint(layer, &mut at); }
                1 => at += 8,
                5 => at += 4,
                2 => {
                    let len = varint(layer, &mut at) as usize;
                    if key >> 3 == 1 { return String::from_utf8_lossy(&layer[at..at + len]).into_owned(); }
                    at += len;
                }
                w => panic!("wire type {w}"),
            }
        }
        String::new()
    };
    let (mut out, mut at) = (Vec::new(), 0);
    while at < bytes.len() {
        let start = at;
        let key = varint(bytes, &mut at);
        assert_eq!(key, (3 << 3) | 2, "a tile is only layers");
        let len = varint(bytes, &mut at) as usize;
        let body = &bytes[at..at + len];
        at += len;
        if layer_name(body) != drop { out.extend_from_slice(&bytes[start..at]); }
    }
    out
}

/// A source switch -- different tile data under the same ids, at the SAME
/// style generation -- never draws the old source's bakes: the key carries
/// the tile's data fingerprint.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_source_switch_under_the_same_ids_and_generation_draws_the_new_data() {
    let _refused = refused_lock();
    let (name, request, store, style) = scenes().remove(0);
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/singapore_z14.pbf")).expect("fixture");
    let other = crate::mvt::decode(tile_bytes_without_layer(&raw, "water")).expect("stripped tile decodes");
    assert!(other.layers.len() < fixture_decoded().layers.len(), "the fixture has a water layer to drop");
    let other_store: TileStore = store.keys().map(|&id| (id, other.clone())).collect();
    let want = capture(&request, &other_store, &style).expect("one-shot").sha256_hex();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let a = session.render(&request, &store, &style).expect("source A").sha256_hex();
    assert_eq!(Some(&a), read_golden().get(name));
    let b = session.render(&request, &other_store, &style).expect("source B").sha256_hex();
    assert_ne!(a, b, "dropping water must change the frame");
    assert_eq!(b, want, "source A's bakes were drawn for source B");
    // A hand-built tile (no decode hash) is told apart by its content.
    let mut edited = fixture_decoded();
    edited.data_hash = 0;
    edited.layers.retain(|l| l.name != "water");
    let edited_store: TileStore = store.keys().map(|&id| (id, edited.clone())).collect();
    let c = session.render(&request, &edited_store, &style).expect("hand-built").sha256_hex();
    assert_eq!(c, capture(&request, &edited_store, &style).expect("one-shot").sha256_hex());
}

/// Every request switch a bake reads is in the key: for each, a fresh
/// session renders the variant, the base and the variant again. Every frame
/// is the one-shot frame, and the base after the variant bakes anew (a
/// switch the key ignored would hit the variant's bakes instead).
#[test]
fn every_key_field_toggled_through_one_session_matches_one_shot() {
    let _refused = refused_lock();
    let mut all = scenes();
    // Subdivision is `NONE` at z14 already; the z3 globe is where it splits.
    let (_, globe, globe_store, globe_style) = all.remove(3);
    let (_, base, store, style) = all.remove(0);
    let cases: Vec<(&str, &CaptureRequest, CaptureRequest, &TileStore, &Style)> = vec![
        ("labels", &base, CaptureRequest { labels: false, ..base.clone() }, &store, &style),
        ("paint", &base, CaptureRequest { paint: Paint::Wash, ..base.clone() }, &store, &style),
        ("compact", &base, CaptureRequest { basemap_compact: !base.basemap_compact, ..base.clone() }, &store, &style),
        (
            "subdivision",
            &globe,
            CaptureRequest { subdivision: SubdivisionPolicy::None, ..globe.clone() },
            &globe_store,
            &globe_style,
        ),
    ];
    for (field, base, variant, store, style) in &cases {
        let (base, variant, store, style): (&CaptureRequest, &CaptureRequest, &TileStore, &Style) = (base, variant, store, style);
        let base_want = capture(base, store, style).expect("one-shot").sha256_hex();
        let want = capture(variant, store, style).expect("one-shot").sha256_hex();
        let mut session = CaptureSession::new_blocking(None).expect("device");
        assert_eq!(session.render(variant, store, style).expect("v").sha256_hex(), want, "{field}: variant");
        let misses = session.mesh_cache_stats().misses;
        assert_eq!(session.render(base, store, style).expect("b").sha256_hex(), base_want, "{field}: base after variant");
        assert!(session.mesh_cache_stats().misses > misses, "{field}: the base hit the variant's bakes");
        assert_eq!(session.render(variant, store, style).expect("v2").sha256_hex(), want, "{field}: variant after base");
    }
}

/// A style edited without a new `style_generation` fails the debug-build
/// check on the first cache hit, instead of drawing a stale mesh.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "style changed without a new style_generation")]
fn a_style_edit_without_a_generation_bump_fails_in_debug() {
    let _refused = refused_lock();
    let (_, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.render(&request, &store, &style).expect("gen 0");
    let mut edited = style;
    edited.layers.pop();
    let _ = session.render(&request, &store, &edited);
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn pipelined_output_equals_synchronous_output() {
    let _refused = refused_lock();
    let golden = read_golden();
    let scenes = scenes();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let mut got = Vec::new();
    for (_, request, store, style) in &scenes {
        if let Some(c) = session.render_pipelined(request, store, style).expect("submit") { got.push(c.sha256_hex()); }
    }
    got.extend(session.flush().expect("flush").into_iter().map(|c| c.sha256_hex()));
    let want: Vec<_> = scenes.iter().map(|s| golden[s.0].clone()).collect();
    assert_eq!(got, want);
}

#[test]
fn the_ring_returns_nothing_for_two_frames_then_the_oldest_and_flush_empties_it() {
    let _refused = refused_lock();
    let (_, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    assert!(session.render_pipelined(&request, &store, &style).expect("1").is_none());
    assert!(session.render_pipelined(&request, &store, &style).expect("2").is_none());
    let third = session.render_pipelined(&request, &store, &style).expect("3").expect("frame 1 comes back on call 3");
    assert!(third.tiles_drawn > 0 && !third.rgba.is_empty());
    assert_eq!(session.flush().expect("flush").len(), 2);
    assert!(session.flush().expect("flush again").is_empty());
    // A synchronous frame after a drained ring is the ordinary frame.
    assert_eq!(session.render(&request, &store, &style).expect("sync").sha256_hex(), third.sha256_hex());
}

#[test]
fn an_invalid_target_size_is_an_err_not_a_panic() {
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.catch_device_errors();
    let request = CaptureRequest { width: 20_000, height: 16, ..CaptureRequest::default() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.render(&request, &TileStore::new(), &Style::default())));
    assert!(matches!(result, Ok(Err(_))), "want Ok(Err(..)), got a panic or a frame");
}

/// The size check, not the handler: refused before any texture exists,
/// with both numbers, and without `catch_device_errors`.
#[test]
fn an_oversized_target_is_refused_by_name_before_wgpu_sees_it() {
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let request = CaptureRequest { width: 20_000, height: 16, ..CaptureRequest::default() };
    let err = session.render(&request, &TileStore::new(), &Style::default()).err().expect("an Err").to_string();
    assert!(err.contains("20000x16") && err.contains(&session.ctx.device.limits().max_texture_dimension_2d.to_string()), "{err}");
}

/// The handler, not the size check: a validation error recorded into the
/// frame's own command buffer fails that frame with `device error: ..`
/// instead of wgpu's panic, and the session draws the next frame normally.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn an_injected_validation_error_fails_the_frame_and_the_next_frame_recovers() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (name, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.catch_device_errors();
    session.inject_validation_error = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.render(&request, &store, &style)));
    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("a spoiled frame came back as a frame"),
        Err(_) => panic!("a spoiled frame panicked"),
    };
    assert!(err.starts_with("device error: "), "want a device error, got {err}");
    session.inject_validation_error = false;
    let shot = session.render(&request, &store, &style).expect("the next frame");
    assert_eq!(Some(&shot.sha256_hex()), golden.get(name), "the frame after an error differs");
}

/// Without `catch_device_errors` the session keeps wgpu's panicking
/// default, so a test's validation error stays loud (spec 4.2).
#[test]
fn without_catch_device_errors_a_validation_error_still_panics() {
    let _refused = refused_lock();
    let (_, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.inject_validation_error = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.render(&request, &store, &style)));
    assert!(result.is_err(), "the default handler no longer panics");
}

/// Per-frame cost through the ring next to the synchronous session, both
/// all-hit 1080p frames of `sg_z14_flat_labels`. Run alone, in release.
#[test]
#[ignore = "timing; run alone in release"]
fn per_frame_cost_at_1080p_session_pipelined() {
    let (_, mut request, _, style) = scenes().remove(0);
    request.width = 1920;
    request.height = 1080;
    let store = store_for(&request);
    let mut session = CaptureSession::new_blocking(noto()).expect("device");
    let n = 60;
    // Warm the caches.
    for _ in 0..3 {
        session.render(&request, &store, &style).expect("warm");
    }
    for round in 0..3 {
        let start = std::time::Instant::now();
        let mut drawn = 0;
        for _ in 0..n {
            drawn += session.render(&request, &store, &style).expect("sync").tiles_drawn;
        }
        let sync_ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        let start = std::time::Instant::now();
        let mut frames = 0;
        for _ in 0..n {
            if let Some(c) = session.render_pipelined(&request, &store, &style).expect("pipelined") {
                drawn += c.tiles_drawn;
                frames += 1;
            }
        }
        for c in session.flush().expect("flush") {
            drawn += c.tiles_drawn;
            frames += 1;
        }
        let ring_ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        assert_eq!(frames, n);
        assert!(drawn > 0, "a timing over frames that drew nothing is not a timing");
        eprintln!("PIPELINED_MS round={round} sync={sync_ms:.2} ring={ring_ms:.2} n={n}");
    }
}

/// Row padding through the ring: 333 px is 1332 B a row, padded to 1536
/// by the copy, so `read_back` must strip 204 B from every row. Every
/// golden width is a multiple of 64 px and never pads.
#[test]
fn the_ring_at_a_padded_width_equals_the_synchronous_frame_byte_for_byte() {
    let _refused = refused_lock();
    let (_, mut request, _, style) = scenes().remove(0);
    request.width = 333;
    request.height = 201;
    let store = store_for(&request);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let sync = session.render(&request, &store, &style).expect("sync");
    assert_eq!(sync.rgba.len(), 333 * 201 * 4);
    let mut ring = Vec::new();
    for _ in 0..4 {
        ring.extend(session.render_pipelined(&request, &store, &style).expect("submit"));
    }
    ring.extend(session.flush().expect("flush"));
    assert_eq!(ring.len(), 4);
    for (i, frame) in ring.iter().enumerate() {
        assert_eq!((frame.width, frame.height), (333, 201));
        assert!(frame.rgba == sync.rgba, "ring frame {i} differs from the synchronous frame");
    }
}

/// A spoiled frame submitted through the ring fails that
/// `render_pipelined` call; the frames queued before it still come back
/// intact from `flush`, and the ring keeps working.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_validation_error_in_the_ring_fails_render_pipelined_and_the_ring_recovers() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (name, request, store, style) = scenes().remove(0);
    let want = golden.get(name).expect("blessed").clone();
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.catch_device_errors();
    assert!(session.render_pipelined(&request, &store, &style).expect("1").is_none());
    assert!(session.render_pipelined(&request, &store, &style).expect("2").is_none());
    session.inject_validation_error = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.render_pipelined(&request, &store, &style)));
    match result {
        Ok(Err(e)) => assert!(e.to_string().starts_with("device error: "), "want a device error, got {e}"),
        Ok(Ok(_)) => panic!("a spoiled frame was queued as a frame"),
        Err(_) => panic!("a spoiled frame panicked"),
    }
    session.inject_validation_error = false;
    let queued: Vec<_> = session.flush().expect("the two good frames").iter().map(Capture::sha256_hex).collect();
    assert_eq!(queued, vec![want.clone(), want.clone()]);
    let mut again = Vec::new();
    for _ in 0..3 {
        again.extend(session.render_pipelined(&request, &store, &style).expect("after the error").map(|c| c.sha256_hex()));
    }
    again.extend(session.flush().expect("flush").iter().map(Capture::sha256_hex));
    assert_eq!(again, vec![want; 3]);
}

/// An error that lands after a frame's post-submit take surfaces from
/// `flush`'s wait, drops the rest of the ring, and the session renders on.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_late_device_error_fails_flush_and_the_session_recovers() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (name, request, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    session.catch_device_errors();
    assert!(session.render_pipelined(&request, &store, &style).expect("1").is_none());
    assert!(session.render_pipelined(&request, &store, &style).expect("2").is_none());
    session.inject_late_device_error("lost after submit");
    let err = session.flush().err().expect("flush fails").to_string();
    assert_eq!(err, "device error: lost after submit");
    assert!(session.flush().expect("an emptied ring").is_empty());
    let shot = session.render(&request, &store, &style).expect("the next frame");
    assert_eq!(Some(&shot.sha256_hex()), golden.get(name));
}

/// Buildings through the session's mesh cache (merge of master abcce6c):
/// a frame that extrudes and one that does not share every other key
/// field, so the key's `extrudes` is all that keeps a flat bake (no
/// building buffers) off a frame that should raise walls, and a bake with
/// buildings off one that should not. Shinjuku, Liberty, pitch 60: the
/// walls frame (opacity 1), then the same camera with the buildings'
/// opacity forced to 0 (the extrusion plan is empty, nothing is
/// tessellated), then walls again -- each frame the one-shot frame.
/// `GPUI_MAP_SESSION_BUILDINGS_PNG=<path>` saves the walls frame to look at.
#[test]
fn session_buildings_never_reuse_a_flat_bake_and_vice_versa() {
    let style = liberty();
    let request = |opacity: f32| CaptureRequest {
        center_lon: 139.6995,
        center_lat: 35.6905,
        zoom: 16.0,
        pitch: 60f32.to_radians(),
        bearing: 30f32.to_radians(),
        width: 512,
        height: 384,
        flat: true,
        extrusion_opacity: Some(opacity),
        ..CaptureRequest::default()
    };
    let mut store = TileStore::new();
    store.insert(
        TileId { z: 14, x: 14549, y: 6451 },
        crate::mvt::decode(std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/benches/fixtures/shinjuku_z14.pbf")).unwrap())
            .unwrap(),
    );
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let walls = session.render(&request(1.0), &store, &style).expect("walls");
    let none = session.render(&request(0.0), &store, &style).expect("no walls");
    let walls_again = session.render(&request(1.0), &store, &style).expect("walls again");
    if let Ok(path) = std::env::var("GPUI_MAP_SESSION_BUILDINGS_PNG") {
        walls.save_png(std::path::Path::new(&path)).expect("save");
    }
    let one_shot = |opacity: f32| capture(&request(opacity), &store, &style).expect("one shot").sha256_hex();
    assert_eq!(walls.sha256_hex(), one_shot(1.0), "the session's walls frame is not the one-shot frame");
    assert_eq!(none.sha256_hex(), one_shot(0.0), "a bake with buildings leaked into a frame without them");
    assert_eq!(walls_again.sha256_hex(), walls.sha256_hex(), "a flat bake leaked into a frame with buildings");
    let (differing, _) = walls.diff(&none, 2);
    assert!(differing > 512 * 384 / 50, "walls must change at least 2% of the frame: {differing} px");
    let stats = session.mesh_cache_stats();
    assert_eq!((stats.misses, stats.hits), (2, 1), "one bake per extrudes value, the third frame a hit: {stats:?}");
}

/// Merge-2 carry: the altitude pipeline is built once per session and
/// reused. Two different point sets through one session, synchronous and
/// pipelined, must each equal a fresh one-shot capture of the same request
/// (a stale instance buffer or uniform would show as a hash difference).
#[test]
fn a_session_reuses_the_altitude_pipeline_and_every_frame_matches_one_shot() {
    use crate::render::altitude::GeoPoint;
    let _refused = refused_lock();
    let mut style = crate::render::altitude_pipeline::AltitudeStyle::default();
    style.palette[0] = Rgba { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
    style.base_px = [8.0; 16];
    let request = |lon0: f64, n: usize| CaptureRequest {
        center_lon: 10.0,
        center_lat: 40.0,
        zoom: 3.0,
        width: 256,
        height: 256,
        altitude: Some(AltitudeCapture {
            points: (0..n).map(|i| GeoPoint { lon: lon0 + i as f64 * 3.0, lat: 35.0 + i as f64, alt_m: 400_000.0 }).collect(),
            style: style.clone(),
            ..Default::default()
        }),
        ..CaptureRequest::default()
    };
    let (a, b) = (request(0.0, 6), request(15.0, 3));
    let empty = Style { layers: Vec::new(), ..Style::default() };
    let tiles = TileStore::new();
    let one_shot = |r: &CaptureRequest| capture(r, &tiles, &empty).expect("device");
    let (want_a, want_b) = (one_shot(&a), one_shot(&b));
    assert!(want_a.altitude_points_drawn > 0 && want_b.altitude_points_drawn > 0, "the frames draw points");
    assert_ne!(want_a.sha256_hex(), want_b.sha256_hex(), "the two point sets draw differently");
    let mut session = CaptureSession::new_blocking(None).expect("device");
    for (r, want) in [(&a, &want_a), (&b, &want_b), (&a, &want_a)] {
        assert_eq!(session.render(r, &tiles, &empty).expect("frame").sha256_hex(), want.sha256_hex());
    }
    let mut got = Vec::new();
    for r in [&b, &a, &b] {
        got.extend(session.render_pipelined(r, &tiles, &empty).expect("submit").map(|c| c.sha256_hex()));
    }
    got.extend(session.flush().expect("flush").into_iter().map(|c| c.sha256_hex()));
    assert_eq!(got, vec![want_b.sha256_hex(), want_a.sha256_hex(), want_b.sha256_hex()]);
}

/// Intersection over union of "pixels the mark changed" (`|shot - without|`
/// above 8 in any channel) and the vendored artwork's alpha (above 128),
/// resized to the mark's footprint `(x, y, w, h)` -- spec §8.5 step 6. The
/// artwork is the one `brand_mark::resolve_for_frame` picks for the frame;
/// the two variants' alpha channels are byte-identical (asserted below and
/// in `brand_mark`), so either decodes to the same mask.
/// Writes `shot` to `$GPUI_MAP_IOU_DUMP/name` when that is set, so the
/// burn-in can be looked at, not only counted.
fn dump_for_eyes(shot: &Capture, name: &str) {
    if let Ok(dir) = std::env::var("GPUI_MAP_IOU_DUMP") {
        if let Some(img) = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone()) {
            let _ = img.save(format!("{dir}/{name}"));
        }
    }
}

fn iou_changed_vs_alpha(shot: &Capture, without: &Capture, footprint: (f32, f32, f32, f32)) -> f64 {
    iou_changed_vs_alpha_at(shot, without, footprint, 8)
}

fn iou_changed_vs_alpha_at(shot: &Capture, without: &Capture, footprint: (f32, f32, f32, f32), threshold: u8) -> f64 {
    let (x, y, w, h) = footprint;
    let (w_px, h_px) = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
    let artwork = crate::brand_mark::decode(crate::brand_mark::SWAT_ON_LIGHT_PNG).expect("vendored artwork decodes");
    let alpha = image::imageops::resize(&artwork, w_px, h_px, image::imageops::FilterType::Triangle);
    let (x0, y0) = (x.round() as u32, y.round() as u32);
    let (mut both, mut either) = (0u64, 0u64);
    for py in 0..h_px {
        for px in 0..w_px {
            let (sx, sy) = (x0 + px, y0 + py);
            if sx >= shot.width || sy >= shot.height {
                continue;
            }
            let (a, b) = (shot.pixel(sx, sy), without.pixel(sx, sy));
            let changed = (0..3).any(|c| a[c].abs_diff(b[c]) > threshold);
            let inked = alpha.get_pixel(px, py)[3] > 128;
            both += u64::from(changed && inked);
            either += u64::from(changed || inked);
        }
    }
    if let Ok(dir) = std::env::var("GPUI_MAP_IOU_DUMP") {
        let mut changed = image::GrayImage::new(w_px, h_px);
        let mut inked = image::GrayImage::new(w_px, h_px);
        for py in 0..h_px {
            for px in 0..w_px {
                if x0 + px >= shot.width || y0 + py >= shot.height {
                    continue;
                }
                let (a, b) = (shot.pixel(x0 + px, y0 + py), without.pixel(x0 + px, y0 + py));
                let d = (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
                changed.put_pixel(px, py, image::Luma([d.saturating_mul(4)]));
                inked.put_pixel(px, py, image::Luma([alpha.get_pixel(px, py)[3]]));
            }
        }
        let _ = changed.save(format!("{dir}/iou-changed.png"));
        let _ = inked.save(format!("{dir}/iou-alpha.png"));
        eprintln!("footprint {footprint:?}, both {both}, either {either}");
    }
    both as f64 / either.max(1) as f64
}

#[test]
fn the_burn_in_draws_the_credit_bottom_right_and_the_mark_bottom_left() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let req = CaptureRequest {
        width: 1920,
        height: 1080,
        attribution: Some(crate::story::burn_in::OFM_CREDIT.into()),
        brand_mark: Some(crate::brand_mark::BrandMark::default()),
        ..base
    };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    let ink = crate::symbol_pass::ATTRIBUTION_TEXT_COLOR;
    let target = [(ink.r * 255.0).round() as u8, (ink.g * 255.0).round() as u8, (ink.b * 255.0).round() as u8];
    let dark = (1920 - 300..1920)
        .flat_map(|x| (1080 - 30..1080).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let p = shot.pixel(x, y);
            (0..3).all(|c| p[c].abs_diff(target[c]) <= 24)
        })
        .count();
    assert!(dark > 50, "credit ink pixels: {dark}");
    dump_for_eyes(&shot, "burn-in-1080p.png");
    // The mark: IoU of "pixels changed by the mark" against the vendored PNG's alpha, at the footprint.
    let without = capture(&CaptureRequest { brand_mark: None, ..req.clone() }, &store, &style).unwrap();
    let (x, y, w, h) = crate::brand_mark::BrandMark::default().footprint(1080.0);
    // The IoU decodes the on-light artwork whichever variant drew: sound
    // only because the two share their alpha exactly.
    let alpha = |png| crate::brand_mark::decode(png).unwrap().pixels().map(|p| p.0[3]).collect::<Vec<u8>>();
    assert_eq!(alpha(crate::brand_mark::SWAT_ON_LIGHT_PNG), alpha(crate::brand_mark::SWAT_ON_DARK_PNG));
    // The brief's literal measure (changed > 8 against alpha > 128) is
    // antialiasing-bound at an 18 px mark: its 1-2 px strokes are mostly
    // edge, and an edge pixel at 10% coverage already changes by > 8. It
    // measured 0.668 here (0.807 at pixel ratio 2), so it is printed, not
    // asserted. The asserted IoU thresholds BOTH masks at 50% coverage:
    // alpha > 128 against a change above half the mark's full-ink change.
    let fp = (x, y, w, h);
    let literal = iou_changed_vs_alpha(&shot, &without, fp);
    let full_ink = (x.round() as u32..(x + w).round() as u32)
        .flat_map(|px| (y.round() as u32..(y + h).round() as u32).map(move |py| (px, py)))
        .map(|(px, py)| {
            let (a, b) = (shot.pixel(px, py), without.pixel(px, py));
            (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let iou = iou_changed_vs_alpha_at(&shot, &without, fp, full_ink / 2);
    let shifted = iou_changed_vs_alpha_at(&shot, &without, (x + 1.0, y, w, h), full_ink / 2);
    eprintln!("brand mark IoU {iou:.3} at 50% coverage (full ink {full_ink}); literal > 8: {literal:.3}; shifted 1 px: {shifted:.3}");
    assert!(iou > 0.9, "brand mark IoU {iou:.3}");
    assert!(shifted < iou - 0.2, "the IoU cannot tell a 1 px shift: {shifted:.3} vs {iou:.3}");
    // And nothing else moved, except a label the mark evicts: the mark is
    // a label obstacle (`corner_chrome`'s `obstacles`), so a label whose
    // box reaches its footprint is dropped. Since the bottom row moved to
    // its own inset (2026-10-08, 6 px then 9) this frame has one: a street name along
    // the bottom edge, 50..191 x 1064..1079, that crosses the mark's
    // padded box. Every other pixel outside the footprint agrees, and the
    // changed ones form one label-sized run that touches the footprint.
    let outside: Vec<(u32, u32)> = (0..1920u32)
        .flat_map(|x| (0..1080u32).map(move |y| (x, y)))
        .filter(|&(px, py)| {
            let inside = (px as f32) >= x - 1.0 && (px as f32) <= x + w + 1.0 && (py as f32) >= y - 1.0 && (py as f32) <= y + h + 1.0;
            !inside && shot.pixel(px, py) != without.pixel(px, py)
        })
        .collect();
    if !outside.is_empty() {
        let (l, t, r, bt) = outside.iter().fold((u32::MAX, u32::MAX, 0, 0), |a, p| (a.0.min(p.0), a.1.min(p.1), a.2.max(p.0), a.3.max(p.1)));
        eprintln!("the mark evicts a label: changed {l}..{r} x {t}..{bt} outside its footprint {:?}", (x, y, w, h));
        let pad = crate::symbol_pass::SYMBOL_COLLISION_PADDING_LOGICAL_PX;
        assert!(
            (l as f32) <= x + w + pad && (t as f32) <= y + h + pad && (bt as f32) >= y - pad && bt - t <= 24 && r - l <= 400,
            "the mark changed pixels outside its footprint that no evicted label explains: {l}..{r} x {t}..{bt}"
        );
    }
}

/// A user wordmark draws beside the SWAT mark under
/// [`crate::brand_mark::BrandMarkPolicy::Always`], and the mark still
/// draws where it did: the two share the icon pipeline's one vertex
/// buffer, and two separate draws once put the mark's texture at the
/// wordmark's quad (the mark vanished from its corner).
///
/// `Always` is explicit here on purpose: the default,
/// [`crate::brand_mark::BrandMarkPolicy::Auto`], now skips the mark on
/// exactly this frame (`auto_skips_the_mark_when_a_wordmark_is_set`,
/// below) -- this test is the one still proving the geometry the "beside"
/// case draws for a host that asks for it.
#[test]
fn a_user_wordmark_draws_beside_the_mark_and_leaves_the_mark_where_it_was() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let req = CaptureRequest {
        width: 640,
        height: 360,
        attribution: Some(crate::story::burn_in::OFM_CREDIT.into()),
        brand_mark: Some(crate::brand_mark::BrandMark::default()),
        brand_mark_policy: crate::brand_mark::BrandMarkPolicy::Always,
        ..base
    };
    let store = store_for(&req);
    let mark_only = capture(&req, &store, &style).unwrap();
    let art = std::sync::Arc::new(image::RgbaImage::from_pixel(80, 20, image::Rgba([255, 0, 255, 255])));
    let with = capture(&CaptureRequest { wordmark: Some(art.into()), ..req.clone() }, &store, &style).unwrap();
    dump_for_eyes(&with, "wordmark-beside-mark.png");
    let (x, y, w, h) = crate::brand_mark::BrandMark::default().footprint(360.0);
    for py in y.floor() as u32..(y + h).ceil() as u32 {
        for px in x.floor() as u32..(x + w).ceil() as u32 {
            assert_eq!(with.pixel(px, py), mark_only.pixel(px, py), "the mark's pixel ({px}, {py}) changed");
        }
    }
    let magenta = (0..640u32)
        .flat_map(|px| (0..360u32).map(move |py| (px, py)))
        .filter(|&(px, py)| {
            let p = with.pixel(px, py);
            p[0] > 240 && p[1] < 16 && p[2] > 240
        })
        .collect::<Vec<_>>();
    assert!(magenta.len() > 400, "wordmark pixels: {}", magenta.len());
    let min_x = magenta.iter().map(|p| p.0).min().unwrap();
    assert!(min_x as f32 > x + w, "the wordmark starts at x = {min_x}, inside the mark (ends {})", x + w);
}

/// The default policy ([`crate::brand_mark::BrandMarkPolicy::Auto`]):
/// the same request as above, minus the explicit `Always`, draws the
/// wordmark ALONE in the mark's corner -- no SWAT mark pixels at all, and
/// the wordmark sits where the mark's own footprint would have been
/// (`capture::chrome::wordmark_quad`'s documented "without a mark" slot).
#[test]
fn auto_skips_the_mark_when_a_wordmark_is_set() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let req = CaptureRequest {
        width: 640,
        height: 360,
        attribution: Some(crate::story::burn_in::OFM_CREDIT.into()),
        brand_mark: Some(crate::brand_mark::BrandMark::default()),
        // Auto is the default; stated for the reader rather than relied on.
        brand_mark_policy: crate::brand_mark::BrandMarkPolicy::Auto,
        ..base
    };
    assert_eq!(req.effective_brand_mark(), Some(crate::brand_mark::BrandMark::default()), "no wordmark yet: mark on");
    let art = std::sync::Arc::new(image::RgbaImage::from_pixel(80, 20, image::Rgba([255, 0, 255, 255])));
    let with_wordmark = CaptureRequest { wordmark: Some(art.into()), ..req.clone() };
    assert_eq!(with_wordmark.effective_brand_mark(), None, "a wordmark is now set: Auto skips the mark");

    let store = store_for(&with_wordmark);
    let frame = capture(&with_wordmark, &store, &style).unwrap();
    dump_for_eyes(&frame, "wordmark-alone-auto.png");
    let (x, y, w, h) = crate::brand_mark::BrandMark::default().footprint(360.0);
    for py in y.floor() as u32..(y + h).ceil() as u32 {
        for px in x.floor() as u32..(x + w).ceil() as u32 {
            // No SWAT ink in the mark's own corner: the dominant SWAT ink
            // colour (#11182B, light-background variant) must not appear.
            let p = frame.pixel(px, py);
            assert!(!(p[0] == 0x11 && p[1] == 0x18 && p[2] == 0x2B), "SWAT ink found at ({px}, {py}) with the mark meant to be off");
        }
    }
    let magenta = (0..640u32)
        .flat_map(|px| (0..360u32).map(move |py| (px, py)))
        .filter(|&(px, py)| {
            let p = frame.pixel(px, py);
            p[0] > 240 && p[1] < 16 && p[2] > 240
        })
        .collect::<Vec<_>>();
    assert!(magenta.len() > 400, "wordmark pixels: {}", magenta.len());
    // Without a mark to sit beside, the wordmark takes the mark's own
    // corner slot (`wordmark_quad`'s documented fallback): its left edge
    // lands at the mark's own inset, not pushed right of a mark that
    // isn't there.
    let min_x = magenta.iter().map(|p| p.0).min().unwrap();
    assert!((min_x as f32 - x).abs() < 2.0, "the wordmark should sit at the mark's own inset ({x}), got {min_x}");
}

#[test]
fn a_long_credit_stacks_into_a_plate_no_wider_than_sixty_percent_of_the_frame() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let long = [crate::story::burn_in::OFM_CREDIT, crate::story::burn_in::S2CLOUDLESS_CREDIT, crate::story::burn_in::GHSL_CREDIT]
        .join(" | ");
    let base = CaptureRequest { width: 1280, height: 720, ..base };
    let req = CaptureRequest { attribution: Some(long), ..base.clone() };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    let bare = capture(&base, &store, &style).unwrap();
    dump_for_eyes(&shot, "stacked-credit.png");
    // The narrow case the credit and the mark must share (F2), for eyes;
    // the geometry is asserted in `chrome::tests`.
    for width in [480u32, 590] {
        let narrow = CaptureRequest {
            width,
            height: 270,
            attribution: req.attribution.clone(),
            brand_mark: Some(crate::brand_mark::BrandMark::default()),
            ..req.clone()
        };
        let store = store_for(&narrow);
        dump_for_eyes(&capture(&narrow, &store, &style).unwrap(), &format!("stacked-{width}.png"));
    }
    // Every changed pixel sits in the right 60% (plus the corner padding)
    // and the changed band is several lines tall.
    let (w, h) = (req.width, req.height);
    let mut left = w;
    let mut top = h;
    for y in 0..h {
        for x in 0..w {
            if shot.pixel(x, y) != bare.pixel(x, y) {
                left = left.min(x);
                top = top.min(y);
            }
        }
    }
    assert!(left as f32 >= w as f32 * 0.4 - 4.0, "plate reaches x = {left} of {w}");
    let line = crate::symbol_pass::ATTRIBUTION_TEXT_SIZE_PX * crate::symbol_pass::LABEL_LINE_HEIGHT_RATIO;
    assert!((h - top) as f32 > line * 2.5, "plate is {} px tall: not stacked", h - top);
}

/// Summed channel change a caption makes in the bottom-left quarter.
fn caption_ink(alpha: f32) -> u64 {
    let (_, base, _, style) = scenes().remove(0);
    let caption = crate::story::timeline::ActiveCaption {
        text: "Marina Bay".into(),
        anchor: crate::story::timeline::Anchor::BottomLeft,
        offset_px: (0.0, 0.0),
        size_px: crate::story::timeline::DEFAULT_CAPTION_SIZE_PX,
        color: crate::story::timeline::DEFAULT_CAPTION_COLOR,
        plate: true,
        alpha,
    };
    let req = CaptureRequest { captions: vec![caption], ..base.clone() };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    let bare = capture(&base, &store, &style).unwrap();
    let (w, h) = (req.width, req.height);
    (0..w / 2)
        .flat_map(|x| (h / 2..h).map(move |y| (x, y)))
        .map(|(x, y)| {
            let (a, b) = (shot.pixel(x, y), bare.pixel(x, y));
            (0..3).map(|c| u64::from(a[c].abs_diff(b[c]))).sum::<u64>()
        })
        .sum()
}

#[test]
fn a_caption_at_half_alpha_is_fainter_than_at_full() {
    let _g = refused_lock();
    let (half, full) = (caption_ink(0.5), caption_ink(1.0));
    assert!(half > 0 && full > 0, "half {half} full {full}");
    assert!(half < full, "half {half} is not fainter than full {full}");
}

#[test]
fn the_default_request_draws_no_chrome_so_golden_is_unchanged() {
    // The defaults ask for none of the three, so the pass never runs; the
    // blessed hashes (`every_golden_scene_is_bit_identical_to_the_blessed_hash`)
    // are the proof in pixels.
    let r = CaptureRequest::default();
    assert!(r.captions.is_empty() && r.attribution.is_none() && r.brand_mark.is_none());
    assert!(!super::chrome::wanted(&r));
}

/// F1 (Task 17 review): no basemap label is placed under the burned-in SWAT
/// mark or the credit's plate. A label-dense Singapore frame, small enough
/// that labels reach both corners, is rendered twice through one session:
/// without the chrome, some placed box meets a corner footprint (the test
/// can fail); with it, none does.
#[test]
fn no_placed_label_meets_the_mark_or_the_credit_plate() {
    let _serial = refused_lock();
    let (_, mut request, tiles, style) = scenes().into_iter().find(|(n, ..)| *n == "sg_z14_flat_labels").unwrap();
    request.zoom = 15.0;
    let credit = crate::story::burn_in::OFM_CREDIT.to_string();
    let brand = crate::brand_mark::BrandMark::default();
    let mut session = CaptureSession::new_blocking(request.label_fonts.clone()).unwrap();
    // The footprints the chrome pass draws, from the one shared layout.
    let footprints: Vec<crate::collision::CollisionBox> = {
        let fonts = super::session::fonts_for(&mut session.fonts, &request.label_fonts);
        let corner = crate::map_view::corner_chrome(
            Some(&credit),
            request.width as f32,
            request.height as f32,
            request.pixel_ratio,
            Some((&brand, crate::brand_mark::artwork_size())),
            (0.0, 0.0),
            Default::default(),
            Default::default(),
            &mut fonts.system,
            &mut fonts.shapes,
        );
        let (plate, mark) = (corner.plate.unwrap(), corner.mark.unwrap());
        vec![
            crate::collision::CollisionBox { left: plate.screen_x, top: plate.screen_y, right: plate.screen_x + plate.width, bottom: plate.screen_y + plate.height },
            crate::collision::CollisionBox { left: mark.screen_x, top: mark.screen_y, right: mark.screen_x + mark.width, bottom: mark.screen_y + mark.height },
        ]
    };
    let meets = |b: &crate::collision::CollisionBox, f: &crate::collision::CollisionBox| {
        b.left < f.right && f.left < b.right && b.top < f.bottom && f.top < b.bottom
    };

    // The control: find a centre (on the fixture tile) where, without the
    // chrome, some placed label reaches a corner footprint.
    let (lon0, lat0) = (request.center_lon, request.center_lat);
    let mut found = false;
    'search: for i in 0..9 {
        for j in 0..9 {
            request.center_lon = lon0 + (f64::from(i) - 4.0) * 0.002;
            request.center_lat = lat0 + (f64::from(j) - 4.0) * 0.002;
            session.render(&request, &tiles, &style).unwrap();
            if session.last_placed_boxes.iter().any(|b| footprints.iter().any(|f| meets(b, f))) {
                found = true;
                break 'search;
            }
        }
    }
    assert!(found, "the control: somewhere near the fixture a label reaches a corner footprint without the chrome");

    request.attribution = Some(credit.clone());
    request.brand_mark = Some(brand);
    let frame = session.render(&request, &tiles, &style).unwrap();
    let placed = session.last_placed_boxes.clone();
    assert!(!placed.is_empty() && frame.labels_drawn > 0, "labels still draw elsewhere");
    for b in &placed {
        for f in &footprints {
            assert!(!meets(b, f), "a placed label {b:?} meets the chrome footprint {f:?}");
        }
    }
}

/// Final review V1: no basemap label is placed under a story element. A
/// label-dense Singapore frame carries a story with a centred caption and
/// a lower-third panel over its labels; rendered without the story, some
/// placed label meets one of their footprints (the test can fail); with
/// it, none does -- the chrome is laid out before placement and every
/// placed element is an obstacle. Mutation-checked by removing the
/// obstacle insert in `session.rs`.
#[test]
fn no_placed_label_meets_a_story_caption_or_panel() {
    use crate::story::timeline::{ActiveCaption, Anchor, DEFAULT_CAPTION_COLOR};
    let _serial = refused_lock();
    let (_, mut request, tiles, style) = scenes().into_iter().find(|(n, ..)| *n == "sg_z14_flat_labels").unwrap();
    request.zoom = 15.0;
    let text = "Container berths moved west to Pasir Panjang";
    let caption = ActiveCaption {
        text: text.into(),
        anchor: Anchor::Center,
        offset_px: (0.0, 0.0),
        size_px: 32.0,
        color: DEFAULT_CAPTION_COLOR,
        plate: true,
        alpha: 1.0,
    };
    let story = std::sync::Arc::new(StoryFrame {
        items: vec![
            crate::story::layout::LayoutItem::Caption {
                id: super::story_chrome::caption_id(0),
                text: text.into(),
                size: 32.0,
                anchor: Anchor::Center,
                offset: (0.0, 0.0),
                plate: true,
            },
            story_panel("/events/0", crate::story::script::Placement::LowerThird, "Harbour"),
        ],
        ..StoryFrame::default()
    });
    let mut with_story = request.clone();
    with_story.captions = vec![caption];
    with_story.story = Some(story);
    let mut session = CaptureSession::new_blocking(request.label_fonts.clone()).unwrap();
    // The story's footprints, from the one shared chrome layout.
    let footprints: Vec<crate::collision::CollisionBox> = {
        let fonts = super::session::fonts_for(&mut session.fonts, &with_story.label_fonts);
        let chrome = super::chrome::layout(&with_story, fonts, None, Default::default());
        let story = chrome.story.as_ref().expect("the story laid out");
        assert_eq!(story.captions.len(), 1, "the caption is placed");
        assert!(story.obstacles.len() >= 2, "the caption and the panel: {:?}", story.obstacles);
        assert_eq!(chrome.label_obstacles, story.obstacles, "no credit or mark here: the label obstacles are the story's");
        story.obstacles.iter().map(|&(l, t, r, b)| crate::collision::CollisionBox { left: l, top: t, right: r, bottom: b }).collect()
    };
    let meets = |b: &crate::collision::CollisionBox, f: &crate::collision::CollisionBox| {
        b.left < f.right && f.left < b.right && b.top < f.bottom && f.top < b.bottom
    };
    // The control: without the story, labels sit where its plates go.
    session.render(&request, &tiles, &style).unwrap();
    let under = session.last_placed_boxes.iter().filter(|b| footprints.iter().any(|f| meets(b, f))).count();
    assert!(under > 0, "the control: some label sits where the story's plates draw");

    let frame = session.render(&with_story, &tiles, &style).unwrap();
    let placed = session.last_placed_boxes.clone();
    assert!(!placed.is_empty() && frame.labels_drawn > 0, "labels still draw elsewhere");
    for b in &placed {
        for f in &footprints {
            assert!(!meets(b, f), "a placed label {b:?} meets the story footprint {f:?}");
        }
    }
}

/// Every icon name the fixture scene can ask liberty for: each string leaf
/// of every layer's `icon-image` that is not a `{token}`, plus the `class`
/// and `subclass` values of the fixture tile's `poi` features (liberty's
/// POI layers take their icon from `["get", "class"]` / `["get",
/// "subclass"]`, so the literal leaves alone name no POI icon).
fn liberty_icon_names() -> Vec<String> {
    fn leaves(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
        match v {
            serde_json::Value::String(s) if !s.starts_with('{') => {
                out.insert(s.clone());
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| leaves(i, out)),
            _ => {}
        }
    }
    let doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).unwrap(),
    )
    .unwrap();
    let mut names = std::collections::BTreeSet::new();
    for layer in doc["layers"].as_array().unwrap() {
        if let Some(icon) = layer.get("layout").and_then(|l| l.get("icon-image")) {
            leaves(icon, &mut names);
        }
    }
    for layer in fixture_decoded().layers.iter().filter(|l| l.name == "poi") {
        for feature in &layer.features {
            for key in ["class", "subclass"] {
                if let Some(crate::mvt::TagValue::String(s)) = feature.tags.get(key) {
                    names.insert(s.clone());
                }
            }
        }
    }
    names.remove("");
    names.into_iter().collect()
}

fn synthetic_sprites() -> std::sync::Arc<crate::sprite::SpriteAtlas> {
    let mut atlas = crate::sprite::SpriteAtlas::new();
    for name in liberty_icon_names() {
        atlas.insert(name, image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 0, 255, 255])), 1.0, false);
    }
    std::sync::Arc::new(atlas)
}

fn magenta_pixels(shot: &Capture) -> usize {
    shot.rgba.as_chunks::<4>().0.iter().filter(|p| p[0] > 230 && p[1] < 30 && p[2] > 230).count()
}

#[test]
fn icons_draw_when_a_sprite_sheet_is_given() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(1); // sg_z15, where POIs are
    let req = CaptureRequest { sprites: Some(synthetic_sprites()), ..base };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    dump_for_eyes(&shot, "icons_sg_z15.png");
    assert!(shot.icons_drawn > 0, "no icon drawn");
    let magenta = magenta_pixels(&shot);
    assert!(magenta > 16 * 16, "magenta icon pixels: {magenta}");
}

#[test]
fn icons_at_ratio_two_are_twice_the_size() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(1);
    let one = CaptureRequest { sprites: Some(synthetic_sprites()), ..base };
    let two = CaptureRequest { width: one.width * 2, height: one.height * 2, pixel_ratio: 2.0, ..one.clone() };
    let at_one = capture(&one, &store_for(&one), &style).unwrap();
    let at_two = capture(&two, &store_for(&two), &style).unwrap();
    let (n1, n2) = (at_one.icons_drawn as f64, at_two.icons_drawn as f64);
    assert!(n1 > 0.0 && n1 == n2, "the same icons place at both ratios: {n1} at 1x, {n2} at 2x");
    let (m1, m2) = (magenta_pixels(&at_one) as f64, magenta_pixels(&at_two) as f64);
    assert!(m1 > 0.0, "no magenta at 1x");
    let ratio = (m2 / n2) / (m1 / n1);
    eprintln!("magenta {m1} over {n1} icons at 1x, {m2} over {n2} at 2x: per-icon ratio {ratio}, raw {}", m2 / m1);
    assert!((3.5..=4.5).contains(&ratio), "2x/1x magenta per icon {ratio} ({m2}/{n2} vs {m1}/{n1})");
}

/// A session that drew icons draws the sprite-less scene bit-identical to
/// the blessed hash afterwards: the cached icon atlas leaks into nothing.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_session_that_drew_icons_draws_the_golden_frame_without_them() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (name, base, store, style) = scenes().remove(1);
    let mut session = CaptureSession::new_blocking(base.label_fonts.clone()).unwrap();
    let with = CaptureRequest { sprites: Some(synthetic_sprites()), ..base.clone() };
    assert!(session.render(&with, &store_for(&with), &style).unwrap().icons_drawn > 0);
    let without = session.render(&base, &store, &style).unwrap();
    assert_eq!(without.icons_drawn, 0);
    assert_eq!(Some(&without.sha256_hex()), golden.get(name), "{name}");
}

/// Review F8: a real `@2x` sheet declares `pixelRatio: 2` with doubled
/// pixels. Its 32x32 icons must draw at the size 16x16 icons at ratio 1
/// do -- `icon_logical_size` divides the ratio out on the capture path too.
#[test]
fn a_2x_declared_sheet_draws_its_icons_at_their_logical_size() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(1);
    let mut doubled = crate::sprite::SpriteAtlas::new();
    for name in liberty_icon_names() {
        doubled.insert(name, image::RgbaImage::from_pixel(32, 32, image::Rgba([255, 0, 255, 255])), 2.0, false);
    }
    let one = CaptureRequest { sprites: Some(synthetic_sprites()), ..base.clone() };
    let two = CaptureRequest { sprites: Some(std::sync::Arc::new(doubled)), ..base };
    let at_one = capture(&one, &store_for(&one), &style).unwrap();
    let at_two = capture(&two, &store_for(&two), &style).unwrap();
    assert!(at_one.icons_drawn > 0);
    assert_eq!(at_one.icons_drawn, at_two.icons_drawn, "same logical size, same placement");
    let (m1, m2) = (magenta_pixels(&at_one) as f64, magenta_pixels(&at_two) as f64);
    assert!((m2 / m1 - 1.0).abs() < 0.1, "magenta: {m1} from 16x16@1, {m2} from 32x32@2");
}

// ---- L2: line-placed labels (street names) through a `GlyphSink` ----

#[test]
fn street_names_draw_along_the_roads_at_z16() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let off = CaptureRequest { zoom: 16.0, width: 960, height: 540, label_fonts: noto(), ..base };
    let on = CaptureRequest { line_labels: true, ..off.clone() };
    let store = store_for(&off);
    let a = capture(&off, &store, &style).unwrap();
    let b = capture(&on, &store, &style).unwrap();
    assert_eq!(a.line_glyphs_drawn, 0);
    assert!(b.line_glyphs_drawn > 20, "glyphs: {}", b.line_glyphs_drawn);
    let changed = a
        .rgba
        .as_chunks::<4>().0.iter()
        .zip(b.rgba.as_chunks::<4>().0.iter())
        .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > 24))
        .count();
    assert!(changed > 500, "text pixels: {changed}");
    b.save_png(&std::path::Path::new(&std::env::var("HOME").unwrap()).join(".cache/gpui-map-stories/l2/z16.png"))
        .unwrap();
}

#[test]
fn line_labels_are_deterministic_across_sessions() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let req = CaptureRequest { zoom: 16.0, line_labels: true, ..base };
    let store = store_for(&req);
    let first = CaptureSession::new_blocking(req.label_fonts.clone()).unwrap().render(&req, &store, &style).unwrap();
    let second = CaptureSession::new_blocking(req.label_fonts.clone()).unwrap().render(&req, &store, &style).unwrap();
    assert!(first.line_glyphs_drawn > 0, "no line glyph drawn");
    assert_eq!(first.line_glyphs_drawn, second.line_glyphs_drawn);
    assert_eq!(first.sha256_hex(), second.sha256_hex());
}

/// The session keeps its line-glyph state across frames (the distance
/// fields of every glyph it ever drew), and a frame must not change for
/// it: the atlas is laid out afresh per frame, exactly as a fresh
/// session's. Review F3: the warm session's state provably differs.
#[test]
fn a_line_label_frame_does_not_depend_on_what_the_session_drew_before() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let warm = CaptureRequest { zoom: 15.0, width: 960, height: 540, line_labels: true, ..base.clone() };
    let req = CaptureRequest { zoom: 15.5, line_labels: true, ..base };
    let (warm_store, store) = (store_for(&warm), store_for(&req));
    let mut fresh_session = CaptureSession::new_blocking(req.label_fonts.clone()).unwrap();
    let fresh = fresh_session.render(&req, &store, &style).unwrap();
    let mut session = CaptureSession::new_blocking(req.label_fonts.clone()).unwrap();
    assert!(session.render(&warm, &warm_store, &style).unwrap().line_glyphs_drawn > 0);
    let warm_layout = session.line_glyph_layout();
    let after = session.render(&req, &store, &style).unwrap();
    assert!(fresh.line_glyphs_drawn > 0);
    // The session was really warm: its atlas held glyphs this frame never
    // asks for, at origins a warm, uncleared atlas would have kept.
    let fresh_layout = fresh_session.line_glyph_layout();
    assert!(warm_layout.len() > fresh_layout.len(), "the warm frame rastered glyphs this one does not use");
    assert_ne!(warm_layout, fresh_layout);
    assert!(session.line_glyph_fields_kept() > fresh_session.line_glyph_fields_kept());
    // ...and the frame's own atlas is the fresh one, so are its bytes.
    assert_eq!(session.line_glyph_layout(), fresh_layout);
    assert_eq!(fresh.sha256_hex(), after.sha256_hex());
}

/// One shelf of glyph fields: a 64-row atlas.
const ONE_SHELF: u32 = 64;

/// An atlas-FILLING frame (z15 at 960x540: 35 glyph keys, two shelves, so
/// a one-shelf atlas refuses some), then a frame that fits one shelf
/// alone (z15.5 at 640x360) and uses glyphs the first one was refused.
fn capped_pair() -> (CaptureRequest, CaptureRequest, Style) {
    let (_, base, _, style) = scenes().remove(0);
    let first = CaptureRequest { zoom: 15.0, width: 960, height: 540, line_labels: true, ..base.clone() };
    let then = CaptureRequest { zoom: 15.5, line_labels: true, ..base };
    (first, then, style)
}

fn capped_session(fonts: &Option<LabelFonts>, rows: u32) -> CaptureSession {
    let mut session = CaptureSession::new_blocking(fonts.clone()).unwrap();
    session.glyph_atlas_max_height = rows;
    session
}

fn capped_layout(
    req: &CaptureRequest,
    store: &TileStore,
    style: &Style,
) -> Vec<(crate::render::GlyphKey, Option<crate::render::glyph_atlas::GlyphRect>)> {
    let mut session = capped_session(&req.label_fonts, ONE_SHELF);
    session.render(req, store, style).unwrap();
    session.line_glyph_layout()
}

/// Review F1: a frame after an atlas-filling frame is the same bytes as
/// that frame in a fresh session -- the atlas is laid out afresh for every
/// frame, so it never draws names with letters missing because of what
/// came before.
#[test]
fn a_frame_after_an_atlas_filling_frame_equals_it_in_a_fresh_session() {
    let _refused = refused_lock();
    let (first, then, style) = capped_pair();
    let (first_store, then_store) = (store_for(&first), store_for(&then));
    let fresh = capped_session(&then.label_fonts, ONE_SHELF).render(&then, &then_store, &style).unwrap();
    assert_eq!(fresh.line_glyphs_dropped, 0, "the frame fits a fresh capped atlas alone");
    assert!(fresh.line_glyphs_drawn > 0);
    let mut session = capped_session(&then.label_fonts, ONE_SHELF);
    let filled = session.render(&first, &first_store, &style).unwrap();
    assert!(filled.line_glyphs_dropped > 0, "the first frame fills the atlas");
    // Glyphs the filled atlas refused that the second frame needs: an
    // atlas that kept its layout would draw those names with letters
    // missing.
    let refused: std::collections::BTreeSet<_> =
        session.line_glyph_layout().into_iter().filter(|(_, rect)| rect.is_none()).map(|(key, _)| key).collect();
    let then_layout = capped_layout(&then, &then_store, &style);
    assert!(
        then_layout.iter().any(|(key, rect)| rect.is_some() && refused.contains(key)),
        "the second frame needs a glyph the filled atlas refused"
    );
    let after = session.render(&then, &then_store, &style).unwrap();
    assert_eq!(after.line_glyphs_dropped, 0, "no letters lost to the earlier frame");
    assert_eq!(after.line_glyphs_drawn, fresh.line_glyphs_drawn);
    assert_eq!(after.sha256_hex(), fresh.sha256_hex());
    assert_eq!(session.line_glyph_layout(), then_layout, "a fresh layout per frame");
}

/// Review F1: a frame asking for more glyphs than even a fresh atlas holds
/// reports them in `line_glyphs_dropped`, and stays the same bytes on a
/// second render.
#[test]
fn glyphs_a_fresh_atlas_cannot_hold_are_reported_not_silent() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    // z15 at 960x540: 35 keys, two shelves.
    let req = CaptureRequest { zoom: 15.0, width: 960, height: 540, line_labels: true, ..base };
    let store = store_for(&req);
    let whole = capped_session(&req.label_fonts, u32::MAX).render(&req, &store, &style).unwrap();
    assert_eq!(whole.line_glyphs_dropped, 0);
    let mut session = capped_session(&req.label_fonts, ONE_SHELF);
    let short = session.render(&req, &store, &style).unwrap();
    assert!(short.line_glyphs_dropped > 0, "a one-shelf atlas cannot hold two shelves of glyphs");
    assert!(short.line_glyphs_drawn < whole.line_glyphs_drawn);
    let again = session.render(&req, &store, &style).unwrap();
    assert_eq!(again.line_glyphs_dropped, short.line_glyphs_dropped);
    assert_eq!(again.sha256_hex(), short.sha256_hex());
}

/// Review F2: how much the set of drawn street names changes frame to
/// frame over a short orbit and zoom, with no fade to hide it. Reported,
/// not judged: the capture cannot fade (a fade reads earlier frames), and
/// the manifest's gaps say so.
#[test]
fn line_label_churn_over_a_short_orbit_is_measured() {
    let _refused = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(base.label_fonts.clone()).unwrap();
    let mut previous: Option<Vec<String>> = None;
    let (mut churned, mut frames, mut labels) = (0usize, 0usize, 0usize);
    for i in 0..24 {
        let req = CaptureRequest {
            zoom: 15.5 + f64::from(i) * 0.05,
            bearing: (f64::from(i) * 2.0).to_radians() as f32,
            width: 960,
            height: 540,
            center_lon: 103.845,
            line_labels: true,
            ..base.clone()
        };
        session.render(&req, &store_for(&req), &style).unwrap();
        let mut now = session.last_line_label_texts.clone();
        now.sort();
        if let Some(prev) = &previous {
            let gone = prev.iter().filter(|t| !now.contains(t)).count();
            let came = now.iter().filter(|t| !prev.contains(t)).count();
            churned += gone + came;
            eprintln!("CHURN frame {i} zoom {:.2}: {} names, +{came} -{gone}", req.zoom, now.len());
        }
        labels += now.len();
        frames += 1;
        previous = Some(now);
    }
    eprintln!(
        "CHURN total: {churned} appear/vanish events over {} frame steps, {:.1} names per frame",
        frames - 1,
        labels as f64 / frames as f64
    );
    assert!(labels > 0, "no street name drawn over the orbit");
}

/// A session that drew street names draws the golden frame (line labels
/// off) bit-identical afterwards.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn a_session_that_drew_street_names_draws_the_golden_frame_without_them() {
    let _refused = refused_lock();
    let golden = read_golden();
    let (name, base, store, style) = scenes().remove(0);
    let mut session = CaptureSession::new_blocking(base.label_fonts.clone()).unwrap();
    let with = CaptureRequest { line_labels: true, ..base.clone() };
    assert!(session.render(&with, &store, &style).unwrap().line_glyphs_drawn > 0);
    let without = session.render(&base, &store, &style).unwrap();
    assert_eq!(without.line_glyphs_drawn, 0);
    assert_eq!(Some(&without.sha256_hex()), golden.get(name), "{name}");
}

// ---- Story overlay layers (story-content 1a, Task 7) ----

/// A story layer over `geojson` with `paint`, as declared: no trim, full
/// opacity, no overrides, no marks.
fn story_layer(
    layer_id: &str,
    geojson: String,
    paint: serde_json::Value,
    line_metrics: bool,
    per_feature: bool,
) -> StoryLayerDraw {
    let paint: serde_json::Map<String, serde_json::Value> = serde_json::from_value(paint).expect("a paint object");
    StoryLayerDraw {
        key: StoryLayerDraw::content_key(layer_id, &geojson, &paint, line_metrics, per_feature),
        layer_id: layer_id.into(),
        geojson: geojson.into(),
        paint: std::sync::Arc::new(paint),
        line_metrics,
        per_feature,
        above_labels: false,
        // Off: these layers' frames are `capture_golden.txt`'s, blessed
        // before the above-buildings pass existed (Task 22).
        above_buildings: false,
        line_fx: crate::render::gpu::LINE_FX_NONE,
        features: Vec::new(),
        marks: None,
        arrowheads: Vec::new(), markers: None, labels: Vec::new(),
    }
}

/// Task 22 ruling: a story route that crosses 3D buildings draws whole,
/// not dashed. One real z14 tile of Shinjuku, a style of nothing but its
/// `fill-extrusion` layer, pitch 60: a red constant-latitude line across
/// the frame projects to one horizontal band, so every column it spans
/// must hold a red pixel. With `above_buildings` off (the tile pass,
/// under the walls) the same frame has columns with none -- the gap this
/// pass exists to close, so the test can fail.
#[test]
fn a_story_route_crossing_buildings_has_no_gap() {
    let _refused = refused_lock();
    let style = crate::style_spec::parse_and_compile(
        &serde_json::to_vec(&serde_json::json!({
            "version": 8,
            "sources": {"buildings": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
            "layers": [{"id": "b", "type": "fill-extrusion", "source": "buildings", "source-layer": "building",
                        "paint": {"fill-extrusion-color": "#888", "fill-extrusion-opacity": 1.0,
                                  "fill-extrusion-height": ["get", "render_height"]}}]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut tiles = TileStore::new();
    tiles.insert(
        TileId { z: 14, x: 14549, y: 6451 },
        crate::mvt::decode(std::fs::read("benches/fixtures/shinjuku_z14.pbf").unwrap()).unwrap(),
    );
    let (lon, lat) = (139.6995, 35.6905);
    let geojson = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[lon - 0.004, lat], [lon + 0.004, lat]]}}]})
    .to_string();
    let request = |above_buildings: bool| CaptureRequest {
        center_lon: lon,
        center_lat: lat,
        zoom: 16.5,
        pitch: 60f32.to_radians(),
        bearing: 0.0,
        width: 256,
        height: 256,
        flat: true,
        extrusion_opacity: Some(1.0),
        story_layers: vec![StoryLayerDraw {
            above_buildings,
            ..story_layer("route", geojson.clone(), serde_json::json!({"line-color": "#ff0000", "line-width": 6}), true, false)
        }],
        ..CaptureRequest::default()
    };
    let red = |p: [u8; 4]| p[0] > 200 && p[1] < 60 && p[2] < 60;
    // The columns between 8 px in from each edge with no red pixel.
    let gaps = |shot: &Capture| (8..shot.width - 8).filter(|&x| !(0..shot.height).any(|y| red(shot.pixel(x, y)))).count();
    let over = capture(&request(true), &tiles, &style).expect("capture");
    let under = capture(&request(false), &tiles, &style).expect("capture");
    assert!(over.tiles_drawn > 0 && under.tiles_drawn > 0, "the building tile drew");
    assert!(gaps(&under) > 0, "the frame must put a building over the route, or this test proves nothing");
    assert_eq!(gaps(&over), 0, "a route above the buildings has a gap: {} of {} columns", gaps(&over), over.width - 16);
}

/// The red 6 px line (line metrics on) through `base`'s centre, west to
/// east, 0.01 deg either side of it, under a fixed key.
fn straight_line_layer(base: &CaptureRequest) -> StoryLayerDraw {
    let (lon, lat) = (base.center_lon, base.center_lat);
    let geojson = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[lon - 0.01, lat], [lon + 0.01, lat]]}}]});
    StoryLayerDraw {
        key: 0x5707_1A7E,
        ..story_layer("route-line", geojson.to_string(), serde_json::json!({"line-color": "red", "line-width": 6}), true, false)
    }
}

/// A flat magenta-cleared frame of `w`x`h` at `ratio`, no tiles, centred
/// on (0, 0) at zoom 0 (the world is 512 logical px wide).
fn flat_blank(w: u32, h: u32, ratio: f32) -> CaptureRequest {
    CaptureRequest {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 0.0,
        width: w,
        height: h,
        pixel_ratio: ratio,
        flat: true,
        globe: false,
        clear: Clear::SENTINEL,
        backdrop: false,
        ..CaptureRequest::default()
    }
}

fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 80 && p[2] < 80
}

/// Columns holding at least one red pixel.
fn red_columns(shot: &Capture) -> usize {
    (0..shot.width).filter(|&x| (0..shot.height).any(|y| is_red(shot.pixel(x, y)))).count()
}

/// The 6 px red line across lon [-100, 100] at lat 0 (284 logical px at z0).
///
/// Not the brief's [-170, 170], because the overlay's local grid used to
/// saturate ~151 deg from the source's centre and cut such a line at both
/// ends. It no longer does
/// (`an_overlay_line_spanning_340_degrees_lands_within_a_pixel_at_both_ends`);
/// [-100, 100] stays because the trim tests below are calibrated on it.
fn wide_line() -> StoryLayerDraw {
    let geojson = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "id": 0, "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[-100.0, 0.0], [100.0, 0.0]]}}]});
    story_layer("wide", geojson.to_string(), serde_json::json!({"line-color": "#ff0000", "line-width": 6}), true, false)
}

#[test]
fn a_story_layer_is_tessellated_once_and_redrawn_from_cache_while_its_trim_moves() {
    let _refused = refused_lock();
    let mut session = CaptureSession::new_blocking(noto()).unwrap();
    let (_, base, store, style) = scenes().remove(0);
    let layer = |trim: f32| StoryLayerDraw { line_fx: [0.0, trim, 1.0, 1.0], ..straight_line_layer(&base) };
    let mut rebuilt = Vec::new();
    let mut red = Vec::new();
    for i in 0..10 {
        let req = CaptureRequest { story_layers: vec![layer(i as f32 / 9.0)], ..base.clone() };
        let shot = session.render(&req, &store, &style).unwrap();
        assert!(shot.tiles_drawn > 0, "frame {i} painted the basemap");
        rebuilt.push(shot.story_retessellations);
        red.push(red_columns(&shot));
    }
    assert_eq!(rebuilt, [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(red[0], 0, "trim 0 draws nothing");
    assert!(red.windows(2).all(|w| w[1] >= w[0]) && red[9] > red[1], "the line grows with its trim: {red:?}");
}

#[test]
fn a_trim_of_one_half_draws_half_the_length_of_a_straight_line() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let store = TileStore::new();
    let at = |trim: f32, session: &mut CaptureSession| {
        let req = CaptureRequest { story_layers: vec![StoryLayerDraw { line_fx: [0.0, trim, 1.0, 1.0], ..wide_line() }], ..flat_blank(640, 96, 1.0) };
        red_columns(&session.render(&req, &store, &style).unwrap())
    };
    let (whole, half, none) = (at(1.0, &mut session), at(0.5, &mut session), at(0.0, &mut session));
    eprintln!("TRIM columns: whole {whole}, half {half}, none {none}");
    assert!((280..=290).contains(&whole), "the whole line is ~284 px: {whole}");
    assert!(half.abs_diff(whole / 2) <= 3, "half the length: {half} of {whole}");
    assert_eq!(none, 0, "trim 0 draws no red pixel");
}

/// The first and last column holding a red pixel in `xs`, or `None`.
fn red_span(shot: &Capture, xs: std::ops::Range<u32>) -> Option<(u32, u32)> {
    let cols: Vec<u32> = xs.filter(|&x| (0..shot.height).any(|y| is_red(shot.pixel(x, y)))).collect();
    Some((*cols.first()?, *cols.last()?))
}

/// Where lon `lon` falls on [`flat_blank`]'s 640 px frame: (0, 0) at
/// x 320 and a 512 px world.
fn flat_blank_x(lon: f64) -> f64 {
    320.0 + lon / 360.0 * 512.0
}

/// One overlay source spanning 340 deg of longitude must land where its
/// coordinates say, within a pixel at both ends. The overlay's local grid
/// (`overlay::geom::OverlayOrigin::to_local`) used to saturate its `i32`
/// ~151 deg from the source's centre and draw this line as x 105..534.
#[test]
fn an_overlay_line_spanning_340_degrees_lands_within_a_pixel_at_both_ends() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let line = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {"line-color": "#ff0000", "line-width": 6},
        "geometry": {"type": "LineString", "coordinates": [[-170.0, 0.0], [170.0, 0.0]]}}]});
    let req = CaptureRequest { overlay: Some(line.to_string()), ..flat_blank(640, 96, 1.0) };
    let shot = session.render(&req, &TileStore::new(), &style).unwrap();
    let (first, last) = red_span(&shot, 0..shot.width).expect("the line drew");
    let (west, east) = (flat_blank_x(-170.0), flat_blank_x(170.0));
    eprintln!("WIDE columns {first}..={last}, expected {west:.1}..{east:.1}");
    assert!((f64::from(first) - west).abs() <= 1.0, "west end at x {first}, not {west:.1}");
    assert!((f64::from(last) + 1.0 - east).abs() <= 1.0, "east end at x {last}, not {east:.1}");
}

/// Two dots either side of the antimeridian, 1 deg apart the short way
/// and 359 deg apart as written: one source, whose bounding-box centre is
/// therefore Greenwich, ~180 deg from both. Each must land within a pixel
/// of its own longitude, not on a stripe ~151 deg out.
#[test]
fn overlay_dots_either_side_of_the_antimeridian_land_within_a_pixel() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let dot = |lon: f64| {
        serde_json::json!({"type": "Feature", "properties": {"circle-color": "#ff0000", "circle-radius": 4},
            "geometry": {"type": "Point", "coordinates": [lon, 0.0]}})
    };
    let dots = serde_json::json!({"type": "FeatureCollection", "features": [dot(-179.5), dot(179.5)]});
    let req = CaptureRequest { overlay: Some(dots.to_string()), ..flat_blank(640, 96, 1.0) };
    let shot = session.render(&req, &TileStore::new(), &style).unwrap();
    let centres: Vec<f64> = [0..320, 320..shot.width]
        .into_iter()
        .filter_map(|xs| red_span(&shot, xs).map(|(a, b)| f64::from(a + b + 1) / 2.0))
        .collect();
    let expected = [flat_blank_x(-179.5), flat_blank_x(179.5)];
    eprintln!("ANTIMERIDIAN dot centres {centres:?}, expected {expected:?}");
    assert_eq!(centres.len(), 2, "both dots drew, one each side of Greenwich: {centres:?}");
    for (got, want) in centres.iter().zip(expected) {
        assert!((got - want).abs() <= 1.0, "a dot centred at x {got}, not {want:.1}");
    }
}

/// A story circle layer of cities across every longitude -- what
/// `story::validate` refused as `feature_too_far` while the overlay grid
/// saturated -- draws each dot within a pixel of its longitude.
#[test]
fn a_story_layer_of_cities_across_every_longitude_lands_each_within_a_pixel() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let lons = [-170.0, -74.0, 2.3, 139.7, 174.8];
    let features: Vec<serde_json::Value> = lons
        .iter()
        .enumerate()
        .map(|(i, lon)| serde_json::json!({"type": "Feature", "id": i, "properties": {}, "geometry": {"type": "Point", "coordinates": [lon, 0.0]}}))
        .collect();
    let geojson = serde_json::json!({"type": "FeatureCollection", "features": features}).to_string();
    let layer = story_layer("cities", geojson, serde_json::json!({"circle-color": "#ff0000", "circle-radius": 4}), false, false);
    let req = CaptureRequest { story_layers: vec![layer], ..flat_blank(640, 96, 1.0) };
    let shot = session.render(&req, &TileStore::new(), &style).unwrap();
    for lon in lons {
        let want = flat_blank_x(lon);
        let xs = (want - 12.0).max(0.0) as u32..((want + 12.0) as u32).min(shot.width);
        let got = red_span(&shot, xs).map(|(a, b)| f64::from(a + b + 1) / 2.0);
        eprintln!("CITY lon {lon}: centred at {got:?}, expected {want:.1}");
        assert!(got.is_some_and(|g| (g - want).abs() <= 1.0), "the dot at lon {lon} is centred at {got:?}, not {want:.1}");
    }
}

/// F30 (spec §3.1): overlay widths scale with the pixel ratio, as the
/// engine's style layers do.
#[test]
fn a_story_line_at_ratio_two_is_twice_as_many_pixels_wide() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let store = TileStore::new();
    let thickness = |ratio: f32, session: &mut CaptureSession| {
        let req = CaptureRequest { story_layers: vec![wide_line()], ..flat_blank((640.0 * ratio) as u32, (96.0 * ratio) as u32, ratio) };
        let shot = session.render(&req, &store, &style).unwrap();
        let x = shot.width / 2;
        (0..shot.height).filter(|&y| is_red(shot.pixel(x, y))).count()
    };
    let (one, two) = (thickness(1.0, &mut session), thickness(2.0, &mut session));
    eprintln!("RATIO rows: 1x {one}, 2x {two}");
    assert!(one >= 5, "a 6 px line at ratio 1: {one} rows");
    assert!(two.abs_diff(2 * one) <= 1, "ratio 2 is twice as wide: {two} vs 2 x {one}");
}

/// Three blue dots (radius 10) at lon -60, 0, 60 on the equator; one op
/// per feature.
fn three_dots() -> StoryLayerDraw {
    let features: Vec<serde_json::Value> = [-60.0, 0.0, 60.0]
        .iter()
        .enumerate()
        .map(|(i, lon)| serde_json::json!({"type": "Feature", "id": i, "properties": {}, "geometry": {"type": "Point", "coordinates": [lon, 0.0]}}))
        .collect();
    let geojson = serde_json::json!({"type": "FeatureCollection", "features": features});
    story_layer("dots", geojson.to_string(), serde_json::json!({"circle-color": "blue", "circle-radius": 10}), false, true)
}

/// The pixel at feature `i` of [`three_dots`] on a 640x96 z0 frame.
fn dot_centre(shot: &Capture, i: usize) -> [u8; 4] {
    let lon = [-60.0f64, 0.0, 60.0][i];
    shot.pixel((320.0 + lon / 360.0 * 512.0).round() as u32, 48)
}

fn highlight(id: u64) -> Vec<crate::story::effects::FeatureFx> {
    (0..3)
        .map(|i| crate::story::effects::FeatureFx {
            id: i,
            color: (i == id).then_some([255, 0, 0, 255]),
            alpha: if i == id { 1.0 } else { 0.25 },
            radius_scale: 1.0,
        })
        .collect()
}

#[test]
fn a_highlight_rewrites_paint_without_retessellating() {
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let store = TileStore::new();
    let frame = |features: Vec<crate::story::effects::FeatureFx>, session: &mut CaptureSession| {
        let req = CaptureRequest { story_layers: vec![StoryLayerDraw { features, ..three_dots() }], ..flat_blank(640, 96, 1.0) };
        session.render(&req, &store, &style).unwrap()
    };
    let one = frame(Vec::new(), &mut session);
    let two = frame(highlight(1), &mut session);
    assert_eq!((one.story_retessellations, two.story_retessellations), (1, 0));
    assert_eq!(dot_centre(&one, 1)[..3], [0, 0, 255], "declared blue (named colour)");
    assert_eq!(dot_centre(&two, 1)[..3], [255, 0, 0], "highlighted red");
    // Dimmed to 25% blue over the magenta clear: its red channel rises from 0.
    let (before, after) = (dot_centre(&one, 0), dot_centre(&two, 0));
    assert!(after[0] > before[0] + 150, "feature 0 dims: {before:?} -> {after:?}");
    assert_eq!(dot_centre(&two, 2), after, "feature 2 dims the same");
}

/// Seek == play at the frame level: a frame drawn from a warm cache after
/// other frames (other overrides, other trims) is the bytes a fresh
/// session draws for it alone.
#[test]
fn a_story_frame_after_other_frames_is_the_frame_drawn_fresh() {
    let style = Style { layers: Vec::new(), ..Style::default() };
    let store = TileStore::new();
    let req = |features: Vec<crate::story::effects::FeatureFx>, trim: f32| CaptureRequest {
        story_layers: vec![StoryLayerDraw { line_fx: [0.0, trim, 0.8, 1.0], ..wide_line() }, StoryLayerDraw { features, ..three_dots() }],
        ..flat_blank(640, 96, 1.0)
    };
    // Review #8: from all three overridden to a smaller non-empty subset
    // (id 2 only, grown by `radius_scale`), so ids 0 and 1 are restored.
    let only_two = vec![crate::story::effects::FeatureFx { id: 2, color: None, alpha: 1.0, radius_scale: 1.5 }];
    let sequence = [(highlight(0), 0.2), (highlight(2), 0.9), (Vec::new(), 0.5), (highlight(1), 0.7), (only_two.clone(), 0.7)];
    let mut warm = CaptureSession::new_blocking(None).unwrap();
    for (i, (features, trim)) in sequence.iter().enumerate() {
        let played = warm.render(&req(features.clone(), *trim), &store, &style).unwrap();
        let fresh = CaptureSession::new_blocking(None).unwrap().render(&req(features.clone(), *trim), &store, &style).unwrap();
        assert_eq!(played.story_retessellations, if i == 0 { 2 } else { 0 });
        assert_eq!(played.sha256_hex(), fresh.sha256_hex(), "frame {i} depends on the frames before it");
        let one_shot = capture(&req(features.clone(), *trim), &store, &style).unwrap();
        assert_eq!(one_shot.sha256_hex(), fresh.sha256_hex(), "frame {i}: the one-shot path draws it too");
    }
    // The shrunk subset: ids 0 and 1 are back to their pristine pixels, and
    // id 2 is drawn at 1.5x its radius.
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let pristine = session.render(&req(Vec::new(), 0.7), &store, &style).unwrap();
    session.render(&req(highlight(1), 0.7), &store, &style).unwrap();
    let shrunk = session.render(&req(only_two, 0.7), &store, &style).unwrap();
    assert_eq!(shrunk.story_retessellations, 0);
    for i in 0..2 {
        assert_eq!(dot_centre(&shrunk, i), dot_centre(&pristine, i), "feature {i} restored to its pristine paint");
    }
    let blue_near = |shot: &Capture, i: usize| {
        let cx = (320.0 + [-60.0f64, 0.0, 60.0][i] / 360.0 * 512.0).round() as i64;
        let (mut n, h) = (0, i64::from(shot.height));
        for y in 0..h {
            for x in cx - 30..=cx + 30 {
                let p = shot.pixel(x as u32, y as u32);
                if p[2] > 200 && p[0] < 60 && p[1] < 60 {
                    n += 1;
                }
            }
        }
        n
    };
    let (before, after) = (blue_near(&pristine, 2), blue_near(&shrunk, 2));
    eprintln!("RADIUS_SCALE blue pixels: {before} -> {after}");
    // Area goes with radius squared: 1.5^2 = 2.25, within the edge AA.
    assert!((after as f64 / before as f64 - 2.25).abs() < 0.25, "radius_scale 1.5: {before} -> {after}");
}

/// Two draws with one key in a request would share one uniform: refused.
#[test]
fn two_story_layers_with_one_key_are_refused() {
    let style = Style { layers: Vec::new(), ..Style::default() };
    let req = CaptureRequest { story_layers: vec![wide_line(), wide_line()], ..flat_blank(64, 32, 1.0) };
    let err = capture(&req, &TileStore::new(), &style).err().expect("refused");
    assert!(format!("{err:#}").contains("distinct"), "{err:#}");
}

/// Review #8/#2: the head is drawn as pixels at the tip (white on an
/// outline in the line's colour), and above the line.
#[test]
fn the_draw_head_is_drawn_at_the_tip_on_an_outline() {
    let style = Style { layers: Vec::new(), ..Style::default() };
    // At trim 0.5 the tip of [-100, 100] is lon 0: x 320, y 48.
    let marks = serde_json::json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {"circle-radius": 10.0, "circle-color": "#ff0000ff"}, "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}},
        {"type": "Feature", "properties": {"circle-radius": 8.0, "circle-color": "#ffffffff"}, "geometry": {"type": "Point", "coordinates": [0.0, 0.0]}}]});
    let req = CaptureRequest {
        story_layers: vec![StoryLayerDraw { line_fx: [0.0, 0.5, 1.0, 1.0], marks: Some(marks.to_string()), ..wide_line() }],
        ..flat_blank(640, 96, 1.0)
    };
    let shot = capture(&req, &TileStore::new(), &style).unwrap();
    let (mut white, mut red_ring) = (0, 0);
    for y in 30..66u32 {
        for x in 300..340u32 {
            let p = shot.pixel(x, y);
            let d2 = (i64::from(x) - 320).pow(2) + (i64::from(y) - 48).pow(2);
            if p[..3] == [255, 255, 255] {
                white += 1;
                assert!(d2 <= 81, "white only inside the head: ({x}, {y})");
            }
            if is_red(p) && d2 > 81 && d2 <= 121 && y.abs_diff(48) > 4 {
                red_ring += 1;
            }
        }
    }
    eprintln!("HEAD white {white}, outline {red_ring}");
    // pi * 8^2 = 201 px, less the anti-aliased rim.
    assert!((150..=215).contains(&white), "a white 8 px head at the tip: {white}");
    assert!(red_ring > 20, "an outline in the line colour around it, off the line itself: {red_ring}");
    // The line is trimmed at the tip: no red well past it.
    assert!((345..400).all(|x| !is_red(shot.pixel(x, 48))), "the line stops at the tip");
}

/// Task 8 (spec §3.2/§3.3, Task 7 review #1, pre-flight F3): a route across
/// the antimeridian goes through the real story pipeline --
/// `story::overlay_layers::layer_sources` unwraps `[179, -179]` the short
/// way (to `[179, 181]`), exactly as a live render would -- and draws as
/// one contiguous run that grows with its trim and never reverses.
#[test]
fn a_line_across_the_antimeridian_goes_the_short_way_with_continuous_progress() {
    use crate::story::script::{OverlayLayerKind, OverlayLayerSpec, OverlaySpec};
    let overlay = OverlaySpec {
        id: "route".into(),
        geojson: None,
        paint: None,
        above_labels: false,
        data: None,
        line_metrics: true,
        layers: vec![OverlayLayerSpec {
            id: "route-line".into(),
            kind: OverlayLayerKind::Line,
            filter: None,
            paint: serde_json::from_value(serde_json::json!({"line-color": "red", "line-width": 6})).unwrap(),
            above_buildings: None,
            role: None,
            label: None,
            legend: None,
        }],
        attribution: None,
    };
    let doc = serde_json::json!({"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[179.0, 0.0], [-179.0, 0.0]]}}]});
    let source = crate::story::overlay_layers::layer_sources(0, &overlay, &doc).unwrap().remove(0);
    let paint_json = serde_json::Value::Object(source.paint.clone());
    let layer =
        |trim: f32| StoryLayerDraw { line_fx: [0.0, trim, 1.0, 1.0], ..story_layer("route-line", source.geojson.clone(), paint_json.clone(), true, false) };

    let base = CaptureRequest { center_lon: 180.0, zoom: 7.0, ..flat_blank(640, 160, 1.0) };
    let mut session = CaptureSession::new_blocking(None).unwrap();
    let style = Style { layers: Vec::new(), ..Style::default() };
    let store = TileStore::new();
    let mid_row = base.height / 2;
    let red_run = |trim: f32, session: &mut CaptureSession| -> Vec<u32> {
        let shot = session.render(&CaptureRequest { story_layers: vec![layer(trim)], ..base.clone() }, &store, &style).unwrap();
        (0..shot.width).filter(|&x| is_red(shot.pixel(x, mid_row))).collect()
    };

    // Trim 1.0: one contiguous run, touching neither edge, 364 +- 4 px (2
    // degrees at z7: the short way across 180, not the long way around).
    let whole = red_run(1.0, &mut session);
    assert!(!whole.is_empty(), "the whole line drew nothing");
    let (min_x, max_x) = (*whole.first().unwrap(), *whole.last().unwrap());
    assert!(min_x > 0 && max_x < base.width - 1, "touches an edge: {min_x}..={max_x} of {}", base.width);
    assert!(whole.windows(2).all(|w| w[1] == w[0] + 1), "not one contiguous run: {whole:?}");
    let width = max_x - min_x + 1;
    assert!((360..=368).contains(&width), "364 +- 4 px: {width}");

    // Trim 0.5: half by length ends within 3 px of the frame centre (lon
    // 180, x = 320): the antimeridian is the exact midpoint of the route.
    let half = red_run(0.5, &mut session);
    assert!(!half.is_empty(), "trim 0.5 drew nothing");
    let half_end = *half.last().unwrap();
    assert!(half_end.abs_diff(320) <= 3, "half ends near x=320: {half_end}");

    // Monotonic progress: sampled trims only grow the run rightward (its
    // start stays pinned at progress 0), standing in for the mesh's own
    // vertex progress (Task 5), which the trim uniform gates directly.
    let mut prev_end: Option<u32> = None;
    for i in 0..=8 {
        let trim = i as f32 / 8.0;
        let cols = red_run(trim, &mut session);
        if i == 0 {
            assert!(cols.is_empty(), "trim 0 draws nothing");
            continue;
        }
        assert!(!cols.is_empty(), "trim {trim} drew nothing");
        let end = *cols.last().unwrap();
        if let Some(p) = prev_end {
            assert!(end >= p, "progress reversed between trims: {p} -> {end}");
        }
        prev_end = Some(end);
    }
}


/// **A session's first building frame is the same frame as its later
/// ones.** The edge highlight's width was `fwidth(shading.y)`, and on Metal
/// the first draws through a fresh pipeline computed it with different
/// bits: before `edge_px`, about one fresh session in fifty drew a
/// different first frame alone and one in eleven under a loaded test run
/// (this test failed 15 of 20 loaded runs). Every one-shot `capture` IS a
/// first frame. A driver race cannot be forced, so this is a sample.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn fresh_sessions_draw_the_building_scene_identically() {
    let _refused = refused_lock();
    let request = CaptureRequest {
        center_lon: 103.851, center_lat: 1.2905, zoom: 16.2,
        pitch: 60f32.to_radians(), bearing: (-25f32).to_radians(),
        width: 960, height: 540, flat: true, labels: true, label_fonts: noto(),
        extrusion_look: crate::render::extrusion::ExtrusionLook::ENHANCED,
        label_occlusion: true,
        ..CaptureRequest::default()
    };
    let style = liberty();
    let store = store_for(&request);
    for i in 0..12 {
        let shot = capture(&request, &store, &style).expect("a headless device");
        assert_eq!(shot.sha256_hex(), ENHANCED_BUILDINGS_HASH, "fresh session {i}: its first building frame differs");
    }
}

// ---- Story panels and the title card in the chrome pass (story-content 1a, Task 11) ----

fn story_panel(id: &str, placement: crate::story::script::Placement, headline: &str) -> crate::story::layout::LayoutItem {
    crate::story::layout::LayoutItem::Panel {
        id: id.into(),
        placement,
        headline: Some(headline.into()),
        body: None,
        source: None,
        image_aspect: None,
        theme: crate::story::panel::theme(Some("dark")).unwrap(),
    }
}

/// One settled (alpha 1, enter 1) item per id.
fn story_frame(items: Vec<crate::story::layout::LayoutItem>) -> std::sync::Arc<StoryFrame> {
    let motion = items.iter().map(|i| (i.id().to_string(), 1.0, 1.0)).collect();
    std::sync::Arc::new(StoryFrame { items, motion, ..StoryFrame::default() })
}

/// What `request` lays its story out as, measured with the fonts the
/// capture uses (`request.label_fonts`), and with no chrome obstacles (the
/// tests' requests name no credit, mark or wordmark).
fn story_draw(request: &CaptureRequest) -> super::story_chrome::StoryChromeDraw {
    let mut state = None;
    let fonts = super::session::fonts_for(&mut state, &request.label_fonts);
    super::story_chrome::story_chrome(request, fonts, &[])
}

fn story_layout(request: &CaptureRequest) -> crate::story::layout::Layout {
    let mut state = None;
    let fonts = super::session::fonts_for(&mut state, &request.label_fonts);
    let super::session::FontState { system, shapes, .. } = fonts;
    let cell = std::cell::RefCell::new((system, shapes));
    let measure = super::story_chrome::GlyphonMeasure::new(&cell, request.pixel_ratio);
    let frame = request.story.as_deref().unwrap();
    crate::story::layout::layout((request.width, request.height), request.pixel_ratio, &frame.safe_area, &frame.items, &[], &measure)
}

/// Task 12 review F12: a story image drawn ~4x smaller than its texture
/// (a 1-px checker, 1800 wide, into a 480-wide corner) samples its mip
/// chain and reads as an even grey, not a moire of black and white.
#[test]
fn a_story_image_drawn_four_times_smaller_does_not_alias() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let checker = std::sync::Arc::new(image::RgbaImage::from_fn(1800, 1200, |x, y| {
        if (x + y) % 2 == 0 { image::Rgba([255, 255, 255, 255]) } else { image::Rgba([0, 0, 0, 255]) }
    }));
    let id = "/events/0".to_string();
    let item = crate::story::layout::LayoutItem::Image { id: id.clone(), placement: crate::story::script::Placement::Corner, aspect: 1.5 };
    let frame = StoryFrame {
        items: vec![item],
        motion: vec![(id.clone(), 1.0, 1.0)],
        images: vec![super::StoryImage { id, image: checker, fit: crate::story::script::Fit::Contain, credit: None }],
        ..StoryFrame::default()
    };
    let req = CaptureRequest { width: 1920, height: 1080, story: Some(std::sync::Arc::new(frame)), ..base };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    dump_for_eyes(&shot, "story-image-minified-1080p.png");
    let rect = story_layout(&req).placed[0].image.unwrap();
    assert!((rect.2 - rect.0 - 480.0).abs() < 1.0, "a 480-wide corner: {rect:?}");
    let (mut n, mut sum, mut sq) = (0f64, 0f64, 0f64);
    for y in (rect.1 as u32 + 4)..(rect.3 as u32 - 4) {
        for x in (rect.0 as u32 + 4)..(rect.2 as u32 - 4) {
            let v = f64::from(shot.pixel(x, y)[0]);
            n += 1.0;
            sum += v;
            sq += v * v;
        }
    }
    let mean = sum / n;
    let sd = (sq / n - mean * mean).max(0.0).sqrt();
    assert!((90.0..=170.0).contains(&mean), "an even grey: mean {mean:.1}");
    assert!(sd < 12.0, "no aliasing: standard deviation {sd:.1} (mean {mean:.1})");
}

#[test]
fn a_lower_third_draws_its_fill_where_the_layout_put_it_and_nowhere_else() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let bare_req = CaptureRequest { width: 1920, height: 1080, ..base };
    let story = story_frame(vec![story_panel("/events/0", crate::story::script::Placement::LowerThird, "Harbour")]);
    let req = CaptureRequest { story: Some(story), ..bare_req.clone() };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    let bare = capture(&bare_req, &store, &style).unwrap();
    dump_for_eyes(&shot, "story-lower-third-1080p.png");
    let laid = story_layout(&req);
    let rect = laid.placed.iter().find(|p| p.id == "/events/0").unwrap().rect;
    let (l, t, r, b) = (rect.0.round() as i64, rect.1.round() as i64, rect.2.round() as i64, rect.3.round() as i64);
    assert!(r - l > 400 && b - t > 40 && b <= 1080 && l >= 0, "a lower-third card: {rect:?}");
    // Inside (inset 4): the dark fill (0x14 at the theme's alpha) over the map, straight alpha in the Unorm target.
    let a = f64::from(crate::story::panel::theme(Some("dark")).unwrap().fill[3]) / 255.0;
    let (mut matching, mut total) = (0u64, 0u64);
    for y in (t + 4)..(b - 4) {
        for x in (l + 4)..(r - 4) {
            let (p, m) = (shot.pixel(x as u32, y as u32), bare.pixel(x as u32, y as u32));
            let ok = (0..3).all(|c| {
                let expected = 20.0 * a + f64::from(m[c]) * (1.0 - a);
                (f64::from(p[c]) - expected).abs() <= 12.0
            });
            matching += u64::from(ok);
            total += 1;
        }
    }
    let share = matching as f64 / total as f64;
    assert!(share >= 0.90, "{:.3} of the card's pixels are its fill over the map", share);
    // A ring 8 px outside the card is the story-less frame.
    let ring = |x: i64, y: i64| {
        if x < 0 || y < 0 || x >= 1920 || y >= 1080 {
            return true;
        }
        let (p, m) = (shot.pixel(x as u32, y as u32), bare.pixel(x as u32, y as u32));
        (0..3).all(|c| p[c].abs_diff(m[c]) <= 2)
    };
    let (rl, rt, rr, rb) = (l - 8, t - 8, r + 8, b + 8);
    let mut bad = Vec::new();
    for x in rl..=rr {
        for y in [rt, rb] {
            if !ring(x, y) {
                bad.push((x, y));
            }
        }
    }
    for y in rt..=rb {
        for x in [rl, rr] {
            if !ring(x, y) {
                bad.push((x, y));
            }
        }
    }
    assert!(bad.is_empty(), "{} ring pixels changed, first {:?}", bad.len(), bad.first());
    assert!(shot.layout_warnings.is_empty(), "{:?}", shot.layout_warnings);
}

/// Mean Rec. 709 luma over the pixels outside `card`.
fn mean_luma_outside(shot: &Capture, card: (f32, f32, f32, f32)) -> f64 {
    let (mut sum, mut n) = (0.0, 0u64);
    for y in 0..shot.height {
        for x in 0..shot.width {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if fx >= card.0 && fx <= card.2 && fy >= card.1 && fy <= card.3 {
                continue;
            }
            let p = shot.pixel(x, y);
            sum += 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]);
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

#[test]
fn a_title_card_dims_the_whole_map() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let bare_req = CaptureRequest { width: 1920, height: 1080, ..base };
    let story = story_frame(vec![story_panel("/events/0", crate::story::script::Placement::Title, "The harbour, 1965")]);
    let req = CaptureRequest { story: Some(story), ..bare_req.clone() };
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    let bare = capture(&bare_req, &store, &style).unwrap();
    dump_for_eyes(&shot, "story-title-1080p.png");
    let draw = story_draw(&req);
    let dim = draw.dim.expect("a title dims the frame");
    assert_eq!((dim.screen_x, dim.screen_y, dim.width, dim.height, dim.radius_px), (0.0, 0.0, 1920.0, 1080.0, 0.0));
    assert!((dim.color[3] - crate::story::layout::TITLE_DIM).abs() < 1e-6);
    let card = draw.plates.first().expect("the title's card");
    let card = (card.screen_x, card.screen_y, card.screen_x + card.width, card.screen_y + card.height);
    assert!(card.2 - card.0 < 1920.0 * 0.9, "the card is the text's, not the frame's: {card:?}");
    let (with, without) = (mean_luma_outside(&shot, card), mean_luma_outside(&bare, card));
    assert!(with < 0.6 * without, "luma outside the card {with:.1} vs {without:.1} without the title");
}

/// The bounding box of the pixels `shot` changed against `bare`.
fn changed_bbox(shot: &Capture, bare: &Capture) -> (u32, u32, u32, u32) {
    let (mut l, mut t, mut r, mut b) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..shot.height {
        for x in 0..shot.width {
            let (p, m) = (shot.pixel(x, y), bare.pixel(x, y));
            if (0..3).any(|c| p[c].abs_diff(m[c]) > 16) {
                (l, t, r, b) = (l.min(x), t.min(y), r.max(x + 1), b.max(y + 1));
            }
        }
    }
    (l, t, r, b)
}

#[test]
fn a_4k_panel_is_the_1080p_panel_scaled_by_two() {
    let _g = refused_lock();
    let style = liberty();
    let store = TileStore::default();
    let story = story_frame(vec![story_panel("/events/0", crate::story::script::Placement::LowerThird, "Harbour")]);
    let bbox = |w: u32, h: u32, ratio: f32| {
        let bare_req = CaptureRequest { label_fonts: noto(), ..flat_blank(w, h, ratio) };
        let req = CaptureRequest { story: Some(story.clone()), ..bare_req.clone() };
        let shot = capture(&req, &store, &style).unwrap();
        let bare = capture(&bare_req, &store, &style).unwrap();
        changed_bbox(&shot, &bare)
    };
    let hd = bbox(1920, 1080, 1.0);
    let uhd = bbox(3840, 2160, 2.0);
    assert!(hd.2 > hd.0 && hd.3 > hd.1, "the 1080p panel drew: {hd:?}");
    for (edge, (a, b)) in [hd.0, hd.1, hd.2, hd.3].into_iter().zip([uhd.0, uhd.1, uhd.2, uhd.3]).enumerate() {
        assert!((i64::from(a) * 2 - i64::from(b)).abs() <= 2, "edge {edge}: 1080p {hd:?} x 2 vs 4K {uhd:?}");
    }
}

/// The burn-in with a story: a title card under a bottom caption, with the
/// credit and the mark. The caption sits at the bottom (captions draw over a
/// title, Task 9's ruling), the card's text is inside its card, and neither
/// meets the mark or the credit's plate.
#[test]
fn a_caption_over_a_title_sits_at_the_bottom_clear_of_the_credit_and_the_mark() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let caption = crate::story::timeline::ActiveCaption {
        text: "Marina Bay".into(),
        anchor: crate::story::timeline::Anchor::Bottom,
        offset_px: (0.0, 0.0),
        size_px: crate::story::timeline::DEFAULT_CAPTION_SIZE_PX,
        color: crate::story::timeline::DEFAULT_CAPTION_COLOR,
        plate: true,
        alpha: 1.0,
    };
    let mut items = vec![story_panel("/events/0", crate::story::script::Placement::Title, "The harbour")];
    items.push(crate::story::layout::LayoutItem::Caption {
        id: super::story_chrome::caption_id(0),
        text: caption.text.clone(),
        size: caption.size_px,
        anchor: caption.anchor,
        offset: (0.0, 0.0),
        plate: true,
    });
    let req = CaptureRequest {
        width: 1920,
        height: 1080,
        attribution: Some(crate::story::burn_in::OFM_CREDIT.into()),
        brand_mark: Some(crate::brand_mark::BrandMark::default()),
        captions: vec![caption],
        story: Some(story_frame(items)),
        ..base
    };
    let mut state = None;
    let fonts = super::session::fonts_for(&mut state, &req.label_fonts);
    let artwork = crate::brand_mark::decode(crate::brand_mark::SWAT_ON_LIGHT_PNG).unwrap().dimensions();
    let chrome = super::chrome::layout(&req, fonts, Some(artwork), Default::default());
    let story = chrome.story.as_ref().unwrap();
    let placed = story.captions.first().expect("the layout placed the caption");
    assert!(placed.rect.3 > 1080.0 * 0.85, "the caption is at the bottom: {:?}", placed.rect);
    let r = |p: &crate::render::PlateInstance| (p.screen_x, p.screen_y, p.screen_x + p.width, p.screen_y + p.height);
    let meets = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3;
    let credit = r(chrome.credit_plate.as_ref().unwrap());
    let mark = chrome.mark.map(|q| (q.screen_x, q.screen_y, q.screen_x + q.width, q.screen_y + q.height)).unwrap();
    let card = r(story.plates.first().unwrap());
    // Not the credit's pill, nor its (i) button inside it.
    let inside = |p: (f32, f32, f32, f32)| p.0 >= credit.0 && p.1 >= credit.1 && p.2 <= credit.2 && p.3 <= credit.3;
    let caption_plate = chrome.plates.iter().map(r).find(|p| !inside(*p)).expect("the caption's plate");
    for (name, rect) in [("card", card), ("caption", caption_plate)] {
        assert!(!meets(rect, credit) && !meets(rect, mark), "{name} {rect:?} meets the credit {credit:?} or the mark {mark:?}");
    }
    // The card's text stays inside the card.
    for ((key, left, top, ..), _) in &story.text {
        let shaped = fonts.shapes.get(key).unwrap();
        let text = (*left, *top, left + shaped.measured_width, top + shaped.measured_height);
        assert!(
            text.0 >= card.0 && text.1 >= card.1 && text.2 <= card.2 + 0.5 && text.3 <= card.3 + 0.5,
            "text {text:?} leaves its card {card:?}"
        );
    }
    let store = store_for(&req);
    let shot = capture(&req, &store, &style).unwrap();
    dump_for_eyes(&shot, "story-title-caption-1080p.png");
}

/// Identical inputs, identical frames: twice through one session and once
/// fresh, a story frame hashes the same.
#[test]
fn a_story_frame_is_deterministic() {
    let _g = refused_lock();
    let (_, base, _, style) = scenes().remove(0);
    let story = story_frame(vec![
        story_panel("/events/0", crate::story::script::Placement::Title, "The harbour"),
        story_panel("/events/1", crate::story::script::Placement::LowerThird, "Harbour"),
    ]);
    let req = CaptureRequest { story: Some(story), ..base };
    let store = store_for(&req);
    let mut session = CaptureSession::new_blocking(None).expect("device");
    let a = session.render(&req, &store, &style).unwrap();
    let b = session.render(&req, &store, &style).unwrap();
    let c = capture(&req, &store, &style).unwrap();
    assert_eq!(a.sha256_hex(), b.sha256_hex());
    assert_eq!(a.sha256_hex(), c.sha256_hex());
}

/// A panel half faded in is drawn at half its fill's alpha, slid down by
/// `panel::slide_offset`; a title never slides.
#[test]
fn a_fading_panel_slides_and_fades_its_plate() {
    let req = |motion: (f32, f32), placement| CaptureRequest {
        width: 1920,
        height: 1080,
        label_fonts: noto(),
        story: Some(std::sync::Arc::new(StoryFrame {
            items: vec![story_panel("/events/0", placement, "Harbour")],
            motion: vec![("/events/0".into(), motion.0, motion.1)],
            ..StoryFrame::default()
        })),
        ..CaptureRequest::default()
    };
    use crate::story::script::Placement;
    let settled = story_draw(&req((1.0, 1.0), Placement::LowerThird)).plates[0];
    let entering = story_draw(&req((0.5, 0.0), Placement::LowerThird)).plates[0];
    assert!((entering.color[3] - settled.color[3] * 0.5).abs() < 1e-6);
    assert!((entering.screen_y - settled.screen_y - crate::story::panel::SLIDE_UNITS).abs() < 1e-3);
    assert_eq!(entering.screen_x, settled.screen_x);
    let title_settled = story_draw(&req((1.0, 1.0), Placement::Title)).plates[0];
    let title_entering = story_draw(&req((0.5, 0.0), Placement::Title)).plates[0];
    assert_eq!(title_settled.screen_y, title_entering.screen_y);
    // An invisible panel draws nothing at all.
    let gone = story_draw(&req((0.0, 1.0), Placement::LowerThird));
    assert!(gone.plates.is_empty() && gone.text.is_empty());
}

/// The layout's warnings reach the capture's result.
#[test]
fn layout_warnings_ride_on_the_capture() {
    let _g = refused_lock();
    let style = liberty();
    let store = TileStore::default();
    let long = "A headline far too long for any lower third to hold at its nominal size ".repeat(12);
    let story = story_frame(vec![story_panel("/events/0", crate::story::script::Placement::LowerThird, &long)]);
    let req = CaptureRequest { label_fonts: noto(), story: Some(story), ..flat_blank(960, 540, 1.0) };
    let shot = capture(&req, &store, &style).unwrap();
    assert!(
        shot.layout_warnings.iter().any(|w| w.id == "/events/0" && w.code.starts_with("text_")),
        "{:?}",
        shot.layout_warnings
    );
}

/// Task 11 review F1: what is drawn is what the layout measured, with real
/// glyphon text that WRAPS -- a two-line title at 9:16 Reels, a two-line
/// body, a caption longer than the frame -- at ratio 1 and 2. Every drawn
/// entry's shaped size is its layout text rect times the ratio (within
/// 0.5 px), and a centre-justified draw breaks into as many lines as the
/// left-justified shape the layout measured.
#[test]
fn wrapped_text_draws_at_the_size_the_layout_measured() {
    use crate::story::script::Placement;
    use crate::story::tiers::{safe_area_rects, Aspect, SafeArea};
    let safe = safe_area_rects(SafeArea::Reels, Aspect::Portrait);
    let body = "Container berths moved west to Pasir Panjang in the 2000s, freeing the waterfront.";
    let caption_text = "The bay was open water until the 1970s, when reclamation pushed the shoreline a kilometre out to sea";
    let frames: Vec<Vec<crate::story::layout::LayoutItem>> = vec![
        vec![story_panel("/events/0", Placement::Title, "Marina Bay, 1965 to today")],
        vec![crate::story::layout::LayoutItem::Panel {
            id: "/events/1".into(),
            placement: Placement::LowerThird,
            headline: Some("Harbour".into()),
            body: Some(body.into()),
            source: Some("Source: MPA".into()),
            image_aspect: None,
            theme: crate::story::panel::theme(Some("dark")).unwrap(),
        }],
        vec![crate::story::layout::LayoutItem::Caption {
            id: super::story_chrome::caption_id(0),
            text: caption_text.into(),
            size: crate::story::timeline::DEFAULT_CAPTION_SIZE_PX,
            anchor: crate::story::timeline::Anchor::Bottom,
            offset: (0.0, 0.0),
            plate: true,
        }],
    ];
    let lines = |fonts: &super::session::FontState, key: &crate::symbol_pass::LabelKey| {
        fonts.shapes.get(key).expect("the draw shaped at the size and wrap the layout measured").buffer.layout_runs().count()
    };
    for ratio in [1.0f32, 2.0] {
        for (k, items) in frames.iter().enumerate() {
            let caption = crate::story::timeline::ActiveCaption {
                text: caption_text.into(),
                anchor: crate::story::timeline::Anchor::Bottom,
                offset_px: (0.0, 0.0),
                size_px: crate::story::timeline::DEFAULT_CAPTION_SIZE_PX,
                color: crate::story::timeline::DEFAULT_CAPTION_COLOR,
                plate: true,
                alpha: 1.0,
            };
            let req = CaptureRequest {
                width: (1080.0 * ratio) as u32,
                height: (1920.0 * ratio) as u32,
                pixel_ratio: ratio,
                label_fonts: noto(),
                captions: if k == 2 { vec![caption] } else { Vec::new() },
                story: Some(std::sync::Arc::new(StoryFrame {
                    items: items.clone(),
                    safe_area: safe.clone(),
                    ..StoryFrame::default()
                })),
                ..CaptureRequest::default()
            };
            let mut state = None;
            let fonts = super::session::fonts_for(&mut state, &req.label_fonts);
            let chrome = super::chrome::layout(&req, fonts, None, Default::default());
            let story = chrome.story.as_ref().unwrap();
            let laid = story_layout(&req);
            // The layout's text blocks, in draw order, against the draws.
            let blocks: Vec<_> = laid.placed.iter().flat_map(|p| p.text.iter()).filter(|t| !t.3.is_empty()).collect();
            let draws: Vec<_> = if k == 2 { chrome.text.iter().collect() } else { story.text.iter().collect() };
            assert_eq!(blocks.len(), draws.len(), "ratio {ratio} frame {k}");
            let mut wrapped = false;
            for (block, ((key, left, top, ..), _)) in blocks.iter().zip(&draws) {
                let (_, rect, units, text) = block;
                let shaped = fonts.shapes.get(key).unwrap();
                let (w, h) = ((rect.2 - rect.0) * ratio, (rect.3 - rect.1) * ratio);
                assert!(
                    (shaped.measured_width - w).abs() <= 0.5 && (shaped.measured_height - h).abs() <= 0.5,
                    "ratio {ratio} frame {k} {text:?}: drawn {}x{} vs laid out {w}x{h}",
                    shaped.measured_width,
                    shaped.measured_height
                );
                assert!((left - rect.0 * ratio).abs() <= 1.0 && (top - rect.1 * ratio).abs() <= 1.0, "{text:?}: origin");
                // The left-justified shape the layout measured, at the same size and wrap.
                let left_key = (key.0.clone(), key.1, key.2, crate::overlay::paint::TextJustify::Left, key.4.clone(), key.5);
                let measured = lines(fonts, &left_key);
                assert_eq!(lines(fonts, key), measured, "ratio {ratio} frame {k} {text:?}: line count");
                assert_eq!(f32::from_bits(key.1), units * ratio);
                wrapped |= measured > 1;
            }
            assert!(wrapped, "ratio {ratio} frame {k}: some block must wrap for this test to mean anything");
        }
    }
}

// ---- Story golden frames (story-content 1a, Task 21) ----
//
// Six story scenes, each built by the real player from a story script
// (`Player::request_at`: the timeline, the effects, the callout schedule,
// the furniture and the safe area, exactly as a render plans its frames),
// at three formats. Blessed ONCE into their own file; `capture_golden.txt`
// is not touched.

const STORY_GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/story_golden.txt");

/// `(name suffix, output.aspect, output.tier)`: 16:9 1080p (ratio 1),
/// 9:16 1080p (ratio 1, the Reels safe area by default) and 16:9 4K
/// (ratio 2).
const STORY_FORMATS: [(&str, &str, &str); 3] = [("16x9_1080p", "16:9", "fullhd"), ("9x16_1080p", "9:16", "fullhd"), ("16x9_4k", "16:9", "4k")];

/// Every story golden's name, by scene then [`STORY_FORMATS`] order.
const STORY_GOLDEN_NAMES: [(&str, [&str; 3]); 6] = [
    ("title", ["story_title_16x9_1080p", "story_title_9x16_1080p", "story_title_16x9_4k"]),
    ("lower_third", ["story_lower_third_16x9_1080p", "story_lower_third_9x16_1080p", "story_lower_third_16x9_4k"]),
    ("edge_callout", ["story_edge_callout_16x9_1080p", "story_edge_callout_9x16_1080p", "story_edge_callout_16x9_4k"]),
    ("half_drawn_route", ["story_half_drawn_route_16x9_1080p", "story_half_drawn_route_9x16_1080p", "story_half_drawn_route_16x9_4k"]),
    ("highlight", ["story_highlight_16x9_1080p", "story_highlight_9x16_1080p", "story_highlight_16x9_4k"]),
    ("legend", ["story_legend_16x9_1080p", "story_legend_9x16_1080p", "story_legend_16x9_4k"]),
];

/// The design system's goldens: `(name, scene, aspect, tier, safe area)`.
/// The slice's `design` scene twice, then the spec §14.1 entries.
const DESIGN_GOLDEN_NAMES: [(&str, &str, &str, &str, &str); 17] = [
    ("story_design_9x16_reels", "design", "9:16", "fullhd", "reels"),
    ("story_design_16x9_youtube", "design", "16:9", "fullhd", "youtube"),
    // Spec §14.1: a caption cue with no card event (§3.5's caption-only card).
    ("design_caption_9x16_1080p", "design_caption", "9:16", "fullhd", "none"),
    // Spec §14.1 (R8, §6.3): caption + stats, ember route at p = .62 with
    // its head, the start dot, the hairline; its 4K twin; the square.
    ("design_journey_9x16_1080p", "design_journey", "9:16", "fullhd", "none"),
    ("design_journey_9x16_4k", "design_journey", "9:16", "4k", "none"),
    ("design_journey_1x1_1080p", "design_journey", "1:1", "fullhd", "none"),
    // Headline + photo, the ink highlight line.
    ("design_place_9x16_1080p", "design_place", "9:16", "fullhd", "none"),
    // Quote + attribution, an arrow, an event dot with its label.
    ("design_timeline_9x16_1080p", "design_timeline", "9:16", "fullhd", "none"),
    // Headline + body, counter 3 / 5, ocean numbered stops, the current label;
    // under TikTok's safe area; landscape.
    ("design_tour_9x16_1080p", "design_tour", "9:16", "fullhd", "none"),
    ("design_tour_9x16_tiktok", "design_tour", "9:16", "fullhd", "tiktok"),
    ("design_tour_16x9_1080p", "design_tour", "16:9", "fullhd", "none"),
    // Spec §14.1 (R9, §6.3 `area`): headline + sequential legend with a
    // current tick, the fill under the labels; headline + categorical
    // legend, 4 items; the same landscape, side card, camera beside.
    ("design_area_seq_9x16_1080p", "design_area_seq", "9:16", "fullhd", "none"),
    ("design_area_cat_9x16_1080p", "design_area_cat", "9:16", "fullhd", "none"),
    ("design_area_cat_16x9_1080p", "design_area_cat", "16:9", "fullhd", "none"),
    // Spec §14.1 (R14, §5.4, §3.2, §9): a German headline that only fits
    // at 48 x 3, its stats deferred; an Arabic headline and stats pair,
    // the card and the colophon row mirrored, the numbers isolated.
    ("design_overflow_de_9x16_1080p", "design_overflow_de", "9:16", "fullhd", "none"),
    ("design_rtl_ar_9x16_1080p", "design_rtl_ar", "9:16", "fullhd", "none"),
    // Spec §3.6 (amended 2026-10-05): `design.credits: "above_platform"`,
    // the author's option -- the sources and credit lifted above reels'
    // band, the mark still bottom-left ([`DESIGN_CREDITS_ABOVE`]).
    (DESIGN_CREDITS_ABOVE, "design", "9:16", "fullhd", "reels"),
];

/// The one design golden that sets `design.credits: "above_platform"`;
/// every other entry renders the default, `bottom`.
const DESIGN_CREDITS_ABOVE: &str = "design_credits_above_9x16_reels";

/// The `area` goldens' hand-drawn districts around Marina Bay (R9), each
/// with a `use` class (unordered, so the palette: ruling R9-b keeps ramps
/// for ordered classes) and a `heat` value: `(name, use, heat, ring)`.
fn area_districts() -> Vec<serde_json::Value> {
    let districts: [(&str, &str, f64, &[[f64; 2]]); 6] = [
        ("Telok Ayer", "Offices", 31.2, &[[103.8478, 1.2795], [103.8502, 1.2832], [103.8531, 1.2858], [103.8547, 1.2842], [103.8521, 1.2806], [103.8497, 1.2776]]),
        ("Raffles Place", "Offices", 34.0, &[[103.8512, 1.2872], [103.8530, 1.2897], [103.8552, 1.2886], [103.8560, 1.2866], [103.8546, 1.2856]]),
        ("Marina Centre", "Homes", 33.1, &[[103.8532, 1.2918], [103.8571, 1.2962], [103.8622, 1.2989], [103.8668, 1.2958], [103.8637, 1.2911], [103.8589, 1.2889], [103.8553, 1.2893]]),
        ("Marina South", "Parks", 30.4, &[[103.8551, 1.2826], [103.8578, 1.2865], [103.8622, 1.2874], [103.8667, 1.2839], [103.8703, 1.2799], [103.8669, 1.2742], [103.8603, 1.2731], [103.8562, 1.2771]]),
        ("Marina East", "Port", 29.0, &[[103.8721, 1.2836], [103.8758, 1.2912], [103.8823, 1.2934], [103.8869, 1.2884], [103.8829, 1.2812], [103.8763, 1.2786]]),
        ("Gardens East", "Port", 29.6, &[[103.8690, 1.2900], [103.8716, 1.2938], [103.8748, 1.2925], [103.8731, 1.2880], [103.8702, 1.2866]]),
    ];
    districts
        .iter()
        .map(|(name, land_use, heat, ring)| {
            let mut ring: Vec<[f64; 2]> = ring.to_vec();
            ring.push(ring[0]);
            serde_json::json!({"type": "Feature", "properties": {"name": name, "use": land_use, "heat": heat},
                "geometry": {"type": "Polygon", "coordinates": [ring]}})
        })
        .collect()
}

/// A §14.1 point-role scene (R8) on the Singapore fixture tile, from
/// `test_util::minimal_json`: `None` for any other scene.
fn design_entry(scene: &str, aspect: &str, tier: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    let fc = |features: Vec<serde_json::Value>| json!({"type": "FeatureCollection", "features": features});
    let point = |name: &str, c: [f64; 2]| json!({"type": "Feature", "properties": {"name": name}, "geometry": {"type": "Point", "coordinates": c}});
    let line = |c: &[[f64; 2]]| json!({"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": c}});
    // Each scene's camera centre: its subject, a little above the frame's
    // middle (the card takes the bottom).
    let centre = match scene {
        "design_journey" => [103.8508, 1.2900],
        "design_place" => [103.8585, 1.2895],
        "design_timeline" => [103.8478, 1.2925],
        "design_area_seq" | "design_area_cat" => [103.8673, 1.2860],
        "design_overflow_de" => [103.8575, 1.2895],
        "design_rtl_ar" => [103.8600, 1.2890],
        _ => [103.8592, 1.2838],
    };
    // The areas' districts span 0.039 degrees of longitude: z14 fits them
    // in a 9:16 frame's width.
    let zoom = if scene.starts_with("design_area") { 14.0 } else { STORY_ZOOM };
    let (archetype, overlays, extra) = match scene {
        "design_journey" => {
            let route = [[103.8425, 1.2985], [103.8480, 1.2930], [103.8545, 1.2905], [103.8590, 1.2850]];
            let names = ["Clarke Quay", "Raffles Place", "Marina Bay", "Gardens by the Bay"];
            let stops: Vec<_> = names.iter().zip(route).map(|(n, c)| point(n, c)).collect();
            let mut features = vec![line(&route)];
            features.extend(stops);
            (
                "journey",
                json!([{"id": "trip", "line_metrics": true, "data": fc(features),
                    "layers": [{"id": "route", "type": "line", "role": "route"}, {"id": "stops", "type": "circle", "role": "stops"}]}]),
                json!({"chapters": [
                    {"id": "out", "title": "Out", "from": 0.0, "to": 1.5, "card": {"caption": "The water taxi leaves Clarke Quay at dawn.",
                        "stats": [{"value": "3.4", "unit": "km", "label": "Distance"}, {"value": "18", "unit": "min", "label": "By boat"}]}},
                    {"id": "back", "title": "Back", "from": 1.5, "to": 3.0, "card": {"caption": "And back by the bay."}}],
                    // p = .62 at the golden's t = 1 s.
                    "events": [{"at": 0.0, "until": 1.0 / 0.62, "overlay": {"layer": "route", "effect": "draw", "easing": "linear"}}]}),
            )
        }
        "design_place" => (
            "place",
            json!([{"id": "bridge", "data": fc(vec![line(&[[103.8560, 1.2860], [103.8600, 1.2925], [103.8610, 1.2960]])]),
                "layers": [{"id": "helix", "type": "line", "role": "highlight"}]}]),
            json!({"chapters": [{"id": "helix", "title": "Helix", "from": 0.0, "to": 2.0,
                "card": {"headline": "The Helix Bridge", "photo": {"src": "img/harbour.png", "alt": "A test gradient", "credit": "Test Studio"}}}]}),
        ),
        "design_timeline" => (
            "timeline",
            json!([{"id": "landing", "data": fc(vec![line(&[[103.8440, 1.2990], [103.8500, 1.2925]]), point("Raffles lands, 1819", [103.8515, 1.2905])]),
                "layers": [{"id": "approach", "type": "line", "role": "arrow"}, {"id": "landing-site", "type": "circle", "role": "event"}]}]),
            json!({"chapters": [{"id": "1819", "title": "1819", "from": 0.0, "to": 2.0,
                "card": {"quote": {"text": "Our object is not territory but trade.", "by": "Stamford Raffles"}}}]}),
        ),
        "design_tour" => {
            let stops = [("Merlion", [103.8545, 1.2868]), ("Esplanade", [103.8555, 1.2898]), ("Marina Bay Sands", [103.8590, 1.2838]),
                ("ArtScience Museum", [103.8595, 1.2863]), ("Gardens by the Bay", [103.8640, 1.2816])];
            let mut features = vec![line(&stops.map(|(_, c)| c))];
            features.extend(stops.iter().map(|(n, c)| point(n, *c)));
            (
                "tour",
                json!([{"id": "walk", "line_metrics": true, "data": fc(features),
                    "layers": [{"id": "path", "type": "line", "role": "tour"}, {"id": "sights", "type": "circle", "role": "numbered"}]}]),
                json!({"chapters": [{"id": "mbs", "title": "Marina Bay Sands", "from": 0.0, "to": 2.0,
                    "card": {"headline": "Marina Bay Sands", "body": "Three towers and a ship on the roof.", "index": {"i": 3, "n": 5}}}]}),
            )
        }
        // The sequential area: the card legend names the layer and holds
        // the values (default ramp, equal spacing over the heat's range).
        "design_area_seq" => (
            "area",
            json!([{"id": "districts", "attribution": "Illustrative outlines, hand-drawn", "data": fc(area_districts()),
                "layers": [{"id": "heat", "type": "fill", "role": "area"}]}]),
            json!({"chapters": [{"id": "heat", "title": "Heat", "from": 0.0, "to": 2.0,
                "card": {"headline": "The bay holds the heat",
                    "legend": {"type": "sequential", "property": "heat", "ticks": ["29°", "31.5°", "34°"], "current": 1,
                        "title": "Night surface heat, illustrative", "layers": ["heat"]}}}]}),
        ),
        // The categorical area: the layer's own legend (palette colours),
        // which the card's `legend: {layers}` shows.
        "design_area_cat" => (
            "area",
            json!([{"id": "districts", "attribution": "Illustrative outlines, hand-drawn", "data": fc(area_districts()),
                "layers": [{"id": "reclaimed", "type": "fill", "role": "area",
                    "legend": {"type": "categorical", "property": "use", "title": "Main use, illustrative",
                        "items": [{"label": "Offices"}, {"label": "Homes"}, {"label": "Parks"}, {"label": "Port"}]}}]}]),
            json!({"chapters": [{"id": "reclaimed", "title": "Reclaimed", "from": 0.0, "to": 2.0,
                "card": {"headline": "What the new land holds", "legend": {"layers": ["reclaimed"]}}}]}),
        ),
        // Spec §5.4, §3.2 (R14): a German headline that fits neither 60 x 2
        // nor 54 x 2 at the card's 836-unit text width, so it is set at
        // 48 x 3 (`text_shrunk`), and the three-line rule drops the stats
        // (`supporting_item_deferred`). The new land's edge, in ink.
        "design_overflow_de" => (
            "place",
            json!([{"id": "coast", "data": fc(vec![line(&[[103.8521, 1.2939], [103.8548, 1.2926], [103.8572, 1.2918], [103.8596, 1.2921], [103.8622, 1.2926]])]),
                "layers": [{"id": "shore", "type": "line", "role": "highlight"}]}]),
            json!({"lang": "de",
                "chapters": [{"id": "neuland", "title": "Neuland", "from": 0.0, "to": 2.0,
                    "card": {"headline": "Wie Singapur der Meerenge vor der Marina Bay jahrzehntelang Neuland abgerungen hat",
                        "stats": [{"value": "~16", "unit": "km²", "label": "Neuland seit 1965"}, {"value": "25", "unit": "%", "label": "mehr Landfläche"}]}}],
                "furniture": {"sources": [{"key": "Daten", "text": "Urban Redevelopment Authority"}]}}),
        ),
        // Spec §9 (R14): an Arabic headline with a year inside it (isolated)
        // and a stats pair; `lang: ar` mirrors the card and the colophon
        // row (a Latin `PUB` inside the Arabic source). The barrage, in ink.
        "design_rtl_ar" => (
            "place",
            json!([{"id": "barrage", "data": fc(vec![line(&[[103.8618, 1.2864], [103.8647, 1.2850]])]),
                "layers": [{"id": "dam", "type": "line", "role": "highlight"}]}]),
            json!({"lang": "ar",
                "chapters": [{"id": "sadd", "title": "سد مارينا", "from": 0.0, "to": 2.0,
                    "card": {"headline": "منذ 2008 يحوّل سد مارينا الخليج إلى خزان للمياه العذبة",
                        "stats": [{"value": "350", "unit": "م", "label": "طول السد"}, {"value": "~10,000", "unit": "هكتار", "label": "مساحة المستجمع"}]}}],
                "furniture": {"sources": [{"key": "المصدر", "text": "PUB، الوكالة الوطنية للمياه"}]}}),
        ),
        _ => return None,
    };
    let mut v = crate::story::test_util::minimal_json(json!({
        "output": {"aspect": aspect, "tier": tier, "fps": 10},
        "scene": {"style": "examples/liberty-style.json", "basemap": {"source": "fixture"}, "labels": true, "overlays": overlays},
        "camera": [{"type": "jump", "to": {"lon": centre[0], "lat": centre[1], "zoom": zoom}}, {"type": "hold", "duration": 3.0}],
        "design": {"archetype": archetype},
        "furniture": {"sources": [{"key": "Map data", "text": "OpenStreetMap"}]}
    }));
    for k in ["width", "height"] {
        v["output"].as_object_mut()?.remove(k);
    }
    for (k, e) in extra.as_object()? {
        v[k] = e.clone();
    }
    Some(v)
}

/// The story camera: Marina Bay on the fixture tile.
pub(crate) const STORY_LON: f64 = 103.851;
pub(crate) const STORY_LAT: f64 = 1.2905;
const STORY_ZOOM: f64 = 14.5;
/// Every scene is a 2 s hold at 10 fps; the golden is frame 10 (t = 1 s),
/// when every element has settled and the route's draw is half done.
pub(crate) const STORY_FRAME: usize = 10;

/// The lower third's body, chosen so its first line ends within 1% of the
/// card's wrap width at 16:9 (Task 11 review #5): a per-tier wrap
/// difference (hinting at the 4K physical size) moves a word and changes
/// the card's height, which the 1080p/4K goldens then catch.
/// `the_lower_third_body_wraps_near_its_limit` asserts the margin.
const NEAR_LIMIT_BODY: &str = "The barrage closed the channel in 2008 and turned the bay into a freshwater reservoir in town.";

/// The route's coordinates, inside the 9:16 frame's 1080 units at z14.5.
fn story_route() -> serde_json::Value {
    serde_json::json!([[103.8445, 1.2945], [103.851, 1.2880], [103.8575, 1.2862]])
}

/// The story script for `scene` at `aspect`/`tier`. Captions are burned in at
/// every aspect (`output.burn_captions`: a 16:9 story with an `aspect`
/// otherwise ships them only as a sidecar track).
pub(crate) fn story_script(scene: &str, aspect: &str, tier: &str) -> serde_json::Value {
    if let Some(v) = design_entry(scene, aspect, tier) {
        return v;
    }
    let bearing = if scene == "legend" { -20.0 } else { 0.0 };
    let mut json = serde_json::json!({
        "story": 1,
        "output": {"aspect": aspect, "tier": tier, "fps": 10, "burn_captions": true},
        "scene": {"style": "examples/liberty-style.json", "basemap": {"source": "fixture"}, "labels": true},
        "camera": [
            {"type": "jump", "to": {"lon": STORY_LON, "lat": STORY_LAT, "zoom": STORY_ZOOM, "bearing": bearing}},
            {"type": "hold", "duration": 2.0}
        ],
        "events": []
    });
    let route_overlay = |paint: serde_json::Value, kinds: bool| {
        let features: Vec<serde_json::Value> = if kinds {
            // Three routes, one per legend entry (the third is the fallback).
            [("ferry", 0.0), ("cargo", -0.0025), ("tanker", 0.0025)]
                .iter()
                .map(|(kind, dy)| {
                    let coords: Vec<serde_json::Value> = story_route()
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| serde_json::json!([c[0].as_f64().unwrap(), c[1].as_f64().unwrap() + dy]))
                        .collect();
                    serde_json::json!({"type": "Feature", "properties": {"kind": kind}, "geometry": {"type": "LineString", "coordinates": coords}})
                })
                .collect()
        } else {
            vec![serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": story_route()}})]
        };
        serde_json::json!({"id": "routes", "line_metrics": true,
            "data": {"type": "FeatureCollection", "features": features},
            "layers": [{"id": "route-line", "type": "line", "paint": paint}]})
    };
    match scene {
        "title" => {
            json["events"].as_array_mut().unwrap().push(serde_json::json!({"at": 0.0, "until": 2.0, "panel": {"placement": "title",
                "headline": "Marina Bay, 1965 to today", "body": "Sixty years of land won from the sea"}}));
            json["events"].as_array_mut().unwrap().push(serde_json::json!({"at": 0.0, "until": 2.0, "caption": {"text": "Singapore", "anchor": "bottom"}}));
        }
        "lower_third" => {
            json["events"].as_array_mut().unwrap().push(serde_json::json!({"at": 0.0, "until": 2.0, "panel": {"placement": "lower-third",
                "headline": "Harbour", "body": NEAR_LIMIT_BODY, "source": "Source: MPA",
                "image": {"src": "img/harbour.png", "alt": "A test gradient", "credit": "Photo: Test", "fit": "cover"}}}));
        }
        "edge_callout" => {
            // 40 units in from the right edge: half the frame's width in
            // 1080 units less 40, in Mercator degrees at the story zoom
            // (512 units per 360 degrees at z0).
            let (w, _) = crate::story::tiers::frame_size(
                if aspect == "9:16" { crate::story::tiers::Aspect::Portrait } else { crate::story::tiers::Aspect::Landscape },
                crate::story::tiers::Tier::Fullhd,
            );
            let lon = STORY_LON + (f64::from(w) / 2.0 - 40.0) / (512.0 * 2f64.powf(STORY_ZOOM) / 360.0);
            json["events"].as_array_mut().unwrap().push(serde_json::json!({"at": 0.0, "until": 2.0, "callout": {"at_coord": [lon, STORY_LAT], "text": "Marina Barrage", "fade_s": 0.0}}));
        }
        "half_drawn_route" => {
            json["scene"]["overlays"] = serde_json::json!([route_overlay(serde_json::json!({"line-color": "#e4572e", "line-width": 6}), false)]);
            json["events"] = serde_json::json!([{"at": 0.0, "until": 2.0, "overlay": {"layer": "route-line", "effect": "draw", "easing": "linear",
                "head": {"radius_px": 8, "color": "#ffffff"}}}]);
        }
        "highlight" => {
            let features: Vec<serde_json::Value> = [-0.006, 0.0, 0.006]
                .iter()
                .enumerate()
                .map(|(i, dx)| serde_json::json!({"type": "Feature", "id": i, "properties": {},
                    "geometry": {"type": "Point", "coordinates": [STORY_LON + dx, STORY_LAT - 0.002]}}))
                .collect();
            json["scene"]["overlays"] = serde_json::json!([{"id": "ports",
                "data": {"type": "FeatureCollection", "features": features},
                "layers": [{"id": "port-dots", "type": "circle", "paint": {"circle-color": "#1f77b4", "circle-radius": 14}}]}]);
            json["events"] = serde_json::json!([{"at": 0.0, "until": 2.0, "overlay": {"layer": "port-dots", "effect": "highlight",
                "ids": [1], "color": "#ff3b30", "dim_opacity": 0.3}}]);
        }
        "legend" => {
            let paint = serde_json::json!({"line-color": ["match", ["get", "kind"], "ferry", "#1f77b4", "cargo", "#e4572e", "#999999"], "line-width": 5});
            json["scene"]["overlays"] = serde_json::json!([route_overlay(paint, true)]);
            json["furniture"] = serde_json::json!({"source_line": "Source: Port authority", "scale_bar": "metric", "north_arrow": "auto",
                "legend": {"layers": ["route-line"], "title": "Vessel"}});
        }
        "design" => {
            // The slice: a top-level `design`, a themed route half drawn,
            // two themed stops and a chapter card with stats; fine print
            // from `furniture.sources` and the overlay's attribution.
            let stops: Vec<serde_json::Value> = [story_route()[0].clone(), story_route()[2].clone()]
                .into_iter()
                .map(|c| serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": c}}))
                .collect();
            let mut features = vec![serde_json::json!({"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": story_route()}})];
            features.extend(stops);
            json["design"] = serde_json::json!({});
            json["output"]["burn_captions"] = serde_json::json!(false);
            json["scene"]["overlays"] = serde_json::json!([{"id": "voyage", "line_metrics": true, "attribution": "Route: illustrative",
                "data": {"type": "FeatureCollection", "features": features},
                "layers": [{"id": "route", "type": "line", "role": "route"}, {"id": "stops", "type": "circle", "role": "stops"}]}]);
            json["furniture"] = serde_json::json!({"sources": [{"key": "Map data", "text": "OpenStreetMap"}]});
            json["chapters"] = serde_json::json!([{"id": "bay", "title": "Marina Bay", "from": 0.0, "to": 2.0,
                "card": {"headline": "Across Marina Bay", "stats": [{"value": "1.6", "unit": "km", "label": "Crossing"}, {"value": "6", "unit": "min", "label": "By ferry"}]}}]);
            json["events"] = serde_json::json!([{"at": 0.0, "until": 2.0, "overlay": {"layer": "route", "effect": "draw", "easing": "linear",
                "head": {"radius_px": 8, "color": "#ffffff"}}}]);
        }
        "design_caption" => {
            // Spec §3.5 (R6): a caption cue and no card, so the cue draws
            // as a caption-only card; burn-in on (as on 9:16 by default).
            json["design"] = serde_json::json!({});
            json["scene"]["overlays"] = serde_json::json!([{"id": "voyage", "line_metrics": true, "attribution": "Route: illustrative",
                "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {}, "geometry": {"type": "LineString", "coordinates": story_route()}}]},
                "layers": [{"id": "route", "type": "line", "role": "route"}]}]);
            json["furniture"] = serde_json::json!({"sources": [{"key": "Map data", "text": "OpenStreetMap"}]});
            json["captions"] = serde_json::json!([{"from": 0.0, "to": 2.0, "text": "The ferry leaves Marina South Pier at dawn."}]);
            json["events"] = serde_json::json!([{"at": 0.0, "until": 2.0, "overlay": {"layer": "route", "effect": "draw", "easing": "linear",
                "head": {"radius_px": 8, "color": "#ffffff"}}}]);
        }
        other => panic!("no story scene `{other}`"),
    }
    json
}

/// The fixture-tile scene inputs the player tests use: Liberty, the
/// shipped Noto Sans, the offline Singapore tile, images from `story_dir`.
pub(crate) fn story_inputs(story_dir: &std::path::Path) -> crate::story::SceneInputs {
    let style: serde_json::Value =
        serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json")).expect("style")).expect("json");
    crate::story::SceneInputs {
        source: std::sync::Arc::new(crate::story::fixture::FixtureSource),
        source_label: "fixture".into(),
        base_style: style,
        load_style: Box::new(|_| None),
        overlays: std::collections::HashMap::new(),
        tracks: crate::story::tracks::TrackSet::default(),
        label_fonts: noto(),
        flat_tile_zoom_max: 14,
        raster: None,
        credits: Vec::new(),
        wordmark: None,
        sprites: None,
        sprite_sheet: None,
        sprite_notes: Vec::new(),
        story_dir: story_dir.into(),
        offline: true,
        images_validated: true,
    }
}

/// The request frame [`STORY_FRAME`] of `script` plans, through the real
/// player. A deterministic 600x400 gradient sits at `img/harbour.png`.
fn story_request(script: &serde_json::Value) -> CaptureRequest {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir(dir.path().join("img")).expect("img/");
    image::RgbaImage::from_fn(600, 400, |x, y| image::Rgba([(x * 255 / 599) as u8, (y * 255 / 399) as u8, 160, 255]))
        .save(dir.path().join("img/harbour.png"))
        .expect("the test image");
    let raw = serde_json::to_vec(script).expect("json");
    let parsed = crate::story::parse(&raw).expect("the story script parses");
    let opts = crate::story::RenderOptions { cache_dir: dir.path().join("cache"), ..crate::story::RenderOptions::default() };
    let player = crate::story::Player::new(&parsed, &raw, story_inputs(dir.path()), opts).expect("the player");
    player.request_at(STORY_FRAME).0
}

/// Six story scenes at three formats (18 frames) on the Singapore
/// fixture tile with the shipped Noto Sans, no network:
/// - `title`: a title card (headline and body) with a bottom caption over it;
/// - `lower_third`: a lower third with an image beside a near-limit body;
/// - `edge_callout`: a callout pinned 40 units from the right edge (its card flips left);
/// - `half_drawn_route`: a route halfway through its `draw`, with its head;
/// - `highlight`: one of three points highlighted, the others dimmed;
/// - `legend`: a `match` line layer's legend, a metric scale bar, a
///   source line and the north arrow (the map turned 20 degrees).
///
/// Every 9:16 frame keeps out of the Reels safe area (the 9:16 default).
///
/// **The map repeats in a grid, by design.** The player's source is
/// [`crate::story::fixture::FixtureSource`], which serves the ONE offline
/// Singapore tile for every tile id it is asked for, and [`store_for`]
/// fills each frame's cover the same way: so every tile of the cover
/// draws the same content and the map reads as a repeated grid. That is
/// what keeps these frames hermetic (no network, one fixture), exactly as
/// `capture_golden.txt`'s scenes are; it is not a tiling bug.
pub(super) fn story_scenes() -> Vec<(&'static str, CaptureRequest, TileStore, Style)> {
    let mut out = Vec::new();
    for (scene, names) in STORY_GOLDEN_NAMES {
        for ((_, aspect, tier), name) in STORY_FORMATS.iter().zip(names) {
            let request = story_request(&story_script(scene, aspect, tier));
            let store = store_for(&request);
            out.push((name, request, store, liberty()));
        }
    }
    for (name, scene, aspect, tier, safe_area) in DESIGN_GOLDEN_NAMES {
        let mut script = story_script(scene, aspect, tier);
        script["output"]["safe_area"] = serde_json::json!(safe_area);
        if name == DESIGN_CREDITS_ABOVE {
            script["design"]["credits"] = serde_json::json!("above_platform");
        }
        let request = story_request(&script);
        let store = store_for(&request);
        out.push((name, request, store, liberty()));
    }
    out
}

fn read_story_golden() -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(STORY_GOLDEN).unwrap_or_default().lines()
        .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.trim().to_string())))
        .collect()
}

/// Rewrites the named entries of `story_golden.txt` and appends names it
/// does not hold yet, in file order then scene order. Refuses a name that
/// no scene renders, so a typo cannot bless nothing.
fn rebless_story_golden(existing: &str, updates: &[(String, String)], known: &[&str]) -> Result<String, String> {
    for (name, _) in updates {
        if !known.contains(&name.as_str()) {
            return Err(format!("{name}: no story golden scene has this name"));
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in existing.lines().filter(|l| !l.trim().is_empty()) {
        let name = line.split_whitespace().next().unwrap_or("");
        match updates.iter().find(|(n, _)| n == name) {
            Some((n, h)) => out.push(format!("{n} {h}")),
            None => out.push(line.to_string()),
        }
        seen.insert(name.to_string());
    }
    for (n, h) in updates.iter().filter(|(n, _)| !seen.contains(n)) {
        out.push(format!("{n} {h}"));
    }
    Ok(out.join("\n") + "\n")
}

/// The re-bless rule (spec §14.1, P1): every listed name must have been
/// rendered, and every entry NOT listed must still match its blessed
/// hash. Anything else refuses, naming each offender, so a flip that
/// moves 18 entries cannot be blessed in groups and a half-rewritten
/// file is never written.
fn rebless_or_refuse(existing: &str, collected: &[(String, String)], listed: &[String]) -> Result<String, String> {
    let blessed: std::collections::HashMap<&str, &str> =
        existing.lines().filter_map(|l| l.split_once(' ')).map(|(n, h)| (n, h.trim())).collect();
    let mut errors = Vec::new();
    for name in listed {
        if !collected.iter().any(|(n, _)| n == name) {
            errors.push(format!("{name}: listed but no story golden scene rendered it"));
        }
    }
    for (name, hash) in collected.iter().filter(|(n, _)| !listed.contains(n)) {
        match blessed.get(name.as_str()) {
            Some(want) if want == hash => {}
            Some(want) => errors.push(format!("{name}: changed but not listed (want {want} got {hash}); list it with a reason or fix the change")),
            None => errors.push(format!("{name}: new but not listed")),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let known: Vec<&str> = collected.iter().map(|(n, _)| n.as_str()).collect();
    let updates: Vec<(String, String)> = collected.iter().filter(|(n, _)| listed.contains(n)).cloned().collect();
    rebless_story_golden(existing, &updates, &known)
}

/// The layout-warning codes an entry is allowed (and required) to carry:
/// the capture's own and its design card's (spec §11), sorted. R14 adds
/// the first rows (spec §14.1).
const EXPECTED_STORY_WARNINGS: &[(&str, &[&str])] = &[
    // KNOWN DEVIATION, owed a layout look (R14 review): the square's
    // caption + stats card leaves under spec §5.6's 0.75 of the frame to
    // the map -- true since R8, first checked here. §14.2's own small-
    // frame figures put a 324-tall 1:1 card at a 0.66 share; this one
    // trips the same `card_too_tall`. Recorded, not fixed: its pixels and
    // hash are unchanged.
    ("design_journey_1x1_1080p", &["card_too_tall"]),
    ("design_overflow_de_9x16_1080p", &["supporting_item_deferred", "text_shrunk"]),
];

#[test]
fn rebless_rewrites_only_the_named_entries_and_appends_new_ones() {
    let existing = "a 111\nb 222\nc 333\n";
    let out = rebless_story_golden(existing, &[("b".into(), "999".into()), ("d".into(), "444".into())], &["a", "b", "c", "d"]).unwrap();
    assert_eq!(out, "a 111\nb 999\nc 333\nd 444\n");
}

#[test]
fn rebless_refuses_a_name_no_scene_renders() {
    let err = rebless_story_golden("a 111\n", &[("zz".into(), "1".into())], &["a"]).unwrap_err();
    assert!(err.contains("zz"), "{err}");
}

#[test]
fn a_rebless_refuses_while_an_unlisted_entry_fails() {
    let existing = "a 111\nb 222\n";
    let collected = vec![("a".to_string(), "999".to_string()), ("b".to_string(), "333".to_string())];
    let err = rebless_or_refuse(existing, &collected, &["a".to_string()]).unwrap_err();
    assert!(err.contains("b:"), "the unlisted entry that changed is named: {err}");
    let out = rebless_or_refuse(existing, &collected, &["a".to_string(), "b".to_string()]).unwrap();
    assert_eq!(out, "a 999\nb 333\n");
}

/// Task 1 (deferred minor): a scene rendered for the first time is not
/// blessed by a re-bless that does not list it -- it is named, and the
/// file is not written; listed, it is appended.
#[test]
fn a_rebless_refuses_a_new_entry_it_does_not_list() {
    let existing = "a 111\n";
    let collected = vec![("a".to_string(), "111".to_string()), ("fresh".to_string(), "777".to_string())];
    let err = rebless_or_refuse(existing, &collected, &[]).unwrap_err();
    assert!(err.contains("fresh: new but not listed"), "{err}");
    let out = rebless_or_refuse(existing, &collected, &["fresh".to_string()]).unwrap();
    assert_eq!(out, "a 111\nfresh 777\n");
}

#[test]
fn a_rebless_refuses_a_listed_name_it_did_not_render() {
    let err = rebless_or_refuse("a 111\n", &[("a".to_string(), "111".to_string())], &["zz".to_string()]).unwrap_err();
    assert!(err.contains("zz"), "{err}");
}

/// The story goldens, each frame through the one-shot path (a fresh
/// session per frame). Blessed once with `GPUI_MAP_BLESS_STORY_GOLDEN=1`,
/// which refuses when the file exists. With `GPUI_MAP_IOU_DUMP` set, each
/// frame is also written there as `<name>.png`.
///
/// Re-blessed with `GPUI_MAP_REBLESS_STORY_GOLDEN=<name>,<name>,...`
/// (which needs `GPUI_MAP_IOU_DUMP`, so the new PNGs exist for the proof):
/// [`rebless_or_refuse`] rewrites exactly the listed entries and refuses
/// -- writing nothing -- when a listed name was not rendered, or an
/// unlisted entry changed or is new (spec §14.1, P1).
///
/// Each scene must also carry exactly the layout-warning codes
/// [`EXPECTED_STORY_WARNINGS`] gives it (none when it has no row): a
/// warning gained or lost fails here even when the pixels hold.
///
/// **Cost:** about 20 s alone (37 s measured under load), the slowest
/// test in the story suite: 18 frames, each needing its own `Player` (a
/// script parse, a timeline and a layout) and a fresh one-shot capture
/// session, three of them at 4K.
///
/// Task 22 re-blessed the six route frames (`half_drawn_route`,
/// `legend`) by hand when story `line` layers moved above the 3D
/// buildings; its report holds the per-frame pixel diff. The final-review
/// fixes re-blessed ten more by hand (labels clear of story chrome, the
/// corner chrome above a platform's bottom band and what sits above it);
/// `final-fix-report.md` holds that diff. The bottom row's own 6 px inset
/// (2026-10-08) re-blessed the eighteen `story_*` corner-chrome frames: the
/// mark and the credit moved down and out, and labels the mark evicts moved
/// with it; the `design_*` frames (their own 64-unit keyline) are unchanged.
/// Moving that inset to 9 the same day (the mark read "quite low" at 6)
/// re-blessed the same corner-chrome frames again, 3 px up and in.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn every_story_golden_is_bit_identical() {
    let _refused = refused_lock();
    let bless = std::env::var("GPUI_MAP_BLESS_STORY_GOLDEN").is_ok_and(|v| v == "1");
    let rebless = std::env::var("GPUI_MAP_REBLESS_STORY_GOLDEN").ok();
    if rebless.is_some() {
        assert!(std::env::var("GPUI_MAP_IOU_DUMP").is_ok(), "re-blessing needs GPUI_MAP_IOU_DUMP so the new PNGs exist for the proof");
    }
    let golden = read_story_golden();
    let mut lines = Vec::new();
    let mut collected = Vec::new();
    let mut failures = Vec::new();
    let mut warning_failures: Vec<String> = Vec::new();
    for (name, request, store, style) in story_scenes() {
        let shot = capture(&request, &store, &style).expect("a headless device");
        assert!(shot.tiles_drawn > 0, "{name}: drew no tile");
        assert!(request.story.is_some() || !request.story_layers.is_empty(), "{name}: the player planned no story frame or layer");
        assert!(!name.starts_with("story_title") || request.captions.len() == 1, "{name}: the caption over the title is burned in");
        let card_codes = super::story_chrome::LayoutFonts::new(request.label_fonts.clone()).design_warnings(&request);
        let got: std::collections::BTreeSet<&str> = shot.layout_warnings.iter().map(|w| w.code).chain(card_codes).collect();
        let want: std::collections::BTreeSet<&str> =
            EXPECTED_STORY_WARNINGS.iter().find(|(n, _)| *n == name).map(|(_, codes)| codes.iter().copied().collect()).unwrap_or_default();
        // Gathered, not asserted here, so one run prints every entry's
        // hash; checked below before anything is blessed.
        if got != want {
            warning_failures.push(format!("{name}: layout warning codes {got:?} (expected {want:?}; raw {:?})", shot.layout_warnings));
        }
        dump_for_eyes(&shot, &format!("{name}.png"));
        let hash = shot.sha256_hex();
        eprintln!("story golden {name} {hash} tiles={} labels={}", shot.tiles_drawn, shot.labels_drawn);
        // The card and fine-print rects (P14): `story-golden-proof.sh
        // --mask auto` holds a re-bless's differing pixels to these.
        let rects = super::story_chrome::LayoutFonts::new(request.label_fonts.clone()).design_rects(&request);
        if !rects.is_empty() {
            let spec: Vec<String> =
                rects.iter().map(|r| format!("{},{},{},{}", r.0.floor(), r.1.floor(), r.2.ceil(), r.3.ceil())).collect();
            eprintln!("story mask {name} {}", spec.join(";"));
        }
        lines.push(format!("{name} {hash}"));
        collected.push((name.to_string(), hash.clone()));
        match golden.get(name) {
            Some(want) if *want == hash => {}
            Some(want) => failures.push(format!("{name}: want {want} got {hash}")),
            None if bless => {}
            None => failures.push(format!("{name}: no blessed hash (run once with GPUI_MAP_BLESS_STORY_GOLDEN=1)")),
        }
    }
    assert!(warning_failures.is_empty(), "story warnings changed:\n{}", warning_failures.join("\n"));
    if bless {
        assert!(golden.is_empty(), "the story golden file already exists; it is blessed once, never again");
        std::fs::write(STORY_GOLDEN, lines.join("\n") + "\n").expect("write the story golden");
        return;
    }
    if let Some(csv) = rebless {
        let listed: Vec<String> = csv.split(',').map(|s| s.to_string()).collect();
        let existing = std::fs::read_to_string(STORY_GOLDEN).expect("read the story golden");
        match rebless_or_refuse(&existing, &collected, &listed) {
            Ok(text) => {
                std::fs::write(STORY_GOLDEN, text).expect("write the story golden");
                return;
            }
            Err(e) => panic!("refusing to re-bless:\n{e}"),
        }
    }
    assert!(failures.is_empty(), "story frames changed:\n{}", failures.join("\n"));
}

/// Spec §6.3 (R9, ruling R9-a): an `area` draws under the basemap's
/// labels and the story's, and over the 3D buildings, whatever its
/// overlay's `above_labels` and its fill's `above_buildings` default say
/// -- its fill and its edge, in the render and in the live map alike.
#[test]
fn areas_draw_below_labels() {
    let mut script = story_script("design_area_cat", "9:16", "fullhd");
    script["scene"]["overlays"][0]["above_labels"] = serde_json::json!(true);
    let request = story_request(&script);
    let area: Vec<_> = request.story_layers.iter().filter(|l| l.layer_id.starts_with("reclaimed")).collect();
    let ids: Vec<&str> = area.iter().map(|l| &*l.layer_id).collect();
    assert_eq!(ids, ["reclaimed", "reclaimed/edge"], "the fill, then its edge");
    let slots: Vec<(bool, bool)> = area.iter().map(|l| (l.above_labels, l.above_buildings)).collect();
    assert_eq!(slots, [(false, true), (false, true)], "under every label, over the 3D buildings (R9-a)");
    let dir = tempfile::tempdir().expect("a temp dir");
    let raw = serde_json::to_vec(&script).expect("json");
    let parsed = crate::story::parse(&raw).expect("parses");
    let docs = crate::story::SceneDocuments { base_style: story_inputs(dir.path()).base_style, load_style: Box::new(|_| None), tracks: Default::default() };
    let corner = crate::story::ViewCorner { credit: None, slot_mark_w: None };
    let live = crate::story::LiveStory::plan(&parsed, dir.path(), docs, (1080.0, 1920.0), 1.0, &corner).map_err(|e| e.0).expect("the live plan").build();
    let live_area: Vec<(bool, bool, &str)> = live
        .layers()
        .iter()
        .filter(|(_, s)| s.layer_id.starts_with("reclaimed"))
        .map(|(above, s)| (*above, s.above_buildings, s.layer_id.as_str()))
        .collect();
    assert_eq!(live_area, [(false, true, "reclaimed"), (false, true, "reclaimed/edge")], "the live map's sources too");
}

/// R9's showcase: `examples/stories/marina-reclaimed.json` validates
/// with no error (R10: its 4-period legend card leaves 0.68-0.71 of the
/// frame to the map, under spec §5.6's 0.75, so `card_too_tall` is now
/// an expected warning -- the whole point of the check) and detects as
/// an `area` story (spec §2.1 rule 4) -- the non-route story the design
/// system ships with.
#[test]
fn marina_reclaimed_validates_and_its_archetype_is_area() {
    let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/stories"));
    let raw = std::fs::read(dir.join("marina-reclaimed.json")).expect("the showcase");
    let script = crate::story::parse(&raw).unwrap_or_else(|d| panic!("{d:?}"));
    assert_eq!(crate::story::design::archetype_for(&script), (crate::story::design::Archetype::Area, 4));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    let opts = crate::story::ValidateOptions { offline: true, base_dir: dir, ..crate::story::ValidateOptions::default() };
    let report = rt.block_on(crate::story::validate_scene(&script, &raw, &opts));
    let unexpected: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.severity != crate::story::Severity::Info && d.code != "card_too_tall")
        .collect();
    assert!(report.ok && unexpected.is_empty(), "{unexpected:#?}");
    // Every chapter's card shows the layer's own legend: the 4 periods.
    let path = crate::story::camera_path::CameraPath::new((1080.0, 1920.0), &script, &crate::story::tracks::TrackSet::default()).expect("the camera path");
    let (spans, _) = crate::story::cards::card_spans(&script, &path);
    assert_eq!(spans.len(), 3);
    for s in &spans {
        let Some(crate::story::card_layout::CardItem::Legend { rows, .. }) = s.content.items.first() else { panic!("{}: no legend", s.id) };
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Before 1970", "1970s", "1980s", "1990s"], "{}", s.id);
    }
}

/// Task R10's `port_to_port_design_validates_with_no_errors`: the frame
/// script every round-5 mockup rendered (`examples/stories/port-to-port-design.json`)
/// validates with no error under the design system.
#[test]
fn port_to_port_design_validates_with_no_errors() {
    let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/stories"));
    let raw = std::fs::read(dir.join("port-to-port-design.json")).expect("the port-to-port design script");
    let script = crate::story::parse(&raw).unwrap_or_else(|d| panic!("{d:?}"));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    let opts = crate::story::ValidateOptions { offline: true, base_dir: dir, ..crate::story::ValidateOptions::default() };
    let report = rt.block_on(crate::story::validate_scene(&script, &raw, &opts));
    let errors: Vec<_> = report.diagnostics.iter().filter(|d| d.severity == crate::story::Severity::Error).collect();
    assert!(report.ok && errors.is_empty(), "{errors:#?}");
    assert!(report.design.is_some(), "a `design` story reports a design object");
}

/// One session, the story goldens in reverse order: a frame never depends
/// on the frames the session drew before it.
#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "golden hashes are blessed on macOS/Metal; another GPU or driver rasterizes differently")]
fn one_session_draws_every_story_golden_in_reverse_order() {
    let _refused = refused_lock();
    let golden = read_story_golden();
    assert_eq!(golden.len(), story_scenes().len(), "every story golden scene is blessed");
    let mut session = CaptureSession::new_blocking(None).expect("device");
    for (name, request, store, style) in story_scenes().into_iter().rev() {
        let shot = session.render(&request, &store, &style).expect("render");
        assert_eq!(Some(&shot.sha256_hex()), golden.get(name), "{name} depends on the previous frame");
    }
}

/// Task 11 review #5: the lower third's body ends a line within 1% of its
/// wrap width at 16:9, at 1080p and at 4K, so the per-tier goldens see a
/// wrap that moves between tiers. A line is "near the limit" when it fills
/// at least 99% of the wrap width, or when the next word would have fit
/// within 101% of it.
#[test]
fn the_lower_third_body_wraps_near_its_limit() {
    use crate::story::layout::{TextMeasure as _, TextRole};
    let _refused = refused_lock();
    for (_, aspect, tier) in STORY_FORMATS.iter().filter(|f| f.1 == "16:9") {
        let request = story_request(&story_script("lower_third", aspect, tier));
        let laid = story_layout(&request);
        let card = laid.placed.iter().find(|p| p.text.iter().any(|t| t.0 == TextRole::Body)).expect("the lower third");
        let wrap = card.text_wrap.unwrap_or(card.rect.2 - card.rect.0 - 2.0 * crate::story::panel::PANEL_PADDING);
        let (_, _, size, text) = card.text.iter().find(|t| t.0 == TextRole::Body).unwrap();
        assert_eq!(text, NEAR_LIMIT_BODY, "{tier}: the body was not shrunk or cut");
        let mut state = None;
        let fonts = super::session::fonts_for(&mut state, &request.label_fonts);
        let super::session::FontState { system, shapes, .. } = fonts;
        let cell = std::cell::RefCell::new((system, shapes));
        let measure = super::story_chrome::GlyphonMeasure::new(&cell, request.pixel_ratio);
        let words: Vec<&str> = text.split(' ').collect();
        let (mut start, mut nearest) = (0, f32::INFINITY);
        while start < words.len() {
            let mut end = start + 1;
            while end < words.len() && measure.measure(&words[start..=end].join(" "), *size, None).0 <= wrap {
                end += 1;
            }
            let filled = measure.measure(&words[start..end].join(" "), *size, None).0 / wrap;
            let next = (end < words.len()).then(|| measure.measure(&words[start..=end].join(" "), *size, None).0 / wrap);
            eprintln!("NEAR_LIMIT {tier} wrap {wrap:.1} line {:?}: fills {filled:.4}, with the next word {next:?}", words[start..end].join(" "));
            nearest = nearest.min(1.0 - filled);
            if let Some(n) = next {
                nearest = nearest.min(n - 1.0);
            }
            start = end;
        }
        assert!(nearest <= 0.01, "{tier}: no line of the body is within 1% of the wrap width {wrap} (nearest {nearest:.4})");
    }
}

/// **Basemap labels hide behind overlay extrusions** (Kepler P4 round 2):
/// an overlay column (`CaptureRequest::extrusions`) takes part in the
/// buildings' label occlusion, with `label_occlusion` itself off. Over
/// Singapore at 45 degrees, a 500 m column on the frame's centre hides the
/// labels under and behind it and keeps the rest (fewer labels, not none);
/// the same column with `occlude_labels: false` hides nothing, so the drop
/// is the occlusion and not the column covering text.
#[test]
fn overlay_extrusions_hide_the_basemap_labels_they_stand_over() {
    use crate::overlay::extrude::{extrude, ExtrudeFeature, ExtrusionStyle};
    let _refused = refused_lock();
    let base = CaptureRequest {
        center_lon: 103.851, center_lat: 1.2905, zoom: 15.2,
        pitch: 45f32.to_radians(), bearing: 30f32.to_radians(),
        width: 640, height: 360, flat: true, labels: true, label_fonts: noto(),
        ..CaptureRequest::default()
    };
    let (lon, lat, d) = (103.851, 1.2905, 0.0015);
    let set = std::sync::Arc::new(extrude(&[ExtrudeFeature {
        polygons: vec![vec![vec![(lon - d, lat - d), (lon + d, lat - d), (lon + d, lat + d), (lon - d, lat + d), (lon - d, lat - d)]]],
        height_m: 500.0,
        base_m: 0.0,
        color: Rgba { r: 0.9, g: 0.2, b: 0.2, a: 1.0 },
    }]));
    let style = liberty();
    let store = store_for(&base);
    let with = |occlude: bool| CaptureRequest {
        extrusions: vec![CaptureExtrusion { set: set.clone(), style: ExtrusionStyle { occlude_labels: occlude, ..Default::default() } }],
        ..base.clone()
    };
    let none = capture(&base, &store, &style).expect("a headless device");
    let kept = capture(&with(false), &store, &style).expect("a headless device");
    let hidden = capture(&with(true), &store, &style).expect("a headless device");
    assert!(none.labels_drawn > 0, "the scene has labels");
    assert_eq!(kept.labels_drawn, none.labels_drawn, "occlude_labels off: every label stays");
    assert!(hidden.label_occlusion.0 > 0, "labels were tested against the column");
    assert!(
        hidden.labels_drawn > 0 && hidden.labels_drawn < none.labels_drawn,
        "the column hides some labels and keeps the ones beside it: {} of {}",
        hidden.labels_drawn,
        none.labels_drawn
    );
    dump_for_eyes(&hidden, "overlay-column-hides-labels.png");
}

/// The credit pill follows the map's tone, end to end through a capture:
/// on a light catalogue basemap (Positron) the pill is MapLibre's white one
/// with `#333` ink and a light-grey (i) button; on a dark one (Dark Matter)
/// it is `#1d2025` at 90% with `#e8eaed` ink and a `#2a2f38` button. Read
/// off the rendered pixels, not off the palette: the tone is decided from
/// the style's background in the session (`AttributionTone::for_background`).
/// `GPUI_MAP_IOU_DUMP=<dir>` writes `credit-pill-{light,dark}.png`.
#[test]
fn the_credit_pill_follows_a_light_and_a_dark_style() {
    use crate::attribution::AttributionTone;
    use crate::basemaps::Basemap;
    let _g = refused_lock();
    let (_, base, _, _) = scenes().remove(0);
    let credit = Basemap::Positron.attribution();
    for (basemap, tone) in [(Basemap::Positron, AttributionTone::Light), (Basemap::Dark, AttributionTone::Dark)] {
        assert_eq!(basemap.is_dark(), tone == AttributionTone::Dark);
        let style = crate::style_spec::parse_and_compile(basemap.style_json()).unwrap();
        let req = CaptureRequest {
            width: 1280,
            height: 720,
            pixel_ratio: 2.0,
            attribution: Some(credit.into()),
            brand_mark: Some(crate::brand_mark::BrandMark::default()),
            ..base.clone()
        };
        let store = store_for(&req);
        let shot = capture(&req, &store, &style).unwrap();
        dump_for_eyes(&shot, &format!("credit-pill-{}.png", if tone == AttributionTone::Dark { "dark" } else { "light" }));
        // Where the pill and its button are: the one shared layout.
        let mut session = CaptureSession::new_blocking(req.label_fonts.clone()).unwrap();
        let fonts = super::session::fonts_for(&mut session.fonts, &req.label_fonts);
        let corner = crate::map_view::corner_chrome(
            Some(credit),
            req.width as f32,
            req.height as f32,
            req.pixel_ratio,
            None,
            (0.0, 0.0),
            Default::default(),
            tone,
            &mut fonts.system,
            &mut fonts.shapes,
        );
        let (plate, button) = (corner.plate.unwrap(), corner.button.unwrap());
        let palette = tone.palette();
        let byte = |c: f32| (c * 255.0).round() as i32;
        let near = |p: [u8; 4], want: [i32; 3], tol: i32| (0..3).all(|c| (i32::from(p[c]) - want[c]).abs() <= tol);
        // The pill's left end, inside its padding (no ink there).
        let mid_y = (plate.screen_y + plate.height / 2.0) as u32;
        let pill = shot.pixel((plate.screen_x + 4.0 * req.pixel_ratio) as u32, mid_y);
        // The button, left of its "i".
        let knob = shot.pixel((button.screen_x + 2.0 * req.pixel_ratio) as u32, (button.screen_y + button.height / 2.0) as u32);
        let [r, g, b, a] = palette.plate;
        match tone {
            AttributionTone::Light => {
                assert!(near(pill, [255, 255, 255], 2), "light: the pill is white, got {pill:?}");
                assert!(near(knob, [0xf2, 0xf2, 0xf2], 3), "light: the button is #f2f2f2, got {knob:?}");
            }
            AttributionTone::Dark => {
                // 90% over whatever the corner holds: within 10% of 255 of
                // the opaque colour, and dark whatever is under it.
                assert!(near(pill, [byte(r), byte(g), byte(b)], byte(1.0 - a) + 2), "dark: the pill is #1d2025 at 90%, got {pill:?}");
                assert!(near(knob, [0x2a, 0x2f, 0x38], 3), "dark: the button is #2a2f38, got {knob:?}");
            }
        }
        // And the credit's ink is the tone's: pixels at the ink colour
        // inside the pill.
        let ink = palette.text.map(byte);
        let inked = (plate.screen_x as u32..(plate.screen_x + plate.width) as u32)
            .flat_map(|x| (plate.screen_y as u32..(plate.screen_y + plate.height) as u32).map(move |y| (x, y)))
            .filter(|&(x, y)| near(shot.pixel(x, y), [ink[0], ink[1], ink[2]], 24))
            .count();
        assert!(inked > 50, "{tone:?}: {inked} pixels of the credit's ink {ink:?} in the pill");
    }
}
