//! Branch scope for every `/staff/*` route (RO-6, RO-9): ONE place that asks
//! the authz model whether the caller may act, and where.
//!
//! - [`require_at`]: the capability at one branch of the org.
//! - [`require_for`]: the capability at ANY of an employee's branches — a
//!   manager of their second branch counts too (audit B13). An employee with
//!   no branch needs the capability at every branch.
//! - [`require_everywhere`]: the capability at every branch of the org — the
//!   org-wide acts: payroll run/approve/reopen, rules, holidays, roster
//!   settings. An owner (or an org-wide role) holds it; a branch manager
//!   never does, even with the capability at their own branch.
//! - [`scope`]: the branches the caller holds a capability at, for filtering
//!   lists (`None` = every branch).
//!
//! No `/staff/*` handler asks "held anywhere" any more: that answer is what
//! let a manager of branch A read and decide for branch B.

use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::authz::{Cap, CapSet, Scope, resolve};
use crate::errors::AppError;
use crate::models::UserRole;

/// An employee a handler acts on, with where they work.
#[derive(Debug, Clone)]
pub struct Subject {
    pub id: Uuid,
    pub org_id: Uuid,
    /// The linked Madar user, if any.
    pub user_id: Option<Uuid>,
    pub name: String,
    pub employment_status: String,
    pub branches: Vec<Uuid>,
}

impl Subject {
    /// Who this person is to the authz crate's "not the same person" rule
    /// (PM-1 `can_settle`): their linked user, else the employee.
    pub fn authz_key(&self) -> String {
        self.user_id.unwrap_or(self.id).to_string()
    }

    /// Is `claims` this very person (acting through their linked user)?
    pub fn is(&self, claims: &Claims) -> bool {
        self.user_id.is_some_and(|u| u.to_string() == claims.sub)
    }

    /// One branch to record against (a flag, an expense): the first.
    pub fn home(&self) -> Option<Uuid> {
        self.branches.first().copied()
    }
}

/// Load an employee of `org_id`, 404 otherwise.
pub async fn subject(pool: &PgPool, org_id: Uuid, employee_id: Uuid) -> Result<Subject, AppError> {
    let row: Option<(Uuid, Option<Uuid>, String, String)> = sqlx::query_as(
        "SELECT org_id, user_id, name, employment_status FROM employees \
          WHERE id = $1 AND org_id = $2",
    )
    .bind(employee_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((org_id, user_id, name, employment_status)) = row else {
        return Err(AppError::NotFound("Employee not found".into()));
    };
    Ok(Subject {
        id: employee_id,
        org_id,
        user_id,
        name,
        employment_status,
        branches: branches_of(pool, employee_id).await?,
    })
}

/// The branches an employee works at, oldest assignment first.
pub async fn branches_of(pool: &PgPool, employee_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT eb.branch_id FROM employee_branches eb \
           JOIN branches b ON b.id = eb.branch_id AND b.deleted_at IS NULL \
          WHERE eb.employee_id = $1 ORDER BY eb.assigned_at, eb.branch_id",
    )
    .bind(employee_id)
    .fetch_all(pool)
    .await?)
}

async fn live_branches(pool: &PgPool, org_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

/// Where the caller holds `cap` in `org_id`: every branch, or these.
async fn held_at(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
) -> Result<(bool, Vec<Uuid>), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok((true, vec![]));
    }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    let branches = live_branches(pool, org_id).await?;
    let mut conn = pool.acquire().await?;
    let Some(loaded) = crate::authz::load::load(&mut conn, claims.user_id_safe()?).await? else {
        return Ok((false, vec![]));
    };
    drop(conn);
    let now = chrono::Utc::now().timestamp();
    let p = &loaded.principal;
    if branches.is_empty() {
        // A business with no branch yet: only an org-wide holder counts.
        let eff = resolve(p, Scope::Anywhere, now, &loaded.policy);
        let org_wide = eff.owner
            || p.assignments
                .iter()
                .any(|a| a.all_branches && a.role.grants.contains(cap));
        return Ok((org_wide && eff.can(cap), vec![]));
    }
    let mut at = Vec::new();
    for b in &branches {
        let s = b.to_string();
        if resolve(p, Scope::Branch(&s), now, &loaded.policy).can(cap) {
            at.push(*b);
        }
    }
    Ok((at.len() == branches.len(), at))
}

/// Every capability the caller holds at EVERY live branch of `org_id`: the
/// test [`require_everywhere`] makes, for all of them at once, so the UI can
/// hide an org-wide act the server would refuse (E2E B-SETUP-3, AT-11). The
/// owner and a super admin hold them all.
pub async fn caps_everywhere(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
) -> Result<CapSet, AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(CapSet::all());
    }
    if claims.org_id() != Some(org_id) {
        return Ok(CapSet::EMPTY);
    }
    let branches = live_branches(pool, org_id).await?;
    let mut conn = pool.acquire().await?;
    let Some(loaded) = crate::authz::load::load(&mut conn, claims.user_id_safe()?).await? else {
        return Ok(CapSet::EMPTY);
    };
    drop(conn);
    let now = chrono::Utc::now().timestamp();
    let p = &loaded.principal;
    if branches.is_empty() {
        // As `held_at`: with no branch yet, only an org-wide holder counts.
        let eff = resolve(p, Scope::Anywhere, now, &loaded.policy);
        let mut out = CapSet::EMPTY;
        for c in eff.caps.iter() {
            if eff.owner
                || p.assignments
                    .iter()
                    .any(|a| a.all_branches && a.role.grants.contains(c))
            {
                out.insert(c);
            }
        }
        return Ok(out);
    }
    let mut all: Option<CapSet> = None;
    for b in &branches {
        let s = b.to_string();
        let caps = resolve(p, Scope::Branch(&s), now, &loaded.policy).caps;
        all = Some(match all {
            None => caps,
            Some(a) => a.intersect(&caps),
        });
    }
    Ok(all.unwrap_or(CapSet::EMPTY))
}

/// 403 unless the caller holds `cap` at `branch`, a live branch of `org_id`.
pub async fn require_at(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
    branch: Uuid,
) -> Result<(), AppError> {
    let in_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch)
            .fetch_optional(pool)
            .await?;
    match in_org {
        None => return Err(AppError::NotFound("Branch not found".into())),
        Some(o) if o != org_id => {
            return Err(AppError::Forbidden(
                "Branch belongs to a different org".into(),
            ));
        }
        Some(_) => {}
    }
    if claims.role != UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    crate::authz::require::require(pool, claims, cap, Some(branch)).await
}

/// 403 unless the caller holds `cap` at one of `subject`'s branches (every
/// branch when the subject has none).
pub async fn require_for(
    pool: &PgPool,
    claims: &Claims,
    cap: Cap,
    subject: &Subject,
) -> Result<(), AppError> {
    let (all, at) = held_at(pool, claims, subject.org_id, cap).await?;
    if all || subject.branches.iter().any(|b| at.contains(b)) {
        Ok(())
    } else {
        Err(crate::authz::require::denied(cap))
    }
}

/// The branch a decision about `subject` is asked at, for limit checks
/// (`decide_for`, `settle`): one of their branches where the caller holds
/// `cap`, else their first. `None` for someone with no branch.
pub async fn decision_branch(
    pool: &PgPool,
    claims: &Claims,
    cap: Cap,
    subject: &Subject,
) -> Result<Option<Uuid>, AppError> {
    let (all, at) = held_at(pool, claims, subject.org_id, cap).await?;
    if all {
        return Ok(subject.home());
    }
    Ok(subject
        .branches
        .iter()
        .find(|b| at.contains(b))
        .copied()
        .or(subject.home()))
}

/// 403 unless the caller holds `cap` at EVERY branch of `org_id` (RO-9).
pub async fn require_everywhere(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
) -> Result<(), AppError> {
    let (all, _) = held_at(pool, claims, org_id, cap).await?;
    if all {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "This needs {} for every branch ({}).",
            cap.meta().en,
            cap.key()
        )))
    }
}

/// The branches whose people and records a list may show: `None` = all of
/// them; otherwise the branches the caller holds `cap` at (403 when none).
pub async fn scope(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
) -> Result<Option<Vec<Uuid>>, AppError> {
    let (all, at) = held_at(pool, claims, org_id, cap).await?;
    if all {
        return Ok(None);
    }
    if at.is_empty() {
        return Err(crate::authz::require::denied(cap));
    }
    Ok(Some(at))
}

/// [`scope`] narrowed to one asked-for branch (which must be in scope).
pub async fn scope_at(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
    asked: Option<Uuid>,
) -> Result<Option<Vec<Uuid>>, AppError> {
    match asked {
        Some(b) => {
            require_at(pool, claims, org_id, cap, b).await?;
            Ok(Some(vec![b]))
        }
        None => scope(pool, claims, org_id, cap).await,
    }
}

/// Does the caller hold `cap` for `subject` (no error)?
pub async fn can_for(
    pool: &PgPool,
    claims: &Claims,
    cap: Cap,
    subject: &Subject,
) -> Result<bool, AppError> {
    match require_for(pool, claims, cap, subject).await {
        Ok(()) => Ok(true),
        Err(AppError::Forbidden(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Does the caller hold `cap` at every branch (no error)?
pub async fn can_everywhere(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
) -> Result<bool, AppError> {
    Ok(held_at(pool, claims, org_id, cap).await?.0)
}

/// SQL: an employee (column `col`) is visible under a list scope bound as
/// `$n` (`uuid[]`, NULL = every branch).
pub fn in_scope(col: &str, n: usize) -> String {
    format!(
        "(${n}::uuid[] IS NULL OR EXISTS (SELECT 1 FROM employee_branches eb_scope \
           WHERE eb_scope.employee_id = {col} AND eb_scope.branch_id = ANY(${n})))"
    )
}

/// 403 unless the caller holds `cap` at SOME branch of `org_id` — the cheap
/// first gate a handler runs before it looks anything up, so a person with no
/// such right learns nothing (not even a 404) about what exists. The precise
/// check at the right branch follows once the subject is known.
pub async fn gate(pool: &PgPool, claims: &Claims, org_id: Uuid, cap: Cap) -> Result<(), AppError> {
    scope(pool, claims, org_id, cap).await.map(|_| ())
}
