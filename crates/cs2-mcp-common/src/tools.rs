//! Tool registry: name -> (JSON schema, policy flags, handler). Transport-agnostic.
//!
//! Policies enforced here, not in individual tools:
//! * **capability gating**: a tool whose declared bridge methods are not advertised returns
//!   `NotImplemented` with `data.missing` without sending anything (a dead bridge is reported as
//!   `BridgeNotRunning`, never as a missing capability);
//! * **per-call budget**: handlers derive every bridge `deadlineMs` from [`Budget`] so one tool call
//!   finishes inside typical MCP client timeouts;
//! * **no raw bridge handles** in results or error payloads (`handle`, `nodeHandle`, `node_handle`);
//! * **forbidden methods** can never be declared by a tool ([`FORBIDDEN_BRIDGE_METHODS`]).
//!
//! Mutating tools never save; writers (one per document kind plus explicit file writers) are
//! marked [`Mutability::Writer`].

use crate::bridge::{BridgeError, SharedBridge};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    ReadOnly,
    /// mutates the live document/game state; never writes files
    Mutating,
    /// writes a file: one per document kind (save) plus explicit file writers
    Writer,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub mutability: Mutability,
    /// bridge methods this tool dispatches (for capability gating); empty = runtime/file only
    pub bridge_methods: &'static [&'static str],
    /// MCP `idempotentHint` override (`None` = leave it out); see [`ToolSpec::idempotent`]
    pub idempotent: Option<bool>,
    /// MCP `destructiveHint` override (`None` = derived from [`Mutability`])
    pub destructive: Option<bool>,
    /// MCP `readOnlyHint` override (`None` = derived from [`Mutability`]): a tool that looks
    /// read-only by name but writes (or the reverse) says so here
    pub read_only: Option<bool>,
    /// the steps a successful call suggests when its handler did not set any
    /// ([`ToolOutput::next`]); see [`ToolSpec::then`]
    pub next_on_success: Vec<NextStep>,
}

impl ToolSpec {
    pub fn new(
        name: &'static str,
        description: &'static str,
        mutability: Mutability,
        bridge_methods: &'static [&'static str],
    ) -> Self {
        ToolSpec {
            name,
            description,
            input_schema: open_object_schema(),
            mutability,
            bridge_methods,
            idempotent: None,
            destructive: None,
            read_only: None,
            next_on_success: Vec::new(),
        }
    }
    /// Per-tool `idempotentHint` (calling again with the same arguments changes nothing more).
    pub fn idempotent(mut self, yes: bool) -> Self {
        self.idempotent = Some(yes);
        self
    }
    /// Per-tool `destructiveHint` (discards unsaved work or overwrites state that cannot be undone),
    /// overriding what the [`Mutability`] implies.
    pub fn destructive(mut self, yes: bool) -> Self {
        self.destructive = Some(yes);
        self
    }
    /// Per-tool `readOnlyHint` override.
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = Some(yes);
        self
    }
    /// A step to suggest after every successful call of this tool (a handler that sets its own
    /// `next` wins). The registry writes it into the result as `"next"`.
    pub fn then(mut self, tool: &str, why: &str) -> Self {
        self.next_on_success.push(NextStep::new(tool, why));
        self
    }
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.input_schema = schema;
        self
    }
}

/// `{"type":"object"}` accepting any properties (tools validate arguments themselves).
pub fn open_object_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": true})
}

/// Bridge methods that are never exposed as tools (POC decision, carried over). `job.cancel` left
/// the list when the bridge got a real job model (`job.start`/`job.status`/`job.cancel`, first kind
/// `resourcecompiler`): hammer-mcp's `job_cancel` and `compile_map {cancel}` end such jobs. `selection.get`/
/// `selection.set` left it with hammer-mcp's live `get_selection`/`set_selection`.
pub const FORBIDDEN_BRIDGE_METHODS: &[&str] = &[
    "console.command",
    "undo.redo",
    "material.assign",
    "entity.delete",
    "control.shutdown",
    "game.saveRestore",
];

/// A tool failure with the same payload shape as bridge errors (`{type, name, code, wireName,
/// message, data, retrySafe, outcomeUnknown}`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{name}: {message}")]
pub struct ToolError {
    pub type_: String,
    pub name: String,
    pub code: Option<i64>,
    pub wire_name: Option<String>,
    pub message: String,
    pub data: Map<String, Value>,
    pub retry_safe: bool,
    pub outcome_unknown: bool,
}

impl ToolError {
    pub fn new(
        type_: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        ToolError {
            type_: type_.into(),
            name: name.into(),
            code: None,
            wire_name: None,
            message: message.into(),
            data: Map::new(),
            retry_safe: false,
            outcome_unknown: false,
        }
    }
    pub fn with_code(mut self, code: i64) -> Self {
        self.code = Some(code);
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        if let Value::Object(m) = data {
            self.data.extend(m);
        }
        self
    }
    pub fn retry_safe(mut self, yes: bool) -> Self {
        self.retry_safe = yes;
        self
    }
    /// Standard `data.didYouMean`: close matches for a wrong name (a tool, model, entity class,
    /// soundevent...). Left out when there are none, so its presence always means "try these".
    pub fn did_you_mean<I, T>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let names: Vec<String> = names.into_iter().map(Into::into).collect();
        if !names.is_empty() {
            self.data.insert("didYouMean".into(), json!(names));
        }
        self
    }
    /// Standard `data.useInstead`: the tool that does what the caller was trying to do.
    pub fn use_instead(mut self, tool: &str, why: &str, args: Option<Value>) -> Self {
        self.data
            .insert("useInstead".into(), NextStep::new(tool, why).args_opt(args).to_json());
        self
    }
    /// Argument validation failure (nothing dispatched).
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        ToolError::new("InvalidArguments", "InvalidParams", message)
            .with_code(-32602)
            .retry_safe(true)
    }
    /// A documented stub / a capability the bridge does not advertise (nothing dispatched).
    pub fn not_implemented(type_: &str, message: impl Into<String>, data: Value) -> Self {
        ToolError::new(type_, "NotImplemented", message)
            .with_code(-32011)
            .retry_safe(true)
            .with_data(data)
    }
    /// The per-call budget ran out before the next request was dispatched (nothing dispatched).
    pub fn budget_exceeded(budget: &Budget) -> Self {
        ToolError::new(
            "ToolBudgetExceeded",
            "ToolBudgetExceeded",
            format!(
                "tool budget of {} ms exhausted before the next bridge request",
                budget.total.as_millis()
            ),
        )
        .retry_safe(true)
        .with_data(json!({"toolBudgetMs": budget.total.as_millis() as u64}))
    }
    pub fn internal(message: impl Into<String>) -> Self {
        ToolError::new("InternalError", "InternalError", message)
    }
    pub fn to_json(&self) -> Value {
        strip_handles(json!({
            "type": self.type_, "name": self.name, "code": self.code, "wireName": self.wire_name,
            "message": self.message, "data": Value::Object(self.data.clone()),
            "retrySafe": self.retry_safe, "outcomeUnknown": self.outcome_unknown,
        }))
    }
}

impl From<BridgeError> for ToolError {
    fn from(e: BridgeError) -> Self {
        ToolError {
            type_: format!("{:?}", e.kind),
            name: e.kind.name().to_string(),
            code: e.code,
            wire_name: e.wire_name.clone(),
            message: e.message.clone(),
            data: e.data.clone(),
            retry_safe: e.retry_safe(),
            outcome_unknown: e.outcome_unknown(),
        }
    }
}

/// Recursively drop raw node handles from anything returned to the client.
pub fn strip_handles(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(k, _)| !matches!(k.as_str(), "handle" | "nodeHandle" | "node_handle"))
                .map(|(k, v)| (k, strip_handles(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_handles).collect()),
        other => other,
    }
}

#[derive(Debug, Clone)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// One suggested follow-up call: `{tool, why, args?}`.
#[derive(Debug, Clone, PartialEq)]
pub struct NextStep {
    pub tool: String,
    pub why: String,
    pub args: Option<Value>,
}

impl NextStep {
    pub fn new(tool: &str, why: &str) -> Self {
        NextStep {
            tool: tool.to_string(),
            why: why.to_string(),
            args: None,
        }
    }
    pub fn args(mut self, args: Value) -> Self {
        self.args = Some(args);
        self
    }
    fn args_opt(mut self, args: Option<Value>) -> Self {
        self.args = args;
        self
    }
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("tool".into(), json!(self.tool));
        m.insert("why".into(), json!(self.why));
        if let Some(a) = &self.args {
            m.insert("args".into(), a.clone());
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub json: Value,
    pub images: Vec<Image>,
    /// What to call next (short; only steps that exist). The registry serializes a non-empty list
    /// into the result JSON as `"next":[{tool, why, args?}]`; absent when empty.
    /// `None` = the handler has no opinion (the registration-time steps apply, see
    /// [`ToolSpec::then`] and [`Registry::after_call`]); `Some(vec![])` = explicitly no steps.
    pub next: Option<Vec<NextStep>>,
}

impl ToolOutput {
    /// This call has no follow-up step, whatever was registered for the tool.
    pub fn no_next(mut self) -> Self {
        self.next = Some(Vec::new());
        self
    }
    /// One-line helper: `ToolOutput::from(json).with_next("save_map", "persist the edits")`.
    pub fn with_next(mut self, tool: &str, why: &str) -> Self {
        self.next.get_or_insert_with(Vec::new).push(NextStep::new(tool, why));
        self
    }
    /// [`with_next`](Self::with_next) with the arguments to pass.
    pub fn with_next_args(mut self, tool: &str, why: &str, args: Value) -> Self {
        self.next
            .get_or_insert_with(Vec::new)
            .push(NextStep::new(tool, why).args(args));
        self
    }
}

impl From<Value> for ToolOutput {
    fn from(json: Value) -> Self {
        ToolOutput {
            json,
            images: Vec::new(),
            next: None,
        }
    }
}

/// Per-tool-call time budget: each bridge request gets the remaining budget, capped by the
/// configured per-request deadline. Exhaustion is `ToolBudgetExceeded` (nothing dispatched).
#[derive(Debug, Clone)]
pub struct Budget {
    start: Instant,
    total: Duration,
    deadline_cap: Duration,
}

impl Budget {
    pub fn new(total_ms: u64, deadline_cap_ms: u64) -> Self {
        Budget {
            start: Instant::now(),
            total: Duration::from_millis(total_ms),
            deadline_cap: Duration::from_millis(deadline_cap_ms),
        }
    }
    pub fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.start.elapsed())
    }
    /// Deadline (ms) for the next bridge request: `min(cap, remaining)`, never 0.
    pub fn deadline_ms(&self) -> Result<u64, ToolError> {
        let ms = self.remaining().min(self.deadline_cap).as_millis() as u64;
        if ms == 0 {
            return Err(ToolError::budget_exceeded(self));
        }
        Ok(ms)
    }
    pub fn total_ms(&self) -> u64 {
        self.total.as_millis() as u64
    }
    /// A child budget of at most `ms` (and never more than remains here), starting now, with the
    /// same per-request cap. Used to run in-process sub-calls (`game_test` steps).
    pub fn slice(&self, ms: Duration) -> Budget {
        Budget {
            start: Instant::now(),
            total: ms.min(self.remaining()),
            deadline_cap: self.deadline_cap,
        }
    }
}

/// Per-call context handed to handlers.
#[derive(Debug, Clone)]
pub struct CallCtx {
    pub budget: Budget,
}

/// What a registry needs from the server state: access to the bridge for capability gating.
pub trait ToolState: Clone + Send + Sync + 'static {
    fn bridge(&self) -> &SharedBridge;

    /// Runs after the capability gate and before the handler (`spec` is the TARGET tool, aliases
    /// resolved; `args` the merged arguments). The default does nothing; hammer-mcp uses it to
    /// refresh the addon the active document belongs to before an addon-scoped tool resolves its
    /// files. `Err` refuses the call (nothing ran); `Ok(Some(note))` runs the tool and adds the
    /// note to its result as `"addonNote"`.
    fn before_call<'a>(
        &'a self,
        _spec: &'a ToolSpec,
        _args: &'a Value,
        _budget: &'a Budget,
    ) -> BoxFuture<'a, Result<Option<String>, ToolError>> {
        Box::pin(async { Ok(None) })
    }
}

pub type Handler<S> = Arc<
    dyn Fn(S, CallCtx, Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;

#[derive(Debug)]
pub enum CallFailure {
    /// not a registered tool (an MCP protocol error, not a tool result)
    UnknownTool(String),
    Tool(ToolError),
}

impl From<ToolError> for CallFailure {
    fn from(e: ToolError) -> Self {
        CallFailure::Tool(e)
    }
}

/// A deprecated name that dispatches to a new tool.
struct Alias {
    spec: ToolSpec,
    target: &'static str,
    fixed_args: Map<String, Value>,
}

/// Hook run after a successful handler, before `next` is serialized: lets a server decide the
/// registration-time steps per call from its own state.
pub type AfterCall<S> = Arc<dyn Fn(&S, &str, &mut ToolOutput) + Send + Sync>;

pub struct Registry<S: ToolState> {
    after_call: Option<AfterCall<S>>,
    strict_steps: bool,
    tools: BTreeMap<&'static str, (ToolSpec, Handler<S>)>,
    aliases: BTreeMap<&'static str, Alias>,
    order: Vec<&'static str>,
}

impl<S: ToolState> Default for Registry<S> {
    fn default() -> Self {
        Registry {
            after_call: None,
            strict_steps: false,
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            order: Vec::new(),
        }
    }
}

impl<S: ToolState> Registry<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Panics on duplicates and on forbidden bridge methods (programming errors).
    pub fn register<F, Fut>(&mut self, spec: ToolSpec, handler: F)
    where
        F: Fn(S, CallCtx, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        assert!(
            !self.tools.contains_key(spec.name) && !self.aliases.contains_key(spec.name),
            "duplicate tool {}",
            spec.name
        );
        for m in spec.bridge_methods {
            assert!(
                !FORBIDDEN_BRIDGE_METHODS.contains(m),
                "tool {} declares forbidden bridge method {m}",
                spec.name
            );
        }
        let h: Handler<S> = Arc::new(move |s, c, a| Box::pin(handler(s, c, a)));
        self.order.push(spec.name);
        self.tools.insert(spec.name, (spec, h));
    }

    /// Register a documented stub: listed, gated like a real tool, answers `NotImplemented`.
    pub fn register_stub(&mut self, spec: ToolSpec, workflow: &'static str) {
        let name = spec.name;
        self.register(spec, move |_s, _c, _a| async move {
            Err(ToolError::not_implemented(
                "NotImplementedYet",
                format!("{name} is not implemented yet"),
                json!({"blockedOn": workflow}),
            ))
        });
    }

    /// Register `old` as a deprecated alias of the already-registered tool `new`, kept for one
    /// release. A call dispatches to `new` (same capability gate, same handler) with `fixed_args`
    /// merged over the caller's arguments (**fixed args win**: they are what the old name meant),
    /// and the result gains `"deprecated":{"alias":old,"use":new}`. It is listed with a one-line
    /// description, an open object schema (no copy of the target's schema: token cost) and the
    /// target's annotations. Panics on a duplicate name, an unknown target, or an alias of an alias
    /// (programming errors).
    pub fn alias(&mut self, old: &'static str, new: &'static str, fixed_args: Value) {
        assert!(
            !self.tools.contains_key(old) && !self.aliases.contains_key(old),
            "duplicate tool {old}"
        );
        let target = &self
            .tools
            .get(new)
            .unwrap_or_else(|| panic!("alias {old}: unknown target {new}"))
            .0;
        let fixed_args = match fixed_args {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => panic!("alias {old}: fixed args must be an object, got {other}"),
        };
        // one-time, startup-only: the registry's names and descriptions are `&'static str`
        let description: &'static str = Box::leak(
            if fixed_args.is_empty() {
                format!("Deprecated: use `{new}`.")
            } else {
                format!(
                    "Deprecated: use `{new}` {}.",
                    Value::Object(fixed_args.clone())
                )
            }
            .into_boxed_str(),
        );
        let mut spec = ToolSpec::new(old, description, target.mutability, target.bridge_methods);
        spec.idempotent = target.idempotent;
        spec.destructive = target.destructive;
        spec.read_only = target.read_only;
        self.order.push(old);
        self.aliases.insert(
            old,
            Alias {
                spec,
                target: new,
                fixed_args,
            },
        );
    }

    /// Specs (aliases included) in registration order.
    pub fn describe(&self) -> Vec<&ToolSpec> {
        self.order
            .iter()
            .filter_map(|n| self.spec(n))
            .collect()
    }
    pub fn names(&self) -> Vec<&'static str> {
        self.order.clone()
    }
    /// True for a deprecated alias registered with [`Registry::alias`].
    pub fn is_alias(&self, name: &str) -> bool {
        self.aliases.contains_key(name)
    }
    pub fn spec(&self, name: &str) -> Option<&ToolSpec> {
        self.tools
            .get(name)
            .map(|(s, _)| s)
            .or_else(|| self.aliases.get(name).map(|a| &a.spec))
    }
    /// Up to three registered names (deprecated aliases left out) close to `name`, best first: for the
    /// `didYouMean` of an unknown-tool answer.
    pub fn did_you_mean(&self, name: &str) -> Vec<String> {
        nearest_names(
            name,
            self.order.iter().copied().filter(|n| !self.aliases.contains_key(n)),
            3,
        )
    }

    /// Install the per-call hook (see [`AfterCall`]); at most one.
    pub fn after_call(&mut self, hook: impl Fn(&S, &str, &mut ToolOutput) + Send + Sync + 'static) {
        self.after_call = Some(Arc::new(hook));
    }

    /// Make [`call`](Self::call) `debug_assert!` that every `next` / `useInstead` tool it emits is
    /// registered here. Off by default (a test may register a partial set); a server's real
    /// registry turns it on once everything is registered.
    pub fn require_known_steps(&mut self) {
        self.strict_steps = true;
    }

    /// Whether `name` is a registered tool or alias.
    pub fn knows(&self, name: &str) -> bool {
        self.tools.contains_key(name) || self.aliases.contains_key(name)
    }

    /// Add the steps `tool` suggests after a successful call (see [`ToolSpec::then`]), for a
    /// registry that wires them in one place after registering. Panics on an unknown tool.
    pub fn suggest_after(&mut self, tool: &str, next: &str, why: &str) {
        let (spec, _) = self
            .tools
            .get_mut(tool)
            .unwrap_or_else(|| panic!("suggest_after: unknown tool {tool}"));
        spec.next_on_success.push(NextStep::new(next, why));
    }

    /// Call a tool: (alias resolution ->) capability gate -> handler -> handle stripping -> `next`.
    pub async fn call(
        &self,
        state: &S,
        name: &str,
        mut args: Value,
        budget: Budget,
    ) -> Result<ToolOutput, CallFailure> {
        let mut alias = None;
        let mut target = name;
        if let Some(a) = self.aliases.get(name) {
            if !a.fixed_args.is_empty() {
                let mut merged = match args {
                    Value::Object(m) => m,
                    Value::Null => Map::new(),
                    _ => {
                        return Err(ToolError::invalid_arguments(format!(
                            "{name}: arguments must be an object"
                        ))
                        .into())
                    }
                };
                merged.extend(a.fixed_args.clone());
                args = Value::Object(merged);
            }
            target = a.target;
            alias = Some(name);
        }
        let (spec, handler) = self
            .tools
            .get(target)
            .ok_or_else(|| CallFailure::UnknownTool(name.to_string()))?;
        let ran: Result<ToolOutput, ToolError> = async {
            check_argument_names(spec, &args)?;
            if !spec.bridge_methods.is_empty() {
                require_caps_within(
                    state.bridge(),
                    spec.bridge_methods,
                    CAPS_LOCK_WAIT.min(budget.remaining()),
                )
                .await?;
            }
            let note = state.before_call(spec, &args, &budget).await?;
            let mut out = handler(state.clone(), CallCtx { budget }, args).await?;
            if let (Some(note), Value::Object(m)) = (note, &mut out.json) {
                m.insert("addonNote".into(), json!(note));
            }
            Ok(out)
        }
        .await;
        let mut out = match ran {
            Ok(out) => out,
            Err(mut e) => {
                if let Some(old) = alias {
                    // the failure still tells the caller which name to use from now on
                    e.data
                        .insert("deprecated".into(), json!({"alias": old, "use": target}));
                    if !e.data.contains_key("useInstead") {
                        let fixed = &self.aliases[old].fixed_args;
                        let args = (!fixed.is_empty()).then(|| Value::Object(fixed.clone()));
                        e = e.use_instead(target, "deprecated alias", args);
                    }
                }
                self.check_use_instead(&e);
                return Err(e.into());
            }
        };
        out.json = strip_handles(out.json);
        if let Some(hook) = &self.after_call {
            hook(state, target, &mut out);
        }
        let steps = match out.next.take() {
            Some(steps) => steps,
            None => spec.next_on_success.clone(),
        };
        debug_assert!(
            !self.strict_steps || steps.iter().all(|n| self.knows(&n.tool)),
            "{target}: next names an unregistered tool: {steps:?}"
        );
        if !steps.is_empty() || alias.is_some() {
            if !out.json.is_object() {
                out.json = json!({ "result": out.json.take() });
            }
            if let Some(m) = out.json.as_object_mut() {
                if !steps.is_empty() {
                    m.insert(
                        "next".into(),
                        Value::Array(steps.iter().map(NextStep::to_json).collect()),
                    );
                }
                if let Some(old) = alias {
                    let mut marker = json!({"alias": old, "use": target});
                    let fixed = &self.aliases[old].fixed_args;
                    if !fixed.is_empty() {
                        marker["args"] = Value::Object(fixed.clone());
                    }
                    m.insert("deprecated".into(), marker);
                }
            }
        }
        out.next = Some(steps);
        Ok(out)
    }

    fn check_use_instead(&self, e: &ToolError) {
        debug_assert!(
            !self.strict_steps
                || e.data
                    .get("useInstead")
                    .and_then(|u| u.get("tool"))
                    .and_then(Value::as_str)
                    .is_none_or(|t| self.knows(t)),
            "{}: useInstead names an unregistered tool: {:?}",
            e.name,
            e.data.get("useInstead")
        );
    }
}

/// Up to `max` of `candidates` close to `name`, best first. Close = a small edit distance
/// (a swapped pair counts once, case-insensitive) or a shared word of at least four letters
/// (words split at `_ . - space`); an unrelated name gets nothing. The request itself is never
/// returned.
pub fn nearest_names<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    max: usize,
) -> Vec<String> {
    let lower = name.trim().to_ascii_lowercase();
    let words = |s: &str| -> Vec<String> {
        s.split(['_', '.', '-', ' '])
            .filter(|w| w.len() >= 4)
            .map(str::to_string)
            .collect()
    };
    let name_words = words(&lower);
    let limit = (lower.chars().count() / 4).max(2);
    let mut scored: Vec<(usize, String)> = Vec::new();
    for c in candidates {
        let cl = c.to_ascii_lowercase();
        if cl == lower && c == name {
            continue;
        }
        let d = edit_distance(&lower, &cl);
        if d <= limit {
            scored.push((d, c.to_string()));
        } else if words(&cl).iter().any(|w| name_words.contains(w)) {
            scored.push((limit + 1, c.to_string()));
        }
    }
    scored.sort();
    scored.truncate(max);
    scored.into_iter().map(|(_, n)| n).collect()
}

/// Refuse top-level argument keys the tool's schema does not declare, so a wrong name
/// (`classname` for `class`) fails instead of being dropped and the call running unfiltered.
/// Open schemas (no declared properties, or `additionalProperties: true`) accept anything; keys
/// declared only inside a top-level `oneOf`/`anyOf`/`allOf` branch count as declared.
fn check_argument_names(spec: &ToolSpec, args: &Value) -> Result<(), ToolError> {
    let Value::Object(args) = args else { return Ok(()) };
    let schema = &spec.input_schema;
    if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    let branches = ["oneOf", "anyOf", "allOf"]
        .iter()
        .filter_map(|k| schema.get(*k).and_then(Value::as_array))
        .flatten();
    let known: Vec<&str> = std::iter::once(schema)
        .chain(branches)
        .filter_map(|s| s.get("properties").and_then(Value::as_object))
        .flat_map(|p| p.keys().map(String::as_str))
        .collect();
    if known.is_empty() {
        return Ok(());
    }
    let unknown: Vec<&str> = args
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k))
        .collect();
    let Some(first) = unknown.first() else { return Ok(()) };
    let mut near = nearest_names(first, known.iter().copied(), 3);
    if near.is_empty() {
        let lower = first.to_ascii_lowercase();
        near = known
            .iter()
            .filter(|k| k.len() >= 3 && (lower.contains(&k.to_ascii_lowercase()) || k.to_ascii_lowercase().contains(&lower)))
            .take(3)
            .map(|k| k.to_string())
            .collect();
    }
    let mut accepted = known.clone();
    accepted.sort_unstable();
    accepted.dedup();
    Err(ToolError::invalid_arguments(format!(
        "{}: unknown argument(s) {}; accepted: {}",
        spec.name,
        unknown.join(", "),
        accepted.join(", ")
    ))
    .with_data(json!({"unknownArguments": unknown}))
    .did_you_mean(near))
}

/// Optimal-string-alignment distance (Levenshtein plus adjacent transposition).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

/// Capability gate (POC `_require_caps`): connect (a dead bridge surfaces as `BridgeNotRunning`),
/// check the advertised methods, re-fetch once, then `NotImplemented{missing}` without dispatching.
pub async fn require_caps(bridge: &SharedBridge, methods: &[&str]) -> Result<(), ToolError> {
    require_caps_inner(bridge, methods, None).await
}

/// The longest [`Registry::call`] waits for another call's bridge sequence before the capability
/// gate (and with it the call) gives up with a retry-safe `BridgeBusy`.
pub const CAPS_LOCK_WAIT: Duration = Duration::from_secs(10);

/// [`require_caps`] that waits at most `wait` for the bridge lock: a bridge held by a long call
/// (a play-test, a compile, a load) is `BridgeBusy` (retry-safe, nothing was sent) instead of an
/// unbounded queue behind it.
pub async fn require_caps_within(
    bridge: &SharedBridge,
    methods: &[&str],
    wait: Duration,
) -> Result<(), ToolError> {
    require_caps_inner(bridge, methods, Some(wait)).await
}

async fn require_caps_inner(
    bridge: &SharedBridge,
    methods: &[&str],
    wait: Option<Duration>,
) -> Result<(), ToolError> {
    let mut b = match wait {
        None => bridge.lock().await,
        Some(wait) => match tokio::time::timeout(wait, bridge.lock()).await {
            Ok(guard) => guard,
            Err(_) => {
                return Err(ToolError::new(
                    "BridgeBusy",
                    "BridgeBusy",
                    format!(
                        "the bridge stayed busy with another call for {} ms; nothing was sent. \
                         Retry when that call has finished",
                        wait.as_millis()
                    ),
                )
                .retry_safe(true))
            }
        },
    };
    b.ensure_connected().await?;
    let mut missing = b.missing(methods);
    if !missing.is_empty() {
        let _ = b.refresh_capabilities().await;
        missing = b.missing(methods);
    }
    if !missing.is_empty() {
        return Err(ToolError::not_implemented(
            "CapabilityMissing",
            format!(
                "the bridge does not advertise {} (nothing was sent)",
                missing.join(", ")
            ),
            json!({"missing": missing, "blockedOn": "docs/data/bridge-protocol.md §7 method table"}),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod code_tests {
    use super::*;

    /// Live-fix C item 3: `code` is not unique per type (a host-side refusal may carry none, or a
    /// reused `-32602`), so clients switch on `type`/`name` (documented in both AGENT.md files).
    #[test]
    fn host_side_refusals_have_a_name_and_no_code_while_bridge_errors_keep_theirs() {
        for name in ["BridgeBusy", "CompileRunning", "OpenUnconfirmed", "OpenPending"] {
            let json = ToolError::new(name, name, "x").to_json();
            assert_eq!(json["code"], Value::Null, "{name}");
            assert_eq!(json["type"], json!(name));
            assert_eq!(json["name"], json!(name));
        }
        let wire = ToolError::not_implemented("CapabilityMissing", "x", json!({})).to_json();
        assert_eq!(wire["code"], json!(-32011));
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn a_swapped_pair_is_one_edit_and_unrelated_names_match_nothing() {
        let names = ["save_map", "compile_map", "run_map", "get_entity"];
        assert_eq!(nearest_names("save_mpa", names, 3)[0], "save_map");
        assert!(nearest_names("frobnicate_widgets", names, 3).is_empty());
        assert!(nearest_names("save_map", ["save_map"], 3).is_empty());
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn a_slice_is_capped_by_the_parent_and_keeps_its_request_cap() {
        let parent = Budget::new(1_000, 500);
        let small = parent.slice(Duration::from_millis(300));
        assert_eq!(small.total_ms(), 300);
        assert!(small.deadline_ms().unwrap() <= 300);
        let big = parent.slice(Duration::from_secs(60));
        assert!(big.total_ms() <= 1_000);
        assert!(
            big.deadline_ms().unwrap() <= 500,
            "the per-request cap is kept"
        );
    }

    #[test]
    fn a_slice_of_a_spent_budget_is_spent() {
        let parent = Budget::new(0, 500);
        let child = parent.slice(Duration::from_millis(300));
        assert!(matches!(child.deadline_ms(), Err(e) if e.type_ == "ToolBudgetExceeded"));
    }
}
