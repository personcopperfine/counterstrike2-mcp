//! The main-thread request queue, the outbound queue and the per-frame `drain`.
//!
//! Ported from the C bridge's `Req`/`Out` records and their machinery:
//!
//! * the request queue - `q_init`/`q_push`/`q_pop`/`q_purge_gen`/`q_remove_id`/`q_depth`
//!   (`bridge/mcp_bridge.c:1428-1477`), filled by the IPC thread and emptied by the main
//!   thread;
//! * the outbound queue - `out_push`/`out_space`/`out_pop_blocking` (`:1499-1528`), read by
//!   the writer thread;
//! * the expired-deadline sweep - `q_sweep_expired` (`:1587-1608`);
//! * `drain` (`:6289-6317`), the main thread's per-frame batch: a re-entrancy guard, at most
//!   four requests, "never execute without room to answer" (`out_space`), the request
//!   deadline check, the inflight timestamp and the 8 ms budget;
//! * `flush_outbound` (`:6323-6334`).
//!
//! The executor is **not** ported: [`Queues::drain`] takes it as `&mut dyn FnMut`, which is
//! what the tick lane will hand it (`exec_request` in the C, `:6289-6317`). Nothing calls the
//! queue yet either - the IPC lane (WP2.A) pushes and the tick wiring pops.
//!
//! # Capacities, codes and timing
//!
//! [`QCAP`] and [`OUT_CAP`] are the C's `#define`s (`:125-126`). [`QUEUE_TIMEOUT_CODE`] is the
//! `-32008` of both deadline paths (`:1601`, `:6308`) and [`INTERNAL_ERROR_CODE`] the
//! `-32603`/"InternalError" the protocol already uses for its own refusals (`:2103`,
//! `:2422`, ...). Every time value is an [`Instant`]/[`Duration`]: the C counts milliseconds
//! of `GetTickCount64`, which this module never mirrors as a tick count.
//!
//! # Differences from the C, and why
//!
//! * **One mutex, one condvar.** The C has two critical sections (`g_qcs`/`g_ocs`) and two
//!   condvars, but nothing ever waits on the queue condvar - the drain is called by the tick
//!   and `q_pop` never blocks - so the port keeps a single mutex and a single outbound
//!   condvar. `q_push`'s `WakeConditionVariable(g_qcv)` (`:1442`) therefore has no Rust
//!   counterpart; `shutdown` wakes everything on the one condvar (`:6597-6598` does the same
//!   on both).
//! * **`flush_outbound` waits on the condvar instead of polling with `Sleep(10)`**
//!   (`:6332`). Every push notifies, every pop notifies and `shutdown` notifies everything;
//!   all of it `notify_all`, because poppers and flush waiters share the one condvar and a
//!   `notify_one` can land on the wrong waiter class (see [`Queues::out_push`]).
//! * **`flush_outbound` returns early when shutting down.** The C keeps polling until its
//!   timeout because shutdown is the last thing it does; a Rust shutdown must not leave a
//!   waiter behind (the WP2.B gate), so the flag is checked in the loop.
//! * **`out_pop_blocking`'s timeout is a deadline, not a re-armed poll interval.** The C
//!   loops `SleepConditionVariableCS(..., timeoutMs)` until an item or shutdown, i.e. it can
//!   block for arbitrarily longer than `timeoutMs`; its only caller passes 100 ms as a wake
//!   interval (`writer_thread`, `:1545`). Returning `None` after one timeout window keeps
//!   that caller's loop (`if (!pop) { if (shuttingDown) break; continue; }`) correct and is
//!   the contract the gate asks for. Pending records are still delivered after `shutdown`
//!   (the C's `while (g_oCount == 0 && !g_shuttingDown)` returns them too).
//! * **A panicking executor** has no C counterpart. It is caught and answered with
//!   `-32603 InternalError`, `data.reason = "exec panicked"` - the code and the `data.reason`
//!   shape the C's own internal refusals use (`op_err`, `:1990-1995`).
//!
//! A reply that cannot be queued after an execution is dropped, exactly as in the C: every
//! `out_push` in `drain` and `q_sweep_expired` ignores its result (`:1605`, `:6312`), and the
//! `out_space` gate makes that unreachable for the normal path barring a concurrent producer.
//!
//! `MAX_LINE` (`:124`) and `DEFAULT_DEADLINE_MS` (`:129`) are not here: the line limit belongs
//! to the IPC reader and the default deadline to the request builder, both WP2.A.

use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Capacity of the main-thread request queue (`mcp_bridge.c:125`, `#define QCAP 64`).
pub const QCAP: usize = 64;

/// Capacity of the outbound queue (`mcp_bridge.c:126`, `#define OUT_CAP 64`).
pub const OUT_CAP: usize = 64;

/// The build string every reply envelope carries (`mcp_bridge.c:127`, `#define BRIDGE_BUILD`),
/// re-exported from [`crate::BRIDGE_BUILD`] - the crate has exactly one copy of the value.
pub use crate::BRIDGE_BUILD;

/// The JSON-RPC code of a request whose queue deadline passed (`mcp_bridge.c:1601`, `:6308`).
pub const QUEUE_TIMEOUT_CODE: i32 = -32008;

/// The JSON-RPC code the bridge uses for its own internal refusals (`mcp_bridge.c:2103`,
/// `:2422`, `op_err`); the panic answer above uses it too.
pub const INTERNAL_ERROR_CODE: i32 = -32603;

/// Requests executed by one `drain` call (`mcp_bridge.c:6294`, `for (int n = 0; n < 4; n++)`).
const DRAIN_BATCH: usize = 4;

/// Wall-clock budget of one `drain` call (`mcp_bridge.c:6314`, `... - t0 > 8`).
const DRAIN_BUDGET: Duration = Duration::from_millis(8);

/// `data` of the panic answer: the C's internal refusals carry `{"reason":...}`.
const PANIC_DATA: &str = "{\"reason\":\"exec panicked\"}";

/// One request waiting for the main thread - the C's `Req` (`mcp_bridge.c:1407-1414`) and
/// [`Queues::push`]'s argument.
///
/// The IPC lane builds it: `params` is the raw JSON object text the request carried (the C
/// copies just the balanced `{...}` span, `:6627-6646`), `received` is the moment the IPC
/// thread accepted it (`receivedAt`), `deadline` is `deadlineMs` mapped as `None` for the C's
/// "0 = no deadline" (`:1615`) and `Some` otherwise, and `gen` is the connection generation.
#[derive(Debug, Clone)]
pub struct Request {
    /// The JSON-RPC request id (`Req.id`, a bounded copy of the frame's `id` string).
    pub id: String,
    /// The method name (`Req.method`).
    pub method: String,
    /// The raw JSON object text of `params`, `None` when the request had none.
    pub params: Option<String>,
    /// When the IPC thread accepted the request (`Req.receivedAt`).
    pub received: Instant,
    /// The queue deadline (`Req.deadlineMs`): `None` and `Some(Duration::ZERO)` are both the
    /// C's `deadlineMs == 0`, "no deadline" (`:1593`, `:6299`); any other value expires the
    /// request when `now > received + deadline` (`:6301`).
    pub deadline: Option<Duration>,
    /// The connection generation the reply belongs to (`Req.gen`).
    pub gen: u32,
}

/// One queued request as `control.status` reports it - the fields of the C's queue loop
/// (`mcp_bridge.c:6534-6539`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedView {
    /// `rq->id` (`:6536`).
    pub id: String,
    /// `rq->method` (`:6537`).
    pub method: String,
    /// `GetTickCount64() - rq->receivedAt` (`:6538-6539`), measured from a single `now`
    /// taken under the queue lock.
    pub age_ms: u64,
}

/// A bounded queue was full - the `0` of `q_push` (`mcp_bridge.c:1438`) and `out_push`
/// (`:1503`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("queue is full")
    }
}

impl std::error::Error for Full {}

/// One outbound record: a complete reply line and the generation it was produced for (the C's
/// `Out`, `mcp_bridge.c:1422`).
#[derive(Debug)]
struct OutLine {
    line: String,
    gen: u32,
}

/// The state both queues share under [`Queues`]' mutex.
struct Inner {
    /// The request queue, head first (`g_q`; `q_push` appends, `q_pop` takes the head).
    requests: VecDeque<Request>,
    /// The outbound queue, head first (`g_out`).
    outbound: VecDeque<OutLine>,
    /// Set while a `drain` is executing (`mcp_bridge.c:6291`, `static volatile int draining`).
    draining: bool,
    /// The start of the currently executing request (`g_inflightStart`, `:6309-6311`); read by
    /// `control.status` (`:6509`, `:6546`).
    inflight: Option<Instant>,
    /// The C's `g_shuttingDown` (`:1018`).
    shutting_down: bool,
}

/// The process-wide queue pair: the C's `g_q`/`g_out` and their two critical sections
/// (`mcp_bridge.c:1415-1425`).
///
/// [`global()`] is the instance the IPC lane, the writer and the tick share - the C keeps these
/// as file statics, and so does the port. [`Queues::new`] exists because the C's statics cannot
/// be instantiated per test, and because a host test that filled the real queue would make
/// every other test in the same process depend on its order.
pub struct Queues {
    inner: Mutex<Inner>,
    /// Signalled when an outbound record is pushed or popped, or on shutdown.
    out_changed: Condvar,
}

/// The one [`Queues`] the bridge uses (`mcp_bridge.c:1415-1425`).
static GLOBAL: LazyLock<Queues> = LazyLock::new(Queues::new);

/// The process-wide queue pair ([`Queues`]); the C's `g_q`/`g_out` statics.
pub fn global() -> &'static Queues {
    &GLOBAL
}

impl Queues {
    /// An empty queue pair - the C's file-scope zeroed `g_q`/`g_out` plus the
    /// `InitializeCriticalSection`s of `q_init` (`mcp_bridge.c:1428-1433`); the Rust
    /// primitives need no initialisation call, so there is no `q_init` counterpart.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                requests: VecDeque::with_capacity(QCAP),
                outbound: VecDeque::with_capacity(OUT_CAP),
                draining: false,
                inflight: None,
                shutting_down: false,
            }),
            out_changed: Condvar::new(),
        }
    }

    /// Locks the shared state, ignoring poisoning.
    ///
    /// Every field update below is a single plain operation under this lock and no invariant
    /// spans two of them, so a thread that panicked while holding it cannot leave the queue
    /// inconsistent; treating the lock as poisoned would only turn a later `drain` into a
    /// panic in the engine's frame.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues a request for the main thread - `q_push` (`mcp_bridge.c:1436-1445`). `Err(Full)`
    /// is the C's `0`, which the IPC thread answers with `-32007 Busy` (`:6662-6665`).
    pub fn push(&self, request: Request) -> Result<(), Full> {
        let mut inner = self.lock();
        if inner.requests.len() >= QCAP {
            return Err(Full);
        }
        inner.requests.push_back(request);
        Ok(())
    }

    /// The queued request count - `q_depth` (`mcp_bridge.c:1477`), reported by `control.status`
    /// as `queueDepth` (`:6513-6514`).
    pub fn depth(&self) -> usize {
        self.lock().requests.len()
    }

    /// Every queued request in head-first order with the fields `control.status` reports
    /// (`mcp_bridge.c:6533-6540`: `id`, `method`, `ageMs`): taken under the queue lock, with
    /// one `now` for all ages, exactly as the C's loop runs inside `g_qcs`.
    pub fn snapshot(&self) -> Vec<QueuedView> {
        self.snapshot_dated().1
    }

    /// [`Queues::snapshot`] plus the instant its ages were measured at.
    ///
    /// `control.status` needs the C's `GetTickCount64()` basis to rebuild each entry's
    /// `receivedAt` (`now - ageMs`, exactly `:6538`): the C takes one clock reading inside the
    /// queue lock and computes every age against it, so a caller that wants the same ages must
    /// use the same reading. Returning it is that reading, not a second one.
    pub fn snapshot_dated(&self) -> (Instant, Vec<QueuedView>) {
        let inner = self.lock();
        let now = Instant::now();
        let views = inner
            .requests
            .iter()
            .map(|request| QueuedView {
                id: request.id.clone(),
                method: request.method.clone(),
                age_ms: u64::try_from(
                    now.saturating_duration_since(request.received).as_millis(),
                )
                .unwrap_or(u64::MAX),
            })
            .collect();
        (now, views)
    }

    /// Drops every queued request of a dead connection - `q_purge_gen`
    /// (`mcp_bridge.c:1458-1475`), called when the connection is closed (`:6406`, `:6686`).
    /// Only the matching generation is dropped; the rest keep their order.
    pub fn purge_gen(&self, gen: u32) {
        self.lock().requests.retain(|request| request.gen != gen);
    }

    /// Removes the first queued request with this id - `q_remove_id` (`mcp_bridge.c:1478-1497`),
    /// the `control.cancel` path (`:6576-6589`). The C removes at most one (`!found &&`), so a
    /// duplicate id later in the queue stays; `bool` is the C's `found`.
    pub fn remove_id(&self, id: &str) -> bool {
        let mut inner = self.lock();
        match inner.requests.iter().position(|request| request.id == id) {
            Some(index) => {
                inner.requests.remove(index);
                true
            }
            None => false,
        }
    }

    /// Answers every *head* request whose deadline has passed with `-32008 QueueTimeout`,
    /// stopping at the first request that is still live - `q_sweep_expired`
    /// (`mcp_bridge.c:1587-1608`), called by the IPC thread so a client gets a definite answer
    /// while the main thread is stuck in a modal (`:6675`).
    ///
    /// The sweep walks from the head only, exactly as the C does: a late request behind a live
    /// one waits for the drain. A request that already started executing is not queued any
    /// more, so it is never swept; its real answer is always sent.
    pub fn sweep_expired(&self) {
        loop {
            let expired = {
                let mut inner = self.lock();
                let now = Instant::now();
                match inner.requests.front() {
                    Some(request) if is_expired(request, now) => inner.requests.pop_front(),
                    _ => None,
                }
            };
            let Some(request) = expired else { return };
            let line = error_reply(Some(&request.id), QUEUE_TIMEOUT_CODE, "QueueTimeout", None);
            // The C ignores out_push's result here too (`:1605`); the request is already gone.
            let _ = self.out_push(line, request.gen);
        }
    }

    /// Whether the outbound queue has room for one more record - `out_space`
    /// (`mcp_bridge.c:1499`). `drain` checks it before every execution (`:6295`).
    pub fn out_space(&self) -> bool {
        self.lock().outbound.len() < OUT_CAP
    }

    /// Appends one reply line for a generation - `out_push` (`mcp_bridge.c:1500-1513`).
    /// `Err(Full)` is the C's `0`; the caller drops the line then (the C frees it, `:1506`).
    pub fn out_push(&self, line: String, gen: u32) -> Result<(), Full> {
        {
            let mut inner = self.lock();
            if inner.outbound.len() >= OUT_CAP {
                return Err(Full);
            }
            inner.outbound.push_back(OutLine { line, gen });
        }
        // Two waiter classes share this condvar: `out_pop_blocking` (the writer) and
        // `flush_outbound` (the C's flush polls with Sleep(10), `:6332`, so it had one). A
        // `notify_one` can wake the flush waiter instead of the writer and leave the reply
        // blocked until the writer's whole timeout; both re-check their own predicate, so
        // waking both is the correct signal.
        self.out_changed.notify_all();
        Ok(())
    }

    /// Pops one record, waiting up to `timeout` for one - `out_pop_blocking`
    /// (`mcp_bridge.c:1515-1528`), the writer thread's call (`:1545`, with 100 ms).
    ///
    /// Returns a pending record even after [`Queues::shutdown`], like the C's
    /// `while (g_oCount == 0 && !g_shuttingDown)`; returns `None` on the deadline or when
    /// shutting down with nothing left.
    pub fn out_pop_blocking(&self, timeout: Duration) -> Option<(String, u32)> {
        let deadline = Instant::now().checked_add(timeout);
        let mut inner = self.lock();
        loop {
            if let Some(out) = inner.outbound.pop_front() {
                drop(inner);
                // A popper is the only event that can retire a generation `flush_outbound`
                // waits on (see its docs); wake every such waiter.
                self.out_changed.notify_all();
                return Some((out.line, out.gen));
            }
            if inner.shutting_down {
                return None;
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    inner = wait_timeout(&self.out_changed, inner, deadline - now);
                }
                // `Instant::checked_add` only fails for absurd durations; treat that as "no
                // deadline" rather than inventing one.
                None => inner = wait(&self.out_changed, inner),
            }
        }
    }

    /// Waits until no outbound record of `gen` is pending, or the timeout expires -
    /// `flush_outbound` (`mcp_bridge.c:6323-6334`), called by `control.shutdown`
    /// (`:6593`, 1000 ms) so the queued drain replies go out before the socket closes.
    ///
    /// Unlike the C, this waits on the outbound condvar instead of polling every 10 ms, and
    /// returns as soon as [`Queues::shutdown`] is called (the C would keep polling to its
    /// timeout).
    pub fn flush_outbound(&self, gen: u32, timeout: Duration) {
        let start = Instant::now();
        let mut inner = self.lock();
        loop {
            // The C's `while (GetTickCount64() - t0 < timeoutMs)` - a zero timeout checks
            // nothing and returns at once (`:6325-6326`).
            if start.elapsed() >= timeout {
                return;
            }
            if !inner.outbound.iter().any(|out| out.gen == gen) {
                return;
            }
            if inner.shutting_down {
                return;
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            inner = wait_timeout(&self.out_changed, inner, remaining);
        }
    }

    /// Executes at most [`DRAIN_BATCH`] queued requests on the calling thread - `drain`
    /// (`mcp_bridge.c:6289-6317`), the tick's slot 13.
    ///
    /// For each request: skip the batch when the outbound queue is full (the error reply could
    /// not be queued, `:6295`), pop the head, answer `-32008 QueueTimeout` without executing
    /// when its deadline passed (`:6299-6307`), otherwise record the inflight start, run
    /// `exec` and queue its line for the request's generation (`:6309-6312`). The call stops
    /// after 8 ms even when the batch has room left (`:6314`).
    ///
    /// A nested call (a modal loop inside `exec` calling the tick again) returns immediately
    /// without executing anything - the C's `static volatile int draining` guard (`:6290-6292`).
    /// `exec` runs with no queue lock held, as in the C (`q_pop` releases `g_qcs` first).
    ///
    /// If `exec` panics, the panic is caught here (it must not unwind into the engine), the
    /// request is answered [`INTERNAL_ERROR_CODE`] `InternalError` with
    /// `data.reason = "exec panicked"`, and the drain continues with the next request.
    pub fn drain(&self, exec: &mut dyn FnMut(&Request) -> String) {
        {
            let mut inner = self.lock();
            if inner.draining {
                return;
            }
            inner.draining = true;
        }
        // Clears the guard on every exit, including a panic from anything that is not `exec`.
        let _draining = DrainingGuard(self);
        let started = Instant::now();
        for _ in 0..DRAIN_BATCH {
            let request = {
                let mut inner = self.lock();
                if inner.outbound.len() >= OUT_CAP {
                    break;
                }
                match inner.requests.pop_front() {
                    Some(request) => request,
                    None => break,
                }
            };
            if is_expired(&request, Instant::now()) {
                // The C's deadline arm answers and `continue`s: an expired request neither
                // sets the inflight timestamp nor reaches the budget check (`:6299-6307`).
                let line =
                    error_reply(Some(&request.id), QUEUE_TIMEOUT_CODE, "QueueTimeout", None);
                let _ = self.out_push(line, request.gen);
                continue;
            }
            self.lock().inflight = Some(Instant::now());
            let line = match catch_unwind(AssertUnwindSafe(|| exec(&request))) {
                Ok(line) => line,
                Err(_) => error_reply(
                    Some(&request.id),
                    INTERNAL_ERROR_CODE,
                    "InternalError",
                    Some(PANIC_DATA),
                ),
            };
            self.lock().inflight = None;
            // The C ignores this result (`:6312`): the request has already run and its reply
            // is the only copy. `out_space` above makes it unreachable unless another producer
            // filled the queue meanwhile. The C frees the reply silently; one log line keeps
            // the loss diagnosable (`log::line` cannot panic, so this stays quiet inside the
            // catch the engine frame needs).
            if self.out_push(line, request.gen).is_err() {
                crate::log::line("drain: reply dropped, outbound queue full");
            }
            if started.elapsed() > DRAIN_BUDGET {
                break;
            }
        }
    }

    /// When the currently executing request started, `None` between requests - the C's
    /// `g_inflightStart` (`mcp_bridge.c:6309-6311`), read by `control.status` (`:6509`,
    /// `:6546`).
    pub fn inflight_since(&self) -> Option<Instant> {
        self.lock().inflight
    }

    /// Whether the bridge is shutting down - the C's `g_shuttingDown` (`mcp_bridge.c:1018`,
    /// reported at `:6526`, `:6550`).
    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
    }

    /// Shuts the queue pair down: sets the flag and wakes every waiter, like the C's
    /// `g_shuttingDown = 1; WakeAllConditionVariable(g_qcv); WakeAllConditionVariable(g_ocv)`
    /// (`mcp_bridge.c:6596-6598`).
    ///
    /// Pending outbound records are kept for the writer to drain (the C's writer exits only
    /// when a pop finds nothing and the flag is set, `:1545-1547`); queued requests are not
    /// purged (neither does the C).
    pub fn shutdown(&self) {
        self.lock().shutting_down = true;
        self.out_changed.notify_all();
    }

    /// Lift [`Queues::shutdown`] again: the bridge's Metamod role can be unloaded and loaded
    /// again in one process (`crate::plugin`), and the second life must serve requests. The C's
    /// `g_shuttingDown` has no such path because the C bridge never unloads; this is the one
    /// difference the Rust port needs.
    ///
    /// Idempotent and harmless while the queue is open. Pending records stay: a re-`init` does
    /// not resurrect a queue, it only lets a new listener and a new drain use it.
    pub fn reopen(&self) {
        self.lock().shutting_down = false;
    }
}

impl Default for Queues {
    fn default() -> Self {
        Self::new()
    }
}

/// Clears [`Inner::draining`] when `drain` returns or unwinds.
struct DrainingGuard<'a>(&'a Queues);

impl Drop for DrainingGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().draining = false;
    }
}

/// `deadlineMs && now > receivedAt + deadlineMs` (`mcp_bridge.c:1593` sweep, `:6299`/`:6301`
/// drain): the C's `deadlineMs == 0` means "no deadline", so `Some(Duration::ZERO)` is no
/// deadline here, and so is `None` (the absent-deadline mapping of `:1615`).
fn is_expired(request: &Request, now: Instant) -> bool {
    match request.deadline {
        None | Some(Duration::ZERO) => false,
        Some(deadline) => now.saturating_duration_since(request.received) > deadline,
    }
}

/// `Condvar::wait_timeout`, treating poisoning like [`Queues::lock`] does.
fn wait_timeout<'a>(
    condvar: &Condvar,
    guard: MutexGuard<'a, Inner>,
    timeout: Duration,
) -> MutexGuard<'a, Inner> {
    condvar
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0
}

/// `Condvar::wait`, treating poisoning like [`Queues::lock`] does.
fn wait<'a>(condvar: &Condvar, guard: MutexGuard<'a, Inner>) -> MutexGuard<'a, Inner> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The exact error envelope of the C's `build_error`/`build_error_ext`
/// (`bridge/mcp_bridge.c:1563-1583`), one line including the trailing `\n`:
///
/// ```text
/// {"jsonrpc":"2.0","id":<id|null>,"bridge":{"protocol":1,"build":"m2.20"},
///  "error":{"code":<code>,"message":<msg>[,"data":<data_json>]}}\n
/// ```
///
/// `id` is JSON-escaped exactly as `sb_addjson` does (`:289-319`; `None` is the C's
/// `!hasId` -> `null`). `msg` is escaped the same way. `data_json` is **raw JSON**, inserted
/// as the C's `sb_add(b, dataJson)` does (`:1579`) - the caller owns its validity - and
/// omitted when `None`.
pub fn error_reply(
    id: Option<&str>,
    code: i32,
    msg: &str,
    data_json: Option<&str>,
) -> String {
    let mut out = String::with_capacity(96 + id.map_or(0, str::len) + msg.len());
    out.push_str("{\"jsonrpc\":\"2.0\",\"id\":");
    match id {
        Some(id) => push_json_string(&mut out, id),
        None => out.push_str("null"),
    }
    out.push_str(",\"bridge\":{\"protocol\":1,\"build\":\"");
    out.push_str(BRIDGE_BUILD);
    out.push_str("\"},\"error\":{\"code\":");
    out.push_str(&code.to_string());
    out.push_str(",\"message\":");
    push_json_string(&mut out, msg);
    if let Some(data) = data_json {
        out.push_str(",\"data\":");
        out.push_str(data);
    }
    out.push_str("}}\n");
    out
}

/// `sb_addjson` (`mcp_bridge.c:289-319`) for a Rust string: quotes, backslashes, `\n`, `\r`,
/// `\t`, other control bytes as `\u00xx`, everything else verbatim.
///
/// The C stops at the string's NUL and replaces invalid UTF-8 with U+FFFD; a `&str` is always
/// valid UTF-8, and it is cut at a NUL here so the two produce the same bytes.
fn push_json_string(out: &mut String, text: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let text = match text.find('\0') {
        Some(end) => &text[..end],
        None => text,
    };
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let byte = c as u8;
                out.push_str("\\u00");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 15)]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
