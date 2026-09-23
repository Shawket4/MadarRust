//! Dawam reports (DSH-3): labour cost vs sales (only with POS on, DSH-4),
//! overtime and payroll history, and advances. Attendance and discipline is
//! `/staff/attendance/summary`.

use std::collections::BTreeMap;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::attendance::load_settings;
use crate::staff::principal::caller;
use crate::staff::rules::PayRates;

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
    let rows: Vec<(NaiveDate, Uuid, i64, Option<i32>, i32, i32)> = sqlx::query_as(
        "SELECT a.business_date, a.branch_id, p.base_salary_piastres, \
                (EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60)::int, \
                a.worked_minutes, a.overtime_minutes \
           FROM attendance_records a JOIN employees p ON p.id = a.employee_id \
          WHERE a.org_id = $1 AND a.business_date BETWEEN $2 AND $3 \
            AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4)) AND a.check_in_at IS NOT NULL",
    )
    .bind(org_id)
    .bind(q.from)
    .bind(q.to)
    .bind(scope.as_deref())
    .fetch_all(pool)
    .await?;
    let mut days: BTreeMap<(NaiveDate, Uuid), (Decimal, i64)> = BTreeMap::new();
    for (date, branch, salary, scheduled, worked, overtime) in rows {
        let rates = PayRates::from_base(
            salary,
            settings.working_days_per_month,
            i64::from(scheduled.unwrap_or(480).max(1)),
        );
        let premium = settings.overtime_day_multiplier - Decimal::ONE;
        let cost = rates.minutes_piastres(Decimal::from(worked))
            + rates.minutes_piastres(Decimal::from(overtime)) * premium.max(Decimal::ZERO);
        days.entry((date, branch)).or_default().0 += cost;
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
            let labour = labour.round().to_i64().unwrap_or(0);
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
    let salary: Vec<SalaryAdvanceRow> = sqlx::query_as(&format!(
        "SELECT a.id, a.employee_id, p.name AS employee_name, a.amount_piastres, a.remaining_piastres, \
                a.installments, a.status, (a.created_at AT TIME ZONE 'Africa/Cairo')::date AS given_on \
           FROM salary_advances a JOIN employees p ON p.id = a.employee_id \
          WHERE a.org_id = $1 AND (a.created_at AT TIME ZONE 'Africa/Cairo')::date BETWEEN $2 AND $3 \
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
