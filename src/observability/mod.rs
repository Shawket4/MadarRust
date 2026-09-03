//! Error reporting to the self-hosted Sentry.
//!
//! Wiring follows the same shape as the AI provider (`src/ai/mod.rs`): the
//! credential lives in an env var, and when it is missing the feature simply
//! does not exist — no panic, no degraded mode, the server boots and behaves
//! exactly as it did before Sentry was introduced. That matters because the
//! same binary runs in dev, in the fuzz/load harnesses, and in CI, none of which
//! should be shipping events to the production issue stream.
//!
//! # The three parts
//!
//!   * [`scrub`] — the redaction layer. A **compliance control**; read its
//!     module docs before changing anything in it.
//!   * [`report`] — the one funnel for failures that never become a 5xx:
//!     handled errors, 200-with-an-error-body, and background work.
//!   * this module — SDK initialisation, the actix middleware, and the
//!     `tracing` bridge.
//!
//! # What produces an event
//!
//! | Path | Reported? |
//! |---|---|
//! | Handler returns `Err(AppError)` mapping to 5xx | yes, by [`middleware`] |
//! | Handler returns a 4xx | no — ordinary API traffic |
//! | A 4xx caused by *our own* data | yes, via [`report::report_data_fault`] |
//! | Handled failure, logged and shown to the user | yes, via [`report::report`] |
//! | 200 carrying a per-item error | yes, via [`report::report`] |
//! | Background tick returning an error, or panicking | yes, via [`report::guarded_tick`] |
//! | `tracing::error!` anywhere | breadcrumb only — never a second event |
//!
//! That last row is the invariant worth protecting: `sentry-actix` is the single
//! capture path for request errors, so the `tracing` layer only ever decorates
//! it. Letting `ERROR` through as an event would raise two issues per failure,
//! with different fingerprints, neither obviously the duplicate of the other.

pub mod report;
pub mod scrub;

// Re-exported at the original paths so existing call sites and tests keep
// working after the split into a directory module.
pub use scrub::{PII_KEY_ALLOWLIST, PII_KEY_DENYLIST, PII_KEY_EXACT, is_pii_key, scrub_event};

/// Environment name used when `SENTRY_ENVIRONMENT` is unset. Defaults to
/// `production` rather than `development`: an unlabelled deployment is far more
/// likely to be the VPS than a laptop (a laptop usually has no DSN at all), and
/// mislabelling prod events as dev hides real incidents.
const DEFAULT_ENVIRONMENT: &str = "production";

/// The distributed-tracing headers browsers and mobile clients send.
///
/// **Neither is CORS-safelisted.** If they are not in the backend's
/// `Access-Control-Allow-Headers`, the browser strips them at preflight and
/// every other piece of trace propagation is inert — the frontend believes it is
/// propagating and the backend never sees a trace. This constant exists so the
/// requirement is named in one place and can be asserted on; see
/// [`crate::observability::tests::trace_headers_survive_a_cors_preflight`].
pub const TRACE_HEADERS: &[&str] = &["sentry-trace", "baggage"];

/// Initialise Sentry from the environment, returning the guard that must be
/// held for the lifetime of the process (dropping it flushes and shuts the
/// transport down).
///
/// `None` means "not configured" and is the normal, quiet path: no `SENTRY_DSN`
/// (dev, tests, fuzz/load runs) or a DSN that does not parse. A malformed DSN is
/// a warning rather than a panic on purpose — a typo in `/opt/madar-rust/.env`
/// must not take the POS backend down.
///
/// Env vars:
///   * `SENTRY_DSN` — enables reporting; unset ⇒ disabled.
///   * `SENTRY_RELEASE` — defaults to the crate version.
///   * `SENTRY_ENVIRONMENT` — defaults to [`DEFAULT_ENVIRONMENT`].
///   * `SENTRY_TRACES_SAMPLE_RATE` — defaults to `0.0`. Note this controls
///     whether transactions are *sent*, not whether they are created; see
///     [`middleware`].
pub fn init() -> Option<sentry::ClientInitGuard> {
    let raw_dsn = std::env::var("SENTRY_DSN").unwrap_or_default();
    let raw_dsn = raw_dsn.trim();
    if raw_dsn.is_empty() {
        // Logged, like the AI provider does, so an operator who expected
        // reporting to be on can tell "off" from "broken".
        tracing::info!("Sentry disabled (set SENTRY_DSN to enable error reporting)");
        return None;
    }

    let dsn = match raw_dsn.parse::<sentry::types::Dsn>() {
        Ok(dsn) => dsn,
        Err(e) => {
            tracing::warn!("Sentry disabled — SENTRY_DSN is not a valid DSN: {}", e);
            return None;
        }
    };

    let release = std::env::var("SENTRY_RELEASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("madar-rust@{}", env!("CARGO_PKG_VERSION")));
    let environment = std::env::var("SENTRY_ENVIRONMENT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENVIRONMENT.to_string());
    let traces_sample_rate = parse_sample_rate(std::env::var("SENTRY_TRACES_SAMPLE_RATE").ok());

    let mut options = sentry::ClientOptions::new()
        .release(release.clone())
        .environment(environment.clone())
        // Stacktrace on plain messages too — the release binary is stripped, so
        // a frame list is often all we get.
        .attach_stacktrace(true)
        // COMPLIANCE: no IPs, no request bodies, no sensitive headers collected
        // by the SDK in the first place. `scrub_event` is the second line, for
        // data WE attach — and it also clears `user` and `server_name` outright
        // so a future SDK release cannot quietly widen what "default" covers.
        .send_default_pii(false)
        .traces_sample_rate(traces_sample_rate)
        .before_send(scrub_event);
    // Set by field rather than `.dsn()`, which panics on a parse failure — we
    // parsed above precisely so a bad value degrades to "off".
    options.dsn = Some(dsn);

    tracing::info!(
        "Sentry enabled (env={}, release={}, traces_sample_rate={})",
        environment,
        release,
        traces_sample_rate
    );
    Some(sentry::init(options))
}

/// The actix middleware.
///
/// # Ordering
///
/// This must be the **outermost** middleware, which in actix means the **last**
/// `.wrap()` call — actix applies wraps in reverse of how they read. Everything
/// downstream then runs on this request's hub, so anything a handler captures
/// carries the request's context and nothing else.
///
/// The classic ordering hazard — a hub layer and a request layer applied the
/// wrong way round, so the request layer decorates the *global* scope and leaks
/// an event processor per request — does not apply to `sentry-actix`, because
/// hub creation and request instrumentation are the same layer here: `call()`
/// builds a `Hub::new_from_top` and only then adds the event processor to *that*
/// hub's scope. Verified against the SDK source rather than assumed, and pinned
/// by [`tests::events_carry_only_their_own_requests_context`].
///
/// # Why a transaction is always started
///
/// `start_transaction(true)` is unconditional, even when
/// `SENTRY_TRACES_SAMPLE_RATE` is `0`. The sample rate decides whether a
/// transaction is **sent**; starting one is what makes `sentry-actix` call
/// `TransactionContext::continue_from_headers` and put the resulting span on the
/// request scope — and *that* is what stamps the incoming `sentry-trace` id onto
/// **error events**. Without it, an error from the backend and the frontend
/// error that caused it live in two unrelated traces, which defeats the point of
/// propagating anything.
///
/// This is a deliberate change from the previous behaviour, which only started a
/// transaction when the sample rate was above zero and therefore never continued
/// a trace on the default configuration.
pub fn middleware() -> sentry_actix::Sentry {
    middleware_for(None)
}

/// [`middleware`] with an explicit hub.
///
/// This exists so tests exercise the **real** construction path rather than a
/// copy of it. A test that rebuilds the builder itself passes whatever
/// `middleware()` is configured to do, which makes it useless for proving that
/// `middleware()` is configured correctly — it will happily stay green after
/// the setting under test is reverted.
pub fn middleware_for(hub: Option<std::sync::Arc<sentry::Hub>>) -> sentry_actix::Sentry {
    let mut builder = sentry_actix::Sentry::builder();
    if let Some(hub) = hub {
        builder = builder.with_hub(hub);
    }
    builder
        // Unconditional — see the doc comment on `middleware`. The sample rate
        // decides whether a transaction is SENT; starting one is what continues
        // the caller's trace onto error events.
        .start_transaction(true)
        .capture_server_errors(true)
        .finish()
}

/// The `tracing` → Sentry bridge, as a `tracing-subscriber` layer.
///
/// # Why this exists
///
/// sqlx narrates itself: every statement it runs is emitted as a `tracing` event
/// on the `sqlx::query` target carrying the SQL and its elapsed time. Bridging
/// that into Sentry means an issue arrives with the last ~100 queries that ran
/// on that request and how long each took, which is usually the whole diagnosis
/// for a report endpoint that timed out or a handler that 500'd on a lock wait.
///
/// # The mapping, and why it is not the default one
///
/// `sentry_tracing::default_event_filter` turns every `ERROR`-level event into a
/// full Sentry **event**. That is wrong here: `sentry-actix` already captures
/// 5xx responses, so a handler that logs `tracing::error!` and then returns a
/// 5xx-mapped `AppError` — the normal shape in this codebase — would raise
/// **two** issues for one failure.
///
/// So the rule is: this layer NEVER produces an event. It only produces
/// breadcrumbs, attached to whatever event the middleware or [`report`]
/// captures.
///
/// # PII
///
/// [`scrub_event`] runs on events, and now also sanitizes breadcrumb *messages*.
/// It does NOT run on span data. That asymmetry is why [`crate::reports`] and
/// [`crate::insights`] instrument with `skip_all` and re-add only ids and date
/// ranges: an argument recorded into a span reaches Sentry unfiltered.
/// `span_filter` restricting to `madar_rust` targets is the second half of that
/// guarantee — the only spans on the wire are ones whose fields we chose by
/// hand.
pub fn tracing_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer;

    sentry_tracing::layer()
        .event_filter(|metadata| event_filter_for(*metadata.level()))
        .span_filter(|metadata| metadata.target().starts_with("madar_rust"))
        .enable_span_attributes()
        // A filter of its own rather than riding on the global `RUST_LOG`. The
        // console and the breadcrumb trail want different things: prod runs
        // `RUST_LOG=info` and the load-test rig runs `RUST_LOG=warn`, and under
        // the latter the `sqlx::query` INFO events — the entire point of this
        // layer — would never be emitted at all.
        .with_filter(sentry_tracing_filter())
}

/// Level → Sentry action for [`tracing_layer`]. Split out so the
/// no-double-report invariant is testable: this must never return
/// [`sentry_tracing::EventFilter::Event`] for ANY level.
fn event_filter_for(level: tracing::Level) -> sentry_tracing::EventFilter {
    use sentry_tracing::EventFilter;
    use tracing::Level;
    match level {
        Level::ERROR | Level::WARN | Level::INFO => EventFilter::Breadcrumb,
        Level::DEBUG | Level::TRACE => EventFilter::Ignore,
    }
}

/// Filter directives for [`tracing_layer`]. `sqlx::query=info` is the
/// load-bearing part: it is what puts executed statements and their timings in
/// the breadcrumb trail.
fn sentry_tracing_filter() -> tracing_subscriber::EnvFilter {
    let raw = std::env::var("SENTRY_TRACING_FILTER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "info,sqlx::query=info".to_string());
    tracing_subscriber::EnvFilter::try_new(&raw).unwrap_or_else(|e| {
        // Same degrade-don't-die posture as a bad DSN: a typo in `.env` must not
        // stop the server booting.
        tracing::warn!("SENTRY_TRACING_FILTER is invalid ({e}); falling back to the default");
        tracing_subscriber::EnvFilter::new("info,sqlx::query=info")
    })
}

/// Parse `SENTRY_TRACES_SAMPLE_RATE`, clamped to the 0.0–1.0 the SDK accepts (it
/// panics outside that range, and a misconfigured env var must not be able to
/// stop the server booting).
fn parse_sample_rate(raw: Option<String>) -> f32 {
    raw.and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The invariant that keeps one failure to one issue.
    #[test]
    fn tracing_layer_never_captures_a_second_event() {
        use sentry_tracing::EventFilter;
        use tracing::Level;

        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert!(
                !event_filter_for(level).contains(EventFilter::Event),
                "{level} must not produce a Sentry event — sentry-actix already reports it"
            );
        }
        for level in [Level::ERROR, Level::WARN, Level::INFO] {
            assert!(event_filter_for(level).contains(EventFilter::Breadcrumb));
        }
        for level in [Level::DEBUG, Level::TRACE] {
            assert!(event_filter_for(level).is_empty());
        }
    }

    #[test]
    fn sentry_tracing_filter_defaults_to_capturing_sqlx_queries() {
        // SAFETY: single-threaded assertion on process env, restored immediately.
        unsafe { std::env::remove_var("SENTRY_TRACING_FILTER") };
        assert!(sentry_tracing_filter().to_string().contains("sqlx::query"));
        unsafe { std::env::set_var("SENTRY_TRACING_FILTER", "=====") };
        assert!(sentry_tracing_filter().to_string().contains("sqlx::query"));
        unsafe { std::env::remove_var("SENTRY_TRACING_FILTER") };
    }

    #[test]
    fn sqlx_queries_become_breadcrumbs_on_a_captured_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let events = sentry::test::with_captured_events(|| {
            let subscriber = tracing_subscriber::registry().with(tracing_layer());
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(
                    target: "sqlx::query",
                    summary = "SELECT ... FROM orders",
                    elapsed_secs = 0.42,
                    "slow-ish query"
                );
                tracing::debug!(target: "sqlx::query", "per-row chatter");
                sentry::capture_message("boom", sentry::Level::Error);
            });
        });

        assert_eq!(events.len(), 1);
        let wire = serde_json::to_string(&events[0].breadcrumbs.values).unwrap();
        assert!(wire.contains("FROM orders"), "query trail missing: {wire}");
        assert!(wire.contains("0.42"), "query timing missing: {wire}");
        assert!(!wire.contains("per-row chatter"), "DEBUG must be ignored");
    }

    #[test]
    fn sample_rate_never_panics_the_sdk() {
        assert_eq!(parse_sample_rate(None), 0.0);
        assert_eq!(parse_sample_rate(Some("".into())), 0.0);
        assert_eq!(parse_sample_rate(Some("nonsense".into())), 0.0);
        assert_eq!(parse_sample_rate(Some(" 0.25 ".into())), 0.25);
        assert_eq!(parse_sample_rate(Some("7".into())), 1.0);
        assert_eq!(parse_sample_rate(Some("-1".into())), 0.0);
        assert_eq!(parse_sample_rate(Some("NaN".into())), 0.0);
    }

    /// Build a client + hub with a test transport, so assertions are made on
    /// the event **as it goes out on the wire** rather than on the code that
    /// builds it. Tags set on the wrong hub still produce an event; it just
    /// arrives empty, which looks like it works.
    fn test_hub(traces_sample_rate: f32) -> (Arc<sentry::Hub>, Arc<sentry::test::TestTransport>) {
        let transport = sentry::test::TestTransport::new();
        let options = sentry::ClientOptions::new()
            .dsn("https://public@example.invalid/1")
            .transport(transport.clone())
            .send_default_pii(false)
            .traces_sample_rate(traces_sample_rate)
            // Force the SDK to collect the body it would normally skip, so the
            // assertion below proves OUR hook removes it rather than proving a
            // default happened to be off.
            .max_request_body_size(sentry::MaxRequestBodySize::Always)
            .before_send(scrub_event);
        let hub = Arc::new(sentry::Hub::new(
            Some(Arc::new(options.into())),
            Default::default(),
        ));
        (hub, transport)
    }

    /// End to end through the real middleware and a fake transport: this is the
    /// test that actually backs the privacy claim.
    ///
    /// It also pins the noise policy: a 500 is an incident and is reported, a
    /// 4xx is ordinary API traffic and is not. `AppError`'s SQLSTATE mapping
    /// means the *same* variant can be either, so the split has to live at the
    /// status code, which is where sentry-actix puts it.
    #[actix_web::test]
    async fn middleware_reports_5xx_only_and_strips_the_request_body() {
        use crate::errors::AppError;
        use actix_web::{App, HttpResponse, test, web};

        let (hub, transport) = test_hub(0.0);
        let app = test::init_service(
            App::new()
                .wrap(sentry_actix::Sentry::builder().with_hub(hub).finish())
                .route(
                    "/boom",
                    web::post().to(|| async { Err::<HttpResponse, AppError>(AppError::Internal) }),
                )
                .route(
                    "/reject",
                    web::post().to(|| async {
                        Err::<HttpResponse, AppError>(AppError::BadRequest("nope".into()))
                    }),
                ),
        )
        .await;

        let body = serde_json::json!({
            "customer_name": "Ali",
            "customer_phone": "+201000000000",
            "address_line": "12 Main St",
        });

        let req = test::TestRequest::post()
            .uri("/reject?customer_phone=%2B201000000000")
            .set_json(&body)
            .to_request();
        assert!(
            test::call_service(&app, req)
                .await
                .status()
                .is_client_error()
        );
        assert!(
            transport.fetch_and_clear_events().is_empty(),
            "expected 4xx must not be reported"
        );

        let req = test::TestRequest::post()
            .uri("/boom?customer_phone=%2B201000000000")
            .insert_header(("authorization", "Bearer secret-token"))
            .set_json(&body)
            .to_request();
        assert!(
            test::call_service(&app, req)
                .await
                .status()
                .is_server_error()
        );

        let events = transport.fetch_and_clear_events();
        assert_eq!(events.len(), 1, "a 500 must be reported");
        let request = events[0].request.clone().expect("request context attached");
        assert!(request.data.is_none(), "request body must never be sent");
        assert!(request.cookies.is_none());
        assert!(request.query_string.is_none());
        assert_eq!(request.url.as_ref().and_then(|u| u.query()), None);
        // Belt and braces over the whole serialized event — a debug
        // representation of the response object would never have rendered the
        // body this is claiming is clean.
        let wire = serde_json::to_string(&events[0]).unwrap();
        for leak in ["+201000000000", "12 Main St", "secret-token", "Ali"] {
            assert!(!wire.contains(leak), "event leaked {leak}: {wire}");
        }
        assert!(events[0].user.is_none(), "user must be cleared outright");
        assert!(events[0].server_name.is_none());
    }

    /// The point of propagation is ONE trace, not three.
    ///
    /// With `traces_sample_rate = 0` no transaction is ever sent, so the only
    /// way the ends line up is if the incoming trace id reaches the **error
    /// event**. That is what `start_transaction(true)` buys, and it is exactly
    /// what the previous conditional wiring did not do.
    #[actix_web::test]
    async fn an_error_event_continues_the_incoming_trace_even_with_tracing_off() {
        use crate::errors::AppError;
        use actix_web::{App, HttpResponse, test, web};

        let (hub, transport) = test_hub(0.0);
        let app = test::init_service(App::new().wrap(middleware_for(Some(hub))).route(
            "/boom",
            web::get().to(|| async { Err::<HttpResponse, AppError>(AppError::Internal) }),
        ))
        .await;

        let trace_id = "d49d9bf66f13450b81f65bc51cf49c03";
        let req = test::TestRequest::get()
            .uri("/boom")
            .insert_header(("sentry-trace", format!("{trace_id}-a0b1c2d3e4f56789-1")))
            .insert_header(("baggage", "sentry-environment=production"))
            .to_request();
        assert!(
            test::call_service(&app, req)
                .await
                .status()
                .is_server_error()
        );

        let events = transport.fetch_and_clear_events();
        assert_eq!(events.len(), 1);

        // Assert on the TRACE CONTEXT, not on the serialized event. The
        // incoming `sentry-trace` header is echoed back in
        // `event.request.headers`, so a substring search over the whole event
        // passes whether or not the trace was actually continued — it would
        // stay green with `start_transaction(false)`, which is the bug this
        // test exists to catch.
        let trace = match events[0].contexts.get("trace") {
            Some(sentry::protocol::Context::Trace(t)) => t.clone(),
            other => panic!("the error event carries no trace context: {other:?}"),
        };
        assert_eq!(
            trace.trace_id.to_string(),
            trace_id,
            "the error event started its own trace instead of joining the caller's"
        );
    }

    /// Middleware ORDER, asserted rather than assumed: each request's event
    /// must carry only its own context.
    ///
    /// If hub creation and request instrumentation were split across two layers
    /// applied the wrong way round, the second request's event would still
    /// carry the first request's URL — and the process would leak one event
    /// processor per request.
    #[actix_web::test]
    async fn events_carry_only_their_own_requests_context() {
        use crate::errors::AppError;
        use actix_web::{App, HttpResponse, test, web};

        let (hub, transport) = test_hub(0.0);
        let app = test::init_service(App::new().wrap(middleware_for(Some(hub))).route(
            "/boom/{id}",
            web::get().to(|| async { Err::<HttpResponse, AppError>(AppError::Internal) }),
        ))
        .await;

        for id in ["first-request", "second-request"] {
            let req = test::TestRequest::get()
                .uri(&format!("/boom/{id}"))
                .to_request();
            let _ = test::call_service(&app, req).await;
        }

        let events = transport.fetch_and_clear_events();
        assert_eq!(events.len(), 2);
        let second = serde_json::to_string(&events[1]).unwrap();
        assert!(
            second.contains("second-request"),
            "the event lost its own request: {second}"
        );
        assert!(
            !second.contains("first-request"),
            "the event picked up a previous request's context: {second}"
        );
    }

    /// `sentry-trace` and `baggage` are NOT CORS-safelisted. If the preflight
    /// does not allow them the browser strips them, and every other piece of
    /// trace propagation is inert.
    ///
    /// This test builds CORS **the way `main` does**, and asserts on the
    /// headers the app actually needs as well as the trace ones.
    ///
    /// The earlier version of this test only checked that `sentry-trace` and
    /// `baggage` were allowed. It passed against a configuration that had
    /// silently restricted CORS to *only* those two headers — because
    /// `actix-cors` downgrades `AllOrSome::All` to `Some(set)` on the first
    /// `allowed_headers()` call. Preflight for `Authorization` then 400s and
    /// every authenticated cross-origin request fails. Asserting only on what
    /// you added is how a change that breaks everything else looks green.
    #[actix_web::test]
    async fn a_preflight_allows_the_headers_the_app_actually_sends() {
        use actix_cors::Cors;
        use actix_web::{App, HttpResponse, test, web};

        // Mirrors `main.rs`. If that changes, this must change with it.
        let cors = Cors::default()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600)
            .allowed_origin("https://dashboard.madar-pos.cloud");

        let app = test::init_service(
            App::new()
                .wrap(cors)
                .route("/orders", web::post().to(|| async { HttpResponse::Ok().finish() })),
        )
        .await;

        // Every header the dashboard and the SDKs actually send.
        let required: Vec<&str> = ["authorization", "content-type", "x-org-id", "x-branch-id"]
            .into_iter()
            .chain(TRACE_HEADERS.iter().copied())
            .collect();

        for header in &required {
            let req = test::TestRequest::default()
                .method(actix_web::http::Method::OPTIONS)
                .uri("/orders")
                .insert_header(("Origin", "https://dashboard.madar-pos.cloud"))
                .insert_header(("Access-Control-Request-Method", "POST"))
                .insert_header(("Access-Control-Request-Headers", *header))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert!(
                resp.status().is_success(),
                "preflight rejected '{header}' ({}) — cross-origin requests carrying it will fail",
                resp.status()
            );
        }

        // ...and all of them together, which is what a browser actually sends.
        let joined = required.join(",");
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::OPTIONS)
            .uri("/orders")
            .insert_header(("Origin", "https://dashboard.madar-pos.cloud"))
            .insert_header(("Access-Control-Request-Method", "POST"))
            .insert_header(("Access-Control-Request-Headers", joined.as_str()))
            .to_request();
        assert!(
            test::call_service(&app, req).await.status().is_success(),
            "preflight rejected the real header set"
        );
    }
}
