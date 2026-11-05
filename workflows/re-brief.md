# Briefing a reverse-engineering agent

An RE agent (Opus with ida-mcp) that starts cold spends most of its budget rediscovering the
project: where the IDBs are, how the fork is committed, how a header change reaches the bridge.
Whoever spawns one front-loads all of that in the prompt, so the agent opens only the files and
line ranges it is pointed at. Copy the skeleton below into the prompt, fill the `<...>` parts and
keep the stable facts as they are (update this file when one of them stops being true).

## What the spawner gathers first (cheap, before spawning)

- **Per item**: the current value and pin name (`bridge/bridge_pins.h`, `sigs/signatures/<module>.json`),
  every bridge site that uses it (`grep -n PIN bridge/*.c bridge/*.inc bridge/s2sdk.h`), the header
  and line where the class is or would be declared, and what earlier RE already found, as file +
  line ranges (`grep -n -i <class> target/orch/*/re/*.md target/orch/*/wp*-re.md`), including old
  function addresses when the build number is the same.
- **Decisions already taken** (e.g. "Qt widget chains stay sigs pins") and items out of scope.
- **Who else is working**, and which files the agent must not touch.
- **Stop conditions**: what the agent must not invent (names, types) and what counts as done.

## Skeleton

```
You are an RE worker for cs2-mcp. Everything you need is below; don't read broadly.

# Where you work
- Worktree <path>, branch <branch>, base <commit>. Only there.
- s2sdk submodule crates/s2sdk/upstream at <commit> (our fork series; examples of good commits:
  `git -C crates/s2sdk/upstream log --format='%h %an %s' <upstream base>..HEAD`).
- IDBs for build <build>: target/ida/win64-<build>/<module>.dll.i64 and
  target/ida/linux-<build>/lib<module>.so.i64 (main checkout's target/). hammer, modeldoc_editor,
  pet and toolframework2 are Windows-only. ONE IDB open at a time; close_idb before the next.
- Rules: AGENTS.md rules 1-9 (no podman except `podman ps`, no push, generated files never hand-edited,
  /tmp is RAM, poll don't sleep, STOP rather than guess).

# The items (in order; finish, verify and commit one before the next)
<n>. <Class / PIN = value> (<module>). Bridge use: <file:line>. Header: <file:line>.
   Known: <facts with addresses>. Notes: <file:lines>. Decide/stop if: <...>.

# Header rules (crates/s2sdk/upstream/AGENTS.md is binding; read its Verifying and Naming sections once)
- Every claim verified in the binary (vtables via strings/RTTI/asserts, sizes from allocation/ctor,
  members from the code that touches them); Linux too where the module exists. MSVC: one dtor slot,
  overload groups reversed.
- Engine names only when strings/RTTI/symbols give them; else unkXYZ / m_unkXYZ. Comments are caveats
  or `AMNOTE:`; never slot numbers, addresses, IDA names or how it was found.
  <If allowed for this task: unnamed classes as "AMNOTE: Name not known" placeholders, fork precedent
  MapEntityConnection_t.>
- One submodule commit per header, short subject naming the class, no body unless crediting a source,
  NO AI trailer. Author+committer personcopperfine:
  git -C crates/s2sdk/upstream -c user.name=personcopperfine \
      -c user.email=273385495+personcopperfine@users.noreply.github.com commit ...
  on a local branch (`git -C crates/s2sdk/upstream switch -c cs2mcp/<topic>`). Never push.
- Evidence (addresses, byte signatures, Windows/Linux, decisions) goes to <notes file under target/orch/>,
  never into headers.

# Header change -> bridge (per item)
1. Commit the header; set crates/s2sdk/s2sdk-sys/UPSTREAM.lock `commit =` to the submodule HEAD.
2. Add the class to crates/s2sdk/s2sdk-sys/gen/allowlist.toml next to its group's classes.
3. python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py; --check; (cd crates/s2sdk/s2sdk-sys/gen && python3 -I gen_selftest.py).
4. The new S2_OFF_/S2_SIZEOF_/S2_SLOT_ in bridge/s2sdk_gen.h must equal the pin.
5. python3 sigs/scripts/sdkadopt.py PIN=S2_EXPR (refuses if the binary measurement disagrees);
   the Rust bridge reads the generated s2sdk_sys value instead of the pin; python3 bridge/pins.py --rust crates/cs2-bridge/src/generated/pins.rs.
6. python3 sigs/scripts/sdkcheck.py (0 FAIL, the new sdk: row PASS) and --self-test;
   sigcheck.py validate, resolve --strict; bridge/test_*.py --static (no new failures).
7. ./bridge/build-rs.sh (the /Brepro byte-identical rename check was
   a C-bridge property; for the Rust bridge compare the pins diff and the export gate instead).
8. One worktree commit per item, ending with the Co-Authored-By line for your model.
Cargo only at the end, -j2, one command at a time, CARGO_TARGET_DIR=$PWD/target/cargo, and only if
`podman ps` shows no game (OOM guard).

# Report
Per item: done / stopped and why, submodule + worktree commits, names and their evidence, generated
value vs pin, sdkcheck row. Overall: `grep -c '^#define' bridge/bridge_pins.h`, sdkcheck totals,
DLL sha identical?, cargo results, judgement calls.
```

The WP7 brief of the de-hardcode project (`target/orch/dehardcode/`, notes in `wp7-re.md`) is a
worked example: six items, one Opus run, no rediscovery.
