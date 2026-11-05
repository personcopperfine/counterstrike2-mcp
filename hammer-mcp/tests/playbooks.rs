//! Playbooks (streamline P4): the texts are short, every tool name and argument key in them is real
//! (checked against the registry, like `instructions.rs`), `get_playbook` and `prompts/get` return the
//! same text, and `prompts/list` / `prompts/get` answer over the real stdio transport.
mod common;

use cs2_mcp_common::tools::Registry;
use hammer_mcp::playbooks::{playbook_index, playbook_text, prompt_set, MAX_PLAYBOOK_CHARS};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// snake_case words in the texts that are not tools: entity classes, events, keyvalues, play_test
/// step kinds, report_ready checks, cvars, files and the playbook names themselves.
const NOT_TOOLS: &[&str] = &[
    "logic_auto", "logic_eventlistener", "round_freeze_end", "func_movelinear", "point_script",
    "info_particle_system", "info_player_counterterrorist", "info_player_terrorist", "effect_name",
    "start_active", "cs_script", "wait_entity", "expect_entity", "wait_log", "loose_compiled",
    "debug_patterns", "test_cvars", "no_metamod_load", "spawn_in_mover_path", "sv_cheats",
    "blockout_map", "iterate_loop", "particle_set", "set_field", "set_root_field", "add_function", "add_child", "player_eye", "csgo_addons", "vjs_c", "vpcf_c",
    "import_model", "prop_materials", "launch_session", "console_exec", "hammer_mesh", "prop_dynamic",
];

fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn texts() -> Vec<(&'static str, String)> {
    let with = args(&[("map", "maps/arena.vmap"), ("script", "myscript"), ("folder", "particles/myset"), ("source", "props/crate.glb")]);
    let mut out: Vec<(&'static str, String)> = playbook_index()
        .into_iter()
        .flat_map(|(name, ..)| {
            [
                (name, playbook_text(name, &BTreeMap::new()).unwrap()),
                (name, playbook_text(name, &with).unwrap()),
            ]
        })
        .collect();
    // import_model has one body per kind: check each
    for kind in ["prop", "world", "hammer_mesh", "player"] {
        let mut a = with.clone();
        a.insert("kind".into(), kind.into());
        out.push(("import_model", playbook_text("import_model", &a).unwrap()));
    }
    out
}

#[test]
fn there_are_six_playbooks_and_each_is_short() {
    let names: Vec<&str> = playbook_index().into_iter().map(|(n, ..)| n).collect();
    assert_eq!(names, ["blockout_map", "iterate_loop", "cs_script", "particle_set", "release", "import_model"]);
    for (name, text) in texts() {
        let n = text.chars().count();
        assert!(n <= MAX_PLAYBOOK_CHARS, "{name} is {n} chars (cap {MAX_PLAYBOOK_CHARS})");
        for marker in ["<MAP>", "<SCRIPT>", "<FOLDER>", "<SOURCE>", "<KIND>", "<PROPKIND>"] {
            assert!(!text.contains(marker), "{name}: unfilled {marker}");
        }
        // definition of done and the step-outside rule are in every one
        assert!(text.contains("Done ="), "{name} lacks the definition of done");
        assert!(text.contains("report_ready"), "{name} lacks report_ready");
        assert!(text.contains("step outside these tools") || text.contains("stepped outside these tools"), "{name} lacks the step-outside rule");
    }
}

#[test]
fn import_model_walks_the_real_chain_per_kind() {
    let text = |kind: &str| playbook_text("import_model", &args(&[("source", "a/b.glb"), ("kind", kind)])).unwrap();
    for kind in ["prop", "world", "hammer_mesh", "player"] {
        let t = text(kind);
        assert!(t.contains(&format!("kind:\"{kind}\", dryRun:true")) && t.contains("source:\"a/b.glb\""), "{kind}");
        // the engine model cache rule is in every kind
        assert!(t.contains("restart the tools session") && t.contains("recompile the map"), "{kind}");
        assert!(t.contains("save_map") && t.contains("compile_map") && t.contains("look"), "{kind}");
    }
    assert!(text("world").contains("kind:\"static\"") && text("prop").contains("kind:\"dynamic\""));
    assert!(text("hammer_mesh").contains("delete_nodes") && !text("hammer_mesh").contains("create_props"));
    let player = text("player");
    for part in ["player_model {action:\"set\"", "launch_session {kind:\"wine-tools\"", "sv_cheats 1; thirdperson", "run_map does NOT show it"] {
        assert!(player.contains(part), "{part}");
    }
    // no kind: the default is prop and the other kinds are named
    let default = playbook_text("import_model", &BTreeMap::new()).unwrap();
    assert!(default.contains("kind:\"prop\"") && default.contains("hammer_mesh (Hammer meshes"), "{default}");
    // an unknown kind never reaches the text
    assert!(playbook_text("import_model", &args(&[("kind", "rigged")])).unwrap().contains("kind:\"prop\""));
}

#[test]
fn release_checks_are_never_a_loop_on_the_verdict() {
    // report_ready release is always `incomplete` (no_metamod_load is skipped by design), so the
    // playbooks that run it must name that skip and must not say "until pass".
    for name in ["release", "cs_script", "particle_set"] {
        let t = playbook_text(name, &BTreeMap::new()).unwrap();
        assert!(t.contains("no_metamod_load") && t.contains("skipped"), "{name}");
        assert!(!t.contains("until it pass") && !t.contains("until pass") && !t.contains("until nothing fails"), "{name}");
    }
}

#[test]
fn arguments_are_substituted() {
    let t = playbook_text("release", &args(&[("map", "maps/x.vmap")])).unwrap();
    assert!(t.contains("maps/x.vmap") && !t.contains("blockout.vmap"));
    let t = playbook_text("particle_set", &args(&[("folder", "particles/fx/")])).unwrap();
    assert!(t.contains("particles/fx/") || t.contains("particles/fx"), "{t}");
    assert!(!t.contains("particles/fx//"));
    assert!(playbook_text("nope", &BTreeMap::new()).is_none());
}

#[test]
fn every_snake_word_is_a_registered_tool_or_a_known_non_tool() {
    let registry = hammer_mcp::build_registry();
    let names = registry.names();
    for (name, text) in texts() {
        let unknown: Vec<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| w.contains('_') && w.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()))
            .filter(|w| !NOT_TOOLS.contains(w) && !names.contains(w))
            .collect();
        assert!(unknown.is_empty(), "{name} names things that are not tools: {unknown:?}");
    }
}

/// A parsed snippet value: `None` is a placeholder (a bare word, `[...]` content we cannot know).
/// `incomplete` objects carry a `...` entry, so their required keys are not checked.
#[derive(Debug)]
enum Snip {
    Placeholder,
    Lit(Value),
    Array(Vec<Snip>),
    Object { entries: Vec<(String, Snip)>, incomplete: bool },
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn string(&mut self) -> String {
        self.i += 1;
        let start = self.i;
        while self.peek().is_some_and(|c| c != b'"') {
            self.i += 1;
        }
        let out = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        out
    }
    fn value(&mut self) -> Snip {
        self.ws();
        match self.peek() {
            Some(b'"') => Snip::Lit(Value::String(self.string())),
            Some(b'{') => {
                self.i += 1;
                let (mut entries, mut incomplete) = (Vec::new(), false);
                loop {
                    self.ws();
                    match self.peek() {
                        Some(b'}') | None => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        _ => {
                            if self.s[self.i..].starts_with(b"...") {
                                incomplete = true;
                                self.i += 3;
                                continue;
                            }
                            let key = if self.peek() == Some(b'"') {
                                self.string()
                            } else {
                                let start = self.i;
                                while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$') {
                                    self.i += 1;
                                }
                                String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
                            };
                            if key.is_empty() {
                                self.i += 1; // not a key: skip the byte so the loop always advances
                                continue;
                            }
                            self.ws();
                            let v = if self.peek() == Some(b':') {
                                self.i += 1;
                                self.value()
                            } else {
                                Snip::Placeholder
                            };
                            entries.push((key, v));
                        }
                    }
                }
                Snip::Object { entries, incomplete }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    match self.peek() {
                        Some(b']') | None => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        _ if self.s[self.i..].starts_with(b"...") => self.i += 3,
                        _ => {
                            let before = self.i;
                            items.push(self.value());
                            if self.i == before {
                                self.i += 1;
                            }
                        }
                    }
                }
                Snip::Array(items)
            }
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| !matches!(c, b',' | b'}' | b']')) {
                    self.i += 1;
                }
                let word = String::from_utf8_lossy(&self.s[start..self.i]).trim().to_string();
                match serde_json::from_str::<Value>(&word) {
                    Ok(v) => Snip::Lit(v),
                    Err(_) => Snip::Placeholder,
                }
            }
        }
    }
}

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
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

/// Validate a snippet against a JSON schema (the subset the tools use: type, enum, properties,
/// required, additionalProperties, items, minimum/maximum, min/maxItems). Placeholders pass.
fn validate(path: &str, schema: &Value, snip: &Snip, out: &mut Vec<String>) {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    match snip {
        Snip::Placeholder => {}
        Snip::Lit(v) => {
            if !types.is_empty() && !types.iter().any(|t| type_ok(t, v)) {
                out.push(format!("{path}: {v} is not of type {types:?}"));
            }
            if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
                if !allowed.contains(v) {
                    out.push(format!("{path}: {v} is not one of {allowed:?}"));
                }
            }
            if let (Some(n), Some(min)) = (v.as_f64(), schema.get("minimum").and_then(Value::as_f64)) {
                if n < min {
                    out.push(format!("{path}: {n} below minimum {min}"));
                }
            }
            if let (Some(n), Some(max)) = (v.as_f64(), schema.get("maximum").and_then(Value::as_f64)) {
                if n > max {
                    out.push(format!("{path}: {n} above maximum {max}"));
                }
            }
        }
        Snip::Array(items) => {
            if !types.is_empty() && !types.contains(&"array") {
                out.push(format!("{path}: an array where {types:?} is expected"));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if items.len() as u64 > max {
                    out.push(format!("{path}: {} items, max {max}", items.len()));
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    validate(&format!("{path}[{i}]"), item_schema, item, out);
                }
            }
        }
        Snip::Object { entries, incomplete } => {
            if !types.is_empty() && !types.contains(&"object") {
                out.push(format!("{path}: an object where {types:?} is expected"));
                return;
            }
            let props = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required").and_then(Value::as_array).filter(|_| !incomplete) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !entries.iter().any(|(k, _)| k == key) {
                        out.push(format!("{path}: required key {key:?} is missing"));
                    }
                }
            }
            for (key, value) in entries {
                match props.and_then(|p| p.get(key)) {
                    Some(prop) => validate(&format!("{path}.{key}"), prop, value, out),
                    None => {
                        let closed = schema.get("additionalProperties") == Some(&json!(false));
                        if closed && props.is_some_and(|p| !p.is_empty()) {
                            out.push(format!("{path}: {key:?} is not an argument"));
                        } else if let Some(extra) = schema.get("additionalProperties").filter(|e| e.is_object()) {
                            validate(&format!("{path}.{key}"), extra, value, out);
                        }
                    }
                }
            }
        }
    }
}

/// The index just past the `}` that closes the `{` at `open` (strings respected).
fn closing(text: &str, open: usize) -> Option<usize> {
    let (mut depth, mut in_str) = (0i32, false);
    for (i, c) in text[open..].char_indices() {
        match c {
            '"' => in_str = !in_str,
            '{' | '[' if !in_str => depth += 1,
            '}' | ']' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Check every `tool {...}` call in `text` against the tool's full input schema. Returns the
/// problems and the number of calls checked.
fn check_calls(registry: &Registry<hammer_mcp::state::HammerState>, text: &str) -> (Vec<String>, usize) {
    let (mut problems, mut checked) = (Vec::new(), 0);
    for tool in registry.names() {
        let needle = format!("{tool} {{");
        for (at, _) in text.match_indices(&needle) {
            if text[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let open = at + needle.len() - 1;
            let Some(end) = closing(text, open) else { continue };
            let schema = &registry.spec(tool).unwrap().input_schema;
            if schema.get("properties").and_then(Value::as_object).is_none_or(|p| p.is_empty()) {
                continue;
            }
            let snip = P { s: &text.as_bytes()[open..end], i: 0 }.value();
            checked += 1;
            let mut found = Vec::new();
            validate(tool, schema, &snip, &mut found);
            problems.extend(found);
            // `look` frames needs something to look at (a target or a pose)
            if tool == "look" {
                if let Snip::Object { entries, .. } = &snip {
                    let get = |k: &str| entries.iter().any(|(e, _)| e == k);
                    let frames = entries.iter().any(|(k, v)| k == "mode" && matches!(v, Snip::Lit(Value::String(s)) if s == "frames"));
                    if frames && !get("target") && !get("pose") {
                        problems.push("look frames needs a target or a pose".to_string());
                    }
                }
            }
        }
    }
    (problems, checked)
}

#[test]
fn every_call_matches_the_full_tool_schema() {
    let registry = hammer_mcp::build_registry();
    let (mut problems, mut checked) = (Vec::new(), 0);
    for (name, text) in texts() {
        let (p, c) = check_calls(&registry, &text);
        problems.extend(p.into_iter().map(|p| format!("{name}: {p}")));
        checked += c;
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(checked > 50, "the parser should see many calls, saw {checked}");
}

#[test]
fn the_checker_catches_wrong_keys_enums_and_missing_required_parts() {
    let registry = hammer_mcp::build_registry();
    let bad = [
        ("compile_map {jobid, mode:\"fast\"}", 2),
        ("look {mode:\"nope\"}", 1),
        // required `origin` missing in a create_entities item
        ("create_entities {entities:[{class:\"logic_eventlistener\"}]}", 1),
        ("tie_to_entity {meshNodeIDs:[1], class:\"func_movelinear\", keyValues:{targetname:\"m\"}}", 0),
        ("add_entity_connections {nodeID:1, connections:[{output:\"a\", target:\"b\"}]}", 1),
        ("look {mode:\"frames\", count:4}", 1),
        ("look {mode:\"frames\", target:\"mover\"}", 0),
        ("look {mode:\"frames\", pose:{origin:[0,0,0], angles:[0,0,0]}}", 0),
        ("report_ready {mode:\"capture\"}", 1),
    ];
    for (text, expected) in bad {
        let (p, _) = check_calls(&registry, text);
        assert_eq!(p.len(), expected, "{text}: {p:?}");
    }
}

#[test]
fn no_concrete_map_path_is_invented() {
    for (name, ..) in playbook_index() {
        let t = playbook_text(name, &BTreeMap::new()).unwrap();
        assert!(!t.contains("blockout.vmap"), "{name} invents a map path");
        // every path:"maps/..." / map:"maps/..." in the default text is a placeholder, not a file
        assert!(!t.contains("path:\"maps/") || t.contains("maps/<name>.vmap"), "{name}: {t}");
        assert!(!t.contains("map:\""), "{name}: report_ready names a map the user never gave");
    }
    let given = playbook_text("release", &args(&[("map", "maps/x.vmap")])).unwrap();
    assert!(given.contains("path:\"maps/x.vmap\"") && given.contains("map:\"maps/x.vmap\""));
}

#[test]
fn prompts_and_get_playbook_return_the_same_text() {
    let set = prompt_set();
    assert_eq!(set.names(), playbook_index().into_iter().map(|(n, ..)| n).collect::<Vec<_>>());
    let a = args(&[("map", "maps/q.vmap")]);
    for (name, ..) in playbook_index() {
        assert_eq!(set.render(name, &a).unwrap(), playbook_text(name, &a).unwrap());
    }
    assert!(set.render("nope", &a).is_err());
}

#[tokio::test]
async fn get_playbook_lists_returns_and_rejects() {
    let mb = cs2_mcp_common::mock::MockBridge::start().await.unwrap();
    let mut s = cs2_mcp_common::mock::stdio::Server::spawn(
        env!("CARGO_BIN_EXE_hammer-mcp"),
        &common::hermetic(&["--discovery", mb.discovery_path.to_str().unwrap()]),
    )
    .await;
    let init = s
        .request(1, "initialize", json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "test", "version": "0"}, "capabilities": {}}))
        .await;
    assert!(init["result"]["capabilities"]["prompts"].is_object(), "{init}");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("get_playbook"));
    s.notify("notifications/initialized", json!({})).await;

    // prompts/list over the real transport
    let list = s.request(2, "prompts/list", json!({})).await;
    let prompts = list["result"]["prompts"].as_array().unwrap_or_else(|| panic!("{list}"));
    let names: Vec<&str> = prompts.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["blockout_map", "iterate_loop", "cs_script", "particle_set", "release", "import_model"]);
    assert!(prompts.iter().all(|p| p["description"].as_str().is_some_and(|d| !d.is_empty())));
    assert_eq!(prompts[0]["arguments"][0]["name"], json!("map"));

    // prompts/get with an argument
    let get = s.request(3, "prompts/get", json!({"name": "release", "arguments": {"map": "maps/zz.vmap"}})).await;
    let text = get["result"]["messages"][0]["content"]["text"].as_str().unwrap_or_else(|| panic!("{get}"));
    assert_eq!(get["result"]["messages"][0]["role"], json!("user"));
    assert!(text.contains("maps/zz.vmap") && text.starts_with("Playbook release"), "{text}");
    let unknown = s.request(4, "prompts/get", json!({"name": "nope"})).await;
    assert_eq!(unknown["error"]["code"], json!(-32602), "{unknown}");

    // the tool: list, then the same text
    let call = |id: i64, args: Value| json!({"name": "get_playbook", "arguments": args, "id": id});
    let r = s.request(5, "tools/call", call(5, json!({}))).await;
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["playbooks"].as_array().unwrap().len(), 6, "{body}");
    let r = s.request(6, "tools/call", call(6, json!({"name": "release", "map": "maps/zz.vmap"}))).await;
    let body: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["playbook"].as_str().unwrap(), text);
    let r = s.request(7, "tools/call", call(7, json!({"name": "releas"}))).await;
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    let err: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(err["error"]["type"], json!("UnknownPlaybook"));
    assert_eq!(err["error"]["data"]["didYouMean"], json!(["release"]), "{err}");
}
