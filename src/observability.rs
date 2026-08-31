//! Error reporting to the self-hosted Sentry.
//!
//! Wiring follows the same shape as the AI provider (`src/ai/mod.rs`): the
//! credential lives in an env var, and when it is missing the feature simply
//! does not exist — no panic, no degraded mode, the server boots and behaves
//! exactly as it did before Sentry was introduced. That matters because the
//! same binary runs in dev, in the fuzz/load harnesses, and in CI, none of
//! which should be shipping events to the production issue stream.
//!
//! The other half of this module is the scrubber. Everything an event can
//! carry is filtered through [`scrub_event`] before it leaves the process; see
//! [`PII_KEY_DENYLIST`] for why that is a hard requirement rather than a
//! nicety.

use sentry::protocol::{Event, Map, Value};

/// Field names whose values must never reach the Sentry server.
///
/// This is a COMPLIANCE control, not defence-in-depth: the published Madar
/// privacy policy tells merchants and their customers that crash/error reports
/// exclude personal data, so that statement has to be true on the wire. This
/// database holds `customer_phone`, `customer_name`, `address_line`,
/// `national_id`, `base_salary_piastres` and delivery/attendance GPS fixes —
/// all of which routinely appear in query parameters, JSON bodies, and the
/// `extra` map of an error event.
///
/// Matching is case-insensitive substring, so `customer_phone`,
/// `phone_number` and `PHONE` all match the single entry `phone`. Entries are
/// deliberately long enough not to collide with innocent keys (`latitude`, not
/// `lat`, which would also hit `translation` and `related`) — a false positive
/// here silently destroys the debugging value of an event, while a miss leaks
/// customer data. When in doubt, add the longer spelling.
pub const PII_KEY_DENYLIST: &[&str] = &[
    // Contact details
    "phone",
    "mobile",
    "email",
    "customer_name",
    "full_name",
    // Addresses / geography (delivery drops, attendance geofence fixes)
    "address",
    "latitude",
    "longitude",
    "coordinate",
    // Government / staff identity and pay
    "national_id",
    "salary",
    "wage",
    "payslip",
    // Credentials — never useful in an issue, always harmful in one
    "password",
    "passwd",
    "secret",
    "token",
    "authorization",
    "api_key",
    "apikey",
    "credential",
    "cookie",
    "jwt",
];

/// What a denied value is replaced with. A visible marker (rather than dropping
/// the key) so an engineer reading the issue knows the field existed and was
/// scrubbed, instead of hunting for a payload that looks truncated.
const REDACTED: &str = "[redacted]";

/// Environment name used when `SENTRY_ENVIRONMENT` is unset. Defaults to
/// `production` rather than `development`: an unlabelled deployment is far more
/// likely to be the VPS than a laptop (a laptop usually has no DSN at all), and
/// mislabelling prod events as dev hides real incidents.
const DEFAULT_ENVIRONMENT: &str = "production";

/// True when `key` matches [`PII_KEY_DENYLIST`].
pub fn is_pii_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    PII_KEY_DENYLIST.iter().any(|needle| key.contains(needle))
}

/// Redact denied keys anywhere inside an arbitrary JSON value.
///
/// Recurses through objects and arrays because the shapes that carry PII here
/// are nested — an order event's `extra` is a whole order payload, with the
/// customer block several levels down. A denied key is redacted whatever its
/// type: replacing the *entire* subtree is intentional, since `customer` is
/// only ever an object and dropping it wholesale is the safe reading.
fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_pii_key(k) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact_value(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        _ => {}
    }
}

/// Redact denied keys in a flat string map (event tags, request headers).
fn redact_string_map(map: &mut Map<String, String>) {
    for (k, v) in map.iter_mut() {
        if is_pii_key(k) {
            *v = REDACTED.to_string();
        }
    }
}

/// The `before_send` hook: strip everything that could carry customer or staff
/// personal data, then let the event through.
///
/// Returning `Some` unconditionally is deliberate — filtering by *level* is the
/// job of the capture site (`sentry-actix` only reports 5xx; see
/// [`init`]), while this hook's single responsibility is redaction. Keeping the
/// two separate means a future capture path cannot accidentally bypass the
/// scrubber.
pub fn scrub_event(mut event: Event<'static>) -> Option<Event<'static>> {
    if let Some(request) = event.request.as_mut() {
        // The body is the biggest single exposure: every POST /orders carries a
        // customer name, phone and address. There is no safe subset, so it goes
        // whole.
        request.data = None;
        // Cookies and the query string are unstructured strings — we cannot
        // redact per-key reliably, and a session cookie is a live credential.
        request.cookies = None;
        request.query_string = None;
        // The URL is kept (it is the single most useful field on an issue) but
        // without its query, which is where `?phone=` and `?lat=` ride.
        if let Some(url) = request.url.as_mut() {
            url.set_query(None);
        }
        // sentry-actix already drops `authorization`-style headers when
        // send_default_pii is false; this catches the custom ones (X-Org-Id is
        // fine, an X-... device or customer header is not) and anything a future
        // integration adds.
        redact_string_map(&mut request.headers);
        redact_string_map(&mut request.env);
    }

    // A user context is only ever set deliberately, but scrub the identifying
    // fields anyway so the privacy claim does not depend on call-site
    // discipline. `id` (an opaque UUID) is kept: it is what makes an issue
    // actionable and is not personal data on its own.
    if let Some(user) = event.user.as_mut() {
        user.email = None;
        user.username = None;
        user.ip_address = None;
        for (k, v) in user.other.iter_mut() {
            if is_pii_key(k) {
                *v = Value::String(REDACTED.to_string());
            } else {
                redact_value(v);
            }
        }
    }

    redact_string_map(&mut event.tags);
    for (k, v) in event.extra.iter_mut() {
        if is_pii_key(k) {
            *v = Value::String(REDACTED.to_string());
        } else {
            redact_value(v);
        }
    }
    for breadcrumb in event.breadcrumbs.values.iter_mut() {
        for (k, v) in breadcrumb.data.iter_mut() {
            if is_pii_key(k) {
                *v = Value::String(REDACTED.to_string());
            } else {
                redact_value(v);
            }
        }
    }

    Some(event)
}

/// Initialise Sentry from the environment, returning the guard that must be
/// held for the lifetime of the process (dropping it flushes and shuts the
/// transport down).
///
/// `None` means "not configured" and is the normal, quiet path: no `SENTRY_DSN`
/// (dev, tests, fuzz/load runs) or a DSN that does not parse. A malformed DSN
/// is a warning rather than a panic on purpose — a typo in
/// `/opt/madar-rust/.env` must not take the POS backend down.
///
/// Env vars:
///   * `SENTRY_DSN` — enables reporting; unset ⇒ disabled.
///   * `SENTRY_RELEASE` — defaults to the crate version, so untagged deploys
///     still group sensibly.
///   * `SENTRY_ENVIRONMENT` — defaults to [`DEFAULT_ENVIRONMENT`].
///   * `SENTRY_TRACES_SAMPLE_RATE` — defaults to `0.0` (performance tracing
///     off). The self-hosted instance shares the VPS with Postgres and the
///     backend, so tracing stays opt-in per deployment.
pub fn init() -> Option<sentry::ClientInitGuard> {
    let raw_dsn = std::env::var("SENTRY_DSN").unwrap_or_default();
    let raw_dsn = raw_dsn.trim();
    if raw_dsn.is_empty() {
        // Logged, like the AI provider does, so an operator who expected
        // reporting to be on can tell the difference between "off" and "broken".
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
        // The privacy claim starts here: no IPs, no request bodies, no
        // sensitive headers collected by the SDK in the first place. The
        // scrubber below is the second line, for data WE attach.
        .send_default_pii(false)
        .traces_sample_rate(traces_sample_rate)
        .before_send(scrub_event);
    // Set by field rather than `.dsn()`, which panics on a parse failure — we
    // already parsed it above precisely so a bad value degrades to "off".
    options.dsn = Some(dsn);

    tracing::info!(
        "Sentry enabled (env={}, release={}, traces_sample_rate={})",
        environment,
        release,
        traces_sample_rate
    );
    Some(sentry::init(options))
}

/// The actix middleware, configured to start a performance transaction per
/// request only when `SENTRY_TRACES_SAMPLE_RATE` asks for one.
///
/// Both arms are the same middleware and both report 5xx; the difference is
/// that `with_transaction` allocates a transaction on every single request.
/// With tracing off (the default) that is pure overhead on a 1-vCPU box serving
/// a POS, so the default path stays the cheap one and the env var stays
/// meaningful instead of inert.
pub fn middleware() -> sentry_actix::Sentry {
    if parse_sample_rate(std::env::var("SENTRY_TRACES_SAMPLE_RATE").ok()) > 0.0 {
        sentry_actix::Sentry::with_transaction()
    } else {
        sentry_actix::Sentry::new()
    }
}

/// Parse `SENTRY_TRACES_SAMPLE_RATE`, clamped to the 0.0–1.0 the SDK accepts
/// (it panics outside that range, and a misconfigured env var must not be able
/// to stop the server booting). Unparseable or absent ⇒ tracing off.
fn parse_sample_rate(raw: Option<String>) -> f32 {
    raw.and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::protocol::{Breadcrumb, Request, User};

    #[test]
    fn denylist_matches_real_column_names() {
        // The exact fields the privacy policy promises never to ship.
        for key in [
            "customer_phone",
            "customer_name",
            "address_line",
            "national_id",
            "base_salary_piastres",
            "latitude",
            "longitude",
            "Authorization",
            "X-Auth-Token",
        ] {
            assert!(is_pii_key(key), "{key} must be denied");
        }
    }

    #[test]
    fn denylist_does_not_swallow_innocent_keys() {
        // A denylist that eats ordinary fields destroys the debugging value of
        // every event, so guard the near-misses explicitly.
        for key in [
            "order_id",
            "branch_id",
            "translation",
            "related_items",
            "status",
            "line_cost",
            "quantity",
        ] {
            assert!(!is_pii_key(key), "{key} must NOT be denied");
        }
    }

    #[test]
    fn nested_pii_is_redacted() {
        let mut v: Value = serde_json::json!({
            "order_id": "abc",
            "customer": { "customer_name": "Ali", "customer_phone": "+2010", "id": 7 },
            "drops": [{ "latitude": 30.1, "longitude": 31.2, "sequence": 1 }]
        });
        redact_value(&mut v);
        assert_eq!(v["order_id"], "abc");
        assert_eq!(v["customer"]["customer_name"], REDACTED);
        assert_eq!(v["customer"]["customer_phone"], REDACTED);
        assert_eq!(v["customer"]["id"], 7);
        assert_eq!(v["drops"][0]["latitude"], REDACTED);
        assert_eq!(v["drops"][0]["sequence"], 1);
    }

    #[test]
    fn scrub_event_strips_request_body_cookies_and_query() {
        let mut event = Event::default();
        let mut request = Request {
            url: "https://api.madar-pos.cloud/orders?customer_phone=%2B2010"
                .parse()
                .ok(),
            method: Some("POST".into()),
            data: Some(r#"{"customer_phone":"+2010"}"#.into()),
            query_string: Some("customer_phone=%2B2010".into()),
            cookies: Some("session=abc".into()),
            ..Default::default()
        };
        request
            .headers
            .insert("authorization".into(), "Bearer xyz".into());
        request.headers.insert("x-org-id".into(), "org-1".into());
        event.request = Some(request);
        event
            .extra
            .insert("customer_phone".into(), serde_json::json!("+2010"));
        event
            .tags
            .insert("address_line".into(), "12 Main St".into());
        event.breadcrumbs.values.push(Breadcrumb {
            data: [("national_id".to_string(), serde_json::json!("123"))]
                .into_iter()
                .collect(),
            ..Default::default()
        });
        event.user = Some(User {
            id: Some("user-uuid".into()),
            email: Some("a@b.c".into()),
            username: Some("ali".into()),
            ..Default::default()
        });

        let event = scrub_event(event).expect("event must still be sent");
        let request = event.request.unwrap();
        assert!(request.data.is_none());
        assert!(request.cookies.is_none());
        assert!(request.query_string.is_none());
        assert_eq!(request.url.unwrap().query(), None);
        assert_eq!(request.headers["authorization"], REDACTED);
        assert_eq!(request.headers["x-org-id"], "org-1");
        assert_eq!(event.extra["customer_phone"], REDACTED);
        assert_eq!(event.tags["address_line"], REDACTED);
        assert_eq!(event.breadcrumbs.values[0].data["national_id"], REDACTED);
        let user = event.user.unwrap();
        assert_eq!(user.id.as_deref(), Some("user-uuid"));
        assert!(user.email.is_none());
        assert!(user.username.is_none());
    }

    #[test]
    fn sample_rate_never_panics_the_sdk() {
        assert_eq!(parse_sample_rate(None), 0.0);
        assert_eq!(parse_sample_rate(Some("".into())), 0.0);
        assert_eq!(parse_sample_rate(Some("nonsense".into())), 0.0);
        assert_eq!(parse_sample_rate(Some(" 0.25 ".into())), 0.25);
        // Out-of-range values are clamped, not fatal.
        assert_eq!(parse_sample_rate(Some("7".into())), 1.0);
        assert_eq!(parse_sample_rate(Some("-1".into())), 0.0);
        assert_eq!(parse_sample_rate(Some("NaN".into())), 0.0);
    }

    /// End-to-end through the real middleware and a fake transport: this is the
    /// test that actually backs the privacy claim, because it asserts on the
    /// event as it would go out on the wire — not on the scrubber in isolation.
    ///
    /// It also pins the noise policy: a 500 is an incident and gets reported, a
    /// 4xx is ordinary API traffic (validation, auth, a missing row) and must
    /// not be. `AppError`'s SQLSTATE mapping means the *same* variant can be
    /// either, so the split has to live at the status code, which is exactly
    /// where sentry-actix puts it.
    #[actix_web::test]
    async fn middleware_reports_5xx_only_and_strips_the_request_body() {
        use crate::errors::AppError;
        use actix_web::{App, HttpResponse, test, web};
        use std::sync::Arc;

        let transport = sentry::test::TestTransport::new();
        let options = sentry::ClientOptions::new()
            .dsn("https://public@example.invalid/1")
            .transport(transport.clone())
            .send_default_pii(false)
            // Force the SDK to collect the body it would normally skip, so the
            // assertion below proves OUR hook removes it rather than proving a
            // default happened to be off.
            .max_request_body_size(sentry::MaxRequestBodySize::Always)
            .before_send(scrub_event);
        // A hub of our own: the middleware otherwise reports into the global
        // main hub, which a unit test must not touch.
        let hub = Arc::new(sentry::Hub::new(
            Some(Arc::new(options.into())),
            Default::default(),
        ));

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
        assert_eq!(
            request.url.as_ref().and_then(|u| u.query()),
            None,
            "the query string carries ?customer_phone="
        );
        // Belt and braces over the whole serialized event: no fragment of the
        // payload may appear anywhere in it.
        let wire = serde_json::to_string(&events[0]).unwrap();
        for leak in ["+201000000000", "12 Main St", "secret-token"] {
            assert!(!wire.contains(leak), "event leaked {leak}: {wire}");
        }
    }
}
