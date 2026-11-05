//! `cs_script {action}`: the TypeScript project of an addon's map scripts (`crate::cs_script`),
//! driven through the MCP so an agent can author, build and ship a `point_script` without a shell.
//!
//! Actions: `init` (scaffold + npm install), `add`, `write`/`read` (the `.ts` sources only),
//! `update` (npm `@s2ze/*@latest` + the game-types check), `build` (tsc + rollup ->
//! `maps/scripts/<name>.js`, watches the console for the hot reload), `compile` (`.js` -> `.vjs_c`
//! through `compile_asset`), `status`. Everything it writes stays inside
//! `content/csgo_addons/<addon>/` (`cs_script::inside_addon`); npm/node/tsc/rollup are host child
//! processes with deadlines inside the call budget, an install that outlives the call keeps running
//! as a job and the next identical call joins it (`cs_script::run`).
//!
//! Loading (verified live 2026-10-28, accept_p4): the engine loads `maps/scripts/<name>.vjs_c`, never
//! the `.js`. A map loaded before that file exists prints `Failed to load cs_script
//! "maps/scripts/<name>.vjs"` and runs without the script (the tools engine then compiles the
//! `.vjs_c` on demand, so only the next load has it): `build` therefore points at `compile` before
//! `run_map`. While the map runs, a changed `.js` is hot-reloaded: the console shows `[AssetSystem]
//! Reloading Resource [1/1]: maps/scripts/<name>.vjs`, an on-demand recompile, then the script's
//! own top-level `Instance.Msg("[<name>] loaded")` as `[cs_script] [<name>] loaded`. `build` reports
//! `reloaded:true` on the script's `[<name>] loaded` line (or, with no such line by the end of the
//! watch, on the engine's `Reloading Resource` line for it), `false` on `Failed to compile cs_script`
//! / `Failed to load cs_script`, else `unknown`.
use super::run_map::{console_log_path, ConsoleWatch};
use crate::cs_script::{self, diag, run, types, CsError, Project};
use crate::state::HammerState;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::{Duration, Instant};

pub const CS_SCRIPT_TOOL_ORDER: [&str; 1] = ["cs_script"];

const ACTIONS: [&str; 8] = ["init", "add", "write", "read", "update", "build", "compile", "status"];
/// Left of the call budget after a step, so the reply can still be built and sent.
const MARGIN: Duration = Duration::from_millis(2_000);
/// How long `build` watches the console for the reload sign (a map must be running).
const RELOAD_WATCH: Duration = Duration::from_millis(4_000);
const RELOAD_POLL: Duration = Duration::from_millis(100);
const NPM_JOB_TIMEOUT: Duration = Duration::from_secs(600);
const WAIT_DEFAULT_MS: u64 = 30_000;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "cs_script",
            "TypeScript project for the addon's map scripts (point_script) at content/csgo_addons/<addon>/cs_script/, built to maps/scripts/<name>.js. \
             init {scripts?:[\"main\"]} scaffolds + npm install; add {name}; write {name,file?,content} / read {name,file?}: the src/<name>/*.ts, the only files you author; \
             update bumps @s2ze/* and checks them against the game's point_script.d.ts (mismatch: scripts use the game's copy); \
             build {name?} type-checks (errors: file,line,col,code,message), writes the .js, reports reloaded; compile {name?} makes the .vjs_c (pre run_map); status. \
             Keyvalue cs_script=maps/scripts/<name>.vjs. Install unfinished: repeat the call.",
            Writer,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ACTIONS},
                "name": {"type": "string", "description": "script name [a-z0-9_]+ (add, write, read; build and compile: default all)"},
                "scripts": {"type": "array", "items": {"type": "string"}, "description": "init: script names, default [\"main\"]"},
                "file": {"type": "string", "description": "write, read: .ts file under src/<name>/, default index.ts"},
                "content": {"type": "string", "description": "write: the whole file text"},
                "waitMs": {"type": "integer", "minimum": 0, "maximum": 600000, "description": "install or compile wait, default 30000; the reply says done"}
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        cs_script,
    );
}

// -------------------------------------------------------------------------------------------
// Errors
// -------------------------------------------------------------------------------------------

pub(super) fn cs_error(error: CsError) -> ToolError {
    let message = error.to_string();
    match error {
        CsError::BadName(_) | CsError::BadFile(_) => {
            ToolError::new("InvalidParams", "InvalidParams", message).with_code(-32602).retry_safe(true)
        }
        CsError::Exists(_) => ToolError::new("ProjectExists", "ProjectExists", message)
            .retry_safe(true)
            .use_instead("cs_script", "update the packages (action update) or add a script (action add)", None),
        CsError::NoProject(_) => ToolError::new("NoProject", "NoProject", message)
            .retry_safe(true)
            .use_instead("cs_script", "scaffold the project first", Some(json!({"action": "init"}))),
        CsError::Duplicate(_) => ToolError::new("DuplicateScript", "DuplicateScript", message)
            .with_code(-32602)
            .retry_safe(true),
        CsError::UnknownScript { ref have, .. } => {
            let have = have.clone();
            ToolError::new("UnknownScript", "UnknownScript", message)
                .with_code(-32602)
                .retry_safe(true)
                .with_data(json!({"scripts": have}))
        }
        CsError::Config(_) => ToolError::new("ProjectConfig", "ProjectConfig", message).retry_safe(true),
        CsError::Io(_) => ToolError::new("IoError", "IoError", message).retry_safe(false),
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

// -------------------------------------------------------------------------------------------
// Arguments
// -------------------------------------------------------------------------------------------

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(invalid(format!("{key} must be a string, got {other}"))),
    }
}

fn wait_arg(args: &Value) -> Result<Duration, ToolError> {
    match args.get("waitMs") {
        None | Some(Value::Null) => Ok(Duration::from_millis(WAIT_DEFAULT_MS)),
        Some(v) => match v.as_u64() {
            Some(ms) if ms <= 600_000 => Ok(Duration::from_millis(ms)),
            _ => Err(invalid(format!("waitMs must be an integer in 0..=600000, got {v}"))),
        },
    }
}

fn scripts_arg(args: &Value) -> Result<Vec<String>, ToolError> {
    match args.get("scripts") {
        None | Some(Value::Null) => Ok(vec!["main".to_string()]),
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= 16 => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| invalid("scripts must be an array of strings")))
            .collect(),
        Some(other) => Err(invalid(format!("scripts must be 1..16 script names, got {other}"))),
    }
}

/// The wait for a step: the caller's `waitMs`, cut to what the call budget leaves.
fn clamp(ctx: &CallCtx, wanted: Duration) -> Duration {
    wanted.min(ctx.budget.remaining().saturating_sub(MARGIN))
}

fn left(ctx: &CallCtx) -> Duration {
    ctx.budget.remaining().saturating_sub(MARGIN)
}

// -------------------------------------------------------------------------------------------
// Entry
// -------------------------------------------------------------------------------------------

pub async fn cs_script(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let action = str_arg(&args, "action")?.ok_or_else(|| invalid("action is required"))?.to_string();
    if !ACTIONS.contains(&action.as_str()) {
        return Err(invalid(format!("unknown action {action:?}: one of {ACTIONS:?}")));
    }
    let allowed: &[&str] = match action.as_str() {
        "init" => &["action", "scripts", "waitMs"],
        "add" => &["action", "name"],
        "write" => &["action", "name", "file", "content"],
        "read" => &["action", "name", "file"],
        "update" | "compile" => &["action", "name", "waitMs"],
        "build" => &["action", "name", "waitMs"],
        _ => &["action"],
    };
    if let Some(object) = args.as_object() {
        if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid(format!("{extra:?} is not an argument of cs_script {action}: it takes {allowed:?}")));
        }
    }
    let project = project_of(&state)?;
    let mut out = match action.as_str() {
        "init" => init(&state, &ctx, &args, &project).await?,
        "add" => add(&args, &project)?,
        "write" => write(&args, &project)?,
        "read" => read(&args, &project)?,
        "update" => update(&state, &ctx, &args, &project).await?,
        "build" => build(&state, &ctx, &args, &project).await?,
        "compile" => compile(&state, &ctx, &args, &project).await?,
        _ => status(&state, &ctx, &project).await?,
    };
    if let Some(map) = out.json.as_object_mut() {
        map.insert("action".into(), json!(action));
        map.insert("addon".into(), json!(project.addon));
    }
    Ok(out)
}

pub(super) fn project_of(state: &HammerState) -> Result<Project, ToolError> {
    let addon = state.addon();
    let root = state.content().addon_root();
    if !root.is_dir() {
        return Err(ToolError::new(
            "AddonNotFound",
            "AddonNotFound",
            format!(
                "the addon content folder {} does not exist (selected addon {addon:?}): set_addon to an existing addon, or new_map to create one",
                root.display()
            ),
        )
        .retry_safe(true)
        .use_instead("session_status", "shows the addon and how to pick another", None));
    }
    Ok(Project::new(&addon, &root))
}

fn programs() -> run::Programs {
    run::Programs::from_env()
}

// -------------------------------------------------------------------------------------------
// npm
// -------------------------------------------------------------------------------------------

fn install_args() -> Vec<String> {
    let mut args: Vec<String> =
        ["install", "--no-audit", "--no-fund", "--loglevel=error"].iter().map(|s| s.to_string()).collect();
    args.extend(cs_script::S2ZE_PACKAGES.iter().map(|p| format!("{p}@latest")));
    args
}

fn job_key(project: &Project) -> String {
    format!("npm:{}", project.dir.display())
}

/// The installed version of a package (`node_modules/<pkg>/package.json`).
fn installed_version(project: &Project, package: &str) -> Option<String> {
    let text = std::fs::read_to_string(project.dir.join("node_modules").join(package).join("package.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get("version")?.as_str().map(str::to_string)
}

fn versions(project: &Project) -> Value {
    let mut map = Map::new();
    for package in cs_script::S2ZE_PACKAGES {
        map.insert(package.into(), installed_version(project, package).map(Value::from).unwrap_or(Value::Null));
    }
    Value::Object(map)
}

fn run_report(result: &run::RunResult) -> Value {
    json!({
        "ok": result.ok(),
        "exitCode": result.code,
        "timedOut": result.timed_out,
        "error": result.spawn_error,
        "elapsedMs": result.elapsed_ms,
        "tail": diag::tail_lines(result.combined().trim(), 12),
    })
}

/// `npm install @s2ze/*@latest` as a job: `Ok(Some)` when it ended, `Ok(None)` when it is still
/// running (the reply tells the caller to repeat the call).
async fn npm_install(project: &Project, ctx: &CallCtx, wait: Duration) -> Option<std::sync::Arc<run::RunResult>> {
    let progs = programs();
    run::run_job(
        &job_key(project),
        progs.npm,
        install_args(),
        project.dir.clone(),
        NPM_JOB_TIMEOUT,
        clamp(ctx, wait),
    )
    .await
}

fn still_running(project: &Project, action: &str, extra: Value) -> ToolOutput {
    let mut json = json!({
        "state": "running",
        "done": false,
        "message": "npm install is still running in the background (it keeps going after this call)",
        "project": project.dir.display().to_string(),
    });
    if let (Some(map), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
        map.extend(extra.clone());
    }
    ToolOutput::from(json).with_next_args(
        "cs_script",
        "repeat the call: it joins the running install and answers when it ends",
        json!({"action": action, "waitMs": 30_000}),
    )
}

// -------------------------------------------------------------------------------------------
// The game-types check
// -------------------------------------------------------------------------------------------

async fn types_step(state: &HammerState, ctx: &CallCtx, project: &Project) -> Value {
    let game = types::game_file(&state.app_root());
    let npm = types::npm_file(&project.dir);
    let game_text = std::fs::read_to_string(&game).ok();
    let npm_text = std::fs::read_to_string(&npm).ok();
    let verdict = match (&game_text, &npm_text) {
        (None, _) => types::Verdict::Unknown(format!(
            "the game's own declaration {} was not found (it ships with the cs_script_demo addon)",
            game.display()
        )),
        (_, None) => types::Verdict::Unknown("@s2ze/types is not installed in the project (npm install)".to_string()),
        (Some(g), Some(n)) if types::normalise(g) == types::normalise(n) => types::Verdict::Equal,
        (Some(_), Some(_)) => {
            let progs = programs();
            let result = run::run(
                &progs.node,
                &["--input-type=module", "-e", cs_script::FINGERPRINT_JS, &game.display().to_string(), &npm.display().to_string()],
                &project.dir,
                &[],
                left(ctx).min(Duration::from_secs(60)).max(Duration::from_secs(5)),
            )
            .await;
            if let Some(error) = &result.spawn_error {
                types::Verdict::Unknown(error.clone())
            } else if !result.ok() {
                types::Verdict::Unknown(format!(
                    "the comparison failed (exit {:?}{}): {}",
                    result.code,
                    if result.timed_out { ", timed out" } else { "" },
                    diag::tail_lines(result.combined().trim(), 6)
                ))
            } else {
                types::verdict_from_report(&result.stdout)
            }
        }
    };
    let synced = types::sync_override(&project.dir, &verdict, game_text.as_deref());
    let package_version = installed_version(project, "@s2ze/types");
    let mut report = json!({
        "verdict": verdict.state(),
        "npmTypes": package_version,
        "gameFile": game.display().to_string(),
        "overrideActive": types::override_active(&project.dir),
    });
    match &synced {
        Ok((action, rewired, unreadable)) => {
            report["override"] = json!(action.as_str());
            report["overrideFile"] = json!(types::OVERRIDE_REL);
            if !rewired.is_empty() {
                report["rewiredScripts"] = json!(rewired);
            }
            if !unreadable.is_empty() {
                report["unparsableTsconfigs"] = json!(unreadable);
            }
            report["overrideActive"] = json!(types::override_active(&project.dir));
        }
        Err(error) => report["overrideError"] = json!(error.to_string()),
    }
    match verdict {
        types::Verdict::Equal => {
            report["note"] = json!("the npm @s2ze/types declares the same API as the game's point_script.d.ts");
        }
        types::Verdict::Different(diff) => {
            report["differences"] = diff;
            report["warning"] = json!(format!(
                "the npm @s2ze/types {} does not declare the same API as the game's point_script.d.ts: the game is the truth, so the scripts type-check against {} (a copy of the game's file) until the package matches again (run update to re-check)",
                report["npmTypes"].as_str().unwrap_or("?"),
                types::OVERRIDE_REL
            ));
        }
        types::Verdict::Unknown(why) => report["reason"] = json!(why),
    }
    report
}

// -------------------------------------------------------------------------------------------
// init / update / add / write / read
// -------------------------------------------------------------------------------------------

async fn init(state: &HammerState, ctx: &CallCtx, args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    let scripts = scripts_arg(args)?;
    let wait = wait_arg(args)?;
    let joining = run::job_pending(&job_key(project));
    let mut created: Vec<String> = Vec::new();
    if !joining {
        created = cs_script::scaffold(project, &scripts).map_err(cs_error)?;
    }
    let Some(result) = npm_install(project, ctx, wait).await else {
        return Ok(still_running(project, "init", json!({"created": created, "scripts": scripts})));
    };
    let installed = result.ok();
    let types_report = if installed { types_step(state, ctx, project).await } else { Value::Null };
    let scripts_now = project.targets().unwrap_or_default();
    let mut out: ToolOutput = json!({
        "ok": installed,
        "done": true,
        "project": project.dir.display().to_string(),
        "outputDir": project.out_dir.display().to_string(),
        "created": created,
        "scripts": scripts_now,
        "install": run_report(&result),
        "versions": versions(project),
        "types": types_report,
        "upstream": format!("Source2ZE create-cs-script {} (MIT, LICENSE.create-cs-script)", cs_script::UPSTREAM_VERSION),
    })
    .into();
    let first = scripts_now.first().cloned().unwrap_or_else(|| "main".into());
    out = if installed {
        out.with_next_args(
            "cs_script",
            "write the script source (the template only logs a line), then build",
            json!({"action": "write", "name": first, "file": "index.ts", "content": "<the script's TypeScript>"}),
        )
    } else {
        out.with_next_args("cs_script", "the files are in place; retry the install (update runs it again)", json!({"action": "update"}))
    };
    Ok(out)
}

async fn update(state: &HammerState, ctx: &CallCtx, args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    let wait = wait_arg(args)?;
    if !run::job_pending(&job_key(project)) {
        project.require().map_err(cs_error)?;
    }
    let before = versions(project);
    let Some(result) = npm_install(project, ctx, wait).await else {
        return Ok(still_running(project, "update", json!({"before": before})));
    };
    let ok = result.ok();
    let types_report = if ok { types_step(state, ctx, project).await } else { Value::Null };
    let out: ToolOutput = json!({
        "ok": ok,
        "done": true,
        "install": run_report(&result),
        "before": before,
        "after": versions(project),
        "types": types_report,
    })
    .into();
    Ok(out.with_next_args("cs_script", "type-check and build against the updated packages", json!({"action": "build"})))
}

pub(super) fn add(args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    let name = str_arg(args, "name")?.ok_or_else(|| invalid("add needs name"))?;
    let written = cs_script::add_script(project, name).map_err(cs_error)?;
    let out: ToolOutput = json!({
        "ok": true,
        "name": name,
        "written": written,
        "scripts": project.targets().map_err(cs_error)?,
        "output": project.js_path(name).display().to_string(),
        "keyvalue": Project::vjs_keyvalue(name),
    })
    .into();
    Ok(out.with_next_args(
        "cs_script",
        "write the script source, then build",
        json!({"action": "write", "name": name, "file": "index.ts", "content": "<the script's TypeScript>"}),
    ))
}

pub(super) fn write(args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    project.require().map_err(cs_error)?;
    let name = str_arg(args, "name")?.ok_or_else(|| invalid("write needs name"))?;
    let file = str_arg(args, "file")?.unwrap_or("index.ts");
    let content = str_arg(args, "content")?.ok_or_else(|| invalid("write needs content (the whole file text)"))?;
    if content.len() > cs_script::SOURCE_MAX_BYTES {
        return Err(invalid(format!("content is {} bytes; the limit is {}", content.len(), cs_script::SOURCE_MAX_BYTES)));
    }
    project.require_script(name).map_err(cs_error)?;
    let path = project.source_path(name, file).map_err(cs_error)?;
    cs_script::inside_addon(&project.addon_root, &path).map_err(cs_error)?;
    let existed = path.is_file();
    cs_script::write_file(&path, content).map_err(cs_error)?;
    let out: ToolOutput = json!({
        "ok": true,
        "path": format!("cs_script/src/{name}/{file}"),
        "bytes": content.len(),
        "created": !existed,
        "files": cs_script::list_sources(project, name),
    })
    .into();
    Ok(out.with_next_args("cs_script", "type-check and build the script", json!({"action": "build", "name": name})))
}

fn read(args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    project.require().map_err(cs_error)?;
    let name = str_arg(args, "name")?.ok_or_else(|| invalid("read needs name"))?;
    project.require_script(name).map_err(cs_error)?;
    let files = cs_script::list_sources(project, name);
    let file = str_arg(args, "file")?.unwrap_or("index.ts");
    let path = project.source_path(name, file).map_err(cs_error)?;
    cs_script::inside_addon(&project.addon_root, &path).map_err(cs_error)?;
    let content = std::fs::read_to_string(&path).map_err(|e| {
        ToolError::new("FileNotFound", "FileNotFound", format!("cs_script/src/{name}/{file}: {e}"))
            .retry_safe(true)
            .with_data(json!({"files": files}))
    })?;
    if content.len() > cs_script::SOURCE_MAX_BYTES {
        return Err(invalid(format!("{file} is {} bytes, over the {} read limit", content.len(), cs_script::SOURCE_MAX_BYTES)));
    }
    let out: ToolOutput = json!({"path": format!("cs_script/src/{name}/{file}"), "content": content, "files": files}).into();
    Ok(out.no_next())
}

// -------------------------------------------------------------------------------------------
// build
// -------------------------------------------------------------------------------------------

fn tool_bin(project: &Project, name: &str) -> std::path::PathBuf {
    project.dir.join("node_modules/.bin").join(name)
}

fn names_arg(args: &Value, project: &Project) -> Result<Vec<String>, ToolError> {
    project.require().map_err(cs_error)?;
    match str_arg(args, "name")? {
        Some(name) => {
            project.require_script(name).map_err(cs_error)?;
            Ok(vec![name.to_string()])
        }
        None => project.targets().map_err(cs_error),
    }
}

/// What the console says about a script (re)loading: `Some(true)` loaded, `Some(false)` the engine
/// refused it, `None` nothing yet. `settled` (the watch is over) also accepts the engine's
/// `Reloading Resource` line for the script when the script itself logged nothing.
fn reload_sign(text: &str, names: &[String], settled: bool) -> Option<(bool, String)> {
    let vjs = |n: &String| format!("maps/scripts/{n}.vjs");
    for line in text.lines() {
        if line.contains("Failed to compile cs_script")
            || (line.contains("Failed to load cs_script") && names.iter().any(|n| line.contains(&vjs(n))))
        {
            return Some((false, line.trim().to_string()));
        }
    }
    let find = |hit: &dyn Fn(&str, &String) -> bool| {
        text.lines()
            .find(|line| names.iter().any(|n| hit(line, n)))
            .map(|line| (true, line.trim().to_string()))
    };
    find(&|line, n| line.contains(&format!("[{n}] loaded")))
        .or_else(|| settled.then(|| find(&|line, n| line.contains("Reloading Resource") && line.contains(&vjs(n)))).flatten())
}

async fn map_running(state: &HammerState, ctx: &CallCtx) -> Value {
    match super::game_live::game_rules(state.clone(), ctx.clone(), json!({})).await {
        Ok(_) => json!(true),
        Err(e) if e.name == "NoMapLoaded" => json!(false),
        Err(_) => json!("unknown"),
    }
}

pub(super) async fn build(state: &HammerState, ctx: &CallCtx, args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    let names = names_arg(args, project)?;
    let wait = wait_arg(args)?;
    // dependencies: install once when the project has none yet (a fresh checkout, a failed init)
    if !tool_bin(project, "rollup").exists() || !tool_bin(project, "tsc").exists() {
        match npm_install(project, ctx, wait).await {
            None => return Ok(still_running(project, "build", json!({"why": "node_modules is missing: installing"}))),
            Some(result) if !result.ok() => {
                let out: ToolOutput = json!({"ok": false, "done": true, "stage": "install", "install": run_report(&result)}).into();
                return Ok(out.with_next_args("cs_script", "the dependencies did not install; see install.tail", json!({"action": "update"})));
            }
            Some(_) => {}
        }
    }
    let mut watch = ConsoleWatch::open(console_log_path(state));
    let mut built: Vec<Value> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut failures: Vec<Value> = Vec::new();
    let mut changed_names: Vec<String> = Vec::new();
    let progs = programs();
    let node = progs.node.clone();
    for (i, name) in names.iter().enumerate() {
        if left(ctx) < Duration::from_secs(8) && i > 0 {
            pending = names[i..].to_vec();
            break;
        }
        let step_timeout = left(ctx).clamp(Duration::from_secs(5), Duration::from_secs(180));
        // 1. type-check: tsc says every error with its position
        let tsconfig = format!("src/{name}/tsconfig.json");
        let tsc = run::run(
            &node,
            &[&tool_bin(project, "tsc").display().to_string(), "--noEmit", "--pretty", "false", "-p", &tsconfig],
            &project.dir,
            &[],
            step_timeout,
        )
        .await;
        if let Some(error) = &tsc.spawn_error {
            return Err(ToolError::new("ToolchainUnavailable", "ToolchainUnavailable", error.clone()).retry_safe(true));
        }
        let diags = diag::parse_tsc(&tsc.combined());
        if tsc.timed_out || !tsc.ok() {
            let mut failure = json!({"name": name, "stage": "typecheck", "timedOut": tsc.timed_out});
            if diags.is_empty() {
                failure["tail"] = json!(diag::tail_lines(tsc.combined().trim(), 12));
            }
            failure.as_object_mut().expect("object").extend(diag::listing(&diags).as_object().expect("object").clone());
            failures.push(failure);
            continue;
        }
        // 2. rollup bundles it into maps/scripts/<name>.js
        let out_path = project.js_path(name);
        cs_script::inside_addon(&project.addon_root, &out_path).map_err(cs_error)?;
        let old = std::fs::read(&out_path).ok();
        let step_timeout = left(ctx).clamp(Duration::from_secs(5), Duration::from_secs(180));
        let rollup = run::run(
            &node,
            &[&tool_bin(project, "rollup").display().to_string(), "-c", "--silent"],
            &project.dir,
            &[("CS_SCRIPT_ONLY", name.as_str())],
            step_timeout,
        )
        .await;
        if !rollup.ok() {
            let diags = diag::parse_rollup(&rollup.combined());
            let mut failure = json!({"name": name, "stage": "bundle", "timedOut": rollup.timed_out});
            if diags.is_empty() {
                failure["tail"] = json!(diag::tail_lines(rollup.combined().trim(), 12));
            }
            failure.as_object_mut().expect("object").extend(diag::listing(&diags).as_object().expect("object").clone());
            failures.push(failure);
            continue;
        }
        let new = std::fs::read(&out_path).ok();
        let changed = old != new;
        if changed {
            changed_names.push(name.clone());
        }
        built.push(json!({
            "name": name,
            "output": out_path.display().to_string(),
            "outputRel": Project::js_rel(name),
            "bytes": new.as_ref().map(Vec::len),
            "changed": changed,
            "existed": old.is_some(),
        }));
    }
    let ok = failures.is_empty() && pending.is_empty();
    let mut json = json!({
        "ok": ok,
        "built": built,
        "failed": failures,
        "pending": pending,
        "keyvalue": names.iter().map(|n| Project::vjs_keyvalue(n)).collect::<Vec<_>>(),
    });
    // 3. hot reload: only meaningful when a script changed and a map runs in the tools session
    let mut reload = json!({"reloaded": "unknown", "mapRunning": "unknown"});
    if !changed_names.is_empty() {
        let running = map_running(state, ctx).await;
        reload["mapRunning"] = running.clone();
        if running == json!(true) {
            let deadline = Instant::now() + RELOAD_WATCH.min(left(ctx));
            let sign = loop {
                watch.poll();
                let settled = Instant::now() >= deadline;
                let found = reload_sign(watch.seen(), &changed_names, settled);
                if found.is_some() || settled {
                    break found;
                }
                tokio::time::sleep(RELOAD_POLL).await;
            };
            match sign {
                Some((loaded, line)) => {
                    reload["reloaded"] = json!(loaded);
                    reload["evidence"] = json!(line);
                }
                None => {
                    reload["note"] = json!(
                        "no reload line within the watch: the script may not have been reloaded, or the engine prints nothing for it (the template logs `[<name>] loaded` at the top level)"
                    );
                }
            }
        } else if running == json!(false) {
            reload["note"] = json!("no map is running in the tools session: the script loads with the map (run_map)");
        }
        reload["marker"] = json!("console.log: `[cs_script] [<name>] loaded` from the script's top-level Instance.Msg, or the engine's `Reloading Resource ...: maps/scripts/<name>.vjs`");
    }
    json["reload"] = reload;
    let mut out: ToolOutput = json.into();
    out = if !ok {
        out.with_next_args(
            "cs_script",
            "fix the first error (read the file, write it back whole) and build again",
            json!({"action": "read", "name": names.first().cloned().unwrap_or_default()}),
        )
    } else {
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| !vjs_c_path(state, &project.addon, n).exists())
            .collect();
        if !missing.is_empty() {
            out.json["vjs_cMissing"] = json!(missing);
        }
        let why = if missing.is_empty() {
            "refresh the .vjs_c (the map loads it; a running tools map recompiles it itself when the .js changes)"
        } else {
            "compile the .vjs_c BEFORE run_map: the engine loads maps/scripts/<name>.vjs_c, never the .js; a map loaded without it prints `Failed to load cs_script` and runs without the script"
        };
        out.with_next_args("cs_script", why, json!({"action": "compile"}))
    };
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// compile
// -------------------------------------------------------------------------------------------

pub(super) fn vjs_c_path(state: &HammerState, addon: &str, name: &str) -> std::path::PathBuf {
    state.app_root().join("game/csgo_addons").join(addon).join("maps/scripts").join(format!("{name}.vjs_c"))
}

fn file_json(path: &Path) -> Value {
    match std::fs::metadata(path) {
        Ok(m) => json!({"path": path.display().to_string(), "exists": true, "bytes": m.len()}),
        Err(_) => json!({"path": path.display().to_string(), "exists": false}),
    }
}

pub(super) async fn compile(state: &HammerState, ctx: &CallCtx, args: &Value, project: &Project) -> Result<ToolOutput, ToolError> {
    let names = names_arg(args, project)?;
    let wait = wait_arg(args)?;
    let mut done: Vec<Value> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let js = project.js_path(name);
        if !js.is_file() {
            return Err(ToolError::new(
                "ScriptNotBuilt",
                "ScriptNotBuilt",
                format!("{} does not exist: build the script first", js.display()),
            )
            .retry_safe(true)
            .use_instead("cs_script", "build it", Some(json!({"action": "build", "name": name}))));
        }
        if left(ctx) < Duration::from_secs(5) && i > 0 {
            let out: ToolOutput = json!({"ok": false, "done": false, "compiled": done, "pending": names[i..].to_vec()}).into();
            return Ok(out.with_next_args("cs_script", "the call budget ended: compile the rest", json!({"action": "compile", "name": names[i]})));
        }
        let started = super::jobs::compile_asset(
            state.clone(),
            ctx.clone(),
            json!({"path": Project::js_rel(name), "flags": ["-nop4"]}),
        )
        .await?;
        let job_id = started.json.get("jobId").and_then(Value::as_str).unwrap_or_default().to_string();
        let polled = super::jobs::job_status(
            state.clone(),
            ctx.clone(),
            json!({"jobId": job_id, "waitMs": clamp(ctx, wait).as_millis() as u64}),
        )
        .await?;
        let finished = polled.json.get("done").and_then(Value::as_bool).unwrap_or(false);
        let job_state = polled.json.get("state").cloned().unwrap_or(Value::Null);
        if !finished {
            let out: ToolOutput = json!({
                "ok": false,
                "done": false,
                "state": "running",
                "script": name,
                "jobId": job_id,
                "compiled": done,
                "pending": names[i + 1..].to_vec(),
            })
            .into();
            return Ok(out.with_next_args(
                "job_status",
                "the compile is still running: wait for it, then compile the rest",
                json!({"jobId": job_id, "waitMs": 30_000}),
            ));
        }
        let ok = job_state == json!("ok");
        let vjs_c = vjs_c_path(state, &project.addon, name);
        let mut entry = json!({
            "name": name,
            "state": job_state,
            "ok": ok,
            "vjs_c": file_json(&vjs_c),
            "summary": polled.json.get("summary").cloned().unwrap_or(Value::Null),
        });
        if !ok {
            for key in ["failLines", "logTail"] {
                if let Some(v) = polled.json.get(key) {
                    entry[key] = v.clone();
                }
            }
        }
        done.push(entry);
        if !ok {
            let out: ToolOutput = json!({"ok": false, "done": true, "compiled": done}).into();
            return Ok(out.no_next());
        }
    }
    let out: ToolOutput = json!({
        "ok": true,
        "done": true,
        "compiled": done,
        "note": "the .vjs_c ships beside the vpk (report_ready lists it under loose_compiled); the map references maps/scripts/<name>.vjs",
    })
    .into();
    Ok(out.with_next_args("report_ready", "check the release", json!({"profile": "release"})))
}

// -------------------------------------------------------------------------------------------
// status
// -------------------------------------------------------------------------------------------

pub(super) fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub(super) fn newest_source(project: &Project, name: &str) -> Option<std::time::SystemTime> {
    cs_script::list_sources(project, name)
        .iter()
        .filter_map(|f| mtime(&project.src_dir(name).join(f)))
        .max()
}

/// Latest `@s2ze/types` on npm, cached for ten minutes; `None` offline.
async fn latest_on_npm(project: &Project) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((at, version)) = cache.lock().ok()?.as_ref() {
        if at.elapsed() < Duration::from_secs(600) {
            return Some(version.clone());
        }
    }
    let progs = programs();
    let result = run::run(
        &progs.npm,
        &["view", "@s2ze/types", "version", "--fetch-retries=0", "--fetch-timeout=6000"],
        &project.dir,
        &[],
        Duration::from_secs(8),
    )
    .await;
    let version = result.stdout.trim().to_string();
    if result.ok() && !version.is_empty() && version.len() < 40 {
        *cache.lock().ok()? = Some((Instant::now(), version.clone()));
        Some(version)
    } else {
        None
    }
}

async fn status(state: &HammerState, ctx: &CallCtx, project: &Project) -> Result<ToolOutput, ToolError> {
    if !project.exists() {
        let out: ToolOutput = json!({
            "project": {"present": false, "dir": project.dir.display().to_string()},
            "outputDir": project.out_dir.display().to_string(),
        })
        .into();
        return Ok(out.with_next_args("cs_script", "scaffold the project", json!({"action": "init"})));
    }
    let names = project.targets().map_err(cs_error)?;
    // point_script entities of the open map, by script
    let document = super::entities_file::resolve_document(state, ctx, &json!({})).await.ok();
    let mut by_script: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    let mut map_name = Value::Null;
    if let Some((host, document, _, _)) = &document {
        map_name = json!(host.file_name().map(|n| n.to_string_lossy().into_owned()));
        for entity in document.find(Some("point_script"), None, None, 200) {
            let value = entity
                .keyvalues
                .iter()
                .find(|(k, _)| k == "cs_script")
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_string();
            let stem = value.rsplit('/').next().unwrap_or("").split('.').next().unwrap_or("").to_string();
            by_script.entry(stem).or_default().push(json!({
                "nodeID": entity.node_id,
                "targetname": entity.targetname,
                "cs_script": value,
            }));
        }
    }
    let scripts: Vec<Value> = names
        .iter()
        .map(|name| {
            let js = project.js_path(name);
            let vjs_c = vjs_c_path(state, &project.addon, name);
            let src = newest_source(project, name);
            let built = match (mtime(&js), src) {
                (None, _) => "missing",
                (Some(j), Some(s)) if s > j => "stale",
                _ => "fresh",
            };
            let compiled = match (mtime(&vjs_c), mtime(&js)) {
                (None, _) => "missing",
                (Some(c), Some(j)) if j > c => "stale",
                _ => "fresh",
            };
            json!({
                "name": name,
                "sources": cs_script::list_sources(project, name),
                "built": built,
                "compiled": compiled,
                "js": file_json(&js),
                "vjs_c": file_json(&vjs_c),
                "keyvalue": Project::vjs_keyvalue(name),
                "pointScripts": by_script.get(name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    let latest = latest_on_npm(project).await;
    let installed = installed_version(project, "@s2ze/types");
    let game = types::game_file(&state.app_root());
    let npm = types::npm_file(&project.dir);
    let drift = match (std::fs::read_to_string(&game), std::fs::read_to_string(&npm)) {
        (Ok(g), Ok(n)) if types::normalise(&g) == types::normalise(&n) => json!("equal"),
        (Ok(_), Ok(_)) => {
            let progs = programs();
            let result = run::run(
                &progs.node,
                &["--input-type=module", "-e", cs_script::FINGERPRINT_JS, &game.display().to_string(), &npm.display().to_string()],
                &project.dir,
                &[],
                left(ctx).clamp(Duration::from_secs(5), Duration::from_secs(60)),
            )
            .await;
            if result.ok() {
                json!(types::verdict_from_report(&result.stdout).state())
            } else {
                json!("unknown")
            }
        }
        _ => json!("unknown"),
    };
    let stray: Vec<String> = by_script.keys().filter(|k| !k.is_empty() && !names.contains(k)).cloned().collect();
    let out: ToolOutput = json!({
        "project": {
            "present": true,
            "dir": project.dir.display().to_string(),
            "installed": project.dir.join("node_modules").is_dir(),
        },
        "outputDir": project.out_dir.display().to_string(),
        "scripts": scripts,
        "packages": {
            "installed": versions(project),
            "latestTypes": latest.clone().map(Value::from).unwrap_or_else(|| json!("unknown")),
            "outdated": match (&installed, &latest) {
                (Some(a), Some(b)) => json!(a != b),
                _ => json!("unknown"),
            },
        },
        "types": {
            "npmVsGame": drift,
            "overrideActive": types::override_active(&project.dir),
            "gameFile": game.display().to_string(),
        },
        "map": {"document": map_name, "pointScriptsReferencingNoScript": stray},
    })
    .into();
    Ok(out.no_next())
}

#[cfg(test)]
mod tests {
    use super::reload_sign;

    // console lines as the tools engine printed them live (2026-10-28, accept_p4)
    const RELOAD: &str = "10/28 13:41:26 [AssetSystem] Reloading Resource [1/1]: maps/scripts/main.vjs\n\
        10/28 13:41:26 [AssetSystem] On-demand recompile of asset maps/scripts/main.vjs\n";

    #[test]
    fn reload_sign_reads_the_live_console_lines() {
        let names = vec!["main".to_string()];
        let loaded = format!("{RELOAD}10/28 13:41:26 [cs_script] [main] loaded v2\n");
        assert_eq!(reload_sign(&loaded, &names, false).unwrap(), (true, "10/28 13:41:26 [cs_script] [main] loaded v2".into()));
        // the engine's own reload line counts only once the watch is over
        assert!(reload_sign(RELOAD, &names, false).is_none());
        assert!(reload_sign(RELOAD, &names, true).unwrap().0);
        let failed = "10/28 13:40:39 [cs_script] Failed to load cs_script \"maps/scripts/main.vjs\"\n";
        assert!(!reload_sign(failed, &names, false).unwrap().0);
        // another script's failure is not this one's
        assert!(reload_sign(failed, &["other".to_string()], true).is_none());
    }
}
