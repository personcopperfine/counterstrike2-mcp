//! `game_test` spec: parsing and validation. Everything here runs before any step is dispatched, so
//! a bad spec sends nothing. `$ref` checks are static (the label must belong to an earlier step); the
//! path inside a referenced result is only known at run time.
use cs2_mcp_common::tools::ToolError;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

/// Tools a step may dispatch: the building blocks of the unit-test design (section 5).
/// `script_run` is deliberately absent: it launches its own session (and addon map), which a test
/// cannot account for in its budget or its session check. Run it outside game_test.
pub const STEP_TOOLS: &[&str] = &[
    "round_start",
    "setpos",
    "game_snapshot",
    "game_restore",
    "input_sequence",
    "fire_weapon",
    "look_at",
    "use_entity",
    "ent_fire",
    "console_exec",
    "console_wait",
    "game_wait",
    "game_state",
    "weapons",
    "sounds_drain",
    "demo_record_start",
    "demo_record_stop",
    "checkpoint",
];

pub const MAX_STEPS: usize = 64;
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_TOLERANCE: f64 = 0.001;

const SPEC_KEYS: &[&str] = &[
    "name",
    "map",
    "addon",
    "setup",
    "steps",
    "expect",
    "teardown",
    "timeoutMs",
    "demo",
    "continueOnFailure",
    "verbose",
];
const STEP_KEYS: &[&str] = &["tool", "args", "label", "expect"];
const ASSERTION_KEYS: &[&str] = &["path", "op", "value", "tolerance", "from"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Approx,
    Contains,
    Matches,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "lt" => Op::Lt,
            "le" => Op::Le,
            "gt" => Op::Gt,
            "ge" => Op::Ge,
            "approx" => Op::Approx,
            "contains" => Op::Contains,
            "matches" => Op::Matches,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Lt => "lt",
            Op::Le => "le",
            Op::Gt => "gt",
            Op::Ge => "ge",
            Op::Approx => "approx",
            Op::Contains => "contains",
            Op::Matches => "matches",
        }
    }
}

/// Where an assertion reads its value from: the step's own result, or a fresh read at check time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Result,
    GameState,
    Weapons,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Result => "result",
            Source::GameState => "game_state",
            Source::Weapons => "weapons",
        }
    }

    /// The read tool behind a fresh-read source (`Result` has none).
    pub fn tool(self) -> &'static str {
        match self {
            Source::Result => "",
            Source::GameState => "game_state",
            Source::Weapons => "weapons",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Assertion {
    pub path: String,
    pub segments: Vec<String>,
    pub op: Op,
    /// literal or `{"$ref": "label.path"}`; resolved when the assertion is checked
    pub value: Value,
    pub tolerance: f64,
    pub from: Source,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub tool: &'static str,
    pub args: Value,
    pub label: Option<String>,
    pub expect: Vec<Assertion>,
}

impl Step {
    /// The label used in records and JUnit: the given label, else the tool name.
    pub fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.tool.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    pub map: Option<String>,
    pub addon: Option<String>,
    pub setup: Vec<Step>,
    pub steps: Vec<Step>,
    pub expect: Vec<Assertion>,
    pub teardown: Vec<Step>,
    pub timeout_ms: u64,
    pub demo: bool,
    pub continue_on_failure: bool,
    /// false: successful steps carry no `result` (failed steps always do)
    pub verbose: bool,
}

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(msg)
}

/// `game_test {spec}`: the whole spec is validated here, before anything is dispatched.
pub fn parse(args: &Value) -> Result<Spec, ToolError> {
    let top = args
        .as_object()
        .ok_or_else(|| invalid("game_test takes {spec: {...}}"))?;
    reject_unknown(top, &["spec"], "game_test")?;
    let obj = top
        .get("spec")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("spec must be an object"))?;
    reject_unknown(obj, SPEC_KEYS, "spec")?;

    let name = match obj.get("name") {
        Some(Value::String(s)) if name_ok(s) => s.clone(),
        _ => {
            return Err(invalid(
                "spec.name must match ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$",
            ))
        }
    };
    let map = opt_string(obj, "map")?;
    if let Some(m) = &map {
        if !map_ok(m) {
            return Err(invalid(
                "spec.map must be [A-Za-z0-9_-] segments joined by / (at most 128 bytes)",
            ));
        }
    }
    let addon = opt_string(obj, "addon")?;
    if let Some(a) = &addon {
        if a.is_empty() || a.len() > 64 || !a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(invalid("spec.addon must match [A-Za-z0-9_]{1,64}"));
        }
    }
    let timeout_ms = match obj.get("timeoutMs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_TIMEOUT_MS).contains(n))
            .ok_or_else(|| invalid("spec.timeoutMs must be an integer in 1..=600000"))?,
    };
    let demo = opt_bool(obj, "demo")?;
    let continue_on_failure = opt_bool(obj, "continueOnFailure")?;
    let verbose = match obj.get("verbose") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("spec.verbose must be a boolean")),
    };

    // labels defined so far (refs may only name earlier steps: setup, steps, spec expect, teardown)
    let mut defined = HashSet::new();
    let mut count = 0usize;
    let setup = parse_steps(obj.get("setup"), "setup", &mut defined, &mut count)?;
    let steps = parse_steps(obj.get("steps"), "steps", &mut defined, &mut count)?;
    if steps.is_empty() {
        return Err(invalid("spec.steps needs at least one step"));
    }
    let expect = parse_assertions(obj.get("expect"), "spec", false, &defined)?;
    let teardown = parse_steps(obj.get("teardown"), "teardown", &mut defined, &mut count)?;

    Ok(Spec {
        name,
        map,
        addon,
        setup,
        steps,
        expect,
        teardown,
        timeout_ms,
        demo,
        continue_on_failure,
        verbose,
    })
}

fn reject_unknown(obj: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), ToolError> {
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(invalid(format!("unknown key {k:?} in {what}"))),
        None => Ok(()),
    }
}

fn opt_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("spec.{key} must be a string"))),
    }
}

fn opt_bool(obj: &Map<String, Value>, key: &str) -> Result<bool, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid(format!("spec.{key} must be a boolean"))),
    }
}

/// Spec name: alphanumeric first, then [A-Za-z0-9_-], at most 64 (also the demo name).
fn name_ok(s: &str) -> bool {
    s.len() <= 64
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Step label: [A-Za-z0-9_-]{1,64}; no dots (a `$ref` splits on the first dot).
fn label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn map_ok(s: &str) -> bool {
    s.len() <= 128
        && !s.is_empty()
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Dot-separated path segments ([A-Za-z0-9_]+ each); `None` when empty or malformed.
pub fn segments(path: &str) -> Option<Vec<String>> {
    let segs: Vec<String> = path.split('.').map(str::to_string).collect();
    segs.iter()
        .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(segs)
}

fn parse_steps(
    v: Option<&Value>,
    phase: &str,
    defined: &mut HashSet<String>,
    count: &mut usize,
) -> Result<Vec<Step>, ToolError> {
    let items = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(invalid(format!("spec.{phase} must be an array of steps"))),
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        *count += 1;
        if *count > MAX_STEPS {
            return Err(invalid(format!("at most {MAX_STEPS} steps per game_test")));
        }
        out.push(parse_step(item, phase, i, defined)?);
    }
    Ok(out)
}

fn parse_step(
    v: &Value,
    phase: &str,
    idx: usize,
    defined: &mut HashSet<String>,
) -> Result<Step, ToolError> {
    let at = format!("{phase}[{idx}]");
    let obj = v.as_object().ok_or_else(|| {
        invalid(format!(
            "{at} must be an object {{tool, args?, label?, expect?}}"
        ))
    })?;
    reject_unknown(obj, STEP_KEYS, &at)?;
    let tool_name = obj
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{at}.tool must be a string")))?;
    let tool = STEP_TOOLS
        .iter()
        .copied()
        .find(|t| *t == tool_name)
        .ok_or_else(|| {
            invalid(format!(
                "{at}.tool {tool_name:?} is not an allowed step tool"
            ))
        })?;
    let args = match obj.get("args") {
        None | Some(Value::Null) => json!({}),
        Some(a @ Value::Object(_)) => a.clone(),
        Some(_) => return Err(invalid(format!("{at}.args must be an object"))),
    };
    check_refs(&args, defined).map_err(|e| invalid(format!("{at}.args: {}", e.message)))?;
    if let Some(spec) = super::step_registry().spec(tool) {
        check_args(&spec.input_schema, &args).map_err(|e| invalid(format!("{at}.args: {e}")))?;
    }
    let label = match obj.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if label_ok(s) => {
            if defined.contains(s) {
                return Err(invalid(format!("{at}.label {s:?} is used twice")));
            }
            Some(s.clone())
        }
        Some(_) => {
            return Err(invalid(format!(
                "{at}.label must match [A-Za-z0-9_-]{{1,64}}"
            )))
        }
    };
    let expect = parse_assertions(obj.get("expect"), &at, true, defined)?;
    if let Some(l) = &label {
        defined.insert(l.clone());
    }
    Ok(Step {
        tool,
        args,
        label,
        expect,
    })
}

fn parse_assertions(
    v: Option<&Value>,
    at: &str,
    in_step: bool,
    defined: &HashSet<String>,
) -> Result<Vec<Assertion>, ToolError> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, x)| parse_assertion(x, &format!("{at}.expect[{i}]"), in_step, defined))
            .collect(),
        Some(_) => Err(invalid(format!(
            "{at}.expect must be an array of assertions"
        ))),
    }
}

fn parse_assertion(
    v: &Value,
    at: &str,
    in_step: bool,
    defined: &HashSet<String>,
) -> Result<Assertion, ToolError> {
    let obj = v
        .as_object()
        .ok_or_else(|| invalid(format!("{at} must be an object {{path, op, value}}")))?;
    reject_unknown(obj, ASSERTION_KEYS, at)?;
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{at}.path must be a string")))?;
    let segs = segments(path).ok_or_else(|| {
        invalid(format!(
            "{at}.path {path:?} must be dot-separated [A-Za-z0-9_] segments"
        ))
    })?;
    let op_name = obj
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{at}.op must be a string")))?;
    let op = Op::parse(op_name).ok_or_else(|| {
        invalid(format!(
            "{at}.op {op_name:?} must be one of eq, ne, lt, le, gt, ge, approx, contains, matches"
        ))
    })?;
    let value = obj
        .get("value")
        .cloned()
        .ok_or_else(|| invalid(format!("{at}.value is required")))?;
    check_refs(&value, defined).map_err(|e| invalid(format!("{at}.value: {}", e.message)))?;
    let literal = !is_ref(&value);
    match op {
        Op::Lt | Op::Le | Op::Gt | Op::Ge if literal && !value.is_number() => {
            return Err(invalid(format!("{at}: {} needs a number value", op.name())))
        }
        Op::Approx if literal && !numeric_like(&value) => {
            return Err(invalid(format!(
                "{at}: approx needs a number or an array of numbers"
            )))
        }
        Op::Matches => match value.as_str() {
            Some(re) if literal => {
                Regex::new(re).map_err(|e| invalid(format!("{at}.value is not a regex: {e}")))?;
            }
            None if literal => return Err(invalid(format!("{at}: matches needs a string regex"))),
            _ => {}
        },
        _ => {}
    }
    let tolerance = match obj.get("tolerance") {
        None | Some(Value::Null) => DEFAULT_TOLERANCE,
        Some(t) => {
            if op != Op::Approx {
                return Err(invalid(format!("{at}.tolerance is only for approx")));
            }
            t.as_f64()
                .filter(|x| x.is_finite() && *x >= 0.0)
                .ok_or_else(|| invalid(format!("{at}.tolerance must be a number >= 0")))?
        }
    };
    let from = match obj.get("from").and_then(Value::as_str) {
        None if in_step => Source::Result,
        None => Source::GameState,
        Some("result") if in_step => Source::Result,
        Some("result") => {
            return Err(invalid(format!(
                "{at}.from \"result\" is only for step expects"
            )))
        }
        Some("game_state") => Source::GameState,
        Some("weapons") => Source::Weapons,
        Some(other) => {
            return Err(invalid(format!(
                "{at}.from {other:?} must be result, game_state or weapons"
            )))
        }
    };
    Ok(Assertion {
        path: path.to_string(),
        segments: segs,
        op,
        value,
        tolerance,
        from,
    })
}

fn numeric_like(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::Array(a) => !a.is_empty() && a.iter().all(Value::is_number),
        _ => false,
    }
}

/// True when some assertion of `expect` reads the top-level `key` (an explicit verdict on it).
pub fn asserts_on(expect: &[Assertion], key: &str) -> bool {
    expect
        .iter()
        .any(|a| a.segments.first().is_some_and(|s| s == key))
}

/// Shallow argument check against a tool's input schema: required keys, unknown keys when the schema
/// sets `additionalProperties: false`, and per-key `type`, `enum`, `minimum` and `maximum`. `$ref`
/// values are skipped (they are resolved at run time). Schemas without `properties` accept anything.
pub fn check_args(schema: &Value, args: &Value) -> Result<(), String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "args must be an object".to_string())?;
    if let Some(req) = schema.get("required").and_then(Value::as_array) {
        for k in req.iter().filter_map(Value::as_str) {
            if !obj.contains_key(k) {
                return Err(format!("missing required arg {k:?}"));
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    if let Some(p) = props {
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            if let Some(k) = obj.keys().find(|k| !p.contains_key(*k)) {
                return Err(format!("unknown arg {k:?}"));
            }
        }
        for (k, v) in obj {
            if is_ref(v) {
                continue;
            }
            if let Some(ps) = p.get(k) {
                check_value(ps, v).map_err(|e| format!("arg {k:?} {e}"))?;
            }
        }
    }
    Ok(())
}

fn check_value(ps: &Value, v: &Value) -> Result<(), String> {
    let types: Vec<&str> = match ps.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|t| matches_type(v, t)) {
        return Err(format!("must be {}", types.join(" or ")));
    }
    if let Some(choices) = ps.get("enum").and_then(Value::as_array) {
        if !choices.contains(v) {
            return Err(format!("must be one of {}", Value::Array(choices.clone())));
        }
    }
    if let Some(x) = v.as_f64() {
        if let Some(min) = ps.get("minimum").and_then(Value::as_f64) {
            if x < min {
                return Err(format!("must be >= {min}"));
            }
        }
        if let Some(max) = ps.get("maximum").and_then(Value::as_f64) {
            if x > max {
                return Err(format!("must be <= {max}"));
            }
        }
    }
    Ok(())
}

fn matches_type(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Rough cost of a step in ms, for the up-front budget check (not a timeout: the tool bounds itself).
pub fn estimate_ms(step: &Step) -> u64 {
    let arg = |k: &str| step.args.get(k).and_then(Value::as_u64);
    match step.tool {
        "input_sequence" => {
            let items = step.args["steps"].as_array().cloned().unwrap_or_default();
            let ticks: u64 = items
                .iter()
                .map(|s| s["holdTicks"].as_u64().unwrap_or(1))
                .sum();
            ticks * 1000 / 64 + items.len() as u64 * 50 + 500
        }
        "setpos" => 2_000,
        "round_start" => 5_000,
        "game_snapshot" => 1_000,
        "game_restore" => 3_000,
        "fire_weapon" => 1_500 * arg("shots").unwrap_or(1),
        "look_at" => 2_000,
        "use_entity" => 3_000,
        "ent_fire" | "console_exec" => 1_000,
        "console_wait" => arg("timeoutMs").unwrap_or(10_000),
        "game_wait" => arg("timeoutMs").unwrap_or(5_000),
        "demo_record_start" => 2_000,
        "demo_record_stop" => 6_000,
        _ => 300,
    }
}

/// `{"$ref": "label"}` or `{"$ref": "label.path"}` (the only key of its object).
pub fn is_ref(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|m| m.len() == 1 && m.contains_key("$ref"))
}

/// Splits a `$ref` string into its label and optional path.
fn split_ref(r: &str) -> (&str, Option<&str>) {
    match r.split_once('.') {
        Some((label, path)) => (label, Some(path)),
        None => (r, None),
    }
}

/// Static `$ref` check: a well-formed ref names a label already defined, and its path is valid.
fn check_refs(v: &Value, defined: &HashSet<String>) -> Result<(), ToolError> {
    match v {
        Value::Object(m) if m.contains_key("$ref") => {
            if m.len() != 1 {
                return Err(invalid("$ref must be the only key of its object"));
            }
            let r = m["$ref"]
                .as_str()
                .ok_or_else(|| invalid("$ref must be a string"))?;
            let (label, path) = split_ref(r);
            if !defined.contains(label) {
                return Err(invalid(format!(
                    "$ref {r:?}: no earlier step is labelled {label:?}"
                )));
            }
            if let Some(p) = path {
                if segments(p).is_none() {
                    return Err(invalid(format!("$ref {r:?}: bad path")));
                }
            }
            Ok(())
        }
        Value::Object(m) => m.values().try_for_each(|x| check_refs(x, defined)),
        Value::Array(a) => a.iter().try_for_each(|x| check_refs(x, defined)),
        _ => Ok(()),
    }
}

/// Walks `segments` through objects (by key) and arrays (by index).
pub fn lookup<'a>(v: &'a Value, segments: &[String]) -> Option<&'a Value> {
    segments.iter().try_fold(v, |cur, seg| match cur {
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        Value::Object(m) => m.get(seg),
        _ => None,
    })
}

/// Replaces every `$ref` with the value it names in `labels` (earlier step results).
pub fn resolve_refs(v: &Value, labels: &HashMap<String, Value>) -> Result<Value, String> {
    match v {
        Value::Object(m) if is_ref(v) => {
            let r = m["$ref"].as_str().unwrap_or_default();
            let (label, path) = split_ref(r);
            let base = labels.get(label).ok_or_else(|| {
                format!("$ref {r:?}: step {label:?} has no result (it failed or did not run)")
            })?;
            match path {
                None => Ok(base.clone()),
                Some(p) => {
                    let segs = segments(p).ok_or_else(|| format!("$ref {r:?}: bad path"))?;
                    lookup(base, &segs)
                        .cloned()
                        .ok_or_else(|| format!("$ref {r:?}: no such path in the result"))
                }
            }
        }
        Value::Object(m) => m
            .iter()
            .map(|(k, x)| Ok((k.clone(), resolve_refs(x, labels)?)))
            .collect::<Result<Map<String, Value>, String>>()
            .map(Value::Object),
        Value::Array(a) => a
            .iter()
            .map(|x| resolve_refs(x, labels))
            .collect::<Result<Vec<Value>, String>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defined(labels: &[&str]) -> HashSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn segments_accept_identifiers_and_indices_only() {
        assert_eq!(
            segments("player.origin.0"),
            Some(vec!["player".into(), "origin".into(), "0".into()])
        );
        assert_eq!(segments(""), None);
        assert_eq!(segments("a..b"), None);
        assert_eq!(segments("a.b c"), None);
        assert_eq!(segments("a.[0]"), None);
    }

    #[test]
    fn lookup_walks_objects_and_arrays() {
        let v = json!({"player": {"origin": [1.0, 2.0, 3.0]}});
        let segs = segments("player.origin.1").unwrap();
        assert_eq!(lookup(&v, &segs), Some(&json!(2.0)));
        assert_eq!(lookup(&v, &segments("player.missing").unwrap()), None);
    }

    #[test]
    fn refs_resolve_against_earlier_labels_only() {
        let mut labels = HashMap::new();
        labels.insert("snap".to_string(), json!({"player": {"origin": [5, 6, 7]}}));
        let v = json!({"origin": {"$ref": "snap.player.origin"}, "n": [{"$ref": "snap"}]});
        let out = resolve_refs(&v, &labels).unwrap();
        assert_eq!(out["origin"], json!([5, 6, 7]));
        assert_eq!(out["n"][0], labels["snap"]);
        let err = resolve_refs(&json!({"$ref": "snap.nope"}), &labels).unwrap_err();
        assert!(err.contains("no such path"), "{err}");
        let err = resolve_refs(&json!({"$ref": "other.x"}), &labels).unwrap_err();
        assert!(err.contains("has no result"), "{err}");
        assert!(check_refs(&json!({"$ref": "later"}), &defined(&["snap"])).is_err());
        assert!(check_refs(&json!({"$ref": "snap.a..b"}), &defined(&["snap"])).is_err());
        assert!(check_refs(&json!({"$ref": "snap", "x": 1}), &defined(&["snap"])).is_err());
    }

    #[test]
    fn name_and_label_charsets() {
        assert!(name_ok("smartprop_zoo"));
        assert!(!name_ok("_x"));
        assert!(!name_ok("a b"));
        assert!(!name_ok(&"a".repeat(65)));
        assert!(label_ok("snap-1"));
        assert!(!label_ok("snap.1"));
        assert!(!label_ok(""));
    }
}
