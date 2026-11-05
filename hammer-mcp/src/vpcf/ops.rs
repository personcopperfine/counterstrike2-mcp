//! The `particle_edit` op set: `add_function`, `remove_function`, `move_function`, `set_field`,
//! `remove_field`, `set_root_field`, `remove_root_field`, `add_child`, `remove_child`,
//! `add_variable`, `remove_variable`.
//!
//! [`Op`] is the wire shape the tool accepts (`serde`, internally tagged on `"op"`); the op
//! functions below run against a [`cs2_kv3::Document`]. [`crate::vpcf::Particle::apply`] clones
//! the document, applies the ops to the clone through [`apply_ops`] and swaps it in only when
//! every op succeeded — so a rejected op never changes the particle.
//!
//! An entry supplied by `add_function` is built from JSON with Valve's key order: `_class` first
//! (the other keys then follow in the JSON's order, which is `serde_json`'s alphabetical one —
//! the same limitation as the ModelDoc layer), everything else last.
//!
//! `m_Children` and `m_NamedValueLocals` (the structural arrays) are reached through the same
//! ops: `add_child`/`remove_child` are `add_function`/`remove_function` on `m_Children`, and
//! `add_variable`/`remove_variable` are their counterparts on `m_NamedValueLocals`, with the
//! entry built by [`crate::vpcf::namedvalue::build_entry`] (pet's own entry form) rather
//! than from the caller's JSON. A `_class` is optional there (absent in pet's files).
//!
//! An op failure is always a [`VpcfError::BadOp`] carrying the op's index in the `ops` array and a
//! message naming the array involved (`op[2]: add_function: "m_Bogus" is not a function array`).

use crate::vpcf::namedvalue;
use crate::vpcf::{array_key, array_type, array_keys, FunctionRef, VpcfError};
use cs2_kv3::{Object, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One edit op (`particle_edit.ops[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Insert `function` into `array` (a function-array key) at `index` (default: append).
    /// `function` must be an object; `_class` must be a string for the function arrays (0..=6).
    AddFunction {
        array: String,
        #[serde(default)]
        index: Option<usize>,
        function: Json,
    },
    /// Delete `array[index]`.
    RemoveFunction { array: String, index: usize },
    /// Move `array[index]` to `to_index` of `to_array` (default: the same array). `to_index`
    /// counts the destination's entries *after* the entry was taken out.
    MoveFunction {
        array: String,
        index: usize,
        to_index: usize,
        #[serde(default)]
        to_array: Option<String>,
    },
    /// Set `key` on `array[index]` (an existing key keeps its position, a new one is appended).
    SetField {
        array: String,
        index: usize,
        key: String,
        value: Json,
    },
    /// Delete `key` from `array[index]` (it must exist).
    RemoveField {
        array: String,
        index: usize,
        key: String,
    },
    /// Set a root field (`key` may not be `_class` or one of the nine array keys).
    SetRootField { key: String, value: Json },
    /// Delete a root field (same refusals; the key must exist).
    RemoveRootField { key: String },
    /// `AddFunction` on `m_Children` (`index` default: append).
    AddChild {
        #[serde(default)]
        index: Option<usize>,
        child: Json,
    },
    /// `RemoveFunction` on `m_Children`.
    RemoveChild { index: usize },
    /// Insert one `m_NamedValueLocals` entry (bridge type 8) built by
    /// [`namedvalue::build_entry`]: `name`, `value_type` (`"PVAL_FLOAT"`, …), `is_public`
    /// (default `true`, which pet omits from the file) and the entry's `m_ConfigValue`
    /// (`default_value`, default [`namedvalue::CONFIG_VALUE_UNSET`]). `index` default: append.
    AddVariable {
        name: String,
        value_type: String,
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        is_public: Option<bool>,
        #[serde(default)]
        default_value: Option<Json>,
    },
    /// Delete `m_NamedValueLocals[index]`.
    RemoveVariable { index: usize },
}

impl Op {
    /// The wire name (`"add_function"`, …).
    pub fn name(&self) -> &'static str {
        match self {
            Op::AddFunction { .. } => "add_function",
            Op::RemoveFunction { .. } => "remove_function",
            Op::MoveFunction { .. } => "move_function",
            Op::SetField { .. } => "set_field",
            Op::RemoveField { .. } => "remove_field",
            Op::SetRootField { .. } => "set_root_field",
            Op::RemoveRootField { .. } => "remove_root_field",
            Op::AddChild { .. } => "add_child",
            Op::RemoveChild { .. } => "remove_child",
            Op::AddVariable { .. } => "add_variable",
            Op::RemoveVariable { .. } => "remove_variable",
        }
    }
}

/// What one applied op touched: the op's name and the reference it touched (`"/"` for a root
/// field, `"m_Operators/2"` for an entry, for `remove_*` the reference the entry *had*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpResult {
    pub op: String,
    pub path: String,
}

/// The array `m_Children`, which [`Op::AddChild`] / [`Op::RemoveChild`] are shorthand for.
const CHILDREN: &str = "m_Children";

/// The array `m_NamedValueLocals` (bridge type 8), which [`Op::AddVariable`] /
/// [`Op::RemoveVariable`] address; the entry form itself lives in [`namedvalue`].
const NAMED_VALUES: &str = namedvalue::KEY;

/// Apply `ops` in order to `doc`. On the first failure `doc` is left partially edited: callers go
/// through [`crate::vpcf::Particle::apply`], which runs this against a clone.
pub(crate) fn apply_ops(
    doc: &mut cs2_kv3::Document,
    ops: &[Op],
) -> Result<Vec<OpResult>, VpcfError> {
    let mut results = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        results.push(apply_op(doc, op, index)?);
    }
    Ok(results)
}

fn apply_op(doc: &mut cs2_kv3::Document, op: &Op, index: usize) -> Result<OpResult, VpcfError> {
    match op {
        Op::AddFunction {
            array,
            index: at,
            function,
        } => add_function(doc, array, *at, function, index, "add_function"),
        Op::RemoveFunction { array, index: at } => {
            remove_function(doc, array, *at, index, "remove_function")
        }
        Op::MoveFunction {
            array,
            index: from,
            to_index,
            to_array,
        } => move_function(
            doc,
            array,
            *from,
            to_array.as_deref().unwrap_or(array),
            *to_index,
            index,
        ),
        Op::SetField {
            array,
            index: at,
            key,
            value,
        } => set_field(doc, array, *at, key, value, index),
        Op::RemoveField {
            array,
            index: at,
            key,
        } => remove_field(doc, array, *at, key, index),
        Op::SetRootField { key, value } => set_root_field(doc, key, value, index),
        Op::RemoveRootField { key } => remove_root_field(doc, key, index),
        Op::AddChild { index: at, child } => {
            add_function(doc, CHILDREN, *at, child, index, "add_child")
        }
        Op::RemoveChild { index: at } => {
            remove_function(doc, CHILDREN, *at, index, "remove_child")
        }
        Op::AddVariable {
            name,
            value_type,
            index: at,
            is_public,
            default_value,
        } => add_variable(
            doc,
            name,
            value_type,
            *at,
            is_public.unwrap_or(true),
            default_value.as_ref(),
            index,
        ),
        Op::RemoveVariable { index: at } => {
            remove_function(doc, NAMED_VALUES, *at, index, "remove_variable")
        }
    }
}

fn bad(index: usize, msg: impl Into<String>) -> VpcfError {
    VpcfError::BadOp {
        index,
        msg: msg.into(),
    }
}

/// The bridge type index of `array`, or a [`VpcfError::BadOp`] naming the nine keys.
fn resolve_array(array: &str, index: usize, op: &str) -> Result<usize, VpcfError> {
    array_type(array).ok_or_else(|| {
        bad(
            index,
            format!(
                "{op}: {array:?} is not a function array (expected one of: {})",
                array_keys().join(", ")
            ),
        )
    })
}

/// The array at `key`, created empty when the root does not have it yet (so a minimal file can
/// gain `m_Emitters`), or a [`VpcfError::BadOp`] when the key is present but not an array.
fn array_mut<'a>(
    doc: &'a mut cs2_kv3::Document,
    key: &str,
    index: usize,
    op: &str,
) -> Result<&'a mut Vec<Value>, VpcfError> {
    if doc.root.get(key).is_none() {
        doc.root.insert(key.to_string(), Value::Array(Vec::new()));
    }
    match doc.root.get_mut(key) {
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(bad(index, format!("{op}: {key} is not an array"))),
        None => Err(bad(index, format!("{op}: could not create {key}"))),
    }
}

/// An entry object from JSON, in Valve's key order: `_class` first (when there is one), the other
/// keys after it.
fn function_object_from_json(
    function: &Json,
    r#type: usize,
    index: usize,
    op: &str,
) -> Result<Object, VpcfError> {
    let map = function
        .as_object()
        .ok_or_else(|| bad(index, format!("{op}: function must be an object")))?;
    let mut object = Object::new();
    match map.get("_class") {
        Some(Json::String(class)) => {
            object.insert("_class".to_string(), Value::String(class.clone()));
        }
        Some(other) => {
            return Err(bad(
                index,
                format!("{op}: _class must be a string, got {other}"),
            ))
        }
        None if r#type <= 6 => {
            return Err(bad(
                index,
                format!(
                    "{op}: {} entries need a string _class",
                    array_key(r#type).unwrap_or("function")
                ),
            ))
        }
        None => {}
    }
    for (key, value) in map {
        if key == "_class" {
            continue;
        }
        object.insert(
            key.clone(),
            cs2_kv3::from_json(value).map_err(|error| bad(index, format!("{op}: field {key:?}: {error}")))?,
        );
    }
    Ok(object)
}

fn add_function(
    doc: &mut cs2_kv3::Document,
    array: &str,
    at: Option<usize>,
    function: &Json,
    index: usize,
    op: &str,
) -> Result<OpResult, VpcfError> {
    let r#type = resolve_array(array, index, op)?;
    let entry = function_object_from_json(function, r#type, index, op)?;
    insert_entry(doc, array, at, entry, index, op)
}

/// Insert an already-built `entry` into `array` at `at` (default: append) — the half
/// [`add_function`] and [`add_variable`] share.
fn insert_entry(
    doc: &mut cs2_kv3::Document,
    array: &str,
    at: Option<usize>,
    entry: Object,
    index: usize,
    op: &str,
) -> Result<OpResult, VpcfError> {
    let entries = array_mut(doc, array, index, op)?;
    let at = at.unwrap_or(entries.len());
    if at > entries.len() {
        return Err(bad(
            index,
            format!(
                "{op}: index {at} is past the end of {array} ({} entries)",
                entries.len()
            ),
        ));
    }
    entries.insert(at, Value::Object(entry));
    Ok(OpResult {
        op: op.to_string(),
        path: FunctionRef::new(array, at).to_string(),
    })
}

/// `Op::AddVariable`: one [`namedvalue::build_entry`] entry in `m_NamedValueLocals`.
///
/// `is_public` is already defaulted by the caller. `default_value` is the entry's `m_ConfigValue`;
/// a JSON value with no KV3 spelling is refused here (`build_entry` would have to drop it), which
/// keeps the op all-or-nothing for the same reasons `set_field` is.
fn add_variable(
    doc: &mut cs2_kv3::Document,
    name: &str,
    value_type: &str,
    at: Option<usize>,
    is_public: bool,
    default_value: Option<&Json>,
    index: usize,
) -> Result<OpResult, VpcfError> {
    resolve_array(NAMED_VALUES, index, "add_variable")?;
    if let Some(value) = default_value {
        cs2_kv3::from_json(value)
            .map_err(|error| bad(index, format!("add_variable: default_value: {error}")))?;
    }
    let entry = namedvalue::build_entry(name, value_type, is_public, default_value.cloned());
    insert_entry(doc, NAMED_VALUES, at, entry, index, "add_variable")
}

fn remove_function(
    doc: &mut cs2_kv3::Document,
    array: &str,
    at: usize,
    index: usize,
    op: &str,
) -> Result<OpResult, VpcfError> {
    resolve_array(array, index, op)?;
    let entries = array_mut(doc, array, index, op)?;
    if at >= entries.len() {
        return Err(bad(
            index,
            format!("{op}: no entry at {}", FunctionRef::new(array, at)),
        ));
    }
    entries.remove(at);
    Ok(OpResult {
        op: op.to_string(),
        path: FunctionRef::new(array, at).to_string(),
    })
}

fn move_function(
    doc: &mut cs2_kv3::Document,
    array: &str,
    from: usize,
    to_array: &str,
    to_index: usize,
    index: usize,
) -> Result<OpResult, VpcfError> {
    resolve_array(array, index, "move_function")?;
    resolve_array(to_array, index, "move_function")?;
    let same = to_array == array;
    let source_len = doc
        .root
        .get(array)
        .and_then(Value::as_array)
        .map(<[Value]>::len)
        .unwrap_or(0);
    if from >= source_len {
        return Err(bad(
            index,
            format!(
                "move_function: no entry at {}",
                FunctionRef::new(array, from)
            ),
        ));
    }
    let dest_len = doc
        .root
        .get(to_array)
        .and_then(Value::as_array)
        .map(<[Value]>::len)
        .unwrap_or(0);
    // Bounds are checked before anything is taken out, so a rejected move never leaves a hole.
    let limit = dest_len - usize::from(same);
    if to_index > limit {
        return Err(bad(
            index,
            format!(
                "move_function: index {to_index} is past the end of {to_array} ({dest_len} entries)"
            ),
        ));
    }
    let moved = {
        let source = array_mut(doc, array, index, "move_function")?;
        source.remove(from)
    };
    let dest = array_mut(doc, to_array, index, "move_function")?;
    dest.insert(to_index, moved);
    Ok(OpResult {
        op: "move_function".to_string(),
        path: FunctionRef::new(to_array, to_index).to_string(),
    })
}

/// The entry at `array[index]`, or a [`VpcfError::BadOp`].
fn entry_mut<'a>(
    doc: &'a mut cs2_kv3::Document,
    array: &str,
    at: usize,
    index: usize,
    op: &str,
) -> Result<&'a mut Object, VpcfError> {
    resolve_array(array, index, op)?;
    let reference = FunctionRef::new(array, at);
    doc.root
        .get_mut(array)
        .and_then(Value::as_array_mut)
        .and_then(|entries| entries.get_mut(at))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| bad(index, format!("{op}: no entry at {reference}")))
}

fn set_field(
    doc: &mut cs2_kv3::Document,
    array: &str,
    at: usize,
    key: &str,
    value: &Json,
    index: usize,
) -> Result<OpResult, VpcfError> {
    let value = cs2_kv3::from_json(value)
        .map_err(|error| bad(index, format!("set_field: {key:?}: {error}")))?;
    let entry = entry_mut(doc, array, at, index, "set_field")?;
    // An existing key keeps its position (so `_class` stays first), a new one is appended.
    entry.insert(key.to_string(), value);
    Ok(OpResult {
        op: "set_field".to_string(),
        path: FunctionRef::new(array, at).to_string(),
    })
}

fn remove_field(
    doc: &mut cs2_kv3::Document,
    array: &str,
    at: usize,
    key: &str,
    index: usize,
) -> Result<OpResult, VpcfError> {
    let reference = FunctionRef::new(array, at);
    let entry = entry_mut(doc, array, at, index, "remove_field")?;
    if entry.remove(key).is_none() {
        return Err(bad(
            index,
            format!("remove_field: {reference} has no field {key:?}"),
        ));
    }
    Ok(OpResult {
        op: "remove_field".to_string(),
        path: reference.to_string(),
    })
}

/// `_class` and the nine array keys are structure, not root fields.
fn refuse_structural_root_key(key: &str, index: usize, op: &str) -> Result<(), VpcfError> {
    if key == "_class" {
        return Err(bad(
            index,
            format!("{op}: \"_class\" is structural; it always names the root class"),
        ));
    }
    if array_type(key).is_some() {
        return Err(bad(
            index,
            format!(
                "{op}: {key:?} is a function array; use the function/child ops on it"
            ),
        ));
    }
    Ok(())
}

fn set_root_field(
    doc: &mut cs2_kv3::Document,
    key: &str,
    value: &Json,
    index: usize,
) -> Result<OpResult, VpcfError> {
    refuse_structural_root_key(key, index, "set_root_field")?;
    let value = cs2_kv3::from_json(value)
        .map_err(|error| bad(index, format!("set_root_field: {key:?}: {error}")))?;
    // An existing key keeps its position; a new one is appended (the arrays follow it).
    doc.root.insert(key.to_string(), value);
    Ok(OpResult {
        op: "set_root_field".to_string(),
        path: "/".to_string(),
    })
}

fn remove_root_field(
    doc: &mut cs2_kv3::Document,
    key: &str,
    index: usize,
) -> Result<OpResult, VpcfError> {
    refuse_structural_root_key(key, index, "remove_root_field")?;
    if doc.root.remove(key).is_none() {
        return Err(bad(
            index,
            format!("remove_root_field: the root has no field {key:?}"),
        ));
    }
    Ok(OpResult {
        op: "remove_root_field".to_string(),
        path: "/".to_string(),
    })
}
