//! Multiplayer tools (workflows/05-multiplayer.md): `server_up` launches a LinuxDedicated session,
//! `client_connect` sends `connect <addr>` through the client's console route, and
//! `server_console_exec` runs a command on the dedicated server over RCON (`cs2_mcp_common::rcon`).
//!
//! The RCON password and port are persisted per server in `<artifactsDir>/server-session.json`
//! (mode 0600), so any MCP process can find a running server. An explicit `rconPassword` argument
//! overrides the file.
//!
//! RCON address: several candidates are tried in order (127.0.0.1, the addresses this host's name
//! resolves to, and the address on the server's `udp/ip :` log line). CS2 listened on the host's
//! address rather than loopback in the first live run.
//!
//! Refusal guard: `server_console_exec` applies `crate::console::refusal`, as `console_exec` does.
use crate::state::ClientState;
use crate::tools::runtime_tools::{self, runtime_error};
use crate::tools::session;
use cs2_mcp_common::config::RuntimeKind;
use cs2_mcp_common::rcon::{RconClient, RconError};
use cs2_mcp_common::runtime::docker::{
    kind_slug, server_game_args, valid_name, RuntimeError, DEDICATED_DEFAULT_PORT,
};
use cs2_mcp_common::runtime::readiness::{Readiness, ReadyStage, SERVER_LOG_NAME};
use cs2_mcp_common::runtime::{LaunchSpec, SessionKind};
use cs2_mcp_common::tools::{CallCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::hash::{BuildHasher, Hasher};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Upper bound per bot kind in one `server_up` (each `bot_add_*` is one RCON command).
pub const MAX_BOTS_PER_SIDE: u64 = 10;
/// Cap on one RCON step (connect, auth, one command); the call budget can be smaller.
const RCON_STEP: Duration = Duration::from_secs(5);
const DEFAULT_MAP: &str = "de_dust2";
/// File in the session artifacts dir holding the server's RCON secret (written with mode 0600).
pub const SESSION_FILE: &str = "server-session.json";
/// Game arguments server_up sets itself; the caller may not pass them (use the `port` parameter).
const RESERVED_ARGS: &[&str] = &[
    "+rcon_password",
    "+hostport",
    "-port",
    "-hostport",
    "+port",
    "+con_logfile",
];

pub fn server_up_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "map": {"type": "string"},
            "port": {"type": "integer", "minimum": 1, "maximum": 65535, "default": 27015},
            "rconPassword": {"type": "string"},
            "bots": {"type": "object", "properties": {
                "ct": {"type": "integer", "minimum": 0, "maximum": MAX_BOTS_PER_SIDE, "default": 0},
                "t": {"type": "integer", "minimum": 0, "maximum": MAX_BOTS_PER_SIDE, "default": 0}
            }},
            "hibernate": {"type": "boolean", "default": false, "description": "false: the server keeps ticking when empty (the bridge needs it)"},
            "args": {"type": "array", "items": {"type": "string"}, "description": "appended last. Not accepted: +rcon_password, +hostport, -port, +con_logfile"},
            "waitFor": {"type": "string", "enum": readiness_stage_names()},
            "timeoutMs": {"type": "integer", "minimum": 1, "maximum": runtime_tools::MAX_TIMEOUT_MS}
        }
    })
}

pub fn client_connect_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "container": {"type": "string"},
            "address": {"type": "string"}
        }
    })
}

pub fn server_console_exec_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "container": {"type": "string"},
            "command": {"type": "string"},
            "rconPassword": {"type": "string"},
            "port": {"type": "integer", "minimum": 1, "maximum": 65535}
        },
        "required": ["command"]
    })
}

/// What `server_up` saves per server (`<artifactsDir>/server-session.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerSession {
    pub container: String,
    pub map: String,
    pub port: u16,
    pub rcon_password: String,
    pub created_unix: u64,
}

fn readiness_stage_names() -> Vec<&'static str> {
    ReadyStage::ALL.iter().map(|s| s.as_str()).collect()
}

fn invalid(msg: &str) -> ToolError {
    ToolError::invalid_arguments(msg)
}

/// `[A-Za-z0-9_]{1,64}`: a map name as the game takes it after `+map`.
pub fn valid_map(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `[A-Za-z0-9_.-]{1,64}`: kept to characters that survive argv and the console unquoted.
pub fn valid_password(p: &str) -> bool {
    (1..=64).contains(&p.len())
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// `host:port` with a host of `[A-Za-z0-9.-]` and a non-zero port.
pub fn valid_address(addr: &str) -> bool {
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}

/// `+connect <addr>` for `launch_session {connect}`; `None` when the argument is absent.
pub fn opt_connect(args: &Value) -> Result<Option<String>, ToolError> {
    match args.get("connect") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(a)) if valid_address(a) => Ok(Some(a.clone())),
        Some(_) => Err(invalid("connect must be host:port")),
    }
}

/// The `port` parameter of server_up (default 27015).
fn port_arg(args: &Value) -> Result<u16, ToolError> {
    match args.get("port") {
        None | Some(Value::Null) => Ok(DEDICATED_DEFAULT_PORT),
        Some(v) => v
            .as_u64()
            .filter(|p| (1..=65535).contains(p))
            .map(|p| p as u16)
            .ok_or_else(|| invalid("port must be an integer in 1..=65535")),
    }
}

/// Refuse game args that duplicate what the tool sets (case-insensitive).
pub fn reject_reserved_args(extra: &[String]) -> Result<(), ToolError> {
    for a in extra {
        if RESERVED_ARGS.iter().any(|r| a.eq_ignore_ascii_case(r)) {
            return Err(invalid(&format!(
                "{a} is set by server_up; use the port / rconPassword parameters instead"
            )));
        }
    }
    Ok(())
}

/// The game args `server_up` adds after `server_game_args`' fixed flags, before the caller's
/// `extra` (which is appended last, so a user `+sv_hibernate_when_empty 1` wins over the default):
/// `+sv_hibernate_when_empty 0` unless `hibernate` opts out, and `+hostport <port>` for a
/// non-default port. Pure.
fn game_extra_args(port: u16, hibernate: bool, extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if !hibernate {
        // an empty dedicated server hibernates by default (sv_hibernate_when_empty 1): no
        // GameFrame runs, so the Metamod-hosted bridge never ticks and never starts its listener
        // (live 2026-10-20: with the convar 0 the bridge logged its first gameframe tick, started
        // the listener and answered RPC on a server with no players)
        out.extend(["+sv_hibernate_when_empty".to_string(), "0".to_string()]);
    }
    if port != DEDICATED_DEFAULT_PORT {
        out.extend(["+hostport".to_string(), port.to_string()]);
    }
    out.extend(extra.iter().cloned());
    out
}

/// A random 16-hex-digit password (process-seeded hash; for a local test server only).
fn random_password() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

fn string_array(args: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(String::from)
                    .ok_or_else(|| invalid(&format!("{key} must be an array of strings")))
            })
            .collect(),
        Some(_) => Err(invalid(&format!("{key} must be an array of strings"))),
    }
}

/// `bots: {ct, t}` as (ct, t); both default to 0.
fn bots_of(args: &Value) -> Result<(u64, u64), ToolError> {
    let Some(bots) = args.get("bots").filter(|b| !b.is_null()) else {
        return Ok((0, 0));
    };
    if !bots.is_object() {
        return Err(invalid("bots must be an object {ct, t}"));
    }
    let side = |key: &str| -> Result<u64, ToolError> {
        match bots.get(key) {
            None | Some(Value::Null) => Ok(0),
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= MAX_BOTS_PER_SIDE)
                .ok_or_else(|| {
                    invalid(&format!(
                        "bots.{key} must be an integer in 0..={MAX_BOTS_PER_SIDE}"
                    ))
                }),
        }
    };
    Ok((side("ct")?, side("t")?))
}

/// Write `server-session.json` with mode 0600. Overwrites an earlier one.
pub fn write_session(dir: &Path, session: &ServerSession) -> std::io::Result<()> {
    use std::io::Write;
    let path = dir.join(SESSION_FILE);
    let _ = std::fs::remove_file(&path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&path)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(&serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?)?;
    Ok(())
}

/// The saved session in `dir`, if there is one that parses.
pub fn read_session(dir: &Path) -> Option<ServerSession> {
    let raw = std::fs::read(dir.join(SESSION_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Hostname's IPv4 addresses as resolved on this host (`/proc/sys/kernel/hostname` + getaddrinfo).
fn hostname_ipv4() -> Vec<std::net::Ipv4Addr> {
    let Ok(host) = std::fs::read_to_string("/proc/sys/kernel/hostname") else {
        return Vec::new();
    };
    let host = host.trim();
    if host.is_empty() {
        return Vec::new();
    }
    (host, 0)
        .to_socket_addrs()
        .map(|it| {
            it.filter_map(|a| match a.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                _ => None,
            })
            .collect()
        })
        .unwrap_or_default()
}

/// The address on the server's `udp/ip :` status line in `text` (e.g. `udp/ip : 127.0.1.1:27015`).
pub fn log_udp_address(text: &str) -> Option<String> {
    let line = text.lines().rev().find(|l| l.contains("udp/ip"))?;
    let after = &line[line.find("udp/ip")? + "udp/ip".len()..];
    after
        .split_whitespace()
        .find_map(|tok| tok.parse::<SocketAddr>().ok())
        .map(|a| a.to_string())
}

/// RCON addresses to try, in order, without duplicates: loopback, the hostname's IPv4 addresses,
/// and the server log's own `udp/ip` address. Each is `ip:port`.
pub fn rcon_candidates(port: u16, artifacts: Option<&Path>) -> Vec<String> {
    let mut out = vec![format!("127.0.0.1:{port}")];
    for ip in hostname_ipv4() {
        out.push(format!("{ip}:{port}"));
    }
    if let Some(dir) = artifacts {
        if let Ok(text) = std::fs::read_to_string(dir.join(SERVER_LOG_NAME)) {
            if let Some(addr) = log_udp_address(&text) {
                out.push(addr);
            }
        }
    }
    let mut seen = Vec::new();
    out.retain(|a| {
        if seen.contains(a) {
            false
        } else {
            seen.push(a.clone());
            true
        }
    });
    out
}

/// Refuse the launch when the game port is already bound (TCP or UDP, any address).
fn check_port_free(port: u16) -> Result<(), ToolError> {
    let tcp = TcpListener::bind(("0.0.0.0", port));
    let udp = UdpSocket::bind(("0.0.0.0", port));
    if tcp.is_err() || udp.is_err() {
        return Err(ToolError::new(
            "PortInUse",
            "PortInUse",
            format!("port {port} is already bound (TCP or UDP); pick another port"),
        )
        .retry_safe(true)
        .with_data(json!({"port": port})));
    }
    Ok(())
}

/// A failed RCON step: which one (`connect`, `auth`, `exec`, `deadline`) and why.
struct RconFailure {
    phase: &'static str,
    error: RconError,
    address: String,
}

/// The time left for one RCON step: `min(RCON_STEP, deadline - now)`. Errors when the shared
/// deadline has passed.
fn step_left(deadline: Instant, address: &str) -> Result<Duration, RconFailure> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(RconFailure {
            phase: "deadline",
            error: RconError::Timeout(Duration::ZERO),
            address: address.to_string(),
        });
    }
    Ok(left.min(RCON_STEP))
}

/// Connect (trying each candidate in order), authenticate, run `commands` on that one connection.
/// One `deadline` bounds every step together. An auth rejection stops at once (the other addresses
/// reach the same server). Returns the address that worked and the replies.
async fn rcon_run(
    candidates: &[String],
    password: &str,
    commands: &[String],
    deadline: Instant,
) -> Result<(String, Vec<String>), RconFailure> {
    let mut last: Option<RconFailure> = None;
    for addr in candidates {
        let step = step_left(deadline, addr)?;
        let mut client = match RconClient::connect(addr, step).await {
            Ok(c) => c,
            Err(e) => {
                last = Some(RconFailure {
                    phase: "connect",
                    error: e,
                    address: addr.clone(),
                });
                continue;
            }
        };
        let step = step_left(deadline, addr)?;
        match client.auth(password, step).await {
            Ok(()) => {}
            Err(RconError::AuthFailed) => {
                return Err(RconFailure {
                    phase: "auth",
                    error: RconError::AuthFailed,
                    address: addr.clone(),
                })
            }
            Err(e) => {
                last = Some(RconFailure {
                    phase: "auth",
                    error: e,
                    address: addr.clone(),
                });
                continue;
            }
        }
        let mut replies = Vec::with_capacity(commands.len());
        for command in commands {
            let step = step_left(deadline, addr)?;
            match client.exec(command, step).await {
                Ok(r) => replies.push(r),
                Err(e) => {
                    return Err(RconFailure {
                        phase: "exec",
                        error: e,
                        address: addr.clone(),
                    })
                }
            }
        }
        return Ok((addr.clone(), replies));
    }
    Err(last.unwrap_or(RconFailure {
        phase: "connect",
        error: RconError::Protocol("no RCON address to try".into()),
        address: candidates.join(", "),
    }))
}

/// Map an RCON failure to the tool taxonomy. Only a failure after a command was sent may have
/// executed it (`outcomeUnknown`, not retry-safe).
fn rcon_error(f: RconFailure) -> ToolError {
    let message = format!("rcon {} to {}: {}", f.phase, f.address, f.error);
    let data = json!({"address": f.address, "phase": f.phase});
    match (f.phase, &f.error) {
        (_, RconError::AuthFailed) => ToolError::new("RconAuthFailed", "RconAuthFailed", message)
            .retry_safe(true)
            .with_data(data),
        ("exec", _) => {
            let mut e = ToolError::new("RconExecFailed", "RconExecFailed", message).with_data(data);
            e.outcome_unknown = true;
            e
        }
        _ => ToolError::new("RconUnavailable", "RconUnavailable", message)
            .retry_safe(true)
            .with_data(json!({
                "address": f.address,
                "phase": f.phase,
                "hint": "the server may still be loading its map (wait_session / ServerLive), or the port or rconPassword is wrong",
            })),
    }
}

/// Bots added after the server went live. Always a report, never an error: the server is up.
async fn add_bots(
    candidates: &[String],
    password: &str,
    ct: u64,
    t: u64,
    deadline: Instant,
) -> Value {
    let requested = json!({"ct": ct, "t": t});
    let commands: Vec<String> = std::iter::repeat_n("bot_add_ct", ct as usize)
        .chain(std::iter::repeat_n("bot_add_t", t as usize))
        .map(String::from)
        .collect();
    if commands.is_empty() {
        return json!({"requested": requested, "applied": false, "reason": "no bots requested"});
    }
    match rcon_run(candidates, password, &commands, deadline).await {
        Ok((addr, replies)) => json!({
            "requested": requested,
            "applied": true,
            "address": addr,
            "replies": replies,
        }),
        Err(f) => json!({
            "requested": requested,
            "applied": false,
            "phase": f.phase,
            "address": f.address,
            "error": f.error.to_string(),
            "hint": "bot_add_* may be partly applied; check with server_console_exec",
        }),
    }
}

/// `server_up`: launch a LinuxDedicated session and wait (bounded) for ServerLive.
pub async fn server_up(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    if state.runtime_kind != RuntimeKind::Docker {
        return Err(ToolError::not_implemented(
            "NotImplementedYet",
            "the local runtime is not implemented yet",
            json!({"blockedOn": "workflows/10-local-install.md"}),
        ));
    }
    let map = match args.get("map") {
        None | Some(Value::Null) => DEFAULT_MAP.to_string(),
        Some(Value::String(m)) if valid_map(m) => m.clone(),
        Some(_) => return Err(invalid("map must be [A-Za-z0-9_]{1,64}")),
    };
    let port = port_arg(&args)?;
    let password = match args.get("rconPassword") {
        None | Some(Value::Null) => random_password(),
        Some(Value::String(p)) if valid_password(p) => p.clone(),
        Some(_) => return Err(invalid("rconPassword must be [A-Za-z0-9_.-]{1,64}")),
    };
    let extra = string_array(&args, "args")?;
    reject_reserved_args(&extra)?;
    let hibernate = match args.get("hibernate") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("hibernate must be a boolean")),
    };
    let (ct, t) = bots_of(&args)?;
    let wait_for = runtime_tools::opt_wait_for(&args)?.unwrap_or(ReadyStage::ServerLive);
    let requested_ms =
        runtime_tools::opt_timeout(&args)?.unwrap_or(runtime_tools::DEFAULT_TIMEOUT_MS);
    check_port_free(port)?;

    // the full server log goes to stdout (no -condebug, and CS2 has no con_logfile): /work/server.log
    let game_extra = game_extra_args(port, hibernate, &extra);
    let spec = LaunchSpec {
        kind: SessionKind::LinuxDedicated,
        testscript: None,
        map: None,
        args: server_game_args(&map, Some(&password), &game_extra),
        headless: true,
        gpus: false,
        container_name: None,
        capture: false,
        bridge: true,
    };
    let launched = state
        .runtime
        .launch_within(&spec, runtime_tools::lock_budget(&ctx))
        .await
        .map_err(runtime_error)?;
    let container = launched.container.clone();
    let session = ServerSession {
        container: container.clone(),
        map: map.clone(),
        port,
        rcon_password: password.clone(),
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    if !launched.reused {
        write_session(&launched.artifacts_dir, &session).map_err(|e| {
            ToolError::new(
                "RuntimeError",
                "RuntimeError",
                format!("cannot save server-session.json: {e}"),
            )
        })?;
    }
    let timeout = runtime_tools::wait_within_budget(&ctx, requested_ms);
    let readiness: Readiness = match state
        .runtime
        .wait_ready(&container, wait_for, timeout)
        .await
    {
        Ok(r) => r,
        Err(e) => Readiness::failed(wait_for, format!("{e:#}")),
    };
    let (pending, next) = runtime_tools::wait_report(&readiness);
    let candidates = rcon_candidates(port, Some(&launched.artifacts_dir));
    let mut out = json!({
        "container": container,
        "kind": kind_slug(&spec.kind),
        "map": map,
        "port": port,
        "address": format!("127.0.0.1:{port}"),
        "rconCandidates": candidates,
        "rconPassword": password,
        "sessionFile": launched.artifacts_dir.join(SESSION_FILE).display().to_string(),
        "reused": launched.reused,
        "argv": launched.argv,
        "paths": runtime_tools::paths_json(&launched.paths),
        "artifactsDir": launched.artifacts_dir.display().to_string(),
        "readiness": readiness,
        "pending": pending,
    });
    if let Some(n) = next {
        out["next"] = json!(n);
    }
    if ct + t > 0 {
        out["bots"] = if readiness.target_reached {
            let deadline = bots_deadline(&ctx)?;
            add_bots(&candidates, &password, ct, t, deadline).await
        } else {
            json!({
                "requested": {"ct": ct, "t": t},
                "applied": false,
                "reason": "the server is not live yet; continue with wait_session, then add bots with server_console_exec",
            })
        };
    }
    Ok(out.into())
}

/// One deadline for all RCON steps of a call: the call budget left (capped by the RCON step cap per
/// step, not overall).
fn bots_deadline(ctx: &CallCtx) -> Result<Instant, ToolError> {
    let ms = ctx.budget.deadline_ms()?;
    Ok(Instant::now() + Duration::from_millis(ms))
}

/// `client_connect`: `connect <address>` through the client session's console route. A wine-tools
/// session has one (the bridge's console.exec, -testscript or -tools sessions only). A linux-client has none
/// yet and says so.
pub async fn client_connect(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let address = match args.get("address") {
        None | Some(Value::Null) => format!("127.0.0.1:{DEDICATED_DEFAULT_PORT}"),
        Some(Value::String(a)) if valid_address(a) => a.clone(),
        Some(_) => return Err(invalid("address must be host:port")),
    };
    let (name, kind) = match args.get("container") {
        None | Some(Value::Null) => {
            let o = state
                .runtime
                .owned()
                .into_iter()
                .rev()
                .find(|o| !matches!(o.spec.kind, SessionKind::LinuxDedicated))
                .ok_or_else(|| {
                    runtime_error(
                        RuntimeError::NoSession("no client session launched by this server".into())
                            .into(),
                    )
                })?;
            (o.name, kind_slug(&o.spec.kind).to_string())
        }
        Some(Value::String(name)) => {
            if !valid_name(name) {
                return Err(invalid("container must match ^cs2mcp-[a-z0-9][a-z0-9-]*$"));
            }
            let entry = state
                .runtime
                .session_entry(name)
                .await
                .map_err(runtime_error)?
                .ok_or_else(|| {
                    runtime_error(RuntimeError::NoSession(format!("{name} does not exist")).into())
                })?;
            (entry.name, entry.kind)
        }
        Some(_) => return Err(invalid("container must be a string")),
    };
    match kind.as_str() {
        "wine-tools" => {
            let mut out = session::console_exec(
                state,
                ctx,
                json!({"command": format!("connect {address}"), "session": name}),
            )
            .await?;
            out.json["address"] = json!(address);
            Ok(out)
        }
        "linux-client" => Err(ToolError::not_implemented(
            "NotImplementedYet",
            format!("a linux-client session has no console command route yet, so {name} cannot be told to connect"),
            json!({
                "container": name,
                "address": address,
                "missing": "a command route into a linux-client session (no bridge; no exec channel yet)",
                "note": "launch_session {connect} (+connect at launch) does NOT work as of the first live test: the client joins its own loopback listen server",
                "blockedOn": "workflows/05-multiplayer.md (open question: client join path)",
            }),
        )),
        "linux-dedicated" => Err(invalid(
            "container is a dedicated server; client_connect targets a client session",
        )),
        other => Err(invalid(&format!(
            "container kind {other} cannot connect to a server"
        ))),
    }
}

/// `server_console_exec`: one command on a dedicated server over RCON, behind the refusal guard.
/// The password and port come from `<artifactsDir>/server-session.json` (or the arguments), so this
/// works from any MCP process, not only the one that launched the server.
pub async fn server_console_exec(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| invalid("command must be a non-empty string"))?;
    if let Some(reason) = cs2_mcp_common::console::refusal(command) {
        return Err(ToolError::new(
            "ForbiddenCommand",
            "ForbiddenCommand",
            format!("refusing {command:?}: {reason}"),
        )
        .retry_safe(true)
        .with_data(json!({"command": command})));
    }
    if let Some(why) = cs2_mcp_common::console::engine_limit_violation(command) {
        return Err(invalid(&format!(
            "{why}. Nothing was sent: shorten or split that command"
        )));
    }
    let entry = dedicated_target(&state, &args).await?;
    let dir = entry.artifacts_dir.clone().ok_or_else(|| {
        runtime_error(
            RuntimeError::NoSession(format!("{} has no artifacts dir", entry.name)).into(),
        )
    })?;
    let saved = read_session(&dir);
    let password =
        match args.get("rconPassword") {
            None | Some(Value::Null) => saved
                .as_ref()
                .map(|s| s.rcon_password.clone())
                .ok_or_else(|| {
                    runtime_error(
                        RuntimeError::NoSession(format!(
                            "{}/{SESSION_FILE} is missing; pass rconPassword",
                            dir.display()
                        ))
                        .into(),
                    )
                })?,
            Some(Value::String(p)) if valid_password(p) => p.clone(),
            Some(_) => return Err(invalid("rconPassword must be [A-Za-z0-9_.-]{1,64}")),
        };
    let port = match args.get("port") {
        None | Some(Value::Null) => saved
            .as_ref()
            .map(|s| s.port)
            .unwrap_or(DEDICATED_DEFAULT_PORT),
        Some(_) => port_arg(&args)?,
    };
    let candidates = rcon_candidates(port, Some(&dir));
    let deadline = bots_deadline(&ctx)?;
    let (address, mut replies) = rcon_run(&candidates, &password, &[command.to_string()], deadline)
        .await
        .map_err(rcon_error)?;
    Ok(json!({
        "container": entry.name,
        "command": command,
        "response": replies.pop().unwrap_or_default(),
        "address": address,
        "route": "rcon",
    })
    .into())
}

/// The one running dedicated server a call refers to: the named container, or the only one.
async fn dedicated_target(
    state: &ClientState,
    args: &Value,
) -> Result<cs2_mcp_common::runtime::docker::SessionEntry, ToolError> {
    let no_server = || {
        runtime_error(
            RuntimeError::NoSession(
                "no running dedicated server found; call server_up first".into(),
            )
            .into(),
        )
    };
    match args.get("container") {
        None | Some(Value::Null) => {
            let running: Vec<_> = state
                .runtime
                .list_sessions()
                .await
                .map_err(runtime_error)?
                .into_iter()
                .filter(|e| e.kind == "linux-dedicated" && e.running())
                .collect();
            match running.len() {
                0 => Err(no_server()),
                1 => Ok(running.into_iter().next().expect("one entry")),
                _ => Err(invalid(
                    "several dedicated servers are running; pass container",
                )),
            }
        }
        Some(Value::String(name)) => {
            if !valid_name(name) {
                return Err(invalid("container must match ^cs2mcp-[a-z0-9][a-z0-9-]*$"));
            }
            let entry = state
                .runtime
                .session_entry(name)
                .await
                .map_err(runtime_error)?
                .filter(|e| e.kind == "linux-dedicated")
                .ok_or_else(no_server)?;
            if !entry.running() {
                return Err(ToolError::new(
                    "ServerNotRunning",
                    "ServerNotRunning",
                    format!("{name} is not running ({})", entry.state),
                )
                .retry_safe(true)
                .with_data(json!({"container": name})));
            }
            Ok(entry)
        }
        Some(_) => Err(invalid("container must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_need_a_host_and_a_nonzero_port() {
        assert!(valid_address("127.0.0.1:27015"));
        assert!(valid_address("localhost:1"));
        assert!(!valid_address("127.0.0.1"));
        assert!(!valid_address("127.0.0.1:0"));
        assert!(!valid_address("127.0.0.1:70000"));
        assert!(!valid_address(":27015"));
        assert!(!valid_address("a b:27015"));
    }

    #[test]
    fn map_and_password_shapes() {
        assert!(valid_map("de_dust2"));
        assert!(!valid_map("de dust2"));
        assert!(!valid_map("../x"));
        assert!(!valid_map(""));
        assert!(valid_password("pw-1.x_y"));
        assert!(!valid_password("has space"));
        assert!(!valid_password("x;rm"));
        assert!(!valid_password(&"a".repeat(65)));
    }

    #[test]
    fn reserved_args_are_refused_whatever_their_case() {
        assert!(reject_reserved_args(&["+rcon_password".into(), "x".into()]).is_err());
        assert!(reject_reserved_args(&["-PORT".into(), "1".into()]).is_err());
        assert!(reject_reserved_args(&["+hostport".into()]).is_err());
        assert!(reject_reserved_args(&["-tickrate".into(), "64".into()]).is_ok());
        // sv_hibernate_when_empty is deliberately NOT reserved: a user value must win
        assert!(reject_reserved_args(&["+sv_hibernate_when_empty".into(), "1".into()]).is_ok());
    }

    #[test]
    fn hibernate_is_off_by_default_and_the_callers_args_win() {
        // default: +sv_hibernate_when_empty 0, before anything the caller passed
        assert_eq!(
            game_extra_args(DEDICATED_DEFAULT_PORT, false, &[]),
            ["+sv_hibernate_when_empty", "0"].map(String::from)
        );
        // hibernate: true opts out: the convar stays at the game default, nothing is added
        assert!(game_extra_args(DEDICATED_DEFAULT_PORT, true, &[]).is_empty());
        // a non-default port still carries +hostport
        assert_eq!(
            game_extra_args(27016, true, &[]),
            ["+hostport", "27016"].map(String::from)
        );
        // the caller's args are appended last, so a user +sv_hibernate_when_empty 1 wins
        let args = game_extra_args(
            DEDICATED_DEFAULT_PORT,
            false,
            &["+sv_hibernate_when_empty".into(), "1".into()],
        );
        assert_eq!(
            args,
            ["+sv_hibernate_when_empty", "0", "+sv_hibernate_when_empty", "1"]
                .map(String::from)
        );
    }

    #[test]
    fn bots_are_bounded_per_side() {
        assert_eq!(
            bots_of(&json!({"bots": {"ct": 3, "t": 2}})).unwrap(),
            (3, 2)
        );
        assert_eq!(bots_of(&json!({})).unwrap(), (0, 0));
        assert!(bots_of(&json!({"bots": {"ct": 11}})).is_err());
        assert!(bots_of(&json!({"bots": 3})).is_err());
    }

    #[test]
    fn the_udp_line_of_a_status_dump_gives_an_address() {
        let text = "hostname: x\nudp/ip  : 127.0.1.1:27015 (public 192.168.122.1:27015)\n";
        assert_eq!(log_udp_address(text).as_deref(), Some("127.0.1.1:27015"));
        assert_eq!(log_udp_address("no such line"), None);
    }

    #[test]
    fn candidates_start_with_loopback_and_have_no_duplicates() {
        let c = rcon_candidates(27015, None);
        assert_eq!(c[0], "127.0.0.1:27015");
        let mut sorted = c.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len());
    }
}
