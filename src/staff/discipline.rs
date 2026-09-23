//! The discipline report: staff ranked within their department by attendance
//! reliability, over a picked range.
//!
//! "Discipline" here means exactly what `attendance_records` already tracks —
//! late arrivals, their minutes, and absences — not a separate write-up or
//! incident system, which doesn't exist yet. Rank is fewest absences first,
//! ties broken by fewest lates, then least total late time; deliberately not
//! a single weighted score, since any weighting between "one absence" and
//! "three lates" would be an invented policy rather than a read of the facts.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    staff::{scope_org, validate_range},
};

/// Widest range one request may ask for — matches `attendance::attendance_summary`.
const MAX_RANGE_DAYS: i64 = 400;

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DisciplineQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Omit for every branch in the org.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Clone, sqlx::FromRow, ToSchema)]
pub struct DisciplineRow {
    pub employee_id: Uuid,
    pub employee_name: String,
    /// `None` for a person with no department set — grouped as "Unassigned".
    pub department_id: Option<Uuid>,
    pub department_name: Option<String>,
    pub present_days: i64,
    pub late_days: i64,
    pub absent_days: i64,
    pub total_late_minutes: i64,
    /// Colleagues' shifts this person covered, confirmed by a manager (CV-7).
    /// A cover is never a present day of the coverer's own.
    #[serde(default)]
    pub covers_given: i64,
    /// Covers still waiting for the manager.
    #[serde(default)]
    pub covers_pending: i64,
    /// Their own shifts a colleague covered (not rejected): the absence stays
    /// theirs (CV-6), this says someone stepped in.
    #[serde(default)]
    pub covered_by_others: i64,
    /// 1 = best in this department: fewest absences, then fewest lates, then
    /// least total late time. Ties share a rank (SQL `RANK()`), so a
    /// department where everyone has a clean record is all `1`s.
    pub rank_in_department: i64,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct DisciplineReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub rows: Vec<DisciplineRow>,
}

#[utoipa::path(
    get, path = "/staff/discipline-report", tag = "staff",
    params(DisciplineQuery),
    responses((status = 200, description = "Staff ranked by attendance discipline, per department", body = DisciplineReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn discipline_report(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<DisciplineQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::staff::principal::caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // Only the branches the caller reads attendance at: a branch manager ranks
    // their own branches' staff, never the whole org's.
    let branches = crate::staff::access::scope_at(
        pool.get_ref(),
        &claims,
        org_id,
        crate::authz::Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, DisciplineRow>(
        r#"
        WITH per_user AS (
            -- Days, not rows: a split day is one day (SC-11). A cover row is
            -- the coverer's cover, never their own present day (CV-7).
            SELECT a.employee_id, sp.name AS employee_name,
                   sp.department_id, d.name AS department_name,
                   COUNT(DISTINCT a.business_date) FILTER (
                       WHERE a.status = 'present' AND a.covered_employee_id IS NULL
                   ) AS present_days,
                   COUNT(DISTINCT a.business_date) FILTER (
                       WHERE a.status = 'late' AND a.covered_employee_id IS NULL
                   ) AS late_days,
                   COUNT(DISTINCT a.business_date) FILTER (
                       WHERE a.status = 'absent' AND a.covered_employee_id IS NULL
                   ) AS absent_days,
                   COALESCE(SUM(a.late_minutes) FILTER (
                       WHERE a.covered_employee_id IS NULL), 0)::bigint AS total_late_minutes,
                   COUNT(*) FILTER (
                       WHERE a.covered_employee_id IS NOT NULL AND a.cover_status = 'confirmed'
                   ) AS covers_given,
                   COUNT(*) FILTER (
                       WHERE a.covered_employee_id IS NOT NULL AND a.cover_status = 'pending'
                   ) AS covers_pending,
                   (SELECT COUNT(*) FROM attendance_records c
                     WHERE c.covered_employee_id = a.employee_id
                       AND c.business_date BETWEEN $2 AND $3
                       AND c.cover_status IS DISTINCT FROM 'rejected') AS covered_by_others
              FROM attendance_records a
              JOIN employees sp ON sp.id = a.employee_id
              LEFT JOIN departments d ON d.id = sp.department_id
             WHERE a.org_id = $1
               AND a.business_date BETWEEN $2 AND $3
               AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4))
               AND sp.employment_status = 'active'
             GROUP BY a.employee_id, sp.name, sp.department_id, d.name
        )
        SELECT *,
               RANK() OVER (
                   PARTITION BY department_id
                   ORDER BY absent_days ASC, late_days ASC, total_late_minutes ASC
               ) AS rank_in_department
          FROM per_user
         ORDER BY department_name IS NULL, department_name, rank_in_department, lower(employee_name)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&branches)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(DisciplineReport {
        from: query.from,
        to: query.to,
        rows,
    }))
}
