use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::{
        guards::{require_org_admin, require_same_org, require_super_admin},
        jwt::Claims,
    },
    errors::{AppError, AppErrorResponse},
    models::{User, UserPublic, UserRole},
    permissions::checker::check_permission,
};

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateUserRequest {
    pub org_id: Uuid,
    #[schema(example = "Ahmed Hassan")]
    pub name: String,
    /// Required for admins and managers; ignored for tellers.
    #[schema(format = Email, example = "ahmed@therue.cafe")]
    pub email: Option<String>,
    #[schema(example = "+201234567890")]
    pub phone: Option<String>,
    pub role: UserRole,
    /// Required when `role` is anything other than `teller`. Plain text;
    /// hashed server-side with bcrypt before storage.
    pub password: Option<String>,
    /// Required when `role = teller`. 4–6 ASCII digits.
    #[schema(
        pattern = "^[0-9]{4,6}$",
        min_length = 4,
        max_length = 6,
        example = "1234"
    )]
    pub pin: Option<String>,
    /// Branches to assign the new user to immediately. Branch managers
    /// can only assign to branches they themselves are assigned to.
    pub branch_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize, ToSchema)]
pub struct AssignBranchRequest {
    pub branch_id: Uuid,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct CreateUserResponse {
    pub user: UserPublic,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListUsersQuery {
    /// Filter to a specific organization. Optional for super-admins
    /// (who see all orgs when omitted); required-by-policy for everyone
    /// else (overridden server-side to the caller's own org).
    pub org_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateUserRequest {
    pub name: Option<String>,
    #[schema(format = Email)]
    pub email: Option<String>,
    pub phone: Option<String>,
    /// Plain-text new password. Server-side bcrypt-hashed.
    pub password: Option<String>,
    #[schema(pattern = "^[0-9]{4,6}$", min_length = 4, max_length = 6)]
    pub pin: Option<String>,
    /// Only org-admins and above can change roles. Promoting to
    /// `super_admin` requires the caller to be a super-admin.
    pub role: Option<UserRole>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct UserBranch {
    pub branch_id: Uuid,
    #[schema(example = "Zamalek")]
    pub branch_name: String,
}

// ── POST /users  ──────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/users",
    tag = "users",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "User created", body = CreateUserResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_user(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateUserRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;

    check_permission(pool.get_ref(), &claims, "users", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    if claims.role == UserRole::BranchManager {
        if !matches!(
            body.role,
            UserRole::Teller | UserRole::Waiter | UserRole::Kitchen
        ) {
            return Err(AppError::Forbidden(
                "Branch managers can only create teller, waiter and kitchen accounts".into(),
            ));
        }

        if let Some(branch_ids) = &body.branch_ids {
            for bid in branch_ids {
                let is_assigned: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
                )
                .bind(claims.user_id())
                .bind(bid)
                .fetch_one(pool.get_ref())
                .await?;
                if !is_assigned {
                    return Err(AppError::Forbidden(format!(
                        "You cannot assign a user to branch {} because it is not assigned to you",
                        bid
                    )));
                }
            }
        }
    }

    if claims.role == UserRole::OrgAdmin && body.role == UserRole::SuperAdmin {
        return Err(AppError::Forbidden(
            "Only super admins can create super admin accounts".into(),
        ));
    }

    match body.role {
        UserRole::Teller | UserRole::Waiter | UserRole::Kitchen => {
            if body.pin.is_none() {
                return Err(AppError::BadRequest(
                    "Tellers, waiters and kitchen users require a PIN".into(),
                ));
            }
            let pin = body.pin.as_deref().unwrap();
            if pin.len() < 4 || pin.len() > 6 || !pin.chars().all(|c| c.is_ascii_digit()) {
                return Err(AppError::BadRequest("PIN must be 4–6 digits".into()));
            }
        }
        _ => {
            if body.password.is_none() {
                return Err(AppError::BadRequest(
                    "Admins and managers require a password".into(),
                ));
            }
            if body.email.is_none() {
                return Err(AppError::BadRequest(
                    "Admins and managers require an email".into(),
                ));
            }
        }
    }

    if let Some(email) = &body.email {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users WHERE email = $1 AND deleted_at IS NULL)",
        )
        .bind(email)
        .fetch_one(pool.get_ref())
        .await?;

        if exists {
            return Err(AppError::Conflict("Email already in use".into()));
        }
    }

    if matches!(
        body.role,
        UserRole::Teller | UserRole::Waiter | UserRole::Kitchen
    ) {
        // PIN login matches by name across the teller+waiter+kitchen namespace, so
        // names must be unique within it (not just among tellers).
        let name_taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users
             WHERE org_id = $1 AND LOWER(name) = LOWER($2)
               AND role IN ('teller', 'waiter', 'kitchen') AND deleted_at IS NULL)",
        )
        .bind(body.org_id)
        .bind(&body.name)
        .fetch_one(pool.get_ref())
        .await?;

        if name_taken {
            return Err(AppError::Conflict(
                "A teller, waiter or kitchen user with this name already exists in this organization".into(),
            ));
        }
    }

    let password_hash = body
        .password
        .as_deref()
        .map(|p| bcrypt::hash(p, bcrypt::DEFAULT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let pin_hash = body
        .pin
        .as_deref()
        .map(|p| bcrypt::hash(p, bcrypt::DEFAULT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (org_id, name, email, phone, role, password_hash, pin_hash)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, org_id, name, email, phone,
                  password_hash, pin_hash, role,
                  is_active, last_login_at,
                  created_at, updated_at, deleted_at
        "#,
    )
    .bind(body.org_id)
    .bind(&body.name)
    .bind(&body.email)
    .bind(&body.phone)
    .bind(&body.role)
    .bind(password_hash)
    .bind(pin_hash)
    .fetch_one(pool.get_ref())
    .await?;

    if let Some(branch_ids) = &body.branch_ids {
        for bid in branch_ids {
            sqlx::query(
                r#"
                INSERT INTO user_branch_assignments (user_id, branch_id, assigned_by)
                VALUES ($1, $2, $3)
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(user.id)
            .bind(bid)
            .bind(claims.user_id())
            .execute(pool.get_ref())
            .await?;
        }
    }

    Ok(HttpResponse::Created().json(CreateUserResponse { user: user.into() }))
}

// ── GET /users?org_id=  ───────────────────────────────────────

#[utoipa::path(
    get,
    path = "/users",
    tag = "users",
    params(ListUsersQuery),
    responses(
        (status = 200, description = "Users visible to the caller", body = Vec<UserPublic>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_users(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListUsersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "read").await?;

    let org_id = if claims.role == UserRole::SuperAdmin {
        query.org_id
    } else {
        let own = claims
            .org_id()
            .ok_or_else(|| AppError::Forbidden("No org assigned".into()))?;
        Some(own)
    };

    let users = match org_id {
        Some(oid) => {
            if claims.role == UserRole::BranchManager {
                sqlx::query_as::<_, User>(
                    r#"
                    SELECT DISTINCT u.id, u.org_id, u.name, u.email, u.phone,
                           u.password_hash, u.pin_hash, u.role,
                           u.is_active, u.last_login_at,
                           u.created_at, u.updated_at, u.deleted_at
                    FROM users u
                    LEFT JOIN user_branch_assignments uba ON uba.user_id = u.id
                    WHERE u.org_id = $1
                      AND u.deleted_at IS NULL
                      AND (
                          u.id = $2
                          OR uba.branch_id IN (
                              SELECT branch_id FROM user_branch_assignments WHERE user_id = $2
                          )
                      )
                    ORDER BY u.name
                    "#,
                )
                .bind(oid)
                .bind(claims.user_id())
                .fetch_all(pool.get_ref())
                .await?
            } else {
                sqlx::query_as::<_, User>(
                    r#"
                    SELECT id, org_id, name, email, phone,
                           password_hash, pin_hash, role,
                           is_active, last_login_at,
                           created_at, updated_at, deleted_at
                    FROM users
                    WHERE org_id = $1 AND deleted_at IS NULL
                    ORDER BY name
                    "#,
                )
                .bind(oid)
                .fetch_all(pool.get_ref())
                .await?
            }
        }

        None => {
            sqlx::query_as::<_, User>(
                r#"
            SELECT id, org_id, name, email, phone,
                   password_hash, pin_hash, role,
                   is_active, last_login_at,
                   created_at, updated_at, deleted_at
            FROM users
            WHERE deleted_at IS NULL
            ORDER BY name
            "#,
            )
            .fetch_all(pool.get_ref())
            .await?
        }
    };

    let public: Vec<UserPublic> = users.into_iter().map(Into::into).collect();
    Ok(HttpResponse::Ok().json(public))
}

// ── GET /users/:id  ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/users/{id}",
    tag = "users",
    params(("id" = Uuid, Path, description = "User ID")),
    responses(
        (status = 200, description = "The requested user", body = UserPublic),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_user(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "read").await?;

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
    .bind(*user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    require_same_org(&claims, user.org_id)?;

    if claims.role == UserRole::BranchManager && claims.user_id() != *user_id {
        let same_branch: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM user_branch_assignments uba1
                JOIN user_branch_assignments uba2 ON uba2.branch_id = uba1.branch_id
                WHERE uba1.user_id = $1 AND uba2.user_id = $2
            )
            "#,
        )
        .bind(*user_id)
        .bind(claims.user_id())
        .fetch_one(pool.get_ref())
        .await?;

        if !same_branch {
            return Err(AppError::Forbidden(
                "You do not have access to this user".into(),
            ));
        }
    }

    Ok(HttpResponse::Ok().json(UserPublic::from(user)))
}

// ── PATCH /users/:id  ─────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/users/{id}",
    tag = "users",
    params(("id" = Uuid, Path, description = "User ID")),
    request_body = UpdateUserRequest,
    responses(
        (status = 200, description = "User updated", body = UserPublic),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn update_user(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    user_id: web::Path<Uuid>,
    body: web::Json<UpdateUserRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "update").await?;

    let existing = sqlx::query_as::<_, User>(
        "SELECT id, org_id, name, email, phone, password_hash, pin_hash, role,
                is_active, last_login_at, created_at, updated_at, deleted_at
         FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    require_same_org(&claims, existing.org_id)?;

    // Vertical-privilege guard (V4): a caller may only reset credentials / toggle
    // status / change the role of a STRICTLY lower-privileged user. This stops a
    // branch_manager from taking over an org_admin (even on a shared branch), or
    // an org_admin from resetting a super_admin's credentials.
    let rank = |r: &UserRole| match r {
        UserRole::SuperAdmin => 3u8,
        UserRole::OrgAdmin => 2,
        UserRole::BranchManager => 1,
        UserRole::Teller => 0,
        UserRole::Waiter => 0,
        UserRole::Kitchen => 0,
    };
    let sensitive = body.password.is_some()
        || body.pin.is_some()
        || body.is_active.is_some()
        || body.role.is_some();
    if sensitive && *user_id != claims.user_id() && rank(&existing.role) > rank(&claims.role) {
        return Err(AppError::Forbidden(
            "You cannot modify a user with higher privileges".into(),
        ));
    }

    if claims.role == UserRole::BranchManager && claims.user_id() != *user_id {
        let same_branch: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM user_branch_assignments uba1
                JOIN user_branch_assignments uba2 ON uba2.branch_id = uba1.branch_id
                WHERE uba1.user_id = $1 AND uba2.user_id = $2
            )
            "#,
        )
        .bind(*user_id)
        .bind(claims.user_id())
        .fetch_one(pool.get_ref())
        .await?;

        if !same_branch {
            return Err(AppError::Forbidden(
                "You do not have access to this user".into(),
            ));
        }
    }

    if body.role.is_some() {
        require_org_admin(&claims)?;
    }

    if body.role == Some(UserRole::SuperAdmin) {
        require_super_admin(&claims)?;
    }

    let password_hash = body
        .password
        .as_deref()
        .map(|p| bcrypt::hash(p, bcrypt::DEFAULT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let pin_hash = body
        .pin
        .as_deref()
        .map(|p| bcrypt::hash(p, bcrypt::DEFAULT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let user = sqlx::query_as::<_, User>(
        r#"
        UPDATE users SET
            name          = COALESCE($2, name),
            email         = COALESCE($3, email),
            phone         = COALESCE($4, phone),
            role          = COALESCE($5, role),
            is_active     = COALESCE($6, is_active),
            password_hash = COALESCE($7, password_hash),
            pin_hash      = COALESCE($8, pin_hash),
            updated_at    = NOW()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, org_id, name, email, phone,
                  password_hash, pin_hash, role,
                  is_active, last_login_at,
                  created_at, updated_at, deleted_at
        "#,
    )
    .bind(*user_id)
    .bind(&body.name)
    .bind(&body.email)
    .bind(&body.phone)
    .bind(&body.role)
    .bind(body.is_active)
    .bind(password_hash)
    .bind(pin_hash)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    Ok(HttpResponse::Ok().json(UserPublic::from(user)))
}

// ── DELETE /users/:id  (soft delete) ─────────────────────────

#[utoipa::path(
    delete,
    path = "/users/{id}",
    tag = "users",
    params(("id" = Uuid, Path, description = "User ID")),
    responses(
        (status = 204, description = "User deleted (soft delete)"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_user(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "delete").await?;

    let user = sqlx::query_as::<_, User>(
        "SELECT id, org_id, name, email, phone, password_hash, pin_hash, role,
                is_active, last_login_at, created_at, updated_at, deleted_at
         FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    require_same_org(&claims, user.org_id)?;

    if user.role == UserRole::SuperAdmin {
        require_super_admin(&claims)?;
    }

    if claims.role == UserRole::BranchManager {
        let same_branch: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM user_branch_assignments uba1
                JOIN user_branch_assignments uba2 ON uba2.branch_id = uba1.branch_id
                WHERE uba1.user_id = $1 AND uba2.user_id = $2
            )
            "#,
        )
        .bind(*user_id)
        .bind(claims.user_id())
        .fetch_one(pool.get_ref())
        .await?;

        if !same_branch {
            return Err(AppError::Forbidden(
                "You can only delete users assigned to your branches".into(),
            ));
        }
    }

    sqlx::query("UPDATE users SET deleted_at = NOW() WHERE id = $1")
        .bind(*user_id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── POST /users/:id/branches  ────────────────────────────────

#[utoipa::path(
    post,
    path = "/users/{id}/branches",
    tag = "users",
    params(("id" = Uuid, Path, description = "User ID")),
    request_body = AssignBranchRequest,
    responses(
        (status = 204, description = "Branch assigned"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn assign_branch(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    user_id: web::Path<Uuid>,
    body: web::Json<AssignBranchRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "update").await?;

    // Org-scope the assignment (V3): both the target user and the branch must be
    // in the caller's org. require_same_org early-returns Ok for super_admin.
    let (target_org, target_role): (Option<Uuid>, UserRole) =
        sqlx::query_as("SELECT org_id, role FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(*user_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    require_same_org(&claims, target_org)?;

    let branch_org: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(&claims, Some(branch_org))?;

    if claims.role == UserRole::BranchManager {
        // A branch_manager must not attach an admin to a branch — that step opens
        // the shared-branch gate that would let them reset the admin's creds (V4).
        if matches!(target_role, UserRole::OrgAdmin | UserRole::SuperAdmin) {
            return Err(AppError::Forbidden(
                "You cannot assign an admin user to a branch".into(),
            ));
        }
        let is_assigned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
        )
        .bind(claims.user_id())
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !is_assigned {
            return Err(AppError::Forbidden(
                "You cannot assign a user to a branch you are not assigned to".into(),
            ));
        }
    }

    sqlx::query(
        r#"
        INSERT INTO user_branch_assignments (user_id, branch_id, assigned_by)
        VALUES ($1, $2, $3)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(*user_id)
    .bind(body.branch_id)
    .bind(claims.user_id())
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── DELETE /users/:id/branches/:branch_id  ───────────────────

#[utoipa::path(
    delete,
    path = "/users/{id}/branches/{branch_id}",
    tag = "users",
    params(
        ("id" = Uuid, Path, description = "User ID"),
        ("branch_id" = Uuid, Path, description = "Branch ID"),
    ),
    responses(
        (status = 204, description = "Branch unassigned"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn unassign_branch(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "update").await?;

    let (user_id, branch_id) = path.into_inner();

    // Org-scope (V3): the target user and branch must both be in the caller's org.
    let target_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(user_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    require_same_org(&claims, target_org)?;

    let branch_org: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(&claims, Some(branch_org))?;

    if claims.role == UserRole::BranchManager {
        let is_assigned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
        )
        .bind(claims.user_id())
        .bind(branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !is_assigned {
            return Err(AppError::Forbidden(
                "You cannot unassign a user from a branch you are not assigned to".into(),
            ));
        }
    }

    sqlx::query("DELETE FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2")
        .bind(user_id)
        .bind(branch_id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /users/:id/branches  ─────────────────────────────────

#[utoipa::path(
    get,
    path = "/users/{id}/branches",
    tag = "users",
    params(("id" = Uuid, Path, description = "User ID")),
    responses(
        (status = 200, description = "Branches assigned to the user", body = Vec<UserBranch>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_user_branches(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    user_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "read").await?;

    if claims.role == UserRole::BranchManager && claims.user_id() != *user_id {
        let same_branch: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM user_branch_assignments uba1
                JOIN user_branch_assignments uba2 ON uba2.branch_id = uba1.branch_id
                WHERE uba1.user_id = $1 AND uba2.user_id = $2
            )
            "#,
        )
        .bind(*user_id)
        .bind(claims.user_id())
        .fetch_one(pool.get_ref())
        .await?;

        if !same_branch {
            return Err(AppError::Forbidden(
                "You do not have access to this user".into(),
            ));
        }
    }

    let rows = sqlx::query_as::<_, UserBranch>(
        r#"
        SELECT uba.branch_id, b.name as branch_name
        FROM user_branch_assignments uba
        JOIN branches b ON b.id = uba.branch_id
        WHERE uba.user_id = $1
        "#,
    )
    .bind(*user_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── Helper ────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}
