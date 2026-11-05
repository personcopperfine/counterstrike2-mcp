#!/usr/bin/env python3
"""game_state_probe.py - live check of the m2.9 game-state methods against a parked session.

Talks to the bridge directly (session.json -> loopback TCP, protocol 1), so it needs no MCP server:
  python3 -I game_state_probe.py [--discovery PATH] [--out DIR]
Expects a WineTools session parked in a map (worknotes/vtest/run_vtest.sh sandbox + load_bridge.sh).
Writes every reply to <out>/<method>.json and prints a PASS/FAIL table; nothing is asserted that the
docs do not promise (workflows/11-game-state.md step 4).
"""
import argparse
import json
import os
import socket
import sys
import time

# the project root: $CS2_MCP_PROJECT_ROOT ($CS2DOCKER_ROOT: deprecated alias), else this checkout
_ROOT = (os.environ.get("CS2_MCP_PROJECT_ROOT") or os.environ.get("CS2DOCKER_ROOT")
         or os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
DEF_DISC = os.path.join(_ROOT, "workshop/run/app/compatdata/730/pfx/drive_c/users/steamuser/"
                        "AppData/Local/cs2mcpbridge/session.json")


class Bridge:
    def __init__(self, disc_path):
        d = json.load(open(disc_path))
        self.s = socket.create_connection((d["host"], d["port"]), timeout=60)
        self.f = self.s.makefile("rb")
        self.n = 0
        r = self.rpc("control.hello", {"protocol": 1, "token": d["token"], "client": {"name": "probe", "version": "0"}})
        self.hello = r

    def rpc(self, method, params=None, deadline=20000):
        self.n += 1
        p = dict(params or {})
        p.setdefault("deadlineMs", deadline)
        self.s.sendall((json.dumps({"jsonrpc": "2.0", "id": "p%d" % self.n, "method": method, "params": p,
                                    "bridge": {"protocol": 1}}) + "\n").encode())
        while True:
            line = self.f.readline()
            if not line:
                raise RuntimeError("connection closed during %s" % method)
            obj = json.loads(line)
            if obj.get("id") == "p%d" % self.n:
                return obj


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--discovery", default=DEF_DISC)
    ap.add_argument("--out", default="/tmp/cs2mcp-live/probe")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    b = Bridge(a.discovery)
    results = []

    def save(name, obj):
        with open(os.path.join(a.out, name + ".json"), "w") as f:
            json.dump(obj, f, indent=1)

    def check(name, cond, detail=""):
        results.append((name, bool(cond), detail))
        print("%-44s %s %s" % (name, "PASS" if cond else "FAIL", detail))

    save("hello", b.hello)
    caps = b.rpc("control.capabilities")["result"]
    save("capabilities", caps)
    mods = caps.get("modules", {})
    check("bridge build m2.9", b.hello["result"].get("bridgeVersion") == "m2.9", b.hello["result"].get("bridgeVersion"))
    for m in ("schemasystem.dll", "soundsystem.dll", "client.dll", "engine2.dll"):
        check("module verified: " + m, mods.get(m, {}).get("state") == "verified", mods.get(m, {}).get("state"))
    methods = set(caps.get("methods", []))
    for m in ("game.local", "game.weapons", "game.radar", "game.selectWeapon", "sound.status", "sound.drain"):
        check("advertised: " + m, m in methods)

    # schema scope diagnostics (which module names resolve to a non-global scope)
    r = b.rpc("game.schemaProbe"); save("game.schemaProbe", r)
    for it in (r.get("result") or {}).get("results", []):
        print("  schemaProbe %-28s found=%s scope=%s" % (it.get("name"), it.get("found"), it.get("scopeName")))

    # game.local
    r = b.rpc("game.local"); save("game.local", r)
    loc = r.get("result") or {}
    check("game.local ok", "result" in r, json.dumps(r.get("error"))[:120])
    pl = loc.get("player") or {}
    check("game.local map/tick", bool(loc.get("map")) and isinstance(loc.get("tick"), int), "%s @ %s" % (loc.get("map"), loc.get("tick")))
    check("game.local origin", isinstance(pl.get("origin"), list), str(pl.get("origin")))
    check("game.local viewAngles", isinstance(loc.get("viewAngles"), list), str(loc.get("viewAngles")))
    check("game.local eye/velocity", isinstance(pl.get("eye"), list) and isinstance(pl.get("velocity"), list))
    check("game.local activeWeapon", isinstance(pl.get("activeWeapon"), dict) and pl["activeWeapon"].get("name"), str((pl.get("activeWeapon") or {}).get("name")))

    # cross-checks through the input hook: the angle override must show up in viewAngles, walking must
    # move the origin (console output such as getpos does not reach console.log in -tools sessions)
    b.rpc("input.set", {"angles": [0.0, 90.0, 0.0]}); time.sleep(0.3)
    loc2 = (b.rpc("game.local").get("result") or {})
    va = loc2.get("viewAngles") or [None, None, None]
    check("viewAngles follows input override (yaw 90)", isinstance(va[1], (int, float)) and abs(va[1] - 90.0) < 1.0, str(va))
    o0 = (loc2.get("player") or {}).get("origin")
    b.rpc("input.set", {"press": ["forward"]}); time.sleep(1.0); b.rpc("input.set", {"release": ["forward"]}); time.sleep(0.3)
    loc3 = (b.rpc("game.local").get("result") or {})
    o1 = (loc3.get("player") or {}).get("origin")
    moved = None
    if isinstance(o0, list) and isinstance(o1, list):
        moved = ((o1[0] - o0[0]) ** 2 + (o1[1] - o0[1]) ** 2) ** 0.5
    check("origin moves after +forward (> 20 u)", moved is not None and moved > 20.0, "moved=%s from %s to %s" % (moved, o0, o1))
    vel = (loc3.get("player") or {}).get("velocity")
    check("velocity present", isinstance(vel, list), str(vel))
    b.rpc("input.clear")
    save("game.local_after_move", loc3)

    # weapons
    r = b.rpc("game.weapons"); save("game.weapons", r)
    w = r.get("result") or {}
    check("game.weapons ok", "result" in r, json.dumps(r.get("error"))[:120])
    check("game.weapons count>0", (w.get("count") or 0) > 0, "count=%s names=%s" % (w.get("count"), [x.get("name") for x in w.get("weapons", [])]))
    active0 = (w.get("active") or {}).get("name")
    clip = (w.get("active") or {}).get("clip1")
    check("game.weapons active clip1 >= 0", isinstance(clip, int) and clip >= 0, "active=%s clip1=%s reserve=%s" % (active0, clip, (w.get("active") or {}).get("reserve")))

    # select weapon: slot 2 then slot 1 (or 3), expect the active name to change
    # the parked CT holds slot 2 (pistol) and slot 3 (knife): slot 3 -> knife, slot 2 -> pistol, weapon_knife -> knife, defIndex 32 -> pistol
    r = b.rpc("game.selectWeapon", {"slot": 3}); save("game.selectWeapon_slot3", r)
    check("game.selectWeapon slot3 queued", (r.get("result") or {}).get("queued") is True, json.dumps(r.get("error"))[:120])
    time.sleep(1.2)
    active3 = ((b.rpc("game.weapons").get("result") or {}).get("active") or {}).get("name")
    b.rpc("game.selectWeapon", {"slot": 2}); time.sleep(1.2)
    active2 = ((b.rpc("game.weapons").get("result") or {}).get("active") or {}).get("name")
    # specific weapon via CCSGOInput MakeWeaponSelection (CS2 has no `use` command): knife by name, then the
    # pistol back by defIndex (32 = hkp2000)
    r = b.rpc("game.selectWeapon", {"weapon": "weapon_knife"}); save("game.selectWeapon_weapon", r); time.sleep(1.2)
    check("game.selectWeapon weapon via MakeWeaponSelection", (r.get("result") or {}).get("via") == "MakeWeaponSelection", json.dumps(r.get("error"))[:120])
    activeW = ((b.rpc("game.weapons").get("result") or {}).get("active") or {}).get("name")
    r = b.rpc("game.selectWeapon", {"defIndex": 32}); save("game.selectWeapon_defIndex", r); time.sleep(1.2)
    activeD = ((b.rpc("game.weapons").get("result") or {}).get("active") or {}).get("name")
    r = b.rpc("game.selectWeapon", {"weapon": "weapon_ak47"}); save("game.selectWeapon_not_held", r)
    save("game.weapons_after_select", {"start": active0, "slot3": active3, "slot2": active2, "weapon_knife": activeW, "defIndex32": activeD})
    check("select_weapon slot3 -> knife", active3 == "weapon_knife", "start=%s slot3=%s" % (active0, active3))
    check("select_weapon slot2 -> pistol", active2 == "weapon_hkp2000", "slot2=%s" % active2)
    check("select_weapon weapon_knife -> knife", activeW == "weapon_knife", "weapon=%s" % activeW)
    check("select_weapon defIndex 32 -> pistol", activeD == "weapon_hkp2000", "defIndex=%s" % activeD)
    check("select_weapon not-held weapon -> -32602", (r.get("error") or {}).get("code") == -32602, json.dumps(r)[:120])
    check("viewAngles vs eyeAngles sanity", isinstance(loc.get("viewAngles"), list) and isinstance(pl.get("eyeAngles"), list), "view=%s eye=%s inputIsStatic=%s" % (loc.get("viewAngles"), pl.get("eyeAngles"), loc.get("inputIsStatic")))

    # radar
    t0 = time.time()
    r = b.rpc("game.radar"); dt = (time.time() - t0) * 1000
    save("game.radar", r)
    rad = r.get("result") or {}
    check("game.radar ok", "result" in r, json.dumps(r.get("error"))[:120])
    players = rad.get("players") or []
    check("game.radar players>0", len(players) > 0, "players=%d bomb=%s hostages=%d (%.0f ms round trip)" % (len(players), [x.get("state") for x in rad.get("bomb", [])], len(rad.get("hostages", [])), dt))
    local = [p for p in players if p.get("isLocal")]
    check("game.radar has isLocal", len(local) == 1, str(local[:1])[:160])
    check("game.radar origins", all(isinstance(p.get("origin"), list) for p in players if p.get("alive")), "")

    # sounds: both sinks, then provoke sounds (jump + attack) through the input hook
    r = b.rpc("sound.subscribe", {"mode": "both"}); save("sound.subscribe", r)
    check("sound.subscribe both", (r.get("result") or {}).get("mode") == "both", json.dumps(r)[:160])
    b.rpc("sound.drain", {"max": 256})   # clear
    b.rpc("input.set", {"tap": ["jump"], "frames": 2})
    time.sleep(0.6)
    b.rpc("input.set", {"tap": ["attack"], "frames": 2})
    time.sleep(1.5)
    r = b.rpc("sound.drain", {"max": 256}); save("sound.drain", r)
    ev = (r.get("result") or {}).get("events") or []
    wrap = [e for e in ev if e.get("via") in (11, 12, 13)]
    lis = [e for e in ev if e.get("via") == 81]
    check("sound.drain events after jump+attack", len(ev) > 0, "total=%d wrap=%d listener=%d names=%s" % (len(ev), len(wrap), len(lis), sorted({e.get("name") for e in ev if e.get("name")})[:8]))
    check("wrap records carry entindex", any(isinstance(e.get("entindex"), int) and e["entindex"] >= 0 for e in wrap), "")
    check("listener records carry names", any(e.get("name") for e in lis), "")
    # does inst150 equal the wrap's entindex for the same guid?
    by_guid = {}
    for e in ev:
        by_guid.setdefault(e.get("guid"), {})[e.get("via")] = e
    pairs = [(g, v[81].get("inst150"), v.get(11, v.get(12, v.get(13, {}))).get("entindex")) for g, v in by_guid.items() if 81 in v and any(k in v for k in (11, 12, 13))]
    same = [p for p in pairs if p[1] == p[2]]
    check("listener inst150 == wrap entindex", bool(pairs) and len(same) == len(pairs), "pairs=%s" % pairs[:6])
    save("sound.pairs", pairs)
    r = b.rpc("sound.status"); save("sound.status", r)
    r = b.rpc("sound.uninstall"); save("sound.uninstall", r)
    check("sound.uninstall", (r.get("result") or {}).get("installed") is False)
    b.rpc("input.clear")

    n_ok = sum(1 for _, ok, _ in results if ok)
    print("\n%d/%d checks passed" % (n_ok, len(results)))
    with open(os.path.join(a.out, "summary.json"), "w") as f:
        json.dump([{"check": n, "pass": ok, "detail": d} for n, ok, d in results], f, indent=1)
    return 0 if n_ok == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
