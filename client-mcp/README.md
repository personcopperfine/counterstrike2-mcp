# client-mcp

Stdio MCP server (Rust) for the CS2 **game client**: in-game unit tests (engine `-testscript`
runner), multiplayer sessions, CS script / VScript test maps, demo recording -> video, and AI
playtesting through a usercmd-level input hook, with screenshots + OCR for the model.

Read [AGENT.md](AGENT.md) first (two session flavours: Wine tools with the bridge vs. headless
Linux client without it), then the numbered [workflows/](workflows/). Status: scaffold.

```sh
cargo build -p client-mcp && cargo test -p client-mcp
RUST_LOG=info target/debug/client-mcp --project-root ..
```

Builds on `../idabins/worknotes/vtest/` (suites, gametest runner, input hook test, movie demo)
and the headless client image (`cs2-docker/client/`, published as `ghcr.io/personcopperfine/cs2-docker`).
