//! The registry of the hooks this bridge owns, and their teardown order.
//!
//! KHook's registrations are not reference counted across owners: every feature that installs a
//! hook (`ops/input.rs`'s two CCSGOInput slots, `ops/sound.rs`'s three
//! `StartSoundEvent` slots, `ops/game/save_restore.rs`'s `AllowSaveRestore`) keeps its own
//! [`s2sdk::hook::Hook`]s in its own static, and the process-wide
//! [`s2sdk::hook::remove_all_blocking`] drain is a backstop, not the way a feature is turned
//! off. This module therefore owns the *order*:
//!
//! 1. every owner's own uninstall runs first (the op's state is cleared with it - e.g.
//!    [`crate::ops::game::save_restore::sr_enabled`] answers false afterwards), so nothing can
//!    re-arm a hook between the drain and the API going away;
//! 2. the module verifier is waited out ([`crate::modules::Modules::wait_verifier`]) before the
//!    API is touched: the verifier's KHook-aware identity check asks
//!    [`s2sdk::hook::api`] for `&'static dyn KHookApi` (`modules::verify_one`'s
//!    `slot_matches`/`byte_pin_matches`), and Metamod deletes its `KHookImpl` when `Unload`
//!    returns true (`core/metamod_plugins.cpp:59`), so forgetting the API under a running
//!    verifier would leave that reference dangling. The wait is signalled (a condvar), not
//!    polled, and bounded; a verifier that does not stop in time refuses the unload;
//! 3. [`s2sdk::hook::remove_all_blocking`] then runs as the backstop for anything an owner
//!    could not release (a removal KHook lost, an owner that is not listed yet);
//! 4. only after that answered `Ok` is the KHook API forgotten
//!    ([`crate::khook_api::uninstall`]), because a removal function of this module cannot run
//!    once the API is gone (see `s2sdk::hook`'s "What callers must still do").
//!
//! [`hooks_teardown`] is what both lifecycle ends call: [`crate::plugin::unload`] (the Metamod
//! role) and [`crate::wiring::pre_shutdown`] (the tool role). It is safe to call repeatedly:
//! each owner's uninstall is idempotent, so a second call finds nothing armed and says so. It
//! never sets a "done" flag, because a plugin can be unloaded and loaded again in one process
//! and a flag would then skip a later teardown that really has a hook to remove.
//!
//! # Why the verifier cannot be started during an unload
//!
//! A verifier thread is only ever started by [`crate::modules::Modules::poll`], and `poll` has
//! exactly one kind of production caller: [`crate::wiring::tick`] - the tools tick
//! (`toolsys.rs`) or, on a host without the tools framework, the GameFrame tick
//! ([`crate::gameframe`]). Both are engine-thread callbacks: the GameFrame callback ticks only
//! on the thread Metamod's `Load` ran on, and a frame on any other thread (engine2's
//! `"ServerGameFrame"` pool job under `sv_tick_parallel_with_client`) does not tick at all.
//! `Unload` reaches us from Metamod's `meta unload` command handler
//! (`core/metamod_console.cpp:547`) and from `UnloadAll` at shutdown
//! (`core/metamod_plugins.cpp:750-760`), i.e. from engine callbacks on that same thread. Two
//! engine-thread callbacks cannot overlap, so no new verifier starts while `Unload` is inside the
//! wait; the wait exists for a verifier an earlier tick started.
//!
//! With the GameFrame tick, an mm-only image **can** have a verifier here, which the pre-E
//! tool-only images could not. The unload therefore also joins the verifier thread after the
//! wait ([`crate::modules::Modules::join_verifier`]): the wait proves its body finished, the
//! join takes the thread out of this image before Metamod `dlclose`s it. `Unload` also sets the
//! verifier's stopping flag first ([`crate::modules::Modules::begin_stop`]), so a verifier that
//! is mid-walk stops before its next call into KHook instead of racing the API teardown - the
//! forced-unload path (`UnloadAll`, `meta clear`, `meta force_unload`) ignores our `false`, so
//! this is the only protection there. A refused unload leaves the flag set. In an mm+tool image
//! the tools tick clears it (`crate::wiring::tick`); in an mm-only image the refusal has already
//! dropped the GameFrame hook, so no tick follows and the plugin stays inert until `meta unload`
//! is retried successfully or the process restarts.

use core::time::Duration;

use s2sdk::hook::Error;

use crate::log;

/// What one owner's uninstall did, for the one log line [`hooks_teardown`] writes per owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeardownReport {
    /// The feature's name, as the log line spells it (`"save/restore"`).
    pub owner: &'static str,
    /// What happened: `"not armed (no-op)"`, the [`s2sdk::hook::Removal`] outcome, or a failure.
    pub detail: String,
}

impl TeardownReport {
    /// A report from an owner, with `detail`'s text kept verbatim.
    #[must_use]
    pub fn new(owner: &'static str, detail: impl Into<String>) -> Self {
        Self {
            owner,
            detail: detail.into(),
        }
    }
}

/// One owner: its feature name and its own uninstall.
type Owner = fn() -> TeardownReport;

/// The owners, in teardown order: the C's `in_uninstall; snd_uninstall;
/// snd_listener_uninstall; sr_set(0)` (`mcp_bridge.c:6783-6786`), one feature per entry. The
/// listener is not a KHook hook (it is a bridge-owned object) but it is one of those four
/// steps, so it keeps its place in the order.
static OWNERS: &[Owner] = &[
    // The tick source on hosts without the tools framework (`gameframe.rs`). First, so the
    // rest of the teardown runs without a frame callback that could re-enter the bridge; its
    // own removal is asynchronous (never `remove_sync`), because an unload issued from inside
    // the frame runs on `GameFrame`'s own stack. Reaching that stack takes another plugin's
    // `GameFrame` hook; the ordinary forced paths - `meta force_unload`, `meta clear`,
    // `UnloadAll` (`core/metamod_plugins.cpp:759`) - are forced, not in-frame paths.
    game_frame,
    // The input lane's two KHook slots (`ops/input.rs`).
    input,
    // The sound wrap's three `StartSoundEvent` slots (`ops/sound.rs`).
    sound,
    // The bridge-owned sound listener object (`ops/sound.rs`).
    sound_listener,
    // Save/restore's `AllowSaveRestore` virtual hook (`ops/game/save_restore.rs`).
    game_save_restore,
];

/// `gameframe`'s owner entry.
fn game_frame() -> TeardownReport {
    crate::gameframe::teardown()
}

/// `input`'s owner entry.
fn input() -> TeardownReport {
    crate::ops::input::teardown()
}

/// The sound wrap's owner entry.
fn sound() -> TeardownReport {
    crate::ops::sound::teardown()
}

/// `sound`'s listener object's owner entry.
fn sound_listener() -> TeardownReport {
    crate::ops::sound::listener_teardown()
}

/// `save/restore`'s owner entry.
fn game_save_restore() -> TeardownReport {
    crate::ops::game::save_restore::teardown()
}

/// Tear down every hook this bridge owns, in [`OWNERS`] order, and write one log line per owner.
///
/// Idempotent: an owner with nothing armed reports `"not armed (no-op)"` and changes nothing, so
/// calling this twice leaves the same state. It does **not** drain the process-wide hook
/// registry - that is [`unload_hooks`]'s later step, kept separate so a caller that only wants
/// the features off (the tool role's `PreShutdown`) does not touch Metamod's `IKHook`.
pub fn hooks_teardown() {
    for owner in OWNERS {
        let report = owner();
        log::line(&format!(
            "hooks teardown: {}: {}",
            report.owner, report.detail
        ));
    }
}

/// Why [`unload_hooks`] refused to go further.
#[derive(Debug)]
pub enum UnloadError {
    /// The module verifier was still running when the deadline passed. It can hold a
    /// `&'static dyn KHookApi` (`crate::modules::verify_one` asks `s2sdk::hook::api`), so the
    /// API must not be forgotten under it - and the unload can only be safe once KHook holds
    /// nothing of this image.
    VerifierRunning,
    /// The process-wide hook drain.
    Hooks(Error),
}

impl core::fmt::Display for UnloadError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VerifierRunning => formatter.write_str("verifier still running"),
            Self::Hooks(error) => write!(formatter, "{error}"),
        }
    }
}

/// The hook half of [`crate::plugin::unload`], in the order the module docs give: the owners,
/// the verifier wait, the process-wide drain, then the API - and the API only when the drain
/// allows it.
///
/// `quiesce` answers whether the module verifier went idle within the deadline
/// ([`crate::modules::Modules::wait_verifier`]); `false` stops the sequence before the drain,
/// with [`UnloadError::VerifierRunning`]. It is a parameter rather than a direct call so the
/// unload order is testable without a verifier thread (the same reason `drain`/`uninstall` are).
///
/// On a drain error the API is kept, **except** for [`Error::KHookNotInstalled`]: that error
/// means ids are still registered and there is no `IKHook` to remove them with, so nothing of
/// this module is reachable from KHook any more and forgetting the slot is the only thing left
/// to do (the next `Load` installs a fresh object over it). Any other error is the timeout case
/// the unload then refuses, and Metamod keeps the plugin's `KHookImpl` alive while `Unload`
/// returned false (`core/metamod_plugins.cpp:656`: `Unload(...) || force`) - so keeping the API
/// is what makes the refused unload survivable. See [`crate::plugin::unload`] for the whole
/// argument, including why the refusal stands even after the API was forgotten.
///
/// `uninstall` is the caller's to gate as well: [`crate::plugin::unload`] passes one that keeps
/// the API installed when the runtime's listener threads are still running, because that
/// refusal (which follows this sequence) leaves the image mapped and KHook may still run a
/// removal function of this module.
///
/// Returns the drain's result for the caller to report, exactly as it came.
pub fn unload_hooks(
    deadline: Duration,
    quiesce: impl FnMut(Duration) -> bool,
    drain: impl FnMut() -> Result<(), Error>,
    uninstall: impl FnMut(),
) -> Result<(), UnloadError> {
    unload_sequence(&mut hooks_teardown, deadline, quiesce, drain, uninstall)
}

/// [`unload_hooks`] with every step injectable: the order is the contract, so the test drives
/// this one and [`hooks_teardown`] is the real first step.
fn unload_sequence(
    teardown: &mut dyn FnMut(),
    deadline: Duration,
    mut quiesce: impl FnMut(Duration) -> bool,
    mut drain: impl FnMut() -> Result<(), Error>,
    mut uninstall: impl FnMut(),
) -> Result<(), UnloadError> {
    teardown();
    if !quiesce(deadline) {
        // Nothing is forgotten: the verifier still holds the API, and it is a thread that is
        // running *our* code (its `FindOriginal` question is a call into KHook).
        log::line(
            "hooks teardown: the module verifier is still running; the KHook API stays installed",
        );
        return Err(UnloadError::VerifierRunning);
    }
    let drained = drain();
    match &drained {
        Ok(()) => uninstall(),
        Err(Error::KHookNotInstalled) => {
            // Nothing left to drain *with*: the API is already gone, so forgetting the slot is
            // the only thing this step could do, and doing it is what the next `Load` expects.
            log::line("hooks teardown: drain answered KHookNotInstalled; forgetting the API anyway");
            uninstall();
        }
        // The timeout (or a broken registration path): the unload is refused and Metamod may
        // keep this plugin's object alive, so the API stays installed for a later drain.
        Err(error) => log::line(&format!(
            "hooks teardown: drain failed ({error}); the KHook API stays installed"
        )),
    }
    drained.map_err(UnloadError::Hooks)
}

#[cfg(test)]
pub(crate) mod test_guard {
    //! The one lock every unit test that installs a mock `KHookApi` takes, and the scratch log
    //! every unit test that lets the bridge write a line uses.
    //!
    //! `s2sdk::hook`'s API slot is process-wide, so two tests of this crate that each install a
    //! mock would race whichever threads `cargo test` gives them (`--test-threads=4`). The
    //! integration tests are separate binaries and have their own; this one serialises the lib
    //! binary's cases.
    //!
    //! The log environment (`CS2_BRIDGE_LOG`, `XDG_RUNTIME_DIR`) is process-wide too, so the
    //! same lock guards it: [`scratch_log`] points the bridge at `target/scratch/` for the
    //! guard's lifetime and restores both variables, and the tests that only want a line
    //! counted hold it anyway (they install the mock, or call a teardown).

    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static HOOK_API_LOCK: Mutex<()> = Mutex::new(());

    /// Take the lock, recovering from a poisoned lock (a failed assertion elsewhere must not
    /// wedge every later case).
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        HOOK_API_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A scratch bridge log and a scratch `XDG_RUNTIME_DIR`, both under the worktree's
    /// `target/`, for as long as the guard lives.
    ///
    /// `CS2_BRIDGE_LOG` is set to `target/scratch/<name>.log`, so every line the case makes the
    /// bridge write lands there and can be counted; `XDG_RUNTIME_DIR` is set to an empty
    /// `target/scratch/<name>-xdg`, so a line that bypassed `CS2_BRIDGE_LOG` would create
    /// `<...>-xdg/cs2mcpbridge/bridge.log` and the test can see that it did not. The user's real
    /// `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` is therefore never touched.
    pub(crate) struct ScratchLog {
        log: PathBuf,
        xdg: PathBuf,
        previous_log: Option<String>,
        previous_xdg: Option<String>,
    }

    impl ScratchLog {
        /// The scratch `XDG_RUNTIME_DIR`, which must stay empty (the bridge log lives under it
        /// only when `CS2_BRIDGE_LOG` is unset).
        pub(crate) fn xdg(&self) -> &std::path::Path {
            &self.xdg
        }

        /// The lines written since the guard was made (empty when nothing was logged).
        pub(crate) fn text(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }

        /// How many lines carry `needle`.
        pub(crate) fn count(&self, needle: &str) -> usize {
            self.text().lines().filter(|line| line.contains(needle)).count()
        }
    }

    impl Drop for ScratchLog {
        fn drop(&mut self) {
            match &self.previous_log {
                Some(value) => std::env::set_var("CS2_BRIDGE_LOG", value),
                None => std::env::remove_var("CS2_BRIDGE_LOG"),
            }
            match &self.previous_xdg {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            let _ = std::fs::remove_file(&self.log);
        }
    }

    /// Point the bridge's log at `target/scratch/<name>.log` and `XDG_RUNTIME_DIR` at
    /// `target/scratch/<name>-xdg` until the returned guard drops. Takes no lock: the caller
    /// holds [`lock`].
    pub(crate) fn scratch_log(name: &str) -> ScratchLog {
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&scratch).expect("target/scratch exists");
        let log = scratch.join(format!("{name}.log"));
        let xdg = scratch.join(format!("{name}-xdg"));
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_dir_all(&xdg);
        std::fs::create_dir_all(&xdg).expect("the scratch XDG dir exists");
        let previous_log = std::env::var("CS2_BRIDGE_LOG").ok();
        let previous_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("CS2_BRIDGE_LOG", &log);
        std::env::set_var("XDG_RUNTIME_DIR", &xdg);
        ScratchLog {
            log,
            xdg,
            previous_log,
            previous_xdg,
        }
    }
}

/// A test-only `KHookApi` that answers only the two `FindOriginal` queries. Shared by the lib
/// binary's identity tests (`ops::ident`); every other call panics, which is how a test notices
/// that the code under it started using KHook for something else.
#[cfg(test)]
pub(crate) mod find_mock {
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use s2sdk::hook::{Action, HookId, KHookApi};

    /// What [`Mock::find_original`] answers; `0` means "answer the input unchanged", the real
    /// KHook's answer for a target it never hooked (`detour.cpp:1897-1905`).
    pub(crate) static INLINE: AtomicUsize = AtomicUsize::new(0);

    /// The slot whose `FindOriginalVirtual` answer is [`SAVED`], keyed on the slot address the
    /// way KHook keys its map (`detour.cpp:1908-1914`). `0` means "no hooked slot".
    pub(crate) static HOOKED_SLOT: AtomicUsize = AtomicUsize::new(0);

    /// The pre-hook entry KHook saved for [`HOOKED_SLOT`].
    pub(crate) static SAVED: AtomicUsize = AtomicUsize::new(0);

    /// Reset both answer pairs.
    pub(crate) fn reset() {
        INLINE.store(0, Ordering::SeqCst);
        HOOKED_SLOT.store(0, Ordering::SeqCst);
        SAVED.store(0, Ordering::SeqCst);
    }

    pub(crate) struct Mock;

    impl KHookApi for Mock {
        unsafe fn setup_hook(
            &self,
            _function: *mut c_void,
            _context: *mut c_void,
            _removed_function: *mut c_void,
            _pre: *mut c_void,
            _post: *mut c_void,
            _make_return: *mut c_void,
            _make_call_original: *mut c_void,
            _stack_size: u32,
            _async: bool,
        ) -> HookId {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        unsafe fn setup_virtual_hook(
            &self,
            _vtable: *mut *mut c_void,
            _index: i32,
            _context: *mut c_void,
            _removed_function: *mut c_void,
            _pre: *mut c_void,
            _post: *mut c_void,
            _make_return: *mut c_void,
            _make_call_original: *mut c_void,
            _stack_size: u32,
            _async: bool,
        ) -> HookId {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        unsafe fn remove_hook(
            &self,
            _id: HookId,
            _async: bool,
            _hook_removal_fn: *mut c_void,
            _context: *mut c_void,
        ) {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        fn get_context_ptr(&self) -> *mut c_void {
            core::ptr::null_mut()
        }

        fn get_original_function(&self) -> *mut c_void {
            core::ptr::null_mut()
        }

        fn get_original_value_ptr(&self) -> *mut c_void {
            core::ptr::null_mut()
        }

        fn get_override_value_ptr(&self) -> *mut c_void {
            core::ptr::null_mut()
        }

        fn get_current_value_ptr(&self, _pop: bool) -> *mut c_void {
            core::ptr::null_mut()
        }

        fn destroy_return_value(&self) {}

        /// `detour.cpp:1897-1905`: the pre-hook function, or `function` when it is not hooked.
        fn find_original(&self, function: *mut c_void) -> *mut c_void {
            let answer = INLINE.load(Ordering::SeqCst);
            if answer == 0 {
                function
            } else {
                answer as *mut c_void
            }
        }

        /// `detour.cpp:1907-1915`: keyed on the slot ADDRESS (`hooks_detour.find(vtable +
        /// index)`); the saved original is answered whatever the slot holds now.
        fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
            let hooked = HOOKED_SLOT.load(Ordering::SeqCst);
            // SAFETY: `s2sdk::hook::find_original_virtual` requires `vtable.offset(index)` to be
            // readable; these tests pass a live fabricated table.
            let slot = unsafe { vtable.offset(index as isize) } as usize;
            if hooked != 0 && slot == hooked {
                SAVED.load(Ordering::SeqCst) as *mut c_void
            } else {
                // SAFETY: as above - KHook's own answer for an unhooked slot reads it.
                unsafe { *vtable.offset(index as isize) }
            }
        }

        unsafe fn do_recall(
            &self,
            _action: Action,
            _ptr_to_return: *mut c_void,
            _return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
        ) -> *mut c_void {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        unsafe fn save_return_value(
            &self,
            _action: Action,
            _ptr_to_return: *mut c_void,
            _return_size: usize,
            _init_op: *mut c_void,
            _deinit_op: *mut c_void,
            _original: bool,
        ) {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        fn lookup_signature(
            &self,
            _start: *mut c_void,
            _size: usize,
            _signature: *const core::ffi::c_char,
        ) -> *mut c_void {
            unreachable!("the identity check only asks the two FindOriginal questions")
        }

        fn was_original_function_skipped(&self) -> bool {
            false
        }
    }
}


/// A test-only `KHookApi` that simulates KHook's per-call value fold (`detour.cpp:359-472`,
/// `:746-990`) well enough to drive the generated thunks of a hook's own tests: the
/// registration log, the removal log, the per-call override/original values and the
/// `Get*ValuePtr` queries. Shared by the input (`ops/input.rs`) and sound (`ops/sound.rs`)
/// lane tests; the save/restore lane keeps its own narrower copy.
///
/// Every mock answer cites the KHook line it stands in for. The API slot is process-wide, so
/// the cases take [`test_guard`]'s lock and the mock is removed by [`with_mock`]'s teardown.
#[cfg(test)]
pub(crate) mod mock_khook {
    use core::cell::RefCell;
    use core::ffi::c_void;
    use core::ptr;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    use s2sdk::hook::{Action, HookId, KHookApi, INVALID_HOOK};

    /// One hooked call's state, in the shape KHook keeps per call (`detour.cpp:359-472`).
    #[derive(Default)]
    struct Frame {
        /// The highest action saved so far (`detour.cpp:383-405`); `Ignore` is the seed
        /// (`khook.hpp:40`).
        action: ActionChoice,
        /// The override value: the bytes of whatever the signature returns.
        override_value: Option<Vec<u8>>,
        /// The original's value (`make_call_original`'s `original = true` save).
        original_value: Option<Vec<u8>>,
        /// `CompleteSkipped` (`detour.cpp:889`).
        original_skipped: bool,
    }

    /// [`Action`] has no `Default`; `Ignore` is the fold's seed.
    #[derive(Clone, Copy)]
    struct ActionChoice(Action);

    impl Default for ActionChoice {
        fn default() -> Self {
            Self(Action::Ignore)
        }
    }

    thread_local! {
        static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
        /// What `make_return` reads when a call has no saved value at all: a real KHook always
        /// has one by then (`detour.cpp:963-973`; the original's value or an override), but a
        /// test can model a call whose original never ran, and the generated `make_return`
        /// refuses a null (`debug_assert!` in a thunk would abort the process).
        static FALLBACK_VALUE: RefCell<[u8; 16]> = const { RefCell::new([0; 16]) };
        static CONTEXTS: RefCell<Vec<*mut c_void>> = const { RefCell::new(Vec::new()) };
        static ORIGINAL: RefCell<*mut c_void> = const { RefCell::new(ptr::null_mut()) };
    }

    /// One KHook registration, as the mock saw it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        /// `SetupVirtualHook`: the table, the slot index, the id KHook answered, the async
        /// flag and the stack size.
        SetupVirtual {
            vtable: usize,
            index: i32,
            id: HookId,
            r#async: bool,
            stack_size: u32,
        },
        /// `RemoveHook(id, async)`.
        Remove { id: HookId, r#async: bool },
    }

    /// How many removals the mock has run in total (the tests' "was the hook still armed when
    /// the deltas were released" seam).
    static REMOVALS: AtomicUsize = AtomicUsize::new(0);

    /// How many removals the current case has run.
    pub(crate) fn removal_count() -> usize {
        REMOVALS.load(Ordering::SeqCst)
    }

    /// The ids this mock hands out. KHook's ids are process-wide and never reused
    /// (`detour.cpp:1546-1548,1728-1729`), and this crate's registry is process-wide too, so a
    /// second mock in the same test binary (save/restore's) must not hand out the same ones:
    /// this counter starts high and that one counts from 1.
    static NEXT_ID: AtomicU32 = AtomicU32::new(0x4000_0000);

    pub(crate) struct Mock {
        calls: Mutex<Vec<Call>>,
        /// id -> context, as KHook stores it per hook (read back by `GetContextPtr`).
        contexts: Mutex<Vec<(HookId, usize)>>,
        /// id -> the removal function, as `SetupVirtualHook` was given it.
        removed: Mutex<Vec<(HookId, usize)>>,
        /// The 1-based setup index that answers [`INVALID_HOOK`] (`khook.hpp:2229`); 0 never.
        fail_setup_at: AtomicUsize,
        /// How many setups were asked for so far.
        setups: AtomicUsize,
        /// The override value KHook already holds when our `pre` runs: the "second value
        /// changer registered ahead of us" (`detour.cpp:381-383`: first saved wins among
        /// equals).
        seed: Mutex<Option<Vec<u8>>>,
        /// Model a removal KHook defers: `RemoveHook` returns, the removal function (and with
        /// it the release of the context) runs later on KHook's delete thread
        /// (`detour.cpp:1630-1669`). While set, `remove_hook` keeps the entry here instead of
        /// running it, so the context it names stays alive and callable - exactly the window
        /// lane E/A2 puts a disarmed pair in.
        defer: AtomicUsize,
        deferred: Mutex<Vec<(HookId, usize, usize)>>,
    }

    impl Default for Mock {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                contexts: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
                fail_setup_at: AtomicUsize::new(0),
                setups: AtomicUsize::new(0),
                seed: Mutex::new(None),
                defer: AtomicUsize::new(0),
                deferred: Mutex::new(Vec::new()),
            }
        }
    }

    impl Mock {
        pub(crate) fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// Make the `nth` (1-based) setup answer `INVALID_HOOK`; `0` clears the failure.
        pub(crate) fn fail_setup_at(&self, nth: usize) {
            self.fail_setup_at.store(nth, Ordering::SeqCst);
        }

        /// Seed the next call with an override value an earlier callback already saved.
        pub(crate) fn seed(&self, value: Vec<u8>) {
            *self.seed.lock().unwrap() = Some(value);
        }

        /// Model KHook's deferred removal: while `yes`, a `RemoveHook` only records the call
        /// and keeps the entry for its delete thread (`detour.cpp:1630-1669`), so the
        /// registration's context stays alive and callable after the disarm returned.
        pub(crate) fn defer_removals(&self, yes: bool) {
            self.defer.store(usize::from(yes), Ordering::SeqCst);
        }

        /// Whether a removal is still held by the modelled delete thread.
        pub(crate) fn deferred_len(&self) -> usize {
            self.deferred.lock().unwrap().len()
        }

        /// The context KHook gave the registration on `index` (the generated slot), for a
        /// trampoline that is about to call that hook's thunks. The LATEST registration wins: a
        /// re-arm adds a second one on the same slot while a deferred removal may still hold
        /// the first.
        pub(crate) fn context_of(&self, index: i32) -> *mut c_void {
            let calls = self.calls.lock().unwrap();
            let contexts = self.contexts.lock().unwrap();
            for call in calls.iter().rev() {
                if let Call::SetupVirtual {
                    index: got, id, ..
                } = call
                {
                    if *got == index {
                        for (hook_id, context) in contexts.iter() {
                            if hook_id == id {
                                return *context as *mut c_void;
                            }
                        }
                    }
                }
            }
            panic!("no live registration on slot {index}");
        }

        /// The removals this case saw, in order, by generated slot index and async flag.
        pub(crate) fn removals_by_index(&self) -> Vec<(i32, bool)> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter_map(|call| match call {
                    Call::Remove { id, r#async } => {
                        let index = calls.iter().find_map(|setup| match setup {
                            Call::SetupVirtual {
                                index, id: setup_id, ..
                            } if setup_id == id => Some(*index),
                            _ => None,
                        });
                        index.map(|index| (index, *r#async))
                    }
                    Call::SetupVirtual { .. } => None,
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
            unreachable!("these lanes only install virtual hooks")
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
            assert!(!pre.is_null() && !post.is_null());
            assert!(!make_return.is_null() && !make_call_original.is_null());
            assert!(!removed_function.is_null(), "KHook needs the removal function");
            assert!(!context.is_null(), "the wrapper always passes its context");
            let nth = self.setups.fetch_add(1, Ordering::SeqCst) + 1;
            let fail = self.fail_setup_at.load(Ordering::SeqCst) == nth;
            let id = if fail {
                INVALID_HOOK
            } else {
                NEXT_ID.fetch_add(1, Ordering::SeqCst) + 1
            };
            self.calls.lock().unwrap().push(Call::SetupVirtual {
                vtable: vtable as usize,
                index,
                id,
                r#async,
                stack_size,
            });
            if fail {
                // `khook.hpp:2229`: the setup failed, nothing registered. The wrapper releases
                // the context it handed over (the `HookSetupFailed` path).
                return INVALID_HOOK;
            }
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
            REMOVALS.fetch_add(1, Ordering::SeqCst);
            if self.defer.load(Ordering::SeqCst) == 1 {
                // The modelled delete thread holds it: the entry (and its context) stays alive
                // until a later drain, which a case may model by keeping the context around.
                if let Some(entry) = self
                    .contexts
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(hook_id, _)| *hook_id == id)
                    .map(|(_, context)| *context)
                {
                    let removed = self
                        .removed
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(hook_id, _)| *hook_id == id)
                        .map(|(_, removed)| *removed)
                        .unwrap_or(0);
                    self.deferred.lock().unwrap().push((id, removed, entry));
                    return;
                }
            }
            // Both kinds run the removal here: the cases check which one was asked for
            // (`async = false` from `remove_sync`, `khook.hpp:673` for `Drop`), and running it
            // keeps the process-wide context count honest.
            self.run_removal(id);
        }

        fn get_context_ptr(&self) -> *mut c_void {
            CONTEXTS.with(|stack| stack.borrow().last().copied().unwrap_or(ptr::null_mut()))
        }

        fn get_original_function(&self) -> *mut c_void {
            ORIGINAL.with(|slot| *slot.borrow())
        }

        fn get_original_value_ptr(&self) -> *mut c_void {
            FRAMES.with(|frames| {
                frames.borrow().last().and_then(|frame| frame.original_value.as_ref()).map_or(
                    ptr::null_mut(),
                    |value| value.as_ptr().cast_mut().cast(),
                )
            })
        }

        fn get_override_value_ptr(&self) -> *mut c_void {
            FRAMES.with(|frames| {
                frames.borrow().last().and_then(|frame| frame.override_value.as_ref()).map_or(
                    ptr::null_mut(),
                    |value| value.as_ptr().cast_mut().cast(),
                )
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
                let value = if frame.action.0 >= Action::Override {
                    frame.override_value.as_mut()
                } else {
                    frame.original_value.as_mut()
                };
                match value {
                    Some(value) => value.as_mut_ptr().cast(),
                    // KHook's `make_return` invariant: a call always has a value by then.
                    None => FALLBACK_VALUE.with(|value| value.borrow_mut().as_mut_ptr().cast()),
                }
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
            // `detour.cpp:1907-1915`: the saved pre-hook entry, or the slot's current word.
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
            if return_size == 0 {
                // `khook.hpp:335-338`: a void hook has no value to copy.
                assert!(ptr_to_return.is_null(), "a void return carries no value pointer");
                return;
            }
            assert!(!ptr_to_return.is_null());
            // SAFETY: the thunk hands a readable pointer for the size it claims.
            let value = unsafe {
                core::slice::from_raw_parts(ptr_to_return.cast::<u8>(), return_size).to_vec()
            };
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                let frame = frames.last_mut().expect("a call is in progress");
                if original {
                    // `detour.cpp:370-372`: the original's value, saved by `make_call_original`.
                    frame.original_value = Some(value);
                } else if action > frame.action.0 {
                    // `detour.cpp:381-383`: a strictly higher action replaces the value; among
                    // equals the first saved wins.
                    frame.action = ActionChoice(action);
                    frame.override_value = Some(value);
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
    /// and leave the process as it was: the API is only forgotten after the drain - the order
    /// `hooks::unload_hooks` uses.
    pub(crate) fn with_mock<R>(
        name: &str,
        body: impl FnOnce(&'static Mock, &super::test_guard::ScratchLog) -> R,
    ) -> R {
        let _guard: MutexGuard<'static, ()> = super::test_guard::lock();
        let scratch = super::test_guard::scratch_log(name);
        assert_eq!(
            s2sdk::hook::live_context_count(),
            0,
            "a previous case left a hook context KHook can still call into"
        );
        REMOVALS.store(0, Ordering::SeqCst);
        FRAMES.with(|frames| frames.borrow_mut().clear());
        CONTEXTS.with(|stack| stack.borrow_mut().clear());
        let mock: &'static Mock = Box::leak(Box::new(Mock::default()));
        s2sdk::hook::install(mock);
        let out = body(mock, &scratch);
        let _ = s2sdk::hook::remove_all_blocking(std::time::Duration::from_secs(10));
        s2sdk::hook::uninstall();
        assert_eq!(
            s2sdk::hook::live_context_count(),
            0,
            "every removal function ran by the end of the case"
        );
        assert!(
            FRAMES.with(|frames| frames.borrow().is_empty()),
            "every call the case started was finished (`make_return` pops it)"
        );
        out
    }

    /// Plant one call's state the way KHook enters a thunk (`detour.cpp:746-790`): the frame,
    /// the hook's context and the original function, plus a seeded override if the case asked
    /// for one.
    pub(crate) fn enter(mock: &Mock, context: *mut c_void, original: *mut c_void) {
        FRAMES.with(|frames| frames.borrow_mut().push(Frame::default()));
        if let Some(seed) = mock.seed.lock().unwrap().take() {
            // An earlier value changer already saved this call's override
            // (`detour.cpp:381-383`: the first value among equal actions stays).
            FRAMES.with(|frames| {
                let mut frames = frames.borrow_mut();
                let frame = frames.last_mut().unwrap();
                frame.action = ActionChoice(Action::Supersede);
                frame.override_value = Some(seed);
            });
        }
        CONTEXTS.with(|stack| stack.borrow_mut().push(context));
        ORIGINAL.with(|slot| *slot.borrow_mut() = original);
    }

    /// The action this call's callbacks have reached so far (the fold's maximum).
    pub(crate) fn action() -> Action {
        FRAMES.with(|frames| frames.borrow().last().expect("a call is in progress").action.0)
    }

    /// Point the next thunk at another registration's context: KHook pushes the context of the
    /// callback it is about to run (`detour.cpp:359-361`), so a call that reaches two
    /// registrations' callbacks reads a different one per callback.
    pub(crate) fn set_context(context: *mut c_void) {
        CONTEXTS.with(|stack| {
            *stack.borrow_mut().last_mut().expect("a call is in progress") = context;
        });
    }

    /// Write a different value into the value KHook has already saved for the call in progress,
    /// through the pointer `GetOverrideValuePtr` answers (`detour.cpp:441-443`: the address of
    /// KHook's own `override_return_ptr`, which whoever holds it can write).
    ///
    /// This is how a call's final override comes to differ from ours: a callback cannot, because
    /// only a **strictly greater** action replaces the saved value (`detour.cpp:381-383`) and a
    /// `Supersede` - what the value changers here answer - is the highest action there is.
    pub(crate) fn overwrite_saved_override(mock: &Mock, value: &[u8]) {
        let ptr = mock.get_override_value_ptr();
        assert!(!ptr.is_null(), "no value was saved for this call");
        // SAFETY: `GetOverrideValuePtr` answers KHook's own saved return value for the call in
        // progress (`detour.cpp:441-443`), `value.len()` bytes long and writable.
        unsafe {
            core::slice::from_raw_parts_mut(ptr.cast::<u8>(), value.len()).copy_from_slice(value);
        }
    }

    /// `detour.cpp:889`: a `Supersede` skips the original.
    pub(crate) fn mark_original_skipped() {
        FRAMES.with(|frames| {
            frames.borrow_mut().last_mut().expect("a call is in progress").original_skipped = true;
        });
    }

    /// Leave the call: KHook pops a thread-local per thunk (`detour.cpp:432`).
    pub(crate) fn leave() {
        CONTEXTS.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2sdk::hook::Error;

    /// The teardown is safe to run twice: each owner's uninstall is idempotent, so the second
    /// pass finds nothing armed and leaves the same state (the log line says so) - and the lines
    /// go to a scratch log under `target/`, never the user's
    /// `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` (review round 2: the scratch `XDG_RUNTIME_DIR`
    /// is left empty, which proves the default path was not used).
    #[test]
    fn hooks_teardown_is_idempotent() {
        let _guard = super::test_guard::lock();
        let scratch = super::test_guard::scratch_log("hooks-teardown-idempotent");
        hooks_teardown();
        hooks_teardown();
        assert!(
            !crate::ops::game::save_restore::sr_enabled(),
            "no hook stays armed after a teardown"
        );
        // And the reports say the second pass had nothing to do.
        let second = game_save_restore();
        assert_eq!(second.owner, "save/restore");
        assert_eq!(second.detail, "not armed (no-op)");
        // Two passes, two lines, both saying the same thing.
        assert_eq!(
            scratch.count("hooks teardown: save/restore: not armed (no-op)"),
            2,
            "one line per pass: {}",
            scratch.text()
        );
        // The lines landed under target/, so the default path was never resolved.
        assert!(!scratch.xdg().join("cs2mcpbridge").exists());
        assert!(
            std::fs::read_dir(scratch.xdg()).expect("the scratch XDG dir").next().is_none(),
            "the scratch XDG_RUNTIME_DIR stays empty: {}",
            scratch.xdg().display()
        );
    }

    /// The unload order is owners -> verifier -> drain -> API; a verifier that does not stop
    /// refuses the unload before the drain, and the API step is skipped for a drain error other
    /// than `KHookNotInstalled`.
    #[test]
    fn unload_order_is_owners_then_verifier_then_drain_then_api() {
        let steps = std::cell::RefCell::new(Vec::new());
        let mut teardown = || steps.borrow_mut().push("owners");
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            true
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let outcome = unload_sequence(
            &mut teardown,
            Duration::from_secs(1),
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        );
        assert!(outcome.is_ok(), "a clean drain lets the unload finish: {outcome:?}");
        assert_eq!(*steps.borrow(), vec!["owners", "verifier", "drain", "api"]);

        // A verifier still running at the deadline refuses the unload before the drain: the API
        // is what the verifier is still holding, so nothing may be forgotten.
        let steps = std::cell::RefCell::new(Vec::new());
        let mut teardown = || steps.borrow_mut().push("owners");
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            false
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let outcome = unload_sequence(
            &mut teardown,
            Duration::from_secs(1),
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        );
        assert!(
            matches!(outcome, Err(UnloadError::VerifierRunning)),
            "the running verifier is reported: {outcome:?}"
        );
        assert_eq!(*steps.borrow(), vec!["owners", "verifier"]);

        // A timeout keeps the API: the refused unload leaves Metamod's KHookImpl alive, so the
        // pointer the adapter holds stays valid for a later drain.
        let steps = std::cell::RefCell::new(Vec::new());
        let mut teardown = || steps.borrow_mut().push("owners");
        let mut quiesce = |_deadline| true;
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Err(Error::UnloadTimeout { remaining: 1 })
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let outcome = unload_sequence(
            &mut teardown,
            Duration::from_secs(1),
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        );
        assert!(
            matches!(outcome, Err(UnloadError::Hooks(Error::UnloadTimeout { remaining: 1 }))),
            "the drain's timeout is reported as it came: {outcome:?}"
        );
        assert_eq!(
            *steps.borrow(),
            vec!["owners", "drain"],
            "the API stays installed"
        );

        // `KHookNotInstalled` is the one error that still forgets the API: there is nothing left
        // to remove with, and a later `Load` installs a fresh object.
        let steps = std::cell::RefCell::new(Vec::new());
        let mut teardown = || steps.borrow_mut().push("owners");
        let mut quiesce = |_deadline| true;
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Err(Error::KHookNotInstalled)
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let outcome = unload_sequence(
            &mut teardown,
            Duration::from_secs(1),
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        );
        assert!(
            matches!(outcome, Err(UnloadError::Hooks(Error::KHookNotInstalled))),
            "the missing-API error is reported as it came: {outcome:?}"
        );
        assert_eq!(*steps.borrow(), vec!["owners", "drain", "api"]);
    }
}
