# ti_convoy: hacks and workarounds to clean up (FIXME)

Every stopgap taken while building ti_convoy goes here: what, why, and the proper fix.

## 2026-10-21

1. **Worktree sessions get the main checkout's `cs2-hammer` (`--addon mcp_test`).** Claude Code
   matched the session in this worktree to the main `cs2-mcp` project entry, so the
   worktree's own `~/.claude.json` entry (with `--addon ti_convoy`) was ignored.
   Workaround: `.mcp.json` in this worktree adds a separately named server `cs2-hammer-ti`
   (`--addon ti_convoy`); it is picked up only after a session restart.
   Proper fix: per-call addon (or follow the active document's addon) in hammer-mcp, or make the
   addon switchable at runtime, so one server serves any addon.
2. **Killed this session's `cs2-hammer` (mcp_test) process** (pid 1052059) so the bridge's single
   client slot was free for raw JSON-RPC. The `cs2-hammer` tools are gone for the rest of that
   session. Proper fix: as in item 1; restarting the session restores both servers.
3. **`target/ti_convoy/tc.py`**: a throwaway helper (console commands over VToolMessage via
   `vtmsend.exe`, waits on `game/csgo/console.log`, raw bridge RPC). It duplicates
   `~/.tmp/defuse-poc/playtest/pt_lib.py` and the hammer-mcp `run_map`/console paths.
   Proper fix: use `cs2-hammer-ti` (`run_map`, live entity tools) and drop the script. Missing in
   hammer-mcp: a console command tool for the tools session (send a command plus read back the new
   console lines). `console.exec` needs `-testscript`.
4. **No console readback for cvar queries**: `mp_c4timer` / `cvarlist x` sent over VToolMessage
   print nothing to `console.log`. S5 was proven indirectly (game.rules `roundTimeSec`, and a cheat
   command being refused under `sv_cheats 0`). Proper fix: a bridge cvar read (`cvar.get`).
5. **Tools-session sv_cheats is on by default**: the RegisterCheatCommand test commands
   (`ti_state`, `ti_win`, `ti_ending`) work there but not on a normal server. After a
   `sv_cheats 0` test, put it back to `sv_cheats 1`.
6. **The tools session loads Metamod** (runtime.sh always does). The entry must not depend on it
   (judging rule). Before handoff, check the map + script once in a session without Metamod
   (or in the plain client).
7. **`target/ti_convoy/vpkls.py`**: a 20-line VPK v2 directory lister to find model paths
   (hammer-mcp `list_models` was unavailable, see item 2). Proper fix: `list_models` via `cs2-hammer-ti`.
8. **The WP0 spike rig is built over raw RPC** (`target/ti_convoy/spike_build.py`): the hull
   extended west to x=-2560, a 3-node test track `cv_01`→`cv_mid`→`cv_02` (+ alt `cv_alt`), a box
   train `convoy_car` with a parented `convoy_shell` truck, 2 T spawns (priority -1) inside it.
   It's temporary and gets replaced by WP1–WP3. Delete it (`spike_nodes.json` lists the node IDs)
   before the layout.
9. **`sync.sh` has no script-only push**: `push` also copies the repo `.vmap` over the workshop one
   (it would clobber Hammer saves not yet pulled). Workaround: copy `ti_convoy.js` to the workshop
   by hand. Proper fix: `sync.sh push-script`.
10. **Release checklist**: `CFG.debug` in `script/src/ti_convoy/config.ts` is true while testing (it
    logs every vehicle bullet impact and C4 drop). Set it false for the Workshop build.
11. **Spike renders are outside the repo**: `workshop/run/app/renders/ti_s4_*.png`, `ti_s1_jump.png`
    (evidence for TODO.md S1/S4). Copy them under the branch if they are needed later.
12. **`materials/dev/cobblestone_001.vmat` renders magenta** (missing) and `dev_water2_cheap` has no
    `g_tColor`: the greybox uses `blockout/tiles_01a` and `colorcode_cyan02` instead. Real materials
    come with the dressing.
13. **Layout meshes were pasted from `layout.py` output into hammer-mcp calls by hand** (the bridge
    has one client, held by `cs2-hammer-ti`). Proper fix: a hammer-mcp "apply layout file" tool, or
    a batch create_mesh from a JSON path.
14. **hammer-mcp `compile_map` reports a good compile as `failed`**: c2 printed
    `OK: 29 compiled, 0 failed, 1 skipped, 9m:35s` and wrote the vpk, but the job state is `failed`
    ("an OK-shaped line was seen but its `compiled/failed/skipped` summary could not be parsed",
    probably the `9m:35s` minutes form). Workaround: read `okLine`/`vpkUpdated` yourself. Proper fix:
    the hammer-mcp OK-line parser (not this branch).
15. **The no-VIS iteration compile runs outside hammer-mcp**: `ADDON=ti_convoy EXTRA_FLAGS="-world
    -phys -bakelighting -nav" ~/.tmp/defuse-poc/compile_map.sh ti_convoy` (`compile_map` has only
    full/nav modes). It `pkill`s resourcecompiler.exe in the container when done, so never run it
    alongside another compile. Proper fix: a `mode: "novis"` (or a stage list) in hammer-mcp
    `compile_map`. Release builds use the full compile.
16. ~~**`sync.sh script` doesn't copy the repo js**~~: fixed, `sync.sh build` builds the TS and
    copies the js; `sync.sh script` does that and then compiles it.
17. **Debug-only script bits**: the cheat commands `ti_who`, `ti_c4`, `ti_state`, `ti_win`,
    `ti_ending`, `ti_enter <veh> [bots]`, `ti_exit`, `ti_damage <veh> <hp>` (index.ts). They're
    registered with RegisterCheatCommand, so they're inert without sv_cheats; keep or drop for release.
18. **`site_a`'s entity origin is still the spike rig's (256, 640, 32)**; its tied volume is right
    (x 3250…3800, y 800…1300). Tidy it in Hammer when touching the site (cosmetic).

19. **The script is TypeScript on Source2ZE/cs_script** (`script/`, rollup, @s2ze/* 2.23.1 from npm).
    `addon/content/maps/scripts/ti_convoy.js` is the rollup output and is committed because the
    addon ships it. A reference clone of the upstream repo sits at `cs2-mcp/target/ext/cs_script`
    (1efee7c), outside git. The rollup build warns about circular imports inside @s2ze/math: upstream,
    harmless.
20. **hammer-mcp additions on this branch** (commit 6f0db76: `game_local` with origin-step jitter
    stats, `input_set`, `input_clear`; `compile_map` mode `novis`; the fix for 14). They belong on
    main as their own change, not with the map. `.mcp.json` here points `cs2-hammer-ti` at a
    separate build, `cs2-mcp/target/hammer-ti/release/hammer-mcp` (built with
    `CARGO_TARGET_DIR=target/hammer-ti`), so the main checkout's binary is untouched. Repoint it
    when this lands on main. Console lines still go through `target/ti_convoy/tc.py`
    (VToolMessage via `vtmsend.exe`): hammer-mcp forbids a console tool by design.
21. **Vehicle models are stock props, no van**: car_a (+ `car_a_details` wheels, `car_a_glass`,
    parented `<veh>_parts`), `de_tides/truck`, `utility_truck` (+ `_windows`). The game ships no van
    body (only `van_glass`), so the "van" is the pickup/truck with exposed bed seats. car_a and
    utility_truck face -x: `modelYaw` 180 in vehicles.ts. Seats were placed from side renders, not
    measured on the mesh.
22. **`target/ti_convoy/drive.py` + `tc.py` drive the live tests** (throwaway, outside git): W held,
    `setang` toward waypoints from `ti_state`. It overshoots the last waypoint. `ti_tp <veh> x y
    [yaw]` (cheat) moves a vehicle for tests.
23. **Rider pose**: riders are MoveType WALK pawns standing on the road inside their car (eyes
    above the roof line; leaning bots show from the waist up). Stuck in the hull, they play the
    in-air animation. Cosmetic; a crouch/sit pose would need something the script API doesn't offer.
24. **`weapon_air_spread_scale 0` server-wide** (index.ts `applyCvars`): riders are stuck in their
    car's bullet hull, so the game counts them as in the air (TODO.md "Car cover"). Side effect:
    jump-shooting is as accurate as standing for everyone. The user chose this over script-computed
    cover. Proper fix: a collision layer that bullets hit and player movement ignores (none found).
25. **Compiles go through `~/.tmp/defuse-poc/compile_map.sh`** (`ADDON=ti_convoy EXTRA_FLAGS="-world
    -phys -bakelighting -nav"`, ~3 min) because hammer-mcp `compile_map` refuses `UnsavedChanges`:
    Hammer's dirty flag stays set after a save (save_map then reports `skippedClean:true`). One
    compile at 07:1x failed in vrad3 ("Failed to create device") and cs2.exe exited 1 after a
    dialog; `bridge/runtime.sh up` + `open maps/ti_convoy.vmap` brought it back and the next compile
    passed.
26. **Test-input quirks** (hammer-mcp `input_set`): release + tap in one call doesn't release (use
    two calls); while an `angles` override is set, console `setang` is ignored; console
    `+attack;-attack` sent back to back often fires nothing (hold until the impact line).
27. **More throwaway helpers in `target/ti_convoy/`** (outside git): `gl_summary.py` (game_local
    samples), `spread3.py` (seated first-shot spread; needs a temporary impact log in index.ts),
    `hullshot.py` (on-foot shots at each hull), `setup_seat.sh`, `abtest*.py/sh`, `vpkx.py` (extract
    one file from a vpk). `drive.py` reads the `hull` field `ti_state` now prints.
28. **Ending test drivers** (throwaway, `target/ti_convoy/`): `endtest.py <case>` (plant-explode,
    plant-defuse, hp-hold, hp-contested, timeout-plant, timeout-hp) and `c4ride.py`. They need the
    cheat-only `ti_bring <t|ct> x y` (index.ts), which teleports a team's bots; it stays in the
    script as a test cheat. `package planted` / `package defused` / `exfil: the chopper is inbound` /
    `chopper landed` log lines exist for these tests. endtest.py's plant regex misses the lowercase
    `package planted` (prints `plant: None`; the round end is still right).
29. **Chopper and floor-A by hand coordinates** (map): the chopper is greybox meshes at fixed spots on
    the pad and the floor A is one 128 u face placed on the overlay material's world-aligned tile grid
    (no UV control in hammer-mcp; moving the face off the 128 grid splits the letter). Replace the
    chopper with a model and the A with a proper overlay/decal when there is time.
30. **Traffic shortcuts** (traffic.ts): one generic car size (`trafficLength`/`trafficHalfWidth`) for
    braking and hits whatever the model; the car's height comes from the path_tracks (no ground
    trace), so terrain between nodes isn't followed; obstacles are checked by footprint maths, not
    traces (a wall across a route doesn't stop a car). The greybox `wb` route starts in view at
    x 2450 (pop-in near the fork). Fine for the greybox; the real map's routes start out of sight.
31. **Particles ship loose** (`game/csgo_addons/ti_convoy/particles/ti_convoy/*.vpcf_c`), not packed in
    the map vpk: the Workshop upload of the addon folder has to carry them. hammer-mcp's
    `particle_compile` builds only the file it's given (not `m_Children`), so the folder was compiled
    with `resourcecompiler -r -i ".../particles/ti_convoy/*.vpcf"` in the tools container (13 compiled).
    Also `ent_fire X DestroyImmediately; ent_fire X Start` in one command line left the system off
    (the Start was dropped); the script never does both on one entity in one tick.
32. **Respawn convars change elimination** (index.ts `applyCvars`, `CFG.respawn`): with
    `mp_respawn_on_death_*` on, wiping a team no longer ends the round; only the endings and the round
    timer do. Decide whether that's the design or count the waiting pilots as dead for the win check.
33. **Greybox drone and pilot box** (map): the drone is a func_brush quadcopter from `layout/drone.py`
    (no model, no animated rotors, no sound); the pilots sit in a sealed greybox box outside the map at
    x 5900..6700, which the real map has to keep out of view and out of reach.
34. **Drone camera and spot visibility unverified** (drone.ts): the follow camera, the red glow and the
    team-only radar markers were only checked through script state and logs, not from a client's view
    (the tools session can't capture frames and holds one player). Check in a real client before
    shipping. A world render from the camera pose (2026-10-22) shows the drone covering the
    lower middle of the view (`droneCameraOffset` z 28 is too low); raise it or pitch the view down.
35. **`ti_drone` test cheat**: it strips the human's weapons where they stand, so a carried C4 drops
    there; and a script hot reload forgets the pooled drone/marker clones (they stay disabled at the
    parking spot until the next map load). Development only.
