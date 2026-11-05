#!/usr/bin/env python3
"""test_edit_batch.py - edit.batch: one undo group, $N handle refs, partial failure.

  1. edit.batch [create A, setKeyValues($0), create B] -> 3 results, failedIndex null
  2. doc.active -> undoDepth +1 (ONE group for the whole batch)
  3. map.save + parse -> both entities present, A has the run's targetname
  4. edit.batch [create, create with unknown class] -> -32012 PartialFailure,
     failedIndex 1, results has op0's result
  5. undo.undo -> the whole partial batch reverts as one group (both entities gone)
  6. cleanup: undo the batch group and save again, so probe.vmap stops growing

Run-unique identity: every name the test creates (A/B/C targetnames) and every
origin carries a per-run suffix/jitter (time+pid hex), so entities left in the
file by earlier runs (4 stale hits per name/origin observed 2026-10-12) can
never match this run's checks. The file checks identify this run's entities by
name and then assert their origins on the same hit - the `== 1` assertions
stay. Old leftovers keep their plain names and fixed origins forever.

Cleanup is best-effort and only pops a group named "MCP: batch" (it stops at
anything else); it is reported but not part of the pass criteria.

Usage: bridge/test_edit_batch.py [--prefix <pfx>]
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
BAND = 1
RUN_OFF = BAND * 20000.0 + (int(RUN, 16) % 1000) * 16.0

A_ORIGIN = [448.0 + RUN_OFF, 448.0 + RUN_OFF, 0.0]
B_ORIGIN = [480.0 + RUN_OFF, 480.0 + RUN_OFF, 0.0]
C_ORIGIN = [512.0 + RUN_OFF, 512.0 + RUN_OFF, 0.0]
NAME_A = "mcp_batch_%s" % RUN
NAME_B = "mcp_batch_b_%s" % RUN
NAME_C = "mcp_batch_c_%s" % RUN


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
    base_depth = doc["undoDepth"]
    expect = {"id": doc["id"], "epoch": doc["epoch"]}
    print(f"baseline: depth={base_depth} run={RUN}")

    r = rpc({"jsonrpc": "2.0", "id": "b1", "method": "edit.batch",
             "params": {"expectDoc": expect, "undoLabel": "MCP: batch test", "ops": [
                 {"method": "entity.create",
                  "params": {"class": "info_target", "origin": A_ORIGIN}},
                 {"method": "entity.setKeyValues",
                  "params": {"handle": "$0", "values": {"targetname": NAME_A}}},
                 {"method": "entity.create",
                  "params": {"class": "info_target", "origin": B_ORIGIN,
                             "keyValues": {"targetname": NAME_B}}},
             ]}})
    if r.get("error"):
        print("FAIL batch:", r["error"])
        return 2
    res = r["result"]
    print(f"batch       -> failedIndex={res['failedIndex']} results={len(res['results'])}")
    checks["batch_ok"] = res["failedIndex"] is None and len(res["results"]) == 3
    checks["handle_ref"] = res["results"][1]["applied"].get("targetname") is True
    checks["b_named"] = (res["results"][2].get("keyValues", {}).get("applied", {})
                         .get("targetname") is True)

    d1 = rpc({"jsonrpc": "2.0", "id": "d1", "method": "doc.active",
              "params": {}})["result"]["doc"]
    print(f"after batch -> depth={d1['undoDepth']} name={d1['undoName']!r}")
    checks["one_group"] = d1["undoDepth"] == base_depth + 1 and d1["undoName"] == "MCP: batch"

    rpc({"jsonrpc": "2.0", "id": "sv1", "method": "map.save", "params": {}})
    m = parse_map()
    a_hit = [e for e in m.entities if e.keyvalues.get("targetname") == NAME_A]
    b_hit = [e for e in m.entities if e.keyvalues.get("targetname") == NAME_B]
    print(f"file        -> A={len(a_hit)} B={len(b_hit)} (names {NAME_A!r}/{NAME_B!r})")
    checks["file_a"] = len(a_hit) == 1
    checks["file_b"] = len(b_hit) == 1
    if a_hit:
        checks["file_a_origin"] = near(a_hit[0].origin, A_ORIGIN)
        checks["file_a_class"] = a_hit[0].keyvalues.get("classname") == "info_target"
    if b_hit:
        checks["file_b_origin"] = near(b_hit[0].origin, B_ORIGIN)

    # partial failure
    r = rpc({"jsonrpc": "2.0", "id": "b2", "method": "edit.batch",
             "params": {"expectDoc": expect, "ops": [
                 {"method": "entity.create",
                  "params": {"class": "info_target", "origin": C_ORIGIN,
                             "keyValues": {"targetname": NAME_C}}},
                 {"method": "entity.create", "params": {"class": "not_a_class_at_all", "origin": [0, 0, 0]}},
             ]}})
    err = r.get("error", {})
    print(f"partial     -> code={err.get('code')} failedIndex={err.get('data', {}).get('failedIndex')} "
          f"results={len(err.get('data', {}).get('results', []))}")
    checks["partial_code"] = err.get("code") == -32012
    checks["partial_index"] = err.get("data", {}).get("failedIndex") == 1
    checks["partial_results"] = len(err.get("data", {}).get("results", [])) == 2

    r = rpc({"jsonrpc": "2.0", "id": "u1", "method": "undo.undo", "params": {"expectDoc": expect}})
    print(f"undo        -> undone={r['result']['undone']} name={r['result']['name']!r} "
          f"depth={r['result'].get('undoDepth')}")
    checks["undo_partial"] = r["result"]["undone"] is True
    checks["undo_partial_name"] = r["result"]["name"] == "MCP: batch"
    rpc({"jsonrpc": "2.0", "id": "sv2", "method": "map.save", "params": {}})
    m = parse_map()
    checks["partial_reverted"] = not any(e.keyvalues.get("targetname") == NAME_C for e in m.entities)

    # --- cleanup: pop exactly the batch group, then save again ---------------
    cleanup = {}
    r = rpc({"jsonrpc": "2.0", "id": "cu1", "method": "undo.undo", "params": {"expectDoc": expect}})
    res = r.get("result", {})
    print(f"cleanup undo -> undone={res.get('undone')} name={res.get('name')!r}")
    if res.get("undone") is not True or res.get("name") != "MCP: batch":
        cleanup["abort"] = "top group is %r, expected 'MCP: batch' - stopping" % res.get("name")
    else:
        r = rpc({"jsonrpc": "2.0", "id": "sv9", "method": "map.save", "params": {}})
        print(f"cleanup save -> saved={r.get('result', {}).get('saved')} "
              f"skippedClean={r.get('result', {}).get('skippedClean')}")
        if r.get("result", {}).get("saved") or r.get("result", {}).get("skippedClean"):
            m = parse_map()
            leftover = [e.keyvalues.get("targetname") for e in m.entities
                        if e.keyvalues.get("targetname") in (NAME_A, NAME_B)]
            cleanup["own_entities_gone"] = not leftover
            if leftover:
                cleanup["leftover"] = leftover

    s.close()
    ok = all(v for v in checks.values())
    print(f"\nchecks: {checks}")
    if not all(cleanup.get(k) for k in cleanup if isinstance(cleanup[k], bool)):
        print("CLEANUP INCOMPLETE (not part of the pass criteria)")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
