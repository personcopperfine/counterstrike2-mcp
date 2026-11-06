# Releasing cs2-mcp

A release is a tag `vX.Y.Z` (or `vX.Y.Z-<suffix>` for a pre-release). Pushing it runs
`.github/workflows/release.yml`, which builds every download and creates a **draft** GitHub
Release. Nothing is published until the maintainer publishes the draft.

| file | built by |
|---|---|
| `cs2mcp-vX.Y.Z-windows-x86_64.msi` | package-windows: WiX 3 through cargo-packager, per user (no UAC) |
| `cs2mcp-vX.Y.Z-windows-x86_64.zip` | build-linux: `package.sh --os windows --archive` (cross-built with xwin + rust-lld) |
| `cs2mcp-vX.Y.Z-linux-x86_64.AppImage` | package-linux: cargo-packager, `opt/cs2mcp` checked against `SHA256SUMS` |
| `cs2mcp-vX.Y.Z-linux-x86_64.tar.gz` | build-linux: `package.sh --os linux --archive` (ubuntu-22.04, glibc <= 2.35) |
| `cs2mcp-vX.Y.Z-linux-amd64.deb` | package-linux: cargo-packager |
| `BUILDINFO-windows.txt`, `BUILDINFO-linux.txt` | the packages' own `BUILDINFO.txt` |
| `SHA256SUMS` | release: every file above |

The release notes are written from the two BUILDINFO files
(`crates/cs2-launcher/packaging/release-notes.py`): the target game build, the pins gate, the git
sha and toolchain, gbe_fork, and Metamod (win64: the official dev build's version, URL and
SHA-256; linux: the metamod-source commit and the hl2sdk commit it was built against). They say
that nothing is code-signed and that the game always runs with `-insecure`.

## 1. Before tagging: the checks a runner cannot do

A runner has no game binaries, so the release workflow builds the bridge with
`bridge/build-rs.sh --no-pins-check`, and every BUILDINFO.txt says `pins gate SKIPPED`. The pins
and signatures are held to the game **here, by you, on the commit you are about to tag**, with
the game binaries in `target/game-bins` and `target/game-bins-linux-<build>` (`sigs/README.md`,
"Binaries"):

```sh
git status                                       # clean; the commit you will tag
cat sigs/CURRENT_BUILD                           # the game build the release targets: today's?
git submodule update --init crates/s2sdk/upstream crates/metamod/upstream workshop
git -C crates/metamod/upstream submodule update --init third_party/khook
cargo clippy --workspace --all-targets           # no warnings
cargo test  --workspace
cargo check -p s2sdk-sys -p s2sdk -p cs2-bridge -p cs2-native -p cs2-launcher --target x86_64-pc-windows-msvc
cargo clippy -p cs2-bridge -p cs2-native -p cs2-launcher --all-targets --target x86_64-pc-windows-msvc -- -D warnings
python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --check
python3 sigs/scripts/sdkcheck.py                 # needs target/game-bins
python3 sigs/scripts/sigcheck.py validate
python3 sigs/scripts/sigcheck.py resolve --strict   # needs target/game-bins
./bridge/build-rs.sh                             # WITH the pins gate: "pins gate: passed"
```

These are the AGENTS.md checks (plus cs2-native and cs2-launcher in the win64 ones). Every one
must pass. If CS2 updated since `sigs/CURRENT_BUILD`, do the game update first
(`workflows/game-update.md`); a release for an old build is still possible, but its users get a
`buildMismatch` warning against the current game.

## 2. Optional: a dry run

GitHub → Actions → **release** → *Run workflow* on the branch. It builds everything under the
version `v0.0.0-dryrun.<run number>` and uploads it as workflow artifacts
(`release-<version>` holds the assets and `notes.md`); it creates no release. Its input
`metamod_win64_sha256` pins the Metamod win64 download for that run.

## 3. Tag

The tag is pushed by you, never by an agent (AGENTS.md rule 4):

```sh
git tag -a v0.1.0 -m "cs2-mcp v0.1.0"
git push origin v0.1.0
```

`v0.1.0-rc.1`-style tags are marked as pre-releases. The MSI's version is the tag's `x.y.z`
(an MSI version is numbers only), so `v0.1.0-rc.1` and `v0.1.0` install as the same MSI version;
the MSI allows same-version upgrades, so installing the final over the rc works.

Metamod for Windows is the newest AlliedModders dev build at build time, on purpose
(`bridge/metamod/fetch-metamod-win64.sh`); the notes record which one. To hold a release to the
build you tested, set the repository variable `MM_WIN64_SHA256` (Settings → Secrets and
variables → Actions → Variables) to that archive's SHA-256 (the dry run's BUILDINFO-windows.txt
has it): a different build then fails the release instead of shipping. Clear the variable again
afterwards, or it fails the next release once AlliedModders publishes a newer build.

## 4. Review the draft, then publish

The workflow ends with a draft at GitHub → Releases. Before *Publish release*:

* `sha256sum -c SHA256SUMS` over the downloaded files;
* the notes: the game build, the `pins gate SKIPPED` line (expected: you ran the checks in
  step 1), the Metamod win64 version (is it the one you tested?);
* Windows: install the MSI as a normal user (no UAC prompt), start cs2-mcp from the Start menu,
  Prepare + Launch, Stop, check the Status says the install is not prepared; uninstall it from
  *Settings → Apps*. SmartScreen warns about an unsigned installer: that is expected;
* Linux: `chmod +x` the AppImage and start it; `<AppImage> cs2mcp-native discover`.

A broken draft: delete the draft (and, if the commit was wrong, the tag: `git push origin
:refs/tags/v0.1.0`), fix, and tag again. A failed job can be re-run from the workflow page;
the draft step only runs when all builds passed.

## What CI checks on a pull request

`.github/workflows/ci.yml`: clippy (`-D warnings`) and tests for the workspace, the win64
`cargo check` and clippy (cs2-bridge, cs2-native, cs2-launcher), `gen.py --check` (clang 21
from apt.llvm.org, the xwin sysroot cached on `XWIN_VERSION`), and `sigcheck.py validate`.
`sdkcheck.py`, `sigcheck.py resolve --strict` and the pins gate need the game binaries: step 1.

## Versions pinned in the workflows

Rust 1.99.0, xwin 0.10.0, cargo-packager 0.11.8, every action by commit SHA (the tag in a
comment). cargo-packager downloads WiX 3.11 (hash-checked) and linuxdeploy; the
linuxdeploy-plugin-appimage it uses is its `continuous` tag, which is not pinned.
