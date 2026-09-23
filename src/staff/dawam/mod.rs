//! Dawam: what the staff app needs beyond the August staff module
//! (DAWAM_TARGET_SPEC, 2026-09-22). Requirement ids in comments (RO-4, CL-6…)
//! point at that spec.
//!
//! - `signin`   — WhatsApp code sign-in and the one live phone (RO-1..RO-5).
//! - `presence` — pings, flags, covers, overtime approval, punch for someone.
//! - `roster`   — the week: publish, open shifts, swaps, preferences,
//!   holidays and coverage needs (SC-*).
//! - `suggest`  — roster suggestions, learning and their guardrails (SC-13).
//! - `pay`      — the current period, the employee's estimate, adjustments
//!   under limits, advances under the cap, paid-per-person, expense
//!   advances, and the inbox.

pub mod clock;
pub mod context;
pub mod engine;
pub mod pay;
pub mod presence;
pub mod reports;
pub mod roster;
pub mod signin;
pub mod suggest;

use actix_web::{HttpResponse, web};
use chrono::{Datelike, Duration, NaiveDate};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::push::Recipient;
use crate::staff::principal::Me;

/// The app name Dawam registers push devices under (`push_devices.app`).
pub(crate) const PUSH_APP: &str = "dawam";
const PUSH_TITLE_KEY: &str = "staff.dawam_by_madar";

/// The header the staff app sends its device token in (RO-3).
pub use crate::staff::principal::DEVICE_HEADER;

/// A roster week starts on Saturday.
pub fn week_start(d: NaiveDate) -> NaiveDate {
    let back = (d.weekday().num_days_from_sunday() + 1) % 7; // Sat=0 … Fri=6
    d - Duration::days(i64::from(back))
}

pub(crate) fn hash_token(token: &str) -> String {
    crate::staff::principal::hash_device_token(token)
}

/// A message for an employee's inbox (APP-6): a core i18n key and its
/// arguments, so it reads in the person's own language.
pub(crate) async fn notify(pool: &PgPool, org_id: Uuid, employee_id: Uuid, key: &str, args: Value) {
    let res = sqlx::query(
        "INSERT INTO staff_notifications (org_id, employee_id, key, args) VALUES ($1, $2, $3, $4)",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(key)
    .bind(&args)
    .execute(pool)
    .await;
    match res {
        Ok(_) => crate::push::send(
            pool,
            Recipient::Employee(employee_id),
            &[PUSH_APP],
            PUSH_TITLE_KEY,
            key,
            &args,
        ),
        Err(e) => tracing::warn!(error = %e, key, "staff notification not stored"),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct PushToken {
    /// The FCM registration token; empty turns pushes off for this phone.
    pub token: String,
    /// `ar` or `en`.
    #[serde(default)]
    pub locale: Option<String>,
}

/// `PUT /staff/me/push-token` — the staff app registers its phone for the
/// employee through the same `push_devices` table as `PUT /push/token`
/// (app = `"dawam"`).
#[utoipa::path(
    put, path = "/staff/me/push-token", tag = "staff", request_body = PushToken,
    operation_id = "set_staff_push_token",
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_push_token(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<PushToken>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let locale = match body.locale.as_deref() {
        Some("en") => "en",
        _ => "ar",
    };
    let token = body.token.trim();
    let who = Recipient::Employee(me.employee_id);
    if token.is_empty() {
        crate::push::revoke_all(pool, who, PUSH_APP).await?;
    } else {
        crate::push::register(pool, me.org_id, who, PUSH_APP, token, locale, "").await?;
    }
    Ok(HttpResponse::NoContent().finish())
}

/// The active employees linked to a user that passes `keep`, except one.
async fn linked_employees(pool: &PgPool, org_id: Uuid) -> Result<Vec<(Uuid, Uuid)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT e.id, e.user_id FROM employees e \
           JOIN users u ON u.id = e.user_id AND u.is_active AND u.deleted_at IS NULL \
          WHERE e.org_id = $1 AND e.employment_status = 'active'",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

/// The owners' employee records — who hears about acts waiting for the owner.
pub(crate) async fn owners(pool: &PgPool, org_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT e.id FROM employees e \
           JOIN users u ON u.id = e.user_id AND u.is_active AND u.deleted_at IS NULL \
                       AND (u.is_owner OR u.role = 'org_admin') \
          WHERE e.org_id = $1 AND e.employment_status = 'active'",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

/// Who manages a branch for `cap`: the employees whose linked user holds it
/// there (RO-6, RO-7), from the authz model — not from role names.
pub(crate) async fn managers_of(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    cap: Cap,
) -> Result<Vec<Uuid>, AppError> {
    let mut out = Vec::new();
    for (employee, user) in linked_employees(pool, org_id).await? {
        let eff = crate::authz::require::effective(pool, user, branch_id).await?;
        if eff.can(cap) {
            out.push(employee);
        }
    }
    Ok(out)
}

pub(crate) async fn notify_managers(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    cap: Cap,
    except: Option<Uuid>,
    key: &str,
    args: Value,
) {
    if let Ok(ids) = managers_of(pool, org_id, branch_id, cap).await {
        for id in ids.into_iter().filter(|id| Some(*id) != except) {
            notify(pool, org_id, id, key, args.clone()).await;
        }
    }
}

/// The branches an employee works at.
pub(crate) async fn branches_of(pool: &PgPool, employee_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    crate::staff::access::branches_of(pool, employee_id).await
}

pub(crate) async fn employee_name(pool: &PgPool, employee_id: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM employees WHERE id = $1")
        .bind(employee_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// A user's name, for acts recorded by user (who punched, who approved).
pub(crate) async fn user_name(pool: &PgPool, user_id: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Revoke an employee's phone at once (RO-4, RO-10): the device, and with it
/// every staff token minted for it, and its pushes.
pub async fn revoke_devices(pool: &PgPool, employee_id: Uuid) -> Result<u64, AppError> {
    let n = sqlx::query(
        "UPDATE staff_devices SET revoked_at = now() \
          WHERE employee_id = $1 AND revoked_at IS NULL",
    )
    .bind(employee_id)
    .execute(pool)
    .await?
    .rows_affected();
    crate::push::revoke_all(pool, Recipient::Employee(employee_id), PUSH_APP).await?;
    Ok(n)
}

/// The linked user was deactivated or deleted (RO-10): their employee's phone
/// is signed out.
pub async fn revoke_for_user(pool: &PgPool, user_id: Uuid) -> Result<u64, AppError> {
    let employee: Option<Uuid> = sqlx::query_scalar("SELECT id FROM employees WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    match employee {
        Some(e) => revoke_devices(pool, e).await,
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_start_on_saturday() {
        let sat = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        for i in 0..7 {
            assert_eq!(week_start(sat + Duration::days(i)), sat);
        }
        assert_eq!(week_start(sat - Duration::days(1)), sat - Duration::days(7));
    }
}
