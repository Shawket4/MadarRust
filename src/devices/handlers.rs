use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::valid_code;
use crate::{
    delivery::require_branch_access,
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
};

/// OpenAPI-only vocabulary for `devices.kind` (CHECK `kind IN ('pos','kds','waiter')`).
/// The struct fields stay `String`, so the wire strings are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Pos,
    Kds,
    Waiter,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Device {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub code: String,
    pub label: Option<String>,
    #[schema(value_type = DeviceKind)]
    pub kind: String,
    pub platform: Option<String>,
    pub app_version: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
    /// Another live device at the same branch uses the same code.
    pub code_conflict: bool,
}

const DEVICE_SELECT: &str = "SELECT d.id, d.org_id, d.branch_id, d.code, d.label, d.kind, d.platform, \
    d.app_version, d.first_seen_at, d.last_seen_at, d.retired_at, \
    EXISTS(SELECT 1 FROM devices d2 WHERE d2.branch_id = d.branch_id AND d2.code = d.code \
           AND d2.id <> d.id AND d2.retired_at IS NULL) AS code_conflict \
    FROM devices d";

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct RegisterDeviceRequest {
    pub id: Uuid,
    pub code: String,
    #[serde(default)]
    pub label: Option<String>,
    /// `pos` | `kds` | `waiter`
    #[schema(value_type = DeviceKind)]
    pub kind: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
    pub branch_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct UpdateDeviceRequest {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default, deserialize_with = "crate::devices::handlers::double_option")]
    #[schema(value_type = Option<String>)]
    pub label: Option<Option<String>>,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub retired: Option<bool>,
}

pub fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListDevicesQuery {
    pub branch_id: Uuid,
}

async fn fetch_device(pool: &sqlx::PgPool, id: Uuid) -> Result<Device, AppError> {
    sqlx::query_as::<_, Device>(&format!("{DEVICE_SELECT} WHERE d.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Device not found".into()))
}

fn clean_label(l: Option<&str>) -> Option<String> {
    l.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(120).collect())
}

#[utoipa::path(post, path = "/devices/register", tag = "devices",
    request_body = RegisterDeviceRequest,
    responses((status = 200, description = "Registered (upsert by id)", body = Device), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn register_device(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<RegisterDeviceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org = claims
        .org_id()
        .ok_or_else(|| AppError::BadRequest("Token has no organization".into()))?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let code = body.code.trim().to_ascii_uppercase();
    if !valid_code(&code) {
        return Err(AppError::BadRequest("Device code must be 1-6 letters or digits".into()));
    }
    if !matches!(body.kind.as_str(), "pos" | "kds" | "waiter") {
        return Err(AppError::BadRequest("kind must be pos, kds or waiter".into()));
    }
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, label, kind, platform, app_version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (id) DO UPDATE SET branch_id = EXCLUDED.branch_id, \
             platform = COALESCE(EXCLUDED.platform, devices.platform), \
             app_version = COALESCE(EXCLUDED.app_version, devices.app_version), \
             last_seen_at = now()",
    )
    .bind(body.id)
    .bind(org)
    .bind(body.branch_id)
    .bind(&code)
    .bind(clean_label(body.label.as_deref()))
    .bind(&body.kind)
    .bind(&body.platform)
    .bind(&body.app_version)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(fetch_device(pool.get_ref(), body.id).await?))
}

#[utoipa::path(get, path = "/devices", tag = "devices", params(ListDevicesQuery),
    responses((status = 200, description = "Devices of a branch", body = Vec<Device>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_devices(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ListDevicesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    require_branch_access(pool.get_ref(), &claims, q.branch_id).await?;
    let rows = sqlx::query_as::<_, Device>(&format!(
        "{DEVICE_SELECT} WHERE d.branch_id = $1 ORDER BY d.retired_at NULLS FIRST, d.code"
    ))
    .bind(q.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(patch, path = "/devices/{id}", tag = "devices",
    params(("id" = Uuid, Path, description = "Device id")), request_body = UpdateDeviceRequest,
    responses((status = 200, description = "Updated", body = Device), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn update_device(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateDeviceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;
    let current = fetch_device(pool.get_ref(), *id).await?;
    let branch = body.branch_id.or(current.branch_id);
    if let Some(b) = branch {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }
    let code = match &body.code {
        Some(c) => {
            let c = c.trim().to_ascii_uppercase();
            if !valid_code(&c) {
                return Err(AppError::BadRequest("Device code must be 1-6 letters or digits".into()));
            }
            c
        }
        None => current.code.clone(),
    };
    if body.code.is_some() || body.branch_id.is_some() {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE branch_id = $1 AND code = $2 AND id <> $3 AND retired_at IS NULL)",
        )
        .bind(branch)
        .bind(&code)
        .bind(*id)
        .fetch_one(pool.get_ref())
        .await?;
        if taken {
            return Err(AppError::Refused {
                code: "DEVICE_CODE_TAKEN",
                reason: "Another device at this branch uses that code".into(),
            });
        }
    }
    let label = match &body.label {
        Some(l) => clean_label(l.as_deref()),
        None => current.label.clone(),
    };
    sqlx::query(
        "UPDATE devices SET code = $2, label = $3, branch_id = $4, \
            retired_at = CASE WHEN $5::bool IS NULL THEN retired_at WHEN $5 THEN COALESCE(retired_at, now()) ELSE NULL END \
         WHERE id = $1",
    )
    .bind(*id)
    .bind(&code)
    .bind(label)
    .bind(branch)
    .bind(body.retired)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(fetch_device(pool.get_ref(), *id).await?))
}
