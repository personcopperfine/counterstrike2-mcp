# fixtures

The bridge recording the mock replays lives in **`../../crates/cs2-mcp-common/fixtures/bridge-latest.jsonl`**
(embedded into `cs2_mcp_common::mock` with `include_str!`; override per test with
`MockBridge::builder().fixtures(text)`). It is a copy of
`../../../idabins/worknotes/mcpserver/fixtures/bridge-latest.jsonl`: a real recording against the live
**M2.1/M2.2** bridge (2026-10-06; `gameBuild=2000921`), one JSON object per line (`tx.obj`,
`rx.line`, `rx.obj`). Tokens in fixtures are dead (they rotate per bridge load).

This directory holds server-specific fixtures only:
`vmdl/` — synthetic `.vmdl` files for `tests/vmdl.rs` (Valve's layout: CRLF, tabs, no final newline;
`static_prop.vmdl` and `skeletal.vmdl` are structurally valid, `bad_tree.vmdl`, `bad_root.vmdl` and
`missing_root.vmdl` exist to be rejected by `hammer_mcp::vmdl::Model::parse`). A fresh m2.8
recording is a remaining bootstrap item (workflow 00).
