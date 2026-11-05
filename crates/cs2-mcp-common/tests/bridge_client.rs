//! Contract tests for the bridge client against the Rust mock (port of the POC
//! `tests/test_bridge_client.py`: MockBasics, TimeoutClassification, ModalGate, Taxonomy).
use cs2_mcp_common::bridge::{BridgeClient, ClientOptions, Kind, RequestOpts};
use cs2_mcp_common::mock::MockBridge;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn client(mb: &MockBridge) -> BridgeClient {
    BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    })
}

fn client_with(mb: &MockBridge, f: impl FnOnce(&mut ClientOptions)) -> BridgeClient {
    let mut o = ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    };
    f(&mut o);
    BridgeClient::new(o)
}

/// Send frames on a raw socket (one connection) and read `reads` replies.
async fn raw_frames(mb: &MockBridge, frames: &[Value], reads: usize) -> Vec<Value> {
    let mut s = tokio::net::TcpStream::connect((mb.host.as_str(), mb.port))
        .await
        .unwrap();
    for f in frames {
        s.write_all(format!("{f}\n").as_bytes()).await.unwrap();
    }
    let (rd, _wr) = s.split();
    let mut lines = BufReader::new(rd).lines();
    let mut out = Vec::new();
    for _ in 0..reads {
        match tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await {
            Ok(Ok(Some(l))) => out.push(serde_json::from_str(&l).unwrap()),
            _ => break,
        }
    }
    out
}

// ---------------------------------------------------------------- MockBasics

#[tokio::test]
async fn hello_capabilities_and_controls() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    let disc = c.connect(false).await.unwrap();
    assert_eq!(disc.bridge_version.as_deref(), Some(mb.build.as_str()));
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
    assert_eq!(c.version().await.unwrap()["protocol"], json!(1));
    assert_eq!(c.status().await.unwrap()["listener"], json!(true));
    let caps = c.capabilities().await.unwrap();
    let methods = caps["methods"].as_array().unwrap();
    assert!(methods.contains(&json!("doc.active")));
    assert!(!methods.contains(&json!("control.hello")));
    assert_eq!(caps["features"]["consoleCommand"], json!(false));
}

#[tokio::test]
async fn every_request_carries_explicit_deadline() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client_with(&mb, |o| o.deadline_ms = 12345);
    c.connect(false).await.unwrap();
    c.ping().await.unwrap();
    c.doc_active(None).await.unwrap();
    let err = c
        .request("control.bogus", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::MethodNotFound);
    let sent = mb.requests();
    assert!(!sent.is_empty());
    for r in &sent {
        let dm = r.params["deadlineMs"].as_u64().unwrap_or(0);
        assert!(dm > 0, "{r:?}");
    }
    assert_eq!(
        mb.method_calls("doc.active")[0].params["deadlineMs"],
        json!(12345)
    );
}

#[tokio::test]
async fn unknown_method() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    let err = c
        .request("control.bogus", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::MethodNotFound);
    assert!(err.retry_safe());
    assert_eq!(err.code, Some(-32601));
}

#[tokio::test]
async fn busy_second_client() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let mut c2 = client_with(&mb, |o| o.busy_retries = 0);
    let err = c2.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::Busy);
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
    assert_eq!(mb.busy_count(), 1);
}

#[tokio::test]
async fn wire_errors_before_auth() {
    let mb = MockBridge::start().await.unwrap();
    let r = raw_frames(
        &mb,
        &[json!({"jsonrpc": "2.0", "id": "n1", "method": "control.ping", "params": {}})],
        1,
    )
    .await;
    assert_eq!(r[0]["error"]["code"], json!(-32001));
    let r = raw_frames(&mb, &[json!({"jsonrpc": "2.0", "id": "n2", "method": "control.hello", "params": {"protocol": 1, "token": "00".repeat(32)}})], 1).await;
    assert_eq!(r[0]["error"]["code"], json!(-32001));
    let r = raw_frames(&mb, &[json!({"jsonrpc": "2.0", "id": "n3", "method": "control.hello", "params": {"protocol": 2, "token": mb.token}})], 1).await;
    assert_eq!(r[0]["error"]["code"], json!(-32002));
    let r = raw_frames(
        &mb,
        &[
            json!({"jsonrpc": "2.0", "id": "h", "method": "control.hello", "params": {"protocol": 1, "token": mb.token}}),
            json!({"jsonrpc": "2.0", "method": "control.ping", "params": {}}),
        ],
        2,
    )
    .await;
    assert_eq!(r[1]["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn build_mismatch_on_hello_result() {
    let mb = MockBridge::builder().protocol(2).start().await.unwrap();
    let mut c = client(&mb);
    let err = c.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::BuildMismatch);
    assert!(err.is_build_mismatch());
}

#[tokio::test]
async fn expected_build_mismatch() {
    let mb = MockBridge::start().await.unwrap();
    let mut c = client_with(&mb, |o| o.expected_build = Some("99999999".into()));
    let err = c.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::BuildMismatch);
    assert_eq!(err.data["expected"], json!("99999999"));
}

#[tokio::test]
async fn not_running_without_discovery_file() {
    let mut c = BridgeClient::new(ClientOptions {
        discovery_path: "/tmp/cs2mcp-definitely-missing/session.json".into(),
        ..Default::default()
    });
    let err = c.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::BridgeNotRunning);
    assert!(err.message.contains("no discovery file"));
    assert!(err.retry_safe());
}

#[tokio::test]
async fn stale_discovery_file() {
    let dir = std::env::temp_dir().join(format!("cs2mcp-stale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");
    std::fs::write(&path, json!({"pid": 999999, "protocol": 1, "bridgeVersion": "m2.1", "gameBuild": "1", "transport": "tcp", "host": "127.0.0.1", "port": 1, "token": "00".repeat(32), "startedAt": 0}).to_string()).unwrap();
    let mut c = BridgeClient::new(ClientOptions {
        discovery_path: path,
        ..Default::default()
    });
    let err = c.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::BridgeNotRunning);
    assert!(err.message.contains("cannot connect"), "{}", err.message);
    assert!(err.data.contains_key("pidAlive"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dead_pid_but_live_endpoint_still_connects() {
    let mb = MockBridge::start().await.unwrap();
    mb.write_discovery_raw(&json!({"pid": 999999, "protocol": 1, "bridgeVersion": mb.build, "gameBuild": "2000921", "transport": "tcp", "host": mb.host, "port": mb.port, "token": mb.token, "startedAt": 0})).unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
}

#[tokio::test]
async fn bad_discovery_token_eventually_not_running() {
    let mb = MockBridge::start().await.unwrap();
    mb.write_discovery_raw(&json!({"pid": 1, "protocol": 1, "bridgeVersion": mb.build, "gameBuild": "2000921", "transport": "tcp", "host": mb.host, "port": mb.port, "token": "11".repeat(32), "startedAt": 0})).unwrap();
    let mut c = client(&mb);
    let err = c.connect(false).await.unwrap_err();
    assert_eq!(err.kind, Kind::BridgeNotRunning);
}

#[tokio::test]
async fn doc_active_no_document() {
    let mb = MockBridge::builder().doc(None).start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert!(c.active_document(false, None).await.unwrap().is_none());
    assert_eq!(
        c.active_document(true, None).await.unwrap_err().kind,
        Kind::NoDocument
    );
    assert_eq!(
        c.doc_active(None).await.unwrap(),
        json!({"doc": null, "reason": "no_session"})
    );
}

#[tokio::test]
async fn hammer_not_loaded_reason() {
    let mb = MockBridge::builder()
        .doc(None)
        .responder_value(
            "doc.active",
            json!({"doc": null, "reason": "hammer_not_loaded"}),
        )
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert_eq!(
        c.active_document(false, None).await.unwrap_err().kind,
        Kind::HammerNotLoaded
    );
}

/// The real bridge answers `doc.active` before hammer.dll loads with `-32017 PrerequisiteFailed
/// {module:"hammer.dll", state:"not_loaded"}` (LIVE-P1b): the same "Hammer is not up" case.
#[tokio::test]
async fn doc_active_prerequisite_failed_for_hammer_dll_is_hammer_not_loaded() {
    for state in ["not_loaded", "verifying"] {
        let mb = MockBridge::builder()
            .error(
                "doc.active",
                -32017,
                "PrerequisiteFailed",
                Some(json!({"check": "identity", "method": "doc.active", "module": "hammer.dll", "state": state})),
            )
            .start()
            .await
            .unwrap();
        let mut c = client(&mb);
        c.connect(false).await.unwrap();
        let e = c.active_document(false, None).await.unwrap_err();
        assert_eq!(e.kind, Kind::HammerNotLoaded, "{state}");
        assert_eq!(e.data.get("module"), Some(&json!("hammer.dll")));
    }
    // a mismatch (or another module) stays a PrerequisiteFailed
    let mb = MockBridge::builder()
        .error(
            "doc.active",
            -32017,
            "PrerequisiteFailed",
            Some(json!({"module": "hammer.dll", "state": "mismatch"})),
        )
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert_eq!(
        c.active_document(false, None).await.unwrap_err().kind,
        Kind::PrerequisiteFailed
    );
}

// ---------------------------------------------------------------- TimeoutClassification

async fn timeout_fixture() -> (MockBridge, BridgeClient) {
    let mb = MockBridge::builder()
        .responder_value("entity.create", json!({"handle": "n1"}))
        .delay("entity.create", 400)
        .start()
        .await
        .unwrap();
    let mut c = client_with(&mb, |o| {
        o.deadline_ms = 150;
        o.reply_grace_ms = 200;
    });
    c.connect(false).await.unwrap();
    (mb, c)
}

#[tokio::test]
async fn queue_timeout_is_retry_safe() {
    let (mb, mut c) = timeout_fixture().await;
    let err = c
        .request(
            "entity.create",
            json!({"class": "x", "origin": [0, 0, 0]}),
            RequestOpts::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::QueueTimeout);
    assert!(err.retry_safe());
    assert!(!err.outcome_unknown());
    assert_eq!(err.code, Some(-32008));
    assert_eq!(mb.method_calls("entity.create").len(), 1);
}

#[tokio::test]
async fn outcome_unknown_is_not_retry_safe() {
    let (mb, mut c) = timeout_fixture().await;
    mb.clear_delays();
    mb.add_silent("entity.create");
    let err = c
        .request(
            "entity.create",
            json!({"class": "x", "origin": [0, 0, 0]}),
            RequestOpts::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::OutcomeUnknown);
    assert!(!err.retry_safe());
    assert!(err.outcome_unknown());
    assert!(!c.connected());
}

#[tokio::test]
async fn connection_lost_is_outcome_unknown() {
    let (mb, mut c) = timeout_fixture().await;
    mb.clear_delays();
    mb.add_drop("entity.create");
    let err = c
        .request(
            "entity.create",
            json!({"class": "x", "origin": [0, 0, 0]}),
            RequestOpts::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::ConnectionLost);
    assert!(err.outcome_unknown());
    assert!(!err.retry_safe());
}

#[tokio::test]
async fn connection_recovers_on_next_request() {
    let (mb, mut c) = timeout_fixture().await;
    mb.clear_delays();
    mb.add_silent("entity.create");
    assert_eq!(
        c.request("entity.create", json!({}), RequestOpts::default())
            .await
            .unwrap_err()
            .kind,
        Kind::OutcomeUnknown
    );
    mb.clear_silent();
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
    assert_eq!(c.connects(), 2);
}

// ---------------------------------------------------------------- ModalGate

#[tokio::test]
async fn wrapped_main_thread_age_is_not_a_stall() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(u64::MAX))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let live = c.liveness().await;
    assert!(!live.hammer_blocked);
    assert_eq!(live.main_thread_age_ms, Some(0));
    assert!(c.doc_active(None).await.unwrap().is_object());
}

#[tokio::test]
async fn hammer_blocked_gate_blocks_queued_methods() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::HammerBlocked);
    assert!(err.retry_safe());
    assert!(!err.outcome_unknown());
    assert!(err.data["mainThreadAgeMs"].as_u64().unwrap() > 10000);
    assert!(mb.method_calls("doc.active").is_empty());
    // control.* bypasses the gate
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
    // widening the threshold un-blocks it
    c.opts.blocked_age_ms = 120000;
    assert!(c.doc_active(None).await.unwrap().is_object());
}

#[tokio::test]
async fn gate_blocks_when_a_request_is_in_progress() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .drain_blocked(true)
        .inflight(1)
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let live = c.liveness().await;
    assert!(live.hammer_blocked);
    assert!(live.note.unwrap().contains("in progress"));
    assert_eq!(
        c.request("doc.active", json!({}), RequestOpts::default())
            .await
            .unwrap_err()
            .kind,
        Kind::HammerBlocked
    );
}

#[tokio::test]
async fn gate_fails_closed_when_liveness_is_unknown() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(None)
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert!(c.liveness().await.unknown);
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::HammerBlocked);
    assert!(err.retry_safe());
    assert_eq!(c.ping().await.unwrap()["ok"], json!(true));
}

#[tokio::test]
async fn engine_busy_during_a_running_job() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    c.note_job(Duration::from_secs(30));
    mb.set_jobs(vec![json!({"id": "j1", "kind": "mapload", "state": "running"})]);
    let live = c.liveness().await;
    assert!(live.engine_busy);
    assert!(!live.hammer_blocked);
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::EngineBusy);
    assert!(err.retry_safe());
    mb.set_jobs(vec![json!({"id": "j1", "state": "done"})]);
    assert_eq!(
        c.request("doc.active", json!({}), RequestOpts::default())
            .await
            .unwrap_err()
            .kind,
        Kind::HammerBlocked
    );
}

#[tokio::test]
async fn an_external_job_is_engine_busy_without_a_bridge_job() {
    // review-1 L1: `run_map`'s load is not a bridge job, so `control.status` lists nothing while
    // the frozen frame loop is being excused — the grace must not be cleared and the gate must
    // say `EngineBusy`, not `HammerBlocked`.
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    mb.set_jobs(Vec::new());
    c.note_external_job(Duration::from_secs(30));
    let live = c.liveness().await;
    assert!(live.engine_busy, "{live:?}");
    assert!(!live.hammer_blocked);
    assert!(live.external_job, "{live:?}");
    assert!(
        live.external_grace_remaining_ms.is_some_and(|ms| ms > 0),
        "the remaining grace is reported (review-2 N4): {live:?}"
    );
    let json = live.to_json();
    assert_eq!(json["externalJob"], json!(true), "{json}");
    assert!(
        json["externalGraceRemainingMs"].as_u64().is_some(),
        "{json}"
    );
    assert!(live.note.unwrap().contains("engine load"));
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::EngineBusy);
    assert!(err.retry_safe());
    assert!(!err.outcome_unknown());

    // the caller releasing the grace stops excusing the stall immediately (the load settled)
    c.clear_external_job();
    let live = c.liveness().await;
    assert!(!live.external_job, "{live:?}");
    assert!(live.hammer_blocked);
    assert_eq!(
        c.request("doc.active", json!({}), RequestOpts::default())
            .await
            .unwrap_err()
            .kind,
        Kind::HammerBlocked
    );
}

#[tokio::test]
async fn a_bridge_job_grace_is_still_cleared_by_the_job_list() {
    // the external flag must not leak into the bridge-job behaviour: a `note_job` grace with no
    // running job in `control.status` is cleared (and `clear_external_job` does not touch it)
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    mb.set_jobs(Vec::new());
    c.note_job(Duration::from_secs(30));
    let live = c.liveness().await;
    assert!(live.hammer_blocked, "{live:?}");
    assert!(!live.engine_busy, "{live:?}");
    assert!(!live.external_job, "the bridge job's excuse is not an external one: {live:?}");
    // ... and once its grace was cleared, a second look is still `HammerBlocked` (not a panic)
    c.clear_external_job();
    assert!(c.liveness().await.hammer_blocked);
}

/// review-2 N4: the external grace is a FELD of its own, so `clear_external_job` releases
/// exactly the external window and never the bridge job's: a bridge `note_job` grace that was
/// claimed alongside an external one survives the external release.
#[tokio::test]
async fn a_bridge_job_grace_survives_the_external_release() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    // an external load is in flight AND the bridge runs a job (both graces can legitimately
    // overlap: run_map's console load does not own the bridge's own job grace)
    mb.set_jobs(vec![json!({"id": "j1", "kind": "mapload", "state": "running"})]);
    c.note_job(Duration::from_secs(30));
    c.note_external_job(Duration::from_secs(30));
    let live = c.liveness().await;
    assert!(live.engine_busy, "{live:?}");
    assert!(!live.external_job, "the bridge job speaks first, it is verifiable: {live:?}");
    // the external load settles: only ITS grace is dropped, the bridge job's stays alive (a
    // running job is still listed, so the frame loop stays excused)
    c.clear_external_job();
    let live = c.liveness().await;
    assert!(live.engine_busy, "{live:?}");
    assert!(!live.external_job);
    // ... and once the bridge's running job is gone, the gate fails closed again
    mb.set_jobs(vec![json!({"id": "j1", "state": "done"})]);
    assert!(c.liveness().await.hammer_blocked, "a done job means no excuse");
}

#[tokio::test]
async fn gate_can_be_disabled() {
    let mb = MockBridge::builder()
        .main_thread_age_ms(Some(60000))
        .start()
        .await
        .unwrap();
    let mut c = client_with(&mb, |o| o.block_when_stale = false);
    c.connect(false).await.unwrap();
    assert!(c.liveness().await.hammer_blocked);
    assert!(c.doc_active(None).await.unwrap().is_object());
}

// ---------------------------------------------------------------- Taxonomy

#[tokio::test]
async fn unknown_wire_code_is_outcome_unknown() {
    let mb = MockBridge::builder()
        .error("doc.active", -32099, "SomethingNew", None)
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::UnknownWire);
    assert_eq!(err.code, Some(-32099));
    assert!(err.outcome_unknown());
    assert!(!err.retry_safe());
}

#[tokio::test]
async fn prerequisite_failed_is_a_build_mismatch() {
    let mb = MockBridge::builder()
        .error(
            "doc.active",
            -32017,
            "PrerequisiteFailed",
            Some(json!({"module": "hammer.dll", "check": "identity", "state": "mismatch"})),
        )
        .start()
        .await
        .unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let err = c
        .request("doc.active", json!({}), RequestOpts::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind, Kind::PrerequisiteFailed);
    assert!(err.is_build_mismatch());
    assert!(err.retry_safe());
    assert_eq!(err.data["module"], json!("hammer.dll"));
}

#[tokio::test]
async fn every_wire_code_maps_to_its_class() {
    let table: &[(i64, Kind, bool, bool)] = &[
        (-32001, Kind::NotAuthenticated, false, false),
        (-32002, Kind::VersionMismatch, false, false),
        (-32003, Kind::NoDocument, false, false),
        (-32004, Kind::DocumentChanged, true, false),
        (-32005, Kind::StaleHandle, true, false),
        (-32006, Kind::UnknownClass, true, false),
        (-32007, Kind::Busy, true, false),
        (-32008, Kind::QueueTimeout, true, false),
        (-32009, Kind::Cancelled, true, false),
        (-32010, Kind::NotCancellable, false, true),
        (-32011, Kind::NotImplemented, true, false),
        (-32012, Kind::PartialFailure, false, true),
        (-32013, Kind::ShuttingDown, true, false),
        (-32014, Kind::AssetNotFound, true, false),
        (-32015, Kind::UntitledDocument, true, false),
        (-32016, Kind::UnsavedChanges, true, false),
        (-32017, Kind::PrerequisiteFailed, true, false),
        (-32018, Kind::DocumentNotEditable, true, false),
        (-32019, Kind::NoViewport, true, false),
        (-32020, Kind::RenderFailed, true, false),
        (-32021, Kind::NoGameWorld, true, false),
        (-32022, Kind::NodeNotFound, true, false),
        (-32023, Kind::SchemaUnavailable, true, false),
        (-32024, Kind::NoLocalPlayer, true, false),
        (-32601, Kind::MethodNotFound, true, false),
        (-32603, Kind::ProtocolError, false, false),
    ];
    let mb = MockBridge::start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    for (code, kind, safe, unknown) in table {
        mb.add_error(
            "doc.active",
            *code,
            kind.name(),
            Some(json!({"detail": format!("detail for {code}")})),
        );
        let err = c
            .request("doc.active", json!({}), RequestOpts::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, *kind, "code {code}");
        assert_eq!(err.code, Some(*code));
        assert_eq!(err.retry_safe(), *safe, "retry_safe for {code}");
        assert_eq!(
            err.outcome_unknown(),
            *unknown,
            "outcome_unknown for {code}"
        );
        assert_eq!(err.message, format!("detail for {code}"));
        assert_eq!(err.data["method"], json!("doc.active"));
        let payload = err.to_json();
        assert_eq!(payload["name"], json!(kind.name()));
        assert_eq!(payload["retrySafe"], json!(*safe));
        assert_eq!(payload["outcomeUnknown"], json!(*unknown));
    }
}

#[tokio::test]
async fn doc_active_reason_mapping() {
    let cases = [
        ("hammer_mismatch", Kind::BuildMismatch),
        ("tf2_mismatch", Kind::BuildMismatch),
        ("bad_session", Kind::BuildMismatch),
        ("hammer_unavailable", Kind::HammerNotLoaded),
        ("hammer_verifying", Kind::HammerNotLoaded),
        ("no_document", Kind::NoDocument),
        ("no_session", Kind::NoDocument),
    ];
    for (reason, kind) in cases {
        let mb = MockBridge::builder()
            .responder_value("doc.active", json!({"doc": null, "reason": reason}))
            .start()
            .await
            .unwrap();
        let mut c = client(&mb);
        c.connect(false).await.unwrap();
        assert_eq!(
            c.active_document(true, None).await.unwrap_err().kind,
            kind,
            "{reason}"
        );
        if kind == Kind::NoDocument {
            assert!(c.active_document(false, None).await.unwrap().is_none());
        } else {
            assert_eq!(
                c.active_document(false, None).await.unwrap_err().kind,
                kind,
                "{reason}"
            );
        }
    }
}

#[tokio::test]
async fn capabilities_refresh_on_revision_change() {
    let mb = MockBridge::builder().revision(1).start().await.unwrap();
    let baseline = mb.baseline_methods();
    mb.set_methods(baseline.clone());
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    assert!(!c.has("map.reload"));
    let mut more = baseline.clone();
    more.push("map.reload".into());
    mb.set_methods(more);
    mb.set_revision(Some(2));
    c.liveness().await;
    assert!(c.has("map.reload"));
    assert_eq!(c.caps_revision(), Some(&json!(2)));
}

/// A running `resourcecompiler` job in `control.status` (the bridge's job table) never excuses a
/// quiet frame loop, even when a grace is alive: a modal during a compile stays `HammerBlocked`.
#[tokio::test]
async fn a_running_compile_job_does_not_excuse_a_blocked_main_thread() {
    let mb = MockBridge::builder().main_thread_age_ms(Some(20_000)).start().await.unwrap();
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    c.note_job(Duration::from_secs(30));
    mb.set_jobs(vec![json!({"id": "j1-x", "kind": "resourcecompiler", "state": "running"})]);
    let live = c.liveness().await;
    assert!(live.hammer_blocked, "{live:?}");
    assert!(!live.engine_busy);
}
