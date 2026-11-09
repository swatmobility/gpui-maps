//! glTF 2.0 binary (`.glb`) files as [`ModelMesh`]es for
//! `MapView::set_model_layer` -- behind the `models` feature.
//!
//! - **What is read**: the default scene's (or else every root node's)
//!   triangle primitives, their node transforms applied, positions,
//!   normals (computed, smooth, when a primitive has none) and indices;
//!   each glTF material becomes one [`ModelMaterial`] (its base colour
//!   factor, its gloss from `1 - roughness`). A material whose name
//!   contains one of [`GltfOptions::paint_names`] is the PAINT slot the
//!   instance colour tints. Past [`MAX_MATERIALS`] materials the rest
//!   share the last slot.
//! - **What is not**: textures (a model layer is colour per material),
//!   skins, morph targets, animation, and buffers outside the GLB's own
//!   binary chunk (an external or `data:` URI is an error): the crate is
//!   built without its file-system and image features.
//! - **Axes**: glTF is +Y up, +Z forward, +X left; the model frame is +x
//!   forward, +y left, +z up -- `(x, y, z) = (gz, gx, gy)`, a rotation, so
//!   the winding and the handedness are kept. Units are metres in both.

use std::fmt;

use glam::{Mat3, Mat4, Vec3};

use crate::render::model::{ModelMaterial, ModelMesh, ModelVertex, MAX_MATERIALS};
use crate::style::Rgba;

/// How a file is read.
#[derive(Debug, Clone, PartialEq)]
pub struct GltfOptions {
    /// Case-insensitive substrings of material names that mark the paint
    /// slot(s).
    pub paint_names: Vec<String>,
    /// Lift (or lower) the mesh so its lowest point is on the ground.
    pub ground: bool,
}

impl Default for GltfOptions {
    fn default() -> Self {
        Self { paint_names: vec!["paint".into(), "body".into(), "livery".into()], ground: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GltfError {
    /// Not a valid glTF/GLB.
    Parse(String),
    /// A buffer outside the GLB's binary chunk.
    ExternalBuffer,
    /// No triangles in the scene.
    Empty,
}

impl fmt::Display for GltfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "not a readable glTF: {e}"),
            Self::ExternalBuffer => write!(f, "the glTF names a buffer outside its GLB binary chunk"),
            Self::Empty => write!(f, "the glTF has no triangles"),
        }
    }
}

impl std::error::Error for GltfError {}

/// The glTF -> model frame rotation: `(x, y, z) = (gz, gx, gy)`.
fn axes() -> Mat4 {
    Mat4::from_cols(
        glam::Vec4::new(0.0, 1.0, 0.0, 0.0),
        glam::Vec4::new(0.0, 0.0, 1.0, 0.0),
        glam::Vec4::new(1.0, 0.0, 0.0, 0.0),
        glam::Vec4::W,
    )
}

/// Reads a `.glb` (or a self-contained `.gltf` JSON with no buffers).
pub fn load_glb(bytes: &[u8], options: &GltfOptions) -> Result<ModelMesh, GltfError> {
    let gltf = gltf::Gltf::from_slice(bytes).map_err(|e| GltfError::Parse(e.to_string()))?;
    let blob = gltf.blob.as_deref();
    for buffer in gltf.document.buffers() {
        if !matches!(buffer.source(), gltf::buffer::Source::Bin) {
            return Err(GltfError::ExternalBuffer);
        }
    }
    let mut mesh = ModelMesh::default();
    // glTF material index (None: the default material) -> slot.
    let mut slots: Vec<(Option<usize>, u32)> = Vec::new();
    let roots: Vec<gltf::Node<'_>> = match gltf.document.default_scene().or_else(|| gltf.document.scenes().next()) {
        Some(scene) => scene.nodes().collect(),
        None => gltf.document.nodes().collect(),
    };
    let mut stack: Vec<(gltf::Node<'_>, Mat4)> = roots.into_iter().map(|n| (n, axes())).collect();
    while let Some((node, parent)) = stack.pop() {
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        let normal_matrix = Mat3::from_mat4(world).inverse().transpose();
        if let Some(m) = node.mesh() {
            for primitive in m.primitives() {
                if primitive.mode() != gltf::mesh::Mode::Triangles {
                    continue;
                }
                let reader = primitive.reader(|_| blob);
                let Some(positions) = reader.read_positions() else { continue };
                let positions: Vec<Vec3> = positions.map(|p| world.transform_point3(Vec3::from_array(p))).collect();
                if positions.is_empty() {
                    continue;
                }
                let indices: Vec<u32> = match reader.read_indices() {
                    Some(i) => i.into_u32().collect(),
                    None => (0..positions.len() as u32).collect(),
                };
                if !indices.len().is_multiple_of(3) || indices.iter().any(|&i| i as usize >= positions.len()) {
                    return Err(GltfError::Parse("a primitive's indices are out of range".into()));
                }
                let normals: Vec<Vec3> = match reader.read_normals() {
                    Some(n) => n.map(|n| (normal_matrix * Vec3::from_array(n)).normalize_or(Vec3::Z)).collect(),
                    None => smooth_normals(&positions, &indices),
                };
                let material = primitive.material();
                let key = material.index();
                let slot = match slots.iter().find(|(k, _)| *k == key) {
                    Some(&(_, s)) => s,
                    None => {
                        let s = mesh.materials.len().min(MAX_MATERIALS - 1) as u32;
                        if mesh.materials.len() < MAX_MATERIALS {
                            mesh.materials.push(to_material(&material, options));
                        }
                        slots.push((key, s));
                        s
                    }
                };
                let base = mesh.vertices.len() as u32;
                mesh.vertices.extend(positions.iter().zip(&normals).map(|(p, n)| ModelVertex {
                    position: p.to_array(),
                    normal: n.to_array(),
                    material: slot,
                }));
                mesh.indices.extend(indices.iter().map(|i| base + i));
            }
        }
        stack.extend(node.children().map(|c| (c, world)));
    }
    if mesh.indices.is_empty() {
        return Err(GltfError::Empty);
    }
    if options.ground {
        let lo = mesh.vertices.iter().map(|v| v.position[2]).fold(f32::INFINITY, f32::min);
        for v in &mut mesh.vertices {
            v.position[2] -= lo;
        }
    }
    Ok(mesh)
}

fn to_material(material: &gltf::Material<'_>, options: &GltfOptions) -> ModelMaterial {
    let pbr = material.pbr_metallic_roughness();
    let [r, g, b, a] = pbr.base_color_factor();
    let name = material.name().unwrap_or("").to_lowercase();
    let paint = !name.is_empty() && options.paint_names.iter().any(|p| name.contains(&p.to_lowercase()));
    ModelMaterial { color: Rgba { r, g, b, a }, paint, gloss: (1.0 - pbr.roughness_factor()).clamp(0.0, 1.0), emissive: 0.0 }
}

fn smooth_normals(positions: &[Vec3], indices: &[u32]) -> Vec<Vec3> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for t in indices.as_chunks::<3>().0 {
        let (a, b, c) = (positions[t[0] as usize], positions[t[1] as usize], positions[t[2] as usize]);
        let n = (b - a).cross(c - a);
        for &i in t {
            acc[i as usize] += n;
        }
    }
    acc.into_iter().map(|n| n.normalize_or(Vec3::Z)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GLB: the JSON chunk (padded with spaces) and the BIN chunk
    /// (padded with zeros).
    fn glb(json: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
        let mut j = serde_json::to_vec(json).unwrap();
        while !j.len().is_multiple_of(4) {
            j.push(b' ');
        }
        let mut b = bin.to_vec();
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        let total = 12 + 8 + j.len() + 8 + b.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(j.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&j);
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&b);
        out
    }

    /// Two triangles of one quad, in two materials (a "Body Paint" and a
    /// grey "Tyre"), under a node lifted 2 m along glTF +Y (up) and moved
    /// 3 m along +Z (forward); no normals in the file.
    fn tiny() -> Vec<u8> {
        let positions: [[f32; 3]; 4] = [[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 2.0], [-1.0, 0.0, 2.0]];
        let indices: [u16; 6] = [0, 2, 1, 0, 3, 2];
        let mut bin: Vec<u8> = bytemuck::cast_slice(&positions).to_vec();
        bin.extend_from_slice(bytemuck::cast_slice(&indices));
        let json = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0, "translation": [0.0, 2.0, 3.0] }],
            "meshes": [{ "primitives": [
                { "attributes": { "POSITION": 0 }, "indices": 1, "material": 0 },
                { "attributes": { "POSITION": 0 }, "indices": 2, "material": 1 }
            ] }],
            "materials": [
                { "name": "Body Paint", "pbrMetallicRoughness": { "baseColorFactor": [1.0, 1.0, 1.0, 1.0], "roughnessFactor": 0.2 } },
                { "name": "Tyre", "pbrMetallicRoughness": { "baseColorFactor": [0.1, 0.1, 0.1, 1.0], "roughnessFactor": 1.0 } }
            ],
            "buffers": [{ "byteLength": bin.len() }],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": 48 },
                { "buffer": 0, "byteOffset": 48, "byteLength": 12 }
            ],
            "accessors": [
                { "bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-1.0, 0.0, 0.0], "max": [1.0, 0.0, 2.0] },
                { "bufferView": 1, "byteOffset": 0, "componentType": 5123, "count": 3, "type": "SCALAR" },
                { "bufferView": 1, "byteOffset": 6, "componentType": 5123, "count": 3, "type": "SCALAR" }
            ]
        });
        glb(&json, &bin)
    }

    #[test]
    fn a_tiny_glb_reads_as_a_mesh_in_the_model_frame() {
        let mesh = load_glb(&tiny(), &GltfOptions { ground: false, ..GltfOptions::default() }).expect("parses");
        assert_eq!(mesh.triangles(), 2);
        assert!(mesh.is_valid());
        assert_eq!(mesh.materials.len(), 2);
        assert!(mesh.materials[0].paint, "\"Body Paint\" is the paint slot");
        assert!(!mesh.materials[1].paint);
        assert!((mesh.materials[0].gloss - 0.8).abs() < 1e-6);
        let (lo, hi) = mesh.bounds();
        // glTF z 0..2 (+3 forward) -> x 3..5; glTF x -1..1 -> y; glTF y 0 (+2 up) -> z 2.
        assert_eq!(lo, [3.0, -1.0, 2.0]);
        assert_eq!(hi, [5.0, 1.0, 2.0]);
        // The quad lies flat, so its computed normals are vertical.
        assert!(mesh.vertices.iter().all(|v| v.normal[2].abs() > 0.999), "{:?}", mesh.vertices[0].normal);
        assert_eq!(mesh.vertices.iter().filter(|v| v.material == 1).count(), 4);
        // Grounded by default.
        let grounded = load_glb(&tiny(), &GltfOptions::default()).unwrap();
        assert_eq!(grounded.bounds().0[2], 0.0);
    }

    #[test]
    fn garbage_and_external_buffers_are_errors() {
        assert!(matches!(load_glb(b"not a model", &GltfOptions::default()), Err(GltfError::Parse(_))));
        let json = serde_json::json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "byteLength": 4, "uri": "elsewhere.bin" }]
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        assert_eq!(load_glb(&bytes, &GltfOptions::default()), Err(GltfError::ExternalBuffer));
        let empty = serde_json::to_vec(&serde_json::json!({ "asset": { "version": "2.0" } })).unwrap();
        assert_eq!(load_glb(&empty, &GltfOptions::default()), Err(GltfError::Empty));
    }
}
