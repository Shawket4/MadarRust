//! `/admin/sessions`: every sign-in's server-side session (`auth::sessions`),
//! listed and revoked by a super admin. The only caller is madar-admin; the
//! dashboard has no session UI.
//!
//! Every handler checks `require_super_admin` itself (the house pattern for
//! super-admin routes), under `JwtMiddleware`.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::auth::guards::require_super_admin;
use crate::auth::jwt::Claims;
use crate::auth::middleware::JwtMiddleware;
use crate::auth::sessions::{self, Revocation};
use crate::errors::{AppError, AppErrorResponse};

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;
const KINDS: [&str; 6] = ["password", "pin", "device", "signup", "demo", "legacy"];

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/admin")
            .wrap(JwtMiddleware)
            .route("/sessions", web::get().to(list_sessions))
            .route("/sessions/{id}", web::get().to(get_session))
            .route("/sessions/{id}/revoke", web::post().to(revoke_session))
            .route(
                "/users/{id}/revoke-sessions",
                web::post().to(revoke_user_sessions),
            )
            .route(
                "/orgs/{id}/revoke-sessions",
                web::post().to(revoke_org_sessions),
            ),
    );
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SessionQuery {
    pub org_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub branch_id: Option<Uuid>,
    /// `password` | `pin` | `device` | `signup` | `demo` | `legacy`.
    pub kind: Option<String>,
    /// Matches the person's name or email, the device id or the IP.
    pub q: Option<String>,
    /// `active` (default) | `revoked` | `expired` | `all`.
    pub state: Option<String>,
    pub limit: Option<i64>,
    /// Keyset cursor: the `next` of the previous page.
    pub before: Option<DateTime<Utc>>,
    pub before_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub user_name: String,
    pub user_email: Option<String>,
    pub role: String,
    pub org_id: Option<Uuid>,
    pub org_name: Option<String>,
    pub branch_id: Option<Uuid>,
    pub branch_name: Option<String>,
    pub kind: String,
    pub device_id: Option<String>,
    /// `X-Madar-Client`: `<app>/<version> (<platform>)`.
    pub client: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by_name: Option<String>,
    pub revoke_reason: Option<String>,
    /// `active` | `revoked` | `expired`.
    pub state: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionCursor {
    pub before: DateTime<Utc>,
    pub before_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionPage {
    pub sessions: Vec<SessionRow>,
    /// Present when there may be more: pass it back as `before` / `before_id`.
    pub next: Option<SessionCursor>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RevokeRequest {
    /// Why, kept on the row (`revoke_reason`). Required.
    pub reason: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RevokeOrgRequest {
    pub reason: String,
    /// Only these kinds (e.g. `["pin", "device"]` signs out every till). Absent:
    /// every session of the org, and older tokens of its people too.
    pub kinds: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RevokeResponse {
    /// Sessions that were live and are now revoked.
    pub revoked: u64,
}

fn caller(req: &HttpRequest) -> Result<Claims, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;
    require_super_admin(&claims)?;
    Ok(claims)
}

fn reason(raw: &str) -> Result<String, AppError> {
    let r = raw.trim();
    if r.is_empty() {
        return Err(AppError::BadRequest("reason is required".into()));
    }
    Ok(r.chars().take(200).collect())
}

fn check_kind(k: &str) -> Result<(), AppError> {
    if KINDS.contains(&k) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("unknown session kind: {k}")))
    }
}

/// The one read: the list, or (with `id`) a single row.
async fn fetch(
    pool: &PgPool,
    q: &SessionQuery,
    id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<SessionRow>, AppError> {
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                format!(
                    "%{}%",
                    s.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });
    let state = q.state.as_deref().unwrap_or("active");
    Ok(sqlx::query_as!(
        SessionRow,
        r#"SELECT s.id, s.user_id, u.name AS user_name, u.email AS "user_email?",
                  s.role::text AS "role!", s.org_id, o.name AS "org_name?",
                  s.branch_id, b.name AS "branch_name?", s.kind, s.device_id, s.client,
                  host(s.ip) AS ip, s.user_agent, s.created_at, s.last_seen_at,
                  s.expires_at, s.revoked_at, rb.name AS "revoked_by_name?",
                  s.revoke_reason,
                  CASE WHEN s.revoked_at IS NOT NULL THEN 'revoked'
                       WHEN s.expires_at <= now() THEN 'expired'
                       ELSE 'active' END AS "state!"
             FROM auth_sessions s
             JOIN users u ON u.id = s.user_id
             LEFT JOIN organizations o ON o.id = s.org_id
             LEFT JOIN branches b ON b.id = s.branch_id
             LEFT JOIN users rb ON rb.id = s.revoked_by
            WHERE ($1::uuid IS NULL OR s.id = $1)
              AND ($2::uuid IS NULL OR s.org_id = $2)
              AND ($3::uuid IS NULL OR s.user_id = $3)
              AND ($4::uuid IS NULL OR s.branch_id = $4)
              AND ($5::text IS NULL OR s.kind = $5)
              AND ($6::text IS NULL OR u.name ILIKE $6 OR u.email ILIKE $6
                   OR s.device_id ILIKE $6 OR host(s.ip) ILIKE $6)
              AND CASE $7::text
                    WHEN 'active' THEN s.revoked_at IS NULL AND s.expires_at > now()
                    WHEN 'revoked' THEN s.revoked_at IS NOT NULL
                    WHEN 'expired' THEN s.revoked_at IS NULL AND s.expires_at <= now()
                    ELSE true END
              AND ($8::timestamptz IS NULL OR (s.created_at, s.id) < ($8, $9::uuid))
            ORDER BY s.created_at DESC, s.id DESC
            LIMIT $10"#,
        id,
        q.org_id,
        q.user_id,
        q.branch_id,
        q.kind.as_deref(),
        search.as_deref(),
        if id.is_some() { "all" } else { state },
        q.before,
        q.before_id.unwrap_or(Uuid::max()),
        limit,
    )
    .fetch_all(pool)
    .await?)
}

/// List sessions, newest first.
#[utoipa::path(
    get,
    path = "/admin/sessions",
    tag = "admin",
    params(SessionQuery),
    responses(
        (status = 200, description = "A page of sessions", body = SessionPage),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_sessions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    q: web::Query<SessionQuery>,
) -> Result<HttpResponse, AppError> {
    caller(&req)?;
    if let Some(k) = q.kind.as_deref() {
        check_kind(k)?;
    }
    if let Some(s) = q.state.as_deref()
        && !matches!(s, "active" | "revoked" | "expired" | "all")
    {
        return Err(AppError::BadRequest(format!("unknown state: {s}")));
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let sessions = fetch(&pool, &q, None, limit).await?;
    let next = (sessions.len() as i64 == limit)
        .then(|| sessions.last())
        .flatten()
        .map(|s| SessionCursor {
            before: s.created_at,
            before_id: s.id,
        });
    Ok(HttpResponse::Ok().json(SessionPage { sessions, next }))
}

/// One session.
#[utoipa::path(
    get,
    path = "/admin/sessions/{id}",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session", body = SessionRow),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_session(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    caller(&req)?;
    let q = SessionQuery {
        org_id: None,
        user_id: None,
        branch_id: None,
        kind: None,
        q: None,
        state: None,
        limit: None,
        before: None,
        before_id: None,
    };
    let row = fetch(&pool, &q, Some(*path), 1)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("session".into()))?;
    Ok(HttpResponse::Ok().json(row))
}

/// Revoke one session: its next request is refused (401 `SESSION_REVOKED`).
#[utoipa::path(
    post,
    path = "/admin/sessions/{id}/revoke",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Session id")),
    request_body = RevokeRequest,
    responses(
        (status = 200, description = "Revoked (0 when it was not live)", body = RevokeResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn revoke_session(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    body: web::Json<RevokeRequest>,
) -> Result<HttpResponse, AppError> {
    let me = caller(&req)?;
    let reason = reason(&body.reason)?;
    let sid = *path;
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM auth_sessions WHERE id = $1) AS "e!""#,
        sid
    )
    .fetch_one(pool.get_ref())
    .await?;
    if !exists {
        return Err(AppError::NotFound("session".into()));
    }
    let live = sessions::revoke_one(
        &pool,
        sid,
        &Revocation {
            by: me.user_id_safe().ok(),
            reason: &reason,
        },
    )
    .await?;
    tracing::info!(target: "auth.sessions.revoke", by = %me.sub, session = %sid, %reason, live);
    Ok(HttpResponse::Ok().json(RevokeResponse {
        revoked: live as u64,
    }))
}

/// Sign a person out everywhere, including tokens of theirs from before
/// sessions existed.
#[utoipa::path(
    post,
    path = "/admin/users/{id}/revoke-sessions",
    tag = "admin",
    params(("id" = Uuid, Path, description = "User id")),
    request_body = RevokeRequest,
    responses(
        (status = 200, description = "Sessions revoked", body = RevokeResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn revoke_user_sessions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    body: web::Json<RevokeRequest>,
) -> Result<HttpResponse, AppError> {
    let me = caller(&req)?;
    let reason = reason(&body.reason)?;
    let user = *path;
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE id = $1) AS "e!""#,
        user
    )
    .fetch_one(pool.get_ref())
    .await?;
    if !exists {
        return Err(AppError::NotFound("user".into()));
    }
    let n = sessions::revoke_user(
        &pool,
        user,
        &Revocation {
            by: me.user_id_safe().ok(),
            reason: &reason,
        },
    )
    .await?;
    tracing::info!(target: "auth.sessions.revoke", by = %me.sub, %user, %reason, revoked = n);
    Ok(HttpResponse::Ok().json(RevokeResponse { revoked: n }))
}

/// Sign out a whole org, or only its sessions of some kinds.
#[utoipa::path(
    post,
    path = "/admin/orgs/{id}/revoke-sessions",
    tag = "admin",
    params(("id" = Uuid, Path, description = "Organization id")),
    request_body = RevokeOrgRequest,
    responses(
        (status = 200, description = "Sessions revoked", body = RevokeResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn revoke_org_sessions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    body: web::Json<RevokeOrgRequest>,
) -> Result<HttpResponse, AppError> {
    let me = caller(&req)?;
    let reason = reason(&body.reason)?;
    let org = *path;
    if let Some(kinds) = &body.kinds {
        if kinds.is_empty() {
            return Err(AppError::BadRequest(
                "kinds, when given, names at least one".into(),
            ));
        }
        for k in kinds {
            check_kind(k)?;
        }
    }
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM organizations WHERE id = $1) AS "e!""#,
        org
    )
    .fetch_one(pool.get_ref())
    .await?;
    if !exists {
        return Err(AppError::NotFound("organization".into()));
    }
    let n = sessions::revoke_org(
        &pool,
        org,
        body.kinds.as_deref(),
        &Revocation {
            by: me.user_id_safe().ok(),
            reason: &reason,
        },
    )
    .await?;
    tracing::info!(
        target: "auth.sessions.revoke",
        by = %me.sub, %org, kinds = ?body.kinds, %reason, revoked = n
    );
    Ok(HttpResponse::Ok().json(RevokeResponse { revoked: n }))
}
