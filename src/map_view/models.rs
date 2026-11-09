//! `MapView`'s model layers: one triangle mesh drawn once per instance
//! (`render::model`), in the tile pass after the buildings and the overlay
//! extrusions, sharing their depth -- a car behind a building is hidden by
//! it.
//!
//! A layer is a mesh (`Arc<ModelMesh>`, uploaded once per `Arc`), its
//! instances and a light. Moving the instances
//! ([`MapView::set_model_instances`]) is the cheap per-frame call: the
//! instances are packed on the CPU (48 B each) and uploaded on the next
//! frame; the mesh is never re-sent ([`ModelLayerStats::mesh_uploads_total`]
//! holds still).
//!
//! A layer may also move PARTS of its mesh each frame
//! ([`MapView::set_model_parts`]: one local 3x4 matrix per part, the
//! vertices naming their part, see `render::model`), and may be drawn in
//! the first-person NEAR PASS ([`MapView::set_model_near_pass`]): after the
//! whole scene, depth cleared, with its own near and far planes -- the
//! viewer's own vehicle round a low eye camera.

use std::sync::Arc;

use super::MapView;
use crate::render::model::{ModelFrameStats, ModelInstance, ModelInstances, ModelLayerDraw, ModelLight, ModelMesh, ModelNearPass, PartMatrix};

struct ModelLayer {
    id: String,
    mesh: Arc<ModelMesh>,
    instances: Vec<ModelInstance>,
    packed: ModelInstances,
    light: ModelLight,
    generation: u64,
    parts: Vec<PartMatrix>,
    near: Option<ModelNearPass>,
    /// The mesh's `(min, max)` corners, metres: kept with the mesh, for
    /// the label occluders.
    bounds: ([f32; 3], [f32; 3]),
    /// Whether the instances hide the basemap's labels behind them
    /// ([`MapView::set_model_label_occlusion`]; on by default).
    occludes_labels: bool,
}

#[derive(Default)]
pub(super) struct ModelLayers {
    layers: Vec<ModelLayer>,
    next_generation: u64,
    pub(super) stats: ModelLayerStats,
}

/// What the model layers cost, for a test or a HUD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLayerStats {
    /// The last frame's plan (zero until a frame draws a model layer).
    pub frame: ModelFrameStats,
    /// Mesh uploads since the GPU state was built.
    pub mesh_uploads_total: u64,
    /// Instance uploads since the GPU state was built.
    pub instance_uploads_total: u64,
    /// The GPU pipeline failed to build (a validation error on this
    /// device): nothing is drawn, and a caller should fall back to another
    /// way of drawing its models.
    pub failed: bool,
}

impl ModelLayers {
    pub(super) fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.packed.is_empty() || l.mesh.indices.is_empty())
    }

    /// This frame's draws, in layer order.
    /// A layer with no instances is still listed: it draws nothing, but the
    /// pipeline keeps its mesh on the GPU, so a host that empties a layer
    /// and fills it again (a camera that hides the viewer's own car) never
    /// re-uploads the mesh.
    pub(super) fn draws(&self) -> Vec<ModelLayerDraw<'_>> {
        self.layers
            .iter()
            .map(|l| ModelLayerDraw { id: &l.id, mesh: &l.mesh, generation: l.generation, instances: &l.packed, light: l.light, parts: &l.parts, near: l.near })
            .collect()
    }

    fn bump(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut ModelLayer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }

    fn get(&self, id: &str) -> Option<&ModelLayer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// Every drawn instance of every layer that occludes labels, as a box
    /// the label occlusion tests (`render::extrusion::BuildingOcclusion::with_model_occluders`).
    pub(super) fn label_occluders(&self) -> Vec<crate::render::extrusion::ModelOccluder> {
        self.layers
            .iter()
            .filter(|l| l.occludes_labels && !l.packed.is_empty() && !l.mesh.indices.is_empty())
            .flat_map(|l| l.instances.iter().filter_map(move |i| i.label_occluder(l.bounds)))
            .collect()
    }
}

impl MapView {
    /// Draws `mesh` once per instance as model layer `id`, replacing any
    /// layer of that id (its light is kept). Layers draw in the order they
    /// were first set. The mesh is uploaded on the next frame unless this
    /// layer already holds the same `Arc`.
    pub fn set_model_layer(&mut self, id: &str, mesh: Arc<ModelMesh>, instances: Vec<ModelInstance>, cx: &mut gpui::Context<Self>) {
        let generation = self.model_layers.bump();
        let packed = ModelInstances::new(&instances);
        match self.model_layers.get_mut(id) {
            Some(l) => {
                l.bounds = mesh.bounds();
                l.mesh = mesh;
                l.instances = instances;
                l.packed = packed;
                l.generation = generation;
            }
            None => self.model_layers.layers.push(ModelLayer {
                id: id.to_string(),
                bounds: mesh.bounds(),
                occludes_labels: true,
                mesh,
                instances,
                packed,
                light: ModelLight::default(),
                generation,
                parts: Vec::new(),
                near: None,
            }),
        }
        cx.notify();
    }

    /// Swaps layer `id`'s mesh, keeping its instances, light and parts
    /// (a day mesh for a night one, say). `false` when no such layer is set.
    pub fn set_model_mesh(&mut self, id: &str, mesh: Arc<ModelMesh>, cx: &mut gpui::Context<Self>) -> bool {
        let generation = self.model_layers.bump();
        let Some(l) = self.model_layers.get_mut(id) else { return false };
        l.bounds = mesh.bounds();
        l.mesh = mesh;
        l.generation = generation;
        cx.notify();
        true
    }

    /// Moves layer `id`'s instances: the cheap per-frame call (an instance
    /// upload, never the mesh). `false` when no such layer is set.
    pub fn set_model_instances(&mut self, id: &str, instances: Vec<ModelInstance>, cx: &mut gpui::Context<Self>) -> bool {
        let generation = self.model_layers.bump();
        let Some(l) = self.model_layers.get_mut(id) else { return false };
        if l.instances == instances {
            return true;
        }
        l.packed = ModelInstances::new(&instances);
        l.instances = instances;
        l.generation = generation;
        cx.notify();
        true
    }

    /// Layer `id`'s light: a uniform write, no upload.
    pub fn set_model_light(&mut self, id: &str, light: ModelLight, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.model_layers.get_mut(id) {
            if l.light != light {
                l.light = light;
                cx.notify();
            }
        }
    }

    /// Layer `id`'s part transforms: one local 3x4 matrix per part id
    /// 1..=`parts.len()` (at most `render::model::MAX_PARTS`), applied in
    /// the mesh's frame before the instance's placement, shared by every
    /// instance. The cheap per-frame call for moving parts (a 48 B row a
    /// part, no mesh or instance upload). `false` when no such layer is set.
    pub fn set_model_parts(&mut self, id: &str, parts: Vec<PartMatrix>, cx: &mut gpui::Context<Self>) -> bool {
        let Some(l) = self.model_layers.get_mut(id) else { return false };
        if l.parts != parts {
            l.parts = parts;
            cx.notify();
        }
        true
    }

    /// Draws layer `id` in the first-person near pass (`Some`) or with the
    /// rest of the scene (`None`): see `render::model::ModelNearPass`.
    pub fn set_model_near_pass(&mut self, id: &str, near: Option<ModelNearPass>, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.model_layers.get_mut(id) {
            if l.near != near {
                l.near = near;
                cx.notify();
            }
        }
    }

    /// Whether layer `id`'s instances hide the basemap's labels behind
    /// them and under them (on by default): a street name never paints
    /// over a 3D bus. Off for small props a label should stay over (people
    /// at a stop). Mapbox GL JS v3 hides symbols behind its 3D models; the
    /// test is the buildings' label occlusion's (`render::extrusion::ModelOccluder`),
    /// with its fade, on flat frames.
    pub fn set_model_label_occlusion(&mut self, id: &str, on: bool, cx: &mut gpui::Context<Self>) {
        if let Some(l) = self.model_layers.get_mut(id) {
            if l.occludes_labels != on {
                l.occludes_labels = on;
                cx.notify();
            }
        }
    }

    /// Stops drawing model layer `id`; its GPU buffers go on the next frame.
    pub fn remove_model_layer(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let before = self.model_layers.layers.len();
        self.model_layers.layers.retain(|l| l.id != id);
        if self.model_layers.layers.len() != before {
            cx.notify();
        }
    }

    /// Layer `id`'s mesh and instances as drawn now, or `None` when no such
    /// layer is set.
    #[must_use]
    pub fn model_layer(&self, id: &str) -> Option<(Arc<ModelMesh>, &[ModelInstance])> {
        self.model_layers.get(id).map(|l| (l.mesh.clone(), l.instances.as_slice()))
    }

    /// What the model layers cost so far (see [`ModelLayerStats`]).
    #[must_use]
    pub fn model_layer_stats(&self) -> ModelLayerStats {
        self.model_layers.stats
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;

    use super::MapView;
    use crate::render::model::{ModelInstance, ModelMaterial, ModelMesh, ModelVertex};
    use crate::style::Rgba;

    fn view() -> MapView {
        super::super::HEADLESS_PAINT.with(|h| h.set(true));
        let handle = crate::map_view::test_runtime_handle();
        MapView::new(
            103.85,
            1.30,
            18.0,
            800.0,
            600.0,
            crate::style::Style { layers: vec![], sprite: None, sky: Default::default() },
            Arc::new(crate::tile_source::EmptyTileSource),
            handle,
        )
    }

    fn mesh() -> Arc<ModelMesh> {
        let v = |x: f32, y: f32| ModelVertex { position: [x, y, 1.0], normal: [0.0, 0.0, 1.0], material: 0 };
        Arc::new(ModelMesh {
            vertices: vec![v(0.0, 0.0), v(1.0, 0.0), v(0.0, 1.0)],
            indices: vec![0, 1, 2],
            materials: vec![ModelMaterial::paint(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, 0.5)],
        })
    }

    /// The layer keeps its mesh `Arc` through instance updates (so the GPU
    /// never re-uploads it), a repeat update is no change, and an update to
    /// a layer that is not set says so.
    #[test]
    fn instance_updates_keep_the_mesh_and_move_only_the_instances() {
        let mut app = TestAppContext::single();
        let (map, cx) = app.add_window_view(|_, _| view());
        let mesh = mesh();
        let a = vec![ModelInstance::at(103.85, 1.30, 0.0)];
        let b = vec![ModelInstance::at(103.851, 1.30, 90.0), ModelInstance::at(103.852, 1.30, 180.0)];
        map.update(cx, |m, cx| {
            assert!(!m.set_model_instances("cars", a.clone(), cx), "no such layer yet");
            m.set_model_layer("cars", mesh.clone(), a.clone(), cx);
            let generation = m.model_layers.get("cars").unwrap().generation;
            assert!(m.set_model_instances("cars", a.clone(), cx));
            assert_eq!(m.model_layers.get("cars").unwrap().generation, generation, "the same instances are no change");
            assert!(m.set_model_instances("cars", b.clone(), cx));
            let l = m.model_layers.get("cars").unwrap();
            assert_ne!(l.generation, generation);
            assert_eq!(l.packed.len(), 2);
            let (held, instances) = m.model_layer("cars").unwrap();
            assert!(Arc::ptr_eq(&held, &mesh), "the mesh Arc is kept: no re-upload");
            assert_eq!(instances, b.as_slice());
            assert!(!m.model_layers.is_empty());
            assert_eq!(m.model_layers.draws().len(), 1);
            // Parts and the near pass ride along to the draw.
            assert!(!m.set_model_parts("none", vec![], cx), "no such layer");
            let slide = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 2.0], [0.0, 0.0, 1.0, 0.0]];
            assert!(m.set_model_parts("cars", vec![slide], cx));
            m.set_model_near_pass("cars", Some(crate::render::model::ModelNearPass::FIRST_PERSON), cx);
            let draws = m.model_layers.draws();
            assert_eq!(draws[0].parts, &[slide][..]);
            assert_eq!(draws[0].near, Some(crate::render::model::ModelNearPass::FIRST_PERSON));
            drop(draws);
            let (held, _) = m.model_layer("cars").unwrap();
            assert!(Arc::ptr_eq(&held, &mesh), "parts never touch the mesh");
            m.remove_model_layer("cars", cx);
            assert!(m.model_layer("cars").is_none());
            assert!(m.model_layers.is_empty());
            assert!(!m.model_layer_stats().failed);
        });
    }
}
