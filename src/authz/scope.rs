//! Where a person works, from the architecture E model.
//!
//! Before phase 3 every feature module carried its own near-identical
//! `require_branch_access`, each one branching on role names: super admin
//! bypass, org match, `OrgAdmin` bypass, "tellers are org-scoped", and only
//! then `user_branch_assignments` for branch managers. That is the last place
//! role names decided access.
//!
//! The replacement asks the model instead: a person works at a branch when a
//! live role assignment covers it (`all_branches`, or the branch listed), or
//! when they are an owner. `resolve` already computes exactly that — an
//! assignment that does not cover the scope contributes no role kind — so
//! "works here" is `owner || !kinds.is_empty()`.
//!
//! The org boundary is unchanged, and so is the super admin bypass: a platform
//! user has no org rows to resolve against.

use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;

/// The branches a caller may act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchScope {
    /// Every branch of the org (owner, super admin, or an org-wide assignment).
    All,
    /// Exactly these branches.
    Only(Vec<Uuid>),
}

impl BranchScope {
    pub fn covers(&self, branch: Uuid) -> bool {
        match self {
            BranchScope::All => true,
            BranchScope::Only(b) => b.contains(&branch),
        }
    }
}

/// Does this person work at `branch`?
pub async fn works_at(pool: &PgPool, user_id: Uuid, branch: Uuid) -> Result<bool, AppError> {
    let eff = super::require::effective(pool, user_id, Some(branch)).await?;
    Ok(eff.owner || eff.kinds.0 != 0)
}

/// The caller's branch scope, straight from their live role assignments.
pub async fn branch_scope(pool: &PgPool, claims: &Claims) -> Result<BranchScope, AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(BranchScope::All);
    }
    let mut conn = pool.acquire().await?;
    let Some(loaded) = super::load::load(&mut conn, claims.user_id()).await? else {
        return Ok(BranchScope::Only(vec![]));
    };
    if !loaded.principal.active {
        return Ok(BranchScope::Only(vec![]));
    }
    if loaded.principal.is_owner {
        return Ok(BranchScope::All);
    }
    let now = chrono::Utc::now().timestamp();
    let mut ids: Vec<Uuid> = Vec::new();
    for a in &loaded.principal.assignments {
        if a.valid_from.is_some_and(|t| now < t) || a.valid_to.is_some_and(|t| now >= t) {
            continue;
        }
        if a.all_branches {
            return Ok(BranchScope::All);
        }
        for b in &a.branches {
            if let Ok(id) = Uuid::parse_str(b)
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
    }
    Ok(BranchScope::Only(ids))
}

/// 403 unless the caller works at `branch_id`; 404 when the branch is gone and
/// 403 when it belongs to another org (both unchanged from the role-name
/// version this replaces).
pub async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }

    if !works_at(pool, claims.user_id(), branch_id).await? {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }
    Ok(())
}

/// The same, plus the token-branch binding some modules enforce (purchasing,
/// stocktakes, reports). Kept separate because orders, delivery and inventory
/// deliberately do NOT bind the token to one branch: a POS worker may act on
/// any branch they are ALLOWED at (`require_branch_access` above still decides
/// that — D13's "any branch of the org" no longer applies, see
/// POS_SIGNIN_OVERHAUL.md §5.2), and the order still records the device's own
/// branch.
pub async fn require_branch_access_bound(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    require_branch_access(pool, claims, branch_id).await?;
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    // A token that carries a branch is bound to it: a session minted at one
    // branch must not act on another, even when the person is assigned to both
    // (V26). Before architecture E this was written as "if the role is teller",
    // which was the same thing — only PIN sessions carry a branch — except that
    // an owner or manager on a tablet now gets a branch-bound PIN session too,
    // and should be bound by it exactly like anyone else. Web sessions carry no
    // branch and are unaffected.
    if let Some(token_branch) = claims.branch_id()
        && token_branch != branch_id
    {
        return Err(AppError::Forbidden(
            "This device is signed in to a different branch.".into(),
        ));
    }

    Ok(())
}
