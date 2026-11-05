//! The tick for hosts without the tools framework: KHook on `ISource2Server::GameFrame`.
//!
//! The bridge's main-thread work loop is [`crate::wiring::tick`]. Until this module it had one
//! caller, the workshop tools framework's `IToolSystem2::RunFrame` (`toolsys.rs`), so the
//! Linux dedicated server, the Linux client and the Wine client - all Metamod-only images with
//! no tools framework - never ticked the bridge at all: no listener, no queue drain. This module
//! gives those images the same tick, from the one per-frame callback the server DLL itself
//! offers: `ISource2Server::GameFrame` (`engine2/isource2server.h`, the generated
//! [`ISource2Server_slots::GameFrame`]).
//!
//! # How it is hooked
//!
//! * The object is the server's own `ISource2Server` ("Source2Server001",
//!   [`INTERFACEVERSION_SERVERGAMEDLL`]), resolved the way Metamod's own samples do it
//!   (`GET_V_IFACE_ANY`: `VInterfaceMatch(GetServerFactory(), name, 0)`, `core/ISmmPlugin.h:585`)
//!   - through the generated [`ISmmAPI`] slots, never a hand-written index.
//! * The vtable is the object's own vptr (`*(obj as *const *mut *mut c_void)`), and the slot is
//!   the generated [`ISource2Server_slots::GameFrame`], so KHook patches the class's shared table
//!   entry exactly as `game.saveRestore` does for its class (`ops/game/save_restore.rs`). Every
//!   `GameFrame` call of any object of that class reaches the callback; CS2 has one server
//!   object, and a frame of any of them is a server frame, so the callback ticks for all of them.
//! * The callback is **post-only** and answers [`Return::Ignore`]: it observes the frame (the
//!   tick runs after the server's own work) and changes nothing - no override, no skipped
//!   original.
//! * The install is [`SetupMode::Async`]: `Load` runs on the main thread at server-lib init and
//!   `GameFrame` is certainly not on that stack, so [`SetupMode::Sync`] would be legal too; the
//!   async flag is what the C++ templates pass "for safety reasons" (`khook.hpp:685`) and it
//!   never holds the hooked address's capsule mutex inside the setup call
//!   (`detour.cpp:1738-1748`). A frame missed before KHook's insert thread gets to the slot is
//!   harmless; nothing in the tick is order-dependent.
//!
//! # Which tick runs: tools, gameframe, or none
//!
//! The hook is installed on **every** Metamod `Load`, including the Wine `-tools` image where the
//! tools framework also ticks the same mapping. There the tools framework's tick must stay the
//! only one (two ticks per frame would double-drain and double-count), so the callback asks
//! [`crate::log::roles`] **per call** whether the [`crate::log::Role::Tool`] role is present and
//! answers [`Return::Ignore`] without ticking if it is. The role is a per-image set that only
//! grows and the tool object can be handed out *after* Metamod's `Load`, which is why the check
//! cannot be made once at install time.
//!
//! [`tick_source`] is the same decision for the status line:
//! `"tools"` (the tool role drives [`crate::wiring::tick`]), `"gameframe"` (this hook does), or
//! `"none"` (neither: no `Load` completed here, or the hook could not be installed - the
//! [`state`] text says why).
//!
//! # Which thread ticks
//!
//! [`install`] records the thread Metamod's `Load` ran on - the engine's main thread - and the
//! callback ticks **only** on that thread. This is not a formality: with
//! `sv_tick_parallel_with_client` on, libengine2 can queue the server's frame as a
//! `"ServerGameFrame"` job on the thread pool, whose run function calls
//! `ISource2Server::GameFrame` (slot 19) from a worker (measured in IDA on 2000930). The
//! bridge's tick is main-thread work (the listener start, the modules' main-thread state, the
//! queue drain whose ops assume the engine's thread), so a frame on any other thread logs
//! `gameframe: GameFrame off the main thread (sv_tick_parallel_with_client?) - not ticking`
//! **once** and answers `Ignore`. The status line still says `"gameframe"`; the off-thread
//! flag (`"gameframeOffThread":true`) and the `"gameframeTicks"` counter are what show the
//! frames are not being taken.
//!
//! # When there is no tick
//!
//! `GameFrame` only runs while a server runs: on a dedicated server always, on a client only with
//! a map loaded (a listen server). **At a client's main menu there is no tick**, and this module
//! deliberately invents no other source - a menu client answers no RPC until a map is up. The
//! status line reports the source as `"gameframe"` as soon as the hook is installed; the
//! `"gameframeTicks"` counter staying at 0 is how a caller sees that no frame has arrived yet,
//! and `"gameframeOffThread":true` (the off-thread line was logged once) says the frames that do
//! arrive are not being taken.
//!
//! # Teardown: asynchronous only
//!
//! An unload can be issued from inside the frame: another plugin's `GameFrame` hook calling into
//! Metamod is the way that happens. The forced paths `meta force_unload`, `meta clear` and
//! `UnloadAll` (`core/metamod_plugins.cpp:656,759`) are **forced**, not in-frame paths - Metamod
//! ignores our `false` there. (Whether an ordinary `meta unload` typed at the console can also
//! run inside the frame is unverified.)
//! When that happens, the unload's stack is `GameFrame`'s own call stack. A synchronous removal
//! there would spin for the capsule mutex this thread holds shared
//! (`detour.cpp:1503,1581-1598`; `s2sdk::hook`'s "When *not* to call this" on
//! [`s2sdk::hook::remove_all_blocking`]). So the teardown entry in [`crate::hooks`] only
//! **drops** the [`Hook`]: KHook's delete thread performs the removal (`detour.cpp:1852-1867`),
//! and on an ordinary unload the later [`s2sdk::hook::remove_all_blocking`] re-issues it
//! synchronously as lane A's drain does for every pending id. `remove_sync` is never called for
//! this hook, anywhere.
//!
//! ## What happens if the unload itself runs inside the frame
//!
//! The async drop alone is not enough for that case: the unload's own
//! [`s2sdk::hook::remove_all_blocking`] re-issues the pending id **synchronously**, and if it
//! ran on the frame's stack it would wait for the capsule mutex this very stack holds shared -
//! the one wait its deadline cannot bound. So the [`pre`]/[`post`] pair counts the frame
//! ([`in_frame`], process-wide) and [`crate::plugin::unload`] **skips only that drain** while a
//! frame is on any thread: the rest of the teardown runs (the wiring stop, every hook owner,
//! the verifier stop/wait/join) and the unload answers normally. Metamod's `Unloader`
//! (`core/metamod_plugins.cpp:46-88`) already asks KHook to remove every id asynchronously and
//! holds the `dlclose` until each removal reported back, so the pending GameFrame removal is
//! finished before the image goes away.
//!
//! ### The window the skip does not close
//!
//! The skip is decided twice - at the entry (before the teardown) and again immediately before
//! `remove_all_blocking` - but a frame can start on a pool thread in the instructions between
//! that second check and the drain's capsule lock. If it does, `__RemoveHook_Sync` waits on the
//! capsule mutex that frame holds shared (`detour.cpp:1578-1598`) for the length of **one
//! frame**: bounded by the frame, not by [`crate::plugin::unload_deadline`]. Neither the
//! GameFrame module nor `crate::hooks` can wait that window out first: `s2sdk::hook` has no
//! per-id "is this id still live" query (only the process-wide
//! [`s2sdk::hook::live_context_count`], which other owners' hooks keep above zero), so there is
//! nothing to poll until the queued asynchronous removal has retired. Closing it needs that
//! per-id query in `s2sdk::hook` (a "wait until this id's removal function ran" helper); until it
//! exists the exposure is this one-frame wait, and it is the only unbounded-by-deadline wait in
//! the teardown.
//!
//! Why the frame depth, and not `s2sdk::hook::in_thunk()`: KHook calls the original through the
//! **first** callback's `make_call_original` (`khook.hpp:268-271`), so the frame body outside
//! our callbacks is not necessarily inside a [`s2sdk::hook::ThunkGuard`] - the generated thunks
//! only cover the callback bodies. `IN_FRAME` is set in our `pre` and cleared in our `post`, so
//! it spans the engine's whole frame body on every thread that entered it, which is exactly the
//! window the drain must avoid.
//!
//! # What the tick runs in: a KHook thunk
//!
//! The callback is a KHook `post` thunk body, so `s2sdk::hook::in_thunk()` is true for the whole
//! of [`crate::wiring::tick`], the queue drain included. Inside a thunk `s2sdk::hook` takes its
//! hook gate with `try_lock` (a gate holder may be spinning in a synchronous removal on this very
//! call's capsule, so waiting could deadlock), and `Hook::remove_sync` defers. Consequences for
//! the one op in this build that touches a hook, `game.saveRestore`: a registration from the tick
//! succeeds when the gate is free and is answered with `GateBusy` (`-32017` with
//! `{"blockedOn":"khook: retry next tick"}`) when another thread holds it at that moment; a
//! disable is accepted (its removal is deferred to the asynchronous path, exactly as the
//! teardown's own drop). The tools host does not have the problem: there the tick is the
//! framework's `RunFrame`, outside any thunk. See `bridge/README.md` "The GameFrame tick".

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::ffi::CString;
use std::sync::{Arc, Mutex, MutexGuard};

use s2sdk::hook::{Hook, HookCall, Return, SetupMode};
use s2sdk::khook_thunks;
use s2sdk_sys::generated::{ISmmAPI, ISource2Server_slots, INTERFACEVERSION_SERVERGAMEDLL};

use crate::hooks::TeardownReport;
use crate::log;

// `ISource2Server::GameFrame` (`engine2/isource2server.h`): `void GameFrame(bool simulating,
// bool bFirstTick, bool bLastTick)`. The three C++ `bool`s are declared `u8` here, never a Rust
// `bool`: KHook enters the thunk with the caller's registers, and a `bool` read across this ABI
// is a validity assumption this crate does not make in either direction (the same rule
// `save_restore.rs` follows for a `bool` return). They are not read: the tick is the same
// whatever the frame's flags say.
khook_thunks! {
    /// The generated `ISource2Server::GameFrame` signature.
    mod game_frame: fn(
        this: *mut c_void,
        simulating: val u8,
        first_tick: val u8,
        last_tick: val u8,
    ) -> () {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

/// The generated slot is an in-table entry (`khook.hpp`: a virtual hook patches
/// `vtable + index`), so it must be inside the class's vtable.
const _: () = assert!(ISource2Server_slots::GameFrame < ISource2Server_slots::VTLEN);

/// The live hook, while installed. [`Hook`] is `!Send` because it holds a raw context pointer;
/// [`GameFrameHook`] is the assertion that KHook's own API is not thread-affine - the handle is
/// created in [`install`] and consumed in [`teardown`], both called by `Load`/`Unload` on
/// whichever thread Metamod uses, and the callback never touches it. (This is not a claim that
/// callbacks and teardown share a thread: the tick's thread rule is [`TICK_THREAD`] below, and
/// the teardown's safety comes from the removal being asynchronous, not from a thread id.)
static GAMEFRAME: Mutex<Option<GameFrameHook>> = Mutex::new(None);

/// The hook plus the const-assertion-friendly wrapper around it.
struct GameFrameHook(Hook);

// SAFETY: every access is behind [`GAMEFRAME`]'s mutex. A `Hook` is only `!Send` because its
// context pointer is not `Sync`-checked by the compiler; nothing in KHook's API is thread-affine
// (`RemoveHook` may be called from any thread, `detour.cpp:1581-1589`) and the reference counts
// inside the context are atomic. `save_restore.rs` makes the same claim for its own hook, and
// `khook_vtable`'s adapter (`IKHook: Send + Sync`) is the trait-level statement of it.
unsafe impl Send for GameFrameHook {}

/// The `ISource2Server` object the hook was installed for (the callback ticks for any object of
/// the class; this is kept for the status/probe and for a clean teardown).
static SERVER_OBJECT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The thread Metamod's `Load` ran on: [`install`] records it and [`post`] on any other thread
/// does not tick. A global (not a thread-local) so a re-`Load` from another thread replaces the
/// recorded one instead of leaving the previous thread's private copy behind. KHook's frame job
/// can run on a pool thread when `sv_tick_parallel_with_client` is on; the tick must stay on the
/// engine's main thread.
static TICK_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// Set once when a frame arrives on a thread that is not [`TICK_THREAD`], so the log gets the
/// explanation once rather than once per frame.
static OFF_THREAD_REPORTED: AtomicBool = AtomicBool::new(false);

/// Whether the calling thread is the one [`install`] recorded.
fn on_tick_thread() -> bool {
    let current = std::thread::current().id();
    *TICK_THREAD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        == Some(current)
}

/// Whether the callback may tick. Cleared **before** the hook is dropped: a dropped hook's
/// removal is asynchronous, so KHook can still call the thunk for a frame or two after the
/// teardown, and a tick into a bridge whose listener/queue are being shut down is not something
/// this module may do. `installed()` answers the same question for the status; this is the
/// callback's lock-free copy.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Frames the callback actually ticked (`"gameframeTicks"` in the status line).
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Why the hook is not installed, for the status line: `"installed"`, or the reason the last
/// install attempt (or teardown) left behind. The text is static, so the status line never
/// allocates a JSON-escaped copy of a dynamic error.
static STATE: Mutex<&'static str> = Mutex::new(NOT_INSTALLED);

/// The initial state: no Metamod `Load` has run in this image.
const NOT_INSTALLED: &str = "not installed: no Metamod Load in this image";

fn gameframe_lock() -> MutexGuard<'static, Option<GameFrameHook>> {
    GAMEFRAME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn state_lock() -> MutexGuard<'static, &'static str> {
    STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why [`install`] did not install the tick. The variant is the status line's reason class; the
/// detailed error (KHook's own) is in the bridge log line the install writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// The `ISmmAPI` has no `GetServerFactory`/`VInterfaceMatch` slot (a broken or new
    /// Metamod, not a state to guess around).
    SlotsMissing,
    /// `GetServerFactory` answered null.
    NoServerFactory,
    /// `VInterfaceMatch("Source2Server001", any)` answered null: no server object in this
    /// process (a tools-only image, or the server DLL is not initialised yet).
    NoServerObject,
    /// The object's own vptr is null, so there is no table to hook.
    NoVtable,
    /// KHook refused the registration (no API, `INVALID_HOOK`, a stack-size refusal, ...).
    HookRefused,
}

impl InstallError {
    /// The status line's `"gameframe"` text for this failure.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::SlotsMissing => "unavailable: ISmmAPI has no GetServerFactory/VInterfaceMatch",
            Self::NoServerFactory => "unavailable: GetServerFactory answered null",
            Self::NoServerObject => "unavailable: VInterfaceMatch(Source2Server001) answered null",
            Self::NoVtable => "unavailable: the server object has no vtable",
            Self::HookRefused => "unavailable: KHook refused the registration",
        }
    }
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.reason())
    }
}

/// Whether the `GameFrame` hook is installed (regardless of which role ticks the image).
#[must_use]
pub fn installed() -> bool {
    gameframe_lock().is_some()
}

/// How many frames the callback has ticked.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Acquire)
}

/// The `"gameframe"` member of the status line: `"installed"` while the hook is live, otherwise
/// the reason it is not.
#[must_use]
pub fn state() -> &'static str {
    *state_lock()
}

/// Which entry point drives [`crate::wiring::tick`] in this image: `"tools"` (the tool role is
/// present - its tick is the only one), `"gameframe"` (this module's hook), or `"none"`.
#[must_use]
pub fn tick_source() -> &'static str {
    if log::roles().contains(log::Role::Tool) {
        "tools"
    } else if installed() {
        "gameframe"
    } else {
        "none"
    }
}

/// `"gameframeOffThread"`: a frame has arrived on a thread other than the one [`install`]
/// recorded, so the tick is not being taken (the callback logged the reason once). The
/// `"tickSource"` still names the hook - this is what says that source is not producing ticks.
#[must_use]
pub fn off_thread() -> bool {
    OFF_THREAD_REPORTED.load(Ordering::Acquire)
}

/// Install the tick: resolve `ISource2Server` and hook its generated `GameFrame` slot.
///
/// Called from Metamod's `Load` after [`crate::khook_api::install`], on every load and on both
/// targets. It never fails the load: a null server object (the normal state of a tools-only
/// image) or a refused registration is logged, recorded in [`state`] and answered as an
/// `Err` for the caller's log line.
///
/// # Safety
///
/// `ismm` must be the live [`ISmmAPI`] Metamod handed this plugin in `Load`
/// (`core/ISmmPlugin.h:107`), valid for the plugin's lifetime.
pub unsafe fn install(ismm: *mut ISmmAPI) -> Result<(), InstallError> {
    if installed() {
        // A second `Load` is refused before it gets here (`plugin::load`), but this module is
        // also callable directly (tests); keep one registration, never two.
        return Ok(());
    }
    // SAFETY: the caller's contract; the generated accessor reborrows the vtable.
    let api = unsafe { ISmmAPI::vtable(ismm) };
    let (Some(get_server_factory), Some(vinterface_match)) =
        (api.GetServerFactory, api.VInterfaceMatch)
    else {
        return Err(record_failure(InstallError::SlotsMissing, ""));
    };
    // `GetServerFactory(true)` is the synthetic wrapper Metamod's own `GET_V_IFACE_ANY` asks for
    // (`core/ISmmPlugin.h:585`, `core/ISmmAPI.h:127-134`).
    //
    // SAFETY: `ismm` is Metamod's live API object (the caller's contract).
    let factory = unsafe { get_server_factory(ismm, true) };
    if factory.is_null() {
        return Err(record_failure(InstallError::NoServerFactory, ""));
    }
    let Ok(name) = CString::new(INTERFACEVERSION_SERVERGAMEDLL) else {
        // The generated string cannot contain a NUL; treat it as a broken binding, not a state.
        return Err(record_failure(InstallError::SlotsMissing, " (interface version has a NUL)"));
    };
    // `VInterfaceMatch(fn, name, 0)`: min = 0 means "any version starting at 001" - the `ANY`
    // form of the macro, which is the one a host uses for an interface it does not pin.
    //
    // SAFETY: `factory` is the `CreateInterfaceFn` the API just answered and `name` outlives the
    // call; `ismm` is the live API object.
    let object = unsafe { vinterface_match(ismm, factory, name.as_ptr(), 0) };
    if object.is_null() {
        return Err(record_failure(InstallError::NoServerObject, ""));
    }
    // SAFETY: `object` is the live `ISource2Server` the server factory answered with; its first
    // word is its vptr (every polymorphic object's is).
    let vtable = unsafe { (object.cast::<*mut *mut c_void>()).read() };
    if vtable.is_null() {
        return Err(record_failure(InstallError::NoVtable, ""));
    }
    // No image check on `vtable`: `server_image()` could only answer `None` here (no module is
    // verified before the first tick's `modules::poll`, and on Linux no Windows module is ever
    // pinned), and KHook keys a virtual hook on the slot's **address** (`detour.cpp:1706-1712,
    // 1907-1915`), so a per-object vptr copy another plugin installed would be patched too -
    // and would go on working only until that plugin restored the object. That is the same
    // limitation `modules::slot_matches` documents for KHook identity; it is not detected here.
    let index = ISource2Server_slots::GameFrame as i32;
    let pre_cbs: Vec<game_frame::PreCb> = vec![Arc::new(pre)];
    let post_cbs: Vec<game_frame::PostCb> = vec![Arc::new(post)];
    // SAFETY: `vtable` is the live server object's own vptr, `index` the generated
    // `ISource2Server::GameFrame` slot of that class, and the thunks were generated for exactly
    // `void f(this, bool, bool, bool)`; the registration is asynchronous so KHook never takes the
    // slot's capsule mutex inside this call from the main thread.
    let hook = match unsafe {
        game_frame::setup_virtual(vtable, index, pre_cbs, post_cbs, None, SetupMode::Async)
    } {
        Ok(hook) => hook,
        Err(error) => {
            return Err(record_failure(
                InstallError::HookRefused,
                &format!(" ({error})"),
            ));
        }
    };
    SERVER_OBJECT.store(object, Ordering::Release);
    *gameframe_lock() = Some(GameFrameHook(hook));
    *TICK_THREAD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::thread::current().id());
    OFF_THREAD_REPORTED.store(false, Ordering::Release);
    ARMED.store(true, Ordering::Release);
    *state_lock() = "installed";
    Ok(())
}

/// Record a failed install for the status line and write the line that names the reason.
fn record_failure(error: InstallError, detail: &str) -> InstallError {
    *state_lock() = error.reason();
    log::line(&format!("gameframe: tick not installed: {error}{detail}"));
    error
}

/// The `gameframe` entry in [`crate::hooks`]: drop the hook, which asks KHook for an
/// **asynchronous** removal, and report what happened for the one log line the teardown writes.
///
/// Never `remove_sync`: the teardown can run on `GameFrame`'s own stack (the unload issued from
/// another plugin's `GameFrame` hook), where a synchronous removal would spin on the capsule
/// mutex this thread holds shared (`detour.cpp:1503,1581-1598`). KHook's delete thread performs
/// the queued removal (`detour.cpp:1852-1867`); on an ordinary unload the later
/// [`s2sdk::hook::remove_all_blocking`] also re-issues the pending id synchronously once the
/// frame call has returned - see the module docs.
///
/// Idempotent: with nothing installed the answer is `"not armed (no-op)"` and no state changes.
pub fn teardown() -> TeardownReport {
    // Stop ticking first: the removal below is asynchronous, so KHook can still enter the thunk
    // until its delete thread runs.
    ARMED.store(false, Ordering::Release);
    let hook = gameframe_lock().take();
    SERVER_OBJECT.store(ptr::null_mut(), Ordering::Release);
    match hook {
        None => TeardownReport::new("gameframe", "not armed (no-op)"),
        Some(GameFrameHook(hook)) => {
            drop(hook); // KHook's delete thread (`detour.cpp:1852-1867`), never a sync removal.
            *state_lock() = "not installed: torn down (the hook removal is queued)";
            TeardownReport::new(
                "gameframe",
                "removal queued (asynchronous; KHook's delete thread performs it)",
            )
        }
    }
}

thread_local! {
    /// Set while [`crate::wiring::tick`] runs on this thread, so a tick that ends up in
    /// `GameFrame` again does not recurse (a panic guard already prevents the unwind, not the
    /// nesting).
    static IN_TICK: Cell<bool> = const { Cell::new(false) };
}

/// How many `ISource2Server::GameFrame` calls are on **any** thread, counted by [`pre`] and
/// lowered by [`post`]. Process-wide, not a thread-local: an unload must skip its synchronous
/// drain if *any* thread is inside the frame, because that thread is the one holding the hook's
/// KHook capsule shared (`detour.cpp:214,1503,1581-1598`), and the drain waits on every removed
/// id, not only this thread's.
static FRAME_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Whether any thread is inside an `ISource2Server::GameFrame` call.
///
/// True from the moment our `pre` callback runs until our `post` callback returns, for every
/// frame call in this process (the engine's frame body included). The unload skips its
/// synchronous hook drain while it is non-zero: KHook holds the hook's capsule mutex shared on
/// that frame's stack, so the drain would spin on it without a deadline
/// (`detour.cpp:1503,1581-1598`).
#[must_use]
pub fn in_frame() -> bool {
    FRAME_DEPTH.load(Ordering::Acquire) > 0
}

/// Lowers [`FRAME_DEPTH`] when the `post` callback returns, including through a caught panic.
///
/// `pub` (hidden) only because [`enter_frame`] returns it; nothing outside this module can
/// construct one.
#[doc(hidden)]
pub struct FrameExit;

impl Drop for FrameExit {
    fn drop(&mut self) {
        // Saturating: a `post` driven without its `pre` (a test that calls the thunk alone)
        // must not wrap the depth and leave every later unload skipping its drain. A CAS loop
        // rather than `fetch_update`/`try_update`: the former is deprecated on this toolchain and
        // the latter is newer than this crate's MSRV.
        let mut depth = FRAME_DEPTH.load(Ordering::Acquire);
        loop {
            let next = depth.saturating_sub(1);
            match FRAME_DEPTH.compare_exchange_weak(
                depth,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => depth = observed,
            }
        }
    }
}

/// Raises [`FRAME_DEPTH`] for a frame whose `post` lowers it through a [`FrameExit`].
fn frame_enter() {
    FRAME_DEPTH.fetch_add(1, Ordering::AcqRel);
}

/// Enter a frame on this thread, lowered again when the guard drops. [`pre`] raises the depth
/// itself (its `post` owns the paired [`FrameExit`]); this is exposed (hidden) so a host test
/// can hold the state an unload from inside the frame meets, without a hooked call.
#[doc(hidden)]
#[must_use]
pub fn enter_frame() -> FrameExit {
    frame_enter();
    FrameExit
}

/// The claim that this thread is inside [`crate::wiring::tick`], taken by [`enter_tick`].
///
/// `!Send` (a raw-pointer marker) on purpose: the flag is a thread-local of the thread the
/// frame call runs on, and dropping the guard on another thread would clear a tick that thread
/// never started.
#[doc(hidden)]
pub struct TickGuard(core::marker::PhantomData<*const ()>);

impl Drop for TickGuard {
    fn drop(&mut self) {
        IN_TICK.with(|flag| flag.set(false));
    }
}

/// Enter a tick on this thread, or answer `None` when one is already running here.
///
/// This is the guard the `GameFrame` callback holds around [`crate::wiring::tick`]; it is public
/// (hidden) so a host test can hold it and prove the callback does not recurse.
#[doc(hidden)]
#[must_use]
pub fn enter_tick() -> Option<TickGuard> {
    IN_TICK.with(|flag| {
        if flag.replace(true) {
            None
        } else {
            Some(TickGuard(core::marker::PhantomData))
        }
    })
}

/// Our `pre` callback: mark the process as inside the frame and change nothing.
///
/// It exists only for [`in_frame`]: the unload has to know whether any thread has `GameFrame`'s
/// stack before it lets the drain remove this hook synchronously. `Return::Ignore` means the
/// original runs unchanged. The depth it raises is lowered by our `post`, which holds the
/// [`FrameExit`] on every path out of the callback.
fn pre(_call: &mut HookCall, _this: *mut c_void, _simulating: u8, _first_tick: u8, _last_tick: u8) -> Return<()> {
    frame_enter();
    Return::ignore(())
}

/// Our `post` callback: on a host without the tool role, run one [`crate::wiring::tick`].
///
/// Runs inside the KHook thunk: the body is panic-guarded ([`crate::guard`]), the role check is
/// one atomic and the only other state is the frame depth and two counters. Nothing is written
/// to the frame: the answer is always [`Return::Ignore`].
fn post(
    _call: &mut HookCall,
    _this: *mut c_void,
    _simulating: u8,
    _first_tick: u8,
    _last_tick: u8,
) -> Return<()> {
    // Paired with `pre`'s increment, on every path out of this callback.
    let _exit = FrameExit;
    crate::guard((), || {
        // A hook KHook has not removed yet (the teardown's removal is asynchronous) must not tick
        // a bridge that is going down.
        if !ARMED.load(Ordering::Acquire) {
            return;
        }
        // The tick is main-thread work (see the module docs): engine2 can run the server frame as
        // a `"ServerGameFrame"` pool job, and a tick from that worker would race the engine's own
        // thread. Say so once and leave the frame alone.
        if !on_tick_thread() {
            if !OFF_THREAD_REPORTED.swap(true, Ordering::AcqRel) {
                log::line(
                    "gameframe: GameFrame off the main thread (sv_tick_parallel_with_client?) - \
                     not ticking",
                );
            }
            return;
        }
        // The Wine `-tools` image: one mapping serves both roles, and the tools framework's
        // `RunFrame` is the tick there. Checked per call: the tool role can appear after `Load`.
        if log::roles().contains(log::Role::Tool) {
            return;
        }
        // A tick that reached `GameFrame` again must not nest: the inner call has already done
        // the frame's work on this thread.
        let Some(_guard) = enter_tick() else {
            return;
        };
        let n = TICKS.fetch_add(1, Ordering::AcqRel) + 1;
        if n.is_power_of_two() {
            // The tools tick's own rule (`toolsys::run_frame`), so one grep reads both hosts.
            log::line(&format!("gameframe tick {n}"));
        }
        crate::wiring::tick();
    });
    Return::ignore(())
}

#[cfg(test)]
mod tests {
    //! The pieces that need no KHook and no Metamod: the state texts, the source decision and
    //! the re-entrancy guard. The hooked path is `tests/gameframe.rs`'s (a mock `KHookApi` and a
    //! mock `ISmmAPI`, driven through the generated tables).

    use super::*;

    /// A fresh image reports no source and no hook, and the state says why.
    #[test]
    fn a_fresh_image_has_no_tick_source() {
        assert_eq!(state(), NOT_INSTALLED);
        assert!(!installed());
        assert_eq!(tick_source(), "none");
        assert_eq!(ticks(), 0);
    }

    /// Every failure has a status text of its own, and each is a JSON-safe ASCII string.
    #[test]
    fn every_install_error_has_its_own_reason_text() {
        let errors = [
            InstallError::SlotsMissing,
            InstallError::NoServerFactory,
            InstallError::NoServerObject,
            InstallError::NoVtable,
            InstallError::HookRefused,
        ];
        for (index, error) in errors.iter().enumerate() {
            assert!(error.reason().starts_with("unavailable: "), "{error}");
            assert!(error.reason().is_ascii());
            assert!(!error.reason().contains('"'));
            for other in &errors[index + 1..] {
                assert_ne!(error.reason(), other.reason(), "two failures share a text");
            }
        }
    }

    /// The re-entrancy guard: the first entry wins, a nested one is refused, and dropping the
    /// first lets the next one in.
    #[test]
    fn the_tick_guard_refuses_a_second_entry_on_the_same_thread() {
        let first = enter_tick().expect("the thread starts outside a tick");
        assert!(enter_tick().is_none(), "a nested tick is refused");
        drop(first);
        let again = enter_tick().expect("the guard released the thread");
        drop(again);
    }

    /// `in_frame` is process-wide, not a thread-local: an unload on this thread must know that
    /// some *other* thread's `GameFrame` call holds the hook's capsule (round 2, item 2).
    #[test]
    fn in_frame_sees_a_frame_on_another_thread() {
        assert!(!in_frame(), "nothing is running yet");
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _frame = enter_frame();
            entered.send(()).expect("the test is waiting");
            let _ = held.recv_timeout(std::time::Duration::from_secs(10));
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the other thread entered the frame");
        assert!(in_frame(), "the other thread's frame is visible here");
        release.send(()).expect("the other thread is waiting");
        thread.join().expect("the frame thread finished");
        assert!(!in_frame(), "the frame depth came back down");
    }
}
