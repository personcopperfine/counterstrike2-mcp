#!/usr/bin/env python3
"""test_reload.py - M5a: map.reload (prompt-free) + the unsaved-changes guard.

  1. entity.create -> dirty
  2. map.reload without force -> -32016 UnsavedChanges (nothing discarded)
  3. map.reload force:true    -> reloaded, discardedChanges=true, epoch bumped,
                                 undoDepth reset, the created entity is gone
  4. map.reload on a clean doc -> reloaded, discardedChanges=false
  5. map.reload with the *previous* fingerprint (a stale expectDoc) -> -32004
     DocumentChanged, nothing reloaded (m2.16 FIX-3B L2: the reload carries the
     doc.active fingerprint like every other mutator)

Every map.reload carries `expectDoc` = the fingerprint just read from doc.active.
Usage: bridge/test_reload.py [--prefix <pfx>]
Exit 0 on pass.
"""
import argparse
import json
import os
import socket
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import PFX_DIR  # noqa: E402

DEFAULT_PFX = PFX_DIR
ORIGIN = [704.0, 704.0, 0.0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    a = ap.parse_args()
    disc = json.load(open(os.path.join(a.prefix, "drive_c", "users", "steamuser",
                                       "AppData", "Local", "cs2mcpbridge", "session.json")))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
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
    print(f"baseline: epoch={doc['epoch']} dirty={doc['dirty']} depth={doc['undoDepth']}")

    r = rpc({"jsonrpc": "2.0", "id": "c", "method": "entity.create",
             "params": {"class": "info_target", "origin": ORIGIN, "expectDoc": expect}})
    print(f"create      -> {r['result']['handle']}")
    doc = rpc({"jsonrpc": "2.0", "id": "d1", "method": "doc.active", "params": {}})["result"]["doc"]
    print(f"after create-> dirty={doc['dirty']} depth={doc['undoDepth']}")
    checks["dirty"] = doc["dirty"] is True
    # the fingerprint each reload below sends: doc.active *just* read (m2.16 FIX-3B L2)
    expect = {"id": doc["id"], "epoch": doc["epoch"]}
    stale = {"id": doc["id"], "epoch": doc["epoch"] - 1}     # a fingerprint from before

    r = rpc({"jsonrpc": "2.0", "id": "r1", "method": "map.reload",
             "params": {"force": False, "expectDoc": expect}})
    print(f"reload      -> {r.get('error', {}).get('code')} (expected -32016)")
    checks["guard"] = r.get("error", {}).get("code") == -32016

    # a stale expectDoc is refused (L2): the document moved between doc.active and the reload
    r = rpc({"jsonrpc": "2.0", "id": "r1b", "method": "map.reload",
             "params": {"force": True, "expectDoc": stale}})
    err = r.get("error", {})
    print(f"stale fp    -> {err.get('code')} {err.get('data')} (expected -32004 DocumentChanged)")
    checks["stale_expect_doc"] = (err.get("code") == -32004
                                 and err.get("message") == "DocumentChanged"
                                 and err.get("data", {}).get("reason") in ("doc_id", "epoch"))
    doc = rpc({"jsonrpc": "2.0", "id": "d1b", "method": "doc.active", "params": {}})["result"]["doc"]
    checks["stale_changed_nothing"] = doc["dirty"] is True      # the refusal reloaded nothing
    print(f"after stale -> dirty={doc['dirty']} depth={doc['undoDepth']}")

    r = rpc({"jsonrpc": "2.0", "id": "r2", "method": "map.reload",
             "params": {"force": True, "expectDoc": expect}})
    if r.get("error"):
        print("FAIL reload force:", r["error"])
        return 2
    res = r["result"]
    print(f"reload force-> reloaded={res['reloaded']} discarded={res['discardedChanges']} "
          f"epoch={res['epoch']} (was {expect['epoch']})")
    checks["forced"] = res["reloaded"] is True and res["discardedChanges"] is True
    checks["epoch_bumped"] = res["epoch"] > expect["epoch"]

    doc = rpc({"jsonrpc": "2.0", "id": "d2", "method": "doc.active", "params": {}})["result"]["doc"]
    print(f"after reload-> dirty={doc['dirty']} depth={doc['undoDepth']} epoch={doc['epoch']} "
          f"editable={doc['editable']}")
    checks["clean_after"] = doc["dirty"] is False and doc["undoDepth"] == 0

    r = rpc({"jsonrpc": "2.0", "id": "r3", "method": "map.reload",
             "params": {"expectDoc": {"id": doc["id"], "epoch": doc["epoch"]}}})
    res = r.get("result", {})
    print(f"reload clean-> reloaded={res.get('reloaded')} discarded={res.get('discardedChanges')}")
    checks["clean_reload"] = res.get("reloaded") is True and res.get("discardedChanges") is False

    s.close()
    ok = all(v for v in checks.values())
    print(f"\nchecks: {checks}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
