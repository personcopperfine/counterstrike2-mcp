//! The RCON client against the fake server in each mode (classic sentinel, sentinel ignored, the
//! lone auth response CS2 sends). No game, loopback only.
use cs2_mcp_common::mock::rcon::{FakeRcon, Mode};
use cs2_mcp_common::rcon::{RconClient, RconError, REPLY_QUIET};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const T: Duration = Duration::from_secs(5);

async fn connect(f: &FakeRcon) -> RconClient<tokio::net::TcpStream> {
    RconClient::connect(&f.addr, T).await.expect("connect")
}

async fn fake(password: &str, mode: Mode) -> FakeRcon {
    FakeRcon::bind("127.0.0.1:0", password, mode, Default::default()).await
}

#[tokio::test]
async fn classic_server_auth_and_sentinel_reply() {
    let f = fake("secret", Mode::Sentinel).await;
    let mut c = connect(&f).await;
    c.auth("secret", T).await.unwrap();
    let started = Instant::now();
    assert_eq!(c.exec("status", T).await.unwrap(), "ran status");
    // the sentinel's reply ends the call before the quiet period would
    assert!(started.elapsed() < REPLY_QUIET * 4);
}

#[tokio::test]
async fn cs2_lone_auth_response_then_quiet_period_ends_the_reply() {
    let f = fake("secret", Mode::LoneAuthResponse).await;
    let mut c = connect(&f).await;
    c.auth("secret", T).await.unwrap();
    let started = Instant::now();
    assert_eq!(c.exec("status", T).await.unwrap(), "ran status");
    // no sentinel answer: the call ends after one quiet period, not at the timeout
    assert!(started.elapsed() >= REPLY_QUIET);
    assert!(started.elapsed() < T);
}

#[tokio::test]
async fn sentinel_ignored_with_a_classic_auth_still_works() {
    let f = fake("secret", Mode::SentinelIgnored).await;
    let mut c = connect(&f).await;
    c.auth("secret", T).await.unwrap();
    assert_eq!(c.exec("status", T).await.unwrap(), "ran status");
}

#[tokio::test]
async fn multi_packet_replies_are_concatenated_in_every_mode() {
    for mode in [
        Mode::Sentinel,
        Mode::SentinelIgnored,
        Mode::LoneAuthResponse,
    ] {
        let f = fake("secret", mode).await;
        let mut c = connect(&f).await;
        c.auth("secret", T).await.unwrap();
        assert_eq!(
            c.exec("cvarlist", T).await.unwrap(),
            "sv_cheats mp_freezetime bot_quota",
            "{mode:?}"
        );
        // the connection stays usable for the next command
        assert_eq!(c.exec("x", T).await.unwrap(), "ran x", "{mode:?}");
    }
}

#[tokio::test]
async fn a_wrong_password_is_rejected_in_both_auth_shapes() {
    for mode in [Mode::Sentinel, Mode::LoneAuthResponse] {
        let f = fake("the-real-one", mode).await;
        let mut c = connect(&f).await;
        assert!(
            matches!(c.auth("nope", T).await, Err(RconError::AuthFailed)),
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn exec_before_auth_is_refused() {
    let f = fake("secret", Mode::LoneAuthResponse).await;
    let mut c = connect(&f).await;
    assert!(matches!(
        c.exec("status", T).await,
        Err(RconError::NotAuthenticated)
    ));
}

#[tokio::test]
async fn commands_are_recorded_only_after_auth() {
    let log = cs2_mcp_common::mock::rcon::CommandLog::default();
    let f = FakeRcon::bind("127.0.0.1:0", "pw", Mode::LoneAuthResponse, log.clone()).await;
    let mut c = connect(&f).await;
    let _ = c.exec("sneaky", Duration::from_millis(500)).await;
    assert!(log.lock().unwrap().is_empty());
    c.auth("pw", T).await.unwrap();
    c.exec("bot_add_ct", T).await.unwrap();
    assert_eq!(f.commands(), vec!["bot_add_ct".to_string()]);
}

#[tokio::test]
async fn a_silent_server_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(sock);
    });
    let mut c = RconClient::connect(&addr, T).await.unwrap();
    let short = Duration::from_millis(200);
    assert!(matches!(
        c.auth("x", short).await,
        Err(RconError::Timeout(_))
    ));
}

#[tokio::test]
async fn connect_to_a_closed_port_is_an_io_error() {
    // bind then drop: the port is free again, nothing listens there
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().to_string()
    };
    assert!(matches!(
        RconClient::connect(&addr, T).await,
        Err(RconError::Io(_))
    ));
}
