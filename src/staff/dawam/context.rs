//! One call the Dawam app boots from: who I am, where I work, the people I
//! can see, the org's pay settings, my limits and the shift templates.

use actix_web::{HttpRequest, HttpResponse};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::branches_of;
use crate::authz::{CAPS, Cap, LimitKey};
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::staff::attendance::{load_settings, require_active_profile};
use crate::staff::dawam::roster::WorkShiftBrief;

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
    pub user_id: Uuid,
    pub name: String,
    pub phone: Option<String>,
    /// `owner` · `manager` · `employee`
    pub role: String,
    pub branch_ids: Vec<Uuid>,
    pub gender: Option<String>,
    pub hire_date: Option<NaiveDate>,
    /// Only for people whose pay the caller may see.
    pub base_salary_piastres: Option<i64>,
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
}

#[derive(Serialize, ToSchema)]
pub struct StaffContext {
    pub user_id: Uuid,
    pub org_id: Uuid,
    pub org_name: String,
    /// The org's modules (`pos`, `dawam`); POS on means till punches (CL-13).
    pub modules: Vec<String>,
    /// `owner` · `manager` · `employee`
    pub role: String,
    /// The HR capabilities I hold (`hr.*` keys).
    pub caps: Vec<String>,
    /// My ceiling on a bonus/deduction before it waits for the owner; null = none.
    pub adjustment_limit_piastres: Option<i64>,
    pub advance_limit_percent: Option<i64>,
    pub branches: Vec<ContextBranch>,
    pub people: Vec<ContextPerson>,
    pub work_shifts: Vec<WorkShiftBrief>,
    pub settings: ContextSettings,
}

#[utoipa::path(
    get, path = "/staff/me/context", tag = "staff",
    responses((status = 200, body = StaffContext), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_context(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let pool = pool.get_ref();
    let org_id = require_active_profile(pool, user_id).await?;
    let eff = crate::authz::require::effective(pool, user_id, None).await?;
    let (is_owner, db_role, org_name, modules): (bool, String, String, Vec<String>) =
        sqlx::query_as(
            "SELECT u.is_owner OR u.role = 'org_admin', u.role::text, o.name, o.modules \
           FROM users u JOIN organizations o ON o.id = u.org_id WHERE u.id = $1",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    if !modules.iter().any(|m| m == "dawam") {
        // Switched off (PS-7): hidden, every record kept.
        return Err(AppError::Coded {
            status: 403,
            code: "DAWAM_OFF",
            reason: format!("Dawam is switched off for {org_name}."),
        });
    }
    let role = if is_owner {
        "owner"
    } else if db_role == "branch_manager" || eff.can(Cap::HrScheduleEdit) {
        "manager"
    } else {
        "employee"
    };
    let mine = branches_of(pool, user_id).await?;
    let branches: Vec<ContextBranch> = sqlx::query_as(
        "SELECT id, name, geo_radius_meters, latitude, longitude, timezone::text AS timezone FROM branches \
          WHERE org_id = $1 AND deleted_at IS NULL AND ($2 OR id = ANY($3)) ORDER BY name",
    )
    .bind(org_id)
    .bind(role == "owner")
    .bind(&mine)
    .fetch_all(pool)
    .await?;
    let branch_ids: Vec<Uuid> = branches.iter().map(|b| b.id).collect();
    let see_pay = role != "employee" && eff.can(Cap::HrPayrollRead);
    let people: Vec<ContextPerson> = sqlx::query_as(
        "SELECT u.id AS user_id, u.name, u.phone, \
                CASE WHEN u.is_owner OR u.role = 'org_admin' THEN 'owner' \
                     WHEN u.role = 'branch_manager' THEN 'manager' ELSE 'employee' END AS role, \
                COALESCE(ARRAY(SELECT a.branch_id FROM user_branch_assignments a \
                                WHERE a.user_id = u.id), '{}') AS branch_ids, \
                p.gender, p.hire_date, \
                CASE WHEN $3 OR u.id = $4 THEN p.base_salary_piastres END AS base_salary_piastres, \
                p.pay_method, CASE WHEN $3 OR u.id = $4 THEN p.pay_account END AS pay_account, \
                p.pref_time, p.cant_work_days, d.model AS device_model, d.first_seen_at AS device_since \
           FROM users u \
           JOIN staff_profiles p ON p.user_id = u.id AND p.employment_status = 'active' \
           LEFT JOIN staff_devices d ON d.user_id = u.id AND d.revoked_at IS NULL \
          WHERE u.org_id = $1 AND u.deleted_at IS NULL AND u.is_active \
            AND ($5 OR u.id = $4 OR u.is_owner OR u.role = 'org_admin' \
                 OR EXISTS (SELECT 1 FROM user_branch_assignments a \
                             WHERE a.user_id = u.id AND a.branch_id = ANY($2))) \
          ORDER BY lower(u.name)",
    )
    .bind(org_id)
    .bind(&branch_ids)
    .bind(see_pay)
    .bind(user_id)
    .bind(role == "owner")
    .fetch_all(pool)
    .await?;
    let work_shifts: Vec<WorkShiftBrief> = sqlx::query_as(
        "SELECT id, name, branch_id, start_time, end_time, crosses_midnight, grace_minutes \
           FROM work_shifts WHERE org_id = $1 AND is_active ORDER BY start_time",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let s = load_settings(pool, org_id, None).await?;
    let caps = CAPS
        .iter()
        .filter(|m| m.group == "hr" && eff.can(m.cap))
        .map(|m| m.key.to_string())
        .collect();
    Ok(HttpResponse::Ok().json(StaffContext {
        user_id,
        org_id,
        modules,
        org_name,
        role: role.into(),
        caps,
        adjustment_limit_piastres: eff
            .limits_of(Cap::HrAdjustmentsCreate)
            .get(LimitKey::MaxAmount),
        advance_limit_percent: eff
            .limits_of(Cap::HrAdvancesDecide)
            .get(LimitKey::MaxPercent),
        branches,
        people,
        work_shifts,
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
        },
    }))
}

async fn rules_saved(pool: &sqlx::PgPool, org_id: Uuid) -> Result<bool, AppError> {
    Ok(crate::staff::attendance::require_rules(pool, org_id)
        .await
        .is_ok())
}
