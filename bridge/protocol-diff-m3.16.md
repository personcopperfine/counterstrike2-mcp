# Protocol changes in m3.16: `job.start` map-stage flags

Bridge build `m3.16` adds no method and no param (branch `streamline/live-fix-a`; LIVE-P1 bug 1).

## What was wrong

The `job.start` flag allowlist (`crates/cs2-bridge/src/ops/job.rs` `PLAIN_FLAGS`) had `-nav` but
not `-world`, `-phys`, `-vis`, `-bakelighting`. hammer-mcp's `compile_map` sends those for
`mode:"novis"` and every `stages` list, so on the bridge route they failed with
`flag "-world" is not allowed` and nothing ran (`full` and `nav` worked).

## Now

`-world -phys -vis -bakelighting` are plain flags (no value) like `-nav`. Still refused: repeats,
anything else unknown. Capability: none to gate on; `hello.build` is `m3.16`. Against an older
bridge `novis` / `stages` keep failing with the old refusal.

Test: `hammer-mcp` `bridge_flags_are_all_in_the_job_allowlist` (imports `PLAIN_FLAGS` /
`NUMERIC_FLAGS` from cs2-bridge and checks every flag `compile_map` emits for every mode and
every stage), plus `job.rs` unit test. Not run live.
