# Protocol changes in m3.17: `job.start` accepts `.js`

Bridge build `m3.17` adds no method and no param (branch `streamline/cs-script`).

`job.start`'s input extension allowlist (`crates/cs2-bridge/src/ops/job.rs` `INPUT_EXTENSIONS`) gains
`js`: `resourcecompiler -i <content>/csgo_addons/<addon>/maps/scripts/<name>.js` builds the
`.vjs_c` a `point_script` loads (hammer-mcp `cs_script {action:"compile"}`, `compile_asset`).
Everything else is unchanged (same path rules, same flags). Capability: none to gate on;
`hello.build` is `m3.17`. Against an older bridge a `.js` input fails with
`input extension must be one of: ...` (hammer-mcp adds "install m3.17").

Test: `crates/cs2-bridge/tests/job.rs` `input_validation_refuses_every_trick` (accepts `.js`, still
refuses `.vjs_c` and `.ts`). Not run live.
