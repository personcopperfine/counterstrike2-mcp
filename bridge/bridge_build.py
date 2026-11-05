#!/usr/bin/env python3
"""bridge_build.py - the Rust bridge's build string and the generated tables the parity
scripts read offline.

The bridge moved from `bridge/mcp_bridge.c` (frozen at m2.20) to `crates/cs2-bridge`. The
build string now has exactly one owner, `BRIDGE_BUILD` in `crates/cs2-bridge/src/lib.rs`, and
every `bridge/test_*.py` version check compares against `expected_build()` instead of a
literal. The `--static` modes read the Rust method table (`registry.rs`), the handler tables
(`ops/**/*.rs`) and the generated pins (`generated/pins.rs`) through the helpers here, so a
renamed or missing file is a hard error - never a silent skip.

The scripts run as `python3 -I bridge/test_x.py`: `-I` drops the script directory from
`sys.path`, so each script inserts its own directory first and then imports this module.
"""
import glob
import os
import re

# bridge/bridge_build.py -> the repository root (the worktree the script runs in)
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUST_SRC = os.path.join(REPO_ROOT, "crates", "cs2-bridge", "src")
LIB_RS = os.path.join(RUST_SRC, "lib.rs")
REGISTRY_RS = os.path.join(RUST_SRC, "registry.rs")
OPS_DIR = os.path.join(RUST_SRC, "ops")
PINS_RS = os.path.join(RUST_SRC, "generated", "pins.rs")

# the Rust bridge numbers its build m3.N upwards (the C bridge stopped at m2.20)
BUILD_SHAPE = re.compile(r"^m(?:[3-9]|[1-9][0-9]+)\.[0-9]+$")

_BUILD_RX = re.compile(r'pub const BRIDGE_BUILD: &str = "(m[0-9.]+)";')

# one `Method { ... }` row of registry.rs `METHODS` (the fields are all on one line today;
# `re.S` keeps the parser working if a row is wrapped)
_METHOD_RX = re.compile(
    r"Method\s*\{\s*name:\s*\"([^\"]+)\",\s*needs:\s*([^,]+?),\s*"
    r"implemented:\s*(true|false),\s*hidden:\s*(true|false),\s*"
    r"blocked_on:\s*(None|Some\(\"(?:[^\"\\]|\\.)*\"\))\s*\}",
    re.S,
)
_HANDLERS_BLOCK_RX = re.compile(r"pub static HANDLERS[^=]*=\s*&\[(.*?)\]\s*;", re.S)
_HANDLER_RX = re.compile(r"\(\"([^\"]+)\",\s*[A-Za-z_][A-Za-z0-9_]*\)")
_PIN_CONST_RX = re.compile(r"^pub const ([A-Z][A-Z0-9_]*):", re.M)
_PIN_WHAT_RX = re.compile(r'what:\s*"([A-Z][A-Z0-9_]*)"')


def read(path) -> str:
    """The file's text; `FileNotFoundError`/`OSError` propagate (a missing file must FAIL)."""
    with open(path, encoding="utf-8") as f:
        return f.read()


def expected_build() -> str:
    """`BRIDGE_BUILD` out of `crates/cs2-bridge/src/lib.rs`.

    Exactly one definition must exist - zero or several is a hard error, because a test that
    guessed would pass against the wrong bridge.
    """
    hits = _BUILD_RX.findall(read(LIB_RS))
    if len(hits) != 1:
        raise RuntimeError(
            f"expected exactly one `pub const BRIDGE_BUILD: &str = \"m...\";` in {LIB_RS}, "
            f"found {len(hits)}: {hits}")
    return hits[0]


_TABLE_RX = re.compile(r"pub static (METHODS|RUST_OVERRIDES|RUST_METHODS): &\[Method\] = &\[(.*?)\n?\];", re.S)


def _tables() -> dict:
    """`{table name: [(name, needs, implemented, hidden, blocked), ...]}` for the three tables."""
    text = read(REGISTRY_RS)
    out = {"METHODS": [], "RUST_OVERRIDES": [], "RUST_METHODS": []}
    for table, body in _TABLE_RX.findall(text):
        out[table] = _METHOD_RX.findall(body)
    found = {table for table, _ in _TABLE_RX.findall(text)}
    for table in ("METHODS", "RUST_OVERRIDES", "RUST_METHODS"):
        if table not in found or not out[table]:
            raise RuntimeError(f"no Method rows parsed for {table} out of {REGISTRY_RS}")
    return out


def _row(needs, implemented, hidden, blocked) -> dict:
    return {
        "needs": needs.strip(),
        "implemented": implemented == "true",
        "hidden": hidden == "true",
        "blocked_on": None if blocked == "None" else blocked,
    }


def c_methods() -> dict:
    """The raw C table (`METHODS`, frozen) as `{name: row}`, in table order."""
    rows = {}
    for name, *rest in _tables()["METHODS"]:
        if name in rows:
            raise RuntimeError(f"duplicate METHODS row {name!r} in {REGISTRY_RS}")
        rows[name] = _row(*rest)
    return rows


def methods() -> dict:
    """The EFFECTIVE table as `{name: row}`: the C rows with `RUST_OVERRIDES` applied, then the
    `RUST_METHODS` rows.

    `row` is `{"needs": <text>, "implemented": bool, "hidden": bool, "blocked_on": str|None}`;
    `needs` stays the source text (`"NEED_HAMMER"`, `"0"`, `"NEED_CLIENT | NEED_ENGINE | ..."`)
    so a check can name the bit it expects. Use `c_methods()` for the raw C table.
    """
    tables = _tables()
    rows = c_methods()
    for name, *rest in tables["RUST_OVERRIDES"]:
        if name not in rows:
            raise RuntimeError(f"RUST_OVERRIDES row {name!r} names no C row in {REGISTRY_RS}")
        rows[name] = _row(*rest)
    for name, *rest in tables["RUST_METHODS"]:
        if name in rows:
            raise RuntimeError(f"RUST_METHODS row {name!r} duplicates another row in {REGISTRY_RS}")
        rows[name] = _row(*rest)
    return rows


def handler_names() -> set:
    """Every name in a `HANDLERS` table under `crates/cs2-bridge/src/ops/` (what is served)."""
    names = set()
    files = glob.glob(os.path.join(OPS_DIR, "**", "*.rs"), recursive=True)
    for path in files:
        for block in _HANDLERS_BLOCK_RX.findall(read(path)):
            names.update(_HANDLER_RX.findall(block))
    if not names:
        raise RuntimeError(f"no HANDLERS table parsed under {OPS_DIR}")
    return names


def pin_names(text=None) -> set:
    """Names `generated/pins.rs` carries: `pub const NAME:` values and `what: "NAME"` rows."""
    text = read(PINS_RS) if text is None else text
    names = set(_PIN_CONST_RX.findall(text)) | set(_PIN_WHAT_RX.findall(text))
    if not names:
        raise RuntimeError(f"no pin names parsed out of {PINS_RS}")
    return names


def row_problems(rows, name, needs=None) -> list:
    """The ways `name`'s METHODS row fails the contract, as short strings (`[]` = fine).

    `needs` names the source text of the required bit(s), e.g. `"NEED_HAMMER"`. The row must
    be implemented, advertised (not hidden) and not blocked.
    """
    row = rows.get(name)
    if row is None:
        return ["missing row"]
    problems = []
    if needs is not None and row["needs"] != needs:
        problems.append(f"needs={row['needs']!r} (expected {needs!r})")
    if not row["implemented"]:
        problems.append("implemented=false")
    if row["hidden"]:
        problems.append("hidden=true")
    if row["blocked_on"] is not None:
        problems.append(f"blocked_on={row['blocked_on']}")
    return problems
