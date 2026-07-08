//! AI analytics chat.
//!
//! A merchant asks a plain-language question about THEIR OWN data ("sales last
//! week", "top 5 products") and gets a table/chart back with an optional short
//! summary. The design keeps the model on a tight leash:
//!
//!   * it never writes SQL — it only picks one of a fixed menu of pre-written
//!     [`catalog`] reports and fills in typed parameters ([`provider`]);
//!   * the backend runs that report on the caller's RLS-scoped tenant pool
//!     inside a read-only, time-limited, row-capped transaction ([`executor`]);
//!   * the model call sits behind the swappable [`provider::LlmProvider`] trait,
//!     with [`gemini::GeminiProvider`] the default implementation;
//!   * repeat questions are served from a short-TTL response cache.
//!
//! Adding a report is one entry in [`catalog::REPORTS`]; nothing else changes.

pub mod catalog;
pub mod executor;
pub mod gemini;
pub mod handlers;
pub mod provider;
pub mod routes;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use provider::LlmProvider;

/// Process-wide AI state shared with handlers via `web::Data`.
///
/// `provider` is `None` when `GEMINI_API_KEY` is unset — the endpoint then
/// reports the feature as unavailable (503) instead of failing to start, so the
/// rest of the server runs unaffected.
pub struct AiState {
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub cache: moka::future::Cache<String, handlers::AiChatResponse>,
}

impl AiState {
    /// Build state, wiring the Gemini provider when a key is present. The
    /// response cache is small and short-lived: it collapses duplicate
    /// questions (a merchant re-asking, a dashboard re-mounting) without
    /// serving stale numbers.
    pub fn from_env() -> Self {
        let provider =
            gemini::GeminiProvider::from_env().map(|p| Arc::new(p) as Arc<dyn LlmProvider>);
        if provider.is_some() {
            tracing::info!("AI analytics enabled (gemini-2.5-flash)");
        } else {
            tracing::info!("AI analytics disabled (set GEMINI_API_KEY to enable)");
        }
        Self {
            provider,
            cache: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }

    /// Construct with an explicit provider (tests).
    #[cfg(test)]
    pub fn with_provider(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider: Some(provider),
            cache: moka::future::Cache::builder()
                .max_capacity(100)
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }
}
