//! Embedded MCP (Model Context Protocol) server.
//!
//! Exposes Emilia's library and playback as MCP tools so an LLM host (Claude
//! Desktop, Claude Code, an agent) can query and control the app. Two
//! interchangeable transport backends call the **same** tool layer
//! ([`tools::dispatch`]):
//!
//! * [`server_jsonrpc`] — a lean, tokio-free JSON-RPC 2.0 server, a near-copy of
//!   the device-sync HTTP server ([`crate::core::sync::server`]). Default on
//!   aarch64 (phones).
//! * `server_sdk` (rmcp/tokio) — the official SDK on its own runtime thread.
//!   Default on desktop architectures. *(Added in a later step.)*
//!
//! Reads run on a fresh [`Library`](crate::core::db::Library) connection per
//! request (WAL — safe alongside the running UI). Writes/playback are forwarded
//! as a UI-agnostic [`McpCommand`] through a control sink the UI installs at
//! startup, keeping this module free of any GTK/relm4 dependency.

pub mod command;
pub mod http;
pub mod jobs;
pub mod protocol;
pub mod server_jsonrpc;
pub mod server_sdk;
pub mod state;
pub mod tools;

pub use command::{ControlFn, McpCommand};
pub use state::{new_handle, NowPlayingHandle};

/// Preferred TCP port (next to the sync server's 8765).
pub const PORT: u16 = 8770;

/// Binds a TCP listener with `SO_REUSEADDR` set. Without it, a freshly
/// restarted Emilia would find its previous port still lingering in `TIME_WAIT`
/// and skip to the next one (8770 → 8771 → …), leaving the configured MCP
/// client pointing at the wrong port. `SO_REUSEADDR` lets the restart reclaim
/// the *same* port immediately, so it stays deterministic across restarts.
pub fn bind_reuse(ip: &str, port: u16) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Socket, Type};
    let addr: std::net::SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let sock = Socket::new(domain, Type::STREAM, None)?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    sock.listen(128)?;
    Ok(sock.into())
}

/// Which MCP backend (if any) serves requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpMode {
    /// No server running. The default — the MCP server is strictly opt-in and
    /// never starts on its own; the user picks a backend in the settings.
    #[default]
    Off,
    /// Lean self-built JSON-RPC backend (tokio-free).
    JsonRpc,
    /// rmcp/tokio SDK backend.
    Sdk,
}

impl McpMode {
    /// Parse the persisted `mcp_mode` setting; unknown/missing → `Off` (opt-in).
    pub fn from_setting(s: &str) -> Self {
        match s {
            "jsonrpc" => Self::JsonRpc,
            "sdk" => Self::Sdk,
            _ => Self::Off,
        }
    }

    /// The string stored in the `mcp_mode` setting.
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::JsonRpc => "jsonrpc",
            Self::Sdk => "sdk",
        }
    }
}

/// Everything a tool needs at request time: a readable now-playing snapshot and
/// a control sink into the UI. The library is opened per request inside
/// [`tools::dispatch`], so the context itself stays `Send + Sync` (required by
/// the tokio backend).
pub struct McpContext {
    pub now: NowPlayingHandle,
    pub control: ControlFn,
    /// Registry of long-running background jobs (downloads).
    pub jobs: std::sync::Arc<jobs::Jobs>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_roundtrips() {
        for m in [McpMode::Off, McpMode::JsonRpc, McpMode::Sdk] {
            assert_eq!(McpMode::from_setting(m.as_setting()), m);
        }
    }

    #[test]
    fn unknown_setting_falls_back_to_off() {
        assert_eq!(McpMode::from_setting("garbage"), McpMode::Off);
        assert_eq!(McpMode::default(), McpMode::Off);
    }
}
