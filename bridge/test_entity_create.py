#!/usr/bin/env python3
"""test_entity_create.py - M3a acceptance: entity.create + map.save + file check.

Sequence (against a live bridge with probe.vmap open):
  1. doc.active                  -> id/epoch/undoDepth baseline
  2. entity.create info_target   -> handle, class, origin
  3. map.save                    -> saved=true, skippedClean=false (real write)
  4. doc.active                  -> dirty=false after save, undoDepth=1,
                                    undoName="MCP: create entity", epoch unchanged
  5. parse the saved .vmap with worknotes/vmapreader and verify the new
     info_target exists with classname (and report its keyvalues)

Usage: bridge/test_entity_create.py [--prefix <pfx>] [--count N]
Exit 0 on pass.
"""
import argparse
import json
import os
import socket
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import APP_DIR, PFX_DIR  # noqa: E402

DEFAULT_PFX = PFX_DIR
HOST_MAP = os.path.join(APP_DIR, "content", "csgo_addons", "mcp_test", "maps", "probe.vmap")


def discovery_path(pfx):
    return os.path.join(pfx, "drive_c", "users", "steamuser", "AppData", "Local",
                        "cs2mcpbridge", "session.json")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--count", type=int, default=1)
    a = ap.parse_args()

    disc = json.load(open(discovery_path(a.prefix)))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=60)
    f = s.makefile("r")

    def rpc(obj):
        s.sendall((json.dumps(obj) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    def hello():
        return rpc({"jsonrpc": "2.0", "id": "h", "method": "control.hello",
                    "params": {"protocol": 1, "token": disc["token"]}})

    hello()
    checks = {}

    d0 = rpc({"jsonrpc": "2.0", "id": "d0", "method": "doc.active", "params": {}})
    doc = d0.get("result", {}).get("doc")
    if not doc:
        print("FAIL: no document open:", d0)
        return 2
    print(f"baseline: id={doc['id']} epoch={doc['epoch']} dirty={doc['dirty']} "
          f"editable={doc['editable']} undoDepth={doc['undoDepth']}")
    checks["baseline_editable"] = doc["editable"] is True
    baseline_depth = doc["undoDepth"]
    expect = {"id": doc["id"], "epoch": doc["epoch"]}

    created = []
    for i in range(a.count):
        r = rpc({"jsonrpc": "2.0", "id": f"c{i}", "method": "entity.create",
                 "params": {"class": "info_target", "origin": [64.0 + 16 * i, 64.0, 0.0],
                            "angles": [0, 0, 0], "expectDoc": expect}})
        if r.get("error"):
            print(f"FAIL: entity.create -> {r['error']}")
            return 2
        res = r["result"]
        created.append(res)
        print(f"entity.create -> handle={res['handle']} class={res['class']} origin={res['origin']}")

    r = rpc({"jsonrpc": "2.0", "id": "sv", "method": "map.save", "params": {}})
    if r.get("error"):
        print(f"FAIL: map.save -> {r['error']}")
        return 2
    sres = r["result"]
    print(f"map.save     -> saved={sres['saved']} skippedClean={sres['skippedClean']} path={sres['path']}")
    checks["save_dirty"] = sres["saved"] is True and sres["skippedClean"] is False

    d1 = rpc({"jsonrpc": "2.0", "id": "d1", "method": "doc.active", "params": {}})
    doc1 = d1.get("result", {}).get("doc")
    print(f"after save: id={doc1['id']} epoch={doc1['epoch']} dirty={doc1['dirty']} "
          f"undoDepth={doc1['undoDepth']} undoName={doc1['undoName']!r} topSerial={doc1['topSerial']}")
    checks["epoch_stable"] = doc1["id"] == doc["id"] and doc1["epoch"] == doc["epoch"]
    checks["dirty_after_save"] = doc1["dirty"] is False
    checks["undo_depth"] = doc1["undoDepth"] == baseline_depth + a.count
    checks["undo_name"] = doc1["undoName"] == "MCP: create entity"
    checks["top_serial"] = isinstance(doc1["topSerial"], int) and doc1["topSerial"] > 0

    s.close()

    # file check with the native reader
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "vmapreader"))
    try:
        import vmap_dmx
        m = vmap_dmx.load(HOST_MAP)
        ents = m.entities
        targets = [e for e in ents if e.classname == "info_target"]
        print(f"file: {len(ents)} entities, {len(targets)} info_target")
        checks["file_entities"] = len(targets) >= a.count
        if targets:
            t = targets[-1]
            print(f"      sample: nodeID={t.id} origin={t.origin} keyvalues={t.keyvalues}")
            checks["file_classname_kv"] = t.keyvalues.get("classname") == "info_target"
    except Exception as e:
        print(f"FAIL: vmap reader: {e}")
        checks["file_entities"] = False

    ok = all(v for v in checks.values())
    print(f"\nchecks: {checks}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
