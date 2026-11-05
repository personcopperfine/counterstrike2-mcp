#!/usr/bin/env python3
"""test_modeldoc_render_live.py - m2.14: modeldoc.render (the ModelDoc main viewport to PNG).

Run by the orchestrator against the live ModelDoc instance that `bridge/test_modeldoc.py` already
passed: `TOOL=modeldoc bridge/runtime.sh up` plus an open .vmdl tab (`bridge/runtime.sh open
models/mcp_probe/glass_probe.vmdl`, addon-relative). It never starts a container and never
calls podman. The call is read-only - it renders the viewport's current camera and writes one PNG
under Z:\\app\\renders, so no document, selection or camera is touched (m2.14 plan section 3).

  1. control.version.bridgeVersion == the Rust bridge's `BRIDGE_BUILD`; control.capabilities
     advertises modeldoc.render
     (gated on NEED_MODELDOC: modeldoc_editor.dll verified)
  2. modeldoc.render {width:320, height:240, name:modeldoc_<RUN>}:
       - result has pet.render's exact shape: {path, file, width, height, bytes, format,
         hasDocument, stats{untouchedPixels, nonBlackPixels, meanLuma, minLuma, maxLuma, crc32},
         durationMs}
       - path == Z:\\app\\renders\\<name>.png, file == <name>.png
       - the host file exists, is exactly `bytes` long, and its PNG IHDR is 320x240 (8-bit RGB)
       - stats.untouchedPixels == 0 (the readback ran: no sentinel pixels survived)
       - stats.maxLuma > stats.minLuma (the render is not one flat colour), crc32 is 8 hex chars
       - format == "RGBA8888", durationMs an int, hasDocument agrees with modeldoc.active
  3. a second render with the same name rewrites the same path, still untouchedPixels == 0
  4. negatives (nothing is rendered or written, the main thread still drains):
       {width:8} -> -32602, {name:".x"} -> -32602, {width:4096, height:4096} -> -32602 (the
       product exceeds the 8388608 cap); each followed by control.ping ok with mainThreadAgeMs
       < 5000, and the renders directory has no new *.png afterwards (the parameters are
       validated before any object is touched, so no file can appear)
  5. the refusals did not wedge the render path: a final 64x64 render succeeds with
     untouchedPixels == 0 and its own host file

Diagnostics: --params '<json>' renders once with those params and prints the raw result;
--call METHOD '<json>' sends one raw request.

Usage: bridge/test_modeldoc_render_live.py [--prefix P] [--project-root R] [--wait N]
       [--params JSON] [--call METHOD JSON]
Exit 0 when every step passes, non-zero on any FAIL.
"""
import argparse
import json
import os
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge_build import expected_build  # noqa: E402
from test_client import discovery_path, connect, hello, rpc  # noqa: E402
from cs2mcp_root import APP_DIR, PROJECT_ROOT  # noqa: E402

BUILD = expected_build()                     # the Rust bridge's single BRIDGE_BUILD

APP = APP_DIR
DEFAULT_PFX = APP + "/compatdata/730/pfx"
RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)

# pet.render's exact result shape (m2.14 section 1: modeldoc.render has the same fields)
RESULT_KEYS = {"path", "file", "width", "height", "bytes", "format", "hasDocument", "stats",
               "durationMs"}
STATS_KEYS = {"untouchedPixels", "nonBlackPixels", "meanLuma", "minLuma", "maxLuma", "crc32"}

CHECKS = {}


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""))
    return bool(ok)


def poll_until(fn, deadline_s):
    """bounded poll on a real signal; no fixed sleeps beyond the tick"""
    end = time.time() + deadline_s
    while True:
        v = fn()
        if v:
            return v
        if time.time() >= end:
            return None
        time.sleep(0.5)


def host_path(win, project_root):
    """Z:\\app\\... -> <project-root>/workshop/run/app/... (like test_pet.py/test_pet_render.py)."""
    p = win.replace("\\", "/")
    if p[:2].lower() == "z:":
        p = p[2:]
    if p.startswith("/app/"):
        return project_root.rstrip("/") + "/workshop/run/app" + p[4:]
    return p


def read_png_size(path):
    """PNG signature + IHDR dimensions (depth 8, colour type 6/2 accepted: RGBA8888/RGB888)."""
    with open(path, "rb") as f:
        d = f.read(26)
    assert d[:8] == b"\x89PNG\r\n\x1a\n", "bad signature"
    assert d[12:16] == b"IHDR", "first chunk is not IHDR"
    w, h, depth, ctype = struct.unpack(">IIBB", d[16:26])
    assert depth == 8, "expected 8-bit"
    assert ctype in (2, 6), "expected RGB or RGBA"
    return w, h


def is_hex8(s):
    return isinstance(s, str) and len(s) == 8 and all(c in "0123456789abcdef" for c in s)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=None)
    ap.add_argument("--project-root", default=PROJECT_ROOT)
    ap.add_argument("--wait", type=float, default=0.0)
    ap.add_argument("--params", help="render once with these JSON params and print the result")
    ap.add_argument("--call", nargs=2, metavar=("METHOD", "JSON"), help="one raw request (diagnostics)")
    args = ap.parse_args()
    prefix = args.prefix or DEFAULT_PFX

    path = discovery_path(prefix)
    if not poll_until(lambda: os.path.exists(path), args.wait):
        print(f"FAIL: discovery file not found: {path}")
        return 2
    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']}")
    s = connect(disc)
    hello(s, disc["token"])
    seq = [0]

    def call(method, params=None):
        seq[0] += 1
        return rpc(s, {"jsonrpc": "2.0", "id": f"r{seq[0]}", "method": method,
                       "params": params or {}})

    def alive():
        p = call("control.ping").get("result") or {}
        age = p.get("mainThreadAgeMs")
        return (p.get("ok") is True and isinstance(age, (int, float)) and age < 5000), age

    if args.call:
        print(json.dumps(call(args.call[0], json.loads(args.call[1])), indent=1))
        s.close()
        return 0
    if args.params is not None:
        r = call("modeldoc.render", json.loads(args.params))
        print(json.dumps(r, indent=1))
        if "result" in r:
            w, h = read_png_size(host_path(r["result"]["path"], args.project_root))
            print("png ok: %dx%d" % (w, h))
        s.close()
        return 0 if "result" in r else 1

    renders = os.path.join(args.project_root.rstrip("/"), "workshop", "run", "app", "renders")

    def render_names():
        try:
            return set(n for n in os.listdir(renders) if n.endswith(".png"))
        except OSError:
            return set()

    ver = call("control.version").get("result") or {}
    step("version", ver.get("bridgeVersion") == BUILD,
         f"bridgeVersion={ver.get('bridgeVersion')!r}")
    meths = set((call("control.capabilities").get("result") or {}).get("methods") or [])
    step("methods_advertised", "modeldoc.render" in meths,
         f"missing={sorted({'modeldoc.render'} - meths)}")

    name = f"modeldoc_{RUN}"
    expect_path = "Z:\\app\\renders\\%s.png" % name
    r = call("modeldoc.render", {"width": 320, "height": 240, "name": name})
    res = r.get("result")
    print("render      ->", json.dumps(res or r.get("error"))[:600])
    if not step("render", bool(res), f"error={r.get('error')}"):
        s.close()
        print(f"\nchecks: {CHECKS}")
        print("RESULT: FAIL")
        return 1

    step("result_shape", set(res) == RESULT_KEYS and set(res.get("stats") or {}) == STATS_KEYS,
         f"keys={sorted(res)} stats={sorted(res.get('stats') or {})}")
    step("path_file", res.get("path") == expect_path and res.get("file") == f"{name}.png",
         f"path={res.get('path')!r} file={res.get('file')!r}")
    step("dims_echo", res.get("width") == 320 and res.get("height") == 240
         and res.get("format") == "RGBA8888"
         and isinstance(res.get("durationMs"), int),
         f"{res.get('width')}x{res.get('height')} fmt={res.get('format')} "
         f"ms={res.get('durationMs')}")

    hp = host_path(res["path"], args.project_root)
    st = res.get("stats") or {}
    step("host_file", os.path.isfile(hp), hp)
    if os.path.isfile(hp):
        w, h = read_png_size(hp)
        step("png_dims", (w, h) == (320, 240), f"host {w}x{h}")
        step("bytes_match", res.get("bytes") == os.path.getsize(hp),
             f"bytes={res.get('bytes')} host={os.path.getsize(hp)}")
        step("crc_hex", is_hex8(st.get("crc32")), f"crc32={st.get('crc32')!r}")
        step("untouched_zero", st.get("untouchedPixels") == 0,
             f"untouchedPixels={st.get('untouchedPixels')} of {320 * 240}")
        step("contrast", st.get("maxLuma", -1) > st.get("minLuma", 256),
             f"minLuma={st.get('minLuma')} maxLuma={st.get('maxLuma')} "
             f"nonBlackPixels={st.get('nonBlackPixels')}")

    # hasDocument is whatever modeldoc.active says (the document is optional for the render)
    doc = (call("modeldoc.active").get("result") or {}).get("doc")
    print(f"modeldoc.active -> doc={'present' if doc else 'null'} path={(doc or {}).get('path')!r}")
    step("has_document_agrees", res.get("hasDocument") == (doc is not None),
         f"hasDocument={res.get('hasDocument')} active_doc={doc is not None}")

    # a second render of the same name: same path, still a real readback
    r2 = call("modeldoc.render", {"width": 320, "height": 240, "name": name})
    res2 = r2.get("result") or {}
    step("render_again", res2.get("path") == expect_path
         and (res2.get("stats") or {}).get("untouchedPixels") == 0,
         f"resp={json.dumps(res2 or r2.get('error'))[:300]}")

    # negatives: refused before any object is touched, nothing written, main thread alive
    before = render_names()
    for label, params in (
        ("width 8", {"width": 8}),
        ("name dot", {"name": ".x"}),
        ("product", {"width": 4096, "height": 4096}),
    ):
        e = (call("modeldoc.render", params).get("error") or {})
        ok_alive, age = alive()
        step("guard_" + label.replace(" ", "_"),
             e.get("code") == -32602 and e.get("message") == "InvalidParams" and ok_alive,
             f"code={e.get('code')} message={e.get('message')!r} pingAge={age}")
    after = render_names()
    step("refused_wrote_nothing", after == before,
         f"new={sorted(after - before)} removed={sorted(before - after)}")

    # the refusals did not wedge the render path
    r3 = call("modeldoc.render", {"width": 64, "height": 64, "name": f"modeldoc_{RUN}_small"})
    res3 = r3.get("result") or {}
    hp3 = host_path(res3["path"], args.project_root) if res3.get("path") else ""
    step("render_after_refusals",
         bool(res3) and res3.get("width") == 64 and os.path.isfile(hp3)
         and (res3.get("stats") or {}).get("untouchedPixels") == 0,
         f"resp={json.dumps(res3 or r3.get('error'))[:300]}")

    s.close()
    ok = all(CHECKS.values())
    print(f"\nrenders: {sorted(after)}")            # run-unique files are left in place, like pet's
    print(f"checks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
