//! Lean, tokio-free JSON-RPC 2.0 / MCP backend.
//!
//! A near-copy of the device-sync server ([`crate::core::sync::server`]): a
//! blocking accept loop in its own thread on a non-blocking [`TcpListener`],
//! polling the stop flag. Each connection serves one request and closes. The
//! whole MCP surface is a single `POST` endpoint carrying a JSON-RPC request;
//! the shared [`tools::handle_rpc`] does the work.
//!
//! Binds to `127.0.0.1` by default (local hosts only); "public" mode binds to
//! `0.0.0.0` and wraps every connection in rustls TLS (the same maintained 0.23
//! stack as sync). A bearer token is always required.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use super::http::{self, HttpReq};
use super::protocol::{RpcResponse, PARSE_ERROR};
use super::{tools, McpContext, PORT};
use crate::core::sync::crypto;

/// Accept loop: maximum blocking time, after which the stop flag is re-checked.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Per-connection read/write timeout, so a slow/stuck peer cannot pin the thread.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap for request bodies (JSON-RPC requests are small).
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Port fallbacks if the preferred one is taken.
const PORT_ATTEMPTS: u16 = 10;

/// A running JSON-RPC server bound to a port, optionally TLS-wrapped.
pub struct JsonRpcServer {
    listener: TcpListener,
    /// `Some` in public (LAN) mode — every connection is TLS-wrapped.
    tls: Option<Arc<rustls::ServerConfig>>,
    token: String,
    ctx: Arc<McpContext>,
    stop: Arc<AtomicBool>,
    port: u16,
}

impl JsonRpcServer {
    /// Binds the server (with port fallback). `public` selects the bind address
    /// and whether TLS is used. Not yet serving — see [`Self::run`].
    pub fn start(
        ctx: Arc<McpContext>,
        token: String,
        public: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        let bind_ip = if public { "0.0.0.0" } else { "127.0.0.1" };
        let mut bound: Option<(TcpListener, u16)> = None;
        let mut port = PORT;
        for _ in 0..PORT_ATTEMPTS {
            match super::bind_reuse(bind_ip, port) {
                Ok(listener) => {
                    bound = Some((listener, port));
                    break;
                }
                Err(_) => port = port.wrapping_add(1),
            }
        }
        let (listener, port) = bound.ok_or_else(|| anyhow!("no free port for the MCP server"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| anyhow!("listener setup failed: {e}"))?;

        let tls = if public {
            let identity = crypto::generate_identity()?;
            Some(crypto::server_config(&identity)?)
        } else {
            None
        };

        Ok(Self {
            listener,
            tls,
            token,
            ctx,
            stop,
            port,
        })
    }

    /// Test server on localhost with an OS-assigned free port (plain HTTP).
    #[cfg(test)]
    fn start_for_test(ctx: Arc<McpContext>, token: String, stop: Arc<AtomicBool>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| anyhow!("test listener bind failed: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            tls: None,
            token,
            ctx,
            stop,
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Blocking accept loop. Returns when the stop flag is set or the listener
    /// errors. Each connection is handled inline (requests are tiny and quick).
    pub fn run(self) {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            match self.listener.accept() {
                Ok((sock, _addr)) => self.serve_connection(sock),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => break,
            }
        }
    }

    /// Optionally TLS-wraps one accepted socket and serves a single request.
    fn serve_connection(&self, mut sock: TcpStream) {
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
        let _ = sock.set_write_timeout(Some(IO_TIMEOUT));

        if let Some(tls) = &self.tls {
            let mut conn = match rustls::ServerConnection::new(tls.clone()) {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut stream = rustls::Stream::new(&mut conn, &mut sock);
            self.handle(&mut stream);
            conn.send_close_notify();
            let _ = conn.complete_io(&mut sock);
        } else {
            self.handle(&mut sock);
        }
    }

    /// Reads one request, authenticates, runs the JSON-RPC method, replies.
    fn handle<S: Read + Write>(&self, stream: &mut S) {
        let mut req = match http::read_head(stream) {
            Ok(r) => r,
            Err(_) => return,
        };
        if http::read_body_fully(stream, &mut req, MAX_BODY).is_err() {
            return;
        }

        // CORS preflight and an unauthenticated health probe are handled before
        // the bearer check; everything else requires the token.
        if req.method == "OPTIONS" {
            http::write_status(stream, 204);
            return;
        }
        if req.method == "GET" && req.path == "/health" {
            http::write_json(stream, 200, &serde_json::json!({ "ok": true }));
            return;
        }
        if !self.bearer_ok(&req) {
            http::write_status(stream, 401);
            return;
        }

        let reply = match serde_json::from_slice(&req.body) {
            Ok(rpc) => tools::handle_rpc(&self.ctx, rpc),
            Err(_) => Some(RpcResponse::error(
                None,
                PARSE_ERROR,
                "invalid JSON-RPC request",
            )),
        };
        match reply {
            // A request → its response. A notification → 202 with no body.
            Some(resp) => http::write_json(stream, 200, &resp),
            None => http::write_status(stream, 202),
        }
    }

    fn bearer_ok(&self, req: &HttpReq) -> bool {
        let expected = format!("Bearer {}", self.token);
        req.header("Authorization")
            .is_some_and(|v| crypto::constant_eq(v.trim(), &expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::{command::McpCommand, state, McpContext};
    use std::sync::Mutex;

    fn test_ctx() -> (Arc<McpContext>, Arc<Mutex<Vec<McpCommand>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let ctx = Arc::new(McpContext {
            now: state::new_handle(),
            control: Arc::new(move |c| sink.lock().unwrap().push(c)),
        });
        (ctx, log)
    }

    /// End-to-end over a real TCP socket: initialize, tools/list, and a
    /// tools/call that maps to a playback command. Also checks bearer auth.
    #[test]
    fn jsonrpc_roundtrip_over_tcp() {
        let (ctx, log) = test_ctx();
        let stop = Arc::new(AtomicBool::new(false));
        let server =
            JsonRpcServer::start_for_test(ctx, "secret-token".into(), stop.clone()).expect("start");
        let port = server.port();
        let handle = std::thread::spawn(move || server.run());

        let base = format!("http://127.0.0.1:{port}/mcp");
        let auth = "Bearer secret-token";

        // Missing token → 401.
        let unauth = ureq::post(&base)
            .send_json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }));
        assert!(
            matches!(unauth, Err(ureq::Error::Status(401, _))),
            "missing bearer must be rejected"
        );

        // initialize.
        let init: serde_json::Value = ureq::post(&base)
            .set("Authorization", auth)
            .send_json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
            .expect("initialize ok")
            .into_json()
            .unwrap();
        assert!(init["result"]["serverInfo"]["name"] == "emilia");

        // tools/list returns a non-empty array.
        let list: serde_json::Value = ureq::post(&base)
            .set("Authorization", auth)
            .send_json(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .expect("list ok")
            .into_json()
            .unwrap();
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 10);

        // tools/call playback_control → recorded as McpCommand::Next.
        let call: serde_json::Value = ureq::post(&base)
            .set("Authorization", auth)
            .send_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "playback_control", "arguments": { "action": "next" } }
            }))
            .expect("call ok")
            .into_json()
            .unwrap();
        assert_eq!(call["result"]["isError"], serde_json::json!(false));
        assert_eq!(log.lock().unwrap().as_slice(), &[McpCommand::Next]);

        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
}
