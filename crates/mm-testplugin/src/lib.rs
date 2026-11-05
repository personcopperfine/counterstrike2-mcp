//! The coexistence fixture: a second Metamod plugin, one virtual KHook, three modes.
//!
//! The bridge (`crates/cs2-bridge`) is one Metamod plugin among many and must coexist: KHook
//! runs the `pre`/`post` callbacks of *every* plugin hooked on a target, and passive hooks of
//! different plugins all fire. This crate is the live fixture that proves it - the
//! orchestrator's live coexistence test (lane D2, `test_metamod_coexist.py`, since deleted:
//! `git show 8cdea42:bridge/test_metamod_coexist.py`) drove it against the bridge -
//! and the seed of a "write a gamemode plugin in Rust" template (see
//! [the template section](#a-template-for-write-a-gamemode-plugin-in-rust)).
//!
//! # What Metamod sees
//!
//! Exactly like the bridge's plugin ([`plugin`](crate::plugin) follows the same
//! `CreateInterface(METAMOD_PLAPI_NAME)` -> `Load` -> `Unload` lifecycle the bridge's
//! `crates/cs2-bridge/src/plugin.rs` documents; the long Metamod-semantics essays live there
//! and are not repeated). `Load`:
//!
//! 1. reads the mode from `CS2MCP_TESTPLUGIN_MODE` (an unknown value refuses the load);
//! 2. takes the `IKHook*` from `ISmmAPI::GetDetourInterface(id)` and installs the
//!    [`s2sdk::khook_vtable`] adapter - the same wiring the bridge's `Load` runs;
//! 3. finds the server module (`s2sdk::module`), asks it for
//!    `Source2ServerConfig001`, and hooks the **object's own vptr** at the generated
//!    `ISource2ServerConfig_slots::AllowSaveRestore` slot: that is the table the game calls
//!    through (the bridge pins the same table on Windows), and KHook patches it in place.
//!
//! One virtual KHook, `pre` + `post`, [`SetupMode::Sync`] - the target is not on any stack
//! during `Load`. `Unload` removes it with [`Hook::remove_sync`] (never from inside a
//! callback: a synchronous removal of a target on this stack would spin on the hooked
//! address's mutex; one asked from inside a thunk anyway falls back to the asynchronous
//! path, [`Removal::Deferred`]), drains with [`s2sdk::hook::remove_all_blocking`] - which is
//! what waits for a deferred removal - and uninstalls the adapter.
//!
//! # The three modes (`CS2MCP_TESTPLUGIN_MODE`, read at `Load`)
//!
//! * `passive` (default): the `pre` answers [`Action::Ignore`] and counts calls - the hook
//!   changes nothing, so on a B2+ bridge (see the coexistence section below) alongside the
//!   bridge's own AllowSaveRestore KHook both plugins' hooks fire and the value stays the
//!   bridge's.
//! * `supersede0`: the `pre` answers `Supersede(0)` - a value changer that **disagrees**
//!   with the bridge's 1; KHook folds two value changers on one call by the higher action
//!   and the first saved value wins (`khook.hpp:744-752`, `detour.cpp:381-383`), so this
//!   is the conflict the bridge counts.
//! * `supersede1`: `Supersede(1)` - agrees with the bridge.
//!
//! The `post` callback counts calls and records the final value
//! ([`HookCall::current_value_ptr`], the override when one was stored, the original's
//! otherwise) - the evidence a live test reads, alongside the log.
//!
//! # Logging (`CS2MCP_TESTPLUGIN_LOG`)
//!
//! One line per event (`crate::log`): `Load(mode=<m>)`, `hooked AllowSaveRestore id=<n>`,
//! `Unload: removed (<Removal>)`, and per call (every call; save/restore checks are rare)
//! `AllowSaveRestore pre#<n> post#<n> final=<v>`.
//!
//! Reading `final=<v>`: it is the folded value **as of this post callback** - the fold of
//! every hook that ran before this plugin's `post` on that one call. A plugin whose `post`
//! runs after this one may still override the value for whoever reads it later, so a
//! coexistence test compares the same call's lines across the plugins' logs, never a later
//! line against an earlier one.
//!
//! # Coexistence with the bridge - and when this proves it
//!
//! Against today's (pre-B2) bridge this fixture is **not runnable at all**: the bridge's
//! `game.saveRestore` still swaps the config object's vptr to a copied table
//! (`crates/cs2-bridge/src/ops/game/save_restore.rs:14-17`), and that file's own rule is
//! *never KHook this object* on a pre-B2 bridge - the bridge's identity check
//! (`crates/cs2-bridge/src/ops/ident.rs`) re-reads every pinned slot of the pinned vtable
//! (`GSV_GAMECONFIG_SERVER_VT`), and a fixture KHook in place patches that shared table
//! underneath it, so `obj_is` fails and `game.saveRestore` answers `-32017
//! PrerequisiteFailed` ("Source2ServerConfig001 is not the pinned CCSGameConfiguration
//! (+8 view)", `save_restore.rs:70-76`) for as long as the fixture's hook exists. Until
//! lane B2 moves `game.saveRestore` onto KHook, the fixture must not be vdf-installed
//! next to a pre-B2 bridge.
//!
//! The coexistence evidence the fixture exists for - both plugins' hooks firing on one
//! `AllowSaveRestore` call, and the fold when both change the value - is KHook-vs-KHook
//! only against a bridge that hooks KHook too: **lane B2**. The D2 coexistence test
//! (`test_metamod_coexist.py`, see above) therefore ran its cases only on a B2-or-later
//! bridge.
//!
//! # A template for "write a gamemode plugin in Rust"
//!
//! Everything this fixture does is what a real gamemode plugin gets, minus the fixture's
//! modes:
//!
//! * a Metamod plugin object - `CreateInterface(METAMOD_PLAPI_NAME)`, and Metamod's
//!   `Load`/`Unload` lifecycle is the only registration path;
//! * the KHook adapter ([`s2sdk::khook_vtable::install`]) and the safe hook layer
//!   ([`s2sdk::hook`]: `khook_thunks!` declares a signature, `setup_virtual` hooks a slot,
//!   the [`Hook`] owns it);
//! * the generated SDK bindings - every slot, interface version and vtable length comes from
//!   `s2sdk_sys::generated`, never a hand-written number.
//!
//! TODO(lane R): once the generated `ISource2Server` bindings land, a gamemode plugin's
//! per-frame work is one more `khook_thunks!` declaration for `GameFrame`, hooked at
//! `ISource2Server_slots`' generated slot the same way [`allow_save_restore`] is below. No
//! code until those bindings exist.
//!
//! [`Action::Ignore`]: s2sdk::hook::Action::Ignore
//! [`Hook::remove_sync`]: s2sdk::hook::Hook::remove_sync
//! [`HookCall::current_value_ptr`]: s2sdk::hook::HookCall::current_value_ptr
//! [`Removal::Deferred`]: s2sdk::hook::Removal::Deferred
//! [`SetupMode::Sync`]: s2sdk::hook::SetupMode::Sync

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

pub mod log;
pub mod plugin;

/// Runs `body`, returning `default` if it panicked.
///
/// A panic must never cross back into the engine or Metamod (the same rule as the bridge's
/// `guard`, `crates/cs2-bridge/src/lib.rs`): every function the game can reach -
/// [`CreateInterface`] and every vtable entry - runs its body inside
/// [`std::panic::catch_unwind`] and answers with the C default if the body panicked.
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
/// (`cargo bridge check-exports`, run by `cargo bridge testplugin`, gates exactly that).
///
/// One name is served: `"ISmmPlugin"` (the generated [`METAMOD_PLAPI_NAME`]), the coexistence
/// fixture's Metamod plugin object ([`plugin`]). `*ret` follows Metamod's
/// `META_IFACE_OK`/`META_IFACE_FAILED` (`core/ISmmPlugin.h:54-55`); `ret` may be null
/// (Metamod itself passes `NULL` when it only wants the object) and a null `name` is "no
/// match" rather than a dereference.
///
/// [`METAMOD_PLAPI_NAME`]: s2sdk_sys::generated::METAMOD_PLAPI_NAME
///
/// # Safety
///
/// `name` must be null or a valid NUL-terminated string that outlives the call; `ret` must be
/// null or point at a writable `int` that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn CreateInterface(name: *const c_char, ret: *mut c_int) -> *mut c_void {
    guard(ptr::null_mut(), || {
        // SAFETY: the caller guarantees `name` is null or a valid C string.
        let requested = (!name.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(name) });

        let object = match requested {
            Some(name)
                if name.to_bytes() == s2sdk_sys::generated::METAMOD_PLAPI_NAME.as_bytes() =>
            {
                log::line("CreateInterface -> ISmmPlugin");
                Some(plugin::object())
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
        object.unwrap_or(ptr::null_mut())
    })
}
