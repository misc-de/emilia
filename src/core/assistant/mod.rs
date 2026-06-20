//! Embedded AI assistant.
//!
//! An in-process **agent loop** that connects an OpenAI-compatible LLM to
//! Emilia's existing MCP tool layer ([`crate::core::mcp::tools::dispatch`]).
//! Where [`crate::core::mcp`] lets an *external* host drive Emilia, this drives
//! an LLM *from inside* Emilia: the user types a task on a detail view (an
//! artist, album, concert, …) and the loop calls tools in-process to carry it
//! out — no network round-trip to reach the tools themselves.
//!
//! * [`llm`] — the OpenAI-compatible `chat/completions` client (MiniMax and any
//!   other OpenAI-shaped endpoint). Transport only; no tool logic.
//! * [`agent`] — the loop that drives the LLM through tool calls, running each
//!   via [`crate::core::mcp::tools::dispatch`] and feeding results back.

pub mod agent;
pub mod llm;

/// MiniMax's OpenAI-compatible API root. Used as the fixed endpoint for the
/// `minimax` provider preset, so the user only supplies a key and model.
pub const MINIMAX_BASE_URL: &str = "https://api.minimax.io/v1";
