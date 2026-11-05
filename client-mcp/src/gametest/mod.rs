//! `game_test` (declarative test run) and `game_test_results` (stored results as JSON or JUnit).
//!
//! * `spec.rs`: the spec grammar and validation (allowed step tools, assertion ops, paths, `$ref`s).
//!   Validation happens before any step is dispatched.
//! * `run.rs`: the runner. Steps go through the same tool registry the MCP server serves
//!   ([`step_registry`]), each with a slice of this call's budget; a spent budget yields `pending`.
//! * `junit.rs`: JUnit XML for stored results.
//!
//! Example (`assets/gametests/zoo_smoke.json`, the spec under `{"spec": ...}`):
//! `round_start`, `setpos`, `game_snapshot` in setup; `input_sequence` (moved >= 100 u), `fire_weapon`
//! (clip1Delta -1) and `game_restore` (restored true) as steps.
pub mod junit;
pub mod run;
pub mod spec;

use crate::state::ClientState;
use cs2_mcp_common::tools::{CallCtx, Registry, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::OnceLock;

/// Stored results per process (oldest dropped first; a name replaces its earlier result).
pub const STORE_CAP: usize = 32;

/// The registry steps dispatch through: the same registration `tools::client::register` makes for
/// the server, built once. Only [`spec::STEP_TOOLS`] are reachable from a spec.
pub fn step_registry() -> &'static Registry<ClientState> {
    static REGISTRY: OnceLock<Registry<ClientState>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = Registry::new();
        crate::tools::client::register(&mut r);
        r
    })
}

/// Results of this process's `game_test` runs.
#[derive(Debug, Default)]
pub struct GameTestStore {
    results: VecDeque<Value>,
}

impl GameTestStore {
    pub fn put(&mut self, result: Value) {
        let name = result["name"].as_str().unwrap_or_default().to_string();
        self.results
            .retain(|r| r["name"].as_str() != Some(name.as_str()));
        self.results.push_back(result);
        while self.results.len() > STORE_CAP {
            self.results.pop_front();
        }
    }

    /// Every stored result, or only the one with `name`.
    pub fn get(&self, name: Option<&str>) -> Vec<Value> {
        self.results
            .iter()
            .filter(|r| match name {
                None => true,
                Some(n) => r["name"].as_str() == Some(n),
            })
            .cloned()
            .collect()
    }
}

pub fn game_test_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "description": "see the tool description",
                "required": ["name", "steps"],
                "properties": {
                    "name": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$"},
                    "map": {"type": "string"},
                    "addon": {"type": "string"},
                    "setup": {"type": "array", "items": {"type": "object"}},
                    "steps": {"type": "array", "minItems": 1, "items": {"type": "object"}},
                    "expect": {"type": "array", "items": {"type": "object"}},
                    "teardown": {"type": "array", "items": {"type": "object"}},
                    "timeoutMs": {"type": "integer", "minimum": 1, "maximum": spec::MAX_TIMEOUT_MS, "default": spec::DEFAULT_TIMEOUT_MS},
                    "demo": {"type": "boolean", "default": false},
                    "continueOnFailure": {"type": "boolean", "default": false},
                    "verbose": {"type": "boolean", "default": true, "description": "false: passing steps carry no result"}
                }
            }
        },
        "required": ["spec"],
        "additionalProperties": false
    })
}

/// `game_test {spec}`: validates, runs, stores and returns the result (a failed test is a result,
/// not an error; an invalid spec is InvalidParams and sends nothing).
pub async fn game_test(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let spec = spec::parse(&args)?;
    let result = run::run(&state, &ctx.budget, spec).await?;
    store(&state).put(result.clone());
    Ok(result.into())
}

/// `game_test_results {name?, format: "json"|"junit"}`: stored results (reads the store only).
pub async fn game_test_results(
    state: ClientState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let format = match args.get("format") {
        None | Some(Value::Null) => "json",
        Some(Value::String(f)) if f == "json" || f == "junit" => f.as_str(),
        Some(_) => {
            return Err(ToolError::invalid_arguments(
                "format must be \"json\" or \"junit\"",
            ))
        }
    };
    let name = match args.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) => Some(n.as_str()),
        Some(_) => return Err(ToolError::invalid_arguments("name must be a string")),
    };
    let results = store(&state).get(name);
    if let Some(n) = name {
        if results.is_empty() {
            return Err(ToolError::new(
                "NotFound",
                "NotFound",
                format!("no stored game_test result named {n:?}"),
            )
            .with_data(json!({"name": n})));
        }
    }
    if format == "junit" {
        return Ok(json!({"format": "junit", "xml": junit::render(&results)}).into());
    }
    Ok(json!({"format": "json", "results": results}).into())
}

fn store(state: &ClientState) -> std::sync::MutexGuard<'_, GameTestStore> {
    state
        .gametests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_replaces_by_name_and_is_bounded() {
        let mut s = GameTestStore::default();
        s.put(json!({"name": "a", "passed": false}));
        s.put(json!({"name": "a", "passed": true}));
        assert_eq!(s.get(None).len(), 1);
        assert_eq!(s.get(Some("a"))[0]["passed"], json!(true));
        for i in 0..(STORE_CAP + 5) {
            s.put(json!({"name": format!("t{i}")}));
        }
        let all = s.get(None);
        assert_eq!(all.len(), STORE_CAP);
        assert_eq!(all[0]["name"], json!("t5"));
    }

    #[test]
    fn every_step_tool_is_registered() {
        let names = step_registry().names();
        for t in spec::STEP_TOOLS {
            assert!(names.contains(t), "{t} is not registered");
        }
        assert!(names.contains(&"input_clear"));
        assert!(names.contains(&"launch_session"));
    }

    #[test]
    fn the_shipped_example_is_a_valid_spec() {
        let text = include_str!("../../assets/gametests/zoo_smoke.json");
        let spec_value: Value = serde_json::from_str(text).expect("example is JSON");
        let parsed = spec::parse(&json!({"spec": spec_value})).expect("example validates");
        assert_eq!(parsed.name, "smartprop_zoo");
        assert_eq!(parsed.steps.len(), 3);
    }
}
