# 02 — Signatures: build pinning, doctor, and the new-method procedure

## Goal
No address in this crate; a game update is a drill with a precise to-do list; adding a client-side
capability (new bridge method) follows one documented path from IDA to tool.

## Prerequisites
- Read `sigs/README.md`, `workflows/game-update.md`, `cargo bridge pins` (modules pinned: hammer,
  toolframework2, scenesystem, engine2, rendersystemvulkan, **client**, **server**, **tier0**).

## Steps
1. **Reader** (`crates/cs2-mcp-common/src/sigs`, shared with hammer-mcp): parse `signatures/*.json`; `newest_build()`, per-module builds,
   `bridge`-tagged entries and their `status`/`expect` coverage. Names only in output.
2. **Build pin**: compare `hello.gameBuild` and the session tree's `steam.inf` `ClientVersion`
   (Wine tree `workshop/run/app/game/csgo/steam.inf`; Linux volume path via the runtime) with
   `--expect-build` / sig DB newest. Mismatch -> `BuildMismatch` on bridge tools; file/console tools
   keep working (they do not depend on addresses) — say so in `session_status`.
3. **`doctor`** tool: bridge `modules` states, sig coverage per pinned module, unbacked bridge
   entries, next WORKFLOW §10 command. For LinuxClient sessions: "no bridge; build pin from steam.inf only".
4. **Dev tooling** (`research/sigs.sh`): wrappers over `sigcheck.py validate|resolve --strict`,
   `sigupdate.py check`. No Rust resolver.
5. **New-method procedure** (this is the one client-mcp will use most — write it as a checklist in
   `research/NEW-METHOD.md`): e.g. `game.state` (local pawn origin/angles/health):
   IDA (`client.dll.i64`) -> name functions/vtables -> `ida_sigs.dump_auto([...])` entries in
   `signatures/client.json` with `bridge` macros -> `cargo bridge pins` -> `crates/cs2-bridge` (`registry.rs` row + `ops/` handler,
   `NEED_CLIENT`) -> `bridge-protocol.md` §7 row with `[V runtime]` evidence -> tool here, gated on
   the capability. Examples that followed exactly this path: `input.*` (m2.7), `test.*`,
   `console.exec`, `game.tick` (m2.8).
6. **Linux binaries**: the sig DB covers Windows modules; Linux `.so`s have IDBs (`libengine2.so.i64`,
   `libtier0.so.til`) but no bridge. If a Linux-side hook ever exists, it needs its own sig files
   (`signatures/linux/...` — format is build-independent).

## Definition of done
- `grep -nE '0x18[0-9A-Fa-f]{7}' client-mcp/src crates/cs2-mcp-common/src` empty + enforcing test; `doctor` on 2000930 shows the 8
  pinned modules verified; `research/NEW-METHOD.md` exists and was used once (for `game.state`).

## Open questions
- Pre-launch hash check of the mounted DLLs against `builds[<build>].sha256` (cheap early
  `BuildMismatch`) — decide with hammer-mcp 02.
- (Resolved 2026-10-11) New client-side bridge methods land in **this repo** (`crates/cs2-bridge`, a handler in `src/ops/`),
  pinned from `sigs/signatures`; the request list stays in `research/bridge-wishlist.md`.

## Evidence
- Sig DB + fail-closed pins [proven]; `input.*` path from IDA to `[V runtime]` in one day
  (`worknotes/inputhook-scratch.md` log 2026-10-10).
