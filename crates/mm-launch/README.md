# crates/mm-launch — the gameinfo-free Metamod loader and its launcher

Metamod has to be in front of the engine's own `server.dll`, and the engine loads that file after it
has read `gameinfo.gi`. The old layout made the engine find Metamod first by putting a directory on
its search path, which meant editing `gameinfo.gi` (a game file Steam updates undo) and, on the
`-tools` side, the search path itself was what made the asset system abort. This crate does it
without touching a game file: it answers the engine's single request for its own server image with
Metamod's loader, through the same mechanism the engine uses anyway.

Background and the live spike behind it: `target/DESIGN.md` (§0 facts, §1 components, §4 risks).
Names, paths and log lines are fixed by `target/CONTRACT.md`.

## What is in here

| artefact | target | what it does |
|---|---|---|
| `cs2mcp_launch.exe` (bin) | win64 | starts `cs2.exe` **suspended** with the caller's command-line tail, puts Metamod's loader path in `CS2MCP_MM_LOADER`, injects the DLL with `CreateRemoteThread(LoadLibraryW)`, waits for the game, exits with its exit code. Linux build is a stub that prints "windows only" and exits 2 (CONTRACT). |
| `cs2mcp_mmload.dll` (lib) | win64 | `DllMain(PROCESS_ATTACH)` patches kernel32's IAT slot holding `kernelbase!LoadLibraryExW`; the first call whose path ends in `\csgo\bin\win64\server.dll` restores the slot and loads Metamod's loader instead. |
| `libcs2mcp_mmload.so` (lib) | linux | exports exactly `dlopen`; the first `dlopen` of `.../csgo/bin/linuxsteamrt64/libserver.so` goes to `.../csgo/addons/metamod/bin/linuxsteamrt64/libserver.so` instead. |

`src/paths.rs`, `src/cmdline.rs` and `src/log.rs` are `cfg`-free and unit-tested on the host
(`cargo test -p mm-launch`), so the redirect rule, the command-line split and the log tags can be
checked without a game. The launcher bin cannot `use` the library (the package builds it as a
`cdylib` only, per CONTRACT), so it includes those modules by `#[path]`.

## How it is wired

* The launcher refuses to run unless its tail carries `-insecure` as a whole argument: Metamod,
  mmload and the bridge must never run in a session that could be VAC-secured.
* The launcher computes Metamod's path from its own directory
  (`<game>\bin\win64\..\..\csgo\addons\metamod\bin\win64\server.dll`) or takes an explicit
  `CS2MCP_MM_LOADER`, refuses to start if `cs2.exe`, `cs2mcp_mmload.dll` or Metamod's loader is
  missing, and passes the path on to the DLL through the same variable. The DLL never derives it.
* The launcher attaches a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before the resume, so
  the game cannot outlive it. Best effort: Wine refusing either call is logged and the boot
  continues.
* The launcher passes the raw `GetCommandLineW` tail through verbatim (no re-splitting, no
  unquoting) and hands the child this process's std handles when they are inheritable, so the
  container's log pipe keeps working.
* Both hooks are one-shot and restore themselves **before** calling the loader, so everything
  Metamod does (including its own load of the real game image) reaches the real function.

## Facts from the spike this relies on (DESIGN §0)

* Wine's kernel32 imports `LoadLibraryExW` from kernelbase, and the engine's `server.dll` load goes
  through that import; exactly one of ~290 calls per `-tools` boot ends in
  `\csgo\bin\win64\server.dll`.
* On Linux the first `libserver.so` `dlopen` is the engine's, by absolute path, flags
  `RTLD_NOW|RTLD_GLOBAL`.
* Metamod's loader finds the real game DLL itself from the **unmodified** gameinfo (it skips its own
  path), so no search path is needed for it either.
* `LD_PRELOAD` cannot carry the absolute game path (it has spaces; `ld.so` splits on them), so the
  Linux interposer is named by a path relative to the game root on the `cs2.sh` command line.

## Failure modes

| failure | what happens |
|---|---|
| no `-insecure` in the launcher's command-line tail (VAC, windows-native.md §4.1) | `[cs2mcp-launch] FAIL: refusing to inject without -insecure ...`, exit 3, nothing is started (checked first; `cmdline::tail_has_insecure`) |
| `CS2MCP_MM_LOADER` unset/relative, loader file gone, kernel32/kernelbase not loaded, no `LoadLibraryExW` export, no matching IAT slot, slot write refused | DLL logs one `[cs2mcp-mmload] FAIL: <reason>` and returns `FALSE`; the launcher sees a NULL module, terminates the suspended `cs2.exe` and exits 3 with its own `[cs2mcp-launch] FAIL:` line |
| injected `LoadLibraryW` does not return in 30 s | launcher terminates `cs2.exe`, exits 3 |
| Metamod's loader `dlopen`/`LoadLibraryExW` fails at the redirect | logged; the game keeps its NULL handle (engine fails its own way) |
| Linux: Metamod's loader missing at the redirect | `[cs2mcp-mmload] FAIL: ...` and `abort()` (user decision Q2: never a silent Metamod-less boot) |
| Linux: ld.so cannot preload the `.so` (wrong cwd for the relative path, glibc older than 2.34) | ld.so prints "cannot be preloaded ... ignored" and the boot continues **without Metamod** - not fail-closed; treat the `redirect:` marker as required |
| the engine never asks for the matched path (renamed, loaded another way) | no redirect, the boot continues **without Metamod** - again, the `redirect:` marker is the proof Metamod was loaded |
| a probe (`RTLD_NOLOAD`; `LOAD_LIBRARY_AS_DATAFILE*` / `AS_IMAGE_RESOURCE`) for the game image | passes through untouched and does not spend the one shot |
| Wine/Proton changes the kernel32→kernelbase import | the DLL refuses at `DllMain`, so the boot stops loudly; the fallback would be hooking `kernelbase!LoadLibraryExW` inline (DESIGN §4) |

Side effect (Linux): every interposed `dlopen` reaches glibc with our `.so` as the caller, so a
bare-name lookup uses our (empty) RPATH/RUNPATH rather than the caller's. Harmless on all four live
paths today.

## Build and gates

`cargo bridge build` builds this crate for both targets, stages
`target/bridge-rs/{win64/cs2mcp_launch.exe,win64/cs2mcp_mmload.dll,linuxsteamrt64/libcs2mcp_mmload.so}`
and runs the loader's export gate (`cargo bridge check-exports --loader <file>`) on the two loader
images: the DLL must export exactly
`DllMain` (the PE entry point's callback, not a plugin entry point) and no ordinal-only names, the
`.so` exactly `dlopen`. The DLL is also held to `+crt-static` for the same reason as the bridge.

Real Windows installs **are** a target now (reversed 2026-11-04): `crates/cs2-native`
(`cs2mcp-native`) installs these artefacts into a user's Steam tree and starts the game through
this launcher. What has been seen live on native Windows is listed in
`workflows/native-launcher-report.md`; the IAT slot question (windows-native.md §4.2) is answered
there.
