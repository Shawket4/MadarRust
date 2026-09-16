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
    orgs::handlers::extract_claims,
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
    pub user_id: Uuid,
    pub user_name: String,
    /// `None` for a person with no department set — grouped as "Unassigned".
    pub department_id: Option<Uuid>,
    pub department_name: Option<String>,
    pub present_days: i64,
    pub late_days: i64,
    pub absent_days: i64,
    pub total_late_minutes: i64,
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
    let claims = extract_claims(&req)?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;
    let org_id = scope_org(&req, &claims)?;
    // Only the branches the caller works at: a branch manager ranks their own
    // branches' staff, never the whole org's.
    let branches =
        crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, query.branch_id)
            .await?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, DisciplineRow>(
        r#"
        WITH per_user AS (
            SELECT a.user_id, u.name AS user_name,
                   sp.department_id, d.name AS department_name,
                   COUNT(*) FILTER (WHERE a.status = 'present') AS present_days,
                   COUNT(*) FILTER (WHERE a.status = 'late')    AS late_days,
                   COUNT(*) FILTER (WHERE a.status = 'absent')  AS absent_days,
                   COALESCE(SUM(a.late_minutes), 0)::bigint     AS total_late_minutes
              FROM attendance_records a
              JOIN users u ON u.id = a.user_id
              LEFT JOIN staff_profiles sp ON sp.user_id = a.user_id
              LEFT JOIN departments d ON d.id = sp.department_id
             WHERE a.org_id = $1
               AND a.business_date BETWEEN $2 AND $3
               AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4))
               AND (sp.employment_status IS NULL OR sp.employment_status = 'active')
             GROUP BY a.user_id, u.name, sp.department_id, d.name
        )
        SELECT *,
               RANK() OVER (
                   PARTITION BY department_id
                   ORDER BY absent_days ASC, late_days ASC, total_late_minutes ASC
               ) AS rank_in_department
          FROM per_user
         ORDER BY department_name IS NULL, department_name, rank_in_department, lower(user_name)
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
