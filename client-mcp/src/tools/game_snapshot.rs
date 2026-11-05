//! `game_snapshot` and `game_restore`: save and restore the local player's pose, health, armor,
//! weapons and active weapon. There is no engine save. A snapshot is read from game.local,
//! game.weapons and game.rules (when advertised), kept in `ClientState` (named, bounded, see
//! `snapshot.rs`), and optionally exported to `<project>/artifacts/snapshots/<name>.json`.
//!
//! `game_restore` runs, in order: input.clear (when advertised), the pose through the setpos path
//! (setpos_exact/setang_exact, verified in process by the `at` wait), then the console commands for
//! health, armor and missing weapons (`ent_fire !player sethealth`, `give`), the active weapon via
//! game.selectWeapon, and one settle phase that polls game.local/game.weapons until the restorable
//! fields match or the single deadline (timeoutMs) ends. Mismatches the console cannot fix (armor or
//! a helmet that would have to be removed, more copies of a weapon than the snapshot had) are
//! classified before anything is sent and reported with a reason; they are not polled for.
//! A team mismatch refuses before sending. Money and round state belong to no restorable part and
//! are not claimed; velocity, crouch, clip ammo and reserve ammo are reported in `notRestored` for
//! the requested parts, with what the player has now and whether it differs.
//!
//! `give` into an occupied slot may drop the weapon that holds it (not live-checked yet); the
//! active-weapon select is sent after any give for that reason.
//!
//! Errors: before the first command is sent, errors are returned as they are (PrerequisiteFailed,
//! InvalidParams, a refused command). Once a command has been sent, every error is not retry-safe
//! and carries `commandsSent` and `commands`. A pose that does not settle is not an error: it is
//! `pose.ok: false` and the restore goes on.
use crate::snapshot::{
    origin_gap, valid_name, yaw_delta, Snapshot, WeaponSnap, ANGLE_TOLERANCE, STORE_CAP,
    XY_TOLERANCE, Z_TOLERANCE,
};
use crate::state::ClientState;
use crate::tools::game_actions::{parse_setpos, setpos_checked};
use crate::tools::game_state::select_params;
use crate::tools::game_wait::{player_of, vec3, SAMPLE_SLACK_MS};
use crate::tools::session::{check_console, exec_console};
use cs2_mcp_common::bridge::{BridgeClient, RequestOpts};
use cs2_mcp_common::tools::{CallCtx, Mutability, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Budget kept back from a restore's waits for the reply (as game_wait does).
const WAIT_MARGIN: Duration = Duration::from_millis(500);
/// Pause between two settle samples (a poll, bounded by the restore deadline).
const POLL: Duration = Duration::from_millis(50);
/// The one sample a settle always takes when the call budget is already spent (a small floor).
const FLOOR_SAMPLE_MS: u64 = 250;
const TIMEOUT_DEFAULT_MS: u64 = 6000;
const TIMEOUT_MAX_MS: u64 = 60_000;
/// The restorable parts, in the order a restore runs them.
const PARTS: [&str; 4] = ["pose", "health", "armor", "weapons"];
const SNAPSHOT_KEYS: [&str; 2] = ["name", "export"];
const RESTORE_KEYS: [&str; 4] = ["name", "snapshot", "parts", "timeoutMs"];
/// Velocity components closer than this count as equal (the pose restore stops the player).
const VELOCITY_TOLERANCE: f64 = 1.0;
/// The select for the active weapon is sent at most this many times in one restore.
const MAX_SELECTS: u32 = 2;

static EXPORT_SEQ: AtomicU64 = AtomicU64::new(0);

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(message)
}

fn millis(d: Duration) -> u64 {
    d.as_millis() as u64
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One bridge read under the caller's lock, with `deadline_ms` as its deadline.
async fn call(
    bridge: &mut BridgeClient,
    method: &str,
    params: Value,
    deadline_ms: u64,
) -> Result<Value, ToolError> {
    Ok(bridge
        .request(method, params, RequestOpts::deadline(deadline_ms))
        .await?
        .result)
}

/// Validated `game_snapshot` arguments: the stored name and whether to export the file.
fn parse_snapshot_args(args: &Value) -> Result<(String, bool), ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if let Some(k) = obj.keys().find(|k| !SNAPSHOT_KEYS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in game_snapshot (allowed: name, export)"
        )));
    }
    let name = match obj.get("name") {
        Some(Value::String(n)) if valid_name(n) => n.clone(),
        _ => return Err(invalid("name is required: 1..64 letters, digits, _ or -")),
    };
    let export = match obj.get("export") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("export must be true or false"))?,
    };
    Ok((name, export))
}

/// Write `<dir>/snapshots/<name>.json` atomically: a uniquely named temp file, then rename. A failed
/// write leaves no temp file behind.
fn write_export(dir: &Path, name: &str, body: &Value) -> Result<PathBuf, ToolError> {
    let dir = dir.join("snapshots");
    let file = dir.join(format!("{name}.json"));
    let seq = EXPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.json.tmp-{}-{seq}", std::process::id()));
    let fail = |e: std::io::Error| {
        ToolError::new(
            "ExportFailed",
            "ExportFailed",
            format!("{}: {e}", file.display()),
        )
        .retry_safe(true)
    };
    let text = serde_json::to_string_pretty(body)
        .map_err(|e| ToolError::internal(format!("cannot encode the snapshot: {e}")))?;
    std::fs::create_dir_all(&dir).map_err(fail)?;
    if let Err(e) = std::fs::write(&tmp, text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(e));
    }
    if let Err(e) = std::fs::rename(&tmp, &file) {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(e));
    }
    Ok(file)
}

/// `game_snapshot {name, export?}`: read the player and store the snapshot under `name`. The export
/// is written first: a failed export leaves the store as it was.
pub async fn game_snapshot(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let (name, export) = parse_snapshot_args(&args)?;
    let snap = {
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        let local = call(&mut bridge, "game.local", json!({}), deadline).await?;
        player_of(&local, false)?;
        let weapons = call(&mut bridge, "game.weapons", json!({}), deadline).await?;
        let rules = if bridge.has("game.rules") {
            Some(call(&mut bridge, "game.rules", json!({}), deadline).await?)
        } else {
            None
        };
        Snapshot::from_reads(&name, now_secs(), &local, &weapons, rules.as_ref())?
    };
    let file = if export {
        Some(write_export(&state.artifacts_dir, &name, &snap.to_json())?)
    } else {
        None
    };
    let (dropped, count) = {
        let mut store = state.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = store.insert(snap.clone());
        (dropped, store.names().len())
    };
    let mut out = json!({
        "snapshot": snap.to_json(),
        "stored": {"name": name, "count": count, "cap": STORE_CAP},
        "source": "bridge game.local + game.weapons (+ game.rules when advertised)",
    });
    if let Some(d) = dropped {
        out["dropped"] = json!(d);
        out["note"] = json!(format!(
            "the store was full ({STORE_CAP}): dropped the least recently stored snapshot {d:?}"
        ));
    }
    if let Some(file) = file {
        out["file"] = json!(file.display().to_string());
    }
    Ok(out.into())
}

/// Validated `game_restore` arguments: the snapshot, the parts in run order, the timeout.
struct RestoreRequest {
    snap: Snapshot,
    parts: Vec<&'static str>,
    timeout_ms: u64,
}

/// Validate `game_restore` arguments and resolve the snapshot (a stored name or an inline object).
/// Nothing is sent.
fn parse_restore(args: &Value, state: &ClientState) -> Result<RestoreRequest, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if let Some(k) = obj.keys().find(|k| !RESTORE_KEYS.contains(&k.as_str())) {
        return Err(invalid(format!(
            "unknown key {k:?} in game_restore (allowed: {})",
            RESTORE_KEYS.join(", ")
        )));
    }
    let by_name = obj.get("name").filter(|v| !v.is_null());
    let inline = obj.get("snapshot").filter(|v| !v.is_null());
    let snap = match (by_name, inline) {
        (Some(n), None) => {
            let n = n
                .as_str()
                .filter(|n| valid_name(n))
                .ok_or_else(|| invalid("name must be 1..64 letters, digits, _ or -"))?;
            let store = state.snapshots.lock().unwrap_or_else(|e| e.into_inner());
            match store.get(n) {
                Some(s) => s.clone(),
                None => {
                    return Err(invalid(format!(
                        "no snapshot named {n:?}; stored: [{}]",
                        store.names().join(", ")
                    )))
                }
            }
        }
        (None, Some(s)) => Snapshot::from_json(s)?,
        _ => return Err(invalid("exactly one of name or snapshot is required")),
    };
    let bad_parts = || invalid("parts must be a non-empty array of pose|health|armor|weapons");
    let parts = match obj.get("parts").filter(|v| !v.is_null()) {
        None => PARTS.to_vec(),
        Some(v) => {
            let items = v
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(bad_parts)?;
            let mut picked: Vec<&str> = Vec::new();
            for p in items {
                let p = p
                    .as_str()
                    .and_then(|s| PARTS.iter().copied().find(|k| *k == s))
                    .ok_or_else(bad_parts)?;
                picked.push(p);
            }
            // run order, whatever order the caller listed them in
            PARTS
                .iter()
                .copied()
                .filter(|p| picked.contains(p))
                .collect()
        }
    };
    let timeout_ms = match obj.get("timeoutMs").filter(|v| !v.is_null()) {
        None => TIMEOUT_DEFAULT_MS,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=TIMEOUT_MAX_MS).contains(n))
            .ok_or_else(|| invalid(format!("timeoutMs must be an integer 1..={TIMEOUT_MAX_MS}")))?,
    };
    Ok(RestoreRequest {
        snap,
        parts,
        timeout_ms,
    })
}

/// What a restore has sent so far. `sent` is set once any command may have reached the game: later
/// errors are then not retry-safe and carry `commands`.
#[derive(Default)]
struct Run {
    commands: Vec<String>,
    sent: bool,
    samples: u64,
    selects: u32,
}

/// The last settle observation: the replies it read, the comparison built from them, and whether
/// the call budget ran out before the deadline did.
struct Settled {
    local: Value,
    weapons: Value,
    entries: Vec<Value>,
    budget_exhausted: bool,
}

/// An error once a command may have gone out: never retry-safe, with what was sent.
fn after_send(e: ToolError, commands: &[String]) -> ToolError {
    e.retry_safe(false)
        .with_data(json!({"commandsSent": true, "commands": commands}))
}

/// Map an error by what was sent: nothing sent keeps the error as it is.
fn sent_error(e: ToolError, run: &Run) -> ToolError {
    if run.sent {
        after_send(e, &run.commands)
    } else {
        e
    }
}

fn held_count(weapons: &Value, name: &str) -> usize {
    weapons
        .get("weapons")
        .and_then(Value::as_array)
        .map_or(0, |ws| {
            ws.iter()
                .filter(|w| w.get("name").and_then(Value::as_str) == Some(name))
                .count()
        })
}

/// The snapshot's weapon names with how many of each it held, in first-appearance order.
fn wanted_counts(list: &[WeaponSnap]) -> Vec<(&str, usize)> {
    let mut out: Vec<(&str, usize)> = Vec::new();
    for w in list {
        match out.iter_mut().find(|(n, _)| *n == w.name) {
            Some((_, c)) => *c += 1,
            None => out.push((w.name.as_str(), 1)),
        }
    }
    out
}

/// The active weapon's designer name from a `game.local` reply.
fn active_name(local: &Value) -> Option<&str> {
    local
        .get("player")
        .and_then(|p| p.get("activeWeapon"))
        .and_then(|w| w.get("name"))
        .and_then(Value::as_str)
}

/// Mismatches the console cannot fix, found from the snapshot and the pre-restore state, before
/// anything is sent: (field, reason). Fields match `compare`'s entries.
fn classify(snap: &Snapshot, parts: &[&str], pre: &Value, pre_w: &Value) -> Vec<(String, String)> {
    let player = pre.get("player").unwrap_or(&Value::Null);
    let mut out = Vec::new();
    if parts.contains(&"armor") {
        let armor = player.get("armor").and_then(Value::as_i64);
        let helmet = player.get("helmet").and_then(Value::as_bool);
        if snap.armor == Some(0) && armor.is_some_and(|a| a > 0) {
            out.push((
                "armor".to_string(),
                "armor cannot be removed from the console; the snapshot had none".to_string(),
            ));
        }
        if snap.helmet == Some(false) && helmet == Some(true) {
            out.push((
                "helmet".to_string(),
                "the helmet cannot be removed from the console; the snapshot had none".to_string(),
            ));
        }
        if snap.helmet == Some(true) && snap.armor == Some(0) {
            out.push((
                "armor".to_string(),
                "the snapshot has a helmet and no armor, but the console gives both together (item_assaultsuit)".to_string(),
            ));
        }
    }
    if parts.contains(&"weapons") {
        for (name, wanted) in wanted_counts(&snap.weapons) {
            let held = held_count(pre_w, name);
            if held > wanted {
                out.push((
                    format!("weapons.{name}"),
                    format!(
                        "{held} held, the snapshot had {wanted}: the console cannot drop a weapon"
                    ),
                ));
            }
        }
    }
    out
}

/// The console commands that fix health, armor and missing weapons, from the pre-restore state: only
/// what differs, in a fixed order (health, armor, weapons in snapshot order). Returns the commands
/// and whether any weapon was given.
fn plan_commands(
    snap: &Snapshot,
    parts: &[&str],
    pre: &Value,
    pre_w: &Value,
) -> (Vec<String>, bool) {
    let player = pre.get("player").unwrap_or(&Value::Null);
    let mut cmds = Vec::new();
    if parts.contains(&"health")
        && player.get("health").and_then(Value::as_i64) != Some(snap.health)
    {
        cmds.push(format!("ent_fire !player sethealth {}", snap.health));
    }
    if parts.contains(&"armor") && snap.armor.is_some_and(|w| w > 0) {
        let armor = player.get("armor").and_then(Value::as_i64).unwrap_or(0);
        let want_helmet = snap.helmet == Some(true);
        let has_helmet = player.get("helmet").and_then(Value::as_bool) == Some(true);
        if armor == 0 || (want_helmet && !has_helmet) {
            cmds.push(if want_helmet {
                "give item_assaultsuit".to_string()
            } else {
                "give item_kevlar".to_string()
            });
        }
    }
    let mut gave_weapon = false;
    if parts.contains(&"weapons") {
        for (name, wanted) in wanted_counts(&snap.weapons) {
            for _ in held_count(pre_w, name)..wanted {
                cmds.push(format!("give {name}"));
                gave_weapon = true;
            }
        }
    }
    (cmds, gave_weapon)
}

/// The comparison of the restorable fields against one observation. Each entry has `field`,
/// `wanted`, `got` and `ok`. `with_active` adds the active weapon. A field in `reasons` is ok: false
/// with its reason and is not waited for.
fn compare(
    snap: &Snapshot,
    parts: &[&str],
    local: &Value,
    weapons: &Value,
    with_active: bool,
    reasons: &[(String, String)],
) -> Vec<Value> {
    let player = local.get("player").unwrap_or(&Value::Null);
    let mut out = Vec::new();
    if parts.contains(&"pose") {
        let got = player.get("origin").and_then(vec3);
        let (xy, dz, ok) = match got {
            Some(g) => {
                let (xy, dz) = origin_gap(snap.origin, g);
                (
                    json!(xy),
                    json!(dz),
                    xy <= XY_TOLERANCE && dz <= Z_TOLERANCE,
                )
            }
            None => (Value::Null, Value::Null, false),
        };
        out.push(json!({
            "field": "origin", "wanted": snap.origin, "got": got, "ok": ok,
            "distance": {"xy": xy, "z": dz},
            "tolerance": {"xy": XY_TOLERANCE, "z": Z_TOLERANCE},
        }));
        let got = local.get("viewAngles").and_then(vec3);
        let (dp, dy, ok) = match got {
            Some(g) => {
                let dp = (g[0] - snap.angles[0]).abs();
                let dy = yaw_delta(g[1], snap.angles[1]).abs();
                (
                    json!(dp),
                    json!(dy),
                    dp <= ANGLE_TOLERANCE && dy <= ANGLE_TOLERANCE,
                )
            }
            None => (Value::Null, Value::Null, false),
        };
        out.push(json!({
            "field": "angles", "wanted": snap.angles, "got": got, "ok": ok,
            "delta": {"pitch": dp, "yaw": dy}, "tolerance": ANGLE_TOLERANCE,
            "note": "roll is not compared",
        }));
    }
    if parts.contains(&"health") {
        let got = player.get("health").and_then(Value::as_i64);
        out.push(json!({
            "field": "health", "wanted": snap.health, "got": got,
            "ok": got == Some(snap.health),
        }));
    }
    if parts.contains(&"armor") {
        if let Some(w) = snap.armor {
            let got = player.get("armor").and_then(Value::as_i64);
            // the exact armor value cannot be set from the console: any armor counts when some was wanted
            let ok = match got {
                Some(g) if w > 0 => g > 0,
                Some(g) => g == 0,
                None => false,
            };
            out.push(json!({"field": "armor", "wanted": w, "got": got, "ok": ok, "approx": true}));
        }
        if let Some(w) = snap.helmet {
            let got = player.get("helmet").and_then(Value::as_bool);
            out.push(json!({"field": "helmet", "wanted": w, "got": got, "ok": got == Some(w)}));
        }
    }
    if parts.contains(&"weapons") {
        for (name, wanted) in wanted_counts(&snap.weapons) {
            let got = held_count(weapons, name);
            out.push(json!({
                "field": format!("weapons.{name}"), "wanted": wanted, "got": got,
                "ok": got >= wanted,
            }));
        }
        if with_active {
            if let Some(a) = &snap.active_weapon {
                let got = active_name(local);
                out.push(json!({
                    "field": "activeWeapon", "wanted": a, "got": got, "ok": got == Some(a.as_str()),
                }));
            }
        }
    }
    for e in out.iter_mut() {
        let field = e["field"].as_str().map(str::to_string);
        if let Some((_, reason)) = field.and_then(|f| reasons.iter().find(|(k, _)| *k == f)) {
            e["ok"] = json!(false);
            e["reason"] = json!(reason);
        }
    }
    out
}

/// Every entry that is waited for (not classified) holds.
fn settled_ok(entries: &[Value]) -> bool {
    entries
        .iter()
        .filter(|e| e.get("reason").is_none())
        .all(|e| e["ok"] == json!(true))
}

/// Every weapon entry that is waited for holds: the inventory has settled.
fn weapons_ok(entries: &[Value]) -> bool {
    entries
        .iter()
        .filter(|e| e.get("reason").is_none())
        .filter(|e| {
            e["field"]
                .as_str()
                .is_some_and(|f| f.starts_with("weapons."))
        })
        .all(|e| e["ok"] == json!(true))
}

/// Select the snapshot's active weapon by name (game.selectWeapon). The send is counted before it
/// goes out: a failed select may still have been applied.
async fn select(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    run: &mut Run,
    weapon: &str,
) -> Result<(), ToolError> {
    let params = select_params(&json!({"weapon": weapon}))?;
    run.commands.push(format!("selectWeapon {weapon}"));
    run.sent = true;
    run.selects += 1;
    call(
        bridge,
        "game.selectWeapon",
        params,
        ctx.budget.deadline_ms()?,
    )
    .await?;
    Ok(())
}

/// The one settle phase: poll game.local and game.weapons until every waited-for field holds or the
/// deadline ends (at least one sample is always taken). A dead or absent player and a bridge error
/// are errors; a mismatch is not. Once the inventory has settled and the active weapon is still
/// wrong, the select is re-sent (MAX_SELECTS in all). A spent call budget reports the last sample
/// with `budgetExhausted`.
#[allow(clippy::too_many_arguments)]
async fn settle(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    snap: &Snapshot,
    parts: &[&str],
    reasons: &[(String, String)],
    target: Option<&str>,
    deadline: Instant,
    run: &mut Run,
) -> Result<Settled, ToolError> {
    let mut last: Option<Settled> = None;
    loop {
        let left = millis(deadline.saturating_duration_since(Instant::now()));
        let ms = match ctx.budget.deadline_ms() {
            Ok(budget) => budget.min(left + SAMPLE_SLACK_MS),
            Err(_) => match last.take() {
                Some(mut s) => {
                    s.budget_exhausted = true;
                    return Ok(s);
                }
                None => FLOOR_SAMPLE_MS,
            },
        };
        let local = call(bridge, "game.local", json!({}), ms).await?;
        player_of(&local, false)?;
        let weapons = call(bridge, "game.weapons", json!({}), ms).await?;
        run.samples += 1;
        let entries = compare(snap, parts, &local, &weapons, true, reasons);
        if let Some(t) = target {
            if run.selects < MAX_SELECTS
                && weapons_ok(&entries)
                && active_name(&local) != Some(t)
                && held_count(&weapons, t) > 0
            {
                select(bridge, ctx, run, t).await?;
            }
        }
        if settled_ok(&entries) || Instant::now() >= deadline {
            return Ok(Settled {
                local,
                weapons,
                entries,
                budget_exhausted: false,
            });
        }
        let pause = POLL.min(deadline.saturating_duration_since(Instant::now()));
        last = Some(Settled {
            local,
            weapons,
            entries,
            budget_exhausted: false,
        });
        tokio::time::sleep(pause).await;
    }
}

/// What the restore did under the bridge lock: the pose record, whether input was cleared, and the
/// settle's last observation.
struct Restored {
    settled: Settled,
    pose: Option<Value>,
    input_cleared: Value,
}

/// The restore itself: input.clear (when advertised), the pose, the console commands, the active
/// weapon select, then the settle. Errors are returned with `run` describing what was sent; the
/// caller maps them with `sent_error`.
#[allow(clippy::too_many_arguments)]
async fn restore_steps(
    bridge: &mut BridgeClient,
    ctx: &CallCtx,
    req: &RestoreRequest,
    pre: &Value,
    pre_weapons: &Value,
    plan: &(Vec<String>, bool),
    reasons: &[(String, String)],
    deadline: Instant,
    run: &mut Run,
) -> Result<Restored, ToolError> {
    let snap = &req.snap;
    let parts = req.parts.as_slice();
    let input_cleared = if bridge.has("input.clear") {
        let deadline_ms = ctx.budget.deadline_ms()?;
        run.commands.push("input.clear".to_string());
        run.sent = true;
        bridge
            .request("input.clear", json!({}), RequestOpts::deadline(deadline_ms))
            .await?;
        json!(true)
    } else {
        json!("notAdvertised")
    };
    let mut pose = None;
    if parts.contains(&"pose") {
        // the pose waits for what is left of the one deadline (at least one sample)
        let pose_ms = millis(deadline.saturating_duration_since(Instant::now())).max(1);
        let req_pose = parse_setpos(&json!({
            "origin": snap.origin,
            "angles": snap.angles,
            "exact": true,
            "timeoutMs": pose_ms,
        }))?;
        match setpos_checked(bridge, ctx, &req_pose).await {
            Ok(reply) => {
                run.commands.push(req_pose.command.clone());
                run.sent = true;
                pose = Some(json!({"ok": true, "toTarget": reply["toTarget"]}));
            }
            // the pose did not settle: recorded, and the restore goes on
            Err(e) if e.name == "WaitTimeout" => {
                if e.data.get("commandsSent") == Some(&json!(true)) {
                    run.commands.push(req_pose.command.clone());
                    run.sent = true;
                }
                pose = Some(json!({
                    "ok": false,
                    "reason": e.data.get("reason").cloned().unwrap_or(json!("timeout")),
                    "toTarget": e.data.get("last").and_then(|l| l.get("toTarget")).cloned().unwrap_or(Value::Null),
                }));
            }
            Err(e) => {
                if e.data.get("commandsSent") == Some(&json!(true)) {
                    run.commands.push(req_pose.command.clone());
                    run.sent = true;
                }
                return Err(e);
            }
        }
    }
    for cmd in &plan.0 {
        exec_console(bridge, ctx, cmd)
            .await
            .map_err(|e| sent_error(e, run))?;
        run.commands.push(cmd.clone());
        run.sent = true;
    }
    let target = snap
        .active_weapon
        .as_deref()
        .filter(|_| parts.contains(&"weapons"));
    if let Some(t) = target {
        // after any give, and whenever the wanted weapon is held but not active
        let held_pre = held_count(pre_weapons, t) > 0;
        if plan.1 || (held_pre && active_name(pre) != Some(t)) {
            select(bridge, ctx, run, t)
                .await
                .map_err(|e| sent_error(e, run))?;
        }
    }
    let settled = settle(bridge, ctx, snap, parts, reasons, target, deadline, run)
        .await
        .map_err(|e| sent_error(e, run))?;
    Ok(Restored {
        settled,
        pose,
        input_cleared,
    })
}

/// Held weapons that the snapshot did not have, by designer name (reported, not dropped).
fn extra_weapons(snap: &Snapshot, weapons: &Value) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    if let Some(ws) = weapons.get("weapons").and_then(Value::as_array) {
        for name in ws
            .iter()
            .filter_map(|w| w.get("name").and_then(Value::as_str))
        {
            if !snap.weapons.iter().any(|s| s.name == name) && !extra.iter().any(|e| e == name) {
                extra.push(name.to_string());
            }
        }
    }
    extra
}

/// `notRestored` for the requested parts only: what the console cannot set, with the wanted value,
/// what the player has now, and whether they differ. Money and round state belong to no part and
/// are not claimed at all.
fn not_restored(snap: &Snapshot, parts: &[&str], local: &Value, weapons: &Value) -> Vec<Value> {
    let player = local.get("player").unwrap_or(&Value::Null);
    let mut out = Vec::new();
    if parts.contains(&"pose") {
        if let Some(wanted) = snap.velocity {
            let got = player.get("velocity").and_then(vec3);
            let mismatch =
                got.is_none_or(|g| (0..3).any(|i| (g[i] - wanted[i]).abs() > VELOCITY_TOLERANCE));
            out.push(json!({
                "field": "velocity", "wanted": wanted, "got": got, "mismatch": mismatch,
                "reason": "setpos_exact leaves the player at rest; the console cannot set a velocity (cs_script Teleport can)",
            }));
        }
        if let Some(wanted) = snap.ducked {
            let got = player.get("ducked").and_then(Value::as_bool);
            out.push(json!({
                "field": "ducked", "wanted": wanted, "got": got, "mismatch": got != Some(wanted),
                "reason": "crouch is a held button, not a console state",
            }));
        }
    }
    if parts.contains(&"weapons") {
        let held: Vec<&Value> = weapons
            .get("weapons")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |ws| ws.iter().collect());
        let mut seen: Vec<&str> = Vec::new();
        for w in &snap.weapons {
            // the k-th snapshot copy of a name is compared with the k-th held copy
            let k = seen.iter().filter(|n| **n == w.name).count();
            seen.push(w.name.as_str());
            let copy = held
                .iter()
                .filter(|h| h.get("name").and_then(Value::as_str) == Some(w.name.as_str()))
                .nth(k);
            let suffix = if k > 0 {
                format!("#{}", k + 1)
            } else {
                String::new()
            };
            for (key, wanted, reason) in [
                (
                    "clip1",
                    w.clip1,
                    "ammo is not settable from the console (cs_script SetClipAmmo can)",
                ),
                (
                    "reserve",
                    w.reserve,
                    "reserve ammo is not settable from the console (cs_script SetReserveAmmo can)",
                ),
            ] {
                if let Some(wanted) = wanted {
                    let got = copy.and_then(|c| c.get(key)).and_then(Value::as_i64);
                    out.push(json!({
                        "field": format!("{key}.{}{suffix}", w.name), "wanted": wanted, "got": got,
                        "mismatch": got != Some(wanted), "reason": reason,
                    }));
                }
            }
        }
    }
    out
}

/// `game_restore {name | snapshot, parts?, timeoutMs?}`.
pub async fn game_restore(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let req = parse_restore(&args, &state)?;
    // the pose's validation (coordinates, angles) happens before any bridge read
    if req.parts.contains(&"pose") {
        parse_setpos(&json!({
            "origin": req.snap.origin,
            "angles": req.snap.angles,
            "exact": true,
            "timeoutMs": 1,
        }))?;
    }
    let started = Instant::now();
    let mut bridge = state.bridge.lock().await;
    if ctx.budget.remaining() < WAIT_MARGIN + Duration::from_millis(SAMPLE_SLACK_MS) {
        return Err(ToolError::budget_exceeded(&ctx.budget)
            .retry_safe(true)
            .with_data(json!({"commandsSent": false})));
    }
    let deadline_ms = ctx.budget.deadline_ms()?;
    let pre = call(&mut bridge, "game.local", json!({}), deadline_ms).await?;
    player_of(&pre, false)?;
    let snap = &req.snap;
    let current = pre.get("map").and_then(Value::as_str).unwrap_or_default();
    if current != snap.map {
        return Err(ToolError::new(
            "PrerequisiteFailed",
            "PrerequisiteFailed",
            format!(
                "the snapshot was taken on {:?} and the game is on {current:?}: restore needs the same map (nothing was sent)",
                snap.map
            ),
        )
        .retry_safe(true)
        .with_data(json!({"snapshotMap": snap.map, "currentMap": current, "commandsSent": false})));
    }
    let now_team = pre
        .get("player")
        .and_then(|p| p.get("team"))
        .and_then(Value::as_i64);
    if let (Some(want), Some(now)) = (snap.team, now_team) {
        if want != now {
            return Err(ToolError::new(
                "PrerequisiteFailed",
                "PrerequisiteFailed",
                format!("the snapshot is of team {want} and the player is on team {now}: a team change is not restored (nothing was sent)"),
            )
            .retry_safe(true)
            .with_data(json!({"snapshotTeam": want, "currentTeam": now, "commandsSent": false})));
        }
    }
    let pre_weapons = call(&mut bridge, "game.weapons", json!({}), deadline_ms).await?;
    let reasons = classify(snap, &req.parts, &pre, &pre_weapons);
    let plan = plan_commands(snap, &req.parts, &pre, &pre_weapons);
    // every planned command is checked before the first send (as exec_console_confirmed does)
    for cmd in &plan.0 {
        check_console(cmd)?;
    }
    let deadline = Instant::now() + Duration::from_millis(req.timeout_ms);
    let mut run = Run::default();
    let restored = match restore_steps(
        &mut bridge,
        &ctx,
        &req,
        &pre,
        &pre_weapons,
        &plan,
        &reasons,
        deadline,
        &mut run,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(sent_error(e, &run)),
    };
    drop(bridge);
    let parts = req.parts.as_slice();
    let settled = restored.settled;
    let ok = settled.entries.iter().all(|e| e["ok"] == json!(true));
    let mut approx = Vec::new();
    if parts.contains(&"armor") && snap.armor.is_some() {
        approx.push("armor");
    }
    let mut out = json!({
        "restored": ok,
        "name": snap.name,
        "map": snap.map,
        "parts": parts,
        "inputCleared": restored.input_cleared,
        "commands": run.commands,
        "comparison": settled.entries,
        "approx": approx,
        "notRestored": not_restored(snap, parts, &settled.local, &settled.weapons),
        "extraWeapons": extra_weapons(snap, &settled.weapons),
        "budgetExhausted": settled.budget_exhausted,
        "waitedMs": millis(started.elapsed()),
        "samples": run.samples,
        "selects": run.selects,
        "source": "setpos_exact via the setpos path (exact); console give/ent_fire/game.selectWeapon; comparison from game.local + game.weapons",
    });
    if let Some(pose) = restored.pose {
        out["pose"] = pose;
    }
    Ok(out.into())
}

pub fn snapshot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "pattern": "^[A-Za-z0-9_\\-]{1,64}$"},
            "export": {"type": "boolean", "default": false}
        },
        "required": ["name"]
    })
}

pub fn restore_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "pattern": "^[A-Za-z0-9_\\-]{1,64}$", "description": "exactly one of name or snapshot"},
            "snapshot": {"type": "object", "description": "inline snapshot in the game_snapshot form"},
            "parts": {"type": "array", "items": {"type": "string", "enum": PARTS}, "minItems": 1},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_MS, "default": TIMEOUT_DEFAULT_MS}
        }
    })
}

pub fn register(r: &mut Registry<ClientState>) {
    r.register(
        ToolSpec::new(
            "game_snapshot",
            "Save the local player (pose, velocity, health, armor, money, team, weapons) and the round summary under name (32 at most, in this process). export: true also writes artifacts/snapshots/<name>.json.",
            Mutability::ReadOnly,
            &["game.local", "game.weapons"],
        )
        .with_schema(snapshot_schema()),
        game_snapshot,
    );
    r.register(
        ToolSpec::new(
            "game_restore",
            "Restore a snapshot on the same map: pose, health, armor, missing weapons, then the active weapon. Returns restored, comparison, notRestored; armor/helmet removal and extra weapon copies cannot be fixed. Refused: another map or team. Once sent, errors are not retry-safe.",
            Mutability::Mutating,
            &["console.exec", "game.local", "game.weapons", "game.selectWeapon"],
        )
        .destructive(true)
        .with_schema(restore_schema()),
        game_restore,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(json_extra: Value) -> Snapshot {
        let mut v = json!({
            "map": "m", "player": {"origin": [1.0, 2.0, 3.0], "angles": [0.0, 90.0, 0.0], "health": 80,
                "armor": 0, "helmet": false, "activeWeapon": "weapon_ak47"},
            "weapons": [{"name": "weapon_ak47", "clip1": 30, "reserve": 90}, {"name": "weapon_ak47"}],
        });
        if let (Value::Object(a), Value::Object(b)) = (&mut v, json_extra) {
            a.extend(b);
        }
        Snapshot::from_json(&v).unwrap()
    }

    #[test]
    fn plan_sends_only_what_differs_in_a_fixed_order() {
        let s = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 100, "helmet": true}}),
        );
        let pre = json!({"player": {"health": 100, "armor": 0, "helmet": false}});
        let pre_w = json!({"weapons": [{"name": "weapon_knife"}, {"name": "weapon_ak47"}]});
        assert_eq!(
            plan_commands(&s, &PARTS, &pre, &pre_w),
            (
                vec![
                    "ent_fire !player sethealth 80".to_string(),
                    "give item_assaultsuit".to_string(),
                    "give weapon_ak47".to_string(),
                ],
                true
            )
        );
        let pre = json!({"player": {"health": 80, "armor": 100, "helmet": true}});
        let pre_w = json!({"weapons": [{"name": "weapon_ak47"}, {"name": "weapon_ak47"}]});
        assert_eq!(plan_commands(&s, &PARTS, &pre, &pre_w), (vec![], false));
    }

    #[test]
    fn plan_gives_kevlar_without_a_wanted_helmet_and_skips_unrequested_parts() {
        let s = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 50, "helmet": false}}),
        );
        let pre = json!({"player": {"health": 80, "armor": 0, "helmet": false}});
        assert_eq!(
            plan_commands(&s, &["armor"], &pre, &json!({"weapons": []})).0,
            vec!["give item_kevlar"]
        );
        assert!(plan_commands(&s, &["pose"], &pre, &json!({"weapons": []}))
            .0
            .is_empty());
    }

    #[test]
    fn classify_names_the_mismatches_the_console_cannot_fix() {
        // armor and helmet that would have to be removed
        let s = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 0, "helmet": false}}),
        );
        let pre = json!({"player": {"armor": 100, "helmet": true}});
        let fields: Vec<String> = classify(&s, &PARTS, &pre, &json!({"weapons": []}))
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(fields, vec!["armor", "helmet"]);
        // more copies than the snapshot had
        let pre_w = json!({"weapons": [{"name": "weapon_ak47"}, {"name": "weapon_ak47"}, {"name": "weapon_ak47"}]});
        let r = classify(&s, &["weapons"], &json!({}), &pre_w);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "weapons.weapon_ak47");
        // a helmet with no armor in the snapshot cannot be given alone
        let s3 = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 0, "helmet": true}}),
        );
        let r3 = classify(
            &s3,
            &["armor"],
            &json!({"player": {"armor": 0, "helmet": false}}),
            &json!({"weapons": []}),
        );
        assert_eq!(r3.len(), 1);
        assert_eq!(r3[0].0, "armor");
        // nothing when the state can be reached
        let s2 = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 100, "helmet": true}}),
        );
        assert!(classify(
            &s2,
            &PARTS,
            &json!({"player": {"armor": 0, "helmet": false}}),
            &json!({"weapons": []})
        )
        .is_empty());
    }

    #[test]
    fn compare_reports_each_field_and_the_missing_weapon() {
        let s = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 100, "helmet": true, "activeWeapon": "weapon_ak47"}}),
        );
        let local = json!({
            "viewAngles": [0.5, 90.5, 0.0],
            "player": {"origin": [2.0, 2.0, 3.0], "health": 80, "armor": 100, "helmet": true,
                "activeWeapon": {"name": "weapon_knife"}}
        });
        let weapons = json!({"weapons": [{"name": "weapon_ak47"}]});
        let entries = compare(&s, &PARTS, &local, &weapons, true, &[]);
        let get = |f: &str| {
            entries
                .iter()
                .find(|e| e["field"] == json!(f))
                .cloned()
                .unwrap()
        };
        assert_eq!(get("origin")["ok"], json!(true));
        assert_eq!(get("angles")["ok"], json!(true));
        assert_eq!(get("health")["ok"], json!(true));
        assert_eq!(get("armor")["ok"], json!(true));
        assert_eq!(get("helmet")["ok"], json!(true));
        assert_eq!(get("weapons.weapon_ak47")["ok"], json!(false));
        assert_eq!(get("weapons.weapon_ak47")["got"], json!(1));
        assert_eq!(get("weapons.weapon_ak47")["wanted"], json!(2));
        assert_eq!(get("activeWeapon")["ok"], json!(false));
        assert!(!settled_ok(&entries));
        assert!(!weapons_ok(&entries));
        // with the active entry left out, two ak47s held settle everything else
        let early = compare(
            &s,
            &PARTS,
            &local,
            &json!({"weapons": [{"name": "weapon_ak47"}, {"name": "weapon_ak47"}]}),
            false,
            &[],
        );
        assert!(settled_ok(&early));
        assert!(early.iter().all(|e| e["field"] != json!("activeWeapon")));
    }

    #[test]
    fn classified_fields_are_reported_false_and_not_waited_for() {
        let s = snap(
            json!({"player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "armor": 0, "helmet": false}}),
        );
        let local = json!({"viewAngles": [0.0, 90.0, 0.0],
            "player": {"origin": [1.0, 2.0, 3.0], "health": 80, "armor": 100, "helmet": true, "activeWeapon": null}});
        let reasons = vec![
            ("armor".to_string(), "no removal".to_string()),
            ("helmet".to_string(), "no removal".to_string()),
        ];
        let entries = compare(
            &s,
            &["pose", "health", "armor"],
            &local,
            &json!({"weapons": []}),
            true,
            &reasons,
        );
        let armor = entries
            .iter()
            .find(|e| e["field"] == json!("armor"))
            .unwrap();
        assert_eq!(armor["ok"], json!(false));
        assert_eq!(armor["reason"], json!("no removal"));
        assert!(
            settled_ok(&entries),
            "a classified mismatch is not waited for"
        );
        assert!(
            !entries.iter().all(|e| e["ok"] == json!(true)),
            "but it still makes the restore false"
        );
    }

    #[test]
    fn extra_weapons_and_not_restored_report_got_and_mismatch() {
        let s = snap(json!({
            "player": {"origin": [1,2,3], "angles": [0,90,0], "health": 80, "velocity": [0,0,0],
                "ducked": true, "money": 800, "team": 3},
            "rules": {"roundStartTime": 1.0, "roundsPlayed": 2},
            "weapons": [{"name": "weapon_ak47", "clip1": 30, "reserve": 90}]
        }));
        let held = json!({"weapons": [{"name": "weapon_ak47", "clip1": 12, "reserve": 90}, {"name": "weapon_glock"}]});
        assert_eq!(extra_weapons(&s, &held), vec!["weapon_glock"]);
        let local = json!({"player": {"velocity": [0.0, 0.0, 0.0], "ducked": false}});
        let nr = not_restored(&s, &["pose", "health", "armor", "weapons"], &local, &held);
        let by = |f: &str| nr.iter().find(|e| e["field"] == json!(f)).cloned().unwrap();
        assert_eq!(by("velocity")["mismatch"], json!(false));
        assert_eq!(by("ducked")["mismatch"], json!(true));
        assert_eq!(by("clip1.weapon_ak47")["got"], json!(12));
        assert_eq!(by("clip1.weapon_ak47")["mismatch"], json!(true));
        assert_eq!(by("reserve.weapon_ak47")["mismatch"], json!(false));
        // money and round state belong to no part: not claimed
        assert!(nr
            .iter()
            .all(|e| e["field"] != json!("money") && e["field"] != json!("rules")));
        // only requested parts
        assert!(not_restored(&s, &["health"], &local, &held).is_empty());
    }

    #[test]
    fn parse_restore_orders_parts_and_validates() {
        let state = test_state();
        let args = json!({"snapshot": {"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 9}, "weapons": []}, "parts": ["weapons", "pose"]});
        let r = parse_restore(&args, &state).unwrap();
        assert_eq!(r.parts, vec!["pose", "weapons"]);
        assert_eq!(r.timeout_ms, TIMEOUT_DEFAULT_MS);
        for bad in [
            json!({}),
            json!({"name": "a", "snapshot": {}}),
            json!({"name": "nope"}),
            json!({"name": "a/b"}),
            json!({"snapshot": {}, "parts": []}),
            json!({"snapshot": {}, "parts": ["money"]}),
            json!({"snapshot": {}, "timeoutMs": 0}),
            json!({"snapshot": {}, "bogus": 1}),
        ] {
            let e = parse_restore(&bad, &state).err().expect("refused");
            assert_eq!(e.name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn export_names_are_unique_and_leave_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("cs2mcp-export-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body = json!({"name": "u"});
        let a = write_export(&dir, "u", &body).unwrap();
        let b = write_export(&dir, "u", &body).unwrap();
        assert_eq!(a, b);
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("snapshots"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // a file where the directory should be: the export fails with ExportFailed
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let err = write_export(&blocked, "u", &body).unwrap_err();
        assert_eq!(err.name, "ExportFailed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_state() -> ClientState {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: crate::state::ClientConfig,
        }
        // no host project tree and no real podman: the state is built from nonexistent paths
        let cfg = W::parse_from([
            "t",
            "--discovery",
            "/nonexistent/discovery.json",
            "--project-root",
            "/nonexistent/cs2mcp-test-project",
            "--podman",
            "/nonexistent/cs2mcp-test-podman",
        ])
        .c;
        ClientState::from_config(&cfg).unwrap()
    }
}
