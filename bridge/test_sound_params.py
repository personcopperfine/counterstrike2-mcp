#!/usr/bin/env python3
"""test_sound_params.py - the packed start params on `sound.drain` (WP-B snd).

The m2.20 pass (plan-snd.md §5 items 4-5) exposed the packed start params the game already
hands to `CSosOperatorSystem::StartSoundEventWithParams` (ISoundOpSystem slot 11): the client's
copy of the server's packed field buffer, a pointer at `params + +8` and a byte length at
`params + +0x1C` on build 2000930 (`+0x20` since 2000940, behind a new 4-byte field). The Rust bridge's method table carries `sound.drain` and the
live version check below compares against that bridge's `BRIDGE_BUILD`. The read used two pins
(SNOFF_SEP_PACKED_PTR expect 8, SNOFF_SEP_PACKED_LEN expect 28, 32 since 2000940, one direct sig on the field-copy
block at soundsystem!0x1801C5C67), clamps the length to 0..65535 (-1 = unreadable) and copies
at most 128 bytes into the ring entry, probing one page at a time. `sound.drain` reports them
per via-11 event as `"params":{"len":N,"hex":"...","truncated":bool}`, `null` when unreadable,
absent for via 12/13/81. WP-S5 (hammer-mcp `drain_sounds`) decodes the walk
`u32 hash LE | u8 type | u16 size LE | data[size]` - `4dce7c5a 0a 0c00 <f32 x><f32 y><f32 z>` is
field hash 0x5a7cce4d (`position`, soundsystem CC_snd_sos_start_soundevent_at_pos), the entity's
Hammer origin.

`--static` (offline; no bridge, no container): reads the Rust bridge's committed sources and
asserts what is still a source fact - `BRIDGE_BUILD` in crates/cs2-bridge/src/lib.rs (m3.N or
later), sound.drain's row in registry.rs (implemented on NEED_SOUND, advertised, not blocked)
and the soundsystem slot pin in the generated pins.rs. The C source-shape checks (the two
SNOFF_* offsets, the capture helper, the slot-11 writer, the ring entry, protocol-diff-m2.20.md
and the no-new-hex diff) are retired with the C bridge; bridge/protocol-diff-m5-tests.md lists
each one and the Rust unit test that covers it. `--src` is retired with them (it drove the C
text checks). It never compiles or runs anything. A missing file is a FAIL, never a skip.

Live mode (default; the ORCHESTRATOR runs it against a running tools session, ideally right after
`drain_sounds {installOnly:true}` + `run_map` under the sound acceptance plan): `sound.status`
(install state), then drain until --events arrive or --seconds pass and PRINT each via-11 event's
params with the positional walk decoded (type 10 + size 12 -> vec3 of f32). This is a reporter for
the orchestrator's §3 step 7 expectations. VERDICT (review-rust L7): the run only counts as a
verification when at least one via-11 event was drained and every check passed - zero via-11
events prints "RESULT: NOT RUN" and exits 2 (never PASS/exit 0), because the params path was never
exercised; zero events at all is the same NOT RUN/exit 2 and is exactly the expected outcome of
acceptance step 8's negative control (muted sounds), not a pass. review N8: params buffers that
were never decoded count the same way - via-11 events drained but every one carrying `params:null`
is NOT RUN/exit 2, and a bridge session that never comes up prints "NOT RUN:" and exits 2, never
"FAIL:" (that is not a failed check, it is a run that did not happen).

Usage: bridge/test_sound_params.py [--static] [--prefix <wine pfx>]
       [--events N] [--seconds S] [--wait S]
Exit 0 when every check passed and at least one via-11 event was drained
(the params path was exercised); exit 1 on a failed check; exit 2 when the run did NOT RUN
(bridge unreachable or zero via-11 events drained).
"""
import argparse
import json
import os
import socket
import struct
import sys
import time

BRIDGE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BRIDGE_DIR)
from bridge_build import (BUILD_SHAPE, expected_build, methods, pin_names,  # noqa: E402
                          row_problems)
from cs2mcp_root import PFX_DIR  # noqa: E402

BUILD = expected_build()                   # the Rust bridge's single BRIDGE_BUILD

# known packed-field hashes from the soundevents corpus (RE: re-snd.md R4); the walk prints the
# name when known, the plain hash otherwise
KNOWN_HASHES = {0x5A7CCE4D: "position"}

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

def static_report(src_inc) -> int:
    """the Rust bridge contract this pass relies on: the build string, sound.drain's row and
    the soundsystem slot pin. The C text checks (and --src, which drove them) are retired."""
    if src_inc is not None:
        print("FAIL: --src is retired: the C source-shape checks it drove are retired "
              "(see bridge/protocol-diff-m5-tests.md)")
        return 1
    try:
        build = expected_build()
        rows = methods()
        pins = pin_names()
    except (OSError, RuntimeError) as ex:
        print(f"FAIL: cannot read the Rust bridge sources: {ex}")
        return 1

    missing = []
    if BUILD_SHAPE.match(build) is None:
        missing.append(f"BRIDGE_BUILD {build!r} (expected m3.N or later)")

    # sound.drain is still the sound lane's body: the row is what this static mode can prove
    missing += [f"sound.drain: {why}" for why in row_problems(rows, "sound.drain", "NEED_SOUND")]
    if "SNVS_SOS_START_WITH_PARAMS" not in pins:
        missing.append("SNVS_SOS_START_WITH_PARAMS missing from "
                       "crates/cs2-bridge/src/generated/pins.rs")

    step("static_sound_params_contract", not missing, f"missing={missing}" if missing else "")
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# ---- live --------------------------------------------------------------------------------------

def decode_params(hexstr):
    """the WP-S5 walk: per field u32 hash LE | u8 type | u16 size LE | data[size];
    type 10 + size 12 -> vec3 of f32; anything else -> its bytes."""
    fields = []
    try:
        raw = bytes.fromhex(hexstr)
    except ValueError:
        return [{"error": "bad hex"}]
    at = 0
    while at + 7 <= len(raw):
        h, ty = struct.unpack_from("<IB", raw, at)
        size = struct.unpack_from("<H", raw, at + 5)[0]
        at += 7
        data = raw[at:at + size]
        at += size
        f = {"hash": "0x%x" % h, "type": ty, "size": size}
        if h in KNOWN_HASHES:
            f["name"] = KNOWN_HASHES[h]
        if ty == 10 and size == 12 and len(data) == 12:
            f["value"] = [round(v, 2) for v in struct.unpack("<fff", data)]
        else:
            f["value"] = data.hex()
        fields.append(f)
        if size == 0 and at >= len(raw):
            break
    return fields


def live_report(args) -> int:
    disc_path = os.path.join(args.prefix, "drive_c", "users", "steamuser", "AppData", "Local",
                             "cs2mcpbridge", "session.json")
    if args.wait > 0 and not poll_until(lambda: os.path.exists(disc_path), args.wait):
        print(f"NOT RUN: the bridge session never came up (discovery file not found: {disc_path})")
        return 2
    try:
        disc = json.load(open(disc_path))
        s = socket.create_connection((disc["host"], disc["port"]), timeout=120)
    except Exception as ex:  # noqa: BLE001 - the bridge session is not up
        print(f"NOT RUN: cannot reach the bridge session ({ex})")
        return 2
    f = s.makefile("r")
    n = [0]

    def call(method, params=None):
        n[0] += 1
        s.sendall((json.dumps({"jsonrpc": "2.0", "id": "sp%d" % n[0], "method": method,
                               "params": params or {}}) + "\n").encode())
        line = f.readline()
        if not line:
            raise RuntimeError("connection closed")
        return json.loads(line)

    call("control.hello", {"protocol": 1, "token": disc["token"]})

    r = call("control.version", {})
    ver = (r.get("result") or {}).get("bridgeVersion")
    step("version", ver == BUILD, f"bridgeVersion={ver!r} expected {BUILD!r}")
    if not step("version", ver == BUILD):
        s.close()
        return 1

    # sound.status first: the wrap must be installed (or installable where the game is loaded)
    r = call("sound.status", {})
    st = r.get("result") or {}
    e = r.get("error") or {}
    if e:
        step("sound_status", False, f"resp={r}")
        s.close()
        return 1
    step("sound_status", True, f"installed={st.get('installed')} mode={st.get('mode')} "
                               f"pending={st.get('pending')} total={st.get('total')} "
                               f"dropped={st.get('dropped')} ringSize={st.get('ringSize')}")

    # drain until --events via-11 events (or any events with --events-any) arrive; every drain's
    # events are printed with their params and the decoded walk
    deadline = time.time() + args.seconds
    seen = []
    slot11 = 0
    decoded = 0
    while len(seen) < args.events and time.time() < deadline:
        r = call("sound.drain", {"max": 256})
        evs = ((r.get("result") or {}).get("events")) or []
        for ev in evs:
            seen.append(ev)
            params = ev.get("params")
            line = f"event seq={ev.get('seq')} name={ev.get('name')!r} hash={ev.get('hash'):#x} " \
                   f"via={ev.get('via')} entindex={ev.get('entindex')} ageMs={ev.get('ageMs')} " \
                   f"entityName={ev.get('entityName')!r}"
            if ev.get("via") == 11:
                slot11 += 1
                if params is None or not (params.get("hex") or ""):
                    print(f"  {line} params={json.dumps(params) if params is not None else 'null'} (no decoded buffer)")
                    continue
                decoded += 1
                dec = decode_params(params.get("hex") or "")
                print(f"  {line} params={{len:{params.get('len')},truncated:{params.get('truncated')}}}")
                for fld in dec:
                    print(f"    field {fld}")
            else:
                print(f"  {line} params=absent")
    if not seen:
        print(f"NOT RUN: events (no drained events within {args.seconds:g}s - expected when the "
              f"session is idle or this is acceptance step 8's negative control)")
    if slot11 == 0:
        # review-rust L7: zero via-11 events means the params path was never exercised - the
        # verdict is NOT RUN, never "PASS" (an exit 0 here read as "m2.20 verified live"). This is
        # the expected outcome of acceptance step 8's negative control (sounds muted), not a pass.
        print(f"NOT RUN: params (0 via-11 events drained within {args.seconds:g}s - the packed "
              f"params path was never exercised)")
        print(f"\nchecks: {CHECKS}")
        print("RESULT: NOT RUN")
        s.close()
        return 2
    if decoded == 0:
        # review N8: via-11 events with params:null never exercised the decode walk either - the
        # wrap is installed but no params buffer was ever captured (or every read failed), so
        # this run verifies nothing about the packed params.
        print(f"NOT RUN: params ({slot11} via-11 events drained within {args.seconds:g}s but "
              f"every one carries params null / no hex - no params buffer was ever decoded, so "
              f"the packed-params path was never exercised)")
        print(f"\nchecks: {CHECKS}")
        print("RESULT: NOT RUN")
        s.close()
        return 2
    step("events_drained", True,
         f"count={len(seen)} via11={slot11} decodedParams={decoded} (each above with its params)")
    for ev in seen:
        if ev.get("via") == 11:
            p = ev.get("params")
            if p is not None and p.get("len") is not None and (p.get("hex") or ""):
                ok_len = 0 < p["len"] <= 65535
                ok_trunc = p.get("truncated") == (len(p.get("hex") or "") < 2 * min(p["len"], 128))
                step("via11_params_shape_%s" % ev.get("seq"), ok_len and ok_trunc,
                     f"len={p['len']} truncated={p.get('truncated')}")
    s.close()
    ok = all(CHECKS.values())
    print(f"\nchecks: {CHECKS}")
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--static", action="store_true",
                    help="offline source expectations (the Rust build string, sound.drain's row, "
                         "the soundsystem pin)")
    ap.add_argument("--src", default=None,
                    help="retired: the C source-shape checks it drove are gone "
                         "(see bridge/protocol-diff-m5-tests.md); passing it FAILs")
    ap.add_argument("--prefix", default=PFX_DIR)
    ap.add_argument("--events", type=int, default=8,
                    help="live: drain until this many events (any slot) were seen")
    ap.add_argument("--seconds", type=float, default=15.0,
                    help="live: how long to keep draining")
    ap.add_argument("--wait", type=float, default=0.0,
                    help="seconds to wait for the discovery file (0 = it must exist)")
    args = ap.parse_args()
    if args.static:
        return static_report(args.src)
    return live_report(args)


if __name__ == "__main__":
    sys.exit(main())
