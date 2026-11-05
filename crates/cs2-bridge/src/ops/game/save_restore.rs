//! `game.saveRestore` (the `op_test` arm, `bridge/mcp_bridge.c:5055-5061`) and its
//! `ISource2ServerConfig::AllowSaveRestore` hook.
//!
//! Mechanism: `AllowSaveRestore` (the generated slot
//! [`sys::ISource2ServerConfig_slots::AllowSaveRestore`]) returns false in CS2, and engine2's
//! save **and** load both test it before doing anything. The bridge makes it answer true while
//! the user has the feature armed, by hooking that one virtual on the class's vtable with KHook
//! (`SetupVirtualHook`) and superseding the call for the config object it recorded:
//!
//! * the table is the pinned `GSV_GAMECONFIG_SERVER_VT`
//!   (`modules_base(PIN_SERVER) + pins::GSV_GAMECONFIG_SERVER_VT.value`), the same pointer the
//!   old `VtSwap` implementation copied;
//! * the index is the generated slot, cast to `i32` - never a hand-written number;
//! * the install is [`SetupMode::Sync`]: `sr_set(true)` runs on the main thread from the drain
//!   and `AllowSaveRestore` is certainly not on that stack, so KHook may insert the hook inside
//!   the call;
//! * the thunks are declared for `bool AllowSaveRestore()` as `-> u8`, never a Rust `bool`: the
//!   game reads the low byte of the return register, and a `bool` would be a Rust-validity
//!   assumption across an ABI this crate does not control (a `bool` read from the game is
//!   forbidden in this crate, in either direction).
//!
//! # Why KHook, and not the old per-object vptr swap
//!
//! The old implementation copied the class's vtable into a private array, pointed **this one
//! object's** vptr at the copy, and restored the original vptr on disable. That was safe only
//! while nothing else touched the class's table, and CS2 has other plugins. A per-object copy
//! *bypasses* a virtual another plugin hooked: that plugin's callback is not in our copy, so it
//! silently stops running for this object. It also *freezes* whatever was in the table when we
//! copied it - a hook another plugin installs afterwards never reaches this object either. And
//! the bridge's own identity gate (`ops/ident.rs`) re-reads pinned slots of the shared table,
//! which our copy left untouched and therefore "verified" while the object ran different code.
//!
//! KHook patches the class's **shared** table entry, once, in place, and never restores it
//! (`third_party/khook/src/detour.hpp:96-103`). Every object of the class, and every plugin
//! that hooks the slot later, sees the same entry, and the bridge's identity check asks KHook
//! for the saved original instead of comparing the word (`modules::slot_matches`) - so a
//! KHook'd slot still identifies the object. Our callback answers `Ignore` for every `this`
//! that is not the config object we recorded, so other objects of the class are unaffected.
//!
//! # Coexistence rule
//!
//! We may **Supersede** (return 1 and skip the original) but we are passive by default: the
//! callback answers `Ignore` unless the feature is armed (the hook exists) and `this` is the
//! recorded object. When another value changer meets us on one call, the bridge reports a
//! **conflict** rather than picking a winner silently:
//!
//! * *ahead of us*: our `pre` runs after an earlier callback already saved an override for this
//!   call (`GetOverrideValuePtr()` non-null, `khook.hpp:744-752` folds by max and
//!   `detour.cpp:381-383` keeps the FIRST value among equal actions); the same call is not
//!   counted a second time by the `post` check.
//! * *after us*: our `post` reads the final override value and it is not ours. With our action
//!   at the top (`Supersede`), nothing later can outrank us, so this only fires when an **earlier
//!   equal-action** callback saved its value first and won the fold (`detour.cpp:381-383`),
//!   which is the *ahead* case under the same rule. What this side can never see is a later
//!   changer that *loses* to us: a callback inserted after our `post` with an equal or lower
//!   action saves nothing, so its attempt at a different value is invisible here. The conflict
//!   report is therefore "another value changer met ours on this call", not "every conflicting
//!   callback was counted"; a later changer with a strictly higher action would move the final
//!   value and is the one case this pair does catch.
//!
//! A conflict is a rate-limited log line (`khook conflict: ISource2ServerConfig::AllowSaveRestore
//! <ahead|after>`, once per arm), a counter in the reply (`{"enabled":..,"conflicts":N}`) and,
//! under `cfg(debug_assertions)` only, a `debug_assert!` that is **caught**
//! (`catch_unwind`): a conflict must never kill the game, and a debug build still has to run this
//! test. In a release build the assert is not compiled at all, so a conflict is only the line
//! and the counter there.
//!
//! # Company the callbacks keep
//!
//! They run inside a KHook thunk: no allocation-heavy work, no lock held across a call into the
//! game, and never a hook removal (a synchronous removal from a thunk would spin on the capsule
//! mutex its own call holds shared, `detour.cpp:1503,1581-1589`). The callbacks only read two
//! atomics and a thread-local flag, and write the counter and the log.
//!
//! # Arming
//!
//! Install on enable, [`Hook::remove_sync`] on disable: "armed" is "the hook exists". The
//! `Ignore` branch only covers a foreign `object`/`this` (the slot is shared, so any object of
//! the class can reach our thunk).

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use s2sdk::hook::{Error, Hook, HookCall, Removal, Return, SetupMode};
use s2sdk::khook_thunks;
use s2sdk_sys::generated as sys;

use crate::entity_json::Sb;
use crate::generated::pins;
use crate::hooks::TeardownReport;
use crate::log;
use crate::ops::game::schema::modules_base;
use crate::ops::game::{GAME_OVERFLOW, TEST_BUF};
use crate::ops::ident::{mod_iface, obj_is};
use crate::ops::{finish, OpErr};
use crate::queue::Request;

// `bool AllowSaveRestore()` on `ISource2ServerConfig`, hooked as a `u8` return: the game reads
// the low byte of `RAX`, and a Rust `bool` across this ABI would assert its validity on a value
// another plugin may have produced. `sret_msvc`/`sret_itanium` are both `no_sret`: a scalar
// return comes back in the register on both targets, so the machine signature is the declared
// `(this) -> u8`.
khook_thunks! {
    /// The generated `ISource2ServerConfig::AllowSaveRestore` signature.
    mod allow_sr: fn(this: *mut c_void) -> u8 {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// The hook's target, as the conflict lines name it.
const TARGET: &str = "ISource2ServerConfig::AllowSaveRestore";

/// mcp_bridge.c:5011-5012: "AllowSaveRestore must be an in-table slot of the measured game
/// vftable".
const _: () = assert!(
    sys::ISource2ServerConfig_slots::AllowSaveRestore < sys::ISource2ServerConfig_slots::VTLEN
);

/// The live hook, while armed. [`Hook`] is `!Send` because it holds a raw context pointer;
/// [`SrHook`] is the main-thread-only claim that makes it storable in a static - `sr_set`, the
/// teardown and the tool `PreShutdown` all run on the main thread, and the callbacks never take
/// this lock.
static SR: Mutex<Option<SrHook>> = Mutex::new(None);

/// The hook plus the claim that only the game's main thread moves it.
struct SrHook(Hook);

// SAFETY: every access is behind [`SR`]'s mutex and comes from the main thread (install from
// the drain, removal from `sr_set(false)`/`hooks_teardown`/`PreShutdown`). A `Hook` is only
// `!Send` because its context pointer is not `Sync`-checked by the compiler; nothing in KHook's
// API is thread-affine - `RemoveHook` may be called from any thread - and the reference counts
// inside the context are atomic.
unsafe impl Send for SrHook {}

/// The config object (`this`) the hook answers for; null while not armed.
static SR_OBJ: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The arm generation: every callback the install publishes captures the value it took and
/// answers `Ignore` once it no longer matches. Bumped when a hook is armed and again as the
/// FIRST step of a disarm, so a callback KHook still holds - a disarm from inside a thunk hands
/// the removal to `Drop`, whose removal function KHook's delete thread runs later
/// (`Removal::Deferred`) - is inert from the moment the disarm starts. Without it, a re-arm over
/// such a deferred removal would let the old `pre` supersede again for the same `SR_OBJ` and
/// report a false "ahead" on a value the live registration saved.
static ARMED_GEN: AtomicU64 = AtomicU64::new(0);

/// Take the next arm generation: the value the new registration's callbacks capture. Called
/// before the registration, so the caller never publishes a callback of an earlier arm as
/// current.
fn next_generation() -> u64 {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel) + 1
}

/// Invalidate the registration that is being removed: every callback it captured answers
/// `Ignore` from here on. Called **before** the armed flag clears, so a callback that already
/// read the object is inert too.
fn retire_generation() {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel);
}

/// Whether a callback captured by an install of `generation` still acts for the current arm.
fn generation_current(generation: u64) -> bool {
    ARMED_GEN.load(Ordering::Acquire) == generation
}

/// Conflict events since the last arm: `{"enabled":..,"conflicts":N}`.
static CONFLICTS: AtomicU64 = AtomicU64::new(0);

/// Whether the rate-limited conflict line has been written since the last arm.
static CONFLICT_REPORTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set by `pre` when it found an override already saved for this call, so `post` does not
    /// count the same conflict a second time.
    static CONFLICT_AHEAD: Cell<bool> = const { Cell::new(false) };
}

/// Which side of our callback the other value changer was on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// An earlier callback in this call saved an override before our `pre` ran.
    Ahead,
    /// The call's final override value is not ours when our `post` reads it.
    After,
}

impl Side {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ahead => "ahead",
            Self::After => "after",
        }
    }
}

/// Record one conflict: count it, and write the rate-limited line once per arm.
///
/// The `debug_assert!` is debug-builds-only and is **caught**: this runs inside a KHook thunk (an
/// `extern "C"` frame), where an escaping panic aborts the process, and a debug build must still
/// be able to run the conflict test. Release builds do not compile it at all.
fn note_conflict(side: Side) {
    CONFLICTS.fetch_add(1, Ordering::Relaxed);
    if !CONFLICT_REPORTED.swap(true, Ordering::AcqRel) {
        log::line(&format!("khook conflict: {TARGET} {}", side.as_str()));
        #[cfg(debug_assertions)]
        {
            let _ = std::panic::catch_unwind(|| {
                debug_assert!(false, "khook conflict on {TARGET} ({})", side.as_str());
            });
        }
    }
}

/// `sr_enabled` (`mcp_bridge.c:5014`): the hook is installed.
#[must_use]
pub fn sr_enabled() -> bool {
    sr_lock().is_some()
}

/// The `sr_enabled`/`conflicts` pair the reply and the tests read.
#[must_use]
pub fn sr_conflicts() -> u64 {
    CONFLICTS.load(Ordering::Relaxed)
}

fn sr_lock() -> MutexGuard<'static, Option<SrHook>> {
    SR.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Our `pre` callback: supersede with 1 for the recorded object, ignore everybody else.
///
/// Runs inside the thunk: atomics and a thread-local only. A callback whose generation no longer
/// matches belongs to a registration this bridge already disarmed (its removal is deferred, see
/// [`ARMED_GEN`]): it answers `Ignore` and reports nothing, even when `SR_OBJ` names the same
/// object again after a re-arm.
fn pre(generation: u64, call: &mut HookCall, this: *mut c_void) -> Return<u8> {
    if !generation_current(generation) {
        return Return::ignore(0);
    }
    let object = SR_OBJ.load(Ordering::Acquire);
    if this.is_null() || this != object {
        // A foreign object of the class reached the shared slot: not ours to change.
        return Return::ignore(0);
    }
    if !call.override_value_ptr().is_null() {
        // An earlier callback in this call already saved a value (`detour.cpp:381-383` keeps the
        // first among equal actions, so ours may not be the one used).
        note_conflict(Side::Ahead);
        CONFLICT_AHEAD.with(|ahead| ahead.set(true));
    }
    Return::supersede_with(1)
}

/// Our `post` callback, installed only while armed: check that the call's value is still ours.
///
/// A stale registration (the generation no longer matches) answers `Ignore` without consuming the
/// thread's "ahead" note: KHook keeps a removed-then-readded slot's old callbacks, and a stale
/// `post` must not swallow the live registration's note (`detour.cpp:1477-1489`).
fn post(generation: u64, call: &mut HookCall, this: *mut c_void) -> Return<u8> {
    if !generation_current(generation) {
        return Return::ignore(0);
    }
    let ahead = CONFLICT_AHEAD.with(|ahead| ahead.replace(false));
    let object = SR_OBJ.load(Ordering::Acquire);
    if !ahead && !this.is_null() && this == object {
        let value = call.override_value_ptr();
        if !value.is_null() {
            // SAFETY: KHook's override slot holds the return value for the signature this hook
            // was registered with - `u8` here - and points into its per-call storage, valid for
            // the duration of the callback (`khook.hpp:744-752`, `detour.cpp:373-405`).
            let saved = unsafe { value.cast::<u8>().read() };
            if saved != 1 {
                note_conflict(Side::After);
            }
        }
    }
    Return::ignore(0)
}

/// Arm the hook on the pinned table for `obj` (`sr_set`'s enable path after the identity check).
///
/// Kept separate from `sr_set` so the update path's resolution and identity check stay exactly
/// the C's, and so the tests can arm against a fabricated object without a loaded server.dll.
fn sr_arm(obj: *mut c_void) -> Result<(), OpErr> {
    let mut slot = sr_lock();
    if slot.is_some() {
        return Ok(());
    }
    // The same pointer the old vptr swap copied: the pinned table inside server.dll.
    let vt = (modules_base(pins::PIN_SERVER) + pins::GSV_GAMECONFIG_SERVER_VT.value)
        as *mut *mut c_void;
    let index = sys::ISource2ServerConfig_slots::AllowSaveRestore as i32;

    SR_OBJ.store(obj, Ordering::Release);
    CONFLICTS.store(0, Ordering::Relaxed);
    CONFLICT_REPORTED.store(false, Ordering::Release);
    // The new registration's callbacks carry this generation: a pair KHook still holds from an
    // earlier arm (a deferred removal) is inert from here on.
    let generation = next_generation();

    let pre_cbs: Vec<allow_sr::PreCb> = vec![Arc::new(move |call: &mut HookCall, this: *mut c_void| {
        pre(generation, call, this)
    })];
    let post_cbs: Vec<allow_sr::PostCb> =
        vec![Arc::new(move |call: &mut HookCall, this: *mut c_void| {
            post(generation, call, this)
        })];
    // SAFETY: `vt` is the pinned `GSV_GAMECONFIG_SERVER_VT` table inside server.dll (the module
    // gate verified the module, and `sr_set` identity-checked the object this is armed for) and
    // `index` is the generated slot of `AllowSaveRestore`, so the entry has exactly the `u8
    // f(this)` signature the thunks were generated for. Installed from the main thread with
    // `SetupMode::Sync`; the target is not on this stack.
    let hook = unsafe {
        allow_sr::setup_virtual(vt, index, pre_cbs, post_cbs, None, SetupMode::Sync)
    };
    let hook = match hook {
        Ok(hook) => hook,
        Err(error) => {
            SR_OBJ.store(ptr::null_mut(), Ordering::Release);
            return Err(khook_registration_error(&error));
        }
    };
    *slot = Some(SrHook(hook));
    log::line("test: save/restore unlocked (ISource2ServerConfig::AllowSaveRestore -> 1 via KHook)");
    Ok(())
}

/// The `-32017` an enable answers for a refused registration.
///
/// The GameFrame tick drains the bridge queue inside a KHook thunk
/// ([`crate::gameframe`]): from there `s2sdk::hook` takes the hook gate with `try_lock` only (a
/// gate holder may be spinning in a synchronous removal on this very call's capsule), so a gate
/// that another thread holds at that moment is [`Error::GateBusy`]. Registering in a thunk with
/// the gate free succeeds and is the normal case. A `GateBusy` is not a permanent failure - a
/// later tick takes the gate when it is free - so the op answers `-32017` with
/// `{"blockedOn":"khook: retry next tick"}` and the caller retries.
///
/// [`Error::InThunk`] can no longer come out of a registration (that is what the `GateBusy`
/// change removed); it is mapped the same way anyway, because a thunk-time refusal will always
/// mean "retry outside the thunk". A refusal that is not about a busy gate keeps its own message.
fn khook_registration_error(error: &Error) -> OpErr {
    if matches!(error, Error::GateBusy | Error::InThunk) {
        return OpErr::with_data(-32017, "PrerequisiteFailed", |db| {
            db.add(b"{\"blockedOn\":\"khook: retry next tick\"}");
        });
    }
    OpErr::new(-32017, "PrerequisiteFailed", &format!("{TARGET}: {error}"))
}

/// Disarm; answers what happened for the teardown log line, or `None` when nothing was armed.
///
/// The generation is retired first (so a callback of this arm is inert before anything else
/// moves), then the armed flag ([`SR_OBJ`]) is cleared: a call still in flight for the object
/// answers `Ignore` from that moment, and a disable that cannot remove the hook synchronously
/// must still leave the feature off (the same order as `gameframe::teardown`).
fn sr_disarm() -> Option<String> {
    let hook = sr_lock().take()?;
    // FIRST: retire the generation. A callback KHook still holds - the removal below can be
    // deferred when this runs from a thunk - is inert from here on.
    retire_generation();
    SR_OBJ.store(ptr::null_mut(), Ordering::Release);
    let SrHook(hook) = hook;
    // A disable from the GameFrame tick runs inside a KHook thunk. A synchronous removal there
    // would spin on the capsule this very frame call holds shared, so `remove_sync` defers in
    // **every** build (A2: it hands the hook to `Drop`'s asynchronous path and answers
    // `Removal::Deferred`, with no debug assert) - take that path directly instead of calling
    // it.
    if s2sdk::hook::in_thunk() {
        drop(hook);
        return Some("removal deferred (asynchronous: inside a hook thunk)".to_string());
    }
    // The target is not on this stack (main-thread drain or `PreShutdown`), so a synchronous
    // removal is the right one: it runs the removal function before returning, which releases
    // the context KHook was holding (`s2sdk::hook::Hook::remove_sync`).
    match hook.remove_sync() {
        // Also reachable without the branch above (a callback of another plugin's hook, say):
        // the feature is off (`SR_OBJ` is null) and the removal is queued - a success.
        Ok(Removal::Deferred) => {
            Some("removal deferred (asynchronous: inside a hook thunk)".to_string())
        }
        Ok(removal) => Some(format!("removed ({})", removal_name(removal))),
        Err(error) => Some(format!("remove failed: {error}")),
    }
}

/// The [`Removal`] outcome in the log line's words.
const fn removal_name(removal: Removal) -> &'static str {
    match removal {
        Removal::Removed => "Removed",
        Removal::AlreadyGone => "AlreadyGone",
        Removal::ReconciledLost => "ReconciledLost",
        Removal::Deferred => "Deferred",
    }
}

/// `sr_set` (`mcp_bridge.c:5015-5035`). Disabling is unconditional success; enabling resolves
/// the interface, identity-checks it and installs the KHook virtual.
pub(crate) fn sr_set(enable: bool) -> Result<(), OpErr> {
    // mcp_bridge.c:5016-5019
    if !enable {
        let _ = sr_disarm();
        return Ok(());
    }
    // mcp_bridge.c:5020
    if sr_enabled() {
        return Ok(());
    }
    // mcp_bridge.c:5021-5025
    let obj = mod_iface(pins::PIN_SERVER, sys::SOURCE2SERVERCONFIG_INTERFACE_VERSION);
    if !obj_is(obj, pins::PIN_SERVER, pins::GSV_GAMECONFIG_SERVER_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "Source2ServerConfig001 is not the pinned CCSGameConfiguration (+8 view)",
        ));
    }
    sr_arm(obj)
}

/// `save/restore`'s entry in [`crate::hooks`]: disarm, and report what happened for the one log
/// line the teardown writes.
///
/// Idempotent: with nothing armed the answer is `"not armed (no-op)"` and no state changes.
pub(crate) fn teardown() -> TeardownReport {
    let detail = match sr_disarm() {
        Some(detail) => detail,
        None => "not armed (no-op)".to_string(),
    };
    TeardownReport::new("save/restore", detail)
}

/// The reply object: `mcp_bridge.c:5059`'s `{"enabled":..}` plus this build's conflict counter.
fn write_reply(sb: &mut Sb<'_>, enabled: bool, conflicts: u64) {
    sb.add(b"{\"enabled\":");
    sb.add(if enabled { b"true" } else { b"false" });
    sb.add(b",\"conflicts\":");
    sb.addnum(conflicts);
    sb.add(b"}");
}

/// `game.saveRestore` handler (the `op_test` arm's body, `bridge/mcp_bridge.c:5055-5061`).
///
/// The reply is `{"enabled":bool,"conflicts":N}`: `conflicts` is the number of calls since the
/// last arm on which another value changer met ours (see the module docs). That member is a
/// wire addition over the C's `{"enabled":..}`.
pub(crate) fn save_restore(r: &Request) -> String {
    let mut buf = vec![0u8; TEST_BUF]; // mcp_bridge.c:5998
    let mut sb = Sb::new(&mut buf);
    let outcome = (|| {
        // mcp_bridge.c:5056-5057: `int en = 1; if (params) json_bool(params, "enable", &en);`
        let mut enable = true;
        if let Some(p) = r.params.as_deref().map(str::as_bytes) {
            if let Some(en) = super::json_bool(p, b"enable") {
                enable = en;
            }
        }
        sr_set(enable)?;
        write_reply(&mut sb, sr_enabled(), sr_conflicts());
        Ok(())
    })();
    // The C's test arm has no overflow branch (`mcp_bridge.c:5996-6008`) and this reply is
    // < 60 bytes - the same `finish` use as the group's other test-arm rows (see
    // protocol-diff-m3-game.md).
    finish(&r.id, outcome, &sb, GAME_OVERFLOW)
}

#[cfg(test)]
mod tests {
    //! The hook's own tests: a mock `KHookApi` that simulates KHook's per-call value fold
    //! (`detour.cpp:746-990`) well enough to drive the generated thunks, and the three
    //! negative paths.
    //!
    //! Every mock answer cites the KHook line it stands in for. The API slot is process-wide, so
    //! the cases take [`crate::hooks::test_guard`]'s lock (shared with the teardown test in the
    //! same binary) and the mock is removed by a drop guard.

    use super::*;
    use core::cell::RefCell;
    use std::sync::atomic::AtomicU32;

    use s2sdk::hook::{Action, HookId, KHookApi, INVALID_HOOK};

    // -------------------------------------------------------------------------------------
    // The mock
    // -------------------------------------------------------------------------------------

    /// One hooked call's state, in the shape KHook keeps per call (`detour.cpp:359-472`).
    struct Frame {
        /// The highest action saved so far (`detour.cpp:383-405`); `Ignore` until one is saved.
        action: Action,
        /// The override value, in a `Box` so `GetOverrideValuePtr` has a stable address.
        override_value: Option<Box<u8>>,
        /// The original's value (`make_call_original`'s `original = true` save).
        original_value: Option<Box<u8>>,
        /// `CompleteSkipped` (`detour.cpp:889`).
        original_skipped: bool,
        /// The window `make_call_original` opens (`detour.cpp:894`).
        original_in_progress: bool,
    }

    impl Default for Frame {
        fn default() -> Self {
            Self {
                // `khook.hpp:40`: `Ignore = 0` is the fold's seed.
                action: Action::Ignore,
                override_value: None,
                original_value: None,
                original_skipped: false,
                original_in_progress: false,
            }
        }
    }

    thread_local! {
        static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
        static CONTEXTS: RefCell<Vec<*mut c_void>> = const { RefCell::new(Vec::new()) };
        static ORIGINAL: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    }

    /// What the mock saw.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        /// `SetupVirtualHook`: the table, the slot index, the async flag and the stack size.
        SetupVirtual {
            vtable: usize,
            index: i32,
            r#async: bool,
            stack_size: u32,
        },
        /// `RemoveHook(id, async)`.
        Remove {
            id: HookId,
            r#async: bool,
        },
    }

    /// A removal the case parks inside KHook: `remove_hook` signals `arrived` and waits for the
    /// release channel to close. A *synchronous* removal does this while it holds `KHOOK_GATE`
    /// (`Hook::remove_sync` takes the gate before `RemoveHook`), which is how a case holds the
    /// gate against a registration made from inside a thunk.
    struct Park {
        arrived: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    struct Mock {
        calls: Mutex<Vec<Call>>,
        /// id -> context, as KHook stores it per hook (read back by `GetContextPtr`).
        contexts: Mutex<Vec<(HookId, usize)>>,
        /// id -> the removal function, as `SetupVirtualHook` was given it.
        removed: Mutex<Vec<(HookId, usize)>>,
        /// `SetupVirtualHook` answers [`INVALID_HOOK`] when set (`khook.hpp:2229`).
        fail: AtomicBool,
        /// The override value KHook already holds when our `pre` runs: the "second value
        /// changer registered ahead of us" (`detour.cpp:381-383`: first saved wins among equals).
        seed: Mutex<Option<u8>>,
        /// The next `RemoveHook` parks in the case (see [`Park`]); taken on first use.
        park: Mutex<Option<Park>>,
        /// Model KHook's delete thread holding a removal (`detour.cpp:1630-1669`): while set,
        /// `remove_hook` records the call and keeps the context (and its callbacks) alive
        /// instead of running the removal function - the window a disarm from inside a thunk
        /// leaves behind (`Removal::Deferred`), and what a re-arm can land on top of.
        defer: AtomicBool,
        /// Removals the modelled delete thread has not run yet.
        deferred: Mutex<Vec<HookId>>,
    }

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    impl Default for Mock {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                contexts: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                seed: Mutex::new(None),
                park: Mutex::new(None),
                defer: AtomicBool::new(false),
                deferred: Mutex::new(Vec::new()),
            }
        }
    }

    impl Mock {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn context(&self) -> *mut c_void {
            let contexts = self.contexts.lock().unwrap();
            assert_eq!(contexts.len(), 1, "one hooked slot per case");
            contexts[0].1 as *mut c_void
        }

        /// The context of the `nth` (0-based) registration, for a call that reaches a
        /// registration the latest arm no longer owns.
        fn context_nth(&self, nth: usize) -> *mut c_void {
            let contexts = self.contexts.lock().unwrap();
            contexts.get(nth).expect("that registration exists").1 as *mut c_void
        }

        /// Model KHook's deferred removal: while `yes`, a `RemoveHook` only records the call and
        /// keeps the context alive (`detour.cpp:1630-1669`), so the callbacks it published stay
        /// callable after the disarm returned.
        fn defer_removals(&self, yes: bool) {
            self.defer.store(yes, Ordering::SeqCst);
        }

        /// Whether a removal is still held by the modelled delete thread.
        fn deferred_len(&self) -> usize {
            self.deferred.lock().unwrap().len()
        }

        fn removals(&self) -> Vec<(HookId, bool)> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Remove { id, r#async } => Some((id, r#async)),
                    _ => None,
                })
                .collect()
        }

        /// KHook runs the removal function it stored at setup with that hook's context pushed
        /// (`detour.cpp:1531-1541`) - that is what releases the context.
        fn run_removal(&self, id: HookId) {
            let context = self
                .contexts
                .lock()
                .unwrap()
                .iter()
                .find(|(hook_id, _)| *hook_id == id)
                .map(|(_, context)| *context as *mut c_void);
            let removed = self
                .removed
                .lock()
                .unwrap()
                .iter()
                .find(|(hook_id, _)| *hook_id == id)
                .map(|(_, removed)| *removed);
            if let (Some(context), Some(removed)) = (context, removed) {
                // SAFETY: `removed` is the address the wrapper registered as the `removed`
                // thunk for this signature (`khook.hpp:711-719`), a `fn(HookId)`.
                let f: unsafe extern "C" fn(HookId) = unsafe { core::mem::transmute(removed) };
                CONTEXTS.with(|stack| stack.borrow_mut().push(context));
                // SAFETY: the wrapper registered this thunk with exactly this signature
                // (`khook.hpp:711-719`); the mock stands in for the KHook that calls it.
                unsafe { f(id) };
                CONTEXTS.with(|stack| stack.borrow_mut().pop());
            }
        }
    }

    impl KHookApi for Mock {
        unsafe fn setup_hook(
            &self,
            _function: *mut c_void,
            context: *mut c_void,
            _removed_function: *mut c_void,
            _pre: *mut c_void,
            _post: *mut c_void,
            _make_return: *mut c_void,
            _make_call_original: *mut c_void,
            _stack_size: u32,
            _async: bool,
        ) -> HookId {
            // The wrapper hands its context ownership over; give that reference back so a
            // wrong-call mock does not leak it.
            //
            // SAFETY: `register_inline` passes a live context whose only outstanding references
            // are the creator's and the one it just took (`s2sdk::hook`'s contract), which is
            // exactly the reference this mock is refusing to keep.
            unsafe { s2sdk::hook::release(context) };
            unreachable!("save/restore only installs a virtual hook")
        }

        unsafe fn setup_virtual_hook(
            &self,
            vtable: *mut *mut c_void,
            index: i32,
            context: *mut c_void,
            removed_function: *mut c_void,
            pre: *mut c_void,
            post: *mut c_void,
            make_return: *mut c_void,
            make_call_original: *mut c_void,
            stack_size: u32,
            r#async: bool,
        ) -> HookId {
            self.calls.lock().unwrap().push(Call::SetupVirtual {
                vtable: vtable as usize,
                index,
                r#async,
                stack_size,
            });
            assert!(!pre.is_null() && !post.is_null());
            assert!(!make_return.is_null() && !make_call_original.is_null());
            assert!(!removed_function.is_null(), "KHook needs the removal function");
            assert!(!context.is_null());
            if self.fail.load(Ordering::SeqCst) {
                // `khook.hpp:2229`: the setup failed, nothing registered.
                return INVALID_HOOK;
            }
            let id = NEXT_ID.fetch_add(1, Ordering::SeqCst) + 1;
            self.contexts
                .lock()
                .unwrap()
                .push((id, context as usize));
            self.removed
                .lock()
                .unwrap()
                .push((id, removed_function as usize));
            id
        }

        unsafe fn remove_hook(
            &self,
            id: HookId,
            r#async: bool,
            _hook_removal_fn: *mut c_void,
            _context: *mut c_void,
        ) {
            self.calls.lock().unwrap().push(Call::Remove { id, r#async });
            // A parked removal stands in for one still inside KHook: the caller (a synchronous
            // removal) holds the hook gate for the whole call, so the case holds the gate here.
            let park = self.park.lock().unwrap().take();
            if let Some(park) = park {
                let _ = park.arrived.send(());
                let _ = park.release.recv_timeout(std::time::Duration::from_secs(10));
            }
            // Both kinds run the removal here: the case only checks which one was asked for
            // (`async = false` from `remove_sync`, `khook.hpp:673` for `Drop`), and running it
            // keeps the process-wide context count honest.
            if self.defer.load(Ordering::SeqCst) {
                // The modelled delete thread holds it: the entry (and its context) stays alive
                // until a later drain, which is exactly what a stale callback needs.
                self.deferred.lock().unwrap().push(id);
                return;
            }
            self.run_removal(id);
        }

        fn get_context_ptr(&self) -> *mut c_void {
            CONTEXTS.with(|stack| stack.borrow().last().copied().unwrap_or(ptr::null_mut()))
        }

        fn get_original_function(&self) -> *mut c_void {
            ORIGINAL.with(Cell::get)
        }

        fn get_original_value_ptr(&self) -> *mut c_void {
            FRAMES.with(|frames| {
                frames
                    .borrow()
                    .last()
                    .and_then(|frame| frame.original_value.as_ref())
                    .map_or(ptr::null_mut(), |value| {
                        core::ptr::from_ref(value.as_ref()).cast_mut().cast()
                    })
            })
        }

        fn get_override_value_ptr(&self) -> *mut c_void {
            FRAMES.with(|frames| {
                frames
                    .borrow()
                    .last()
                    .and_then(|frame| frame.override_value.as_ref())
                    .map_or(ptr::null_mut(), |value| {
                        core::ptr::from_ref(value.as_ref()).cast_mut().cast()
                    })
            })
        }

        fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                let Some(frame) = frames.last_mut() else {
                    return ptr::null_mut();
                };
                // `detour.cpp:453-468`: the override once the action reached Override, the
                // original's otherwise. The value is read straight after (`make_return`), so
                // the pointer only has to stay valid for the call.
                let value = if frame.action >= Action::Override {
                    frame.override_value.as_mut()
                } else {
                    frame.original_value.as_mut()
                };
                value.map_or(ptr::null_mut(), |value| {
                    core::ptr::from_mut(value.as_mut()).cast()
                })
            })
        }

        fn destroy_return_value(&self) {
            FRAMES.with(|frames| {
                assert!(
                    !frames.borrow().is_empty(),
                    "DestroyReturnValue with no call in progress (`detour.cpp:408-433` pops one)"
                );
                frames.borrow_mut().pop();
            });
        }

        fn find_original(&self, function: *mut c_void) -> *mut c_void {
            // `detour.cpp:1897-1905`: the function itself when it is not hooked.
            function
        }

        fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
            // SAFETY: the caller guarantees the slot is readable (`s2sdk::hook`'s contract).
            unsafe { *vtable.offset(index as isize) }
        }

        unsafe fn do_recall(
            &self,
            _action: Action,
            _ptr_to_return: *mut c_void,
            _return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
        ) -> *mut c_void {
            unreachable!("the wrapper never recalls a hook")
        }

        unsafe fn save_return_value(
            &self,
            action: Action,
            ptr_to_return: *mut c_void,
            return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
            original: bool,
        ) {
            assert_eq!(return_size, 1, "the hooked signature returns one byte");
            assert!(!ptr_to_return.is_null());
            // SAFETY: the thunk hands a readable pointer for the size it claims.
            let value = unsafe { ptr_to_return.cast::<u8>().read() };
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                let frame = frames.last_mut().expect("a call is in progress");
                if original {
                    // `detour.cpp:370-372`: only inside `make_call_original`'s window.
                    assert!(frame.original_in_progress);
                    frame.original_value = Some(Box::new(value));
                } else if action > frame.action {
                    // `detour.cpp:381-383`: a strictly higher action replaces the value; among
                    // equals the first saved wins.
                    frame.action = action;
                    frame.override_value = Some(Box::new(value));
                }
            });
        }

        fn lookup_signature(
            &self,
            _start: *mut c_void,
            _size: usize,
            _signature: *const core::ffi::c_char,
        ) -> *mut c_void {
            unreachable!("the wrapper never looks a signature up by itself")
        }

        fn was_original_function_skipped(&self) -> bool {
            FRAMES.with(|frames| frames.borrow().last().is_some_and(|frame| frame.original_skipped))
        }
    }

    // -------------------------------------------------------------------------------------
    // The harness
    // -------------------------------------------------------------------------------------

    /// Run one case with a fresh mock installed and the bridge's log pointed at
    /// `target/scratch/<name>.log` (never the user's `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log`),
    /// and leave the process as it was: disarm the hook, remove the mock, and fail if KHook still
    /// holds a context of this module.
    fn with_mock<R>(name: &str, body: impl FnOnce(&'static Mock, &crate::hooks::test_guard::ScratchLog) -> R) -> R {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log(name);
        assert_eq!(
            s2sdk::hook::live_context_count(),
            0,
            "a previous case left a hook context KHook can still call into"
        );
        let mock: &'static Mock = Box::leak(Box::new(Mock::default()));
        s2sdk::hook::install(mock);
        let out = body(mock, &scratch);
        // Whatever the case left armed is disarmed here, through the mock, and the API is only
        // forgotten after the drain - the order `hooks::unload_hooks` uses.
        let _ = sr_disarm();
        let _ = s2sdk::hook::remove_all_blocking(std::time::Duration::from_secs(10));
        s2sdk::hook::uninstall();
        assert_eq!(
            s2sdk::hook::live_context_count(),
            0,
            "every removal function ran by the end of the case"
        );
        out
    }

    /// The trampoline in the shape the generated thunks expect: KHook's per-call state around
    /// one hooked call (`detour.cpp:746-990`).
    fn trampoline(
        mock: &Mock,
        this: *mut c_void,
        original: unsafe extern "C" fn(*mut c_void) -> u8,
    ) -> u8 {
        FRAMES.with(|frames| frames.borrow_mut().push(Frame::default()));
        CONTEXTS.with(|stack| stack.borrow_mut().push(mock.context()));
        ORIGINAL.with(|slot| slot.set(original as *mut c_void));
        if let Some(seed) = mock.seed.lock().unwrap().take() {
            // An earlier value changer already saved this call's override.
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                let frame = frames.last_mut().unwrap();
                frame.action = Action::Supersede;
                frame.override_value = Some(Box::new(seed));
            });
        }
        // SAFETY: the callbacks are the ones this module registered for `u8 f(this)`.
        let _ = unsafe { allow_sr::pre(this) };
        let skip = FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            let frame = frames.last_mut().unwrap();
            if frame.action == Action::Supersede {
                frame.original_skipped = true; // `detour.cpp:889`
                true
            } else {
                frame.original_in_progress = true; // `detour.cpp:894`
                false
            }
        });
        if !skip {
            // SAFETY: as above - the generated thunk for `u8 f(this)`, given KHook's per-call
            // state planted above.
            let _ = unsafe { allow_sr::make_call_original(this) };
            FRAMES.with(|frames| frames.borrow_mut().last_mut().unwrap().original_in_progress = false);
        }
        // SAFETY: as above; `post` is the other callback this module registered.
        let _ = unsafe { allow_sr::post(this) };
        // SAFETY: `make_return` reads the saved value and destroys the per-call state; the
        // harness planted both above.
        let out = unsafe { allow_sr::make_return(this) };
        CONTEXTS.with(|stack| stack.borrow_mut().pop());
        out
    }

    /// A fake config object: only its address is used (`sr_arm` records it, the callbacks
    /// compare it).
    fn fake_object() -> *mut c_void {
        static OBJECT: AtomicU64 = AtomicU64::new(0x5A5A_5A5A);
        (&OBJECT as *const AtomicU64).cast_mut().cast()
    }

    /// Plant one call's state (what [`trampoline`] does before it runs a callback) without
    /// running any: the frame, the first registration's context and the original.
    fn begin_call(context: *mut c_void, original: unsafe extern "C" fn(*mut c_void) -> u8) {
        FRAMES.with(|frames| frames.borrow_mut().push(Frame::default()));
        CONTEXTS.with(|stack| stack.borrow_mut().push(context));
        ORIGINAL.with(|slot| slot.set(original as *mut c_void));
    }

    /// Point the next thunk at another registration's context: KHook pushes the context of the
    /// callback it is about to run (`detour.cpp:359-361`), so one call that reaches two
    /// registrations' callbacks reads a different one per callback.
    fn at(context: *mut c_void) {
        CONTEXTS.with(|stack| {
            *stack.borrow_mut().last_mut().expect("a call is in progress") = context;
        });
    }

    /// The action this call's callbacks have reached so far (the fold's maximum).
    fn action() -> Action {
        FRAMES.with(|frames| frames.borrow().last().expect("a call is in progress").action)
    }

    /// `detour.cpp:889`/`:894`: a `Supersede` skips the original, anything lower calls it.
    fn original_step(this: *mut c_void) {
        let skip = FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            let frame = frames.last_mut().expect("a call is in progress");
            if frame.action == Action::Supersede {
                frame.original_skipped = true;
                true
            } else {
                frame.original_in_progress = true;
                false
            }
        });
        if !skip {
            // SAFETY: the generated thunk for `u8 f(this)`, with KHook's per-call state planted
            // by [`begin_call`].
            let _ = unsafe { allow_sr::make_call_original(this) };
            FRAMES.with(|frames| {
                frames.borrow_mut().last_mut().unwrap().original_in_progress = false;
            });
        }
    }

    /// Finish the call: `make_return` reads the saved value and destroys the per-call state.
    fn end_call(this: *mut c_void) -> u8 {
        // SAFETY: the generated thunk for `u8 f(this)`, with KHook's per-call state planted by
        // [`begin_call`].
        let out = unsafe { allow_sr::make_return(this) };
        CONTEXTS.with(|stack| {
            stack.borrow_mut().pop();
        });
        out
    }

    /// A re-arm over a disarm whose removal KHook still holds cannot make the old callbacks act:
    /// the retired generation answers `Ignore`, so the stale `pre` neither supersedes again for
    /// the (re-recorded) object nor counts a false "ahead" against the value the live one saved.
    #[test]
    fn a_rearm_over_a_deferred_removal_leaves_the_stale_callbacks_inert() {
        with_mock("save-restore-stale-generation", |mock, scratch| {
            let object = fake_object();
            sr_arm(object).expect("arm");
            let stale = mock.context_nth(0);
            // The disarm runs from the GameFrame tick's thunk: its removal goes to `Drop`, and
            // the modelled delete thread has not run the removal function yet.
            mock.defer_removals(true);
            {
                let _thunk = s2sdk::hook::ThunkGuard::enter();
                let detail = sr_disarm().expect("something was armed");
                assert!(detail.starts_with("removal deferred"), "{detail}");
            }
            mock.defer_removals(false);
            assert_eq!(
                mock.deferred_len(),
                1,
                "the removal is still held by the delete thread"
            );
            // Re-arm for the same object: a new registration, a new generation, the same
            // `SR_OBJ` (which is why the generation, not the object, has to tell them apart).
            sr_arm(object).expect("re-arm over the pending removal");
            let live = mock.context_nth(1);
            assert_ne!(stale, live, "a second registration");

            unsafe extern "C" fn original(_this: *mut c_void) -> u8 {
                0 // CS2's AllowSaveRestore: false.
            }

            // KHook's order inside one call: the newest registration's `pre` first, then the
            // stale one (`detour.cpp:1477-1489`: a later pre+post hook is inserted ahead of the
            // existing ones), the skipped original, then the posts in reverse.
            begin_call(live, original);
            // SAFETY: the callbacks are the ones this module registered for `u8 f(this)`.
            let _ = unsafe { allow_sr::pre(object) };
            assert_eq!(action(), Action::Supersede, "the live pre owns the call");
            at(stale);
            // SAFETY: as above.
            let _ = unsafe { allow_sr::pre(object) };
            assert_eq!(action(), Action::Supersede, "the stale pre changed nothing");
            assert_eq!(sr_conflicts(), 0, "and reported no conflict");
            original_step(object);
            // The posts run in reverse: the stale one first, and it must leave the thread's
            // note alone; the live one then sees its own value still saved.
            // SAFETY: as above.
            let _ = unsafe { allow_sr::post(object) };
            // The live post must run with the live pair's context, not the stale one's: the
            // note it takes and the value it compares are keyed by the generation, and this
            // call's live registration is the one that owns them.
            at(live);
            // SAFETY: as above.
            let _ = unsafe { allow_sr::post(object) };
            assert_eq!(end_call(object), 1, "the live registration's value is the call's");
            assert_eq!(sr_conflicts(), 0);

            // On its own (no live registration in the call) the stale pre must not supersede
            // either: the original runs and its false stands.
            begin_call(stale, original);
            // SAFETY: as above.
            let _ = unsafe { allow_sr::pre(object) };
            assert_eq!(action(), Action::Ignore, "a retired registration does not act");
            original_step(object);
            // SAFETY: as above.
            let _ = unsafe { allow_sr::post(object) };
            assert_eq!(end_call(object), 0, "the original's answer");
            assert_eq!(sr_conflicts(), 0);
            assert!(
                !scratch.text().contains("khook conflict"),
                "no false conflict line: {}",
                scratch.text()
            );
        });
    }

    #[test]
    fn enable_installs_one_virtual_hook_on_the_pinned_slot_synchronously() {
        with_mock("save-restore-enable", |mock, _scratch| {
            sr_arm(fake_object()).expect("the mock accepts the registration");
            assert!(sr_enabled());
            let calls = mock.calls();
            assert_eq!(calls.len(), 1, "one registration: {calls:?}");
            match calls[0] {
                Call::SetupVirtual {
                    vtable,
                    index,
                    r#async,
                    stack_size,
                } => {
                    let pinned = (modules_base(pins::PIN_SERVER)
                        + pins::GSV_GAMECONFIG_SERVER_VT.value) as usize;
                    assert_eq!(vtable, pinned, "the pinned GSV_GAMECONFIG_SERVER_VT table");
                    assert_eq!(
                        index,
                        sys::ISource2ServerConfig_slots::AllowSaveRestore as i32,
                        "the generated slot, cast to i32"
                    );
                    assert!(!r#async, "SetupMode::Sync on the main thread");
                    assert_eq!(stack_size, allow_sr::STACK_SIZE, "the computed stack size");
                }
                other => panic!("expected a virtual setup, got {other:?}"),
            }
            // Disable: one synchronous removal, and the state is gone.
            let detail = sr_disarm().expect("something was armed");
            assert_eq!(detail, "removed (Removed)");
            assert!(!sr_enabled());
            assert_eq!(mock.removals().len(), 1, "one RemoveHook");
            assert!(!mock.removals()[0].1, "async = false (remove_sync)");
        });
    }

    #[test]
    fn the_hooked_slot_answers_one_for_our_object_and_skips_the_original() {
        with_mock("save-restore-answers", |_mock, _scratch| {
            let object = fake_object();
            sr_arm(object).expect("arm");
            static ORIGINAL_CALLS: AtomicU64 = AtomicU64::new(0);
            unsafe extern "C" fn original(_this: *mut c_void) -> u8 {
                ORIGINAL_CALLS.fetch_add(1, Ordering::SeqCst);
                0 // CS2's AllowSaveRestore: false.
            }
            let value = trampoline(_mock, object, original);
            assert_eq!(value, 1, "our object answers true");
            assert_eq!(
                ORIGINAL_CALLS.load(Ordering::SeqCst),
                0,
                "Supersede skips the original (`detour.cpp:889`)"
            );
            assert_eq!(sr_conflicts(), 0, "no other value changer was on the call");
            assert!(!CONFLICT_REPORTED.load(Ordering::Acquire));

            // A foreign object of the class: `Ignore`, so the original runs and its value wins.
            let foreign = 0x1234_usize as *mut c_void;
            let value = trampoline(_mock, foreign, original);
            assert_eq!(value, 0, "the original's value (false) for a foreign object");
            assert_eq!(ORIGINAL_CALLS.load(Ordering::SeqCst), 1, "the original ran");
            assert_eq!(sr_conflicts(), 0);
        });
    }

    #[test]
    fn an_earlier_value_changer_is_reported_once_and_counted_once() {
        // The log line is read back from a scratch file under `target/`: `log::line` is the only
        // writer, so a count of the marker in this file is a count of the lines written. The
        // guard also points `XDG_RUNTIME_DIR` at an empty scratch directory, so the user's
        // `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` is provably not written.
        with_mock("khook-conflict", |mock, scratch| {
            let object = fake_object();
            sr_arm(object).expect("arm");
            *mock.seed.lock().unwrap() = Some(0);
            unsafe extern "C" fn original(_this: *mut c_void) -> u8 {
                0
            }
            // `detour.cpp:381-383`: the earlier equal-action value stays, so the call answers
            // 0 and our post sees a value that is not ours.
            assert_eq!(trampoline(mock, object, original), 0);
            assert_eq!(sr_conflicts(), 1, "one conflict for the call, not two");
            assert!(
                CONFLICT_REPORTED.load(Ordering::Acquire),
                "the rate-limited line was written once"
            );

            // A second conflicting call counts, but does not write a second line.
            *mock.seed.lock().unwrap() = Some(0);
            assert_eq!(trampoline(mock, object, original), 0);
            assert_eq!(sr_conflicts(), 2);

            // The two lines the case can write: the rate-limited conflict line and the arm line.
            // The marker is counted while the scratch log still exists.
            let marker = "khook conflict: ISource2ServerConfig::AllowSaveRestore ahead";
            let text = scratch.text();
            assert_eq!(
                text.matches(marker).count(),
                1,
                "exactly one rate-limited line per arm: {text}"
            );
            assert!(
                text.lines().any(|line| line.ends_with(marker)),
                "the line carries the role tag and the target: {text}"
            );
            // And nothing went to the default path (the scratch XDG_RUNTIME_DIR is empty).
            assert!(
                std::fs::read_dir(scratch.xdg())
                    .expect("the scratch XDG dir")
                    .next()
                    .is_none(),
                "the user's XDG_RUNTIME_DIR log was never created"
            );
        });
    }

    #[test]
    fn a_refused_registration_answers_prerequisite_failed_and_stays_disarmed() {
        with_mock("save-restore-refused", |mock, _scratch| {
            mock.fail.store(true, Ordering::SeqCst);
            let error = sr_arm(fake_object()).expect_err("INVALID_HOOK is refused");
            assert_eq!(error.code, -32017);
            assert_eq!(error.name, "PrerequisiteFailed");
            assert!(
                error.data.contains("SetupVirtualHook returned INVALID_HOOK"),
                "the reason names the refused call: {}",
                error.data
            );
            assert!(!sr_enabled(), "nothing is armed after a refused registration");
            assert!(SR_OBJ.load(Ordering::Acquire).is_null());
        });
    }

    /// No API installed: the registration is refused with `KHookNotInstalled`, which the op
    /// turns into `-32017`; nothing is armed. (The gate's own `-32017` for this state is
    /// `tests/ops_hooks.rs`'s case - it answers before the body runs.)
    #[test]
    fn arming_without_khook_is_refused_and_stays_disarmed() {
        let _guard = crate::hooks::test_guard::lock();
        assert!(s2sdk::hook::api().is_none());
        let error = sr_arm(fake_object()).expect_err("no API, no hook");
        assert_eq!(error.code, -32017);
        assert!(
            error.data.contains("no KHook IKHook API is installed"),
            "{}",
            error.data
        );
        assert!(!sr_enabled());
    }

    /// The reply member this build adds: `conflicts` follows `enabled`, byte for byte (a wire
    /// addition over the C's `{"enabled":..}`).
    #[test]
    fn reply_bytes_carry_the_conflict_counter() {
        let text = |enabled: bool, conflicts: u64| {
            let mut buf = vec![0u8; 64];
            let mut sb = Sb::new(&mut buf);
            write_reply(&mut sb, enabled, conflicts);
            String::from_utf8(sb.as_cstr().to_vec()).expect("the reply is ASCII")
        };
        assert_eq!(text(true, 0), r#"{"enabled":true,"conflicts":0}"#);
        assert_eq!(text(false, 3), r#"{"enabled":false,"conflicts":3}"#);
    }

    /// An enable from the GameFrame tick runs inside a KHook thunk. There `s2sdk::hook` takes the
    /// hook gate with `try_lock` only (waiting could deadlock against a synchronous removal
    /// spinning on this very call's capsule), so the answer depends on whether the gate is free:
    /// free -> the registration happens; held -> [`Error::GateBusy`], mapped to `-32017` with
    /// `{"blockedOn":"khook: retry next tick"}` and nothing installed. [`Error::InThunk`] is no
    /// longer produced by a registration and is mapped the same way (defensive).
    #[test]
    fn a_registration_from_inside_a_thunk_tries_the_gate() {
        with_mock("save-restore-in-thunk", |mock, _scratch| {
            // (a) The gate is free: a call of one of our own hooks on this thread means the tick
            // is draining the bridge queue, and a registration from there is accepted.
            {
                let _thunk = s2sdk::hook::ThunkGuard::enter();
                sr_arm(fake_object()).expect("the gate is free inside a thunk");
                assert!(sr_enabled(), "the hook is armed");
                assert!(s2sdk::hook::in_thunk(), "still inside the thunk");
            }
            let setups = mock
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::SetupVirtual { .. }))
                .count();
            assert_eq!(setups, 1, "KHook was asked exactly once");

            // A fresh registration for the held-gate case: an armed slot would short-circuit
            // `sr_arm` before it reaches the gate.
            let detail = sr_disarm().expect("something was armed");
            assert!(detail.starts_with("removed"), "{detail}");

            // (b) The gate is held by another thread's synchronous removal, which is parked
            // inside the mock: the registration must not wait for it, it answers `GateBusy`.
            let (arrived_tx, arrived_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            *mock.park.lock().unwrap() = Some(Park {
                arrived: arrived_tx,
                release: release_rx,
            });
            let remover = std::thread::spawn(move || {
                // The hook being removed is registered on this thread (`Hook` is `!Send`). Its
                // table is fabricated: the mock is the KHook and never calls the thunk.
                let mut table = [0usize; 4];
                // SAFETY: a live four-entry table and an index inside it, with the `u8 f(this)`
                // thunks `sr_arm` uses; the mock never dispatches a call through it.
                let hook = unsafe {
                    allow_sr::setup_virtual(
                        table.as_mut_ptr().cast(),
                        2,
                        Vec::new(),
                        Vec::new(),
                        None,
                        SetupMode::Sync,
                    )
                }
                .expect("the remover's own hook is registered");
                hook.remove_sync()
            });
            arrived_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the remover is inside KHook with the gate held");
            let calls_before = mock.calls().len();
            {
                let _thunk = s2sdk::hook::ThunkGuard::enter();
                let error = sr_arm(fake_object()).expect_err("the gate is held: retry later");
                assert_eq!(error.code, -32017);
                assert_eq!(error.name, "PrerequisiteFailed");
                assert_eq!(error.data, r#"{"blockedOn":"khook: retry next tick"}"#);
                assert!(!sr_enabled(), "nothing is armed");
                assert!(SR_OBJ.load(Ordering::Acquire).is_null());
            }
            assert_eq!(
                mock.calls().len(),
                calls_before,
                "KHook was never asked: the refusal is s2sdk's gate"
            );
            release_tx.send(()).expect("the remover is still parked");
            let removal = remover.join().expect("the remover finished");
            assert_eq!(removal.expect("removed"), Removal::Removed);

            // The retry, outside the thunk and with the gate free, is the plain success path.
            sr_arm(fake_object()).expect("the retry after the gate is free succeeds");
            assert!(sr_enabled(), "the retry armed the hook");
        });
    }

    /// Both thunk-time refusals answer the same retry hint; a refusal that is not about a busy
    /// gate keeps its own `{"reason":...}` text instead.
    #[test]
    fn thunk_and_gate_refusals_are_retry_next_tick() {
        for error in [Error::GateBusy, Error::InThunk] {
            let reply = khook_registration_error(&error);
            assert_eq!(reply.code, -32017);
            assert_eq!(reply.name, "PrerequisiteFailed");
            assert_eq!(reply.data, r#"{"blockedOn":"khook: retry next tick"}"#);
        }
        // Anything else is the registration's own failure: it keeps the C's `{"reason":..}`
        // shape and names the hook, instead of borrowing the gate's retry hint.
        let other = khook_registration_error(&Error::KHookNotInstalled);
        assert_eq!(other.code, -32017);
        assert_eq!(other.name, "PrerequisiteFailed");
        assert!(
            other.data.starts_with(r#"{"reason":"ISource2ServerConfig::AllowSaveRestore: "#),
            "the non-gate refusal keeps its own reason: {}",
            other.data
        );
        assert!(
            !other.data.contains("retry next tick"),
            "the gate hint is not borrowed: {}",
            other.data
        );
    }

    /// A disable from the same tick is a success: the armed flag is cleared first and the hook
    /// goes down the asynchronous path (`Drop`), never `remove_sync` (which defers in every
    /// build, so the test asserts the mode rather than a spin).
    #[test]
    fn a_disable_inside_a_thunk_defers_the_removal_and_still_turns_the_feature_off() {
        with_mock("save-restore-deferred", |mock, _scratch| {
            sr_arm(fake_object()).expect("arm");
            let _thunk = s2sdk::hook::ThunkGuard::enter();
            let detail = sr_disarm().expect("something was armed");
            assert_eq!(
                detail,
                "removal deferred (asynchronous: inside a hook thunk)"
            );
            assert!(!sr_enabled(), "the feature is off");
            assert!(SR_OBJ.load(Ordering::Acquire).is_null());
            let removals = mock.removals();
            assert_eq!(removals.len(), 1, "one RemoveHook: {removals:?}");
            assert!(removals[0].1, "async = true (`Drop`), never remove_sync");
        });
    }

    /// The busy-vs-other split of [`khook_registration_error`]: a refusal that is not the gate
    /// keeps its own message and its `{"reason":...}` data.
    #[test]
    fn a_non_busy_refusal_keeps_its_reason() {
        let error = khook_registration_error(&Error::KHookNotInstalled);
        assert_eq!(error.code, -32017);
        assert_eq!(error.name, "PrerequisiteFailed");
        assert!(
            error.data.starts_with("{\"reason\":"),
            "the ordinary reason shape: {}",
            error.data
        );
        assert!(error.data.contains("no KHook IKHook API is installed"));
        assert!(!error.data.contains("retry next tick"));
    }
}
