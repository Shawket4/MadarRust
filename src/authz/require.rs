//! Capability checks for handlers: `authz::require(pool, &claims, Cap::X, branch)`.
//!
//! A super admin (platform) passes. Everyone else is resolved through the shared
//! crate at the branch the request is about (or anywhere they work when the
//! request has no branch).

use sqlx::PgPool;
use uuid::Uuid;

use super::{Cap, Decision, EffectiveSet, Request, Scope, decide};
use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::models::UserRole;

/// A person's effective set at a branch (or anywhere). Inactive or unknown
/// people hold nothing.
pub async fn effective(
    pool: &PgPool,
    user_id: Uuid,
    branch: Option<Uuid>,
) -> Result<EffectiveSet, AppError> {
    // From memory when the fast path knows the person's epoch: no connection.
    if let Some(loaded) = super::load::cached(user_id).await {
        return Ok(resolve_at_branch(&loaded, branch));
    }
    let mut conn = pool.acquire().await?;
    effective_on(&mut conn, user_id, branch).await
}

pub async fn effective_on(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    branch: Option<Uuid>,
) -> Result<EffectiveSet, AppError> {
    let Some(loaded) = super::load::load(conn, user_id).await? else {
        return Ok(EffectiveSet::default());
    };
    Ok(resolve_at_branch(&loaded, branch))
}

fn resolve_at_branch(loaded: &super::load::Loaded, branch: Option<Uuid>) -> EffectiveSet {
    let b = branch.map(|b| b.to_string());
    let scope = match &b {
        Some(b) => Scope::Branch(b),
        None => Scope::Anywhere,
    };
    loaded.resolve_at(scope, chrono::Utc::now().timestamp())
}

/// The caller's effective set; a super admin holds everything.
pub async fn effective_for_claims(
    pool: &PgPool,
    claims: &Claims,
    branch: Option<Uuid>,
) -> Result<EffectiveSet, AppError> {
    if claims.role == UserRole::SuperAdmin {
        let mut e = EffectiveSet {
            caps: super::CapSet::all(),
            owner: true,
            ..Default::default()
        };
        e.kinds.insert(super::RoleKind::OrgAdmin);
        return Ok(e);
    }
    effective(pool, claims.user_id(), branch).await
}

pub async fn can(
    pool: &PgPool,
    claims: &Claims,
    cap: Cap,
    branch: Option<Uuid>,
) -> Result<bool, AppError> {
    Ok(effective_for_claims(pool, claims, branch).await?.can(cap))
}

/// 403 unless the caller holds `cap` (at `branch` when given).
pub async fn require(
    pool: &PgPool,
    claims: &Claims,
    cap: Cap,
    branch: Option<Uuid>,
) -> Result<(), AppError> {
    if can(pool, claims, cap, branch).await? {
        Ok(())
    } else {
        Err(denial(pool, claims.user_id(), cap).await)
    }
}

/// The same for an explicit person (replay attributes an op to its author).
pub async fn require_for(
    pool: &PgPool,
    user_id: Uuid,
    cap: Cap,
    branch: Option<Uuid>,
) -> Result<(), AppError> {
    if effective(pool, user_id, branch).await?.can(cap) {
        Ok(())
    } else {
        Err(denial(pool, user_id, cap).await)
    }
}

/// Decide a request with figures (limits, approval).
pub async fn decide_for(
    pool: &PgPool,
    user_id: Uuid,
    req: &Request,
    branch: Option<Uuid>,
) -> Result<Decision, AppError> {
    Ok(decide(&effective(pool, user_id, branch).await?, req))
}

/// PM-1: may `approver` settle a parked act (from their phone or the
/// dashboard)? Anyone allowed it outright — the capability without the limit
/// that made it wait — never the one who asked or the person it is for.
pub async fn settle(
    pool: &PgPool,
    approver: Uuid,
    pending: &super::Pending,
    branch: Option<Uuid>,
) -> Result<(), AppError> {
    let eff = effective(pool, approver, branch).await?;
    super::can_settle(&eff, &approver.to_string(), pending).map_err(|why| match why {
        super::Why::SamePerson => AppError::Coded {
            status: 403,
            code: "OWN_DECISION",
            reason: "Someone else has to decide this one.".into(),
        },
        _ => AppError::Coded {
            status: 403,
            code: "ABOVE_LIMIT",
            reason: "This is above your limit too — it waits for someone with a higher one.".into(),
        },
    })
}

/// Why `user_id` may not use `cap`: the plan's refusal when the plan is what
/// removed it, else the ordinary [`denied`].
pub async fn denial(pool: &PgPool, user_id: Uuid, cap: Cap) -> AppError {
    let plan = match pool.acquire().await {
        Ok(mut conn) => match super::load::load(&mut conn, user_id).await {
            Ok(Some(l)) => l.plan,
            _ => return denied(cap),
        },
        Err(_) => return denied(cap),
    };
    crate::plan::refusal_for(plan, cap).unwrap_or_else(|| denied(cap))
}

pub fn denied(cap: Cap) -> AppError {
    AppError::Forbidden(format!(
        "You don't have permission to do this: {} ({})",
        cap.meta().en,
        cap.key()
    ))
}
