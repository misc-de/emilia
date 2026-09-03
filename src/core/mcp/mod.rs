//! Embedded MCP (Model Context Protocol) server.
//!
//! Exposes Emilia's library and playback as MCP tools so an LLM host (Claude
//! Desktop, Claude Code, an agent) can query and control the app. Two
//! interchangeable transport backends call the **same** tool layer
//! ([`tools::dispatch`]):
//!
//! * [`server_jsonrpc`] — a lean, tokio-free JSON-RPC 2.0 server on the same
//!   blocking HTTP helpers as the device-sync server ([`crate::core::http`]).
//!   Default on aarch64 (phones).
//! * `server_sdk` (rmcp/tokio) — the official SDK on its own runtime thread.
//!   Only compiled with the `mcp-sdk` cargo feature (on by default; phone
//!   builds leave it out — see [`SDK_AVAILABLE`]).
//!
//! Reads run on a fresh [`Library`](crate::core::db::Library) connection per
//! request (WAL — safe alongside the running UI). Writes/playback are forwarded
//! as a UI-agnostic [`McpCommand`] through a control sink the UI installs at
//! startup, keeping this module free of any GTK/relm4 dependency.

pub mod command;
pub mod jobs;
pub mod protocol;
pub mod server_jsonrpc;
#[cfg(feature = "mcp-sdk")]
pub mod server_sdk;
pub mod state;
pub mod tools;

pub use command::{ControlFn, McpCommand};
pub use state::{new_handle, new_sync_handle, NowPlayingHandle, SyncStateHandle};

/// Preferred TCP port (next to the sync server's 8765).
pub const PORT: u16 = 8770;

/// Whether the rmcp/tokio SDK backend is part of this build (`mcp-sdk`
/// feature). Builds without it — the phone Flatpak — offer only the lean
/// JSON-RPC backend, and a persisted `"sdk"` choice degrades to it.
pub const SDK_AVAILABLE: bool = cfg!(feature = "mcp-sdk");

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
    /// `"sdk"` in a build without the SDK backend degrades to `JsonRpc`, so the
    /// server still comes up instead of silently staying off.
    pub fn from_setting(s: &str) -> Self {
        match s {
            "jsonrpc" => Self::JsonRpc,
            "sdk" if SDK_AVAILABLE => Self::Sdk,
            "sdk" => Self::JsonRpc,
            _ => Self::Off,
        }
    }

    /// The backends this build can offer, in settings order.
    pub fn selectable() -> &'static [McpMode] {
        if SDK_AVAILABLE {
            &[Self::Off, Self::JsonRpc, Self::Sdk]
        } else {
            &[Self::Off, Self::JsonRpc]
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
    /// Device-sync state the `sync_*` tools report and gate on.
    pub sync: SyncStateHandle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_roundtrips() {
        for m in [McpMode::Off, McpMode::JsonRpc] {
            assert_eq!(McpMode::from_setting(m.as_setting()), m);
        }
        // "sdk" only survives the round trip when the backend is compiled in;
        // otherwise it degrades to the lean backend rather than to "off".
        let expected = if SDK_AVAILABLE {
            McpMode::Sdk
        } else {
            McpMode::JsonRpc
        };
        assert_eq!(McpMode::from_setting(McpMode::Sdk.as_setting()), expected);
    }

    #[test]
    fn selectable_matches_build() {
        let modes = McpMode::selectable();
        assert_eq!(modes[0], McpMode::Off);
        assert_eq!(modes.contains(&McpMode::Sdk), SDK_AVAILABLE);
    }

    #[test]
    fn unknown_setting_falls_back_to_off() {
        assert_eq!(McpMode::from_setting("garbage"), McpMode::Off);
        assert_eq!(McpMode::default(), McpMode::Off);
    }
}
