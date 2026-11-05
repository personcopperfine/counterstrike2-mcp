//! The socket-level `control.shutdown` lane, in its own test binary.
//!
//! `control.shutdown` shuts the **process-wide** queue down (`mcp_bridge.c:6596-6599`), so
//! every later test in the same binary would start against a shut-down bridge (the listener
//! refuses to start, `:6731`). One test, one process, and the C's observable sequence:
//! `{"closing":true}` (`:6592`), the socket closed after the reply, `g_shuttingDown` set, the
//! listener no longer accepting and the discovery file deleted by `stop()` (`:6799`).

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cs2_bridge::ipc::{self, Config, ControlCall, ControlHandler, ControlReply};
use cs2_bridge::queue::{self, BRIDGE_BUILD};

/// The stub handler's capabilities object (`build_caps` at `:6447`, stubbed: the method table
/// is WP2.C's).
const CAPS: &str = r#"{"methods":["pet.text"]}"#;

/// The token the fixed RNG below produces: `0x5a` repeated 32 times, as 64 hex characters
/// (`mcp_bridge.c:6739-6741`).
const FIXED_TOKEN: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// The `control.shutdown` half of tests/ipc.rs's stub: the reply is the C's constant
/// (`:6592`), and everything else is unknown (`None` -> `-32601`).
struct Stub;

impl ControlHandler for Stub {
    fn capabilities(&self) -> String {
        CAPS.to_string()
    }

    fn control(&self, call: ControlCall<'_>) -> Option<ControlReply> {
        match call {
            // `control.shutdown` result, `:6592`.
            ControlCall::Shutdown => Some(ControlReply::Result(r#"{"closing":true}"#.to_string())),
            ControlCall::Cancel { .. } | ControlCall::Other { .. } => None,
        }
    }
}

/// A connected client: a writer and a buffered reader over the same socket, as in tests/ipc.rs.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
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
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            Ok(count) => panic!("expected EOF, read {count} bytes: {line:?}"),
            Err(error) => panic!("expected EOF, got {error}"),
        }
    }
}

/// The hello frame the listener's token authenticates (`:6424-6426`).
fn hello(token: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":"h1","method":"control.hello","params":{{"protocol":1,"token":"{token}"}}}}"#
    )
}

#[test]
fn control_shutdown_answers_and_stops_serving() {
    scratch_log();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ipc-shutdown");
    let _ = std::fs::remove_dir_all(&dir);
    let path: PathBuf = dir.join("session.json");

    let mut listener = ipc::start(Config {
        handler: Arc::new(Stub),
        router: Arc::new(ipc::QueueEverything),
        discovery_path: Some(path.clone()),
        hello_deadline: ipc::HELLO_DEADLINE,
        // The fixed draw of tests/ipc.rs, so the token is `FIXED_TOKEN`.
        rng: Box::new(|raw| {
            *raw = [0x5a; ipc::token::RAW_BYTES];
            true
        }),
        pid: 4242,
        started_at_ms: 987_654,
    })
    .expect("bind the loopback listener")
    .expect("the injected RNG succeeds");
    assert_eq!(listener.token(), FIXED_TOKEN);
    let port = listener.port();

    let mut client = Client::connect(port);
    client.send(&hello(listener.token()));
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"h1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BRIDGE_BUILD}\"}},\
             \"result\":{{\"protocol\":1,\"bridgeVersion\":\"{BRIDGE_BUILD}\",\
             \"gameBuild\":\"{}\",\"capabilities\":{CAPS}}}}}\n",
            cs2_bridge::generated::pins::PIN_GAME_BUILD,
        ),
        "the C's hello reply shape (`:6444-6451`)"
    );

    client.send(r#"{"jsonrpc":"2.0","id":"bye","method":"control.shutdown","params":{}}"#);
    assert_eq!(
        client.line(),
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"bye\",\"bridge\":{{\"protocol\":1,\"build\":\"{BRIDGE_BUILD}\"}},\
             \"result\":{{\"closing\":true}}}}\n"
        ),
        "the C's control.shutdown reply (`:6592`)"
    );
    client.expect_closed();

    // `g_shuttingDown = 1; WakeAllConditionVariable(...)` (`:6596-6598`).
    assert!(
        queue::global().is_shutting_down(),
        "control.shutdown marks the process-wide queue shutting down"
    );
    // `start_listener_once`'s `if (g_started || g_shuttingDown) return;` (`:6731`): the
    // process cannot start a second listener now.
    let refused = ipc::start(Config {
        handler: Arc::new(Stub),
        router: Arc::new(ipc::QueueEverything),
        discovery_path: None,
        hello_deadline: ipc::HELLO_DEADLINE,
        rng: Box::new(|raw| {
            *raw = [0x5a; ipc::token::RAW_BYTES];
            true
        }),
        pid: 4242,
        started_at_ms: 987_654,
    })
    .expect("not an I/O error");
    assert!(refused.is_none(), "a shut-down bridge starts no listener");

    // The listener itself is stopped by the tool teardown; both steps are idempotent.
    listener.stop();
    assert!(!path.exists(), "session.json is deleted (`:6799`)");
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err(),
        "connecting to the stopped listener is refused"
    );
    listener.stop();
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
        let path = dir.join("cs2-bridge-ipc-shutdown-test.log");
        std::env::set_var("CS2_BRIDGE_LOG", &path);
        path
    })
    .clone()
}
