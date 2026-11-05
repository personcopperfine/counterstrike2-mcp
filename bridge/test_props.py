#!/usr/bin/env python3
"""test_props.py - bridge m2.18 prop pass: model draw + value undo, node.setTransform, entity I/O.

The m2.18 pass (WP-B, worked from target/re/re-props.md) is what makes a prop *load and draw*:
`entity.setKeyValues`/`entity.create` now write through Hammer's own `CMapGameDataNode::SetKeyValue`
(which runs the node's OnKeyValueChanged handler, so the helpers/CModelHelper rebuild), record the
node tree so `undo.undo` really reverts the values, and the bridge gained `node.setTransform`,
`entity.addConnections` and `entity.removeConnections` plus the live I/O vector in `entity.get`.

Live mode (default; the orchestrator runs it against a running tools session with probe.vmap open):

  1. version/capabilities: `bridgeVersion` == the Rust bridge's `BRIDGE_BUILD` and the three
     new methods advertised
  2. `entity.create prop_dynamic` (no model, run-unique targetname) -> `entity.find` by that
     targetname gives its nodeID (the create reply carries a handle, not a nodeID)
  3. `view.render` of a camera aimed at the prop: the model-less placeholder box -> crc0
  4. `entity.setKeyValues {model: MODEL_A}` -> `applied:true`, `valueUndo:"recorded"`, and the
     render crc CHANGES (polled with a deadline: the ModelHelper may draw a frame later)
  5. `entity.get` -> `model` is MODEL_A (+ the create-time `scales` echo)
  6. `entity.setKeyValues {model: MODEL_B}` -> the render crc changes again
  7. `undo.undo` -> `entity.get.model` is back to MODEL_A and the render crc is back to crcA
     (m2.18's real value undo; m2.15 could only pop the group)
  8. `node.setTransform` -> the reply reads all three fields back; the render crc changes; the
     angles/scale values are visible in a second call's read-back, then `undo.undo` restores them
     (`entity.get` origin/angles + a read-through write for the scale value)
  9. `entity.addConnections` x2 -> the reply reports both added; `entity.get` lists exactly them
     with the .vmap reader's field names
 10. `entity.removeConnections {match:{...}}` -> exactly one left; `undo.undo` -> both back
 11. the refusals: an empty transform, a 0 scale, an empty/oversized connections array, an
     out-of-range index, both indices+match, a match with no hits (-32026), a mesh for the
     entity-only ops (-32602) -- each must leave the group count where it was
 12. cleanup: pop this run's own groups (by name) and save the map again

Run-unique identity: the targetnames carry a per-run suffix and both props live in band 3
(60000 + jitter on the /16 grid), so leftovers from an aborted run can never satisfy a check
(bridge/README.md "Live test conventions"). Cleanup is best-effort and only pops groups whose name
is one of this run's labels; it is reported but not part of the pass criteria.

`--static` (offline; no bridge, no container): reads the Rust bridge's committed sources and
asserts what is still a source fact - `BRIDGE_BUILD` in crates/cs2-bridge/src/lib.rs (m3.N or
later), the three rows in registry.rs (implemented on NEED_HAMMER, advertised, not blocked),
their bodies in the ops HANDLERS table, and the generated pins they call in
crates/cs2-bridge/src/generated/pins.rs. The C source-shape checks (the ops' undo groups, the
vtable call order, the connection writers, the geo_reset_mesh slot fix, the s2sdk.h asserts,
protocol-diff-m2.18.md and the C host unit test) are retired; bridge/protocol-diff-m5-tests.md
lists each one and the Rust unit test that covers it. A missing file is a FAIL, never a skip.

Usage: bridge/test_props.py [--static] [--prefix <wine pfx>] [--map <host .vmap>]
       [--model-a <vmdl>] [--model-b <vmdl>] [--wait N]
Exit 0 when every check passes.
"""
import argparse
import json
import os
import socket
import sys
import time

DEFAULT_MAP_REL = os.path.join("content", "csgo_addons", "mcp_test", "maps", "probe.vmap")

# both models are loadable in the tools' search paths: A is the game-content chair the RE drew
# live (re-props.md Q1), B is the mcp_test addon's own compiled model
# (game/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl_c exists)
MODEL_A = "models/generic/terrace_set_01/terrace_chair_01.vmdl"
MODEL_B = "models/mcp_probe/glass_probe.vmdl"

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BRIDGE_DIR)
from bridge_build import (BUILD_SHAPE, expected_build, handler_names, methods,  # noqa: E402
                          pin_names, row_problems)
from cs2mcp_root import PFX_DIR, PROJECT_ROOT  # noqa: E402

DEFAULT_PFX = PFX_DIR
DEFAULT_ROOT = PROJECT_ROOT

BUILD = expected_build()                      # the Rust bridge's single BRIDGE_BUILD

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
BAND = 3                                      # bands 0-2 belong to set_keyvalues/edit_batch/undo
RUN_OFF = BAND * 20000.0 + (int(RUN, 16) % 1000) * 16.0
TARGET_A = "mcp_prop_%s" % RUN
TARGET_B = "mcp_props_%s" % RUN
LABELS = ("MCP: create entity", "MCP: set keyvalues", "MCP: set transform",
          "MCP: add connections", "MCP: remove connections")

CHECKS = {}
RENDER_SEQ = [0]


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""))
    return bool(ok)


def poll_until(fn, deadline_s):
    """bounded poll on a real signal (fn -> truthy); no fixed sleep beyond the tick"""
    end = time.time() + deadline_s
    while True:
        v = fn()
        if v:
            return v
        if time.time() >= end:
            return None
        time.sleep(0.4)


# ---- live -------------------------------------------------------------------------------------

def live_report(args) -> int:
    map_path = args.map or os.path.join(args.root, DEFAULT_MAP_REL)
    disc_path = os.path.join(args.prefix, "drive_c", "users", "steamuser", "AppData", "Local",
                             "cs2mcpbridge", "session.json")
    if args.wait > 0 and not poll_until(lambda: os.path.exists(disc_path), args.wait):
        print(f"FAIL: discovery file not found: {disc_path}")
        return 2
    disc = json.load(open(disc_path))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
    f = s.makefile("r")
    n = [0]

    def call(method, params=None):
        n[0] += 1
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": "p%d" % n[0], "method": method,
                               "params": params or {}}) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    def err(r):
        return r.get("error") or {}

    call("control.hello", {"protocol": 1, "token": disc["token"]})

    # 1. version + capabilities
    r = call("control.version", {})
    ver = (r.get("result") or {}).get("bridgeVersion")
    step("version", ver == BUILD, f"bridgeVersion={ver!r} expected {BUILD!r}")
    r = call("control.capabilities", {})
    methods = set((r.get("result") or {}).get("methods") or [])
    want = {"node.setTransform", "entity.addConnections", "entity.removeConnections"}
    step("methods_advertised", want <= methods, f"missing={sorted(want - methods)}")

    d0 = call("doc.active", {})
    doc = (d0.get("result") or {}).get("doc") or {}
    if not doc:
        print("FAIL: no document open:", d0)
        s.close()
        return 2
    print(f"baseline: id={doc['id']} epoch={doc['epoch']} editable={doc['editable']} "
          f"undoDepth={doc['undoDepth']} run={RUN}")
    if not step("doc_editable", doc.get("editable") is True, f"editable={doc.get('editable')}"):
        s.close()
        return 1
    base_depth = doc["undoDepth"]
    expect = {"id": doc["id"], "epoch": doc["epoch"]}

    # 2. create prop A (no model) and find its nodeID through the run-unique targetname: the
    # create reply itself already names it (m2.18 review W2: entity.create carries the nodeID)
    ox, oy, oz = 64.0 + RUN_OFF, 64.0 + RUN_OFF, 512.0
    r = call("entity.create", {"class": "prop_dynamic", "origin": [ox, oy, oz], "angles": [0, 0, 0],
                               "keyValues": {"targetname": TARGET_A}, "expectDoc": expect})
    res = r.get("result") or {}
    step("create_a", "error" not in r and res.get("class") == "prop_dynamic"
         and isinstance(res.get("nodeID"), int),
         f"resp={r if 'error' in r else res.get('handle')} nodeID={res.get('nodeID')}")
    node_a = None
    def find_a():
        rr = call("entity.find", {"targetname": TARGET_A})
        hits = (rr.get("result") or {}).get("entities") or []
        return hits[0]["nodeID"] if len(hits) == 1 else None
    node_a = poll_until(find_a, 10.0)
    step("find_a", node_a is not None, f"nodeID={node_a} (a run-unique targetname must hit once)")
    step("create_a_nodeid_matches_find", res.get("nodeID") == node_a,
         f"create nodeID={res.get('nodeID')} find nodeID={node_a}")
    if node_a is None:
        s.close()
        return 2

    # 3. the render baseline: the model-less prop draws the white placeholder box
    cam = {"origin": [ox - 320.0, oy, oz + 160.0], "angles": [26.5, 0.0, 0.0]}

    def render(name):
        RENDER_SEQ[0] += 1
        r = call("view.render", {"width": 640, "height": 360, "fov": 60, "name": name,
                                 "origin": cam["origin"], "angles": cam["angles"]})
        if r.get("error"):
            return None, r
        return (r.get("result") or {}).get("stats", {}).get("crc32"), r

    crc0, r0 = render("mcp_props_box")
    step("render_baseline", crc0 is not None, f"crc0={crc0} resp={r0 if crc0 is None else 'ok'}")
    if crc0 is None:
        s.close()
        return 2

    def poll_crc(want_diff, label):
        """render until the crc is not `want_diff` (the ModelHelper may draw a frame later)"""
        seen = [None]
        def tick():
            crc, _ = render("mcp_props_%s" % label)
            seen[0] = crc
            return crc if crc and crc != want_diff else None
        return poll_until(tick, args.render_wait), seen[0]

    # 4. the model write: SetKeyValue -> the helpers rebuild -> the chair draws
    r = call("entity.setKeyValues", {"nodeID": node_a, "values": {"model": args.model_a},
                                     "expectDoc": expect})
    res = r.get("result") or {}
    step("set_model_a", (res.get("applied") or {}).get("model") is True
         and res.get("valueUndo") == "recorded",
         f"applied={res.get('applied')} valueUndo={res.get('valueUndo')!r}")
    crc_a, seen = poll_crc(crc0, "model_a")
    step("render_model_a", crc_a is not None,
         f"crc_a={crc_a} (baseline {crc0}, last rendered {seen})")

    # 5. entity.get shows the model (and the loop is the same one the tools read)
    r = call("entity.get", {"nodeID": node_a})
    got = r.get("result") or {}
    kv = got.get("keyValues") or {}
    step("get_model_a", kv.get("model") == args.model_a,
         f"model={kv.get('model')!r}")
    step("get_connections_empty", got.get("connections") == [],
         f"connections={got.get('connections')!r} (a live vector, not the old null)")

    # 6. a second model: the render changes again
    r = call("entity.setKeyValues", {"nodeID": node_a, "values": {"model": args.model_b},
                                     "expectDoc": expect})
    step("set_model_b", ((r.get("result") or {}).get("applied") or {}).get("model") is True,
         f"resp={r if 'error' in r else 'ok'}")
    crc_b, seen = poll_crc(crc_a, "model_b")
    step("render_model_b", crc_b is not None, f"crc_b={crc_b} (crc_a {crc_a}, last {seen})")

    # 7. undo reverts the VALUE and the drawn model (m2.18's real value undo)
    r = call("undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("undo_model_b", res.get("undone") is True and res.get("name") == "MCP: set keyvalues",
         f"resp={r}")
    r = call("entity.get", {"nodeID": node_a})
    kv = (r.get("result") or {}).get("keyValues") or {}
    step("undo_model_value", kv.get("model") == args.model_a,
         f"model={kv.get('model')!r} (must be back to {args.model_a!r})")
    back, seen = poll_crc(crc_b, "undo_back")
    step("undo_render_back", back == crc_a,
         f"crc={back} expected crc_a={crc_a} (last {seen})")

    # 8. node.setTransform: create-time scales, then origin/angles/scales on an existing node
    ox2 = ox + 512.0
    r = call("entity.create", {"class": "prop_dynamic", "origin": [ox2, oy, oz], "angles": [0, 0, 0],
                               "scales": [2.0, 2.0, 2.0], "keyValues": {"targetname": TARGET_B},
                               "expectDoc": expect})
    res = r.get("result") or {}
    step("create_b_scales", res.get("scales") == [2.0, 2.0, 2.0] and res.get("class") == "prop_dynamic",
         f"scales={res.get('scales')}")
    def find_b():
        rr = call("entity.find", {"targetname": TARGET_B})
        hits = (rr.get("result") or {}).get("entities") or []
        return hits[0]["nodeID"] if len(hits) == 1 else None
    node_b = poll_until(find_b, 10.0)
    step("find_b", node_b is not None, f"nodeID={node_b}")
    step("create_b_nodeid_matches_find", res.get("nodeID") == node_b,
         f"create nodeID={res.get('nodeID')} find nodeID={node_b}")
    if node_b is None:
        s.close()
        return 2
    # entity.get reads the field back (m2.18 added it: scales are a C++ field, not a keyvalue)
    r = call("entity.get", {"nodeID": node_b})
    got = r.get("result") or {}
    step("get_b_scales", got.get("scales") == [2.0, 2.0, 2.0],
         f"scales={got.get('scales')} (the create-time setter really stored 2,2,2)")
    # a setKeyValues with the model on the scaled prop, so the transform has something to move;
    # its own draw is waited for first, so the transform's render check below is not satisfied by
    # this model appearing
    r = call("entity.setKeyValues", {"nodeID": node_b, "values": {"model": args.model_a},
                                     "expectDoc": expect})
    step("set_model_b_prop", ((r.get("result") or {}).get("applied") or {}).get("model") is True,
         f"resp={r if 'error' in r else 'ok'}")
    crc_b_model, seen = poll_crc(crc_a, "prop_b_model")
    step("render_prop_b_model", crc_b_model is not None,
         f"crc={crc_b_model} (crc_a {crc_a}, last {seen})")

    # the transform the undo must restore: read origin/angles first (entity.get), scale through the
    # create-time reply (2,2,2) - node.setTransform is a write, so the scale read-back below is the
    # second call's reply, not a read of its own
    r = call("entity.get", {"nodeID": node_b})
    before = r.get("result") or {}
    origin0, angles0 = before.get("origin"), before.get("angles")
    want_origin = [ox2 + 64.0, oy, oz + 32.0]
    want_angles = [0.0, 45.0, 0.0]
    want_scales = [1.5, 1.5, 1.5]
    r = call("node.setTransform", {"nodeID": node_b, "origin": want_origin, "angles": want_angles,
                                   "scales": want_scales, "expectDoc": expect})
    res = r.get("result") or {}
    ok_vals = (res.get("nodeID") == node_b and res.get("class") == "entity"
               and res.get("origin") == want_origin and res.get("angles") == want_angles
               and res.get("scales") == want_scales
               and res.get("undoLabel") == "MCP: set transform")
    step("transform_reply", ok_vals,
         f"origin={res.get('origin')} angles={res.get('angles')} scales={res.get('scales')} "
         f"undoLabel={res.get('undoLabel')!r}")
    step("transform_doc_tail", isinstance((res.get("doc") or {}).get("topSerial"), int),
         f"doc={res.get('doc')}")
    r = call("entity.get", {"nodeID": node_b})
    got = r.get("result") or {}
    step("transform_applied", got.get("origin") == want_origin
         and got.get("angles") == want_angles,
         f"origin={got.get('origin')} angles={got.get('angles')}")
    moved, seen = poll_crc(crc_b_model, "transform")
    step("transform_render", moved is not None, f"crc={moved} (was {crc_b_model}, last {seen})")

    # undo the transform: origin/angles come straight back out of entity.get, and the scale value
    # out of the next call's read-back (a same-value write, popped again in the cleanup)
    r = call("undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("undo_transform", res.get("undone") is True and res.get("name") == "MCP: set transform",
         f"resp={r}")
    r = call("entity.get", {"nodeID": node_b})
    got = r.get("result") or {}
    step("undo_transform_values", got.get("origin") == origin0 and got.get("angles") == angles0,
         f"origin={got.get('origin')} (was {origin0}) angles={got.get('angles')} (was {angles0})")
    r = call("node.setTransform", {"nodeID": node_b, "origin": want_origin, "expectDoc": expect})
    res = r.get("result") or {}
    step("undo_transform_scales", res.get("scales") == [2.0, 2.0, 2.0],
         f"scales={res.get('scales')} (the undo restored the create-time 2,2,2)")

    # 9. entity.addConnections x2
    r = call("entity.addConnections",
             {"nodeID": node_b, "expectDoc": expect,
              "connections": [
                  {"output": "OnUser1", "target": TARGET_A, "input": "Kill", "delay": 0.5,
                   "timesToFire": -1},
                  {"output": "OnUser2", "target": TARGET_B, "input": "SetAnimation",
                   "param": "idle", "delay": 1.0, "timesToFire": 2},
              ]})
    res = r.get("result") or {}
    step("add_connections", res.get("count") == 2 and len(res.get("added") or []) == 2
         and all(isinstance(a.get("index"), int) for a in res.get("added") or []),
         f"added={res.get('added')}")
    # every wire entry is confirmed: `landed[i]` (review W2 #3) is per wire entry, `index` the
    # live index it landed on, so a readback:false entry can never hide behind a bare count
    landed = res.get("landed") or []
    step("add_landed_reports_every_entry",
         len(landed) == 2 and all(e.get("landed") is True for e in landed)
         and [e.get("index") for e in landed] == [0, 1], f"landed={landed}")
    r = call("entity.get", {"nodeID": node_b})
    conns = (r.get("result") or {}).get("connections") or []
    step("get_connections_two", len(conns) == 2
         and [c.get("outputName") for c in conns] == ["OnUser1", "OnUser2"]
         and [c.get("delay") for c in conns] == [0.5, 1.0]
         and [c.get("timesToFire") for c in conns] == [-1, 2]
         and all(c.get("targetType") == 7 for c in conns),
         f"connections={conns}")

    # 10. removeConnections by match -> one left, then undo -> two again
    r = call("entity.removeConnections",
             {"nodeID": node_b, "match": {"output": "OnUser1", "target": TARGET_A},
              "expectDoc": expect})
    res = r.get("result") or {}
    step("remove_by_match", res.get("count") == 1 and res.get("removed") == [0]
         and res.get("remaining") == 1,
         f"removed={res.get('removed')} count={res.get('count')} (the removed count) "
         f"remaining={res.get('remaining')} (the vector left)")
    r = call("entity.get", {"nodeID": node_b})
    conns = (r.get("result") or {}).get("connections") or []
    step("remove_left_one", len(conns) == 1 and conns[0].get("outputName") == "OnUser2",
         f"connections={conns}")
    r = call("undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("undo_remove", res.get("undone") is True
         and res.get("name") == "MCP: remove connections", f"resp={r}")
    r = call("entity.get", {"nodeID": node_b})
    conns = (r.get("result") or {}).get("connections") or []
    step("undo_remove_restored", len(conns) == 2, f"connections={conns}")

    # 11. refusals: every one of them must leave the undo depth where it was
    def depth():
        return ((call("doc.active", {}).get("result") or {}).get("doc") or {}).get("undoDepth")

    depth0 = depth()
    cases = [
        ("transform_no_fields", "node.setTransform", {"nodeID": node_b, "expectDoc": expect}, -32602),
        ("transform_zero_scale", "node.setTransform",
         {"nodeID": node_b, "scales": [0.0, 1.0, 1.0], "expectDoc": expect}, -32602),
        ("transform_over_origin", "node.setTransform",
         {"nodeID": node_b, "origin": [0.0, 0.0, 2000000.0], "expectDoc": expect}, -32602),
        ("add_empty", "entity.addConnections",
         {"nodeID": node_b, "connections": [], "expectDoc": expect}, -32602),
        ("add_missing_output", "entity.addConnections",
         {"nodeID": node_b, "connections": [{"target": TARGET_A, "input": "Kill"}],
          "expectDoc": expect}, -32602),
        ("add_long_output", "entity.addConnections",
         {"nodeID": node_b, "connections": [{"output": "x" * 300, "target": TARGET_A,
                                             "input": "Kill"}], "expectDoc": expect}, -32602),
        ("add_bad_times", "entity.addConnections",
         {"nodeID": node_b, "connections": [{"output": "o", "target": TARGET_A, "input": "i",
                                             "timesToFire": 0}], "expectDoc": expect}, -32602),
        ("remove_both", "entity.removeConnections",
         {"nodeID": node_b, "indices": [0], "match": {"output": "OnUser1"}, "expectDoc": expect}, -32602),
        ("remove_neither", "entity.removeConnections", {"nodeID": node_b, "expectDoc": expect}, -32602),
        ("remove_out_of_range", "entity.removeConnections",
         {"nodeID": node_b, "indices": [99], "expectDoc": expect}, -32602),
        ("remove_no_match", "entity.removeConnections",
         {"nodeID": node_b, "match": {"output": "NoSuchOutput"}, "expectDoc": expect}, -32026),
        ("remove_missing_output", "entity.removeConnections",
         {"nodeID": node_b, "match": {"output": ""}, "expectDoc": expect}, -32602),
        ("remove_unknown_filter", "entity.removeConnections",
         {"nodeID": node_b, "match": {"bogus": "x"}, "expectDoc": expect}, -32602),
        ("remove_long_filter", "entity.removeConnections",
         {"nodeID": node_b, "match": {"output": "x" * 300}, "expectDoc": expect}, -32602),
    ]
    for name, method, params, want in cases:
        r = call(method, params)
        code = err(r).get("code")
        step("refuse_" + name, code == want, f"code={code} expected {want} resp={r if code != want else ''}")
    # the origin refusal names the bound the gate really uses (review W2 #5: the text said 65536
    # after the gate had moved to 1048576 - a client that "fixed" its value from the text stayed
    # wrong). Read out of the error's data.reason.
    r = call("node.setTransform",
             {"nodeID": node_b, "origin": [0.0, 0.0, 2000000.0], "expectDoc": expect})
    reason = (err(r).get("data") or {}).get("reason") or ""
    step("refuse_origin_text_names_the_bound",
         "1048576" in reason and "65536" not in reason, f"reason={reason!r}")
    step("refusals_did_not_open_groups", depth() == depth0,
         f"undoDepth={depth()} expected {depth0}")

    # 12. cleanup: pop this run's groups (their labels are ours - stop at anything else) and save
    cleanup = {"popped": []}
    for _ in range(24):
        if depth() <= base_depth:
            break
        r = call("undo.undo", {"expectDoc": expect})
        res = r.get("result") or {}
        if res.get("undone") is not True or res.get("name") not in LABELS:
            cleanup["stopped_at"] = res.get("name")
            break
        cleanup["popped"].append(res.get("name"))
    cleanup["depth_restored"] = depth() == base_depth
    r = call("map.save", {})
    cleanup["saved"] = (r.get("result") or {}).get("saved")
    print(f"cleanup: {cleanup}")

    s.close()
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    if not all(k for k in (cleanup.get("depth_restored"), cleanup.get("saved"))):
        print("CLEANUP INCOMPLETE (not part of the pass criteria)")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# ---- offline (--static) ------------------------------------------------------------------------

def static_report() -> int:
    """the Rust bridge contract for this pass: the three rows, their bodies and their pins."""
    try:
        build = expected_build()
        rows = methods()
        served = handler_names()
        pins = pin_names()
    except (OSError, RuntimeError) as ex:
        print(f"FAIL: cannot read the Rust bridge sources: {ex}")
        return 1

    missing = []
    if BUILD_SHAPE.match(build) is None:
        missing.append(f"BRIDGE_BUILD {build!r} (expected m3.N or later)")

    want_rows = ("node.setTransform", "entity.addConnections", "entity.removeConnections")
    for name in want_rows:
        missing += [f"{name}: {why}" for why in row_problems(rows, name, "NEED_HAMMER")]
    for name in want_rows:
        if name not in served:
            missing.append(f"the {name} handler is not registered in ops/")

    for pin in ("HFN_SET_KEY_VALUE", "HFN_ADD_CONNECTION", "HFN_REMOVE_CONNECTION",
                "HFN_KV3_INIT", "HFN_KV3_FREE"):
        if pin not in pins:
            missing.append(f"{pin} missing from crates/cs2-bridge/src/generated/pins.rs")

    step("static_props_contract", not missing, f"missing={missing}")
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true",
                    help="offline source expectations (m2.18 rows, ops, pins, bug fix)")
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--root", default=DEFAULT_ROOT, help="the project root (map default)")
    ap.add_argument("--map", default=None, help="host path of the open .vmap (default probe.vmap)")
    ap.add_argument("--model-a", default=MODEL_A)
    ap.add_argument("--model-b", default=MODEL_B)
    ap.add_argument("--render-wait", type=float, default=20.0,
                    help="seconds to poll view.render after a model/transform write")
    ap.add_argument("--wait", type=float, default=0.0,
                    help="seconds to wait for the discovery file (0 = it must exist)")
    args = ap.parse_args()
    if args.static:
        return static_report()
    return live_report(args)


if __name__ == "__main__":
    sys.exit(main())
