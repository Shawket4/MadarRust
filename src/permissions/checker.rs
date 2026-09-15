use crate::{auth::jwt::Claims, errors::AppError, models::UserRole};
use sqlx::PgPool;

/// Check if a user has permission for a resource+action.
/// Resolution order:
///   1. super_admin → always granted
///   2. per-user override in `permissions` table → use that value
///   3. role default in `role_permissions` table → use that value
///   4. not found → deny
pub async fn check_permission(
    pool: &PgPool,
    claims: &Claims,
    resource: &str,
    action: &str,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }
    let mut conn = pool.acquire().await?;
    check_on(
        &mut conn,
        claims.user_id(),
        &claims.role,
        Some(claims.iat as i64),
        resource,
        action,
    )
    .await
}

/// Like [`check_permission`] but for an EXPLICIT principal (user id + role) instead
/// of the bearer's claims — used by `/sync/replay`, where the acting principal is
/// the op's EMBEDDED author (the offline device's teller), not the token flushing
/// the queue. Same resolution order (super_admin → per-user override → role default
/// → deny), so a queued offline write can't bypass a per-user permission revocation
/// that a live request would honor.
pub async fn check_permission_for(
    pool: &PgPool,
    user_id: uuid::Uuid,
    role: &UserRole,
    resource: &str,
    action: &str,
) -> Result<(), AppError> {
    if *role == UserRole::SuperAdmin {
        return Ok(());
    }
    let mut conn = pool.acquire().await?;
    check_permission_for_on(&mut conn, user_id, role, resource, action).await
}

/// [`check_permission_for`] on a connection the caller already holds — e.g. a
/// handler's open transaction, so the check never takes a second pooled
/// connection while that transaction is held.
pub async fn check_permission_for_on(
    conn: &mut sqlx::PgConnection,
    user_id: uuid::Uuid,
    role: &UserRole,
    resource: &str,
    action: &str,
) -> Result<(), AppError> {
    check_on(conn, user_id, role, None, resource, action).await
}

/// The one resolution. `issued_at` is the bearer token's `iat` when the check is
/// for a live request: a token issued before the account's
/// `sessions_valid_after` (bumped on a password, role or active-flag change, or a
/// delete) is refused, which is how a web session is revoked.
async fn check_on(
    conn: &mut sqlx::PgConnection,
    user_id: uuid::Uuid,
    role: &UserRole,
    issued_at: Option<i64>,
    resource: &str,
    action: &str,
) -> Result<(), AppError> {
    // super_admin bypasses everything
    if *role == UserRole::SuperAdmin {
        return Ok(());
    }

    // 0. Reject a known-disabled / soft-deleted account (V28): a deactivated or
    // deleted user must not keep acting until their JWT expires. Resolves to
    // Some(true) = active, Some(false) = disabled, None = no such row. We deny
    // only Some(false), so a missing row (service/integration tokens with no
    // corresponding stored user) still passes through.
    let account: Option<(bool, Option<i64>)> = sqlx::query_as(
        "SELECT (is_active AND deleted_at IS NULL), \
                floor(extract(epoch FROM sessions_valid_after))::bigint \
           FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((ok, valid_after)) = account {
        if !ok {
            return Err(AppError::Forbidden("Account is disabled".into()));
        }
        if let (Some(iat), Some(after)) = (issued_at, valid_after)
            && iat < after
        {
            return Err(AppError::Unauthorized(
                "Your session has ended. Please sign in again.".into(),
            ));
        }
    }

    let legacy = legacy_decision(conn, user_id, role, resource, action).await;
    crate::authz::shadow::observe(conn, user_id, resource, action, legacy).await
}

/// Today's resolution as a boolean, for the side-by-side comparison.
pub async fn check_permission_for_legacy(
    conn: &mut sqlx::PgConnection,
    user_id: uuid::Uuid,
    role: &UserRole,
    resource: &str,
    action: &str,
) -> Result<bool, AppError> {
    match legacy_decision(conn, user_id, role, resource, action).await {
        Ok(()) => Ok(true),
        Err(AppError::Forbidden(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Today's resolution: per-user override, then the global role default, then deny.
async fn legacy_decision(
    conn: &mut sqlx::PgConnection,
    user_id: uuid::Uuid,
    role: &UserRole,
    resource: &str,
    action: &str,
) -> Result<(), AppError> {
    // 1. Check per-user override (cached; invalidated on `permissions` writes)
    let c = &mut *conn;
    let user_override: Option<bool> =
        crate::cache::user_override(user_id, resource, action, || async move {
            let v: Option<bool> = sqlx::query_scalar(
                r#"
                SELECT granted FROM permissions
                WHERE user_id  = $1
                  AND resource = $2::permission_resource
                  AND action   = $3::permission_action
                "#,
            )
            .bind(user_id)
            .bind(resource)
            .bind(action)
            .fetch_optional(c)
            .await?;
            Ok(v)
        })
        .await?;

    if let Some(granted) = user_override {
        return if granted {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "Permission denied: {} {}",
                action, resource
            )))
        };
    }

    // 2. Fall back to role default
    let role_str = match role {
        UserRole::OrgAdmin => "org_admin",
        UserRole::BranchManager => "branch_manager",
        UserRole::Teller => "teller",
        UserRole::Waiter => "waiter",
        UserRole::Kitchen => "kitchen",
        UserRole::SuperAdmin => unreachable!(),
    };

    let c = &mut *conn;
    let role_default: Option<bool> =
        crate::cache::role_default(role_str, resource, action, || async move {
            let v: Option<bool> = sqlx::query_scalar(
                r#"
                SELECT granted FROM role_permissions
                WHERE role     = $1::user_role
                  AND resource = $2::permission_resource
                  AND action   = $3::permission_action
                "#,
            )
            .bind(role_str)
            .bind(resource)
            .bind(action)
            .fetch_optional(c)
            .await?;
            Ok(v)
        })
        .await?;

    match role_default {
        Some(true) => Ok(()),
        Some(false) => Err(AppError::Forbidden(format!(
            "Permission denied: {} {}",
            action, resource
        ))),
        None => Err(AppError::Forbidden(format!(
            "Permission denied: {} {} (no rule found)",
            action, resource
        ))),
    }
}
