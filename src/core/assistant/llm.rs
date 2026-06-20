//! OpenAI-compatible chat-completions client for the embedded assistant.
//!
//! Targets the standard `POST {base_url}/chat/completions` shape with
//! function/tool calling. MiniMax exposes exactly this surface at
//! `https://api.minimax.io/v1`, as do local servers (Ollama, LM Studio) and
//! OpenAI itself — so a single client covers every backend; only `base_url`,
//! `model` and the API key differ.
//!
//! This layer is **transport only**: it sends a list of [`Message`]s plus the
//! available [`ToolDef`]s and returns the assistant's reply [`Message`] (which
//! may carry `tool_calls`). Deciding what to do with those calls — running them
//! through [`crate::core::mcp::tools::dispatch`] and looping — is the agent's
//! job, not this client's. Non-streaming for now: one request, one full reply.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::net;

/// One message in a chat conversation. Mirrors the OpenAI schema: a `system` /
/// `user` / `assistant` / `tool` turn. `content` is absent on an assistant turn
/// that only carries `tool_calls`; `tool_call_id` is set only on a `tool` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool invocations the model requested (assistant turns only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Links a `tool` reply back to the `ToolCall.id` it answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The tool's function name on a `tool` reply (some servers want it echoed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// A `system` instruction turn.
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    /// A `user` turn.
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    /// An `assistant` text turn (used by the UI to inject notes like errors;
    /// real assistant turns come deserialized from the model).
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text("assistant", content)
    }

    /// A `tool` reply, answering the call with `id` (named `name`).
    pub fn tool(
        id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
            name: Some(name.into()),
        }
    }

    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
}

/// One tool call the model wants performed. `arguments` is a JSON **string**
/// (OpenAI encodes the call's arguments as text, not as a nested object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded argument object; parse before dispatch.
    pub arguments: String,
}

impl ToolCall {
    /// Parses [`FunctionCall::arguments`] into a JSON object. An empty/blank
    /// string (some models send no args) maps to an empty object, not an error.
    pub fn parse_arguments(&self) -> Result<Value> {
        let raw = self.function.arguments.trim();
        if raw.is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        Ok(serde_json::from_str(raw)?)
    }
}

/// A tool advertised to the model, in OpenAI function shape. Built from the
/// shared MCP registry's descriptors (`inputSchema` → `parameters`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the call arguments.
    pub parameters: Value,
}

impl ToolDef {
    /// Wraps a function name + description + JSON-Schema into a tool descriptor.
    pub fn function(
        name: impl Into<String>,
        description: Option<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description,
                parameters,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolDef],
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
    #[serde(default)]
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

/// A configured OpenAI-compatible endpoint. Cheap to clone (the `ureq::Agent`
/// is internally reference-counted).
#[derive(Clone)]
pub struct LlmClient {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    model: String,
    temperature: Option<f32>,
}

impl LlmClient {
    /// `base_url` is the API root (e.g. `https://api.minimax.io/v1`); the
    /// `/chat/completions` path is appended. A full endpoint URL is accepted too
    /// (used verbatim if it already ends in `/chat/completions`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        // Generous read timeout: LLMs (especially reasoning models) can take tens
        // of seconds to a full reply. Connect/write stay short to fail fast.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(180))
            .build();
        Self {
            agent,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            temperature: None,
        }
    }

    /// The full `chat/completions` endpoint for this client.
    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    /// Sends one completion request and returns the assistant's reply message.
    /// `tools` may be empty for a plain chat turn. The returned message carries
    /// `tool_calls` when the model wants to act.
    pub fn complete(&self, messages: &[Message], tools: &[ToolDef]) -> Result<Message> {
        let body = ChatRequest {
            model: &self.model,
            messages,
            tools,
            temperature: self.temperature,
            stream: false,
        };

        let resp = match self
            .agent
            .post(&self.endpoint())
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(&body)
        {
            Ok(resp) => resp,
            // Surface the API's own error body (status text alone is rarely
            // enough); it carries the auth/quota/model reason. The key lives in
            // the request header, not the response, so this won't leak it.
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                bail!("LLM endpoint returned {code}: {}", detail.trim());
            }
            Err(e) => return Err(e.into()),
        };

        let parsed: ChatResponse = net::json_capped(resp, net::MAX_JSON_BYTES)?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| anyhow!("LLM response contained no choices"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_in_openai_shape() {
        let msgs = vec![Message::system("you are helpful"), Message::user("hi")];
        let tools = vec![ToolDef::function(
            "list_artists",
            Some("List artists".into()),
            json!({"type": "object", "properties": {}}),
        )];
        let body = ChatRequest {
            model: "MiniMax-M2",
            messages: &msgs,
            tools: &tools,
            temperature: Some(0.4),
            stream: false,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["model"], "MiniMax-M2");
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "list_artists");
        assert_eq!(v["stream"], false);
        // Absent optional fields must not be emitted (servers reject nulls).
        assert!(v["messages"][0].get("tool_calls").is_none());
    }

    #[test]
    fn tools_omitted_when_empty() {
        let body = ChatRequest {
            model: "m",
            messages: &[Message::user("hi")],
            tools: &[],
            temperature: None,
            stream: false,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("tools").is_none());
        assert!(v.get("temperature").is_none());
    }

    #[test]
    fn response_with_tool_calls_deserializes() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "list_artists",
                            "arguments": "{\"limit\": 5}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let parsed: ChatResponse = serde_json::from_value(raw).unwrap();
        let msg = &parsed.choices[0].message;
        assert_eq!(msg.role, "assistant");
        assert!(msg.content.is_none());
        let calls = msg.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "list_artists");
        assert_eq!(calls[0].parse_arguments().unwrap()["limit"], 5);
    }

    #[test]
    fn empty_arguments_parse_to_empty_object() {
        let call = ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "get_stats".into(),
                arguments: "  ".into(),
            },
        };
        assert_eq!(call.parse_arguments().unwrap(), json!({}));
    }

    #[test]
    fn endpoint_appends_path_once() {
        let c = LlmClient::new("https://api.minimax.io/v1/", "k", "m");
        assert_eq!(c.endpoint(), "https://api.minimax.io/v1/chat/completions");
        let c2 = LlmClient::new("https://host/v1/chat/completions", "k", "m");
        assert_eq!(c2.endpoint(), "https://host/v1/chat/completions");
    }

    /// Live check against the real endpoint — the one thing unit tests can't
    /// cover: that the provider actually returns structured `tool_calls` for the
    /// OpenAI shape. Run with credentials, e.g.:
    ///   MINIMAX_API_KEY=… MINIMAX_MODEL=MiniMax-M2 \
    ///     cargo test assistant::llm::tests::live -- --ignored --nocapture
    /// Override MINIMAX_BASE_URL / MINIMAX_MODEL as needed for your account.
    #[test]
    #[ignore = "hits the live LLM API; set MINIMAX_API_KEY to run"]
    fn live_endpoint_emits_tool_call() {
        let key = std::env::var("MINIMAX_API_KEY").expect("set MINIMAX_API_KEY");
        let base = std::env::var("MINIMAX_BASE_URL")
            .unwrap_or_else(|_| "https://api.minimax.io/v1".into());
        let model = std::env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M2".into());

        let client = LlmClient::new(base, key, model);
        let tools = vec![ToolDef::function(
            "get_weather",
            Some("Get the current weather for a city.".into()),
            json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            }),
        )];
        let reply = client
            .complete(
                &[Message::user(
                    "What's the weather in Berlin right now? Use the tool.",
                )],
                &tools,
            )
            .expect("live request failed");

        let calls = reply.tool_calls.unwrap_or_default();
        assert!(
            !calls.is_empty(),
            "model returned no tool_calls; content was: {:?}",
            reply.content
        );
        assert_eq!(calls[0].function.name, "get_weather");
        let args = calls[0].parse_arguments().expect("arguments must be JSON");
        eprintln!("tool call args: {args}");
    }
}
