# LOG
- STEP: list --full (read tool descriptions; server instructions text is not printed by list) - ok
- STEP: session_status - Hammer not loaded (hammer.dll not_loaded), no document, bridge m3.16, addon accept_p1
- ERROR: open_map maps/convoy_slice.vmap - AssetNotFound missing_host_file (file does not exist; no tool creates a .vmap)
- ERROR: create_block - NotImplemented, bridge does not advertise mesh.create (no Hammer loaded)
- ERROR: save_map - NotImplemented, bridge does not advertise map.save
- ERROR: run_map - VpkNotFound, never compiled
- GAP: need a way to create a new empty .vmap (or load Hammer with a new document). Tried open_map (needs existing file), create_block/save_map (capabilities missing). No tool offers new_map. Entire build/save/compile/play chain depends on it.
- CONFUSED: open_map description says on a fresh session the first map opens over vtm, but that requires an existing file; nothing says how to get one.
- DONE: stopped at step 1; the task cannot proceed through the MCP alone.
