use sqlx::PgPool;
use crate::{auth::jwt::Claims, errors::AppError, models::UserRole};

/// Check if a user has permission for a resource+action.
/// Resolution order:
///   1. super_admin → always granted
///   2. per-user override in `permissions` table → use that value
///   3. role default in `role_permissions` table → use that value
///   4. not found → deny
pub async fn check_permission(
    pool:     &PgPool,
    claims:   &Claims,
    resource: &str,
    action:   &str,
) -> Result<(), AppError> {
    // super_admin bypasses everything
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let user_id = claims.user_id();

    // 0. Reject a known-disabled / soft-deleted account (V28): a deactivated or
    // deleted user must not keep acting until their JWT expires. Resolves to
    // Some(true) = active, Some(false) = disabled, None = no such row. We deny
    // only Some(false), so a missing row (service/integration tokens with no
    // corresponding stored user) still passes through.
    let account_ok: Option<bool> = sqlx::query_scalar(
        "SELECT (is_active AND deleted_at IS NULL) FROM users WHERE id = $1"
    )
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
    let role_str = match claims.role {
        UserRole::OrgAdmin      => "org_admin",
        UserRole::BranchManager => "branch_manager",
        UserRole::Teller        => "teller",
        UserRole::Waiter        => "waiter",
        UserRole::Kitchen       => "kitchen",
        UserRole::SuperAdmin    => unreachable!(),
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
        Some(true)  => Ok(()),
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