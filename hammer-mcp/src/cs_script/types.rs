//! Types follow the game. The npm package `@s2ze/types` mirrors Valve's `point_script.d.ts`, but
//! lags or leads the game build (2.23.3 declares a `Console`/`GameRules`/hostage API the 2.23.1
//! file Valve ships does not; types that do not match the game let a script type-check and then
//! fail in the game). The game's own file is
//! `<app>/content/csgo_addons/cs_script_demo/maps/scripts/point_script.d.ts`.
//!
//! * the comparison is semantic: [`GAME_FILE_REL`] against the npm package's built
//!   `autoimport/index.d.ts` (the package ships tsc's declaration output, not Valve's text, so the
//!   two never match byte for byte; the node script `api-fingerprint.mjs` compares what they
//!   declare). Equal modulo whitespace short-circuits.
//! * a mismatch writes the game's file to `types/point_script.d.ts` in the project and rewires
//!   every `src/*/tsconfig.json` to `types: []` plus that file in `include`: the npm
//!   `autoimport` types (which declare the same ambient module) are no longer loaded, so the game's
//!   file is the only declaration of `cs_script/point_script` (an ambient module declaration beats
//!   a `paths` mapping, so a `paths` override cannot work). The npm `extended` helpers go with it:
//!   they augment the npm declaration. When the package matches the game again the file is deleted
//!   and the tsconfigs return to `extends: @s2ze/tsconfig`.
use super::CsError;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The game's declaration, relative to the app root.
pub const GAME_FILE_REL: &str = "content/csgo_addons/cs_script_demo/maps/scripts/point_script.d.ts";
/// The override inside the project.
pub const OVERRIDE_REL: &str = "types/point_script.d.ts";
/// `include` of an overridden script tsconfig (the folder's own sources stay in).
pub const OVERRIDE_INCLUDE: [&str; 2] = ["../../types/point_script.d.ts", "./**/*"];

pub fn game_file(app_root: &Path) -> PathBuf {
    app_root.join(GAME_FILE_REL)
}

/// The npm package's API declaration inside a project.
pub fn npm_file(project_dir: &Path) -> PathBuf {
    project_dir.join("node_modules/@s2ze/types/build/autoimport/index.d.ts")
}

pub fn override_file(project_dir: &Path) -> PathBuf {
    project_dir.join(OVERRIDE_REL)
}

pub fn override_active(project_dir: &Path) -> bool {
    override_file(project_dir).is_file()
}

/// Text with trailing whitespace stripped per line, CRLF as LF and no trailing blank lines.
pub fn normalise(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    lines[..end].join("\n")
}

/// What the comparison found.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Equal,
    /// The node script's report (`onlyA` = only in the game, `onlyB` = only in the package).
    Different(Value),
    Unknown(String),
}

impl Verdict {
    pub fn state(&self) -> &'static str {
        match self {
            Verdict::Equal => "equal",
            Verdict::Different(_) => "different",
            Verdict::Unknown(_) => "unknown",
        }
    }
}

/// Read the node script's JSON line.
pub fn verdict_from_report(stdout: &str) -> Verdict {
    let Some(line) = stdout.lines().rev().find(|l| l.trim_start().starts_with('{')) else {
        return Verdict::Unknown(format!("the comparison printed no report: {}", tail(stdout)));
    };
    match serde_json::from_str::<Value>(line) {
        Ok(report) if report.get("equal").and_then(Value::as_bool) == Some(true) => Verdict::Equal,
        Ok(report) if report.get("equal").and_then(Value::as_bool) == Some(false) => Verdict::Different(json!({
            "onlyInGame": report["onlyA"],
            "onlyInNpm": report["onlyB"],
            "changed": report["changed"],
            "counts": report["counts"],
        })),
        _ => Verdict::Unknown(format!("the comparison report is unreadable: {}", tail(line))),
    }
}

fn tail(text: &str) -> String {
    let t = text.trim();
    if t.len() > 300 {
        let mut cut = t.len() - 300;
        while !t.is_char_boundary(cut) {
            cut += 1;
        }
        format!("...{}", &t[cut..])
    } else {
        t.to_string()
    }
}

/// What [`sync_override`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideAction {
    /// no override, none needed
    None,
    /// written for the first time
    Written,
    /// the game's file changed since: the copy was replaced
    Refreshed,
    /// already in place and current
    Kept,
    /// the package matches the game again: the file is gone and the tsconfigs are back to normal
    Removed,
    /// the verdict is unknown: whatever was there stays
    Untouched,
}

impl OverrideAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverrideAction::None => "none",
            OverrideAction::Written => "written",
            OverrideAction::Refreshed => "refreshed",
            OverrideAction::Kept => "kept",
            OverrideAction::Removed => "removed",
            OverrideAction::Untouched => "untouched",
        }
    }
}

/// `base` (a script tsconfig.json text) with the override applied.
pub fn override_tsconfig_text(base: &str) -> String {
    let mut value: Value = serde_json::from_str(base).unwrap_or_else(|_| json!({"extends": "@s2ze/tsconfig"}));
    apply_override(&mut value, true);
    format!("{}\n", serde_json::to_string_pretty(&value).expect("json"))
}

/// Set (`on`) or clear the override keys of a tsconfig value. Returns whether it changed.
fn apply_override(config: &mut Value, on: bool) -> bool {
    let Some(map) = config.as_object_mut() else { return false };
    let before = map.clone();
    let include = json!(OVERRIDE_INCLUDE);
    if on {
        let options = map.entry("compilerOptions").or_insert_with(|| json!({}));
        if let Some(options) = options.as_object_mut() {
            options.insert("types".into(), json!([]));
        }
        map.insert("include".into(), include);
    } else {
        if let Some(options) = map.get_mut("compilerOptions").and_then(Value::as_object_mut) {
            if options.get("types") == Some(&json!([])) {
                options.remove("types");
            }
        }
        if map.get("compilerOptions").and_then(Value::as_object).is_some_and(|o| o.is_empty()) {
            map.remove("compilerOptions");
        }
        if map.get("include") == Some(&include) {
            map.remove("include");
        }
    }
    *map != before
}

const SAVED_FILE: &str = ".cs-override-saved.json";

/// Rewire every `src/*/tsconfig.json`; returns `(rewritten, unreadable)` script names. A script
/// tsconfig that already had its own `compilerOptions.types` / `include` keeps them: the first
/// rewiring saves them in `<project>/.cs-override-saved.json` and removal restores them exactly.
fn rewire_tsconfigs(project_dir: &Path, on: bool) -> Result<(Vec<String>, Vec<String>), CsError> {
    let mut rewritten = Vec::new();
    let mut unreadable = Vec::new();
    let saved_path = project_dir.join(SAVED_FILE);
    let mut saved: Map<String, Value> = std::fs::read_to_string(&saved_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let before_saved = saved.clone();
    let Ok(entries) = std::fs::read_dir(project_dir.join("src")) else { return Ok((rewritten, unreadable)) };
    let mut dirs: Vec<_> = entries.flatten().filter(|e| e.path().join("tsconfig.json").is_file()).collect();
    dirs.sort_by_key(|e| e.file_name());
    let ours = json!(OVERRIDE_INCLUDE);
    for entry in dirs {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().join("tsconfig.json");
        let text = std::fs::read_to_string(&path).map_err(|e| CsError::Io(format!("{}: {e}", path.display())))?;
        let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
            // hand-edited with comments: leave it, say so
            unreadable.push(name);
            continue;
        };
        let changed = if on {
            let types = value.pointer("/compilerOptions/types").cloned();
            let include = value.get("include").cloned();
            let already_ours = types == Some(json!([])) && include.as_ref() == Some(&ours);
            if !already_ours && !saved.contains_key(&name) && (types.is_some() || include.is_some()) {
                saved.insert(name.clone(), json!({"types": types, "include": include}));
            }
            apply_override(&mut value, true)
        } else if let Some(prev) = saved.remove(&name) {
            restore(&mut value, &prev)
        } else {
            apply_override(&mut value, false)
        };
        if changed {
            let out = format!("{}\n", serde_json::to_string_pretty(&value).expect("json"));
            super::write_file(&path, &out)?;
            rewritten.push(name);
        }
    }
    if saved != before_saved {
        if saved.is_empty() {
            let _ = std::fs::remove_file(&saved_path);
        } else {
            super::write_file(&saved_path, &serde_json::to_string_pretty(&Value::Object(saved)).expect("json"))?;
        }
    }
    Ok((rewritten, unreadable))
}

/// Put back the `types` / `include` a tsconfig had before the override (absent = removed).
fn restore(config: &mut Value, prev: &Value) -> bool {
    let Some(map) = config.as_object_mut() else { return false };
    let before = map.clone();
    let types = prev.get("types").filter(|v| !v.is_null());
    let include = prev.get("include").filter(|v| !v.is_null());
    match types {
        Some(t) => {
            let options = map.entry("compilerOptions").or_insert_with(|| json!({}));
            if let Some(o) = options.as_object_mut() {
                o.insert("types".into(), t.clone());
            }
        }
        None => {
            if let Some(o) = map.get_mut("compilerOptions").and_then(Value::as_object_mut) {
                o.remove("types");
            }
            if map.get("compilerOptions").and_then(Value::as_object).is_some_and(|o| o.is_empty()) {
                map.remove("compilerOptions");
            }
        }
    }
    match include {
        Some(i) => {
            map.insert("include".into(), i.clone());
        }
        None => {
            map.remove("include");
        }
    }
    *map != before
}

/// Apply a verdict to the project: write/refresh/remove the override and rewire the tsconfigs.
/// `game_text` is the game's file (needed for `Different`). Returns the action, the scripts whose
/// tsconfig was rewritten and those it could not parse.
pub fn sync_override(
    project_dir: &Path,
    verdict: &Verdict,
    game_text: Option<&str>,
) -> Result<(OverrideAction, Vec<String>, Vec<String>), CsError> {
    let file = override_file(project_dir);
    match verdict {
        Verdict::Unknown(_) => Ok((OverrideAction::Untouched, vec![], vec![])),
        Verdict::Equal => {
            if !file.exists() {
                return Ok((OverrideAction::None, vec![], vec![]));
            }
            std::fs::remove_file(&file).map_err(|e| CsError::Io(format!("{}: {e}", file.display())))?;
            // the (now empty) types/ folder goes too
            if let Some(dir) = file.parent() {
                let _ = std::fs::remove_dir(dir);
            }
            let (rewritten, unreadable) = rewire_tsconfigs(project_dir, false)?;
            Ok((OverrideAction::Removed, rewritten, unreadable))
        }
        Verdict::Different(_) => {
            let text = game_text.ok_or_else(|| CsError::Config("the game's point_script.d.ts is needed for the override".into()))?;
            let action = match std::fs::read_to_string(&file) {
                Ok(old) if old == text => OverrideAction::Kept,
                Ok(_) => OverrideAction::Refreshed,
                Err(_) => OverrideAction::Written,
            };
            if action != OverrideAction::Kept {
                super::write_file(&file, text)?;
            }
            let (rewritten, unreadable) = rewire_tsconfigs(project_dir, true)?;
            Ok((action, rewritten, unreadable))
        }
    }
}
