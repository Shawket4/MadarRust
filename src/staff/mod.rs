//! Staff, attendance, leave, and payroll.
//!
//! An EMPLOYEE IS A USER — there is no parallel person entity. A `users` row with
//! a `staff_profiles` row attached is staff; without one it is only a login. That
//! is why no `user_role` was added: the admin surface is gated by the `staff` /
//! `work_shifts` / `attendance` / `leave` / `payroll` permission resources, while
//! `/staff/me/*` is own-row scoped and needs no grant at all. A teller can clock
//! in and read their own payslip without being able to see anyone else's salary.
//!
//! NAMING: `shifts` in this codebase is a TELLER CASH-DRAWER SESSION. HR
//! schedules are `work_shifts` everywhere. Never overload `shifts`.
//!
//! Layout:
//! - [`rules`]      — pure attendance/payroll math. No DB, no clock. The money.
//! - [`directory`]  — departments, employee profiles, documents.
//! - [`schedules`]  — work shifts, roster assignment, per-date overrides.
//! - [`attendance`] — check-in/out (server-side geofence), manual correction,
//!   the ledger and its reports.
//! - [`requests`]   — the ONE table behind every "may I?": leave, late arrival,
//!   early departure, mid-shift permission, mission. An approved request removes
//!   a penalty at its source rather than generating one and cancelling it.
//! - [`penalties`]  — where rules become money. The only writer of automatic
//!   deductions, and the thing that refuses to overwrite a human's override.
//! - [`payroll`]    — deductions, bonuses, advances, periods, payslips.
//! - [`jobs`]       — the nightly sweep: mark absences, close forgotten checkouts.

pub mod attendance;
pub mod directory;
pub mod jobs;
pub mod payroll;
pub mod penalties;
pub mod requests;
pub mod routes;
pub mod rules;
pub mod schedules;

#[cfg(test)]
mod tests;

use actix_web::HttpRequest;
use chrono::NaiveDate;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{auth::jwt::Claims, errors::AppError};

/// Fallback timezone when neither the branch nor the org sets one. Same default
/// the rest of the system uses.
pub(crate) const DEFAULT_TZ: &str = "Africa/Cairo";

/// The org every staff query is scoped to: the caller's own, or — for a super
/// admin, who has no org of their own — the one pinned with `X-Org-Id`.
pub(crate) fn scope_org(req: &HttpRequest, claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .scope_org(crate::auth::middleware::header_org_id(req))
        .ok_or_else(|| {
            AppError::Forbidden(
                "No organization in scope — pin one with the X-Org-Id header".into(),
            )
        })
}

/// Confirm `user_id` is a live user in `org_id`, so a handler addressing an
/// employee by id gets a 404 rather than silently doing nothing.
///
/// RLS already makes another tenant's rows invisible; this turns that invisibility
/// into an explicit, testable status code.
pub(crate) async fn require_user_in_org(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<(), AppError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(user_id)
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(AppError::NotFound("Employee not found".into()))
    }
}

/// A branch's effective IANA timezone: branch → org → `Africa/Cairo`.
///
/// Every business date in this module is derived from an instant `AT TIME ZONE`
/// this value — never from the device clock. `::text` on the enum-typed columns
/// is required: `AT TIME ZONE` does not accept the timezone enum directly.
pub(crate) async fn branch_timezone(pool: &PgPool, branch_id: Uuid) -> Result<String, AppError> {
    sqlx::query_scalar(
        "SELECT COALESCE(b.timezone::text, o.timezone::text, $2)
           FROM branches b
           JOIN organizations o ON o.id = b.org_id
          WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(branch_id)
    .bind(DEFAULT_TZ)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// Resolve a branch's org (and confirm it is live). Mirrors
/// `reservations::resolve_branch_org`.
pub(crate) async fn resolve_branch_org(pool: &PgPool, branch_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// Guard a reporting window so a single request cannot ask for a decade of
/// attendance rows.
pub(crate) fn validate_range(
    from: NaiveDate,
    to: NaiveDate,
    max_days: i64,
) -> Result<(), AppError> {
    if to < from {
        return Err(AppError::BadRequest("`to` is before `from`".into()));
    }
    if (to - from).num_days() > max_days {
        return Err(AppError::BadRequest(format!(
            "Range too wide — at most {max_days} days per request"
        )));
    }
    Ok(())
}

/// The four-state request machine shared by leave requests, late passes, and
/// missions. Returns the canonical string for the `status` column.
pub(crate) fn validate_decision(status: &str) -> Result<&'static str, AppError> {
    match status {
        "approved" => Ok("approved"),
        "rejected" => Ok("rejected"),
        "cancelled" => Ok("cancelled"),
        other => Err(AppError::BadRequest(format!(
            "Unknown decision '{other}' — expected approved, rejected, or cancelled"
        ))),
    }
}
