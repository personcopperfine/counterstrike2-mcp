//! `selection.get` / `selection.set`: the document's selection, nodes and faces (bridge m3.12).
//!
//! Unlike the rest of `ops`, these two rows have no C counterpart: the C table lists them as
//! blocked ("not isolated"), the m3.12 RE pinned the entry points (`HFN_SEL_*`,
//! `HFN_FACESEL_*`, `generated/pins.rs`) and [`crate::registry::RUST_OVERRIDES`] flips the rows.
//!
//! # What Hammer holds
//!
//! The selection object is `CMapDoc::m_pSelection` ([`Chain::selection`]). It owns eight
//! selection sets, one per mode (0 groups, 1 objects, 2 meshes, 3 vertices, 4 edges, 5 faces,
//! 6 mixed, 7 tile), and a current mode. Modes 0..=2 hold nodes; mode 5 holds, per mesh, the
//! selected faces. Nothing here opens an undo group or marks the document dirty: Hammer's own
//! selection functions record neither (the reply says `"undoGroupOpened":false`).
//!
//! # Mode switches
//!
//! `SET_MODE` is always called with `convert = 0`: Hammer then converts nothing, and the target
//! mode's set keeps whatever it held the last time that mode was active (stale). So after the
//! bridge switches mode it clears the target set (`SELECT_NODES(empty, op 1)`, which clears the
//! current mode's set only) before applying the op, and the reply says `"modeChanged":true`.
//! `add` / `remove` after a switch are therefore relative to an empty selection. A face
//! `replace` always clears first and then adds per mesh (`SELECT_FACES` returns early for a
//! mesh that ignores face selection, even before an op-1 clear, so op 1 is never used on it).
//!
//! # Wire shapes
//!
//! `selection.get {}` answers `{"mode":<name|null>,"count":N,"nodeIDs":[..],"faces":[..]}`.
//! `mode` is found by pointer comparison (the `k` whose set is the current set; no offset is
//! read). Outside face mode `count` is Hammer's item count and `nodeIDs` the selected nodes;
//! in face mode `nodeIDs` are the meshes that have selected faces, `faces` is
//! `[{"nodeID":N,"faces":[i,..]},..]`, each `i` a position in that mesh's `CollectHandles` face
//! list (the order `mesh.get` reports and the saved `.vmap` stores), sorted ascending, and
//! `count` the number of faces listed.
//!
//! `selection.set {"op":"replace"|"add"|"remove"|"clear","nodeIDs"?:[..],"faces"?:[..],
//! "expectDoc"}` takes exactly one of `nodeIDs` / `faces` unless `op` is `clear` (neither).
//! Everything is validated before Hammer is called, so a refusal changes nothing. Hammer may
//! filter (locked or hidden nodes, meshes that ignore face selection): the reply's `applied` is
//! then smaller than `requested`, which is not an error.
//!
//! # Host
//!
//! Everything that touches Hammer is one [`Selection`] method; the production implementation is
//! [`GameSelection`] (the `live` module, Windows only) bound to the pinned functions. The
//! face-index mapping reuses [`crate::ops::mesh`]'s `mesh_counts` / `collect_faces`
//! (`CollectHandles`, the same list `mesh.setMaterial` walks), never a second convention.
//! Vectors handed to Hammer are caller-owned buffers laid out like the generated
//! `CUtlVector_CEntityHandle` (compile-time asserted), `alloc` sized as the RE contract says:
//! `GET_NODES`, `GET_MESHES` and `GET_FACES` get an empty vector with `alloc == COUNT` (read in the
//! mode being listed; `GET_FACES` writes one entry per face-set entry, so the mesh's face count
//! would not be enough).

use std::collections::{BTreeSet, HashMap};

use crate::entity_json::Sb;
use crate::ops::hammer::{
    active_doc_with, write_finish, write_gate, Chain, Doc, Node, Nodes,
    GEO_MAX_DEPTH, GEO_MAX_NODES, NODEID_MAX, OP_RESULT_CAP,
};
use crate::ops::mesh::MeshHandle;
use crate::ops::{finish, OpErr};
use crate::queue::Request;

/// Most node ids (and most `faces` entries) one `selection.set` carries.
pub(crate) const MAX_NODE_IDS: usize = 4096;
/// Most face indices, summed over all entries, one `selection.set` carries.
pub(crate) const MAX_FACE_INDICES: usize = 65_536;
/// Sanity cap on an item count Hammer reports: bigger is refused instead of allocated.
const MAX_SELECTION: i32 = 1 << 20;

/// Hammer's selection modes (`ISelectionSet` slots), in `GET_SET` order (6 is a
/// `CMixedSelectionSet`).
const MODE_NAMES: [&str; 8] =
    ["groups", "objects", "meshes", "vertices", "edges", "faces", "mixed", "tile"];
/// The objects mode: what a node selection switches to when the current mode holds no nodes.
const MODE_OBJECTS: i32 = 1;
/// The faces mode.
const MODE_FACES: i32 = 5;

/// `SELECT_NODES` / `SELECT_FACES` operation 1 (replace).
const OP_HAMMER_REPLACE: i32 = 1;
/// Operation 2 (add).
const OP_HAMMER_ADD: i32 = 2;
/// Operation 3 (remove).
const OP_HAMMER_REMOVE: i32 = 3;

/// The overflow text of a selection result.
const OVERFLOW: &str = "response too large (select fewer faces)";

/// The selection primitives, one per pinned Hammer call, plus the mesh face list.
///
/// `sel` is `doc->m_pSelection` ([`Chain::selection`]); `set` is `GET_SET(sel, 5)`. Callers
/// pass only modes `0..=7` and sized `alloc`s; implementations may assert it.
pub(crate) trait Selection: Chain + Nodes {
    /// `HFN_SEL_CURRENT_SET(sel)`: the current mode's set, 0 if Hammer's mode is outside 0..=7.
    fn current_set(&self, sel: usize) -> usize;
    /// `HFN_SEL_GET_SET(sel, mode)`; `mode` is `0..=7`.
    fn get_set(&self, sel: usize, mode: i32) -> usize;
    /// `HFN_SEL_COUNT(sel)`: the item count of the current set.
    fn count(&self, sel: usize) -> i32;
    /// `HFN_SEL_GET_NODES(sel, &vec)`: the selected nodes (it assigns; empty in modes 3..=7),
    /// into an empty buffer of `alloc` slots.
    fn selected_nodes(&self, sel: usize, alloc: usize) -> Vec<Node>;
    /// `HFN_FACESEL_GET_MESHES(set, &vec)`: each mesh with selected faces, `alloc` slots.
    fn selected_meshes(&self, set: usize, alloc: usize) -> Vec<Node>;
    /// `HFN_FACESEL_GET_FACES(set, mesh, &vec)`: that mesh's selected face handles, an empty
    /// buffer of `alloc` (>= the face-mode COUNT) slots.
    fn selected_faces(&self, set: usize, mesh: Node, alloc: usize) -> Vec<MeshHandle>;
    /// `HFN_SEL_SET_MODE(sel, mode, 0)`; `mode` is `0..=7`; no conversion.
    fn set_mode(&self, sel: usize, mode: i32);
    /// `HFN_SEL_SELECT_NODES(sel, &vec, op, 1)`.
    fn select_nodes(&self, sel: usize, nodes: &[Node], op: i32);
    /// `HFN_FACESEL_SELECT_FACES(set, mesh, &vec, op, 0)`.
    fn select_faces(&self, set: usize, mesh: Node, faces: &[MeshHandle], op: i32);
    /// The mesh's face count (`hm->faces.count`): `GET_FACES`'s `alloc` and `CollectHandles`'s.
    fn mesh_face_count(&self, mesh: Node) -> i32;
    /// The mesh's face handles in `CollectHandles` order, `None` when the call failed.
    fn face_handles(&self, mesh: Node) -> Option<Vec<MeshHandle>>;
}

// ---------------------------------------------------------------------------------------------
// The live tree and the selection read
// ---------------------------------------------------------------------------------------------

/// Every node under the world with an id, both ways.
struct Tree {
    by_id: HashMap<i32, Node>,
    by_ptr: HashMap<usize, i32>,
}

/// Walk the document's tree once (`geo_find`'s bounds: depth and visit budget).
fn tree(host: &dyn Selection, doc: Doc) -> Tree {
    fn visit(host: &dyn Selection, t: &mut Tree, visited: &mut u32, node: Node, depth: i32) {
        if depth > GEO_MAX_DEPTH || *visited > GEO_MAX_NODES {
            return;
        }
        let Some(children) = host.children(node) else {
            return;
        };
        for child in children {
            *visited += 1;
            if let Some(id) = host.id(child) {
                t.by_id.entry(id).or_insert(child);
                t.by_ptr.entry(child.0).or_insert(id);
            }
            visit(host, t, visited, child, depth + 1);
        }
    }
    let mut t = Tree { by_id: HashMap::new(), by_ptr: HashMap::new() };
    if let Some(world) = host.world(doc) {
        let mut visited = 0;
        visit(host, &mut t, &mut visited, world, 0);
    }
    t
}

/// `doc->m_pSelection`, checked as `doc_editable` checks it.
fn selection_of(host: &dyn Selection, doc: Doc) -> Result<usize, OpErr> {
    let sel = host.selection(doc.0);
    if sel == 0 || !host.is_of(sel, crate::generated::pins::HV_SELECTION_VT.value as u32) {
        return Err(OpErr::new(-32603, "InternalError", "no selection object"));
    }
    Ok(sel)
}

/// The mode: the `k` in `0..=7` with `GET_SET(sel, k) == CURRENT_SET(sel)`, `None` otherwise.
fn read_mode(host: &dyn Selection, sel: usize) -> Option<i32> {
    let current = host.current_set(sel);
    if current == 0 {
        return None;
    }
    (0..8).find(|&k| host.get_set(sel, k) == current)
}

/// A handle's identity: its low `u32` (22-bit index + 10-bit generation, RE contract).
fn handle_key(handle: &MeshHandle) -> u32 {
    u32::from_le_bytes([handle[0], handle[1], handle[2], handle[3]])
}

/// What `selection.get` reports.
#[derive(Debug, Default, PartialEq, Eq)]
struct State {
    mode: Option<i32>,
    count: i64,
    node_ids: Vec<i32>,
    /// `(nodeID, sorted face indices)` per mesh, face mode only.
    faces: Vec<(i32, Vec<u32>)>,
    /// Selected items that could not be mapped to a node id / face list (walk caps, no id, a
    /// mesh whose faces cannot be collected).
    unmapped: i64,
    /// The lists above were cut at [`MAX_LISTED`] entries (`count` stays the true count).
    truncated: bool,
}

/// Most node ids / face indices one reported selection lists (keeps a reply under the cap).
const MAX_LISTED: usize = 16_384;

/// Read the selection. Sizes every buffer as the RE contract says (`alloc == COUNT`, or the
/// mesh's face count) and refuses an absurd count instead of allocating it.
fn read_state(host: &dyn Selection, sel: usize, tree: &Tree) -> Result<State, OpErr> {
    let mut state = State { mode: read_mode(host, sel), ..State::default() };
    let Some(mode) = state.mode else {
        return Ok(state);
    };
    let count = host.count(sel);
    if !(0..=MAX_SELECTION).contains(&count) {
        return Err(OpErr::new(-32603, "InternalError", "selection count out of range"));
    }
    if count == 0 {
        return Ok(state);
    }
    let alloc = count as usize;
    if mode != MODE_FACES {
        state.count = i64::from(count);
        let nodes = host.selected_nodes(sel, alloc);
        let ids: Vec<i32> =
            nodes.iter().filter_map(|node| tree.by_ptr.get(&node.0).copied()).collect();
        state.unmapped = (nodes.len() - ids.len()) as i64;
        state.truncated = ids.len() > MAX_LISTED;
        state.node_ids = ids.into_iter().take(MAX_LISTED).collect();
        return Ok(state);
    }
    let set = host.get_set(sel, MODE_FACES);
    let mut listed = 0usize;
    for mesh in host.selected_meshes(set, alloc) {
        let Some(&id) = tree.by_ptr.get(&mesh.0) else {
            state.unmapped += 1;
            continue;
        };
        let Some(all) = host.face_handles(mesh) else {
            state.unmapped += 1;
            continue;
        };
        let position: HashMap<u32, usize> =
            all.iter().enumerate().map(|(i, h)| (handle_key(h), i)).collect();
        let picked: BTreeSet<u32> = host
            .selected_faces(set, mesh, alloc)
            .iter()
            .filter_map(|h| position.get(&handle_key(h)).map(|&i| i as u32))
            .collect();
        if picked.is_empty() {
            continue;
        }
        state.count += picked.len() as i64;
        if listed + picked.len() > MAX_LISTED {
            state.truncated = true;
            continue;
        }
        listed += picked.len();
        state.node_ids.push(id);
        state.faces.push((id, picked.into_iter().collect()));
    }
    Ok(state)
}

/// The members of a `selection.get` object, without the braces.
fn state_fields_into(out: &mut Sb, state: &State) {
    out.add(b"\"mode\":");
    match state.mode {
        Some(mode) => {
            out.add(b"\"");
            out.add(MODE_NAMES[mode as usize].as_bytes());
            out.add(b"\"");
        }
        None => out.add(b"null"),
    }
    out.add(b",\"count\":");
    out.addi(state.count);
    out.add(b",\"unmapped\":");
    out.addi(state.unmapped);
    if state.truncated {
        out.add(b",\"truncated\":true");
    }
    out.add(b",\"nodeIDs\":[");
    for (i, id) in state.node_ids.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.addi(i64::from(*id));
    }
    out.add(b"],\"faces\":[");
    for (i, (id, faces)) in state.faces.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.add(b"{\"nodeID\":");
        out.addi(i64::from(*id));
        out.add(b",\"faces\":[");
        for (j, face) in faces.iter().enumerate() {
            if j > 0 {
                out.add(b",");
            }
            out.addi(i64::from(*face));
        }
        out.add(b"]}");
    }
    out.add(b"]");
}

/// `selection.get`'s body.
pub(crate) fn op_get(host: &dyn Selection, doc: Doc, out: &mut Sb) -> Result<(), OpErr> {
    let sel = selection_of(host, doc)?;
    let state = read_state(host, sel, &tree(host, doc))?;
    out.add(b"{");
    state_fields_into(out, &state);
    out.add(b"}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// selection.set: parse, validate, apply
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Replace,
    Add,
    Remove,
    Clear,
}

/// The parsed target of a set.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    None,
    Nodes(Vec<i32>),
    /// `(nodeID, sorted unique indices)`; entries naming one node are merged.
    Faces(Vec<(i32, Vec<u32>)>),
}

fn invalid(reason: &str) -> OpErr {
    OpErr::new(-32602, "InvalidParams", reason)
}

/// A node id: an integer in `0..=NODEID_MAX`.
fn node_id(value: &serde_json::Value, what: &str) -> Result<i32, OpErr> {
    match value.as_i64() {
        Some(id) if (0..=NODEID_MAX).contains(&id) => Ok(id as i32),
        _ => Err(invalid(&format!("{what}: a non-negative int up to 2147483647 is required"))),
    }
}

/// Parse the params (after the document gate): shape, bounds and the one-of rule.
fn parse_set(params: Option<&str>) -> Result<(Op, Target), OpErr> {
    use serde_json::Value;
    let Some(params) = params else {
        return Err(invalid("params"));
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(params) else {
        return Err(invalid("params: a JSON object is required"));
    };
    let op = match map.get("op").and_then(Value::as_str) {
        Some("replace") => Op::Replace,
        Some("add") => Op::Add,
        Some("remove") => Op::Remove,
        Some("clear") => Op::Clear,
        _ => return Err(invalid("op: replace, add, remove or clear is required")),
    };
    let present = |key: &str| map.get(key).is_some_and(|v| !v.is_null());
    let (has_nodes, has_faces) = (present("nodeIDs"), present("faces"));
    if op == Op::Clear {
        if has_nodes || has_faces {
            return Err(invalid("clear takes neither nodeIDs nor faces"));
        }
        return Ok((op, Target::None));
    }
    if has_nodes == has_faces {
        return Err(invalid("exactly one of nodeIDs or faces is required (clear takes neither)"));
    }
    if has_nodes {
        let Some(list) = map["nodeIDs"].as_array() else {
            return Err(invalid("nodeIDs: an array of node ids is required"));
        };
        if list.is_empty() {
            return Err(invalid("nodeIDs: at least one id (use op clear to deselect)"));
        }
        if list.len() > MAX_NODE_IDS {
            return Err(invalid("nodeIDs: at most 4096 ids per call"));
        }
        let mut ids: Vec<i32> = Vec::with_capacity(list.len());
        for value in list {
            let id = node_id(value, "nodeIDs")?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        return Ok((op, Target::Nodes(ids)));
    }
    let Some(list) = map["faces"].as_array() else {
        return Err(invalid("faces: an array of {nodeID, faces:[i..]} is required"));
    };
    if list.is_empty() {
        return Err(invalid("faces: at least one entry (use op clear to deselect)"));
    }
    if list.len() > MAX_NODE_IDS {
        return Err(invalid("faces: at most 4096 entries per call"));
    }
    let mut total = 0usize;
    let mut entries: Vec<(i32, BTreeSet<u32>)> = Vec::new();
    for entry in list {
        let Some(object) = entry.as_object() else {
            return Err(invalid("faces: each entry is {nodeID, faces:[i..]}"));
        };
        let id = node_id(object.get("nodeID").unwrap_or(&Value::Null), "faces[].nodeID")?;
        let Some(indices) = object.get("faces").and_then(Value::as_array) else {
            return Err(invalid("faces[].faces: an array of face indices is required"));
        };
        if indices.is_empty() {
            return Err(invalid("faces[].faces: at least one index"));
        }
        total += indices.len();
        if total > MAX_FACE_INDICES {
            return Err(invalid("faces: at most 65536 indices per call"));
        }
        let mut set = BTreeSet::new();
        for value in indices {
            match value.as_i64() {
                Some(i) if (0..=i64::from(i32::MAX)).contains(&i) => {
                    set.insert(i as u32);
                }
                _ => return Err(invalid("faces[].faces: indices are integers >= 0")),
            }
        }
        match entries.iter_mut().find(|(n, _)| *n == id) {
            Some((_, existing)) => existing.extend(set),
            None => entries.push((id, set)),
        }
    }
    let entries = entries
        .into_iter()
        .map(|(id, set)| (id, set.into_iter().collect()))
        .collect();
    Ok((op, Target::Faces(entries)))
}

/// `-32602` naming the ids the live tree does not hold (at most 64 listed).
fn unknown_error(unknown: &[i32]) -> OpErr {
    let mut data = String::from("{\"reason\":\"unknown nodeIDs\",\"unknown\":[");
    for (i, id) in unknown.iter().take(64).enumerate() {
        if i > 0 {
            data.push(',');
        }
        data.push_str(&id.to_string());
    }
    data.push_str(&format!("],\"unknownTotal\":{}}}", unknown.len()));
    OpErr { code: -32602, name: "InvalidParams", data }
}

/// A validated face target: the mesh and the handles to hand Hammer.
struct FaceTarget {
    mesh: Node,
    handles: Vec<MeshHandle>,
}

/// Resolve every nodeID and face index against the live tree. Nothing has changed yet.
fn resolve_faces(
    host: &dyn Selection,
    tree: &Tree,
    entries: &[(i32, Vec<u32>)],
) -> Result<Vec<FaceTarget>, OpErr> {
    let unknown: Vec<i32> =
        entries.iter().map(|(id, _)| *id).filter(|id| !tree.by_id.contains_key(id)).collect();
    if !unknown.is_empty() {
        return Err(unknown_error(&unknown));
    }
    let mut targets = Vec::with_capacity(entries.len());
    for (id, indices) in entries {
        let mesh = tree.by_id[id];
        if host.class(mesh) != "mesh" {
            return Err(OpErr::with_data(-32602, "InvalidParams", |db| {
                db.add(b"{\"reason\":\"not a mesh\",\"nodeID\":");
                db.addi(i64::from(*id));
                db.add(b"}");
            }));
        }
        let slots = host.mesh_face_count(mesh);
        // A mesh with no face slots has no face to select: every index is out of range.
        let all = if slots <= 0 {
            Some(Vec::new())
        } else if slots <= MAX_SELECTION {
            host.face_handles(mesh)
        } else {
            None
        };
        let Some(all) = all else {
            return Err(OpErr::new(-32603, "InternalError", "could not collect face handles"));
        };
        let bad: Vec<u32> =
            indices.iter().copied().filter(|&i| i as usize >= all.len()).collect();
        if !bad.is_empty() {
            return Err(OpErr::with_data(-32602, "InvalidParams", |db| {
                db.add(b"{\"reason\":\"face index out of range\",\"nodeID\":");
                db.addi(i64::from(*id));
                db.add(b",\"faceCount\":");
                db.addi(all.len() as i64);
                db.add(b",\"badIndices\":[");
                for (k, i) in bad.iter().take(16).enumerate() {
                    if k > 0 {
                        db.add(b",");
                    }
                    db.addi(i64::from(*i));
                }
                db.add(b"]}");
            }));
        }
        let handles = indices.iter().map(|&i| all[i as usize]).collect();
        targets.push(FaceTarget { mesh, handles });
    }
    Ok(targets)
}

/// `selection.set`'s body (after [`write_gate`]).
pub(crate) fn op_set(
    host: &dyn Selection,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (op, target) = parse_set(params)?;
    let sel = selection_of(host, doc)?;
    let tree = tree(host, doc);

    // Validate everything first: after this point Hammer is called and nothing is refused.
    enum Plan {
        Clear,
        Nodes(Vec<Node>),
        Faces(Vec<FaceTarget>),
    }
    let (plan, requested) = match &target {
        Target::None => (Plan::Clear, "{}".to_string()),
        Target::Nodes(ids) => {
            let unknown: Vec<i32> =
                ids.iter().copied().filter(|id| !tree.by_id.contains_key(id)).collect();
            if !unknown.is_empty() {
                return Err(unknown_error(&unknown));
            }
            let nodes = ids.iter().map(|id| tree.by_id[id]).collect();
            (Plan::Nodes(nodes), format!("{{\"nodeIDs\":{}}}", ids.len()))
        }
        Target::Faces(entries) => {
            let targets = resolve_faces(host, &tree, entries)?;
            let faces: usize = entries.iter().map(|(_, f)| f.len()).sum();
            (Plan::Faces(targets), format!("{{\"meshes\":{},\"faces\":{faces}}}", entries.len()))
        }
    };

    // An unknown mode (Hammer's -1 sentinel, or > 7) has no set behind it: SELECT_NODES and
    // SET_MODE would call through a null set. Refuse every op, clear included, before any call.
    let before = read_mode(host, sel);
    if before.is_none() {
        return Err(OpErr::new(-32603, "InternalError", "selection mode unknown"));
    }
    let mut mode_changed = false;
    match plan {
        Plan::Clear => host.select_nodes(sel, &[], OP_HAMMER_REPLACE),
        Plan::Nodes(nodes) => {
            if !matches!(before, Some(0..=2)) {
                host.set_mode(sel, MODE_OBJECTS);
                mode_changed = true;
                if op != Op::Replace {
                    // The switched-to set is stale (no conversion): start from empty.
                    host.select_nodes(sel, &[], OP_HAMMER_REPLACE);
                }
            }
            let hammer_op = match op {
                Op::Replace | Op::Clear => OP_HAMMER_REPLACE,
                Op::Add => OP_HAMMER_ADD,
                Op::Remove => OP_HAMMER_REMOVE,
            };
            host.select_nodes(sel, &nodes, hammer_op);
        }
        Plan::Faces(targets) => {
            if before != Some(MODE_FACES) {
                host.set_mode(sel, MODE_FACES);
                mode_changed = true;
            }
            // In face mode this clears the face set. Replace always starts empty; add/remove
            // only after a switch, whose target set is stale.
            if op == Op::Replace || mode_changed {
                host.select_nodes(sel, &[], OP_HAMMER_REPLACE);
            }
            let set = host.get_set(sel, MODE_FACES);
            let hammer_op = if op == Op::Remove { OP_HAMMER_REMOVE } else { OP_HAMMER_ADD };
            for target in &targets {
                host.select_faces(set, target.mesh, &target.handles, hammer_op);
            }
        }
    }

    // The selection has changed: a failed re-read is reported inside the success, not as one.
    let applied = read_state(host, sel, &tree);
    out.add(b"{\"requested\":");
    out.add(requested.as_bytes());
    match &applied {
        Ok(state) => {
            out.add(b",\"applied\":{");
            state_fields_into(out, state);
            out.add(b"}");
        }
        Err(e) => {
            let reason = serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_string))
                .unwrap_or_else(|| e.name.to_string());
            out.add(b",\"applied\":null,\"appliedError\":");
            out.addjson(reason.as_bytes());
        }
    }
    out.add(b",\"modeChanged\":");
    out.add(if mode_changed { b"true" } else { b"false" });
    out.add(b",\"undoGroupOpened\":false}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The dispatch arms
// ---------------------------------------------------------------------------------------------

/// `selection.get`: the active document, then the body.
pub(crate) fn get_dispatch(host: &dyn Selection, id: &str, out: &mut Sb) -> String {
    let outcome = active_doc_with(host)
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))
        .and_then(|doc| op_get(host, doc, out));
    finish(id, outcome, out, OVERFLOW)
}

/// `selection.set`: the write gate (`-32003`/`-32018`/`-32004`), then the body, then the `doc`
/// tail every mutating reply carries.
pub(crate) fn set_dispatch(
    host: &dyn Selection,
    id: &str,
    params: Option<&str>,
    out: &mut Sb,
) -> String {
    let outcome = write_gate(host, params.map(str::as_bytes))
        .and_then(|doc| op_set(host, doc, params, out).map(|()| (doc, false)));
    write_finish(id, outcome, out, OVERFLOW)
}

fn selection_get(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    get_dispatch(&host(), &req.id, &mut out)
}

fn selection_set(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    set_dispatch(&host(), &req.id, req.params.as_deref(), &mut out)
}

/// The rows this group serves.
pub static HANDLERS: &[(&str, crate::ops::Handler)] =
    &[("selection.get", selection_get), ("selection.set", selection_set)];

// ---------------------------------------------------------------------------------------------
// The production host
// ---------------------------------------------------------------------------------------------

/// The production host: the mesh lane's host (document chain, tree, `CollectHandles`) plus the
/// nine bound selection functions.
pub(crate) struct GameSelection(crate::ops::mesh::GameMesh);

/// The production host (`cfg(windows)`), or the loud non-Windows refusal.
pub(crate) fn host() -> GameSelection {
    GameSelection(crate::ops::mesh::host())
}

macro_rules! forward {
    ($trait:ident { $( fn $name:ident($($arg:ident: $argt:ty),*) -> $ret:ty; )* }) => {
        impl $trait for GameSelection {
            $( fn $name(&self, $($arg: $argt),*) -> $ret { $trait::$name(&self.0, $($arg),*) } )*
        }
    };
}

forward!(Chain {
    fn module_reason() -> Option<&'static str>;
    fn app() -> Option<usize>;
    fn session(app: usize) -> Option<usize>;
    fn doc_session(session: usize) -> Option<usize>;
    fn owner(doc_session: usize) -> usize;
    fn doc_of(doc_session: usize) -> usize;
    fn doc_backptr(doc: usize) -> usize;
    fn is_of(object: usize, vtable: u32) -> bool;
    fn root(doc: usize) -> usize;
    fn active_flag(doc: usize) -> bool;
    fn selection(doc: usize) -> usize;
    fn scope_depth(doc: usize) -> u32;
    fn undo_history(doc: usize) -> usize;
    fn active_history() -> usize;
});

forward!(Nodes {
    fn children(node: Node) -> Option<Vec<Node>>;
    fn class(node: Node) -> &'static str;
    fn id(node: Node) -> Option<i32>;
    fn origin(node: Node) -> Option<[f32; 3]>;
    fn mesh_slots(node: Node) -> Option<(i32, i32)>;
    fn world(doc: Doc) -> Option<Node>;
});

#[cfg(windows)]
mod live {
    use core::ffi::c_void;
    use core::mem::offset_of;

    use s2sdk_sys::generated::{CUtlVector_CEntityHandle, EXTERNAL_CONST_BUFFER_MARKER};

    use super::{GameSelection, MeshHandle, Node, Selection};
    use crate::ops::hammer::bind::{hfn, HammerFns};
    use crate::ops::mesh::Mesh;

    /// A `CUtlVector<T>` with caller-owned memory: the generated vector's layout with a typed
    /// element pointer (the selection functions take `CUtlVector<CMapNode*>`,
    /// `CUtlVector<CMapMesh*>` and `CUtlVector<MeshHandle>`; the header only generates the
    /// `CEntityHandle` one, whose layout is element-independent).
    #[repr(C)]
    struct SelVec<T> {
        size: i32,
        pad: i32,
        mem: *mut T,
        alloc: i32,
        flags: i32,
    }

    const _: () = {
        assert!(
            core::mem::size_of::<SelVec<*mut c_void>>()
                == core::mem::size_of::<CUtlVector_CEntityHandle>()
        );
        assert!(
            core::mem::size_of::<SelVec<MeshHandle>>()
                == core::mem::size_of::<CUtlVector_CEntityHandle>()
        );
        assert!(
            offset_of!(SelVec<*mut c_void>, size) == offset_of!(CUtlVector_CEntityHandle, m_Size)
        );
        assert!(offset_of!(SelVec<*mut c_void>, pad) == 4);
        assert!(
            offset_of!(SelVec<*mut c_void>, mem)
                == offset_of!(CUtlVector_CEntityHandle, m_pMemory)
        );
        assert!(
            offset_of!(SelVec<*mut c_void>, alloc)
                == offset_of!(CUtlVector_CEntityHandle, m_nAllocationCount)
        );
        assert!(
            offset_of!(SelVec<*mut c_void>, flags)
                == offset_of!(CUtlVector_CEntityHandle, m_nGrowSize)
        );
        assert!(offset_of!(SelVec<MeshHandle>, mem) == offset_of!(SelVec<*mut c_void>, mem));
        assert!(offset_of!(SelVec<MeshHandle>, alloc) == offset_of!(SelVec<*mut c_void>, alloc));
        assert!(offset_of!(SelVec<MeshHandle>, flags) == offset_of!(SelVec<*mut c_void>, flags));
    };

    /// "Caller-owned, never realloc": the generated marker, as `ops::mesh` uses it.
    const EXTERNAL: i32 = EXTERNAL_CONST_BUFFER_MARKER as i32;

    /// Largest `alloc` this host builds (the callers cap counts far below it).
    const ALLOC_MAX: usize = 1 << 20;

    fn table() -> HammerFns {
        hfn().expect("cs2-bridge: the hammer function table is bound before any selection call")
    }

    fn vec_of<T>(buffer: &mut [T], size: usize) -> SelVec<T> {
        SelVec {
            size: size as i32,
            pad: 0,
            mem: buffer.as_mut_ptr(),
            alloc: buffer.len() as i32,
            flags: EXTERNAL,
        }
    }

    impl Selection for GameSelection {
        fn current_set(&self, sel: usize) -> usize {
            // SAFETY: verified HFN_SEL_CURRENT_SET over the identity-checked selection object.
            unsafe { (table().sel_current_set)(sel as *mut c_void) as usize }
        }

        fn get_set(&self, sel: usize, mode: i32) -> usize {
            assert!((0..8).contains(&mode), "cs2-bridge: selection mode {mode} outside 0..=7");
            // SAFETY: verified HFN_SEL_GET_SET; the mode is range-checked (the function itself
            // does not check).
            unsafe { (table().sel_get_set)(sel as *mut c_void, mode) as usize }
        }

        fn count(&self, sel: usize) -> i32 {
            // SAFETY: verified HFN_SEL_COUNT.
            unsafe { (table().sel_count)(sel as *mut c_void) }
        }

        fn selected_nodes(&self, sel: usize, alloc: usize) -> Vec<Node> {
            let alloc = alloc.min(ALLOC_MAX);
            let mut buffer: Vec<*mut c_void> = vec![core::ptr::null_mut(); alloc];
            let mut out = vec_of(&mut buffer, 0);
            // SAFETY: verified HFN_SEL_GET_NODES; `out` is a caller-owned buffer of exactly
            // `alloc` slots (the caller passes COUNT) that outlives the call.
            unsafe {
                (table().sel_get_nodes)(sel as *mut c_void, (&mut out as *mut SelVec<_>).cast())
            };
            let size = out.size.clamp(0, alloc as i32) as usize;
            buffer.truncate(size);
            buffer.into_iter().filter(|p| !p.is_null()).map(|p| Node(p as usize)).collect()
        }

        fn selected_meshes(&self, set: usize, alloc: usize) -> Vec<Node> {
            let alloc = alloc.min(ALLOC_MAX);
            let mut buffer: Vec<*mut c_void> = vec![core::ptr::null_mut(); alloc];
            let mut out = vec_of(&mut buffer, 0);
            // SAFETY: verified HFN_FACESEL_GET_MESHES over `GET_SET(sel, 5)`; the buffer holds
            // `alloc` (== the set's count) slots and outlives the call.
            unsafe {
                (table().facesel_get_meshes)(
                    set as *mut c_void,
                    (&mut out as *mut SelVec<_>).cast(),
                )
            };
            let size = out.size.clamp(0, alloc as i32) as usize;
            buffer.truncate(size);
            buffer.into_iter().filter(|p| !p.is_null()).map(|p| Node(p as usize)).collect()
        }

        fn selected_faces(&self, set: usize, mesh: Node, alloc: usize) -> Vec<MeshHandle> {
            let alloc = alloc.min(ALLOC_MAX);
            let mut buffer: Vec<MeshHandle> = vec![[0u8; 16]; alloc];
            let mut out = vec_of(&mut buffer, 0);
            // SAFETY: verified HFN_FACESEL_GET_FACES; `mesh` is an identity-checked mesh from
            // the live tree and the buffer holds `alloc` (== its face count) handles.
            unsafe {
                (table().facesel_get_faces)(
                    set as *mut c_void,
                    mesh.0 as *mut c_void,
                    (&mut out as *mut SelVec<_>).cast(),
                )
            };
            buffer.truncate(out.size.clamp(0, alloc as i32) as usize);
            buffer
        }

        fn set_mode(&self, sel: usize, mode: i32) {
            assert!((0..8).contains(&mode), "cs2-bridge: selection mode {mode} outside 0..=7");
            // SAFETY: verified HFN_SEL_SET_MODE; the mode is range-checked (the function itself
            // does not) and `convert` is 0 (no conversion; the third argument is the
            // conversion kind and is read unconditionally).
            unsafe { (table().sel_set_mode)(sel as *mut c_void, mode, 0) };
        }

        fn select_nodes(&self, sel: usize, nodes: &[Node], op: i32) {
            let mut buffer: Vec<*mut c_void> =
                nodes.iter().map(|node| node.0 as *mut c_void).collect();
            let size = buffer.len();
            let vec = vec_of(&mut buffer, size);
            // SAFETY: verified HFN_SEL_SELECT_NODES; every node came from the live tree, the
            // vector is caller-owned and outlives the call, `b` is 1.
            unsafe {
                (table().sel_select_nodes)(
                    sel as *mut c_void,
                    (&vec as *const SelVec<_>).cast(),
                    op,
                    1,
                )
            };
        }

        fn select_faces(&self, set: usize, mesh: Node, faces: &[MeshHandle], op: i32) {
            let mut buffer: Vec<MeshHandle> = faces.to_vec();
            let size = buffer.len();
            let vec = vec_of(&mut buffer, size);
            // SAFETY: verified HFN_FACESEL_SELECT_FACES; the mesh is an identity-checked mesh,
            // every handle came from its `CollectHandles` list, `force` is 0.
            unsafe {
                (table().facesel_select_faces)(
                    set as *mut c_void,
                    mesh.0 as *mut c_void,
                    (&vec as *const SelVec<_>).cast(),
                    op,
                    0,
                )
            };
        }

        fn mesh_face_count(&self, mesh: Node) -> i32 {
            self.0.mesh_counts(mesh).1
        }

        fn face_handles(&self, mesh: Node) -> Option<Vec<MeshHandle>> {
            let slots = self.0.mesh_counts(mesh).1;
            if slots <= 0 {
                return None;
            }
            self.0.collect_faces(mesh, slots as usize)
        }
    }
}

/// Non-Windows: no Hammer module exists, so the `-32017` gate answers before a body runs.
#[cfg(not(windows))]
impl Selection for GameSelection {
    fn current_set(&self, _sel: usize) -> usize {
        no_selection("the current set")
    }
    fn get_set(&self, _sel: usize, _mode: i32) -> usize {
        no_selection("a mode's set")
    }
    fn count(&self, _sel: usize) -> i32 {
        no_selection("the item count")
    }
    fn selected_nodes(&self, _sel: usize, _alloc: usize) -> Vec<Node> {
        no_selection("the selected nodes")
    }
    fn selected_meshes(&self, _set: usize, _alloc: usize) -> Vec<Node> {
        no_selection("the selected meshes")
    }
    fn selected_faces(&self, _set: usize, _mesh: Node, _alloc: usize) -> Vec<MeshHandle> {
        no_selection("the selected faces")
    }
    fn set_mode(&self, _sel: usize, _mode: i32) {
        no_selection("the mode switch")
    }
    fn select_nodes(&self, _sel: usize, _nodes: &[Node], _op: i32) {
        no_selection("a node selection")
    }
    fn select_faces(&self, _set: usize, _mesh: Node, _faces: &[MeshHandle], _op: i32) {
        no_selection("a face selection")
    }
    fn mesh_face_count(&self, _mesh: Node) -> i32 {
        no_selection("a mesh's face count")
    }
    fn face_handles(&self, _mesh: Node) -> Option<Vec<MeshHandle>> {
        no_selection("a mesh's face handles")
    }
}

#[cfg(not(windows))]
fn no_selection(what: &str) -> ! {
    panic!("cs2-bridge: {what} reached without a Hammer module")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const WORLD: Node = Node(0x8000);
    const APP: usize = 0x1000;
    const SESSION: usize = 0x2000;
    const DOCSESSION: usize = 0x3000;
    const SEL: usize = 0x6000;
    const FACESET: usize = 0x6500;
    const HISTORY: usize = 0x7000;

    /// Node ids of the fake tree: an entity, two meshes (4 and 3 faces), a group.
    const ENTITY: i32 = 10;
    const MESH_A: i32 = 11;
    const MESH_B: i32 = 12;
    const GROUP: i32 = 13;

    fn ptr(id: i32) -> usize {
        0x9000 + id as usize
    }

    fn class_of(id: i32) -> &'static str {
        match id {
            ENTITY => "entity",
            MESH_A | MESH_B => "mesh",
            _ => "group",
        }
    }

    fn face_slots(id: i32) -> i32 {
        match id {
            MESH_A => 4,
            MESH_B => 3,
            _ => 0,
        }
    }

    /// Face `i` of mesh `id`: the low `u32` identifies it, the tail is junk the code must ignore.
    fn handle(id: i32, i: u32) -> MeshHandle {
        let key = (id as u32) * 100 + i;
        let mut h = [0xEE; 16];
        h[..4].copy_from_slice(&key.to_le_bytes());
        h
    }

    #[derive(Default)]
    struct State {
        /// Hammer's mode; -1 = a mode outside 0..=7 (no set matches).
        mode: i32,
        /// Every mode's node set (index 5 unused): stale content survives a mode switch, as
        /// `SET_MODE(.., convert = 0)` converts nothing.
        nodes: [Vec<usize>; 8],
        /// The face set (mode 5): `(mesh ptr, selected face indices)`.
        faces: Vec<(usize, Vec<u32>)>,
        calls: Vec<String>,
    }

    /// A model of Hammer's selection: asserts every buffer size and every argument range.
    struct Fake {
        st: RefCell<State>,
        /// Node ptrs Hammer refuses to select (locked / hidden).
        locked: Vec<usize>,
        /// Meshes that ignore face selection (`SELECT_FACES` returns at once for them).
        face_deaf: Vec<usize>,
        no_selection: bool,
        collect_fails: bool,
    }

    impl Fake {
        fn new(mode: i32) -> Self {
            Self {
                st: RefCell::new(State { mode, ..State::default() }),
                locked: Vec::new(),
                face_deaf: Vec::new(),
                no_selection: false,
                collect_fails: false,
            }
        }

        fn with_nodes(mode: i32, ids: &[i32]) -> Self {
            let f = Self::new(mode);
            if (0..8).contains(&mode) {
                f.st.borrow_mut().nodes[mode as usize] = ids.iter().map(|&i| ptr(i)).collect();
            }
            f
        }

        fn with_faces(picks: &[(i32, &[u32])]) -> Self {
            let f = Self::new(5);
            f.st.borrow_mut().faces =
                picks.iter().map(|(id, v)| (ptr(*id), v.to_vec())).collect();
            f
        }

        /// Hammer calls only (reads excluded) since the last take.
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.st.borrow_mut().calls)
        }

        fn set_of(&self, mode: i32) -> usize {
            if mode == MODE_FACES {
                FACESET
            } else {
                0x6100 + mode as usize
            }
        }

        /// `COUNT`: the current set's item count.
        fn count_now(&self, st: &State) -> usize {
            match st.mode {
                5 => st.faces.iter().map(|(_, f)| f.len()).sum(),
                m if (0..8).contains(&m) => st.nodes[m as usize].len(),
                _ => 0,
            }
        }
    }

    impl Chain for Fake {
        fn module_reason(&self) -> Option<&'static str> {
            None
        }
        fn app(&self) -> Option<usize> {
            Some(APP)
        }
        fn session(&self, _app: usize) -> Option<usize> {
            Some(SESSION)
        }
        fn doc_session(&self, _session: usize) -> Option<usize> {
            Some(DOCSESSION)
        }
        fn owner(&self, _doc_session: usize) -> usize {
            SESSION
        }
        fn doc_of(&self, _doc_session: usize) -> usize {
            0x4000
        }
        fn doc_backptr(&self, _doc: usize) -> usize {
            DOCSESSION
        }
        fn is_of(&self, _object: usize, _vtable: u32) -> bool {
            true
        }
        fn root(&self, _doc: usize) -> usize {
            0x5000
        }
        fn active_flag(&self, _doc: usize) -> bool {
            true
        }
        fn selection(&self, _doc: usize) -> usize {
            if self.no_selection {
                0
            } else {
                SEL
            }
        }
        fn scope_depth(&self, _doc: usize) -> u32 {
            0
        }
        fn undo_history(&self, _doc: usize) -> usize {
            HISTORY
        }
        fn active_history(&self) -> usize {
            HISTORY
        }
    }

    impl Nodes for Fake {
        fn children(&self, node: Node) -> Option<Vec<Node>> {
            if node == WORLD {
                // The group nests the meshes: the walk must be recursive.
                Some(vec![Node(ptr(ENTITY)), Node(ptr(GROUP))])
            } else if node.0 == ptr(GROUP) {
                Some(vec![Node(ptr(MESH_A)), Node(ptr(MESH_B))])
            } else {
                Some(Vec::new())
            }
        }
        fn class(&self, node: Node) -> &'static str {
            class_of(node.0 as i32 - 0x9000)
        }
        fn id(&self, node: Node) -> Option<i32> {
            Some(node.0 as i32 - 0x9000)
        }
        fn origin(&self, _node: Node) -> Option<[f32; 3]> {
            None
        }
        fn mesh_slots(&self, _node: Node) -> Option<(i32, i32)> {
            None
        }
        fn world(&self, _doc: Doc) -> Option<Node> {
            Some(WORLD)
        }
    }

    impl Selection for Fake {
        fn current_set(&self, sel: usize) -> usize {
            assert_eq!(sel, SEL);
            let mode = self.st.borrow().mode;
            if (0..8).contains(&mode) {
                self.set_of(mode)
            } else {
                0
            }
        }
        fn get_set(&self, sel: usize, mode: i32) -> usize {
            assert_eq!(sel, SEL);
            assert!((0..8).contains(&mode), "GET_SET has no bounds check");
            self.set_of(mode)
        }
        fn count(&self, _sel: usize) -> i32 {
            let st = self.st.borrow();
            assert!((0..8).contains(&st.mode), "COUNT calls through a null set for mode > 7");
            self.count_now(&st) as i32
        }
        fn selected_nodes(&self, _sel: usize, alloc: usize) -> Vec<Node> {
            let st = self.st.borrow();
            assert!((0..8).contains(&st.mode), "GET_NODES with a null set");
            assert!(alloc >= self.count_now(&st), "GET_NODES alloc >= COUNT");
            // It assigns the set's nodes in modes 0..=2 and leaves size 0 in modes 3..=7.
            match st.mode {
                0..=2 => st.nodes[st.mode as usize].iter().map(|&p| Node(p)).collect(),
                _ => Vec::new(),
            }
        }
        fn selected_meshes(&self, set: usize, alloc: usize) -> Vec<Node> {
            assert_eq!(set, FACESET);
            let st = self.st.borrow();
            assert!(alloc >= self.count_now(&st), "GET_MESHES alloc >= COUNT");
            st.faces.iter().filter(|(_, f)| !f.is_empty()).map(|(m, _)| Node(*m)).collect()
        }
        fn selected_faces(&self, set: usize, mesh: Node, alloc: usize) -> Vec<MeshHandle> {
            assert_eq!(set, FACESET);
            let st = self.st.borrow();
            assert_eq!(st.mode, MODE_FACES, "COUNT is read in face mode");
            assert!(alloc >= self.count_now(&st), "GET_FACES alloc >= COUNT");
            let id = mesh.0 as i32 - 0x9000;
            st.faces
                .iter()
                .find(|(m, _)| *m == mesh.0)
                .map(|(_, f)| f.iter().map(|&i| handle(id, i)).collect())
                .unwrap_or_default()
        }
        fn set_mode(&self, sel: usize, mode: i32) {
            assert_eq!(sel, SEL);
            assert!((0..8).contains(&mode), "SET_MODE has no bounds check");
            let mut st = self.st.borrow_mut();
            assert!((0..8).contains(&st.mode), "SET_MODE reads the old set unchecked");
            st.calls.push(format!("set_mode {mode}"));
            // convert = 0: nothing is converted, the target set keeps its stale content.
            st.mode = mode;
        }
        fn select_nodes(&self, sel: usize, nodes: &[Node], op: i32) {
            assert_eq!(sel, SEL);
            assert!((1..=4).contains(&op));
            let mut st = self.st.borrow_mut();
            assert!((0..8).contains(&st.mode), "SELECT_NODES calls through a null set");
            st.calls.push(format!(
                "select_nodes op={op} [{}]",
                nodes.iter().map(|n| (n.0 as i32 - 0x9000).to_string()).collect::<Vec<_>>().join(",")
            ));
            let accepted: Vec<usize> =
                nodes.iter().map(|n| n.0).filter(|p| !self.locked.contains(p)).collect();
            let mode = st.mode;
            if mode == MODE_FACES {
                // Only the empty replace is ever sent here: it clears the face set.
                assert!(nodes.is_empty() && op == 1, "nodes are never selected in face mode");
                st.faces.clear();
                return;
            }
            assert!((0..8).contains(&mode));
            let set = &mut st.nodes[mode as usize];
            match op {
                1 => *set = accepted,
                2 => {
                    for p in accepted {
                        if !set.contains(&p) {
                            set.push(p);
                        }
                    }
                }
                3 => set.retain(|p| !accepted.contains(p)),
                _ => unreachable!("toggle is never sent"),
            }
        }
        fn select_faces(&self, set: usize, mesh: Node, faces: &[MeshHandle], op: i32) {
            assert_eq!(set, FACESET);
            let id = mesh.0 as i32 - 0x9000;
            let idx: Vec<u32> = faces.iter().map(|h| handle_key(h) - id as u32 * 100).collect();
            let mut st = self.st.borrow_mut();
            st.calls.push(format!("select_faces {id} op={op} {idx:?}"));
            assert!(op == 2 || op == 3, "face op 1 is never sent: a deaf mesh would skip its clear");
            if self.face_deaf.contains(&mesh.0) {
                return;
            }
            if !st.faces.iter().any(|(m, _)| *m == mesh.0) {
                st.faces.push((mesh.0, Vec::new()));
            }
            let entry = st.faces.iter_mut().find(|(m, _)| *m == mesh.0).unwrap();
            if op == 2 {
                for i in idx {
                    if !entry.1.contains(&i) {
                        entry.1.push(i);
                    }
                }
            } else {
                entry.1.retain(|i| !idx.contains(i));
            }
        }
        fn mesh_face_count(&self, mesh: Node) -> i32 {
            face_slots(mesh.0 as i32 - 0x9000)
        }
        fn face_handles(&self, mesh: Node) -> Option<Vec<MeshHandle>> {
            if self.collect_fails {
                return None;
            }
            let id = mesh.0 as i32 - 0x9000;
            Some((0..face_slots(id) as u32).map(|i| handle(id, i)).collect())
        }
    }

    fn get(fake: &Fake) -> String {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op_get(fake, Doc(0x4000), &mut out).expect("get");
        String::from_utf8(out.as_cstr().to_vec()).unwrap()
    }

    fn set(fake: &Fake, params: &str) -> Result<serde_json::Value, OpErr> {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op_set(fake, Doc(0x4000), Some(params), &mut out)?;
        Ok(serde_json::from_slice(out.as_cstr()).expect("a JSON reply"))
    }

    fn refused(fake: &Fake, params: &str) -> OpErr {
        let before = fake.take();
        assert!(before.is_empty());
        let err = set(fake, params).expect_err("refusal");
        assert!(fake.take().is_empty(), "a refusal makes no Hammer call: {params}");
        assert_eq!(err.code, -32602, "{params}: {err:?}");
        err
    }

    // ---- get ----------------------------------------------------------------------------

    #[test]
    fn get_reports_each_node_mode() {
        for (mode, name) in [(0, "groups"), (1, "objects"), (2, "meshes")] {
            let f = Fake::with_nodes(mode, &[ENTITY, MESH_A]);
            assert_eq!(
                get(&f),
                format!(r#"{{"mode":"{name}","count":2,"unmapped":0,"nodeIDs":[10,11],"faces":[]}}"#)
            );
        }
        // GET_NODES leaves size 0 in modes 3..=7: the count is reported, no ids.
        let f = Fake::with_nodes(3, &[MESH_B]);
        assert_eq!(get(&f), r#"{"mode":"vertices","count":1,"unmapped":0,"nodeIDs":[],"faces":[]}"#);
        let f = Fake::with_nodes(6, &[MESH_B]);
        assert_eq!(get(&f), r#"{"mode":"mixed","count":1,"unmapped":0,"nodeIDs":[],"faces":[]}"#);
    }

    #[test]
    fn get_in_an_unknown_mode_is_null_and_empty_without_calling_hammer_further() {
        let f = Fake::with_nodes(-1, &[ENTITY]);
        assert_eq!(get(&f), r#"{"mode":null,"count":0,"unmapped":0,"nodeIDs":[],"faces":[]}"#);
    }

    #[test]
    fn get_in_face_mode_lists_sorted_indices_per_mesh() {
        let f = Fake::with_faces(&[(MESH_B, &[2, 0]), (MESH_A, &[3, 1])]);
        assert_eq!(
            get(&f),
            r#"{"mode":"faces","count":4,"unmapped":0,"nodeIDs":[12,11],"faces":[{"nodeID":12,"faces":[0,2]},{"nodeID":11,"faces":[1,3]}]}"#
        );
    }

    #[test]
    fn get_with_nothing_selected_is_empty() {
        let f = Fake::new(1);
        assert_eq!(get(&f), r#"{"mode":"objects","count":0,"unmapped":0,"nodeIDs":[],"faces":[]}"#);
        let f = Fake::new(5);
        assert_eq!(get(&f), r#"{"mode":"faces","count":0,"unmapped":0,"nodeIDs":[],"faces":[]}"#);
    }

    #[test]
    fn get_without_a_selection_object_is_an_internal_error() {
        let mut f = Fake::new(1);
        f.no_selection = true;
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        let err = op_get(&f, Doc(0x4000), &mut out).unwrap_err();
        assert_eq!(err.code, -32603);
    }

    // ---- set: nodes -----------------------------------------------------------------------

    #[test]
    fn set_replace_nodes_in_objects_mode_makes_one_call_and_reports_applied() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        let r = set(&f, r#"{"op":"replace","nodeIDs":[11,12]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_nodes op=1 [11,12]"]);
        assert_eq!(r["requested"], serde_json::json!({"nodeIDs":2}));
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([11, 12]));
        assert_eq!(r["applied"]["mode"], "objects");
        assert_eq!(r["modeChanged"], false);
        assert_eq!(r["undoGroupOpened"], false);
    }

    #[test]
    fn set_add_and_remove_nodes_use_ops_2_and_3() {
        let f = Fake::with_nodes(2, &[MESH_A]);
        let r = set(&f, r#"{"op":"add","nodeIDs":[12]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_nodes op=2 [12]"]);
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([11, 12]));
        let r = set(&f, r#"{"op":"remove","nodeIDs":[11]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_nodes op=3 [11]"]);
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([12]));
    }

    #[test]
    fn set_nodes_switches_to_objects_unless_already_in_modes_0_to_2() {
        for mode in [3, 4, 5, 6, 7] {
            let f = Fake::with_nodes(mode, &[]);
            let r = set(&f, r#"{"op":"replace","nodeIDs":[10]}"#).unwrap();
            assert_eq!(f.take(), vec!["set_mode 1".to_string(), "select_nodes op=1 [10]".into()]);
            assert_eq!(r["modeChanged"], true, "mode {mode}");
        }
        // The switched-to set is stale (no conversion): add/remove first clear it, so they are
        // relative to an empty selection.
        let f = Fake::with_nodes(3, &[]);
        f.st.borrow_mut().nodes[1] = vec![ptr(ENTITY), ptr(GROUP)];
        let r = set(&f, r#"{"op":"add","nodeIDs":[11]}"#).unwrap();
        assert_eq!(
            f.take(),
            vec![
                "set_mode 1".to_string(),
                "select_nodes op=1 []".into(),
                "select_nodes op=2 [11]".into()
            ]
        );
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([11]));
        let f = Fake::with_nodes(4, &[]);
        f.st.borrow_mut().nodes[1] = vec![ptr(ENTITY)];
        let r = set(&f, r#"{"op":"remove","nodeIDs":[11]}"#).unwrap();
        assert_eq!(r["applied"]["count"], 0);
        for mode in [0, 1, 2] {
            let f = Fake::with_nodes(mode, &[]);
            let r = set(&f, r#"{"op":"add","nodeIDs":[10]}"#).unwrap();
            assert_eq!(f.take(), vec!["select_nodes op=2 [10]"]);
            assert_eq!(r["modeChanged"], false);
        }
    }

    #[test]
    fn set_clear_is_an_empty_replace_in_any_mode_without_a_mode_switch() {
        for mode in [1, 5] {
            let f = if mode == 5 {
                Fake::with_faces(&[(MESH_A, &[0])])
            } else {
                Fake::with_nodes(mode, &[ENTITY])
            };
            let r = set(&f, r#"{"op":"clear"}"#).unwrap();
            assert_eq!(f.take(), vec!["select_nodes op=1 []"]);
            assert_eq!(r["requested"], serde_json::json!({}));
            assert_eq!(r["applied"]["count"], 0);
            assert_eq!(r["modeChanged"], false);
        }
    }

    #[test]
    fn clear_empties_only_the_current_modes_set() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        f.st.borrow_mut().nodes[2] = vec![ptr(MESH_A)];
        set(&f, r#"{"op":"clear"}"#).unwrap();
        let st = f.st.borrow();
        assert!(st.nodes[1].is_empty());
        assert_eq!(st.nodes[2], vec![ptr(MESH_A)], "other modes keep their content");
    }

    #[test]
    fn set_dedupes_node_ids_and_reports_filtering_as_applied_below_requested() {
        let mut f = Fake::with_nodes(1, &[]);
        f.locked = vec![ptr(MESH_B)];
        let r = set(&f, r#"{"op":"replace","nodeIDs":[11,12,11]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_nodes op=1 [11,12]"]);
        assert_eq!(r["requested"], serde_json::json!({"nodeIDs":2}));
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([11]));
        assert_eq!(r["applied"]["count"], 1);
    }

    #[test]
    fn an_unknown_mode_refuses_every_set_without_a_hammer_call() {
        for mode in [-1, 8] {
            for params in [
                r#"{"op":"clear"}"#,
                r#"{"op":"replace","nodeIDs":[10]}"#,
                r#"{"op":"add","nodeIDs":[10]}"#,
                r#"{"op":"replace","faces":[{"nodeID":11,"faces":[0]}]}"#,
                r#"{"op":"remove","faces":[{"nodeID":11,"faces":[0]}]}"#,
            ] {
                let f = Fake::with_nodes(mode, &[]);
                let err = set(&f, params).unwrap_err();
                assert_eq!(err.code, -32603, "{params}");
                assert_eq!(err.data, r#"{"reason":"selection mode unknown"}"#);
                assert!(f.take().is_empty(), "mode {mode} {params}");
                assert_eq!(f.st.borrow().mode, mode);
            }
            // get: the mode read only (the Fake panics on COUNT / GET_*)
            let f = Fake::with_nodes(mode, &[]);
            assert_eq!(
                get(&f),
                r#"{"mode":null,"count":0,"unmapped":0,"nodeIDs":[],"faces":[]}"#
            );
        }
    }

    #[test]
    fn unmappable_selected_items_are_counted_not_dropped_silently() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        f.st.borrow_mut().nodes[1].push(0xdead);
        assert_eq!(
            get(&f),
            r#"{"mode":"objects","count":2,"unmapped":1,"nodeIDs":[10],"faces":[]}"#
        );
        // a selected mesh whose faces cannot be collected is skipped, the rest still reported
        let mut f = Fake::with_faces(&[(MESH_A, &[0]), (MESH_B, &[1])]);
        f.collect_fails = true;
        assert_eq!(
            get(&f),
            r#"{"mode":"faces","count":0,"unmapped":2,"nodeIDs":[],"faces":[]}"#
        );
    }

    #[test]
    fn a_mesh_without_face_slots_is_out_of_range_not_internal() {
        // MESH_A reports zero face slots through a thin wrapper host
        let f = Fake::with_nodes(1, &[]);
        // MESH_A with zero slots through the resolver directly
        struct Zero<'a>(&'a Fake);
        impl Chain for Zero<'_> {
            fn module_reason(&self) -> Option<&'static str> { self.0.module_reason() }
            fn app(&self) -> Option<usize> { self.0.app() }
            fn session(&self, a: usize) -> Option<usize> { self.0.session(a) }
            fn doc_session(&self, s: usize) -> Option<usize> { self.0.doc_session(s) }
            fn owner(&self, d: usize) -> usize { self.0.owner(d) }
            fn doc_of(&self, d: usize) -> usize { self.0.doc_of(d) }
            fn doc_backptr(&self, d: usize) -> usize { self.0.doc_backptr(d) }
            fn is_of(&self, o: usize, v: u32) -> bool { self.0.is_of(o, v) }
            fn root(&self, d: usize) -> usize { self.0.root(d) }
            fn active_flag(&self, d: usize) -> bool { self.0.active_flag(d) }
            fn selection(&self, d: usize) -> usize { self.0.selection(d) }
            fn scope_depth(&self, d: usize) -> u32 { self.0.scope_depth(d) }
            fn undo_history(&self, d: usize) -> usize { self.0.undo_history(d) }
            fn active_history(&self) -> usize { self.0.active_history() }
        }
        impl Nodes for Zero<'_> {
            fn children(&self, n: Node) -> Option<Vec<Node>> { self.0.children(n) }
            fn class(&self, n: Node) -> &'static str { self.0.class(n) }
            fn id(&self, n: Node) -> Option<i32> { self.0.id(n) }
            fn origin(&self, n: Node) -> Option<[f32; 3]> { self.0.origin(n) }
            fn mesh_slots(&self, n: Node) -> Option<(i32, i32)> { self.0.mesh_slots(n) }
            fn world(&self, d: Doc) -> Option<Node> { self.0.world(d) }
        }
        impl Selection for Zero<'_> {
            fn current_set(&self, s: usize) -> usize { self.0.current_set(s) }
            fn get_set(&self, s: usize, m: i32) -> usize { self.0.get_set(s, m) }
            fn count(&self, s: usize) -> i32 { self.0.count(s) }
            fn selected_nodes(&self, s: usize, a: usize) -> Vec<Node> { self.0.selected_nodes(s, a) }
            fn selected_meshes(&self, s: usize, a: usize) -> Vec<Node> { self.0.selected_meshes(s, a) }
            fn selected_faces(&self, s: usize, m: Node, a: usize) -> Vec<MeshHandle> { self.0.selected_faces(s, m, a) }
            fn set_mode(&self, s: usize, m: i32) { self.0.set_mode(s, m) }
            fn select_nodes(&self, s: usize, n: &[Node], o: i32) { self.0.select_nodes(s, n, o) }
            fn select_faces(&self, s: usize, m: Node, f: &[MeshHandle], o: i32) { self.0.select_faces(s, m, f, o) }
            fn mesh_face_count(&self, _m: Node) -> i32 { 0 }
            fn face_handles(&self, _m: Node) -> Option<Vec<MeshHandle>> { panic!("no slots, no collect") }
        }
        let z = Zero(&f);
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        let err = op_set(&z, Doc(0x4000), Some(r#"{"op":"add","faces":[{"nodeID":11,"faces":[0]}]}"#), &mut out)
            .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.data.contains(r#""faceCount":0"#) && err.data.contains("out of range"), "{}", err.data);
        assert!(f.take().is_empty());
    }

    #[test]
    fn a_huge_face_selection_is_truncated_not_overflowed() {
        let s2 = super::State {
            mode: Some(5),
            count: 20_000,
            truncated: true,
            faces: vec![(11, (0..MAX_LISTED as u32).collect())],
            ..super::State::default()
        };
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        state_fields_into(&mut out, &s2);
        assert!(!out.ovf());
        assert!(String::from_utf8_lossy(out.as_cstr()).contains(r#""truncated":true"#));
    }

    #[test]
    fn set_nodes_refusals_leave_hammer_untouched() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        let err = refused(&f, r#"{"op":"replace","nodeIDs":[10,99,98]}"#);
        assert!(err.data.contains(r#""unknown":[99,98]"#), "{}", err.data);
        for bad in [
            r#"{"nodeIDs":[10]}"#,
            r#"{"op":"toggle","nodeIDs":[10]}"#,
            r#"{"op":"replace"}"#,
            r#"{"op":"replace","nodeIDs":[10],"faces":[{"nodeID":11,"faces":[0]}]}"#,
            r#"{"op":"replace","nodeIDs":[]}"#,
            r#"{"op":"replace","nodeIDs":[-1]}"#,
            r#"{"op":"replace","nodeIDs":[1.5]}"#,
            r#"{"op":"replace","nodeIDs":["10"]}"#,
            r#"{"op":"replace","nodeIDs":[2147483648]}"#,
            r#"{"op":"replace","nodeIDs":10}"#,
            r#"{"op":"clear","nodeIDs":[10]}"#,
            r#"{"op":"clear","faces":[]}"#,
            r#"[1]"#,
            r#"nope"#,
        ] {
            refused(&f, bad);
        }
        assert_eq!(get(&f), r#"{"mode":"objects","count":1,"unmapped":0,"nodeIDs":[10],"faces":[]}"#);
    }

    #[test]
    fn set_bounds_4096_ids_and_65536_indices() {
        let f = Fake::with_nodes(1, &[]);
        let ids: Vec<String> = (0..4097).map(|_| "10".to_string()).collect();
        refused(&f, &format!(r#"{{"op":"add","nodeIDs":[{}]}}"#, ids.join(",")));
        let ids: Vec<String> = (0..4096).map(|_| "10".to_string()).collect();
        set(&f, &format!(r#"{{"op":"add","nodeIDs":[{}]}}"#, ids.join(","))).unwrap();
        assert_eq!(f.take(), vec!["select_nodes op=2 [10]"]);
        let idx: Vec<String> = (0..65_537).map(|_| "0".to_string()).collect();
        refused(
            &f,
            &format!(r#"{{"op":"add","faces":[{{"nodeID":11,"faces":[{}]}}]}}"#, idx.join(",")),
        );
        // 65536 in total across entries is fine to parse (and then out of range here).
        let idx: Vec<String> = (0..65_536).map(|_| "0".to_string()).collect();
        assert!(parse_set(Some(&format!(
            r#"{{"op":"add","faces":[{{"nodeID":11,"faces":[{}]}}]}}"#,
            idx.join(",")
        )))
        .is_ok());
    }

    // ---- set: faces -----------------------------------------------------------------------

    #[test]
    fn set_faces_replace_clears_with_an_empty_node_replace_then_adds_per_mesh() {
        let f = Fake::with_nodes(1, &[]);
        let r = set(
            &f,
            r#"{"op":"replace","faces":[{"nodeID":12,"faces":[2,0]},{"nodeID":11,"faces":[3]}]}"#,
        )
        .unwrap();
        assert_eq!(
            f.take(),
            vec![
                "set_mode 5".to_string(),
                "select_nodes op=1 []".into(),
                "select_faces 12 op=2 [0, 2]".into(),
                "select_faces 11 op=2 [3]".into()
            ]
        );
        assert_eq!(r["requested"], serde_json::json!({"meshes":2,"faces":3}));
        assert_eq!(r["modeChanged"], true);
        assert_eq!(r["applied"]["mode"], "faces");
        assert_eq!(r["applied"]["count"], 3);
        assert_eq!(
            r["applied"]["faces"],
            serde_json::json!([{"nodeID":12,"faces":[0,2]},{"nodeID":11,"faces":[3]}])
        );
    }

    #[test]
    fn set_faces_add_and_remove_in_face_mode_do_not_switch_modes() {
        let f = Fake::with_faces(&[(MESH_A, &[0])]);
        let r = set(&f, r#"{"op":"add","faces":[{"nodeID":11,"faces":[1,2]}]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_faces 11 op=2 [1, 2]"]);
        assert_eq!(r["modeChanged"], false);
        assert_eq!(r["applied"]["faces"], serde_json::json!([{"nodeID":11,"faces":[0,1,2]}]));
        let r = set(&f, r#"{"op":"remove","faces":[{"nodeID":11,"faces":[1]}]}"#).unwrap();
        assert_eq!(f.take(), vec!["select_faces 11 op=3 [1]"]);
        assert_eq!(r["applied"]["faces"], serde_json::json!([{"nodeID":11,"faces":[0,2]}]));
    }

    #[test]
    fn set_faces_after_a_switch_start_from_an_empty_set() {
        // No conversion: the mesh selected in mode 2 does not become faces, and the face set's
        // stale content is cleared by add/remove too.
        let f = Fake::with_nodes(2, &[MESH_A]);
        f.st.borrow_mut().faces = vec![(ptr(MESH_B), vec![0, 1])];
        let r = set(&f, r#"{"op":"add","faces":[{"nodeID":12,"faces":[2]}]}"#).unwrap();
        assert_eq!(
            f.take(),
            vec![
                "set_mode 5".to_string(),
                "select_nodes op=1 []".into(),
                "select_faces 12 op=2 [2]".into()
            ]
        );
        assert_eq!(r["modeChanged"], true);
        assert_eq!(r["applied"]["faces"], serde_json::json!([{"nodeID":12,"faces":[2]}]));
        let f = Fake::with_nodes(2, &[MESH_A]);
        let r = set(&f, r#"{"op":"remove","faces":[{"nodeID":12,"faces":[2]}]}"#).unwrap();
        assert_eq!(r["applied"]["count"], 0);
    }

    #[test]
    fn a_filtered_first_mesh_does_not_stop_the_replace_from_clearing() {
        let mut f = Fake::with_faces(&[(MESH_B, &[0, 1])]);
        f.face_deaf = vec![ptr(MESH_A)];
        let r = set(
            &f,
            r#"{"op":"replace","faces":[{"nodeID":11,"faces":[1]},{"nodeID":12,"faces":[2]}]}"#,
        )
        .unwrap();
        assert_eq!(r["applied"]["faces"], serde_json::json!([{"nodeID":12,"faces":[2]}]));
        assert_eq!(r["requested"], serde_json::json!({"meshes":2,"faces":2}));
    }

    #[test]
    fn set_faces_merges_duplicate_entries_and_reports_filtering() {
        let mut f = Fake::with_faces(&[]);
        f.face_deaf = vec![ptr(MESH_B)];
        let r = set(
            &f,
            r#"{"op":"replace","faces":[{"nodeID":11,"faces":[1,1]},{"nodeID":12,"faces":[0]},{"nodeID":11,"faces":[0]}]}"#,
        )
        .unwrap();
        assert_eq!(
            f.take(),
            vec![
                "select_nodes op=1 []".to_string(),
                "select_faces 11 op=2 [0, 1]".into(),
                "select_faces 12 op=2 [0]".into()
            ]
        );
        assert_eq!(r["requested"], serde_json::json!({"meshes":2,"faces":3}));
        assert_eq!(r["applied"]["count"], 2);
        assert_eq!(r["applied"]["nodeIDs"], serde_json::json!([11]));
        assert_eq!(r["modeChanged"], false);
    }

    #[test]
    fn set_faces_refusals_leave_hammer_untouched() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        let err = refused(&f, r#"{"op":"add","faces":[{"nodeID":11,"faces":[0,4,9]}]}"#);
        assert!(err.data.contains(r#""faceCount":4"#) && err.data.contains(r#""badIndices":[4,9]"#));
        assert!(err.data.contains(r#""nodeID":11"#));
        let err = refused(&f, r#"{"op":"add","faces":[{"nodeID":10,"faces":[0]}]}"#);
        assert!(err.data.contains("not a mesh"));
        let err = refused(&f, r#"{"op":"add","faces":[{"nodeID":77,"faces":[0]}]}"#);
        assert!(err.data.contains(r#""unknown":[77]"#));
        // A valid first entry does not get applied when a later one is bad.
        refused(
            &f,
            r#"{"op":"replace","faces":[{"nodeID":11,"faces":[0]},{"nodeID":12,"faces":[3]}]}"#,
        );
        for bad in [
            r#"{"op":"add","faces":[]}"#,
            r#"{"op":"add","faces":[{"nodeID":11,"faces":[]}]}"#,
            r#"{"op":"add","faces":[{"nodeID":11,"faces":[-1]}]}"#,
            r#"{"op":"add","faces":[{"nodeID":11,"faces":[0.5]}]}"#,
            r#"{"op":"add","faces":[{"faces":[0]}]}"#,
            r#"{"op":"add","faces":[{"nodeID":11}]}"#,
            r#"{"op":"add","faces":[5]}"#,
            r#"{"op":"add","faces":{"nodeID":11,"faces":[0]}}"#,
        ] {
            refused(&f, bad);
        }
        assert!(get(&f).contains("objects"));
    }

    #[test]
    fn a_failed_collect_is_an_internal_error_without_a_hammer_call() {
        let mut f = Fake::with_nodes(1, &[]);
        f.collect_fails = true;
        let err = set(&f, r#"{"op":"add","faces":[{"nodeID":11,"faces":[0]}]}"#).unwrap_err();
        assert_eq!(err.code, -32603);
        assert!(f.take().is_empty());
    }

    #[test]
    fn the_mode_names_cover_hammers_eight_modes() {
        assert_eq!(MODE_NAMES[MODE_OBJECTS as usize], "objects");
        assert_eq!(MODE_NAMES[MODE_FACES as usize], "faces");
    }

    // ---- dispatch -------------------------------------------------------------------------

    #[test]
    fn dispatch_get_wraps_the_envelope_and_set_appends_the_doc_tail() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        let reply = get_dispatch(&f, "g1", &mut out);
        assert_eq!(
            reply,
            crate::control::result_ok(
                Some("g1"),
                r#"{"mode":"objects","count":1,"unmapped":0,"nodeIDs":[10],"faces":[]}"#
            )
        );
        let (seq, epoch) = crate::ops::hammer::fingerprint_ids();
        let params = format!(
            r#"{{"op":"clear","expectDoc":{{"id":"d{seq}","epoch":{epoch}}}}}"#
        );
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        let reply = set_dispatch(&f, "s1", Some(&params), &mut out);
        assert!(reply.contains(r#""undoGroupOpened":false,"doc":{"id":"d"#), "{reply}");
        assert_eq!(f.take(), vec!["select_nodes op=1 []"]);
    }

    #[test]
    fn dispatch_set_checks_expect_doc_before_anything() {
        let f = Fake::with_nodes(1, &[ENTITY]);
        let (seq, epoch) = crate::ops::hammer::fingerprint_ids();
        for params in [
            r#"{"op":"clear"}"#.to_string(),
            format!(r#"{{"op":"clear","expectDoc":{{"id":"d{seq}"}}}}"#),
            format!(
                r#"{{"op":"clear","expectDoc":{{"id":"d{}","epoch":{epoch}}}}}"#,
                seq.wrapping_add(7)
            ),
        ] {
            let mut buf = vec![0u8; OP_RESULT_CAP];
            let mut out = Sb::new(&mut buf);
            let reply = set_dispatch(&f, "s2", Some(&params), &mut out);
            assert!(reply.contains("-32004"), "{reply}");
            assert!(f.take().is_empty());
        }
    }

    #[test]
    fn a_selection_less_document_is_not_editable() {
        let mut f = Fake::with_nodes(1, &[ENTITY]);
        f.no_selection = true;
        let (seq, epoch) = crate::ops::hammer::fingerprint_ids();
        let params = format!(r#"{{"op":"clear","expectDoc":{{"id":"d{seq}","epoch":{epoch}}}}}"#);
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        let reply = set_dispatch(&f, "s3", Some(&params), &mut out);
        assert!(reply.contains("-32018"), "{reply}");
    }

    #[test]
    fn the_rows_are_registered_implemented_and_gated_on_hammer() {
        for name in ["selection.get", "selection.set"] {
            let row = crate::registry::find(name).unwrap();
            assert!(row.implemented && row.blocked_on.is_none());
            assert_eq!(row.needs, crate::registry::NEED_HAMMER);
            assert!(crate::ops::lookup(name).is_some());
        }
    }
}
