"""cs2mcp_root.py - where the project root is, for the bridge/ Python tools (stdlib only).

The same rule as bridge/lib-root.sh (shell): `$CS2_MCP_PROJECT_ROOT`, else `$CS2DOCKER_ROOT` (the
deprecated alias), else the repo's main checkout (a linked git worktree resolves to the main
worktree, which holds the shared runtime state), else this checkout. The project root holds
`workshop/run/app` (the Wine game tree, mounted at /app in the tools container) and `.podman/`.

Older layout: a parent directory holding this repo as `cs2-mcp/` next to `workshop/` - set
CS2_MCP_PROJECT_ROOT to that directory.

The scripts run as `python3 -I bridge/<script>.py`, so each inserts its own directory into
`sys.path` before importing this module.
"""
import os
import subprocess

# bridge/cs2mcp_root.py -> this checkout's root
CHECKOUT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def main_checkout(checkout: str = CHECKOUT) -> str:
    """The main worktree of the repo `checkout` belongs to; `checkout` without git or a match."""
    try:
        common = subprocess.run(
            ["git", "-C", checkout, "rev-parse", "--path-format=absolute", "--git-common-dir"],
            capture_output=True, text=True, timeout=10, check=True).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return checkout
    main = os.path.dirname(common)
    if os.path.isfile(os.path.join(main, "bridge", "install-rs.sh")):
        return main
    return checkout


def project_root() -> str:
    for name in ("CS2_MCP_PROJECT_ROOT", "CS2DOCKER_ROOT"):
        value = os.environ.get(name)
        if value:
            return value
    return main_checkout()


PROJECT_ROOT = project_root()
APP_DIR = os.path.join(PROJECT_ROOT, "workshop", "run", "app")
PFX_DIR = os.path.join(APP_DIR, "compatdata", "730", "pfx")
PODMAN_XDG = os.path.join(PROJECT_ROOT, ".podman", "xdg")
