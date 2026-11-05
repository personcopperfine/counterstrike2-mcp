# Protocol changes for bridge m3.0 (the Rust bridge answers the wire)

**Breaking where it matters**: m3.0 is the first Rust bridge build that serves the protocol at
all (WP2.D wired the pieces merged in M2/M3). It speaks the same v1 envelope, the same
`control.*` methods and the same method table as the C bridge at m2.20, but it implements **no op
method**: every op is refused at the gate instead of executed. A host that points at m3.0 gets a
working control plane and a `-32011 NotImplemented` for everything else; this is the intended
state until M4 ports the first ops. No method was added or removed (84 registered / 69
implemented, unchanged).

## The wire, field by field

| Where | m2.20 (C bridge) | m3.0 (Rust bridge) |
|---|---|---|
| every envelope's `bridge.build` | `"m2.20"` | `"m3.0"` |
| `control.hello` result `bridgeVersion` | `"m2.20"` | `"m3.0"` |
| `discovery.session.json` `"bridgeVersion"` | `"m2.20"` | `"m3.0"` |
| `control.version` result `bridgeVersion` | `"m2.20"` | `"m3.0"` |
| `control.capabilities` / hello `capabilities.methods` | every implemented, unhidden row whose needs are verified | the same rows **intersected with the ported list**: only `control.*` in m3.0 |
| `capabilities.features` / `limits` / `modules` / `revision` | `tcp:true, pipe:false, consoleCommand:false`, `maxLineBytes` 1048576, twelve pin modules, live revision | unchanged |
| `control.ping`, `control.status`, `control.version` shapes | as documented | unchanged (same builders, `crates/cs2-bridge/src/control.rs`, held byte for byte to the C by `tests/control.rs`) |

`BRIDGE_BUILD` is one constant in the crate (`crates/cs2-bridge/src/lib.rs`, re-exported by
`queue`/`control`), so the four sites above cannot drift.

## §7 method table — changed rows

The table itself is the C's, row for row (`tests/registry.rs` compiles the frozen C table and
compares every field). What changed is which rows a caller can actually run:

| Method | Status in m3.0 | Bridge m3.0 answer | Params | Result | Notes |
|---|---|---|---|---|---|
| `control.*` (7 rows, `hello` hidden) | served `[I: in-process gate 1-2]` | unchanged from m2.20 | unchanged | unchanged | `hello` is the handshake; `ping`/`status`/`version`/`capabilities` read live state (`queue::snapshot`, `inflight_since`, `depth`, uptime, last tick, listener flag, shutting-down); `cancel` removes a queued id; `shutdown` flushes and closes |
| every C-implemented op (62 rows: `doc.*`, `view.render`, `world.render`, `input.*`, `test.*`, `console.exec`, `game.*`, `sound.*`, `node.*`, `mesh.*`, `map.*`, `undo.*`, `entity.*`, `edit.batch`, `file.open_asset`, `modeldoc.*`, `pet.*`) | **not ported** `[I: in-process gate 1]` | `-32011 NotImplemented`, `data.blockedOn = "rust bridge m3.0: not ported yet"` | unchanged (parsed, never read) | none | answered by the IPC lane **before** the queue (the C's immediate `out_push` arms, `mcp_bridge.c:6603-6616`), so the queued-request depth does not move |
| the C's documented-but-blocked rows (`doc.list`, `map.run`, `file.open_asset`, `entity.delete`, `selection.get/set`, `mesh.setFaceRect`/`createFaceFromPoints`/`moveVertices`/`extrudeFaces`/`deleteFaces`/`get`, `material.assign`, `job.status`, `job.cancel`) | still blocked `[I: offline]` | `-32011` with the row's own `blockedOn` (unchanged text) | unchanged | none | the `!impl` test still precedes the Rust-only "not ported" test, so these keep their original reason |

Method count: **84 registered / 69 implemented** (unchanged). The Rust bridge serves 7 of the
69; the other 62 are the "not ported" row group above.

## §8 error codes

No new code. The existing meanings are reused:

- `-32011 NotImplemented` now also covers "the C implements it, this Rust build does not"
  (`data.blockedOn = "rust bridge m3.0: not ported yet"`); the C's documented-blocked rows keep
  their own `blockedOn` strings.
- `-32017 PrerequisiteFailed` still exists and still names the first unmet need in the C's
  `bits[]`/`mods[]` order (`mcp_bridge.c:5676-5677`); in m3.0 it is unreachable *through the
  shipped ported list* (step 3 refuses first), but the arm is implemented and tested through an
  injected ported set (`tests/integration.rs`, `dispatch::decide` unit tests) so M4 inherits it.
- `-32601 MethodNotFound` for unknown names and for a `needs == 0` row that reaches the IPC
  gate (unchanged).

## §5/§6 notes

- **The gate moved to the IPC thread for everything it can decide.** In m2.20 a `needs == 0` row
  and an unknown/blocked name were answered immediately but a needs-unmet op still took a queue
  slot and got `-32017` on the main thread. In m3.0 an unported op is answered immediately
  (there is nothing for the main thread to decide), so `control.status.queue` is empty in
  practice; a request that did take a slot would be answered `-32601` by the dispatch tail
  (`mcp_bridge.c:6279`), because no body is ported.
- **The tick is wired.** `IAppSystem::Init` arms the listener; the tick slot (`m0_Slot13` body)
  records `g_lastTick`, polls the modules, starts the listener once and drains at most four
  requests. `PreShutdown`/`Shutdown` shut the queue down and stop the listener (threads joined,
  `session.json` deleted). A tick before `Init` or after `Shutdown` starts nothing.
- **The clock** is `std::time::Instant` since the first call in the process, exposed as
  milliseconds; the C's `GetTickCount64()` is milliseconds since boot. Only `uptimeMs`,
  `mainThreadAgeMs`, `ageMs`, `startedAt` and `inflight` are derived from it, and no host compares
  those across processes.
- **The host side needs no change.** There is no accept/reject version gate in `hammer-mcp`,
  `client-mcp` or `cs2-mcp-common`: capabilities are gated by the advertised method list, and the
  only place a build string is read is `hammer-mcp/src/tools/sound_runtime.rs:484,609`, which
  quotes it in a note when a via-11 `sound.drain` event lacks `params` (data-driven, not a
  comparison). A host pointed at m3.0 therefore reports "method not advertised" /
  `-32011 NotImplemented` for the op tools, not "too old".

## What to expect live (`bridge/test_client.py` against `BRIDGE=rust-only`)

Pass: `unauth`, `badtoken`, `proto`, `hello`, `caps_hello_hidden`, `caps_modules`,
`caps_revision`, `noid`, `ping`, `ping_modules`, `parse_control_*`, `busy`, `unknown` (via
`control.bogus`), `blocked` (`material.assign` keeps its C `blockedOn`).
Fail, by design: `deadline_neg` (`doc.active` is refused `-32011` at the gate before the enqueue
arm can validate `deadlineMs`) and `doc` (`doc.active` has no result). See the WP2.D report for
the exact run.

## Negative evidence (revert one thing at a time)

- `BRIDGE_BUILD` left at `m2.20`: `tests/queue.rs::error_reply_is_byte_equal...` and the
  integration test's hello/discovery assertions fail, and `tests/common/mod.rs`'s one asserted
  substitution stops matching the C's literal;
- `ported` widened past `control.*` without a body: the integration test's
  "only the served control.* rows" assertion fails (the op rows appear in capabilities), and
  `dispatch`'s unit test `ported_is_a_subset_of_the_table_and_is_exactly_control` fails;
- the "not ported" test moved after the needs check: `dispatch`'s
  `an_unported_row_is_not_ported_even_with_every_module_verified` fails (`-32017` instead of
  `-32011`);
- the tick's `Init` gate removed: the integration test's first negative (tick before Init opens
  no port and writes no `session.json`) fails;
- the listener's once-flag removed: the second-tick assertions fail (new port, rewritten
  `session.json`);
- the queue teardown removed: the shutdown-wakes-the-flush-waiter assertion runs into its 30 s
  deadline instead of returning.
