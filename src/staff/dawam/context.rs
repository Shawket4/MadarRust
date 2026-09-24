//! One call the Dawam app boots from: who I am, where I work, the people I
//! can see, the org's pay settings, my limits and the shift templates.

use actix_web::{HttpMessage, HttpRequest, HttpResponse};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::branches_of;
use crate::auth::jwt::Claims;
use crate::authz::{CAPS, Cap, EffectiveSet, LimitKey, Tier};
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::attendance::load_settings;
use crate::staff::dawam::roster::WorkShiftBrief;
use crate::staff::principal::Me;

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ContextBranch {
    pub id: Uuid,
    pub name: String,
    pub geo_radius_meters: Option<i32>,
    /// The fence centre, so the phone can say inside/outside offline.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub timezone: String,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ContextPerson {
    pub employee_id: Uuid,
    /// Their Madar account, when they have one.
    pub user_id: Option<Uuid>,
    pub name: String,
    pub phone: Option<String>,
    /// `owner` · `manager` · `employee` (from the linked account; an employee
    /// with no account is `employee`).
    pub role: String,
    pub branch_ids: Vec<Uuid>,
    pub gender: Option<String>,
    pub hire_date: Option<NaiveDate>,
    /// Only for people whose pay the caller may see.
    pub base_salary_piastres: Option<i64>,
    /// Their salary-advance cap, decided by the server (AV-5, AT-3); shown
    /// under the same visibility as the salary.
    pub advance_cap_piastres: Option<i64>,
    pub pay_method: String,
    pub pay_account: Option<String>,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
    pub device_model: Option<String>,
    pub device_since: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct ContextSettings {
    pub period_start_day: i16,
    pub overtime_mode: String,
    pub overtime_day_multiplier: Decimal,
    pub overtime_night_multiplier: Decimal,
    pub holiday_multiplier: Decimal,
    pub advance_cap_percent: Decimal,
    pub absence_deduction_days: Decimal,
    pub late_deduction_tiers: serde_json::Value,
    /// The business saved its rules; nobody clocks in before (RU-1, DSH-6).
    pub rules_saved: bool,
    /// When the rules were first saved; null until then. The sweep never
    /// marks absent (or charges) a shift that started before it (B-SETUP-5),
    /// so neither does the app (B-ONB-1).
    pub rules_saved_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct StaffContext {
    /// Who is signed in: the employee.
    pub employee_id: Uuid,
    /// Their Madar account, when they have one; manager acts go through it.
    pub user_id: Option<Uuid>,
    pub name: String,
    pub org_id: Uuid,
    pub org_name: String,
    /// The org's modules (`pos`, `dawam`); POS on means till punches (CL-13).
    pub modules: Vec<String>,
    /// `owner` · `manager` · `employee`
    pub role: String,
    /// The HR capabilities I hold (`hr.*` keys) — through my Madar account;
    /// empty for an employee with none. The app gates tabs on these (PM-4).
    pub caps: Vec<String>,
    /// The capabilities I hold at EVERY branch: the list `GET /authz/me`
    /// puts in `everywhere`, for the business-wide acts (the rules, payroll,
    /// public holidays: `hr.rules.edit`, D3). Empty without a Madar account.
    pub caps_everywhere: Vec<String>,
    /// My ceiling on a bonus before it waits for the owner; null = none.
    pub adjustment_limit_piastres: Option<i64>,
    /// My ceiling on a deduction (AD-5: separate from the bonus limit).
    pub deduction_limit_piastres: Option<i64>,
    /// My ceiling on an advance, as whole percent of the person's salary owed
    /// after it (the grant stores basis points); null = none.
    pub advance_limit_percent: Option<i64>,
    pub branches: Vec<ContextBranch>,
    pub people: Vec<ContextPerson>,
    pub work_shifts: Vec<WorkShiftBrief>,
    pub settings: ContextSettings,
    /// When THIS phone accepted the location notice; null = show it before
    /// any location is taken (AT-5). A new phone, or a restored session on
    /// one that never accepted, starts null.
    pub privacy_accepted_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[utoipa::path(
    get, path = "/staff/me/context", tag = "staff",
    responses((status = 200, body = StaffContext), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_context(
    req: HttpRequest,
    me: Me,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = me.org_id;
    // The linked account's powers, when the employee has one (StaffAuth put
    // its claims here only if it is active in this org).
    let claims: Option<Claims> = req.extensions().get::<Claims>().cloned();
    let eff = match me.user_id {
        Some(u) => crate::authz::require::effective(pool, u, None).await?,
        None => EffectiveSet::default(),
    };
    let (name, org_name, modules): (String, String, Vec<String>) = sqlx::query_as(
        "SELECT e.name, o.name, o.modules FROM employees e \
           JOIN organizations o ON o.id = e.org_id WHERE e.id = $1",
    )
    .bind(me.employee_id)
    .fetch_one(pool)
    .await?;
    let manages = [
        Cap::HrScheduleEdit,
        Cap::HrAttendanceEdit,
        Cap::HrLeaveEdit,
        Cap::HrAttendancePunchOthers,
    ]
    .iter()
    .any(|c| eff.can(*c));
    let role = if eff.owner {
        "owner"
    } else if manages {
        "manager"
    } else {
        "employee"
    };
    // Where I work, plus — for a manager — the branches I run.
    let mine = branches_of(pool, me.employee_id).await?;
    let run: Option<Vec<Uuid>> = match (&claims, role) {
        (Some(c), "owner" | "manager") => {
            match access::scope(pool, c, org_id, Cap::HrAttendanceRead).await {
                Ok(s) => s,
                Err(AppError::Forbidden(_)) => Some(vec![]),
                Err(e) => return Err(e),
            }
        }
        _ => Some(vec![]),
    };
    let all_branches = run.is_none();
    let mut shown: Vec<Uuid> = mine.clone();
    for b in run.clone().unwrap_or_default() {
        if !shown.contains(&b) {
            shown.push(b);
        }
    }
    let branches: Vec<ContextBranch> = sqlx::query_as(
        "SELECT id, name, geo_radius_meters, latitude, longitude, COALESCE(timezone::text, 'Africa/Cairo') AS timezone FROM branches \
          WHERE org_id = $1 AND deleted_at IS NULL AND ($2 OR id = ANY($3)) ORDER BY name",
    )
    .bind(org_id)
    .bind(all_branches)
    .bind(&shown)
    .fetch_all(pool)
    .await?;
    let branch_ids: Vec<Uuid> = branches.iter().map(|b| b.id).collect();
    // Pay: only where my account may read payroll.
    let pay: Option<Option<Vec<Uuid>>> = match &claims {
        Some(c) if role != "employee" => {
            match access::scope(pool, c, org_id, Cap::HrPayrollRead).await {
                Ok(s) => Some(s),
                Err(AppError::Forbidden(_)) => None,
                Err(e) => return Err(e),
            }
        }
        _ => None,
    };
    let people: Vec<ContextPerson> = sqlx::query_as(
        "SELECT e.id AS employee_id, e.user_id, e.name, e.phone, \
                CASE WHEN u.is_owner OR u.role = 'org_admin' THEN 'owner' \
                     WHEN u.role = 'branch_manager' THEN 'manager' ELSE 'employee' END AS role, \
                COALESCE(ARRAY(SELECT eb.branch_id FROM employee_branches eb \
                                WHERE eb.employee_id = e.id ORDER BY eb.assigned_at), '{}') AS branch_ids, \
                e.gender, e.hire_date, \
                CASE WHEN e.id = $4 OR ($3 AND ($6::uuid[] IS NULL OR EXISTS ( \
                         SELECT 1 FROM employee_branches pb WHERE pb.employee_id = e.id \
                            AND pb.branch_id = ANY($6)))) \
                     THEN e.base_salary_piastres END AS base_salary_piastres, \
                CASE WHEN e.id = $4 OR ($3 AND ($6::uuid[] IS NULL OR EXISTS ( \
                         SELECT 1 FROM employee_branches pb WHERE pb.employee_id = e.id \
                            AND pb.branch_id = ANY($6)))) \
                     THEN dawam_advance_cap(e.org_id, e.base_salary_piastres) END AS advance_cap_piastres, \
                e.pay_method, \
                CASE WHEN e.id = $4 OR $3 THEN e.pay_account END AS pay_account, \
                e.pref_time, e.cant_work_days, d.model AS device_model, d.first_seen_at AS device_since \
           FROM employees e \
           LEFT JOIN users u ON u.id = e.user_id \
           LEFT JOIN staff_devices d ON d.employee_id = e.id AND d.revoked_at IS NULL \
          WHERE e.org_id = $1 AND e.employment_status = 'active' \
            AND ($5 OR e.id = $4 \
                 OR EXISTS (SELECT 1 FROM employee_branches a \
                             WHERE a.employee_id = e.id AND a.branch_id = ANY($2))) \
          ORDER BY lower(e.name)",
    )
    .bind(org_id)
    .bind(&branch_ids)
    .bind(pay.is_some())
    .bind(me.employee_id)
    .bind(all_branches)
    .bind(pay.clone().flatten())
    .fetch_all(pool)
    .await?;
    // With each block's days and weekday times (offer it only on its days).
    let work_shifts: Vec<WorkShiftBrief> =
        crate::staff::dawam::roster::work_shifts_of(pool, org_id).await?;
    let s = load_settings(pool, org_id, None).await?;
    let caps = CAPS
        .iter()
        .filter(|m| m.group == "hr" && eff.can(m.cap))
        .map(|m| m.key.to_string())
        .collect();
    let caps_everywhere = match &claims {
        Some(c) => access::caps_everywhere(pool, c, org_id)
            .await?
            .iter()
            .filter(|c| c.meta().tier != Tier::Legacy)
            .map(|c| c.key().to_string())
            .collect(),
        None => Vec::new(),
    };
    Ok(HttpResponse::Ok().json(StaffContext {
        employee_id: me.employee_id,
        user_id: me.user_id,
        name,
        org_id,
        modules,
        org_name,
        role: role.into(),
        caps,
        caps_everywhere,
        adjustment_limit_piastres: eff
            .limits_of(Cap::HrAdjustmentsCreate)
            .get(LimitKey::MaxAmount),
        deduction_limit_piastres: eff
            .limits_of(Cap::HrDeductionsCreate)
            .get(LimitKey::MaxAmount),
        // Stored in basis points; the app reads whole percent (B-SETUP-4).
        advance_limit_percent: eff
            .limits_of(Cap::HrAdvancesDecide)
            .get(LimitKey::MaxPercent)
            .map(|bp| bp / 100),
        branches,
        people,
        work_shifts,
        privacy_accepted_at: super::privacy::accepted_at(pool, me.device_id).await?,
        settings: ContextSettings {
            period_start_day: s.period_start_day,
            overtime_mode: s.overtime_mode,
            overtime_day_multiplier: s.overtime_day_multiplier,
            overtime_night_multiplier: s.overtime_night_multiplier,
            holiday_multiplier: s.holiday_multiplier,
            advance_cap_percent: s.advance_cap_percent,
            absence_deduction_days: s.absence_deduction_days,
            late_deduction_tiers: s.late_deduction_tiers,
            rules_saved: rules_saved(pool, org_id).await?,
            rules_saved_at: s.rules_saved_at,
        },
    }))
}

async fn rules_saved(pool: &sqlx::PgPool, org_id: Uuid) -> Result<bool, AppError> {
    Ok(crate::staff::attendance::require_rules(pool, org_id)
        .await
        .is_ok())
}
