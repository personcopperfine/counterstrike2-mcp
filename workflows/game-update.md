# Game update: keep the pointers working after a CS2 update

Valve ships a new CS2 build every few days. Two parts of this repo carry knowledge about Valve's
binaries, and each has its own lane in this workflow:

| Lane | What breaks | Where the knowledge lives | Who fixes it |
|---|---|---|---|
| **A: s2sdk (Linux, Rust; also the bridge's slots and offsets)** | interface version strings, vtable slots, struct layouts, call signatures | the C++ headers in `crates/s2sdk/upstream` (our fork branch of alliedmodders/s2sdk), turned into Rust by `crates/s2sdk/s2sdk-sys/gen/gen.py` and into `bridge/s2sdk_gen.h` (the `S2_SLOT_`/`S2_OFF_` numbers the bridge compiles) | update the headers, regenerate; a fix we make goes upstream as a PR so the community shares it |
| **B: bridge (Windows/Wine)** | RVAs of vtables, functions, globals, Qt chains in 10 DLLs, and the binary measurement of every slot/offset the headers own | `sigs/signatures/*.json` (locators; an entry with `sdk` names the generated expression it measures), turned into `bridge/bridge_pins.h` by `bridge/pins.py` | mechanical: re-resolve the locators, gate, regenerate. A broken locator needs a person with IDA. |

The lanes are not independent where a slot or offset moves: the header (lane A) and the sigs
measurement (lane B) must change together, and `sdkcheck.py` is where they meet (A2b, B5). Do lane
A first: it needs no Steam account.

Read [../AGENTS.md](../AGENTS.md) before starting; its rules apply. In particular, **STOP** means:
write the report (last section), tell the user what you stopped on, and don't try to work around it.

## Before you start

```sh
cd <cs2-mcp worktree>                       # never the shared main checkout
git status --short                          # must be clean (see "Resuming" below if it is not)
git submodule update --init crates/s2sdk/upstream crates/metamod/upstream cs2-docker workshop
git -C crates/metamod/upstream submodule update --init third_party/khook   # metamod's own submodule (gen.py reads khook.hpp)
# in a worktree whose submodule URLs point at local clones, add `-c protocol.file.allow=always` to both
export CS2_MCP_PROJECT_ROOT=<project root>  # holds workshop/run/app + .podman (default: the main checkout); a worktree-built server needs it
export CS2_MCP_IMAGE=${CS2_MCP_IMAGE:-ghcr.io/personcopperfine/cs2-docker:latest}   # the client image (or a local build of cs2-docker/client)
export XDG_RUNTIME_DIR=$CS2_MCP_PROJECT_ROOT/.podman/xdg   # every podman call needs it
export CS2_UPDATE_DIR=${CS2_UPDATE_DIR:-$PWD/target/cs2-update}   # downloads (several GB; never /tmp)
APP=$CS2_MCP_PROJECT_ROOT/workshop/run/app  # the Wine install the bridge runs in
OLD=$(cat sigs/CURRENT_BUILD)
NEW=$(curl -s https://raw.githubusercontent.com/SteamDatabase/GameTracking-CS2/master/game/csgo/steam.inf \
      | sed -n 's/^ClientVersion=//p' | tr -d '\r')
echo "OLD=$OLD NEW=$NEW"
```

**Resuming.** If `sed -n 's/^#define PIN_GAME_BUILD "\([0-9]*\)".*/\1/p' bridge/bridge_pins.h` is
not equal to `$OLD`, an earlier run stopped between B3 and B5. Set `NEW=$OLD`, set `OLD` to the
pinned value, read `workflows/updates/$NEW.md` for what was done, and continue at **B4**. Don't
redo B1–B3.

If `NEW` = `OLD` (and you're not resuming), Valve hasn't shipped anything this workflow cares
about yet: check lane A step A2 only (upstream may have moved anyway) and stop. GameTracking can lag Steam by a few minutes. If the
user says there is an update and GameTracking doesn't show it yet, poll it with a deadline (every 2
minutes, give up after 30) rather than guessing the number.

Start the report file now and fill it in as you go: `workflows/updates/$NEW.md` (template at the end).
Write each step's result into it as soon as the step finishes, so a later run can resume.

---

## Lane A: s2sdk (Linux dedicated server, Rust)

The Rust crates never contain a hand-written address, slot or offset. Everything comes from the
headers at the commit pinned in `crates/s2sdk/s2sdk-sys/UPSTREAM.lock`, and the bridge reads its slots
and offsets from the same headers (`bridge/s2sdk_gen.h`). The pin is **our fork branch**
`cs2mcp/main` (remote `https://github.com/personcopperfine/s2sdk.git`, the `.gitmodules` URL; the series of tool-class headers
that sits on `origin/cs2` until alliedmodders merges it), not `origin/cs2` itself. Your job is to (1) move
that branch onto newer upstream headers when they exist, (2) prove the result works in the real game, and (3) when the
headers are wrong for the new build, fix them locally and prepare the upstream PR.

### A1. Update the Linux game install (the `cs2-home` volume)

Only when no container is using the volume:

```sh
podman ps --filter volume=cs2-home --format '{{.Names}}'     # must print nothing; else STOP and ask
podman run --rm --network=host -v cs2-home:/home/steam --entrypoint steamcmd "$CS2_MCP_IMAGE" \
  +force_install_dir "/home/steam/steamapps/common/Counter-Strike Global Offensive" \
  +@sSteamCmdForcePlatformType linux +login anonymous +app_update 730 +quit
podman run --rm -v cs2-home:/home/steam --entrypoint cat "$CS2_MCP_IMAGE" \
  "/home/steam/steamapps/common/Counter-Strike Global Offensive/game/csgo/steam.inf"
```

The `ClientVersion` printed must equal `NEW`. (Same steamcmd flags as `cs2-docker/client/entrypoint.sh`
in the `cs2-docker` submodule; the image is built from `cs2-docker/client/Dockerfile`. CS2 is free to play, so anonymous login
works for the Linux game.) That entrypoint also re-validates the game by itself when the install is
more than 7 days old, so the volume can change without anyone running this step. Always read
`steam.inf` instead of assuming the volume's build.

The update also rewrote the files Metamod:Source and the bridge live in, so re-ensure the volume
before anything reads it (read-only while current; A1 already requires that no container mounts it):

```sh
./bridge/install-rs.sh --platform linux ensure
```

Then stage the Linux tree (the Linux `sdkcheck.py` row in A2b and the ELF entries in B3 read it):

```sh
python3 sigs/scripts/stagebins.py --linux --unshare \
  --game "$HOME/.local/share/containers/storage/volumes/cs2-home/_data/steamapps/common/Counter-Strike Global Offensive" \
  --out target/game-bins-linux-$NEW
python3 sigs/scripts/sigupdate.py check --platform linux --tree target/game-bins-linux-$NEW --build $NEW
```

A FAIL there is a Linux locator (no bridge entry pins Linux yet): fix it only when the binary
shows a plain cause (a masked operand that moved), and record it with `sigupdate.py record
--platform linux` in B3; otherwise list it in the report.

### A2. Rebase the fork branch onto the newest upstream headers

```sh
git -C crates/s2sdk/upstream fetch https://github.com/alliedmodders/s2sdk.git cs2:refs/remotes/origin/cs2
# (not `fetch origin`: in a worktree `origin` is the main checkout's local clone, which shows nothing new)
PIN=$(sed -n 's/^commit = "\(.*\)"/\1/p' crates/s2sdk/s2sdk-sys/UPSTREAM.lock)
git -C crates/s2sdk/upstream log --oneline $PIN..origin/cs2         # commits upstream gained since our base; empty = nothing new
git -C crates/s2sdk/upstream log --oneline origin/cs2..$PIN         # our series on top of origin/cs2 (47 commits at the time of writing)
```

(Once
alliedmodders merges the series, the second list is empty: check out `origin/cs2` and drop the
`# fork:` line from the lock.)

```sh
git -C crates/s2sdk/upstream remote get-url fork 2>/dev/null \
  || git -C crates/s2sdk/upstream remote add fork git@github.com:personcopperfine/s2sdk.git
git -C crates/s2sdk/upstream switch -C cs2mcp/tool-types-split $PIN   # a local branch at the pin
git -C crates/s2sdk/upstream rebase origin/cs2                      # expect conflicts in interfaces.h
# edit crates/s2sdk/s2sdk-sys/UPSTREAM.lock: commit = the new full sha, date = today (YYYY-MM-DD),
#   and the `# fork:` comment's base sha
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py
git diff --stat crates/s2sdk/s2sdk-sys/ir crates/s2sdk/s2sdk-sys/src/generated bridge/s2sdk_gen.h
cargo test -p s2sdk-sys -p s2sdk
cargo clippy --workspace --all-targets
```

- **Conflicts**: resolve only when both sides are plainly independent (an upstream addition next to
  ours). If upstream changed or renamed a class our series also touches, STOP: the series needs
  a person (its notes are in `workflows/upstream-prs/s2sdk-tool-types/`).
- **Do not push the rebased branch to `fork` unless the user says so.** Until it is pushed, the
  lock points at a commit that exists only on this machine: say so in the report.
- A rename upstream makes in a placeholder name the bridge spells (`unk3021`, `UndoGroup_t`, ...)
  breaks the bridge build with a compile error; fix the call sites, never add alias macros.
- Put the IR diff summary in the report: which records, slots or enums changed. A changed
  `itanium_slot` or field `offset` is the interesting part.
- `gen.py` fails with "a name that upstream dropped": remove that name from
  `crates/s2sdk/s2sdk-sys/gen/allowlist.toml` (and the matching `S2SDK_FORCE_*` row in
  `gen/probe.cpp`) **only if** `cargo build --workspace` still passes afterwards. If the safe layer
  (`crates/s2sdk/s2sdk/src`) uses the name, STOP: the wrapper needs a design change.
- Any other `gen.py` or test failure: STOP and paste the output into the report.

### A2b. The double-ABI check + the binary cross-check

```sh
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --check        # both ABIs: regenerate into target/ and
                                                     # fail on any diff (catches an upstream bump
                                                     # that only changes the MSVC packing)
python3 sigs/scripts/sdkcheck.py                     # generated numbers vs the Windows binaries
```

`sdkcheck.py` needs `target/game-bins` (lane B's B0 stages it). `bridge/build-rs.sh` does NOT run
it, so run it yourself before every build: an `sdk` entry has no compile-time tie any more, and the
`sdk:` row is the only thing that holds the header to the binary. The check must print `0 FAIL` on the build
`sigs/CURRENT_BUILD` pins:

- **FAIL on the `==` row or any `>=` row: STOP.** The upstream headers and the game disagree (a
  virtual added/removed, a vtable moved). That is the upstream-PR path: collect the evidence, do
  A5 (fix the header locally, prepare the PR); never edit generated files or sigs entries to make
  the check pass.
- **FAIL on an `sdk:<module>/<entry>` row**: the generated header and the binary measurement of that
  sigs entry disagree (the row names the `sdk` expression). There is no bridge pin to
  drift any more, only the header and the measurement. STOP and hand it to a person with IDA
  (`sigs/ida/`): the fix is the header (A5) **and** the sigs entry (B2) together, never one side to
  make the other pass. Fingerprints and `sigupdate.py --bridge-gate` must agree before anything is
  rebuilt.
- An upstream bump that only changes an MSVC slot the bridge consumes is invisible to the Rust
  tests; this is the check that catches it.

The Linux half of the check is `sdkcheck.py --platform linux --tree target/game-bins-linux-$NEW` (needs
the Linux tree, staged like A1's install copy): it compares the DB-measured Itanium VTLENs and the
entity-identity geometry with the IR (sigs/README.md "sdkcheck.py"). Rows come out SKIP naming the
missing fact until the DB carries them - only a FAIL stops, with the same upstream-PR path.

### A3. Prove it in the game

`A2b` must be green first: `sdkcheck.py` compares the generated numbers against the pinned
binaries before the live probe spends a container.

```sh
cargo test -p s2sdk-probe --test live -- --ignored --nocapture 2>&1 | tee target/probe-$NEW.log
grep -E '\[s2sdk-probe\] RESULT|artifacts dir|probe json' target/probe-$NEW.log
```

Needs `CS2_MCP_PROJECT_ROOT` from "Before you start". Pass = `RESULT ok stage=sdk` and the test passes. The test builds the payload itself
(`crates/s2sdk/docker/build-probe.sh`). The test starts exactly one dedicated-server
container, waits with its own deadline, and always removes the container. The `events` check is
allowed to fail (a known gap, see `crates/s2sdk/README.md` "Live findings"). Note in the report if it
starts passing.

### A4. If the probe fails: find out which header is wrong

The probe report (JSON, printed by the test) names the failing check. Match it here:

| Failing check | Usual cause on a new build | Evidence to collect (no IDA needed) |
|---|---|---|
| `schemasystem` (CreateInterface) | interface version string bumped (`SchemaSystem_001` → `_002`) | `vgrep 'SchemaSystem_[0-9]{3}' libschemasystem.so` (below) |
| `cvar` (`VEngineCvar007` not found) | same, for ICvar | `vgrep 'VEngineCvar[0-9]{3}' libtier0.so` |
| `schema_system` / `schema_scope` | an `ISchemaSystem` / `CSchemaSystemTypeScope` virtual was added or removed, so slots shifted | upstream commits since our pin touching `public/schemasystem/`; GameTracking-CS2 diff for this build |
| `schema_class` (wrong sizes, garbage names) | `SchemaClassInfoData_t` / `SchemaClassFieldData_t` layout changed | compare the probe's numbers with GameTracking-CS2 `DumpSource2/schemas/server/<Class>.h` for `NEW` |
| no `RESULT` line, server died | a call through a shifted slot crashed the game | `server.log` in the test's artifacts dir; last `[s2sdk-probe]` line names the step |

`vgrep` = look for a string in a game library inside the volume:

```sh
vgrep() { podman run --rm -v cs2-home:/home/steam --entrypoint grep "$CS2_MCP_IMAGE" -aoE "$1" \
  "/home/steam/steamapps/common/Counter-Strike Global Offensive/game/bin/linuxsteamrt64/$2" | sort -u; }
```

Then decide:

1. **Upstream already fixed it**: there is a newer commit on `origin/cs2`, or an open PR at
   `https://github.com/alliedmodders/s2sdk/pulls` (reading it with `gh pr list --repo alliedmodders/s2sdk`
   is fine). Pin that commit (A2) and rerun A3.
2. **A version string or a value you can read straight from the binary or GameTracking** (interface
   names, an enum value, a struct size GameTracking prints): fix it yourself in A5.
3. **A vtable slot or field offset that needs disassembly to prove**: STOP. Put the evidence you
   collected in the report and hand it to someone with IDA. Never guess a slot number.

### A5. Fix the header locally and prepare the upstream PR

The fix lives as a commit on a branch in the submodule, so the wrapper keeps generating from headers
and nobody hand-edits Rust. Branch off the fork branch that A2 left checked out (a moved tool-class
slot lands there like any other fix):

```sh
TOPIC=<short-kebab-name>                 # e.g. schemasystem-002
git -C crates/s2sdk/upstream switch -c cs2mcp/$NEW-$TOPIC
# edit the header(s) under crates/s2sdk/upstream/public/... : the smallest change that matches the evidence
git -C crates/s2sdk/upstream commit -am "<what changed in CS2 build $NEW, one line>"
# UPSTREAM.lock: commit = the new sha, branch = "cs2mcp/$NEW-$TOPIC", date = today, and add
#   local_patch = "workflows/upstream-prs/$TOPIC.md"   (gen.py ignores extra keys)
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py && cargo test -p s2sdk-sys -p s2sdk
# then A3 again: it must pass
mkdir -p workflows/upstream-prs
git -C crates/s2sdk/upstream format-patch -1 -o "$PWD/workflows/upstream-prs/"
```

Write `workflows/upstream-prs/$TOPIC.md`:

```markdown
# <PR title: "Update <Interface> for CS2 build <NEW>">

Target: alliedmodders/s2sdk, branch cs2. Patch: <the .patch file next to this one>.

## What changed in the game
<one paragraph: build NEW, which interface/struct, old vs new value>

## Evidence
<the exact commands and their output: strings, GameTracking file + line, probe JSON excerpt>

## How it was tested
cs2-mcp s2sdk-probe in a Linux dedicated server on build NEW: `RESULT ok stage=sdk` (log excerpt).
```

**Do not push the branch or open the PR.** That's the user's call. Until it's pushed to a fork,
the submodule commit exists only on this machine, so say that in the report. When upstream merges
the fix (or an equivalent one), do A2 again to go back to a plain upstream commit and delete
`local_patch` from the lock. A header fix for a slot or offset the bridge reads also needs the
sigs entry re-measured in lane B (B2); the `sdkcheck.py` row is green only when both agree.

### A6. Commit lane A

```sh
git add crates/s2sdk/upstream crates/s2sdk/s2sdk-sys bridge/s2sdk_gen.h bridge/s2sdk_gen_ties.h workflows/
git commit -m "s2sdk: CS2 $NEW, upstream <short sha>[, local patch $TOPIC]"
```

---

## Lane B: bridge (Windows tools under Wine)

### B0. Stage the binaries of the current build

The judge compares the new binaries with the ones the database is on, so those must be staged first
(`target/` is per worktree and starts empty):

```sh
python3 sigs/scripts/stagebins.py --game "$APP" --out target/game-bins
python3 sigs/scripts/sigcheck.py resolve --strict > target/resolve-$OLD.txt; echo "exit $?"
```

Every module must print `= build $OLD` and the exit status must be 0. If a module shows
`new (no recorded build)`, the Wine install was already updated outside this workflow and the old
binaries are gone: STOP and ask the user.

### B1. Download the new Windows binaries (nothing installed yet)

```sh
D=$CS2_UPDATE_DIR/$NEW; mkdir -p "$D/steamhome"
podman run --rm --network=host -e HOME=/steamhome -v "$D/steamhome:/steamhome" \
  --entrypoint steamcmd "$CS2_MCP_IMAGE" \
  +@sSteamCmdForcePlatformType windows +login anonymous +download_depot 730 2347771 +quit
```

Depot 2347771 (win64 game binaries) is anonymous. `hammer.dll` and the other tools are in depot
**2347779**, which needs a Steam account that owns CS2:

- If the user gave you credentials for this task (`STEAM_USERNAME` / `STEAM_PASSWORD` in your
  environment), run the same command with `+login "$STEAM_USERNAME" "$STEAM_PASSWORD"` and
  `+download_depot 730 2347779`. Never echo the credentials. If steamcmd asks for a Steam Guard
  code, STOP and ask the user.
- Otherwise skip it. B2 will report the tool DLLs as MISSING and the gate will stop you. Ask the
  user to run the download, then continue.

(The `workshop` submodule's `Dockerfile` does the same two downloads to build the tools image.)

### B2. Stage and judge

```sh
C=$(find "$D/steamhome" -type d -path '*/steamapps/content/app_730' | head -1)
ls "$C"                                   # depot_2347771 (and depot_2347779 if you could download it)
python3 sigs/scripts/stagebins.py --game "$C/depot_2347771" --game "$C/depot_2347779" \
  --out target/game-bins-$NEW
python3 sigs/scripts/sigupdate.py check --tree target/game-bins-$NEW --build $NEW \
  --bridge-gate --json target/sigupdate-$NEW.json | tee target/sigupdate-$NEW.txt
```

`stagebins.py` prints each module's hash and whether it's a build we already know. `sigupdate.py`
prints a line per module (`unchanged`/`CHANGED` with SAME/OK/DRIFT/FAIL/UNVERIFIED counts) and ends
with `BRIDGE GATE PASS` or `BRIDGE GATE FAIL` (exit 3). The `BRIDGE COVERAGE` line under it counts
the fingerprints the bridge entries require, how many are missing, and how many of those present are
*weak* (naming them: a weak fingerprint still matches the function the entry could be swapped with,
so a pure slot swap of one is only caught live or by a person — sigs/README.md "The bridge gate").

- **Gate FAIL: STOP.** Copy every line under `BRIDGE GATE:` into the report. Each names the
  entry, the bridge macro and its `via` hint. FAIL means the locator no longer matches (or an
  unchanged module's value moved, or a committed fingerprint does not match the build it was taken
  on). DRIFT means it matches, but the code there changed. UNVERIFIED means a bridge entry that
  pins code has no fingerprint in `sigs/fingerprints/`, so nothing proves the locator found the
  right function — including a locator (`vslot`) that cannot fail by itself: a moved vtable slot
  looks fine to the resolver, and without a fingerprint the gate could not tell. All three need a
  person with IDA (`sigs/ida/`). **Never pass `--accept-drift` for a bridge entry** (it cannot
  accept an UNVERIFIED entry either: the fix is to add the fingerprint on the *old* build with
  `sigs/ida/ida_fp.py`, commit it, and rerun B2). The Wine install hasn't been touched, so the old
  bridge keeps working on the old game.
- **A FAIL/DRIFT on a `vslot` entry that carries `sdk` is also a lane-A item.** The gate line names the
  macro; `python3 sigs/scripts/sdkadopt.py --list | grep <macro>` prints the generated expression the
  entry measures. The slot moved in the game, so the header (A5) and the entry's locator/`expect`
  change together; a person with IDA does both. Do not edit `sdk` to match the old number.
- **Gate PASS**: FAIL/DRIFT on entries without a bridge macro are fine. `record` marks them `stale`;
  list their count per module in the report.

### B3. Record the new build

```sh
python3 sigs/scripts/sigupdate.py record --tree target/game-bins-$NEW --build $NEW --bridge-gate
echo $NEW > sigs/CURRENT_BUILD
rm -rf target/game-bins && mv target/game-bins-$NEW target/game-bins
python3 sigs/scripts/sigcheck.py validate
python3 sigs/scripts/sigcheck.py resolve --strict > target/resolve-$NEW.txt; echo "exit $?"
grep -E '^  (FAIL|DIFF|REFUSE)' target/resolve-$NEW.txt     # must print nothing
```

The exit status must be 0. `STALE` lines are fine (entries the bridge doesn't use). Any `FAIL`
or `DIFF` line means STOP.

`sigupdate.py record` refuses to write anything while a bridge entry is not SAME/OK, whether or not
`--bridge-gate` was passed: pins.py only re-checks `builds[NEW].sha256`, so a recorded build with a
bad bridge entry would be pinned silently.

Don't touch `sigs/fingerprints/`. They stay at the build in `sigs/fingerprints/BUILD` until someone
regenerates them with IDA (`sigs/ida/ida_fp.py`) — a fingerprint remains valid across builds while
the function's code does. What the workflow does need is *coverage*: every bridge entry that pins
code must have one, and a new bridge entry (or a new `ident_slot`) without a fingerprint fails B2
with `UNVERIFIED` until someone with IDA runs `ida_fp.py` for that module on the **old** build and
commits the result. Write that build in the report.

### B4. Update the Wine install (this is the only step with downtime)

```sh
podman ps --filter name=cs2tools --format '{{.Names}}'   # must print nothing; else STOP and ask
cp "$APP/game/bin/win64/steam_api64.dll" "$D/steam_api64.dll.gbe"        # the Steam emulator's copy
podman run --rm --network=host -e HOME=/steamhome -v "$D/steamhome:/steamhome" -v "$APP:/app" \
  --entrypoint steamcmd "$CS2_MCP_IMAGE" \
  +@sSteamCmdForcePlatformType windows +force_install_dir /app +login anonymous +app_update 730 +quit
rsync -a "$C/depot_2347779/" "$APP/" --exclude='csgo_community_addons/' --exclude='game/bin/win64/steam_api64.dll'
cp "$D/steam_api64.dll.gbe" "$APP/game/bin/win64/steam_api64.dll"
sed -n 's/^ClientVersion=//p' "$APP/game/csgo/steam.inf"                  # must print $NEW
python3 sigs/scripts/stagebins.py --game "$APP" --out target/game-bins-check   # every module "= build $NEW"
rm -rf target/game-bins-check
```

- `app_update` updates the whole Windows game (binaries, content, `steam.inf`). The tools
  (depot 2347779) aren't part of it, which is why the rsync follows. If anonymous login is
  refused, use the account from B1 under the same rules, or STOP and ask.
- `app_update` puts Valve's `steam_api64.dll` back. The install runs the gbe_fork Steam emulator's
  copy, and Valve's DLL kills the game in `SteamAPI_Init`, so the saved copy is restored.
- If `steam.inf` or any `stagebins` line doesn't say `$NEW`, STOP.

### B5. Regenerate, build, install, test

```sh
python3 sigs/scripts/sdkcheck.py         # first: header vs the NEW binaries; 0 FAIL, else STOP (A2b)
python3 bridge/pins.py --rust crates/cs2-bridge/src/generated/pins.rs
                                         # rewrites bridge/bridge_pins.h AND the Rust pins the bridge
                                         # embeds; refuses if any `sdk` expression disagrees with
                                         # s2sdk_gen.h or anything else is inconsistent
git diff --stat bridge/bridge_pins.h crates/cs2-bridge/src/generated/pins.rs
                                         # only RVAs (and Qt chains, literals) move now; goes in the report
./bridge/build-rs.sh                     # runs `pins.py --check --rust` again and builds both targets
./bridge/install-rs.sh ensure            # the new cs2_bridge.dll + Metamod + the launcher files into
                                         # the Wine tools tree (ensure re-installs what is missing/stale;
                                         # read-only while current). No gameinfo.gi edit: the loader is
                                         # injected at launch (bridge/metamod/README.md "Launching")
./bridge/runtime.sh cycle                # up -> open map -> test_client
```

`bridge_pins.h` holds only measurements the SDK cannot give (170 `#define`s today:
`grep -c '^#define' bridge/bridge_pins.h`), so a normal update moves far fewer macros than it used
to. Slots and offsets do not appear in that diff at all: they move with `s2sdk_gen.h` (lane A).

`runtime.sh cycle` needs the host X display (Wine tools can't run headless yet). If it can't start,
the update is **built but not live-tested**: say exactly that in the report. Look at
`./bridge/runtime.sh logs` for `verify:` lines: every pinned module must be `verified`.

### B6. Metamod still loads

B5's `ensure` re-installs the Metamod layout, the launcher files and the bridge in the Wine tools
tree, and A1's `--platform linux ensure` does the same for `cs2-home`, so what remains is whether
the Metamod core still loads in this game build: B5's `./bridge/runtime.sh cycle` is the proof
(`runtime.sh up` starts `cs2mcp_launch.exe`, which injects the loader; a launcher/DLL failure is
exit 3 with `[cs2mcp-launch] FAIL: ...`, and a missing Linux loader fails the session's launch
script before the game starts). A Metamod/game
mismatch shows up there as a Metamod load failure in `console.log` (the core refuses
`metamod.2.cs2.dll`, or the plugin's factory), not as a bridge pin error: rebuild/refetch Metamod
per `bridge/metamod/README.md` when the game's Metamod-facing interfaces changed.

An abort with `MMS: Fatal error: Metamod:Source requires -dedicated or -insecure` is not a Metamod
build problem: the launch lost `-insecure`. Check the fixed flags in
`client-mcp/docker/linux-client.sh` (Linux client) and `bridge/runtime.sh` (Wine) and restore them,
do not drop `-insecure`.

`ensure` is read-only while the tree is current and repairs only what is missing/stale: exit 0 =
current or repaired, 1 = failure, 2 = usage, 4 = a container mounts the tree that needs repair
(stderr names it and what is stale). Stop that container (`./bridge/runtime.sh down`) and run
`ensure` again.

### B7. Commit lane B

```sh
git add sigs/ bridge/bridge_pins.h crates/cs2-bridge/src/generated/pins.rs workflows/updates/$NEW.md
git commit -m "sigs: CS2 $NEW (bridge gate pass, N entries stale), bridge pins regenerated"
```

---

## Report template (`workflows/updates/<NEW>.md`)

```markdown
# CS2 <NEW> (from <OLD>)

| | |
|---|---|
| Date | YYYY-MM-DD |
| Agent | <model> |
| PatchVersion | <from steam.inf> |
| Result | lane A: pass / stopped at A? / skipped · lane B: pass / stopped at B? / skipped |

## Lane A: s2sdk
- Upstream: <old sha> -> <new sha> (<n> commits) | unchanged
- IR changes: <records/slots/enums that changed, or "none">
- Probe: RESULT <ok|fail> stage=<…>; events: <ok|fail>
- Local patches: <none | TOPIC -> workflows/upstream-prs/TOPIC.md (not pushed)>
- Fork branch: <rebased onto origin/cs2 <sha>, pushed: yes/no (only if the user said so)>

## Lane B: bridge
- Changed modules: <list from sigupdate>
- Gate: PASS | FAIL (paste the gate lines)
- Stale non-bridge entries: <module: n, …>
- bridge_pins.h: <n> macros moved (RVAs only) · sdkcheck: <n> PASS / <n> SKIP / 0 FAIL
- Live: runtime.sh cycle PASS | not run (<why>)
- Fingerprints last regenerated on: <sigs/fingerprints/BUILD>

## Stopped on / needs a human
- <exact step, the command, its output, and what evidence you already collected>
```
