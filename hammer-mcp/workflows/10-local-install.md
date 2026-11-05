# 10 — Local install runtime backend (phase 2)

## Goal
`--runtime local` drives the same tools session from a game install on the host filesystem (a
Steam library with the tools depot, run under the user's Proton, or native Windows), with no
behavioural difference above the `Runtime` trait.

## Prerequisites
- 01 stable (the trait is proven by the docker backend); 08 ideally done (otherwise local = the
  user's own display, which is the common desktop case anyway).
- Read `../idabins/docs/WORKFLOW.md` §10 step 7 (tools depot 2347779 is **not** installed by
  `app_update 730`; gbe `steam_api64.dll` swap), `workshop/entrypoint.sh` (wine env derivation).

## Steps
1. **Discovery of the install**: `--install-root` or Steam library parsing (`libraryfolders.vdf`,
   `appmanifest_730.acf`); require `game/bin/win64/tools/hammer.dll` and `game/bin/sdkenginetools.txt`.
2. **Prefix**: `compatdata/730/pfx` next to the library (Proton) -> discovery file path
   `<pfx>/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json`; on native Windows,
   `%LOCALAPPDATA%\cs2mcpbridge\session.json` and the named-pipe binding (protocol §1) later.
3. **Launcher**: with the Steam client running, `steam -applaunch 730 -tools ...` (Steam owns the
   Proton env); without it, derive the env as `workshop/entrypoint.sh` does from a Proton dir.
   Bridge install = `install-rs.sh install` against the local tree (manifest entry + DLL); back up and restore
   the manifest on `down` so the user's install is left clean.
4. **Path map**: `Z:` = `/`, so Windows paths from `doc.active` map to host paths directly; `C:` ->
   `<pfx>/drive_c`.
5. **Build pin**: read `steam.inf` from the local tree; refuse (BuildMismatch) when it is newer than
   the sig DB's build — a local Steam install auto-updates, which is the main reason this backend is
   phase 2.
6. **Policy**: never modify the user's install beyond the manifest entry + `tools/cs2_bridge.dll`,
   and only with `--allow-install-bridge`.

## Definition of done
- The 03 acceptance scripts pass with `--runtime local` on a Proton install; `down` leaves the
  manifest byte-identical.

## Open questions
- Steam client presence: gbe is not used locally; does `-tools` with the real Steam client admit the
  unsigned bridge DLL the same way (`tool-plugin-admission.md` open items on `csgo.signatures`)?
- Native Windows host: the named-pipe variant of the bridge transport and a Windows build of the
  server (this repo targets Linux hosts first).

## Evidence
- None yet for the local backend; the docker backend's ladder and paths are the specification.
