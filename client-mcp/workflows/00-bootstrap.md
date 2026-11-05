# 00 — Bootstrap: stdio MCP server + mock bridge — **DONE** (2026-10-11)

## Goal
`client-mcp` answers `initialize`/`tools/list`/`tools/call` over stdio with `session_status`
working against (a) no session, (b) the mock bridge, and the bridge client passing the ported
contract tests. No game needed.

## Prerequisites
- Rust >= 1.89 (`File::try_lock`, launch lock); `../idabins` present only for the sig DB at runtime (tests do not need it).
- Read `../idabins/worknotes/mcpserver/README.md` (Policies, Timeout classification, Mock bridge),
  `../idabins/docs/data/bridge-protocol.md` §2–§3, §8, and the `input.*`/`test.*`/`console.exec`/
  `game.*` rows of §7.

## Steps (status)
1. **rmcp adapter** — done, shared: `crates/cs2-mcp-common/src/mcp.rs` (see hammer-mcp 00 for the mapping).
2. **Bridge client** — done, shared: `crates/cs2-mcp-common/src/bridge/client.rs` (full `bridge_client.py` port).
3. **Mock bridge** — done, shared: `crates/cs2-mcp-common/src/mock.rs` (feature `mock`); client-side
   responders are configured per test (`responder_value("console.exec", {"queued":true})`,
   `methods([... "input.set", "test.checkpoint", "console.exec", "game.tick"])`).
4. **`session_status`** — done: `client-mcp/src/tools/session.rs`: runtime stanza (backend state,
   configured + effective flavour, console.log path/existence), the shared bridge section
   (`crates/cs2-mcp-common/src/status.rs`), and `toolGroups` (`console_log`, `console_exec`, `test`, `input`,
   `render_world`, `session`) with `available` + `reason` (+ `missing` methods) — e.g. "LinuxClient:
   bridge not reachable -> input unavailable".
5. **Tests** — done: shared contract tests in `crates/cs2-mcp-common/tests/` (bridge client matrix, registry
   policies), `client-mcp/tests/server.rs` (tool list + forbidden names, session_status dead/live/
   explicit flavour, `console_exec` refusal before dispatch, dispatch with the budget deadline,
   gating when `console.exec` is absent, surfacing the bridge's `-32011` outside `-testscript`
   sessions, stubs gated then `NotImplementedYet`), `client-mcp/tests/stdio.rs` (real binary over
   stdio: initialize 2024-11-05, tools/list, session_status, console_exec ok + refused, unknown tool
   -32602, EOF -> exit 0, stdout JSON-only). `console::refusal` has unit tests.
6. **lib + bin** — done (`client_mcp` lib, `client-mcp` bin; `ClientConfig` = `CommonConfig` +
   `--flavour` + `--console-log`).
7. **`console_exec` guard** — done and wired to the bridge (`ForbiddenCommand` for
   `quit|exit|disconnect|Test_ExitProcess|crash|_restart|killserver|quit_prompt` in any `;`/newline
   segment; `+`/`-` prefixes stripped).
8. **Re-record fixtures against an m2.8 client session** — remaining (parked `sandbox` + bridge;
   `research/record.py`).

## Remaining
- Step 8 (m2.8 client recording with `input.status`, `test.active`, `game.tick` shapes).
- `session_status.runtime` is static until workflow 01 gives the docker backend `status()`.

## Definition of done
- `cargo test --workspace` green without a game; an MCP client calls `session_status` with no
  session (`BridgeNotRunning`, retrySafe) and against the mock. **Met.**

## Notes from the port
- See hammer-mcp 00 "Notes from the port" (rmcp 3.5 API, heap scratch buffer).

## Evidence
- POC client/mock/tests [proven, 104 mock tests]; m2.8 client-side methods `[V runtime]`
  (`bridge-protocol.md` §7; `worknotes/vtest/input_hook_test.py` PASS x3).
