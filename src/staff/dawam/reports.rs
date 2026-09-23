//! Dawam reports (DSH-3): labour cost vs sales (only with POS on, DSH-4),
//! overtime and payroll history, and advances. Attendance and discipline is
//! `/staff/attendance/summary`.

use std::collections::BTreeMap;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::attendance::load_settings;
use crate::staff::principal::caller;

const MAX_DAYS: i64 = 400;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReportQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

fn check(q: &ReportQuery) -> Result<(), AppError> {
    if q.to < q.from || (q.to - q.from).num_days() > MAX_DAYS {
        return Err(AppError::BadRequest(format!(
            "The range must be 0–{MAX_DAYS} days"
        )));
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub struct LabourDay {
    pub date: NaiveDate,
    pub branch_id: Uuid,
    /// From the clock: worked minutes at each person's minute rate, plus the
    /// overtime premium. The payslip stays the final word.
    pub labour_piastres: i64,
    /// Completed sales, net of refunds.
    pub sales_piastres: i64,
    /// Labour as a share of sales, basis points (null with no sales).
    pub labour_share_bp: Option<i64>,
}

/// Labour cost vs sales, per branch and day. Only when POS is on (DSH-4).
#[utoipa::path(
    get, path = "/staff/reports/labour-vs-sales", tag = "staff", params(ReportQuery),
    responses((status = 200, body = Vec<LabourDay>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn labour_vs_sales(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ReportQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    check(&q)?;
    // The branches whose pay the caller reads (RO-6).
    let scope = access::scope_at(pool, &claims, org_id, Cap::HrPayrollRead, q.branch_id).await?;
    if !super::roster::has_module(pool, org_id, "pos").await? {
        return Err(AppError::Forbidden(
            "Labour cost vs sales needs POS switched on.".into(),
        ));
    }
    let settings = load_settings(pool, org_id, None).await?;
    // Every worked shift at the person's minute rate that day, plus the
    // overtime PREMIUM as payroll prices it (the one function, AT-9): the
    // branch's rules, the shift's own rates, night minutes at the night rate.
    #[derive(sqlx::FromRow)]
    struct Row {
        business_date: NaiveDate,
        branch_id: Uuid,
        salary: i64,
        scheduled: Option<i32>,
        worked: i32,
        overtime: i32,
        night: i64,
        overtime_status: Option<String>,
        is_cover: bool,
        cover_status: Option<String>,
        shift_day: Option<Decimal>,
        shift_night: Option<Decimal>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.business_date, a.branch_id, \
                COALESCE((SELECT h.base_salary_piastres FROM employee_salary_history h \
                           WHERE h.employee_id = a.employee_id AND h.effective_from <= a.business_date \
                           ORDER BY h.effective_from DESC LIMIT 1), p.base_salary_piastres) AS salary, \
                (EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60)::int AS scheduled, \
                COALESCE(a.worked_minutes, 0) AS worked, COALESCE(a.overtime_minutes, 0) AS overtime, \
                COALESCE(dawam_night_minutes(a.scheduled_end_at, a.check_out_at, br.timezone::text, $5, $6), 0)::bigint AS night, \
                a.overtime_status, a.covered_employee_id IS NOT NULL AS is_cover, a.cover_status, \
                ws.ot_day_multiplier AS shift_day, ws.ot_night_multiplier AS shift_night \
           FROM attendance_records a JOIN employees p ON p.id = a.employee_id \
           JOIN branches br ON br.id = a.branch_id \
           LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id \
          WHERE a.org_id = $1 AND a.business_date BETWEEN $2 AND $3 \
            AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4)) AND a.check_in_at IS NOT NULL",
    )
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .bind(scope.as_deref())
    .bind(settings.night_start)
    .bind(settings.night_end)
    .fetch_all(pool)
    .await?;
    let mut by_branch: BTreeMap<Uuid, crate::staff::attendance::AttendanceSettings> = BTreeMap::new();
    let mut days: BTreeMap<(NaiveDate, Uuid), (i64, i64)> = BTreeMap::new();
    for r in rows {
        if !by_branch.contains_key(&r.branch_id) {
            by_branch.insert(r.branch_id, load_settings(pool, org_id, Some(r.branch_id)).await?);
        }
        let branch_rules = &by_branch[&r.branch_id];
        let rules = crate::staff::pricing::ShiftRules::from_settings(branch_rules, r.shift_day, r.shift_night);
        let scheduled = i64::from(r.scheduled.unwrap_or(480).max(1));
        if r.is_cover && r.cover_status.as_deref() != Some("confirmed") {
            continue;
        }
        let plain = crate::staff::pricing::minutes_piastres(
            r.salary,
            rules.working_days_per_month,
            scheduled,
            i64::from(r.worked),
        );
        let premium = if !r.is_cover
            && crate::staff::pricing::overtime_counts(&rules.overtime_mode, r.overtime_status.as_deref())
        {
            let total = i64::from(r.overtime.max(0));
            let night = r.night.clamp(0, total);
            crate::staff::pricing::overtime_piastres(
                r.salary,
                rules.working_days_per_month,
                scheduled,
                total - night,
                night,
                (rules.overtime_day_multiplier - Decimal::ONE).max(Decimal::ZERO),
                (rules.overtime_night_multiplier - Decimal::ONE).max(Decimal::ZERO),
            )
        } else {
            0
        };
        days.entry((r.business_date, r.branch_id)).or_default().0 += plain + premium;
    }
    let sales: Vec<(NaiveDate, Uuid, i64)> = sqlx::query_as(
        "SELECT (o.created_at AT TIME ZONE COALESCE(b.timezone::text, 'Africa/Cairo'))::date, \
                o.branch_id, \
                SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))::bigint \
           FROM orders o JOIN branches b ON b.id = o.branch_id \
           LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id \
          WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR o.branch_id = ANY($4)) \
            AND o.status NOT IN ('voided', 'refunded') \
            AND o.created_at >= $2::date - 1 AND o.created_at < $3::date + 2 \
          GROUP BY 1, 2",
    )
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .bind(scope.as_deref())
    .fetch_all(pool)
    .await?;
    for (date, branch, amount) in sales {
        if date >= q.from && date <= q.to {
            days.entry((date, branch)).or_default().1 += amount;
        }
    }
    let out: Vec<LabourDay> = days
        .into_iter()
        .map(|((date, branch_id), (labour, sales))| {
            LabourDay {
                date,
                branch_id,
                labour_piastres: labour,
                sales_piastres: sales,
                labour_share_bp: (sales > 0).then(|| labour * 10_000 / sales),
            }
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct PayrollHistoryRow {
    pub period_id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: String,
    pub people: i64,
    pub base_piastres: i64,
    pub overtime_minutes: i64,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    pub deductions_piastres: i64,
    pub advances_piastres: i64,
    pub net_piastres: i64,
}

/// Overtime and payroll history: one row per pay period in range.
#[utoipa::path(
    get, path = "/staff/reports/payroll-history", tag = "staff", params(ReportQuery),
    responses((status = 200, body = Vec<PayrollHistoryRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn payroll_history(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ReportQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    check(&q)?;
    // Whole-business totals: payroll read for every branch.
    access::require_everywhere(pool, &claims, org_id, Cap::HrPayrollRead).await?;
    let rows: Vec<PayrollHistoryRow> = sqlx::query_as(
        "SELECT pp.id AS period_id, pp.name, pp.start_date, pp.end_date, pp.status, \
                COUNT(s.id) AS people, \
                COALESCE(SUM(s.base_salary_piastres), 0)::bigint AS base_piastres, \
                COALESCE(SUM(s.overtime_minutes), 0)::bigint AS overtime_minutes, \
                COALESCE(SUM(s.overtime_piastres), 0)::bigint AS overtime_piastres, \
                COALESCE(SUM(s.bonuses_piastres), 0)::bigint AS bonuses_piastres, \
                COALESCE(SUM(s.deductions_piastres), 0)::bigint AS deductions_piastres, \
                COALESCE(SUM(s.advance_installment_piastres), 0)::bigint AS advances_piastres, \
                COALESCE(SUM(s.net_piastres), 0)::bigint AS net_piastres \
           FROM payroll_periods pp LEFT JOIN payslips s ON s.payroll_period_id = pp.id \
          WHERE pp.org_id = $1 AND pp.end_date >= $2 AND pp.start_date <= $3 \
          GROUP BY pp.id ORDER BY pp.start_date DESC",
    )
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .fetch_all(pool)
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct SalaryAdvanceRow {
    pub id: Uuid,
    pub employee_id: Uuid,
    pub employee_name: String,
    pub amount_piastres: i64,
    pub remaining_piastres: i64,
    pub installments: i32,
    /// `pending` · `approved` · `rejected` · …
    pub status: String,
    pub given_on: NaiveDate,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ExpenseAdvanceRow {
    pub id: Uuid,
    pub employee_id: Uuid,
    pub employee_name: String,
    pub amount_piastres: i64,
    pub purpose: String,
    /// `safe` · `bank` · `till`
    pub via: String,
    pub given_on: NaiveDate,
}

#[derive(Serialize, ToSchema)]
pub struct AdvancesReport {
    /// Against salary, repaid by installments.
    pub salary: Vec<SalaryAdvanceRow>,
    /// Cash for shop purchases: a log, never deducted (AV-7).
    pub expense: Vec<ExpenseAdvanceRow>,
    pub salary_given_piastres: i64,
    pub salary_outstanding_piastres: i64,
    pub expense_given_piastres: i64,
}

/// Advances given in range, and what is still owed.
#[utoipa::path(
    get, path = "/staff/reports/advances", tag = "staff", params(ReportQuery),
    responses((status = 200, body = AdvancesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn advances(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ReportQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    check(&q)?;
    let scope = access::scope_at(pool, &claims, org_id, Cap::HrPayrollRead, q.branch_id).await?;
    // Dated in the person's branch's zone, never the server's (AT-1).
    let salary: Vec<SalaryAdvanceRow> = sqlx::query_as(&format!(
        "SELECT a.id, a.employee_id, p.name AS employee_name, a.amount_piastres, a.remaining_piastres, \
                a.installments, a.status, \
                (COALESCE(a.decided_at, a.created_at) AT TIME ZONE COALESCE(bz.tz, 'Africa/Cairo'))::date AS given_on \
           FROM salary_advances a JOIN employees p ON p.id = a.employee_id \
           LEFT JOIN LATERAL (SELECT b.timezone::text AS tz FROM employee_branches eb \
                               JOIN branches b ON b.id = eb.branch_id \
                              WHERE eb.employee_id = a.employee_id ORDER BY eb.assigned_at LIMIT 1) bz ON true \
          WHERE a.org_id = $1 \
            AND (COALESCE(a.decided_at, a.created_at) AT TIME ZONE COALESCE(bz.tz, 'Africa/Cairo'))::date BETWEEN $2 AND $3 \
            AND a.status IN ('approved', 'settled') \
            AND {} \
          ORDER BY a.created_at DESC",
        access::in_scope("a.employee_id", 4)
    ))
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .bind(scope.as_deref())
    .fetch_all(pool)
    .await?;
    let expense: Vec<ExpenseAdvanceRow> = sqlx::query_as(
        "SELECT e.id, e.employee_id, p.name AS employee_name, e.amount_piastres, e.purpose, e.via, e.given_on \
           FROM expense_advances e JOIN employees p ON p.id = e.employee_id \
          WHERE e.org_id = $1 AND e.given_on BETWEEN $2 AND $3 \
            AND ($4::uuid[] IS NULL OR e.branch_id = ANY($4)) \
          ORDER BY e.given_on DESC, e.created_at DESC",
    )
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .bind(scope.as_deref())
    .fetch_all(pool)
    .await?;
    let live = |s: &SalaryAdvanceRow| s.status == "approved" || s.status == "settled";
    Ok(HttpResponse::Ok().json(AdvancesReport {
        salary_given_piastres: salary
            .iter()
            .filter(|s| live(s))
            .map(|s| s.amount_piastres)
            .sum(),
        salary_outstanding_piastres: salary
            .iter()
            .filter(|s| s.status == "approved")
            .map(|s| s.remaining_piastres)
            .sum(),
        expense_given_piastres: expense.iter().map(|e| e.amount_piastres).sum(),
        salary,
        expense,
    }))
}
