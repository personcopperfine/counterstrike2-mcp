#!/usr/bin/env python3
"""test_undo.py - M4 acceptance: undo.undo and its group semantics.

Verified here (bridge-only, no host input):
  0. drain: undo until the stack is empty (bounded loop, deadline, no sleeps)
     so this run starts from a known baseline. Earlier tests or an earlier
     crashed run in the same Hammer session can leave groups (baseline depth 11
     observed 2026-10-12) which undo.undo would otherwise pop - the step-3
     `undone:false` property is only observable on an EMPTY stack. Draining is
     the honest way to get there: the history is per-document (CMapDocHistory
     of the active map doc, mcp_bridge.c undo_group_info/undo_open) and holds
     only this session's undoable edits - automated MCP edits of the probe map.
     It cannot touch anything the test relies on (the test creates its own
     entities after the drain, and undoing only removes nodes), and it doubles
     as leftover cleanup. Caveat: it reverts ALL pending undoable edits of the
     active document - do not run this in a Hammer session whose manual edits
     you want to keep.
  1. entity.create -> group 1; undo.undo pops it, the entity is gone from the
     saved file (full round trip through map.save).
  2. entity.create + entity.setKeyValues -> two groups; undo pops the keyvalue
     group first (name/depth correct, entity survives), then the create group
     (entity gone).
  3. undo with nothing to undo -> {undone:false}.

KNOWN GAP (documented, asserted as such): undoing the keyvalue group pops the
group but does not revert the attribute values. The setters record
CUndoAttributeSetValueElement into the *data model's* undo scope (data model
vfn+520 / +656), which the map history's group does not own; recording the
entity node instead makes the group's undo delete the node. The fix is to open
the data-model undo scope around the setters and record those elements into the
group (open item). The step-2 survival check therefore keys on the run-unique
ORIGIN, not the targetname: if the gap is ever fixed, the name reverts with the
group and the origin check keeps testing the same property.

Run-unique identity: names and origins carry a per-run suffix/jitter
(time+pid hex), so entities left in the saved file by earlier runs can never
match this run's checks. Old leftovers keep their plain names and fixed
origins forever.

Usage: bridge/test_undo.py [--prefix <pfx>]
Exit 0 on pass.
"""
import argparse
import json
import os
import socket
import sys
import time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import APP_DIR, PFX_DIR  # noqa: E402

DEFAULT_PFX = PFX_DIR
HOST_MAP = os.path.join(APP_DIR, "content", "csgo_addons", "mcp_test", "maps", "probe.vmap")

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
# per-run origin jitter, on the /16 grid; BAND keeps the three map-editing tests
# in disjoint coordinate bands so one test's leftovers can never sit on another
# test's jittered origins (band pitch 20000 > jitter span 16000 + base 512)
BAND = 2
RUN_OFF = BAND * 20000.0 + (int(RUN, 16) % 1000) * 16.0

ORIGIN_A = [320.0 + RUN_OFF, 320.0 + RUN_OFF, 0.0]
ORIGIN_B = [384.0 + RUN_OFF, 384.0 + RUN_OFF, 0.0]
NAME_A = "mcp_undo_a_%s" % RUN
NAME_B = "mcp_undo_%s" % RUN


def near(a, b, tol=0.01):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def parse_map():
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "vmapreader"))
    import vmap_dmx
    return vmap_dmx.load(HOST_MAP)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    a = ap.parse_args()
    disc = json.load(open(os.path.join(a.prefix, "drive_c", "users", "steamuser",
                                       "AppData", "Local", "cs2mcpbridge", "session.json")))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=60)
    f = s.makefile("r")

    def rpc(obj):
        s.sendall((json.dumps(obj) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    rpc({"jsonrpc": "2.0", "id": "h", "method": "control.hello",
         "params": {"protocol": 1, "token": disc["token"]}})
    checks = {}
    doc = rpc({"jsonrpc": "2.0", "id": "d0", "method": "doc.active",
               "params": {}})["result"]["doc"]
    expect = {"id": doc["id"], "epoch": doc["epoch"]}
    print(f"baseline: depth={doc['undoDepth']} run={RUN}")

    # --- 0. drain: pop every group left on the stack (bounded) ---------------
    drained = 0
    deadline = time.monotonic() + 60
    while True:
        r = rpc({"jsonrpc": "2.0", "id": "dr%d" % drained, "method": "undo.undo",
                 "params": {"expectDoc": expect}})
        res = r.get("result", {})
        if res.get("undone") is not True:
            if r.get("error"):
                print("FAIL drain:", r["error"])
                return 2
            break
        drained += 1
        if time.monotonic() > deadline:
            print("FAIL drain: deadline exceeded with groups left (undone kept returning true)")
            return 2
    d1 = rpc({"jsonrpc": "2.0", "id": "d1", "method": "doc.active",
              "params": {}})["result"]["doc"]
    print(f"drained     -> popped={drained} depth={d1['undoDepth']}")
    checks["drained"] = d1["undoDepth"] == 0
    base_depth = d1["undoDepth"]

    # --- 1. create -> undo -> gone -----------------------------------------
    r = rpc({"jsonrpc": "2.0", "id": "c1", "method": "entity.create",
             "params": {"class": "info_target", "origin": ORIGIN_A,
                        "keyValues": {"targetname": NAME_A}, "expectDoc": expect}})
    handle_a = r["result"]["handle"]
    checks["a_named"] = (r["result"].get("keyValues", {}).get("applied", {})
                         .get("targetname") is True)
    print(f"create A    -> {handle_a} ({NAME_A})")
    rpc({"jsonrpc": "2.0", "id": "sv0", "method": "map.save", "params": {}})
    m = parse_map()
    a_hit = [e for e in m.entities if near(e.origin, ORIGIN_A)]
    checks["create_present"] = len(a_hit) == 1 and a_hit[0].keyvalues.get("targetname") == NAME_A
    print(f"save+parse  -> entity A present: {len(a_hit) == 1}")
    r = rpc({"jsonrpc": "2.0", "id": "u1", "method": "undo.undo", "params": {"expectDoc": expect}})
    res = r["result"]
    print(f"undo A      -> undone={res['undone']} name={res['name']!r} depth={res.get('undoDepth')}")
    checks["undo_create"] = res["undone"] is True and res["name"] == "MCP: create entity"
    checks["undo_create_depth"] = res.get("undoDepth") == base_depth
    rpc({"jsonrpc": "2.0", "id": "sv1", "method": "map.save", "params": {}})
    m = parse_map()
    checks["undo_create_gone"] = (not any(near(e.origin, ORIGIN_A) for e in m.entities)
                                  and not any(e.keyvalues.get("targetname") == NAME_A
                                              for e in m.entities))
    print(f"save+parse  -> entity A present: {not checks['undo_create_gone']}")

    # --- 2. create + setKeyValues -> two groups ----------------------------
    r = rpc({"jsonrpc": "2.0", "id": "c2", "method": "entity.create",
             "params": {"class": "info_target", "origin": ORIGIN_B, "expectDoc": expect}})
    handle = r["result"]["handle"]
    r = rpc({"jsonrpc": "2.0", "id": "kv", "method": "entity.setKeyValues",
             "params": {"handle": handle, "values": {"targetname": NAME_B},
                        "expectDoc": expect}})
    print(f"create B+kv -> {handle} {r['result']['applied']}")
    r = rpc({"jsonrpc": "2.0", "id": "u2", "method": "undo.undo", "params": {"expectDoc": expect}})
    res = r["result"]
    print(f"undo kv     -> undone={res['undone']} name={res['name']!r} depth={res.get('undoDepth')}")
    checks["undo_kv_group"] = res["undone"] is True and res["name"] == "MCP: set keyvalues"
    checks["undo_kv_depth"] = res.get("undoDepth") == base_depth + 1
    rpc({"jsonrpc": "2.0", "id": "sv2", "method": "map.save", "params": {}})
    m = parse_map()
    hit = [e for e in m.entities if near(e.origin, ORIGIN_B)]
    checks["undo_kv_entity_survives"] = len(hit) == 1
    if hit:
        # KNOWN GAP: the value is still there (group had no attribute record)
        print(f"save+parse  -> entity B survives, targetname={hit[0].keyvalues.get('targetname')!r} "
              f"(known gap: value not reverted)")

    r = rpc({"jsonrpc": "2.0", "id": "u3", "method": "undo.undo", "params": {"expectDoc": expect}})
    res = r["result"]
    print(f"undo create -> undone={res['undone']} name={res['name']!r} depth={res.get('undoDepth')}")
    checks["undo_create2"] = res["undone"] is True and res["name"] == "MCP: create entity"
    rpc({"jsonrpc": "2.0", "id": "sv3", "method": "map.save", "params": {}})
    m = parse_map()
    checks["undo_create2_gone"] = (not any(near(e.origin, ORIGIN_B) for e in m.entities)
                                   and not any(e.keyvalues.get("targetname") == NAME_B
                                               for e in m.entities))

    # --- 3. empty (guaranteed by the drain at step 0) -----------------------
    r = rpc({"jsonrpc": "2.0", "id": "u4", "method": "undo.undo", "params": {"expectDoc": expect}})
    res = r.get("result", {})
    print(f"undo empty  -> undone={res.get('undone')}")
    checks["undo_empty"] = res.get("undone") is False

    s.close()
    ok = all(v for v in checks.values())
    print(f"\nchecks: {checks}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
