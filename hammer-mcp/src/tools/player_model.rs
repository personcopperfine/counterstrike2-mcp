//! `player_model {action}`: make the real player pawn wear an imported model (I7, cs_script route).
//!
//! The game's own script API does it: `Instance.OnPlayerReset` -> `QueueAfterThinks` ->
//! `pawn.SetModel(model)` keeps AnimGraph2 driving the pawn (verified live 2026-11-03, FIXME.md "I7
//! spike"). This tool owns one fixed script, `player_model`, in the addon's cs_script project:
//!
//! * `set {model, team?, bots?}`: one rule per slot (`any`, `ct`, `t`; `ct`/`t` win over `any`).
//!   It validates the model (the catalog, compiled), writes `src/player_model/index.ts` from ONE
//!   fixed template (the rules ride in a marked JSON const, so `status` reads them back), then runs
//!   the SAME internals as `cs_script` (`add` when missing, `write`, `build`, `compile`) and syncs
//!   the open map: one `point_script` named `player_model_script` (keyvalue `cs_script`) and one
//!   hidden `prop_dynamic` per distinct model (`player_model_precache_<k>`), because a model that is
//!   not in the map's resource manifest shows the ERROR model on the pawn.
//! * `clear {team?}`: drops a slot (all without `team`), regenerates the same way; with no rule
//!   left it deletes the `point_script` and the precache props (only entities with our targetnames).
//! * `status`: the rules from the generated file, built/compiled state, our entities in the map.
//!
//! No project: refused with `next` cs_script init (an npm install is not started implicitly; a
//! project whose `node_modules` is missing is installed by `cs_script build` itself, as for any
//! script). Nothing is saved: `save_map` -> `compile_map` -> a real session (client-mcp).
use super::cs_script as cs;
use super::{entities_live, entities_query, lifecycle, model_info};
use crate::cs_script::{self as project_api, Project};
use crate::state::HammerState;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::time::SystemTime;

pub const PLAYER_MODEL_TOOL_ORDER: [&str; 1] = ["player_model"];

const ACTIONS: [&str; 3] = ["set", "clear", "status"];
const TEAMS: [&str; 3] = ["any", "ct", "t"];
/// The fixed script name (`cs_script` project target, `maps/scripts/player_model.vjs`).
pub const SCRIPT: &str = "player_model";
/// The `point_script` entity this tool owns.
pub const SCRIPT_TARGETNAME: &str = "player_model_script";
/// The precache props: `player_model_precache_<k>`.
pub const PRECACHE_PREFIX: &str = "player_model_precache_";
const SCRIPT_FILE: &str = "index.ts";
/// First line of the generated source: a file without it is not ours and is never overwritten.
const MARKER: &str = "// @generated-by player_model (hammer-mcp): rewritten by `player_model set|clear`; do not edit";
const RULES_MARKER: &str = "// @player_model:rules";
const RULES_PREFIX: &str = "const RULES: Rules = ";
/// Precache names looked up past the current model count (leftovers of a longer rule list).
const PRECACHE_MARGIN: usize = 8;
const DEFAULT_ORIGIN: [f64; 3] = [0.0, 0.0, 0.0];
/// Keyvalues of a precache prop. Source: the real `core/base.fgd` (prop_dynamic = prop_dynamic_base +
/// EnableDisable + ...): `solid` 0 = "Not Solid"; `rendermode` kRenderNone = "Dont Render"
/// (RenderModeChoices); `disableshadows` 1 = "No Shadows" (Studiomodel). `StartDisabled` is NOT used:
/// the FGD only calls it "Start Disabled", and whether a disabled entity still precaches is not
/// established (FIXME.md I7). The model keyvalue is what the compiler lists in the manifest.
const PRECACHE_KV: [(&str, &str); 3] = [("solid", "0"), ("rendermode", "kRenderNone"), ("disableshadows", "1")];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "player_model",
            "Real player pawn wears an imported model (cs_script SetModel on respawn). \
             set {model,team?,bots?}: slots any|ct|t (ct/t beat any); builds+compiles script player_model, syncs the open map (point_script + hidden precache prop_dynamics); \
             clear {team?}; status. Needs cs_script init. Not saved; verify in a client-mcp session, not run_map.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ACTIONS},
                "model": {"type": "string", "description": "set: models/**.vmdl or agents/models/**.vmdl, compiled"},
                "team": {"type": "string", "enum": TEAMS, "description": "set, clear: slot, default any (clear: all)"},
                "bots": {"type": "boolean", "description": "set: bots get the model too, default true"}
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        player_model,
    );
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

// -------------------------------------------------------------------------------------------
// Rules and the generated source
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub any: Option<String>,
    pub ct: Option<String>,
    pub t: Option<String>,
    pub bots: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Rules { any: None, ct: None, t: None, bots: true }
    }
}

impl Rules {
    pub fn is_empty(&self) -> bool {
        self.any.is_none() && self.ct.is_none() && self.t.is_none()
    }

    fn slot(&mut self, team: &str) -> &mut Option<String> {
        match team {
            "ct" => &mut self.ct,
            "t" => &mut self.t,
            _ => &mut self.any,
        }
    }

    /// The distinct models in slot order (any, ct, t): `player_model_precache_<k>` is the k-th.
    pub fn models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for model in [&self.any, &self.ct, &self.t].into_iter().flatten() {
            if !out.contains(model) {
                out.push(model.clone());
            }
        }
        out
    }

    pub fn json(&self) -> Value {
        json!({"any": self.any, "ct": self.ct, "t": self.t, "bots": self.bots})
    }

    fn from_json(value: &Value) -> Option<Rules> {
        let slot = |key: &str| match value.get(key) {
            Some(Value::String(s)) => Some(Some(s.clone())),
            Some(Value::Null) | None => Some(None),
            _ => None,
        };
        Some(Rules { any: slot("any")?, ct: slot("ct")?, t: slot("t")?, bots: value.get("bots")?.as_bool()? })
    }
}

/// The whole `index.ts` for a rule set: one fixed template, the rules as a JSON const (JSON is valid
/// TypeScript, `serde_json` escapes every string). Deterministic: the same rules, the same bytes.
pub fn render_source(rules: &Rules) -> String {
    format!(
        "{MARKER}\n\
         import {{ Instance }} from \"cs_script/point_script\";\n\
         \n\
         type Rules = {{ any: string | null; ct: string | null; t: string | null; bots: boolean }};\n\
         {RULES_MARKER}\n\
         {RULES_PREFIX}{rules};\n\
         \n\
         // team numbers: 2 = T, 3 = CT; a slot (ct/t) wins over any\n\
         function modelFor(team: number): string | null {{\n\
         \x20   if (team === 3) return RULES.ct !== null ? RULES.ct : RULES.any;\n\
         \x20   if (team === 2) return RULES.t !== null ? RULES.t : RULES.any;\n\
         \x20   return null;\n\
         }}\n\
         \n\
         // fires on respawn, team change and round restart; the model is set after the engine's own\n\
         // setup, and it must be in the map's resource manifest (player_model_precache_<k> props)\n\
         Instance.OnPlayerReset(({{ player }}) => {{\n\
         \x20   Instance.QueueAfterThinks(() => {{\n\
         \x20       if (!player.IsValid()) return;\n\
         \x20       const model = modelFor(player.GetTeamNumber());\n\
         \x20       if (model === null) return;\n\
         \x20       if (!RULES.bots) {{\n\
         \x20           const controller = player.GetPlayerController();\n\
         \x20           if (controller !== undefined && controller.IsBot()) return;\n\
         \x20       }}\n\
         \x20       player.SetModel(model);\n\
         \x20   }});\n\
         }});\n\
         \n\
         Instance.Msg(\"[player_model] loaded\");\n",
        rules = rules.json()
    )
}

/// The rules back out of a generated source; `None` when the file is not ours or the const is damaged.
pub fn parse_source(text: &str) -> Option<Rules> {
    if text.lines().next()? != MARKER {
        return None;
    }
    let mut lines = text.lines().skip_while(|l| *l != RULES_MARKER);
    lines.next()?;
    let line = lines.next()?.strip_prefix(RULES_PREFIX)?.strip_suffix(';')?;
    Rules::from_json(&serde_json::from_str(line).ok()?)
}

// -------------------------------------------------------------------------------------------
// Entry
// -------------------------------------------------------------------------------------------

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(invalid(format!("{key} must be a string, got {other}"))),
    }
}

fn team_arg(args: &Value) -> Result<Option<&'static str>, ToolError> {
    match str_arg(args, "team")? {
        None => Ok(None),
        Some(text) => TEAMS
            .iter()
            .find(|t| **t == text)
            .copied()
            .map(Some)
            .ok_or_else(|| invalid(format!("team must be one of {TEAMS:?}, got {text:?}"))),
    }
}

pub async fn player_model(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let action = str_arg(&args, "action")?.ok_or_else(|| invalid("action is required"))?.to_string();
    if !ACTIONS.contains(&action.as_str()) {
        return Err(invalid(format!("unknown action {action:?}: one of {ACTIONS:?}")));
    }
    let allowed: &[&str] = match action.as_str() {
        "set" => &["action", "model", "team", "bots"],
        "clear" => &["action", "team"],
        _ => &["action"],
    };
    if let Some(object) = args.as_object() {
        if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid(format!("{extra:?} is not an argument of player_model {action}: it takes {allowed:?}")));
        }
    }
    let team = team_arg(&args)?;
    let mut out = match action.as_str() {
        "set" => set(&state, &ctx, &args, team).await?,
        "clear" => clear(&state, &ctx, team).await?,
        _ => status(&state, &ctx).await?,
    };
    if let Some(map) = out.json.as_object_mut() {
        map.insert("action".into(), json!(action));
    }
    Ok(out)
}

/// The cs_script project of the selected addon, or the refusal that points at `cs_script init`.
fn project(state: &HammerState) -> Result<Project, ToolError> {
    let project = cs::project_of(state)?;
    if !project.exists() {
        return Err(ToolError::new(
            "NoProject",
            "NoProject",
            format!(
                "addon {:?} has no cs_script project ({} missing): player_model is a cs_script, so scaffold the project first (it runs npm install)",
                project.addon,
                project.dir.display()
            ),
        )
        .retry_safe(true)
        .use_instead("cs_script", "scaffold the project (npm install), then repeat player_model", Some(json!({"action": "init"}))));
    }
    Ok(project)
}

fn source_path(project: &Project) -> Result<std::path::PathBuf, ToolError> {
    let path = project.source_path(SCRIPT, SCRIPT_FILE).map_err(cs::cs_error)?;
    project_api::inside_addon(&project.addon_root, &path).map_err(cs::cs_error)?;
    Ok(path)
}

enum Stored {
    Missing,
    Ours(Rules),
    /// Our marker line, but the rules const does not parse.
    Damaged,
    /// A file without our marker.
    Foreign,
}

fn read_stored(project: &Project) -> Result<(Stored, std::path::PathBuf), ToolError> {
    let path = source_path(project)?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok((Stored::Missing, path));
    };
    let state = if text.lines().next() != Some(MARKER) {
        Stored::Foreign
    } else {
        parse_source(&text).map_or(Stored::Damaged, Stored::Ours)
    };
    Ok((state, path))
}

fn not_ours(path: &std::path::Path) -> ToolError {
    ToolError::new(
        "ScriptNotOurs",
        "ScriptNotOurs",
        format!(
            "{} exists but is not a player_model generated file (first line {MARKER:?}); move your script to another name, or delete the file",
            path.display()
        ),
    )
    .retry_safe(true)
    .use_instead("cs_script", "read what is there", Some(json!({"action": "read", "name": SCRIPT})))
}

fn damaged(path: &std::path::Path) -> ToolError {
    ToolError::new(
        "ScriptDamaged",
        "ScriptDamaged",
        format!(
            "{} carries the player_model marker but its rules const cannot be read; `player_model set` or `clear` rewrites the file (earlier slots are lost, set them again)",
            path.display()
        ),
    )
    .retry_safe(true)
}

/// The rules stored in the project, for `status`: `None` when there is no script yet.
fn stored_rules(project: &Project) -> Result<Option<Rules>, ToolError> {
    match read_stored(project)? {
        (Stored::Missing, _) => Ok(None),
        (Stored::Ours(rules), _) => Ok(Some(rules)),
        (Stored::Damaged, path) => Err(damaged(&path)),
        (Stored::Foreign, path) => Err(not_ours(&path)),
    }
}

/// The rules for `set`/`clear`: a damaged file with our marker is ours to rewrite (`true` = it was
/// damaged), a file without the marker is never touched.
fn stored_for_write(project: &Project) -> Result<(Option<Rules>, bool), ToolError> {
    match read_stored(project)? {
        (Stored::Missing, _) => Ok((None, false)),
        (Stored::Ours(rules), _) => Ok((Some(rules), false)),
        (Stored::Damaged, _) => Ok((None, true)),
        (Stored::Foreign, path) => Err(not_ours(&path)),
    }
}

// -------------------------------------------------------------------------------------------
// set / clear
// -------------------------------------------------------------------------------------------

async fn validated_model(state: &HammerState, model: &str) -> Result<String, ToolError> {
    let canonical = super::model_catalog::normalize_model_path(model)
        .map_err(|reason| invalid(format!("bad model path: {reason}")))?;
    if !canonical.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'/' | b'.')) {
        return Err(invalid(format!("model {canonical:?}: only a-z, 0-9, '_', '-', '.' and '/' are allowed in a player model path")));
    }
    let resolved = match model_info::resolve(state, model).await {
        Ok(resolved) => resolved,
        Err(error) if error.name == "ModelNotFound" => {
            return Err(error.use_instead("model_import", "import it first (kind player, then it is models/<name>/<name>.vmdl)", None))
        }
        Err(error) => return Err(error),
    };
    if !resolved.compiled {
        return Err(model_info::model_not_compiled(&resolved.canonical).use_instead(
            "model_compile",
            "build the .vmdl_c (the pawn can only wear a compiled model)",
            Some(json!({"path": resolved.canonical})),
        ));
    }
    Ok(resolved.canonical)
}

async fn set(state: &HammerState, ctx: &CallCtx, args: &Value, team: Option<&'static str>) -> Result<ToolOutput, ToolError> {
    let model = str_arg(args, "model")?.ok_or_else(|| invalid("set needs model"))?;
    let bots = match args.get("bots") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(other) => return Err(invalid(format!("bots must be a boolean, got {other}"))),
    };
    let canonical = validated_model(state, model).await?;
    let project = project(state)?;
    let (stored, damaged) = stored_for_write(&project)?;
    let mut rules = stored.unwrap_or_default();
    *rules.slot(team.unwrap_or("any")) = Some(canonical);
    if let Some(bots) = bots {
        rules.bots = bots;
    }
    apply(state, ctx, &project, rules, damaged, json!({"action": "set", "model": model, "team": team.unwrap_or("any"), "bots": bots})).await
}

async fn clear(state: &HammerState, ctx: &CallCtx, team: Option<&'static str>) -> Result<ToolOutput, ToolError> {
    let project = project(state)?;
    let (stored, damaged) = stored_for_write(&project)?;
    let Some(mut rules) = stored else {
        if !damaged {
            // no script was ever generated: nothing to drop, but the map may still carry our entities
            let map = sync_report(state, ctx, &Rules::default()).await;
            let synced = map["synced"] != json!(false);
            let touched = map["deleted"].as_array().is_some_and(|d| !d.is_empty());
            let out: ToolOutput = json!({"ok": synced, "rules": Rules::default().json(), "script": {"present": false}, "map": map}).into();
            return Ok(if !synced {
                map_failed(out, "clear", json!({"action": "clear"}))
            } else if touched {
                with_save(out, true)
            } else {
                out.no_next()
            });
        }
        // a damaged generated file: nothing to keep; rewrite it with empty rules
        return apply(state, ctx, &project, Rules::default(), true, json!({"action": "clear", "team": team})).await;
    };
    match team {
        Some(team) => *rules.slot(team) = None,
        None => rules = Rules { bots: rules.bots, ..Rules::default() },
    }
    apply(state, ctx, &project, rules, damaged, json!({"action": "clear", "team": team})).await
}

/// Which step of the script pipeline stopped: the reply so far + the steps that name the way on.
fn stopped(mut report: Map<String, Value>, stage: &str, why: &str, inner: &ToolOutput, retry: Value) -> ToolOutput {
    report.insert("ok".into(), json!(false));
    report.insert("stage".into(), json!(stage));
    report.insert("message".into(), json!(why));
    let mut out: ToolOutput = Value::Object(report).into();
    match &inner.next {
        Some(steps) if !steps.is_empty() => {
            for step in steps {
                out = out.with_next_args(&step.tool, &step.why, step.args.clone().unwrap_or(Value::Null));
            }
            out.with_next_args("player_model", "then repeat this call (the steps are idempotent)", retry)
        }
        _ => out.with_next_args("player_model", "repeat this call (the steps are idempotent)", retry),
    }
}

fn step_json(out: &ToolOutput) -> Value {
    let mut value = out.json.clone();
    if let Some(map) = value.as_object_mut() {
        for noisy in ["reload", "action", "addon", "install"] {
            map.remove(noisy);
        }
    }
    value
}

/// A script pipeline step that stopped the call (`stage`, why, the step's own output).
struct Stop {
    stage: &'static str,
    why: &'static str,
    inner: ToolOutput,
}

/// add (only when the project has no such target) -> write -> build -> compile, filling `report`.
/// `Ok(Some(stop))`: build or compile failed or is still running.
async fn run_script(state: &HammerState, ctx: &CallCtx, project: &Project, rules: &Rules, report: &mut Map<String, Value>) -> Result<Option<Stop>, ToolError> {
    let targets = project.targets().map_err(cs::cs_error)?;
    if !targets.iter().any(|t| t == SCRIPT) {
        // `src/player_model/` already there with our marked file but no rollup target (an interrupted
        // add, a hand-edited rollup.config.js): re-insert only the target
        if matches!(read_stored(project)?.0, Stored::Ours(_) | Stored::Damaged) {
            project_api::add_target_only(project, SCRIPT).map_err(cs::cs_error)?;
            report.insert("add".into(), json!({"ok": true, "written": ["rollup.config.js"], "note": "only the rollup target was missing"}));
        } else {
            let added = cs::add(&json!({"name": SCRIPT}), project)?;
            report.insert("add".into(), json!({"ok": true, "written": added.json["written"]}));
        }
    }
    let source = render_source(rules);
    let path = source_path(project)?;
    let unchanged = std::fs::read_to_string(&path).map(|old| old == source).unwrap_or(false);
    if !unchanged {
        cs::write(&json!({"name": SCRIPT, "file": SCRIPT_FILE, "content": source}), project)?;
    }
    report.insert("write".into(), json!({"ok": true, "path": format!("cs_script/src/{SCRIPT}/{SCRIPT_FILE}"), "bytes": source.len(), "changed": !unchanged}));
    // build (tsc + rollup; installs node_modules itself when the project has none)
    let built = cs::build(state, ctx, &json!({"name": SCRIPT}), project).await?;
    report.insert("build".into(), step_json(&built));
    if built.json["ok"] != json!(true) {
        let why = if built.json["state"] == json!("running") {
            "npm install is still running; repeat the call"
        } else {
            "the generated script did not build (see build.failed / build.install)"
        };
        return Ok(Some(Stop { stage: "build", why, inner: built }));
    }
    // compile (.js -> .vjs_c through compile_asset)
    let compiled = cs::compile(state, ctx, &json!({"name": SCRIPT}), project).await?;
    report.insert("compile".into(), step_json(&compiled));
    if compiled.json["ok"] != json!(true) {
        let why = if compiled.json["done"] == json!(false) {
            "the .vjs_c compile is still running; repeat the call after it ends"
        } else {
            "the .vjs_c did not compile (see compile.compiled)"
        };
        return Ok(Some(Stop { stage: "compile", why, inner: compiled }));
    }
    Ok(None)
}

/// Generate -> add -> write -> build -> compile -> map sync, for the rule set `rules`. An EMPTY rule
/// set (clear) syncs the map FIRST: deleting our entities must not depend on the toolchain, so the
/// script steps run afterwards as best effort and are only reported.
async fn apply(state: &HammerState, ctx: &CallCtx, project: &Project, rules: Rules, damaged: bool, retry: Value) -> Result<ToolOutput, ToolError> {
    let mut report = Map::new();
    report.insert("rules".into(), rules.json());
    report.insert("script".into(), json!(SCRIPT));
    if damaged {
        report.insert("warning".into(), json!("the generated source had a damaged rules const (ScriptDamaged); it was rewritten from this call's rules only, earlier slots are lost"));
    }
    if rules.is_empty() {
        let map = sync_report(state, ctx, &rules).await;
        let synced = map["synced"] != json!(false);
        report.insert("map".into(), map);
        let mut steps = Map::new();
        match run_script(state, ctx, project, &rules, &mut steps).await {
            Ok(None) => {
                steps.insert("ok".into(), json!(true));
            }
            Ok(Some(stop)) => {
                steps.insert("stage".into(), json!(stop.stage));
                steps.insert("message".into(), json!(stop.why));
                steps.insert("ok".into(), json!(false));
            }
            Err(error) => {
                steps.insert("error".into(), error.to_json());
                steps.insert("ok".into(), json!(false));
            }
        }
        report.insert("scriptSteps".into(), Value::Object(steps));
        report.insert("ok".into(), json!(synced));
        let out: ToolOutput = Value::Object(report).into();
        return Ok(if synced { with_save(out, true) } else { map_failed(out, "clear", retry) });
    }
    if let Some(stop) = run_script(state, ctx, project, &rules, &mut report).await? {
        return Ok(stopped(report, stop.stage, stop.why, &stop.inner, retry));
    }
    let map = sync_report(state, ctx, &rules).await;
    let synced = map["synced"] != json!(false);
    report.insert("map".into(), map);
    report.insert("ok".into(), json!(synced));
    if !synced {
        report.insert("stage".into(), json!("map"));
        return Ok(map_failed(Value::Object(report).into(), "set", retry));
    }
    Ok(with_save(Value::Object(report).into(), false))
}

/// The map step failed: `open_map` only when there is no document, else just repeat (the retry
/// converges by targetname).
fn map_failed(out: ToolOutput, what: &str, retry: Value) -> ToolOutput {
    let no_document = out.json["map"]["noDocument"] == json!(true);
    let out = if no_document {
        out.with_next_args("open_map", "no map is open in Hammer: open the addon map", json!({"path": "maps/<map>.vmap"}))
    } else {
        out
    };
    out.with_next_args("player_model", &format!("repeat the {what}: the sync is idempotent and finishes what the failed call left"), retry)
}

const VERIFY: &str = "Hammer's run_map is NOT a valid check (it loads models on demand and the pawn stays at the origin): verify in a real session with client-mcp launch_session {kind:\"wine-tools\", addon, map}, console_exec \"sv_cheats 1; thirdperson\", screenshot";

fn with_save(mut out: ToolOutput, cleared: bool) -> ToolOutput {
    if let Some(map) = out.json.as_object_mut() {
        map.insert("saved".into(), json!(false));
        if cleared {
            map.insert("note".into(), json!("all rules are gone: the point_script and the precache props were removed from the open map; the pawn keeps its default model once the map is saved and recompiled"));
        } else {
            map.insert("verify".into(), json!(VERIFY));
        }
    }
    if cleared {
        out.with_next("save_map", "nothing auto-saves: write the map without the removed script entity and precache props")
            .with_next("compile_map", "then compile the map")
    } else {
        out.with_next("save_map", "nothing auto-saves: write the map with the script entity and precache props")
            .with_next("compile_map", "then compile the map; verify in a real session (client-mcp launch_session wine-tools + console_exec \"sv_cheats 1; thirdperson\" + screenshot), run_map does not show it")
    }
}

// -------------------------------------------------------------------------------------------
// The open map
// -------------------------------------------------------------------------------------------

struct Row {
    node_id: i64,
    targetname: String,
    origin: [f64; 3],
}

fn rows_of(reply: &Value) -> Vec<Row> {
    reply["entities"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let origin = row["origin"].as_array().map(|a| {
                        let mut o = DEFAULT_ORIGIN;
                        for (i, v) in a.iter().take(3).enumerate() {
                            o[i] = v.as_f64().unwrap_or(0.0);
                        }
                        o
                    });
                    Some(Row {
                        node_id: row["nodeID"].as_i64()?,
                        targetname: row["targetname"].as_str().unwrap_or_default().to_string(),
                        origin: origin.unwrap_or(DEFAULT_ORIGIN),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Our entities of one class by EXACT targetname (the bridge matches it case-insensitively). Never a
/// whole-class scan: a big map would cut that at the bridge's 2000 rows (`truncated`) and hide ours.
async fn find_named(state: &HammerState, ctx: &CallCtx, class: &str, name: &str) -> Result<Vec<Row>, ToolError> {
    let reply = entities_query::find_entities_live(state.clone(), ctx.clone(), json!({"class": class, "targetname": name, "limit": 100})).await?;
    Ok(rows_of(&reply.json).into_iter().filter(|r| ours(r, name)).collect())
}

async fn our_scripts(state: &HammerState, ctx: &CallCtx) -> Result<Vec<Row>, ToolError> {
    find_named(state, ctx, "point_script", SCRIPT_TARGETNAME).await
}

/// `player_model_precache_<k>` for k below `models + PRECACHE_MARGIN` (leftovers of a longer rule
/// list sit a few numbers past the current count; a name beyond that is not looked up).
async fn our_props(state: &HammerState, ctx: &CallCtx, models: usize) -> Result<Vec<Row>, ToolError> {
    let mut rows = Vec::new();
    for k in 0..models + PRECACHE_MARGIN {
        rows.extend(find_named(state, ctx, "prop_dynamic", &precache_name(k)).await?);
    }
    Ok(rows)
}

async fn keyvalue_of(state: &HammerState, ctx: &CallCtx, node_id: i64, key: &str) -> Result<Option<String>, ToolError> {
    let reply = entities_query::get_entity_live(state.clone(), ctx.clone(), json!({"nodeID": node_id})).await?;
    Ok(reply.json["keyValues"][key].as_str().map(str::to_string))
}

/// The keyvalues of an entity that differ from `want`.
async fn drift(state: &HammerState, ctx: &CallCtx, node_id: i64, want: &[(&str, String)]) -> Result<Map<String, Value>, ToolError> {
    let reply = entities_query::get_entity_live(state.clone(), ctx.clone(), json!({"nodeID": node_id})).await?;
    let mut differing = Map::new();
    for (key, value) in want {
        if reply.json["keyValues"][key].as_str() != Some(value.as_str()) {
            differing.insert((*key).into(), json!(value));
        }
    }
    Ok(differing)
}

fn script_keyvalues() -> Vec<(&'static str, String)> {
    vec![("cs_script", Project::vjs_keyvalue(SCRIPT))]
}

fn precache_keyvalues(model: &str) -> Vec<(&'static str, String)> {
    let mut kv = vec![("model", model.to_string())];
    kv.extend(PRECACHE_KV.iter().map(|(k, v)| (*k, (*v).to_string())));
    kv
}

fn ours(row: &Row, name: &str) -> bool {
    row.targetname.eq_ignore_ascii_case(name)
}

fn precache_name(k: usize) -> String {
    format!("{PRECACHE_PREFIX}{k}")
}

/// What a sync did so far; a failure part way reports it instead of "the map was not touched".
#[derive(Default)]
struct Progress {
    created: Vec<String>,
    created_nodes: Vec<Value>,
    fixed: Vec<Value>,
    deleted: Vec<String>,
}

impl Progress {
    fn json(&self) -> Value {
        json!({"created": self.created, "createdNodes": self.created_nodes, "fixed": self.fixed, "deleted": self.deleted})
    }
}

async fn sync_report(state: &HammerState, ctx: &CallCtx, rules: &Rules) -> Value {
    let mut progress = Progress::default();
    match sync_map(state, ctx, rules, &mut progress).await {
        Ok(report) => report,
        Err(error) => {
            let no_document = error.name == "NoDocument";
            let touched = !(progress.created.is_empty() && progress.fixed.is_empty() && progress.deleted.is_empty());
            let mut report = progress.json();
            report["synced"] = json!(false);
            report["noDocument"] = json!(no_document);
            report["error"] = error.to_json();
            report["note"] = json!(if no_document {
                "no map is open in Hammer; open the addon map and repeat the call"
            } else if touched {
                "the sync stopped part way: created/fixed/deleted list what was done (unsaved, undo_last takes it back); repeat the call, it finishes the rest by targetname"
            } else {
                "the sync failed before it changed anything; repeat the call"
            });
            report
        }
    }
}

/// Bring the open document to the rules: exactly one `player_model_script` point_script when any
/// rule exists, one precache prop per distinct model, nothing of ours otherwise. Creates first, then
/// keyvalue fixes, then deletes (only entities carrying our targetnames).
async fn sync_map(state: &HammerState, ctx: &CallCtx, rules: &Rules, progress: &mut Progress) -> Result<Value, ToolError> {
    let models = rules.models();
    let scripts: Vec<Row> = our_scripts(state, ctx).await?;
    let props: Vec<Row> = our_props(state, ctx, models.len()).await?;
    let want_script = !rules.is_empty();
    let origin = scripts.first().map(|r| r.origin).unwrap_or(DEFAULT_ORIGIN);

    let mut create: Vec<Value> = Vec::new();
    let mut created: Vec<String> = Vec::new();
    let mut fixes: Vec<(i64, String, Map<String, Value>)> = Vec::new();
    let mut delete: Vec<i64> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut kept: Vec<Value> = Vec::new();

    if want_script {
        match scripts.split_first() {
            None => {
                create.push(json!({"class": "point_script", "origin": origin, "keyValues": {"targetname": SCRIPT_TARGETNAME, "cs_script": Project::vjs_keyvalue(SCRIPT)}}));
                created.push(SCRIPT_TARGETNAME.into());
            }
            Some((first, extra)) => {
                let differing = drift(state, ctx, first.node_id, &script_keyvalues()).await?;
                if !differing.is_empty() {
                    fixes.push((first.node_id, SCRIPT_TARGETNAME.into(), differing));
                }
                kept.push(json!({"nodeID": first.node_id, "targetname": SCRIPT_TARGETNAME}));
                for row in extra {
                    delete.push(row.node_id);
                    deleted.push(format!("{} (duplicate, nodeID {})", row.targetname, row.node_id));
                }
            }
        }
    } else {
        for row in &scripts {
            delete.push(row.node_id);
            deleted.push(format!("{} (nodeID {})", row.targetname, row.node_id));
        }
    }

    let mut seen: Vec<String> = Vec::new();
    for row in &props {
        let wanted = (0..models.len()).map(precache_name).find(|n| ours(row, n));
        match wanted {
            Some(name) if !seen.contains(&name) => {
                let k: usize = name[PRECACHE_PREFIX.len()..].parse().unwrap_or(0);
                let differing = drift(state, ctx, row.node_id, &precache_keyvalues(&models[k])).await?;
                if !differing.is_empty() {
                    fixes.push((row.node_id, name.clone(), differing));
                }
                kept.push(json!({"nodeID": row.node_id, "targetname": name}));
                seen.push(name);
            }
            _ => {
                delete.push(row.node_id);
                deleted.push(format!("{} (nodeID {})", row.targetname, row.node_id));
            }
        }
    }
    for (k, model) in models.iter().enumerate() {
        let name = precache_name(k);
        if seen.contains(&name) {
            continue;
        }
        let mut kv = Map::new();
        kv.insert("targetname".into(), json!(name));
        for (key, value) in precache_keyvalues(model) {
            kv.insert(key.into(), json!(value));
        }
        create.push(json!({"class": "prop_dynamic", "origin": origin, "keyValues": kv}));
        created.push(name);
    }

    if !create.is_empty() {
        let out = entities_live::create_entities(state.clone(), ctx.clone(), json!({"entities": create})).await?;
        progress.created = created;
        progress.created_nodes = out.json["entities"].as_array().map(|a| a.iter().map(|n| n["nodeID"].clone()).collect()).unwrap_or_default();
    }
    for (node_id, name, values) in fixes {
        entities_live::set_entity_keyvalues(state.clone(), ctx.clone(), json!({"nodeID": node_id, "keyValues": values})).await?;
        progress.fixed.push(json!({"nodeID": node_id, "targetname": name, "set": values}));
    }
    if !delete.is_empty() {
        lifecycle::delete_nodes(state.clone(), ctx.clone(), json!({"nodeIDs": delete})).await?;
        progress.deleted = deleted;
    }
    let mut report = progress.json();
    report["synced"] = json!(true);
    report["kept"] = json!(kept);
    report["precacheModels"] = json!(models);
    report["precacheKeyValues"] = json!(PRECACHE_KV.iter().map(|(k, v)| ((*k).to_string(), json!(v))).collect::<Map<_, _>>());
    report["origin"] = json!(origin);
    Ok(report)
}

// -------------------------------------------------------------------------------------------
// status
// -------------------------------------------------------------------------------------------

fn freshness(output: Option<SystemTime>, input: Option<SystemTime>) -> &'static str {
    match (output, input) {
        (None, _) => "missing",
        (Some(o), Some(i)) if i > o => "stale",
        _ => "fresh",
    }
}

async fn map_status(state: &HammerState, ctx: &CallCtx, rules: &Rules) -> Value {
    let found = async {
        let models = rules.models();
        let scripts: Vec<Row> = our_scripts(state, ctx).await?;
        let props: Vec<Row> = our_props(state, ctx, models.len()).await?;
        let mut script_rows: Vec<Value> = Vec::new();
        for row in &scripts {
            let cs_script = keyvalue_of(state, ctx, row.node_id, "cs_script").await?;
            let ok = cs_script.as_deref() == Some(Project::vjs_keyvalue(SCRIPT).as_str());
            script_rows.push(json!({"nodeID": row.node_id, "targetname": row.targetname, "cs_script": cs_script, "ok": ok}));
        }
        let mut prop_rows: Vec<Value> = Vec::new();
        for row in &props {
            let model = keyvalue_of(state, ctx, row.node_id, "model").await?;
            let expected = (0..models.len()).find(|k| ours(row, &precache_name(*k))).map(|k| models[k].clone());
            let ok = expected.is_some() && model == expected;
            prop_rows.push(json!({"nodeID": row.node_id, "targetname": row.targetname, "model": model, "expected": expected, "ok": ok}));
        }
        let missing: Vec<String> = (0..models.len()).map(precache_name).filter(|n| !props.iter().any(|r| ours(r, n))).collect();
        let in_sync = if rules.is_empty() {
            script_rows.is_empty() && prop_rows.is_empty()
        } else {
            script_rows.len() == 1
                && script_rows.iter().all(|r| r["ok"] == json!(true))
                && missing.is_empty()
                && prop_rows.iter().all(|r| r["ok"] == json!(true))
        };
        Ok::<Value, ToolError>(json!({"available": true, "inSync": in_sync, "pointScripts": script_rows, "precacheProps": prop_rows, "missingPrecache": missing}))
    };
    match found.await {
        Ok(value) => value,
        Err(error) => json!({"available": false, "error": error.to_json()}),
    }
}

async fn status(state: &HammerState, ctx: &CallCtx) -> Result<ToolOutput, ToolError> {
    let project = cs::project_of(state)?;
    if !project.exists() {
        let out: ToolOutput = json!({"project": {"present": false, "dir": project.dir.display().to_string()}, "rules": Rules::default().json()}).into();
        return Ok(out.with_next_args("cs_script", "scaffold the project first (player_model is a cs_script)", json!({"action": "init"})));
    }
    let rules = stored_rules(&project)?;
    let present = rules.is_some();
    let rules = rules.unwrap_or_default();
    let js = project.js_path(SCRIPT);
    let vjs_c = cs::vjs_c_path(state, &project.addon, SCRIPT);
    let built = freshness(cs::mtime(&js), cs::newest_source(&project, SCRIPT));
    let compiled = freshness(cs::mtime(&vjs_c), cs::mtime(&js));
    let map = map_status(state, ctx, &rules).await;
    let out: ToolOutput = json!({
        "project": {"present": true, "dir": project.dir.display().to_string()},
        "script": {"name": SCRIPT, "present": present, "built": built, "compiled": compiled, "keyvalue": Project::vjs_keyvalue(SCRIPT)},
        "rules": rules.json(),
        "map": map,
    })
    .into();
    let repair = map["available"] == json!(true) && map["inSync"] == json!(false);
    let out = if repair {
        // re-asserting any present slot regenerates and resyncs everything (the other rules are kept)
        let slot = [("any", &rules.any), ("ct", &rules.ct), ("t", &rules.t)].into_iter().find_map(|(team, m)| m.as_ref().map(|m| (team, m.clone())));
        let args = match slot {
            Some((team, model)) => json!({"action": "set", "model": model, "team": team}),
            None => json!({"action": "clear"}),
        };
        out.with_next_args("player_model", "the map differs from the rules (or holds our entities with no rules): repeat a set (or clear) to resync it", args)
    } else {
        out.no_next()
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules { any: Some("models/a/a.vmdl".into()), ct: Some("models/b/b.vmdl".into()), t: None, bots: false }
    }

    #[test]
    fn the_rules_round_trip_through_the_generated_source() {
        let source = render_source(&rules());
        assert_eq!(parse_source(&source), Some(rules()));
        assert_eq!(source, render_source(&rules()), "deterministic");
        assert!(source.starts_with(MARKER));
        assert!(source.contains(r#"const RULES: Rules = {"any":"models/a/a.vmdl","bots":false,"ct":"models/b/b.vmdl","t":null};"#), "{source}");
        assert_eq!(parse_source("// my own script\n"), None);
        assert_eq!(parse_source(&source.replace("\"bots\":false", "\"bots\":3")), None);
    }

    #[test]
    fn models_are_distinct_in_slot_order() {
        let mut r = rules();
        assert_eq!(r.models(), ["models/a/a.vmdl", "models/b/b.vmdl"]);
        r.t = Some("models/a/a.vmdl".into());
        assert_eq!(r.models().len(), 2);
        r.any = None;
        assert_eq!(r.models(), ["models/b/b.vmdl", "models/a/a.vmdl"]);
    }
}
