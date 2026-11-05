//! MCP transport layer: stdio JSON-RPC via `rmcp`, adapted onto [`crate::tools::Registry`].
//!
//! Boundary rule: nothing outside this module imports `rmcp`. The registry is plain Rust so tools
//! are unit-testable without a transport, and an rmcp API change touches one file. stdout belongs
//! to the protocol; all logging goes to stderr (`tracing`).
//!
//! Mapping (same as the POC server): a registered tool's failure is a tool result with
//! `isError: true` whose single text block is `{"error": <payload>}`; an **unknown tool** is a
//! JSON-RPC `-32602` error carrying `data:{type:"UnknownTool", didYouMean?:[...]}`; a successful call is a text block with the JSON result plus one
//! `image` block per PNG.
//!
//! Prompts: a server may attach a [`PromptSet`] ([`McpServer::with_prompts`]); `prompts/list` and
//! `prompts/get` then answer from it (one user message holding the rendered text) and the
//! `prompts` capability is advertised. Without a set the capability is absent.
use crate::prompts::PromptSet;
use crate::tools::{Budget, CallFailure, Mutability, Registry, ToolSpec, ToolState};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, InitializeResult, ListPromptsResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, Role,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ServerIdentity {
    pub name: &'static str,
    pub version: &'static str,
    pub instructions: String,
}

pub struct McpServer<S: ToolState> {
    registry: Arc<Registry<S>>,
    state: S,
    identity: ServerIdentity,
    tool_budget_ms: u64,
    deadline_ms: u64,
    prompts: PromptSet,
}

impl<S: ToolState> McpServer<S> {
    pub fn new(
        registry: Registry<S>,
        state: S,
        identity: ServerIdentity,
        tool_budget_ms: u64,
        deadline_ms: u64,
    ) -> Self {
        McpServer {
            registry: Arc::new(registry),
            state,
            identity,
            tool_budget_ms,
            deadline_ms,
            prompts: PromptSet::new(),
        }
    }

    /// Offer these prompts over `prompts/list` / `prompts/get`.
    pub fn with_prompts(mut self, prompts: PromptSet) -> Self {
        self.prompts = prompts;
        self
    }

    pub fn prompts(&self) -> &PromptSet {
        &self.prompts
    }

    pub fn registry(&self) -> &Registry<S> {
        &self.registry
    }

    /// The server state, for a caller that must act on it **after** the transport is done: the
    /// binary's shutdown cleanup (a compile job this server left running in a container would
    /// otherwise be orphaned) clones it before `serve_stdio` consumes the server.
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Transport-free call (what `tools/call` does), for tests and embedding.
    pub async fn call(
        &self,
        name: &str,
        args: Value,
    ) -> Result<crate::tools::ToolOutput, CallFailure> {
        self.registry
            .call(
                &self.state,
                name,
                args,
                Budget::new(self.tool_budget_ms, self.deadline_ms),
            )
            .await
    }

    /// The `tools` array exactly as `tools/list` serializes it (for size guards).
    pub fn tools_list_wire(&self) -> Vec<Value> {
        self.tool_list()
            .iter()
            .map(|t| serde_json::to_value(t).expect("tool serializes"))
            .collect()
    }

    fn tool_list(&self) -> Vec<Tool> {
        self.registry
            .describe()
            .into_iter()
            .map(|s| {
                let schema = s.input_schema.as_object().cloned().unwrap_or_default();
                Tool::new(s.name, s.description, Arc::new(schema)).with_annotations(annotations(s))
            })
            .collect()
    }
}

/// MCP annotations from the tool's policy: ReadOnly -> `readOnlyHint`; Mutating -> live,
/// undoable edits (not destructive); Writer -> writes files (destructive). Nothing is open-world.
///
/// Only hints that differ from the MCP defaults are worth the bytes, so the defaults are left out:
/// `readOnlyHint:false`, `destructiveHint:true`, `idempotentHint:false`, and the destructive /
/// idempotent hints of a read-only tool (they have no meaning there). Emitted: `openWorldHint:false`
/// always, `readOnlyHint:true`, and on a non-read-only tool `destructiveHint:false` and
/// `idempotentHint:true`.
fn annotations(s: &ToolSpec) -> ToolAnnotations {
    let (read_only, destructive) = match s.mutability {
        Mutability::ReadOnly => (true, false),
        Mutability::Mutating => (false, false),
        Mutability::Writer => (false, true),
    };
    let read_only = s.read_only.unwrap_or(read_only);
    let destructive = s.destructive.unwrap_or(destructive);
    let mut a = ToolAnnotations::new().open_world(false);
    if read_only {
        return a.read_only(true);
    }
    if !destructive {
        a = a.destructive(false);
    }
    if s.idempotent == Some(true) {
        a = a.idempotent(true);
    }
    a
}

impl<S: ToolState> ServerHandler for McpServer<S> {
    fn get_info(&self) -> ServerConfig {
        let caps = if self.prompts.is_empty() {
            ServerCapabilities::builder().enable_tools().build()
        } else {
            ServerCapabilities::builder().enable_tools().enable_prompts().build()
        };
        let mut info = InitializeResult::new(caps);
        let mut imp = Implementation::default();
        imp.name = self.identity.name.to_string();
        imp.version = self.identity.version.to_string();
        info.server_info = imp;
        info.instructions = Some(self.identity.instructions.clone());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_list()))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let prompts = self
            .prompts
            .list()
            .iter()
            .map(|p| {
                let args = p
                    .arguments
                    .iter()
                    .map(|a| {
                        PromptArgument::new(a.name)
                            .with_description(a.description)
                            .with_required(a.required)
                    })
                    .collect::<Vec<_>>();
                Prompt::new(p.name, Some(p.description), (!args.is_empty()).then_some(args))
            })
            .collect();
        Ok(ListPromptsResult::with_all_items(prompts))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        let args = request
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, v)
            })
            .collect();
        match self.prompts.render(&request.name, &args) {
            Ok(text) => {
                let description = self
                    .prompts
                    .list()
                    .iter()
                    .find(|p| p.name == request.name)
                    .map(|p| p.description);
                let mut result =
                    GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
                result.description = description.map(str::to_string);
                Ok(result.into())
            }
            Err(e) => Err(McpError::invalid_params(e.to_string(), None)),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        let name = request.name.to_string();
        tracing::debug!(tool = %name, "tools/call");
        match self.call(&name, args).await {
            Ok(out) => {
                let mut content = vec![ContentBlock::text(out.json.to_string())];
                for img in out.images {
                    content.push(ContentBlock::image(base64_encode(&img.bytes), img.mime));
                }
                Ok(CallToolResult::success(content).into())
            }
            Err(CallFailure::UnknownTool(n)) => {
                let near = self.registry.did_you_mean(&n);
                let hint = if near.is_empty() {
                    String::new()
                } else {
                    format!(" (did you mean {}?)", near.join(", "))
                };
                let mut data = json!({"type": "UnknownTool", "name": "UnknownTool"});
                if !near.is_empty() {
                    data["didYouMean"] = json!(near);
                }
                Err(McpError::invalid_params(
                    format!("Unknown tool: {n}{hint}"),
                    Some(data),
                ))
            }
            Err(CallFailure::Tool(e)) => {
                tracing::info!(tool = %name, error = %e, "tool error");
                Ok(CallToolResult::error(vec![ContentBlock::text(
                    json!({"error": e.to_json()}).to_string(),
                )])
                .into())
            }
        }
    }
}

/// Serve on stdio until the client closes the pipe.
pub async fn serve_stdio<S: ToolState>(server: McpServer<S>) -> anyhow::Result<()> {
    let running = server
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP initialize failed: {e}"))?;
    let reason = running.waiting().await?;
    tracing::info!(?reason, "MCP transport closed");
    Ok(())
}

/// The process-termination signals a stdio server is killed with (the client closing the pipe is
/// the transport's own path: [`serve_stdio`] returns by itself). A binary races this against
/// `serve_stdio` when it has post-exit work to do — the hammer-mcp shutdown cleanup ends the
/// container-side process tree of a still-running compile job, which would otherwise outlive the
/// process with nobody watching it and no owner for its `CS2MCP_COMPILE_JOB=<jobId>` tag.
///
/// The signal `termination_signal` saw, in the process-reporting convention (review-2 N7):
/// `SIGTERM` -> 15, `SIGINT` -> 2, `0` when none fired yet (a transport end, or signals could
/// not be registered). A stdio server that exits on a signal leaves the conventional
/// `128 + <number>` status so a supervisor can see that a signal got it.
pub fn termination_signal_number() -> i32 {
    TERMINATION_SIGNAL.load(std::sync::atomic::Ordering::SeqCst)
}

static TERMINATION_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Never returns. On a platform without these signals (or when they cannot be registered) it
/// waits forever, so a `select!` against it keeps the transport as the only terminator.
pub async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM: the shutdown cleanup runs on stdio EOF only");
                std::future::pending::<()>().await;
                return;
            }
        };
        let mut interrupt = signal(SignalKind::interrupt()).ok();
        match interrupt.as_mut() {
            Some(interrupt) => tokio::select! {
                _ = terminate.recv() => {
                    TERMINATION_SIGNAL.store(15, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!("SIGTERM received");
                }
                _ = interrupt.recv() => {
                    TERMINATION_SIGNAL.store(2, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!("SIGINT received");
                }
            },
            None => {
                terminate.recv().await;
                TERMINATION_SIGNAL.store(15, std::sync::atomic::Ordering::SeqCst);
                tracing::info!("SIGTERM received");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        TERMINATION_SIGNAL.store(2, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("interrupt received");
    }
}

/// Standard base64 (RFC 4648, padded) — small enough not to warrant a dependency.
pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::annotations;
    use crate::tools::{Mutability, ToolSpec};
    use serde_json::{json, Value};

    fn ann(m: Mutability, f: impl FnOnce(ToolSpec) -> ToolSpec) -> Value {
        serde_json::to_value(annotations(&f(ToolSpec::new("t", "d", m, &[])))).unwrap()
    }

    #[test]
    fn annotations_emit_only_non_default_hints() {
        let same = |s: ToolSpec| s;
        assert_eq!(ann(Mutability::ReadOnly, same), json!({"readOnlyHint": true, "openWorldHint": false}));
        assert_eq!(ann(Mutability::Mutating, same), json!({"destructiveHint": false, "openWorldHint": false}));
        assert_eq!(ann(Mutability::Writer, same), json!({"openWorldHint": false}));
        // idempotent:true is kept on a writing tool, idempotent:false is the default and omitted
        assert_eq!(
            ann(Mutability::Mutating, |s| s.idempotent(true)),
            json!({"destructiveHint": false, "idempotentHint": true, "openWorldHint": false})
        );
        assert_eq!(ann(Mutability::Writer, |s| s.idempotent(true)), json!({"idempotentHint": true, "openWorldHint": false}));
        assert_eq!(ann(Mutability::Mutating, |s| s.idempotent(false)), json!({"destructiveHint": false, "openWorldHint": false}));
        // overrides: destructive:true is the default (omitted), destructive:false is kept
        assert_eq!(ann(Mutability::Mutating, |s| s.destructive(true)), json!({"openWorldHint": false}));
        assert_eq!(ann(Mutability::Writer, |s| s.destructive(false)), json!({"destructiveHint": false, "openWorldHint": false}));
        // a read-only tool carries no destructive/idempotent hint, whatever was asked
        assert_eq!(
            ann(Mutability::ReadOnly, |s| s.idempotent(true).destructive(false)),
            json!({"readOnlyHint": true, "openWorldHint": false})
        );
        assert_eq!(
            ann(Mutability::Writer, |s| s.read_only(true).idempotent(true)),
            json!({"readOnlyHint": true, "openWorldHint": false})
        );
        // read_only(false) on a ReadOnly-by-policy tool is the default: omitted, then the rest applies
        assert_eq!(ann(Mutability::ReadOnly, |s| s.read_only(false)), json!({"destructiveHint": false, "openWorldHint": false}));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(super::base64_encode(b""), "");
        assert_eq!(super::base64_encode(b"f"), "Zg==");
        assert_eq!(super::base64_encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
