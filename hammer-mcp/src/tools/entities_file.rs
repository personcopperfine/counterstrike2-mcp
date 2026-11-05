//! Entity reads from the SAVED `.vmap` (file route, `crate::vmap`): `list_entities`,
//! `find_entities`, `get_entity` (the last two are registered by `entities_read.rs`, which routes
//! `source:"saved"` here). Every result reports `source.fromDisk` and `staleRead`
//! (unsaved bridge edits since the last save). Plan: target/entities-plan.md WP-T1.
//!
//! Path handling ([`crate::vmdl::ContentPaths::resolve_read_ext`], extension `vmap` — errors map
//! as the vmdl/particle routes do): a mapped Windows/host path, a content-relative path
//! (`csgo_addons/<addon>/maps/x.vmap`) or an **addon-relative** path (`maps/x.vmap`, tried as
//! `csgo_addons/<addon>/maps/x.vmap` first). Omitted `path` = the active Hammer document through
//! the bridge's `doc.active` — the ONLY bridge use of this file route (no bridge methods are
//! declared on the specs, so the registry never gates these tools); no bridge or no active
//! document is `NoDocument` naming the `path` argument.
//!
//! `staleRead` = the file is the active document AND (unsaved bridge edits > 0 OR Hammer's own
//! `doc.active.dirty` is true); `staleReason` says which. The bridge is asked for the active
//! document in both cases (path given and omitted) — always `doc.active`, nothing else. When it
//! cannot say (bridge down / no document / unmapped path) the brief's keep-it-simple rule applies:
//! `staleRead = unsaved > 0`. `VmapError` mapping: Io NotFound -> `FileNotFound`, everything else ->
//! `VmapParseError` with the message; reads are retry-safe.
//!
//! Filters (`find_entities`' `class`/`targetname`/`nameContains`) must be non-empty: an empty
//! string would match everything while pretending to filter (brief item 13). `nodeID` is bounded
//! to 0..=2147483647 like the live route (brief item 5).
//!
//! I/O connections (`get_entity`'s `connections`) are read from the node's `connectionsData` in the
//! saved file. Every checked-in fixture has empty `connectionsData`, so the path is verified
//! against two real maps instead: `template_wingman.vmap` (one `logic_auto` connection) and
//! `cs_script_demo.vmap` (12 entities with connections) — the `#[ignore]`d real-tree tests
//! `real_tree_template_wingman_connections_through_get_entity` here and
//! `real_tree_template_wingman_connections` / `real_tree_cs_script_demo_connections` in
//! `tests/vmap_dmx.rs`. Run them with:
//! `cargo test -p hammer-mcp --test entities_file -- --ignored` and
//! `cargo test -p hammer-mcp --test vmap_dmx -- --ignored`.
use crate::state::HammerState;
use crate::vmap::{Document, Entity, VmapError};
use crate::vmdl::VmdlError;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// `list_entities`/`find_entities` limit bound and default (brief WP-T1).
const LIMIT_MAX: u64 = 2000;
const LIMIT_DEFAULT: u64 = 500;
/// The bridge parses a live `nodeID` into an `int`; the file route keeps the same bound so one
/// id means the same thing on both routes (brief item 5).
const MAX_NODE_ID: i64 = 2_147_483_647;

/// Register one of the three file-route tools (called at its POC position by `hammer.rs`).
pub fn register_one(r: &mut Registry<HammerState>, name: &str) {
    match name {
        "list_entities" => r.register(
            ToolSpec::new(
                "list_entities",
                "Entities from the saved .vmap on disk (not unsaved edits; see find_entities). Prefab instances come as `prefabs`.",
                ReadOnly,
                &[],
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": path_schema(),
                    "classnames": {
                        "type": "boolean",
                        "description": "only a per-classname histogram"
                    },
                    "limit": limit_schema("entities returned")
                },
                "additionalProperties": false
            })),
            list_entities,
        ),
        other => panic!("entities_file: unknown tool {other}"),
    }
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "description": ".vmap, addon-relative; default = active doc"
    })
}

fn limit_schema(what: &str) -> Value {
    json!({
        "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
        "description": format!("max {what}")
    })
}

// -------------------------------------------------------------------------------------------
// Shared plumbing: resolution, source/staleRead envelope, error mapping
// -------------------------------------------------------------------------------------------

/// `(host path of the resolved .vmap, parsed document, is the file the active document?, is the
/// active document dirty?)`.
///
/// `is_active` is `None` when the bridge could not say (down, no document, unmapped path): the
/// keep-it-simple staleRead rule then applies ([`stale_read`]). `dirty` is Hammer's own flag for
/// the active document (false when there is no active document).
pub(crate) async fn resolve_document(
    state: &HammerState,
    ctx: &CallCtx,
    args: &Value,
) -> Result<(PathBuf, Arc<Document>, Option<bool>, bool), ToolError> {
    match args.get("path").and_then(Value::as_str) {
        Some(input) => {
            // read the active document FIRST: it refreshes the document-derived addon that
            // `resolve_input` resolves a relative path in
            let active = active_document(state, ctx).await?;
            let host = resolve_input(state, input)?;
            let document =
                crate::vmap::load(&host).map_err(|error| vmap_error(error, Some(input)))?;
            let (is_active, dirty) = match active {
                Some((active, dirty)) => (Some(same_file(&host, &active)), dirty),
                None => (None, false),
            };
            Ok((host, document, is_active, dirty))
        }
        None => {
            let (host, dirty) = active_document(state, ctx)
                .await?
                .ok_or_else(no_document)?;
            let document = crate::vmap::load(&host).map_err(|error| vmap_error(error, None))?;
            Ok((host, document, Some(true), dirty))
        }
    }
}

/// Resolve a `path` argument (see the module header for the accepted forms).
fn resolve_input(state: &HammerState, input: &str) -> Result<PathBuf, ToolError> {
    let attempt =
        |candidate: &str| state.content().resolve_read_ext(candidate, &state.path_map, "vmap");
    // Addon-relative form first (`maps/x.vmap` -> `csgo_addons/<addon>/maps/x.vmap`) for plain
    // relative inputs; its error wins when both attempts fail (it is the intended spelling).
    let prefixed = if !is_windowsish(input)
        && !Path::new(input).is_absolute()
        && !input.starts_with("csgo_addons/")
    {
        Some(format!(
            "csgo_addons/{}/{}",
            state.addon(),
            input.trim_start_matches('/')
        ))
    } else {
        None
    };
    match prefixed {
        Some(addon_rel) => match attempt(&addon_rel) {
            Ok(host) => Ok(host),
            Err(error) => attempt(input).map_err(|_| path_error(error, input)),
        },
        None => attempt(input).map_err(|error| path_error(error, input)),
    }
}

/// `C:\…` / `\\…` (the `vmdl::paths` window check, inlined for the relative-input decision).
fn is_windowsish(input: &str) -> bool {
    let bytes = input.as_bytes();
    input.starts_with("\\\\")
        || input.starts_with("//?/")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// The active document's mapped host path and Hammer `dirty` flag, best effort. `Ok(None)` = the
/// bridge cannot name one (down, no document, unmapped Windows path — the caller applies its
/// fallback); `Err` is only a budget exhaustion.
async fn active_document(
    state: &HammerState,
    ctx: &CallCtx,
) -> Result<Option<(PathBuf, bool)>, ToolError> {
    let deadline = ctx.budget.deadline_ms()?;
    // the shared read: a file-route tool's `doc.active` is a read like any other and runs the mesh
    // cache's sticky comparison (review-4 R4-1)
    let Ok(Some(doc)) =
        crate::tools::live_common::read_active_document(state, Some(deadline)).await
    else {
        return Ok(None);
    };
    let dirty = doc.get("dirty").and_then(Value::as_bool).unwrap_or(false);
    let windows = doc.get("path").and_then(Value::as_str);
    Ok(windows
        .and_then(|windows| state.path_map.windows_to_host(windows))
        .map(|host| (host, dirty)))
}

/// `NoDocument`: the route cannot proceed without a path (naming the argument, brief WP-T1).
fn no_document() -> ToolError {
    ToolError::new(
        "NoDocument",
        "NoDocument",
        "no active Hammer document is available through the bridge; pass `path` \
         (addon-relative maps/x.vmap or a mapped host path)",
    )
    .with_code(-32003)
    .retry_safe(true)
}

/// `(staleRead, staleReason)`: true when the file is the active document and **either** this
/// server has unsaved bridge edits **or** Hammer's own `dirty` flag is up (the user edited the
/// document, or an undo popped a keyvalue group that reverted nothing — brief item 4). When the
/// bridge could not say whether the file is active (`None`), the keep-it-simple rule applies:
/// `unsaved > 0`. `staleReason` names the cause and is `null` when the read is fresh.
pub(crate) fn stale_read(
    state: &HammerState,
    is_active: Option<bool>,
    dirty: bool,
) -> (bool, Option<String>) {
    let unsaved = state.unsaved_edits();
    if is_active == Some(false) {
        return (false, None);
    }
    match (unsaved > 0, dirty) {
        (true, true) => (
            true,
            Some(format!(
                "{unsaved} unsaved bridge edit(s) and the active Hammer document is dirty"
            )),
        ),
        (true, false) => (
            true,
            Some(format!("{unsaved} unsaved bridge edit(s) since the last save")),
        ),
        (false, true) => (
            true,
            Some(
                "the active Hammer document is dirty: it was edited in Hammer, or an undo popped \
                 a keyvalue group that does not restore values"
                    .to_string(),
            ),
        ),
        (false, false) => (false, None),
    }
}

/// The `source` object every result carries (brief WP-T1: fromDisk, hostPath, mtime, entityCount).
pub(crate) fn source_json(host: &Path, document: &Document) -> Value {
    json!({
        "fromDisk": true,
        "hostPath": host.display().to_string(),
        "mtime": document.mtime.map(|mtime| {
            mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        }),
        "entityCount": document.entities().len(),
    })
}

/// The envelope keys shared by all three results.
pub(crate) fn read_envelope(
    state: &HammerState,
    source: Value,
    stale: bool,
    reason: Option<String>,
) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("source".into(), source);
    out.insert("staleRead".into(), json!(stale));
    out.insert(
        "staleReason".into(),
        reason.map(|reason| json!(reason)).unwrap_or(Value::Null),
    );
    out.insert("unsavedBridgeEdits".into(), json!(state.unsaved_edits()));
    out.insert("warning".into(), json!(state.unsaved_warning()));
    out
}

/// [`VmdlError`] from [`crate::vmdl::ContentPaths::resolve_read_ext`] -> [`ToolError`]: the
/// content-path layer reports vmdl errors, distinguished by wording (as in `particle_file.rs`).
/// A file that cannot be read (canonicalize fails) is `FileNotFound` — the required negative.
fn path_error(error: VmdlError, input: &str) -> ToolError {
    match error {
        VmdlError::Path(msg) if msg.contains("cannot be read") => ToolError::new(
            "FileNotFound",
            "FileNotFound",
            format!("{input:?} cannot be read: {msg}"),
        )
        .with_data(json!({"path": input}))
        .retry_safe(true),
        VmdlError::Path(msg) if msg.contains("no host mapping") => {
            ToolError::new("PathNotMapped", "PathNotMapped", msg)
                .with_code(-32602)
                .with_data(json!({"path": input}))
                .retry_safe(true)
        }
        VmdlError::Path(msg) => ToolError::new("InvalidPath", "InvalidPath", msg)
            .with_code(-32602)
            .with_data(json!({"path": input}))
            .retry_safe(true),
        VmdlError::Io(error) => {
            ToolError::new("IoError", "IoError", error.to_string()).retry_safe(false)
        }
        other => ToolError::internal(format!("unexpected content-path error: {other}")),
    }
}

/// [`VmapError`] -> [`ToolError`]: Io NotFound -> `FileNotFound`, everything else ->
/// `VmapParseError` with the message; reads are retry-safe (brief WP-T1). Shared with the mesh
/// rebuild operators (`tools/mesh_live`), whose SAVED-`.vmap` geometry source reads the same file.
pub(crate) fn vmap_error(error: VmapError, input: Option<&str>) -> ToolError {
    let data = input.map(|input| json!({"path": input})).unwrap_or(json!({}));
    match error {
        VmapError::Io { path: file, source } if source.kind() == ErrorKind::NotFound => {
            ToolError::new(
                "FileNotFound",
                "FileNotFound",
                format!("{}: {source}", file.display()),
            )
            .with_data(data)
            .retry_safe(true)
        }
        other => ToolError::new("VmapParseError", "VmapParseError", other.to_string())
            .with_data(data)
            .retry_safe(true),
    }
}

/// Same file? Canonical comparison when both exist, case-insensitive text otherwise (the
/// PathMap is case-insensitive on the Windows side by contract) — the `model_file.rs` rule.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    a.to_string_lossy()
        .replace('\\', "/")
        .eq_ignore_ascii_case(&b.to_string_lossy().replace('\\', "/"))
}

// -------------------------------------------------------------------------------------------
// Argument helpers
// -------------------------------------------------------------------------------------------

/// `limit?: integer` — [`LIMIT_DEFAULT`] when absent, 1..=[`LIMIT_MAX`] when given.
fn limit_arg(args: &Value) -> Result<usize, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT as usize),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(limit as usize),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
        ))),
    }
}

/// `classnames?: boolean` (default false).
fn classnames_arg(args: &Value) -> Result<bool, ToolError> {
    match args.get("classnames") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "classnames must be a boolean, got {other}"
        ))),
    }
}

/// `withKeyvalues?: boolean` (default false).
fn with_keyvalues_arg(args: &Value) -> Result<bool, ToolError> {
    match args.get("withKeyvalues") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "withKeyvalues must be a boolean, got {other}"
        ))),
    }
}

/// `within?: {origin: [x,y,z], radius > 0}` -> `Some((origin, radius))`.
///
/// The file route computes the sphere in Rust, so the live route's wire-format coordinate rule
/// (no exponent-form number, `|x| <= 1e9`) does not apply here: the numbers are used exactly as
/// given and must only be finite.
fn within_arg(args: &Value) -> Result<Option<([f64; 3], f64)>, ToolError> {
    let Some(within) = args.get("within").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = |msg: String| ToolError::invalid_arguments(format!("within: {msg}"));
    let values = within
        .get("origin")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 3)
        .ok_or_else(|| invalid("origin must be [x, y, z]".to_string()))?;
    let mut origin = [0.0; 3];
    for (axis, value) in values.iter().enumerate() {
        origin[axis] = value
            .as_f64()
            .filter(|axis| axis.is_finite())
            .ok_or_else(|| {
                invalid(format!("origin[{axis}] must be a finite number, got {value}"))
            })?;
    }
    let radius = within
        .get("radius")
        .and_then(Value::as_f64)
        .filter(|radius| *radius > 0.0 && radius.is_finite())
        .ok_or_else(|| invalid("radius must be a number > 0".to_string()))?;
    Ok(Some((origin, radius)))
}

/// Optional string argument: `None` when absent or null, an error for any other type.
fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// Optional integer argument (`nodeID`): `None` when absent or null, otherwise an integer in
/// `0..=`[`MAX_NODE_ID`] (the same bound as the live route: a bigger id is refused locally).
pub(crate) fn optional_i64(args: &Value, name: &str) -> Result<Option<i64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|id| (0..=MAX_NODE_ID).contains(id))
            .map(Some)
            .ok_or_else(|| {
                ToolError::invalid_arguments(format!(
                    "{name} must be an integer in 0..={MAX_NODE_ID}, got {n}"
                ))
            }),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be an integer in 0..={MAX_NODE_ID}, got {other}"
        ))),
    }
}

/// A filter that is present but empty: refused, because an empty class/targetname/nameContains
/// matches everything while pretending to filter (brief item 13).
fn filter_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match optional_str(args, name)? {
        Some("") => Err(ToolError::invalid_arguments(format!(
            "{name} is empty: an empty filter matches everything; omit {name} instead"
        ))),
        other => Ok(other),
    }
}

/// Case-insensitive substring match on classname OR targetname (`nameContains`).
fn name_contains(entity: &Entity, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    entity.classname.to_lowercase().contains(&needle)
        || entity
            .targetname
            .as_deref()
            .is_some_and(|name| name.to_lowercase().contains(&needle))
}

// -------------------------------------------------------------------------------------------
// list_entities
// -------------------------------------------------------------------------------------------

/// Every entity of the SAVED `.vmap` (`to_json(false)`), or with `classnames:true` only the
/// per-classname histogram. `limit` caps the list (never the histogram).
pub async fn list_entities(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let limit = limit_arg(&args)?;
    let classnames = classnames_arg(&args)?;
    let (host, document, is_active, dirty) = resolve_document(&state, &ctx, &args).await?;
    let total = document.entities().len();

    let (stale, reason) = stale_read(&state, is_active, dirty);
    let mut out = read_envelope(&state, source_json(&host, &document), stale, reason);
    out.insert("total".into(), json!(total));
    if classnames {
        let mut histogram: BTreeMap<&str, u64> = BTreeMap::new();
        for entity in document.entities() {
            *histogram.entry(entity.classname.as_str()).or_default() += 1;
        }
        out.insert("count".into(), json!(histogram.len()));
        let classes: Map<String, Value> = histogram
            .into_iter()
            .map(|(class, count)| (class.to_string(), json!(count)))
            .collect();
        out.insert("classnames".into(), Value::Object(classes));
        out.insert("truncated".into(), json!(false));
    } else {
        let entities: Vec<Value> = document
            .entities()
            .iter()
            .take(limit)
            .map(|entity| entity.to_json(false))
            .collect();
        out.insert("entities".into(), Value::Array(entities));
        out.insert("count".into(), json!(total.min(limit)));
        out.insert("truncated".into(), json!(total > limit));
    }
    // CMapPrefab nodes are instances of another map, not entities: listed beside them (I4)
    if !document.prefabs().is_empty() {
        out.insert("prefabCount".into(), json!(document.prefabs().len()));
        out.insert(
            "prefabs".into(),
            Value::Array(document.prefabs().iter().take(limit).map(|p| p.to_json()).collect()),
        );
    }
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// find_entities
// -------------------------------------------------------------------------------------------

/// Filter the SAVED `.vmap`'s entities: exact classname/targetname (case-insensitive) and an
/// origin sphere through [`Document::find`], plus the tool-layer `nameContains` substring;
/// at least one filter is required. `withKeyvalues:true` adds each entity's keyvalues.
pub async fn find_entities(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let class = filter_str(&args, "class")?;
    let targetname = filter_str(&args, "targetname")?;
    let within = within_arg(&args)?;
    let name_filter = filter_str(&args, "nameContains")?;
    if class.is_none() && targetname.is_none() && within.is_none() && name_filter.is_none() {
        return Err(ToolError::invalid_arguments(
            "at least one filter is required: class, targetname, within or nameContains",
        ));
    }
    let limit = limit_arg(&args)?;
    let with_keyvalues = with_keyvalues_arg(&args)?;
    let (host, document, is_active, dirty) = resolve_document(&state, &ctx, &args).await?;

    let mut matches = document.find(class, targetname, within, usize::MAX);
    if let Some(needle) = name_filter {
        matches.retain(|entity| name_contains(entity, needle));
    }
    let total = matches.len();
    let entities: Vec<Value> = matches
        .into_iter()
        .take(limit)
        .map(|entity| entity.to_json(with_keyvalues))
        .collect();

    let (stale, reason) = stale_read(&state, is_active, dirty);
    let mut out = read_envelope(&state, source_json(&host, &document), stale, reason);
    out.insert("entities".into(), Value::Array(entities));
    out.insert("total".into(), json!(total));
    out.insert("count".into(), json!(total.min(limit)));
    out.insert("truncated".into(), json!(total > limit));
    Ok(Value::Object(out).into())
}

// -------------------------------------------------------------------------------------------
// get_entity
// -------------------------------------------------------------------------------------------

/// One entity of the SAVED `.vmap` by `nodeID` (wins) or exact case-insensitive `targetname` (2+
/// matches: `AmbiguousTargetname`), with keyvalues and
/// I/O connections; not found is `EntityNotFound {data.nodeID|targetname, data.path}`.
pub async fn get_entity(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let node_id = optional_i64(&args, "nodeID")?;
    let targetname = optional_str(&args, "targetname")?;
    if node_id.is_none() && targetname.is_none() {
        return Err(ToolError::invalid_arguments(
            "one selector is required: nodeID or targetname",
        ));
    }
    let (host, document, is_active, dirty) = resolve_document(&state, &ctx, &args).await?;
    let input = args.get("path").and_then(Value::as_str);
    let path_label = input
        .map(str::to_string)
        .unwrap_or_else(|| host.display().to_string());

    // by name (no nodeID): two or more matches are ambiguous, as on the live route
    if node_id.is_none() {
        if let Some(name) = targetname {
            let hits = document.by_targetname(name);
            if hits.len() > 1 {
                let candidates = hits
                    .iter()
                    .map(|e| {
                        json!({
                            "nodeID": e.node_id,
                            "classname": e.classname,
                            "targetname": e.targetname,
                        })
                    })
                    .collect();
                return Err(super::entities_query::ambiguous_targetname(name, candidates));
            }
        }
    }
    let entity = document
        .get(node_id, targetname)
        .map_err(|error| vmap_error(error, input))?;
    let Some(entity) = entity else {
        let mut data = Map::new();
        if let Some(node_id) = node_id {
            data.insert("nodeID".into(), json!(node_id));
        }
        if let Some(targetname) = targetname {
            data.insert("targetname".into(), json!(targetname));
        }
        data.insert("path".into(), json!(path_label));
        return Err(ToolError::new(
            "EntityNotFound",
            "EntityNotFound",
            format!(
                "no entity with {} in {path_label}",
                match (node_id, targetname) {
                    (Some(node_id), _) => format!("nodeID {node_id}"),
                    (None, Some(targetname)) => format!("targetname {targetname:?}"),
                    (None, None) => unreachable!("checked above"),
                }
            ),
        )
        .with_data(Value::Object(data))
        .retry_safe(true));
    };

    let (stale, reason) = stale_read(&state, is_active, dirty);
    let mut out = read_envelope(&state, source_json(&host, &document), stale, reason);
    out.insert("entity".into(), entity.to_json(true));
    Ok(Value::Object(out).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limit_arg_bounds() {
        assert_eq!(limit_arg(&json!({})).unwrap(), LIMIT_DEFAULT as usize);
        assert_eq!(limit_arg(&json!({"limit": 1})).unwrap(), 1);
        assert_eq!(limit_arg(&json!({"limit": 2000})).unwrap(), 2000);
        for bad in [json!({"limit": 0}), json!({"limit": 2001}), json!({"limit": "500"})] {
            assert_eq!(limit_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn within_arg_shape() {
        assert_eq!(within_arg(&json!({})).unwrap(), None);
        assert_eq!(
            within_arg(&json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 5.0}})).unwrap(),
            Some(([1.0, 2.0, 3.0], 5.0))
        );
        for bad in [
            json!({"within": {"origin": [1.0, 2.0], "radius": 5.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0], "radius": 0.0}}),
            json!({"within": {"origin": [1.0, 2.0, 3.0]}}),
            json!({"within": {"origin": [1.0, 2.0, "x"], "radius": 5.0}}),
        ] {
            assert_eq!(within_arg(&bad).unwrap_err().name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn name_contains_is_case_insensitive_on_both_fields() {
        let entity = Entity {
            guid: "g".into(),
            node_class: "CMapEntity".into(),
            node_id: Some(1),
            reference_id: None,
            classname: "func_buyzone".into(),
            targetname: Some("BuyZone".into()),
            origin: None,
            angles: None,
            scales: None,
            keyvalues: vec![],
            connections: vec![],
        };
        assert!(name_contains(&entity, "BUY"));
        assert!(name_contains(&entity, "zone"));
        assert!(name_contains(&entity, "buyzone"));
        assert!(!name_contains(&entity, "light"));
    }
}
