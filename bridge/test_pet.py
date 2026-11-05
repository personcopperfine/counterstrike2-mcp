#!/usr/bin/env python3
"""test_pet.py - particle editor (pet) first pass live test (bridge m2.12, pet plan WP1).

The orchestrator runs this against a tools session (the csgo/hammer session, `bridge/runtime.sh
up`); it never starts a container and never calls podman.

  0. capabilities + version: poll until `pet.load` is advertised (toolframework2.dll verified;
     the verifier hashes every pinned module in turn), then check bridgeVersion/gameBuild
  1. pet.load -> alreadyLoaded (fine) or loaded:true. loaded:false is a FAIL: a failed load is
     cached by the framework and pops a modal, so the runtime has to be restarted
  2. poll modules["pet.dll"].state to a terminal state (<= 240 s); it must be `verified`
     (verification hashes 9.3 MB - the module poll picks the DLL up within ~32 frames)
  3. the twelve pet.* methods are advertised
  4. pet.open {path: particles/mcp_probe/probe.vpcf} -> opened, active.hasRoot:true and the
     type-1 (m_Emitters) array classes == ["C_OP_ContinuousEmitter"]
  5. pet.active -> path ends probe.vpcf, dirty false
  6. pet.reload -> reloaded, hasRoot true, discardedChanges false
  7. pet.save on the clean doc -> skippedClean true (nothing is written)
  8. bad paths (absolute, drive-qualified, .vmdl, control character, ..) -> five -32602, and a
     following pet.active still answers (the main thread drains: OpenFile's modals never ran)
  9. pet.schema: C_OP_ContinuousEmitter found with fields and CParticleFunction in bases;
     CParticleSystemDefinition has m_nMaxParticles; NoSuchClass_x -> found:false
 10. pet.functionTypes: count > 0, C_OP_RenderSprites at type 6 and C_OP_ContinuousEmitter at
     type 1, every className non-empty ASCII, and the type-1 class names include the class
     pet.active reported in step 4 (the GetSchemaBinding deref cross-check)
 11. negative on purpose: a run-unique copy of probe.vpcf is opened, then overwritten with two
     malformed variants (the live stack-overflow inputs: `{{{ not kv3` and a valid header with a
     truncated body); each `pet.reload` must be REFUSED with -32602 UnloadableFile and a non-empty
     data.reason, the document must still have its root and `control.ping.mainThreadAgeMs` must
     stay under 5 s - pet's ReloadDocument on such a file recurses until the Wine main thread's
     stack overflows and wedges the whole process, so the bridge lex-checks before calling it. The
     copy's content is restored, it reloads with hasRoot:true again, and the copy is deleted

With --write-snapshot FILE the authoritative schema snapshot (the pet plan §0.3 shape) is
written from pet.functionTypes + pet.schema for every distinct registry class plus the root
class and the two structural classes; `placeholder:false`. A single failed/absent pet.schema
aborts the write (no partial snapshot).

Run-unique identity: the copy is particles/mcp_probe/mcp_<RUN>.vpcf (time+pid hex), so earlier
runs' leftovers can never match this run's step 11. No fixed sleeps: every wait is a bounded
poll against a real signal with a deadline (advertised methods, module states).

Usage: bridge/test_pet.py [--prefix <wine pfx>] [--project-root <project root>]
       [--write-snapshot <file>] [--wait N]
Exit 0 when every step passes, non-zero on any FAIL.
"""
import argparse
import json
import os
import shutil
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge_build import expected_build  # noqa: E402
from test_client import discovery_path, connect, hello, rpc  # noqa: E402
from cs2mcp_root import PFX_DIR, PROJECT_ROOT  # noqa: E402

BUILD = expected_build()                     # the Rust bridge's single BRIDGE_BUILD

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
PET_METHODS = {"pet.load", "pet.active", "pet.open", "pet.reload", "pet.save",
               "pet.functionTypes", "pet.schema", "pet.text", "pet.apply", "pet.undo",
               "pet.redo", "pet.render"}

# the shared function-array contract (pet plan §0.1): type index -> file key / element class
ARRAYS = [
    {"type": 0, "key": "m_PreEmissionOperators", "elementClass": None},
    {"type": 1, "key": "m_Emitters", "elementClass": None},
    {"type": 2, "key": "m_Initializers", "elementClass": None},
    {"type": 3, "key": "m_Operators", "elementClass": None},
    {"type": 4, "key": "m_ForceGenerators", "elementClass": None},
    {"type": 5, "key": "m_Constraints", "elementClass": None},
    {"type": 6, "key": "m_Renderers", "elementClass": None},
    {"type": 7, "key": "m_Children", "elementClass": "ParticleChildrenInfo_t"},
    {"type": 8, "key": "m_NamedValueLocals", "elementClass": "ParticleNamedValueSource_t"},
]
ROOT_CLASS = "CParticleSystemDefinition"
STRUCT_CLASSES = ["ParticleChildrenInfo_t", "ParticleNamedValueSource_t",
                  "ParticleNamedValueConfiguration_t"]
PROBE = "particles/mcp_probe/probe.vpcf"
# the real .vpcf header line + the two live stack-overflow inputs (pet's ReloadDocument recursed
# until the Wine main thread's stack overflowed; the bridge now refuses them before the call)
KV3_HEADER = ("<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} "
              "format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->")
BROKEN_VARIANTS = [
    ("kv3_garbage", b"{{{ not kv3\n"),
    ("truncated_body", (KV3_HEADER + "\r\n{\r\n\t_class = \"CParticleSystemDefinition\"\r\n"
                                    "\tm_Emitters = [ { _class = \r\n").encode()),
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


def is_ascii(s):
    return bool(s) and all(32 <= ord(c) < 127 for c in s)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=None)
    ap.add_argument("--project-root", default=PROJECT_ROOT)
    ap.add_argument("--write-snapshot", default=None)
    ap.add_argument("--wait", type=float, default=0.0)
    args = ap.parse_args()
    prefix = args.prefix or PFX_DIR
    addon = os.path.join(args.project_root, "workshop", "run", "app", "content",
                         "csgo_addons", "mcp_test")
    probe_host = os.path.join(addon, "particles", "mcp_probe", "probe.vpcf")
    copy_rel = f"particles/mcp_probe/mcp_{RUN}.vpcf"
    copy_host = os.path.join(addon, "particles", "mcp_probe", f"mcp_{RUN}.vpcf")

    path = discovery_path(prefix)
    if not poll_until(lambda: os.path.exists(path), args.wait):
        print(f"FAIL: discovery file not found: {path}")
        return 2
    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']}")
    s = connect(disc)
    hello(s, disc["token"])

    def call(rid, method, params):
        return rpc(s, {"jsonrpc": "2.0", "id": rid, "method": method, "params": params})

    def methods():
        r = call("cap", "control.capabilities", {})
        return set(r.get("result", {}).get("methods", []))

    # 0. capabilities + version. pet.load is the only pet method that can be advertised before
    #    pet.dll exists (NEED_TF2), so waiting for it is waiting for toolframework2 to verify.
    if not poll_until(lambda: "pet.load" in methods(), 240.0):
        print("FAIL: pet.load was never advertised (toolframework2.dll not verified after 240 s)")
        return 2
    r = call("v1", "control.version", {})
    ver = r.get("result") or {}
    step("version", ver.get("bridgeVersion") == BUILD and bool(ver.get("gameBuild")),
         f"bridgeVersion={ver.get('bridgeVersion')!r} gameBuild={ver.get('gameBuild')!r}")

    # 1. load: pet.dll is not mapped yet -> the framework loads it once. An already-mapped DLL is
    #    reported as alreadyLoaded (a second LoadToolSystem call is never made).
    r = call("l1", "pet.load", {})
    res = r.get("result") or {}
    step("pet_load", res.get("loaded") is True,
         f"resp={r}" + (" - a modal may be up: restart the runtime" if res else ""))
    if not res.get("loaded"):
        s.close()
        print(f"\nchecks: {CHECKS}")
        print("RESULT: FAIL")
        return 1

    # 2. the module poll picks pet.dll up, then the verifier hashes it (9.3 MB)
    def pet_state():
        return call("c0", "control.capabilities", {}).get("result", {}) \
            .get("modules", {}).get("pet.dll", {}).get("state")
    poll_until(lambda: pet_state() in ("verified", "mismatch"), 240.0)
    state = pet_state()
    if state != "verified":
        print(f"FAIL: pet.dll state={state!r} after 240 s (mismatch fails closed)")
        s.close()
        return 2
    print("PASS: module state verified (pet.dll)")

    # 3. the twelve methods are advertised now that pet.dll is verified
    both = methods()
    step("methods_advertised", PET_METHODS <= both,
         f"missing={sorted(PET_METHODS - both)}")

    # 4. open the probe: pet's own OpenFile, addon-content-relative
    r = call("o1", "pet.open", {"path": PROBE})
    res = r.get("result") or {}
    active = res.get("active") or {}
    emitters = [a.get("classes") for a in (active.get("arrays") or []) if a.get("type") == 1]
    step("open_probe", res.get("opened") is True and active.get("hasRoot") is True
         and emitters == [["C_OP_ContinuousEmitter"]],
         f"resp={r}")
    if not step("open_probe_doc", bool(active.get("path", "").endswith("probe.vpcf"))
                and active.get("dirty") is False,
                f"path={active.get('path')!r} dirty={active.get('dirty')}"):
        s.close()
        print(f"\nchecks: {CHECKS}")
        print("RESULT: FAIL")
        return 1

    # 5. pet.active agrees (the same builder as pet.open's `active`)
    r = call("a1", "pet.active", {})
    doc = (r.get("result") or {}).get("doc")
    step("active_probe", isinstance(doc, dict) and doc.get("path", "").endswith("probe.vpcf")
         and doc.get("dirty") is False and doc.get("hasRoot") is True,
         f"resp={r}")

    # 6. reload: no prompt, discards nothing (clean document)
    r = call("r1", "pet.reload", {})
    res = r.get("result") or {}
    step("reload", res.get("reloaded") is True and res.get("hasRoot") is True
         and res.get("discardedChanges") is False, f"resp={r}")

    # 7. save on a clean document: nothing written
    r = call("s1", "pet.save", {})
    res = r.get("result") or {}
    step("save_clean", res.get("saved") is True and res.get("skippedClean") is True, f"resp={r}")

    # 8. bad paths are refused BEFORE OpenFile (a modal would block the main thread); the
    #    follow-up pet.active proves the main thread still drains
    bad = ["/app/content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf",
           "Z:/app/content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf",
           "particles/mcp_probe/probe.vmdl", "particles/mcp_probe/a\tb.vpcf",
           "particles/../mcp_probe/probe.vpcf"]
    codes = [((call(f"b{i}", "pet.open", {"path": p}).get("error") or {}).get("code"))
             for i, p in enumerate(bad)]
    alive = (call("b9", "pet.active", {}).get("result") or {}).get("doc")
    step("open_refuses_bad_paths", codes == [-32602] * len(bad) and isinstance(alive, dict),
         f"codes={codes} alive={bool(alive)}")

    # 9. schema read (particles.dll's own type scope)
    r = call("sc1", "pet.schema", {"class": "C_OP_ContinuousEmitter"})
    ce = r.get("result") or {}
    step("schema_operator", ce.get("found") is True and len(ce.get("fields") or []) > 0
         and "CParticleFunction" in (ce.get("bases") or []), f"resp={r}")
    r = call("sc2", "pet.schema", {"class": ROOT_CLASS})
    rs = r.get("result") or {}
    step("schema_root", rs.get("found") is True
         and "m_nMaxParticles" in [f.get("name") for f in (rs.get("fields") or [])], f"resp={r}")
    r = call("sc3", "pet.schema", {"class": "NoSuchClass_x"})
    step("schema_unknown", (r.get("result") or {}).get("found") is False, f"resp={r}")

    # 10. the registry (particles.dll IParticleSystemMgr) + the class-binding cross-check
    r = call("ft1", "pet.functionTypes", {})
    types_res = r.get("result") or {}
    rows = types_res.get("types") or []
    at_type = lambda t: [x.get("className") for x in rows if x.get("type") == t]  # noqa: E731
    step("functionTypes",
         types_res.get("count", 0) > 0
         and "C_OP_RenderSprites" in at_type(6)
         and "C_OP_ContinuousEmitter" in at_type(1)
         and all(is_ascii(x.get("className") or "") for x in rows)
         and all(cls in at_type(1) for cls in (emitters[0] if emitters else [])),
         f"count={types_res.get('count')} type1={len(at_type(1))} type6={len(at_type(6))} "
         f"stderr={r.get('error')}")

    # 11. the loadability guard, on purpose: a malformed file is REFUSED by pet.reload before
    #     ReloadDocument runs, so the document and the main thread survive. Run-unique copy.
    wrote = False
    try:
        orig = open(probe_host, "rb").read()
        shutil.copyfile(probe_host, copy_host)
        wrote = True
        r = call("n0", "pet.open", {"path": copy_rel})
        opened = (r.get("result") or {}).get("opened") is True
        for variant, broken in BROKEN_VARIANTS:
            with open(copy_host, "wb") as f:
                f.write(broken)
            r = call("n1", "pet.reload", {})
            err = r.get("error") or {}
            data = err.get("data") or {}
            r2 = call("n2", "pet.active", {})
            doc2 = (r2.get("result") or {}).get("doc") or {}
            ping = (call("n3", "control.ping", {}).get("result") or {})
            age = ping.get("mainThreadAgeMs")
            step(f"broken_reload_refused_{variant}",
                 opened and err.get("code") == -32602 and bool(err.get("message"))
                 and isinstance(data.get("reason"), str) and bool(data.get("reason"))
                 and doc2.get("hasRoot") is True and ping.get("ok") is True
                 and isinstance(age, (int, float)) and age < 5000,
                 f"error={r.get('error')} active_hasRoot={doc2.get('hasRoot')} "
                 f"mainThreadAgeMs={age}")
        with open(copy_host, "wb") as f:
            f.write(orig)
        r = call("n4", "pet.reload", {})
        res = r.get("result") or {}
        step("restored_reload_root", res.get("reloaded") is True and res.get("hasRoot") is True,
             f"resp={r}")
    finally:
        if wrote:
            try:
                os.unlink(copy_host)
                print(f"cleanup: removed {copy_host}")
            except OSError as ex:
                print(f"cleanup: could not remove {copy_host}: {ex}")

    # snapshot (§0.3), only after every live check above answered
    if args.write_snapshot:
        distinct = sorted({x.get("className") for x in rows if x.get("className")})
        wanted = distinct + [ROOT_CLASS] + STRUCT_CLASSES
        classes = {}
        for name in wanted:
            r = call(f"sn_{len(classes)}", "pet.schema", {"class": name})
            res = r.get("result") or {}
            if res.get("found") is not True:
                step("snapshot_classes", False, f"pet.schema({name!r}) -> {r}")
                s.close()
                print(f"\nchecks: {CHECKS}")
                print("RESULT: FAIL")
                return 1
            classes[name] = {"bases": res.get("bases") or [],
                             "fields": res.get("fields") or [],
                             "metadata": res.get("metadata") or []}
        snap = {"build": disc.get("gameBuild") or ver.get("gameBuild"),
                "placeholder": False, "rootClass": ROOT_CLASS, "arrays": ARRAYS,
                "functions": [{"type": x.get("type"), "className": x.get("className"),
                               "name": x.get("name"), "obsolete": x.get("obsolete"),
                               "singleton": x.get("singleton")} for x in rows],
                "classes": classes}
        os.makedirs(os.path.dirname(os.path.abspath(args.write_snapshot)) or ".", exist_ok=True)
        tmp = args.write_snapshot + ".tmp"
        with open(tmp, "w") as f:
            json.dump(snap, f, indent=1)
            f.write("\n")
        os.replace(tmp, args.write_snapshot)
        step("snapshot_classes", True, f"{len(classes)} classes")
        print(f"snapshot: {args.write_snapshot} ({len(classes)} classes, "
              f"{len(snap['functions'])} function rows)")

    s.close()
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
