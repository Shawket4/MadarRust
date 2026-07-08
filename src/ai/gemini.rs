//! Gemini 2.5 Flash implementation of [`LlmProvider`].
//!
//! Cost & latency are minimized deliberately:
//!
//!   * **Cached tool definitions.** The function declarations (one per catalog
//!     report) and the system instruction are built ONCE into a byte-stable
//!     JSON prefix (`tool_declarations()`), reused verbatim on every request.
//!     Because that prefix is identical across calls and sits *first* in the
//!     payload (the user's question is the only varying, trailing part), Gemini
//!     2.5's implicit context caching hits it automatically — the large,
//!     invariant tool schema isn't re-billed at full input price on repeat
//!     calls, and we spend zero CPU re-serializing it.
//!   * **No thinking.** `thinkingBudget: 0` disables Flash's thinking tokens.
//!   * **Forced single call.** `functionCallingConfig.mode = ANY` makes the
//!     model answer with a function call and no prose, so one round trip picks
//!     the report — no wasted output tokens, no follow-up turn.
//!   * **Tight output cap + temperature 0** for determinism (also cache-friendly).
//!
//! The summary pass is a separate, optional, equally-small call.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::catalog::{ParamKind, REPORTS};
use super::provider::{ChatContext, LlmProvider, ProviderError, ToolChoice};

const MODEL: &str = "gemini-2.5-flash";
const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";

const SYSTEM_PROMPT: &str = "You are the analytics assistant for a restaurant \
point-of-sale system. The merchant asks about THEIR OWN business data in plain \
language, in English or Arabic (including Egyptian dialect). Choose exactly one \
of the provided report functions and fill in its parameters from the question. \
The user's message states today's date and timezone — resolve relative dates \
(\"last week\", \"this month\", \"yesterday\", \"الأسبوع الماضي\", \"امبارح\", \
\"الشهر ده\") to concrete ISO-8601 dates relative to that. You do NOT choose \
which branches to include — branch access is enforced by the backend. If no \
report fits, still choose the closest one. You never write SQL and never invent \
data.";

/// Gemini-backed provider. Cheap to clone (holds an `reqwest::Client`, which is
/// internally reference-counted).
#[derive(Clone)]
pub struct GeminiProvider {
    api_key: String,
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
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .ok()?;
        Some(Self { api_key, http })
    }

    async fn post(&self, body: &Value) -> Result<Value, ProviderError> {
        let url = format!("{ENDPOINT}/{MODEL}:generateContent");
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
        // per-request variation (question + today's date/timezone/language) goes
        // in the trailing user turn.
        let user_text = format!(
            "Today is {} in timezone {}. Answer language: {}.\n\nQuestion: {}",
            ctx.today, ctx.timezone, ctx.locale, ctx.question
        );
        let body = json!({
            "systemInstruction": { "parts": [{ "text": SYSTEM_PROMPT }] },
            "tools": tool_declarations(),
            "toolConfig": { "functionCallingConfig": { "mode": "ANY" } },
            "contents": [{ "role": "user", "parts": [{ "text": user_text }] }],
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
            "systemInstruction": { "parts": [{ "text":
                "You summarize restaurant analytics results. Given the user's \
                 question and the resulting data as JSON, reply with ONE short, \
                 factual sentence stating the key takeaway. Reply in the SAME \
                 language as the question. No preamble, no markdown, no lists." }] },
            "contents": [{ "role": "user", "parts": [{ "text":
                format!("Language: {}\nQuestion: {}\nReport: {report_title}\nData: {data_json}",
                        ctx.locale, ctx.question) }] }],
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

    fn name(&self) -> &'static str {
        MODEL
    }
}

/// The `tools` array (function declarations) for the request, built once from
/// the report catalog and cached. Its bytes are identical on every call, which
/// is what makes the request prefix cacheable upstream.
fn tool_declarations() -> &'static Value {
    static TOOLS: OnceLock<Value> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let declarations: Vec<Value> = REPORTS
            .iter()
            .map(|r| {
                let mut properties = Map::new();
                let mut required: Vec<Value> = Vec::new();
                for p in r.params {
                    let schema = match p.kind {
                        ParamKind::Date => json!({
                            "type": "string",
                            "format": "date-time",
                            "description": p.description
                        }),
                        ParamKind::Int { .. } => json!({
                            "type": "integer",
                            "description": p.description
                        }),
                    };
                    properties.insert(p.name.to_string(), schema);
                    if p.required {
                        required.push(Value::from(p.name));
                    }
                }
                json!({
                    "name": r.id,
                    "description": r.description,
                    "parameters": {
                        "type": "object",
                        "properties": properties,
                        "required": required
                    }
                })
            })
            .collect();
        json!([{ "functionDeclarations": declarations }])
    })
}
