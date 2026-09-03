//! Gemini transport.
//!
//! Wire format and HTTP only — no domain logic. Cost and latency are managed
//! here because they are properties of the transport:
//!
//!   * the system instruction and tool declarations are byte-stable across every
//!     request and sit *first* in the payload, so Gemini's implicit context
//!     cache hits them automatically;
//!   * `thinkingBudget: 0` disables Flash's thinking tokens;
//!   * `temperature: 0` for determinism, which also keeps the prefix cacheable.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::llm::{Completion, LlmProvider, Message, ProviderError, ToolCall, ToolDef, Turn};

/// Default model — override with `GEMINI_MODEL`. Flash-Lite is cheap and fast
/// and supports the function calling the agent depends on.
const DEFAULT_MODEL: &str = "gemini-3.1-flash-lite";
const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// Per-call HTTP timeout. The agent may make several calls, so this is well
/// under what a merchant will wait for the whole turn.
const HTTP_TIMEOUT_SECS: u64 = 20;

#[derive(Clone)]
pub struct GeminiProvider {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl GeminiProvider {
    /// Build from `GEMINI_API_KEY`. `None` when unset or empty, so the server
    /// starts with the feature simply disabled rather than failing.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("GEMINI_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let model = std::env::var("GEMINI_MODEL")
            .ok()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
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
        let url = format!("{ENDPOINT}/{}:generateContent", self.model);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
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
impl LlmProvider for GeminiProvider {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        let mut body = json!({
            "systemInstruction": { "parts": [{ "text": req.system }] },
            "contents": contents(req.messages),
            "generationConfig": {
                "temperature": 0,
                "maxOutputTokens": req.max_tokens,
                "thinkingConfig": { "thinkingBudget": 0 }
            }
        });
        // Omitted entirely when there are none: an empty `functionDeclarations`
        // array is rejected, and the compaction call deliberately offers no
        // tools because it wants prose.
        if !req.tools.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": declarations(req.tools) }]);
            if req.force_tool {
                body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "ANY" } });
            }
        }

        let payload = self.post(&body).await?;
        let parts = payload
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let calls: Vec<ToolCall> = parts
            .iter()
            .enumerate()
            // The signature lives on the PART, beside `functionCall`, not
            // inside it — so the part has to be kept, not just the call.
            .filter_map(|(i, part)| {
                let c = part.get("functionCall")?;
                let name = c.get("name").and_then(Value::as_str)?.to_string();
                Some(ToolCall {
                    // Gemini matches tool results by name, not id, so one is
                    // synthesized purely to keep the transport types uniform.
                    id: format!("{name}-{i}"),
                    name,
                    args: c.get("args").cloned().unwrap_or_else(|| json!({})),
                    signature: part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect();
        if !calls.is_empty() {
            return Ok(Turn::Calls(calls));
        }

        let text = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(ProviderError::Parse(
                "no function call and no text in the response".into(),
            ));
        }
        Ok(Turn::Text(text))
    }

    fn name(&self) -> String {
        self.model.clone()
    }
}

fn declarations(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// Translate the transport's messages into Gemini `contents`.
///
/// Gemini has no dedicated tool role: a tool result is a `functionResponse` part
/// on a **user** turn, and it is matched to its call by function name.
fn contents(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::User(text) => out.push(json!({ "role": "user", "parts": [{ "text": text }] })),
            Message::Assistant { text, calls } => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = text.as_ref().filter(|t| !t.trim().is_empty()) {
                    parts.push(json!({ "text": t }));
                }
                for c in calls {
                    let mut part = json!({
                        "functionCall": { "name": c.name, "args": c.args }
                    });
                    // Echoed back verbatim. Gemini 3 rejects the request
                    // outright when a replayed `functionCall` part has lost its
                    // signature, because its reasoning does not persist between
                    // requests and this is how it is restored.
                    if let Some(sig) = &c.signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    out.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Message::ToolResult { name, content, .. } => {
                out.push(json!({
                    "role": "user",
                    "parts": [{ "functionResponse": {
                        "name": name,
                        // Gemini requires an object here; a bare array or scalar
                        // is rejected, so results are always wrapped.
                        "response": { "result": content }
                    }}]
                }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_is_wrapped_in_an_object() {
        let msgs = vec![Message::ToolResult {
            id: "x".into(),
            name: "query_metrics".into(),
            content: json!([1, 2, 3]),
        }];
        let c = contents(&msgs);
        // A bare array here is rejected by the API — the wrapper is required.
        assert_eq!(
            c[0]["parts"][0]["functionResponse"]["response"]["result"],
            json!([1, 2, 3])
        );
        assert_eq!(c[0]["role"], "user");
    }

    #[test]
    fn assistant_tool_calls_replay_as_model_function_calls() {
        let msgs = vec![Message::Assistant {
            text: None,
            calls: vec![ToolCall {
                id: "a".into(),
                name: "run_preset".into(),
                args: json!({ "preset": "top_products" }),
                signature: Some("sig-abc".into()),
            }],
        }];
        let c = contents(&msgs);
        assert_eq!(c[0]["role"], "model");
        assert_eq!(c[0]["parts"][0]["functionCall"]["name"], "run_preset");
    }

    #[test]
    fn a_thought_signature_survives_the_round_trip() {
        // Gemini 3 rejects the whole request when a replayed `functionCall`
        // part has lost its signature: "Function call is missing a
        // thought_signature". It lives on the PART, beside `functionCall`, not
        // inside it — which is exactly how it got dropped.
        let payload = json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "run_preset", "args": { "preset": "top_products" } },
                "thoughtSignature": "Ct8BAd..."
            }]}}]
        });
        let parts = payload
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap();
        let call = parts
            .iter()
            .enumerate()
            .find_map(|(i, part)| {
                let c = part.get("functionCall")?;
                Some(ToolCall {
                    id: format!("x-{i}"),
                    name: c.get("name")?.as_str()?.to_string(),
                    args: c.get("args").cloned().unwrap(),
                    signature: part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .expect("a call");
        assert_eq!(call.signature.as_deref(), Some("Ct8BAd..."));

        // ...and it is emitted again on the way back out.
        let replayed = contents(&[Message::Assistant {
            text: None,
            calls: vec![call],
        }]);
        assert_eq!(replayed[0]["parts"][0]["thoughtSignature"], "Ct8BAd...");
    }

    #[test]
    fn a_call_without_a_signature_omits_the_field_entirely() {
        // Sending `thoughtSignature: null` is not the same as omitting it.
        let replayed = contents(&[Message::Assistant {
            text: None,
            calls: vec![ToolCall {
                id: "a".into(),
                name: "answer".into(),
                args: json!({ "text": "hi" }),
                signature: None,
            }],
        }]);
        assert!(replayed[0]["parts"][0].get("thoughtSignature").is_none());
    }

    #[test]
    fn an_empty_assistant_turn_is_dropped_not_sent_as_a_blank() {
        // Gemini rejects a content entry with no parts.
        let msgs = vec![Message::Assistant {
            text: Some("   ".into()),
            calls: vec![],
        }];
        assert!(contents(&msgs).is_empty());
    }

    #[test]
    fn declarations_pass_the_schema_through_unchanged() {
        let tools = super::super::tools::tool_defs();
        let d = declarations(tools);
        assert_eq!(d.len(), tools.len());
        assert_eq!(d[0]["name"], tools[0].name);
        assert_eq!(d[0]["parameters"], tools[0].parameters);
    }
}
