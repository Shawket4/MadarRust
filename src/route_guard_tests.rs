//! Route-coverage guard: every mounted API route is permission-guarded, or it is
//! on a reviewed allowlist below.
//!
//! # Mechanism, and why
//!
//! actix-web has no public route listing, and the two obvious substitutes are
//! both blind in the direction that matters:
//! - `openapi.json` lists what is DOCUMENTED, not what is MOUNTED; an
//!   undocumented route is exactly the kind that slips through review.
//! - grepping `routes.rs` misses nested scopes, `configure` closures and routes
//!   mounted elsewhere.
//!
//! So the guard asks the running app. It mounts [`crate::app_routes::configure_api`]
//! (the exact function `main.rs` mounts, plus the demo routes and static files that
//! `main.rs` adds conditionally) and reads the app's own `ResourceMap` through
//! its `Debug` output — the router tree actix built, so nothing registered can be
//! missing from it. The map has paths but no methods, so each path is then
//! probed with every method; the app's default service answers 418, which is how
//! "no route for this method" is told apart from a handler's own 404/405.
//!
//! Each discovered (method, path) is then called:
//! 1. with no token: must be 401 (or 403), unless the route is [`PUBLIC`];
//! 2. with a valid token for a REAL, active user of a fresh org who holds NO
//!    capability (no role assignment, no override, not an owner), once with an
//!    `org_admin` token role and once with a `teller` one (so a handler that
//!    trusts the token's role name instead of a capability is caught): must be
//!    403, unless the route is [`PUBLIC`] or [`AUTHENTICATED`].
//!
//! JwtMiddleware is scope-level (it runs AFTER routing), so a 404/400/422/2xx in
//! either call means the request reached the handler without a permission
//! decision: a failure. Path params get fixtures (a uuid, an integer or a word,
//! from the OpenAPI parameter schema when documented, else from the name) and a
//! JSON body is synthesized from the documented request schema, so an extractor
//! 400 is not mistaken for a gate. A handler that validates before it checks
//! permission therefore fails here on purpose: move the check first.
//!
//! Cross-org leakage (a user with capabilities in ANOTHER org) is not probed:
//! it needs real per-route entities to be meaningful, which this guard does not
//! seed. The per-module tests own that.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use actix_web::{App, HttpRequest, HttpResponse, http::Method, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use utoipa::OpenApi;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

/// Routes anyone may call without a token. Every entry says why.
/// Paths are the mounted pattern; `*` as the method means every method.
pub const PUBLIC: &[(&str, &str, &str)] = &[
    // ── Platform and sign-in ──
    (
        "GET",
        "/health",
        "load balancer / uptime probe; says nothing",
    ),
    (
        "POST",
        "/auth/login",
        "the sign-in itself (email+password or PIN); rate limited",
    ),
    (
        "POST",
        "/auth/activate-device",
        "a POS redeems a one-time activation code before it has any token; rate limited",
    ),
    (
        "GET",
        "/auth/authz-keys",
        "public verification keys for signed permission snapshots; public by design",
    ),
    (
        "POST",
        "/auth/resolve-branch",
        "pre-login geofence: nearest branch of an org within its radius; rate limited (see owner note)",
    ),
    (
        "POST",
        "/demo/session",
        "public demo playground; 404 unless DEMO_MODE, which runs on a separate backend",
    ),
    // ── Own credential instead of a staff JWT ──
    (
        "GET",
        "/devices/me/authz-snapshot",
        "device-authenticated (the device's own credential), 401 without it",
    ),
    (
        "GET",
        "/integrations/analytics/orders",
        "integration API key (Basic auth, IntegrationCaller), 401 without it",
    ),
    (
        "GET",
        "/assets/{scope}/{file}",
        "org assets: a valid signed URL (exp+sig) or a JWT of the owning org; 404 otherwise",
    ),
    ("HEAD", "/assets/{scope}/{file}", "same as GET"),
    (
        "GET",
        "/uploads/{tail:.*}",
        "legacy upload URLs for old clients: signed URL or JWT, same check as /assets",
    ),
    ("HEAD", "/uploads/{tail:.*}", "same as GET"),
    (
        "GET",
        "/wallet/v1/devices/{device}/registrations/{pass_type}",
        "Apple Wallet web service (Apple's paths); lists serials only",
    ),
    (
        "POST",
        "/wallet/v1/devices/{device}/registrations/{pass_type}/{serial}",
        "Apple Wallet: authenticated by the pass's ApplePass token",
    ),
    (
        "DELETE",
        "/wallet/v1/devices/{device}/registrations/{pass_type}/{serial}",
        "Apple Wallet: authenticated by the pass's ApplePass token",
    ),
    (
        "GET",
        "/wallet/v1/passes/{pass_type}/{serial}",
        "Apple Wallet: authenticated by the pass's ApplePass token",
    ),
    (
        "POST",
        "/wallet/v1/log",
        "Apple Wallet error log sink; writes to the server log only",
    ),
    // ── Guest ordering (QR table + delivery), rate limited ──
    (
        "GET",
        "/public/branches",
        "guest ordering: the org's branch picker",
    ),
    (
        "GET",
        "/public/branches/{id}/menu",
        "guest ordering: the public menu",
    ),
    (
        "GET",
        "/public/branches/{id}/delivery-quote",
        "guest ordering: delivery fee for a location",
    ),
    (
        "GET",
        "/public/tables/{id}",
        "QR table landing: which branch/table a code is",
    ),
    ("GET", "/public/tables/{id}/menu", "QR table menu"),
    (
        "POST",
        "/public/table-orders",
        "a guest's table order; lands as a pending order for staff",
    ),
    (
        "POST",
        "/public/delivery-orders",
        "a guest's delivery order; OTP-verified where the branch requires it",
    ),
    (
        "GET",
        "/public/delivery-orders/{id}/track",
        "guest tracking by unguessable order id",
    ),
    (
        "GET",
        "/public/delivery-orders/history",
        "guest's own history by phone (+ device token when OTP is on); see owner note",
    ),
    (
        "GET",
        "/public/delivery-orders/past-locations",
        "guest's saved addresses by phone (+ device token when OTP is on); see owner note",
    ),
    (
        "POST",
        "/public/otp/request",
        "guest phone verification (WhatsApp OTP); rate limited",
    ),
    (
        "POST",
        "/public/otp/verify",
        "guest phone verification (WhatsApp OTP); rate limited",
    ),
    (
        "GET",
        "/public/orgs/brand",
        "public branding (name, colours, logo) for the ordering pages",
    ),
    (
        "GET",
        "/public/orgs/favicon",
        "public favicon for the ordering pages",
    ),
    // ── Loyalty card (the member's own unguessable token) ──
    (
        "GET",
        "/public/loyalty/join-info",
        "the join page: programme name and terms",
    ),
    (
        "POST",
        "/public/loyalty/join",
        "a customer joins the programme",
    ),
    (
        "GET",
        "/public/loyalty/card/{token}",
        "the member's card, by its secret token",
    ),
    (
        "GET",
        "/public/loyalty/card/{token}/orders",
        "the member's own orders, by the card's secret token",
    ),
    (
        "GET",
        "/public/loyalty/card/{token}/qr.png",
        "the member's card QR, by its secret token",
    ),
    (
        "POST",
        "/public/loyalty/card/{token}/preferences",
        "the member's own preferences, by the card's secret token",
    ),
    (
        "GET",
        "/public/loyalty/pass/{token}/apple.pkpass",
        "the member's Wallet pass, by the card's secret token",
    ),
    (
        "GET",
        "/public/loyalty/brand/logo.png",
        "default brand image for passes",
    ),
    (
        "GET",
        "/public/loyalty/brand/{org_id}/logo/{v}.png",
        "org logo for Wallet passes (public images)",
    ),
    (
        "GET",
        "/public/loyalty/brand/{org_id}/banner/{v}.png",
        "org banner for Wallet passes (public images)",
    ),
    // ── Guest bookings (DEPRECATED flow, still mounted) ──
    (
        "GET",
        "/public/booking-branches",
        "guest booking: branches that take bookings",
    ),
    (
        "GET",
        "/public/branches/{id}/booking-info",
        "guest booking: a branch's booking rules",
    ),
    (
        "GET",
        "/public/branches/{id}/booking-slots",
        "guest booking: free slots",
    ),
    ("POST", "/public/bookings", "guest booking: create"),
    (
        "GET",
        "/public/bookings/{token}",
        "guest booking: manage by its secret token",
    ),
    (
        "PATCH",
        "/public/bookings/{token}",
        "guest booking: change by its secret token",
    ),
    (
        "POST",
        "/public/bookings/{token}/cancel",
        "guest booking: cancel by its secret token",
    ),
];

/// Routes any signed-in person may call without holding a capability.
pub const AUTHENTICATED: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/auth/me",
        "the caller's own profile and tax context",
    ),
    (
        "GET",
        "/auth/permissions",
        "the caller's own legacy permission grid (old tablets)",
    ),
    (
        "GET",
        "/authz/me",
        "the caller's own effective capabilities (the UI hides what they lack)",
    ),
    (
        "GET",
        "/authz/policy",
        "which capabilities ask a manager in this org; every till needs it to know when to ask",
    ),
    ("GET", "/timezones", "the static list of IANA zone names"),
    (
        "POST",
        "/tills",
        "tombstone: 410 TILL_ENTITY_REMOVED for everyone",
    ),
    (
        "PATCH",
        "/tills/{till_id}",
        "tombstone: 410 TILL_ENTITY_REMOVED for everyone",
    ),
    (
        "DELETE",
        "/tills/{till_id}",
        "tombstone: 410 TILL_ENTITY_REMOVED for everyone",
    ),
];

/// The static file services' own mount points (exact patterns, not prefixes:
/// API routes under `/uploads/...` are still checked).
const STATIC_PREFIXES: &[&str] = &["/uploads", crate::recipes::steps::STATIC_URL_PREFIX];

const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Pseudo-status: the request was answered by a different pattern.
const COLLISION: u16 = 1;

const RMAP_ROUTE: &str = "/__route_guard_rmap";

fn secret() -> JwtSecret {
    JwtSecret("route-guard".to_string())
}

/// Parse the pretty `Debug` of a `ResourceMap` into full leaf patterns.
///
/// Layout (actix-web 4): `ResourceMap { pattern: ResourceDef { .., patterns:
/// Single("/x") | List([..]), .. }, named: {..}, parent: .., nodes: None |
/// Some([ResourceMap {..}, ..]) }`. `named` holds clones of named children and
/// is skipped; a node with `nodes: None` is a routable leaf.
fn leaf_patterns(dump: &str) -> Vec<String> {
    let lines: Vec<&str> = dump.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    // (indent of the `ResourceMap {` line, its own pattern)
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let t = l.trim();
        let ind = indent(l);
        if t.starts_with("named: {") && !t.ends_with("},") {
            // Skip to the matching close at the same indent.
            i += 1;
            while i < lines.len() && !(indent(lines[i]) == ind && lines[i].trim().starts_with('}'))
            {
                i += 1;
            }
            i += 1;
            continue;
        }
        if t == "ResourceMap {" || t.ends_with(": ResourceMap {") {
            while stack.last().is_some_and(|(d, _)| *d >= ind) {
                stack.pop();
            }
            stack.push((ind, String::new()));
        } else if t.starts_with("patterns: Single(") {
            // Either inline `Single("..")` or the string on the next line.
            let s = if let Some(rest) = t.strip_prefix("patterns: Single(\"") {
                rest.trim_end_matches("),")
                    .trim_end_matches('"')
                    .to_string()
            } else {
                i += 1;
                lines[i]
                    .trim()
                    .trim_end_matches(',')
                    .trim_matches('"')
                    .to_string()
            };
            // The ResourceDef belongs to the innermost ResourceMap whose field it is.
            if let Some(top) = stack.last_mut()
                && top.1.is_empty()
            {
                top.1 = s;
            }
        } else if t.starts_with("patterns: List(") {
            panic!("route guard: a multi-pattern resource needs handling: {t}");
        } else if t == "nodes: None,"
            && let Some((d, _)) = stack.last()
            && ind == d + 4
        {
            out.push(stack.iter().map(|(_, p)| p.as_str()).collect::<String>());
        }
        i += 1;
    }
    out
}

/// `{id}` / `{tail:.*}` → `{}` so mounted patterns and OpenAPI paths compare.
fn normalize(p: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in p.chars() {
        match c {
            '{' => {
                if depth == 0 {
                    out.push_str("{}");
                }
                depth += 1;
            }
            '}' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn param_names(p: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = p;
    while let Some(s) = rest.find('{') {
        let e = rest[s..].find('}').unwrap() + s;
        names.push(rest[s + 1..e].split(':').next().unwrap().to_string());
        rest = &rest[e + 1..];
    }
    names
}

struct Spec {
    doc: Value,
    /// The guard user's own org and branch: a param or field named `org_id` /
    /// `branch_id` gets these, so a handler that must look the branch up first
    /// meets a real branch the caller simply holds nothing at.
    org: Uuid,
    branch: Uuid,
}

impl Spec {
    fn load(org: Uuid, branch: Uuid) -> Self {
        Spec {
            org,
            branch,
            doc: serde_json::from_str(&crate::openapi::ApiDoc::openapi().to_json().unwrap())
                .unwrap(),
        }
    }

    fn op(&self, method: &str, pattern: &str) -> Option<&Value> {
        let n = normalize(pattern);
        self.doc["paths"]
            .as_object()?
            .iter()
            .find(|(p, _)| normalize(p) == n)
            .and_then(|(_, ops)| ops.get(method.to_lowercase()))
    }

    fn resolve<'a>(&'a self, s: &'a Value) -> &'a Value {
        match s.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                self.resolve(&self.doc["components"]["schemas"][r.rsplit('/').next().unwrap()])
            }
            None => s,
        }
    }

    /// A minimal value that satisfies `schema` structurally.
    fn sample(&self, schema: &Value, depth: usize) -> Value {
        let s = self.resolve(schema);
        if depth > 6 {
            return Value::Null;
        }
        for k in ["oneOf", "anyOf", "allOf"] {
            if let Some(v) = s.get(k).and_then(Value::as_array) {
                if k == "allOf" {
                    let mut m = serde_json::Map::new();
                    for part in v {
                        if let Value::Object(o) = self.sample(part, depth + 1) {
                            m.extend(o);
                        }
                    }
                    return Value::Object(m);
                }
                let pick = v
                    .iter()
                    .find(|x| self.resolve(x).get("type") != Some(&json!("null")))
                    .unwrap_or(&v[0]);
                return self.sample(pick, depth + 1);
            }
        }
        if let Some(e) = s.get("enum").and_then(Value::as_array) {
            return e[0].clone();
        }
        let ty = match s.get("type") {
            Some(Value::Array(a)) => a
                .iter()
                .find(|t| *t != "null")
                .and_then(Value::as_str)
                .unwrap_or("null"),
            Some(Value::String(t)) => t.as_str(),
            _ => "object",
        };
        match ty {
            "string" => match s.get("format").and_then(Value::as_str) {
                Some("uuid") => json!(Uuid::new_v4()),
                Some("date-time") => json!("2026-01-01T00:00:00Z"),
                Some("date") => json!("2026-01-01"),
                Some("time") | Some("partial-time") => json!("09:00:00"),
                _ => json!("x"),
            },
            "integer" => json!(1),
            "number" => json!(1),
            "boolean" => json!(false),
            "array" => json!([]),
            "null" => Value::Null,
            _ => {
                let mut m = serde_json::Map::new();
                let required: BTreeSet<&str> = s
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if let Some(props) = s.get("properties").and_then(Value::as_object) {
                    for (k, v) in props {
                        if required.contains(k.as_str()) {
                            m.insert(k.clone(), self.named(k, v, depth + 1));
                        }
                    }
                }
                Value::Object(m)
            }
        }
    }

    fn named(&self, name: &str, schema: &Value, depth: usize) -> Value {
        let plain_string = self.resolve(schema).get("type") == Some(&json!("string"))
            && self.resolve(schema).get("format").is_none();
        match name {
            "branch_id" => json!(self.branch),
            "org_id" => json!(self.org),
            // Undocumented formats, told by name.
            n if plain_string && (n == "date" || n.ends_with("_date")) => json!("2026-01-01"),
            n if plain_string && (n == "time" || n.ends_with("_time")) => json!("09:00:00"),
            _ => self.sample(schema, depth),
        }
    }

    fn body(&self, op: Option<&Value>) -> Option<Value> {
        let schema = op?.get("requestBody")?["content"]
            .get("application/json")?
            .get("schema")?;
        Some(self.sample(schema, 0))
    }

    fn query(&self, op: Option<&Value>) -> String {
        let mut parts = Vec::new();
        for p in op
            .and_then(|o| o.get("parameters"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if p["in"] == "query" && p["required"] == true {
                let v = self.named(p["name"].as_str().unwrap(), &p["schema"], 0);
                let v = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                parts.push(format!("{}={}", p["name"].as_str().unwrap(), v));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }

    fn path_value(&self, op: Option<&Value>, name: &str) -> String {
        match name {
            "branch_id" => return self.branch.to_string(),
            "org_id" => return self.org.to_string(),
            _ => {}
        }
        let documented = op
            .and_then(|o| o.get("parameters"))
            .and_then(Value::as_array)
            .and_then(|ps| ps.iter().find(|p| p["in"] == "path" && p["name"] == name))
            .map(|p| self.sample(&p["schema"], 0));
        match documented {
            Some(Value::String(s)) => s,
            Some(Value::Null) | None => fallback_path_value(name),
            Some(v) => v.to_string(),
        }
    }
}

/// Path fixtures for undocumented params, keyed by name.
fn fallback_path_value(name: &str) -> String {
    const WORDS: &[&str] = &[
        "slug",
        "token",
        "code",
        "key",
        "name",
        "kind",
        "tail",
        "filename",
        "path",
        "serial",
        "pass_type",
        "lang",
        "locale",
        "platform",
        "_",
    ];
    const INTS: &[&str] = &["version", "year", "month", "day", "n", "page", "number"];
    if WORDS.iter().any(|w| name.contains(w)) && !name.ends_with("id") {
        "x".into()
    } else if INTS.contains(&name) {
        "1".into()
    } else {
        Uuid::new_v4().to_string()
    }
}

/// Bodies for routes whose JSON body is not documented in OpenAPI.
fn body_fixture(method: &str, pattern: &str, fx: &Fixture, spec: &Spec) -> Option<Value> {
    match (method, pattern) {
        // The op's author is the caller, who holds nothing (not even pos.sign_in).
        ("POST", "/sync/replay") => Some(json!({
            "op": "open_till",
            "teller_id": fx.user,
            "branch_id": fx.branch,
            "request": spec.sample(&json!({"$ref": "#/components/schemas/OpenTillRequest"}), 0),
        })),
        _ => None,
    }
}

fn allowed(list: &[(&str, &str, &str)], method: &str, path: &str) -> bool {
    list.iter()
        .any(|(m, p, _)| (*m == "*" || *m == method) && *p == path)
}

struct Fixture {
    org: Uuid,
    /// The teller-token person (holds nothing).
    user: Uuid,
    branch: Uuid,
    org_admin_token: String,
    teller_token: String,
}

/// A real, active person of a fresh org with no capability anywhere.
async fn zero_cap_user(pool: &PgPool) -> Fixture {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Guard', $2)")
        .bind(org)
        .bind(format!("guard-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Guard branch')")
        .bind(branch)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let mut tokens = Vec::new();
    let mut user = Uuid::nil();
    for role in ["org_admin", "teller"] {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, org_id, name, role, email, password_hash, is_owner)
             VALUES ($1, $2, 'Nobody', $3::user_role, $4, 'h', false)",
        )
        .bind(id)
        .bind(org)
        .bind(role)
        .bind(format!("{id}@guard.test"))
        .execute(pool)
        .await
        .unwrap();
        // Whatever a trigger granted (an org_admin row is made an owner), take it away.
        sqlx::query("UPDATE users SET is_owner = false WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM user_overrides WHERE user_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM role_assignments WHERE user_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM permissions WHERE user_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        let eff = crate::authz::require::effective(pool, id, Some(branch))
            .await
            .unwrap();
        assert!(
            eff.caps == crate::authz::CapSet::default() && !eff.owner,
            "the guard's user must hold nothing: {eff:?}"
        );
        user = id;
        let r = if role == "teller" {
            UserRole::Teller
        } else {
            UserRole::OrgAdmin
        };
        tokens.push(create_token(&secret(), id, Some(org), r, Some(branch), 1).unwrap());
    }
    Fixture {
        org,
        user,
        branch,
        teller_token: tokens.pop().unwrap(),
        org_admin_token: tokens.pop().unwrap(),
    }
}

#[sqlx::test]
async fn every_route_is_guarded_or_allowlisted(pool: PgPool) {
    // SAFETY: nextest runs each test in its own process; nothing else reads these.
    unsafe {
        std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
        std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
    }
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    crate::authz::sync_catalogue(&pool).await.unwrap();
    let fx = zero_cap_user(&pool).await;
    let uploads = std::env::temp_dir().join(format!("route-guard-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&uploads).unwrap();

    let read_pool = web::Data::new(pool.clone());
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(crate::menu::cache::MenuCache::from_env()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(
                crate::auth::org_status::OrgStatusCache::new(),
            ))
            .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
            .app_data(crate::qr_card::routes::make_provider())
            .app_data(web::Data::new(crate::demo::config::DemoConfig::from_env()))
            .app_data(web::Data::new(crate::ai::AiState::from_env()))
            .app_data(web::PathConfig::default().error_handler(|err, _req| {
                crate::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .app_data(web::QueryConfig::default().error_handler(|err, _req| {
                crate::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .app_data(web::JsonConfig::default().error_handler(|err, _req| {
                crate::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .configure(|cfg| crate::app_routes::configure_api(cfg, read_pool.clone()))
            .configure(crate::demo::routes::configure)
            .route(
                RMAP_ROUTE,
                web::get().to(|req: HttpRequest| async move {
                    HttpResponse::Ok().body(format!("{:#?}", req.resource_map()))
                }),
            )
            .service(actix_files::Files::new("/uploads", &uploads))
            .service(
                web::scope(crate::recipes::steps::STATIC_URL_PREFIX)
                    .service(actix_files::Files::new("", &uploads)),
            )
            .default_service(web::to(|| async { HttpResponse::ImATeapot().finish() })),
    )
    .await;

    let dump =
        test::call_and_read_body(&app, test::TestRequest::get().uri(RMAP_ROUTE).to_request()).await;
    let dump = String::from_utf8(dump.to_vec()).unwrap();
    let mut patterns: Vec<String> = leaf_patterns(&dump)
        .into_iter()
        .filter(|p| p != RMAP_ROUTE)
        .filter(|p| !STATIC_PREFIXES.contains(&p.as_str()))
        .collect();
    patterns.sort();
    patterns.dedup();
    assert!(
        patterns.len() > 300,
        "the router dump did not parse: {} patterns\n{}",
        patterns.len(),
        &dump[..dump.len().min(3000)]
    );

    let spec = Spec::load(fx.org, fx.branch);

    let send = |method: &str, pattern: &str, token: Option<&str>| {
        let op = spec.op(method, pattern);
        let mut uri = pattern.to_string();
        for name in param_names(pattern) {
            let start = uri.find('{').unwrap();
            let end = uri[start..].find('}').unwrap() + start;
            uri.replace_range(start..=end, &spec.path_value(op, &name));
        }
        uri.push_str(&spec.query(op));
        let mut req = test::TestRequest::default()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(&uri);
        if let Some(t) = token {
            req = req.insert_header(("Authorization", format!("Bearer {t}")));
        }
        if let Some(b) = body_fixture(method, pattern, &fx, &spec).or_else(|| spec.body(op)) {
            req = req.set_json(b);
        }
        (uri, req.to_request())
    };

    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    let mut routes = 0;
    let mut discovered: BTreeSet<(String, String)> = BTreeSet::new();
    for pattern in &patterns {
        for method in METHODS {
            let mut statuses = Vec::new();
            let mut last_body = String::new();
            for token in [
                None,
                Some(fx.org_admin_token.as_str()),
                Some(fx.teller_token.as_str()),
            ] {
                let (uri, req) = send(method, pattern, token);
                let status = match tokio::time::timeout(
                    Duration::from_secs(10),
                    test::call_service(&app, req),
                )
                .await
                {
                    Ok(resp) => {
                        // Which resource answered? Its captured params name it. A
                        // literal path swallowed by a sibling `{param}` route (or the
                        // other way round) belongs to that other pattern, which is
                        // probed on its own.
                        let captured: Vec<String> = resp
                            .request()
                            .match_info()
                            .iter()
                            .map(|(k, _)| k.to_string())
                            .collect();
                        let code = resp.status().as_u16();
                        if code != 401 && captured != param_names(pattern) {
                            COLLISION
                        } else {
                            if let Ok(body) =
                                tokio::time::timeout(Duration::from_secs(2), test::read_body(resp))
                                    .await
                            {
                                last_body =
                                    String::from_utf8_lossy(&body).chars().take(160).collect();
                            }
                            code
                        }
                    }
                    Err(_) => 0, // a stream that opened: the handler ran
                };
                statuses.push((uri, status));
            }
            let [anon, admin, teller] = [statuses[0].1, statuses[1].1, statuses[2].1];
            // No route for this method: every token path fell to the default service.
            let absent = |s: u16| s == 418 || s == 405 || s == COLLISION;
            if absent(admin) && absent(teller) && (absent(anon) || anon == 401) {
                continue;
            }
            routes += 1;
            discovered.insert((method.to_string(), pattern.clone()));
            let key = format!("{method} {pattern}");
            if allowed(PUBLIC, method, pattern) {
                continue;
            }
            let anon_ok = anon == 401 || anon == 403;
            let auth_ok = allowed(AUTHENTICATED, method, pattern)
                || ([admin, teller].iter().all(|s| *s == 403 || *s == 418));
            if !anon_ok || !auth_ok {
                problems.insert(
                    key,
                    format!(
                        "anon={anon} org_admin={admin} teller={teller} ({}) {last_body}",
                        statuses[1].0
                    ),
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&uploads);
    // A reviewed entry for a route that is gone is a hole waiting for the next
    // route to reuse its path: keep the lists exact.
    for (m, p, _) in PUBLIC.iter().chain(AUTHENTICATED) {
        if !discovered.contains(&(m.to_string(), p.to_string())) {
            problems.insert(
                format!("{m} {p}"),
                "allowlisted but not mounted: remove the entry".into(),
            );
        }
    }
    assert!(
        problems.is_empty(),
        "{} of {routes} routes are neither permission-guarded nor allowlisted \
         (see src/route_guard_tests.rs):\n{}",
        problems.len(),
        problems
            .iter()
            .map(|(k, v)| format!("  {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The routes the guard caught used to validate or look up BEFORE deciding
/// permission. Each now refuses a person who holds nothing with 403, while a
/// person who holds everything (the org's owner) still gets the handler's own
/// 400/404 for the same malformed request, so the order is what changed.
#[sqlx::test]
async fn caught_routes_decide_permission_before_validation(pool: PgPool) {
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    crate::authz::sync_catalogue(&pool).await.unwrap();
    let fx = zero_cap_user(&pool).await;
    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash, is_owner)
         VALUES ($1, $2, 'Owner', 'org_admin'::user_role, $3, 'h', true)",
    )
    .bind(owner)
    .bind(fx.org)
    .bind(format!("{owner}@guard.test"))
    .execute(&pool)
    .await
    .unwrap();
    let owner_token =
        create_token(&secret(), owner, Some(fx.org), UserRole::OrgAdmin, None, 1).unwrap();

    let read_pool = web::Data::new(pool.clone());
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
            .app_data(web::JsonConfig::default().error_handler(|err, _req| {
                crate::errors::AppError::BadRequest(err.to_string()).into()
            }))
            .configure(|cfg| crate::app_routes::configure_api(cfg, read_pool.clone())),
    )
    .await;

    let missing = Uuid::new_v4();
    // (method, uri, body, what the owner gets)
    let cases: Vec<(&str, String, Value, u16)> = vec![
        (
            "POST",
            "/authz/roles".into(),
            json!({"kind": "x", "name_en": "a", "name_ar": "b"}),
            400,
        ),
        (
            "PUT",
            format!("/authz/roles/{missing}/grants"),
            json!({"capability": "x", "granted": true}),
            400,
        ),
        (
            "PUT",
            format!("/authz/users/{missing}/overrides"),
            json!({"capability": "x", "effect": "allow"}),
            400,
        ),
        (
            "PUT",
            format!("/authz/users/{missing}/assignments"),
            json!({"assignments": []}),
            404,
        ),
        ("DELETE", "/loyalty/settings".into(), Value::Null, 400),
        (
            "PATCH",
            format!("/staff/requests/{missing}/decision"),
            json!({"status": "x"}),
            400,
        ),
        (
            "POST",
            format!("/devices/activation-codes/{missing}/revoke"),
            Value::Null,
            404,
        ),
        ("GET", format!("/assets/jobs/{missing}"), Value::Null, 404),
        (
            "GET",
            format!("/sync/asset-bundles/{}/x", fx.org),
            Value::Null,
            404,
        ),
        (
            "POST",
            "/sync/assets".into(),
            json!({"branch_id": missing, "hashes": []}),
            404,
        ),
    ];
    for (method, uri, body, owner_status) in cases {
        for (who, token, want) in [
            ("nobody", fx.teller_token.as_str(), 403),
            ("nobody (org_admin token)", fx.org_admin_token.as_str(), 403),
            ("owner", owner_token.as_str(), owner_status),
        ] {
            let mut req = test::TestRequest::default()
                .method(Method::from_bytes(method.as_bytes()).unwrap())
                .uri(&uri)
                .insert_header(("Authorization", format!("Bearer {token}")));
            if !body.is_null() {
                req = req.set_json(&body);
            }
            let resp = test::call_service(&app, req.to_request()).await;
            assert_eq!(resp.status().as_u16(), want, "{who}: {method} {uri}");
        }
    }
}
