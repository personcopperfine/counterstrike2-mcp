#!/usr/bin/env python3
"""test_view_render.py - task 13: view.render (Hammer offscreen render -> PNG).

  1. default call (viewport camera, 640x360) -> PNG on disk, decodes, non-blank
  2. explicit camera A and camera B (different yaw) -> different pixels (crc32)
  3. camera A again -> same pixels or a near-identical image (mean |diff| small)
  4. hideToolMaterials -> different pixels; the next default render is bit-identical to A
  5. parameter guards (nothing renders): width 8, fov 0, bad name, malformed origin -> -32602
  6. the document stays clean (view.render must not dirty the map)

Usage:
  bridge/test_view_render.py                      # full test
  bridge/test_view_render.py --params '{"width":320,"height":180}'
  bridge/test_view_render.py --explore            # fov semantics + tool materials
  bridge/test_view_render.py --call doc.active '{}'
Exit 0 on pass. PNGs land in <app>/renders/ (Z:\\app\\renders\\ in the container).
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


def host_path(win):
    p = win.replace("\\", "/")
    if p[:2].lower() == "z:":
        p = p[2:]
    if p.startswith("/app/"):
        return APP + p[4:]
    return p


def read_png_rgb(path):
    """minimal PNG reader for the bridge's output (8-bit RGB, filter 0)."""
    d = open(path, "rb").read()
    assert d[:8] == b"\x89PNG\r\n\x1a\n", "bad signature"
    pos, idat, w = 8, b"", None
    while pos < len(d):
        n, = struct.unpack(">I", d[pos:pos + 4])
        typ = d[pos + 4:pos + 8]
        body = d[pos + 8:pos + 8 + n]
        crc, = struct.unpack(">I", d[pos + 8 + n:pos + 12 + n])
        assert zlib.crc32(typ + body) & 0xFFFFFFFF == crc, "chunk crc %r" % typ
        if typ == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            assert depth == 8 and ctype == 2, "expected RGB8"
        elif typ == b"IDAT":
            idat += body
        pos += 12 + n
    raw = zlib.decompress(idat)
    stride = 1 + 3 * w
    assert len(raw) == stride * h, "raw size"
    rows = [raw[y * stride + 1:(y + 1) * stride] for y in range(h)]
    assert all(raw[y * stride] == 0 for y in range(h)), "filter bytes"
    return w, h, b"".join(rows)


def mean_abs_diff(a, b):
    return sum(abs(x - y) for x, y in zip(a, b)) / max(1, len(a))


def explore(rpc):
    """fov semantics (horizontal vs vertical) and hideToolMaterials."""
    base = rpc("view.render", {"width": 64, "height": 64, "name": "t13_probe"})["result"]["camera"]
    cam = {"origin": base["origin"], "angles": base["angles"], "fov": 90}
    wide = rpc("view.render", dict(cam, width=640, height=360, name="t13_fov_wide"))["result"]
    square = rpc("view.render", dict(cam, width=640, height=640, name="t13_fov_square"))["result"]
    _, _, pw = read_png_rgb(host_path(wide["path"]))
    _, _, ps = read_png_rgb(host_path(square["path"]))
    row_w = pw[180 * 640 * 3:181 * 640 * 3]
    row_s = ps[320 * 640 * 3:321 * 640 * 3]
    col_w = bytes(pw[(y * 640 + 320) * 3 + c] for y in range(360) for c in range(3))
    col_s = bytes(ps[(y * 640 + 320) * 3 + c] for y in range(140, 500) for c in range(3))
    print("center rows    mean|diff| %.2f  (small -> fov is horizontal)" % mean_abs_diff(row_w, row_s))
    print("center columns mean|diff| %.2f  (square rows 140..499 vs wide 0..359)" % mean_abs_diff(col_w, col_s))
    for hide in (False, True, False):
        r = rpc("view.render", dict(cam, width=640, height=360, hideToolMaterials=hide,
                                    name="t13_tools_" + ("hidden" if hide else "shown")))
        res = r.get("result")
        print("hideTools=%-5s -> %s" % (hide, json.dumps({k: res[k] for k in ("toolMaterials", "stats") if k in res}) if res else r.get("error")))
    doc = rpc("doc.active")["result"]["doc"]
    print("doc after      dirty=%s epoch=%s undoDepth=%s" % (doc["dirty"], doc["epoch"], doc["undoDepth"]))
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--params", help="single call with these JSON params, print the result")
    ap.add_argument("--explore", action="store_true", help="fov semantics + hideToolMaterials renders")
    ap.add_argument("--call", nargs=2, metavar=("METHOD", "JSON"), help="one raw request (diagnostics)")
    a = ap.parse_args()
    disc = json.load(open(os.path.join(a.prefix, "drive_c", "users", "steamuser",
                                       "AppData", "Local", "cs2mcpbridge", "session.json")))
    s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
    f = s.makefile("r")
    seq = [0]

    def rpc(method, params=None):
        seq[0] += 1
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": str(seq[0]), "method": method,
                               "params": params or {}}) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    rpc("control.hello", {"protocol": 1, "token": disc["token"]})
    if a.explore:
        return explore(rpc)
    if a.call:
        print(json.dumps(rpc(a.call[0], json.loads(a.call[1]))))
        return 0
    if a.params is not None:
        r = rpc("view.render", json.loads(a.params))
        print(json.dumps(r, indent=1))
        if "result" in r:
            w, h, px = read_png_rgb(host_path(r["result"]["path"]))
            print("png ok: %dx%d, %d bytes of pixels" % (w, h, len(px)))
        return 0 if "result" in r else 1

    checks = {}
    doc0 = rpc("doc.active")["result"]["doc"]

    r = rpc("view.render", {"name": "t13_default"})
    res = r.get("result")
    print("default     ->", json.dumps(res or r.get("error"))[:600])
    checks["default"] = bool(res)
    if res:
        w, h, px = read_png_rgb(host_path(res["path"]))
        checks["default_png"] = (w, h) == (640, 360)
        checks["default_nonblank"] = res["stats"]["untouchedPixels"] == 0 and res["stats"]["maxLuma"] > res["stats"]["minLuma"]
        cam = res["camera"]
    else:
        cam = {"origin": [0, 0, 512], "angles": [30, 0, 0], "fov": 90}

    A = {"origin": cam["origin"], "angles": [cam["angles"][0], cam["angles"][1], 0], "fov": 90,
         "width": 480, "height": 270}
    B = dict(A, angles=[cam["angles"][0], (cam["angles"][1] + 90.0) % 360.0, 0])
    ra = rpc("view.render", dict(A, name="t13_A")).get("result")
    rb = rpc("view.render", dict(B, name="t13_B")).get("result")
    ra2 = rpc("view.render", dict(A, name="t13_A2")).get("result")
    checks["explicit"] = bool(ra and rb and ra2)
    if ra and rb and ra2:
        _, _, pa = read_png_rgb(host_path(ra["path"]))
        _, _, pb = read_png_rgb(host_path(rb["path"]))
        _, _, pa2 = read_png_rgb(host_path(ra2["path"]))
        dab, daa = mean_abs_diff(pa, pb), mean_abs_diff(pa, pa2)
        print("A vs B      -> crc %s/%s mean|diff| %.2f" % (ra["stats"]["crc32"], rb["stats"]["crc32"], dab))
        print("A vs A2     -> crc %s/%s mean|diff| %.2f" % (ra["stats"]["crc32"], ra2["stats"]["crc32"], daa))
        print("timings     -> A %s, B %s" % (ra["ms"], rb["ms"]))
        checks["camera_changes_image"] = dab > 4.0
        checks["same_camera_stable"] = daa < dab / 4

    # hideToolMaterials: flipped through Hammer's own toggle handler inside the call
    rt = rpc("view.render", dict(A, hideToolMaterials=True, name="t13_A_notools")).get("result")
    ra3 = rpc("view.render", dict(A, name="t13_A3")).get("result")
    if rt and ra3 and ra:
        print("A notools   -> crc %s (%s), A again %s" % (rt["stats"]["crc32"], rt["toolMaterials"], ra3["stats"]["crc32"]))
        checks["hide_tools_changes_image"] = rt["stats"]["crc32"] != ra["stats"]["crc32"] and rt["toolMaterials"] == "hidden"
        checks["hide_tools_restored"] = ra3["stats"]["crc32"] == ra["stats"]["crc32"] and ra3["toolMaterials"] == "shown"
    else:
        checks["hide_tools_changes_image"] = checks["hide_tools_restored"] = False

    for label, params in (("width 8", {"width": 8}), ("fov 0", {"fov": 0}),
                          ("bad name", {"name": "../x"}), ("bad origin", {"origin": [1, "x", 3]})):
        e = rpc("view.render", params).get("error", {})
        print("%-11s -> %s %s" % (label, e.get("code"), (e.get("data") or {}).get("reason")))
        checks["guard_" + label.replace(" ", "_")] = e.get("code") == -32602

    doc1 = rpc("doc.active")["result"]["doc"]
    checks["doc_clean"] = doc1["dirty"] is False and doc1["epoch"] == doc0["epoch"] and doc1["undoDepth"] == doc0["undoDepth"]
    ping = rpc("control.ping")["result"]
    checks["alive"] = ping.get("ok") is True

    print("\nchecks:", checks)
    ok = all(checks.values())
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
