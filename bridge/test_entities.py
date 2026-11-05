#!/usr/bin/env python3
"""test_entities.py - entity pass: entity.get, entity.find and setKeyValues by nodeID.

The entity semantics landed in the C bridge's m2.15 pass, and m2.18 moved the writes onto
Hammer's own SetKeyValue (which closed the value-undo gap and gave entity.get its live
connections); those rows are served by the Rust bridge now, and the live version check below
compares against the Rust bridge's `BRIDGE_BUILD`.

  --static (offline): no bridge, no container, no discovery file. Reads the Rust bridge's
  committed sources and asserts what is still a source fact: `BRIDGE_BUILD`
  (crates/cs2-bridge/src/lib.rs, m3.N or later), the entity.get / entity.find rows in
  registry.rs (implemented on NEED_HAMMER, advertised, not blocked), the undo.redo row, the
  three prop rows (node.setTransform, entity.addConnections, entity.removeConnections), the
  generated pins those rows call, and that every expected row with a body is registered in its
  ops HANDLERS table. The C source-shape checks (dispatch arms, the nodeID branch, the JSON
  scanners and coercion, the doc tail, protocol-diff-m2.15.md and the no-new-hex diff) are
  retired; bridge/protocol-diff-m5-tests.md lists each one and the Rust unit test that covers
  it. A missing/renamed file is a FAIL, never a skip.
  Run `python3 -I bridge/test_entities.py --static`.

  Live mode (default, the orchestrator runs it; needs a running tools session with
  `runtime.sh open maps/defuse_poc.vmap` - the addon-relative asset name; `mcp_test/maps/...`
  raises Hammer's "Unrecognized Asset" modal and stalls the main thread): plan section 5
  steps 1-4 over raw JSON-RPC
  through test_client.py - the version/capabilities, V1/V2 (entity.get{nodeID:30} equals the
  `vmap_dmx.py get 30 --json` oracle read), the func_buyzone query and its refusals (-32602 on a
  mesh nodeID, -32022 on a missing one, -32602 with both handle and nodeID), and V3/V4
  (setKeyValues{nodeID:30} -> previous/valueUndo -> map.save -> the file holds the coerced
  strings -> undo.undo pops the group but does NOT revert the values, the recorded gap -> restore
  from `previous` and save). The run-unique `vscripts` value and the restore keep defuse_poc.vmap
  where this run found it.

Usage: bridge/test_entities.py [--static] [--prefix <wine pfx>] [--project-root <project root>]
       [--map <host .vmap>] [--wait N]
Exit 0 when every step passes.
"""
import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge_build import (BUILD_SHAPE, expected_build, handler_names, methods,  # noqa: E402
                          pin_names, row_problems)
from test_client import discovery_path, connect, hello, rpc  # noqa: E402
from cs2mcp_root import PFX_DIR, PROJECT_ROOT  # noqa: E402

DEFAULT_PFX = PFX_DIR
DEFAULT_PROJECT_ROOT = PROJECT_ROOT
DEFAULT_MAP_REL = os.path.join("content", "csgo_addons", "mcp_test", "maps", "defuse_poc.vmap")

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
VMAP_CLI = os.path.join(BRIDGE_DIR, "vmapreader", "vmap_dmx.py")
BUILD = expected_build()                     # the Rust bridge's single BRIDGE_BUILD

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
V3_NODE = 30                                 # info_map_parameters in defuse_poc.vmap (reader: bombradius "500")
V3_BUYZONES = [67, 70]                       # the two func_buyzone nodeIDs the reader reports

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


# ---- offline (--static) expectations: the Rust bridge contract ---------------------------------

def static_report() -> int:
    """offline: the Rust bridge contract this pass relies on (build string, method rows,
    handler registration, pins). Reads crates/cs2-bridge; a missing file is a FAIL."""
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

    # §3.5: both rows implemented (NEED_HAMMER, implemented, advertised, no blockedOn) and
    # served by an ops HANDLERS table
    want = ("entity.get", "entity.find")
    missing = {name: row_problems(rows, name, "NEED_HAMMER") for name in want}
    missing = {name: why for name, why in missing.items() if why}
    step("static_rows_implemented", not missing, f"missing={missing}")
    step("static_rows_served", set(want) <= served,
         f"missing={sorted(set(want) - served)}")

    # m2.17: undo.redo is implemented on the same gate and served
    step("static_undo_redo",
         not row_problems(rows, "undo.redo", "NEED_HAMMER") and "undo.redo" in served,
         f"row={row_problems(rows, 'undo.redo', 'NEED_HAMMER')} served={'undo.redo' in served}")

    # m2.18 (the props pass): the three rows behind node.setTransform and entity I/O, served,
    # and the pins they call in the generated pins.rs
    props_rows = ("node.setTransform", "entity.addConnections", "entity.removeConnections")
    missing = {name: row_problems(rows, name, "NEED_HAMMER") for name in props_rows}
    missing = {name: why for name, why in missing.items() if why}
    if set(props_rows) - served:
        missing["served"] = f"missing={sorted(set(props_rows) - served)}"
    want_pins = ("HFN_GET_REDO_HISTORY", "HFN_SET_KEY_VALUE", "HFN_ADD_CONNECTION",
                 "HFN_REMOVE_CONNECTION", "HFN_KV3_INIT", "HFN_KV3_FREE")
    if set(want_pins) - pins:
        missing["pins"] = f"missing={sorted(set(want_pins) - pins)}"
    step("static_props_pass", not missing, f"missing={missing}")

    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# ---- live (default) ---------------------------------------------------------------------------

def oracle_get(map_path, selector):
    """the plan's file oracle: python3 -I bridge/vmapreader/vmap_dmx.py FILE get SELECTOR --json"""
    p = subprocess.run([sys.executable, "-I", VMAP_CLI, map_path, "get", str(selector), "--json"],
                       capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"vmap_dmx.py get {selector} failed rc={p.returncode}: {p.stderr.strip()}")
    return json.loads(p.stdout)


def oracle_find_class(map_path, cls):
    p = subprocess.run([sys.executable, "-I", VMAP_CLI, map_path, "find", cls, "--json"],
                       capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"vmap_dmx.py find {cls} failed rc={p.returncode}: {p.stderr.strip()}")
    return {e["nodeID"]: e for e in json.loads(p.stdout)}


def live_report(args) -> int:
    map_path = args.map or os.path.join(args.project_root, "workshop", "run", "app", DEFAULT_MAP_REL)
    if not os.path.exists(map_path):
        print(f"FAIL: map not found: {map_path}")
        return 2
    path = discovery_path(args.prefix)
    if not poll_until(lambda: os.path.exists(path), args.wait):
        print(f"FAIL: discovery file not found: {path}")
        return 2
    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']} run={RUN}")
    s = connect(disc)
    hello(s, disc["token"])

    def call(rid, method, params):
        return rpc(s, {"jsonrpc": "2.0", "id": rid, "method": method, "params": params})

    def err(r):
        return (r.get("error") or {})

    # 1. version + capabilities
    r = call("v1", "control.version", {})
    step("version", (r.get("result") or {}).get("bridgeVersion") == BUILD,
         f"bridgeVersion={(r.get('result') or {}).get('bridgeVersion')!r}")
    r = call("c1", "control.capabilities", {})
    methods = set((r.get("result") or {}).get("methods") or [])
    step("methods_advertised", {"entity.get", "entity.find"} <= methods,
         f"missing={sorted({'entity.get', 'entity.find'} - methods)}")

    # the map under test must be the open one: the file oracle and the live tree then describe
    # the same document (V1/V2/V3 are exactly that equivalence)
    r = call("d0", "doc.active", {})
    doc = (r.get("result") or {}).get("doc") or {}
    doc_path = (doc.get("path") or "").replace("\\", "/")
    step("active_doc", doc_path.endswith(os.path.basename(map_path)),
         f"active={doc_path!r} expected *{os.path.basename(map_path)} (open it first)")
    if not CHECKS["active_doc"]:
        s.close()
        return 1
    expect = {"id": doc["id"], "epoch": doc["epoch"]}

    # 2. V1/V2: entity.get{nodeID:30} equals the reader's keyvalues
    oracle30 = oracle_get(map_path, V3_NODE)
    r = call("g30", "entity.get", {"nodeID": V3_NODE})
    res = r.get("result") or {}
    step("get_answered", "error" not in r, f"resp={r if 'error' in r else 'ok'}")
    step("get_classname", res.get("classname") == oracle30["classname"]
         and res.get("classname") == "info_map_parameters", f"classname={res.get('classname')!r}")
    step("get_bombradius", (res.get("keyValues") or {}).get("bombradius") == "500",
         f"bombradius={(res.get('keyValues') or {}).get('bombradius')!r}")
    step("get_unsupported_empty", res.get("unsupported") == [],
         f"unsupported={res.get('unsupported')}")
    live_kv = res.get("keyValues") or {}
    step("get_keyvalues_match_oracle", live_kv == oracle30["keyvalues"],
         f"diff={ {k: (live_kv.get(k), oracle30['keyvalues'].get(k)) for k in set(live_kv) | set(oracle30['keyvalues']) if live_kv.get(k) != oracle30['keyvalues'].get(k)} }")
    step("get_keyvalue_count", res.get("keyValueCount") == len(live_kv),
         f"keyValueCount={res.get('keyValueCount')} keys={len(live_kv)}")
    # m2.18: the live I/O vector, not the old hard-coded null - an entity with no outputs answers
    # [], and every entry (when there is one) has the .vmap reader's field names
    conns = res.get("connections")
    step("get_source", res.get("source") == "live" and isinstance(conns, list)
         and all(set(c) == {"outputName", "targetType", "targetName", "inputName", "overrideParam",
                            "delay", "timesToFire"} for c in conns),
         f"source={res.get('source')!r} connections={conns!r}")
    step("get_origin", res.get("origin") == oracle30["origin"],
         f"origin={res.get('origin')} oracle={oracle30['origin']}")

    # 3. entity.find + the refusals
    oracle_bz = oracle_find_class(map_path, "func_buyzone")
    r = call("f1", "entity.find", {"class": "func_buyzone"})
    res = r.get("result") or {}
    hits = res.get("entities") or []
    step("find_buyzone", sorted(e["nodeID"] for e in hits) == V3_BUYZONES
         and res.get("count") == len(V3_BUYZONES) and res.get("truncated") is False,
         f"nodeIDs={sorted(e['nodeID'] for e in hits)} count={res.get('count')}")
    step("find_buyzone_origins",
         {e["nodeID"]: e["origin"] for e in hits} == {k: v["origin"] for k, v in oracle_bz.items()},
         f"origins={ {e['nodeID']: e['origin'] for e in hits} }")
    step("find_source", res.get("source") == "live" and isinstance(res.get("visited"), int),
         f"source={res.get('source')!r} visited={res.get('visited')}")

    r = call("nl", "node.list", {"class": "mesh", "limit": 1})
    meshes = [n["nodeID"] for n in (r.get("result") or {}).get("nodes") or []]
    step("mesh_present", bool(meshes), "defuse_poc.vmap must have a mesh for the not-an-entity check")
    if meshes:
        r = call("gm", "entity.get", {"nodeID": meshes[0]})
        step("get_mesh_refused", err(r).get("code") == -32602
             and (err(r).get("data") or {}).get("reason") == "not an entity",
             f"resp={r}")
    r = call("gn", "entity.get", {"nodeID": 99999999})
    step("get_missing_refused", err(r).get("code") == -32022, f"resp={r}")

    both = call("sxb", "entity.setKeyValues",
                {"handle": "n1", "nodeID": V3_NODE, "values": {"bombradius": "1"}, "expectDoc": expect})
    step("set_both_handle_and_nodeid", err(both).get("code") == -32602, f"resp={both}")
    neither = call("sxn", "entity.setKeyValues", {"values": {"bombradius": "1"}, "expectDoc": expect})
    step("set_neither_handle_nor_nodeid", err(neither).get("code") == -32602, f"resp={neither}")

    # 4. V3/V4: setKeyValues by nodeID -> save -> the file holds the coerced strings -> undo gap
    values = {"bombradius": 501, "vscripts": f"x_{RUN}"}
    r = call("s1", "entity.setKeyValues", {"nodeID": V3_NODE, "values": values, "expectDoc": expect})
    res = r.get("result") or {}
    set_prev = res.get("previous") or {}
    step("set_applied", res.get("applied") == {"bombradius": True, "vscripts": True},
         f"applied={res.get('applied')} resp={r if 'error' in r else 'ok'}")
    step("set_previous", set_prev.get("bombradius") == "500", f"previous={set_prev}")
    step("set_value_undo", res.get("valueUndo") == "recorded",
         f"valueUndo={res.get('valueUndo')!r} (m2.18 records the node tree: the group reverts values)")

    r = call("sv", "map.save", {})
    step("saved", (r.get("result") or {}).get("saved") is True, f"resp={r}")

    def file_kv():
        try:
            return oracle_get(map_path, V3_NODE)["keyvalues"]
        except Exception:
            return None

    def file_shows(want):
        kv = file_kv()
        return kv if kv and kv.get("bombradius") == want else None

    kv = poll_until(lambda: file_shows("501"), 30.0)
    step("file_coerced_strings", bool(kv) and kv.get("bombradius") == "501"
         and kv.get("vscripts") == values["vscripts"],
         f"file bombradius={(kv or {}).get('bombradius')!r} vscripts={(kv or {}).get('vscripts')!r}")

    r = call("u1", "undo.undo", {"expectDoc": expect})
    res = r.get("result") or {}
    step("undo_group", res.get("undone") is True and res.get("name") == "MCP: set keyvalues",
         f"resp={r}")
    r = call("g30b", "entity.get", {"nodeID": V3_NODE})
    # m2.18 closes m2.15's recorded gap: the write RecordNodeTree'd the node, so the popped group
    # restores the values too (re-props.md Q1 [V live]). The check is the live value, read back.
    step("undo_values_reverted",
         ((r.get("result") or {}).get("keyValues") or {}).get("bombradius") == "500",
         "m2.18: undo.undo pops the group AND reverts the attribute values")

    # the file still holds the written value; re-apply `previous` (the m2.15 revert contract's
    # value) and save again so the map ends where this run found it
    restore = {k: v for k, v in set_prev.items() if v is not None}
    step("restore_has_previous", "bombradius" in restore, f"restore={restore}")
    r = call("s2", "entity.setKeyValues", {"nodeID": V3_NODE, "values": restore, "expectDoc": expect})
    step("restore_applied", "error" not in r
         and all((r.get("result") or {}).get("applied", {}).values()),
         f"resp={r}")
    r = call("sv2", "map.save", {})
    step("saved_again", (r.get("result") or {}).get("saved") is True, f"resp={r}")
    kv2 = poll_until(lambda: file_shows("500"), 30.0)
    step("file_restored", bool(kv2) and kv2.get("bombradius") == "500",
         f"file bombradius={(kv2 or {}).get('bombradius')!r}")

    s.close()
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true",
                    help="offline source expectations (the Rust build string, method rows, pins)")
    ap.add_argument("--base-ref", default=None,
                    help="ignored: the C-source diff check is retired "
                         "(see bridge/protocol-diff-m5-tests.md)")
    ap.add_argument("--prefix", default=DEFAULT_PFX)
    ap.add_argument("--project-root", default=DEFAULT_PROJECT_ROOT)
    ap.add_argument("--map", default=None, help="host path of the open .vmap (default defuse_poc.vmap)")
    ap.add_argument("--wait", type=float, default=0.0)
    args = ap.parse_args()
    if args.static:
        return static_report()
    return live_report(args)


if __name__ == "__main__":
    sys.exit(main())
