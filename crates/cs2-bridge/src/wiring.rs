//! The integration: the tools tick drives the listener and the queue, and the listener answers
//! the wire from real state (WP2.D, bridge build m3.0).
//!
//! Before this module nothing called the M2/M3 pieces. It wires the C bridge's three sites
//! together:
//!
//! * the tick - C `m0_Slot13` (`bridge/mcp_bridge.c:6809-6826`): record `g_lastTick`, poll the
//!   modules, sample the Hammer document fingerprint, start the listener once, drain the
//!   main-thread queue;
//! * `exec_request` (`:5650-5700`, the main thread): the method gate over
//!   [`crate::dispatch::decide`] and the C's three refusal objects, then (no op is ported yet)
//!   the C's own tail - `-32601 MethodNotFound` (`:6279`);
//! * the IPC lane's gate (`:6603-6623`): [`MethodRouter`] over the same decision, so an unknown
//!   or blocked method is answered immediately and never takes a queue slot;
//! * the `control.*` handler ([`crate::ipc::ControlHandler`]): capabilities, ping, status,
//!   version, cancel and shutdown from the live queue and module state, through WP2.C's
//!   byte-exact builders in [`crate::control`].
//!
//! # The clock
//!
//! The C reads `GetTickCount64()`: milliseconds since boot, monotonic. There is no portable
//! equivalent, so [`mono_ms`] returns milliseconds since the first call in this process, from a
//! `std::time::Instant` captured in a `LazyLock` ([`EPOCH`]). `Instant` is monotonic and
//! `std::time` documents it as measuring a monotonically increasing clock; the numbers are not
//! compared across processes (the host only records `startedAt`), so the different epoch is
//! unobservable on the wire.
//!
//! # One process, one listener
//!
//! [`tick`] is the only starter. The C's `g_started` (`:6731`) is [`Runtime::started`]: the
//! listener is started once and never replaced (a second start would rewrite `session.json`
//! with a new port and token, leaving a connected host pointed at a dead listener). The
//! Rust-only [`Runtime::initialized`] gate is the WP2.D negative: a tick before
//! `IAppSystem::Init` (or after `Shutdown` cleared it) must not bind a port. The C has no such
//! gate - its slot 13 is only ever called after `Init` - but the Rust plugin object can be
//! ticked by a test or by a framework whose `Init` was refused, and binding a port from an
//! uninitialised object is not something a negative test could distinguish from "started".
//!
//! Teardown is the C's order (`m0_PreShutdown`/`m0_Shutdown`, `:6780-6808`): clear the
//! initialized flag, shut the queue down (the C's `g_shuttingDown = 1` + wake), then stop the
//! listener - which shuts the sockets down, joins the threads and deletes `session.json`.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::control::{self, PingInput, QueueEntry, StatusInput};
use crate::dispatch::{self, Decision};
use crate::ipc::{
    self, ControlCall, ControlHandler, ControlReply, Listener, MethodRouter, Route,
};
use crate::log;
use crate::modules::{self, Live, Modules, ModuleViews, MODULE_COUNT};
use crate::queue::{self, Request};
use crate::registry::{ModState, ModuleView};
/// The monotonic clock's epoch: the first call to [`mono_ms`] in this process.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds from a duration, saturating where the C's `u64` tick count would wrap.
fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The monotonic milliseconds the C's `GetTickCount64()` provides (`mcp_bridge.c:6811` et al.),
/// measured from [`EPOCH`].
#[must_use]
pub fn mono_ms() -> u64 {
    ms(EPOCH.elapsed())
}

/// [`mono_ms`] for an [`Instant`] the queue recorded (`inflight_since`, `snapshot_dated`).
///
/// An instant before [`EPOCH`] (only possible if a caller took one before this module's clock
/// was first read) maps to 0, the C's "no timestamp".
#[must_use]
pub fn mono_ms_at(instant: Instant) -> u64 {
    ms(instant.saturating_duration_since(*EPOCH))
}

/// The production state: the gate, the handler and the IPC router read it, the tick updates the
/// two timestamps.
pub struct BridgeState {
    modules: Arc<dyn ModuleViews>,
    ported: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// C `g_lastTick` (`mcp_bridge.c:1015`), written by the tick.
    last_tick: AtomicU64,
    /// C `g_startTick` (`mcp_bridge.c:1024`), written when the listener starts.
    started_at: AtomicU64,
    /// C `g_listener != INVALID_SOCKET` (`mcp_bridge.c:6543`), for `control.status`.
    listener_up: AtomicBool,
}

impl BridgeState {
    /// A state over `modules`, with `ported` deciding which implemented rows this build serves
    /// ([`crate::dispatch::PORTED`] in the game).
    #[must_use]
    pub fn new(
        modules: Arc<dyn ModuleViews>,
        ported: Box<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            modules,
            ported,
            last_tick: AtomicU64::new(0),
            started_at: AtomicU64::new(0),
            listener_up: AtomicBool::new(false),
        }
    }

    /// The module states in pin order (`control.ping`/`capabilities`/`version` report them).
    #[must_use]
    pub fn views(&self) -> [ModuleView; MODULE_COUNT] {
        modules::views(self.modules.as_ref())
    }

    /// The verified mask every gate in this module reads.
    #[must_use]
    pub fn mask(&self) -> u32 {
        modules::mask(self.modules.as_ref())
    }

    /// The caps revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.modules.revision()
    }

    /// The raw module state the gate error object names for `pin`.
    fn module_state(&self, pin: u8) -> ModState {
        self.modules.state(pin)
    }

    /// `exec_request`'s head, C `:5657-5698`, then the method body from [`crate::ops`] (the C's
    /// dispatch chain, `:5698-6279`); a name with no body falls to the C's tail (`:6279`,
    /// `-32601`). The caller is the tick's drain, on the main thread.
    #[must_use]
    pub fn exec_request(&self, request: &Request) -> String {
        match dispatch::decide(&request.method, self.mask(), &*self.ported) {
            Decision::Unknown => control::not_found(Some(&request.id)),
            Decision::Blocked { blocked_on } => {
                control::not_implemented(Some(&request.id), blocked_on)
            }
            Decision::Prerequisite { module, pin } => {
                control::prerequisite_failed(Some(&request.id), module, self.module_state(pin))
            }
            // The modules are verified; the row's hook needs an `IKHook` and there is none
            // (`NEED_KHOOK`). `-32017` with the blockedOn text - see `control::khook_missing`.
            Decision::KhookMissing => control::khook_missing(Some(&request.id)),
            // The gate passed: the group's body (`crate::ops`), on this (main) thread.
            Decision::Queue => match crate::ops::lookup(&request.method) {
                Some(body) => body(request),
                // Ported by an injected predicate but without a body: the C's dispatch tail.
                None => control::not_found(Some(&request.id)),
            },
            // `FastPath` rows are answered by the IPC lane and never queued; one that arrives
            // here lands on the C's dispatch tail (`mcp_bridge.c:6279`).
            Decision::FastPath => control::not_found(Some(&request.id)),
        }
    }

    /// The IPC lane's gate, C `:6603-6623`: only the three immediate answers are decided here;
    /// everything the main thread must judge (the `-32017` arm needs no queue-slot decision)
    /// is queued.
    ///
    /// One row is answered **before** that gate: this build's own control-plane row
    /// `metamod.plugins` ([`crate::registry::RUST_METHODS`]). It is answered HERE, on the IPC
    /// thread, from [`crate::ops::metamod`]'s snapshot - the C's `find_method` chain has no
    /// arm for it (the frozen table cannot carry the row), and queueing a status row would
    /// put its answer behind the tick's drain, which is exactly what the snapshot exists to
    /// avoid. The two Metamod prerequisites are the row's own `-32017` answers (see
    /// [`crate::ops::metamod::plugins_answer`]).
    #[must_use]
    pub fn route(&self, method: &str) -> Route {
        self.route_with_params(method, None)
    }

    /// [`BridgeState::route`] with the request's raw `params` text: the three `job.*` rows
    /// ([`crate::ops::job`]) are answered here on the lane too, from the job table - spawning
    /// and polling a process needs no tick, so they work while the frame loop is frozen.
    #[must_use]
    pub fn route_with_params(&self, method: &str, params: Option<&str>) -> Route {
        if crate::ops::job::serves(method) {
            return match crate::ops::job::answer(method, params) {
                Ok(result) => Route::Reply(ControlReply::Result(result)),
                Err((code, message, data)) => Route::Reply(ControlReply::Error {
                    code: i32::try_from(code).unwrap_or(-32603),
                    message: message.to_string(),
                    data: Some(data),
                }),
            };
        }
        if crate::ops::metamod::serves(method) {
            return match crate::ops::metamod::plugins_answer() {
                Ok(result) => Route::Reply(ControlReply::Result(result)),
                Err((code, message, data)) => Route::Reply(ControlReply::Error {
                    code: i32::try_from(code).unwrap_or(-32603),
                    message: message.to_string(),
                    data: Some(data),
                }),
            };
        }
        match dispatch::decide(method, self.mask(), &*self.ported) {
            // `:6601-6610`: an unknown name, and `:6618-6621`: a `needs == 0` row that reached
            // this arm (the control fast path answers its own names first).
            Decision::Unknown | Decision::FastPath => Route::Reply(ControlReply::Error {
                code: -32601,
                message: "MethodNotFound".to_string(),
                data: None,
            }),
            // `:6611-6616`: the same `build_error_ext(-32011, ...)` bytes as exec_request.
            Decision::Blocked { blocked_on } => Route::Reply(ControlReply::Error {
                code: -32011,
                message: "NotImplemented".to_string(),
                data: Some(control::blocked_on_data(blocked_on)),
            }),
            // The C's enqueue arm (`:6623`): the main thread decides `-32017` or runs it. Both
            // prerequisite shapes are queued, so the main thread answers them.
            Decision::Prerequisite { .. } | Decision::KhookMissing | Decision::Queue => {
                Route::Queue
            }
        }
    }

    /// The capabilities object (the `control.hello` member and the `control.capabilities`
    /// result), narrowed to the rows this build serves.
    #[must_use]
    pub fn capabilities(&self) -> String {
        let ported = &*self.ported;
        control::capabilities_object_filtered(&self.views(), self.revision(), &|method| {
            ported(method.name)
        })
    }

    /// `control.ping`'s result object, C `:6497-6529`.
    #[must_use]
    pub fn ping_object(&self) -> String {
        let views = self.views();
        let input = PingInput {
            now: mono_ms(),
            start_tick: self.started_at.load(Ordering::Acquire),
            last_tick: self.last_tick.load(Ordering::Acquire),
            inflight_start: inflight_ms(),
            queue_depth: queue::global().depth() as u64,
            modules: &views,
            revision: self.revision(),
            shutting_down: queue::global().is_shutting_down(),
        };
        control::ping_object(&input)
    }

    /// `control.status`'s result object, C `:6530-6553`.
    ///
    /// The ages come from [`queue::Queues::snapshot_dated`], and `now` is that snapshot's own
    /// instant on the [`mono_ms`] clock: the C reads `GetTickCount64()` once inside the queue
    /// lock and subtracts each `receivedAt` from it, so re-deriving `receivedAt = now - age`
    /// makes the builder's own subtraction land on exactly the snapshot's age.
    #[must_use]
    pub fn status_object(&self) -> String {
        let (as_of, queued) = queue::global().snapshot_dated();
        let now = mono_ms_at(as_of);
        let entries: Vec<QueueEntry<'_>> = queued
            .iter()
            .map(|request| QueueEntry {
                id: &request.id,
                method: &request.method,
                received_at: now.saturating_sub(request.age_ms),
            })
            .collect();
        let jobs = crate::ops::job::summaries();
        let jobs: Vec<(&str, &str, &str)> = jobs.iter().map(|(id, kind, state)| (id.as_str(), *kind, *state)).collect();
        let input = StatusInput {
            now,
            queue: &entries,
            listener: self.listener_up.load(Ordering::Acquire),
            inflight_start: inflight_ms(),
            revision: self.revision(),
            shutting_down: queue::global().is_shutting_down(),
            jobs: &jobs,
        };
        control::status_object(&input)
    }

    /// `control.version`'s result object, C `:6560-6577`.
    #[must_use]
    pub fn version_object(&self) -> String {
        control::version_object(&self.views())
    }
}

/// The C's `g_inflightStart` as a [`mono_ms`] value; 0 = nothing in flight.
fn inflight_ms() -> u64 {
    queue::global().inflight_since().map_or(0, mono_ms_at)
}

impl ControlHandler for BridgeState {
    fn capabilities(&self) -> String {
        BridgeState::capabilities(self)
    }

    fn control(&self, call: ControlCall<'_>) -> Option<ControlReply> {
        match call {
            // `:6576-6589`: the lane already ran `q_remove_id`; `removed` is the C's `found`.
            ControlCall::Cancel { removed, .. } => Some(if removed {
                ControlReply::Result(control::CANCELLED.to_string())
            } else {
                ControlReply::Error {
                    code: ipc::listener::NOT_CANCELLABLE,
                    message: "NotCancellable".to_string(),
                    data: Some(control::NOT_CANCELLABLE_DATA.to_string()),
                }
            }),
            // `:6591-6600`: the lane owns the side effects and answers the constant if this
            // declines; the result is the same either way.
            ControlCall::Shutdown => Some(ControlReply::Result(control::CLOSING.to_string())),
            ControlCall::Other { method, .. } => match method {
                "control.ping" => Some(ControlReply::Result(self.ping_object())),
                "control.status" => Some(ControlReply::Result(self.status_object())),
                "control.capabilities" => Some(ControlReply::Result(BridgeState::capabilities(
                    self,
                ))),
                "control.version" => Some(ControlReply::Result(self.version_object())),
                // `:6599-6601`: the lane turns `None` into -32601.
                _ => None,
            },
        }
    }
}

impl MethodRouter for BridgeState {
    fn route(&self, method: &str) -> Route {
        BridgeState::route(self, method)
    }

    fn route_with_params(&self, method: &str, params: Option<&str>) -> Route {
        BridgeState::route_with_params(self, method, params)
    }
}

/// The process-wide runtime: the state the replies read and the one listener.
struct Runtime {
    state: Arc<BridgeState>,
    /// C `g_started` (`mcp_bridge.c:6731`): the one-shot listener start.
    started: AtomicBool,
    /// Rust-only: `IAppSystem::Init` ran and no `Shutdown`/`PreShutdown` has cleared it.
    initialized: AtomicBool,
    /// The running listener, if any. Taken out before it is stopped, so a teardown never holds
    /// the lock while joining a connection thread that may be reading the state.
    listener: Mutex<Option<Listener>>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            state: Arc::new(BridgeState::new(
                Arc::new(modules::live().clone()),
                Box::new(dispatch::is_ported),
            )),
            started: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            listener: Mutex::new(None),
        }
    }

    fn lock_listener(&self) -> std::sync::MutexGuard<'_, Option<Listener>> {
        self.listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

static RUNTIME: LazyLock<Runtime> = LazyLock::new(Runtime::new);

fn runtime() -> &'static Runtime {
    &RUNTIME
}

/// The production state (the same object the tick and the listener use).
#[must_use]
pub fn state() -> Arc<BridgeState> {
    Arc::clone(&runtime().state)
}

/// `IAppSystem::Init` ran: the tick may start the listener.
///
/// The C's `m0_Init` (`mcp_bridge.c:6813`) only logs; the flag is the WP2.D negative's hook.
///
/// Both roles call this at their own lifecycle start (the tools framework's `Init` slot and the
/// Metamod `Load`), so it must also undo the previous lifecycle's stop: a `meta unload` in an
/// mm-only image stops the listener and shuts the queue down (there is no image left to serve
/// from), and a later `meta load` in the same process must serve again. Reopening the queue is
/// idempotent and changes nothing while it is open; clearing `initialized` is the stop's.
pub fn init() {
    let runtime = runtime();
    // The queue's `shutting_down` is the C's permanent `g_shuttingDown`; the C bridge has no
    // unload, this one does (`plugin::unload`), so a re-Load has to lift it again.
    queue::global().reopen();
    crate::ops::job::reopen();
    runtime.initialized.store(true, Ordering::Release);
}

/// One frame - the body of the tick slot (C `m0_Slot13`, `mcp_bridge.c:6809-6826`), in the C's
/// order:
///
/// 1. `g_lastTick = GetTickCount64()` (`:6811`);
/// 2. `modules_poll()` (`:6818`) - the throttled starter of the verification thread;
/// 3. `doc_fingerprint()` (`:6819`) - the Hammer document fingerprint (M4 HAMMER-R);
/// 4. `start_listener_once()` (`:6820`);
/// 5. `drain()` (`:6822`) - up to four queued requests through [`BridgeState::exec_request`].
///
/// The C also pins its own module (`:6813-6816`); that belongs to the Windows loader half.
pub fn tick() {
    let runtime = runtime();
    // A refused unload (`plugin::unload`) leaves the verifier's stopping flag set: an unload
    // cannot tell a forced one (`UnloadAll`, `meta clear`, `meta force_unload`) from one Metamod
    // will honour, and the forced path gives nobody a second chance to stop the verifier. A tick
    // is proof the image is still mapped and still ours, so the verifier may run again.
    crate::modules::live().resume_verify();
    runtime.state.last_tick.store(mono_ms(), Ordering::Release); // :6811
    modules::live().poll(); // :6818
    crate::ops::hammer::doc_fingerprint(); // :6819
    start_listener_once(runtime); // :6820
    let state = Arc::clone(&runtime.state);
    let mut exec = move |request: &Request| state.exec_request(request);
    queue::global().drain(&mut exec); // :6822
}

/// C `start_listener_once` (`mcp_bridge.c:6730-6779`): bind, write `session.json`, start the
/// writer and the accept thread - at most once per process.
fn start_listener_once(runtime: &'static Runtime) {
    // The WP2.D negative gate: no Init, no port.
    if !runtime.initialized.load(Ordering::Acquire) {
        return;
    }
    // C `if (g_started || g_shuttingDown) return; g_started = 1;` (`:6731-6732`). The flag is
    // set before the attempt, so a failed bind or RNG is not retried every frame (the C does
    // the same: `g_started` is already 1 on every failure path below it).
    if runtime.started.swap(true, Ordering::AcqRel) {
        return;
    }
    if queue::global().is_shutting_down() {
        return;
    }
    let started_at = mono_ms(); // C `g_startTick = GetTickCount64()` (`:6734`)
    runtime.state.started_at.store(started_at, Ordering::Release);
    log::line("bridge: starting listener"); // C `:6763`
    let config = ipc::Config {
        handler: Arc::clone(&runtime.state) as Arc<dyn ControlHandler>,
        router: Arc::clone(&runtime.state) as Arc<dyn MethodRouter>,
        discovery_path: ipc::discovery::default_path().ok(),
        hello_deadline: ipc::HELLO_DEADLINE,
        rng: Box::new(ipc::token::random),
        pid: std::process::id(),
        started_at_ms: started_at,
    };
    match ipc::start(config) {
        Ok(Some(listener)) => {
            *runtime.lock_listener() = Some(listener);
            runtime.state.listener_up.store(true, Ordering::Release);
        }
        Ok(None) => log::line("bridge: listener not started (see the line above)"),
        Err(error) => log::line(&format!("bridge: listener start failed: {error}")),
    }
}

/// The C's teardown (`m0_PreShutdown` + `m0_Shutdown`, `mcp_bridge.c:6780-6808`), in its order:
/// clear the initialized flag, shut the queue down and wake its waiters (`:6795-6798`), stop
/// the listener - sockets down (`:6791-6793`), threads joined, `session.json` deleted (`:6799`).
///
/// Also resets the one-shot (`Runtime::started`, the C's `g_started`) so a later [`init`] in the
/// same process can start a listener again: the C bridge never unloads, this one does
/// (`crate::plugin::unload`).
///
/// Not idempotent while a listener thread is stuck: the listener slot is taken out only when its
/// threads are joined, so an ordinary second call is a no-op and a second call after a missed
/// deadline is not - the listener stayed in the slot (with the threads it could not join), and
/// every later call waits on it again, up to one [`ipc::listener`] stop deadline per call
/// (`wiring::pre_shutdown`, then `wiring::shutdown`, may each wait that long). The one-shot
/// (`Runtime::started`) stays set in that case too, so no `init` starts a second listener over
/// the top of the stuck one. The tools host has no refusal path (`m0_Shutdown` returns void), so
/// a thread that outlives the deadline there is logged and left to finish: the process is going
/// away.
pub fn pre_shutdown() {
    // mcp_bridge.c:6783-6786: the hooks' teardown runs first, in the C's order
    // (`in_uninstall; snd_uninstall; snd_listener_uninstall; sr_set(0)`); `pre_shutdown`
    // runs from the tools framework's IAppSystem `PreShutdown`/`Shutdown`
    // (`toolsys.rs:326-334`) on the main thread, and each teardown no-ops when its hook
    // is not installed. Every owner is registered in `crate::hooks` in that order (the
    // GameFrame tick first, which the C has no analogue for), so the whole hook sequence is
    // one call there; the runtime half follows.
    crate::hooks::hooks_teardown();
    stop_runtime();
}

/// The Metamod role's teardown of what its `Load` started: the runtime half of [`pre_shutdown`],
/// without the hook owners - [`crate::plugin::unload`] tears those down through
/// [`crate::hooks::unload_hooks`] right after, in its own order.
///
/// Called only when this image does **not** serve the tool role: with the tools framework
/// present (`cs2.exe -tools`), the tool half owns the listener and the queue, and a Metamod
/// unload must leave them running. Stopping them here is what makes a `meta unload` safe in an
/// mm-only image: Metamod `dlclose`s this image once `Unload` answers true
/// (`core/metamod_plugins.cpp:46-88`), and the accept and writer threads are code in it.
///
/// Answers whether every listener thread was joined. False leaves the listener (and the threads
/// it still owns) in the runtime slot, so a retried `meta unload` can join the rest once they
/// exit; the caller refuses that unload instead of letting Metamod unmap the image under them.
pub fn stop_for_metamod_unload() -> bool {
    stop_runtime()
}

/// The runtime half of the teardown: initialized flag, queue, listener, one-shot.
///
/// Answers whether the listener's threads are all joined. When one is not, the listener stays in
/// the runtime slot (with the handles it could not join) and the one-shot stays set: a retried
/// stop joins it, and no `init` may start a second listener over the top of it. The discovery
/// file is already gone by then (`Listener::stop`), so no host is pointed at the dead port.
fn stop_runtime() -> bool {
    let runtime = runtime();
    runtime.initialized.store(false, Ordering::Release);
    runtime.state.listener_up.store(false, Ordering::Release);
    queue::global().shutdown();
    // Bridge jobs: end the compiler trees and join the watchers (code in this image) before
    // the listener's own deadline - one place both `Shutdown` and a Metamod `Unload` reach.
    let jobs_joined = crate::ops::job::shutdown(JOB_STOP_TIMEOUT);
    if !jobs_joined {
        log::line("stop: a job watcher thread is still running");
    }
    let mut listener = runtime.lock_listener().take();
    let joined = match listener.as_mut() {
        Some(listener) => listener.stop(),
        None => true,
    };
    if !joined {
        *runtime.lock_listener() = listener;
    }
    // Cleared after the stop: the listener really is gone now, so a later `init` may start one.
    // Kept set while a thread outlives the deadline, for the same reason.
    runtime.started.store(!joined, Ordering::Release);
    joined && jobs_joined
}

/// How long the stop path waits for the job watcher threads (they poll every 100 ms, so a
/// terminated job is joined within a poll).
const JOB_STOP_TIMEOUT: Duration = Duration::from_millis(1500);

/// `m0_Shutdown`'s teardown half: the same sequence as [`pre_shutdown`] (the C's `Shutdown`
/// calls `PreShutdown` first, `:6783-6784`).
pub fn shutdown() {
    pre_shutdown();
}

/// The running listener's port, or `None` (the C's `g_port` between `start_listener_once` and
/// `Shutdown`).
#[must_use]
pub fn port() -> Option<u16> {
    runtime().lock_listener().as_ref().map(Listener::port)
}

/// Where the running listener wrote `session.json`, when discovery was enabled.
#[must_use]
pub fn discovery_path() -> Option<PathBuf> {
    runtime()
        .lock_listener()
        .as_ref()
        .and_then(|listener| listener.discovery_path().map(PathBuf::from))
}

/// The last tick's [`mono_ms`] value (the C's `g_lastTick`).
#[must_use]
pub fn last_tick_ms() -> u64 {
    runtime().state.last_tick.load(Ordering::Acquire)
}

/// The [`mono_ms`] value the listener started at (the C's `g_startTick`).
#[must_use]
pub fn started_at_ms() -> u64 {
    runtime().state.started_at.load(Ordering::Acquire)
}

/// The live module state the production runtime reads - for a test that wants the same source
/// the wire uses instead of a fake.
#[must_use]
pub fn live_modules() -> &'static Modules<Live> {
    modules::live()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener thread that misses the stop deadline is not dropped: [`stop_runtime`] puts the
    /// listener back into the runtime slot and keeps the one-shot set, so no later [`init`] can
    /// start a second listener over the top of it and a retried stop can join it. The retry,
    /// after the thread exits, takes the slot and clears the one-shot.
    #[test]
    fn an_unjoined_listener_stays_in_the_slot_with_the_one_shot_set() {
        let _guard = crate::hooks::test_guard::lock();
        let _scratch = crate::hooks::test_guard::scratch_log("wiring-stop-held-listener");

        let runtime = runtime();
        assert!(
            runtime.lock_listener().is_none(),
            "the case starts with no listener"
        );
        let (listener, gate) = crate::ipc::listener::held_writer();
        *runtime.lock_listener() = Some(listener);
        // The one-shot `start_listener_once` would have set for the listener above.
        runtime.started.store(true, Ordering::Release);

        assert!(
            !stop_runtime(),
            "the writer is still running: the stop must say so"
        );
        assert!(
            runtime.lock_listener().is_some(),
            "the listener stays in the slot so the retry can join its thread"
        );
        assert!(
            runtime.started.load(Ordering::Acquire),
            "the one-shot stays set: init must not start a second listener over the stuck one"
        );

        drop(gate);
        assert!(stop_runtime(), "the retry joins the released writer");
        assert!(
            runtime.lock_listener().is_none(),
            "the listener is taken once every thread is joined"
        );
        assert!(
            !runtime.started.load(Ordering::Acquire),
            "a later init may start one again"
        );
        assert!(stop_runtime(), "an ordinary second stop is a no-op");

        // `stop_runtime` shut the process-wide queue down; leave it open for the other cases.
        queue::global().reopen();
    }
}
