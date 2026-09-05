//! Host surfaces: list / create / edit / cancel / no-show / seat / complete,
//! availability for a day, and a small stats endpoint. Every mutation happens
//! in one transaction with the booking row locked, publishes on the `bookings`
//! topic after commit, and messages the guest best-effort.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::availability::{Ground, SlotAvailability, slot_starts};
use super::model::{BookingView, booking_view, list_views};
use super::settings::{BookingSettings, load_settings};
use super::whatsapp::{Kind, notify};
use super::{branch_tz, publish_booking};
use crate::delivery::{normalize_phone, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;
use crate::realtime::hub::BranchEventHub;
use crate::reservations::resolve_branch_org;
use crate::sync::ActingContext;

/// A service day runs 05:00 → 05:00 local, so a 00:30 booking belongs to the
/// evening before it.
const DAY_CUTOFF_HOUR: u32 = 5;

pub(crate) fn service_day_bounds(
    tz: chrono_tz::Tz,
    date: NaiveDate,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let cutoff = NaiveTime::from_hms_opt(DAY_CUTOFF_HOUR, 0, 0).unwrap_or_default();
    let start = tz
        .from_local_datetime(&date.and_time(cutoff))
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&date.and_time(cutoff)));
    (start, start + Duration::days(1))
}

/// Today's service date in the branch zone.
pub(crate) fn service_today(tz: chrono_tz::Tz, now: DateTime<Utc>) -> NaiveDate {
    let local = now.with_timezone(&tz);
    let d = local.date_naive();
    if local.time() < NaiveTime::from_hms_opt(DAY_CUTOFF_HOUR, 0, 0).unwrap_or_default() {
        d - Duration::days(1)
    } else {
        d
    }
}

/// An exclusion-constraint hit reads as "already booked", not a 500 or a
/// generic conflict.
fn map_claim_error(e: sqlx::Error) -> AppError {
    let exclusion =
        matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23P01"));
    if exclusion {
        return AppError::Conflict("That table is already booked for this time".into());
    }
    AppError::from(e)
}

/// `(branch_id, status, party_size, starts_at, ends_at, section_id)` of a locked booking row.
type LockedBooking = (
    Uuid,
    String,
    i16,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<Uuid>,
);

async fn insert_claims(
    tx: &mut Transaction<'_, Postgres>,
    booking_id: Uuid,
    table_ids: &[Uuid],
) -> Result<(), AppError> {
    for t in table_ids {
        // org_id / during / active are filled by the BEFORE INSERT trigger.
        sqlx::query("INSERT INTO booking_tables (booking_id, table_id) VALUES ($1, $2)")
            .bind(booking_id)
            .bind(t)
            .execute(&mut **tx)
            .await
            .map_err(map_claim_error)?;
    }
    Ok(())
}

/// The tables must exist on the branch and be active; returns their seat sum.
async fn validate_tables(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    table_ids: &[Uuid],
) -> Result<i32, AppError> {
    if table_ids.is_empty() {
        return Ok(0);
    }
    let rows: Vec<(Uuid, i16)> = sqlx::query_as(
        "SELECT id, seats FROM branch_tables WHERE branch_id = $1 AND is_active AND id = ANY($2)",
    )
    .bind(branch_id)
    .bind(table_ids)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() != table_ids.len() {
        return Err(AppError::BadRequest(
            "One or more tables are not on this branch".into(),
        ));
    }
    Ok(rows.iter().map(|(_, s)| *s as i32).sum())
}

fn validate_party(settings: &BookingSettings, party: i32, host: bool) -> Result<(), AppError> {
    if party <= 0 {
        return Err(AppError::BadRequest("party_size must be at least 1".into()));
    }
    if !host && (party < settings.min_party as i32 || party > settings.max_party as i32) {
        return Err(AppError::BadRequest(format!(
            "We take parties of {} to {} online — please call for larger groups",
            settings.min_party, settings.max_party
        )));
    }
    Ok(())
}

fn clean_locale(l: Option<&str>) -> String {
    match l.map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s.starts_with("ar") => "ar".into(),
        _ => "en".into(),
    }
}

// ── List / get ────────────────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListBookingsQuery {
    pub branch_id: Uuid,
    /// Service date (`YYYY-MM-DD`, branch-local, 05:00→05:00). Defaults to today.
    #[param(value_type = Option<String>)]
    pub date: Option<NaiveDate>,
    /// Explicit window (overrides `date`).
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Only `confirmed` / `seated`.
    #[serde(default)]
    pub active: Option<bool>,
    pub status: Option<String>,
}

#[utoipa::path(get, path = "/bookings", tag = "bookings", params(ListBookingsQuery),
    responses((status = 200, body = Vec<BookingView>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_bookings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListBookingsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let (from, to) = match (query.from, query.to) {
        (Some(f), Some(t)) => (f, t),
        _ => {
            let tz = branch_tz(pool.get_ref(), query.branch_id).await?;
            let date = query.date.unwrap_or_else(|| service_today(tz, Utc::now()));
            service_day_bounds(tz, date)
        }
    };
    let rows = list_views(
        pool.get_ref(),
        query.branch_id,
        from,
        to,
        query.active.unwrap_or(false),
        query.status.as_deref(),
    )
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

async fn load_for_access(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    id: Uuid,
) -> Result<BookingView, AppError> {
    let view = booking_view(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Booking not found".into()))?;
    require_branch_access(pool, claims, view.branch_id).await?;
    Ok(view)
}

#[utoipa::path(get, path = "/bookings/{id}", tag = "bookings",
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    let view = load_for_access(pool.get_ref(), &claims, *id).await?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Create ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateBookingRequest {
    pub branch_id: Uuid,
    pub party_size: i32,
    pub starts_at: DateTime<Utc>,
    /// Defaults to the branch's `default_duration_minutes`.
    #[serde(default)]
    pub duration_minutes: Option<i32>,
    pub guest_name: String,
    pub guest_phone: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// Seating preference for the auto-assigner.
    #[serde(default)]
    pub section_id: Option<Uuid>,
    /// Explicit tables (skips auto-assignment). Empty = deliberately none.
    #[serde(default)]
    pub table_ids: Option<Vec<Uuid>>,
    /// `en` | `ar` for the guest's messages.
    #[serde(default)]
    pub locale: Option<String>,
    /// Create even when no table fits (the booking shows as "needs a table").
    #[serde(default)]
    pub force: Option<bool>,
    /// Send the WhatsApp confirmation (default true).
    #[serde(default)]
    pub send_confirmation: Option<bool>,
}

/// Shared by the host route and any future replay: inserts the booking + claims.
/// Returns the new id.
pub(crate) async fn create_booking_inner(
    pool: &PgPool,
    body: &CreateBookingRequest,
    created_by: Option<Uuid>,
    source: &str,
    phone_verified: bool,
    host: bool,
) -> Result<Uuid, AppError> {
    let settings = load_settings(pool, body.branch_id).await?;
    validate_party(&settings, body.party_size, host)?;
    let name = body.guest_name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("guest_name is required".into()));
    }
    let phone = normalize_phone(&body.guest_phone)?;
    let duration = body
        .duration_minutes
        .unwrap_or(settings.default_duration_minutes as i32);
    if !(15..=600).contains(&duration) {
        return Err(AppError::BadRequest(
            "duration_minutes must be 15..600".into(),
        ));
    }
    let ends_at = body.starts_at + Duration::minutes(duration as i64);
    let now = Utc::now();
    if ends_at <= now {
        return Err(AppError::BadRequest("That time has already passed".into()));
    }
    let org_id = resolve_branch_org(pool, body.branch_id).await?;

    let mut tx = pool.begin().await?;
    let table_ids: Vec<Uuid> = match &body.table_ids {
        Some(ids) => {
            let seats = validate_tables(&mut tx, body.branch_id, ids).await?;
            if !ids.is_empty() && seats < body.party_size && !body.force.unwrap_or(false) {
                return Err(AppError::Conflict(format!(
                    "Those tables seat {seats}, the party is {}",
                    body.party_size
                )));
            }
            ids.clone()
        }
        None => {
            let ground = Ground::load(pool, body.branch_id, body.starts_at, ends_at, None).await?;
            match ground.pick(
                &settings,
                now,
                body.starts_at,
                ends_at,
                body.party_size,
                body.section_id,
            ) {
                Some(ids) => ids,
                None if body.force.unwrap_or(false) => Vec::new(),
                None => {
                    return Err(AppError::Conflict(format!(
                        "No table seats {} at that time",
                        body.party_size
                    )));
                }
            }
        }
    };
    if let Some(cap) = settings.max_covers_per_slot {
        let booked =
            super::availability::covers_at(&mut *tx, body.branch_id, body.starts_at, None).await?;
        if !host && booked + body.party_size as i64 > cap as i64 {
            return Err(AppError::Conflict("That time is fully booked".into()));
        }
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, \
             guest_phone, phone_verified, notes, source, locale, section_id, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(body.party_size as i16)
    .bind(body.starts_at)
    .bind(ends_at)
    .bind(name)
    .bind(&phone)
    .bind(phone_verified)
    .bind(
        body.notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(source)
    .bind(clean_locale(body.locale.as_deref()))
    .bind(body.section_id)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;
    insert_claims(&mut tx, id, &table_ids).await?;
    tx.commit().await?;
    Ok(id)
}

#[utoipa::path(post, path = "/bookings", tag = "bookings", request_body = CreateBookingRequest,
    responses((status = 201, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    body: web::Json<CreateBookingRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let id = create_booking_inner(
        pool.get_ref(),
        &body,
        Some(claims.user_id()),
        "host",
        false,
        true,
    )
    .await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.created", id).await;
    let view = booking_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    if body.send_confirmation.unwrap_or(true) {
        notify(pool.get_ref(), &view, Kind::Confirmed).await;
    }
    Ok(HttpResponse::Created().json(view))
}

// ── Update ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema, Default)]
pub struct UpdateBookingRequest {
    #[serde(default)]
    pub party_size: Option<i32>,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub duration_minutes: Option<i32>,
    #[serde(default)]
    pub guest_name: Option<String>,
    #[serde(default)]
    pub guest_phone: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    /// Present = reassign to exactly these tables (empty = unassign).
    #[serde(default)]
    pub table_ids: Option<Vec<Uuid>>,
    /// Keep the booking when no table fits after a move (default true).
    #[serde(default)]
    pub force: Option<bool>,
}

/// Core of a host or guest edit. Re-checks capacity when the window or party
/// changes: keeps the current tables when they still fit and are free, else
/// auto-assigns again. `host = false` applies the online party limits and
/// refuses an edit that leaves the booking without a table.
pub(crate) async fn update_booking_inner(
    pool: &PgPool,
    id: Uuid,
    body: &UpdateBookingRequest,
    host: bool,
) -> Result<bool, AppError> {
    let mut tx = pool.begin().await?;
    let cur: Option<LockedBooking> = sqlx::query_as(
        "SELECT branch_id, status::text, party_size, starts_at, ends_at, section_id \
         FROM bookings WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, party0, starts0, ends0, section0)) = cur else {
        return Err(AppError::NotFound("Booking not found".into()));
    };
    if !matches!(status.as_str(), "confirmed" | "seated") {
        return Err(AppError::Conflict(format!(
            "A {status} booking cannot be changed"
        )));
    }
    let settings = load_settings(&mut *tx, branch_id).await?;
    let party = body.party_size.unwrap_or(party0 as i32);
    validate_party(&settings, party, host)?;
    let starts_at = body.starts_at.unwrap_or(starts0);
    let duration = body
        .duration_minutes
        .map(|d| d as i64)
        .unwrap_or((ends0 - starts0).num_minutes());
    if !(15..=600).contains(&duration) {
        return Err(AppError::BadRequest(
            "duration_minutes must be 15..600".into(),
        ));
    }
    let ends_at = starts_at + Duration::minutes(duration);
    let now = Utc::now();
    if ends_at <= now {
        return Err(AppError::BadRequest("That time has already passed".into()));
    }
    let section_id = body.section_id.or(section0);
    let window_moved = starts_at != starts0 || ends_at != ends0;
    let time_changed = starts_at != starts0;

    // Decide the tables.
    let current: Vec<(Uuid, i16)> = sqlx::query_as(
        "SELECT bt.table_id, t.seats FROM booking_tables bt JOIN branch_tables t ON t.id = bt.table_id \
         WHERE bt.booking_id = $1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let new_tables: Vec<Uuid> = match &body.table_ids {
        Some(ids) => {
            let seats = validate_tables(&mut tx, branch_id, ids).await?;
            if !ids.is_empty() && seats < party && !body.force.unwrap_or(true) {
                return Err(AppError::Conflict(format!(
                    "Those tables seat {seats}, the party is {party}"
                )));
            }
            ids.clone()
        }
        None if !window_moved
            && party <= current.iter().map(|(_, s)| *s as i32).sum::<i32>()
            && !current.is_empty() =>
        {
            current.iter().map(|(t, _)| *t).collect()
        }
        None => {
            let ground = Ground::load(pool, branch_id, starts_at, ends_at, Some(id)).await?;
            let cur_ids: Vec<Uuid> = current.iter().map(|(t, _)| *t).collect();
            let cur_seats: i32 = current.iter().map(|(_, s)| *s as i32).sum();
            let cur_free = !cur_ids.is_empty()
                && cur_seats >= party
                && ground.claims.iter().all(|c| {
                    !cur_ids.contains(&c.table_id)
                        || c.ends_at <= starts_at
                        || c.starts_at >= ends_at
                });
            if cur_free {
                cur_ids
            } else {
                match ground.pick(&settings, now, starts_at, ends_at, party, section_id) {
                    Some(ids) => ids,
                    None if host && body.force.unwrap_or(true) => Vec::new(),
                    None => {
                        return Err(AppError::Conflict(format!(
                            "No table seats {party} at that time"
                        )));
                    }
                }
            }
        }
    };
    if let Some(cap) = settings.max_covers_per_slot {
        let booked =
            super::availability::covers_at(&mut *tx, branch_id, starts_at, Some(id)).await?;
        if !host && booked + party as i64 > cap as i64 {
            return Err(AppError::Conflict("That time is fully booked".into()));
        }
    }

    // Drop the claims first so the window move never trips the exclusion
    // against the booking's own old range, then re-claim.
    sqlx::query("DELETE FROM booking_tables WHERE booking_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let phone = match &body.guest_phone {
        Some(p) => Some(normalize_phone(p)?),
        None => None,
    };
    let name = body
        .guest_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.guest_name.is_some() && name.is_none() {
        return Err(AppError::BadRequest("guest_name cannot be empty".into()));
    }
    sqlx::query(
        "UPDATE bookings SET party_size = $2, starts_at = $3, ends_at = $4, section_id = $5, \
             guest_name = COALESCE($6, guest_name), guest_phone = COALESCE($7, guest_phone), \
             notes = CASE WHEN $8 THEN $9 ELSE notes END, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(party as i16)
    .bind(starts_at)
    .bind(ends_at)
    .bind(section_id)
    .bind(name)
    .bind(phone)
    .bind(body.notes.is_some())
    .bind(
        body.notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .execute(&mut *tx)
    .await?;
    insert_claims(&mut tx, id, &new_tables).await?;
    tx.commit().await?;
    Ok(time_changed || party != party0 as i32)
}

#[utoipa::path(patch, path = "/bookings/{id}", tag = "bookings", request_body = UpdateBookingRequest,
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn update_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateBookingRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    load_for_access(pool.get_ref(), &claims, *id).await?;
    let material = update_booking_inner(pool.get_ref(), *id, &body, true).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", *id).await;
    let view = booking_view(pool.get_ref(), *id)
        .await?
        .ok_or(AppError::Internal)?;
    if material {
        notify(pool.get_ref(), &view, Kind::Changed).await;
    }
    Ok(HttpResponse::Ok().json(view))
}

// ── Status transitions ────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema, Default)]
pub struct CancelBookingRequest {
    #[serde(default)]
    pub reason: Option<String>,
    /// Message the guest (default true).
    #[serde(default)]
    pub notify_guest: Option<bool>,
}

/// `confirmed`/`seated` → `cancelled`. Returns false when it already was.
pub(crate) async fn cancel_inner(
    pool: &PgPool,
    id: Uuid,
    by: &str,
    reason: Option<&str>,
) -> Result<bool, AppError> {
    let n = sqlx::query(
        "UPDATE bookings SET status = 'cancelled', cancelled_at = now(), cancelled_by = $2, \
             cancel_reason = $3, updated_at = now() \
         WHERE id = $1 AND status IN ('confirmed', 'seated')",
    )
    .bind(id)
    .bind(by)
    .bind(reason.map(str::trim).filter(|s| !s.is_empty()))
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

#[utoipa::path(post, path = "/bookings/{id}/cancel", tag = "bookings", request_body = CancelBookingRequest,
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn cancel_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: Option<web::Json<CancelBookingRequest>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    let before = load_for_access(pool.get_ref(), &claims, *id).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    if !before.is_active() {
        return Err(AppError::Conflict(format!(
            "Booking is already {}",
            before.status
        )));
    }
    cancel_inner(pool.get_ref(), *id, "host", body.reason.as_deref()).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", *id).await;
    let view = booking_view(pool.get_ref(), *id)
        .await?
        .ok_or(AppError::Internal)?;
    if body.notify_guest.unwrap_or(true) {
        notify(pool.get_ref(), &view, Kind::Cancelled).await;
    }
    Ok(HttpResponse::Ok().json(view))
}

/// `confirmed`/`seated` → `no_show`. Shared with `/sync/replay`.
pub(crate) async fn no_show_inner(pool: &PgPool, id: Uuid) -> Result<HttpResponse, AppError> {
    let n = sqlx::query(
        "UPDATE bookings SET status = 'no_show', no_show_at = now(), updated_at = now() \
         WHERE id = $1 AND status IN ('confirmed', 'seated')",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    let view = booking_view(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Booking not found".into()))?;
    if n == 0 && view.status != "no_show" {
        return Err(AppError::Conflict(format!(
            "Booking is already {}",
            view.status
        )));
    }
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/bookings/{id}/no-show", tag = "bookings",
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn no_show_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    load_for_access(pool.get_ref(), &claims, *id).await?;
    let resp = no_show_inner(pool.get_ref(), *id).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", *id).await;
    Ok(resp)
}

#[derive(Deserialize, ToSchema, Default)]
pub struct SeatBookingRequest {
    /// Seat the party on these tables instead (a walk-in took theirs, or the
    /// host prefers another). Omit to keep the claim.
    #[serde(default)]
    pub table_ids: Option<Vec<Uuid>>,
}

/// `confirmed` → `seated` (idempotent when already seated). The ticket the
/// POS fires afterwards carries `booking_id` and links itself. Shared with
/// `/sync/replay` so a waiter can seat a party while the cloud is unreachable.
pub(crate) async fn seat_inner(
    pool: &PgPool,
    id: Uuid,
    body: &SeatBookingRequest,
    _actor: &ActingContext,
) -> Result<HttpResponse, AppError> {
    let mut tx = pool.begin().await?;
    let cur: Option<(Uuid, String)> =
        sqlx::query_as("SELECT branch_id, status::text FROM bookings WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((branch_id, status)) = cur else {
        return Err(AppError::NotFound("Booking not found".into()));
    };
    if !matches!(status.as_str(), "confirmed" | "seated") {
        return Err(AppError::Conflict(format!("Booking is {status}")));
    }
    if let Some(ids) = &body.table_ids {
        validate_tables(&mut tx, branch_id, ids).await?;
        sqlx::query("DELETE FROM booking_tables WHERE booking_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        insert_claims(&mut tx, id, ids).await?;
    }
    sqlx::query(
        "UPDATE bookings SET status = 'seated', seated_at = COALESCE(seated_at, now()), updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let view = booking_view(pool, id).await?.ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/bookings/{id}/seat", tag = "bookings", request_body = SeatBookingRequest,
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn seat_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: Option<web::Json<SeatBookingRequest>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    load_for_access(pool.get_ref(), &claims, *id).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    let actor = ActingContext::live(&claims)?;
    let resp = seat_inner(pool.get_ref(), *id, &body, &actor).await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", *id).await;
    Ok(resp)
}

#[utoipa::path(post, path = "/bookings/{id}/complete", tag = "bookings",
    params(("id" = Uuid, Path, description = "Booking ID")),
    responses((status = 200, body = BookingView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn complete_booking(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "update").await?;
    let before = load_for_access(pool.get_ref(), &claims, *id).await?;
    if before.status != "seated" && before.status != "completed" {
        return Err(AppError::Conflict(
            "Only a seated booking can be completed".into(),
        ));
    }
    sqlx::query(
        "UPDATE bookings SET status = 'completed', completed_at = COALESCE(completed_at, now()), \
             updated_at = now() WHERE id = $1 AND status = 'seated'",
    )
    .bind(*id)
    .execute(pool.get_ref())
    .await?;
    publish_booking(pool.get_ref(), hub.get_ref(), "booking.changed", *id).await;
    let view = booking_view(pool.get_ref(), *id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Ticket hooks (called from the tickets module inside its transaction) ──────

/// Link a fired ticket to its booking and mark the party seated. The booking
/// must be active on the same branch; anything else is a 409 so a stale POS
/// cannot seat a cancelled booking by accident.
pub(crate) async fn link_ticket(
    tx: &mut Transaction<'_, Postgres>,
    booking_id: Uuid,
    branch_id: Uuid,
    ticket_id: Uuid,
) -> Result<(), AppError> {
    let cur: Option<(Uuid, String)> =
        sqlx::query_as("SELECT branch_id, status::text FROM bookings WHERE id = $1 FOR UPDATE")
            .bind(booking_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((b, status)) = cur else {
        return Err(AppError::NotFound("Booking not found".into()));
    };
    if b != branch_id {
        return Err(AppError::Conflict(
            "Booking belongs to another branch".into(),
        ));
    }
    if !matches!(status.as_str(), "confirmed" | "seated") {
        return Err(AppError::Conflict(format!("Booking is {status}")));
    }
    sqlx::query(
        "UPDATE bookings SET status = 'seated', seated_at = COALESCE(seated_at, now()), \
             open_ticket_id = $2, updated_at = now() WHERE id = $1",
    )
    .bind(booking_id)
    .bind(ticket_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The ticket settled: its booking is done. Returns the booking id when one
/// moved, so the caller can publish after its own commit.
pub(crate) async fn complete_by_ticket<'e, E>(
    exec: E,
    ticket_id: Uuid,
) -> Result<Option<Uuid>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE bookings SET status = 'completed', completed_at = now(), updated_at = now() \
         WHERE open_ticket_id = $1 AND status = 'seated' RETURNING id",
    )
    .bind(ticket_id)
    .fetch_optional(exec)
    .await?;
    Ok(id)
}

// ── Availability + stats ──────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AvailabilityQuery {
    pub branch_id: Uuid,
    #[param(value_type = String)]
    pub date: NaiveDate,
    pub party_size: i32,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    /// Ignore this booking's own claims (when moving it).
    #[serde(default)]
    pub exclude_booking_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct AvailabilityResponse {
    #[schema(value_type = String)]
    pub date: NaiveDate,
    pub timezone: String,
    pub slots: Vec<SlotAvailability>,
}

/// Every slot on the day for the host, with the auto-assigner's pick. Hosts
/// see the whole window (no lead-time cut); a past slot is simply unavailable.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn day_availability(
    pool: &PgPool,
    settings: &BookingSettings,
    tz: chrono_tz::Tz,
    branch_id: Uuid,
    date: NaiveDate,
    party: i32,
    section_id: Option<Uuid>,
    exclude: Option<Uuid>,
    public: bool,
) -> Result<Vec<SlotAvailability>, AppError> {
    let now = Utc::now();
    let starts = slot_starts(settings, tz, date);
    let duration = Duration::minutes(settings.default_duration_minutes as i64);
    let Some(first) = starts.first() else {
        return Ok(Vec::new());
    };
    let last_end = *starts.last().unwrap_or(first) + duration;
    let ground = Ground::load(pool, branch_id, *first, last_end, exclude).await?;
    let lead = Duration::minutes(settings.lead_time_minutes as i64);
    let mut out = Vec::with_capacity(starts.len());
    for s in starts {
        let end = s + duration;
        let too_soon = if public { s < now + lead } else { end <= now };
        let mut pick = if too_soon {
            None
        } else {
            ground.pick(settings, now, s, end, party, section_id)
        };
        if let (Some(_), Some(cap)) = (&pick, settings.max_covers_per_slot) {
            let booked = super::availability::covers_at(pool, branch_id, s, exclude).await?;
            if booked + party as i64 > cap as i64 {
                pick = None;
            }
        }
        out.push(SlotAvailability {
            starts_at: s,
            ends_at: end,
            available: pick.is_some(),
            table_ids: pick.unwrap_or_default(),
        });
    }
    Ok(out)
}

#[utoipa::path(get, path = "/bookings/availability", tag = "bookings", operation_id = "booking_availability", params(AvailabilityQuery),
    responses((status = 200, body = AvailabilityResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn availability(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<AvailabilityQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    if query.party_size <= 0 {
        return Err(AppError::BadRequest("party_size must be at least 1".into()));
    }
    let settings = load_settings(pool.get_ref(), query.branch_id).await?;
    let tz = branch_tz(pool.get_ref(), query.branch_id).await?;
    let slots = day_availability(
        pool.get_ref(),
        &settings,
        tz,
        query.branch_id,
        query.date,
        query.party_size,
        query.section_id,
        query.exclude_booking_id,
        false,
    )
    .await?;
    Ok(HttpResponse::Ok().json(AvailabilityResponse {
        date: query.date,
        timezone: tz.name().to_string(),
        slots,
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsQuery {
    pub branch_id: Uuid,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct BookingStats {
    pub total: i64,
    /// Guests across bookings that were seated or completed.
    pub covers: i64,
    pub seated: i64,
    pub completed: i64,
    pub no_show: i64,
    pub cancelled: i64,
    pub public_count: i64,
    pub host_count: i64,
    /// no_show / (no_show + seated + completed), 0 when nothing happened.
    pub no_show_rate: f64,
}

#[utoipa::path(get, path = "/bookings/stats", tag = "bookings", operation_id = "booking_stats", params(StatsQuery),
    responses((status = 200, body = BookingStats), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn stats(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<StatsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "bookings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let row: BookingStats = sqlx::query_as(
        "SELECT COUNT(*)::bigint AS total, \
            COALESCE(SUM(party_size) FILTER (WHERE status IN ('seated','completed')), 0)::bigint AS covers, \
            COUNT(*) FILTER (WHERE status = 'seated')::bigint AS seated, \
            COUNT(*) FILTER (WHERE status = 'completed')::bigint AS completed, \
            COUNT(*) FILTER (WHERE status = 'no_show')::bigint AS no_show, \
            COUNT(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled, \
            COUNT(*) FILTER (WHERE source = 'public')::bigint AS public_count, \
            COUNT(*) FILTER (WHERE source = 'host')::bigint AS host_count, \
            CASE WHEN COUNT(*) FILTER (WHERE status IN ('no_show','seated','completed')) = 0 THEN 0.0 \
                 ELSE COUNT(*) FILTER (WHERE status = 'no_show')::float8 \
                      / COUNT(*) FILTER (WHERE status IN ('no_show','seated','completed'))::float8 END \
                AS no_show_rate \
         FROM bookings WHERE branch_id = $1 AND starts_at >= $2 AND starts_at < $3",
    )
    .bind(query.branch_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}
