#!/usr/bin/env python3
"""test_modeldoc.py - ModelDoc live test (lifecycle, render).

The lifecycle landed in the C bridge's m2.11 pass and the render in m2.14; the rows are served
by the Rust bridge now, whose `BRIDGE_BUILD` the companion live scripts compare against.

The orchestrator runs this against the single ModelDoc instance
(`TOOL=modeldoc bridge/runtime.sh up` + `bridge/runtime.sh open
models/mcp_probe/glass_probe.vmdl`, addon-relative: a csgo_addons/... path raises the
"Unrecognized Asset" modal); it never starts a container.

  --static (offline): no bridge, no container, no discovery file. It reads the Rust bridge's
  committed sources and asserts what is still a source fact: `BRIDGE_BUILD`
  (crates/cs2-bridge/src/lib.rs, m3.N or later), the seven modeldoc.* rows in registry.rs
  (implemented on NEED_MODELDOC, advertised, not blocked), pet.render's row, the six
  modeldoc.* bodies that have landed in the ops HANDLERS table, and the generated pins the
  render chain reads (crates/cs2-bridge/src/generated/pins.rs). The C source-shape checks (the
  render include, the chain reasons, the shared render tail and its JSON skeleton) are retired;
  bridge/protocol-diff-m5-tests.md lists each one and the Rust unit test that covers it. This
  is the mode the plan calls the offline/static expectations; run
  `python3 -I bridge/test_modeldoc.py --static`.

Live steps (`python3 -I bridge/test_modeldoc.py`, needs the running ModelDoc instance):

  0. capabilities: poll until modules["modeldoc_editor.dll"] is terminal (verified,
     else FAIL), max 240 s: the verifier hashes every pinned module in turn, ~75 s live
  1. the seven modeldoc.* methods are advertised
  2. modeldoc.active -> a doc whose path ends glass_probe.vmdl, dirty == false
  3. modeldoc.nodeTypes -> count > 0, contains RootNode, RenderMeshFile,
     BreakPieceList
  4. modeldoc.reload -> reloaded, discardedChanges false
  5. modeldoc.save on the clean doc -> skippedClean true
  6. modeldoc.compile -> failed null (status is unreadable, see protocol-diff); on the host the compiled
     game/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl_c mtime advanced
     (poll <= 60 s; the compile is synchronous, but the .vmdl_c readback lags)
  7. copy glass_probe.vmdl to models/mcp_probe/mcp_<RUN>.vmdl on the host, then
     modeldoc.open {path: addon-relative models/...} -> opened, active path ends with it
     (a second session/window stays open; run-unique name, no cleanup call exists
     in the protocol - the copy is left behind like every run-unique artifact)
  8. modeldoc.save on the copy -> skippedClean (not an untitled document)
  9. modeldoc.render 320x240 (bridge m2.14): the live PNG check is the separate
     `bridge/test_modeldoc_render_live.py`, which needs the whole chain (this script proves the
     session/window it renders from); the method must nevertheless be advertised here.

With --write-snapshot FILE (the orchestrator commits it as
hammer-mcp/research/modeldoc-nodetypes.json) `modeldoc.nodeTypes` is dumped as
{"build": "<gameBuild>", "classes": [...]}.

Run-unique identity: the copied model gets the per-run name mcp_<RUN>.vmdl
(time+pid hex), so earlier runs' leftovers can never match this run's step 7.
No fixed sleeps: every wait is a bounded poll against a real signal with a
deadline (capabilities states, .vmdl_c mtime).

Usage: bridge/test_modeldoc.py [--static] [--prefix <wine pfx>] [--project-root <project root>]
       [--write-snapshot <file>] [--wait N]
Exit 0 when every step passes.
"""
import argparse
import json
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge_build import (BUILD_SHAPE, expected_build, handler_names, methods,  # noqa: E402
                          pin_names, row_problems)
from test_client import discovery_path, connect, hello, rpc  # noqa: E402
from cs2mcp_root import PFX_DIR, PROJECT_ROOT  # noqa: E402

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
MODELDOC_METHODS = {"modeldoc.active", "modeldoc.open", "modeldoc.reload",
                    "modeldoc.save", "modeldoc.compile", "modeldoc.nodeTypes",
                    "modeldoc.render"}
# modeldoc.render's body belongs to the render lane (ops/modeldoc.rs: it stays unregistered);
# the row is still checked above, the handler once that lane lands
MODELDOC_SERVED = MODELDOC_METHODS - {"modeldoc.render"}

# ---- offline (--static) expectations: the Rust bridge contract --------------------------------

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
# the generated pins the render chain reads (m2.14 plan section 0) and the module it must stay
# in; S2_OFF_CModelDocEditorSession_m_pMainWnd is a header offset now owned by s2sdk-sys
MODELDOC_CHAIN_PINS = [
    "MDOFF_MAINWND_SCENEWIDGET", "MDOFF_SCENEWIDGET_TOOLWIDGET",
    "MDOFF_TOOLWIDGET_SCENE", "MDOFF_TOOLWIDGET_CAMERA", "MDV_SESSION_VT", "MDV_EDITORWND_VT",
    "MDV_SCENEWIDGET_VT", "MDV_TOOLWIDGET_VT", "MDV_TOOLSCENE_VT", "MDV_SCENECAMERA_VT",
    "MDFN_TOOLSCENE_RENDER", "PIN_MODELDOC",
]

CHECKS = {}


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""))
    return bool(ok)


def poll_until(fn, deadline_s):
    """bounded poll on a real signal (fn -> truthy); no fixed sleeps beyond the tick"""
    end = time.time() + deadline_s
    while True:
        v = fn()
        if v:
            return v
        if time.time() >= end:
            return None
        time.sleep(0.5)


def static_report() -> int:
    """offline: the Rust bridge's modeldoc contract (build string, the seven rows, the pins).

    Every check reads a committed file; nothing here talks to a bridge or a container and a
    missing/renamed file is a FAIL, never a skip.
    """
    try:
        build = expected_build()
        rows = methods()
        served = handler_names()
        pins = pin_names()
    except (OSError, RuntimeError) as ex:
        print(f"FAIL: cannot read the Rust bridge sources: {ex}")
        return 1

    # the build string: one `pub const BRIDGE_BUILD` in lib.rs, m3.N or later
    step("static_build_string", BUILD_SHAPE.match(build) is not None,
         f"BRIDGE_BUILD={build!r} (expected m3.N or later)")

    # the modeldoc.* method table: the seven rows exist in registry.rs
    present = sorted(name for name in MODELDOC_METHODS if name in rows)
    step("static_method_list", set(present) == MODELDOC_METHODS,
         f"rows={present} missing={sorted(MODELDOC_METHODS - set(present))}")

    # ...and every one is implemented on NEED_MODELDOC (advertised, not blocked)
    bad = {name: row_problems(rows, name, "NEED_MODELDOC") for name in MODELDOC_METHODS}
    bad = {name: why for name, why in bad.items() if why}
    step("static_method_rows", not bad, f"rows={bad}")

    # pet.render's row is untouched by this pass
    step("static_pet_render_row",
         not row_problems(rows, "pet.render", "NEED_PET"),
         f"row={row_problems(rows, 'pet.render', 'NEED_PET')}")

    # the bodies that have landed are registered in ops/modeldoc.rs
    step("static_modeldoc_served", MODELDOC_SERVED <= served,
         f"missing={sorted(MODELDOC_SERVED - served)}")

    # the generated pins the render chain reads (never a pet pin)
    missing = [pin for pin in MODELDOC_CHAIN_PINS if pin not in pins]
    step("static_modeldoc_pins", not missing, f"missing={missing}")

    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true",
                    help="offline source expectations (m2.14 method list, build string, render tail)")
    ap.add_argument("--prefix", default=None)
    ap.add_argument("--project-root", default=PROJECT_ROOT)
    ap.add_argument("--write-snapshot", default=None)
    ap.add_argument("--wait", type=float, default=0.0)
    args = ap.parse_args()
    if args.static:
        return static_report()
    prefix = args.prefix or PFX_DIR
    app = os.path.join(args.project_root, "workshop", "run", "app")
    model = os.path.join(app, "content", "csgo_addons", "mcp_test", "models", "mcp_probe")

    path = discovery_path(prefix)
    if not poll_until(lambda: os.path.exists(path), args.wait):
        print(f"FAIL: discovery file not found: {path}")
        return 2
    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']}")
    s = connect(disc)
    hello(s, disc["token"])

    # 0. modeldoc_editor.dll verified (terminal state) before anything else
    def md_state():
        r = rpc(s, {"jsonrpc": "2.0", "id": "c0", "method": "control.capabilities", "params": {}})
        return r.get("result", {}).get("modules", {}).get("modeldoc_editor.dll", {}).get("state")

    # poll_until stops on the first truthy value: wait for a terminal state, not just any string
    poll_until(lambda: md_state() in ("verified", "mismatch"), 240.0)
    state = md_state()
    if state != "verified":
        print(f"FAIL: modeldoc_editor.dll state={state!r} after 240 s (mismatch would fail closed)")
        return 2
    print(f"PASS: module state verified (modeldoc_editor.dll)")

    def call(rid, method, params):
        return rpc(s, {"jsonrpc": "2.0", "id": rid, "method": method, "params": params})

    # 1. the seven methods are advertised
    r = call("cap", "control.capabilities", {})
    methods = set(r.get("result", {}).get("methods", []))
    step("methods_advertised", MODELDOC_METHODS <= methods,
         f"missing={sorted(MODELDOC_METHODS - methods)}")

    # 2. active document: the glass probe, clean (an already-open file just re-activates its
    #    session, so a previous run's copy being active is harmless)
    call("a0", "modeldoc.open", {"path": "models/mcp_probe/glass_probe.vmdl"})
    r = call("a1", "modeldoc.active", {})
    doc = r.get("result", {}).get("doc")
    ok = isinstance(doc, dict) and doc.get("path", "").endswith("glass_probe.vmdl") \
        and doc.get("dirty") is False
    step("active_doc", ok, f"path={doc.get('path')!r}" if doc else f"resp={r}")
    if not ok:
        return 1

    # 3. node types: authoritative _class list
    r = call("nt1", "modeldoc.nodeTypes", {})
    res = r.get("result") or {}
    classes = [t.get("className") for t in res.get("types", [])]
    step("nodeTypes", res.get("count", 0) > 0 and
         {"RootNode", "RenderMeshFile", "BreakPieceList"} <= set(classes),
         f"count={res.get('count')} stderr={r.get('error')}")
    if args.write_snapshot:
        os.makedirs(os.path.dirname(os.path.abspath(args.write_snapshot)) or ".", exist_ok=True)
        with open(args.write_snapshot, "w") as f:
            json.dump({"build": disc["gameBuild"],
                       "classes": sorted(filter(None, classes))}, f, indent=1)
            f.write("\n")
        print(f"snapshot: {args.write_snapshot}")

    # 4. reload: no prompt, discards nothing (clean doc)
    r = call("r1", "modeldoc.reload", {})
    res = r.get("result") or {}
    step("reload", res.get("reloaded") is True and res.get("discardedChanges") is False,
         f"resp={r}")

    # 5. save on a clean document: nothing written
    r = call("s1", "modeldoc.save", {})
    res = r.get("result") or {}
    step("save_clean", res.get("saved") is True and res.get("skippedClean") is True,
         f"resp={r}")

    # 6. compile through the editor (synchronous, no modal) + the .vmdl_c truth
    compiled = os.path.join(app, "game", "csgo_addons", "mcp_test", "models", "mcp_probe",
                            "glass_probe.vmdl_c")
    t0 = os.stat(compiled).st_mtime if os.path.exists(compiled) else 0
    r = call("c1", "modeldoc.compile", {})
    res = r.get("result") or {}
    step("compile_answered", "failed" in res and res.get("failed") is None, f"resp={r}")
    step("compile_output",
         poll_until(lambda: os.path.exists(compiled) and os.stat(compiled).st_mtime > t0, 60.0),
         f".vmdl_c mtime was {t0}")

    # 7. open a run-unique copy -> a second session, active follows it
    copy_path = os.path.join(model, f"mcp_{RUN}.vmdl")
    shutil.copyfile(os.path.join(model, "glass_probe.vmdl"), copy_path)
    r = call("o1", "modeldoc.open",
             {"path": f"models/mcp_probe/mcp_{RUN}.vmdl"})   # relative to the addon content root
    res = r.get("result") or {}
    step("open_copy", res.get("opened") is True and
         (res.get("active") or {}).get("path", "").endswith(f"mcp_{RUN}.vmdl"),
         f"resp={r}")

    # 7b. a file created after the tools started has no asset object (live 2026-10-12: neither
    #     reload nor a resourcecompiler build binds one) -> compile must refuse, not no-op
    if (res.get("active") or {}).get("hasAsset") is False:
        r = call("c2", "modeldoc.compile", {})
        step("compile_new_no_asset",
             ((r.get("error") or {}).get("data") or {}).get("reason") == "no_asset", f"resp={r}")

    # 7c. bad paths are refused BEFORE OpenFile (a modal would block the main thread): the
    #     follow-up modeldoc.active proves the main thread still drains
    bad = ["/app/content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl",
           "Z:/app/content/x.vmdl", "models/mcp_probe/glass_probe.vmat", "models/a\tb.vmdl"]
    codes = [((call(f"b{i}", "modeldoc.open", {"path": p}).get("error") or {}).get("code")) for i, p in enumerate(bad)]
    alive = (call("b9", "modeldoc.active", {}).get("result") or {}).get("doc")
    step("open_refuses_bad_paths", codes == [-32602] * len(bad) and isinstance(alive, dict),
         f"codes={codes} alive={bool(alive)}")

    # 8. save the copy (opened from a file: not an untitled document, clean)
    r = call("s2", "modeldoc.save", {})
    res = r.get("result") or {}
    step("save_copy", res.get("saved") is True and res.get("skippedClean") is True,
         f"resp={r}")

    s.close()
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
