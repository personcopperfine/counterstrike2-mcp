//! `cs2-bridge`: the Rust port of the Windows C bridge (`bridge/mcp_bridge.c`), built as a
//! cdylib for two targets - the workshop tools under Wine (`x86_64-pc-windows-msvc`) and the
//! Linux dedicated server (`x86_64-unknown-linux-gnu`) - and as an rlib so the host tests can
//! link against it.
//!
//! # What is here today
//!
//! WP1.A + the build half of WP0.4, then WP1.G:
//!
//! * one exported symbol, [`CreateInterface`], the C `mcp_bridge.c:6846` entry point;
//! * [`toolsys`], the workshop tools' tool-plugin object and its vtable: the C `M0Obj` /
//!   `build_vtable` / `m0_*` block (`mcp_bridge.c:6765-6839`), including the per-frame tick;
//! * [`json_text`], the JSON string-body unescaper for `pet.apply`: the Rust port of
//!   `bridge/json_text.h` (escaping/writing helper, m2.13 WP-B1);
//! * [`plugin`], the Metamod:Source 2.0-dev plugin object (`CreateInterface("ISmmPlugin")`):
//!   `Load` installs Metamod's `IKHook` into [`s2sdk::hook`] and registers the listener whose
//!   `OnMetamodQuery` serves [`iface`]; `Unload` tears the hook owners down, drains the hooks
//!   and only then forgets the API ([`hooks`]);
//! * [`khook_api`], the `KHookApi` implementation over Metamod's generated `IKHook_vtable`;
//! * [`gameframe`], the tick for hosts without the tools framework: KHook on
//!   `ISource2Server::GameFrame` -> `wiring::tick` (the Metamod-only Linux/Wine images);
//! * [`iface`], `McpBridge_001`: a provisional status object (`metamod`, `khook`, `tick`,
//!   `roles` - see [`iface::status_json`]);
//! * [`log`], the line-at-a-time bridge log.
//! * [`kv3_check`], the pre-load gate for KV3 text (the C `bridge/kv3_check.h`) and pet.open's
//!   host-path derivation.
//! * [`queue`], the bounded main-thread request queue, the bounded outbound queue, the deadline
//!   sweep and the per-frame `drain` (the C `Req`/`Out`/`q_*`/`out_*` block,
//!   `mcp_bridge.c:1406-1530`, `:1587-1621`, `:6289-6335`). The executor is a parameter, so
//!   nothing calls it yet: the IPC lane pushes and the tick drains.
//! * [`modules`] + [`pe`], the fail-closed module identity gate (the C's `verify_one` block):
//!   a module is only used after its PE facts, RSDS GUID/age, file SHA-256, byte pins and
//!   vtable-slot pins match the generated pins, in that order, and the image is then pinned.
//! * [`registry`], the method table and the verification gate: the C's `k_methods[]`,
//!   `NEED_*` bits, `find_method`, `verified_mask` and the dispatch gate
//!   (`mcp_bridge.c:1030-1176`, `exec_request:5657-5698`).
//! * [`control`], the pure `control.*` reply builders: `build_caps`, `build_error` /
//!   `build_error_ext`, the ping/status/capabilities/version/hello replies and the method-gate
//!   error objects (`mcp_bridge.c:1190-1220`, `:1563-1583`, `:6478-6623`).
//! * [`ipc`], the TCP listener, the NDJSON reader, the `control.hello` handshake, the writer
//!   thread, the session token and the discovery file (the C `ipc_thread`/`writer_thread`/
//!   `write_discovery`/`start_listener_once`, `mcp_bridge.c:1531-1562`, `:6336-6779`).
//! * [`dispatch`], the request gate: the C's `exec_request` head and IPC-lane chain over the
//!   method table, plus the Rust-only `ported` list.
//! * [`wiring`], the integration (WP2.D, bridge build m3.0): the tick starts the listener and
//!   drains the queue, the listener's gate routes unknown/blocked methods, and a
//!   [`ipc::ControlHandler`] answers `control.*` from live state. Every op row still answers
//!   `-32011 NotImplemented` with `blockedOn` - no method body is ported yet (M4) - and
//!   `control.capabilities` advertises only the served rows.
//! * [`ops`], the method bodies (M4), one module per group, registered in one table; a name
//!   with a body is served, every other op row keeps answering `-32011`.
//!
//! Both targets serve `ISmmPlugin` and `McpBridge_001`; `ToolSystem2_001` is Windows only (the
//! tools framework's plugin, `cs2.exe -tools`).
//!
//! # Numbers
//!
//! Vtable slots, vtable lengths, ABI constants and struct layout all come from the generated
//! `s2sdk-sys` bindings, per the repo rule. Nothing in this crate is a hand-written address,
//! slot, offset or size.
//!
//! # Panics
//!
//! Every function the game can reach - [`CreateInterface`] and every vtable entry - runs its
//! body inside [`std::panic::catch_unwind`] ([`guard`]) and answers with the C default if the
//! body panicked, so a Rust panic can never unwind into the engine. The crate is therefore built
//! with the workspace's default `panic = "unwind"` (a `panic = "abort"` profile would also do,
//! but the profile table is workspace-wide and this WP may only touch the member).
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr::null_mut;

pub mod control;
pub mod dispatch;
pub mod entity_json;
pub mod gameframe;
pub mod hooks;
pub mod iface;
pub mod ipc;
pub mod json_text;
pub mod khook_api;
pub mod kv3_check;
pub mod log;
pub mod map_path;
pub mod modules;
pub mod ops;
pub mod pe;
pub mod plugin;
pub mod queue;
pub mod registry;
pub mod toolsys;
pub mod vac;
pub mod wiring;

/// A deferring `KHookApi` mock for the library's unload tests (see [`khook_mock`]).
#[cfg(test)]
mod khook_mock;

/// The build string every reply envelope, the discovery file and `control.version` carry - the
/// C's `BRIDGE_BUILD` (`bridge/mcp_bridge.c:127`), one constant for the whole crate.
///
/// `m3.0` is the first Rust build that answers the wire (WP2.D): it serves the `control.*`
/// methods and refuses every other method with `-32011` + `blockedOn` until the ops are ported
/// (M4). The C bridge stays at `m2.20`; hosts tell the two apart by this string (there is no
/// ordering comparison anywhere in the host crates - see `bridge/protocol-diff-m3.0.md`).
/// Each merged M4 method group bumps the minor: `m3.1` adds the game group
/// (`bridge/protocol-diff-m3.1.md`), `m3.2` the Hammer core + read ops (`protocol-diff-m3.2.md`), `m3.3` pet without
/// render (`protocol-diff-m3.3.md`), `m3.4` ModelDoc without render
/// (`protocol-diff-m3.4.md`), `m3.5` the Hammer document writes (`protocol-diff-m3.5.md`),
/// `m3.6` the map lifecycle (`protocol-diff-m3.6.md`),
/// `m3.7` the mesh writes (`protocol-diff-m3.7.md`),
/// `m3.8` the renders (`protocol-diff-m3.8.md`),
/// `m3.9` the hooks: game.saveRestore, input.*, sound.* (`protocol-diff-m3.9.md`).
/// `m3.10` edit.batch, entity.tieMeshes, entity.untie: every C row served (`protocol-diff-m3.10.md`).
/// `m3.11` KHook + Metamod everywhere: the hooks on KHook, the GameFrame tick, `metamod.plugins`,
/// conflict counters, module selection by full path (`protocol-diff-m3.11.md`).
/// `m3.12` the last blocked rows: job.* (resourcecompiler jobs), mesh.get, selection.*, the
/// `RUST_OVERRIDES` table (`protocol-diff-m3.12.md`).
/// `m3.13` adds `console.exec` in `-tools` sessions and `cvar.get` (`protocol-diff-m3.13.md`).
/// `m3.14` adds `mesh.setFaceTexture` (`protocol-diff-m3.14.md`).
/// `m3.15` adds no method: `input.set` with `release` and `tap` of the same held button now re-presses
/// it (`protocol-diff-m3.15.md`).
/// `m3.16` adds no method: `job.start` accepts the map-stage flags `-world -phys -vis -bakelighting`
/// (`protocol-diff-m3.16.md`).
/// `m3.17` adds no method: `job.start` accepts a `.js` input (a cs_script source; the compiler writes the
/// `.vjs_c`) (`protocol-diff-m3.17.md`).
/// `m3.18` adds `node.createPrefab` and `node.prefabFromSelection`, and `mesh.setFaceTexture` gains
/// `mode:"axes"` (absolute per-face texture axes) (`protocol-diff-m3.18.md`).
pub const BRIDGE_BUILD: &str = "m3.18";

/// The generated pins (`bridge/pins.py` -> `src/generated/pins.rs`): the module identities,
/// vtable RVAs, slots and prologue bytes the coming IPC listener gates its callers with.
///
/// Generated code is exempted narrowly: `missing_docs` (the generator's `///` comments are
/// the docs), and `dead_code` (this build does not read every table yet - the tables are the
/// data the ITPORT half ports; they must not gate this crate's build until then).
#[allow(dead_code, missing_docs)]
pub mod generated {
    pub mod pins;
}

/// The interface version string the tools framework asks for: `bridge/s2sdk.h:55`
/// (`S2_TOOLSYSTEM2_VERSION`), which has no upstream macro.
const TOOLSYSTEM2_VERSION: &core::ffi::CStr = c"ToolSystem2_001";

/// Runs `body`, returning `default` if it panicked.
///
/// A panic must never cross back into the game (the C bridge has no unwinding at all, and the
/// engine is not compiled to expect it), so every reachable function's body goes through here.
/// The one line written on the panic path uses [`log::line`], which cannot panic itself.
pub(crate) fn guard<R>(default: R, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(_) => {
            log::line("panic caught; answering with the C default");
            default
        }
    }
}

/// The module's exported entry point - the only symbol this cdylib exports
/// (`bridge/mcp_bridge.c:6846`).
///
/// Three names are served:
///
/// * `"ISmmPlugin"` (the generated [`s2sdk_sys::generated::METAMOD_PLAPI_NAME`]): the
///   Metamod:Source plugin object ([`plugin`]). On **both** targets - Metamod runs inside the
///   Linux dedicated server too.
/// * `"ToolSystem2_001"`: the workshop tools' tool-plugin object ([`toolsys`]), **Windows only**
///   - that is the target `cs2.exe -tools` runs on, and the object is what WP0.4 proved live.
/// * `"McpBridge_001"` ([`iface::IFACE_NAME`]): the bridge's own provisional status object.
///
/// `*ret` follows Metamod's `META_IFACE_OK`/`META_IFACE_FAILED` (`core/ISmmPlugin.h:54-55`,
/// which are HL2SDK's `IFACE_OK`/`IFACE_FAILED`): 0 for a returned object, 1 for "no such
/// interface". `ret` may be null - Metamod itself passes `NULL` when it only wants the object
/// (`core/metamod_plugins.cpp:571`) - and a null `name` is "no match" rather than a
/// dereference.
///
/// Serving `"ISmmPlugin"` also adds the `mm` role ([`log::add_role`]) before `Load` is ever
/// called, so that a refused `Load` and the lines around it carry the role tag; see
/// [`log::Role`] for why the roles are a set per image rather than a first-wins name.
///
/// # Safety
///
/// `name` must be null or a valid NUL-terminated string that outlives the call; `ret` must be
/// null or point at a writable `int` that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn CreateInterface(name: *const c_char, ret: *mut c_int) -> *mut c_void {
    guard(null_mut(), || {
        // SAFETY: the caller guarantees `name` is null or a valid C string.
        let requested = (!name.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(name) });

        let object = match requested {
            Some(name) if name.to_bytes() == s2sdk_sys::generated::METAMOD_PLAPI_NAME.as_bytes() => {
                // The Metamod entry point adds the `mm` role as soon as it is asked for the
                // plugin object - before `Load`, so a refused `Load`'s own lines ("Load got a
                // null ISmmAPI*", "already loaded") carry the tag too and are traceable to the
                // Metamod entry point. The same image may also serve the tools framework, and
                // then the tag is `mm+tool` (`log::Role`).
                log::add_role(log::Role::Metamod);
                log::line("CreateInterface -> ISmmPlugin");
                Some(plugin::object())
            }
            Some(name) if name == TOOLSYSTEM2_VERSION && cfg!(windows) => {
                log::line("CreateInterface -> object");
                Some(toolsys::object())
            }
            Some(name) if name == iface::IFACE_NAME => {
                log::line("CreateInterface -> McpBridge_001");
                Some(iface::object())
            }
            _ => None,
        };

        if !ret.is_null() {
            // SAFETY: the caller guarantees `ret` is null or writable.
            unsafe {
                *ret = if object.is_some() {
                    plugin::META_IFACE_OK
                } else {
                    plugin::META_IFACE_FAILED
                }
            };
        }
        object.unwrap_or(null_mut())
    })
}

#[cfg(test)]
mod tests {
    use super::guard;

    /// A panicking body must answer with the C default instead of unwinding into the caller -
    /// the caller here is a test, but in the game it is an engine frame with no landing pad.
    #[test]
    fn guard_answers_the_default_when_the_body_panics() {
        let previous = std::panic::take_hook();
        // Keep the expected panic's message out of the test output; the hook is restored below.
        std::panic::set_hook(Box::new(|_| {}));
        let value = guard(7, || panic!("the body under test panics"));
        std::panic::set_hook(previous);
        assert_eq!(value, 7);

        // And a body that does not panic has its value passed through untouched.
        assert_eq!(guard(7, || 9), 9);
    }
}
