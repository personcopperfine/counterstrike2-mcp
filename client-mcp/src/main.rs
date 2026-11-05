//! client-mcp binary: stdio MCP server. All logging goes to stderr; stdout is the protocol.
use clap::Parser;
use client_mcp::state::ClientConfig;

#[derive(Parser, Debug)]
#[command(
    name = "client-mcp",
    version,
    about = "MCP server (stdio) for the CS2 game client"
)]
struct Cli {
    #[command(flatten)]
    cfg: ClientConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cs2_mcp_common::init_tracing();
    let cli = Cli::parse();
    tracing::info!(runtime = ?cli.cfg.common.runtime, flavour = ?cli.cfg.flavour, discovery = %cli.cfg.common.discovery_path().display(), "client-mcp starting");
    if cli.cfg.common.launch {
        tracing::info!("--launch only controls startup auto-launch, which is not implemented; the launch_session tool needs no flag");
    }
    let (server, state) = client_mcp::build_server_with_state(&cli.cfg)?;
    // wine-tools sessions write their bridge discovery under the app dir; the bridge tools read --discovery
    let session_discovery = state.runtime.wine_paths().discovery;
    let bridge_discovery = cli.cfg.common.discovery_path();
    // (a Windows host has no wine-tools sessions: the discovery is the bridge's own %LOCALAPPDATA% file)
    if !cfg!(windows)
        && session_discovery
            .as_ref()
            .is_some_and(|d| *d != bridge_discovery)
    {
        tracing::warn!(
            discovery = %bridge_discovery.display(),
            session = ?session_discovery.as_ref().map(|d| d.display().to_string()),
            "--discovery is not where wine-tools sessions write the bridge's discovery file; launch_session's BridgeHello will not see it"
        );
    }
    let served = cs2_mcp_common::mcp::serve_stdio(server).await;
    client_mcp::shutdown(&state).await;
    served
}
