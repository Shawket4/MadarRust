//! Location privacy (AT-4, AT-5, CL-17).
//!
//! - **AT-5, consent.** The notice is accepted per employee AND phone, on the
//!   server (`staff_devices.privacy_accepted_at`). A new phone starts
//!   unaccepted, and so does a restored session on it until the person says
//!   yes: pings, and punches that carry a location, are refused with 403
//!   `PRIVACY_NOT_ACCEPTED` before that.
//! - **AT-4, the wipe.** Exact coordinates (the punches' and every ping's) are
//!   nulled once the payroll month they belong to is approved. Distances,
//!   inside/outside, times and flags are kept. It runs when the period is
//!   approved and again in the sweep as a safety net.

use actix_web::HttpResponse;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::staff::principal::Me;

/// The refusal before the notice is accepted on this phone.
pub fn not_accepted() -> AppError {
    AppError::Coded {
        status: 403,
        code: "PRIVACY_NOT_ACCEPTED",
        reason: "Accept the location notice in the app first.".into(),
    }
}

/// When this phone accepted the notice, if it did.
pub async fn accepted_at(
    pool: &PgPool,
    device_id: Uuid,
) -> Result<Option<DateTime<Utc>>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT privacy_accepted_at FROM staff_devices WHERE id = $1")
            .bind(device_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
    )
}

/// Location is collected only after the notice was accepted on this phone
/// (AT-5).
pub async fn require_accepted(pool: &PgPool, me: &Me) -> Result<(), AppError> {
    match accepted_at(pool, me.device_id).await? {
        Some(_) => Ok(()),
        None => Err(not_accepted()),
    }
}

#[derive(Serialize, ToSchema)]
pub struct PrivacyAccepted {
    pub accepted_at: DateTime<Utc>,
}

/// The employee accepted the location notice on this phone (AT-5). Kept on
/// the device row: a new phone asks again.
#[utoipa::path(
    post, path = "/staff/me/privacy", tag = "staff",
    operation_id = "accept_staff_privacy",
    responses((status = 200, body = PrivacyAccepted), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn accept(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let at: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE staff_devices SET privacy_accepted_at = COALESCE(privacy_accepted_at, now()) \
          WHERE id = $1 AND employee_id = $2 AND revoked_at IS NULL \
          RETURNING privacy_accepted_at",
    )
    .bind(me.device_id)
    .bind(me.employee_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(crate::staff::principal::device_revoked)?;
    Ok(HttpResponse::Ok().json(PrivacyAccepted { accepted_at: at }))
}

/// Null the exact coordinates of one org's days `from..=to` — the punches and
/// the pings — keeping distances, inside/outside, times and flags (AT-4).
/// Returns the rows touched.
pub async fn wipe_period_coordinates(
    pool: &PgPool,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    let records = sqlx::query(
        "UPDATE attendance_records SET \
             check_in_latitude = NULL, check_in_longitude = NULL, \
             check_out_latitude = NULL, check_out_longitude = NULL, updated_at = now() \
          WHERE org_id = $1 AND business_date BETWEEN $2 AND $3 \
            AND (check_in_latitude IS NOT NULL OR check_in_longitude IS NOT NULL \
                 OR check_out_latitude IS NOT NULL OR check_out_longitude IS NOT NULL)",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // A ping belongs to the month of the record it was taken on.
    let pings = sqlx::query(
        "UPDATE attendance_pings p SET latitude = NULL, longitude = NULL \
           FROM attendance_records a \
          WHERE a.id = p.attendance_record_id AND a.org_id = $1 \
            AND a.business_date BETWEEN $2 AND $3 \
            AND (p.latitude IS NOT NULL OR p.longitude IS NOT NULL)",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE payroll_periods SET coordinates_wiped_at = now() \
          WHERE org_id = $1 AND start_date = $2 AND end_date = $3 \
            AND status IN ('generated', 'paid', 'closed')",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(records + pings)
}

/// The sweep's safety net: every approved (or paid, or closed) period whose
/// coordinates were not wiped yet, and punches or pings added to an approved
/// month since (a late correction, a queued ping). Runs for every org: a
/// privacy duty, not a Dawam feature.
#[doc(hidden)]
pub async fn wipe_approved_months(pool: &PgPool) -> Result<u64, AppError> {
    let periods: Vec<(Uuid, NaiveDate, NaiveDate)> = sqlx::query_as(
        "SELECT p.org_id, p.start_date, p.end_date FROM payroll_periods p \
          WHERE p.status IN ('generated', 'paid', 'closed') \
            AND (p.coordinates_wiped_at IS NULL \
                 OR p.end_date >= (now() - INTERVAL '120 days')::date AND EXISTS (SELECT 1 FROM attendance_records a \
                             WHERE a.org_id = p.org_id \
                               AND a.business_date BETWEEN p.start_date AND p.end_date \
                               AND (a.check_in_latitude IS NOT NULL OR a.check_out_latitude IS NOT NULL \
                                    OR EXISTS (SELECT 1 FROM attendance_pings x \
                                                WHERE x.attendance_record_id = a.id \
                                                  AND x.latitude IS NOT NULL)))) \
          LIMIT 50",
    )
    .fetch_all(pool)
    .await?;
    let mut n = 0;
    for (org, from, to) in periods {
        // One month that fails is reported and skipped (E2E B-TEAM-4).
        match wipe_period_coordinates(pool, org, from, to).await {
            Ok(k) => n += k,
            Err(e) => crate::staff::jobs::skipped("purge_stale_coordinates", org, None, &e),
        }
    }
    if n > 0 {
        tracing::info!(rows = n, "wiped the coordinates of approved payroll months");
    }
    Ok(n)
}
