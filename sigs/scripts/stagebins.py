#!/usr/bin/env python3
"""stagebins.py - copy the binaries the signature database needs into a $CS2_BINS-style tree.

    stagebins.py --game SRC [--game SRC2 ...] --out DIR
    stagebins.py --linux --game SRC --out DIR [--unshare]

SRC is a directory that contains `game/` (a CS2 install such as <project root>/workshop/run/app, or a
steamcmd `download_depot` output directory). With several --game sources the first one that has a
module wins, so a win64 depot plus the tools depot can be combined. Every module named in
sigs/signatures/*.json is copied to DIR/windows/<path inside the install> (sigcheck.MODULE_PATHS).

--linux stages the Linux modules instead (the .so files Linux entries resolve against), to
DIR/linux/<path inside the install> (sigcheck.LINUX_MODULE_PATHS). The Linux game install lives in
the podman volume `cs2-home`, owned by a subuid, so it is only readable inside a user namespace:
pass --unshare and every read/copy goes through `podman unshare` (needs XDG_RUNTIME_DIR; see
sigs/README.md "Linux binaries"). The Linux build is *not* the Windows build number: the
ClientVersion of the staged install is printed and must be recorded as the key of that file's
`builds`/`expect`.

Prints, per module, the 16-hex SHA-256 prefix and the build it matches in the database (`builds`),
plus the ClientVersion in game/csgo/steam.inf when a source has one. Exit 1 when a module is missing.
The copies are Valve's files: DIR must stay outside git (target/ is ignored).
"""
import argparse
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sigcheck  # noqa: E402


def client_version(src, text_of):
    inf = src / "game/csgo/steam.inf"
    try:
        text = text_of(inf)
    except (OSError, subprocess.CalledProcessError):
        return None
    if text is None:
        return None
    for line in text.splitlines():
        k, _, v = line.partition("=")
        if k.strip() == "ClientVersion":
            return v.strip()
    return None


class Source:
    """read/copy a file: directly, or through `podman unshare` for an install that only exists
    inside a user namespace (the cs2-home podman volume is owned by a subuid)."""

    def __init__(self, unshare=False):
        self.podman = ["podman", "unshare"] if unshare else None

    def _run(self, argv, **kw):
        p = subprocess.run(self.podman + argv, capture_output=True, text=True, **kw)
        if p.returncode:
            raise subprocess.CalledProcessError(p.returncode, argv, p.stdout, p.stderr)
        return p

    def exists(self, path):
        if self.podman:
            return self._run(["test", "-f", str(path)], check=False).returncode == 0
        return Path(path).is_file()

    def text(self, path):
        if self.podman:
            return self._run(["cat", str(path)]).stdout
        return Path(path).read_text(errors="ignore") if Path(path).is_file() else None

    def copy(self, src, dst):
        if self.podman:
            self._run(["mkdir", "-p", str(Path(dst).parent)])
            self._run(["cp", "-f", str(src), str(dst)])
        else:
            Path(dst).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--game", action="append", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--linux", action="store_true", help="stage the Linux .so modules instead of the win64 DLLs")
    ap.add_argument("--unshare", action="store_true",
                    help="read/copy through `podman unshare` (a game install owned by a subuid)")
    a = ap.parse_args()
    platform = "linux" if a.linux else "windows"
    src = Source(a.unshare)
    for s in a.game:
        v = client_version(s, src.text)
        print("source %s: steam.inf ClientVersion %s" % (s, v or "(no steam.inf)"))
    missing = 0
    builds_of = {d["module"]: d.get("builds", {}) for d in sigcheck.load_db(None).values()}
    if a.linux:
        # the Linux module set is fixed (sigcheck.LINUX_MODULE_PATHS): it is the set a Linux entry
        # can name, and the .so files of the *other* build are staged before those entries exist
        mods = [(m, {"module": m, "builds": builds_of.get(m, {})}) for m in sigcheck.LINUX_MODULE_PATHS]
    else:
        mods = [(d["module"], d) for d in sigcheck.load_db(None, platform="windows").values()]
    for mod, d in mods:
        rel = sigcheck.module_path(mod, root=Path("/"), platform=platform).relative_to("/")
        inside = rel.relative_to(platform)                                   # game/...
        found = next((s / inside for s in a.game if src.exists(s / inside)), None)
        if found is None:
            print("MISSING %-24s %s" % (mod, inside))
            missing += 1
            continue
        dst = a.out / rel
        src.copy(found, dst)
        sha = sigcheck.sha16(dst)
        builds = [b for b, info in d.get("builds", {}).items() if info.get("sha256") == sha]
        print("%-24s sha256:%s  %s" % (mod, sha, ("= build " + ",".join(builds)) if builds else "new (no recorded build)"))
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
