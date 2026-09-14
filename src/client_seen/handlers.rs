//! `GET /devices/client-versions` — which devices and app versions still take
//! legacy code paths (the removal gates in LEGACY_REMOVAL.md read this).

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ClientVersionsQuery {
    /// Only clients that took a legacy path within the window (default `true`).
    #[serde(default)]
    pub legacy_only: Option<bool>,
    /// Look-back window in days, 1..=365 (default 14 — the G-old gate).
    #[serde(default)]
    pub days: Option<i32>,
    /// Narrow to one branch.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

/// One device (or device-less client) as last seen.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ClientSeen {
    pub branch_id: Option<Uuid>,
    pub branch_name: Option<String>,
    pub device_id: Option<Uuid>,
    /// The registered device's code, when the device is registered.
    pub device_code: Option<String>,
    /// `X-Madar-Client`; else `dashboard` for a browser; else the User-Agent.
    pub client: Option<String>,
    /// From an `X-Madar-Client` of the form `<app>/<semver>` only; `null` for
    /// the dashboard and for any client identified by its User-Agent.
    pub app_version: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    /// The latest legacy path kind (`legacy_shifts_route`, `replay_shift_id_field`, …).
    pub last_legacy_kind: Option<String>,
    pub last_legacy_path: Option<String>,
    pub last_legacy_at: Option<DateTime<Utc>>,
    /// Every legacy kind this client has hit.
    pub legacy_kinds: Vec<String>,
}

#[utoipa::path(get, path = "/devices/client-versions", tag = "devices", params(ClientVersionsQuery),
    responses((status = 200, description = "Devices and app versions seen in the window, newest legacy hit first", body = Vec<ClientSeen>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_client_versions(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ClientVersionsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // An org-wide operational view: the branch EDITOR grant (org admins by default).
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::BadRequest("A token with an organization is required".into()))?;
    let days = q.days.unwrap_or(14);
    if !(1..=365).contains(&days) {
        return Err(AppError::BadRequest(
            "days must be between 1 and 365".into(),
        ));
    }
    let legacy_only = q.legacy_only.unwrap_or(true);
    let rows = sqlx::query_as::<_, ClientSeen>(
        "SELECT c.branch_id, b.name AS branch_name, c.device_id, d.code AS device_code, c.client, c.app_version, \
                c.first_seen_at, c.last_seen_at, c.last_legacy_kind, c.last_legacy_path, c.last_legacy_at, c.legacy_kinds \
           FROM client_seen c \
           LEFT JOIN branches b ON b.id = c.branch_id \
           LEFT JOIN devices d ON d.id = c.device_id AND d.org_id = c.org_id \
          WHERE c.org_id = $1 \
            AND ($2::uuid IS NULL OR c.branch_id = $2) \
            AND CASE WHEN $3 THEN c.last_legacy_at >= now() - make_interval(days => $4) \
                     ELSE c.last_seen_at >= now() - make_interval(days => $4) END \
          ORDER BY c.last_legacy_at DESC NULLS LAST, c.last_seen_at DESC",
    )
    .bind(org_id)
    .bind(q.branch_id)
    .bind(legacy_only)
    .bind(days)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}
