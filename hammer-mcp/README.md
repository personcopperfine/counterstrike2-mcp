# hammer-mcp

Stdio MCP server (Rust) for the CS2 **workshop tools** (Hammer, ModelDoc, pet, asset browser; SFM
and the workshop manager later), driving a live `cs2.exe -tools` session under Proton in a podman
container through the in-process bridge DLL from `bridge/`.

Read [AGENT.md](AGENT.md) first (purpose, architecture, conventions), then the numbered
[workflows/](workflows/). Status: scaffold — nothing works yet.

```sh
cargo build -p hammer-mcp && cargo test -p hammer-mcp
RUST_LOG=info target/debug/hammer-mcp --project-root ..
```

Ports the Python POC in `../idabins/worknotes/mcpserver/` (24 tools, no-auto-save, modal gate,
error taxonomy, fixture-driven contract tests) to Rust and extends it to the other tools.
