//! Which house rules apply at a branch, and where they come from.
//!
//! The organisation sets a rule; a branch may override it, per FIELD, and
//! `NULL` means inherit — never "off". The tax policy resolves this way in
//! `tax::policy::for_branch`, and the table rule resolves the same way here,
//! so that a reader who has learned one has learned the other.
//!
//! This is the ONE place the table rule is resolved. The enforcement site in
//! `orders` and the login payload the till caches both ask here, because a
//! rule resolved branch-first in one place and org-only in another is a
//! setting that looks like it does nothing.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Whether every dine-in sale at this branch must belong to a table, resolved
/// branch-first then org — `COALESCE(b.x, o.x)`.
///
/// `None` when the branch does not exist or is deleted. Callers decide what
/// that means for them: refusing a sale needs a live branch and treats it as
/// "no rule"; a login falls back to the organisation.
pub async fn require_table_for_orders(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Option<bool>, AppError> {
    let resolved: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(b.require_table_for_orders, o.require_table_for_orders) \
           FROM branches b JOIN organizations o ON o.id = b.org_id \
          WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    Ok(resolved)
}
