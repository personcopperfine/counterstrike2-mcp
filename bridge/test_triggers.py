#!/usr/bin/env python3
"""test_triggers.py - trigger pass: entity.tieMeshes, entity.untie, entity.get children.

The trigger semantics landed in the C bridge's m2.19 pass (WP-B, worked from
target/re-triggers.md + target/plan-triggers.md §5) and are carried by the Rust bridge's
method table; the live version check below compares against that bridge's `BRIDGE_BUILD`.
That pass turns meshes into brush entities the way Hammer's own Tie To Entity does,
reimplemented (Hammer's command reads the UI selection and would open a modal on an empty one):

  - `entity.tieMeshes {class? | entityID?, nodeIDs:[1..64 distinct], keyValues?, allowRetie?,
    expectDoc}` -> one undo group "MCP: tie meshes to entity": CreateNodeByName at the FIRST
    mesh's origin/angles (Hammer's rule, not the bounds centre) or the entityID brush entity,
    keyValues through the entity.create SetKeyValue path, RecordNodesAndParents BEFORE the
    AddChild calls, one AddChild (slot 57) per mesh, emptied old brush entities deleted in the
    same group and reported in removedEmptyEntities. Meshes keep their world geometry (node
    transforms are world-space; [V live] 2026-10-16).
  - `entity.untie {nodeID, expectDoc}` -> Hammer's own CMapDoc::MoveToWorld with a 1-entry vector;
    it opens its OWN undo group ("Untie from entity") so the bridge opens none; the meshes go back
    to the entity's parent and the entity is deleted.
  - `entity.get` gained `children:[{nodeID, class}]` (one level, <= 1024, `childrenTruncated`/
    `childrenTotal` past that, `null` when the child array is unreadable).

Live mode (default; the ORCHESTRATOR runs it against a running tools session with a scratch map
open - a throwaway map, this test ties and unties real trigger volumes):

  1. version/capabilities: `bridgeVersion` == the Rust bridge's `BRIDGE_BUILD` and the two new
     methods advertised
  2. `mesh.create` a box far from the origin (band 4, x ~ 80000 - the run-unique convention puts
     each test's coordinate band 20000 units apart; "far from the origin" is what matters, and
     band 0-3 belong to the kv/batch/undo/props tests) -> tie it as `trigger_once` via `class`:
     the reply's origin == the mesh origin, `created:true`, `entity.get` children == [mesh],
     spawnflags keyvalue "4097"
  3. a second mesh into the same trigger via `entityID` -> `created:false`, 2 children
  4. a third mesh into a second trigger; retying it into the first WITHOUT `allowRetie` is
     -32602 "already tied" {nodeID, parentNodeID} with the undo depth unchanged; WITH
     `allowRetie` it moves, the emptied second trigger is deleted and reported in
     removedEmptyEntities
  5. the refusal matrix - every row re-reads doc.active right before the refusal it checks
     (FIX-B L2: a fresh baseline per row, never a running counter, so one bug cannot cascade);
     the all-alreadyTied call is a SUCCESS no-op now (FIX-B L1: no undo group, alreadyTied
     reported, doc unchanged) - the old suite wrongly expected -32602 there. A point class sent
     via `class` stays the one documented in-group refusal: it passes the registry gate, is
     created inside the group, fails the defensive gdType check there and is deleted again - the
     group stays open as a no-op (undoDepth +1 until one `undo.undo` pops it) and the error
     carries undoGroupOpened/undoLabel so the caller knows (FIX-B L3)
  6. `entity.untie` on the trigger -> the meshes back under the world with unchanged origins, the
     entity gone; one `undo.undo` (label "Untie from entity") restores entity + children
  7. cleanup: pop this run's own groups (by name) and save the map again

   Rows a session cannot run print an explicit `NOT RUN: <row> (<why>)` line and are summarised
   at the end - never a silent pass (the lock row: nothing in the method table can set a lock;
   the prefab row: no prefab/instance in the scratch map).

`--static` (offline; no bridge, no container): reads the Rust bridge's committed sources and
asserts what is still a source fact - `BRIDGE_BUILD` in crates/cs2-bridge/src/lib.rs (m3.N or
later), the entity.tieMeshes / entity.untie rows in registry.rs (implemented on NEED_HAMMER,
advertised, not blocked), and the two generated pins in crates/cs2-bridge/src/generated/pins.rs.
The C source-shape checks this script used to make (the dispatch arms, the tie op's
validation-before-undo_open order, the untie op's no-undo_open rule, the gdType checks, the
children writer, the s2sdk.h asserts and protocol-diff-m2.19.md) are retired -
bridge/protocol-diff-m5-tests.md lists each one with the Rust unit test that covers it.
**Source shape only**: it never compiles or runs anything, so it proves the table's text, not
behaviour - the live run above is what verifies the bridge (review-b M1). A missing file is a
FAIL, never a skip.

Usage: bridge/test_triggers.py [--static] [--prefix <wine pfx>] [--map <host .vmap>]
       [--base-ref <git ref>] [--wait N]
Exit 0 when every check passes.
"""
import argparse
import json
import os
import socket
import sys
import time

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BRIDGE_DIR)
from bridge_build import (BUILD_SHAPE, expected_build, handler_names, methods,  # noqa: E402
                          pin_names, row_problems)
from cs2mcp_root import PFX_DIR  # noqa: E402

BUILD = expected_build()                      # the Rust bridge's single BRIDGE_BUILD

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
BAND = 4                                      # bands 0-2 kv/batch/undo, 3 props, 4 triggers
RUN_OFF = BAND * 20000.0 + (int(RUN, 16) % 1000) * 16.0
TN1 = "mcp_trig_%s" % RUN
TN2 = "mcp_trigs_%s" % RUN
MAT = "materials/tools/toolstrigger.vmat"
LABELS = ("MCP: create mesh", "MCP: tie meshes to entity", "MCP: create entity",
          "Untie from entity")

CHECKS = {}


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


def box(x0, y0, z0, x1, y1, z1):
    """an axis-aligned box as mesh.create vertices/faces (outward quads, test_geometry's winding)"""
    v = [[x0, y0, z0], [x1, y0, z0], [x1, y1, z0], [x0, y1, z0],
         [x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]]
    f = [[0, 3, 2, 1], [4, 5, 6, 7], [0, 1, 5, 4], [2, 3, 7, 6], [0, 4, 7, 3], [1, 2, 6, 5]]
    return v, f


# ---- live --------------------------------------------------------------------------------------

def live_report(args) -> int:
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
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": "t%d" % n[0], "method": method,
                               "params": params or {}}) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    def err(r):
        return r.get("error") or {}

    def doc_state():
        return (call("doc.active", {}).get("result") or {}).get("doc") or {}

    def depth():
        return doc_state().get("undoDepth")

    call("control.hello", {"protocol": 1, "token": disc["token"]})

    # 1. version + capabilities
    r = call("control.version", {})
    ver = (r.get("result") or {}).get("bridgeVersion")
    step("version", ver == BUILD, f"bridgeVersion={ver!r} expected {BUILD!r}")
    r = call("control.capabilities", {})
    methods = set((r.get("result") or {}).get("methods") or [])
    want = {"entity.tieMeshes", "entity.untie"}
    step("methods_advertised", want <= methods, f"missing={sorted(want - methods)}")

    d0 = doc_state()
    if not d0:
        print("FAIL: no document open - open a scratch map first (the orchestrator's step 0)")
        s.close()
        return 2
    print(f"baseline: id={d0['id']} epoch={d0['epoch']} editable={d0['editable']} "
          f"undoDepth={d0['undoDepth']} topSerial={d0.get('topSerial')} run={RUN}")
    if not step("doc_editable", d0.get("editable") is True, f"editable={d0.get('editable')}"):
        s.close()
        return 1
    base_depth = d0["undoDepth"]
    base_serial = d0.get("topSerial")
    expect = {"id": d0["id"], "epoch": d0["epoch"]}

    def node_origin_of(nid):
        for nd in ((call("node.list", {}).get("result") or {}).get("nodes") or []):
            if nd.get("nodeID") == nid:
                return nd.get("origin")
        return None

    # 2. mesh A far from the origin, tied as trigger_once via class
    ax, ay, az = RUN_OFF, RUN_OFF + 256.0, 0.0
    va, fa = box(ax - 64, ay - 64, az, ax + 64, ay + 64, az + 128)
    r = call("mesh.create", {"vertices": va, "faces": fa, "material": MAT, "expectDoc": expect})
    ma = ((r.get("result") or {}).get("nodeID"))
    step("create_mesh_a", isinstance(ma, int), f"resp={r if ma is None else 'nodeID=%s' % ma}")
    ma_origin = ((r.get("result") or {}).get("origin"))
    if ma is None:
        s.close()
        return 2

    r = call("entity.tieMeshes", {"class": "trigger_once", "nodeIDs": [ma],
                                  "keyValues": {"targetname": TN1, "spawnflags": "4097"},
                                  "expectDoc": expect})
    res = r.get("result") or {}
    e1 = res.get("nodeID")
    step("tie_a", "error" not in r and isinstance(e1, int) and res.get("created") is True
         and res.get("class") == "entity" and res.get("classname") == "trigger_once"
         and res.get("undoLabel") == "MCP: tie meshes to entity",
         f"resp={r if 'error' in r else 'nodeID=%s' % e1}")
    step("tie_a_origin_is_mesh_origin", res.get("origin") == ma_origin,
         f"entity={res.get('origin')} mesh={ma_origin} (Hammer: the FIRST mesh's origin/angles)")
    step("tie_a_meshes_echo",
         [m.get("nodeID") for m in (res.get("meshes") or [])] == [ma]
         and (res.get("meshes") or [{}])[0].get("origin") == ma_origin,
         f"meshes={res.get('meshes')}")
    step("tie_a_doc_tail", isinstance((res.get("doc") or {}).get("undoDepth"), int),
         f"doc={res.get('doc')}")

    r = call("entity.get", {"nodeID": e1})
    got = r.get("result") or {}
    step("get_children_a", got.get("children") == [{"nodeID": ma, "class": "mesh"}],
         f"children={got.get('children')!r}")
    kv = got.get("keyValues") or {}
    step("get_spawnflags_4097", kv.get("spawnflags") == "4097",
         f"spawnflags={kv.get('spawnflags')!r} (FGD default Clients+mass, sent explicitly too)")
    step("get_classname", kv.get("classname") == "trigger_once", f"classname={kv.get('classname')!r}")
    step("get_targetname", kv.get("targetname") == TN1, f"targetname={kv.get('targetname')!r}")
    if e1 is None:
        s.close()
        return 2

    # 3. a second mesh into the SAME trigger via entityID
    bx, by = ax + 256.0, ay
    vb, fb = box(bx - 64, by - 64, az, bx + 64, by + 64, az + 128)
    r = call("mesh.create", {"vertices": vb, "faces": fb, "material": MAT, "expectDoc": expect})
    mb = (r.get("result") or {}).get("nodeID")
    step("create_mesh_b", isinstance(mb, int), f"resp={r if mb is None else 'nodeID=%s' % mb}")
    r = call("entity.tieMeshes", {"entityID": e1, "nodeIDs": [mb], "expectDoc": expect})
    res = r.get("result") or {}
    step("tie_b_entity_form", "error" not in r and res.get("created") is False
         and res.get("nodeID") == e1 and res.get("classname") == "trigger_once",
         f"resp={r if 'error' in r else res.get('nodeID')}")
    r = call("entity.get", {"nodeID": e1})
    kids = (r.get("result") or {}).get("children") or []
    step("get_children_two", sorted(k["nodeID"] for k in kids) == sorted([ma, mb]),
         f"children={kids!r}")

    # 4. a third mesh into a SECOND trigger, then the retie matrix
    cx, cy = ax, ay + 512.0
    vc_, fc_ = box(cx - 64, cy - 64, az, cx + 64, cy + 64, az + 128)
    r = call("mesh.create", {"vertices": vc_, "faces": fc_, "material": MAT, "expectDoc": expect})
    mc = (r.get("result") or {}).get("nodeID")
    step("create_mesh_c", isinstance(mc, int), f"resp={r if mc is None else 'nodeID=%s' % mc}")
    r = call("entity.tieMeshes", {"class": "trigger_once", "nodeIDs": [mc],
                                  "keyValues": {"targetname": TN2}, "expectDoc": expect})
    e2 = ((r.get("result") or {}).get("nodeID"))
    step("tie_c_second_trigger", isinstance(e2, int), f"resp={r if e2 is None else 'nodeID=%s' % e2}")

    d_before = depth()
    s_before = (doc_state() or {}).get("topSerial")
    r = call("entity.tieMeshes", {"entityID": e1, "nodeIDs": [mc], "expectDoc": expect})
    e = err(r)
    step("retie_refused", e.get("code") == -32602 and e.get("message") == "InvalidParams",
         f"resp={r}")
    data = e.get("data") or {}
    step("retie_refused_names_nodes", data.get("nodeID") == mc and data.get("parentNodeID") == e2,
         f"data={data} (the Rust tool's MeshAlreadyTied fields)")
    step("retie_refused_doc_unchanged", depth() == d_before and doc_state().get("topSerial") == s_before,
         f"undoDepth={depth()} (expected {d_before}) topSerial={doc_state().get('topSerial')}")

    r = call("entity.tieMeshes", {"entityID": e1, "nodeIDs": [mc], "allowRetie": True,
                                  "expectDoc": expect})
    res = r.get("result") or {}
    step("retie_allowed", "error" not in r and res.get("removedEmptyEntities") == [e2],
         f"resp={r if 'error' in r else res.get('removedEmptyEntities')}")
    r = call("entity.get", {"nodeID": e2})
    step("emptied_trigger_gone", (err(r).get("code")) == -32022, f"resp={r}")
    r = call("entity.get", {"nodeID": e1})
    kids = [k["nodeID"] for k in ((r.get("result") or {}).get("children") or [])]
    step("retie_children_three", sorted(kids) == sorted([ma, mb, mc]), f"children={kids!r}")

    # geometry unchanged: the tie must not transform (node.list origins before/after)
    o_a = node_origin_of(ma)
    step("tie_keeps_world_origin", o_a == ma_origin,
         f"node.list origin={o_a} mesh.create origin={ma_origin} (world-space transforms)")

    # 5. the refusal matrix - every row re-reads doc.active RIGHT BEFORE the refusal it checks,
    #    so one buggy row's bookkeeping cannot cascade into the others (FIX-B live finding L2).
    #    The all-alreadyTied call is a SUCCESS no-op now (FIX-B L1), so it moved out of the
    #    refusal cases into noop_already_tied below.
    dx, dy = ax + 768.0, ay
    vd, fd = box(dx - 64, dy - 64, az, dx + 64, dy + 64, az + 128)
    r = call("mesh.create", {"vertices": vd, "faces": fd, "material": MAT, "expectDoc": expect})
    md = (r.get("result") or {}).get("nodeID")
    step("create_mesh_d", isinstance(md, int), f"resp={r if md is None else 'nodeID=%s' % md}")
    if md is None:
        s.close()
        return 2

    r = call("entity.create", {"class": "info_target", "origin": [dx, dy, az + 256.0],
                               "expectDoc": expect})
    e_pt = ((r.get("result") or {}).get("nodeID"))
    step("create_point_entity", isinstance(e_pt, int),
         f"resp={r if e_pt is None else 'nodeID=%s' % e_pt}")
    if e_pt is None:
        s.close()
        return 2

    NOTRUN = []

    def notrun(name, why):
        NOTRUN.append({"row": name, "why": why})
        print(f"NOT RUN: {name} ({why})")

    def serial():
        return (doc_state() or {}).get("topSerial")

    def refuse_row(name, method, params, want_code=None, want_message=None, want_reason=None,
                   want_data=None, check_doc=True):
        """one refusal row with a FRESH doc.active baseline taken right before the call
        (FIX-B L2: never a running counter); extra data fields can be pinned via want_data"""
        d0, s0 = depth(), serial()
        r = call(method, params)
        e_ = err(r)
        bad = []
        if want_code is not None and e_.get("code") != want_code:
            bad.append(f"code={e_.get('code')} expected {want_code}")
        if want_message is not None and e_.get("message") != want_message:
            bad.append(f"message={e_.get('message')!r} expected {want_message!r}")
        if want_reason is not None and (e_.get("data") or {}).get("reason") != want_reason:
            bad.append(f"reason={(e_.get('data') or {}).get('reason')!r} expected {want_reason!r}")
        for k, v in (want_data or {}).items():
            if (e_.get("data") or {}).get(k) != v:
                bad.append(f"data.{k}={(e_.get('data') or {}).get(k)!r} expected {v!r}")
        if check_doc:
            d1, s1 = depth(), serial()
            if d1 != d0 or s1 != s0:
                bad.append(f"doc moved: depth {d0}->{d1} serial {s0}->{s1}")
        step(name, not bad, "; ".join(bad) or f"resp={r}")
        return r

    def call_raw(method, params_text):
        """send params as raw JSON text (the malformed-keyValues row must send {"a":}, which
        json.dumps cannot build)"""
        n[0] += 1
        s.sendall(('{"jsonrpc":"2.0","id":"t%d","method":"%s","params":%s}\n'
                   % (n[0], method, params_text)).encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    cases = [
        ("no_target", {"nodeIDs": [md]}, -32602),
        ("both_targets", {"class": "trigger_once", "entityID": e1, "nodeIDs": [md]}, -32602),
        ("kv_with_entity", {"entityID": e1, "nodeIDs": [md], "keyValues": {"x": "1"}}, -32602),
        ("bad_allow_retie", {"entityID": e1, "nodeIDs": [md], "allowRetie": "yes"}, -32602),
        ("empty_node_ids", {"entityID": e1, "nodeIDs": []}, -32602),
        ("node_ids_not_array", {"entityID": e1, "nodeIDs": md}, -32602),
        ("duplicate_node_ids", {"entityID": e1, "nodeIDs": [md, md]}, -32602),
        ("node_id_not_found", {"entityID": e1, "nodeIDs": [999999001]}, -32022),
        ("entity_id_not_found", {"entityID": 999999001, "nodeIDs": [md]}, -32022),
        ("entity_not_mesh", {"entityID": e1, "nodeIDs": [e1]}, -32602),
        ("entity_not_brush", {"entityID": e_pt, "nodeIDs": [md]}, -32602),
    ]
    for name, params, want_code in cases:
        refuse_row("refuse_" + name, "entity.tieMeshes", dict(params, expectDoc=expect),
                   want_code=want_code)

    # FIX-B live L1: every mesh already under the target entity -> a SUCCESS no-op: no undo
    # group, nothing reparented, doc state unchanged (the old suite wrongly expected -32602)
    d0, s0 = depth(), serial()
    r = call("entity.tieMeshes", {"entityID": e1, "nodeIDs": [ma], "expectDoc": expect})
    res = r.get("result") or {}
    step("noop_already_tied", "error" not in r and res.get("created") is False
         and res.get("nodeID") == e1 and res.get("alreadyTied") == [ma]
         and res.get("removedEmptyEntities") == []
         and res.get("noop") is True and res.get("undoLabel", "missing") is None,
         f"resp={r if 'error' in r else res}")
    step("noop_already_tied_doc_unchanged", depth() == d0 and serial() == s0,
         f"depth {d0}->{depth()} serial {s0}->{serial()}")

    # review-b M4: a class the node factory does not know is -32006 UnknownClass, exactly like
    # entity.create (the old -32602 InvalidParams was mock/bridge drift)
    refuse_row("refuse_unknown_class", "entity.tieMeshes",
               {"class": "mcp_no_such_class_%s" % RUN, "nodeIDs": [md], "expectDoc": expect},
               want_code=-32006, want_message="UnknownClass")

    # review-b M2: a retie must refuse when the OLD parent (the entity it would empty and
    # delete) is locked. Nothing in the bridge method table can SET a lock, so the row cannot
    # run - explicit, never a silent pass.
    notrun("refuse_locked_old_parent",
           "no lock method (nothing in k_methods sets a lock; NodeIsLocked is only checked)")

    # review-b L2/L3: classname/origin/angles keyValues and a malformed keyValues object are
    # refused BEFORE the group opens (the old path ran SetKeyValue after the solid-class gate)
    for kvname in ("classname", "origin", "angles"):
        refuse_row("refuse_kv_" + kvname, "entity.tieMeshes",
                   {"class": "trigger_once", "nodeIDs": [md], "keyValues": {kvname: "1 2 3"},
                    "expectDoc": expect},
                   want_code=-32602)
    d0, s0 = depth(), serial()
    r = call_raw("entity.tieMeshes",
                 '{"class":"trigger_once","nodeIDs":[%d],"keyValues":{"a":},"expectDoc":%s}'
                 % (md, json.dumps(expect)))
    e_ = err(r)
    ok = e_.get("code") == -32602 and "malformed" in ((e_.get("data") or {}).get("reason") or "")
    d1, s1 = depth(), serial()
    step("refuse_kv_malformed", ok and d1 == d0 and s1 == s0,
         f"code={e_.get('code')} data={e_.get('data')} depth {d0}->{d1} serial {s0}->{s1}")

    # review-b M5: a mesh inside a prefab/instance is refused before anything opens. The
    # bridge exposes no parentage read, so the row runs only when node.list shows a
    # prefab/instance AND one of its mesh descendants can be located through entity.get
    # (entity.get only walks CMapEntity nodes - a prefab's own children are not reachable).
    nodes = (call("node.list", {}).get("result") or {}).get("nodes") or []
    prefabs = [nd.get("nodeID") for nd in nodes if nd.get("class") in ("prefab", "instance")]
    mesh_in_prefab = None
    if prefabs:
        seen, frontier = set(), [p for p in prefabs if p is not None]
        for _ in range(8):                    # bounded walk down through entity children
            nxt = []
            for nid in frontier:
                if nid in seen:
                    continue
                seen.add(nid)
                for k in ((call("entity.get", {"nodeID": nid}).get("result") or {})
                          .get("children") or []):
                    if k.get("class") == "mesh":
                        mesh_in_prefab = k.get("nodeID")
                        break
                    if k.get("class") in ("prefab", "instance", "group", "entity"):
                        nxt.append(k.get("nodeID"))
                if mesh_in_prefab is not None:
                    break
            if mesh_in_prefab is not None or not nxt:
                break
            frontier = [x for x in nxt if x is not None]
    if mesh_in_prefab is None:
        notrun("refuse_mesh_in_prefab",
               ("no prefab/instance in the scratch map" if not prefabs else
                "a prefab/instance exists but no mesh descendant is reachable through "
                "entity.get (it only walks CMapEntity nodes)"))
    else:
        refuse_row("refuse_mesh_in_prefab", "entity.tieMeshes",
                   {"class": "trigger_once", "nodeIDs": [mesh_in_prefab], "expectDoc": expect},
                   want_code=-32602, want_reason="mesh is inside a prefab/instance")

    refuse_row("refuse_65_node_ids", "entity.tieMeshes",
               {"class": "trigger_once", "nodeIDs": list(range(1, 66)), "expectDoc": expect},
               want_code=-32602)
    refuse_row("refuse_untie_point_entity", "entity.untie", {"nodeID": e_pt, "expectDoc": expect},
               want_code=-32602)
    refuse_row("refuse_untie_mesh", "entity.untie", {"nodeID": md, "expectDoc": expect},
               want_code=-32602)
    refuse_row("refuse_untie_not_found", "entity.untie",
               {"nodeID": 999999001, "expectDoc": expect}, want_code=-32022)
    # not a brush entity names its class, and the two historical aggregate rows kept as
    # belt-and-braces over exactly these last calls - each with a FRESH baseline taken right
    # before (FIX-B L2 moved the real per-refusal depth checks into refuse_row above)
    d0, s0 = depth(), serial()
    r2 = call("entity.tieMeshes", {"entityID": e_pt, "nodeIDs": [md], "expectDoc": expect})
    step("refuse_not_brush_names_class", (err(r2).get("message")) == "InvalidParams",
         f"resp={r2}")
    step("refusals_did_not_open_groups", depth() == d0,
         f"undoDepth={depth()} expected {d0}")
    step("refusals_top_serial_unchanged", serial() == s0,
         f"topSerial={serial()} expected {s0}")

    # the point-class refusal: the group IS a documented no-op (create + defensive gdType
    # delete), and since FIX-B live L3 the error says so in-band
    d0, s0 = depth(), serial()
    r = call("entity.tieMeshes", {"class": "info_target", "nodeIDs": [md], "expectDoc": expect})
    e_ = err(r)
    pdata = e_.get("data") or {}
    step("refuse_point_class", e_.get("code") == -32602
         and pdata.get("reason") == "not a solid (brush) class"
         and pdata.get("undoGroupOpened") is True
         and pdata.get("undoLabel") == "MCP: tie meshes to entity"
         and "maybeExecuted" not in pdata,
         f"resp={r}")
    step("refuse_point_class_group_noop", depth() == d0 + 1,
         f"undoDepth={depth()} expected {d0 + 1} (documented: the group opened, the created "
         f"info_target was deleted inside it)")
    r = call("undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("refuse_point_class_group_popped",
         res.get("undone") is True and res.get("name") == "MCP: tie meshes to entity"
         and depth() == d0, f"resp={r} depth={depth()}")


    # 6. untie the trigger: meshes back under the world, entity gone, one undo restores
    r = call("entity.untie", {"nodeID": e1, "expectDoc": expect})
    res = r.get("result") or {}
    step("untie", "error" not in r and res.get("entityDeleted") is True
         and res.get("undoLabel") == "Untie from entity"
         and sorted(res.get("meshes") or []) == sorted([ma, mb, mc]), f"resp={r}")
    r = call("entity.get", {"nodeID": e1})
    step("untie_entity_gone", err(r).get("code") == -32022, f"resp={r}")
    step("untie_origins_unchanged",
         node_origin_of(ma) == ma_origin and node_origin_of(mb) is not None
         and node_origin_of(mc) is not None,
         f"a={node_origin_of(ma)} (mesh.get is file-only: the live vertex check is the saved-file "
         f"route; the origins here are the world-space pivots)")
    r = call("undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("untie_undo_restores", res.get("undone") is True and res.get("name") == "Untie from entity",
         f"resp={r}")
    r = call("entity.get", {"nodeID": e1})
    kids = [k["nodeID"] for k in ((r.get("result") or {}).get("children") or [])]
    step("untie_undo_children_back", sorted(kids) == sorted([ma, mb, mc]), f"children={kids!r}")

    # 7. cleanup: pop this run's groups (stop at anything else) and save again
    cleanup = {"popped": []}
    for _ in range(32):
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
    if NOTRUN:
        print(f"NOT RUN ({len(NOTRUN)} rows, never silent passes): {NOTRUN}")
    if not all(k for k in (cleanup.get("depth_restored"), cleanup.get("saved"))):
        print("CLEANUP INCOMPLETE (not part of the pass criteria)")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# ---- offline (--static) ------------------------------------------------------------------------

def static_report() -> int:
    """the Rust bridge contract this pass relies on (SOURCE SHAPE ONLY): the two registry rows,
    the pins they call and the handler tables' consistency. A missing file is a FAIL."""
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

    want = ("entity.tieMeshes", "entity.untie")
    for name in want:
        missing += [f"{name}: {why}" for why in row_problems(rows, name, "NEED_HAMMER")]

    # a served name must be an implemented, advertised, unblocked row (the invariant the ops
    # tables rely on). The two trigger bodies are still other lanes' work: while a name has no
    # HANDLERS entry this check cannot cover it, so say so instead of passing quietly.
    orphan = sorted(name for name in served if row_problems(rows, name))
    if orphan:
        missing.append(f"HANDLERS names without an implemented row: {orphan}")
    unported = [name for name in want if name not in served]
    if unported:
        print(f"NOTE: no Rust body yet for {unported} (their rows are checked above; the "
              f"handler check starts covering them once their lane lands)")

    for pin in ("HFN_DOC_MOVE_TO_WORLD", "HFN_RECORD_NODES_AND_PARENTS"):
        if pin not in pins:
            missing.append(f"{pin} missing from crates/cs2-bridge/src/generated/pins.rs")

    step("static_triggers_contract", not missing, f"missing={missing}")
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true",
                    help="offline source expectations (the Rust build string, rows, pins)")
    ap.add_argument("--prefix", default=PFX_DIR)
    ap.add_argument("--base-ref", default=None,
                    help="ignored: the C-source diff check is retired "
                         "(see bridge/protocol-diff-m5-tests.md)")
    ap.add_argument("--wait", type=float, default=0.0,
                    help="seconds to wait for the discovery file (0 = it must exist)")
    args = ap.parse_args()
    if args.static:
        return static_report()
    return live_report(args)


if __name__ == "__main__":
    sys.exit(main())
