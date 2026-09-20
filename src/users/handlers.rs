use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    models::{User, UserPublic, UserRole},
    permissions::{checker::check_permission, guard},
};

// ── PINs ──────────────────────────────────────────────────────

/// How long a NEWLY ISSUED PIN is (POS_SIGNIN_OVERHAUL.md §3.2, owner decision
/// 2026-09-16): six digits, one rule for every org. Four was thin once PINs are
/// unique across a whole company — a 30-branch chain shares 10,000 of them, so
/// "PIN taken" becomes routine and a colleague can guess. PINs already in use
/// keep working at whatever length they have; only setting a new one is held to
/// this.
pub const NEW_PIN_LEN: usize = 6;

pub(crate) fn check_new_pin(pin: &str) -> Result<(), AppError> {
    if pin.len() == NEW_PIN_LEN && pin.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "A new PIN must be {NEW_PIN_LEN} digits"
        )))
    }
}

/// Org-wide uniqueness (§3.1): a PIN must identify ONE person wherever the
/// device stands. Branch-scoped uniqueness would make the same PIN two people
/// at two branches, and would collide someone allowed at both with themselves.
///
/// The check is a single indexed lookup on the fingerprint — the same tool that
/// makes sign-in a lookup — so no plaintext is stored or compared. It cannot see
/// a PIN that has no fingerprint yet (§6); the partial unique index is the
/// backstop, and every PIN is re-issued at rollout.
async fn pin_is_free(
    pool: &sqlx::PgPool,
    org: Uuid,
    pin: &str,
    except: Option<Uuid>,
) -> Result<bool, AppError> {
    let fp = crate::auth::pin_fingerprint::fingerprint(org, pin);
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users
                        WHERE org_id = $1 AND pin_fingerprint = $2
                          AND deleted_at IS NULL AND ($3::uuid IS NULL OR id <> $3))",
    )
    .bind(org)
    .bind(&fp)
    .bind(except)
    .fetch_one(pool)
    .await?;
    Ok(!taken)
}

fn pin_taken() -> AppError {
    AppError::Conflict("Someone in this organization already uses that PIN".into())
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct PinSuggestion {
    /// Shown to the admin ONCE. Nothing stores it until it is set on a person.
    #[schema(example = "402913")]
    pub pin: String,
}

/// A free PIN for this org.
///
/// The owner's question was how the server can suggest a PIN when it stores no
/// plaintext. The fingerprint answers it: pick a candidate, fingerprint it, one
/// indexed lookup says taken or free. A handful of tries at most, and
/// uniqueness stays a database property rather than something the application
/// hopes it got right.
#[utoipa::path(
    get,
    path = "/users/pin-suggestion",
    tag = "users",
    responses(
        (status = 200, description = "A PIN nobody in this org is using", body = PinSuggestion),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn suggest_pin(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "create").await?;
    let org = claims
        .org_id()
        .ok_or_else(|| AppError::BadRequest("Choose an organization first".into()))?;

    for _ in 0..40 {
        // OS randomness without a new dependency, the same way the offline PIN
        // salt is minted (auth::offline): a v4 uuid is 122 random bits.
        let bytes = *Uuid::new_v4().as_bytes();
        let n: [u8; 8] = bytes[8..16].try_into().expect("8 of 16 bytes");
        let value = u64::from_le_bytes(n) % 10u64.pow(NEW_PIN_LEN as u32);
        let pin = format!("{value:0width$}", width = NEW_PIN_LEN);
        if pin_is_free(pool.get_ref(), org, &pin, None).await? {
            return Ok(HttpResponse::Ok().json(PinSuggestion { pin }));
        }
    }
    // A million combinations and forty misses means something is very wrong.
    Err(AppError::Internal)
}

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
    /// Required when `role = teller`. A NEW PIN is exactly 6 ASCII digits
    /// (owner decision, 2026-09-16); PINs already in use keep working at their
    /// old length. Ask `GET /users/pin-suggestion` for a free one.
    #[schema(
        pattern = "^[0-9]{6}$",
        min_length = 6,
        max_length = 6,
        example = "402913"
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
    /// A NEW PIN is exactly 6 digits; an existing shorter one keeps working
    /// until it is changed.
    #[schema(pattern = "^[0-9]{6}$", min_length = 6, max_length = 6)]
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
    pool: crate::db::Db,
    body: web::Json<CreateUserRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;

    check_permission(pool.get_ref(), &claims, "users", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    // Holding `users:create` is necessary, never sufficient: the caller must
    // already hold everything the new account's role would give it (G2), and
    // only an owner creates an owner (see permissions::guard).
    guard::require_can_create(pool.get_ref(), &claims, &body.role).await?;

    if claims.role == UserRole::BranchManager
        && let Some(branch_ids) = &body.branch_ids
    {
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

    // S8: every branch must belong to the new user's org, checked in code before
    // anything is written (RLS used to catch it only after the user row existed).
    if let Some(branch_ids) = &body.branch_ids
        && !branch_ids.is_empty()
    {
        let foreign: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM unnest($1::uuid[]) AS b(id)
              WHERE NOT EXISTS (SELECT 1 FROM branches br
                                 WHERE br.id = b.id AND br.org_id = $2 AND br.deleted_at IS NULL)",
        )
        .bind(branch_ids)
        .bind(body.org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if foreign > 0 {
            return Err(AppError::BadRequest(
                "Every branch must belong to the user's organization".into(),
            ));
        }
    }

    match body.role {
        UserRole::Teller | UserRole::Waiter | UserRole::Kitchen => {
            if body.pin.is_none() {
                return Err(AppError::BadRequest(
                    "Tellers, waiters and kitchen users require a PIN".into(),
                ));
            }
            let pin = body.pin.as_deref().unwrap();
            check_new_pin(pin)?;
            if !pin_is_free(pool.get_ref(), body.org_id, pin, None).await? {
                return Err(pin_taken());
            }
        }
        _ => {
            // A manager or owner may also work a till: an optional PIN.
            if let Some(pin) = body.pin.as_deref() {
                check_new_pin(pin)?;
                if !pin_is_free(pool.get_ref(), body.org_id, pin, None).await? {
                    return Err(pin_taken());
                }
            }
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
        .map(|p| bcrypt::hash(p, crate::secrets::BCRYPT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let pin_hash = body
        .pin
        .as_deref()
        .map(|p| bcrypt::hash(p, crate::secrets::BCRYPT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let mut tx = pool.begin().await?;
    let user = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (org_id, name, email, phone, role, password_hash, pin_hash,
                           pin_fingerprint)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
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
    // The keyed fingerprint (POS_SIGNIN_OVERHAUL.md §2): stamped here because
    // this is one of the two moments the plaintext PIN exists — the other is a
    // successful sign-in. Lookup only; the salted hash still verifies.
    .bind(
        body.pin
            .as_deref()
            .map(|p| crate::auth::pin_fingerprint::fingerprint(body.org_id, p)),
    )
    .fetch_one(&mut *tx)
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
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;

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
    pool: crate::db::Db,
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
                      -- Not a person: the actor a self-service order is
                      -- attributed to. It has no credentials and belongs in
                      -- no staff list.
                      AND NOT u.is_guest_principal
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
                      AND NOT is_guest_principal
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
            WHERE deleted_at IS NULL AND NOT is_guest_principal
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
    pool: crate::db::Db,
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
    pool: crate::db::Db,
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

    // S5 / G4-G6 (permissions::guard). Editing someone else requires dominating
    // their role now and the role they would get; nobody changes their own role
    // or active flag; the last active owner cannot be demoted or deactivated.
    let is_self = *user_id == claims.user_id();
    if is_self {
        if body.role.as_ref().is_some_and(|r| *r != existing.role) || body.is_active == Some(false)
        {
            return Err(AppError::Forbidden(
                "You cannot change your own role or deactivate yourself".into(),
            ));
        }
    } else {
        guard::require_dominance(
            pool.get_ref(),
            &claims,
            *user_id,
            crate::authz::Cap::StaffUsersEdit,
        )
        .await?;
        if let Some(new_role) = &body.role {
            guard::require_can_create(pool.get_ref(), &claims, new_role).await?;
        }
    }

    if claims.role == UserRole::BranchManager && !is_self {
        let mut conn = pool.acquire().await?;
        if !guard::share_a_branch(&mut conn, *user_id, claims.user_id()).await? {
            return Err(AppError::Forbidden(
                "You do not have access to this user".into(),
            ));
        }
    }

    let demotes_owner = existing.role == UserRole::OrgAdmin
        && existing.is_active
        && (body.role.as_ref().is_some_and(|r| *r != UserRole::OrgAdmin)
            || body.is_active == Some(false));
    if demotes_owner && let Some(org) = existing.org_id {
        let mut conn = pool.acquire().await?;
        if guard::is_last_active_owner(&mut conn, org, existing.id).await? {
            return Err(guard::last_owner_error());
        }
    }

    // A new PIN is held to the current length rule and to org-wide uniqueness,
    // which the update path never checked at all.
    if let Some(pin) = body.pin.as_deref() {
        check_new_pin(pin)?;
        if let Some(org) = existing.org_id
            && !pin_is_free(pool.get_ref(), org, pin, Some(existing.id)).await?
        {
            return Err(pin_taken());
        }
    }

    let password_hash = body
        .password
        .as_deref()
        .map(|p| bcrypt::hash(p, crate::secrets::BCRYPT_COST))
        .transpose()
        .map_err(|_| AppError::Internal)?;

    let pin_hash = body
        .pin
        .as_deref()
        .map(|p| bcrypt::hash(p, crate::secrets::BCRYPT_COST))
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
            pin_fingerprint = COALESCE($10, pin_fingerprint),
            -- A password, role or active-flag change ends every web session.
            sessions_valid_after = CASE WHEN $9 THEN NOW() ELSE sessions_valid_after END,
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
    .bind(
        body.password.is_some()
            || body.role.as_ref().is_some_and(|r| *r != existing.role)
            || body.is_active == Some(false),
    )
    // A new PIN gets a new fingerprint in the same statement, so the two can
    // never disagree. Without an org (a platform account) there is nothing to
    // scope it by and nothing that signs in at a till.
    .bind(
        body.pin
            .as_deref()
            .zip(existing.org_id)
            .map(|(p, org)| crate::auth::pin_fingerprint::fingerprint(org, p)),
    )
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
    pool: crate::db::Db,
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

    if user.id == claims.user_id() {
        return Err(AppError::Forbidden("You cannot delete yourself".into()));
    }
    guard::require_dominance(
        pool.get_ref(),
        &claims,
        *user_id,
        crate::authz::Cap::StaffUsersDelete,
    )
    .await?;

    let mut conn = pool.acquire().await?;
    if claims.role == UserRole::BranchManager
        && !guard::share_a_branch(&mut conn, *user_id, claims.user_id()).await?
    {
        return Err(AppError::Forbidden(
            "You can only delete users assigned to your branches".into(),
        ));
    }
    if user.role == UserRole::OrgAdmin
        && user.is_active
        && let Some(org) = user.org_id
        && guard::is_last_active_owner(&mut conn, org, user.id).await?
    {
        return Err(guard::last_owner_error());
    }
    drop(conn);

    sqlx::query("UPDATE users SET deleted_at = NOW(), sessions_valid_after = NOW() WHERE id = $1")
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
    pool: crate::db::Db,
    user_id: web::Path<Uuid>,
    body: web::Json<AssignBranchRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "update").await?;

    // Org-scope the assignment (V3): both the target user and the branch must be
    // in the caller's org. require_same_org early-returns Ok for super_admin.
    let (target_org, _target_role): (Option<Uuid>, UserRole) =
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

    // Branch assignments are access: never your own, and only on someone whose
    // access you already dominate.
    guard::require_dominance(
        pool.get_ref(),
        &claims,
        *user_id,
        crate::authz::Cap::StaffPermissionsEdit,
    )
    .await?;

    if claims.role == UserRole::BranchManager {
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
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "users", "update").await?;

    let (user_id, branch_id) = path.into_inner();

    // Org-scope (V3): the target user and branch must both be in the caller's org.
    let (target_org, _target_role): (Option<Uuid>, UserRole) =
        sqlx::query_as("SELECT org_id, role FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(user_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    require_same_org(&claims, target_org)?;
    guard::require_dominance(
        pool.get_ref(),
        &claims,
        user_id,
        crate::authz::Cap::StaffPermissionsEdit,
    )
    .await?;

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
    pool: crate::db::Db,
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
