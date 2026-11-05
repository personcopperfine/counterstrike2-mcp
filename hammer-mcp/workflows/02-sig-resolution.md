# 02 — Signature-backed addresses and game-update porting

## Goal
The server never contains an address, yet a game update is a mechanical drill: the sig DB
(`sigs/signatures/*.json`) is re-recorded, the bridge regenerated, and `hammer-mcp` reports
precisely what is stale until then.

## Prerequisites
- Read `sigs/README.md`, `workflows/game-update.md`, `cargo bridge pins`.
- `python3` on the host (dev tooling only).

## Steps
1. **Reader** (`crates/cs2-mcp-common/src/sigs`, shared with client-mcp): parse every `signatures/<module>.json` (skip `SCHEMA.json`, `idb/`);
   expose `newest_build()`, `module_builds()`, `entries_with_bridge()`,
   `unverified_bridge_entries(build)`, `status(name)`. Entry names are the vocabulary
   (`HFN_*`, `HV_*`, `EVS_RS_GAME_WORLD`, `CMapDoc::...`). Unit-test against the real files.
2. **Build pin at connect**: compare `hello.gameBuild` with `--expect-build` (default: sig DB
   newest build) and with `workshop/run/app/game/csgo/steam.inf` `ClientVersion` (host path via
   the runtime). Mismatch -> `BuildMismatch` for every bridge tool; `session_status` still works.
3. **`doctor` tool** (read-only, never raises): per module — bridge pin state from
   `control.capabilities.modules` (`verified|failed|not_loaded|verifying`), sig DB build coverage,
   count of `bridge`-tagged entries without `expect[<build>]`, and the exact next command from
   WORKFLOW §10 (`scripts/sigupdate.py check --tree ... --old OLD --build NEW`, then `cargo bridge pins`,
   `cargo bridge build`, `cargo bridge install`).
4. **Dev tooling** (`research/sigs.sh`): wrappers that call `sigs/scripts/sigcheck.py validate|resolve
   --strict` and `sigupdate.py check` so a developer can run the drill from this repo. No Rust
   resolver port (byte-pattern/RTTI resolution stays in Python; it is used at build time, not at
   runtime).
5. **New symbol procedure** (`crates/cs2-bridge/README.md` "Adding a method"): find in IDA -> `sigs/ida/ida_sigs.py
   dump_auto` -> entry with a `bridge` macro in `sigs/signatures` -> `cargo bridge pins`
   -> handler in `crates/cs2-bridge/src/ops/` + its `registry.rs` row (this repo) -> its `crates/cs2-bridge/PROTOCOL.md` section -> tool
   here. The server consumes the **method**, never the macro.
6. **Update drill test**: a fixture sig DB with a module missing `expect["<new>"]` must make
   `doctor` list it and `session_status` say `BuildMismatch` when hello reports `<new>`.

## Definition of done
- `grep -nE '0x18[0-9A-Fa-f]{7}' hammer-mcp/src crates/cs2-mcp-common/src client-mcp/src` is empty and a CI-style test enforces it (allow-list: none).
- `doctor` output on 2000930 shows 8 pinned modules verified and zero unbacked bridge entries.

## Open questions
- Whether the Rust side should verify module hashes itself (sha256 of the DLLs in the mounted tree
  vs `builds[<build>].sha256`) as an early, pre-launch `BuildMismatch` — cheap and useful; decide
  in 01 once the runtime exposes host paths.
- `expect` holds RVAs as integers; the reader must never print them in tool results (only names,
  builds and states) to honour the no-addresses rule in model-visible output.

## Evidence
- `../idabins/signatures/*.json` (9 406 of 9 879 citations backed on 2000930), `scripts/sigcheck.py`
  (`CURRENT_BUILD = "2000930"`), `scripts/sigupdate.py`, `cargo bridge pins` (fail-closed consumer) [proven].
