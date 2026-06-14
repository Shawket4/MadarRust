use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::{guards::{require_super_admin, require_same_org}, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct Permission {
    pub id:       Uuid,
    pub user_id:  Uuid,
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "update")]
    pub action:   String,
    pub granted:  bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct RolePermission {
    #[schema(example = "branch_manager")]
    pub role:     String,
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "update")]
    pub action:   String,
    pub granted:  bool,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpsertPermissionRequest {
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "update")]
    pub action:   String,
    pub granted:  bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpsertRolePermissionRequest {
    #[schema(example = "branch_manager")]
    pub role:     String,
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "update")]
    pub action:   String,
    pub granted:  bool,
}

/// One cell of the resolved permission matrix for a user.
/// `effective` = `user_override` if present, else `role_default`, else false.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PermissionMatrix {
    pub resource:      String,
    pub action:        String,
    pub role_default:  Option<bool>,
    pub user_override: Option<bool>,
    pub effective:     bool,
}

// ── GET /permissions/user/:user_id ────────────────────────────

#[utoipa::path(
    get,
    path = "/permissions/user/{user_id}",
    tag = "permissions",
    params(("user_id" = Uuid, Path, description = "User ID")),
    responses(
        (status = 200, description = "Per-user permission overrides", body = Vec<Permission>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

#[utoipa::path(
    get,
    path = "/permissions/matrix/{user_id}",
    tag = "permissions",
    params(("user_id" = Uuid, Path, description = "User ID")),
    responses(
        (status = 200, description = "Fully resolved permission matrix for the user", body = Vec<PermissionMatrix>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

    let resources = crate::permissions::RESOURCES;
    let actions   = crate::permissions::ACTIONS;

    // Build O(1) lookup maps so the nested loop is O(n) not O(n²)
    let role_map: HashMap<(&str, &str), bool> = role_defaults
        .iter()
        .map(|r| ((r.resource.as_str(), r.action.as_str()), r.granted))
        .collect();
    let override_map: HashMap<(&str, &str), bool> = user_overrides
        .iter()
        .map(|p| ((p.resource.as_str(), p.action.as_str()), p.granted))
        .collect();

    let mut matrix: Vec<PermissionMatrix> = Vec::with_capacity(resources.len() * actions.len());

    for resource in resources {
        for action in actions {
            let role_default  = role_map.get(&(resource, action)).copied();
            let user_override = override_map.get(&(resource, action)).copied();
            let effective     = user_override.or(role_default).unwrap_or(false);

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

#[utoipa::path(
    put,
    path = "/permissions/user/{user_id}",
    tag = "permissions",
    params(("user_id" = Uuid, Path, description = "User ID")),
    request_body = UpsertPermissionRequest,
    responses(
        (status = 200, description = "Permission upserted", body = Permission),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

    crate::cache::invalidate_user_override(*user_id, &body.resource, &body.action).await;

    Ok(HttpResponse::Ok().json(perm))
}

// ── DELETE /permissions/user/:user_id/:resource/:action ──────

#[utoipa::path(
    delete,
    path = "/permissions/user/{user_id}/{resource}/{action}",
    tag = "permissions",
    params(
        ("user_id"  = Uuid,   Path, description = "User ID"),
        ("resource" = String, Path, description = "Resource name (e.g. menu_items, orders)", example = "menu_items"),
        ("action"   = String, Path, description = "Action (create | read | update | delete)", example = "update"),
    ),
    responses(
        (status = 204, description = "Permission override removed"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

    crate::cache::invalidate_user_override(user_id, &resource, &action).await;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /permissions/roles ────────────────────────────────────

#[utoipa::path(
    get,
    path = "/permissions/roles",
    tag = "permissions",
    responses(
        (status = 200, description = "All role permission defaults", body = Vec<RolePermission>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

#[utoipa::path(
    put,
    path = "/permissions/roles",
    tag = "permissions",
    request_body = UpsertRolePermissionRequest,
    responses(
        (status = 200, description = "Role permission default upserted", body = RolePermission),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
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

    crate::cache::invalidate_role_default(&body.role, &body.resource, &body.action).await;

    Ok(HttpResponse::Ok().json(perm))
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

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