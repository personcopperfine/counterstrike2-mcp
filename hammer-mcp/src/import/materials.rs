//! glTF PBR -> Source 2 `.vmat` (`csgo_complex.vfx`).
//!
//! A `.vmat` is KeyValues1 text: one `Layer0` block. Written with `cs2-kv1`.
//!
//! # Key provenance `[I]` (inferred; the live test confirms them)
//!
//! | key | value | source |
//! |---|---|---|
//! | `shader` | `"csgo_complex.vfx"` | the shader the CS2 props use |
//! | `TextureColor` | base-colour texture path | `[I]` csgo_complex parameter |
//! | `TextureNormal` | normal map path | `[I]` |
//! | `TextureRoughness` | roughness texture path | `[I]` |
//! | `TextureMetalness` | metalness texture path | `[I]` |
//! | `g_vColorTint` | `"[r g b a]"` base-colour factor | `[I]` |
//!
//! glTF packs roughness in G and metalness in B of one texture; splitting channels needs pixel
//! decoding, so that texture is written as it is and named for both keys ([`PACKED_WARNING`]).
//! The glTF normal map is OpenGL style (+Y up); the channel convention is not converted.
use super::gltf_read::{Material, Scene, Texture};
use cs2_kv1::{KeyValue, Object, Value};

pub const PACKED_WARNING: &str =
    "metallicRoughness packed texture used as both roughness and metalness source; channels not split";
pub const NORMAL_WARNING: &str = "normal map used as is (glTF OpenGL green channel; not converted)";

#[derive(Debug, Clone, PartialEq)]
pub struct VmatFile {
    /// The SMD material name this material answers to.
    pub smd_name: String,
    /// `materials/models/<model>/<name>.vmat` (addon-relative).
    pub vmat: String,
    pub text: String,
    /// Texture files to write: (addon-relative path, bytes).
    pub textures: Vec<(String, Vec<u8>)>,
    pub dropped: Vec<String>,
}

/// Unique, file-name-safe SMD material names for `scene.materials` (same order).
pub fn smd_names(scene: &Scene) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for material in &scene.materials {
        let base = sanitize(&material.name);
        let mut name = base.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        names.push(name);
    }
    names
}

/// Lowercase `[a-z0-9_]`; never empty.
pub fn sanitize(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "material".to_string() } else { s }
}

pub fn build(scene: &Scene, names: &[String], model: &str) -> Vec<VmatFile> {
    scene
        .materials
        .iter()
        .zip(names)
        .map(|(material, name)| build_one(material, name, model))
        .collect()
}

fn build_one(material: &Material, name: &str, model: &str) -> VmatFile {
    let dir = format!("materials/models/{model}");
    let mut textures: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |tex: &Texture, suffix: &str| -> String {
        let path = format!("{dir}/{name}_{suffix}.{}", tex.ext);
        textures.push((path.clone(), tex.bytes.clone()));
        path
    };
    let mut layer = Object::new();
    layer.push("shader", Value::String("csgo_complex.vfx".to_string()));
    let [r, g, b, _] = material.base_color;
    layer.push("g_vColorTint", Value::String(format!("[{r:.6} {g:.6} {b:.6} 0.000000]")));
    if let Some(tex) = &material.base_color_tex {
        layer.push("TextureColor", Value::String(add(tex, "color")));
    }
    if let Some(tex) = &material.normal_tex {
        layer.push("TextureNormal", Value::String(add(tex, "normal")));
    }
    let mut dropped = material.dropped.clone();
    if let Some(tex) = &material.metal_rough_tex {
        let path = add(tex, "metalrough");
        layer.push("TextureRoughness", Value::String(path.clone()));
        layer.push("TextureMetalness", Value::String(path));
        dropped.push(PACKED_WARNING.to_string());
    } else if material.metallic != 1.0 || material.roughness != 1.0 {
        // glTF defaults are 1/1; factors without a texture have no key we can name safely
        dropped.push("metallic/roughness factors dropped (no texture to carry them)".to_string());
    }
    let doc = KeyValue { key: "Layer0".to_string(), value: Value::Object(layer), condition: None };
    let text = format!("// THIS FILE IS AUTO-GENERATED (model_import)\n\n{}", cs2_kv1::text::write(&doc));
    VmatFile {
        smd_name: name.to_string(),
        vmat: format!("{dir}/{name}.vmat"),
        text,
        textures,
        dropped,
    }
}

/// Pixel size of a PNG or JPEG (header only), `None` for anything else or a truncated header.
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // IHDR: width, height as big-endian u32 right after the 8+8 byte signature/chunk header
        let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let seg = u16::from_be_bytes([bytes[i + 2], *bytes.get(i + 3)?]) as usize;
            // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC)
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                let h = u16::from_be_bytes(bytes.get(i + 5..i + 7)?.try_into().ok()?);
                let w = u16::from_be_bytes(bytes.get(i + 7..i + 9)?.try_into().ok()?);
                return (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)));
            }
            i += 2 + seg;
        }
    }
    None
}

/// `materials:"existing"`: the target `.vmat` for each of `names` from `map` (`source name ->
/// resolved target`). A name not in the map takes `fallback` and a warning; a map key that
/// matches no name is warned about. Keys match after stripping `.vmat` and, failing that,
/// after [`sanitize`].
pub fn match_existing(
    map: &[(String, String)],
    names: &[String],
    fallback: &str,
    warnings: &mut Vec<String>,
) -> Vec<(String, String)> {
    let strip = |k: &str| k.trim().strip_suffix(".vmat").unwrap_or(k.trim()).to_string();
    let mut used = vec![false; map.len()];
    let mut out = Vec::new();
    for name in names {
        let hit = map
            .iter()
            .position(|(k, _)| strip(k) == *name)
            .or_else(|| map.iter().position(|(k, _)| sanitize(&strip(k)) == *name));
        let target = match hit {
            Some(i) => {
                used[i] = true;
                map[i].1.clone()
            }
            None => {
                warnings.push(format!("material {name} is not in materialMap: it uses the fallback {fallback}"));
                fallback.to_string()
            }
        };
        out.push((name.clone(), target));
    }
    for ((key, _), _) in map.iter().zip(&used).filter(|(_, u)| !**u) {
        warnings.push(format!(
            "materialMap key {key:?} matches no material of the file (its materials: {})",
            names.join(", ")
        ));
    }
    out
}
