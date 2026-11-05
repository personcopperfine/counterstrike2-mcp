//! `game_test` runner: session check, setup, steps, spec expects, the demo hook, teardown. Every step
//! is dispatched through the tool registry (the handlers the MCP server serves) with a budget slice.
//!
//! Budget accounting: `Reserve` is what cleanup still needs from the call budget (input_clear and each
//! teardown item, and the demo stop until it has run or never will). A slice never eats into it. Each
//! slice is bounded by one of three limits, and the limit decides the verdict: the test's own timeoutMs
//! (a timeout is a failure), the call budget (during the steps: BudgetTooSmall, a failure; during the
//! session launch: pending, since a retry progresses), or the per-step cap (a failure). Teardown runs
//! input_clear first, and each later teardown item is capped to leave what the items after it still
//! need. No blind waits: every wait is a tool's own bounded poll.
use super::spec::{self, asserts_on, lookup, resolve_refs, Assertion, Op, Source, Spec, Step};
use super::step_registry;
use crate::state::ClientState;
use cs2_mcp_common::tools::{Budget, CallFailure, ToolError};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Reserved per teardown item (input_clear included).
const TEARDOWN_STEP_RESERVE_MS: u64 = 1_000;
/// Reserved for the demo stop: demo_record_stop waits up to 5 s for its completion line.
const DEMO_STOP_RESERVE_MS: u64 = 6_000;
/// A step needs at least this much usable budget to be dispatched.
const MIN_STEP_MS: u64 = 250;
const STEP_CAP_MS: u64 = 30_000;
const TEARDOWN_CAP_MS: u64 = 10_000;
const READ_CAP_MS: u64 = 5_000;
const MARKER_CAP_MS: u64 = 2_000;
const RESULT_CAP_BYTES: usize = 8 * 1024;
const RESULT_STR_CAP: usize = 256;
const RESULT_ARRAY_CAP: usize = 32;
const BUDGET_NEXT: &str = "call game_test again with the same spec: the call budget ran out, so the test restarts from setup (teardown already ran)";
const LAUNCH_NEXT: &str = "call game_test again with the same spec: launch_session is still starting the session (a repeated launch reuses it)";

/// The bound that limited a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// the test's timeoutMs
    Test,
    /// the call budget, minus the reserve owed to cleanup
    Call,
    /// the per-step cap
    Cap,
}

/// Picks the slice for a step: the smallest of the three bounds, or the limit that makes the step
/// impossible to start (`Err`: Test is a failure, Call is pending).
pub fn choose(
    test_left: Duration,
    call_left: Duration,
    cap: Duration,
) -> Result<(Duration, Limit), Limit> {
    let min = Duration::from_millis(MIN_STEP_MS);
    if test_left < min {
        return Err(Limit::Test);
    }
    if call_left < min {
        return Err(Limit::Call);
    }
    let mut slice = (test_left, Limit::Test);
    if call_left < slice.0 {
        slice = (call_left, Limit::Call);
    }
    if cap < slice.0 {
        slice = (cap, Limit::Cap);
    }
    Ok(slice)
}

/// Classifies a step's tool error under the limit of its slice. `consumed`: the step ran for its whole
/// slice; `budget_spent`: the call budget is now too small for another step. Under the call budget, a
/// refusal to start, or a timeout that came with the budget spent, means the test needs more budget
/// than it was given: BudgetTooSmall {neededMs}. Under the test timeout or the per-step cap, a refusal
/// or a whole-slice run is TestTimeout / StepTimeout with the tool's error as cause. Any other error,
/// a genuine WaitTimeout included, keeps its own type and message.
pub fn classify(
    limit: Limit,
    e: ToolError,
    consumed: bool,
    budget_spent: bool,
    needed_ms: u64,
) -> ToolError {
    // a QueueTimeout is the bridge's own deadline check: the request's deadline (our slice) expired
    let consumed = consumed || e.type_ == "QueueTimeout";
    let timeout_like = e.type_ == "ToolBudgetExceeded" || e.type_.contains("Timeout");
    match limit {
        Limit::Call
            if e.type_ == "ToolBudgetExceeded" || (timeout_like && (consumed || budget_spent)) =>
        {
            too_small(needed_ms).with_data(json!({"cause": e.to_json()}))
        }
        Limit::Test | Limit::Cap if e.type_ == "ToolBudgetExceeded" || consumed => {
            let (kind, msg) = match limit {
                Limit::Test => ("TestTimeout", "timeoutMs ran out during this step"),
                _ => ("StepTimeout", "the step exceeded the per-step cap"),
            };
            ToolError::new(kind, kind, msg).with_data(json!({"cause": e.to_json()}))
        }
        _ => e,
    }
}

fn too_small(needed_ms: u64) -> ToolError {
    ToolError::new(
        "BudgetTooSmall",
        "BudgetTooSmall",
        format!("the call budget ran out during the steps; the test needs about {needed_ms} ms"),
    )
    .with_data(json!({"neededMs": needed_ms}))
}

/// What cleanup still needs from the call budget: input_clear and each teardown item, and the demo
/// stop. The demo part is held through the steps and released only once the stop has run (or the
/// demo never started), so the steps cannot spend the time demo_record_stop needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserve {
    teardown: Duration,
    demo: Duration,
}

impl Reserve {
    pub fn for_spec(spec: &Spec) -> Self {
        Reserve {
            teardown: Duration::from_millis(
                TEARDOWN_STEP_RESERVE_MS * (spec.teardown.len() as u64 + 1),
            ),
            demo: Duration::from_millis(if spec.demo { DEMO_STOP_RESERVE_MS } else { 0 }),
        }
    }

    pub fn total(&self) -> Duration {
        self.teardown + self.demo
    }

    fn release_teardown_item(&mut self) {
        self.teardown = self
            .teardown
            .saturating_sub(Duration::from_millis(TEARDOWN_STEP_RESERVE_MS));
    }

    fn release_demo(&mut self) {
        self.demo = Duration::ZERO;
    }
}

/// Reserve owed to cleanup for `spec`, in ms.
pub fn reserve_ms(spec: &Spec) -> u64 {
    let r = Reserve::for_spec(spec);
    (r.total().as_millis()) as u64
}

/// Everything the test needs from the call budget: setup and steps (their estimates) plus the cleanup
/// reserve. A spec whose total cannot fit is refused up front (nothing is sent).
pub fn needed_ms(spec: &Spec) -> u64 {
    spec.setup
        .iter()
        .chain(spec.steps.iter())
        .map(spec::estimate_ms)
        .sum::<u64>()
        + reserve_ms(spec)
}

/// Runs `spec` inside `parent` (the calling tool's budget). `Err` only for a spec that cannot fit
/// its setup in the budget (BudgetTooSmall, nothing sent); failures and pending runs are results.
pub async fn run(state: &ClientState, parent: &Budget, spec: Spec) -> Result<Value, ToolError> {
    let needed = needed_ms(&spec);
    let budget_ms = parent.remaining().as_millis() as u64;
    if needed > budget_ms {
        return Err(too_small(needed).with_data(json!({"budgetMs": budget_ms})));
    }
    let mut run = Run::new(state, parent, &spec);
    run.execute(&spec).await;
    Ok(run.finish(&spec))
}

#[derive(Debug, Clone)]
enum Halt {
    Failed,
    Pending(&'static str),
}

struct Outcome {
    ok: bool,
    /// the tool's result when it returned one
    result: Option<Value>,
}

impl Outcome {
    fn failed() -> Self {
        Outcome {
            ok: false,
            result: None,
        }
    }
}

struct Run<'a> {
    state: &'a ClientState,
    parent: &'a Budget,
    started: Instant,
    timeout: Duration,
    reserve: Reserve,
    needed: u64,
    verbose: bool,
    labels: HashMap<String, Value>,
    records: Vec<Value>,
    markers: Vec<Value>,
    halt: Option<Halt>,
    ticks_start: Option<u64>,
    ticks_end: Option<u64>,
    demo: Option<Value>,
    demo_started: bool,
    demo_tick_start: Option<u64>,
}

impl<'a> Run<'a> {
    fn new(state: &'a ClientState, parent: &'a Budget, spec: &Spec) -> Self {
        Run {
            state,
            parent,
            started: Instant::now(),
            timeout: Duration::from_millis(spec.timeout_ms),
            reserve: Reserve::for_spec(spec),
            needed: needed_ms(spec),
            verbose: spec.verbose,
            labels: HashMap::new(),
            records: Vec::new(),
            markers: Vec::new(),
            halt: None,
            ticks_start: None,
            ticks_end: None,
            demo: None,
            demo_started: false,
            demo_tick_start: None,
        }
    }

    async fn execute(&mut self, spec: &Spec) {
        if spec.map.is_some() || spec.addon.is_some() {
            self.session(spec).await;
        }
        self.ticks_start = self.read_tick().await;
        self.marker(format!("gt_begin_{}", spec.name)).await;
        self.phase("setup", &spec.setup, spec).await;
        if spec.demo && self.halt.is_none() {
            self.demo_start(spec).await;
        }
        self.phase("steps", &spec.steps, spec).await;
        if spec.demo {
            if self.demo_started {
                self.demo_stop(spec).await;
            } else {
                // never started: nothing will need the demo reserve
                self.reserve.release_demo();
            }
        }
        self.expect(spec).await;
        self.ticks_end = self.read_tick().await;
        self.teardown(spec).await;
        let marker = if self.passed() {
            format!("gt_pass_{}", spec.name)
        } else {
            format!("gt_fail_{}", spec.name)
        };
        self.marker(marker).await;
    }

    /// input_clear first, then the spec's teardown steps. Each item gives up its own reserve, and
    /// its slice is what remains after the reserves still owed to the items after it.
    async fn teardown(&mut self, spec: &Spec) {
        let mut items = vec![Step {
            tool: "input_clear",
            args: json!({}),
            label: Some("input_clear".into()),
            expect: Vec::new(),
        }];
        items.extend(spec.teardown.iter().cloned());
        for step in &items {
            self.reserve.release_teardown_item();
            let cap = self
                .parent
                .remaining()
                .saturating_sub(self.reserve.total())
                .min(Duration::from_millis(TEARDOWN_CAP_MS));
            self.step("teardown", step, cap, Limit::Cap).await;
        }
    }

    fn set_halt(&mut self, h: Halt) {
        if self.halt.is_none() {
            self.halt = Some(h);
        }
    }

    fn call_left(&self) -> Duration {
        self.parent.remaining().saturating_sub(self.reserve.total())
    }

    fn gate(&self) -> Result<(Duration, Limit), Limit> {
        let test_left = self.timeout.saturating_sub(self.started.elapsed());
        choose(
            test_left,
            self.call_left(),
            Duration::from_millis(STEP_CAP_MS),
        )
    }

    /// Records a step that may not start. Under the call budget: BudgetTooSmall during the steps, and
    /// pending only for the session launch (a retry progresses there). Under the test timeout:
    /// TestTimeout.
    fn refused(&mut self, phase: &str, label: &str, tool: &str, limit: Limit) {
        let session = phase == "session";
        let body = match limit {
            Limit::Call if session => {
                json!({"ok": false, "skipped": true, "skipReason": "not run: the call budget ran out before the session started"})
            }
            Limit::Call => json!({"ok": false, "error": too_small(self.needed).to_json()}),
            _ => {
                json!({"ok": false, "error": ToolError::new("TestTimeout", "TestTimeout", "timeoutMs ran out before this step").to_json()})
            }
        };
        self.records.push(rec(phase, label, tool, 0, body));
        self.set_halt(match limit {
            Limit::Call if session => Halt::Pending(BUDGET_NEXT),
            _ => Halt::Failed,
        });
    }

    async fn phase(&mut self, phase: &str, steps: &[Step], spec: &Spec) {
        for step in steps {
            let label = step.display_label();
            if self.halt.is_some() {
                self.records.push(rec(
                    phase,
                    &label,
                    step.tool,
                    0,
                    json!({"ok": false, "skipped": true, "skipReason": "not run: an earlier step stopped the test"}),
                ));
                continue;
            }
            let (slice, limit) = match self.gate() {
                Ok(x) => x,
                Err(limit) => {
                    self.refused(phase, &label, step.tool, limit);
                    continue;
                }
            };
            let o = self.step(phase, step, slice, limit).await;
            if !o.ok && !spec.continue_on_failure {
                self.set_halt(Halt::Failed);
            }
        }
    }

    /// Runs one step: resolve refs, dispatch, record (with its result), then check its expects.
    /// A result saying `pending: true` or `passed: false` fails the step unless the step asserts on
    /// that key itself.
    async fn step(&mut self, phase: &str, step: &Step, slice: Duration, limit: Limit) -> Outcome {
        let label = step.display_label();
        let t0 = Instant::now();
        let args = match resolve_refs(&step.args, &self.labels) {
            Ok(a) => a,
            Err(e) => {
                let err = synth("RefUnresolved", &e);
                self.records.push(rec(
                    phase,
                    &label,
                    step.tool,
                    0,
                    json!({"ok": false, "error": err}),
                ));
                return Outcome::failed();
            }
        };
        let outcome = call(self.state, step.tool, args, self.parent.slice(slice)).await;
        let consumed = t0.elapsed() >= slice;
        let spent = self.call_left() < Duration::from_millis(MIN_STEP_MS);
        let ms = elapsed_ms(t0);
        match outcome {
            Err(e) => {
                let err = classify(limit, e, consumed, spent, self.needed);
                self.records.push(rec(
                    phase,
                    &label,
                    step.tool,
                    ms,
                    json!({"ok": false, "error": err.to_json()}),
                ));
                Outcome {
                    ok: false,
                    result: None,
                }
            }
            Ok(value) => {
                if let Some(l) = &step.label {
                    self.labels.insert(l.clone(), value.clone());
                }
                let assertions = self.evaluate(&step.expect, Some(&value)).await;
                let mut ok = assertions.iter().all(passed);
                let mut error = None;
                if value["pending"].as_bool() == Some(true) && !asserts_on(&step.expect, "pending")
                {
                    ok = false;
                    error = Some(synth(
                        "StepPending",
                        "the step returned pending: true (assert on pending to accept it)",
                    ));
                } else if value["passed"].as_bool() == Some(false)
                    && !asserts_on(&step.expect, "passed")
                {
                    ok = false;
                    error = Some(synth(
                        "StepFailed",
                        "the step's result says passed: false (assert on passed to accept it)",
                    ));
                }
                let (result, truncated) = if ok && !self.verbose {
                    (None, false)
                } else {
                    let (t, c) = trim_result(&value);
                    (Some(t), c)
                };
                self.records.push(rec(
                    phase,
                    &label,
                    step.tool,
                    ms,
                    json!({
                        "ok": ok,
                        "error": error,
                        "assertions": assertions,
                        "result": result,
                        "resultTruncated": truncated.then_some(true),
                    }),
                ));
                Outcome {
                    ok,
                    result: Some(value),
                }
            }
        }
    }

    /// The spec-level expects, over fresh reads (after the steps, before teardown).
    async fn expect(&mut self, spec: &Spec) {
        if spec.expect.is_empty() {
            return;
        }
        if self.halt.is_some() {
            self.records.push(rec(
                "expect",
                "expect",
                "game_state",
                0,
                json!({"ok": false, "skipped": true, "skipReason": "not run: an earlier step stopped the test"}),
            ));
            return;
        }
        let t0 = Instant::now();
        let assertions = self.evaluate(&spec.expect, None).await;
        let ok = assertions.iter().all(passed);
        self.records.push(rec(
            "expect",
            "expect",
            "game_state",
            elapsed_ms(t0),
            json!({"ok": ok, "assertions": assertions}),
        ));
    }

    /// Checks assertions against `result` (a step's) or fresh reads. Each assertion is one entry.
    async fn evaluate(&self, assertions: &[Assertion], result: Option<&Value>) -> Vec<Value> {
        let mut fetched: Vec<(Source, Result<Value, String>)> = Vec::new();
        let mut out = Vec::with_capacity(assertions.len());
        for a in assertions {
            let source = match (a.from, result) {
                (Source::Result, Some(r)) => Ok(r.clone()),
                (Source::Result, None) => Err("there is no step result to read".to_string()),
                (src, _) => match fetched.iter().find(|(s, _)| *s == src) {
                    Some((_, r)) => r.clone(),
                    None => {
                        let r = self.read(src).await.map_err(|e| error_text(&e));
                        fetched.push((src, r.clone()));
                        r
                    }
                },
            };
            out.push(self.check(a, source));
        }
        out
    }

    fn check(&self, a: &Assertion, source: Result<Value, String>) -> Value {
        let mut rec = json!({
            "path": a.path,
            "op": a.op.name(),
            "from": a.from.name(),
            "wanted": Value::Null,
            "got": Value::Null,
            "ok": false,
        });
        let wanted = match resolve_refs(&a.value, &self.labels) {
            Ok(w) => w,
            Err(e) => {
                rec["error"] = json!(e);
                return rec;
            }
        };
        rec["wanted"] = wanted.clone();
        let src = match source {
            Ok(v) => v,
            Err(e) => {
                rec["error"] = json!(format!("could not read the value: {e}"));
                return rec;
            }
        };
        match lookup(&src, &a.segments) {
            None => rec["error"] = json!(format!("path {} not found", a.path)),
            Some(got) => {
                rec["got"] = got.clone();
                match compare(a.op, got, &wanted, a.tolerance) {
                    Ok(ok) => rec["ok"] = json!(ok),
                    Err(e) => rec["error"] = json!(e),
                }
            }
        }
        rec
    }

    async fn demo_start(&mut self, spec: &Spec) {
        self.demo_tick_start = self.read_tick().await;
        let (slice, limit) = match self.gate() {
            Ok(x) => x,
            Err(limit) => {
                self.refused("demo", "demo_record_start", "demo_record_start", limit);
                return;
            }
        };
        let step = Step {
            tool: "demo_record_start",
            args: json!({"name": spec.name}),
            label: None,
            expect: Vec::new(),
        };
        let o = self.step("demo", &step, slice, limit).await;
        if o.ok {
            self.demo_started = true;
        } else {
            self.set_halt(Halt::Failed);
        }
    }

    /// Stops the recording (always, once started, even when the steps failed). The path goes in the
    /// result, and demo.ticks.end is the tick read right after the stop.
    async fn demo_stop(&mut self, spec: &Spec) {
        self.demo_started = false;
        self.reserve.release_demo();
        let cap = self
            .call_left()
            .min(Duration::from_millis(DEMO_STOP_RESERVE_MS));
        let step = Step {
            tool: "demo_record_stop",
            args: json!({"name": spec.name}),
            label: None,
            expect: Vec::new(),
        };
        let o = self.step("demo", &step, cap, Limit::Cap).await;
        if let Some(r) = o.result.filter(|_| o.ok) {
            let end = self.read_tick().await;
            self.demo = Some(json!({
                "name": spec.name,
                "path": r["demoPath"],
                "ticks": {"start": self.demo_tick_start, "end": end},
            }));
        }
    }

    /// Session check: a running session on `spec.map` is used as is; otherwise launch_session starts
    /// one (it reuses an identical running session, and refuses a busy mount).
    async fn session(&mut self, spec: &Spec) {
        if let Some(map) = &spec.map {
            if let Ok(gs) = self.read(Source::GameState).await {
                if map_is(&gs, map) {
                    self.records.push(rec(
                        "session",
                        "session",
                        "game_state",
                        0,
                        json!({"ok": true}),
                    ));
                    return;
                }
            }
        }
        let mut args = json!({"kind": "wine-tools"});
        if let Some(m) = &spec.map {
            args["map"] = json!(m);
        }
        if let Some(a) = &spec.addon {
            args["addon"] = json!(a);
        }
        let (slice, limit) = match self.gate() {
            Ok(x) => x,
            Err(limit) => {
                self.refused("session", "session", "launch_session", limit);
                return;
            }
        };
        let t0 = Instant::now();
        let launched = call(self.state, "launch_session", args, self.parent.slice(slice)).await;
        let consumed = t0.elapsed() >= slice;
        let spent = self.call_left() < Duration::from_millis(MIN_STEP_MS);
        let ms = elapsed_ms(t0);
        match launched {
            Err(e) => {
                // a session launch the call budget cut short is pending: the retry starts it again
                let pending = limit == Limit::Call && spent;
                let err = if pending {
                    e
                } else {
                    classify(limit, e, consumed, spent, self.needed)
                };
                self.records.push(rec(
                    "session",
                    "session",
                    "launch_session",
                    ms,
                    json!({"ok": false, "error": err.to_json()}),
                ));
                self.set_halt(if pending {
                    Halt::Pending(BUDGET_NEXT)
                } else {
                    Halt::Failed
                });
            }
            Ok(v) if v["pending"].as_bool() == Some(true) => {
                let e = synth("LaunchPending", "the session is still starting");
                self.records.push(rec(
                    "session",
                    "session",
                    "launch_session",
                    ms,
                    json!({"ok": false, "error": e}),
                ));
                self.set_halt(Halt::Pending(LAUNCH_NEXT));
            }
            Ok(_) => match &spec.map {
                None => self.records.push(rec(
                    "session",
                    "session",
                    "launch_session",
                    ms,
                    json!({"ok": true}),
                )),
                Some(map) => match self.read(Source::GameState).await {
                    Ok(gs) if map_is(&gs, map) => self.records.push(rec(
                        "session",
                        "session",
                        "launch_session",
                        ms,
                        json!({"ok": true}),
                    )),
                    Ok(gs) => {
                        let cur = gs["map"].as_str().unwrap_or("<none>");
                        let e = synth(
                            "SessionMismatch",
                            &format!("the game is on {cur}, not {map}"),
                        );
                        self.records.push(rec(
                            "session",
                            "session",
                            "launch_session",
                            ms,
                            json!({"ok": false, "error": e}),
                        ));
                        self.set_halt(Halt::Failed);
                    }
                    Err(e) => {
                        let e = synth(
                            "SessionMismatch",
                            &format!("could not confirm the map: {}", error_text(&e)),
                        );
                        self.records.push(rec(
                            "session",
                            "session",
                            "launch_session",
                            ms,
                            json!({"ok": false, "error": e}),
                        ));
                        self.set_halt(Halt::Failed);
                    }
                },
            },
        }
    }

    /// A read of the game state (or weapons) under the call budget minus what cleanup still needs.
    async fn read(&self, src: Source) -> Result<Value, ToolError> {
        let cap = self.call_left().min(Duration::from_millis(READ_CAP_MS));
        call(self.state, src.tool(), json!({}), self.parent.slice(cap)).await
    }

    async fn read_tick(&self) -> Option<u64> {
        self.read(Source::GameState)
            .await
            .ok()
            .and_then(|v| v["tick"].as_u64())
    }

    /// A checkpoint marker through the checkpoint tool (best effort: reported in `markers`, never a
    /// failure, since a session without a test script has no checkpoint route).
    async fn marker(&mut self, name: String) {
        let cap = self.call_left().min(Duration::from_millis(MARKER_CAP_MS));
        let r = call(
            self.state,
            "checkpoint",
            json!({"name": name.clone()}),
            self.parent.slice(cap),
        )
        .await;
        self.markers.push(match r {
            Ok(_) => json!({"name": name, "ok": true}),
            Err(e) => json!({"name": name, "ok": false, "error": e.to_json()}),
        });
    }

    /// Every record passed or was skipped, and nothing halted the run.
    fn passed(&self) -> bool {
        self.halt.is_none()
            && self
                .records
                .iter()
                .all(|r| r["ok"].as_bool() == Some(true) || r["skipped"].as_bool() == Some(true))
    }

    fn finish(self, spec: &Spec) -> Value {
        let pending = matches!(self.halt, Some(Halt::Pending(_)));
        let passed = self.passed();
        let mut out = json!({
            "name": spec.name,
            "passed": passed,
            "pending": pending,
            "steps": self.records,
            "markers": self.markers,
            "durationMs": elapsed_ms(self.started),
            "ticks": {"start": self.ticks_start, "end": self.ticks_end},
            "timeoutMs": spec.timeout_ms,
        });
        if let Some(d) = self.demo {
            out["demo"] = d;
        }
        if let Some(Halt::Pending(next)) = self.halt {
            out["next"] = json!(next);
        }
        out
    }
}

/// Dispatches `tool` through the registry (capability gate, handler, handle stripping).
async fn call(
    state: &ClientState,
    tool: &str,
    args: Value,
    budget: Budget,
) -> Result<Value, ToolError> {
    match step_registry().call(state, tool, args, budget).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => {
            Err(ToolError::internal(format!("tool {n} is not registered")))
        }
    }
}

/// Bounded copy of a tool result for a record: long strings and arrays are shortened, and anything
/// still over the cap becomes a text preview. `truncated` says whether anything was cut.
pub fn trim_result(v: &Value) -> (Value, bool) {
    let mut changed = false;
    let trimmed = trim_value(v, &mut changed);
    let text = trimmed.to_string();
    if text.len() <= RESULT_CAP_BYTES {
        return (trimmed, changed);
    }
    let mut end = RESULT_CAP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (json!({"preview": &text[..end]}), true)
}

fn trim_value(v: &Value, changed: &mut bool) -> Value {
    match v {
        Value::String(s) if s.len() > RESULT_STR_CAP => {
            *changed = true;
            json!(format!("<{} bytes>", s.len()))
        }
        Value::Array(a) if a.len() > RESULT_ARRAY_CAP => {
            *changed = true;
            let mut out: Vec<Value> = a
                .iter()
                .take(RESULT_ARRAY_CAP)
                .map(|x| trim_value(x, changed))
                .collect();
            out.push(json!(format!(
                "<{} more items>",
                a.len() - RESULT_ARRAY_CAP
            )));
            Value::Array(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| trim_value(x, changed)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), trim_value(x, changed)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// True when the game is in a game on `want` (the full map path, or its last segments).
fn map_is(gs: &Value, want: &str) -> bool {
    let cur = gs["map"].as_str().unwrap_or_default();
    gs["inGame"].as_bool() == Some(true) && (cur == want || cur.ends_with(&format!("/{want}")))
}

fn passed(a: &Value) -> bool {
    a["ok"].as_bool() == Some(true)
}

fn elapsed_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

fn error_text(e: &ToolError) -> String {
    format!("{}: {}", e.type_, e.message)
}

/// A locally made error object (same shape as a tool error).
fn synth(kind: &str, message: &str) -> Value {
    ToolError::new(kind, kind, message).to_json()
}

/// One record: the identifying fields, then `body` with its null fields left out.
fn rec(phase: &str, label: &str, tool: &str, ms: u64, body: Value) -> Value {
    let mut v = json!({"phase": phase, "label": label, "tool": tool, "ms": ms});
    if let (Value::Object(dst), Value::Object(src)) = (&mut v, body) {
        for (k, x) in src {
            if !x.is_null() {
                dst.insert(k, x);
            }
        }
    }
    v
}

/// Compares `got` with `wanted` for `op`. `Err` when the operands do not fit the operator.
pub fn compare(op: Op, got: &Value, wanted: &Value, tolerance: f64) -> Result<bool, String> {
    let num = |label: &str, a: &Value, b: &Value| -> Result<(f64, f64), String> {
        match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!(
                "{label} needs numbers (got {got}, wanted {wanted})"
            )),
        }
    };
    match op {
        Op::Eq => Ok(values_equal(got, wanted)),
        Op::Ne => Ok(!values_equal(got, wanted)),
        Op::Lt => num("lt", got, wanted).map(|(a, b)| a < b),
        Op::Le => num("le", got, wanted).map(|(a, b)| a <= b),
        Op::Gt => num("gt", got, wanted).map(|(a, b)| a > b),
        Op::Ge => num("ge", got, wanted).map(|(a, b)| a >= b),
        Op::Approx => match (got, wanted) {
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return Err(format!(
                        "approx: array lengths differ (got {}, wanted {})",
                        a.len(),
                        b.len()
                    ));
                }
                a.iter()
                    .zip(b)
                    .map(|(x, y)| match (x.as_f64(), y.as_f64()) {
                        (Some(p), Some(q)) => Ok((p - q).abs() <= tolerance),
                        _ => Err("approx needs arrays of numbers".to_string()),
                    })
                    .try_fold(true, |acc, r| r.map(|ok| acc && ok))
            }
            _ => num("approx", got, wanted).map(|(a, b)| (a - b).abs() <= tolerance),
        },
        Op::Contains => match (got, wanted) {
            (Value::String(s), Value::String(sub)) => Ok(s.contains(sub.as_str())),
            (Value::Array(items), _) => Ok(items.iter().any(|x| values_equal(x, wanted))),
            _ => Err(format!("contains needs a string or an array (got {got})")),
        },
        Op::Matches => match (got, wanted) {
            (Value::String(s), Value::String(re)) => regex::Regex::new(re)
                .map(|r| r.is_match(s))
                .map_err(|e| format!("bad regex: {e}")),
            _ => Err(format!("matches needs a string (got {got})")),
        },
    }
}

/// JSON equality: numbers compare by value (1 == 1.0), arrays and objects element-wise.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => match (a, b) {
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
            }
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, p)| y.get(k).is_some_and(|q| values_equal(p, q)))
            }
            _ => a == b,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_rules_per_operator() {
        assert!(compare(Op::Eq, &json!(1), &json!(1.0), 0.0).unwrap());
        assert!(!compare(Op::Ne, &json!([1, 2]), &json!([1, 2]), 0.0).unwrap());
        assert!(compare(Op::Ge, &json!(100.5), &json!(100), 0.0).unwrap());
        assert!(!compare(Op::Lt, &json!(-1), &json!(-1), 0.0).unwrap());
        assert!(compare(Op::Approx, &json!(1.0004), &json!(1.0), 0.001).unwrap());
        assert!(!compare(Op::Approx, &json!([0, 0, 2]), &json!([0, 0, 0]), 0.5).unwrap());
        assert!(compare(Op::Contains, &json!("weapon_hkp2000"), &json!("hkp"), 0.0).unwrap());
        assert!(compare(Op::Contains, &json!(["a", "b"]), &json!("b"), 0.0).unwrap());
        assert!(compare(
            Op::Matches,
            &json!("Weapon_x.Single"),
            &json!("\\.Single$"),
            0.0
        )
        .unwrap());
        assert!(compare(Op::Lt, &json!("x"), &json!(1), 0.0).is_err());
        assert!(compare(Op::Matches, &json!("x"), &json!("("), 0.0).is_err());
    }

    #[test]
    fn approx_length_mismatch_is_named() {
        let e = compare(Op::Approx, &json!([1.0, 2.0]), &json!([1.0]), 0.1).unwrap_err();
        assert!(e.contains("lengths differ (got 2, wanted 1)"), "{e}");
    }

    #[test]
    fn equality_is_numeric_inside_objects_and_arrays() {
        assert!(values_equal(
            &json!({"a": 1, "b": [2]}),
            &json!({"b": [2.0], "a": 1.0})
        ));
        assert!(!values_equal(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(!values_equal(&json!({"a": 1}), &json!({"a": "1"})));
    }

    #[test]
    fn map_match_accepts_full_path_or_suffix() {
        let gs = json!({"inGame": true, "map": "editor/zoo/smartprop_zoo"});
        assert!(map_is(&gs, "editor/zoo/smartprop_zoo"));
        assert!(map_is(&gs, "smartprop_zoo"));
        assert!(!map_is(&gs, "zoo"));
        assert!(!map_is(
            &json!({"inGame": false, "map": "de_dust2"}),
            "de_dust2"
        ));
    }

    #[test]
    fn choose_names_the_binding_limit() {
        let s = Duration::from_secs;
        assert_eq!(choose(s(10), s(10), s(30)), Ok((s(10), Limit::Test)));
        assert_eq!(choose(s(20), s(5), s(30)), Ok((s(5), Limit::Call)));
        assert_eq!(choose(s(20), s(40), s(30)), Ok((s(20), Limit::Test)));
        let (d, l) = choose(s(100), s(100), s(30)).unwrap();
        assert_eq!((d, l), (s(30), Limit::Cap));
        assert_eq!(choose(Duration::ZERO, s(10), s(30)), Err(Limit::Test));
        assert_eq!(
            choose(s(10), Duration::from_millis(100), s(30)),
            Err(Limit::Call)
        );
    }

    fn wait_timeout() -> ToolError {
        ToolError::new("WaitTimeout", "WaitTimeout", "player did not arrive")
    }

    #[test]
    fn a_genuine_wait_timeout_under_the_cap_keeps_its_own_type_and_message() {
        let e = classify(Limit::Cap, wait_timeout(), false, false, 0);
        assert_eq!(e.type_, "WaitTimeout");
        assert_eq!(e.message, "player did not arrive");
        // the same under the test timeout, when the step did not use its whole slice
        assert_eq!(
            classify(Limit::Test, wait_timeout(), false, false, 0).type_,
            "WaitTimeout"
        );
    }

    #[test]
    fn a_step_that_ran_its_whole_slice_is_a_step_timeout_or_test_timeout() {
        let e = classify(Limit::Cap, wait_timeout(), true, false, 0);
        assert_eq!(e.type_, "StepTimeout");
        assert_eq!(e.data["cause"]["type"], json!("WaitTimeout"));
        assert_eq!(
            classify(Limit::Test, wait_timeout(), true, false, 0).type_,
            "TestTimeout"
        );
    }

    #[test]
    fn a_timeout_with_the_call_budget_spent_is_budget_too_small() {
        // the bridge refused a slice the call budget had shrunk to (QueueTimeout at its deadline)
        let queue = || ToolError::new("QueueTimeout", "QueueTimeout", "queued too long");
        assert_eq!(
            classify(Limit::Call, queue(), false, true, 4000).type_,
            "BudgetTooSmall"
        );
        // its deadline was the call budget's slice: expired means the budget ran out
        assert_eq!(
            classify(Limit::Call, queue(), false, false, 4000).type_,
            "BudgetTooSmall"
        );
        assert_eq!(
            classify(Limit::Cap, queue(), false, false, 4000).type_,
            "StepTimeout"
        );
        assert_eq!(
            classify(Limit::Call, wait_timeout(), true, true, 4000).type_,
            "BudgetTooSmall"
        );
    }

    #[test]
    fn a_refusal_to_start_under_the_call_budget_is_budget_too_small() {
        let budget = ToolError::new("ToolBudgetExceeded", "ToolBudgetExceeded", "spent");
        let e = classify(Limit::Call, budget.clone(), false, false, 4000);
        assert_eq!(e.type_, "BudgetTooSmall");
        assert_eq!(e.data["neededMs"], json!(4000));
        // a refusal under the test timeout or the cap is a timeout, not a budget verdict
        assert_eq!(
            classify(Limit::Cap, budget, false, false, 4000).type_,
            "StepTimeout"
        );
        // a genuine error under the call budget keeps its type
        assert_eq!(
            classify(
                Limit::Call,
                ToolError::new("CapabilityMissing", "x", "y"),
                false,
                false,
                0
            )
            .type_,
            "CapabilityMissing"
        );
    }

    fn bare_spec(teardown: usize, demo: bool, steps: usize) -> Spec {
        let item = || Step {
            tool: "game_state",
            args: json!({}),
            label: None,
            expect: Vec::new(),
        };
        Spec {
            name: "t".into(),
            map: None,
            addon: None,
            setup: Vec::new(),
            steps: (0..steps).map(|_| item()).collect(),
            expect: Vec::new(),
            teardown: (0..teardown).map(|_| item()).collect(),
            timeout_ms: 1,
            demo,
            continue_on_failure: false,
            verbose: true,
        }
    }

    #[test]
    fn the_demo_reserve_is_held_through_the_steps_until_released() {
        let s = Duration::from_secs;
        let mut r = Reserve::for_spec(&bare_spec(0, true, 1));
        assert_eq!(r.total(), s(7));
        // the steps see 10 s minus everything cleanup still needs: 3 s, not 9 s
        let call_left = s(10).saturating_sub(r.total());
        assert_eq!(choose(s(100), call_left, s(30)), Ok((s(3), Limit::Call)));
        r.release_demo();
        assert_eq!(r.total(), s(1));
    }

    #[test]
    fn needed_ms_counts_setup_steps_and_the_cleanup_reserve() {
        assert_eq!(needed_ms(&bare_spec(0, false, 2)), 2 * 300 + 1000);
        assert_eq!(needed_ms(&bare_spec(2, true, 1)), 300 + 3000 + 6000);
    }

    #[test]
    fn trimming_drops_big_strings_and_long_arrays_and_caps_the_whole() {
        let big = json!({"b64": "x".repeat(5000), "n": (0..100).collect::<Vec<i32>>()});
        let (t, changed) = trim_result(&big);
        assert!(changed);
        assert_eq!(t["b64"], json!("<5000 bytes>"));
        assert_eq!(t["n"].as_array().unwrap().len(), RESULT_ARRAY_CAP + 1);
        let (t, changed) = trim_result(&json!({"small": 1}));
        assert!(!changed);
        assert_eq!(t, json!({"small": 1}));
        let wide = json!((0..60)
            .map(|i| "y".repeat(200) + &i.to_string())
            .collect::<Vec<_>>());
        let (t, changed) = trim_result(&wide);
        assert!(changed);
        assert!(t.to_string().len() <= RESULT_CAP_BYTES + 64);
    }

    #[test]
    fn the_reserve_covers_input_clear_teardown_and_demo() {
        let spec = |teardown: usize, demo: bool| Spec {
            name: "t".into(),
            map: None,
            addon: None,
            setup: Vec::new(),
            steps: Vec::new(),
            expect: Vec::new(),
            teardown: (0..teardown)
                .map(|_| Step {
                    tool: "game_state",
                    args: json!({}),
                    label: None,
                    expect: Vec::new(),
                })
                .collect(),
            timeout_ms: 1,
            demo,
            continue_on_failure: false,
            verbose: true,
        };
        assert_eq!(reserve_ms(&spec(0, false)), 1_000);
        assert_eq!(reserve_ms(&spec(2, false)), 3_000);
        assert_eq!(reserve_ms(&spec(0, true)), 7_000);
    }
}
