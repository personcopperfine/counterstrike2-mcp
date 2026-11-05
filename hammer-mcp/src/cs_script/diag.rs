//! tsc / rollup output -> `{file, line, col, code, message}`.
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// The most diagnostics a reply lists (the count of all is reported).
pub const MAX_LISTED: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub code: String,
    pub message: String,
}

impl Diag {
    pub fn to_json(&self) -> Value {
        json!({"file": self.file, "line": self.line, "col": self.col, "code": self.code, "message": self.message})
    }
}

fn re_tsc() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<file>.+?)\((?P<line>\d+),(?P<col>\d+)\): (?:error|warning) (?P<code>TS\d+): (?P<msg>.*)$").expect("regex")
    })
}

fn re_global() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:error|warning) (?P<code>TS\d+): (?P<msg>.*)$").expect("regex"))
}

/// The `[!] (plugin typescript) ... TS2304: ...` and `file (line:col)` shapes rollup uses for a
/// typescript plugin failure, and the bare `[!] message` of any other.
fn re_rollup_loc() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?P<file>\S+\.(?:ts|js|mjs|json)) \((?P<line>\d+):(?P<col>\d+)\)\s*$").expect("regex"))
}

/// Diagnostics of `tsc --pretty false` (a continuation line indented under an error extends its
/// message). Lines that are not diagnostics are ignored.
pub fn parse_tsc(output: &str) -> Vec<Diag> {
    let mut diags: Vec<Diag> = Vec::new();
    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(c) = re_tsc().captures(line) {
            diags.push(Diag {
                file: c["file"].replace('\\', "/"),
                line: c["line"].parse().unwrap_or(0),
                col: c["col"].parse().unwrap_or(0),
                code: c["code"].to_string(),
                message: c["msg"].to_string(),
            });
        } else if let Some(c) = re_global().captures(line) {
            diags.push(Diag { file: String::new(), line: 0, col: 0, code: c["code"].to_string(), message: c["msg"].to_string() });
        } else if line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            if let Some(last) = diags.last_mut() {
                if last.message.len() < 600 {
                    last.message.push(' ');
                    last.message.push_str(line.trim());
                }
            }
        }
    }
    diags
}

/// Diagnostics of a failed `rollup -c`: the typescript plugin's `TS<n>` errors when it printed
/// them, else one entry for the `[!]` error (with its `file (line:col)` location when there is one).
pub fn parse_rollup(output: &str) -> Vec<Diag> {
    let ts = parse_tsc(output);
    if !ts.is_empty() {
        return ts;
    }
    let lines: Vec<&str> = output.lines().map(|l| l.trim_end_matches('\r')).collect();
    let Some(start) = lines.iter().position(|l| l.starts_with("[!]")) else { return Vec::new() };
    let message = lines[start].trim_start_matches("[!]").trim().to_string();
    let mut diag = Diag { file: String::new(), line: 0, col: 0, code: "ROLLUP".into(), message };
    for l in &lines[start + 1..lines.len().min(start + 6)] {
        if let Some(c) = re_rollup_loc().captures(l.trim()) {
            diag.file = c["file"].to_string();
            diag.line = c["line"].parse().unwrap_or(0);
            diag.col = c["col"].parse().unwrap_or(0);
            break;
        }
    }
    vec![diag]
}

/// `{errors, total, truncated}` fields for a reply.
pub fn listing(diags: &[Diag]) -> Value {
    json!({
        "errors": diags.iter().take(MAX_LISTED).map(Diag::to_json).collect::<Vec<_>>(),
        "errorCount": diags.len(),
        "truncated": diags.len() > MAX_LISTED,
    })
}

/// The last `max` lines of `text`.
pub fn tail_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max)..].join("\n")
}
