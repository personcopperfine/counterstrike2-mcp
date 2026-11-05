//! The pure reply builders of the C bridge's `control.*` methods, ported from
//! `bridge/mcp_bridge.c` (frozen, m2.20): `build_caps` (`:1190-1220`), `build_error` /
//! `build_error_ext` (`:1563-1583`) and the reply-building halves of the `ipc_thread` control
//! handlers - `control.ping` (`:6497-6529`), `control.status` (`:6530-6553`),
//! `control.capabilities` (`:6554-6558`), `control.version` (`:6559-6578`), the
//! `control.hello` ok reply (`:6478-6486`) and the method-gate error objects
//! (`:6601-6623`, `exec_request:5657-5698`).
//!
//! No sockets, no threads, no queues: the verified-module state and the runtime numbers
//! ([`crate::registry::ModuleView`], ticks, queue depth, revision) arrive as plain input
//! structs; WP3.A and WP2.A fill them. Output must be byte-identical to the C for the same
//! inputs - the differential test (`crates/cs2-bridge/tests/control.rs`) compiles the C
//! builders out of the frozen source with `cc` and compares the bytes, scenario by scenario.
//!
//! # What is deliberately NOT ported here
//!
//! * `control.cancel` (`mcp_bridge.c:6579-6590`): when the id is queued the reply is the
//!   `{"cancelled":true}` result (`:6585`), else [`error_ext`] with `-32010 NotCancellable`
//!   and data `{"reason":"not queued or already running"}` (`:6587`); the queue interaction
//!   (`q_remove_id`) is WP2.A's.
//! * `control.shutdown` (`:6591-6602`): the reply is `{"closing":true}` via [`result_ok`]
//!   (`:6593`); the side effects - flushing the outbound queue, setting `g_shuttingDown`,
//!   waking both condition variables, `shutdown()` on the listener, closing after the reply -
//!   belong to the listener port.
//! * The enqueue path after the gate (`:6623-6668`): queue push, `deadlineMs` parsing and the
//!   `-32602` / `-32007 Busy` replies are queue concerns; [`error`] / [`error_ext`] are the
//!   builders those paths call.
//!
//! # Overflow
//!
//! Every builder writes into a [`MAX_LINE`] buffer through [`Sb`] - the C's own `SB`
//! behaviour (`mcp_bridge.c:228-245`): a write that would not fit sets the overflow flag and
//! refuses the byte, and the caller ships what fit. The C's control handlers never check the
//! flag for these replies, so neither do these builders; on overflow the frame is the
//! truncated buffer, exactly like the C's `resp`. [`MAX_LINE`] is `mcp_bridge.c:124`
//! (`1024u * 1024u`).

use crate::entity_json::Sb;
use crate::generated::pins;
use crate::registry::{verified_mask, ModState, ModuleView, METHODS};

/// `BRIDGE_BUILD` (`mcp_bridge.c:127`), re-exported from [`crate::BRIDGE_BUILD`] - one copy of
/// the value for the whole crate.
pub use crate::BRIDGE_BUILD;

/// `MAX_LINE` (`mcp_bridge.c:124`): the advertised `limits.maxLineBytes`, and the size of the
/// reply buffer the C builds every reply in.
pub const MAX_LINE: u32 = 1024 * 1024;

/// `gameBuild`: the generated `PIN_GAME_BUILD` (`bridge_pins.h:18`, `mcp_bridge.c:128`).
pub const GAME_BUILD: &str = pins::PIN_GAME_BUILD;

/// What `control.ping` reads (`mcp_bridge.c:6497-6528`): the ticks and queue state WP2.A
/// hands over. The derived fields (uptime, main-thread age, drain-blocked) are computed here,
/// from the C's own expressions.
pub struct PingInput<'a> {
    /// `now` - the reader loop's `GetTickCount64()`.
    pub now: u64,
    /// `g_startTick` (`mcp_bridge.c:1024`).
    pub start_tick: u64,
    /// `g_lastTick` (`mcp_bridge.c:1015`, slot 13). The C saturates a stale-or-newer tick to 0
    /// (`:6505-6510`).
    pub last_tick: u64,
    /// `g_inflightStart` (`mcp_bridge.c:1016`); 0 = nothing in flight.
    pub inflight_start: u64,
    /// `q_depth()` (`mcp_bridge.c:6520`).
    pub queue_depth: u64,
    /// The pin modules' states, in pin order.
    pub modules: &'a [ModuleView],
    /// `g_capsRevision` (`mcp_bridge.c:1150`).
    pub revision: u64,
    /// `g_shuttingDown` (`mcp_bridge.c:1018`).
    pub shutting_down: bool,
}

/// One queued request as `control.status` reports it (`mcp_bridge.c:6532-6541`).
pub struct QueueEntry<'a> {
    /// `rq->id` - the id as the C stored it (a C string).
    pub id: &'a str,
    /// `rq->method`.
    pub method: &'a str,
    /// `rq->receivedAt` (`GetTickCount64()` at queue time).
    pub received_at: u64,
}

/// What `control.status` reads (`mcp_bridge.c:6530-6552`).
pub struct StatusInput<'a> {
    /// The status call's own `GetTickCount64()`; ages are computed against it here.
    pub now: u64,
    /// The queue, head first (the C walks `g_q[(g_qHead + qi) % QCAP]`).
    pub queue: &'a [QueueEntry<'a>],
    /// `g_listener != INVALID_SOCKET` (`mcp_bridge.c:6543-6544`).
    pub listener: bool,
    /// `g_inflightStart`; 0 = nothing in flight.
    pub inflight_start: u64,
    /// `g_capsRevision`.
    pub revision: u64,
    /// `g_shuttingDown`.
    pub shutting_down: bool,
    /// Rust-only: the bridge jobs as `(id, kind, state)`, oldest first ([`crate::ops::job`]).
    /// The C's member is always `[]`; so is this on a bridge that never started a job.
    pub jobs: &'a [(&'a str, &'a str, &'a str)],
}

/// Runs `build` over a fresh [`MAX_LINE`] buffer - the C's `SB b; sb_init(&b, resp, MAX_LINE);`
/// (`mcp_bridge.c:6496` et al.) - and returns what was written.
fn build_reply(f: impl FnOnce(&mut Sb)) -> String {
    let mut buf = vec![0u8; MAX_LINE as usize];
    let mut sb = Sb::new(&mut buf);
    f(&mut sb);
    // The inputs are fixed ASCII literals, generated pin names and addjson-escaped strings -
    // all valid UTF-8 (addjson emits one U+FFFD per invalid subpart), so the lossy conversion
    // is the identity except on an overflow-truncated multibyte sequence.
    String::from_utf8_lossy(sb.as_cstr()).into_owned()
}

/// The `{"jsonrpc":"2.0","id":X,"bridge":{"protocol":1,"build":"m2.20"}}` every reply opens
/// with (`mcp_bridge.c:6499`, `:1565`).
fn prefix(sb: &mut Sb, id: Option<&str>) {
    sb.add(b"{\"jsonrpc\":\"2.0\",\"id\":");
    match id {
        Some(id) => sb.addjson(id.as_bytes()),
        None => sb.add(b"null"),
    }
    sb.add(b",\"bridge\":{\"protocol\":1,\"build\":\"");
    sb.add(BRIDGE_BUILD.as_bytes());
    sb.add(b"\"}");
}

/// The C's id handling for the four plain control replies: `sb_addjson(&b, idbuf)`
/// UNCONDITIONALLY (`mcp_bridge.c:6499`, `:6530`, `:6554`, `:6560`) - there is no `hasId`
/// check on that path (notifications are rejected earlier, `:6490-6496`), and `idbuf` is `""`
/// when the request carried no id (`:6433-6435`), so the reply reads `"id":""`. [`None`]
/// reproduces exactly that.
fn prefix_always_id(sb: &mut Sb, id: Option<&str>) {
    let id = id.unwrap_or(""); // the C's empty idbuf (mcp_bridge.c:6433-6435)
    prefix(sb, Some(id));
}

/// The pre-serialized `data` member of a gate error object: the C builds it in a 192-byte
/// scratch `SB` (`char data[192]; SB db; sb_init(&db, data, sizeof data);`,
/// `mcp_bridge.c:6612`, `:5692`) and hands the bytes to `build_error_ext`. Same here - same
/// 192 bytes, same truncation-on-overflow behaviour.
fn data_json(f: impl FnOnce(&mut Sb)) -> String {
    const DATA_CAP: usize = 192; // mcp_bridge.c:6612, :5692
    let mut buf = vec![0u8; DATA_CAP];
    let mut sb = Sb::new(&mut buf);
    f(&mut sb);
    String::from_utf8_lossy(sb.as_cstr()).into_owned()
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `build_caps` (`mcp_bridge.c:1190-1220`), over an existing builder: the capabilities
/// object - the advertised methods (implemented, not hidden, needs satisfied by the verified
/// mask **plus** [`crate::registry::NEED_KHOOK`], which is folded in unconditionally - see the
/// comment on `mask` below and `capabilities_object`), the fixed features, the `maxLineBytes`
/// limit, the pin modules with their state and the first 8 bytes of their pinned sha256, and
/// the revision.
///
/// `include` is a Rust-only narrowing of the C's predicate: the C's table has no notion of a
/// row that exists and is implemented but is not served by *this* build, so the Rust bridge
/// passes its `ported` list here (WP2.D). The C's own three tests are still applied first, so
/// `|_| true` reproduces the C byte for byte (the differential's call).
///
/// `rust_rows` is the second Rust-only switch, and the two C-identical paths pass `false`:
/// when it is set the advertisement also walks [`crate::registry::RUST_METHODS`] - this
/// build's own rows, which the C table cannot carry. Only
/// [`capabilities_object_filtered`]/[`capabilities_filtered`] (the Rust bridge's live
/// advertisement) set it, so the differential's `capabilities`/`hello` bytes stay the C's.
fn build_caps(
    sb: &mut Sb,
    modules: &[ModuleView],
    revision: u64,
    include: &dyn Fn(&crate::registry::Method) -> bool,
    rust_rows: bool,
) {
    // `NEED_KHOOK` is folded in unconditionally: it is not a module, and its presence changes
    // at runtime (Metamod `load`/`unload`, while this image keeps running). A hook row is
    // advertised because *this build implements it*; whether it can run right now is the gate's
    // answer (`-32017` with the `blockedOn` text, `registry::KHOOK_BLOCKED_ON`). Tying the
    // advertised set to the `IKHook` would make a client re-fetch capabilities on a Metamod
    // reload without the caps revision moving.
    let mask = verified_mask(modules) | crate::registry::NEED_KHOOK;
    sb.add(b"{\"methods\":["); // mcp_bridge.c:1192
    let mut first = true;
    for m in METHODS {
        // The Rust advertisement reads each C row through its override; the C-identical path
        // (`rust_rows == false`) stays the raw C table.
        let m = if rust_rows { crate::registry::effective(m) } else { m };
        if !m.implemented || m.hidden {
            continue; // mcp_bridge.c:1195
        }
        if (mask & m.needs) != m.needs {
            continue; // mcp_bridge.c:1196
        }
        if !include(m) {
            continue; // Rust-only (WP2.D): not served by this build yet
        }
        if !first {
            sb.add(b",");
        }
        first = false;
        sb.addjson(m.name.as_bytes());
    }
    if rust_rows {
        // This build's own rows, advertised under the same three tests. Rows answered on
        // the IPC lane are those whose op says so via `serves` (`ops::metamod::serves` for
        // `metamod.plugins`): such a row answers its own prerequisites, so its `needs`
        // names no module bit. That is a property of THAT row, not of being a Rust row -
        // a future Rust row that needs a module is gated here exactly like a C row.
        for m in crate::registry::RUST_METHODS {
            if !m.implemented || m.hidden || (mask & m.needs) != m.needs || !include(m) {
                continue;
            }
            if !first {
                sb.add(b",");
            }
            first = false;
            sb.addjson(m.name.as_bytes());
        }
    }
    sb.add(b"],\"features\":{\"tcp\":true,\"pipe\":false,\"consoleCommand\":false},\"limits\":{\"maxLineBytes\":"); // mcp_bridge.c:1199
    sb.addnum(u64::from(MAX_LINE));
    sb.add(b"},\"modules\":{"); // mcp_bridge.c:1201
    assert_eq!(
        modules.len(),
        pins::PIN_MODULE_COUNT,
        "modules must carry one view per pin module, in PIN_MODULES order"
    );
    for (i, pm) in pins::PIN_MODULES.iter().enumerate() {
        if i != 0 {
            sb.add(b",");
        }
        sb.addjson(pm.name.as_bytes());
        sb.add(b":{\"state\":\""); // mcp_bridge.c:1205-1207
        sb.add(modules[i].state.as_str().as_bytes());
        sb.add(b"\",\"sha256\":\""); // mcp_bridge.c:1208
        for &byte in pm.sha256.iter().take(8) {
            // first 8 bytes, two lowercase hex digits each (mcp_bridge.c:1209-1213)
            sb.putc(HEX[usize::from(byte >> 4)]);
            sb.putc(HEX[usize::from(byte & 15)]);
        }
        sb.add(b"\"");
        // Rust-only, and only when non-empty: the pins the KHook-aware identity check relaxed
        // for this module (`modules::slot_matches`). An empty list is omitted rather than sent
        // as `[]`, which keeps the bytes identical to the C's for a module with no relaxation
        // (the differential in `tests/control.rs` compares them) and keeps the wire small; a
        // reader treats a missing member as `[]`.
        if !modules[i].relaxed.is_empty() {
            sb.add(b",\"relaxed\":[");
            for (r, what) in modules[i].relaxed.iter().enumerate() {
                if r != 0 {
                    sb.add(b",");
                }
                sb.addjson(what.as_bytes());
            }
            sb.add(b"]");
        }
        sb.add(b"}");
    }
    sb.add(b"},\"revision\":"); // mcp_bridge.c:1217
    sb.addnum(revision);
    sb.add(b"}");
}

/// The capabilities object itself - the value of `control.capabilities`' result, and the
/// `capabilities` member of `control.hello`'s result.
///
/// The advertisement's method mask is the verified modules' **plus**
/// [`crate::registry::NEED_KHOOK`], unconditionally (`build_caps`): the bit is not a module, and
/// its presence changes at runtime (Metamod `load`/`unload` while this image keeps running), so
/// tying the advertised set to the `IKHook` would make a client re-fetch capabilities on a
/// Metamod reload without the caps revision moving. A row that needs the bit is therefore
/// advertised because this build implements it; whether it can run right now is the gate's
/// `-32017` answer. The caps revision moves when a module verifies and when a module's
/// `"relaxed"` list changes (`modules::record_relaxed`).
pub fn capabilities_object(modules: &[ModuleView], revision: u64) -> String {
    capabilities_object_filtered_c(modules, revision, &|_| true)
}

/// [`capabilities_object_filtered`] with this build's own rows **excluded** - the C's
/// `build_caps` byte for byte (the differential's call): `capabilities`/`hello`/this object
/// are what the frozen C bridge would have answered.
pub fn capabilities_object_filtered_c(
    modules: &[ModuleView],
    revision: u64,
    include: &dyn Fn(&crate::registry::Method) -> bool,
) -> String {
    build_reply(|sb| build_caps(sb, modules, revision, include, false))
}

/// [`capabilities_object`] with the Rust-only `ported` filter (see `build_caps`): the
/// capabilities object the Rust bridge advertises, where a C-implemented but unported method is
/// not listed **and this build's own rows** ([`crate::registry::RUST_METHODS`]) are. The
/// differential never calls the filtered builders for a byte comparison - the C's
/// `build_caps` cannot know a Rust-only row, so the extra names live behind this function
/// only.
pub fn capabilities_object_filtered(
    modules: &[ModuleView],
    revision: u64,
    include: &dyn Fn(&crate::registry::Method) -> bool,
) -> String {
    build_reply(|sb| build_caps(sb, modules, revision, include, true))
}

/// `control.capabilities` (`mcp_bridge.c:6554-6558`): the full reply - the capabilities object
/// IS the result.
pub fn capabilities(id: Option<&str>, modules: &[ModuleView], revision: u64) -> String {
    capabilities_filtered_c(id, modules, revision, &|_| true)
}

/// [`capabilities`] with the Rust-only `ported` filter.
pub fn capabilities_filtered(
    id: Option<&str>,
    modules: &[ModuleView],
    revision: u64,
    include: &dyn Fn(&crate::registry::Method) -> bool,
) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":");
        build_caps(sb, modules, revision, include, true);
        sb.add(b"}\n"); // mcp_bridge.c:6557: the result IS the capabilities object
    })
}

/// [`capabilities`] byte-identical to the C, with this build's own rows excluded (see
/// [`capabilities_object_filtered_c`]) - the differential's entry point.
pub fn capabilities_filtered_c(
    id: Option<&str>,
    modules: &[ModuleView],
    revision: u64,
    include: &dyn Fn(&crate::registry::Method) -> bool,
) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":");
        build_caps(sb, modules, revision, include, false);
        sb.add(b"}\n"); // mcp_bridge.c:6557: the result IS the capabilities object
    })
}

/// The `control.hello` ok reply (`mcp_bridge.c:6478-6486`), sent synchronously through the
/// auth gate. Unlike the four plain control replies this one DOES check `hasId`
/// (`:6480`): [`None`] writes `"id":null`.
///
/// The Rust bridge does not call this builder on the wire: its listener builds the hello frame
/// around the handler's capabilities object (`ipc::listener::hello_reply`), which is the same
/// bytes with the handler's `ported`-filtered capabilities. This stays for the differential.
pub fn hello(id: Option<&str>, modules: &[ModuleView], revision: u64) -> String {
    build_reply(|sb| {
        prefix(sb, id);
        sb.add(b",\"result\":{\"protocol\":1,\"bridgeVersion\":\""); // mcp_bridge.c:6481
        sb.add(BRIDGE_BUILD.as_bytes());
        sb.add(b"\",\"gameBuild\":\""); // mcp_bridge.c:6481-6482 (the BRIDGE_BUILD / GAME_BUILD literals)
        sb.add(GAME_BUILD.as_bytes());
        sb.add(b"\",\"capabilities\":"); // mcp_bridge.c:6482
        build_caps(sb, modules, revision, &|_| true, false);
        sb.add(b"}"); // mcp_bridge.c:6483 (closes the result)
        sb.add(b"}\n"); // mcp_bridge.c:6484 (closes the frame)
    })
}

/// The `control.version` arm's result object, without the envelope (`mcp_bridge.c:6560-6577`).
///
/// The IPC lane wraps a handler's reply in the envelope, so the handler returns this object;
/// [`version`] is the same bytes with the envelope around it (the differential's call).
pub fn version_object(modules: &[ModuleView]) -> String {
    build_reply(|sb| {
        sb.add(b"{\"protocol\":1,\"bridgeVersion\":\""); // mcp_bridge.c:6561
        sb.add(BRIDGE_BUILD.as_bytes());
        sb.add(b"\",\"gameBuild\":\"");
        sb.add(GAME_BUILD.as_bytes());
        sb.add(b"\",\"hostModules\":["); // mcp_bridge.c:6561-6562
        assert_eq!(
            modules.len(),
            pins::PIN_MODULE_COUNT,
            "modules must carry one view per pin module, in PIN_MODULES order"
        );
        for (i, pm) in pins::PIN_MODULES.iter().enumerate() {
            if i != 0 {
                sb.add(b",");
            }
            sb.add(b"{\"name\":"); // mcp_bridge.c:6567-6568
            sb.addjson(pm.name.as_bytes());
            sb.add(b",\"state\":"); // mcp_bridge.c:6569
            sb.addjson(modules[i].state.as_str().as_bytes());
            sb.add(b",\"timeDateStamp\":"); // mcp_bridge.c:6570
            let mut hexbuf = [0u8; 9];
            let v = pm.time_stamp;
            for (h, byte) in hexbuf.iter_mut().enumerate().take(8) {
                let nib = (v >> (28 - 4 * h)) & 15; // mcp_bridge.c:6571
                *byte = if nib < 10 { b'0' + nib as u8 } else { b'a' + nib as u8 - 10 };
            }
            hexbuf[8] = 0;
            sb.addjson(&hexbuf[..8]); // mcp_bridge.c:6572: sb_addjson, the quoted string
            sb.add(b"}"); // mcp_bridge.c:6573
        }
        sb.add(b"]}");
    })
}

/// `control.version` (`mcp_bridge.c:6559-6578`): protocol, the bridge and game builds, and
/// every pin module with its state and `TimeDateStamp` as 8 lowercase hex digits.
pub fn version(id: Option<&str>, modules: &[ModuleView]) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":"); // mcp_bridge.c:6560
        sb.add(version_object(modules).as_bytes());
        sb.add(b"}\n");
    })
}

/// The `control.ping` result object, without the envelope (`mcp_bridge.c:6499-6528`).
pub fn ping_object(input: &PingInput<'_>) -> String {
    build_reply(|sb| {
        sb.add(b"{\"ok\":true,\"uptimeMs\":"); // mcp_bridge.c:6500-6501
        sb.addnum(input.now.wrapping_sub(input.start_tick));
        sb.add(b",\"mainThreadAgeMs\":"); // mcp_bridge.c:6502
        // One read per volatile + saturating difference: slot 13 can store a tick newer than
        // `now` between the two reads, and the unsigned difference then wrapped to 2^64-1
        // ("blocked" to every client) (mcp_bridge.c:6505-6508).
        let last = input.last_tick;
        if last != 0 && input.now > last {
            sb.addnum(input.now - last);
        } else {
            sb.addnum(0);
        }
        sb.add(b",\"drainBlocked\":"); // mcp_bridge.c:6509
        let inflight = input.inflight_start;
        if inflight != 0 && input.now > inflight && input.now - inflight > 250 {
            sb.add(b"true");
        } else {
            sb.add(b"false");
        }
        sb.add(b",\"inflight\":"); // mcp_bridge.c:6513-6514
        sb.addnum(u64::from(inflight != 0));
        sb.add(b",\"queueDepth\":"); // mcp_bridge.c:6515
        sb.addnum(input.queue_depth);
        sb.add(b",\"modules\":"); // mcp_bridge.c:6517
        sb.add(b"{");
        assert_eq!(
            input.modules.len(),
            pins::PIN_MODULE_COUNT,
            "modules must carry one view per pin module, in PIN_MODULES order"
        );
        for (i, pm) in pins::PIN_MODULES.iter().enumerate() {
            if i != 0 {
                sb.add(b",");
            }
            sb.addjson(pm.name.as_bytes());
            sb.add(b":\""); // mcp_bridge.c:6522-6523
            sb.add(input.modules[i].state.as_str().as_bytes());
            sb.add(b"\"");
        }
        sb.add(b"}");
        sb.add(b",\"revision\":"); // mcp_bridge.c:6527
        sb.addnum(input.revision);
        sb.add(b",\"shuttingDown\":"); // mcp_bridge.c:6529
        sb.add(if input.shutting_down { b"true" } else { b"false" });
        sb.add(b"}");
    })
}

/// `control.ping` (`mcp_bridge.c:6497-6529`).
pub fn ping(id: Option<&str>, input: &PingInput<'_>) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":"); // mcp_bridge.c:6499
        sb.add(ping_object(input).as_bytes());
        sb.add(b"}\n"); // mcp_bridge.c:6530 (closes result and frame)
    })
}

/// The `control.status` result object, without the envelope (`mcp_bridge.c:6531-6552`).
pub fn status_object(input: &StatusInput<'_>) -> String {
    build_reply(|sb| {
        sb.add(b"{\"queue\":["); // mcp_bridge.c:6531
        for (qi, rq) in input.queue.iter().enumerate() {
            if qi != 0 {
                sb.add(b",");
            }
            sb.add(b"{\"id\":"); // mcp_bridge.c:6536
            sb.addjson(rq.id.as_bytes());
            sb.add(b",\"method\":"); // mcp_bridge.c:6537
            sb.addjson(rq.method.as_bytes());
            sb.add(b",\"ageMs\":"); // mcp_bridge.c:6538
            sb.addnum(input.now.wrapping_sub(rq.received_at));
            sb.add(b"}");
        }
        sb.add(b"],\"jobs\":["); // mcp_bridge.c:6542 (the C: always empty; filled from the job table)
        for (ji, (id, kind, state)) in input.jobs.iter().enumerate() {
            if ji != 0 {
                sb.add(b",");
            }
            sb.add(b"{\"id\":");
            sb.addjson(id.as_bytes());
            sb.add(b",\"kind\":");
            sb.addjson(kind.as_bytes());
            sb.add(b",\"state\":");
            sb.addjson(state.as_bytes());
            sb.add(b"}");
        }
        sb.add(b"],\"listener\":");
        sb.add(if input.listener { b"true" } else { b"false" });
        sb.add(b",\"drainBlocked\":false,\"inflight\":"); // mcp_bridge.c:6542 (a status call runs
        // on the reader thread, which is never the blocked one)
        sb.addnum(u64::from(input.inflight_start != 0));
        sb.add(b",\"revision\":"); // mcp_bridge.c:6544
        sb.addnum(input.revision);
        sb.add(b",\"shuttingDown\":"); // mcp_bridge.c:6546
        sb.add(if input.shutting_down { b"true" } else { b"false" });
        sb.add(b"}");
    })
}

/// `control.status` (`mcp_bridge.c:6530-6552`).
pub fn status(id: Option<&str>, input: &StatusInput<'_>) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":"); // mcp_bridge.c:6530
        sb.add(status_object(input).as_bytes());
        sb.add(b"}\n"); // mcp_bridge.c:6548
    })
}

/// `build_error` (`mcp_bridge.c:1563-1570`): the plain JSON-RPC error object with the
/// `bridge` member the C's protocol adds.
pub fn error(id: Option<&str>, code: i64, message: &str) -> String {
    build_reply(|sb| {
        prefix(sb, id);
        sb.add(b",\"error\":{\"code\":"); // mcp_bridge.c:1566
        sb.addi(code);
        sb.add(b",\"message\":"); // mcp_bridge.c:1568
        sb.addjson(message.as_bytes());
        sb.add(b"}}\n"); // mcp_bridge.c:1569
    })
}

/// `build_error_ext` (`mcp_bridge.c:1572-1583`): the error object with a pre-serialized
/// `data` member (the C's `dataJson`).
pub fn error_ext(id: Option<&str>, code: i64, message: &str, data: &str) -> String {
    build_reply(|sb| {
        prefix(sb, id);
        sb.add(b",\"error\":{\"code\":"); // mcp_bridge.c:1575
        sb.addi(code);
        sb.add(b",\"message\":"); // mcp_bridge.c:1577
        sb.addjson(message.as_bytes());
        sb.add(b",\"data\":"); // mcp_bridge.c:1579
        sb.add(data.as_bytes());
        sb.add(b"}}\n"); // mcp_bridge.c:1580
    })
}

/// A result object with the one member the C writes verbatim - the shape of the
/// `control.shutdown` (`{"closing":true}`, `mcp_bridge.c:6593`) and the cancel-found
/// (`{"cancelled":true}`, `:6585`) replies. The queue/socket behaviour around them is NOT
/// here (see the module docs).
pub fn result_ok(id: Option<&str>, result_json: &str) -> String {
    build_reply(|sb| {
        prefix_always_id(sb, id);
        sb.add(b",\"result\":"); // mcp_bridge.c:6585/6593
        sb.add(result_json.as_bytes());
        sb.add(b"}\n");
    })
}

/// `control.cancel`'s result when a queued request was removed (`mcp_bridge.c:6585`).
pub const CANCELLED: &str = r#"{"cancelled":true}"#;

/// `control.cancel`'s `data` when nothing was removed (`mcp_bridge.c:6587`).
pub const NOT_CANCELLABLE_DATA: &str = r#"{"reason":"not queued or already running"}"#;

/// `control.shutdown`'s result (`mcp_bridge.c:6592-6593`).
pub const CLOSING: &str = r#"{"closing":true}"#;

/// `-32601 MethodNotFound` - an unknown method name, `control.*` or not
/// (`mcp_bridge.c:6601-6605`, `:6607-6610`, `exec_request:5662`).
pub fn not_found(id: Option<&str>) -> String {
    error(id, -32601, "MethodNotFound")
}

/// The `{"blockedOn":...}` data object of `-32011 NotImplemented` (`mcp_bridge.c:6613`,
/// `:5672`), built in the C's 192-byte scratch buffer. The IPC lane answers a blocked request
/// immediately and must produce the same bytes [`not_implemented`] puts in a full frame.
pub fn blocked_on_data(blocked_on: &str) -> String {
    data_json(|sb| {
        sb.add(b"{\"blockedOn\":"); // mcp_bridge.c:6613
        sb.addjson(blocked_on.as_bytes());
        sb.add(b"}");
    })
}

/// `-32011 NotImplemented` with the `{"blockedOn":...}` data object
/// (`mcp_bridge.c:6611-6616`, `exec_request:5668-5674`). `blocked_on` is the row's string,
/// the C's `"unknown"` fallback already applied ([`crate::registry::gate`]).
pub fn not_implemented(id: Option<&str>, blocked_on: &str) -> String {
    error_ext(id, -32011, "NotImplemented", &blocked_on_data(blocked_on))
}

/// `-32017 PrerequisiteFailed` for a row that needs a KHook `IKHook` and has none
/// ([`crate::registry::NEED_KHOOK`]).
///
/// The data object is the plain `{"blockedOn":...}` shape with
/// [`crate::registry::KHOOK_BLOCKED_ON`], not the module shape: KHook is not a pinned module, so
/// there is no module name or identity state to report - what is missing is the `IKHook` that
/// Metamod hands a plugin at `Load`. Built in the C's 192-byte scratch buffer, like
/// [`blocked_on_data`].
pub fn khook_missing(id: Option<&str>) -> String {
    error_ext(
        id,
        -32017,
        "PrerequisiteFailed",
        &blocked_on_data(crate::registry::KHOOK_BLOCKED_ON),
    )
}

/// `-32017 PrerequisiteFailed` with the `{"module":...,"check":"identity","state":...}` data
/// object (`exec_request:5693-5698`). `module` and `state` are the FIRST unmet need and its
/// current state: [`crate::registry::gate`] picks the module, the caller reads its
/// [`ModuleView`] state.
pub fn prerequisite_failed(id: Option<&str>, module: &str, state: ModState) -> String {
    let data = data_json(|sb| {
        sb.add(b"{\"module\":"); // exec_request, mcp_bridge.c:5694
        sb.addjson(module.as_bytes());
        sb.add(b",\"check\":\"identity\",\"state\":"); // mcp_bridge.c:5695
        sb.addjson(state.as_str().as_bytes());
        sb.add(b"}");
    });
    error_ext(id, -32017, "PrerequisiteFailed", &data)
}
