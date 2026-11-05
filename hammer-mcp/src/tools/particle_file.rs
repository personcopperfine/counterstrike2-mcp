//! File-route particle tools: read/list/validate/edit/create/compile `.vpcf` sources on disk.
//!
//! Facts these encode (`hammer-mcp/research/pet-re.md`, plan §0.3/§0.4/§0.5):
//! * a `.vpcf` is KV3 text in the `vpcf66` format whose root object **is** the
//!   `CParticleSystemDefinition`; the nine function arrays are the single type-index <-> key
//!   mapping ([`vpcf::ARRAYS`]);
//! * edits are all-or-nothing ([`vpcf::Particle::apply`]) and written through
//!   [`crate::vmdl::fsutil::write_atomic_with_backup`] (backup beside the file, atomic rename);
//! * writes land only in the active addon
//!   ([`ContentPaths::resolve_write_ext`](crate::vmdl::ContentPaths::resolve_write_ext)); reads
//!   anywhere under `content/`
//!   ([`ContentPaths::resolve_read_ext`](crate::vmdl::ContentPaths::resolve_read_ext), extension
//!   `vpcf`);
//! * the schema check runs against the snapshot `<repo>/hammer-mcp/research/pet-schema.json`
//!   when that file is usable, else the built-in copy of the same snapshot
//!   ([`vpcf::known_schema`]; the built-in is the **live snapshot** taken 2026-10-13 on build
//!   2000930 — 379 classes, `placeholder:false`, written by `particle_types {writeSnapshot:true}`
//!   — so validation works offline out of the box). A snapshot
//!   that is unusable everywhere (malformed, or `placeholder:true`) still fails closed with
//!   `SchemaUnavailable` — pass `skipSchema:true` where the tool has that knob (`particle_edit`)
//!   to skip only the schema part;
//! * `particle_compile` runs `resourcecompiler.exe` in any tools container (`cs2mcp.tool`, the
//!   pet session is the default `cs2mcp.tool=hammer`), the same route `model_compile` uses for
//!   `.vmdl`; `ok:false` is a result, never an error.
//!
//! The live half (`particle_open`/`reload`/`save`/…) lives in `particle_live.rs` (WP5);
//! `particle_status` mirrors `model_status` there.
use super::compile::{self, CompileTarget};
use crate::state::HammerState;
use crate::vmdl;
use crate::vpcf::{self, FunctionRef, Issue, Level, Op, Particle, ValidateOptions, VpcfError};
use cs2_mcp_common::tools::{
    CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// `particle_edit`'s op bound per call (sequential application against one tree), the
/// `model_edit` bound.
pub const MAX_OPS: usize = 256;

/// Where the particle schema snapshot lives, below this repo's checkout under `--project-root`
/// ([`cs2_mcp_common::mcp_repo_dir`]). Read first by the validator; the built-in copy of the same
/// file is the fallback.
const SNAPSHOT_REL: &str = "hammer-mcp/research/pet-schema.json";

// -------------------------------------------------------------------------------------------
// Errors
// -------------------------------------------------------------------------------------------

/// [`VpcfError`] -> [`ToolError`]. Every particle-layer failure happens before any byte is written
/// (ops run against an in-memory clone, writes are atomic), so these are retry-safe except I/O on
/// a path whose state is no longer certain.
pub fn vpcf_error(error: VpcfError) -> ToolError {
    match error {
        VpcfError::Parse(parse) => ToolError::new(
            "ParseError",
            "ParseError",
            format!("cannot parse: {parse}"),
        )
        .with_data(json!({"line": parse.line, "col": parse.col}))
        .retry_safe(true),
        VpcfError::Invalid(issues) => {
            let first = issues
                .first()
                .map(|issue| format!("{}: {}", issue.path, issue.msg))
                .unwrap_or_default();
            let listed: Vec<Value> = issues
                .iter()
                .map(|issue| json!({"path": issue.path, "msg": issue.msg}))
                .collect();
            ToolError::new(
                "ValidationFailed",
                "ValidationFailed",
                format!("invalid particle: {first}"),
            )
            .with_code(-32602)
            .with_data(json!({"issues": listed}))
            .retry_safe(true)
        }
        VpcfError::UnknownArray(array) => ToolError::new(
            "InvalidArray",
            "InvalidArray",
            format!(
                "{array:?} is not a function array (expected one of: {})",
                vpcf::array_keys().join(", ")
            ),
        )
        .with_code(-32602)
        .with_data(json!({"array": array, "expected": vpcf::array_keys()}))
        .retry_safe(true),
        VpcfError::FunctionNotFound(path) => {
            ToolError::new("FunctionNotFound", "FunctionNotFound", format!("no function at {path}"))
                .with_code(-32602)
                .with_data(json!({"path": path}))
                .retry_safe(true)
        }
        VpcfError::BadOp { index, msg } => ToolError::new(
            "InvalidOp",
            "InvalidOp",
            format!("bad op at index {index}: {msg}"),
        )
        .with_code(-32602)
        .with_data(json!({"opIndex": index, "detail": msg}))
        .retry_safe(true),
        VpcfError::Schema(msg) => schema_unavailable(
            "schema_invalid",
            format!("the particle schema snapshot is malformed: {msg}"),
        ),
        VpcfError::SchemaPlaceholder => schema_unavailable(
            "placeholder",
            "the particle schema is still the committed placeholder (no classes)".to_string(),
        ),
        VpcfError::Io(error) => {
            ToolError::new("IoError", "IoError", error.to_string()).retry_safe(false)
        }
        VpcfError::Path(msg) => {
            // `ContentPaths` messages distinguish these by wording (as in `model_file.rs`)
            if msg.contains("no host mapping") {
                ToolError::new("PathNotMapped", "PathNotMapped", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else if msg.contains("of the active addon can be written") {
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

/// [`ContentPaths::resolve_read_ext`](crate::vmdl::ContentPaths::resolve_read_ext) /
/// [`resolve_write_ext`](crate::vmdl::ContentPaths::resolve_write_ext) -> [`ToolError`]: the
/// content-path layer reports [`VmdlError`], which distinguishes its refusals by wording.
fn path_error(error: crate::vmdl::VmdlError) -> ToolError {
    use crate::vmdl::VmdlError;
    match error {
        VmdlError::Path(msg) => {
            if msg.contains("no host mapping") {
                ToolError::new("PathNotMapped", "PathNotMapped", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else if msg.contains("of the active addon can be written") {
                ToolError::new("WriteOutsideAddon", "WriteOutsideAddon", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            } else {
                ToolError::new("InvalidPath", "InvalidPath", msg)
                    .with_code(-32602)
                    .retry_safe(true)
            }
        }
        VmdlError::Io(error) => {
            ToolError::new("IoError", "IoError", error.to_string()).retry_safe(false)
        }
        other => ToolError::internal(format!("unexpected content-path error: {other}")),
    }
}

/// `-32023 SchemaUnavailable`: no usable particle schema (the committed placeholder, or a
/// malformed snapshot). Never "validate against nothing and report ok".
fn schema_unavailable(reason: &str, detail: String) -> ToolError {
    ToolError::new(
        "SchemaUnavailable",
        "SchemaUnavailable",
        format!("{detail}; no schema check is possible (run the live snapshot step, or use skipSchema where offered)"),
    )
    .with_code(-32023)
    .with_data(json!({"reason": reason, "snapshot": SNAPSHOT_REL}))
    .retry_safe(true)
}

/// A local I/O failure with the file named.
fn io_fail<T>(what: &str, result: std::io::Result<T>) -> Result<T, ToolError> {
    result.map_err(|error| {
        ToolError::new("IoError", "IoError", format!("{what}: {error}")).retry_safe(true)
    })
}

pub(crate) fn missing_arg(name: &str) -> ToolError {
    ToolError::invalid_arguments(format!("{name} (string) is required"))
}

/// `path`/`array`/`class`-style required string argument.
fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_arg(name))
}

/// `optional` string argument: `None` when absent or null, an error for any other type.
pub(crate) fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// Boolean argument with a default; a non-boolean value is an argument error.
pub(crate) fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// `array?` argument: it must be one of the nine keys (a typo must not look like "no entries").
fn array_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match optional_str(args, name)? {
        None => Ok(None),
        Some(key) if vpcf::array_type(key).is_some() => Ok(Some(key)),
        Some(key) => Err(ToolError::invalid_arguments(format!(
            "{name} {key:?} is not a function array (expected one of: {})",
            vpcf::array_keys().join(", ")
        ))),
    }
}

/// The snapshot path this server reads first ([`SNAPSHOT_REL`] under `--project-root`).
fn snapshot_path(state: &HammerState) -> PathBuf {
    cs2_mcp_common::mcp_repo_dir(&state.project_root).join(SNAPSHOT_REL)
}

/// Read a `.vpcf` from disk into a [`Particle`] (the extension/content rules of the read route).
fn read_particle(state: &HammerState, path: &str) -> Result<(PathBuf, Particle), ToolError> {
    let host = state
        .content()
        .resolve_read_ext(path, &state.path_map, "vpcf")
        .map_err(path_error)?;
    let text =
        io_fail(&format!("cannot read {}", host.display()), std::fs::read_to_string(&host))?;
    let particle = Particle::parse(&text).map_err(vpcf_error)?;
    Ok((host, particle))
}

/// The root's non-array fields as JSON (`_class` included: it is a root key, not an array).
pub(crate) fn root_fields(particle: &Particle) -> Value {
    let mut fields = Map::new();
    for (key, value) in particle.root().iter() {
        if vpcf::array_type(key).is_some() {
            continue;
        }
        fields.insert(key.to_string(), cs2_kv3::to_json(value));
    }
    Value::Object(fields)
}

/// How many entries each array the file actually has holds (`{<key>: n}`); an absent array key is
/// not reported (an empty one is).
pub(crate) fn counts(particle: &Particle) -> Value {
    let mut counts = Map::new();
    for (_, key, _) in vpcf::ARRAYS {
        if let Some(value) = particle.root().get(key) {
            let entries = value.as_array().map(<[cs2_kv3::Value]>::len).unwrap_or(0);
            counts.insert(key.to_string(), json!(entries));
        }
    }
    Value::Object(counts)
}

/// The schema to validate against: the tree snapshot first, the built-in second
/// ([`vpcf::known_schema`]). `Err` is `SchemaUnavailable` (the caller never proceeds against
/// nothing); the second value names the source that was used.
fn schema_for(state: &HammerState) -> Result<(vpcf::Schema, String), ToolError> {
    let (schema, source) = vpcf::known_schema(&snapshot_path(state));
    schema.map_err(vpcf_error).map(|schema| (schema, source))
}

/// The issues of `issues` with [`Level::Error`], plus their `{path, msg}` wire form.
fn split_issues(issues: &[Issue]) -> (Vec<&Issue>, Value) {
    let errors: Vec<&Issue> = issues.iter().filter(|issue| issue.level.is_error()).collect();
    let listed: Vec<Value> = errors
        .iter()
        .map(|issue| json!({"path": issue.path, "msg": issue.msg}))
        .collect();
    (errors, Value::Array(listed))
}

// -------------------------------------------------------------------------------------------
// particle_read / particle_list
// -------------------------------------------------------------------------------------------

/// Read a `.vpcf` from disk (file route, `source:"file"`): the header, the root's non-array
/// fields, the per-array entry counts and, with `function:"m_Operators/2"`, that one entry.
pub async fn particle_read(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
    let function = match optional_str(&args, "function")? {
        None => None,
        Some(reference) => Some(reference.parse::<FunctionRef>().map_err(|error| {
            ToolError::invalid_arguments(format!("function: {error}"))
        })?),
    };
    let (host, particle) = read_particle(&state, path)?;

    let mut out = Map::new();
    out.insert("source".into(), json!("file"));
    out.insert("path".into(), json!(path));
    out.insert("hostPath".into(), json!(host.display().to_string()));
    out.insert("header".into(), json!(particle.doc.header.raw));
    out.insert("root".into(), root_fields(&particle));
    out.insert("counts".into(), counts(&particle));
    if let Some(function) = function {
        out.insert(
            "function".into(),
            particle.function_json(&function).map_err(vpcf_error)?,
        );
    }
    Ok(Value::Object(out).into())
}

/// Flat list of a `.vpcf`'s function entries from disk, filterable by `array` (one of the nine
/// keys) and `class` (`_class` equality; `null` class on `_class`-less structural entries).
pub async fn particle_list(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
    let array = array_arg(&args, "array")?;
    let class = optional_str(&args, "class")?;
    let (host, particle) = read_particle(&state, path)?;

    let functions: Vec<Value> = particle
        .list(array, class)
        .iter()
        .map(|info| {
            json!({
                "array": info.array,
                "type": info.r#type,
                "index": info.index,
                "class": info.class,
                "fieldKeys": info.field_keys,
            })
        })
        .collect();
    Ok(json!({
        "source": "file",
        "path": path,
        "hostPath": host.display().to_string(),
        "functions": functions,
        "count": functions.len(),
        "counts": counts(&particle),
    })
    .into())
}

// -------------------------------------------------------------------------------------------
// particle_validate
// -------------------------------------------------------------------------------------------

/// Validate a `.vpcf` on disk against the particle schema snapshot: structural problems, root
/// fields, array registration and every entry's fields. `ok` is true only when there is no error
/// (an obsolete class is a warning unless `allowObsolete:false`, which makes it an error).
pub async fn particle_validate(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
    let allow_obsolete = bool_arg(&args, "allowObsolete", true)?;
    let (host, particle) = read_particle(&state, path)?;
    let (schema, source) = schema_for(&state)?;
    let issues = particle.validate(&schema, ValidateOptions { allow_obsolete });
    let (errors, listed) = split_issues(&issues);
    let warnings: Vec<Value> = issues
        .iter()
        .filter(|issue| !issue.level.is_error())
        .map(|issue| json!({"path": issue.path, "msg": issue.msg}))
        .collect();
    Ok(json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": host.display().to_string(),
        "ok": errors.is_empty(),
        "errors": listed,
        "warnings": warnings,
        "schemaSource": source,
        "allowObsolete": allow_obsolete,
    })
    .into())
}

// -------------------------------------------------------------------------------------------
// particle_edit
// -------------------------------------------------------------------------------------------

/// The shared edit core of `particle_edit` and `particle_live_edit` (WP-T1): the ops are applied
/// all-or-nothing against an in-memory [`Particle`], then checked (structure always, the schema
/// unless `skip_schema`). No I/O happens here — the caller decides where the resulting text goes
/// (a file write or `pet.apply` to the live document).
pub(crate) struct EditOutcome {
    /// `{"op": <name>, "path": <reference>}` per applied op, in order.
    pub applied: Vec<Value>,
    /// Human-readable warnings (schema warnings, the `skipSchema` note).
    pub warnings: Vec<String>,
    /// The schema source that was used; `null` when the check was skipped.
    pub schema_source: Value,
}

/// Apply + check `ops` against `particle` in memory. On `Err` nothing went anywhere: the ops ran
/// against a clone (the particle is unchanged on failure) and no bytes were written or sent.
pub(crate) fn edit_in_memory(
    state: &HammerState,
    particle: &mut Particle,
    ops: &[Op],
    skip_schema: bool,
    allow_obsolete: bool,
) -> Result<EditOutcome, ToolError> {
    let results = particle.apply(ops).map_err(vpcf_error)?;

    let mut warnings: Vec<String> = Vec::new();
    let schema_source = if skip_schema {
        let issues = particle.validate_structure();
        let (errors, listed) = split_issues(&issues);
        if !errors.is_empty() {
            return Err(validation_failed(listed));
        }
        warnings.push(
            "skipSchema: true; classes and fields were not checked against the schema".to_string(),
        );
        Value::Null
    } else {
        let (schema, source) = schema_for(state)?;
        let issues = particle.validate(&schema, ValidateOptions { allow_obsolete });
        let (errors, listed) = split_issues(&issues);
        if !errors.is_empty() {
            return Err(validation_failed(listed));
        }
        for issue in issues.iter().filter(|issue| issue.level == Level::Warning) {
            warnings.push(format!("{}: {}", issue.path, issue.msg));
        }
        json!(source)
    };
    let applied: Vec<Value> = results
        .iter()
        .map(|result| json!({"op": result.op, "path": result.path}))
        .collect();
    Ok(EditOutcome {
        applied,
        warnings,
        schema_source,
    })
}

/// Edit a `.vpcf` in the active addon: ops applied all-or-nothing, then checked (structure always,
/// the schema unless `skipSchema`) and written with a backup + atomic replace.
///
/// A structural or schema error refuses the write (`ValidationFailed {data.issues}`); the file and
/// its directory are untouched (no backup, no temporary). `skipSchema:true` skips only the schema
/// check — the structural checks and the op bounds stay — and the result says
/// `schemaChecked:false`.
pub async fn particle_edit(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
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
    let skip_schema = bool_arg(&args, "skipSchema", false)?;
    let allow_obsolete = bool_arg(&args, "allowObsolete", true)?;

    let host = state
        .content()
        .resolve_write_ext(path, &state.path_map, "vpcf")
        .map_err(path_error)?;
    let original =
        io_fail(&format!("cannot read {}", host.display()), std::fs::read_to_string(&host))?;
    let mut particle = Particle::parse(&original).map_err(vpcf_error)?;
    let outcome = edit_in_memory(&state, &mut particle, &ops, skip_schema, allow_obsolete)?;

    let backup = io_fail(
        &format!("cannot write {}", host.display()),
        vmdl::fsutil::write_atomic_with_backup(&host, &particle.write()),
    )?;
    Ok(json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": host.display().to_string(),
        "applied": outcome.applied,
        "appliedCount": outcome.applied.len(),
        "backup": backup.map(|p| json!(p.display().to_string())).unwrap_or(Value::Null),
        "warnings": outcome.warnings,
        "schemaChecked": !skip_schema,
        "schemaSource": outcome.schema_source,
        "nextStep": "particle_reload if the file is open in pet",
    })
    .into())
}

/// `ValidationFailed`: the ops were fine but the resulting document is not (`{data.issues}`).
/// Nothing is written; retrying with different ops cannot corrupt anything.
fn validation_failed(issues: Value) -> ToolError {
    let first = issues
        .as_array()
        .and_then(|list| list.first())
        .map(|issue| {
            format!(
                "{}: {}",
                issue.get("path").and_then(Value::as_str).unwrap_or("/"),
                issue.get("msg").and_then(Value::as_str).unwrap_or("")
            )
        })
        .unwrap_or_default();
    ToolError::new(
        "ValidationFailed",
        "ValidationFailed",
        format!("refusing to write: {first}"),
    )
    .with_code(-32602)
    .with_data(json!({"issues": issues}))
    .retry_safe(true)
}

// -------------------------------------------------------------------------------------------
// particle_create
// -------------------------------------------------------------------------------------------

/// Create a new `.vpcf` in the active addon from a template (`minimal`); refuses an existing file
/// unless `overwrite` (which then goes through the same backup + atomic replace as `particle_edit`).
pub async fn particle_create(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
    let kind = optional_str(&args, "template")?.unwrap_or("minimal");
    let empty = json!({});
    let params = args.get("params").filter(|value| !value.is_null()).unwrap_or(&empty);
    let overwrite = bool_arg(&args, "overwrite", false)?;

    let host = state
        .content()
        .resolve_write_ext(path, &state.path_map, "vpcf")
        .map_err(path_error)?;
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
    let particle = vpcf::template(kind, params).map_err(|error| match error {
        VpcfError::Invalid(issues) => ToolError::invalid_arguments(
            issues
                .iter()
                .map(|issue| format!("template {kind:?}: {}", issue.msg))
                .collect::<Vec<_>>()
                .join("; "),
        ),
        other => vpcf_error(other),
    })?;
    // The template's classes are checked against the schema when one is usable; the committed
    // placeholder is a warning-shaped omission (the file route must work before the live step).
    let (classes_verified, mut warnings) = check_template_classes(&state)?;
    io_fail(
        &format!("cannot create {}", host.display()),
        std::fs::create_dir_all(host.parent().unwrap_or_else(|| Path::new("/"))),
    )?;
    let backup = io_fail(
        &format!("cannot write {}", host.display()),
        vmdl::fsutil::write_atomic_with_backup(&host, &particle.write()),
    )?;
    if !classes_verified {
        warnings.push(
            "no usable particle schema snapshot: the template's classes were not verified"
                .to_string(),
        );
    }
    let created = json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": host.display().to_string(),
        "created": true,
        "backup": backup.map(|p| json!(p.display().to_string())).unwrap_or(Value::Null),
        "classes": vpcf::TEMPLATE_CLASSES,
        "classesVerified": classes_verified,
        "warnings": warnings,
    });
    Ok(ToolOutput::from(created)
        .with_next_args("particle_open", "load it in pet", json!({"path": path}))
        .with_next_args("particle_compile", "build the .vpcf_c", json!({"path": path})))
}

/// Every [`vpcf::TEMPLATE_CLASSES`] entry must be a class of the schema; with no usable schema the
/// template still writes (`classesVerified:false`, the caller adds the warning) — the class names
/// come from the live probe file that compiled, so a missing one is a real mismatch.
fn check_template_classes(state: &HammerState) -> Result<(bool, Vec<String>), ToolError> {
    let (schema, source) = match schema_for(state) {
        Ok(loaded) => loaded,
        Err(_) => return Ok((false, Vec::new())),
    };
    for class in vpcf::TEMPLATE_CLASSES {
        if schema.class(class).is_none() {
            return Err(ToolError::new(
                "UnknownClass",
                "UnknownClass",
                format!("the template emits {class:?}, which the schema has no class for"),
            )
            .with_data(json!({"class": class, "snapshot": source}))
            .retry_safe(true));
        }
    }
    Ok((true, Vec::new()))
}

// -------------------------------------------------------------------------------------------
// particle_compile
// -------------------------------------------------------------------------------------------

/// Most files one `particle_compile` run takes (a folder plus the children it pulls in).
pub const MAX_COMPILE_FILES: usize = 500;

/// Compile a `.vpcf` (or a folder of them) to `.vpcf_c` with `resourcecompiler.exe` inside any
/// tools container (the same verdict rules as `model_compile`'s route `resourcecompiler`;
/// `ok:false` is a result). `path` is a file, or a folder of the active addon (`recursive`
/// descends); `children` (default true) also compiles every `m_Children[].m_ChildRef` the files
/// reference, each once, children before their parents. A single file with no children keeps the
/// one-file result; anything else is one compiler run with a per-file `files` verdict list.
pub async fn particle_compile(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let path = required_str(&args, "path")?;
    let recursive = bool_arg(&args, "recursive", false)?;
    let children = bool_arg(&args, "children", true)?;
    let force = Some(bool_arg(&args, "force", false)?);
    let is_file = path.to_ascii_lowercase().ends_with(".vpcf");
    let timeout = compile::compile_timeout(&args, &ctx.budget)?;

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut skipped_links: Vec<String> = Vec::new();
    if is_file {
        if recursive {
            return Err(ToolError::invalid_arguments(
                "recursive applies to a folder path; this path is a single .vpcf",
            ));
        }
        let host = state
            .content()
            .resolve_write_ext(path, &state.path_map, "vpcf")
            .map_err(path_error)?;
        if !host.is_file() {
            return Err(ToolError::invalid_arguments(format!(
                "{} does not exist",
                host.display()
            )));
        }
        roots.push(host);
    } else {
        let dir = state
            .content()
            .resolve_write_dir(path, &state.path_map)
            .map_err(path_error)?;
        if !dir.is_dir() {
            // a wrong extension keeps the file route's wording ("is not a .vpcf file")
            state
                .content()
                .resolve_write_ext(path, &state.path_map, "vpcf")
                .map_err(path_error)?;
            return Err(ToolError::invalid_arguments(format!(
                "{} is neither a .vpcf file nor an existing folder of the active addon",
                dir.display()
            )));
        }
        collect_vpcf(&dir, recursive, &mut roots, &mut skipped_links)?;
        roots.sort();
        if roots.is_empty() {
            return Err(ToolError::invalid_arguments(format!(
                "no .vpcf files in {}{}",
                dir.display(),
                if recursive { "" } else { " (recursive:true descends into sub-folders)" }
            )));
        }
    }

    let mut plan = Plan::default();
    for root in &roots {
        plan.visit(&state, root, None, children).await?;
    }
    if plan.order.len() + plan.missing.len() > MAX_COMPILE_FILES {
        return Err(ToolError::invalid_arguments(format!(
            "{} files to compile exceeds the {MAX_COMPILE_FILES}-file limit; compile a smaller folder",
            plan.order.len() + plan.missing.len()
        )));
    }

    // the plain single-file case keeps its long-standing result shape
    if is_file && plan.order.len() == 1 && plan.missing.is_empty() && plan.external.is_empty() {
        let file = plan.order.remove(0);
        let mut out =
            compile::compile_resourcecompiler(state, path, file.host, CompileTarget::ANY_TOOLS, timeout, force)
                .await?;
        out.json["files"] = json!([{
            "path": file.rel,
            "ok": out.json["ok"],
            "skippedByCompiler": out.json["skippedByCompiler"],
            "reason": out.json["reason"],
            "upToDate": out.json["upToDate"],
            "failLines": out.json["failLines"],
        }]);
        out.json["children"] = json!(children);
        add_notes(&mut out.json, plan_notes(&skipped_links, plan.game_note.as_deref()));
        return Ok(out);
    }
    let game_note = plan.game_note.take();
    let mut out = compile::compile_resourcecompiler_set(
        state,
        plan.order,
        plan.missing.into_iter().chain(plan.external).collect(),
        CompileTarget::ANY_TOOLS,
        timeout,
        force,
    )
    .await?;
    out.json["path"] = json!(path);
    out.json["recursive"] = json!(recursive);
    out.json["children"] = json!(children);
    add_notes(&mut out.json, plan_notes(&skipped_links, game_note.as_deref()));
    Ok(out)
}

/// Notes for the result: symlinks the folder walk did not follow, a VPK read problem.
fn plan_notes(skipped_links: &[String], game_note: Option<&str>) -> Vec<Value> {
    let mut notes = Vec::new();
    if !skipped_links.is_empty() {
        notes.push(json!(format!(
            "symlinks were not followed (their .vpcf files are not compiled): {}",
            skipped_links.join(", ")
        )));
    }
    if let Some(note) = game_note {
        notes.push(json!(note));
    }
    notes
}

fn add_notes(json: &mut Value, notes: Vec<Value>) {
    if notes.is_empty() {
        return;
    }
    match json.get_mut("notes").and_then(Value::as_array_mut) {
        Some(existing) => existing.extend(notes),
        None => json["notes"] = Value::Array(notes),
    }
}

/// Every `.vpcf` directly in `dir` (and below it with `recursive`). Symlinks are skipped, so the
/// walk cannot leave the addon.
fn collect_vpcf(
    dir: &Path,
    recursive: bool,
    out: &mut Vec<PathBuf>,
    skipped_links: &mut Vec<String>,
) -> Result<(), ToolError> {
    let entries = io_fail(&format!("cannot list {}", dir.display()), std::fs::read_dir(dir))?;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_symlink() {
            skipped_links.push(path.display().to_string());
        } else if kind.is_dir() && recursive {
            collect_vpcf(&path, recursive, out, skipped_links)?;
        } else if kind.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("vpcf"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The compile set being built: files in compile order (children first), the referenced files
/// that cannot be found anywhere (`missing`: failures) and the ones that exist outside the
/// addon's sources (`external`: verdicts that never count against `ok`).
#[derive(Default)]
struct Plan {
    order: Vec<compile::CompileFile>,
    missing: Vec<Value>,
    external: Vec<Value>,
    visited: std::collections::HashSet<PathBuf>,
    seen_refs: std::collections::HashSet<String>,
    /// `None` until the first reference outside the addon needs the game VPKs; then the shared
    /// catalog's trees (`Some(None)` when they cannot be read: noted once).
    game: Option<Option<std::sync::Arc<super::model_catalog::ModelCatalog>>>,
    game_note: Option<String>,
}

type VisitFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + 'a>>;

/// `rel` (forward slashes) under `base`, matching each component case-insensitively (Wine's
/// file system is case-insensitive, this host's is not).
fn find_ci(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut current = base.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let exact = current.join(part);
        if exact.symlink_metadata().is_ok() {
            current = exact;
            continue;
        }
        let wanted = part.to_ascii_lowercase();
        let found = std::fs::read_dir(&current)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().to_ascii_lowercase() == wanted)?;
        current = found.path();
    }
    Some(current)
}

impl Plan {
    /// Post-order walk: a file's children are placed before it; every file once (the visited set
    /// is marked before descending, which is what ends a reference cycle).
    fn visit<'a>(
        &'a mut self,
        state: &'a HammerState,
        host: &'a Path,
        referenced_by: Option<&'a str>,
        children: bool,
    ) -> VisitFuture<'a> {
        Box::pin(async move {
            let key = std::fs::canonicalize(host).unwrap_or_else(|_| host.to_path_buf());
            if !self.visited.insert(key) {
                return Ok(());
            }
            let rel = state
                .content()
                .addon_rel(host)
                .unwrap_or_else(|| host.display().to_string());
            if children {
                // an unreadable or unparsable file still goes to the compiler, which reports it
                if let Some(refs) = std::fs::read_to_string(host)
                    .ok()
                    .and_then(|text| Particle::parse(&text).ok())
                    .map(|particle| child_refs(&particle))
                {
                    for child in refs {
                        self.resolve_child(state, &child, &rel, children).await?;
                    }
                }
            }
            self.order.push(compile::CompileFile {
                rel,
                host: host.to_path_buf(),
                referenced_by: referenced_by.map(str::to_string),
            });
            Ok(())
        })
    }

    /// Resolve one `m_ChildRef` in order, stopping at the first hit: addon source (compiled with
    /// the set), addon compiled output without a source, `content/csgo|core` source, a game VPK
    /// entry; nowhere is a failure naming the parent. Everything but the first is external: not
    /// compiled, never a failure.
    async fn resolve_child(
        &mut self,
        state: &HammerState,
        child: &str,
        parent: &str,
        children: bool,
    ) -> Result<(), ToolError> {
        let fail = |this: &mut Plan, why: String| {
            this.missing.push(json!({
                "path": child, "ok": false, "missing": true, "referencedBy": parent,
                "failLines": [why],
            }));
        };
        let norm = child.replace('\\', "/");
        let norm = norm.trim_start_matches('/');
        let usable = !norm.is_empty()
            && !norm.split('/').any(|p| p.is_empty() || p == "." || p == "..")
            && norm.to_ascii_lowercase().ends_with(".vpcf");
        if !usable {
            fail(
                self,
                format!("child reference {child:?} in {parent} is not a usable relative .vpcf path"),
            );
            return Ok(());
        }
        let norm = norm.to_string();
        // (a) the addon's source, case-insensitively
        let addon_root = state.content().addon_root();
        if let Some(found) = find_ci(&addon_root, &norm).filter(|p| p.is_file()) {
            let checked = state.content().resolve_write_ext(
                &found.display().to_string(),
                &state.path_map,
                "vpcf",
            );
            return match checked {
                Ok(host) => self.visit(state, &host, Some(parent), children).await,
                Err(error) => {
                    fail(self, format!("child {child:?} of {parent}: {}", path_error(error).message));
                    Ok(())
                }
            };
        }
        if !self.seen_refs.insert(norm.to_ascii_lowercase()) {
            return Ok(());
        }
        let external = |this: &mut Plan, source: &str| {
            this.external.push(json!({
                "path": norm, "ok": true, "external": true, "compiled": false,
                "source": source, "referencedBy": parent,
            }));
        };
        let app = &state.content().app_root;
        // (b) compiled in the addon, no source
        if find_ci(&app.join("game/csgo_addons").join(state.addon()), &format!("{norm}_c"))
            .is_some_and(|p| p.is_file())
        {
            self.external.push(json!({
                "path": norm, "ok": true, "compiled": false, "source": "addon-compiled",
                "referencedBy": parent,
            }));
            return Ok(());
        }
        // (c) a source in the stock content trees
        for tree in ["csgo", "core"] {
            if find_ci(&app.join("content").join(tree), &norm).is_some_and(|p| p.is_file()) {
                external(self, &format!("content/{tree}"));
                return Ok(());
            }
        }
        // (d) a compiled entry in the game VPKs (the catalog's cached trees)
        if self.game.is_none() {
            self.game = Some(match super::model_catalog::catalog_for(state).await {
                Ok(catalog) => Some(catalog),
                Err(error) => {
                    self.game_note = Some(format!(
                        "the game VPKs could not be read ({}): children not found in the addon or content \
                         are reported as external, source \"unknown\", verified:false",
                        error.message
                    ));
                    None
                }
            });
        }
        let hit = self
            .game
            .as_ref()
            .and_then(Option::as_ref)
            .and_then(|catalog| catalog.game_has(&format!("{norm}_c")));
        if let Some(pak) = hit {
            external(self, &format!("vpk:{pak}"));
            return Ok(());
        }
        // the lookup itself failed (no readable VPKs): that is not the file failing, so the
        // child is passed on unverified, with the note as the warning
        if matches!(self.game, Some(None)) {
            self.external.push(json!({
                "path": norm, "ok": true, "external": true, "compiled": false,
                "source": "unknown", "verified": false, "referencedBy": parent,
            }));
            return Ok(());
        }
        // (e) nowhere
        fail(
            self,
            format!("{norm} does not exist anywhere (addon, content, game VPKs); referenced by {parent}"),
        );
        Ok(())
    }
}

/// The `m_Children[].m_ChildRef` paths of a particle (a `resource:"..."` flagged string or a
/// plain one), in file order.
fn child_refs(particle: &Particle) -> Vec<String> {
    let Some(items) = particle.doc.root.get("m_Children").and_then(cs2_kv3::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(cs2_kv3::Value::as_object)
        .filter_map(|child| child.get("m_ChildRef"))
        .filter_map(|value| match value {
            cs2_kv3::Value::Flagged { value, .. } => value.as_str(),
            other => other.as_str(),
        })
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect()
}

// -------------------------------------------------------------------------------------------
// Registration
// -------------------------------------------------------------------------------------------

/// Register the six file-route particle tools (WP4 half of `PET_TOOL_ORDER`; the live half is
/// `particle_live.rs`, the combined order `particles.rs`).
pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "particle_read",
            "Read a .vpcf from disk: the kv3 header, root non-array fields, per-array entry counts; function:\"m_Operators/2\" returns that entry's fields. Path: host, Z:\\app\\..., or csgo_addons/<addon>/particles/x.vpcf.",
            ReadOnly,
            &[],
        ),
        particle_read,
    );
    r.register(
        ToolSpec::new(
            "particle_list",
            "Flat list of a .vpcf's function entries from disk (array, type, index, class, fieldKeys), filterable by array and class; counts gives entries per array.",
            ReadOnly,
            &[],
        ),
        particle_list,
    );
    r.register(
        ToolSpec::new(
            "particle_validate",
            "Validate a .vpcf on disk against the particle schema snapshot (structure, classes, fields); returns ok, errors, warnings. No usable snapshot is SchemaUnavailable, never ok:true.",
            ReadOnly,
            &[],
        ),
        particle_validate,
    );
    r.register(
        ToolSpec::new(
            "particle_edit",
            "Edit a .vpcf in the active addon: ops add_function/remove_function/move_function/set_field/remove_field/set_root_field/remove_root_field/add_child/remove_child/add_variable/remove_variable, all-or-nothing, schema-checked (skipSchema skips only that), backup + atomic write. Does not touch the editor: particle_reload afterwards.",
            Writer,
            &[],
        ),
        particle_edit,
    );
    r.register(
        ToolSpec::new(
            "particle_create",
            "Create a new .vpcf in the active addon from the minimal template (maxParticles, renderer). Refuses an existing file unless overwrite.",
            Writer,
            &[],
        ),
        particle_create,
    );
    r.register(
        ToolSpec::new(
            "particle_compile",
            "Compile a .vpcf (or a folder; recursive) to .vpcf_c with resourcecompiler. children (default true) also compiles m_ChildRef files, children first; stock/external children are reported, not compiled. ok only if every file is.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "a .vpcf or folder of the active addon: addon-relative `particles/x.vpcf`"},
                "recursive": {"type": "boolean", "description": "folder: descend into sub-folders"},
                "children": {"type": "boolean", "description": "also compile m_ChildRef children (default true)"},
                "force": {"type": "boolean", "description": "recompile even when the compiler thinks the output is current"},
                "timeoutMs": {"type": "integer", "minimum": 1, "description": "whole-run deadline in ms (default 120000, max 600000)"}
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        particle_compile,
    );
}

/// The order [`register`] installs, for the merge-time `PET_TOOL_ORDER` check.
pub const PARTICLE_FILE_TOOL_ORDER: [&str; 6] = [
    "particle_read",
    "particle_list",
    "particle_validate",
    "particle_edit",
    "particle_create",
    "particle_compile",
];
