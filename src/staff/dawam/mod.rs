//! Dawam: what the staff app needs beyond the August staff module
//! (DAWAM_TARGET_SPEC, 2026-09-22). Requirement ids in comments (RO-4, CL-6…)
//! point at that spec.
//!
//! - `signin`   — WhatsApp code sign-in and the one live phone (RO-1..RO-5).
//! - `presence` — pings, flags, covers, overtime approval, punch for someone.
//! - `roster`   — the week: publish, open shifts, swaps, preferences,
//!   holidays and suggestions (SC-*).
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

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{Datelike, Duration, NaiveDate};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;

/// The app name Dawam registers push devices under (`push_devices.app`).
pub(crate) const PUSH_APP: &str = "dawam";
const PUSH_TITLE_KEY: &str = "staff.dawam_by_madar";

/// The header the staff app sends its device token in (RO-3).
pub const DEVICE_HEADER: &str = "x-staff-device";

/// A roster week starts on Saturday.
pub fn week_start(d: NaiveDate) -> NaiveDate {
    let back = (d.weekday().num_days_from_sunday() + 1) % 7; // Sat=0 … Fri=6
    d - Duration::days(i64::from(back))
}

pub(crate) fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// A message for someone's inbox (APP-6): a core i18n key and its arguments,
/// so it reads in the person's own language.
pub(crate) async fn notify(pool: &PgPool, org_id: Uuid, user_id: Uuid, key: &str, args: Value) {
    let res = sqlx::query(
        "INSERT INTO staff_notifications (org_id, user_id, key, args) VALUES ($1, $2, $3, $4)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(key)
    .bind(&args)
    .execute(pool)
    .await;
    match res {
        Ok(_) => crate::push::send(pool, user_id, &[PUSH_APP], PUSH_TITLE_KEY, key, &args),
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

/// `PUT /staff/me/push-token` — kept for old app builds; registers through
/// the same `push_devices` table as `PUT /push/token` (app = `"dawam"`).
#[utoipa::path(
    put, path = "/staff/me/push-token", tag = "staff", request_body = PushToken,
    operation_id = "set_staff_push_token",
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_push_token(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PushToken>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Unauthorized("No organization for this account".into()))?;
    let pool = pool.get_ref();
    require_device(&req, pool, user_id).await?;
    let locale = match body.locale.as_deref() {
        Some("en") => "en",
        _ => "ar",
    };
    let token = body.token.trim();
    if token.is_empty() {
        crate::push::revoke_all(pool, user_id, PUSH_APP).await?;
    } else {
        crate::push::register(pool, org_id, user_id, PUSH_APP, token, locale, "").await?;
    }
    Ok(HttpResponse::NoContent().finish())
}

/// The owner(s) of an org.
pub(crate) async fn owners(pool: &PgPool, org_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM users WHERE org_id = $1 AND role = 'org_admin' \
            AND is_active AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

/// Who manages a branch: its branch managers and the owners (RO-6).
pub(crate) async fn managers_of(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT u.id FROM users u \
          WHERE u.org_id = $1 AND u.is_active AND u.deleted_at IS NULL \
            AND (u.role = 'org_admin' \
                 OR (u.role = 'branch_manager' AND ($2::uuid IS NULL OR EXISTS ( \
                     SELECT 1 FROM user_branch_assignments a \
                      WHERE a.user_id = u.id AND a.branch_id = $2))))",
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?)
}

pub(crate) async fn notify_managers(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    except: Option<Uuid>,
    key: &str,
    args: Value,
) {
    if let Ok(ids) = managers_of(pool, org_id, branch_id).await {
        for id in ids.into_iter().filter(|id| Some(*id) != except) {
            notify(pool, org_id, id, key, args.clone()).await;
        }
    }
}

/// The branches a person works at.
pub(crate) async fn branches_of(pool: &PgPool, user_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 ORDER BY assigned_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub(crate) async fn user_name(pool: &PgPool, user_id: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// A punch or ping must come from the person's live phone (CL-1). People who
/// never signed in with a code (a password session) have no device row and
/// pass: the rule binds phones, it does not lock out the dashboard.
pub(crate) async fn require_device(
    req: &HttpRequest,
    pool: &PgPool,
    user_id: Uuid,
) -> Result<(), AppError> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT token_hash, revoked_at IS NULL FROM staff_devices WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let sent = req
        .headers()
        .get(DEVICE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(hash_token);
    let Some(sent) = sent else {
        return Err(AppError::Unauthorized(
            "This phone is not signed in for attendance. Sign in again with your code.".into(),
        ));
    };
    if rows.iter().any(|(h, live)| *live && *h == sent) {
        sqlx::query("UPDATE staff_devices SET last_seen_at = now() WHERE token_hash = $1")
            .bind(&sent)
            .execute(pool)
            .await?;
        return Ok(());
    }
    Err(AppError::Unauthorized(
        "This phone was signed out because you signed in on another one.".into(),
    ))
}

/// Revoke a person's phone at once (RO-4, RO-10).
pub async fn revoke_devices(pool: &PgPool, user_id: Uuid) -> Result<u64, AppError> {
    let n = sqlx::query(
        "UPDATE staff_devices SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    crate::push::revoke_all(pool, user_id, PUSH_APP).await?;
    Ok(n)
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
