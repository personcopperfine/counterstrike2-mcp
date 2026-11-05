//! glTF/GLB fixture builder for the model-import tests. Pure bytes: no files unless asked, and
//! those go under the caller's directory (always below `CARGO_TARGET_TMPDIR`, never /tmp).
#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A 1x1 PNG (only the signature matters to the importer, which passes bytes through).
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41,
    0x54, 0x78, 0x9C, 0x63, 0x60, 0x60, 0x60, 0xF8, 0x0F, 0x00, 0x01, 0x01, 0x01, 0x00, 0x1B, 0xB6, 0xEE, 0x56, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[derive(Clone, Default)]
pub struct Prim {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub indices: Option<Vec<u32>>,
    pub material: Option<usize>,
}

#[derive(Clone, Default)]
pub struct Mat {
    pub name: String,
    pub base_color: [f32; 4],
    /// indices into [`Fixture::images`]
    pub base_tex: Option<usize>,
    pub normal_tex: Option<usize>,
    pub mr_tex: Option<usize>,
}

#[derive(Clone, Default)]
pub struct Fixture {
    pub prims: Vec<Prim>,
    pub materials: Vec<Mat>,
    pub images: Vec<Vec<u8>>,
    /// column-major node matrix of the single mesh node
    pub node_matrix: Option<[f32; 16]>,
    pub extensions_required: Vec<String>,
    /// adds a skin (one joint) to the file
    pub skinned: bool,
}

fn pad4(v: &mut Vec<u8>, byte: u8) {
    while !v.len().is_multiple_of(4) {
        v.push(byte);
    }
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// How the binary data is stored.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// GLB: buffer 0 is the BIN chunk, images are bufferViews
    Glb,
    /// .gltf with `data:` URIs
    DataUri,
    /// .gltf with `scene.bin` and `image<N>.png` beside it
    External,
}

impl Fixture {
    /// A unit cube (side 1, centred), 24 vertices with normals and UVs, one material-less primitive.
    pub fn cube() -> Fixture {
        let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
            ([0., 0., 1.], [[-0.5, -0.5, 0.5], [0.5, -0.5, 0.5], [0.5, 0.5, 0.5], [-0.5, 0.5, 0.5]]),
            ([0., 0., -1.], [[-0.5, 0.5, -0.5], [0.5, 0.5, -0.5], [0.5, -0.5, -0.5], [-0.5, -0.5, -0.5]]),
            ([1., 0., 0.], [[0.5, -0.5, -0.5], [0.5, 0.5, -0.5], [0.5, 0.5, 0.5], [0.5, -0.5, 0.5]]),
            ([-1., 0., 0.], [[-0.5, 0.5, -0.5], [-0.5, -0.5, -0.5], [-0.5, -0.5, 0.5], [-0.5, 0.5, 0.5]]),
            ([0., 1., 0.], [[0.5, 0.5, -0.5], [-0.5, 0.5, -0.5], [-0.5, 0.5, 0.5], [0.5, 0.5, 0.5]]),
            ([0., -1., 0.], [[-0.5, -0.5, -0.5], [0.5, -0.5, -0.5], [0.5, -0.5, 0.5], [-0.5, -0.5, 0.5]]),
        ];
        let mut prim = Prim { normals: Some(vec![]), uvs: Some(vec![]), indices: Some(vec![]), ..Prim::default() };
        for (n, quad) in faces {
            let base = prim.positions.len() as u32;
            for (i, p) in quad.iter().enumerate() {
                prim.positions.push(*p);
                prim.normals.as_mut().unwrap().push(n);
                prim.uvs.as_mut().unwrap().push([[0., 1.], [1., 1.], [1., 0.], [0., 0.]][i]);
            }
            prim.indices.as_mut().unwrap().extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        Fixture { prims: vec![prim], ..Fixture::default() }
    }

    /// One counter-clockwise triangle with the asymmetric corners glTF (+1,0,0), (0,+2,0),
    /// (0,0,+3): every axis distinct, so a wrong axis mapping cannot hide. UVs (0,0), (1,0), (0,1).
    pub fn corner_triangle() -> Fixture {
        Fixture {
            prims: vec![Prim {
                positions: vec![[1., 0., 0.], [0., 2., 0.], [0., 0., 3.]],
                normals: Some(vec![[6., 3., 2.]; 3].into_iter().map(|n: [f32; 3]| {
                    let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    [n[0] / l, n[1] / l, n[2] / l]
                }).collect()),
                uvs: Some(vec![[0., 0.], [1., 0.], [0., 1.]]),
                indices: Some(vec![0, 1, 2]),
                material: None,
            }],
            ..Fixture::default()
        }
    }

    /// A 1x1 quad (XY plane, facing +Z) with a base-colour PNG.
    pub fn textured_quad() -> Fixture {
        Fixture {
            prims: vec![Prim {
                positions: vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]],
                normals: Some(vec![[0., 0., 1.]; 4]),
                uvs: Some(vec![[0., 1.], [1., 1.], [1., 0.], [0., 0.]]),
                indices: Some(vec![0, 1, 2, 0, 2, 3]),
                material: Some(0),
            }],
            materials: vec![Mat { name: "Brick Wall".into(), base_color: [0.5, 0.25, 1.0, 1.0], base_tex: Some(0), ..Mat::default() }],
            images: vec![TINY_PNG.to_vec()],
            ..Fixture::default()
        }
    }

    /// Two quads, two materials (the second has no texture).
    pub fn two_materials() -> Fixture {
        let mut f = Fixture::textured_quad();
        let mut second = f.prims[0].clone();
        for p in &mut second.positions {
            p[0] += 2.0;
        }
        second.material = Some(1);
        f.prims.push(second);
        f.materials.push(Mat { name: "Plain".into(), base_color: [1.0, 0.0, 0.0, 1.0], ..Mat::default() });
        f
    }

    /// Everything the PBR subset carries: base colour, normal map and a packed metal-rough map.
    pub fn full_pbr() -> Fixture {
        let mut f = Fixture::textured_quad();
        f.images.push(TINY_PNG.to_vec());
        f.images.push(TINY_PNG.to_vec());
        f.materials[0].normal_tex = Some(1);
        f.materials[0].mr_tex = Some(2);
        f
    }

    /// `count` copies of one triangle (for the size cap).
    pub fn many_triangles(count: usize) -> Fixture {
        Fixture {
            prims: vec![Prim {
                positions: vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
                normals: Some(vec![[0., 0., 1.]; 3]),
                uvs: Some(vec![[0., 0.]; 3]),
                indices: Some((0..count).flat_map(|_| [0u32, 1, 2]).collect()),
                material: None,
            }],
            ..Fixture::default()
        }
    }

    fn build(&self, storage: Storage, dir: Option<&Path>) -> (Value, Vec<u8>) {
        let mut bin: Vec<u8> = Vec::new();
        let mut views: Vec<Value> = Vec::new();
        let mut accessors: Vec<Value> = Vec::new();
        let mut view = |bin: &mut Vec<u8>, data: &[u8], target: Option<u32>| -> usize {
            pad4(bin, 0);
            let mut v = json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": data.len()});
            if let Some(t) = target {
                v["target"] = json!(t);
            }
            bin.extend_from_slice(data);
            views.push(v);
            views.len() - 1
        };
        let mut primitives = Vec::new();
        for prim in &self.prims {
            let mut attributes = json!({});
            let flat: Vec<f32> = prim.positions.iter().flatten().copied().collect();
            let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
            for p in &prim.positions {
                for k in 0..3 {
                    min[k] = min[k].min(p[k]);
                    max[k] = max[k].max(p[k]);
                }
            }
            let v = view(&mut bin, &floats(&flat), Some(34962));
            accessors.push(json!({"bufferView": v, "componentType": 5126, "count": prim.positions.len(), "type": "VEC3", "min": min, "max": max}));
            attributes["POSITION"] = json!(accessors.len() - 1);
            if let Some(normals) = &prim.normals {
                let flat: Vec<f32> = normals.iter().flatten().copied().collect();
                let v = view(&mut bin, &floats(&flat), Some(34962));
                accessors.push(json!({"bufferView": v, "componentType": 5126, "count": normals.len(), "type": "VEC3"}));
                attributes["NORMAL"] = json!(accessors.len() - 1);
            }
            if let Some(uvs) = &prim.uvs {
                let flat: Vec<f32> = uvs.iter().flatten().copied().collect();
                let v = view(&mut bin, &floats(&flat), Some(34962));
                accessors.push(json!({"bufferView": v, "componentType": 5126, "count": uvs.len(), "type": "VEC2"}));
                attributes["TEXCOORD_0"] = json!(accessors.len() - 1);
            }
            let mut primitive = json!({"attributes": attributes});
            if let Some(indices) = &prim.indices {
                let data: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
                let v = view(&mut bin, &data, Some(34963));
                accessors.push(json!({"bufferView": v, "componentType": 5125, "count": indices.len(), "type": "SCALAR"}));
                primitive["indices"] = json!(accessors.len() - 1);
            }
            if let Some(m) = prim.material {
                primitive["material"] = json!(m);
            }
            primitives.push(primitive);
        }
        // images
        let mut images = Vec::new();
        for (n, bytes) in self.images.iter().enumerate() {
            match storage {
                Storage::Glb => {
                    let v = view(&mut bin, bytes, None);
                    images.push(json!({"bufferView": v, "mimeType": "image/png"}));
                }
                Storage::DataUri => images.push(json!({"uri": format!("data:image/png;base64,{}", base64(bytes))})),
                Storage::External => {
                    let name = format!("image{n}.png");
                    std::fs::write(dir.unwrap().join(&name), bytes).unwrap();
                    images.push(json!({"uri": name}));
                }
            }
        }
        if self.skinned {
            // inverse bind matrix accessor for the single joint
            let ibm = floats(&[1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.]);
            let v = view(&mut bin, &ibm, None);
            accessors.push(json!({"bufferView": v, "componentType": 5126, "count": 1, "type": "MAT4"}));
        }
        pad4(&mut bin, 0);

        let mut root = json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"name": "fixture", "primitives": primitives}],
            "accessors": accessors,
            "bufferViews": views,
        });
        if let Some(m) = self.node_matrix {
            root["nodes"][0]["matrix"] = json!(m);
        }
        if !self.materials.is_empty() {
            let mats: Vec<Value> = self
                .materials
                .iter()
                .map(|m| {
                    let mut pbr = json!({"baseColorFactor": m.base_color});
                    if let Some(t) = m.base_tex {
                        pbr["baseColorTexture"] = json!({"index": t});
                    }
                    if let Some(t) = m.mr_tex {
                        pbr["metallicRoughnessTexture"] = json!({"index": t});
                    }
                    let mut out = json!({"name": m.name, "pbrMetallicRoughness": pbr});
                    if let Some(t) = m.normal_tex {
                        out["normalTexture"] = json!({"index": t});
                    }
                    out
                })
                .collect();
            root["materials"] = json!(mats);
        }
        if !images.is_empty() {
            root["images"] = json!(images);
            root["textures"] = json!((0..images.len()).map(|i| json!({"source": i})).collect::<Vec<_>>());
        }
        if !self.extensions_required.is_empty() {
            root["extensionsRequired"] = json!(self.extensions_required);
            root["extensionsUsed"] = json!(self.extensions_required);
        }
        if self.skinned {
            let ibm = root["accessors"].as_array().unwrap().len() - 1;
            root["nodes"] = json!([{"mesh": 0, "skin": 0, "children": [1]}, {"name": "joint"}]);
            root["skins"] = json!([{"joints": [1], "inverseBindMatrices": ibm}]);
        }
        let buffer = match storage {
            Storage::Glb => json!({"byteLength": bin.len()}),
            Storage::DataUri => json!({"byteLength": bin.len(), "uri": format!("data:application/octet-stream;base64,{}", base64(&bin))}),
            Storage::External => {
                std::fs::write(dir.unwrap().join("scene.bin"), &bin).unwrap();
                json!({"byteLength": bin.len(), "uri": "scene.bin"})
            }
        };
        root["buffers"] = json!([buffer]);
        (root, bin)
    }

    pub fn glb(&self) -> Vec<u8> {
        let (root, bin) = self.build(Storage::Glb, None);
        let mut json = serde_json::to_vec(&root).unwrap();
        pad4(&mut json, b' ');
        let mut bin = bin;
        pad4(&mut bin, 0);
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    /// Write `<dir>/<stem>.glb`.
    pub fn write_glb(&self, dir: &Path, stem: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{stem}.glb"));
        std::fs::write(&path, self.glb()).unwrap();
        path
    }

    /// Write `<dir>/<stem>.gltf` (data URIs or external files).
    pub fn write_gltf(&self, dir: &Path, stem: &str, storage: Storage) -> PathBuf {
        assert!(storage != Storage::Glb);
        std::fs::create_dir_all(dir).unwrap();
        let (root, _) = self.build(storage, Some(dir));
        let path = dir.join(format!("{stem}.gltf"));
        std::fs::write(&path, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
        path
    }
}

// ------------------------------------------------------------------------------------ rigged (I5)

/// One mesh of a [`RigFixture`] (a single primitive; `joints`/`weights` make it skinned data).
#[derive(Clone, Default)]
pub struct RigMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub joints: Option<Vec<[u16; 4]>>,
    pub weights: Option<Vec<[f32; 4]>>,
    pub joints1: Option<Vec<[u16; 4]>>,
    pub weights1: Option<Vec<[f32; 4]>>,
    pub indices: Vec<u32>,
}

#[derive(Clone)]
pub struct RigChannel {
    pub node: usize,
    /// `translation`, `rotation`, `scale` or `weights`
    pub path: &'static str,
    /// `LINEAR`, `STEP` or `CUBICSPLINE`
    pub interpolation: &'static str,
    pub times: Vec<f32>,
    /// flat output values (3 or 4 per key; 3 per key times 3 for CUBICSPLINE)
    pub values: Vec<f32>,
}

#[derive(Clone)]
pub struct RigAnim {
    pub name: Option<String>,
    pub channels: Vec<RigChannel>,
}

/// A skinned glTF built from explicit nodes (JSON objects as in the glTF `nodes` array).
#[derive(Clone, Default)]
pub struct RigFixture {
    pub nodes: Vec<Value>,
    pub scene: Vec<usize>,
    pub meshes: Vec<RigMesh>,
    /// `joints` of the first skin; `ibms` empty = no `inverseBindMatrices`
    pub skin_joints: Vec<usize>,
    pub ibms: Vec<[f32; 16]>,
    /// extra skins (joints only), to test the "first skin only" rule
    pub extra_skin_joints: Vec<Vec<usize>>,
    pub anims: Vec<RigAnim>,
}

impl RigFixture {
    pub fn glb(&self) -> Vec<u8> {
        let mut bin: Vec<u8> = Vec::new();
        let mut views: Vec<Value> = Vec::new();
        let mut accessors: Vec<Value> = Vec::new();
        let mut add = |bin: &mut Vec<u8>, data: Vec<u8>, component: u32, count: usize, kind: &str, extra: Option<(Vec<f32>, Vec<f32>)>| -> usize {
            pad4(bin, 0);
            views.push(json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": data.len()}));
            bin.extend_from_slice(&data);
            let mut a = json!({"bufferView": views.len() - 1, "componentType": component, "count": count, "type": kind});
            if let Some((min, max)) = extra {
                a["min"] = json!(min);
                a["max"] = json!(max);
            }
            accessors.push(a);
            accessors.len() - 1
        };
        let mut meshes = Vec::new();
        for m in &self.meshes {
            let mut attributes = json!({});
            let (mut min, mut max) = (vec![f32::MAX; 3], vec![f32::MIN; 3]);
            for p in &m.positions {
                for k in 0..3 {
                    min[k] = min[k].min(p[k]);
                    max[k] = max[k].max(p[k]);
                }
            }
            let flat: Vec<f32> = m.positions.iter().flatten().copied().collect();
            attributes["POSITION"] = json!(add(&mut bin, floats(&flat), 5126, m.positions.len(), "VEC3", Some((min, max))));
            let flat: Vec<f32> = m.normals.iter().flatten().copied().collect();
            attributes["NORMAL"] = json!(add(&mut bin, floats(&flat), 5126, m.normals.len(), "VEC3", None));
            let shorts = |v: &[[u16; 4]]| -> Vec<u8> { v.iter().flatten().flat_map(|j| j.to_le_bytes()).collect() };
            let quads = |v: &[[f32; 4]]| -> Vec<f32> { v.iter().flatten().copied().collect() };
            if let Some(j) = &m.joints {
                attributes["JOINTS_0"] = json!(add(&mut bin, shorts(j), 5123, j.len(), "VEC4", None));
            }
            if let Some(w) = &m.weights {
                attributes["WEIGHTS_0"] = json!(add(&mut bin, floats(&quads(w)), 5126, w.len(), "VEC4", None));
            }
            if let Some(j) = &m.joints1 {
                attributes["JOINTS_1"] = json!(add(&mut bin, shorts(j), 5123, j.len(), "VEC4", None));
            }
            if let Some(w) = &m.weights1 {
                attributes["WEIGHTS_1"] = json!(add(&mut bin, floats(&quads(w)), 5126, w.len(), "VEC4", None));
            }
            let data: Vec<u8> = m.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
            let indices = add(&mut bin, data, 5125, m.indices.len(), "SCALAR", None);
            meshes.push(json!({"name": format!("mesh{}", meshes.len()), "primitives": [{"attributes": attributes, "indices": indices}]}));
        }
        let mut skins = Vec::new();
        if !self.skin_joints.is_empty() {
            let mut skin = json!({"joints": self.skin_joints});
            if !self.ibms.is_empty() {
                let flat: Vec<f32> = self.ibms.iter().flatten().copied().collect();
                skin["inverseBindMatrices"] = json!(add(&mut bin, floats(&flat), 5126, self.ibms.len(), "MAT4", None));
            }
            skins.push(skin);
        }
        for joints in &self.extra_skin_joints {
            skins.push(json!({"joints": joints}));
        }
        let mut animations = Vec::new();
        for anim in &self.anims {
            let mut samplers = Vec::new();
            let mut channels = Vec::new();
            for c in &anim.channels {
                let input = add(&mut bin, floats(&c.times), 5126, c.times.len(), "SCALAR", Some((vec![c.times[0]], vec![*c.times.last().unwrap()])));
                let (kind, width) = if c.path == "rotation" { ("VEC4", 4) } else { ("VEC3", 3) };
                let per_key = if c.interpolation == "CUBICSPLINE" { 3 } else { 1 };
                let output = add(&mut bin, floats(&c.values), 5126, c.values.len() / width, kind, None);
                assert_eq!(c.values.len(), c.times.len() * width * per_key, "fixture channel size");
                samplers.push(json!({"input": input, "output": output, "interpolation": c.interpolation}));
                channels.push(json!({"sampler": samplers.len() - 1, "target": {"node": c.node, "path": c.path}}));
            }
            let mut a = json!({"samplers": samplers, "channels": channels});
            if let Some(name) = &anim.name {
                a["name"] = json!(name);
            }
            animations.push(a);
        }
        pad4(&mut bin, 0);
        let mut root = json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": self.scene}],
            "nodes": self.nodes,
            "meshes": meshes,
            "accessors": accessors,
            "bufferViews": views,
            "buffers": [{"byteLength": bin.len()}],
        });
        if !skins.is_empty() {
            root["skins"] = json!(skins);
        }
        if !animations.is_empty() {
            root["animations"] = json!(animations);
        }
        let mut json = serde_json::to_vec(&root).unwrap();
        pad4(&mut json, b' ');
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&json);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
        out
    }

    /// Two bones: `root` at the origin and `tip` one metre up (rest, bind and node translation
    /// (0,1,0)); the tip bends 90 degrees about +Z over 1 s (clip `bend`, LINEAR, keys at 0 and 1).
    /// Six vertices: 0-2 on `root` (y = 0), 3-5 on `tip` (y = 1); vertex 2 is 50/50, vertex 5
    /// carries a 0.0004 `root` weight that must be pruned.
    pub fn tube() -> RigFixture {
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let mut ibm_tip = [0.0f32; 16];
        ibm_tip[0] = 1.0;
        ibm_tip[5] = 1.0;
        ibm_tip[10] = 1.0;
        ibm_tip[15] = 1.0;
        ibm_tip[13] = -1.0;
        let mut ibm_root = ibm_tip;
        ibm_root[13] = 0.0;
        RigFixture {
            nodes: vec![
                json!({"name": "Mesh", "mesh": 0, "skin": 0}),
                json!({"name": "Root", "children": [2]}),
                json!({"name": "Tip", "translation": [0.0, 1.0, 0.0]}),
            ],
            scene: vec![0, 1],
            meshes: vec![RigMesh {
                positions: vec![[0.0, 0.0, 0.0], [0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 1.0, 0.0], [0.5, 1.0, 0.0], [0.0, 1.5, 0.25]],
                normals: vec![[0.0, 0.0, 1.0]; 6],
                joints: Some(vec![[0, 0, 0, 0], [0, 0, 0, 0], [0, 1, 0, 0], [1, 0, 0, 0], [1, 0, 0, 0], [1, 0, 0, 0]]),
                weights: Some(vec![
                    [1.0, 0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    [0.5, 0.5, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    [1.0, 0.0, 0.0, 0.0],
                    [0.9996, 0.0004, 0.0, 0.0],
                ]),
                indices: vec![0, 1, 2, 3, 4, 5],
                ..RigMesh::default()
            }],
            skin_joints: vec![1, 2],
            ibms: vec![ibm_root, ibm_tip],
            anims: vec![RigAnim {
                name: Some("Bend".into()),
                channels: vec![RigChannel {
                    node: 2,
                    path: "rotation",
                    interpolation: "LINEAR",
                    times: vec![0.0, 1.0],
                    values: vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, h, h],
                }],
            }],
            ..RigFixture::default()
        }
    }
}
