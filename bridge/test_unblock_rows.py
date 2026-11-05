#!/usr/bin/env python3
"""test_unblock_rows.py - bridge m3.12 live suite: job.*, mesh.get, selection.*.

The m3.12 wave (branch bridge/unblock-rows, design target/orch/unblock-rows/DESIGN.md) turned the
last `-32011` rows into methods: a bridge job model whose kind `resourcecompiler` runs
resourcecompiler.exe in a Win32 job object (`job.start`/`job.status`/`job.cancel`, lane-answered),
the live mesh read-back `mesh.get`, and `selection.get`/`selection.set` (nodes and faces).
`map.run` stays blocked with the "superseded by run_map" text.

Live mode (default; a running tools session with maps/probe.vmap open: `runtime.sh up`, then
`runtime.sh open maps/probe.vmap`):

  1. version/capabilities: `bridgeVersion` == BRIDGE_BUILD; the five rows advertised; `map.run`
     answers -32011 with the superseded text
  2. mesh.get round trip: `mesh.create` a box with two materials -> `mesh.get` gives 8 vertices,
     6 quads, the sent world positions (local + origin), both materials, face 1 on MAT2; the
     read changes neither undoDepth nor topSerial nor dirty
  3. `mesh.setGeometry` (moved) -> `mesh.get` follows; `undo.undo` -> `mesh.get` is back at the
     created positions (the case the old hammer-mcp cache could not see)
  4. mesh.get refusals: unknown nodeID, a non-mesh node, a bad expectDoc
  5. selection: replace by nodeIDs -> get shows exactly it; add a second mesh; remove one;
     face replace {box, [0, 2]} -> mode faces, exactly those faces; face add [4]; clear ->
     count 0; an unknown nodeID is refused and changes nothing; dirty / undoDepth / topSerial
     unchanged across every selection call; a view.render after the face set (saved, not compared)
  6. jobs: `job.status {}` lists; `job.start` the addon's soundevents (game:true) -> ok with a
     summary and clientRef echoed; `job.start` compile_test.vmap with the map recipe -> running,
     a second start -> -32017 `job: <id> running`, `control.status` lists it with kind,
     `job.cancel` -> cancelled, and no resourcecompiler.exe/vrad3.exe left in the container;
     unknown jobId -> -32602; `--full-map` additionally compiles compile_test.vmap to ok
  7. cleanup: pop this run's undo groups by label, clear the selection

Run-unique identity: meshes live in band 4 (80000 + jitter on the /16 grid, bridge/README.md
"Live test conventions"); nothing here saves the map. Cleanup is reported, not part of the
verdict.

`--static` (offline): BRIDGE_BUILD is m3.12 or later; the five rows are effective-implemented
with the expected needs (job.* 0, the others NEED_HAMMER) and map.run is blocked as superseded;
mesh.get and selection.* have HANDLERS bodies, job.* are lane-served (ops/job.rs); every pin the
new bodies bind exists in generated/pins.rs; protocol-diff-m3.12.md exists. A missing file is a
FAIL, never a skip.

Usage: bridge/test_unblock_rows.py [--static] [--prefix <wine pfx>] [--container cs2tools]
       [--full-map] [--job-wait N]
Exit 0 when every check passes.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import bridge_build  # noqa: E402
from test_client import DEFAULT_PFX, connect, discovery_path, hello, rpc  # noqa: E402
from cs2mcp_root import PODMAN_XDG  # noqa: E402

ROOT = os.path.dirname(HERE)
MAT = "materials/dev/reflectivity_30.vmat"
MAT2 = "materials/tools/toolsnodraw.vmat"
SNDEVTS = "csgo_addons/mcp_test/soundevents/soundevents_addon.vsndevts"
MAP_INPUT = "csgo_addons/mcp_test/maps/compile_test.vmap"
MAP_FLAGS = ["-threads", "8", "-fshallow", "-maxtextureres", "256", "-nop4", "-quiet",
             "-unbufferedio", "-noassert"]
NEW_ROWS = {"job.start": "0", "job.status": "0", "job.cancel": "0",
            "mesh.get": "NEED_HAMMER", "selection.get": "NEED_HAMMER",
            "selection.set": "NEED_HAMMER"}
PODMAN_ENV = dict(os.environ, XDG_RUNTIME_DIR=os.environ.get(
    "XDG_RUNTIME_DIR_PODMAN", PODMAN_XDG))

RUN = "%x%x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
RUN_OFF = 16 * (int(RUN, 16) % 512)
BAND = 80000 + RUN_OFF

CHECKS = []


def check(name, ok, detail=""):
    CHECKS.append((name, bool(ok)))
    print(("PASS " if ok else "FAIL ") + name + (("  -- " + detail) if detail and not ok else ""))
    return ok


# ---------------------------------------------------------------------------------------------
# static
# ---------------------------------------------------------------------------------------------

def run_static():
    build = bridge_build.expected_build()
    m = re.fullmatch(r"m(\d+)\.(\d+)", build)
    check("static: BRIDGE_BUILD >= m3.12", m and (int(m.group(1)), int(m.group(2))) >= (3, 12), build)
    rows = bridge_build.methods()
    for name, needs in NEW_ROWS.items():
        problems = bridge_build.row_problems(rows, name, needs)
        check("static: row %s" % name, not problems, "; ".join(problems))
    run = rows.get("map.run") or {}
    check("static: map.run blocked as superseded",
          not run.get("implemented") and "superseded" in (run.get("blocked_on") or ""), str(run))
    handlers = bridge_build.handler_names()
    for name in ("mesh.get", "selection.get", "selection.set"):
        check("static: HANDLERS has %s" % name, name in handlers)
    job_rs = os.path.join(ROOT, "crates/cs2-bridge/src/ops/job.rs")
    text = bridge_build.read(job_rs)
    for name in ("job.start", "job.status", "job.cancel"):
        check("static: ops/job.rs serves %s" % name, ('"%s"' % name) in text)
    pins = bridge_build.pin_names()
    for pin in ("HFN_MESH_FACE_FIRST_EDGE", "HFN_MESH_EDGE_NEXT", "HFN_MESH_EDGE_VERTEX",
                "HFN_MESH_VERTEX_POSITIONS", "HFN_MESH_FACE_MATERIAL_INDEX",
                "HFN_MESH_MATERIAL_NAME", "HFN_SEL_CURRENT_SET", "HFN_SEL_GET_SET",
                "HFN_SEL_COUNT", "HFN_SEL_GET_NODES", "HFN_SEL_SELECT_NODES", "HFN_SEL_SET_MODE",
                "HFN_FACESEL_GET_MESHES", "HFN_FACESEL_GET_FACES", "HFN_FACESEL_SELECT_FACES"):
        check("static: pin %s" % pin, pin in pins)
    mesh_rs = bridge_build.read(os.path.join(ROOT, "crates/cs2-bridge/src/ops/mesh.rs"))
    check("static: mesh.rs never calls HFN_MESH_EDGE_POSITIONS", "HFN_MESH_EDGE_POSITIONS" not in mesh_rs)
    check("static: protocol-diff-m3.12.md exists",
          os.path.isfile(os.path.join(HERE, "protocol-diff-m3.12.md")))


# ---------------------------------------------------------------------------------------------
# live helpers
# ---------------------------------------------------------------------------------------------

class Bridge:
    def __init__(self, pfx):
        disc = json.load(open(discovery_path(pfx)))
        self.sock = connect(disc)
        self.sock.settimeout(120)
        self.n = 0
        r = hello(self.sock, disc["token"])
        if "result" not in r:
            raise SystemExit("hello failed: %r" % r)
        self.hello = r["result"]

    def call(self, method, params=None):
        self.n += 1
        return rpc(self.sock, {"jsonrpc": "2.0", "id": "u%d" % self.n, "method": method,
                               "params": params or {}})

    def ok(self, method, params=None):
        r = self.call(method, params)
        if "result" not in r:
            raise RuntimeError("%s failed: %s" % (method, json.dumps(r.get("error"))[:400]))
        return r["result"]

    def doc(self):
        return self.ok("doc.active")["doc"]

    def expect(self):
        d = self.doc()
        return {"id": d["id"], "epoch": d["epoch"]}

    def mut(self, method, params):
        p = dict(params)
        p["expectDoc"] = self.expect()
        return self.call(method, p)


def box(x0, y0, z0, s):
    v = [[x0 + dx * s, y0 + dy * s, z0 + dz * s] for dz in (0, 1) for dy in (0, 1) for dx in (0, 1)]
    f = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]]
    return v, f


def world_points(mesh):
    """`mesh.get`'s local vertices to world (identity angles/scales, as mesh.create leaves them)."""
    ox, oy, oz = mesh["origin"]
    return sorted((round(x + ox, 2), round(y + oy, 2), round(z + oz, 2)) for x, y, z in mesh["vertices"])


def pts(vertices):
    return sorted((round(x, 2), round(y, 2), round(z, 2)) for x, y, z in vertices)


def state(b):
    d = b.doc()
    return (d.get("undoDepth"), d.get("topSerial"), d.get("dirty"))


def err_code(r):
    return (r.get("error") or {}).get("code")


def container_procs(container):
    try:
        out = subprocess.run(["podman", "exec", container, "sh", "-c",
                              "ps -eo args | grep -iE 'resourcecompiler|vrad3' | grep -v grep || true"],
                             env=PODMAN_ENV, capture_output=True, text=True, timeout=30)
        return out.stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return "<podman failed: %s>" % e


def wait_job(b, job_id, until, deadline_s):
    end = time.monotonic() + deadline_s
    st = b.ok("job.status", {"jobId": job_id})
    while st["state"] not in until and time.monotonic() < end:
        time.sleep(0.5)  # polling the job state (signal), bounded by the deadline
        st = b.ok("job.status", {"jobId": job_id})
    return st


# ---------------------------------------------------------------------------------------------
# live sections
# ---------------------------------------------------------------------------------------------

def sec_version(b):
    v = b.ok("control.version")
    build = bridge_build.expected_build()
    check("version: bridgeVersion == %s" % build, v.get("bridgeVersion") == build, str(v))
    caps = b.ok("control.capabilities")
    advertised = {m["name"] if isinstance(m, dict) else m for m in caps.get("methods", [])}
    for name in NEW_ROWS:
        check("caps: %s advertised" % name, name in advertised)
    r = b.call("map.run", {})
    data = (r.get("error") or {}).get("data") or {}
    check("map.run: -32011 superseded", err_code(r) == -32011 and "superseded" in str(data.get("blockedOn")), str(r))


def sec_mesh(b, labels):
    v, f = box(BAND, BAND, 0, 64)
    mats = [None] * 6
    mats[1] = MAT2
    r = b.mut("mesh.create", {"vertices": v, "faces": f, "material": MAT, "faceMaterials": mats})
    if not check("mesh: create", "result" in r, str(r)):
        return None, None
    labels.append("create")
    node = r["result"]["nodeID"]
    before = state(b)
    m = b.ok("mesh.get", {"nodeID": node})
    after = state(b)
    check("mesh.get: read-only (undoDepth/topSerial/dirty unchanged)", before == after, "%s -> %s" % (before, after))
    check("mesh.get: space local", m.get("space") == "local", str(m.get("space")))
    check("mesh.get: 8 vertices / 6 faces", m.get("vertexCount") == 8 and m.get("faceCount") == 6
          and len(m["vertices"]) == 8 and len(m["faces"]) == 6, "%s/%s" % (m.get("vertexCount"), m.get("faceCount")))
    check("mesh.get: world positions == sent", world_points(m) == pts(v), "%s vs %s" % (world_points(m), pts(v)))
    check("mesh.get: every face a quad of valid indices",
          all(len(q) == 4 and all(0 <= i < len(m["vertices"]) for i in q) for q in m["faces"]))
    check("mesh.get: materials", sorted(m.get("materials", [])) == sorted([MAT, MAT2]), str(m.get("materials")))
    fm = m.get("faceMaterials", [])
    n2 = sum(1 for i in fm if 0 <= i < len(m["materials"]) and m["materials"][i] == MAT2)
    check("mesh.get: exactly one face on MAT2", n2 == 1, str(fm))
    check("mesh.get: doc member", isinstance(m.get("doc"), dict) and "undoDepth" in m["doc"], str(m.get("doc")))

    moved = [[x, y + 256, z] for x, y, z in v]
    r = b.mut("mesh.setGeometry", {"nodeID": node, "vertices": moved, "faces": f, "material": MAT})
    if check("mesh: setGeometry", "result" in r, str(r)):
        labels.append("setGeometry")
        m2 = b.ok("mesh.get", {"nodeID": node})
        check("mesh.get: follows setGeometry", world_points(m2) == pts(moved), str(world_points(m2)))
        u = b.mut("undo.undo", {})
        if check("mesh: undo setGeometry", "result" in u and u["result"].get("undone"), str(u)):
            labels.pop()
            m3 = b.ok("mesh.get", {"nodeID": node})
            check("mesh.get: follows the undo (back at the created positions)",
                  world_points(m3) == pts(v), str(world_points(m3)))

    r = b.call("mesh.get", {"nodeID": 2147480000})
    check("mesh.get: unknown nodeID refused", err_code(r) == -32022, str(r))
    world = next((n for n in b.ok("node.list").get("nodes", []) if n.get("class") != "mesh"), None)
    if world is not None:
        r = b.call("mesh.get", {"nodeID": world["nodeID"]})
        check("mesh.get: non-mesh refused", err_code(r) == -32602, str(r))
    r = b.call("mesh.get", {"nodeID": node, "expectDoc": {"id": "nope", "epoch": 0}})
    check("mesh.get: bad expectDoc refused", err_code(r) is not None, str(r))

    v2, f2 = box(BAND + 256, BAND, 0, 32)
    r = b.mut("mesh.create", {"vertices": v2, "faces": f2, "material": MAT})
    node2 = None
    if check("mesh: create second", "result" in r, str(r)):
        labels.append("create")
        node2 = r["result"]["nodeID"]
    return node, node2


def sec_selection(b, node, node2, out_dir):
    s0 = state(b)
    r = b.mut("selection.set", {"op": "replace", "nodeIDs": [node]})
    a = (r.get("result") or {}).get("applied") or {}
    check("selection.set replace nodes", "result" in r and a.get("nodeIDs") == [node]
          and r["result"].get("undoGroupOpened") is False, str(r))
    g = b.ok("selection.get")
    check("selection.get == [box]", g.get("nodeIDs") == [node] and g.get("mode") in ("objects", "meshes", "groups"), str(g))
    if node2 is not None:
        r = b.mut("selection.set", {"op": "add", "nodeIDs": [node2]})
        a = (r.get("result") or {}).get("applied") or {}
        check("selection.set add", sorted(a.get("nodeIDs", [])) == sorted([node, node2]), str(r))
        r = b.mut("selection.set", {"op": "remove", "nodeIDs": [node]})
        a = (r.get("result") or {}).get("applied") or {}
        check("selection.set remove", a.get("nodeIDs") == [node2], str(r))
    r = b.mut("selection.set", {"op": "replace", "faces": [{"nodeID": node, "faces": [0, 2]}]})
    a = (r.get("result") or {}).get("applied") or {}
    check("selection.set replace faces", a.get("mode") == "faces"
          and a.get("faces") == [{"nodeID": node, "faces": [0, 2]}], str(r))
    g = b.ok("selection.get")
    check("selection.get faces", g.get("mode") == "faces" and g.get("faces") == [{"nodeID": node, "faces": [0, 2]}], str(g))
    try:
        rr = b.ok("view.render", {"name": "unblock_rows_faces_%s" % RUN})
        print("     render (face selection, for a look):", rr.get("path") or rr)
    except RuntimeError as e:
        print("     render skipped:", e)
    r = b.mut("selection.set", {"op": "add", "faces": [{"nodeID": node, "faces": [4]}]})
    a = (r.get("result") or {}).get("applied") or {}
    check("selection.set add face", a.get("faces") == [{"nodeID": node, "faces": [0, 2, 4]}], str(r))
    before = b.ok("selection.get")
    r = b.mut("selection.set", {"op": "replace", "nodeIDs": [2147480000]})
    check("selection.set unknown nodeID refused", err_code(r) == -32602, str(r))
    check("selection unchanged after a refusal", b.ok("selection.get") == before)
    r = b.mut("selection.set", {"op": "clear"})
    a = (r.get("result") or {}).get("applied") or {}
    check("selection.set clear", "result" in r and a.get("count") == 0, str(r))
    s1 = state(b)
    check("selection: undoDepth/topSerial/dirty unchanged", s0 == s1, "%s -> %s" % (s0, s1))


def sec_jobs(b, container, job_wait, full_map):
    lst = b.ok("job.status", {})
    check("job.status {} lists", isinstance(lst.get("jobs"), list), str(lst))
    r = b.call("job.status", {"jobId": "j999999-zz"})
    check("job.status unknown jobId -32602", err_code(r) == -32602, str(r))
    ref = "ut-%s" % RUN
    r = b.call("job.start", {"kind": "resourcecompiler", "input": SNDEVTS, "game": True,
                             "flags": ["-nop4"], "clientRef": ref})
    if check("job.start soundevents", "result" in r, str(r)):
        jid = r["result"]["jobId"]
        st = wait_job(b, jid, ("ok", "failed", "timeout", "cancelled"), job_wait)
        check("job soundevents ok", st.get("state") == "ok" and st.get("summary"), json.dumps(st)[:600])
        check("job.status echoes clientRef + input", st.get("clientRef") == ref and st.get("input") == SNDEVTS, str(st))
        print("     soundevents job: %s summary=%s exit=%s log=%s" % (
            st.get("state"), st.get("summary"), st.get("exitCode"), (st.get("log") or {}).get("relative")))
    r = b.call("job.start", {"kind": "resourcecompiler", "input": MAP_INPUT, "flags": MAP_FLAGS})
    if check("job.start map", "result" in r, str(r)):
        jid = r["result"]["jobId"]
        r2 = b.call("job.start", {"kind": "resourcecompiler", "input": SNDEVTS, "game": True})
        blocked = ((r2.get("error") or {}).get("data") or {}).get("blockedOn", "")
        check("second start refused -32017 running/starting", err_code(r2) == -32017
              and ("running" in blocked or "starting" in blocked), str(r2))
        cs = b.ok("control.status")
        mine = [j for j in cs.get("jobs", []) if j.get("id") == jid]
        check("control.status lists the job with kind", mine and mine[0].get("kind") == "resourcecompiler", str(cs.get("jobs")))
        if full_map:
            st = wait_job(b, jid, ("ok", "failed", "timeout", "cancelled"), job_wait)
            check("job map compile ok", st.get("state") == "ok", json.dumps(st)[:800])
        else:
            c = b.ok("job.cancel", {"jobId": jid})
            check("job.cancel -> cancelled", c.get("cancelled") is True and c.get("state") == "cancelled", str(c))
            end = time.monotonic() + 20
            left = container_procs(container)
            while left and time.monotonic() < end:
                time.sleep(0.5)  # polling the container's process table, bounded
                left = container_procs(container)
            check("no compiler process left after cancel", not left, left)
            c2 = b.ok("job.cancel", {"jobId": jid})
            check("job.cancel finished -> cancelled:false", c2.get("cancelled") is False, str(c2))


def cleanup(b, labels):
    try:
        b.mut("selection.set", {"op": "clear"})
    except Exception as e:  # noqa: BLE001 - best effort
        print("cleanup: selection clear failed:", e)
    popped = 0
    for _ in labels:
        r = b.mut("undo.undo", {})
        if "result" in r and r["result"].get("undone"):
            popped += 1
    print("cleanup: popped %d of %d own groups" % (popped, len(labels)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true")
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--container", default="cs2tools")
    ap.add_argument("--full-map", action="store_true")
    ap.add_argument("--job-wait", type=int, default=600)
    ap.add_argument("--only", choices=["version", "mesh", "selection", "jobs"], action="append")
    args = ap.parse_args()
    if args.static:
        run_static()
    else:
        b = Bridge(args.prefix)
        only = set(args.only or ["version", "mesh", "selection", "jobs"])
        labels = []
        try:
            if "version" in only:
                sec_version(b)
            node = node2 = None
            if only & {"mesh", "selection"}:
                node, node2 = sec_mesh(b, labels)
            if "selection" in only and node is not None:
                sec_selection(b, node, node2, HERE)
            if "jobs" in only:
                sec_jobs(b, args.container, args.job_wait, args.full_map)
        finally:
            cleanup(b, labels)
    bad = [n for n, ok in CHECKS if not ok]
    print("%d checks, %d failed" % (len(CHECKS), len(bad)))
    print("RESULT:", "PASS" if not bad else "FAIL")
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
