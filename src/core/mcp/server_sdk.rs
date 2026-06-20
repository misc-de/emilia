//! "Tokio SDK" MCP backend: the official `rmcp` server over Streamable HTTP.
//!
//! Runs an `axum` app on a dedicated tokio runtime thread; the MCP protocol
//! framing/sessions are handled by rmcp's [`StreamableHttpService`]. Our
//! [`EmiliaServer`] implements [`ServerHandler`] by delegating `list_tools` /
//! `call_tool` to the **same** shared [`tools`] layer the lean JSON-RPC backend
//! uses — so both backends expose an identical tool surface.
//!
//! Binds to `127.0.0.1` by default; "public" mode binds `0.0.0.0` and wraps the
//! listener in rustls TLS (reusing the device-sync identity). A bearer token is
//! always required, enforced by an axum middleware.

use std::net::TcpListener as StdTcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};

use super::{tools, McpContext, PORT};
use crate::core::sync::crypto;

/// Port fallbacks if the preferred one is taken.
const PORT_ATTEMPTS: u16 = 10;

/// One MCP session's server. Cheap to clone (just an `Arc`); the session manager
/// builds a fresh one per connection via the factory in [`start`].
#[derive(Clone)]
struct EmiliaServer {
    ctx: Arc<McpContext>,
}

impl ServerHandler for EmiliaServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // The shared registry already emits MCP-shaped tool descriptors
        // (camelCase `inputSchema`), so they deserialize straight into `Tool`.
        let tools: Vec<Tool> = serde_json::from_value(tools::tool_list()).unwrap_or_default();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        // `dispatch` does blocking SQLite I/O; run it on the blocking pool so it
        // never stalls this runtime's single worker thread (which also drives the
        // SSE response streams and keep-alives for every other live session).
        let ctx = self.ctx.clone();
        let name = request.name.to_string();
        let dispatched =
            tokio::task::spawn_blocking(move || tools::dispatch(&ctx, &name, &args)).await;
        match dispatched {
            Ok(Ok(value)) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "error: {e}"
            ))])),
            // The blocking task panicked or was cancelled — surface it as a tool error.
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "error: dispatch task failed: {e}"
            ))])),
        }
    }
}

/// Binds the server (with port fallback) and runs it on a dedicated tokio
/// runtime thread. Returns the bound port. The thread exits when `stop` is set.
pub fn start(
    ctx: Arc<McpContext>,
    token: String,
    public: bool,
    stop: Arc<AtomicBool>,
) -> Result<u16> {
    let bind_ip = if public { "0.0.0.0" } else { "127.0.0.1" };
    let mut bound: Option<(StdTcpListener, u16)> = None;
    let mut port = PORT;
    for _ in 0..PORT_ATTEMPTS {
        if let Ok(listener) = super::bind_reuse(bind_ip, port) {
            bound = Some((listener, port));
            break;
        }
        port = port.wrapping_add(1);
    }
    let (listener, port) = bound.ok_or_else(|| anyhow!("no free port for the MCP server"))?;
    // tokio's listener adoption requires a non-blocking socket.
    listener.set_nonblocking(true)?;

    let tls = if public {
        let identity = crypto::generate_identity()?;
        Some(crypto::server_config(&identity)?)
    } else {
        None
    };

    std::thread::spawn(move || {
        if let Err(e) = serve(ctx, token, listener, tls, stop) {
            tracing::error!("MCP SDK server stopped: {e}");
        }
    });
    Ok(port)
}

/// Builds the runtime + axum app and serves until the stop flag fires.
fn serve(
    ctx: Arc<McpContext>,
    token: String,
    listener: StdTcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let factory = move || Ok::<_, std::io::Error>(EmiliaServer { ctx: ctx.clone() });
        let service = StreamableHttpService::new(
            factory,
            Arc::new(LocalSessionManager::default()),
            Default::default(),
        );
        let app = Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(Arc::new(token), auth));

        // Drive a graceful shutdown from the stop flag.
        let handle = axum_server::Handle::new();
        {
            let handle = handle.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                handle.graceful_shutdown(Some(Duration::from_secs(1)));
            });
        }

        let make = app.into_make_service();
        match tls {
            Some(cfg) => {
                let rustls_cfg = axum_server::tls_rustls::RustlsConfig::from_config(cfg);
                axum_server::tls_rustls::from_tcp_rustls(listener, rustls_cfg)
                    .handle(handle)
                    .serve(make)
                    .await?;
            }
            None => {
                axum_server::from_tcp(listener)
                    .handle(handle)
                    .serve(make)
                    .await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

/// Bearer-token middleware: every request must carry `Authorization: Bearer …`.
async fn auth(
    State(token): State<Arc<String>>,
    req: Request,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let expected = format!("Bearer {token}");
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| crypto::constant_eq(v.trim(), &expected));
    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared registry must deserialize straight into rmcp's `Tool` (the one
    /// custom integration point — the camelCase `inputSchema` mapping). The
    /// dispatch path itself is covered by the shared `tools` tests.
    #[test]
    fn tool_list_deserializes_into_rmcp_tools() {
        let tools: Vec<Tool> =
            serde_json::from_value(tools::tool_list()).expect("rmcp Tool deserialization");
        assert!(tools.len() >= 10);
        assert!(tools.iter().any(|t| t.name == "search_library"));
        assert!(tools.iter().all(|t| !t.input_schema.is_empty()));
    }
}
