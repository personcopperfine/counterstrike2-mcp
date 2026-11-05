# 00 — Bootstrap: stdio MCP server + mock bridge — **DONE** (2026-10-11)

## Goal
`hammer-mcp` answers `initialize`/`tools/list`/`tools/call` over stdio with one real tool,
`session_status`, backed by a bridge client that passes the ported contract tests against a mock
bridge. No game needed.

## Prerequisites
- Rust >= 1.88 (`rmcp` 3.5 MSRV). `../idabins` present only for the sig DB at runtime (tests do not need it).
- Read: `../idabins/worknotes/mcpserver/README.md` (Policies, Timeout classification, Mock bridge),
  `../idabins/docs/data/bridge-protocol.md` §2–§3, §8.

## Steps (status)
1. **rmcp adapter** — done: `crates/cs2-mcp-common/src/mcp.rs` implements `ServerHandler` over
   `tools::Registry` (`tools/list` from `ToolSpec`, `tools/call` -> handler -> text JSON block +
   `image` blocks; tool failure = `isError:true` with `{"error": payload}`; unknown tool = JSON-RPC
   `-32602`), serves on `rmcp::transport::io::stdio()`. `rmcp` is imported only there.
2. **Bridge client** — done: `crates/cs2-mcp-common/src/bridge/client.rs` ports `bridge_client.py`
   (discovery, hello-first, NDJSON with advertised `limits.maxLineBytes`, string ids, `deadlineMs`
   in params, reply grace, timeout classification, reconnect on `OutcomeUnknown`/`ConnectionLost`,
   Busy/NotAuthenticated retry on connect, capability refresh on `revision`, liveness/modal gate,
   `note_job`/`EngineBusy`, `active_document` reason mapping).
3. **Mock bridge** — done: `crates/cs2-mcp-common/src/mock.rs` (feature `mock`) ports `mock_bridge.py`:
   ephemeral loopback listener, `session.json`, fixture replay (`crates/cs2-mcp-common/fixtures/bridge-latest.jsonl`),
   auth/version/id-less/unknown-method/accept-time Busy, knobs `main_thread_age_ms`, `drain_blocked`,
   `inflight`, `delay`, `silent`, `drop_method`, `error`, `responder`, `methods`, `revision`, `jobs`.
4. **`session_status`** — done: `hammer-mcp/src/tools/session.rs` (+ `crates/cs2-mcp-common/src/status.rs`
   for the bridge half): never raises; bridge/discovery (token never included), capabilities,
   version, status, liveness, active document + mapped host path (`paths::PathMap`), unsaved-edit
   counter, `launch.command` (`bridge/runtime.sh up`) when down.
5. **Tests** — done: `crates/cs2-mcp-common/tests/bridge_client.rs` (hello/caps, explicit deadline, unknown
   method, Busy, raw-frame wire errors, protocol/build mismatch, missing/stale discovery, dead pid,
   bad token, no document, QueueTimeout vs OutcomeUnknown vs ConnectionLost + recovery, the whole
   modal-gate matrix, every wire code -32001..-32022/-32601/-32603, reason mapping, revision refresh),
   `crates/cs2-mcp-common/tests/registry.rs` (gating, dead bridge, budget bounds/exhaustion, blocked gate,
   timeout classes through a tool, handle stripping, forbidden methods), `hammer-mcp/tests/server.rs`
   (POC tool order, forbidden names, session_status live/dead/unmapped/not-loaded, gating per tool,
   stubs never dispatch), `hammer-mcp/tests/stdio.rs` (real binary: initialize, tools/list = 24,
   session_status, gated tool, unknown tool -32602, unknown method -32601, ping, EOF -> exit 0,
   stdout is JSON-only, logs on stderr). Plus unit tests for `PathMap` and base64.
6. **lib + bin** — done (`hammer_mcp` lib, `hammer-mcp` bin).
7. **Re-record fixtures against m2.8** — remaining (needs a live session; `research/record.py`).

## Remaining
- Step 7 (m2.8 recording). The mock synthesises m2.2+ shapes (`revision`, `inflight`, `modules`) already.
- `notifications/cancelled`: rmcp handles the notification; nothing is cancelled bridge-side by design.

## Definition of done
- `cargo test --workspace` green without a game; an MCP client lists tools and calls
  `session_status` with the bridge down (`BridgeNotRunning`, retrySafe) and against the mock. **Met.**
- stderr-only logging verified (stdout byte-exact protocol). **Met (stdio test).**

## Notes from the port
- rmcp 3.5: `ServerHandler` methods are native `async fn`; `get_info()` returns `ServerConfig` =
  `InitializeResult` (non-exhaustive: build with `InitializeResult::new(caps)` and set fields);
  `CallToolResult::success/error` convert into `CallToolResponse`; `ListToolsResult::with_all_items`.
  Version negotiation is rmcp's (clients asking 2024-11-05 / 2025-06-18 get that version back).
- Futures: a 64 KiB stack array inside the read loop made `session_status`'s future overflow a
  2 MiB test-thread stack; the client keeps a heap scratch buffer instead. Keep large buffers off
  the stack in async code.

## Evidence
- POC contract and tests: `../idabins/worknotes/mcpserver/{server.py,bridge_client.py,mock_bridge.py,tests/}` [proven, 104 mock tests].
- Protocol: `../idabins/docs/data/bridge-protocol.md` rev m2.8 (method table generated from `bridge/mcp_bridge.c` `k_methods`).
