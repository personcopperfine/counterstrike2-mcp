//! `create_props` (WP-R1: target/WP.md): place prop model entities in one `edit.batch` — one
//! undo group of `entity.create` ops, every prop locally validated **before** anything is sent.
//! The write gate, the `expectDoc` fingerprint of the pre-write `doc.active` read, the
//! unsaved-edit counter, the maybe-executed taxonomy and the batch plumbing are
//! `entities_live.rs`'s shared live-mutator wiring (review L7); this file is the prop-specific
//! argument and model work:
//!
//! * **class selection**: `kind` (`"static"|"dynamic"|"physics"`) picks the fixed
//!   `prop_static`/`prop_dynamic`/`prop_physics` classes; `class` names an explicit FGD class
//!   that must carry a prop tag (`fgd::Database::prop_tag`, the `tags(Prop*)` helper through
//!   the resolved base chain — WP-P's eligibility rule). Exactly one of the two. A known
//!   class without a prop tag is refused `NotAPropClass`, an unknown one `UnknownClass`,
//!   both with nothing sent.
//! * **model**: required; normalized and catalog-looked-up through the shared
//!   `model_info::resolve`, so `create_props` speaks `get_model_info`'s error terms:
//!   `ModelNotFound` with the catalog's suggestions, and the catalog's canonical `models/**`
//!   spelling is what the op's `model` keyvalue carries. *Uncompiled* addon models are
//!   refused `ModelNotCompiled` with the `model_compile` hint — the compiled form is what
//!   Hammer draws — unless the call said `allowUncompiled:true`, which places them with a
//!   per-prop note that the model does not draw until the addon compiles and the document
//!   reloads.
//! * **scales**: optional `[x,y,z]`, each finite in `(0, 1000]` (the m2.18 builder's own
//!   bound, BRIDGE-M2.18-SPEC §2, applied locally so the refusal is pre-send). Scaled creates
//!   need the m2.18 builder (`entity.create`'s SetScales vslot); a bridge that does not
//!   advertise `node.setTransform` is older than m2.18, and a call with any scaled prop is
//!   refused `BridgeTooOld` before the catalog is even consulted. Without scales older
//!   builders still work — the result notes that a pre-m2.18 write does not make the model
//!   draw until the document reloads (re-props.md Q1).
//! * **snapToFloor**: lifts the prop's origin so the model's render box rests on a floor at
//!   the target z: the lift is the lowest of the box's 8 corners, each corner scaled per axis
//!   and rotated by the prop's `angles` (Source QAngle convention: pitch about Y, yaw about
//!   Z, roll about X — `model_info::snap_floor_lift`). Yaw-only placements give exactly
//!   `get_model_info`'s `placement.floorOffset` × `scales.z` (the old, still-correct case);
//!   any pitch/roll rotates the box and is now measured on the rendered geometry, not the
//!   raw `-min.z` (review w2b #1). The applied
//!   offset rides the result item (`floorOffsetApplied`). No render bounds → `ModelBounds`
//!   refusal, never a silent skip (brief: no silent fallbacks); no compiled model (`allow
//!   Uncompiled`) → refused too: there is nothing to measure.
//! * **keyvalues**: `skin` and `targetname` are first-class fields; anything else rides
//!   `keyValues`, merged over them and validated against the FGD exactly like
//!   `create_entities` — with one stricter reading: an FGD *error* refuses the whole call
//!   (a prop batch that knowingly produces invalid entities sends nothing; warnings ride).
//!   `model`/`classname`/`origin`/`angles`/`scales`/`skin`/`targetname` inside `keyValues`
//!   are refused — the fields above are the validated spellings of those keys.
//! * **the notes**: `prop_static` is baked into the world at compile time (`compile_map`);
//!   it is not a runtime entity — no targetname/I/O at runtime. `prop_dynamic` (and physics)
//!   are runtime entities driven by I/O. The batch is never saved; the group label is the
//!   bridge's own `MCP: batch` for a create batch (m2.15 §7), like `create_entities`.
//! * **the per-prop result item**: `index`, `nodeID` (the bridge's create reply carries the
//!   created node's id since the W2 fix; a reply without it has no `nodeID` here), the bridge's
//!   `class`/`origin`/`scales`/`keyValues` read-back, this server's `kind`/`model` and the
//!   transform the call requested — `angles` and `scales`, each `null` when the caller omitted it
//!   (review W2 3b). `scales` therefore appears twice in the item when it was sent: the bridge's
//!   echo is the applied value, this server's record of the request; they agree on a success.
use crate::state::HammerState;
use crate::tools::entities_live::{
    batch_failure, batch_maybe_executed, keyvalues_arg, note_edit,
    result_items, short_batch_failure, unknown_class, vec3_arg, FgdNotes, FgdSource, MAX_ENTITIES,
    MAX_OP_BYTES, MAX_VALUE_BYTES,
};
use crate::tools::live_common::{bool_arg, doc_id_of, expect_doc, require_document};
use crate::tools::model_info::{compiled_model_info, resolve, snap_floor_lift};
/// The render box as [`crate::resource::Aabb`]-shaped bounds: what the snap pass caches per
/// canonical model instead of a one-size lift (the lift depends on each prop's angles/scales).
type FloorCacheEntry = Option<crate::resource::Aabb>;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};

/// The known top-level arguments; anything else is refused before any dispatch.
const KNOWN_ARGS: &[&str] = &["props", "allowUncompiled"];

/// One `edit.batch` fits at most this many props (the `create_entities` batch bound).
pub(crate) const MAX_PROPS: usize = MAX_ENTITIES;
/// The m2.18 build marker: a bridge that does not advertise it cannot apply scales at create
/// time (BRIDGE-M2.18-SPEC §2: `entity.create`'s SetScales vslot; the spec's k_methods row).
/// `undo_last` (entities_live) reuses it as the "this build really records value undo" marker.
pub(crate) const M2_18_MARKER: &str = "node.setTransform";
/// The scale value bound the m2.18 builder itself checks (spec §2: each in `(0, 1000]`).
const SCALES_MAX: f64 = 1000.0;
/// An origin component the bridge's plain-decimal number scan reads exactly
/// (`entities_live::coord_axis`'s bound; a snapToFloor lift is checked against it too).
const ORIGIN_MAX_ABS: f64 = 1e9;

/// A kind (the `kind` argument's spelling) with the FGD class it names.
struct Kind {
    spelling: &'static str,
    class: &'static str,
}

const KINDS: [Kind; 3] = [
    Kind { spelling: "static", class: "prop_static" },
    Kind { spelling: "dynamic", class: "prop_dynamic" },
    Kind { spelling: "physics", class: "prop_physics" },
];

fn kind_of(spelling: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|kind| kind.spelling == spelling)
}

/// The kind spelling of an FGD prop tag (`PropTag` → the `kind` argument's vocabulary).
fn kind_spelling_of_tag(tag: crate::fgd::PropTag) -> &'static str {
    match tag {
        crate::fgd::PropTag::Static => "static",
        crate::fgd::PropTag::Dynamic => "dynamic",
        crate::fgd::PropTag::Physics => "physics",
    }
}

/// The per-kind runtime note (WP-R1's result contract: static is compile-time, dynamic and
/// physics are runtime entities). Worded by the prop **tag**, not a class name: an explicit
/// class with the same tag is what the note describes too (review w2b #11 — a
/// "prop_dynamic is a runtime entity" note on a `prop_door_rotating` create named the wrong
/// class).
fn kind_note(kind: &Kind) -> &'static str {
    match kind.spelling {
        "static" => {
            "A PropStatic class is baked into the world at compile time (compile_map): it is \
             not a runtime entity, no targetname/I/O at runtime"
        }
        "dynamic" => "A PropDynamic class is a runtime entity: driven by I/O",
        _ => {
            "A PropPhysics class is a runtime entity driven by I/O, and the engine simulates \
             it (collisions, gravity)"
        }
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

// -------------------------------------------------------------------------------------------
// argument parsing (pure: no I/O, so a shape error happens before any scan)
// -------------------------------------------------------------------------------------------

/// `props[i].scales`: three finite numbers, each in `(0, 1000]` — the bound the m2.18 builder
/// itself checks with `InvalidParams` before anything is created (spec §2), applied locally so
/// the refusal happens before the batch is even assembled. `None` = absent/null.
fn scales_arg(label: &str, value: Option<&Value>) -> Result<Option<[f64; 3]>, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid(format!(
            "{label}.scales: [x, y, z] of numbers in (0, {SCALES_MAX}]"
        )));
    };
    let mut out = [0.0f64; 3];
    for (axis, axis_value) in axes.iter().enumerate() {
        match axis_value.as_f64() {
            Some(number) if number.is_finite() && number > 0.0 && number <= SCALES_MAX => {
                out[axis] = number;
            }
            Some(number) => {
                return Err(invalid(format!(
                    "{label}.scales[{axis}]: {number} is outside (0, {SCALES_MAX}] (0, negative \
                     and non-finite scales are refused before anything is created)"
                )))
            }
            None => {
                return Err(invalid(format!(
                    "{label}.scales[{axis}]: {axis_value} is not a finite number"
                )))
            }
        }
    }
    Ok(Some(out))
}

/// A bounded string field: the same 511-byte value cap and control/NUL prohibitions every
/// keyvalue write here carries (`entities_live::MAX_VALUE_BYTES`).
fn text_bound(label: &str, text: &str) -> Result<(), ToolError> {
    if text.len() > MAX_VALUE_BYTES {
        return Err(invalid(format!(
            "{label}: the value is {} bytes, at most {MAX_VALUE_BYTES} fit",
            text.len()
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{label}: a control character is not writable (the engine stores C strings)"
        )));
    }
    if text.contains('\0') {
        return Err(invalid(format!(
            "{label}: a value cannot contain a NUL character (the engine cuts it there)"
        )));
    }
    Ok(())
}

/// One parsed prop: everything the op needs, with the model/FGD/snap passes still ahead.
struct ParsedProp {
    index: usize,
    /// The `kind` argument's mapping, `None` when the caller named an explicit `class`.
    kind: Option<&'static Kind>,
    /// The FGD class: the kind-mapped default, or the caller's explicit class.
    class: String,
    /// The raw `model` argument until the model pass replaces it with the canonical spelling.
    model: String,
    origin: [f64; 3],
    angles: Option<[f64; 3]>,
    scales: Option<[f64; 3]>,
    skin: Option<String>,
    targetname: Option<String>,
    keyvalues: Map<String, Value>,
    snap_to_floor: bool,
    /// The lift this prop got (`snapToFloor`), also reported in the result item.
    floor_offset: Option<f64>,
    /// The model-pass finding, `None` until then.
    model_ref: Option<ModelRef>,
}

struct ModelRef {
    /// The catalog's canonical spelling (`models/**.vmdl`), the `model` keyvalue sent.
    canonical: String,
    compiled: bool,
}

/// The `props[i]` keys; anything else is a per-prop `InvalidArguments`.
const PROP_KEYS: &[&str] = &[
    "kind",
    "class",
    "model",
    "origin",
    "angles",
    "scales",
    "skin",
    "targetname",
    "keyValues",
    "snapToFloor",
];

fn parse_prop(index: usize, prop: &Value) -> Result<ParsedProp, ToolError> {
    let label = format!("props[{index}]");
    let Some(object) = prop.as_object() else {
        return Err(invalid(format!("{label} must be an object")));
    };
    for key in object.keys() {
        if !PROP_KEYS.contains(&key.as_str()) {
            return Err(invalid(format!(
                "unknown {label} field {key:?} (accepted: {})",
                PROP_KEYS.join(", ")
            )));
        }
    }
    let kind = match object.get("kind") {
        None | Some(Value::Null) => None,
        Some(Value::String(spelling)) => Some(kind_of(spelling).ok_or_else(|| {
            invalid(format!(
                "{label}.kind must be \"static\", \"dynamic\" or \"physics\", got {spelling:?}"
            ))
        })?),
        Some(other) => {
            return Err(invalid(format!(
                "{label}.kind must be \"static\", \"dynamic\" or \"physics\", got {other}"
            )))
        }
    };
    let explicit_class = match object.get("class") {
        None | Some(Value::Null) => None,
        Some(Value::String(class)) => {
            let trimmed = class.trim();
            if trimmed.is_empty() {
                return Err(invalid(format!(
                    "{label}.class is empty (pass kind or a non-empty FGD class)"
                )));
            }
            Some(trimmed.to_string())
        }
        Some(other) => {
            return Err(invalid(format!(
                "{label}.class must be a string, got {other}"
            )))
        }
    };
    let class = match (kind, explicit_class) {
        (None, None) => {
            return Err(invalid(format!(
                "{label}: exactly one of kind (static|dynamic|physics) or class (an FGD prop \
                 class) is required"
            )))
        }
        (Some(_), Some(_)) => {
            return Err(invalid(format!(
                "{label}: exactly one of kind or class, got both (the kind already names its \
                 FGD class)"
            )))
        }
        (Some(kind), None) => kind.class.to_string(),
        (None, Some(class)) => class,
    };

    let model = match object.get("model") {
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(invalid(format!("{label}.model is empty")));
            }
            trimmed.to_string()
        }
        Some(other) => {
            return Err(invalid(format!("{label}.model must be a string, got {other}")))
        }
        None => return Err(invalid(format!("{label}.model is required"))),
    };
    let origin = vec3_arg(&format!("{label}.origin"), object.get("origin"))?;
    let angles = match object.get("angles").filter(|value| !value.is_null()) {
        Some(_) => Some(vec3_arg(&format!("{label}.angles"), object.get("angles"))?),
        None => None,
    };
    let scales = scales_arg(&label, object.get("scales"))?;
    let skin = match object.get("skin").filter(|value| !value.is_null()) {
        Some(Value::String(skin)) => {
            let trimmed = skin.trim();
            if trimmed.is_empty() {
                return Err(invalid(format!(
                    "{label}.skin is empty (omit it for the model's default skin)"
                )));
            }
            text_bound(&format!("{label}.skin"), trimmed)?;
            Some(trimmed.to_string())
        }
        Some(other) => {
            return Err(invalid(format!("{label}.skin must be a string, got {other}")))
        }
        None => None,
    };
    let targetname = match object.get("targetname").filter(|value| !value.is_null()) {
        Some(Value::String(name)) => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(invalid(format!(
                    "{label}.targetname is empty (omit it for no name)"
                )));
            }
            text_bound(&format!("{label}.targetname"), trimmed)?;
            // the same 511-byte value cap every keyvalue write carries (a name sent through
            // create_entities.keyValues may be that long; this field must not be stricter)
            if trimmed.len() > MAX_VALUE_BYTES {
                return Err(invalid(format!(
                    "{label}.targetname: the name is {} bytes, at most {MAX_VALUE_BYTES} fit",
                    trimmed.len()
                )));
            }
            Some(trimmed.to_string())
        }
        Some(other) => {
            return Err(invalid(format!(
                "{label}.targetname must be a string, got {other}"
            )))
        }
        None => None,
    };
    let keyvalues = match object.get("keyValues").filter(|value| !value.is_null()) {
        Some(keyvalues) => {
            let map = keyvalues_arg(&format!("{label}.keyValues"), keyvalues)?;
            for key in map.keys() {
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "model" | "classname" | "origin" | "angles" | "scales" | "skin" | "targetname"
                ) {
                    return Err(invalid(format!(
                        "{label}.keyValues.{key} cannot be set as a keyvalue: use the prop's own \
                         fields (this tool validates them for the class)"
                    )));
                }
            }
            map
        }
        None => Map::new(),
    };
    let snap_to_floor = match object.get("snapToFloor") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Bool(false)) => false,
        Some(other) => {
            return Err(invalid(format!(
                "{label}.snapToFloor must be a boolean, got {other}"
            )))
        }
    };
    Ok(ParsedProp {
        index,
        kind,
        class,
        model,
        origin,
        angles,
        scales,
        skin,
        targetname,
        keyvalues,
        snap_to_floor,
        floor_offset: None,
        model_ref: None,
    })
}

/// The `keyValues` object the op carries (the same map the FGD check validates): the canonical
/// model first (it is the reason the tool exists), then skin/targetname on top as fields, then
/// the caller's keyvalues (already bounded + type-checked by `keyvalues_arg`).
fn merged_keyvalues(prop: &ParsedProp) -> Map<String, Value> {
    let mut keyvalues = Map::new();
    keyvalues.insert("model".into(), json!(prop.model));
    if let Some(skin) = &prop.skin {
        keyvalues.insert("skin".into(), json!(skin));
    }
    if let Some(targetname) = &prop.targetname {
        keyvalues.insert("targetname".into(), json!(targetname));
    }
    for (key, value) in &prop.keyvalues {
        keyvalues.insert(key.clone(), value.clone());
    }
    keyvalues
}

/// The `entity.create` op for one prop: the exact `entity.create` contract of
/// BRIDGE-M2.18-SPEC §1/§2 — `{class, origin, angles?, scales?, keyValues}`.
fn build_op(prop: &ParsedProp) -> (Value, usize) {
    let mut params = Map::new();
    params.insert("class".into(), json!(prop.class));
    params.insert("origin".into(), json!(prop.origin));
    if let Some(angles) = prop.angles {
        params.insert("angles".into(), json!(angles));
    }
    if let Some(scales) = prop.scales {
        params.insert("scales".into(), json!(scales));
    }
    params.insert("keyValues".into(), Value::Object(merged_keyvalues(prop)));
    let op = json!({"method": "entity.create", "params": Value::Object(params)});
    let bytes = op["params"].to_string().len();
    (op, bytes)
}

// -------------------------------------------------------------------------------------------
// the I/O passes (model catalog, FGD, snapToFloor facts)
// -------------------------------------------------------------------------------------------

/// The model pass: resolve + compile status, with `allowUncompiled` applied. The shared
/// `resolve`'s errors name the model (`ModelNotFound` + the catalog's suggestions); the
/// prop's index is prepended so a multi-prop call points at its entry.
async fn resolve_model(
    state: &HammerState,
    prop_index: usize,
    model: &str,
    allow_uncompiled: bool,
) -> Result<ModelRef, ToolError> {
    let resolved = resolve(state, model).await.map_err(|error| {
        let mut error = error;
        error.message = format!("props[{prop_index}]: {}", error.message);
        error
    })?;
    if !resolved.compiled && !allow_uncompiled {
        let mut refused = crate::tools::model_info::model_not_compiled(&resolved.canonical);
        refused.message = format!("props[{prop_index}]: {}", refused.message);
        return Err(refused);
    }
    Ok(ModelRef {
        canonical: resolved.canonical,
        compiled: resolved.compiled,
    })
}

/// The FGD pass: class eligibility (unknown class; an explicit class without a prop tag) plus
/// the keyvalue check of the merged keyvalues. Returns the notes the result item merges; an
/// FGD *error* refuses the whole call. The FGD being unavailable is handled by the caller
/// (a note names what was not checked — never silence, create_entities' warning policy).
fn check_fgd(fgd: &FgdSource, prop: &ParsedProp) -> Result<FgdNotes, ToolError> {
    let Some(db) = fgd.db() else {
        return Ok(FgdNotes::from_issues(&[]));
    };
    let label = format!("props[{}]", prop.index);
    let keyvalues = merged_keyvalues(prop);
    let validation = db.validate(&prop.class, &keyvalues);
    if validation.unknown_class {
        return Err(unknown_class(prop.index, &prop.class, &validation.nearest));
    }
    if prop.kind.is_none() && db.prop_tag(&prop.class).is_none() {
        return Err(ToolError::new(
            "NotAPropClass",
            "NotAPropClass",
            format!(
                "{label}: class '{}' is in the FGD but carries no prop tag (tags(PropStatic|\
                 PropDynamic|PropPhysics), itself or through base()) — create_entities places \
                 non-prop entities",
                prop.class
            ),
        )
        .with_data(json!({"index": prop.index, "class": prop.class}))
        .retry_safe(true));
    }
    let notes = FgdNotes::from_issues(&validation.issues);
    if notes.has_errors() {
        return Err(invalid(format!(
            "{label} (class {}) failed the FGD keyvalue check: {}",
            prop.class,
            notes.errors.join("; ")
        )));
    }
    Ok(notes)
}

/// The snapToFloor pass for one prop: the compiled model's render bounds (cached per distinct
/// canonical model across the call) turned into a lift with the prop's own `angles` and
/// `scales` — [`snap_floor_lift`]: the 8 corners of the box, scaled per axis, rotated by the
/// Source QAngle convention, so a pitched/rolled prop is lifted by where its rendered model
/// actually bottoms out (a yaw-only placement gives exactly the unrotated `-min.z · scales.z`
/// lift `get_model_info` reports as `placement.floorOffset`). No render bounds →
/// `ModelBounds` (nothing measurable, never a silent skip); applying it updates the origin and
/// stores the applied offset for the result.
async fn apply_snap_to_floor(
    state: &HammerState,
    // Option<Aabb>: the render bounds, `None` = the model has none
    floor_cache: &mut std::collections::BTreeMap<String, FloorCacheEntry>,
    prop: &mut ParsedProp,
) -> Result<(), ToolError> {
    let label = format!("props[{}]", prop.index);
    let reference = prop
        .model_ref
        .as_ref()
        .expect("the model pass ran before snapToFloor");
    if !reference.compiled {
        return Err(ToolError::new(
            "ModelBounds",
            "ModelBounds",
            format!(
                "{label}: {} has no compiled .vmdl_c, and snapToFloor needs the compiled \
                 model's placement bounds to lift the origin — there is nothing to measure \
                 (the same Kind as a model without render bounds)",
                reference.canonical
            ),
        )
        .with_data(json!({"model": reference.canonical, "missing": "compiled"}))
        .retry_safe(true));
    }
    let bounds = match floor_cache.get(&reference.canonical) {
        Some(cached) => *cached,
        None => {
            let (_, info) = compiled_model_info(state, &reference.canonical).await?;
            let bounds = info.render_bounds;
            floor_cache.insert(reference.canonical.clone(), bounds);
            bounds
        }
    };
    let Some(bounds) = bounds else {
        return Err(ToolError::new(
            "ModelBounds",
            "ModelBounds",
            format!(
                "{label}: {} carries no render bounds, so its placement floor offset is \
                 unknown and snapToFloor has nothing to measure (get_model_info reports the \
                 same nothing)",
                reference.canonical
            ),
        )
        .with_data(json!({"model": reference.canonical}))
        .retry_safe(true));
    };
    let lift = snap_floor_lift(bounds, prop.angles, prop.scales);
    let raised = prop.origin[2] + lift;
    if raised.abs() > ORIGIN_MAX_ABS {
        return Err(invalid(format!(
            "{label}.origin z after the snapToFloor lift ({raised}) is beyond |{ORIGIN_MAX_ABS}| \
             (serde_json would write exponent notation, which the bridge's number scan \
             misreads); place the prop lower"
        )));
    }
    prop.origin[2] = raised;
    prop.floor_offset = Some(lift);
    Ok(())
}

/// The kind the result item reports: the `kind` argument's spelling, or — for an explicit
/// class — the FGD prop tag's kind when the database named one (`create_entities`' warning
/// policy keeps the unavailable-database case honest: nothing is guessed).
fn result_kind(prop: &ParsedProp, fgd: &FgdSource) -> Value {
    match prop.kind {
        Some(kind) => json!(kind.spelling),
        None => match fgd.db().and_then(|db| db.prop_tag(&prop.class)) {
            Some(tag) => json!(kind_spelling_of_tag(tag)),
            None => Value::Null,
        },
    }
}

/// Whether the bridge can carry m2.18's create-time scales at all: `node.setTransform` is
/// advertised (connected first — a dead bridge surfaces as `BridgeNotRunning` here), one
/// capabilities refresh, then the answer.
async fn bridge_has_marker(state: &HammerState) -> Result<bool, ToolError> {
    let mut bridge = state.bridge.lock().await;
    bridge.ensure_connected().await?;
    let mut missing = bridge.missing(&[M2_18_MARKER]);
    if !missing.is_empty() {
        let _ = bridge.refresh_capabilities().await;
        missing = bridge.missing(&[M2_18_MARKER]);
    }
    Ok(missing.is_empty())
}

// -------------------------------------------------------------------------------------------
// the handler
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "create_props",
            "Place prop entities in one undo group (never saves). Each prop: `kind` static|dynamic|physics, or an explicit prop-tagged FGD `class` (exactly one). `model` is catalog-checked (unknown -> ModelNotFound with suggestions; uncompiled addon model refused unless allowUncompiled). `scales` need a bridge with node.setTransform, else the whole call is refused (BridgeTooOld).",
            Mutating,
            &["edit.batch", "entity.create"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "props": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_PROPS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["static", "dynamic", "physics"],
                                "description": "prop_static / prop_dynamic / prop_physics"
                            },
                            "class": {
                                "type": "string",
                                "description": "FGD class carrying a prop tag, else NotAPropClass"
                            },
                            "model": {
                                "type": "string",
                                "description": "models/**.vmdl"
                            },
                            "origin": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 3,
                                "maxItems": 3,
                                "description": "world units"
                            },
                            "angles": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 3,
                                "maxItems": 3,
                                "description": "pitch, yaw, roll"
                            },
                            "scales": {
                                "type": "array",
                                "items": {"type": "number"},
                                "minItems": 3,
                                "maxItems": 3,
                                "description": "each in (0, 1000]"
                            },
                            "skin": {
                                "type": "string",
                                "description": "skin name from get_model_info"
                            },
                            "targetname": {
                                "type": "string",
                                "description": "prop_static ignores it at runtime"
                            },
                            "keyValues": {
                                "type": "object",
                                "description": "other keyvalues, FGD-checked; not model/classname/origin/angles/scales/skin/targetname",
                                "additionalProperties": {"type": ["string", "number", "boolean"]}
                            },
                            "snapToFloor": {
                                "type": "boolean",
                                "description": "lift origin.z by the model floorOffset (get_model_info), scaled by scales.z"
                            }
                        },
                        "required": ["model", "origin"],
                        "additionalProperties": false
                    }
                },
                "allowUncompiled": {
                    "type": "boolean",
                    "default": false,
                    "description": "place props whose model has no compiled .vmdl_c (they do not draw until compiled)"
                }
            },
            "required": ["props"],
            "additionalProperties": false
        })),
        create_props,
    );
}

/// One bridge-older-than-m2.18 create note (re-props.md Q1): the raw keyvalue setter does not
/// make Hammer load/draw the model, the white placeholder stays until the document reloads.
fn pre_m2_18_note() -> Value {
    json!(
        "the bridge is older than m2.18: entity.create writes the model keyvalue through the \
         raw setter, which does not make Hammer load and draw the model — a prop shows its \
         white placeholder box until the document reloads (re-props.md Q1); a scales-bearing \
         call would have been refused BridgeTooOld"
    )
}


/// `create_props {props, allowUncompiled?}`: one `edit.batch` of `entity.create` ops. The
/// scale/m2.18 marker check runs after the shape pass and before any catalog work (a refused
/// scales call must not even read a VPK); the catalog, FGD and snapToFloor passes keep
/// everything local, so any refusal sends nothing.
pub async fn create_props(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let map = match &args {
        Value::Object(map) => map,
        other => {
            return Err(invalid(format!(
                "arguments must be an object, got {other}"
            )))
        }
    };
    for key in map.keys() {
        if !KNOWN_ARGS.contains(&key.as_str()) {
            return Err(invalid(format!(
                "unknown argument {key:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    let Some(props) = args.get("props").and_then(Value::as_array) else {
        return Err(invalid(format!(
            "props is required (an array of 1..={MAX_PROPS} prop objects)"
        )));
    };
    if props.is_empty() {
        return Err(invalid("props is empty, there is nothing to create"));
    }
    if props.len() > MAX_PROPS {
        return Err(invalid(format!(
            "props has {} entries, at most {MAX_PROPS} fit in one edit.batch",
            props.len()
        )));
    }
    let allow_uncompiled = bool_arg(&args, "allowUncompiled")?.unwrap_or(false);

    // pass 1: the shapes (no I/O yet, so an ill-typed prop never triggers a scan)
    let mut parsed: Vec<ParsedProp> = Vec::with_capacity(props.len());
    for (index, prop) in props.iter().enumerate() {
        parsed.push(parse_prop(index, prop)?);
    }

    // pass 2: the m2.18 marker (it decides the scales refusal AND the pre-m2.18 note; one
    // connect, done before the catalog/FGD work so a scaled refusal costs no VPK read)
    let bridge_is_m2_18 = bridge_has_marker(&state).await?;
    if !bridge_is_m2_18 && parsed.iter().any(|prop| prop.scales.is_some()) {
        return Err(ToolError::new(
            "BridgeTooOld",
            "BridgeTooOld",
            format!(
                "the bridge does not advertise {M2_18_MARKER}: it is older than m2.18, and \
                 scales at create time need the m2.18 builder (entity.create's SetScales \
                 vslot, BRIDGE-M2.18-SPEC §2) — or the Hammer module is simply not loaded yet. \
                 Send the props without scales, or update the bridge to m2.18 (nothing was sent)"
            ),
        )
        .with_code(-32011)
        .with_data(json!({
            "missing": [M2_18_MARKER],
            "blockedOn": "bridge m2.18 (entity.create scales, node.setTransform)",
        }))
        .retry_safe(true));
    }

    // the document read comes before the catalog: it refreshes the addon the model catalog is
    // built for (the addon follows the active document)
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);

    let fgd = FgdSource::load(&state);
    let fgd_unavailable = fgd.db().is_none();

    // pass 3: the model catalog, the canonical replacement per prop
    for prop in &mut parsed {
        let reference = resolve_model(&state, prop.index, &prop.model, allow_uncompiled).await?;
        prop.model = reference.canonical.clone();
        prop.model_ref = Some(reference);
    }

    // pass 3b: the FGD class eligibility + keyvalue check of every prop
    let mut notes: Vec<FgdNotes> = Vec::with_capacity(parsed.len());
    for prop in &parsed {
        notes.push(check_fgd(&fgd, prop)?);
    }

    // pass 3c: snapToFloor facts (the render bounds cached per distinct canonical model across
    // the call; the lift itself depends on the prop's angles/scales)
    let mut floor_cache: std::collections::BTreeMap<String, FloorCacheEntry> =
        std::collections::BTreeMap::new();
    for prop in &mut parsed {
        if prop.snap_to_floor {
            apply_snap_to_floor(&state, &mut floor_cache, prop).await?;
        }
    }

    // pass 4: the ops — everything is validated; the only remaining failure is the tool's own
    // op-size contract
    let mut ops: Vec<Value> = Vec::with_capacity(parsed.len());
    for (index, prop) in parsed.iter().enumerate() {
        let (op, bytes) = build_op(prop);
        if bytes >= MAX_OP_BYTES {
            return Err(invalid(format!(
                "props[{index}] serializes to {bytes} bytes of op params; the bridge cuts op \
                 params at 8192, keep each prop under {MAX_OP_BYTES} bytes"
            )));
        }
        ops.push(op);
    }

    let mut params = Map::new();
    // the m2.15 edit.batch arm takes no undoLabel (a create batch is "MCP: batch"); echoed
    // for parity like create_entities does
    params.insert("undoLabel".into(), json!("MCP: create props"));
    params.insert("ops".into(), Value::Array(ops.clone()));

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "edit.batch",
                Value::Object(params),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(&doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if batch_maybe_executed(&error) {
                note_edit(&state, doc_id.as_deref());
            }
            return Err(batch_failure(error, &state, &ops));
        }
    };
    note_edit(&state, doc_id.as_deref());

    if !reply.result.is_object() {
        return Err(ToolError::internal(format!(
            "edit.batch returned a non-object result: {} (the m2.15 §7 `results` contract)",
            reply.result
        )));
    }
    // fewer results than ops sent: an unknown number ran (brief item 2) — never a success
    let created = result_items(&reply.result["results"], &ops);
    if created.len() != ops.len() {
        return Err(short_batch_failure(
            &state,
            created,
            ops.len(),
            reply
                .result
                .get("failedIndex")
                .cloned()
                .unwrap_or(Value::Null),
        ));
    }

    // merge the per-prop fields (kind, model, floor offset, notes) over the sanitized bridge
    // results; `result_items` already replaced handles with handleIssued:true
    let overall_warnings: Vec<String> = fgd.warning().into_iter().collect();
    let mut overall_notes: Vec<Value> = Vec::new();
    overall_notes.push(json!(
        "no auto-save: run save_map to write the changes; file-backed reads remain stale until then"
    ));
    if !bridge_is_m2_18 {
        // this call went through on an older builder: the model note is what a caller needs
        // (a scales-bearing prop was refused above, so a passed call is scale-less)
        overall_notes.push(pre_m2_18_note());
    }
    if fgd_unavailable {
        overall_notes.push(json!(
            "fgd unavailable: the prop-tag class check and the keyvalue validation were not \
             run; an unknown class or an invalid keyvalue can still fail the batch at the \
             bridge (PartialFailure)"
        ));
    }

    let items: Vec<Value> = created
        .into_iter()
        .enumerate()
        .map(|(index, mut item)| {
            let prop = &parsed[index];
            let entity_notes = &notes[index];
            if let Some(object) = item.as_object_mut() {
                object.insert("kind".into(), result_kind(prop, &fgd));
                object.insert("model".into(), json!(prop.model));
                // the transform this prop was created with, as sent: what was asked for, `null`
                // when the caller omitted it (review W2 3b — the bridge echoes the applied
                // `origin`/`scales`, but nothing echoed `angles`, so a caller could not see which
                // orientation its own call requested)
                object.insert(
                    "angles".into(),
                    prop.angles
                        .map(|angles| json!(angles))
                        .unwrap_or(Value::Null),
                );
                object.insert(
                    "scales".into(),
                    prop.scales
                        .map(|scales| json!(scales))
                        .unwrap_or(Value::Null),
                );
                if let Some(offset) = prop.floor_offset {
                    object.insert("floorOffsetApplied".into(), json!(offset));
                }
                let mut prop_notes: Vec<String> = Vec::new();
                let kind_note = match prop.kind {
                    Some(kind) => Some(kind_note(kind)),
                    None => result_kind(prop, &fgd)
                        .as_str()
                        .and_then(kind_of)
                        .map(kind_note),
                };
                if let Some(note) = kind_note {
                    prop_notes.push(note.to_string());
                }
                if let Some(reference) = &prop.model_ref {
                    if !reference.compiled {
                        prop_notes.push(format!(
                            "the model {} has no compiled .vmdl_c (allowUncompiled): Hammer \
                             does not draw it until the addon compiles and the document reloads",
                            reference.canonical
                        ));
                    }
                }
                if !entity_notes.is_empty() {
                    let mut merged = object
                        .get("warnings")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    merged.extend(entity_notes.messages.iter().map(|m| json!(m)));
                    object.insert("warnings".into(), Value::Array(merged));
                    object.insert("unknownKeys".into(), json!(entity_notes.unknown_keys));
                    object.insert("typeIssues".into(), json!(entity_notes.type_issues));
                }
                object.insert("notes".into(), json!(prop_notes));
            }
            item
        })
        .collect();

    let mut out = Map::new();
    out.insert("count".into(), json!(items.len()));
    out.insert("requested".into(), json!(ops.len()));
    out.insert("created".into(), Value::Array(items));
    // the bridge's own group label for a create batch (m2.15 §7), like create_entities
    out.insert("undoLabel".into(), json!("MCP: batch"));
    out.insert("requestedUndoLabel".into(), json!("MCP: create props"));
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("saved".into(), json!(false));
    out.insert("fgd".into(), fgd.json());
    out.insert("warnings".into(), json!(overall_warnings));
    out.insert("notes".into(), Value::Array(overall_notes));
    Ok(Value::Object(out).into())
}
