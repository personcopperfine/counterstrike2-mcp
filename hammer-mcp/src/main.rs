//! hammer-mcp binary: stdio MCP server. All logging goes to stderr; stdout is the protocol.
use clap::Parser;
use cs2_mcp_common::config::CommonConfig;

#[derive(Parser, Debug)]
#[command(
    name = "hammer-mcp",
    version,
    about = "MCP server (stdio) for the CS2 Workshop Tools"
)]
struct Cli {
    #[command(flatten)]
    common: CommonConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cs2_mcp_common::init_tracing();
    let cli = Cli::parse();
    tracing::info!(runtime = ?cli.common.runtime, discovery = %cli.common.discovery_path().display(), "hammer-mcp starting");
    if cli.common.launch {
        tracing::warn!("--launch is accepted but the docker runtime backend is not implemented yet (workflows/01-docker-runtime.md); attach-only");
    }
    let server = hammer_mcp::build_server(&cli.common)?;
    // The state must outlive the transport: the shutdown cleanup below reads the compile jobs
    // (and the runtime) from it after `serve_stdio` has consumed the server.
    let state = server.state().clone();
    // The transport ends on the client's stdin EOF; a SIGTERM/SIGINT kill is the other way this
    // process goes away, and both must reach the same cleanup below.
    let end = tokio::select! {
        result = cs2_mcp_common::mcp::serve_stdio(server) => End::Transport(result),
        _ = cs2_mcp_common::mcp::termination_signal() => End::Signal,
    };
    // A compile job still `running` here would keep its compiler alive in the container after this
    // process is gone: nobody watches it, `CS2MCP_COMPILE_JOB=<tag>` has no owner left, and the
    // next server cannot tell the orphan from a live compile. End this server's own tagged process
    // trees first — bounded, so a hung podman cannot hold the exit. (The cleanup also flags the
    // store "shutting down", review-2 N7: a start that raced the shutdown is refused instead of
    // being orphaned by the exit below.)
    let cleanup = hammer_mcp::shutdown_cleanup(&state).await;
    tracing::info!(cleanup = %cleanup, "shutdown: compile-job cleanup");
    match end {
        // the transport's own end (stdin EOF / the client closing the pipe): the process exits
        // like any other one, reporting what `serve_stdio` returned
        End::Transport(result) => result,
        // A signal: the running runtime's drop waits for the blocking stdin reader rmcp started,
        // which is still parked in a read while the client is alive — so the cleanup above would
        // be the last thing that ever happens. Leave through `exit` instead; the cleanup (the
        // whole point of handling the signal at all) has already run and been logged. The status
        // is the conventional `128 + <signal>` (SIGTERM -> 143, SIGINT -> 130), so a supervisor
        // can see that a signal got the process and not a clean end (review-2 N7).
        End::Signal => {
            let signal = cs2_mcp_common::mcp::termination_signal_number();
            let exit_code = if signal > 0 { 128 + signal } else { 128 + 15 };
            std::process::exit(exit_code);
        }
    }
}

/// How `main`'s transport ended: the client closed the pipe, or the process was signalled.
enum End {
    Transport(anyhow::Result<()>),
    Signal,
}
