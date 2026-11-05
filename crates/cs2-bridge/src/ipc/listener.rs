//! The listener: token, bind, discovery, accept loop, the hello handshake and the per-connection
//! NDJSON state machine.
//!
//! Port of `ipc_thread` (`mcp_bridge.c:6336-6702`) and the token/path half of
//! `start_listener_once` (`:6730-6779`). The reject/parse/queue decisions are cited inline at
//! each site; [`super`] lists the structural differences (thread per connection instead of one
//! `select` loop, and the method table living in the tick).

use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::entity_json::{json_int, json_string};
use crate::log;
use crate::queue::{self, Request};

use super::discovery::{self, Ticket};
use super::envelope::{self, Frame};
use super::token;
use super::{
    BUSY, DEFAULT_DEADLINE_MS, HELLO_DEADLINE, ID_BUF, MAX_LINE, METHOD_NOT_FOUND, PARSE_ERROR,
    RECV_BUF, TICK, TOKEN_BUF,
};

/// The `-32600` the C answers a malformed or id-less request and an over-long line with
/// (`:6416`, `:6459`, `:6469`).
const INVALID_REQUEST: i32 = -32600;

/// `control.shutdown`'s reply (`mcp_bridge.c:6592`), the constant the lane answers when the
/// handler declines the call (the C always shuts down, so the handler cannot veto it).
const CLOSING: &str = crate::control::CLOSING;

/// `-32002 VersionMismatch`, the hello reply to a `protocol` other than 1 (`:6433`).
const VERSION_MISMATCH: i32 = -32002;

/// `-32001 NotAuthenticated`, the hello reply to a wrong token and the pre-auth reply to any
/// other first frame (`:6439`, `:6412`).
const NOT_AUTHENTICATED: i32 = -32001;

/// `control.cancel`'s `-32010`, built by the handler when nothing was removed (`:6586`).
pub const NOT_CANCELLABLE: i32 = -32010;

/// How long the whole teardown waits for its threads. The C bounds each join with
/// `WaitForSingleObject(..., 2000)` (`mcp_bridge.c:6796-6797`); one deadline covers the
/// accept, connection and writer joins. It is deliberately larger than [`WRITE_TIMEOUT`]: a
/// send that hits the timeout must be allowed to return and the writer to be joined, so the
/// "still running" answer is reserved for a `send` that `shutdown` did not wake at all. A stop
/// that misses the deadline stays under this bound and keeps the thread (never detaches it), so
/// a retried stop can join it.
const STOP_TIMEOUT: Duration = Duration::from_millis(3000);

/// How often the bounded join re-checks [`JoinHandle::is_finished`]. Small enough that a
/// finished thread is reaped promptly, large enough not to spin while the deadline runs.
const JOIN_POLL: Duration = Duration::from_millis(1);

/// The write timeout of every accepted stream: a host that stops reading must not leave a
/// writer (or a synchronous reply) blocked in `write_all` for the process's lifetime. The C
/// has no write timeout - it relies on `shutdown(SD_BOTH)` in `PreShutdown` (`:6791-6793`) to
/// wake a pending `send`, which Wine does not always do; its bounded joins (`:6796-6797`) are
/// the acknowledgement that a send can stay stuck. This is that bound on the send itself.
const WRITE_TIMEOUT: Duration = Duration::from_millis(2000);

/// The reply-building half of a `control.*` method: the IPC lane owns the envelope, the handler
/// owns the `result` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlReply {
    /// Raw JSON for the envelope's `result` member (the handler's `build_caps`/result text).
    Result(String),
    /// A `build_error_ext`-shaped refusal (`:1563-1583`); `data` is raw JSON, inserted only
    /// when present.
    Error {
        /// The JSON-RPC error code.
        code: i32,
        /// The error message (JSON-escaped by the lane).
        message: String,
        /// Raw JSON for the error's `data` member.
        data: Option<String>,
    },
}

/// What the IPC lane did before asking the handler for a `control.*` reply, so the handler can
/// answer without touching the queues itself.
#[derive(Debug, Clone, Copy)]
pub enum ControlCall<'a> {
    /// `control.cancel` (`:6568-6590`). `queue::remove_id` has already run; `removed` is the C's
    /// `found`. A handler answers `{"cancelled":true}` or `-32010 NotCancellable`.
    Cancel {
        /// The frame's raw `params` object text, when it had one.
        params: Option<&'a str>,
        /// Whether a queued request with the named id was removed.
        removed: bool,
    },
    /// `control.shutdown` (`:6591-6600`). The lane flushes this generation's outbound records,
    /// sends the reply synchronously, marks the bridge shutting down and closes the connection
    /// after it; the handler only builds `{"closing":true}` (it cannot veto the shutdown - the
    /// lane answers the C's constant when the handler declines).
    Shutdown,
    /// Any other `control.*` (`ping`, `status`, `capabilities`, `version`, ...): the lane has no
    /// side effect for it and the handler answers. `None` from the handler is `-32601`.
    Other {
        /// The `control.*` method name.
        method: &'a str,
        /// The frame's raw `params` object text, when it had one.
        params: Option<&'a str>,
    },
}

/// The caller-supplied `control.*` implementation (WP2.C's `control.rs` at integration).
///
/// The IPC lane calls it on the connection thread; it may call the queue's read-only accessors
/// (`depth`, `inflight_since`, `is_shutting_down`) as `control.status`/`control.ping` do. It
/// must not block waiting for the main thread: the C answers these arms from the IPC thread
/// too (`:6466-6599`).
pub trait ControlHandler: Send + Sync + 'static {
    /// The capabilities object the `control.hello` reply carries (`build_caps`, `:6447`).
    ///
    /// Raw JSON object text; inserted after `"capabilities":` verbatim.
    fn capabilities(&self) -> String;

    /// The reply to one `control.*` call, or `None` for `-32601 MethodNotFound` (the C's
    /// `control.` fallthrough, `:6599-6601`). `control.hello` after authentication never
    /// reaches the handler: the lane answers it `-32601`, as the C's fallthrough does.
    fn control(&self, call: ControlCall<'_>) -> Option<ControlReply>;
}

/// What the IPC lane does with a non-`control.*` request before it takes a queue slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The C's enqueue arm (`mcp_bridge.c:6623` and on): the request becomes a queue record
    /// for the main thread.
    Queue,
    /// Answer now, without touching the queue - the C's three immediate arms: `-32601` for an
    /// unknown name (`:6601-6610`), `-32011 NotImplemented` for a blocked row (`:6611-6616`),
    /// and `-32601` for a `needs == 0` row that reached this arm (`:6618-6621`).
    Reply(ControlReply),
}

/// The method table as the IPC lane needs it: the C's `find_method` / `!impl` / `needs == 0`
/// chain (`mcp_bridge.c:6603-6623`), which decides *before* the queue whether a request can
/// ever run.
///
/// [`crate::wiring::BridgeState`] is the production implementation; [`QueueEverything`] is the
/// "no table yet" default (the pre-WP2.D behaviour of this lane, kept so a test that only
/// exercises the reader can start a listener without one).
pub trait MethodRouter: Send + Sync + 'static {
    /// The decision for one non-`control.*` method name.
    fn route(&self, method: &str) -> Route;

    /// [`MethodRouter::route`] with the request's raw `params` object text (`None` when the
    /// request carries none). A row answered on the lane that reads its parameters (`job.*`)
    /// overrides this; the default ignores them.
    fn route_with_params(&self, method: &str, params: Option<&str>) -> Route {
        let _ = params;
        self.route(method)
    }
}

/// A [`MethodRouter`] that queues every method - the C's enqueue arm with no table lookup.
pub struct QueueEverything;

impl MethodRouter for QueueEverything {
    fn route(&self, _method: &str) -> Route {
        Route::Queue
    }
}

/// The OS RNG seam: fills 32 bytes, `false` on failure (the fail-closed path,
/// `mcp_bridge.c:6736-6738`). Tests inject a failing or fixed source.
pub type Rng = Box<dyn Fn(&mut [u8; token::RAW_BYTES]) -> bool + Send + Sync>;

/// What [`start`] needs.
pub struct Config {
    /// The `control.*` implementation; [`start`] holds it for the listener's lifetime.
    pub handler: Arc<dyn ControlHandler>,
    /// The non-`control.*` method gate (the C's `find_method` chain, `mcp_bridge.c:6603-6623`).
    pub router: Arc<dyn MethodRouter>,
    /// Where `session.json` is written. `None` skips discovery (the C only falls back to a
    /// path, never to no file; tests use this to keep the tmpdir clean).
    pub discovery_path: Option<PathBuf>,
    /// `control.hello`'s deadline: the C's 5000 ms is [`HELLO_DEADLINE`], and tests shrink it.
    pub hello_deadline: Duration,
    /// The OS RNG (the C's `BCryptGenRandom`).
    pub rng: Rng,
    /// Written to the discovery file (`GetCurrentProcessId`, `:6706`).
    pub pid: u32,
    /// Written as `startedAt` (the C's `g_startTick`, `:6714`).
    pub started_at_ms: u64,
}

impl Config {
    /// The production defaults: the platform discovery path, [`HELLO_DEADLINE`], the OS RNG,
    /// this process's pid, `started_at_ms = 0` and a router that queues everything.
    ///
    /// `started_at_ms` is 0 because the C's value is a boot-relative tick with no portable
    /// equivalent; the field is a hint (the host records `startedAt` but never decides on it).
    /// The production caller ([`crate::wiring`]) overrides it with its own monotonic clock and
    /// supplies the real [`MethodRouter`].
    pub fn new(handler: Arc<dyn ControlHandler>) -> Self {
        Self {
            handler,
            router: Arc::new(QueueEverything),
            discovery_path: discovery::default_path().ok(),
            hello_deadline: HELLO_DEADLINE,
            rng: Box::new(token::random),
            pid: std::process::id(),
            started_at_ms: 0,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("discovery_path", &self.discovery_path)
            .field("hello_deadline", &self.hello_deadline)
            .field("pid", &self.pid)
            .field("started_at_ms", &self.started_at_ms)
            .finish_non_exhaustive()
    }
}

/// The next connection generation (`mcp_bridge.c:1023`, `static u32 g_nextGen = 1`).
///
/// Process-wide, not per [`Listener`]: the queue is process-wide, so a listener that starts
/// after an earlier one stopped must not hand out a generation whose outbound records or
/// in-flight replies are still around (they would reach the new client as replies to ids it
/// never sent).
static NEXT_GEN: AtomicU32 = AtomicU32::new(1);

/// One connection's socket and the lock every write to it goes through.
///
/// The C's `ipc_thread` and `writer_thread` both call `send_all` on the same socket with
/// nothing serialising them (`:1546`, `:6452`), so two replies can interleave mid-line. This
/// lane keeps the same order but serialises the writes: the connection thread's synchronous
/// replies ([`Connection::send_now`]) and the writer's records ([`Shared::deliver`]) both go
/// through [`Conn::write_line`].
pub(crate) struct Conn {
    stream: TcpStream,
    write: Mutex<()>,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        // Every accepted stream carries the write timeout, so no `write_all` on it (the
        // writer's record or a synchronous reply) can outlive [`WRITE_TIMEOUT`]. A failure to
        // set it is logged and not fatal: the socket is still usable, only unbounded in the
        // one case the stop deadline then covers.
        if let Err(error) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
            log::line(&format!("ipc: write timeout setup failed: {error}"));
        }
        Self {
            stream,
            write: Mutex::new(()),
        }
    }

    /// `send_all` (`:1531-1538`) under the connection's write lock.
    ///
    /// A failed send closes the socket. The C only ever saw `send_all` fail on a dead socket;
    /// the write timeout makes a **partial** send reachable, and a partial line followed by
    /// the next record would corrupt the NDJSON framing. Shutting the stream down makes the
    /// connection thread read EOF, purge the generation and free the connection slot, so the
    /// receiver sees the end of the stream instead of a spliced line.
    fn write_line(&self, bytes: &[u8]) -> io::Result<()> {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stream = &self.stream;
        match stream.write_all(bytes).and_then(|()| stream.flush()) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = self.stream.shutdown(Shutdown::Both);
                Err(error)
            }
        }
    }

    /// `shutdown(c, SD_BOTH)` (`:1541`, `:6391`, `:6414`); does not take the write lock, so it
    /// also unblocks a writer stuck in `send_all`.
    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The authenticated connection and the generation it belongs to
/// (`g_authedClient`/`g_clientGen`, `:1019-1020`).
struct Authed {
    gen: u32,
    conn: Arc<Conn>,
}

/// State shared by the accept thread, the connection thread and the writer.
pub(crate) struct Shared {
    /// Set by [`Listener::stop`] and by `control.shutdown`; the C's `g_shuttingDown` for this
    /// lane.
    pub(crate) shutdown: AtomicBool,
    /// A connection is accepted and not yet closed: the C's
    /// `client != INVALID_SOCKET || g_authedClient != INVALID_SOCKET` (`:6385`).
    active: AtomicBool,
    /// The authenticated connection and its generation (`g_authedClient`/`g_clientGen`,
    /// `:1019-1020`).
    authed: Mutex<Option<Authed>>,
    /// The socket of the connection currently being served, authenticated or not, so
    /// [`Listener::stop`] can shut it down before joining (the C's `Shutdown` does the same,
    /// `:6792-6793`).
    live: Mutex<Option<Arc<Conn>>>,
    /// The live connection thread, so [`Listener::stop`] can join it.
    connection: Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<Authed>> {
        // No invariant spans two fields of this slot (the writer clones the handle out and the
        // connection thread replaces it), so poisoning is not a reason to panic the host.
        self.authed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The live connection, when there is one.
    fn live(&self) -> Option<Arc<Conn>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Makes `gen` the authenticated generation (`:6442-6443`).
    fn set_authed(&self, gen: u32, conn: Arc<Conn>) {
        *self.lock() = Some(Authed { gen, conn });
    }

    /// Clears the slot only when it still belongs to `gen`, so a late close of one connection
    /// cannot forget a newer one (the C's unconditional clear is safe only because it never
    /// serves two connections; see `:6671-6672`).
    fn clear_authed(&self, gen: u32) {
        let mut slot = self.lock();
        if slot.as_ref().is_some_and(|authed| authed.gen == gen) {
            *slot = None;
        }
    }

    /// Sends one writer record: only to the connection whose generation produced it (`:1548`).
    pub(crate) fn deliver(&self, gen: u32, bytes: &[u8]) -> Delivery {
        let target = {
            let slot = self.lock();
            slot.as_ref()
                .filter(|authed| authed.gen == gen)
                .map(|authed| Arc::clone(&authed.conn))
        };
        match target {
            Some(conn) => match conn.write_line(bytes) {
                Ok(()) => Delivery::Sent,
                Err(_) => Delivery::Failed,
            },
            None => Delivery::Stale,
        }
    }
}

/// The writer's outcome for one record, so the caller can log the C's two lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// `send_all` answered 1 (`:1550`).
    Sent,
    /// `send_all` answered 0 (`:1551`).
    Failed,
    /// No authenticated connection of that generation (`:1553`).
    Stale,
}

/// A running listener. Dropping it stops and joins the threads, like the C's `Shutdown`
/// (`:6786-6799`).
pub struct Listener {
    port: u16,
    token: String,
    discovery_path: Option<PathBuf>,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Listener {
    /// The bound ephemeral port (`g_port`, `:6358`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The 64-hex session token the hello frame must carry (`g_token`, `:1013`).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Where the discovery file was written, when discovery was enabled.
    pub fn discovery_path(&self) -> Option<&std::path::Path> {
        self.discovery_path.as_deref()
    }

    /// Stops the listener and joins every thread; idempotent, and a retry joins the threads that
    /// missed an earlier deadline. Answers whether **every** thread (accept, connection, writer)
    /// was joined: false means one is still running this image's code, so the caller must not
    /// let the image be unmapped (the Metamod unload refuses with `UnloadTimeout`).
    ///
    /// The live socket is shut down first (the C's `PreShutdown`, `:6791-6793`), so a client
    /// that stopped reading cannot block the writer's `send_all` past this call. The joins
    /// share one [`STOP_TIMEOUT`] deadline (the C's two 2000 ms waits, `:6796-6797`). A thread
    /// still running when it expires is left in its slot, not detached, so the next `stop`
    /// joins it when it exits. The discovery file is deleted with the socket (the C's
    /// `Shutdown`, `:6799`), so no host is left pointed at a dead port with a stale token.
    pub fn stop(&mut self) -> bool {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(conn) = self.shared.live() {
            conn.shutdown();
        }
        // One deadline for all three joins: the C waits up to 2 s per handle, and the stop
        // path must stay bounded even when every thread is stuck on the same dead socket.
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut all = join_bounded(&mut self.accept, "ipc", deadline);
        let mut connection = self
            .shared
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        all &= join_bounded(&mut connection, "connection", deadline);
        drop(connection);
        all &= join_bounded(&mut self.writer, "writer", deadline);
        if let Some(path) = &self.discovery_path {
            let _ = std::fs::remove_file(path);
        }
        all
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // The refusal path never drops a listener whose threads are still running: the runtime
        // keeps it so a retried stop can join them. Drop is the last word (process teardown,
        // tests) and then the outstanding handles detach with the struct.
        let _ = self.stop();
    }
}

/// Joins one teardown thread, waiting at most until `deadline`; a thread that misses it stays in
/// `slot` — still owned by the [`Listener`] — and the answer is false, so a retried [`stop`]
/// can join it once it exits. `std` has no timed `join`, so the wait polls
/// [`JoinHandle::is_finished`] and only joins a thread that has finished: the thread is never
/// detached, which is what lets the Metamod unload refuse while a thread of this image is still
/// running. The wait is bounded by the single deadline shared by every thread.
///
/// [`stop`]: Listener::stop
fn join_bounded(slot: &mut Option<JoinHandle<()>>, what: &str, deadline: Instant) -> bool {
    let Some(handle) = slot.as_ref() else {
        return true;
    };
    loop {
        if handle.is_finished() {
            // The thread is done; the join cannot block. A panicked thread reports its panic
            // here, which is not a reason to fail the host's teardown.
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log::line(&format!(
                "ipc: {what} thread still running at the stop deadline; it stays owned by the \
                 listener so a retried stop can join it"
            ));
            return false;
        }
        thread::sleep(remaining.min(JOIN_POLL));
    }
}

/// Starts the token, the listener, the discovery file, the writer and the accept thread.
///
/// `Ok(None)` is the C's fail-closed RNG path (`:6736-6738`) - a log line, no listener and no
/// discovery file - and also the C's `if (g_started || g_shuttingDown) return;` refusal
/// (`:6731`): once the process-wide queue is shut down, a listener could accept and
/// authenticate a client but never deliver a reply, because the writer exits on its first
/// empty pop. `Err` is a bind/listen failure, which the C only logs (`:6348-6362`) because its
/// thread has nobody to return to; a caller with a place to report it gets the error.
///
/// The discovery write failure is **not** fatal, as in the C (`:6364`: the listener keeps
/// serving), but it is logged with the path - with one exception: an existing discovery
/// directory that is not private to this user is a fail-closed refusal (`Ok(None)`), because
/// the token file it would hold is a capability and this crate will not write it where
/// another user can reach it.
pub fn start(config: Config) -> io::Result<Option<Listener>> {
    if queue::global().is_shutting_down() {
        log::line("bridge: shutting down; refusing to start the listener");
        return Ok(None);
    }
    let mut raw = [0u8; token::RAW_BYTES];
    if !(config.rng)(&mut raw) {
        log::line("token: OS RNG failed; refusing to start (fail closed)");
        return Ok(None);
    }
    let token = token::from_bytes(&raw);

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    log::line(&format!("ipc: listening port {port}"));

    let discovery_path = config.discovery_path;
    if let Some(path) = &discovery_path {
        let text = discovery::render(&Ticket {
            pid: config.pid,
            port,
            token: &token,
            started_at: config.started_at_ms,
        });
        match discovery::write(path, &text) {
            Ok(()) => log::line("discovery: written"),
            // Refuse to serve: an existing discovery directory that another user owns or can
            // reach would expose the session token, so a listener started into it is no
            // listener at all (fail closed, unlike the C's log-and-serve `:6364`).
            Err(error) if discovery::is_insecure_directory(&error) => {
                log::line(&format!("discovery: refusing to start: {error}"));
                return Ok(None);
            }
            // The C logs and serves on for every other write failure (`:6364`).
            Err(error) => log::line(&format!(
                "ipc: discovery write failed for {}: {error}",
                path.display()
            )),
        }
    }

    let shared = Arc::new(Shared {
        shutdown: AtomicBool::new(false),
        active: AtomicBool::new(false),
        authed: Mutex::new(None),
        live: Mutex::new(None),
        connection: Mutex::new(None),
    });
    let context = Arc::new(ConnectionContext {
        handler: config.handler,
        router: config.router,
        token: token.clone(),
        hello_deadline: config.hello_deadline,
    });

    let writer = {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("cs2-bridge-writer".into())
            .spawn(move || super::writer::run(shared))?
    };
    let accept = {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("cs2-bridge-ipc".into())
            .spawn(move || accept_loop(listener, shared, context))?
    };
    log::line("ipc: thread start");

    Ok(Some(Listener {
        port,
        token,
        discovery_path,
        shared,
        accept: Some(accept),
        writer: Some(writer),
    }))
}

/// What a connection thread needs from [`Config`].
struct ConnectionContext {
    handler: Arc<dyn ControlHandler>,
    router: Arc<dyn MethodRouter>,
    token: String,
    hello_deadline: Duration,
}

/// The accept loop: one connection at a time, extras answered `-32007 Busy` (`:6374-6400`).
fn accept_loop(server: TcpListener, shared: Arc<Shared>, context: Arc<ConnectionContext>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match server.accept() {
            Ok((stream, _peer)) => {
                // Winsock gives an accepted socket the listening socket's properties, including
                // non-blocking mode (`listener.set_nonblocking(true)` above); Linux does not.
                // The connection is blocking, like the C's (`:6388-6399`).
                if let Err(error) = stream.set_nonblocking(false) {
                    log::line(&format!(
                        "ipc: failed to make the connection blocking: {error}"
                    ));
                }
                let conn = Arc::new(Conn::new(stream));
                if shared
                    .active
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    // `build_error(&b, 0, 0, -32007, "Busy")`, then `shutdown`+close (`:6386-6393`).
                    let reply = queue::error_reply(None, BUSY, "Busy", None);
                    let _ = conn.write_line(reply.as_bytes());
                    conn.shutdown();
                    log::line("ipc: extra client -> Busy");
                    continue;
                }
                let gen = NEXT_GEN.fetch_add(1, Ordering::AcqRel);
                log::line(&format!("ipc: client connected gen={gen}"));
                *shared
                    .live
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&conn));
                let thread_shared = Arc::clone(&shared);
                let thread_context = Arc::clone(&context);
                let thread_conn = Arc::clone(&conn);
                match thread::Builder::new()
                    .name(format!("cs2-bridge-conn-{gen}"))
                    .spawn(move || {
                        let _active = ActiveGuard {
                            shared: Arc::clone(&thread_shared),
                            conn: Arc::clone(&thread_conn),
                        };
                        let mut connection =
                            Connection::new(thread_conn, gen, thread_context, thread_shared);
                        connection.run();
                    }) {
                    Ok(handle) => {
                        *shared
                            .connection
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
                    }
                    Err(error) => {
                        log::line(&format!("ipc: connection thread failed: {error}"));
                        shared.active.store(false, Ordering::Release);
                        let mut live = shared
                            .live
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if live.as_ref().is_some_and(|live| Arc::ptr_eq(live, &conn)) {
                            *live = None;
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(error) => {
                log::line(&format!("ipc: accept failed: {error}"));
                thread::sleep(TICK);
            }
        }
    }
    log::line("ipc: thread exit");
}

/// Clears [`Shared::active`] and [`Shared::live`] when the connection thread ends, on every
/// path. The live slot is cleared only when it is still this connection, so a newer one is
/// never forgotten.
struct ActiveGuard {
    shared: Arc<Shared>,
    conn: Arc<Conn>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
        let mut live = self
            .shared
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if live
            .as_ref()
            .is_some_and(|conn| Arc::ptr_eq(conn, &self.conn))
        {
            *live = None;
        }
    }
}

/// One `recv`'s outcome (`:6409`): end of stream, the C's `> 0` case, or the tick.
enum Recv {
    /// `recv <= 0`: the client closed or failed.
    Eof,
    /// `recv > 0`, this many bytes.
    Bytes(usize),
    /// Nothing to read within the 100 ms timeout (or an interrupted wait).
    Tick,
}

/// One connection's NDJSON state (`client`, `gen`, `authed`, `linelen`, `overflow`,
/// `close_after_reply`, `helloDeadline`, `:6367-6371`).
struct Connection {
    conn: Arc<Conn>,
    gen: u32,
    context: Arc<ConnectionContext>,
    shared: Arc<Shared>,
    line: Vec<u8>,
    linelen: usize,
    overflow: bool,
    authed: bool,
    close_after_reply: bool,
    hello_deadline: Instant,
}

impl Connection {
    fn new(
        conn: Arc<Conn>,
        gen: u32,
        context: Arc<ConnectionContext>,
        shared: Arc<Shared>,
    ) -> Self {
        // The C's 100 ms `select` tick (`:6392-6393`): the timeout is how the sweep and the
        // hello deadline run while the client is silent.
        if let Err(error) = conn.stream.set_read_timeout(Some(TICK)) {
            log::line(&format!("ipc: read timeout setup failed: {error}"));
        }
        // `helloDeadline = now + 5000` at accept (`:6394`), with the deadline a parameter.
        let hello_deadline = Instant::now() + context.hello_deadline;
        Self {
            conn,
            gen,
            context,
            shared,
            line: vec![0u8; MAX_LINE],
            linelen: 0,
            overflow: false,
            authed: false,
            close_after_reply: false,
            hello_deadline,
        }
    }

    /// The recv loop (`:6403-6696`).
    fn run(&mut self) {
        // The C's `cbuf` (`:6388`), one chunk at a time so the parser can mutate `self`.
        let mut chunk = [0u8; RECV_BUF];
        loop {
            if self.shared.shutdown.load(Ordering::Acquire) {
                break;
            }
            let recv = {
                // `TcpStream` reads through `&TcpStream`, which is what keeps the socket owned
                // by the one [`Conn`] the writer shares.
                let mut reader = &self.conn.stream;
                match reader.read(&mut chunk) {
                    Ok(0) => Recv::Eof,
                    Ok(n) => Recv::Bytes(n),
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            || error.kind() == io::ErrorKind::TimedOut =>
                    {
                        // The tick's own exit: the sweep and the hello deadline below still run.
                        Recv::Tick
                    }
                    // A signal interrupted the wait; the C's `recv` would just be called again
                    // (`:6409`). Not a disconnect.
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => Recv::Tick,
                    // `recv <= 0` (`:6409-6417`): the client closed or failed.
                    Err(_) => Recv::Eof,
                }
            };
            let closed = match recv {
                // The C's `for (i = 0; i < n && !close_after_reply; i++)` (`:6408-6700`).
                Recv::Bytes(n) => self.feed(&chunk[..n]),
                Recv::Eof => true,
                Recv::Tick => false,
            };

            // `q_sweep_expired()` on every pass (`:6675-6676`).
            queue::global().sweep_expired();

            // `if (client != INVALID_SOCKET && !authed && now > helloDeadline)` (`:6678-6683`).
            if !self.authed && Instant::now() > self.hello_deadline {
                log::line("ipc: hello timeout -> close");
                break;
            }
            // The deferred close after a synchronous reply (`:6685-6693`).
            if closed || self.close_after_reply {
                break;
            }
        }
        self.close();
    }

    /// Consumes one recv chunk; answers whether the connection must close immediately.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        for &byte in chunk {
            if byte != b'\n' {
                // `if (linelen + 1 < MAX_LINE) line[linelen++] = ch; else overflow = 1;`
                // (`:6647-6650`).
                if self.linelen + 1 < MAX_LINE {
                    self.line[self.linelen] = byte;
                    self.linelen += 1;
                } else {
                    self.overflow = true;
                }
                continue;
            }
            let len = self.linelen;
            self.linelen = 0;
            if self.overflow {
                // `build_error(&b, 0, 0, -32600, "line too long")`, send, close (`:6652-6659`).
                self.overflow = false;
                self.send_now(&queue::error_reply(
                    None,
                    INVALID_REQUEST,
                    "line too long",
                    None,
                ));
                self.close_after_reply = true;
                return true;
            }
            // `if (!line[0]) continue;` (`:6431`): an empty line and a line whose first byte
            // is NUL are both skipped without a reply, and the connection stays open.
            if len == 0 || self.line[0] == 0 {
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let close = self.handle_line(&line[..len]);
            self.line = line;
            if close {
                self.close_after_reply = true;
                return true;
            }
        }
        false
    }

    /// One complete line (`:6661-6700`); `true` means "close after this reply".
    fn handle_line(&mut self, line: &[u8]) -> bool {
        let frame = envelope::parse(line);

        // `if (!hasMethod || !json_find(line, "jsonrpc"))` (`:6424-6430`).
        if frame.method.is_none() || !frame.has_jsonrpc {
            let reply = queue::error_reply(frame.id.as_deref(), PARSE_ERROR, "parse error", None);
            if !self.authed {
                // Pre-auth: deliver synchronously, then close (`:6428`).
                self.send_now(&reply);
                return true;
            }
            // Post-auth: the writer delivers it, like every other non-control reply.
            let _ = queue::global().out_push(reply, self.gen);
            return false;
        }
        let method = frame.method.as_deref().unwrap_or_default();

        if !self.authed {
            return self.authenticate(line, &frame, method);
        }

        // `if (!hasId)` - v1 has no client notifications (`:6457-6463`).
        if frame.id.is_none() {
            let reply =
                queue::error_reply(None, INVALID_REQUEST, "invalid request: id required", None);
            let _ = queue::global().out_push(reply, self.gen);
            return false;
        }
        let id = frame.id.as_deref().unwrap_or_default();

        if is_control(method) {
            return self.handle_control(line, &frame, method, id);
        }

        // The C's `find_method` chain (`:6603-6623`): a name the table cannot ever run is
        // answered here, immediately, and never takes a queue slot.
        match self.context.router.route_with_params(method, frame.params.as_deref()) {
            Route::Queue => self.enqueue(frame.params, method, id),
            Route::Reply(reply) => {
                let _ = queue::global().out_push(render(Some(reply), id), self.gen);
            }
        }
        false
    }

    /// The pre-auth arm: `control.hello` with protocol 1 and the token, or close (`:6401-6455`).
    fn authenticate(&mut self, line: &[u8], frame: &Frame, method: &str) -> bool {
        if method != "control.hello" {
            // `-32001 NotAuthenticated`, then close (`:6405-6414`).
            let reply = queue::error_reply(
                frame.id.as_deref(),
                NOT_AUTHENTICATED,
                "NotAuthenticated",
                None,
            );
            self.send_now(&reply);
            return true;
        }

        // `json_int(line, "protocol", &proto)` then `json_string(line, "token", ...)`
        // (`:6424-6426`).
        let protocol = json_int(line, b"protocol").unwrap_or(0);
        let mut tokenbuf = [0u8; TOKEN_BUF];
        let has_token = json_string(line, b"token", &mut tokenbuf);
        let presented = tokenbuf_string(&tokenbuf);

        if protocol != 1 {
            let reply = queue::error_reply(
                frame.id.as_deref(),
                VERSION_MISMATCH,
                "VersionMismatch",
                None,
            );
            self.send_now(&reply);
            log::line("ipc: hello protocol mismatch -> close");
            return true;
        }
        // `ct_eq(tokbuf, g_token)` (`:6439`).
        if !has_token || !token::ct_eq(&presented, &self.context.token) {
            let reply = queue::error_reply(
                frame.id.as_deref(),
                NOT_AUTHENTICATED,
                "NotAuthenticated",
                None,
            );
            self.send_now(&reply);
            log::line("ipc: hello bad token -> close");
            return true;
        }

        self.authed = true;
        self.shared.set_authed(self.gen, Arc::clone(&self.conn));

        // The C's hello reply (`:6444-6451`), with the capabilities object supplied by the
        // handler in place of `build_caps`.
        let reply = hello_reply(frame.id.as_deref(), &self.context.handler.capabilities());
        self.send_now(&reply); // "the auth gate: deliver synchronously" (`:6452`)
        log::line("ipc: hello ok");
        false
    }

    /// The authenticated `control.*` arms (`:6466-6601`). Only the two side effects belong to
    /// this lane; the reply text is the handler's.
    fn handle_control(&mut self, line: &[u8], frame: &Frame, method: &str, id: &str) -> bool {
        let params = frame.params.as_deref();
        if method == "control.hello" {
            // Already authenticated: the C has no second hello arm, so it falls through the
            // `control.` catch-all to `-32601` (`:6599-6601`). Answering it from the handler
            // would put capabilities on a frame the C refuses.
            let _ = queue::global().out_push(method_not_found(id), self.gen);
            return false;
        }

        if method == "control.cancel" {
            // `json_find(line, "params")`, then `json_string(pp, "id", ...)`, then
            // `q_remove_id` (`:6569-6574`).
            let mut removed = false;
            if let Some(start) = frame.params_start {
                let mut target = [0u8; ID_BUF];
                if envelope::nested_string(line, start, b"id", &mut target) {
                    removed = queue::global().remove_id(&tokenbuf_string(&target));
                }
            }
            let reply = self
                .context
                .handler
                .control(ControlCall::Cancel { params, removed });
            let _ = queue::global().out_push(render(reply, id), self.gen);
            return false;
        }

        if method == "control.shutdown" {
            // The C's reply is a constant (`:6592`) and the shutdown always happens, so a
            // handler that declines cannot veto either: the lane shuts down regardless and
            // answers the constant.
            let reply = self
                .context
                .handler
                .control(ControlCall::Shutdown)
                .unwrap_or(ControlReply::Result(CLOSING.to_string()));
            // `flush_outbound(gen, 1000)` then the synchronous send (`:6594-6595`).
            queue::global().flush_outbound(self.gen, Duration::from_millis(1000));
            self.send_now(&render(Some(reply), id));
            log::line("ipc: control.shutdown");
            // `g_shuttingDown = 1`, wake the queues, stop accepting (`:6596-6599`).
            queue::global().shutdown();
            self.shared.shutdown.store(true, Ordering::Release);
            return true;
        }

        let call = ControlCall::Other { method, params };
        let _ = queue::global().out_push(render(self.context.handler.control(call), id), self.gen);
        false
    }

    /// The `else` arm: a non-control request becomes a [`Request`] for the main thread
    /// (`:6603-6696`), or `-32602`/`-32007` when the params or the queue refuse it.
    fn enqueue(&mut self, params: Option<String>, method: &str, id: &str) {
        // `deadlineMs` only exists when the request carried a params object (`:6652-6656`);
        // the C's default is 30000 and an explicit 0 means no deadline.
        let mut deadline = Some(Duration::from_millis(DEFAULT_DEADLINE_MS));
        if let Some(params) = &params {
            if let Some(value) = envelope::deadline_ms(params.as_bytes()) {
                if value < 0 {
                    let reply = queue::error_reply(
                        Some(id),
                        -32602,
                        "invalid params: deadlineMs < 0",
                        None,
                    );
                    let _ = queue::global().out_push(reply, self.gen);
                    return;
                }
                deadline = (value != 0).then(|| Duration::from_millis(value as u64));
            }
        }
        let request = Request {
            id: id.to_string(),
            method: method.to_string(),
            params,
            // `rq.receivedAt = GetTickCount64()` (`:6649`).
            received: Instant::now(),
            deadline,
            gen: self.gen,
        };
        match queue::global().push(request) {
            Ok(()) => log::line("ipc: request queued for main thread"),
            // `-32007 Busy` when the queue is full (`:6662-6666`).
            Err(_) => {
                let reply = queue::error_reply(Some(id), BUSY, "Busy", None);
                let _ = queue::global().out_push(reply, self.gen);
            }
        }
    }

    /// `send_all` for a reply that must not wait for the writer (`:1531-1538`): the auth gate,
    /// every pre-auth error, the overflow reply and `control.shutdown`'s own reply. It goes
    /// through the connection's write lock, so it cannot interleave with a writer record.
    fn send_now(&mut self, line: &str) {
        let _ = self.conn.write_line(line.as_bytes());
    }

    /// `purge_gen` + authed-slot clear + close, in the C's order (`:6409-6416`, `:6686-6692`).
    fn close(&mut self) {
        queue::global().purge_gen(self.gen);
        if self.authed {
            self.shared.clear_authed(self.gen);
        }
        self.conn.shutdown();
        log::line("ipc: client closed");
    }
}

/// The C's `method[0..8] == "control."` test (`:6599`).
fn is_control(method: &str) -> bool {
    method.starts_with("control.")
}

/// `MethodNotFound` from the handler declining a call (`:6599-6601`).
fn method_not_found(id: &str) -> String {
    queue::error_reply(Some(id), METHOD_NOT_FOUND, "MethodNotFound", None)
}

/// The handler's reply as a full line, envelope included (`build_error`/`build_error_ext`,
/// `:1563-1583`), or `MethodNotFound` when there is no reply.
fn render(reply: Option<ControlReply>, id: &str) -> String {
    match reply {
        Some(ControlReply::Result(result)) => {
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"bridge\":{{\"protocol\":1,\"build\":\"{}\"}},\"result\":{result}}}\n",
                json_string_literal(id),
                queue::BRIDGE_BUILD,
            )
        }
        Some(ControlReply::Error {
            code,
            message,
            data,
        }) => queue::error_reply(Some(id), code, &message, data.as_deref()),
        None => method_not_found(id),
    }
}

/// The hello reply (`mcp_bridge.c:6444-6451`): the envelope, then protocol/bridge/game build
/// and the handler's capabilities object as `result`.
fn hello_reply(id: Option<&str>, capabilities: &str) -> String {
    let id = match id {
        Some(id) => json_string_literal(id),
        None => "null".to_string(),
    };
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"bridge\":{{\"protocol\":1,\"build\":\"{build}\"}},\
         \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{build}\",\"gameBuild\":\"{game}\",\
         \"capabilities\":{capabilities}}}}}\n",
        build = queue::BRIDGE_BUILD,
        game = crate::generated::pins::PIN_GAME_BUILD,
    )
}

/// `sb_addjson` (`mcp_bridge.c:289-319`) for an id this lane echoes, through the ported `Sb`.
///
/// The scratch buffer is sized for the worst case of the C's id buffer: 127 bytes, each escaped
/// to at most six (the C's `MAX_LINE` cap is far away).
fn json_string_literal(text: &str) -> String {
    let mut buf = [0u8; 6 * ID_BUF];
    let mut sb = crate::entity_json::Sb::new(&mut buf);
    sb.addjson(text.as_bytes());
    String::from_utf8_lossy(sb.as_cstr()).into_owned()
}

/// A C buffer as a string: up to its NUL.
fn tokenbuf_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// A listener whose only thread is a "writer" blocked until the returned sender is dropped: no
/// socket, no accept loop.
///
/// The seam for the teardown tests (this module's own cases and `wiring::stop_runtime`'s use it
/// too): a real socket write cannot be made to stay stuck past [`STOP_TIMEOUT`] -
/// [`WRITE_TIMEOUT`] is deliberately shorter, so a stream that stopped reading still lets the
/// writer return first.
#[cfg(test)]
pub(crate) fn held_writer() -> (Listener, std::sync::mpsc::Sender<()>) {
    let (gate, held) = std::sync::mpsc::channel::<()>();
    let writer = thread::Builder::new()
        .name("cs2-bridge-writer-held".into())
        .spawn(move || {
            let _ = held.recv();
        })
        .expect("spawn the held writer");
    (
        Listener {
            port: 0,
            token: String::new(),
            discovery_path: None,
            shared: Arc::new(Shared {
                shutdown: AtomicBool::new(false),
                active: AtomicBool::new(false),
                authed: Mutex::new(None),
                live: Mutex::new(None),
                connection: Mutex::new(None),
            }),
            accept: None,
            writer: Some(writer),
        },
        gate,
    )
}

#[cfg(test)]
mod tests {
    //! The teardown's answer: a thread that outlives the deadline keeps the listener unjoinable
    //! (and the listener keeps the thread), and the retry after it exits joins it. The seam is a
    //! real writer thread blocked on a channel - no socket, no accept loop.

    use super::*;

    /// A writer that misses the deadline is reported, not detached: the answer is false and the
    /// handle stays in the listener. Releasing the thread makes the retried stop answer true.
    #[test]
    fn a_stop_reports_a_thread_it_could_not_join_and_a_retry_joins_it() {
        let _guard = crate::hooks::test_guard::lock();
        let _scratch = crate::hooks::test_guard::scratch_log("ipc-stop-held-writer");
        let (mut listener, gate) = held_writer();
        assert!(
            !listener.stop(),
            "the writer is still running: the stop must say so"
        );
        assert!(
            listener.writer.is_some(),
            "the thread stays owned by the listener for the retry, never detached"
        );
        drop(gate);
        assert!(listener.stop(), "the retry joins the released writer");
        assert!(listener.writer.is_none(), "joined and taken");
        assert!(listener.stop(), "an idempotent stop with nothing left answers true");
    }

    /// The ordinary case: every thread (none here) joined, so the answer is true at once.
    #[test]
    fn a_listener_with_no_threads_stops_true() {
        let (mut listener, _gate) = held_writer();
        let writer = listener.writer.take().expect("the held writer");
        drop(writer);
        assert!(listener.stop());
    }
}
