//! Anti-escalation guards for every write that changes who can do what.
//!
//! Phase 0 of PERMISSIONS_ARCHITECTURE.md (the interim rank rules). The rules
//! here are deliberately stricter than "hold the permission": holding
//! `users:create` or `permissions:update` is necessary, never sufficient.
//!
//! - **Dominance (S1, S5).** A caller may create, edit, deactivate or delete
//!   only a user whose role (current AND new) they dominate. An org owner
//!   (`org_admin`) dominates every org role including other owners; a branch
//!   manager dominates only teller, waiter and kitchen; nobody else dominates
//!   anyone. A teller holding a stray `users:create` grant can create nobody.
//! - **Access edits are strictly downward (S2).** Per-user overrides and branch
//!   assignments may be changed only on a user of strictly lower rank, never on
//!   yourself, and an owner's access is edited by the platform only — so no
//!   override can ever lock an owner out.
//! - **Hold to grant (S2).** Granting a cell (or deleting an override, which can
//!   re-expose a role default) requires the caller to hold that cell.
//! - **The last owner (G6).** The last active owner of an org cannot be
//!   deactivated, demoted or deleted.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::{errors::AppError, models::UserRole};

/// Interim privilege rank. Replaced by capability dominance in Phase 3.
pub fn rank(r: &UserRole) -> u8 {
    match r {
        UserRole::SuperAdmin => 3,
        UserRole::OrgAdmin => 2,
        UserRole::BranchManager => 1,
        UserRole::Teller | UserRole::Waiter | UserRole::Kitchen => 0,
    }
}

/// May `actor` create or modify an account whose role is `target`?
pub fn may_manage_role(actor: &UserRole, target: &UserRole) -> bool {
    match actor {
        UserRole::SuperAdmin => true,
        // Owners manage owners (multiple owners are allowed); never a platform user.
        UserRole::OrgAdmin => *target != UserRole::SuperAdmin,
        UserRole::BranchManager => rank(target) == 0,
        _ => false,
    }
}

/// May `actor` change the ACCESS (overrides, branch assignments) of a user whose
/// role is `target`? Strictly downward; an owner's access only by the platform.
pub fn may_edit_access(actor: &UserRole, target: &UserRole) -> bool {
    match actor {
        UserRole::SuperAdmin => true,
        UserRole::OrgAdmin | UserRole::BranchManager => rank(target) < rank(actor),
        _ => false,
    }
}

pub fn require_manage_role(actor: &UserRole, target: &UserRole) -> Result<(), AppError> {
    if may_manage_role(actor, target) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "You cannot manage a user with this role".into(),
        ))
    }
}

pub fn require_edit_access(
    actor_id: Uuid,
    actor: &UserRole,
    target_id: Uuid,
    target: &UserRole,
) -> Result<(), AppError> {
    if actor_id == target_id && *actor != UserRole::SuperAdmin {
        return Err(AppError::Forbidden(
            "You cannot change your own access".into(),
        ));
    }
    if may_edit_access(actor, target) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "You cannot change the access of a user at or above your level".into(),
        ))
    }
}

/// Is `user_id` the only active, undeleted owner of `org_id`?
pub async fn is_last_active_owner(
    conn: &mut PgConnection,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<bool, AppError> {
    let others: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users
          WHERE org_id = $1 AND id <> $2 AND role = 'org_admin'
            AND is_active AND deleted_at IS NULL",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(others == 0)
}

pub fn last_owner_error() -> AppError {
    AppError::Conflict(
        "This is the organization's last active owner. Add another owner first.".into(),
    )
}

/// Do `a` and `b` share at least one branch assignment?
pub async fn share_a_branch(conn: &mut PgConnection, a: Uuid, b: Uuid) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM user_branch_assignments x
            JOIN user_branch_assignments y ON y.branch_id = x.branch_id
            WHERE x.user_id = $1 AND y.user_id = $2)",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&mut *conn)
    .await?)
}

#[cfg(test)]
mod unit {
    use super::*;

    const ALL: [UserRole; 6] = [
        UserRole::SuperAdmin,
        UserRole::OrgAdmin,
        UserRole::BranchManager,
        UserRole::Teller,
        UserRole::Waiter,
        UserRole::Kitchen,
    ];

    #[test]
    fn nobody_below_owner_can_create_an_owner() {
        for a in ALL {
            let ok = may_manage_role(&a, &UserRole::OrgAdmin);
            assert_eq!(
                ok,
                matches!(a, UserRole::SuperAdmin | UserRole::OrgAdmin),
                "{a:?}"
            );
        }
    }

    #[test]
    fn manage_never_goes_above_the_actor() {
        for a in ALL {
            for t in ALL {
                if may_manage_role(&a, &t) {
                    assert!(rank(&t) <= rank(&a), "{a:?} manages {t:?}");
                }
                if may_edit_access(&a, &t) && a != UserRole::SuperAdmin {
                    assert!(rank(&t) < rank(&a), "{a:?} edits access of {t:?}");
                }
            }
        }
    }

    #[test]
    fn floor_roles_manage_nobody() {
        for a in [UserRole::Teller, UserRole::Waiter, UserRole::Kitchen] {
            for t in ALL {
                assert!(!may_manage_role(&a, &t));
                assert!(!may_edit_access(&a, &t));
            }
        }
    }

    #[test]
    fn self_access_edit_is_refused() {
        let me = Uuid::new_v4();
        assert!(require_edit_access(me, &UserRole::OrgAdmin, me, &UserRole::Teller).is_err());
    }
}
