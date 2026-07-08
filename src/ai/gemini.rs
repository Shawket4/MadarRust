//! Gemini 2.5 Flash implementation of [`LlmProvider`].
//!
//! Cost & latency are minimized deliberately:
//!
//!   * **Cached tool definitions.** The function declarations (one per catalog
//!     report) and the system instruction are built ONCE into a byte-stable
//!     JSON prefix (`tool_declarations()`), reused verbatim on every request.
//!     Because that prefix is identical across calls and sits *first* in the
//!     payload (the user's question is the only varying, trailing part), Gemini
//!     2.5's implicit context caching hits it automatically.
//!   * **No thinking.** `thinkingBudget: 0` disables Flash's thinking tokens.
//!   * **Forced single call.** `functionCallingConfig.mode = ANY` makes the
//!     model answer with a function call and no prose.
//!   * **Tight output cap + temperature 0** for determinism (also cache-friendly).
//!
//! Prompts and the per-report tool schema are shared with every other provider
//! via [`super::prompt`], so they can't drift.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::catalog::REPORTS;
use super::prompt;
use super::provider::{ChatContext, LlmProvider, ProviderError, ToolChoice};

/// Default Gemini model — override with `GEMINI_MODEL`. Gemini 3.1 Flash-Lite
/// (GA): cheap + fast and supports the function calling the report router needs.
const DEFAULT_MODEL: &str = "gemini-3.1-flash-lite";
const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Gemini-backed provider. Cheap to clone (holds an `reqwest::Client`, which is
/// internally reference-counted).
#[derive(Clone)]
pub struct GeminiProvider {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl GeminiProvider {
    /// Build from `GEMINI_API_KEY`. Returns `None` when unset/empty so the
    /// server can start with the AI feature simply disabled.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("GEMINI_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        // `GEMINI_MODEL` overrides the default (e.g. to try a newer model or pin
        // a dated build) without a rebuild.
        let model = std::env::var("GEMINI_MODEL")
            .ok()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .ok()?;
        Some(Self { api_key, model, http })
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
    async fn choose_report(&self, ctx: &ChatContext) -> Result<ToolChoice, ProviderError> {
        // Stable prefix (system + tools) first so it caches upstream; ALL the
        // per-request variation goes in the trailing user turn. Recent history is
        // replayed as plain text turns so follow-ups ("and last month?") resolve
        // without the function-call/response protocol.
        let mut contents: Vec<Value> = Vec::with_capacity(ctx.history.len() * 2 + 1);
        for turn in &ctx.history {
            contents.push(json!({ "role": "user", "parts": [{ "text": turn.question }] }));
            if let Some(report) = &turn.report_id {
                contents.push(json!({
                    "role": "model",
                    "parts": [{ "text": format!("Answered using report: {report}.") }]
                }));
            }
        }
        contents.push(json!({ "role": "user", "parts": [{ "text": prompt::user_text(ctx) }] }));

        let body = json!({
            "systemInstruction": { "parts": [{ "text": prompt::SYSTEM_PROMPT }] },
            "tools": tool_declarations(),
            "toolConfig": { "functionCallingConfig": { "mode": "ANY" } },
            "contents": contents,
            "generationConfig": {
                "temperature": 0,
                "maxOutputTokens": 256,
                "thinkingConfig": { "thinkingBudget": 0 }
            }
        });

        let payload = self.post(&body).await?;
        let parts = payload
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::NoChoice("The assistant gave no answer.".into()))?;

        let call = parts
            .iter()
            .find_map(|p| p.get("functionCall"))
            .ok_or_else(|| {
                ProviderError::NoChoice("I couldn't match that question to a report.".into())
            })?;

        let report_id = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::Parse("function call missing name".into()))?
            .to_string();
        let args = call
            .get("args")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        Ok(ToolChoice { report_id, args })
    }

    async fn summarize(
        &self,
        ctx: &ChatContext,
        report_title: &str,
        data_json: &str,
    ) -> Result<Option<String>, ProviderError> {
        let body = json!({
            "systemInstruction": { "parts": [{ "text": prompt::SUMMARY_SYSTEM_PROMPT }] },
            "contents": [{ "role": "user", "parts": [{
                "text": prompt::summary_user_text(ctx, report_title, data_json) }] }],
            "generationConfig": {
                "temperature": 0,
                "maxOutputTokens": 160,
                "thinkingConfig": { "thinkingBudget": 0 }
            }
        });

        let payload = self.post(&body).await?;
        let text = payload
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(text)
    }

    fn name(&self) -> String {
        self.model.clone()
    }
}

/// The `tools` array (function declarations) for the request, built once from
/// the report catalog and cached. Identical bytes on every call → cacheable
/// prefix upstream.
fn tool_declarations() -> &'static Value {
    static TOOLS: OnceLock<Value> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let declarations: Vec<Value> = REPORTS
            .iter()
            .map(|r| {
                json!({
                    "name": r.id,
                    "description": r.description,
                    "parameters": prompt::report_parameters_schema(r),
                })
            })
            .collect();
        json!([{ "functionDeclarations": declarations }])
    })
}
