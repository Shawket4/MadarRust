//! The swappable LLM provider interface.
//!
//! The pipeline (handler → provider → executor) depends only on this trait, so
//! Gemini can be replaced with any other model — or a deterministic mock in
//! tests — without touching the report catalog, executor, or HTTP layer. The
//! provider's ONLY job is to translate a natural-language question into a choice
//! of pre-written report + typed arguments (it never sees or emits SQL), plus an
//! optional one-line summary of a result.

use async_trait::async_trait;
use serde_json::{Map, Value};

/// Everything the model needs to interpret a question: the question itself plus
/// the grounding context (today's date + timezone so relative dates resolve, and
/// the answer language). Passed to both provider calls; the heavy, invariant
/// part of the request (system prompt + tool schema) stays out of here so it can
/// remain a byte-stable cacheable prefix.
#[derive(Debug, Clone)]
pub struct ChatContext {
    pub question: String,
    /// Current date in the merchant's timezone, ISO `YYYY-MM-DD`.
    pub today: String,
    /// IANA timezone name of the merchant (e.g. "Africa/Cairo").
    pub timezone: String,
    /// BCP-47-ish locale the answer should be in (e.g. "en", "ar").
    pub locale: String,
}

/// What the model decided: which report to run and the values to fill in.
#[derive(Debug, Clone)]
pub struct ToolChoice {
    pub report_id: String,
    pub args: Map<String, Value>,
}

#[derive(Debug)]
pub enum ProviderError {
    /// The provider isn't configured (e.g. missing API key). Distinct so the
    /// endpoint can report the feature as unavailable rather than a bad request.
    NotConfigured(String),
    /// The upstream call failed (network, HTTP status, quota).
    Upstream(String),
    /// The model returned no usable report choice for this question.
    NoChoice(String),
    /// The upstream response couldn't be parsed into a choice.
    Parse(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NotConfigured(m) => write!(f, "{m}"),
            ProviderError::Upstream(m) => write!(f, "AI provider error: {m}"),
            ProviderError::NoChoice(m) => write!(f, "{m}"),
            ProviderError::Parse(m) => {
                write!(f, "AI provider returned an unexpected response: {m}")
            }
        }
    }
}

impl From<ProviderError> for crate::errors::AppError {
    fn from(e: ProviderError) -> Self {
        use crate::errors::AppError;
        match e {
            ProviderError::NotConfigured(m) => AppError::ServiceUnavailable(m),
            ProviderError::Upstream(m) => AppError::ServiceUnavailable(m),
            // The model couldn't map the question to a report — that's a client
            // problem (unanswerable question), surfaced as a clean 400.
            ProviderError::NoChoice(m) => AppError::BadRequest(m),
            ProviderError::Parse(m) => AppError::ServiceUnavailable(m),
        }
    }
}

/// A model provider that picks a report and (optionally) summarizes a result.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Choose a report id + arguments for the question, drawn from the fixed
    /// [`crate::ai::catalog::REPORTS`] menu, grounded by [`ChatContext`].
    async fn choose_report(&self, ctx: &ChatContext) -> Result<ToolChoice, ProviderError>;

    /// Produce ONE short sentence (in `ctx.locale`) summarizing `data_json` (the
    /// report result) in answer to the question. Best-effort: returns `None` if
    /// the model declines or the call fails softly.
    async fn summarize(
        &self,
        ctx: &ChatContext,
        report_title: &str,
        data_json: &str,
    ) -> Result<Option<String>, ProviderError>;

    /// Provider label for logs / responses (e.g. "gemini-2.5-flash").
    fn name(&self) -> &'static str;
}

/// A deterministic provider for tests: maps a question to a report by keyword,
/// with no network. Keeps the whole pipeline testable without a real key.
#[cfg(test)]
pub struct MockProvider;

#[cfg(test)]
#[async_trait]
impl LlmProvider for MockProvider {
    async fn choose_report(&self, ctx: &ChatContext) -> Result<ToolChoice, ProviderError> {
        let q = ctx.question.to_lowercase();
        let id = if q.contains("profit") || q.contains("margin") {
            "product_profit"
        } else if q.contains("product") || q.contains("best sell") {
            "top_products"
        } else if q.contains("hour") || q.contains("peak") {
            "sales_by_hour"
        } else if q.contains("per day") || q.contains("daily") || q.contains("trend") {
            "sales_by_day"
        } else if q.contains("payment") || q.contains("cash") || q.contains("card") {
            "payment_method_breakdown"
        } else if q.contains("waiter") {
            "waiter_performance"
        } else if q.contains("low stock") || q.contains("reorder") {
            "low_stock"
        } else if q.contains("branch") || q.contains("store") {
            "sales_by_branch"
        } else if q.contains("category") {
            "top_categories"
        } else if q.contains("sale") || q.contains("revenue") {
            "sales_summary"
        } else {
            return Err(ProviderError::NoChoice(
                "I couldn't match that question to a report.".into(),
            ));
        };
        let mut args = Map::new();
        args.insert("limit".into(), Value::from(5));
        Ok(ToolChoice {
            report_id: id.to_string(),
            args,
        })
    }

    async fn summarize(
        &self,
        _ctx: &ChatContext,
        report_title: &str,
        _data_json: &str,
    ) -> Result<Option<String>, ProviderError> {
        Ok(Some(format!(
            "Here is your {}.",
            report_title.to_lowercase()
        )))
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}
