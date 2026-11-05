//! `edit_soundevents` + `compile_soundevents` (WP-S2): edit the addon's `.vsndevts` text without
//! destroying the user's comments ([`crate::soundevents::text`]), then compile it with the shared
//! `resourcecompiler` route.
//!
//! * `edit_soundevents` edits `content/csgo_addons/<addon>/soundevents/*.vsndevts` with
//!   `set`/`remove`/`rename` ops. Validation runs entirely before any write (one failure refuses
//!   the whole call): names are `[A-Za-z0-9_.]{1..127}`, `properties` must convert to KV3, every
//!   edited event needs `type` or a resolvable `base` (in the edited file or the index),
//!   `position` must be 3 finite numbers, a `remove` of a referenced event is refused unless
//!   `force`. The splice ([`crate::soundevents::text::splice`]) replaces only the touched event
//!   spans; the result is re-parsed and must equal the intended root before
//!   [`write_atomic_with_backup`](crate::vmdl::fsutil::write_atomic_with_backup) runs — a splice
//!   that cannot be proven writes nothing. A comment inside a replaced event is reported in
//!   `commentsDropped` (every comment outside the touched spans survives byte-for-byte). The
//!   whole read-modify-write runs under [`HammerState::soundevents_gate`] (review L6: two
//!   concurrent edits of one file must not race; review N2: a dedicated gate, not the server
//!   -wide write gate, so a container exec of `compile_soundevents` never blocks a bridge
//!   mutator), and a rename whose splice cannot round-trip is refused an `InternalError` (never
//!   a write).
//! * `compile_soundevents` reuses [`super::compile::compile_resourcecompiler`] unchanged (the
//!   particle route's exact command; `ok:false` is a result) and adds what R1 measured for
//!   `.vsndevts`: every `RESOURCE COMPILE ERROR` line becomes
//!   `errors:[{line, col, message}]` (the Windows path stripped), and `events` counts the events
//!   of the fresh `_c`. The compiler checks nothing but syntax (RE R1: unknown bases, children,
//!   vsnds and property keys all compile), so the edit tool's validation is the real check.
//!
//! **Seams (FIX-S2 wiring):** `base` resolution and the warning lookups go through
//! [`BaseResolver`] — a closure set the tests inject verbatim ([`ResolverSource::Fixed`]). The
//! production path ([`ResolverSource::FromIndex`]) builds the **real** soundevent index
//! (`soundevents::index_for`, WP-S1) lazily, at the first base/children/keys check — the ORDER
//! defect FIX-S2 fixes: argument validation, path resolution, the file existence/`createFile`
//! check and the KV3 parse all run BEFORE the index is built, so a local refusal
//! (`WriteOutsideAddon`, `SoundEventsFileNotFound`, `SoundEventsParseError`, …) is never masked
//! by a `VpkMissing`, and a call refused before those checks never builds the index at all.
//! When the index cannot be built: `validate:true` (the default) refuses the edit with that
//! error — nothing is written; `validate:false` proceeds with a warning that no reference check
//! ran. `EditSeams.splicer` exists so a test can inject a buggy splice and pin the
//! `InternalError`-refuses behaviour.
use super::compile::{self, CompileTarget};
use crate::soundevents::text::{self, SpliceOp, Spliced, SpliceError};
use crate::soundevents::SoundEventIndex;
use crate::state::HammerState;
use crate::vmdl;
use cs2_kv3::Object;
use cs2_mcp_common::tools::{
    CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// `ops` bound per call (the WP's 1..=64).
pub const MAX_OPS: usize = 64;

/// The template's exact header line (copied verbatim from the addon template file,
/// `fixtures/soundevents/soundevents_addon.vsndevts`); `createFile` writes it.
pub const TEMPLATE_HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

/// The addon file created when `path` is omitted.
pub fn default_path(addon: &str) -> String {
    format!("csgo_addons/{addon}/soundevents/soundevents_addon.vsndevts")
}

// -------------------------------------------------------------------------------------------
// Seams (merge-time wiring: see the module doc)
// -------------------------------------------------------------------------------------------

/// The type of [`EditSeams::splicer`]: the whole splice step, so a test can inject a broken one.
pub type Splicer = fn(&str, &[SpliceOp]) -> Result<Spliced, SpliceError>;

/// The type of [`BaseResolver::base`]: resolve one `base` name — `Ok(())` = known,
/// `Err(suggestions)` = unknown.
pub type BaseLookup = Arc<dyn Fn(&str) -> Result<(), Vec<String>> + Send + Sync>;
/// The type of [`BaseResolver::known_event`] / `known_keys`: `None` = no index, the check is
/// skipped.
pub type KnownLookup<T> = Arc<dyn Fn() -> Option<T> + Send + Sync>;
/// The type of [`BaseResolver::known_event`]'s closure argument form (`pub` for the tests).
pub type KnownEvent = Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;

/// The lookups `edit_soundevents` needs from the soundevent index. A fixed resolver is what the
/// tests inject; the production one is built from the cached index
/// ([`BaseResolver::from_index`]).
#[derive(Clone)]
pub struct BaseResolver {
    /// Resolve one `base` name: `Ok(())` = known, `Err(suggestions)` = unknown.
    pub base: BaseLookup,
    /// Is this event name known anywhere? `None` = no index, the `unknownChildren` warning is
    /// skipped.
    pub known_event: KnownEvent,
    /// The property keys the corpus has ever seen; `None` = no index, the `unknownKeys` warning
    /// is skipped (`SoundEventIndex::known_property_keys()`, WP-S1).
    pub known_keys: KnownLookup<BTreeSet<String>>,
}

impl Default for BaseResolver {
    fn default() -> Self {
        BaseResolver {
            base: Arc::new(|_| Err(Vec::new())),
            known_event: Arc::new(|_| None),
            known_keys: Arc::new(|| None),
        }
    }
}

impl BaseResolver {
    /// The resolver over a built index (`index_for`'s cached answer): `get`/`suggestions` for
    /// `base`, `known_event` for the child warning and `known_property_keys()` for `known_keys`.
    pub(crate) fn from_index(index: Arc<SoundEventIndex>) -> BaseResolver {
        let for_event = Arc::clone(&index);
        let for_keys = Arc::clone(&index);
        BaseResolver {
            base: Arc::new(move |name| match index.get(name) {
                Some(_) => Ok(()),
                None => Err(index.suggestions(name, 5)),
            }),
            known_event: Arc::new(move |name| Some(for_event.get(name).is_some())),
            // an empty key set means the index found no events at all: skip the check rather
            // than warn on every key
            known_keys: Arc::new(move || {
                let keys = for_keys.known_property_keys();
                (!keys.is_empty()).then(|| keys.clone())
            }),
        }
    }
}

/// Where [`EditSeams`]' resolver comes from. `Fixed` is the tests' injected closure set (the
/// index is never built); `FromIndex` is the production path — the cached soundevent index,
/// built lazily at the first base/children/keys check (see the module doc's ORDER note).
#[derive(Clone)]
pub enum ResolverSource {
    /// Use this resolver as-is; no index is ever built (the tests' seams).
    Fixed(BaseResolver),
    /// Build (or reuse the cached) soundevent index on first use.
    FromIndex,
}

/// Everything the edit core needs that a test may want to replace.
pub struct EditSeams {
    pub resolver: ResolverSource,
    pub splicer: Splicer,
}

impl Default for EditSeams {
    fn default() -> Self {
        EditSeams { resolver: ResolverSource::Fixed(BaseResolver::default()), splicer: text::splice }
    }
}

/// The edit core's view of [`ResolverSource`]: the resolver, resolved at the first check that
/// needs it — never before (FIX-S2's ordering: argument validation, path resolution, the file
/// check and the parse all run first, and a call refused before them never builds the index).
struct LazyResolver {
    fixed: Option<BaseResolver>,
    built: Option<BaseResolver>,
}

impl LazyResolver {
    fn new(source: &ResolverSource) -> LazyResolver {
        match source {
            ResolverSource::Fixed(resolver) => {
                LazyResolver { fixed: Some(resolver.clone()), built: None }
            }
            ResolverSource::FromIndex => LazyResolver { fixed: None, built: None },
        }
    }

    /// The resolver, building the index on the first call. An unbuildable index refuses when
    /// `validate` is true (nothing has been written at this point — the write is the last step)
    /// and, with `validate:false`, downgrades to the no-knowledge resolver plus a warning that
    /// no reference check ran.
    async fn get(
        &mut self,
        state: &HammerState,
        validate: bool,
        warnings: &mut Vec<Value>,
    ) -> Result<&BaseResolver, ToolError> {
        if let Some(resolver) = self.fixed.as_ref() {
            return Ok(resolver);
        }
        if self.built.is_none() {
            let resolver = match crate::soundevents::index_for(state).await {
                Ok(index) => BaseResolver::from_index(index),
                Err(error) => {
                    if validate {
                        return Err(error);
                    }
                    warnings.push(json!({
                        "type": "indexUnavailable",
                        "message": format!(
                            "the soundevent index could not be built, so no reference check ran: {}",
                            error.message
                        ),
                    }));
                    BaseResolver::default()
                }
            };
            self.built = Some(resolver);
        }
        Ok(self.built.as_ref().expect("the resolver was just built"))
    }
}

// -------------------------------------------------------------------------------------------
// Shared helpers
// -------------------------------------------------------------------------------------------

/// [`crate::vmdl::VmdlError`] from `resolve_write_ext` -> [`ToolError`], the particle route's
/// wording mapping plus the `..` case (which this tool reports as `InvalidArguments`).
fn path_error(error: crate::vmdl::VmdlError) -> ToolError {
    use crate::vmdl::VmdlError;
    match error {
        VmdlError::Path(msg) if msg.contains("path contains `..`") => {
            ToolError::invalid_arguments(msg)
        }
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
                ToolError::invalid_arguments(msg)
            }
        }
        VmdlError::Io(error) => {
            ToolError::new("IoError", "IoError", error.to_string()).retry_safe(false)
        }
        other => ToolError::internal(format!("unexpected content-path error: {other}")),
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

/// A `path?` string argument: the default addon file when absent.
fn path_arg(state: &HammerState, args: &Value) -> Result<String, ToolError> {
    match args.get("path") {
        None | Some(Value::Null) => Ok(default_path(&state.addon())),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(other) => Err(invalid(format!("path must be a string, got {other}"))),
    }
}

/// A boolean flag with a default (a non-boolean value is an argument error).
fn flag(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(invalid(format!("{name} must be a boolean, got {other}"))),
    }
}

/// A soundevent name argument: `[A-Za-z0-9_.]{1..=127}` (the engine hashes the exact text, so
/// anything else — a space, a `/`, 128 characters — is refused before anything is looked up).
fn event_name(value: Option<&Value>, field: &str) -> Result<String, ToolError> {
    let name = value
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid(format!("{field} must be a non-empty string, got {}", value.map(|v| v.to_string()).unwrap_or_else(|| "nothing".into()))))?;
    if !valid_event_name(name) {
        return Err(invalid(format!(
            "{field} {name:?} is not a soundevent name (expected 1..=127 characters of [A-Za-z0-9_.])"
        )));
    }
    Ok(name.to_string())
}

fn valid_event_name(name: &str) -> bool {
    (1..=127).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
}

/// The `SoundEventsFileNotFound` refusal (`createFile:true` is the way out).
fn file_not_found(path: &str, host: &Path) -> ToolError {
    ToolError::new(
        "SoundEventsFileNotFound",
        "SoundEventsFileNotFound",
        format!(
            "{} does not exist (pass createFile:true to create it with the addon template's header)",
            host.display()
        ),
    )
    .with_data(json!({"path": path, "hint": "createFile:true"}))
    .retry_safe(true)
}

fn parse_error(path: &str, host: &Path, error: cs2_kv3::ParseError) -> ToolError {
    ToolError::new(
        "SoundEventsParseError",
        "SoundEventsParseError",
        format!("{} cannot be parsed: {error}", host.display()),
    )
    .with_data(json!({"path": path, "line": error.line, "col": error.col}))
    .retry_safe(true)
}

fn sound_event_exists(name: &str, host: &Path) -> ToolError {
    ToolError::new(
        "SoundEventExists",
        "SoundEventExists",
        format!(
            "the soundevent {name:?} already exists in {}; pass merge:true to merge into it or overwrite:true to replace it",
            host.display()
        ),
    )
    .with_data(json!({"name": name, "file": host.display().to_string()}))
    .retry_safe(true)
}

fn sound_event_invalid(name: &str, problems: Vec<String>) -> ToolError {
    let first = problems.first().cloned().unwrap_or_default();
    ToolError::new(
        "SoundEventInvalid",
        "SoundEventInvalid",
        format!("the soundevent {name:?} is invalid: {first}"),
    )
    .with_data(json!({"name": name, "problems": problems}))
    .retry_safe(true)
}

fn sound_event_base_not_found(name: &str, base: &str, suggestions: Vec<String>) -> ToolError {
    let with_list = if suggestions.is_empty() {
        String::new()
    } else {
        format!("; closest known events: {}", suggestions.join(", "))
    };
    ToolError::new(
        "SoundEventBaseNotFound",
        "SoundEventBaseNotFound",
        format!(
            "the soundevent {name:?} has base {base:?}, which is neither in the edited file nor in the soundevent index{with_list}"
        ),
    )
    .with_data(json!({"name": name, "base": base}))
    .did_you_mean(suggestions)
    .retry_safe(true)
}

fn sound_event_not_found(name: &str, file: &str, suggestions: Vec<String>) -> ToolError {
    ToolError::new(
        "SoundEventNotFound",
        "SoundEventNotFound",
        format!("no soundevent named {name:?} in {file}"),
    )
    .with_data(json!({
        "name": name,
        "sourcesChecked": [file],
    }))
    .did_you_mean(suggestions)
    .retry_safe(true)
}

/// Near-miss suggestions from one file's own event names (case-insensitive exact first, then
/// shared-prefix length, like the model catalog's ranking).
fn suggest_from<'a>(names: impl Iterator<Item = &'a str>, name: &str, limit: usize) -> Vec<String> {
    let lower = name.to_lowercase();
    let mut scored: Vec<(usize, String)> = names
        .filter(|candidate| *candidate != name)
        .map(|candidate| {
            let candidate_lower = candidate.to_lowercase();
            let exact = usize::from(candidate_lower == lower);
            let prefix = candidate_lower
                .as_bytes()
                .iter()
                .zip(lower.as_bytes())
                .take_while(|(a, b)| a == b)
                .count();
            (exact * 10_000 + prefix, candidate.to_string())
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, name)| name).collect()
}

/// Does `value` (one event's object) reference `name` via `base` or `soundevent_01`?
fn references_event(value: &cs2_kv3::Value, name: &str) -> bool {
    let Some(object) = value.as_object() else { return false };
    if object.get("base").and_then(cs2_kv3::Value::as_str) == Some(name) {
        return true;
    }
    match object.get("soundevent_01") {
        Some(cs2_kv3::Value::String(text)) => text == name,
        Some(cs2_kv3::Value::Array(items)) => {
            items.iter().any(|item| item.as_str() == Some(name))
        }
        _ => false,
    }
}

/// Rewrite `from` -> `to` in one event's `base` / `soundevent_01` (the rename's document-level
/// half). Returns how many references moved.
fn rewrite_references(value: &mut cs2_kv3::Value, from: &str, to: &str) -> usize {
    let mut count = 0;
    let Some(object) = value.as_object_mut() else { return 0 };
    if let Some(base) = object.get_mut("base") {
        if base.as_str() == Some(from) {
            *base = cs2_kv3::Value::from(to);
            count += 1;
        }
    }
    if let Some(child) = object.get_mut("soundevent_01") {
        match child {
            cs2_kv3::Value::String(text) if text == from => {
                *child = cs2_kv3::Value::from(to);
                count += 1;
            }
            cs2_kv3::Value::Array(items) => {
                for item in items.iter_mut() {
                    if item.as_str() == Some(from) {
                        *item = cs2_kv3::Value::from(to);
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// A KV3 number as f64 (`cs2_kv3::Value` has no `as_f64`; the number keeps its lexeme).
fn kv_f64(value: &cs2_kv3::Value) -> Option<f64> {
    match value {
        cs2_kv3::Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

/// JSON `properties` -> an ordered KV3 object; a value `from_json` cannot convert is an
/// `InvalidArguments` naming the property key.
fn properties_object(properties: &Map<String, Value>, where_: &str) -> Result<Object, ToolError> {
    let mut object = Object::new();
    for (key, value) in properties {
        let converted = cs2_kv3::from_json(value)
            .map_err(|error| invalid(format!("{where_}.{key}: {error}")))?;
        object.insert(key.clone(), converted);
    }
    Ok(object)
}

// -------------------------------------------------------------------------------------------
// edit_soundevents
// -------------------------------------------------------------------------------------------

/// Register the two tools (the order [`register`] installs is pinned by the tests).
pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "edit_soundevents",
            "Edit a .vsndevts (default: the addon's soundevents_addon.vsndevts) keeping comments. Args: path?, ops[] of {op:\"set\",name,properties,merge?} | {op:\"remove\",name} | {op:\"rename\",name,to}, dryRun?, createFile?, overwrite? (set replaces an existing event), force? (remove a referenced event), validate?=true. Checked before writing. Compile with compile_soundevents.",
            Writer,
            &[],
        ),
        |state, ctx, args| async move {
            // The production seams: the real splice and the cached soundevent index — built
            // LAZILY inside the edit core, at the first base/children/keys check (never before
            // the argument, path, file and parse checks; FIX-S2's ordering defect).
            let seams = EditSeams { resolver: ResolverSource::FromIndex, splicer: text::splice };
            edit_soundevents_with(&seams, state, ctx, args).await
        },
    );
    r.register(
        ToolSpec::new(
            "compile_soundevents",
            "Compile a .vsndevts to <file>_c (args: path?, timeoutMs?). Needs a running tools container. ok:false is a result with `errors`. Loaded at the next map load (run_map).",
            Writer,
            &[],
        ),
        compile_soundevents,
    );
}

/// The registered order (the WP's two tools, edit first).
pub const SOUND_EVENTS_EDIT_TOOL_ORDER: [&str; 2] = ["edit_soundevents", "compile_soundevents"];

/// The edit core, with the seams injected ([`EditSeams`]). The registered tool calls this with
/// the production [`ResolverSource::FromIndex`] resolver; the tests call it with their own
/// fixed resolver/splicer.
pub async fn edit_soundevents_with(
    seams: &EditSeams,
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    // The soundevents gate (review N2), not the server-wide write gate: the edit is files-only
    // (a read-modify-write of the addon text plus the index invalidation), so it must not block a
    // concurrent bridge mutator — but two edits, and an edit against a compile, of the same file
    // must not race, so a second edit still waits here, re-reads the first's written bytes and
    // lands on top of them (the L6 test's lost-update invariant).
    let _gate = state.soundevents_gate().await;
    super::live_common::reject_unknown_keys(
        &args,
        &["path", "ops", "createFile", "overwrite", "validate", "dryRun", "force"],
    )?;
    let dry_run = flag(&args, "dryRun", false)?;
    let create_file = flag(&args, "createFile", false)?;
    let overwrite = flag(&args, "overwrite", false)?;
    let validate = flag(&args, "validate", true)?;
    let force = flag(&args, "force", false)?;
    let path = path_arg(&state, &args)?;

    let ops_value = args
        .get("ops")
        .filter(|value| !value.is_null())
        .ok_or_else(|| invalid("ops (array) is required"))?;
    let ops_list = ops_value
        .as_array()
        .ok_or_else(|| invalid(format!("ops must be an array, got {ops_value}")))?;
    if ops_list.is_empty() || ops_list.len() > MAX_OPS {
        return Err(invalid(format!(
            "ops must hold 1..{MAX_OPS} entries, got {}",
            ops_list.len()
        )));
    }

    let host = state
        .content()
        .resolve_write_ext(&path, &state.path_map, "vsndevts")
        .map_err(path_error)?;
    let default_host = state.content().addon_root().join("soundevents/soundevents_addon.vsndevts");

    // The file (or the createFile stub), parsed.
    let (original, mut root, created) = match std::fs::read_to_string(&host) {
        Ok(text) => {
            let document = cs2_kv3::parse(&text).map_err(|error| parse_error(&path, &host, error))?;
            (text, document.root, false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create_file {
                return Err(file_not_found(&path, &host));
            }
            let document = cs2_kv3::parse(&format!("{TEMPLATE_HEADER}\r\n{{}}"))
                .expect("the createFile stub parses");
            (format!("{TEMPLATE_HEADER}\r\n{{}}"), document.root, true)
        }
        Err(error) => {
            return Err(ToolError::new(
                "IoError",
                "IoError",
                format!("cannot read {}: {error}", host.display()),
            )
            .retry_safe(true))
        }
    };
    let file_display = host.display().to_string();

    // Apply the ops to the document (the intended root) and collect the splice ops.
    let mut splice_ops: Vec<SpliceOp> = Vec::with_capacity(ops_list.len());
    let mut ops_result: Vec<Value> = Vec::with_capacity(ops_list.len());
    let mut touched: Vec<String> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();
    let mut unknown_keys: Vec<Value> = Vec::new();
    // Review N7: the (name, property keys) pairs the unknown-key warning needs, collected in
    // the loop and resolved once AFTER every op was arg-checked.
    let mut unknown_key_candidates: Vec<(String, Vec<String>)> = Vec::new();
    let mut references_updated = 0usize;
    // The index-backed lookups, resolved at the first check that needs them (never before the
    // argument, path, file and parse checks above).
    let mut resolver = LazyResolver::new(&seams.resolver);

    for (index, op) in ops_list.iter().enumerate() {
        let where_ = format!("ops[{index}]");
        let Some(op_object) = op.as_object() else {
            return Err(invalid(format!("{where_} must be an object, got {op}")));
        };
        let kind = op_object
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("{where_}.op must be \"set\", \"remove\" or \"rename\"")))?;
        match kind {
            "set" => {
                super::live_common::reject_unknown_keys(
                    op,
                    &["op", "name", "properties", "merge"],
                )
                .map_err(|error| invalid(format!("{where_}: {}", error.message)))?;
                let name = event_name(op_object.get("name"), &format!("{where_}.name"))?;
                let properties = op_object
                    .get("properties")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        invalid(format!(
                            "{where_}.properties must be an object of key/value pairs, got {}",
                            op_object.get("properties").map(|value| value.to_string()).unwrap_or_else(|| "nothing".into())
                        ))
                    })?;
                let merge = matches!(op_object.get("merge"), Some(Value::Bool(true)));
                let (new_object, status) = match root.get(&name) {
                    Some(cs2_kv3::Value::Object(existing)) if merge => {
                        let mut merged = existing.clone();
                        for (key, value) in properties.iter() {
                            let converted = cs2_kv3::from_json(value)
                                .map_err(|error| invalid(format!("{where_}.properties.{key}: {error}")))?;
                            merged.insert(key.clone(), converted);
                        }
                        (merged, "merged")
                    }
                    Some(cs2_kv3::Value::Object(_)) if overwrite => {
                        (properties_object(properties, &format!("{where_}.properties"))?, "replaced")
                    }
                    Some(cs2_kv3::Value::Object(_)) => return Err(sound_event_exists(&name, &host)),
                    Some(_) => {
                        return Err(sound_event_invalid(
                            &name,
                            vec!["the name exists in the file but is not an event object".into()],
                        ))
                    }
                    None => (
                        properties_object(properties, &format!("{where_}.properties"))?,
                        "created",
                    ),
                };
                root.insert(name.clone(), cs2_kv3::Value::Object(new_object.clone()));
                touched.push(name.clone());
                splice_ops.push(SpliceOp::Set { name: name.clone(), object: new_object });
                ops_result.push(json!({"op": "set", "name": name, "status": status}));
                // NEW (RE R1): a property key the corpus has never seen is a warning, not a
                // refusal (the compiler checks nothing; this is the only early signal). The
                // candidates are collected here and checked against the corpus ONCE after the
                // loop (review N7: the index build inside the loop used to let a `VpkMissing`
                // from op 0 mask a later op's `InvalidArguments`).
                unknown_key_candidates.push((name.clone(), properties.keys().cloned().collect()));
            }
            "remove" => {
                super::live_common::reject_unknown_keys(op, &["op", "name"])
                    .map_err(|error| invalid(format!("{where_}: {}", error.message)))?;
                let name = event_name(op_object.get("name"), &format!("{where_}.name"))?;
                if root.get(&name).is_none() {
                    return Err(sound_event_not_found(
                        &name,
                        &file_display,
                        suggest_from(root.keys(), &name, 5),
                    ));
                }
                let referrers: Vec<String> = root
                    .iter()
                    .filter(|(other, _)| *other != name.as_str())
                    .filter(|(_, value)| references_event(value, &name))
                    .map(|(other, _)| other.to_string())
                    .collect();
                if !referrers.is_empty() {
                    if !force {
                        return Err(sound_event_invalid(
                            &name,
                            vec![format!("referenced by {}", referrers.join(", "))],
                        ));
                    }
                    warnings.push(json!({
                        "type": "removedReferenced",
                        "name": name,
                        "referrers": referrers,
                        "message": format!("removed {name} although referenced by {}", referrers.join(", ")),
                    }));
                }
                root.remove(&name);
                splice_ops.push(SpliceOp::Remove { name: name.clone() });
                ops_result.push(json!({"op": "remove", "name": name, "status": "removed"}));
            }
            "rename" => {
                super::live_common::reject_unknown_keys(op, &["op", "name", "to"])
                    .map_err(|error| invalid(format!("{where_}: {}", error.message)))?;
                let from = event_name(op_object.get("name"), &format!("{where_}.name"))?;
                let to = event_name(op_object.get("to"), &format!("{where_}.to"))?;
                if from == to {
                    return Err(invalid(format!(
                        "{where_}: renaming {from:?} to itself changes nothing"
                    )));
                }
                if root.get(&from).is_none() {
                    return Err(sound_event_not_found(
                        &from,
                        &file_display,
                        suggest_from(root.keys(), &from, 5),
                    ));
                }
                if root.get(&to).is_some() {
                    return Err(sound_event_exists(&to, &host));
                }
                // Rename + reference rewrite in one ordered rebuild (the splice renames in
                // place; the root must end up in the same order).
                let mut rebuilt = Object::new();
                for (key, value) in root.iter() {
                    let mut value = value.clone();
                    references_updated += rewrite_references(&mut value, &from, &to);
                    rebuilt.insert(if key == from { to.clone() } else { key.to_string() }, value);
                }
                root = rebuilt;
                touched.push(to.clone());
                splice_ops.push(SpliceOp::Rename { from: from.clone(), to: to.clone() });
                ops_result.push(json!({"op": "rename", "name": from, "to": to, "status": "renamed"}));
            }
            other => return Err(invalid(format!(
                "{where_}.op {other:?} is not \"set\", \"remove\" or \"rename\""
            ))),
        }
    }

    // Validation (all before any write; one failure refuses the whole call).
    if validate {
        for name in &touched {
            let Some(cs2_kv3::Value::Object(object)) = root.get(name) else { continue };
            let has_type = matches!(object.get("type"), Some(cs2_kv3::Value::String(text)) if !text.is_empty());
            let base = object.get("base").and_then(cs2_kv3::Value::as_str).filter(|text| !text.is_empty());
            if !has_type && base.is_none() {
                return Err(sound_event_invalid(name, vec!["neither type nor base".into()]));
            }
            if let Some(base) = base {
                if root.get(base).is_none() {
                    let lookup =
                        Arc::clone(&resolver.get(&state, validate, &mut warnings).await?.base);
                    lookup(base).map_err(|suggestions| {
                        sound_event_base_not_found(name, base, suggestions)
                    })?;
                }
            }
            if let Some(position) = object.get("position") {
                let ok = matches!(position, cs2_kv3::Value::Array(items) if items.len() == 3
                    && items.iter().all(|item| kv_f64(item).map(|f| f.is_finite()).unwrap_or(false)));
                if !ok {
                    return Err(sound_event_invalid(
                        name,
                        vec!["position must be 3 finite numbers".into()],
                    ));
                }
            }
            // Unknown children are a warning (RE R1: the compiler accepts them, the runtime
            // fails silently — the only early signal is this check).
            if let Some(children) = object.get("soundevent_01") {
                let names: Vec<&str> = match children {
                    cs2_kv3::Value::String(text) if !text.is_empty() => vec![text.as_str()],
                    cs2_kv3::Value::Array(items) => {
                        items.iter().filter_map(cs2_kv3::Value::as_str).collect()
                    }
                    _ => Vec::new(),
                };
                for child in names {
                    let known_event = Arc::clone(
                        &resolver.get(&state, validate, &mut warnings).await?.known_event,
                    );
                    if known_event(child) == Some(false) {
                        warnings.push(json!({
                            "type": "unknownChild",
                            "name": name,
                            "child": child,
                            "message": format!("soundevent_01 names {child:?}, which is in no indexed source"),
                        }));
                    }
                }
            }
        }
    }

    // The unknown-key warnings, now that every op is arg-checked (review N7): the index build is
    // the LAST thing that can refuse on an unbuildable corpus, so a later op's InvalidArguments
    // always wins over a first op's unknown-key lookup. `validate:false` downgrades the build
    // error to the `indexUnavailable` warning inside `LazyResolver::get`.
    if !unknown_key_candidates.is_empty() {
        let known_keys = Arc::clone(
            &resolver.get(&state, validate, &mut warnings).await?.known_keys,
        );
        if let Some(known) = known_keys() {
            for (name, keys) in &unknown_key_candidates {
                for key in keys {
                    if !known.contains(key) {
                        unknown_keys.push(json!({"name": name, "key": key}));
                    }
                }
            }
        }
    }

    // Splice + round-trip check: the written bytes must re-parse to exactly the intended root.
    // Since FIX-T the splice rewrote only the key and the `base`/`soundevent_01` reference tokens
    // — the exact set the document side moves — so a property value that happens to hold the old
    // name no longer breaks the round trip (the old M2 misattribution, review N5: such a rename
    // simply succeeds). What is left is a genuine scanner/parse failure, and that is
    // `InternalError`, never a write.
    let splice_failure = |message: String| -> ToolError { ToolError::internal(message) };
    let Spliced { text: spliced_text, comments_dropped } = (seams.splicer)(&original, &splice_ops)
        .map_err(|error| {
            splice_failure(format!("the soundevent splice failed; nothing was written: {error}"))
        })?;
    let reparsed = cs2_kv3::parse(&spliced_text)
        .map_err(|error| {
            splice_failure(format!(
                "the spliced file no longer parses; nothing was written: {error}"
            ))
        })?;
    if !text::roots_equal(&reparsed.root, &root) {
        return Err(splice_failure(
            "the spliced file does not match the intended edit; nothing was written".to_string(),
        ));
    }

    let (written, backup) = if dry_run {
        (false, None)
    } else {
        if created {
            // `write_atomic_with_backup` requires the parent directory to exist.
            std::fs::create_dir_all(host.parent().unwrap_or_else(|| Path::new("/"))).map_err(|error| {
                ToolError::new(
                    "IoError",
                    "IoError",
                    format!("cannot create {}: {error}", host.parent().unwrap_or_else(|| Path::new("/")).display()),
                )
                .retry_safe(true)
            })?;
        }
        let backup = vmdl::fsutil::write_atomic_with_backup(&host, &spliced_text).map_err(|error| {
            ToolError::new("IoError", "IoError", format!("cannot write {}: {error}", host.display()))
                .retry_safe(true)
        })?;
        (true, backup)
    };
    if written {
        state.soundevent_index_invalidate();
    }

    if !unknown_keys.is_empty() {
        warnings.push(json!({
            "type": "unknownKeys",
            "entries": unknown_keys,
            "message": "property keys the soundevent corpus has never seen (the compiler accepts anything)",
        }));
    }

    let mut notes = Vec::new();
    if host != default_host {
        notes.push(
            "only soundevents_addon.vsndevts compiles into the addon (the template's own rule); \
             other .vsndevts files under the addon are not loaded by the game"
                .to_string(),
        );
    }
    notes.push(format!(
        "compile_soundevents to produce the compiled {}; run_map loads it at the next map load",
        state
            .content()
            .compiled_output(&host)
            .map(|output| output.display().to_string())
            .unwrap_or_else(|| format!("{file_display}_c"))
    ));
    if created {
        notes.push("the file was created with the addon template's kv3 header".to_string());
    }

    Ok(json!({
        "path": path,
        "addon": state.addon(),
        "addonSource": state.addon_selection().1.as_str(),
        "hostPath": file_display,
        "created": created,
        "written": written,
        "dryRun": dry_run,
        "backup": backup
            .map(|path| json!(path.display().to_string()))
            .unwrap_or(Value::Null),
        "ops": ops_result,
        "events": root.len(),
        "commentsDropped": comments_dropped
            .into_iter()
            .map(|(name, lines)| json!({"name": name, "lines": lines}))
            .collect::<Vec<_>>(),
        "referencesUpdated": references_updated,
        "warnings": warnings,
        "notes": notes,
    })
    .into())
}

// -------------------------------------------------------------------------------------------
// compile_soundevents
// -------------------------------------------------------------------------------------------

/// `RESOURCE COMPILE ERROR` lines (RE R1's failure shape) as `errors:[{line, col, message}]`,
/// the `Z:\app\...` path stripped.
fn parse_compile_errors(text: &str) -> Vec<Value> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let regex = RE.get_or_init(|| {
        regex::Regex::new(r"\((\d+),\s*(\d+)\):\s*RESOURCE COMPILE ERROR:\s*(.+?)\s*$")
            .expect("compile-error regex")
    });
    text.lines()
        .filter_map(|line| {
            let capture = regex.captures(line)?;
            Some(json!({
                "line": capture[1].parse::<u64>().unwrap_or(0),
                "col": capture[2].parse::<u64>().unwrap_or(0),
                "message": capture[3].to_string(),
            }))
        })
        .collect()
}

/// The event count of the fresh compiled file (the container's `DATA` KV3 root object) —
/// only members whose value is an object (review M4: the `_auto_calc_vnsd_duration_` scalar
/// sits in almost every block and is not an event; the index's own rule is the same). `None`
/// when anything on the way fails — the result then carries `events: null` and a note, never a
/// made-up count. (At merge this moves behind the soundevent index; WP-S2 reads the file the
/// compiler just wrote directly, which is the same fact without the S1 dependency.)
fn compiled_event_count(state: &HammerState, host: &Path) -> Option<usize> {
    let output: PathBuf = state.content().compiled_output(host)?;
    let bytes = std::fs::read(output).ok()?;
    let resource = crate::resource::Resource::parse(&bytes).ok()?;
    let data = resource.blocks.iter().find(|block| block.type_name() == "DATA")?;
    let document = crate::resource::decode_kv3(resource.block_bytes(data)).ok()?;
    document.root.as_object().map(|members| {
        members
            .iter()
            .filter(|(_, value)| matches!(value, crate::resource::Kv3Value::Object(_)))
            .count()
    })
}

/// Compile a `.vsndevts` with the shared resourcecompiler route (the particle file route's exact
/// command), plus R1's `.vsndevts` facts: the `RESOURCE COMPILE ERROR` lines as structured
/// `errors` and the fresh `_c`'s event count.
pub async fn compile_soundevents(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    // The soundevents gate too (review N2): the compile overwrites `<file>_c` and invalidates the
    // index, so it must not interleave with an edit of the same source file — but the write gate
    // stays free, so bridge mutators are not blocked for a container exec's duration.
    let _gate = state.soundevents_gate().await;
    super::live_common::reject_unknown_keys(&args, &["path", "timeoutMs"])?;
    let path = path_arg(&state, &args)?;
    let host = state
        .content()
        .resolve_write_ext(&path, &state.path_map, "vsndevts")
        .map_err(path_error)?;
    if !host.is_file() {
        return Err(file_not_found(&path, &host));
    }
    let timeout = compile::compile_timeout(&args, &ctx.budget)?;
    let result =
        compile::compile_resourcecompiler(state.clone(), &path, host.clone(), CompileTarget::ANY_TOOLS, timeout, None)
            .await?;

    let mut out = result.json;
    let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let errors = parse_compile_errors(out.get("stdoutTail").and_then(Value::as_str).unwrap_or(""));
    let (events, events_note) = if ok {
        match compiled_event_count(&state, &host) {
            Some(count) => (json!(count), None),
            None => (
                Value::Null,
                Some("the compiled file could not be re-read for an event count".to_string()),
            ),
        }
    } else {
        (Value::Null, None)
    };
    if ok {
        state.soundevent_index_invalidate();
    }

    let mut notes = vec![
        // RE R2: the addon's compiled soundevents are loaded at the next map load.
        "loaded at the next map load (run_map); a running engine keeps the previous compiled file until then"
            .to_string(),
    ];
    if let Some(note) = events_note {
        notes.push(note);
    }
    if let Some(map) = out.as_object_mut() {
        map.insert("errors".into(), Value::Array(errors));
        map.insert("events".into(), events);
        map.insert("notes".into(), json!(notes));
    }
    Ok(out.into())
}
