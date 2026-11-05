//! Starter `.vmdl` templates for `model_create`.
//!
//! [`template`] builds a fresh model from a small parameter set — enough for the editor to open it
//! and the resource compiler to try to compile it. Two kinds:
//!
//! * `static_prop` — `{meshFile, physicsFile?, physicsKind?, material?, materialRemaps?, importScale?,
//!   lodGroup?}` (`physicsKind` `"hull"` (default, `PhysicsHullFile`) or `"mesh"` (concave,
//!   `PhysicsMeshFile`); `materialRemaps` `[{from, to}]`, see [`material_remap`]; `importScale`
//!   default `1.0`)
//! * `skeletal` — `static_prop` plus `{animFiles?: [..], nmSkeleton?, animGraph2?,
//!   animGraph2Identifiers?, keepBones?, renameBones?, attachments?, hitboxSets?}` (animation/
//!   hitbox/bone-markup/attachment lists; `nmSkeleton` / `animGraph2` are resource paths emitted as
//!   `NmSkeletonList > NmSkeletonReference{filename}` / `AnimGraph2List > DefaultAnimGraph2{filename}`
//!   between the hitbox and attachment lists, the layout of the I0 spike's `spike_agent.vmdl`;
//!   `animGraph2Identifiers` repeats `animGraph2` as `AnimGraph2 {name: <identifier>, filename}`
//!   (the player pawn asks for `worldmodel`: without it the pawn stays in the bind pose);
//!   `renameBones` see [`rename_bones`]; `keepBones` (bone names) become `BoneMarkup {target_bone, do_not_discard: true}` children of
//!   the `BoneMarkupList`, without which the compiler drops every unweighted bone that no
//!   attachment or hitbox names; `attachments` / `hitboxSets`: see [`attachments`] and
//!   [`hitbox_sets`])
//!
//! `meshFile` is required for both and is the FBX/DMX the mesh is imported from
//! (`models/mcp_probe/body.fbx`); the render-mesh *name* is that file's stem, the contract the
//! import uses.
//!
//! # Class and key provenance `[I]`
//!
//! The class names and the keys beside them are **inferred** for this pass: they come from
//! ValveResourceFormat's `ModelExtract*` (MIT), which decompiles a compiled model back into the
//! same ModelDoc node tree, plus the shipped `glass_probe.vmdl` where noted. `model_create`
//! reports `classesUnverified` until the `model_node_types` snapshot confirms the classes, and the
//! only end-to-end check of the keys is that the editor opens the file and the compiler accepts it.
//!
//! | class | keys written | source |
//! |---|---|---|
//! | `RootNode` | `children` | sample `[V]` |
//! | `RenderMeshList` / `PhysicsShapeList` / `MaterialGroupList` / `LODGroupList` / `AnimationList` / `BoneMarkupList` / `HitboxSetList` / `AttachmentList` | `children` | VRF `[I]` |
//! | `RenderMeshFile` | `name`, `filename`, `import_scale` | sample `[V]` |
//! | `PhysicsHullFile` / `PhysicsMeshFile` | `name`, `filename` | VRF `ModelExtract.Physics` `[I]` |
//! | `DefaultMaterialGroup` | `name`, `remaps[{from, to}]`, `use_global_default`, `global_default_material` | VRF `ModelExtract.MeshLists` / `ModelExtract.Physics` `[I]` (remap key spelling: [`material_remap`]) |
//! | `LODGroup` | `switch_threshold`, `mesh_references[{mesh_name}]` | VRF `ModelExtract.MeshLists` `[I]` |
//! | `AnimFile` | `name`, `source_filename` | VRF `ModelExtract.Sequences` `[I]` |
//! | `BoneMarkupList` | `bone_cull_type` | VRF `ModelExtract.ValveModel` `[I]` |
//! | `HitboxSet` | `name`, `children` | VRF `ModelExtract.Hitboxes` `[I]` |
//! | `HitboxCapsule` / `HitboxSphere` / `Hitbox` | `name`, `parent_bone`, `surface_property`, `translation_only`, `group_id` + `radius`/`point0`/`point1`, `center`/`radius`, `hitbox_mins`/`hitbox_maxs` | VRF `ModelExtract.Hitboxes` `[I]` |
//! | `Attachment` / `AttachmentInfluence` | `name`, `ignore_rotation`, `parent_bone`, `relative_origin`, `relative_angles`, `weight` | VRF `ModelExtract.MeshLists` `[I]` |
//! | `NmSkeletonReference` | `filename` | I0 spike `[V]`; `do_not_discard_skeleton_bones = true` fails to compile (the compiler looks for the vnmskel's source) `[V]` |
//! | `RenameBone` (under `RenderMeshFile`) | `original_bone_name`, `new_bone_name` | strings in `resourcecompiler.dll`; compiled + read back `[V]` |
//! | `AnimGraph2` | `name` (identifier), `filename` | VRF `ModelExtract.GameData`; compiled `m_animGraph2Refs` read back `[V]` |
//! | `BoneMarkup` | `target_bone`, `do_not_discard` | strings in `resourcecompiler.dll`; compiled + read back (the bones stay) `[V]` |
//!
//! The root node of a Valve-authored file also carries `model_archetype`,
//! `primary_associated_entity`, `anim_graph_name` and `document_sub_type` (sample `[V]`); a fresh
//! template leaves them out so the editor fills in its own defaults on save.

use crate::vmdl::{Issue, Model, VmdlError};
use cs2_kv3::{Document, Header, Number, Object, Value};
use serde_json::{Map, Value as Json};
use std::path::Path;

/// The header of a new model: `modeldoc36`, the shipped sample's version GUIDs (a header line is
/// text, so the GUID is copyable).
const TEMPLATE_HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:modeldoc36:version{972dada4-b828-45a4-bb93-7795cf0585da} -->";

/// Every `_class` [`template`] can emit (a subset of the node-type registry). A test checks it
/// against `research/modeldoc-nodetypes.json` when that snapshot exists.
pub const TEMPLATE_CLASSES: &[&str] = &[
    "RootNode",
    "RenderMeshList",
    "RenderMeshFile",
    "PhysicsShapeList",
    "PhysicsHullFile",
    "PhysicsMeshFile",
    "MaterialGroupList",
    "DefaultMaterialGroup",
    "LODGroupList",
    "LODGroup",
    "AnimationList",
    "AnimFile",
    "BoneMarkupList",
    "BoneMarkup",
    "RenameBone",
    "AnimGraph2",
    "HitboxSetList",
    "HitboxSet",
    "AttachmentList",
    "Attachment",
    "AttachmentInfluence",
    "HitboxCapsule",
    "HitboxSphere",
    "Hitbox",
    "NmSkeletonList",
    "NmSkeletonReference",
    "AnimGraph2List",
    "DefaultAnimGraph2",
];

/// Build a starter model. `kind` is `"static_prop"` or `"skeletal"`; `params` is the tool's
/// `params` object (see the module docs). The result is structurally valid by construction —
/// [`Model::validate`] runs on it before it is returned.
pub fn template(kind: &str, params: &Json) -> Result<Model, VmdlError> {
    let children = match kind {
        "static_prop" => template_children(params, false)?,
        "skeletal" => template_children(params, true)?,
        other => {
            return Err(template_issue(format!(
                "unknown template {other:?}: expected \"static_prop\" or \"skeletal\""
            )))
        }
    };
    let header = Header::parse(TEMPLATE_HEADER).map_err(VmdlError::Parse)?;
    let mut root = Object::new();
    root.insert("_class".to_string(), Value::String("RootNode".to_string()));
    root.insert("children".to_string(), Value::Array(children));
    let mut doc = Document::new(header);
    doc.root.insert("rootNode".to_string(), Value::Object(root));
    let model = Model { doc };
    let issues = model.validate(None);
    if issues.is_empty() {
        Ok(model)
    } else {
        Err(VmdlError::Invalid(issues))
    }
}

fn template_children(params: &Json, skeletal: bool) -> Result<Vec<Value>, VmdlError> {
    let map = params
        .as_object()
        .ok_or_else(|| template_issue("params must be an object"))?;
    let mesh_file = normalize_path(&required_string(map, "meshFile")?);
    let mesh_name = stem_of(&mesh_file)?;
    let mut children = Vec::new();

    let mut render_mesh = Object::new();
    render_mesh.insert("_class".to_string(), Value::String("RenderMeshFile".to_string()));
    render_mesh.insert("name".to_string(), Value::String(mesh_name.clone()));
    render_mesh.insert("filename".to_string(), Value::String(mesh_file));
    let import_scale = match map.get("importScale") {
        None | Some(Json::Null) => Number::from_f64(1.0),
        Some(value) => number_from(value, "params.importScale")?,
    };
    render_mesh.insert("import_scale".to_string(), Value::Number(import_scale));
    let renames = rename_bones(map)?;
    if !renames.is_empty() {
        if !skeletal {
            return Err(template_issue("params.renameBones needs the \"skeletal\" template"));
        }
        render_mesh.insert("children".to_string(), Value::Array(renames));
    }
    children.push(list_node("RenderMeshList", vec![Value::Object(render_mesh)]));

    if let Some(physics) = optional_string(map, "physicsFile")? {
        let physics = normalize_path(&physics);
        let class = match optional_string(map, "physicsKind")?.as_deref() {
            None | Some("hull") => "PhysicsHullFile",
            Some("mesh") => "PhysicsMeshFile",
            Some(other) => {
                return Err(template_issue(format!(
                    "params.physicsKind must be \"hull\" or \"mesh\", got {other:?}"
                )))
            }
        };
        let mut shape = Object::new();
        shape.insert("_class".to_string(), Value::String(class.to_string()));
        shape.insert("name".to_string(), Value::String(stem_of(&physics)?));
        shape.insert("filename".to_string(), Value::String(physics));
        children.push(list_node("PhysicsShapeList", vec![Value::Object(shape)]));
    }

    let mut material_group = Object::new();
    material_group.insert(
        "_class".to_string(),
        Value::String("DefaultMaterialGroup".to_string()),
    );
    material_group.insert("name".to_string(), Value::String("default".to_string()));
    material_group.insert("remaps".to_string(), Value::Array(remaps(map)?));
    if let Some(material) = optional_string(map, "material")? {
        material_group.insert("use_global_default".to_string(), Value::Bool(true));
        material_group.insert(
            "global_default_material".to_string(),
            Value::String(normalize_path(&material)),
        );
    }
    children.push(list_node(
        "MaterialGroupList",
        vec![Value::Object(material_group)],
    ));

    if let Some(lod_group) = lod_group(map, &mesh_name)? {
        children.push(list_node("LODGroupList", vec![lod_group]));
    }

    if !skeletal {
        for key in ["attachments", "hitboxSets", "keepBones", "nmSkeleton", "animGraph2", "animGraph2Identifiers", "animFiles"] {
            if map.get(key).is_some_and(|v| !v.is_null()) {
                return Err(template_issue(format!("params.{key} needs the \"skeletal\" template")));
            }
        }
    }
    if skeletal {
        let anim_files = string_list(map, "animFiles")?;
        let mut animations = Vec::with_capacity(anim_files.len());
        for file in anim_files {
            let file = normalize_path(&file);
            let mut node = Object::new();
            node.insert("_class".to_string(), Value::String("AnimFile".to_string()));
            node.insert("name".to_string(), Value::String(stem_of(&file)?));
            node.insert("source_filename".to_string(), Value::String(file));
            animations.push(Value::Object(node));
        }
        children.push(list_node("AnimationList", animations));

        let mut bone_markup = Object::new();
        bone_markup.insert(
            "_class".to_string(),
            Value::String("BoneMarkupList".to_string()),
        );
        let keep: Vec<Value> = string_list(map, "keepBones")?
            .into_iter()
            .map(|bone| {
                let mut node = Object::new();
                node.insert("_class".to_string(), Value::String("BoneMarkup".to_string()));
                node.insert("target_bone".to_string(), Value::String(bone));
                node.insert("do_not_discard".to_string(), Value::Bool(true));
                Value::Object(node)
            })
            .collect();
        bone_markup.insert("children".to_string(), Value::Array(keep));
        bone_markup.insert(
            "bone_cull_type".to_string(),
            Value::String("None".to_string()),
        );
        children.push(Value::Object(bone_markup));

        children.push(list_node("HitboxSetList", hitbox_sets(map)?));
        if !string_list(map, "animGraph2Identifiers")?.is_empty() && optional_string(map, "animGraph2")?.is_none() {
            return Err(template_issue("params.animGraph2Identifiers needs params.animGraph2"));
        }
        for (param, list, class) in [
            ("nmSkeleton", "NmSkeletonList", "NmSkeletonReference"),
            ("animGraph2", "AnimGraph2List", "DefaultAnimGraph2"),
        ] {
            if let Some(file) = optional_string(map, param)? {
                let file = normalize_path(&file);
                let mut node = Object::new();
                node.insert("_class".to_string(), Value::String(class.to_string()));
                node.insert("filename".to_string(), Value::String(file.clone()));
                let mut nodes = vec![Value::Object(node)];
                if class == "DefaultAnimGraph2" {
                    // the same graph again under a semantic identifier (`AnimGraph2 {name, filename}`)
                    for identifier in string_list(map, "animGraph2Identifiers")? {
                        let mut named = Object::new();
                        named.insert("_class".to_string(), Value::String("AnimGraph2".to_string()));
                        named.insert("name".to_string(), Value::String(identifier));
                        named.insert("filename".to_string(), Value::String(file.clone()));
                        nodes.push(Value::Object(named));
                    }
                }
                children.push(list_node(list, nodes));
            }
        }
        children.push(list_node("AttachmentList", attachments(map)?));
    }

    Ok(children)
}

/// `params.renameBones`: `[{from, to}]` -> `RenameBone {original_bone_name, new_bone_name}` modifiers
/// under the `RenderMeshFile` (they act on that file's data; strings of `resourcecompiler.dll`,
/// compiled and read back `[V]`). The SMD importer lowercases bone names; this restores them.
fn rename_bones(map: &Map<String, Json>) -> Result<Vec<Value>, VmdlError> {
    array_param(map, "renameBones")?
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Json::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| template_issue(format!("params.renameBones[{i}] needs a non-empty string {key:?}")))
            };
            let (from, to) = (field("from")?, field("to")?);
            if from == to {
                return Err(template_issue(format!("params.renameBones[{i}]: from and to are equal ({from:?})")));
            }
            let mut node = Object::new();
            node.insert("_class".to_string(), Value::String("RenameBone".to_string()));
            node.insert("original_bone_name".to_string(), Value::String(from));
            node.insert("new_bone_name".to_string(), Value::String(to));
            Ok(Value::Object(node))
        })
        .collect()
}

/// The keys an `Attachment` / `AttachmentInfluence` entry of `params.attachments` may carry.
const ATTACHMENT_KEYS: &[&str] = &["name", "parent_bone", "relative_origin", "relative_angles", "weight", "ignore_rotation", "influences"];
const INFLUENCE_KEYS: &[&str] = &["parent_bone", "relative_origin", "relative_angles", "weight"];
/// `shape` -> class and the keys beside the common ones (`name`, `parent_bone`,
/// `surface_property`, `translation_only`, `group_id`).
const HITBOX_SHAPES: &[(&str, &str, &[&str])] = &[
    ("capsule", "HitboxCapsule", &["radius", "point0", "point1"]),
    ("sphere", "HitboxSphere", &["center", "radius"]),
    ("box", "Hitbox", &["hitbox_mins", "hitbox_maxs"]),
];
const HITBOX_COMMON: &[&str] = &["name", "parent_bone", "surface_property", "translation_only", "group_id"];

/// One node of `class` from a params object holding only `allowed` keys (values converted
/// verbatim; `skip` keys are handled by the caller).
fn node_from(class: &str, item: &Json, allowed: &[&str], skip: &[&str], what: &str) -> Result<Object, VmdlError> {
    let fields = item.as_object().ok_or_else(|| template_issue(format!("{what} must be an object, got {item}")))?;
    let mut node = Object::new();
    node.insert("_class".to_string(), Value::String(class.to_string()));
    for (key, value) in fields {
        if !allowed.contains(&key.as_str()) {
            return Err(template_issue(format!("{what}: unknown key {key:?} (allowed {allowed:?})")));
        }
        if skip.contains(&key.as_str()) {
            continue;
        }
        let value = cs2_kv3::from_json(value).map_err(|e| template_issue(format!("{what}.{key}: {e}")))?;
        node.insert(key.clone(), value);
    }
    Ok(node)
}

fn require(node: &Object, keys: &[&str], what: &str) -> Result<(), VmdlError> {
    match keys.iter().find(|k| node.get(k).is_none()) {
        Some(missing) => Err(template_issue(format!("{what} needs {missing:?}"))),
        None => Ok(()),
    }
}

fn array_param<'a>(map: &'a Map<String, Json>, key: &str) -> Result<&'a [Json], VmdlError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(&[]),
        Some(Json::Array(items)) => Ok(items),
        Some(other) => Err(template_issue(format!("params.{key} must be an array, got {other}"))),
    }
}

/// `params.attachments`: `[{name, parent_bone, relative_origin, relative_angles, weight?,
/// ignore_rotation?, influences?: [{parent_bone, relative_origin, relative_angles, weight}]}]` ->
/// `Attachment` nodes (`influences` become `AttachmentInfluence` children). `[I]` keys from
/// ValveResourceFormat `ModelExtract.MeshLists`; an empty `parent_bone` is the model origin.
fn attachments(map: &Map<String, Json>) -> Result<Vec<Value>, VmdlError> {
    array_param(map, "attachments")?
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let what = format!("params.attachments[{i}]");
            let mut node = node_from("Attachment", item, ATTACHMENT_KEYS, &["influences"], &what)?;
            if !matches!(node.get("name"), Some(Value::String(n)) if !n.is_empty()) {
                return Err(template_issue(format!("{what} needs a non-empty string name")));
            }
            require(&node, &["parent_bone", "relative_origin", "relative_angles"], &what)?;
            if let Some(influences) = item.get("influences") {
                let list = influences.as_array().ok_or_else(|| template_issue(format!("{what}.influences must be an array")))?;
                let children = list
                    .iter()
                    .enumerate()
                    .map(|(k, inf)| {
                        let what = format!("{what}.influences[{k}]");
                        let node = node_from("AttachmentInfluence", inf, INFLUENCE_KEYS, &[], &what)?;
                        require(&node, &["parent_bone", "relative_origin", "relative_angles"], &what)?;
                        Ok::<_, VmdlError>(Value::Object(node))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                node.insert("children".to_string(), Value::Array(children));
            }
            Ok(Value::Object(node))
        })
        .collect()
}

/// `params.hitboxSets`: `[{name, hitboxes: [{shape: capsule|sphere|box, name, parent_bone,
/// surface_property, translation_only, group_id, <shape keys>}]}]` -> `HitboxSet` nodes. Absent:
/// one empty set `default` (the compiler accepts it). `[I]` keys from ValveResourceFormat
/// `ModelExtract.Hitboxes`.
fn hitbox_sets(map: &Map<String, Json>) -> Result<Vec<Value>, VmdlError> {
    let sets = array_param(map, "hitboxSets")?;
    let set_node = |name: String, children: Vec<Value>| {
        let mut node = Object::new();
        node.insert("_class".to_string(), Value::String("HitboxSet".to_string()));
        node.insert("name".to_string(), Value::String(name));
        node.insert("children".to_string(), Value::Array(children));
        Value::Object(node)
    };
    if sets.is_empty() {
        return Ok(vec![set_node("default".to_string(), Vec::new())]);
    }
    sets.iter()
        .enumerate()
        .map(|(i, set)| {
            let what = format!("params.hitboxSets[{i}]");
            let name = set
                .get("name")
                .and_then(Json::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| template_issue(format!("{what} needs a non-empty string name")))?;
            let hitboxes = set.get("hitboxes").and_then(Json::as_array).ok_or_else(|| template_issue(format!("{what}.hitboxes must be an array")))?;
            let children = hitboxes
                .iter()
                .enumerate()
                .map(|(k, h)| {
                    let what = format!("{what}.hitboxes[{k}]");
                    let shape = h.get("shape").and_then(Json::as_str).unwrap_or("");
                    let (_, class, keys) = HITBOX_SHAPES
                        .iter()
                        .find(|(s, _, _)| *s == shape)
                        .ok_or_else(|| template_issue(format!("{what}.shape must be capsule, sphere or box, got {shape:?}")))?;
                    let allowed: Vec<&str> = HITBOX_COMMON.iter().chain(keys.iter()).copied().chain(["shape"]).collect();
                    let node = node_from(class, h, &allowed, &["shape"], &what)?;
                    let needed: Vec<&str> = ["name", "parent_bone"].into_iter().chain(keys.iter().copied()).collect();
                    require(&node, &needed, &what)?;
                    Ok::<_, VmdlError>(Value::Object(node))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(set_node(name.to_string(), children))
        })
        .collect()
}

/// One `DefaultMaterialGroup.remaps` entry: the material name the mesh file uses -> the `.vmat` it
/// should render with. `[I]` The key spelling (`from`/`to`) and the `<name>.vmat` form of `from`
/// come from VRF `ModelExtract.MeshLists`; this is the one place to fix them if the live compile
/// disagrees.
pub fn material_remap(mesh_material: &str, vmat: &str) -> Json {
    serde_json::json!({"from": format!("{mesh_material}.vmat"), "to": vmat})
}

/// `params.materialRemaps`: `[{from, to}]` -> remap objects.
fn remaps(map: &Map<String, Json>) -> Result<Vec<Value>, VmdlError> {
    let items = match map.get("materialRemaps") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(other) => {
            return Err(template_issue(format!(
                "params.materialRemaps must be an array, got {other}"
            )))
        }
    };
    items
        .iter()
        .map(|item| {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Json::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| Value::String(normalize_path(s)))
                    .ok_or_else(|| {
                        template_issue(format!("params.materialRemaps entries need a string {key:?}, got {item}"))
                    })
            };
            let mut remap = Object::new();
            remap.insert("from".to_string(), field("from")?);
            remap.insert("to".to_string(), field("to")?);
            Ok(Value::Object(remap))
        })
        .collect()
}

/// `{_class, children}` — the shape of every ModelDoc list node.
fn list_node(class: &str, children: Vec<Value>) -> Value {
    let mut node = Object::new();
    node.insert("_class".to_string(), Value::String(class.to_string()));
    node.insert("children".to_string(), Value::Array(children));
    Value::Object(node)
}

/// `params.lodGroup`: absent/`false` emits no `LODGroupList`; `true` emits one LOD group with the
/// template's mesh at threshold `0.0`; an object takes `switch_threshold` (number) and `meshes`
/// (list of render-mesh names, defaulting to the template's mesh).
fn lod_group(map: &Map<String, Json>, mesh_name: &str) -> Result<Option<Value>, VmdlError> {
    let Some(param) = map.get("lodGroup") else {
        return Ok(None);
    };
    let (threshold, meshes) = match param {
        Json::Null | Json::Bool(false) => return Ok(None),
        Json::Bool(true) => (Number::from_f64(0.0), vec![mesh_name.to_string()]),
        Json::Object(options) => {
            let threshold = match options.get("switch_threshold") {
                Some(value) => number_from(value, "params.lodGroup.switch_threshold")?,
                None => Number::from_f64(0.0),
            };
            let mut meshes = string_list(options, "meshes")?;
            if meshes.is_empty() {
                meshes.push(mesh_name.to_string());
            }
            (threshold, meshes)
        }
        other => {
            return Err(template_issue(format!(
                "params.lodGroup must be true, false or an object, got {other}"
            )))
        }
    };
    let references = meshes
        .iter()
        .map(|name| {
            let mut reference = Object::new();
            reference.insert("mesh_name".to_string(), Value::String(name.clone()));
            Value::Object(reference)
        })
        .collect();
    let mut node = Object::new();
    node.insert("_class".to_string(), Value::String("LODGroup".to_string()));
    node.insert("switch_threshold".to_string(), Value::Number(threshold));
    node.insert("mesh_references".to_string(), Value::Array(references));
    Ok(Some(Value::Object(node)))
}

fn optional_string(map: &Map<String, Json>, key: &str) -> Result<Option<String>, VmdlError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(other) => Err(template_issue(format!(
            "params.{key} must be a non-empty string, got {other}"
        ))),
    }
}

fn required_string(map: &Map<String, Json>, key: &str) -> Result<String, VmdlError> {
    optional_string(map, key)?.ok_or_else(|| template_issue(format!("params.{key} is required")))
}

fn string_list(map: &Map<String, Json>, key: &str) -> Result<Vec<String>, VmdlError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| match item {
                Json::String(text) if !text.trim().is_empty() => Ok(text.clone()),
                other => Err(template_issue(format!(
                    "params.{key} entries must be non-empty strings, got {other}"
                ))),
            })
            .collect(),
        Some(other) => Err(template_issue(format!(
            "params.{key} must be an array, got {other}"
        ))),
    }
}

fn number_from(value: &Json, what: &str) -> Result<Number, VmdlError> {
    match cs2_kv3::from_json(value) {
        Ok(Value::Number(number)) => Ok(number),
        _ => Err(template_issue(format!("{what} must be a number, got {value}"))),
    }
}

/// `models/x/body.fbx` -> `body` (the render-mesh name the importer uses).
fn stem_of(file: &str) -> Result<String, VmdlError> {
    Path::new(file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
        .ok_or_else(|| template_issue(format!("{file:?} has no file name to name the node after")))
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn template_issue(msg: impl Into<String>) -> VmdlError {
    VmdlError::Invalid(vec![Issue {
        path: "/".to_string(),
        msg: msg.into(),
    }])
}
