#!/usr/bin/env python3
"""test_pet_render.py - m2.13: pet.render (the particle editor's preview to PNG).

  1. pet.render {width:320, height:240, name:<RUN-stamped>} -> result path, the host file
     exists (Z:\\app maps to <project-root>/workshop/run/app), PNG IHDR is 320x240,
     stats.untouchedPixels == 0 and stats.maxLuma > stats.minLuma
  2. negatives (nothing renders):
     {width:8}                  -> -32602
     {name:".x"}                -> -32602
     {width:4096,height:4096}   -> -32602 (the product exceeds the 8388608 cap)
     after each, control.ping is ok with mainThreadAgeMs < 5000 (the main thread still drains)

Usage (orchestrator-run, against a live bridge; no containers started here):
  bridge/test_pet_render.py
  bridge/test_pet_render.py --params '{"width":320,"height":240}'
  bridge/test_pet_render.py --call pet.render '{"width":320}'
Exit non-zero on any FAIL.
"""
import argparse
import json
import os
import socket
import struct
import sys
import zlib
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import APP_DIR  # noqa: E402

APP = APP_DIR
DEFAULT_PFX = APP + "/compatdata/730/pfx"


def arg_project_root():
    for i, a in enumerate(sys.argv):
        if a == "--project-root" and i + 1 < len(sys.argv):
            return sys.argv[i + 1].rstrip("/")
    return None


PROJECT_ROOT = arg_project_root()


def host_path(win):
    """Z:\\app\\... -> <project-root>/workshop/run/app/... (like test_pet.py)."""
    p = win.replace("\\", "/")
    if p[:2].lower() == "z:":
        p = p[2:]
    if p.startswith("/app/"):
        rel = p[4:]
        if PROJECT_ROOT:
            return PROJECT_ROOT + "/workshop/run/app" + rel
        return APP + rel
    return p


def read_png_size(path):
    """PNG signature + IHDR dimensions (depth 8, colour type 6/2 accepted: RGBA8888/RGB888)."""
    d = open(path, "rb").read()
    assert d[:8] == b"\x89PNG\r\n\x1a\n", "bad signature"
    assert d[12:16] == b"IHDR", "first chunk is not IHDR"
    w, h, depth, ctype = struct.unpack(">IIBB", d[16:26])
    assert depth == 8, "expected 8-bit"
    assert ctype in (2, 6), "expected RGB or RGBA"
    return w, h


def rpc(s, f, seq, method, params=None):
    seq[0] += 1
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": str(seq[0]), "method": method,
                           "params": params or {}}) + "\n").encode())
    line = f.readline()
    if not line:
        raise RuntimeError("connection closed")
    return json.loads(line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--params", help="single call with these JSON params, print the result")
    ap.add_argument("--call", nargs=2, metavar=("METHOD", "JSON"), help="one raw request (diagnostics)")
    a = ap.parse_args()
    disc = json.load(open(os.path.join(a.prefix, "drive_c", "users", "steamuser",
                                       "AppData", "Local", "cs2mcpbridge", "session.json")))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
    f = s.makefile("r")
    seq = [0]

    def call(method, params=None):
        return rpc(s, f, seq, method, params)

    call("control.hello", {"protocol": 1, "token": disc["token"]})
    if a.call:
        print(json.dumps(call(a.call[0], json.loads(a.call[1]))))
        return 0
    if a.params is not None:
        r = call("pet.render", json.loads(a.params))
        print(json.dumps(r, indent=1))
        if "result" in r:
            w, h = read_png_size(host_path(r["result"]["path"]))
            print("png ok: %dx%d" % (w, h))
        return 0 if "result" in r else 1

    checks = {}

    run = os.environ.get("PET_RENDER_RUN") or str(os.getpid())
    name = "pet_%s" % run
    r = call("pet.render", {"width": 320, "height": 240, "name": name})
    res = r.get("result")
    print("render      ->", json.dumps(res or r.get("error"))[:600])
    checks["render"] = bool(res)
    if res:
        checks["path_field"] = (res["file"] == "%s.png" % name
                                and res["path"] == "Z:\\app\\renders\\%s.png" % name)
        hp = host_path(res["path"])
        checks["host_file"] = os.path.isfile(hp)
        if checks["host_file"]:
            w, h = read_png_size(hp)
            checks["png_dims"] = (w, h) == (320, 240)
        st = res.get("stats", {})
        checks["untouched_zero"] = st.get("untouchedPixels") == 0
        checks["contrast"] = st.get("maxLuma", -1) > st.get("minLuma", 256)
        checks["format"] = res.get("format") == "RGBA8888"
        checks["duration"] = isinstance(res.get("durationMs"), int)

    for label, params in (
        ("width 8", {"width": 8}),
        ("name dot", {"name": ".x"}),
        ("product", {"width": 4096, "height": 4096}),
    ):
        e = call("pet.render", params).get("error", {})
        print("%-10s -> %s %s" % (label, e.get("code"), (e.get("data") or {}).get("reason")))
        checks["guard_" + label.replace(" ", "_")] = e.get("code") == -32602
        ping = call("control.ping")["result"]
        age = ping.get("mainThreadAgeMs")
        checks["alive_after_" + label.replace(" ", "_")] = ping.get("ok") is True and age is not None and age < 5000

    print("\nchecks:", checks)
    ok = all(checks.values())
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
