//! `cs2_bridge::ipc` against the C it ports, over a real `127.0.0.1` socket.
//!
//! Every test starts its own listener with an ephemeral port and its own discovery file under
//! `CARGO_TARGET_TMPDIR`, authenticates with the token the listener reports, and drives the
//! process-wide queues the way the game does: a stub "main thread" drains
//! [`queue::global`] while the test acts as the client. Each assertion cites the line of
//! `mcp_bridge.c` it holds to.
//!
//! The tests share the process-wide queues, so they take a mutex and start from an empty
//! outbound queue; each listener's close purges its generation, which is also what the corpus
//! asserts.

use std::io::{BufRead as _, BufReader, ErrorKind, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cs2_bridge::generated::pins::PIN_GAME_BUILD;
use cs2_bridge::ipc::{self, Config, ControlCall, ControlHandler, ControlReply, Listener};
use cs2_bridge::queue::{self, Request};

/// The build string every envelope carries (`mcp_bridge.c:127`).
const BUILD: &str = cs2_bridge::BRIDGE_BUILD;

/// The stub handler's capabilities object (`build_caps` at `:6447`, stubbed: the method table
/// is WP2.C's).
const CAPS: &str = r#"{"methods":["pet.text"]}"#;

/// The token the fixed RNG below produces: `0x5a` repeated 32 times, as 64 hex characters
/// (`mcp_bridge.c:6739-6741`).
const FIXED_TOKEN: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// Serialises the tests: they share [`queue::global`] and the writer's outbound queue.
static SERIAL: Mutex<()> = Mutex::new(());

/// The serial guard.
///
/// It does **not** drain the outbound queue: generations are process-wide (the C's static
/// `g_nextGen`, `mcp_bridge.c:1023`), so a record a stopped listener left behind belongs to a
/// generation no later listener hands out and the next test's client can never receive it.
struct SerialGuard {
    _guard: MutexGuard<'static, ()>,
}

fn serial() -> SerialGuard {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    SerialGuard { _guard: guard }
}

/// The stub `control.*` handler: capabilities, `control.cancel` both ways, `control.shutdown`,
/// and `control.ping`; every other `control.*` is unknown (`None` -> `-32601`).
struct Stub;

impl ControlHandler for Stub {
    fn capabilities(&self) -> String {
        CAPS.to_string()
    }

    fn control(&self, call: ControlCall<'_>) -> Option<ControlReply> {
        match call {
            // `control.cancel` result/error shapes, `mcp_bridge.c:6577-6588`.
            ControlCall::Cancel { removed: true, .. } => {
                Some(ControlReply::Result(r#"{"cancelled":true}"#.to_string()))
            }
            ControlCall::Cancel { removed: false, .. } => Some(ControlReply::Error {
                code: ipc::listener::NOT_CANCELLABLE,
                message: "NotCancellable".to_string(),
                data: Some(r#"{"reason":"not queued or already running"}"#.to_string()),
            }),
            // `control.shutdown` result, `:6592`.
            ControlCall::Shutdown => Some(ControlReply::Result(r#"{"closing":true}"#.to_string())),
            // A `control.*` method this stub knows; the C's real arm is WP2.C's.
            ControlCall::Other {
                method: "control.ping",
                ..
            } => Some(ControlReply::Result(r#"{"ok":true}"#.to_string())),
            ControlCall::Other { .. } => None,
        }
    }
}

/// The stub executor: drains [`queue::global`] on its own thread, the way slot 13 runs on the
/// main thread (`mcp_bridge.c:6289-6317`), and records what it executed.
struct MainThread {
    stop: Arc<AtomicBool>,
    executed: Arc<Mutex<Vec<Request>>>,
    handle: Option<JoinHandle<()>>,
}

impl MainThread {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = Arc::clone(&stop);
        let thread_executed = Arc::clone(&executed);
        let handle = std::thread::spawn(move || {
            let mut exec = |request: &Request| {
                thread_executed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request.clone());
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":\"{}\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"executed\":\"{}\"}}}}\n",
                    request.id, request.method
                )
            };
            while !thread_stop.load(Ordering::Acquire) {
                queue::global().drain(&mut exec);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            stop,
            executed,
            handle: Some(handle),
        }
    }

    fn executed(&self) -> Vec<Request> {
        self.executed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for MainThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A "main thread" whose replies are 1 MiB each (`filler`), so a client that stops reading
/// leaves the writer blocked in `write_all` after a few records; `ran` counts the requests it
/// executed, which is the signal a test waits on instead of a sleep.
struct BigReplies {
    stop: Arc<AtomicBool>,
    ran: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl BigReplies {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicUsize::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_ran = Arc::clone(&ran);
        let handle = std::thread::spawn(move || {
            let filler = "x".repeat(1024 * 1024);
            let mut exec = |request: &Request| {
                thread_ran.fetch_add(1, Ordering::Relaxed);
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":\"{}\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"filler\":\"{filler}\"}}}}\n",
                    request.id
                )
            };
            while !thread_stop.load(Ordering::Acquire) {
                queue::global().drain(&mut exec);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            stop,
            ran,
            handle: Some(handle),
        }
    }

    fn ran(&self) -> usize {
        self.ran.load(Ordering::Relaxed)
    }
}

impl Drop for BigReplies {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Sends the 64 requests [`BigReplies`] is sized for, each with no deadline (`deadlineMs: 0`,
/// `mcp_bridge.c:6657`) so none expires while the writer is stuck.
fn send_big_requests(client: &mut Client) {
    for n in 0..64 {
        client.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"b{n}","method":"pet.text","params":{{"deadlineMs":0}}}}"#
        ));
    }
}

/// Empties the process-wide outbound queue: replies of a generation no later listener hands
/// out would still occupy its capacity.
fn drain_outbound() {
    while queue::global()
        .out_pop_blocking(Duration::ZERO)
        .is_some()
    {}
}

/// One test's discovery path under the per-binary tmpdir.
fn discovery_path(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ipc-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("session.json")
}

/// Starts a listener with the fixed token, pid 4242 and startedAt 987654 (`:6706`, `:6714`),
/// a 5000 ms hello deadline like the C (`:6394`), and the platform RNG seam replaced by a fixed
/// draw (the real one is exercised by `ipc::token`'s unit test).
fn start(name: &str, hello_deadline: Duration) -> Listener {
    ipc::start(Config {
        handler: Arc::new(Stub),
        router: Arc::new(ipc::QueueEverything),
        discovery_path: Some(discovery_path(name)),
        hello_deadline,
        rng: Box::new(|raw| {
            *raw = [0x5a; ipc::token::RAW_BYTES];
            true
        }),
        pid: 4242,
        started_at_ms: 987_654,
    })
    .expect("bind the loopback listener")
    .expect("the injected RNG succeeds")
}

/// A connected client: a writer and a buffered reader over the same socket.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(listener: &Listener) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", listener.port())).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        Self {
            reader: BufReader::new(stream.try_clone().expect("clone the socket")),
            writer: stream,
        }
    }

    fn send(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).expect("write");
        self.writer.write_all(b"\n").expect("write newline");
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write bytes");
    }

    /// Reads one line, failing the test on EOF or a timeout.
    fn line(&mut self) -> String {
        let mut line = String::new();
        let count = self.reader.read_line(&mut line).expect("read a line");
        assert!(count > 0, "the bridge closed before a reply arrived");
        line
    }

    /// Asserts the bridge closed the socket (`closesocket(client)` / `shutdown(SD_BOTH)`).
    fn expect_closed(&mut self) {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => {}
            // A reset is the other way a closed socket surfaces; nothing was read.
            Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
            Ok(count) => panic!("expected EOF, read {count} bytes: {line:?}"),
            Err(error) => panic!("expected EOF, got {error}"),
        }
    }
}

fn hello(_listener: &Listener, id: &str, token: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":"{id}","method":"control.hello","params":{{"protocol":1,"token":"{token}"}}}}"#
    )
}

/// Sends the hello frame and asserts the C's reply (`mcp_bridge.c:6444-6451`), id echoed.
fn authenticate(client: &mut Client, listener: &Listener, id: &str) {
    client.send(&hello(listener, id, listener.token()));
    let expected = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
         \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BUILD}\",\"gameBuild\":\"{PIN_GAME_BUILD}\",\
         \"capabilities\":{CAPS}}}}}\n"
    );
    assert_eq!(client.line(), expected, "the C's hello reply shape");
}

/// Waits for `condition` with a deadline, then fails naming it.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// Gate 1: hello with the right token, then a queued method round-trips through the global
/// drain and the writer, ids echoed, the balanced params span intact.
#[test]
fn hello_then_a_queued_request_round_trips() {
    scratch_log();
    let _serial = serial();
    let listener = start("round-trip", ipc::HELLO_DEADLINE);
    let main_thread = MainThread::start();
    let mut client = Client::connect(&listener);

    authenticate(&mut client, &listener, "h1");

    // The params carry a brace inside a string (the C's string-aware span, `:6631-6643`).
    client.send(
        r#"{"jsonrpc":"2.0","id":"r1","method":"pet.text","params":{"text":"a}b","deadlineMs":5000}}"#,
    );
    let reply = client.line();
    assert_eq!(
        reply,
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"executed\":\"pet.text\"}}}}\n"
        ),
        "the id is echoed and the writer delivered the executor's line"
    );

    let executed = main_thread.executed();
    assert_eq!(executed.len(), 1, "the request ran exactly once");
    assert_eq!(executed[0].id, "r1");
    assert_eq!(executed[0].method, "pet.text");
    assert_eq!(
        executed[0].params.as_deref(),
        Some(r#"{"text":"a}b","deadlineMs":5000}"#),
        "the params span is the balanced object, brace-in-string included"
    );
    assert_eq!(
        executed[0].deadline,
        Some(Duration::from_millis(5000)),
        "an explicit deadlineMs is the queue deadline (`:6656`)"
    );
    // Generations are process-wide (`g_nextGen`, `:1023`), so this binary's earlier tests have
    // consumed the low numbers; the value is the counter's, never zero.
    assert!(executed[0].gen >= 1, "every connection has a generation");

    // An explicit `deadlineMs: 0` is the C's "no deadline" (`:6657`).
    client.send(r#"{"jsonrpc":"2.0","id":"r2","method":"pet.text","params":{"deadlineMs":0}}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"r2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"executed\":\"pet.text\"}}}}\n"
        )
    );
    wait_for("the second request to run", || {
        main_thread.executed().len() == 2
    });
    assert_eq!(
        main_thread.executed()[1].deadline,
        None,
        "deadlineMs 0 means no deadline (`:6657`)"
    );
}

/// Gate 1/5: an explicit small `deadlineMs` is answered `-32008 QueueTimeout` by the IPC
/// thread's sweep when the main thread does not run (`mcp_bridge.c:1587-1608`, `:6675`).
#[test]
fn a_queued_deadline_is_answered_with_queue_timeout() {
    scratch_log();
    let _serial = serial();
    let listener = start("timeout", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":"t1","method":"pet.text","params":{"deadlineMs":30}}"#);
    // No main thread runs: only `sweep_expired` can answer this.
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"t1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32008,\"message\":\"QueueTimeout\"}}}}\n"
        )
    );
}

/// Gate 1: `control.cancel` removes the queued id before the handler answers
/// (`mcp_bridge.c:6568-6590`); the cancelled request never executes.
#[test]
fn control_cancel_removes_the_queued_id() {
    scratch_log();
    let _serial = serial();
    let listener = start("cancel", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":"q1","method":"pet.text","params":{"deadlineMs":30000}}"#);
    wait_for("the request to be queued", || queue::global().depth() == 1);

    client.send(r#"{"jsonrpc":"2.0","id":"c1","method":"control.cancel","params":{"id":"q1"}}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"c1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"cancelled\":true}}}}\n"
        )
    );
    assert_eq!(queue::global().depth(), 0, "the queued request is gone");
    assert!(
        !queue::global().remove_id("q1"),
        "there is nothing left to remove"
    );

    // The C answers -32010 when the id is no longer queued (`:6584-6588`).
    client.send(r#"{"jsonrpc":"2.0","id":"c2","method":"control.cancel","params":{"id":"q1"}}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"c2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32010,\"message\":\"NotCancellable\",\"data\":{{\"reason\":\"not queued or already running\"}}}}}}\n"
        )
    );
    // The queue is empty and the id is gone, which is what "the cancelled request never runs"
    // means; the executor is not started here, so there is nothing to wake and nothing to
    // sleep for.
}

/// Gate 1: closing the connection purges its generation (`q_purge_gen`, `:6409-6411`), so a
/// queued request of the dead connection never runs.
#[test]
fn close_purges_the_generation() {
    scratch_log();
    let _serial = serial();
    let listener = start("purge", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":"p1","method":"pet.text","params":{"deadlineMs":30000}}"#);
    wait_for("the request to be queued", || queue::global().depth() == 1);

    drop(client);
    wait_for("the generation to be purged", || {
        queue::global().depth() == 0
    });
    // The purge is the assertion: with the request gone from the queue the executor has
    // nothing to run, so no executor and no settling sleep is needed here.
}

/// Gate 2: the discovery file is the C's exact field set and format (`mcp_bridge.c:6703-6729`),
/// written through `session.json.tmp` and renamed (no temporary file survives).
#[test]
fn discovery_is_the_c_bytes_and_atomic() {
    scratch_log();
    let _serial = serial();
    let mut listener = start("discovery", ipc::HELLO_DEADLINE);
    let path = listener
        .discovery_path()
        .expect("discovery enabled")
        .to_path_buf();
    assert_eq!(listener.token(), FIXED_TOKEN);

    // Derived from the C's `sb_add` sequence at `:6705-6717`, not from the writer's own render.
    let expected = format!(
        "{{\n  \"pid\": 4242,\n  \"protocol\": 1,\n  \"bridgeVersion\": \"{BUILD}\",\n  \
         \"gameBuild\": \"{PIN_GAME_BUILD}\",\n  \"transport\": \"tcp\",\n  \"host\": \"127.0.0.1\",\n  \
         \"port\": {port},\n  \"pipeName\": null,\n  \"token\": \"{token}\",\n  \
         \"startedAt\": 987654\n}}\n",
        port = listener.port(),
        token = FIXED_TOKEN,
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the discovery file"),
        expected
    );
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    assert!(!tmp.exists(), "the temporary file was renamed away");

    // The file carries the session token, a capability: it is private to the user (0600).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the token file is not readable by other users");
    }

    // The C's `Shutdown` deletes the file (`:6799`), so no host stays pointed at a dead port.
    listener.stop();
    assert!(!path.exists(), "stop removes the discovery file");
}

/// Gate 3 - NEGATIVE: the wrong token draws the C's `-32001 NotAuthenticated` and the bridge
/// closes the socket (`mcp_bridge.c:6439-6441`).
#[test]
fn wrong_token_is_refused_and_closed() {
    scratch_log();
    let _serial = serial();
    let listener = start("bad-token", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);

    client.send(&hello(&listener, "1", &"0".repeat(64)));
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32001,\"message\":\"NotAuthenticated\"}}}}\n"
        )
    );
    client.expect_closed();
}

/// Gate 3 - NEGATIVE: a token that is a prefix of the real one and a token longer than the
/// real 64 characters are both refused with the C's `-32001 NotAuthenticated` and a close
/// (`mcp_bridge.c:6439-6441`); the comparison is length-first, then every byte (`ct_eq`,
/// `:158-163`), so neither a truncation nor an extension can pass.
#[test]
fn a_prefix_or_overlong_token_is_refused_and_closed() {
    scratch_log();
    let _serial = serial();
    let listener = start("token-lengths", ipc::HELLO_DEADLINE);

    // `FIXED_TOKEN` is 64 characters, which is what the listener generated.
    assert_eq!(listener.token(), FIXED_TOKEN);
    let prefix = &FIXED_TOKEN[..FIXED_TOKEN.len() - 1];
    let overlong = format!("{FIXED_TOKEN}0");
    let far_overlong = "a".repeat(200);

    for token in [prefix, overlong.as_str(), far_overlong.as_str()] {
        let mut client = Client::connect(&listener);
        client.send(&hello(&listener, "1", token));
        assert_eq!(
            client.line(),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
                 \"error\":{{\"code\":-32001,\"message\":\"NotAuthenticated\"}}}}\n"
            ),
            "token of {} characters must be refused",
            token.len()
        );
        client.expect_closed();
    }
}

/// Gate 3 - NEGATIVE: the first frame must be `control.hello`; anything else draws the C's
/// `-32001 NotAuthenticated` and a close (`mcp_bridge.c:6401-6414`).
#[test]
fn first_frame_must_be_hello() {
    scratch_log();
    let _serial = serial();
    let listener = start("first-frame", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);

    client.send(r#"{"jsonrpc":"2.0","id":"1","method":"control.ping"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32001,\"message\":\"NotAuthenticated\"}}}}\n"
        )
    );
    client.expect_closed();
}

/// Gate 3/5 - NEGATIVE: a client that never sends hello is closed at the deadline
/// (`mcp_bridge.c:6678-6683`). The deadline is a parameter, so this takes ~250 ms, not 5 s.
#[test]
fn hello_deadline_closes_a_silent_client() {
    scratch_log();
    let _serial = serial();
    let listener = start("hello-deadline", Duration::from_millis(250));
    let mut client = Client::connect(&listener);

    let started = Instant::now();
    client.expect_closed();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "closed before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the configured deadline must not be the 5 s production default: {elapsed:?}"
    );
}

/// Gate 3 - NEGATIVE: a numeric `id` is not a string id, so the C answers `-32600
/// invalid request: id required` with `id: null` and keeps the connection (`:6457-6463`).
#[test]
fn numeric_id_is_an_invalid_request() {
    scratch_log();
    let _serial = serial();
    let listener = start("numeric-id", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":7,"method":"control.ping"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32600,\"message\":\"invalid request: id required\"}}}}\n"
        )
    );

    // The connection stays open: a well-formed request still works.
    client.send(r#"{"jsonrpc":"2.0","id":"ok","method":"control.ping"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"ok\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"ok\":true}}}}\n"
        )
    );
}

/// Gate 3 - NEGATIVE: a second client while one is authenticated draws `-32007 Busy` and is
/// closed, and the first connection keeps working (`mcp_bridge.c:6385-6393`).
#[test]
fn a_second_client_gets_busy() {
    scratch_log();
    let _serial = serial();
    let listener = start("busy", ipc::HELLO_DEADLINE);
    let mut first = Client::connect(&listener);
    authenticate(&mut first, &listener, "h1");

    let mut second = Client::connect(&listener);
    assert_eq!(
        second.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32007,\"message\":\"Busy\"}}}}\n"
        )
    );
    second.expect_closed();

    first.send(r#"{"jsonrpc":"2.0","id":"still","method":"control.ping"}"#);
    assert_eq!(
        first.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"still\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"ok\":true}}}}\n"
        ),
        "the authenticated connection was not disturbed"
    );
}

/// Gate 3 - NEGATIVE: a line over [`ipc::MAX_LINE`] draws the C's `-32600 line too long` and the
/// connection closes (`mcp_bridge.c:6647-6659`).
#[test]
fn an_overlong_line_gets_the_overflow_reply() {
    scratch_log();
    let _serial = serial();
    let listener = start("overflow", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    // A line of exactly `MAX_LINE - 1` bytes is accepted (the C stores while
    // `linelen + 1 < MAX_LINE`, `:6647`); it is not JSON-RPC, so the C's parse-error arm
    // answers it (`:6424-6430`) and the connection stays open.
    let mut boundary = vec![b'x'; ipc::MAX_LINE - 1];
    boundary.push(b'\n');
    client.send_bytes(&boundary);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32700,\"message\":\"parse error\"}}}}\n"
        ),
        "a line at the limit is served, not refused"
    );

    // One byte past the C's capacity: the store is refused once `linelen + 1 == MAX_LINE`.
    let mut overlong = vec![b'x'; ipc::MAX_LINE + 1];
    overlong.push(b'\n');
    client.send_bytes(&overlong);

    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32600,\"message\":\"line too long\"}}}}\n"
        )
    );
    client.expect_closed();
}

/// Gate 3 - NEGATIVE: when the RNG fails, the listener does not start and no discovery file is
/// written (the C's fail-closed path, `mcp_bridge.c:6736-6738`).
#[test]
fn rng_failure_starts_nothing() {
    scratch_log();
    let _serial = serial();
    let path = discovery_path("rng");
    let started = ipc::start(Config {
        handler: Arc::new(Stub),
        router: Arc::new(ipc::QueueEverything),
        discovery_path: Some(path.clone()),
        hello_deadline: ipc::HELLO_DEADLINE,
        rng: Box::new(|_| false),
        pid: 4242,
        started_at_ms: 987_654,
    })
    .expect("a closed RNG is not an I/O error, it is the fail-closed answer");

    assert!(started.is_none(), "no listener is created");
    assert!(!path.exists(), "no discovery file is written");
    assert!(
        !PathBuf::from(format!("{}.tmp", path.display())).exists(),
        "not even a temporary file is written"
    );
}

/// Gate 2 - NEGATIVE (Unix): an existing discovery directory that grants group or other
/// access is fail-closed - `start` serves nothing and writes no file - unlike the C's
/// log-and-serve for an ordinary write failure (`:6364`), because the token file is a
/// capability.
#[cfg(unix)]
#[test]
fn an_insecure_discovery_directory_refuses_to_start() {
    scratch_log();
    use std::os::unix::fs::PermissionsExt as _;

    let _serial = serial();
    let path = discovery_path("insecure-dir");
    let dir = path.parent().expect("the discovery path has a parent");
    std::fs::create_dir_all(dir).expect("create the discovery directory");
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .expect("widen it to 0755");

    let started = ipc::start(Config {
        handler: Arc::new(Stub),
        router: Arc::new(ipc::QueueEverything),
        discovery_path: Some(path.clone()),
        hello_deadline: ipc::HELLO_DEADLINE,
        rng: Box::new(|raw| {
            *raw = [0x5a; ipc::token::RAW_BYTES];
            true
        }),
        pid: 4242,
        started_at_ms: 987_654,
    })
    .expect("the refusal is not an I/O error");

    assert!(
        started.is_none(),
        "no listener is created on an insecure directory"
    );
    assert!(!path.exists(), "no discovery file is written");
    assert!(
        !PathBuf::from(format!("{}.tmp", path.display())).exists(),
        "not even a temporary file is written"
    );
}

/// Gate 3 - NEGATIVE: the second connection is refused while the first is still **pre-auth**,
/// because the C's test is `client != INVALID_SOCKET || g_authedClient != INVALID_SOCKET`
/// (`mcp_bridge.c:6385`), and the first connection is unaffected.
#[test]
fn a_second_client_during_the_pre_auth_window_gets_busy() {
    scratch_log();
    let _serial = serial();
    let listener = start("busy-pre-auth", ipc::HELLO_DEADLINE);
    let mut first = Client::connect(&listener);

    let mut second = Client::connect(&listener);
    assert_eq!(
        second.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32007,\"message\":\"Busy\"}}}}\n"
        )
    );
    second.expect_closed();

    // The first connection was never disturbed and can still authenticate.
    authenticate(&mut first, &listener, "h1");
}

/// Framing: the hello frame arrives one byte at a time; the line assembler must not act until
/// the newline (`mcp_bridge.c:6644-6660`).
#[test]
fn a_hello_split_across_writes_is_authenticated() {
    scratch_log();
    let _serial = serial();
    let listener = start("split-hello", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);

    for byte in hello(&listener, "h1", listener.token()).into_bytes() {
        client.send_bytes(&[byte]);
    }
    client.send_bytes(b"\n");
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"h1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BUILD}\",\"gameBuild\":\"{PIN_GAME_BUILD}\",\
             \"capabilities\":{CAPS}}}}}\n"
        )
    );
}

/// Framing: hello and a request pipelined into one write are both served, in order
/// (`:6408-6700` walks the whole chunk).
#[test]
fn hello_and_a_request_in_one_write_are_both_served() {
    scratch_log();
    let _serial = serial();
    let listener = start("pipelined", ipc::HELLO_DEADLINE);
    let main_thread = MainThread::start();
    let mut client = Client::connect(&listener);

    let mut both = hello(&listener, "h1", listener.token());
    both.push('\n');
    both.push_str(
        r#"{"jsonrpc":"2.0","id":"r1","method":"pet.text","params":{"deadlineMs":2000}}"#,
    );
    both.push('\n');
    client.send_bytes(both.as_bytes());

    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"h1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BUILD}\",\"gameBuild\":\"{PIN_GAME_BUILD}\",\
             \"capabilities\":{CAPS}}}}}\n"
        )
    );
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"executed\":\"pet.text\"}}}}\n"
        )
    );
    assert_eq!(main_thread.executed().len(), 1);
}

/// Gate 3 - NEGATIVE: a hello with `protocol != 1` draws `-32002 VersionMismatch` and a close
/// (`mcp_bridge.c:6429-6435`). A hello with no `protocol` at all is the same arm (`proto = 0`).
#[test]
fn a_version_mismatch_is_refused() {
    scratch_log();
    let _serial = serial();
    let listener = start("version", ipc::HELLO_DEADLINE);

    for frame in [
        format!(
            r#"{{"jsonrpc":"2.0","id":"1","method":"control.hello","params":{{"protocol":2,"token":"{}"}}}}"#,
            listener.token()
        ),
        r#"{"jsonrpc":"2.0","id":"1","method":"control.hello","params":{}}"#.to_string(),
    ] {
        let mut client = Client::connect(&listener);
        client.send(&frame);
        assert_eq!(
            client.line(),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
                 \"error\":{{\"code\":-32002,\"message\":\"VersionMismatch\"}}}}\n"
            )
        );
        client.expect_closed();
    }
}

/// Gate 3 - NEGATIVE: a line that is not a JSON-RPC request at all is answered `-32700` and,
/// before authentication, the connection closes (`mcp_bridge.c:6424-6430`).
#[test]
fn a_pre_auth_parse_error_closes() {
    scratch_log();
    let _serial = serial();
    let listener = start("pre-auth-parse", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);

    client.send("not json at all");
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32700,\"message\":\"parse error\"}}}}\n"
        )
    );
    client.expect_closed();
}

/// Gate 3: a line whose first byte is NUL is skipped, not parsed - the C's
/// `if (!line[0]) continue;` (`mcp_bridge.c:6431`). Pre-auth that line would otherwise be a
/// parse error and close (`:6424-6430`); the connection stays open and the hello authenticates.
#[test]
fn a_nul_led_line_is_skipped() {
    scratch_log();
    let _serial = serial();
    let listener = start("nul-led", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);

    client.send_bytes(b"\0{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"control.hello\"}\n");
    authenticate(&mut client, &listener, "h1");
}

/// Gate 3 - NEGATIVE: after authentication an id-less request is `-32600 invalid request: id
/// required` with `id: null`, and the connection stays open (`mcp_bridge.c:6457-6463`).
#[test]
fn an_id_less_request_is_refused() {
    scratch_log();
    let _serial = serial();
    let listener = start("no-id", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","method":"control.ping"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32600,\"message\":\"invalid request: id required\"}}}}\n"
        )
    );

    client.send(r#"{"jsonrpc":"2.0","id":"ok","method":"control.ping"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"ok\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"result\":{{\"ok\":true}}}}\n"
        ),
        "the connection is still serving"
    );
}

/// Gate 3 - NEGATIVE: `deadlineMs < 0` is `-32602` and is never queued (`mcp_bridge.c:6654-6661`).
#[test]
fn a_negative_deadline_is_refused() {
    scratch_log();
    let _serial = serial();
    let listener = start("negative-deadline", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":"n1","method":"pet.text","params":{"deadlineMs":-1}}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"n1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32602,\"message\":\"invalid params: deadlineMs < 0\"}}}}\n"
        )
    );
    assert_eq!(
        queue::global().depth(),
        0,
        "a refused request is not queued"
    );
}

/// Gate 3 - NEGATIVE: an unknown `control.*` method is the handler's `-32601 MethodNotFound`
/// (`mcp_bridge.c:6599-6601`), and a second `control.hello` is refused by the lane itself, as
/// the C's fallthrough does.
#[test]
fn unknown_control_methods_are_method_not_found() {
    scratch_log();
    let _serial = serial();
    let listener = start("unknown-control", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    client.send(r#"{"jsonrpc":"2.0","id":"u1","method":"control.teleport"}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"u1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32601,\"message\":\"MethodNotFound\"}}}}\n"
        )
    );

    client.send(&hello(&listener, "u2", listener.token()));
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"u2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32601,\"message\":\"MethodNotFound\"}}}}\n"
        ),
        "a second hello is not a second authentication"
    );
}

/// Gate 1: a `control.cancel` with no `params`, or with a `params` that names nothing, is the
/// C's `-32010 NotCancellable` (`mcp_bridge.c:6569-6588`).
#[test]
fn cancel_without_a_queued_id_is_not_cancellable() {
    scratch_log();
    let _serial = serial();
    let listener = start("cancel-edges", ipc::HELLO_DEADLINE);
    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");

    let expected = |id: &str| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
             \"error\":{{\"code\":-32010,\"message\":\"NotCancellable\",\
             \"data\":{{\"reason\":\"not queued or already running\"}}}}}}\n"
        )
    };
    client.send(r#"{"jsonrpc":"2.0","id":"c1","method":"control.cancel"}"#);
    assert_eq!(client.line(), expected("c1"));
    client.send(r#"{"jsonrpc":"2.0","id":"c2","method":"control.cancel","params":{}}"#);
    assert_eq!(client.line(), expected("c2"));
    client.send(r#"{"jsonrpc":"2.0","id":"c3","method":"control.cancel","params":{"id":"never"}}"#);
    assert_eq!(client.line(), expected("c3"));
}

/// Gate for the stop path: a client that authenticates, sends requests whose replies fill the
/// socket and then never reads leaves the writer blocked in `send_all`. The C bounds its
/// teardown joins with `WaitForSingleObject(..., 2000)` (`mcp_bridge.c:6796-6797`) because
/// `shutdown` does not always wake a pending send under Wine; `stop()` must be bounded too
/// (the stream's write timeout plus the shared stop deadline), never wait forever.
#[test]
fn stop_returns_while_a_non_reading_client_blocks_the_writer() {
    scratch_log();
    let _serial = serial();
    let mut listener = start("stop-blocked-write", ipc::HELLO_DEADLINE);
    let big = BigReplies::start();

    let mut client = Client::connect(&listener);
    authenticate(&mut client, &listener, "h1");
    send_big_requests(&mut client);
    // Every request has run, so 64 MiB of replies are owed to a client that has read nothing:
    // far more than any socket buffer, which leaves the writer blocked in `write_all` with the
    // rest of the replies queued behind it.
    wait_for("every request to be executed", || big.ran() == 64);

    let started = Instant::now();
    listener.stop();
    let elapsed = started.elapsed();

    drop(big);
    drain_outbound();

    assert!(
        elapsed < Duration::from_secs(5),
        "stop() must be bounded like the C's joins (`:6796-6797`): {elapsed:?}"
    );
}

/// Gate for the write timeout: when `write_all` fails on a client that stopped reading, the
/// connection is closed instead of leaving the socket "open" with half a line written and the
/// next record spliced onto it. The connection thread then reads EOF, purges the generation,
/// frees the single slot - so a second client, which gets `-32007 Busy` while the first one is
/// still there, authenticates once the timed-out send has taken the first one down.
#[test]
fn a_timed_out_send_closes_the_connection() {
    scratch_log();
    let _serial = serial();
    let listener = start("write-timeout-close", ipc::HELLO_DEADLINE);
    let big = BigReplies::start();

    let mut first = Client::connect(&listener);
    authenticate(&mut first, &listener, "h1");
    send_big_requests(&mut first);
    wait_for("every request to be executed", || big.ran() == 64);
    // The first client never reads again; its writer is now blocked in `write_all`, which
    // fails at the 2 s write timeout (`WRITE_TIMEOUT`).

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut second = Client::connect(&listener);
        second.send(&hello(&listener, "h2", listener.token()));
        let line = second.line();
        if line.contains("\"result\"") {
            assert_eq!(
                line,
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":\"h2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
                     \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BUILD}\",\"gameBuild\":\"{PIN_GAME_BUILD}\",\
                     \"capabilities\":{CAPS}}}}}\n"
                ),
                "the freed slot authenticates the second client"
            );
            break;
        }
        assert!(
            line.contains("-32007"),
            "only Busy or a hello reply is possible here: {line:?}"
        );
        assert!(
            Instant::now() < deadline,
            "the timed-out connection never went away; a second client kept getting Busy"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(big);
    drain_outbound();
}

/// Point the bridge log at `target/scratch/` for this test binary, and answer the path.
///
/// An integration test links the library **without** `cfg(test)`, so `log::line`'s default
/// path would be the user's real `$XDG_RUNTIME_DIR/cs2mcpbridge/bridge.log` (`src/log.rs`). A
/// `cargo test` must never append to that file, so every case that can log points
/// `CS2_BRIDGE_LOG` here first. Process-wide and idempotent: the first call sets the variable
/// and every later call answers the same path (a case that logs to its own file restores it
/// with this).
fn scratch_log() -> std::path::PathBuf {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
        std::fs::create_dir_all(&dir).expect("target/scratch is writable");
        let path = dir.join("cs2-bridge-ipc-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
