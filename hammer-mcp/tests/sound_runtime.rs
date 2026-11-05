//! Mock-backed tests for `drain_sounds` (WP-S5, plan §1 + §5 item 4).
//!
//! Everything goes through `MockBridge`'s `sound.drain` / `sound.status` responders (no mock
//! source change): the tool is a passthrough plus a local filter and the packed-params decoder, so
//! the mock's job is to hand back the exact object the C bridge writes
//! (`bridge/mcp_bridge_game.inc` `op_sound`) — field for field, with the live captured
//! `params.hex` from the R3 probe (`target/orch/re-snd.md`).
//!
//! "nothing sent" means no `sound.*` request: the registry's capability gate and the client's
//! modal gate talk to the mock (`control.hello`/`control.ping`) before any handler runs, which is
//! the shared behaviour of every tool here, so the assertions count the tool's own methods only.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{Budget, CallFailure, Mutability, Registry, ToolError, ToolOutput};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::sounds;
use serde_json::{json, Value};
use std::path::Path;

/// The live captured packed-params buffer for a `snd_event_*` (R3 "Position"): field hash
/// `0x5A7CCE4D` (`position`), type `0x0a`, `u16` size 12, then the f32 origin (944, 160, 64).
const CAPTURED: &str = "4dce7c5a0a0c0000006c440000204300008042";

// ---------------------------------------------------------------- fixtures

fn config(discovery: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    // hermetic podman/vk-layer/project (the pattern of tests/render.rs, copied): nothing here may
    // touch the host's real podman or project tree.
    W::parse_from([
        "t".to_string(),
        "--discovery".to_string(),
        discovery.display().to_string(),
        "--podman".into(),
        "/nonexistent/cs2mcp-test-podman".into(),
        "--vk-layer-dir".into(),
        "/nonexistent/cs2mcp-test-vklayer".into(),
        "--project-root".into(),
        "/nonexistent/cs2mcp-test-project".into(),
    ])
    .c
}

/// The production registration path (`tools/sounds.rs`), so the order, the capability gating and
/// the declared bridge methods are exercised exactly as in production.
struct Server {
    state: HammerState,
    registry: Registry<HammerState>,
}

impl Server {
    fn new(discovery: &Path) -> Server {
        let state = HammerState::from_config(&config(discovery)).unwrap();
        let mut registry = Registry::new();
        sounds::register(&mut registry);
        Server { state, registry }
    }

    async fn call_output(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        let budget = Budget::new(45_000, 30_000);
        match self.registry.call(&self.state, name, args, budget).await {
            Ok(out) => Ok(out),
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }

    async fn call(&self, args: Value) -> Result<Value, ToolError> {
        self.call_output("drain_sounds", args)
            .await
            .map(|out| out.json)
    }
}

/// The mock with both sound methods advertised (`sound.status` is what `installOnly` sends).
fn sound_mock(drain: Value) -> cs2_mcp_common::mock::MockBuilder {
    MockBridge::builder()
        .methods(["sound.drain", "sound.status"])
        .responder_value("sound.drain", drain)
}

/// The `sound.drain` reply with no events (the baseline every case overrides pieces of).
fn reply(events: Value) -> Value {
    let total = events.as_array().map_or(0, Vec::len);
    json!({"events": events, "truncated": false, "dropped": 0, "total": total, "pending": 0})
}

/// A minimal drain event carrying everything the C bridge writes for a via-11 start.
fn point_event(name: &str, seq: i64, params: Value) -> Value {
    json!({
        "seq": seq, "hash": 1518138957i64, "name": name, "entindex": 0, "guid": 4294967297i64,
        "via": 11, "flags": 0, "ageMs": 1000, "origin": [0.0, 0.0, 0.0],
        "entityName": "worldent", "params": params,
    })
}

/// The `params` object a via-11 event carries (`len` + the hex copy + the bridge's truncation
/// flag).
fn params(len: u64, hex: &str, truncated: bool) -> Value {
    json!({"len": len, "hex": hex, "truncated": truncated})
}

/// Every `sound.*` request the tool dispatched (the control-plane calls are not the tool's).
fn sound_requests(mb: &MockBridge) -> Vec<String> {
    mb.requests()
        .into_iter()
        .filter(|r| r.method.starts_with("sound."))
        .map(|r| r.method)
        .collect()
}

fn error_of(result: Result<Value, ToolError>) -> ToolError {
    match result {
        Ok(value) => panic!("expected a failure, got {value}"),
        Err(error) => error,
    }
}

// ---------------------------------------------------------------- registration

#[test]
fn drain_sounds_is_registered_in_the_sound_block_with_a_closed_schema() {
    let mut registry = Registry::<HammerState>::new();
    sounds::register(&mut registry);
    let names = registry.names();
    assert_eq!(
        names.iter().filter(|n| **n == "drain_sounds").count(),
        1,
        "registered exactly once: {names:?}"
    );
    let spec = registry.spec("drain_sounds").unwrap();
    assert_eq!(spec.mutability, Mutability::Mutating);
    assert_eq!(spec.bridge_methods.to_vec(), vec!["sound.drain"]);
    assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    for key in ["max", "resolve", "filter", "installOnly"] {
        assert!(
            spec.input_schema["properties"][key].is_object(),
            "{key} is in the input schema"
        );
    }
    // the module's own order pin matches the registration it performed
    assert_eq!(sounds::SOUND_TOOL_ORDER.last(), Some(&"drain_sounds"));
}

// ---------------------------------------------------------------- passthrough

#[tokio::test]
async fn every_bridge_field_passes_through_and_the_arithmetic_adds_up() {
    let mb = sound_mock(reply(json!([
        point_event("mcp.snd.point", 6, params(19, CAPTURED, false)),
        {"seq": 7, "hash": 99, "name": "Buttons.snd9", "entindex": 0, "guid": 5, "via": 12,
         "flags": 1, "ageMs": 500, "origin": [944.0, 160.0, 64.0], "entityName": "worldent",
         "aux": [1, 2], "inst150": 7},
    ])))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();

    assert_eq!(out["matched"], json!(2));
    assert_eq!(out["unmatched"], json!(0));
    assert_eq!(out["total"], json!(2));
    assert_eq!(out["dropped"], json!(0));
    assert_eq!(out["truncated"], json!(false));
    assert_eq!(out["pending"], json!(0));
    let events = out["events"].as_array().unwrap();
    assert_eq!(
        events[0],
        json!({
            "seq": 6, "hash": 1518138957i64, "name": "mcp.snd.point", "entindex": 0,
            "guid": 4294967297i64, "via": 11, "flags": 0, "ageMs": 1000,
            "origin": [0.0, 0.0, 0.0], "entityName": "worldent",
            "position": [944.0, 160.0, 64.0],
            "params": {
                "len": 19, "bytes": 19, "truncated": false, "decoded": true,
                "fields": [{"hash": "0x5a7cce4d", "type": 10, "size": 12, "name": "position",
                            "value": [944.0, 160.0, 64.0]}],
            },
        }),
        "every bridge field rides through, plus the decoded params and the position"
    );
    // a via-12 event has no params in the reply (the bridge omits it) and no decoded position,
    // but its own fields — the slot-81 aux/inst150 included — are untouched
    assert_eq!(events[1]["via"], json!(12));
    assert_eq!(events[1]["aux"], json!([1, 2]));
    assert_eq!(events[1]["inst150"], json!(7));
    assert_eq!(events[1]["origin"], json!([944.0, 160.0, 64.0]));
    assert_eq!(events[1]["params"], Value::Null, "no params key -> null");
    assert_eq!(events[1]["position"], Value::Null);
    // the request carried the plan's defaults explicitly (the bridge's own default is max 128)
    let sent = mb.method_calls("sound.drain");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["max"], json!(256));
    assert_eq!(sent[0].params["resolve"], json!(true));
    assert!(sent[0].params["deadlineMs"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn max_is_always_sent_and_the_caller_value_wins() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    server.call(json!({"max": 1})).await.unwrap();
    assert_eq!(
        mb.method_calls("sound.drain").last().unwrap().params["max"],
        json!(1)
    );
    server.call(json!({"max": 256})).await.unwrap();
    assert_eq!(
        mb.method_calls("sound.drain").last().unwrap().params["max"],
        json!(256)
    );
    assert_eq!(mb.method_calls("sound.drain").len(), 2);
}

#[tokio::test]
async fn resolve_false_is_sent_and_noted() {
    let mb = sound_mock(reply(
        json!([{"seq": 1, "name": "x", "via": 11, "ageMs": 5}]),
    ))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({"resolve": false})).await.unwrap();
    assert_eq!(
        mb.method_calls("sound.drain").last().unwrap().params["resolve"],
        json!(false)
    );
    // no origin/entityName keys came back (the bridge skipped the entity read) and they are not
    // invented
    let event = &out["events"][0];
    assert!(event.get("origin").is_none());
    assert!(event.get("entityName").is_none());
    assert_eq!(event["position"], Value::Null);
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("resolve:false")),
        "the missing origins are noted: {notes:?}"
    );
}

#[tokio::test]
async fn dropped_events_get_a_note() {
    let mb = sound_mock(json!({
        "events": [point_event("mcp.snd.point", 6, Value::Null)],
        "truncated": false, "dropped": 3, "total": 40, "pending": 0,
    }))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    assert_eq!(out["dropped"], json!(3));
    assert_eq!(out["total"], json!(40));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("overflowed")),
        "the ring overflow is noted: {notes:?}"
    );
}

#[tokio::test]
async fn truncated_is_reported_with_its_note() {
    let mb = sound_mock(json!({
        "events": [point_event("mcp.snd.point", 6, Value::Null)],
        "truncated": true, "dropped": 0, "total": 9, "pending": 4,
    }))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    assert_eq!(out["truncated"], json!(true));
    assert_eq!(out["pending"], json!(4), "the cut events are still queued");
    assert_eq!(out["matched"], json!(1));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("reply cap")),
        "a truncated reply is noted: {notes:?}"
    );
}

// ---------------------------------------------------------------- params decoding

#[tokio::test]
async fn the_captured_params_decode_into_the_position() {
    let mb = sound_mock(reply(json!([point_event(
        "mcp.snd.point",
        6,
        params(19, CAPTURED, false)
    )])))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let event = &out["events"][0];
    assert_eq!(
        event["position"],
        json!([944.0, 160.0, 64.0]),
        "the live captured buffer is the entity's Hammer origin"
    );
    assert_eq!(
        event["params"],
        json!({
            "len": 19, "bytes": 19, "truncated": false, "decoded": true,
            "fields": [{"hash": "0x5a7cce4d", "type": 10, "size": 12, "name": "position",
                        "value": [944.0, 160.0, 64.0]}],
        })
    );
    // the same event through `within` matches on the decoded position, not the worldent origin
    let out = server
        .call(json!({"filter": {"within": {"center": [944, 160, 64], "radius": 1}}}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(1));
    assert_eq!(out["unmatched"], json!(0));
}

#[tokio::test]
async fn an_m2_19_bridge_reply_notes_the_missing_params_for_via_11_only() {
    let mb = MockBridge::builder()
        .build("m2.19")
        .methods(["sound.drain", "sound.status"])
        .responder_value("sound.drain", reply(json!([
            // the pre-m2.20 shape: the key was never written, so it is ABSENT everywhere
            {"seq": 1, "name": "mcp.snd.point", "via": 11, "ageMs": 10,
             "origin": [0.0, 0.0, 0.0],
             "entityName": "worldent"},
            {"seq": 2, "name": "t.other.slot", "via": 12, "ageMs": 5},
        ])))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(events[0]["params"], Value::Null, "absent stays null");
    assert_eq!(events[0]["position"], Value::Null);
    assert_eq!(events[1]["params"], Value::Null, "via 12 is the same shape");
    let notes = out["notes"].as_array().unwrap();
    let age: Vec<&str> = notes
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| n.contains("bridge < m2.20"))
        .collect();
    assert_eq!(
        age.len(),
        1,
        "exactly one bridge-age note per drain: {notes:?}"
    );
    assert!(
        age[0].contains("(build m2.19)")
            && age[0].contains("no packed params")
            && age[0].contains("positions unavailable"),
        "the note names the build it saw: {age:?}"
    );

    // the same reply WITHOUT the via-11 event never carries the note: the other slots never had
    // params, so absence there is ordinary
    let mb2 = MockBridge::builder()
        .build("m2.19")
        .methods(["sound.drain", "sound.status"])
        .responder_value(
            "sound.drain",
            reply(json!([{"seq": 3, "name": "t.12.only", "via": 12, "ageMs": 5}])),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb2.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let notes = out["notes"].as_array().unwrap();
    assert!(
        !notes
            .iter()
            .filter_map(Value::as_str)
            .any(|n| n.contains("bridge < m2.20")),
        "via-12-only absence is not bridge age: {notes:?}"
    );
}

#[tokio::test]
async fn an_m2_20_reply_with_a_null_params_is_marked_unreadable_but_not_old_bridge() {
    let mb = MockBridge::builder()
        .build("m2.20")
        .methods(["sound.drain", "sound.status"])
        .responder_value("sound.drain", reply(json!([
            point_event("t.unreadable", 1, Value::Null),
            point_event("t.also.sent", 2, params(19, CAPTURED, false)),
            // a via-12 event with the key absent is ordinary on either build
            {"seq": 3, "name": "t.12", "via": 12, "ageMs": 5},
        ])))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(
        events[0]["params"],
        json!({"unreadable": true}),
        "present but null = the pointer was unreadable, not an old bridge"
    );
    assert_eq!(events[0]["position"], Value::Null);
    assert_eq!(
        events[2]["params"],
        Value::Null,
        "an absent key on a non-slot-11 event stays plain null"
    );
    let notes = out["notes"].as_array().unwrap();
    assert!(
        !notes
            .iter()
            .filter_map(Value::as_str)
            .any(|n| n.contains("bridge < m2.20")),
        "no bridge-age note when the key was present: {notes:?}"
    );
}

#[tokio::test]
async fn the_bridge_age_note_quotes_the_envelope_build_the_reply_carried() {
    // the mock's default build string (not m2.19/m2.20) must be what the note quotes, not a
    // guessed one
    let mb = MockBridge::builder()
        .build("m2.18")
        .methods(["sound.drain", "sound.status"])
        .responder_value(
            "sound.drain",
            reply(json!([{"seq": 1, "name": "t.p", "via": 11, "ageMs": 9}])),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .filter_map(Value::as_str)
            .any(|n| n.contains("(build m2.18)") && n.contains("positions unavailable")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn truncated_overrunning_and_mismatched_params_stay_undecoded_with_a_null_position() {
    let mb = sound_mock(reply(json!([
        // the bridge copied only 128 of 200 bytes: the walk may still fit, the position must not
        point_event("t.a", 1, params(200, CAPTURED, true)),
        // the entry header claims 32 bytes of data but only 12 follow
        point_event("t.b", 2, params(19, "4dce7c5a0a200000006c440000204300008042", false)),
        // a `len` the copied bytes do not match
        point_event("t.c", 3, params(25, CAPTURED, false)),
        // the params pointer was unreadable: the m2.20 bridge sends null (marked unreadable below)
        point_event("t.d", 4, Value::Null),
        // ...and an event with no params key at all (via 12/13/81)
        {"seq": 5, "name": "t.e", "via": 12, "ageMs": 3},
    ])))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({})).await.unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(
        out["matched"],
        json!(5),
        "an undecoded event is still matched"
    );
    for (index, event) in events.iter().take(3).enumerate() {
        assert_eq!(event["params"]["decoded"], json!(false), "event #{index}");
        assert_eq!(
            event["position"],
            Value::Null,
            "event #{index} has no position"
        );
    }
    // the copy that fits is still reported (it says what the buffer held), the overrun's entries
    // beyond the cut are not invented
    assert_eq!(events[0]["params"]["bytes"], json!(19));
    assert_eq!(events[0]["params"]["fields"].as_array().unwrap().len(), 1);
    assert!(events[1]["params"]["fields"].as_array().unwrap().is_empty());
    assert_eq!(events[2]["params"]["len"], json!(25));
    assert_eq!(
        events[3]["params"],
        json!({"unreadable": true}),
        "an unreadable pointer is the per-event unreadable marker (review M5), never plain null \
         on a via-11 event"
    );
    assert_eq!(events[3]["position"], Value::Null);
    assert_eq!(events[4]["params"], Value::Null);
    assert_eq!(events[4]["position"], Value::Null);
    // a within filter cannot use any of them: they must not match, and the reason is noted
    let out = server
        .call(json!({"filter": {"within": {"center": [944, 160, 64], "radius": 1}}}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(0));
    assert_eq!(out["unmatched"], json!(5));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n
            .as_str()
            .unwrap_or("")
            .contains("no decoded position and no non-null")),
        "{notes:?}"
    );
}

// ---------------------------------------------------------------- local filters

#[tokio::test]
async fn within_prefers_the_position_then_a_non_zero_origin_and_counts_the_rest() {
    // the position (100, 0, 64) as f32 LE, and a far position (944, 160, 64)
    const AT_CENTER: &str = "4dce7c5a0a0c000000c8420000000000008042";
    let center = json!({"center": [100, 0, 64], "radius": 20});
    let mb = sound_mock(reply(json!([
        // its decoded position is the center, its origin is the worldent's zero
        point_event("w.pos.in", 1, params(19, AT_CENTER, false)),
        // no position at all: the non-zero origin is 10 units from the center
        {"seq": 2, "name": "w.origin.in", "via": 12, "ageMs": 2, "origin": [110.0, 0.0, 64.0]},
        // a zero origin (the live worldent shape) can never match
        {"seq": 3, "name": "w.zero", "via": 12, "ageMs": 2, "origin": [0.0, 0.0, 0.0]},
        // a null origin can never match either
        {"seq": 4, "name": "w.null", "via": 12, "ageMs": 2, "origin": Value::Null},
        // no origin key at all (a bridge without the scope)
        {"seq": 5, "name": "w.no.origin", "via": 12, "ageMs": 2},
        // a position OUTSIDE beats an origin INSIDE: the position is what is asked for
        point_event("w.pos.out", 6, params(19, CAPTURED, false)),
    ])))
    .start()
    .await
    .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server
        .call(json!({"filter": {"within": center}}))
        .await
        .unwrap();
    let matched: Vec<&str> = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        matched,
        vec!["w.pos.in", "w.origin.in"],
        "position first, origin second"
    );
    assert_eq!(out["matched"], json!(2));
    assert_eq!(out["unmatched"], json!(4));
    assert_eq!(
        out["matched"].as_u64().unwrap() + out["unmatched"].as_u64().unwrap(),
        6,
        "every drained event is counted"
    );
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n
            .as_str()
            .unwrap_or("")
            .contains("3 event(s) had no decoded position")),
        "the three unlocatable events are noted: {notes:?}"
    );

    // a sphere around the far position keeps exactly the event whose position is there
    let out = server
        .call(json!({"filter": {"within": {"center": [944, 160, 64], "radius": 1}}}))
        .await
        .unwrap();
    let matched: Vec<&str> = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        matched,
        vec!["w.pos.out"],
        "the position decides, not the origin"
    );
    assert_eq!(out["matched"], json!(1));
    assert_eq!(out["unmatched"], json!(5));
}

#[tokio::test]
async fn name_prefix_and_names_filter_locally_after_the_drain() {
    let events = json!([
        {"seq": 1, "name": "mcp.snd.point", "via": 11, "ageMs": 1},
        {"seq": 2, "name": "mcp.snd.timer", "via": 11, "ageMs": 1},
        {"seq": 3, "name": "Buttons.snd9", "via": 11, "ageMs": 1},
    ]);
    let mb = sound_mock(reply(events)).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);

    let out = server
        .call(json!({"filter": {"namePrefix": "mcp.snd."}}))
        .await
        .unwrap();
    let names: Vec<&str> = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["mcp.snd.point", "mcp.snd.timer"]);
    assert_eq!(
        (out["matched"].clone(), out["unmatched"].clone()),
        (json!(2), json!(1))
    );

    let out = server
        .call(json!({"filter": {"names": ["Buttons.snd9", "mcp.snd.point"]}}))
        .await
        .unwrap();
    let names: Vec<&str> = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["mcp.snd.point", "Buttons.snd9"],
        "drain order is kept"
    );
    assert_eq!(
        (out["matched"].clone(), out["unmatched"].clone()),
        (json!(2), json!(1))
    );

    // names are exact and case-sensitive (the engine hashes them)
    let out = server
        .call(json!({"filter": {"names": ["mcp.snd.POINT"]}}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(0));
    assert!(out["events"].as_array().unwrap().is_empty());

    // prefix + names are ANDed
    let out = server
        .call(json!({"filter": {"namePrefix": "mcp.", "names": ["mcp.snd.timer"]}}))
        .await
        .unwrap();
    assert_eq!(out["matched"], json!(1));
    assert_eq!(out["events"][0]["name"], json!("mcp.snd.timer"));
    // every filtered-out event was still consumed from the ring: one drain per call
    assert_eq!(mb.method_calls("sound.drain").len(), 4);
}

// ---------------------------------------------------------------- installOnly

#[tokio::test]
async fn install_only_sends_sound_status_and_nothing_else() {
    let mb = MockBridge::builder()
        .methods(["sound.drain", "sound.status"])
        .responder_value("sound.drain", reply(json!([])))
        .responder_value(
            "sound.status",
            json!({"installed": true, "mode": "wrap", "pending": 0, "total": 0, "dropped": 0,
                   "ringSize": 256}),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({"installOnly": true})).await.unwrap();
    assert_eq!(out["installOnly"], json!(true));
    assert_eq!(out["installed"], json!(true));
    assert_eq!(out["mode"], json!("wrap"));
    assert_eq!(out["ringSize"], json!(256));
    assert!(out["notes"].as_array().unwrap().len() >= 2);

    let status = mb.method_calls("sound.status");
    assert_eq!(status.len(), 1, "exactly one sound.status");
    assert_eq!(
        status[0]
            .params
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["deadlineMs"],
        "no max/resolve/filter is sent with sound.status: {}",
        status[0].params
    );
    assert!(
        mb.method_calls("sound.drain").is_empty(),
        "installOnly must not drain"
    );
    assert_eq!(sound_requests(&mb), vec!["sound.status"]);

    // max/resolve with installOnly are ignored with a note, never a drain
    let out = server
        .call(json!({"installOnly": true, "max": 8, "resolve": false}))
        .await
        .unwrap();
    assert_eq!(out["installOnly"], json!(true));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("max/resolve are ignored")),
        "{notes:?}"
    );
    assert!(mb.method_calls("sound.drain").is_empty());
    assert_eq!(mb.method_calls("sound.status").len(), 2);
}

#[tokio::test]
async fn install_only_reports_an_unarmed_wrap() {
    let mb = MockBridge::builder()
        .methods(["sound.drain", "sound.status"])
        .responder_value(
            "sound.status",
            json!({"installed": false, "mode": Value::Null, "pending": 0, "total": 0,
                   "dropped": 0, "ringSize": 256}),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let out = server.call(json!({"installOnly": true})).await.unwrap();
    assert_eq!(out["installed"], json!(false));
    let notes = out["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("NOT armed")),
        "an unarmed wrap must not read as a success: {notes:?}"
    );
}

// ---------------------------------------------------------------- negatives

#[tokio::test]
async fn a_bridge_without_sound_drain_is_not_implemented_and_sends_nothing() {
    let mb = MockBridge::builder()
        .methods(["sound.status"])
        .responder_value("sound.drain", reply(json!([{"name": "not.reached"}])))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let error = error_of(server.call(json!({})).await);
    assert_eq!(error.name, "NotImplemented");
    assert_eq!(error.data["missing"], json!(["sound.drain"]));
    assert_eq!(error.code, Some(-32011));
    assert!(error.retry_safe);
    assert!(
        mb.method_calls("sound.drain").is_empty(),
        "nothing was sent"
    );
    assert!(sound_requests(&mb).is_empty(), "no sound.* request at all");
}

#[tokio::test]
async fn max_out_of_bounds_is_refused_with_nothing_sent() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    for bad in [json!(0), json!(257), json!(-1), json!(1.5), json!("8")] {
        let error = error_of(server.call(json!({"max": bad})).await);
        assert_eq!(error.type_, "InvalidArguments", "max {bad}");
        assert!(error.retry_safe);
        assert!(
            error.message.contains("max"),
            "the message names the argument: {}",
            error.message
        );
    }
    assert!(
        sound_requests(&mb).is_empty(),
        "an argument failure dispatches nothing: {:?}",
        sound_requests(&mb)
    );
}

#[tokio::test]
async fn a_non_positive_within_radius_is_refused() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    for bad in [json!(0), json!(-5), json!(0.0)] {
        let error = error_of(
            server
                .call(json!({"filter": {"within": {"center": [0, 0, 0], "radius": bad}}}))
                .await,
        );
        assert_eq!(error.type_, "InvalidArguments", "radius {bad}");
        assert!(error.message.contains("radius"), "{}", error.message);
    }
    // a `center` the filter cannot use is refused too
    let error = error_of(
        server
            .call(json!({"filter": {"within": {"center": [0, 0], "radius": 10}}}))
            .await,
    );
    assert_eq!(error.type_, "InvalidArguments");
    assert!(sound_requests(&mb).is_empty());
}

#[tokio::test]
async fn names_above_the_cap_and_ill_typed_entries_are_refused() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    let too_many: Vec<String> = (0..33).map(|i| format!("e.{i}")).collect();
    let error = error_of(server.call(json!({"filter": {"names": too_many}})).await);
    assert_eq!(error.type_, "InvalidArguments");
    assert!(error.message.contains("32"), "{}", error.message);
    let error = error_of(server.call(json!({"filter": {"names": ["ok", 7]}})).await);
    assert_eq!(error.type_, "InvalidArguments");
    let error = error_of(server.call(json!({"filter": {"names": []}})).await);
    assert_eq!(error.type_, "InvalidArguments");
    // 32 is fine
    let ok: Vec<String> = (0..32).map(|i| format!("e.{i}")).collect();
    server.call(json!({"filter": {"names": ok}})).await.unwrap();
    assert_eq!(mb.method_calls("sound.drain").len(), 1);
}

#[tokio::test]
async fn unknown_arguments_are_refused_at_the_top_level_and_nested() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    for args in [
        json!({"nope": 1}),
        json!({"filter": {"nope": 1}}),
        json!({"filter": {"within": {"center": [0, 0, 0], "radius": 1, "nope": 1}}}),
        json!({"filter": "mcp."}),
        json!({"resolve": "yes"}),
        json!({"installOnly": 1}),
    ] {
        let error = error_of(server.call(args.clone()).await);
        assert_eq!(error.type_, "InvalidArguments", "{args}");
    }
    assert!(sound_requests(&mb).is_empty());
}

#[tokio::test]
async fn filter_with_install_only_is_refused_with_nothing_sent() {
    let mb = sound_mock(reply(json!([]))).start().await.unwrap();
    let server = Server::new(&mb.discovery_path);
    let error = error_of(
        server
            .call(json!({"installOnly": true, "filter": {"namePrefix": "mcp."}}))
            .await,
    );
    assert_eq!(error.type_, "InvalidArguments");
    assert!(error.message.contains("installOnly"), "{}", error.message);
    assert!(sound_requests(&mb).is_empty());
}

#[tokio::test]
async fn a_prerequisite_failed_passes_through_unchanged() {
    let mb = MockBridge::builder()
        .methods(["sound.drain", "sound.status"])
        .error(
            "sound.drain",
            -32017,
            "PrerequisiteFailed",
            Some(json!({"reason": "SoundOpSystem001 is not the pinned CSoundOpSystem"})),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let error = error_of(server.call(json!({})).await);
    assert_eq!(error.code, Some(-32017));
    assert_eq!(error.name, "PrerequisiteFailed");
    assert_eq!(error.wire_name.as_deref(), Some("PrerequisiteFailed"));
    assert_eq!(error.type_, "PrerequisiteFailed");
    assert_eq!(
        error.data["reason"],
        json!("SoundOpSystem001 is not the pinned CSoundOpSystem"),
        "the bridge's data is not rewritten"
    );
}

#[tokio::test]
async fn a_reply_without_events_is_an_internal_error_never_an_empty_success() {
    let mb = sound_mock(json!({"truncated": false, "dropped": 0, "total": 0, "pending": 0}))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let error = error_of(server.call(json!({})).await);
    assert_eq!(error.type_, "InternalError");
    assert!(
        error.message.contains("events") && error.message.contains("truncated"),
        "the message names the shape it got: {}",
        error.message
    );
    assert!(!error.retry_safe, "an unreadable reply is not retry-safe");

    // an events key of the wrong type is the same failure
    let mb = sound_mock(json!({"events": "mcp.snd.point"}))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path);
    let error = error_of(server.call(json!({})).await);
    assert_eq!(error.type_, "InternalError");
    assert!(error.message.contains("events"), "{}", error.message);
}
