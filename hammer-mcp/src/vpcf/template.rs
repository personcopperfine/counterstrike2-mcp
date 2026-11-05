//! Starter `.vpcf` templates for `particle_create`.
//!
//! One kind this pass: `minimal` — a `CParticleSystemDefinition` with one emitter, one
//! initializer, two operators and one renderer, which is the smallest file the live step proved
//! compiles (`resourcecompiler.exe`: `OK: 1 compiled`) and that `pet` opens. Its parameters:
//!
//! | parameter | type | default |
//! |---|---|---|
//! | `maxParticles` | int | `32` |
//! | `renderer` | string | `"C_OP_RenderSprites"` |
//!
//! `m_nBehaviorVersion` is not a parameter: it is always
//! [`CURRENT_BEHAVIOR_VERSION`](crate::vpcf::CURRENT_BEHAVIOR_VERSION).
//!
//! With the defaults, [`Particle::write`](crate::vpcf::Particle::write) is
//! `fixtures/vpcf/probe.vpcf` with exactly one line added: the `m_nBehaviorVersion` root field =
//! [`CURRENT_BEHAVIOR_VERSION`](crate::vpcf::CURRENT_BEHAVIOR_VERSION), inserted directly after
//! `_class` (fact A: pet shows its "old behavior version" banner for a file without the field;
//! the probe fixture itself is the live-compiled reference and predates the field), checked by
//! `tests/vpcf.rs`. The entry `_class` names are the only
//! class names this module emits and are listed in [`TEMPLATE_CLASSES`]; the live snapshot is what
//! confirms them, so `particle_create` reports them as unverified until the schema holds them.

use crate::vpcf::{
    BEHAVIOR_VERSION_FIELD, CURRENT_BEHAVIOR_VERSION, Issue, Level, Particle, ROOT_CLASS, VpcfError,
};
use cs2_kv3::{Document, Header, Number, Object, Value};
use serde_json::{Map, Value as Json};

/// Every `_class` [`template`] can emit. A test checks it against the schema snapshot when one
/// (non-placeholder) is loadable.
pub const TEMPLATE_CLASSES: &[&str] = &[
    ROOT_CLASS,
    "C_OP_ContinuousEmitter",
    "C_INIT_CreateWithinSphereTransform",
    "C_OP_BasicMovement",
    "C_OP_Decay",
    "C_OP_RenderSprites",
];

/// Build a starter particle system. `kind` is `"minimal"`; `params` is the tool's `params`
/// object (see the module docs).
pub fn template(kind: &str, params: &Json) -> Result<Particle, VpcfError> {
    match kind {
        "minimal" => minimal(params),
        other => Err(template_issue(format!(
            "unknown template {other:?}: expected \"minimal\""
        ))),
    }
}

fn minimal(params: &Json) -> Result<Particle, VpcfError> {
    let map = params
        .as_object()
        .ok_or_else(|| template_issue("params must be an object"))?;
    let max_particles = integer(map, "maxParticles")?.unwrap_or(32);
    let renderer = optional_string(map, "renderer")?.unwrap_or_else(|| "C_OP_RenderSprites".to_string());

    let header = Header::parse(crate::vpcf::HEADER).map_err(VpcfError::Parse)?;
    let mut root = Object::new();
    root.insert("_class".to_string(), Value::String(ROOT_CLASS.to_string()));
    // Fact A: pet shows the "old behavior version" banner when the root has no
    // `m_nBehaviorVersion` (a missing field reads as 0). Written directly after `_class`.
    root.insert(
        BEHAVIOR_VERSION_FIELD.to_string(),
        Value::Number(Number::from_i64(CURRENT_BEHAVIOR_VERSION)),
    );
    root.insert(
        "m_nMaxParticles".to_string(),
        Value::Number(Number::from_i64(max_particles)),
    );
    root.insert(
        "m_Emitters".to_string(),
        array(vec![function("C_OP_ContinuousEmitter")]),
    );
    root.insert(
        "m_Initializers".to_string(),
        array(vec![function("C_INIT_CreateWithinSphereTransform")]),
    );
    root.insert(
        "m_Operators".to_string(),
        array(vec![function("C_OP_BasicMovement"), function("C_OP_Decay")]),
    );
    root.insert(
        "m_Renderers".to_string(),
        array(vec![function(&renderer)]),
    );

    let mut doc = Document::new(header);
    doc.root = root;
    Ok(Particle { doc })
}

/// `{"_class": class}` — one entry.
fn function(class: &str) -> Value {
    let mut node = Object::new();
    node.insert("_class".to_string(), Value::String(class.to_string()));
    Value::Object(node)
}

fn array(entries: Vec<Value>) -> Value {
    Value::Array(entries)
}

fn optional_string(map: &Map<String, Json>, key: &str) -> Result<Option<String>, VpcfError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(other) => Err(template_issue(format!(
            "params.{key} must be a non-empty string, got {other}"
        ))),
    }
}

fn integer(map: &Map<String, Json>, key: &str) -> Result<Option<i64>, VpcfError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(number)) => match number.as_i64() {
            Some(value) if value >= 0 => Ok(Some(value)),
            _ => Err(template_issue(format!(
                "params.{key} must be a non-negative integer, got {number}"
            ))),
        },
        Some(other) => Err(template_issue(format!(
            "params.{key} must be a non-negative integer, got {other}"
        ))),
    }
}

fn template_issue(msg: impl Into<String>) -> VpcfError {
    VpcfError::Invalid(vec![Issue::new(Level::Error, "/", msg)])
}
