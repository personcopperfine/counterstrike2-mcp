//! The IPC side of the bridge: the TCP listener, the NDJSON line reader, the `control.hello`
//! token handshake and the writer thread.
//!
//! Ported from `bridge/mcp_bridge.c`:
//!
//! * the listener thread `ipc_thread` (`:6336-6702`) - 127.0.0.1 with an ephemeral port, one
//!   authenticated connection, extra connections answered `-32007 Busy` and closed, the recv
//!   loop with line assembly and the `MAX_LINE` overflow reply, the JSON-RPC envelope checks,
//!   the hello handshake with its 5 s deadline, the per-connection generation, the queue push
//!   with `-32007 Busy` on a full queue, the rejection of `deadlineMs < 0`, the `control.cancel`
//!   and `control.shutdown` side effects, `purge_gen` on close and the per-pass
//!   `sweep_expired`;
//! * `send_all` and `writer_thread` (`:1531-1562`, [`writer`]);
//! * `write_discovery` (`:6703-6729`, [`discovery`]);
//! * the token of `start_listener_once` (`:6734-6742`, [`token`]).
//!
//! The queue is [`crate::queue`] (already ported): this lane enqueues requests and the tick
//! drains them. The `control.*` methods other than `hello` are **not** built here; the listener
//! asks a caller-supplied [`ControlHandler`] for their replies, so this lane is testable with a
//! stub and WP2.C's `control.rs` plugs in at integration. `hello` itself (the envelope, the
//! protocol and token checks, the synchronous send) is here, because it is the authentication
//! gate; the handler supplies only the capabilities object, which belongs to the method table.
//!
//! Nothing in the game calls [`start`] directly; [`crate::wiring`]'s tick starts it once
//! (`start_listener_once`, `mcp_bridge.c:6730-6779`) and its teardown stops it, and
//! [`crate::wiring::BridgeState`] is the [`ControlHandler`] and [`MethodRouter`] production
//! supplies.
//!
//! # Behaviours shared with the C on purpose
//!
//! These are not accidents of the port; the C does the same, and changing either is a
//! protocol decision, not a bug fix.
//!
//! * **A half-closed client loses its queued replies.** The C's `recv` returning 0 (`:6409`)
//!   is the disconnect path: it purges the connection's generation (`q_purge_gen`,
//!   `:6409-6411`), clears `g_authedClient` and closes the socket. A client that sends a
//!   request and then `shutdown(SHUT_WR)`s therefore loses that request's reply even though
//!   the reply could still be written - the request is purged before the writer can reach it,
//!   and the socket is closed on top. Replies this lane sends synchronously (the hello, the
//!   pre-auth errors, `control.shutdown`) are written while the frame is processed, before
//!   the next `recv`, so they still arrive; only what goes through the queue is lost. The
//!   lane here does the same on `Ok(0)`: the connection ends and
//!   [`crate::queue::Queues::purge_gen`] runs. A client that wants its answers must keep its
//!   read side open until it has read them.
//! * **The single connection slot starves reconnects.** One connection is served at a time:
//!   the C's `client != INVALID_SOCKET || g_authedClient != INVALID_SOCKET` (`:6385`) answers
//!   every extra connection `-32007 Busy` and closes it, and the slot is released only when
//!   the *served* connection's own `recv` fails or its hello deadline expires (`:6678-6683`,
//!   `5 s`). A connected but idle host therefore excludes every other host for as long as it
//!   holds its socket - there is no idle timeout, no eviction and no reconnect takeover, and
//!   this lane reproduces that exactly. A host that lost its socket and cannot open a new one
//!   is waiting for the old one to be reaped, not looking at a port that is down.
//!
//! # Differences from the C, and why
//!
//! * **One accept thread plus one thread per connection**, where the C runs one `select` loop
//!   over the listener and the served socket. `std` has no portable `select`/`poll`; a
//!   per-connection thread with a 100 ms read timeout reproduces the C's 100 ms tick: on every
//!   timeout the connection runs [`crate::queue::Queues::sweep_expired`] and checks the hello
//!   deadline, exactly as the C does at the bottom of each `select` pass (`:6675-6689`). The
//!   listener polls [`std::net::TcpListener::accept`] every 100 ms instead of selecting, so an
//!   extra connection's `Busy` is delayed by at most one tick.
//! * **The expired-request sweep runs while a connection exists.** The C sweeps once per
//!   `select` pass, even with no client connected (`:6675`); this lane sweeps on the connection
//!   thread's tick. The difference is unobservable: a connection's close purges its generation
//!   (`:6409-6411`), so with no connection the queue holds nothing to expire.
//! * **An accepted socket is put back in blocking mode.** Winsock gives a socket from `accept`
//!   the listening socket's properties, including the non-blocking mode the accept poll sets
//!   (`listener.rs`); Linux does not. The connection is explicitly made blocking, like the C's
//!   (`recv`/`send` block, `:6409`, `:1533`), so the 100 ms tick is the read timeout, not a
//!   spin. The read timeout is `SO_RCVTIMEO`: on Windows a timed-out receive leaves the socket
//!   usable but its state is documented as indeterminate for further receives, which is the one
//!   place this design is weaker than the C's `select` (that loop would be the alternative;
//!   `std` has no portable `poll`).
//! * **One write lock per connection.** The C's `ipc_thread` and `writer_thread` both call
//!   `send_all` on the same socket with nothing serialising them (`:1546`, `:6452`), so two
//!   replies can interleave mid-line; every write here goes through the connection's write
//!   lock, which keeps the C's ordering without the interleaving.
//! * **The generation counter is process-wide** (`NEXT_GEN`), as the C's static `g_nextGen`
//!   (`:1023`) is: the queue outlives a listener, so a restarted listener must not reuse a
//!   generation whose records are still in the queue.
//! * **`stop` shuts the live socket down before joining and deletes the discovery file**, both
//!   from the C's `Shutdown` (`:6791-6799`), so a client that stopped reading cannot block the
//!   writer past the teardown and no host is left pointed at a dead port. Every accepted
//!   stream also carries a 2 s write timeout and the accept/connection/writer joins share one
//!   3 s deadline - the C's `WaitForSingleObject(..., 2000)` (`:6796-6797`), which exists
//!   because `shutdown(SD_BOTH)` does not always wake a pending `send` under Wine; a thread
//!   that misses the deadline is detached with a log line, never waited on forever.
//! * **`start` refuses while the process queue is shut down** (the C's `g_shuttingDown` test,
//!   `:6731`): the writer would exit on its first empty pop and the listener would answer
//!   nothing.
//! * **An insecure discovery directory is fatal, not logged.** The C creates the directory and
//!   writes the token file wherever it is told, logging and serving on when the write fails
//!   (`:6364`); this lane refuses to write the session token into an *existing* directory that
//!   another uid owns or that grants group/other access, and [`start`] then refuses to serve
//!   (`discovery::InsecureDirectory`). The token is a capability, and the `$HOME/.local/share`
//!   fallback is a shared tree - the check is the price of writing a capability there. The
//!   temporary file gets the same treatment on Unix: an existing `session.json.tmp` is unlinked
//!   (a symlink goes, its target stays) and the new one is created `O_EXCL|O_NOFOLLOW`, `0600`.
//! * **The method table is a caller-supplied [`MethodRouter`].** The C's
//!   `find_method`/`blockedOn`/`needs` checks (`:6603-6623`) run on the IPC thread before a
//!   request takes a queue slot; [`crate::wiring::BridgeState`] supplies that decision from
//!   [`crate::dispatch`]. This lane keeps its `control.*` fast path (`:6466-6601`) and asks the
//!   router only for the remaining names. A [`Config`] built with [`Config::new`] queues every
//!   method ([`QueueEverything`]); the production start in [`crate::wiring`] never does.
//! * **Reply text is a Rust `String`.** The C copies the frame's `id` bytes into `idbuf` and
//!   re-escapes them raw; a `String` cannot carry invalid UTF-8, so a non-UTF-8 id is echoed
//!   with U+FFFD. Every shipped client sends UTF-8 (JSON strings are UTF-8 by RFC 8259).
//! * **`startedAt` is supplied by the caller.** The C writes `GetTickCount64()` at start
//!   (`:6733`, `:6714`); [`discovery::Ticket`] keeps the value a parameter so the discovery
//!   bytes are reproducible in a test.

pub mod discovery;
pub mod envelope;
pub mod listener;
pub mod token;
pub mod writer;

pub use listener::{
    start, Config, ControlCall, ControlHandler, ControlReply, Listener, MethodRouter,
    QueueEverything, Route,
};

/// The maximum frame length, newline excluded (`mcp_bridge.c:124`,
/// `#define MAX_LINE (1024u * 1024u)`), advertised to clients as `limits.maxLineBytes`.
pub const MAX_LINE: usize = 1024 * 1024;

/// The queue deadline a request without `deadlineMs` gets (`mcp_bridge.c:129`,
/// `#define DEFAULT_DEADLINE_MS 30000u`; assigned at `:6651`).
pub const DEFAULT_DEADLINE_MS: u64 = 30_000;

/// How long a new connection has to send `control.hello` (`mcp_bridge.c:6394`,
/// `helloDeadline = now + 5000`).
pub const HELLO_DEADLINE: std::time::Duration = std::time::Duration::from_millis(5000);

/// How often the connection loop wakes with nothing to read: the C's `select` timeout
/// (`mcp_bridge.c:6392-6393`, 100 ms), which also paces the expired-request sweep and the
/// hello deadline check.
pub(crate) const TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// The recv buffer size (`mcp_bridge.c:6388`, `static char cbuf[4096]`).
pub(crate) const RECV_BUF: usize = 4096;

/// The request id buffer (`mcp_bridge.c:6419`, `char idbuf[128]`).
pub(crate) const ID_BUF: usize = 128;

/// The method buffer (`mcp_bridge.c:6420`, `char method[64]`).
pub(crate) const METHOD_BUF: usize = 64;

/// The token buffer of the hello frame (`mcp_bridge.c:6421`, `char tokbuf[128]`).
pub(crate) const TOKEN_BUF: usize = 128;

/// The reply text for a frame that is not a JSON-RPC request at all (`:6418-6428`).
pub(crate) const PARSE_ERROR: i32 = -32700;

/// The reply text for a method this lane does not know (`:6600`, `:6607`).
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;

/// The reply code of a full queue (`:6665`) and of an extra connection (`:6388`).
pub(crate) const BUSY: i32 = -32007;
