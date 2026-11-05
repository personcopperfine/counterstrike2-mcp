//! The sound group (`sound.status` / `sound.subscribe` / `sound.drain` /
//! `sound.uninstall`): the C's `op_sound` (`bridge/mcp_bridge_game.inc:1094-1205`) with its
//! CSoundOpSystem start hooks ported onto KHook.
//!
//! # Mechanism
//!
//! The class vtable's three `StartSoundEvent` overloads are hooked in place with KHook
//! (`SetupVirtualHook`, [SetupMode::Sync]) on the pinned table
//! `modules_base(PIN_SOUND) + pins::SNV_SOUNDOPSYSTEM_VT`, at the generated slots
//! ([`sys::ISoundOpSystem_slots::StartSoundEvent_2`] with params, `StartSoundEvent_1` by name,
//! `StartSoundEvent` with a name/entity/layer) - never a number written here.
//!
//! One **post** callback per slot, no `pre`: the observer runs after the original, so it
//! records exactly what the old wrap recorded (same `snd_ev_init` / `snd_capture_packed` /
//! `snd_push` calls, same `via` ids) and reads the event guid from the original's saved return
//! value ([`HookCall::original_value_ptr`], KHook's `GetOriginalValuePtr`,
//! `khook.hpp:2191`). It answers `Ignore`, so the call's own return value is untouched.
//!
//! Observers never change a call's value, so there is **no conflict detection** on these
//! slots: two plugins that both only watch a start cannot disagree about its result, and the
//! §13 rule (report when two *value changers* meet) has nothing to report. The three hooks
//! only ever add records to the ring, so another plugin's hook on the same slot composes with
//! ours whatever order KHook runs them in.
//!
//! # Why KHook, and not a per-object vptr copy
//!
//! The old implementation copied the class's vtable into a private array and pointed **this
//! object's** vptr at it. That was safe only while nothing else touched the class's table, and
//! CS2 has other plugins: a per-object copy *bypasses* a virtual another plugin hooked (its
//! callback is not in our copy) and *freezes* whatever was in the table when we copied it.
//! KHook patches the class's **shared** entry, once, in place, and never restores it
//! (`third_party/khook/src/detour.hpp:96-103`), so every object of the class and every plugin
//! that hooks the slot later sees the same entry - and the bridge's identity check asks KHook
//! for the saved original instead of comparing the word (`crate::modules::slot_matches`), so
//! `SNV_SOUNDOPSYSTEM_VT`'s pinned slots still identify the object. Our callbacks answer
//! `Ignore` for every `this` that is not the recorded CSoundOpSystem (it is a singleton, so
//! that is every other object of the class).
//!
//! # The listener object is not a hook
//!
//! The supported listener (`AddSoundEventListener` / `RemoveSoundEventListener`, slots 81/82)
//! is a **bridge-owned** 16-slot vtable (`mcp_bridge_game.inc:1042-1043`) behind a
//! **bridge-owned object** ([`SndListener`]: one word, the table's address), not a game
//! virtual: registering it changes nothing in any game table, so it stays exactly the C's
//! object and needs neither KHook nor a conflict rule. Registering the table *itself* (as if
//! the table were the object) is the crash this shape fixes: see [`SndListener`].
//!
//! # Threads and state
//!
//! The three start slots run on the caller's thread (the main thread and the sound thread);
//! the listener's on-start thunk runs on the sound thread (`:1040-1041`). Every body touches
//! only lock-protected state: the event ring behind one [`Mutex`] where the C used its
//! `g_sndLock` spinlock (`:916-917`). Install/uninstall happen only from the main-thread drain,
//! and a hook is removed with [`Hook::remove_sync`] - never from inside a callback.
//!
//! # Arming
//!
//! `sound.subscribe` (the row that carries `NEED_KHOOK`) is where the C's `mode` selection has
//! to run; `sound.status` and `sound.drain` run the same selection **when the KHook API is
//! present**, because real clients rely on the C's "the first status/drain arms the wrap"
//! (`client-mcp`'s `drain_sounds` / `sounds_status`, `hammer-mcp`'s `installOnly`,
//! `bridge/test_sound_params.py`). They carry no `NEED_KHOOK` bit, so after a Metamod unload
//! they must still answer: without the API the **wrap half** of the selection is skipped (the
//! wrap cannot be armed without it) while the **listener half** still follows `mode` - the
//! listener is a bridge-owned object and needs no KHook, so the C's `{mode:"both"}` still
//! registers it - and the reply adds `"blockedOn":"metamod: KHook not loaded"`: a note, never
//! an error. `sound.uninstall` only tears down and keeps answering either way. See
//! `target/protocol-diff-b3.md`.
//!
//! `sound.subscribe {mode:"listener"}` installs no KHook hook, but the row keeps the
//! `NEED_KHOOK` bit: the gate is per ROW, not per request, and a row that can install a hook
//! must say so, or `control.capabilities` would advertise a row whose first `wrap` call fails.
//! A listener-only request without KHook therefore answers the gate's `-32017` with
//! `blockedOn` - by design, not by accident.
//!
//! # Teardown: a disarmed wrap is inert, even before it is gone
//!
//! `remove_sync` from inside a thunk (lane E/A2) answers [`Removal::Deferred`], so the
//! delete thread can run a disarmed callback a few ms later. Each `post` therefore carries the
//! [`ARMED_GEN`] its install captured and records nothing when the counter has moved on: the
//! disarm bumps it **first**, and a re-arm during a pending removal is harmless (the old three
//! are inert whatever KHook still calls; the new three are the only ones that record).
//!
//! The three start slots run on the **caller's** thread - the main thread and the sound
//! thread - so a post's generation check and a concurrent disarm are not atomic with respect to
//! each other: one event can still be recorded in the instant between them (the check passed,
//! the disarm then retired the generation, the record runs). That is harmless (the ring is
//! lock-protected and an event the game really started is a true observation), and it is the one
//! window the generation guard does not close.

use core::cell::UnsafeCell;
use core::ffi::c_char;
use core::ffi::c_void;
use core::mem::offset_of;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use s2sdk::hook::{Hook, HookCall, Removal, Return, SetupMode};
use s2sdk::khook_thunks;
use s2sdk_sys::generated as sys;

use crate::entity_json::{json_int, json_string, Sb};
use crate::generated::pins;
use crate::ops::game::builders::{add_kv_i, add_kv_str, add_kv_v3};
use crate::ops::game::json_bool;
use crate::ops::game::schema::{gs_designer_name, gs_ent_by_index, gs_origin, gs_scope,
                               lock as gs_lock, modules_base};
use crate::ops::ident::{mem_ok, mod_iface, obj_is};
use crate::ops::{finish, Handler, OpErr};
use crate::hooks::TeardownReport;
use crate::queue::Request;
use crate::{guard, log};

/// The sound dispatch arm's result buffer (`mcp_bridge.c:6032`,
/// `static char rbuf[98304]`): drain, up to 256 events (~300 B each), rest requeued.
const SND_BUF: usize = 98304;
/// The arm's overflow answer (`mcp_bridge.c:6037`).
const SND_OVERFLOW: &str = "InternalError: sound drain result too large";

/// `SND_RING` (`mcp_bridge_game.inc:895`): the event ring's size.
const SND_RING: u32 = 256;
/// `SND_PARAMS_MAX` (`mcp_bridge_game.inc:896`): packed start params copied per event (drain
/// hex <= 256 chars).
const SND_PARAMS_MAX: usize = 128;

/// The with-params start's slot (the C's `S2_SLOT_ISoundOpSystem_StartSoundEvent_2`; 11 on
/// MSVC, 13 on Itanium - the cfg'd generated const) - the wire's `via` for those events.
pub const START_WITH_PARAMS_SLOT: u8 = sys::ISoundOpSystem_slots::StartSoundEvent_2 as u8;
/// The by-name start's slot (the wire's `via`).
pub const START_BY_NAME_SLOT: u8 = sys::ISoundOpSystem_slots::StartSoundEvent_1 as u8;
/// The plain start's slot (the wire's `via`).
pub const START_SLOT: u8 = sys::ISoundOpSystem_slots::StartSoundEvent as u8;
/// The listener event's slot: the C records it as slot 81 (`AddSoundEventListener`),
/// `mcp_bridge_game.inc:1060`.
pub const LISTENER_SLOT: u8 = sys::ISoundOpSystem_slots::AddSoundEventListener as u8;

/// One started sound event (the C's `SndEvt`, `mcp_bridge_game.inc:897`). `tick` keeps the
/// C's `-1` init and is never written again by the C either — kept 1:1.
#[derive(Clone, Copy)]
pub struct SndEvt {
    /// assigned at push (`:938`)
    pub seq: u32,
    /// the sound event hash (0 for the by-name/layer paths that carry a name)
    pub hash: u32,
    /// the source entity index
    pub entindex: i32,
    /// the event guid
    pub guid: u32,
    /// the vtable slot the event came through (11/12/13/81)
    pub slot: u8,
    /// the voice layer byte (`m_nLayer`; the wire key stays `flags`, `:1149`)
    pub layer: u8,
    /// -1 (the C's init; never written again by the C)
    pub tick: i32,
    /// `GetTickCount64()` at the event ([`wiring::mono_ms`] here — the wire shows only
    /// `ageMs` differences)
    pub ms: u64,
    /// the slot-81 event's aux guids (`:1061`)
    pub aux: [u32; 2],
    /// the slot-86 instance int, -1 unreadable (`:1061`)
    pub inst150: i32,
    /// the event name, up to 63 bytes + NUL (`:933-934`)
    pub name: [u8; 64],
    /// the caller's declared packed-params length, -1 unreadable (`:954-955`)
    pub plen: i32,
    /// the packed-params bytes actually copied (`:956`)
    pub pcopied: u16,
    /// the packed-params copy (`:897`)
    pub praw: [u8; SND_PARAMS_MAX],
}

impl SndEvt {
    /// `snd_ev_init`'s zeroed shape before its own writes (the array fields have no
    /// `Default` derive on this Rust; the C's zero-init).
    const ZERO: Self = Self {
        seq: 0,
        hash: 0,
        entindex: 0,
        guid: 0,
        slot: 0,
        layer: 0,
        tick: 0,
        ms: 0,
        aux: [0; 2],
        inst150: 0,
        name: [0; 64],
        plen: 0,
        pcopied: 0,
        praw: [0; SND_PARAMS_MAX],
    };
}

impl Default for SndEvt {
    fn default() -> Self {
        Self::ZERO
    }
}

/// The ring behind one mutex: the C's `g_snd` / `g_sndHead` / `g_sndCount` / `g_sndSeq` /
/// `g_sndDropped` / `g_sndTotal` (`mcp_bridge_game.inc:913-914`). Touched by the sound
/// thread's thunks (push) and the main thread's drain (pop + requeue).
pub struct Ring {
    /// `g_snd` (allocated once, on first use — a 256 x ~244 B static stays out of the
    /// binary, per the wave-2 big-static rule).
    pub slots: Vec<SndEvt>,
    /// `g_sndHead`
    pub head: u32,
    /// `g_sndCount`
    pub count: u32,
    /// `g_sndSeq`
    pub seq: u32,
    /// `g_sndDropped`
    pub dropped: u32,
    /// `g_sndTotal`
    pub total: u32,
}

impl Ring {
    fn ensure(&mut self) {
        if self.slots.is_empty() {
            self.slots.resize(usize::try_from(SND_RING).unwrap(), SndEvt::ZERO);
        }
    }
}

static SND_RING_STATE: Mutex<Ring> = Mutex::new(Ring {
    slots: Vec::new(),
    head: 0,
    count: 0,
    seq: 0,
    dropped: 0,
    total: 0,
});

/// The ring lock (the C's `snd_lock` / `snd_unlock`, `mcp_bridge_game.inc:916-917`). Exposed
/// for the host tests.
#[doc(hidden)]
pub fn ring_lock() -> MutexGuard<'static, Ring> {
    SND_RING_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The C's `g_sosObj` (`mcp_bridge_game.inc:906`): the resolved CSoundOpSystem, set by
/// [`snd_sos_resolve`] / [`snd_install`] on the main thread, read by the sound thread's
/// listener thunk.
static SOS_OBJ: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// The C's `g_sosOrigVt` (`mcp_bridge_game.inc:907`): the pinned original vtable.
static SOS_ORIG_VT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// `SoundEventGuid_t` as `khook_thunks!` can carry it: the generated mirror derives `Copy` but
/// has no `Default` (the C++ class has a user-written default constructor instead,
/// `isoundopsystem.h:25`), and the macro's return type needs both.
///
/// A transparent newtype, not a hand-written layout: every byte still comes from the generated
/// [`sys::SoundEventGuid_t`], and [`Default`] writes the constructor's own `m_Data = 0`.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct Guid(sys::SoundEventGuid_t);

impl Default for Guid {
    fn default() -> Self {
        // `SoundEventGuid_t() : m_Data( 0 ) {}` - `isoundopsystem.h:25`.
        Self(sys::SoundEventGuid_t { m_Data: 0 })
    }
}

// The three start overloads as the vtable carries them. Their return is `SoundEventGuid_t`, a
// C++ class: MSVC returns every class through the hidden pointer after `this`, Itanium returns
// a trivially-copyable 4-byte class in a register (the class has no copy constructor or
// destructor, so it is trivial for calls there) - exactly what the generated per-signature
// consts say, and the assertions below hold the two flags to them instead of choosing by hand.
khook_thunks! {
    /// `SoundEventGuid_t ISoundOpSystem::StartSoundEvent(const SosStartSoundEventParams_t*)`
    /// (the generated `StartSoundEvent_2` slot).
    mod snd_params_hook: fn(
        this: *mut c_void,
        params: val *const sys::SosStartSoundEventParams_t,
    ) -> Guid {
        sret_msvc = sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `SoundEventGuid_t ISoundOpSystem::StartSoundEvent(const char *,
    /// SosStartSoundEventParams_t*)` (the generated `StartSoundEvent_1` slot).
    mod snd_by_name_hook: fn(
        this: *mut c_void,
        name: val *const c_char,
        params: val *mut sys::SosStartSoundEventParams_t,
    ) -> Guid {
        sret_msvc = sret,
        sret_itanium = no_sret,
    }
}

khook_thunks! {
    /// `SoundEventGuid_t ISoundOpSystem::StartSoundEvent(const char *, CEntityIndex, uint8)`
    /// (the generated `StartSoundEvent` slot).
    mod snd_start_hook: fn(
        this: *mut c_void,
        name: val *const c_char,
        entindex: val sys::CEntityIndex,
        layer: val u8,
    ) -> Guid {
        sret_msvc = sret,
        sret_itanium = no_sret,
    }
}

// The declared sret flags are the generated ones, per target: `StartSoundEvent*_SRET` is `true`
// in the MSVC binding block and `false` in the Itanium one (`soundsystem.rs`), so this pins
// `sret_msvc = sret` / `sret_itanium = no_sret` to the generator rather than to this comment.
const _: () = assert!(sys::ISoundOpSystem_slots::StartSoundEvent_SRET == cfg!(windows));
const _: () = assert!(sys::ISoundOpSystem_slots::StartSoundEvent_1_SRET == cfg!(windows));
const _: () = assert!(sys::ISoundOpSystem_slots::StartSoundEvent_2_SRET == cfg!(windows));

// Both hooked slots are in-table entries of the measured table (the C's `_Static_assert`
// shape, `mcp_bridge.c:5011`).
const _: () = assert!(sys::ISoundOpSystem_slots::StartSoundEvent < sys::ISoundOpSystem_slots::VTLEN);
const _: () = assert!(
    sys::ISoundOpSystem_slots::StartSoundEvent_1 < sys::ISoundOpSystem_slots::VTLEN
);
const _: () = assert!(
    sys::ISoundOpSystem_slots::StartSoundEvent_2 < sys::ISoundOpSystem_slots::VTLEN
);

/// The three live hooks, while armed. [`Hook`] is `!Send` because it holds a raw context
/// pointer; [`SndHooks`] is the main-thread-only claim that makes it storable in a static -
/// `snd_install`, the teardown and the tool `PreShutdown` all run on the main thread, and the
/// callbacks never take this lock.
static SND: Mutex<Option<SndHooks>> = Mutex::new(Option::None);

/// The hooks plus the claim that only the game's main thread moves them.
struct SndHooks {
    /// [`sys::ISoundOpSystem_slots::StartSoundEvent_2`] (the packed params overload).
    params: Hook,
    /// [`sys::ISoundOpSystem_slots::StartSoundEvent_1`] (the by-name overload).
    by_name: Hook,
    /// [`sys::ISoundOpSystem_slots::StartSoundEvent`] (name / entity / layer).
    start: Hook,
}

// SAFETY: every access is behind [`SND`]'s mutex and comes from the main thread (install from
// the drain, removal from `snd_uninstall`/`hooks_teardown`/`PreShutdown`). A `Hook` is only
// `!Send` because its context pointer is not `Sync`-checked by the compiler; nothing in KHook's
// API is thread-affine - `RemoveHook` may be called from any thread - and the reference counts
// inside the context are atomic.
unsafe impl Send for SndHooks {}

/// The hooks lock (main thread only, see [`SndHooks`]).
fn snd_lock() -> MutexGuard<'static, Option<SndHooks>> {
    SND.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The wrap's arm generation: every hook's `post` captures the value its install took and
/// records nothing when it no longer matches. Bumped when the three are armed and again as the
/// FIRST step of a disarm, so a callback KHook still holds (its removal was deferred,
/// `Removal::Deferred`) cannot record an event after the wrap was turned off.
static ARMED_GEN: AtomicU64 = AtomicU64::new(0);

/// Take the next arm generation (see [`ARMED_GEN`]).
fn next_generation() -> u64 {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel) + 1
}

/// Invalidate the wrap that is being removed (see [`ARMED_GEN`]).
fn retire_generation() {
    ARMED_GEN.fetch_add(1, Ordering::AcqRel);
}

/// Whether a callback captured by an install of `generation` still records for the live wrap.
fn generation_current(generation: u64) -> bool {
    ARMED_GEN.load(Ordering::Acquire) == generation
}

/// The pinned original vtable as a typed reference, for the saved originals
/// (`mcp_bridge_game.inc:1016-1018`): `g_sosOrigVt ? g_sosOrigVt : *(void ***)g_sosObj`
/// (`:1052`). Null when neither is recorded (the wrap's thunks cannot run then).
fn pinned_op_vt() -> *const sys::ISoundOpSystem_vtable {
    let orig = SOS_ORIG_VT.load(Ordering::Relaxed);
    let vt = if !orig.is_null() {
        orig
    } else {
        let obj = SOS_OBJ.load(Ordering::Relaxed);
        if obj.is_null() {
            return core::ptr::null();
        }
        // SAFETY: `obj` is the resolved CSoundOpSystem (the `obj_is` check in
        // `snd_sos_resolve` / `snd_install`); its vptr is the C's `*(void ***)g_sosObj`
        // read (`:1052`).
        let vptr = unsafe { (obj as *const *const c_void).read_unaligned() };
        vptr as *mut c_void
    };
    vt as *const sys::ISoundOpSystem_vtable
}

/// `snd_name_of` (`mcp_bridge_game.inc:919-925`): the event's name by hash through the
/// ISoundEventManager base, or null (the raw pointer the C passes on; every caller
/// `mem_ok`s it before reading bytes).
fn snd_name_of(hash: u32) -> *const u8 {
    let obj = SOS_OBJ.load(Ordering::Relaxed);
    if obj.is_null() {
        return core::ptr::null();
    }
    // mcp_bridge_game.inc:921: the ISoundEventManager base subobject.
    // SAFETY: `obj` is the resolved CSoundOpSystem; the base offset is generated.
    let sem = unsafe { (obj as *const u8).byte_add(sys::ISoundOpSystem::BASE_OFFSET_ISoundEventManager) };
    // mcp_bridge_game.inc:922: the manager's vptr must be the pinned one.
    // SAFETY: `sem` is inside the resolved object, 8-byte aligned subobject start.
    let vptr = unsafe { (sem as *const *const c_void).read_unaligned() };
    if vptr != (modules_base(pins::PIN_SOUND) + pins::SNV_SOUNDEVENTMGR_VT.value) as *const c_void {
        return core::ptr::null();
    }
    let iface = sem as *mut sys::ISoundEventManager;
    // SAFETY: the vptr was just proved the pinned ISoundEventManager vftable, whose slot 2
    // is verified (pins.rs:608); the typed call is the C's raw slot-2 call (`:923`).
    let get_name = unsafe { (*(*iface).vtable).GetSoundEventName }
        .expect("ISoundEventManager::GetSoundEventName (pinned slot 2)");
    // SAFETY: the verified slot entry, called with the object it was read from.
    let n = unsafe { get_name(iface, hash) };
    // mcp_bridge_game.inc:924
    if n.is_null() || !mem_ok(n.cast()) {
        return core::ptr::null();
    }
    n.cast()
}

/// `snd_ev_init` (`mcp_bridge_game.inc:926-935`).
fn snd_ev_init(
    ev: &mut SndEvt,
    slot: u8,
    hash: u32,
    name: *const u8,
    entindex: i32,
    guid: u32,
    layer: u8,
) {
    *ev = SndEvt::ZERO;
    ev.hash = hash;
    ev.entindex = entindex;
    ev.guid = guid;
    ev.slot = slot;
    ev.layer = layer;
    ev.tick = -1; // :928
    ev.ms = crate::wiring::mono_ms(); // GetTickCount64(), :928
    ev.inst150 = -1;
    ev.plen = -1;
    // mcp_bridge_game.inc:931-934: the name (the given one, else by hash), <= 63 bytes.
    let n = if !name.is_null() { name } else { snd_name_of(hash) };
    let mut i = 0usize;
    if !n.is_null() {
        while i < 63 {
            // SAFETY: the pointer passed a `mem_ok` first-byte check on every path
            // (`:996`, `:1054`, or `snd_name_of`'s own `:924`); the read-until-NUL loop is
            // the C's (`:933`).
            let b = unsafe { *n.add(i) };
            if b == 0 {
                break;
            }
            ev.name[i] = b;
            i += 1;
        }
    }
    ev.name[i] = 0;
}

/// `snd_push` (`mcp_bridge_game.inc:936-944`): one slot under the ring lock, dropping the
/// oldest when full.
fn snd_push(ev: &mut SndEvt) {
    let mut ring = ring_lock();
    ring.ensure();
    ring.seq = ring.seq.wrapping_add(1); // ev->seq = ++g_sndSeq
    ev.seq = ring.seq;
    ring.total += 1;
    if ring.count >= SND_RING {
        ring.head = (ring.head + 1) % SND_RING;
        ring.count -= 1;
        ring.dropped += 1;
    }
    let at = ((ring.head + ring.count) % SND_RING) as usize;
    ring.slots[at] = *ev;
    ring.count += 1;
}

/// `snd_record` (`mcp_bridge_game.inc:945-949`).
fn snd_record(slot: u8, hash: u32, name: *const u8, entindex: i32, guid: u32, layer: u8) {
    let mut ev = SndEvt::ZERO;
    snd_ev_init(&mut ev, slot, hash, name, entindex, guid, layer);
    snd_push(&mut ev);
}

/// `snd_capture_packed` (`mcp_bridge_game.inc:957-972`): the packed start params, captured
/// ONLY here (slot-11 starts). A len < 0 or > 65535 counts as unreadable (`plen` -1); the
/// copy probes one page at a time and stops at the first unreadable page.
fn snd_capture_packed(p: *const u8, ev: &mut SndEvt) {
    ev.plen = -1;
    ev.pcopied = 0;
    // mcp_bridge_game.inc:959-961: both extents probed before either is read.
    // SAFETY: `p` is non-null (checked below) and the offsets are inside the generated
    // layout; the probes themselves are `mem_ok` calls.
    let (fld_ptr_end, fld_len_end) = unsafe {
        (
            p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_pFieldData) + 7),
            p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nFieldDataSize) + 3),
        )
    };
    if p.is_null() || !mem_ok(fld_ptr_end.cast()) || !mem_ok(fld_len_end.cast()) {
        return;
    }
    // SAFETY: the C's unguarded reads inside the probed struct (`:963-964`).
    let (pp, len) = unsafe {
        (
            (p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_pFieldData))
                .cast::<*const u8>())
                .read_unaligned(),
            (p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nFieldDataSize))
                .cast::<i32>())
                .read_unaligned(),
        )
    };
    // mcp_bridge_game.inc:965: plen stays -1 = unreadable
    if !(0..=65535).contains(&len) {
        return;
    }
    ev.plen = len;
    let n = usize::try_from(len).unwrap_or(0).min(SND_PARAMS_MAX);
    for q in 0..n {
        // SAFETY: the arithmetic; the byte read is probed (or the C's unguarded read,
        // `:970`).
        let (at, probe) = unsafe {
            (
                pp.byte_add(q),
                q == 0 || (pp as u64 + q as u64) & 0xFFF == 0,
            )
        };
        // mcp_bridge_game.inc:969: the first byte + every page start is probed.
        if probe && !mem_ok(at.cast()) {
            break;
        }
        // SAFETY: probed above (or the C's unguarded read, `:970`).
        ev.praw[ev.pcopied as usize] = unsafe { *at };
        ev.pcopied += 1;
    }
}

/// The guid the original's call produced, from KHook's saved original value
/// (`khook.hpp:2191` `GetOriginalValuePtr`): `None` when no original ran (or no API is
/// installed), so there is nothing to record.
///
/// Runs inside the thunk: a pointer query and a 4-byte read.
fn original_guid(call: &HookCall) -> Option<u32> {
    let p = call.original_value_ptr();
    if p.is_null() || !mem_ok(p) {
        return None;
    }
    // SAFETY: KHook's original-value slot holds the return value for the signature this hook
    // was registered with - the 4-byte `SoundEventGuid_t` - and points into its per-call
    // storage, valid for the duration of the callback (`khook.hpp:744-752`,
    // `detour.cpp:373-405`). The mirror is `#[repr(C, align(4))]` and KHook's buffer is
    // allocated for it, but the read is unaligned so a tighter buffer cannot fault.
    let guid = unsafe { p.cast::<sys::SoundEventGuid_t>().read_unaligned() };
    Some(guid.m_Data as u32)
}

/// The packed start's `post` (`mcp_bridge_game.inc:977-989`): after the original, record the
/// slot-11 event from the params object it was given and the guid it returned.
///
/// Runs inside the thunk: an atomic load, one ring lock, no game call.
fn params_post(
    generation: u64,
    call: &mut HookCall,
    this: *mut c_void,
    params: *const sys::SosStartSoundEventParams_t,
) -> Return<Guid> {
    guard(Return::ignore(Guid::default()), || {
        if !generation_current(generation) {
            // A wrap this arm already retired: KHook may still call it (a deferred removal).
            return Return::ignore(Guid::default());
        }
        // The slot is shared by every object of the class: a foreign `this` is not ours.
        if this.is_null() || this != SOS_OBJ.load(Ordering::Acquire) {
            return Return::ignore(Guid::default());
        }
        // No saved original (KHook's `pre` skipped it, or the API is gone): nothing to record.
        let Some(guid) = original_guid(call) else {
            return Return::ignore(Guid::default());
        };
        let p = params.cast::<u8>();
        // SAFETY: the extent's probe runs only for a non-null `params` (the short-circuit of
        // the C's `p && mem_ok(p + S2_SIZEOF_...)`, `:981`).
        let probe_end = (!p.is_null())
            && mem_ok(unsafe {
                p.byte_add(core::mem::size_of::<sys::SosStartSoundEventParams_t>())
            }
            .cast());
        if probe_end {
            let mut ev = SndEvt::ZERO;
            // SAFETY: the struct extent was probed above (the C's unguarded reads, `:983`).
            let (hash, ent, layer) = unsafe {
                (
                    (p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nSoundEventHash))
                        .cast::<u32>())
                        .read_unaligned(),
                    (p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_SourceEntity))
                        .cast::<i32>())
                        .read_unaligned(),
                    *p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nLayer)),
                )
            };
            snd_ev_init(
                &mut ev,
                START_WITH_PARAMS_SLOT,
                hash,
                core::ptr::null(),
                ent,
                guid,
                layer,
            );
            snd_capture_packed(p, &mut ev);
            snd_push(&mut ev);
        }
        Return::ignore(Guid::default())
    })
}

/// The by-name start's `post` (`mcp_bridge_game.inc:990-998`): the params object the original
/// was given carries the hash and the source entity it wrote.
fn by_name_post(
    generation: u64,
    call: &mut HookCall,
    this: *mut c_void,
    name: *const c_char,
    params: *mut sys::SosStartSoundEventParams_t,
) -> Return<Guid> {
    guard(Return::ignore(Guid::default()), || {
        if !generation_current(generation) {
            return Return::ignore(Guid::default());
        }
        if this.is_null() || this != SOS_OBJ.load(Ordering::Acquire) {
            return Return::ignore(Guid::default());
        }
        let Some(guid) = original_guid(call) else {
            return Return::ignore(Guid::default());
        };
        let p = params.cast::<u8>();
        // SAFETY: the pointer arithmetic; each extent's probe is the `mem_ok` call below.
        let (hash_at, hash_probe, ent_at, ent_probe) = unsafe {
            (
                p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nSoundEventHash)),
                p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_nSoundEventHash) + 4),
                p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_SourceEntity)),
                p.byte_add(offset_of!(sys::SosStartSoundEventParams_t, m_SourceEntity) + 4),
            )
        };
        // mcp_bridge_game.inc:994: the hash the original wrote to the params
        // (mem_ok probes the byte AFTER the 4-byte read, as the C's `+ sizeof(u32)` does).
        let hash = if !p.is_null() && mem_ok(hash_probe.cast()) {
            // SAFETY: probed above.
            unsafe { hash_at.cast::<u32>().read_unaligned() }
        } else {
            0
        };
        // mcp_bridge_game.inc:995
        let ent = if !p.is_null() && mem_ok(ent_probe.cast()) {
            // SAFETY: probed above.
            unsafe { ent_at.cast::<i32>().read_unaligned() }
        } else {
            -1
        };
        // mcp_bridge_game.inc:996
        let name = if !name.is_null() && mem_ok(name.cast()) {
            name as *const u8
        } else {
            core::ptr::null()
        };
        snd_record(START_BY_NAME_SLOT, hash, name, ent, guid, 0);
        Return::ignore(Guid::default())
    })
}

/// The plain start's `post` (`mcp_bridge_game.inc:999-1004`): the arguments go in whole.
fn start_post(
    generation: u64,
    call: &mut HookCall,
    this: *mut c_void,
    name: *const c_char,
    entindex: sys::CEntityIndex,
    layer: u8,
) -> Return<Guid> {
    guard(Return::ignore(Guid::default()), || {
        if !generation_current(generation) {
            return Return::ignore(Guid::default());
        }
        if this.is_null() || this != SOS_OBJ.load(Ordering::Acquire) {
            return Return::ignore(Guid::default());
        }
        let Some(guid) = original_guid(call) else {
            return Return::ignore(Guid::default());
        };
        // mcp_bridge_game.inc:1002
        let name = if !name.is_null() && mem_ok(name.cast()) {
            name as *const u8
        } else {
            core::ptr::null()
        };
        snd_record(START_SLOT, 0, name, entindex._index, guid, layer);
        Return::ignore(Guid::default())
    })
}

/// `snd_installed` (`mcp_bridge_game.inc:1005`): the three start slots are hooked.
#[must_use]
pub fn snd_installed() -> bool {
    snd_lock().is_some()
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

/// Arm the three start slots on the pinned table for `obj` (`snd_install`'s enable path after
/// the identity check); kept separate so the tests can arm a fabricated object without a
/// loaded soundsystem.dll.
///
/// The three registrations are all-or-nothing: when one is refused, the ones already
/// registered are removed again before the error is answered, so `snd_installed()` never
/// reports a half-armed wrap.
fn snd_arm(obj: *mut c_void) -> Result<(), OpErr> {
    let mut slot = snd_lock();
    if slot.is_some() {
        return Ok(());
    }
    // mcp_bridge_game.inc:1013: the pinned vftable `obj_is` just matched, for KHook's
    // `SetupVirtualHook`.
    let vt = (modules_base(pins::PIN_SOUND) + pins::SNV_SOUNDOPSYSTEM_VT.value) as *mut *mut c_void;
    // mcp_bridge_game.inc:1022-1023: record the object and the original before the hooks are
    // published (the C's order). Both persist after a disarm, as the C's do.
    SOS_OBJ.store(obj, Ordering::Release);
    SOS_ORIG_VT.store(vt.cast(), Ordering::Release);
    // A new wrap: any three still waiting for their deferred removal are now inert, and the
    // callbacks about to be published carry the new generation.
    let generation = next_generation();

    // The C's order: the with-params overload (11), the by-name one (12), the plain start (13).
    // Each is an observer: no `pre` callbacks, one `post`. A refused registration undoes the
    // ones before it, so `snd_installed()` never reports a half-armed wrap.
    let params_cbs: Vec<snd_params_hook::PostCb> = vec![Arc::new(
        move |call: &mut HookCall,
              this: *mut c_void,
              params: *const sys::SosStartSoundEventParams_t| {
            params_post(generation, call, this, params)
        },
    )];
    // SAFETY: `vt` is the pinned `SNV_SOUNDOPSYSTEM_VT` table inside soundsystem.dll (the
    // module gate verified the module and `obj_is` identity-checked the object) and the index
    // is the generated slot of `StartSoundEvent_2`, so the entry has exactly the declared
    // `SoundEventGuid_t f(this, const SosStartSoundEventParams_t *)` signature. Installed from
    // the main thread with `SetupMode::Sync`; the target is not on this stack.
    let params: Hook = match unsafe {
        snd_params_hook::setup_virtual(
            vt,
            sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32,
            Vec::new(),
            params_cbs,
            None,
            SetupMode::Sync,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            return Err(registration_error("ISoundOpSystem::StartSoundEvent_2", &error, ""));
        }
    };
    let by_name_cbs: Vec<snd_by_name_hook::PostCb> = vec![Arc::new(
        move |call: &mut HookCall,
              this: *mut c_void,
              name: *const c_char,
              params: *mut sys::SosStartSoundEventParams_t| {
            by_name_post(generation, call, this, name, params)
        },
    )];
    // SAFETY: as above, for the generated slot of `StartSoundEvent_1` and its declared
    // `SoundEventGuid_t f(this, const char *, SosStartSoundEventParams_t *)` signature.
    let by_name: Hook = match unsafe {
        snd_by_name_hook::setup_virtual(
            vt,
            sys::ISoundOpSystem_slots::StartSoundEvent_1 as i32,
            Vec::new(),
            by_name_cbs,
            None,
            SetupMode::Sync,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            retire_generation();
            let detail = snd_remove(params);
            return Err(registration_error(
                "ISoundOpSystem::StartSoundEvent_1",
                &error,
                &format!("; StartSoundEvent_2 was {detail}"),
            ));
        }
    };
    let start_cbs: Vec<snd_start_hook::PostCb> = vec![Arc::new(
        move |call: &mut HookCall,
              this: *mut c_void,
              name: *const c_char,
              entindex: sys::CEntityIndex,
              layer: u8| { start_post(generation, call, this, name, entindex, layer) },
    )];
    // SAFETY: as above, for the generated slot of `StartSoundEvent` and its declared
    // `SoundEventGuid_t f(this, const char *, CEntityIndex, uint8)` signature.
    let start: Hook = match unsafe {
        snd_start_hook::setup_virtual(
            vt,
            sys::ISoundOpSystem_slots::StartSoundEvent as i32,
            Vec::new(),
            start_cbs,
            None,
            SetupMode::Sync,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            retire_generation();
            let by_name_detail = snd_remove(by_name);
            let params_detail = snd_remove(params);
            return Err(registration_error(
                "ISoundOpSystem::StartSoundEvent",
                &error,
                &format!(
                    "; StartSoundEvent_1 was {by_name_detail}, StartSoundEvent_2 was \
                     {params_detail}"
                ),
            ));
        }
    };
    *slot = Some(SndHooks {
        params,
        by_name,
        start,
    });
    log::line("sound: hook installed (KHook CSoundOpSystem StartSoundEvent x3, post)"); // :1025
    Ok(())
}

/// Remove one hook synchronously and answer the [`Removal`] outcome for a log line.
fn snd_remove(hook: Hook) -> String {
    // The target is not on this stack (main-thread drain or `PreShutdown`), so a synchronous
    // removal is the right one: it runs the removal function before returning, which releases
    // the context KHook was holding (`s2sdk::hook::Hook::remove_sync`).
    match hook.remove_sync() {
        Ok(removal) => removal_name(removal).to_string(),
        Err(error) => format!("remove failed: {error}"),
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

/// `snd_install` (`mcp_bridge_game.inc:1006-1027`): resolve the interface, identity-check it,
/// hook the three start slots. Idempotent on an armed wrap.
fn snd_install() -> Result<(), OpErr> {
    if snd_installed() {
        return Ok(());
    }
    // mcp_bridge_game.inc:1008-1012
    let obj = mod_iface(pins::PIN_SOUND, sys::SOUNDOPSYSTEM_INTERFACE_VERSION);
    if !obj_is(obj, pins::PIN_SOUND, pins::SNV_SOUNDOPSYSTEM_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "SoundOpSystem001 is not the pinned CSoundOpSystem",
        ));
    }
    snd_arm(obj)
}

/// Disarm the wrap: the C's order is the plain start first, then the by-name one, then the
/// with-params one (`13 -> 12 -> 11`). Answers what happened for the teardown log line, or
/// `None` when nothing was armed.
fn snd_disarm() -> Option<String> {
    // FIRST: retire the generation. The three removals below can be deferred when this runs
    // from a thunk (lane E/A2), and a callback KHook still holds must record nothing from here
    // on - whatever a later arm publishes.
    retire_generation();
    let SndHooks {
        params,
        by_name,
        start,
    } = snd_lock().take()?;
    let start = snd_remove(start);
    let by_name = snd_remove(by_name);
    let params = snd_remove(params);
    Some(format!(
        "removed 3 hooks (StartSoundEvent: {start}, StartSoundEvent_1: {by_name}, \
         StartSoundEvent_2: {params})"
    ))
}

/// `snd_uninstall` (`mcp_bridge_game.inc:1028-1032`).
pub fn snd_uninstall() {
    if snd_disarm().is_some() {
        log::line("sound: hook removed"); // :1031
    }
}

/// `sound`'s entry in [`crate::hooks`]: disarm the wrap, and report what happened for the one
/// log line the teardown writes. The listener object is not a hook and has its own owner.
///
/// Idempotent: with nothing armed the answer is `"not armed (no-op)"` and no state changes.
pub(crate) fn teardown() -> TeardownReport {
    let detail = match snd_disarm() {
        Some(detail) => detail,
        None => "not armed (no-op)".to_string(),
    };
    TeardownReport::new("sound", detail)
}

/// The C's listener-table padding (`mcp_bridge_game.inc:1042` `#define SND_LISTENER_SLOTS
/// 16 /* listener-object vtable shape (b): not in the sigs DB yet (bridge-owned object) */`):
/// the C registers the listener object with a 16-entry bridge-owned vtable so a game update
/// that adds virtuals to `ISoundEventListener` still reads inside the table. 16 is that
/// padding constant, not a measured pin; the generated `ISoundEventListener_slots::VTLEN`
/// is today's real slot count.
const SND_LISTENER_SLOTS: usize = 16;

/// The static table's length: `max(VTLEN, SND_LISTENER_SLOTS)` — the generated `VTLEN`
/// entries the real shape has today, padded out to the C's 16 (`:1043`'s
/// `static void *g_sndLvt[SND_LISTENER_SLOTS]`) with no-op stubs in every slot at or past
/// `VTLEN` (the C's `:1078` fill puts `snd_l_noop` in all 16 before the one real slot is
/// set, so its padding slots are no-ops too).
const SND_LVT_LEN: usize = if sys::ISoundEventListener_slots::VTLEN > SND_LISTENER_SLOTS {
    sys::ISoundEventListener_slots::VTLEN
} else {
    SND_LISTENER_SLOTS
};

// The real thunk's slot is inside the table the object points at (the same shape as the three
// start hooks' in-table assertions above).
const _: () = assert!(sys::ISoundEventListener_slots::OnSoundEventStarted < SND_LVT_LEN);

/// The C's `g_sndLvt` (`mcp_bridge_game.inc:1043`), filled at install; written on the main
/// thread before registration, read by the game afterwards (the same static-array shape).
struct SndLvt(UnsafeCell<[*const c_void; SND_LVT_LEN]>);
// SAFETY: the access contract is the C's (`:1078`): the table is filled once on the main
// thread BEFORE the object is registered, and the game then only reads it.
unsafe impl Sync for SndLvt {}
static SND_LVT: SndLvt = SndLvt(UnsafeCell::new([core::ptr::null(); SND_LVT_LEN]));

/// The listener **object** the game holds (`mcp_bridge_game.inc:1080-1081`, `object = { vptr }`):
/// one word whose value is the **address of the class's vtable**, never the table itself. A C++
/// object's first word is a pointer to its table (that is what `mov rcx,[rbx]; mov rax,[rcx]`
/// in soundsystem's listener walk reads); registering a pointer *to the table* - what
/// [`listener_object`] answered until 2026-10-20 - made that walk read the table's first entry
/// (a code pointer) as a vptr and call `[code + 0x18]` (byte offset 0x18 = slot 3,
/// `OnLibraryStackFieldsMessages`, a no-op here), which faulted on the first sound event after
/// `sound.status {mode:"both"}`.
#[repr(C)]
struct SndListener {
    /// The object's first - and only - member: the table's address, not the table. Set by
    /// [`snd_lvt_fill`], the step that must run before the object is registered.
    vptr: *const [*const c_void; SND_LVT_LEN],
}

/// [`SndListener`]'s storage, with the same write-once-then-read contract as [`SndLvt`].
struct SndListenerCell(UnsafeCell<SndListener>);
// SAFETY: as `SndLvt`: `snd_lvt_fill` writes the vptr on the main thread in the same step that
// fills the table, before the object's address is registered; the game then only reads it.
unsafe impl Sync for SndListenerCell {}
static SND_LISTENER: SndListenerCell = SndListenerCell(UnsafeCell::new(SndListener {
    vptr: core::ptr::null(),
}));

/// The address registered as the `ISoundEventListener*` (`:1081`'s `g_sndListener`): the
/// [`SndListener`] object, whose only word is the table's address (set by [`snd_lvt_fill`]).
fn listener_object() -> *const c_void {
    SND_LISTENER.0.get().cast()
}

/// `g_sndListenerOn` (`mcp_bridge_game.inc:1045`).
static SND_LISTENER_ON: AtomicBool = AtomicBool::new(false);

/// `snd_l_noop` for the one-pointer slots (`mcp_bridge_game.inc:1046`): the C casts one
/// 0-argument function into every slot; the Rust noops are typed to each slot's real
/// signature (same no-op behaviour, no wire difference).
unsafe extern "C" fn snd_l_noop1(_self: *mut c_void, _arg: *mut c_void) {}

/// `snd_l_on_start` (`mcp_bridge_game.inc:1047-1063`): slot 5, called after every
/// successful `StartSoundEvent_Internal` on the sound thread. Reads the name and the
/// candidate source entity while the instance is alive, through the pinned original
/// vtable (`:1052-1056`).
#[allow(unused_assignments)] // the Itanium arm assigns `tmp` outright; the init is MSVC's
unsafe extern "C" fn snd_l_on_start(_self: *mut c_void, guid: u32, aux0: u32, aux1: u32) {
    guard((), || {
        // SAFETY: the pointer arithmetic below stays inside the resolved CSoundOpSystem
        // object (SOS_OBJ), whose identity was checked by `snd_sos_resolve`.
        unsafe {
            let mut name: *const u8 = core::ptr::null();
            let mut inst: i32 = -1;
            let sos = SOS_OBJ.load(Ordering::Relaxed);
            if !sos.is_null() {
                let vt = pinned_op_vt();
                // mcp_bridge_game.inc:1053-1054: slot 68 GetSoundEventNameByGuid.
                let get_name = (*vt).GetActiveSoundEventName
                    .expect("ISoundOpSystem::GetActiveSoundEventName (pinned slot 68)");
                let n = get_name(sos.cast(), sys::SoundEventGuid_t { m_Data: guid as i32 });
                let n = if !n.is_null() && mem_ok(n.cast()) { n } else { core::ptr::null() };
                // mcp_bridge_game.inc:1055-1057: slot 86, the instance int (a candidate
                // source entity, unverified - the C's comment, `:1039-1040`).
                let get_inst = (*vt).GetSoundEventSourceEntity
                    .expect("ISoundOpSystem::GetSoundEventSourceEntity (pinned slot 86)");
                let mut tmp: i32 = -1;
                #[cfg(windows)]
                {
                    get_inst(
                        sos.cast(),
                        &mut tmp as *mut i32 as *mut sys::CEntityIndex,
                        sys::SoundEventGuid_t { m_Data: guid as i32 },
                    );
                }
                #[cfg(not(windows))]
                {
                    tmp = get_inst(sos.cast(), sys::SoundEventGuid_t { m_Data: guid as i32 })._index;
                }
                inst = tmp;
                name = n.cast();
            }
            let mut ev = SndEvt::ZERO;
            snd_ev_init(
                &mut ev,
                LISTENER_SLOT,
                0,
                name,
                -1,
                guid,
                0,
            );
            ev.aux = [aux0, aux1]; // :1061
            ev.inst150 = inst;
            snd_push(&mut ev);
        }
    });
}

/// `snd_listener_install`'s table fill (`mcp_bridge_game.inc:1078-1079`): `snd_l_noop`
/// in every slot, then the real thunk in `OnSoundEventStarted`. The padding slots at or
/// past the generated `VTLEN` (see [`SND_LVT_LEN`]) stay the no-op, as the C's 16-slot
/// fill leaves them.
fn snd_lvt_fill() {
    // SAFETY: the table is ours (filled before registration; the Sync contract above).
    let lvt = unsafe { &mut *SND_LVT.0.get() };
    for (i, slot) in lvt.iter_mut().enumerate() {
        *slot = if i == sys::ISoundEventListener_slots::OnSoundEventStarted {
            (snd_l_on_start as unsafe extern "C" fn(*mut c_void, u32, u32, u32)) as *const c_void
        } else {
            (snd_l_noop1 as unsafe extern "C" fn(*mut c_void, *mut c_void)) as *const c_void
        };
    }
    // mcp_bridge_game.inc:1080-1081: the object is `{ vptr }` and its one word is the table's
    // OWN ADDRESS. Set here, in the same main-thread step that fills the table, so the two are
    // never published apart: the object is registered only after this returns.
    // SAFETY: the listener's write contract (`SndListenerCell`'s `Sync`): main thread, before
    // registration.
    unsafe {
        (*SND_LISTENER.0.get()).vptr = lvt as *const [*const c_void; SND_LVT_LEN];
    }
}

/// `snd_sos_resolve` (`mcp_bridge_game.inc:1064-1074`): `g_sosObj`/`g_sosOrigVt` without
/// installing anything.
fn snd_sos_resolve() -> Result<(), OpErr> {
    if !SOS_OBJ.load(Ordering::Relaxed).is_null() {
        return Ok(());
    }
    // mcp_bridge_game.inc:1066-1070
    let obj = mod_iface(pins::PIN_SOUND, sys::SOUNDOPSYSTEM_INTERFACE_VERSION);
    if !obj_is(obj, pins::PIN_SOUND, pins::SNV_SOUNDOPSYSTEM_VT.value as u32) {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "SoundOpSystem001 is not the pinned CSoundOpSystem",
        ));
    }
    // SAFETY: the gate verified soundsystem.dll (the drain's route), so base + the pinned
    // vftable RVA is the pinned table inside the image.
    let vt = (modules_base(pins::PIN_SOUND) + pins::SNV_SOUNDOPSYSTEM_VT.value) as *mut c_void;
    SOS_OBJ.store(obj, Ordering::Release);
    SOS_ORIG_VT.store(vt, Ordering::Release);
    Ok(())
}

/// `snd_listener_install` (`mcp_bridge_game.inc:1075-1086`): fill the bridge-owned vtable,
/// register the object through the pinned slot 81.
fn snd_listener_install() -> Result<(), OpErr> {
    if SND_LISTENER_ON.load(Ordering::Relaxed) {
        return Ok(());
    }
    snd_sos_resolve()?;
    snd_lvt_fill();
    // mcp_bridge_game.inc:1082: AddSoundEventListener through the pinned original vtable.
    // SAFETY: `vt` is the pinned CSoundOpSystem vftable (proved by `snd_sos_resolve`'s
    // `obj_is`), `sos` the resolved object, and the listener object is the static above.
    unsafe {
        let vt = pinned_op_vt();
        let add = (*vt).AddSoundEventListener
            .expect("ISoundOpSystem::AddSoundEventListener (pinned slot 81)");
        add(
            SOS_OBJ.load(Ordering::Relaxed).cast(),
            listener_object() as *mut sys::ISoundEventListener,
        );
    }
    SND_LISTENER_ON.store(true, Ordering::Release);
    log::line("sound: listener registered (ISoundOpSystem slot 81)"); // :1084
    Ok(())
}

/// `snd_listener_uninstall` (`mcp_bridge_game.inc:1087-1092`).
pub fn snd_listener_uninstall() {
    if !SND_LISTENER_ON.load(Ordering::Relaxed) {
        return;
    }
    // mcp_bridge_game.inc:1089: RemoveSoundEventListener through the pinned original vtable.
    // SAFETY: as `snd_listener_install`'s (the listener was registered and never
    // unregistered twice - the flag guards this).
    unsafe {
        let vt = pinned_op_vt();
        let remove = (*vt).RemoveSoundEventListener
            .expect("ISoundOpSystem::RemoveSoundEventListener (pinned slot 82)");
        remove(
            SOS_OBJ.load(Ordering::Relaxed).cast(),
            listener_object() as *mut sys::ISoundEventListener,
        );
    }
    SND_LISTENER_ON.store(false, Ordering::Release);
    log::line("sound: listener removed (ISoundOpSystem slot 82)"); // :1091
}

/// The listener object's entry in [`crate::hooks`]: the bridge-owned listener is not a KHook
/// hook (nothing in a game table is patched), but it is one of the C's four teardown steps in
/// order (`snd_uninstall; snd_listener_uninstall; sr_set(0)`), so it is an owner here too.
///
/// Idempotent: with nothing registered the answer is `"not registered (no-op)"`.
pub(crate) fn listener_teardown() -> TeardownReport {
    let registered = SND_LISTENER_ON.load(Ordering::Acquire);
    if registered {
        snd_listener_uninstall();
    }
    TeardownReport::new(
        "sound listener",
        if registered {
            "removed (ISoundOpSystem slot 82)"
        } else {
            "not registered (no-op)"
        },
    )
}

/// The reads of `snd_status_json` (`mcp_bridge_game.inc:1094-1102`) over plain values.
pub struct SndStatusRead {
    /// `snd_installed()`.
    pub installed: bool,
    /// `g_sndListenerOn`.
    pub listener_on: bool,
    /// `g_sndCount`.
    pub pending: u32,
    /// `g_sndTotal`.
    pub total: u32,
    /// `g_sndDropped`.
    pub dropped: u32,
    /// Whether the reply carries the `"blockedOn"` note (the wrap was wanted but there is no
    /// KHook API - see [`op_sound`]). Not a C member: the C always had its `IKHook`.
    pub blocked_on: bool,
}

/// The `"blockedOn"` member's value when a report row could not arm the wrap: the same text
/// [`crate::control::khook_missing`] answers, but here as a note in a normal reply (the row is
/// an off-ramp and must keep answering after a Metamod unload).
fn add_blocked_on(out: &mut Sb<'_>) {
    out.add(b",\"blockedOn\":");
    out.addjson(crate::registry::KHOOK_BLOCKED_ON.as_bytes());
}

/// `snd_status_json` (`mcp_bridge_game.inc:1094-1102`) over the given reads.
pub fn snd_status_json_in(read: &SndStatusRead, out: &mut Sb<'_>) {
    out.add(b"{\"installed\":");
    out.add(if read.installed || read.listener_on { b"true" } else { b"false" }); // :1095
    // mcp_bridge_game.inc:1096: "both" | "wrap" | "listener" | null (unquoted null)
    out.add(b",\"mode\":");
    out.add(if read.installed && read.listener_on {
        b"\"both\""
    } else if read.installed {
        b"\"wrap\""
    } else if read.listener_on {
        b"\"listener\""
    } else {
        b"null"
    });
    out.add(b",\"pending\":");
    out.addnum(u64::from(read.pending)); // :1097
    out.add(b",\"total\":");
    out.addnum(u64::from(read.total)); // :1098
    out.add(b",\"dropped\":");
    out.addnum(u64::from(read.dropped)); // :1099
    out.add(b",\"ringSize\":");
    out.addnum(u64::from(SND_RING)); // :1100
    // This build's member, appended last so every C member keeps its position and bytes.
    if read.blocked_on {
        add_blocked_on(out);
    }
    out.add(b"}");
}

/// The live [`snd_status_json_in`]. The C reads the counters without its lock (`:1097`);
/// the Rust ring is behind the mutex, so this takes it (memory-safety fix, same bytes).
fn snd_status_json(out: &mut Sb<'_>, blocked_on: bool) {
    let read = {
        let ring = ring_lock();
        SndStatusRead {
            installed: snd_installed(),
            listener_on: SND_LISTENER_ON.load(Ordering::Relaxed),
            pending: ring.count,
            total: ring.total,
            dropped: ring.dropped,
            blocked_on,
        }
    };
    snd_status_json_in(&read, out);
}

/// `sound.drain`'s body (`mcp_bridge_game.inc:1119-1204`), `out` over the arm's 98304-byte
/// buffer (the C reads `out->cap` in the per-event rollback rule, `:1184`).
fn snd_drain(params: Option<&[u8]>, blocked_on: bool, out: &mut Sb<'_>) -> Result<(), OpErr> {
    // mcp_bridge_game.inc:1120-1124
    let mut max: i64 = 128;
    let mut resolve = true;
    if let Some(p) = params {
        if let Some(m) = json_int(p, b"max") {
            max = m;
        }
        if let Some(r) = json_bool(p, b"resolve") {
            resolve = r;
        }
    }
    // mcp_bridge_game.inc:1123-1124
    if max < 1 {
        max = 1;
    } else if max > i64::from(SND_RING) {
        max = i64::from(SND_RING);
    }
    // mcp_bridge_game.inc:1125-1130: pop under the ring lock, then read-and-clear
    // `g_sndDropped` in the same critical section.
    let (batch, dropped, total) = {
        let mut ring = ring_lock();
        ring.ensure();
        let mut batch: Vec<SndEvt> = Vec::with_capacity(max as usize);
        while batch.len() < max as usize && ring.count != 0 {
            batch.push(ring.slots[ring.head as usize]);
            ring.head = (ring.head + 1) % SND_RING;
            ring.count -= 1;
        }
        let dropped = ring.dropped;
        let total = ring.total;
        ring.dropped = 0;
        (batch, dropped, total)
    };
    // mcp_bridge_game.inc:1131-1132: the scope is optional; a failed one clears the error.
    let have_scope = if resolve {
        let mut gs = gs_lock();
        let mut e = OpErr::bare(0, "");
        gs_scope(&mut gs, &mut e)
    } else {
        false
    };
    let n = batch.len() as u32;
    let now = crate::wiring::mono_ms(); // GetTickCount64(), :1133
    out.add(b"{\"events\":["); // :1134
    let mut k = 0u32; // events actually written
    while (k as usize) < batch.len() {
        // mcp_bridge_game.inc:1139: each event is written whole or rolled back.
        let mark = out.bytes().len();
        let ev = &batch[k as usize];
        if k != 0 {
            out.add(b",");
        }
        // mcp_bridge_game.inc:1175-1181: the optional position/entity-name resolution, read
        // in the C's order (entity by index, origin, designer name) and handed to the pure
        // builder prebuilt. The C writes the origin between the two reads; here the writes
        // happen after - the reads and their order are unchanged (a designer name is a pure
        // read; no wire difference).
        let scope = if have_scope && ev.entindex >= 0 {
            let mut gs = gs_lock();
            let ent = gs_ent_by_index(&mut gs, ev.entindex as u32);
            let mut o = [0f32; 3];
            let ho = !ent.is_null() && gs_origin(&mut gs, ent, &mut o);
            let en = if ent.is_null() { None } else { gs_designer_name(&mut gs, ev.entindex as u32) };
            Some(EventScope {
                origin: ho.then_some(o),
                entity_name: en.map(|n| n.as_bytes().to_vec()),
            })
        } else {
            None
        };
        // `now.wrapping_sub(ev.ms)`: the C's unsigned `now - ev->ms` (`:1133`, the
        // `(long long)` cast at `:1149`) wraps on the clock's wrap instead of panicking.
        snd_event_json(ev, now.wrapping_sub(ev.ms), scope, out);
        // mcp_bridge_game.inc:1183-1187: roll back an event that overflowed or left < 256 B
        // for the tail (the tail is < 100 B).
        if out.ovf() || out.bytes().len() + 256 > out.cap() {
            out.rewind(mark);
            break;
        }
        k += 1;
    }
    // mcp_bridge_game.inc:1189-1198: requeue the unwritten tail at the FRONT (oldest first);
    // room/skip computed under the lock.
    let m = n - k;
    let pending = {
        let mut ring = ring_lock();
        ring.ensure();
        let room = SND_RING - ring.count;
        let skip = m.saturating_sub(room);
        ring.dropped += skip;
        let mut i = n as usize;
        while i > (k + skip) as usize {
            i -= 1;
            ring.head = (ring.head + SND_RING - 1) % SND_RING;
            let head = ring.head as usize;
            ring.slots[head] = batch[i];
            ring.count += 1;
        }
        ring.count
    };
    // mcp_bridge_game.inc:1199-1203
    out.add(b"],\"truncated\":");
    out.add(if k < n { b"true" } else { b"false" });
    out.add(b",\"dropped\":");
    out.addnum(u64::from(dropped));
    out.add(b",\"total\":");
    out.addnum(u64::from(total));
    out.add(b",\"pending\":");
    out.addnum(u64::from(pending));
    // This build's member (see `add_blocked_on`): the drain ran, but the wrap it was asked to
    // arm is not installed because no KHook API is loaded.
    if blocked_on {
        add_blocked_on(out);
    }
    out.add(b"}");
    Ok(())
}

/// A NUL-terminated buffer's byte length (the C's string semantics over the fixed array).
fn cstr_len(buf: &[u8]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(buf.len())
}

/// The optional per-event scope block (`mcp_bridge_game.inc:1175-1181`), read off game
/// memory by the drain and handed to [`snd_event_json`] as a plain value.
#[derive(Clone, Default)]
pub struct EventScope {
    /// `"origin"`: the entity's Hammer origin, `None` when unreadable (`gs_add_kv_v3`'s `ho`).
    pub origin: Option<[f32; 3]>,
    /// `"entityName"`: the entity's designer name, `None` when unreadable.
    pub entity_name: Option<Vec<u8>>,
}

/// One event's JSON object (`mcp_bridge_game.inc:1141-1182`), the pure builder the host
/// tests pin byte-for-byte (`age_ms` = `(long long)(now - ev->ms)`).
pub fn snd_event_json(ev: &SndEvt, age_ms: u64, scope: Option<EventScope>, out: &mut Sb<'_>) {
    out.add(b"{");
    add_kv_i(out, b"seq", i64::from(ev.seq)); // :1143
    out.add(b",");
    add_kv_i(out, b"hash", i64::from(ev.hash)); // (long long)ev->hash, :1144
    out.add(b",");
    add_kv_str(out, b"name", (ev.name[0] != 0).then(|| &ev.name[..cstr_len(&ev.name)])); // :1145
    out.add(b",");
    add_kv_i(out, b"entindex", i64::from(ev.entindex)); // :1146
    out.add(b",");
    add_kv_i(out, b"guid", i64::from(ev.guid)); // :1147
    out.add(b",");
    add_kv_i(out, b"via", i64::from(ev.slot)); // :1148
    out.add(b",");
    // mcp_bridge_game.inc:1149: the wire key stays "flags"; the byte is the voice layer.
    add_kv_i(out, b"flags", i64::from(ev.layer));
    out.add(b",");
    add_kv_i(out, b"ageMs", age_ms as i64); // :1150
    // mcp_bridge_game.inc:1151-1170: the packed start params, slot-11 starts only.
    if ev.slot == START_WITH_PARAMS_SLOT {
        out.add(b",\"params\":");
        if ev.plen < 0 {
            out.add(b"null");
        } else {
            out.add(b"{\"len\":");
            out.addnum(u64::from(ev.plen as u32)); // sb_addnum((u32)ev->plen)
            out.add(b",\"hex\":\"");
            for q in 0..ev.pcopied as usize {
                out.add(&format!("{:02x}", ev.praw[q]).into_bytes());
            }
            out.add(b"\",\"truncated\":");
            out.add(if ev.pcopied < ev.plen as u16 { b"true" } else { b"false" });
            out.add(b"}");
        }
    }
    // mcp_bridge_game.inc:1171-1174: the slot-81 (listener) event's aux guids + the
    // candidate source entity.
    if ev.slot == LISTENER_SLOT {
        out.add(b",\"aux\":[");
        out.addnum(u64::from(ev.aux[0]));
        out.add(b",");
        out.addnum(u64::from(ev.aux[1]));
        out.add(b"]");
        out.add(b",");
        add_kv_i(out, b"inst150", i64::from(ev.inst150));
    }
    if let Some(scope) = scope {
        out.add(b",");
        add_kv_v3(out, b"origin", scope.origin.as_ref()); // :1179
        out.add(b",");
        add_kv_str(out, b"entityName", scope.entity_name.as_deref()); // :1180
    }
    out.add(b"}"); // :1182
}

/// The C's `op_sound` (`mcp_bridge_game.inc:1103-1205`).
fn op_sound(method: &[u8], params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    // mcp_bridge_game.inc:1104
    if method == b"sound.uninstall" {
        snd_uninstall();
        snd_listener_uninstall();
        snd_status_json(out, false);
        return Ok(());
    }
    // `sound.subscribe` is the group's arming row (the one that carries `NEED_KHOOK`): it is
    // where the C's mode selection always runs. A `{mode:"listener"}` request needs no KHook
    // hook, but the gate bit is per ROW, not per request: a row that sometimes installs a hook
    // must advertise the requirement, or `control.capabilities` would advertise a row that can
    // fail on its first wrap call. It keeps the bit; see `registry`'s `NEED_KHOOK` docs.
    if method == b"sound.subscribe" {
        sound_mode(params)?;
        snd_status_json(out, false); // :1118
        return Ok(());
    }
    // `sound.status` / `sound.drain` run the same selection (real clients rely on the first of
    // them arming the wrap: `client-mcp`'s `drain_sounds`/`sounds_status`, `hammer-mcp`'s
    // `installOnly`, `bridge/test_sound_params.py`), but they carry no `NEED_KHOOK` bit, so
    // they must still answer after a Metamod unload: without the API the wrap half of the
    // selection is skipped (the wrap cannot be armed without it) while the listener half - a
    // bridge-owned object that needs no KHook - still follows `mode` (the C's `{mode:"both"}`
    // installs both), and the reply says `"blockedOn"` - a note, not an error.
    let blocked = s2sdk::hook::api().is_none() && mode_wants_wrap(params)?;
    sound_mode_select(params, !blocked)?;
    if method == b"sound.status" {
        snd_status_json(out, blocked); // :1118
        return Ok(());
    }
    snd_drain(params, blocked, out)
}

/// Whether the C's sink selection (`:1105-1113`) would install the wrap for `params`: `mode`
/// absent means wrap (or, when a sink is already armed, the wrap's current state - which
/// without an API is never on). A malformed `mode` answers the C's `-32602`, the same check
/// [`sound_mode`] makes, so the report rows validate it before anything else.
fn mode_wants_wrap(params: Option<&[u8]>) -> Result<bool, OpErr> {
    let mut mode = [0u8; 16]; // :1106, static char mode[16]
    if let Some(p) = params {
        json_string(p, b"mode", &mut mode);
    }
    let mode = &mode[..cstr_len(&mode)];
    match mode {
        b"listener" => Ok(false),
        b"both" | b"wrap" => Ok(true),
        b"" => {
            if snd_installed() || SND_LISTENER_ON.load(Ordering::Relaxed) {
                // :1113: absent = keep the current sink selection.
                Ok(snd_installed())
            } else {
                Ok(true) // :1105: default = wrap
            }
        }
        // mcp_bridge_game.inc:1112
        _ => Err(OpErr::new(
            -32602,
            "InvalidParams",
            "mode must be wrap|listener|both",
        )),
    }
}

/// The C's sink selection (`mcp_bridge_game.inc:1105-1117`): `mode` = wrap | listener | both,
/// absent = keep the current state (or the wrap when nothing is armed).
fn sound_mode(params: Option<&[u8]>) -> Result<(), OpErr> {
    sound_mode_select(params, true)
}

/// [`sound_mode`], with the wrap half optional: the listener half is a **bridge-owned** object
/// that needs no KHook, so a report row with no API still honours `mode` for it - the C's
/// `{mode:"both"}` installs both, and only the wrap half is what this build cannot do without
/// the API (`mode_wants_wrap`). With `wrap_available` false the wrap's state is left alone: it
/// cannot have been armed without the API, and an uninstall there would be a teardown the row
/// does not own.
fn sound_mode_select(params: Option<&[u8]>, wrap_available: bool) -> Result<(), OpErr> {
    // mcp_bridge_game.inc:1105-1113: the sink selection; default = wrap until the
    // listener's entity field is verified.
    let mut mode = [0u8; 16]; // :1106, static char mode[16] (re-zeroed every call)
    if let Some(p) = params {
        json_string(p, b"mode", &mut mode);
    }
    let mode = &mode[..cstr_len(&mode)];
    let mut want_wrap = true;
    let mut want_listener = false;
    if mode == b"listener" {
        want_wrap = false;
        want_listener = true;
    } else if mode == b"both" {
        want_wrap = true;
        want_listener = true;
    } else if !mode.is_empty() && mode != b"wrap" {
        // mcp_bridge_game.inc:1112
        return Err(OpErr::new(-32602, "InvalidParams", "mode must be wrap|listener|both"));
    }
    // mcp_bridge_game.inc:1113
    if mode.is_empty() && (snd_installed() || SND_LISTENER_ON.load(Ordering::Relaxed)) {
        want_wrap = snd_installed();
        want_listener = SND_LISTENER_ON.load(Ordering::Relaxed);
    }
    // mcp_bridge_game.inc:1114-1117
    if wrap_available {
        if want_wrap {
            snd_install()?;
        }
        if !want_wrap {
            snd_uninstall();
        }
    }
    if want_listener {
        snd_listener_install()?;
    }
    if !want_listener {
        snd_listener_uninstall();
    }
    Ok(())
}

/// The dispatch arm's tail over the group's 98304-byte buffer (`mcp_bridge.c:6029-6041`),
/// with the C arm's own overflow message.
fn sound(r: &Request) -> String {
    let mut buf = vec![0u8; SND_BUF]; // mcp_bridge.c:6032
    let mut sb = Sb::new(&mut buf);
    let outcome = op_sound(r.method.as_bytes(), r.params.as_deref().map(str::as_bytes), &mut sb);
    finish(&r.id, outcome, &sb, SND_OVERFLOW)
}

/// The group's handler table (the C table's `sound.*` rows,
/// `mcp_bridge.c:1080-1083`).
pub static HANDLERS: &[(&str, Handler)] = &[
    ("sound.status", sound),
    ("sound.subscribe", sound),
    ("sound.drain", sound),
    ("sound.uninstall", sound),
];

/// [`snd_record`] for the host tests.
#[doc(hidden)]
pub fn snd_record_for_test(slot: u8, hash: u32, name: *const u8, entindex: i32, guid: u32, layer: u8) {
    snd_record(slot, hash, name, entindex, guid, layer);
}

/// [`snd_drain`] for the host tests.
#[doc(hidden)]
pub fn snd_drain_for_test(params: Option<&[u8]>, out: &mut Sb<'_>) -> Result<(), OpErr> {
    snd_drain(params, false, out)
}

/// [`snd_lvt_fill`] for the host tests (`snd_listener_install` needs a game; the table
/// fill does not).
#[doc(hidden)]
pub fn snd_lvt_fill_for_test() {
    snd_lvt_fill();
}

/// The bridge-owned listener table's words plus the no-op's address, for the host test of
/// the padding slots (entries at or past the generated `VTLEN` are the no-op, as the C's
/// `:1078` 16-slot fill leaves them).
#[doc(hidden)]
pub fn snd_lvt_for_test() -> (Vec<*const c_void>, *const c_void) {
    // SAFETY: a read-only copy of the static table's words; no install runs concurrently
    // (the test contract).
    let words = unsafe { core::ptr::read(SND_LVT.0.get() as *const [*const c_void; SND_LVT_LEN]) };
    (
        words.to_vec(),
        snd_l_noop1 as unsafe extern "C" fn(*mut c_void, *mut c_void) as *const c_void,
    )
}

/// [`listener_object`] and the table's address, for the host test of the object's shape (the
/// object's one word must be the table's address, not the table's first entry).
#[doc(hidden)]
pub fn snd_listener_object_for_test() -> (*const c_void, *const c_void) {
    (listener_object(), SND_LVT.0.get().cast::<c_void>())
}

/// [`snd_listener_install`] against a fabricated `g_sosObj` / `g_sosOrigVt` and a fabricated
/// vtable: the host has no soundsystem.dll, so `snd_sos_resolve`'s `mod_iface` cannot answer a
/// real `CSoundOpSystem` and the registration path is otherwise unreachable off-game.
///
/// The caller owns the two globals it set; [`snd_listener_reset_for_test`] puts them back.
#[doc(hidden)]
pub fn snd_listener_install_for_test(
    sos: *mut c_void,
    vtable: *const sys::ISoundOpSystem_vtable,
) -> Result<(), OpErr> {
    SOS_OBJ.store(sos, Ordering::Release);
    SOS_ORIG_VT.store(vtable.cast_mut().cast(), Ordering::Release);
    snd_listener_install()
}

/// Undo [`snd_listener_install_for_test`]: the listener flag is process-wide and the fabricated
/// object must not outlive its case.
#[doc(hidden)]
pub fn snd_listener_reset_for_test() {
    SND_LISTENER_ON.store(false, Ordering::Release);
    SOS_OBJ.store(core::ptr::null_mut(), Ordering::Release);
    SOS_ORIG_VT.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    //! The three start hooks' own tests, over the shared mock `KHookApi` in
    //! [`crate::hooks::mock_khook`] (it simulates KHook's per-call value fold,
    //! `detour.cpp:359-472`, well enough to drive the generated thunks): the two registration
    //! orders, the guid read from `GetOriginalValuePtr`, and the three negative paths (a foreign
    //! `this`, no saved original, no API).
    //!
    //! The trampolines are the caller's side of the ABI: the plain shape on Itanium (the value
    //! comes back in a register) and the hidden-return-buffer shape on MSVC (`sret_msvc = sret`).
    //! Compiling the MSVC arm is what proves the declared flags; it is not run on the host.

    use super::*;
    use crate::hooks::mock_khook::{self, Call};

    /// The guid the tests' "original" answers.
    const GUID: i32 = 42;

    /// A fake CSoundOpSystem for the callbacks: `SOS_OBJ` is compared by address, and the one
    /// read any body makes of it (`snd_name_of`'s manager subobject) lands inside this zeroed
    /// allocation, whose vptr word is 0 - not the pinned table, so the name lookup answers null
    /// exactly as it does in the game for an unknown hash.
    fn fake_object() -> *mut c_void {
        static MEM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        MEM.get_or_init(|| vec![0u8; 4096]).as_ptr().cast_mut().cast()
    }

    /// An empty ring, so a case's "one event" is its own.
    fn reset_ring() {
        let mut ring = ring_lock();
        *ring = Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }

    /// The params object the tests pass: a zeroed `SosStartSoundEventParams_t` with the fields
    /// the record reads set, and (when `packed` is given) a readable field-data extent.
    fn params(hash: u32, ent: i32, layer: u8, packed: Option<&mut [u8]>) -> sys::SosStartSoundEventParams_t {
        // SAFETY: the generated mirror is `#[repr(C)]` plain-old-data (pointers, integers, a
        // `SoundEventGuid_t`), so an all-zero value is a valid one - the C's zero-init.
        let mut p: sys::SosStartSoundEventParams_t = unsafe { core::mem::zeroed() };
        p.m_nSoundEventHash = hash;
        p.m_SourceEntity = sys::CEntityIndex { _index: ent };
        p.m_nLayer = layer;
        p.m_nFieldDataSize = -1; // `:965` unreadable, unless a case gives an extent
        if let Some(buf) = packed {
            p.m_pFieldData = buf.as_mut_ptr().cast();
            p.m_nFieldDataSize = buf.len() as i32;
        }
        p
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

    // -------------------------------------------------------------------------------------
    // The caller's side of one hooked call, per ABI.
    // -------------------------------------------------------------------------------------

    /// One `StartSoundEvent(params)` call: pre, the original (which answers [`GUID`]), post,
    /// make_return.
    #[cfg(not(windows))]
    fn run_params(mock: &mock_khook::Mock, this: *mut c_void, p: *const sys::SosStartSoundEventParams_t) -> Guid {
        unsafe extern "C" fn original(_this: *mut c_void, _p: *const sys::SosStartSoundEventParams_t) -> Guid {
            Guid(sys::SoundEventGuid_t { m_Data: GUID })
        }
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32),
            original as *mut c_void,
        );
        // SAFETY: the thunks are the ones this module registered for the generated
        // `SoundEventGuid_t f(this, const SosStartSoundEventParams_t *)` signature.
        unsafe {
            let _ = snd_params_hook::pre(this, p);
            let _ = snd_params_hook::make_call_original(this, p);
            let _ = snd_params_hook::post(this, p);
            let out = snd_params_hook::make_return(this, p);
            mock_khook::leave();
            out
        }
    }

    /// The MSVC twin: the hidden return buffer comes after `this` and is handed back.
    #[cfg(windows)]
    fn run_params(mock: &mock_khook::Mock, this: *mut c_void, p: *const sys::SosStartSoundEventParams_t) -> Guid {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            ret: *mut Guid,
            _p: *const sys::SosStartSoundEventParams_t,
        ) -> *mut Guid {
            // SAFETY: `ret` is the caller's return buffer.
            unsafe { ret.write(Guid(sys::SoundEventGuid_t { m_Data: GUID })) };
            ret
        }
        let mut out = Guid::default();
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32),
            original as *mut c_void,
        );
        // SAFETY: as in the plain arm, for the MSVC shape.
        unsafe {
            let _ = snd_params_hook::pre(this, &mut out, p);
            let _ = snd_params_hook::make_call_original(this, &mut out, p);
            let _ = snd_params_hook::post(this, &mut out, p);
            let out = snd_params_hook::make_return(this, &mut out, p);
            mock_khook::leave();
            *out
        }
    }

    /// The `post` of one call with **no** original saved (the path `make_call_original` never
    /// ran on): `GetOriginalValuePtr` answers null.
    #[cfg(not(windows))]
    fn run_params_without_original(mock: &mock_khook::Mock, this: *mut c_void, p: *const sys::SosStartSoundEventParams_t) {
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32),
            null_mut(),
        );
        // SAFETY: as in `run_params`; the call's value stays unset.
        unsafe {
            let _ = snd_params_hook::post(this, p);
            let _ = snd_params_hook::make_return(this, p);
            mock_khook::leave();
        }
    }

    /// The MSVC twin of [`run_params_without_original`].
    #[cfg(windows)]
    fn run_params_without_original(mock: &mock_khook::Mock, this: *mut c_void, p: *const sys::SosStartSoundEventParams_t) {
        let mut out = Guid::default();
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32),
            null_mut(),
        );
        // SAFETY: as in the plain arm; the call's value stays unset.
        unsafe {
            let _ = snd_params_hook::post(this, &mut out, p);
            let _ = snd_params_hook::make_return(this, &mut out, p);
            mock_khook::leave();
        }
    }

    #[cfg(not(windows))]
    fn run_by_name(mock: &mock_khook::Mock, this: *mut c_void, name: *const c_char, p: *mut sys::SosStartSoundEventParams_t) -> Guid {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            _name: *const c_char,
            _p: *mut sys::SosStartSoundEventParams_t,
        ) -> Guid {
            Guid(sys::SoundEventGuid_t { m_Data: GUID })
        }
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_1 as i32),
            original as *mut c_void,
        );
        // SAFETY: the thunks are the ones this module registered for the generated
        // `SoundEventGuid_t f(this, const char *, SosStartSoundEventParams_t *)` signature.
        unsafe {
            let _ = snd_by_name_hook::pre(this, name, p);
            let _ = snd_by_name_hook::make_call_original(this, name, p);
            let _ = snd_by_name_hook::post(this, name, p);
            let out = snd_by_name_hook::make_return(this, name, p);
            mock_khook::leave();
            out
        }
    }

    #[cfg(windows)]
    fn run_by_name(mock: &mock_khook::Mock, this: *mut c_void, name: *const c_char, p: *mut sys::SosStartSoundEventParams_t) -> Guid {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            ret: *mut Guid,
            _name: *const c_char,
            _p: *mut sys::SosStartSoundEventParams_t,
        ) -> *mut Guid {
            // SAFETY: `ret` is the caller's return buffer.
            unsafe { ret.write(Guid(sys::SoundEventGuid_t { m_Data: GUID })) };
            ret
        }
        let mut out = Guid::default();
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_1 as i32),
            original as *mut c_void,
        );
        // SAFETY: as in the plain arm, for the MSVC shape.
        unsafe {
            let _ = snd_by_name_hook::pre(this, &mut out, name, p);
            let _ = snd_by_name_hook::make_call_original(this, &mut out, name, p);
            let _ = snd_by_name_hook::post(this, &mut out, name, p);
            let out = snd_by_name_hook::make_return(this, &mut out, name, p);
            mock_khook::leave();
            *out
        }
    }

    #[cfg(not(windows))]
    fn run_start(
        mock: &mock_khook::Mock,
        this: *mut c_void,
        name: *const c_char,
        ent: i32,
        layer: u8,
    ) -> Guid {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            _name: *const c_char,
            ent: sys::CEntityIndex,
            _layer: u8,
        ) -> Guid {
            let _ = ent;
            Guid(sys::SoundEventGuid_t { m_Data: GUID })
        }
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent as i32),
            original as *mut c_void,
        );
        let ent = sys::CEntityIndex { _index: ent };
        // SAFETY: the thunks are the ones this module registered for the generated
        // `SoundEventGuid_t f(this, const char *, CEntityIndex, uint8)` signature.
        unsafe {
            let _ = snd_start_hook::pre(this, name, ent, layer);
            let _ = snd_start_hook::make_call_original(this, name, ent, layer);
            let _ = snd_start_hook::post(this, name, ent, layer);
            let out = snd_start_hook::make_return(this, name, ent, layer);
            mock_khook::leave();
            out
        }
    }

    #[cfg(windows)]
    fn run_start(
        mock: &mock_khook::Mock,
        this: *mut c_void,
        name: *const c_char,
        ent: i32,
        layer: u8,
    ) -> Guid {
        unsafe extern "C" fn original(
            _this: *mut c_void,
            ret: *mut Guid,
            _name: *const c_char,
            ent: sys::CEntityIndex,
            _layer: u8,
        ) -> *mut Guid {
            let _ = ent;
            // SAFETY: `ret` is the caller's return buffer.
            unsafe { ret.write(Guid(sys::SoundEventGuid_t { m_Data: GUID })) };
            ret
        }
        let mut out = Guid::default();
        mock_khook::enter(
            mock,
            mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent as i32),
            original as *mut c_void,
        );
        let ent = sys::CEntityIndex { _index: ent };
        // SAFETY: as in the plain arm, for the MSVC shape.
        unsafe {
            let _ = snd_start_hook::pre(this, &mut out, name, ent, layer);
            let _ = snd_start_hook::make_call_original(this, &mut out, name, ent, layer);
            let _ = snd_start_hook::post(this, &mut out, name, ent, layer);
            let out = snd_start_hook::make_return(this, &mut out, name, ent, layer);
            mock_khook::leave();
            *out
        }
    }

    /// A `post` call against an explicit registration context and with **no** original value:
    /// what a pair KHook still calls after a deferred removal sees.
    #[cfg(not(windows))]
    fn run_params_at(
        mock: &mock_khook::Mock,
        context: *mut c_void,
        this: *mut c_void,
        p: *const sys::SosStartSoundEventParams_t,
    ) {
        mock_khook::enter(mock, context, null_mut());
        // SAFETY: the thunks are the ones this module registered for this signature; the call's
        // value stays unset (no `make_call_original` ran), which `make_return` tolerates.
        unsafe {
            let _ = snd_params_hook::post(this, p);
            let _ = snd_params_hook::make_return(this, p);
            mock_khook::leave();
        }
    }

    /// The MSVC twin of [`run_params_at`].
    #[cfg(windows)]
    fn run_params_at(
        mock: &mock_khook::Mock,
        context: *mut c_void,
        this: *mut c_void,
        p: *const sys::SosStartSoundEventParams_t,
    ) {
        let mut out = Guid::default();
        mock_khook::enter(mock, context, null_mut());
        // SAFETY: as in the plain arm, for the MSVC shape.
        unsafe {
            let _ = snd_params_hook::post(this, &mut out, p);
            let _ = snd_params_hook::make_return(this, &mut out, p);
            mock_khook::leave();
        }
    }

    // -------------------------------------------------------------------------------------
    // The cases
    // -------------------------------------------------------------------------------------

    /// Registration failures go through one helper: the retryable `GateBusy` (and `InThunk`,
    /// which no registration produces any more) answers the `blockedOn` retry hint, every other
    /// error the plain `{"reason":...}` naming the slot.
    #[test]
    fn registration_errors_map_in_thunk_to_a_retry_hint() {
        for retryable in [
            s2sdk::hook::Error::GateBusy,
            s2sdk::hook::Error::InThunk,
        ] {
            let error =
                registration_error("ISoundOpSystem::StartSoundEvent_2", &retryable, "");
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
            "ISoundOpSystem::StartSoundEvent_2",
            &s2sdk::hook::Error::KHookNotInstalled,
            "",
        );
        assert!(
            error.data.contains(r#"{"reason":"ISoundOpSystem::StartSoundEvent_2: no KHook"#),
            "{}",
            error.data
        );
    }

    /// A pair whose removal KHook defers records nothing once the disarm has started, and a
    /// re-arm over the old, still-associated three is clean (only the new pair records).
    #[test]
    fn a_disarmed_wrap_is_inert_before_its_removal_lands() {
        mock_khook::with_mock("sound-stale-generation", |mock, _scratch| {
            let obj = fake_object();
            reset_ring();
            snd_arm(obj).expect("arm");
            let old_params = mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32);
            let p = params(0x1, 1, 0, None);

            // Disarm, but the three removals sit in KHook's delete thread (lane E/A2).
            mock.defer_removals(true);
            snd_uninstall();
            mock.defer_removals(false);
            assert!(!snd_installed());
            assert_eq!(mock.deferred_len(), 3, "all three removals are still pending");

            run_params_at(mock, old_params, obj, &p);
            assert_eq!(ring_lock().count, 0, "a retired wrap records nothing");

            // Re-arm while the old three are still associated: the new pair records, once.
            snd_arm(obj).expect("re-arm over a pending removal");
            let new_params = mock.context_of(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32);
            assert_ne!(new_params, old_params, "a new registration, a new context");
            run_params_at(mock, old_params, obj, &p);
            assert_eq!(ring_lock().count, 0, "the old pair is still inert after the re-arm");
            run_params_at(mock, new_params, obj, &p);
            assert_eq!(ring_lock().count, 0, "no original value: nothing recorded");
            let value = run_params(mock, obj, &p);
            assert_eq!(value.0.m_Data, GUID);
            assert_eq!(ring_lock().count, 1, "one event from the single live pair");
            snd_uninstall();
        });
    }

    /// `sound.status` runs the C's sink selection when the KHook API is present: with a mock
    /// API and no game the install path is reached, which is its identity refusal (the row
    /// arms exactly as `sound.subscribe` does). Without an API it answers instead of refusing.
    #[test]
    fn sound_status_arms_with_khook_and_reports_without_it() {
        mock_khook::with_mock("sound-status-arms", |_mock, _scratch| {
            reset_ring();
            let mut buf = vec![0u8; SND_BUF];
            let mut sb = Sb::new(&mut buf);
            let error = op_sound(b"sound.status", None, &mut sb)
                .expect_err("the arming path is reached and the interface is not the game's");
            assert_eq!(error.code, -32017);
            assert!(
                error.data.contains("SoundOpSystem001 is not the pinned CSoundOpSystem"),
                "the status row reached the install's identity check: {}",
                error.data
            );
            assert!(!snd_installed());
        });
        // No API: the same row answers its status object with the note and installs nothing.
        let _guard = crate::hooks::test_guard::lock();
        assert!(s2sdk::hook::api().is_none(), "no mock in this process now");
        reset_ring();
        let mut buf = vec![0u8; SND_BUF];
        let mut sb = Sb::new(&mut buf);
        op_sound(b"sound.status", None, &mut sb).expect("an off-ramp answers without KHook");
        assert_eq!(
            std::str::from_utf8(sb.as_cstr()).unwrap(),
            r#"{"installed":false,"mode":null,"pending":0,"total":0,"dropped":0,"ringSize":256,"blockedOn":"metamod: KHook not loaded"}"#
        );
        assert!(!snd_installed(), "nothing was installed");

        // An explicit wrap request reports the same; a drain does too (and still drains).
        let mut sb = Sb::new(&mut buf);
        op_sound(b"sound.status", Some(br#"{"mode":"wrap"}"#), &mut sb).expect("still answers");
        assert!(std::str::from_utf8(sb.as_cstr()).unwrap().contains("\"blockedOn\""));
        let mut sb = Sb::new(&mut buf);
        op_sound(b"sound.drain", Some(br#"{"max":4}"#), &mut sb).expect("the drain answers");
        let text = std::str::from_utf8(sb.as_cstr()).unwrap();
        assert!(
            text.starts_with(r#"{"events":[]"#) && text.contains("\"blockedOn\""),
            "{text}"
        );
        // The listener half needs no KHook, so `{mode:"both"}` still reaches the C's listener
        // install without the API - here that is its own identity refusal (no game), which is
        // what "the wrap half is skipped, the listener half is not" looks like off a game.
        // The cases above arm a fabricated object, and `snd_arm` records it as the resolved
        // sound-op object (`SOS_OBJ`/`SOS_ORIG_VT` persist after a disarm, as the C's do): clear
        // them so this case resolves against this process's modules instead of that fake.
        SOS_OBJ.store(core::ptr::null_mut(), Ordering::Relaxed);
        SOS_ORIG_VT.store(core::ptr::null_mut(), Ordering::Relaxed);
        let mut sb = Sb::new(&mut buf);
        let error = op_sound(b"sound.status", Some(br#"{"mode":"both"}"#), &mut sb)
            .expect_err("the listener install is reached and the interface is not the game's");
        assert_eq!(error.code, -32017);
        assert!(
            error.data.contains("SoundOpSystem001 is not the pinned CSoundOpSystem"),
            "the listener half ran: {}",
            error.data
        );
        assert!(
            !SND_LISTENER_ON.load(Ordering::Relaxed),
            "and registered nothing off a game"
        );
        // A malformed mode is still the C's parameter error, before any arming decision.
        let mut sb = Sb::new(&mut buf);
        let error = op_sound(b"sound.status", Some(br#"{"mode":"x"}"#), &mut sb)
            .expect_err("the mode check precedes everything");
        assert_eq!(error.code, -32602);
    }

    /// Install registers the three start slots on the pinned table, synchronously, with the
    /// generated indices and no `pre` callbacks; uninstall removes them `13 -> 12 -> 11`
    /// (the plain start first), synchronously.
    #[test]
    fn install_and_uninstall_use_the_pinned_slots_in_the_c_order() {
        mock_khook::with_mock("sound-install", |mock, _scratch| {
            let obj = fake_object();
            snd_arm(obj).expect("the mock accepts all three registrations");
            assert!(snd_installed());
            assert_eq!(
                setup_indices(mock),
                vec![
                    sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32,
                    sys::ISoundOpSystem_slots::StartSoundEvent_1 as i32,
                    sys::ISoundOpSystem_slots::StartSoundEvent as i32,
                ],
                "the pinned SNV_SOUNDOPSYSTEM_VT slots, generated indices"
            );
            let pinned = (modules_base(pins::PIN_SOUND) + pins::SNV_SOUNDOPSYSTEM_VT.value) as usize;
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
                assert_eq!(vtable, pinned, "the pinned SNV_SOUNDOPSYSTEM_VT table");
                assert!(!r#async, "SetupMode::Sync on the main thread");
                assert!(stack_size > 0, "the computed stack size");
            }

            let detail = snd_disarm().expect("something was armed");
            assert!(detail.starts_with("removed 3 hooks"), "{detail}");
            assert!(!snd_installed());
            assert_eq!(
                mock.removals_by_index(),
                vec![
                    (sys::ISoundOpSystem_slots::StartSoundEvent as i32, false),
                    (sys::ISoundOpSystem_slots::StartSoundEvent_1 as i32, false),
                    (sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32, false),
                ],
                "the plain start first (13), then 12, then 11 - all synchronous"
            );
        });
    }

    /// The with-params `post`: the guid comes from the original's saved value, the hash /
    /// entity / layer from the params object, and the packed extent is captured as the C did.
    /// A foreign `this` records nothing; no saved original records nothing and does not crash.
    #[test]
    fn params_post_records_the_original_guid_and_ignores_foreign_or_absent_originals() {
        mock_khook::with_mock("sound-params", |mock, _scratch| {
            let obj = fake_object();
            reset_ring();
            snd_arm(obj).expect("arm");
            let mut packed = *b"abcd";
            let p = params(0x1234, 7, 3, Some(&mut packed));

            let value = run_params(mock, obj, &p);
            assert_eq!(value.0.m_Data, GUID, "the call's own return value is untouched");
            {
                let ring = ring_lock();
                assert_eq!(ring.count, 1, "one event");
                let ev = &ring.slots[ring.head as usize];
                assert_eq!(ev.guid, GUID as u32, "the guid from GetOriginalValuePtr");
                assert_eq!(ev.slot, START_WITH_PARAMS_SLOT);
                assert_eq!(ev.hash, 0x1234);
                assert_eq!(ev.entindex, 7);
                assert_eq!(ev.layer, 3);
                assert_eq!(ev.plen, 4, "the packed extent's declared length");
                assert_eq!(ev.pcopied, 4);
                assert_eq!(&ev.praw[..4], b"abcd");
            }

            // A foreign object of the class reached the shared slot: Ignore, nothing recorded.
            let value = run_params(mock, 0x1234 as *mut c_void, &p);
            assert_eq!(value.0.m_Data, GUID);
            assert_eq!(ring_lock().count, 1, "a foreign `this` records nothing");

            // No saved original (the `pre` skipped the call, or the API went away): nothing to
            // record, and the callback answers without touching the null pointer.
            run_params_without_original(mock, obj, &p);
            assert_eq!(ring_lock().count, 1, "a null original value records nothing");

            snd_uninstall();
        });
    }

    /// The by-name `post`: the hash and entity are the ones the original wrote into the params
    /// object, and the name is the argument.
    #[test]
    fn by_name_post_records_the_params_hash_entity_and_name() {
        mock_khook::with_mock("sound-by-name", |mock, _scratch| {
            let obj = fake_object();
            reset_ring();
            snd_arm(obj).expect("arm");
            let mut p = params(0x55, 9, 0, None);
            let name = c"boom".as_ptr();
            let _ = run_by_name(mock, obj, name, &mut p);
            {
                let ring = ring_lock();
                assert_eq!(ring.count, 1, "one event");
                let ev = &ring.slots[ring.head as usize];
                assert_eq!(ev.guid, GUID as u32);
                assert_eq!(ev.slot, START_BY_NAME_SLOT);
                assert_eq!(ev.hash, 0x55);
                assert_eq!(ev.entindex, 9);
                assert_eq!(&ev.name[..4], b"boom", "the caller's name");
                assert_eq!(ev.name[4], 0);
            }
            snd_uninstall();
        });
    }

    /// The plain start's `post`: name, entity and layer go in whole, the hash stays 0.
    #[test]
    fn start_post_records_name_entity_and_layer() {
        mock_khook::with_mock("sound-start", |mock, _scratch| {
            let obj = fake_object();
            reset_ring();
            snd_arm(obj).expect("arm");
            let name = c"explode".as_ptr();
            let _ = run_start(mock, obj, name, 12, 5);
            {
                let ring = ring_lock();
                assert_eq!(ring.count, 1, "one event");
                let ev = &ring.slots[ring.head as usize];
                assert_eq!(ev.guid, GUID as u32);
                assert_eq!(ev.slot, START_SLOT);
                assert_eq!(ev.hash, 0);
                assert_eq!(ev.entindex, 12);
                assert_eq!(ev.layer, 5);
            }
            snd_uninstall();
        });
    }

    /// A refused registration leaves no half-armed wrap: the one before it is removed again.
    #[test]
    fn a_refused_second_slot_leaves_no_half_installed_wrap() {
        mock_khook::with_mock("sound-refused", |mock, _scratch| {
            mock.fail_setup_at(2);
            let error = snd_arm(fake_object()).expect_err("the second slot is refused");
            assert_eq!(error.code, -32017);
            assert_eq!(error.name, "PrerequisiteFailed");
            assert!(
                error.data.contains("StartSoundEvent_1")
                    && error.data.contains("SetupVirtualHook returned INVALID_HOOK"),
                "the reason names the refused slot: {}",
                error.data
            );
            assert!(
                error.data.contains("StartSoundEvent_2 was Removed"),
                "and the cleanup of the first: {}",
                error.data
            );
            assert!(!snd_installed(), "no half-armed wrap");
            assert_eq!(
                mock.removals_by_index(),
                vec![(sys::ISoundOpSystem_slots::StartSoundEvent_2 as i32, false)],
                "the first slot was removed again, synchronously"
            );
        });
    }

    /// No API installed: the registration is refused with `KHookNotInstalled`, which the op
    /// turns into `-32017`; nothing is armed.
    #[test]
    fn arming_without_khook_is_refused() {
        let _guard = crate::hooks::test_guard::lock();
        assert!(s2sdk::hook::api().is_none());
        let error = snd_arm(fake_object()).expect_err("no API, no hook");
        assert_eq!(error.code, -32017);
        assert!(
            error.data.contains("no KHook IKHook API is installed"),
            "{}",
            error.data
        );
        assert!(!snd_installed());
    }

    /// The listener owner reports itself without touching a game table unless it was registered.
    #[test]
    fn listener_teardown_reports_and_never_needs_khook() {
        let _guard = crate::hooks::test_guard::lock();
        assert!(!SND_LISTENER_ON.load(Ordering::Acquire), "the host never registered one");
        let report = listener_teardown();
        assert_eq!(report.owner, "sound listener");
        assert_eq!(report.detail, "not registered (no-op)");
    }

    /// The object handed to `AddSoundEventListener` is a C++ object, not the table: its first
    /// (only) word is the **table's address**, and the slot soundsystem's listener walk reads
    /// through it is the on-start thunk (the live crash of 2026-10-20 was this word holding the
    /// table itself, so the walk read the first entry - a code pointer - as a vptr).
    #[test]
    fn the_listener_object_holds_the_tables_address() {
        let _guard = crate::hooks::test_guard::lock();
        snd_lvt_fill();
        let (object, table) = snd_listener_object_for_test();
        assert!(!object.is_null() && !table.is_null());
        assert_ne!(object, table, "the object is not the table");
        // SAFETY: `object` is the `SndListener` static; its first word is its vptr.
        let vptr = unsafe { object.cast::<*const c_void>().read() };
        assert_eq!(vptr, table, "the object's vptr is the table's address");
        // SAFETY: the table is `SND_LVT_LEN` entries and the slot is inside it (the const
        // assertion above `SND_LVT_LEN`).
        let started = unsafe {
            vptr.cast::<*const c_void>()
                .add(sys::ISoundEventListener_slots::OnSoundEventStarted)
                .read()
        };
        assert_eq!(
            started,
            snd_l_on_start as unsafe extern "C" fn(*mut c_void, u32, u32, u32) as *const c_void,
            "the slot the object's table carries is the on-start thunk"
        );
    }

    /// The registration path end to end, on the host: `snd_listener_install` hands the object
    /// to the pinned slot 81, and the call soundsystem's update makes on a listener - read the
    /// object's vptr, take the table entry at `OnSoundEventStarted`, call it with the object -
    /// reaches [`snd_l_on_start`], which records one slot-81 event with the guid and the aux
    /// words the game passed.
    #[test]
    fn the_soundsystem_listener_walk_reaches_the_on_start_thunk() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("sound-listener-object");
        reset_ring();
        snd_listener_reset_for_test();

        /// The slot's signature (`ISoundEventListener::OnSoundEventStarted`): the three guids
        /// are 4-byte `SoundEventGuid_t`s, which the thunk takes as `u32` words.
        type OnStarted = unsafe extern "C" fn(*mut c_void, u32, u32, u32);

        /// The shape of soundsystem's listener walk (`soundsystem.dll+0x1c8849`):
        /// `mov rcx,[rbx]` (the registered object), `mov rax,[rcx]` (its vptr: the table),
        /// `call [rax + 0x18]` (byte offset 0x18 = slot 3, `OnLibraryStackFieldsMessages`),
        /// with the object as `this`. This walk makes the same two loads and then calls
        /// `OnSoundEventStarted`'s entry, the slot the bridge implements.
        unsafe extern "C" fn walk(listener: *mut sys::ISoundEventListener) {
            if listener.is_null() {
                return;
            }
            // SAFETY: the registered object is [`SndListener`]: one word, a pointer to the
            // bridge-owned table.
            let table = unsafe {
                listener
                    .cast::<*const [*const c_void; SND_LVT_LEN]>()
                    .read()
            };
            if table.is_null() {
                return;
            }
            // SAFETY: the table has the entry (the fill's bound) and it holds the thunk.
            let entry = unsafe { (*table)[sys::ISoundEventListener_slots::OnSoundEventStarted] };
            // SAFETY: the entry is `snd_l_on_start`, whose signature is `OnStarted`.
            let on_started: OnStarted = unsafe { core::mem::transmute(entry) };
            // SAFETY: the slot's own signature; the thunk reads the guid/aux words only.
            unsafe { on_started(listener.cast(), 7, 11, 13) };
        }

        /// Slot 81: keep the pointer (as the game does) and run one listener walk.
        unsafe extern "C" fn add(
            _this: *mut sys::ISoundOpSystem,
            listener: *mut sys::ISoundEventListener,
        ) {
            // SAFETY: the call's contract: a non-null bridge-owned object.
            unsafe { walk(listener) };
        }

        /// Slot 82: nothing to do on the host (the object is a static of ours).
        unsafe extern "C" fn remove(
            _this: *mut sys::ISoundOpSystem,
            _listener: *mut sys::ISoundEventListener,
        ) {
        }

        /// Slot 68: no name for the guid (the unreadable case the C's `mem_ok` arm covers).
        unsafe extern "C" fn no_name(
            _this: *mut sys::ISoundOpSystem,
            _guid: sys::SoundEventGuid_t,
        ) -> *const c_char {
            core::ptr::null()
        }

        /// Slot 86: the candidate source entity the event records as `inst150`.
        #[cfg(not(windows))]
        unsafe extern "C" fn source_entity(
            _this: *mut sys::ISoundOpSystem,
            _guid: sys::SoundEventGuid_t,
        ) -> sys::CEntityIndex {
            sys::CEntityIndex { _index: 77 }
        }

        /// Slot 86, MSVC shape: the class's out-parameter ABI.
        #[cfg(windows)]
        unsafe extern "C" fn source_entity(
            _this: *mut sys::ISoundOpSystem,
            out: *mut sys::CEntityIndex,
            _guid: sys::SoundEventGuid_t,
        ) -> *mut sys::CEntityIndex {
            // SAFETY: `out` is the caller's `CEntityIndex` buffer.
            unsafe { out.write(sys::CEntityIndex { _index: 77 }) };
            out
        }

        let mut vtable: sys::ISoundOpSystem_vtable = unsafe { core::mem::zeroed() };
        vtable.AddSoundEventListener = Some(add);
        vtable.RemoveSoundEventListener = Some(remove);
        vtable.GetActiveSoundEventName = Some(no_name);
        vtable.GetSoundEventSourceEntity = Some(source_entity);

        snd_listener_install_for_test(fake_object(), &vtable)
            .expect("the fabricated object installs");
        assert!(
            SND_LISTENER_ON.load(Ordering::Acquire),
            "the listener was registered"
        );
        assert!(
            scratch
                .text()
                .contains("sound: listener registered (ISoundOpSystem slot 81)"),
            "the C's registration line: {}",
            scratch.text()
        );

        let (count, ev) = {
            let ring = ring_lock();
            let head = ring.head as usize;
            (ring.count, ring.slots[head])
        };
        assert_eq!(count, 1, "the walk recorded exactly one event");
        assert_eq!(
            ev.slot, LISTENER_SLOT,
            "recorded as slot 81 (the listener's via)"
        );
        assert_eq!(ev.guid, 7, "the guid the game passed");
        assert_eq!(ev.aux, [11, 13], "the two aux guids the game passed");
        assert_eq!(ev.inst150, 77, "the candidate source entity from slot 86");

        snd_listener_uninstall();
        assert!(
            !SND_LISTENER_ON.load(Ordering::Acquire),
            "unregistered again"
        );
        snd_listener_reset_for_test();
    }
}
