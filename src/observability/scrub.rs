//! Redaction of personal data from Sentry events.
//!
//! # This is a COMPLIANCE CONTROL, not defence in depth. Do not remove it.
//!
//! The published Madar privacy policy tells merchants, their staff, and their
//! customers that error reports exclude personal data. That statement has to be
//! true **on the wire**, so everything an event can carry passes through
//! [`scrub_event`] before it leaves the process.
//!
//! # The personal data this system actually handles
//!
//! The denylist below is written from this schema, not from a generic list:
//!
//!   * **Customers** — `orders.customer_name`, `delivery_orders.customer_phone`,
//!     `address_line`, `place_name`, `floor`, `unit_number`, `landmark`,
//!     `delivery_notes`, and `customer_lat` / `customer_lng` (a home address as
//!     a coordinate pair).
//!   * **Staff** — `users.name`, `users.email`, `users.phone`,
//!     `staff_profiles.national_id`, `base_salary_piastres`,
//!     `emergency_contact_name` / `_phone`, and the attendance geofence fixes
//!     `check_in_latitude` / `check_in_longitude`.
//!   * **Credentials** — `password_hash`, `pin_hash`, `offline_pin_hash`,
//!     `organizations.lan_secret`, `integration_credentials`, delivery OTP
//!     codes, WhatsApp device tokens, and every bearer JWT.
//!
//! All of those routinely appear in query parameters, JSON bodies, the `extra`
//! map, breadcrumb data, and — the hard case — interpolated into free-text
//! error messages.
//!
//! # Three matching rules, in order
//!
//! 1. [`PII_KEY_ALLOWLIST`] — checked FIRST. Keys where the value is a
//!    machine's word for itself (`os.name`, `sdk.name`). Without it the `name`
//!    fragment in rule 2 redacts the SDK's own metadata and events can no
//!    longer say which platform or job they came from.
//! 2. [`PII_KEY_DENYLIST`] — case-insensitive **substring**. Long enough not to
//!    collide: `latitude`, never `lat`, which would also hit `translation`.
//! 3. [`PII_KEY_EXACT`] — case-insensitive **equality**, for short forms a
//!    substring rule cannot express. A login query string uses `pass=`, not
//!    `password=`; an owner column is `owner`, not `owner_name`. These MUST
//!    stay exact: as substrings, `pass` would eat `bypass` and `lat` would eat
//!    `translate`.
//!
//! # Keep these lists identical across all three surfaces
//!
//! The same three lists exist in the web dashboard (`src/lib/sentry-scrub.ts`)
//! and the Flutter app (`lib/app/observability.dart`). They will drift unless
//! something fails when they do — see `scripts/check-scrub-parity.sh`, which is
//! wired into `preflight.sh` and CI.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use sentry::protocol::{Event, Map, Value};

/// What a denied value is replaced with. A visible marker rather than a dropped
/// key, so an engineer reading the issue knows the field existed and was
/// scrubbed instead of hunting for a payload that looks truncated.
pub const REDACTED: &str = "[redacted]";

/// Case-insensitive **substring** matches. See the module docs for the data
/// each entry protects.
pub const PII_KEY_DENYLIST: &[&str] = &[
    // ── Contact details ──────────────────────────────────────────────────
    "phone",
    "mobile",
    "msisdn",
    "whatsapp",
    "email",
    // Deliberately broad: `customer_name`, `user_name`, `emergency_contact_name`
    // and `place_name` are all personal data here. The allowlist above is what
    // keeps this from eating `os.name`.
    "name",
    "customer",
    "recipient",
    // ── Addresses and geography ─────────────────────────────────────────
    "address",
    "street",
    "building",
    "apartment",
    "landmark",
    "postcode",
    "zipcode",
    "latitude",
    "longitude",
    "coordinate",
    "geolocation",
    // ── Government identity and pay ─────────────────────────────────────
    "national_id",
    "nationalid",
    "passport",
    "salary",
    "wage",
    "payslip",
    "payroll",
    // ── Credentials: never useful in an issue, always harmful in one ────
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "cookie",
    "jwt",
    "bearer",
    "api_key",
    "apikey",
    "authorization",
    "signature",
    "private_key",
    "privatekey",
    "pin_hash",
    "pinhash",
    "otp_code",
    "sessionid",
    "session_token",
    // ── Payment instruments ─────────────────────────────────────────────
    "iban",
    "card_number",
    "cardnumber",
];

/// Case-insensitive **equality** matches — short forms a substring rule cannot
/// safely express.
///
/// These exist because real payloads use them: a sign-in query string carries
/// `pass=`, a teller login carries `pin=`, a delivery verification carries
/// `otp=`, and a geofence fix carries `lat=` / `lng=`. Every one of them is
/// missed by the longer spellings above.
///
/// They must stay EXACT. As substrings `pass` eats `bypass`, `lat` eats
/// `translate` and `latency`, `key` eats `keyboard`, and `user` eats
/// `user_agent` — each of which quietly destroys the debugging value of an
/// event while protecting nothing.
pub const PII_KEY_EXACT: &[&str] = &[
    "pass", "pin", "otp", "lat", "lng", "lon", "ssn", "nid", "dob", "tel", "addr", "key", "auth",
    "user", "owner", "uid", "cvv", "cvc", "gps", "pwd",
];

/// Checked **before** the denylist. Keys whose value is a machine describing
/// itself, not a person.
///
/// Without this, the `name` fragment redacts `os.name`, `sdk.name` and
/// `runtime.name` — and an event that cannot say which platform or SDK produced
/// it is very close to useless. Entries are matched as `parent.key`, so only
/// `name` *in those contexts* survives.
///
/// `device.name` is deliberately absent: that is a person's own label for their
/// device ("Ali's iPad"), which is exactly the personal data this file exists to
/// stop.
pub const PII_KEY_ALLOWLIST: &[&str] = &[
    "os.name",
    "runtime.name",
    "browser.name",
    "sdk.name",
    "job.name",
    "app.name",
    "package.name",
    "integration.name",
    "transaction.name",
    "span.name",
    "event.name",
    "device.family",
    "device.model",
];

/// True when a key must be redacted, ignoring any parent context.
pub fn is_pii_key(key: &str) -> bool {
    is_pii_path(None, key)
}

/// True when a key must be redacted, given the key of the object containing it.
///
/// The parent is what makes the allowlist work: `name` on its own is personal
/// data here, but `name` inside `os` is "Linux".
pub fn is_pii_path(parent: Option<&str>, key: &str) -> bool {
    let key = key.to_ascii_lowercase();

    // 1. Allowlist first, or the `name` fragment eats the SDK's own metadata.
    if let Some(parent) = parent {
        let path = format!("{}.{key}", parent.to_ascii_lowercase());
        if PII_KEY_ALLOWLIST.iter().any(|a| *a == path) {
            return false;
        }
    }
    if PII_KEY_ALLOWLIST
        .iter()
        .any(|a| a.rsplit('.').next() == Some(key.as_str()) && *a == key)
    {
        return false;
    }

    // 2. Exact short forms.
    if PII_KEY_EXACT.iter().any(|e| *e == key) {
        return true;
    }

    // 3. Substrings.
    PII_KEY_DENYLIST.iter().any(|needle| key.contains(needle))
}

/// Redact denied keys anywhere inside an arbitrary JSON value.
///
/// A denied key takes its **whole subtree**, whatever its type. That is
/// deliberate: `customer` is only ever an object, and walking into it field by
/// field would let an unanticipated child through. Dropping it wholesale is the
/// safe reading.
///
/// String values that survive the key check are still passed through
/// [`sanitize_text`], because a value under an innocent key (`note`, `detail`,
/// `error`) can carry a phone number in free text.
pub fn redact_value(value: &mut Value) {
    redact_value_within(None, value);
}

fn redact_value_within(parent: Option<&str>, value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_pii_path(parent, k) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact_value_within(Some(k), v);
                }
            }
        }
        // An array inherits its parent's key: `drops: [{lat, lng}]` must still
        // see `drops` as the parent of each element.
        Value::Array(items) => items
            .iter_mut()
            .for_each(|v| redact_value_within(parent, v)),
        Value::String(s) => {
            let cleaned = sanitize_text(s);
            if cleaned != *s {
                *s = cleaned;
            }
        }
        _ => {}
    }
}

/// Redact denied keys in a flat string map (tags, request headers, env).
pub fn redact_string_map(map: &mut Map<String, String>) {
    for (k, v) in map.iter_mut() {
        if is_pii_key(k) {
            *v = REDACTED.to_string();
        } else {
            let cleaned = sanitize_text(v);
            if cleaned != *v {
                *v = cleaned;
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Free text
// ─────────────────────────────────────────────────────────────────────────────

/// Redact personal data **inside** a free-text string.
///
/// # Why this exists
///
/// The structured scrubber redacts by KEY. It cannot clean a message. An error
/// whose text interpolates an upstream response body, a login URL, or a
/// `Debug`-formatted struct walks straight past every rule above and lands in
/// the exception value — which is the single most-read field on an issue.
///
/// The tempting fix is to withhold messages entirely. That is worse: it leaves
/// events with an operation name and no indication of what went wrong, which
/// makes the issue stream look healthy and be useless. So this redacts *within*
/// the text and keeps the key, so a message still says which field was
/// involved:
///
/// ```text
///   before: no customer for phone=+201000000000 in branch 7
///   after:  no customer for phone=[redacted] in branch 7
/// ```
///
/// # The rules, in order
///
/// 1. `key=value` / `key: value` / `"key": "value"` where the key matches the
///    same [`is_pii_key`] predicate the structured scrubber uses. Running this
///    first means a labelled field is redacted by its NAME — always correct —
///    rather than by looking phone-shaped.
/// 2. `Bearer <token>` and `Basic <credential>` runs.
/// 3. Email addresses, which are personal data by definition.
/// 4. Phone-SHAPED digit runs, confirmed by [`looks_like_phone`]. The shape
///    rule alone would also eat order refs (`MDR-260831-0042`), timestamps,
///    piastre totals and SQLSTATE codes, and a stripped stack trace is a stack
///    trace nobody can act on.
///
/// # The other direction matters as much
///
/// Ordinary diagnostics must come through untouched — a connection error, a
/// parser position, a SQLSTATE, a duration. See the tests: over-redaction
/// produces events that arrive, look fine, and cannot be acted on, which is a
/// real failure mode and not a safe default.
pub fn sanitize_text(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let out = labelled_pattern().replace_all(input, |c: &Captures| {
        // group 1 = everything up to and including the separator, exactly as
        // written (so surrounding quotes survive); group 2 = the bare key name
        // for the predicate; group 3 = the value.
        let prefix = &c[1];
        let key = &c[2];
        let value = &c[3];
        // Idempotent: the hook can run over already-scrubbed text (a breadcrumb
        // re-sent on a later event), and a second pass must not corrupt the
        // marker into `[redacted]]`.
        if value.starts_with(REDACTED) || !is_pii_key(key) {
            return c[0].to_string();
        }
        format!("{prefix}{REDACTED}")
    });
    let out =
        auth_scheme_pattern().replace_all(&out, |c: &Captures| format!("{} {REDACTED}", &c[1]));
    let out = email_pattern().replace_all(&out, REDACTED);
    let out = phone_pattern().replace_all(&out, |c: &Captures| {
        let m = &c[0];
        if looks_like_phone(m) {
            REDACTED.to_string()
        } else {
            m.to_string()
        }
    });
    out.into_owned()
}

/// `key = value`, `key: value`, `"key": "value"` — the shapes a `Debug` struct,
/// a JSON fragment and a URL query all collapse into once they are one string.
///
/// The value runs to the next delimiter, or is taken whole when quoted. `/` and
/// `?` are delimiters specifically so a URL does not swallow itself: without
/// them the leading `https:` matches as a key and consumes the entire URL as
/// one value, and the `?phone=` inside it is never seen at all.
fn labelled_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)("?([A-Za-z_][A-Za-z0-9_\-]*)"?\s*[:=]\s*)("[^"]*"|'[^']*'|\[redacted\]|[^,;&/?\s}\)\]"]+)"#,
        )
        .expect("labelled_pattern is a valid regex")
    })
}

/// `Authorization: Bearer …` survives rule 1 only when the header name is
/// present; a bare `Bearer eyJ…` in a message does not, so it gets its own rule.
fn auth_scheme_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(bearer|basic|token)\s+[A-Za-z0-9\-._~+/=]{8,}")
            .expect("auth_scheme_pattern is a valid regex")
    })
}

fn email_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}")
            .expect("email_pattern is a valid regex")
    })
}

/// Loose phone-shaped runs, confirmed by [`looks_like_phone`].
fn phone_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:\+|00)?\d[\d ()\-]{6,18}\d").expect("phone_pattern is a valid regex")
    })
}

/// True when a digit run plausibly dials somewhere, rather than being an id, a
/// timestamp or a money amount. Tuned for Egypt (`01XXXXXXXXX`,
/// `+201XXXXXXXXX`) while still catching any internationally-prefixed run.
fn looks_like_phone(raw: &str) -> bool {
    let prefixed = raw.starts_with('+') || raw.starts_with("00");
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 9 || digits.len() > 15 {
        return false;
    }
    if prefixed {
        return true;
    }
    digits.starts_with('0') || digits.starts_with("20")
}

// ─────────────────────────────────────────────────────────────────────────────
// The event hook
// ─────────────────────────────────────────────────────────────────────────────

/// The `before_send` hook: strip everything that could carry personal data,
/// then let the event through.
///
/// Returning `Some` unconditionally is deliberate. Deciding *whether* an event
/// is worth reporting belongs at the capture site; this hook's single
/// responsibility is redaction, so a future capture path cannot accidentally
/// bypass the scrubber by being added somewhere that forgot to filter.
pub fn scrub_event(mut event: Event<'static>) -> Option<Event<'static>> {
    if let Some(request) = event.request.as_mut() {
        // The body is the biggest single exposure: every POST /orders carries a
        // customer name, phone and address. There is no safe subset.
        request.data = None;
        // Cookies and the raw query are unstructured — a session cookie is a
        // live credential and cannot be redacted per key reliably.
        request.cookies = None;
        request.query_string = None;
        // The URL is the most useful field on an issue, so it is kept — minus
        // its query, which is where `?phone=` and `?lat=` ride.
        if let Some(url) = request.url.as_mut() {
            url.set_query(None);
        }
        redact_string_map(&mut request.headers);
        redact_string_map(&mut request.env);
    }

    // Identity is dropped OUTRIGHT rather than reduced. Trimming individual
    // fields leaves the privacy claim depending on the SDK's definition of
    // "default", which a future release is free to widen; removing the whole
    // context does not. Correlation is preserved by the `org`/`branch` tags the
    // report helpers set, which are not personal data.
    event.user = None;
    // The hostname of a POS terminal is routinely set by the branch to a staff
    // member's name.
    event.server_name = None;

    redact_string_map(&mut event.tags);

    for (k, v) in event.extra.iter_mut() {
        if is_pii_key(k) {
            *v = Value::String(REDACTED.to_string());
        } else {
            redact_value_within(Some(k), v);
        }
    }

    for (name, ctx) in event.contexts.iter_mut() {
        // Contexts are typed; only `Other` carries arbitrary keys, and it is the
        // one an integration can put anything into.
        if let sentry::protocol::Context::Other(map) = ctx {
            let parent = name.clone();
            for (k, v) in map.iter_mut() {
                if is_pii_path(Some(&parent), k) {
                    *v = Value::String(REDACTED.to_string());
                } else {
                    redact_value_within(Some(k), v);
                }
            }
        }
    }

    for breadcrumb in event.breadcrumbs.values.iter_mut() {
        // A breadcrumb's message is free text from a log line — the same hole
        // the exception value has.
        if let Some(message) = breadcrumb.message.as_mut() {
            *message = sanitize_text(message);
        }
        for (k, v) in breadcrumb.data.iter_mut() {
            if is_pii_key(k) {
                *v = Value::String(REDACTED.to_string());
            } else {
                redact_value_within(Some(k), v);
            }
        }
    }

    // Free text last, and this is the part the key-based rules above cannot
    // reach: the message and every exception value.
    if let Some(message) = event.message.as_mut() {
        *message = sanitize_text(message);
    }
    if let Some(logentry) = event.logentry.as_mut() {
        logentry.message = sanitize_text(&logentry.message);
        for param in logentry.params.iter_mut() {
            redact_value(param);
        }
    }
    for exception in event.exception.values.iter_mut() {
        if let Some(value) = exception.value.as_mut() {
            *value = sanitize_text(value);
        }
    }

    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::protocol::{Breadcrumb, Request, User};

    // ── Key matching ────────────────────────────────────────────────────

    #[test]
    fn denylist_matches_the_real_column_names_in_this_schema() {
        for key in [
            "customer_phone",
            "customer_name",
            "address_line",
            "place_name",
            "landmark",
            "national_id",
            "base_salary_piastres",
            "emergency_contact_phone",
            "check_in_latitude",
            "check_in_longitude",
            "customer_lat",
            "pin_hash",
            "offline_pin_hash",
            "lan_secret",
            "Authorization",
            "X-Auth-Token",
        ] {
            assert!(is_pii_key(key), "{key} must be denied");
        }
    }

    #[test]
    fn short_forms_are_matched_exactly_and_only_exactly() {
        // A login query string uses `pass=`, not `password=`; a teller login
        // uses `pin=`; a geofence fix uses `lat=`.
        for key in [
            "pass", "PIN", "otp", "lat", "lng", "user", "owner", "auth", "key", "uid", "tel",
        ] {
            assert!(is_pii_key(key), "short form {key} must be denied");
        }
        // ...and as substrings each of these would be a disaster.
        for key in [
            "bypass",
            "passed",
            "translate",
            "latency",
            "keyboard",
            "user_agent",
            "pinned",
            "lngth",
            "authority_id",
            "uuid",
            "auth_flow_step",
        ] {
            assert!(!is_pii_key(key), "{key} must NOT be denied by a short form");
        }
    }

    #[test]
    fn the_allowlist_protects_a_machines_word_for_itself() {
        // Without this the `name` fragment redacts the SDK's own metadata and
        // an event can no longer say what produced it.
        assert!(!is_pii_path(Some("os"), "name"));
        assert!(!is_pii_path(Some("sdk"), "name"));
        assert!(!is_pii_path(Some("runtime"), "name"));
        assert!(!is_pii_path(Some("browser"), "name"));
        assert!(!is_pii_path(Some("job"), "name"));
        // But a person's label for their device is exactly what we redact.
        assert!(is_pii_path(Some("device"), "name"));
        // And a bare `name` with no context stays denied.
        assert!(is_pii_key("name"));
        assert!(is_pii_path(Some("customer"), "name"));
    }

    #[test]
    fn ordinary_domain_keys_survive() {
        // Over-redaction is a real failure mode: it produces events that
        // arrive, look fine, and cannot be acted on.
        for key in [
            "order_id",
            "branch_id",
            "org_id",
            "status",
            "line_cost",
            "quantity",
            "total_amount",
            "sqlstate",
            "elapsed_ms",
            "route",
            "method",
            "dataset",
            "measure",
            "preset_id",
            "translation",
            "related_items",
            "shift_id",
            "till_id",
            "attempt",
            "retry_count",
        ] {
            assert!(!is_pii_key(key), "{key} must NOT be denied");
        }
    }

    // ── Structured redaction ────────────────────────────────────────────

    #[test]
    fn a_sensitive_key_takes_its_whole_subtree() {
        let mut v: Value = serde_json::json!({
            "order_id": "abc",
            "customer": { "given": "Ali", "id": 7, "deep": { "anything": "x" } },
        });
        redact_value(&mut v);
        assert_eq!(v["order_id"], "abc");
        // Not walked into field by field — taken whole.
        assert_eq!(v["customer"], REDACTED);
    }

    #[test]
    fn nested_objects_and_arrays_are_both_recursed() {
        let mut v: Value = serde_json::json!({
            "drops": [
                { "latitude": 30.1, "longitude": 31.2, "sequence": 1 },
                { "latitude": 30.3, "longitude": 31.4, "sequence": 2 }
            ],
            "meta": { "inner": { "national_id": "123", "kept": true } }
        });
        redact_value(&mut v);
        assert_eq!(v["drops"][0]["latitude"], REDACTED);
        assert_eq!(v["drops"][1]["longitude"], REDACTED);
        assert_eq!(v["drops"][0]["sequence"], 1);
        assert_eq!(v["meta"]["inner"]["national_id"], REDACTED);
        assert_eq!(v["meta"]["inner"]["kept"], true);
    }

    #[test]
    fn a_string_under_an_innocent_key_is_still_sanitized() {
        let mut v: Value = serde_json::json!({ "detail": "called +201000000000 twice" });
        redact_value(&mut v);
        assert_eq!(v["detail"], format!("called {REDACTED} twice"));
    }

    // ── Free text ───────────────────────────────────────────────────────

    #[test]
    fn labelled_values_are_redacted_and_the_key_is_kept() {
        // Keeping the key is the point: the message still says which field was
        // involved, so the event stays actionable.
        assert_eq!(
            sanitize_text("no customer for phone=+201000000000 in branch 7"),
            format!("no customer for phone={REDACTED} in branch 7")
        );
        assert_eq!(
            sanitize_text(r#"{"customer_name":"Ali","order_id":"abc"}"#),
            format!(r#"{{"customer_name":{REDACTED},"order_id":"abc"}}"#)
        );
        assert_eq!(
            sanitize_text("Employee { national_id: 29001011234567, id: 4 }"),
            format!("Employee {{ national_id: {REDACTED}, id: 4 }}")
        );
    }

    #[test]
    fn a_url_query_string_inside_a_message_is_redacted_per_key() {
        let out = sanitize_text(
            "GET https://api.madar-pos.cloud/public/orders?phone=%2B201000000000&branch_id=7 failed",
        );
        assert!(out.contains(&format!("phone={REDACTED}")), "{out}");
        // The non-sensitive parameter survives, so the request is still
        // identifiable.
        assert!(out.contains("branch_id=7"), "{out}");
        assert!(!out.contains("201000000000"), "{out}");
    }

    #[test]
    fn credentials_in_free_text_are_redacted_even_unlabelled() {
        let out =
            sanitize_text("upstream rejected Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.abc.def");
        assert_eq!(out, format!("upstream rejected Bearer {REDACTED}"));
        assert!(sanitize_text("Basic YWxpOnNlY3JldA==").contains(REDACTED));
    }

    #[test]
    fn emails_and_phone_shaped_runs_are_redacted() {
        assert_eq!(
            sanitize_text("could not notify ali.hassan@example.com"),
            format!("could not notify {REDACTED}")
        );
        assert!(!sanitize_text("customer +20 100 000 0000 unreachable").contains("100 000 0000"));
        assert!(!sanitize_text("rang 01000000000 three times").contains("01000000000"));
    }

    #[test]
    fn ordinary_diagnostics_come_through_unchanged() {
        // The other direction, and the one that decides whether this is a
        // sanitizer or just an expensive way to send empty messages.
        for message in [
            "error connecting to server: Connection refused (os error 61)",
            "expected `,` or `}` at line 3 column 12",
            "db error: SQLSTATE 23505 duplicate key value violates unique constraint",
            "statement timeout after 8000 ms",
            "pool timed out while waiting for an open connection",
            "order MDR-260831-0042 not found",
            "no such file or directory (os error 2)",
            "invalid UUID: 7f3a1c2e-1111-4222-8333-444455556666",
            "request took 1234 ms and returned 503",
        ] {
            assert_eq!(
                sanitize_text(message),
                message,
                "an ordinary diagnostic was mangled"
            );
        }
    }

    #[test]
    fn an_order_reference_is_not_mistaken_for_a_phone_number() {
        // 260831 0042 is digit-shaped but not dialable; eating it would strip
        // the single most useful identifier in a POS issue.
        let m = "duplicate order_ref MDR-260831-0042 for branch 7";
        assert_eq!(sanitize_text(m), m);
    }

    #[test]
    fn a_money_amount_is_not_mistaken_for_a_phone_number() {
        let m = "total_amount 16300 exceeds limit 1000000";
        assert_eq!(sanitize_text(m), m);
    }

    #[test]
    fn sanitizing_is_idempotent() {
        // The hook can run over already-scrubbed text (a breadcrumb re-sent on
        // a later event); a second pass must not corrupt the marker.
        let once = sanitize_text("phone=+201000000000");
        assert_eq!(sanitize_text(&once), once);
    }

    #[test]
    fn empty_and_huge_inputs_are_handled() {
        assert_eq!(sanitize_text(""), "");
        let big = "x".repeat(100_000);
        assert_eq!(sanitize_text(&big).len(), big.len());
    }

    // ── The event hook end to end ───────────────────────────────────────

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
            message: Some("looking up phone=+201000000000".into()),
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
        event.server_name = Some("alis-ipad".into());

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
        assert!(
            event.breadcrumbs.values[0]
                .message
                .as_deref()
                .unwrap()
                .contains(REDACTED)
        );
        assert_eq!(event.breadcrumbs.values[0].data["national_id"], REDACTED);
        // Identity is dropped outright, not trimmed field by field.
        assert!(event.user.is_none());
        assert!(event.server_name.is_none());
    }

    #[test]
    fn an_exception_value_is_sanitized() {
        // The single most-read field on an issue, and the one the key-based
        // rules can never reach.
        use sentry::protocol::Exception;
        let mut event = Event::default();
        event.exception.values.push(Exception {
            ty: "OrderError".into(),
            value: Some("no customer for +201000000000 at 12 Main St".into()),
            ..Default::default()
        });
        event.message = Some("failed for customer_phone=+201000000000".into());

        let event = scrub_event(event).unwrap();
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("201000000000"), "{wire}");
        // ...but the exception still says what went wrong.
        assert!(
            event.exception.values[0]
                .value
                .as_ref()
                .unwrap()
                .contains("no customer for")
        );
    }

    #[test]
    fn a_context_map_is_scrubbed_but_keeps_its_platform_metadata() {
        use sentry::protocol::Context;
        let mut event = Event::default();
        event.contexts.insert(
            "os".into(),
            Context::Other(
                [
                    ("name".to_string(), serde_json::json!("Linux")),
                    ("version".to_string(), serde_json::json!("6.1")),
                ]
                .into_iter()
                .collect(),
            ),
        );
        event.contexts.insert(
            "device".into(),
            Context::Other(
                [("name".to_string(), serde_json::json!("Ali's iPad"))]
                    .into_iter()
                    .collect(),
            ),
        );
        let event = scrub_event(event).unwrap();
        let os = match &event.contexts["os"] {
            Context::Other(m) => m.clone(),
            _ => panic!("expected an Other context"),
        };
        assert_eq!(os["name"], "Linux", "the allowlist must protect os.name");
        let device = match &event.contexts["device"] {
            Context::Other(m) => m.clone(),
            _ => panic!("expected an Other context"),
        };
        assert_eq!(device["name"], REDACTED);
    }
}
