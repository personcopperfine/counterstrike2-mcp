//! Live particle-editor tools (`pet.*`, bridge m2.12): the *running* pet document through the
//! bridge, next to the `.vpcf` file route (`particle_file.rs`, WP4) and the layer in
//! [`crate::vpcf`] (WP3). The mock bridge's `pet.*` built-ins are the contract for the tests
//! (`tests/particle_live.rs`).
//!
//! Policies (pet plan §0.5, table 0.2; mirroring `model_live.rs`):
//!
//! * `particle_status` never raises: the bridge status section, `petLoaded` = the bridge advertises
//!   `pet.active` after one capabilities refresh, the `pet.*` methods that are missing, the active
//!   document (+ mapped host path, `inAddon`, the nine arrays mapped type -> file key via
//!   [`vpcf::ARRAYS`]) and a launch/open hint when `pet` is not running.
//! * `particle_load` calls `pet.load` once and then waits for the module to reach a terminal state
//!   (`verified`/`mismatch`) by polling `control.capabilities.modules["pet.dll"].state`, bounded by
//!   the call budget; a `mismatch` is `BuildMismatch` and a `loaded:false` reply is
//!   `PetLoadFailed` (a modal may be up; a failed `LoadToolSystem` is cached by the engine, so the
//!   tool never retries blindly and marks the error **not** retry-safe).
//! * `particle_open`/`particle_reload` validate on the host as the **last step before the send**
//!   (parse, structure, then the schema unless `skipSchema`): the dirty/unknown check runs first
//!   under the bridge lock, then the file is read + validated with nothing awaited in between, and
//!   only then is `pet.open`/`pet.reload` dispatched (TOCTOU guard: a file swapped in during the
//!   call is re-checked). A broken `.vpcf` pops a modal in pet's `OpenFile`, and a malformed file
//!   handed to `ReloadDocument` does not merely leave an empty document — it overflows the main
//!   thread's stack and wedges the whole game process (live fact B, 2026-10-13). A reload of a
//!   broken file is therefore refused, never attempted, and nothing is sent when the validation
//!   fails. Structural problems are never skippable. A caller-visible consequence of the order:
//!   a dirty-document refusal may come before a validation refusal.
//! * `particle_save` never opens Save As: untitled documents (`isNewFile`) and paths outside the
//!   active addon are refused. A clean document is skipped by the bridge unless `force:true`,
//!   which re-serializes it in pet's own form. `particle_types {writeSnapshot}` builds the
//!   `pet-schema.json` snapshot (0.3) from `pet.functionTypes` + `pet.schema`; a failed lookup
//!   aborts the write (never a partial snapshot).
//!
//! Every bridge request carries `RequestOpts::deadline(ctx.budget.deadline_ms()?)`; `-32003`,
//! `-32015`, `-32016`, `-32017`, `-32023` and `-32602` surface with their wire names through
//! `ToolError::from`.
use crate::state::HammerState;
use crate::vmdl::VmdlError;
use crate::vpcf::{self, Issue, Particle, Schema, ValidateOptions, VpcfError};
use cs2_mcp_common::bridge::{BridgeClient, BridgeError, Kind, RequestOpts};
use cs2_mcp_common::tools::{
    Budget, CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The seven `pet.*` bridge methods (m2.12, plan §0.2). `particle_status` reports which of these
/// the bridge does not advertise; the tools below declare their own subset and are gated on it by
/// the registry before the handler runs (nothing is sent when one is missing).
const PET_METHODS: [&str; 7] = [
    "pet.load",
    "pet.active",
    "pet.open",
    "pet.reload",
    "pet.save",
    "pet.functionTypes",
    "pet.schema",
];

/// The seven tools this module registers, in registration order. `PET_TOOL_ORDER` (written by the
/// orchestrator at merge time) places these after the six file-route tools.
pub const PARTICLE_LIVE_TOOL_ORDER: [&str; 7] = [
    "particle_status",
    "particle_types",
    "particle_schema",
    "particle_load",
    "particle_open",
    "particle_reload",
    "particle_save",
];

/// The four classes that are not particle functions but must be in the snapshot twice-checked:
/// the root definition, the two structural element classes and their config struct (the
/// `m_NamedValueLocals` entries' `m_DefaultConfig`).
const FIXED_SNAPSHOT_CLASSES: [&str; 4] = [
    vpcf::ROOT_CLASS,
    "ParticleChildrenInfo_t",
    "ParticleNamedValueSource_t",
    vpcf::namedvalue::CONFIG_CLASS,
];

/// The `pet-schema.json` snapshot `particle_types {writeSnapshot}` writes and the validator /
/// `particle_open`/`particle_reload` read: the repo's checkout of
/// `hammer-mcp/research/pet-schema.json` under `--project-root` (an installed binary has moved away
/// from `CARGO_MANIFEST_DIR`, so the path comes from state at runtime).
fn snapshot_path(state: &HammerState) -> PathBuf {
    cs2_mcp_common::mcp_repo_dir(&state.project_root)
        .join("hammer-mcp/research/pet-schema.json")
}

// ---------------------------------------------------------------- registration

/// Register the seven live pet tools (see `particle_live.rs` header). Gating: the registry refuses
/// a tool whose declared `pet.*` methods are not advertised, without sending anything.
pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "particle_status",
            "Particle editor (pet) status: whether pet.dll is loaded, missing capabilities, the active .vpcf document (+ host path, inAddon) and a launch/load hint. Never errors.",
            ReadOnly,
            &[],
        ),
        particle_status,
    );
    r.register(
        ToolSpec::new(
            "particle_types",
            "Every particle function class the running pet registers, grouped by array. writeSnapshot stores the full schema snapshot under hammer-mcp/research/.",
            Mutating,
            &["pet.functionTypes"],
        ),
        particle_types,
    );
    r.register(
        ToolSpec::new(
            "particle_schema",
            "Schema (bases, flattened fields, metadata) of one particle class, from the running pet if available, else the local snapshot.",
            ReadOnly,
            &[],
        ),
        particle_schema,
    );
    r.register(
        ToolSpec::new(
            "particle_load",
            "Load the particle editor tool into the running tools session and wait for pet.dll to verify. One shot: loaded:false means a modal may be up (restart the session).",
            Mutating,
            &["pet.load"],
        ),
        particle_load,
    );
    r.register(
        ToolSpec::new(
            "particle_open",
            "Open a .vpcf of the active addon in the running pet. Refuses while the live document is dirty unless force.",
            Mutating,
            &["pet.open", "pet.active"],
        ),
        particle_open,
    );
    r.register(
        ToolSpec::new(
            "particle_reload",
            "Reload the active pet document from disk after a file edit. A malformed file would crash pet, so it is validated first and nothing is sent if invalid. Refuses while dirty unless force.",
            Mutating,
            &["pet.reload", "pet.active"],
        )
        .destructive(true),
        particle_reload,
    );
    r.register(
        ToolSpec::new(
            "particle_save",
            "Save the active pet document to its own path (never Save As: refuses untitled documents and files outside the addon). force re-saves a clean document in pet's own form.",
            Writer,
            &["pet.save", "pet.active"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "force": {
                    "type": "boolean",
                    "description": "re-save a clean document too (pet re-serializes it)"
                }
            },
            "additionalProperties": true
        })),
        particle_save,
    );
}

// ---------------------------------------------------------------- small helpers (own file)

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!("{key}: a non-empty string is required"))
        })
}

pub(crate) fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `Some(true|false)` when the document reports it, `None` when it does not say (a missing or
/// non-boolean `dirty` counts as unknown: refuse rather than risk discarding edits).
fn dirty_state(doc: &Value) -> Option<bool> {
    doc.get("dirty").and_then(Value::as_bool)
}

pub(crate) fn issues_json(issues: &[Issue]) -> Vec<Value> {
    issues
        .iter()
        .map(|issue| {
            json!({
                "path": issue.path,
                "level": if issue.level.is_error() { "error" } else { "warning" },
                "msg": issue.msg,
            })
        })
        .collect()
}

/// A host-side validation failure (parse/structure/schema): nothing was dispatched, so the error
/// is retry-safe. One shape for open/reload: `ValidationFailed {data.issues}` (plan §0.5).
fn validation_failed(issues: &[Issue]) -> ToolError {
    let first = issues
        .iter()
        .find(|issue| issue.level.is_error())
        .map(|issue| format!("{}: {}", issue.path, issue.msg))
        .unwrap_or_else(|| "the particle file is not valid".to_string());
    ToolError::new(
        "InvalidParticle",
        "ValidationFailed",
        format!("refusing to hand pet a file it would fail to load ({first}); nothing was sent"),
    )
    .with_code(-32602)
    .retry_safe(true)
    .with_data(json!({"issues": issues_json(issues)}))
}

/// `-32023 SchemaUnavailable`: no usable schema snapshot. The built-in one is the live snapshot
/// taken 2026-10-13 (build 2000930), so this fires when the tree snapshot exists but is unusable
/// (a corrupt/partial write) and the caller did not pass `skipSchema:true`.
fn schema_unavailable(error: &VpcfError, snapshot: &Path, source: &str) -> ToolError {
    ToolError::new(
        "SchemaUnavailable",
        "SchemaUnavailable",
        format!(
            "no particle schema to validate against ({error}); the snapshot {} is not usable \
             (source: {source}). Run particle_types {{writeSnapshot:true}} in a live session, or \
             pass skipSchema:true to skip only the schema check",
            snapshot.display()
        ),
    )
    .with_code(-32023)
    .retry_safe(true)
    .with_data(json!({
        "snapshot": snapshot.display().to_string(),
        "schemaSource": source,
    }))
    .use_instead(
        "particle_types",
        "write the schema snapshot from a live session",
        Some(json!({"writeSnapshot": true})),
    )
}

/// Host path resolution failures for the `.vpcf` route, mapped like `model_live::validation_error`.
fn path_error(error: VmdlError) -> ToolError {
    match error {
        VmdlError::Path(msg) => {
            if msg.contains("only files of the active addon") {
                ToolError::new("InvalidParams", "WriteOutsideAddon", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else if msg.contains("has no host mapping") {
                ToolError::new("PathMap", "PathNotMapped", msg)
                    .with_code(-32602)
                    .retry_safe(true)
                    .with_data(json!({"note": "extend --path-map / CS2_MCP_PATH_MAP"}))
            } else {
                ToolError::invalid_arguments(msg)
            }
        }
        VmdlError::Io(error) => ToolError::new(
            "InvalidParticle",
            "ParseError",
            format!("cannot resolve the path: {error}"),
        )
        .retry_safe(true),
        other => ToolError::new(
            "InvalidParticle",
            "ValidationFailed",
            format!("cannot use the path: {other}"),
        )
        .retry_safe(true),
    }
}

/// The document as the client sees it: the `pet.active` object plus the mapped host path
/// (`session::doc_view`), `inAddon`, and the nine arrays mapped from the bridge's integer type
/// index to the file key ([`vpcf::ARRAYS`]) as `{<key>:[classes]}`.
pub(crate) fn document_view(state: &HammerState, doc: &Value) -> Value {
    let mut view = crate::tools::session::doc_view(state, Some(doc));
    if view.is_object() {
        let host = doc
            .get("path")
            .and_then(Value::as_str)
            .and_then(|p| state.path_map.windows_to_host(p));
        view["inAddon"] = json!(host.is_some_and(|h| state.content().in_addon(&h)));
        match doc.get("arrays").and_then(Value::as_array) {
            Some(rows) => {
                let mut mapped = Map::new();
                for row in rows {
                    let Some(r#type) = row.get("type").and_then(Value::as_u64) else {
                        continue;
                    };
                    let Some(key) = vpcf::array_key(r#type as usize) else {
                        continue;
                    };
                    mapped.insert(
                        key.to_string(),
                        row.get("classes").cloned().unwrap_or_else(|| json!([])),
                    );
                }
                view["arrays"] = Value::Object(mapped);
            }
            None => view["arrays"] = Value::Null,
        }
    }
    view
}

/// `-32016 UnsavedChanges` before open/reload (for `reload` the bridge answers the same code when
/// `dirty && !force`; the tool pre-checks so dirty-*unknown* is refused as well).
fn unsaved_changes(doc: &Value) -> ToolError {
    let (message, reason) = match dirty_state(doc) {
        Some(true) => (
            "the active pet document is dirty (unsaved in-editor edits); pass force:true to discard them",
            "document is dirty; pass force:true to discard",
        ),
        _ => (
            "the active document's dirty state is not reported; refusing to risk unsaved edits, pass force:true",
            "dirty state unreported; pass force:true to discard",
        ),
    };
    ToolError::new("UnsavedChanges", "UnsavedChanges", message)
        .with_code(-32016)
        .retry_safe(true)
        .with_data(json!({"reason": reason, "path": doc.get("path").unwrap_or(&Value::Null)}))
}

/// `pet.active` resolved to a document, or a no-document error shaped exactly like the wire's
/// `-32003 NoActiveDocument` (built through `BridgeError -> ToolError`, so type/name/wireName/code
/// are indistinguishable from a bridge-reported one).
pub(crate) async fn require_document(
    bridge: &mut BridgeClient,
    deadline: u64,
) -> Result<Value, ToolError> {
    let resp = bridge
        .request("pet.active", json!({}), RequestOpts::deadline(deadline))
        .await;
    let result = match resp {
        Ok(resp) => resp.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    match result.get("doc") {
        Some(doc) if doc.is_object() => Ok(doc.clone()),
        _ => {
            let reason = result
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no_document");
            Err(ToolError::from(
                BridgeError::new(
                    Kind::NoDocument,
                    format!("no active particle document ({reason}): open one with particle_open first"),
                )
                .with_code(-32003)
                .with_wire_name("NoActiveDocument")
                .with_data(json!({"reason": reason})),
            ))
        }
    }
}

/// The schema to validate against: the snapshot under `--project-root` when it loads, else the
/// built-in one; a placeholder/missing built-in is [`schema_unavailable`], never a pass.
fn load_schema(state: &HammerState) -> Result<Schema, ToolError> {
    let path = snapshot_path(state);
    match vpcf::known_schema(&path) {
        (Ok(schema), _) => Ok(schema),
        (Err(error), source) => Err(schema_unavailable(&error, &path, &source)),
    }
}

/// Host-side pre-validation shared by open/reload: parse, structure, then the schema unless
/// `skip_schema`. Nothing unvalidated ever reaches pet: a malformed `.vpcf` crashes pet
/// (ReloadDocument overflows the main thread's stack and wedges the whole game process — fact B,
/// 2026-10-13), which is why the file is parsed and validated here first and nothing is sent on
/// failure. Structural checks are never skippable (a wrong root `_class` or a non-array
/// function list is refused as well). Returns the parsed particle, or the concrete refusal.
fn validate_particle(
    state: &HammerState,
    host: &Path,
    skip_schema: bool,
) -> Result<Particle, ToolError> {
    let text = std::fs::read_to_string(host).map_err(|error| {
        ToolError::new(
            "InvalidParticle",
            "ParseError",
            format!("cannot read {}: {error}", host.display()),
        )
        .retry_safe(true)
    })?;
    let particle = match Particle::parse(&text) {
        Ok(particle) => particle,
        // a file pet cannot parse at all: report it as a validation issue, never send it
        Err(error) => {
            return Err(validation_failed(&[Issue::error("/", error.to_string())]));
        }
    };
    let issues = if skip_schema {
        particle.validate_structure()
    } else {
        let schema = load_schema(state)?;
        particle.validate(&schema, ValidateOptions { allow_obsolete: true })
    };
    if issues.iter().any(|issue| issue.level.is_error()) {
        return Err(validation_failed(&issues));
    }
    Ok(particle)
}

/// `value` as pretty JSON with a trailing newline, written `path.tmp-<pid>` then renamed
/// (atomic; the parent directory is created).
fn write_json_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// The module state `control.capabilities.modules["pet.dll"].state` reports, when present.
fn pet_module_state(bridge: &BridgeClient) -> Option<String> {
    bridge
        .capabilities
        .get("modules")
        .and_then(|modules| modules.get("pet.dll"))
        .and_then(|module| module.get("state"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `verified`/`mismatch` are terminal; `not_loaded`/`verifying` mean the module poll has not picked
/// the freshly loaded DLL up yet.
fn module_terminal(state: &str) -> bool {
    matches!(state, "verified" | "mismatch")
}

/// Wait for the freshly loaded `pet.dll` to reach a terminal module state, bounded by the call
/// budget. The state starts from the `pet.load` reply (the bridge knows it directly) and is then
/// refreshed from `control.capabilities`, whose modules list is what the engine updates as the
/// verification thread finishes. The refresh also publishes the six `NEED_PET` methods the load
/// just advertised, so a following gated call does not need its own re-fetch.
async fn resolve_module_state(bridge: &mut BridgeClient, budget: &Budget, initial: &str) -> String {
    let mut current = initial.to_string();
    if bridge.refresh_capabilities().await.is_ok() {
        if let Some(state) = pet_module_state(bridge) {
            current = state;
        }
    }
    while !module_terminal(&current) {
        let remaining = budget.remaining();
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(250))).await;
        if bridge.refresh_capabilities().await.is_err() {
            break;
        }
        if let Some(state) = pet_module_state(bridge) {
            current = state;
        }
    }
    current
}

/// `pet.load` answered `loaded:false`: the engine cached the failure and may be showing a modal.
fn pet_load_failed(reply: &Value) -> ToolError {
    ToolError::new(
        "PetLoadFailed",
        "PetLoadFailed",
        "pet.load did not load pet.dll (loaded:false); a modal dialog may be up in the tools window. \
         Dismiss it and restart the tools session before retrying (the engine caches a failed \
         LoadToolSystem, so an immediate retry cannot succeed)",
    )
    .retry_safe(false)
    .with_data(json!({
        "module": reply.get("module").cloned().unwrap_or(Value::Null),
        "toolIndex": reply.get("toolIndex").cloned().unwrap_or(Value::Null),
    }))
}

/// The module reached `mismatch`: pet.dll is running but does not match the pinned signatures.
fn build_mismatch(module: &str) -> ToolError {
    ToolError::new(
        "BuildMismatch",
        "BuildMismatch",
        format!(
            "pet.dll module state is {module:?}: the running binary does not match the pinned \
             signatures. Rebuild/install the bridge for this game build (workflows/game-update.md)"
        ),
    )
    .retry_safe(false)
    .with_data(json!({"moduleName": "pet.dll", "module": module}))
}

// ---------------------------------------------------------------- particle_status

/// `particle_status`: never raises. Mirrors `model_status` with the pet module, including the
/// active document view (arrays mapped to file keys).
pub async fn particle_status(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut out = Map::new();
    let mut bridge = state.bridge.lock().await;
    let section = cs2_mcp_common::status::collect(&mut bridge, &state.launch_command).await;
    out.extend(section.fields);
    let connected = section.connected;

    let missing = if connected {
        match bridge.refresh_capabilities().await {
            Ok(_) => bridge.missing(&PET_METHODS),
            Err(_) => PET_METHODS.iter().map(|m| m.to_string()).collect(),
        }
    } else {
        PET_METHODS.iter().map(|m| m.to_string()).collect()
    };
    match out.get_mut("capabilities").and_then(Value::as_object_mut) {
        Some(caps) => {
            caps.insert("missing".into(), json!(missing));
        }
        None => {
            out.insert("capabilities".into(), json!({"missing": missing}));
        }
    }
    // `petLoaded` is the bridge advertising `pet.active` after one capabilities refresh. When it is
    // not loaded, nothing `pet.*` may be probed (the bridge would answer MethodNotFound).
    let loaded = connected && bridge.has("pet.active");
    out.insert("petLoaded".into(), json!(loaded));

    let hint = if !connected {
        json!(state.launch_command)
    } else if !loaded {
        json!(format!("{}; then particle_load", state.launch_command))
    } else {
        Value::Null
    };
    out.insert("hint".into(), hint);

    if loaded {
        let deadline = ctx.budget.deadline_ms().unwrap_or(5_000);
        match bridge
            .request("pet.active", json!({}), RequestOpts::deadline(deadline))
            .await
        {
            Ok(resp) => match resp.result.get("doc").filter(|d| d.is_object()) {
                Some(doc) => out.insert("document".into(), document_view(&state, doc)),
                None => {
                    out.insert("document".into(), Value::Null);
                    out.insert(
                        "documentReason".into(),
                        resp.result
                            .get("reason")
                            .cloned()
                            .unwrap_or_else(|| json!("no_document")),
                    );
                    None
                }
            },
            // a blocked gate means a modal dialog owns the main thread: report, never raise
            Err(e) => {
                out.insert("document".into(), Value::Null);
                out.insert("documentError".into(), e.to_json());
                None
            }
        };
    }
    drop(bridge);
    Ok(Value::Object(out).into())
}

// ---------------------------------------------------------------- particle_load

/// `particle_load {}`: one-shot `pet.load`, then wait for the `pet.dll` module to verify. A
/// `loaded:false` result is an error (a modal may be up and the engine caches the failure),
/// `module:"mismatch"` is `BuildMismatch`.
pub async fn particle_load(
    state: HammerState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let reply = bridge
        .request(
            "pet.load",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    let result = reply.result;
    if result.get("loaded").and_then(Value::as_bool) != Some(true) {
        return Err(pet_load_failed(&result));
    }
    let initial = result
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let module = resolve_module_state(&mut bridge, &ctx.budget, &initial).await;
    if module == "mismatch" {
        return Err(build_mismatch(&module));
    }
    Ok(json!({
        "loaded": true,
        "alreadyLoaded": result.get("alreadyLoaded").cloned().unwrap_or(json!(false)),
        "toolIndex": result.get("toolIndex").cloned().unwrap_or(Value::Null),
        "module": module,
    })
    .into())
}

// ---------------------------------------------------------------- particle_types

/// `particle_types {writeSnapshot?, array?}`: the running pet's function classes, grouped by array.
/// `writeSnapshot` also fetches `pet.schema` for every class (plus the root and the two structural
/// classes) and writes the `pet-schema.json` snapshot (0.3) atomically; a failed lookup aborts the
/// write, so the snapshot is never partial.
pub async fn particle_types(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let write_snapshot = bool_arg(&args, "writeSnapshot");
    let filter = match args.get("array").and_then(Value::as_str) {
        Some(key) => Some(vpcf::array_type(key).ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "array: {key:?} is not one of the nine particle arrays ({})",
                vpcf::array_keys().join(", ")
            ))
        })?),
        None => None,
    };

    let mut bridge = state.bridge.lock().await;
    if write_snapshot && !bridge.has("pet.schema") {
        // fail closed: never send an unadvertised method; structural snapshot needs every class
        return Err(ToolError::not_implemented(
            "CapabilityMissing",
            "writeSnapshot needs pet.schema, which the bridge does not advertise (nothing was sent)",
            json!({"missing": ["pet.schema"]}),
        ));
    }
    let reply = bridge
        .request(
            "pet.functionTypes",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    let types = reply
        .result
        .get("types")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count = reply
        .result
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(types.len() as u64);

    let arrays: Vec<Value> = vpcf::ARRAYS
        .iter()
        .map(|(r#type, key, _element)| {
            let n = types
                .iter()
                .filter(|row| row.get("type").and_then(Value::as_u64) == Some(*r#type as u64))
                .count();
            json!({"type": r#type, "key": key, "count": n})
        })
        .collect();
    let functions: Vec<Value> = types
        .iter()
        .filter(|row| {
            filter.is_none_or(|r#type| row.get("type").and_then(Value::as_u64) == Some(r#type as u64))
        })
        .cloned()
        .collect();

    let build = bridge
        .version()
        .await
        .ok()
        .and_then(|v| v.get("gameBuild").cloned())
        .filter(|b| b.is_string())
        .unwrap_or(Value::Null);

    let mut out = json!({
        "count": count,
        "build": build,
        "arrays": arrays,
        "functions": functions,
    });
    if write_snapshot {
        let snapshot = build_snapshot(&mut bridge, &ctx.budget, &types, build.clone()).await?;
        let path = snapshot_path(&state);
        write_json_atomic(&path, &snapshot).map_err(|error| {
            ToolError::internal(format!("cannot write {}: {error}", path.display()))
        })?;
        out["snapshotPath"] = json!(path.display().to_string());
    }
    Ok(out.into())
}

/// The 0.3 snapshot document: `pet.schema` for every function class plus the three fixed classes
/// (per-class deadline from the budget). Any failure returns before anything is written.
async fn build_snapshot(
    bridge: &mut BridgeClient,
    budget: &Budget,
    types: &[Value],
    build: Value,
) -> Result<Value, ToolError> {
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for row in types {
        if let Some(class) = row.get("className").and_then(Value::as_str) {
            if !class.is_empty() {
                wanted.insert(class.to_string());
            }
        }
    }
    for class in FIXED_SNAPSHOT_CLASSES {
        wanted.insert(class.to_string());
    }

    let mut classes = Map::new();
    for class in &wanted {
        let reply = bridge
            .request(
                "pet.schema",
                json!({"class": class}),
                RequestOpts::deadline(budget.deadline_ms()?),
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            // abort: a snapshot written from a partial lookup would validate nothing
            Err(e) => return Err(ToolError::from(e)),
        };
        if reply.result.get("found").and_then(Value::as_bool) != Some(true) {
            return Err(ToolError::new(
                "SchemaUnavailable",
                "SchemaUnavailable",
                format!(
                    "pet.schema did not find {class:?} (found:false); the snapshot was not written"
                ),
            )
            .with_code(-32023)
            .retry_safe(true)
            .with_data(json!({"class": class})));
        }
        let field = |key: &str| reply.result.get(key).cloned().unwrap_or_else(|| json!([]));
        classes.insert(
            class.clone(),
            json!({
                "bases": field("bases"),
                "fields": field("fields"),
                "metadata": field("metadata"),
            }),
        );
    }

    let arrays: Vec<Value> = vpcf::ARRAYS
        .iter()
        .map(|(r#type, key, element)| json!({"type": r#type, "key": key, "elementClass": element}))
        .collect();
    let functions: Vec<Value> = types
        .iter()
        .map(|row| {
            json!({
                "type": row.get("type").cloned().unwrap_or(Value::Null),
                "className": row.get("className").cloned().unwrap_or(Value::Null),
                "name": row.get("name").cloned().unwrap_or(Value::Null),
                "obsolete": row.get("obsolete").cloned().unwrap_or(json!(false)),
                "singleton": row.get("singleton").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(json!({
        "build": build,
        "placeholder": false,
        "rootClass": vpcf::ROOT_CLASS,
        "arrays": arrays,
        "functions": functions,
        "classes": Value::Object(classes),
    }))
}

// ---------------------------------------------------------------- particle_schema

/// `particle_schema {class}`: the live `pet.schema` reply when the bridge advertises it, else the
/// class from the local snapshot. `source` says which was used.
pub async fn particle_schema(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let class = class_arg(&args)?;
    let mut bridge = state.bridge.lock().await;
    let live = bridge.ensure_connected().await.is_ok() && bridge.has("pet.schema");
    if live {
        let reply = bridge
            .request(
                "pet.schema",
                json!({"class": class}),
                RequestOpts::deadline(ctx.budget.deadline_ms()?),
            )
            .await;
        let mut result = match reply {
            Ok(reply) => reply.result,
            Err(e) => return Err(ToolError::from(e)),
        };
        if let Some(map) = result.as_object_mut() {
            map.insert("source".into(), json!("live"));
        }
        return Ok(result.into());
    }
    drop(bridge);
    let schema = load_schema(&state)?;
    let mut out = snapshot_class_json(&schema, class);
    out["source"] = json!("snapshot");
    Ok(out.into())
}

/// `class`: 1..127 chars, `[A-Za-z0-9_]` only (the bridge's own `pet.schema` rule; refuse locally
/// so a bad name never reaches the wire).
fn class_arg(args: &Value) -> Result<&str, ToolError> {
    let class = str_arg(args, "class")?;
    if class.len() > 127 || !class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ToolError::invalid_arguments(
            "class: 1..127 chars, [A-Za-z0-9_] only",
        ));
    }
    Ok(class)
}

/// One class from a loaded snapshot, shaped like the `pet.schema` reply (`size`/`project` are only
/// known live, so they are `null` here). `found:false` clears every other key.
fn snapshot_class_json(schema: &Schema, class: &str) -> Value {
    match schema.class(class) {
        Some(info) => json!({
            "class": class,
            "found": true,
            "size": Value::Null,
            "project": Value::Null,
            "bases": info.bases,
            "fields": info.fields.iter().map(|field| json!({
                "name": field.name, "type": field.r#type, "declaredIn": field.declared_in,
            })).collect::<Vec<Value>>(),
            "metadata": info.metadata,
        }),
        None => json!({
            "class": class, "found": false, "size": Value::Null, "project": Value::Null,
            "bases": [], "fields": [], "metadata": []
        }),
    }
}

// ---------------------------------------------------------------- particle_open

/// `particle_open {path, force?, skipSchema?}`: take the bridge lock, refuse a dirty/unknown
/// live document unless `force`, then pre-validate the file on the host **right before the
/// send** and hand `pet.open` the addon-relative path (`particles/x/y.vpcf`: pet's `OpenFile`
/// search paths are rooted at the addon). Validation is the last step before the wire on
/// purpose (no `await` in between): a writer between an earlier check and the send must not
/// reach pet unvalidated, and pet answers -32602 `UnloadableFile` on top with a lex check that
/// only the bridge's lexer gets right. This inverts the caller-visible order of two refusals
/// (a dirty document is now refused *before* a validation failure; both were previously
/// validation-before-dirty).
pub async fn particle_open(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = str_arg(&args, "path")?;
    let force = bool_arg(&args, "force");
    let skip_schema = bool_arg(&args, "skipSchema");

    let host = state
        .content()
        .resolve_write_ext(path, &state.path_map, "vpcf")
        .map_err(path_error)?;
    if !host.is_file() {
        return Err(ToolError::invalid_arguments(format!(
            "{path:?} does not exist on the host ({})",
            host.display()
        )));
    }
    let mut bridge = state.bridge.lock().await;
    let active = bridge
        .request(
            "pet.active",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let active = match active {
        Ok(reply) => reply.result,
        Err(e) => return Err(ToolError::from(e)),
    };
    if let Some(doc) = active.get("doc").filter(|d| d.is_object()) {
        if dirty_state(doc).unwrap_or(true) && !force {
            return Err(unsaved_changes(doc));
        }
    }
    // resolve_write_ext already pinned `host` under content/csgo_addons/<addon>/, so this cannot fail
    let Some(addon_path) = state.content().addon_rel(&host) else {
        return Err(ToolError::internal(format!(
            "{} resolved in the addon but has no addon-relative form",
            host.display()
        )));
    };
    // TOCTOU guard: read + validate now, after the dirty check, with nothing awaited before the
    // send — a file swapped in by particle_edit/another writer under our feet is re-checked here.
    validate_particle(&state, &host, skip_schema)?;
    let reply = bridge
        .request(
            "pet.open",
            json!({"path": addon_path.clone()}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    if reply.result.get("opened").and_then(Value::as_bool) != Some(true) {
        return Err(ToolError::new(
            "OpenFailed",
            "OpenFailed",
            format!(
                "pet reported opened:false for {addon_path:?} (OpenFile returned 0: re-entrancy guard or load failure)"
            ),
        )
        .retry_safe(true)
        .with_data(reply.result.clone()));
    }
    let document = match reply.result.get("active").filter(|d| d.is_object()) {
        Some(active) => document_view(&state, active),
        None => Value::Null,
    };
    Ok(json!({"opened": true, "path": path, "document": document}).into())
}

// ---------------------------------------------------------------- particle_reload

/// `particle_reload {force?, skipSchema?}`: refuse a dirty/unknown document unless `force`, then
/// pre-validate the live document's host file **right before the send** (nothing awaited in
/// between) and reload from disk. A malformed file crashes pet (ReloadDocument overflows the
/// main thread's stack and wedges the whole game process), so a file pet would fail on is
/// refused **before** anything is sent; a file swapped in during the call is caught by the
/// late validation (TOCTOU guard, same order as `particle_open`).
pub async fn particle_reload(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let force = bool_arg(&args, "force");
    let skip_schema = bool_arg(&args, "skipSchema");
    let mut bridge = state.bridge.lock().await;
    let doc = require_document(&mut bridge, ctx.budget.deadline_ms()?).await?;
    if dirty_state(&doc).unwrap_or(true) && !force {
        return Err(unsaved_changes(&doc));
    }
    let win_path = doc.get("path").and_then(Value::as_str).unwrap_or("");
    let host = state.path_map.windows_to_host(win_path).ok_or_else(|| {
        ToolError::new(
            "PathMap",
            "PathNotMapped",
            format!(
                "the live document's path {win_path:?} has no host mapping (extend --path-map); \
                 refusing to reload a file that cannot be checked first"
            ),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({"path": win_path}))
    })?;
    validate_particle(&state, &host, skip_schema)?;

    let reply = bridge
        .request(
            "pet.reload",
            json!({"force": force}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    Ok(json!({
        "reloaded": reply.result.get("reloaded").cloned().unwrap_or(Value::Null),
        "hasRoot": reply.result.get("hasRoot").cloned().unwrap_or(Value::Null),
        "path": reply.result.get("path").cloned().unwrap_or_else(|| json!(win_path)),
        "discardedChanges": reply.result.get("discardedChanges").cloned().unwrap_or(Value::Null),
    })
    .into())
}

// ---------------------------------------------------------------- particle_save

/// `particle_save {force?}`: refuse untitled documents (Save As modal) and read-only (outside-addon)
/// paths, then `pet.save` on the document's own path. The bridge skips a clean document
/// (`skippedClean:true`, nothing written) unless `force:true`, which re-serializes it in pet's own
/// form; `force:true` is only put on the wire when it was asked for. `forced` in the result echoes
/// whether this call carried it.
pub async fn particle_save(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let force = bool_arg(&args, "force");
    let mut bridge = state.bridge.lock().await;
    let doc = require_document(&mut bridge, ctx.budget.deadline_ms()?).await?;
    if doc.get("isNewFile").and_then(Value::as_bool) == Some(true) {
        return Err(ToolError::new(
            "UntitledDocument",
            "UntitledDocument",
            "the active document is untitled: saving would open the modal Save As; open the file with particle_open first",
        )
        .with_code(-32015)
        .retry_safe(true)
        .with_data(json!({"path": doc.get("path").cloned().unwrap_or(Value::Null)})));
    }
    let win_path = doc.get("path").and_then(Value::as_str).unwrap_or("");
    let host = state.path_map.windows_to_host(win_path);
    if !host.as_ref().is_some_and(|h| state.content().in_addon(h)) {
        let note = if host.is_none() {
            "the document's path has no --path-map mapping, so it cannot be shown editable"
        } else {
            "a particle system outside content/csgo_addons/<addon> is [ READ ONLY ] in the editor"
        };
        return Err(ToolError::new(
            "ReadOnlyParticle",
            "ReadOnlyParticle",
            format!("refusing to save {win_path:?}: {note}; nothing was sent"),
        )
        .retry_safe(true)
        .with_data(json!({"path": win_path, "note": note})));
    }
    let reply = bridge
        .request(
            "pet.save",
            pet_save_params(force),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Err(ToolError::from(e)),
    };
    Ok(json!({
        "saved": reply.result.get("saved").cloned().unwrap_or(Value::Null),
        "skippedClean": reply.result.get("skippedClean").cloned().unwrap_or(Value::Null),
        "forced": force,
        "path": reply
            .result
            .get("path")
            .filter(|p| !p.as_str().unwrap_or_default().is_empty())
            .cloned()
            .unwrap_or_else(|| json!(win_path)),
    })
    .into())
}

/// The `pet.save` params: `{"force":true}` only when the caller asked to force, `{}` as before
/// otherwise (a clean document must keep being skipped without it).
fn pet_save_params(force: bool) -> Value {
    if force {
        json!({"force": true})
    } else {
        json!({})
    }
}
