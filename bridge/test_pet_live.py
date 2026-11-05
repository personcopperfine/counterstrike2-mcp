#!/usr/bin/env python3
"""test_pet_live.py - particle editor (pet) second pass live test (bridge m2.13, pet plan 2 §3).

Run by the orchestrator against a tools session that already passed bridge/test_pet.py (pet.dll
loaded and verified). Never starts a container and never calls podman. Everything happens on a
run-unique copy of the probe (particles/mcp_probe/mcp_live_<RUN>.vpcf), so probe.vpcf itself is
never edited; at the end the probe is reopened and the copy deleted.

 12. pet.text on the copy: kv3 header, the emitter class, bytes == len(text), clean, no history
 13. pet.apply (+1 operator) -> applied, m_Operators count 3, dirty, canUndo; pet.active agrees;
     the main thread still drains (control.ping)
 14. undo -> count 2, canRedo; redo -> 3; undo, undo -> the second is done:false nothing_to_undo
 15. refusals (garbage, truncated body, wrong root class, empty text, 65-char label): each one
     refused, count unchanged, main thread alive
 16. an unknown operator class: applied, the count does not grow (the bridge runs LoadFromFile's
     FilterFunctions after the load, so pet drops it exactly as on open)
 19. pet.save after an apply writes the file (the new operator is on disk); reload force
 18. (--render) pet.render 320x240: host PNG exists, untouched 0, maxLuma > minLuma
 20. (--namedvalue) the m_NamedValueLocals candidate: applied, type-8 count 1, the live text's
     block is written to --out-dir/namedvalue.live.txt, the saved file to namedvalue.saved.vpcf;
     variants (no m_DefaultConfig, a non-string m_ValueType) are applied and their text recorded
 21. (--depth) nested arrays in a root field at 64/128/200/255/256 levels (root object = level 1):
     each applied + ping alive + undo; 257 levels must be refused by the bridge (too deep) without
     reaching pet. A wedge here ends the session: run it last.

Usage: bridge/test_pet_live.py [--prefix P] [--project-root R] [--render] [--namedvalue]
       [--depth] [--out-dir DIR]
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
from cs2mcp_root import PFX_DIR, PROJECT_ROOT  # noqa: E402

BUILD = expected_build()                     # the Rust bridge's single BRIDGE_BUILD

RUN = "%06x%03x" % (int(time.time()) & 0xFFFFFF, os.getpid() & 0xFFF)
NEW_METHODS = {"pet.text", "pet.apply", "pet.undo", "pet.redo", "pet.render"}
PROBE = "particles/mcp_probe/probe.vpcf"
NAMEDVALUE_ENTRY = (
    "\t\t{\r\n\t\t\tm_Name = \"mcp_float\"\r\n\t\t\tm_IsPublic = true\r\n"
    "\t\t\tm_ValueType = \"PVAL_FLOAT\"\r\n\t\t\tm_DefaultConfig = \r\n\t\t\t{\r\n"
    "\t\t\t\tm_ConfigName = \"\"\r\n\t\t\t\tm_ConfigValue = 1.0\r\n"
    "\t\t\t\tm_BoundValuePath = \"\"\r\n\t\t\t\tm_iAttachType = \"PATTACH_INVALID\"\r\n"
    "\t\t\t\tm_strEntityScope = \"\"\r\n\t\t\t\tm_strAttachmentName = \"\"\r\n\t\t\t}\r\n\t\t},\r\n")
NAMEDVALUE_NO_CONFIG = (
    "\t\t{\r\n\t\t\tm_Name = \"mcp_noconfig\"\r\n\t\t\tm_IsPublic = true\r\n"
    "\t\t\tm_ValueType = \"PVAL_INT\"\r\n\t\t},\r\n")
NAMEDVALUE_NUMERIC_TYPE = (
    "\t\t{\r\n\t\t\tm_Name = \"mcp_numtype\"\r\n\t\t\tm_IsPublic = false\r\n"
    "\t\t\tm_ValueType = 2\r\n\t\t},\r\n")

CHECKS = {}


def step(name, ok, detail=""):
    CHECKS[name] = bool(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail else ""))
    return bool(ok)


def poll_until(fn, deadline_s):
    end = time.time() + deadline_s
    while True:
        v = fn()
        if v:
            return v
        if time.time() >= end:
            return None
        time.sleep(0.5)


def count_of(arrays, t):
    for a in arrays or []:
        if a.get("type") == t:
            return a.get("count")
    return None


def classes_of(arrays, t):
    for a in arrays or []:
        if a.get("type") == t:
            return a.get("classes") or []
    return None


def root_open(text):
    """index just after the root object's `{` line (the first `{` after the header line)"""
    nl = text.index("\n")
    brace = text.index("{", nl)
    return text.index("\n", brace) + 1


def insert_in_array(text, key, entry):
    """put `entry` (one `{...},` element with CRLF) first in array `key`, or add the array to the
    root when the document has none"""
    k = text.find("\t" + key + " =")
    if k < 0:
        at = root_open(text)
        return text[:at] + "\t" + key + " = \r\n\t[\r\n" + entry + "\t]\r\n" + text[at:]
    lb = text.index("[", k)
    at = text.index("\n", lb) + 1
    return text[:at] + entry + text[at:]


def add_root_field(text, line):
    at = root_open(text)
    return text[:at] + "\t" + line + "\r\n" + text[at:]


def array_block(text, key):
    k = text.find("\t" + key + " =")
    if k < 0:
        return None
    lb = text.index("[", k)
    depth = 0
    for i in range(lb, len(text)):
        c = text[i]
        if c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                return text[k:i + 1]
    return None


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    return struct.unpack(">II", head[16:24])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--prefix", default=None)
    ap.add_argument("--project-root", default=PROJECT_ROOT)
    ap.add_argument("--render", action="store_true")
    ap.add_argument("--namedvalue", action="store_true")
    ap.add_argument("--depth", action="store_true")
    ap.add_argument("--out-dir", default=None)
    args = ap.parse_args()
    prefix = args.prefix or PFX_DIR
    app = os.path.join(args.project_root, "workshop", "run", "app")
    addon = os.path.join(app, "content", "csgo_addons", "mcp_test")
    probe_host = os.path.join(addon, "particles", "mcp_probe", "probe.vpcf")
    copy_rel = f"particles/mcp_probe/mcp_live_{RUN}.vpcf"
    copy_host = os.path.join(addon, "particles", "mcp_probe", f"mcp_live_{RUN}.vpcf")
    out_dir = args.out_dir or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..",
                                           "target", "orch", "live2")
    os.makedirs(out_dir, exist_ok=True)

    path = discovery_path(prefix)
    if not os.path.exists(path):
        print(f"FAIL: discovery file not found: {path}")
        return 2
    disc = json.load(open(path))
    print(f"discovery: port={disc['port']} pid={disc['pid']} build={disc['bridgeVersion']}")
    s = connect(disc)
    hello(s, disc["token"])
    seq = [0]

    def call(method, params=None):
        seq[0] += 1
        return rpc(s, {"jsonrpc": "2.0", "id": f"x{seq[0]}", "method": method,
                       "params": params or {}})

    def active():
        return (call("pet.active").get("result") or {}).get("doc") or {}

    def ops_count():
        return count_of(active().get("arrays"), 3)

    def alive():
        p = call("control.ping").get("result") or {}
        age = p.get("mainThreadAgeMs")
        return p.get("ok") is True and isinstance(age, (int, float)) and age < 5000, age

    def finish():
        s.close()
        ok = all(CHECKS.values())
        print(f"\nchecks: {CHECKS}")
        print("RESULT:", "PASS" if ok else "FAIL")
        return 0 if ok else 1

    ver = call("control.version").get("result") or {}
    step("version", ver.get("bridgeVersion") == BUILD, f"bridgeVersion={ver.get('bridgeVersion')!r}")
    meths = set((call("control.capabilities").get("result") or {}).get("methods") or [])
    if not step("methods_advertised", NEW_METHODS <= meths, f"missing={sorted(NEW_METHODS - meths)}"):
        return finish()

    wrote = False
    try:
        orig = open(probe_host, "rb").read()
        with open(copy_host, "wb") as f:
            f.write(orig)
        wrote = True
        r = call("pet.open", {"path": copy_rel})
        res = r.get("result") or {}
        if not step("open_copy", res.get("opened") is True, f"resp={r}"):
            return finish()

        # 12. the live text
        r = call("pet.text")
        t = r.get("result") or {}
        text = t.get("text") or ""
        with open(os.path.join(out_dir, "live_text.vpcf"), "w", newline="") as f:
            f.write(text)
        step("text", text.startswith("<!-- kv3") and "C_OP_ContinuousEmitter" in text
             and t.get("bytes") == len(text.encode()) and t.get("dirty") is False
             and t.get("canUndo") is False and t.get("canRedo") is False
             and str(t.get("path", "")).endswith(f"mcp_live_{RUN}.vpcf"),
             f"bytes={t.get('bytes')} dirty={t.get('dirty')} canUndo={t.get('canUndo')} "
             f"canRedo={t.get('canRedo')} path={t.get('path')!r} err={r.get('error')}")
        if not text:
            return finish()
        base = ops_count()

        # 13. apply: one more operator, one undo step
        added = insert_in_array(text, "m_Operators",
                                "\t\t{\r\n\t\t\t_class = \"C_OP_FadeInSimple\"\r\n\t\t},\r\n")
        r = call("pet.apply", {"text": added, "label": "MCP: live test"})
        a = r.get("result") or {}
        ok_alive, age = alive()
        step("apply", a.get("applied") is True and count_of(a.get("arrays"), 3) == base + 1
             and a.get("dirty") is True and a.get("canUndo") is True and a.get("canRedo") is False
             and ok_alive, f"base={base} resp={r} pingAge={age}")
        doc = active()
        step("apply_active_agrees", doc.get("dirty") is True
             and count_of(doc.get("arrays"), 3) == base + 1
             and "C_OP_FadeInSimple" in (classes_of(doc.get("arrays"), 3) or []), f"doc={doc}")

        # 14. undo / redo
        u = call("pet.undo").get("result") or {}
        step("undo", u.get("done") is True and count_of(u.get("arrays"), 3) == base
             and u.get("canRedo") is True, f"resp={u}")
        rd = call("pet.redo").get("result") or {}
        step("redo", rd.get("done") is True and count_of(rd.get("arrays"), 3) == base + 1, f"resp={rd}")
        u1 = call("pet.undo").get("result") or {}
        u2 = call("pet.undo").get("result") or {}
        step("undo_until_empty", u1.get("done") is True and count_of(u1.get("arrays"), 3) == base
             and u2.get("done") is False and u2.get("reason") == "nothing_to_undo",
             f"u1={u1} u2={u2}")
        t2 = (call("pet.text").get("result") or {}).get("text") or ""
        step("undo_restores_text", t2 == text, f"len {len(t2)} vs {len(text)}")
        ok_alive, age = alive()
        step("alive_after_undo", ok_alive, f"pingAge={age}")

        # 15. refusals: nothing reaches pet, the count is unchanged
        header = text[:text.index("\n") + 1]
        refusals = [
            ("garbage", {"text": "{{{ not kv3"}, "UnloadableFile"),
            ("truncated", {"text": header + "{\r\n\t_class = \"CParticleSystemDefinition\"\r\n"
                                            "\tm_Emitters = [ { _class = \r\n"}, "UnloadableFile"),
            ("wrong_root", {"text": text.replace("CParticleSystemDefinition", "CNotParticles", 1)},
             "UnloadableFile"),
            ("empty", {"text": ""}, "InvalidParams"),
            ("long_label", {"text": text, "label": "x" * 65}, "InvalidParams"),
        ]
        for name, params, wire in refusals:
            r = call("pet.apply", params)
            err = r.get("error") or {}
            ok_alive, age = alive()
            n = ops_count()
            step(f"refuse_{name}", err.get("code") == -32602 and err.get("message") == wire
                 and n == base and ok_alive, f"error={err} count={n} pingAge={age}")

        # braces inside a string value are text, not structure (the bridge's params scan)
        notes = insert_in_array(text, "m_Operators",
                                "\t\t{\r\n\t\t\t_class = \"C_OP_FadeInSimple\"\r\n"
                                "\t\t\tm_Notes = \"a } b { c\"\r\n\t\t},\r\n")
        r = call("pet.apply", {"text": notes, "label": "MCP: braces"})
        a = r.get("result") or {}
        lt = (call("pet.text").get("result") or {}).get("text") or ""
        step("braces_in_string_apply", a.get("applied") is True
             and count_of(a.get("arrays"), 3) == base + 1 and "a } b { c" in lt,
             f"err={r.get('error')} notes_kept={'a } b { c' in lt}")
        call("pet.undo")

        # 16. unknown class: pet drops it
        r = call("pet.apply", {"text": insert_in_array(
            text, "m_Operators", "\t\t{\r\n\t\t\t_class = \"C_OP_NoSuchThing\"\r\n\t\t},\r\n")})
        a = r.get("result") or {}
        ok_alive, age = alive()
        step("unknown_class_dropped", a.get("applied") is True and ok_alive
             and count_of(a.get("arrays"), 3) == base
             and "C_OP_NoSuchThing" not in (classes_of(a.get("arrays"), 3) or []),
             f"count={count_of(a.get('arrays'), 3)} (base {base}) classes="
             f"{classes_of(a.get('arrays'), 3)} pingAge={age}")
        call("pet.undo")

        # 19. save after apply
        r = call("pet.apply", {"text": added, "label": "MCP: save test"})
        sv = call("pet.save")
        svr = sv.get("result") or {}
        disk = open(copy_host, "rb").read().decode("utf-8", "replace")
        step("save_after_apply", svr.get("saved") is True and svr.get("skippedClean") is False
             and "C_OP_FadeInSimple" in disk, f"resp={sv}")
        tt = call("pet.text").get("result") or {}
        print(f"info: after save dirty={tt.get('dirty')} canUndo={tt.get('canUndo')}")
        with open(copy_host, "wb") as f:
            f.write(orig)
        rl = call("pet.reload", {"force": True}).get("result") or {}
        tt = call("pet.text").get("result") or {}
        step("reload_after_restore", rl.get("reloaded") is True and ops_count() == base,
             f"resp={rl}")
        print(f"info: canUndo after reload={tt.get('canUndo')} canRedo={tt.get('canRedo')}")

        # 18. render
        if args.render:
            r = call("pet.render", {"width": 320, "height": 240, "name": f"pet_{RUN}"})
            rr = r.get("result") or {}
            host = os.path.join(app, "renders", f"pet_{RUN}.png")
            st = rr.get("stats") or {}
            size = png_size(host) if os.path.exists(host) else None
            step("render", size == (320, 240) and st.get("untouchedPixels") == 0
                 and (st.get("maxLuma") or 0) > (st.get("minLuma") or 0),
                 f"resp={r} host={host} size={size}")

        # 20. m_NamedValueLocals
        if args.namedvalue:
            nv = insert_in_array(text, "m_NamedValueLocals", NAMEDVALUE_ENTRY)
            r = call("pet.apply", {"text": nv, "label": "MCP: namedvalue"})
            a = r.get("result") or {}
            lt = (call("pet.text").get("result") or {}).get("text") or ""
            blk = array_block(lt, "m_NamedValueLocals")
            with open(os.path.join(out_dir, "namedvalue.live.txt"), "w", newline="") as f:
                f.write(blk or "<no m_NamedValueLocals block>")
            step("namedvalue_apply", a.get("applied") is True and count_of(a.get("arrays"), 8) == 1
                 and classes_of(a.get("arrays"), 8) == ["ParticleNamedValueSource_t"],
                 f"resp={r} block={'yes' if blk else 'no'}")
            call("pet.save")
            with open(copy_host, "rb") as f:
                saved = f.read()
            with open(os.path.join(out_dir, "namedvalue.saved.vpcf"), "wb") as f:
                f.write(saved)
            for name, entry in (("noconfig", NAMEDVALUE_NO_CONFIG),
                                ("numtype", NAMEDVALUE_NUMERIC_TYPE)):
                v = insert_in_array(text, "m_NamedValueLocals", entry)
                r = call("pet.apply", {"text": v})
                a = r.get("result") or {}
                lt = (call("pet.text").get("result") or {}).get("text") or ""
                with open(os.path.join(out_dir, f"namedvalue.{name}.txt"), "w", newline="") as f:
                    f.write(array_block(lt, "m_NamedValueLocals") or "<no block>")
                ok_alive, age = alive()
                step(f"namedvalue_{name}", ok_alive,
                     f"applied={a.get('applied')} type8={count_of(a.get('arrays'), 8)} "
                     f"err={r.get('error')} pingAge={age}")
            with open(copy_host, "wb") as f:
                f.write(orig)
            call("pet.reload", {"force": True})

        # 21. depth (last)
        if args.depth:
            for levels in (64, 128, 200, 255, 256):
                deep = add_root_field(text, "m_mcpDeep = " + "[" * (levels - 1) + "]" * (levels - 1))
                r = call("pet.apply", {"text": deep, "label": f"MCP: depth {levels}"})
                ok_alive, age = alive()
                ok = (r.get("result") or {}).get("applied") is True and ok_alive
                step(f"depth_{levels}", ok, f"err={r.get('error')} pingAge={age}")
                if not ok_alive:
                    print("STOP: the main thread is not draining; the session is wedged")
                    return finish()
                call("pet.undo")
            deep = add_root_field(text, "m_mcpDeep = " + "[" * 256 + "]" * 256)
            r = call("pet.apply", {"text": deep})
            err = r.get("error") or {}
            ok_alive, age = alive()
            step("depth_257_refused", err.get("code") == -32602 and ops_count() == base and ok_alive,
                 f"error={err} pingAge={age}")

        # leave the copy's session clean (undo marks dirty; the file still holds `orig`) so the
        # deleted copy's tab never prompts, then make the probe the active document again
        call("pet.reload", {"force": True})
        rl = call("pet.open", {"path": PROBE}).get("result") or {}
        step("reopen_probe", rl.get("opened") is True, f"resp={rl}")
    finally:
        if wrote:
            try:
                os.unlink(copy_host)
                print(f"cleanup: removed {copy_host}")
            except OSError as ex:
                print(f"cleanup: could not remove {copy_host}: {ex}")
    return finish()


if __name__ == "__main__":
    sys.exit(main())
