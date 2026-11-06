#!/usr/bin/env python3
"""Write the GitHub release notes for a cs2-mcp release from what was actually built:

  python3 -I release-notes.py --version v0.1.0 --dist <dir with the assets> \\
      --buildinfo-windows <BUILDINFO.txt> --buildinfo-linux <BUILDINFO.txt> > notes.md

Everything about provenance (git sha, toolchain, the pins gate, gbe, Metamod) is quoted from the
packages' BUILDINFO.txt files, never restated by hand.
"""
import argparse
import pathlib


def kv(path):
    out = {}
    lines = pathlib.Path(path).read_text(encoding="utf-8").splitlines() if path else []
    for line in lines:
        if "=" in line:
            k, v = line.split("=", 1)
            out.setdefault(k, v)
    return out, lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--dist", required=True, type=pathlib.Path)
    ap.add_argument("--buildinfo-windows")
    ap.add_argument("--buildinfo-linux")
    a = ap.parse_args()
    win, win_lines = kv(a.buildinfo_windows)
    lin, lin_lines = kv(a.buildinfo_linux)
    assets = sorted(p.name for p in a.dist.iterdir() if p.is_file())
    pins = win.get("pins_gate") or lin.get("pins_gate") or "unknown"
    print(f"""## cs2-mcp {a.version}

The launcher prepares your own Steam Counter-Strike 2 install for the cs2-mcp MCP servers (the
bridge, Metamod:Source, optionally the gbe Steam emulator), launches it, and puts every file back
when the game exits. Everything it changes is recorded in a manifest with backups; a file Steam
changed meanwhile is reported, never overwritten silently.

**The game always runs with `-insecure`** while the bridge is loaded: VAC is off and official or
secure servers are unavailable in those sessions.

### Downloads

| File | What |
|---|---|""")
    what = {
        "windows-x86_64.msi": "Windows installer, per user (no admin rights; installs to `%LOCALAPPDATA%\\Programs\\cs2-mcp`)",
        "windows-x86_64.zip": "Windows portable: unzip anywhere, run `cs2mcp-launcher.exe`",
        "linux-x86_64.AppImage": "Linux, one file: `chmod +x` and run (`<AppImage> client-mcp`, `<AppImage> hammer-mcp` and `<AppImage> cs2mcp-native` run the CLI tools)",
        "linux-x86_64.tar.gz": "Linux portable: unpack, run `cs2mcp-launcher`; also the package for the container mode (`client-mcp --native-package`)",
        "linux-amd64.deb": "Debian/Ubuntu package (`/opt/cs2mcp`, `/usr/bin/cs2mcp-launcher`)",
        "SHA256SUMS": "SHA-256 of every file here (`sha256sum -c SHA256SUMS`)",
    }
    for name in assets:
        desc = next((d for suffix, d in what.items() if name.endswith(suffix)), "build information" if name.startswith("BUILDINFO") else "")
        print(f"| `{name}` | {desc} |")
    print(f"""
### Not code-signed

None of these files is code-signed. Windows SmartScreen will warn ("Windows protected your PC" ->
More info -> Run anyway), and some antivirus products flag unsigned tools that load code into a
game. Check the download against `SHA256SUMS` first.

### Built with

- game build targeted: `{win.get('game_build_targeted') or lin.get('game_build_targeted', '?')}` (the signatures and pins in this release; another build gets a warning, see the launcher's status)
- **{pins}**{': the runner had no game binaries; RELEASING.md has the maintainer run the full checks (sdkcheck, sigcheck resolve --strict) against them before tagging' if 'SKIPPED' in pins else ''}
- git: `{win.get('git_sha') or lin.get('git_sha', '?')}`; {win.get('rustc') or lin.get('rustc', '?')}
- gbe_fork (the Steam emulator, `--steam gbe`): {win.get('gbe') or lin.get('gbe') or '?'}
- Metamod:Source for Windows: the official AlliedModders dev build **{win.get('metamod_version', '?')}**, downloaded from {win.get('metamod_url', '?')} (sha256 `{win.get('metamod_archive_sha256', '?')}`)
- Metamod:Source for Linux: built from source at `{lin.get('metamod_commit', '?')}` ({lin.get('metamod_version', '?')}), against hl2sdk (alliedmodders/s2sdk) `{lin.get('hl2sdk_commit', '?')}`
""")
    for title, lines in [("Windows", win_lines), ("Linux", lin_lines)]:
        if lines:
            print(f"<details><summary>BUILDINFO ({title})</summary>\n\n```\n" + "\n".join(lines) + "\n```\n</details>\n")


main()
