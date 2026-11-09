//! Spec §8.1: the story pipeline never panics on a mutated script. Every
//! stage is run on each mutation: `parse`, `CameraPath::new` and 50
//! `pose_at` samples, `Timeline::new` over a style holding the seeds'
//! layers, then on every timeline built `validate_style_edits` (every
//! generation through `style_spec::parse_and_compile`) and 50 `state_at`
//! samples through a `StyleCache`, and the track parser over the same
//! bytes. Declared tracks are given a synthetic log, so a `follow`
//! resolves. Every `Err` must carry at least one
//! `Severity::Error` diagnostic.

use crate::story::script::Severity;

/// Spec §8.1: 10 000 mutations of the example stories never panic.
#[test]
fn ten_thousand_mutated_scripts_never_panic() {
    use std::hash::{Hash, Hasher};
    let seeds = SEEDS;
    let mut panics = Vec::new();
    let mut empty_errs = Vec::new();
    let mut reached = 0u32;
    for i in 0..10_000u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        i.hash(&mut h);
        let r = h.finish();
        // The seed from high bits: `mutate` picks its kind from `r % 5`,
        // and a seed chosen by `r % 4` (as first written, with four seeds)
        // tied each seed to one kind -- every mutation of the fourth seed
        // was a truncation.
        let seed_index = ((r >> 40) % seeds.len() as u64) as usize;
        let seed = seeds[seed_index];
        let bytes = mutate(seed.as_bytes(), r);
        let result = std::panic::catch_unwind(|| {
            let has_error = |d: &[crate::story::script::Diagnostic]| d.iter().any(|d| d.severity == Severity::Error);
            let mut ok = true;
            if let Err(e) = crate::story::tracks::parse_track(&bytes, "f", Default::default(), 0.0) {
                ok &= e.severity == Severity::Error;
            }
            let script = match crate::story::script::parse(&bytes) {
                Ok(script) => script,
                Err(d) => return (ok && has_error(&d), false),
            };
            // Every track the script declares, as a short synthetic log,
            // so `follow` segments resolve and mutations reach the timeline.
            let tracks = crate::story::tracks::TrackSet {
                tracks: script
                    .scene
                    .tracks
                    .iter()
                    .filter_map(|spec| {
                        crate::story::tracks::parse_track(b"t,lon,lat\n0,103.80,1.30\n30,103.85,1.32\n60,103.90,1.35\n", &spec.id, spec.style.clone(), 0.0).ok()
                    })
                    .collect(),
            };
            let path = match crate::story::camera_path::CameraPath::new((800.0, 600.0), &script, &tracks) {
                Ok(path) => path,
                Err(d) => return (ok && has_error(&d), false),
            };
            for j in 0..50 {
                let p = path.pose_at(path.duration() * j as f64 / 49.0);
                assert!(p.camera.zoom.is_finite() && p.camera.center_lon.is_finite() && p.camera.center_lat.is_finite());
                // R1: a pose is always drawable, whatever the easing overshoot.
                assert!((crate::camera_control::MIN_ZOOM..=crate::camera_control::MAX_ZOOM).contains(&p.camera.zoom), "zoom {}", p.camera.zoom);
                assert!(p.camera.center_lat.abs() <= crate::camera_control::MAX_MERCATOR_LAT, "lat {}", p.camera.center_lat);
            }
            match crate::story::timeline::Timeline::new(&script, &path, fuzz_style(), &|_| Some(fuzz_style()), &[]) {
                Ok(tl) => {
                    // The compile path too: every generation through
                    // `parse_and_compile`, and the cache over the samples.
                    for d in crate::story::style_cache::validate_style_edits(&tl) {
                        ok &= d.severity == Severity::Error && !d.path.is_empty();
                    }
                    let mut cache = crate::story::style_cache::StyleCache::new();
                    for j in 0..50 {
                        let state = tl.state_at(path.duration() * j as f64 / 49.0, script.output().fps);
                        let _ = cache.get(&tl, state.style_generation);
                    }
                    BUILT.with(|b| b.set(b.get() + 1));
                    BUILT_PER_SEED.with(|b| b.borrow_mut()[seed_index] += 1);
                    (ok, true)
                }
                // Reached the timeline stage: that is what is counted.
                Err(d) => (ok && has_error(&d), true),
            }
        });
        match result {
            Err(_) => panics.push((i, String::from_utf8_lossy(&bytes).into_owned())),
            Ok((false, _)) => empty_errs.push((i, String::from_utf8_lossy(&bytes).into_owned())),
            Ok((true, timeline_stage)) => reached += u32::from(timeline_stage),
        }
    }
    for (i, text) in &panics { eprintln!("PANICINPUT {i} {text}"); }
    assert!(panics.is_empty(), "{} panics; first: {:?}", panics.len(), panics.first());
    assert!(empty_errs.is_empty(), "{} Errs with no Error; first: {:?}", empty_errs.len(), empty_errs.first());
    // The harness is only worth something if mutations reach the later
    // stages: a good share must parse and resolve a camera path, so that
    // `Timeline::new` runs on them. (Few build a whole timeline: the
    // seeds' layer edits name layers the empty style lacks.)
    assert!(reached > 1_000, "only {reached} of 10 000 mutations reached Timeline::new");
    let built = BUILT.with(|b| b.get());
    eprintln!("fuzz: {reached} of 10 000 mutations reached Timeline::new, {built} built a timeline");
    assert!(built > 2_000, "only {built} of 10 000 mutations built a timeline");
    // N7: the aggregate floor alone would let one seed (say spec-example,
    // the only one with layer edits) stop building while the others keep
    // the total up. Each seed must build its own share.
    let per_seed = BUILT_PER_SEED.with(|b| *b.borrow());
    eprintln!("fuzz: built per seed {per_seed:?}");
    for (i, n) in per_seed.iter().enumerate() {
        assert!(*n >= 200, "seed {i} built only {n} timelines: {per_seed:?}");
    }
}

/// The seed stories every mutation starts from.
const SEEDS: [&str; 4] = [
    include_str!("../../tests/fixtures/stories/minimal.json"),
    include_str!("../../tests/fixtures/stories/spec-example.json"),
    include_str!("../../tests/fixtures/stories/showcase.json"),
    include_str!("../../tests/fixtures/stories/task10-follow.json"),
];

std::thread_local! {
    /// Timelines built (catch_unwind's closure cannot borrow a counter).
    static BUILT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Timelines built, per seed (N7).
    static BUILT_PER_SEED: std::cell::RefCell<[u32; SEEDS.len()]> = const { std::cell::RefCell::new([0; SEEDS.len()]) };
}

/// A style holding every layer the seed stories edit (`water`,
/// `road-primary`, `poi-label`) plus a background, over one vector source,
/// so mutated edits get through `Timeline::new` and reach the compiler.
fn fuzz_style() -> serde_json::Value {
    serde_json::json!({"version":8,"sources":{"s":{"type":"vector","tiles":["http://x/{z}/{x}/{y}.pbf"]}},"layers":[
        {"id":"bg","type":"background","paint":{"background-color":"#fff"}},
        {"id":"water","type":"fill","source":"s","source-layer":"water","paint":{"fill-color":"#00f"}},
        {"id":"road-primary","type":"line","source":"s","source-layer":"road","filter":["==",["get","class"],"primary"],"paint":{"line-color":"#888","line-width":2}},
        {"id":"poi-label","type":"symbol","source":"s","source-layer":"poi","layout":{"text-field":["get","name"],"text-size":12},"paint":{"text-color":"#333"}}
    ]})
}

/// Flip a type, delete a key, insert an extreme number, truncate, or
/// nudge a number, chosen by `r`.
fn mutate(seed: &[u8], r: u64) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(seed).unwrap();
    let extremes = [serde_json::json!(1e308), serde_json::json!(-1e308), serde_json::json!(0), serde_json::json!(-0.0), serde_json::json!(u64::MAX), serde_json::json!("x"), serde_json::json!(null), serde_json::json!([]), serde_json::json!({})];
    let mut leaves = Vec::new();
    collect_pointers(&v, String::new(), &mut leaves);
    let target = leaves[(r >> 8) as usize % leaves.len()].clone();
    match r % 5 {
        // Nudge a number: the script stays well-typed, so the mutation
        // reaches the camera path, the timeline and the compiler with
        // values that are merely unusual (negative, zero, far, tiny).
        4 => {
            let numbers: Vec<&String> = leaves.iter().filter(|p| v.pointer(p).is_some_and(serde_json::Value::is_number)).collect();
            if let Some(target) = numbers.get((r >> 8) as usize % numbers.len().max(1)).map(|p| (*p).clone()) {
                let factors = [-1.0, 0.0, 0.5, 2.0, 10.0, 1e-9, 1.000001, 100.0];
                if let Some(x) = v.pointer_mut(&target) {
                    let n = x.as_f64().unwrap_or(0.0) * factors[(r >> 16) as usize % factors.len()];
                    *x = serde_json::json!(n);
                }
            }
        }
        0 => { if let Some(x) = v.pointer_mut(&target) { *x = extremes[(r >> 16) as usize % extremes.len()].clone(); } }
        1 => { if let Some((parent, key)) = target.rsplit_once('/') { if let Some(serde_json::Value::Object(m)) = v.pointer_mut(parent) { m.remove(key); } } }
        2 => { if let Some(x) = v.pointer_mut(&target) { *x = match x.take() { serde_json::Value::Number(n) => n.to_string().into(), serde_json::Value::String(s) => s.len().into(), other => serde_json::json!([other]) }; } }
        _ => { let b = serde_json::to_vec(&v).unwrap(); let cut = (r >> 20) as usize % b.len(); return b[..cut].to_vec(); }
    }
    serde_json::to_vec(&v).unwrap()
}

fn collect_pointers(v: &serde_json::Value, at: String, out: &mut Vec<String>) {
    out.push(at.clone());
    match v { serde_json::Value::Object(m) => for (k, x) in m { collect_pointers(x, format!("{at}/{k}"), out) }, serde_json::Value::Array(a) => for (i, x) in a.iter().enumerate() { collect_pointers(x, format!("{at}/{i}"), out) }, _ => {} }
}
