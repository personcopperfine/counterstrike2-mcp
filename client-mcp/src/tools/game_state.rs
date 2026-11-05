//! In-game state read through the bridge (workflows/11-game-state.md): `game_state` (local view,
//! position, velocity, active weapon), `weapons` (inventory + ammo), `radar` (every controller/pawn,
//! bomb, hostages, spotted flags, projected onto the map's radar overview), `select_weapon` (the
//! engine's own `slotN`/`invnext` commands and CCSGOInput `MakeWeaponSelection`), `sounds_drain`/`sounds_status`
//! (sound events recorded by the bridge's `ISoundOpSystem` wrap).
//!
//! Entity fields are resolved by the bridge through the game's SchemaSystem at runtime; this crate
//! only names classes/fields in docs and never carries offsets. Bridge handlers:
//! `mcp_bridge_game.inc`. Live-verified 2026-10-11 for game_state/weapons/radar/select_weapon/sounds
//! in wrap mode; listener mode failed live (`research/evidence/2026-10-11-game-state-probe-weaponselect.json`).
pub use cs2_mcp_common::play::wait::is_weapon_name;
use crate::state::ClientState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Radar overview metadata from `resource/overviews/<map>.txt` (CS:GO/CS2 KeyValues).
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    pub map: String,
    pub material: Option<String>,
    pub pos_x: f64,
    pub pos_y: f64,
    pub scale: f64,
    pub rotate: Option<f64>,
    pub zoom: Option<f64>,
}

/// Radar images are 1024x1024 (`scale` = world units per pixel at that size).
pub const RADAR_IMAGE_SIZE: f64 = 1024.0;

impl Overview {
    /// Minimal KeyValues reader: `"key" "value"` pairs anywhere in the file (comments stripped).
    pub fn parse(map: &str, text: &str) -> Result<Overview, String> {
        let mut kv: Map<String, Value> = Map::new();
        for raw in text.lines() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line
                .split('"')
                .filter(|s| !s.trim().is_empty() && s.trim() != "\t");
            let (Some(k), Some(v)) = (parts.next(), parts.next()) else {
                continue;
            };
            kv.insert(k.trim().to_ascii_lowercase(), json!(v.trim()));
        }
        let num = |k: &str| {
            kv.get(k)
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
        };
        let (Some(pos_x), Some(pos_y), Some(scale)) = (num("pos_x"), num("pos_y"), num("scale"))
        else {
            return Err(format!("{map}: overview is missing pos_x/pos_y/scale"));
        };
        if scale <= 0.0 {
            return Err(format!("{map}: overview scale must be > 0"));
        }
        Ok(Overview {
            map: map.to_string(),
            material: kv
                .get("material")
                .and_then(Value::as_str)
                .map(str::to_string),
            pos_x,
            pos_y,
            scale,
            rotate: num("rotate"),
            zoom: num("zoom"),
        })
    }

    /// World (x, y) -> radar pixel (px, py) in the 1024x1024 image (y grows downward).
    pub fn project(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.pos_x) / self.scale, (self.pos_y - y) / self.scale)
    }

    pub fn to_json(&self, file: &Path) -> Value {
        json!({
            "map": self.map, "file": file.display().to_string(), "material": self.material,
            "posX": self.pos_x, "posY": self.pos_y, "scale": self.scale, "rotate": self.rotate,
            "zoom": self.zoom, "imageSize": RADAR_IMAGE_SIZE,
            "note": "radar px = (x - posX)/scale, py = (posY - y)/scale; uv = px/imageSize",
        })
    }
}

/// Load `<dir>/<map>.txt`; `None` when the file is absent (not an error: the radar still works in world units).
pub fn load_overview(dir: &Path, map: &str) -> Result<Option<(Overview, PathBuf)>, String> {
    let ok = !map.is_empty()
        && map
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(format!(
            "map name {map:?} is not a plain file stem (letters, digits, _ and - only)"
        ));
    }
    let file = dir.join(format!("{map}.txt"));
    match std::fs::read_to_string(&file) {
        Ok(text) => Overview::parse(map, &text).map(|o| Some((o, file))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

fn add_projection(v: &mut Value, ov: &Overview) {
    if let Some(origin) = v.get("origin").and_then(Value::as_array) {
        if let (Some(x), Some(y)) = (
            origin.first().and_then(Value::as_f64),
            origin.get(1).and_then(Value::as_f64),
        ) {
            let (px, py) = ov.project(x, y);
            v["radar"] = json!([px, py]);
            v["radarUv"] = json!([px / RADAR_IMAGE_SIZE, py / RADAR_IMAGE_SIZE]);
        }
    }
}

async fn bridge_call(
    state: &ClientState,
    ctx: &CallCtx,
    method: &str,
    params: Value,
) -> Result<Value, ToolError> {
    let mut bridge = state.bridge.lock().await;
    let deadline = ctx.budget.deadline_ms()?;
    Ok(bridge
        .request(method, params, RequestOpts::deadline(deadline))
        .await?
        .result)
}

/// `game.local` as-is plus the schema/evidence note.
pub async fn game_state(
    state: ClientState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    Ok(cs2_mcp_common::play::state::read(&mut bridge, &ctx).await?.into())
}

/// `game.weapons` as-is.
pub async fn weapons(
    state: ClientState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut v = bridge_call(&state, &ctx, "game.weapons", json!({})).await?;
    v["source"] = json!("bridge game.weapons");
    Ok(v.into())
}

/// `game.radar` + overview projection (`--overviews-dir/<map>.txt`).
pub async fn radar(state: ClientState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let mut v = bridge_call(&state, &ctx, "game.radar", json!({})).await?;
    let map_name = args
        .get("map")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| v.get("map").and_then(Value::as_str).map(str::to_string));
    let mut overview: Option<Overview> = None;
    match map_name.as_deref() {
        Some(map) => match load_overview(&state.overviews_dir, map) {
            Ok(Some((ov, file))) => {
                v["overview"] = ov.to_json(&file);
                overview = Some(ov);
            }
            Ok(None) => {
                v["overview"] = Value::Null;
                v["overviewNote"] = json!(format!(
                    "no {map}.txt under {}; positions are world units only",
                    state.overviews_dir.display()
                ));
            }
            Err(e) => {
                v["overview"] = Value::Null;
                v["overviewNote"] = json!(e);
            }
        },
        None => {
            v["overview"] = Value::Null;
            v["overviewNote"] =
                json!("no map name from the bridge (not in a game?); pass {map} to project anyway");
        }
    }
    if let Some(ov) = &overview {
        for key in ["players", "bomb", "hostages"] {
            if let Some(items) = v.get_mut(key).and_then(Value::as_array_mut) {
                for item in items {
                    add_projection(item, ov);
                }
            }
        }
    }
    v["source"] = json!("bridge game.radar; spotted flags are the game's EntitySpottedState_t, every entity is listed regardless");
    Ok(v.into())
}

/// `game.rules` as-is: C_CSGameRules through the cs_gamerules proxy (warmup, freeze, round state).
pub async fn game_rules(
    state: ClientState,
    ctx: CallCtx,
    _args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    Ok(cs2_mcp_common::play::state::rules(&mut bridge, &ctx).await?.into())
}

/// `game.entities` -> entities matching `class` and/or `name`, with origin, angles and health.
pub async fn entity_find(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut bridge = state.bridge.lock().await;
    cs2_mcp_common::play::entities::find(&mut bridge, &ctx, &args).await
}

/// Inventory ConCommands registered by client.dll on 2000930 (`invnextgun` & co. are not commands).
pub const SELECT_COMMANDS: [&str; 5] = [
    "invnext",
    "invprev",
    "invnextselect",
    "invprevselect",
    "lastinv",
];

/// Validate `select_weapon` arguments client-side (mirrors the bridge allow-list) -> wire params.
pub fn select_params(args: &Value) -> Result<Value, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    let given: Vec<&str> = ["slot", "command", "weapon", "defIndex", "handle"]
        .into_iter()
        .filter(|k| obj.contains_key(*k))
        .collect();
    if given.len() != 1 {
        return Err(ToolError::invalid_arguments("exactly one of slot (0..13), command (invnext|invprev|lastinv|...), weapon (weapon_x), defIndex or handle is required"));
    }
    if let Some(s) = obj.get("slot") {
        let n = s
            .as_i64()
            .ok_or_else(|| ToolError::invalid_arguments("slot must be an integer"))?;
        if !(0..=13).contains(&n) {
            return Err(ToolError::invalid_arguments("slot must be 0..13"));
        }
        return Ok(json!({"slot": n}));
    }
    if let Some(c) = obj.get("command") {
        let c = c
            .as_str()
            .ok_or_else(|| ToolError::invalid_arguments("command must be a string"))?;
        if !SELECT_COMMANDS.contains(&c) {
            return Err(ToolError::invalid_arguments(format!(
                "command must be one of {}",
                SELECT_COMMANDS.join("|")
            )));
        }
        return Ok(json!({"command": c}));
    }
    if let Some(d) = obj.get("defIndex") {
        let d = d
            .as_u64()
            .filter(|d| *d <= u64::from(u16::MAX))
            .ok_or_else(|| ToolError::invalid_arguments("defIndex must be an integer 0..65535"))?;
        return Ok(json!({"defIndex": d}));
    }
    if let Some(h) = obj.get("handle") {
        let h = h
            .as_u64()
            .filter(|h| *h <= u64::from(u32::MAX))
            .ok_or_else(|| {
                ToolError::invalid_arguments("handle must be a u32 entity handle (from weapons)")
            })?;
        return Ok(json!({"handle": h}));
    }
    let w = obj["weapon"]
        .as_str()
        .ok_or_else(|| ToolError::invalid_arguments("weapon must be a string"))?;
    if !is_weapon_name(w) {
        return Err(ToolError::invalid_arguments(
            "weapon must look like weapon_ak47 (lower-case, weapon_ prefix)",
        ));
    }
    Ok(json!({"weapon": w}))
}

/// `game.selectWeapon`: `slotN` / inventory commands on the engine command buffer, or a specific held weapon
/// (`weapon` / `defIndex` / `handle`) through CCSGOInput `MakeWeaponSelection` -> usercmd `weaponselect`.
pub async fn select_weapon(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let params = select_params(&args)?;
    let mut v = bridge_call(&state, &ctx, "game.selectWeapon", params).await?;
    v["note"] = json!("applied on the next usercmd (slot/command: engine command buffer, like a key bind; weapon/defIndex/handle: MakeWeaponSelection -> weaponselect); confirm with weapons/game_state after a tick");
    if let Some(next) = select_next(&v, &args) {
        v["next"] = json!(next);
    }
    Ok(v.into())
}

/// `game_wait` hint after a weapon selection. The reply's `weapon` is what the bridge actually
/// picked (`weapon_knife` lands on `weapon_knife_t` for a T), so it wins: by defIndex, or by its
/// name when the defIndex is missing or negative. Only when the reply has no `weapon` does the
/// argument (`weapon` or `defIndex`) stand in; `handle` alone gives no hint.
fn select_next(result: &Value, args: &Value) -> Option<String> {
    let by_name = |n: &str| {
        is_weapon_name(n).then(|| format!("game_wait {{\"until\":\"weapon\",\"weapon\":\"{n}\"}}"))
    };
    let by_def = |d: u64| {
        (d <= u64::from(u16::MAX))
            .then(|| format!("game_wait {{\"until\":\"weapon\",\"defIndex\":{d}}}"))
    };
    match result.get("weapon").filter(|w| w.is_object()) {
        Some(w) => match w.get("defIndex").and_then(Value::as_i64) {
            Some(d) if d >= 0 => by_def(d as u64),
            _ => w.get("name").and_then(Value::as_str).and_then(by_name),
        },
        None => match args.get("weapon").and_then(Value::as_str) {
            Some(n) => by_name(n),
            None => args
                .get("defIndex")
                .and_then(Value::as_u64)
                .and_then(by_def),
        },
    }
}

/// `sound.drain {max?, resolve?}` -> the bridge's ring buffer since the last drain.
pub async fn sounds_drain(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut params = Map::new();
    if let Some(m) = args.get("max") {
        let m = m
            .as_u64()
            .ok_or_else(|| ToolError::invalid_arguments("max must be a positive integer"))?;
        if m == 0 || m > 256 {
            return Err(ToolError::invalid_arguments(
                "max must be 1..256 (bridge ring size)",
            ));
        }
        params.insert("max".into(), json!(m));
    }
    if let Some(r) = args.get("resolve") {
        let r = r
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("resolve must be a boolean"))?;
        params.insert("resolve".into(), json!(r));
    }
    let mut v = bridge_call(&state, &ctx, "sound.drain", Value::Object(params)).await?;
    v["note"] = json!("events recorded by the ISoundOpSystem wrap (slots 11/12/13) since the previous drain; origin = source entity's current position at drain time; if truncated is true, call again for the rest");
    Ok(v.into())
}

/// `sound.subscribe {mode?}` / `sound.status`: selects the sink (`wrap` default, `listener` = the
/// game's own slot-81 registry, `both`) and reports ring counters.
pub async fn sounds_status(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let mut params = Map::new();
    if let Some(m) = args.get("mode") {
        let m = m
            .as_str()
            .filter(|m| SOUND_MODES.contains(m))
            .ok_or_else(|| ToolError::invalid_arguments("mode must be wrap|listener|both"))?;
        params.insert("mode".into(), json!(m));
    }
    let method = if params.is_empty() {
        "sound.status"
    } else {
        "sound.subscribe"
    };
    let mut v = bridge_call(&state, &ctx, method, Value::Object(params)).await?;
    v["note"] = json!("wrap = vtable copy wrapping ISoundOpSystem slots 11/12/13 (hash+entity, API entry); listener = the engine's slot-81 sound-event listener (recorded nothing for top-level starts in the 2026-10-11 live run; keep wrap).");
    Ok(v.into())
}

pub const SOUND_MODES: [&str; 3] = ["wrap", "listener", "both"];

pub fn register(r: &mut cs2_mcp_common::tools::Registry<ClientState>) {
    r.register(
        ToolSpec::new("game_state", "Local player: map, tick, view angles, origin/eye/velocity, health, flags, active weapon.", Mutability::ReadOnly, &["game.local"]),
        game_state,
    );
    r.register(
        ToolSpec::new("weapons", "Local inventory: active weapon and every held weapon (entindex, name, defIndex, clip1/clip2/reserve).", Mutability::ReadOnly, &["game.weapons"]),
        weapons,
    );
    r.register(
        ToolSpec::new("radar", "Players (team, alive, health, origin, eye yaw, spotted), bomb and hostages, projected onto the map overview.", Mutability::ReadOnly, &["game.radar"])
            .with_schema(json!({"type": "object", "properties": {"map": {"type": "string"}}})),
        radar,
    );
    r.register(
        ToolSpec::new("game_rules", cs2_mcp_common::play::state::RULES_DESCRIPTION, Mutability::ReadOnly, &["game.rules"]),
        game_rules,
    );
    r.register(
        ToolSpec::new("entity_find", "Find entities in the RUNNING game by class (designer name) and/or name (targetname). Not for: the Hammer document (hammer-mcp). Returns entindex, class, name, origin, angles, health, toggleState; truncated:true when more matched. Needs class or name; max 1..256.", Mutability::ReadOnly, &["game.entities"])
            .with_schema(cs2_mcp_common::play::entities::schema()),
        entity_find,
    );
    r.register(
        ToolSpec::new("select_weapon","Select a weapon without keypresses. One of: slot 0..13 (slot1 primary, slot2 pistol, slot3 knife, slot4 grenades, slot5 C4), command, weapon (designer name; weapon_knife matches any knife), defIndex or handle (from weapons).", Mutability::Mutating, &["game.selectWeapon"])
            .with_schema(json!({"type": "object", "properties": {"slot": {"type": "integer", "minimum": 0, "maximum": 13}, "command": {"type": "string", "enum": SELECT_COMMANDS}, "weapon": {"type": "string", "pattern": "^weapon_[a-z0-9_]+$"}, "defIndex": {"type": "integer", "minimum": 0, "maximum": 65535}, "handle": {"type": "integer", "minimum": 0, "maximum": 4294967295u64}}})),
        select_weapon,
    );
    r.register(
        ToolSpec::new("sounds_drain", "Drain recorded game sound events (name, source entindex, origin). Empties the ring (256; dropped counts overflow); installs the hook on first use.", Mutability::Mutating, &["sound.drain"])
            .with_schema(json!({"type": "object", "properties": {"max": {"type": "integer", "minimum": 1, "maximum": 256}, "resolve": {"type": "boolean", "default": true}}})),
        sounds_drain,
    );
    r.register(
        ToolSpec::new("sounds_status", "Install the sound-event sink (idempotent) and report ring counters. mode: wrap (default), listener, both.", Mutability::Mutating, &["sound.status", "sound.subscribe"])
            .with_schema(json!({"type": "object", "properties": {"mode": {"type": "string", "enum": SOUND_MODES}}})),
        sounds_status,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUST2: &str = r#"// HLTV overview description file for de_dust2_v2.bsp

"de_dust2"
{
	"material"	"overviews/de_dust2_v2"	// texture file
	"pos_x"		"-2476"	// upper left world coordinate
	"pos_y"		"3239"
	"scale"		"4.4"
	"rotate"	"1"
	"zoom"		"1.1"
}
"#;

    #[test]
    fn parses_overview_and_projects() {
        let ov = Overview::parse("de_dust2", DUST2).unwrap();
        assert_eq!(ov.pos_x, -2476.0);
        assert_eq!(ov.pos_y, 3239.0);
        assert_eq!(ov.scale, 4.4);
        assert_eq!(ov.material.as_deref(), Some("overviews/de_dust2_v2"));
        assert_eq!(ov.project(-2476.0, 3239.0), (0.0, 0.0));
        let (px, py) = ov.project(-2476.0 + 4.4 * 512.0, 3239.0 - 4.4 * 512.0);
        assert!((px - 512.0).abs() < 1e-9 && (py - 512.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_incomplete_overviews_and_bad_names() {
        assert!(Overview::parse("x", "\"x\" { \"pos_x\" \"1\" }").is_err());
        assert!(load_overview(Path::new("/nonexistent"), "../etc").is_err());
        assert_eq!(
            load_overview(Path::new("/nonexistent-dir"), "de_dust2").unwrap(),
            None
        );
    }

    #[test]
    fn select_params_allow_list() {
        assert_eq!(
            select_params(&json!({"slot": 2})).unwrap(),
            json!({"slot": 2})
        );
        assert_eq!(
            select_params(&json!({"command": "invnext"})).unwrap(),
            json!({"command": "invnext"})
        );
        assert_eq!(
            select_params(&json!({"weapon": "weapon_ak47"})).unwrap(),
            json!({"weapon": "weapon_ak47"})
        );
        assert_eq!(
            select_params(&json!({"defIndex": 42})).unwrap(),
            json!({"defIndex": 42})
        );
        assert_eq!(
            select_params(&json!({"handle": 303})).unwrap(),
            json!({"handle": 303})
        );
        for bad in [
            json!({}),
            json!({"command": "invnextgun"}),
            json!({"defIndex": 70000}),
            json!({"handle": -1}),
            json!({"weapon": "weapon_ak47", "defIndex": 7}),
            json!({"slot": 14}),
            json!({"slot": 1, "command": "invnext"}),
            json!({"command": "quit"}),
            json!({"weapon": "ak47"}),
            json!({"weapon": "weapon_AK47; quit"}),
        ] {
            assert!(select_params(&bad).is_err(), "{bad}");
        }
    }
}
