#!/usr/bin/env python3
"""test_metamod_plugins.py - the `metamod.plugins` table, live (KHOOK-B4).

`metamod.plugins` answers from the bridge's snapshot of Metamod's ISmmPluginManager (rebuilt
on the main thread at Load/AllPluginsLoaded/plugin events - the row is `needs == 0`, answered
on the IPC lane, so it works when nothing ticks). This pass checks what only a live session
can prove:

- the method is advertised (`control.capabilities` names `metamod.plugins`);
- the answer carries the plugins: OUR row (`"self":true`, `"status":"running"`, file ending
  `cs2_bridge.dll` / `libcs2_bridge.so` - the one image the tools Metamod vdf points at),
  `generation >= 1` and a `rebuiltAtMs`;
- with env `CS2MCP_TESTPLUGIN=1` (the orchestrator loaded the test Metamod plugin first):
  the `mm_testplugin` row too (`"self":false`, `"status":"running"`, file ending
  `mm_testplugin.dll` / `mm_testplugin.so`) - the point of the table is that the bridge is
  ONE plugin among many, and the rows say so.

`--static` (offline; no bridge, no container): reads the Rust bridge's committed sources and
asserts what is still a source fact - `BRIDGE_BUILD` in crates/cs2-bridge/src/lib.rs (m3.N or
later) and `metamod.plugins`'s Rust-only row in registry.rs (`RUST_METHODS`: needs 0,
implemented, advertised, not blocked; the frozen C table's 84 rows cannot carry it). It never
compiles or runs anything. The behaviour itself (snapshot building, gaps, self flag, the
never-off-the-main-thread rule, the two blockedOn answers) is the host test
`crates/cs2-bridge/tests/metamod_plugin.rs`'s, which runs without a game.

Usage: bridge/test_metamod_plugins.py [--static] [--prefix <wine pfx>] [--wait S]
Exit 0 when every check passed; exit 1 on a failed check; exit 2 when the run did NOT RUN
(bridge unreachable - CS2MCP_TESTPLUGIN=1 with a missing mm_testplugin row is a FAIL, not a
NOT RUN: the plugin was supposed to be loaded, and a table that lost a plugin is exactly the
bug this test exists for).
"""
import argparse
import json
import os
import socket
import sys
import time

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BRIDGE_DIR)
from bridge_build import (BUILD_SHAPE, expected_build, methods,  # noqa: E402
                          row_problems)
from cs2mcp_root import PFX_DIR  # noqa: E402

BUILD = expected_build()                   # the Rust bridge's single BRIDGE_BUILD
OUR_FILES = ("cs2_bridge.dll", "libcs2_bridge.so")
TESTPLUGIN_FILES = ("mm_testplugin.dll", "mm_testplugin.so")

CHECKS = {}


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""))
    return bool(ok)


def poll_until(fn, deadline_s):
    """bounded poll on a real signal (fn -> truthy); no fixed sleep beyond the tick"""
    end = time.time() + deadline_s
    while True:
        v = fn()
        if v:
            return v
        if time.time() >= end:
            return None
        time.sleep(0.4)


# ---- offline (--static) ------------------------------------------------------------------------

def static_report() -> int:
    """the Rust bridge contract this pass relies on: the build string and the Rust-only row."""
    try:
        build = expected_build()
        rows = methods()
    except (OSError, RuntimeError) as ex:
        print(f"FAIL: cannot read the Rust bridge sources: {ex}")
        return 1

    missing = []
    if BUILD_SHAPE.match(build) is None:
        missing.append(f"BRIDGE_BUILD {build!r} (expected m3.N or later)")

    # The row is a Rust-only registry row: needs 0 (the IPC lane answers it, never the
    # main-thread queue), implemented, advertised, no blockedOn (the two metamod
    # prerequisites are the row's own -32017 answers, not a table blockedOn).
    missing += [f"metamod.plugins: {why}"
                for why in row_problems(rows, "metamod.plugins", needs="0")]

    step("static_metamod_plugins_contract", not missing, f"missing={missing}" if missing else "")
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# ---- live --------------------------------------------------------------------------------------

def find_row(plugins, suffixes):
    """the row whose file ends with one of `suffixes`, or None."""
    for row in plugins:
        if str(row.get("file", "")).endswith(tuple(suffixes)):
            return row
    return None


def live_report(args) -> int:
    disc_path = os.environ.get("CS2MCP_SESSION")
    if not disc_path:
        prefix = args.prefix or os.environ.get("WINEPREFIX", "")
        if not prefix:
            print("NOT RUN: no discovery file set; pass --prefix (the wine pfx) or set "
                  "CS2MCP_SESSION")
            return 2
        disc_path = os.path.join(prefix, "drive_c", "users", "steamuser", "AppData", "Local",
                                 "cs2mcpbridge", "session.json")
    if args.wait > 0 and not poll_until(lambda: os.path.exists(disc_path), args.wait):
        print(f"NOT RUN: the bridge session never came up (discovery file not found: {disc_path})")
        return 2
    try:
        disc = json.load(open(disc_path))
        s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
    except Exception as ex:  # noqa: BLE001 - the bridge session is not up
        print(f"NOT RUN: cannot reach the bridge session ({ex}); discovery={disc_path}")
        return 2
    f = s.makefile("r")
    n = [0]

    def call(method, params=None):
        n[0] += 1
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": "mp%d" % n[0], "method": method,
                               "params": params or {}}) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    call("control.hello", {"protocol": 1, "token": disc["token"]})

    # The row is advertised, like every other served method.
    caps = call("control.capabilities")
    advertised = caps.get("result", {}).get("methods", [])
    step("capabilities_advertise_metamod_plugins", "metamod.plugins" in advertised,
         f"{len(advertised)} methods advertised")

    # The table itself. With nothing ticking (no queue, no drain) the answer must still come:
    # the lane reads the snapshot.
    answer = call("metamod.plugins")
    if "error" in answer:
        step("metamod_plugins_answers", False, f"error: {answer['error']}")
        ok = all(CHECKS.values())
        print(f"\nchecks: {CHECKS}")
        print("RESULT:", "PASS" if ok else "FAIL")
        return 0 if ok else 1
    result = answer.get("result", {})
    plugins = result.get("plugins")
    if not isinstance(plugins, list):
        step("metamod_plugins_answers", False, f"no plugins array: {answer}")
        ok = all(CHECKS.values())
        print(f"\nchecks: {CHECKS}")
        print("RESULT:", "PASS" if ok else "FAIL")
        return 0 if ok else 1
    step("metamod_plugins_answers", True, f"{len(plugins)} plugin(s)")

    generation = result.get("generation")
    step("generation_present", isinstance(generation, int) and generation >= 1,
         f"generation={generation!r}")
    step("rebuilt_at_present", isinstance(result.get("rebuiltAtMs"), int),
         f"rebuiltAtMs={result.get('rebuiltAtMs')!r}")

    ours = find_row(plugins, OUR_FILES)
    step("our_row_present", ours is not None,
         f"files={[p.get('file') for p in plugins]}")
    if ours is not None:
        step("our_row_is_self", ours.get("self") is True, f"row={ours}")
        step("our_row_is_running", ours.get("status") == "running", f"row={ours}")
        step("our_row_id_positive", isinstance(ours.get("id"), int) and ours["id"] >= 1,
             f"id={ours.get('id')!r}")

    if os.environ.get("CS2MCP_TESTPLUGIN") == "1":
        test_row = find_row(plugins, TESTPLUGIN_FILES)
        step("testplugin_row_present", test_row is not None,
             f"CS2MCP_TESTPLUGIN=1, files={[p.get('file') for p in plugins]}")
        if test_row is not None:
            step("testplugin_row_is_running", test_row.get("status") == "running",
                 f"row={test_row}")
            step("testplugin_row_is_not_self", test_row.get("self") is False,
                 f"row={test_row}")
    else:
        print("SKIP: mm_testplugin row not required (set CS2MCP_TESTPLUGIN=1 to require it)")

    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--static", action="store_true",
                        help="offline: check the Rust bridge's committed sources only")
    # The same default/semantics as test_sound_params.py: the wine pfx whose
    # `drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json` names the live
    # bridge session, used only when CS2MCP_SESSION is not set.
    parser.add_argument("--prefix", default=PFX_DIR,
                        help="wine pfx: where the bridge session's discovery file is looked "
                             "up when CS2MCP_SESSION is not set")
    parser.add_argument("--wait", type=int, default=0,
                        help="seconds to wait for the session's discovery file")
    args = parser.parse_args()
    if args.static:
        return static_report()
    return live_report(args)


if __name__ == "__main__":
    sys.exit(main())
