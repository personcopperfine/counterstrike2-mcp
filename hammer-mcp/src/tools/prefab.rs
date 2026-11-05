//! `prefab {action}`: reusable `.vmap` prefabs of the active addon (`maps/prefabs/<name>.vmap`),
//! made from live nodes and instanced into the open map (bridge m3.18 `node.prefabFromSelection` /
//! `node.createPrefab`; CS2 has no `func_instance`, a prefab is a `CMapPrefab` node).
//!
//! * `create {name, nodeIDs, centered?}`: select `nodeIDs` (`selection.set` replace), then
//!   `node.prefabFromSelection`. Hammer writes the prefab `.vmap` at once and **replaces the
//!   originals in the open document with the prefab node** (one undo group restores them; the
//!   written file stays). The tool creates `maps/prefabs/` when missing (the bridge refuses a
//!   missing directory) and refuses an existing name (`PrefabExists`) before any bridge call.
//! * `instance {name|path, origin, angles?}`: `node.createPrefab`, a `CMapPrefab` node in the open
//!   document; the file must exist (`PrefabNotFound{available}` otherwise). Instance the same file
//!   as often as you like.
//! * `list`: the `.vmap` files under `maps/prefabs/` of the active addon, with node counts.
//! * `read {name|path}`: one file through `src/vmap` (entities, meshes, nested prefab nodes).
//!
//! The bridge refuses a prefab whose target is the map open in Hammer (`-32602`). Nothing here
//! saves the open map: `save_map` after `create`/`instance`.
use super::entities_file::vmap_error;
use super::entities_read::need_live;
use super::live_common::{
    doc_id_of, expect_doc, mutator_failure, note_edit, reject_unknown_keys, require_document,
    MAX_NODE_ID,
};
use crate::state::HammerState;
use crate::vmap;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const ACTIONS: [&str; 4] = ["create", "instance", "list", "read"];
const KNOWN: [&str; 8] = ["action", "name", "path", "nodeIDs", "centered", "origin", "angles", "limit"];
const PREFAB_DIR: &str = "maps/prefabs";
const MAX_NODE_IDS: usize = 4096;
const NAME_MAX: usize = 64;
const ORIGIN_MAX_ABS: f64 = 1_048_576.0;
/// `list` parses at most this many files for their counts; the rest are listed without.
const LIST_COUNT_CAP: usize = 50;
const AVAILABLE_CAP: usize = 100;
const LIMIT_DEFAULT: usize = 200;
const LIMIT_MAX: usize = 2000;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "prefab",
            "Prefabs: .vmap files under maps/prefabs/ of the addon, instanced as prefab nodes. \
             create {name,nodeIDs,centered?} writes maps/prefabs/<name>.vmap from live nodes and REPLACES them in the open map with the prefab node (undo restores them, the file stays); \
             instance {name|path,origin,angles?} adds a node (twice = two copies); list; read. Not saved: save_map. Errors PrefabNotFound{available}, PrefabExists.",
            Mutating,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ACTIONS},
                "name": {"type": "string", "description": "[a-z0-9_-]{1,64}"},
                "path": {"type": "string", "description": "maps/....vmap (instance, read)"},
                "nodeIDs": {
                    "type": "array", "items": {"type": "integer", "minimum": 0, "maximum": MAX_NODE_ID},
                    "minItems": 1, "maxItems": MAX_NODE_IDS, "description": "create: nodes to use"
                },
                "centered": {"type": "boolean", "description": "create: recentre on the bounds"},
                "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "instance: [x,y,z]"},
                "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "instance: [pitch,yaw,roll]"},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIMIT_MAX, "description": "read: max items per list"}
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        prefab,
    );
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

// -------------------------------------------------------------------------------------------
// Names and paths
// -------------------------------------------------------------------------------------------

/// `name` -> its stem, or why it is refused: lowercase letters, digits, `_` and `-`, starting with a
/// letter or digit; no separators, no dots (so no `..`), at most [`NAME_MAX`] characters.
fn valid_name(name: &str) -> Result<&str, String> {
    let bytes = name.as_bytes();
    let ok_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
    if bytes.is_empty() || bytes.len() > NAME_MAX {
        return Err(format!("name must be 1..={NAME_MAX} characters, got {}", bytes.len()));
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) || !bytes.iter().copied().all(ok_char) {
        return Err(format!(
            "name {name:?} must use only lowercase letters, digits, '_' and '-' and start with a letter or digit \
             (no '/', '\\\\', '.', spaces or capitals: it becomes maps/prefabs/<name>.vmap)"
        ));
    }
    // Win32 (and so Wine) opens these as devices whatever the extension
    const DEVICES: &[&str] = &["con", "prn", "aux", "nul", "conin$", "conout$"];
    let device = DEVICES.contains(&name)
        || ((name.starts_with("com") || name.starts_with("lpt")) && name.len() == 4 && bytes[3].is_ascii_digit());
    if device {
        return Err(format!("name {name:?} is a Windows device name; pick another"));
    }
    Ok(name)
}

/// Whether `host` exists, compared without case like the Windows side does (an existing `Foo.vmap`
/// is the same file to Hammer as `foo.vmap`).
fn exists_any_case(host: &std::path::Path) -> bool {
    if host.exists() {
        return true;
    }
    let (Some(dir), Some(file)) = (host.parent(), host.file_name().and_then(|f| f.to_str())) else {
        return false;
    };
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(file)))
    })
}

fn name_rel(name: &str) -> String {
    format!("{PREFAB_DIR}/{name}.vmap")
}

/// An addon-relative map path (`maps/.../x.vmap`) as the bridge's own shape gate takes it.
fn valid_path(path: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("path {path:?}: {why} (addon-relative maps/....vmap, '/' separators)"));
    if path.contains('\\') || path.starts_with('/') || path.contains(':') {
        return bad("no backslash, drive or absolute path");
    }
    if !path.starts_with("maps/") || !path.ends_with(".vmap") {
        return bad("must start with maps/ and end with .vmap");
    }
    if path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return bad("no empty, '.' or '..' segments");
    }
    if !path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/' | b'.' | b' ')) {
        return bad("unsupported character");
    }
    Ok(path.to_string())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(invalid(format!("{key} must be a string, got {other}"))),
    }
}

/// `name` xor `path` -> `(addon-relative path, name when given by name)`.
fn target_arg(args: &Value) -> Result<(String, Option<String>), ToolError> {
    match (str_arg(args, "name")?, str_arg(args, "path")?) {
        (Some(_), Some(_)) => Err(invalid("give name or path, not both")),
        (None, None) => Err(invalid("name or path is required")),
        (Some(name), None) => {
            let name = valid_name(name).map_err(invalid)?;
            Ok((name_rel(name), Some(name.to_string())))
        }
        (None, Some(path)) => Ok((valid_path(path).map_err(invalid)?, None)),
    }
}

fn vec3_arg(args: &Value, key: &str) -> Result<Option<[f64; 3]>, ToolError> {
    let Some(value) = args.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    let list = value
        .as_array()
        .filter(|l| l.len() == 3)
        .ok_or_else(|| invalid(format!("{key} must be [x, y, z] numbers, got {value}")))?;
    let mut out = [0.0; 3];
    for (axis, item) in list.iter().enumerate() {
        out[axis] = item
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| invalid(format!("{key}[{axis}] must be a finite number, got {item}")))?;
    }
    Ok(Some(out))
}

/// Every prefab file of the addon (`maps/prefabs/**/*.vmap`), addon-relative, sorted.
fn prefab_files(addon_root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = format!("{rel}/{name}");
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && depth < 6 {
                walk(&entry.path(), &child, out, depth + 1);
            } else if kind.is_file() && name.ends_with(".vmap") {
                out.push(child);
            }
        }
    }
    let mut out = Vec::new();
    walk(&addon_root.join(PREFAB_DIR), PREFAB_DIR, &mut out, 0);
    out.sort();
    out
}

fn not_found(rel: &str, addon_root: &Path) -> ToolError {
    let all = prefab_files(addon_root);
    let available: Vec<&String> = all.iter().take(AVAILABLE_CAP).collect();
    let mut error = ToolError::new(
        "PrefabNotFound",
        "PrefabNotFound",
        format!(
            "{rel} does not exist in the active addon ({} prefab file(s) under {PREFAB_DIR}/)",
            all.len()
        ),
    )
    .with_data(json!({"path": rel, "available": available, "truncated": all.len() > AVAILABLE_CAP}))
    .retry_safe(true);
    if all.is_empty() {
        error = error.use_instead("prefab", "make one from live nodes first", Some(json!({"action": "create"})));
    } else {
        error = error.use_instead("prefab", "list the prefab files", Some(json!({"action": "list"})));
    }
    error
}

fn host_of(addon_root: &Path, rel: &str) -> PathBuf {
    addon_root.join(rel)
}

// -------------------------------------------------------------------------------------------
// The tool
// -------------------------------------------------------------------------------------------

pub async fn prefab(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    reject_unknown_keys(&args, &KNOWN)?;
    let action = match args.get("action").and_then(Value::as_str) {
        Some(a) if ACTIONS.contains(&a) => a,
        _ => return Err(invalid(format!("action is required: one of {ACTIONS:?}"))),
    };
    // keys that belong to another action are refused instead of ignored
    let only = |keys: &[&str]| -> Result<(), ToolError> {
        for key in KNOWN {
            if key != "action" && !keys.contains(&key) && args.get(key).is_some_and(|v| !v.is_null()) {
                return Err(invalid(format!("{key} is not an argument of prefab {action}")));
            }
        }
        Ok(())
    };
    match action {
        "create" => {
            only(&["name", "nodeIDs", "centered"])?;
            create(state, ctx, args).await
        }
        "instance" => {
            only(&["name", "path", "origin", "angles"])?;
            instance(state, ctx, args).await
        }
        "list" => {
            only(&["limit"])?;
            let limit = limit_of(&args)?;
            list(state, limit)
        }
        _ => {
            only(&["name", "path", "limit"])?;
            read(state, args)
        }
    }
}

fn node_ids_arg(args: &Value) -> Result<Vec<i64>, ToolError> {
    let list = args
        .get("nodeIDs")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| invalid("create needs nodeIDs: a non-empty array of the nodes to turn into the prefab"))?;
    if list.len() > MAX_NODE_IDS {
        return Err(invalid(format!("nodeIDs: at most {MAX_NODE_IDS} per call, got {}", list.len())));
    }
    let mut ids = Vec::new();
    for item in list {
        let id = item
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .ok_or_else(|| invalid(format!("nodeIDs[] must be integers in 0..={MAX_NODE_ID}, got {item}")))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

async fn create(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let Some(name) = str_arg(&args, "name")? else {
        return Err(invalid("create needs name (becomes maps/prefabs/<name>.vmap)"));
    };
    let name = valid_name(name).map_err(invalid)?.to_string();
    let ids = node_ids_arg(&args)?;
    let centered = match args.get("centered") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => return Err(invalid(format!("centered must be a boolean, got {other}"))),
    };
    let rel = name_rel(&name);
    need_live(&state, &ctx, &["selection.set", "node.prefabFromSelection"]).await?;
    let addon_root = state.content().addon_root();
    let host = host_of(&addon_root, &rel);
    if exists_any_case(&host) {
        return Err(ToolError::new(
            "PrefabExists",
            "PrefabExists",
            format!("{rel} already exists in the active addon; pick another name (the bridge never overwrites)"),
        )
        .with_data(json!({"path": rel, "hostPath": host.display().to_string()}))
        .retry_safe(true)
        .use_instead("prefab", "instance the existing prefab", Some(json!({"action": "instance", "name": name}))));
    }

    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    // the bridge refuses a missing directory
    if let Some(dir) = host.parent() {
        std::fs::create_dir_all(dir).map_err(|error| {
            ToolError::new("IoError", "IoError", format!("{}: {error}", dir.display())).retry_safe(true)
        })?;
    }
    let opts = || RequestOpts { deadline_ms: Some(deadline), expect_doc: Some(expect_doc(&doc)), gate: None };

    // the selection before (reported, not restored: the originals are gone afterwards)
    let previous = {
        let mut bridge = state.bridge.lock().await;
        if bridge.missing(&["selection.get"]).is_empty() {
            bridge
                .request("selection.get", json!({}), RequestOpts::deadline(deadline))
                .await
                .ok()
                .map(|r| json!({"mode": r.result.get("mode"), "nodeIDs": r.result.get("nodeIDs")}))
        } else {
            None
        }
    };

    let select = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request("selection.set", json!({"op": "replace", "nodeIDs": ids}), opts())
            .await
    };
    let select = match select {
        Ok(reply) => reply.result,
        Err(error) => {
            let mut tool = ToolError::from(error);
            // a timeout may have changed the selection already: only a refusal is safe to retry
            tool.retry_safe = !tool.outcome_unknown;
            if let Some(previous) = &previous {
                tool = tool.with_data(json!({"previousSelection": previous}));
            }
            return Err(tool);
        }
    };
    let applied: Vec<i64> = select
        .pointer("/applied/nodeIDs")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let left_out: Vec<i64> = ids.iter().copied().filter(|id| !applied.contains(id)).collect();
    if !left_out.is_empty() {
        return Err(ToolError::new(
            "SelectionNotApplied",
            "SelectionNotApplied",
            format!(
                "Hammer did not select {} of the {} nodes (locked or hidden?); no prefab was made",
                left_out.len(),
                ids.len()
            ),
        )
        .with_data(json!({"notSelected": left_out, "selected": applied, "previousSelection": previous}))
        .retry_safe(true));
    }

    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "node.prefabFromSelection",
                json!({"path": rel, "centered": centered}),
                opts(),
            )
            .await
    };
    let result = match reply {
        Ok(reply) => reply.result,
        Err(error) => return Err(mutator_failure(error, &state, doc_id.as_deref())),
    };
    note_edit(&state, doc_id.as_deref());
    let Value::Object(mut out) = result else {
        return Err(ToolError::internal(format!("node.prefabFromSelection answered a non-object result: {result}")));
    };
    out.remove("doc");
    let mut notes = vec![
        "the selected nodes were REPLACED by the prefab node in the open map (undo_last restores them; the prefab file stays on disk)".to_string(),
        "the open map is not saved: save_map".to_string(),
    ];
    if out.get("loaded").and_then(Value::as_bool) == Some(false) {
        notes.push("Hammer did not load the new prefab's content (loaded:false): save_map and reload_map, then check".into());
    }
    if let Some(warning) = out.get("warning").and_then(Value::as_str) {
        notes.push(format!("bridge warning: {warning}"));
    }
    out.insert("action".into(), json!("create"));
    out.insert("name".into(), json!(name));
    out.insert("path".into(), json!(rel));
    out.insert("hostPath".into(), json!(host.display().to_string()));
    out.insert("replacedNodeIDs".into(), json!(ids));
    out.insert("previousSelection".into(), previous.unwrap_or(Value::Null));
    out.insert("saved".into(), json!(false));
    out.insert("note".into(), json!(notes.join("; ")));
    Ok(ToolOutput::from(Value::Object(out))
        .with_next_args(
            "prefab",
            "add another copy of the prefab",
            json!({"action": "instance", "name": name, "origin": [0, 0, 0]}),
        )
        .with_next("save_map", "persist the open map (the prefab node replaced the originals)"))
}

async fn instance(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let _gate = state.write_gate().await;
    let (rel, name) = target_arg(&args)?;
    let Some(origin) = vec3_arg(&args, "origin")? else {
        return Err(invalid("instance needs origin [x, y, z]"));
    };
    if origin.iter().any(|c| c.abs() > ORIGIN_MAX_ABS) {
        return Err(invalid(format!("origin must be within +-{ORIGIN_MAX_ABS} per coordinate, got {origin:?}")));
    }
    let angles = vec3_arg(&args, "angles")?;
    if angles.is_some_and(|a| a.iter().any(|c| c.abs() > f64::from(f32::MAX))) {
        return Err(invalid("angles are outside the 32-bit float range"));
    }
    need_live(&state, &ctx, &["node.createPrefab"]).await?;
    let addon_root = state.content().addon_root();
    let host = host_of(&addon_root, &rel);
    if !host.is_file() {
        return Err(not_found(&rel, &addon_root));
    }
    let deadline = ctx.budget.deadline_ms()?;
    let doc = require_document(&state, deadline).await?;
    let doc_id = doc_id_of(&doc);
    let mut params = Map::new();
    params.insert("path".into(), json!(rel));
    params.insert("origin".into(), json!(origin));
    if let Some(angles) = angles {
        params.insert("angles".into(), json!(angles));
    }
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                "node.createPrefab",
                Value::Object(params),
                RequestOpts { deadline_ms: Some(deadline), expect_doc: Some(expect_doc(&doc)), gate: None },
            )
            .await
    };
    let result = match reply {
        Ok(reply) => reply.result,
        Err(error) => return Err(mutator_failure(error, &state, doc_id.as_deref())),
    };
    note_edit(&state, doc_id.as_deref());
    let Value::Object(mut out) = result else {
        return Err(ToolError::internal(format!("node.createPrefab answered a non-object result: {result}")));
    };
    out.remove("doc");
    let node_id = out.get("nodeID").cloned().unwrap_or(Value::Null);
    let mut notes = vec!["the open map is not saved: save_map".to_string()];
    if out.get("loaded").and_then(Value::as_bool) == Some(false) {
        notes.push(
            "WARNING loaded:false: Hammer did not load the prefab's content (the node is there, empty in the view); save_map + reload_map may load it"
                .into(),
        );
    }
    if let Some(warning) = out.get("warning").and_then(Value::as_str) {
        notes.push(format!("bridge warning: {warning}"));
    }
    out.insert("action".into(), json!("instance"));
    if let Some(name) = name {
        out.insert("name".into(), json!(name));
    }
    out.insert("path".into(), json!(rel));
    out.insert("saved".into(), json!(false));
    out.insert("note".into(), json!(notes.join("; ")));
    Ok(ToolOutput::from(Value::Object(out))
        .with_next_args("look", "see the instance", json!({"target": node_id}))
        .with_next("save_map", "persist the open map")
        .with_next(
            "compile_map",
            "bake it (the prefab is compiled in; run_map {unload:true} first if the map is loaded in the engine)",
        )
        .with_next("run_map", "play the compiled map"))
}

// -------------------------------------------------------------------------------------------
// list / read
// -------------------------------------------------------------------------------------------

fn limit_of(args: &Value) -> Result<usize, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=LIMIT_MAX as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| invalid(format!("limit must be an integer in 1..={LIMIT_MAX}, got {v}"))),
    }
}

fn list(state: HammerState, limit: usize) -> Result<ToolOutput, ToolError> {
    let addon_root = state.content().addon_root();
    let files = prefab_files(&addon_root);
    let mut items = Vec::new();
    for (index, rel) in files.iter().take(limit).enumerate() {
        let host = host_of(&addon_root, rel);
        let mut item = Map::new();
        item.insert("path".into(), json!(rel));
        if let Some(name) = rel.strip_prefix(&format!("{PREFAB_DIR}/")).and_then(|r| r.strip_suffix(".vmap")) {
            item.insert("name".into(), json!(name));
        }
        if let Ok(meta) = std::fs::metadata(&host) {
            item.insert("bytes".into(), json!(meta.len()));
        }
        if index < LIST_COUNT_CAP {
            match vmap::load(&host) {
                Ok(doc) => {
                    item.insert(
                        "counts".into(),
                        json!({
                            "entities": doc.entities().len(),
                            "meshes": doc.meshes().len(),
                            "prefabs": doc.prefabs().len(),
                        }),
                    );
                }
                Err(error) => {
                    item.insert("readError".into(), json!(error.to_string()));
                }
            }
        }
        items.push(Value::Object(item));
    }
    let mut out = json!({
        "action": "list",
        "addon": state.addon(),
        "directory": PREFAB_DIR,
        "count": files.len(),
        "truncated": files.len() > limit,
        "prefabs": items,
    });
    if files.len() > LIST_COUNT_CAP {
        out["note"] = json!(format!(
            "counts are read for the first {LIST_COUNT_CAP} files only; prefab read shows any one"
        ));
    }
    let mut output = ToolOutput::from(out);
    output = match files.first() {
        Some(first) => output.with_next_args("prefab", "read one", json!({"action": "read", "path": first})),
        None => output.with_next_args(
            "prefab",
            "make one from live nodes",
            json!({"action": "create", "name": "<name>", "nodeIDs": ["<nodeID>"]}),
        ),
    };
    Ok(output)
}

fn read(state: HammerState, args: Value) -> Result<ToolOutput, ToolError> {
    let (rel, _) = target_arg(&args)?;
    let limit = limit_of(&args)?;
    let addon_root = state.content().addon_root();
    let host = host_of(&addon_root, &rel);
    if !host.is_file() {
        return Err(not_found(&rel, &addon_root));
    }
    let doc = vmap::load(&host).map_err(|error| vmap_error(error, Some(&rel)))?;
    let entities: Vec<Value> = doc.entities().iter().take(limit).map(|e| e.to_json(false)).collect();
    let meshes: Vec<Value> = doc.meshes().iter().take(limit).map(|m| m.summary_json()).collect();
    let prefabs: Vec<Value> = doc.prefabs().iter().take(limit).map(|p| p.to_json()).collect();
    let (total_e, total_m, total_p) = (doc.entities().len(), doc.meshes().len(), doc.prefabs().len());
    let out = json!({
        "action": "read",
        "path": rel,
        "hostPath": host.display().to_string(),
        "counts": {"entities": total_e, "meshes": total_m, "prefabs": total_p},
        "entities": entities,
        "meshes": meshes,
        "prefabs": prefabs,
        "truncated": total_e.max(total_m).max(total_p) > limit,
    });
    Ok(ToolOutput::from(out).with_next_args(
        "prefab",
        "add it to the open map",
        json!({"action": "instance", "path": rel, "origin": [0, 0, 0]}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_stems() {
        for ok in ["crate", "a", "tm-phoenix_2", "0day"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "A", "a/b", "a\\b", "..", "a.b", "a b", "-a", "_a", &"x".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn paths_are_addon_relative_maps() {
        assert!(valid_path("maps/prefabs/x.vmap").is_ok());
        assert!(valid_path("maps/m318/two_boxes.vmap").is_ok());
        for bad in [
            "x.vmap", "maps/x.vmx", "/maps/x.vmap", "maps\\x.vmap", "maps/../x.vmap", "maps//x.vmap",
            "z:/maps/x.vmap", "maps/./x.vmap", "maps/x;y.vmap",
        ] {
            assert!(valid_path(bad).is_err(), "{bad}");
        }
    }
}
