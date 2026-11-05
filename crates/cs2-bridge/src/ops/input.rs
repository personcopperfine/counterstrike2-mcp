//! The input group (`input.set` / `input.clear` / `input.status` / `input.uninstall`): the
//! C's `op_input` (`bridge/mcp_bridge.c:4932-4975`) and its two CCSGOInput hooks
//! (`:4748-4868`), ported onto KHook.
//!
//! # Mechanism
//!
//! The class vtable's two empty-ish entries are hooked in place with KHook
//! (`SetupVirtualHook`, [`SetupMode::Sync`]) on the pinned table
//! `modules_base(PIN_CLIENT) + pins::CV_CSGOINPUT_VT`:
//!
//! * [`sys::CClientInput_slots::unk3021`] (ProcessInput begin, before the button gather):
//!   the game's entry is an empty function; one **pre** callback runs the button-delta body
//!   and answers [`Return::ignore`], so the call's outcome never changes.
//! * [`sys::CClientInput_slots::unk3023`] (the CreateMove view-angle override): one **pre**
//!   callback supersedes the call with 1 and writes the three angles while the override is
//!   armed, and a **post** callback watches for a competing value changer.
//!
//! The index is always the generated slot, never a number written here. The install is
//! [`SetupMode::Sync`] because `input.set` runs on the main thread from the drain and the
//! frame calls are not on that stack.
//!
//! # Why KHook, and not a per-object vptr copy
//!
//! The old implementation copied the class's vtable into a private array, pointed **this one
//! object's** vptr at the copy, and restored the original vptr on disable. That was safe only
//! while nothing else touched the class's table, and CS2 has other plugins. A per-object copy
//! *bypasses* a virtual another plugin hooked (that plugin's callback is not in our copy) and
//! *freezes* whatever was in the table when we copied it (a hook another plugin installs
//! afterwards never reaches this object). KHook patches the class's **shared** table entry,
//! once, in place, and never restores it (`third_party/khook/src/detour.hpp:96-104`), so every
//! object of the class and every plugin that hooks the slot later sees the same entry - and
//! the bridge's identity check asks KHook for the saved original instead of comparing the
//! word (`crate::modules::slot_matches`), so the pinned slots 30/32 still identify the object.
//!
//! # Coexistence rule (the §13 rule)
//!
//! Passive by default: each callback answers `Ignore` unless `this` is the recorded
//! `CCSGOInput` (both targets are singletons, so the recorded object is the only one that
//! matters). The view-angle callback is the one value changer and it **Supersedes** for that
//! object. When two value changers meet on one call the bridge reports a **conflict** rather
//! than picking a winner silently:
//!
//! * *ahead*: our `pre` runs after an earlier callback already saved an override for this
//!   call (`GetOverrideValuePtr()` non-null; KHook folds by max, `khook.hpp:744-752`, and
//!   `detour.cpp:381-383` keeps the FIRST value among equal actions), so a plugin loaded
//!   before us wins the fold. That is accepted: pre-only callbacks are inserted at the head
//!   (`detour.cpp:1458-1466`), so a bot/gamemode plugin registered **after** us runs its pre
//!   first, saves first, and beats our angle override on an equal `Supersede`. A neighbour
//!   that answered `Handled` on the call saves a value too, so it counts as a value changer
//!   here - the same reading save/restore makes.
//! * *after*: our `post` reads the call's final override value and it is not the 1 we saved.
//!
//! A conflict is a rate-limited log line (`khook conflict: CClientInput::unk3023
//! <ahead|after>`, at most once per target per arm), the `conflicts` counter in the status
//! reply, and - under `cfg(debug_assertions)` only - a **caught** `debug_assert!`: this runs
//! in an `extern "C"` thunk, where an escaping panic aborts the process. The void
//! frame-begin target changes nothing, so it has no conflict detection.
//!
//! # Threads and state
//!
//! Both slots are main-thread frame calls; the per-frame state ([`InState`]) sits behind one
//! [`Mutex`] where the C used its `g_inLock` spinlock (`mcp_bridge.c:4773-4774`), and the
//! counters stay the game group's statics ([`crate::ops::game::IN_FRAMES`],
//! [`crate::ops::game::IN_ANGLES_ON`], [`crate::ops::game::IN_ANGLES`]) plus [`IN_EDGES`] /
//! [`IN_ANGLE_TICKS`] here. Install/uninstall happen only from the main-thread drain, and a
//! hook is removed with [`Hook::remove_sync`] - never from inside a callback.
//!
//! # Teardown: a disarmed pair is inert, even before it is gone
//!
//! `remove_sync` from inside a thunk (a lane E host can run an op on our own `GameFrame` stack,
//! lane A2) answers [`Removal::Deferred`] instead of waiting: KHook's delete thread removes the
//! hook a few ms later, so the callbacks of a disarmed pair can still be invoked. Every
//! callback therefore carries the [`ARMED_GEN`] its install captured and answers `Ignore`
//! (recording nothing) when the counter has moved on. Disarm bumps it **first**, before the
//! held deltas are released, so a pending removal can never run our body behind the user's
//! back; a re-arm during a pending removal is harmless for the same reason (the old pair is
//! inert whatever KHook still calls), and the new pair is the only one that acts.

use core::cell::Cell;
use core::ffi::c_char;
use core::ffi::c_void;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use s2sdk::hook::{Hook, HookCall, Removal, Return, SetupMode};
use s2sdk::khook_thunks;
use s2sdk_sys::generated as sys;

use crate::control;
use crate::entity_json::{json_find, json_float3, json_int, Sb};
use crate::generated::pins;
use crate::hooks::TeardownReport;
use crate::ops::game::cstr_tail;
use crate::ops::game::schema::modules_base;
use crate::ops::game::{IN_ANGLES, IN_ANGLES_ON, IN_FRAMES};
use crate::ops::ident::{mem_ok, obj_is};
use crate::ops::OpErr;
use crate::queue::Request;
use crate::{guard, log};

/// The input dispatch arm's result buffer (`mcp_bridge.c:6047`, `static char rbuf[2048]`).
const INPUT_BUF: usize = 2048;

/// A button name's scan buffer (`mcp_bridge.c:4882`, `char name[48]`).
const NAME_CAP: usize = 48;

// `void CClientInput::unk3021(CSplitScreenSlot, void*)` as the generated vtable carries it,
// with the generated 4-byte `CSplitScreenSlot` by value: both ABIs pass such a class in the
// argument slot an `int` would take, so the machine signature is the generated one. A void
// return comes back in no register on either target.
khook_thunks! {
    /// The generated `CClientInput::unk3021` signature (ProcessInput begin).
    pub mod in_frame_begin_hook: fn(
        this: *mut c_void,
        slot: val sys::CSplitScreenSlot,
        rec: val *mut c_void,
    ) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

// `bool CClientInput::unk3023(CSplitScreenSlot, double, QAngle*)` as the generated vtable
// carries it, declared `-> u8` and never a Rust `bool`: the game reads the low byte of the
// return register, and a `bool` would be a Rust-validity assumption across an ABI this crate
// does not control (another plugin's callback may produce the value). A scalar return comes
// back in the register on both targets, so both sret flags are `no_sret`.
khook_thunks! {
    /// The generated `CClientInput::unk3023` signature (the CreateMove view-angle override).
    pub mod in_angles_hook: fn(
        this: *mut c_void,
        slot: val sys::CSplitScreenSlot,
        curtime: val f64,
        ang: val *mut sys::QAngle,
    ) -> u8 {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// The conflict lines' name for the view-angle target.
const TARGET: &str = "CClientInput::unk3023";

// Both hooked slots are in-table entries of the measured table (the C's `_Static_assert`
// shape, `mcp_bridge.c:5011`).
const _: () = assert!(sys::CClientInput_slots::unk3021 < sys::CClientInput_slots::VTLEN);
const _: () = assert!(sys::CClientInput_slots::unk3023 < sys::CClientInput_slots::VTLEN);

/// The two live hooks, while armed. [`Hook`] is `!Send` because it holds a raw context
/// pointer; [`InputHooks`] is the main-thread-only claim that makes it storable in a static -
/// `in_install`, the teardown and the tool `PreShutdown` all run on the main thread, and the
/// callbacks never take this lock.
static IN: Mutex<Option<InputHooks>> = Mutex::new(None);

/// The hooks plus the claim that only the game's main thread moves them.
struct InputHooks {
    /// Slot [`sys::CClientInput_slots::unk3021`] (ProcessInput begin).
    frame_begin: Hook,
    /// Slot [`sys::CClientInput_slots::unk3023`] (the view-angle override).
    angles: Hook,
}

// SAFETY: every access is behind [`IN`]'s mutex and comes from the main thread (install from
// the drain, removal from `in_uninstall`/`hooks_teardown`/`PreShutdown`). A `Hook` is only
// `!Send` because its context pointer is not `Sync`-checked by the compiler; nothing in
// KHook's API is thread-affine - `RemoveHook` may be called from any thread - and the
// reference counts inside the context are atomic.
unsafe impl Send for InputHooks {}

/// The C's `g_inObj`: the recorded object. It **persists after uninstall** (the C's `g_inObj`
/// does, and `game.selectWeapon`'s second arm compares against it while the hook is
/// installed), so it is its own atomic rather than part of [`IN`].
static IN_OBJ: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Conflict events on the view-angle target since the last arm: `"conflicts":N`.
static CONFLICTS: AtomicU64 = AtomicU64::new(0);

/// Whether the rate-limited conflict line has been written since the last arm.
static CONFLICT_REPORTED: AtomicBool = AtomicBool::new(false);

/// What this arm's `pre` decided for the call the matching `post` is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreNote {
    /// Our `pre` answered `Ignore` (a foreign object, a non-zero sub-slot, the override off, a
    /// null `ang`): the call's value is not ours, so `post` has nothing to check - even when
    /// some other value changer saved something.
    Ignored,
    /// Our `pre` superseded with 1 and no earlier callback had saved a value.
    Superseded,
    /// Our `pre` superseded with 1, but an earlier callback in this call had already saved a
    /// value (counted as "ahead" - `post` must not count the same call again as "after").
    SupersededAhead,
}

thread_local! {
    /// What our `pre` decided on this thread, keyed by the [`ARMED_GEN`] of the pair that wrote
    /// it: a stale pair's `post` (a deferred removal still in KHook's delete thread) must not
    /// consume the note of the pair that is live now, and vice versa. A stale `pre` writes no
    /// note at all (see [`angles_pre`]), so it cannot overwrite the live pair's one either.
    static PRE_NOTE: Cell<Option<(u64, PreNote)>> = const { Cell::new(None) };
}

/// Record this pair's decision for the call its `post` will see.
fn set_pre_note(generation: u64, note: PreNote) {
    PRE_NOTE.with(|cell| cell.set(Some((generation, note))));
}

/// Take this pair's decision; `None` when no `pre` of this generation wrote one (a `post`
/// without our `pre`, or a stale pair).
fn take_pre_note(generation: u64) -> Option<PreNote> {
    PRE_NOTE.with(|cell| match cell.get() {
        Some((generation_of_note, note)) if generation_of_note == generation => {
            cell.set(None);
            Some(note)
        }
        _ => None,
    })
}

/// Which side of our callback the other value changer was on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// An earlier callback in this call saved an override before our `pre` ran.
    Ahead,
    /// The call's final override value is not the one our `pre` saved.
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
/// The `debug_assert!` is debug-builds-only and is **caught**: this runs inside a KHook thunk
/// (an `extern "C"` frame), where an escaping panic aborts the process, and a debug build must
/// still be able to run the conflict test. Release builds do not compile it at all.
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

/// `g_inEdges` (`mcp_bridge.c:4771`): button deltas applied, +1 and -1 alike.
pub static IN_EDGES: AtomicI64 = AtomicI64::new(0);
/// `g_inAngleTicks` (`mcp_bridge.c:4771`): frames the angle override answered true.
pub static IN_ANGLE_TICKS: AtomicI64 = AtomicI64::new(0);

/// `g_inApply` (`mcp_bridge.c:4764`): `CInputValue::ApplyButtonDelta`, the pinned
/// `EFN_INPUTVALUE_APPLY` entry in engine2. Set at install, before the hooks are published;
/// never cleared (the C never clears it either). The host tests overwrite it with a recorder.
static IN_APPLY: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The C's `InApplyFn` typedef (`mcp_bridge.c:4760`).
pub type InApplyFn = unsafe extern "C" fn(iv: *mut c_void, slot: i32, delta: f32, frac: f32);

/// The C's `g_inWant` / `g_inApplied` / `g_inTap` block (`mcp_bridge.c:4766-4768`), behind
/// one [`Mutex`] where the C used its `g_inLock` spinlock. The view-angle state stays the
/// game group's statics ([`IN_ANGLES_ON`] / [`IN_ANGLES`]), as the counters do; all of it is
/// main-thread state (the hook's slots and every body are main-thread calls).
pub struct InState {
    /// `g_inWant`: the virtual buttons held (`IN_` bit mask).
    pub want: u64,
    /// `g_inApplied`: the bits whose +1 delta we currently own.
    pub applied: u64,
    /// `g_inTap`: the remaining frames of a tap per bit.
    pub tap: [u16; 64],
    /// m3.15: bits released AND tapped in one `input.set` while we owned their +1 delta. The next
    /// frame forces them up (a real -1), then the tap presses them again. Without the gap the held
    /// bit never changes, so the engine sees no second press (ti_convoy FIXME 26).
    pub regap: u64,
}

static IN_STATE: Mutex<InState> = Mutex::new(InState {
    want: 0,
    applied: 0,
    tap: [0; 64],
    regap: 0,
});

/// The state lock (the C's `in_lock` / `in_unlock`, `mcp_bridge.c:4773-4774`). Exposed for
/// the host tests; the production bodies take it themselves.
#[doc(hidden)]
pub fn lock_state() -> MutexGuard<'static, InState> {
    IN_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The arm generation ([`ARMED_GEN`]'s name): every hook's callbacks capture the value their
/// install took and answer `Ignore` when it no longer matches. Bumped when a pair is armed and
/// again as the FIRST step of a disarm, so a callback KHook still holds (its removal was
/// deferred, `Removal::Deferred`) is inert as soon as the disarm starts.
static ARMED_GEN: AtomicU64 = AtomicU64::new(0);

/// The hooks lock (main thread only, see [`InputHooks`]).
fn in_lock() -> MutexGuard<'static, Option<InputHooks>> {
    IN.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take the next arm generation: the value the new pair's callbacks capture. Called before the
/// first registration, so the caller never publishes a callback of an earlier arm as current.
fn next_generation() -> u64 {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel) + 1
}

/// Invalidate the pair that is being removed: every callback it captured answers `Ignore` from
/// here on. Called **before** the state is touched, so a deferred removal cannot run a body
/// that a later arm already owns.
fn retire_generation() {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel);
}

/// Whether a callback captured by an install of `generation` still acts for the current arm.
fn generation_current(generation: u64) -> bool {
    ARMED_GEN.load(Ordering::Acquire) == generation
}

/// `in_installed` (`mcp_bridge.c:4835`): both slots are hooked.
#[must_use]
pub fn installed() -> bool {
    in_lock().is_some()
}

/// The C's `g_inObj` (the recorded object; persists after uninstall, as the C's does).
#[must_use]
pub fn object() -> *mut c_void {
    IN_OBJ.load(Ordering::Acquire)
}

/// The view-angle target's `conflicts` counter since the last arm (`input.status`'s
/// `"conflicts"` member).
#[must_use]
pub fn in_conflicts() -> u64 {
    CONFLICTS.load(Ordering::Relaxed)
}

/// `in_uninstall` (`mcp_bridge.c:4856-4868`): drop our outstanding +1 deltas so no button
/// stays held after we are gone, reset the state, disarm the angles, remove both hooks. Only
/// from the main-thread drain ([`crate::wiring::pre_shutdown`] and the `input.uninstall` arm).
pub fn uninstall() {
    in_uninstall();
}

/// What [`teardown`] reports to [`crate::hooks`].
pub(crate) fn teardown() -> TeardownReport {
    let detail = match in_disarm() {
        Some(detail) => detail,
        None => "not armed (no-op)".to_string(),
    };
    TeardownReport::new("input", detail)
}

/// `in_table` (`mcp_bridge.c:4776`): the object's button table.
fn in_table(obj: *const c_void) -> *const *mut c_void {
    // SAFETY: `obj` is a live CCSGOInput whose layout carries the 64-slot table at the
    // generated offset; the arithmetic itself is unchecked (the C's same arithmetic).
    unsafe {
        (obj as *const u8)
            .byte_add(offset_of!(sys::CClientInput, m_pButtons))
            .cast()
    }
}

/// `in_btn_name` (`mcp_bridge.c:4777-4781`): the button's CInputValue name pointer, or null.
/// SAFETY of the caller's deref: the pointer is only read after `mem_ok` (below or at the
/// call site), exactly as the C reads it.
fn in_btn_name(btn: *mut c_void) -> *const u8 {
    if btn.is_null() || !mem_ok(btn) {
        return ptr::null();
    }
    // mcp_bridge.c:4779: btn + S2_OFF_ClientInputButton_t_m_InputValue = CInputValue, whose
    // first member (CInputValueBase::m_pszName) is the name pointer.
    // SAFETY: `mem_ok(btn)` above proves the button object readable, the offset is inside
    // the generated layout, and the read is the C's unguarded read (`:4779`).
    let n = unsafe {
        (btn as *const u8)
            .byte_add(offset_of!(sys::ClientInputButton_t, m_InputValue))
            .cast::<*const c_char>()
            .read_unaligned()
    };
    // mcp_bridge.c:4780
    if n.is_null() || !mem_ok(n.cast()) {
        return ptr::null();
    }
    n.cast()
}

/// `ieq` (`mcp_bridge.c:4782-4790`): ASCII-case-insensitive equality over raw bytes. The
/// scan of `a` is bounded by `b`'s NUL (the caller's bounded buffer), so no unbounded game
/// read is introduced over the C.
fn ieq(a: *const u8, b: &[u8]) -> bool {
    for (i, &y) in b.iter().enumerate() {
        let mut x = unsafe { *a.add(i) };
        if x == 0 {
            return false; // *a ended before *b
        }
        if x.is_ascii_uppercase() {
            x += 32;
        }
        let y = y.to_ascii_lowercase();
        if x != y {
            return false;
        }
    }
    // mcp_bridge.c:4789: `return *a == *b` — both must have ended (b's NUL is implicit).
    let last = unsafe { *a.add(b.len()) };
    last == 0
}

/// `in_find_bit` (`mcp_bridge.c:4794-4801`) over the given object's table: matches a button
/// by name AND requires its mask to be exactly `1 << i`. The moveup/movedown buttons carry
/// mask -1 (IN_ALL) and never match — recorded in the C comment, `:4791-4793`.
fn in_find_bit(obj: *const c_void, name: &[u8]) -> i32 {
    // SAFETY: `obj` is the recorded CCSGOInput (the install's `obj_is` check, the C's
    // unguarded table read through g_inObj, `:4795`).
    let t = in_table(obj);
    for i in 0..64usize {
        // SAFETY: the table is the object's own 64-slot member array (the generated
        // layout); the C reads `t[i]` unguarded (`:4797`).
        let btn = unsafe { *t.add(i) };
        let n = in_btn_name(btn);
        if n.is_null() {
            continue;
        }
        // SAFETY: `mem_ok` in `in_btn_name` proved the name's first byte readable; the mask
        // is the button's own first member (the C's `*(u64 *)t[i]`, `:4798`).
        let mask = unsafe { (btn as *const u64).read_unaligned() };
        if ieq(n, name) && mask == 1u64 << i {
            return i as i32;
        }
    }
    -1
}

/// The game call the frame-begin body makes per changed bit: `g_inApply(iv, 0, ±1.0f, 0.0f)`
/// (`mcp_bridge.c:4816`). SAFETY: `g_inApply` is the pinned `EFN_INPUTVALUE_APPLY` entry
/// (byte-verified at module load), set before the hooks were published — the C's typedef +
/// cast (`:4760`, `:4849`).
fn apply_delta(iv: *const c_void, delta: f32) {
    let apply: InApplyFn = unsafe { core::mem::transmute(IN_APPLY.load(Ordering::Relaxed)) };
    // SAFETY: the pinned function; `iv` is a live CInputValue's address inside the button
    // object the C passes (`:4816`).
    unsafe { apply(iv.cast_mut(), 0, delta, 0.0) };
}

/// `in_hook_frame_begin`'s body (`mcp_bridge.c:4803-4823`), over an already-locked state
/// (the C's `in_lock()..in_unlock()` critical section): release finished taps, then send
/// one ±1 `ApplyButtonDelta` per bit whose held state changed. The apply fn is a parameter
/// for the host tests; the production thunk passes the pinned `g_inApply` ([`apply_delta`]).
#[doc(hidden)]
pub fn frame_begin_locked(state: &mut InState, obj: *const c_void, apply: InApplyFn) {
    let mut want = state.want;
    for (i, tap) in state.tap.iter().enumerate() {
        if *tap != 0 {
            want |= 1u64 << i;
        }
    }
    // m3.15: a release+tap of a held bit takes this frame to go up; its tap starts on the next.
    let regap = core::mem::take(&mut state.regap);
    want &= !regap;
    let diff = want ^ state.applied;
    if diff != 0 {
        // SAFETY: `obj` is the hooked CCSGOInput; the C reads the table unguarded (`:4812`).
        let t = in_table(obj);
        for i in 0..64usize {
            let bit = 1u64 << i;
            if diff & bit == 0 {
                continue;
            }
            // SAFETY: the C skips a null `t[i]` (`:4815`) and reads the entry unguarded.
            let btn = unsafe { *t.add(i) };
            if btn.is_null() {
                continue;
            }
            // SAFETY: btn + m_InputValue is a live CInputValue inside the button (the C's
            // same arithmetic, `:4816`).
            let iv: *const c_void = unsafe {
                (btn as *const u8)
                    .byte_add(offset_of!(sys::ClientInputButton_t, m_InputValue))
                    .cast()
            };
            let delta = if want & bit != 0 { 1.0 } else { -1.0 };
            // SAFETY: the pinned function (production) or the test's recorder.
            unsafe { apply(iv.cast_mut(), 0, delta, 0.0) };
            state.applied ^= bit;
            IN_EDGES.fetch_add(1, Ordering::Relaxed); // il_inc(&g_inEdges), `:4818`
        }
    }
    // mcp_bridge.c:4809: the taps count down (the C folds this into the same loop that
    // takes the mask; the take above reads the pre-decrement values either way).
    for (i, tap) in state.tap.iter_mut().enumerate() {
        if *tap != 0 && regap & (1u64 << i) == 0 {
            *tap -= 1;
        }
    }
}

/// The frame-begin target's `pre` (`mcp_bridge.c:4804-4823`): run the button-delta body for
/// our object and answer [`Return::ignore`] — this call's outcome is never ours to change.
///
/// Runs inside the thunk: a generation read, an atomic read, one mutex, and the counters. A
/// foreign `this` (the slot is shared, so any object of the class can reach the thunk) is not
/// ours to touch, and neither is a callback whose pair was disarmed (`generation`).
fn frame_begin_pre(
    generation: u64,
    _call: &mut HookCall,
    this: *mut c_void,
    slot: sys::CSplitScreenSlot,
    rec: *mut c_void,
) -> Return<()> {
    let _ = rec;
    guard((), || {
        if !generation_current(generation) {
            // A pair this arm already retired: KHook may still call it (a deferred removal).
            return;
        }
        if this.is_null() || this != object() {
            return;
        }
        // mcp_bridge.c:4806: only the first split-screen sub-slot.
        if slot.m_Data != 0 {
            return;
        }
        {
            let mut state = lock_state();
            // SAFETY: `IN_APPLY` is the pinned `EFN_INPUTVALUE_APPLY` (set at install,
            // `:4849`, before the hook was published).
            let apply: InApplyFn = unsafe { core::mem::transmute(IN_APPLY.load(Ordering::Relaxed)) };
            frame_begin_locked(&mut state, this, apply);
        }
        IN_FRAMES.fetch_add(1, Ordering::Relaxed); // il_inc(&g_inFrames), `:4822`
    });
    Return::ignore(())
}

/// The view-angle target's `pre` (`mcp_bridge.c:4825-4833`): while the override is armed,
/// write the three angles and **supersede** the call with 1 (the game's own entry answers
/// false). `Ignore` everywhere else, so the original runs and its answer (0) stands.
fn angles_pre(
    generation: u64,
    call: &mut HookCall,
    this: *mut c_void,
    slot: sys::CSplitScreenSlot,
    curtime: f64,
    ang: *mut sys::QAngle,
) -> Return<u8> {
    let _ = curtime;
    guard(Return::ignore(0), || {
        if !generation_current(generation) {
            // A pair this arm already retired (a deferred removal still in KHook's hands):
            // it writes nothing, counts no tick and reports no conflict. It also leaves the
            // thread's note alone. KHook's order over a re-armed slot is the newest
            // registration's `pre` first, then the retired one still in its list, in the same
            // call (`detour.cpp:1477-1489`; the order this module's test
            // `a_stale_pre_does_not_hide_the_live_pairs_own_note` models), so writing a note
            // here would hide the live pair's `Superseded` note from its own `post` and lose a
            // real "after" conflict. The fix does not depend on that order: the note is keyed
            // by the live pair's generation, so only the live `post` can consume it whatever
            // position the retired pair takes.
            return Return::ignore(0);
        }
        // mcp_bridge.c:4827: the C reads `g_inAnglesOn` before `ang` and answers 0 otherwise.
        if this.is_null()
            || this != object()
            || slot.m_Data != 0
            || !IN_ANGLES_ON.load(Ordering::Relaxed)
            || ang.is_null()
        {
            set_pre_note(generation, PreNote::Ignored);
            return Return::ignore(0);
        }
        if !call.override_value_ptr().is_null() {
            // An earlier callback in this call already saved a value, and `detour.cpp:381-383`
            // keeps the FIRST among equal actions: ours may not be the one used. A neighbour
            // that answered `Handled` on this call saves a value too, so it counts here as a
            // value changer - the same reading save/restore makes.
            note_conflict(Side::Ahead);
            set_pre_note(generation, PreNote::SupersededAhead);
        } else {
            set_pre_note(generation, PreNote::Superseded);
        }
        // mcp_bridge.c:4828-4830: the three stores, from the game group's atomics - read the
        // same way the status reader reads them.
        // SAFETY: `ang` is the caller's live QAngle (the game's own `pAngles`, or the test's
        // buffer) - the C's three stores at `:4828-4830`.
        unsafe {
            (*ang).x = f32::from_bits(IN_ANGLES[0].load(Ordering::Relaxed));
            (*ang).y = f32::from_bits(IN_ANGLES[1].load(Ordering::Relaxed));
            (*ang).z = f32::from_bits(IN_ANGLES[2].load(Ordering::Relaxed));
        }
        IN_ANGLE_TICKS.fetch_add(1, Ordering::Relaxed); // il_inc(&g_inAngleTicks), `:4831`
        Return::supersede_with(1)
    })
}

/// The view-angle target's `post`, registered only while armed: does the call's final override
/// value still carry the 1 we saved? "after" is the report when it does not.
///
/// Only a call our own `pre` superseded on (this thread's note for this arm generation) is
/// checked: a call where `pre` answered `Ignore` - a foreign object, a non-zero sub-slot, the
/// override off, a null `ang` - is not a conflict even when another plugin's value is the
/// call's. A call `pre` already counted as "ahead" is not counted twice. A stale pair (the
/// generation no longer matches) answers `Ignore` without touching the note.
fn angles_post(
    generation: u64,
    call: &mut HookCall,
    this: *mut c_void,
    slot: sys::CSplitScreenSlot,
    curtime: f64,
    ang: *mut sys::QAngle,
) -> Return<u8> {
    let _ = (curtime, ang);
    guard(Return::ignore(0), || {
        if !generation_current(generation) {
            return Return::ignore(0);
        }
        // `Superseded` is the one case a final value that is not ours is a conflict: `Ignored`
        // means the call was never ours, `SupersededAhead` was already counted in `pre`.
        if take_pre_note(generation) != Some(PreNote::Superseded) {
            return Return::ignore(0);
        }
        if this.is_null() || this != object() || slot.m_Data != 0 {
            return Return::ignore(0);
        }
        let value = call.override_value_ptr();
        if !value.is_null() {
            // SAFETY: KHook's override slot holds the return value for the signature this hook
            // was registered with - a one-byte `bool`, declared `u8` here - and points into its
            // per-call storage, valid for the duration of the callback
            // (`khook.hpp:744-752`, `detour.cpp:373-405`).
            let saved = unsafe { value.cast::<u8>().read() };
            if saved != 1 {
                note_conflict(Side::After);
            }
        }
        Return::ignore(0)
    })
}

/// `in_install` (`mcp_bridge.c:4836-4855`): resolve `g_pInput`, identity-check it, hook the two
/// slots on the pinned table. Idempotent on an already-armed hook.
///
/// The two registrations are all-or-nothing: when the second is refused, the first is removed
/// again before the error is answered, so `installed()` never reports a half-armed input lane.
fn in_install() -> Result<(), OpErr> {
    if installed() {
        return Ok(());
    }
    // mcp_bridge.c:4839: `void **obj = *(void ***)(cb + CG_INPUT_PTR);` — no `mod_ok` guard
    // in the C; the NEED_INPUT gate (client + engine2 verified) makes the global readable,
    // so the same read is valid here.
    // SAFETY: the gate verified client.dll byte-for-byte before this body ran, so
    // base + CG_INPUT_PTR is the pinned global inside the image (the C reads it unguarded).
    let obj = unsafe {
        ((modules_base(pins::PIN_CLIENT) + pins::CG_INPUT_PTR.value) as *const *mut c_void)
            .read_unaligned()
    };
    // mcp_bridge.c:4840-4843: KHook never rewrites an object's vptr, so this is the C's check,
    // with its pinned slots read through `FindOriginalVirtual` when KHook holds one.
    if !obj_is(obj, pins::PIN_CLIENT, pins::CV_CSGOINPUT_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "g_pInput is not the pinned CCSGOInput (vtable)",
        ));
    }
    in_arm(obj)
}

/// One registration error, in the shape the op answers: `-32017 PrerequisiteFailed` with the
/// slot and KHook's reason.
///
/// [`s2sdk::hook::Error::GateBusy`] is the retryable one - a registration made from inside a
/// thunk (the GameFrame tick drains the queue on its own stack, `crate::gameframe`) takes the
/// hook gate with `try_lock` only, and a gate another thread holds at that moment is
/// `GateBusy` - and it answers the `blockedOn` retry hint rather than a reason, the way
/// save/restore answers a retryable refusal. [`s2sdk::hook::Error::InThunk`] can no longer come
/// out of a registration; it is mapped the same way, because a thunk-time refusal always means
/// "retry outside the thunk".
fn registration_error(what: &str, error: &s2sdk::hook::Error, extra: &str) -> OpErr {
    let reason = format!("{what}: {error}{extra}");
    if matches!(
        error,
        s2sdk::hook::Error::GateBusy | s2sdk::hook::Error::InThunk
    ) {
        return OpErr::with_data(-32017, "PrerequisiteFailed", |db| {
            db.add(b"{\"blockedOn\":\"khook: retry next tick\",\"reason\":");
            db.addjson(reason.as_bytes());
            db.add(b"}");
        });
    }
    OpErr::new(-32017, "PrerequisiteFailed", &reason)
}

/// Arm both slots on the pinned table for `obj` (`in_install`'s enable path after the identity
/// check).
///
/// Kept separate from `in_install` so the resolution and identity check stay exactly the C's,
/// and so the tests can arm against a fabricated object without a loaded client.dll.
fn in_arm(obj: *mut c_void) -> Result<(), OpErr> {
    // The table the class's vptr points at (the C's `base + CV_CSGOINPUT_VT`), for KHook's
    // `SetupVirtualHook`: the pinned CCSGOInput vftable inside the verified image.
    let vt = (modules_base(pins::PIN_CLIENT) + pins::CV_CSGOINPUT_VT.value) as *mut *mut c_void;
    // mcp_bridge.c:4849: g_inApply = (InApplyFn)(eb + EFN_INPUTVALUE_APPLY)
    IN_APPLY.store(
        (modules_base(pins::PIN_ENGINE2) + pins::EFN_INPUTVALUE_APPLY.value) as *mut c_void,
        Ordering::Release,
    );
    let mut slot = in_lock();
    if slot.is_some() {
        return Ok(());
    }
    IN_OBJ.store(obj, Ordering::Release);
    CONFLICTS.store(0, Ordering::Relaxed);
    CONFLICT_REPORTED.store(false, Ordering::Release);
    // A new arm: any pair still waiting for its deferred removal is now inert, and the
    // callbacks about to be published carry the new generation.
    let generation = next_generation();

    let frame_pre: Vec<in_frame_begin_hook::PreCb> = vec![Arc::new(
        move |call: &mut HookCall, this: *mut c_void, slot: sys::CSplitScreenSlot, rec: *mut c_void| {
            frame_begin_pre(generation, call, this, slot, rec)
        },
    )];
    // SAFETY: `vt` is the pinned `CV_CSGOINPUT_VT` table inside client.dll (the module gate
    // verified the module and `obj_is` identity-checked the object) and the index is the
    // generated slot of `unk3021`, so the entry has exactly the declared `void
    // f(this, CSplitScreenSlot, void*)` signature. Installed from the main thread with
    // `SetupMode::Sync`; the target is not on this stack.
    let frame_begin = match unsafe {
        in_frame_begin_hook::setup_virtual(
            vt,
            sys::CClientInput_slots::unk3021 as i32,
            frame_pre,
            Vec::new(),
            None,
            SetupMode::Sync,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            IN_OBJ.store(ptr::null_mut(), Ordering::Release);
            return Err(registration_error("CClientInput::unk3021", &error, ""));
        }
    };
    let angles_pre_cbs: Vec<in_angles_hook::PreCb> = vec![Arc::new(
        move |call: &mut HookCall,
              this: *mut c_void,
              slot: sys::CSplitScreenSlot,
              curtime: f64,
              ang: *mut sys::QAngle| { angles_pre(generation, call, this, slot, curtime, ang) },
    )];
    let angles_post_cbs: Vec<in_angles_hook::PostCb> = vec![Arc::new(
        move |call: &mut HookCall,
              this: *mut c_void,
              slot: sys::CSplitScreenSlot,
              curtime: f64,
              ang: *mut sys::QAngle| { angles_post(generation, call, this, slot, curtime, ang) },
    )];
    // SAFETY: as above, for the generated slot of `unk3023` and its declared
    // `u8 f(this, CSplitScreenSlot, double, QAngle*)` signature (the game's `bool`, declared
    // `u8`; see the macro's docs).
    let angles = match unsafe {
        in_angles_hook::setup_virtual(
            vt,
            sys::CClientInput_slots::unk3023 as i32,
            angles_pre_cbs,
            angles_post_cbs,
            None,
            SetupMode::Sync,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            // No half-armed input lane: the first registration goes away again, and the
            // removal is safe here (this is the main-thread drain, not a callback). The pair
            // just retired first, so even a deferred removal of it cannot run its body.
            retire_generation();
            let detail = match frame_begin.remove_sync() {
                Ok(removal) => format!("removed ({})", removal_name(Ok(removal))),
                Err(error) => format!("remove failed: {error}"),
            };
            IN_OBJ.store(ptr::null_mut(), Ordering::Release);
            return Err(registration_error(
                "CClientInput::unk3023",
                &error,
                &format!("; CClientInput::unk3021 was {detail}"),
            ));
        }
    };
    *slot = Some(InputHooks { frame_begin, angles });
    log::line("input: hook installed (KHook CCSGOInput slots unk3021/unk3023)"); // :4853
    Ok(())
}

/// Disarm: the held deltas, the state, the angles, then both hooks in the C's order (slot 32
/// first, then 30). Answers what happened for the teardown log line, or `None` when nothing
/// was armed.
fn in_disarm() -> Option<String> {
    // The C's order is the order the state is touched in (`:4858-4866`): the deltas are
    // released while the hook is still registered, and only then is it removed. So the check,
    // the release and the take are three steps, not one - `installed()` still answers true
    // during the release. Arming and teardown both run on the main thread (that is what keeps
    // the window between the check and the take unobservable).
    if in_lock().is_none() {
        return None;
    }
    // FIRST: retire the generation. A callback KHook still holds - the removal below can be
    // deferred when this runs from a thunk (lane E/A2) - is inert from here on, so it can
    // neither release a delta twice nor run the body of a later arm.
    retire_generation();
    // in_lock, `:4858`: release one -1 delta per bit we still own (`:4860-4863`).
    {
        let mut state = lock_state();
        let obj = object();
        if !obj.is_null() {
            // SAFETY: the hook was armed, so the recorded object is the live CCSGOInput (the
            // C's unguarded read through g_inObj, `:4860`).
            let t = in_table(obj);
            for i in 0..64usize {
                // mcp_bridge.c:4861-4862: one -1 delta per bit we still own; no edge counted.
                if state.applied >> i & 1 == 0 {
                    continue;
                }
                // SAFETY: the C reads `t[i]` and skips a null (`:4862`).
                let btn = unsafe { *t.add(i) };
                if btn.is_null() {
                    continue;
                }
                // SAFETY: btn + m_InputValue is a live CInputValue (the C's arithmetic, `:4862`).
                let iv = unsafe {
                    (btn as *const u8)
                        .byte_add(offset_of!(sys::ClientInputButton_t, m_InputValue))
                        .cast()
                };
                apply_delta(iv, -1.0);
            }
        }
        state.applied = 0;
        state.want = 0;
        // The tap frame counters belong to the disarmed pair (lane B3 round 2: a stale pair
        // must not keep a tapped button down, so they do not survive a disarm).
        state.tap = [0; 64];
        state.regap = 0;
        // in_unlock, `:4865` (the angles reset rides the same critical section, `:4864`)
        IN_ANGLES_ON.store(false, Ordering::Relaxed);
    }
    let InputHooks {
        frame_begin,
        angles,
    } = in_lock().take()?;
    // The target is not on this stack (main-thread drain or `PreShutdown`), so a synchronous
    // removal is the right one; the C's order is the view-angle slot first (`:4858-4866`).
    let angles = angles.remove_sync();
    let frame = frame_begin.remove_sync();
    Some(format!(
        "removed 2 hooks (unk3023: {}, unk3021: {})",
        removal_name(angles),
        removal_name(frame)
    ))
}

/// `in_uninstall` (`mcp_bridge.c:4856-4868`): [`in_disarm`] plus the C's log line.
fn in_uninstall() {
    if in_disarm().is_some() {
        log::line("input: hook removed"); // :4867
    }
}

/// The [`Removal`] outcome in the log line's words.
fn removal_name(removal: Result<Removal, s2sdk::hook::Error>) -> String {
    match removal {
        Ok(Removal::Removed) => "Removed".to_string(),
        Ok(Removal::AlreadyGone) => "AlreadyGone".to_string(),
        Ok(Removal::ReconciledLost) => "ReconciledLost".to_string(),
        Ok(Removal::Deferred) => "Deferred".to_string(),
        Err(error) => format!("remove failed: {error}"),
    }
}

/// The reads of `in_status_json` (`mcp_bridge.c:4898-4927`) over plain values — the pure
/// builder the host tests drive.
pub struct InStatusRead {
    /// `in_installed()`.
    pub installed: bool,
    /// `g_inWant`.
    pub want: u64,
    /// `g_inApplied`.
    pub applied: u64,
    /// `g_inAnglesOn`.
    pub angles_on: bool,
    /// `g_inAngles`.
    pub angles: [f32; 3],
    /// `(int)g_inFrames`.
    pub frames: i32,
    /// `(int)g_inEdges`.
    pub edges: i32,
    /// `(int)g_inAngleTicks`.
    pub angle_ticks: i32,
    /// The view-angle target's conflict counter since the last arm (`"conflicts"`; a wire
    /// addition over the C's object).
    pub conflicts: u64,
}

/// A game NUL-terminated string copied into a bounded local for [`Sb::addjson`]: the C hands
/// the raw pointer to `sb_addjson`, which reads until NUL (`:4908`); the Rust copy keeps the
/// same bytes with a bound (no engine button name is near it). `None` for a null pointer
/// (an empty string stays `Some(empty)`).
fn copy_name(p: *const u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if p.is_null() {
        return None;
    }
    for i in 0..256usize {
        // SAFETY: the caller's `mem_ok` (via `in_btn_name`) proved the first byte readable;
        // this is the C's read-until-NUL (`:4908`).
        let b = unsafe { *p.add(i) };
        if b == 0 {
            break;
        }
        out.push(b);
    }
    Some(out)
}

/// `in_status_json` (`mcp_bridge.c:4898-4928`) over the given reads and button table.
///
/// The trailing `"conflicts":N` member is this build's addition over the C (see the module
/// docs); every other byte and the member order are the C's.
pub fn in_status_json_in(read: &InStatusRead, obj: *const c_void, out: &mut Sb<'_>) {
    out.add(b"{\"installed\":");
    out.add(if read.installed { b"true" } else { b"false" }); // :4899
    out.add(b",\"held\":["); // :4900
    let mut first = true;
    // mcp_bridge.c:4902: t = g_inObj ? in_table(g_inObj) : 0
    let t = (!obj.is_null()).then(|| in_table(obj));
    if let Some(t) = t {
        for i in 0..64usize {
            if (read.want | read.applied) >> i & 1 == 0 {
                continue;
            }
            // SAFETY: the C reads `t[i]` unguarded (`:4905`).
            let n = copy_name(in_btn_name(unsafe { *t.add(i) }));
            if !first {
                out.add(b",");
            }
            first = false;
            // sb_addjson(out, n ? n : "?"), `:4908`
            out.addjson(match &n {
                Some(v) => v.as_slice(),
                None => b"?",
            });
        }
    }
    out.add(b"],\"angles\":"); // :4910
    if read.angles_on {
        // mcp_bridge.c:4912-4914
        out.add(b"[");
        out.addf(read.angles[0]);
        out.add(b",");
        out.addf(read.angles[1]);
        out.add(b",");
        out.addf(read.angles[2]);
        out.add(b"]");
    } else {
        out.add(b"null");
    }
    out.add(b",\"frames\":");
    out.addi(i64::from(read.frames)); // (int)g_inFrames, :4915
    out.add(b",\"edges\":");
    out.addi(i64::from(read.edges)); // :4916
    out.add(b",\"angleTicks\":");
    out.addi(i64::from(read.angle_ticks)); // :4917
    out.add(b",\"buttons\":["); // :4918
    first = true;
    if let Some(t) = t {
        for i in 0..64usize {
            // SAFETY: the C reads `t[i]` unguarded (`:4921`).
            let n = match copy_name(in_btn_name(unsafe { *t.add(i) })) {
                Some(n) => n,
                None => continue,
            };
            if !first {
                out.add(b",");
            }
            first = false;
            out.addjson(&n); // :4925
        }
    }
    // A wire addition over the C's object: the view-angle target's conflict counter (see the
    // module docs). Appended last so every C member keeps its position and bytes.
    out.add(b"],\"conflicts\":");
    out.addnum(read.conflicts);
    out.add(b"}");
}

/// The live [`in_status_json_in`]: the same reads off the hook's real state.
///
/// The C reads `g_inWant` / `g_inApplied` / `g_inAngles` without its lock here (`:4902`,
/// `:4911-4913`); the Rust state is behind the mutex, so this takes it for the mask read —
/// the same bytes for any interleaving (memory-safety fix, no wire change).
fn in_status_json(out: &mut Sb<'_>) {
    let read = {
        let state = lock_state();
        InStatusRead {
            installed: installed(),
            want: state.want,
            applied: state.applied,
            angles_on: IN_ANGLES_ON.load(Ordering::Relaxed),
            angles: [
                f32::from_bits(IN_ANGLES[0].load(Ordering::Relaxed)),
                f32::from_bits(IN_ANGLES[1].load(Ordering::Relaxed)),
                f32::from_bits(IN_ANGLES[2].load(Ordering::Relaxed)),
            ],
            frames: IN_FRAMES.load(Ordering::Relaxed) as i32,
            edges: IN_EDGES.load(Ordering::Relaxed) as i32,
            angle_ticks: IN_ANGLE_TICKS.load(Ordering::Relaxed) as i32,
            conflicts: in_conflicts(),
        }
    };
    in_status_json_in(&read, object(), out);
}

/// One string in a JSON array value of `key` -> `cb(state, bit, ctx)`; returns the count.
/// A malformed list or a bad name is `Err(the bad name's bytes)` (`mcp_bridge.c:4870-4893`).
fn in_each_name(
    params: Option<&[u8]>,
    key: &[u8],
    obj: *const c_void,
    state: &mut InState,
    ctx: i32,
    mut cb: impl FnMut(&mut InState, i32, i32) -> bool,
) -> Result<usize, Vec<u8>> {
    // mcp_bridge.c:4873-4875
    let Some(start) = params.and_then(|p| json_find(p, key)) else {
        return Ok(0);
    };
    let params = params.unwrap_or(&[]);
    // mcp_bridge.c:4875: `*p != '['` reads the NUL when the value is missing
    // (`json_find` may answer `start == len`, e.g. `{"press":`), which is the C's
    // "unknown button or bad list: ?"; `get` keeps that a validation error, not a panic.
    if params.get(start) != Some(&b'[') {
        return Err(Vec::new());
    }
    let mut p = start + 1;
    let mut n = 0usize;
    loop {
        // mcp_bridge.c:4879
        while matches!(params.get(p), Some(b' ') | Some(b',')) {
            p += 1;
        }
        if matches!(params.get(p), None | Some(b']') | Some(0)) {
            break;
        }
        if params.get(p) != Some(&b'"') {
            return Err(Vec::new());
        }
        // mcp_bridge.c:4882-4886: char name[48], copied without escape processing.
        let mut name = [0u8; NAME_CAP];
        let mut k = 0usize;
        p += 1;
        while matches!(params.get(p), Some(&b) if b != 0 && b != b'"') && k + 1 < NAME_CAP {
            name[k] = params[p];
            k += 1;
            p += 1;
        }
        if params.get(p) == Some(&b'"') {
            p += 1;
        }
        // mcp_bridge.c:4887-4891
        let bit = in_find_bit(obj, cstr_tail(&name));
        if bit < 0 {
            return Err(name[..k].to_vec());
        }
        if !cb(state, bit, ctx) {
            return Err(name[..k].to_vec());
        }
        n += 1;
    }
    Ok(n)
}

/// The pure `input.set` core (`mcp_bridge.c:4939-4965`), over the given button-table object
/// (the C reads `g_inObj`) and the already-installed hook — the host tests drive it with a
/// fake table; the production [`op_input_installed`] passes the recorded object. No result
/// is written here (the arm's `in_status_json` tail does, `:4973`).
#[doc(hidden)]
pub fn input_set_installed(params: Option<&[u8]>, obj: *const c_void) -> Result<(), OpErr> {
    let mut frames: i64 = 1;
    if let Some(p) = params {
        // mcp_bridge.c:4942
        if let Some(f) = json_int(p, b"frames") {
            frames = f;
        }
    }
    // mcp_bridge.c:4943-4945 (the C's two-sided clamp)
    frames = frames.clamp(1, 1000);
    let fr = frames as i32;
    // mcp_bridge.c:4946-4949
    let has_ang = params.and_then(|p| json_float3(p, b"angles"));
    let ap = params.and_then(|p| json_find(p, b"angles"));
    // mcp_bridge.c:4949: ap[0..4] == "null" (the C's NUL-bounded reads).
    let ang_off = ap
        .map(|p| params.unwrap_or(&[])[p..].starts_with(b"null"))
        .unwrap_or(false);
    // mcp_bridge.c:4950
    if ap.is_some() && has_ang.is_none() && !ang_off {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "angles must be [pitch,yaw,roll] or null",
        ));
    }
    // mcp_bridge.c:4951-4959: one critical section; a failed list rolls `g_inWant` back
    // (the taps are NOT rolled back — the C resets only `g_inWant`, `:4956`).
    let ok = {
        let mut state = lock_state();
        let save_want = state.want;
        let save_regap = state.regap;
        let mut released_held = 0u64;
        let mut tapped = 0u64;
        let outcome = (|| -> Result<(), Vec<u8>> {
            // mcp_bridge.c:4894, 4895, 4896 callbacks.
            in_each_name(params, b"press", obj, &mut state, 0, |st, bit, _| {
                st.want |= 1u64 << bit;
                true
            })?;
            in_each_name(params, b"release", obj, &mut state, 0, |st, bit, _| {
                st.want &= !(1u64 << bit);
                st.tap[bit as usize] = 0;
                st.regap &= !(1u64 << bit);
                released_held |= st.applied & (1u64 << bit);
                true
            })?;
            in_each_name(params, b"tap", obj, &mut state, fr, |st, bit, frames| {
                st.tap[bit as usize] = frames as u16;
                tapped |= 1u64 << bit;
                true
            })?;
            Ok(())
        })();
        let ok = outcome.is_ok();
        if !ok {
            state.want = save_want;
            state.regap = save_regap;
        } else if let Some(a) = has_ang {
            // mcp_bridge.c:4957
            IN_ANGLES[0].store(a[0].to_bits(), Ordering::Relaxed);
            IN_ANGLES[1].store(a[1].to_bits(), Ordering::Relaxed);
            IN_ANGLES[2].store(a[2].to_bits(), Ordering::Relaxed);
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
        } else if ang_off {
            // mcp_bridge.c:4958
            IN_ANGLES_ON.store(false, Ordering::Relaxed);
        }
        if ok {
            // m3.15: only a bit we own (+1 sent) needs the gap to be seen as pressed again
            state.regap |= released_held & tapped;
        }
        (ok, outcome.err())
    };
    if !ok.0 {
        // mcp_bridge.c:4960-4964: the reason built in the C's 96-byte scratch.
        let bad = ok.1.unwrap_or_default();
        let mut msg = [0u8; 96];
        let len = {
            let mut mb = Sb::new(&mut msg);
            mb.add(b"unknown button or bad list: ");
            mb.add(if bad.is_empty() { b"?" } else { &bad });
            mb.as_cstr().len()
        };
        return Err(OpErr::with_data(-32602, "InvalidParams", |db| {
            db.add(b"{\"reason\":");
            db.addjson(&msg[..len]);
            db.add(b"}");
        }));
    }
    Ok(())
}

/// The C's `op_input` body after the install gate (`mcp_bridge.c:4939-4974`), over the given
/// button-table object. Exposed for the host tests (`#[doc(hidden)]`); the production
/// [`op_input`] passes the recorded object.
#[doc(hidden)]
pub fn op_input_installed(
    method: &[u8],
    params: Option<&[u8]>,
    obj: *const c_void,
    out: &mut Sb<'_>,
) -> Result<(), OpErr> {
    // mcp_bridge.c:4939
    if method == b"input.set" {
        input_set_installed(params, obj)?;
    } else if method == b"input.clear" {
        // mcp_bridge.c:4966-4971
        let mut state = lock_state();
        state.want = 0;
        state.tap = [0; 64];
        state.regap = 0;
        IN_ANGLES_ON.store(false, Ordering::Relaxed);
    }
    // mcp_bridge.c:4973
    in_status_json(out);
    Ok(())
}

/// The C's `op_input` (`mcp_bridge.c:4932-4975`), with the install split off: only
/// `input.set` arms the hooks (`NEED_KHOOK`'s row). `input.status` / `input.clear` only read or
/// reset state, so they keep answering after a Metamod unload and can never install a hook.
fn op_input(method: &[u8], params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    // mcp_bridge.c:4933-4937
    if method == b"input.uninstall" {
        in_uninstall();
        in_status_json(out);
        return Ok(());
    }
    // mcp_bridge.c:4938: `if (!in_install(e)) return 0;` — the arming row only.
    if method == b"input.set" {
        in_install()?;
    }
    op_input_installed(method, params, object(), out)
}

/// The dispatch arm's tail (`mcp_bridge.c:6043-6055`): this arm has NO overflow branch
/// (unlike the sound arm's `:6030-6038`) — a too-large status is emitted truncated, exactly
/// as the C emits it. Also testable through [`in_status_json_in`]-driven builders.
fn input(r: &Request) -> String {
    let mut buf = vec![0u8; INPUT_BUF]; // mcp_bridge.c:6047
    let mut sb = Sb::new(&mut buf);
    let outcome = op_input(r.method.as_bytes(), r.params.as_deref().map(str::as_bytes), &mut sb);
    match outcome {
        Err(e) if e.data.is_empty() => control::error(Some(&r.id), e.code, e.name),
        Err(e) => control::error_ext(Some(&r.id), e.code, e.name, &e.data),
        Ok(()) => control::result_ok(Some(&r.id), &String::from_utf8_lossy(sb.as_cstr())),
    }
}

/// The group's handler table (the C table's `input.*` rows, `mcp_bridge.c:1064-1067`).
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("input.set", input),
    ("input.clear", input),
    ("input.status", input),
    ("input.uninstall", input),
];

#[cfg(test)]
mod tests {
    //! The hook's own tests: a mock `KHookApi` that simulates KHook's per-call value fold
    //! (`detour.cpp:746-990`) well enough to drive the generated thunks (the harness lives in
    //! [`crate::hooks::mock_khook`], shared with the sound lane), the §13 conflict pair, and
    //! the two negative paths (no API, second slot refused).

    use super::*;
    use crate::hooks::mock_khook::{self, Call};

    /// A fake CCSGOInput: only the address and the button table are used by the bodies. `mem`
    /// is the generated `m_pButtons` layout (64 pointers), each a non-null fake button whose
    /// `m_InputValue` the apply recorder only ever compares by address.
    fn fake_object() -> *mut c_void {
        static MEM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        let mem = MEM.get_or_init(|| {
            let mut mem = vec![0u8; offset_of!(sys::CClientInput, m_pButtons) + 64 * 8];
            for i in 0..64usize {
                let btn = 0x1000_0000usize + i * 0x100;
                let at = offset_of!(sys::CClientInput, m_pButtons) + i * 8;
                mem[at..at + 8].copy_from_slice(&(btn as u64).to_ne_bytes());
            }
            mem
        });
        mem.as_ptr().cast_mut().cast()
    }

    /// The `iv` address the uninstall's release passes for bit 0 (the C's
    /// `btn + m_InputValue` arithmetic, `mcp_bridge.c:4862`).
    fn btn_iv(bit: usize) -> isize {
        (0x1000_0000usize + bit * 0x100 + offset_of!(sys::ClientInputButton_t, m_InputValue)) as isize
    }

    /// Which slot each mock registration was for, in the generated indices.
    fn setup_indices(mock: &mock_khook::Mock) -> Vec<i32> {
        mock.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::SetupVirtual { index, .. } => Some(index),
                Call::Remove { .. } => None,
            })
            .collect()
    }

    /// Install registers both slots on the pinned table, synchronously, with the generated
    /// indices; uninstall removes them in the order 32 then 30, synchronously, after the held
    /// deltas are released (the apply recorder runs while both hooks are still armed).
    #[test]
    fn install_and_uninstall_use_the_pinned_slots_in_the_c_order() {
        mock_khook::with_mock("input-install", |mock, _scratch| {
            let obj = fake_object();
            in_arm(obj).expect("the mock accepts both registrations");
            assert!(installed());
            assert_eq!(object(), obj, "the recorded object");
            let indices = setup_indices(mock);
            assert_eq!(
                indices,
                vec![
                    sys::CClientInput_slots::unk3021 as i32,
                    sys::CClientInput_slots::unk3023 as i32,
                ],
                "the pinned CV_CSGOINPUT_VT slots, generated indices"
            );
            for call in mock.calls() {
                let Call::SetupVirtual {
                    vtable,
                    r#async,
                    stack_size,
                    ..
                } = call
                else {
                    continue;
                };
                let pinned = (modules_base(pins::PIN_CLIENT) + pins::CV_CSGOINPUT_VT.value) as usize;
                assert_eq!(vtable, pinned, "the pinned CV_CSGOINPUT_VT table");
                assert!(!r#async, "SetupMode::Sync on the main thread");
                assert!(stack_size > 0, "the computed stack size");
            }

            // Hold a +1 delta on bit 0 and check the release happens while the hooks are still
            // in place (the C's order: deltas, state, angles, then the removals).
            {
                let mut state = lock_state();
                state.want = 1;
                state.applied = 1;
                state.tap = [0; 64];
                state.regap = 0;
            }
            static SEEN: Mutex<Vec<(bool, usize)>> = Mutex::new(Vec::new());
            SEEN.lock().unwrap().clear();
            unsafe extern "C" fn record(_iv: *mut c_void, _slot: i32, delta: f32, _frac: f32) {
                SEEN.lock()
                    .unwrap()
                    .push((installed(), mock_khook::removal_count()));
                assert_eq!(delta, -1.0, "the release is a -1");
            }
            IN_APPLY.store(record as InApplyFn as *mut c_void, Ordering::Release);
            in_uninstall();
            assert!(!installed());
            let seen = SEEN.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "one -1 per held bit");
            assert!(seen[0].0, "the release ran while the hooks were still armed");
            assert_eq!(seen[0].1, 0, "and before the first removal");
            IN_APPLY.store(ptr::null_mut(), Ordering::Release);

            let removals = mock.removals_by_index();
            assert_eq!(
                removals,
                vec![
                    (sys::CClientInput_slots::unk3023 as i32, false),
                    (sys::CClientInput_slots::unk3021 as i32, false),
                ],
                "slot 32 first, then slot 30, both synchronous"
            );
        });
    }

    /// Slot 32, our object, angles on: writes `*ang`, counts the tick, `Supersede(1)`. A foreign
    /// `this` answers `Ignore` and leaves `*ang` untouched.
    #[test]
    fn angles_pre_supersedes_for_our_object_and_ignores_a_foreign_one() {
        mock_khook::with_mock("input-angles", |mock, _scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            IN_ANGLES[0].store(11.0f32.to_bits(), Ordering::Relaxed);
            IN_ANGLES[1].store(22.0f32.to_bits(), Ordering::Relaxed);
            IN_ANGLES[2].store(33.5f32.to_bits(), Ordering::Relaxed);
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
            let ticks = IN_ANGLE_TICKS.load(Ordering::Relaxed);

            let mut ang = sys::QAngle {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            };
            let (action, value) = trampoline_angles(mock, obj, 0, &mut ang);
            assert_eq!(action, s2sdk::hook::Action::Supersede, "we own this call");
            assert_eq!(value, 1, "the game reads the low byte: true");
            assert_eq!((ang.x, ang.y, ang.z), (11.0, 22.0, 33.5));
            assert_eq!(IN_ANGLE_TICKS.load(Ordering::Relaxed), ticks + 1);

            // A foreign object of the class reached the shared slot.
            let mut foreign_ang = sys::QAngle {
                x: 9.0,
                y: 9.0,
                z: 9.0,
            };
            let (action, value) = trampoline_angles(mock, 0x1234 as *mut c_void, 0, &mut foreign_ang);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(value, 0, "the original's own answer (false)");
            assert_eq!((foreign_ang.x, foreign_ang.y, foreign_ang.z), (9.0, 9.0, 9.0));
            assert_eq!(IN_ANGLE_TICKS.load(Ordering::Relaxed), ticks + 1, "no tick");

            // A non-zero split-screen sub-slot is not the hooked one (`:4827`).
            let mut slot_ang = sys::QAngle {
                x: 4.0,
                y: 4.0,
                z: 4.0,
            };
            let (action, value) = trampoline_angles(mock, obj, 1, &mut slot_ang);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(value, 0);
            assert_eq!((slot_ang.x, slot_ang.y, slot_ang.z), (4.0, 4.0, 4.0));

            // A null `ang` is the C's "nothing to write" (`:4827`): Ignore, no tick.
            let (action, value) = trampoline_angles_null_ang(mock, obj);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(value, 0);
            assert_eq!(IN_ANGLE_TICKS.load(Ordering::Relaxed), ticks + 1);

            // Angles off: Ignore for our object too.
            IN_ANGLES_ON.store(false, Ordering::Relaxed);
            let (action, value) = trampoline_angles(mock, obj, 0, &mut ang);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(value, 0);
            in_uninstall();
        });
    }

    /// Slot 30 runs the frame body for our object (the counters move, the delta goes out) and
    /// answers `Ignore` - the original still runs and answers.
    #[test]
    fn frame_begin_pre_runs_the_body_and_answers_ignore() {
        mock_khook::with_mock("input-frame", |mock, _scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            {
                let mut state = lock_state();
                state.want = 1;
                state.applied = 0;
                state.tap = [0; 64];
                state.regap = 0;
            }
            IN_FRAMES.store(0, Ordering::Relaxed);
            IN_EDGES.store(0, Ordering::Relaxed);
            static APPLIED: Mutex<Vec<isize>> = Mutex::new(Vec::new());
            APPLIED.lock().unwrap().clear();
            unsafe extern "C" fn record(iv: *mut c_void, _slot: i32, delta: f32, _frac: f32) {
                assert_eq!(delta, 1.0);
                APPLIED.lock().unwrap().push(iv as isize);
            }
            IN_APPLY.store(record as InApplyFn as *mut c_void, Ordering::Release);

            let action = trampoline_frame_begin(mock, obj, 0);
            assert_eq!(action, s2sdk::hook::Action::Ignore, "we never change this call");
            assert_eq!(*APPLIED.lock().unwrap(), vec![btn_iv(0)], "the C's iv arithmetic");
            assert_eq!(IN_EDGES.load(Ordering::Relaxed), 1);
            assert_eq!(IN_FRAMES.load(Ordering::Relaxed), 1);
            // Slot 1 (the split-screen sub-slot) does nothing.
            let action = trampoline_frame_begin(mock, obj, 1);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(IN_FRAMES.load(Ordering::Relaxed), 1, "the C's slot check");
            // A foreign object: Ignore, and the body does not run.
            let action = trampoline_frame_begin(mock, 0x1234 as *mut c_void, 0);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(IN_FRAMES.load(Ordering::Relaxed), 1);

            {
                let mut state = lock_state();
                state.want = 0;
                state.applied = 0;
            }
            IN_APPLY.store(ptr::null_mut(), Ordering::Release);
            in_uninstall();
        });
    }

    /// §13: a second value changer ahead is counted and reported once per arm (the log line
    /// is rate-limited, the counter is not).
    #[test]
    fn a_competing_value_changer_is_reported_once_and_counted() {
        // The log line is read back from a scratch file under `target/`: `log::line` is the
        // only writer, so a count of the marker in this file is a count of the lines written.
        mock_khook::with_mock("input-conflict", |mock, scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
            let mut ang = sys::QAngle {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            };
            // The earlier callback already saved 0 for this call (`detour.cpp:381-383`: among
            // equal actions the first saved wins).
            mock.seed(vec![0u8]);
            let (action, value) = trampoline_angles(mock, obj, 0, &mut ang);
            assert_eq!(action, s2sdk::hook::Action::Supersede, "our answer is still ours");
            assert_eq!(value, 0, "but the earlier changer's value is the call's");
            assert_eq!(in_conflicts(), 1, "one conflict for the call, not two");
            assert!(CONFLICT_REPORTED.load(Ordering::Acquire));

            // A second conflicting call counts, but writes no second line.
            mock.seed(vec![0u8]);
            let _ = trampoline_angles(mock, obj, 0, &mut ang);
            assert_eq!(in_conflicts(), 2);

            let marker = "khook conflict: CClientInput::unk3023 ahead";
            assert_eq!(
                scratch.count(marker),
                1,
                "exactly one rate-limited line per arm: {}",
                scratch.text()
            );
            assert!(
                std::fs::read_dir(scratch.xdg())
                    .expect("the scratch XDG dir")
                    .next()
                    .is_none(),
                "the user's XDG_RUNTIME_DIR log was never created"
            );
            IN_ANGLES_ON.store(false, Ordering::Relaxed);
            in_uninstall();
        });
    }

    /// A call our `pre` did not supersede on is never "after"-checked: a neighbour's saved
    /// value with a null `ang` (our `pre` answered `Ignore`) is not a conflict.
    #[test]
    fn post_does_not_report_a_conflict_when_our_pre_ignored() {
        mock_khook::with_mock("input-no-false-after", |mock, _scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
            // A neighbour saved 0 for this call; our pre ignores the call because `ang` is null.
            mock.seed(vec![0u8]);
            let (_, value) = trampoline_angles_null_ang(mock, obj);
            assert_eq!(value, 0, "the neighbour's value is the call's; we did not change it");
            assert_eq!(in_conflicts(), 0, "our pre never superseded: no conflict to report");
            assert!(!CONFLICT_REPORTED.load(Ordering::Acquire));

            // The same with the override off: the value is not ours even though one is saved.
            IN_ANGLES_ON.store(false, Ordering::Relaxed);
            mock.seed(vec![0u8]);
            let mut ang = sys::QAngle {
                x: 5.0,
                y: 6.0,
                z: 7.0,
            };
            let (_, _) = trampoline_angles(mock, obj, 0, &mut ang);
            assert_eq!((ang.x, ang.y, ang.z), (5.0, 6.0, 7.0));
            assert_eq!(in_conflicts(), 0);
            in_uninstall();
        });
    }

    /// Registration failures go through one helper: the retryable `GateBusy` (and `InThunk`,
    /// which no registration produces any more) answers the `blockedOn` retry hint, every other
    /// error the plain `{"reason":...}` naming the slot.
    #[test]
    fn registration_errors_map_in_thunk_to_a_retry_hint() {
        for retryable in [
            s2sdk::hook::Error::GateBusy,
            s2sdk::hook::Error::InThunk,
        ] {
            let error = registration_error("CClientInput::unk3021", &retryable, "");
            assert_eq!(error.code, -32017);
            assert_eq!(error.name, "PrerequisiteFailed");
            assert!(
                error
                    .data
                    .contains(r#""blockedOn":"khook: retry next tick""#),
                "{retryable}: {}",
                error.data
            );
        }
        let error = registration_error(
            "CClientInput::unk3021",
            &s2sdk::hook::Error::KHookNotInstalled,
            "",
        );
        assert!(error.data.contains(r#"{"reason":"CClientInput::unk3021: no KHook"#), "{}", error.data);
    }

    /// A pair whose removal KHook defers (lane E/A2: `remove_sync` answers `Deferred` from a
    /// thunk and the delete thread removes it later) is inert as soon as the disarm starts: it
    /// applies no delta, counts no frame and no tick, and reports no conflict. Re-arming while
    /// the old pair is still associated is clean: only the new pair acts, once per frame.
    #[test]
    fn a_disarmed_pair_is_inert_before_its_removal_lands() {
        mock_khook::with_mock("input-stale-generation", |mock, _scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            let old_frame = mock.context_of(sys::CClientInput_slots::unk3021 as i32);
            let old_angles = mock.context_of(sys::CClientInput_slots::unk3023 as i32);
            {
                let mut state = lock_state();
                state.want = 1; // a tapped/held bit the body would apply
                state.applied = 0;
                state.tap = [0; 64];
                state.regap = 0;
                state.tap[0] = 3; // a tap the retired body must not keep alive
            }
            IN_FRAMES.store(0, Ordering::Relaxed);
            IN_EDGES.store(0, Ordering::Relaxed);
            IN_ANGLE_TICKS.store(0, Ordering::Relaxed);
            static APPLIED: Mutex<Vec<(isize, i32)>> = Mutex::new(Vec::new());
            APPLIED.lock().unwrap().clear();
            unsafe extern "C" fn record(iv: *mut c_void, _slot: i32, delta: f32, _frac: f32) {
                APPLIED.lock().unwrap().push((iv as isize, delta.to_bits() as i32));
            }
            IN_APPLY.store(record as InApplyFn as *mut c_void, Ordering::Release);

            // Disarm, but KHook's delete thread has not run the two removals yet: the contexts
            // (and the callbacks they hold) stay alive, exactly the window lane E/A2 opens.
            mock.defer_removals(true);
            in_uninstall();
            mock.defer_removals(false);
            assert!(!installed());
            assert_eq!(mock.deferred_len(), 2, "both removals are still pending");
            assert_eq!(lock_state().tap, [0; 64], "disarm zeroes the tap counters");

            let action = trampoline_frame_begin_at(mock, old_frame, obj, 0);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(
                IN_FRAMES.load(Ordering::Relaxed),
                0,
                "a retired pair runs no frame body"
            );
            assert!(APPLIED.lock().unwrap().is_empty(), "and applies no +1");
            assert_eq!(lock_state().applied, 0, "nor owns a delta");

            let mut ang = sys::QAngle {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            };
            let (action, value) = trampoline_angles_at(mock, old_angles, obj, 0, &mut ang);
            assert_eq!(action, s2sdk::hook::Action::Ignore);
            assert_eq!(value, 0);
            assert_eq!((ang.x, ang.y, ang.z), (1.0, 2.0, 3.0), "no angle write");
            assert_eq!(IN_ANGLE_TICKS.load(Ordering::Relaxed), 0, "no tick");
            assert_eq!(in_conflicts(), 0, "no conflict from a retired pair");

            // Re-arm while the old pair is still associated (the same slot, a second live
            // registration): the old callbacks are inert, the new pair is the one that acts.
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
            {
                let mut state = lock_state();
                state.want = 1; // the re-armed lane's own held bit
            }
            in_arm(obj).expect("re-arm over a pending removal");
            // A re-arm re-points `IN_APPLY` at the pinned engine entry; the recorder stands in
            // for it again.
            IN_APPLY.store(record as InApplyFn as *mut c_void, Ordering::Release);
            let new_frame = mock.context_of(sys::CClientInput_slots::unk3021 as i32);
            assert_ne!(new_frame, old_frame, "a new registration, a new context");
            trampoline_frame_begin_at(mock, old_frame, obj, 0);
            assert_eq!(
                IN_FRAMES.load(Ordering::Relaxed),
                0,
                "the old pair still runs nothing after the re-arm"
            );
            trampoline_frame_begin_at(mock, new_frame, obj, 0);
            assert_eq!(
                IN_FRAMES.load(Ordering::Relaxed),
                1,
                "the body runs once per frame, not once per live pair"
            );
            assert_eq!(APPLIED.lock().unwrap().len(), 1, "one +1, not two");
            assert_eq!(lock_state().applied, 1);

            {
                let mut state = lock_state();
                state.want = 0;
            }
            in_uninstall();
            IN_APPLY.store(ptr::null_mut(), Ordering::Release);
        });
    }


    /// A call in KHook's order over a re-armed slot: the newest registration's `pre` first, then
    /// the retired one still in KHook's list, the skipped original, the posts in reverse
    /// (`detour.cpp:1477-1489`). The stale `pre` must leave the live pair's note alone: if it
    /// wrote `Ignored`, the live `post` would take no note, skip its check and lose the real
    /// "after" conflict a neighbour override on this call causes.
    #[test]
    fn a_stale_pre_does_not_hide_the_live_pairs_own_note() {
        mock_khook::with_mock("input-stale-note", |mock, scratch| {
            let obj = fake_object();
            in_arm(obj).expect("arm");
            let stale = mock.context_of(sys::CClientInput_slots::unk3023 as i32);
            // A disarm whose removal KHook's delete thread still holds, then a re-arm: the old
            // callbacks stay in the slot's callback list, the new ones go ahead of them.
            mock.defer_removals(true);
            in_uninstall();
            mock.defer_removals(false);
            assert_eq!(mock.deferred_len(), 2, "both removals are still pending");
            in_arm(obj).expect("re-arm over the pending removal");
            let live = mock.context_of(sys::CClientInput_slots::unk3023 as i32);
            assert_ne!(stale, live, "a new registration, a new context");
            IN_ANGLES_ON.store(true, Ordering::Relaxed);
            let ticks = IN_ANGLE_TICKS.load(Ordering::Relaxed);

            unsafe extern "C" fn original(
                _this: *mut c_void,
                _slot: sys::CSplitScreenSlot,
                _curtime: f64,
                _ang: *mut sys::QAngle,
            ) -> u8 {
                0 // CS2's unk3023: false.
            }
            let mut ang = sys::QAngle {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            };
            let slot = sys::CSplitScreenSlot { m_Data: 0 };
            mock_khook::enter(mock, live, original as *mut c_void);
            // SAFETY: the callbacks are the ones this module registered for the generated
            // `u8 f(CSplitScreenSlot, double, QAngle *)` thunk set.
            unsafe {
                in_angles_hook::pre(obj, slot, 0.0, &mut ang);
            }
            assert_eq!(mock_khook::action(), s2sdk::hook::Action::Supersede);
            // The retired pair's `pre`, still in the list, runs after ours.
            mock_khook::set_context(stale);
            // SAFETY: as above.
            unsafe {
                in_angles_hook::pre(obj, slot, 0.0, &mut ang);
            }
            assert_eq!(
                mock_khook::action(),
                s2sdk::hook::Action::Supersede,
                "the stale pre changes nothing"
            );
            // Supersede skips the original (`detour.cpp:889`). Then a neighbour changes the value
            // KHook has saved for this call: a callback cannot outrank our `Supersede`
            // (`detour.cpp:381-383` keeps the first among equals and nothing is greater), so the
            // test models the one path that can - a holder of the override pointer
            // (`GetOverrideValuePtr`) writing it (`detour.cpp:441-443`).
            mock_khook::mark_original_skipped();
            mock_khook::overwrite_saved_override(mock, &[0]);
            // The posts run in reverse: the stale one first (it must not consume the note), then
            // ours, which sees a final value that is not the 1 it saved.
            // SAFETY: as above.
            unsafe {
                in_angles_hook::post(obj, slot, 0.0, &mut ang);
                mock_khook::set_context(live);
                in_angles_hook::post(obj, slot, 0.0, &mut ang);
                let value = in_angles_hook::make_return(obj, slot, 0.0, &mut ang);
                mock_khook::leave();
                assert_eq!(value, 0, "the neighbour's value is the call's");
            }
            assert_eq!(IN_ANGLE_TICKS.load(Ordering::Relaxed), ticks + 1, "one tick");
            assert_eq!(
                in_conflicts(),
                1,
                "the live post still saw the neighbour's value: one 'after'"
            );
            let marker = "khook conflict: CClientInput::unk3023 after";
            assert_eq!(
                scratch.text().matches(marker).count(),
                1,
                "one rate-limited line: {}",
                scratch.text()
            );

            {
                let mut state = lock_state();
                state.want = 0;
            }
            in_uninstall();
        });
    }

    /// NEGATIVE: the second registration is refused (`INVALID_HOOK`) - the first hook is
    /// removed again, nothing is armed, and the op answers `-32017` naming the refused slot
    /// and the cleanup.
    #[test]
    fn a_refused_second_slot_leaves_no_half_installed_input() {
        mock_khook::with_mock("input-refused", |mock, _scratch| {
            mock.fail_setup_at(2);
            let error = in_arm(fake_object()).expect_err("the second slot is refused");
            assert_eq!(error.code, -32017);
            assert_eq!(error.name, "PrerequisiteFailed");
            assert!(
                error.data.contains("CClientInput::unk3023")
                    && error.data.contains("SetupVirtualHook returned INVALID_HOOK"),
                "the reason names the refused slot: {}",
                error.data
            );
            assert!(
                error.data.contains("CClientInput::unk3021 was removed"),
                "and the cleanup of the first: {}",
                error.data
            );
            assert!(!installed(), "no half-installed input");
            assert!(object().is_null(), "no object recorded");
            assert_eq!(
                mock.removals_by_index(),
                vec![(sys::CClientInput_slots::unk3021 as i32, false)],
                "the first slot was removed again, synchronously"
            );
        });
    }

    /// NEGATIVE: no API installed - the registration is refused with `KHookNotInstalled`,
    /// which the op turns into `-32017`; nothing is armed.
    #[test]
    fn arming_without_khook_is_refused() {
        let _guard = crate::hooks::test_guard::lock();
        assert!(s2sdk::hook::api().is_none());
        let error = in_arm(fake_object()).expect_err("no API, no hook");
        assert_eq!(error.code, -32017);
        assert!(
            error.data.contains("no KHook IKHook API is installed"),
            "{}",
            error.data
        );
        assert!(!installed());
        assert!(object().is_null());
    }

    /// The reply member this build adds: `"conflicts":N` after the C's `"buttons"` array.
    #[test]
    fn reply_bytes_carry_the_conflict_counter() {
        let text = |conflicts: u64| {
            let mut buf = vec![0u8; 512];
            let mut sb = Sb::new(&mut buf);
            let read = InStatusRead {
                installed: false,
                want: 0,
                applied: 0,
                angles_on: false,
                angles: [0.0; 3],
                frames: 0,
                edges: 0,
                angle_ticks: 0,
                conflicts,
            };
            in_status_json_in(&read, ptr::null(), &mut sb);
            String::from_utf8(sb.as_cstr().to_vec()).expect("the reply is ASCII")
        };
        assert_eq!(
            text(0),
            r#"{"installed":false,"held":[],"angles":null,"frames":0,"edges":0,"angleTicks":0,"buttons":[],"conflicts":0}"#
        );
        assert_eq!(
            text(7),
            r#"{"installed":false,"held":[],"angles":null,"frames":0,"edges":0,"angleTicks":0,"buttons":[],"conflicts":7}"#
        );
    }

    // -------------------------------------------------------------------------------------
    // The trampolines: KHook's per-call state around one hooked call
    // (`detour.cpp:746-990`), in the shape the generated thunks expect.
    // -------------------------------------------------------------------------------------

    /// One slot-30 call against a registration's context: pre, the original (an empty function,
    /// counted), post, make_return.
    fn trampoline_frame_begin_at(
        mock: &mock_khook::Mock,
        context: *mut c_void,
        this: *mut c_void,
        slot: i32,
    ) -> s2sdk::hook::Action {
        unsafe extern "C" fn original(_this: *mut c_void, _slot: sys::CSplitScreenSlot, _rec: *mut c_void) {}
        mock_khook::enter(mock, context, original as *mut c_void);
        let slot = sys::CSplitScreenSlot { m_Data: slot };
        // SAFETY: the callbacks are the ones this module registered for the generated
        // `void f(CSplitScreenSlot, void *)` thunk set.
        unsafe {
            in_frame_begin_hook::pre(this, slot, ptr::null_mut());
        }
        let action = mock_khook::action();
        if action == s2sdk::hook::Action::Supersede {
            mock_khook::mark_original_skipped();
        } else {
            // SAFETY: as above (`make_call_original` of the same generated module).
            unsafe {
                in_frame_begin_hook::make_call_original(this, slot, ptr::null_mut());
            }
        }
        // SAFETY: as above (`post`).
        unsafe {
            in_frame_begin_hook::post(this, slot, ptr::null_mut());
        }
        // SAFETY: as above; `make_return` reads KHook's saved value and destroys the call.
        unsafe {
            in_frame_begin_hook::make_return(this, slot, ptr::null_mut());
        }
        mock_khook::leave();
        action
    }

    /// One slot-30 call against the current registration on the slot.
    fn trampoline_frame_begin(
        mock: &mock_khook::Mock,
        this: *mut c_void,
        slot: i32,
    ) -> s2sdk::hook::Action {
        let index = sys::CClientInput_slots::unk3021 as i32;
        trampoline_frame_begin_at(mock, mock.context_of(index), this, slot)
    }

    /// One slot-32 call against a registration's context; answers the action our pre took and
    /// the value the call ends with.
    fn trampoline_angles_at(
        mock: &mock_khook::Mock,
        context: *mut c_void,
        this: *mut c_void,
        slot: i32,
        ang: *mut sys::QAngle,
    ) -> (s2sdk::hook::Action, u8) {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            _slot: sys::CSplitScreenSlot,
            _curtime: f64,
            _ang: *mut sys::QAngle,
        ) -> u8 {
            0 // CS2's unk3023: false.
        }
        mock_khook::enter(mock, context, original as *mut c_void);
        let slot = sys::CSplitScreenSlot { m_Data: slot };
        // SAFETY: the callbacks are the ones this module registered for the generated
        // `u8 f(CSplitScreenSlot, double, QAngle *)` thunk set.
        unsafe {
            in_angles_hook::pre(this, slot, 0.0, ang);
        }
        let action = mock_khook::action();
        if action == s2sdk::hook::Action::Supersede {
            mock_khook::mark_original_skipped();
        } else {
            // SAFETY: as above.
            unsafe {
                in_angles_hook::make_call_original(this, slot, 0.0, ang);
            }
        }
        // SAFETY: as above.
        unsafe {
            in_angles_hook::post(this, slot, 0.0, ang);
        }
        // SAFETY: as above.
        let value = unsafe { in_angles_hook::make_return(this, slot, 0.0, ang) };
        mock_khook::leave();
        (action, value)
    }

    /// One slot-32 call against the current registration on the slot.
    fn trampoline_angles(
        mock: &mock_khook::Mock,
        this: *mut c_void,
        slot: i32,
        ang: &mut sys::QAngle,
    ) -> (s2sdk::hook::Action, u8) {
        let index = sys::CClientInput_slots::unk3023 as i32;
        trampoline_angles_at(mock, mock.context_of(index), this, slot, core::ptr::from_mut(ang))
    }

    /// One slot-32 call with a null `ang`: the pre must answer `Ignore` without dereferencing.
    fn trampoline_angles_null_ang(
        mock: &mock_khook::Mock,
        this: *mut c_void,
    ) -> (s2sdk::hook::Action, u8) {
        let index = sys::CClientInput_slots::unk3023 as i32;
        trampoline_angles_at(mock, mock.context_of(index), this, 0, ptr::null_mut())
    }
}
