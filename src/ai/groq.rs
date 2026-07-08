//! Groq implementation of [`LlmProvider`] (OpenAI-compatible chat completions).
//!
//! Groq runs open models (Llama, …) on their LPU inference — very fast and
//! cheap. The report picker forces exactly one tool call
//! (`tool_choice: "required"`); the summary is a plain completion. Prompts and
//! the per-report tool schema are shared with Gemini via [`super::prompt`], so
//! both providers behave identically — only the wire format and HTTP differ.
//!
//! Select it with `AI_PROVIDER=groq` and set `GROQ_API_KEY` (+ optional
//! `GROQ_MODEL`) in `.env`.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::catalog::REPORTS;
use super::prompt;
use super::provider::{ChatContext, LlmProvider, ProviderError, ToolChoice};

const ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";
/// Default model — a solid tool-calling model on Groq. Override with `GROQ_MODEL`.
const DEFAULT_MODEL: &str = "llama-3.3-70b-versatile";

#[derive(Clone)]
pub struct GroqProvider {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl GroqProvider {
    /// Build from `GROQ_API_KEY` (+ optional `GROQ_MODEL`). `None` when unset/empty.
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
            .timeout(Duration::from_secs(20))
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
    async fn choose_report(&self, ctx: &ChatContext) -> Result<ToolChoice, ProviderError> {
        // OpenAI-style messages: system, then replayed history (user + a short
        // assistant note of which report answered), then the current user turn.
        // `tool_choice: "required"` forces exactly one function call.
        let mut messages: Vec<Value> = Vec::with_capacity(ctx.history.len() * 2 + 2);
        messages.push(json!({ "role": "system", "content": prompt::SYSTEM_PROMPT }));
        for turn in &ctx.history {
            messages.push(json!({ "role": "user", "content": turn.question }));
            if let Some(report) = &turn.report_id {
                messages.push(json!({
                    "role": "assistant",
                    "content": format!("Answered using report: {report}.")
                }));
            }
        }
        messages.push(json!({ "role": "user", "content": prompt::user_text(ctx) }));

        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tool_declarations(),
            "tool_choice": "required",
            "temperature": 0,
            "max_tokens": 512
        });

        let payload = self.post(&body).await?;
        let call = payload
            .pointer("/choices/0/message/tool_calls/0/function")
            .ok_or_else(|| {
                ProviderError::NoChoice("I couldn't match that question to a report.".into())
            })?;

        let report_id = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Parse("tool call missing name".into()))?
            .to_string();
        // OpenAI-style: `arguments` is a JSON *string* that must be parsed.
        let args = match call.get("arguments").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => serde_json::from_str::<Map<String, Value>>(s)
                .map_err(|e| ProviderError::Parse(format!("bad tool arguments: {e}")))?,
            _ => Map::new(),
        };

        Ok(ToolChoice { report_id, args })
    }

    async fn summarize(
        &self,
        ctx: &ChatContext,
        report_title: &str,
        data_json: &str,
    ) -> Result<Option<String>, ProviderError> {
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": prompt::SUMMARY_SYSTEM_PROMPT },
                { "role": "user", "content": prompt::summary_user_text(ctx, report_title, data_json) }
            ],
            "temperature": 0,
            "max_tokens": 160
        });

        let payload = self.post(&body).await?;
        let text = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(text)
    }

    fn name(&self) -> String {
        format!("groq/{}", self.model)
    }
}

/// OpenAI-style `tools` array, built once from the catalog and cached.
fn tool_declarations() -> &'static Value {
    static TOOLS: OnceLock<Value> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let tools: Vec<Value> = REPORTS
            .iter()
            .map(|r| {
                json!({
                    "type": "function",
                    "function": {
                        "name": r.id,
                        "description": r.description,
                        "parameters": prompt::report_parameters_schema(r),
                    }
                })
            })
            .collect();
        Value::Array(tools)
    })
}
