//! Client version telemetry (LEGACY_REMOVAL.md, Phase T).
//!
//! Two halves:
//!
//! * [`legacy_hit`] — called at every legacy code path (the `/shifts` adapters,
//!   the `GET /tills` entity, `shift_id` alias fields, the `shifts:*` permission
//!   mirror, `open_shift`/`close_shift` replays, old error wording, analytics
//!   aliases, `/catalog/sync`, legacy `/uploads`, §8a mirror lists read by a
//!   POS/KDS). It emits one structured `tracing` event, target `madar.legacy`,
//!   message `legacy_hit`, field `kind` — so journald and Sentry breadcrumbs show
//!   it — and notes the hit on the current request.
//! * [`record`] — app-level middleware. After the handler ran (so `JwtMiddleware`
//!   has resolved the org), it upserts one `client_seen` row per device (org,
//!   branch (the token's, else the device row's, else `X-Madar-Branch`),
//!   device, client, app version from `X-Madar-Client` only, first/last seen,
//!   last legacy path) in
//!   a background task, at most once a minute per device (and once a minute per
//!   device + legacy kind), so the request path pays one map lookup.
//!
//! The hits reach the middleware through a tokio task-local that the middleware
//! scopes around the handler, so a deep site (the analytics dataset alias, an
//! inner refund refusal) needs no `HttpRequest`. Outside a request (unit tests,
//! background jobs) a hit is only logged.
//!
//! Nothing here ever fails or slows a request: every error is swallowed.

pub mod handlers;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{HttpMessage, web};
use futures::StreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::devices::{CLIENT_HEADER, ClientHeader, DeviceHeader};

/// A device's row is rewritten at most this often (per legacy kind, too).
pub const THROTTLE: Duration = Duration::from_secs(60);

/// Largest request body the alias scan buffers. Bigger bodies are passed
/// through unscanned (no POS body with a `shift_id` comes close).
const MAX_SCAN_BODY: usize = 1024 * 1024;

// ── legacy kinds ─────────────────────────────────────────────────────────────
// One stable id per family of legacy code path; the removal gates in
// LEGACY_REMOVAL.md query `client_seen.legacy_kinds` / `last_legacy_kind` by these.

/// Any `/shifts/*` adapter route (§1.1–1.7).
pub const KIND_SHIFTS_ROUTE: &str = "legacy_shifts_route";
/// `GET /tills` served the synthesized drawer entity to a POS < 0.7 (§1.8).
pub const KIND_TILLS_ENTITY: &str = "legacy_tills_entity";
/// `POST/PATCH/DELETE /tills/{entity}` answered 410 `TILL_ENTITY_REMOVED` (§1.9).
pub const KIND_TILLS_ENTITY_GONE: &str = "legacy_tills_entity_gone";
/// `GET /refunds/shift/{id}` (§1.10).
pub const KIND_REFUNDS_SHIFT_ROUTE: &str = "legacy_refunds_shift_route";
/// `GET /reports/shifts/{id}/summary|deductions` (§1.11).
pub const KIND_REPORTS_SHIFTS_ROUTE: &str = "legacy_reports_shifts_route";
/// A live request body named the till `shift_id` (§2a: orders, settle, refund, finalize).
pub const KIND_SHIFT_ID_BODY: &str = "shift_id_body";
/// `GET /orders?shift_id=` / `/orders/export?shift_id=` (§2a).
pub const KIND_SHIFT_ID_QUERY: &str = "shift_id_query";
/// `GET /auth/permissions` read by an old POS, which gates on the `shifts:*` mirror (§3.1).
pub const KIND_PERM_PAYLOAD_OLD: &str = "perm_payload_old";
/// `/sync/replay` of `open_shift` / `close_shift` (§4.1, 4.2, 4.4).
pub const KIND_REPLAY_LEGACY_OP: &str = "replay_open_close_shift_op";
/// `/sync/replay` envelope or request carrying `shift_id` (§4.2, 4.3, 4.8).
pub const KIND_REPLAY_SHIFT_ID_FIELD: &str = "replay_shift_id_field";
/// A refusal kept in its old "shift" prose for old clients (§5).
pub const KIND_ERROR_WORDING: &str = "legacy_error_wording";
/// Analytics dataset `shifts` / preset `shift_cash_summary` (§6.1, 6.2).
pub const KIND_ANALYTICS_ALIAS: &str = "analytics_shifts_alias";
/// `GET /catalog/sync` (§8.1; no dashboard caller).
pub const KIND_CATALOG_SYNC: &str = "catalog_sync";
/// `GET /uploads/{path}` served the original file (§8.24).
pub const KIND_UPLOADS_LEGACY_PATH: &str = "uploads_legacy_path";
/// `GET /uploads/{path}` answered a 302 to the signed `full` variant (§8.24).
pub const KIND_UPLOADS_LEGACY_REDIRECT: &str = "uploads_legacy_redirect";
/// A per-endpoint offline mirror list read by a `pos/` or `kds/` client (§8a).
pub const KIND_MIRROR_LIST_POS: &str = "mirror_list_pos";

/// Optional header naming the branch a client works at (read for telemetry
/// only, and only when it is a branch of the token's org).
pub const BRANCH_HEADER: &str = "X-Madar-Branch";

/// One legacy path taken during a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyHit {
    pub kind: &'static str,
    /// Which site within the kind (e.g. the error wording), for the log only.
    pub site: Option<&'static str>,
    /// For hits on unauthenticated routes (login) whose handler knows the org.
    pub org_id: Option<Uuid>,
}

tokio::task_local! {
    static HITS: Arc<Mutex<Vec<LegacyHit>>>;
}

/// A legacy code path was taken. Logs `legacy_hit{kind}` and notes it for the
/// request's `client_seen` row. Never fails.
pub fn legacy_hit(kind: &'static str) {
    note(LegacyHit { kind, site: None, org_id: None });
}

/// [`legacy_hit`] naming the site within the kind.
pub fn legacy_hit_at(kind: &'static str, site: &'static str) {
    note(LegacyHit { kind, site: Some(site), org_id: None });
}

/// [`legacy_hit_at`] on a route with no token, when the handler knows the org.
pub fn legacy_hit_for_org(kind: &'static str, site: &'static str, org_id: Option<Uuid>) {
    note(LegacyHit { kind, site: Some(site), org_id });
}

fn note(hit: LegacyHit) {
    tracing::info!(target: "madar.legacy", kind = hit.kind, site = hit.site.unwrap_or(""), "legacy_hit");
    let _ = HITS.try_with(|hits| {
        if let Ok(mut v) = hits.lock() {
            v.push(hit);
        }
    });
}

/// Run `fut` with hit collection on and return what it recorded (tests, jobs).
pub async fn collect_hits<F: std::future::Future>(fut: F) -> (F::Output, Vec<LegacyHit>) {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let out = HITS.scope(hits.clone(), fut).await;
    let v = hits.lock().map(|v| v.clone()).unwrap_or_default();
    (out, v)
}

// ── request classification (no handler involvement) ─────────────────────────

/// Legacy route families recognisable from the path alone.
pub fn route_kind(path: &str) -> Option<&'static str> {
    if path == "/shifts" || path.starts_with("/shifts/") {
        Some(KIND_SHIFTS_ROUTE)
    } else if path.starts_with("/refunds/shift/") {
        Some(KIND_REFUNDS_SHIFT_ROUTE)
    } else if path.starts_with("/reports/shifts/") {
        Some(KIND_REPORTS_SHIFTS_ROUTE)
    } else {
        None
    }
}

/// A §8a offline-mirror list read (the POS-shaped variant of the route), by
/// method + path + query alone. Whether the caller is a POS is decided by
/// [`is_native_client`].
pub fn mirror_list_route(method: &actix_web::http::Method, path: &str, query: &str) -> bool {
    if method != actix_web::http::Method::GET {
        return false;
    }
    let has = |key: &str| query.split('&').any(|pair| pair.split('=').next() == Some(key));
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    match segs.as_slice() {
        ["", "menu-items"] => query.split('&').any(|p| p == "full=true"),
        ["", "floor", "transfers"] => has("since"),
        ["", "orders"] => has("till_id"),
        ["", "addon-items"]
        | ["", "categories"]
        | ["", "bundles"]
        | ["", "payment-methods"]
        | ["", "discounts"]
        | ["", "branches", _]
        | ["", "floor", "sections"]
        | ["", "floor", "tables"]
        | ["", "bookings"]
        | ["", "open-tickets"]
        | ["", "open-tickets", _]
        | ["", "kitchen", "orders"]
        | ["", "kitchen", "stations"]
        | ["", "kitchen", "routing-mode"]
        | ["", "delivery-orders"]
        | ["", "delivery-orders", _]
        | ["", "orgs", _, "offline-auth-bundle"]
        | ["", "tills", "branches", _]
        | ["", "tills", "branches", _, "open"]
        | ["", "tills", _, "cash-movements"]
        | ["", "tills", _, "refunds"]
        | ["", "refunds", "order", _] => true,
        _ => false,
    }
}

/// `X-Madar-Client` names a POS or KDS build (`pos/…`, `kds/…`).
pub fn is_native_client(headers: &actix_web::http::header::HeaderMap) -> bool {
    matches!(
        ClientHeader::parse(headers.get(CLIENT_HEADER).and_then(|v| v.to_str().ok())).app.as_deref(),
        Some("pos" | "kds")
    )
}

/// `GET /orders?shift_id=…` and `GET /orders/export?shift_id=…`.
pub fn query_uses_shift_id(method: &actix_web::http::Method, path: &str, query: &str) -> bool {
    method == actix_web::http::Method::GET
        && matches!(path, "/orders" | "/orders/export")
        && query.split('&').any(|pair| pair.split('=').next() == Some("shift_id"))
}

/// Live POST routes whose body structs accept `shift_id` as an alias of `till_id`.
pub fn body_may_alias_shift_id(method: &actix_web::http::Method, path: &str) -> bool {
    if method != actix_web::http::Method::POST {
        return false;
    }
    let segs: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    matches!(segs.as_slice(), ["", "orders"] | ["", "refunds"])
        || matches!(segs.as_slice(), ["", "open-tickets", _, "settle"])
        || matches!(segs.as_slice(), ["", "delivery-orders", _, "finalize"])
}

/// A JSON body naming the key `shift_id`.
pub fn body_names_shift_id(body: &[u8]) -> bool {
    const KEY: &[u8] = b"\"shift_id\"";
    body.windows(KEY.len()).any(|w| w == KEY)
}

fn header_str<'a>(headers: &'a actix_web::http::header::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

/// A web browser's User-Agent (the dashboard, in a browser or the Tauri webview).
pub fn is_browser(headers: &actix_web::http::header::HeaderMap) -> bool {
    header_str(headers, "User-Agent").is_some_and(|ua| ua.contains("Mozilla"))
}

/// The client string: `X-Madar-Client`; else `dashboard` for a browser; else
/// the `User-Agent`. Trimmed, ≤ 200 chars.
pub fn client_string(headers: &actix_web::http::header::HeaderMap) -> Option<String> {
    if let Some(c) = header_str(headers, CLIENT_HEADER) {
        return Some(c.chars().take(200).collect());
    }
    if is_browser(headers) {
        return Some(DASHBOARD_CLIENT.to_string());
    }
    header_str(headers, "User-Agent").map(|s| s.chars().take(200).collect())
}

/// What a browser without `X-Madar-Client` is recorded as.
pub const DASHBOARD_CLIENT: &str = "dashboard";

/// The app version, read ONLY from an `X-Madar-Client` of the form
/// `<app>/<semver>` (`pos/0.7.2 (ios)` → `0.7.2`). A User-Agent never yields
/// one (`madar-core/0.1.0` is the crate, `Dart/3.4` the runtime, a browser's
/// `Mozilla/5.0` nothing), and the dashboard has no version.
pub fn app_version(headers: &actix_web::http::header::HeaderMap) -> Option<String> {
    let parsed = ClientHeader::parse(header_str(headers, CLIENT_HEADER));
    if matches!(parsed.app.as_deref(), None | Some(DASHBOARD_CLIENT)) {
        return None;
    }
    parsed.version.map(|(a, b, c)| format!("{a}.{b}.{c}"))
}

/// `X-Madar-Branch`, when it is a UUID (checked against the org at upsert).
pub fn branch_header(headers: &actix_web::http::header::HeaderMap) -> Option<Uuid> {
    header_str(headers, BRANCH_HEADER).and_then(|v| Uuid::parse_str(v).ok())
}

/// A POS that predates `X-Madar-Client` (or reports < 0.7). Browsers (the
/// dashboard, which never gated on `shifts:*`) are not counted.
pub fn is_legacy_pos_request(headers: &actix_web::http::header::HeaderMap) -> bool {
    let client = ClientHeader::parse(headers.get(CLIENT_HEADER).and_then(|v| v.to_str().ok()));
    client.is_legacy_pos() && !(client.app.is_none() && is_browser(headers))
}

pub fn seen_key(device_id: Option<Uuid>, branch_id: Option<Uuid>, client: Option<&str>) -> String {
    match device_id {
        Some(d) => format!("d:{d}"),
        None => format!(
            "c:{}:{}",
            branch_id.map(|b| b.to_string()).unwrap_or_else(|| "-".into()),
            client.unwrap_or("-")
        ),
    }
}

// ── throttle ─────────────────────────────────────────────────────────────────

fn throttle_map() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `key` was not let through within [`THROTTLE`]; marks it now.
pub fn throttle_allows(key: &str, now: Instant) -> bool {
    let Ok(mut map) = throttle_map().lock() else { return false };
    if map.len() > 50_000 {
        map.retain(|_, t| now.duration_since(*t) < THROTTLE);
    }
    match map.get(key) {
        Some(t) if now.duration_since(*t) < THROTTLE => false,
        _ => {
            map.insert(key.to_string(), now);
            true
        }
    }
}

// ── the row ──────────────────────────────────────────────────────────────────

/// What one request contributes to `client_seen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub org_id: Uuid,
    /// The token's branch claim. When absent the upsert resolves the branch
    /// from the device row (`devices.branch_id`), then from `branch_hint`.
    pub branch_id: Option<Uuid>,
    /// `X-Madar-Branch`; used only when it is a branch of `org_id`.
    pub branch_hint: Option<Uuid>,
    pub device_id: Option<Uuid>,
    pub client: Option<String>,
    pub app_version: Option<String>,
    /// Legacy kinds not throttled for this device (empty = a plain sighting).
    pub legacy_kinds: Vec<&'static str>,
    pub path: String,
}

/// Upsert one sighting. `last_legacy_*` move only when a legacy kind is present.
pub async fn upsert(pool: &PgPool, s: &Sighting) -> Result<(), sqlx::Error> {
    let key = seen_key(s.device_id, s.branch_id.or(s.branch_hint), s.client.as_deref());
    let last_kind = s.legacy_kinds.last().copied();
    let kinds: Vec<String> = s.legacy_kinds.iter().map(|k| k.to_string()).collect();
    sqlx::query(
        "INSERT INTO client_seen (org_id, seen_key, branch_id, device_id, client, app_version, \
                                  last_legacy_kind, last_legacy_path, last_legacy_at, legacy_kinds) \
         VALUES ($1, $2, \
                 COALESCE($3, (SELECT d.branch_id FROM devices d WHERE d.id = $4 AND d.org_id = $1), \
                          (SELECT b.id FROM branches b WHERE b.id = $10 AND b.org_id = $1)), \
                 $4, $5, $6, $7, CASE WHEN $7::text IS NULL THEN NULL ELSE $8 END, \
                 CASE WHEN $7::text IS NULL THEN NULL ELSE now() END, $9) \
         ON CONFLICT (org_id, seen_key) DO UPDATE SET \
           branch_id        = COALESCE(EXCLUDED.branch_id, client_seen.branch_id), \
           device_id        = COALESCE(EXCLUDED.device_id, client_seen.device_id), \
           client           = COALESCE(EXCLUDED.client, client_seen.client), \
           app_version      = CASE WHEN EXCLUDED.client IS NULL THEN client_seen.app_version ELSE EXCLUDED.app_version END, \
           last_seen_at     = now(), \
           last_legacy_kind = COALESCE(EXCLUDED.last_legacy_kind, client_seen.last_legacy_kind), \
           last_legacy_path = COALESCE(EXCLUDED.last_legacy_path, client_seen.last_legacy_path), \
           last_legacy_at   = COALESCE(EXCLUDED.last_legacy_at, client_seen.last_legacy_at), \
           legacy_kinds     = ARRAY(SELECT DISTINCT k FROM unnest(client_seen.legacy_kinds || EXCLUDED.legacy_kinds) k ORDER BY k)",
    )
    .bind(s.org_id)
    .bind(&key)
    .bind(s.branch_id)
    .bind(s.device_id)
    .bind(&s.client)
    .bind(&s.app_version)
    .bind(last_kind)
    .bind(&s.path)
    .bind(&kinds)
    .bind(s.branch_hint)
    .execute(pool)
    .await
    .map(|_| ())
}

// ── middleware ───────────────────────────────────────────────────────────────

/// App-level middleware: classify, run the handler with hit collection on,
/// then (throttled, in the background) upsert the caller's `client_seen` row.
pub async fn record(
    mut req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    // The ROOT pool (owner role; the table is written for the org the token
    // names). Taken now: after routing, a scope's own pool (reports' read
    // replica) would shadow it.
    let pool = req.app_data::<web::Data<PgPool>>().map(|p| p.get_ref().clone());
    let hits = Arc::new(Mutex::new(Vec::new()));

    let path = req.path().to_string();
    let method = req.method().clone();
    let pre: Vec<&'static str> = {
        let mut v = Vec::new();
        if let Some(kind) = route_kind(&path) {
            v.push(kind);
        }
        if query_uses_shift_id(&method, &path, req.query_string()) {
            v.push(KIND_SHIFT_ID_QUERY);
        }
        if is_native_client(req.headers()) && mirror_list_route(&method, &path, req.query_string()) {
            v.push(KIND_MIRROR_LIST_POS);
        }
        v
    };
    if body_may_alias_shift_id(&method, &path) {
        let small = req
            .headers()
            .get(actix_web::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|n| n <= MAX_SCAN_BODY);
        if small {
            let mut payload = req.take_payload();
            let mut buf = web::BytesMut::new();
            while let Some(chunk) = payload.next().await {
                buf.extend_from_slice(&chunk?);
            }
            let bytes = buf.freeze();
            let aliased = body_names_shift_id(&bytes);
            req.set_payload(actix_web::dev::Payload::from(bytes));
            if aliased {
                hits.lock().map(|mut h| h.push(LegacyHit { kind: KIND_SHIFT_ID_BODY, site: None, org_id: None })).ok();
                tracing::info!(target: "madar.legacy", kind = KIND_SHIFT_ID_BODY, site = "", "legacy_hit");
            }
        }
    }
    for kind in pre {
        hits.lock().map(|mut h| h.push(LegacyHit { kind, site: None, org_id: None })).ok();
        tracing::info!(target: "madar.legacy", kind, site = "", "legacy_hit");
    }

    let headers = req.headers().clone();
    let res = HITS.scope(hits.clone(), async move { next.call(req).await }).await?;

    let claims = res.request().extensions().get::<Claims>().cloned();
    let hits = hits.lock().map(|v| v.clone()).unwrap_or_default();
    let org = claims.as_ref().and_then(|c| c.org_id()).or_else(|| hits.iter().find_map(|h| h.org_id));
    if let (Some(pool), Some(org_id)) = (pool, org) {
        let device_id = DeviceHeader::from_request_headers(res.request());
        let branch_id = claims.as_ref().and_then(|c| c.branch_id());
        let branch_hint = branch_header(&headers);
        let client = client_string(&headers);
        let key = format!("{org_id}|{}", seen_key(device_id, branch_id.or(branch_hint), client.as_deref()));
        let now = Instant::now();
        let mut kinds: Vec<&'static str> = Vec::new();
        for h in &hits {
            if !kinds.contains(&h.kind) && throttle_allows(&format!("{key}|{}", h.kind), now) {
                kinds.push(h.kind);
            }
        }
        if throttle_allows(&key, now) || !kinds.is_empty() {
            let sighting = Sighting {
                org_id,
                branch_id,
                branch_hint,
                device_id,
                app_version: app_version(&headers),
                client,
                legacy_kinds: kinds,
                path,
            };
            tokio::spawn(async move {
                if let Err(e) = upsert(&pool, &sighting).await {
                    tracing::debug!(target: "madar.client_seen", error = %e, "client_seen upsert failed");
                }
            });
        }
    }
    Ok(res)
}
