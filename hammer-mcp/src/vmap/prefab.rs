//! `CMapPrefab` nodes of a saved `.vmap`: the instances of another map (`targetMapPath`), as the
//! `prefab` tool and the node listings show them.
//!
//! Layout: INFERRED, not read from a Hammer-written file (no saved `.vmap` of the installed trees
//! holds a `CMapPrefab`; `workflows/import/FIXME.md` `## I4`). The element class and the attribute
//! names are the hammer.dll ones (RE `re-prefab.md`: factory `CMapPrefab : CMapInstance : CMapNode`,
//! attribute table `m_TargetMapPath` + the `m_b*` flags), spelled the way the other node classes
//! spell theirs (`nodeID`, `origin`, `angles`, `scales`, `children`). Everything is optional on read:
//! a prefab with no `targetMapPath` is still listed (`targetMapPath: ""`), never dropped.
//!
//! A prefab is **not a mesh parent**: [`super::mesh`]'s ancestor gate keeps refusing a mesh under a
//! `CMapPrefab` (its children may be stored in the prefab's own space).
use super::dmx::{DmxDocument, DmxElement, Value};
use serde_json::{Map, Value as Json};

/// The element class of an instanced prefab node.
pub const PREFAB_CLASS: &str = "CMapPrefab";

/// The optional flag/attribute names an `inspect` reports when the file carries them.
const EXTRA_KEYS: [&str; 5] = [
    "fixupEntityNames",
    "loadAtRuntime",
    "loadIfNested",
    "useTargetnameAsPrefix",
    "entityIOTargetName",
];

/// One `CMapPrefab` node.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefabRecord {
    pub guid: String,
    pub node_id: Option<i64>,
    /// `targetMapPath` as stored (content-relative, `maps/prefabs/x.vmap`), `""` when absent.
    pub target_map_path: String,
    pub origin: Option<[f64; 3]>,
    pub angles: Option<[f64; 3]>,
    pub scales: Option<[f64; 3]>,
    /// The flags/strings of [`EXTRA_KEYS`] the element carries, in that order.
    pub extras: Vec<(String, Json)>,
}

impl PrefabRecord {
    pub fn to_json(&self) -> Json {
        let vec3 = |v: Option<[f64; 3]>| v.map_or(Json::Null, |v| Json::from(v.to_vec()));
        let mut map = Map::new();
        map.insert("nodeID".into(), self.node_id.map_or(Json::Null, Json::from));
        map.insert("class".into(), Json::from("prefab"));
        map.insert("nodeClass".into(), Json::from(PREFAB_CLASS));
        map.insert("guid".into(), Json::from(self.guid.as_str()));
        map.insert("targetMapPath".into(), Json::from(self.target_map_path.as_str()));
        map.insert("origin".into(), vec3(self.origin));
        map.insert("angles".into(), vec3(self.angles));
        map.insert("scales".into(), vec3(self.scales));
        for (key, value) in &self.extras {
            map.insert(key.clone(), value.clone());
        }
        Json::Object(map)
    }
}

/// Every `CMapPrefab` of a parsed file, in element order.
pub fn prefab_view(document: &DmxDocument) -> Vec<PrefabRecord> {
    let elements = &document.elements;
    elements
        .iter()
        .filter(|element| element.class_name == PREFAB_CLASS)
        .map(|element| record_of(element, elements))
        .collect()
}

fn record_of(element: &DmxElement, elements: &[DmxElement]) -> PrefabRecord {
    let vec3 = |name: &str| {
        element
            .get(name)
            .and_then(Value::as_vec3)
            .map(|v| [super::dmx::round_f32(v[0]), super::dmx::round_f32(v[1]), super::dmx::round_f32(v[2])])
    };
    PrefabRecord {
        guid: element.guid.clone(),
        node_id: element.get("nodeID").and_then(Value::as_i64),
        target_map_path: match element.get("targetMapPath") {
            Some(Value::Str(path)) => path.clone(),
            _ => String::new(),
        },
        origin: vec3("origin"),
        angles: vec3("angles"),
        scales: vec3("scales"),
        extras: EXTRA_KEYS
            .iter()
            .filter_map(|key| element.get(key).map(|value| ((*key).to_string(), value.json_value(elements, 0))))
            .collect(),
    }
}
