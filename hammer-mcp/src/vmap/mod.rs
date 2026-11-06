//! File-backed reads of the SAVED `.vmap` (binary DMX, `encoding binary 9`): entities with their
//! keyvalues and I/O connections. A Rust port of `vmap_dmx.py` (the retired Python oracle `vmap_dmx.py`, `git show 8cdea42:bridge/vmapreader/vmap_dmx.py`; the oracle;
//! golden-tested against `fixtures/vmap/`). Reads always report `staleRead` (unsaved bridge edits
//! since the last save) and `source.fromDisk: true` at the tool layer (`tools::entities_file`).
//!
//! Semantics (kept identical to the Python reader): an entity is a node that owns an
//! `entity_properties` child element (so `worldspawn`, nodeID 1, is included); `keyvalues` are the
//! child's attributes minus `id`, rendered as text; `origin`/`angles`/`scales` come from the node;
//! `node_id` is the id tools expose, `guid` the durable element id. Plan: entities-plan.md §2, WP-R.
//!
//! Only the entity view is ported (the oracle's world/orphan/descendant walks and its
//! substring `find_entities` are not needed here).
//!
//! `connections` (I/O) come from the node's `connectionsData` (`connections_of`). Both checked-in
//! fixtures have empty `connectionsData`, so the path is verified against real maps by the
//! `#[ignore]`d real-tree tests: `template_wingman.vmap`'s nodeID 73 `logic_auto` (one
//! `OnMapSpawn` connection) and `cs_script_demo.vmap` (183 entities, 12 with connections) in
//! `hammer-mcp/tests/vmap_dmx.rs`, plus the `get_entity` route in
//! `hammer-mcp/tests/entities_file.rs` — `cargo test -p hammer-mcp --test vmap_dmx -- --ignored`
//! and `cargo test -p hammer-mcp --test entities_file -- --ignored`.
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

pub mod dmx;
pub mod mesh;
pub mod prefab;

#[derive(Debug, thiserror::Error)]
pub enum VmapError {
    #[error("vmap reader not implemented yet")]
    NotImplemented,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported DMX encoding {found:?} (only binary 9)")]
    UnsupportedEncoding { found: String },
    #[error("unsupported binary DMX version {version} (only binary {v})", v = dmx::BINARY_VERSION)]
    UnsupportedBinaryVersion { version: i32 },
    #[error("malformed DMX at byte {offset}: {reason}")]
    Malformed { offset: u64, reason: String },
    #[error("{count} trailing bytes after the last element")]
    TrailingData { count: u64 },
    #[error("invalid selector: give a node id or a targetname")]
    InvalidSelector,
}

/// One entity (POC `vmap_reader.py` `to_dict` shape through [`Entity::to_json`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// Element GUID (`uuid.UUID(bytes_le=...)` text form, as the Python reader prints it).
    pub guid: String,
    /// The node element's type (`CMapEntity`, `CMapWorld`, ...).
    pub node_class: String,
    pub node_id: Option<i64>,
    pub reference_id: Option<u64>,
    pub classname: String,
    pub targetname: Option<String>,
    pub origin: Option<[f64; 3]>,
    pub angles: Option<[f64; 3]>,
    pub scales: Option<[f64; 3]>,
    /// `entity_properties` attributes (minus `id`) in file order, values as JSON.
    pub keyvalues: Vec<(String, Value)>,
    /// `connectionsData` entries (I/O), each as a JSON object of its attributes.
    pub connections: Vec<Map<String, Value>>,
}

impl Entity {
    /// JSON view; keyvalues/connections only when `with_keyvalues`.
    ///
    /// The keys are exactly the oracle's `Entity.to_dict`: `guid`, `nodeClass`, `nodeID`,
    /// `referenceID`, `classname`, `targetname`, `origin`, `angles`, `scales`, `connections` and
    /// (when requested) `keyvalues`. Differences from the sketch in the WP-R brief, deliberately:
    /// there is no `id` key (the MCP-facing id *is* `nodeID`; the Python reader has no `id` either),
    /// and `referenceID` is included because the oracle prints it.
    ///
    /// `targetname` is `""` (not `null`) for an unnamed entity, as in the oracle. The `keyvalues`
    /// object holds the values in file order in [`Entity::keyvalues`]; `serde_json`'s default map
    /// sorts object keys, so a consumer that needs the on-disk order reads the field, not the JSON.
    pub fn to_json(&self, with_keyvalues: bool) -> Value {
        let mut map = Map::new();
        map.insert("guid".into(), Value::from(self.guid.as_str()));
        map.insert("nodeClass".into(), Value::from(self.node_class.as_str()));
        map.insert(
            "nodeID".into(),
            self.node_id.map(Value::from).unwrap_or(Value::Null),
        );
        map.insert(
            "referenceID".into(),
            self.reference_id.map(Value::from).unwrap_or(Value::Null),
        );
        map.insert("classname".into(), Value::from(self.classname.as_str()));
        map.insert(
            "targetname".into(),
            Value::from(self.targetname.clone().unwrap_or_default()),
        );
        map.insert("origin".into(), vec3_json(self.origin));
        map.insert("angles".into(), vec3_json(self.angles));
        map.insert("scales".into(), vec3_json(self.scales));
        map.insert(
            "connections".into(),
            Value::Array(
                self.connections
                    .iter()
                    .map(|connection| Value::Object(connection.clone()))
                    .collect(),
            ),
        );
        if with_keyvalues {
            let mut keyvalues = Map::new();
            for (name, value) in &self.keyvalues {
                keyvalues.insert(name.clone(), value.clone());
            }
            map.insert("keyvalues".into(), Value::Object(keyvalues));
        }
        Value::Object(map)
    }
}

fn vec3_json(vector: Option<[f64; 3]>) -> Value {
    match vector {
        None => Value::Null,
        Some([x, y, z]) => Value::Array(vec![Value::from(x), Value::from(y), Value::from(z)]),
    }
}

/// A parsed `.vmap` (entities and meshes; both views over the same DMX tree). Meshes are
/// [`mesh::MeshRecord`]s (POC `vmap_reader.py` port), one per usable `CMapMesh` element.
#[derive(Debug)]
pub struct Document {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    pub len: u64,
    entities: Vec<Entity>,
    meshes: Vec<mesh::MeshRecord>,
    /// The `CMapPrefab` nodes ([`prefab::prefab_view`]).
    prefabs: Vec<prefab::PrefabRecord>,
    /// The `children` arrays by the owning node's id (`children_map`), WP-L.
    children: HashMap<i64, Vec<(String, Option<i64>)>>,
}

/// Parse `path` (cached by `(path, mtime, len)`: a re-save invalidates the entry).
pub fn load(path: &Path) -> Result<Arc<Document>, VmapError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| VmapError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let before = std::fs::metadata(&canonical).map_err(|source| VmapError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let len = before.len();
    let mtime = before.modified().ok();
    let key = CacheKey {
        path: canonical.clone(),
        mtime,
        len,
    };
    if let Some(document) = cache_lock().get(&key) {
        return Ok(Arc::clone(document));
    }

    let data = std::fs::read(&canonical).map_err(|source| VmapError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // A concurrent Hammer save would otherwise be parsed half-written (oracle `DmxFile.load`).
    let after = std::fs::metadata(&canonical).map_err(|source| VmapError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if after.len() != len || after.modified().ok() != mtime {
        return Err(VmapError::Malformed {
            offset: 0,
            reason: "file changed while reading (Hammer saving?)".to_string(),
        });
    }

    let parsed = dmx::parse(&data)?;
    let parsed = Arc::new(Document {
        path: canonical,
        mtime,
        len,
        entities: entity_view(&parsed),
        meshes: mesh::mesh_view(&parsed),
        prefabs: prefab::prefab_view(&parsed),
        children: children_map(&parsed),
    });
    // `entry().or_insert_with` rather than `insert`: two threads that both missed the entry above
    // must still end up with the same `Arc`, so the second `load` is always the cached document.
    let mut cache = cache_lock();
    let document = cache.entry(key).or_insert_with(|| Arc::clone(&parsed));
    Ok(Arc::clone(document))
}

/// Cache key: canonical path, modification time and size (an unchanged file is re-used).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    mtime: Option<SystemTime>,
    len: u64,
}

fn cache() -> &'static Mutex<HashMap<CacheKey, Arc<Document>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Arc<Document>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A poisoned cache mutex still holds usable documents; only the owners of the poisoned lock are
/// suspect, so recover rather than fail the read.
fn cache_lock() -> std::sync::MutexGuard<'static, HashMap<CacheKey, Arc<Document>>> {
    cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The entity view of a parsed file (oracle `DmxFile.entities`): every element that owns an
/// `entity_properties` element, in element-index order.
fn entity_view(document: &dmx::DmxDocument) -> Vec<Entity> {
    let elements = &document.elements;
    let mut entities = Vec::new();
    for node in elements {
        let Some(properties) = properties_of(node, elements) else {
            continue;
        };
        // keyvalues in file order, values in the oracle's `_json_value` form
        let keyvalues: Vec<(String, Value)> = properties
            .attributes
            .iter()
            .filter(|(name, _)| name != "id")
            .map(|(name, value)| (name.clone(), value.json_value(elements, 0)))
            .collect();
        // text form of the raw attribute, as the oracle's `_kv_text` does
        let text = |name: &str| {
            properties
                .get(name)
                .map(|value| value.to_text(elements))
                .unwrap_or_default()
        };
        let targetname = text("targetname");
        entities.push(Entity {
            guid: node.guid.clone(),
            node_class: node.class_name.clone(),
            node_id: node.get("nodeID").and_then(dmx::Value::as_i64),
            reference_id: node.get("referenceID").and_then(dmx::Value::as_u64),
            classname: text("classname"),
            targetname: (!targetname.is_empty()).then_some(targetname),
            origin: node.get("origin").and_then(dmx::Value::as_vec3).map(rounded),
            angles: node.get("angles").and_then(dmx::Value::as_vec3).map(rounded),
            scales: node.get("scales").and_then(dmx::Value::as_vec3).map(rounded),
            connections: connections_of(node, elements),
            keyvalues,
        });
    }
    entities
}

/// The `entity_properties` child of a node, if it has one (oracle `isinstance(props, DmxElement)`).
fn properties_of<'a>(
    node: &dmx::DmxElement,
    elements: &'a [dmx::DmxElement],
) -> Option<&'a dmx::DmxElement> {
    match node.get("entity_properties") {
        Some(dmx::Value::Element(index)) => elements.get(*index),
        _ => None,
    }
}

/// `connectionsData` entries (oracle `Entity.connections`), each element's attributes minus `id`.
fn connections_of(node: &dmx::DmxElement, elements: &[dmx::DmxElement]) -> Vec<Map<String, Value>> {
    let Some(dmx::Value::Array(items)) = node.get("connectionsData") else {
        return Vec::new();
    };
    let mut connections = Vec::new();
    for item in items {
        let dmx::Value::Element(index) = item else {
            continue;
        };
        let Some(element) = elements.get(*index) else {
            continue;
        };
        let mut map = Map::new();
        for (name, value) in &element.attributes {
            if name != "id" {
                map.insert(name.clone(), value.json_value(elements, 0));
            }
        }
        connections.push(map);
    }
    connections
}

/// float32 -> the oracle's `_round_f32` (9 significant digits) as an f64.
fn rounded(vector: [f32; 3]) -> [f64; 3] {
    [
        dmx::round_f32(vector[0]),
        dmx::round_f32(vector[1]),
        dmx::round_f32(vector[2]),
    ]
}

/// The `children` arrays of the parsed tree, keyed by the owning node's `nodeID` (WP-L): one
/// `(child element class, child nodeID)` pair per child, in element order. A parent without a
/// `nodeID` cannot be addressed by the tools, so it is not a key; when two elements claim the
/// same id the first owner wins, exactly like `mesh::children_parents`. Brush entities are the
/// consumers: a `CMapEntity` whose children are `CMapMesh` elements (probe.vmap: nodeID 67 ->
/// mesh 66, 70 -> mesh 71).
fn children_map(document: &dmx::DmxDocument) -> HashMap<i64, Vec<(String, Option<i64>)>> {
    let mut map: HashMap<i64, Vec<(String, Option<i64>)>> = HashMap::new();
    for parent in &document.elements {
        let Some(dmx::Value::Array(kids)) = parent.get("children") else {
            continue;
        };
        let Some(parent_id) = parent.get("nodeID").and_then(dmx::Value::as_i64) else {
            continue;
        };
        let entry = map.entry(parent_id).or_default();
        for kid in kids {
            if let dmx::Value::Element(index) = kid {
                if let Some(kid_element) = document.elements.get(*index) {
                    entry.push((
                        kid_element.class_name.clone(),
                        kid_element.get("nodeID").and_then(dmx::Value::as_i64),
                    ));
                }
            }
        }
    }
    map
}

impl Document {
    /// Test seam for the tool tests (no file needed).
    #[doc(hidden)]
    pub fn from_entities(path: PathBuf, entities: Vec<Entity>) -> Self {
        Document {
            path,
            mtime: None,
            len: 0,
            entities,
            meshes: Vec::new(),
            prefabs: Vec::new(),
            children: HashMap::new(),
        }
    }

    /// Test seam with meshes ([`crate::tools::mesh_file`] tool tests).
    #[doc(hidden)]
    pub fn from_parts(path: PathBuf, entities: Vec<Entity>, meshes: Vec<mesh::MeshRecord>) -> Self {
        Document {
            path,
            mtime: Some(SystemTime::UNIX_EPOCH),
            len: 0,
            entities,
            meshes,
            prefabs: Vec::new(),
            children: HashMap::new(),
        }
    }

    /// Test seam: the `children` map (brush entity -> its mesh volumes) of a hand-built document.
    #[doc(hidden)]
    pub fn with_children(mut self, children: HashMap<i64, Vec<(String, Option<i64>)>>) -> Self {
        self.children = children;
        self
    }

    /// Test seam: the `CMapPrefab` records of a hand-built document.
    #[doc(hidden)]
    pub fn with_prefabs(mut self, prefabs: Vec<prefab::PrefabRecord>) -> Self {
        self.prefabs = prefabs;
        self
    }

    /// The `CMapPrefab` nodes in element order (instances of another map; not entities).
    pub fn prefabs(&self) -> &[prefab::PrefabRecord] {
        &self.prefabs
    }

    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// The CMapMesh records in element order ([`mesh::mesh_view`]).
    pub fn meshes(&self) -> &[mesh::MeshRecord] {
        &self.meshes
    }

    /// One mesh by node id (brief WP-A: the reusable lookup WP-C's edit tools call).
    pub fn mesh_by_id(&self, id: i64) -> Option<&mesh::MeshRecord> {
        self.meshes.iter().find(|mesh| mesh.id == Some(id))
    }

    /// The direct `children` of one node, addressed by the node id the tools expose (WP-L):
    /// `(child element class, child nodeID)` pairs in element order — a brush entity's
    /// `CMapMesh` volumes live here (probe.vmap: nodeID 67 -> `[("CMapMesh", 66)]`).
    ///
    /// Built at load from the tree's `children` arrays ([`children_map`]). `None` when the
    /// document holds no node with that id; a node with an empty `children` attribute reads as
    /// an empty slice (the caller's "no mesh children" decision), an unknown id as `None`. The
    /// slice is the cached document's own memory, not a clone; the signature deviates from the
    /// WP sketch's `Option<Vec<..>>` on purpose and the WP brief says to say so.
    pub fn children_of(&self, node_id: i64) -> Option<&[(String, Option<i64>)]> {
        self.children.get(&node_id).map(Vec::as_slice)
    }

    /// Entities matching every given filter (classname/targetname exact, case-insensitive;
    /// `within` = (origin, radius) sphere on the node origin), at most `limit`.
    ///
    /// Unlike the oracle's substring `find_entities`, the filters here are exact matches (the tool
    /// layer owns fuzzy matching), and the result keeps the file's element order.
    pub fn find(
        &self,
        class: Option<&str>,
        targetname: Option<&str>,
        within: Option<([f64; 3], f64)>,
        limit: usize,
    ) -> Vec<&Entity> {
        self.entities
            .iter()
            .filter(|entity| match class {
                Some(class) => entity.classname.eq_ignore_ascii_case(class),
                None => true,
            })
            .filter(|entity| match targetname {
                Some(name) => entity
                    .targetname
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(name)),
                None => true,
            })
            .filter(|entity| match within {
                Some((origin, radius)) => entity.origin.is_some_and(|point| {
                    let distance: f64 = (0..3)
                        .map(|axis| (point[axis] - origin[axis]).powi(2))
                        .sum();
                    distance <= radius * radius
                }),
                None => true,
            })
            .take(limit)
            .collect()
    }

    /// Every entity named `targetname`, compared case-insensitively (Hammer's own targetname
    /// semantics, the same rule as the live `entity.find`); an empty name matches nothing.
    pub fn by_targetname(&self, targetname: &str) -> Vec<&Entity> {
        if targetname.is_empty() {
            return Vec::new();
        }
        self.entities
            .iter()
            .filter(|entity| {
                entity
                    .targetname
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(targetname))
            })
            .collect()
    }

    /// One entity by node id or targetname (first match; the tools use [`Self::by_targetname`] to
    /// detect a collision); neither -> `InvalidSelector`.
    ///
    /// `node_id` wins when both are given; the targetname match is exact but case-insensitive, and
    /// an empty targetname matches nothing.
    pub fn get(
        &self,
        node_id: Option<i64>,
        targetname: Option<&str>,
    ) -> Result<Option<&Entity>, VmapError> {
        if let Some(node_id) = node_id {
            return Ok(self
                .entities
                .iter()
                .find(|entity| entity.node_id == Some(node_id)));
        }
        if let Some(targetname) = targetname {
            return Ok(self.by_targetname(targetname).into_iter().next());
        }
        Err(VmapError::InvalidSelector)
    }
}
