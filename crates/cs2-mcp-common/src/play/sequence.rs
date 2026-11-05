//! `input_sequence`: a list of input.set steps run back to back under one bridge lock. Each step is
//! held for `holdTicks` game ticks, measured on game.local `tick` samples (`run_wait` with the `tick`
//! predicate), never wall time. `input.clear` is always sent once the first step may have started.
//! Shared by client-mcp and hammer-mcp: the caller locks its bridge and passes it in.
use super::input::{apply_turn, override_angles, parse_input_set, InputRequest, InputSet, Turn};
use super::wait::{distance, run_wait, sample_of, view, Predicate, Sample, WaitEnd, WaitRequest};
use crate::bridge::{BridgeClient, RequestOpts};
use crate::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::time::Duration;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}


const MAX_STEPS: usize = 32;
/// Longest hold of one step: 10 s at 64 ticks/s (the design's cap).
const MAX_HOLD_TICKS: u64 = 640;
const MAX_LABEL_BYTES: usize = 64;
/// Game tick rate: ticks are converted to wall time only for budgeting and timeouts.
const TICK_RATE: u64 = 64;
/// Budget reserved per step: its input.set, its tick samples and the pacing between them.
const STEP_OVERHEAD_MS: u64 = 100;
/// Budget kept back on top of the estimate for the closing input.clear and the reply.
const SEQUENCE_MARGIN_MS: u64 = 1500;
/// Slack added to a step's tick wait (on top of twice its nominal duration).
const STEP_SLACK_MS: u64 = 1000;
/// Pace between real game.local samples while a step waits (one tick is about 15.6 ms).
const STEP_POLL_MS: u64 = 15;
/// The closing clear always gets at least this long, even when the budget is nearly spent.
pub const CLEAR_FLOOR_MS: u64 = 1000;

/// One validated step: the input.set params (or a relative turn), and how many ticks it is held.
struct Step {
    set: InputSet,
    turn: Option<Turn>,
    hold: u64,
    label: Option<String>,
}

struct Sequence {
    steps: Vec<Step>,
}

/// Where a run got to. `sent` is set before the first input.set, so later errors are not retry-safe.
struct Progress {
    sent: bool,
    steps: Vec<Value>,
    start: Sample,
    last: Sample,
    distance: f64,
    waited: Duration,
}

fn parse_step(v: &Value) -> Result<Step, ToolError> {
    let obj = match v {
        Value::Object(m) => m,
        _ => return Err(invalid("step must be an object")),
    };
    let hold = match obj.get("holdTicks") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_HOLD_TICKS).contains(n))
            .ok_or_else(|| invalid(format!("holdTicks must be an integer 1..={MAX_HOLD_TICKS}")))?,
    };
    let label = match obj.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= MAX_LABEL_BYTES => Some(s.clone()),
        _ => {
            return Err(invalid(format!(
                "label must be a string of at most {MAX_LABEL_BYTES} bytes"
            )))
        }
    };
    // the input_set fields are validated by parse_input_set, the same rules as input_set
    let mut input = obj.clone();
    input.remove("holdTicks");
    input.remove("label");
    let InputRequest { set, turn } = parse_input_set(&Value::Object(input))?;
    Ok(Step {
        set,
        turn,
        hold,
        label,
    })
}

/// Validate `input_sequence` arguments. Nothing is sent. A bad step is named by its index.
fn parse_sequence(args: &Value) -> Result<Sequence, ToolError> {
    let obj = match args {
        Value::Object(m) => m,
        _ => return Err(invalid("arguments must be an object")),
    };
    if let Some(k) = obj.keys().find(|k| k.as_str() != "steps") {
        return Err(invalid(format!(
            "unknown key {k:?} in input_sequence (allowed: steps)"
        )));
    }
    let items = obj
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("steps must be an array of 1..={MAX_STEPS} steps")))?;
    if items.is_empty() || items.len() > MAX_STEPS {
        return Err(invalid(format!(
            "steps must hold 1..={MAX_STEPS} steps, got {}",
            items.len()
        )));
    }
    let mut steps = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let step = parse_step(item).map_err(|e| invalid(format!("steps[{i}]: {}", e.message)))?;
        steps.push(step);
    }
    Ok(Sequence { steps })
}

/// Estimated wall time of a sequence: its ticks at the game rate plus the per-step overhead.
fn estimate_ms(total_ticks: u64, steps: usize) -> u64 {
    (total_ticks * 1000).div_ceil(TICK_RATE) + steps as u64 * STEP_OVERHEAD_MS
}

/// Refuse a sequence the call budget cannot finish, before anything is sent. The error names the
/// most ticks that would have fit (`maxTotalTicks`), so the caller can split the sequence.
fn check_budget(ctx: &CallCtx, seq: &Sequence) -> Result<(), ToolError> {
    let total: u64 = seq.steps.iter().map(|s| s.hold).sum();
    let estimate = estimate_ms(total, seq.steps.len());
    let remaining = ctx.budget.remaining().as_millis() as u64;
    let need = estimate + SEQUENCE_MARGIN_MS;
    if remaining >= need {
        return Ok(());
    }
    let spare =
        remaining.saturating_sub(SEQUENCE_MARGIN_MS + seq.steps.len() as u64 * STEP_OVERHEAD_MS);
    let max_ticks = spare * TICK_RATE / 1000;
    Err(ToolError::new(
        "ToolBudgetExceeded",
        "ToolBudgetExceeded",
        format!(
            "input_sequence needs about {estimate} ms for {total} ticks but {remaining} ms of the call budget remain (maxTotalTicks {max_ticks}); nothing was sent"
        ),
    )
    .retry_safe(true)
    .with_data(json!({
        "commandsSent": false,
        "estimateMs": estimate,
        "remainingMs": remaining,
        "totalTicks": total,
        "maxTotalTicks": max_ticks,
    })))
}

/// The wait for one step: its ticks at the game rate, twice over, plus slack.
fn step_timeout_ms(hold: u64) -> u64 {
    hold * 1000 / TICK_RATE * 2 + STEP_SLACK_MS
}

/// Resolve a relative turn against the current override angles (input.status).
async fn resolve_turn(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    turn: Turn,
) -> Result<[f64; 3], ToolError> {
    let status = bridge
        .request(
            "input.status",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    apply_turn(override_angles(&status)?, turn)
}

/// Run the steps in order. Each step sends input.set, then holds for its `holdTicks`, counted from
/// the first game.local reading after that set (so the hold always covers the set's usercmds). The
/// first reading is the step's startTick. Errors leave `p` describing what was sent and completed.
async fn play(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    seq: &Sequence,
    p: &mut Progress,
) -> Result<(), ToolError> {
    for (i, step) in seq.steps.iter().enumerate() {
        let mut set = step.set.clone();
        if let Some(turn) = step.turn {
            set.angles = Some(Some(resolve_turn(bridge, ctx, turn).await?));
        }
        let params = serde_json::to_value(&set)
            .map_err(|e| ToolError::internal(format!("cannot encode input.set params: {e}")))?;
        let deadline = ctx.budget.deadline_ms()?;
        // from here on the bridge may have applied the set: an error is not retry-safe
        p.sent = true;
        bridge
            .request("input.set", params, RequestOpts::deadline(deadline))
            .await?;
        let req = WaitRequest {
            predicate: Predicate::Tick { ticks: step.hold },
            from: None,
            timeout_ms: step_timeout_ms(step.hold),
            poll_ms: STEP_POLL_MS,
            round: Default::default(),
            ammo_from: None,
        };
        let out = run_wait(bridge, ctx, &req).await?;
        p.waited += out.waited;
        // with no `from`, `before` is the first reading after the set
        let first = out.before;
        let s = match out.end {
            WaitEnd::Matched { sample, .. } => sample,
            WaitEnd::Expired { bridge_error, .. } => {
                let (reason, message) = match &bridge_error {
                    Some(_) => (
                        "bridgeQueueTimeout",
                        "bridge main thread did not serve game.local during a step",
                    ),
                    None => ("timeout", "step did not reach its tick count in time"),
                };
                return Err(ToolError::new(
                    "WaitTimeout",
                    "WaitTimeout",
                    format!("{message} (step {i})"),
                )
                .retry_safe(false)
                .with_data(json!({"reason": reason, "step": i, "holdTicks": step.hold})));
            }
        };
        // movement since the previous step's end, including the gap before this step's set
        if let (Some(a), Some(b)) = (p.last.origin, s.origin) {
            p.distance += distance(a, b);
        }
        p.steps.push(json!({
            "step": i,
            "label": step.label,
            "holdTicks": step.hold,
            "startTick": first["tick"],
            "endTick": s.tick,
            "origin": s.origin,
            "viewAngles": s.view_angles,
        }));
        p.last = s;
    }
    Ok(())
}

/// The closing release. Ungated, with the remaining budget or the 1 s floor, whichever is larger:
/// it may overrun the call budget by up to the floor, because a held button outlives the call.
pub async fn release(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<(), ToolError> {
    let ms = (ctx.budget.remaining().as_millis() as u64).max(CLEAR_FLOOR_MS);
    bridge
        .request("input.clear", json!({}), RequestOpts::ungated(ms))
        .await
        .map(|_| ())
        .map_err(ToolError::from)
}

/// The `inputCleared` value, as input_set reports it: true, "failed", or false when no clear was sent.
fn cleared_value(cleared: &Option<Result<(), ToolError>>) -> Value {
    match cleared {
        None => json!(false),
        Some(Ok(())) => json!(true),
        Some(Err(_)) => json!("failed"),
    }
}

/// An error after the run began: stepsCompleted and the steps done so far. Retry-safe only when
/// no input.set was attempted.
fn run_error(e: ToolError, p: &Progress, cleared: &Option<Result<(), ToolError>>) -> ToolError {
    let mut e = e;
    if p.sent {
        e = e.retry_safe(false);
    }
    e.with_data(json!({
        "inputSent": p.sent,
        "stepsCompleted": p.steps.len(),
        "steps": p.steps,
        "inputCleared": cleared_value(cleared),
    }))
}

/// Validate `args` (`{steps}`) without sending anything.
pub fn validate(args: &Value) -> Result<(), ToolError> {
    parse_sequence(args).map(|_| ())
}

/// Run `args` (`{steps}`) against the bridge the caller has locked. Both servers register this.
/// Ends with `input.clear`.
pub async fn input_sequence(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    args: &Value,
) -> Result<ToolOutput, ToolError> {
    run(bridge, ctx, args, true).await
}

/// As [`input_sequence`], and with `clear_after` false the buttons and view override stay set when
/// the steps are done (`play_test` holds W across its next wait). The caller owns the clear then:
/// on every error the held input is still there, and `inputCleared` says `false`.
pub async fn run(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    args: &Value,
    clear_after: bool,
) -> Result<ToolOutput, ToolError> {
    let seq = parse_sequence(args)?;
    check_budget(ctx, &seq)?;
    // refuse before any input: not in game or dead is PrerequisiteFailed
    let pre = bridge
        .request(
            "game.local",
            json!({}),
            RequestOpts::deadline(ctx.budget.deadline_ms()?),
        )
        .await?
        .result;
    let start = sample_of(&pre, None, false)?;
    if seq.steps.iter().any(|s| s.turn.is_some()) && !bridge.has("input.status") {
        return Err(ToolError::not_implemented(
            "CapabilityMissing",
            "turn needs input.status to read the current override angles; the bridge does not advertise it (nothing was sent)",
            json!({"missing": ["input.status"]}),
        ));
    }
    let mut p = Progress {
        sent: false,
        steps: Vec::new(),
        start: start.clone(),
        last: start,
        distance: 0.0,
        waited: Duration::ZERO,
    };
    let played = play(bridge, ctx, &seq, &mut p).await;
    // the closing release only when a set may have gone out: otherwise there is nothing to undo
    let cleared = if p.sent && clear_after {
        Some(release(bridge, ctx).await)
    } else {
        None
    };
    match played {
        Ok(()) => match cleared {
            Some(Err(e)) => Err(ToolError::new(
                "ClearFailed",
                "ClearFailed",
                format!(
                    "every step ran but input.clear failed ({}); buttons may still be held: call input_clear",
                    e.message
                ),
            )
            .retry_safe(false)
            .with_data(json!({
                "inputSent": p.sent,
                "stepsCompleted": p.steps.len(),
                "steps": p.steps,
                "inputCleared": "failed",
            }))),
            cleared => Ok(json!({
                "steps": p.steps,
                "totals": {
                    "ticks": p.last.tick.saturating_sub(p.start.tick),
                    "distance": p.distance,
                    "waitedMs": p.waited.as_millis() as u64,
                },
                "start": {"tick": p.start.tick, "origin": p.start.origin},
                "final": view(&p.last),
                "inputCleared": cleared_value(&cleared),
            })
            .into()),
        },
        Err(e) => Err(run_error(e, &p, &cleared)),
    }
}

pub fn input_sequence_schema() -> Value {
    let mut step = super::input::input_set_schema();
    step["properties"]["holdTicks"] = json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_HOLD_TICKS,
        "description": "ticks the step lasts", "default": 1
    });
    step["properties"]["label"] = json!({
        "type": "string",
        "maxLength": MAX_LABEL_BYTES,
        "description": "echoed in the result"
    });
    json!({
        "type": "object",
        "properties": {
            "steps": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_STEPS,
                "items": step,
                "description": "run in order; each takes the input_set fields"
            }
        },
        "required": ["steps"],
        "additionalProperties": false
    })
}

