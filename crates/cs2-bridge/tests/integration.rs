//! WP2.D's acceptance gate: the real tick body, the real listener, the real handler, over a
//! real loopback socket.
//!
//! One test binary and one test, because the pieces under test are process-wide on purpose: the
//! listener is started once per process (the C's `g_started`, `mcp_bridge.c:6731`), the queue's
//! shutdown is permanent (the C's `g_shuttingDown`, `:1018`), and `session.json`'s path comes
//! from the process environment. The single test therefore walks the gates in the order the
//! brief lists them: negative before `Init`, the happy path, the negatives that need a live
//! listener, then the teardown and the negative after it.
//!
//! Every expected reply is the byte string a [`cs2_bridge::control`] builder produces for the
//! same state - the builders WP2.C holds to the C output byte for byte
//! (`tests/control.rs`) - so the listener is checked against the C's bytes, not a shape.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::generated::pins;
use cs2_bridge::ipc::Route;
use cs2_bridge::queue::{self, Request};
use cs2_bridge::registry::{self, ModState, ModuleView};
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::wiring::{self, BridgeState};
use cs2_bridge::BRIDGE_BUILD;

/// The build string every envelope carries ([`cs2_bridge::BRIDGE_BUILD`], m3.N; its shape is
/// pinned in `tests/queue.rs`).
const BUILD: &str = cs2_bridge::BRIDGE_BUILD;

/// Waits for `condition` with a deadline, then fails naming it.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// The scratch directory for the discovery file, under the per-binary cargo tmpdir (never
/// `/tmp`, the repo rule).
fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("wiring-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
    dir
}

/// A test-only tick loop with a deadline-driven pause: it calls the same body the vtable's
/// slot 13 calls ([`wiring::tick`]), one call per pass, and parks between passes on request so
/// the test can observe what the tick would have drained.
struct Ticker {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    idle: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let idle = Arc::new(AtomicBool::new(true));
        let handle = {
            let stop = Arc::clone(&stop);
            let paused = Arc::clone(&paused);
            let idle = Arc::clone(&idle);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if paused.load(Ordering::Acquire) {
                        idle.store(true, Ordering::Release);
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    idle.store(false, Ordering::Release);
                    wiring::tick();
                    std::thread::sleep(Duration::from_millis(1));
                }
                idle.store(true, Ordering::Release);
            })
        };
        Self {
            stop,
            paused,
            idle,
            handle: Some(handle),
        }
    }

    /// Parks the loop and waits until no tick is in progress.
    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        wait_for("the tick loop to park", || self.idle.load(Ordering::Acquire));
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A module-state source a test constructs: every module in one state, revision fixed.
struct FakeModules {
    state: ModState,
    revision: u64,
}

impl ModuleViews for FakeModules {
    fn state(&self, _module: u8) -> ModState {
        self.state
    }

    fn is_verified(&self, _module: u8) -> bool {
        self.state == ModState::Verified
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

/// A connected client: a writer and a buffered reader over the same socket.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Self {
        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
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

    /// Writes one request and returns its reply line (newline included).
    fn request(&mut self, method: &str, id: &str, params: &str) -> String {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"{id}","method":"{method}","params":{params}}}"#
        ));
        self.line()
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        let count = self.reader.read_line(&mut line).expect("read a line");
        assert!(count > 0, "the bridge closed before a reply arrived");
        line
    }
}

/// The quoted string value of `key` in the discovery file (its shape is fixed:
/// `"key": "value"` or `"key": 123`).
fn quoted_field(text: &str, key: &str) -> String {
    let marker = format!("\"{key}\": \"");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("the discovery file has no quoted `{key}`"))
        + marker.len();
    let rest = &text[start..];
    let end = rest.find('"').expect("a closing quote");
    rest[..end].to_string()
}

/// The unsigned number value of `key` (`"key": 123`).
fn number_field(text: &str, key: &str) -> u64 {
    let marker = format!("\"{key}\":");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("the reply has no `{key}`"))
        + marker.len();
    let digits: String = text[start..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("`{key}` is not a number: {:?}", &text[start..start + 32]))
}

/// The `modules` object of a `control.ping` reply, for the twelve generated module names.
fn modules_json(views: &[ModuleView]) -> String {
    let fields: Vec<String> = pins::PIN_MODULES
        .iter()
        .zip(views.iter())
        .map(|(module, view)| format!("\"{}\":\"{}\"", module.name, view.state.as_str()))
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// The first op row (`needs != 0`): the probe the capabilities filter drops. Since M4 every
/// implemented row has a body, so the "unported" case is built with a filter that leaves it out.
fn first_op() -> &'static str {
    registry::METHODS
        .iter()
        .find(|row| row.implemented && row.needs != 0)
        .map(|row| row.name)
        .expect("the C table has op rows")
}

/// The first row the C table itself blocks (`implemented == 0`, e.g. `doc.list`): the wire's
/// `-32011` probe now that every implemented row is served.
fn first_blocked_row() -> &'static registry::Method {
    registry::METHODS
        .iter()
        .find(|row| !row.implemented)
        .expect("the C table has blocked rows")
}

/// The `methods` array of a capabilities object.
fn methods_array(capabilities: &str) -> &str {
    let marker = "\"methods\":[";
    let start = capabilities
        .find(marker)
        .expect("a capabilities object carries methods")
        + marker.len();
    let rest = &capabilities[start..];
    let end = rest.find(']').expect("the methods array closes");
    &rest[..end]
}

#[test]
fn the_wire_answers_m3_0_and_tears_down_in_the_c_order() {
    assert_eq!(BUILD, BRIDGE_BUILD, "the test's literal and the crate's const agree");

    // ---- the scratch discovery path ($XDG_RUNTIME_DIR/cs2mcpbridge/session.json)
    let dir = scratch();
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    let discovery = dir.join("cs2mcpbridge").join("session.json");
    assert!(!discovery.exists());

    // ---- NEGATIVE (gate 3): a tick before IAppSystem::Init starts nothing.
    for _ in 0..3 {
        wiring::tick();
    }
    assert_eq!(wiring::port(), None, "no Init, no listener");
    assert!(!discovery.exists(), "no Init, no discovery file");

    // ---- Init, then the tick loop the game runs (the vtable's slot 13 body).
    wiring::init();
    let mut ticker = Ticker::start();
    wait_for("the listener to start", || wiring::port().is_some());
    let port = wiring::port().expect("the tick started the listener");
    wait_for("the discovery file", || discovery.exists());
    assert_eq!(
        wiring::discovery_path(),
        Some(discovery.clone()),
        "the listener wrote the platform default path"
    );

    let first = std::fs::read_to_string(&discovery).expect("read the discovery file");
    let first_mtime = std::fs::metadata(&discovery)
        .expect("stat")
        .modified()
        .expect("the file has an mtime");
    assert!(
        first.contains(&format!("\"port\": {port}")),
        "the file names the bound port"
    );
    assert_eq!(
        quoted_field(&first, "bridgeVersion"),
        BUILD,
        "the discovery file advertises m3.0 (`:6714`)"
    );

    // ---- NEGATIVE (gate 3): a second and third tick do not start a second listener.
    wiring::tick();
    wiring::tick();
    assert_eq!(wiring::port(), Some(port), "one listener, one port");
    assert_eq!(
        std::fs::read_to_string(&discovery).expect("read"),
        first,
        "no second discovery write"
    );
    assert_eq!(
        std::fs::metadata(&discovery)
            .expect("stat")
            .modified()
            .expect("the file has an mtime"),
        first_mtime,
        "the discovery file's write time did not move (a second start rewrites it)"
    );

    let token = quoted_field(&first, "token");
    assert_eq!(token.len(), 64, "the 32-byte token as 64 hex characters");
    let state = wiring::state();
    let views = state.views();
    let revision = state.revision();

    // ---- hello (`:6478-6486`), the auth gate; answers the capabilities object.
    let mut client = Client::connect(port);
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":"h1","method":"control.hello","params":{{"protocol":1,"token":"{token}"}}}}"#
    ));
    let hello = client.line();
    let capabilities = state.capabilities();
    let expected_hello = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":\"h1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\
         \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BUILD}\",\
         \"gameBuild\":\"{}\",\"capabilities\":{capabilities}}}}}\n",
        pins::PIN_GAME_BUILD
    );
    assert_eq!(hello, expected_hello, "the C's hello bytes");
    // ... and WP2.C's builder agrees for the same state, up to this build's own rows: the
    // Rust bridge advertises `registry::RUST_METHODS`' names (which the frozen C's
    // `build_caps` cannot know), appended after the C table's rows. Stripping EVERY such
    // name from the live hello leaves exactly `control::hello`'s bytes - and stays true
    // when a sibling work package adds a second Rust row.
    let mut hello_without_rust_rows = hello;
    // (an override that the C blocks and this build implements is advertised at its C position)
    for name in registry::RUST_METHODS.iter().chain(registry::RUST_OVERRIDES) {
        hello_without_rust_rows =
            hello_without_rust_rows.replace(&format!(",\"{}\"", name.name), "");
    }
    assert_eq!(
        hello_without_rust_rows,
        control::hello(Some("h1"), &views, revision),
        "the listener's hello is control::hello's bytes plus this build's own rows"
    );
    assert!(
        !methods_array(&capabilities).contains("control.hello"),
        "hello stays hidden (`:1195`)"
    );

    // ---- ping (`:6497-6529`): every field from live state, the static tail byte-exact.
    let ping = client.request("control.ping", "p1", "{}");
    assert!(
        ping.starts_with(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"p1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"ok\":true,\"uptimeMs\":"
        )),
        "the C's ping envelope: {ping:?}"
    );
    assert!(number_field(&ping, "uptimeMs") > 0, "the listener started before the call");
    assert!(
        ping.contains(&format!("\"modules\":{}", modules_json(&views))),
        "the modules object reports the live states: {ping:?}"
    );
    assert_eq!(number_field(&ping, "queueDepth"), 0);
    assert_eq!(number_field(&ping, "inflight"), 0);
    assert_eq!(number_field(&ping, "revision"), revision);
    assert!(
        ping.contains("\"drainBlocked\":false,\"inflight\":0,\"queueDepth\":0"),
        "the C's field order: {ping:?}"
    );
    assert!(
        ping.ends_with(&format!("\"revision\":{revision},\"shuttingDown\":false}}}}\n")),
        "the C's ping tail: {ping:?}"
    );

    // ---- status (`:6530-6553`) shows a queued request while the tick is parked.
    ticker.pause();
    queue::global()
        .push(Request {
            id: "q1".to_string(),
            method: "pet.apply".to_string(),
            params: None,
            received: Instant::now(),
            deadline: None,
            // No connection: the drain's reply is dropped by the writer (stale generation).
            gen: 999_999,
        })
        .expect("the queue has room");
    let status = client.request("control.status", "s1", "{}");
    assert!(
        status.starts_with(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"s1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"queue\":["
        )),
        "the C's status envelope: {status:?}"
    );
    assert!(
        status.contains("{\"id\":\"q1\",\"method\":\"pet.apply\",\"ageMs\":"),
        "the parked request is reported (`:6536-6539`): {status:?}"
    );
    assert!(number_field(&status, "ageMs") < 60_000, "a live age, not a wrapped one");
    assert!(
        status.ends_with(&format!(
            "],\"jobs\":[],\"listener\":true,\"drainBlocked\":false,\"inflight\":0,\"revision\":{revision},\"shuttingDown\":false}}}}\n"
        )),
        "the C's status tail: {status:?}"
    );
    ticker.resume();
    wait_for("the tick to drain the parked request", || {
        queue::global().depth() == 0
    });

    // ---- capabilities (`:6554-6558`) and version (`:6559-6578`): deterministic, byte-exact.
    // The wire's capabilities are the live builder's (`wiring::capabilities`): the C's 84 rows
    // byte-identical (the `tests/control.rs` differential pins those) plus this build's own
    // `metamod.plugins` row - which is the one delta `control::capabilities` (the C's bytes)
    // cannot carry, so the comparison is against the filtered builder.
    assert_eq!(
        client.request("control.capabilities", "c1", "{}"),
        control::capabilities_filtered(Some("c1"), &views, revision, &|method| {
            dispatch::is_ported(method.name)
        }),
        "the live builder's capabilities bytes (the C's plus this build's own row)"
    );
    assert_eq!(
        client.request("control.version", "v1", "{}"),
        control::version(Some("v1"), &views),
        "the C's version bytes"
    );

    // ---- a blocked row: -32011 + the row's blockedOn, without a queue slot.
    ticker.pause();
    wait_for("the tick loop to park", || queue::global().depth() == 0);
    let depth_before = queue::global().depth();
    let blocked = first_blocked_row();
    assert_eq!(
        client.request(blocked.name, "u1", r#"{"deadlineMs":5000}"#),
        control::not_implemented(Some("u1"), blocked.blocked_on.unwrap_or("unknown")),
        "the C's -32011 object with the row's own blockedOn"
    );
    assert_eq!(
        queue::global().depth(),
        depth_before,
        "a blocked method never enters the queue (the C's `out_push` arms, `:6611-6616`)"
    );

    // ---- an unknown method: -32601, also without a queue slot (`:6607-6610`).
    assert_eq!(
        client.request("no.such", "x1", "{}"),
        control::not_found(Some("x1")),
        "the C's MethodNotFound bytes"
    );
    assert_eq!(
        queue::global().depth(),
        depth_before,
        "an unknown method never enters the queue"
    );

    // ---- the -32017 arm on the main thread, through a fake module source: `pet.apply` needs
    // PET|TIER0 and nothing verifies, so the C's bits[] order names tier0.dll first
    // (`mcp_bridge.c:5676-5677`).
    let unverified = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::NotLoaded,
            revision: 3,
        }),
        // The row is "ported" here only to reach the needs check; m3.0's shipped list stops at
        // -32011 above. This is the M4 shape the gate needs to stay testable today.
        Box::new(|_: &str| true),
    );
    let request = Request {
        id: "q2".to_string(),
        method: "pet.apply".to_string(),
        params: None,
        received: Instant::now(),
        deadline: None,
        gen: 1,
    };
    assert_eq!(
        unverified.exec_request(&request),
        control::prerequisite_failed(Some("q2"), "tier0.dll", ModState::NotLoaded),
        "the C's -32017 object, module from the priority table"
    );
    assert_eq!(
        unverified.route("pet.apply"),
        Route::Queue,
        "the IPC lane defers the needs check to the main thread (`:6623`)"
    );

    // ---- capabilities lists exactly the ported+verified methods: with every module verified
    // and m3.0's list, only the visible control.* rows are advertised; with the same modules
    // and every row "ported", the op rows appear (so the filter, not the mask, is what excludes
    // them).
    let all_verified = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(dispatch::is_ported),
    );
    let shipped = all_verified.capabilities();
    // Every visible, implemented row this build serves (control.* + the ops bodies + this
    // build's own rows), in table order (`:1195-1196`) - computed so each group's registration
    // extends it by itself. `RUST_METHODS` is the Rust-only tail of the advertisement
    // (`control::build_caps`'s `rust_rows` arm).
    let expected: Vec<String> = registry::effective_methods()
        .filter(|row| row.implemented && !row.hidden && dispatch::is_ported(row.name))
        .map(|row| format!("\"{}\"", row.name))
        .collect();
    assert!(
        expected.iter().any(|name| name == "\"control.ping\""),
        "the expectation is not vacuous"
    );
    assert_eq!(
        methods_array(&shipped),
        expected.join(","),
        "only the served rows, in table order"
    );
    let unported = first_op();
    let without_one = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(move |name: &str| name != unported && dispatch::is_ported(name)),
    )
    .capabilities();
    assert!(
        methods_array(&shipped).contains(&format!("\"{unported}\"")),
        "a served op row is advertised: {shipped}"
    );
    assert!(
        !methods_array(&without_one).contains(&format!("\"{unported}\"")),
        "an unported row is not advertised: {without_one}"
    );
    let filtered_views: [ModuleView; pins::PIN_MODULE_COUNT] =
        core::array::from_fn(|_| ModuleView::new(ModState::Verified));
    assert_eq!(
        shipped,
        control::capabilities_object_filtered(&filtered_views, 5, &|method| {
            dispatch::is_ported(method.name)
        }),
        "the filter is control.rs's builder, not a second copy"
    );
    let everything = BridgeState::new(
        Arc::new(FakeModules {
            state: ModState::Verified,
            revision: 5,
        }),
        Box::new(|_: &str| true),
    )
    .capabilities();
    assert!(
        methods_array(&everything).contains(&format!("\"{unported}\"")),
        "the control: without the filter an op row is advertised: {everything}"
    );

    // ---- teardown (gate 2): PreShutdown stops the listener, deletes session.json, wakes the
    // queue's waiters; Shutdown is the same body (the C calls PreShutdown from Shutdown).
    ticker.stop();
    client.send(r#"{"jsonrpc":"2.0","id":"bye","method":"control.shutdown","params":{}}"#);
    let closing = client.line();
    assert_eq!(
        closing,
        control::result_ok(Some("bye"), control::CLOSING),
        "the C's control.shutdown reply (`:6592`)"
    );
    // `control.shutdown` already marks the bridge shutting down and stops accepting; the tool
    // teardown then runs. Both are idempotent.
    wiring::shutdown();
    wiring::pre_shutdown();

    assert_eq!(wiring::port(), None, "the listener is gone");
    assert!(!discovery.exists(), "session.json is deleted (`:6799`)");
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err(),
        "connecting to the stopped listener is refused"
    );
    assert!(
        queue::global().is_shutting_down(),
        "the queue is shut down (the C's g_shuttingDown)"
    );

    // The waiters: a flush with a record pending for a generation the writer will never see
    // returns at once once the queue is shut down (the C's WakeAllConditionVariable, `:6597`).
    queue::global()
        .out_push("pending\n".to_string(), 999_998)
        .expect("the outbound queue has room");
    let started = Instant::now();
    let waiter = std::thread::spawn(|| {
        queue::global().flush_outbound(999_998, Duration::from_secs(30));
    });
    waiter.join().expect("the waiter thread");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown wakes the flush waiter instead of leaving it to its 30 s deadline"
    );

    // ---- NEGATIVE (gate 3): a tick after Shutdown starts nothing, even if the once-flag were
    // somehow cleared - the initialized gate and the shut-down queue both refuse.
    for _ in 0..3 {
        wiring::tick();
    }
    assert_eq!(wiring::port(), None, "no listener after Shutdown");
    assert!(!discovery.exists(), "no discovery file after Shutdown");
}
