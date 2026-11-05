//! Mock semantics of the Hammer entity world (WP-M, plan §3 + §4.1) against the real
//! `BridgeClient` (pattern of `tests/pet_mock.rs` / `tests/modeldoc_mock.rs`): result shapes
//! and error codes mirror `bridge/mcp_bridge.c`'s `op_entity_create` / `op_set_keyvalues` /
//! `edit.batch` / `node.*` / `undo.undo` / `map.save` / `map.reload` / `doc.active`.
use cs2_mcp_common::bridge::{BridgeClient, BridgeError, ClientOptions, RequestOpts};
use cs2_mcp_common::mock::{make_doc, MockBridge, MockNode, HAMMER_METHODS};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn client(mb: &MockBridge) -> BridgeClient {
    BridgeClient::new(ClientOptions {
        discovery_path: mb.discovery_path.clone(),
        ..Default::default()
    })
}

fn kv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `expectDoc` against the mock's fingerprint (`d1`, epoch E).
fn opts(epoch: i64) -> RequestOpts {
    RequestOpts {
        expect_doc: Some(json!({"id": "d1", "epoch": epoch})),
        ..Default::default()
    }
}

async fn call_req(mb: &MockBridge, method: &str, params: Value, opts: RequestOpts) -> Value {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    c.request(method, params, opts)
        .await
        .expect("request")
        .result
}

async fn call(mb: &MockBridge, method: &str, params: Value) -> Value {
    call_req(mb, method, params, RequestOpts::default()).await
}

async fn call_err_req(mb: &MockBridge, method: &str, params: Value, opts: RequestOpts) -> BridgeError {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    c.request(method, params, opts)
        .await
        .expect_err("expected a wire error")
}

async fn call_err(mb: &MockBridge, method: &str, params: Value) -> BridgeError {
    call_err_req(mb, method, params, RequestOpts::default()).await
}

async fn doc(mb: &MockBridge) -> Value {
    let mut c = client(mb);
    c.connect(false).await.unwrap();
    // the bridge answers {"doc": {...}} (mcp_bridge.c doc.active arm)
    c.doc_active(None).await.unwrap()["doc"].clone()
}

/// The world every entity test starts with: nodeID 1 worldspawn.
async fn start_world(nodes: Vec<MockNode>) -> MockBridge {
    MockBridge::builder().hammer_nodes(nodes).start().await.unwrap()
}

// ---------------------------------------------------------------- advertisement

/// m2.17: every undoable write's reply carries the document state it left behind
/// (`doc: {id, topSerial, undoDepth}`, read inside the same dispatch as the op) — the state
/// hammer-mcp's mesh cache commits a write against instead of making a second `doc.active` read
/// (review-4 R4-1 step 3, the L2 window review-5 closed). One write, one group: the depth advances
/// by exactly one and each group takes a fresh serial.
#[tokio::test]
async fn undoable_write_replies_carry_the_document_state() {
    let mut e = MockNode::entity(9, "info_target", [0.0; 3], BTreeMap::new());
    e.keyvalues = kv(&[("targetname", "kv9")]);
    e.targetname = Some("kv9".into());
    let mb = start_world(vec![MockNode::world(), e]).await;

    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 9, "values": {"targetname": "kv9b"}}),
        opts(epoch(&mb).await),
    )
    .await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 1, "undoDepth": 1}),
        "{r}"
    );

    let r = call_req(
        &mb,
        "entity.create",
        json!({"class": "logic_relay", "origin": [1, 2, 3]}),
        opts(epoch(&mb).await),
    )
    .await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 2, "undoDepth": 2}),
        "{r}"
    );
    let created = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.classname == "logic_relay")
        .expect("the create added the relay")
        .node_id;

    let r = call_req(
        &mb,
        "edit.batch",
        json!({"ops": [
            {"method": "entity.create", "params": {"class": "info_target", "origin": [4, 5, 6]}},
            {"method": "entity.setKeyValues", "params": {"handle": "$0", "values": {"targetname": "batched"}}}
        ]}),
        opts(epoch(&mb).await),
    )
    .await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 3, "undoDepth": 3}),
        "a batch is one group, so one step: {r}"
    );

    let r = call_req(&mb, "node.delete", json!({"nodeIDs": [created]}), opts(epoch(&mb).await)).await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 4, "undoDepth": 4}),
        "{r}"
    );

    // a mesh write carries it too (the three `geo_dispatch` mutators)
    let r = call_req(
        &mb,
        "mesh.create",
        json!({"vertices": [[0, 0, 0], [64, 0, 0], [64, 64, 0], [0, 64, 0]],
               "faces": [[0, 1, 2, 3]],
               "material": "materials/dev/reflectivity_30.vmat"}),
        opts(epoch(&mb).await),
    )
    .await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 5, "undoDepth": 5}),
        "{r}"
    );

    // undo.undo reports the state AFTER its pop (depth one lower, the new top group's serial) —
    // and the empty-stack branch carries the tail as well (asserted with the rest of its shape in
    // create_undo_restores_nodes_and_empty_stack_shape)
    let r = call_req(&mb, "undo.undo", json!({}), opts(epoch(&mb).await)).await;
    assert_eq!(
        r["doc"],
        json!({"id": "d1", "topSerial": 4, "undoDepth": 4}),
        "{r}"
    );

    // an older bridge's reply (set_write_doc(false)) has no tail at all
    mb.set_hammer_write_doc(false);
    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 9, "values": {"targetname": "kv9c"}}),
        opts(epoch(&mb).await),
    )
    .await;
    assert!(r.get("doc").is_none(), "an older bridge sends no doc: {r}");
}

/// The eighteen built-in methods are advertised with `.hammer_nodes(..)`; without it none of the
/// new entity methods is advertised and the wire answers them with MethodNotFound.
#[tokio::test]
async fn hammer_world_advertises_the_eighteen_methods() {
    let mb = start_world(vec![MockNode::world()]).await;
    let mut c = client(&mb);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    let methods = caps["methods"].as_array().unwrap();
    for m in HAMMER_METHODS {
        assert!(methods.contains(&json!(m)), "{m} not advertised");
        assert!(c.has(m), "{m} gated out client-side");
    }

    let mb_plain = MockBridge::start().await.unwrap();
    let mut c = client(&mb_plain);
    c.connect(false).await.unwrap();
    let caps = c.capabilities().await.unwrap();
    // without a hammer world the baseline fixture list is exactly the recorded set
    let fixture: Vec<Value> = [
        "control.ping", "control.status", "control.capabilities", "control.version",
        "control.cancel", "control.shutdown", "doc.active", "map.save", "undo.undo",
        "entity.create", "entity.setKeyValues", "edit.batch",
    ]
    .iter()
    .map(|m| json!(m))
    .collect();
    assert_eq!(caps["methods"], json!(fixture), "baseline unchanged");
    for m in ["entity.get", "entity.find", "node.list", "node.delete"] {
        assert!(
            !caps["methods"].as_array().unwrap().contains(&json!(m)),
            "{m} advertised without a hammer world"
        );
        let err = c
            .request(m, json!({"nodeID": 1}), RequestOpts::default())
            .await
            .expect_err("unadvertised method must answer MethodNotFound");
        assert_eq!(err.code, Some(-32601), "{err:?}");
    }
    // the baseline hammer methods stay untouched
    assert!(caps["methods"].as_array().unwrap().contains(&json!("doc.active")));
}

/// Sanity: the disabled world is exactly `Option<HammerWorld>::None` semantics — doc.active
/// still serves the fixture doc.
#[tokio::test]
async fn hammer_doc_active_serves_the_world_fingerprint() {
    let mb = start_world(vec![MockNode::world()]).await;
    let d = doc(&mb).await;
    assert_eq!(d["id"], json!("d1"));
    assert_eq!(d["epoch"], json!(1));
    assert_eq!(d["dirty"], json!(false));
    assert_eq!(d["docType"], json!("vmap"));
    assert_eq!(d["undoName"], json!(null));
    assert_eq!(d["undoDepth"], json!(0));
    assert_eq!(d["topSerial"], json!(0));
    assert!(d["path"].as_str().is_some_and(|p| p.contains("probe.vmap")));
}

// ---------------------------------------------------------------- entity.create

#[tokio::test]
async fn entity_create_shape_coercion_and_next_node_id() {
    let mb = start_world(vec![MockNode::world()]).await;
    let r = call_req(
        &mb,
        "entity.create",
        json!({"class": "info_target", "origin": [400.0, 300.0, 64.0],
               "angles": [0, 90, 0],
               "keyValues": {"targetname": "mcp_t", "spawnflags": 4, "buying": true}}),
        opts(1),
    )
    .await;
    assert_eq!(r["handle"], json!("n1"), "{r}");
    assert_eq!(r["class"], json!("info_target"));
    assert_eq!(r["origin"], json!([400.0, 300.0, 64.0]));
    // the create reply names the node it made (review W2 acceptance 3a: the handle is batch-scoped
    // and the id is what every other read addresses the entity by)
    assert_eq!(r["nodeID"], json!(2), "{r}");
    assert_eq!(r["undoLabel"], json!("MCP: create entity"));
    let applied = r["keyValues"]["applied"].as_object().unwrap();
    assert_eq!(applied.len(), 3);
    for (_, ok) in applied {
        assert_eq!(ok, &json!(true), "{applied:?}");
    }
    assert_eq!(r["keyValues"]["warnings"], json!([]));

    // state: the next nodeID after the world, everything stored as text
    let nodes = mb.hammer_nodes();
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    let e = &nodes[1];
    assert_eq!(e.node_id, 2);
    assert_eq!(e.class, "entity");
    assert_eq!(e.classname, "info_target");
    assert_eq!(e.targetname.as_deref(), Some("mcp_t"));
    assert_eq!(e.keyvalues.get("targetname").map(String::as_str), Some("mcp_t"));
    assert_eq!(e.keyvalues.get("spawnflags").map(String::as_str), Some("4"));
    assert_eq!(e.keyvalues.get("buying").map(String::as_str), Some("1"));
    assert_eq!(e.angles, [0.0, 90.0, 0.0]);

    // the fingerprint moved: dirty + epoch 2, the create is on the undo stack
    let d = doc(&mb).await;
    assert_eq!(d["epoch"], json!(2));
    assert_eq!(d["dirty"], json!(true));
    assert_eq!(d["undoName"], json!("MCP: create entity"));
    assert_eq!(d["undoDepth"], json!(1));
}

/// Negative: unknown class -> -32006 UnknownClass, no node created, nothing mutated.
#[tokio::test]
async fn entity_create_unknown_class() {
    let mb = start_world(vec![MockNode::world()]).await;
    let err = call_err_req(
        &mb,
        "entity.create",
        json!({"class": "not_a_class", "origin": [0, 0, 0]}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32006), "{err:?}");
    assert_eq!(err.data["reason"], json!("not in the CMapNodeFactory registry"));
    assert_eq!(mb.hammer_nodes().len(), 1, "nothing must be created");
    let d = doc(&mb).await;
    assert_eq!(d["epoch"], json!(1), "failed create mutates nothing");
    assert_eq!(d["dirty"], json!(false));

    // missing class -> -32602
    let err = call_err_req(&mb, "entity.create", json!({}), opts(1)).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data["reason"], json!("class"));
}

// ---------------------------------------------------------------- entity.get

/// m2.18 §2 (WP-R1): a create-time `scales` array is a scale write like `node.setTransform`'s —
/// the reply echoes the array the entity vslot applied and the world stores it in the
/// object-properties map (`hammer_scales`); the pre-create undo group removes the stored scales
/// again when the group is popped (the read-back default is the identity).
#[tokio::test]
async fn entity_create_scales_store_into_the_object_properties() {
    let mb = start_world(vec![MockNode::world()]).await;
    let r = call_req(
        &mb,
        "entity.create",
        json!({"class": "prop_dynamic", "origin": [1, 2, 3], "scales": [2.0, 1.5, 2.0],
               "keyValues": {"model": "models/props/fixture/chair.vmdl"}}),
        opts(1),
    )
    .await;
    assert_eq!(r["scales"], json!([2.0, 1.5, 2.0]), "{r}");
    assert_eq!(r["nodeID"], json!(2), "{r}");
    assert_eq!(mb.hammer_nodes()[1].node_id, 2);
    assert_eq!(mb.hammer_scales(2), [2.0, 1.5, 2.0]);
    // entity.get reports the stored scales (review W2 #4/3c: the mock's read used to omit the
    // field although it tracked node_scales; m2.18's read reports it, and list_props live uses it)
    let read = call(&mb, "entity.get", json!({"nodeID": 2})).await;
    assert_eq!(read["scales"], json!([2.0, 1.5, 2.0]), "{read}");
    // undoing the scaled create removes the stored scales with the node (pre-create snapshot)
    let undone = call_req(&mb, "undo.undo", json!({}), opts(epoch(&mb).await)).await;
    assert_eq!(undone["undone"], json!(true), "{undone}");
    assert_eq!(
        mb.hammer_scales(2),
        [1.0, 1.0, 1.0],
        "the map entry is gone"
    );
    assert_eq!(mb.hammer_nodes().len(), 1, "the scaled node is gone");
    // a create without scales stores nothing: hammer_scales reads back the identity and the read
    // reports the identity the field holds. The reply echoes `scales` unconditionally — the real
    // bridge sends `[1.0, 1.0, 1.0]` when the request had none (protocol-diff §7, review w2b #6).
    let r = call_req(
        &mb,
        "entity.create",
        json!({"class": "prop_static", "origin": [4, 5, 6]}),
        opts(epoch(&mb).await),
    )
    .await;
    assert_eq!(r["nodeID"], json!(2), "{r}");
    assert_eq!(r["scales"], json!([1.0, 1.0, 1.0]), "{r}");
    let read = call(&mb, "entity.get", json!({"nodeID": 2})).await;
    assert_eq!(read["scales"], json!([1.0, 1.0, 1.0]), "{read}");
    assert_eq!(mb.hammer_scales(2), [1.0, 1.0, 1.0]);
}

// ---------------------------------------------------------------- entity.get (reads)

/// The m2.15 live read shape (plan §3.1), plus the required non-entity and not-found negs.
#[tokio::test]
async fn entity_get_shape_and_negatives() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_map_parameters", [100.0, 200.0, 300.0], kv(&[("bombradius", "500")])),
        MockNode {
            node_id: 41,
            class: "mesh".into(),
            classname: "mesh".into(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
    ])
    .await;
    let r = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    assert_eq!(r["nodeID"], json!(30));
    assert_eq!(r["classname"], json!("info_map_parameters"));
    assert_eq!(r["targetname"], json!(null));
    assert_eq!(r["origin"], json!([100.0, 200.0, 300.0]));
    assert_eq!(r["angles"], json!([0.0, 0.0, 0.0]));
    assert_eq!(r["keyValues"], json!({"bombradius": "500"}));
    assert_eq!(r["keyValueCount"], json!(1));
    assert_eq!(r["unsupported"], json!([]));
    // m2.18: the live connection vector (an entity without connections answers an empty array;
    // the wire used to hard-code `null`, re-props.md Q3)
    assert_eq!(r["connections"], json!([]));
    assert_eq!(r["source"], json!("live"));

    // the world is not an entity, a mesh is not an entity
    for id in [1, 41] {
        let err = call_err(&mb, "entity.get", json!({"nodeID": id})).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
        assert_eq!(err.data["reason"], json!("not an entity"), "nodeID {id}");
    }
    let err = call_err(&mb, "entity.get", json!({"nodeID": 999})).await;
    assert_eq!(err.code, Some(-32022), "{err:?}");
    assert_eq!(err.data["reason"], json!("nodeID 999 not found"));
    let err = call_err(&mb, "entity.get", json!({})).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
}

// ---------------------------------------------------------------- entity.setKeyValues

#[tokio::test]
async fn entity_set_keyvalues_nodeid_form() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "t0"), ("bombradius", "500")])),
    ])
    .await;
    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {"bombradius": 501, "new_key": 9, "targetname": "t1", "flag": false}}),
        opts(1),
    )
    .await;
    assert_eq!(
        r["applied"],
        json!({"bombradius": true, "new_key": true, "targetname": true, "flag": true}),
        "{r}"
    );
    let prev = r["previous"].as_object().unwrap();
    assert_eq!(prev.get("bombradius"), Some(&json!("500"))); // attribute existed
    assert_eq!(prev.get("new_key"), Some(&json!(null)));     // attribute did not exist
    assert_eq!(prev.get("flag"), Some(&json!(null)));
    // the default world is the m2.18 shape (review w2b #5); the group-only pre-m2.18 pin is
    // `keyvalue_undo_leaves_the_value_changed`
    assert_eq!(r["valueUndo"], json!("recorded"), "{r}");
    assert_eq!(r["warnings"], json!([]));

    let nodes = mb.hammer_nodes();
    let e = nodes.iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(e.keyvalues.get("bombradius").map(String::as_str), Some("501"));
    assert_eq!(e.keyvalues.get("new_key").map(String::as_str), Some("9"));
    assert_eq!(e.keyvalues.get("flag").map(String::as_str), Some("0"));
    assert_eq!(e.targetname.as_deref(), Some("t1"));

    let d = doc(&mb).await;
    assert_eq!(d["epoch"], json!(2));
    assert_eq!(d["undoName"], json!("MCP: set keyvalues"));
    assert_eq!(d["dirty"], json!(true));
}

#[tokio::test]
async fn entity_set_keyvalues_selector_and_target_negatives() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new()),
        MockNode {
            node_id: 41,
            class: "mesh".into(),
            classname: "mesh".into(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
    ])
    .await;
    // both selectors -> -32602
    let err = call_err_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "handle": "$0", "values": {}}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    // neither -> -32602
    let err = call_err_req(&mb, "entity.setKeyValues", json!({"values": {}}), opts(1)).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    // not found / not an entity / locked
    let err = call_err_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 999, "values": {}}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32022), "{err:?}");
    for id in [1, 41] {
        let err = call_err_req(
            &mb,
            "entity.setKeyValues",
            json!({"nodeID": id, "values": {}}),
            opts(1),
        )
        .await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
        assert_eq!(err.data["reason"], json!("not an entity"), "nodeID {id}");
    }
    // missing values
    let err = call_err_req(&mb, "entity.setKeyValues", json!({"nodeID": 30}), opts(1)).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(mb.method_calls("entity.setKeyValues").len(), 6);
}

/// A locked node refuses writes (-32018 DocumentNotEditable).
#[tokio::test]
async fn entity_set_keyvalues_locked_node() {
    let mut e = MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new());
    e.locked = true;
    let mb = start_world(vec![MockNode::world(), e]).await;
    let err = call_err_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {}}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32018), "{err:?}");
    assert_eq!(err.data["reason"], json!("nodeID 30 is locked"));

    // node.delete refuses a locked id the same way and deletes nothing
    let err = call_err_req(&mb, "node.delete", json!({"nodeIDs": [30]}), opts(1)).await;
    assert_eq!(err.code, Some(-32018), "{err:?}");
    assert_eq!(err.data["reason"], json!("nodeID 30 is locked"));
    assert_eq!(mb.hammer_nodes().len(), 2, "nothing must be deleted");
}

/// Required negative: a handle from a previous request (or any handle outside a batch)
/// answers -32005 StaleHandle with data.reason unknown_handle; inside a batch "$0" resolves.
#[tokio::test]
async fn handles_only_work_inside_their_batch() {
    let mb = start_world(vec![MockNode::world()]).await;
    let created = call_req(
        &mb,
        "entity.create",
        json!({"class": "info_target", "origin": [0, 0, 0]}),
        opts(1),
    )
    .await;
    let handle = created["handle"].as_str().unwrap().to_string();
    assert!(handle.starts_with('n'), "{created}");

    // standalone write on that (now stale) handle
    let err = call_err_req(
        &mb,
        "entity.setKeyValues",
        json!({"handle": handle, "values": {"x": "y"}}),
        opts(2),
    )
    .await;
    assert_eq!(err.code, Some(-32005), "{err:?}");
    assert_eq!(err.data["reason"], json!("unknown_handle"));

    // inside one batch, "$0" resolves to op 0's handle
    let r = call_req(
        &mb,
        "edit.batch",
        json!({
            "ops": [
                {"method": "entity.create", "params": {"class": "logic_relay", "origin": [1, 2, 3]}},
                {"method": "entity.setKeyValues", "params": {"handle": "$0", "values": {"targetname": "mcp_b"}}},
            ]
        }),
        opts(2),
    )
    .await;
    assert_eq!(r["failedIndex"], json!(null), "{r}");
    assert_eq!(r["results"].as_array().unwrap().len(), 2);
    assert!(r["results"][0]["handle"].as_str().unwrap().starts_with('n'));
    assert_eq!(r["results"][1]["applied"], json!({"targetname": true}));
    let nodes = mb.hammer_nodes();
    let t = nodes.iter().find(|n| n.targetname.as_deref() == Some("mcp_b"));
    assert!(t.is_some(), "{nodes:?}");
    assert_eq!(t.unwrap().classname, "logic_relay");

    // an unresolvable "$N" (beyond the issued handles) is stale too — the "$0" batch moved
    // the fingerprint twice (one mutation per op), so the doc object says epoch 4
    let err = call_err_req(
        &mb,
        "edit.batch",
        json!({"ops": [{"method": "entity.setKeyValues", "params": {"handle": "$3", "values": {}}}]}),
        opts(4),
    )
    .await;
    assert_eq!(err.code, Some(-32012), "{err:?}");
    assert_eq!(err.data["failedIndex"], json!(0));
    assert_eq!(err.data["results"][0]["error"]["code"], json!(-32005));
    assert_eq!(err.data["results"][0]["error"]["data"]["reason"], json!("unknown_handle"));
}

// ---------------------------------------------------------------- edit.batch

/// Required negative: unknown class at op 1 -> -32012, failedIndex 1, op 0's entity stays.
#[tokio::test]
async fn batch_partial_failure_stops_and_does_not_roll_back() {
    let mb = start_world(vec![MockNode::world()]).await;
    let err = call_err_req(
        &mb,
        "edit.batch",
        json!({"ops": [
            {"method": "entity.create", "params": {"class": "info_target", "origin": [5, 6, 7], "keyValues": {"targetname": "kept"}}},
            {"method": "entity.create", "params": {"class": "not_a_class", "origin": [0, 0, 0]}},
        ]}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32012), "{err:?}");
    let results = err.data["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "results run up to and include the failure");
    assert!(results[0].get("handle").is_some(), "{results:?}");
    assert_eq!(results[1]["error"]["code"], json!(-32006));
    assert_eq!(err.data["failedIndex"], json!(1));

    // no rollback: op 0's entity is still there with its keyvalues
    let nodes = mb.hammer_nodes();
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    let created = nodes.iter().find(|n| n.classname == "info_target").unwrap();
    assert_eq!(created.keyvalues.get("targetname").map(String::as_str), Some("kept"));
    assert_eq!(created.origin, [5.0, 6.0, 7.0]);
    // the group exists (something was applied)
    let d = doc(&mb).await;
    assert_eq!(d["undoName"], json!("MCP: batch"));
}

/// The m2.8 whitelist: anything but entity.create/entity.setKeyValues is a NotImplemented op.
#[tokio::test]
async fn batch_whitelist_and_ops_guard() {
    let mb = start_world(vec![MockNode::world()]).await;
    let err = call_err_req(
        &mb,
        "edit.batch",
        json!({"ops": [{"method": "node.list", "params": {}}]}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32012), "{err:?}");
    assert_eq!(err.data["failedIndex"], json!(0));
    assert_eq!(err.data["results"][0]["error"]["code"], json!(-32011));
    assert_eq!(err.data["results"][0]["error"]["message"], json!("NotImplemented"));

    let err = call_err_req(&mb, "edit.batch", json!({}), opts(1)).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data.len(), 1, "no data beyond the from_wire 'method' marker");
    assert_eq!(err.data["method"], json!("edit.batch"));
    assert_eq!(err.wire_name.as_deref(), Some("invalid params: ops"));
}

// ---------------------------------------------------------------- node.list / node.delete

#[tokio::test]
async fn node_list_shape_and_filters() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(5, "info_target", [0.0; 3], BTreeMap::new()),
        MockNode {
            node_id: 9,
            class: "mesh".into(),
            classname: "mesh".into(),
            targetname: None,
            origin: [1.0, 2.0, 3.0],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
    ])
    .await;
    let r = call(&mb, "node.list", json!({})).await;
    // the world itself is not listed (the bridge walks its children)
    assert_eq!(r["count"], json!(2), "{r}");
    assert_eq!(r["truncated"], json!(false));
    assert_eq!(r["source"], json!("live"));
    assert_eq!(r["nodes"][0]["nodeID"], json!(5));
    assert_eq!(r["nodes"][1]["class"], json!("mesh"));
    assert_eq!(r["nodes"][1]["origin"], json!([1.0, 2.0, 3.0]));
    assert_eq!(r["nodes"][1]["vertexSlots"], json!(0));
    assert_eq!(r["nodes"][1]["faceSlots"], json!(0));

    let r = call(&mb, "node.list", json!({"class": "mesh"})).await;
    assert_eq!(r["count"], json!(1));
    assert_eq!(r["nodes"][0]["nodeID"], json!(9));

    let r = call(&mb, "node.list", json!({"limit": 1})).await;
    assert_eq!(r["count"], json!(1));
    assert_eq!(r["truncated"], json!(true));
}

/// Required negative: node.delete of the world refuses -32602 and deletes nothing.
#[tokio::test]
async fn node_delete_world_refused() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(5, "info_target", [0.0; 3], BTreeMap::new()),
    ])
    .await;
    let err = call_err_req(&mb, "node.delete", json!({"nodeIDs": [1]}), opts(1)).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(
        err.data["reason"],
        json!("nodeID 1 cannot be deleted (world)")
    );
    assert_eq!(mb.hammer_nodes().len(), 2, "nothing must be deleted");
    let d = doc(&mb).await;
    assert_eq!(d["epoch"], json!(1), "a refused delete mutates nothing");
    assert_eq!(d["undoDepth"], json!(0));

    // missing id -> -32022, nothing deleted
    let err = call_err_req(&mb, "node.delete", json!({"nodeIDs": [5, 999]}), opts(1)).await;
    assert_eq!(err.code, Some(-32022), "{err:?}");
    assert_eq!(err.data["reason"], json!("nodeID 999 not found"));
    assert_eq!(mb.hammer_nodes().len(), 2);

    // guards on the params
    for bad in [json!({}), json!({"nodeIDs": []}), json!({"nodeIDs": [-1]})] {
        let err = call_err_req(&mb, "node.delete", bad, opts(1)).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
}

#[tokio::test]
async fn node_delete_removes_and_undo_restores() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(5, "info_target", [0.0; 3], BTreeMap::new()),
        MockNode::entity(6, "logic_relay", [1.0; 3], BTreeMap::new()),
    ])
    .await;
    let r = call_req(&mb, "node.delete", json!({"nodeIDs": [5, 6]}), opts(1)).await;
    assert_eq!(r["deleted"], json!([5, 6]), "{r}");
    assert_eq!(r["undoLabel"], json!("MCP: delete nodes"));
    assert_eq!(mb.hammer_nodes().len(), 1);
    let d = doc(&mb).await;
    assert_eq!(d["undoName"], json!("MCP: delete nodes"));
    assert_eq!(d["undoDepth"], json!(1));

    // one create/delete group restores the whole node snapshot
    let r = call_req(&mb, "undo.undo", json!({}), opts(2)).await;
    assert_eq!(r["undone"], json!(true));
    assert_eq!(r["name"], json!("MCP: delete nodes"));
    assert_eq!(r["undoDepth"], json!(0));
    let nodes = mb.hammer_nodes();
    assert_eq!(nodes.len(), 3, "{nodes:?}");
    assert_eq!(nodes.iter().find(|n| n.node_id == 6).unwrap().classname, "logic_relay");
}

// ---------------------------------------------------------------- entity.find

#[tokio::test]
async fn entity_find_filters_case_and_sphere_and_limit() {
    let near = [100.0, 0.0, 0.0];
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(5, "info_target", near, kv(&[("targetname", "alpha")])),
        MockNode::entity(6, "INFO_PLAYER_TERRORIST", near, kv(&[("targetname", "Alpha")])),
        MockNode::entity(7, "info_target", [0.0, 0.0, 90000.0], BTreeMap::new()),
        MockNode::entity(8, "func_buyzone", near, BTreeMap::new()),
        MockNode {
            node_id: 9,
            class: "mesh".into(),
            classname: "mesh".into(),
            targetname: None,
            origin: near,
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
    ])
    .await;
    // case-insensitive exact class and targetname; meshes are never found
    let r = call(&mb, "entity.find", json!({"class": "iNfO_tArGeT"})).await;
    assert_eq!(r["count"], json!(2), "{r}");
    assert!(r["entities"].as_array().unwrap().iter().all(|e| e["nodeID"].as_i64().unwrap() != 9));
    assert_eq!(r["source"], json!("live"));

    let r = call(&mb, "entity.find", json!({"targetname": "AlPhA"})).await;
    let hit_ids: Vec<i64> = r["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["nodeID"].as_i64().unwrap())
        .collect();
    assert_eq!(hit_ids, vec![5, 6], "case-insensitive exact targetname, {r}");

    // sphere on the origin: entity 7 lies outside
    let r = call(
        &mb,
        "entity.find",
        json!({"within": {"origin": [100, 0, 0], "radius": 10}}),
    )
    .await;
    assert_eq!(r["count"], json!(3), "5, 6, 8 are 0 away, 7 is ~90000: {r}");
    let r = call(
        &mb,
        "entity.find",
        json!({"within": {"origin": [0, 0, 0], "radius": 1}, "class": "info_target"}),
    )
    .await;
    assert_eq!(r["count"], json!(0), "no info_target within 1: {r}");

    // limit + truncated + visited (all six doc nodes: world + 5 candidates + mesh)
    let r = call(&mb, "entity.find", json!({"limit": 2})).await;
    assert_eq!(r["count"], json!(2));
    assert_eq!(r["truncated"], json!(true), "{r}");
    assert_eq!(r["visited"], json!(6));
}

// ---------------------------------------------------------------- undo

/// Required negative semantics (pre-m2.18 build, the knob off): undoing a keyvalue group pops
/// it WITHOUT restoring values (`valueUndo:"group-only"`, F2). The default (knob on) is the
/// m2.18 build — `keyvalue_undo_records_the_values` pins that instead.
#[tokio::test]
async fn keyvalue_undo_leaves_the_value_changed() {
    // the pre-m2.18 build pair (review w2b #5): "group-only" reply, no marker advertised
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(30, "info_target", [0.0; 3], kv(&[("bombradius", "500")])),
        ])
        .hammer_value_undo(false)
        .start()
        .await
        .unwrap();
    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {"bombradius": 501}}),
        opts(1),
    )
    .await;
    assert_eq!(r["valueUndo"], json!("group-only"));
    let r = call_req(&mb, "undo.undo", json!({}), opts(2)).await;
    assert_eq!(r["undone"], json!(true), "{r}");
    assert_eq!(r["name"], json!("MCP: set keyvalues"));
    assert_eq!(r["undoDepth"], json!(0));

    // F2 on purpose: the value is NOT restored
    let nodes = mb.hammer_nodes();
    let e = nodes.iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(
        e.keyvalues.get("bombradius").map(String::as_str),
        Some("501")
    );
    // the write stays recorded as unsaved work
    assert_eq!(doc(&mb).await["dirty"], json!(true));
}

/// The m2.18 shape (the knob's default `true`, review w2b #5): the reply says
/// `valueUndo:"recorded"` and popping the group REALLY restores the values (RecordNodeTree).
#[tokio::test]
async fn keyvalue_undo_records_the_values() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("bombradius", "500")])),
    ])
    .await;
    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {"bombradius": 501}}),
        opts(1),
    )
    .await;
    assert_eq!(r["valueUndo"], json!("recorded"), "{r}");
    let r = call_req(&mb, "undo.undo", json!({}), opts(2)).await;
    assert_eq!(r["undone"], json!(true), "{r}");
    assert_eq!(r["name"], json!("MCP: set keyvalues"));
    let nodes = mb.hammer_nodes();
    let e = nodes.iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(
        e.keyvalues.get("bombradius").map(String::as_str),
        Some("500"),
        "RecordNodeTree: the pop restored the pre-write value"
    );
}

/// A batch with **no** `entity.create` op is labelled `MCP: set keyvalues` (the bridge patches
/// this in 35528e8 via `batch_has_create`, decided on the listed ops before the group opens,
/// protocol-diff-m2.15.md item 16) and pops without restoring; a batch with a create keeps the
/// batch label and its snapshot restores.
#[tokio::test]
async fn batch_without_create_op_is_labelled_set_keyvalues() {
    // the pre-m2.18 build pair (review w2b #5): the popped set-only batch is a gap only there
    let mb = MockBridge::builder()
        .hammer_nodes(vec![
            MockNode::world(),
            MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "t0")])),
        ])
        .hammer_value_undo(false)
        .start()
        .await
        .unwrap();
    let r = call_req(
        &mb,
        "edit.batch",
        json!({"ops": [
            {"method": "entity.setKeyValues", "params": {"nodeID": 30, "values": {"targetname": "t1"}}},
            {"method": "entity.setKeyValues", "params": {"nodeID": 30, "values": {"new": "kv"}}},
        ]}),
        opts(1),
    )
    .await;
    assert_eq!(r["failedIndex"], json!(null), "{r}");
    // per-op results: op 0 applied only its own values (previous per op, not per batch)
    assert_eq!(r["results"][0]["previous"], json!({"targetname": "t0"}));
    assert_eq!(r["results"][1]["previous"].get("new"), Some(&json!(null)));
    let r = call_req(&mb, "undo.undo", json!({}), opts(3)).await;
    assert_eq!(r["name"], json!("MCP: set keyvalues"), "{r}");
    let e = mb
        .hammer_nodes()
        .into_iter()
        .find(|n| n.node_id == 30)
        .unwrap();
    assert_eq!(e.keyvalues.get("targetname").map(String::as_str), Some("t1"), "F2");
    // the keyvalue label is what the server's unsaved-edit counter leaves alone: the write stays
    // recorded as unsaved work (the mock's analogue is the dirty flag, still up after the undo)
    assert_eq!(doc(&mb).await["dirty"], json!(true));
    assert_eq!(e.keyvalues.get("new").map(String::as_str), Some("kv"));
}

#[tokio::test]
async fn create_undo_restores_nodes_and_empty_stack_shape() {
    let mb = start_world(vec![MockNode::world()]).await;
    call_req(&mb, "entity.create", json!({"class": "info_target", "origin": [0, 0, 0]}), opts(1)).await;
    call_req(&mb, "entity.create", json!({"class": "logic_relay", "origin": [1, 1, 1]}), opts(2)).await;
    assert_eq!(mb.hammer_nodes().len(), 3);

    let r = call_req(&mb, "undo.undo", json!({}), opts(3)).await;
    assert_eq!(r["undone"], json!(true));
    assert_eq!(r["name"], json!("MCP: create entity"));
    assert_eq!(r["undoDepth"], json!(1));
    // m2.17: every undoable write's reply carries the document state it left behind
    assert_eq!(r["doc"], json!({"id": "d1", "topSerial": 1, "undoDepth": 1}), "{r}");
    // the SNAPSHOT of the first-undo entry is "after create 1": the first entity stays
    let nodes = mb.hammer_nodes();
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    assert!(nodes.iter().any(|n| n.classname == "info_target"));
    assert!(!nodes.iter().any(|n| n.classname == "logic_relay"));

    let r = call_req(&mb, "undo.undo", json!({}), opts(4)).await;
    assert_eq!(r["undoDepth"], json!(0));
    assert_eq!(mb.hammer_nodes().len(), 1);

    // empty stack: the bridge's result shape ("undone":false,"name":null + the m2.17 `doc` tail) —
    // expectDoc still required first
    let err = call_err_req(&mb, "undo.undo", json!({}), RequestOpts::default()).await;
    assert_eq!(err.code, Some(-32004), "{err:?}");
    let r = call_req(&mb, "undo.undo", json!({}), opts(5)).await;
    assert_eq!(
        r,
        json!({"undone": false, "name": null, "doc": {"id": "d1", "topSerial": 0, "undoDepth": 0}}),
        "{r}"
    );
}

// ---------------------------------------------------------------- the expectDoc gate

/// Required negative: every mutator without expectDoc -> -32004 DocumentChanged reason missing.
#[tokio::test]
async fn mutators_without_expect_doc_are_refused() {
    let mut e = MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new());
    e.locked = true;
    let mb = start_world(vec![MockNode::world(), e]).await;
    let cases = [
        ("entity.create", json!({"class": "info_target", "origin": [0, 0, 0]})),
        ("entity.setKeyValues", json!({"nodeID": 30, "values": {}})),
        ("edit.batch", json!({"ops": []})),
        ("node.delete", json!({"nodeIDs": []})),
        ("undo.undo", json!({})),
    ];
    for (method, params) in cases {
        let err = call_err(&mb, method, params).await;
        assert_eq!(err.code, Some(-32004), "{method}: {err:?}");
        assert_eq!(err.data["reason"], json!("missing"), "{method}");
    }
    // nothing went through
    assert_eq!(mb.hammer_nodes().len(), 2);
}

/// The fingerprint parts: wrong epoch -> "epoch", wrong id -> "doc_id".
#[tokio::test]
async fn expect_doc_fingerprint_mismatch_reasons() {
    let mb = start_world(vec![MockNode::world()]).await;
    let wrong_epoch = RequestOpts {
        expect_doc: Some(json!({"id": "d1", "epoch": 7})),
        ..Default::default()
    };
    let err = call_err_req(&mb, "entity.create", json!({"class": "info_target"}), wrong_epoch).await;
    assert_eq!(err.code, Some(-32004), "{err:?}");
    assert_eq!(err.data["reason"], json!("epoch"));
    let wrong_id = RequestOpts {
        expect_doc: Some(json!({"id": "d2", "epoch": 1})),
        ..Default::default()
    };
    let err = call_err_req(&mb, "entity.create", json!({"class": "info_target"}), wrong_id).await;
    assert_eq!(err.code, Some(-32004), "{err:?}");
    assert_eq!(err.data["reason"], json!("doc_id"));
    let no_epoch = RequestOpts {
        expect_doc: Some(json!({"id": "d1"})),
        ..Default::default()
    };
    let err = call_err_req(&mb, "entity.create", json!({"class": "info_target"}), no_epoch).await;
    assert_eq!(err.data["reason"], json!("missing_epoch"));
    // the refusals mutated nothing
    assert_eq!(doc(&mb).await["epoch"], json!(1));
}

/// Epoch bumps per mutation; `doc.active` reports it without mutating.
#[tokio::test]
async fn epoch_bumps_per_mutation() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new()),
    ])
    .await;
    assert_eq!(doc(&mb).await["epoch"], json!(1));
    call_req(&mb, "entity.create", json!({"class": "logic_relay", "origin": [0, 0, 0]}), opts(1)).await;
    assert_eq!(doc(&mb).await["epoch"], json!(2));
    call_req(&mb, "entity.setKeyValues", json!({"nodeID": 30, "values": {"a": "b"}}), opts(2)).await;
    assert_eq!(doc(&mb).await["epoch"], json!(3));
    call_req(&mb, "node.delete", json!({"nodeIDs": [30]}), opts(3)).await;
    assert_eq!(doc(&mb).await["epoch"], json!(4));
    call_req(&mb, "undo.undo", json!({}), opts(4)).await;
    assert_eq!(doc(&mb).await["epoch"], json!(5));
    // the delete's snapshot is the pre-delete set (also contains the created relay)
    assert_eq!(mb.hammer_nodes().len(), 3);
}

// ---------------------------------------------------------------- map.save

#[tokio::test]
async fn map_save_clears_dirty_and_skips_clean() {
    let mb = start_world(vec![MockNode::world()]).await;
    call_req(&mb, "entity.create", json!({"class": "info_target", "origin": [0, 0, 0]}), opts(1)).await;
    let d = doc(&mb).await;
    assert_eq!(d["dirty"], json!(true));
    assert!(d["path"].as_str().is_some_and(|p| p.contains("probe.vmap")), "{d}");
    let r = call_req(&mb, "map.save", json!({}), opts(2)).await;
    assert_eq!(r["saved"], json!(true), "{r}");
    assert_eq!(r["skippedClean"], json!(false));
    assert!(r["path"].as_str().unwrap().contains(&"probe.vmap".to_string()), "{r}");
    assert_eq!(doc(&mb).await["dirty"], json!(false));
    // a clean document skips the write
    let r = call_req(&mb, "map.save", json!({}), opts(2)).await;
    assert_eq!(r["saved"], json!(true));
    assert_eq!(r["skippedClean"], json!(true));
}

/// `.hammer_new_file(true)` -> doc isActiveFile and map.save -32015 UntitledDocument.
#[tokio::test]
async fn map_save_untitled_document() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .hammer_new_file(true)
        .start()
        .await
        .unwrap();
    let d = doc(&mb).await;
    assert_eq!(d["isNewFile"], json!(true));
    assert_eq!(d["path"], json!(null));
    let err = call_err_req(&mb, "map.save", json!({}), opts(1)).await;
    assert_eq!(err.code, Some(-32015), "{err:?}");
    assert_eq!(err.data["reason"], json!("no path (Save-As is modal)"));
}

/// `.hammer_new_file(true)` -> `map.reload` -32015 UntitledDocument (FIX-3R M1 for the client-side
/// pre-check, FIX-3B M1 for the bridge arm; the mock mirrors the bridge arm, so the gate and its
/// `map.save` wording are pinned here).
#[tokio::test]
async fn map_reload_untitled_document() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .hammer_new_file(true)
        .start()
        .await
        .unwrap();
    let d = doc(&mb).await;
    assert_eq!(d["isNewFile"], json!(true));
    assert_eq!(d["path"], json!(null));

    for params in [json!({}), json!({"force": true})] {
        // even force does not unlock it: an untitled document has no file to reload from
        let err = call_err_req(&mb, "map.reload", params.clone(), opts(1)).await;
        assert_eq!(err.code, Some(-32015), "{params}: {err:?}");
        assert_eq!(err.data["reason"], json!("no path (Save-As is modal)"));
    }
    // nothing happened: the document is still untitled and clean
    let d = doc(&mb).await;
    assert_eq!(d["dirty"], json!(false));
    assert_eq!(d["undoDepth"], json!(0));
}

/// FIX-3B L2: `map.reload` carries the `expectDoc` gate every other mutator has. A caller whose
/// fingerprint moved (a tab switch between `doc.active` and the reload) is refused with `-32004
/// DocumentChanged` **before** the untitled/dirty gates, so `force:true` can never discard another
/// document's edits. Negative evidence: the document is untouched by every refusal (dirty, undo
/// depth and epoch all kept), and the matching fingerprint still reloads.
#[tokio::test]
async fn map_reload_with_a_stale_expect_doc_changes_nothing() {
    let mb = start_world(vec![MockNode::world()]).await;
    call_req(&mb, "entity.create", json!({"class": "info_target", "origin": [0, 0, 0]}), opts(1)).await;
    let d = doc(&mb).await;
    let epoch = d["epoch"].as_i64().unwrap();
    let undo = d["undoDepth"].as_i64().unwrap();
    assert_eq!(d["dirty"], json!(true), "setup: the created entity made it dirty");

    // a switch to another document, a stale epoch, and no `expectDoc` at all: all -32004
    let cases = [
        (json!({"id": "d9", "epoch": epoch}), "doc_id"),
        (json!({"id": "d1", "epoch": epoch - 1}), "epoch"),
    ];
    for (expect_doc, reason) in cases {
        let err = call_err_req(
            &mb,
            "map.reload",
            json!({"force": true}),
            RequestOpts { expect_doc: Some(expect_doc.clone()), ..Default::default() },
        )
        .await;
        assert_eq!(err.code, Some(-32004), "{expect_doc}: {err:?}");
        assert_eq!(err.data["reason"], json!(reason), "{expect_doc}: {err:?}");
        let after = doc(&mb).await;
        assert_eq!(after["dirty"], json!(true), "{expect_doc}: the refusal reloaded nothing");
        assert_eq!(after["undoDepth"], json!(undo), "{expect_doc}: the undo history survived");
        assert_eq!(after["epoch"], json!(epoch), "{expect_doc}: the fingerprint did not move");
    }
    let err = call_err_req(&mb, "map.reload", json!({"force": true}), RequestOpts::default()).await;
    assert_eq!(err.code, Some(-32004), "no expectDoc is DocumentChanged too: {err:?}");
    assert_eq!(err.data["reason"], json!("missing"));
    let after = doc(&mb).await;
    assert_eq!(after["dirty"], json!(true));
    assert_eq!(after["epoch"], json!(epoch));

    // the matching fingerprint reloads (the control case: the gate refuses only the mismatch)
    let r = call_req(&mb, "map.reload", json!({"force": true}), opts(epoch)).await;
    assert_eq!(r["reloaded"], json!(true), "{r}");
    assert_eq!(r["discardedChanges"], json!(true));
    let after = doc(&mb).await;
    assert_eq!(after["dirty"], json!(false));
    assert_eq!(after["undoDepth"], json!(0));
    assert!(after["epoch"].as_i64().unwrap() > epoch);
}

/// `map.reload` mirrors the bridge arm (`mcp_bridge.c` + live `bridge/test_reload.py`): a dirty
/// document refuses without `force` (-32016), a forced reload discards (dirty cleared, undo
/// history dropped, epoch bumped, `discardedChanges` = the dirty flag before the reload) and a
/// clean document reloads with `discardedChanges:false`.
#[tokio::test]
async fn map_reload_refuses_dirty_without_force_and_reloads_with_it() {
    let mb = start_world(vec![MockNode::world()]).await;
    call_req(&mb, "entity.create", json!({"class": "info_target", "origin": [0, 0, 0]}), opts(1)).await;
    let d = doc(&mb).await;
    let before_epoch = d["epoch"].as_i64().unwrap();
    assert_eq!(d["dirty"], json!(true));
    let undo = d["undoDepth"].as_i64().unwrap();

    // dirty without force -> -32016 UnsavedChanges, document state untouched
    let err = call_err_req(&mb, "map.reload", json!({}), opts(before_epoch)).await;
    assert_eq!(err.code, Some(-32016), "{err:?}");
    assert_eq!(err.data["reason"], json!("document is dirty; pass force:true to discard"));
    let d = doc(&mb).await;
    assert_eq!(d["dirty"], json!(true), "the refusal discards nothing");
    assert_eq!(d["undoDepth"], json!(undo));
    assert_eq!(d["epoch"], json!(before_epoch));

    // force:true -> reload, discardedChanges:true, clean, undo dropped, epoch bumped
    let r = call_req(&mb, "map.reload", json!({"force": true}), opts(before_epoch)).await;
    assert_eq!(r["reloaded"], json!(true), "{r}");
    assert_eq!(r["discardedChanges"], json!(true));
    assert!(r["path"].as_str().is_some_and(|p| p.contains("probe.vmap")), "{r}");
    let d = doc(&mb).await;
    assert_eq!(d["dirty"], json!(false));
    assert_eq!(d["undoDepth"], json!(0), "ReloadFromDisk drops the undo history");
    assert!(d["epoch"].as_i64().unwrap() > before_epoch, "the reload bumps the fingerprint epoch");
    let after = r["epoch"].as_i64().unwrap();
    assert_eq!(after, d["epoch"].as_i64().unwrap());
    let after_epoch = d["epoch"].as_i64().unwrap();

    // a clean document reloads with discardedChanges:false
    let r = call_req(&mb, "map.reload", json!({}), opts(after_epoch)).await;
    assert_eq!(r["reloaded"], json!(true), "{r}");
    assert_eq!(r["discardedChanges"], json!(false));
}

/// `map.reload` is gated on editable first, exactly like the bridge arm (the `-32018` here is with
/// *no* `expectDoc`, so it also pins that editable is checked before the fingerprint gate).
#[tokio::test]
async fn map_reload_on_a_not_editable_document_is_document_not_editable() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .hammer_editable(false)
        .start()
        .await
        .unwrap();
    let err = call_err(&mb, "map.reload", json!({})).await;
    assert_eq!(err.code, Some(-32018), "{err:?}");
    assert_eq!(err.data["reason"], json!("not_editable"));
}

#[tokio::test]
async fn hammer_editable_false_gates_every_mutator() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world(), MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new())])
        .hammer_editable(false)
        .start()
        .await
        .unwrap();
    let d = doc(&mb).await;
    assert_eq!(d["editable"], json!(false));
    assert_eq!(d["notEditableReason"], json!("not_editable"));
    for (method, params) in [
        ("entity.create", json!({"class": "info_target", "origin": [0, 0, 0]})),
        ("entity.setKeyValues", json!({"nodeID": 30, "values": {}})),
        ("node.delete", json!({"nodeIDs": [30]})),
        ("undo.undo", json!({})),
        ("map.save", json!({})),
    ] {
        let err = call_err_req(&mb, method, params, opts(1)).await;
        assert_eq!(err.code, Some(-32018), "{method}: {err:?}");
        assert_eq!(err.data["reason"], json!("not_editable"), "{method}");
    }
    // reads still work
    let r = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    assert_eq!(r["nodeID"], json!(30));
}

// ---------------------------------------------------------------- map.open (m2.16)

/// The document path the map.open tests use (the fixture's own, explicit so the session-leak gate's
/// comparison is against a known addon-relative form).
const DOC_WIN: &str = "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
/// The Windows path `map.open` builds for an addon-relative one (`mcp_bridge.c`'s
/// `kv3_host_path` shape, as the modeldoc/pet mock arms spell it).
const OPENED_WIN: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\b.vmap";

/// `.hammer_map_files(..)` + the explicit document, so every test below reads the same way.
async fn map_open_world(files: Vec<&str>) -> MockBridge {
    MockBridge::builder()
        .doc(Some(make_doc(DOC_WIN, Some(false))))
        .hammer_nodes(vec![MockNode::world()])
        .hammer_map_files(files)
        .start()
        .await
        .unwrap()
}

/// The shape gate is the bridge's own (`bridge/map_path.h`, mirrored in the mock): a path that is
/// not a `maps/…vmap` of the addon is `-32602`, a path the host does not have is `-32014`
/// `AssetNotFound` (both BEFORE anything runs: no document change either way).
#[tokio::test]
async fn map_open_refuses_a_bad_path_and_a_missing_file() {
    let mb = map_open_world(vec!["maps/b.vmap"]).await;
    let before = doc(&mb).await;

    for (path, reason) in [
        ("", "path: a non-empty path relative to the addon content root (maps/... .vmap) is required"),
        ("models/probe.vmap", "path: under maps/ (the addon's map directory)"),
        ("maps/probe.vmdl", "path: a .vmap file"),
        ("maps/../probe.vmap", "path: no '..'"),
        ("Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap", "path: relative to the addon content root (maps/...), not absolute"),
        // review M1: the canonical spelling only - Win32 opens every one of these for the same file
        ("maps\\probe.vmap", "path: no '\\' (use '/' separators)"),
        ("maps//probe.vmap", "path: no empty path segment ('//')"),
        ("maps/./probe.vmap", "path: no '.' or '..' path segment"),
        ("maps/a./probe.vmap", "path: no path segment ending in '.' or a space"),
        ("maps/a /probe.vmap", "path: no path segment ending in '.' or a space"),
    ] {
        let err = call_err(&mb, "map.open", json!({"path": path})).await;
        assert_eq!(err.code, Some(-32602), "{path}: {err:?}");
        assert_eq!(err.data["reason"], json!(reason), "{path}");
    }

    // a file the host does not have: the bridge's host gate (-32014 AssetNotFound)
    let err = call_err(&mb, "map.open", json!({"path": "maps/nope.vmap"})).await;
    assert_eq!(err.code, Some(-32014), "{err:?}");
    assert_eq!(err.wire_name.as_deref(), Some("AssetNotFound"));
    assert_eq!(err.data["reason"], json!("no_such_file"));

    assert_eq!(doc(&mb).await, before, "nothing opened, nothing changed");
}

/// The session-leak gate: a file one of the open sessions holds answers
/// `{opened:false, existing:true}` and the document is NOT touched — the bridge does not call
/// `OpenFile`, whose session match is a `xor al,al; ret` stub (a second call would create a second
/// session for the same file).
#[tokio::test]
async fn map_open_of_an_already_open_file_leaves_the_document_alone() {
    let mb = map_open_world(vec!["maps/b.vmap"]).await;
    let before = doc(&mb).await;

    let r = call(&mb, "map.open", json!({"path": "maps/probe.vmap"})).await;
    assert_eq!(r["opened"], json!(false), "{r}");
    assert_eq!(r["existing"], json!(true), "{r}");
    assert_eq!(r["active"], json!(true), "the active document's own file is the active session");
    assert_eq!(r["path"], json!(DOC_WIN), "{r}");
    assert!(r["epoch"].is_i64(), "{r}");

    // a case-insensitive spelling of the same file is the same session
    let r = call(&mb, "map.open", json!({"path": "MAPS/Probe.VMAP"})).await;
    assert_eq!(r["existing"], json!(true), "{r}");

    assert_eq!(
        doc(&mb).await,
        before,
        "an already-open file must not touch the document (no OpenFile, no epoch/doc id move)"
    );
}

/// Review M2: live, the app keeps EVERY session in its vector, so the A -> B -> A sequence ends with
/// A still open in the background - `existing:true` and `active:false`. The mock used to model one
/// session and answered `opened:true` with a fresh document identity for the second `maps/a.vmap`,
/// which no live run can do.
#[tokio::test]
async fn map_open_of_a_background_session_is_existing_and_not_active() {
    let mb = map_open_world(vec!["maps/b.vmap"]).await;
    let a = doc(&mb).await;
    assert_eq!(a["path"], json!(DOC_WIN), "the world starts with probe.vmap open");

    // A (= probe.vmap) -> B: a new session, B is now the active document
    let r = call(&mb, "map.open", json!({"path": "maps/b.vmap"})).await;
    assert_eq!(r["opened"], json!(true), "{r}");
    let b = doc(&mb).await;
    assert_ne!(b["id"], a["id"], "{b}");
    assert_eq!(b["path"], json!(OPENED_WIN), "{b}");

    // B -> A: A's session is still there, in the background
    let r = call(&mb, "map.open", json!({"path": "maps/probe.vmap"})).await;
    assert_eq!(r["opened"], json!(false), "{r}");
    assert_eq!(r["existing"], json!(true), "{r}");
    assert_eq!(r["active"], json!(false), "A's session is in the background: {r}");
    assert_eq!(r["path"], json!(DOC_WIN), "{r}");
    assert_eq!(r["epoch"], b["epoch"], "nothing opened, so nothing moved: {r}");
    assert_eq!(doc(&mb).await, b, "B is still the active document: no OpenFile was called");

    // and B is still the active session
    let r = call(&mb, "map.open", json!({"path": "maps/b.vmap"})).await;
    assert_eq!(r["existing"], json!(true), "{r}");
    assert_eq!(r["active"], json!(true), "{r}");
    assert_eq!(r["path"], json!(OPENED_WIN), "{r}");
}

/// A file the host has that no session holds becomes the document, like the new session
/// `OpenFile` creates: a new `id`, a bumped `epoch`, clean, no undo history.
#[tokio::test]
async fn map_open_of_a_new_file_switches_the_document() {
    let mb = map_open_world(vec!["maps/b.vmap"]).await;
    let before = doc(&mb).await;

    let r = call(&mb, "map.open", json!({"path": "maps/b.vmap"})).await;
    assert_eq!(r["opened"], json!(true), "{r}");
    assert_eq!(r["existing"], json!(false), "{r}");
    assert_eq!(r["path"], json!(OPENED_WIN), "{r}");

    let after = doc(&mb).await;
    assert_eq!(after["path"], json!(OPENED_WIN));
    assert_eq!(after["dirty"], json!(false));
    assert_eq!(after["undoDepth"], json!(0), "a fresh document has no undo history");
    assert_ne!(after["id"], before["id"], "a new session is a new document identity");
    assert!(after["epoch"].as_i64().unwrap() > before["epoch"].as_i64().unwrap());
    assert_eq!(r["epoch"], after["epoch"], "the reply carries the fingerprint's epoch");

    // and the file is now the open session's: a second open answers existing:true
    let r = call(&mb, "map.open", json!({"path": "maps/b.vmap"})).await;
    assert_eq!(r["existing"], json!(true), "{r}");
    assert_eq!(r["path"], json!(OPENED_WIN));
}

// ---------------------------------------------------------------- mock fidelity (brief item 15)

/// The empty value (m2.16): Hammer represents the empty string as a NULL `CUtlSymbolLarge`, and the
/// bridge writes it like any other text — a NULL symbol used to skip the write and answer
/// `applied:false` with no warning at all (the live bug this pass fixes). The mock applies it and
/// reads it back as `""`.
#[tokio::test]
async fn empty_string_values_are_applied() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "old")])),
    ])
    .await;

    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {"targetname": "", "newkey": ""}}),
        opts(1),
    )
    .await;
    assert_eq!(r["applied"]["targetname"], json!(true), "{r}");
    assert_eq!(r["applied"]["newkey"], json!(true), "{r}");
    assert_eq!(r["warnings"], json!([]), "an empty value is not a warning: {r}");
    assert_eq!(r["previous"]["targetname"], json!("old"), "{r}");

    let r = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    assert_eq!(r["keyValues"]["targetname"], json!(""), "{r}");
    assert_eq!(r["keyValues"]["newkey"], json!(""), "{r}");
}

/// A value longer than the bridge's `KV_VALUE_MAX` 1024-byte value buffer (`sval[1025]`,
/// `KV_VALUE_MAX` bytes plus the NUL) is not applied: `applied:false` plus the bridge's warning,
/// on create and on setKeyValues, and the node keeps its old value. 1024 bytes fit exactly.
#[tokio::test]
async fn values_over_1024_bytes_are_not_applied() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("vscripts", "short")])),
    ])
    .await;
    let at_cap = "v".repeat(1024);
    let over = "v".repeat(1025);

    let r = call_req(
        &mb,
        "entity.setKeyValues",
        json!({"nodeID": 30, "values": {"vscripts": over.clone(), "targetname": at_cap.clone()}}),
        opts(1),
    )
    .await;
    assert_eq!(r["applied"]["vscripts"], json!(false), "{r}");
    assert_eq!(r["applied"]["targetname"], json!(true), "{r}");
    let warnings = r["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default() == "vscripts: value longer than 1024 bytes"),
        "{warnings:?}"
    );
    // the old value survives; the 1024-byte one (at the cap) is stored whole
    let nodes = mb.hammer_nodes();
    let e = nodes.iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(e.keyvalues.get("vscripts").map(String::as_str), Some("short"));
    assert_eq!(e.keyvalues.get("targetname").map(String::len), Some(1024));

    // the same on create
    let r = call_req(
        &mb,
        "entity.create",
        json!({"class": "info_target", "origin": [0, 0, 0],
               "keyValues": {"vscripts": "v".repeat(1100)}}),
        opts(2),
    )
    .await;
    assert_eq!(r["keyValues"]["applied"]["vscripts"], json!(false), "{r}");
    assert!(
        !r["keyValues"]["warnings"].as_array().unwrap().is_empty(),
        "{r}"
    );
    let created = mb.hammer_nodes().into_iter().find(|n| n.node_id == 31).unwrap();
    assert!(!created.keyvalues.contains_key("vscripts"));
}

/// A `nodeID` above `i32::MAX` would truncate to a different node in the bridge: the mock refuses
/// it as -32602 like the fixed bridge (brief item 15).
#[tokio::test]
async fn node_ids_above_int_max_are_refused() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new()),
    ])
    .await;
    for (method, params) in [
        ("entity.get", json!({"nodeID": 4_294_967_298i64})),
        (
            "entity.setKeyValues",
            json!({"nodeID": 2_147_483_648i64, "values": {"a": "b"}}),
        ),
    ] {
        // the mutator needs the expectDoc gate first; the nodeID check runs in the op
        let opts = if method == "entity.get" {
            RequestOpts::default()
        } else {
            opts(1)
        };
        let err = call_err_req(&mb, method, params, opts).await;
        assert_eq!(err.code, Some(-32602), "{method}: {err:?}");
    }
    // the world would be node 30 with the truncated id: nothing happened
    let e = mb.hammer_nodes().into_iter().find(|n| n.node_id == 30).unwrap();
    assert!(e.keyvalues.is_empty(), "{e:?}");

    let err = call_err_req(
        &mb,
        "node.delete",
        json!({"nodeIDs": [4_294_967_298i64]}),
        opts(1),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(mb.hammer_nodes().len(), 2, "nothing was deleted");
}

/// An empty `class`/`targetname` filter is read by the bridge as "no filter": the mock refuses it
/// so a tool that stops checking is caught (brief item 13).
#[tokio::test]
async fn empty_entity_find_filters_are_refused() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], BTreeMap::new()),
    ])
    .await;
    for params in [json!({"class": ""}), json!({"targetname": ""})] {
        let err = call_err(&mb, "entity.find", params.clone()).await;
        assert_eq!(err.code, Some(-32602), "{params}: {err:?}");
        assert!(
            err.data["reason"].as_str().unwrap_or_default().contains("empty"),
            "{params}: {err:?}"
        );
    }
    // a non-string filter is a parameter error too
    for params in [json!({"class": 3}), json!({"targetname": 3})] {
        let err = call_err(&mb, "entity.find", params.clone()).await;
        assert_eq!(err.code, Some(-32602), "{params}: {err:?}");
    }
    // a null filter is simply absent (JSON null means "not given"), not an error
    let r = call(&mb, "entity.find", json!({"class": null})).await;
    assert_eq!(r["count"], json!(1), "{r}");
}

// ---------------------------------------------------------------- geometry (WP-B)

/// A box request the way the POC's `create_block` sends it.
fn box_params() -> Value {
    json!({
        "vertices": [[0, 0, 0], [8, 0, 0], [0, 8, 0], [8, 8, 0],
                     [0, 0, 8], [8, 0, 8], [0, 8, 8], [8, 8, 8]],
        "faces": [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]],
        "material": "materials/dev/reflectivity_30.vmat",
    })
}

/// The world's current fingerprint epoch (`doc.active`).
async fn epoch(mb: &MockBridge) -> i64 {
    doc(mb).await["epoch"].as_i64().unwrap_or(0)
}

/// The geometry the mock world holds for `node_id`.
fn meshes(mb: &MockBridge, node_id: i64) -> Option<cs2_mcp_common::mock::MockMesh> {
    mb.with_state(|state| {
        state
            .hammer
            .as_ref()
            .and_then(|world| world.meshes.get(&node_id).cloned())
    })
}

/// `mesh.create` stores the geometry, re-centres the pivot on the bounds and answers `geo_mesh_summary`
/// plus the op's own fields (`mcp_bridge.c` `op_mesh_create`).
#[tokio::test]
async fn mesh_create_stores_the_geometry_and_centres_the_pivot() {
    let mb = start_world(vec![MockNode::world()]).await;
    let r = call(&mb, "mesh.create", box_params_with_epoch(&mb).await).await;

    assert_eq!(r["nodeID"], json!(2), "the world owns nodeID 1: {r}");
    assert_eq!(r["class"], json!("mesh"));
    assert_eq!(r["origin"], json!([4.0, 4.0, 4.0]), "the bounds centre");
    assert_eq!(r["vertexSlots"], json!(8));
    assert_eq!(r["faceSlots"], json!(6));
    assert_eq!(r["inputVertices"], json!(8));
    assert_eq!(r["faces"], json!(6));
    assert_eq!(r["materials"], json!(1), "no faceMaterials -> one material");
    assert_eq!(r["undoLabel"], json!("MCP: create mesh"));

    let stored = meshes(&mb, 2).expect("geometry stored");
    assert_eq!(stored.vertices.len(), 8);
    assert_eq!(stored.faces.len(), 6);
    assert!(stored
        .materials
        .iter()
        .all(|m| m == "materials/dev/reflectivity_30.vmat"));
    // the node is a mesh in the walk (node.list sees it, no slots reported for the file route)
    let listed = call(&mb, "node.list", json!({"class": "mesh"})).await;
    assert_eq!(listed["count"], json!(1), "{listed}");
    assert_eq!(listed["nodes"][0]["nodeID"], json!(2));

    // a second create takes the next free id and counts the distinct face materials
    let mut params = box_params_with_epoch(&mb).await;
    params["faceMaterials"] = json!(["materials/a.vmat", null, "materials/b.vmat", null, null, null]);
    let r = call(&mb, "mesh.create", params).await;
    assert_eq!(r["nodeID"], json!(3), "{r}");
    assert_eq!(r["materials"], json!(3), "base + two distinct: {r}");
    let stored = meshes(&mb, 3).expect("geometry stored");
    assert_eq!(stored.materials[0], "materials/a.vmat");
    assert_eq!(stored.materials[1], "materials/dev/reflectivity_30.vmat");
    assert_eq!(stored.materials[2], "materials/b.vmat");
}

/// `mesh.create` with the world's current epoch in `expectDoc`.
async fn box_params_with_epoch(mb: &MockBridge) -> Value {
    let mut params = box_params();
    params["expectDoc"] = json!({"id": "d1", "epoch": epoch(mb).await});
    params
}

/// `geom_parse`'s rejections (the bridge's own reasons) and the gate order: `expectDoc` first, then
/// the parameters.
#[tokio::test]
async fn mesh_create_refuses_like_geom_parse() {
    let mb = start_world(vec![MockNode::world()]).await;
    let e = epoch(&mb).await;
    let cases: [(Value, &str); 7] = [
        (json!({"faces": [[0, 1, 2]], "material": "m"}), "vertices"),
        (json!({"vertices": [[0, 0, 0], [1, 0, 0]], "faces": [[0, 1, 0]], "material": "m"}), "at least 3"),
        (json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "material": "m"}), "faces"),
        (json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1]], "material": "m"}), "at least 3 vertices"),
        (json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 5]], "material": "m"}), "integers in range"),
        (json!({"vertices": [[1e6, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]], "material": "m"}), "1e6"),
        (json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]]}), "material"),
    ];
    for (params, needle) in cases {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "mesh.create", params.clone()).await;
        assert_eq!(err.code, Some(-32602), "{params}: {err:?}");
        assert!(
            err.data["reason"].as_str().unwrap_or_default().contains(needle),
            "{params}: {err:?}"
        );
    }
    // faceMaterials: longer than faces, at most 64 distinct, or not a string/null
    for params in [
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
               "material": "m", "faceMaterials": ["a", "b"]}),
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
               "material": "m", "faceMaterials": [7]}),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "mesh.create", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
    let faces: Vec<Vec<usize>> = vec![vec![0, 1, 2]; 65];
    let materials: Vec<String> = (0..65).map(|i| format!("m{i}")).collect();
    let err = call_err(
        &mb,
        "mesh.create",
        json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": faces, "material": "m",
               "faceMaterials": materials, "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"].as_str().unwrap_or_default().contains("64 distinct"),
        "{err:?}"
    );

    // nothing was created and the epoch never moved
    assert_eq!(epoch(&mb).await, e);
    assert!(meshes(&mb, 2).is_none());

    // the gate order: a stale expectDoc is -32004 before any parameter check
    let err = call_err_req(&mb, "mesh.create", json!({}), opts(e + 99)).await;
    assert_eq!(err.code, Some(-32004), "{err:?}");
    assert_eq!(err.data["reason"], json!("epoch"));
}

/// `mesh.setGeometry`/`mesh.setMaterial` address a live mesh: a missing node is `-32022`, anything
/// that is not a mesh is `-32602 "node is not a mesh"` — both before the undo group opens.
#[tokio::test]
async fn mesh_edits_need_a_live_mesh() {
    let mb = start_world(vec![MockNode::world()]).await;
    let created = call(&mb, "mesh.create", box_params_with_epoch(&mb).await).await;
    let id = created["nodeID"].as_i64().expect("nodeID");
    let e = epoch(&mb).await;

    for (method, params) in [
        ("mesh.setGeometry", json!({"nodeID": 999, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                                    "faces": [[0, 1, 2]], "material": "m"})),
        ("mesh.setMaterial", json!({"nodeID": 999, "material": "m"})),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, method, params).await;
        assert_eq!(err.code, Some(-32022), "{method}: {err:?}");
        assert_eq!(err.message, "NodeNotFound");
    }
    // the world node is not a mesh
    for (method, params) in [
        ("mesh.setGeometry", json!({"nodeID": 1, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                                    "faces": [[0, 1, 2]], "material": "m"})),
        ("mesh.setMaterial", json!({"nodeID": 1, "material": "m"})),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, method, params).await;
        assert_eq!(err.code, Some(-32602), "{method}: {err:?}");
        assert_eq!(err.data["reason"], json!("node is not a mesh"));
    }
    // a mesh node the world knows nothing about has no faces (the bridge's -32603)
    mb.with_state(|state| {
        state
            .hammer
            .as_mut()
            .expect("world")
            .nodes
            .push(MockNode {
                node_id: 7,
                class: "mesh".into(),
                classname: String::new(),
                targetname: None,
                origin: [0.0; 3],
                angles: [0.0; 3],
                keyvalues: BTreeMap::new(),
                locked: false,
                parent: None,
            });
        state.hammer.as_mut().expect("world").meshes.remove(&7);
    });
    let err = call_err(
        &mb,
        "mesh.setMaterial",
        json!({"nodeID": 7, "material": "m", "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(err.code, Some(-32603), "{err:?}");
    assert_eq!(err.data["reason"], json!("mesh has no faces"));

    // setGeometry rebuilds from scratch and never needs the old geometry
    let r = call(
        &mb,
        "mesh.setGeometry",
        json!({"nodeID": 7, "vertices": [[0, 0, 0], [4, 0, 0], [0, 4, 0]], "faces": [[0, 1, 2]],
               "material": "m", "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(r["nodeID"], json!(7));
    assert_eq!(r["faces"], json!(1));
    assert_eq!(r["origin"], json!([2.0, 2.0, 0.0]));
    assert_eq!(r["undoLabel"], json!("MCP: set mesh geometry"));
    assert_eq!(meshes(&mb, 7).expect("geometry").faces.len(), 1);

    // the id kept its mesh class and the created mesh is untouched by the rebuild
    assert_eq!(meshes(&mb, id).expect("created geometry").faces.len(), 6);
}

/// `mesh.setMaterial` takes every face, and the undo group restores the previous materials (a real
/// geometry snapshot, not the keyvalue `group-only` gap).
#[tokio::test]
async fn mesh_set_material_and_undo_restore_the_geometry() {
    let mb = start_world(vec![MockNode::world()]).await;
    let created = call(&mb, "mesh.create", box_params_with_epoch(&mb).await).await;
    let id = created["nodeID"].as_i64().expect("nodeID");

    let r = call(
        &mb,
        "mesh.setMaterial",
        json!({"nodeID": id, "material": "materials/tools/toolsnodraw.vmat",
               "expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(r["faces"], json!(6));
    assert_eq!(r["material"], json!("materials/tools/toolsnodraw.vmat"));
    assert_eq!(r["undoLabel"], json!("MCP: set mesh material"));
    assert!(meshes(&mb, id)
        .expect("geometry")
        .materials
        .iter()
        .all(|m| m == "materials/tools/toolsnodraw.vmat"));

    let undo = call(
        &mb,
        "undo.undo",
        json!({"expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(undo["name"], json!("MCP: set mesh material"));
    assert!(
        meshes(&mb, id)
            .expect("geometry")
            .materials
            .iter()
            .all(|m| m == "materials/dev/reflectivity_30.vmat"),
        "the material group restores the materials"
    );

    // undoing the create removes the node and its geometry again
    let undo = call(
        &mb,
        "undo.undo",
        json!({"expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(undo["name"], json!("MCP: create mesh"));
    assert!(meshes(&mb, id).is_none(), "the snapshot dropped the geometry");
    let listed = call(&mb, "node.list", json!({"class": "mesh"})).await;
    assert_eq!(listed["count"], json!(0), "{listed}");
}

/// The three mesh ops are `geo_dispatch` mutators: `doc_editable` is checked before `expectDoc`, so
/// a read-only document is `-32018` even with a stale fingerprint.
#[tokio::test]
async fn mesh_ops_are_gated_on_an_editable_document() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .hammer_editable(false)
        .start()
        .await
        .unwrap();
    for (method, params) in [
        ("mesh.create", json!({"vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]], "faces": [[0, 1, 2]],
                               "material": "m", "expectDoc": {"id": "d1", "epoch": 1}})),
        ("mesh.setGeometry", json!({"nodeID": 2, "vertices": [[0, 0, 0], [1, 0, 0], [0, 1, 0]],
                                    "faces": [[0, 1, 2]], "material": "m",
                                    "expectDoc": {"id": "d1", "epoch": 1}})),
        ("mesh.setMaterial", json!({"nodeID": 2, "material": "m",
                                    "expectDoc": {"id": "d1", "epoch": 1}})),
    ] {
        let err = call_err(&mb, method, params).await;
        assert_eq!(err.code, Some(-32018), "{method}: {err:?}");
        assert_eq!(err.message, "DocumentNotEditable");
        assert_eq!(err.data["reason"], json!("not_editable"));
    }
    assert!(meshes(&mb, 2).is_none());
}

// ---------------------------------------------------------------- other built-ins untouched

/// The knobs stay in front of the hammer built-ins (out of arms: `responder` wins).
#[tokio::test]
async fn responder_still_overrides_hammer_builtins() {
    let mb = MockBridge::builder()
        .hammer_nodes(vec![MockNode::world()])
        .responder("entity.get", |_: &Value| json!({"synthetic": true}))
        .start()
        .await
        .unwrap();
    let r = call(&mb, "entity.get", json!({"nodeID": 1})).await;
    assert_eq!(r, json!({"synthetic": true}));
}

/// `HammerWorld` stays out of the public API surface except through the mock module exports.
#[test]
fn hammer_exports_are_wired() {
    assert_eq!(HAMMER_METHODS.len(), 20);
    assert!(HAMMER_METHODS.contains(&"entity.create"));
    assert!(HAMMER_METHODS.contains(&"map.open"));
    assert!(HAMMER_METHODS.contains(&"mesh.create"));
    assert!(HAMMER_METHODS.contains(&"mesh.setGeometry"));
    assert!(HAMMER_METHODS.contains(&"mesh.setMaterial"));
    // m2.18 (WP-R2): the prop writes
    assert!(HAMMER_METHODS.contains(&"node.setTransform"));
    assert!(HAMMER_METHODS.contains(&"entity.addConnections"));
    assert!(HAMMER_METHODS.contains(&"entity.removeConnections"));
    // m2.19 (WP-T): the brush-entity pair (`entity.tieMeshes` is the gate's marker method)
    assert!(HAMMER_METHODS.contains(&"entity.tieMeshes"));
    assert!(HAMMER_METHODS.contains(&"entity.untie"));
}

// ---------------------------------------------------------------- m2.18 prop writes (WP-R2)

/// The world the m2.18 write tests start from: nodeID 30 an entity with a targetname, 41 a mesh,
/// 42 a locked entity.
fn prop_world() -> Vec<MockNode> {
    let mut locked = MockNode::entity(42, "info_target", [0.0; 3], kv(&[("targetname", "locked")]));
    locked.locked = true;
    vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [1.0, 2.0, 3.0], kv(&[("targetname", "t30")])),
        MockNode {
            node_id: 41,
            class: "mesh".into(),
            classname: String::new(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
        locked,
    ]
}

/// `node.setTransform` (spec §3): the reply reads the node's fields back, the write opens one
/// group and is undoable, and every gate (member shapes, the origin/scales ranges, the class, the
/// lock, `expectDoc`) refuses with nothing written.
#[tokio::test]
async fn node_set_transform_shape_and_negatives() {
    let mb = start_world(prop_world()).await;

    let mut epoch = 1;
    let r = call_req(
        &mb,
        "node.setTransform",
        json!({"nodeID": 30, "origin": [4, 5, 6], "angles": [0, 90, 0], "scales": [2, 2, 2]}),
        opts(epoch),
    )
    .await;
    assert_eq!(r["nodeID"], json!(30));
    assert_eq!(r["class"], json!("entity"));
    assert_eq!(r["origin"], json!([4.0, 5.0, 6.0]));
    assert_eq!(r["angles"], json!([0.0, 90.0, 0.0]));
    assert_eq!(r["scales"], json!([2.0, 2.0, 2.0]));
    assert_eq!(r["doc"]["undoDepth"], json!(1), "one group");
    let undo = mb.with_state(|st| st.hammer.as_ref().expect("world").undo.clone());
    assert_eq!(undo[0].label, "MCP: set transform");
    let entity = mb.hammer_nodes().into_iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(entity.origin, [4.0, 5.0, 6.0]);
    assert_eq!(entity.angles, [0.0, 90.0, 0.0]);
    assert_eq!(mb.hammer_scales(30), [2.0, 2.0, 2.0]);
    epoch += 1;

    // a mesh node transforms too (the tool's second valid class)
    let r = call_req(&mb, "node.setTransform", json!({"nodeID": 41, "origin": [7, 8, 9]}), opts(epoch))
        .await;
    assert_eq!(r["class"], json!("mesh"));
    assert_eq!(r["origin"], json!([7.0, 8.0, 9.0]));
    epoch += 1;

    for (params, code, reason) in [
        (json!({"nodeID": 30}), -32602, "at least one"),
        (json!({"nodeID": 30, "origin": [1, 2]}), -32602, "origin"),
        (json!({"nodeID": 30, "origin": [1, 2, "3"]}), -32602, "origin"),
        (json!({"nodeID": 30, "origin": [2000000, 0, 0]}), -32602, "origin"),
        (json!({"nodeID": 30, "scales": [0, 1, 1]}), -32602, "scales"),
        (json!({"nodeID": 30, "scales": [1, 1, 1000.5]}), -32602, "scales"),
        (json!({"nodeID": 999, "origin": [0, 0, 0]}), -32022, "999"),
        (json!({"nodeID": 1, "origin": [0, 0, 0]}), -32602, "not an entity or a mesh"),
        (json!({"nodeID": 42, "origin": [0, 0, 0]}), -32018, "locked"),
    ] {
        let err = call_err_req(&mb, "node.setTransform", params.clone(), opts(epoch)).await;
        assert_eq!(err.code, Some(code), "{params}: {err:?}");
        let text = err.data.get("reason").and_then(Value::as_str).unwrap_or_default();
        assert!(text.contains(reason), "{params}: {text}");
    }
    // no expectDoc at all -> DocumentChanged (the fingerprint gate runs first)
    let err = call_err(&mb, "node.setTransform", json!({"nodeID": 30, "origin": [1, 1, 1]})).await;
    assert_eq!(err.code, Some(-32004));
    // ... and nothing of the refused calls moved the node or opened a group
    let entity = mb.hammer_nodes().into_iter().find(|n| n.node_id == 30).unwrap();
    assert_eq!(entity.origin, [4.0, 5.0, 6.0]);
    assert_eq!(mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()), 2);
}

/// `entity.addConnections` / `entity.removeConnections` (spec §4): every member is validated
/// before the group opens, the appended entries are indexed in the live vector, `entity.get`
/// reports them with the reader's names, and the selector refusals (duplicates, out of range, a
/// zero-hit match) leave the vector untouched.
#[tokio::test]
async fn connection_writes_shape_and_negatives() {
    let mb = start_world(prop_world()).await;
    let mut epoch = 1;

    for (params, reason) in [
        (json!({"nodeID": 30}), "connections"),
        (json!({"nodeID": 30, "connections": []}), "connections"),
        (json!({"nodeID": 30, "connections": "x"}), "connections"),
        (json!({"nodeID": 30, "connections": [1]}), "each entry"),
        (json!({"nodeID": 30, "connections": [{"target": "t", "input": "i"}]}), "output"),
        (json!({"nodeID": 30, "connections": [{"output": "", "target": "t", "input": "i"}]}), "output"),
        (json!({"nodeID": 30, "connections": [{"output": "o", "target": "t", "input": null}]}), "input"),
        (json!({"nodeID": 30, "connections": [{"output": "o", "target": "t", "input": "i", "param": 1}]}), "param"),
        (json!({"nodeID": 30, "connections": [{"output": "o", "target": "t", "input": "i", "delay": 3601}]}), "delay"),
        (json!({"nodeID": 30, "connections": [{"output": "o", "target": "t", "input": "i", "timesToFire": 0}]}), "timesToFire"),
        (json!({"nodeID": 30, "connections": [{"output": "o", "target": "t", "input": "i", "timesToFire": -2}]}), "timesToFire"),
        (json!({"nodeID": 41, "connections": [{"output": "o", "target": "t", "input": "i"}]}), "not an entity"),
        (json!({"nodeID": 999, "connections": [{"output": "o", "target": "t", "input": "i"}]}), "999"),
    ] {
        let err = call_err_req(&mb, "entity.addConnections", params.clone(), opts(epoch)).await;
        let code = if params["nodeID"] == json!(999) { -32022 } else { -32602 };
        assert_eq!(err.code, Some(code), "{params}: {err:?}");
        let text = err.data.get("reason").and_then(Value::as_str).unwrap_or_default();
        assert!(text.contains(reason), "{params}: {text}");
    }
    // 65 entries: the per-request cap (the string at the cap is refused separately, below)
    let many: Vec<Value> = (0..65)
        .map(|i| json!({"output": format!("o{i}"), "target": "t", "input": "i"}))
        .collect();
    let err = call_err_req(&mb, "entity.addConnections", json!({"nodeID": 30, "connections": many}), opts(epoch)).await;
    assert_eq!(err.code, Some(-32602));
    assert!(err.data["reason"].as_str().unwrap_or_default().contains("64"));
    let too_long = "x".repeat(256);
    let err = call_err_req(
        &mb,
        "entity.addConnections",
        json!({"nodeID": 30, "connections": [{"output": too_long, "target": "t", "input": "i"}]}),
        opts(epoch),
    )
    .await;
    assert_eq!(err.code, Some(-32602));
    // a refused add opened no group and wrote nothing
    assert!(mb.hammer_connections(30).is_empty());
    assert_eq!(mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()), 0);

    let r = call_req(
        &mb,
        "entity.addConnections",
        json!({"nodeID": 30, "connections": [
            {"output": "OnStartTouch", "target": "t1", "input": "Enable", "param": "p", "delay": 0.5, "timesToFire": 2},
            {"output": "OnStartTouch", "target": "t2", "input": "Kill"}
        ]}),
        opts(epoch),
    )
    .await;
    assert_eq!(r["nodeID"], json!(30));
    assert_eq!(r["count"], json!(2));
    assert_eq!(
        r["added"],
        json!([
            {"index": 0, "outputName": "OnStartTouch", "targetType": 7, "targetName": "t1",
             "inputName": "Enable", "overrideParam": "p", "delay": 0.5, "timesToFire": 2},
            {"index": 1, "outputName": "OnStartTouch", "targetType": 7, "targetName": "t2",
             "inputName": "Kill", "overrideParam": "", "delay": 0.0, "timesToFire": -1}
        ]),
        "the bridge's added[] shape (review W2 #2)"
    );
    assert_eq!(
        r["landed"],
        json!([{"index": 0, "landed": true}, {"index": 1, "landed": true}]),
        "one landed entry per wire entry, in request order"
    );
    assert_eq!(r["doc"]["undoDepth"], json!(1));
    epoch += 1;

    // a second add appends and reports the live indices; `count` is the confirmed adds of THIS
    // call (2 -> 1), never the vector's size after it (the bridge's own semantics)
    let r = call_req(
        &mb,
        "entity.addConnections",
        json!({"nodeID": 30, "connections": [{"output": "OnUser1", "target": "t3", "input": "Kill"}]}),
        opts(epoch),
    )
    .await;
    assert_eq!(r["added"][0]["index"], json!(2));
    assert_eq!(r["count"], json!(1), "one add, one confirmed: {r}");
    assert_eq!(mb.hammer_connections(30).len(), 3);
    epoch += 1;

    // the read-back seam: entry 0's confirmation fails -> {"readback":false} + landed false, out
    // of `count` (review W2 #3; the tool turns this into `unconfirmed`/`outcomeUnknown`)
    mb.set_hammer_drop_conn_readback(Some(0));
    let r = call_req(
        &mb,
        "entity.addConnections",
        json!({"nodeID": 30, "connections": [{"output": "OnUser2", "target": "t4", "input": "Kill"}]}),
        opts(epoch),
    )
    .await;
    assert_eq!(r["count"], json!(0), "{r}");
    assert_eq!(r["added"], json!([{"readback": false, "index": 3}]), "{r}");
    assert_eq!(r["landed"], json!([{"index": 3, "landed": false}]), "{r}");
    mb.set_hammer_drop_conn_readback(None);
    epoch += 1;

    // entity.get reports the live vector with the reader's names (m2.18, re-props.md Q3) - all
    // four, including the entry whose read-back the seam above could not confirm (it landed)
    let read = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    assert_eq!(
        read["connections"],
        json!([
            {"outputName": "OnStartTouch", "targetType": 7, "targetName": "t1", "inputName": "Enable",
             "overrideParam": "p", "delay": 0.5, "timesToFire": 2},
            {"outputName": "OnStartTouch", "targetType": 7, "targetName": "t2", "inputName": "Kill",
             "overrideParam": "", "delay": 0.0, "timesToFire": -1},
            {"outputName": "OnUser1", "targetType": 7, "targetName": "t3", "inputName": "Kill",
             "overrideParam": "", "delay": 0.0, "timesToFire": -1},
            {"outputName": "OnUser2", "targetType": 7, "targetName": "t4", "inputName": "Kill",
             "overrideParam": "", "delay": 0.0, "timesToFire": -1}
        ])
    );

    // the remove selector gates
    for (params, code) in [
        (json!({"nodeID": 30}), -32602),
        (json!({"nodeID": 30, "indices": [0], "match": {"output": "o"}}), -32602),
        (json!({"nodeID": 30, "indices": []}), -32602),
        (json!({"nodeID": 30, "indices": [0, 0]}), -32602),
        (json!({"nodeID": 30, "indices": [9]}), -32602),
        (json!({"nodeID": 30, "match": {}}), -32602),
        (json!({"nodeID": 30, "match": {"nope": "x"}}), -32602),
        (json!({"nodeID": 30, "match": {"input": "Nothing"}}), -32026),
        (json!({"nodeID": 41, "indices": [0]}), -32602),
    ] {
        let err = call_err_req(&mb, "entity.removeConnections", params.clone(), opts(epoch)).await;
        assert_eq!(err.code, Some(code), "{params}: {err:?}");
    }
    assert_eq!(mb.hammer_connections(30).len(), 4, "every refusal ran nothing");

    // indices remove in the bridge's descending order; the reply's set is ascending
    let r = call_req(&mb, "entity.removeConnections", json!({"nodeID": 30, "indices": [2, 0]}), opts(epoch))
        .await;
    assert_eq!(r["removed"], json!([0, 2]));
    assert_eq!(
        r["count"],
        json!(2),
        "count is the number REMOVED (review W2 #1: it always was, the name misleads): {r}"
    );
    assert_eq!(r["remaining"], json!(2), "two of the four were removed: {r}");
    assert_eq!(r["doc"]["undoDepth"], json!(4));
    let left = mb.hammer_connections(30);
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].target, "t2");

    // and the match form (case-insensitive on the FGD-style names): both surviving entries match
    let r = call_req(
        &mb,
        "entity.removeConnections",
        json!({"nodeID": 30, "match": {"target": "t2"}}),
        opts(epoch + 1),
    )
    .await;
    assert_eq!(r["removed"], json!([0]));
    assert_eq!(r["count"], json!(1));
    assert_eq!(r["remaining"], json!(1));
    assert_eq!(mb.hammer_connections(30).len(), 1);
}

// ---------------------------------------------------------------- m2.19 tie/untie (WP-T)

/// A `mesh.create` request for a box between `min` and `max` (the `box_params` loops).
fn box_between(min: [f64; 3], max: [f64; 3]) -> Value {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    json!({
        "vertices": [[x0, y0, z0], [x1, y0, z0], [x0, y1, z0], [x1, y1, z0],
                     [x0, y0, z1], [x1, y0, z1], [x0, y1, z1], [x1, y1, z1]],
        "faces": [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]],
        "material": "materials/tools/toolstrigger.vmat",
    })
}

/// One `entity.tieMeshes` request with the world's current epoch in `expectDoc`.
async fn tie_params(mb: &MockBridge, class: &str, ids: Value, kv: Value) -> Value {
    let mut params = json!({"class": class, "nodeIDs": ids});
    if !kv.is_null() {
        params["keyValues"] = kv;
    }
    params["expectDoc"] = json!({"id": "d1", "epoch": epoch(mb).await});
    params
}

/// The world's nodes as `(nodeID, classname, parent)` rows (the tree the tie maintains).
fn tree(mb: &MockBridge) -> Vec<(i64, String, Option<i64>)> {
    mb.hammer_nodes()
        .into_iter()
        .map(|n| (n.node_id, n.classname, n.parent))
        .collect()
}

/// `entity.tieMeshes` (m2.19 FINAL contract §5): the gates run before the undo group opens, the
/// entity lands with the meshes as its children (the **first mesh's** origin as its own — Hammer's
/// rule — and the `keyValues` through the m2.18 `SetKeyValue` path), `entity.get` reports
/// `children`, and the one `"MCP: tie meshes to entity"` group undoes the whole thing.
#[tokio::test]
async fn tie_meshes_creates_the_entity_reparents_and_undoes() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "point")])),
    ])
    .await;
    // two boxes: [0..8]^3 and [16..24] x [0..8] x [0..8]; the first mesh's pivot is [4, 4, 4]
    let a = call(&mb, "mesh.create", box_params_with_epoch(&mb).await).await;
    let first = a["nodeID"].as_i64().expect("nodeID");
    let mut b = box_between([16.0, 0.0, 0.0], [24.0, 8.0, 8.0]);
    b["expectDoc"] = json!({"id": "d1", "epoch": epoch(&mb).await});
    let b = call(&mb, "mesh.create", b).await;
    let second = b["nodeID"].as_i64().expect("nodeID");

    let r = call(
        &mb,
        "entity.tieMeshes",
        tie_params(
            &mb,
            "trigger_once",
            json!([first, second]),
            json!({"targetname": "ta_once", "spawnflags": "4097"}),
        )
        .await,
    )
    .await;
    let entity = r["nodeID"].as_i64().expect("nodeID");
    assert_eq!(r["class"], json!("entity"));
    assert_eq!(r["classname"], json!("trigger_once"));
    assert_eq!(r["created"], json!(true));
    assert_eq!(
        r["origin"],
        json!([4.0, 4.0, 4.0]),
        "a new entity's origin is the FIRST mesh's (Hammer's rule): {r}"
    );
    assert_eq!(
        r["meshes"],
        json!([{"nodeID": first, "origin": [4.0, 4.0, 4.0]},
               {"nodeID": second, "origin": [20.0, 4.0, 4.0]}]),
        "{r}"
    );
    assert_eq!(r["alreadyTied"], json!([]));
    assert_eq!(r["removedEmptyEntities"], json!([]));
    assert_eq!(r["undoLabel"], json!("MCP: tie meshes to entity"));
    assert_eq!(r["doc"]["undoDepth"], json!(3), "create, create, tie: {r}");
    // A4: the reply echoes the keyvalues it wrote, and issues a handle for the node it created
    assert_eq!(
        r["keyValues"],
        json!({"applied": {"targetname": true, "spawnflags": true}, "warnings": []}),
        "{r}"
    );
    assert!(r["handle"].is_string(), "the created node's handle: {r}");

    // the tree edge: the entity is the parent of both meshes
    let rows = tree(&mb);
    assert!(
        rows.contains(&(entity, "trigger_once".to_string(), None)),
        "{rows:?}"
    );
    assert!(rows.contains(&(first, String::new(), Some(entity))), "{rows:?}");
    assert!(rows.contains(&(second, String::new(), Some(entity))), "{rows:?}");
    // ... and the keyvalues went through the m2.18 path
    let read = call(&mb, "entity.get", json!({"nodeID": entity})).await;
    assert_eq!(read["targetname"], json!("ta_once"));
    assert_eq!(read["keyValues"]["spawnflags"], json!("4097"));
    assert_eq!(
        read["children"],
        json!([{"nodeID": first, "class": "mesh"}, {"nodeID": second, "class": "mesh"}]),
        "{read}"
    );
    assert_eq!(read["childrenTruncated"], json!(false));
    // a point entity has no children (the empty array, not null)
    let point = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    assert_eq!(point["children"], json!([]));
    assert_eq!(point["childrenTruncated"], json!(false));

    // the undo restores the tree
    let undo = call(
        &mb,
        "undo.undo",
        json!({"expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(undo["name"], json!("MCP: tie meshes to entity"));
    let rows = tree(&mb);
    assert!(!rows.iter().any(|(id, _, _)| *id == entity), "the entity is gone");
    assert!(rows.contains(&(first, String::new(), None)), "{rows:?}");
    assert!(rows.contains(&(second, String::new(), None)), "{rows:?}");
}

/// The tie's gates, in the spec's order: the class (string, length, factory registry), the id
/// list, then every id (live, a mesh, not already tied, unlocked) — all before the undo group
/// opens, so a refused tie leaves the document and the undo stack untouched.
#[tokio::test]
async fn tie_meshes_gates_leave_nothing_behind() {
    let locked = MockNode {
        node_id: 8,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin: [0.0; 3],
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: true,
        parent: None,
    };
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "point")])),
        MockNode::entity(31, "info_target", [0.0; 3], kv(&[("targetname", "tied_to")])),
        // a mesh already tied to nodeID 31 (seeded with the builder)
        MockNode {
            node_id: 7,
            class: "mesh".into(),
            classname: String::new(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        }
        .with_parent(31),
        // nodeID 8 is locked and untied, nodeID 9 is locked AND tied to 31: the bridge checks the
        // lock BEFORE the already-tied state (review A9/L11), so both answer -32018
        locked.clone(),
        {
            let mut tied_and_locked = locked.clone();
            tied_and_locked.node_id = 9;
            tied_and_locked.with_parent(31)
        },
    ])
    .await;
    let e = epoch(&mb).await;

    // class gates
    for (params, code, reason) in [
        (json!({"class": "", "nodeIDs": [7]}), -32602, "class"),
        (json!({"class": "x".repeat(64), "nodeIDs": [7]}), -32602, "class"),
        (json!({"nodeIDs": [7]}), -32602, "class"),
        (json!({"class": "not_a_class", "nodeIDs": [7]}), -32006, "registry"),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "entity.tieMeshes", params).await;
        assert_eq!(err.code, Some(code), "{err:?}");
        assert!(
            err.data["reason"].as_str().unwrap_or_default().contains(reason),
            "{err:?}"
        );
    }
    // the id list gates
    for params in [
        json!({"class": "trigger_once"}),
        json!({"class": "trigger_once", "nodeIDs": []}),
        json!({"class": "trigger_once", "nodeIDs": "7"}),
        json!({"class": "trigger_once", "nodeIDs": ["7"]}),
        json!({"class": "trigger_once", "nodeIDs": [-1]}),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "entity.tieMeshes", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
    }
    let many: Vec<i64> = (0..65).collect();
    let mut params = json!({"class": "trigger_once", "nodeIDs": many});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    let err = call_err(&mb, "entity.tieMeshes", params).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(err.data["reason"].as_str().unwrap_or_default().contains("64"));

    // every id (in the spec's order: missing, not a mesh, locked, already tied). The bridge (and
    // now the mock) names the node in every one of these refusals: `{reason, nodeID}`.
    for (ids, code, reason, node) in [
        (json!([999]), -32022, "999", Some(999)),
        (json!([30]), -32602, "not a mesh", Some(30)),
        (json!([1]), -32602, "not a mesh", Some(1)),
        (json!(["7"]), -32602, "non-negative ints", None),
        (json!([7]), -32602, "already tied", Some(7)),
        (json!([8]), -32018, "locked", Some(8)),
        // locked AND tied: the lock wins (the bridge's order)
        (json!([9]), -32018, "locked", Some(9)),
    ] {
        let mut params = json!({"class": "trigger_once", "nodeIDs": ids});
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "entity.tieMeshes", params).await;
        assert_eq!(err.code, Some(code), "{ids}: {err:?}");
        assert!(
            err.data["reason"].as_str().unwrap_or_default().contains(reason),
            "{ids}: {err:?}"
        );
        if let Some(node) = node {
            assert_eq!(err.data["nodeID"], json!(node), "{ids}: {err:?}");
        } else {
            assert!(err.data.get("nodeID").is_none(), "{ids}: {err:?}");
        }
        if code == -32602 && reason == "already tied" {
            // the offending parent, which the tool reports as MeshAlreadyTied
            assert_eq!(err.data["parentNodeID"], json!(31), "{err:?}");
        }
    }
    // an id that is above the bridge's int range and a call that changes nothing: no group opened
    // (the mock never pushes one for a refusal)
    let mut params = json!({"class": "trigger_once", "nodeIDs": [json!(2147483648i64)]});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    assert_eq!(
        call_err(&mb, "entity.tieMeshes", params).await.code,
        Some(-32602)
    );
    // ... and none of them created anything, moved a mesh or opened a group
    assert_eq!(mb.hammer_nodes().len(), 6);
    assert_eq!(epoch(&mb).await, e, "no mutation reached the document");
    assert_eq!(mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()), 0);
}

/// An all-`alreadyTied` tie (every named mesh already under the target entity) is a **success
/// no-op** (§5 / review-b L4): no undo group opens (the reply says `noop:true` and
/// `undoLabel:null`), the fingerprint does not move and the undo stack stays empty. The Rust tool
/// reads that marker to report `undoGroups:0` instead of promising an undo of a group that does
/// not exist (A8).
#[tokio::test]
async fn an_all_already_tied_tie_opens_no_undo_group() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(70, "trigger_multiple", [1.0, 2.0, 3.0], BTreeMap::new()),
        mesh_node(41).with_parent(70),
        mesh_node(42),
    ])
    .await;
    let e = epoch(&mb).await;

    let r = call(
        &mb,
        "entity.tieMeshes",
        json!({"entityID": 70, "nodeIDs": [41], "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(r["nodeID"], json!(70));
    assert_eq!(r["created"], json!(false));
    assert_eq!(r["alreadyTied"], json!([41]));
    assert_eq!(r["noop"], json!(true), "{r}");
    assert_eq!(r["undoLabel"], Value::Null, "{r}");
    assert_eq!(r["doc"]["undoDepth"], json!(0), "{r}");
    assert_eq!(
        epoch(&mb).await,
        e,
        "a no-op does not move the document fingerprint"
    );
    assert_eq!(
        mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()),
        0,
        "no group was pushed"
    );
    assert!(tree(&mb).contains(&(41, String::new(), Some(70))));

    // a mixed list is a real tie: 42 moves, one group opens
    let r = call(
        &mb,
        "entity.tieMeshes",
        json!({"entityID": 70, "nodeIDs": [41, 42], "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(r["alreadyTied"], json!([41]));
    assert_eq!(r["undoLabel"], json!("MCP: tie meshes to entity"));
    assert_eq!(
        mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()),
        1
    );
}

/// A plain `class:"mesh"` node at the origin (the seeded fixtures of the tie tests).
fn mesh_node(id: i64) -> MockNode {
    MockNode {
        node_id: id,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin: [0.0; 3],
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    }
}

/// `entity.tieMeshes`' `entityID` form and `allowRetie` (§5): a mesh already under the target is
/// skipped (`alreadyTied`), a mesh under ANOTHER brush entity is refused unless `allowRetie` moves
/// it and deletes the emptied old entity (`removedEmptyEntities`), and one undo restores both.
#[tokio::test]
async fn tie_meshes_into_an_existing_entity_and_allow_retie() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(
            70,
            "trigger_multiple",
            [1.0, 2.0, 3.0],
            kv(&[("targetname", "ta_keep")]),
        ),
        MockNode::entity(71, "trigger_once", [0.0; 3], BTreeMap::new()),
        mesh_node(41).with_parent(70),
        mesh_node(42),
        mesh_node(43).with_parent(71),
    ])
    .await;

    // entityID: 41 is already under 70 (skipped), 42 is tied; the entity's own origin comes back
    let r = call(
        &mb,
        "entity.tieMeshes",
        json!({
            "entityID": 70, "nodeIDs": [41, 42],
            "expectDoc": {"id": "d1", "epoch": epoch(&mb).await}
        }),
    )
    .await;
    assert_eq!(r["nodeID"], json!(70));
    assert_eq!(r["class"], json!("entity"));
    assert_eq!(r["classname"], json!("trigger_multiple"));
    assert_eq!(r["created"], json!(false));
    assert_eq!(
        r["origin"],
        json!([1.0, 2.0, 3.0]),
        "an existing entity keeps its own origin: {r}"
    );
    assert_eq!(r["alreadyTied"], json!([41]));
    assert_eq!(r["removedEmptyEntities"], json!([]));
    assert_eq!(r["undoLabel"], json!("MCP: tie meshes to entity"));
    let rows = tree(&mb);
    assert!(rows.contains(&(41, String::new(), Some(70))), "{rows:?}");
    assert!(rows.contains(&(42, String::new(), Some(70))), "{rows:?}");

    // 43 belongs to 71: refused by default, with the offending parent, and nothing moved
    let e = epoch(&mb).await;
    let err = call_err(
        &mb,
        "entity.tieMeshes",
        json!({"entityID": 70, "nodeIDs": [43], "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(err.data["nodeID"], json!(43));
    assert_eq!(err.data["parentNodeID"], json!(71));
    assert_eq!(
        err.data["reason"].as_str().unwrap_or_default(),
        "nodeID 43 is already tied to nodeID 71"
    );
    assert_eq!(epoch(&mb).await, e, "the refusal ran nothing");
    assert!(tree(&mb).contains(&(43, String::new(), Some(71))));

    // allowRetie moves it and deletes 71 (emptied) in the same group
    let r = call(
        &mb,
        "entity.tieMeshes",
        json!({
            "entityID": 70, "nodeIDs": [43], "allowRetie": true,
            "expectDoc": {"id": "d1", "epoch": e}
        }),
    )
    .await;
    assert_eq!(r["removedEmptyEntities"], json!([71]));
    assert_eq!(r["alreadyTied"], json!([]));
    let rows = tree(&mb);
    assert!(!rows.iter().any(|(id, _, _)| *id == 71), "{rows:?}");
    assert!(rows.contains(&(43, String::new(), Some(70))), "{rows:?}");

    // one undo restores BOTH: 71 back, with 43 under it again
    let undo = call(
        &mb,
        "undo.undo",
        json!({"expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(undo["name"], json!("MCP: tie meshes to entity"));
    let rows = tree(&mb);
    assert!(
        rows.contains(&(71, "trigger_once".to_string(), None)),
        "{rows:?}"
    );
    assert!(rows.contains(&(43, String::new(), Some(71))), "{rows:?}");
    assert!(rows.contains(&(42, String::new(), Some(70))), "{rows:?}");
}

/// The `class? | entityID?` gate of §5: exactly one, `keyValues` only with `class`, a non-brush
/// target refused — all before the undo group opens.
#[tokio::test]
async fn tie_meshes_class_and_entity_id_gates_leave_nothing_behind() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "point")])),
        MockNode::entity(70, "trigger_multiple", [0.0; 3], BTreeMap::new()),
        MockNode::entity(80, "info_target", [0.0; 3], BTreeMap::new()),
        mesh_node(41),
    ])
    .await;
    let e = epoch(&mb).await;

    // class / entityID: exactly one
    for params in [
        json!({"class": "trigger_once", "entityID": 70, "nodeIDs": [41]}),
        json!({"nodeIDs": [41]}),
        json!({"entityID": null, "nodeIDs": [41]}),
    ] {
        let mut params = params;
        params["expectDoc"] = json!({"id": "d1", "epoch": e});
        let err = call_err(&mb, "entity.tieMeshes", params).await;
        assert_eq!(err.code, Some(-32602), "{err:?}");
        assert!(
            err.data["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("class"),
            "{err:?}"
        );
    }
    // entityID: the class-form arguments are refused
    let mut params = json!({"entityID": 70, "nodeIDs": [41], "keyValues": {"wait": "1"}});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    let err = call_err(&mb, "entity.tieMeshes", params).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("keyValues"),
        "{err:?}"
    );
    // entityID of a point entity / a missing id
    let mut params = json!({"entityID": 80, "nodeIDs": [41]});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    let err = call_err(&mb, "entity.tieMeshes", params).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not a brush entity"),
        "{err:?}"
    );
    let mut params = json!({"entityID": 999, "nodeIDs": [41]});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    let err = call_err(&mb, "entity.tieMeshes", params).await;
    assert_eq!(err.code, Some(-32022), "{err:?}");
    // ... and an entityID that is missing carries NO nodeID (the bridge's distinction), which is
    // what lets the tool name the entity instead of the meshes (A3)
    assert!(err.data.get("nodeID").is_none(), "{err:?}");
    // a class that is not a solid class (the bridge's defensive gdType != 5 check, which runs
    // inside the group it then leaves behind: `undoGroupOpened` marks it)
    let mut params = json!({"class": "info_target", "nodeIDs": [41]});
    params["expectDoc"] = json!({"id": "d1", "epoch": e});
    let err = call_err(&mb, "entity.tieMeshes", params).await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not a solid"),
        "{err:?}"
    );
    assert_eq!(err.data["undoGroupOpened"], json!(true), "{err:?}");

    // none of them created anything, moved a mesh or opened a group
    assert_eq!(epoch(&mb).await, e, "no mutation reached the document");
    assert_eq!(
        mb.with_state(|st| st.hammer.as_ref().expect("world").undo.len()),
        0
    );
    assert_eq!(mb.hammer_nodes().len(), 5);
}

/// `entity.untie` (m2.19): the tied meshes go back under the world and the entity is deleted, one
/// undoable `"Untie from entity"` group; a point entity, a non-entity and a locked entity are
/// refused before the group opens.
#[tokio::test]
async fn untie_returns_the_meshes_and_deletes_the_entity() {
    let mb = start_world(vec![
        MockNode::world(),
        MockNode::entity(30, "info_target", [0.0; 3], kv(&[("targetname", "point")])),
        MockNode {
            node_id: 41,
            class: "mesh".into(),
            classname: String::new(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        },
    ])
    .await;
    let e = epoch(&mb).await;

    // a point entity: no mesh children -> "not a brush entity"
    let err = call_err(
        &mb,
        "entity.untie",
        json!({"nodeID": 30, "expectDoc": {"id": "d1", "epoch": e}}),
    )
    .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert!(
        err.data["reason"].as_str().unwrap_or_default().contains("not a brush entity"),
        "{err:?}"
    );
    // the world is not an entity, a missing id is NodeNotFound
    let err = call_err(&mb, "entity.untie", json!({"nodeID": 1, "expectDoc": {"id": "d1", "epoch": e}}))
        .await;
    assert_eq!(err.code, Some(-32602), "{err:?}");
    let err = call_err(&mb, "entity.untie", json!({"nodeID": 999, "expectDoc": {"id": "d1", "epoch": e}}))
        .await;
    assert_eq!(err.code, Some(-32022), "{err:?}");
    assert_eq!(epoch(&mb).await, e, "every refusal ran nothing");

    // a real tie first: build the brush entity through the tie, so the pair is covered end to end
    let mut params = json!({"class": "trigger_once", "nodeIDs": [41]});
    params["expectDoc"] = json!({"id": "d1", "epoch": epoch(&mb).await});
    let tied = call(&mb, "entity.tieMeshes", params).await;
    let entity = tied["nodeID"].as_i64().expect("nodeID");

    let r = call(
        &mb,
        "entity.untie",
        json!({"nodeID": entity, "expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(r["nodeID"], json!(entity));
    assert_eq!(r["meshes"], json!([41]));
    assert_eq!(r["entityDeleted"], json!(true));
    assert_eq!(r["undoLabel"], json!("Untie from entity"));
    let rows = tree(&mb);
    assert!(!rows.iter().any(|(id, _, _)| *id == entity), "the entity is gone");
    assert!(rows.contains(&(41, String::new(), None)), "the mesh is a world node: {rows:?}");

    // undo restores the tie (the entity and the parent edge)
    let undo = call(
        &mb,
        "undo.undo",
        json!({"expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(undo["name"], json!("Untie from entity"));
    assert!(tree(&mb).contains(&(41, String::new(), Some(entity))));

    // a locked brush entity is -32018
    mb.with_state(|state| {
        let world = state.hammer.as_mut().expect("world");
        if let Some(node) = world.nodes.iter_mut().find(|n| n.node_id == entity) {
            node.locked = true;
        }
    });
    let err = call_err(
        &mb,
        "entity.untie",
        json!({"nodeID": entity, "expectDoc": {"id": "d1", "epoch": epoch(&mb).await}}),
    )
    .await;
    assert_eq!(err.code, Some(-32018), "{err:?}");
}

/// `entity.get.children` is capped at 1024 rows with `childrenTruncated` (the spec's own bound).
#[tokio::test]
async fn entity_get_caps_the_children_list() {
    let mut nodes = vec![
        MockNode::world(),
        MockNode::entity(30, "trigger_multiple", [0.0; 3], BTreeMap::new()),
    ];
    for id in 0..1025 {
        nodes.push(
            MockNode {
                node_id: 1000 + id,
                class: "mesh".into(),
                classname: String::new(),
                targetname: None,
                origin: [0.0; 3],
                angles: [0.0; 3],
                keyvalues: BTreeMap::new(),
                locked: false,
                parent: None,
            }
            .with_parent(30),
        );
    }
    let mb = start_world(nodes).await;
    let read = call(&mb, "entity.get", json!({"nodeID": 30})).await;
    let children = read["children"].as_array().expect("children");
    assert_eq!(children.len(), 1024);
    assert_eq!(children[0], json!({"nodeID": 1000, "class": "mesh"}));
    assert_eq!(children[1023], json!({"nodeID": 2023, "class": "mesh"}));
    assert_eq!(read["childrenTruncated"], json!(true), "{read}");
}
