# 10 — Local install runtime backend (phase 2)

## Goal
`--runtime local` runs the same session flavours from a game install on the host (Linux native
CS2 from a Steam library, or the Windows build under the user's Proton), with no difference above
the `Runtime` trait.

## Prerequisites
- 01 stable. Read `cs2-docker/client/entrypoint.sh` (what the container does that a host launch must
  replicate: gbe setup, `cs2_video.txt` seeding, `LD_PRELOAD` audio libs, `-condebug`),
  `../.github/skills/cs2-lavapipe/SKILL.md` (`cs2.sh` codename gate: `VERSION_CODENAME` must be
  `sniper` — on a host that is the Steam Runtime's job), `WORKFLOW.md` §10 step 7.

## Steps
1. **Install discovery**: `--install-root` or Steam `libraryfolders.vdf` + `appmanifest_730.acf`;
   require `game/cs2.sh` (Linux) or `game/bin/win64/cs2.exe` + Proton prefix (Windows build).
2. **Launch**: prefer the Steam client (`steam -applaunch 730 -insecure -testscript <abs path> ...`)
   so the runtime, Proton and Steam auth are the user's; without Steam, replicate the container's
   env (steamrt pressure-vessel is non-trivial — document as unsupported first).
   Display: the user's `DISPLAY` by default; `headless:true` starts a private `Xvfb :99` (Linux) —
   the Wine flavour inherits hammer-mcp's 08 result.
3. **Paths**: `console.log` under `game/csgo/`, screenshots/demos likewise; discovery file under the
   Proton prefix for the Wine flavour; Linux flavour has no bridge.
4. **Build pin**: `steam.inf` `ClientVersion` vs sig DB newest; a local Steam install auto-updates,
   so expect `BuildMismatch` after every game update until WORKFLOW §10 runs (that is why this is
   phase 2).
5. **Policy**: never modify the install except staging test scripts into a dedicated subdirectory
   and, with `--allow-install-bridge`, the manifest entry + `tools/cs2_bridge.dll` (Wine flavour).
6. Tests: install discovery against fixture `.vdf/.acf` files; live test behind a feature.

## Definition of done
- `run_testscript move_jump` passes with `--runtime local` on a Linux Steam install (markers +
  screenshots + demo), leaving the install unchanged apart from artifacts.

## Open questions
- `-insecure -testscript` with the real Steam client (VAC-disabled sessions, offline mode).
- Linux build and `CTestScriptMgr` (01 open question) — decisive for the Linux local flavour.

## Evidence
- None for the local backend; the docker backend's ladder and paths are the specification.
