//! `get_model_info` (WP-R1: target/WP.md): the compiled `*.vmdl_c` facts a map author needs to
//! place a prop, as a read-only tool over [`hammer_mcp::resource::model_info_from_bytes`] and the
//! model catalog (`model_catalog.rs`, WP-V's `lookup_model`/`read_compiled_model`).
//!
//! The result is one model's [`ModelInfo`] jsonified: render/physics bounds (the boxes the
//! corpus verified usable — `m_modelInfo`'s own hull/view are all-zero in every CS2 prop,
//! `resource/model.rs` §verified), skins, bodygroups, sequences, attachments, and the
//! `placement` block `create_props` (`props_live.rs`) computes `snapToFloor` from:
//! `floorOffset` lifts the origin so the model rests on a floor at the origin's z
//! (`-render.min.z`), `footprint` is the render box's x/y size.
//!
//! Errors name the model, never empty facts: an unknown path is `ModelNotFound` with up to
//! [`SUGGESTIONS`] near-miss suggestions from the catalog (same directory or a shared name
//! substring, [`catalog_for`]'s map is the source), a cataloged-but-uncompiled addon model is
//! `ModelNotCompiled` with a `model_compile` hint, a cataloged-but-unreadable one `ModelRead`
//! (the source moved mid-call), and a decode failure `ModelRead` too — that last one is what a
//! truncated VPK entry or a recompile in flight looks like. The shared helpers (`resolve`,
//! `compiled_model_info`, the error constructors) serve `create_props` and the `model` check of
//! `set_entity_keyvalues` on the same terms.
use crate::resource::{model::ModelInfo, model_info_from_bytes};
use crate::state::HammerState;
use crate::tools::model_catalog::{catalog_for, lookup_model, read_compiled_model};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};

/// The plan reference (`model_catalog.rs`'s comment style: which document owns the behaviour).
const PLAN: &str = "target/WP.md (get_model_info)";
/// Default and maximum `limit` per included list (`sequences`/`attachments`/`bodygroups`).
const LIST_LIMIT_DEFAULT: u64 = 200;
const LIST_LIMIT_MAX: u64 = 2000;
/// How many near-miss suggestions `ModelNotFound` carries (`create_props` shows the same list).
const SUGGESTIONS: usize = 5;

// -------------------------------------------------------------------------------------------
// The shared model-resolution API (this tool + `create_props` + `set_entity_keyvalues`)
// -------------------------------------------------------------------------------------------

/// The catalog's data for one requested model (canonical path in the `.vmdl` spelling).
pub(crate) struct Resolved {
    pub canonical: String,
    pub sources: Vec<&'static str>,
    pub compiled: bool,
}

/// `model` (a required string, any spelling `normalize_model_path` accepts) → the catalog
/// entry, with the addon-only rescan of `lookup_model` in the path. A still-unknown path is
/// `ModelNotFound` with suggestions (from the cached catalog — the lookup just refreshed it).
pub(crate) async fn resolve(state: &HammerState, model: &str) -> Result<Resolved, ToolError> {
    let entry = match lookup_model(state, model).await? {
        Some(entry) => entry,
        None => {
            let canonical = crate::tools::model_catalog::normalize_model_path(model)
                .unwrap_or_else(|_| model.trim().to_lowercase());
            let catalog = catalog_for(state).await?;
            return Err(model_not_found(
                &canonical,
                catalog.suggestions(model, SUGGESTIONS),
            ));
        }
    };
    Ok(Resolved {
        canonical: entry.path.clone(),
        sources: entry.sources.iter().map(|s| s.label()).collect(),
        compiled: entry.compiled,
    })
}

/// `ModelNotFound` naming the canonical path, with the catalog's near misses in `data`.
pub(crate) fn model_not_found(canonical: &str, suggestions: Vec<String>) -> ToolError {
    let with_list = if suggestions.is_empty() {
        String::new()
    } else {
        format!("; closest catalog entries: {}", suggestions.join(", "))
    };
    ToolError::new(
        "ModelNotFound",
        "ModelNotFound",
        format!(
            "no model named {canonical} in the model catalog{with_list} (list_models shows the catalog)"
        ),
    )
    .with_data(json!({
        "model": canonical,
    }))
    .did_you_mean(suggestions)
    .retry_safe(true)
}

/// A cataloged-but-uncompiled addon model: the tool refuses (this server only reports the
/// compiled form, which is what Hammer draws) and the data carries the `model_compile` hint.
pub(crate) fn model_not_compiled(canonical: &str) -> ToolError {
    ToolError::new(
        "ModelNotCompiled",
        "ModelNotCompiled",
        format!(
            "the model {canonical} has no compiled .vmdl_c (the addon source is not compiled yet); \
             build it with model_compile, or pick a compiled model from the catalog"
        ),
    )
    .with_data(json!({
        "model": canonical,
    }))
    .use_instead("model_compile", "compile the addon source to a .vmdl_c", None)
    .retry_safe(true)
}

/// The decoded facts refused: a clear error naming the model, never empty facts (a cataloged
/// source that disappeared mid-read, or bytes this reader cannot parse — a truncated VPK entry,
/// a recompile in flight).
pub(crate) fn model_decode_error(canonical: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::new(
        "ModelRead",
        "ModelRead",
        format!(
            "the compiled model {canonical} could not be read/decoded: {error} (re-compile the \
             model or pick a different one)"
        ),
    )
    .with_data(json!({
        "model": canonical,
        "error": error.to_string(),
    }))
    .retry_safe(true)
}

/// The compiled bytes of one cataloged model, decoded into facts (the decode is the cpu-bound
/// parse, so it runs inside `spawn_blocking`). Every failure names the model.
pub(crate) async fn decode_compiled(
    state: &HammerState,
    canonical: &str,
) -> Result<ModelInfo, ToolError> {
    let bytes = read_compiled_model(state, canonical).await?;
    tokio::task::spawn_blocking(move || model_info_from_bytes(&bytes))
        .await
        .map_err(|error| ToolError::internal(format!("{PLAN}: the decode task failed: {error}")))?
        .map_err(|error| model_decode_error(canonical, error))
}

/// `resolve` + the compiled facts: what `create_props`' `snapToFloor` reads. A cataloged
/// source that is not compiled (an addon `.vmdl` without its `.vmdl_c`) is
/// [`model_not_compiled`] — `create_props` refuses that one too unless its call said
/// `allowUncompiled:true`.
pub(crate) async fn compiled_model_info(
    state: &HammerState,
    model: &str,
) -> Result<(String, ModelInfo), ToolError> {
    let resolved = resolve(state, model).await?;
    if !resolved.compiled {
        return Err(model_not_compiled(&resolved.canonical));
    }
    let info = decode_compiled(state, &resolved.canonical).await?;
    Ok((resolved.canonical, info))
}

// -------------------------------------------------------------------------------------------
// Argument shapes
// -------------------------------------------------------------------------------------------

/// The lists `include` names; the default is every list except the embedded keyvalue text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Include {
    pub sequences: bool,
    pub attachments: bool,
    pub bodygroups: bool,
    pub keyvalues: bool,
    /// The bone names and parents (`DATA.m_modelSkeleton`), only on request.
    pub skeleton: bool,
}

impl Include {
    /// The default: all lists the tool knows, `keyvalues` only on request.
    fn all() -> Include {
        Include {
            sequences: true,
            attachments: true,
            bodygroups: true,
            keyvalues: false,
            skeleton: false,
        }
    }
}

/// The known `get_model_info` arguments; anything else is refused before any work.
const KNOWN_ARGS: &[&str] = &["model", "include", "limit"];

/// `limit?: integer` — [`LIST_LIMIT_DEFAULT`] when absent, `1..=`[`LIST_LIMIT_MAX`] when given.
pub(crate) fn limit_arg(args: &Value) -> Result<u64, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIST_LIMIT_DEFAULT),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIST_LIMIT_MAX).contains(&limit) => Ok(limit),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIST_LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIST_LIMIT_MAX}, got {other}"
        ))),
    }
}

/// `include?: ["sequences"|"attachments"|"bodygroups"|"keyvalues"]` — the default covers every
/// list except `keyvalues` (the embedded KV3 text can be large and no tool needs it below).
pub(crate) fn include_arg(args: &Value) -> Result<Include, ToolError> {
    match args.get("include") {
        None | Some(Value::Null) => Ok(Include::all()),
        Some(Value::Array(entries)) => {
            let mut include = Include::default();
            for entry in entries {
                let Some(text) = entry.as_str() else {
                    return Err(ToolError::invalid_arguments(format!(
                        "include entries must be strings, got {entry}"
                    )));
                };
                match text {
                    "sequences" => include.sequences = true,
                    "attachments" => include.attachments = true,
                    "bodygroups" => include.bodygroups = true,
                    "keyvalues" => include.keyvalues = true,
                    "skeleton" => include.skeleton = true,
                    other => {
                        return Err(ToolError::invalid_arguments(format!(
                            "unknown include {other:?} (accepted: sequences, attachments, \
                             bodygroups, keyvalues, skeleton)"
                        )))
                    }
                }
            }
            Ok(include)
        }
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "include must be an array of list names, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "get_model_info",
            "Compiled-model facts for placing a prop: render/physics bounds, `facing` (forward +x/-x/+y/-y from front/rear attachments, else unknown + longAxis), `placement` (floorOffset lifts the origin so the model rests on a floor; footprint = x/y box size), skins, bodygroups, sequences, attachments, keyvalues. An uncompiled addon model is refused (ModelNotCompiled). Unknown name: up to 5 suggestions.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "any spelling (case, backslashes, .vmdl_c); also agents/models/** and animation/skeletons/*.vnmskel"
                },
                "include": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["sequences", "attachments", "bodygroups", "keyvalues", "skeleton"]
                    },
                    "description": "default: sequences, attachments, bodygroups; keyvalues, skeleton (bones) only when named"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": LIST_LIMIT_MAX
                }
            },
            "required": ["model"],
            "additionalProperties": false
        })),
        get_model_info,
    );
}

/// `get_model_info {model, include?, limit?}` (ReadOnly — no bridge methods: the catalog and the
/// `.vmdl_c` bytes are local files). Everything the tool reports comes from the compiled file or
/// the catalog scan; a list that ran over `limit` is reported in `truncated`.
pub async fn get_model_info(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let map = match &args {
        Value::Object(map) => map,
        other => {
            return Err(ToolError::invalid_arguments(format!(
                "arguments must be an object, got {other}"
            )))
        }
    };
    for key in map.keys() {
        if !KNOWN_ARGS.contains(&key.as_str()) {
            return Err(ToolError::invalid_arguments(format!(
                "unknown argument {key:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    let model = match args.get("model") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.as_str(),
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "model is required (a models/**.vmdl path), got {other}"
            )))
        }
        None => {
            return Err(ToolError::invalid_arguments(
                "model is required (a models/**.vmdl path)",
            ))
        }
    };
    let include = include_arg(&args)?;
    let limit = limit_arg(&args)? as usize;

    if let Some(compiled) = nm_skeleton_path(model) {
        return nm_skeleton_info(&state, &compiled).await;
    }
    let resolved = resolve(&state, model).await?;
    if !resolved.compiled {
        return Err(model_not_compiled(&resolved.canonical));
    }
    let info = decode_compiled(&state, &resolved.canonical).await?;
    let out = info_json(&resolved, &info, include, limit);
    let unknown_facing = out["facing"]["forward"] == "unknown";
    let output: ToolOutput = out.into();
    Ok(if unknown_facing {
        output.with_next(
            "model_render",
            "the facing is unknown from the file: model_open it and render from two sides",
        )
    } else {
        output
    })
}

/// `animation/skeletons/**.vnmskel` (or `_c`, any case/slashes) -> the compiled path to read.
fn nm_skeleton_path(model: &str) -> Option<String> {
    let path = model.trim().replace('\\', "/").to_lowercase();
    if path.contains("..") || path.contains(':') || path.starts_with('/') {
        return None;
    }
    let stem = path.strip_suffix(".vnmskel_c").or_else(|| path.strip_suffix(".vnmskel"))?;
    (stem.starts_with("animation/skeletons/") && stem.len() > "animation/skeletons/".len()).then(|| format!("{stem}.vnmskel_c"))
}

/// A NmSkeleton (`.vnmskel_c`) from the game paks or the addon: bone names, parents and both
/// reference poses (`[tx, ty, tz, scale, qx, qy, qz, qw]`; the retarget target of `model_import
/// kind:"player"`).
async fn nm_skeleton_info(state: &HammerState, compiled: &str) -> Result<ToolOutput, ToolError> {
    use crate::tools::asset_resolve::{Paths, Sources};
    let paths = Paths::of(state);
    let wanted = compiled.to_string();
    let bytes = tokio::task::spawn_blocking(move || Sources::new(&paths, None).read(&wanted))
        .await
        .map_err(|error| ToolError::internal(format!("{PLAN}: the read task failed: {error}")))?;
    let Some(bytes) = bytes else {
        return Err(ToolError::new(
            "ModelNotFound",
            "ModelNotFound",
            format!("no skeleton {compiled} in the addon or the game paks"),
        )
        .with_data(json!({"model": compiled}))
        .retry_safe(true));
    };
    let skeleton = crate::import::retarget::skeleton::from_resource_bytes(&bytes).map_err(|error| model_decode_error(compiled, error))?;
    let pose = |p: &crate::import::retarget::skeleton::Pose| json!([p.t[0], p.t[1], p.t[2], p.s, p.q[0], p.q[1], p.q[2], p.q[3]]);
    let bones: Vec<Value> = (0..skeleton.names.len())
        .map(|i| {
            json!({
                "index": i,
                "name": skeleton.names[i],
                "parent": skeleton.parents[i].map_or(-1, |p| p as i64),
                "local": pose(&skeleton.local[i]),
                "model": pose(&skeleton.model[i]),
            })
        })
        .collect();
    Ok(ToolOutput::from(json!({
        "model": compiled,
        "kind": "nmskeleton",
        "id": skeleton.id,
        "boneCount": bones.len(),
        "bones": bones,
        "poseFormat": "[tx, ty, tz, scale, qx, qy, qz, qw], inches; local = parent space, model = model space",
    })))
}

// -------------------------------------------------------------------------------------------
// Result assembly (pure, unit-testable)
// -------------------------------------------------------------------------------------------

/// One bounds object: min/max plus the derived size and centre (what a caller drags a box by).
fn box_json(min: [f32; 3], max: [f32; 3]) -> Value {
    let min = [f64::from(min[0]), f64::from(min[1]), f64::from(min[2])];
    let max = [f64::from(max[0]), f64::from(max[1]), f64::from(max[2])];
    let size = [
        max[0] - min[0],
        max[1] - min[1],
        max[2] - min[2],
    ];
    let center = [
        (max[0] + min[0]) / 2.0,
        (max[1] + min[1]) / 2.0,
        (max[2] + min[2]) / 2.0,
    ];
    json!({ "min": min, "max": max, "size": size, "center": center })
}

/// A plain `min`/`max` box (the `m_modelInfo` hull/view spelling that has no derived fields).
fn raw_box(min: [f32; 3], max: [f32; 3]) -> Value {
    json!({
        "min": [f64::from(min[0]), f64::from(min[1]), f64::from(min[2])],
        "max": [f64::from(max[0]), f64::from(max[1]), f64::from(max[2])],
    })
}

// -------------------------------------------------------------------------------------------
// snapToFloor math
// -------------------------------------------------------------------------------------------

/// The rotation matrix Source's `AngleMatrix` writes for a `QAngle` (`pitch` about Y, `yaw`
/// about Z, `roll` about X, `R = Rz(yaw)·Ry(pitch)·Rx(roll)`), as degree input:
///   forward = (cp·cy, cp·sy, −sp); right column = (sp·sr·cy − cr·sy, sp·sr·sy + cr·cy, sr·cp);
///   up column = (sp·cr·cy + sr·sy, sp·cr·sy − sr·cy, cr·cp).
///
/// `create_props` only needs the model-local point's rotated **z** — the matrix's third row on
/// each basis vector: z_rot(x, y, z) = −sp·x + cp·sr·y + cp·cr·z.
///
/// Hand-checkable cases (unit-tested below and end-to-end in `tests/props_live.rs` with the
/// fixture chair `[-10,-10,-2]..[10,10,46]`):
/// * pitch 90° (yaw/roll 0): z_rot = −x — the local x extent becomes the vertical extent, so
///   the chair bottoms out at −10, not −min.z;
/// * roll 90°: z_rot = y — the local y extent becomes vertical, bottom −10 as well;
/// * pitch 180° (upside down): z_rot = −z, bottom −max.z (−46 for the fixture chair);
/// * all zero: z_rot = z, bottom = min.z — exactly the unrotated [`floor_offset`] case.
///
/// The corners are scaled per axis **before** the rotation (it is one matrix multiply either
/// way, but the scaled box's own extremes are what Source applies the vslot to), and the lift
/// is `-min rotated z`.
pub(crate) fn snap_floor_lift(
    (min, max): ([f32; 3], [f32; 3]),
    angles: Option<[f64; 3]>,
    scales: Option<[f64; 3]>,
) -> f64 {
    let (pitch, roll) = match angles {
        Some([pitch, _, roll]) => (pitch.to_radians(), roll.to_radians()),
        _ => (0.0, 0.0),
    };
    let sp = pitch.sin();
    let cp = pitch.cos();
    let sr = roll.sin();
    let cr = roll.cos();
    // the third matrix row: z_rot = −sp·x + cp·sr·y + cp·cr·z (see the doc above; yaw is about
    // the world Z axis, so it cannot move a corner's z and drops out — a yaw-only placement is
    // exactly the old unrotated case)
    let row = [-sp, cp * sr, cp * cr];
    let scales = scales.unwrap_or([1.0; 3]);
    let lo = [
        f64::from(min[0]) * scales[0],
        f64::from(min[1]) * scales[1],
        f64::from(min[2]) * scales[2],
    ];
    let hi = [
        f64::from(max[0]) * scales[0],
        f64::from(max[1]) * scales[1],
        f64::from(max[2]) * scales[2],
    ];
    let mut lowest = f64::INFINITY;
    for x in [lo[0], hi[0]] {
        for y in [lo[1], hi[1]] {
            for z in [lo[2], hi[2]] {
                lowest = lowest.min(row[0] * x + row[1] * y + row[2] * z);
            }
        }
    }
    -lowest
}

/// How far up the origin must move so the model rests on a floor at the origin's z
/// (`-render.min.z`, `get_model_info`'s `placement.floorOffset`). `None` when the render
/// bounds are missing — a caller that needs the offset is refused, never silently skipped.
/// A **rotated** placement must not use this (a pitched/rolled box bottoms out elsewhere);
/// [`snap_floor_lift`] is that case.
pub(crate) fn floor_offset(info: &ModelInfo) -> Option<f64> {
    info.render_bounds.map(|(min, _)| -f64::from(min[2]))
}

/// The render box's x/y size (the `placement.footprint`).
pub(crate) fn footprint(info: &ModelInfo) -> Option<[f64; 2]> {
    info.render_bounds.map(|(min, max)| {
        [
            f64::from(max[0]) - f64::from(min[0]),
            f64::from(max[1]) - f64::from(min[1]),
        ]
    })
}

/// The `bodygroups` list: the flat `ModelInfo::bodygroups` entries (one per mesh-group name)
/// merged into the *named groups* an entity's `bodygroups` mask actually selects — items share
/// the authored group name (`"<group>_@<choice index>_#&<choice name>"`, `resource/model.rs`).
/// An entry without an authored group (Valve's auto-generated `bg_@N` exporter names) has no
/// group to merge into: it carries its raw name and `auto:true` with no choices — the report
/// never invents a group name the file does not carry.
pub(crate) struct MergedBodyGroups {
    pub items: Vec<Value>,
    pub notes: Vec<String>,
}

pub(crate) fn merged_bodygroups(info: &ModelInfo) -> MergedBodyGroups {
    let mut notes: Vec<String> = Vec::new();
    // (group name, item) pairs in bodygroup index order; a same-name entry merges in place.
    let mut items: Vec<(String, Value)> = Vec::new();
    for entry in &info.bodygroups {
        let group = match &entry.group {
            Some(group) => group.clone(),
            None => {
                items.push((
                    entry.name.clone(),
                    json!({
                        "name": entry.name,
                        "choices": Value::Null,
                        "mask": entry.value,
                        "meshCount": entry.mesh_count,
                        "bits": [entry.index],
                        "auto": true,
                    }),
                ));
                continue;
            }
        };
        match items.iter_mut().find(|(name, _)| name == &group) {
            Some((_, item)) => {
                let choice = entry.choice.clone().unwrap_or_default();
                if let Some(choices) = item["choices"].as_array_mut() {
                    // a choice name may repeat across meshes of one group: keep each once
                    if !choices.iter().any(|c| c.as_str() == Some(choice.as_str())) {
                        choices.push(json!(choice));
                    }
                }
                if let Some(mask) = item["mask"].as_u64() {
                    item["mask"] = json!(mask | entry.value);
                }
                if let Some(mesh_count) = item["meshCount"].as_u64() {
                    item["meshCount"] = json!(mesh_count + entry.mesh_count as u64);
                }
                if let Some(bits) = item["bits"].as_array_mut() {
                    bits.push(json!(entry.index));
                }
            }
            None => items.push((
                group.clone(),
                json!({
                    "name": group,
                    "choices": [entry.choice.clone().unwrap_or_default()],
                    "mask": entry.value,
                    "meshCount": entry.mesh_count,
                    "bits": [entry.index],
                }),
            )),
        }
    }
    if info.bodygroups.len() > items.len() {
        notes.push(format!(
            "{} mesh-group entries merged into {} named groups (the bodygroups list reports \
             the authored groups; the raw per-name entries are in resource::model's BodyGroup)",
            info.bodygroups.len(),
            items.len()
        ));
    }
    MergedBodyGroups {
        items: items.into_iter().map(|(_, item)| item).collect(),
        notes,
    }
}

/// A bounded copy of a plain name list; the bool answers "was it cut".
fn included_list(names: &[String], wanted: bool, limit: usize) -> (Vec<Value>, bool) {
    if !wanted {
        return (Vec::new(), false);
    }
    let truncated = names.len() > limit;
    (
        names.iter().take(limit).map(|name| json!(name)).collect(),
        truncated,
    )
}

/// The full result object for one loaded model ([`get_model_info`]'s body, factored out for
/// unit tests).
pub(crate) fn info_json(
    resolved: &Resolved,
    info: &ModelInfo,
    include: Include,
    limit: usize,
) -> Value {
    let mut notes: Vec<String> = Vec::new();
    let mut truncated = serde_json::Map::new();

    let render = info
        .render_bounds
        .map(|(min, max)| box_json(min, max))
        .unwrap_or(Value::Null);
    if info.render_bounds.is_none() {
        notes.push(
            "no MDAT scene-object bounds in the file: the render box (and with it \
             the placement/footprint) is unknown"
                .into(),
        );
    }
    let physics = info
        .physics_bounds
        .map(|(min, max)| box_json(min, max))
        .unwrap_or(Value::Null);
    if info.physics_bounds.is_none() {
        notes.push(
            "no PHYS hull/mesh/sphere box in the file: the physics bounds are unknown"
                .into(),
        );
    }

    let placement = match (floor_offset(info), footprint(info)) {
        (Some(offset), Some(footprint)) => json!({
            "floorOffset": offset,
            "footprint": footprint,
        }),
        _ => json!({ "floorOffset": Value::Null, "footprint": Value::Null }),
    };

    let (sequences, sequences_cut) = included_list(&info.sequences, include.sequences, limit);
    if sequences_cut {
        truncated.insert("sequences".into(), json!(true));
        notes.push(format!(
            "{} sequences over the {limit} limit: the list is cut (truncated.sequences)",
            info.sequences.len()
        ));
    }
    let (attachments, attachments_cut) = included_list(&info.attachments, include.attachments, limit);
    if attachments_cut {
        truncated.insert("attachments".into(), json!(true));
        notes.push(format!(
            "{} attachments over the {limit} limit: the list is cut (truncated.attachments)",
            info.attachments.len()
        ));
    }
    let bodygroups: MergedBodyGroups = if include.bodygroups {
        merged_bodygroups(info)
    } else {
        MergedBodyGroups {
            items: Vec::new(),
            notes: Vec::new(),
        }
    };
    if include.bodygroups && bodygroups.items.len() > limit {
        truncated.insert("bodygroups".into(), json!(true));
        notes.push(format!(
            "{} named bodygroups over the {limit} limit: the list is cut (truncated.bodygroups)",
            bodygroups.items.len()
        ));
    }
    notes.extend(bodygroups.notes);

    let mut out = json!({
        "model": resolved.canonical,
        "sources": resolved.sources,
        "compiled": resolved.compiled,
        "name": info.name,
        "bounds": { "render": render, "physics": physics },
        "hull": info.hull.map(|(min, max)| raw_box(min, max)).unwrap_or(Value::Null),
        "view": info.view.map(|(min, max)| raw_box(min, max)).unwrap_or(Value::Null),
        "mass": info.mass.map(f64::from),
        "surfaceProperty": info.surface_property,
        "hasPhysics": info.has_physics,
        "skins": info.skins,
        "placement": placement,
        "facing": super::model_facing::facing(info),
    });
    let out = out.as_object_mut().expect("json object");
    if include.sequences {
        out.insert("sequences".into(), json!(sequences));
    }
    if include.attachments {
        out.insert("attachments".into(), json!(attachments));
    }
    if include.bodygroups {
        // finding 4: the same honour `limit` gets that sequences/attachments have — the flag and
        // note above answer "was anything beyond `limit` present", nothing more ships after it
        out.insert(
            "bodygroups".into(),
            json!(bodygroups
                .items
                .iter()
                .take(limit)
                .cloned()
                .collect::<Vec<Value>>()),
        );
    }
    if include.skeleton {
        let bones: Vec<Value> = info
            .bones
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, (name, parent))| json!({"index": i, "name": name, "parent": parent}))
            .collect();
        if info.bones.len() > limit {
            truncated.insert("skeleton".into(), json!(true));
            notes.push(format!("{} bones over the {limit} limit: the list is cut (truncated.skeleton)", info.bones.len()));
        }
        out.insert("boneCount".into(), json!(info.bones.len()));
        out.insert("skeleton".into(), json!(bones));
    }
    if include.keyvalues {
        out.insert(
            "keyvalues".into(),
            json!(info.keyvalues_text.clone().unwrap_or_default()),
        );
    }
    // the parser's own notes close the list (they explain every null above already)
    notes.extend(info.notes.iter().cloned());
    out.insert("notes".into(), json!(notes));
    if !truncated.is_empty() {
        out.insert("truncated".into(), Value::Object(truncated));
    }
    Value::Object(out.clone())
}

// -------------------------------------------------------------------------------------------
// unit tests ([`snap_floor_lift`]'s hand-checkable cases + [`info_json`]'s bodygroup cut)
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_floor_lift_matches_the_hand_worked_angles() {
        // the fixture chair box: render min [-10,-10,-2], max [10,10,46]
        let chair: ([f32; 3], [f32; 3]) = ([-10.0, -10.0, -2.0], [10.0, 10.0, 46.0]);
        // unrotated: the old (and still correct) -min.z case, and yaw-only placement is the
        // same no matter the yaw (rotation about Z keeps every corner's z)
        for angles in [None, Some([0.0, 0.0, 0.0]), Some([0.0, 77.0, 0.0])] {
            assert_eq!(snap_floor_lift(chair, angles, None), 2.0);
        }
        // roll 90°: z_rot = y → the y extent [-10, 10] becomes vertical, bottom -10
        assert_eq!(snap_floor_lift(chair, Some([0.0, 0.0, 90.0]), None), 10.0);
        // pitch 90°: z_rot = -x → the x extent becomes vertical, bottom -10 again
        assert_eq!(snap_floor_lift(chair, Some([90.0, 0.0, 0.0]), None), 10.0);
        // pitch 180° (upside down): z_rot = -z → bottom = -max.z = -46
        assert_eq!(snap_floor_lift(chair, Some([180.0, 0.0, 0.0]), None), 46.0);
        // pitch 270° (-90°): z_rot = x → bottom = -min.x = 10 as well
        assert_eq!(snap_floor_lift(chair, Some([-90.0, 0.0, 0.0]), None), 10.0);

        // scales act on the local corners before the rotation: with a non-uniform scale the
        // rotated *scaled* box's bottom is not scales.z × anything
        // pitch 90° + scales [2,1,1]: z_rot = -x with x ∈ [-20, 20] → bottom -20 → lift 20
        assert_eq!(
            snap_floor_lift(chair, Some([90.0, 0.0, 0.0]), Some([2.0, 1.0, 1.0])),
            20.0
        );
        // roll 90° + scales [1,3,1]: z_rot = y with y ∈ [-30, 30] → lift 30
        assert_eq!(
            snap_floor_lift(chair, Some([0.0, 0.0, 90.0]), Some([1.0, 3.0, 1.0])),
            30.0
        );
        // the unrotated scaled case still reads floorOffset × scales.z: -min.z × 3 = 6.
        // With roll 90 instead, z_rot = y: the z extent (and its scale) is horizontal now, so
        // the vertical extent is the unscaled y extent — lift 10, NOT scales.z related
        assert_eq!(
            snap_floor_lift(chair, Some([0.0, 0.0, 90.0]), Some([1.0, 1.0, 3.0])),
            10.0,
        );
        assert_eq!(
            snap_floor_lift(chair, None, Some([1.0, 1.0, 3.0])),
            2.0 * 3.0
        );
    }

    #[test]
    fn get_model_info_bodygroups_honour_the_limit() {
        // three mesh-group entries in three DISTINCT authored groups → three named items
        // (the real fixture chair merges two entries into one group, so it cannot show the cut)
        let info = ModelInfo {
            name: None,
            hull: None,
            view: None,
            mass: None,
            surface_property: None,
            keyvalues_text: None,
            skins: Vec::new(),
            bodygroups: vec![
                crate::resource::model::BodyGroup {
                    index: 0,
                    name: "wheel_@0_#&left".into(),
                    value: 1,
                    mesh_count: 1,
                    group: Some("wheel".into()),
                    choice: Some("left".into()),
                },
                crate::resource::model::BodyGroup {
                    index: 1,
                    name: "handle_@0_#&brass".into(),
                    value: 2,
                    mesh_count: 1,
                    group: Some("handle".into()),
                    choice: Some("brass".into()),
                },
                crate::resource::model::BodyGroup {
                    index: 2,
                    name: "lid_@0_#&on".into(),
                    value: 4,
                    mesh_count: 1,
                    group: Some("lid".into()),
                    choice: Some("on".into()),
                },
            ],
            sequences: Vec::new(),
            attachments: Vec::new(),
            attachment_points: Vec::new(),
            has_physics: None,
            render_bounds: None,
            physics_bounds: None,
            bones: Vec::new(),
            blocks: Vec::new(),
            notes: Vec::new(),
        };
        let resolved = Resolved {
            canonical: "models/props/fixture/duo.vmdl".into(),
            sources: vec![],
            compiled: true,
        };
        // limit 1: one item ships, the cut is flagged and noted
        let out = info_json(&resolved, &info, Include::all(), 1);
        let items = out["bodygroups"].as_array().expect("bodygroups");
        assert_eq!(items.len(), 1, "{out}");
        assert_eq!(items[0]["name"], json!("wheel"), "{out}");
        assert_eq!(out["truncated"], json!({"bodygroups": true}), "{out}");
        let notes = out["notes"].as_array().unwrap();
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or_default()
                .contains("3 named bodygroups")),
            "{notes:?}"
        );
        // limit 3: everything fits, no flag at all
        let out = info_json(&resolved, &info, Include::all(), 3);
        assert_eq!(out["bodygroups"].as_array().unwrap().len(), 3, "{out}");
        assert!(out.get("truncated").is_none(), "{out}");
    }
}
