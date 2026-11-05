#!/usr/bin/env python3
"""test_metamod_coexist.py - the bridge and a second Metamod plugin on one KHook target, live (KHOOK-D2).

The fixture (`crates/mm-testplugin`, built by `bridge/build-testplugin.sh`) hooks
`ISource2ServerConfig::AllowSaveRestore` with KHook at its `Load`; the bridge hooks the same
virtual when `game.saveRestore` is enabled. Both run on every call, and engine2's `CanSaveGame`
calls it on every `save`, so a `save` console command is one observed call. The answer is
visible in the console without reading either hook: "game does not support save/restore." means
the folded value was 0; anything past that gate (CS2 then refuses with "savegame not allowed in
multiplayer games", a separate gameinfo gate) means it was 1.

One -tools -testscript session per fixture mode (`CS2MCP_TESTPLUGIN_MODE` passive | supersede0 |
supersede1, passed through by `bridge/runtime.sh`). The fixture is NOT in a vdf: the session's
script `meta load`s it after the bridge armed, so both hook orders happen in one session:

  s1  bridge only                      (baseline: allowed)
  --  meta load the fixture             -> hooks (bridge, fixture)
  s2  save
  --  bridge disarm + re-arm            -> hooks (fixture, bridge)
  s3  save
  --  bridge disarm                     -> the fixture alone
  s4  save
  --  meta unload the fixture

Expected per mode (the bridge supersedes 1 while armed; DESIGN §13). KHook runs the NEWEST
hook's pre first, and of two equal-rank `Supersede`s the first saved value wins
(`detour.cpp:381-383`), so the newer hook's value wins:
  passive:    s2 s3 allowed, conflicts 0; s4 refused (the original's 0); fixture post sees 1,1,0.
  supersede1: s2 s3 allowed; s2 counts a conflict (two value changers on one call - the bridge
              counts changers, not disagreement); s4 allowed (the fixture's 1).
  supersede0: s2 refused (the newer fixture's 0 wins) and counted + logged as a conflict
              (`khook conflict: ... ahead`); s3 allowed (the newer bridge's 1 wins); s4 refused.
  Known limit, both supersede modes, s3: when the bridge's value wins, the losing changer's
  `Supersede` was discarded by KHook before the bridge's post runs, and nothing KHook exposes
  tells it apart from "no other changer" - s3 counts NO conflict. The fixture's own post sees
  the final value (the evidence a plugin has). Asserted, so a KHook change shows up here.
Plus: the game survives every step, both unloads, and the bridge's own log has no panic line.

Usage: bridge/test_metamod_coexist.py [--modes passive,supersede0,supersede1] [--keep]
Needs: target/testplugin/win64/mm_testplugin.dll (bridge/build-testplugin.sh), the bridge
installed (bridge/install-rs.sh; runtime.sh up runs ensure), no other cs2tools container.
Exit 0 = every check passed, 1 = a failed check, 2 = NOT RUN (setup failed).
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import PROJECT_ROOT  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
CS2DOCKER = PROJECT_ROOT
APP = os.path.join(CS2DOCKER, "workshop", "run", "app")
CONSOLE = os.path.join(APP, "game", "csgo", "console.log")
BRIDGE_LOG = os.path.join(APP, "mcp_bridge_rs.log")
TP_LOG = os.path.join(APP, "mm_testplugin.log")
TP_SRC = os.path.join(REPO, "target", "testplugin", "win64", "mm_testplugin.dll")
TP_DIR = os.path.join(APP, "game", "csgo", "addons", "mm_testplugin")
TP_REL = "addons/mm_testplugin/mm_testplugin.dll"   # meta load path, relative to csgo/
SCRIPT_NAME = "khook_coexist"
SCRIPT = os.path.join(APP, "vtest", SCRIPT_NAME + ".txt")
DISC = os.path.join(APP, "compatdata", "730", "pfx", "drive_c", "users", "steamuser", "AppData",
                    "Local", "cs2mcpbridge", "session.json")
PODMAN_ENV = dict(os.environ, XDG_RUNTIME_DIR=os.path.join(CS2DOCKER, ".podman", "xdg"))

ALLOWED_PAST = "savegame not allowed in multiplayer games"
REFUSED = "game does not support save/restore."

SCRIPT_TEXT = f"""// {SCRIPT_NAME}: bridge/test_metamod_coexist.py's session (generated; safe to delete).
testscript_debug 1
sv_cheats 1
r_always_render_all_windows 1
map de_dust2
Test_WaitForCheckPoint FinishedMapLoad once
Test_Run mark VTEST_MAP_LOADED
Test_Run setup_player
Test_Run mark VTEST_PARKED
Test_WaitForCheckPoint c1
save khook_co_s1
Test_Run mark C1_DONE
Test_WaitForCheckPoint c2
meta load {TP_REL}
Test_Run mark C2_DONE
Test_WaitForCheckPoint c3
save khook_co_s2
Test_Run mark C3_DONE
Test_WaitForCheckPoint c4
save khook_co_s3
Test_Run mark C4_DONE
Test_WaitForCheckPoint c5
save khook_co_s4
Test_Run mark C5_DONE
Test_WaitForCheckPoint c6
meta unload {TP_REL}
Test_Run mark C6_DONE
Test_WaitForCheckPoint vtest_release
quit
Test_Wait 30
Test_ExitProcess 3
"""

CHECKS = {}


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""), flush=True)
    return bool(ok)


def size(path):
    try:
        return os.path.getsize(path)
    except OSError:
        return 0


def read_from(path, off):
    try:
        with open(path, "rb") as f:
            f.seek(off)
            return f.read().decode("utf-8", "replace")
    except OSError:
        return ""


def wait_for(path, off, needle, deadline_s):
    """bounded poll on a log line (a real signal); None on the deadline"""
    end = time.time() + deadline_s
    while time.time() < end:
        text = read_from(path, off)
        if needle in text:
            return text
        if not container_running():
            return None
        time.sleep(0.25)
    return None


def container_running():
    out = subprocess.run(["podman", "ps", "--filter", "name=^cs2tools$", "--format", "{{.Names}}"],
                         env=PODMAN_ENV, capture_output=True, text=True).stdout
    return "cs2tools" in out


class Bridge:
    def __init__(self):
        disc = json.load(open(DISC))
        self.s = socket.create_connection((disc["host"], disc["port"]), timeout=60)
        self.f = self.s.makefile("r")
        self.n = 0
        self.call("control.hello", {"protocol": 1, "token": disc["token"]})

    def call(self, method, params=None):
        self.n += 1
        self.s.sendall((json.dumps({"jsonrpc": "2.0", "id": "co%d" % self.n, "method": method,
                                    "params": params or {}}) + "\n").encode())
        line = self.f.readline()
        if not line:
            raise RuntimeError(f"bridge connection closed during {method}")
        return json.loads(line)

    def close(self):
        self.s.close()


def runtime(*args, extra_env=None):
    env = dict(os.environ, **(extra_env or {}))
    return subprocess.run([os.path.join(HERE, "runtime.sh"), *args], env=env, capture_output=True,
                          text=True, timeout=900)


def one_save(br, checkpoint, done_mark, label):
    """release `checkpoint`, wait for `done_mark`, return (allowed?, console text)"""
    off = size(CONSOLE)
    r = br.call("test.checkpoint", {"name": checkpoint})
    if "error" in r:
        return None, f"checkpoint error {r['error']}"
    text = wait_for(CONSOLE, off, f"CheckPoint -> '{done_mark}'", 60)
    if text is None:
        return None, "no checkpoint mark (game gone?)"
    if REFUSED in text:
        return False, text
    if ALLOWED_PAST in text:
        return True, text
    return None, f"{label}: neither save answer in the console"


def run_mode(mode, keep):
    print(f"\n=== mode {mode}", flush=True)
    tp_off = size(TP_LOG)
    up = runtime("up", extra_env={"CS2MCP_TESTPLUGIN_MODE": mode,
                                  "EXTRA_ARGS": f"-testscript /app/vtest/{SCRIPT_NAME}.txt"})
    if up.returncode != 0:
        print(up.stdout[-2000:], up.stderr[-2000:])
        return "NOT RUN"
    if wait_for(CONSOLE, 0, "CheckPoint -> 'VTEST_PARKED'", 300) is None:
        print("NOT RUN: the session never parked")
        return "NOT RUN"
    br_off = size(BRIDGE_LOG)
    br = Bridge()
    p = f"{mode}:"
    try:
        r = br.call("game.saveRestore", {"enable": True})
        step(p + "arm", r.get("result", {}).get("enabled") is True, json.dumps(r)[:200])
        a1, _ = one_save(br, "c1", "C1_DONE", "s1")
        step(p + "s1_bridge_only_allowed", a1 is True, f"allowed={a1}")

        off = size(CONSOLE)
        br.call("test.checkpoint", {"name": "c2"})
        text = wait_for(CONSOLE, off, "CheckPoint -> 'C2_DONE'", 60) or ""
        tp_text = read_from(TP_LOG, tp_off)
        step(p + "fixture_loaded", f"Load(mode={mode})" in tp_text and "hooked AllowSaveRestore" in tp_text,
             tp_text.strip().splitlines()[-1] if tp_text.strip() else "no fixture log")
        plugins = br.call("metamod.plugins").get("result", {}).get("plugins", [])
        step(p + "plugins_table_has_both", len(plugins) == 2 and
             any(str(x.get("file", "")).endswith("mm_testplugin.dll") for x in plugins),
             f"files={[x.get('file') for x in plugins]}")

        c0 = br.call("game.saveRestore", {"enable": True}).get("result", {}).get("conflicts", 0)
        a2, _ = one_save(br, "c3", "C3_DONE", "s2")
        c2 = br.call("game.saveRestore", {"enable": True}).get("result", {}).get("conflicts", 0)
        print(f"  s2 (bridge, fixture): allowed={a2} conflicts {c0}->{c2}")

        br.call("game.saveRestore", {"enable": False})
        r = br.call("game.saveRestore", {"enable": True})
        c2b = r.get("result", {}).get("conflicts", 0)
        a3, _ = one_save(br, "c4", "C4_DONE", "s3")
        c3 = br.call("game.saveRestore", {"enable": True}).get("result", {}).get("conflicts", 0)
        print(f"  s3 (fixture, bridge): allowed={a3} conflicts {c2b}->{c3}")

        br.call("game.saveRestore", {"enable": False})
        a4, _ = one_save(br, "c5", "C5_DONE", "s4")
        print(f"  s4 (fixture alone): allowed={a4}")

        off = size(CONSOLE)
        br.call("test.checkpoint", {"name": "c6"})
        wait_for(CONSOLE, off, "CheckPoint -> 'C6_DONE'", 60)
        tp_text = read_from(TP_LOG, tp_off)
        step(p + "fixture_unloaded", "Unload: removed" in tp_text,
             [ln for ln in tp_text.splitlines() if "Unload" in ln][-1:] or "no Unload line")
        posts = [ln for ln in tp_text.splitlines() if "AllowSaveRestore pre#" in ln]
        print("  fixture calls:", [ln.split("] ", 1)[-1] for ln in posts])
        br_text = read_from(BRIDGE_LOG, br_off)
        conflict_lines = [ln for ln in br_text.splitlines() if "khook conflict:" in ln]
        print("  bridge conflict lines:", conflict_lines[:4])

        # `conflicts` is per arm (a re-arm starts at 0): one delta per save.
        d2, d3 = c2 - c0, c3 - c2b
        if mode == "passive":
            step(p + "s2_allowed", a2 is True, f"{a2}")
            step(p + "s3_allowed", a3 is True, f"{a3}")
            step(p + "s4_refused", a4 is False, f"{a4}")
            step(p + "no_conflicts", d2 == 0 and d3 == 0 and not conflict_lines,
                 f"s2 +{d2}, s3 +{d3}, {len(conflict_lines)} line(s)")
        else:
            want = mode == "supersede1"
            step(p + "s2_newer_fixture_wins", a2 is want, f"allowed={a2}")
            step(p + "s2_conflict_counted", d2 >= 1, f"s2 +{d2}")
            step(p + "s2_conflict_logged", any(ln.endswith(" ahead") for ln in conflict_lines),
                 f"{conflict_lines[:2]}")
            step(p + "s3_newer_bridge_wins", a3 is True, f"allowed={a3}")
            step(p + "s3_known_limit_no_conflict", d3 == 0, f"s3 +{d3} (see the docstring)")
            step(p + "s4_fixture_alone", a4 is want, f"allowed={a4}")
        step(p + "fixture_saw_calls", len(posts) >= 3, f"{len(posts)} call line(s)")
        step(p + "bridge_alive", "result" in br.call("control.ping"), "")
        step(p + "no_panic", "panic" not in br_text.lower(), "")
        br.call("test.checkpoint", {"name": "vtest_release"})
    finally:
        br.close()
        if not keep:
            runtime("down")
    return "RAN"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--modes", default="passive,supersede0,supersede1")
    ap.add_argument("--keep", action="store_true", help="leave the last session running")
    args = ap.parse_args()
    if not os.path.exists(TP_SRC):
        print(f"NOT RUN: {TP_SRC} missing (bridge/build-testplugin.sh)")
        return 2
    if container_running():
        print("NOT RUN: a cs2tools container is already running")
        return 2
    os.makedirs(TP_DIR, exist_ok=True)
    shutil.copy2(TP_SRC, os.path.join(TP_DIR, "mm_testplugin.dll"))
    with open(SCRIPT, "w") as f:
        f.write(SCRIPT_TEXT)
    modes = [m for m in args.modes.split(",") if m]
    try:
        for i, mode in enumerate(modes):
            if run_mode(mode, args.keep and i == len(modes) - 1) == "NOT RUN":
                return 2
    finally:
        os.remove(SCRIPT)
        shutil.rmtree(TP_DIR, ignore_errors=True) if not args.keep else None
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
