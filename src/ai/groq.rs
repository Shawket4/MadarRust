//! Groq transport (OpenAI-compatible chat completions).
//!
//! Wire format and HTTP only. Groq runs open models on their LPU inference,
//! which is fast and cheap; select it with `AI_PROVIDER=groq` and `GROQ_API_KEY`.
//!
//! Because the tool declarations and prompts are shared with every other
//! provider through [`super::tools`] and [`super::prompt`], the agent behaves
//! identically here — only the JSON envelope differs.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::llm::{Completion, LlmProvider, Message, ProviderError, ToolCall, ToolDef, Turn};

const ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";
/// Default model — override with `GROQ_MODEL`.
const DEFAULT_MODEL: &str = "llama-3.3-70b-versatile";
const HTTP_TIMEOUT_SECS: u64 = 20;

#[derive(Clone)]
pub struct GroqProvider {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl GroqProvider {
    /// Build from `GROQ_API_KEY` (+ optional `GROQ_MODEL`). `None` when unset.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("GROQ_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let model = std::env::var("GROQ_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .ok()?;
        Some(Self {
            api_key,
            model,
            http,
        })
    }

    async fn post(&self, body: &Value) -> Result<Value, ProviderError> {
        let resp = self
            .http
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;
        if !status.is_success() {
            let msg = payload
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error");
            return Err(ProviderError::Upstream(format!("{status}: {msg}")));
        }
        Ok(payload)
    }
}

#[async_trait]
impl LlmProvider for GroqProvider {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        let mut messages = vec![json!({ "role": "system", "content": req.system })];
        messages.extend(wire_messages(req.messages));

        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tool_specs(req.tools),
            "tool_choice": if req.force_tool { "required" } else { "auto" },
            "temperature": 0,
            "max_tokens": req.max_tokens,
        });

        let payload = self.post(&body).await?;
        let message = payload
            .pointer("/choices/0/message")
            .ok_or_else(|| ProviderError::Parse("no message in the response".into()))?;

        let calls: Vec<ToolCall> = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        let name = c.pointer("/function/name").and_then(Value::as_str)?;
                        // Arguments arrive as a JSON *string* that must be
                        // parsed; a model emitting malformed JSON here is
                        // common enough that it becomes an empty object rather
                        // than failing the whole turn — the tool then rejects it
                        // with a message the model can act on.
                        let args = c
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_else(|| json!({}));
                        Some(ToolCall {
                            id: c
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or(name)
                                .to_string(),
                            name: name.to_string(),
                            args,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !calls.is_empty() {
            return Ok(Turn::Calls(calls));
        }

        let text = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if text.is_empty() {
            return Err(ProviderError::Parse(
                "no tool call and no content in the response".into(),
            ));
        }
        Ok(Turn::Text(text))
    }

    fn name(&self) -> String {
        format!("groq/{}", self.model)
    }
}

fn tool_specs(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

/// Translate the transport's messages into OpenAI-style ones. Tool results get
/// their own `tool` role and are correlated by `tool_call_id`.
fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::User(text) => out.push(json!({ "role": "user", "content": text })),
            Message::Assistant { text, calls } => {
                let mut msg = json!({ "role": "assistant" });
                msg["content"] = match text {
                    Some(t) if !t.trim().is_empty() => json!(t),
                    _ => Value::Null,
                };
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(
                        calls
                            .iter()
                            .map(|c| json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    // Arguments go back out as a string, the
                                    // same shape they arrived in.
                                    "arguments": c.args.to_string(),
                                }
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                out.push(msg);
            }
            Message::ToolResult { id, name, content } => out.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "name": name,
                "content": content.to_string(),
            })),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_correlate_by_call_id() {
        let msgs = vec![
            Message::Assistant {
                text: None,
                calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "run_preset".into(),
                    args: json!({ "preset": "top_products" }),
                }],
            },
            Message::ToolResult {
                id: "call_1".into(),
                name: "run_preset".into(),
                content: json!({ "row_count": 5 }),
            },
        ];
        let w = wire_messages(&msgs);
        assert_eq!(w[0]["tool_calls"][0]["id"], "call_1");
        // Arguments are a JSON string on this wire format, not an object.
        assert!(w[0]["tool_calls"][0]["function"]["arguments"].is_string());
        assert_eq!(w[1]["role"], "tool");
        assert_eq!(w[1]["tool_call_id"], "call_1");
    }

    #[test]
    fn an_assistant_turn_with_no_text_sends_null_content() {
        // OpenAI-style APIs require the key to be present even when there is
        // only a tool call; omitting it is a 400.
        let msgs = vec![Message::Assistant {
            text: None,
            calls: vec![],
        }];
        assert!(wire_messages(&msgs)[0]["content"].is_null());
    }

    #[test]
    fn tool_specs_wrap_the_shared_declarations_unchanged() {
        let tools = super::super::tools::tool_defs();
        let s = tool_specs(tools);
        assert_eq!(s[0]["type"], "function");
        assert_eq!(s[0]["function"]["name"], tools[0].name);
        assert_eq!(s[0]["function"]["parameters"], tools[0].parameters);
    }
}
