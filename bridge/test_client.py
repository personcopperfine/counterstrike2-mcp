#!/usr/bin/env python3
"""test_client.py - bridge contract test client (M2.2).

Reads the discovery file from the scratch tree's Wine prefix, connects to the
bridge's loopback TCP endpoint (the container runs with --network=host, so
127.0.0.1 is shared with the host), and exercises the M1+M2.1+M2.2 contract:

  unauth first frame   -> -32001 NotAuthenticated + close
  bad token            -> -32001
  protocol mismatch    -> -32002 VersionMismatch
  id-less request      -> -32600
  hello + ping         -> capabilities (method table, modules, revision),
                          ping.modules/revision present
  second client        -> -32007 Busy
  unknown method       -> -32601
  blocked method       -> -32011 NotImplemented + data.blockedOn
  deadlineMs < 0       -> -32602
  doc.active           -> {"doc":null,"reason":...} or a doc object (real path
                          once a map is open)

Usage: bridge/test_client.py [--prefix <wine pfx path>] [--wait N]
Exit 0 when every check passes.
"""
import argparse
import json
import os
import socket
import sys
import time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import PFX_DIR  # noqa: E402

DEFAULT_PFX = PFX_DIR


def discovery_path(pfx: str) -> str:
    return os.path.join(
        pfx, "drive_c", "users", "steamuser", "AppData", "Local",
        "cs2mcpbridge", "session.json")


_READERS = {}


def rpc(sock: socket.socket, obj: dict) -> dict:
    sock.sendall((json.dumps(obj) + "\n").encode())
    f = _READERS.setdefault(sock, sock.makefile("rb"))   # one buffered reader per connection
    line = f.readline()
    if not line:
        raise RuntimeError("connection closed with no response")
    try:
        return json.loads(line)
    except json.JSONDecodeError as e:
        raise RuntimeError("malformed JSON from bridge: %r (%s)" % (line[:200], e))


def connect(disc: dict) -> socket.socket:
    return socket.create_connection((disc["host"], disc["port"]), timeout=8)


def hello(sock: socket.socket, token: str, proto: int = 1, rid: str = "h1") -> dict:
    return rpc(sock, {"jsonrpc": "2.0", "id": rid, "method": "control.hello",
                      "params": {"protocol": proto, "token": token}})


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--wait", type=float, default=0.0)
    args = ap.parse_args()

    path = discovery_path(args.prefix)
    deadline = time.time() + args.wait
    while not os.path.exists(path):
        if time.time() >= deadline:
            print(f"FAIL: discovery file not found: {path}")
            return 2
        time.sleep(0.5)

    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']}")
    host, port, token = disc["host"], disc["port"], disc["token"]
    checks = {}

    # 1. unauthenticated first frame
    s = connect(disc)
    r = rpc(s, {"jsonrpc": "2.0", "id": "bad-1", "method": "control.ping"})
    checks["unauth"] = r.get("error", {}).get("code") == -32001
    print(f"unauth ping        -> {r.get('error', {}).get('code')}")
    s.close()

    # 2. bad token
    s = connect(disc)
    r = hello(s, "00" * 32, rid="bad-2")
    checks["badtoken"] = r.get("error", {}).get("code") == -32001
    print(f"bad token          -> {r.get('error', {}).get('code')}")
    s.close()

    # 3. protocol mismatch
    s = connect(disc)
    r = hello(s, token, proto=2, rid="bad-3")
    checks["proto"] = r.get("error", {}).get("code") == -32002
    print(f"protocol mismatch  -> {r.get('error', {}).get('code')}")
    s.close()

    # 4. authenticated round trip: capabilities shape (method table + modules)
    s = connect(disc)
    r = hello(s, token)
    res = r.get("result", {})
    caps = res.get("capabilities", {})
    methods = caps.get("methods", [])
    mods = caps.get("modules", {})
    checks["hello"] = r.get("result", {}).get("protocol") == 1
    checks["caps_hello_hidden"] = "control.hello" not in methods
    checks["caps_modules"] = ("hammer.dll" in mods and "toolframework2.dll" in mods
                             and mods.get("toolframework2.dll", {}).get("state") in
                             ("not_loaded", "verifying", "verified", "mismatch"))
    checks["caps_revision"] = isinstance(caps.get("revision"), int) and caps["revision"] >= 1
    print(f"hello              -> protocol={res.get('protocol')} methods={methods}")
    print(f"                      modules={ {k: v.get('state') for k, v in mods.items()} } "
          f"revision={caps.get('revision')}")

    # 5. id-less request rejected
    s.sendall((json.dumps({"jsonrpc": "2.0", "method": "control.ping", "params": {}}) + "\n").encode())
    r = json.loads(s.makefile("rb").readline())
    checks["noid"] = r.get("error", {}).get("code") == -32600
    print(f"id-less request    -> {r.get('error', {}).get('code')}")

    # 6. ping shape (modules + revision + inflight)
    r = rpc(s, {"jsonrpc": "2.0", "id": "p1", "method": "control.ping", "params": {}})
    pres = r.get("result", {})
    checks["ping"] = pres.get("ok") is True and "mainThreadAgeMs" in pres
    checks["ping_modules"] = isinstance(pres.get("modules"), dict) and "revision" in pres
    print(f"ping               -> ok={pres.get('ok')} mainThreadAgeMs={pres.get('mainThreadAgeMs')} "
          f"drainBlocked={pres.get('drainBlocked')} queueDepth={pres.get('queueDepth')} "
          f"modules={pres.get('modules')} revision={pres.get('revision')}")

    # 6b. every control response parses as JSON (shape regressions)
    for mid, meth in (("cap1", "control.capabilities"), ("ver1", "control.version"),
                      ("st1", "control.status")):
        r = rpc(s, {"jsonrpc": "2.0", "id": mid, "method": meth, "params": {}})
        checks[f"parse_{meth}"] = "result" in r or "error" in r
    print("control responses  -> capabilities/version/status parsed OK")

    # 7. second client gets Busy while this one is authenticated (bridge replies on accept)
    s2 = connect(disc)
    try:
        r2 = json.loads(s2.makefile("rb").readline())
    except Exception as e:
        r2 = {"error": {"code": "exception", "message": str(e)}}
    checks["busy"] = r2.get("error", {}).get("code") == -32007
    print(f"second client      -> {r2.get('error', {}).get('code')}")
    s2.close()

    # 8. unknown method -> -32601, blocked method -> -32011 + blockedOn
    r = rpc(s, {"jsonrpc": "2.0", "id": "u1", "method": "control.bogus", "params": {}})
    checks["unknown"] = r.get("error", {}).get("code") == -32601
    print(f"unknown method     -> {r.get('error', {}).get('code')}")
    r = rpc(s, {"jsonrpc": "2.0", "id": "b1", "method": "material.assign",
                "params": {"handle": "n0", "material": "materials/dev/dev_measuregeneric01.vmat"}})
    blocked = r.get("error", {})
    checks["blocked"] = blocked.get("code") == -32011 and bool(blocked.get("data", {}).get("blockedOn"))
    print(f"blocked method     -> {blocked.get('code')} blockedOn={blocked.get('data', {}).get('blockedOn')}")

    # 9. negative deadline is rejected
    r = rpc(s, {"jsonrpc": "2.0", "id": "dm1", "method": "doc.active",
                "params": {"deadlineMs": -1}})
    checks["deadline_neg"] = r.get("error", {}).get("code") == -32602
    print(f"deadlineMs < 0     -> {r.get('error', {}).get('code')}")

    # 10. doc.active (informational: null + reason, or a real doc)
    r = rpc(s, {"jsonrpc": "2.0", "id": "d1", "method": "doc.active", "params": {}})
    res = r.get("result", {})
    doc = res.get("doc")
    print(f"doc.active         -> doc={'present' if doc else 'null'} reason={res.get('reason')} "
          f"path={(doc or {}).get('path')!r}")
    if doc:
        print(f"                      dirty={doc.get('dirty')} editable={doc.get('editable')} "
              f"notEditableReason={doc.get('notEditableReason')} epoch={doc.get('epoch')} "
              f"isNewFile={doc.get('isNewFile')} undoName={doc.get('undoName')} "
              f"undoDepth={doc.get('undoDepth')} topSerial={doc.get('topSerial')}")
        checks["doc_epoch"] = isinstance(doc.get("epoch"), int) and doc["epoch"] >= 1
        # editable is runtime state: an open_asset-loaded doc is not activated
        # until Hammer's active-editor-session event fires (see worknote); the
        # bridge must call doc_activate before mutating.
        checks["doc_editable_bool"] = isinstance(doc.get("editable"), bool)
        checks["doc_reason"] = doc.get("notEditableReason") in (
            "ok", "no_root", "bad_root", "inactive", "no_selection", "bad_selection",
            "edit_scope_active", "no_history", "bad_history", "history_not_active")
    checks["doc"] = ("doc" in res)   # informational either way
    if r.get("error"):
        print(f"                      error={r['error']}")

    # 11. map.save on a clean document -> skippedClean (no file write)
    if doc:
        r = rpc(s, {"jsonrpc": "2.0", "id": "sv1", "method": "map.save", "params": {}})
        if r.get("error"):
            print(f"map.save           -> error={r['error']}")
            checks["save_clean"] = False
        else:
            sres = r.get("result", {})
            print(f"map.save           -> saved={sres.get('saved')} "
                  f"skippedClean={sres.get('skippedClean')} path={sres.get('path')!r}")
            checks["save_clean"] = sres.get("saved") is True and sres.get("skippedClean") is True

    s.close()

    ok = all(v for k, v in checks.items())
    print(f"\nchecks: {checks}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
