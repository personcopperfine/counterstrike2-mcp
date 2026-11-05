//! ModelDoc `.vmdl` model layer: a node tree over [`cs2_kv3::Document`] with addressing, edit ops,
//! validation, templates, content-path rules and safe file writes.
//!
//! A `.vmdl` is KV3 text (`cs2-kv3` parses and writes it byte-identically for Valve-authored
//! files). This module adds the *model* semantics on top — the tree of `_class` nodes every
//! ModelDoc file has:
//!
//! ```text
//! <!-- kv3 … format:modeldoc36 … -->
//! {
//!     rootNode =
//!     {
//!         _class = "RootNode"
//!         children = [ { _class = "RenderMeshList" children = [ … ] }, … ]
//!     }
//! }
//! ```
//!
//! * [`NodePath`] addresses nodes: `"/"` is `rootNode`, `"/0/2"` is
//!   `rootNode.children[0].children[2]`.
//! * [`Model`] parses + structurally validates, lists nodes ([`Model::list`]), renders a subtree as
//!   JSON ([`Model::node_json`], values converted with [`cs2_kv3::to_json`]) and applies edit ops
//!   ([`Model::apply`], atomic: all-or-nothing).
//! * [`template`] builds a fresh starter model; [`ContentPaths`] resolves the three path forms the
//!   tools accept and derives the container/compile paths; [`fsutil::write_atomic_with_backup`]
//!   is the only file writer.
//!
//! Conventions that match Valve's own files (verified against the shipped samples):
//!
//! * a node's `_class` comes first, `name` next, other attributes after, `children` last;
//! * `children` holds node objects only, never strings;
//! * [`Model::apply`]'s `set_attr` keeps an existing key's position; a *new* key is inserted
//!   before `children` when the node has one (the key order inside a KV3 object is cosmetic, the
//!   writer follows it);
//! * number lexemes survive (`0.0` stays `0.0`); JSON values are converted with
//!   [`cs2_kv3::from_json`] / [`cs2_kv3::to_json`], so a flagged `resource_name:"x"` is
//!   `{"$flag":"resource_name","value":"x"}` and a `#[ … ]` blob is `{"$blob":"<hex>"}`.
//!
//! Known limitations (documented, not accidental):
//!
//! * object key order is *not* preserved by a JSON round trip: `serde_json` maps are alphabetical,
//!   so an object inserted from JSON gets alphabetical attribute order — except for the
//!   `_class` / `name` / `children` placement above, which [`crate::vmdl::ops`] fixes up;
//! * sub-tree truncation (`depth`) reports a cut node with an empty `children` array; the real
//!   child count is always available from [`Model::list`];
//! * template class and key names are marked `[I]` in [`template`]'s docs: they come from
//!   ValveResourceFormat's `ModelExtract` (MIT) rather than from a live editor session.

pub mod fsutil;
mod ops;
mod paths;
mod template;

pub use ops::{Op, OpResult};
pub use paths::ContentPaths;
pub use template::{material_remap, template, TEMPLATE_CLASSES};

use cs2_kv3::{Object, Value};
use serde_json::{json, Map, Value as Json};
use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

/// Attribute keys that are structure, not data: they are never reported as a node's attributes and
/// [`Op::SetAttr`] / [`Op::RemoveAttr`] refuse them.
pub const META_KEYS: [&str; 3] = ["_class", "name", "children"];

/// True for `_class` / `name` / `children`.
pub fn is_meta_key(key: &str) -> bool {
    META_KEYS.contains(&key)
}

/// A node address: `[]` is `rootNode`, `[0, 2]` is `rootNode.children[0].children[2]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct NodePath(pub Vec<usize>);

impl NodePath {
    /// The root node (`"/"`).
    pub fn root() -> NodePath {
        NodePath(Vec::new())
    }

    /// `self` extended by one child index.
    pub fn child(&self, index: usize) -> NodePath {
        let mut indices = self.0.clone();
        indices.push(index);
        NodePath(indices)
    }

    /// The parent path, `None` for the root.
    pub fn parent(&self) -> Option<NodePath> {
        (!self.0.is_empty()).then(|| NodePath(self.0[..self.0.len() - 1].to_vec()))
    }

    /// The last index, `None` for the root.
    pub fn last(&self) -> Option<usize> {
        self.0.last().copied()
    }

    /// True when `self` is `other` or a node inside it.
    pub fn starts_with(&self, other: &NodePath) -> bool {
        self.0.starts_with(&other.0)
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("/");
        }
        for index in &self.0 {
            write!(f, "/{index}")?;
        }
        Ok(())
    }
}

impl FromStr for NodePath {
    type Err = VmdlError;

    /// `"/"`, `"/0/2"` (a leading `"/"` is optional; an empty string is the root).
    fn from_str(s: &str) -> Result<NodePath, VmdlError> {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return Ok(NodePath::root());
        }
        let body = trimmed.trim_start_matches('/');
        let mut indices = Vec::new();
        for part in body.split('/') {
            let index = part
                .parse::<usize>()
                .map_err(|_| VmdlError::Path(format!("bad node path {s:?}: {part:?} is not an index")))?;
            indices.push(index);
        }
        Ok(NodePath(indices))
    }
}

/// One node as [`Model::list`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// `"/"`, `"/0/1"`.
    pub path: String,
    /// `_class`.
    pub class: String,
    /// `name`, when the node has one.
    pub name: Option<String>,
    /// Number of `children` entries (0 when there is no `children` key).
    pub child_count: usize,
    /// Attribute keys, `_class` / `name` / `children` excluded, in file order.
    pub attr_keys: Vec<String>,
}

/// One structural problem [`Model::validate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Node path the problem is at (`"/"` for model-level problems).
    pub path: String,
    pub msg: String,
}

/// Everything the model layer can fail with.
#[derive(Debug)]
pub enum VmdlError {
    /// `cs2-kv3` could not parse the text.
    Parse(cs2_kv3::ParseError),
    /// [`Model::validate`] found structural problems (also used for bad [`template`] arguments,
    /// with `path` = `"/"`).
    Invalid(Vec<Issue>),
    /// There is no node at that path.
    NodeNotFound(String),
    /// An edit op was rejected. `index` is its position in the `ops` array.
    BadOp { index: usize, msg: String },
    /// A `_class` that is not in the node-types snapshot.
    UnknownClass { path: String, class: String },
    /// A file-system problem while reading or writing.
    Io(std::io::Error),
    /// A path argument that cannot be resolved (wrong shape, `..`, not mapped, outside the addon).
    Path(String),
}

impl fmt::Display for VmdlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmdlError::Parse(error) => write!(f, "parse error: {error}"),
            VmdlError::Invalid(issues) => match issues.first() {
                Some(first) => write!(
                    f,
                    "invalid model: {} issue(s), first at {}: {}",
                    issues.len(),
                    first.path,
                    first.msg
                ),
                None => f.write_str("invalid model"),
            },
            VmdlError::NodeNotFound(path) => write!(f, "no node at {path}"),
            VmdlError::BadOp { index, msg } => write!(f, "op[{index}]: {msg}"),
            VmdlError::UnknownClass { path, class } => {
                write!(f, "unknown _class {class:?} at {path}")
            }
            VmdlError::Io(error) => write!(f, "{error}"),
            VmdlError::Path(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for VmdlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VmdlError::Parse(error) => Some(error),
            VmdlError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<cs2_kv3::ParseError> for VmdlError {
    fn from(error: cs2_kv3::ParseError) -> Self {
        VmdlError::Parse(error)
    }
}

impl From<std::io::Error> for VmdlError {
    fn from(error: std::io::Error) -> Self {
        VmdlError::Io(error)
    }
}

/// A parsed and (structurally) validated `.vmdl`.
#[derive(Debug, Clone)]
pub struct Model {
    pub doc: cs2_kv3::Document,
}

impl Model {
    /// Parse `text` and check the structural rules ([`Model::validate`] with no class list).
    pub fn parse(text: &str) -> Result<Model, VmdlError> {
        let model = Model { doc: cs2_kv3::parse(text)? };
        let issues = model.validate(None);
        if issues.is_empty() {
            Ok(model)
        } else {
            Err(VmdlError::Invalid(issues))
        }
    }

    /// The file text: Valve's layout (CRLF, no final newline).
    pub fn write(&self) -> String {
        cs2_kv3::write(&self.doc)
    }

    /// The `rootNode` object. A model that failed to validate has no `rootNode`; this then returns
    /// an empty object so it cannot panic (use [`Model::node`] when the distinction matters).
    pub fn root(&self) -> &Object {
        match self.doc.root.get("rootNode").and_then(Value::as_object) {
            Some(root) => root,
            None => empty_object(),
        }
    }

    /// The node at `path`.
    pub fn node(&self, path: &NodePath) -> Result<&Object, VmdlError> {
        node_at(root_object(&self.doc)?, path)
    }

    /// The node at `path`, mutably.
    pub fn node_mut(&mut self, path: &NodePath) -> Result<&mut Object, VmdlError> {
        node_at_mut(root_object_mut(&mut self.doc)?, path)
    }

    /// Every node in pre-order, optionally limited to `depth` levels below the root and filtered
    /// by `_class` / `name`. Filtering never prunes the traversal: a matching node deeper in a
    /// non-matching subtree is still reported.
    pub fn list(&self, depth: Option<usize>, class: Option<&str>, name: Option<&str>) -> Vec<NodeInfo> {
        let mut walk = ListWalk { class, name, out: Vec::new() };
        if let Some(root) = self.doc.root.get("rootNode").and_then(Value::as_object) {
            walk.visit(&NodePath::root(), root, depth);
        }
        walk.out
    }

    /// The sub-tree at `path` as JSON:
    /// `{"path", "_class", "name", "attrs", "children"}` — `attrs` holds every attribute except
    /// `_class` / `name` / `children`, `children` the child nodes of the same shape (empty at the
    /// `depth` limit), `name` is `null` when the node has none.
    pub fn node_json(&self, path: &NodePath, depth: Option<usize>) -> Result<Json, VmdlError> {
        let node = self.node(path)?;
        Ok(node_to_json(path, node, depth))
    }

    /// Apply `ops` in order. **All-or-nothing**: the ops run against a clone of the document and
    /// the clone replaces it only when every op succeeded, so a failure leaves the model exactly
    /// as it was.
    pub fn apply(&mut self, ops: &[Op]) -> Result<Vec<OpResult>, VmdlError> {
        let mut working = self.doc.clone();
        let results = ops::apply_ops(&mut working, ops)?;
        self.doc = working;
        Ok(results)
    }

    /// Structural problems: a non-modeldoc header, a missing/odd `rootNode`, a node without a
    /// string `_class`, `children` that is not an array of objects, a non-string `name`, a
    /// `rootNode._class` that is not `"RootNode"`, and — when `known_classes` is given — every
    /// `_class` that is not in it.
    pub fn validate(&self, known_classes: Option<&[String]>) -> Vec<Issue> {
        let mut issues = Vec::new();
        if !self.doc.header.format.starts_with("modeldoc") {
            issues.push(Issue {
                path: "/".to_string(),
                msg: format!(
                    "header format {:?} is not a modeldoc format",
                    self.doc.header.format
                ),
            });
        }
        let root = match self.doc.root.get("rootNode") {
            Some(Value::Object(root)) => root,
            Some(_) => {
                issues.push(Issue {
                    path: "/".to_string(),
                    msg: "rootNode is not an object".to_string(),
                });
                return issues;
            }
            None => {
                issues.push(Issue {
                    path: "/".to_string(),
                    msg: "rootNode is missing".to_string(),
                });
                return issues;
            }
        };
        if class_of(root) != Some("RootNode") {
            issues.push(Issue {
                path: "/".to_string(),
                msg: format!(
                    "rootNode._class is {:?}, expected \"RootNode\"",
                    class_of(root).unwrap_or("<missing>")
                ),
            });
        }
        check_node(&NodePath::root(), root, &mut issues, known_classes);
        issues
    }

    /// [`Model::validate`] restricted to the class check: the first `_class` that is not in
    /// `known` (the `model_node_types` snapshot) is an error, so a tool can name it.
    pub fn validate_classes(&self, known: &[String]) -> Result<(), VmdlError> {
        let mut found = None;
        if let Some(root) = self.doc.root.get("rootNode").and_then(Value::as_object) {
            first_unknown_class(&NodePath::root(), root, known, &mut found);
        }
        match found {
            Some((path, class)) => Err(VmdlError::UnknownClass { path, class }),
            None => Ok(()),
        }
    }
}

struct ListWalk<'a> {
    class: Option<&'a str>,
    name: Option<&'a str>,
    out: Vec<NodeInfo>,
}

impl ListWalk<'_> {
    fn visit(&mut self, path: &NodePath, node: &Object, depth: Option<usize>) {
        let class = class_of(node).unwrap_or_default();
        if self.class.is_none_or(|wanted| wanted == class)
            && self.name.is_none_or(|wanted| name_of(node) == Some(wanted))
        {
            self.out.push(NodeInfo {
                path: path.to_string(),
                class: class.to_string(),
                name: name_of(node).map(str::to_string),
                child_count: children_of(node).len(),
                attr_keys: attr_keys(node),
            });
        }
        if depth == Some(0) {
            return;
        }
        let child_depth = depth.map(|d| d - 1);
        for (index, child) in children_of(node).iter().enumerate() {
            if let Some(child) = child.as_object() {
                self.visit(&path.child(index), child, child_depth);
            }
        }
    }
}

fn node_to_json(path: &NodePath, node: &Object, depth: Option<usize>) -> Json {
    let mut attrs = Map::new();
    for (key, value) in node.iter() {
        if !is_meta_key(key) {
            attrs.insert(key.to_string(), cs2_kv3::to_json(value));
        }
    }
    let children = match depth {
        Some(0) => Vec::new(),
        _ => children_of(node)
            .iter()
            .enumerate()
            .filter_map(|(index, child)| {
                child
                    .as_object()
                    .map(|child| node_to_json(&path.child(index), child, depth.map(|d| d - 1)))
            })
            .collect(),
    };
    json!({
        "path": path.to_string(),
        "_class": class_of(node).unwrap_or_default(),
        "name": name_of(node),
        "attrs": Json::Object(attrs),
        "children": children,
    })
}

fn check_node(
    path: &NodePath,
    node: &Object,
    issues: &mut Vec<Issue>,
    known_classes: Option<&[String]>,
) {
    match node.get("_class") {
        Some(Value::String(class)) => {
            if let Some(known) = known_classes {
                if !known.iter().any(|candidate| candidate == class) {
                    issues.push(Issue {
                        path: path.to_string(),
                        msg: format!("unknown _class {class:?} (not in the node-type list)"),
                    });
                }
            }
        }
        Some(_) => issues.push(Issue {
            path: path.to_string(),
            msg: "_class is not a string".to_string(),
        }),
        None => issues.push(Issue {
            path: path.to_string(),
            msg: "_class is missing".to_string(),
        }),
    }
    if let Some(name) = node.get("name") {
        if !matches!(name, Value::String(_)) {
            issues.push(Issue {
                path: path.to_string(),
                msg: "name is not a string".to_string(),
            });
        }
    }
    match node.get("children") {
        None => {}
        Some(Value::Array(children)) => {
            for (index, child) in children.iter().enumerate() {
                let child_path = path.child(index);
                match child.as_object() {
                    Some(child) => check_node(&child_path, child, issues, known_classes),
                    None => issues.push(Issue {
                        path: child_path.to_string(),
                        msg: "child is not an object".to_string(),
                    }),
                }
            }
        }
        Some(_) => issues.push(Issue {
            path: path.to_string(),
            msg: "children is not an array".to_string(),
        }),
    }
}

fn first_unknown_class(
    path: &NodePath,
    node: &Object,
    known: &[String],
    found: &mut Option<(String, String)>,
) {
    if found.is_some() {
        return;
    }
    if let Some(class) = class_of(node) {
        if !known.iter().any(|candidate| candidate == class) {
            *found = Some((path.to_string(), class.to_string()));
            return;
        }
    }
    for (index, child) in children_of(node).iter().enumerate() {
        if let Some(child) = child.as_object() {
            first_unknown_class(&path.child(index), child, known, found);
        }
    }
}

/// The `rootNode` object of a document, or a [`VmdlError::NodeNotFound`] for `"/"`.
pub(crate) fn root_object(doc: &cs2_kv3::Document) -> Result<&Object, VmdlError> {
    doc.root
        .get("rootNode")
        .and_then(Value::as_object)
        .ok_or_else(|| VmdlError::NodeNotFound("/".to_string()))
}

pub(crate) fn root_object_mut(doc: &mut cs2_kv3::Document) -> Result<&mut Object, VmdlError> {
    doc.root
        .get_mut("rootNode")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| VmdlError::NodeNotFound("/".to_string()))
}

/// Walk `root`'s children by `path` (the path is relative to `root`).
pub(crate) fn node_at<'a>(root: &'a Object, path: &NodePath) -> Result<&'a Object, VmdlError> {
    let mut node = root;
    for (depth, index) in path.0.iter().enumerate() {
        node = children_of(node)
            .get(*index)
            .and_then(Value::as_object)
            .ok_or_else(|| {
                VmdlError::NodeNotFound(NodePath(path.0[..=depth].to_vec()).to_string())
            })?;
    }
    Ok(node)
}

pub(crate) fn node_at_mut<'a>(
    root: &'a mut Object,
    path: &NodePath,
) -> Result<&'a mut Object, VmdlError> {
    let Some((index, rest)) = path.0.split_last() else {
        return Ok(root);
    };
    let parent = node_at_mut(root, &NodePath(rest.to_vec()))?;
    parent
        .get_mut("children")
        .and_then(Value::as_array_mut)
        .and_then(|children| children.get_mut(*index))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| VmdlError::NodeNotFound(path.to_string()))
}

pub(crate) fn class_of(node: &Object) -> Option<&str> {
    node.get("_class").and_then(Value::as_str)
}

pub(crate) fn name_of(node: &Object) -> Option<&str> {
    node.get("name").and_then(Value::as_str)
}

pub(crate) fn children_of(node: &Object) -> &[Value] {
    match node.get("children").and_then(Value::as_array) {
        Some(children) => children,
        None => &[],
    }
}

pub(crate) fn attr_keys(node: &Object) -> Vec<String> {
    node.keys()
        .filter(|key| !is_meta_key(key))
        .map(str::to_string)
        .collect()
}

fn empty_object() -> &'static Object {
    static EMPTY: OnceLock<Object> = OnceLock::new();
    EMPTY.get_or_init(Object::new)
}

/// Class names from a `research/modeldoc-nodetypes.json` snapshot
/// (`{"build": "…", "classes": ["RootNode", …]}`). `None` when the file is missing, unreadable or
/// not in that shape.
pub fn load_snapshot(path: &std::path::Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: Json = serde_json::from_str(&text).ok()?;
    let classes = json.get("classes")?.as_array()?;
    let mut out = Vec::with_capacity(classes.len());
    for class in classes {
        out.push(class.as_str()?.to_string());
    }
    Some(out)
}

/// The node-type snapshot committed with this crate (`research/modeldoc-nodetypes.json`), built
/// into the binary so class checks work from any checkout or install location.
pub const BUILTIN_SNAPSHOT: &str = include_str!("../../research/modeldoc-nodetypes.json");

/// The `_class` list to check models against: the snapshot at `path` when it is readable (a newer
/// `model_node_types {writeSnapshot}` wins), else [`BUILTIN_SNAPSHOT`]. The second value names the
/// source (`path` or `"built-in (build <n>)"`).
pub fn known_classes(path: &std::path::Path) -> (Vec<String>, String) {
    if let Some(classes) = load_snapshot(path) {
        return (classes, path.display().to_string());
    }
    let json: Json = serde_json::from_str(BUILTIN_SNAPSHOT).expect("built-in snapshot is JSON");
    let classes = json["classes"]
        .as_array()
        .expect("built-in snapshot has classes")
        .iter()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect();
    let build = json["build"].as_str().unwrap_or("?");
    (classes, format!("built-in (build {build})"))
}
