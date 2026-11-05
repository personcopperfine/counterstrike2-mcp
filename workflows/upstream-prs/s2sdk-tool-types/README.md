# s2sdk tool-types: draft upstream PR(s) for alliedmodders/s2sdk (branch cs2)

Published as 47 cherry-pickable commits on `git@github.com:personcopperfine/s2sdk.git` branch
`cs2mcp/tool-types-split` (`cs2mcp/tool-types` holds the same tree squashed into one commit).

47 patches on alliedmodders/s2sdk `cs2` at `ab23a7e3`. They declare every class that cs2-mcp's
bridge pins (`crates/cs2-bridge/src/generated/pins.rs`), verified against CS2 2000930 (win64 and linuxsteamrt64).
Rebased from `e7086d0f` onto `89096c5d` for CS2 2000933 (2026-10-19): upstream obsoleted
X360 code and deleted `mathlib/vertexcolor.h`, so 0003 drops its X360 branch, 0007 drops the
unused include and 0008 takes `const Color &` for `modulationColor` (same 4-byte RGBA by reference,
no slot or layout change).
Rebased from `89096c5d` onto `ab23a7e3` for CS2 2000940 (2026-11-03): 0007 keeps upstream's new
`ActiveLoop_t` struct next to its `HSceneViewRenderTarget` change, 0010 is down to the two derived
destructors (upstream added `IEngineService`'s own), 0009 adds the 4-byte `m_unk201` the game
inserted at `SosStartSoundEventParams_t+0x1C` in 2000940 (later members +4, sizeof 0x30), 0031
re-diffed only. The patches carry no author line. Apply them as commits authored by your own git identity:

```sh
git checkout -b <branch> ab23a7e3
sh /path/to/workflows/upstream-prs/s2sdk-tool-types/apply.sh
```

The resulting tree is identical to the tree cs2-mcp's `crates/s2sdk/s2sdk-sys/UPSTREAM.lock`
was generated from. `NOTES.md` lists the points an upstream reviewer is likely to raise. Put
them in the PR description(s).

## Suggested split (one PR per group, in this order; a group may depend on earlier groups, never on later ones)

| PR | Patches | Contents |
|---|---|---|
| 1 engine/tier0 | 0001-0013 | ITestScriptMgr, IToolFramework2, CFrustum/Camera_t fix, buffer handles, ImageFormat, IScreenshotCallback fix, scenesystem, ISceneUtils, soundsystem, IEngineService dtor fix, IRenderService, IInputService, IVEngineClient2 |
| 2 rendersystem/particles | 0014-0018 | render resource handles, CStrongHandle, IRenderDevice, IParticleFunctionDefinition, IParticleSystemMgr |
| 3 client | 0019-0020 | CClientInput, CCSGOInput |
| 4 tools (Windows-only) | 0021-0028, 0032-0033 | toolutils2 bases, pet, ModelDoc |
| 5 enum fixes | 0029-0031 | EAttributeDataType, ButtonCode_t, MAX_JOYSTICKS |
| 6 hammer (Windows-only) | 0034-0047 | datamodel, mapdoclib, hammer |

Patches that port Wend4r/sourcesdk headers carry `Co-authored-by: Wend4r` (and boeing666 where
they co-authored). No patch has an AI trailer.

Upstream AGENTS.md rules followed: `unkXYZ` names, `AMNOTE:` comments, no addresses/indices, one
concern per commit, Valve paths, no static_asserts.
