//! AI analytics chat.
//!
//! A merchant asks a plain-language question about their own data and gets real
//! figures back, with the table or chart behind them.
//!
//! ```text
//!   POST /ai/chat
//!     └─ agent loop (bounded)
//!          ├─ llm transport ── gemini | groq | mock   (wire format only)
//!          └─ tools ─────────> analytics::compile ──> analytics::execute
//! ```
//!
//! The design in one line: **this module owns the conversation, and owns nothing
//! about analytics.** Every number comes from [`crate::analytics`], through the
//! same compiler and the same execution choke point a dashboard widget uses.
//! There is no AI-specific query path, no AI-specific report catalog, and no way
//! for a model to reach the database except by naming things that already exist.
//!
//! The model's leash, concretely:
//!
//!   * it never writes SQL — it emits a [`crate::analytics::spec::QuerySpec`],
//!     which is deserialized, validated against the registry, and compiled from
//!     author-written fragments;
//!   * it never chooses scope — `:branch_ids` is injected by the executor from
//!     the caller's verified access, and a branch name it supplies can only
//!     narrow within that;
//!   * every query runs read-only, time-limited, row-capped and RLS-scoped;
//!   * a rejected spec is returned *to the model* with the valid options, so the
//!     next step is a correction rather than a failed request.

pub mod agent;
pub mod compaction;
pub mod gemini;
pub mod groq;
pub mod handlers;
pub mod llm;
pub mod prompt;
pub mod pseudonym;
pub mod routes;
pub mod store;
pub mod telemetry;
pub mod tools;

#[cfg(test)]
pub mod mock;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use llm::LlmProvider;

/// Which backend to wire, from `AI_PROVIDER` plus which keys are present.
#[derive(Debug, PartialEq, Eq)]
enum ProviderKind {
    Gemini,
    Groq,
    None,
}

/// Pure selection logic, unit-testable without touching the environment:
///
///   * an explicit `AI_PROVIDER` (`gemini` / `groq`) picks that backend, but
///     only if its key is present — otherwise the feature is off. There is no
///     silent fallback to a provider the operator did not choose, because
///     quietly sending a merchant's questions to a different vendor is not a
///     failure mode anyone should have to discover from a bill;
///   * unset or unknown → auto: Gemini first, then Groq, else off.
fn choose_provider_kind(flag: Option<&str>, has_gemini: bool, has_groq: bool) -> ProviderKind {
    match flag.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("gemini") if has_gemini => ProviderKind::Gemini,
        Some("groq") if has_groq => ProviderKind::Groq,
        Some("gemini") | Some("groq") => ProviderKind::None,
        _ if has_gemini => ProviderKind::Gemini,
        _ if has_groq => ProviderKind::Groq,
        _ => ProviderKind::None,
    }
}

fn env_present(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
}

/// Process-wide AI state, shared with handlers via `web::Data`.
///
/// `provider` is `None` when nothing is configured — the endpoint then reports
/// the feature as unavailable (503) instead of the server failing to start.
pub struct AiState {
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub cache: moka::future::Cache<String, handlers::AiChatResponse>,
}

impl AiState {
    /// Build state from the environment.
    ///
    /// The response cache is small and short-lived: it collapses duplicate
    /// questions — a merchant re-asking, a dashboard remounting, two managers
    /// with the same access asking the same thing — without ever serving stale
    /// numbers. Sixty seconds is chosen against how fast the underlying figures
    /// actually move during service.
    pub fn from_env() -> Self {
        let flag = std::env::var("AI_PROVIDER").ok();
        let provider: Option<Arc<dyn LlmProvider>> = match choose_provider_kind(
            flag.as_deref(),
            env_present("GEMINI_API_KEY"),
            env_present("GROQ_API_KEY"),
        ) {
            ProviderKind::Gemini => {
                gemini::GeminiProvider::from_env().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
            }
            ProviderKind::Groq => {
                groq::GroqProvider::from_env().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
            }
            ProviderKind::None => None,
        };
        match &provider {
            Some(p) => tracing::info!("AI analytics enabled ({})", p.name()),
            None => tracing::info!(
                "AI analytics disabled (set GEMINI_API_KEY or GROQ_API_KEY; pick with AI_PROVIDER)"
            ),
        }
        Self {
            provider,
            cache: Self::cache(10_000),
        }
    }

    fn cache(capacity: u64) -> moka::future::Cache<String, handlers::AiChatResponse> {
        moka::future::Cache::builder()
            .max_capacity(capacity)
            .time_to_live(Duration::from_secs(60))
            .build()
    }

    /// Construct with an explicit provider (tests).
    #[cfg(test)]
    pub fn with_provider(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider: Some(provider),
            cache: Self::cache(100),
        }
    }
}

#[cfg(test)]
mod selection_tests {
    use super::{ProviderKind, choose_provider_kind};

    #[test]
    fn an_explicit_flag_picks_that_backend_when_it_is_keyed() {
        assert_eq!(
            choose_provider_kind(Some("groq"), true, true),
            ProviderKind::Groq
        );
        assert_eq!(
            choose_provider_kind(Some("GEMINI"), true, true),
            ProviderKind::Gemini
        );
    }

    #[test]
    fn an_explicit_flag_without_its_key_disables_the_feature() {
        // Never a silent fallback to the other vendor.
        assert_eq!(
            choose_provider_kind(Some("groq"), true, false),
            ProviderKind::None
        );
        assert_eq!(
            choose_provider_kind(Some("gemini"), false, true),
            ProviderKind::None
        );
    }

    #[test]
    fn no_flag_prefers_gemini_then_groq_then_off() {
        assert_eq!(choose_provider_kind(None, true, true), ProviderKind::Gemini);
        assert_eq!(choose_provider_kind(None, false, true), ProviderKind::Groq);
        assert_eq!(choose_provider_kind(None, false, false), ProviderKind::None);
        // An unrecognized flag falls back to auto rather than failing.
        assert_eq!(
            choose_provider_kind(Some("openai"), false, true),
            ProviderKind::Groq
        );
    }
}
