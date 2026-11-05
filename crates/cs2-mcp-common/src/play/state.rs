//! `game_state`: the local player from inside the game (bridge `game.local`), once or sampled.
//! Shared by client-mcp (one read) and hammer-mcp (one read, or `samples` reads `intervalMs` apart
//! with per-axis `originStepStats`, which is how a jittering view shows up in numbers).
use crate::bridge::RequestOpts;
use crate::bridge::{BridgeClient, SharedBridge};
use crate::tools::{CallCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const MAX_SAMPLES: u64 = 2000;
pub const MAX_INTERVAL_MS: u64 = 1000;
pub const SOURCE_NOTE: &str = "bridge game.local (schema-resolved reads on the tools main thread)";
pub const RULES_SOURCE_NOTE: &str = "bridge game.rules (cs_gamerules, schema-resolved; a null field did not resolve, see schemaMissing)";
/// The `game_rules` description (client-mcp only; hammer-mcp words its own).
pub const RULES_DESCRIPTION: &str = "Round and warmup state: warmup, freeze, team intro, restart, game phase, rounds played, round times and win status, bomb planted. A null field did not resolve (see schemaMissing).";

/// One `game.rules` read (C_CSGameRules through the cs_gamerules proxy) plus the `source` note.
pub async fn rules(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    let mut v = bridge
        .request("game.rules", json!({}), RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)?
        .result;
    v["source"] = json!(RULES_SOURCE_NOTE);
    Ok(v)
}

/// One `game.local` read, as the bridge answers it plus the `source` note.
pub async fn read(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    let mut v = local(bridge, ctx).await?;
    v["source"] = json!(SOURCE_NOTE);
    Ok(v)
}

async fn local(bridge: &mut BridgeClient, ctx: &CallCtx) -> Result<Value, ToolError> {
    Ok(bridge
        .request("game.local", json!({}), RequestOpts::deadline(ctx.budget.deadline_ms()?))
        .await
        .map_err(ToolError::from)?
        .result)
}

fn opt_u64(args: &Value, key: &str, default: u64, max: u64) -> Result<u64, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= max)
            .ok_or_else(|| ToolError::invalid_arguments(format!("{key} must be an integer in 0..={max}"))),
    }
}

/// `game_state {samples?, intervalMs?}` for a server that offers the sampling mode. The bridge lock
/// is taken per read, never across the pause, so other tools (an `input_set` steering the run) can
/// interleave with a sampling run.
pub async fn game_state(
    bridge: &SharedBridge,
    ctx: &CallCtx,
    args: &Value,
) -> Result<ToolOutput, ToolError> {
    if let Some(k) = args
        .as_object()
        .and_then(|m| m.keys().find(|k| !["samples", "intervalMs"].contains(&k.as_str())))
    {
        return Err(ToolError::invalid_arguments(format!(
            "unknown key {k:?} in game_state (allowed: samples, intervalMs)"
        )));
    }
    let samples = opt_u64(args, "samples", 1, MAX_SAMPLES)?.max(1);
    let interval = opt_u64(args, "intervalMs", 16, MAX_INTERVAL_MS)?;
    if samples == 1 {
        return Ok(read(&mut *bridge.lock().await, ctx).await?.into());
    }
    Ok(sampled(bridge, ctx, samples, interval).await?.into())
}

/// `samples` reads, `interval` ms apart (fewer when the budget runs short): `{count, intervalMs,
/// originStepStats, samples: [{tMs, ...reply}]}`.
pub async fn sampled(
    bridge: &SharedBridge,
    ctx: &CallCtx,
    samples: u64,
    interval: u64,
) -> Result<Value, ToolError> {
    let start = Instant::now();
    let mut out = Vec::new();
    for i in 0..samples {
        let mut s = local(&mut *bridge.lock().await, ctx).await?;
        if let Value::Object(m) = &mut s {
            m.insert("tMs".into(), json!(start.elapsed().as_secs_f64() * 1000.0));
        }
        out.push(s);
        if i + 1 < samples && interval > 0 {
            if ctx.budget.remaining() < Duration::from_millis(interval + 200) {
                break; // keep what we have: the budget is nearly spent
            }
            tokio::time::sleep(Duration::from_millis(interval)).await;
        }
    }
    let stats = step_stats(&out);
    Ok(json!({"count": out.len(), "intervalMs": interval, "originStepStats": stats, "samples": out, "source": SOURCE_NOTE}))
}

pub fn game_state_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "samples": {"type": "integer", "minimum": 1, "maximum": MAX_SAMPLES,
                        "default": 1, "description": "number of reads"},
            "intervalMs": {"type": "integer", "minimum": 0, "maximum": MAX_INTERVAL_MS,
                           "default": 16, "description": "ms between reads (16 = one frame); 0 = back to back"}
        },
        "additionalProperties": false
    })
}

/// Per-axis statistics of the frame-to-frame origin steps of a sample run.
pub fn step_stats(samples: &[Value]) -> Value {
    let origins: Vec<[f64; 3]> = samples
        .iter()
        .filter_map(|s| {
            let o = s.pointer("/player/origin")?.as_array()?;
            Some([o.first()?.as_f64()?, o.get(1)?.as_f64()?, o.get(2)?.as_f64()?])
        })
        .collect();
    if origins.len() < 3 {
        return Value::Null;
    }
    let axis = |i: usize| {
        let steps: Vec<f64> = origins.windows(2).map(|w| w[1][i] - w[0][i]).collect();
        let n = steps.len() as f64;
        let mean = steps.iter().sum::<f64>() / n;
        let var = steps.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
        // a reversal: the step changes sign against the overall direction while moving
        let reversals = steps
            .iter()
            .filter(|s| mean.abs() > 0.5 && s.signum() != mean.signum() && s.abs() > 0.5)
            .count();
        let zero = steps.iter().filter(|s| s.abs() < 0.01).count();
        json!({
            "mean": mean, "stddev": var.sqrt(),
            "min": steps.iter().cloned().fold(f64::INFINITY, f64::min),
            "max": steps.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            "reversals": reversals, "zeroSteps": zero,
        })
    };
    json!({"steps": origins.len() - 1, "x": axis(0), "y": axis(1), "z": axis(2)})
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn step_stats_counts_reversals() {
        let s = |x: f64| json!({"player": {"origin": [x, 0.0, 0.0]}});
        let smooth: Vec<Value> = (0..10).map(|i| s(i as f64 * 5.0)).collect();
        assert_eq!(step_stats(&smooth)["x"]["reversals"], 0);
        let jitter: Vec<Value> = [0.0, 6.0, 4.0, 12.0, 10.0, 18.0].iter().map(|x| s(*x)).collect();
        assert_eq!(step_stats(&jitter)["x"]["reversals"], 2);
    }
}
