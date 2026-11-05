//! The `model_edit` op set: `add_node`, `remove_node`, `move_node`, `set_attr`, `remove_attr`.
//!
//! [`Op`] is the wire shape the tool accepts (`serde`, internally tagged on `"op"`); the op
//! functions below run against a [`cs2_kv3::Object`] tree. [`crate::Model::apply`] clones the
//! document, applies the ops to the clone through [`apply_ops`] and swaps it in only when every op
//! succeeded — so a rejected op never changes the model.
//!
//! A node supplied by `add_node` is built from JSON with Valve's key order: `_class`, `name`, the
//! other attributes (in the JSON's order, which is `serde_json`'s alphabetical one), `children`
//! last. Nested `children` entries are ordered the same way and must be objects with a `_class`.
//!
//! An op failure is always a [`VmdlError::BadOp`] carrying the op's index in the `ops` array and a
//! message that names the path involved (`op[2]: set_attr: no node at /0/3`); the structural read
//! errors (`NodeNotFound`, …) are what [`crate::Model::node`] and [`crate::Model::node_json`]
//! return.

use crate::vmdl::{
    children_of, is_meta_key, node_at, node_at_mut, root_object, root_object_mut, NodePath,
    VmdlError,
};
use cs2_kv3::{Object, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One edit op (`model_edit.ops[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Insert `node` into `parent` (a path) at `index` (default: append).
    AddNode {
        parent: String,
        #[serde(default)]
        index: Option<usize>,
        node: Json,
    },
    /// Delete the node at `path` (`"/"` is refused).
    RemoveNode { path: String },
    /// Move the node at `path` into `parent` at `index` (default: append). `index` counts the
    /// destination's children *after* the node was taken out.
    MoveNode {
        path: String,
        parent: String,
        #[serde(default)]
        index: Option<usize>,
    },
    /// Set `key` on the node at `path` (creating it before `children` when it is new).
    SetAttr { path: String, key: String, value: Json },
    /// Delete `key` from the node at `path` (it must exist).
    RemoveAttr { path: String, key: String },
}

impl Op {
    /// The wire name (`"add_node"`, …).
    pub fn name(&self) -> &'static str {
        match self {
            Op::AddNode { .. } => "add_node",
            Op::RemoveNode { .. } => "remove_node",
            Op::MoveNode { .. } => "move_node",
            Op::SetAttr { .. } => "set_attr",
            Op::RemoveAttr { .. } => "remove_attr",
        }
    }
}

/// What one applied op touched: the op's name and the path of the node it touched (for
/// `remove_node`, the path the node had).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpResult {
    pub op: String,
    pub path: String,
}

/// Apply `ops` in order to `doc`. On the first failure `doc` is left partially edited: callers go
/// through [`crate::Model::apply`], which runs this against a clone.
pub(crate) fn apply_ops(
    doc: &mut cs2_kv3::Document,
    ops: &[Op],
) -> Result<Vec<OpResult>, VmdlError> {
    let mut results = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        results.push(apply_op(doc, op, index)?);
    }
    Ok(results)
}

fn apply_op(
    doc: &mut cs2_kv3::Document,
    op: &Op,
    index: usize,
) -> Result<OpResult, VmdlError> {
    match op {
        Op::AddNode {
            parent,
            index: at,
            node,
        } => add_node(doc, parent, *at, node, index),
        Op::RemoveNode { path } => remove_node(doc, path, index),
        Op::MoveNode { path, parent, index: at } => move_node(doc, path, parent, *at, index),
        Op::SetAttr { path, key, value } => set_attr(doc, path, key, value, index),
        Op::RemoveAttr { path, key } => remove_attr(doc, path, key, index),
    }
}

fn bad(index: usize, msg: impl Into<String>) -> VmdlError {
    VmdlError::BadOp {
        index,
        msg: msg.into(),
    }
}

fn parse_path(text: &str, index: usize) -> Result<NodePath, VmdlError> {
    text.parse::<NodePath>().map_err(|error| bad(index, error.to_string()))
}

fn node_mut_for<'a>(
    doc: &'a mut cs2_kv3::Document,
    path: &NodePath,
    index: usize,
    op: &str,
) -> Result<&'a mut Object, VmdlError> {
    let root = root_object_mut(doc).map_err(|error| bad(index, format!("{op}: {error}")))?;
    node_at_mut(root, path).map_err(|error| bad(index, format!("{op}: {error}")))
}

fn ensure_children(node: &mut Object) -> Result<&mut Vec<Value>, String> {
    if node.get("children").is_none() {
        node.insert("children".to_string(), Value::Array(Vec::new()));
    }
    match node.get_mut("children") {
        Some(Value::Array(children)) => Ok(children),
        Some(_) => Err("children is not an array".to_string()),
        None => Err("could not create a children array".to_string()),
    }
}

fn add_node(
    doc: &mut cs2_kv3::Document,
    parent: &str,
    at: Option<usize>,
    node: &Json,
    index: usize,
) -> Result<OpResult, VmdlError> {
    let parent_path = parse_path(parent, index)?;
    let new_node = node_object_from_json(node)
        .map_err(|msg| bad(index, format!("add_node: {msg}")))?;
    let parent_node = node_mut_for(doc, &parent_path, index, "add_node")?;
    let children = ensure_children(parent_node)
        .map_err(|msg| bad(index, format!("add_node: {}: {msg}", parent_path)))?;
    let at = at.unwrap_or(children.len());
    if at > children.len() {
        return Err(bad(
            index,
            format!(
                "add_node: index {at} is past the end of {parent_path} ({} children)",
                children.len()
            ),
        ));
    }
    children.insert(at, Value::Object(new_node));
    Ok(OpResult {
        op: "add_node".to_string(),
        path: parent_path.child(at).to_string(),
    })
}

fn remove_node(
    doc: &mut cs2_kv3::Document,
    path: &str,
    index: usize,
) -> Result<OpResult, VmdlError> {
    let target = parse_path(path, index)?;
    let Some(last) = target.last() else {
        return Err(bad(index, "remove_node: the root node (/) cannot be removed"));
    };
    let parent_path = target.parent().unwrap_or_default();
    let parent = node_mut_for(doc, &parent_path, index, "remove_node")?;
    let children = ensure_children(parent)
        .map_err(|msg| bad(index, format!("remove_node: {}: {msg}", parent_path)))?;
    if last >= children.len() {
        return Err(bad(index, format!("remove_node: no node at {target}")));
    }
    children.remove(last);
    Ok(OpResult {
        op: "remove_node".to_string(),
        path: target.to_string(),
    })
}

fn move_node(
    doc: &mut cs2_kv3::Document,
    path: &str,
    parent: &str,
    at: Option<usize>,
    index: usize,
) -> Result<OpResult, VmdlError> {
    let source = parse_path(path, index)?;
    let dest = parse_path(parent, index)?;
    let Some(source_index) = source.last() else {
        return Err(bad(index, "move_node: the root node (/) cannot be moved"));
    };
    if dest.starts_with(&source) {
        return Err(bad(
            index,
            format!("move_node: cannot move {source} into itself ({dest})"),
        ));
    }
    let source_parent_path = source.parent().unwrap_or_default();
    let same_list = source_parent_path == dest;

    // Bounds are checked before anything is taken out, so a rejected move never leaves a hole.
    let source_len = node_at(root_object(doc)?, &source_parent_path)
        .map(|node| children_of(node).len())
        .map_err(|error| bad(index, format!("move_node: {error}")))?;
    if source_index >= source_len {
        return Err(bad(index, format!("move_node: no node at {source}")));
    }
    let dest_len = node_at(root_object(doc)?, &dest)
        .map(|node| children_of(node).len())
        .map_err(|error| bad(index, format!("move_node: {error}")))?;
    let at = at.unwrap_or(dest_len - usize::from(same_list));
    if at > dest_len - usize::from(same_list) {
        return Err(bad(
            index,
            format!("move_node: index {at} is past the end of {dest} ({dest_len} children)"),
        ));
    }

    let source_parent = node_mut_for(doc, &source_parent_path, index, "move_node")?;
    let moved = ensure_children(source_parent)
        .map_err(|msg| bad(index, format!("move_node: {}: {msg}", source_parent_path)))?
        .remove(source_index);
    let dest_node = node_mut_for(doc, &dest, index, "move_node")?;
    let dest_children = ensure_children(dest_node)
        .map_err(|msg| bad(index, format!("move_node: {dest}: {msg}")))?;
    dest_children.insert(at, moved);
    Ok(OpResult {
        op: "move_node".to_string(),
        path: dest.child(at).to_string(),
    })
}

fn set_attr(
    doc: &mut cs2_kv3::Document,
    path: &str,
    key: &str,
    value: &Json,
    index: usize,
) -> Result<OpResult, VmdlError> {
    if is_meta_key(key) {
        return Err(bad(
            index,
            format!("set_attr: {key:?} is structural; use the node ops instead"),
        ));
    }
    let target = parse_path(path, index)?;
    let value = cs2_kv3::from_json(value)
        .map_err(|error| bad(index, format!("set_attr: {key:?}: {error}")))?;
    let node = node_mut_for(doc, &target, index, "set_attr")?;
    insert_attr(node, key, value);
    Ok(OpResult {
        op: "set_attr".to_string(),
        path: target.to_string(),
    })
}

fn remove_attr(
    doc: &mut cs2_kv3::Document,
    path: &str,
    key: &str,
    index: usize,
) -> Result<OpResult, VmdlError> {
    if is_meta_key(key) {
        return Err(bad(
            index,
            format!("remove_attr: {key:?} is structural; use the node ops instead"),
        ));
    }
    let target = parse_path(path, index)?;
    let node = node_mut_for(doc, &target, index, "remove_attr")?;
    if node.remove(key).is_none() {
        return Err(bad(
            index,
            format!("remove_attr: {target} has no attribute {key:?}"),
        ));
    }
    Ok(OpResult {
        op: "remove_attr".to_string(),
        path: target.to_string(),
    })
}

/// Insert `key`: an existing key keeps its position, a new one goes before `children` when the
/// node has a `children` key (and at the end otherwise).
fn insert_attr(node: &mut Object, key: &str, value: Value) {
    if node.get(key).is_some() {
        node.insert(key.to_string(), value);
        return;
    }
    match node.remove("children") {
        Some(children) => {
            node.insert(key.to_string(), value);
            node.insert("children".to_string(), children);
        }
        None => {
            node.insert(key.to_string(), value);
        }
    }
}

/// A node object from JSON, in Valve's key order: `_class`, `name`, other attributes, `children`.
/// `children` entries must be node objects too (they are ordered the same way).
fn node_object_from_json(json: &Json) -> Result<Object, String> {
    let map = json
        .as_object()
        .ok_or_else(|| "node must be an object".to_string())?;
    let class = map
        .get("_class")
        .and_then(Json::as_str)
        .ok_or_else(|| "node needs a string _class".to_string())?;
    let mut node = Object::new();
    node.insert("_class".to_string(), Value::String(class.to_string()));
    if let Some(name) = map.get("name") {
        let name = name
            .as_str()
            .ok_or_else(|| "node name must be a string".to_string())?;
        node.insert("name".to_string(), Value::String(name.to_string()));
    }
    for (key, value) in map {
        if key == "_class" || is_meta_key(key) {
            continue;
        }
        node.insert(
            key.clone(),
            cs2_kv3::from_json(value).map_err(|error| format!("attribute {key:?}: {error}"))?,
        );
    }
    if let Some(children) = map.get("children") {
        let children = children
            .as_array()
            .ok_or_else(|| "children must be an array".to_string())?;
        let mut list = Vec::with_capacity(children.len());
        for (at, child) in children.iter().enumerate() {
            list.push(Value::Object(
                node_object_from_json(child).map_err(|msg| format!("children[{at}]: {msg}"))?,
            ));
        }
        node.insert("children".to_string(), Value::Array(list));
    }
    Ok(node)
}
