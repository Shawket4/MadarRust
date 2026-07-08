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
//!     with [`gemini::GeminiProvider`] and [`groq::GroqProvider`] as the two
//!     implementations, selected by the `AI_PROVIDER` env flag;
//!   * repeat questions are served from a short-TTL response cache.
//!
//! Adding a report is one entry in [`catalog::REPORTS`]; nothing else changes.

pub mod catalog;
pub mod executor;
pub mod gemini;
pub mod groq;
pub mod handlers;
pub mod prompt;
pub mod provider;
pub mod routes;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use provider::LlmProvider;

/// Which LLM backend to wire, decided from the `AI_PROVIDER` flag + which API
/// keys are present.
#[derive(Debug, PartialEq, Eq)]
enum ProviderKind {
    Gemini,
    Groq,
    None,
}

/// Pure selection logic (unit-testable without touching the environment):
///   * an explicit `AI_PROVIDER` (`gemini` / `groq`) picks that backend, but only
///     if its key is present — otherwise the feature is off (no silent fallback
///     to a provider the operator didn't choose);
///   * unset/unknown flag → auto: prefer Gemini, then Groq, else off.
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

/// Process-wide AI state shared with handlers via `web::Data`.
///
/// `provider` is `None` when no configured backend is available — the endpoint
/// then reports the feature as unavailable (503) instead of failing to start, so
/// the rest of the server runs unaffected.
pub struct AiState {
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub cache: moka::future::Cache<String, handlers::AiChatResponse>,
}

impl AiState {
    /// Build state, wiring the LLM provider chosen by `AI_PROVIDER`
    /// (`gemini` | `groq`; unset → whichever key is present, Gemini first). The
    /// response cache is small and short-lived: it collapses duplicate questions
    /// (a merchant re-asking, a dashboard re-mounting) without serving stale
    /// numbers.
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

#[cfg(test)]
mod selection_tests {
    use super::{ProviderKind, choose_provider_kind};

    #[test]
    fn explicit_flag_picks_that_backend_if_keyed() {
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
    fn explicit_flag_without_its_key_disables_no_silent_fallback() {
        // AI_PROVIDER=groq but only Gemini is keyed → OFF, never Gemini.
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
    fn no_flag_auto_prefers_gemini_then_groq() {
        assert_eq!(choose_provider_kind(None, true, true), ProviderKind::Gemini);
        assert_eq!(choose_provider_kind(None, false, true), ProviderKind::Groq);
        assert_eq!(choose_provider_kind(None, false, false), ProviderKind::None);
        // An unknown flag falls back to auto too.
        assert_eq!(
            choose_provider_kind(Some("openai"), false, true),
            ProviderKind::Groq
        );
    }
}
