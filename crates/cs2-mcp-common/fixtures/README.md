# fixtures

`bridge-latest.jsonl` — copy of `../../idabins/worknotes/mcpserver/fixtures/bridge-latest.jsonl`:
a real recording against the live M2.1/M2.2 bridge (2026-10-06; `gameBuild=2000921`), one JSON
object per line (`tx.obj`, `rx.line`, `rx.obj`). `cs2_mcp_common::mock::Fixtures::parse` extracts
the discovery values, the hello result (capabilities, modules, revision), the document object and
the error envelopes; the mock synthesises everything else. Tokens are dead (rotate per bridge load).
