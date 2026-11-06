//! "Register with Claude Desktop / Claude Code": put this install's `mcpServers` entries
//! (`session::mcp_config`) into the MCP client's configuration, only after the user saw what
//! changes.
//!
//! * Claude Desktop reads one JSON file. [`merge`] adds or replaces our entries in it and keeps
//!   everything else exactly as written (key order and formatting of the untouched parts too, so
//!   the [`diff`] the user confirms is only our lines); [`write`] backs the old file up first.
//!   The file's place: [`desktop_candidates`]. Only the documented Windows/macOS path is offered
//!   when it does not exist yet; any other place is offered only when the file is already there.
//! * Claude Code owns its own config: the change goes through its CLI
//!   (`claude mcp add-json --scope user <name> <json>`, [`claude_code_commands`]), never a file
//!   edit here.

use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Where Claude Desktop's `claude_desktop_config.json` may be, best first: `(path, exists)`.
pub fn desktop_candidates() -> Vec<(PathBuf, bool)> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    let mut add = |p: PathBuf, documented: bool| {
        let exists = p.is_file();
        if (exists || documented) && !out.iter().any(|(q, _)| q == &p) {
            out.push((p, exists));
        }
    };
    if cfg!(windows) {
        // the documented place (the installer's build)
        if let Some(a) = env("APPDATA") {
            add(a.join("Claude").join("claude_desktop_config.json"), true);
        }
        // the Microsoft Store (MSIX) build keeps its roaming data under its package dir: only
        // offered when that file exists (its package name is not guessed)
        if let Some(l) = env("LOCALAPPDATA") {
            if let Ok(rd) = std::fs::read_dir(l.join("Packages")) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with("Claude_") {
                        add(e.path().join("LocalCache/Roaming/Claude/claude_desktop_config.json"), false);
                    }
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = env("HOME") {
            add(h.join("Library/Application Support/Claude/claude_desktop_config.json"), true);
        }
    } else {
        // no official Linux build: only an existing file of a community build is offered
        if let Some(c) = env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))) {
            add(c.join("Claude/claude_desktop_config.json"), false);
        }
    }
    out
}

/// A JSON object's members in file order, each value as its original text.
struct Members(Vec<(String, Box<RawValue>)>);

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Members, A::Error> {
                let mut v = Vec::new();
                while let Some((k, val)) = m.next_entry::<String, Box<RawValue>>()? {
                    v.push((k, val));
                }
                Ok(Members(v))
            }
        }
        d.deserialize_map(V)
    }
}

/// `value` pretty-printed, its continuation lines indented by `indent`.
fn pretty_at(value: &Value, indent: &str) -> String {
    let s = serde_json::to_string_pretty(value).unwrap_or_default();
    s.lines().enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { format!("{indent}{l}") }).collect::<Vec<_>>().join("\n")
}

fn key(k: &str) -> String {
    serde_json::to_string(k).unwrap_or_default()
}

/// The config file's text with `servers` (an `mcpServers` object: name -> entry) added or
/// replaced. `existing`: the current text (`None` or blank: a new file). Everything else in the
/// file keeps its text and order; a file that is not a JSON object is refused (never replaced).
pub fn merge(existing: Option<&str>, servers: &Value) -> anyhow::Result<String> {
    let ours = servers.as_object().ok_or_else(|| anyhow::anyhow!("mcpServers is not an object"))?;
    let existing = existing.filter(|t| !t.trim().is_empty());
    let top: Vec<(String, Box<RawValue>)> = match existing {
        Some(t) => serde_json::from_str::<Members>(t).map_err(|e| anyhow::anyhow!("the file is not a JSON object ({e}); fix or move it first, it is not overwritten"))?.0,
        None => Vec::new(),
    };
    let mut lines: Vec<String> = Vec::new();
    let mut had_servers = false;
    for (k, raw) in &top {
        if k == "mcpServers" {
            had_servers = true;
            let theirs = serde_json::from_str::<Members>(raw.get()).map_err(|e| anyhow::anyhow!("mcpServers is not an object ({e}); fix it first, the file is not overwritten"))?.0;
            let mut inner: Vec<String> = Vec::new();
            for (name, entry) in &theirs {
                match ours.get(name) {
                    Some(v) => inner.push(format!("    {}: {}", key(name), pretty_at(v, "    "))),
                    None => inner.push(format!("    {}: {}", key(name), entry.get())),
                }
            }
            for (name, v) in ours {
                if !theirs.iter().any(|(n, _)| n == name) {
                    inner.push(format!("    {}: {}", key(name), pretty_at(v, "    ")));
                }
            }
            lines.push(format!("  \"mcpServers\": {{\n{}\n  }}", inner.join(",\n")));
        } else {
            lines.push(format!("  {}: {}", key(k), raw.get()));
        }
    }
    if !had_servers {
        let inner: Vec<String> = ours.iter().map(|(name, v)| format!("    {}: {}", key(name), pretty_at(v, "    "))).collect();
        lines.push(format!("  \"mcpServers\": {{\n{}\n  }}", inner.join(",\n")));
    }
    let text = format!("{{\n{}\n}}\n", lines.join(",\n"));
    // the result must parse and carry exactly our entries
    let check: Value = serde_json::from_str(&text)?;
    for (name, v) in ours {
        anyhow::ensure!(check["mcpServers"][name] == *v, "merge check failed for {name}");
    }
    Ok(text)
}

/// One line of a [`diff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Same(String),
    Removed(String),
    Added(String),
}

/// A line diff of `a` -> `b` (LCS; config files are small).
pub fn diff(a: &str, b: &str) -> Vec<Line> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut l = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push(Line::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if j < m && (i == n || l[i][j + 1] >= l[i + 1][j]) {
            out.push(Line::Added(b[j].to_string()));
            j += 1;
        } else {
            out.push(Line::Removed(a[i].to_string()));
            i += 1;
        }
    }
    out
}

/// Write `text` to `path`, the old file (if any) copied to `<name>.cs2mcp-backup-<time>` first.
/// Returns the backup's path.
pub fn write(path: &Path, text: &str) -> anyhow::Result<Option<PathBuf>> {
    let backup = if path.is_file() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let b = path.with_file_name(format!("{name}.cs2mcp-backup-{}", cs2_native::manifest::now_secs()));
        std::fs::copy(path, &b)?;
        Some(b)
    } else {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        None
    };
    cs2_native::manifest::write_atomic(path, text.as_bytes())?;
    Ok(backup)
}

/// The `claude` CLI on PATH (`claude.exe` on Windows: an npm `claude.cmd` cannot take a JSON
/// argument through cmd.exe safely, so it is not run, only shown).
pub fn claude_cli() -> Option<PathBuf> {
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// The Claude Code commands that register `servers` for the user (all projects): with
/// `replace`, each `add-json` is preceded by a `remove` of the same name (which fails harmlessly
/// when there is none).
pub fn claude_code_commands(servers: &Value, replace: bool) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for (name, v) in servers.as_object().into_iter().flatten() {
        let mut entry = serde_json::json!({"type": "stdio"});
        if let (Some(e), Some(o)) = (entry.as_object_mut(), v.as_object()) {
            for (k, x) in o {
                e.insert(k.clone(), x.clone());
            }
        }
        if replace {
            out.push(["mcp", "remove", "--scope", "user", name].map(String::from).to_vec());
        }
        out.push(vec!["mcp".into(), "add-json".into(), "--scope".into(), "user".into(), name.clone(), entry.to_string()]);
    }
    out
}

/// A command line for display (POSIX-quoted).
pub fn shell_line(program: &str, args: &[String]) -> String {
    let q = |s: &str| {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c)) {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    };
    std::iter::once(q(program)).chain(args.iter().map(|a| q(a))).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ours() -> Value {
        json!({"cs2-client": {"command": "/opt/c/client-mcp", "args": ["--discovery", "/run/d.json"]}})
    }

    #[test]
    fn a_new_file_gets_only_our_servers() {
        let t = merge(None, &ours()).unwrap();
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v, json!({"mcpServers": ours()}));
        assert_eq!(merge(Some("  \n"), &ours()).unwrap(), t);
    }

    /// The user's other settings and servers keep their text and order; ours is replaced in place.
    #[test]
    fn an_existing_file_keeps_everything_else_as_written() {
        let before = "{\n  \"zeta\": 1,\n  \"mcpServers\": {\n    \"theirs\": {\"command\": \"x\"},\n    \"cs2-client\": {\"command\": \"old\"}\n  },\n  \"alpha\": [1, 2]\n}\n";
        let after = merge(Some(before), &ours()).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["mcpServers"]["theirs"], json!({"command": "x"}));
        assert_eq!(v["mcpServers"]["cs2-client"], ours()["cs2-client"]);
        assert_eq!(v["zeta"], 1);
        // order: zeta, mcpServers, alpha; theirs before ours
        assert!(after.find("zeta").unwrap() < after.find("mcpServers").unwrap());
        assert!(after.find("mcpServers").unwrap() < after.find("alpha").unwrap());
        assert!(after.find("theirs").unwrap() < after.find("cs2-client").unwrap());
        assert!(after.contains("\"theirs\": {\"command\": \"x\"}"), "untouched text stays: {after}");
        let d = diff(before, &after);
        assert!(d.iter().any(|l| matches!(l, Line::Removed(s) if s.contains("old"))));
        assert!(d.iter().filter(|l| matches!(l, Line::Removed(_))).count() <= 2, "{d:?}");
        // idempotent
        assert_eq!(merge(Some(&after), &ours()).unwrap(), after);
    }

    #[test]
    fn a_broken_file_is_refused_not_overwritten() {
        assert!(merge(Some("{ \"a\": "), &ours()).is_err());
        assert!(merge(Some("[1]"), &ours()).is_err());
        assert!(merge(Some("{\"mcpServers\": 3}"), &ours()).is_err());
    }

    #[test]
    fn write_backs_the_old_file_up() {
        // under target/, never /tmp (it may be RAM)
        let d = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/scratch/cs2-launcher")).join(format!("register-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let p = d.join("Claude/claude_desktop_config.json");
        assert_eq!(write(&p, "{}\n").unwrap(), None);
        let b = write(&p, "{\"a\": 1}\n").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "{}\n");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\": 1}\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn claude_code_gets_stdio_entries_per_server() {
        let c = claude_code_commands(&ours(), true);
        assert_eq!(c[0], ["mcp", "remove", "--scope", "user", "cs2-client"]);
        assert_eq!(&c[1][..5], ["mcp", "add-json", "--scope", "user", "cs2-client"]);
        let e: Value = serde_json::from_str(&c[1][5]).unwrap();
        assert_eq!(e["type"], "stdio");
        assert_eq!(e["command"], "/opt/c/client-mcp");
        assert_eq!(shell_line("claude", &c[0]), "claude mcp remove --scope user cs2-client");
        assert!(shell_line("claude", &c[1]).ends_with("'"));
    }
}
