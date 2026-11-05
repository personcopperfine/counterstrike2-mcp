//! The mock's selection model (bridge m3.12 `selection.get` / `selection.set`).
//!
//! Wire-level, not pointer-level: the model holds node ids and face indices, and follows what the
//! Rust bridge (`cs2-bridge` `ops::selection`) does to Hammer. It is off unless
//! [`HammerWorld::enable_selection`] ran (`MockBuilder::hammer_selection`), in which case the two
//! methods are advertised and served; otherwise the capability is absent.
//!
//! * Eight modes (0 groups, 1 objects, 2 meshes, 3 vertices, 4 edges, 5 faces, 6 mixed, 7 tile),
//!   each with its own node set; the face set (mode 5) is `(mesh, indices)`.
//! * A mode switch converts nothing: the target set keeps its stale content, so the bridge clears
//!   it before applying an `add`/`remove`.
//! * Filtering knobs: `filter_nodes` are refused by node selection (locked/hidden), and
//!   `deaf_meshes` ignore face selection. Neither is an error.
//! * No undo group, no dirty: the reply says `undoGroupOpened:false`.
use serde_json::{json, Value};

use super::{err, expect_doc_ok_for_selection, invalid_params, with_doc, HammerWorld};

/// The model's `(mode, current-mode nodes, faces)` as [`HammerWorld::selection_state`] reports it.
pub type SelectionState = (i32, Vec<i64>, Vec<(i64, Vec<usize>)>);

const MODE_NAMES: [&str; 8] =
    ["groups", "objects", "meshes", "vertices", "edges", "faces", "mixed", "tile"];

/// The model's state.
#[derive(Debug, Clone, Default)]
pub(crate) struct SelectionModel {
    /// Hammer's current mode (`-1`: outside 0..=7, reported as `null`).
    pub mode: i32,
    /// Per-mode node ids (index 5 unused).
    pub nodes: [Vec<i64>; 8],
    /// The face set: `(mesh node id, selected face indices)`.
    pub faces: Vec<(i64, Vec<usize>)>,
    /// Nodes node-selection refuses.
    pub filter_nodes: Vec<i64>,
    /// Meshes face-selection ignores.
    pub deaf_meshes: Vec<i64>,
}

impl SelectionModel {
    fn new() -> Self {
        SelectionModel { mode: 1, ..Default::default() }
    }
}

impl HammerWorld {
    /// Turn the selection model on (objects mode, nothing selected).
    pub fn enable_selection(&mut self) {
        self.selection = Some(SelectionModel::new());
    }

    /// Test seam: Hammer's UI selected these nodes in `mode` (and made it the current mode).
    pub fn ui_select_nodes(&mut self, mode: i32, ids: &[i64]) {
        if let Some(sel) = self.selection.as_mut() {
            sel.mode = mode;
            sel.nodes[mode as usize] = ids.to_vec();
        }
    }

    /// Test seam: Hammer's UI selected these faces (face mode becomes current).
    pub fn ui_select_faces(&mut self, picks: &[(i64, Vec<usize>)]) {
        if let Some(sel) = self.selection.as_mut() {
            sel.mode = 5;
            sel.faces = picks.to_vec();
        }
    }

    /// Test seam: nodes node-selection refuses and meshes face-selection ignores.
    pub fn selection_filters(&mut self, nodes: &[i64], meshes: &[i64]) {
        if let Some(sel) = self.selection.as_mut() {
            sel.filter_nodes = nodes.to_vec();
            sel.deaf_meshes = meshes.to_vec();
        }
    }

    /// The model's `(mode, current-mode nodes, faces)`.
    pub fn selection_state(&self) -> Option<SelectionState> {
        self.selection.as_ref().map(|s| {
            let nodes = if (0..8).contains(&s.mode) && s.mode != 5 {
                s.nodes[s.mode as usize].clone()
            } else {
                Vec::new()
            };
            (s.mode, nodes, s.faces.clone())
        })
    }

    fn live_node(&self, id: i64) -> bool {
        self.nodes.iter().any(|n| n.node_id == id)
    }

    fn mesh_face_count(&self, id: i64) -> Option<usize> {
        let is_mesh = self.nodes.iter().any(|n| n.node_id == id && n.class == "mesh");
        if !is_mesh {
            return None;
        }
        self.meshes.get(&id).map(|m| m.faces.len())
    }
}

/// `selection.get`'s result (no gate beyond a document: the mock always has one).
pub(crate) fn op_get(w: &HammerWorld) -> Result<Value, Value> {
    let sel = w.selection.as_ref().expect("selection model enabled");
    Ok(state_json(sel))
}

fn state_json(sel: &SelectionModel) -> Value {
    let mode = (0..8).contains(&sel.mode).then(|| MODE_NAMES[sel.mode as usize]);
    let (count, ids, faces): (usize, Vec<i64>, Vec<Value>) = match sel.mode {
        5 => {
            let mut ids = Vec::new();
            let mut faces = Vec::new();
            let mut count = 0;
            for (mesh, picked) in &sel.faces {
                if picked.is_empty() {
                    continue;
                }
                let mut sorted = picked.clone();
                sorted.sort_unstable();
                sorted.dedup();
                count += sorted.len();
                ids.push(*mesh);
                faces.push(json!({"nodeID": mesh, "faces": sorted}));
            }
            (count, ids, faces)
        }
        // modes 3..=7 hold items the bridge cannot list as nodes: a count, no ids
        3 | 4 | 6 | 7 => (sel.nodes[sel.mode as usize].len(), Vec::new(), Vec::new()),
        m if (0..8).contains(&m) => {
            let set = &sel.nodes[m as usize];
            (set.len(), set.clone(), Vec::new())
        }
        _ => (0, Vec::new(), Vec::new()),
    };
    json!({"mode": mode, "count": count, "unmapped": 0, "nodeIDs": ids, "faces": faces})
}

enum Target {
    None,
    Nodes(Vec<i64>),
    Faces(Vec<(i64, Vec<usize>)>),
}

fn parse(params: &Value) -> Result<(String, Target), Value> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or("");
    if !["replace", "add", "remove", "clear"].contains(&op) {
        return Err(invalid_params("op: replace, add, remove or clear is required"));
    }
    let present = |k: &str| params.get(k).is_some_and(|v| !v.is_null());
    let (has_nodes, has_faces) = (present("nodeIDs"), present("faces"));
    if op == "clear" {
        if has_nodes || has_faces {
            return Err(invalid_params("clear takes neither nodeIDs nor faces"));
        }
        return Ok((op.into(), Target::None));
    }
    if has_nodes == has_faces {
        return Err(invalid_params("exactly one of nodeIDs or faces is required (clear takes neither)"));
    }
    if has_nodes {
        let list = params["nodeIDs"].as_array().filter(|l| !l.is_empty() && l.len() <= 4096);
        let Some(list) = list else {
            return Err(invalid_params("nodeIDs: 1..=4096 node ids"));
        };
        let mut ids: Vec<i64> = Vec::new();
        for v in list {
            match v.as_i64() {
                Some(id) if (0..=2_147_483_647).contains(&id) => {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                _ => return Err(invalid_params("nodeIDs: a non-negative int up to 2147483647")),
            }
        }
        return Ok((op.into(), Target::Nodes(ids)));
    }
    let list = params["faces"].as_array().filter(|l| !l.is_empty() && l.len() <= 4096);
    let Some(list) = list else {
        return Err(invalid_params("faces: 1..=4096 entries of {nodeID, faces:[i..]}"));
    };
    let mut total = 0usize;
    let mut entries: Vec<(i64, Vec<usize>)> = Vec::new();
    for entry in list {
        let id = entry.get("nodeID").and_then(Value::as_i64).filter(|i| (0..=2_147_483_647).contains(i));
        let idx = entry.get("faces").and_then(Value::as_array).filter(|l| !l.is_empty());
        let (Some(id), Some(idx)) = (id, idx) else {
            return Err(invalid_params("faces: each entry is {nodeID, faces:[i..]}"));
        };
        total += idx.len();
        if total > 65_536 {
            return Err(invalid_params("faces: at most 65536 indices per call"));
        }
        let mut set: Vec<usize> = Vec::new();
        for v in idx {
            match v.as_i64() {
                Some(i) if (0..=i64::from(i32::MAX)).contains(&i) => {
                    if !set.contains(&(i as usize)) {
                        set.push(i as usize);
                    }
                }
                _ => return Err(invalid_params("faces: indices are integers >= 0")),
            }
        }
        match entries.iter_mut().find(|(n, _)| *n == id) {
            Some((_, existing)) => {
                for i in set {
                    if !existing.contains(&i) {
                        existing.push(i);
                    }
                }
            }
            None => entries.push((id, set)),
        }
    }
    Ok((op.into(), Target::Faces(entries)))
}

/// `selection.set`: editable -> `expectDoc` (the shared mutator gate, run by the caller) ->
/// params -> validate everything -> apply -> reply with the document state.
pub(crate) fn op_set(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    expect_doc_ok_for_selection(w, params)?;
    let (op, target) = parse(params)?;
    // validate first
    let requested = match &target {
        Target::None => json!({}),
        Target::Nodes(ids) => {
            let unknown: Vec<i64> = ids.iter().copied().filter(|i| !w.live_node(*i)).collect();
            if !unknown.is_empty() {
                return Err(err(
                    -32602,
                    "InvalidParams",
                    json!({"reason": "unknown nodeIDs", "unknown": unknown, "unknownTotal": unknown.len()}),
                ));
            }
            json!({"nodeIDs": ids.len()})
        }
        Target::Faces(entries) => {
            let unknown: Vec<i64> =
                entries.iter().map(|e| e.0).filter(|i| !w.live_node(*i)).collect();
            if !unknown.is_empty() {
                return Err(err(
                    -32602,
                    "InvalidParams",
                    json!({"reason": "unknown nodeIDs", "unknown": unknown, "unknownTotal": unknown.len()}),
                ));
            }
            for (id, idx) in entries {
                let Some(count) = w.mesh_face_count(*id) else {
                    return Err(err(
                        -32602,
                        "InvalidParams",
                        json!({"reason": "not a mesh", "nodeID": id}),
                    ));
                };
                let bad: Vec<usize> = idx.iter().copied().filter(|i| *i >= count).collect();
                if !bad.is_empty() {
                    return Err(err(
                        -32602,
                        "InvalidParams",
                        json!({"reason": "face index out of range", "nodeID": id,
                               "faceCount": count, "badIndices": bad}),
                    ));
                }
            }
            let faces: usize = entries.iter().map(|e| e.1.len()).sum();
            json!({"meshes": entries.len(), "faces": faces})
        }
    };
    let sel = w.selection.as_mut().expect("selection model enabled");
    let before = sel.mode;
    if !(0..8).contains(&before) {
        // the bridge refuses every op in a mode it cannot name, before any Hammer call
        return Err(err(-32603, "InternalError", json!({"reason": "selection mode unknown"})));
    }
    let mut changed = false;
    match target {
        Target::None => clear_current(sel),
        Target::Nodes(ids) => {
            if !matches!(before, 0..=2) {
                sel.mode = 1;
                changed = true;
                if op != "replace" {
                    clear_current(sel);
                }
            }
            let accepted: Vec<i64> =
                ids.into_iter().filter(|i| !sel.filter_nodes.contains(i)).collect();
            let set = &mut sel.nodes[sel.mode as usize];
            match op.as_str() {
                "replace" => *set = accepted,
                "add" => {
                    for i in accepted {
                        if !set.contains(&i) {
                            set.push(i);
                        }
                    }
                }
                _ => set.retain(|i| !accepted.contains(i)),
            }
        }
        Target::Faces(entries) => {
            if before != 5 {
                sel.mode = 5;
                changed = true;
            }
            if op == "replace" || changed {
                clear_current(sel);
            }
            for (id, idx) in entries {
                if sel.deaf_meshes.contains(&id) {
                    continue;
                }
                if !sel.faces.iter().any(|(m, _)| *m == id) {
                    sel.faces.push((id, Vec::new()));
                }
                let entry = sel.faces.iter_mut().find(|(m, _)| *m == id).expect("just pushed");
                if op == "remove" {
                    entry.1.retain(|i| !idx.contains(i));
                } else {
                    for i in idx {
                        if !entry.1.contains(&i) {
                            entry.1.push(i);
                        }
                    }
                }
            }
        }
    }
    let applied = state_json(w.selection.as_ref().expect("selection model enabled"));
    Ok(with_doc(
        w,
        json!({
            "requested": requested,
            "applied": applied,
            "modeChanged": changed,
            "undoGroupOpened": false,
        }),
    ))
}

/// `SELECT_NODES(empty, op 1)`: clears only the current mode's set.
fn clear_current(sel: &mut SelectionModel) {
    if sel.mode == 5 {
        sel.faces.clear();
    } else if (0..8).contains(&sel.mode) {
        sel.nodes[sel.mode as usize].clear();
    }
}
