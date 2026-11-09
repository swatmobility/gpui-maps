//! Map stories: see docs/superpowers/specs/2026-09-29-map-stories-capture-design.md.
pub mod callout;
pub mod camera_path;
pub mod card_layout;
pub mod cards;
pub mod captions;
pub mod chapters;
pub mod design;
pub mod effects;
pub mod fly;
pub mod frame_layout;
pub mod furniture;
pub mod layout;
#[cfg(not(target_arch = "wasm32"))]
pub mod media;
#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub mod fixture;
#[cfg(not(target_arch = "wasm32"))]
pub mod bundle;
#[cfg(not(target_arch = "wasm32"))]
pub mod burn_in;
#[cfg(not(target_arch = "wasm32"))]
pub mod player;
#[cfg(not(target_arch = "wasm32"))]
pub mod prefetch;
pub mod overlay_layers;
pub mod panel;
pub mod script;
#[cfg(not(target_arch = "wasm32"))]
pub mod sink;
pub mod style_cache;
pub mod theme_layers;
pub mod tiers;
#[cfg(not(target_arch = "wasm32"))]
pub mod transcript;
pub mod timeline;
pub mod tracks;
#[cfg(not(target_arch = "wasm32"))]
pub mod validate;
#[cfg(test)]
mod fuzz_tests;
pub use camera_path::{CameraPath, Pose};
pub use chapters::{chapter_at, Chapter};
pub use script::{parse, Diagnostic, Script, Severity};
pub use effects::{EffectPlan, FeatureFx, LayerFx};
pub use overlay_layers::LayerSource;
pub use timeline::{path_and_timeline, ActiveElement, ElementKind, SceneState, Timeline};
#[cfg(not(target_arch = "wasm32"))]
pub use player::{
    default_cache_dir, live_zoom_offset, publish_table_for, Coverage, FrameRow, FrameSink, LiveStory, LiveStoryPlan, Player, Progress, RenderManifest, RenderOptions, SceneInputs,
    SpriteSheetInfo, StoryLayer, ViewCorner, STAGES,
};
#[cfg(not(target_arch = "wasm32"))]
pub use bundle::{manifest_json, relative_to_bundle, scrub_local_paths, write_bundle, write_manifest, write_manifest_value, BundleOptions, BundleReport};
#[cfg(not(target_arch = "wasm32"))]
pub use validate::{publish_preview, scene_documents, scene_inputs, scene_inputs_with_sprites, shipped_label_fonts, SceneDocuments, validate_scene, Estimate, Report, ValidateOptions};
#[cfg(not(target_arch = "wasm32"))]
pub use sink::{
    detect_ffmpeg, output_is_video, video_sink, Codec, ContactSheet, EncodeLaterSink, FfmpegChoice, FfmpegInfo, FfmpegSink, NullSink,
    PngSequenceSink, StillSink, Tee,
};

/// The camera of a [`SceneSnapshot`], in the units a story script uses.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CameraSnapshot {
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
    pub pitch_deg: f64,
    pub bearing_deg: f64,
}

/// What a live `MapView` is showing, as the pieces of a story scene
/// (`MapView::scene_snapshot`): the agent's `capture.image` with
/// `scene: "current"` renders a one-frame story from it.
///
/// Never carries a secret: `basemap` and `raster` name a source the story
/// loader knows, or a URL template whose `access_token` value is replaced
/// by the NAME of the variable it comes from ([`SceneSnapshot::redacted`]).
/// `left_out` names each thing on the map the snapshot does not carry, so a
/// still that differs from the window says why.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SceneSnapshot {
    pub camera: CameraSnapshot,
    pub globe: bool,
    /// The style JSON the current style was compiled from, when it was set
    /// with its document (`MapView::set_style_json`).
    pub style: Option<serde_json::Value>,
    /// A story basemap source name (`openfreemap`, `mapbox`, `pmtiles:...`)
    /// or a redacted tile URL template.
    pub basemap: Option<String>,
    pub raster: Option<String>,
    /// `(id, geojson)` per overlay the snapshot carries.
    pub overlays: Vec<(String, String)>,
    /// The credit the window draws (`MapView::frame_attribution`).
    pub credit: Option<String>,
    pub left_out: Vec<String>,
    /// How the map shades its buildings (`ExtrusionOverrides::look`), for
    /// the script's `scene.buildings`: a still of the current map draws
    /// the look the window draws.
    pub buildings: script::BuildingsLook,
}

/// The variable a redacted `access_token` names.
pub const ACCESS_TOKEN_NAME: &str = "MAPBOX_ACCESS_TOKEN";

impl SceneSnapshot {
    /// `url` with every `access_token=` value replaced by
    /// `${MAPBOX_ACCESS_TOKEN}`: the template names the token, never holds
    /// it.
    #[must_use]
    pub fn redacted(url: &str) -> String {
        crate::sprite::redact_access_token(url).replace("access_token=[REDACTED]", &format!("access_token=${{{ACCESS_TOKEN_NAME}}}"))
    }
}

/// `text` with every credential a URL can carry gone: an `access_token`
/// becomes the `${MAPBOX_ACCESS_TOKEN}` template (so the redacted script
/// still loads where the token is set), and a `key=`, `token=`,
/// `password=` value or a `user:pass@` authority becomes `[REDACTED]`
/// ([`crate::sprite::redact_url_credentials`]).
#[must_use]
pub fn redact_url_secrets(text: &str) -> String {
    crate::sprite::redact_url_credentials(text).replace("access_token=[REDACTED]", &format!("access_token=${{{ACCESS_TOKEN_NAME}}}"))
}

/// `script` as the manifest records it: every string passed through
/// [`redact_url_secrets`], so a keyed tiles URL, a presigned archive or a
/// `user:pass@` host never lands in `<stem>.story.json` beside a video
/// that may be published. The manifest's `script_sha256` stays the hash of
/// the raw input, so it still identifies the script that was rendered.
///
/// # Errors
/// When the script cannot be serialised (it always can).
pub fn redacted_script(script: &Script) -> serde_json::Result<serde_json::Value> {
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => {
                let clean = redact_url_secrets(s);
                if clean != *s {
                    *s = clean;
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
            serde_json::Value::Object(map) => map.values_mut().for_each(walk),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(script)?;
    walk(&mut v);
    Ok(v)
}

/// Render `script` (parsed from `raw_script`) into `sink`: a
/// [`Player`] on a 2-worker multi-thread tokio runtime built for this
/// render and shut down after it, so the caller needs no runtime.
///
/// # Errors
/// See [`Player::new`] and [`Player::run`].
#[cfg(not(target_arch = "wasm32"))]
pub fn render(
    script: &Script,
    raw_script: &[u8],
    inputs: SceneInputs,
    opts: RenderOptions,
    sink: &mut dyn FrameSink,
    progress: &mut dyn FnMut(&Progress),
) -> anyhow::Result<RenderManifest> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let manifest = {
        let _entered = rt.enter();
        let mut player = Player::new(script, raw_script, inputs, opts)?;
        player.run(sink, progress)
        // The player (and its prefetcher) drops here, before the runtime.
    };
    rt.shutdown_background();
    manifest
}

/// Test helpers shared by the story modules' tests.
#[cfg(test)]
pub(crate) mod test_util {
    /// A valid 800x600 script around `camera`, with one `bus` track
    /// declared so `follow` segments naming it pass `parse`.
    pub(crate) fn test_script(camera: serde_json::Value) -> super::Script {
        let v = serde_json::json!({
            "story": 1,
            "output": {"width": 800, "height": 600, "fps": 30},
            "scene": {
                "style": "x.json",
                "basemap": {"source": "openfreemap"},
                "tracks": [{"id": "bus", "file": "bus.csv", "style": {"color": "#1f77b4", "radius_px": 5, "trail_s": 30}}]
            },
            "camera": camera,
            "events": []
        });
        match super::parse(v.to_string().as_bytes()) {
            Ok(script) => script,
            Err(diagnostics) => panic!("test script does not parse: {diagnostics:?}"),
        }
    }

    /// A valid 1080x1920 script (Marina Bay: a jump, then a 10 s hold)
    /// with `extra`'s top-level keys merged in. An object merges one
    /// level deep into the base's object of the same key (so `"scene":
    /// {"overlays": …}` keeps the base `style` and `basemap`, and
    /// `"output": {"safe_area": "all"}` keeps the size); any other value
    /// replaces the key. Chapters in `extra` still need their `title`
    /// (`ChapterSpec::title` is required) and times within 0–10 s.
    pub(crate) fn minimal_json(extra: serde_json::Value) -> serde_json::Value {
        let mut v = serde_json::json!({
            "story": 1,
            "output": {"width": 1080, "height": 1920, "fps": 30},
            "scene": {"style": "x.json", "basemap": {"source": "openfreemap"}},
            "camera": [
                {"type": "jump", "to": {"lon": 103.851, "lat": 1.2905, "zoom": 14}},
                {"type": "hold", "duration": 10}
            ]
        });
        if let (Some(base), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            for (k, e) in extra {
                if let (Some(b), Some(e)) = (base.get_mut(k).and_then(serde_json::Value::as_object_mut), e.as_object()) {
                    b.extend(e.clone());
                    continue;
                }
                base.insert(k.clone(), e.clone());
            }
        }
        v
    }

    /// [`minimal_json`] parsed; panics with the diagnostics if it does not.
    pub(crate) fn minimal_script(extra: serde_json::Value) -> super::Script {
        let v = minimal_json(extra);
        match super::parse(v.to_string().as_bytes()) {
            Ok(script) => script,
            Err(diagnostics) => panic!("minimal_script does not parse: {diagnostics:?}\n{v}"),
        }
    }
}
