//! `report_ready {map?, profile?, checks?, debugPatterns?}` (streamline WP3.2): the ship-readiness
//! verdict. Every other tool answers "how much"; this one answers "is it done", so a weaker agent
//! does not have to assemble a ship check from the compile result, `list_entities` and
//! `session_status` by hand (acceptance runs 2 and 3).
//!
//! Read-only: it reads the SAVED `.vmap` (`crate::vmap`), the compiled `.vpk` and the addon tree on
//! disk, and (optionally) the live document state and two cvars through the bridge. It never saves,
//! compiles or loads.
//!
//! Result: `{verdict, profile, map, checks:[{name, status, detail, evidence?, fix?}], counts}`.
//! `status` is `pass|warn|fail|skipped` per check. `verdict` is `fail` when any check failed,
//! `incomplete` when any check that applies was skipped (or nothing was checked at all), `warn` when
//! any warned, else `pass`: **never `pass` when a check could not run or nothing ran**. `fix` is
//! `{tool, args, why}` where a tool exists for the repair, else `{do}` (a manual step).
//!
//! Checks, profile `map` ([`MAP_CHECKS`]): `saved`, `compiled`, `materials`, `prop_materials`, `spawns`,
//! `spawn_in_mover_path`, `objectives`. Profile `release` adds [`RELEASE_CHECKS`]: `loose_compiled`,
//! `debug_patterns`, `no_metamod_load` (always skipped: the tools session always loads Metamod, so
//! this cannot be proven here) and `test_cvars`. `checks` limits which run; any known name is
//! accepted whatever the profile, an unknown one is `InvalidArguments` with `didYouMean`.
//!
//! The compiled-map checks reuse `run_map`'s own lookup (`run_map::vpk_status`: the candidate
//! helper, the header check, the older-than-the-vmap rule) so the ship check and the load cannot
//! disagree.
use super::entities_file::{resolve_document, stale_read};
use crate::state::HammerState;
use crate::vmap::{Document, Entity};
use super::asset_resolve::{find_ci, norm, open_vpk, Paths, Sources};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Checks of the `map` profile, in the order they run and are reported.
pub const MAP_CHECKS: [&str; 7] =
    ["saved", "compiled", "materials", "prop_materials", "spawns", "spawn_in_mover_path", "objectives"];
/// Checks the `release` profile adds.
pub const RELEASE_CHECKS: [&str; 4] =
    ["loose_compiled", "debug_patterns", "no_metamod_load", "test_cvars"];

/// Debug leftovers `debug_patterns` always looks for (FIXME 10, 17); the caller's `debugPatterns`
/// are added to them.
const DEFAULT_DEBUG_PATTERNS: [&str; 3] =
    [r"(?i)\bdebug\s*[:=]\s*true", r"RegisterCheatCommand", r"\bsv_cheats\b"];
const DEBUG_PATTERNS_MAX: usize = 20;
/// Files `debug_patterns` / `loose_compiled` read, and how many hits / names a result lists.
const WALK_FILES_MAX: usize = 20_000;
const WALK_DEPTH_MAX: usize = 12;
const TEXT_FILE_MAX_BYTES: u64 = 1 << 20;
const HITS_MAX: usize = 40;
const LIST_MAX: usize = 30;
/// Extensions of the addon files `debug_patterns` reads as text (compiled and binary files are not).
const TEXT_EXTENSIONS: [&str; 15] = [
    "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "cfg", "txt", "kv3", "lua", "vjs", "vdata",
    "vsndevts", "vpcf",
];
const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", "target", "renders", "screenshots"];
/// The Source 2 player hull (units): half-width and standing height.
const HULL_HALF: f64 = 16.0;
const HULL_HEIGHT: f64 = 72.0;
/// A spawn suggestion clears the swept volume by this much beyond the hull.
const ESCAPE_MARGIN: f64 = 1.0;
/// A dev material every install has, offered as the stand-in in a `materials` fix.
const STAND_IN_MATERIAL: &str = "materials/dev/reflectivity_30.vmat";

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "report_ready",
            "Ship check: runs the checks and answers verdict pass|warn|fail|incomplete, each check pass|warn|fail|skipped with detail and fix. \
             profile map (default): saved, compiled (vpk current), materials (incl. textures), prop_materials (each prop's model -> .vmat -> .vtex resolve), spawns, spawn_in_mover_path, objectives (informational unless mode defuse|hostage). \
             release adds loose_compiled, debug_patterns, no_metamod_load (always skipped), test_cvars. \
             Reads the saved .vmap, the vpk and the addon tree; never saves, compiles or loads. \
             incomplete = a check could not run: not done. Not for: seeing the map (render_world). \
             Next: apply each failing check's fix, then call again.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "map": {
                    "type": "string",
                    "description": "maps/x.vmap, addon-relative; default = active doc, else the addon's only map"
                },
                "profile": {
                    "type": "string", "enum": ["map", "release"],
                    "description": "map (default) or release (adds the shipping checks)"
                },
                "checks": {
                    "type": "array", "minItems": 1, "items": {"type": "string"},
                    "description": "run only these checks (names as in the result)"
                },
                "mode": {
                    "type": "string", "enum": ["defuse", "hostage", "none"],
                    "description": "objectives: defuse needs bomb sites + buy zones, hostage needs hostages + rescue zone; else informational"
                },
                "debugPatterns": {
                    "type": "array", "maxItems": DEBUG_PATTERNS_MAX,
                    "items": {"type": "string", "minLength": 1},
                    "description": "extra regexes for debug_patterns (added to the built-in list)"
                }
            },
            "additionalProperties": false
        })),
        report_ready,
    );
}

// -------------------------------------------------------------------------------------------
// result model
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skipped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub evidence: Option<Value>,
    pub fix: Option<Value>,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
        Check { name, status, detail: detail.into(), evidence: None, fix: None }
    }
    fn evidence(mut self, evidence: Value) -> Check {
        self.evidence = Some(evidence);
        self
    }
    fn fix_tool(mut self, tool: &str, args: Value, why: &str) -> Check {
        self.fix = Some(json!({"tool": tool, "args": args, "why": why}));
        self
    }
    fn fix_manual(mut self, todo: &str) -> Check {
        self.fix = Some(json!({"do": todo}));
        self
    }
    fn to_json(&self) -> Value {
        let mut m = json!({"name": self.name, "status": self.status.as_str(), "detail": self.detail});
        if let Some(e) = &self.evidence {
            m["evidence"] = e.clone();
        }
        if let Some(f) = &self.fix {
            m["fix"] = f.clone();
        }
        m
    }
}

/// `fail` if any check failed; `incomplete` if any check was skipped or nothing ran; `warn` if any
/// warned; else `pass`. Zero checks is never a pass.
pub fn verdict(checks: &[Check]) -> &'static str {
    let any = |s: Status| checks.iter().any(|c| c.status == s);
    if checks.is_empty() {
        "incomplete"
    } else if any(Status::Fail) {
        "fail"
    } else if any(Status::Skipped) {
        "incomplete"
    } else if any(Status::Warn) {
        "warn"
    } else {
        "pass"
    }
}

fn counts(checks: &[Check]) -> Value {
    let n = |s: Status| checks.iter().filter(|c| c.status == s).count();
    json!({
        "pass": n(Status::Pass), "warn": n(Status::Warn), "fail": n(Status::Fail),
        "skipped": n(Status::Skipped), "total": checks.len(),
    })
}

// -------------------------------------------------------------------------------------------
// arguments
// -------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Request {
    names: Vec<&'static str>,
    profile: &'static str,
    patterns: Vec<(String, Regex)>,
    custom_patterns: Vec<String>,
    mode: Mode,
}

/// The game mode `objectives` holds the map to (`unknown` = the caller did not say).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Unknown,
    None,
    Defuse,
    Hostage,
}

fn parse_args(args: &Value) -> Result<Request, ToolError> {
    if let Some(obj) = args.as_object() {
        for key in obj.keys() {
            if !["map", "profile", "checks", "debugPatterns", "mode"].contains(&key.as_str()) {
                return Err(ToolError::invalid_arguments(format!(
                    "unknown argument {key:?}: report_ready takes map, profile, checks, debugPatterns, mode"
                )));
            }
        }
    }
    let profile = match args.get("profile") {
        None | Some(Value::Null) => "map",
        Some(Value::String(p)) if p == "map" => "map",
        Some(Value::String(p)) if p == "release" => "release",
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "profile must be \"map\" or \"release\", got {other}"
            )))
        }
    };
    let mode = match args.get("mode") {
        None | Some(Value::Null) => Mode::Unknown,
        Some(Value::String(m)) if m == "none" => Mode::None,
        Some(Value::String(m)) if m == "defuse" => Mode::Defuse,
        Some(Value::String(m)) if m == "hostage" => Mode::Hostage,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "mode must be \"defuse\", \"hostage\" or \"none\", got {other}"
            )))
        }
    };
    let all: Vec<&'static str> = MAP_CHECKS.iter().chain(RELEASE_CHECKS.iter()).copied().collect();
    let names: Vec<&'static str> = match args.get("checks") {
        None | Some(Value::Null) => {
            if profile == "release" {
                all.clone()
            } else {
                MAP_CHECKS.to_vec()
            }
        }
        Some(Value::Array(list)) if !list.is_empty() => {
            let mut wanted = BTreeSet::new();
            for item in list {
                let Some(text) = item.as_str() else {
                    return Err(ToolError::invalid_arguments(format!(
                        "checks must be strings, got {item}"
                    )));
                };
                match all.iter().find(|n| n.eq_ignore_ascii_case(text.trim())) {
                    Some(name) => {
                        wanted.insert(*name);
                    }
                    None => return Err(unknown_check(text, &all)),
                }
            }
            all.iter().copied().filter(|n| wanted.contains(n)).collect()
        }
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "checks must be a non-empty array of check names, got {other}"
            )))
        }
    };
    let mut custom_patterns = Vec::new();
    match args.get("debugPatterns") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) if list.len() <= DEBUG_PATTERNS_MAX => {
            for item in list {
                match item.as_str().filter(|s| !s.is_empty()) {
                    Some(s) => custom_patterns.push(s.to_string()),
                    None => {
                        return Err(ToolError::invalid_arguments(format!(
                            "debugPatterns must be non-empty regex strings, got {item}"
                        )))
                    }
                }
            }
        }
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "debugPatterns must be an array of at most {DEBUG_PATTERNS_MAX} regex strings, got {other}"
            )))
        }
    }
    let mut patterns = Vec::new();
    for text in DEFAULT_DEBUG_PATTERNS.iter().map(|s| s.to_string()).chain(custom_patterns.clone()) {
        let re = Regex::new(&text).map_err(|e| {
            ToolError::invalid_arguments(format!("debugPatterns: {text:?} is not a valid regex: {e}"))
        })?;
        patterns.push((text, re));
    }
    Ok(Request { names, profile, patterns, custom_patterns, mode })
}

fn unknown_check(given: &str, all: &[&'static str]) -> ToolError {
    let lower = given.trim().to_ascii_lowercase();
    let near: Vec<&str> = all
        .iter()
        .copied()
        .filter(|n| {
            super::compile_map::edit_distance(&lower, n) <= 3
                || n.contains(lower.as_str())
                || lower.contains(*n)
        })
        .collect();
    ToolError::invalid_arguments(format!(
        "unknown check {given:?}: the checks are {}",
        all.join(", ")
    ))
    .with_data(json!({"valid": all}))
    .did_you_mean(near)
}

// -------------------------------------------------------------------------------------------
// the map under test
// -------------------------------------------------------------------------------------------

struct Resolved {
    host: PathBuf,
    document: Arc<Document>,
    /// `maps/x.vmap`
    content_path: String,
    /// `Some(true)` = the file is Hammer's active document; `None` = the bridge could not say.
    is_active: Option<bool>,
    dirty: bool,
}

async fn resolve_map(state: &HammerState, ctx: &CallCtx, args: &Value) -> Result<Resolved, ToolError> {
    let given = args.get("map").and_then(Value::as_str).filter(|m| !m.is_empty());
    let attempt = match given {
        Some(map) => resolve_document(state, ctx, &json!({"path": map})).await,
        None => resolve_document(state, ctx, &json!({})).await,
    };
    let (host, document, is_active, dirty) = match (attempt, given) {
        (Ok(found), _) => found,
        // no map given and no active document: the addon's only map
        (Err(e), None) if e.type_ == "NoDocument" => {
            let (names, total) = super::new_map::addon_maps(state, 5);
            match (names.as_slice(), total) {
                ([only], 1) => resolve_document(state, ctx, &json!({"path": only})).await?,
                _ => {
                    return Err(ToolError::new(
                        "NoDocument",
                        "NoDocument",
                        format!(
                            "no active document and the addon {:?} has {total} maps: pass `map` \
                             (maps/x.vmap){}",
                            state.addon(),
                            if names.is_empty() { String::new() } else { format!(", e.g. {:?}", names[0]) }
                        ),
                    )
                    .with_code(-32003)
                    .retry_safe(true)
                    .with_data(json!({"maps": names, "total": total})))
                }
            }
        }
        (Err(e), _) => return Err(e),
    };
    let Some(content_path) = state.content().addon_rel(&host) else {
        return Err(ToolError::invalid_arguments(format!(
            "{} is not inside the active addon {:?} (content/csgo_addons/{}/): select its addon \
             with set_addon, or pass `map` of this addon's maps",
            host.display(),
            state.addon(),
            state.addon()
        )));
    };
    Ok(Resolved { host, document, content_path, is_active, dirty })
}

// -------------------------------------------------------------------------------------------
// the handler
// -------------------------------------------------------------------------------------------

pub async fn report_ready(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let request = parse_args(&args)?;
    let map = resolve_map(&state, &ctx, &args).await?;
    let paths = Paths::of(&state);
    let mut checks: Vec<Check> = Vec::new();
    for name in &request.names {
        let check = match *name {
            "saved" => check_saved(&state, &map),
            "compiled" => check_compiled(&state, &map),
            "materials" => {
                let (paths, doc, vpk) = (paths.clone(), map.document.clone(), map_vpk_path(&state, &map));
                blocking(name, move || check_materials(&paths, &doc, vpk.as_deref())).await
            }
            "prop_materials" => {
                let (paths, doc, vpk) = (paths.clone(), map.document.clone(), map_vpk_path(&state, &map));
                blocking(name, move || check_prop_materials(&paths, &doc, vpk.as_deref())).await
            }
            "spawns" => check_spawns(&map.document, &map.content_path),
            "spawn_in_mover_path" => check_spawn_in_mover_path(&map.document),
            "objectives" => check_objectives(&map.document, request.mode),
            "loose_compiled" => {
                let (paths, doc, vpk) = (paths.clone(), map.document.clone(), map_vpk_path(&state, &map));
                let content_path = map.content_path.clone();
                blocking(name, move || check_loose_compiled(&paths, &doc, &content_path, vpk.as_deref())).await
            }
            "debug_patterns" => {
                let (paths, doc, pats) =
                    (paths.clone(), map.document.clone(), request.patterns.clone());
                blocking(name, move || check_debug_patterns(&paths, &doc, &pats)).await
            }
            "no_metamod_load" => check_no_metamod_load(),
            "test_cvars" => check_test_cvars(&state, &ctx, &request.custom_patterns).await,
            other => unreachable!("check {other} is not in MAP_CHECKS / RELEASE_CHECKS"),
        };
        checks.push(check);
    }
    let verdict = verdict(&checks);
    let mut json = json!({
        "verdict": verdict,
        "profile": request.profile,
        "map": map.content_path,
        "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
        "counts": counts(&checks),
        "file": {
            "hostPath": map.host.display().to_string(),
            "mtime": secs(map.document.mtime),
        },
    });
    if verdict == "incomplete" {
        json["note"] = json!(
            "incomplete: at least one check could not run, so this is NOT a pass. Say which ones in \
             the report instead of calling the map done."
        );
    }
    let mut output = ToolOutput::from(json);
    let mut steps = 0;
    for check in checks.iter().filter(|c| matches!(c.status, Status::Fail | Status::Warn)) {
        let Some(fix) = &check.fix else { continue };
        let Some(tool) = fix.get("tool").and_then(Value::as_str) else { continue };
        if steps == 3 {
            break;
        }
        steps += 1;
        output = output.with_next_args(
            tool,
            &format!("{}: {}", check.name, fix.get("why").and_then(Value::as_str).unwrap_or("fix")),
            fix.get("args").cloned().unwrap_or(Value::Null),
        );
    }
    if steps == 0 {
        output = output.no_next();
    }
    Ok(output)
}

/// A sync check on the blocking pool (VPK parses and directory walks). A panic becomes a skipped
/// check that says so rather than a missing one.
async fn blocking(name: &'static str, f: impl FnOnce() -> Check + Send + 'static) -> Check {
    match tokio::task::spawn_blocking(f).await {
        Ok(check) => check,
        Err(e) => Check::new(name, Status::Skipped, format!("the check crashed: {e}")),
    }
}

fn map_vpk_path(state: &HammerState, map: &Resolved) -> Option<PathBuf> {
    let status = super::run_map::vpk_status(state, &map.content_path).ok()?;
    (status.found && status.header_problem.is_none()).then_some(status.path)
}

fn secs(t: Option<SystemTime>) -> Option<u64> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs())
}

// -------------------------------------------------------------------------------------------
// saved / compiled
// -------------------------------------------------------------------------------------------

fn check_saved(state: &HammerState, map: &Resolved) -> Check {
    let (stale, reason) = stale_read(state, map.is_active, map.dirty);
    if stale {
        let check = Check::new(
            "saved",
            Status::Fail,
            format!(
                "unsaved edits: {}; the saved file (and so the compile) does not have them",
                reason.unwrap_or_default()
            ),
        )
        .evidence(json!({"unsavedBridgeEdits": state.unsaved_edits(), "hammerDirty": map.dirty}));
        return if state.document_untitled() {
            check.fix_manual("the document is untitled: save it once from Hammer's File menu (save_map refuses it)")
        } else {
            check.fix_tool("save_map", json!({}), "persist the live edits to the .vmap")
        };
    }
    match map.is_active {
        Some(true) => Check::new(
            "saved",
            Status::Pass,
            "Hammer reports the open document clean and this server counted 0 unsaved edits",
        ),
        Some(false) => Check::new(
            "saved",
            Status::Pass,
            "this map is not Hammer's active document; the saved file is what was checked",
        ),
        None => Check::new(
            "saved",
            Status::Skipped,
            "Hammer's document state could not be read (no bridge, no active document or an unmapped \
             path), and this server counted 0 unsaved edits: cannot confirm the map is saved",
        )
        .fix_tool("session_status", json!({}), "see whether Hammer is reachable and which document is open"),
    }
}

fn check_compiled(state: &HammerState, map: &Resolved) -> Check {
    // a compile of this very map is in flight: the vpk on disk is half-written or about to change
    if let Some(job) = state.compile_jobs.running_for_path(&state.addon(), &map.content_path) {
        return Check::new(
            "compiled",
            Status::Skipped,
            format!("a compile of this map is running (job {job}): wait for it, then check again; do not start another"),
        )
        .fix_tool("job_status", json!({"jobId": job, "waitMs": 30000}), "wait for the running compile");
    }
    let status = match super::run_map::vpk_status(state, &map.content_path) {
        Ok(s) => s,
        Err(e) => {
            return Check::new("compiled", Status::Skipped, format!("cannot locate the compiled map: {}", e.message))
        }
    };
    let compile = |why: &str| ("compile_map", json!({"path": map.content_path}), why.to_string());
    let evidence = json!({
        "vpk": status.relative,
        "vpkMtime": secs(status.vpk_mtime),
        "vmapMtime": secs(status.vmap_mtime),
        "bytes": status.bytes,
    });
    if !status.found {
        let (tool, args, why) = compile("the map was never compiled");
        return Check::new(
            "compiled",
            Status::Fail,
            format!("no compiled map: {} does not exist", status.relative),
        )
        .evidence(evidence)
        .fix_tool(tool, args, &why);
    }
    if let Some(problem) = &status.header_problem {
        let (tool, args, why) = compile("recompile the damaged vpk");
        return Check::new(
            "compiled",
            Status::Fail,
            format!("{} is not a loadable VPK: {problem}", status.relative),
        )
        .evidence(evidence)
        .fix_tool(tool, args, &why);
    }
    if status.stale {
        let (tool, args, why) = compile("recompile so the vpk has the current map");
        return Check::new(
            "compiled",
            Status::Fail,
            format!(
                "{} is older than the .vmap: it was compiled before the last save, so run_map would \
                 load the old map",
                status.relative
            ),
        )
        .evidence(evidence)
        .fix_tool(tool, args, &why);
    }
    Check::new(
        "compiled",
        Status::Pass,
        format!("{} exists, has a valid VPK header and is not older than the .vmap", status.relative),
    )
    .evidence(evidence)
}

// -------------------------------------------------------------------------------------------
// VPK cache
// -------------------------------------------------------------------------------------------

// -------------------------------------------------------------------------------------------
// materials
// -------------------------------------------------------------------------------------------

/// One material the map uses: its spelling in the file and where (mesh -> face indices).
#[derive(Debug, Clone, Default)]
struct MatUse {
    spelling: String,
    /// `(nodeID, faces using it)`; `None` faces = the mesh names it only in its material list.
    meshes: Vec<(i64, Option<Vec<usize>>)>,
}

/// Every material the saved map's meshes use, keyed by lowercase path.
fn material_refs(doc: &Document) -> BTreeMap<String, MatUse> {
    let mut refs: BTreeMap<String, MatUse> = BTreeMap::new();
    let clean = |m: &str| m.trim().replace('\\', "/").trim_start_matches('/').to_string();
    for mesh in doc.meshes() {
        let mut per: BTreeMap<String, (String, Vec<usize>)> = BTreeMap::new();
        for (face, material) in mesh.face_materials.iter().enumerate() {
            let m = clean(material);
            if !m.is_empty() {
                per.entry(m.to_ascii_lowercase()).or_insert_with(|| (m, Vec::new())).1.push(face);
            }
        }
        let face_listed = !per.is_empty();
        for material in &mesh.materials {
            let m = clean(material);
            if !m.is_empty() && !per.contains_key(&m.to_ascii_lowercase()) {
                per.insert(m.to_ascii_lowercase(), (m, Vec::new()));
            }
        }
        for (key, (spelling, faces)) in per {
            let entry = refs.entry(key).or_default();
            if entry.spelling.is_empty() {
                entry.spelling = spelling;
            }
            if let Some(id) = mesh.id {
                entry.meshes.push((id, (face_listed && !faces.is_empty()).then_some(faces)));
            }
        }
    }
    refs
}

/// Materials whose compiled file is read for its texture list, at most (a bound: past it the rest
/// are reported as unchecked).
const TEXTURE_CHECK_MAX: usize = 3000;

fn check_materials(paths: &Paths, doc: &Document, map_vpk: Option<&Path>) -> Check {
    let refs = material_refs(doc);
    if refs.is_empty() {
        return Check::new(
            "materials",
            Status::Skipped,
            format!(
                "the saved map has {} mesh(es) and none names a material: nothing to check",
                doc.meshes().len()
            ),
        );
    }
    let sources = Sources::new(paths, map_vpk);
    let game_known = sources.pak_errors.is_empty();
    // 1. does a compiled material exist at all?
    let mut missing: Vec<(&MatUse, bool)> = Vec::new();
    let mut resolved: Vec<(&String, &MatUse)> = Vec::new();
    for (key, used) in &refs {
        if sources.has(&format!("{key}_c")) {
            resolved.push((key, used));
        } else {
            let source_only = find_ci(&paths.content, &used.spelling).is_some();
            missing.push((used, source_only));
        }
    }
    // 2. does each resolved material's texture list resolve? A material file can exist and still
    // render magenta because its .vtex are nowhere (ti_convoy FIXME 12).
    let mut missing_tex: Vec<(&MatUse, String)> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();
    let mut tex_seen: HashMap<String, bool> = HashMap::new();
    let mut tex_checked = 0usize;
    if game_known {
        for (n, (key, used)) in resolved.iter().enumerate() {
            let checked = if n < TEXTURE_CHECK_MAX {
                sources.check_textures(key, &mut tex_seen)
            } else {
                Err(format!("past the {TEXTURE_CHECK_MAX}-material check bound"))
            };
            let (count, missing) = match checked {
                Ok(found) => found,
                Err(why) => {
                    unchecked.push(format!("{} ({why})", used.spelling));
                    continue;
                }
            };
            tex_checked += count;
            missing_tex.extend(missing.into_iter().map(|t| (*used, t)));
        }
    } else {
        unchecked = resolved.iter().map(|(_, u)| u.spelling.clone()).collect();
    }
    // `.vmat` sources in the addon with no compiled `_c` that the map does not use: a note only
    let mut uncompiled: Vec<String> = Vec::new();
    let (found_sources, walk_truncated) =
        walk_files_capped(&paths.content.join("materials"), &["vmat"], WALK_FILES_MAX);
    for source in &found_sources {
        let Ok(rel) = source.strip_prefix(&paths.content) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let lower = rel.to_ascii_lowercase();
        if !refs.contains_key(&lower) && !sources.has(&format!("{lower}_c")) {
            uncompiled.push(rel);
        }
    }
    let meshes_of = |u: &MatUse| json!(u.meshes.iter().take(5).map(|(id, _)| id).collect::<Vec<_>>());
    let evidence = json!({
        "referenced": refs.len(), "resolved": resolved.len(), "texturesChecked": tex_checked,
        "missing": missing.iter().take(LIST_MAX).map(|(u, source_only)| json!({
            "material": u.spelling, "meshes": meshes_of(u), "meshCount": u.meshes.len(),
            "sourceOnly": source_only,
        })).collect::<Vec<_>>(),
        "missingTextures": missing_tex.iter().take(LIST_MAX).map(|(u, t)| json!({
            "material": u.spelling, "texture": t, "meshes": meshes_of(u),
        })).collect::<Vec<_>>(),
        "texturesUnchecked": unchecked.iter().take(LIST_MAX).collect::<Vec<_>>(),
        "uncompiledSources": uncompiled.iter().take(LIST_MAX).collect::<Vec<_>>(),
        "scanTruncated": walk_truncated,
    });
    let note = if uncompiled.is_empty() {
        String::new()
    } else {
        format!(
            "; note: {} .vmat source(s) in the addon have no compiled _c and the map does not use them: {}",
            uncompiled.len(),
            uncompiled.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        )
    };
    // the replacement is the alternative, never the default fix; it repaints only the faces that
    // use the bad material (set_mesh_material without `faces` would repaint the whole mesh)
    let alternative = |u: &MatUse| {
        u.meshes.first().map(|(node, faces)| {
            let mut args = json!({"nodeID": node, "material": STAND_IN_MATERIAL});
            if let Some(faces) = faces {
                args["faces"] = json!(faces);
            }
            json!({"tool": "set_mesh_material", "args": args,
                   "why": format!("or repaint only the faces that use {} with an existing material", u.spelling)})
        })
    };
    let hard_missing = !missing.is_empty() && game_known;
    if hard_missing || !missing_tex.is_empty() {
        let mut parts = Vec::new();
        if hard_missing {
            parts.push(format!(
                "{} of {} material(s) resolve nowhere (not in the addon, the map vpk or the game VPKs): {}",
                missing.len(),
                refs.len(),
                missing.iter().take(5).map(|(u, _)| u.spelling.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if !missing_tex.is_empty() {
            parts.push(format!(
                "{} texture(s) of resolved materials resolve nowhere, so those faces render magenta: {}",
                missing_tex.len(),
                missing_tex
                    .iter()
                    .take(5)
                    .map(|(u, t)| format!("{} -> {t}", u.spelling))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut check =
            Check::new("materials", Status::Fail, format!("{}{note}", parts.join("; "))).evidence(evidence);
        let (first, mut fix) = match (hard_missing, missing.first(), missing_tex.first()) {
            (true, Some((u, true)), _) => (
                *u,
                json!({"tool": "compile_asset", "args": {"path": u.spelling},
                       "why": format!("{} has a .vmat source in the addon but no compiled _c", u.spelling)}),
            ),
            (true, Some((u, false)), _) => (
                *u,
                json!({"do": format!("supply a compiled {}: add or compile the material in the addon", u.spelling)}),
            ),
            (_, _, Some((u, t))) => (
                *u,
                json!({"do": format!("supply the texture {t} for {} (compile its source into the addon, or fix the material's texture path)", u.spelling)}),
            ),
            _ => unreachable!("a failure has a missing material or texture"),
        };
        if let Some(alt) = alternative(first) {
            fix["alternative"] = alt;
        }
        check.fix = Some(fix);
        return check;
    }
    if !missing.is_empty() {
        // some materials are absent, but a game pak could not be read to look there
        return Check::new(
            "materials",
            Status::Skipped,
            format!(
                "{} of {} material(s) were not found in the addon or the map vpk ({}), and the game VPKs could not be read to look there ({}): cannot tell whether they are game materials",
                missing.len(),
                refs.len(),
                missing.iter().take(5).map(|(u, _)| u.spelling.as_str()).collect::<Vec<_>>().join(", "),
                sources.pak_errors.join("; ")
            ),
        )
        .evidence(evidence);
    }
    if !unchecked.is_empty() || walk_truncated {
        return Check::new(
            "materials",
            Status::Skipped,
            format!(
                "all {} material(s) resolve, but their textures were not checked for {} of them ({}){}: this is not a pass for \"renders\"",
                refs.len(),
                unchecked.len(),
                if game_known {
                    "the compiled file could not be read, or its texture list is missing, truncated or malformed (see texturesUnchecked)".to_string()
                } else {
                    sources.pak_errors.join("; ")
                },
                if walk_truncated { "; the addon materials scan was truncated" } else { "" }
            ),
        )
        .evidence(evidence);
    }
    Check::new(
        "materials",
        Status::Pass,
        format!(
            "all {} material(s) resolve and their {tex_checked} texture reference(s) resolve{note}",
            refs.len()
        ),
    )
    .evidence(evidence)
}

// -------------------------------------------------------------------------------------------
// prop_materials
// -------------------------------------------------------------------------------------------

/// Distinct models whose compiled file is read; past it the rest are reported as unchecked.
const PROP_MODEL_MAX: usize = 2000;

/// One prop using a model: `(nodeID, targetname, class)`.
type PropUse = (Option<i64>, String, String);

/// Why one model does not render, first broken link only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelLink {
    Ok,
    /// Resolves in the game paks only; its materials are in the same paks, so it is not opened.
    Shipped,
    NotCompiled,
    MaterialMissing(String),
    TextureMissing { material: String, texture: String },
    Unchecked(String),
}

/// `model` keyvalue -> `models/x.vmdl` (lowercase, no `_c`); `None` when it is not a `.vmdl`.
fn model_key(raw: &str) -> Option<String> {
    let key = norm(raw);
    let key = key.strip_suffix("_c").unwrap_or(&key).to_string();
    key.ends_with(".vmdl").then_some(key)
}

/// For every entity with a `model` keyvalue: the compiled model resolves, every `.vmat` in its
/// reference list resolves to a `.vmat_c`, and every `.vtex` in that material's list to a
/// `.vtex_c` (the order the engine looks: addon game dir, map vpk, game dirs, game paks).
/// A model that only the game paks hold is a shipped model: it passes at the model level (its
/// materials ship in the same paks), so a typo in a `models/props/...` path fails and a real one
/// costs no read. Models are cached; at most [`PROP_MODEL_MAX`] distinct ones are opened.
pub(crate) fn check_prop_materials(paths: &Paths, doc: &Document, map_vpk: Option<&Path>) -> Check {
    // model key -> (spelling, props using it as (nodeID, targetname, class))
    let mut models: BTreeMap<String, (String, Vec<PropUse>)> = BTreeMap::new();
    for entity in doc.entities() {
        let Some(raw) = kv_text(entity, "model") else { continue };
        let Some(key) = model_key(&raw) else { continue };
        models
            .entry(key)
            .or_insert_with(|| (raw.trim().to_string(), Vec::new()))
            .1
            .push((entity.node_id, entity.targetname.clone().unwrap_or_default(), entity.classname.clone()));
    }
    let props: usize = models.values().map(|(_, p)| p.len()).sum();
    if models.is_empty() {
        return Check::new("prop_materials", Status::Pass, "no entity in the saved map names a model: nothing to check");
    }
    let sources = Sources::new(paths, map_vpk);
    let game_known = sources.pak_errors.is_empty();
    let map_pak = map_vpk.and_then(|p| open_vpk(p).ok());
    let mut tex_seen: HashMap<String, bool> = HashMap::new();
    let mut mat_cache: HashMap<String, Result<Option<ModelLink>, String>> = HashMap::new();
    let mut links: Vec<(&String, ModelLink)> = Vec::new();
    for (n, key) in models.keys().enumerate() {
        let compiled = format!("{key}_c");
        let in_addon = find_ci(&paths.game, &compiled).is_some()
            || map_pak.as_ref().is_some_and(|v| v.contains(&compiled));
        let link = if n >= PROP_MODEL_MAX {
            ModelLink::Unchecked(format!("past the {PROP_MODEL_MAX}-model check bound"))
        } else if !in_addon {
            if !sources.has(&compiled) {
                if game_known {
                    ModelLink::NotCompiled
                } else {
                    ModelLink::Unchecked(format!("game paks unreadable: {}", sources.pak_errors.join("; ")))
                }
            } else {
                ModelLink::Shipped
            }
        } else {
            addon_model_link(&sources, &compiled, game_known, &mut mat_cache, &mut tex_seen)
        };
        links.push((key, link));
    }
    let broken = |l: &ModelLink| {
        matches!(l, ModelLink::NotCompiled | ModelLink::MaterialMissing(_) | ModelLink::TextureMissing { .. })
    };
    let failing: Vec<&(&String, ModelLink)> = links.iter().filter(|(_, l)| broken(l)).collect();
    let unchecked: Vec<&(&String, ModelLink)> =
        links.iter().filter(|(_, l)| matches!(l, ModelLink::Unchecked(_))).collect();
    let describe = |link: &ModelLink| match link {
        ModelLink::NotCompiled => "model not compiled".to_string(),
        ModelLink::MaterialMissing(m) => format!("material {m} missing"),
        ModelLink::TextureMissing { material, texture } => {
            format!("texture {texture} missing (of {material})")
        }
        ModelLink::Unchecked(why) => format!("not checked: {why}"),
        ModelLink::Ok | ModelLink::Shipped => "ok".to_string(),
    };
    let who = |key: &String| {
        let (spelling, users) = &models[key];
        let names: Vec<String> = users
            .iter()
            .take(3)
            .map(|(id, tn, _)| match (id, tn.is_empty()) {
                (Some(id), true) => format!("node {id}"),
                (Some(id), false) => format!("node {id} '{tn}'"),
                (None, _) => format!("'{tn}'"),
            })
            .collect();
        let more = if users.len() > 3 { format!(" +{} more", users.len() - 3) } else { String::new() };
        format!("{} ({spelling}{})", names.join(", "), more)
    };
    let evidence = json!({
        "props": props, "models": models.len(),
        "shipped": links.iter().filter(|(_, l)| *l == ModelLink::Shipped).count(),
        "failing": failing.iter().take(LIST_MAX).map(|(key, link)| {
            let (spelling, users) = &models[*key];
            json!({
                "model": spelling, "problem": describe(link),
                "props": users.iter().take(5).map(|(id, tn, class)| json!({"nodeID": id, "targetname": tn, "class": class})).collect::<Vec<_>>(),
                "propCount": users.len(),
            })
        }).collect::<Vec<_>>(),
        "unchecked": unchecked.iter().take(LIST_MAX).map(|(key, link)| json!({"model": models[*key].0, "why": describe(link)})).collect::<Vec<_>>(),
    });
    if let Some((key, link)) = failing.first().map(|f| (f.0, &f.1)) {
        let detail = format!(
            "{} of {} model(s) used by {props} prop(s) do not resolve to renderable materials: {}",
            failing.len(),
            models.len(),
            failing
                .iter()
                .take(5)
                .map(|(k, l)| format!("{}: {}", who(k), describe(l)))
                .collect::<Vec<_>>()
                .join("; ")
        );
        let spelling = &models[key].0;
        let source_in_addon = find_ci(&paths.content, spelling).is_some();
        let in_addon = find_ci(&paths.game, &format!("{key}_c")).is_some();
        let check = Check::new("prop_materials", Status::Fail, detail).evidence(evidence);
        return match link {
            ModelLink::NotCompiled if source_in_addon => check.fix_tool(
                "model_compile",
                json!({"path": spelling}),
                &format!("{spelling} has a .vmdl source in the addon but no compiled _c"),
            ),
            ModelLink::NotCompiled => check.fix_manual(&format!(
                "the model path {spelling} is wrong: it is not in the addon, the map vpk or the game paks (find_assets / list_models list the real ones), or import the file with model_import"
            )),
            ModelLink::MaterialMissing(m) if find_ci(&paths.content, m).is_some() => check.fix_tool(
                "compile_asset",
                json!({"path": m}),
                &format!("{m} has a source in the addon but no compiled _c"),
            ),
            _ if in_addon => check.fix_manual(&format!(
                "re-import the model with model_import {{source, overwrite:true}} (materials:\"generate\" writes the .vmat and textures) or point its remaps at existing materials (materials:\"existing\", materialMap), then model_compile {{path:\"{spelling}\"}}"
            )),
            _ => check.fix_manual(&format!("supply the missing material/texture of {spelling}")),
        };
    }
    if !unchecked.is_empty() {
        return Check::new(
            "prop_materials",
            Status::Skipped,
            format!(
                "{} of {} model(s) could not be checked ({}): this is not a pass for \"renders\"",
                unchecked.len(),
                models.len(),
                unchecked.iter().take(3).map(|(k, l)| format!("{}: {}", models[*k].0, describe(l))).collect::<Vec<_>>().join("; ")
            ),
        )
        .evidence(evidence);
    }
    let shipped = links.iter().filter(|(_, l)| *l == ModelLink::Shipped).count();
    Check::new(
        "prop_materials",
        Status::Pass,
        format!(
            "all {} model(s) of {props} prop(s) resolve ({} addon model(s) with their materials and textures, {shipped} shipped model(s) checked to the model only)",
            models.len(),
            models.len() - shipped
        ),
    )
    .evidence(evidence)
}

/// [`check_prop_materials`] for a hand-built document (test seam: no `.vmap` file needed).
#[doc(hidden)]
pub fn check_prop_materials_in(app_root: &Path, addon: &str, doc: &Document, map_vpk: Option<&Path>) -> Check {
    let paths = Paths {
        content: app_root.join("content/csgo_addons").join(addon),
        game: app_root.join("game/csgo_addons").join(addon),
        app_root: app_root.to_path_buf(),
    };
    check_prop_materials(&paths, doc, map_vpk)
}

/// The first broken link of one compiled addon model (`compiled` = `models/x.vmdl_c`).
fn addon_model_link(
    sources: &Sources,
    compiled: &str,
    game_known: bool,
    mat_cache: &mut HashMap<String, Result<Option<ModelLink>, String>>,
    tex_seen: &mut HashMap<String, bool>,
) -> ModelLink {
    let Some(bytes) = sources.read(compiled) else {
        return ModelLink::Unchecked("the compiled model could not be read".into());
    };
    let refs = match crate::resource::Resource::parse(&bytes) {
        Ok(resource) => match resource.external_refs_checked() {
            Ok(list) => list,
            Err(why) => return ModelLink::Unchecked(why),
        },
        Err(e) => return ModelLink::Unchecked(format!("the compiled model is not a valid resource: {e}")),
    };
    let mut unchecked: Option<String> = None;
    for reference in refs.iter().filter(|r| r.to_ascii_lowercase().ends_with(".vmat")) {
        let material = norm(reference);
        let outcome = mat_cache
            .entry(material.clone())
            .or_insert_with(|| {
                if !sources.has(&format!("{material}_c")) {
                    return if game_known {
                        Ok(Some(ModelLink::MaterialMissing(material.clone())))
                    } else {
                        Err("game paks unreadable".to_string())
                    };
                }
                if !game_known {
                    return Err("game paks unreadable".to_string());
                }
                sources.check_textures(&material, tex_seen).map(|(_, missing)| {
                    missing.into_iter().next().map(|texture| ModelLink::TextureMissing {
                        material: material.clone(),
                        texture,
                    })
                })
            })
            .clone();
        match outcome {
            Ok(Some(link)) => return link,
            Ok(None) => {}
            Err(why) => unchecked = unchecked.or(Some(format!("{material}: {why}"))),
        }
    }
    unchecked.map_or(ModelLink::Ok, ModelLink::Unchecked)
}

// -------------------------------------------------------------------------------------------
// spawns / objectives
// -------------------------------------------------------------------------------------------

fn count_class(doc: &Document, class: &str) -> usize {
    doc.entities().iter().filter(|e| e.classname.eq_ignore_ascii_case(class)).count()
}

/// Where to put a missing spawn: next to an existing spawn (64 units over), else the centre of the
/// map's mesh bounds. `None` when the map has neither.
fn spawn_origin(doc: &Document) -> Option<[f64; 3]> {
    let near = doc
        .entities()
        .iter()
        .filter(|e| SPAWN_CLASSES.iter().any(|c| e.classname.eq_ignore_ascii_case(c)))
        .find_map(|e| e.origin);
    if let Some(o) = near {
        return Some([o[0] + 64.0, o[1], o[2]]);
    }
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let mut any = false;
    for mesh in doc.meshes().iter().filter(|m| !m.vertices.is_empty()) {
        let (a, b) = mesh.bounds();
        for i in 0..3 {
            lo[i] = lo[i].min(a[i]);
            hi[i] = hi[i].max(b[i]);
        }
        any = true;
    }
    any.then(|| [0, 1, 2].map(|i| r1((lo[i] + hi[i]) / 2.0)))
}

pub fn check_spawns(doc: &Document, content_path: &str) -> Check {
    let ct = count_class(doc, "info_player_counterterrorist");
    let t = count_class(doc, "info_player_terrorist");
    let evidence = json!({"info_player_counterterrorist": ct, "info_player_terrorist": t});
    // create_entities needs an origin: a position beside an existing spawn or the map's centre,
    // else a manual step
    let add = |check: Check, class: &str| match spawn_origin(doc) {
        Some(origin) => check.fix_tool(
            "create_entities",
            json!({"entities": [{"class": class, "origin": origin}]}),
            &format!("add a {class} (origin is beside an existing spawn or the map centre: move it onto open floor); map {content_path}"),
        ),
        None => check.fix_manual(&format!("add a {class} on open floor (the map has no spawn or mesh to place it by)")),
    };
    match (ct, t) {
        (0, 0) => add(
            Check::new(
                "spawns",
                Status::Fail,
                "no spawns at all: 0 info_player_counterterrorist and 0 info_player_terrorist; \
                 nobody can join a team",
            )
            .evidence(evidence),
            "info_player_terrorist",
        ),
        (0, _) | (_, 0) => {
            let missing = if ct == 0 { "info_player_counterterrorist" } else { "info_player_terrorist" };
            add(
                Check::new(
                    "spawns",
                    Status::Warn,
                    format!("one team has no spawn: {ct} info_player_counterterrorist, {t} info_player_terrorist"),
                )
                .evidence(evidence),
                missing,
            )
        }
        _ => Check::new(
            "spawns",
            Status::Pass,
            format!("{ct} info_player_counterterrorist, {t} info_player_terrorist"),
        )
        .evidence(evidence),
    }
}

/// Game-mode entities `objectives` looks for.
const OBJECTIVE_CLASSES: [&str; 5] = [
    "func_bomb_target",
    "func_hostage_rescue",
    "hostage_entity",
    "info_hostage_spawn",
    "func_buyzone",
];

fn check_objectives(doc: &Document, mode: Mode) -> Check {
    let found: BTreeMap<&str, usize> = OBJECTIVE_CLASSES
        .iter()
        .map(|c| (*c, count_class(doc, c)))
        .filter(|(_, n)| *n > 0)
        .collect();
    let n = |c: &str| found.get(c).copied().unwrap_or(0);
    let list = if found.is_empty() {
        "none".to_string()
    } else {
        found.iter().map(|(c, n)| format!("{n} {c}")).collect::<Vec<_>>().join(", ")
    };
    // (class, what it is called, status when missing)
    let required: &[(&[&str], &str, Status)] = match mode {
        Mode::Defuse => &[
            (&["func_bomb_target"], "bomb site (func_bomb_target)", Status::Fail),
            (&["func_buyzone"], "buy zone (func_buyzone)", Status::Warn),
        ],
        Mode::Hostage => &[
            (&["hostage_entity", "info_hostage_spawn"], "hostage (hostage_entity / info_hostage_spawn)", Status::Fail),
            (&["func_hostage_rescue"], "rescue zone (func_hostage_rescue)", Status::Fail),
        ],
        Mode::Unknown | Mode::None => &[],
    };
    let missing: Vec<(&str, Status)> = required
        .iter()
        .filter(|(classes, _, _)| classes.iter().all(|c| n(c) == 0))
        .map(|(_, what, status)| (*what, *status))
        .collect();
    if missing.is_empty() {
        let detail = match mode {
            Mode::Unknown | Mode::None if found.is_empty() => {
                "no objectives; pass mode:\"defuse\" to require them".to_string()
            }
            _ => list,
        };
        return Check::new("objectives", Status::Pass, detail).evidence(json!(found));
    }
    let status = if missing.iter().any(|(_, s)| *s == Status::Fail) { Status::Fail } else { Status::Warn };
    let names = missing.iter().map(|(w, _)| *w).collect::<Vec<_>>().join(" and ");
    Check::new(
        "objectives",
        status,
        format!("mode {}: no {names} (found: {list})", if mode == Mode::Defuse { "defuse" } else { "hostage" }),
    )
    .evidence(json!(found))
    .fix_manual(&format!("add the missing {names} entities, or pass mode:\"none\" if this map is not that mode"))
}

// -------------------------------------------------------------------------------------------
// spawn_in_mover_path
// -------------------------------------------------------------------------------------------

const SPAWN_CLASSES: [&str; 3] =
    ["info_player_counterterrorist", "info_player_terrorist", "info_player_start"];
/// Brush movers this check sweeps.
const SWEPT_CLASSES: [&str; 2] = ["func_movelinear", "func_door"];
/// Movers it cannot sweep cheaply (path- or rotation-driven): named in `detail`, and the check is
/// then at best skipped, never a pass.
const UNSWEPT_CLASSES: [&str; 5] =
    ["func_tracktrain", "func_train", "func_rotating", "func_door_rotating", "func_platrot"];

/// An axis-aligned box.
type Aabb = ([f64; 3], [f64; 3]);

#[derive(Debug, Clone)]
struct Mover {
    node_id: Option<i64>,
    class: String,
    targetname: String,
    /// The volume the brush occupies at any point of its travel.
    sweep: Aabb,
}

fn kv_text(entity: &Entity, key: &str) -> Option<String> {
    entity.keyvalues.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn kv_f64(entity: &Entity, key: &str) -> Option<f64> {
    let (_, value) = entity.keyvalues.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn kv_vec3(entity: &Entity, key: &str) -> Option<[f64; 3]> {
    let (_, value) = entity.keyvalues.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
    match value {
        Value::Array(a) if a.len() == 3 => {
            Some([a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?])
        }
        Value::String(s) => {
            let parts: Vec<f64> = s.split_whitespace().filter_map(|p| p.parse().ok()).collect();
            (parts.len() == 3).then(|| [parts[0], parts[1], parts[2]])
        }
        _ => None,
    }
}

/// Source `movedir` (pitch yaw roll, degrees) -> the unit vector it points along:
/// pitch -90 is straight up.
fn direction(angles: [f64; 3]) -> [f64; 3] {
    let (pitch, yaw) = (angles[0].to_radians(), angles[1].to_radians());
    [pitch.cos() * yaw.cos(), pitch.cos() * yaw.sin(), -pitch.sin()]
}

/// The box a brush occupies over its whole linear travel: the union of its start and end boxes.
fn swept(start: Aabb, delta: [f64; 3]) -> Aabb {
    let mut lo = start.0;
    let mut hi = start.1;
    for axis in 0..3 {
        lo[axis] = start.0[axis].min(start.0[axis] + delta[axis]);
        hi[axis] = start.1[axis].max(start.1[axis] + delta[axis]);
    }
    (lo, hi)
}

/// Does a player standing with its feet at `p` overlap `sweep`? Strict: touching (standing on top
/// of, or beside, the volume) is not an overlap.
fn hull_overlaps(p: [f64; 3], sweep: &Aabb) -> bool {
    p[0] > sweep.0[0] - HULL_HALF
        && p[0] < sweep.1[0] + HULL_HALF
        && p[1] > sweep.0[1] - HULL_HALF
        && p[1] < sweep.1[1] + HULL_HALF
        && p[2] < sweep.1[2]
        && p[2] + HULL_HEIGHT > sweep.0[2]
}

/// The nearest horizontal position just outside the volume (same z).
fn escape_point(p: [f64; 3], sweep: &Aabb) -> [f64; 3] {
    let reach = HULL_HALF + ESCAPE_MARGIN;
    let options = [
        [sweep.0[0] - reach, p[1], p[2]],
        [sweep.1[0] + reach, p[1], p[2]],
        [p[0], sweep.0[1] - reach, p[2]],
        [p[0], sweep.1[1] + reach, p[2]],
    ];
    options
        .into_iter()
        .min_by(|a, b| {
            let d = |q: &[f64; 3]| (q[0] - p[0]).abs() + (q[1] - p[1]).abs();
            d(a).total_cmp(&d(b))
        })
        .unwrap_or(p)
}

/// The tied mesh volumes of a brush entity as one box (`None` = none found).
fn tied_bounds(doc: &Document, entity: &Entity) -> Option<Aabb> {
    let children = doc.children_of(entity.node_id?)?;
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let mut any = false;
    for (_, id) in children {
        let Some(mesh) = id.and_then(|id| doc.mesh_by_id(id)) else { continue };
        if mesh.vertices.is_empty() {
            continue;
        }
        let (a, b) = mesh.bounds();
        for axis in 0..3 {
            lo[axis] = lo[axis].min(a[axis]);
            hi[axis] = hi[axis].max(b[axis]);
        }
        any = true;
    }
    any.then_some((lo, hi))
}

/// The movers of the map that can be swept, and why some could not.
fn movers(doc: &Document) -> (Vec<Mover>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for entity in doc.entities() {
        let class = entity.classname.to_ascii_lowercase();
        if UNSWEPT_CLASSES.contains(&class.as_str()) {
            notes.push(format!(
                "{} {}: path- or rotation-driven, not swept",
                entity.classname,
                entity.node_id.map(|n| n.to_string()).unwrap_or_default()
            ));
            continue;
        }
        if !SWEPT_CLASSES.contains(&class.as_str()) {
            continue;
        }
        let label = format!("{} {}", entity.classname, entity.node_id.map(|n| n.to_string()).unwrap_or_default());
        let Some(bounds) = tied_bounds(doc, entity) else {
            notes.push(format!("{label}: no tied mesh volume, not swept"));
            continue;
        };
        let dir = direction(kv_vec3(entity, "movedir").unwrap_or([0.0; 3]));
        let distance = if class == "func_movelinear" {
            kv_f64(entity, "movedistance").unwrap_or(100.0)
        } else {
            // func_door: travel = the brush's extent along the direction minus the lip
            let extent: f64 = (0..3).map(|a| (bounds.1[a] - bounds.0[a]) * dir[a].abs()).sum();
            (extent - kv_f64(entity, "lip").unwrap_or(8.0)).max(0.0)
        };
        // the saved position is at `fraction` of the travel: func_movelinear `startposition`, or its
        // `authoredposition` (1 = saved open, 2 = saved closed); a func_door's `spawnpos`
        let fraction = if class == "func_movelinear" {
            match kv_f64(entity, "authoredposition").map(|a| a as i64) {
                Some(1) => 1.0,
                Some(2) => 0.0,
                _ => kv_f64(entity, "startposition").unwrap_or(0.0),
            }
        } else {
            kv_f64(entity, "spawnpos").unwrap_or(0.0)
        }
        .clamp(0.0, 1.0);
        let back = [-dir[0] * distance * fraction, -dir[1] * distance * fraction, -dir[2] * distance * fraction];
        let fwd = [
            dir[0] * distance * (1.0 - fraction),
            dir[1] * distance * (1.0 - fraction),
            dir[2] * distance * (1.0 - fraction),
        ];
        let (a, b) = (swept(bounds, back), swept(bounds, fwd));
        out.push(Mover {
            node_id: entity.node_id,
            class: entity.classname.clone(),
            targetname: kv_text(entity, "targetname").or(entity.targetname.clone()).unwrap_or_default(),
            sweep: (
                [0, 1, 2].map(|i| a.0[i].min(b.0[i])),
                [0, 1, 2].map(|i| a.1[i].max(b.1[i])),
            ),
        });
    }
    (out, notes)
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub fn check_spawn_in_mover_path(doc: &Document) -> Check {
    let spawns: Vec<&Entity> = doc
        .entities()
        .iter()
        .filter(|e| SPAWN_CLASSES.iter().any(|c| e.classname.eq_ignore_ascii_case(c)) && e.origin.is_some())
        .collect();
    if spawns.is_empty() {
        return Check::new("spawn_in_mover_path", Status::Skipped, "the map has no spawn entities to test");
    }
    // `unswept`: movers this check could not sweep (no tied mesh, or a path/rotation-driven class)
    let (movers, unswept) = movers(doc);
    let mut hits = Vec::new();
    for spawn in &spawns {
        let Some(origin) = spawn.origin else { continue };
        for mover in &movers {
            if hull_overlaps(origin, &mover.sweep) {
                let escape = escape_point(origin, &mover.sweep);
                hits.push((spawn, mover, escape));
            }
        }
    }
    let label = |class: &str, id: Option<i64>| format!("{class} {}", id.map(|n| n.to_string()).unwrap_or_default());
    let evidence = json!({
        "spawnsChecked": spawns.len(),
        "moversSwept": movers.iter().map(|m| label(&m.class, m.node_id)).collect::<Vec<_>>(),
        "notSwept": unswept,
        "hits": hits.iter().take(LIST_MAX).map(|(s, m, e)| json!({
            "spawn": {"nodeID": s.node_id, "classname": s.classname, "origin": s.origin},
            "mover": {"nodeID": m.node_id, "classname": m.class, "targetname": m.targetname},
            "sweep": {"min": m.sweep.0.map(r1), "max": m.sweep.1.map(r1)},
            "suggestedOrigin": e.map(r1),
        })).collect::<Vec<_>>(),
    });
    let tail = if unswept.is_empty() {
        String::new()
    } else {
        format!("; NOT swept: {}", unswept.iter().take(8).cloned().collect::<Vec<_>>().join("; "))
    };
    let Some((spawn, mover, escape)) = hits.first() else {
        if !unswept.is_empty() {
            return Check::new(
                "spawn_in_mover_path",
                Status::Skipped,
                format!(
                    "{} spawn(s) clear of the {} swept mover(s), but {} mover(s) could not be swept, so this is not a pass{tail}",
                    spawns.len(),
                    movers.len(),
                    unswept.len()
                ),
            )
            .evidence(evidence);
        }
        return Check::new(
            "spawn_in_mover_path",
            Status::Pass,
            format!(
                "{} spawn(s) tested against {} moving brush(es) (func_movelinear, func_door): none overlaps a swept volume",
                spawns.len(),
                movers.len()
            ),
        )
        .evidence(evidence);
    };
    let check = Check::new(
        "spawn_in_mover_path",
        Status::Warn,
        format!(
            "{} spawn(s) lie inside the swept volume of a moving brush, e.g. {} inside {} \
             (a player spawned there gets wedged on the mover){tail}",
            hits.len(),
            label(&spawn.classname, spawn.node_id),
            label(&mover.class, mover.node_id),
        ),
    )
    .evidence(evidence);
    match spawn.node_id {
        Some(node) => check.fix_tool(
            "set_node_transform",
            json!({"nodeID": node, "origin": escape.map(r1)}),
            "move the spawn just outside the mover's lane (origin shown is the nearest clear spot; check it is on floor)",
        ),
        None => check.fix_manual("move the spawn out of the mover's lane"),
    }
}

// -------------------------------------------------------------------------------------------
// release: loose_compiled / debug_patterns / no_metamod_load / test_cvars
// -------------------------------------------------------------------------------------------

/// Files below `root` with one of `extensions` (case-insensitive), bounded in depth and in the
/// directory entries looked at. The flag is true when the bound cut the walk short: a truncated scan
/// cannot support a pass.
fn walk_files_capped(root: &Path, extensions: &[&str], cap: usize) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    let mut truncated = false;
    'walk: while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > cap {
                truncated = true;
                break 'walk;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if kind.is_dir() {
                if depth >= WALK_DEPTH_MAX {
                    truncated = true;
                } else if !SKIP_DIRS.contains(&name.as_str()) {
                    pending.push((entry.path(), depth + 1));
                }
            } else if kind.is_file() {
                let ext = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("");
                if extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                    found.push(entry.path());
                }
            }
        }
    }
    found.sort();
    (found, truncated)
}

fn walk_files(root: &Path, extensions: &[&str]) -> (Vec<PathBuf>, bool) {
    walk_files_capped(root, extensions, WALK_FILES_MAX)
}

/// A text file's contents; `None` for a binary, oversized or unreadable one.
fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > TEXT_FILE_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(4096).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Asset paths (`particles/x/y.vpcf`, `models/a.vmdl`, ...) named in `text`, normalised.
fn asset_refs(text: &str, into: &mut BTreeSet<String>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)[a-z0-9_\-./\\]+\.(?:vmdl|vpcf|vmat|vsndevts|vsnd|vtex|vmesh|vjs)\b")
            .expect("asset ref regex")
    });
    for m in re.find_iter(text) {
        let path = norm(m.as_str());
        if path.contains('/') {
            into.insert(path);
        }
    }
}

/// Text that is declarations or tooling config, not shipped script: Valve's `point_script.d.ts`
/// names `RegisterCheatCommand` and `sv_cheats` on purpose.
fn is_declaration_or_config(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    name.ends_with(".d.ts") || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

fn check_loose_compiled(paths: &Paths, doc: &Document, content_path: &str, map_vpk: Option<&Path>) -> Check {
    let compile_fix = |check: Check| {
        check.fix_tool("compile_map", json!({"path": content_path}), "compile the map so its vpk exists")
    };
    let Some(vpk) = map_vpk.and_then(|p| open_vpk(p).ok()) else {
        return compile_fix(Check::new(
            "loose_compiled",
            Status::Skipped,
            "no readable map vpk to compare the addon's compiled files against (run `compiled` first)",
        ));
    };
    // everything the map and the addon's scripts name
    let mut refs = BTreeSet::new();
    for entity in doc.entities() {
        for (_, value) in &entity.keyvalues {
            if let Value::String(s) = value {
                asset_refs(s, &mut refs);
            }
        }
    }
    refs.extend(material_refs(doc).into_keys());
    let mut truncated = false;
    for root in [&paths.content, &paths.game] {
        let (files, cut) = walk_files(root, &TEXT_EXTENSIONS);
        truncated |= cut;
        for file in files.into_iter().filter(|f| !is_declaration_or_config(f)) {
            if let Some(text) = read_text(&file) {
                asset_refs(&text, &mut refs);
            }
        }
    }
    // every compiled file in the addon's game dir, except the map's own vpk (not a `_c`) and its
    // `maps/<name>/` build output; maps/scripts/*.vjs_c and friends are real shipped files
    let stem = Path::new(content_path).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
    let own_dir = format!("maps/{stem}/");
    let (compiled, cut) = walk_files(
        &paths.game,
        &["vmdl_c", "vpcf_c", "vmat_c", "vsnd_c", "vsndevts_c", "vtex_c", "vmesh_c", "vjs_c"],
    );
    truncated |= cut;
    let mut loose: Vec<(String, bool)> = Vec::new();
    for file in compiled {
        let Ok(rel) = file.strip_prefix(&paths.game) else { continue };
        let rel = norm(&rel.to_string_lossy());
        if (!stem.is_empty() && rel.starts_with(&own_dir)) || vpk.contains(&rel) {
            continue;
        }
        let referenced = refs.contains(rel.trim_end_matches("_c"));
        loose.push((rel, referenced));
    }
    let referenced: Vec<&String> = loose.iter().filter(|(_, r)| *r).map(|(p, _)| p).collect();
    let evidence = json!({
        "referencesFound": refs.len(),
        "looseReferenced": referenced.iter().take(LIST_MAX).collect::<Vec<_>>(),
        "looseOther": loose.iter().filter(|(_, r)| !*r).map(|(p, _)| p).take(LIST_MAX).collect::<Vec<_>>(),
        "looseTotal": loose.len(),
        "scanTruncated": truncated,
    });
    if !referenced.is_empty() {
        return Check::new(
            "loose_compiled",
            Status::Warn,
            format!(
                "{} compiled file(s) the map or its scripts reference are NOT packed in the map vpk \
                 (e.g. {}); the Workshop upload must carry the whole game/csgo_addons/<addon> folder, \
                 not only the vpk",
                referenced.len(),
                referenced.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        )
        .evidence(evidence)
        .fix_manual("upload the whole addon folder (or confirm the Workshop packer includes these files)");
    }
    if truncated {
        return Check::new(
            "loose_compiled",
            Status::Skipped,
            format!("the addon scan hit its bound ({WALK_FILES_MAX} entries or {WALK_DEPTH_MAX} levels): found no referenced loose file, but the scan was incomplete"),
        )
        .evidence(evidence);
    }
    if refs.is_empty() {
        return Check::new(
            "loose_compiled",
            Status::Skipped,
            "found no asset references in the map's keyvalues, meshes or the addon's scripts: nothing to match against",
        )
        .evidence(evidence);
    }
    Check::new(
        "loose_compiled",
        Status::Pass,
        format!(
            "no referenced compiled file lives outside the map vpk ({} reference(s) checked, {} other loose compiled file(s) unreferenced)",
            refs.len(),
            loose.len()
        ),
    )
    .evidence(evidence)
}

fn check_debug_patterns(paths: &Paths, doc: &Document, patterns: &[(String, Regex)]) -> Check {
    let mut hits: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut files = 0usize;
    let mut truncated = false;
    let mut push = |hit: Value, hits: &mut Vec<Value>| {
        total += 1;
        if hits.len() < HITS_MAX {
            hits.push(hit);
        }
    };
    let mut roots = vec![paths.content.clone()];
    if paths.game != paths.content {
        roots.push(paths.game.clone());
    }
    for root in roots {
        let (found, cut) = walk_files(&root, &TEXT_EXTENSIONS);
        truncated |= cut;
        for file in found.into_iter().filter(|f| !is_declaration_or_config(f)) {
            let Some(text) = read_text(&file) else { continue };
            files += 1;
            let rel = file.strip_prefix(&paths.app_root).unwrap_or(&file).display().to_string();
            for (number, line) in text.lines().enumerate() {
                for (pattern, re) in patterns {
                    if re.is_match(line) {
                        push(
                            json!({"file": rel, "line": number + 1, "pattern": pattern,
                                   "text": line.trim().chars().take(160).collect::<String>()}),
                            &mut hits,
                        );
                        break;
                    }
                }
            }
        }
    }
    let mut keyvalues = 0usize;
    for entity in doc.entities() {
        for (key, value) in &entity.keyvalues {
            let Value::String(text) = value else { continue };
            keyvalues += 1;
            if let Some((pattern, _)) = patterns.iter().find(|(_, re)| re.is_match(text)) {
                push(
                    json!({"file": "the saved .vmap", "nodeID": entity.node_id, "classname": entity.classname,
                           "key": key, "pattern": pattern, "text": text.chars().take(160).collect::<String>()}),
                    &mut hits,
                );
            }
        }
    }
    let scanned = json!({"textFiles": files, "entityKeyvalues": keyvalues, "truncated": truncated,
                         "patterns": patterns.iter().map(|(p, _)| p).collect::<Vec<_>>()});
    if total > 0 {
        return Check::new(
            "debug_patterns",
            Status::Warn,
            format!(
                "{total} debug leftover(s) match the pattern list (first: {}:{}): fine while testing, \
                 remove or disable them for the release build{}",
                hits[0]["file"].as_str().unwrap_or("?"),
                hits[0].get("line").map(|l| l.to_string()).unwrap_or_else(|| hits[0]["nodeID"].to_string()),
                if truncated { " (the scan was truncated: there may be more)" } else { "" }
            ),
        )
        .evidence(json!({"scanned": scanned, "hits": hits, "totalHits": total}))
        .fix_manual("edit the listed lines (script debug flags, cheat commands) or the entity keyvalues, rebuild, then rerun");
    }
    if truncated {
        return Check::new(
            "debug_patterns",
            Status::Skipped,
            format!("no match in the {files} file(s) read, but the addon scan hit its bound ({WALK_FILES_MAX} entries or {WALK_DEPTH_MAX} levels): incomplete"),
        )
        .evidence(json!({"scanned": scanned}));
    }
    if files == 0 && keyvalues == 0 {
        return Check::new(
            "debug_patterns",
            Status::Skipped,
            "no addon text files and no entity keyvalues to scan",
        );
    }
    Check::new(
        "debug_patterns",
        Status::Pass,
        format!("no match in {files} addon text file(s) and {keyvalues} entity keyvalue(s)"),
    )
    .evidence(json!({"scanned": scanned}))
}

fn check_no_metamod_load() -> Check {
    Check::new(
        "no_metamod_load",
        Status::Skipped,
        "the tools session always loads Metamod, so this server cannot prove the map and its script \
         work without it (ti_convoy FIXME 6)",
    )
    .fix_manual(
        "load the map once in a session WITHOUT Metamod (the plain client or a dedicated server without the \
         metamod gameinfo entry) and check it spawns, the objectives work and the script runs; then state that in the report",
    )
}

async fn check_test_cvars(state: &HammerState, ctx: &CallCtx, custom: &[String]) -> Check {
    let deadline = match ctx.budget.deadline_ms() {
        Ok(d) => d,
        Err(e) => return Check::new("test_cvars", Status::Skipped, e.message),
    };
    let local = super::run_map::read_game_local(state, deadline).await;
    match local.as_ref().and_then(|l| l.mounted()) {
        Some(true) => {}
        Some(false) => {
            return Check::new("test_cvars", Status::Skipped, "no map is running in the tools engine, so there are no session cvars to read")
        }
        None => {
            return Check::new(
                "test_cvars",
                Status::Skipped,
                "cannot tell whether a map is running (no bridge, or it does not expose game.local)",
            )
        }
    }
    // plain lower-case words in the caller's pattern list are cvar names too
    let mut names = vec!["sv_cheats".to_string()];
    names.extend(
        custom
            .iter()
            .filter(|p| p.len() <= 64 && p.contains('_') && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .take(8)
            .cloned(),
    );
    names.dedup();
    let mut bridge = state.bridge.lock().await;
    if !bridge.has("cvar.get") {
        return Check::new("test_cvars", Status::Skipped, "the bridge does not expose cvar.get");
    }
    let reply = match bridge
        .request("cvar.get", json!({"names": names}), RequestOpts::deadline(deadline))
        .await
    {
        Ok(r) => r.result,
        Err(e) => return Check::new("test_cvars", Status::Skipped, format!("cvar.get failed: {}", e.message)),
    };
    let entries = reply.get("cvars").and_then(Value::as_array).cloned().unwrap_or_default();
    let value_of = |name: &str| {
        entries.iter().find(|e| e.get("name").and_then(Value::as_str) == Some(name)).and_then(|e| {
            if e.get("found").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            match e.get("value")? {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            }
        })
    };
    let cheats = value_of("sv_cheats");
    // cvars the caller named through debugPatterns: set to anything but 0/false is a warning too
    let set_extra: Vec<(String, String)> = names
        .iter()
        .filter(|n| *n != "sv_cheats")
        .filter_map(|n| value_of(n).map(|v| (n.clone(), v)))
        .filter(|(_, v)| !matches!(v.trim(), "" | "0" | "false"))
        .collect();
    let evidence = json!({"cvars": entries});
    let cheats_on = matches!(cheats.as_deref().map(str::trim), Some("1") | Some("true"));
    if cheats_on || !set_extra.is_empty() {
        let mut parts = Vec::new();
        if cheats_on {
            parts.push(
                "sv_cheats is 1 in this session: the tools session turns it on by default, so cheat-only \
                 test commands work here and not on a normal server. Test once with sv_cheats 0 before release"
                    .to_string(),
            );
        }
        if !set_extra.is_empty() {
            parts.push(format!(
                "test cvar(s) set: {}",
                set_extra.iter().map(|(n, v)| format!("{n} {v}")).collect::<Vec<_>>().join(", ")
            ));
        }
        let check = Check::new("test_cvars", Status::Warn, parts.join("; ")).evidence(evidence);
        return if cheats_on {
            check.fix_tool(
                "console_exec",
                json!({"command": "sv_cheats 0"}),
                "turn it off, retest, then put it back with sv_cheats 1 if you keep testing",
            )
        } else {
            check.fix_manual("reset the listed cvars in the session (console_exec <name> 0) and retest")
        };
    }
    match cheats.as_deref().map(str::trim) {
        Some(_) => Check::new("test_cvars", Status::Pass, "sv_cheats is off in this session").evidence(evidence),
        None => Check::new("test_cvars", Status::Skipped, "sv_cheats could not be read (cvar.get returned no value)").evidence(evidence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmap::mesh::MeshRecord;

    fn entity(id: i64, class: &str, origin: Option<[f64; 3]>, kv: Vec<(&str, Value)>) -> Entity {
        Entity {
            guid: format!("g{id}"),
            node_class: "CMapEntity".into(),
            node_id: Some(id),
            reference_id: None,
            classname: class.into(),
            targetname: None,
            origin,
            angles: None,
            scales: None,
            keyvalues: kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            connections: Vec::new(),
        }
    }

    fn cube(id: i64, lo: [f64; 3], hi: [f64; 3]) -> MeshRecord {
        MeshRecord {
            id: Some(id),
            origin: None,
            angles: None,
            scales: None,
            vertices: vec![lo, hi],
            faces: vec![],
            face_materials: vec![],
            materials: vec![],
            ancestors: None,
        }
    }

    /// One func_movelinear (mesh 100: x 0..100, y 0..100, z 0..100) moving +x by 200, plus spawns.
    fn doc_with_spawns(spawns: Vec<[f64; 3]>) -> Document {
        let mut entities = vec![entity(
            10,
            "func_movelinear",
            Some([0.0; 3]),
            vec![("movedir", json!([0.0, 0.0, 0.0])), ("movedistance", json!("200"))],
        )];
        for (i, origin) in spawns.into_iter().enumerate() {
            entities.push(entity(20 + i as i64, "info_player_terrorist", Some(origin), vec![]));
        }
        let children: HashMap<i64, Vec<(String, Option<i64>)>> =
            HashMap::from([(10, vec![("CMapMesh".to_string(), Some(100))])]);
        Document::from_parts(
            PathBuf::from("maps/t.vmap"),
            entities,
            vec![cube(100, [0.0; 3], [100.0; 3])],
        )
        .with_children(children)
    }

    #[test]
    fn verdict_is_never_pass_without_a_check_that_ran() {
        assert_eq!(verdict(&[]), "incomplete");
        let c = |s| Check::new("x", s, "");
        assert_eq!(verdict(&[c(Status::Pass)]), "pass");
        assert_eq!(verdict(&[c(Status::Pass), c(Status::Warn)]), "warn");
        assert_eq!(verdict(&[c(Status::Warn), c(Status::Skipped)]), "incomplete");
        assert_eq!(verdict(&[c(Status::Skipped), c(Status::Fail)]), "fail");
    }

    #[test]
    fn a_spawn_inside_a_movelinear_sweep_warns_and_one_outside_does_not() {
        // the box covers x 0..100 and travels +x 200: the swept lane is x 0..300
        let inside = check_spawn_in_mover_path(&doc_with_spawns(vec![[250.0, 50.0, 0.0]]));
        assert_eq!(inside.status, Status::Warn, "{}", inside.detail);
        let fix = inside.fix.expect("a fix");
        assert_eq!(fix["tool"], json!("set_node_transform"));
        assert_eq!(fix["args"]["nodeID"], json!(20));
        // the suggestion clears the lane
        let to = &fix["args"]["origin"];
        let moved = [to[0].as_f64().unwrap(), to[1].as_f64().unwrap(), to[2].as_f64().unwrap()];
        assert!(!hull_overlaps(moved, &([0.0; 3], [300.0, 100.0, 100.0])), "{moved:?}");

        // 330 > 300 + 16: clear of the lane
        let outside = check_spawn_in_mover_path(&doc_with_spawns(vec![[330.0, 50.0, 0.0]]));
        assert_eq!(outside.status, Status::Pass, "{}", outside.detail);
        // beside the lane in y, above it in z (feet on top of the box), and just past the hull
        for origin in [[150.0, 130.0, 0.0], [150.0, 50.0, 100.0], [150.0, 50.0, -72.0], [317.0, 50.0, 0.0]] {
            let c = check_spawn_in_mover_path(&doc_with_spawns(vec![origin]));
            assert_eq!(c.status, Status::Pass, "{origin:?}: {}", c.detail);
        }
    }

    #[test]
    fn movedir_up_sweeps_vertically_and_a_door_uses_its_extent_minus_the_lip() {
        assert!((direction([-90.0, 0.0, 0.0])[2] - 1.0).abs() < 1e-9, "pitch -90 is up");
        let d = direction([0.0, 90.0, 0.0]);
        assert!(d[0].abs() < 1e-9 && (d[1] - 1.0).abs() < 1e-9);
        let (lo, hi) = swept(([0.0; 3], [10.0; 3]), [0.0, 0.0, 50.0]);
        assert_eq!((lo[2], hi[2]), (0.0, 60.0));
        // a func_door: movedir up, 100 tall, lip 8 -> travels 92
        let door = entity(5, "func_door", None, vec![("movedir", json!("-90 0 0")), ("lip", json!("8"))]);
        let spawn = entity(6, "info_player_terrorist", Some([5.0, 5.0, 150.0]), vec![]);
        let doc = Document::from_parts(PathBuf::from("m.vmap"), vec![door, spawn], vec![cube(1, [0.0; 3], [10.0, 10.0, 100.0])])
            .with_children(HashMap::from([(5, vec![("CMapMesh".to_string(), Some(1))])]));
        let (found, _) = movers(&doc);
        assert_eq!(found[0].sweep.1[2], 192.0);
        assert_eq!(check_spawn_in_mover_path(&doc).status, Status::Warn);
    }

    #[test]
    fn spawns_and_objectives_decide_from_counts() {
        let doc = |classes: &[&str]| {
            Document::from_entities(
                PathBuf::from("m.vmap"),
                classes.iter().enumerate().map(|(i, c)| entity(i as i64, c, None, vec![])).collect(),
            )
        };
        let both = doc(&["info_player_terrorist", "info_player_counterterrorist"]);
        assert_eq!(check_spawns(&both, "maps/m.vmap").status, Status::Pass);
        let one = doc(&["info_player_terrorist"]);
        let c = check_spawns(&one, "maps/m.vmap");
        assert_eq!(c.status, Status::Warn);
        assert!(c.fix.unwrap()["do"].as_str().unwrap().contains("info_player_counterterrorist"), "no origin to place it by: manual");
        assert_eq!(check_spawns(&doc(&[]), "maps/m.vmap").status, Status::Fail);
        let obj = |classes: &[&str], mode| check_objectives(&doc(classes), mode);
        assert_eq!(obj(&["func_buyzone"], Mode::Unknown).status, Status::Pass);
        // a blockout / custom-mode map is informational unless the caller names a mode
        for mode in [Mode::Unknown, Mode::None] {
            let c = obj(&["info_target"], mode);
            assert_eq!(c.status, Status::Pass);
            assert!(c.detail.contains("pass mode:\"defuse\""), "{}", c.detail);
        }
        assert_eq!(obj(&["func_bomb_target", "func_buyzone"], Mode::Defuse).status, Status::Pass);
        assert_eq!(obj(&["func_buyzone"], Mode::Defuse).status, Status::Fail);
        assert_eq!(obj(&["func_bomb_target"], Mode::Defuse).status, Status::Warn);
        assert_eq!(obj(&["hostage_entity", "func_hostage_rescue"], Mode::Hostage).status, Status::Pass);
        assert_eq!(obj(&["hostage_entity"], Mode::Hostage).status, Status::Fail);
        assert_eq!(obj(&["func_bomb_target"], Mode::Hostage).status, Status::Fail);
    }

    #[test]
    fn a_missing_spawn_fix_carries_an_origin_or_is_manual() {
        let spawn = entity(1, "info_player_terrorist", Some([100.0, 200.0, 8.0]), vec![]);
        let doc = Document::from_entities(PathBuf::from("m.vmap"), vec![spawn]);
        let c = check_spawns(&doc, "maps/m.vmap");
        assert_eq!(c.status, Status::Warn);
        let fix = c.fix.unwrap();
        assert_eq!(fix["tool"], json!("create_entities"));
        assert_eq!(fix["args"]["entities"][0]["origin"], json!([164.0, 200.0, 8.0]));
        // no spawn, but a mesh: its centre
        let doc = Document::from_parts(PathBuf::from("m.vmap"), vec![], vec![cube(1, [0.0; 3], [100.0, 200.0, 50.0])]);
        assert_eq!(check_spawns(&doc, "maps/m.vmap").fix.unwrap()["args"]["entities"][0]["origin"], json!([50.0, 100.0, 25.0]));
        // nothing to place it by: a manual step
        let doc = Document::from_entities(PathBuf::from("m.vmap"), vec![]);
        let fix = check_spawns(&doc, "maps/m.vmap").fix.unwrap();
        assert!(fix.get("tool").is_none() && fix["do"].is_string());
    }

    #[test]
    fn a_mover_that_was_not_swept_is_never_a_pass() {
        // a spawn clear of everything, plus a func_tracktrain and a movelinear with no tied mesh
        let spawn = entity(1, "info_player_terrorist", Some([5000.0, 0.0, 0.0]), vec![]);
        let train = entity(2, "func_tracktrain", Some([0.0; 3]), vec![]);
        let bare = entity(3, "func_movelinear", Some([0.0; 3]), vec![]);
        let doc = Document::from_entities(PathBuf::from("m.vmap"), vec![spawn.clone(), train, bare]);
        let c = check_spawn_in_mover_path(&doc);
        assert_eq!(c.status, Status::Skipped, "{}", c.detail);
        assert!(c.detail.contains("func_tracktrain 2") && c.detail.contains("func_movelinear 3"), "{}", c.detail);
        assert!(c.detail.contains("not a pass"));
        // only swept movers: a real pass
        assert_eq!(check_spawn_in_mover_path(&doc_with_spawns(vec![[5000.0, 0.0, 0.0]])).status, Status::Pass);
        // and a hit still warns while naming what was not swept
        let hit = spawn.clone();
        let mut hit = hit;
        hit.origin = Some([250.0, 50.0, 0.0]);
        let mut ents = doc_with_spawns(vec![]).entities().to_vec();
        ents.push(hit);
        ents.push(entity(9, "func_tracktrain", None, vec![]));
        let doc = Document::from_parts(PathBuf::from("t.vmap"), ents, vec![cube(100, [0.0; 3], [100.0; 3])])
            .with_children(HashMap::from([(10, vec![("CMapMesh".to_string(), Some(100))])]));
        let c = check_spawn_in_mover_path(&doc);
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("NOT swept"), "{}", c.detail);
    }

    #[test]
    fn startposition_and_authoredposition_move_the_sweep_both_ways() {
        // the box (x 0..100) travels 200 along +x. Saved at the open end (authoredposition 1) it
        // came from x -200..-100 and goes no further: the lane is x -200..100
        let doc = |kv: Vec<(&str, Value)>, spawn_x: f64| {
            let mut kv = kv;
            kv.push(("movedir", json!([0.0, 0.0, 0.0])));
            kv.push(("movedistance", json!("200")));
            Document::from_parts(
                PathBuf::from("m.vmap"),
                vec![
                    entity(10, "func_movelinear", Some([0.0; 3]), kv),
                    entity(20, "info_player_terrorist", Some([spawn_x, 50.0, 0.0]), vec![]),
                ],
                vec![cube(100, [0.0; 3], [100.0; 3])],
            )
            .with_children(HashMap::from([(10, vec![("CMapMesh".to_string(), Some(100))])]))
        };
        assert_eq!(check_spawn_in_mover_path(&doc(vec![], -150.0)).status, Status::Pass, "default: forward only");
        assert_eq!(check_spawn_in_mover_path(&doc(vec![("authoredposition", json!(1))], -150.0)).status, Status::Warn);
        // startposition 0.5: 100 back, 100 forward -> x -100..200
        assert_eq!(check_spawn_in_mover_path(&doc(vec![("startposition", json!("0.5"))], -90.0)).status, Status::Warn);
        assert_eq!(check_spawn_in_mover_path(&doc(vec![("startposition", json!("0.5"))], 250.0)).status, Status::Pass);
    }

    #[test]
    fn a_truncated_walk_says_so() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/tmp-walk").join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..6 {
            std::fs::write(dir.join(format!("f{i}.ts")), "x").unwrap();
        }
        let (all, cut) = walk_files_capped(&dir, &["ts"], 100);
        assert_eq!((all.len(), cut), (6, false));
        let (some, cut) = walk_files_capped(&dir, &["ts"], 3);
        assert!(cut && some.len() < 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_check_names_get_suggestions() {
        let err = parse_args(&json!({"checks": ["materal"]})).unwrap_err();
        assert_eq!(err.data["didYouMean"], json!(["materials"]));
        assert!(parse_args(&json!({"profile": "ship"})).is_err());
        assert!(parse_args(&json!({"debugPatterns": ["("]})).is_err());
        let ok = parse_args(&json!({"profile": "release"})).unwrap();
        assert_eq!(ok.names.len(), 11);
        let few = parse_args(&json!({"checks": ["spawns", "saved"]})).unwrap();
        assert_eq!(few.names, ["saved", "spawns"], "canonical order");
    }
}
