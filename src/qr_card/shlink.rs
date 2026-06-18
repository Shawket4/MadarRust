//! Shlink REST client + `ShortLinkProvider` trait for dependency injection.
//!
//! The real client reads `SHLINK_BASE_URL` and `SHLINK_API_KEY` from the
//! environment on each call. If either is unset, it returns
//! `AppError::ServiceUnavailable` — degrade-safe, the server keeps running.
//!
//! Tests inject a `FakeShortLinkProvider` to avoid a live Shlink instance.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;

use crate::errors::AppError;

/// A successfully created (or looked-up) Shlink short URL.
#[derive(Debug, Clone)]
pub struct ShortUrl {
    pub short_code: String,
    pub short_url: String,
}

/// Alias for a boxed, pinned, `Send` future — lets `ShortLinkProvider` be
/// object-safe (usable as `Arc<dyn ShortLinkProvider>`).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trait over the short-URL creation operation.  The single method must return
/// a boxed future so that `dyn ShortLinkProvider` is object-safe.
pub trait ShortLinkProvider: Send + Sync + 'static {
    fn create_short_url<'a>(
        &'a self,
        long_url: &'a str,
        custom_slug: Option<&'a str>,
        tags: &'a [String],
    ) -> BoxFut<'a, Result<ShortUrl, AppError>>;
}

// ── Shlink JSON shapes ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ShlinkShortUrlResponse {
    #[serde(rename = "shortCode")]
    short_code: String,
    #[serde(rename = "shortUrl")]
    short_url: String,
}

// ── Real client ───────────────────────────────────────────────────────────────

/// Calls the Shlink v3 REST API.  Reads env vars on every call so the
/// values can be hot-reloaded without a restart (matches the OSRM pattern).
pub struct ShlinkClient;

impl ShortLinkProvider for ShlinkClient {
    fn create_short_url<'a>(
        &'a self,
        long_url: &'a str,
        custom_slug: Option<&'a str>,
        tags: &'a [String],
    ) -> BoxFut<'a, Result<ShortUrl, AppError>> {
        Box::pin(async move {
            let base = std::env::var("SHLINK_BASE_URL").map_err(|_| {
                AppError::ServiceUnavailable("short-link service not configured".into())
            })?;
            let key = std::env::var("SHLINK_API_KEY").map_err(|_| {
                AppError::ServiceUnavailable("short-link service not configured".into())
            })?;
            let base = base.trim_end_matches('/');
            if base.is_empty() || key.is_empty() {
                return Err(AppError::ServiceUnavailable(
                    "short-link service not configured".into(),
                ));
            }

            let url = format!("{base}/rest/v3/short-urls");

            let mut body = serde_json::json!({
                "longUrl": long_url,
                "tags": tags,
                "findIfExists": true,
            });
            if let Some(slug) = custom_slug {
                body["customSlug"] = serde_json::Value::String(slug.to_string());
            }

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|_| {
                    AppError::ServiceUnavailable("could not build HTTP client".into())
                })?;

            let resp = client
                .post(&url)
                .header("X-Api-Key", key)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "Shlink request failed");
                    AppError::ServiceUnavailable("short-link service unreachable".into())
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                tracing::warn!(status = %status, "Shlink returned non-2xx");
                return Err(AppError::ServiceUnavailable(format!(
                    "short-link service returned {status}"
                )));
            }

            let data: ShlinkShortUrlResponse = resp.json().await.map_err(|e| {
                tracing::warn!(error = %e, "Shlink response parse error");
                AppError::ServiceUnavailable("short-link service returned unexpected body".into())
            })?;

            Ok(ShortUrl {
                short_code: data.short_code,
                short_url: data.short_url,
            })
        })
    }
}

// ── Fake provider (tests) ─────────────────────────────────────────────────────

#[cfg(test)]
pub mod fake {
    use super::{BoxFut, ShortLinkProvider, ShortUrl};
    use crate::errors::AppError;
    use std::sync::{Arc, Mutex};

    /// Deterministic fake: returns `https://sfx.link/<slug>` where `<slug>` is
    /// `custom_slug` when given, otherwise a counter-based value.
    pub struct FakeShortLinkProvider {
        counter: Arc<Mutex<u32>>,
    }

    impl FakeShortLinkProvider {
        pub fn new() -> Self {
            Self {
                counter: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl ShortLinkProvider for FakeShortLinkProvider {
        fn create_short_url<'a>(
            &'a self,
            _long_url: &'a str,
            custom_slug: Option<&'a str>,
            _tags: &'a [String],
        ) -> BoxFut<'a, Result<ShortUrl, AppError>> {
            let counter = Arc::clone(&self.counter);
            Box::pin(async move {
                let code = if let Some(slug) = custom_slug {
                    slug.to_string()
                } else {
                    let mut n = counter.lock().unwrap();
                    *n += 1;
                    format!("fake{n:04}")
                };
                Ok(ShortUrl {
                    short_url: format!("https://sfx.link/{code}"),
                    short_code: code,
                })
            })
        }
    }
}
