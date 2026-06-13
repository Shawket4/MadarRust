use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::jwt::{create_token, Claims, JwtSecret},
    errors::{AppError, AppErrorResponse},
    models::{User, UserPublic, UserRole},
};

// ── Request / Response types ─────────────────────────────────

/// Login is dual-mode:
///
/// - **Email + password** (admins, managers, super-admins): supply
///   `email` and `password`. `org_id` is optional — if provided, the
///   user must belong to that org; if omitted, lookup is by email only.
/// - **PIN + name** (tellers): supply `name`, `pin`, and **`branch_id`**
///   (required). The teller must be assigned to that branch. `org_id` is
///   derived server-side from the branch — never trusted from the client.
#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub org_id:    Option<Uuid>,
    #[schema(format = Email, example = "ahmed@therue.cafe")]
    pub email:     Option<String>,
    pub password:  Option<String>,
    #[schema(pattern = "^[0-9]{4,6}$", min_length = 4, max_length = 6, example = "1234")]
    pub pin:       Option<String>,
    /// Teller's display name (required for PIN login, unused otherwise).
    #[schema(example = "Mariam")]
    pub name:      Option<String>,
    /// Required for PIN login. The org is derived from this branch server-side.
    pub branch_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct ResolveBranchRequest {
    /// Organization to search within.
    pub org_id:    Uuid,
    /// Device GPS latitude (WGS-84).
    pub latitude:  f64,
    /// Device GPS longitude (WGS-84).
    pub longitude: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ResolveBranchResponse {
    pub branch_id:       Uuid,
    pub branch_name:     String,
    /// Straight-line distance from the supplied coordinates to the branch, in metres.
    pub distance_meters: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct LoginResponse {
    /// JWT to send as `Authorization: Bearer <token>` on subsequent requests.
    pub token: String,
    pub user:  UserPublic,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: UserPublic,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UserPermissionItem {
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "read")]
    pub action:   String,
    pub granted:  bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct AuthPermissionsResponse {
    pub permissions: Vec<UserPermissionItem>,
}

#[derive(sqlx::FromRow)]
struct DbPermission {
    pub resource: String,
    pub action:   String,
    pub granted:  bool,
}

// ── POST /auth/login ─────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Authentication succeeded; JWT issued", body = LoginResponse),
        AppErrorResponse,
    )
)]
pub async fn login(
    pool:   web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body:   web::Json<LoginRequest>,
) -> Result<HttpResponse, AppError> {

    let user: User = match (&body.email, &body.pin) {

        (Some(email), None) => {
            let password = body.password.as_deref().ok_or_else(|| {
                AppError::BadRequest("password is required for email login".into())
            })?;

            let u = sqlx::query_as::<_, User>(
                r#"
                SELECT id, org_id, name, email, phone,
                       password_hash, pin_hash, role,
                       is_active, last_login_at,
                       created_at, updated_at, deleted_at
                FROM users
                WHERE email = $1
                  AND ($2::uuid IS NULL OR org_id = $2)
                  AND deleted_at IS NULL
                "#,
            )
            .bind(email)
            .bind(body.org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::Unauthorized("Invalid credentials".into()))?;

            let hash = u.password_hash.as_deref().ok_or_else(|| {
                AppError::Unauthorized("No password set for this account".into())
            })?;
            if !bcrypt::verify(password, hash).unwrap_or(false) {
                return Err(AppError::Unauthorized("Invalid credentials".into()));
            }
            u
        }

        (None, Some(pin)) => {
            let name = body.name.as_deref().ok_or_else(|| {
                AppError::BadRequest("name is required for PIN login".into())
            })?;

            let branch_id = body.branch_id.ok_or_else(|| {
                AppError::BadRequest("branch_id is required for PIN login".into())
            })?;

            // Derive org_id from the branch — never trust the client to supply it
            let branch_org_id: Uuid = sqlx::query_scalar(
                "SELECT org_id FROM branches WHERE id = $1 AND is_active = TRUE AND deleted_at IS NULL"
            )
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::Unauthorized("Invalid branch".into()))?;

            // Branch-scoped lookup: teller must be assigned to this branch
            let tellers = sqlx::query_as::<_, User>(
                r#"
                SELECT u.id, u.org_id, u.name, u.email, u.phone,
                       u.password_hash, u.pin_hash, u.role,
                       u.is_active, u.last_login_at,
                       u.created_at, u.updated_at, u.deleted_at
                FROM users u
                JOIN user_branch_assignments uba ON uba.user_id = u.id AND uba.branch_id = $1
                WHERE LOWER(u.name) = LOWER($2)
                  AND u.org_id      = $3
                  AND u.pin_hash    IS NOT NULL
                  AND u.role        = 'teller'
                  AND u.is_active   = TRUE
                  AND u.deleted_at  IS NULL
                "#,
            )
            .bind(branch_id)
            .bind(name)
            .bind(branch_org_id)
            .fetch_all(pool.get_ref())
            .await?;

            tellers
                .into_iter()
                .find(|u| {
                    u.pin_hash
                        .as_deref()
                        .is_some_and(|h| bcrypt::verify(pin, h).unwrap_or(false))
                })
                .ok_or_else(|| AppError::Unauthorized("Invalid PIN".into()))?
        }

        _ => return Err(AppError::BadRequest(
            "Provide either (email + password) or pin".into()
        )),
    };

    if !user.is_active {
        return Err(AppError::Unauthorized("Account is disabled".into()));
    }

    // A user with an OPEN shift may NOT start a new login session — anywhere.
    // The open shift must be closed first. This stops a teller (assigned to
    // several branches) being live at two places at once, and blocks any
    // duplicate session. The device that opened the shift keeps working on its
    // persisted token (no re-login needed mid-shift). Surfaced as an error state.
    let open_shift_branch: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM shifts WHERE teller_id = $1 AND status = 'open'"
    )
    .bind(user.id)
    .fetch_optional(pool.get_ref())
    .await?;
    if let Some(open_branch) = open_shift_branch {
        tracing::warn!(
            target: "auth.login.blocked_open_shift",
            user_id = %user.id,
            role = ?user.role,
            open_shift_branch = %open_branch,
            attempted_branch = ?body.branch_id,
            "login blocked: user already has an open shift"
        );
        return Err(AppError::Conflict(
            "You already have an open shift. It must be closed before signing in again.".into(),
        ));
    }

    let token_branch_id = if user.role == UserRole::Teller {
        body.branch_id
    } else {
        None
    };

    let hours = if user.role == UserRole::Teller { 12 } else { 24 };

    let token = create_token(
        &secret,
        user.id,
        user.org_id,
        user.role.clone(),
        token_branch_id,
        hours,
    )
    .map_err(|_| AppError::Internal)?;

    sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(pool.get_ref())
        .await?;

    // For tellers, branch_id is already validated above (from body.branch_id).
    // For other roles, fall back to looking up the first assignment.
    let branch_id_for_response: Option<Uuid> = if user.role == UserRole::Teller {
        body.branch_id
    } else {
        sqlx::query_scalar(
            "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 LIMIT 1"
        )
        .bind(user.id)
        .fetch_optional(pool.get_ref())
        .await?
        .flatten()
    };

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id_for_response;

    Ok(HttpResponse::Ok().json(LoginResponse {
        token,
        user: user_public,
    }))
}

// ── GET /auth/me ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "Current authenticated user", body = MeResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn me(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;

    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT id, org_id, name, email, phone,
               password_hash, pin_hash, role,
               is_active, last_login_at,
               created_at, updated_at, deleted_at
        FROM users
        WHERE id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(claims.user_id())
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let branch_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 LIMIT 1"
    )
    .bind(user.id)
    .fetch_optional(pool.get_ref())
    .await?;

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id;

    Ok(HttpResponse::Ok().json(MeResponse { user: user_public }))
}

// ── POST /auth/resolve-branch ────────────────────────────────

#[utoipa::path(
    post,
    path = "/auth/resolve-branch",
    tag = "auth",
    request_body = ResolveBranchRequest,
    responses(
        (status = 200, description = "Nearest branch within its geofence radius", body = ResolveBranchResponse),
        AppErrorResponse,
    )
)]
pub async fn resolve_branch(
    pool: web::Data<PgPool>,
    body: web::Json<ResolveBranchRequest>,
) -> Result<HttpResponse, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id:              Uuid,
        name:            String,
        distance_meters: f64,
    }

    let row: Option<Row> = sqlx::query_as(
        r#"
        SELECT b.id, b.name,
            (6371000.0 * ACOS(LEAST(1.0,
                SIN(RADIANS($2)) * SIN(RADIANS(b.latitude))
              + COS(RADIANS($2)) * COS(RADIANS(b.latitude))
              * COS(RADIANS(b.longitude - $3))
            ))) AS distance_meters
        FROM branches b
        WHERE b.org_id     = $1
          AND b.is_active  = TRUE
          AND b.deleted_at IS NULL
          AND b.latitude   IS NOT NULL
          AND b.longitude  IS NOT NULL
          AND (6371000.0 * ACOS(LEAST(1.0,
                SIN(RADIANS($2)) * SIN(RADIANS(b.latitude))
              + COS(RADIANS($2)) * COS(RADIANS(b.latitude))
              * COS(RADIANS(b.longitude - $3))
              ))) <= COALESCE(b.geo_radius_meters, 200)
        ORDER BY distance_meters ASC
        LIMIT 1
        "#,
    )
    .bind(body.org_id)
    .bind(body.latitude)
    .bind(body.longitude)
    .fetch_optional(pool.get_ref())
    .await?;

    match row {
        Some(r) => Ok(HttpResponse::Ok().json(ResolveBranchResponse {
            branch_id:       r.id,
            branch_name:     r.name,
            distance_meters: r.distance_meters,
        })),
        None => Err(AppError::NotFound("No branch found within range".into())),
    }
}

// ── GET /auth/permissions ────────────────────────────────────

#[utoipa::path(
    get,
    path = "/auth/permissions",
    tag = "auth",
    // operation_id overrides the default `permissions` (the function name)
    // to avoid collision with the permissions module's handlers and to
    // give generated clients a clearer method name (`getMyPermissions`).
    operation_id = "get_my_permissions",
    responses(
        (status = 200, description = "Effective permission grants for the authenticated user", body = AuthPermissionsResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn permissions(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;

    let role: String = sqlx::query_scalar(
        "SELECT role::text FROM users WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(claims.user_id())
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let role_defaults = sqlx::query_as::<_, DbPermission>(
        "SELECT resource::text as resource, action::text as action, granted
         FROM role_permissions WHERE role = $1::user_role",
    )
    .bind(&role)
    .fetch_all(pool.get_ref())
    .await?;

    let user_overrides = sqlx::query_as::<_, DbPermission>(
        "SELECT resource::text as resource, action::text as action, granted
         FROM permissions WHERE user_id = $1",
    )
    .bind(claims.user_id())
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

    let mut permissions = Vec::with_capacity(resources.len() * actions.len());

    for resource in resources {
        for action in actions {
            let role_default  = role_map.get(&(resource, action)).copied();
            let user_override = override_map.get(&(resource, action)).copied();

            let effective = if role == "super_admin" {
                true
            } else {
                user_override.or(role_default).unwrap_or(false)
            };

            permissions.push(UserPermissionItem {
                resource: resource.to_string(),
                action:   action.to_string(),
                granted:  effective,
            });
        }
    }

    Ok(HttpResponse::Ok().json(AuthPermissionsResponse { permissions }))
}