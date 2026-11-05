//! CS script (`point_script`, `cs_script`) and VScript test runs (workflows/06-cs-script-vscript.md).
//!
//! What is here: argument parsing for `script_run`, the judge (ordered `expect` matches and
//! script-error lines), the line filter and ordered matcher the runner waits with, and the
//! staging-name and command builders. The runner itself is in `tools::script`.
//!
//! `kind: "cs_script"` runs the scripts of an EXISTING compiled addon map (`addon` + `map`): the
//! engine logs `Instance.Msg` output as `[cs_script] <text>` lines. Inline `source`/`path` and
//! `vscript` are not run: [`blocker`] names what is missing, and `script_run` returns
//! `NotImplementedYet` before anything is staged, launched or sent.
//!
//! The error lines in [`ERROR_PATTERNS`] are the strings the server binary logs for script failures
//! (idabins `docs/interfaces/CCSScriptSystem.md`, "Loading a script" and "Running an entity's
//! script"). They are matched as substrings of console lines; the console line prefix is not known,
//! so the patterns do not anchor at line start.
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::sync::LazyLock;

/// Upper bound on the number of `expect` patterns in one call.
pub const MAX_EXPECTS: usize = 16;
/// Upper bound on one `expect` pattern, in bytes.
pub const MAX_EXPECT_BYTES: usize = 512;
/// Upper bound on an inline `source`, in bytes.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Lines of the log tail kept in the judge's `logExcerpt` (error and matched lines are always kept).
pub const EXCERPT_TAIL_LINES: usize = 60;
/// Upper bound on the excerpt, in bytes.
pub const EXCERPT_MAX_BYTES: usize = 16 * 1024;
/// Prefix of the console lines the engine writes for `Instance.Msg` output of a cs_script (live
/// 2026-10-12: `10/12 08:23:56 [cs_script] Hello cs_script!`, stamp included).
pub const MSG_LINE_PREFIX: &str = "[cs_script] ";
/// `timeoutMs` default for a cs_script run: how long the expects may take to match.
pub const DEFAULT_RUN_TIMEOUT_MS: u64 = 60_000;

/// Server-side script error lines (substring regexes). Sources: CCSBaseScript::LoadModule
/// (`Exception during CompileModule` / `InstantiateModule` / `Evaluate`, `Failed to compile
/// cs_script[%d]`, `Invalid script. No valid imports found.`), CCSBaseScript::CallFunction
/// (`Uncaught Exception during callback "%s"`), CCSPointScriptEntity::Spawn_LoadScript
/// (`Failed to load cs_script "%s"`), ResolveModuleCallback (`Invalid module specifier`), and the
/// argument checks (`Method %s.%s invoked with bad %s value`, `requires %d argument(s)`).
pub const ERROR_PATTERNS: &[&str] = &[
    r#"Uncaught Exception during callback ""#,
    r"Exception during (CompileModule|InstantiateModule|Evaluate)",
    r"Failed to compile cs_script\[",
    r#"Failed to load cs_script ""#,
    r"Invalid script\. No valid imports found\.",
    r#"Invalid module specifier ""#,
    r"invoked with bad .* value\. \(parameter #\d+\)",
    r"requires \d+ argument\(s\)",
];

static ERROR_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    ERROR_PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("error pattern compiles"))
        .collect()
});

/// The two script runtimes a test can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    /// `point_script` JavaScript/TypeScript (V8, server `CCSScriptSystem`).
    CsScript,
    /// VScript (`vscript.dll`); on retail CS2 the VM is `CNopVM`.
    VScript,
}

impl ScriptKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cs_script" => Some(ScriptKind::CsScript),
            "vscript" => Some(ScriptKind::VScript),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScriptKind::CsScript => "cs_script",
            ScriptKind::VScript => "vscript",
        }
    }

    /// File extension of a script staged for this kind (the source form, not the compiled one).
    pub fn extension(self) -> &'static str {
        match self {
            ScriptKind::CsScript => "js",
            // CLuaVM is LuaJIT (IScriptVM.md); the Squirrel string count in vscript.dll is 0.
            ScriptKind::VScript => "lua",
        }
    }
}

/// What fires after the map loads: an entity input (`ent_fire <target> RunScriptInput <input>`,
/// `input` being the script function the point_script runs), or a plain console command (a cheat
/// a script registered, e.g. `script_zoo_trace`, whose output is `[cs_script] ` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryFire {
    Input { target: String, input: String },
    Command(String),
}

impl EntryFire {
    /// The console command that fires the trigger.
    pub fn command(&self) -> String {
        match self {
            EntryFire::Input { target, input } => {
                format!("ent_fire {target} RunScriptInput {input}")
            }
            EntryFire::Command(command) => command.clone(),
        }
    }
}

/// Validated `script_run` arguments.
#[derive(Debug, Clone)]
pub struct RunArgs {
    pub kind: ScriptKind,
    /// Inline script text (exclusive with `path`).
    pub source: Option<String>,
    /// Host path of a script file (exclusive with `source`).
    pub path: Option<String>,
    /// The compiled addon whose scripts run (cs_script; required unless `source`/`path` is given).
    pub addon: Option<String>,
    /// The map to load. Default: the addon's name.
    pub map: Option<String>,
    pub entry: Option<EntryFire>,
    /// Match the expects against every console line, not only `[cs_script] ` lines.
    pub any_line: bool,
    pub timeout_ms: Option<u64>,
    pub expect: Vec<String>,
    pub expects: Vec<Regex>,
}

impl RunArgs {
    /// The map the run loads: `map`, else the addon's name.
    pub fn map_name(&self) -> Option<String> {
        self.map.clone().or_else(|| self.addon.clone())
    }
}

/// Why a `script_run` argument is refused. Maps to `InvalidArguments` in the tool.
pub type ArgError = String;

impl RunArgs {
    pub fn parse(args: &Value) -> Result<RunArgs, ArgError> {
        let kind = match args.get("kind").and_then(Value::as_str) {
            Some(k) => ScriptKind::parse(k)
                .ok_or_else(|| "kind must be \"cs_script\" or \"vscript\"".to_string())?,
            None => return Err("kind must be \"cs_script\" or \"vscript\"".into()),
        };
        let source = opt_str(args, "source")?;
        let path = opt_str(args, "path")?;
        if source.is_some() && path.is_some() {
            return Err("pass source or path, not both".into());
        }
        // a cs_script run takes neither (it runs an addon's compiled scripts); vscript needs one
        if kind == ScriptKind::VScript && source.is_none() && path.is_none() {
            return Err("pass source (inline) or path".into());
        }
        if let Some(s) = &source {
            if s.trim().is_empty() {
                return Err("source must not be empty".into());
            }
            if s.len() > MAX_SOURCE_BYTES {
                return Err(format!("source is over {MAX_SOURCE_BYTES} bytes"));
            }
        }
        if let Some(p) = &path {
            if p.is_empty() || p.len() > 4096 || p.contains('\0') {
                return Err("path must be a non-empty host path under 4096 bytes".into());
            }
        }
        let map = match args.get("map") {
            None | Some(Value::Null) => None,
            Some(Value::String(m)) if crate::tools::multiplayer::valid_map(m) => Some(m.clone()),
            Some(_) => return Err("map must be [A-Za-z0-9_]{1,64}".into()),
        };
        let addon = match args.get("addon") {
            None | Some(Value::Null) => None,
            Some(Value::String(a)) if crate::testscript::valid_addon(a) => Some(a.clone()),
            Some(_) => return Err("addon must be [A-Za-z0-9_]{1,64}".into()),
        };
        let entry = parse_entry(args)?;
        let any_line = match args.get("anyLine") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("anyLine must be a boolean".into()),
        };
        if kind == ScriptKind::VScript && (addon.is_some() || entry.is_some() || any_line) {
            return Err("addon, entry and anyLine are cs_script only".into());
        }
        // a cs_script run needs its compiled addon; inline source is refused by the blocker instead
        let inline = source.is_some() || path.is_some();
        if kind == ScriptKind::CsScript && !inline && addon.is_none() {
            return Err(
                "addon is required: the name of a compiled addon map whose scripts run".into(),
            );
        }
        let timeout_ms = match args.get("timeoutMs") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .filter(|n| (1..=crate::tools::runtime_tools::MAX_TIMEOUT_MS).contains(n))
                    .ok_or_else(|| "timeoutMs must be an integer in 1..=900000".to_string())?,
            ),
        };
        let expect: Vec<String> = match args.get("expect") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| {
                    i.as_str()
                        .map(String::from)
                        .ok_or_else(|| "expect must be an array of regex strings".to_string())
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("expect must be an array of regex strings".into()),
        };
        // Evidence first (convention 9): a run with nothing to match would pass on silence.
        if expect.is_empty() {
            return Err("expect needs at least one regex: a run must match output to pass".into());
        }
        if expect.len() > MAX_EXPECTS {
            return Err(format!("expect takes at most {MAX_EXPECTS} patterns"));
        }
        let mut expects = Vec::with_capacity(expect.len());
        for e in &expect {
            if e.is_empty() || e.len() > MAX_EXPECT_BYTES {
                return Err(format!("each expect must be 1..={MAX_EXPECT_BYTES} bytes"));
            }
            let re = RegexBuilder::new(e)
                .size_limit(1 << 20)
                .build()
                .map_err(|err| format!("expect {e:?} is not a valid regex: {err}"))?;
            expects.push(re);
        }
        Ok(RunArgs {
            kind,
            source,
            path,
            addon,
            map,
            entry,
            any_line,
            timeout_ms,
            expect,
            expects,
        })
    }
}

/// `entry`: `{target, input}` (an entity input) or `{command}` (a console command). Each shape is
/// checked, so the fired command has no other form. The command charset has no `;` or newline.
fn parse_entry(args: &Value) -> Result<Option<EntryFire>, ArgError> {
    let bad = || {
        "entry must be {target: [A-Za-z0-9_]{1,64}, input: identifier} or {command: a console command}"
            .to_string()
    };
    let entry = match args.get("entry") {
        None | Some(Value::Null) => return Ok(None),
        Some(e @ Value::Object(_)) => e,
        Some(_) => return Err(bad()),
    };
    if let Some(command) = entry.get("command") {
        if entry.get("target").is_some() || entry.get("input").is_some() {
            return Err(bad());
        }
        let command = command
            .as_str()
            .map(str::trim)
            .filter(|c| {
                !c.is_empty()
                    && c.len() <= 128
                    && c.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ' ' | '.' | '-'))
            })
            .ok_or_else(|| "entry command must be 1..=128 bytes of [A-Za-z0-9_ .-]".to_string())?;
        // the console refusal list (quit, exit, disconnect, ...) applies to entries too: refused
        // here, before any launch
        if let Some(reason) = cs2_mcp_common::console::refusal(command) {
            return Err(format!("entry command {command:?} is refused: {reason}"));
        }
        return Ok(Some(EntryFire::Command(command.to_string())));
    }
    let target = entry
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(bad)?;
    let input = entry.get("input").and_then(Value::as_str).ok_or_else(bad)?;
    if !crate::testscript::valid_addon(target) || !valid_identifier(input) {
        return Err(bad());
    }
    Ok(Some(EntryFire::Input {
        target: target.to_string(),
        input: input.to_string(),
    }))
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, ArgError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Result of [`judge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Every `expect` matched, in order, and no script-error line was seen.
    pub passed: bool,
    /// The line that satisfied each `expect` that matched, in `expect` order.
    pub matched: Vec<String>,
    /// The index into `expects` of each entry of `matched` (same length, same order).
    pub matched_expects: Vec<usize>,
    /// The `expect` patterns that did not match (in order). Empty when all matched.
    pub unmatched: Vec<String>,
    /// Script-error lines, in log order.
    pub errors: Vec<String>,
    pub log_excerpt: String,
}

/// Judge a run's output. `log` must hold only the lines written by this run (the caller takes the
/// cursor before execution). Each `expect` must match a line strictly after the line matched by
/// the previous one; the first matching line after the cursor is taken (greedy, in order).
pub fn judge(log: &str, expects: &[Regex], expect_src: &[String]) -> Verdict {
    let lines: Vec<&str> = log.lines().collect();
    let mut matched = Vec::new();
    let mut matched_expects = Vec::new();
    let mut unmatched = Vec::new();
    let mut cursor = 0usize;
    let mut matched_idx = Vec::new();
    for (k, re) in expects.iter().enumerate() {
        let found = (cursor..lines.len()).find(|&i| re.is_match(lines[i]));
        match found {
            Some(i) => {
                matched.push(lines[i].to_string());
                matched_expects.push(k);
                matched_idx.push(i);
                cursor = i + 1;
            }
            None => {
                // The cursor does not move, so later expects are still checked and the report
                // names every one that failed.
                unmatched.push(expect_src.get(k).cloned().unwrap_or_default());
            }
        }
    }
    let error_idx: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| ERROR_RES.iter().any(|r| r.is_match(l)))
        .map(|(i, _)| i)
        .collect();
    let errors: Vec<String> = error_idx.iter().map(|&i| lines[i].to_string()).collect();
    let passed = unmatched.is_empty() && errors.is_empty();
    let log_excerpt = excerpt(&lines, &matched_idx, &error_idx);
    Verdict {
        passed,
        matched,
        matched_expects,
        unmatched,
        errors,
        log_excerpt,
    }
}

/// The prefix of the marker command the runner queues after the entry (`exec_console_confirmed`);
/// its `Unknown command:` line is the runner's own, never a script's output.
pub const CONFIRM_MARKER_PREFIX: &str = "cs2mcp_mark_";
/// `scriptLines` keeps the newest this many lines (`scriptLinesTruncated` counts the rest) ...
pub const SCRIPT_LINES_MAX: usize = 200;
/// ... and at most this many bytes of them.
pub const SCRIPT_LINES_MAX_BYTES: usize = 32 * 1024;

/// The `MM/DD HH:MM:SS ` stamp `-condebug` writes in front of every console.log line.
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d\d/\d\d \d\d:\d\d:\d\d ").expect("stamp pattern compiles"));

/// The text of a console line after its timestamp (the line itself when it has none). The
/// `[Channel] ` tag, when there is one, stays in the text: `[cs_script] ` is the channel of the
/// script output.
pub fn message(line: &str) -> &str {
    match STAMP.find(line) {
        Some(m) => &line[m.end()..],
        None => line,
    }
}

/// Is this console line one a run looks at? `[cs_script] ` lines (after the timestamp), script-error
/// lines (the engine may log those without the prefix), and every line when `any_line` is set. The
/// confirmation marker's line is never a run line. An expect matches the whole raw line, timestamp
/// included, so a pattern that is not anchored at `^` matches wherever it occurs.
pub fn is_run_line(line: &str, any_line: bool) -> bool {
    if line.contains(CONFIRM_MARKER_PREFIX) {
        return false;
    }
    any_line
        || message(line).starts_with(MSG_LINE_PREFIX)
        || ERROR_RES.iter().any(|r| r.is_match(line))
}

/// The `[cs_script] ` lines of a log segment, in order, as the engine wrote them (timestamp kept).
pub fn script_lines<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .copied()
        .filter(|l| message(l).starts_with(MSG_LINE_PREFIX))
        .collect()
}

/// The newest `[cs_script] ` lines within [`SCRIPT_LINES_MAX`] lines and [`SCRIPT_LINES_MAX_BYTES`]
/// bytes, oldest first, and how many older ones were dropped.
pub fn cap_script_lines(lines: &[&str]) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if kept.len() >= SCRIPT_LINES_MAX || bytes + line.len() > SCRIPT_LINES_MAX_BYTES {
            break;
        }
        bytes += line.len();
        kept.push(line.to_string());
    }
    kept.reverse();
    let dropped = lines.len() - kept.len();
    (kept, dropped)
}

/// The predicate the runner waits with (`ConsoleLog::wait_for_by`): called once per complete line,
/// in order. It advances through `expects` on each run line that matches the next one, and returns
/// true once the last one has matched. Greedy like [`judge`], so the wait and the verdict agree on
/// where the expects land.
pub fn ordered_matcher(expects: &[Regex], any_line: bool) -> impl Fn(&str) -> bool + Sync + '_ {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    move |line: &str| {
        let k = next.load(Ordering::Relaxed);
        if k < expects.len() && is_run_line(line, any_line) && expects[k].is_match(line) {
            next.store(k + 1, Ordering::Relaxed);
        }
        next.load(Ordering::Relaxed) == expects.len()
    }
}

/// The lines that matter (matches, errors) plus the log tail, capped in lines and bytes.
fn excerpt(lines: &[&str], matched_idx: &[usize], error_idx: &[usize]) -> String {
    let tail_start = lines.len().saturating_sub(EXCERPT_TAIL_LINES);
    let mut keep: Vec<usize> = (tail_start..lines.len())
        .chain(matched_idx.iter().copied())
        .chain(error_idx.iter().copied())
        .collect();
    keep.sort_unstable();
    keep.dedup();
    let mut out = String::new();
    for i in keep {
        out.push_str(lines[i]);
        out.push('\n');
    }
    if out.len() > EXCERPT_MAX_BYTES {
        let mut cut = out.len() - EXCERPT_MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out.drain(..cut);
    }
    out
}

/// Why a kind cannot run today, with the evidence and the unblock route.
#[derive(Debug, Clone, Copy)]
pub struct Blocker {
    pub missing: &'static str,
    pub evidence: &'static [&'static str],
    pub unblock: &'static str,
}

pub const BLOCKED_ON: &str = "workflows/06-cs-script-vscript.md";

pub fn blocker(kind: ScriptKind) -> Blocker {
    match kind {
        ScriptKind::CsScript => Blocker {
            missing: "compiling JS needs resourcecompiler (not wired): a source file cannot become a .vjs_c \
                      here. Only scripts already compiled into an addon map run: script_run {kind:\"cs_script\", \
                      addon, map?, entry?, expect}",
            evidence: &[
                "live 2026-10-12: launch_session {kind:\"wine-tools\", addon:\"cs_script_demo\", map:\"cs_script_demo\"} loads via map_workshop cs_script_demo cs_script_demo (mapVerified true); console.log then holds `10/12 08:23:56 [cs_script] Hello cs_script!`",
                "live 2026-10-12: compiled scripts are game/csgo_addons/cs_script_demo/maps/scripts/*.vjs_c (hello, setup, input, trace, mdlchange, grenadetraining, chess); sources in content/csgo_addons/cs_script_demo/maps/scripts/*.js with point_script.d.ts",
                "live 2026-10-12: no resourcecompiler run has been made in this image, so no inline JS has been compiled",
                "idabins docs/interfaces/CCSScriptSystem.md:175-183 (Spawn_LoadScript reads the cs_script keyvalue and requires resource type vjs)",
            ],
            unblock: "wire resourcecompiler (wine prefix, flags not yet probed) to compile a source under content/csgo_addons/<addon>/maps/scripts/ into game/csgo_addons/<addon>/maps/scripts/*.vjs_c, with a point_script in that addon's map that names it",
        },
        ScriptKind::VScript => Blocker {
            missing: "a VScript VM that executes. CS2 creates CNopVM for the game's VM, so every \
                      script call is a stub and nothing runs. The script command `script` does not exist \
                      in CS2 (no such name in the command dump). script_reload_code is a no-op on retail",
            evidence: &[
                "idabins docs/interfaces/IScriptManager.md:19-22 (CCSGOVScriptGameSystem asks for language 2, CNopVM)",
                "idabins docs/interfaces/IScriptManager.md:127-145 (script_reload_code: every step is a CNopVM stub, nothing executes)",
                "idabins worknotes/ifacesurvey/B21-scratch.md:264 (script_reload_code is a no-op on retail)",
                "GameTracking-CS2/DumpSource2/commands.txt:2937-2942 (script_reload, script_reload_code; no bare `script` or `script_execute` entry)",
            ],
            unblock: "a build or patch where the server selects CLuaVM (CCSGOVScriptGameSystem vfunc[65] returns 1), or a bridge call to IScriptManager::CreateVM(1) with CompileScript/Run (IScriptVM.md slots 13-15); neither has been tried",
        },
    }
}

/// The unique token for one staged file: process id, time and a per-process counter, hex.
pub fn unique_token(pid: u32, nanos: u128, seq: u64) -> String {
    format!("{pid:x}-{:x}-{seq:x}", nanos & 0xffff_ffff_ffff)
}

/// Where a script is staged, relative to the game's `csgo/` dir. For VScript this is under
/// `scripts/vscripts/` (the root `script_reload_code` reads from, IScriptManager.md:136-138).
/// The tail is always `cs2mcp/<kind>-<token>.<ext>`.
pub fn staged_rel_path(kind: ScriptKind, token: &str) -> Result<String, ArgError> {
    if !valid_token(token) {
        return Err(format!("unsafe staging token {token:?}"));
    }
    let root = match kind {
        // GUESS: no reference shows where a point_script's loose .vjs_c would be read from. The
        // shipped form is maps/scripts/*.vjs_c inside the VPKs (pak01_dir.vpk tree).
        ScriptKind::CsScript => "maps/scripts",
        ScriptKind::VScript => "scripts/vscripts",
    };
    Ok(format!(
        "{root}/cs2mcp/{}-{token}.{}",
        kind.as_str(),
        kind.extension()
    ))
}

/// Tokens are `[0-9a-f-]{1,40}`: nothing that could escape the staging dir.
fn valid_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 40 && t.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// The console command that would run a staged VScript file: `script_reload_code <file>` with the
/// path relative to `scripts/vscripts/`. Refuses anything but a plain relative path. Not sent
/// anywhere today (see [`blocker`]).
pub fn vscript_command(rel_under_vscripts: &str) -> Result<String, ArgError> {
    let ok = !rel_under_vscripts.is_empty()
        && !rel_under_vscripts.starts_with('/')
        && !rel_under_vscripts
            .split('/')
            .any(|seg| seg == ".." || seg.is_empty())
        && rel_under_vscripts
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'));
    if !ok {
        return Err(format!("unsafe script path {rel_under_vscripts:?}"));
    }
    Ok(format!("script_reload_code {rel_under_vscripts}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Result<RunArgs, ArgError> {
        RunArgs::parse(&v)
    }

    // Synthetic fixtures: the line shapes come from the binary's log strings (see ERROR_PATTERNS),
    // not from captured console output. The prefixes are invented.
    const PASS_LOG: &str = "\
10/12 08:23:56 [cs_script] Loading script \"maps/scripts/cs2mcp_probe.vjs\"
10/12 08:23:56 [cs_script] Msg: SCRIPT_TEST_BEGIN smoke
10/12 08:23:56 [cs_script] Msg: SCRIPT_TEST_PASS smoke
";

    const ERROR_LOG: &str = "\
10/12 08:23:56 [cs_script] Msg: SCRIPT_TEST_BEGIN smoke
10/12 08:23:56 [cs_script] Uncaught Exception during callback \"OnRoundStart\"
TypeError: x is undefined
    at OnRoundStart (cs2mcp_probe.vjs:4:3)
";

    #[test]
    fn kinds_parse_and_nothing_else_does() {
        assert_eq!(ScriptKind::parse("cs_script"), Some(ScriptKind::CsScript));
        assert_eq!(ScriptKind::parse("vscript"), Some(ScriptKind::VScript));
        assert_eq!(ScriptKind::parse("lua"), None);
        assert_eq!(ScriptKind::parse("CS_SCRIPT"), None);
    }

    #[test]
    fn run_args_need_exactly_one_source_and_an_expect() {
        let base = json!({"kind": "cs_script", "expect": ["SCRIPT_TEST_PASS"]});
        let mut both = base.clone();
        both["source"] = json!("Instance.Msg('x')");
        both["path"] = json!("/tmp/x.js");
        assert!(parse(both).unwrap_err().contains("not both"));
        assert!(parse(base.clone())
            .unwrap_err()
            .contains("addon is required"));
        let mut vs = base.clone();
        vs["kind"] = json!("vscript");
        assert!(parse(vs).unwrap_err().contains("source (inline) or path"));
        let mut inline = base.clone();
        inline["source"] = json!("Instance.Msg('x')");
        let ok = parse(inline).unwrap();
        assert_eq!(ok.map, None);
        assert_eq!(ok.expects.len(), 1);
        let mut none = base;
        none["expect"] = json!([]);
        none["path"] = json!("/x.js");
        assert!(parse(none).unwrap_err().contains("at least one regex"));
    }

    #[test]
    fn run_args_bound_the_fields() {
        let good = |extra: Value| {
            let mut v = json!({"kind": "vscript", "source": "print(1)", "expect": ["1"]});
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            v
        };
        assert!(parse(good(json!({"kind": "lua"}))).is_err());
        assert!(parse(good(json!({"map": "../x"}))).is_err());
        assert!(parse(good(json!({"entry": "main_fn"}))).is_err());
        assert!(parse(good(json!({"timeoutMs": 0}))).is_err());
        assert!(parse(good(json!({"timeoutMs": 900_001}))).is_err());
        assert!(parse(good(json!({"expect": ["("]}))).is_err());
        assert!(parse(good(
            json!({"expect": (0..17).map(|_| "a").collect::<Vec<_>>()})
        ))
        .is_err());
        assert!(parse(good(json!({"source": "   "}))).is_err());
        assert!(parse(good(json!({"addon": "cs_script_demo"}))).is_err());
        assert!(parse(good(json!({"anyLine": true}))).is_err());
    }

    fn run(extra: Value) -> Value {
        let mut v = json!({"kind": "cs_script", "addon": "cs_script_demo", "expect": ["Hello"]});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn cs_script_run_needs_a_valid_addon_and_defaults_the_map_to_it() {
        let ok = parse(run(json!({}))).unwrap();
        assert_eq!(ok.addon.as_deref(), Some("cs_script_demo"));
        assert_eq!(ok.map_name().as_deref(), Some("cs_script_demo"));
        assert!(!ok.any_line && ok.entry.is_none() && ok.timeout_ms.is_none());
        let other = parse(run(json!({"map": "de_dust2"}))).unwrap();
        assert_eq!(other.map_name().as_deref(), Some("de_dust2"));
        let no_addon = json!({"kind": "cs_script", "expect": ["Hello"]});
        assert!(parse(no_addon).unwrap_err().contains("addon is required"));
        assert!(parse(run(json!({"addon": "bad-name"}))).is_err());
        assert!(parse(run(json!({"addon": 3}))).is_err());
        assert!(parse(run(json!({"map": "../x"}))).is_err());
    }

    #[test]
    fn inline_source_for_cs_script_parses_without_an_addon_so_the_blocker_can_answer() {
        let v = json!({"kind": "cs_script", "source": "Instance.Msg('x')", "expect": ["x"]});
        let r = parse(v).unwrap();
        assert!(r.addon.is_none() && r.source.is_some());
    }

    #[test]
    fn entry_is_an_ent_fire_trigger_with_checked_names() {
        let r = parse(run(
            json!({"entry": {"target": "hello", "input": "ScaleUp"}}),
        ))
        .unwrap();
        let e = r.entry.unwrap();
        assert_eq!(e.command(), "ent_fire hello RunScriptInput ScaleUp");
        let c = parse(run(
            json!({"entry": {"command": " script_zoo_trace line "}}),
        ))
        .unwrap();
        assert_eq!(c.entry.unwrap().command(), "script_zoo_trace line");
        for refused in ["quit", "exit", "disconnect"] {
            let r = parse(run(json!({"entry": {"command": refused}})));
            assert!(r.unwrap_err().contains("refused"), "{refused}");
        }
        let marker = "Unknown command: cs2mcp_mark_1a2b";
        assert!(
            !is_run_line(marker, true),
            "the marker line is never a run line"
        );
        let many: Vec<String> = (0..300)
            .map(|i| format!("10/12 08:23:56 [cs_script] line {i}"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let (kept, dropped) = cap_script_lines(&refs);
        assert_eq!((kept.len(), dropped), (SCRIPT_LINES_MAX, 100));
        assert_eq!(kept.last().unwrap(), "10/12 08:23:56 [cs_script] line 299");
        let big = "10/12 08:23:56 [cs_script] ".to_string() + &"x".repeat(1000);
        let bigs: Vec<&str> = std::iter::repeat_n(big.as_str(), 100).collect();
        let (kept, dropped) = cap_script_lines(&bigs);
        assert!(kept.len() * big.len() <= SCRIPT_LINES_MAX_BYTES && dropped > 0);
        for bad in [
            json!({"command": "x; quit"}),
            json!({"command": "a\nb"}),
            json!({"command": ""}),
            json!({"command": 3}),
            json!({"command": "x", "target": "hello"}),
            json!("hello"),
            json!({"target": "hello"}),
            json!({"target": "a b", "input": "ScaleUp"}),
            json!({"target": "hello", "input": "1bad"}),
            json!({"target": "hello", "input": "x;quit"}),
        ] {
            assert!(parse(run(json!({"entry": bad.clone()}))).is_err(), "{bad}");
        }
    }

    #[test]
    fn vscript_refuses_the_cs_script_only_fields() {
        let v = json!({"kind": "vscript", "source": "print(1)", "expect": ["1"], "addon": "a"});
        assert!(parse(v).unwrap_err().contains("cs_script only"));
    }

    #[test]
    fn ordered_matcher_finishes_only_when_every_expect_matched_in_order() {
        let re = |s: &str| Regex::new(s).unwrap();
        let expects = [re("Hello"), re("Bye")];
        let m = ordered_matcher(&expects, false);
        assert!(!m("10/12 08:23:56 [cs_script] Bye first"));
        assert!(!m("10/12 08:23:56 [cs_script] Hello there"));
        assert!(!m("random line Bye"), "not a run line");
        assert!(m("10/12 08:23:56 [cs_script] Bye now"));
    }

    #[test]
    fn run_lines_are_prefixed_script_lines_and_error_lines_unless_any_line() {
        assert!(is_run_line("10/12 08:23:56 [cs_script] Hello", false));
        assert!(!is_run_line("engine: Hello", false));
        assert!(is_run_line("Failed to load cs_script \"x\"", false));
        assert!(is_run_line("engine: Hello", true));
        let lines = [
            "engine: x",
            "10/12 08:23:56 [cs_script] A",
            "10/12 08:23:56 [cs_script] B",
        ];
        assert_eq!(
            script_lines(&lines),
            vec![
                "10/12 08:23:56 [cs_script] A",
                "10/12 08:23:56 [cs_script] B"
            ]
        );
    }

    #[test]
    fn judge_reports_which_expect_each_match_belongs_to() {
        let re = |s: &str| Regex::new(s).unwrap();
        let log = "10/12 08:23:56 [cs_script] A\n10/12 08:23:56 [cs_script] C\n";
        let v = judge(
            log,
            &[re("A"), re("B"), re("C")],
            &["A".into(), "B".into(), "C".into()],
        );
        assert_eq!(v.matched_expects, vec![0, 2]);
        assert_eq!(v.unmatched, vec!["B".to_string()]);
        assert!(!v.passed);
    }

    #[test]
    fn judge_passes_when_expects_match_in_order_and_no_error_line() {
        let re = |s: &str| Regex::new(s).unwrap();
        let v = judge(
            PASS_LOG,
            &[re("SCRIPT_TEST_BEGIN"), re("SCRIPT_TEST_PASS smoke")],
            &["SCRIPT_TEST_BEGIN".into(), "SCRIPT_TEST_PASS smoke".into()],
        );
        assert!(v.passed, "{v:?}");
        assert_eq!(v.matched.len(), 2);
        assert!(v.unmatched.is_empty() && v.errors.is_empty());
    }

    #[test]
    fn judge_is_order_sensitive() {
        let re = |s: &str| Regex::new(s).unwrap();
        let v = judge(
            PASS_LOG,
            &[re("SCRIPT_TEST_PASS"), re("SCRIPT_TEST_BEGIN")],
            &["SCRIPT_TEST_PASS".into(), "SCRIPT_TEST_BEGIN".into()],
        );
        assert!(!v.passed);
        assert_eq!(v.unmatched, vec!["SCRIPT_TEST_BEGIN".to_string()]);
    }

    #[test]
    fn judge_fails_on_a_script_error_even_when_every_expect_matched() {
        let re = |s: &str| Regex::new(s).unwrap();
        let v = judge(
            ERROR_LOG,
            &[re("SCRIPT_TEST_BEGIN")],
            &["SCRIPT_TEST_BEGIN".into()],
        );
        assert!(!v.passed);
        assert!(v.unmatched.is_empty());
        assert_eq!(v.errors.len(), 1);
        assert!(v.errors[0].contains("Uncaught Exception during callback"));
        assert!(v.log_excerpt.contains("Uncaught Exception during callback"));
        assert!(v.log_excerpt.contains("cs2mcp_probe.vjs:4:3"));
    }

    #[test]
    fn every_error_pattern_is_recognised_and_ordinary_lines_are_not() {
        for line in [
            "Uncaught Exception during callback \"OnPlayerKill\"",
            "Exception during CompileModule",
            "Exception during Evaluate",
            "Failed to compile cs_script[3]",
            "Failed to load cs_script \"maps/scripts/x.vjs\"",
            "Invalid script. No valid imports found.",
            "Invalid module specifier \"./x\". Relative values are not supported.",
            "Method CSPlayerPawn.GiveNamedItem invoked with bad int value. (parameter #1)",
            "Method Domain.Delay requires 1 argument(s), got 0.",
        ] {
            assert!(ERROR_RES.iter().any(|r| r.is_match(line)), "{line}");
        }
        for line in [
            "Loading script \"x\"",
            "SCRIPT_TEST_PASS ok",
            "Msg: round start",
        ] {
            assert!(!ERROR_RES.iter().any(|r| r.is_match(line)), "{line}");
        }
    }

    #[test]
    fn excerpt_keeps_errors_and_matches_and_caps_bytes() {
        let mut log = String::new();
        for i in 0..500 {
            log.push_str(&format!("filler {i}\n"));
        }
        log.insert_str(0, "SCRIPT_TEST_PASS early\n");
        let re = [Regex::new("SCRIPT_TEST_PASS").unwrap()];
        let v = judge(&log, &re, &["SCRIPT_TEST_PASS".into()]);
        assert!(v.log_excerpt.starts_with("SCRIPT_TEST_PASS early"));
        assert!(v.log_excerpt.lines().count() <= EXCERPT_TAIL_LINES + 1);
        assert!(v.log_excerpt.len() <= EXCERPT_MAX_BYTES);
    }

    #[test]
    fn staging_paths_are_unique_per_token_and_refuse_unsafe_tokens() {
        let a = unique_token(42, 1_000, 1);
        let b = unique_token(42, 1_000, 2);
        assert_ne!(a, b);
        let p = staged_rel_path(ScriptKind::VScript, &a).unwrap();
        assert!(p.starts_with("scripts/vscripts/cs2mcp/vscript-"));
        assert!(p.ends_with(".lua"));
        assert!(staged_rel_path(ScriptKind::CsScript, "../etc").is_err());
        assert!(staged_rel_path(ScriptKind::CsScript, "").is_err());
    }

    #[test]
    fn vscript_command_is_script_reload_code_with_a_safe_path_only() {
        assert_eq!(
            vscript_command("cs2mcp/vscript-1a-2-3.nut").unwrap(),
            "script_reload_code cs2mcp/vscript-1a-2-3.nut"
        );
        assert!(vscript_command("../x.nut").is_err());
        assert!(vscript_command("/abs.nut").is_err());
        assert!(vscript_command("a;quit").is_err());
        assert!(vscript_command("a b.nut").is_err());
        assert!(vscript_command("a//b.nut").is_err());
    }

    #[test]
    fn real_console_lines_carry_a_stamp_before_the_channel() {
        let hello = "10/12 08:23:56 [cs_script] Hello cs_script!";
        let server = "10/12 08:23:56 [Server] changelevel de_dust2";
        assert_eq!(message(hello), "[cs_script] Hello cs_script!");
        assert_eq!(message("no stamp [cs_script] x"), "no stamp [cs_script] x");
        // stamped script lines are run lines and script lines; other channels are not
        assert!(is_run_line(hello, false));
        assert!(!is_run_line(server, false));
        assert_eq!(script_lines(&[server, hello]), vec![hello]);
        // an expect matches the whole raw line: unanchored works, `^` anchors at the stamp
        let re = |s: &str| Regex::new(s).unwrap();
        let v = judge(hello, &[re("Hello cs_script")], &["Hello cs_script".into()]);
        assert!(v.passed, "{v:?}");
        let anchored = judge(hello, &[re("^\\[cs_script\\]")], &["^".into()]);
        assert!(!anchored.passed);
        let stamped = judge(
            hello,
            &[re("^10/12 08:23:56 \\[cs_script\\] Hello")],
            &["^".into()],
        );
        assert!(stamped.passed);
    }

    /// A cs_script_demo console.log segment (shape from the 2026-10-12 live run; the error line is
    /// invented in the engine's error format).
    const DEMO_LOG: &str = "\
[Workshop] Loading custom game cs_script_demo
10/12 08:23:56 [cs_script] Hello cs_script!
10/12 08:23:56 [cs_script] Loaded setup
engine: unrelated line with Trace
Uncaught Exception during callback \"OnRoundStart\"
10/12 08:23:56 [cs_script] Trace 1
";

    #[test]
    fn ordered_judge_over_the_demo_fixture_matches_in_order_and_reports_the_error() {
        let re = |s: &str| Regex::new(s).unwrap();
        let src = |s: &str| s.to_string();
        let lines: Vec<&str> = DEMO_LOG.lines().collect();
        let filtered: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| is_run_line(l, false))
            .collect();
        let text = filtered.join("\n");
        assert_eq!(script_lines(&lines).len(), 3);
        // in order: Hello, then Trace: both matched, the error line still fails the run
        let v = judge(
            &text,
            &[re("Hello"), re("Trace")],
            &[src("Hello"), src("Trace")],
        );
        assert_eq!(
            v.matched,
            vec![
                "10/12 08:23:56 [cs_script] Hello cs_script!",
                "10/12 08:23:56 [cs_script] Trace 1"
            ]
        );
        assert_eq!(v.matched_expects, vec![0, 1]);
        assert_eq!(v.errors.len(), 1);
        assert!(!v.passed);
        // out of order: Trace comes after Hello, so the reverse order does not match
        let v = judge(
            &text,
            &[re("Trace"), re("Loaded")],
            &[src("Trace"), src("Loaded")],
        );
        assert_eq!(v.unmatched, vec!["Loaded".to_string()]);
        // the wait agrees: it finishes on the last line it needs (Trace 1), not before
        let expects = [re("Hello"), re("Trace")];
        let m = ordered_matcher(&expects, false);
        let done_at = filtered.iter().position(|l| m(l));
        assert_eq!(done_at, Some(filtered.len() - 1));
        // the unprefixed engine line only counts with any_line
        let unrelated = [re("unrelated")];
        let any = ordered_matcher(&unrelated, true);
        assert!(lines.iter().any(|l| any(l)));
        let strict = ordered_matcher(&unrelated, false);
        assert!(!lines.iter().any(|l| strict(l)));
    }

    #[test]
    fn both_kinds_have_a_blocker_with_evidence_and_an_unblock_route() {
        for k in [ScriptKind::CsScript, ScriptKind::VScript] {
            let b = blocker(k);
            assert!(!b.missing.is_empty() && !b.unblock.is_empty());
            assert!(!b.evidence.is_empty());
        }
    }
}
