# fixtures

The bridge recording the mock replays lives in **`../../crates/cs2-mcp-common/fixtures/bridge-latest.jsonl`**
(embedded into `cs2_mcp_common::mock` with `include_str!`; override per test with
`MockBridge::builder().fixtures(text)`). It is a copy of
`../../../idabins/worknotes/mcpserver/fixtures/bridge-latest.jsonl`: a real recording against the live
**M2.1/M2.2** bridge (2026-10-06; `gameBuild=2000921`), one JSON object per line (`tx.obj`,
`rx.line`, `rx.obj`). Tokens in fixtures are dead (they rotate per bridge load).

This directory holds server-specific fixtures only (none yet). A fresh m2.8 recording is a
remaining bootstrap item (workflow 00).

## testscript/

- `testscript/move_jump_pass.console.log`: 17 lines of a real move_jump run (POC artifacts
  `idabins/worknotes/vtest/artifacts/move_jump-20261010-031947/console.log`), selected line by line, verbatim (not contiguous): the checkpoints,
  `setpos` lines and the quit line. Judge tests read it.
- `testscript/move_jump_nojump.console.log`: derived from the above: the `midjump` setpos z is set to the
  moved z (`13.163074` -> `-42.892700`), so the jump and landing checks fail.
