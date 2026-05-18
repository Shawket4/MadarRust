use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::{require_super_admin, require_same_org}, jwt::Claims},
    errors::AppError,
    models::UserRole,
    permissions::checker::check_permission,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Permission {
    pub id:       Uuid,
    pub user_id:  Uuid,
    pub resource: String,
    pub action:   String,
    pub granted:  bool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RolePermission {
    pub role:     String,
    pub resource: String,
    pub action:   String,
    pub granted:  bool,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UpsertPermissionRequest {
    pub resource: String,
    pub action:   String,
    pub granted:  bool,
}

#[derive(Deserialize)]
pub struct UpsertRolePermissionRequest {
    pub role:     String,
    pub resource: String,
    pub action:   String,
    pub granted:  bool,
}

// ── GET /permissions/user/:user_id ────────────────────────────

pub async fn get_user_permissions(
    req:     HttpRequest,
    pool:    web::Data<PgPool>,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "permissions", "read").await?;
    require_same_org_as_target(pool.get_ref(), &claims, *user_id).await?;

    let perms = sqlx::query_as::<_, Permission>(
        "SELECT id, user_id, resource::text, action::text, granted
         FROM permissions WHERE user_id = $1 ORDER BY resource, action",
    )
    .bind(*user_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(perms))
}

// ── GET /permissions/matrix/:user_id ─────────────────────────

#[derive(Serialize)]
pub struct PermissionMatrix {
    pub resource:      String,
    pub action:        String,
    pub role_default:  Option<bool>,
    pub user_override: Option<bool>,
    pub effective:     bool,
}

pub async fn get_permission_matrix(
    req:     HttpRequest,
    pool:    web::Data<PgPool>,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "permissions", "read").await?;
    require_same_org_as_target(pool.get_ref(), &claims, *user_id).await?;

    let role: String = sqlx::query_scalar(
        "SELECT role::text FROM users WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let role_defaults = sqlx::query_as::<_, RolePermission>(
        "SELECT role::text, resource::text, action::text, granted
         FROM role_permissions WHERE role = $1::user_role",
    )
    .bind(&role)
    .fetch_all(pool.get_ref())
    .await?;

    let user_overrides = sqlx::query_as::<_, Permission>(
        "SELECT id, user_id, resource::text, action::text, granted
         FROM permissions WHERE user_id = $1",
    )
    .bind(*user_id)
    .fetch_all(pool.get_ref())
    .await?;

    let resources = [
        "orgs", "branches", "users", "categories",
        "menu_items", "addon_groups", "shifts",
        "orders", "order_items", "payments", "permissions",
        "addon_items", "inventory", "inventory_adjustments", "inventory_transfers",
        "recipes", "soft_serve_batches", "shift_counts",
    ];
    let actions = ["create", "read", "update", "delete"];

    let mut matrix: Vec<PermissionMatrix> = Vec::new();

    for resource in resources {
        for action in actions {
            let role_default = role_defaults.iter()
                .find(|r| r.resource == resource && r.action == action)
                .map(|r| r.granted);

            let user_override = user_overrides.iter()
                .find(|p| p.resource == resource && p.action == action)
                .map(|p| p.granted);

            let effective = user_override.or(role_default).unwrap_or(false);

            matrix.push(PermissionMatrix {
                resource: resource.to_string(),
                action:   action.to_string(),
                role_default,
                user_override,
                effective,
            });
        }
    }

    Ok(HttpResponse::Ok().json(matrix))
}

// ── PUT /permissions/user/:user_id ────────────────────────────

pub async fn upsert_user_permission(
    req:     HttpRequest,
    pool:    web::Data<PgPool>,
    user_id: web::Path<Uuid>,
    body:    web::Json<UpsertPermissionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "permissions", "update").await?;
    require_same_org_as_target(pool.get_ref(), &claims, *user_id).await?;

    let perm = sqlx::query_as::<_, Permission>(
        r#"
        INSERT INTO permissions (user_id, resource, action, granted)
        VALUES ($1, $2::permission_resource, $3::permission_action, $4)
        ON CONFLICT (user_id, resource, action)
        DO UPDATE SET granted = EXCLUDED.granted
        RETURNING id, user_id, resource::text, action::text, granted
        "#,
    )
    .bind(*user_id)
    .bind(&body.resource)
    .bind(&body.action)
    .bind(body.granted)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(perm))
}

// ── DELETE /permissions/user/:user_id/resource/:resource/action/:action

pub async fn delete_user_permission(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, String, String)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "permissions", "delete").await?;
    let (user_id, resource, action) = path.into_inner();
    require_same_org_as_target(pool.get_ref(), &claims, user_id).await?;

    sqlx::query(
        "DELETE FROM permissions WHERE user_id = $1
         AND resource = $2::permission_resource
         AND action   = $3::permission_action",
    )
    .bind(user_id)
    .bind(&resource)
    .bind(&action)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /permissions/roles ────────────────────────────────────

pub async fn get_role_permissions(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "permissions", "read").await?;

    let perms = sqlx::query_as::<_, RolePermission>(
        "SELECT role::text, resource::text, action::text, granted
         FROM role_permissions ORDER BY role, resource, action",
    )
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(perms))
}

// ── PUT /permissions/roles  (super_admin only) ────────────────

pub async fn upsert_role_permission(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<UpsertRolePermissionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_super_admin(&claims)?;

    let perm = sqlx::query_as::<_, RolePermission>(
        r#"
        INSERT INTO role_permissions (role, resource, action, granted)
        VALUES ($1::user_role, $2::permission_resource, $3::permission_action, $4)
        ON CONFLICT (role, resource, action)
        DO UPDATE SET granted = EXCLUDED.granted
        RETURNING role::text, resource::text, action::text, granted
        "#,
    )
    .bind(&body.role)
    .bind(&body.resource)
    .bind(&body.action)
    .bind(body.granted)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(perm))
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Ensure the target user belongs to the same org as the caller.
/// super_admin bypasses this check entirely.
async fn require_same_org_as_target(
    pool:    &PgPool,
    claims:  &Claims,
    user_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let target_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM users WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    require_same_org(claims, target_org)
}