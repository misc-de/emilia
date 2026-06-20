//! The agent loop: drives an LLM through tool calls against Emilia's shared MCP
//! tool layer ([`crate::core::mcp::tools`]), in-process.
//!
//! One [`Agent::run`] turn sends the conversation to the model, runs whatever
//! tools it asks for via [`tools::dispatch`], feeds the results back, and loops
//! until the model replies with plain text (no more tool calls) — or a step
//! ceiling is hit. The tool surface is exactly what the MCP server exposes, so
//! the assistant can do everything an external host could, without a round-trip.
//!
//! **Destructive tools** (`delete_*`) are gated: the loop asks a [`ConfirmFn`]
//! before running one, and only then injects the `"confirm": true` the dispatch
//! layer requires. The safe default (used in tests) declines them.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use super::llm::{LlmClient, Message, ToolCall, ToolDef};
use crate::core::mcp::{tools, McpContext};

/// Tools that mutate irreversibly; each is `require_confirm`-gated in dispatch.
const DESTRUCTIVE: &[&str] = &["delete_playlist", "delete_memo", "delete_recording"];

/// Max LLM round-trips per turn before the loop gives up — a backstop against a
/// model that keeps calling tools without ever settling on a final answer.
const MAX_STEPS: usize = 12;

/// Asked before a destructive tool runs: `(tool_name, args) -> proceed?`. The UI
/// wires this to a confirmation dialog; headless callers pass a denying closure.
pub type ConfirmFn = Arc<dyn Fn(&str, &Value) -> bool + Send + Sync>;

/// A confirm function that declines every destructive action (the safe default).
pub fn deny_destructive() -> ConfirmFn {
    Arc::new(|_, _| false)
}

/// Builds the LLM tool list from the shared MCP registry, mapping each
/// `{ name, description, inputSchema }` descriptor into OpenAI function shape.
pub fn tools_for_llm() -> Vec<ToolDef> {
    let list = tools::tool_list_enabled();
    let Some(arr) = list.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let desc = t
                .get("description")
                .and_then(|d| d.as_str())
                .map(str::to_owned);
            let params = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            Some(ToolDef::function(name, desc, params))
        })
        .collect()
}

/// Applies the destructive-tool policy to a pending call. Returns the args to
/// dispatch (with `"confirm": true` injected for an approved destructive tool),
/// or `Err(message)` to hand straight back to the model when the user declined.
fn apply_policy(
    name: &str,
    mut args: Value,
    confirm: &ConfirmFn,
) -> std::result::Result<Value, String> {
    if DESTRUCTIVE.contains(&name) {
        if !confirm(name, &args) {
            return Err(format!(
                "declined: the user did not confirm the destructive action '{name}'"
            ));
        }
        if let Value::Object(map) = &mut args {
            map.insert("confirm".into(), Value::Bool(true));
        }
    }
    Ok(args)
}

/// An assistant tied to one LLM endpoint and the in-process tool context.
pub struct Agent {
    client: LlmClient,
    ctx: Arc<McpContext>,
    confirm: ConfirmFn,
    tools: Vec<ToolDef>,
}

impl Agent {
    pub fn new(client: LlmClient, ctx: Arc<McpContext>, confirm: ConfirmFn) -> Self {
        Self {
            client,
            ctx,
            confirm,
            tools: tools_for_llm(),
        }
    }

    /// Runs the loop on `history` (already seeded with system + user turns),
    /// appending every assistant/tool turn in place so the caller keeps the full
    /// transcript for a continuing chat. Returns the model's final text reply.
    pub fn run(&self, history: &mut Vec<Message>) -> Result<String> {
        for _ in 0..MAX_STEPS {
            let reply = self.client.complete(history, &self.tools)?;
            history.push(reply.clone());

            let calls = match reply.tool_calls {
                Some(c) if !c.is_empty() => c,
                // No tool calls → this is the final answer for the turn.
                _ => return Ok(reply.content.unwrap_or_default()),
            };

            for call in &calls {
                let result = self.run_tool(call);
                history.push(Message::tool(&call.id, &call.function.name, result));
            }
        }
        Ok("(stopped: reached the maximum number of tool steps for one turn)".into())
    }

    /// Runs one tool call, returning the textual result to feed back to the model
    /// (tool errors are reported as text, never as a hard failure of the loop).
    fn run_tool(&self, call: &ToolCall) -> String {
        let name = call.function.name.as_str();
        let args = match call.parse_arguments() {
            Ok(a) => a,
            Err(e) => return format!("error: could not parse tool arguments: {e}"),
        };
        let args = match apply_policy(name, args, &self.confirm) {
            Ok(a) => a,
            Err(decline) => return decline,
        };
        match tools::dispatch(&self.ctx, name, &args) {
            Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()),
            Err(e) => format!("error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_mapping_covers_registry() {
        let tools = tools_for_llm();
        assert!(
            tools.len() >= 10,
            "expected the full registry, got {}",
            tools.len()
        );
        let names: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
        assert!(names.contains(&"search_library"));
        // Every tool must be a function with an object schema the API accepts.
        for t in &tools {
            assert_eq!(t.kind, "function");
            assert_eq!(t.function.parameters["type"], "object");
        }
    }

    #[test]
    fn policy_passes_safe_tools_through_unchanged() {
        let confirm = deny_destructive();
        let args = json!({ "query": "miles" });
        let out = apply_policy("search_library", args.clone(), &confirm).unwrap();
        assert_eq!(out, args);
    }

    #[test]
    fn policy_declines_destructive_without_confirmation() {
        let confirm = deny_destructive();
        let err = apply_policy("delete_playlist", json!({ "id": 3 }), &confirm).unwrap_err();
        assert!(err.contains("declined"));
        assert!(err.contains("delete_playlist"));
    }

    #[test]
    fn policy_injects_confirm_when_approved() {
        let confirm: ConfirmFn = Arc::new(|_, _| true);
        let out = apply_policy("delete_memo", json!({ "id": 7 }), &confirm).unwrap();
        assert_eq!(out["confirm"], true);
        assert_eq!(out["id"], 7);
    }
}
