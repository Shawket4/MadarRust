//! Super-admin relay to the WhatsApp send gateway (the Go `madar-whatsapp`
//! service). The gateway is **never** exposed to the public; only the backend
//! reaches it over the private network. These endpoints let a super-admin pair
//! a number (scan the QR from the dashboard), see the link status, log out, and
//! pause/resume sending — all without anyone touching the Go service directly.
//!
//! Every handler is hard-gated to `SuperAdmin` via [`require_super_admin`]; the
//! permission table is intentionally NOT consulted, so no per-role grant can
//! ever open these up to org admins or below.
//!
//! Upstream contract (see `madar-whatsapp/main.go`):
//!   `POST /sessions/{name}/pair`    → start pairing (QR appears shortly after)
//!   `GET  /sessions/{name}/status`  → { connected, logged_in, has_qr }
//!   `GET  /sessions/{name}/qr.png`  → current pairing QR as a PNG
//!   `POST /sessions/{name}/logout`  → unlink the number
//!
//! The QR PNG is fetched server-side and inlined as a base64 data-URL so the
//! dashboard renders it with a plain `<img>` and never has a route to the
//! gateway itself.

use actix_web::{web, HttpRequest, HttpResponse};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;

use super::extract_claims;
use crate::auth::guards::require_super_admin;
use crate::errors::{AppError, AppErrorResponse};

// ── env plumbing ──────────────────────────────────────────────

/// Base URL of the Go gateway (`WHATSAPP_SERVICE_URL`), trailing slash trimmed.
fn gateway_base() -> Option<String> {
    std::env::var("WHATSAPP_SERVICE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// Session name the relay operates on (`WHATSAPP_SESSION`, default `main`).
/// One linked number is the norm; the name just keys it in the gateway store.
fn session_name() -> String {
    std::env::var("WHATSAPP_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Optional shared secret echoed to the gateway (`WHATSAPP_AUTH_HEADER`). The
/// gateway only enforces it on `/send/message`, but we send it everywhere for
/// consistency and future-proofing.
fn auth_header() -> Option<String> {
    std::env::var("WHATSAPP_AUTH_HEADER").ok().filter(|s| !s.is_empty())
}

// ── wire shapes ───────────────────────────────────────────────

/// Snapshot returned to the dashboard. Combines the gateway's live link state
/// with the backend's persisted pause switch.
#[derive(Serialize, ToSchema)]
pub struct WhatsappStatus {
    /// `WHATSAPP_SERVICE_URL` is set on the backend.
    pub configured: bool,
    /// The gateway answered over HTTP (false = not configured or unreachable).
    pub reachable: bool,
    /// Session name the relay pairs/sends under.
    pub session: String,
    /// Underlying socket is connected to WhatsApp.
    pub connected: bool,
    /// A number is linked and ready to send.
    pub logged_in: bool,
    /// A pairing QR is currently available to scan.
    pub has_qr: bool,
    /// Current pairing QR as a `data:image/png;base64,…` URL (only when `has_qr`).
    pub qr_image: Option<String>,
    /// Sending is paused by an admin — the number stays linked but every
    /// outbound message (OTP + status) is suppressed until resumed.
    pub paused: bool,
    /// When sending was last paused (audit).
    pub paused_at: Option<DateTime<Utc>>,
}

/// Body for `POST /whatsapp/pause`.
#[derive(Deserialize, ToSchema)]
pub struct PauseInput {
    /// `true` = mute all sends; `false` = resume.
    pub paused: bool,
}

/// The gateway's `GET /sessions/{name}/status` shape (subset we care about).
#[derive(Deserialize, Default)]
struct GatewayStatus {
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    has_qr: bool,
}

// ── persisted pause switch ────────────────────────────────────

struct PauseState {
    paused: bool,
    paused_at: Option<DateTime<Utc>>,
}

/// Read the singleton pause row. Treats any read failure / missing row as
/// "not paused" so a transient DB hiccup never silently mutes the gateway.
async fn read_pause(pool: &PgPool) -> PauseState {
    match sqlx::query_as::<_, (bool, Option<DateTime<Utc>>)>(
        "SELECT paused, paused_at FROM whatsapp_gateway_settings WHERE id",
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some((paused, paused_at))) => PauseState { paused, paused_at },
        _ => PauseState { paused: false, paused_at: None },
    }
}

/// True when the gateway is muted. Used by the send path (cheap single-row read).
pub async fn is_paused(pool: &PgPool) -> bool {
    read_pause(pool).await.paused
}

// ── gateway HTTP relay ────────────────────────────────────────

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn fetch_gateway_status(base: &str, session: &str) -> Result<GatewayStatus, reqwest::Error> {
    let url = format!("{base}/sessions/{session}/status");
    let mut req = client().get(&url);
    if let Some(h) = auth_header() {
        req = req.header("Authorization", h);
    }
    req.send().await?.error_for_status()?.json::<GatewayStatus>().await
}

/// Fetch the current QR PNG and inline it as a data-URL. Returns `None` if the
/// QR isn't available (already linked, or pairing not yet started).
async fn fetch_qr_data_url(base: &str, session: &str) -> Option<String> {
    let url = format!("{base}/sessions/{session}/qr.png");
    let mut req = client().get(&url);
    if let Some(h) = auth_header() {
        req = req.header("Authorization", h);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:image/png;base64,{b64}"))
}

/// Build the full status snapshot (gateway link state + pause switch). Never
/// errors: an unconfigured or unreachable gateway yields `reachable = false`.
async fn build_status(pool: &PgPool) -> WhatsappStatus {
    let PauseState { paused, paused_at } = read_pause(pool).await;
    let session = session_name();

    let Some(base) = gateway_base() else {
        return WhatsappStatus {
            configured: false,
            reachable: false,
            session,
            connected: false,
            logged_in: false,
            has_qr: false,
            qr_image: None,
            paused,
            paused_at,
        };
    };

    match fetch_gateway_status(&base, &session).await {
        Ok(s) => {
            let qr_image = if s.has_qr {
                fetch_qr_data_url(&base, &session).await
            } else {
                None
            };
            WhatsappStatus {
                configured: true,
                reachable: true,
                session,
                connected: s.connected,
                logged_in: s.logged_in,
                has_qr: s.has_qr,
                qr_image,
                paused,
                paused_at,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "WhatsApp gateway status fetch failed");
            WhatsappStatus {
                configured: true,
                reachable: false,
                session,
                connected: false,
                logged_in: false,
                has_qr: false,
                qr_image: None,
                paused,
                paused_at,
            }
        }
    }
}

/// Require a configured gateway, returning the base URL or a 503.
fn require_gateway() -> Result<String, AppError> {
    gateway_base().ok_or_else(|| {
        AppError::ServiceUnavailable("WhatsApp gateway is not configured (WHATSAPP_SERVICE_URL unset)".into())
    })
}

// ── handlers ──────────────────────────────────────────────────

/// Current WhatsApp link + pause status, with the pairing QR inlined when one
/// is waiting to be scanned. Safe to poll from the dashboard.
#[utoipa::path(
    get, path = "/whatsapp/status", tag = "whatsapp", operation_id = "whatsapp_status",
    responses((status = 200, body = WhatsappStatus), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn status(req: HttpRequest, pool: web::Data<PgPool>) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_super_admin(&claims)?;
    Ok(HttpResponse::Ok().json(build_status(pool.get_ref()).await))
}

/// Start (or restart) pairing on the gateway. The QR becomes available a moment
/// later — the dashboard polls `GET /whatsapp/status` until `has_qr`, shows it,
/// then keeps polling until `logged_in`.
#[utoipa::path(
    post, path = "/whatsapp/pair", tag = "whatsapp", operation_id = "whatsapp_pair",
    responses(
        (status = 200, body = WhatsappStatus),
        (status = 503, description = "WhatsApp gateway not configured or unreachable"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn pair(req: HttpRequest, pool: web::Data<PgPool>) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_super_admin(&claims)?;
    let base = require_gateway()?;
    let session = session_name();

    let url = format!("{base}/sessions/{session}/pair");
    let mut r = client().post(&url);
    if let Some(h) = auth_header() {
        r = r.header("Authorization", h);
    }
    match r.send().await {
        // 200 = pairing started; 409 = already linked. Both are fine — the
        // follow-up status poll reflects the real state either way.
        Ok(resp) if resp.status().is_success() || resp.status() == reqwest::StatusCode::CONFLICT => {}
        Ok(resp) => {
            let code = resp.status();
            tracing::warn!(status = %code, "WhatsApp pair returned non-2xx");
            return Err(AppError::ServiceUnavailable(format!(
                "WhatsApp gateway rejected pairing (HTTP {code})"
            )));
        }
        Err(e) => {
            tracing::warn!(error = %e, "WhatsApp pair request failed");
            return Err(AppError::ServiceUnavailable("WhatsApp gateway is unreachable".into()));
        }
    }

    Ok(HttpResponse::Ok().json(build_status(pool.get_ref()).await))
}

/// Unlink the current number. Idempotent — logging out an already-unlinked
/// session still returns the (now logged-out) status.
#[utoipa::path(
    post, path = "/whatsapp/logout", tag = "whatsapp", operation_id = "whatsapp_logout",
    responses(
        (status = 200, body = WhatsappStatus),
        (status = 503, description = "WhatsApp gateway not configured or unreachable"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn logout(req: HttpRequest, pool: web::Data<PgPool>) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_super_admin(&claims)?;
    let base = require_gateway()?;
    let session = session_name();

    let url = format!("{base}/sessions/{session}/logout");
    let mut r = client().post(&url);
    if let Some(h) = auth_header() {
        r = r.header("Authorization", h);
    }
    match r.send().await {
        // 200 = logged out; 400 = unknown session (already gone). Both fine.
        Ok(resp) if resp.status().is_success() || resp.status() == reqwest::StatusCode::BAD_REQUEST => {}
        Ok(resp) => {
            let code = resp.status();
            tracing::warn!(status = %code, "WhatsApp logout returned non-2xx");
            return Err(AppError::ServiceUnavailable(format!(
                "WhatsApp gateway rejected logout (HTTP {code})"
            )));
        }
        Err(e) => {
            tracing::warn!(error = %e, "WhatsApp logout request failed");
            return Err(AppError::ServiceUnavailable("WhatsApp gateway is unreachable".into()));
        }
    }

    Ok(HttpResponse::Ok().json(build_status(pool.get_ref()).await))
}

/// Pause or resume all outbound WhatsApp sends. Persisted; survives restarts
/// and does not touch the linked session.
#[utoipa::path(
    post, path = "/whatsapp/pause", tag = "whatsapp", operation_id = "whatsapp_pause",
    request_body = PauseInput,
    responses((status = 200, body = WhatsappStatus), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn pause(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<PauseInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_super_admin(&claims)?;

    sqlx::query(
        "UPDATE whatsapp_gateway_settings \
         SET paused = $1, \
             paused_at = CASE WHEN $1 THEN now() ELSE NULL END, \
             paused_by = CASE WHEN $1 THEN $2 ELSE NULL END, \
             updated_at = now() \
         WHERE id",
    )
    .bind(body.paused)
    .bind(claims.user_id())
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(build_status(pool.get_ref()).await))
}
