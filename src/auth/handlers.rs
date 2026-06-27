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
    /// Org tax rate as a decimal (e.g. 0.14 = 14% VAT); 0.0 when no org. Mirrors
    /// /auth/me so the POS has it immediately after login.
    #[schema(example = 0.14)]
    pub tax_rate: f64,
    #[schema(example = "EGP")]
    pub currency_code: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: UserPublic,
    /// Org tax rate as a decimal (e.g. 0.14 = 14% VAT); 0.0 when the user has no
    /// org. Exposed so the POS can compute a tax-inclusive cart total client-side.
    #[schema(example = 0.14)]
    pub tax_rate: f64,
    /// Org currency code (e.g. "EGP").
    #[schema(example = "EGP")]
    pub currency_code: String,
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
    _req:   HttpRequest,
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

            // ORG-scoped lookup: teller names are unique per org, so resolve the
            // teller from (name, pin) within the branch's org FIRST. Branch
            // assignment is checked separately below — that lets us distinguish
            // "wrong name/pin (or wrong org)" → 401 from "valid teller, but no
            // access to THIS branch" → 403, instead of conflating both.
            let tellers = sqlx::query_as::<_, User>(
                r#"
                SELECT u.id, u.org_id, u.name, u.email, u.phone,
                       u.password_hash, u.pin_hash, u.role,
                       u.is_active, u.last_login_at,
                       u.created_at, u.updated_at, u.deleted_at
                FROM users u
                WHERE LOWER(u.name) = LOWER($1)
                  AND u.org_id      = $2
                  AND u.pin_hash    IS NOT NULL
                  AND u.role        IN ('teller', 'waiter', 'kitchen')
                  AND u.is_active   = TRUE
                  AND u.deleted_at  IS NULL
                "#,
            )
            .bind(name)
            .bind(branch_org_id)
            .fetch_all(pool.get_ref())
            .await?;

            let matched = tellers
                .into_iter()
                .find(|u| {
                    u.pin_hash
                        .as_deref()
                        .is_some_and(|h| bcrypt::verify(pin, h).unwrap_or(false))
                })
                // No teller in this org matches name+PIN (includes a real teller
                // from a DIFFERENT org) → generic invalid credentials.
                .ok_or_else(|| AppError::Unauthorized("Invalid credentials".into()))?;

            // D13: tellers are ORG-scoped, not branch-scoped. The teller was
            // resolved within the branch's own org above (that's the boundary),
            // so any active org teller may sign in at this branch's device — no
            // per-branch `user_branch_assignments` gate.

            // Layer 3: silently (re)derive the teller's OFFLINE PIN verifier
            // (argon2id, distinct from the bcrypt login hash) so the org's
            // offline-auth bundle can let them unlock offline later. Best-effort
            // — a hashing/store failure must never block a valid login.
            if let Ok(off_hash) = crate::auth::offline::hash_offline_pin(pin) {
                let _ = sqlx::query("UPDATE users SET offline_pin_hash = $1 WHERE id = $2")
                    .bind(&off_hash)
                    .bind(matched.id)
                    .execute(pool.get_ref())
                    .await;
            }

            matched
        }

        _ => return Err(AppError::BadRequest(
            "Provide either (email + password) or pin".into()
        )),
    };

    if !user.is_active {
        return Err(AppError::Unauthorized("Account is disabled".into()));
    }

    // Refuse to issue a token to a suspended / soft-deleted org. The middleware
    // also rejects live requests for such an org, but stopping it here means we
    // never hand out a fresh token in the first place. Super admins carry no
    // org_id and are unaffected.
    if let Some(org_id) = user.org_id
        && !crate::auth::org_status::org_is_allowed(pool.get_ref(), org_id).await?
    {
        return Err(AppError::OrgSuspended);
    }

    // Open-shift login rules (authoritative — the backend is the source of truth):
    //   • same teller, SAME branch as their open shift  → allow (resume; e.g.
    //     after a token expiry — don't lock them out of the shift they must close)
    //   • same teller, DIFFERENT branch                 → reject (no two live places)
    //   • DIFFERENT teller at a branch that already has open shifts → ALLOW: with
    //     multi-teller tills, several tellers operate concurrently at one branch,
    //     each on their own till/drawer. The one-open-per-till index (not login)
    //     prevents two people sharing one drawer.
    //
    // (1) This teller's own open shift must be at the branch they're signing into.
    let open_shift_branch: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM shifts WHERE teller_id = $1 AND status = 'open'"
    )
    .bind(user.id)
    .fetch_optional(pool.get_ref())
    .await?;
    if let Some(open_branch) = open_shift_branch
        && body.branch_id != Some(open_branch) {
        tracing::warn!(
            target: "auth.login.blocked_open_shift",
            user_id = %user.id, role = ?user.role,
            open_shift_branch = %open_branch, attempted_branch = ?body.branch_id,
            "login blocked: user has an open shift at a different branch"
        );
        return Err(AppError::Conflict(
            "You already have an open shift at another branch. Close it before signing in here.".into(),
        ));
    }

    // (2) [removed for multi-teller] A branch may now hold several tellers' open
    //     shifts at once — one per till — so signing in alongside another teller's
    //     live shift is allowed. The previous `X-Sufrix-Closing-Shifts` handover
    //     handshake is no longer needed (a closing shift simply replays its close).

    // Tellers, waiters AND kitchen users are device-bound (PIN) and branch-bound;
    // waiters/kitchen just never hold a shift. All get the short device TTL.
    let token_branch_id = if matches!(user.role, UserRole::Teller | UserRole::Waiter | UserRole::Kitchen) {
        body.branch_id
    } else {
        None
    };

    let hours = if matches!(user.role, UserRole::Teller | UserRole::Waiter | UserRole::Kitchen) { 12 } else { 24 };

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

    // For tellers/waiters, branch_id is the device branch (from body.branch_id).
    // For other roles, fall back to looking up the first assignment.
    let branch_id_for_response: Option<Uuid> = if matches!(user.role, UserRole::Teller | UserRole::Waiter | UserRole::Kitchen) {
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

    let (tax_rate, currency_code): (f64, String) = match user.org_id {
        Some(org_id) => sqlx::query_as(
            "SELECT COALESCE(tax_rate, 0)::float8, currency_code FROM organizations WHERE id = $1"
        )
        .bind(org_id)
        .fetch_optional(pool.get_ref())
        .await?
        .unwrap_or((0.0, "EGP".to_string())),
        None => (0.0, "EGP".to_string()),
    };

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id_for_response;

    Ok(HttpResponse::Ok().json(LoginResponse {
        token,
        user: user_public,
        tax_rate,
        currency_code,
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

    // Prefer the branch this token is actually bound to (tellers always carry
    // one). An arbitrary LIMIT-1 assignment would, for a teller assigned to more
    // than one branch, report a DIFFERENT branch than the token — the POS adopts
    // that branch as `user.branchId` and then calls branch-scoped endpoints with
    // it, tripping require_branch_access's teller token-branch binding (403),
    // while /auth/me itself still returns 200. Non-branch-bound roles (admins)
    // have no token branch, so they fall back to an assignment lookup.
    let branch_id: Option<Uuid> = match claims.branch_id() {
        Some(b) => Some(b),
        None => sqlx::query_scalar(
            "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 LIMIT 1"
        )
        .bind(user.id)
        .fetch_optional(pool.get_ref())
        .await?,
    };

    // Org-level config the POS needs for a tax-inclusive cart total.
    let (tax_rate, currency_code): (f64, String) = match user.org_id {
        Some(org_id) => sqlx::query_as(
            "SELECT COALESCE(tax_rate, 0)::float8, currency_code FROM organizations WHERE id = $1"
        )
        .bind(org_id)
        .fetch_optional(pool.get_ref())
        .await?
        .unwrap_or((0.0, "EGP".to_string())),
        None => (0.0, "EGP".to_string()),
    };

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id;

    Ok(HttpResponse::Ok().json(MeResponse { user: user_public, tax_rate, currency_code }))
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