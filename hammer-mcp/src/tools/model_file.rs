//! File-route ModelDoc tools: read/list/edit/create/compile `.vmdl` sources on disk.
//!
//! Facts these encode (hammer-mcp/research/modeldoc-re.md, target/orch/facts-round2.md):
//! * a `.vmdl` is KV3 text ([`crate::vmdl::Model`]); edits are all-or-nothing, written through
//!   [`crate::vmdl::fsutil::write_atomic_with_backup`] (backup beside the file, atomic rename);
//! * writes land only in the active addon ([`crate::vmdl::ContentPaths::resolve_write`]), reads
//!   anywhere under `content/` ([`crate::vmdl::ContentPaths::resolve_read`]);
//! * `classCheck:"snapshot"` (the default) checks `_class` names against
//!   `<repo>/hammer-mcp/research/modeldoc-nodetypes.json` when that snapshot exists; a missing
//!   snapshot is only a warning (facts round 2 §8);
//! * `model_compile` route `resourcecompiler` (the default) execs `resourcecompiler.exe` inside
//!   the ModelDoc container with `-game /app/game/csgo` (never `<game>/csgo_addons/<addon>`:
//!   FATAL gameinfo.gi); route `editor` calls the bridge's `modeldoc.compile`, whose **only**
//!   success signal is the `.vmdl_c` mtime advancing, and refuses documents without an editor
//!   asset (`hasAsset:false`, created after the tools started).
//!
//! `ok:false` is a *result*, never an error: the caller reads `failLines` / `fatal`.
//!
//! The resourcecompiler route itself lives in [`crate::tools::compile`] (shared with
//! `particle_compile`); what stays here is the `.vmdl` behaviour around it: the ModelDoc
//! container label ([`CompileTarget::MODELDOC`]) and the editor route.
use crate::state::HammerState;
use crate::tools::compile::{
    compile_resourcecompiler, compile_timeout, mtime_of, poll_output_updated, CompileTarget,
};
use crate::vmdl::{self, Model, NodePath, Op, VmdlError};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{require_caps, CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The compiler run parser, unchanged, re-exported for `model_compile`'s callers and tests
/// (`tests/model_file.rs` unit-tests it as `hammer_mcp::tools::model_file::parse_compile_output`).
pub use crate::tools::compile::{parse_compile_output, CompileSummary};

/// Where the node-type snapshot lives, below this repo's checkout under `--project-root`
/// ([`cs2_mcp_common::mcp_repo_dir`]). Read by `classCheck:"snapshot"`.
const SNAPSHOT_REL: &str = "hammer-mcp/research/modeldoc-nodetypes.json";
/// `model_edit`'s op bound per call (sequential application against one tree).
pub const MAX_OPS: usize = 256;

// -------------------------------------------------------------------------------------------
// Error mapping
// -------------------------------------------------------------------------------------------

/// [`VmdlError`] -> [`ToolError`]. Every model-layer failure happens before any byte is written
/// (ops run against an in-memory clone, writes are atomic), so these are retry-safe except I/O
/// on a path whose state is no longer certain.
pub(crate) fn vmdl_error(error: VmdlError) -> ToolError {
    match error {
        VmdlError::Parse(parse) => ToolError::new(
            "ParseError",
            "ParseError",
            format!("cannot parse: {parse}"),
        )
        .with_data(json!({"line": parse.line, "col": parse.col}))
        .retry_safe(true),
        VmdlError::Invalid(issues) => {
            let first = issues
                .first()
                .map(|issue| format!("{}: {}", issue.path, issue.msg))
                .unwrap_or_default();
            let listed: Vec<Value> = issues
                .iter()
                .map(|issue| json!({"path": issue.path, "msg": issue.msg}))
                .collect();
            ToolError::new(
                "InvalidModel",
                "InvalidModel",
                format!("invalid model structure: {first}"),
            )
            .with_data(json!({"issues": listed}))
            .retry_safe(true)
        }
        VmdlError::NodeNotFound(path) => {
            ToolError::new("NodeNotFound", "NodeNotFound", format!("no node at {path}"))
                .with_data(json!({"path": path}))
                .retry_safe(true)
        }
        VmdlError::BadOp { index, msg } => ToolError::new(
            "InvalidOp",
            "InvalidOp",
            format!("bad op at index {index}: {msg}"),
        )
        .with_code(-32602)
        .with_data(json!({"opIndex": index, "detail": msg}))
        .retry_safe(true),
        VmdlError::UnknownClass { path, class } => ToolError::new(
            "UnknownClass",
            "UnknownClass",
            format!("unknown _class {class:?} at {path} (not in the node-type snapshot)"),
        )
        .with_data(json!({"class": class, "path": path}))
        .retry_safe(true),
        VmdlError::Io(error) => {
            ToolError::new("IoError", "IoError", error.to_string()).retry_safe(false)
        }
        VmdlError::Path(msg) => {
            // facts round 2 §7: VmdlError::Path messages distinguish these two by wording
            if msg.contains("no host mapping") {
                ToolError::new("PathNotMapped", "PathNotMapped", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else if msg.contains("only models of the active addon") {
                ToolError::new("WriteOutsideAddon", "WriteOutsideAddon", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else {
                ToolError::new("InvalidPath", "InvalidPath", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            }
        }
    }
}

fn io_fail<T>(what: &str, result: std::io::Result<T>) -> Result<T, ToolError> {
    result.map_err(|error| {
        ToolError::new("IoError", "IoError", format!("{what}: {error}")).retry_safe(true)
    })
}

fn missing_arg(name: &str) -> ToolError {
    ToolError::invalid_arguments(format!("{name} (string) is required"))
}

// -------------------------------------------------------------------------------------------
// model_read / model_list_nodes
// -------------------------------------------------------------------------------------------

/// Read a `.vmdl` from disk and return one node subtree as JSON (`source:"file"`: the file may be
/// older or newer than whatever is open in the editor; pass 1 never queries the bridge).
pub async fn model_read(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Err(missing_arg("path"));
    };
    let node = args.get("node").and_then(Value::as_str).unwrap_or("/");
    let node_path: NodePath = node.parse().map_err(|error: VmdlError| {
        vmdl_error(error).with_code(-32602)
    })?;
    let depth = depth_arg(&args)?;

    let host = state
        .content()
        .resolve_read(path, &state.path_map)
        .map_err(vmdl_error)?;
    let text =
        io_fail(&format!("cannot read {}", host.display()), std::fs::read_to_string(&host))?;
    let model = Model::parse(&text).map_err(vmdl_error)?;
    let subtree = model.node_json(&node_path, depth).map_err(vmdl_error)?;
    Ok(json!({
        "path": path,
        "hostPath": host.display().to_string(),
        "format": model.doc.header.format,
        "node": subtree,
        "source": "file",
    })
    .into())
}

/// Flat node list of a `.vmdl` from disk, filterable by `_class` / `name`.
pub async fn model_list_nodes(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Err(missing_arg("path"));
    };
    let depth = depth_arg(&args)?;
    let class = args.get("class").and_then(Value::as_str);
    let name = args.get("name").and_then(Value::as_str);

    let host = state
        .content()
        .resolve_read(path, &state.path_map)
        .map_err(vmdl_error)?;
    let text =
        io_fail(&format!("cannot read {}", host.display()), std::fs::read_to_string(&host))?;
    let model = Model::parse(&text).map_err(vmdl_error)?;
    let nodes = nodes_json(&model.list(depth, class, name));
    Ok(json!({
        "path": path,
        "hostPath": host.display().to_string(),
        "source": "file",
        "nodes": nodes,
        "count": nodes.len(),
    })
    .into())
}

/// [`crate::vmdl::NodeInfo`] as the wire shape (`_class`, camelCase `childCount`).
fn nodes_json(infos: &[vmdl::NodeInfo]) -> Vec<Value> {
    infos
        .iter()
        .map(|info| {
            json!({
                "path": info.path,
                "_class": info.class,
                "name": info.name,
                "childCount": info.child_count,
                "attrKeys": info.attr_keys,
            })
        })
        .collect()
}

/// `depth?: int (>= 0)`; `None` = full.
fn depth_arg(args: &Value) -> Result<Option<usize>, ToolError> {
    match args.get("depth") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|d| Some(d as usize))
            .ok_or_else(|| ToolError::invalid_arguments("depth must be a non-negative integer")),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "depth must be a non-negative integer, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// classCheck (edit + create)
// -------------------------------------------------------------------------------------------

/// The snapshot path for this server (`--project-root` + [`SNAPSHOT_RESP`]).
fn snapshot_path(state: &HammerState) -> PathBuf {
    cs2_mcp_common::mcp_repo_dir(&state.project_root).join(SNAPSHOT_REL)
}

/// `classCheck?: "snapshot" | "off"` (default `snapshot`).
fn wants_class_check(args: &Value) -> Result<bool, ToolError> {
    match args.get("classCheck") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::String(mode)) if mode == "snapshot" => Ok(true),
        Some(Value::String(mode)) if mode == "off" => Ok(false),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "classCheck must be \"snapshot\" or \"off\", got {other}"
        ))),
    }
}

/// Outcome of the `classCheck` phase: were the classes verified, and any warnings added.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClassCheck {
    /// All `_class` names were confirmed against a snapshot.
    pub(crate) verified: bool,
    pub(crate) warnings: Vec<String>,
}

/// Check `model`'s `_class` names against the snapshot. With no snapshot the check cannot run
/// (warning, never a failure); with `classCheck:"off"` nothing runs and nothing is verified.
pub(crate) fn check_classes(model: &Model, state: &HammerState, check: bool) -> Result<ClassCheck, ToolError> {
    if !check {
        return Ok(ClassCheck {
            verified: false,
            warnings: vec!["classCheck: off; class names not checked".to_string()],
        });
    }
    let (classes, source) = vmdl::known_classes(&snapshot_path(state));
    if let Err(error) = model.validate_classes(&classes) {
        return Err(vmdl_error(error).with_data(json!({"snapshot": source})));
    }
    Ok(ClassCheck { verified: true, warnings: Vec::new() })
}

// -------------------------------------------------------------------------------------------
// model_edit
// -------------------------------------------------------------------------------------------

/// Edit a `.vmdl` in the active addon: sequential ops applied all-or-nothing, class-checked,
/// then a backup + atomic replace (or nothing on `dryRun`).
pub async fn model_edit(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let modeldoc_warning = crate::tools::addon::modeldoc_addon_warning(&state).await;
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Err(missing_arg("path"));
    };
    let ops: Vec<Op> = match args.get("ops") {
        Some(ops) => serde_json::from_value(ops.clone())
            .map_err(|error| ToolError::invalid_arguments(format!("ops: {error}")))?,
        None => return Err(missing_arg("ops")),
    };
    if ops.is_empty() || ops.len() > MAX_OPS {
        return Err(ToolError::invalid_arguments(format!(
            "ops must hold 1..{MAX_OPS} entries, got {}",
            ops.len()
        )));
    }
    let dry_run = args.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let check = wants_class_check(&args)?;

    let host = state
        .content()
        .resolve_write(path, &state.path_map)
        .map_err(vmdl_error)?;
    let original =
        io_fail(&format!("cannot read {}", host.display()), std::fs::read_to_string(&host))?;
    let mut model = Model::parse(&original).map_err(vmdl_error)?;
    let results = model.apply(&ops).map_err(vmdl_error)?;
    let mut checked = check_classes(&model, &state, check)?;
    checked.warnings.extend(modeldoc_warning);

    let (written, backup) = if dry_run {
        (false, None)
    } else {
        let backup = io_fail(
            &format!("cannot write {}", host.display()),
            vmdl::fsutil::write_atomic_with_backup(&host, &model.write()),
        )?;
        (true, backup)
    };
    Ok(json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": host.display().to_string(),
        "applied": results.len(),
        "results": results
            .iter()
            .map(|r| json!({"op": r.op, "path": r.path}))
            .collect::<Vec<_>>(),
        "backup": backup.map(|p| json!(p.display().to_string())).unwrap_or(Value::Null),
        "written": written,
        "warnings": checked.warnings,
        "nextStep": "model_reload if the file is open in ModelDoc",
    })
    .into())
}

// -------------------------------------------------------------------------------------------
// model_create
// -------------------------------------------------------------------------------------------

/// Create a new `.vmdl` in the active addon from a template; refuses an existing file unless
/// `overwrite` (which then goes through the same backup + atomic replace as `model_edit`).
pub async fn model_create(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let modeldoc_warning = crate::tools::addon::modeldoc_addon_warning(&state).await;
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Err(missing_arg("path"));
    };
    let Some(template) = args.get("template").and_then(Value::as_str) else {
        return Err(missing_arg("template"));
    };
    let empty = json!({});
    let params = args.get("params").filter(|v| !v.is_null()).unwrap_or(&empty);
    let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    let check = wants_class_check(&args)?;

    let host = state
        .content()
        .resolve_write(path, &state.path_map)
        .map_err(vmdl_error)?;
    if host.exists() && !overwrite {
        return Err(ToolError::new(
            "FileExists",
            "FileExists",
            format!(
                "{} already exists (pass overwrite:true to replace it)",
                host.display()
            ),
        )
        .with_code(-32602)
        .retry_safe(true));
    }
    let model = vmdl::template(template, params).map_err(vmdl_error)?;
    // A create is written exactly like an edit: template classes are snapshot-checked unless the
    // caller opted out or no snapshot exists (then the result says classesUnverified).
    let mut checked = check_classes(&model, &state, check)?;
    checked.warnings.extend(modeldoc_warning);
    io_fail(
        &format!("cannot create {}", host.display()),
        std::fs::create_dir_all(host.parent().unwrap_or_else(|| Path::new("/"))),
    )?;
    let backup = io_fail(
        &format!("cannot write {}", host.display()),
        vmdl::fsutil::write_atomic_with_backup(&host, &model.write()),
    )?;
    Ok(json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": host.display().to_string(),
        "written": true,
        "classesUnverified": !checked.verified,
        "backup": backup.map(|p| json!(p.display().to_string())).unwrap_or(Value::Null),
        "nodes": nodes_json(&model.list(None, None, None)),
        "warnings": checked.warnings,
        "nextStep": "model_open to view it, then model_compile with route resourcecompiler (a file created after the tools started has no editor asset; the editor route refuses with PrerequisiteFailed)",
    })
    .into())
}

// -------------------------------------------------------------------------------------------
// model_compile
// -------------------------------------------------------------------------------------------

/// Compile a `.vmdl` to `.vmdl_c`. Route `resourcecompiler` (default) runs `resourcecompiler.exe`
/// inside the ModelDoc container (works for files with no editor asset); route `editor` calls the
/// bridge's `modeldoc.compile` and reports success by the `.vmdl_c` mtime alone.
pub async fn model_compile(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Err(missing_arg("path"));
    };
    let host = state
        .content()
        .resolve_write(path, &state.path_map)
        .map_err(vmdl_error)?;
    if !host.is_file() {
        return Err(ToolError::invalid_arguments(format!(
            "{} does not exist",
            host.display()
        )));
    }
    let timeout = compile_timeout(&args, &ctx.budget)?;
    let save_first = args.get("saveFirst").and_then(Value::as_bool).unwrap_or(false);
    let out = match args.get("route").and_then(Value::as_str) {
        None | Some("resourcecompiler") => {
            compile_resourcecompiler(state, path, host, CompileTarget::MODELDOC, timeout, None).await?
        }
        Some("editor") => compile_editor(state, ctx, save_first, path, host, timeout).await?,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "route must be \"resourcecompiler\" or \"editor\", got {other:?}"
            )))
        }
    };
    Ok(with_engine_cache_advice(out))
}

/// What a compiled model's reader must know: the engine keeps a `.vmdl_c` it already loaded
/// (FIXME.md I1 live), so a recompile shows only in a fresh tools session and a recompiled map.
pub(crate) const ENGINE_CACHE_ADVICE: &str = "A running engine keeps a model it already loaded: after recompiling a model, restart the tools session (and recompile the map, which embeds models) to see the change";

/// Add [`ENGINE_CACHE_ADVICE`] to a successful compile as a note and a `compile_map` next step.
fn with_engine_cache_advice(mut out: ToolOutput) -> ToolOutput {
    if out.json["ok"] != json!(true) {
        return out;
    }
    match out.json.get_mut("notes").and_then(Value::as_array_mut) {
        Some(notes) => notes.push(json!(ENGINE_CACHE_ADVICE)),
        None => out.json["notes"] = json!([ENGINE_CACHE_ADVICE]),
    }
    out.with_next("compile_map", "a compiled map embeds its models: recompile it, then restart the tools session to see a changed model")
}

/// Route `editor`: requires the bridge's `modeldoc.compile`/`modeldoc.active`. Compiles only the
/// ACTIVE document (a doc whose mapped host path is the requested file), refuses documents with
/// no editor asset, and dirty documents without `saveFirst`. Success = the `.vmdl_c` mtime
/// advanced before the remaining budget ran out (facts round 2 §2: the editor never reports a
/// compile status; the bridge's reply only carries `savedFirst`).
async fn compile_editor(
    state: HammerState,
    ctx: CallCtx,
    save_first: bool,
    path: &str,
    host: PathBuf,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    require_caps(&state.bridge, &["modeldoc.compile", "modeldoc.active"]).await?;
    let mut bridge = state.bridge.lock().await;
    let active = bridge
        .request(
            "modeldoc.active",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    let document = match active.get("doc").filter(|d| d.is_object()) {
        Some(document) => document.clone(),
        None => {
            return Err(ToolError::new(
                "NoActiveDocument",
                "NoActiveDocument",
                "ModelDoc has no active document (open one with model_open or in the editor)",
            )
            .with_code(-32003)
            .retry_safe(true))
        }
    };
    let Some(doc_path) = document.get("path").and_then(Value::as_str) else {
        return Err(ToolError::new(
            "NotActiveDocument",
            "NotActiveDocument",
            "the active document reports no path (compile route editor cannot proceed)",
        )
        .retry_safe(true));
    };
    let Some(doc_host) = state.path_map.windows_to_host(doc_path) else {
        return Err(ToolError::new(
            "PathNotMapped",
            "PathNotMapped",
            format!("the active document path {doc_path:?} has no host mapping (check --path-map)"),
        )
        .with_code(-32602)
        .retry_safe(true));
    };
    // Compile only ever compiles the active session's document: the editor keeps several
    // sessions open and `modeldoc.active` follows whichever is focused, so anything else is
    // refused here rather than silently skipped.
    if !same_file(&doc_host, &host) {
        return Err(ToolError::new(
            "NotActiveDocument",
            "NotActiveDocument",
            format!(
                "{} is not the active document ({doc_path:?}); compile the active file or use \
                 route resourcecompiler",
                host.display()
            ),
        )
        .retry_safe(true));
    }
    if document.get("hasAsset").and_then(Value::as_bool) != Some(true) {
        // A file created after the tools started never binds an asset; the live bridge would
        // answer -32017 no_asset and do nothing, so refuse before sending.
        return Err(ToolError::new(
            "PrerequisiteFailed",
            "PrerequisiteFailed",
            "the active document has no editor asset (its file was created after the tools \
             started); compile it with route \"resourcecompiler\" instead",
        )
        .with_code(-32017)
        .with_data(json!({"reason": "no_asset"}))
        .retry_safe(true));
    }
    let dirty = match document.get("dirty").and_then(Value::as_bool) {
        Some(dirty) => dirty,
        None => {
            return Err(ToolError::new(
                "UnsavedChanges",
                "UnsavedChanges",
                "the active document's dirty state is unknown (pass saveFirst:true to save and \
                 compile anyway)",
            )
            .with_code(-32016)
            .retry_safe(true))
        }
    };
    if dirty && !save_first {
        return Err(ToolError::new(
            "UnsavedChanges",
            "UnsavedChanges",
            "the active document has unsaved changes (pass saveFirst:true to save and compile, \
             or reload/discard them first)",
        )
        .with_code(-32016)
        .retry_safe(true));
    }
    let output_path = state
        .content()
        .compiled_output(&host)
        .expect("the active document is inside the content root");
    let before = mtime_of(&output_path);
    let started = Instant::now();
    let reply = bridge
        .request(
            "modeldoc.compile",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?;
    drop(bridge);
    let saved_first = reply
        .result
        .get("savedFirst")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Poll the output's mtime until it moves, bounded by the budget and `timeoutMs`.
    let poll_deadline = Instant::now() + ctx.budget.remaining().min(timeout);
    let updated = poll_output_updated(&output_path, before, poll_deadline).await;
    Ok(json!({
        "route": "editor",
        "path": path,
        "ok": updated,
        "compiled": Value::Null,
        "failed": Value::Null,
        "skipped": Value::Null,
        "failLines": [],
        "fatal": false,
        "outputPath": output_path.display().to_string(),
        "outputUpdated": updated,
        "durationMs": started.elapsed().as_millis() as u64,
        "stdoutTail": "",
        "savedFirst": saved_first,
        "container": Value::Null,
        "okNote": if updated { Value::Null } else {
            json!(format!(
                "the .vmdl_c mtime never advanced within {} ms (budget-bounded); the editor \
                 never reports a compile status, so this compile produced no output",
                timeout.as_millis()
            ))
        },
        "nextStep": if updated { json!("the compiled output is fresh") } else {
            json!("check the editor is really compiling (asset bound, no modal); the resourcecompiler route works for any file")
        },
    })
    .into())
}

/// Same file? Canonical comparison when both exist, case-insensitive text otherwise (the
/// PathMap is case-insensitive on the Windows side by contract).
fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    a.to_string_lossy()
        .replace('\\', "/")
        .eq_ignore_ascii_case(&b.to_string_lossy().replace('\\', "/"))
}
