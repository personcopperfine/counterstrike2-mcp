//! The game group's result builders, as pure functions over plain inputs (the brief's test 1):
//! each one transcribes a C `sb_add` sequence from `bridge/mcp_bridge_game.inc` /
//! `bridge/mcp_bridge.c` member for member, so a test can pin the exact bytes against values
//! read off the C. The game-memory half of every call site is a thin reader in [`super`]/
//! [`super::schema`] that fills the plain structs these builders take; the builders themselves
//! never touch game memory.

use crate::entity_json::Sb;

/// `sb_addf3` (`mcp_bridge.c:2648-2654`): `[f,f,f]` through [`Sb::addf`]. Not in
/// `entity_json.rs` (this crate's shared region), so it lives with its callers.
pub(crate) fn addf3(out: &mut Sb<'_>, v: &[f32; 3]) {
    out.add(b"[");
    out.addf(v[0]);
    out.add(b",");
    out.addf(v[1]);
    out.add(b",");
    out.addf(v[2]);
    out.add(b"]");
}

/// `gs_add_kv_str` (`mcp_bridge_game.inc:324-327`): `"key":<json string or null>`.
pub(crate) fn add_kv_str(out: &mut Sb<'_>, key: &[u8], v: Option<&[u8]>) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    match v {
        Some(v) => out.addjson(v),
        None => out.add(b"null"),
    }
}

/// `gs_add_kv_i` (`mcp_bridge_game.inc:328`): `"key":<integer>` (`long long`).
pub(crate) fn add_kv_i(out: &mut Sb<'_>, key: &[u8], v: i64) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    out.addi(v);
}

/// `gs_add_kv_f` (`mcp_bridge_game.inc:329`): `"key":<float>` (`sb_addf`).
pub(crate) fn add_kv_f(out: &mut Sb<'_>, key: &[u8], v: f32) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    out.addf(v);
}

/// `gs_add_kv_b` (`mcp_bridge_game.inc:330`): `"key":true|false`.
pub(crate) fn add_kv_b(out: &mut Sb<'_>, key: &[u8], v: bool) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    out.add(if v { b"true" } else { b"false" });
}

/// `gs_add_kv_v3` (`mcp_bridge_game.inc:331-334`): `"key":[f,f,f]` or `"key":null`.
pub(crate) fn add_kv_v3(out: &mut Sb<'_>, key: &[u8], v: Option<&[f32; 3]>) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    match v {
        Some(v) => addf3(out, v),
        None => out.add(b"null"),
    }
}

/// `gs_fld_i` (`mcp_bridge_game.inc:354-359`) as a builder: the value or `null`.
pub(crate) fn add_kv_i_opt(out: &mut Sb<'_>, key: &[u8], v: Option<i64>) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    match v {
        Some(v) => out.addi(v),
        None => out.add(b"null"),
    }
}

/// `gs_fld_b` (`mcp_bridge_game.inc:342-347`) as a builder: the value or `null`.
pub(crate) fn add_kv_b_opt(out: &mut Sb<'_>, key: &[u8], v: Option<bool>) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    match v {
        Some(true) => out.add(b"true"),
        Some(false) => out.add(b"false"),
        None => out.add(b"null"),
    }
}

/// `gs_fld_f` (`mcp_bridge_game.inc:360-365`) as a builder: the value or `null`.
pub(crate) fn add_kv_f_opt(out: &mut Sb<'_>, key: &[u8], v: Option<f32>) {
    out.add(b"\"");
    out.add(key);
    out.add(b"\":");
    match v {
        Some(v) => out.addf(v),
        None => out.add(b"null"),
    }
}

/// `gs_miss_add` (`mcp_bridge_game.inc:337-341`): the comma-joined entry of a
/// `schemaMissing` builder.
pub(crate) fn miss_add(out: &mut Sb<'_>, field: &[u8]) {
    if !out.as_cstr().is_empty() {
        out.add(b",");
    }
    out.addjson(field);
}

/// A weapon object, the plain-input form of `gs_weapon_json`
/// (`mcp_bridge_game.inc:368-393`, C_BasePlayerWeapon / C_EconEntity); filled by
/// `schema::read_weapon`.
pub(crate) struct WeaponJson {
    /// `entindex`: `gs_ent_index(w)`, -1 when the identity is unreadable.
    pub entindex: i32,
    /// `handle`: the identity's `m_EHandle`, `INVALID_EHANDLE_INDEX` when unreadable.
    pub handle: u32,
    /// `name`: the weapon entity's designer name, null when unreadable.
    pub name: Option<Vec<u8>>,
    /// `defIndex`: `C_EconItemView::m_iItemDefinitionIndex` through the attribute chain, -1
    /// when any hop fails.
    pub def_index: i32,
    /// `clip1` / `clip2`: `m_iClip1` / `m_iClip2`, -1 when the field does not resolve.
    pub clip1: i32,
    pub clip2: i32,
    /// `reserve`: the first entry of `m_pReserveAmmo`, -1 when the field does not resolve.
    pub reserve: i32,
    /// `reloading`: `C_CSWeaponBase::m_bInReload`, false when the field does not resolve.
    pub reloading: bool,
}

/// `gs_weapon_json` (`mcp_bridge_game.inc:383-392`).
pub(crate) fn weapon_json(w: &WeaponJson, out: &mut Sb<'_>) {
    out.add(b"{");
    add_kv_i(out, b"entindex", i64::from(w.entindex));
    out.add(b",");
    add_kv_i(out, b"handle", i64::from(w.handle));
    out.add(b",");
    add_kv_str(out, b"name", w.name.as_deref());
    out.add(b",");
    add_kv_i(out, b"defIndex", i64::from(w.def_index));
    out.add(b",");
    add_kv_i(out, b"clip1", i64::from(w.clip1));
    out.add(b",");
    add_kv_i(out, b"clip2", i64::from(w.clip2));
    out.add(b",");
    add_kv_i(out, b"reserve", i64::from(w.reserve));
    out.add(b",");
    add_kv_b(out, b"reloading", w.reloading);
    out.add(b"}");
}

/// The detail block of a player object (`withDetails` in `gs_player_json`,
/// `mcp_bridge_game.inc:434-470`). Every `Option` field is null in the reply when `None`,
/// exactly as the C's failed field reads are.
pub(crate) struct PlayerDetails {
    /// `eye`: origin + `m_vecViewOffset`, present only when both reads resolved.
    pub eye: Option<[f32; 3]>,
    /// `velocity`: `C_BaseEntity::m_vecAbsVelocity`.
    pub velocity: Option<[f32; 3]>,
    /// `lifeState`: `C_BaseEntity::m_lifeState`, 0 when unresolved.
    pub life_state: u8,
    /// `flags`: `C_BaseEntity::m_fFlags`, 0 when unresolved.
    pub flags: u32,
    /// `scoped`: `C_CSPlayerPawn::m_bIsScoped`, false when unresolved.
    pub scoped: bool,
    /// `armor`: `C_CSPlayerPawn::m_ArmorValue` (the C's `gs_fld_i`, null on failure).
    pub armor: Option<i32>,
    /// `helmet`: `CCSPlayerController::m_bPawnHasHelmet` (null on failure).
    pub helmet: Option<bool>,
    /// `money`: the money services' `m_iAccount` (null when the services pointer or the field
    /// does not resolve).
    pub money: Option<i32>,
    /// `ducked`: the movement services' `m_bDucked` (null likewise).
    pub ducked: Option<bool>,
    /// `moveType`: `C_BaseEntity::m_MoveType` (null on failure).
    pub move_type: Option<u8>,
    /// `activeWeapon`: the weapon services' active weapon, null when any hop fails.
    pub active_weapon: Option<WeaponJson>,
}

/// A player object, the plain-input form of `gs_player_json` (`mcp_bridge_game.inc:409-473`),
/// used by game.local (with details) and game.radar (without).
pub(crate) struct PlayerJson {
    /// `controllerIndex`: the controller entity's entindex.
    pub controller_index: i32,
    /// `pawnIndex`: the pawn's entindex, -1 when there is no pawn.
    pub pawn_index: i32,
    /// `name`: the controller's inline `m_iszPlayerName`, null when unreadable.
    pub name: Option<Vec<u8>>,
    /// `team`: `m_iTeamNum`, 0 when unresolved.
    pub team: u8,
    /// `alive`: `m_bPawnIsAlive`, false when unresolved.
    pub alive: bool,
    /// `health`: `m_iPawnHealth`, 0 when unresolved.
    pub health: i32,
    /// `isLocal`: the pawn pointer equals the local pawn.
    pub is_local: bool,
    /// `origin`: the pawn's `m_vecAbsOrigin`.
    pub origin: Option<[f32; 3]>,
    /// `eyeAngles`: `C_CSPlayerPawn::m_angEyeAngles`.
    pub eye_angles: Option<[f32; 3]>,
    /// `spotted`: `m_bSpotted`, false without a pawn.
    pub spotted: bool,
    /// `spottedByMask`: the low word of `m_bSpottedByMask`, 0 without a pawn.
    pub spotted_mask: u32,
    /// game.local's detail block; None for game.radar.
    pub details: Option<PlayerDetails>,
}

/// `gs_player_json` (`mcp_bridge_game.inc:434-471`).
pub(crate) fn player_json(p: &PlayerJson, out: &mut Sb<'_>) {
    out.add(b"{");
    add_kv_i(out, b"controllerIndex", i64::from(p.controller_index));
    out.add(b",");
    add_kv_i(out, b"pawnIndex", i64::from(p.pawn_index));
    out.add(b",");
    add_kv_str(out, b"name", p.name.as_deref());
    out.add(b",");
    add_kv_i(out, b"team", i64::from(p.team));
    out.add(b",");
    add_kv_b(out, b"alive", p.alive);
    out.add(b",");
    add_kv_i(out, b"health", i64::from(p.health));
    out.add(b",");
    add_kv_b(out, b"isLocal", p.is_local);
    out.add(b",");
    add_kv_v3(out, b"origin", p.origin.as_ref());
    out.add(b",");
    add_kv_v3(out, b"eyeAngles", p.eye_angles.as_ref());
    out.add(b",");
    add_kv_b(out, b"spotted", p.spotted);
    out.add(b",");
    add_kv_i(out, b"spottedByMask", i64::from(p.spotted_mask));
    if let Some(d) = &p.details {
        out.add(b",");
        add_kv_v3(out, b"eye", d.eye.as_ref());
        out.add(b",");
        add_kv_v3(out, b"velocity", d.velocity.as_ref());
        out.add(b",");
        add_kv_i(out, b"lifeState", i64::from(d.life_state));
        out.add(b",");
        add_kv_i(out, b"flags", i64::from(d.flags));
        out.add(b",");
        add_kv_b(out, b"scoped", d.scoped);
        out.add(b",");
        // The C's gs_fld_* with miss = 0 (`mcp_bridge_game.inc:463-467`): null on failure,
        // no schemaMissing entry.
        add_kv_i_opt(out, b"armor", d.armor.map(i64::from));
        out.add(b",");
        add_kv_b_opt(out, b"helmet", d.helmet);
        out.add(b",");
        add_kv_i_opt(out, b"money", d.money.map(i64::from));
        out.add(b",");
        add_kv_b_opt(out, b"ducked", d.ducked);
        out.add(b",");
        add_kv_i_opt(out, b"moveType", d.move_type.map(i64::from));
        out.add(b",");
        out.add(b"\"activeWeapon\":");
        match &d.active_weapon {
            Some(w) => weapon_json(w, out),
            None => out.add(b"null"),
        }
    }
    out.add(b"}");
}

/// `gs_session_header` (`mcp_bridge_game.inc:475-484`): `{"tick":..,"inGame":..,"map":..` -
/// no closing brace; every caller continues the object. The C returns `inGame`; every caller
/// ignores it, so this returns nothing.
pub(crate) fn session_header(out: &mut Sb<'_>, tick: i32, in_game: bool, map: Option<&[u8]>) {
    out.add(b"{");
    add_kv_i(out, b"tick", i64::from(tick));
    out.add(b",");
    add_kv_b(out, b"inGame", in_game);
    out.add(b",");
    add_kv_str(out, b"map", map);
}

/// `game.tick`'s result (`mcp_bridge.c:5050-5052`).
pub(crate) fn tick_result(out: &mut Sb<'_>, tick: i32, frames: i32) {
    out.add(b"{\"tick\":");
    out.addi(i64::from(tick));
    out.add(b",\"frames\":");
    out.addi(i64::from(frames));
    out.add(b"}");
}

/// `test.checkpoint` / `test.active`'s result (`mcp_bridge.c:5096`).
pub(crate) fn active_result(out: &mut Sb<'_>, active: bool) {
    out.add(b"{\"active\":");
    out.add(if active { b"true" } else { b"false" });
    out.add(b"}");
}

/// `console.exec`'s result (`mcp_bridge.c:5078`).
pub(crate) fn queued_result(out: &mut Sb<'_>) {
    out.add(b"{\"queued\":true}");
}

/// A planted bomb of game.radar (`mcp_bridge_game.inc:599-606`).
pub(crate) fn bomb_planted_json(
    out: &mut Sb<'_>,
    entindex: u32,
    origin: Option<&[f32; 3]>,
    ticking: bool,
    defused: bool,
    site: i32,
    blow_time: f32,
) {
    out.add(b"{\"state\":\"planted\",");
    add_kv_i(out, b"entindex", i64::from(entindex));
    out.add(b",");
    add_kv_v3(out, b"origin", origin);
    out.add(b",");
    add_kv_b(out, b"ticking", ticking);
    out.add(b",");
    add_kv_b(out, b"defused", defused);
    out.add(b",");
    add_kv_i(out, b"site", i64::from(site));
    out.add(b",");
    add_kv_f(out, b"blowTime", blow_time);
    out.add(b"}");
}

/// A carried or dropped `weapon_c4` of game.radar (`mcp_bridge_game.inc:607-617`).
pub(crate) fn bomb_carried_json(
    out: &mut Sb<'_>,
    carried: bool,
    entindex: u32,
    owner_pawn_index: i32,
    origin: Option<&[f32; 3]>,
) {
    out.add(if carried {
        b"{\"state\":\"carried\","
    } else {
        b"{\"state\":\"dropped\","
    });
    add_kv_i(out, b"entindex", i64::from(entindex));
    out.add(b",");
    add_kv_i(out, b"ownerPawnIndex", i64::from(owner_pawn_index));
    out.add(b",");
    add_kv_v3(out, b"origin", origin);
    out.add(b"}");
}

/// A hostage of game.radar (`mcp_bridge_game.inc:618-635`, written into the 2048-byte
/// hostage scratch).
pub(crate) fn hostage_json(
    out: &mut Sb<'_>,
    entindex: u32,
    origin: Option<&[f32; 3]>,
    state: i32,
    rescued: bool,
    health: i32,
    leader_pawn_index: i32,
) {
    out.add(b"{");
    add_kv_i(out, b"entindex", i64::from(entindex));
    out.add(b",");
    add_kv_v3(out, b"origin", origin);
    out.add(b",");
    add_kv_i(out, b"state", i64::from(state));
    out.add(b",");
    add_kv_b(out, b"rescued", rescued);
    out.add(b",");
    add_kv_i(out, b"health", i64::from(health));
    out.add(b",");
    add_kv_i(out, b"leaderPawnIndex", i64::from(leader_pawn_index));
    out.add(b"}");
}

/// One entity of game.entities (`mcp_bridge_game.inc:750-758`).
pub(crate) struct EntityJson<'a> {
    pub class: Option<&'a [u8]>,
    pub name: Option<&'a [u8]>,
    pub origin: Option<&'a [f32; 3]>,
    pub angles: Option<&'a [f32; 3]>,
    pub health: Option<i32>,
    pub toggle_state: Option<i32>,
}

/// `op_game_entities`' per-entity object (`mcp_bridge_game.inc:750-758`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn entity_json(out: &mut Sb<'_>, entindex: u32, e: &EntityJson<'_>) {
    let EntityJson { class, name, origin, angles, health, toggle_state } = *e;
    out.add(b"{");
    add_kv_i(out, b"entindex", i64::from(entindex));
    out.add(b",");
    add_kv_str(out, b"class", class);
    out.add(b",");
    add_kv_str(out, b"name", name);
    out.add(b",");
    add_kv_v3(out, b"origin", origin);
    out.add(b",");
    add_kv_v3(out, b"angles", angles);
    out.add(b",");
    add_kv_i_opt(out, b"health", health.map(i64::from));
    out.add(b",");
    add_kv_i_opt(out, b"toggleState", toggle_state.map(i64::from));
    out.add(b"}");
}

/// The field list of game.rules after the session header and `proxyIndex`
/// (`mcp_bridge_game.inc:688-700`): every `Option` is a `gs_fld_*` read, null on failure and
/// named in `schema_missing` (the C's `miss` builder, comma-joined JSON strings, already
/// built).
pub(crate) struct RulesJson<'m> {
    pub proxy_index: i32,
    pub warmup: Option<bool>,
    pub freeze: Option<bool>,
    pub team_intro: Option<bool>,
    pub game_restart: Option<bool>,
    pub has_match_started: Option<bool>,
    pub game_phase: Option<i32>,
    pub rounds_played: Option<i32>,
    pub round_start_time: Option<f32>,
    pub restart_round_time: Option<f32>,
    pub round_time_sec: Option<i32>,
    pub round_win_status: Option<i32>,
    pub round_end_reason: Option<i32>,
    pub bomb_planted: Option<bool>,
    /// The comma-joined `schemaMissing` strings (the C's `miss` builder's bytes).
    pub schema_missing: &'m [u8],
}

/// `op_game_rules`' tail after the session header (`mcp_bridge_game.inc:686-701`): the caller
/// writes the header and this writes `,"proxyIndex":N,...`.
pub(crate) fn rules_tail(out: &mut Sb<'_>, r: &RulesJson<'_>) {
    out.add(b",");
    add_kv_i(out, b"proxyIndex", i64::from(r.proxy_index));
    out.add(b",");
    add_kv_b_opt(out, b"warmup", r.warmup);
    out.add(b",");
    add_kv_b_opt(out, b"freeze", r.freeze);
    out.add(b",");
    add_kv_b_opt(out, b"teamIntro", r.team_intro);
    out.add(b",");
    add_kv_b_opt(out, b"gameRestart", r.game_restart);
    out.add(b",");
    add_kv_b_opt(out, b"hasMatchStarted", r.has_match_started);
    out.add(b",");
    add_kv_i_opt(out, b"gamePhase", r.game_phase.map(i64::from));
    out.add(b",");
    add_kv_i_opt(out, b"roundsPlayed", r.rounds_played.map(i64::from));
    out.add(b",");
    add_kv_f_opt(out, b"roundStartTime", r.round_start_time);
    out.add(b",");
    add_kv_f_opt(out, b"restartRoundTime", r.restart_round_time);
    out.add(b",");
    add_kv_i_opt(out, b"roundTimeSec", r.round_time_sec.map(i64::from));
    out.add(b",");
    add_kv_i_opt(out, b"roundWinStatus", r.round_win_status.map(i64::from));
    out.add(b",");
    add_kv_i_opt(out, b"roundEndReason", r.round_end_reason.map(i64::from));
    out.add(b",");
    add_kv_b_opt(out, b"bombPlanted", r.bomb_planted);
    out.add(b",\"schemaMissing\":[");
    out.add(r.schema_missing);
    out.add(b"]}");
}

/// One `game.schemaProbe` result row (`mcp_bridge_game.inc:1236-1239`). `is_global` is the
/// C's `int isGlobal`: -1 when the scope is not the pinned class, else the slot's bool.
pub(crate) fn probe_result(
    out: &mut Sb<'_>,
    name: &[u8],
    found: bool,
    scope_name: Option<&[u8]>,
    is_global: i64,
) {
    out.add(b"{");
    add_kv_str(out, b"name", Some(name));
    out.add(b",");
    add_kv_b(out, b"found", found);
    out.add(b",");
    add_kv_str(out, b"scopeName", scope_name);
    out.add(b",");
    add_kv_i(out, b"isGlobal", is_global);
    out.add(b"}");
}

/// `game.selectWeapon`'s command result (`mcp_bridge_game.inc:887`).
pub(crate) fn select_command_result(out: &mut Sb<'_>, cmd: &[u8]) {
    out.add(b"{\"queued\":true,\"via\":\"command\",\"command\":");
    out.addjson(cmd);
    out.add(b"}");
}

/// `game.selectWeapon`'s MakeWeaponSelection result (`mcp_bridge_game.inc:876-878`).
pub(crate) fn select_weapon_result(out: &mut Sb<'_>, w: &WeaponJson) {
    out.add(b"{\"queued\":true,\"via\":\"MakeWeaponSelection\",\"weapon\":");
    weapon_json(w, out);
    out.add(b"}");
}
