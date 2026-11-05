//! The game group: the C bridge's game-state and engine ops (`console.exec`, `test.*`,
//! `game.*`); `game.saveRestore` answers from [`save_restore`], the hooks lane's
//! ISource2ServerConfig vptr swap. The bodies are the C's `op_test`
//! (`mcp_bridge.c:5042-5098`) and `op_game_*` (`mcp_bridge_game.inc`), ported
//! 1:1: same checks in the same order, same error codes/names/`reason` strings, same JSON
//! member order and number formatting.
//!
//! Two dispatch arms serve the group (`mcp_bridge.c:5996-6024`): the test/console/tick arm
//! over a 512-byte result buffer and the game-state arm over a 98304-byte one (radar: up to 64
//! players + bomb + hostages; entities: up to 256 x ~250 B), whose overflow answer is
//! [`GAME_OVERFLOW`]. The group's shared state lives in [`schema::GS`] (the C's `gs_*`
//! statics) and the input-hook counters below; all of it is touched on the main thread only,
//! where the tick's drain runs every body.

pub(crate) mod builders;
pub(crate) mod save_restore;
pub(crate) mod schema;

#[cfg(test)]
mod tests;

use core::ffi::c_void;
use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

use s2sdk_sys::generated as sys;

use crate::entity_json::{json_find, json_int, json_string, Sb};
use crate::generated::pins;
use crate::ops::ident::{mem_ok, mod_iface, obj_is};
use crate::ops::{finish, Handler, OpErr};
use crate::queue::Request;

/// The test/console/tick arm's result buffer (`mcp_bridge.c:5998`, `static char rbuf[512]`).
pub(crate) const TEST_BUF: usize = 512;
/// The game-state arm's result buffer (`mcp_bridge.c:6012`, `static char rbuf[98304]`).
pub(crate) const GAME_BUF: usize = 98304;
/// The game-state arm's overflow answer (`mcp_bridge.c:6018`).
pub(crate) const GAME_OVERFLOW: &str = "InternalError: game state result too large";

/// The C's input-hook statics this group reads (`mcp_bridge.c:4769-4771`): the frame
/// counter `g_inFrames` (game.tick's `frames`), the view-angle override state `g_inAnglesOn` /
/// `g_inAngles` (game.local's `inputOverride`). The input hook that writes them belongs to the
/// input lane; until it is ported they read 0/false, which is exactly what the C reads while
/// its hook is not installed. Main thread only (the input hook and the drain both are).
pub static IN_FRAMES: AtomicI64 = AtomicI64::new(0);
/// `g_inAnglesOn` (`mcp_bridge.c:4769`): the override view angles are set.
pub static IN_ANGLES_ON: AtomicBool = AtomicBool::new(false);
/// `g_inAngles[3]` (`mcp_bridge.c:4770`): the override angles as `f32` bit patterns.
pub static IN_ANGLES: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

/// The group's handler table: every name is a row of the frozen C table
/// (`registry::METHODS`); the `ops` self-test holds the list to it.
pub static HANDLERS: &[(&str, Handler)] = &[
    ("test.checkpoint", test_checkpoint),
    ("test.active", test_active),
    ("console.exec", console_exec),
    ("game.tick", game_tick),
    ("game.saveRestore", save_restore::save_restore),
    ("game.local", game_local),
    ("game.weapons", game_weapons),
    ("game.radar", game_radar),
    ("game.selectWeapon", game_select_weapon),
    ("game.schemaProbe", game_schema_probe),
    ("game.rules", game_rules),
    ("game.entities", game_entities),
];

/// The C's `cmdline_has` (`mcp_bridge.c:4988-4996`) against this process's command line.
/// The C reads `GetCommandLineA()` (one NUL-terminated, space-separated string); the same
/// matcher runs over the same string on Windows, and over `/proc/self/cmdline` (NUL-separated
/// argv) on Linux, where the scan continues past NULs.
#[cfg(windows)]
pub(crate) fn cmdline_has(flag: &str) -> bool {
    use core::ffi::CStr;
    // mcp_bridge.c:4989: the C's data source.
    let cmdline = unsafe { windows_sys::Win32::System::Environment::GetCommandLineA() };
    if cmdline.is_null() {
        return false;
    }
    // SAFETY: GetCommandLineA answers a NUL-terminated, process-static string that lives as
    // long as the process (the C scans it the same way, `mcp_bridge.c:4989-4996`).
    cmdline_has_in(unsafe { CStr::from_ptr(cmdline.cast::<core::ffi::c_char>()) }.to_bytes_with_nul(), flag.as_bytes())
}

/// [`cmdline_has`] on Linux: `/proc/self/cmdline` is the argv, NUL-separated.
#[cfg(unix)]
pub(crate) fn cmdline_has(flag: &str) -> bool {
    let Ok(cmdline) = std::fs::read("/proc/self/cmdline") else {
        return false;
    };
    cmdline_has_in(&cmdline, flag.as_bytes())
}

/// [`cmdline_has`]'s matcher, pure over a command-line buffer (the brief's test): `flag` is a
/// whole token - it must match from some position to its end, and the next byte must be a
/// terminator (NUL, space or quote; end-of-buffer is the C's trailing NUL).
fn cmdline_has_in(cmdline: &[u8], flag: &[u8]) -> bool {
    // Never called with an empty flag; the C has no guard and would match one at a
    // terminator byte (mcp_bridge.c:4994), which is not a behavior worth keeping.
    if flag.is_empty() {
        return false;
    }
    for c in 0..=cmdline.len() {
        let mut i = 0;
        while i < flag.len() && cmdline.get(c + i) == flag.get(i) {
            i += 1;
        }
        if i == flag.len() {
            match cmdline.get(c + i) {
                None | Some(0) | Some(b' ') | Some(b'"') => return true,
                _ => {}
            }
        }
    }
    false
}

/// `jws` (`mcp_bridge.c:3162`): skip spaces and newlines/tabs.
fn jws(s: &[u8], mut p: usize) -> usize {
    while matches!(s.get(p), Some(b' ' | b'\n' | b'\t' | b'\r')) {
        p += 1;
    }
    p
}

/// `jstr` (`mcp_bridge.c:3164-3178`): the JSON string at `p` (must start with `"`) copied to
/// `out` (at most `out.len()` - 1 bytes), no escapes beyond `\"`, `\\`, `\/`. Answers the
/// position after the closing quote, or `None`.
fn jstr(s: &[u8], mut p: usize, out: &mut [u8]) -> Option<usize> {
    p = jws(s, p);
    if s.get(p) != Some(&b'"') {
        return None;
    }
    p += 1;
    let mut n = 0;
    loop {
        match s.get(p) {
            None | Some(0) | Some(b'"') => break,
            _ => {}
        }
        let mut c = s[p];
        p += 1;
        if c == b'\\' {
            // mcp_bridge.c:3170-3171: c = *p++ (a NUL past the backslash fails below).
            c = *s.get(p)?;
            p += 1;
            if !matches!(c, b'"' | b'\\' | b'/') {
                return None;
            }
        }
        if n + 1 >= out.len() {
            return None;
        }
        out[n] = c;
        n += 1;
    }
    if s.get(p) != Some(&b'"') {
        return None;
    }
    out[n] = 0;
    Some(p + 1)
}

/// The bytes of a NUL-terminated buffer, up to the NUL.
pub(crate) fn cstr_tail(buf: &[u8]) -> &[u8] {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..len]
}

/// A copied name as an owned byte vector (the builders take plain slices; the vector lives to
/// the end of the call).
fn owned(name: &schema::GsName) -> Vec<u8> {
    name.as_bytes().to_vec()
}

/// `game.tick` (the `game.tick` arm of `op_test`, `mcp_bridge.c:5043-5054`): the engine
/// client's last server tick and the input hook's frame count.
fn game_tick(r: &Request) -> String {
    let mut buf = vec![0u8; TEST_BUF]; // mcp_bridge.c:5998
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_tick(&mut sb);
    // The C's 512-byte arm has no overflow check (`mcp_bridge.c:5996-6008`; the game-state
    // arm's is `:6017-6018`); `finish`'s check cannot fire for these replies (< 40 bytes).
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_test`'s `game.tick` arm (`mcp_bridge.c:5043-5054`).
fn op_game_tick(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ec = schema::gs_engine_client()?;
    let iface = ec.cast::<sys::IVEngineClient2>();
    // SAFETY: `gs_engine_client` proved the object is the pinned CEngineClient (`obj_is`),
    // so its typed vtable's verified slot entry is the C's S2_SLOT_ call
    // (`mcp_bridge.c:5049`).
    let get_tick = unsafe {
        (*(*iface).vtable)
            .GetLastServerTick
            .expect("IVEngineClient2::GetLastServerTick (pinned slot 37)")
    };
    // SAFETY: the same verified entry, called with the object it was read from.
    let tick = unsafe { get_tick(iface) };
    builders::tick_result(out, tick, IN_FRAMES.load(Ordering::Relaxed) as i32); // (int)g_inFrames
    Ok(())
}

/// `console.exec` (the arm at `mcp_bridge.c:5062-5080`): queue one console command on the
/// engine's input service, only in `-testscript` or `-tools` sessions.
fn console_exec(r: &Request) -> String {
    let mut buf = vec![0u8; TEST_BUF]; // mcp_bridge.c:5998
    let mut sb = Sb::new(&mut buf);
    let outcome = op_console_exec(r.params.as_deref().map(str::as_bytes), &mut sb);
    // No overflow answer in the C's arm; unreachable for this result - see game_tick.
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// The pure body of `console.exec`, parameterised on the session gate so a host test can drive
/// the gate and the param validation (`mcp_bridge.c:5062-5080`; the `-tools` half of the gate is
/// policy added after the C: a tools session is where a play-test console line is wanted).
fn op_console_exec_in(allowed: bool, params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    if !allowed {
        return Err(OpErr::new(
            -32011,
            "NotImplemented",
            "console.exec is only enabled in -testscript or -tools sessions",
        ));
    }
    // mcp_bridge.c:5067-5071: static char cmd[1024].
    let mut cmd = [0u8; 1024];
    let mut have_cmd = false;
    if let Some(p) = params {
        have_cmd = json_string(p, b"command", &mut cmd) && cmd[0] != 0;
    }
    if !have_cmd {
        return Err(OpErr::new(-32602, "InvalidParams", "command (string) required"));
    }
    // mcp_bridge.c:5072-5076
    let is = mod_iface(pins::PIN_ENGINE2, sys::INPUTSERVICE_INTERFACE_VERSION);
    if !obj_is(is, pins::PIN_ENGINE2, pins::EV_INPUTSERVICE_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "InputService_001 is not the pinned CInputService",
        ));
    }
    let iface = is.cast::<sys::IInputService>();
    // SAFETY: `obj_is` proved the object is the pinned CInputService; the typed vtable's
    // AddCommandText is the C's S2_SLOT_IInputService_AddCommandText (MSVC slot 25, the
    // same signature), the C's call at `mcp_bridge.c:5077`.
    let add_command = unsafe {
        (*(*iface).vtable)
            .AddCommandText
            .expect("IInputService::AddCommandText (pinned slot 25)")
    };
    // SAFETY: the verified slot entry; arguments (5, cmd, 0) as the C passes (`:5077`).
    unsafe { add_command(iface, 5, cmd.as_ptr().cast::<core::ffi::c_char>(), 0) };
    builders::queued_result(out);
    Ok(())
}

/// The session gate of `console.exec` over a command-line flag matcher: `-testscript` or `-tools`.
fn console_exec_allowed(has_flag: impl Fn(&str) -> bool) -> bool {
    has_flag("-testscript") || has_flag("-tools")
}

/// [`op_console_exec_in`] against this process's command line.
fn op_console_exec(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    op_console_exec_in(console_exec_allowed(cmdline_has), params, out)
}

/// `test.checkpoint` (`mcp_bridge.c:5081-5097`): mark a checkpoint on the tier0 test-script
/// manager, then report whether a test script is running.
fn test_checkpoint(r: &Request) -> String {
    let mut buf = vec![0u8; TEST_BUF]; // mcp_bridge.c:5998
    let mut sb = Sb::new(&mut buf);
    let outcome = op_test_checkpoint(r.params.as_deref().map(str::as_bytes), &mut sb);
    // No overflow answer in the C's arm; unreachable for this result - see game_tick.
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `test.active` (`mcp_bridge.c:5081-5097`, no checkpoint).
fn test_active(r: &Request) -> String {
    let mut buf = vec![0u8; TEST_BUF]; // mcp_bridge.c:5998
    let mut sb = Sb::new(&mut buf);
    let outcome = op_test_active(&mut sb);
    // No overflow answer in the C's arm; unreachable for this result - see game_tick.
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_test`'s shared head (`mcp_bridge.c:5082-5086`): the tier0 test-script manager,
/// identity-checked once for both ops.
fn test_script_mgr() -> Result<*mut sys::ITestScriptMgr, OpErr> {
    let ts = mod_iface(pins::PIN_TIER0, sys::TESTSCRIPTMANAGER_INTERFACE_VERSION);
    if !obj_is(ts, pins::PIN_TIER0, pins::T0V_TESTSCRIPTMGR_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "TestScriptMgr001 is not the pinned CTestScriptMgr",
        ));
    }
    Ok(ts.cast::<sys::ITestScriptMgr>())
}

/// `IsRunningTestScript` on an [`obj_is`]-verified manager (`mcp_bridge.c:5087`).
fn test_script_is_running(ts: *mut sys::ITestScriptMgr) -> bool {
    // SAFETY: `test_script_mgr`'s `obj_is` proved the object is the pinned CTestScriptMgr;
    // the typed vtable's IsRunningTestScript is the C's S2_SLOT_ITestScriptMgr_IsRunningTestScript
    // (slot 13), called at `mcp_bridge.c:5087`.
    let is_running_fn = unsafe {
        (*(*ts).vtable)
            .IsRunningTestScript
            .expect("ITestScriptMgr::IsRunningTestScript (pinned slot 13)")
    };
    // SAFETY: the same verified entry, called with the object it was read from.
    unsafe { is_running_fn(ts) }
}

/// `op_test`'s shared head + `test.active` tail (`mcp_bridge.c:5082-5087`, `:5096`).
fn op_test_active(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let active = test_script_is_running(test_script_mgr()?);
    builders::active_result(out, active);
    Ok(())
}

/// `op_test`'s `test.checkpoint` arm's validation + tail (`mcp_bridge.c:5088-5095`),
/// parameterised on the `active` value (the C reads it before validating the name) and the
/// checkpoint call, so a host test can drive both.
fn op_test_checkpoint_in(
    active: bool,
    params: Option<&[u8]>,
    checkpoint: impl FnOnce(&[u8]),
    out: &mut Sb<'_>,
) -> Result<(), OpErr> {
    // mcp_bridge.c:5089: static char name[128].
    let mut name = [0u8; 128];
    let mut have_name = false;
    if let Some(p) = params {
        have_name = json_string(p, b"name", &mut name) && name[0] != 0;
    }
    if !have_name {
        return Err(OpErr::new(-32602, "InvalidParams", "name (string) required"));
    }
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    checkpoint(&name[..len]);
    builders::active_result(out, active);
    Ok(())
}

/// [`op_test_checkpoint_in`] against the live manager: the C's order - resolve and
/// identity-check the manager once (`test_script_mgr`, `mcp_bridge.c:5082-5086`), read
/// `active` (`:5087`), then validate the name, then `ITestScriptMgr::CheckPoint` (MSVC
/// slot 12, the C's call at `:5094`).
fn op_test_checkpoint(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ts = test_script_mgr()?;
    let active = test_script_is_running(ts);
    op_test_checkpoint_in(
        active,
        params,
        |name| {
            // SAFETY: the same `obj_is`-verified manager `test_script_mgr` returned (tier0's
            // interface object is process-static); CheckPoint is the C's S2_SLOT_ call
            // (`mcp_bridge.c:5094`).
            let checkpoint = unsafe {
                (*(*ts).vtable)
                    .CheckPoint
                    .expect("ITestScriptMgr::CheckPoint (pinned slot 12)")
            };
            let mut name_buf = name.to_vec();
            name_buf.push(0);
            // SAFETY: the verified slot entry; the name as the C passes it (`:5094`).
            unsafe { checkpoint(ts, name_buf.as_ptr().cast::<core::ffi::c_char>()) };
        },
        out,
    )
}

/// `game.local` (`op_game_local`, `mcp_bridge_game.inc:487-516`).
fn game_local(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_local(&mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_game_local` (`mcp_bridge_game.inc:487-516`).
fn op_game_local(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    let mut ps = -1;
    let ctl = match schema::gs_local_controller(ec, Some(&mut ps)) {
        Ok(ctl) => ctl,
        Err(why) => return Err(OpErr::new(-32024, "NoLocalPlayer", why)),
    };
    schema::gs_session_header(ec, out); // mcp_bridge_game.inc:495-497
    out.add(b",");
    builders::add_kv_i(out, b"playerSlot", i64::from(ps));
    out.add(b",");
    // mcp_bridge_game.inc:498-511: the pawn's eye angles plus the input hook's override state.
    let mut hp = sys::INVALID_EHANDLE_INDEX;
    let mut pawn: *mut c_void = core::ptr::null_mut();
    if schema::gs_u32(&mut gs, ctl, "CCSPlayerController", "m_hPlayerPawn", &mut hp) {
        pawn = schema::gs_ent_by_handle(&mut gs, hp);
    }
    {
        let mut va = [0f32; 3];
        let have_va = !pawn.is_null()
            && schema::gs_vec(&mut gs, pawn, "C_CSPlayerPawn", "m_angEyeAngles", &mut va);
        builders::add_kv_v3(out, b"viewAngles", if have_va { Some(&va) } else { None });
        out.add(b",");
        out.add(b"\"inputOverride\":");
        if IN_ANGLES_ON.load(Ordering::Relaxed) {
            let a = [
                f32::from_bits(IN_ANGLES[0].load(Ordering::Relaxed)),
                f32::from_bits(IN_ANGLES[1].load(Ordering::Relaxed)),
                f32::from_bits(IN_ANGLES[2].load(Ordering::Relaxed)),
            ];
            builders::addf3(out, &a);
        } else {
            out.add(b"null");
        }
        out.add(b",");
    }
    out.add(b"\"player\":");
    let player = read_player(&mut gs, ctl, pawn, true);
    builders::player_json(&player, out);
    out.add(b"}");
    Ok(())
}

/// The reads of `gs_player_json` (`mcp_bridge_game.inc:409-445`), handed to the pure builder.
fn read_player(
    gs: &mut schema::Gs,
    ctl: *const c_void,
    local_pawn: *const c_void,
    with_details: bool,
) -> builders::PlayerJson {
    // mcp_bridge_game.inc:410-416
    let mut hp = sys::INVALID_EHANDLE_INDEX;
    let mut pawn: *mut c_void = core::ptr::null_mut();
    if schema::gs_u32(gs, ctl, "CCSPlayerController", "m_hPlayerPawn", &mut hp) {
        pawn = schema::gs_ent_by_handle(gs, hp);
    }
    let mut team = 0u8;
    let mut alive = 0u8;
    let mut health = 0i32;
    let _ = schema::gs_u8(gs, ctl, "C_BaseEntity", "m_iTeamNum", &mut team);
    let _ = schema::gs_u8(gs, ctl, "CCSPlayerController", "m_bPawnIsAlive", &mut alive);
    let _ = schema::gs_i32(gs, ctl, "CCSPlayerController", "m_iPawnHealth", &mut health);
    let mut origin = [0f32; 3];
    let mut eye = [0f32; 3];
    let mut ang = [0f32; 3];
    let mut vel = [0f32; 3];
    let mut vo = [0f32; 3];
    let (mut have_origin, mut have_ang, mut have_vel, mut have_eye) = (false, false, false, false);
    let mut spotted = 0u8;
    let mut spotted_mask = [0u8; 8];
    if !pawn.is_null() {
        have_origin = schema::gs_origin(gs, pawn, &mut origin);
        have_ang = schema::gs_vec(gs, pawn, "C_CSPlayerPawn", "m_angEyeAngles", &mut ang);
        have_vel = schema::gs_vec(gs, pawn, "C_BaseEntity", "m_vecAbsVelocity", &mut vel);
        if have_origin && schema::gs_vec(gs, pawn, "C_BaseModelEntity", "m_vecViewOffset", &mut vo) {
            eye = [origin[0] + vo[0], origin[1] + vo[1], origin[2] + vo[2]];
            have_eye = true;
        }
        // mcp_bridge_game.inc:427-432: the spotted state is a sub-object at a schema offset.
        let mut so = 0;
        if schema::gs_off(gs, "C_CSPlayerPawn", "m_entitySpottedState", &mut so) {
            // The pawn's readability is the caller's identity check; `so` is a
            // schema-resolved offset inside it (the C's `(u8*)pawn + so`, `:429`) - wrapping,
            // so a negative offset behaves as the C's; the reads below probe it.
            let st = pawn.cast::<u8>().wrapping_byte_offset(so as isize).cast::<c_void>();
            let _ = schema::gs_u8(gs, st, "EntitySpottedState_t", "m_bSpotted", &mut spotted);
            let _ = schema::gs_rd(gs, st, "EntitySpottedState_t", "m_bSpottedByMask", &mut spotted_mask);
        }
    }
    let name = schema::gs_inline_cstr(gs, ctl, "CBasePlayerController", "m_iszPlayerName");
    let mut details = None;
    if with_details {
        // mcp_bridge_game.inc:446-467
        let mut life = 0u8;
        let mut flags: u32 = 0;
        let mut scoped = 0u8;
        let _ = schema::gs_u8(gs, pawn, "C_BaseEntity", "m_lifeState", &mut life);
        let _ = schema::gs_u32(gs, pawn, "C_BaseEntity", "m_fFlags", &mut flags);
        let _ = schema::gs_u8(gs, pawn, "C_CSPlayerPawn", "m_bIsScoped", &mut scoped);
        let mut money: *mut c_void = core::ptr::null_mut();
        let mut move_svc: *mut c_void = core::ptr::null_mut();
        let _ = schema::gs_ptr(gs, ctl, "CCSPlayerController", "m_pInGameMoneyServices", &mut money);
        if !pawn.is_null() {
            let _ = schema::gs_ptr(gs, pawn, "C_BasePlayerPawn", "m_pMovementServices", &mut move_svc);
        }
        let mut armor = 0i32;
        let armor = schema::gs_i32(gs, pawn, "C_CSPlayerPawn", "m_ArmorValue", &mut armor)
            .then_some(armor);
        let mut helmet = 0u8;
        let helmet = schema::gs_u8(gs, ctl, "CCSPlayerController", "m_bPawnHasHelmet", &mut helmet)
            .then_some(helmet != 0);
        let mut account = 0i32;
        let money = schema::gs_i32(
            gs,
            money,
            "CCSPlayerController_InGameMoneyServices",
            "m_iAccount",
            &mut account,
        )
        .then_some(account);
        let mut ducked = 0u8;
        let ducked = schema::gs_u8(gs, move_svc, "CCSPlayer_MovementServices", "m_bDucked", &mut ducked)
            .then_some(ducked != 0);
        let mut move_type = 0u8;
        let move_type =
            schema::gs_u8(gs, pawn, "C_BaseEntity", "m_MoveType", &mut move_type).then_some(move_type);
        // mcp_bridge_game.inc:468-470: the active weapon, read when there is a pawn.
        let mut aw: *mut c_void = core::ptr::null_mut();
        if !pawn.is_null() {
            aw = schema::gs_active_weapon(gs, pawn);
        }
        let active_weapon = if !aw.is_null() { Some(schema::read_weapon(gs, aw)) } else { None };
        details = Some(builders::PlayerDetails {
            eye: if have_eye { Some(eye) } else { None },
            velocity: if have_vel { Some(vel) } else { None },
            life_state: life,
            flags,
            scoped: scoped != 0,
            armor,
            helmet,
            money,
            ducked,
            move_type,
            active_weapon,
        });
    }
    builders::PlayerJson {
        controller_index: schema::gs_ent_index(ctl),
        pawn_index: if !pawn.is_null() {
            schema::gs_ent_index(pawn)
        } else {
            -1
        },
        name: name.as_ref().map(owned),
        team,
        alive: alive != 0,
        health,
        is_local: !pawn.is_null() && pawn.cast_const() == local_pawn,
        origin: if have_origin { Some(origin) } else { None },
        eye_angles: if have_ang { Some(ang) } else { None },
        spotted: spotted != 0,
        spotted_mask: u32::from_le_bytes([
            spotted_mask[0],
            spotted_mask[1],
            spotted_mask[2],
            spotted_mask[3],
        ]),
        details,
    }
}

/// `game.weapons` (`op_game_weapons`, `mcp_bridge_game.inc:519-555`).
fn game_weapons(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_weapons(&mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_game_weapons` (`mcp_bridge_game.inc:519-555`).
fn op_game_weapons(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    let ctl = match schema::gs_local_controller(ec, None) {
        Ok(ctl) => ctl,
        Err(why) => return Err(OpErr::new(-32024, "NoLocalPlayer", why)),
    };
    // mcp_bridge_game.inc:526-528
    let mut hp = sys::INVALID_EHANDLE_INDEX;
    let mut pawn: *mut c_void = core::ptr::null_mut();
    if schema::gs_u32(&mut gs, ctl, "CCSPlayerController", "m_hPlayerPawn", &mut hp) {
        pawn = schema::gs_ent_by_handle(&mut gs, hp);
    }
    if pawn.is_null() {
        return Err(OpErr::new(
            -32024,
            "NoLocalPlayer",
            "local controller has no pawn (dead/spectating?)",
        ));
    }
    let ws = schema::gs_weapon_services(&mut gs, pawn);
    if ws.is_null() {
        return Err(OpErr::new(-32024, "NoLocalPlayer", "pawn has no weapon services"));
    }
    let mut vo = 0;
    if !schema::gs_off(&mut gs, "CPlayer_WeaponServices", "m_hMyWeapons", &mut vo) {
        return Err(OpErr::new(
            -32023,
            "SchemaUnavailable",
            "CPlayer_WeaponServices::m_hMyWeapons",
        ));
    }
    // mcp_bridge_game.inc:531-537: CUtlVector<CEntityHandle> at ws + vo.
    // `ws` passed `mem_ok` in `gs_weapon_services` and `vo` is a schema-resolved
    // offset inside it; wrapping, the C's `(u8*)ws + vo` with any i32 offset; the C reads
    // the vector's members unguarded (`:536-537`).
    let vec = ws.cast::<u8>().wrapping_byte_offset(vo as isize)
        .cast::<sys::CUtlVector_CEntityHandle>();
    // SAFETY: as above; the generated struct's members, the C's `vec->m_Size` / `m_pMemory`.
    let (n, hs) = unsafe {
        (
            addr_of!((*vec).m_Size).read_unaligned(),
            addr_of!((*vec).m_pMemory).read_unaligned(),
        )
    };
    let n = n as usize;
    let valid = n > 0 && n < 256 && !hs.is_null() && mem_ok(hs.cast());
    let aw = schema::gs_active_weapon(&mut gs, pawn);
    schema::gs_session_header(ec, out); // mcp_bridge_game.inc:539
    out.add(b",\"active\":");
    if !aw.is_null() {
        schema::gs_weapon_json(&mut gs, aw, out);
    } else {
        out.add(b"null");
    }
    // mcp_bridge_game.inc:542: the count is the valid n, else 0.
    out.add(b",\"count\":");
    out.addi(if valid { n as i64 } else { 0 });
    out.add(b",\"weapons\":[");
    let mut wrote = 0;
    if valid {
        for i in 0..n {
            // SAFETY: `hs` passed `mem_ok` above; `i` is bounded by `n` (< 256), the C's
            // `hs[i]` (`mcp_bridge_game.inc:547`).
            let h = unsafe { *(hs.add(i).cast::<u32>()) };
            let w = schema::gs_ent_by_handle(&mut gs, h);
            if w.is_null() {
                continue;
            }
            if wrote > 0 {
                out.add(b",");
            }
            wrote += 1;
            schema::gs_weapon_json(&mut gs, w, out);
        }
    }
    out.add(b"]}");
    Ok(())
}

/// `game.radar` (`op_game_radar`, `mcp_bridge_game.inc:558-644`).
fn game_radar(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_radar(&mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_game_radar` (`mcp_bridge_game.inc:558-644`).
fn op_game_radar(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    // mcp_bridge_game.inc:562-565: the local pawn is best-effort here (it only decides the
    // players' isLocal flag).
    let ctl = schema::gs_local_controller(ec, None).unwrap_or(core::ptr::null_mut());
    let mut local_pawn: *mut c_void = core::ptr::null_mut();
    if !ctl.is_null() {
        let mut hp = sys::INVALID_EHANDLE_INDEX;
        if schema::gs_u32(&mut gs, ctl, "CCSPlayerController", "m_hPlayerPawn", &mut hp) {
            local_pawn = schema::gs_ent_by_handle(&mut gs, hp);
        }
    }
    schema::gs_session_header(ec, out); // mcp_bridge_game.inc:566
    out.add(b",\"players\":[");
    let mut wrote = 0;
    // mcp_bridge_game.inc:569: controllers occupy entindex 1..64 (player slot + 1).
    for i in 1..=64u32 {
        let c = schema::gs_ent_by_index(&mut gs, i);
        if c.is_null() {
            continue;
        }
        let Some(dn) = schema::gs_designer_name(&mut gs, i) else {
            continue;
        };
        if !schema::name_is(&dn, "cs_player_controller") {
            continue;
        }
        if wrote > 0 {
            out.add(b",");
        }
        wrote += 1;
        let player = read_player(&mut gs, c, local_pawn, false);
        builders::player_json(&player, out);
    }
    out.add(b"],\"bomb\":");
    let mut bomb_written = 0;
    out.add(b"[");
    let mut hostages = 0;
    let mut host_buf = vec![0u8; 2048]; // mcp_bridge_game.inc:581, static char hostBuf[2048]
    let mut hb = Sb::new(&mut host_buf);
    // mcp_bridge_game.inc:583: world entities; chunk-null skips 512 at a time.
    let mut i: u32 = 65;
    while i < sys::MAX_EDICTS {
        let id = schema::gs_ident(&mut gs, i);
        if id.is_null() {
            if i & schema::IDENT_CHUNK_MASK == 0
                && schema::gs_ident(&mut gs, i | schema::IDENT_CHUNK_MASK).is_null()
            {
                i |= schema::IDENT_CHUNK_MASK;
            }
            i += 1;
            continue;
        }
        // mcp_bridge_game.inc:586: `void *ent = *(void **)id` - the identity's m_pInstance.
        // SAFETY: `gs_ident` returned a chunk slot whose first word the C reads unguarded
        // (`mcp_bridge_game.inc:586`); the null check below is the C's own.
        let ent = unsafe { *(id as *const *mut c_void) };
        if ent.is_null() {
            i += 1;
            continue;
        }
        let Some(dn) = schema::gs_designer_name(&mut gs, i) else {
            i += 1;
            continue;
        };
        if schema::name_is(&dn, "planted_c4") {
            if !mem_ok(ent) {
                i += 1;
                continue;
            }
            // mcp_bridge_game.inc:590-606
            let mut o = [0f32; 3];
            let ho = schema::gs_origin(&mut gs, ent, &mut o);
            let mut ticking = 0u8;
            let mut defused = 0u8;
            let mut site = -1;
            let mut blow = 0f32;
            let _ = schema::gs_u8(&mut gs, ent, "C_PlantedC4", "m_bBombTicking", &mut ticking);
            let _ = schema::gs_u8(&mut gs, ent, "C_PlantedC4", "m_bBombDefused", &mut defused);
            let _ = schema::gs_i32(&mut gs, ent, "C_PlantedC4", "m_nBombSite", &mut site);
            let _ = schema::gs_f32(&mut gs, ent, "C_PlantedC4", "m_flC4Blow", &mut blow);
            if bomb_written > 0 {
                out.add(b",");
            }
            bomb_written += 1;
            builders::bomb_planted_json(
                out,
                i,
                if ho { Some(&o) } else { None },
                ticking != 0,
                defused != 0,
                site,
                blow,
            );
        } else if schema::name_is(&dn, "weapon_c4") {
            if !mem_ok(ent) {
                i += 1;
                continue;
            }
            // mcp_bridge_game.inc:607-617
            let mut o = [0f32; 3];
            let ho = schema::gs_origin(&mut gs, ent, &mut o);
            let mut owner = sys::INVALID_EHANDLE_INDEX;
            let _ = schema::gs_u32(&mut gs, ent, "C_BaseEntity", "m_hOwnerEntity", &mut owner);
            let own = schema::gs_ent_by_handle(&mut gs, owner);
            if bomb_written > 0 {
                out.add(b",");
            }
            bomb_written += 1;
            builders::bomb_carried_json(
                out,
                !own.is_null(),
                i,
                if !own.is_null() { schema::gs_ent_index(own) } else { -1 },
                if ho { Some(&o) } else { None },
            );
        } else if schema::name_is(&dn, "hostage_entity") {
            if !mem_ok(ent) {
                i += 1;
                continue;
            }
            // mcp_bridge_game.inc:618-635
            let mut o = [0f32; 3];
            let ho = schema::gs_origin(&mut gs, ent, &mut o);
            let mut rescued = 0u8;
            let mut state = -1;
            let mut leader = sys::INVALID_EHANDLE_INDEX;
            let mut health = 0i32;
            let _ = schema::gs_u8(&mut gs, ent, "C_Hostage", "m_isRescued", &mut rescued);
            let _ = schema::gs_i32(&mut gs, ent, "C_Hostage", "m_nHostageState", &mut state);
            let _ = schema::gs_u32(&mut gs, ent, "C_Hostage", "m_leader", &mut leader);
            let _ = schema::gs_i32(&mut gs, ent, "C_BaseEntity", "m_iHealth", &mut health);
            let ld = schema::gs_ent_by_handle(&mut gs, leader);
            if hostages > 0 {
                hb.add(b",");
            }
            hostages += 1;
            builders::hostage_json(
                &mut hb,
                i,
                if ho { Some(&o) } else { None },
                state,
                rescued != 0,
                health,
                if !ld.is_null() { schema::gs_ent_index(ld) } else { -1 },
            );
        }
        i += 1;
    }
    out.add(b"],\"hostages\":[");
    if !hb.ovf() {
        out.add(hb.as_cstr()); // on overflow the scratch is cut mid-object: emit none, flag it
    }
    out.add(b"]");
    if hb.ovf() {
        out.add(b",\"hostagesTruncated\":true");
    }
    out.add(b"}");
    Ok(())
}

/// `game.rules` (`op_game_rules`, `mcp_bridge_game.inc:671-703`).
fn game_rules(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_rules(&mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `op_game_rules` (`mcp_bridge_game.inc:671-703`).
fn op_game_rules(out: &mut Sb<'_>) -> Result<(), OpErr> {
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    let mut off = 0;
    if !schema::gs_off(&mut gs, "C_CSGameRulesProxy", "m_pGameRules", &mut off) {
        return Err(OpErr::new(
            -32023,
            "SchemaUnavailable",
            "C_CSGameRulesProxy::m_pGameRules",
        ));
    }
    let mut idx = -1;
    let proxy = schema::gs_gamerules_ent(&mut gs, &mut idx);
    let mut rules: *mut c_void = core::ptr::null_mut();
    if proxy.is_null()
        || !schema::gs_ptr(&mut gs, proxy, "C_CSGameRulesProxy", "m_pGameRules", &mut rules)
        || rules.is_null()
        || !mem_ok(rules)
    {
        return Err(OpErr::new(
            -32025,
            "NoGameRules",
            "no cs_gamerules proxy with a C_CSGameRules object (not in a game?)",
        ));
    }
    // mcp_bridge_game.inc:684-685: the miss builder over the C's 1024-byte scratch.
    let mut miss_buf = vec![0u8; 1024];
    let mut mb = Sb::new(&mut miss_buf);
    schema::gs_session_header(ec, out); // mcp_bridge_game.inc:686
    // The thirteen gs_fld_* rows (`mcp_bridge_game.inc:688-700`): read, else null + miss.
    let mut v8 = 0u8;
    let mut vi = 0i32;
    let mut vf = 0f32;
    let warmup = schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bWarmupPeriod", &mut v8).then_some(v8 != 0);
    if warmup.is_none() {
        builders::miss_add(&mut mb, b"m_bWarmupPeriod");
    }
    let freeze = schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bFreezePeriod", &mut v8).then_some(v8 != 0);
    if freeze.is_none() {
        builders::miss_add(&mut mb, b"m_bFreezePeriod");
    }
    let team_intro =
        schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bTeamIntroPeriod", &mut v8).then_some(v8 != 0);
    if team_intro.is_none() {
        builders::miss_add(&mut mb, b"m_bTeamIntroPeriod");
    }
    let game_restart =
        schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bGameRestart", &mut v8).then_some(v8 != 0);
    if game_restart.is_none() {
        builders::miss_add(&mut mb, b"m_bGameRestart");
    }
    let has_match_started =
        schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bHasMatchStarted", &mut v8).then_some(v8 != 0);
    if has_match_started.is_none() {
        builders::miss_add(&mut mb, b"m_bHasMatchStarted");
    }
    let game_phase = schema::gs_i32(&mut gs, rules, "C_CSGameRules", "m_gamePhase", &mut vi).then_some(vi);
    if game_phase.is_none() {
        builders::miss_add(&mut mb, b"m_gamePhase");
    }
    let rounds_played =
        schema::gs_i32(&mut gs, rules, "C_CSGameRules", "m_totalRoundsPlayed", &mut vi).then_some(vi);
    if rounds_played.is_none() {
        builders::miss_add(&mut mb, b"m_totalRoundsPlayed");
    }
    let round_start_time =
        schema::gs_f32(&mut gs, rules, "C_CSGameRules", "m_fRoundStartTime", &mut vf).then_some(vf);
    if round_start_time.is_none() {
        builders::miss_add(&mut mb, b"m_fRoundStartTime");
    }
    let restart_round_time =
        schema::gs_f32(&mut gs, rules, "C_CSGameRules", "m_flRestartRoundTime", &mut vf).then_some(vf);
    if restart_round_time.is_none() {
        builders::miss_add(&mut mb, b"m_flRestartRoundTime");
    }
    let round_time_sec = schema::gs_i32(&mut gs, rules, "C_CSGameRules", "m_iRoundTime", &mut vi).then_some(vi);
    if round_time_sec.is_none() {
        builders::miss_add(&mut mb, b"m_iRoundTime");
    }
    let round_win_status =
        schema::gs_i32(&mut gs, rules, "C_CSGameRules", "m_iRoundWinStatus", &mut vi).then_some(vi);
    if round_win_status.is_none() {
        builders::miss_add(&mut mb, b"m_iRoundWinStatus");
    }
    let round_end_reason =
        schema::gs_i32(&mut gs, rules, "C_CSGameRules", "m_eRoundEndReason", &mut vi).then_some(vi);
    if round_end_reason.is_none() {
        builders::miss_add(&mut mb, b"m_eRoundEndReason");
    }
    let bomb_planted =
        schema::gs_u8(&mut gs, rules, "C_CSGameRules", "m_bBombPlanted", &mut v8).then_some(v8 != 0);
    if bomb_planted.is_none() {
        builders::miss_add(&mut mb, b"m_bBombPlanted");
    }
    let tail = builders::RulesJson {
        proxy_index: idx,
        warmup,
        freeze,
        team_intro,
        game_restart,
        has_match_started,
        game_phase,
        rounds_played,
        round_start_time,
        restart_round_time,
        round_time_sec,
        round_win_status,
        round_end_reason,
        bomb_planted,
        schema_missing: mb.as_cstr(),
    };
    builders::rules_tail(out, &tail);
    Ok(())
}

/// `game.entities` (`op_game_entities`, `mcp_bridge_game.inc:712-770`).
fn game_entities(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_entities(r.params.as_deref().map(str::as_bytes), &mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// `GS_ENT_MAX` (`mcp_bridge_game.inc:711`).
const GS_ENT_MAX: i64 = 256;

/// The pure param validation of `op_game_entities` (`mcp_bridge_game.inc:723-729`): the max
/// bound, then the single-index bound, in the C's order, before any game access.
fn entities_params_validate(max: i64, single: bool, only: i64) -> Result<(), OpErr> {
    if !(1..=GS_ENT_MAX).contains(&max) {
        return Err(OpErr::new(-32602, "InvalidParams", "max must be 1..256"));
    }
    if single && !(1..=i64::from(sys::MAX_EDICTS - 1)).contains(&only) {
        // mcp_bridge_game.inc:724-729: the bound spelled from the const, in the C's 40 bytes.
        let mut bnd = [0u8; 40];
        let mut eb = Sb::new(&mut bnd);
        eb.add(b"entindex must be 1..");
        eb.addnum(u64::from(sys::MAX_EDICTS - 1));
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            &String::from_utf8_lossy(eb.as_cstr()),
        ));
    }
    Ok(())
}

/// The game-state arm's trailer reserve (`mcp_bridge_game.inc:759`, the C's literal 64):
/// an entity commits only when 64 bytes stay free behind it for the `count`/`truncated` tail.
const GS_TRAILER_RESERVE: usize = 64;

/// The C's keep condition for one entity of `game.entities` (`mcp_bridge_game.inc:759`):
/// the entity (its leading comma included) commits only when it built without overflow and
/// leaves the trailer reserve free - `out->len + 64 > out->cap` rolls it back and answers
/// `truncated` (`:759-763`). Pure over the two buffers' lengths, so the host tests drive the
/// boundary.
fn entities_keep(scratch_len: usize, scratch_ovf: bool, out_len: usize) -> bool {
    !scratch_ovf && scratch_len + GS_TRAILER_RESERVE + out_len <= GAME_BUF
}

/// `op_game_entities` (`mcp_bridge_game.inc:712-770`).
fn op_game_entities(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    let mut cls = [0u8; 128];
    let mut nm = [0u8; 128];
    let mut max: i64 = 64;
    let mut only: i64 = 0;
    let mut single = false;
    if let Some(p) = params {
        let _ = json_string(p, b"class", &mut cls);
        let _ = json_string(p, b"name", &mut nm);
        if let Some(v) = json_int(p, b"max") {
            max = v;
        }
        if let Some(v) = json_int(p, b"entindex") {
            single = true;
            only = v;
        }
    }
    entities_params_validate(max, single, only)?;
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    schema::gs_session_header(ec, out); // mcp_bridge_game.inc:732
    out.add(b",\"entities\":[");
    let mut wrote: i64 = 0;
    let mut truncated = false;
    let (first, last): (u32, u32) = if single {
        (
            u32::try_from(only).expect("validated 1..16383"),
            u32::try_from(only).expect("validated") + 1,
        )
    } else {
        (1, sys::MAX_EDICTS)
    };
    // The C builds each entity straight into `out` and rolls the write back when the buffer
    // came within 64 bytes of its cap (`mcp_bridge_game.inc:746-763`); `Sb` has no rollback,
    // so the entity (with its comma) is built into a same-size scratch first and committed
    // only when the C's keep condition holds - the emitted bytes are the same.
    let mut scratch_buf = vec![0u8; GAME_BUF];
    let mut i = first;
    while i < last && !truncated {
        let id = schema::gs_ident(&mut gs, i);
        if id.is_null() {
            if i & schema::IDENT_CHUNK_MASK == 0
                && schema::gs_ident(&mut gs, i | schema::IDENT_CHUNK_MASK).is_null()
            {
                i |= schema::IDENT_CHUNK_MASK;
            }
            i += 1;
            continue;
        }
        let ent = schema::gs_ent_by_index(&mut gs, i);
        if ent.is_null() {
            i += 1;
            continue;
        }
        let dn = schema::gs_designer_name(&mut gs, i);
        let tn = schema::gs_entity_name(&mut gs, i);
        if cls[0] != 0 && dn.as_ref().is_none_or(|d| !schema::name_is_bytes(d, cstr_tail(&cls))) {
            i += 1;
            continue;
        }
        if nm[0] != 0 && tn.as_ref().is_none_or(|t| !schema::name_is_bytes(t, cstr_tail(&nm))) {
            i += 1;
            continue;
        }
        // mcp_bridge_game.inc:745: max counts matches.
        if wrote == max {
            truncated = true;
            break;
        }
        let mut scratch = Sb::new(&mut scratch_buf);
        if wrote > 0 {
            scratch.add(b",");
        }
        let mut o = [0f32; 3];
        let mut a = [0f32; 3];
        let ho = schema::gs_origin(&mut gs, ent, &mut o);
        let ha = schema::gs_angles(&mut gs, ent, &mut a);
        let mut health = 0i32;
        let health = schema::gs_i32(&mut gs, ent, "C_BaseEntity", "m_iHealth", &mut health).then_some(health);
        let mut toggle = 0i32;
        let toggle =
            schema::gs_i32(&mut gs, ent, "C_BaseToggle", "m_toggle_state", &mut toggle).then_some(toggle);
        builders::entity_json(
            &mut scratch,
            i,
            &builders::EntityJson {
                class: dn.as_ref().map(|d| d.as_bytes()),
                name: tn.as_ref().map(|t| t.as_bytes()),
                origin: if ho { Some(&o) } else { None },
                angles: if ha { Some(&a) } else { None },
                health,
                toggle_state: toggle,
            },
        );
        if entities_keep(scratch.as_cstr().len(), scratch.ovf(), out.as_cstr().len()) {
            out.add(scratch.as_cstr());
            wrote += 1;
            i += 1;
        } else {
            truncated = true;
            break;
        }
    }
    out.add(b"],\"count\":");
    out.addi(wrote);
    out.add(b",\"truncated\":");
    out.add(if truncated { b"true" } else { b"false" });
    out.add(b"}");
    Ok(())
}

/// `json_bool` (`mcp_bridge.c:2525-2531`): `true` / `false` at the member's value, `None`
/// when the member is absent or is neither literal. Used by `game.saveRestore` (`:5057`)
/// and `sound.drain` (`mcp_bridge_game.inc:1122`).
pub(crate) fn json_bool(json: &[u8], key: &[u8]) -> Option<bool> {
    let p = json_find(json, key)?;
    if json[p..].starts_with(b"true") {
        Some(true) // mcp_bridge.c:2528
    } else if json[p..].starts_with(b"false") {
        Some(false) // mcp_bridge.c:2529
    } else {
        None
    }
}

/// `game.selectWeapon` (`op_game_select_weapon`, `mcp_bridge_game.inc:834-889`).
fn game_select_weapon(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012 (the weapon JSON can be long)
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_select_weapon(r.params.as_deref().map(str::as_bytes), &mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// The C's `k_gsInvCommands` (`mcp_bridge_game.inc:782`).
const K_GS_INV_COMMANDS: [&str; 5] =
    ["invnext", "invprev", "invnextselect", "invprevselect", "lastinv"];

/// `gs_is_weapon_name` (`mcp_bridge_game.inc:783-789`): `weapon_[a-z0-9_]+`, over raw bytes.
fn gs_is_weapon_name(s: &[u8]) -> bool {
    let Some(rest) = s.strip_prefix(b"weapon_") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// `op_game_select_weapon` (`mcp_bridge_game.inc:834-889`).
fn op_game_select_weapon(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    let mut cmd = [0u8; 160]; // mcp_bridge_game.inc:835, static char cmd[160]
    let mut word = [0u8; 96]; // mcp_bridge_game.inc:836, static char word[96]
    let mut def: i64 = -1;
    let mut handle: i64 = -1;
    let mut by_weapon = false;
    let mut weapon_name: Option<Vec<u8>> = None;
    if let Some(p) = params {
        if let Some(slot) = json_int(p, b"slot") {
            if !(0..=13).contains(&slot) {
                return Err(OpErr::new(-32602, "InvalidParams", "slot must be 0..13"));
            }
            let mut c = Sb::new(&mut cmd);
            c.add(b"slot");
            c.addnum(slot as u64); // mcp_bridge_game.inc:842, sb_addnum(&c, (u64)slot)
        } else if json_string(p, b"command", &mut word) && word[0] != 0 {
            let command = cstr_tail(&word);
            if !K_GS_INV_COMMANDS.iter().any(|c| c.as_bytes() == command) {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "command must be invnext|invprev|invnextselect|invprevselect|lastinv",
                ));
            }
            let mut c = Sb::new(&mut cmd);
            c.add(command);
        } else if json_string(p, b"weapon", &mut word) && word[0] != 0 {
            let w = cstr_tail(&word);
            if !gs_is_weapon_name(w) {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "weapon must look like weapon_ak47",
                ));
            }
            by_weapon = true;
            weapon_name = Some(w.to_vec());
        } else if let Some(d) = json_int(p, b"defIndex") {
            if !(0..=65535).contains(&d) {
                return Err(OpErr::new(-32602, "InvalidParams", "defIndex must be 0..65535"));
            }
            by_weapon = true;
            def = d;
            weapon_name = Some(Vec::new()); // the C's word[0] = 0
        } else if let Some(h) = json_int(p, b"handle") {
            if !(0..=0xFFFF_FFFFi64).contains(&h) {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "handle must be a u32 entity handle",
                ));
            }
            by_weapon = true;
            handle = h;
            weapon_name = Some(Vec::new()); // the C's word[0] = 0
        } else {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "one of slot, command, weapon, defIndex or handle required",
            ));
        }
    } else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "one of slot, command, weapon, defIndex or handle required",
        ));
    }
    if by_weapon {
        // mcp_bridge_game.inc:862-865
        if !schema::mod_ok(pins::PIN_CLIENT) || !schema::mod_ok(pins::PIN_SCHEMA) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "weapon/defIndex/handle selection needs client.dll and schemasystem.dll pinned",
            ));
        }
        let base = schema::modules_base(pins::PIN_CLIENT);
        // mcp_bridge_game.inc:866-867: `void **in = *(void ***)(cb + CG_INPUT_PTR)`.
        // SAFETY: `base` is a verified module's load address, so the global is inside the
        // pinned image; the C reads it unguarded (`mcp_bridge_game.inc:867`).
        let input =
            unsafe { ((base + pins::CG_INPUT_PTR.value) as *const *mut c_void).read_unaligned() };
        // mcp_bridge_game.inc:868-872: slot 9 is the pinned original - with KHook the table's
        // entry is patched in place, so the identity check below reads it through
        // `FindOriginalVirtual` (`ops/ident.rs`) exactly as the pinned word. The C's second arm
        // (`in == g_inObj && in_installed()`, `mcp_bridge_game.inc:869`) compares against the
        // input lane's recorded object while the hook is installed.
        let input_hooked = !input.is_null()
            && std::ptr::eq(input, crate::ops::input::object())
            && crate::ops::input::installed();
        if !(obj_is(input, pins::PIN_CLIENT, pins::CV_CSGOINPUT_VT.value as u32) || input_hooked) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "g_pInput is not the pinned CCSGOInput (vtable)",
            ));
        }
        let name = weapon_name.unwrap_or_default();
        let w = gs_find_held_weapon(&name, def, handle)?;
        // mcp_bridge_game.inc:875: the call goes through the pinned ORIGINAL vtable
        // address (base + CV_CSGOINPUT_VT), not the object's (possibly swapped) vptr.
        // SAFETY: `mod_ok(PIN_CLIENT)` verified the image byte-for-byte, so `base +
        // CV_CSGOINPUT_VT` is the pinned CCSGOInput vftable inside it and its MakeWeaponSelection
        // field (MSVC slot 9) holds the pinned target (`pins.rs`, the PinSlot `slot: 9`);
        // the C calls the same address (`mcp_bridge_game.inc:875`).
        let vt = (base + pins::CV_CSGOINPUT_VT.value) as *const sys::CCSGOInput_vtable;
        let make_selection = unsafe {
            (*vt)
                .base
                .MakeWeaponSelection
                .expect("CClientInput::MakeWeaponSelection (pinned slot 9)")
        };
        // SAFETY: the same verified entry; `input` passed `obj_is` above and `w` was proven a
        // live entity by `gs_ent_by_handle`, the C's arguments (`mcp_bridge_game.inc:875`).
        unsafe {
            make_selection(
                input.cast::<sys::CClientInput>(),
                sys::CSplitScreenSlot { m_Data: 0 },
                w.cast::<sys::CEntityInstance>(),
            )
        };
        let wstruct = {
            let mut gs = schema::lock();
            schema::read_weapon(&mut gs, w)
        };
        builders::select_weapon_result(out, &wstruct);
        return Ok(());
    }
    // mcp_bridge_game.inc:881-887: the command path, on the engine's input service.
    let is = mod_iface(pins::PIN_ENGINE2, sys::INPUTSERVICE_INTERFACE_VERSION);
    if !obj_is(is, pins::PIN_ENGINE2, pins::EV_INPUTSERVICE_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "InputService_001 is not the pinned CInputService",
        ));
    }
    let iface = is.cast::<sys::IInputService>();
    // SAFETY: `obj_is` proved the object is the pinned CInputService; the typed vtable's
    // AddCommandText is the C's slot-25 call (`mcp_bridge_game.inc:886`).
    let add_command = unsafe {
        (*(*iface).vtable)
            .AddCommandText
            .expect("IInputService::AddCommandText (pinned slot 25)")
    };
    // SAFETY: the verified slot entry; arguments (5, cmd, 0) as the C passes (`:886`).
    unsafe { add_command(iface, 5, cmd.as_ptr().cast::<core::ffi::c_char>(), 0) };
    builders::select_command_result(out, cstr_tail(&cmd));
    Ok(())
}

/// `gs_weapon_def` (`mcp_bridge_game.inc:794-798`): the held weapon's definition index through
/// the attribute chain, -1 when any hop fails.
fn gs_weapon_def(gs: &mut schema::Gs, w: *const c_void) -> i32 {
    let mut a = 0;
    let mut b = 0;
    if !schema::gs_off(gs, "C_EconEntity", "m_AttributeManager", &mut a)
        || !schema::gs_off(gs, "C_AttributeContainer", "m_Item", &mut b)
    {
        return -1;
    }
    // `w` is a readable weapon object (proven by its handle); the offsets are
    // schema-resolved inside it, the C's `(u8*)w + a + b` (`mcp_bridge_game.inc:797`) -
    // wrapping, so a negative offset behaves as the C's; the gs_rd below probes.
    let item = w.cast::<u8>().wrapping_byte_offset(a as isize + b as isize);
    let mut d16 = [0u8; 2];
    if schema::gs_rd(gs, item.cast::<c_void>(), "C_EconItemView", "m_iItemDefinitionIndex", &mut d16) {
        i32::from(u16::from_le_bytes(d16))
    } else {
        -1
    }
}

/// `gs_find_held_weapon` (`mcp_bridge_game.inc:801-833`): the weapon held by the local pawn
/// matching exactly one of name / defIndex / handle; `weapon_knife` also matches any knife
/// variant when there is no exact match.
fn gs_find_held_weapon(name: &[u8], def: i64, handle: i64) -> Result<*mut c_void, OpErr> {
    let ec = schema::gs_engine_client()?;
    let mut gs = schema::lock();
    let mut e = OpErr::bare(0, "");
    if !schema::gs_scope(&mut gs, &mut e) {
        return Err(e);
    }
    let ctl = match schema::gs_local_controller(ec, None) {
        Ok(ctl) => ctl,
        Err(why) => return Err(OpErr::new(-32024, "NoLocalPlayer", why)),
    };
    let mut hp = sys::INVALID_EHANDLE_INDEX;
    let mut pawn: *mut c_void = core::ptr::null_mut();
    if schema::gs_u32(&mut gs, ctl, "CCSPlayerController", "m_hPlayerPawn", &mut hp) {
        pawn = schema::gs_ent_by_handle(&mut gs, hp);
    }
    if pawn.is_null() {
        return Err(OpErr::new(
            -32024,
            "NoLocalPlayer",
            "local controller has no pawn (dead/spectating?)",
        ));
    }
    let ws = schema::gs_weapon_services(&mut gs, pawn);
    if ws.is_null() {
        return Err(OpErr::new(-32024, "NoLocalPlayer", "pawn has no weapon services"));
    }
    let mut vo = 0;
    if !schema::gs_off(&mut gs, "CPlayer_WeaponServices", "m_hMyWeapons", &mut vo) {
        return Err(OpErr::new(
            -32023,
            "SchemaUnavailable",
            "CPlayer_WeaponServices::m_hMyWeapons",
        ));
    }
    // As `op_game_weapons` - a mem_ok'd services pointer plus a schema offset,
    // wrapping like the C's `(u8*)ws + vo` with any i32 offset; the C reads the vector's
    // members unguarded.
    let vec = ws.cast::<u8>().wrapping_byte_offset(vo as isize)
        .cast::<sys::CUtlVector_CEntityHandle>();
    // SAFETY: as above; the generated struct's members.
    let (n, hs) = unsafe {
        (
            addr_of!((*vec).m_Size).read_unaligned(),
            addr_of!((*vec).m_pMemory).read_unaligned(),
        )
    };
    let n = n as usize;
    let valid = n > 0 && n < 256 && !hs.is_null() && mem_ok(hs.cast());
    let n = if valid { n } else { 0 };
    let mut knife: *mut c_void = core::ptr::null_mut();
    for i in 0..n {
        // SAFETY: `hs` passed `mem_ok` when valid; `i` is bounded by `n`.
        let h = unsafe { *(hs.add(i).cast::<u32>()) };
        let w = schema::gs_ent_by_handle(&mut gs, h);
        if w.is_null() {
            continue;
        }
        // mcp_bridge_game.inc:822-823
        if handle >= 0 {
            if i64::from(h) == handle {
                return Ok(w);
            }
            continue;
        }
        if def >= 0 {
            if i64::from(gs_weapon_def(&mut gs, w)) == def {
                return Ok(w);
            }
            continue;
        }
        let idx = schema::gs_ent_index(w);
        let dn = if idx >= 0 {
            schema::gs_designer_name(&mut gs, u32::try_from(idx).expect("checked >= 0"))
        } else {
            None
        };
        let Some(dn) = dn else { continue };
        if schema::name_is_bytes(&dn, name) {
            return Ok(w);
        }
        // mcp_bridge_game.inc:828: the knife fallback.
        if knife.is_null()
            && name == b"weapon_knife"
            && (schema::name_starts_with(&dn, b"weapon_knife")
                || schema::name_is(&dn, "weapon_bayonet"))
        {
            knife = w;
        }
    }
    if !knife.is_null() {
        return Ok(knife);
    }
    Err(OpErr::new(
        -32602,
        "InvalidParams",
        "the local player does not hold that weapon (see game.weapons)",
    ))
}

/// `game.schemaProbe` (`op_game_schema_probe`, `mcp_bridge_game.inc:1210-1243`).
fn game_schema_probe(r: &Request) -> String {
    let mut buf = vec![0u8; GAME_BUF]; // mcp_bridge.c:6012
    let mut sb = Sb::new(&mut buf);
    let outcome = op_game_schema_probe(r.params.as_deref().map(str::as_bytes), &mut sb);
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

/// The C's default probe candidates (`mcp_bridge_game.inc:1213`).
const PROBE_DEFAULTS: [&str; 9] = [
    "client.dll",
    "client",
    "engine2.dll",
    "server.dll",
    "schemasystem.dll",
    "tier0.dll",
    "host.dll",
    "csgo/bin/win64/client.dll",
    "Client.dll",
];

/// The probe's names source (`mcp_bridge_game.inc:1217-1218`): the params' `names` value when
/// it starts with `[` (the scan resumes after it), else `None` for the C's default list.
/// `params` can end at `"names":` (`json_find` answers the value's start == len), so the
/// start byte is `get`, not an index - and `None` there is exactly the C reading a NUL and
/// probing the defaults (`:1218`).
fn probe_names_source(params: Option<&[u8]>) -> Option<(&[u8], usize)> {
    let params = params?;
    let start = json_find(params, b"names")?;
    if params.get(start) == Some(&b'[') {
        Some((params, start + 1))
    } else {
        None
    }
}

/// `op_game_schema_probe` (`mcp_bridge_game.inc:1210-1243`): which module names resolve to a
/// non-global type scope.
fn op_game_schema_probe(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    // mcp_bridge_game.inc:1211-1212
    let ss = mod_iface(pins::PIN_SCHEMA, sys::SCHEMASYSTEM_INTERFACE_VERSION);
    if !obj_is(ss, pins::PIN_SCHEMA, pins::SSV_SCHEMASYSTEM_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "SchemaSystem_001 is not the pinned CSchemaSystem",
        ));
    }
    let ss = ss.cast::<sys::ISchemaSystem>();
    // SAFETY: `obj_is` proved the schema system is the pinned object; the typed vtable's
    // entry is the C's S2_SLOT_ISchemaSystem_FindTypeScopeForModule call (`mcp_bridge_game.inc:1231`).
    let find_scope = unsafe {
        (*(*ss).vtable)
            .FindTypeScopeForModule
            .expect("ISchemaSystem::FindTypeScopeForModule (pinned slot 13)")
    };
    let mut name = [0u8; 128]; // mcp_bridge_game.inc:1214, static char name[128]
    out.add(b"{\"results\":[");
    let mut wrote = 0;
    // mcp_bridge_game.inc:1217-1218: the params' names array, or the defaults.
    let mut p: Option<(&[u8], usize)> = probe_names_source(params);
    let mut i = 0usize;
    loop {
        let n: Vec<u8>;
        if let Some((s, pos)) = p.as_mut() {
            // mcp_bridge_game.inc:1222-1223: skip spaces and commas, expect a quote.
            while matches!(s.get(*pos), Some(b' ' | b',')) {
                *pos += 1;
            }
            if s.get(*pos) != Some(&b'"') {
                break;
            }
            let Some(next) = jstr(s, *pos, &mut name) else {
                break;
            };
            *pos = next;
            let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            n = name[..len].to_vec();
        } else {
            let Some(d) = PROBE_DEFAULTS.get(i) else {
                break;
            };
            n = d.as_bytes().to_vec();
        }
        i += 1;
        let mut module = n.clone();
        module.push(0);
        // SAFETY: the verified slot entry; the module name as a NUL-terminated string, the
        // C's call (`mcp_bridge_game.inc:1231`).
        let scope = unsafe {
            find_scope(
                ss,
                module.as_ptr().cast::<core::ffi::c_char>(),
                core::ptr::null_mut(),
            )
        };
        let is_obj = obj_is(scope.cast::<c_void>(), pins::PIN_SCHEMA, pins::SSV_TYPESCOPE_VT.value as u32);
        // mcp_bridge_game.inc:1232-1233: isGlobal is the slot's bool, -1 when not the pinned
        // class.
        let mut is_global: i64 = -1;
        let mut scope_name: Option<Vec<u8>> = None;
        if is_obj {
            let scope = scope.cast::<sys::CSchemaSystemTypeScope>();
            // SAFETY: `obj_is` proved the scope is the pinned class; the two slot calls are
            // the C's (`mcp_bridge_game.inc:1233-1234`).
            let global_fn = unsafe {
                (*(*scope).vtable)
                    .base
                    .IsGlobalScope
                    .expect("ISchemaSystemTypeScope::IsGlobalScope (pinned slot 29)")
            };
            // SAFETY: the same verified entry, called with the interface pointer its
            // generated signature takes.
            is_global = i64::from(unsafe { global_fn(scope.cast::<sys::ISchemaSystemTypeScope>()) });
            // SAFETY: as above; GetScopeName is the C's pinned slot 28 (`mcp_bridge_game.inc:1234`).
            let name_fn = unsafe {
                (*(*scope).vtable)
                    .base
                    .GetScopeName
                    .expect("ISchemaSystemTypeScope::GetScopeName (pinned slot 28)")
            };
            // SAFETY: the same verified entry.
            let sn = unsafe { name_fn(scope.cast::<sys::ISchemaSystemTypeScope>()) };
            if !sn.is_null() && mem_ok(sn.cast()) {
                // The C hands the string to addjson in place after one mem_ok; the copy here
                // probes every page and renders identically.
                scope_name = schema::copy_cstr_checked(sn.cast()).map(|g| g.as_bytes().to_vec());
            }
        }
        if wrote > 0 {
            out.add(b",");
        }
        wrote += 1;
        builders::probe_result(out, &n, is_obj && is_global == 0, scope_name.as_deref(), is_global);
    }
    out.add(b"]}");
    Ok(())
}
