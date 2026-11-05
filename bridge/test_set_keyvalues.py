#!/usr/bin/env python3
"""test_set_keyvalues.py - M3b acceptance: entity.setKeyValues + save + file check.

  1. entity.create info_target (distinct origin) -> handle
  2. entity.setKeyValues on the handle: targetname / spawnflags / vscripts
  3. map.save (real write)
  4. parse the saved .vmap: the keyvalues must be on the entity's
     entity_properties child with classname intact
  5. cleanup: undo exactly the two groups this test created (setKeyValues,
     create) and save again, so probe.vmap stops growing run over run

Run-unique identity: the targetname carries a per-run suffix (time+pid hex), so
entities left in the file by earlier runs (they persist across runs; 4 stale
`mcp_kv_1` hits observed 2026-10-12) can never match this run's checks. Old
leftovers keep their plain names forever - the run-unique assertion `== 1`
stays, it just can no longer be satisfied by a leftover.

Cleanup is best-effort and only pops groups whose name matches what this test
opened (it stops at anything else); it is reported but not part of the pass
criteria. A mid-test abort can leave this run's entity behind (harmless: the
name is run-unique).

Usage: bridge/test_set_keyvalues.py [--prefix <pfx>]
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
BAND = 0
RUN_OFF = BAND * 20000.0 + (int(RUN, 16) % 1000) * 16.0

VALUES = {"targetname": "mcp_kv_%s" % RUN, "spawnflags": "4", "vscripts": "mcp_test.nut"}


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
    d0 = rpc({"jsonrpc": "2.0", "id": "d0", "method": "doc.active", "params": {}})
    doc = d0["result"]["doc"]
    base_depth = doc["undoDepth"]
    expect = {"id": doc["id"], "epoch": doc["epoch"]}
    print(f"baseline: id={doc['id']} epoch={doc['epoch']} editable={doc['editable']} "
          f"undoDepth={base_depth} run={RUN}")

    origin = [128.0 + RUN_OFF, 128.0 + RUN_OFF, 0.0]
    r = rpc({"jsonrpc": "2.0", "id": "c", "method": "entity.create",
             "params": {"class": "info_target", "origin": origin,
                        "angles": [0, 0, 0], "expectDoc": expect}})
    if r.get("error"):
        print("FAIL create:", r["error"])
        return 2
    handle = r["result"]["handle"]
    print(f"entity.create -> handle={handle}")

    r = rpc({"jsonrpc": "2.0", "id": "kv", "method": "entity.setKeyValues",
             "params": {"handle": handle, "values": VALUES, "expectDoc": expect}})
    if r.get("error"):
        print("FAIL setKeyValues:", r["error"])
        return 2
    applied = r["result"]["applied"]
    print(f"setKeyValues -> applied={applied}")
    checks["applied"] = all(applied.get(k) is True for k in VALUES)

    # stale-handle check: a handle from another epoch must be rejected
    r = rpc({"jsonrpc": "2.0", "id": "kv2", "method": "entity.setKeyValues",
             "params": {"handle": handle, "values": VALUES,
                        "expectDoc": {"id": doc["id"], "epoch": doc["epoch"] + 99}}})
    checks["stale_handle"] = r.get("error", {}).get("code") in (-32004, -32005)
    print(f"stale handle  -> {r.get('error', {}).get('code')} (expected -32004/-32005)")

    r = rpc({"jsonrpc": "2.0", "id": "sv", "method": "map.save", "params": {}})
    if r.get("error"):
        print("FAIL save:", r["error"])
        return 2
    print(f"map.save     -> saved={r['result']['saved']} skippedClean={r['result']['skippedClean']}")
    checks["saved"] = r["result"]["saved"] is True and r["result"]["skippedClean"] is False

    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "vmapreader"))
    import vmap_dmx
    m = vmap_dmx.load(HOST_MAP)
    hits = [e for e in m.entities if e.keyvalues.get("targetname") == VALUES["targetname"]]
    print(f"file: {len(m.entities)} entities, {len(hits)} with targetname {VALUES['targetname']}")
    checks["file_found"] = len(hits) == 1
    if hits:
        kv = hits[0].keyvalues
        print(f"      keyvalues={ {k: kv.get(k) for k in ('classname',) + tuple(VALUES)} }")
        checks["file_values"] = all(kv.get(k) == v for k, v in VALUES.items())
        checks["classname_intact"] = kv.get("classname") == "info_target"
        checks["origin"] = tuple(hits[0].origin) == tuple(origin)

    # --- cleanup: pop exactly our two groups (kv, create), then save again ---
    cleanup = {}
    for step, want in ((1, "MCP: set keyvalues"), (2, "MCP: create entity")):
        r = rpc({"jsonrpc": "2.0", "id": "cu%d" % step, "method": "undo.undo",
                 "params": {"expectDoc": expect}})
        res = r.get("result", {})
        print(f"cleanup undo{step} -> undone={res.get('undone')} name={res.get('name')!r}")
        if res.get("undone") is not True or res.get("name") != want:
            cleanup["abort"] = "top group is %r, expected %r - stopping" % (res.get("name"), want)
            break
    else:
        d_end = rpc({"jsonrpc": "2.0", "id": "d9", "method": "doc.active", "params": {}})
        cleanup["depth_restored"] = d_end["result"]["doc"]["undoDepth"] == base_depth
        r = rpc({"jsonrpc": "2.0", "id": "sv9", "method": "map.save", "params": {}})
        print(f"cleanup save  -> saved={r.get('result', {}).get('saved')} "
              f"skippedClean={r.get('result', {}).get('skippedClean')}")
    print(f"cleanup: {cleanup}")

    s.close()

    ok = all(v for v in checks.values())
    print(f"\nchecks: {checks}")
    if not all(cleanup.get(k) for k in cleanup if isinstance(cleanup[k], bool)):
        print("CLEANUP INCOMPLETE (not part of the pass criteria)")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
