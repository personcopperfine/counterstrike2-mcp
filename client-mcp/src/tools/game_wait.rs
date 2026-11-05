//! `game_wait` tool: the wrapper over the engine in `cs2_mcp_common::play::wait` (`run_wait` and the
//! predicates, shared with hammer-mcp's `play_test`).
use crate::state::ClientState;
use cs2_mcp_common::tools::{
    require_caps, CallCtx, Mutability, Registry, ToolError, ToolOutput, ToolSpec,
};
pub use cs2_mcp_common::play::wait::*;
use serde_json::{json, Value};

/// `game_wait {until, ...}`: poll `game.local` until the predicate matches or the wait times out.
pub async fn game_wait(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_request(&args)?;
    if req.predicate.reads_rules() {
        // only the round predicates need game.rules; the other predicates work on an older bridge
        require_caps(&state.bridge, &["game.rules"]).await?;
    }
    let mut bridge = state.bridge.lock().await;
    let out = run_wait(&mut bridge, &ctx, &req).await?;
    let WaitOutcome {
        end,
        samples,
        waited,
        timeout,
        clamped,
        before,
        ..
    } = out;
    // `at` reports how far the sample stood from its target, on a match and in a timeout's `last`
    let target = match &req.predicate {
        Predicate::At(t) => Some(t),
        _ => None,
    };
    match end {
        WaitEnd::Matched { base: b, sample: s } => {
            let mut delta = json!({
                "ticks": s.tick as i64 - b.tick as i64,
                "distance": b.origin.zip(s.origin).map(|(a, c)| distance(a, c)),
            });
            if let Some(t) = target {
                delta["toTarget"] = to_target(t, &s);
            }
            if let Predicate::Ammo { .. } = &req.predicate {
                delta["clip1"] = json!(clip1_of(&s.weapon)
                    .zip(b.clip1)
                    .map(|(now, from)| now - from));
            }
            let mut out = json!({
                "matched": true,
                "until": req.predicate.name(),
                "waitedMs": millis(waited),
                "samples": samples,
                "before": before,
                "after": view(&s),
                "delta": delta,
            });
            if let Some(r) = &s.rules {
                out["rules"] = r.clone();
            }
            Ok(out.into())
        }
        WaitEnd::Expired { last, bridge_error } => {
            let last_json = last.as_ref().map(|s| {
                let mut v = view(s);
                if let Some(t) = target {
                    v["toTarget"] = to_target(t, s);
                }
                if let Some(r) = &s.rules {
                    v["rules"] = r.clone();
                }
                v
            });
            let mut data = json!({
                "until": req.predicate.name(),
                "waitedMs": millis(waited),
                "timeoutMs": millis(timeout),
                "requestedTimeoutMs": req.timeout_ms,
                "clampedToBudget": clamped,
                "samples": samples,
                "before": before,
                "last": last_json,
            });
            let message = match &bridge_error {
                Some(e) => {
                    data["reason"] = json!("bridgeQueueTimeout");
                    data["bridgeError"] =
                        json!({"name": e.name, "code": e.code, "message": e.message});
                    format!(
                        "bridge main thread did not serve game.local; wait ended early after {samples} samples ({} ms)",
                        millis(waited)
                    )
                }
                None => {
                    data["reason"] = json!("timeout");
                    format!(
                        "{} not reached within {} ms ({samples} samples)",
                        req.predicate.name(),
                        millis(waited)
                    )
                }
            };
            Err(ToolError::new("WaitTimeout", "WaitTimeout", message)
                .retry_safe(true)
                .with_data(data))
        }
    }
}

pub fn game_wait_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "until": {"type": "string", "enum": UNTIL},
            "ticks": {"type": "integer", "minimum": 1, "maximum": TICKS_MAX},
            "minDistance": {"type": "number", "minimum": 0},
            "weapon": {"type": "string", "pattern": "^weapon_[a-z0-9_]+$"},
            "defIndex": {"type": "integer", "minimum": 0, "maximum": 65535},
            "from": {"type": "object", "properties": {"tick": {"type": "integer", "minimum": 0}, "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}}, "required": ["tick", "origin"]},
            "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "tolerance": {"type": "number", "minimum": 0},
            "zTolerance": {"type": "number", "minimum": 0},
            "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3, "description": "until at: [pitch, yaw, roll]; roll ignored"},
            "angleTolerance": {"type": "number", "minimum": 0},
            "requireStopped": {"type": "boolean"},
            "clip1Delta": {"type": "integer", "minimum": -30, "maximum": 30, "description": "until ammo: change from baseline; negative = at most (default -1)"},
            "clip1": {"type": "integer", "minimum": 0, "maximum": 1000, "description": "until ammo: baseline clip count (with entindex)"},
            "entindex": {"type": "integer", "minimum": 0, "maximum": s2sdk_sys::generated::ENTITY_INDEX_MASK},
            "roundStartTime": {"type": "number", "minimum": 0},
            "roundsPlayed": {"type": "integer", "minimum": 0},
            "timeoutMs": {"type": "integer", "minimum": 1},
            "pollMs": {"type": "integer", "minimum": POLL_MIN_MS, "maximum": POLL_MAX_MS}
        },
        "required": ["until"]
    })
}

pub fn register(r: &mut Registry<ClientState>) {
    r.register(
        ToolSpec::new("game_wait", "Wait until a predicate over the local player holds. until: tick (ticks past the baseline), moved (distance from the baseline origin), stopped (speed < 1), weapon (activeWeapon name or defIndex), at (origin within tolerance 2 xy / zTolerance 4, optionally view angles; speed < 1), roundLive (no warmup, freeze, team intro, restart or winner; player alive), roundRestarted (a new round began, then roundLive), ammo (clip1 moved by clip1Delta on the baseline weapon). A timeout (default 5000 ms) is WaitTimeout, retry-safe.", Mutability::ReadOnly, &["game.local"])
            .with_schema(game_wait_schema()),
        game_wait,
    );
}

