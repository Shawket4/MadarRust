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
    check_permission_for(pool, claims.user_id(), &claims.role, resource, action).await
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
    // super_admin bypasses everything
    if *role == UserRole::SuperAdmin {
        return Ok(());
    }

    // 0. Reject a known-disabled / soft-deleted account (V28): a deactivated or
    // deleted user must not keep acting until their JWT expires. Resolves to
    // Some(true) = active, Some(false) = disabled, None = no such row. We deny
    // only Some(false), so a missing row (service/integration tokens with no
    // corresponding stored user) still passes through.
    let account_ok: Option<bool> =
        sqlx::query_scalar("SELECT (is_active AND deleted_at IS NULL) FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    if account_ok == Some(false) {
        return Err(AppError::Forbidden("Account is disabled".into()));
    }

    // 1. Check per-user override (cached; invalidated on `permissions` writes)
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
            .fetch_optional(pool)
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
            .fetch_optional(pool)
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
