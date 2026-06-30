//! Tills = physical cash drawers / registers. A till is the unit of cash
//! continuity and of shift concurrency (see `migrations/20260625000000_tills.sql`).
//! CRUD mirrors the branches handlers; tills are branch-scoped so access is gated
//! by `require_branch_access` + the `branches` permission resource (same admins
//! manage both). A device binds to a till and is reconfigurable; `is_default`
//! marks the catch-all till a shift opens against when no till_id is supplied.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    delivery::require_branch_access,
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Till {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub is_default: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTillsQuery {
    pub branch_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateTillRequest {
    pub branch_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateTillRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

const TILL_COLS: &str =
    "id, org_id, branch_id, name, is_default, is_active, created_at, updated_at";

async fn fetch_till(pool: &PgPool, id: Uuid) -> Result<Till, AppError> {
    sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLS} FROM tills WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Till not found".into()))
}

// ── GET /tills?branch_id ──────────────────────────────────────

#[utoipa::path(
    get,
    path = "/tills",
    tag = "tills",
    params(ListTillsQuery),
    responses(
        (status = 200, description = "Tills for the branch", body = Vec<Till>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_tills(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListTillsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let tills = sqlx::query_as::<_, Till>(&format!(
        "SELECT {TILL_COLS} FROM tills \
         WHERE branch_id = $1 AND deleted_at IS NULL \
         ORDER BY is_default DESC, lower(name)"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(tills))
}

// ── POST /tills ───────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/tills",
    tag = "tills",
    request_body = CreateTillRequest,
    responses(
        (status = 201, description = "Till created", body = Till),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_till(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateTillRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Till name is required".into()));
    }

    // Resolve the branch's org (and confirm it exists / is live).
    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let is_default = body.is_default.unwrap_or(false);

    let mut tx = pool.get_ref().begin().await?;
    if is_default {
        // Only one default till per branch — demote any existing default first.
        sqlx::query(
            "UPDATE tills SET is_default = false, updated_at = now() \
             WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
        )
        .bind(body.branch_id)
        .execute(&mut *tx)
        .await?;
    }

    let till = sqlx::query_as::<_, Till>(&format!(
        "INSERT INTO tills (org_id, branch_id, name, is_default, is_active) \
         VALUES ($1, $2, $3, $4, $5) RETURNING {TILL_COLS}"
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(name)
    .bind(is_default)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(HttpResponse::Created().json(till))
}

// ── PATCH /tills/{id} ─────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/tills/{id}",
    tag = "tills",
    params(("id" = Uuid, Path, description = "Till ID")),
    request_body = UpdateTillRequest,
    responses(
        (status = 200, description = "Till updated", body = Till),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn update_till(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateTillRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;

    let existing = fetch_till(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, existing.branch_id).await?;

    let new_name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.name.as_deref().is_some_and(|s| s.trim().is_empty()) {
        return Err(AppError::BadRequest("Till name cannot be empty".into()));
    }

    let mut tx = pool.get_ref().begin().await?;
    if body.is_default == Some(true) {
        sqlx::query(
            "UPDATE tills SET is_default = false, updated_at = now() \
             WHERE branch_id = $1 AND is_default AND deleted_at IS NULL AND id <> $2",
        )
        .bind(existing.branch_id)
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    }

    let till = sqlx::query_as::<_, Till>(&format!(
        "UPDATE tills SET \
             name       = COALESCE($2, name), \
             is_default = COALESCE($3, is_default), \
             is_active  = COALESCE($4, is_active), \
             updated_at = now() \
         WHERE id = $1 AND deleted_at IS NULL \
         RETURNING {TILL_COLS}"
    ))
    .bind(*id)
    .bind(new_name)
    .bind(body.is_default)
    .bind(body.is_active)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(HttpResponse::Ok().json(till))
}

// ── DELETE /tills/{id} ────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/tills/{id}",
    tag = "tills",
    params(("id" = Uuid, Path, description = "Till ID")),
    responses(
        (status = 204, description = "Till deleted"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_till(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "delete").await?;

    let existing = fetch_till(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, existing.branch_id).await?;

    // A till backing an open drawer can't be retired — its shift must close first.
    let has_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE till_id = $1 AND status = 'open')",
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    if has_open {
        return Err(AppError::Conflict(
            "Cannot delete a till with an open shift — close it first.".into(),
        ));
    }

    sqlx::query("UPDATE tills SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}
