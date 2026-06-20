//! Minimal JSON-RPC 2.0 + MCP wire types, shared by the backends.
//!
//! MCP rides on JSON-RPC 2.0. The self-built backend (`server_jsonrpc`) speaks
//! this directly; the rmcp/tokio backend reuses only the tool registry/dispatch
//! ([`super::tools`]) and lets the SDK own the framing. Kept deliberately tiny —
//! only the methods we actually serve (`initialize`, `tools/list`, `tools/call`,
//! `ping`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC version string echoed in every response.
pub const JSONRPC_VERSION: &str = "2.0";
/// MCP protocol revision we advertise in `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// Standard JSON-RPC error codes we use.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;

/// An incoming JSON-RPC request (or notification, when `id` is absent).
#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[serde(default)]
    pub jsonrpc: String,
    /// Absent for notifications — those get no response.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// A JSON-RPC response: exactly one of `result` / `error` is set.
#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    /// A success response carrying `result`.
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response.
    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}
